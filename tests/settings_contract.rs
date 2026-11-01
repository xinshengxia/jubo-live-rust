use live_hub_rust::core::{
    EmoteSizeMode, OverlaySettings, PercentRect, Platform, ScCorner, ScMode, Settings, Store,
    Target, TextBorderStyle,
};
use std::{fs, path::PathBuf};
use tempfile::tempdir;

#[test]
fn old_state_without_overlay_preferences_loads_defaults_without_rewriting() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("state.json");
    let legacy =
        br#"{"schema_version":1,"settings":{"mpv":"players/mpv.exe"},"favorites":[],"history":[]}"#;
    fs::write(&path, legacy).unwrap();
    let store = Store::open(directory.path().to_owned()).unwrap();
    assert_eq!(store.settings().overlay, OverlaySettings::default());
    assert_eq!(store.settings().mpv, Some(PathBuf::from("players/mpv.exe")));
    assert_eq!(fs::read(&path).unwrap(), legacy);
    let partial: OverlaySettings =
        serde_json::from_str(r#"{"enabled":false,"font_size":40}"#).unwrap();
    assert!(!partial.enabled);
    assert_eq!(partial.font_size, 40);
    assert_eq!(partial.speed_px_per_sec, 125);
    assert_eq!(partial.max_visible, 150);
}

#[test]
fn saved_preferences_survive_reopening_and_merge_without_losing_player_paths() {
    let directory = tempdir().unwrap();
    let mut editor = Store::open(directory.path().to_owned()).unwrap();
    let mut other = Store::open(directory.path().to_owned()).unwrap();
    other
        .save_settings(Settings {
            mpv: Some("players/new-mpv.exe".into()),
            ..Default::default()
        })
        .unwrap();
    other
        .add_favorite(
            "Synthetic favorite".into(),
            Target::new(Platform::Bili, "12345").unwrap(),
        )
        .unwrap();
    let overlay = OverlaySettings {
        enabled: false,
        font_size: 41,
        speed_px_per_sec: 185,
        opacity_percent: 70,
        display_area_percent: 50,
        max_visible: 80,
        superchat_enabled: false,
        ..Default::default()
    };
    editor.save_overlay_settings(overlay.clone()).unwrap();
    let reopened = Store::open(directory.path().to_owned()).unwrap();
    assert_eq!(reopened.settings().overlay, overlay);
    assert_eq!(reopened.settings().mpv, Some("players/new-mpv.exe".into()));
    assert_eq!(reopened.favorites().len(), 1);
    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.path().join("state.json")).unwrap()).unwrap();
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["settings"]["overlay"]["enabled"], false);
    other
        .save_player_paths(Settings {
            potplayer: Some("players/new-potplayer.exe".into()),
            ..Default::default()
        })
        .unwrap();
    let reopened = Store::open(directory.path().to_owned()).unwrap();
    assert_eq!(
        reopened.settings().overlay,
        overlay,
        "the stale path editor must not revert overlay settings"
    );
    assert_eq!(
        reopened.settings().potplayer,
        Some("players/new-potplayer.exe".into())
    );
}

#[test]
fn every_out_of_range_preference_is_rejected_without_replacing_committed_state() {
    let directory = tempdir().unwrap();
    let mut store = Store::open(directory.path().to_owned()).unwrap();
    store.save_settings(Settings::default()).unwrap();
    let original = fs::read(directory.path().join("state.json")).unwrap();
    let mut candidates = Vec::new();
    for font_size in [0, 11, 73, u16::MAX] {
        candidates.push(OverlaySettings {
            font_size,
            ..Default::default()
        });
    }
    for speed_px_per_sec in [0, 19, 601, u16::MAX] {
        candidates.push(OverlaySettings {
            speed_px_per_sec,
            ..Default::default()
        });
    }
    for opacity_percent in [0, 9, 101, u8::MAX] {
        candidates.push(OverlaySettings {
            opacity_percent,
            ..Default::default()
        });
    }
    for display_area_percent in [0, 9, 101, u8::MAX] {
        candidates.push(OverlaySettings {
            display_area_percent,
            ..Default::default()
        });
    }
    for max_visible in [0, 9, 501, u16::MAX] {
        candidates.push(OverlaySettings {
            max_visible,
            ..Default::default()
        });
    }
    for invalid in candidates {
        assert!(store.save_overlay_settings(invalid).is_err());
        assert_eq!(
            fs::read(directory.path().join("state.json")).unwrap(),
            original
        );
        assert_eq!(store.settings().overlay, OverlaySettings::default());
    }
    for valid in [
        OverlaySettings {
            font_size: 12,
            speed_px_per_sec: 20,
            opacity_percent: 10,
            display_area_percent: 10,
            max_visible: 10,
            ..Default::default()
        },
        OverlaySettings {
            font_size: 72,
            speed_px_per_sec: 600,
            opacity_percent: 100,
            display_area_percent: 100,
            max_visible: 500,
            ..Default::default()
        },
    ] {
        assert!(valid.validate().is_ok());
    }
}

