//! Explicit login flows and server-backed health checks. Secrets stay in memory
//! until a candidate session has been verified and atomically protected by DPAPI.
//! QR scanning, browser login, challenges and consent are always performed by
//! the user; this module never controls authentication UI or existing profiles.

use crate::auth::{
    CapturedCookie, CookieVault, LocalLoginState, captured_header, platform_contains_host,
};
use crate::core::Platform;
use anyhow::{Context, Result, anyhow, bail, ensure};
use chrono::TimeZone;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::Read,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use url::Url;

pub mod browser;

const MAX_REPLY: usize = 256 * 1024;
const MAX_COOKIES: usize = 2048;
pub const QR_POLL_INTERVAL: Duration = Duration::from_secs(2);
pub const QR_LIFETIME: Duration = Duration::from_secs(180);
/// A confirmed-valid result is only displayed as current for this long. After
/// that it becomes "上次有效，待复查" until the platform confirms it again.
pub const HEALTH_FRESH_SECS: u64 = 600;
/// Minimum gap between two on-demand checks for one platform. Repeating the same
/// request faster only wears out the platform session and cannot add evidence.
pub const MANUAL_CHECK_MIN_SECS: u64 = 10;

#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    fn check(&self) -> Result<()> {
        ensure!(!self.is_cancelled(), "登录操作已取消");
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Missing,
    AuxiliaryOnly,
    Expired,
    Valid,
    Invalid,
    VerificationRequired,
    NetworkUnknown,
    Unknown,
    Unreadable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginHealth {
    pub platform: Platform,
    pub state: HealthState,
    pub reason: String,
    /// When the platform last answered this check; never a local file mtime.
    pub checked_at: u64,
    /// Last time the platform itself explicitly confirmed this account. Kept
    /// across later network failures so a blip never erases the last good time.
    #[serde(default)]
    pub verified_at: u64,
}
impl LoginHealth {
    pub fn is_valid(&self) -> bool {
        self.state == HealthState::Valid
    }
    pub fn label(&self) -> &'static str {
        match self.state {
            HealthState::Missing => "未登录",
            HealthState::AuxiliaryOnly => "仅游客凭据",
            HealthState::Expired => "凭据已到期",
            HealthState::Valid => "登录有效",
            HealthState::Invalid => "登录失效",
            HealthState::VerificationRequired => "需要网页核验",
            HealthState::NetworkUnknown => "网络异常，无法确认",
            HealthState::Unknown => "无法确认登录态",
            HealthState::Unreadable => "凭据暂不可读",
        }
    }

    /// True once a previously confirmed result is older than
    /// `HEALTH_FRESH_SECS`. Staleness is a display fact, not an expiry claim.
    pub fn is_stale(&self, now: u64) -> bool {
        self.state == HealthState::Valid
            && now > self.checked_at
            && now - self.checked_at > HEALTH_FRESH_SECS
    }

    /// Title for the current moment. A stale confirmation reads
    /// "上次有效，待复查" instead of pretending the credential already expired.
    pub fn current_label(&self, now: u64) -> &'static str {
        if self.is_stale(now) {
            "上次有效，待复查"
        } else {
            self.label()
        }
    }

    /// Reason line matching `current_label`.
    pub fn current_reason(&self, now: u64) -> String {
        if self.is_stale(now) {
            "上次验证有效，超过 10 分钟，待复查；这不代表 Cookie 过期。".to_owned()
        } else {
            self.reason.clone()
        }
    }

    /// Carries the last explicit-valid timestamp forward. Only a platform
    /// confirmation may move it; failures keep the previous time.
    pub fn carry_verified(mut self, previous: Option<&LoginHealth>) -> Self {
        self.verified_at = if self.state == HealthState::Valid {
            self.checked_at
        } else {
            previous.map(|old| old.verified_at).unwrap_or(0)
        };
        self
    }

    /// Two-line detail used by the account page: current verdict, check time and
    /// the last time the platform explicitly confirmed the account.
    pub fn detail_text(&self, now: u64) -> String {
        let mut text = format!(
            "{} 登录态：{}\n  {}\n  最近检测：{}",
            self.platform.label(),
            self.current_label(now),
            self.current_reason(now),
            time_text(self.checked_at)
        );
        if self.verified_at > 0 {
            text.push_str(&format!("；上次明确有效：{}", time_text(self.verified_at)));
        }
        text
    }
}

