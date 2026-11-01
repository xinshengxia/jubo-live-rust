//! Independent end-to-end acceptance for favorites export: store -> JSON file ->
//! second store -> export again, plus legacy-map conversion. The contract tests in
//! tests/favorites_transfer_contract.rs check the module in isolation; this file
//! verifies the actual on-disk round trip an owner would use from the UI button.

use chrono::Local;
use live_hub_rust::core::{Favorite, Platform, Store, Target};
use live_hub_rust::favorites_transfer::{export_favorites_json, export_file_name};
use std::fs;
use tempfile::tempdir;

fn favorite(name: &str, platform: Platform, room: &str) -> Favorite {
    Favorite {
        name: name.to_owned(),
        target: Target::new(platform, room).expect("synthetic target"),
    }
}

fn identities(favorites: &[Favorite]) -> Vec<String> {
    let mut values: Vec<String> = favorites
        .iter()
        .map(|favorite| format!("{}:{}", favorite.target.platform, favorite.target.room))
        .collect();
    values.sort();
    values
}

#[test]
fn exported_file_round_trips_through_two_stores() {
    let source = tempdir().unwrap();
    let mut store = Store::open(source.path().to_owned()).unwrap();
    for (name, platform, room) in [
        ("合成主播甲", Platform::Bili, "7777"),
        ("合成主播乙", Platform::Huya, "synthetic_room"),
        ("合成主播丙", Platform::Douyu, "55001"),
        ("合成主播丁", Platform::Douyin, "6600001"),
    ] {
        store
            .add_favorite(name.to_owned(), Target::new(platform, room).unwrap())
            .unwrap();
    }
    let exported = export_favorites_json(store.favorites()).expect("export succeeds");
    assert!(exported.ends_with("\n"), "exported document ends with a newline");
    assert!(!exported.starts_with("\u{feff}"), "no BOM");
    let parsed: serde_json::Value = serde_json::from_str(&exported).expect("valid JSON");
    let array = parsed.as_array().expect("a pure JSON array");
    assert_eq!(array.len(), 4);
    for entry in array {
        assert!(entry.get("name").is_some(), "each entry has a name");
        assert!(entry.get("target").is_some(), "each entry has a target");
    }
    let file_name = export_file_name(Local::now());
    assert!(file_name.ends_with(".json"), "file name: {file_name}");
    assert!(
        file_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_'),
        "file name stays path safe: {file_name}"
    );
    let path = source.path().join(&file_name);
    fs::write(&path, &exported).unwrap();

    let target = tempdir().unwrap();
    let mut reopened = Store::open(target.path().to_owned()).unwrap();
    reopened
        .import_json(&exported)
        .expect("the exported array re-imports through Store::import_json");
    assert_eq!(identities(reopened.favorites()), identities(store.favorites()));
    let exported_again = export_favorites_json(reopened.favorites()).unwrap();
    assert_eq!(exported_again, exported, "second export is byte stable");
}

#[test]
fn legacy_map_format_is_converted_to_the_export_array() {
    let directory = tempdir().unwrap();
    let mut store = Store::open(directory.path().to_owned()).unwrap();
    let legacy = r#"{"合成主播甲":"bili:7777","合成主播乙":"huya:synthetic_room"}"#;
    store.import_json(legacy).expect("legacy map imports");
    let exported = export_favorites_json(store.favorites()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&exported).unwrap();
    assert_eq!(parsed.as_array().expect("array").len(), 2);
    assert!(exported.contains("7777"));
    assert!(exported.contains("synthetic_room"));
}

#[test]
fn names_with_emoji_and_quotes_survive_the_round_trip() {
    let source = tempdir().unwrap();
    let mut store = Store::open(source.path().to_owned()).unwrap();
    store
        .add_favorite(
            "弹 幕 引号 🎉 表情".to_owned(),
            Target::new(Platform::Bili, "8888").unwrap(),
        )
        .unwrap();
    let exported = export_favorites_json(store.favorites()).unwrap();
    let target = tempdir().unwrap();
    let mut reopened = Store::open(target.path().to_owned()).unwrap();
    reopened.import_json(&exported).unwrap();
    assert_eq!(reopened.favorites().len(), 1);
    assert_eq!(reopened.favorites()[0].name, store.favorites()[0].name);
    assert_eq!(export_favorites_json(reopened.favorites()).unwrap(), exported);
}

#[test]
fn empty_store_exports_a_valid_empty_array_that_imports_back() {
    let directory = tempdir().unwrap();
    let store = Store::open(directory.path().to_owned()).unwrap();
    let exported = export_favorites_json(store.favorites()).unwrap();
    assert_eq!(exported.trim(), "[]");
    // Keep the tempdir alive: a temporary dropped in the same statement would
    // delete the store directory before Store::open can create its lock.
    let target_directory = tempdir().unwrap();
    let mut target = Store::open(target_directory.path().to_owned()).unwrap();
    target.import_json(&exported).unwrap();
    assert!(target.favorites().is_empty());
}
