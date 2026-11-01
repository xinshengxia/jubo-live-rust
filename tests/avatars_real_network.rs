//! Real-network acceptance tests for the anonymous avatar chain.
//!
//! Ignored by default: the offline matrix never opens a network connection.
//! Run explicitly with:
//!
//!   pwsh -NoProfile -File scripts/Invoke-Cargo.ps1 -ToolchainRoot <toolchain> \
//!     -TargetDirectory <target> test --locked --offline --no-default-features \
//!     --test avatars_real_network -- --ignored --nocapture
//!
//! The tests print only public URLs, HTTP-level status/classification and byte
//! sizes; they never read cookies, account data or browser profiles.

use live_hub_rust::avatars::{AvatarService, AvatarTransport, OfficialTransport};
use live_hub_rust::core::{Platform, Target};
use live_hub_rust::platforms::{avatar_url_detailed, normalize_avatar_url};
use std::time::{Duration, Instant};

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

#[test]
#[ignore = "real network: anonymous Bilibili avatar lookup + bounded PNG download"]
fn bili_7777_avatar_resolves_and_downloads_png() {
    let target = Target::parse("bili:7777").expect("bili:7777 parses");
    let url = avatar_url_detailed(&target).expect("anonymous Bilibili avatar URL");
    println!("bili:7777 avatar_url = {url}");
    assert!(url.starts_with("https://"), "not HTTPS: {url}");
    assert!(url.contains("hdslb"), "unexpected avatar CDN host: {url}");
    let checked = normalize_avatar_url(&url).expect("allowlisted avatar URL");
    assert_eq!(checked, url);

    let bytes = AvatarTransport::fetch(&OfficialTransport, &url).expect("bounded avatar download");
    println!("bili:7777 avatar bytes = {}", bytes.len());
    assert!(!bytes.is_empty() && bytes.len() <= 256 * 1024, "unbounded payload");
    assert!(
        bytes.starts_with(PNG_SIGNATURE),
        "expected the 96px PNG thumbnail, got {} bytes starting {:02x?}",
        bytes.len(),
        &bytes[..bytes.len().min(8)]
    );

    // End-to-end through the service: resolve + download + 48px PNG + disk cache.
    let directory = tempfile::tempdir().expect("temp data dir");
    let service = AvatarService::new(directory.path());
    service.request(Platform::Bili, "7777");
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut update = None;
    while Instant::now() < deadline && update.is_none() {
        update = service.try_recv();
        if update.is_none() {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let update = update.expect("avatar update through the bounded service");
    println!(
        "bili:7777 cached url={} png_bytes={} size={}x{}",
        update.entry.url,
        update.entry.png.len(),
        update.entry.width,
        update.entry.height
    );
    assert_eq!((update.entry.width, update.entry.height), (48, 48));
    assert!(
        service.cached(Platform::Bili, "7777").is_some(),
        "disk cache must serve a restart without network"
    );
    assert!(
        directory.path().join("avatars").join("index.json").is_file(),
        "avatar index not persisted"
    );
}

#[test]
#[ignore = "real network: anonymous Huya/Douyu/Douyin avatar availability (informational)"]
fn huya_douyu_douyin_avatar_availability() {
    // Public rooms taken from the project's own documentation/fixtures; a room
    // going offline or a platform risk-control page is reported, not asserted.
    for (platform, room) in [
        (Platform::Huya, "910323"),
        (Platform::Douyu, "93589"),
        (Platform::Douyin, "46595453118"),
    ] {
        let target = Target::parse(&format!("{platform}:{room}")).expect("target parses");
        match avatar_url_detailed(&target) {
            Ok(url) => {
                println!("{platform}:{room} avatar_url = {url}");
                match AvatarTransport::fetch_detailed(&OfficialTransport, &url) {
                    Ok(bytes) => println!(
                        "{platform}:{room} fetch: ok, {} bytes, png={}",
                        bytes.len(),
                        bytes.starts_with(PNG_SIGNATURE)
                    ),
                    Err(failure) => println!("{platform}:{room} fetch: {:?}", failure),
                }
            }
            Err(failure) => {
                println!("{platform}:{room} resolve: {:?} ({})", failure, failure.label())
            }
        }
    }
}
#[test]
#[ignore = "real network: anonymous Huya alias page avatar lookup (informational)"]
fn huya_alias_rooms_resolve_their_avatar() {
    for alias in ["a16789", "clkimmy"] {
        match live_hub_rust::platforms::avatar_url_for_room(Platform::Huya, alias) {
            Ok(url) => println!("huya:{alias} avatar_url = {url}"),
            Err(failure) => println!("huya:{alias} resolve: {:?} ({})", failure, failure.label()),
        }
    }
}
