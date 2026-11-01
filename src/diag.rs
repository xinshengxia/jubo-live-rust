//! Bounded, redacted diagnostic logs (one file per component).
//!
//! Design goals, mirroring the legacy bounded logger but tightened for the
//! native process:
//! * one append-only file per component (<log_dir>/<component>.log);
//! * hard 64 KiB per-file cap: when an append would exceed it, the oldest
//!   whole lines are dropped first so the newest evidence survives;
//! * silent failure (IO errors never propagate, no panic);
//! * UTC timestamps with millisecond precision;
//! * best-effort redaction for credentials, cookies, bearer tokens and URL
//!   query strings (see redact()).
//!
//! init() wires the global directory once; log() is a no-op until then so the
//! library stays side-effect free in tests and non-GUI binaries.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Maximum size of one component log file.
pub const MAX_FILE_BYTES: u64 = 64 * 1024;
/// Maximum characters kept from one message (before the newline is appended).
pub const MAX_MESSAGE_CHARS: usize = 4096;
/// Maximum component name length (after sanitising).
pub const MAX_COMPONENT_CHARS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Sets the global diagnostic directory once. Later calls are ignored.
pub fn init(log_dir: impl Into<PathBuf>) {
    let _ = LOG_DIR.set(log_dir.into());
}

pub fn is_initialized() -> bool {
    LOG_DIR.get().is_some()
}

/// Appends one line to the configured directory. No-op before init().
pub fn log(component: &str, level: Level, message: &str) {
    if let Some(dir) = LOG_DIR.get() {
        let _ = append(dir, component, level, message);
    }
}

/// Path of one component log file (component is sanitised).
pub fn log_path(log_dir: &Path, component: &str) -> PathBuf {
    log_dir.join(format!("{}.log", sanitize_component(component)))
}

/// Appends one line without relying on the global init. Returns false when the
/// write failed (missing permissions, path is a file, ...). Never panics.
pub fn append(log_dir: &Path, component: &str, level: Level, message: &str) -> bool {
    let path = log_path(log_dir, component);
    let message = truncate_chars(&redact(message), MAX_MESSAGE_CHARS);
    let line = format!("{} [{}] {}\n", utc_timestamp(), level.label(), message);
    match std::fs::create_dir_all(log_dir) {
        Ok(()) => {}
        Err(_) => return false,
    }
    let existing = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    let new_len = existing + line.len() as u64;
    if new_len <= MAX_FILE_BYTES {
        use std::io::Write;
        let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path)
        else {
            return false;
        };
        return file.write_all(line.as_bytes()).is_ok();
    }
    // Keep only the newest evidence: read the current file, keep its tail so the
    // combined size fits, cut at the first line break, then append the new line.
    let Ok(current) = std::fs::read(&path) else {
        return false;
    };
    let keep = MAX_FILE_BYTES.saturating_sub(line.len() as u64).min(MAX_FILE_BYTES) as usize;
    let start = current.len().saturating_sub(keep);
    let slice = &current[start..];
    let cut = slice
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|offset| offset + 1)
        .unwrap_or(slice.len());
    let mut rebuilt = Vec::with_capacity(slice.len() - cut + line.len());
    rebuilt.extend_from_slice(&slice[cut..]);
    rebuilt.extend_from_slice(line.as_bytes());
    std::fs::write(&path, rebuilt).is_ok()
}

/// UTC timestamp with millisecond precision, e.g. 2026-10-31T12:00:00.123Z.
pub fn utc_timestamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn sanitize_component(component: &str) -> String {
    let mut name = String::with_capacity(component.len().min(MAX_COMPONENT_CHARS));
    for ch in component.chars() {
        if name.len() >= MAX_COMPONENT_CHARS {
            break;
        }
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            name.push(ch.to_ascii_lowercase());
        } else {
            name.push('_');
        }
    }
    if name.is_empty() {
        "app".to_owned()
    } else {
        name
    }
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push_str("...");
    out
}

