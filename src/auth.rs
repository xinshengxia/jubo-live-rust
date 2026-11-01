//! Explicit, platform-scoped cookie imports stored with user-level Windows DPAPI.
//!
//! No browser/profile discovery or legacy-application access occurs here. Plain
//! Cookie headers have no domain metadata and are conservatively bound to the
//! selected platform's canonical live host. Netscape imports preserve scope.

use crate::core::Platform;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

const MAX_IMPORT_BYTES: usize = 512 * 1024;
const MAX_ENCRYPTED_BYTES: u64 = 1024 * 1024;
const MAX_COOKIES: usize = 2048;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAGIC: &[u8] = b"JLCOOKIE\x01";

/// A clone carries only the explicitly chosen vault root, never plaintext data.
#[derive(Debug, Clone)]
pub struct CookieVault {
    root: PathBuf,
}

impl CookieVault {
    /// The explicit directory this vault uses; callers may pass it to child
    /// processes so a relay can reopen the same DPAPI store instead of receiving
    /// plaintext cookie values through the environment.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn open(root: PathBuf) -> Result<Self> {
        ensure!(
            !root.as_os_str().is_empty(),
            "Choose an explicit credential directory"
        );
        #[cfg(not(windows))]
        {
            let _ = root;
            bail!(
                "Encrypted cookie storage requires Windows DPAPI; no plaintext fallback is available"
            )
        }
        #[cfg(windows)]
        {
            reject_link(&root)?;
            fs::create_dir_all(&root).context("Cannot create credential directory")?;
            ensure!(root.is_dir(), "Credential root must be a directory");
            Ok(Self { root })
        }
    }

    /// Import only caller-supplied text. Invalid or foreign-domain content fails
    /// before replacing the previously encrypted file.
    pub fn import(&self, platform: Platform, text: &str) -> Result<()> {
        let cookies = parse_import(platform, text, now_seconds()?)?;
        self.store_records(platform, cookies)
    }

    /// Store only caller-supplied, structured cookies captured by an explicitly
    /// started login session. Scope is validated before the existing file is
    /// replaced. This does not discover or read any browser profile.
    pub fn import_captured(&self, platform: Platform, cookies: &[CapturedCookie]) -> Result<()> {
        let now = now_seconds()?;
        ensure!(
            !cookies.is_empty() && cookies.len() <= MAX_COOKIES,
            "Captured cookie count is invalid"
        );
        let mut unique = BTreeMap::new();
        for cookie in cookies {
            let record = cookie.record();
            record.validate(platform)?;
            let key = (
                record.domain.clone(),
                record.path.clone(),
                record.name.clone(),
            );
            if record.expires_at.is_some_and(|expiry| expiry <= now) {
                unique.remove(&key);
            } else {
                unique.insert(key, record);
            }
        }
        ensure!(
            !unique.is_empty(),
            "Captured session has no unexpired cookies"
        );
        self.store_records(platform, unique.into_values().collect())
    }

    fn store_records(&self, platform: Platform, cookies: Vec<CookieRecord>) -> Result<()> {
        let payload = VaultData {
            version: 1,
            platform,
            cookies,
        };
        let mut plaintext =
            serde_json::to_vec(&payload).context("Cannot encode imported cookies")?;
        let encrypted_result = protect(&plaintext, &entropy(platform));
        plaintext.fill(0);
        let encrypted = encrypted_result?;
        ensure!(
            encrypted.len() as u64 <= MAX_ENCRYPTED_BYTES,
            "Encrypted credential data is too large"
        );
        let mut bytes = Vec::with_capacity(MAGIC.len() + encrypted.len());
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&encrypted);
        self.atomic_write(platform, &bytes)
    }

    /// A convenience for the fixed HTTPS live-room origin only. Callers making
    /// API/subdomain requests must use header_for_url for that exact target URL.
    pub fn header_for(&self, platform: Platform) -> Result<Option<String>> {
        self.header_for_url(platform, canonical_url(platform))
    }

    pub fn header_for_url(&self, platform: Platform, address: &str) -> Result<Option<String>> {
        let url = request_url(platform, address)?;
        let Some(data) = self.read(platform)? else {
            return Ok(None);
        };
        cookie_header(&data.cookies, &url, now_seconds()?)
    }

    pub fn remove(&self, platform: Platform) -> Result<()> {
        reject_link(&self.root)?;
        let path = self.path(platform);
        reject_link(&path)?;
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("Cannot remove imported credentials"),
        }
    }

    /// Content fingerprint of the encrypted credential file. The launcher uses it
    /// only to discard a login-health result that described an older credential;
    /// the value is never displayed, logged or exported.
    pub fn credential_stamp(&self, platform: Platform) -> Result<Option<String>> {
        reject_link(&self.root)?;
        let path = self.path(platform);
        reject_link(&path)?;
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("Cannot read encrypted credentials"),
        };
        let metadata = file
            .metadata()
            .context("Cannot inspect encrypted credentials")?;
        ensure!(
            metadata.is_file() && metadata.len() <= MAX_ENCRYPTED_BYTES,
            "Invalid encrypted credential file"
        );
        let mut bytes = Vec::new();
        file.take(MAX_ENCRYPTED_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("Cannot read encrypted credentials")?;
        ensure!(
            bytes.len() as u64 <= MAX_ENCRYPTED_BYTES,
            "Invalid encrypted credential file"
        );
        Ok(Some(format!("{:x}", md5::compute(&bytes))))
    }

    pub fn has_credentials(&self, platform: Platform) -> Result<bool> {
        let Some(data) = self.read(platform)? else {
            return Ok(false);
        };
        let now = now_seconds()?;
        Ok(data
            .cookies
            .iter()
            .any(|cookie| cookie.expires_at.is_none_or(|expiry| expiry > now)))
    }

    /// Value-free local evidence; only an online check may call a login valid.
    pub fn local_login_state(&self, platform: Platform) -> Result<LocalLoginState> {
        let Some(data) = self.read(platform)? else {
            return Ok(LocalLoginState::Missing);
        };
        let now = now_seconds()?;
        let mut key_seen = false;
        for cookie in &data.cookies {
            if !cookie.value.is_empty() && is_login_key(platform, &cookie.name) {
                key_seen = true;
                if cookie.expires_at.is_none_or(|expiry| expiry > now) {
                    return Ok(LocalLoginState::Ready);
                }
            }
        }
        Ok(if key_seen {
            LocalLoginState::Expired
        } else {
            LocalLoginState::AuxiliaryOnly
        })
    }

    fn path(&self, platform: Platform) -> PathBuf {
        self.root.join(format!("{platform}.cookies.dpapi"))
    }

    fn read(&self, platform: Platform) -> Result<Option<VaultData>> {
        reject_link(&self.root)?;
        let path = self.path(platform);
        reject_link(&path)?;
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("Cannot read encrypted credentials"),
        };
        let metadata = file
            .metadata()
            .context("Cannot inspect encrypted credentials")?;
        ensure!(
            metadata.is_file() && metadata.len() <= MAX_ENCRYPTED_BYTES,
            "Invalid encrypted credential file"
        );
        let mut bytes = Vec::new();
        file.take(MAX_ENCRYPTED_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("Cannot read encrypted credentials")?;
        ensure!(
            bytes.len() as u64 <= MAX_ENCRYPTED_BYTES && bytes.starts_with(MAGIC),
            "Unsupported credential file; import a new export explicitly"
        );
        let mut plaintext = unprotect(&bytes[MAGIC.len()..], &entropy(platform))?;
        let parsed = serde_json::from_slice::<VaultData>(&plaintext)
            .context("Invalid protected credential document");
        plaintext.fill(0);
        let data = parsed?;
        ensure!(
            data.version == 1 && data.platform == platform,
            "Protected credentials belong to another platform or version"
        );
        ensure!(
            data.cookies.len() <= MAX_COOKIES,
            "Too many protected cookies"
        );
        for cookie in &data.cookies {
            cookie.validate(platform)?;
        }
        Ok(Some(data))
    }

    fn atomic_write(&self, platform: Platform, bytes: &[u8]) -> Result<()> {
        reject_link(&self.root)?;
        let destination = self.path(platform);
        reject_link(&destination)?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".credentials-")
            .suffix(".tmp")
            .tempfile_in(&self.root)
            .context("Cannot prepare encrypted credential write")?;
        temporary
            .write_all(bytes)
            .context("Cannot write encrypted credentials")?;
        temporary
            .as_file()
            .sync_all()
            .context("Cannot flush encrypted credentials")?;
        // Antivirus/indexer handles may briefly deny Windows replacement. Keep
        // the same encrypted temporary file and retry; never delete the old one.
        for delay in [5_u64, 10, 20, 40, 80, 0] {
            match temporary.persist(&destination) {
                Ok(_) => return Ok(()),
                Err(failure) => {
                    let retryable = failure.error.kind() == std::io::ErrorKind::PermissionDenied
                        || matches!(failure.error.raw_os_error(), Some(32 | 33));
                    if !retryable || delay == 0 {
                        return Err(failure.error).context("Cannot replace encrypted credentials; the previous file was left intact");
                    }
                    temporary = failure.file;
                    thread::sleep(Duration::from_millis(delay));
                }
            }
        }
        unreachable!("the final replacement attempt returns")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalLoginState {
    Missing,
    AuxiliaryOnly,
    Expired,
    Ready,
}

