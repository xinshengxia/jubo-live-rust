//! Offline acceptance for the real Douyin VP8L WebP emote payloads.
//!
//! The default suite never reads external files and never touches the network:
//! this test only runs when `LIVE_HUB_EMOTE_FIXTURE_DIR` points at a directory
//! holding real 96x96 VP8L `.webp` samples (for example the task-local probe
//! download of the public Douyin emoji dictionary images). It exercises the
//! production remote-image path (`live_hub_rust::images::decode`) and never
//! the local colour-emoji sprite-sheet path (`decode_sheet`), because Douyin
//! chat emotes are loaded as individual remote images.
//!
//! Opt in explicitly (ASCII target directory required by the GNU toolchain):
//!
//! ```powershell
//! $env:LIVE_HUB_EMOTE_FIXTURE_DIR='<dir with real *.webp>'
//! pwsh -NoProfile -File scripts/Invoke-Cargo.ps1 -ToolchainRoot '<toolchain root>' `
//!   -TargetDirectory '<ascii target>' test --locked --offline `
//!   --test douyin_webp_decode_acceptance -- --nocapture
//! ```
//!
//! When the variable is unset or empty the test prints a skip line and passes,
//! so the default offline suite stays hermetic.

use live_hub_rust::images;
use std::path::{Path, PathBuf};

const EMOTE_SIDE: usize = 96;

fn fixture_dir() -> Option<PathBuf> {
    let value = std::env::var_os("LIVE_HUB_EMOTE_FIXTURE_DIR")?;
    let text = value.to_string_lossy().trim().to_owned();
    (!text.is_empty()).then(|| PathBuf::from(text))
}

fn webp_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("cannot read fixture directory {}: {error}", dir.display()))
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("webp"))
        })
        .collect();
    files.sort();
    files
}

fn is_vp8l_webp(bytes: &[u8]) -> bool {
    bytes.len() >= 20
        && bytes.get(0..4) == Some(b"RIFF")
        && bytes.get(8..12) == Some(b"WEBP")
        && bytes.get(12..16) == Some(b"VP8L")
        && bytes.get(20) == Some(&0x2f)
}

#[test]
fn real_douyin_vp8l_webp_decodes_through_the_remote_image_path() {
    let Some(dir) = fixture_dir() else {
        eprintln!(
            "skipped: set LIVE_HUB_EMOTE_FIXTURE_DIR to a directory of real Douyin 96x96 VP8L *.webp samples"
        );
        return;
    };
    let files = webp_files(&dir);
    assert!(
        !files.is_empty(),
        "LIVE_HUB_EMOTE_FIXTURE_DIR={} contains no *.webp fixtures",
        dir.display()
    );

    let expected_bytes = EMOTE_SIDE * EMOTE_SIDE * 4;
    for file in &files {
        let bytes = std::fs::read(file)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", file.display()));
        assert!(
            is_vp8l_webp(&bytes),
            "{} is not a VP8L WebP payload; this acceptance test must run against real Douyin emote images",
            file.display()
        );
        let image = images::decode(&bytes).unwrap_or_else(|error| {
            panic!(
                "production images::decode failed for real Douyin WebP {} ({} bytes): {error:#}",
                file.display(),
                bytes.len()
            )
        });
        assert_eq!(
            (image.width, image.height),
            (EMOTE_SIDE, EMOTE_SIDE),
            "{} decoded with unexpected dimensions {}x{}",
            file.display(),
            image.width,
            image.height
        );
        assert_eq!(
            image.rgba.len(),
            expected_bytes,
            "{} decoded RGBA buffer has {} bytes, expected {expected_bytes}",
            file.display(),
            image.rgba.len()
        );
        assert!(
            image.rgba.iter().any(|channel| *channel != 0),
            "{} decoded to an all-zero RGBA buffer",
            file.display()
        );
        let non_zero = image.rgba.iter().filter(|channel| **channel != 0).count();
        let opaque = image.rgba.chunks_exact(4).filter(|px| px[3] != 0).count();
        eprintln!(
            "ok: {} ({} bytes) -> {}x{}, rgba={} bytes, non-zero channels={}, opaque pixels={}",
            file.display(),
            bytes.len(),
            image.width,
            image.height,
            image.rgba.len(),
            non_zero,
            opaque
        );
    }
    eprintln!(
        "verified {} real Douyin VP8L WebP sample(s) through live_hub_rust::images::decode",
        files.len()
    );

    assert!(
        images::decode(b"<html>not an image</html>").is_err(),
        "production images::decode accepted non-image bytes"
    );
}
