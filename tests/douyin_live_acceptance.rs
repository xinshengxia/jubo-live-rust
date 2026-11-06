//! Live network acceptance for the Douyin anonymous path (ignored by default).
//!
//! The default suite never touches the network. This test only runs when the
//! operator explicitly opts in with a public room's web_rid (no cookies, no user
//! data, no browser profile):
//!
//! ```powershell
//! $env:LIVE_HUB_LIVE_ACCEPTANCE='1'; $env:LIVE_HUB_DOUYIN_ROOM='<web_rid>'
//! pwsh -NoProfile -File scripts/Invoke-Cargo.ps1 -ToolchainRoot '<root>' `
//!   -TargetDirectory '<ascii target>' test --locked --offline --test douyin_live_acceptance -- --ignored --nocapture
//! ```
//!
//! What it asserts: an anonymous room resolve returns an HTTPS FLV address with a
//! real title, and the anonymous Webcast WebSocket handshake reports "已连接"
//! within the observation window (that is the signature/join-URL acceptance
//! signal). Chat volume is printed but not asserted because it depends on the
//! chosen room and moment.

use live_hub_rust::core::{Settings, Target};
use live_hub_rust::danmaku::{self, DanmakuEvent};
use std::time::{Duration, Instant};

fn opted_in_room() -> Option<String> {
    if std::env::var("LIVE_HUB_LIVE_ACCEPTANCE").ok().as_deref() != Some("1") {
        return None;
    }
    std::env::var("LIVE_HUB_DOUYIN_ROOM")
        .ok()
        .map(|room| room.trim().to_owned())
        .filter(|room| !room.is_empty())
}

#[test]
#[ignore = "live network acceptance: opt in with LIVE_HUB_LIVE_ACCEPTANCE=1 and LIVE_HUB_DOUYIN_ROOM=<web_rid>"]
fn anonymous_douyin_room_resolve_and_danmaku_handshake() {
    let Some(room) = opted_in_room() else {
        eprintln!("skipped: set LIVE_HUB_LIVE_ACCEPTANCE=1 and LIVE_HUB_DOUYIN_ROOM=<web_rid>");
        return;
    };
    let target = Target::parse(&format!("douyin:{room}")).expect("synthetic target parse");
    let resolved = live_hub_rust::platforms::resolve(&target, &Settings::default())
        .expect("anonymous Douyin room resolve must return a media address");
    assert!(resolved.url.starts_with("https://"), "unexpected media scheme: {}", resolved.url);
    assert!(!resolved.title.is_empty(), "resolve should carry the real room title");
    eprintln!("resolved title: {}", resolved.title);

    let (sender, receiver) = danmaku::bounded_channel(512);
    let cancellation = danmaku::Cancellation::new();
    let worker_cancel = cancellation.clone();
    let worker_target = target.clone();
    let worker = std::thread::spawn(move || {
        live_hub_rust::danmaku_douyin::connect(&worker_target, sender, worker_cancel)
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut connected = false;
    let mut messages = 0_usize;
    let mut statuses: Vec<String> = Vec::new();
    while Instant::now() < deadline {
        match receiver.recv_timeout(Duration::from_millis(200)) {
            Ok(DanmakuEvent::Message(_)) => messages += 1,
            Ok(DanmakuEvent::Status(status)) => {
                if status.contains("已连接") {
                    connected = true;
                }
                if !statuses.contains(&status) {
                    statuses.push(status);
                }
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
    cancellation.cancel();
    let result = worker.join().expect("danmaku worker panicked");
    eprintln!("statuses: {statuses:?}; chat messages observed: {messages}");
    assert!(connected, "anonymous Webcast handshake never reported 已连接: {statuses:?}");
    assert!(result.is_ok(), "danmaku worker failed after cancel: {result:?}");
}
