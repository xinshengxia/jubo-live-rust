//! Offline contract tests for the mpv title/OSD hint surface (parity PB-01).
//!
//! These tests never start a player, never touch a real runtime directory and
//! never open a network connection: they inspect the UTF-8 hint files
//! live-title.lua reads and feed synthetic relay/room events through the same
//! observer the playback supervisor uses.

use live_hub_rust::core::Platform;
use live_hub_rust::playback::{
    self, BitrateEstimator, HintSpec, PlaybackEvent, RelayTelemetry, SessionHints,
};
use live_hub_rust::room_state::{RoomObservation, RoomState};
use std::path::Path;
use std::time::{Duration, Instant};

fn prepare(
    root: &Path,
    platform: Platform,
    room: &str,
    base: &str,
    media: &str,
    relay: bool,
    bitrate_kbps: Option<u32>,
) -> SessionHints {
    SessionHints::prepare_in(
        root,
        HintSpec {
            platform,
            room,
            base,
            media,
            relay,
            bitrate_kbps,
            room_observation: None,
        },
    )
    .expect("hint files")
}

fn telemetry(state: &str, forwarded_bytes: u64) -> RelayTelemetry {
    RelayTelemetry {
        state: state.to_owned(),
        forwarded_bytes,
        ..Default::default()
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).expect("hint file")
}

#[test]
fn per_platform_base_and_media_hints_match_the_legacy_launcher() {
    for platform in [Platform::Bili, Platform::Huya, Platform::Douyu] {
        let texts = playback::title_hint_texts(platform, "主播A - 今天的直播间");
        assert_eq!(texts.base, "主播A - 今天的直播间", "{platform}");
        assert_eq!(texts.media, "今天的直播间", "{platform}");
    }
    // Douyin resolves the room title only, like the legacy fallback label.
    let texts = playback::title_hint_texts(Platform::Douyin, "只有一个房间标题");
    assert_eq!(texts.base, "只有一个房间标题");
    assert_eq!(texts.media, "只有一个房间标题");
    // No separator (generic fallback) keeps both hints equal.
    let texts = playback::title_hint_texts(Platform::Bili, "B站 · 7777");
    assert_eq!(texts.base, texts.media);
    // Control characters never reach the single-line hint files.
    let texts = playback::title_hint_texts(Platform::Huya, "标\u{0}题\n第二行");
    assert!(!texts.base.contains('\n'));
    assert!(!texts.media.contains('\u{0}'));
    // Empty titles still produce something readable.
    assert_eq!(playback::title_hint_texts(Platform::Douyu, "   ").base, "直播");
}

#[test]
fn session_hints_write_legacy_file_names_and_script_opts() {
    let directory = tempfile::tempdir().unwrap();
    let hints = prepare(
        directory.path(),
        Platform::Bili,
        "7777",
        "主播A - 标题",
        "标题",
        true,
        Some(4000),
    );
    let paths = hints.paths();
    let file_name = |path: &Path| path.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(file_name(&paths.base), "bili-7777.anchor.txt");
    assert!(file_name(&paths.media).starts_with("session-"));
    assert!(file_name(&paths.media).ends_with(".media-title.txt"));
    assert_eq!(file_name(&paths.bitrate), "bili-7777.bitrate.txt");
    assert!(file_name(&paths.state).starts_with("session-"));
    assert!(file_name(&paths.state).ends_with(".state.txt"));
    let relay = paths.relay.as_ref().expect("relay hint path");
    assert!(file_name(relay).ends_with(".relay.txt"));
    assert_eq!(read(&paths.base), "主播A - 标题");
    assert_eq!(read(&paths.media), "标题");
    assert_eq!(read(&paths.bitrate), "4000");
    assert_eq!(read(relay), "");

    let options: Vec<String> = hints
        .script_opts(Path::new("C:/pkg/assets/lua/live-title.lua"), None)
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    for key in [
        "live_title_base_file=",
        "live_title_media_file=",
        "live_title_state_file=",
        "live_title_relay_file=",
        "live_title_bitrate_file=",
    ] {
        assert!(
            options
                .iter()
                .any(|option| option.starts_with(&format!("--script-opts-append={key}"))),
            "missing {key}: {options:?}"
        );
    }
    assert!(
        options
            .iter()
            .any(|option| option == "--script-opts-append=live_title_autoreplay=yes"),
        "relay platforms must carry the autoreplay OSD wording"
    );
}

#[test]
fn direct_playback_has_no_relay_and_no_autoreplay_hints() {
    let directory = tempfile::tempdir().unwrap();
    let hints = prepare(directory.path(), Platform::Bili, "7777", "主播A - 标题", "标题", false, None);
    assert!(hints.paths().relay.is_none());
    let options: Vec<String> = hints
        .script_opts(Path::new("C:/pkg/assets/lua/live-title.lua"), Some(Path::new("C:/pkg/assets/lua/live-resync.lua")))
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    assert!(!options.iter().any(|option| option.contains("live_title_relay_file")));
    assert!(!options.iter().any(|option| option.contains("autoreplay")));
    assert!(options.iter().any(|option| option == "--script=C:/pkg/assets/lua/live-resync.lua"));
}

