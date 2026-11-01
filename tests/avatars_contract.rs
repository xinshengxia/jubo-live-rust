//! Offline contract tests for the favorites-card avatar service (FAV-04).
//!
//! Every test uses an injected transport: no network, no real platform account
//! and no user data are touched. The tests cover URL parsing/normalization,
//! platform payload extraction, disk/memory caching across restarts, the failure
//! retry window, bounded concurrency and corrupt-cache degradation.

use live_hub_rust::avatars::{
    AVATAR_EDGE, AvatarEntry, AvatarFailure, AvatarLimits, AvatarService, AvatarTransport,
    AvatarUpdate, avatar_key,
};
use live_hub_rust::core::Platform;
use live_hub_rust::platforms::{
    avatar_thumbnail_url, bili_avatar_from_info, douyu_avatar_from_json, douyin_avatar_from_page,
    huya_avatar_from_json, normalize_avatar_url,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

fn png_bytes(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(width, height, image::Rgba([9, 20, 30, 255]));
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

#[test]
fn avatar_urls_are_normalized_and_restricted_to_the_platform_cdn_allowlist() {
    assert_eq!(
        normalize_avatar_url("//i0.hdslb.com/bfs/face/a.jpg").as_deref(),
        Some("https://i0.hdslb.com/bfs/face/a.jpg")
    );
    assert_eq!(
        normalize_avatar_url("http://i0.hdslb.com/bfs/face/a.jpg").as_deref(),
        Some("https://i0.hdslb.com/bfs/face/a.jpg")
    );
    assert_eq!(
        normalize_avatar_url("https://p3-pc.douyinpic.com/aweme/a.jpeg").as_deref(),
        Some("https://p3-pc.douyinpic.com/aweme/a.jpeg")
    );
    for rejected in [
        "",
        "https://example.invalid/avatar.png",
        "https://evil.example/hdslb.com/avatar.png",
        "https://user:secret@i0.hdslb.com/a.jpg",
        "https://i0.hdslb.com:8443/a.jpg",
        "javascript:alert(1)",
        "file:///C:/avatar.png",
    ] {
        assert!(
            normalize_avatar_url(rejected).is_none(),
            "accepted {rejected}"
        );
    }
    assert_eq!(
        avatar_thumbnail_url("https://i0.hdslb.com/bfs/face/a.jpg"),
        "https://i0.hdslb.com/bfs/face/a.jpg@96w_96h_1c.png"
    );
    // An already-thumbnailed URL is not nested a second time.
    assert_eq!(
        avatar_thumbnail_url("https://i0.hdslb.com/bfs/face/a.jpg@96w_96h_1c.png"),
        "https://i0.hdslb.com/bfs/face/a.jpg@96w_96h_1c.png"
    );
    assert_eq!(
        avatar_thumbnail_url("https://apic.douyucdn.cn/upload/avatar/a.jpg"),
        "https://apic.douyucdn.cn/upload/avatar/a.jpg"
    );
}

#[test]
fn platform_payloads_yield_the_anchor_avatar() {
    let bili = json!({
        "code": 0,
        "data": {
            "anchor_info": {"base_info": {"face": "https://i0.hdslb.com/bfs/face/a.jpg"}}
        }
    });
    assert_eq!(
        bili_avatar_from_info(&bili).as_deref(),
        Some("https://i0.hdslb.com/bfs/face/a.jpg")
    );
    assert!(bili_avatar_from_info(&json!({"code": 0, "data": {}})).is_none());

    let huya = json!({
        "status": 200,
        "data": {"nick": "主播", "avatar180": "//alpic.huya.com/avatar/a.png"}
    });
    assert_eq!(
        huya_avatar_from_json(&huya).as_deref(),
        Some("//alpic.huya.com/avatar/a.png")
    );

    let douyu = json!({
        "room": {"nickname": "主播", "owner_avatar": "https://apic.douyucdn.cn/a.jpg"}
    });
    assert_eq!(
        douyu_avatar_from_json(&douyu).as_deref(),
        Some("https://apic.douyucdn.cn/a.jpg")
    );
    assert!(douyu_avatar_from_json(&json!({"room": {"nickname": "主播"}})).is_none());

    // Douyin room page payload: the avatar lives in owner.avatar_thumb.url_list.
    let room = json!({
        "state": {
            "roomStore": {
                "roomInfo": {
                    "room": {
                        "status": 2,
                        "title": "标题",
                        "owner": {
                            "nickname": "主播",
                            "avatar_thumb": {"url_list": [
                                "https://p3-pc.douyinpic.com/aweme/a.jpeg"
                            ]}
                        }
                    }
                }
            }
        }
    });
    let payload = serde_json::to_string(&room).unwrap();
    let literal = serde_json::to_string(&payload).unwrap();
    let page = format!("<script>self.__pace_f.push([1,{literal}])</script>");
    assert_eq!(
        douyin_avatar_from_page(&page).as_deref(),
        Some("https://p3-pc.douyinpic.com/aweme/a.jpeg")
    );
    assert!(douyin_avatar_from_page("<html></html>").is_none());
}

/// Deterministic fake network: per-room avatar URLs, bounded fetch delay and
/// concurrency accounting.
struct FakeTransport {
    avatars: Mutex<HashMap<String, Option<String>>>,
    payload: Vec<u8>,
    resolve_failure: Mutex<Option<AvatarFailure>>,
    fail_fetch: AtomicBool,
    delay: Mutex<Duration>,
    resolve_calls: AtomicUsize,
    fetch_calls: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
}

impl FakeTransport {
    fn new(avatars: &[(Platform, &str, Option<&str>)], payload: Vec<u8>) -> Arc<Self> {
        let map = avatars
            .iter()
            .map(|(platform, room, url)| {
                (
                    avatar_key(*platform, room),
                    url.map(str::to_owned),
                )
            })
            .collect();
        Arc::new(Self {
            avatars: Mutex::new(map),
            payload,
            resolve_failure: Mutex::new(None),
            fail_fetch: AtomicBool::new(false),
            delay: Mutex::new(Duration::ZERO),
            resolve_calls: AtomicUsize::new(0),
            fetch_calls: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
        })
    }
}

impl AvatarTransport for FakeTransport {
    fn resolve(&self, platform: Platform, room: &str) -> Option<String> {
        self.resolve_calls.fetch_add(1, Ordering::SeqCst);
        self.avatars
            .lock()
            .unwrap()
            .get(&avatar_key(platform, room))
            .cloned()
            .flatten()
    }

    fn resolve_detailed(&self, platform: Platform, room: &str) -> Result<String, AvatarFailure> {
        if let Some(failure) = *self.resolve_failure.lock().unwrap() {
            return Err(failure);
        }
        self.resolve(platform, room)
            .ok_or(AvatarFailure::Missing)
    }

    fn fetch(&self, _url: &str) -> Option<Vec<u8>> {
        self.fetch_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_fetch.load(Ordering::SeqCst) {
            return None;
        }
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        let delay = *self.delay.lock().unwrap();
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
        Some(self.payload.clone())
    }
}

fn wait_updates(service: &AvatarService, count: usize, timeout: Duration) -> Vec<AvatarUpdate> {
    let deadline = Instant::now() + timeout;
    let mut updates = Vec::new();
    while Instant::now() < deadline && updates.len() < count {
        while let Some(update) = service.try_recv() {
            updates.push(update);
        }
        if updates.len() < count {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    updates
}

#[test]
fn request_resolves_downloads_and_persists_without_repeating_work() {
    let directory = tempfile::tempdir().unwrap();
    let transport = FakeTransport::new(
        &[(
            Platform::Bili,
            "7777",
            Some("https://i0.hdslb.com/bfs/face/synthetic.jpg"),
        )],
        png_bytes(96, 96),
    );
    let service = AvatarService::with_transport(
        directory.path(),
        Arc::clone(&transport) as Arc<dyn AvatarTransport>,
        AvatarLimits::default(),
    );
    assert!(service.cached(Platform::Bili, "7777").is_none());
    service.request(Platform::Bili, "7777");
    // A second request while pending is deduplicated.
    service.request(Platform::Bili, "7777");
    let updates = wait_updates(&service, 1, Duration::from_secs(5));
    assert_eq!(updates.len(), 1, "no avatar update");
    let entry = &updates[0].entry;
    assert!(entry.url.starts_with("https://i0.hdslb.com/"));
    assert_eq!((entry.width, entry.height), (AVATAR_EDGE, AVATAR_EDGE));
    assert_eq!(transport.resolve_calls.load(Ordering::SeqCst), 1);
    assert_eq!(transport.fetch_calls.load(Ordering::SeqCst), 1);
    // Cached lookup and a repeat request do no further work.
    let cached = service.cached(Platform::Bili, "7777").expect("memory cache");
    assert_eq!(cached.url, entry.url);
    service.request(Platform::Bili, "7777");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(transport.resolve_calls.load(Ordering::SeqCst), 1);
    assert!(directory.path().join("avatars").join("index.json").is_file());
    assert!(
        directory
            .path()
            .join("avatars")
            .join("bili-7777.png")
            .is_file()
    );
}

#[test]
fn a_restart_paints_cached_avatars_without_network() {
    let directory = tempfile::tempdir().unwrap();
    let first_transport = FakeTransport::new(
        &[(
            Platform::Douyu,
            "8888",
            Some("https://apic.douyucdn.cn/synthetic.jpg"),
        )],
        png_bytes(64, 64),
    );
    let service = AvatarService::with_transport(
        directory.path(),
        Arc::clone(&first_transport) as Arc<dyn AvatarTransport>,
        AvatarLimits::default(),
    );
    service.request(Platform::Douyu, "8888");
    assert_eq!(
        wait_updates(&service, 1, Duration::from_secs(5)).len(),
        1
    );
    drop(service);

    let second_transport = FakeTransport::new(&[], png_bytes(8, 8));
    let restarted = AvatarService::with_transport(
        directory.path(),
        Arc::clone(&second_transport) as Arc<dyn AvatarTransport>,
        AvatarLimits::default(),
    );
    let cached = restarted
        .cached(Platform::Douyu, "8888")
        .expect("disk cache after restart");
    assert_eq!(cached.width, AVATAR_EDGE);
    assert_eq!(second_transport.resolve_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn failures_wait_for_the_retry_window_before_trying_again() {
    let directory = tempfile::tempdir().unwrap();
    let transport = FakeTransport::new(&[(Platform::Huya, "9999", None)], png_bytes(8, 8));
    let limits = AvatarLimits {
        retry_after: Duration::from_millis(300),
        ..AvatarLimits::default()
    };
    let service = AvatarService::with_transport(
        directory.path(),
        Arc::clone(&transport) as Arc<dyn AvatarTransport>,
        limits,
    );
    service.request(Platform::Huya, "9999");
    // Wait only until the first (failed) attempt finished.
    let deadline = Instant::now() + Duration::from_secs(2);
    while transport.resolve_calls.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(transport.resolve_calls.load(Ordering::SeqCst), 1);
    assert!(service.try_recv().is_none(), "a failure must not publish");
    // Inside the retry window nothing is scheduled.
    service.request(Platform::Huya, "9999");
    std::thread::sleep(Duration::from_millis(40));
    assert_eq!(transport.resolve_calls.load(Ordering::SeqCst), 1);
    // After the window a new attempt is allowed (and succeeds here).
    std::thread::sleep(Duration::from_millis(320));
    transport.avatars.lock().unwrap().insert(
        avatar_key(Platform::Huya, "9999"),
        Some("https://alpic.huya.com/a.png".into()),
    );
    service.request(Platform::Huya, "9999");
    let updates = wait_updates(&service, 1, Duration::from_secs(5));
    assert_eq!(updates.len(), 1);
    assert_eq!(transport.resolve_calls.load(Ordering::SeqCst), 2);
}

#[test]
fn concurrency_is_bounded_by_the_pool_size() {
    let directory = tempfile::tempdir().unwrap();
    let rooms: Vec<(Platform, &str, Option<&str>)> = ["101", "102", "103", "104", "105", "106"]
        .iter()
        .map(|room| {
            (
                Platform::Bili,
                *room,
                Some("https://i0.hdslb.com/bfs/face/synthetic.jpg"),
            )
        })
        .collect();
    let transport = FakeTransport::new(&rooms, png_bytes(32, 32));
    *transport.delay.lock().unwrap() = Duration::from_millis(60);
    let service = AvatarService::with_transport(
        directory.path(),
        Arc::clone(&transport) as Arc<dyn AvatarTransport>,
        AvatarLimits {
            concurrency: 3,
            ..AvatarLimits::default()
        },
    );
    for (platform, room, _) in &rooms {
        service.request(*platform, room);
    }
    let updates = wait_updates(&service, rooms.len(), Duration::from_secs(10));
    assert_eq!(updates.len(), rooms.len());
    let max_active = transport.max_active.load(Ordering::SeqCst);
    assert!(max_active <= 3, "exceeded the pool: {max_active}");
    assert!(max_active >= 2, "pool did not run in parallel: {max_active}");
}

#[test]
fn oversized_and_undecodable_images_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let transport = FakeTransport::new(
        &[(
            Platform::Bili,
            "123",
            Some("https://i0.hdslb.com/bfs/face/huge.jpg"),
        )],
        vec![7_u8; 300 * 1024],
    );
    let service = AvatarService::with_transport(
        directory.path(),
        Arc::clone(&transport) as Arc<dyn AvatarTransport>,
        AvatarLimits::default(),
    );
    service.request(Platform::Bili, "123");
    assert!(wait_updates(&service, 1, Duration::from_millis(300)).is_empty());
    assert_eq!(transport.fetch_calls.load(Ordering::SeqCst), 1);
    assert!(service.cached(Platform::Bili, "123").is_none());
}

#[test]
fn observer_receives_completed_avatars() {
    let directory = tempfile::tempdir().unwrap();
    let transport = FakeTransport::new(
        &[(
            Platform::Bili,
            "42",
            Some("https://i0.hdslb.com/bfs/face/a.jpg"),
        )],
        png_bytes(48, 48),
    );
    let service = AvatarService::with_transport(
        directory.path(),
        Arc::clone(&transport) as Arc<dyn AvatarTransport>,
        AvatarLimits::default(),
    );
    let seen: Arc<Mutex<Vec<AvatarUpdate>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    service.set_observer(Some(Arc::new(move |update| {
        sink.lock().unwrap().push(update);
    })));
    service.request(Platform::Bili, "42");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && seen.lock().unwrap().is_empty() {
        std::thread::sleep(Duration::from_millis(5));
    }
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].key(), "bili:42");
}

#[test]
fn a_corrupt_index_or_missing_image_degrades_to_a_placeholder() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("avatars")).unwrap();
    std::fs::write(
        directory.path().join("avatars").join("index.json"),
        b"{not json",
    )
    .unwrap();
    let transport = FakeTransport::new(
        &[(
            Platform::Bili,
            "5",
            Some("https://i0.hdslb.com/bfs/face/a.jpg"),
        )],
        png_bytes(48, 48),
    );
    let service = AvatarService::with_transport(
        directory.path(),
        Arc::clone(&transport) as Arc<dyn AvatarTransport>,
        AvatarLimits::default(),
    );
    assert!(service.cached(Platform::Bili, "5").is_none());
    // The cache miss schedules a normal resolve instead of staying broken.
    service.request(Platform::Bili, "5");
    assert_eq!(
        wait_updates(&service, 1, Duration::from_secs(5)).len(),
        1
    );

    // A valid index whose image file vanished also falls back to None.
    let valid_index = directory.path().join("avatars").join("index.json");
    let text = std::fs::read_to_string(&valid_index).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    value["entries"]["bili:5"]["file"] = json!("bili-5.png");
    std::fs::write(&valid_index, serde_json::to_vec(&value).unwrap()).unwrap();
    let transport = FakeTransport::new(&[], png_bytes(8, 8));
    let fresh = AvatarService::with_transport(
        directory.path(),
        Arc::clone(&transport) as Arc<dyn AvatarTransport>,
        AvatarLimits::default(),
    );
    // The in-memory entry from this service does not exist in the new one and the
    // on-disk image is present, so it reads it; delete it first to simulate loss.
    std::fs::remove_file(directory.path().join("avatars").join("bili-5.png")).unwrap();
    assert!(fresh.cached(Platform::Bili, "5").is_none());
}

#[test]
fn avatar_entries_are_png_and_48px() {
    let directory = tempfile::tempdir().unwrap();
    let transport = FakeTransport::new(
        &[(
            Platform::Bili,
            "9",
            Some("https://i0.hdslb.com/bfs/face/a.jpg"),
        )],
        png_bytes(300, 200),
    );
    let service = AvatarService::with_transport(
        directory.path(),
        Arc::clone(&transport) as Arc<dyn AvatarTransport>,
        AvatarLimits::default(),
    );
    service.request(Platform::Bili, "9");
    let updates = wait_updates(&service, 1, Duration::from_secs(5));
    let entry: &AvatarEntry = &updates[0].entry;
    assert_eq!((entry.width, entry.height), (AVATAR_EDGE, AVATAR_EDGE));
    let decoded = image::load_from_memory(&entry.png).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (AVATAR_EDGE, AVATAR_EDGE));
}