fn is_login_key(platform: Platform, name: &str) -> bool {
    match platform {
        Platform::Bili => name == "SESSDATA",
        Platform::Huya => matches!(name, "udb_biztoken" | "udb_l" | "udb_uid"),
        Platform::Douyu => matches!(name, "dy_auth" | "acf_auth" | "acf_stk" | "LTP0"),
        Platform::Douyin => matches!(name, "sessionid" | "sessionid_ss"),
    }
}

/// An explicitly captured cookie with its original HTTP scope. Debug output
/// intentionally omits the value, which may be an account session credential.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CapturedCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub host_only: bool,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    pub expires_at: Option<u64>,
}

impl std::fmt::Debug for CapturedCookie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapturedCookie")
            .field("name", &self.name)
            .field("value", &"[redacted]")
            .field("domain", &self.domain)
            .field("host_only", &self.host_only)
            .field("path", &self.path)
            .field("secure", &self.secure)
            .field("http_only", &self.http_only)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl CapturedCookie {
    fn record(&self) -> CookieRecord {
        CookieRecord {
            name: self.name.clone(),
            value: self.value.clone(),
            domain: self.domain.clone(),
            host_only: self.host_only,
            path: self.path.clone(),
            secure: self.secure,
            http_only: self.http_only,
            expires_at: self.expires_at,
        }
    }
    pub fn validate(&self, platform: Platform) -> Result<()> {
        self.record().validate(platform)
    }
}

