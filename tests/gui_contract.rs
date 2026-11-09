#![cfg(feature = "gui")]
//! Contract tests for the launcher's pure helpers.
//!
//! No GUI window is created, no player is started, no network is contacted and
//! only synthetic text in temporary directories is used.

use live_hub_rust::core::{Platform, Target};
use live_hub_rust::gui::{
    AUTO_HEALTH_INTERVAL_SECS, CredentialState, OverlayLaunch, app_icon, clear_due_health_attempts,
    credential_platform_label, detected_platform, health_age_text, implemented_summary,
    next_health_check, overlay_arguments, overlay_platform_supported, overlay_status_text,
    parse_extra_args, parse_reserved_areas, pending_summary, read_credential_text,
    reserved_areas_text, room_format_help_text, room_input_hint,
};
use std::collections::BTreeMap;
use std::fs;
use tempfile::tempdir;

#[test]
fn window_icon_is_a_self_contained_accent_play_mark() {
    // The native window must not keep eframe's placeholder icon: the launcher
    // supplies a 64x64 RGBA icon generated from pure arithmetic, without files.
    let icon = app_icon();
    assert_eq!((icon.width, icon.height), (64, 64));
    assert_eq!(icon.rgba.len(), 64 * 64 * 4);
    // Rounded corners stay transparent instead of painting a full square.
    assert_eq!(
        &icon.rgba[0..4],
        &[0, 0, 0, 0],
        "top-left corner must be transparent"
    );
    let mut opaque = 0_usize;
    let mut accent = 0_usize;
    let mut play_mark = 0_usize;
    for pixel in icon.rgba.chunks_exact(4) {
        if pixel[3] == 255 {
            opaque += 1;
        }
        if pixel[3] == 255 && pixel[0] == 244 && pixel[1] == 137 && pixel[2] == 67 {
            accent += 1;
        }
        if pixel[3] == 255 && pixel[0] == 30 && pixel[1] == 24 && pixel[2] == 20 {
            play_mark += 1;
        }
    }
    assert!(
        opaque > (64 * 64) / 2,
        "icon plate must cover most of the canvas"
    );
    assert!(accent > 1_000, "accent plate missing: {accent}");
    assert!(play_mark > 100, "play mark missing: {play_mark}");
    assert!(
        accent + play_mark < 64 * 64,
        "the icon must keep transparent margins"
    );
}

#[test]
fn overlay_command_matches_the_cli_subcommand_contract() {
    let bili = Target::new(Platform::Bili, "123").unwrap();
    assert_eq!(
        overlay_arguments(&bili, 4321).unwrap(),
        vec![
            "overlay".to_owned(),
            "bili:123".to_owned(),
            "--player-pid".to_owned(),
            "4321".to_owned()
        ]
    );
    let douyu = Target::new(Platform::Douyu, "room_1").unwrap();
    assert_eq!(overlay_arguments(&douyu, 7).unwrap()[1], "douyu:room_1");
    assert!(
        overlay_arguments(&bili, 0).is_err(),
        "a zero player pid can never be followed"
    );
    for (platform, room) in [
        (Platform::Huya, "abc"),
        (Platform::Douyu, "room_1"),
        (Platform::Douyin, "987654"),
    ] {
        let target = Target::new(platform, room).unwrap();
        assert_eq!(
            overlay_arguments(&target, 9).unwrap()[1],
            format!("{platform}:{room}")
        );
    }
}

#[test]
fn overlay_platform_support_matches_the_receiver_modules() {
    // The shared danmaku receiver dispatches per platform; every launcher platform
    // may start the overlay (an unsupported live transport reports its own error in-window).
    assert!(
        Platform::ALL
            .iter()
            .all(|platform| overlay_platform_supported(*platform))
    );
}

#[test]
fn overlay_status_keeps_playback_success_visible_in_every_outcome() {
    assert_eq!(
        overlay_status_text("合成主播", &OverlayLaunch::Disabled),
        "已启动播放器：合成主播。"
    );
    assert!(
        overlay_status_text("合成主播", &OverlayLaunch::Launched)
            .starts_with("已启动播放器：合成主播。")
    );
    for launch in [
        OverlayLaunch::UnsupportedPlatform(Platform::Huya),
        OverlayLaunch::UnsupportedSystem,
        OverlayLaunch::Failed("合成失败".to_owned()),
    ] {
        let text = overlay_status_text("合成主播", &launch);
        assert!(
            text.starts_with("已启动播放器：合成主播。"),
            "playback must stay reported: {text}"
        );
        assert!(
            text.contains("播放不受影响"),
            "failure must not block playback: {text}"
        );
    }
    assert!(OverlayLaunch::Failed("合成失败".to_owned()).is_failure());
    assert!(!OverlayLaunch::Launched.is_failure());
    assert!(!OverlayLaunch::Disabled.is_failure());
}

