//! Contract tests for the R2 favourites export document.
//!
//! The important guarantee is round-trip compatibility: the exported plain JSON
//! array must be readable by `Store::import_json` without a wrapper object and
//! without losing names, rooms or quality. Everything here is offline and uses a
//! temporary store directory.

use chrono::{Local, TimeZone};
use live_hub_rust::core::{Favorite, Platform, Quality, Store, Target};
use live_hub_rust::favorites_transfer::{
    MAX_EXPORT_FAVORITES, export_favorites_json, export_file_name,
};

fn favorite(name: &str, platform: Platform, room: &str, quality: Quality) -> Favorite {
    Favorite {
        name: name.to_owned(),
        target: Target {
            platform,
            room: room.to_owned(),
            quality,
        },
    }
}

fn sample_favorites() -> Vec<Favorite> {
    vec![
        favorite("B站主播", Platform::Bili, "7777", Quality::Best),
        favorite("虎牙主播", Platform::Huya, "a-b_c", Quality::High),
        favorite("斗鱼主播", Platform::Douyu, "952595", Quality::Low),
        favorite("抖音主播", Platform::Douyin, "723910165684", Quality::Best),
    ]
}

#[test]
fn exported_array_round_trips_through_store_import_json() {
    let favorites = sample_favorites();
    let json = export_favorites_json(&favorites).expect("export must succeed");

    // The importer only special-cases objects with favourites/history keys; a
    // plain array takes the legacy branch, which detects the "target" field.
    assert!(
        json.trim_start().starts_with('['),
        "export must be a plain JSON array: {json}"
    );
    assert!(
        !json.contains("\"favorites\""),
        "no wrapper key is allowed (ImportData denies unknown keys)"
    );

    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(directory.path().to_path_buf()).unwrap();
    assert!(store.favorites().is_empty());
    let summary = store.import_json(&json).expect("import must accept the export");
    assert_eq!(summary.favorites_added, favorites.len());
    assert_eq!(summary.favorites_updated, 0);
    assert_eq!(summary.history_added + summary.history_skipped, 0);

    let imported = store.favorites();
    assert_eq!(imported.len(), favorites.len());
    for (expected, actual) in favorites.iter().zip(imported) {
        assert_eq!(actual.name, expected.name);
        assert_eq!(actual.target, expected.target);
    }

    // A second export of the imported entries is byte-stable.
    let again = export_favorites_json(imported).unwrap();
    assert_eq!(again, json);
}

#[test]
fn empty_favorites_export_as_a_valid_empty_array() {
    let json = export_favorites_json(&[]).unwrap();
    assert_eq!(json, "[]\n");
    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(directory.path().to_path_buf()).unwrap();
    let summary = store.import_json(&json).unwrap();
    assert_eq!(summary.favorites_added, 0);
    assert_eq!(summary.favorites_updated, 0);
    assert!(store.favorites().is_empty());
}

#[test]
fn names_and_quality_are_json_escaped_and_restored_exactly() {
    let favorites = vec![favorite(
        "引号\"反斜杠\\ 表情🎉 空格",
        Platform::Bili,
        "7777",
        Quality::High,
    )];
    let json = export_favorites_json(&favorites).unwrap();
    // Escaping is proven by parsing the document back rather than by matching a
    // fragile literal: the parsed name must be byte-identical to the source.
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value[0]["name"].as_str().unwrap(), favorites[0].name);
    assert!(json.len() > favorites[0].name.len(), "escaping must lengthen the text: {json}");

    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::open(directory.path().to_path_buf()).unwrap();
    store.import_json(&json).unwrap();
    assert_eq!(store.favorites()[0].name, favorites[0].name);
    assert_eq!(store.favorites()[0].target.quality, Quality::High);
}

#[test]
fn names_the_importer_would_reject_are_rejected_before_writing() {
    // Whitespace-only and control-character names are exactly what
    // core::normalized_name rejects during import; export must fail early so a
    // user never receives a file that cannot be read back.
    let whitespace = vec![favorite("   ", Platform::Bili, "1", Quality::Best)];
    let error = format!("{:#}", export_favorites_json(&whitespace).unwrap_err());
    assert!(error.contains("名称"), "{error}");

    let control = vec![favorite("换行\n制表\t", Platform::Bili, "1", Quality::Best)];
    let error = format!("{:#}", export_favorites_json(&control).unwrap_err());
    assert!(error.contains("控制字符"), "{error}");
}

#[test]
fn oversized_favorite_sets_are_rejected_before_serialization() {
    let favorites: Vec<Favorite> = (0..=MAX_EXPORT_FAVORITES)
        .map(|index| {
            favorite(
                &format!("主播{index}"),
                Platform::Bili,
                &format!("{}", 10_000 + index),
                Quality::Best,
            )
        })
        .collect();
    assert_eq!(favorites.len(), MAX_EXPORT_FAVORITES + 1);
    let error = export_favorites_json(&favorites).unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("上限"), "{text}");
    assert!(text.contains(&MAX_EXPORT_FAVORITES.to_string()), "{text}");
}

#[test]
fn export_file_name_is_stable_and_path_safe() {
    let now = Local.with_ymd_and_hms(2026, 10, 31, 10, 10, 0).unwrap();
    let name = export_file_name(now);
    assert_eq!(name, "live-hub-favorites-20261031-1010.json");
    assert!(
        name.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_')),
        "unexpected character in {name}"
    );
    assert!(name.ends_with(".json"));
    // A different minute produces a different file name.
    let later = Local.with_ymd_and_hms(2026, 10, 31, 10, 11, 0).unwrap();
    assert_ne!(name, export_file_name(later));
}

#[test]
fn exported_document_is_utf8_and_has_no_bom_or_sidecar_keys() {
    let json = export_favorites_json(&sample_favorites()).unwrap();
    assert!(!json.starts_with('\u{feff}'), "no UTF-8 BOM");
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let array = value.as_array().expect("top-level array");
    assert_eq!(array.len(), 4);
    for entry in array {
        let object = entry.as_object().expect("favourite object");
        let mut keys: Vec<&String> = object.keys().collect();
        keys.sort();
        assert_eq!(keys, vec!["name", "target"], "only domain keys: {keys:?}");
        let target = object["target"].as_object().unwrap();
        let mut target_keys: Vec<&String> = target.keys().collect();
        target_keys.sort();
        assert_eq!(target_keys, vec!["platform", "quality", "room"]);
    }
}