#[test]
fn invalid_persisted_overlay_values_are_not_silently_reset() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("state.json");
    let invalid = br#"{"schema_version":1,"settings":{"overlay":{"max_visible":0}},"favorites":[],"history":[]}"#;
    fs::write(&path, invalid).unwrap();
    assert!(Store::open(directory.path().to_owned()).is_err());
    assert_eq!(fs::read(&path).unwrap(), invalid);
    assert!(serde_json::from_str::<OverlaySettings>(r#"{"mystery":true}"#).is_err());
}

#[cfg(feature = "gui")]
#[test]
fn overlay_child_uses_the_exact_explicit_data_root() {
    use live_hub_rust::gui::overlay_arguments_with_data_root;
    use std::ffi::OsString;
    let directory = tempdir().unwrap();
    let data_root = directory.path().join("独立 数据目录");
    let target = Target::new(Platform::Huya, "synthetic_room").unwrap();
    let arguments = overlay_arguments_with_data_root(&target, 4321, &data_root).unwrap();
    assert_eq!(arguments[0], OsString::from("--data-dir"));
    assert_eq!(arguments[1], data_root.as_os_str());
    assert_eq!(
        &arguments[2..],
        &[
            OsString::from("overlay"),
            OsString::from("huya:synthetic_room"),
            OsString::from("--player-pid"),
            OsString::from("4321")
        ]
    );
    assert!(overlay_arguments_with_data_root(&target, 4321, std::path::Path::new("")).is_err());
    assert!(overlay_arguments_with_data_root(&target, 0, &data_root).is_err());
    assert!(!data_root.exists());
}

#[cfg(feature = "gui")]
#[test]
fn verified_account_cannot_replace_another_profile_or_an_invalid_room() {
    use live_hub_rust::{gui::apply_verified_candidate, platforms::LookupCandidate};
    let selected = LookupCandidate {
        name: "Synthetic account".into(),
        room: String::new(),
        platform: Platform::Douyin,
        title: String::new(),
        profile_url: Some("https://www.douyin.com/user/synthetic-account".into()),
        account_id: Some("1234".into()),
        ..Default::default()
    };
    let mut candidates = vec![selected.clone()];
    let mut verified = selected.clone();
    verified.room = "5678".into();
    let mut wrong = verified.clone();
    wrong.profile_url = Some("https://www.douyin.com/user/other-account".into());
    assert!(apply_verified_candidate(&mut candidates, &selected, wrong).is_err());
    assert!(candidates[0].room.is_empty());
    let mut wrong = verified.clone();
    wrong.room = "../not-a-room".into();
    assert!(apply_verified_candidate(&mut candidates, &selected, wrong).is_err());
    assert!(candidates[0].room.is_empty());
    apply_verified_candidate(&mut candidates, &selected, verified).unwrap();
    assert_eq!(candidates[0].room, "5678");
    assert_eq!(candidates[0].profile_url, selected.profile_url);
}

#[test]
fn old_overlay_state_without_rich_media_fields_gets_documented_defaults() {
    let partial: OverlaySettings = serde_json::from_str(
        r#"{"enabled":false,"font_size":40,"behavior":{"show_images":false}}"#,
    )
    .unwrap();
    assert!(!partial.enabled);
    assert_eq!(partial.font_size, 40);
    assert!(!partial.behavior.show_images);
    // New fields keep the documented backward-compatible defaults.
    assert!(partial.solid_text);
    assert_eq!(partial.text_opacity_percent, 100);
    assert_eq!(partial.border_style, TextBorderStyle::Outline);
    assert!(partial.scale_with_window);
    assert_eq!(partial.scale_reference_height, 720);
    assert_eq!(partial.line_gap_percent, 100);
    assert_eq!(partial.emote_size, EmoteSizeMode::Platform);
    assert!(partial.reserved_areas.is_empty());
    assert_eq!(partial.sc_mode, ScMode::Full);
    assert_eq!(partial.sc_max_stack, 6);
    assert_eq!(partial.sc_scale_percent, 100);
    assert_eq!(partial.sc_duration_scale_percent, 100);
    assert!(partial.sc_show_avatar);
    assert_eq!(partial.sc_corner, ScCorner::TopRight);
    assert!(partial.behavior.show_scrolling);
    assert!(partial.behavior.show_top);
    assert!(partial.behavior.show_bottom);
    assert!(partial.behavior.show_colored);
    assert_eq!(partial.behavior.event_duration_secs, 5);
    assert_eq!(partial.behavior.event_max_visible, 3);
}

#[test]
fn rich_media_overlay_preferences_round_trip_and_reject_out_of_range_values() {
    let directory = tempdir().unwrap();
    let mut store = Store::open(directory.path().to_owned()).unwrap();
    let mut overlay = OverlaySettings::default();
    overlay.solid_text = false;
    overlay.text_opacity_percent = 73;
    overlay.stroke_enabled = false;
    overlay.stroke_width = 3;
    overlay.border_style = TextBorderStyle::Shadow;
    overlay.scale_with_window = false;
    overlay.scale_reference_height = 1080;
    overlay.line_gap_percent = 130;
    overlay.show_medal = true;
    overlay.show_guard = true;
    overlay.show_user_level = true;
    overlay.show_user_name = true;
    overlay.emote_size = EmoteSizeMode::Large;
    overlay.show_color_emoji = false;
    overlay.reserved_areas = vec![
        PercentRect::parse("0,85,100,15").unwrap(),
        PercentRect::parse("0,0,100,10").unwrap(),
    ];
    overlay.sc_mode = ScMode::Compact;
    overlay.sc_max_stack = 0;
    overlay.sc_scale_percent = 150;
    overlay.sc_duration_scale_percent = 250;
    overlay.sc_show_avatar = false;
    overlay.sc_corner = ScCorner::BottomLeft;
    overlay.behavior.show_scrolling = false;
    overlay.behavior.show_top = false;
    overlay.behavior.show_bottom = false;
    overlay.behavior.show_colored = false;
    overlay.behavior.min_user_level = 20;
    overlay.behavior.min_medal_level = 15;
    overlay.behavior.show_guards = true;
    overlay.behavior.show_notice = true;
    overlay.behavior.show_live_stats = true;
    overlay.behavior.event_duration_secs = 8;
    overlay.behavior.event_max_visible = 5;
    store.save_overlay_settings(overlay.clone()).unwrap();
    let reopened = Store::open(directory.path().to_owned()).unwrap();
    assert_eq!(reopened.settings().overlay, overlay);
    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.path().join("state.json")).unwrap()).unwrap();
    assert_eq!(json["settings"]["overlay"]["border_style"], "shadow");
    assert_eq!(json["settings"]["overlay"]["sc_corner"], "bl");
    assert_eq!(
        json["settings"]["overlay"]["reserved_areas"][0]["width"],
        100
    );
    assert_eq!(
        json["settings"]["overlay"]["behavior"]["event_max_visible"],
        5
    );

    let rejected = |mutate: &dyn Fn(&mut OverlaySettings)| {
        let mut invalid = overlay.clone();
        mutate(&mut invalid);
        assert!(invalid.validate().is_err());
    };
    rejected(&|value| value.text_opacity_percent = 101);
    rejected(&|value| value.stroke_width = 0);
    rejected(&|value| value.scale_reference_height = 100);
    rejected(&|value| value.line_gap_percent = 10);
    rejected(&|value| value.sc_scale_percent = 59);
    rejected(&|value| value.sc_duration_scale_percent = 401);
    rejected(&|value| value.sc_max_stack = 201);
    rejected(&|value| {
        value.reserved_areas = (0..13)
            .map(|index| PercentRect {
                x: index as u8,
                y: 0,
                width: 5,
                height: 5,
            })
            .collect()
    });
    rejected(&|value| value.behavior.min_user_level = 61);
    rejected(&|value| value.behavior.min_medal_level = 41);
    rejected(&|value| value.behavior.event_duration_secs = 1);
    rejected(&|value| value.behavior.event_max_visible = 0);
}

