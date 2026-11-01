//! Platform adapters. All parsing is independently testable without network access.
//!
//! Huya's anti-code and Douyu's auth sequence adapt the Streamlink plugins
//! (Copyright Christopher Rosell and Streamlink Team, BSD-2-Clause).
//! See licenses/streamlink-LICENSE.txt and THIRD_PARTY_NOTICES.md.
//! Bilibili WBI permutation/protocol reference follows blivedm (MIT), with its
//! retained upstream notice in licenses/blivedm-LICENSE.txt.
//! This module does not read legacy cookies, browser profiles or private settings.

use crate::auth::CookieVault;
use crate::core::{Platform, Quality, ResolvedStream, Settings, Target};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::blocking::{Client, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashSet},
    io::Read,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    process::{Command, Stdio},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use url::Url;

const MAX_BODY: usize = 4 * 1024 * 1024;
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/130.0.0.0 Safari/537.36";

/// The official Douyin web client UA observed by the legacy launcher. Douyin's
/// search endpoint rejects or silences non-browser-looking clients, so this is
/// sent explicitly instead of the generic platform UA.
const DOUYIN_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36 Edg/154.0.0.0";

/// Requests obtain cookies for their exact destination, preserving domain,
/// host-only, path, expiry and Secure attributes from the owning vault.
pub trait CookieSource: Send + Sync {
    fn header_for_url(&self, platform: Platform, url: &str) -> Result<Option<String>>;
}

impl CookieSource for CookieVault {
    fn header_for_url(&self, platform: Platform, url: &str) -> Result<Option<String>> {
        CookieVault::header_for_url(self, platform, url)
    }
}

thread_local! {
    // Blocking adapters stay on one worker. Sessions never cross threads or
    // become process-global; nested operations restore their preceding scope.
    static REQUEST_SESSIONS: RefCell<Vec<(Platform, Arc<dyn CookieSource>)>> = RefCell::new(Vec::new());
}

struct SessionGuard(std::marker::PhantomData<std::rc::Rc<()>>);
impl SessionGuard {
    fn enter(platform: Platform, source: Arc<dyn CookieSource>) -> Self {
        REQUEST_SESSIONS.with(|sessions| sessions.borrow_mut().push((platform, source)));
        Self(std::marker::PhantomData)
    }
}
impl Drop for SessionGuard {
    fn drop(&mut self) {
        REQUEST_SESSIONS.with(|sessions| {
            sessions.borrow_mut().pop();
        });
    }
}

pub fn with_authenticated_session<T>(
    platform: Platform,
    vault: &CookieVault,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    with_cookie_source(platform, Arc::new(vault.clone()), action)
}

/// Scope an explicit request-level cookie source to this blocking worker only.
/// Used by protocol clients to combine stored cookies with temporary identities
/// while preserving each individual request's destination scope.
pub fn with_cookie_source<T>(
    platform: Platform,
    source: Arc<dyn CookieSource>,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let _guard = SessionGuard::enter(platform, source);
    action()
}

pub fn resolve_authenticated(
    target: &Target,
    settings: &Settings,
    vault: &CookieVault,
) -> Result<ResolvedStream> {
    with_authenticated_session(target.platform, vault, || resolve(target, settings))
}

pub fn search_authenticated(
    platform: Platform,
    keyword: &str,
    vault: &CookieVault,
) -> Result<Vec<LookupCandidate>> {
    with_authenticated_session(platform, vault, || search(platform, keyword))
}

pub fn verify_authenticated(
    candidate: &LookupCandidate,
    vault: &CookieVault,
) -> Result<LookupCandidate> {
    with_authenticated_session(candidate.platform, vault, || {
        verify_candidate(candidate, None)
    })
}

/// One public source behind a candidate's details (platform search, room page,
/// account profile or an AI research citation).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookupSource {
    /// HTTPS URL that was read (public page/API; never carries credentials).
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    /// Short excerpt already cleaned of markup; empty when no excerpt is available.
    #[serde(default)]
    pub excerpt: String,
    /// ISO-8601 UTC retrieval time, empty when unknown.
    #[serde(default)]
    pub retrieved_at: String,
    /// What this source supports, for example "房间号" or "主播名".
    #[serde(default)]
    pub supports: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LookupCandidate {
    pub name: String,
    pub room: String,
    pub platform: Platform,
    pub title: String,
    #[serde(default)]
    pub profile_url: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
    #[serde(default)]
    pub follower_count: Option<u64>,
    /// Public category/game the platform reports for this account.
    #[serde(default)]
    pub category: String,
    /// Public signature/introduction the platform reports, used to compare clues.
    #[serde(default)]
    pub description: String,
    /// Platform handle (for example a Douyin account handle); never a web room ID.
    #[serde(default)]
    pub account_handle: Option<String>,
    /// Douyin stable account uid; None for other platforms and unverified rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sec_uid: Option<String>,
    /// Platform-reported live state: "live", "offline" or "replay".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_status: Option<String>,
    /// Platform-reported update/last-change text (never fabricated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform_update: Option<String>,
    /// Epoch seconds of the last successful platform identity verification
    /// (0 = this candidate was never verified against the platform room).
    #[serde(default)]
    pub verified_at: u64,
    /// Human-readable note explaining what verification proved.
    #[serde(default)]
    pub verification_note: String,
    /// Why the candidate was matched to the query (official search, verified room,
    /// AI citation, ...). Only facts the platform/AI actually returned.
    #[serde(default)]
    pub match_reasons: Vec<String>,
    /// Which producer created this candidate: platform_search, platform_verify,
    /// ai or input.
    #[serde(default)]
    pub provider: String,
    /// Public sources backing the details panel.
    #[serde(default)]
    pub sources: Vec<LookupSource>,
}
impl Default for LookupCandidate {
    fn default() -> Self {
        Self {
            name: String::new(),
            room: String::new(),
            platform: Platform::Bili,
            title: String::new(),
            profile_url: None,
            account_id: None,
            avatar_url: None,
            follower_count: None,
            category: String::new(),
            description: String::new(),
            account_handle: None,
            sec_uid: None,
            live_status: None,
            platform_update: None,
            verified_at: 0,
            verification_note: String::new(),
            match_reasons: Vec::new(),
            provider: String::new(),
            sources: Vec::new(),
        }
    }
}

pub fn candidate_followers(platform: Platform, row: &Value) -> Option<u64> {
    let keys: &[&str] = match platform {
        Platform::Bili => &["attentions", "attention", "follower_num"],
        Platform::Huya => &["game_activityCount", "fans"],
        Platform::Douyu => &["fansNum", "followerCount", "fans_num", "fans"],
        Platform::Douyin => &["follower_count"],
    };
    keys.iter().find_map(|key| {
        row[*key]
            .as_u64()
            .or_else(|| row[*key].as_str()?.parse::<u64>().ok())
    })
}
pub fn candidate_avatar(row: &Value) -> Option<String> {
    let names = [
        "uface",
        "face",
        "avatar",
        "avatar_url",
        "avatarMiddle",
        "avatar_small",
        "game_avatarUrl180",
        "game_avatarUrl52",
    ];
    let raw = names
        .iter()
        .find_map(|key| row[*key].as_str())
        .or_else(|| {
            row.pointer("/avatar_thumb/url_list/0")
                .and_then(Value::as_str)
        })?;
    let address = if raw.starts_with("//") {
        format!("https:{raw}")
    } else {
        raw.into()
    };
    crate::images::approved_url(&address)
        .ok()
        .map(|url| url.to_string())
}

fn epoch_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// ISO-8601 UTC retrieval timestamp for a public source.
pub fn retrieval_timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Platform search rows expose live state under different names/values; map only
/// values the platform clearly returned, never a guessed state.
pub fn candidate_live_status(platform: Platform, row: &Value) -> Option<String> {
    let number = |keys: &[&str]| -> Option<i64> {
        keys.iter()
            .find_map(|key| row[*key].as_i64().or_else(|| row[*key].as_str()?.parse().ok()))
    };
    let text = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|key| row[*key].as_str().map(|value| value.trim().to_ascii_lowercase()))
    };
    let status = match platform {
        Platform::Bili => match number(&["live_status"]) {
            Some(1) => "live",
            Some(0) => "offline",
            Some(2) => "replay",
            _ => return None,
        },
        Platform::Huya => match text(&["liveStatus", "live_status", "liveState"]) {
            Some(value) if value == "on" || value == "1" || value == "2" => "live",
            Some(value) if value == "off" || value == "0" => "offline",
            Some(value) if value == "replay" => "replay",
            _ => match number(&["liveStatus", "live_status"]) {
                Some(1 | 2) => "live",
                Some(0) => "offline",
                _ => return None,
            },
        },
        Platform::Douyu => {
            if number(&["videoLoop"]) == Some(1) {
                "replay"
            } else {
                match number(&["show_status", "isLive", "roomStatus"]) {
                    Some(1) => "live",
                    Some(0) => "offline",
                    _ => return None,
                }
            }
        }
        Platform::Douyin => match number(&["live_status", "status"]) {
            Some(2) => "live",
            Some(4) => "offline",
            _ => return None,
        },
    };
    Some(status.to_owned())
}

fn candidate_sec_uid(platform: Platform, row: &Value) -> Option<String> {
    if platform != Platform::Douyin {
        return None;
    }
    let sec = str_field(row, "sec_uid");
    valid_sec_uid(&sec).then_some(sec)
}

/// Fill the detail-panel fields for a candidate verified against the platform.
fn with_verification(
    mut candidate: LookupCandidate,
    provider: &str,
    note: &str,
    live_status: Option<String>,
    source: LookupSource,
) -> LookupCandidate {
    candidate.provider = provider.to_owned();
    candidate.verified_at = epoch_seconds();
    candidate.verification_note = note.to_owned();
    candidate.live_status = live_status;
    candidate
        .match_reasons
        .push("平台官方接口返回的直播间与主播身份一致".into());
    candidate.sources.push(source);
    candidate
}

fn platform_room_page(platform: Platform, room: &str) -> Option<String> {
    match platform {
        Platform::Bili => Some(format!("https://live.bilibili.com/{room}")),
        Platform::Huya => Some(format!("https://www.huya.com/{room}")),
        Platform::Douyu => Some(format!("https://www.douyu.com/{room}")),
        Platform::Douyin => Some(format!("https://live.douyin.com/{room}")),
    }
}

/// Public category/game fields returned by platform search or room JSON.
pub fn candidate_category(row: &Value) -> Option<String> {
    [
        "area_name",
        "parent_area_name",
        "game_name",
        "gameFullName",
        "cate2_name",
        "cate_name",
    ]
    .iter()
    .find_map(|key| {
        let value = clean_label(row[*key].as_str()?);
        (!value.trim().is_empty()).then(|| value.chars().take(128).collect())
    })
}

/// Public signature/introduction fields; never a private profile field.
pub fn candidate_description(row: &Value) -> Option<String> {
    ["signature", "sign", "introduction", "description", "desc"]
        .iter()
        .find_map(|key| {
            let value = clean_label(row[*key].as_str()?);
            (!value.trim().is_empty()).then(|| value.chars().take(1024).collect())
        })
}

/// Numeric platform account identifier (uid family); never a room ID.
pub fn candidate_account(platform: Platform, row: &Value) -> Option<String> {
    let keys: &[&str] = match platform {
        Platform::Bili => &["uid"],
        Platform::Huya => &["uid", "owner_uid"],
        Platform::Douyu => &["uid", "owner_uid"],
        Platform::Douyin => &["uid_str", "uid"],
    };
    keys.iter().find_map(|key| {
        let raw = match &row[*key] {
            Value::String(text) => text.clone(),
            Value::Number(number) => number.to_string(),
            _ => return None,
        };
        numeric(raw)
    })
}

