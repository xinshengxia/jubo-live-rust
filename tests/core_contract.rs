use live_hub_rust::core::{HISTORY_LIMIT, Platform, Quality, Settings, Store, Target};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Barrier};
use tempfile::tempdir;

fn target(value: &str) -> Target {
    Target::parse(value).unwrap()
}

#[test]
fn official_urls_and_prefixed_rooms_have_the_same_identity() {
    let cases = [
        ("https://live.bilibili.com/123?from=synthetic", "bili:123"),
        ("https://live.bilibili.com/123/", "bili:123"),
        ("https://live.bilibili.com/123/extra", "bili:123"),
        (
            "https://live.bilibili.com/blanc/7777",
            "bili:7777",
        ),
        ("https://live.bilibili.com/h5/7777", "bili:7777"),
        (
            "https://www.huya.com/example_room-1/",
            "huya:example_room-1",
        ),
        ("https://www.huya.com/910323/extra", "huya:910323"),
        ("http://www.douyu.com/example123", "douyu:example123"),
        ("https://m.douyu.com/123456", "douyu:123456"),
        ("https://www.douyin.com/987654", "douyin:987654"),
        ("https://live.douyin.com/987654?from=x", "douyin:987654"),
    ];
    for (url, prefixed) in cases {
        assert_eq!(target(url), target(prefixed));
        assert_eq!(target(prefixed).quality, Quality::Best);
    }
    assert_eq!(target(" 123 "), target("bili:123"));
    assert_eq!("HUYA".parse::<Platform>().unwrap(), Platform::Huya);
}

#[test]
fn target_rejects_spoofed_hosts_paths_credentials_and_invalid_rooms() {
    for input in [
        "",
        "bili:",
        "bili:abc",
        "douyin:-123",
        "huya:../room",
        "douyu:abc/def",
        "huya:space room",
        "bili:１２３",
        "unknown:123",
        "file:///123",
        "https://live.bilibili.com.example.invalid/123",
        "https://example.invalid/live.bilibili.com/123",
        "https://user:secret@live.bilibili.com/123",
        "https://live.bilibili.com:1234/123",
        "https://live.bilibili.com/",
        "https://live.bilibili.com/blanc",
        "https://live.bilibili.com/blanc/",
        "https://m.douyu.com/",
        "https://www.douyin.com/not-a-room",
        "https://live.douyin.com/%31%32%33",
    ] {
        assert!(
            Target::parse(input).is_err(),
            "accepted invalid target: {input}"
        );
    }
    assert!(Target::parse(&format!("huya:{}", "x".repeat(129))).is_err());
    assert!(serde_json::from_str::<Target>(r#"{"platform":"huya","room":"../escape"}"#).is_err());
}

#[test]
fn settings_serde_defaults_and_round_trip_do_not_discover_legacy_config() {
    let settings: Settings = serde_json::from_str("{}").unwrap();
    assert_eq!(settings, Settings::default());
    let settings = Settings {
        playback: Default::default(),
        mpv: Some(PathBuf::from("players/mpv.exe")),
        potplayer: None,
        yt_dlp: Some(PathBuf::from("tools/yt-dlp.exe")),
        legacy_exe: None,
        ..Settings::default()
    };
    assert_eq!(
        serde_json::from_str::<Settings>(&serde_json::to_string(&settings).unwrap()).unwrap(),
        settings
    );
    let directory = tempdir().unwrap();
    let legacy_root = directory.path().join("legacy");
    fs::create_dir(&legacy_root).unwrap();
    fs::write(
        legacy_root.join("live-hub.json"),
        r#"{"mpv":"synthetic-legacy.exe"}"#,
    )
    .unwrap();
    fs::write(
        legacy_root.join("favorites.json"),
        r#"{"Synthetic legacy":"bili:1"}"#,
    )
    .unwrap();
    let store = Store::open(directory.path().join("new-store")).unwrap();
    assert_eq!(store.settings(), &Settings::default());
    assert!(store.favorites().is_empty());
    assert!(store.history().is_empty());
    assert!(!store.root().join("state.json").exists());
}

#[test]
fn favorite_identity_ignores_quality_and_isolated_stores_remain_independent() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("first");
    let mut first = Store::open(root.clone()).unwrap();
    first
        .add_favorite(" Original ".into(), target("huya:example"))
        .unwrap();
    let mut updated = target("huya:example");
    updated.quality = Quality::Low;
    first
        .add_favorite("Updated".into(), updated.clone())
        .unwrap();
    first
        .add_favorite("Other platform".into(), target("douyu:example"))
        .unwrap();
    assert_eq!(first.favorites().len(), 2);
    assert_eq!(first.favorites()[0].name, "Updated");
    assert_eq!(first.favorites()[0].target, updated);
    let mut reloaded = Store::open(root).unwrap();
    assert_eq!(reloaded.favorites(), first.favorites());
    reloaded.remove_favorite(Platform::Huya, "example").unwrap();
    assert_eq!(reloaded.favorites().len(), 1);
    assert_eq!(reloaded.favorites()[0].target.platform, Platform::Douyu);
    assert!(
        Store::open(directory.path().join("second"))
            .unwrap()
            .favorites()
            .is_empty()
    );
}

#[test]
fn history_is_recent_first_unique_and_bounded_after_reopening() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("store");
    let mut store = Store::open(root.clone()).unwrap();
    for room in 1..=(HISTORY_LIMIT + 5) {
        store
            .record_play(
                format!("Synthetic room {room}"),
                target(&format!("bili:{room}")),
            )
            .unwrap();
    }
    assert_eq!(store.history().len(), HISTORY_LIMIT);
    assert_eq!(
        store.history()[0].target.room,
        (HISTORY_LIMIT + 5).to_string()
    );
    assert_eq!(store.history().last().unwrap().target.room, "6");
    store
        .record_play("Replayed".into(), target("bili:6"))
        .unwrap();
    assert_eq!(store.history().len(), HISTORY_LIMIT);
    assert_eq!(store.history()[0].target.room, "6");
    assert_eq!(store.history()[0].name, "Replayed");
    assert!(store.history()[0].played_at.is_some());
    assert_eq!(Store::open(root).unwrap().history(), store.history());
}