const SENSITIVE_KEYS: [&str; 22] = [
    "token",
    "access_token",
    "refresh_token",
    "api_key",
    "apikey",
    "secret",
    "password",
    "passwd",
    "cookie",
    "authorization",
    "auth",
    "session",
    "sessdata",
    "bili_jct",
    "csrf",
    "csrf_token",
    "jwt",
    "sig",
    "sign",
    "player_token",
    "playertoken",
    "access_key",
];

/// Best-effort credential/URL redaction for diagnostic lines.
///
/// * http(s)/ws(s) URLs keep scheme + host + path but lose userinfo and query;
/// * key=value / key: value pairs for sensitive keys get value = ***;
/// * Bearer tokens and Cookie header values become ***.
///
/// Room ids, user names and 32-hex session ids are deliberately preserved so
/// operators can correlate events.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some((position, kind)) = next_sensitive_start(rest) else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..position]);
        let tail = &rest[position..];
        match kind {
            SensitiveStart::Url => {
                let end = tail
                    .find(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '<' | '>'))
                    .unwrap_or(tail.len());
                out.push_str(&redact_url(&tail[..end]));
                rest = &tail[end..];
            }
            SensitiveStart::Bearer => {
                out.push_str("Bearer ***");
                let value = &tail["Bearer ".len()..];
                let end = value.find(char::is_whitespace).unwrap_or(value.len());
                rest = &value[end..];
            }
        }
    }
    redact_key_values(&out)
}

#[derive(Clone, Copy)]
enum SensitiveStart {
    Url,
    Bearer,
}

fn next_sensitive_start(text: &str) -> Option<(usize, SensitiveStart)> {
    let mut best: Option<(usize, SensitiveStart)> = None;
    for scheme in ["https://", "http://", "wss://", "ws://"] {
        if let Some(position) = text.find(scheme) {
            if best.map(|(index, _)| position < index).unwrap_or(true) {
                best = Some((position, SensitiveStart::Url));
            }
        }
    }
    if let Some(position) = text.find("Bearer ") {
        if best.map(|(index, _)| position < index).unwrap_or(true) {
            best = Some((position, SensitiveStart::Bearer));
        }
    }
    best
}

fn redact_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_owned();
    };
    let scheme = &url[..scheme_end + 3];
    let rest = &url[scheme_end + 3..];
    let authority_end = rest
        .find(|ch: char| matches!(ch, '/' | '?' | '#'))
        .unwrap_or(rest.len());
    let mut authority = &rest[..authority_end];
    if let Some(at) = authority.rfind('@') {
        authority = &authority[at + 1..];
    }
    let path_and_query = &rest[authority_end..];
    let path_end = path_and_query.find('?').unwrap_or(path_and_query.len());
    let mut out = String::with_capacity(url.len());
    out.push_str(scheme);
    out.push_str(authority);
    out.push_str(&path_and_query[..path_end]);
    if path_and_query.len() > path_end {
        out.push_str("?<redacted>");
    }
    out
}

fn is_key_delimiter(ch: char) -> bool {
    matches!(
        ch,
        ' ' | '\t' | '\n' | '\r' | '&' | ';' | ',' | '"' | '\'' | '(' | ')' | '{' | '}' | '[' | ']'
            | '<' | '>' | '/' | '?' | '#' | '|'
    )
}

fn is_value_delimiter(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '\r' | '&' | ';' | ',' | ')' | '"' | '\'' | '}')
}