/// Whether another on-demand check may start now. Clock regressions are treated
/// as "no usable timestamp" so an explicit retry is not suppressed forever.
pub fn check_allowed(last_attempt: Option<u64>, now: u64, min_secs: u64) -> bool {
    match last_attempt {
        None => true,
        Some(at) if now < at => true,
        Some(at) => now - at >= min_secs,
    }
}

/// Local display time for a unix timestamp; zero means "never checked online".
pub fn time_text(timestamp: u64) -> String {
    if timestamp == 0 {
        return "尚未在线验证".to_owned();
    }
    chrono::Local
        .timestamp_opt(timestamp as i64, 0)
        .single()
        .map(|time| time.format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "时间未知".to_owned())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn health(platform: Platform, state: HealthState, reason: &str) -> LoginHealth {
    let checked_at = now();
    LoginHealth {
        platform,
        state,
        reason: reason.into(),
        checked_at,
        verified_at: if state == HealthState::Valid {
            checked_at
        } else {
            0
        },
    }
}

/// An HTTP result whose Debug formatter does not reveal login responses,
/// redirect parameters or Set-Cookie values.
pub struct LoginReply {
    pub status: u16,
    pub body: String,
    pub set_cookies: Vec<String>,
    pub location: Option<String>,
}
impl std::fmt::Debug for LoginReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginReply")
            .field("status", &self.status)
            .field("body_bytes", &self.body.len())
            .field("set_cookie_count", &self.set_cookies.len())
            .finish()
    }
}

pub trait LoginTransport: Send + Sync {
    fn get(
        &self,
        url: &Url,
        cookie: Option<&str>,
        cancellation: &Cancellation,
    ) -> Result<LoginReply>;
}

#[derive(Default)]
pub struct OfficialTransport;
impl LoginTransport for OfficialTransport {
    fn get(
        &self,
        url: &Url,
        cookie: Option<&str>,
        cancellation: &Cancellation,
    ) -> Result<LoginReply> {
        cancellation.check()?;
        validate_endpoint(url)?;
        let client = crate::platforms::endpoint_client(url)?;
        let mut request = client.get(url.clone()).header(
            "Referer",
            referer(platform_for_host(url.host_str().unwrap())?),
        );
        if let Some(cookie) = cookie {
            ensure!(
                cookie.len() <= 64 * 1024 && !cookie.contains(['\r', '\n']),
                "登录请求凭据格式无效"
            );
            request = request.header("Cookie", cookie);
        }
        let response = request
            .send()
            .map_err(|_| anyhow!("登录检查请求超时或网络异常"))?;
        cancellation.check()?;
        let status = response.status().as_u16();
        let set_cookies = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|value| {
                value
                    .to_str()
                    .map(str::to_owned)
                    .map_err(|_| anyhow!("登录响应Cookie编码无效"))
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(set_cookies.len() <= MAX_COOKIES, "登录响应Cookie数量超限");
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        ensure!(
            response
                .content_length()
                .is_none_or(|length| length <= MAX_REPLY as u64),
            "登录响应超过限额"
        );
        let mut bytes = Vec::new();
        response
            .take(MAX_REPLY as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| anyhow!("登录响应读取超时或网络异常"))?;
        ensure!(bytes.len() <= MAX_REPLY, "登录响应超过限额");
        cancellation.check()?;
        Ok(LoginReply {
            status,
            body: String::from_utf8(bytes).map_err(|_| anyhow!("登录响应不是UTF-8"))?,
            set_cookies,
            location,
        })
    }
}

fn platform_for_host(host: &str) -> Result<Platform> {
    for platform in Platform::ALL {
        if platform_contains_host(platform, host) {
            return Ok(platform);
        }
    }
    bail!("登录请求主机不属于受支持平台")
}
fn referer(platform: Platform) -> &'static str {
    match platform {
        Platform::Bili => "https://www.bilibili.com/",
        Platform::Huya => "https://www.huya.com/",
        Platform::Douyu => "https://www.douyu.com/",
        Platform::Douyin => "https://www.douyin.com/",
    }
}
fn validate_endpoint(url: &Url) -> Result<()> {
    ensure!(
        url.scheme() == "https"
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "登录接口地址未通过校验"
    );
    let accepted = match (url.host_str(), url.path()) {
        (
            Some("passport.bilibili.com"),
            "/x/passport-login/web/qrcode/generate"
            | "/x/passport-login/web/qrcode/poll"
            | "/x/passport-login/web/crossDomain",
        ) => true,
        (Some("api.bilibili.com"), "/x/web-interface/nav") => true,
        (Some("l.huya.com"), "/udb_web/udbport2.php") => true,
        (Some("passport.douyu.com"), "/wgapi/member/passport/safeAuth") => true,
        (Some("www.douyin.com"), "/aweme/v1/web/user/profile/self/") => true,
        _ => false,
    };
    ensure!(accepted, "登录接口不在固定官方端点清单中");
    Ok(())
}