#[test]
fn stale_instances_merge_individual_mutations_instead_of_losing_updates() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("store");
    let mut first = Store::open(root.clone()).unwrap();
    let mut stale = Store::open(root.clone()).unwrap();
    first
        .add_favorite("First".into(), target("bili:1"))
        .unwrap();
    stale
        .add_favorite("Second".into(), target("bili:2"))
        .unwrap();
    first
        .record_play("Played".into(), target("bili:3"))
        .unwrap();
    stale
        .save_settings(Settings {
            mpv: Some(PathBuf::from("players/mpv.exe")),
            ..Settings::default()
        })
        .unwrap();
    let reopened = Store::open(root).unwrap();
    assert_eq!(reopened.favorites().len(), 2);
    assert_eq!(reopened.history().len(), 1);
    assert_eq!(
        reopened.settings().mpv,
        Some(PathBuf::from("players/mpv.exe"))
    );
}

#[test]
fn concurrent_store_writers_preserve_all_distinct_favorites() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("store");
    let barrier = Arc::new(Barrier::new(4));
    let writers: Vec<_> = (0..4)
        .map(|writer| {
            let root = root.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut store = Store::open(root).unwrap();
                barrier.wait();
                for index in 0..10 {
                    let room = writer * 10 + index + 1;
                    store
                        .add_favorite(format!("Synthetic {room}"), target(&format!("bili:{room}")))
                        .unwrap();
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    assert_eq!(Store::open(root).unwrap().favorites().len(), 40);
}

#[test]
fn invalid_requests_preserve_both_the_committed_file_and_memory_snapshot() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("store");
    let mut store = Store::open(root.clone()).unwrap();
    store
        .add_favorite("Existing".into(), target("bili:1"))
        .unwrap();
    let original_bytes = fs::read(root.join("state.json")).unwrap();
    let original_favorites = store.favorites().to_vec();
    assert!(store.add_favorite("".into(), target("bili:2")).is_err());
    assert!(
        store
            .add_favorite(
                "Invalid".into(),
                Target {
                    platform: Platform::Bili,
                    room: "../escape".into(),
                    quality: Quality::Best
                }
            )
            .is_err()
    );
    assert!(
        store
            .save_settings(Settings {
                mpv: Some(PathBuf::new()),
                ..Settings::default()
            })
            .is_err()
    );
    assert_eq!(fs::read(root.join("state.json")).unwrap(), original_bytes);
    assert_eq!(store.favorites(), original_favorites);
}

#[test]
fn corrupt_or_future_state_is_never_replaced_by_a_default_snapshot() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("store");
    let mut store = Store::open(root.clone()).unwrap();
    store
        .add_favorite("Existing".into(), target("bili:1"))
        .unwrap();
    for broken in [
        "{broken",
        r#"{"schema_version":999,"favorites":[],"history":[]}"#,
    ] {
        fs::write(root.join("state.json"), broken).unwrap();
        assert!(Store::open(root.clone()).is_err());
        assert!(store.add_favorite("New".into(), target("bili:2")).is_err());
        assert_eq!(fs::read_to_string(root.join("state.json")).unwrap(), broken);
        assert_eq!(store.favorites().len(), 1);
        assert_eq!(store.favorites()[0].name, "Existing");
    }
}

