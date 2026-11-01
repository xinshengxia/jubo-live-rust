//! Pure, request-scoped cookie rules shared by the live receivers.
//! Credentials are never persisted here, included in errors, or copied between
//! origins. WSS uses the corresponding HTTPS cookie origin, as browsers do.

use crate::core::Platform;
use crate::platforms::CookieSource;
use anyhow::{Result, ensure};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

pub const BILI_DANMAKU_INFO_URL: &str =
    "https://api.live.bilibili.com/xlive/web-room/v1/index/getDanmuInfo";

fn cookie_url(address: &str) -> Result<Url> {
    let mut url =
        Url::parse(address).map_err(|_| anyhow::anyhow!("Invalid receiver request URL"))?;
    if url.scheme() == "wss" {
        url.set_scheme("https")
            .map_err(|_| anyhow::anyhow!("Invalid receiver origin"))?;
    }
    ensure!(
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && url.port().is_none(),
        "Receiver credentials require a standard HTTPS origin"
    );
    Ok(url)
}

pub fn scoped_cookie(
    source: Option<&dyn CookieSource>,
    platform: Platform,
    address: &str,
) -> Result<Option<String>> {
    let url = cookie_url(address)?;
    let value = source
        .map(|source| source.header_for_url(platform, url.as_str()))
        .transpose()?
        .flatten();
    if let Some(value) = &value {
        ensure!(
            value.len() <= 64 * 1024 && value.bytes().all(|b| (0x20..=0x7e).contains(&b)),
            "Invalid scoped receiver cookie"
        );
    }
    Ok(value.filter(|value| !value.is_empty()))
}

pub fn cookie_named(header: Option<&str>, wanted: &str) -> Option<String> {
    header?.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        (name == wanted && !value.is_empty()).then(|| value.to_owned())
    })
}

fn append_missing(header: &mut String, name: &str, value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 4096
            && value
                .bytes()
                .all(|b| (0x21..=0x7e).contains(&b) && b != b';'),
        "Invalid ephemeral receiver cookie"
    );
    if cookie_named(Some(header.as_str()), name).is_none() {
        if !header.is_empty() {
            header.push_str("; ");
        }
        header.push_str(name);
        header.push('=');
        header.push_str(value);
    }
    Ok(())
}

/// A login packet uses a server-verified UID only when the *danmaku API* request
/// actually carries SESSDATA. Host-only cookies from a different origin cannot
/// accidentally turn an anonymous token into a logged-in handshake.
pub fn bili_uid_for_auth(danmaku_cookie: Option<&str>, verified_uid: Option<u64>) -> Result<u64> {
    if cookie_named(danmaku_cookie, "SESSDATA").is_none() {
        return Ok(0);
    }
    let uid = verified_uid
        .filter(|uid| *uid > 0)
        .ok_or_else(|| anyhow::anyhow!("B站登录身份无法核验，请重新导入有效凭据"))?;
    if let Some(value) = cookie_named(danmaku_cookie, "DedeUserID") {
        ensure!(
            value.parse::<u64>().ok() == Some(uid),
            "B站登录凭据的用户身份不一致，请重新导入同一账号的凭据"
        );
    }
    Ok(uid)
}

/// Adds an ephemeral anonymous buvid only where the vault has no applicable one.
/// Imported cookies are still looked up for every exact API/WebSocket URL.
pub struct BiliCookieSource {
    source: Arc<dyn CookieSource>,
    buvid: String,
}
impl BiliCookieSource {
    pub fn new(source: Arc<dyn CookieSource>, buvid: String) -> Result<Self> {
        let mut checked = String::new();
        append_missing(&mut checked, "buvid3", &buvid)?;
        Ok(Self { source, buvid })
    }
}
impl CookieSource for BiliCookieSource {
    fn header_for_url(&self, platform: Platform, address: &str) -> Result<Option<String>> {
        ensure!(platform == Platform::Bili, "B站会话不能用于其他平台");
        let url = cookie_url(address)?;
        let host = url.host_str().unwrap_or("");
        ensure!(
            host == "bilibili.com" || host.ends_with(".bilibili.com"),
            "B站会话不能用于其他来源"
        );
        let mut header = self
            .source
            .header_for_url(platform, url.as_str())?
            .unwrap_or_default();
        if let Some(existing) = cookie_named(Some(&header), "buvid3") {
            ensure!(
                existing == self.buvid,
                "B站凭据的设备会话标识不一致，请重新导入同一会话的凭据"
            );
        }
        append_missing(&mut header, "buvid3", &self.buvid)?;
        Ok(Some(header))
    }
}

#[derive(Clone)]
struct ResponseCookie {
    value: String,
    domain: String,
    host_only: bool,
    path: String,
    secure: bool,
    expires: Option<u64>,
}