/// Build a request header from a temporary login candidate without persisting it.
pub fn captured_header(
    platform: Platform,
    cookies: &[CapturedCookie],
    address: &str,
    now: u64,
) -> Result<Option<String>> {
    ensure!(
        cookies.len() <= MAX_COOKIES,
        "Captured cookie count exceeds the limit"
    );
    let mut records = Vec::with_capacity(cookies.len());
    for cookie in cookies {
        cookie.validate(platform)?;
        records.push(cookie.record());
    }
    cookie_header(&records, &request_url(platform, address)?, now)
}

pub fn platform_contains_host(platform: Platform, host: &str) -> bool {
    domain_matches(&host.to_ascii_lowercase(), parent_domain(platform))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VaultData {
    version: u32,
    platform: Platform,
    cookies: Vec<CookieRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CookieRecord {
    name: String,
    value: String,
    domain: String,
    host_only: bool,
    path: String,
    secure: bool,
    http_only: bool,
    expires_at: Option<u64>,
}

impl CookieRecord {
    fn validate(&self, platform: Platform) -> Result<()> {
        ensure!(
            !self.name.is_empty()
                && self.name.len() <= 256
                && self.name.bytes().all(cookie_name_byte),
            "Invalid cookie name"
        );
        ensure!(
            self.value.len() <= 16 * 1024 && self.value.bytes().all(cookie_value_byte),
            "Invalid cookie value"
        );
        ensure!(
            self.domain == self.domain.to_ascii_lowercase()
                && valid_domain(&self.domain)
                && domain_matches(&self.domain, parent_domain(platform)),
            "Cookie domain is outside the selected platform"
        );
        ensure!(
            self.path.starts_with('/')
                && self.path.len() <= 2048
                && self
                    .path
                    .bytes()
                    .all(|byte| (0x21..=0x7e).contains(&byte) && byte != b';'),
            "Invalid cookie path"
        );
        // The __Host-/__Secure- prefixes carry constraints even in an export.
        if self.name.starts_with("__Secure-") {
            ensure!(self.secure, "A secure-prefix cookie must be Secure");
        }
        if self.name.starts_with("__Host-") {
            ensure!(
                self.secure && self.host_only && self.path == "/",
                "A host-prefix cookie must be Secure, host-only, and use path /"
            );
        }
        Ok(())
    }
}

fn cookie_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn cookie_value_byte(byte: u8) -> bool {
    matches!(byte, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e)
}

fn canonical_url(platform: Platform) -> &'static str {
    match platform {
        Platform::Bili => "https://live.bilibili.com/",
        Platform::Huya => "https://www.huya.com/",
        Platform::Douyu => "https://www.douyu.com/",
        Platform::Douyin => "https://live.douyin.com/",
    }
}

fn parent_domain(platform: Platform) -> &'static str {
    match platform {
        Platform::Bili => "bilibili.com",
        Platform::Huya => "huya.com",
        Platform::Douyu => "douyu.com",
        Platform::Douyin => "douyin.com",
    }
}