#[test]
fn pasted_official_links_select_their_platform_but_bare_rooms_do_not() {
    assert_eq!(
        detected_platform("https://live.bilibili.com/123"),
        Some(Platform::Bili)
    );
    assert_eq!(
        detected_platform("https://www.huya.com/example_room-1"),
        Some(Platform::Huya)
    );
    assert_eq!(
        detected_platform("https://www.douyu.com/abc123"),
        Some(Platform::Douyu)
    );
    assert_eq!(
        detected_platform("https://live.douyin.com/987654"),
        Some(Platform::Douyin)
    );
    assert_eq!(
        detected_platform("huya:example_room-1"),
        Some(Platform::Huya)
    );
    for input in ["12345", "", "https://example.invalid/123", "unknown:123"] {
        assert_eq!(
            detected_platform(input),
            None,
            "input must not override the picker: {input}"
        );
    }
}

#[test]
fn room_errors_become_platform_specific_hints() {
    let numeric = Target::new(Platform::Bili, "abc").unwrap_err();
    let hint = room_input_hint("abc", Platform::Bili, &format!("{numeric:#}"));
    assert!(
        hint.contains("B站") && hint.contains("纯数字"),
        "unexpected hint: {hint}"
    );
    let huya = Target::new(Platform::Huya, "bad/room").unwrap_err();
    let hint = room_input_hint("bad/room", Platform::Huya, &format!("{huya:#}"));
    assert!(
        hint.contains("虎牙") && hint.contains("只能包含"),
        "unexpected hint: {hint}"
    );
    assert!(
        room_input_hint(
            "",
            Platform::Bili,
            "Enter a room ID or an official room URL"
        )
        .contains("房间号")
    );
    let link = room_input_hint(
        "https://example.invalid/1",
        Platform::Bili,
        "Use an official live-room URL or platform:room",
    );
    assert!(link.contains("官方直播间链接"), "unexpected hint: {link}");
}

#[test]
fn format_help_dialog_text_documents_every_supported_room_form() {
    let help = room_format_help_text();
    for marker in [
        "房间号",
        "官方直播间链接",
        "平台:房间",
        "bili:123",
        "huya:123",
        "douyu:123",
        "douyin:123",
        "https://live.bilibili.com/12345",
        "https://www.huya.com/12345",
        "https://www.douyu.com/12345",
        "https://live.douyin.com/12345",
        "纯数字",
    ] {
        assert!(help.contains(marker), "format help misses {marker}: {help}");
    }
    assert!(
        !help.contains("\\n"),
        "help text must not contain a literal backslash-n: {help}"
    );
    // Every concrete example in the dialog must be accepted by the real parser,
    // so the help cannot drift away from what the launch card accepts.
    for example in [
        "bili:123",
        "huya:196645",
        "douyu:93589",
        "douyin:46595453118",
        "https://live.bilibili.com/12345",
        "https://www.huya.com/12345",
        "https://www.douyu.com/12345",
        "https://live.douyin.com/12345",
    ] {
        Target::parse(example).unwrap_or_else(|error| {
            panic!("documented example must parse, {example}: {error:#}")
        });
    }
}

#[test]
fn credential_text_reader_enforces_the_documented_limits() {
    let directory = tempdir().unwrap();
    let valid = directory.path().join("cookies.txt");
    fs::write(
        &valid,
        "\u{feff}SESSION=SYNTHETIC_ONE; token=SYNTHETIC_TWO\n",
    )
    .unwrap();
    assert_eq!(
        read_credential_text(&valid).unwrap(),
        "SESSION=SYNTHETIC_ONE; token=SYNTHETIC_TWO\n"
    );
    assert!(read_credential_text(&directory.path().join("missing.txt")).is_err());
    assert!(
        read_credential_text(directory.path()).is_err(),
        "a directory is not a credential file"
    );
    let binary = directory.path().join("binary.txt");
    fs::write(&binary, [0xff, 0xfe, 0xfd]).unwrap();
    assert!(
        read_credential_text(&binary).is_err(),
        "non-UTF-8 text must be rejected"
    );
    let oversized = directory.path().join("oversized.txt");
    let file = fs::File::create(&oversized).unwrap();
    file.set_len(2 * 1024 * 1024 + 1).unwrap();
    drop(file);
    assert!(
        read_credential_text(&oversized).is_err(),
        "files over 2 MiB must be rejected"
    );
}

