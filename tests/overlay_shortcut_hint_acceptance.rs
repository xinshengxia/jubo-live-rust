//! Acceptance for the Rust-only "show shortcut hint" switch (user request
//! 2026-10-31: keep the lower-left F7/F8/F9/F10 hint but let users hide it).
//!
//! The switch must: default to visible, survive old saved preferences that never
//! knew the field, round-trip through the store, and keep status/feedback
//! rendering independent. These assertions are offline and headless.

use live_hub_rust::core::{OverlaySettings, Settings, Store};
use std::fs;
use tempfile::tempdir;

#[test]
fn shortcut_hint_defaults_to_visible() {
    assert!(
        OverlaySettings::default().show_shortcut_hint,
        "the shortcut hint must default to visible for existing and new users"
    );
}

#[test]
fn old_saved_preferences_without_the_field_keep_the_hint_visible() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("state.json");
    // A state file written before the switch existed: same overlay block shape,
    // no show_shortcut_hint key at all.
    let legacy = br#"{"schema_version":1,"settings":{"overlay":{"enabled":true,"font_size":33,"opacity_percent":80}},"favorites":[],"history":[]}"#;
    fs::write(&path, legacy).unwrap();
    let store = Store::open(directory.path().to_owned()).unwrap();
    assert!(
        store.settings().overlay.show_shortcut_hint,
        "an old state file must load the documented default (visible)"
    );
    assert_eq!(store.settings().overlay.font_size, 33);
    assert_eq!(fs::read(&path).unwrap(), legacy, "opening must not rewrite old state");
    let partial: OverlaySettings =
        serde_json::from_str(r#"{"font_size":40}"#).unwrap();
    assert!(partial.show_shortcut_hint);
    let explicit: OverlaySettings =
        serde_json::from_str(r#"{"show_shortcut_hint":false}"#).unwrap();
    assert!(!explicit.show_shortcut_hint);
}

#[test]
fn hiding_the_hint_round_trips_through_the_store() {
    let directory = tempdir().unwrap();
    let mut store = Store::open(directory.path().to_owned()).unwrap();
    let hidden = OverlaySettings {
        show_shortcut_hint: false,
        font_size: 31,
        ..Default::default()
    };
    store.save_overlay_settings(hidden.clone()).unwrap();
    let reopened = Store::open(directory.path().to_owned()).unwrap();
    assert_eq!(reopened.settings().overlay, hidden);
    assert!(!reopened.settings().overlay.show_shortcut_hint);
    let document: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.path().join("state.json")).unwrap()).unwrap();
    assert_eq!(
        document["settings"]["overlay"]["show_shortcut_hint"],
        serde_json::json!(false),
        "the switch must be written to disk, not only held in memory"
    );
    // Turning it back on must also persist (no one-way flag).
    let mut store = Store::open(directory.path().to_owned()).unwrap();
    store
        .save_overlay_settings(OverlaySettings {
            show_shortcut_hint: true,
            ..hidden.clone()
        })
        .unwrap();
    let reopened = Store::open(directory.path().to_owned()).unwrap();
    assert!(reopened.settings().overlay.show_shortcut_hint);
    assert_eq!(reopened.settings().overlay.font_size, 31, "unrelated overlay fields survive");
}

#[test]
fn both_hint_states_validate() {
    for visible in [true, false] {
        let settings = Settings {
            overlay: OverlaySettings {
                show_shortcut_hint: visible,
                ..Default::default()
            },
            ..Default::default()
        };
        settings.validate().unwrap();
    }
}

#[cfg(feature = "gui")]
#[test]
fn hint_text_stays_the_same_when_the_switch_is_off() {
    use live_hub_rust::overlay::{DisplaySettings, hint_text};
    let shown = DisplaySettings::default();
    let text = hint_text(&shown);
    assert!(text.contains(&format!("F7/F8 字号 {}px", shown.font_size.round() as i32)), "{text}");
    assert!(text.contains("F9 弹幕开"), "{text}");
    assert!(text.contains("F10 SC开"), "{text}");
    let muted = DisplaySettings {
        danmaku_visible: false,
        super_chat_visible: false,
        ..Default::default()
    };
    let muted_text = hint_text(&muted);
    assert!(muted_text.contains("F9 弹幕关"), "{muted_text}");
    assert!(muted_text.contains("F10 SC关"), "{muted_text}");
    // hint_text only describes the hotkey state; whether the line is painted at
    // all is decided by OverlaySettings.show_shortcut_hint in paint_status, so
    // hiding the hint cannot change the status/feedback text.
    assert_eq!(hint_text(&DisplaySettings::default()), text);
}