/// Public account handle (Douyin unique_id/short_id); other platforms use UIDs.
pub fn candidate_handle(platform: Platform, row: &Value) -> Option<String> {
    if platform != Platform::Douyin {
        return None;
    }
    ["unique_id", "short_id"].iter().find_map(|key| {
        let value = str_field(row, key);
        let trimmed = value.trim();
        (!trimmed.is_empty()
            && trimmed.len() <= 64
            && trimmed.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'.' || byte == b'-'
            }))
        .then(|| trimmed.to_owned())
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterError {
    Offline,
    ReplayUnavailable,
    VerificationRequired,
    RateLimited,
    Unsupported(&'static str),
    InvalidResponse(&'static str),
    Transport,
}

/// Platform-specific replay wording. Bilibili calls it 轮播, Douyu 录播 and the
/// other platforms 重播; the message never claims the room went offline.
pub fn replay_unavailable_message(platform: Platform, reason: &str) -> String {
    let detail = if reason.trim().is_empty() {
        "平台未返回可用媒体"
    } else {
        reason.trim()
    };
    match platform {
        Platform::Bili => format!("B站轮播流不可用（{detail}）；可等待直播恢复"),
        Platform::Huya => format!("虎牙重播流不可用（{detail}）；可等待直播恢复"),
        Platform::Douyu => format!("斗鱼录播流不可用（{detail}）；可等待直播恢复"),
        Platform::Douyin => format!("抖音重播流不可用（{detail}）；可等待直播恢复"),
    }
}

/// Whether a stored session error already describes the replay-unavailable case.
pub fn is_replay_error(message: &str) -> bool {
    message.contains("房间正在重播")
        || message.contains("轮播流不可用")
        || message.contains("重播流不可用")
        || message.contains("录播流不可用")
}

/// Huya room page replay marker. Only the unambiguous replay marker is used: the
/// page's liveStatus-on also appears on replay pages (C# 2026-10-16 observation).
pub fn huya_page_replay(page: &str) -> bool {
    page.contains("liveStatus-replay")
}

/// Douyu room flags from the official betard JSON: show_status and the loop flag.
pub fn douyu_room_flags(root: &Value) -> (Option<i64>, i64) {
    let room = &root["room"];
    (
        int_field(room, "show_status"),
        int_field(room, "videoLoop").unwrap_or(0),
    )
}

/// A recorded/looping Douyu room is a replay only when the room is not reported
/// live; show_status=1 is never silently reclassified.
pub fn douyu_replay_from_flags(show_status: Option<i64>, video_loop: i64) -> bool {
    show_status != Some(1) && video_loop == 1
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Offline => f.write_str("房间当前未开播，可在会话页等待开播或取消"),
            Self::ReplayUnavailable => {
                f.write_str("房间正在重播，但平台暂未提供可用媒体，可等待直播恢复")
            }
            Self::VerificationRequired => {
                f.write_str("平台要求登录或网页验证，请到账号与登录页完成正常验证")
            }
            Self::RateLimited => f.write_str("平台临时限流或返回空壳页，请稍后重试"),
            Self::Unsupported(message) | Self::InvalidResponse(message) => f.write_str(message),
            Self::Transport => f.write_str("平台请求失败或超时，请检查网络后重试"),
        }
    }
}
impl std::error::Error for AdapterError {}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn str_field(value: &Value, key: &str) -> String {
    match &value[key] {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => String::new(),
    }
}

fn int_field(value: &Value, key: &str) -> Option<i64> {
    value[key]
        .as_i64()
        .or_else(|| value[key].as_str()?.parse().ok())
}

fn suffix(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

/// Validate a resolved media address, without contacting it or exposing its query.
/// CDN families are explicit; unexpected new CDN domains need review.
pub fn validate_media_url(address: &str) -> Result<Url> {
    let mut url = Url::parse(address).context("媒体地址格式无效")?;
    if url.scheme() == "http" {
        url.set_scheme("https")
            .map_err(|_| anyhow!("媒体地址协议无效"))?;
    }
    ensure!(
        url.scheme() == "https" && url.username().is_empty() && url.password().is_none(),
        "仅允许不含账号的 HTTPS 媒体地址"
    );
    // 平台 CDN（例如斗鱼 rtmp_url）会给出带非 443 端口的 https 地址；
    // 主机名仍受下面的 CDN 白名单约束，因此允许显式端口但不允许 0/privileged 之外的任意值。
    ensure!(url.port().is_none_or(|port| port != 0), "媒体地址端口无效");
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("媒体地址缺少主机名"))?;
    let allowed = [
        "bilivideo.com",
        "bilivideo.cn",
        "biliapi.net",
        // Bilibili live CDN 302 target observed 2026-10-31:
        // https://<hash>.v.smtcdns.net/d1--cn-gotcha04.bilivideo.com/...
        // The path still identifies the bilivideo object; Tencent's smtcdns
        // family is the only extra redirect family needed for Bili live.
        "smtcdns.net",
        "huya.com",
        "huyacdn.com",
        "douyucdn.cn",
        "douyucdn.com",
        "douyucdn2.cn",
        "douyucdn3.cn",
        "douyucdn4.cn",
        "douyucdn6.cn",
        "dycdn.cn",
        // Observed Douyu CDN family in addition to douyucdn (2026-10-29, anonymous resolve).
        "edgesrv.com",
        "douyu.com",
        // Observed Douyin live FLV CDN families (2026-10-29, anonymous resolve):
        // pull-flv-*.douyincdn.com and pull-flv-*.douyinliving.com (both answer
        // video/x-flv over HTTPS, so the http→https upgrade stays valid).
        "douyin.com",
        "douyincdn.com",
        "douyinliving.com",
        "douyinvod.com",
        "bytefcdnrd.com",
        "bytefcdn.com",
        // Douyin live 302 dispatch (observed 2026-10-31 for room 796794251979):
        // https://rt2016n.jomodns.com/pull-flv-q13.douyincdn.com/...; the path
        // still identifies the original douyincdn object. ByteDance's jomodns
        // CDN family is the only extra redirect family needed here.
        "jomodns.com",
        "tcdnlive.com",
        "alicdn.com",
        "amemv.com",
        "snssdk.com",
        "pstatp.com",
        "bdcdn.net",
    ];
    ensure!(
        allowed.iter().any(|domain| suffix(host, domain)),
        format!("平台返回了尚未允许的媒体主机：{host}")
    );
    ensure!(url.fragment().is_none(), "媒体地址不允许包含片段");
    Ok(url)
}

fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            let o = ip.octets();
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_multicast()
                && !ip.is_unspecified()
                && !ip.is_broadcast()
                && o[0] != 0
                && o[0] < 224
                && !(o[0] == 100 && (64..128).contains(&o[1]))
                && !(o[0] == 192 && o[1] == 0)
                && !(o[0] == 198 && (o[1] == 18 || o[1] == 19))
                && !ip.is_documentation()
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            (s[0] & 0xe000) == 0x2000
                && !ip.is_loopback()
                && !ip.is_unspecified()
                && !ip.is_multicast()
                && (s[0] & 0xfe00) != 0xfc00
                && (s[0] & 0xffc0) != 0xfe80
                && !(s[0] == 0x2001 && s[1] == 0x0db8)
                && ip.to_ipv4_mapped().is_none()
        }
    }
}

/// Local TUN proxies in fake-IP mode answer every hostname with a reserved
/// 198.18.0.0/15 address. The TLS peer identity still has to match the platform
/// host because the request keeps its original URL (SNI/Host), and every caller
/// of `endpoint_client` already passed the fixed host allowlist, so dialling the
/// proxy-mapped address does not widen the SSRF surface. This mirrors the
/// danmaku/receiver and relay policies already applied to the same TUN.
fn is_proxy_fake_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            octets[0] == 198 && (octets[1] & 0xfe) == 18
        }
        IpAddr::V6(_) => false,
    }
}

pub(crate) fn endpoint_client(url: &Url) -> Result<Client> {
    let host = url.host_str().ok_or_else(|| anyhow!("平台接口缺少主机"))?;
    let allowed = [
        "api.bilibili.com",
        "api.live.bilibili.com",
        "www.bilibili.com",
        "passport.bilibili.com",
        "www.huya.com",
        "mp.huya.com",
        "l.huya.com",
        "search.cdn.huya.com",
        "www.douyu.com",
        "passport.douyu.com",
        "live.douyin.com",
        "www.douyin.com",
        "www.iesdouyin.com",
        "webcast.amemv.com",
    ];
    ensure!(
        url.scheme() == "https"
            && allowed.contains(&host)
            && url.port().is_none_or(|port| port == 443)
            && url.username().is_empty()
            && url.password().is_none(),
        "平台接口地址未通过校验"
    );
    // Pin the same public addresses that were checked; do not validate one DNS answer
    // and let the HTTP client silently resolve a different one.
    let owned_host = host.to_owned();
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let result = (owned_host.as_str(), 443)
            .to_socket_addrs()
            .map(|addresses| addresses.collect::<Vec<_>>());
        let _ = send.send(result);
    });
    let addresses: Vec<SocketAddr> = receive
        .recv_timeout(Duration::from_secs(4))
        .map_err(|_| AdapterError::Transport)?
        .map_err(|_| AdapterError::Transport)?
        .into_iter()
        .filter(|address| is_public(address.ip()) || is_proxy_fake_ip(address.ip()))
        .filter(|address| host != "live.douyin.com" || address.is_ipv4())
        .collect();
    ensure!(!addresses.is_empty(), "平台域名没有可用的公网地址");
    Client::builder()
        .no_proxy()
        .user_agent(USER_AGENT)
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(12))
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(host, &addresses)
        .build()
        .map_err(|_| AdapterError::Transport.into())
}

fn read_response(response: Response) -> Result<String> {
    let status = response.status();
    if status.as_u16() == 429 {
        return Err(AdapterError::RateLimited.into());
    }
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(AdapterError::VerificationRequired.into());
    }
    ensure!(status.is_success(), "平台返回 HTTP {}", status.as_u16());
    ensure!(
        response
            .content_length()
            .is_none_or(|size| size <= MAX_BODY as u64),
        "平台响应超过大小上限"
    );
    let mut bytes = Vec::new();
    response
        .take(MAX_BODY as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| AdapterError::Transport)?;
    ensure!(bytes.len() <= MAX_BODY, "平台响应超过大小上限");
    String::from_utf8(bytes).context("平台响应不是 UTF-8")
}

fn checked_cookie(cookie: Option<&str>) -> Result<Option<&str>> {
    if let Some(value) = cookie {
        ensure!(
            value.len() <= 32 * 1024 && value.bytes().all(|byte| (32..127).contains(&byte)),
            "显式 Cookie 格式无效"
        );
    }
    Ok(cookie.filter(|value| !value.is_empty()))
}

fn cookie_for_request(url: &Url, diagnostic_cookie: Option<&str>) -> Result<Option<String>> {
    let source = REQUEST_SESSIONS.with(|sessions| sessions.borrow().last().cloned());
    match source {
        Some((platform, source)) => {
            // An error decrypting/loading the session propagates. A diagnostic
            // raw header is never used as a fallback for a scoped vault.
            let scoped = source.header_for_url(platform, url.as_str())?;
            checked_cookie(scoped.as_deref())?;
            if scoped.is_none()
                && platform == Platform::Douyin
                && url.host_str() == Some("live.douyin.com")
            {
                let nonce = uuid::Uuid::new_v4().simple().to_string();
                Ok(Some(format!("__ac_nonce={}", &nonce[..21])))
            } else {
                Ok(scoped)
            }
        }
        None => Ok(checked_cookie(diagnostic_cookie)?.map(str::to_owned)),
    }
}

pub(crate) fn get(url: Url, referer: &str, cookie: Option<&str>) -> Result<String> {
    let request_cookie = cookie_for_request(&url, cookie)?;
    let client = endpoint_client(&url)?;
    let mut request = client.get(url).header("Referer", referer);
    if let Some(cookie) = request_cookie {
        request = request.header("Cookie", cookie);
    }
    read_response(request.send().map_err(|_| AdapterError::Transport)?)
}

pub(crate) fn json_get(url: Url, referer: &str) -> Result<Value> {
    json_get_with_cookie(url, referer, None)
}

fn json_get_with_cookie(url: Url, referer: &str, cookie: Option<&str>) -> Result<Value> {
    let body = get(url, referer, cookie)?;
    if body.trim().is_empty() {
        return Err(AdapterError::VerificationRequired.into());
    }
    serde_json::from_str(&body)
        .map_err(|_| AdapterError::InvalidResponse("平台没有返回可识别的 JSON").into())
}

/// Build a public, cookie-free request to one of the fixed platform endpoints.
///
/// The Douyin account/live endpoints (www.iesdouyin.com, webcast.amemv.com)
/// are separate public origins from douyin.com. While a logged-in Douyin
/// session is active, get() rightly refuses to hand platform cookies to those
/// hosts, which used to make verification fail with "Refusing to send cookies
/// outside the selected platform". These endpoints are public profile/session
/// APIs; send no credentials at all instead of weakening the cookie-domain
/// guard.
fn public_request(url: &Url, referer: &str) -> Result<reqwest::blocking::RequestBuilder> {
    let client = endpoint_client(url)?;
    Ok(client
        .get(url.clone())
        .header("Referer", referer)
        .header("User-Agent", USER_AGENT))
}

fn public_get(url: Url, referer: &str) -> Result<String> {
    let request = public_request(&url, referer)?;
    read_response(request.send().map_err(|_| AdapterError::Transport)?)
}

pub(crate) fn public_json_get(url: Url, referer: &str) -> Result<Value> {
    let body = public_get(url, referer)?;
    if body.trim().is_empty() {
        return Err(AdapterError::VerificationRequired.into());
    }
    serde_json::from_str(&body)
        .map_err(|_| AdapterError::InvalidResponse("平台没有返回可识别的 JSON").into())
}

/// POST a public form without any credentials. Douyu's anonymous H5 play API
/// rejects a request that carries the account cookie (the signature's synthetic
/// did does not match the logged-in session), while the same request succeeds
/// anonymously. Keep the cookie-domain guard, just do not send credentials to
/// this public endpoint.
fn public_post_form_json(url: Url, referer: &str, form: &[(&str, &str)]) -> Result<Value> {
    let client = endpoint_client(&url)?;
    let request = client
        .post(url)
        .header("Referer", referer)
        .header("User-Agent", USER_AGENT)
        .form(form);
    let body = read_response(request.send().map_err(|_| AdapterError::Transport)?)?;
    if body.trim().is_empty() {
        return Err(AdapterError::VerificationRequired.into());
    }
    serde_json::from_str(&body)
        .map_err(|_| AdapterError::InvalidResponse("平台没有返回可识别的 JSON").into())
}

fn endpoint(base: &str, pairs: &[(&str, String)]) -> Result<Url> {
    let mut url = Url::parse(base)?;
    url.query_pairs_mut()
        .extend_pairs(pairs.iter().map(|(key, value)| (*key, value.as_str())));
    Ok(url)
}

/// Extra playback hints the legacy launcher passed to mpv's live-title.lua
/// separately from the media URL: the anchor name, the room title and the
/// nominal bitrate from the platform response.
///
/// Deliberately not part of [`ResolvedStream`]: relay children serialize that value
/// through their environment and every existing caller builds it with a struct
/// literal. Only the launcher-side playback path consumes these hints.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamHints {
    /// 主播昵称 for the window title (Bilibili/Huya/Douyu/Douyin).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    /// 直播间标题 for the bottom control bar (never includes the anchor name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_title: Option<String>,
    /// Nominal kbps from the platform response (Douyu `multirates[].bit`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u32>,
}

pub fn resolve(target: &Target, settings: &Settings) -> Result<ResolvedStream> {
    resolve_with_cookie(target, settings, None)
}

pub fn resolve_authenticated_with_hints(
    target: &Target,
    settings: &Settings,
    vault: &CookieVault,
) -> Result<(ResolvedStream, StreamHints)> {
    with_authenticated_session(target.platform, vault, || {
        resolve_with_hints(target, settings, None)
    })
}

/// Cookie is passed explicitly by the caller for this platform only; never read
/// from an existing application, sent to CDNs, placed in a URL or logged.
pub fn resolve_with_cookie(
    target: &Target,
    settings: &Settings,
    cookie: Option<&str>,
) -> Result<ResolvedStream> {
    resolve_with_hints(target, settings, cookie).map(|(stream, _)| stream)
}

/// Resolve the media URL plus the optional title/bitrate hints the mpv title
/// script needs. Hint lookups stay best effort and never fail an otherwise
/// usable resolve.
pub fn resolve_with_hints(
    target: &Target,
    settings: &Settings,
    cookie: Option<&str>,
) -> Result<(ResolvedStream, StreamHints)> {
    checked_cookie(cookie)?;
    // Revalidate even when a Target was manually constructed instead of parsed.
    Target::parse(&format!("{}:{}", target.platform, target.room))?;
    match target.platform {
        Platform::Bili => match resolve_bili(target, cookie) {
            Ok(result) => Ok(result),
            Err(error)
                if matches!(
                    error.downcast_ref::<AdapterError>(),
                    Some(
                        AdapterError::Transport
                            | AdapterError::InvalidResponse(_)
                            | AdapterError::ReplayUnavailable
                    )
                ) && settings.yt_dlp.is_some() =>
            {
                resolve_ytdlp(target, settings).map(|stream| (stream, StreamHints::default()))
            }
            Err(error) => Err(error),
        },
        Platform::Huya => resolve_huya(target),
        Platform::Douyu => resolve_douyu(target),
        Platform::Douyin => resolve_douyin(target, cookie),
    }
}

fn stream(url: String, title: String, referer: &str) -> Result<ResolvedStream> {
    let checked = validate_media_url(&url)?;
    Ok(ResolvedStream {
        url: checked.to_string(),
        title,
        headers: BTreeMap::from([
            ("Referer".into(), referer.into()),
            ("User-Agent".into(), USER_AGENT.into()),
        ]),
    })
}

// ---- Best-effort playback titles -------------------------------------------------
// The legacy launcher fetched anchor/room titles for the mpv window title. These
// lookups are optional: any transport, status or parse failure falls back to the
// generic "<platform> - <room>" label and never fails a resolve.
//
// The parts are also returned as StreamHints so the mpv title script can show the
// anchor in the window title but only the room title in the bottom control bar,
// exactly like the original launcher's base/media hint files.

fn clean_title(value: &str) -> Option<String> {
    let cleaned: String = value
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect();
    let trimmed = cleaned.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn join_title(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (Some(a), Some(b)) => Some(format!("{a} - {b}")),
        (Some(one), None) | (None, Some(one)) => Some(one),
        (None, None) => None,
    }
}