#[test]
fn room_state_hint_maps_platform_replay_kinds() {
    assert_eq!(playback::room_state_hint_text(Platform::Bili, RoomState::Live), "LIVE");
    assert_eq!(playback::room_state_hint_text(Platform::Douyu, RoomState::Offline), "OFFLINE");
    // Bili replay is a loop, Douyu replay is a recording, the rest are replay feeds.
    assert_eq!(playback::room_state_hint_text(Platform::Bili, RoomState::Replay), "LOOP");
    assert_eq!(playback::room_state_hint_text(Platform::Douyu, RoomState::Replay), "RECORDING");
    assert_eq!(playback::room_state_hint_text(Platform::Huya, RoomState::Replay), "REPLAY");
    assert_eq!(playback::room_state_hint_text(Platform::Douyin, RoomState::Replay), "REPLAY");
    let unknown = playback::room_state_hint_text(Platform::Bili, RoomState::Unknown);
    let ticks = unknown
        .strip_prefix("UNKNOWN|")
        .expect("UNKNOWN|timestamp");
    assert!(ticks.parse::<i64>().unwrap() > 621_355_968_000_000_000);
}

#[test]
fn synthetic_relay_sequence_updates_relay_and_state_hints() {
    let directory = tempfile::tempdir().unwrap();
    let hints = prepare(directory.path(), Platform::Douyu, "8888", "主播 - 标题", "标题", true, None);
    let relay = hints.paths().relay.clone().unwrap();
    let state = hints.paths().state.clone();
    let start = Instant::now();
    let at = |seconds: u64| start + Duration::from_secs(seconds);
    let observe = |telemetry: RelayTelemetry, seconds: u64| {
        hints.observe_at(&PlaybackEvent::RelayStatistics { telemetry }, at(seconds));
    };

    observe(telemetry("connecting", 0), 0);
    assert_eq!(read(&relay), "RECONNECTING");

    observe(telemetry("streaming", 0), 2);
    assert_eq!(read(&relay), "ONLINE");

    // Confirmed offline: both hint files must say so, immediately.
    let mut offline = telemetry("waiting", 0);
    offline.waiting_state = Some("offline".into());
    observe(offline, 4);
    assert_eq!(read(&relay), "OFFLINE");
    assert_eq!(read(&state), "OFFLINE");

    // Media for the same room starts flowing again: the room-state file may say
    // LIVE now (the relay confirmed the room is broadcasting).
    observe(telemetry("streaming", 0), 6);
    assert_eq!(read(&relay), "ONLINE");
    assert_eq!(read(&state), "LIVE");

    // A replay feed that cannot be resolved reports its own vocabularies.
    let mut replay = telemetry("waiting", 0);
    replay.waiting_state = Some("replay_unavailable".into());
    observe(replay, 8);
    assert_eq!(read(&relay), "REPLAY-UNAVAILABLE|重播流不可用");
    assert_eq!(read(&state), "RECORDING");

    // The replay feed working again does not turn the recording into a live room.
    observe(telemetry("streaming", 0), 10);
    assert_eq!(read(&relay), "ONLINE");
    assert_eq!(read(&state), "RECORDING");

    hints.observe_at(&PlaybackEvent::RelayExited { exit_code: Some(1) }, at(12));
    assert_eq!(read(&relay), "RECONNECTING");
}

#[test]
fn relay_metadata_bitrate_writes_the_expected_platform_hint() {
    let directory = tempfile::tempdir().unwrap();
    let hints = prepare(directory.path(), Platform::Douyin, "9999", "标题", "标题", true, None);
    let bitrate = hints.paths().bitrate.clone();
    let mut sample = telemetry("streaming", 1_000_000);
    sample.stream_bitrate_kbps = Some(2500);
    hints.observe_at(&PlaybackEvent::RelayStatistics { telemetry: sample }, Instant::now());
    assert_eq!(read(&bitrate), "2500");
    let mut update = telemetry("streaming", 2_000_000);
    update.stream_bitrate_kbps = Some(2600);
    hints.observe_at(&PlaybackEvent::RelayStatistics { telemetry: update }, Instant::now());
    assert_eq!(read(&bitrate), "2600");
}