fn redact_key_values(text: &str) -> String {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        let (start_byte, current) = chars[index];
        let previous_is_delimiter = index == 0 || is_key_delimiter(chars[index - 1].1);
        if current.is_ascii_alphanumeric() && previous_is_delimiter {
            let mut key_end = index;
            while key_end < chars.len()
                && (chars[key_end].1.is_ascii_alphanumeric()
                    || matches!(chars[key_end].1, '_' | '-'))
            {
                key_end += 1;
            }
            let key_end_byte = chars.get(key_end).map(|(offset, _)| *offset).unwrap_or(text.len());
            let key = text[start_byte..key_end_byte].to_ascii_lowercase();
            let mut separator = key_end;
            while separator < chars.len() && chars[separator].1 == ' ' {
                separator += 1;
            }
            if separator < chars.len() && matches!(chars[separator].1, '=' | ':') {
                let is_header = matches!(
                    key.as_str(),
                    "cookie" | "set-cookie" | "authorization" | "proxy-authorization"
                );
                if SENSITIVE_KEYS.contains(&key.as_str()) || is_header {
                    out.push_str(&text[start_byte..key_end_byte]);
                    out.push(chars[separator].1);
                    out.push_str(if chars[separator].1 == '=' { "***" } else { " ***" });
                    let mut value = separator + 1;
                    if is_header {
                        while value < chars.len() && chars[value].1 != '\n' {
                            value += 1;
                        }
                    } else {
                        while value < chars.len() && chars[value].1 == ' ' {
                            value += 1;
                        }
                        if value < chars.len() && matches!(chars[value].1, '"' | '\'') {
                            let quote = chars[value].1;
                            value += 1;
                            while value < chars.len() && chars[value].1 != quote {
                                value += 1;
                            }
                            if value < chars.len() {
                                value += 1;
                            }
                        } else {
                            while value < chars.len() && !is_value_delimiter(chars[value].1) {
                                value += 1;
                            }
                        }
                    }
                    index = value;
                    continue;
                }
            }
        }
        out.push(current);
        index += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_bearer_cookie_and_sensitive_pairs() {
        assert_eq!(redact("Cookie: SESSDATA=abc; bili_jct=def"), "Cookie: ***");
        assert_eq!(redact("Authorization: Bearer abc.def"), "Authorization: ***");
        assert_eq!(redact("token=abc123&room=7777"), "token=***&room=7777");
        assert_eq!(redact("password: \"hunter2\""), "password: ***");
        assert_eq!(redact("普通消息 room=7777"), "普通消息 room=7777");
    }

    #[test]
    fn redacts_url_query_but_keeps_host_and_path() {
        assert_eq!(
            redact("open https://api.example.com/live?token=secret&room=1 now"),
            "open https://api.example.com/live?<redacted> now"
        );
        assert_eq!(
            redact("ws://user:pw@127.0.0.1:7777/ws?key=1"),
            "ws://127.0.0.1:7777/ws?<redacted>"
        );
    }

    #[test]
    fn component_names_are_sanitised() {
        assert_eq!(sanitize_component("relay"), "relay");
        assert_eq!(sanitize_component("Overlay/../x"), "overlay____x");
        assert_eq!(sanitize_component(""), "app");
        assert!(sanitize_component(&"a".repeat(80)).len() <= MAX_COMPONENT_CHARS);
    }

    #[test]
    fn bounded_append_keeps_newest_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = log_path(dir.path(), "relay");
        for index in 0..4000 {
            assert!(append(dir.path(), "relay", Level::Info, &format!("line-{index}-{}", "x".repeat(40))));
        }
        let metadata = std::fs::metadata(&path).unwrap();
        assert!(metadata.len() <= MAX_FILE_BYTES, "size {}", metadata.len());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("line-3999-"), "newest line must survive");
        assert!(!text.contains("line-0-"), "oldest line must be dropped");
    }

    #[test]
    fn failed_writes_are_silent_and_return_false() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("blocked");
        std::fs::write(&file, b"x").unwrap();
        assert!(!append(&file, "relay", Level::Info, "hello"));
    }

    #[test]
    fn timestamps_are_utc_iso8601() {
        let timestamp = utc_timestamp();
        assert!(timestamp.ends_with('Z'));
        assert_eq!(timestamp.len(), 24, "{timestamp}");
        assert!(timestamp.contains('T'));
    }
}