/// Parse the room title from the Bilibili get_info response.
pub fn bili_title_from_info(root: &Value) -> Option<String> {
    (int_field(root, "code") == Some(0))
        .then(|| str_field(&root["data"], "title"))
        .and_then(|value| clean_title(&value))
}

/// Parse the anchor name from the Bilibili get_anchor_in_room response.
pub fn bili_anchor_from_info(root: &Value) -> Option<String> {
    let info = root.pointer("/data/info")?;
    clean_title(&str_field(info, "uname"))
}

/// Douyu betard room response split into (nickname, room_name).
pub fn douyu_title_parts_from_json(root: &Value) -> (Option<String>, Option<String>) {
    let room = &root["room"];
    (
        clean_title(&str_field(room, "nickname")),
        clean_title(&str_field(room, "room_name")),
    )
}

/// Parse the Douyu betard room response into "nickname - room_name".
pub fn douyu_title_from_json(root: &Value) -> Option<String> {
    let (nick, name) = douyu_title_parts_from_json(root);
    join_title(nick, name)
}

/// The nominal kbps of the Douyu rate the launcher selects for this quality.
///
/// Mirrors the legacy `PickDouyuRate`: `rate` 0 is the original-quality entry and
/// its `bit` is used as-is; the other entries are sorted by bitrate and the
/// middle one (high) / lowest one (smooth) is selected. `bit` is already kbps.
pub fn douyu_selected_bitrate(multirates: &Value, quality: &Quality) -> Option<u32> {
    let mut rates: Vec<(u64, u64)> = multirates
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let rate = int_field(item, "rate")?;
            let bit = int_field(item, "bit")?;
            (bit > 0).then_some((bit as u64, rate as u64))
        })
        .collect();
    let original = rates
        .iter()
        .find(|(_, rate)| *rate == 0)
        .map(|(bit, _)| *bit);
    rates.retain(|(_, rate)| *rate > 0);
    rates.sort_unstable();
    let selected = match quality {
        Quality::Best => original,
        Quality::Low => rates.first().map(|(bit, _)| *bit),
        Quality::High => rates.get(rates.len() / 2).map(|(bit, _)| *bit),
    };
    selected
        .and_then(|bit| u32::try_from(bit).ok())
        .filter(|bit| *bit > 0)
}

/// Recursively find the first string value for a key, bounded in depth.
fn find_string_key(value: &Value, key: &str, depth: usize) -> Option<String> {
    if depth > 8 {
        return None;
    }
    match value {
        Value::Object(map) => {
            if let Some(Value::String(text)) = map.get(key) {
                return Some(text.clone());
            }
            map.values()
                .find_map(|child| find_string_key(child, key, depth + 1))
        }
        Value::Array(items) => items
            .iter()
            .find_map(|child| find_string_key(child, key, depth + 1)),
        _ => None,
    }
}

/// Huya mobile profile response split into (nick, room name).
pub fn huya_title_parts_from_json(root: &Value) -> (Option<String>, Option<String>) {
    let nick = find_string_key(root, "nick", 0).and_then(|value| clean_title(&value));
    let name = find_string_key(root, "roomName", 0)
        .and_then(|value| clean_title(&value))
        .or_else(|| find_string_key(root, "introduction", 0).and_then(|value| clean_title(&value)));
    (nick, name)
}

/// Parse the Huya mobile profile response into "nickname - room name".
pub fn huya_title_from_json(root: &Value) -> Option<String> {
    let (nick, name) = huya_title_parts_from_json(root);
    join_title(nick, name)
}

fn bili_playback_titles(
    room: &str,
    cookie: Option<&str>,
) -> (Option<String>, Option<String>) {
    let info = endpoint(
        "https://api.live.bilibili.com/room/v1/Room/get_info",
        &[("room_id", room.into())],
    )
    .ok()
    .and_then(|url| json_get_with_cookie(url, "https://live.bilibili.com/", cookie).ok());
    let anchor = endpoint(
        "https://api.live.bilibili.com/live_user/v1/UserInfo/get_anchor_in_room",
        &[("roomid", room.into())],
    )
    .ok()
    .and_then(|url| json_get_with_cookie(url, "https://live.bilibili.com/", cookie).ok());
    (
        anchor.as_ref().and_then(bili_anchor_from_info),
        info.as_ref().and_then(bili_title_from_info),
    )
}

fn huya_playback_titles(room: &str) -> (Option<String>, Option<String>) {
    // Huya alias rooms (e.g. a16789 / clkimmy) are not accepted by the profile
    // API. Resolve the public alias page to its numeric profileRoom first so the
    // title still starts with the anchor instead of the generic "虎牙直播".
    let numeric = if room.bytes().all(|byte| byte.is_ascii_digit()) {
        room.to_owned()
    } else {
        match huya_alias_room(room) {
            Ok(numeric) => numeric,
            Err(_) => return (None, None),
        }
    };
    let Some(root) = Url::parse(&format!(
        "https://mp.huya.com/cache.php?m=Live&do=profileRoom&roomid={numeric}"
    ))
    .ok()
    .and_then(|url| json_get(url, "https://www.huya.com/").ok())
    else {
        return (None, None);
    };
    huya_title_parts_from_json(&root)
}

fn douyu_playback_titles(room: &str) -> (Option<String>, Option<String>) {
    let Some(root) = Url::parse(&format!("https://www.douyu.com/betard/{room}"))
        .ok()
        .and_then(|url| json_get(url, "https://www.douyu.com/").ok())
    else {
        return (None, None);
    };
    douyu_title_parts_from_json(&root)
}

fn resolve_bili(target: &Target, cookie: Option<&str>) -> Result<(ResolvedStream, StreamHints)> {
    let init = json_get_with_cookie(
        endpoint(
            "https://api.live.bilibili.com/room/v1/Room/room_init",
            &[("id", target.room.clone())],
        )?,
        "https://live.bilibili.com/",
        cookie,
    )?;
    ensure!(
        int_field(&init, "code") == Some(0),
        "B站房间不存在或房间接口受阻"
    );
    let state = int_field(&init["data"], "live_status");
    match state {
        Some(0) => return Err(AdapterError::Offline.into()),
        Some(1 | 2) => (),
        _ => return Err(AdapterError::InvalidResponse("B站未确认房间状态，未当成下播").into()),
    }
    let room = str_field(&init["data"], "room_id");
    ensure!(!room.is_empty(), "B站未返回标准房间号");
    let quality = match target.quality {
        Quality::Best => "10000",
        Quality::High => "400",
        Quality::Low => "250",
    };
    // Fetch the display title before the play-info request consumes the room string.
    let (anchor, room_title) = bili_playback_titles(&room, cookie);
    let title = join_title(anchor.clone(), room_title.clone())
        .unwrap_or_else(|| format!("B站 · {}", target.room));
    let data = json_get_with_cookie(
        endpoint(
            "https://api.live.bilibili.com/xlive/web-room/v2/index/getRoomPlayInfo",
            &[
                ("room_id", room),
                ("protocol", "0,1".into()),
                ("format", "0,1,2".into()),
                ("codec", "0".into()),
                ("qn", quality.into()),
                ("platform", "web".into()),
                ("ptype", "8".into()),
            ],
        )?,
        "https://live.bilibili.com/",
        cookie,
    )?;
    let result = parse_bili_play(&data, &title);
    if state == Some(2) && int_field(&data, "code") == Some(0) && result.is_err() {
        return Err(AdapterError::ReplayUnavailable.into());
    }
    result.map(|stream| {
        (
            stream,
            StreamHints {
                anchor,
                room_title,
                bitrate_kbps: None,
            },
        )
    })
}

/// Parse the official Bilibili play-info response, preferring FLV/H.264.
pub fn parse_bili_play(root: &Value, title: &str) -> Result<ResolvedStream> {
    if int_field(root, "code") != Some(0) {
        return Err(AdapterError::VerificationRequired.into());
    }
    let streams = root
        .pointer("/data/playurl_info/playurl/stream")
        .and_then(Value::as_array)
        .ok_or(AdapterError::InvalidResponse("B站没有返回直播流列表"))?;
    for wanted in ["flv", "fmp4", "ts"] {
        for transport in streams {
            for format in transport["format"].as_array().into_iter().flatten() {
                if format["format_name"].as_str() != Some(wanted) {
                    continue;
                }
                for codec in format["codec"].as_array().into_iter().flatten() {
                    if codec["codec_name"]
                        .as_str()
                        .is_some_and(|name| name != "avc")
                    {
                        continue;
                    }
                    for route in codec["url_info"].as_array().into_iter().flatten() {
                        let url = format!(
                            "{}{}{}",
                            str_field(route, "host"),
                            str_field(codec, "base_url"),
                            str_field(route, "extra")
                        );
                        if let Ok(result) = stream(url, title.into(), "https://live.bilibili.com/")
                        {
                            return Ok(result);
                        }
                    }
                }
            }
        }
    }
    Err(AdapterError::InvalidResponse("B站没有可用的 H.264 播放地址，可能需要登录").into())
}

fn resolve_ytdlp(target: &Target, settings: &Settings) -> Result<ResolvedStream> {
    let binary = settings
        .yt_dlp
        .as_ref()
        .ok_or_else(|| anyhow!("未设置 yt-dlp 路径"))?;
    let room_url = format!("https://live.bilibili.com/{}", target.room);
    let format = match target.quality {
        Quality::Best => "best[ext=flv][vcodec^=avc]/best[ext=flv]/best",
        Quality::High => "best[format_id^=400][ext=flv]/best[ext=flv]/best",
        Quality::Low => "worst[ext=flv]/worst",
    };
    let mut command = Command::new(binary);
    command
        .args([
            "--ignore-config",
            "--no-playlist",
            "--no-warnings",
            "--no-progress",
            "--socket-timeout",
            "10",
            "-f",
            format,
            "--get-url",
            "--",
            &room_url,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn().context("无法启动指定的 yt-dlp")?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("yt-dlp 输出不可用"))?;
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = send.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(35);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("yt-dlp 解析超时，进程已结束");
        }
        thread::sleep(Duration::from_millis(25));
    };
    ensure!(
        status.success(),
        "yt-dlp 未能解析；可能需要登录或更新 yt-dlp"
    );
    let bytes = receive
        .recv_timeout(Duration::from_secs(1))
        .context("yt-dlp 输出读取超时")??;
    ensure!(bytes.len() <= 1024 * 1024, "yt-dlp 输出超过大小上限");
    let output = String::from_utf8(bytes)?;
    let lines: Vec<&str> = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    ensure!(
        lines.len() == 1,
        "yt-dlp 返回多个音视频地址，尚不支持此格式"
    );
    stream(
        lines[0].trim().into(),
        format!("B站 · {}", target.room),
        "https://live.bilibili.com/",
    )
}

fn resolve_huya(target: &Target) -> Result<(ResolvedStream, StreamHints)> {
    let url = Url::parse(&format!("https://www.huya.com/{}", target.room))?;
    let page = get(url, "https://www.huya.com/", None)?;
    let mut result = match parse_huya_page(&page, &target.quality, epoch_millis(), 12_345_678) {
        Ok(result) => result,
        Err(error) => {
            // A page that explicitly announces a replay is not "offline": report a
            // replay-unavailable stream so the session can wait for the live feed.
            if huya_page_replay(&page) {
                return Err(AdapterError::ReplayUnavailable.into());
            }
            return Err(error);
        }
    };
    let (anchor, room_title) = huya_playback_titles(&target.room);
    if let Some(title) = join_title(anchor.clone(), room_title.clone()) {
        result.title = title;
    }
    Ok((
        result,
        StreamHints {
            anchor,
            room_title,
            // Huya streams carry demux-bitrate inside mpv, so the legacy launcher
            // never wrote a bitrate hint for this platform.
            bitrate_kbps: None,
        },
    ))
}