#[test]
fn douyu_nominal_bitrate_is_kept_until_exact_metadata_arrives() {
    let directory = tempfile::tempdir().unwrap();
    let hints = prepare(
        directory.path(),
        Platform::Douyu,
        "8888",
        "主播 - 标题",
        "标题",
        true,
        Some(8000),
    );
    let bitrate = hints.paths().bitrate.clone();
    assert_eq!(read(&bitrate), "8000");
    // A measured sample must not replace the platform's nominal value.
    hints.observe_at(
        &PlaybackEvent::RelayStatistics {
            telemetry: telemetry("streaming", 2_000_000),
        },
        Instant::now(),
    );
    hints.observe_at(
        &PlaybackEvent::RelayStatistics {
            telemetry: telemetry("streaming", 4_000_000),
        },
        Instant::now() + Duration::from_secs(2),
    );
    assert_eq!(read(&bitrate), "8000");
    // Exact onMetaData metadata wins over the nominal value.
    let mut metadata = telemetry("streaming", 4_000_000);
    metadata.stream_bitrate_kbps = Some(8100);
    hints.observe_at(
        &PlaybackEvent::RelayStatistics { telemetry: metadata },
        Instant::now() + Duration::from_secs(4),
    );
    assert_eq!(read(&bitrate), "8100");
}

#[test]
fn measured_bitrate_fallback_is_smoothed_with_a_display_step() {
    let mut estimator = BitrateEstimator::default();
    assert_eq!(estimator.observe(0, Duration::ZERO), None);
    assert_eq!(
        estimator.observe(2_000_000, Duration::from_millis(500)),
        None,
        "below the measurement window"
    );
    // 2 MB in 2 s is ~8000 kbps.
    let first = estimator.observe(4_000_000, Duration::from_secs(2)).unwrap();
    assert!((7800..=8200).contains(&first), "{first}");
    // The same steady rate inside the hysteresis step is not republished.
    assert_eq!(estimator.observe(6_000_000, Duration::from_secs(2)), None);
    // A clearly faster rate is.
    let changed = estimator.observe(10_000_000, Duration::from_secs(2)).unwrap();
    assert!(changed > first + 500, "{changed}");
}

#[test]
fn room_observations_update_state_and_media_hints() {
    let directory = tempfile::tempdir().unwrap();
    let observation = RoomObservation {
        state: RoomState::Offline,
        title: Some("服务器维护中".into()),
        note: "未开播".into(),
    };
    let hints = SessionHints::prepare_in(
        directory.path(),
        HintSpec {
            platform: Platform::Bili,
            room: "7777",
            base: "主播 - 旧标题",
            media: "旧标题",
            relay: true,
            bitrate_kbps: None,
            room_observation: Some(&observation),
        },
    )
    .unwrap();
    assert_eq!(read(&hints.paths().state), "OFFLINE");
    assert_eq!(read(&hints.paths().media), "服务器维护中");
    hints.observe_room(&RoomObservation {
        state: RoomState::Live,
        title: Some("新标题".into()),
        note: "直播中".into(),
    });
    assert_eq!(read(&hints.paths().state), "LIVE");
    assert_eq!(read(&hints.paths().media), "新标题");
    hints.observe_at(
        &PlaybackEvent::RoomObserved {
            state: RoomState::Replay,
            title: Some("重播标题".into()),
            note: "重播中".into(),
        },
        Instant::now(),
    );
    assert_eq!(read(&hints.paths().state), "LOOP");
    assert_eq!(read(&hints.paths().media), "重播标题");
}

#[test]
fn a_new_session_never_inherits_previous_hints() {
    let directory = tempfile::tempdir().unwrap();
    let first = prepare(directory.path(), Platform::Douyu, "8888", "主播 - 标题", "标题", true, None);
    let first_directory = first.paths().directory.clone();
    let relay = first.paths().relay.clone().unwrap();
    let mut offline = telemetry("waiting", 0);
    offline.waiting_state = Some("offline".into());
    first.observe_at(&PlaybackEvent::RelayStatistics { telemetry: offline }, Instant::now());
    assert_eq!(read(&relay), "OFFLINE");
    drop(first);
    assert!(!first_directory.exists(), "session directory must be removed");

    let second = prepare(directory.path(), Platform::Douyu, "8888", "主播 - 新标题", "新标题", true, None);
    assert_ne!(second.paths().directory, first_directory);
    assert_eq!(read(second.paths().relay.as_ref().unwrap()), "");
    assert!(!second.paths().state.exists(), "no stale room state");
    assert_eq!(read(&second.paths().base), "主播 - 新标题");
}

#[test]
fn stale_hint_directories_are_pruned() {
    let directory = tempfile::tempdir().unwrap();
    for name in ["old-a", "old-b"] {
        let path = directory.path().join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("stale.txt"), b"stale").unwrap();
    }
    // Everything is older than zero, so the stale directories go.
    playback::prune_stale_hint_directories(directory.path(), Duration::ZERO);
    assert!(!directory.path().join("old-a").exists());
    assert!(!directory.path().join("old-b").exists());
    // A live session's directory is old enough to survive the production maximum.
    let fresh = prepare(directory.path(), Platform::Bili, "1", "标题", "标题", false, None);
    let fresh_directory = fresh.paths().directory.clone();
    playback::prune_stale_hint_directories(directory.path(), Duration::from_secs(3600));
    assert!(fresh_directory.exists());
}