pub fn health_endpoint(platform: Platform) -> Url {
    Url::parse(match platform {
        Platform::Bili=>"https://api.bilibili.com/x/web-interface/nav",
        Platform::Huya=>"https://l.huya.com/udb_web/udbport2.php?m=HuyaLogin&do=checkLogin",
        Platform::Douyu=>"https://passport.douyu.com/wgapi/member/passport/safeAuth?client_id=1&redirect_url=https%3A%2F%2Fwww.douyu.com%2F&did=&callback=livehubLoginCheck",
        Platform::Douyin=>"https://www.douyin.com/aweme/v1/web/user/profile/self/?device_platform=webapp&aid=6383",
    }).expect("fixed valid login URL")
}

pub fn check_health(
    platform: Platform,
    vault: &CookieVault,
    cancellation: &Cancellation,
) -> Result<LoginHealth> {
    check_health_with(platform, vault, cancellation, &OfficialTransport)
}

pub fn check_health_with(
    platform: Platform,
    vault: &CookieVault,
    cancellation: &Cancellation,
    transport: &dyn LoginTransport,
) -> Result<LoginHealth> {
    cancellation.check()?;
    match vault.local_login_state(platform) {
        Ok(LocalLoginState::Missing) => {
            return Ok(health(
                platform,
                HealthState::Missing,
                "尚未保存本程序的登录凭据",
            ));
        }
        Ok(LocalLoginState::AuxiliaryOnly) => {
            return Ok(health(
                platform,
                HealthState::AuxiliaryOnly,
                "只找到游客或辅助Cookie，不能视为已登录",
            ));
        }
        Ok(LocalLoginState::Expired) => {
            return Ok(health(
                platform,
                HealthState::Expired,
                "本地关键登录Cookie已到期，请重新登录",
            ));
        }
        Err(_) => {
            return Ok(health(
                platform,
                HealthState::Unreadable,
                "本地加密凭据暂不可读，未判定服务器已注销",
            ));
        }
        Ok(LocalLoginState::Ready) => (),
    }
    let url = health_endpoint(platform);
    let cookie = vault.header_for_url(platform, url.as_str())?;
    if cookie.is_none() {
        return Ok(health(
            platform,
            HealthState::Unknown,
            "已保存的Cookie不适用于该登录检查接口，请同步完整网页登录会话",
        ));
    }
    match transport.get(&url, cookie.as_deref(), cancellation) {
        Ok(reply) => Ok(evaluate_session_health(platform, &reply, cookie.as_deref())),
        Err(error) if cancellation.is_cancelled() => Err(error),
        Err(_) => Ok(health(
            platform,
            HealthState::NetworkUnknown,
            "请求超时或网络异常，未判定登录失效",
        )),
    }
}

/// Verify a browser/QR candidate in memory before replacing an existing vault.
pub fn verify_captured(
    platform: Platform,
    cookies: &[CapturedCookie],
    cancellation: &Cancellation,
    transport: &dyn LoginTransport,
) -> Result<LoginHealth> {
    cancellation.check()?;
    let url = health_endpoint(platform);
    let cookie = captured_header(platform, cookies, url.as_str(), now())?;
    let Some(cookie) = cookie else {
        return Ok(health(
            platform,
            HealthState::Unknown,
            "登录候选未覆盖官方检查接口，未保存或覆盖旧凭据",
        ));
    };
    match transport.get(&url, Some(&cookie), cancellation) {
        Ok(reply) => Ok(evaluate_session_health(platform, &reply, Some(&cookie))),
        Err(error) if cancellation.is_cancelled() => Err(error),
        Err(_) => Ok(health(
            platform,
            HealthState::NetworkUnknown,
            "候选会话检查遇到网络异常，旧凭据保持原样",
        )),
    }
}