fn extract_object(text: &str, start: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    if bytes.get(start) != Some(&b'{') {
        return None;
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, &byte) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else {
            match byte {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return text.get(start..=index);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

pub fn parse_huya_page(
    page: &str,
    quality: &Quality,
    timestamp_ms: u64,
    uid: u64,
) -> Result<ResolvedStream> {
    let config = page
        .find("hyPlayerConfig")
        .ok_or(AdapterError::InvalidResponse(
            "虎牙页面没有播放器配置；可能未开播或页面已变化",
        ))?;
    let marker = page[config..]
        .find("stream:")
        .or_else(|| page[config..].find("\"stream\":"))
        .map(|index| index + config)
        .ok_or(AdapterError::InvalidResponse("虎牙页面没有流配置"))?;
    let start = page[marker..]
        .find('{')
        .map(|index| marker + index)
        .ok_or(AdapterError::InvalidResponse("虎牙流配置不完整"))?;
    let root: Value = serde_json::from_str(
        extract_object(page, start).ok_or(AdapterError::InvalidResponse("虎牙流配置截断"))?,
    )?;
    let first = root["data"]
        .as_array()
        .and_then(|items| items.first())
        .ok_or(AdapterError::Offline)?;
    let mut rates: Vec<u64> = first["vMultiStreamInfo"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| int_field(item, "iBitRate"))
        .filter(|rate| *rate > 0)
        .map(|rate| rate as u64)
        .collect();
    rates.sort_unstable();
    rates.dedup();
    let bitrate = if rates.is_empty() {
        0
    } else {
        match quality {
            Quality::Best => *rates.last().unwrap(),
            Quality::High => rates[rates.len() / 2],
            Quality::Low => rates[0],
        }
    };
    let mut entries: Vec<&Value> = first["gameStreamInfoList"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    entries.sort_by_key(|entry| !str_field(entry, "sFlvUrl").contains("hs."));
    for entry in entries {
        if let Ok(url) = huya_signed_url(entry, bitrate, timestamp_ms, uid) {
            return stream(url, "虎牙直播".into(), "https://www.huya.com/");
        }
    }
    Err(AdapterError::InvalidResponse("虎牙没有可用的 FLV 地址或签名参数").into())
}

pub fn huya_signed_url(entry: &Value, bitrate: u64, timestamp_ms: u64, uid: u64) -> Result<String> {
    let anti = str_field(entry, "sFlvAntiCode");
    let params: BTreeMap<String, String> = url::form_urlencoded::parse(anti.as_bytes())
        .into_owned()
        .collect();
    let ws_time = params.get("wsTime").ok_or_else(|| anyhow!("缺少 wsTime"))?;
    let fm = STANDARD.decode(params.get("fm").ok_or_else(|| anyhow!("缺少 fm"))?)?;
    let prefix = std::str::from_utf8(&fm)?
        .split('_')
        .next()
        .unwrap_or_default();
    ensure!(!prefix.is_empty(), "无效 fm 前缀");
    let ctype = params
        .get("ctype")
        .map(String::as_str)
        .unwrap_or("huya_live");
    let fs = params.get("fs").map(String::as_str).unwrap_or("");
    let stream_name = str_field(entry, "sStreamName");
    let extension = str_field(entry, "sFlvUrlSuffix");
    ensure!(
        !stream_name.is_empty() && !stream_name.contains(['/', '?', '#']) && extension == "flv",
        "无效虎牙流名称"
    );
    let rotated = ((uid << 8) | (uid >> 24)) & 0xffff_ffff;
    let sequence = uid
        .checked_add(timestamp_ms)
        .ok_or_else(|| anyhow!("签名时间溢出"))?;
    let hash = format!("{:x}", md5::compute(format!("{sequence}|{ctype}|100")));
    let secret = format!(
        "{:x}",
        md5::compute(format!("{prefix}_{rotated}_{stream_name}_{hash}_{ws_time}"))
    );
    let base = str_field(entry, "sFlvUrl");
    let mut url = validate_media_url(&format!(
        "{}/{stream_name}.{extension}",
        base.trim_end_matches('/')
    ))?;
    url.query_pairs_mut().extend_pairs([
        ("wsSecret", secret),
        ("wsTime", ws_time.clone()),
        ("ctype", ctype.into()),
        ("fs", fs.into()),
        ("seqid", sequence.to_string()),
        ("u", rotated.to_string()),
        ("sdk_sid", timestamp_ms.to_string()),
        ("ratio", bitrate.to_string()),
        ("t", "100".into()),
        ("ver", "1".into()),
        ("sv", "2401090219".into()),
        ("codec", "264".into()),
    ]);
    Ok(url.to_string())
}

pub fn douyu_auth(
    room: &str,
    timestamp: u64,
    key: &str,
    random: &str,
    rounds: usize,
    special: bool,
) -> Result<String> {
    ensure!(
        rounds <= 1000 && key.len() <= 1024 && random.len() <= 1024,
        "斗鱼签名参数超出上限"
    );
    let mut chain = random.to_owned();
    for _ in 0..rounds {
        chain = format!("{:x}", md5::compute(format!("{chain}{key}")));
    }
    let suffix = if special {
        String::new()
    } else {
        format!("{room}{timestamp}")
    };
    Ok(format!(
        "{:x}",
        md5::compute(format!("{chain}{key}{suffix}"))
    ))
}

pub fn pick_douyu_rate(data: &Value, quality: &Quality) -> i64 {
    if matches!(quality, Quality::Best) {
        return 0;
    }
    let mut rates: Vec<(i64, i64)> = data
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let rate = int_field(item, "rate")?;
            let bit = int_field(item, "bit")?;
            (rate > 0 && bit > 0).then_some((bit, rate))
        })
        .collect();
    rates.sort_unstable();
    if rates.is_empty() {
        return 0;
    }
    rates[if matches!(quality, Quality::Low) {
        0
    } else {
        rates.len() / 2
    }]
    .1
}

fn resolve_douyu(target: &Target) -> Result<(ResolvedStream, StreamHints)> {
    let did = "10000000000000000000000000001501"; // Synthetic anonymous protocol value, not a device identifier.
    let encryption = public_json_get(
        endpoint(
            "https://www.douyu.com/wgapi/livenc/liveweb/websec/getEncryption",
            &[("did", did.into())],
        )?,
        "https://www.douyu.com/",
    )?;
    ensure!(
        int_field(&encryption, "error") == Some(0),
        "斗鱼签名接口受阻"
    );
    let data = &encryption["data"];
    let timestamp = epoch_millis() / 1000;
    let key = str_field(data, "key");
    let random = str_field(data, "rand_str");
    let encrypted = str_field(data, "enc_data");
    ensure!(
        !key.is_empty() && !random.is_empty() && !encrypted.is_empty(),
        "斗鱼未返回完整签名参数"
    );
    let rounds = int_field(data, "enc_time")
        .filter(|rounds| *rounds >= 0)
        .ok_or_else(|| anyhow!("斗鱼签名轮数无效"))? as usize;
    let auth = douyu_auth(
        &target.room,
        timestamp,
        &key,
        &random,
        rounds,
        int_field(data, "is_special") == Some(1),
    )?;
    let url = Url::parse(&format!(
        "https://www.douyu.com/lapi/live/getH5PlayV1/{}",
        target.room
    ))?;
    // Only used on the failure path: one official read decides whether the room is
    // a looping recording instead of an offline room.
    let replay_room = || -> bool {
        Url::parse(&format!("https://www.douyu.com/betard/{}", target.room))
            .ok()
            .and_then(|url| public_json_get(url, "https://www.douyu.com/").ok())
            .is_some_and(|root| {
                let (show_status, video_loop) = douyu_room_flags(&root);
                douyu_replay_from_flags(show_status, video_loop)
            })
    };
    let fetch = |rate: i64| -> Result<Value> {
        let rate_text = rate.to_string();
        let timestamp_text = timestamp.to_string();
        let root = public_post_form_json(
            url.clone(),
            "https://www.douyu.com/",
            &[
                ("enc_data", encrypted.as_str()),
                ("tt", timestamp_text.as_str()),
                ("did", did),
                ("auth", auth.as_str()),
                ("cdn", ""),
                ("rate", rate_text.as_str()),
                ("hevc", "0"),
                ("fa", "0"),
                ("ive", "0"),
            ],
        )?;
        match int_field(&root, "error") {
            Some(0) => Ok(root["data"].clone()),
            Some(102) | Some(104) => {
                if replay_room() {
                    return Err(AdapterError::ReplayUnavailable.into());
                }
                Err(AdapterError::Offline.into())
            }
            _ => Err(AdapterError::InvalidResponse("斗鱼取流失败，可能需要登录或网页验证").into()),
        }
    };
    let mut play = fetch(0)?;
    // The rate-0 response carries the full multirates table: the selected rate is
    // used for the second request, while its nominal kbps becomes the bitrate hint
    // (Douyu FLV metadata has no videodatarate, exactly like the legacy launcher).
    let selected = pick_douyu_rate(&play["multirates"], &target.quality);
    let bitrate_kbps = douyu_selected_bitrate(&play["multirates"], &target.quality);
    if selected > 0 {
        play = fetch(selected)?;
    }
    let base = str_field(&play, "rtmp_url");
    let path = str_field(&play, "rtmp_live");
    if base.is_empty() || path.is_empty() {
        if replay_room() {
            return Err(AdapterError::ReplayUnavailable.into());
        }
        bail!("斗鱼没有返回直播流，房间可能未开播");
    }
    let (anchor, room_title) = douyu_playback_titles(&target.room);
    let title = join_title(anchor.clone(), room_title.clone())
        .unwrap_or_else(|| format!("斗鱼 · {}", target.room));
    let stream = stream(
        format!("{}/{path}", base.trim_end_matches('/')),
        title,
        "https://www.douyu.com/",
    )?;
    Ok((
        stream,
        StreamHints {
            anchor,
            room_title,
            bitrate_kbps,
        },
    ))
}

fn resolve_douyin(target: &Target, cookie: Option<&str>) -> Result<(ResolvedStream, StreamHints)> {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let nonce_cookie = format!("__ac_nonce={}", &nonce[..21]);
    let page = get(
        Url::parse(&format!("https://live.douyin.com/{}", target.room))?,
        "https://live.douyin.com/",
        cookie.or(Some(&nonce_cookie)),
    )?;
    parse_douyin_page_with_hints(&page, &target.quality)
}

pub(crate) fn pace_payloads(page: &str) -> Vec<String> {
    let marker = "self.__pace_f.push([";
    let bytes = page.as_bytes();
    let mut index = 0;
    let mut values = Vec::new();
    while index < bytes.len() {
        let Some(found) = page[index..].find(marker) else {
            break;
        };
        index += found + marker.len();
        let mut cursor = index;
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        let first_digit = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        if cursor == first_digit {
            continue;
        }
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b',') {
            continue;
        }
        cursor += 1;
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'"') {
            continue;
        }
        let start = cursor;
        cursor += 1;
        let mut escaped = false;
        let mut end = None;
        while cursor < bytes.len() {
            let byte = bytes[cursor];
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                end = Some(cursor + 1);
                break;
            }
            cursor += 1;
        }
        let Some(end) = end else {
            break;
        };
        if let Ok(payload) = serde_json::from_str::<String>(&page[start..end]) {
            values.push(payload);
        }
        index = end;
    }
    values
}

pub fn parse_douyin_page(page: &str, quality: &Quality) -> Result<ResolvedStream> {
    parse_douyin_page_with_hints(page, quality).map(|(stream, _)| stream)
}

/// Room page parser that also returns the anchor/room-title hints. The legacy
/// launcher labeled a Douyin window "主播 - 直播间标题"; the stream title stays the
/// room title so existing session/history text is unchanged.
pub fn parse_douyin_page_with_hints(
    page: &str,
    quality: &Quality,
) -> Result<(ResolvedStream, StreamHints)> {
    let mut missing_room = false;
    for payload in pace_payloads(page) {
        if !payload.contains("roomStore") {
            continue;
        }
        let json = payload
            .find(':')
            .filter(|index| payload[..*index].chars().all(|c| c.is_ascii_alphanumeric()))
            .map_or(payload.as_str(), |index| &payload[index + 1..]);
        let Ok(root) = serde_json::from_str::<Value>(json) else {
            continue;
        };
        let entries: Vec<&Value> = root
            .as_array()
            .map(|values| values.iter().collect())
            .unwrap_or_else(|| vec![&root]);
        for entry in entries {
            let room_info = entry.pointer("/state/roomStore/roomInfo");
            let Some(info) = room_info else {
                continue;
            };
            let Some(room) = info.get("room").filter(|value| value.is_object()) else {
                missing_room = true;
                continue;
            };
            match int_field(room, "status") {
                Some(2) => (),
                // 4 is the platform's explicit "not live" value.
                Some(4) => return Err(AdapterError::Offline.into()),
                // Missing or other values stay unknown: a page change must never
                // turn an actually-live room into "offline".
                _ => {
                    return Err(
                        AdapterError::InvalidResponse("抖音房间状态异常，未当成下播").into(),
                    );
                }
            }
            let order = match quality {
                Quality::Best => ["FULL_HD1", "HD1", "SD1", "SD2"],
                Quality::High => ["HD1", "SD1", "SD2", "FULL_HD1"],
                Quality::Low => ["SD1", "SD2", "HD1", "FULL_HD1"],
            };
            // The anchor object is called "owner" on most pages and "anchor" on
            // some variants; both carry the public nickname the old launcher used.
            let anchor = room
                .get("owner")
                .or_else(|| room.get("anchor"))
                .and_then(|owner| owner.get("nickname"))
                .and_then(Value::as_str)
                .and_then(clean_title);
            let room_title = clean_title(&str_field(room, "title"));
            // The legacy launcher's PotPlayer title is "<anchor> - <room title>";
            // mpv gets the two parts separately through StreamHints. Keep the
            // resolved media title in the same combined form so PotPlayer never
            // starts with the room title alone.
            let stream_title = join_title(anchor.clone(), room_title.clone())
                .or_else(|| room_title.clone())
                .or_else(|| clean_title(&str_field(room, "title")))
                .unwrap_or_default();
            let mut last_error = None;
            for name in order {
                if let Some(address) = room
                    .pointer("/stream_url/flv_pull_url")
                    .and_then(|urls| urls[name].as_str())
                {
                    match stream(
                        address.into(),
                        stream_title.clone(),
                        "https://live.douyin.com/",
                    ) {
                        Ok(stream) => {
                            return Ok((
                                stream,
                                StreamHints {
                                    anchor,
                                    room_title,
                                    bitrate_kbps: None,
                                },
                            ));
                        }
                        // Keep the concrete rejection (e.g. a CDN family outside the
                        // allowlist) instead of hiding it behind "no address": that
                        // message is what an operator needs to extend the allowlist.
                        Err(error) => last_error = Some(error),
                    }
                }
            }
            return Err(last_error.unwrap_or_else(|| {
                AdapterError::InvalidResponse("抖音直播中但未返回可用 FLV 地址").into()
            }));
        }
    }
    if missing_room {
        return Err(AdapterError::RateLimited.into());
    }
    if page.contains("captcha") || page.contains("verify") || page.contains("验证码") {
        return Err(AdapterError::VerificationRequired.into());
    }
    Err(AdapterError::InvalidResponse("抖音页面格式变化或未返回房间信息").into())
}

pub fn search(platform: Platform, keyword: &str) -> Result<Vec<LookupCandidate>> {
    search_with_cookie(platform, keyword, None)
}

pub fn search_with_cookie(
    platform: Platform,
    keyword: &str,
    cookie: Option<&str>,
) -> Result<Vec<LookupCandidate>> {
    checked_cookie(cookie)?;
    let keyword = keyword.trim();
    ensure!(
        !keyword.is_empty() && keyword.chars().count() <= 200,
        "请输入 1–200 字的主播昵称"
    );
    let url = match platform {
        Platform::Bili => {
            let mixin = bili_mixin_key(cookie)?;
            bili_signed_url(
                "https://api.bilibili.com/x/web-interface/wbi/search/type",
                &BTreeMap::from([
                    ("search_type".into(), "live_user".into()),
                    ("keyword".into(), keyword.into()),
                    ("page".into(), "1".into()),
                    ("web_location".into(), "1550101".into()),
                ]),
                &mixin,
                epoch_millis() / 1000,
            )?
        }
        Platform::Huya => endpoint(
            "https://search.cdn.huya.com/",
            &[
                ("m", "Search".into()),
                ("do", "getSearchContent".into()),
                ("q", keyword.into()),
                ("uid", "0".into()),
                ("v", "4".into()),
                ("typ", "-5".into()),
                ("livestate", "0".into()),
                ("rows", "10".into()),
            ],
        )?,
        Platform::Douyu => endpoint(
            "https://www.douyu.com/japi/search/api/searchUser",
            &[
                ("kw", keyword.into()),
                ("page", "1".into()),
                ("pageSize", "10".into()),
            ],
        )?,
        Platform::Douyin => douyin_search_url(keyword)?,
    };
    let root = match platform {
        Platform::Douyin => douyin_search_json(url, cookie)?,
        _ => json_get_with_cookie(
            url,
            match platform {
                Platform::Bili => "https://www.bilibili.com/",
                Platform::Huya => "https://www.huya.com/",
                Platform::Douyu => "https://www.douyu.com/",
                Platform::Douyin => unreachable!(),
            },
            cookie,
        )?,
    };
    parse_search(platform, &root)
}

/// The exact query the official Douyin web search page sends, as recorded by the
/// legacy launcher. Extra device/browser fingerprint fields are part of the
/// request shape; omitting them makes the endpoint return an empty user list.
fn douyin_search_url(keyword: &str) -> Result<Url> {
    let cores = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(8)
        .to_string();
    endpoint(
        "https://www.douyin.com/aweme/v1/web/discover/search/",
        &[
            ("device_platform", "webapp".into()),
            ("aid", "6383".into()),
            ("channel", "channel_pc_web".into()),
            ("search_channel", "aweme_user_web".into()),
            ("keyword", keyword.into()),
            ("search_source", "normal_search".into()),
            ("query_correct_type", "1".into()),
            ("is_filter_search", "0".into()),
            ("from_group_id", String::new()),
            ("disable_rs", "0".into()),
            ("offset", "0".into()),
            ("count", "10".into()),
            ("need_filter_settings", "1".into()),
            ("list_type", "single".into()),
            (
                "pc_search_top_1_params",
                "{\"enable_ai_search_top_1\":1}".into(),
            ),
            ("update_version_code", "170400".into()),
            ("pc_client_type", "1".into()),
            ("pc_libra_divert", "Windows".into()),
            ("support_h265", "1".into()),
            ("support_dash", "0".into()),
            ("cpu_core_num", cores),
            ("version_code", "170400".into()),
            ("version_name", "17.4.0".into()),
            ("cookie_enabled", "true".into()),
            ("screen_width", "1280".into()),
            ("screen_height", "800".into()),
            ("browser_language", "zh-CN".into()),
            ("browser_platform", "Win32".into()),
            ("browser_name", "Edge".into()),
            ("browser_version", "154.0.0.0".into()),
            ("browser_online", "true".into()),
            ("engine_name", "Blink".into()),
            ("engine_version", "154.0.0.0".into()),
            ("os_name", "Windows".into()),
            ("os_version", "10".into()),
            ("device_memory", "8".into()),
            ("platform", "PC".into()),
            ("downlink", "10".into()),
            ("effective_type", "4g".into()),
            ("round_trip_time", "100".into()),
        ],
    )
}