#[test]
fn explicit_legacy_import_is_transactional_and_preserves_unrelated_files() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("store");
    let mut store = Store::open(root.clone()).unwrap();
    let unrelated = root.join("unrelated.txt");
    fs::write(&unrelated, "synthetic marker").unwrap();
    let result = store
        .import_json(
            r#"[{"name":"First","room":"huya:example"},{"name":"Updated","room":"huya:example"}]"#,
        )
        .unwrap();
    assert_eq!(result.favorites_added, 1);
    assert_eq!(result.favorites_updated, 1);
    assert_eq!(store.favorites().len(), 1);
    assert_eq!(store.favorites()[0].name, "Updated");
    let committed = fs::read(root.join("state.json")).unwrap();
    assert!(
        store
            .import_json(
                r#"[{"name":"Valid","room":"bili:123"},{"name":"Invalid","room":"huya:../escape"}]"#
            )
            .is_err()
    );
    assert!(
        store
            .import_json(r#"{"settings":{"mpv":"unexpected.exe"}}"#)
            .is_err()
    );
    assert_eq!(fs::read(root.join("state.json")).unwrap(), committed);
    assert_eq!(fs::read_to_string(unrelated).unwrap(), "synthetic marker");
    store.import_json(r#"{"Synthetic map":"bili:42"}"#).unwrap();
    assert_eq!(store.favorites().len(), 2);
}

#[test]
fn structured_and_legacy_history_imports_do_not_invent_dates_or_displace_recent_playback() {
    let directory = tempdir().unwrap();
    let mut store = Store::open(directory.path().join("store")).unwrap();
    store
        .record_play("Current".into(), target("bili:1"))
        .unwrap();
    let result = store.import_json(r#"{"history":[{"name":"Duplicate","target":{"platform":"bili","room":"1"}},{"name":"Older","target":{"platform":"huya","room":"example"}}]}"#).unwrap();
    assert_eq!(result.history_added, 1);
    assert_eq!(result.history_skipped, 1);
    assert_eq!(store.history()[0].name, "Current");
    assert!(store.history()[1].played_at.is_none());
    store
        .import_legacy_history_json(
            r#"{"douyu":[{"Room":"example-2","Name":"Synthetic old room","At":"10-01 12:30"}]}"#,
        )
        .unwrap();
    let imported = &store.history()[2];
    assert_eq!(imported.target, target("douyu:example-2"));
    assert!(imported.played_at.is_none());
    assert_eq!(imported.legacy_time.as_deref(), Some("10-01 12:30"));
    assert!(
        store
            .import_legacy_history_json(r#"{"huya":[{"Room":"bili:2","Name":"Wrong platform"}]}"#)
            .is_err()
    );
}

#[cfg(windows)]
#[test]
fn failed_atomic_replacement_keeps_the_original_and_removes_temporary_files() {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    let directory = tempdir().unwrap();
    let root = directory.path().join("store");
    let mut store = Store::open(root.clone()).unwrap();
    store
        .add_favorite("Existing".into(), target("bili:1"))
        .unwrap();
    let original = fs::read(root.join("state.json")).unwrap();
    // Allow reads so the transaction reload succeeds, but deny deletion and
    // replacement to exercise failure after a temporary file has been flushed.
    let held = OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(root.join("state.json"))
        .unwrap();
    assert!(
        store
            .add_favorite("Blocked".into(), target("bili:2"))
            .is_err()
    );
    assert_eq!(store.favorites().len(), 1);
    assert_eq!(fs::read(root.join("state.json")).unwrap(), original);
    assert!(!fs::read_dir(&root).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".state-")
    }));
    drop(held);
    store
        .add_favorite("Recovered".into(), target("bili:2"))
        .unwrap();
    assert_eq!(Store::open(root).unwrap().favorites().len(), 2);
}

#[cfg(unix)]
#[test]
fn symbolic_store_state_is_rejected_without_touching_its_target() {
    use std::os::unix::fs::symlink;
    let directory = tempdir().unwrap();
    let outside = directory.path().join("outside.json");
    fs::write(&outside, "synthetic outside marker").unwrap();
    let root = directory.path().join("store");
    fs::create_dir(&root).unwrap();
    symlink(&outside, root.join("state.json")).unwrap();
    assert!(Store::open(root).is_err());
    assert_eq!(
        fs::read_to_string(outside).unwrap(),
        "synthetic outside marker"
    );
}