fn number(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.parse::<u64>().ok())
        .filter(|n| *n > 0)
}
fn code(root: &Value, key: &str, expected: i64) -> bool {
    root[key]
        .as_i64()
        .or_else(|| root[key].as_str()?.parse::<i64>().ok())
        == Some(expected)
}

fn evaluate_session_health(
    platform: Platform,
    reply: &LoginReply,
    cookie: Option<&str>,
) -> LoginHealth {
    let checked = evaluate_health(platform, reply);
    if platform != Platform::Bili || !checked.is_valid() {
        return checked;
    }
    let root: Value = match serde_json::from_str(&reply.body) {
        Ok(root) => root,
        Err(_) => return checked,
    };
    let uid = number(&root["data"]["mid"]);
    let identities: Vec<_> = cookie
        .unwrap_or("")
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .filter(|(name, _)| *name == "DedeUserID")
        .map(|(_, value)| value.parse::<u64>().ok())
        .collect();
    if identities
        .iter()
        .any(|identity| identity.is_none() || *identity != uid)
    {
        return health(
            platform,
            HealthState::Unknown,
            "B站会话与账号标识不一致，请重新同步完整会话；原凭据未改动",
        );
    }
    checked
}

/// Pure response classifier. It never treats a cookie's mere presence or a
/// successful stream resolution as proof of account authentication.
pub fn evaluate_health(platform: Platform, reply: &LoginReply) -> LoginHealth {
    if platform == Platform::Douyu
        && reply.status == 302
        && reply.location.as_deref().is_some_and(valid_douyu_redirect)
    {
        return health(
            platform,
            HealthState::Valid,
            "斗鱼官方SSO返回明确授权成功跳转",
        );
    }
    if matches!(reply.status, 401 | 403 | 429) || (300..400).contains(&reply.status) {
        return health(
            platform,
            HealthState::VerificationRequired,
            "平台拒绝或限制检查请求，可能需要网页登录或验证；未判定Cookie已失效",
        );
    }
    if reply.status != 200 {
        return health(
            platform,
            HealthState::NetworkUnknown,
            "平台暂不可用，无法确认登录态",
        );
    }
    let mut text = reply.body.trim();
    if text.is_empty() {
        return health(
            platform,
            HealthState::VerificationRequired,
            "平台返回空内容，可能需要网页核验",
        );
    }
    if let Some(json) = text.strip_prefix("livehubLoginCheck(") {
        text = json
            .trim_end_matches(|c: char| c == ';' || c.is_whitespace())
            .strip_suffix(')')
            .unwrap_or("");
    }
    let root: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(_) => {
            return health(
                platform,
                HealthState::Unknown,
                "响应格式变化或网页验证，未取得明确登录证据",
            );
        }
    };
    let (valid, invalid) = match platform {
        Platform::Bili => (
            code(&root, "code", 0)
                && root["data"]["isLogin"] == true
                && number(&root["data"]["mid"]).is_some(),
            code(&root, "code", -101)
                || (code(&root, "code", 0) && root["data"]["isLogin"] == false),
        ),
        Platform::Huya => (
            root["isLogined"] == true && number(&root["uid"]).is_some(),
            root["isLogined"] == false,
        ),
        Platform::Douyu => (
            code(&root, "error", 0) && root["data"].is_object(),
            code(&root, "error", 16),
        ),
        Platform::Douyin => (
            code(&root, "status_code", 0) && number(&root["user"]["uid"]).is_some(),
            code(&root, "status_code", 8)
                && matches!(
                    root["status_msg"].as_str(),
                    Some("用户未登录" | "用户未登陆")
                ),
        ),
    };
    if valid {
        health(
            platform,
            HealthState::Valid,
            "官方只读接口明确确认当前会话已登录",
        )
    } else if invalid {
        health(
            platform,
            HealthState::Invalid,
            "平台明确返回未登录，当前凭据已失效或撤销",
        )
    } else {
        health(
            platform,
            HealthState::Unknown,
            "平台未返回明确登录结论；播放或搜索成功不能替代登录验证",
        )
    }
}