/// Per-connection response cookies, never persisted. Only ttwid is needed by
/// this protocol; Domain, HostOnly, Path, Secure and expiry remain enforced.
#[derive(Default)]
pub struct DouyinSessionCookies {
    values: Vec<ResponseCookie>,
}

impl DouyinSessionCookies {
    pub fn absorb(&mut self, response_address: &str, set_cookie: &str, now: u64) -> Result<()> {
        let url = cookie_url(response_address)?;
        let host = url.host_str().unwrap_or("");
        ensure!(
            host == "douyin.com" || host.ends_with(".douyin.com"),
            "Unexpected Douyin response origin"
        );
        let mut parts = set_cookie.split(';');
        let Some((name, value)) = parts.next().and_then(|p| p.trim().split_once('=')) else {
            return Ok(());
        };
        if name != "ttwid" {
            return Ok(());
        }
        ensure!(
            value.len() <= 4096
                && value
                    .bytes()
                    .all(|b| (0x21..=0x7e).contains(&b) && b != b';'),
            "Invalid response cookie"
        );
        let path = url
            .path()
            .rsplit_once('/')
            .map(|(p, _)| if p.is_empty() { "/" } else { p })
            .unwrap_or("/")
            .to_owned();
        let mut item = ResponseCookie {
            value: value.to_owned(),
            domain: host.to_owned(),
            host_only: true,
            path,
            secure: false,
            expires: None,
        };
        let mut max_age = None;
        for attribute in parts {
            let (name, value) = attribute
                .trim()
                .split_once('=')
                .unwrap_or((attribute.trim(), ""));
            match name.to_ascii_lowercase().as_str() {
                "domain" => {
                    let domain = value.trim().trim_start_matches('.').to_ascii_lowercase();
                    ensure!(
                        domain == "douyin.com" || domain.ends_with(".douyin.com"),
                        "Response cookie has a foreign domain"
                    );
                    ensure!(
                        host == domain
                            || host
                                .strip_suffix(&domain)
                                .is_some_and(|prefix| prefix.ends_with('.')),
                        "Response cookie domain does not own this origin"
                    );
                    item.domain = domain;
                    item.host_only = false;
                }
                "path" if value.starts_with('/') => item.path = value.to_owned(),
                "secure" => item.secure = true,
                "max-age" => max_age = value.trim().parse::<i64>().ok(),
                "expires" => {
                    item.expires = chrono::DateTime::parse_from_rfc2822(value.trim())
                        .ok()
                        .map(|date| date.timestamp().max(0) as u64)
                }
                _ => {}
            }
        }
        if let Some(seconds) = max_age {
            item.expires = Some(if seconds <= 0 {
                now
            } else {
                now.saturating_add(seconds as u64)
            });
        }
        self.values.retain(|old| {
            !(old.domain == item.domain && old.path == item.path && old.host_only == item.host_only)
        });
        if !item.value.is_empty() && item.expires.is_none_or(|expiry| expiry > now) {
            ensure!(
                self.values.len() < 16,
                "Too many ephemeral response cookies"
            );
            self.values.push(item);
        }
        Ok(())
    }

    pub fn header_for(
        &self,
        source: Option<&dyn CookieSource>,
        address: &str,
        nonce: Option<&str>,
        now: u64,
    ) -> Result<Option<String>> {
        let url = cookie_url(address)?;
        let host = url.host_str().unwrap_or("");
        let mut header = scoped_cookie(source, Platform::Douyin, address)?.unwrap_or_default();
        // __ac_nonce is deliberately host-only; it does not follow a redirect or
        // move to a separate WebSocket origin.
        if host == "live.douyin.com" {
            if let Some(nonce) = nonce {
                append_missing(&mut header, "__ac_nonce", nonce)?;
            }
        }
        let mut matches: Vec<_> = self
            .values
            .iter()
            .filter(|cookie| {
                let domain_matches = host == cookie.domain
                    || (!cookie.host_only
                        && host
                            .strip_suffix(&cookie.domain)
                            .is_some_and(|p| p.ends_with('.')));
                let path_matches = url.path() == cookie.path
                    || (url.path().starts_with(&cookie.path)
                        && (cookie.path.ends_with('/')
                            || url.path().as_bytes().get(cookie.path.len()) == Some(&b'/')));
                domain_matches
                    && path_matches
                    && (!cookie.secure || url.scheme() == "https")
                    && cookie.expires.is_none_or(|expiry| expiry > now)
            })
            .collect();
        matches.sort_by_key(|cookie| std::cmp::Reverse(cookie.path.len()));
        for cookie in matches {
            append_missing(&mut header, "ttwid", &cookie.value)?;
        }
        Ok((!header.is_empty()).then_some(header))
    }
}

pub fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
