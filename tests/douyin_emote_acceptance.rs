//! Live network acceptance for the Douyin emote dictionary and remote image
//! path (ignored by default).
//!
//! The default suite never touches the network: this test is `#[ignore]`-ed and
//! additionally gated by the environment variable below, mirroring
//! `tests/douyin_live_acceptance.rs`. It contacts only the public,
//! credential-free Douyin `emoji/list` endpoint and the CDN URL it returns
//! (no cookies, no login, no room id — the endpoint is platform-wide):
//!
//! ```powershell
//! $env:LIVE_HUB_LIVE_ACCEPTANCE='1'
//! $cmd = "& 'scripts\Invoke-Cargo.ps1' -ToolchainRoot '<toolchain root>' " +
//!        "-TargetDirectory '<ascii target>' -- test --locked --offline " +
//!        "--test douyin_emote_acceptance -- --ignored --nocapture"
//! & pwsh -NoProfile -Command $cmd
//! ```
//!
//! What it proves end to end:
//! 1. `danmaku::fetch_douyin_emoticons_anonymous_result()` returns a real
//!    dictionary (currently ~214 entries) containing `[捂脸]` and `[微笑]`;
//! 2. the `[捂脸]` URL survives the production URL whitelist, is downloaded by
//!    `images::ImageLoader` through the application HTTP client, and its WebP
//!    bytes decode to a 96x96 non-empty RGBA image;
//! 3. `rich::split_text_segments("[捂脸]", ...)` yields exactly one
//!    `TextSegment::Emoticon` (no leftover `Text`), i.e. the chat text would
//!    render as the image instead of the literal bracketed code.

use live_hub_rust::danmaku;
use live_hub_rust::danmaku::rich::{self, TextSegment};
use live_hub_rust::images::{self, ImageData, ImageLoader};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

const LOAD_TIMEOUT: Duration = Duration::from_secs(20);
const EMOTE_SIDE: usize = 96;

fn opted_in() -> bool {
    std::env::var("LIVE_HUB_LIVE_ACCEPTANCE").ok().as_deref() == Some("1")
}

/// Drives the real `ImageLoader` worker: `request` returns `None` while the
/// background download is pending and invokes `notify` on its worker thread
/// once it settles, so wait for that signal and then read the cached image.
fn load_via_production_loader(loader: &ImageLoader, url: &str) -> Arc<ImageData> {
    let (sender, receiver) = mpsc::channel::<()>();
    if let Some(image) = loader.request(url, move || {
        let _ = sender.send(());
    }) {
        return image;
    }
    match receiver.recv_timeout(LOAD_TIMEOUT) {
        Ok(()) => {}
        Err(error) => panic!(
            "ImageLoader worker for {url} never signalled completion within {LOAD_TIMEOUT:?} \
             ({error}); the URL may have been rejected before a worker started"
        ),
    }
    loader.request(url, || {}).unwrap_or_else(|| {
        panic!(
            "ImageLoader finished for {url} but no decoded image is cached \
             (HTTP download or WebP decode failed inside the worker)"
        )
    })
}

#[test]
#[ignore = "live network acceptance: opt in with LIVE_HUB_LIVE_ACCEPTANCE=1 (public Douyin emoji/list, no cookies or login)"]
fn douyin_emote_dictionary_and_remote_image_path() {
    if !opted_in() {
        eprintln!(
            "skipped: set LIVE_HUB_LIVE_ACCEPTANCE=1 to contact the public Douyin emoji/list endpoint (no cookies, no login)"
        );
        return;
    }

    // (1) Real platform-wide emote dictionary through the production fetch.
    let dictionary = danmaku::fetch_douyin_emoticons_anonymous_result()
        .expect("anonymous Douyin emoji/list fetch must return a dictionary");
    eprintln!("dictionary entries: {}", dictionary.len());
    assert!(
        !dictionary.is_empty(),
        "live Douyin emoji/list returned an empty dictionary"
    );
    assert!(
        dictionary.len() >= 100,
        "live Douyin emoji/list should expose >= 100 entries, got {}",
        dictionary.len()
    );
    let frown_url = dictionary
        .get("[捂脸]")
        .expect("live dictionary must contain [捂脸]")
        .to_owned();
    let smile_url = dictionary
        .get("[微笑]")
        .expect("live dictionary must contain [微笑]")
        .to_owned();
    eprintln!("[捂脸] -> {frown_url}");
    eprintln!("[微笑] -> {smile_url}");
    assert!(
        frown_url.starts_with("https://"),
        "emote URL is not HTTPS: {frown_url}"
    );

    // (2) Full production image path: whitelist + HTTP through the app client +
    // WebP decode. `approved_url` is the same gate `ImageLoader::request` applies.
    let approved = images::approved_url(&frown_url)
        .expect("live Douyin emote URL must pass the production URL whitelist");
    eprintln!(
        "approved image URL: host={} address={approved}",
        approved.host_str().unwrap_or("<none>")
    );
    let loader = ImageLoader::new();
    let image = load_via_production_loader(&loader, &frown_url);
    let non_zero = image.rgba.iter().filter(|channel| **channel != 0).count();
    let opaque = image.rgba.chunks_exact(4).filter(|px| px[3] != 0).count();
    eprintln!(
        "ImageLoader: {}x{}, rgba={} bytes, non-zero channels={}, opaque pixels={}",
        image.width,
        image.height,
        image.rgba.len(),
        non_zero,
        opaque
    );
    assert_eq!(
        (image.width, image.height),
        (EMOTE_SIDE, EMOTE_SIDE),
        "real Douyin emote image must decode to 96x96 through the production loader"
    );
    assert_eq!(
        image.rgba.len(),
        EMOTE_SIDE * EMOTE_SIDE * 4,
        "decoded RGBA buffer has an unexpected length"
    );
    assert!(
        !image.rgba.is_empty(),
        "decoded RGBA buffer is empty for {frown_url}"
    );
    assert!(
        image.rgba.iter().any(|channel| *channel != 0),
        "decoded RGBA buffer is all zero for {frown_url}"
    );

    // (3) Danmaku text segmentation uses the dictionary entry, so the chat text
    // renders as the image rather than as the literal "[捂脸]".
    let segments = rich::split_text_segments("[捂脸]", Some(&dictionary), None);
    eprintln!("split_text_segments(\"[捂脸]\") -> {segments:?}");
    match segments.as_slice() {
        [TextSegment::Emoticon { code, url }] => {
            assert_eq!(code, "[捂脸]", "emoticon segment code mismatch");
            assert_eq!(
                url, &frown_url,
                "emoticon segment URL must equal the dictionary URL"
            );
        }
        other => panic!(
            "expected exactly one TextSegment::Emoticon (and no surrounding Text segment), got {other:?}"
        ),
    }
}