/// Douyin search needs the full browser request headers, not just a Referer.
/// Cookies are still scoped to their exact destination by the request helper.
fn douyin_search_json(url: Url, cookie: Option<&str>) -> Result<Value> {
    let request_cookie = cookie_for_request(&url, cookie)?;
    let client = endpoint_client(&url)?;
    let mut request = client
        .get(url)
        .header("Referer", "https://www.douyin.com/")
        .header("User-Agent", DOUYIN_USER_AGENT)
        .header("Accept", "application/json, text/plain, */*")
        .header("Accept-Language", "zh-CN,zh;q=0.9");
    if let Some(cookie) = request_cookie {
        request = request.header("Cookie", cookie);
    }
    let body = read_response(request.send().map_err(|_| AdapterError::Transport)?)?;
    if std::env::var_os("LIVE_HUB_DOUYIN_SEARCH_DEBUG").is_some() {
        eprintln!(
            "[douyin-search] body_len={} prefix={:?}",
            body.len(),
            body.chars().take(240).collect::<String>()
        );
    }
    if body.trim().is_empty() {
        return Err(AdapterError::VerificationRequired.into());
    }
    serde_json::from_str(&body).map_err(|_| {
        AdapterError::InvalidResponse("抖音搜索没有返回可识别的 JSON（可能需要登录或网页验证）")
            .into()
    })
}

fn clean_label(input: &str) -> String {
    let mut output = String::new();
    let mut in_tag = false;
    for character in input.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => output.push(character),
            _ => {}
        }
    }
    output
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

pub fn parse_search(platform: Platform, root: &Value) -> Result<Vec<LookupCandidate>> {
    let rows = match platform {
        Platform::Bili => {
            if int_field(root, "code") != Some(0) {
                return Err(AdapterError::VerificationRequired.into());
            }
            root.pointer("/data/result")
        }
        Platform::Huya => {
            ensure!(
                int_field(&root["responseHeader"], "status") == Some(0),
                "虎牙平台搜索受阻"
            );
            root.pointer("/response/1/docs")
        }
        Platform::Douyu => {
            ensure!(int_field(root, "error") == Some(0), "斗鱼平台搜索受阻");
            root.pointer("/data/relateUser")
        }
        Platform::Douyin => return parse_douyin_search(root),
    }
    .and_then(Value::as_array)
    .ok_or(AdapterError::InvalidResponse(
        "平台没有返回可识别的主播列表",
    ))?;
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for raw in rows {
        let row = if matches!(platform, Platform::Douyu) {
            &raw["anchorInfo"]
        } else {
            raw
        };
        let (name_key, room_key, title_key) = match platform {
            Platform::Bili => ("uname", "roomid", "title"),
            Platform::Huya => ("game_nick", "room_id", "game_introduction"),
            _ => ("nickName", "rid", "roomName"),
        };
        let room = valid_room(&str_field(row, room_key));
        let account = candidate_account(platform, row);
        // A result without a numeric room is still useful when it carries a stable
        // account identity: the UI shows it as "找到账号，房间待解析" and never
        // lets the account stage become a playable room.
        let Some(identity) = room.clone().or_else(|| account.clone()) else {
            continue;
        };
        if !seen.insert(identity) {
            continue;
        }
        candidates.push(LookupCandidate {
            name: clean_label(&str_field(row, name_key))
                .chars()
                .take(256)
                .collect(),
            room: room.unwrap_or_default(),
            platform,
            title: clean_label(&str_field(row, title_key))
                .chars()
                .take(1024)
                .collect(),
            profile_url: candidate_room_url(platform, &account, &str_field(row, room_key)),
            account_id: account,
            avatar_url: candidate_avatar(row),
            follower_count: candidate_followers(platform, row),
            category: candidate_category(row).unwrap_or_default(),
            description: candidate_description(row).unwrap_or_default(),
            account_handle: candidate_handle(platform, row),
            sec_uid: candidate_sec_uid(platform, row),
            live_status: candidate_live_status(platform, row),
            provider: "platform_search".into(),
            match_reasons: vec!["官方平台搜索结果，身份归属待核验".into()],
            ..Default::default()
        });
        if candidates.len() >= 20 {
            break;
        }
    }
    Ok(candidates)
}

fn valid_room(value: &str) -> Option<String> {
    (!value.is_empty()
        && value != "0"
        && value.len() <= 24
        && value.bytes().all(|byte| byte.is_ascii_digit()))
    .then(|| value.to_owned())
}

/// Official public page for a search result. B站 prefers the account space page
/// when a UID is present; other platforms link the live room when known.
fn candidate_room_url(
    platform: Platform,
    account: &Option<String>,
    raw_room: &str,
) -> Option<String> {
    if let (Platform::Bili, Some(account)) = (platform, account) {
        return Some(format!("https://space.bilibili.com/{account}"));
    }
    let room = valid_room(raw_room)?;
    Some(match platform {
        Platform::Bili => format!("https://live.bilibili.com/{room}"),
        Platform::Huya => format!("https://www.huya.com/{room}"),
        Platform::Douyu => format!("https://www.douyu.com/{room}"),
        Platform::Douyin => return None,
    })
}

/// The public WBI permutation, referenced from blivedm's protocol implementation.
pub fn bili_mixin(image_key: &str, sub_key: &str) -> Result<String> {
    let keys = format!("{image_key}{sub_key}");
    ensure!(
        image_key.len() == 32
            && sub_key.len() == 32
            && keys.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "B站签名密钥格式无效"
    );
    const ORDER: [usize; 32] = [
        46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42, 19,
        29, 28, 14, 39, 12, 38, 41, 13,
    ];
    Ok(ORDER
        .iter()
        .map(|index| keys.as_bytes()[*index] as char)
        .collect())
}

pub fn bili_mixin_key(cookie: Option<&str>) -> Result<String> {
    let nav = json_get_with_cookie(
        Url::parse("https://api.bilibili.com/x/web-interface/nav")?,
        "https://www.bilibili.com/",
        cookie,
    )?;
    let images = &nav["data"]["wbi_img"];
    let image = Url::parse(&str_field(images, "img_url")).context("B站没有返回签名图片标识")?;
    let sub = Url::parse(&str_field(images, "sub_url")).context("B站没有返回签名子标识")?;
    let key = |url: &Url| -> String {
        url.path_segments()
            .and_then(|mut parts| parts.next_back())
            .unwrap_or("")
            .split('.')
            .next()
            .unwrap_or("")
            .into()
    };
    bili_mixin(&key(&image), &key(&sub))
}

/// Sign a Bilibili API query. This helper is pure and does not expose credentials.
pub fn bili_signed_url(
    base: &str,
    params: &BTreeMap<String, String>,
    mixin: &str,
    timestamp: u64,
) -> Result<Url> {
    let mut url = Url::parse(base)?;
    ensure!(
        url.scheme() == "https"
            && matches!(
                url.host_str(),
                Some("api.bilibili.com" | "api.live.bilibili.com")
            )
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "B站签名接口地址无效"
    );
    ensure!(
        mixin.len() == 32 && mixin.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "B站混合签名密钥无效"
    );
    ensure!(
        !params.contains_key("w_rid") && !params.contains_key("wts") && params.len() <= 50,
        "B站签名参数无效"
    );
    let mut sorted = params.clone();
    sorted.insert("wts".into(), timestamp.to_string());
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in &sorted {
        ensure!(
            key.len() <= 100 && value.len() <= 4096,
            "B站签名参数超过上限"
        );
        let filtered: String = value.chars().filter(|c| !"!'()*".contains(*c)).collect();
        serializer.append_pair(key, &filtered);
    }
    let canonical = serializer.finish();
    let digest = format!("{:x}", md5::compute(format!("{canonical}{mixin}")));
    url.set_query(Some(&format!("{canonical}&w_rid={digest}")));
    Ok(url)
}

/// Server-verified logged-in UID for the current request-scoped Bili session.
/// A missing/expired login returns None; protocol or vault errors remain errors.
pub fn bili_login_uid() -> Result<Option<u64>> {
    let root = json_get(
        Url::parse("https://api.bilibili.com/x/web-interface/nav")?,
        "https://www.bilibili.com/",
    )?;
    if int_field(&root, "code") == Some(-101) {
        return Ok(None);
    }
    ensure!(int_field(&root, "code") == Some(0), "B站登录状态查询受阻");
    let data = &root["data"];
    if data["isLogin"].as_bool() != Some(true) {
        return Ok(None);
    }
    let uid = data["mid"]
        .as_u64()
        .or_else(|| data["mid"].as_str()?.parse::<u64>().ok())
        .filter(|uid| *uid > 0)
        .ok_or(AdapterError::InvalidResponse("B站登录状态缺少有效用户标识"))?;
    Ok(Some(uid))
}

/// Fetch a server-issued anonymous Bilibili identifier; never synthesize it from
/// the user's device, inspect a browser profile, or persist it here.
pub fn bili_anonymous_identity() -> Result<String> {
    let root = json_get(
        Url::parse("https://api.bilibili.com/x/frontend/finger/spi")?,
        "https://www.bilibili.com/",
    )?;
    ensure!(
        int_field(&root, "code") == Some(0),
        "B站没有接受匿名会话请求"
    );
    let identity = str_field(&root["data"], "b_3");
    ensure!(
        !identity.is_empty()
            && identity.len() <= 200
            && identity
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "B站没有返回有效匿名会话标识"
    );
    Ok(identity)
}

/// Return the official getDanmuInfo data plus real_room_id for short-room aliases.
pub fn bili_danmaku_info(room: &str, cookie: Option<&str>) -> Result<Value> {
    Target::parse(&format!("bili:{room}"))?;
    let init = json_get_with_cookie(
        endpoint(
            "https://api.live.bilibili.com/room/v1/Room/room_init",
            &[("id", room.into())],
        )?,
        "https://live.bilibili.com/",
        cookie,
    )?;
    ensure!(int_field(&init, "code") == Some(0), "B站房间解析失败");
    let real = str_field(&init["data"], "room_id");
    ensure!(
        !real.is_empty() && real.bytes().all(|byte| byte.is_ascii_digit()),
        "B站未返回标准房间号"
    );
    let params = BTreeMap::from([("id".into(), real.clone()), ("type".into(), "0".into())]);
    for attempt in 0..2 {
        let mixin = bili_mixin_key(cookie)?;
        let url = bili_signed_url(
            "https://api.live.bilibili.com/xlive/web-room/v1/index/getDanmuInfo",
            &params,
            &mixin,
            epoch_millis() / 1000,
        )?;
        let mut root = json_get_with_cookie(url, "https://live.bilibili.com/", cookie)?;
        if int_field(&root, "code") == Some(-352) && attempt == 0 {
            continue;
        }
        if int_field(&root, "code") != Some(0) {
            return Err(AdapterError::VerificationRequired.into());
        }
        let data = root["data"]
            .as_object_mut()
            .ok_or(AdapterError::InvalidResponse("B站没有返回弹幕服务器配置"))?;
        ensure!(
            !str_field(&Value::Object(data.clone()), "token").is_empty()
                && data.get("host_list").is_some_and(Value::is_array),
            "B站没有返回弹幕连接令牌或服务器"
        );
        data.insert("real_room_id".into(), Value::String(real));
        return Ok(Value::Object(data.clone()));
    }
    Err(AdapterError::VerificationRequired.into())
}

