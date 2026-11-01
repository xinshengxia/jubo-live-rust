//! Offline contract tests for the bounded/redacted diag logger.

use live_hub_rust::diag::{self, Level};

#[test]
fn append_writes_readable_lines_with_utc_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    assert!(diag::append(dir.path(), "overlay", Level::Warn, "hidden (NotForeground)"));
    let text = std::fs::read_to_string(diag::log_path(dir.path(), "overlay")).unwrap();
    assert!(text.contains("[WARN] hidden (NotForeground)"), "{text}");
    assert!(text.trim_end().len() > 24);
    assert!(text.starts_with("20"), "UTC ISO8601 must start with the year: {text}");
}

#[test]
fn component_files_are_separate_and_sanitised() {
    let dir = tempfile::tempdir().unwrap();
    assert!(diag::append(dir.path(), "relay", Level::Info, "ready"));
    assert!(diag::append(dir.path(), "Overlay/../x", Level::Info, "hi"));
    assert!(diag::log_path(dir.path(), "relay").exists());
    assert!(diag::log_path(dir.path(), "Overlay/../x").exists());
    let name = diag::log_path(dir.path(), "Overlay/../x");
    assert!(name.file_name().unwrap().to_string_lossy().starts_with("overlay_"));
}

#[test]
fn file_is_capped_and_newest_lines_survive() {
    let dir = tempfile::tempdir().unwrap();
    for index in 0..3000 {
        assert!(diag::append(
            dir.path(),
            "relay",
            Level::Info,
            &format!("line-{index}-{}", "x".repeat(60))
        ));
    }
    let path = diag::log_path(dir.path(), "relay");
    let size = std::fs::metadata(&path).unwrap().len();
    assert!(size <= diag::MAX_FILE_BYTES, "{size}");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("line-2999-"));
    assert!(!text.contains("line-0-"));
}

#[test]
fn redaction_covers_credentials_and_url_queries() {
    assert_eq!(diag::redact("Cookie: SESSDATA=abc; bili_jct=def"), "Cookie: ***");
    assert_eq!(
        diag::redact("Authorization: Bearer abc.def"),
        "Authorization: ***"
    );
    assert_eq!(diag::redact("token=abc123&room=7777"), "token=***&room=7777");
    assert_eq!(
        diag::redact("open https://api.example.com/x?token=1&room=2"),
        "open https://api.example.com/x?<redacted>"
    );
    assert_eq!(
        diag::redact("ws://user:pw@127.0.0.1:7777/ws"),
        "ws://127.0.0.1:7777/ws"
    );
    // Room ids and ordinary text are preserved for correlation.
    assert_eq!(diag::redact("room 7777 ready"), "room 7777 ready");
}

#[test]
fn failed_writes_are_silent() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("blocked");
    std::fs::write(&blocker, b"x").unwrap();
    assert!(!diag::append(&blocker, "relay", Level::Error, "boom"));
    // Global API stays a no-op instead of panicking when init() was never called.
    diag::log("relay", Level::Error, "undo");
}
