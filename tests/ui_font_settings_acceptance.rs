#![cfg(feature = "gui")]
//! Acceptance for the R2 launcher font settings (legacy UiFontKey / UiFontPercent).
//!
//! Covers the persisted contract (defaults, store round trip, old-file tolerance,
//! out-of-range rejection without rewriting) and the pure mapping into the theme
//! install options. The global font-scale application is asserted in one test only
//! because it mutates process-wide state; it is restored before returning.

use live_hub_rust::core::{Settings, Store};
use live_hub_rust::gui::theme::{
    self, FontOptions, FontPreference, UI_FONT_KEYS, UI_FONT_PERCENT_STEPS, normalize_ui_font_key,
    normalize_ui_font_percent, ui_font_key_index,
};
use std::fs;
use tempfile::tempdir;

#[test]
fn defaults_match_the_original_ui_font_settings() {
    let settings = Settings::default();
    assert_eq!(settings.ui_font_key, "auto");
    assert_eq!(settings.ui_font_percent, 100);
    let options = FontOptions::default();
    assert_eq!(options.preference, FontPreference::SystemUi);
    assert_eq!(options.scale_percent, 100.0);
}

#[test]
fn persisted_font_settings_round_trip_through_the_store() {
    let directory = tempdir().unwrap();
    let mut store = Store::open(directory.path().to_owned()).unwrap();
    let settings = Settings {
        ui_font_key: "harmony".to_owned(),
        ui_font_percent: 115,
        ..Default::default()
    };
    store.save_settings(settings).unwrap();
    let reopened = Store::open(directory.path().to_owned()).unwrap();
    assert_eq!(reopened.settings().ui_font_key, "harmony");
    assert_eq!(reopened.settings().ui_font_percent, 115);
    let document: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.path().join("state.json")).unwrap()).unwrap();
    assert_eq!(document["settings"]["ui_font_key"], serde_json::json!("harmony"));
    assert_eq!(document["settings"]["ui_font_percent"], serde_json::json!(115));
}

#[test]
fn old_state_without_ui_font_fields_loads_documented_defaults() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("state.json");
    let legacy = br#"{"schema_version":1,"settings":{"mpv":"players/mpv.exe"},"favorites":[],"history":[]}"#;
    fs::write(&path, legacy).unwrap();
    let store = Store::open(directory.path().to_owned()).unwrap();
    assert_eq!(store.settings().ui_font_key, "auto");
    assert_eq!(store.settings().ui_font_percent, 100);
    assert_eq!(fs::read(&path).unwrap(), legacy, "opening must not rewrite old state");
}

#[test]
fn invalid_persisted_font_settings_are_rejected_without_rewriting() {
    for invalid in [
        r#"{"schema_version":1,"settings":{"ui_font_percent":200},"favorites":[],"history":[]}"#,
        r#"{"schema_version":1,"settings":{"ui_font_key":"comic-sans"},"favorites":[],"history":[]}"#,
    ] {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.json");
        fs::write(&path, invalid).unwrap();
        assert!(Store::open(directory.path().to_owned()).is_err(), "must reject {invalid}");
        assert_eq!(fs::read(&path).unwrap(), invalid.as_bytes(), "invalid file stays untouched");
    }
}

#[test]
fn font_options_map_persisted_keys_to_install_options() {
    let system = FontOptions::from_settings("yahei-ui", 130);
    assert_eq!(system.preference, FontPreference::SystemUi);
    assert_eq!(system.scale_percent, 130.0);
    let bundled = FontOptions::from_settings("harmony", 85);
    assert_eq!(bundled.preference, FontPreference::BundledSans);
    assert_eq!(bundled.scale_percent, 85.0);
    // Unknown keys fall back to the system preference, out-of-range percents clamp.
    let fallback = FontOptions::from_settings("bogus", 200);
    assert_eq!(fallback.preference, FontPreference::SystemUi);
    assert_eq!(fallback.scale_percent, 130.0);
    assert_eq!(normalize_ui_font_key("  YaHei-UI  "), "system");
    assert_eq!(normalize_ui_font_key("harmonyos"), "harmony");
    assert_eq!(normalize_ui_font_key("unknown"), "auto");
    assert_eq!(normalize_ui_font_percent(80), 85);
    assert_eq!(normalize_ui_font_percent(2000), 130);

    // The settings page dropdown lists exactly the three supported keys and the
    // original four font-size notches.
    assert_eq!(UI_FONT_KEYS.len(), 3);
    assert_eq!(UI_FONT_KEYS[0].0, "auto");
    assert_eq!(ui_font_key_index("harmony"), 2);
    assert_eq!(ui_font_key_index("yahei-ui"), 1);
    assert_eq!(ui_font_key_index("nonsense"), 0);
    assert_eq!(FontPreference::SystemUi.settings_key(), "system");
    assert_eq!(FontPreference::BundledSans.settings_key(), "harmony");
}

#[test]
fn font_scale_percent_applies_to_theme_metrics() {
    assert_eq!(UI_FONT_PERCENT_STEPS, [85, 100, 115, 130]);
    // set_ui_font_scale takes a percentage while ui_font_scale returns the factor.
    let previous_percent = theme::ui_font_scale() * 100.0;
    theme::set_ui_font_scale(115.0);
    assert!((theme::ui_font_scale() - 1.15).abs() < 0.001);
    assert!((theme::scale_font_size(12.0, 115.0) - 13.8).abs() < 0.01);
    assert!((theme::scaled_font_size(12.0) - 13.8).abs() < 0.01);
    theme::set_ui_font_scale(85.0);
    assert!((theme::scaled_font_size(12.0) - 10.2).abs() < 0.01);
    // Out-of-range requests clamp to the documented 85..=130 window.
    theme::set_ui_font_scale(500.0);
    assert!((theme::ui_font_scale() - 1.3).abs() < 0.001);
    theme::set_ui_font_scale(previous_percent);
    assert!(
        (theme::ui_font_scale() * 100.0 - previous_percent).abs() < 0.001,
        "global font scale restored"
    );
}