fn valid_douyu_redirect(address: &str) -> bool {
    let Ok(url) = Url::parse(address) else {
        return false;
    };
    if url.scheme() != "https"
        || url.host_str() != Some("www.douyu.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.path() != "/api/passport/login"
    {
        return false;
    }
    let mut query = std::collections::BTreeMap::new();
    for (name, value) in url.query_pairs() {
        if query
            .insert(name.into_owned(), value.into_owned())
            .is_some()
        {
            return false;
        }
    }
    query
        .get("uid")
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|uid| uid > 0)
        && query
            .get("code")
            .is_some_and(|value| !value.is_empty() && value.len() <= 8192)
        && query
            .get("client_id")
            .is_some_and(|value| value == "1" || value.is_empty())
        && query
            .get("callback")
            .is_some_and(|value| value == "livehubLoginCheck")
}

/// Validate the legacy and current concrete official QR routes without allowing
/// arbitrary subdomains, downgraded transport or a mismatched challenge key.
pub fn validate_qr_challenge_url(address: &str, key: &str) -> Result<()> {
    let checked = Url::parse(address).map_err(|_| anyhow!("二维码地址格式无效"))?;
    let route = matches!(
        (checked.host_str(), checked.path()),
        (Some("passport.bilibili.com"), "/h5-app/passport/login/scan")
            | (Some("account.bilibili.com"), "/h5/account-h5/auth/scan-web")
    );
    ensure!(
        checked.scheme() == "https"
            && route
            && checked.port().is_none()
            && checked.username().is_empty()
            && checked.password().is_none()
            && checked.fragment().is_none()
            && address.len() <= 4096,
        "二维码地址未通过官方主机校验"
    );
    let keys: Vec<_> = checked
        .query_pairs()
        .filter(|(name, _)| name == "qrcode_key")
        .map(|(_, value)| value.into_owned())
        .collect();
    ensure!(
        keys.len() == 1 && keys[0] == key,
        "二维码标识与本次登录请求不一致"
    );
    Ok(())
}

/// Public material is intended for QR rendering only. Debug redacts the URL,
/// since that URL contains the temporary login challenge.
#[derive(Clone)]
pub struct QrChallenge {
    pub url: String,
    pub expires_in_seconds: u64,
}
impl std::fmt::Debug for QrChallenge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QrChallenge")
            .field("url", &"[redacted login challenge]")
            .field("expires_in_seconds", &self.expires_in_seconds)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QrStatus {
    WaitingScan,
    WaitingConfirm,
    Expired,
    Cancelled,
    VerificationPending(LoginHealth),
    LoggedIn(LoginHealth),
}

pub struct BiliQrLogin {
    challenge: QrChallenge,
    key: String,
    transport: Arc<dyn LoginTransport>,
    created: Instant,
    cookies: Vec<CapturedCookie>,
    candidate: bool,
    finished: bool,
}
impl Drop for BiliQrLogin {
    fn drop(&mut self) {
        unsafe { self.key.as_bytes_mut().fill(0) };
        for cookie in &mut self.cookies {
            unsafe { cookie.value.as_bytes_mut().fill(0) }
        }
    }
}