#[test]
fn second_round_danmaku_layout_fields_round_trip_and_validate() {
    // Old state files without the new fields must keep the documented defaults.
    let partial: OverlaySettings =
        serde_json::from_str(r#"{"behavior":{"show_scrolling":false}}"#).unwrap();
    let behavior = &partial.behavior;
    assert!(!behavior.show_scrolling);
    assert!(behavior.show_reverse);
    assert_eq!(behavior.top_fixed_zone_percent, 40);
    assert_eq!(behavior.bottom_fixed_zone_percent, 40);
    assert_eq!(behavior.max_fixed_per_mode, 6);
    assert!(behavior.fixed_overflow_to_scroll);
    assert!(behavior.smooth_admission);
    assert_eq!(behavior.admission_per_second, 0);
    assert_eq!(behavior.queue_wait_ms, 800);
    assert_eq!(behavior.queue_capacity, 64);
    assert!(behavior.shrink_when_crowded);
    assert_eq!(behavior.crowded_scale_percent, 85);

    let directory = tempdir().unwrap();
    let mut store = Store::open(directory.path().to_owned()).unwrap();
    let mut overlay = OverlaySettings::default();
    overlay.behavior.show_reverse = false;
    overlay.behavior.top_fixed_zone_percent = 25;
    overlay.behavior.bottom_fixed_zone_percent = 30;
    overlay.behavior.max_fixed_per_mode = 2;
    overlay.behavior.fixed_overflow_to_scroll = false;
    overlay.behavior.smooth_admission = false;
    overlay.behavior.admission_per_second = 40;
    overlay.behavior.queue_wait_ms = 250;
    overlay.behavior.queue_capacity = 10;
    overlay.behavior.shrink_when_crowded = false;
    overlay.behavior.crowded_scale_percent = 70;
    store.save_overlay_settings(overlay.clone()).unwrap();
    let reopened = Store::open(directory.path().to_owned()).unwrap();
    assert_eq!(reopened.settings().overlay, overlay);
    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.path().join("state.json")).unwrap()).unwrap();
    assert_eq!(
        json["settings"]["overlay"]["behavior"]["show_reverse"],
        false
    );
    assert_eq!(
        json["settings"]["overlay"]["behavior"]["queue_capacity"],
        10
    );
    assert_eq!(
        json["settings"]["overlay"]["behavior"]["crowded_scale_percent"],
        70
    );

    let rejected = |mutate: &dyn Fn(&mut OverlaySettings)| {
        let mut invalid = overlay.clone();
        mutate(&mut invalid);
        assert!(
            invalid.validate().is_err(),
            "an out-of-range layout field must be rejected before saving"
        );
    };
    rejected(&|value| value.behavior.top_fixed_zone_percent = 9);
    rejected(&|value| value.behavior.top_fixed_zone_percent = 101);
    rejected(&|value| value.behavior.bottom_fixed_zone_percent = 9);
    rejected(&|value| value.behavior.max_fixed_per_mode = 61);
    rejected(&|value| value.behavior.admission_per_second = 121);
    rejected(&|value| value.behavior.queue_wait_ms = 1001);
    rejected(&|value| value.behavior.queue_capacity = 0);
    rejected(&|value| value.behavior.queue_capacity = 201);
    rejected(&|value| value.behavior.crowded_scale_percent = 69);
    rejected(&|value| value.behavior.crowded_scale_percent = 101);
    // Boundary values are accepted.
    let mut boundary = overlay.clone();
    boundary.behavior.top_fixed_zone_percent = 10;
    boundary.behavior.bottom_fixed_zone_percent = 100;
    boundary.behavior.max_fixed_per_mode = 60;
    boundary.behavior.admission_per_second = 120;
    boundary.behavior.queue_wait_ms = 1000;
    boundary.behavior.queue_capacity = 200;
    boundary.behavior.crowded_scale_percent = 70;
    assert!(boundary.validate().is_ok());
}