fn valid_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain.len() <= 253
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn domain_matches(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

fn path_matches(request: &str, cookie: &str) -> bool {
    request == cookie
        || request
            .strip_prefix(cookie)
            .is_some_and(|suffix| cookie.ends_with('/') || suffix.starts_with('/'))
}

fn request_url(platform: Platform, address: &str) -> Result<Url> {
    ensure!(
        address.len() <= 16 * 1024,
        "Credential request URL is too long"
    );
    let url = Url::parse(address).context("Invalid credential request URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none(),
        "Credentials require an HTTP(S) platform URL without user information"
    );
    let host = url
        .host_str()
        .context("Credential request URL has no host")?;
    ensure!(
        domain_matches(host, parent_domain(platform)),
        "Refusing to send cookies outside the selected platform"
    );
    ensure!(
        url.port().is_none(),
        "Refusing to send cookies to a custom platform port"
    );
    Ok(url)
}

fn cookie_header(cookies: &[CookieRecord], url: &Url, now: u64) -> Result<Option<String>> {
    let host = url.host_str().context("Credential request has no host")?;
    let mut matching: Vec<_> = cookies
        .iter()
        .filter(|cookie| {
            cookie.expires_at.is_none_or(|expiry| expiry > now)
                && (!cookie.secure || url.scheme() == "https")
                && (if cookie.host_only {
                    host == cookie.domain
                } else {
                    domain_matches(host, &cookie.domain)
                })
                && path_matches(url.path(), &cookie.path)
        })
        .collect();
    // RFC cookie order: more specific paths first. Keep both same-name cookies
    // when their paths differ; request recipients apply their own semantics.
    matching.sort_by_key(|cookie| std::cmp::Reverse(cookie.path.len()));
    let header = matching
        .into_iter()
        .map(|cookie| format!("{}={}", cookie.name, cookie.value))
        .collect::<Vec<_>>()
        .join("; ");
    ensure!(
        header.len() <= MAX_HEADER_BYTES,
        "Selected cookies exceed the request header limit"
    );
    Ok(if header.is_empty() {
        None
    } else {
        Some(header)
    })
}

fn parse_import(platform: Platform, source: &str, now: u64) -> Result<Vec<CookieRecord>> {
    ensure!(
        !source.is_empty() && source.len() <= MAX_IMPORT_BYTES,
        "Cookie export is empty or exceeds 512 KiB"
    );
    let source = source.trim_start_matches('\u{feff}').trim();
    ensure!(!source.is_empty(), "Cookie export is empty");
    let cookies = if source.contains('\t') || source.lines().any(|line| line.starts_with('#')) {
        parse_netscape(platform, source)?
    } else {
        parse_header(platform, source)?
    };
    ensure!(
        cookies.len() <= MAX_COOKIES,
        "Cookie export contains too many entries"
    );
    let mut unique = BTreeMap::new();
    for cookie in cookies {
        cookie.validate(platform)?;
        let key = (
            cookie.domain.clone(),
            cookie.path.clone(),
            cookie.name.clone(),
        );
        if cookie.expires_at.is_some_and(|expiry| expiry <= now) {
            unique.remove(&key);
        } else {
            unique.insert(key, cookie);
        }
    }
    ensure!(
        !unique.is_empty(),
        "Cookie export has no unexpired entries for this platform"
    );
    Ok(unique.into_values().collect())
}

fn parse_netscape(platform: Platform, source: &str) -> Result<Vec<CookieRecord>> {
    let mut cookies = Vec::new();
    for line in source.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let (line, http_only) = if let Some(line) = line.strip_prefix("#HttpOnly_") {
            (line, true)
        } else if line.starts_with('#') {
            continue;
        } else {
            (line, false)
        };
        let fields: Vec<_> = line.split('\t').collect();
        ensure!(
            fields.len() == 7,
            "Netscape cookie lines must have seven tab-separated fields"
        );
        let domain = fields[0]
            .strip_prefix('.')
            .unwrap_or(fields[0])
            .to_ascii_lowercase();
        let subdomains = parse_flag(fields[1])?;
        let secure = parse_flag(fields[3])?;
        let expiry: u64 = fields[4].parse().context("Invalid cookie expiration")?;
        let cookie = CookieRecord {
            name: fields[5].into(),
            value: fields[6].into(),
            domain,
            host_only: !subdomains,
            path: fields[2].into(),
            secure,
            http_only,
            expires_at: if expiry == 0 { None } else { Some(expiry) },
        };
        cookie.validate(platform)?;
        cookies.push(cookie);
        ensure!(
            cookies.len() <= MAX_COOKIES,
            "Cookie export contains too many entries"
        );
    }
    Ok(cookies)
}