impl BiliQrLogin {
    pub fn start(cancellation: &Cancellation) -> Result<Self> {
        Self::start_with_transport(Arc::new(OfficialTransport), cancellation)
    }
    pub fn start_with_transport(
        transport: Arc<dyn LoginTransport>,
        cancellation: &Cancellation,
    ) -> Result<Self> {
        cancellation.check()?;
        let url = Url::parse("https://passport.bilibili.com/x/passport-login/web/qrcode/generate")?;
        let reply = transport.get(&url, None, cancellation)?;
        ensure!(reply.status == 200, "B站二维码申请受阻，请稍后重试");
        let root: Value =
            serde_json::from_str(&reply.body).map_err(|_| anyhow!("B站二维码响应格式变化"))?;
        ensure!(code(&root, "code", 0), "B站未接受二维码登录申请");
        let key = root["data"]["qrcode_key"]
            .as_str()
            .filter(|value| {
                !value.is_empty()
                    && value.len() <= 256
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            })
            .ok_or_else(|| anyhow!("B站未返回有效二维码标识"))?
            .to_owned();
        let address = root["data"]["url"]
            .as_str()
            .ok_or_else(|| anyhow!("B站未返回二维码内容"))?;
        validate_qr_challenge_url(address, &key)?;
        let mut result = Self {
            challenge: QrChallenge {
                url: address.into(),
                expires_in_seconds: QR_LIFETIME.as_secs(),
            },
            key,
            transport,
            created: Instant::now(),
            cookies: Vec::new(),
            candidate: false,
            finished: false,
        };
        result.absorb(&url, &reply.set_cookies)?;
        cancellation.check()?;
        Ok(result)
    }
    pub fn challenge(&self) -> QrChallenge {
        let mut value = self.challenge.clone();
        value.expires_in_seconds = QR_LIFETIME.saturating_sub(self.created.elapsed()).as_secs();
        value
    }
    /// Perform one bounded poll. Call no faster than QR_POLL_INTERVAL from a
    /// background worker. A failed verification leaves the previous vault intact.
    pub fn poll(&mut self, vault: &CookieVault, cancellation: &Cancellation) -> Result<QrStatus> {
        if cancellation.is_cancelled() {
            return Ok(QrStatus::Cancelled);
        }
        ensure!(!self.finished, "此二维码流程已经结束，请重新申请");
        if self.created.elapsed() >= QR_LIFETIME {
            return Ok(QrStatus::Expired);
        }
        if !self.candidate {
            let mut url =
                Url::parse("https://passport.bilibili.com/x/passport-login/web/qrcode/poll")?;
            url.query_pairs_mut()
                .append_pair("qrcode_key", &self.key)
                .append_pair("source", "main-fe-header");
            let cookie = captured_header(Platform::Bili, &self.cookies, url.as_str(), now())?;
            let reply = self.transport.get(&url, cookie.as_deref(), cancellation)?;
            cancellation.check()?;
            ensure!(reply.status == 200, "B站扫码状态检查受阻，请稍后重试");
            let root: Value = serde_json::from_str(&reply.body)
                .map_err(|_| anyhow!("B站扫码状态响应格式变化"))?;
            ensure!(code(&root, "code", 0), "B站未接受扫码状态检查");
            let state = root["data"]["code"]
                .as_i64()
                .ok_or_else(|| anyhow!("B站没有返回有效扫码状态"))?;
            match state {
                86101 => return Ok(QrStatus::WaitingScan),
                86090 => return Ok(QrStatus::WaitingConfirm),
                86038 => return Ok(QrStatus::Expired),
                0 => (),
                _ => bail!("B站返回未知扫码状态，请刷新二维码"),
            }
            self.absorb(&url, &reply.set_cookies)?;
            // Some deployments return a cross-domain completion URL. Visit
            // only the fixed passport endpoint to obtain actual Set-Cookie
            // metadata; never invent parent-domain scope from query values.
            if !self
                .cookies
                .iter()
                .any(|cookie| cookie.name == "SESSDATA" && !cookie.value.is_empty())
            {
                if let Some(address) = root["data"]["url"]
                    .as_str()
                    .filter(|address| !address.is_empty())
                {
                    let redirect =
                        Url::parse(address).map_err(|_| anyhow!("扫码完成地址格式无效"))?;
                    ensure!(
                        redirect.host_str() == Some("passport.bilibili.com")
                            && redirect.path() == "/x/passport-login/web/crossDomain",
                        "扫码完成地址不属于固定官方端点"
                    );
                    validate_endpoint(&redirect)?;
                    let header =
                        captured_header(Platform::Bili, &self.cookies, redirect.as_str(), now())?;
                    let completion =
                        self.transport
                            .get(&redirect, header.as_deref(), cancellation)?;
                    cancellation.check()?;
                    ensure!(
                        matches!(completion.status, 200 | 302),
                        "B站扫码完成请求受阻，旧凭据未改动"
                    );
                    self.absorb(&redirect, &completion.set_cookies)?;
                }
            }
            ensure!(
                self.cookies
                    .iter()
                    .any(|cookie| cookie.name == "SESSDATA" && !cookie.value.is_empty()),
                "扫码已确认，但未收到完整登录Cookie；原凭据未改动"
            );
            self.candidate = true;
        }
        let url = health_endpoint(Platform::Bili);
        let cookie = captured_header(Platform::Bili, &self.cookies, url.as_str(), now())?;
        let Some(cookie) = cookie else {
            return Ok(QrStatus::VerificationPending(health(
                Platform::Bili,
                HealthState::Unknown,
                "扫码Cookie未覆盖官方登录检查接口，原凭据未改动",
            )));
        };
        let checked = match self.transport.get(&url, Some(&cookie), cancellation) {
            Ok(reply) => {
                let checked = evaluate_session_health(Platform::Bili, &reply, Some(&cookie));
                self.absorb(&url, &reply.set_cookies)?;
                checked
            }
            Err(error) if cancellation.is_cancelled() => return Err(error),
            Err(_) => health(
                Platform::Bili,
                HealthState::NetworkUnknown,
                "扫码已确认，平台登录检查暂不可用；原凭据未改动",
            ),
        };
        cancellation.check()?;
        if !checked.is_valid() {
            return Ok(QrStatus::VerificationPending(checked));
        }
        vault.import_captured(Platform::Bili, &self.cookies)?;
        self.finished = true;
        Ok(QrStatus::LoggedIn(checked))
    }
    fn absorb(&mut self, url: &Url, headers: &[String]) -> Result<()> {
        for header in headers {
            merge_set_cookie(Platform::Bili, &mut self.cookies, url, header, now())?;
        }
        Ok(())
    }
}