#[test]
fn default_retry_window_is_ten_minutes_for_upgraded_launchers() {
    // The legacy launcher retried after 20 minutes; a freshly upgraded launcher
    // must not look broken that long, so the default window is 10 minutes.
    assert_eq!(
        AvatarLimits::default().retry_after,
        Duration::from_secs(10 * 60)
    );
}

#[test]
fn platform_failures_are_classified_and_exposed_for_diagnostics() {
    let directory = tempfile::tempdir().unwrap();
    let transport = FakeTransport::new(
        &[(
            Platform::Bili,
            "7777",
            Some("https://i0.hdslb.com/bfs/face/a.jpg"),
        )],
        png_bytes(48, 48),
    );
    *transport.resolve_failure.lock().unwrap() = Some(AvatarFailure::Blocked);
    let service = AvatarService::with_transport(
        directory.path(),
        Arc::clone(&transport) as Arc<dyn AvatarTransport>,
        AvatarLimits::default(),
    );
    service.request(Platform::Bili, "7777");
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut failure = None;
    while Instant::now() < deadline && failure.is_none() {
        failure = service.try_recv_failure();
        if failure.is_none() {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let failure = failure.expect("classified failure event");
    assert_eq!(failure.failure, AvatarFailure::Blocked);
    assert_eq!(failure.key(), "bili:7777");
    assert_eq!(
        service.last_failure(Platform::Bili, "7777"),
        Some(AvatarFailure::Blocked)
    );
    assert!(
        service.try_recv().is_none(),
        "a failure must not publish an avatar update"
    );
    assert_eq!(
        AvatarFailure::Blocked.label(),
        "平台风控或拒绝",
        "the UI/log can show an actionable reason"
    );
}

#[test]
fn network_and_invalid_image_failures_get_distinct_classes() {
    let directory = tempfile::tempdir().unwrap();
    let transport = FakeTransport::new(
        &[(
            Platform::Bili,
            "8888",
            Some("https://i0.hdslb.com/bfs/face/a.jpg"),
        )],
        png_bytes(48, 48),
    );
    transport.fail_fetch.store(true, Ordering::SeqCst);
    let service = AvatarService::with_transport(
        directory.path(),
        Arc::clone(&transport) as Arc<dyn AvatarTransport>,
        AvatarLimits::default(),
    );
    service.request(Platform::Bili, "8888");
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut failure = None;
    while Instant::now() < deadline && failure.is_none() {
        failure = service.try_recv_failure();
        if failure.is_none() {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    assert_eq!(
        failure.expect("network failure event").failure,
        AvatarFailure::Transport
    );
    drop(service);

    // A decodable-URL but undecodable payload is an invalid-image failure.
    let directory = tempfile::tempdir().unwrap();
    let transport = FakeTransport::new(
        &[(
            Platform::Huya,
            "910323",
            Some("https://alpic.huya.com/avatar/a.png"),
        )],
        vec![1_u8; 512],
    );
    let service = AvatarService::with_transport(
        directory.path(),
        Arc::clone(&transport) as Arc<dyn AvatarTransport>,
        AvatarLimits::default(),
    );
    service.request(Platform::Huya, "910323");
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut failure = None;
    while Instant::now() < deadline && failure.is_none() {
        failure = service.try_recv_failure();
        if failure.is_none() {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let failure = failure.expect("invalid image failure event");
    assert_eq!(failure.failure, AvatarFailure::InvalidImage);
    assert!(service.cached(Platform::Huya, "910323").is_none());
}