fn valid_sec_uid(value: &str) -> bool {
    value.starts_with("MS4wLj")
        && (7..=186).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn numeric(value: String) -> Option<String> {
    (!value.is_empty()
        && value != "0"
        && value.len() <= 24
        && value.bytes().all(|byte| byte.is_ascii_digit()))
    .then_some(value)
}

pub fn parse_douyin_search(root: &Value) -> Result<Vec<LookupCandidate>> {
    if std::env::var_os("LIVE_HUB_DOUYIN_SEARCH_DEBUG").is_some() {
        eprintln!(
            "[douyin-search] status_code={:?} user_list={:?}",
            root.get("status_code"),
            root.get("user_list")
                .and_then(Value::as_array)
                .map(Vec::len)
        );
    }
    if int_field(root, "status_code") != Some(0) {
        return Err(AdapterError::VerificationRequired.into());
    }
    let rows = root["user_list"]
        .as_array()
        .ok_or(AdapterError::InvalidResponse("抖音未返回可识别的用户列表"))?;
    let mut seen = HashSet::new();
    let mut results = Vec::new();
    for row in rows {
        let user = &row["user_info"];
        let sec = str_field(user, "sec_uid");
        if !valid_sec_uid(&sec) || !seen.insert(sec.clone()) {
            continue;
        }
        let account =
            numeric(str_field(user, "uid_str")).or_else(|| numeric(str_field(user, "uid")));
        results.push(LookupCandidate {
            name: clean_label(&str_field(user, "nickname"))
                .chars()
                .take(256)
                .collect(),
            room: String::new(), // Account IDs and internal live-session IDs are NOT web room IDs.
            platform: Platform::Douyin,
            title: clean_label(&str_field(user, "signature"))
                .chars()
                .take(1024)
                .collect(),
            profile_url: Some(format!("https://www.douyin.com/user/{sec}")),
            account_id: account,
            avatar_url: candidate_avatar(user),
            follower_count: candidate_followers(Platform::Douyin, user),
            category: candidate_category(user).unwrap_or_default(),
            description: clean_label(&str_field(user, "signature"))
                .chars()
                .take(1024)
                .collect(),
            account_handle: candidate_handle(Platform::Douyin, user),
            sec_uid: Some(sec.clone()),
            live_status: candidate_live_status(Platform::Douyin, user),
            provider: "platform_search".into(),
            match_reasons: vec!["抖音官方用户搜索结果，直播房间待解析".into()],
            ..Default::default()
        });
        if results.len() >= 20 {
            break;
        }
    }
    if !rows.is_empty() && results.is_empty() {
        return Err(
            AdapterError::InvalidResponse("抖音用户列表缺少有效主页身份，未当作空结果").into(),
        );
    }
    Ok(results)
}

/// Extract the numeric web room id from a Douyin account/profile page payload.
///
/// The public account page may embed the roomStore even when the anchor is
/// offline; this is only a fallback clue, the profile sec_uid is still checked by
/// the caller before it is used. Returns None unless the value is digits and is
/// not the internal all-zero placeholder.
fn extract_douyin_web_rid(page: &str) -> Option<String> {
    for marker in ["\"web_rid\":", "\\\"web_rid\\\":" ] {
        let mut rest = page;
        while let Some(index) = rest.find(marker) {
            rest = &rest[index + marker.len()..];
            let rest = rest.trim_start();
            // Plain JSON uses "123", a Flight string literal uses \"123\".
            let rest = rest
                .strip_prefix("\\\"")
                .or_else(|| rest.strip_prefix('"'))
                .unwrap_or(rest);
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if !digits.is_empty() && digits != "0" && digits.len() <= 24 {
                return Some(digits);
            }
        }
    }
    None
}

fn douyin_offline_candidate(
    candidate: &LookupCandidate,
    sec: &str,
    user: &Value,
    web_rid: &str,
    alias_handle: Option<&str>,
) -> LookupCandidate {
    let mut result = candidate.clone();
    result.room = web_rid.to_owned();
    let name = clean_label(&str_field(user, "nickname"));
    if !name.trim().is_empty() {
        result.name = name.chars().take(256).collect();
    }
    result.sec_uid = Some(sec.to_owned());
    result.profile_url = Some(format!("https://www.douyin.com/user/{sec}"));
    result.account_id = numeric(str_field(user, "uid_str"))
        .or_else(|| numeric(str_field(user, "uid")))
        .or(result.account_id);
    result.account_handle = candidate_handle(Platform::Douyin, user).or(result.account_handle);
    result.avatar_url = candidate_avatar(user).or(result.avatar_url);
    result.follower_count = candidate_followers(Platform::Douyin, user).or(result.follower_count);
    result.description = clean_label(&str_field(user, "signature"))
        .chars()
        .take(1024)
        .collect();
    result.title = result.description.clone();
    result.sources.push(LookupSource {
        url: format!("https://www.douyin.com/user/{sec}"),
        title: "抖音公开账号资料".into(),
        excerpt: result.description.clone(),
        retrieved_at: retrieval_timestamp(),
        supports: vec!["sec_uid".into(), "平台昵称".into()],
    });
    if let Some(handle) = alias_handle {
        result.sources.push(LookupSource {
            url: format!("https://live.douyin.com/{handle}"),
            title: "抖音号对应直播页".into(),
            excerpt: String::new(),
            retrieved_at: retrieval_timestamp(),
            supports: vec!["抖音号".into(), "内部直播场次".into()],
        });
    }
    with_verification(
        result,
        "platform_verify",
        if alias_handle.is_some() {
            "账号资料已核实；抖音号→直播页→数字房间核验；核验时未开播，可直接收藏"
        } else {
            "账号资料已核实；核验时未开播，已绑定数字网页房间，可直接收藏"
        },
        Some("offline".into()),
        LookupSource {
            url: format!("https://live.douyin.com/{web_rid}"),
            title: "抖音直播间（核验时未开播）".into(),
            excerpt: String::new(),
            retrieved_at: retrieval_timestamp(),
            supports: vec!["数字网页房间号".into(), "未开播仍可收藏".into()],
        },
    )
}

fn douyin_profile_page(sec: &str) -> Option<String> {
    let page = get(
        Url::parse(&format!("https://www.douyin.com/user/{sec}")).ok()?,
        "https://www.douyin.com/",
        None,
    )
    .ok()?;
    if std::env::var_os("LIVE_HUB_DOUYIN_VERIFY_DEBUG").is_some() {
        eprintln!(
            "[douyin-verify] profile-page len={} captcha={} roomStore={} web_rid={} roomId={}",
            page.len(),
            page.contains("captcha") || page.contains("验证码"),
            page.contains("roomStore"),
            page.contains("web_rid"),
            page.contains("roomId")
        );
    }
    if page.contains("captcha") || page.contains("verify") || page.contains("验证码") {
        return None;
    }
    Some(page)
}

#[derive(Default)]
struct DouyinProfileRoom {
    session: Option<String>,
    web_rid: Option<String>,
}

/// Parse the public account page's roomStore even when the account is offline.
/// The legacy launcher did exactly this: profile page SSR -> internal session id
/// -> reflow -> owner.web_rid, so an offline account could still be saved.
fn douyin_profile_page_room(page: &str, sec: &str) -> DouyinProfileRoom {
    let mut found = DouyinProfileRoom {
        session: extract_douyin_session_room(page),
        web_rid: extract_douyin_web_rid(page),
    };
    for payload in pace_payloads(page) {
        if !payload.contains("roomStore") {
            continue;
        }
        let json = payload
            .find(':')
            .filter(|index| payload[..*index].chars().all(|c| c.is_ascii_alphanumeric()))
            .map_or(payload.as_str(), |index| &payload[index + 1..]);
        let Ok(root) = serde_json::from_str::<Value>(json) else {
            continue;
        };
        let entries: Vec<&Value> = root
            .as_array()
            .map(|values| values.iter().collect())
            .unwrap_or_else(|| vec![&root]);
        for entry in entries {
            let Some(room) = entry
                .pointer("/state/roomStore/roomInfo")
                .and_then(|info| info.get("room"))
                .filter(|room| room.is_object())
            else {
                continue;
            };
            let owner = room.get("owner").or_else(|| room.get("anchor"));
            if owner.and_then(|owner| owner.get("sec_uid")).and_then(Value::as_str) != Some(sec) {
                continue;
            }
            found.session = found
                .session
                .or_else(|| numeric(str_field(room, "id_str")))
                .or_else(|| numeric(str_field(room, "roomId")));
            found.web_rid = found
                .web_rid
                .or_else(|| owner.and_then(|owner| numeric(str_field(owner, "web_rid"))))
                .or_else(|| numeric(str_field(room, "web_rid")));
            return found;
        }
    }
    found
}

#[derive(Default)]
struct DouyinPageRoom {
    session: Option<String>,
    web_rid: Option<String>,
}

/// Parse one live.douyin.com/<web_rid or handle> page and return the internal
/// session plus numeric web room for the expected sec_uid. This mirrors the
/// legacy launcher's account-alias path even when the anchor is offline: the
/// page SSR still carries the room object, and reflow then yields the web_rid.
fn douyin_page_room(page: &str, sec: &str) -> Option<DouyinPageRoom> {
    let mut fallback = DouyinPageRoom {
        session: extract_douyin_session_room(page),
        web_rid: extract_douyin_web_rid(page),
    };
    for payload in pace_payloads(page) {
        if !payload.contains("roomStore") {
            continue;
        }
        let json = payload
            .find(':')
            .filter(|index| payload[..*index].chars().all(|c| c.is_ascii_alphanumeric()))
            .map_or(payload.as_str(), |index| &payload[index + 1..]);
        let Ok(root) = serde_json::from_str::<Value>(json) else {
            continue;
        };
        let entries: Vec<&Value> = root
            .as_array()
            .map(|values| values.iter().collect())
            .unwrap_or_else(|| vec![&root]);
        for entry in entries {
            let Some(room) = entry
                .pointer("/state/roomStore/roomInfo")
                .and_then(|info| info.get("room"))
                .filter(|room| room.is_object())
            else {
                continue;
            };
            let owner = room.get("owner").or_else(|| room.get("anchor"));
            let owner_sec = owner
                .and_then(|owner| owner.get("sec_uid"))
                .and_then(Value::as_str);
            if owner_sec.is_some_and(|value| value != sec) {
                continue;
            }
            let session = numeric(str_field(room, "id_str"))
                .or_else(|| numeric(str_field(room, "roomId")))
                .or_else(|| numeric(str_field(room, "room_id")));
            let web = owner
                .and_then(|owner| numeric(str_field(owner, "web_rid")))
                .or_else(|| numeric(str_field(room, "web_rid")));
            if session.is_some() || web.is_some() {
                return Some(DouyinPageRoom {
                    session: session.or(fallback.session),
                    web_rid: web.or(fallback.web_rid),
                });
            }
        }
    }
    (fallback.session.is_some() || fallback.web_rid.is_some()).then_some(fallback)
}

fn valid_douyin_handle(handle: &str) -> bool {
    let bytes = handle.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_alphanumeric() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

fn douyin_live_page(handle: &str) -> Option<String> {
    if !valid_douyin_handle(handle) {
        return None;
    }
    let url = Url::parse(&format!("https://live.douyin.com/{handle}")).ok()?;
    let client = endpoint_client(&url).ok()?;
    let cookie = cookie_for_request(&url, None).ok().flatten();
    let mut request = client
        .get(url)
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:154.0) Gecko/20100101 Firefox/154.0",
        )
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", "zh-CN,zh;q=0.9")
        .header("Referer", "https://live.douyin.com/");
    if let Some(cookie) = cookie {
        request = request.header("Cookie", cookie);
    }
    let page = read_response(request.send().ok()?).ok()?;
    if std::env::var_os("LIVE_HUB_DOUYIN_VERIFY_DEBUG").is_some() {
        eprintln!(
            "[douyin-verify] alias-page handle={handle} len={} roomStore={} captcha={}",
            page.len(),
            page.contains("roomStore"),
            page.contains("captcha") || page.contains("验证码")
        );
    }
    // The room page also carries a captcha script string in its normal SSR
    // payload, so only reject an empty/oversized shell; the roomStore parser
    // decides whether this page actually carries the account's room object.
    (!page.trim().is_empty()).then_some(page)
}

/// Read one numeric value from an inline JSON property on a page (untrusted
/// page text, so only short digit strings are accepted).
fn extract_douyin_session_room(page: &str) -> Option<String> {
    let mut rest = page;
    while let Some(index) = rest.find("\"roomId\":") {
        rest = &rest[index + 9..];
        let rest = rest.trim_start();
        let digits: String = rest
            .chars()
            .skip_while(|character| *character == '"')
            .take_while(char::is_ascii_digit)
            .collect();
        if !digits.is_empty() && digits != "0" && digits.len() <= 24 {
            return Some(digits);
        }
    }
    None
}

/// Resolve a selected Douyin account through official profile + reflow data.
/// Profile identity, internal session and numeric web room are checked separately.
/// Explicit cookies are not forwarded to the separate iesdouyin/amemv origins.
pub fn verify_candidate(
    candidate: &LookupCandidate,
    _cookie: Option<&str>,
) -> Result<LookupCandidate> {
    if candidate.platform != Platform::Douyin {
        if candidate.room.trim().is_empty() {
            return Err(anyhow!(
                "该候选只有账号线索（{}），房间待解析；请粘贴官方直播间链接或房间号后重试",
                candidate.name
            ));
        }
        return verify_native_room(candidate);
    }
    let profile = Url::parse(
        candidate
            .profile_url
            .as_deref()
            .ok_or_else(|| anyhow!("候选缺少账号主页"))?,
    )?;
    ensure!(
        profile.scheme() == "https"
            && profile.host_str() == Some("www.douyin.com")
            && profile.path().starts_with("/user/")
            && profile.username().is_empty()
            && profile.password().is_none()
            && profile.port().is_none()
            && profile.query().is_none()
            && profile.fragment().is_none(),
        "候选主页不属于抖音账号"
    );
    let sec = profile.path().strip_prefix("/user/").unwrap_or("");
    ensure!(valid_sec_uid(sec), "候选账号标识无效");
    let root = public_json_get(
        endpoint(
            "https://www.iesdouyin.com/web/api/v2/user/info/",
            &[("sec_uid", sec.into())],
        )?,
        "https://www.douyin.com/",
    )?;
    ensure!(
        int_field(&root, "status_code") == Some(0),
        "抖音账号资料受阻，已保留所选账号"
    );
    let user = &root["user_info"];
    ensure!(
        str_field(user, "sec_uid") == sec,
        "账号资料与所选账号不一致，未绑定房间"
    );
    let mut session = numeric(str_field(user, "room_id"));
    let mut profile_room = DouyinProfileRoom::default();
    let mut alias_room = DouyinPageRoom::default();
    let mut alias_handle: Option<String> = None;
    if session.is_none() {
        // The legacy launcher parsed the public account page's roomStore even
        // when the account was offline, then asked reflow for the web room id.
        if let Some(page) = douyin_profile_page(sec) {
            profile_room = douyin_profile_page_room(&page, sec);
            session = profile_room.session.clone();
        }
    }
    if session.is_none() {
        // Documented legacy resolution path: 账号资料 -> 抖音号 -> 直播页
        // (live.douyin.com/<handle>) -> 数字房间. The alias page keeps the SSR
        // room object for offline anchors, so reflow can return web_rid.
        let handle = candidate_handle(Platform::Douyin, user)
            .or_else(|| candidate.account_handle.clone())
            .filter(|handle| valid_douyin_handle(handle));
        if let Some(handle) = handle
            && let Some(page) = douyin_live_page(&handle)
            && let Some(found) = douyin_page_room(&page, sec)
        {
            session = found.session.clone();
            alias_room = found;
            alias_handle = Some(handle);
        }
    }
    let offline_web = || {
        numeric(str_field(user, "web_rid"))
            .or_else(|| alias_room.web_rid.clone())
            .or_else(|| profile_room.web_rid.clone())
            .or_else(|| numeric(candidate.room.clone()))
            .filter(|value| !value.is_empty() && value != "0")
    };
    let Some(session) = session else {
        if let Some(web) = offline_web() {
            return Ok(douyin_offline_candidate(candidate, sec, user, &web, alias_handle.as_deref()));
        }
        return Err(anyhow!(
            "账号资料已核实，但未返回直播场次或数字网页房间号；可使用直播分享链接"
        ));
    };
    let reflow = match public_json_get(
        endpoint(
            "https://webcast.amemv.com/webcast/room/reflow/info/",
            &[
                ("room_id", session.clone()),
                ("sec_user_id", sec.into()),
                ("live_id", "1".into()),
                ("app_id", "1128".into()),
                ("type_id", "0".into()),
            ],
        )?,
        "https://www.douyin.com/",
    ) {
        Ok(root) => root,
        Err(error) => {
            if let Some(web) = offline_web() {
                return Ok(douyin_offline_candidate(candidate, sec, user, &web, alias_handle.as_deref()));
            }
            return Err(error);
        }
    };
    let mut result = match apply_douyin_reflow(candidate, &session, &reflow) {
        Ok(result) => result,
        Err(error) => {
            if let Some(web) = offline_web() {
                return Ok(douyin_offline_candidate(candidate, sec, user, &web, alias_handle.as_deref()));
            }
            return Err(error);
        }
    };
    // The public profile response may carry an avatar even when the search row
    // did not; preserve whichever credential-free field is available.
    if result.avatar_url.is_none() {
        result.avatar_url = candidate_avatar(user);
    }
    Ok(result)
}

