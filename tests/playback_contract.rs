//! Offline contract tests for the playback launch surface.
//!
//! These tests never start a real mpv/PotPlayer, never touch real user data and
//! never open a network connection: they only inspect planning/spawning errors.
use live_hub_rust::core::{Player, ResolvedStream, Settings};
use live_hub_rust::playback::{self, LaunchOutcome, PlaybackPlan};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[test]
fn bili_flv_recovery_uses_relay_but_explicit_direct_or_hls_remain_direct() {
    use live_hub_rust::core::{PlaybackPreferences, Target};
    let target = Target::parse("bili:12345").unwrap();
    let mut stream = ResolvedStream {
        url: "https://cdn.bilivideo.com/live.flv".into(),
        title: "synthetic".into(),
        headers: Default::default(),
    };
    let mut preferences = PlaybackPreferences::default();
    assert!(playback::uses_relay(&target, &stream, &preferences));
    preferences.relay_bili = false;
    assert!(!playback::uses_relay(&target, &stream, &preferences));
    preferences.relay_bili = true;
    stream.url = "https://cdn.bilivideo.com/live/index.m3u8".into();
    assert!(!playback::uses_relay(&target, &stream, &preferences));
}

#[test]
fn launch_outcome_keeps_the_gui_contract_shape() {
    // The GUI consumes exactly these two fields; adding or renaming one is a
    // breaking contract change that requires the GUI to be updated as well.
    let outcome = LaunchOutcome {
        title: "Synthetic 直播间".into(),
        player_pid: 4242,
    };
    let json = serde_json::to_value(&outcome).unwrap();
    let object = json.as_object().unwrap();
    assert_eq!(object.len(), 2);
    assert_eq!(json["title"], "Synthetic 直播间");
    assert_eq!(json["player_pid"], 4242);
}

#[test]
fn explicit_player_paths_are_used_and_missing_paths_fail_actionably() {
    let directory = tempfile::tempdir().unwrap();
    let player = directory.path().join("synthetic-mpv.exe");
    std::fs::write(&player, b"synthetic placeholder").unwrap();

    let settings = Settings {
        mpv: Some(player.clone()),
        ..Settings::default()
    };
    assert_eq!(
        playback::find_player(Player::Mpv, &settings).unwrap(),
        player
    );

    let settings = Settings {
        mpv: Some(directory.path().join("missing-mpv.exe")),
        ..Settings::default()
    };
    let error = playback::find_player(Player::Mpv, &settings).unwrap_err();
    assert!(
        format!("{error:#}").contains("播放器路径不存在"),
        "{error:#}"
    );

    let settings = Settings {
        potplayer: Some(player.clone()),
        ..Settings::default()
    };
    assert_eq!(
        playback::find_player(Player::PotPlayer, &settings).unwrap(),
        player
    );
    let settings = Settings {
        potplayer: Some(directory.path().join("missing-potplayer.exe")),
        ..Settings::default()
    };
    assert!(playback::find_player(Player::PotPlayer, &settings).is_err());
}

#[test]
fn spawning_a_missing_player_is_an_actionable_error() {
    let directory = tempfile::tempdir().unwrap();
    let plan = PlaybackPlan {
        player: Player::Mpv,
        executable: directory.path().join("missing").join("mpv.exe"),
        arguments: Vec::new(),
    };
    let error = playback::spawn_plan(&plan).unwrap_err();
    assert!(format!("{error:#}").contains("无法启动播放器"), "{error:#}");
}

#[test]
fn potplayer_plan_passes_url_title_and_new_window_as_separate_arguments() {
    let stream = ResolvedStream {
        url: "https://media.example.test/live.flv?a=1&b=2".into(),
        title: "主播 \"标题\" & 测试\n尾部".into(),
        headers: BTreeMap::new(),
    };
    let plan = playback::plan(
        Player::PotPlayer,
        PathBuf::from("PotPlayerMini64.exe"),
        &stream,
    )
    .unwrap();
    let arguments: Vec<String> = plan
        .arguments
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        arguments,
        vec![
            "/new".to_owned(),
            "https://media.example.test/live.flv?a=1&b=2".to_owned(),
            "/title=主播 \"标题\" & 测试尾部".to_owned(),
        ]
    );
}

#[test]
fn user_agent_and_referer_headers_map_to_mpv_options_only() {
    let stream = ResolvedStream {
        url: "https://media.example.test/live.flv".into(),
        headers: BTreeMap::from([
            ("User-Agent".to_owned(), "Synthetic-UA".to_owned()),
            ("Referer".to_owned(), "https://example.invalid/".to_owned()),
            ("X-Synthetic".to_owned(), "value".to_owned()),
        ]),
        title: "Synthetic".into(),
    };
    let plan = playback::plan(Player::Mpv, PathBuf::from("mpv.exe"), &stream).unwrap();
    let arguments: Vec<String> = plan
        .arguments
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    assert!(arguments.contains(&"--user-agent=Synthetic-UA".to_owned()));
    assert!(arguments.contains(&"--referrer=https://example.invalid/".to_owned()));
    assert!(arguments.contains(&"--http-header-fields-append=X-Synthetic: value".to_owned()));
}
