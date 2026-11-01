//! Explicit favourites export for the workbench (R2).
//!
//! The exported document is a **plain JSON array** of `Favorite` entries. It is
//! deliberately not wrapped in an object: `Store::import_json` already accepts
//! this exact shape (the legacy-array branch detects entries with a `target`
//! field and parses them as domain favourites), and `ImportData` denies unknown
//! top-level keys, so adding a `schema`/version key here would make the file
//! unreadable. Any human-readable provenance must live in a separate sidecar
//! file (for example `<export>.license.txt`), never inside this JSON.
//!
//! Nothing here reads or writes the store; the UI owns file paths and errors.

use crate::core::Favorite;
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Local};

/// Mirrors the importer's bound (`core::FAVORITES_LIMIT`). Keeping the export
/// bounded means a corrupted state cannot produce a document that the importer
/// would have to reject after the user picked a save location.
pub const MAX_EXPORT_FAVORITES: usize = 10_000;

/// Serialize favourites as the importer-compatible plain array.
///
/// The output is pretty-printed with a trailing newline so manual inspection is
/// pleasant; the importer ignores whitespace. Names and rooms are escaped by
/// serde_json, so quotes, backslashes, tabs and newlines round-trip unchanged.
pub fn export_favorites_json(favorites: &[Favorite]) -> Result<String> {
    ensure!(
        favorites.len() <= MAX_EXPORT_FAVORITES,
        "收藏数量超过导出上限（{} 条，最多 {} 条）",
        favorites.len(),
        MAX_EXPORT_FAVORITES
    );
    for favorite in favorites {
        favorite
            .target
            .validate()
            .with_context(|| format!("收藏「{}」的房间无效，无法导出", favorite.name))?;
        let name = favorite.name.trim();
        ensure!(!name.is_empty(), "存在没有名称的收藏，无法导出");
        ensure!(
            name.chars().count() <= 256,
            "收藏「{}」的名称过长，无法导出",
            name
        );
        // Mirrors the importer's name rule so an exported file is always
        // readable again: control characters (newline/tab/NUL) are rejected.
        ensure!(
            !name.chars().any(char::is_control),
            "收藏「{}」的名称包含控制字符，无法导出",
            name
        );
    }
    let mut text = serde_json::to_string_pretty(favorites).context("无法序列化收藏")?;
    text.push('\n');
    Ok(text)
}

/// Stable, filesystem-safe default file name for the native save dialog.
///
/// `live-hub-favorites-YYYYMMDD-HHMM.json` contains only ASCII letters, digits,
/// '-' and '.', so it is usable on every supported Windows path without
/// depending on the system locale.
pub fn export_file_name(now: DateTime<Local>) -> String {
    format!("live-hub-favorites-{}.json", now.format("%Y%m%d-%H%M"))
}