#[test]
fn credential_labels_cover_imported_missing_and_unsupported() {
    let mut state = CredentialState {
        supported: true,
        imported: BTreeMap::new(),
        error: None,
    };
    assert_eq!(
        credential_platform_label(Platform::Bili, &state),
        "B站：未导入"
    );
    state.imported.insert(Platform::Bili, true);
    assert!(credential_platform_label(Platform::Bili, &state).contains("已导入"));
    assert!(credential_platform_label(Platform::Douyu, &state).contains("未导入"));
    let unsupported = CredentialState {
        supported: false,
        imported: BTreeMap::new(),
        error: None,
    };
    assert!(credential_platform_label(Platform::Douyin, &unsupported).contains("不支持"));
}

#[test]
fn feature_summaries_cover_overlay_credentials_and_pending_work() {
    let implemented = implemented_summary().join("\n");
    assert!(implemented.contains("弹幕覆盖层"));
    assert!(implemented.contains("DPAPI"));
    assert!(implemented.contains("平台取流"));
    assert!(implemented.contains("虎牙") && implemented.contains("抖音"));
    assert!(implemented.contains("登录凭据"));
    let pending = pending_summary().join("\n");
    assert!(
        implemented.contains("二维码") && pending.contains("专属浏览器登录"),
        "implemented login and remaining real-account acceptance must be distinguished"
    );
    assert!(
        pending.contains("真实平台"),
        "live acceptance must stay visible as pending"
    );
    assert!(pending.contains("虎牙") && pending.contains("抖音"));
    assert!(!implemented_summary().is_empty() && !pending_summary().is_empty());
}
fn imported_credentials(platforms: &[Platform]) -> CredentialState {
    let mut state = CredentialState {
        supported: true,
        imported: BTreeMap::new(),
        error: None,
    };
    for platform in platforms {
        state.imported.insert(*platform, true);
    }
    state
}

#[test]
fn automatic_health_checks_visit_each_saved_platform_once_per_interval() {
    let interval = AUTO_HEALTH_INTERVAL_SECS;
    let mut attempts = BTreeMap::new();
    // A platform without saved credentials is never contacted.
    assert_eq!(
        next_health_check(&CredentialState::default(), &attempts, 1_000, interval),
        None
    );
    let imported = imported_credentials(&[Platform::Bili, Platform::Huya]);
    // First due platform in the fixed platform order.
    assert_eq!(
        next_health_check(&imported, &attempts, 1_000, interval),
        Some(Platform::Bili)
    );
    attempts.insert(Platform::Bili, 1_000);
    // The next platform follows; the checked one stays quiet.
    assert_eq!(
        next_health_check(&imported, &attempts, 1_200, interval),
        Some(Platform::Huya)
    );
    attempts.insert(Platform::Huya, 1_200);
    assert_eq!(
        next_health_check(&imported, &attempts, 1_599, interval),
        None
    );
    // Once the interval elapsed the first platform becomes due again.
    assert_eq!(
        next_health_check(&imported, &attempts, 1_600, interval),
        Some(Platform::Bili)
    );
    // A clock that moved backwards must not suppress a check forever.
    assert_eq!(
        next_health_check(&imported, &attempts, 10, interval),
        Some(Platform::Bili)
    );
}

#[test]
fn health_age_labels_are_bounded_and_never_claim_the_future() {
    assert_eq!(health_age_text(1_000, 1_000), "刚刚检查");
    assert_eq!(health_age_text(940, 1_000), "1 分钟前检查");
    assert_eq!(health_age_text(400, 1_000), "10 分钟前检查");
    assert_eq!(health_age_text(0, 7_200), "2 小时前检查");
    assert_eq!(health_age_text(0, 172_800), "2 天前检查");
    assert_eq!(health_age_text(2_000, 1_000), "检查时间未知");
}

#[test]
fn manual_rechecks_clear_only_attempts_older_than_the_ten_second_floor() {
    let imported = imported_credentials(&[Platform::Bili, Platform::Huya]);
    let mut attempts = BTreeMap::new();
    attempts.insert(Platform::Bili, 1_015);
    attempts.insert(Platform::Huya, 900);
    attempts.insert(Platform::Douyu, 100);
    let cleared = clear_due_health_attempts(&mut attempts, &imported, 1_020);
    assert_eq!(cleared, 1, "only the saved platform older than 10 s is due");
    assert!(attempts.contains_key(&Platform::Bili));
    assert!(!attempts.contains_key(&Platform::Huya));
    // A platform without saved credentials is never scheduled.
    assert!(!attempts.contains_key(&Platform::Douyu));
    assert_eq!(
        clear_due_health_attempts(&mut attempts, &imported, 1_021),
        0
    );
}

