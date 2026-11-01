//! Bounded, redacted relay diagnostics contract (R2 task-16).
//!
//! The relay uses dm-reference's diag.rs (64 KiB per-component cap, UTC
//! timestamps, credential/URL redaction). These tests drive real relay events
//! against loopback fixtures and inspect the resulting relay.log; they never
//! touch a real platform, player or account.

use live_hub_rust::{
    core::ResolvedStream,
    diag,
    flv::{self, FlvTag},
    relay::{self, RelayConfig, RelayServer},
};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

const HEADER: &[u8] = b"FLV\x01\x05\0\0\0\x09\0\0\0\0";
const TOKEN: &str = "synthetic-diagnostic-token";

fn config() -> RelayConfig {
    RelayConfig {
        token: TOKEN.into(),
        allow_loopback_fixture: true,
        idle_timeout: Duration::from_millis(300),
        upstream_timeout: Duration::from_millis(500),
        request_timeout: Duration::from_secs(2),
        write_timeout: Duration::from_secs(3),
        retry_delay: Duration::ZERO,
        ..Default::default()
    }
}

fn connect(address: SocketAddr) -> TcpStream {
    let mut player = TcpStream::connect(address).unwrap();
    player
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    write!(
        player,
        "GET /live.flv?t={TOKEN} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut header = Vec::new();
    let mut byte = [0];
    while !header.ends_with(b"\r\n\r\n") {
        player.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
    }
    assert!(header.starts_with(b"HTTP/1.1 200 OK\r\n"));
    player
}

fn upstream_request(stream: &mut TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut header = Vec::new();
    let mut byte = [0];
    while !header.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
    }
}

fn productive_segment(index: u32) -> Vec<u8> {
    let mut body = HEADER.to_vec();
    body.extend_from_slice(FlvTag::new(9, 0, &[0x17, 0, 0, 0, 0]).unwrap().as_bytes());
    let mut payload = vec![0x5a; 512];
    payload[0] = 0x27;
    payload[1] = 1;
    for tag in 0..relay::STABLE_MEDIA_TAGS as u32 {
        body.extend_from_slice(
            FlvTag::new(9, index * 2_000 + tag * 50, &payload)
                .unwrap()
                .as_bytes(),
        );
    }
    body
}

fn log_text(dir: &Path) -> String {
    let path = diag::log_path(&dir.join("logs"), "relay");
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("relay log at {path:?}: {error}"))
}

#[test]
fn relay_lifecycle_events_are_logged_redacted_and_bounded() {
    let directory = tempfile::tempdir().unwrap();
    let source = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/synthetic.flv", source.local_addr().unwrap());
    let upstream = thread::spawn(move || {
        let (mut stream, _) = source.accept().unwrap();
        upstream_request(&mut stream);
        let body = productive_segment(1);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(&body).unwrap();
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let resolver = Arc::new(move || -> anyhow::Result<ResolvedStream> {
        let index = observed.fetch_add(1, Ordering::SeqCst);
        if index < 2 {
            return Err(relay::ResolveWaiting {
                state: "offline".into(),
                retry_after: Duration::from_millis(200),
            }
            .into());
        }
        Ok(ResolvedStream {
            url: url.clone(),
            headers: BTreeMap::new(),
            title: "Synthetic".into(),
        })
    });
    let mut cfg = config();
    cfg.max_reconnects = 1;
    cfg.log_dir = Some(directory.path().join("logs"));
    let server = RelayServer::bind(cfg, resolver).unwrap();
    let address = server.local_addr().unwrap();
    let relay = thread::spawn(move || server.run());
    let mut player = connect(address);
    flv::read_header(&mut player).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut media = 0;
    while media < relay::STABLE_MEDIA_TAGS && Instant::now() < deadline {
        let Some(tag) = flv::read_tag(&mut player).unwrap() else {
            break;
        };
        if matches!(tag.kind(), 8 | 9) && !tag.is_codec_header() {
            media += 1;
        }
    }
    player.shutdown(Shutdown::Both).unwrap();
    drop(player);
    let _ = relay.join().unwrap();
    upstream.join().unwrap();

    let text = log_text(directory.path());
    assert!(text.contains("connection #"), "{text}");
    assert!(text.contains("opened"), "{text}");
    assert!(
        text.contains("room offline; waiting"),
        "offline wait must be visible: {text}"
    );
    for line in text.lines() {
        assert!(
            line.starts_with("20") && line.contains('Z') && line.contains('T'),
            "missing UTC timestamp: {line}"
        );
        assert!(
            line.contains("[INFO]") || line.contains("[WARN]") || line.contains("[ERROR]"),
            "missing level: {line}"
        );
    }
    // Redaction: the session token and any URL must never reach the log.
    assert!(!text.contains(TOKEN), "session token leaked: {text}");
    assert!(
        !text.to_ascii_lowercase().contains("http://")
            && !text.to_ascii_lowercase().contains("https://"),
        "URL leaked: {text}"
    );
    let path = diag::log_path(&directory.path().join("logs"), "relay");
    assert!(
        std::fs::metadata(&path).unwrap().len() <= diag::MAX_FILE_BYTES,
        "relay log exceeded the cap"
    );
}

#[test]
fn relay_resolve_failures_and_budget_exhaustion_use_warn_and_error_levels() {
    let directory = tempfile::tempdir().unwrap();
    let resolver = Arc::new(|| -> anyhow::Result<ResolvedStream> {
        anyhow::bail!("synthetic resolver failure")
    });
    let mut cfg = config();
    cfg.max_reconnects = 1;
    cfg.log_dir = Some(directory.path().join("logs"));
    let server = RelayServer::bind(cfg, resolver).unwrap();
    let address = server.local_addr().unwrap();
    let relay = thread::spawn(move || server.run());
    let mut player = connect(address);
    let mut body = Vec::new();
    player.read_to_end(&mut body).unwrap();
    let _ = relay.join().unwrap();

    let text = log_text(directory.path());
    assert!(text.contains("[WARN]"), "{text}");
    assert!(
        text.contains("upstream segment failed: resolve_failed"),
        "{text}"
    );
    assert!(text.contains("[ERROR]"), "{text}");
    assert!(text.contains("reconnect budget exhausted"), "{text}");
    let path = diag::log_path(&directory.path().join("logs"), "relay");
    assert!(std::fs::metadata(&path).unwrap().len() <= diag::MAX_FILE_BYTES);
}

#[test]
fn relay_component_log_is_capped_and_keeps_the_newest_lines() {
    let directory = tempfile::tempdir().unwrap();
    for index in 0..4000 {
        let _ = diag::append(
            directory.path(),
            "relay",
            diag::Level::Info,
            &format!("relay-event-{index}-{}", "x".repeat(64)),
        );
    }
    let path = diag::log_path(directory.path(), "relay");
    let size = std::fs::metadata(&path).unwrap().len();
    assert!(size <= diag::MAX_FILE_BYTES, "size {size}");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("relay-event-3999-"), "newest evidence lost");
    assert!(!text.contains("relay-event-0-"), "oldest line not dropped");
}