fn verify_native_room(candidate: &LookupCandidate) -> Result<LookupCandidate> {
    let target = Target::new(candidate.platform, candidate.room.clone())?;
    let (root, name) = match target.platform {
        Platform::Bili => {
            let root = json_get(
                endpoint(
                    "https://api.live.bilibili.com/room/v1/Room/get_info",
                    &[("room_id", target.room.clone())],
                )?,
                "https://live.bilibili.com/",
            )?;
            ensure!(int_field(&root, "code") == Some(0), "B站未确认此房间");
            let anchor = json_get(
                endpoint(
                    "https://api.live.bilibili.com/live_user/v1/UserInfo/get_anchor_in_room",
                    &[("roomid", target.room.clone())],
                )?,
                "https://live.bilibili.com/",
            )?;
            let name = str_field(&anchor["data"]["info"], "uname");
            (root, name)
        }
        Platform::Huya => {
            let root = json_get(
                endpoint(
                    "https://mp.huya.com/cache.php",
                    &[
                        ("m", "Live".into()),
                        ("do", "profileRoom".into()),
                        ("roomid", target.room.clone()),
                    ],
                )?,
                "https://www.huya.com/",
            )?;
            ensure!(int_field(&root, "status") == Some(200), "虎牙未确认此房间");
            let name = root
                .pointer("/data/profileInfo/nick")
                .and_then(Value::as_str)
                .or_else(|| root.pointer("/data/nick").and_then(Value::as_str))
                .unwrap_or("")
                .to_owned();
            (root, name)
        }
        Platform::Douyu => {
            let root = json_get(
                Url::parse(&format!("https://www.douyu.com/betard/{}", target.room))?,
                "https://www.douyu.com/",
            )?;
            let name = str_field(&root["room"], "nickname");
            (root, name)
        }
        Platform::Douyin => unreachable!(),
    };
    ensure!(
        !name.trim().is_empty(),
        "平台没有返回可核验的主播名称，候选未采用"
    );
    ensure!(
        clean_label(&name).trim().to_lowercase()
            == clean_label(&candidate.name).trim().to_lowercase(),
        "平台实际主播名称与候选不同，未自动关联同名或错误账号"
    );
    let mut result = candidate.clone();
    result.name = clean_label(&name);
    let row = match target.platform {
        Platform::Bili => &root["data"],
        Platform::Huya => &root["data"],
        Platform::Douyu => &root["room"],
        _ => &root,
    };
    result.follower_count = candidate_followers(target.platform, row).or(result.follower_count);
    result.avatar_url = candidate_avatar(row).or(result.avatar_url);
    if let Some(category) = candidate_category(row) {
        result.category = category;
    }
    if let Some(description) = candidate_description(row) {
        result.description = description;
    }
    let excerpt = result.description.clone();
    let source = LookupSource {
        url: platform_room_page(target.platform, &target.room).unwrap_or_default(),
        title: "平台官方直播间".into(),
        excerpt,
        retrieved_at: retrieval_timestamp(),
        supports: vec!["房间号".into(), "主播名".into(), "开播状态".into()],
    };
    Ok(with_verification(
        result,
        "platform_verify",
        "平台官方直播间接口返回了该主播名，房间号已确认",
        candidate_live_status(target.platform, row),
        source,
    ))
}

pub fn apply_douyin_reflow(
    candidate: &LookupCandidate,
    session: &str,
    root: &Value,
) -> Result<LookupCandidate> {
    ensure!(
        int_field(root, "status_code") == Some(0),
        "抖音直播场次受阻或过期"
    );
    let room = &root["data"]["room"];
    let owner = &room["owner"];
    let sec = candidate
        .profile_url
        .as_deref()
        .and_then(|url| url.strip_prefix("https://www.douyin.com/user/"))
        .ok_or_else(|| anyhow!("候选主页标识无效"))?;
    ensure!(
        valid_sec_uid(sec)
            && str_field(owner, "sec_uid") == sec
            && str_field(room, "id_str") == session,
        "抖音主播身份或场次不一致，未绑定房间"
    );
    let web = numeric(str_field(owner, "web_rid"))
        .or_else(|| numeric(str_field(room, "web_rid")))
        .ok_or_else(|| anyhow!("平台未返回数字网页房间号，未用内部场次号替代"))?;
    let mut result = candidate.clone();
    result.room = web.clone();
    let name = str_field(owner, "nickname");
    if !name.is_empty() {
        result.name = clean_label(&name).chars().take(256).collect();
    }
    result.title = clean_label(&str_field(room, "title"))
        .chars()
        .take(1024)
        .collect();
    result.sec_uid = Some(sec.to_owned());
    let live_status = match int_field(room, "status") {
        Some(2) => Some("live".to_owned()),
        Some(4) => Some("offline".to_owned()),
        _ => None,
    };
    let mut sources = vec![LookupSource {
        url: candidate
            .profile_url
            .clone()
            .unwrap_or_else(|| format!("https://www.douyin.com/user/{sec}")),
        title: "抖音账号主页".into(),
        excerpt: result.description.clone(),
        retrieved_at: retrieval_timestamp(),
        supports: vec!["账号身份".into(), "sec_uid".into()],
    }];
    sources.push(LookupSource {
        url: format!("https://live.douyin.com/{web}"),
        title: "抖音直播间".into(),
        excerpt: result.title.clone(),
        retrieved_at: retrieval_timestamp(),
        supports: vec!["数字网页房间号".into(), "直播标题".into()],
    });
    let verified = with_verification(
        result,
        "platform_verify",
        "抖音账号资料与直播场次身份一致，已解析数字网页房间号",
        live_status,
        sources.remove(0),
    );
    let mut verified = verified;
    verified.sources.extend(sources);
    Ok(verified)
}

// ---- Avatar URLs (favorites cards) ------------------------------------------
// Presentation only. Every lookup is anonymous, best effort and bounded: a
// transport/status/parse failure simply returns None and the UI keeps its
// initial-letter placeholder. No login state, cookies or user files are read.

/// Image hosts the launcher is willing to download avatars from; this is the
/// legacy launcher's allowlist.
pub const AVATAR_HOSTS: &[&str] = &[
    "hdslb.com",
    "huya.com",
    "msstatic.com",
    "douyucdn.cn",
    "douyinpic.com",
    "byteimg.com",
];

/// Flat avatar field names in the legacy launcher's priority order.
const AVATAR_FIELDS: &[&str] = &[
    "face",
    "uface",
    "avatar180",
    "game_avatarUrl180",
    "game_avatarUrl52",
    "owner_avatar_big",
    "owner_avatar_middle",
    "owner_avatar_small",
    "owner_avatar",
    "avatar_mid",
    "avatar_small",
    "avatarUrl",
    "avatar",
    "logo",
];

fn avatar_host_allowed(host: &str) -> bool {
    AVATAR_HOSTS
        .iter()
        .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")))
}

/// Normalizes a platform avatar URL: protocol-relative and plain-HTTP URLs become
/// HTTPS, then the host must be on the avatar allowlist. Returns None otherwise.
pub fn normalize_avatar_url(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 2048 {
        return None;
    }
    let value = if let Some(rest) = value.strip_prefix("//") {
        format!("https://{rest}")
    } else if let Some(rest) = value.strip_prefix("http://") {
        format!("https://{rest}")
    } else {
        value.to_owned()
    };
    let url = Url::parse(&value).ok()?;
    let host = url.host_str()?;
    (url.scheme() == "https"
        && avatar_host_allowed(host)
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none_or(|port| port == 443))
    .then(|| url.to_string())
}

/// Requests a bounded 96px PNG from the Bilibili CDN instead of the original
/// (possibly multi-megabyte or WebP) face image, exactly like the legacy
/// launcher. Other platforms are returned unchanged.
pub fn avatar_thumbnail_url(url: &str) -> String {
    let Ok(mut parsed) = Url::parse(url) else {
        return url.to_owned();
    };
    let host = parsed.host_str().unwrap_or_default();
    if host != "hdslb.com" && !host.ends_with(".hdslb.com") {
        return url.to_owned();
    }
    let path = parsed.path().split('@').next().unwrap_or_default().to_owned();
    parsed.set_path(&format!("{path}@96w_96h_1c.png"));
    parsed.to_string()
}

fn first_avatar_field(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        let Some(found) = find_string_key(value, key, 0) else {
            continue;
        };
        let trimmed = found.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }
    None
}

/// Why an anonymous avatar lookup produced no URL.
///
/// The avatar service surfaces this instead of hiding a platform rate limit
/// behind a generic "no image"; the launcher UI/log can then show the difference
/// between "网络失败" and "平台风控/无头像字段".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AvatarFailure {
    /// Network/DNS/TLS/HTTP transport failure.
    Transport,
    /// The platform answered with an error, rate limit or verification page.
    Blocked,
    /// The platform answered but carried no usable avatar field.
    Missing,
    /// The URL was not on the avatar CDN allowlist.
    Disallowed,
    /// The downloaded bytes were not a decodable, in-limit image.
    InvalidImage,
}

impl AvatarFailure {
    pub fn label(self) -> &'static str {
        match self {
            Self::Transport => "网络错误",
            Self::Blocked => "平台风控或拒绝",
            Self::Missing => "平台未返回头像",
            Self::Disallowed => "头像地址不在白名单",
            Self::InvalidImage => "头像图片无法解码或超出限制",
        }
    }

    fn severity(self) -> u8 {
        match self {
            Self::Blocked => 4,
            Self::InvalidImage => 3,
            Self::Transport => 2,
            Self::Missing => 1,
            Self::Disallowed => 0,
        }
    }
}

fn avatar_failure_from_error(error: &anyhow::Error) -> AvatarFailure {
    match error.downcast_ref::<AdapterError>() {
        Some(AdapterError::Transport) => AvatarFailure::Transport,
        Some(
            AdapterError::RateLimited
            | AdapterError::VerificationRequired
            | AdapterError::InvalidResponse(_)
            | AdapterError::Offline
            | AdapterError::ReplayUnavailable,
        ) => AvatarFailure::Blocked,
        Some(AdapterError::Unsupported(_)) => AvatarFailure::Missing,
        None => AvatarFailure::Transport,
    }
}

fn merge_avatar_failure(current: AvatarFailure, next: AvatarFailure) -> AvatarFailure {
    if next.severity() > current.severity() {
        next
    } else {
        current
    }
}

/// Parse the anchor avatar from the Bilibili `getInfoByRoom` response.
pub fn bili_avatar_from_info(root: &Value) -> Option<String> {
    let face = str_field(&root["data"]["anchor_info"]["base_info"], "face");
    let trimmed = face.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Parse the anchor avatar from the Bilibili `get_anchor_in_room` response.
///
/// This anonymous endpoint is not affected by the `getInfoByRoom` -352 risk
/// control that made favourites cards fall back to their initial letter.
pub fn bili_avatar_from_anchor_info(root: &Value) -> Option<String> {
    let face = str_field(&root["data"]["info"], "face");
    let trimmed = face.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Parse the anchor avatar from the Bilibili `Master/info` response (by uid).
pub fn bili_avatar_from_master_info(root: &Value) -> Option<String> {
    let face = str_field(&root["data"]["info"], "face");
    let trimmed = face.trim();
    if !trimmed.is_empty() {
        return Some(trimmed.to_owned());
    }
    let face = str_field(&root["data"], "face");
    let trimmed = face.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Parse the anchor avatar from the Huya mobile profile response.
pub fn huya_avatar_from_json(root: &Value) -> Option<String> {
    first_avatar_field(root, AVATAR_FIELDS)
}

/// Parse the anchor avatar from the Douyu betard room response.
pub fn douyu_avatar_from_json(root: &Value) -> Option<String> {
    first_avatar_field(&root["room"], AVATAR_FIELDS).or_else(|| first_avatar_field(root, AVATAR_FIELDS))
}

/// Parse the anchor avatar from the Douyin room page payload.
pub fn douyin_avatar_from_page(page: &str) -> Option<String> {
    for payload in pace_payloads(page) {
        if !payload.contains("roomStore") {
            continue;
        }
        let json = payload
            .find(':')
            .filter(|index| payload[..*index].chars().all(|c| c.is_ascii_alphanumeric()))
            .map_or(payload.as_str(), |index| &payload[index + 1..]);
        let Ok(root) = serde_json::from_str::<Value>(json) else {
            continue;
        };
        let entries: Vec<&Value> = root
            .as_array()
            .map(|values| values.iter().collect())
            .unwrap_or_else(|| vec![&root]);
        for entry in entries {
            let Some(room) = entry
                .pointer("/state/roomStore/roomInfo/room")
                .filter(|value| value.is_object())
            else {
                continue;
            };
            let owner = room.get("owner").or_else(|| room.get("anchor"));
            if let Some(owner) = owner {
                if let Some(list) = owner
                    .pointer("/avatar_thumb/url_list")
                    .and_then(Value::as_array)
                {
                    for item in list {
                        let Some(url) = item.as_str().map(str::trim) else {
                            continue;
                        };
                        if !url.is_empty() {
                            return Some(url.to_owned());
                        }
                    }
                }
            }
            if let Some(found) = first_avatar_field(room, AVATAR_FIELDS) {
                return Some(found);
            }
        }
    }
    None
}

/// Resolve the Bilibili anchor avatar through the anonymous chain the legacy
/// launcher could rely on:
///
/// 1. `UserInfo/get_anchor_in_room` (the same endpoint the title path uses; not
///    subject to the `getInfoByRoom` -352 risk control);
/// 2. `live_user/v1/Master/info?uid=` when room_init reported a uid;
/// 3. `xlive/.../getInfoByRoom` as the last fallback.
fn bili_avatar_value(room: &str) -> Result<String, AvatarFailure> {
    let mut failure = AvatarFailure::Missing;
    let init = match json_get(
        endpoint(
            "https://api.live.bilibili.com/room/v1/Room/room_init",
            &[("id", room.to_owned())],
        )
        .map_err(|_| AvatarFailure::Transport)?,
        "https://live.bilibili.com/",
    ) {
        Ok(root) => root,
        Err(error) => return Err(avatar_failure_from_error(&error)),
    };
    if int_field(&init, "code") != Some(0) {
        failure = merge_avatar_failure(failure, AvatarFailure::Blocked);
    }
    let canonical = str_field(&init["data"], "room_id");
    let uid = str_field(&init["data"], "uid");
    let canonical = if canonical.trim().is_empty() {
        room.to_owned()
    } else {
        canonical
    };

    match json_get(
        endpoint(
            "https://api.live.bilibili.com/live_user/v1/UserInfo/get_anchor_in_room",
            &[("roomid", canonical.clone())],
        )
        .map_err(|_| AvatarFailure::Transport)?,
        "https://live.bilibili.com/",
    ) {
        Ok(root) => {
            if int_field(&root, "code") == Some(0) {
                if let Some(face) = bili_avatar_from_anchor_info(&root) {
                    return Ok(face);
                }
            } else {
                failure = merge_avatar_failure(failure, AvatarFailure::Blocked);
            }
        }
        Err(error) => failure = merge_avatar_failure(failure, avatar_failure_from_error(&error)),
    }

    if !uid.trim().is_empty() {
        match json_get(
            endpoint(
                "https://api.live.bilibili.com/live_user/v1/Master/info",
                &[("uid", uid.clone())],
            )
            .map_err(|_| AvatarFailure::Transport)?,
            "https://live.bilibili.com/",
        ) {
            Ok(root) => {
                if int_field(&root, "code") == Some(0) {
                    if let Some(face) = bili_avatar_from_master_info(&root) {
                        return Ok(face);
                    }
                }
            }
            Err(error) => failure = merge_avatar_failure(failure, avatar_failure_from_error(&error)),
        }
    }

    match json_get(
        endpoint(
            "https://api.live.bilibili.com/xlive/web-room/v1/index/getInfoByRoom",
            &[("room_id", canonical)],
        )
        .map_err(|_| AvatarFailure::Transport)?,
        "https://live.bilibili.com/",
    ) {
        Ok(root) => {
            if let Some(face) = bili_avatar_from_info(&root) {
                return Ok(face);
            }
        }
        Err(error) => failure = merge_avatar_failure(failure, avatar_failure_from_error(&error)),
    }
    Err(failure)
}

fn huya_avatar_value(room: &str) -> Result<String, AvatarFailure> {
    let root = json_get(
        Url::parse(&format!(
            "https://mp.huya.com/cache.php?m=Live&do=profileRoom&roomid={room}"
        ))
        .map_err(|_| AvatarFailure::Transport)?,
        "https://www.huya.com/",
    )
    .map_err(|error| avatar_failure_from_error(&error))?;
    if int_field(&root, "status") != Some(200) {
        return Err(AvatarFailure::Blocked);
    }
    huya_avatar_from_json(&root).ok_or(AvatarFailure::Missing)
}

fn douyu_avatar_value(room: &str) -> Result<String, AvatarFailure> {
    let root = json_get(
        Url::parse(&format!("https://www.douyu.com/betard/{room}"))
            .map_err(|_| AvatarFailure::Transport)?,
        "https://www.douyu.com/",
    )
    .map_err(|error| avatar_failure_from_error(&error))?;
    if let Some(error) = root["error"].as_i64().or_else(|| {
        root["error"]
            .as_str()
            .and_then(|value| value.parse::<i64>().ok())
    }) {
        if error != 0 {
            return Err(AvatarFailure::Blocked);
        }
    }
    douyu_avatar_from_json(&root).ok_or(AvatarFailure::Missing)
}

fn douyin_avatar_value(room: &str) -> Result<String, AvatarFailure> {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let nonce_cookie = format!("__ac_nonce={}", &nonce[..21]);
    let page = get(
        Url::parse(&format!("https://live.douyin.com/{room}"))
            .map_err(|_| AvatarFailure::Transport)?,
        "https://live.douyin.com/",
        Some(&nonce_cookie),
    )
    .map_err(|error| avatar_failure_from_error(&error))?;
    if page.contains("captcha") || page.contains("verify") || page.contains("验证码") {
        return Err(AvatarFailure::Blocked);
    }
    douyin_avatar_from_page(&page).ok_or(AvatarFailure::Missing)
}

/// Extract the numeric room id from a Huya alias page. Huya serves the same
/// profile JSON inline for numeric and alias URLs; aliases such as "clkimmy"
/// must be resolved to this number before the avatar profile API accepts them.
fn extract_huya_profile_room(page: &str) -> Option<String> {
    for marker in ["\"profileRoom\":", "\\\"profileRoom\\\":"] {
        let mut rest = page;
        while let Some(index) = rest.find(marker) {
            rest = &rest[index + marker.len()..];
            let rest = rest.trim_start();
            let digits: String = rest
                .chars()
                .skip_while(|character| *character == '"')
                .take_while(char::is_ascii_digit)
                .collect();
            if !digits.is_empty() && digits != "0" && digits.len() <= 24 {
                return Some(digits);
            }
        }
    }
    None
}

fn huya_alias_room(alias: &str) -> Result<String, AvatarFailure> {
    let url = Url::parse(&format!("https://www.huya.com/{alias}"))
        .map_err(|_| AvatarFailure::Transport)?;
    let page = get(url, "https://www.huya.com/", None)
        .map_err(|error| avatar_failure_from_error(&error))?;
    extract_huya_profile_room(&page).ok_or(AvatarFailure::Missing)
}

fn avatar_room_component(platform: Platform, room: &str) -> Result<(), AvatarFailure> {
    let room = room.trim();
    let valid = !room.is_empty()
        && room.len() <= 64
        && room.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
        });
    if !valid {
        return Err(AvatarFailure::Missing);
    }
    if !matches!(platform, Platform::Huya) && !room.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(AvatarFailure::Missing);
    }
    Ok(())
}