fn parse_flag(value: &str) -> Result<bool> {
    if value.eq_ignore_ascii_case("true") {
        Ok(true)
    } else if value.eq_ignore_ascii_case("false") {
        Ok(false)
    } else {
        bail!("Invalid Netscape cookie boolean field")
    }
}

fn parse_header(platform: Platform, source: &str) -> Result<Vec<CookieRecord>> {
    ensure!(
        !source.contains(['\r', '\n']),
        "A Cookie header must be one line"
    );
    let source = if source
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("cookie:"))
    {
        source[7..].trim()
    } else {
        source
    };
    let host = Url::parse(canonical_url(platform))?
        .host_str()
        .context("Missing canonical platform host")?
        .to_owned();
    let mut cookies = Vec::new();
    for pair in source
        .split(';')
        .map(str::trim)
        .filter(|pair| !pair.is_empty())
    {
        let (name, value) = pair
            .split_once('=')
            .context("Import a Cookie header, not a Set-Cookie response")?;
        ensure!(
            ![
                "domain", "path", "expires", "max-age", "secure", "httponly", "samesite"
            ]
            .iter()
            .any(|attribute| name.eq_ignore_ascii_case(attribute)),
            "Cookie attributes require Netscape export format"
        );
        let cookie = CookieRecord {
            name: name.into(),
            value: value.into(),
            domain: host.clone(),
            host_only: true,
            path: "/".into(),
            secure: true,
            http_only: false,
            expires_at: None,
        };
        cookie.validate(platform)?;
        cookies.push(cookie);
        ensure!(
            cookies.len() <= MAX_COOKIES,
            "Cookie export contains too many entries"
        );
    }
    Ok(cookies)
}