/// Parse one response cookie without inventing a Domain attribute. Public for
/// the browser/manual-login adapter and synthetic contract tests.
pub fn merge_set_cookie(
    platform: Platform,
    jar: &mut Vec<CapturedCookie>,
    origin: &Url,
    header: &str,
    at: u64,
) -> Result<()> {
    ensure!(
        origin.scheme() == "https"
            && origin.username().is_empty()
            && origin.password().is_none()
            && origin.port().is_none(),
        "登录Cookie响应来源无效"
    );
    let host = origin
        .host_str()
        .ok_or_else(|| anyhow!("登录响应缺少主机"))?;
    ensure!(
        platform_contains_host(platform, host),
        "登录Cookie响应不属于所选平台"
    );
    ensure!(
        header.len() <= 32 * 1024 && !header.contains(['\r', '\n']),
        "登录Cookie响应格式无效"
    );
    let mut parts = header.split(';');
    let first = parts.next().unwrap_or("").trim();
    let (name, value) = first
        .split_once('=')
        .ok_or_else(|| anyhow!("登录Cookie缺少名称或值"))?;
    let mut cookie = CapturedCookie {
        name: name.into(),
        value: value.into(),
        domain: host.to_ascii_lowercase(),
        host_only: true,
        path: default_cookie_path(origin.path()),
        secure: false,
        http_only: false,
        expires_at: None,
    };
    let mut max_age = None;
    let mut seen = std::collections::BTreeSet::new();
    for attribute in parts {
        let attribute = attribute.trim();
        if attribute.is_empty() {
            continue;
        }
        let (key, value) = attribute.split_once('=').unwrap_or((attribute, ""));
        let key = key.to_ascii_lowercase();
        if matches!(key.as_str(), "domain" | "path" | "expires" | "max-age") {
            ensure!(seen.insert(key.clone()), "登录Cookie重复作用域属性");
        }
        match key.as_str() {
            "domain" => {
                let domain = value.trim_start_matches('.').to_ascii_lowercase();
                ensure!(
                    host == domain
                        || host
                            .strip_suffix(&domain)
                            .is_some_and(|prefix| prefix.ends_with('.')),
                    "登录响应试图设置其他域Cookie"
                );
                cookie.domain = domain;
                cookie.host_only = false;
            }
            "path" => {
                if value.starts_with('/') {
                    cookie.path = value.into();
                }
            }
            "secure" => cookie.secure = true,
            "httponly" => cookie.http_only = true,
            "max-age" => {
                max_age = Some(
                    value
                        .parse::<i64>()
                        .map_err(|_| anyhow!("登录Cookie到期值无效"))?,
                )
            }
            "expires" => {
                let expiry = chrono::DateTime::parse_from_rfc2822(value)
                    .map_err(|_| anyhow!("登录Cookie到期日期无效"))?
                    .timestamp();
                cookie.expires_at = Some(expiry.max(0) as u64);
            }
            _ => (),
        }
    }
    if let Some(age) = max_age {
        cookie.expires_at = Some(if age <= 0 {
            0
        } else {
            at.checked_add(age as u64)
                .ok_or_else(|| anyhow!("登录Cookie到期值溢出"))?
        });
    }
    cookie.validate(platform)?;
    jar.retain(|old| {
        !(old.name == cookie.name && old.domain == cookie.domain && old.path == cookie.path)
    });
    if cookie.expires_at.is_none_or(|expiry| expiry > at) {
        jar.push(cookie);
    }
    ensure!(jar.len() <= MAX_COOKIES, "登录Cookie数量超过限额");
    Ok(())
}
fn default_cookie_path(path: &str) -> String {
    match path.rfind('/') {
        Some(index) if index > 0 => path[..index].into(),
        _ => "/".into(),
    }
}