/// Resolve the anchor avatar URL for one platform room, keeping the failure reason.
///
/// Accepts numeric room ids, plus Huya aliases (their public page is resolved to
/// the numeric room first). Anonymous public endpoints only; the caller (avatar
/// service) turns a failure into a classified diagnostic and a placeholder,
/// never into a launch/frame error. The returned URL is HTTPS and restricted to
/// the avatar host allowlist.
pub fn avatar_url_for_room(platform: Platform, room: &str) -> Result<String, AvatarFailure> {
    avatar_room_component(platform, room)?;
    let room = room.trim();
    let value = match platform {
        Platform::Bili => bili_avatar_value(room)?,
        Platform::Huya => {
            let numeric = if room.bytes().all(|byte| byte.is_ascii_digit()) {
                room.to_owned()
            } else {
                huya_alias_room(room)?
            };
            huya_avatar_value(&numeric)?
        }
        Platform::Douyu => douyu_avatar_value(room)?,
        Platform::Douyin => douyin_avatar_value(room)?,
    };
    normalize_avatar_url(&value).ok_or(AvatarFailure::Disallowed)
}

/// Resolve the anchor avatar URL for one room, keeping the failure reason.
pub fn avatar_url_detailed(target: &Target) -> Result<String, AvatarFailure> {
    avatar_url_for_room(target.platform, &target.room)
}

/// Resolve the anchor avatar URL for one room (None on any failure).
pub fn avatar_url(target: &Target) -> Option<String> {
    avatar_url_detailed(target).ok()
}

#[cfg(test)]
mod session_tests {
    use super::*;

    #[test]
    fn douyin_web_rid_parser_accepts_plain_and_flight_escaped_payloads() {
        assert_eq!(
            extract_douyin_web_rid("{\"web_rid\":\"989125753255\"}").as_deref(),
            Some("989125753255")
        );
        assert_eq!(
            extract_douyin_web_rid("{\"\\\"web_rid\\\":\\\"6753235366\\\"}").as_deref(),
            Some("6753235366")
        );
        assert_eq!(extract_douyin_web_rid("{\"web_rid\":\"0\"}"), None);
        assert_eq!(extract_douyin_web_rid("no room here"), None);
    }

    #[test]
    fn huya_alias_pages_expose_the_numeric_profile_room() {
        assert_eq!(
            extract_huya_profile_room("{\"profileRoom\":\"196645\"}").as_deref(),
            Some("196645")
        );
        assert_eq!(
            extract_huya_profile_room("window.__DATA__ = {\"profileRoom\":910323}").as_deref(),
            Some("910323")
        );
        assert_eq!(extract_huya_profile_room("{\"profileRoom\":0}"), None);
        assert_eq!(extract_huya_profile_room("{}"), None);
    }

    #[test]
    fn avatar_room_validation_allows_huya_aliases_but_not_other_platform_aliases() {
        assert!(avatar_room_component(Platform::Huya, "clkimmy").is_ok());
        assert!(avatar_room_component(Platform::Bili, "7777").is_ok());
        assert!(avatar_room_component(Platform::Bili, "clkimmy").is_err());
        assert!(avatar_room_component(Platform::Douyin, "../../etc").is_err());
        assert!(avatar_room_component(Platform::Douyin, "989125753255").is_ok());
    }


    #[test]
    fn media_allowlist_accepts_douyin_jomodns_redirect_targets() {
        let url = validate_media_url(
            "https://rt2016n.jomodns.com/pull-flv-q13.douyincdn.com/thirdgame/synthetic.flv?expire=1",
        )
        .expect("observed Douyin redirect CDN");
        assert_eq!(url.host_str(), Some("rt2016n.jomodns.com"));
        assert!(validate_media_url("https://rt2016n.attacker.invalid/live.flv").is_err());
    }

    #[test]
    fn media_allowlist_accepts_bili_smtcdns_redirect_targets() {
        let url = validate_media_url(
            "https://3edc55ba2a75e6efb8adc1bd58417fc8.v.smtcdns.net/d1--cn-gotcha04.bilivideo.com/live-bvc/synthetic.flv?expire=1",
        )
        .expect("observed Bili redirect CDN");
        assert_eq!(url.host_str(), Some("3edc55ba2a75e6efb8adc1bd58417fc8.v.smtcdns.net"));
        assert!(validate_media_url("https://smtcdns.net.attacker.invalid/live.flv").is_err());
    }

    #[test]
    fn public_requests_have_no_cookie_and_stay_on_the_endpoint_allowlist() {
        let ok = Url::parse("https://www.iesdouyin.com/web/api/v2/user/info/").unwrap();
        let request = public_request(&ok, "https://www.douyin.com/")
            .unwrap()
            .build()
            .unwrap();
        assert!(request.headers().get("cookie").is_none());
        assert!(request.headers().get("user-agent").is_some());
        let bad = Url::parse("https://example.com/").unwrap();
        assert!(public_request(&bad, "https://www.douyin.com/").is_err());
    }


    struct ExactHostCookies {
        host: &'static str,
        cookie: &'static str,
    }
    impl CookieSource for ExactHostCookies {
        fn header_for_url(&self, _platform: Platform, url: &str) -> Result<Option<String>> {
            Ok((Url::parse(url)?.host_str() == Some(self.host)).then(|| self.cookie.into()))
        }
    }

    #[test]
    fn scoped_requests_resolve_each_destination_and_never_fall_back_to_a_raw_header() {
        let source = Arc::new(ExactHostCookies {
            host: "live.bilibili.com",
            cookie: "synthetic=live-only",
        });
        let _guard = SessionGuard::enter(Platform::Bili, source);
        let live = Url::parse("https://live.bilibili.com/123").unwrap();
        let api = Url::parse("https://api.live.bilibili.com/room/v1/Room/room_init").unwrap();
        assert_eq!(
            cookie_for_request(&live, None).unwrap().as_deref(),
            Some("synthetic=live-only")
        );
        assert_eq!(
            cookie_for_request(&api, Some("synthetic=must-not-cross-host")).unwrap(),
            None
        );
        assert_eq!(
            cookie_for_request(
                &Url::parse("https://cdn.bilivideo.com/live.flv").unwrap(),
                None
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn session_guard_restores_nested_scope_and_does_not_cross_threads() {
        let url = Url::parse("https://api.bilibili.com/x/test").unwrap();
        let _outer = SessionGuard::enter(
            Platform::Bili,
            Arc::new(ExactHostCookies {
                host: "api.bilibili.com",
                cookie: "synthetic=outer",
            }),
        );
        {
            let _inner = SessionGuard::enter(
                Platform::Bili,
                Arc::new(ExactHostCookies {
                    host: "api.bilibili.com",
                    cookie: "synthetic=inner",
                }),
            );
            assert_eq!(
                cookie_for_request(&url, None).unwrap().as_deref(),
                Some("synthetic=inner")
            );
        }
        assert_eq!(
            cookie_for_request(&url, None).unwrap().as_deref(),
            Some("synthetic=outer")
        );
        assert_eq!(
            thread::spawn(move || cookie_for_request(&url, None).unwrap())
                .join()
                .unwrap(),
            None
        );
    }

    struct BrokenCookies;
    impl CookieSource for BrokenCookies {
        fn header_for_url(&self, _platform: Platform, _url: &str) -> Result<Option<String>> {
            bail!("synthetic encrypted vault failure")
        }
    }
    #[test]
    fn tun_fake_ip_answers_are_accepted_for_pinned_platform_hosts_only() {
        // Anonymous live-platform resolution behind a TUN proxy (Clash fake-IP
        // mode) must not be rejected as "no public address": the fixed host
        // allowlist and TLS hostname still bound the request. Private/loopback
        // answers stay rejected, as do real public answers.
        assert!(!is_public("198.18.1.53".parse().unwrap()));
        assert!(is_proxy_fake_ip("198.18.1.53".parse().unwrap()));
        assert!(is_proxy_fake_ip("198.19.255.254".parse().unwrap()));
        assert!(!is_proxy_fake_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_proxy_fake_ip("127.0.0.1".parse().unwrap()));
        assert!(!is_proxy_fake_ip("10.0.0.1".parse().unwrap()));
        assert!(!is_proxy_fake_ip("2001:2::f5".parse().unwrap()));
        assert!(is_proxy_fake_ip("198.18.0.1".parse().unwrap()));
        assert!(!is_proxy_fake_ip("198.20.0.1".parse().unwrap()));
    }

    #[test]
    fn vault_failure_is_reported_without_plaintext_or_anonymous_fallback() {
        let _guard = SessionGuard::enter(Platform::Bili, Arc::new(BrokenCookies));
        assert!(
            cookie_for_request(
                &Url::parse("https://api.bilibili.com/x/test").unwrap(),
                Some("synthetic=raw-fallback")
            )
            .is_err()
        );
        let settings = Settings {
            yt_dlp: Some("synthetic-not-an-executable".into()),
            ..Default::default()
        };
        let error = resolve(&Target::new(Platform::Bili, "123").unwrap(), &settings).unwrap_err();
        assert_eq!(error.to_string(), "synthetic encrypted vault failure");
    }

    #[test]
    fn douyin_search_url_matches_the_official_web_request_shape() {
        let url = douyin_search_url("花小龙").expect("valid query");
        assert_eq!(url.host_str(), Some("www.douyin.com"));
        assert_eq!(url.path(), "/aweme/v1/web/discover/search/");
        let query: std::collections::BTreeMap<String, String> =
            url.query_pairs().into_owned().collect();
        assert_eq!(query.get("keyword").map(String::as_str), Some("花小龙"));
        assert_eq!(
            query.get("search_channel").map(String::as_str),
            Some("aweme_user_web")
        );
        assert_eq!(query.get("offset").map(String::as_str), Some("0"));
        assert_eq!(query.get("count").map(String::as_str), Some("10"));
        assert_eq!(query.get("browser_name").map(String::as_str), Some("Edge"));
        assert_eq!(query.get("platform").map(String::as_str), Some("PC"));
        assert!(
            query
                .get("pc_search_top_1_params")
                .is_some_and(|value| value.contains("enable_ai_search_top_1"))
        );
        assert!(query.get("cpu_core_num").is_some());
        assert!(query.get("device_memory").is_some());
    }

    #[test]
    fn douyin_search_result_maps_profile_identity() {
        let root = serde_json::json!({
            "status_code": 0,
            "user_list": [{
                "user_info": {
                    "uid": "1125513249165984",
                    "short_id": "3842869016",
                    "nickname": "花小龙",
                    "signature": "synthetic signature",
                    "sec_uid": "MS4wLjABAAAA_synthetic",
                    "unique_id": "huaxiaolong"
                }
            }]
        });
        let rows = parse_douyin_search(&root).expect("parsed");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].platform, Platform::Douyin);
        assert_eq!(rows[0].name, "花小龙");
        assert_eq!(
            rows[0].profile_url.as_deref(),
            Some("https://www.douyin.com/user/MS4wLjABAAAA_synthetic")
        );
        assert_eq!(rows[0].account_id.as_deref(), Some("1125513249165984"));
        assert_eq!(rows[0].sec_uid.as_deref(), Some("MS4wLjABAAAA_synthetic"));
    }
}