fn entropy(platform: Platform) -> Vec<u8> {
    format!("live-hub-rust.cookie-vault.v1.{platform}").into_bytes()
}

fn now_seconds() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System clock precedes the Unix epoch")?
        .as_secs())
}

fn reject_link(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            !metadata.file_type().is_symlink(),
            "Credential paths cannot be symbolic links"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error).context("Cannot inspect credential path"),
    }
    Ok(())
}

#[cfg(windows)]
mod dpapi {
    use super::*;
    use std::ptr;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    struct Output(CRYPT_INTEGER_BLOB);
    impl Drop for Output {
        fn drop(&mut self) {
            if !self.0.pbData.is_null() {
                // DPAPI owns this LocalAlloc buffer. Clear its contents before
                // returning it, and never free the caller's input/entropy slices.
                unsafe {
                    ptr::write_bytes(self.0.pbData, 0, self.0.cbData as usize);
                    LocalFree(self.0.pbData.cast());
                }
            }
        }
    }

    fn input(bytes: &[u8]) -> Result<CRYPT_INTEGER_BLOB> {
        Ok(CRYPT_INTEGER_BLOB {
            cbData: bytes.len().try_into().context("DPAPI input is too large")?,
            pbData: bytes.as_ptr().cast_mut(),
        })
    }

    pub(super) fn transform(bytes: &[u8], entropy: &[u8], encrypt: bool) -> Result<Vec<u8>> {
        let input_blob = input(bytes)?;
        let entropy_blob = input(entropy)?;
        let mut output = Output(CRYPT_INTEGER_BLOB::default());
        // Both input slices outlive the call, DPAPI does not modify them, and the
        // output allocation is released by Output even if validation below fails.
        let success = unsafe {
            if encrypt {
                CryptProtectData(
                    &input_blob,
                    ptr::null(),
                    &entropy_blob,
                    ptr::null(),
                    ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output.0,
                )
            } else {
                CryptUnprotectData(
                    &input_blob,
                    ptr::null_mut(),
                    &entropy_blob,
                    ptr::null(),
                    ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output.0,
                )
            }
        };
        if success == 0 {
            return Err(std::io::Error::last_os_error()).context(if encrypt { "Windows DPAPI could not protect cookies; nothing was saved" } else { "Windows DPAPI could not decrypt cookies for this user/platform; explicitly import a fresh export" });
        }
        ensure!(
            !output.0.pbData.is_null() && output.0.cbData as u64 <= MAX_ENCRYPTED_BYTES,
            "Invalid DPAPI output"
        );
        // The preceding successful DPAPI call provides exactly cbData readable
        // bytes; copy them before Output clears/frees the allocation.
        Ok(
            unsafe { std::slice::from_raw_parts(output.0.pbData, output.0.cbData as usize) }
                .to_vec(),
        )
    }
}