#[test]
fn reserved_area_text_round_trips_and_rejects_invalid_or_excess_rows() {
    let text = "0,85,100,15\n10,0,30,20";
    let areas = parse_reserved_areas(text).unwrap();
    assert_eq!(areas.len(), 2);
    assert_eq!(reserved_areas_text(&areas), text);
    assert!(parse_reserved_areas("   \n").unwrap().is_empty());
    assert!(
        parse_reserved_areas("0,0,0,10").is_err(),
        "zero width is invalid"
    );
    assert!(parse_reserved_areas("bad row").is_err());
    let too_many = (0..13)
        .map(|index| format!("{index},0,10,10"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(parse_reserved_areas(&too_many).is_err());
}

#[test]
fn player_extra_arguments_are_one_validated_flag_per_line() {
    let parsed =
        parse_extra_args("--cache-secs=3\n\n  --demuxer-readahead-secs=2 ", "--", 64).unwrap();
    assert_eq!(
        parsed,
        vec![
            "--cache-secs=3".to_owned(),
            "--demuxer-readahead-secs=2".to_owned()
        ]
    );
    assert!(parse_extra_args("-bad", "--", 64).is_err());
    assert!(parse_extra_args("--control\u{7}", "--", 64).is_err());
    assert!(parse_extra_args("/x", "--", 64).is_err());
    assert!(parse_extra_args(&"--x\n".repeat(65), "--", 64).is_err());
}

#[test]
fn local_observations_mark_stale_active_stages_without_claiming_expiry() {
    use live_hub_rust::gui::observations::{ObservationLog, ObservationStage, ObservationState};
    let mut log = ObservationLog::default();
    log.record(
        Platform::Bili,
        ObservationStage::Room,
        ObservationState::Live,
        1_000,
        "123",
    );
    log.record_state(
        Platform::Bili,
        ObservationStage::Danmaku,
        ObservationState::OverlayStarted,
        1_000,
    );
    let rows = log.rows(Platform::Bili, 1_200);
    assert_eq!(rows[0].result, "正在直播");
    assert_eq!(rows[0].room, "123");
    assert!(!rows[0].stale);
    assert_eq!(rows[2].stage, ObservationStage::Danmaku);
    assert!(rows[2].result.contains("弹幕覆盖层已启动"));
    let stale = log.rows(Platform::Bili, 1_301);
    assert!(stale[0].stale && stale[2].stale);
    assert!(stale[0].result.contains("记录超过 5 分钟，待重新观测"));
    assert!(stale[0].result.contains("不表示 Cookie 过期"));
    // Terminal states are not re-observed; only 5-minute activity claims go stale.
    log.record_state(
        Platform::Huya,
        ObservationStage::Playback,
        ObservationState::Stopped,
        1_000,
    );
    assert!(!log.rows(Platform::Huya, 99_999)[1].stale);
    assert_eq!(log.rows(Platform::Douyu, 1_000)[0].result, "尚无记录");
    assert!(
        log.detail_text(Platform::Bili, 1_000)
            .contains("房间状态：正在直播")
    );
}

#[test]
fn observation_log_round_trips_under_the_isolated_data_root() {
    use live_hub_rust::gui::observations::{
        ObservationLog, ObservationStage, ObservationState, observation_path,
    };
    let directory = tempdir().unwrap();
    let mut log = ObservationLog::default();
    log.record(
        Platform::Douyu,
        ObservationStage::Playback,
        ObservationState::Playing,
        5_000,
        "room_1",
    );
    log.save(directory.path()).unwrap();
    assert!(observation_path(directory.path()).is_file());
    let loaded = ObservationLog::load(directory.path());
    let row = loaded.rows(Platform::Douyu, 5_001)[1].clone();
    assert_eq!(row.room, "room_1");
    assert!(row.result.contains("播放中"));
    std::fs::write(observation_path(directory.path()), "not json").unwrap();
    assert!(ObservationLog::load(directory.path()).is_empty());
    std::fs::write(observation_path(directory.path()), r#"{"version":99}"#).unwrap();
    assert!(ObservationLog::load(directory.path()).is_empty());
}

#[test]
fn local_data_snapshot_projects_owned_files_and_hides_protected_names() {
    use live_hub_rust::core::{Favorite, HistoryEntry, Settings, Target};
    use live_hub_rust::gui::data_page::{
        DataCategory, DiagnosticLog, build_snapshot, cleanup_runtime, runtime_dir, sanitize_line,
        size_text, state_path,
    };
    let directory = tempdir().unwrap();
    let root = directory.path();
    std::fs::write(state_path(root), "{}").unwrap();
    std::fs::create_dir_all(runtime_dir(root)).unwrap();
    std::fs::write(runtime_dir(root).join("platform-observations.json"), "{}").unwrap();
    std::fs::write(runtime_dir(root).join("diagnostics.log"), "line\n").unwrap();
    std::fs::write(runtime_dir(root).join("notes.txt"), "unowned").unwrap();
    std::fs::write(runtime_dir(root).join("cookie-backup.txt"), "hidden").unwrap();
    let target = Target::new(Platform::Bili, "123").unwrap();
    let favorites = vec![Favorite {
        name: "合成主播".into(),
        target: target.clone(),
    }];
    let history = vec![HistoryEntry {
        name: "合成主播".into(),
        target,
        played_at: None,
        legacy_time: Some("10-30 18:00".into()),
    }];
    let snapshot = build_snapshot(root, &favorites, &history, &Settings::default());
    assert_eq!(snapshot.categories.len(), 5);
    let favorites_view = &snapshot.categories[0];
    assert_eq!(favorites_view.category, DataCategory::Favorites);
    assert_eq!(favorites_view.rows[0], vec!["合成主播", "B站", "123"]);
    assert_eq!(favorites_view.location, "state.json");
    let config = &snapshot.categories[2];
    assert!(config.rows.iter().any(|row| row[0] == "弹幕字号"));
    assert!(!config.rows.iter().any(|row| row[0].contains("Cookie")));
    let cache = &snapshot.categories[3];
    assert!(
        cache
            .rows
            .iter()
            .any(|row| row[0] == "platform-observations.json")
    );
    assert!(cache.rows.iter().any(|row| row[0] == "notes.txt"));
    assert!(!cache.rows.iter().any(|row| row[0] == "cookie-backup.txt"));
    let logs = &snapshot.categories[4];
    assert_eq!(logs.rows.len(), 1);
    assert_eq!(size_text(1_536), "1.50 KiB");
    let log = DiagnosticLog::new(root);
    log.record("Cookie: SESSION=SYNTHETIC_SECRET");
    log.record("GET https://example.invalid/private 失败");
    let preview = log.preview();
    assert!(!preview.contains("SYNTHETIC_SECRET"));
    assert!(!preview.contains("https://example.invalid"));
    assert!(preview.contains("[URL]"));
    assert_eq!(sanitize_line("普通行"), "普通行");
    let freed = cleanup_runtime(root).unwrap();
    assert!(freed > 0);
    assert!(
        !runtime_dir(root)
            .join("platform-observations.json")
            .exists()
    );
    assert!(runtime_dir(root).join("notes.txt").exists());
    assert_eq!(cleanup_runtime(root).unwrap(), 0);
}

#[test]
fn search_cache_respects_ttl_refresh_and_failure_boundaries() {
    use live_hub_rust::platforms::LookupCandidate;
    use live_hub_rust::search::{
        HINT_ALIAS, SEARCH_CACHE_TTL_SECS, SearchCache, SearchQuery, account_page, alias_match,
        candidate_stage, candidate_summary, normalize, rank_candidates, recovery_hint, room_page,
    };
    let candidate = |platform, name: &str, room: &str| LookupCandidate {
        name: name.into(),
        room: room.into(),
        platform,
        ..Default::default()
    };
    let mut cache = SearchCache::default();
    let key = SearchQuery::new("Synthetic", "", "", "").cache_key("bili");
    assert!(cache.get(&key, 1_000).is_none());
    cache.insert(
        key.clone(),
        vec![candidate(Platform::Bili, "Synthetic", "1")],
        1_000,
    );
    assert_eq!(cache.get(&key, 1_000).unwrap().candidates.len(), 1);
    assert!(cache.get(&key, 1_000 + SEARCH_CACHE_TTL_SECS - 1).is_some());
    assert!(cache.get(&key, 1_000 + SEARCH_CACHE_TTL_SECS).is_none());
    assert!(cache.remove(&key));
    cache.insert(key.clone(), Vec::new(), 1_000);
    assert!(
        cache.get(&key, 1_001).is_none(),
        "empty results are not cached"
    );

    let query = SearchQuery::from_input("浪D 游戏/领域：英雄联盟 外号/曾用名：K神");
    assert_eq!(query.platform_terms(), "浪D");
    assert_eq!(
        live_hub_rust::search::hint("浪D 外号/曾用名：K神", HINT_ALIAS),
        Some("K神")
    );
    assert_eq!(normalize("ＳＹＮＴＨＥＴＩＣ·Ｂ"), "syntheticb");
    assert!(alias_match("微竞-浪D", "浪D"));
    assert!(!alias_match("浪D粉丝团", "浪D"));
    let mut hinted = candidate(Platform::Huya, "微竞-浪D", "2");
    hinted.category = "英雄联盟".into();
    let unrelated = candidate(Platform::Bili, "浪D粉丝团", "3");
    let mut candidates = vec![unrelated, hinted];
    rank_candidates(&mut candidates, &query, false);
    assert_eq!(candidates[0].name, "微竞-浪D");
    assert!(recovery_hint("平台限流").contains("限流"));
    assert!(recovery_hint("登录验证").contains("账号与登录"));

    let mut account = candidate(Platform::Douyin, "Synthetic", "");
    account.account_id = Some("123456".into());
    assert_eq!(candidate_stage(&account), "找到账号，房间待解析");
    assert!(candidate_summary(&account).contains("找到账号"));
    assert!(account_page(&account).is_none());
    assert!(room_page(&account).is_none());
    let room = candidate(Platform::Bili, "Synthetic", "123");
    assert_eq!(
        room_page(&room).as_deref(),
        Some("https://live.bilibili.com/123")
    );
    assert_eq!(candidate_stage(&room), "已取得房间");
}

#[test]
fn live_badges_are_bounded_fresh_and_never_guess_a_failed_query() {
    use live_hub_rust::gui::badges::{
        BADGE_FRESH_SECS, BADGE_MAX_IN_FLIGHT, BADGE_UNKNOWN_RETRY_SECS, Badge, pick_badges,
        record_result, room_key,
    };
    use live_hub_rust::room_state::RoomState;
    let bili = Target::new(Platform::Bili, "1").unwrap();
    let huya = Target::new(Platform::Huya, "2").unwrap();
    let douyu = Target::new(Platform::Douyu, "3").unwrap();
    let keys = vec![
        room_key(&bili),
        room_key(&huya),
        room_key(&bili),
        room_key(&douyu),
    ];
    let mut times = std::collections::BTreeMap::new();
    let now = 10_000;
    times.insert(room_key(&bili), now - 5);
    let pending = std::collections::BTreeSet::from([room_key(&douyu)]);
    let batch = pick_badges(&keys, &pending, &times, now, BADGE_MAX_IN_FLIGHT);
    assert_eq!(batch, vec![room_key(&huya)]);
    assert!(batch.len() + pending.len() <= BADGE_MAX_IN_FLIGHT);
    // An unknown result retries after ~20 s, a confirmed one waits 2 minutes.
    let mut times = std::collections::BTreeMap::new();
    record_result(&mut times, &room_key(&bili), now, true);
    assert!(
        pick_badges(
            std::slice::from_ref(&room_key(&bili)),
            &std::collections::BTreeSet::new(),
            &times,
            now + BADGE_UNKNOWN_RETRY_SECS - 1,
            4
        )
        .is_empty()
    );
    assert_eq!(
        pick_badges(
            std::slice::from_ref(&room_key(&bili)),
            &std::collections::BTreeSet::new(),
            &times,
            now + BADGE_UNKNOWN_RETRY_SECS + 1,
            4
        )
        .len(),
        1
    );
    record_result(&mut times, &room_key(&bili), now, false);
    assert!(
        pick_badges(
            std::slice::from_ref(&room_key(&bili)),
            &std::collections::BTreeSet::new(),
            &times,
            now + BADGE_FRESH_SECS - 1,
            4
        )
        .is_empty()
    );
    assert_eq!(Badge::from_state(RoomState::Live).label(), "[播] ");
    assert_eq!(Badge::from_state(RoomState::Offline).label(), "[休] ");
    assert_eq!(Badge::from_state(RoomState::Replay).label(), "[重播] ");
    assert_eq!(Badge::from_state(RoomState::Unknown).label(), "");
}

#[test]
fn replay_classification_uses_platform_wording_and_never_claims_offline() {
    use live_hub_rust::gui::{replay_wait_hint, session_error_text};
    use live_hub_rust::platforms::{
        douyu_replay_from_flags, douyu_room_flags, huya_page_replay, is_replay_error,
        replay_unavailable_message,
    };
    assert!(huya_page_replay(r#"<div class="liveStatus-replay">"#));
    assert!(!huya_page_replay(r#"<div class="liveStatus-on">"#));
    assert_eq!(
        douyu_room_flags(&serde_json::json!({"room":{"show_status":0,"videoLoop":1}})),
        (Some(0), 1)
    );
    assert!(douyu_replay_from_flags(Some(0), 1));
    assert!(douyu_replay_from_flags(Some(2), 1));
    // A room the platform reports live is never reclassified as a recording.
    assert!(!douyu_replay_from_flags(Some(1), 1));
    assert!(!douyu_replay_from_flags(Some(0), 0));
    for (platform, expected) in [
        (Platform::Bili, "轮播"),
        (Platform::Huya, "重播"),
        (Platform::Douyu, "录播"),
        (Platform::Douyin, "重播"),
    ] {
        let message = replay_unavailable_message(platform, "合成原因");
        assert!(message.contains(expected), "{platform}: {message}");
        assert!(message.contains("合成原因"));
        assert!(!message.contains("未开播"));
        assert!(is_replay_error(&message));
        assert!(is_replay_error("房间正在重播，但平台暂未提供可用媒体"));
    }
    assert!(!is_replay_error("房间当前未开播"));
    // Session errors are translated per platform instead of keeping B站 wording.
    let translated = session_error_text(Platform::Huya, "房间正在重播，但平台暂未提供可用媒体");
    assert!(translated.contains("虎牙重播流不可用"), "{translated}");
    assert_eq!(session_error_text(Platform::Bili, "合成错误"), "合成错误");
    assert!(replay_wait_hint(Platform::Douyu).contains("斗鱼录播流不可用"));
}

#[test]
fn douyin_account_clues_stay_in_the_account_stage() {
    use live_hub_rust::search::douyin_account_candidate;
    let sec = "MS4wLjSyntheticAccount_B";
    let candidate = douyin_account_candidate(&format!("https://www.douyin.com/user/{sec}"))
        .expect("profile URL must be recognized");
    assert!(candidate.room.is_empty());
    assert_eq!(candidate.platform, Platform::Douyin);
    assert_eq!(
        candidate.profile_url.as_deref(),
        Some(format!("https://www.douyin.com/user/{sec}").as_str())
    );
    let handle = douyin_account_candidate("抖音号:synthetic_handle").unwrap();
    assert_eq!(handle.account_handle.as_deref(), Some("synthetic_handle"));
    assert!(handle.room.is_empty());
    assert!(douyin_account_candidate("https://live.douyin.com/123").is_none());
    assert!(douyin_account_candidate("抖音号:bad handle").is_none());
    // A verification failure points at the page assist only for Douyin.
    use live_hub_rust::search::douyin_web_assist_hint;
    assert!(
        douyin_web_assist_hint(Platform::Douyin, "平台要求登录或网页验证")
            .unwrap()
            .contains("抖音网页辅助")
    );
    assert!(douyin_web_assist_hint(Platform::Bili, "平台要求登录").is_none());
    assert!(douyin_web_assist_hint(Platform::Douyin, "unexpected shape").is_none());
}

#[test]
fn local_config_projection_covers_second_round_layout_fields() {
    use live_hub_rust::core::{DanmakuBehavior, OverlaySettings, Settings};
    use live_hub_rust::gui::data_page::config_rows;
    let settings = Settings {
        overlay: OverlaySettings {
            behavior: DanmakuBehavior {
                show_reverse: false,
                top_fixed_zone_percent: 30,
                bottom_fixed_zone_percent: 35,
                max_fixed_per_mode: 4,
                fixed_overflow_to_scroll: false,
                smooth_admission: false,
                admission_per_second: 30,
                queue_wait_ms: 500,
                queue_capacity: 12,
                shrink_when_crowded: false,
                crowded_scale_percent: 75,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let flat = config_rows(&settings)
        .iter()
        .map(|row| row.join("="))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(flat.contains("逆向弹幕=关闭"), "{flat}");
    assert!(flat.contains("固定弹幕区域（顶/底）=30% / 35%"), "{flat}");
    assert!(flat.contains("固定弹幕同屏上限=4 条"), "{flat}");
    assert!(flat.contains("过长固定弹幕转滚动=关闭"), "{flat}");
    assert!(flat.contains("平滑准入 / 每秒上限=关闭 / 30/秒"), "{flat}");
    assert!(
        flat.contains("短队列等待 / 容量=500 毫秒 / 12 条"),
        "{flat}"
    );
    assert!(flat.contains("拥挤缩小=关闭"), "{flat}");
    // Unbounded/automatic values are described, never shown as a raw zero limit.
    let defaults = config_rows(&Settings::default())
        .iter()
        .map(|row| row.join("="))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(defaults.contains("固定弹幕同屏上限=6 条"), "{defaults}");
    assert!(
        defaults.contains("平滑准入 / 每秒上限=开启 / 自动"),
        "{defaults}"
    );
    assert!(defaults.contains("拥挤缩小=开启（85%）"), "{defaults}");
}

#[test]
fn only_a_successful_douyin_web_assist_requests_a_search_refresh() {
    use live_hub_rust::gui::web_assist_refresh_allowed;
    assert!(web_assist_refresh_allowed(true, false, Platform::Douyin));
    assert!(!web_assist_refresh_allowed(true, true, Platform::Douyin));
    assert!(!web_assist_refresh_allowed(false, false, Platform::Douyin));
    assert!(!web_assist_refresh_allowed(true, false, Platform::Huya));
}

#[test]
fn executable_path_validation_rejects_unsafe_drafts_before_saving() {
    use live_hub_rust::gui::file_dialog::{
        ToolKind, path_status, validate_tool_path, validate_tool_paths,
    };
    let directory = tempdir().unwrap();
    assert!(validate_tool_path(ToolKind::Mpv, "").unwrap().is_none());
    assert!(validate_tool_path(ToolKind::Mpv, "   ").unwrap().is_none());
    let relative = validate_tool_path(ToolKind::Mpv, "players/mpv.exe").unwrap_err();
    assert!(relative.to_string().contains("绝对路径"), "{relative}");
    let missing = directory.path().join("missing.exe");
    let error = validate_tool_path(ToolKind::YtDlp, missing.to_str().unwrap()).unwrap_err();
    assert!(error.to_string().contains("不存在"), "{error}");
    let folder = directory.path().join("a-folder");
    std::fs::create_dir_all(&folder).unwrap();
    let error = validate_tool_path(ToolKind::Mpv, folder.to_str().unwrap()).unwrap_err();
    assert!(error.to_string().contains("不是普通文件"), "{error}");
    let tiny = directory.path().join("tiny.exe");
    std::fs::write(&tiny, vec![0u8; 100]).unwrap();
    let error = validate_tool_path(ToolKind::PotPlayer, tiny.to_str().unwrap()).unwrap_err();
    assert!(error.to_string().contains("过小"), "{error}");
    let valid = directory.path().join("mpv.exe");
    std::fs::write(&valid, vec![0x4d_u8; 2048]).unwrap();
    assert_eq!(
        validate_tool_path(ToolKind::Mpv, valid.to_str().unwrap())
            .unwrap()
            .unwrap(),
        valid
    );
    // One broken draft rejects the whole save; empty drafts remain allowed.
    assert!(validate_tool_paths("relative.exe", "", "").is_err());
    assert!(validate_tool_paths("", valid.to_str().unwrap(), "").is_ok());
    assert!(path_status(ToolKind::Mpv, "").is_none());
    assert!(!path_status(ToolKind::Mpv, "relative.exe").unwrap().0);
    assert!(
        path_status(ToolKind::Mpv, valid.to_str().unwrap())
            .unwrap()
            .0
    );
}

#[test]
fn player_auto_detection_uses_injected_directories_only() {
    use live_hub_rust::gui::file_dialog::{ToolKind, detect_tool_in, tool_candidates};
    let directory = tempdir().unwrap();
    let app = directory.path().join("app");
    let program_files = directory.path().join("PF");
    let bin = app.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(program_files.join("mpv")).unwrap();
    let found = bin.join("mpv.exe");
    std::fs::write(&found, vec![0x4d_u8; 2048]).unwrap();
    std::fs::write(
        program_files.join("mpv").join("mpv.exe"),
        vec![0x4d_u8; 2048],
    )
    .unwrap();
    let candidates = tool_candidates(
        ToolKind::Mpv,
        &app,
        &[directory.path().join("path-dir")],
        &[program_files.clone()],
        &[],
    );
    assert_eq!(detect_tool_in(ToolKind::Mpv, &candidates).unwrap(), found);
    assert!(detect_tool_in(ToolKind::YtDlp, &candidates).is_none());
    // Every candidate comes from an injected directory; no reference-project or
    // host-specific location is appended by the pure builder.
    assert!(
        candidates
            .iter()
            .all(|path| path.starts_with(directory.path()))
    );
    let potplayer = tool_candidates(
        ToolKind::PotPlayer,
        &app,
        &[],
        &[program_files.clone()],
        &[],
    );
    assert!(
        potplayer.iter().any(|path| path
            .to_string_lossy()
            .to_lowercase()
            .replace('\\', "/")
            .ends_with("daum/potplayer")),
        "{potplayer:?}"
    );
    let ytdlp = tool_candidates(ToolKind::YtDlp, &app, &[], &[program_files], &[]);
    assert!(ytdlp.iter().any(|path| {
        path.to_string_lossy()
            .to_lowercase()
            .replace('\\', "/")
            .ends_with("yt-dlp")
    }));
}

#[test]
fn native_dialog_filter_is_double_null_terminated_utf16() {
    use live_hub_rust::gui::file_dialog::{ToolKind, build_filter, tool_filter};
    let filter = build_filter(&[("合成 (*.exe)", "*.exe")]);
    assert_eq!(
        filter.iter().rev().take(2).copied().collect::<Vec<_>>(),
        vec![0, 0]
    );
    assert!(String::from_utf16_lossy(&filter).contains("合成 (*.exe)"));
    for tool in [ToolKind::Mpv, ToolKind::PotPlayer, ToolKind::YtDlp] {
        let filter = tool_filter(tool);
        assert!(filter.len() > 4);
        assert_eq!(
            filter.iter().rev().take(2).copied().collect::<Vec<_>>(),
            vec![0, 0]
        );
    }
}

#[cfg(windows)]
#[test]
fn win32_dialog_layout_is_the_size_comdlg32_expects() {
    use live_hub_rust::gui::file_dialog::open_file_name_size;
    assert_eq!(open_file_name_size(), 152);
}