#[cfg(windows)]
pub(crate) fn protect(bytes: &[u8], entropy: &[u8]) -> Result<Vec<u8>> {
    dpapi::transform(bytes, entropy, true)
}
#[cfg(windows)]
pub(crate) fn unprotect(bytes: &[u8], entropy: &[u8]) -> Result<Vec<u8>> {
    dpapi::transform(bytes, entropy, false)
}
#[cfg(not(windows))]
pub(crate) fn protect(_bytes: &[u8], _entropy: &[u8]) -> Result<Vec<u8>> {
    bail!("Encrypted cookie storage requires Windows DPAPI; plaintext fallback is disabled")
}
#[cfg(not(windows))]
pub(crate) fn unprotect(_bytes: &[u8], _entropy: &[u8]) -> Result<Vec<u8>> {
    bail!("Encrypted cookie storage requires Windows DPAPI; plaintext fallback is disabled")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netscape_scope_preserves_host_path_secure_httponly_and_expiry() {
        let source = concat!(
            "# Netscape HTTP Cookie File\n",
            "#HttpOnly_.bilibili.com\tTRUE\t/\tTRUE\t2000\tSESSION\tSYNTHETIC_DOMAIN\n",
            "api.live.bilibili.com\tFALSE\t/room\tTRUE\t2000\tHOST\tSYNTHETIC_HOST\n",
            ".bilibili.com\tTRUE\t/\tFALSE\t999\tEXPIRED\tSYNTHETIC_EXPIRED\n",
        );
        let cookies = parse_import(Platform::Bili, source, 1000).unwrap();
        assert_eq!(cookies.len(), 2);
        assert!(
            cookies
                .iter()
                .find(|cookie| cookie.name == "SESSION")
                .unwrap()
                .http_only
        );
        let api = request_url(Platform::Bili, "https://api.live.bilibili.com/room/info").unwrap();
        assert_eq!(
            cookie_header(&cookies, &api, 1000).unwrap().unwrap(),
            "HOST=SYNTHETIC_HOST; SESSION=SYNTHETIC_DOMAIN"
        );
        let near_path =
            request_url(Platform::Bili, "https://api.live.bilibili.com/roommate").unwrap();
        assert_eq!(
            cookie_header(&cookies, &near_path, 1000).unwrap().unwrap(),
            "SESSION=SYNTHETIC_DOMAIN"
        );
        let other_subdomain =
            request_url(Platform::Bili, "https://live.bilibili.com/room/info").unwrap();
        assert_eq!(
            cookie_header(&cookies, &other_subdomain, 1000)
                .unwrap()
                .unwrap(),
            "SESSION=SYNTHETIC_DOMAIN"
        );
        let insecure = request_url(Platform::Bili, "http://live.bilibili.com/").unwrap();
        assert!(cookie_header(&cookies, &insecure, 1000).unwrap().is_none());
        assert!(cookie_header(&cookies, &api, 2000).unwrap().is_none());
    }

    #[test]
    fn plain_cookie_headers_are_bound_to_one_canonical_https_host() {
        let cookies = parse_import(
            Platform::Douyin,
            "Cookie: SESSION=SYNTHETIC_ONE; token=SYNTHETIC_TWO",
            1000,
        )
        .unwrap();
        let canonical = request_url(Platform::Douyin, "https://live.douyin.com/").unwrap();
        assert!(
            cookie_header(&cookies, &canonical, 1000)
                .unwrap()
                .unwrap()
                .contains("SESSION=SYNTHETIC_ONE")
        );
        let api = request_url(Platform::Douyin, "https://www.douyin.com/").unwrap();
        assert!(cookie_header(&cookies, &api, 1000).unwrap().is_none());
        assert!(request_url(Platform::Douyin, "https://douyin.com.example.invalid/").is_err());
        assert!(request_url(Platform::Douyin, "https://www.bilibili.com/").is_err());
        assert!(request_url(Platform::Douyin, "https://user:password@live.douyin.com/").is_err());
    }

    #[test]
    fn invalid_or_foreign_cookie_exports_are_rejected_as_a_whole() {
        for source in [
            ".example.invalid\tTRUE\t/\tTRUE\t0\tSESSION\tSYNTHETIC",
            ".bilibili.com.example.invalid\tTRUE\t/\tTRUE\t0\tSESSION\tSYNTHETIC",
            ".bilibili.com\tTRUE\t/\tTRUE\t0\tSESSION\tgood\r\nInjected: bad",
            "SESSION=good\r\nX-Injected: bad",
            "Set-Cookie: SESSION=bad",
            "SESSION=ok; Path=/",
            ".bilibili.com\tTRUE\t/\tFALSE\t0\t__Secure-SESSION\tSYNTHETIC",
            ".bilibili.com\tTRUE\t/\tTRUE\t0\t__Host-SESSION\tSYNTHETIC",
        ] {
            assert!(parse_import(Platform::Bili, source, 1000).is_err());
        }
        assert!(
            parse_import(
                Platform::Bili,
                ".bilibili.com\tTRUE\t/\tTRUE\t999\tOLD\tSYNTHETIC",
                1000
            )
            .is_err()
        );
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_vault_round_trip_is_encrypted_isolated_and_explicitly_removable() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("credentials");
        let vault = CookieVault::open(root.clone()).unwrap();
        assert!(vault.header_for(Platform::Bili).unwrap().is_none());
        assert!(!vault.has_credentials(Platform::Bili).unwrap());
        vault
            .import(Platform::Bili, "SESSION=SYNTHETIC_SESSION_MARKER")
            .unwrap();
        let encrypted = fs::read(vault.path(Platform::Bili)).unwrap();
        assert!(encrypted.starts_with(MAGIC));
        assert!(
            !encrypted
                .windows(b"SYNTHETIC_SESSION_MARKER".len())
                .any(|bytes| bytes == b"SYNTHETIC_SESSION_MARKER")
        );
        let reopened = CookieVault::open(root.clone()).unwrap();
        assert_eq!(
            reopened.header_for(Platform::Bili).unwrap().as_deref(),
            Some("SESSION=SYNTHETIC_SESSION_MARKER")
        );
        assert!(reopened.header_for(Platform::Huya).unwrap().is_none());
        assert!(
            reopened
                .header_for_url(Platform::Bili, "https://www.huya.com/")
                .is_err()
        );
        assert!(
            vault
                .import(
                    Platform::Bili,
                    ".huya.com\tTRUE\t/\tTRUE\t0\tSESSION\tFOREIGN_SYNTHETIC"
                )
                .is_err()
        );
        assert_eq!(fs::read(vault.path(Platform::Bili)).unwrap(), encrypted);
        fs::write(root.join("unrelated.txt"), "synthetic marker").unwrap();
        vault.remove(Platform::Bili).unwrap();
        assert!(vault.header_for(Platform::Bili).unwrap().is_none());
        assert_eq!(
            fs::read_to_string(root.join("unrelated.txt")).unwrap(),
            "synthetic marker"
        );
    }

    #[cfg(windows)]
    #[test]
    fn copying_ciphertext_between_platforms_cannot_change_its_scope() {
        let directory = tempfile::tempdir().unwrap();
        let vault = CookieVault::open(directory.path().join("credentials")).unwrap();
        vault
            .import(Platform::Bili, "SESSION=SYNTHETIC_PLATFORM_LOCK")
            .unwrap();
        fs::copy(vault.path(Platform::Bili), vault.path(Platform::Huya)).unwrap();
        assert!(vault.header_for(Platform::Huya).is_err());
        let mut bytes = fs::read(vault.path(Platform::Bili)).unwrap();
        *bytes.last_mut().unwrap() ^= 0x80;
        fs::write(vault.path(Platform::Bili), bytes).unwrap();
        assert!(vault.header_for(Platform::Bili).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn failed_encrypted_replacement_preserves_previous_credentials() {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;
        let directory = tempfile::tempdir().unwrap();
        let vault = CookieVault::open(directory.path().join("credentials")).unwrap();
        vault
            .import(Platform::Bili, "SESSION=SYNTHETIC_OLD")
            .unwrap();
        let old = fs::read(vault.path(Platform::Bili)).unwrap();
        let held = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(vault.path(Platform::Bili))
            .unwrap();
        assert!(
            vault
                .import(Platform::Bili, "SESSION=SYNTHETIC_NEW")
                .is_err()
        );
        assert_eq!(fs::read(vault.path(Platform::Bili)).unwrap(), old);
        assert!(!fs::read_dir(&vault.root).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".credentials-")
        }));
        drop(held);
        assert_eq!(
            vault.header_for(Platform::Bili).unwrap().as_deref(),
            Some("SESSION=SYNTHETIC_OLD")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unsupported_platform_never_creates_a_plaintext_vault() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("credentials");
        assert!(CookieVault::open(root.clone()).is_err());
        assert!(!root.exists());
    }
}
