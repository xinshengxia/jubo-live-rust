//! Bounded, presentation-only platform avatar cache for the favorites cards.
//!
//! The legacy launcher resolved the platform account avatar, downloaded a
//! bounded thumbnail, re-encoded it to a 48px PNG and kept both an index and the
//! images under its runtime directory, so a restart painted the known faces with
//! no network request. This module keeps that contract with a small request pool:
//!
//! * AvatarService::cached is synchronous and never touches the network; the UI
//!   paints a cached avatar (or its initial-letter placeholder) immediately.
//! * AvatarService::request queues an anonymous platform lookup + download.
//!   Requests are deduplicated, capped by a small concurrency limit and put on a
//!   10 minute retry window after a failure (the legacy launcher retried after 20
//!   minutes, which made a freshly upgraded launcher look broken for too long).
//!   There is no per-room polling loop: the UI asks on demand, exactly like the
//!   legacy workbench avatar pump.
//! * Failures are classified (network / platform block / missing field /
//!   non-allowlisted URL) and exposed through try_recv_failure plus the optional
//!   failure observer, so "no avatar" is diagnosable instead of silent.
//! * Completed avatars are pushed to a queue (AvatarService::try_recv) and/or an
//!   observer callback, which the UI drains per frame.
//!
//! Disk layout under the launcher data directory: avatars/index.json plus one
//! <platform>-<room>.png per room. Writes are atomic (temp file + rename); a
//! corrupted index or image is ignored instead of failing a launch or a frame.

use crate::core::Platform;
pub use crate::platforms::AvatarFailure;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    io::{Cursor, Read},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, RwLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// Largest accepted avatar payload before decoding.
const MAX_AVATAR_BYTES: usize = 256 * 1024;
/// Largest accepted source image edge (the platform image is downscaled after).
const MAX_SOURCE_EDGE: u32 = 2048;
/// Cached image edge: the legacy launcher stored 48px PNGs.
pub const AVATAR_EDGE: u32 = 48;
const DEFAULT_CONCURRENCY: usize = 4;
const DEFAULT_MEMORY_CACHE: usize = 32;
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(10 * 60);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(8);
/// The favorites page shows at most a few dozen cards; keep the persisted index
/// bounded so a long-lived data directory cannot grow without limit.
const MAX_INDEX_ENTRIES: usize = 256;
/// Bound the update queue even if a UI never drains it.
const MAX_QUEUED_UPDATES: usize = 256;
const INDEX_FILE: &str = "index.json";
const INDEX_VERSION: u32 = 1;

/// One resolved avatar: canonical HTTPS URL and a bounded 48px PNG.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvatarEntry {
    pub url: String,
    pub png: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
}

/// A completed background request, delivered to the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvatarUpdate {
    pub platform: Platform,
    pub room: String,
    pub entry: AvatarEntry,
}

impl AvatarUpdate {
    /// Stable cache key shared with AvatarService::cached.
    pub fn key(&self) -> String {
        avatar_key(self.platform, &self.room)
    }
}

/// platform:room cache key used by every public method.
pub fn avatar_key(platform: Platform, room: &str) -> String {
    format!("{platform}:{room}")
}

/// Tunables, primarily for the offline contract tests.
#[derive(Clone, Copy, Debug)]
pub struct AvatarLimits {
    pub concurrency: usize,
    pub memory_cache: usize,
    pub retry_after: Duration,
    pub timeout: Duration,
    pub max_bytes: usize,
}

impl Default for AvatarLimits {
    fn default() -> Self {
        Self {
            concurrency: DEFAULT_CONCURRENCY,
            memory_cache: DEFAULT_MEMORY_CACHE,
            retry_after: DEFAULT_RETRY_AFTER,
            timeout: DEFAULT_TIMEOUT,
            max_bytes: MAX_AVATAR_BYTES,
        }
    }
}

/// Network surface of the service. Production uses OfficialTransport; the
/// offline tests inject a deterministic fake.
pub trait AvatarTransport: Send + Sync {
    /// Anonymous platform lookup: room -> avatar URL (None on any failure).
    fn resolve(&self, platform: Platform, room: &str) -> Option<String>;
    /// Download the source image bytes for an already-normalized avatar URL.
    fn fetch(&self, url: &str) -> Option<Vec<u8>>;

    /// Classified variant used for the diagnostics channel. The default wraps the
    /// Option API; the production transport reports platform/transport details.
    fn resolve_detailed(&self, platform: Platform, room: &str) -> Result<String, AvatarFailure> {
        self.resolve(platform, room)
            .ok_or(AvatarFailure::Missing)
    }
    /// Classified download variant; the default reports a generic network failure.
    fn fetch_detailed(&self, url: &str) -> Result<Vec<u8>, AvatarFailure> {
        self.fetch(url).ok_or(AvatarFailure::Transport)
    }
}

/// One classified failure, delivered to diagnostics/log consumers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvatarFailureEvent {
    pub platform: Platform,
    pub room: String,
    pub failure: AvatarFailure,
    pub at: DateTime<Utc>,
}

impl AvatarFailureEvent {
    pub fn key(&self) -> String {
        avatar_key(self.platform, &self.room)
    }
}

/// Anonymous public platform lookup plus a bounded HTTPS download.
pub struct OfficialTransport;

impl AvatarTransport for OfficialTransport {
    fn resolve(&self, platform: Platform, room: &str) -> Option<String> {
        self.resolve_detailed(platform, room).ok()
    }

    fn fetch(&self, url: &str) -> Option<Vec<u8>> {
        self.fetch_detailed(url).ok()
    }

    fn resolve_detailed(&self, platform: Platform, room: &str) -> Result<String, AvatarFailure> {
        crate::platforms::avatar_url_for_room(platform, room)
    }

    fn fetch_detailed(&self, url: &str) -> Result<Vec<u8>, AvatarFailure> {
        let thumbnail = crate::platforms::avatar_thumbnail_url(url);
        // The URL was normalized by the resolver; re-check before every request so a
        // future caller cannot point the downloader at a non-allowlisted host.
        let checked = crate::platforms::normalize_avatar_url(&thumbnail)
            .ok_or(AvatarFailure::Disallowed)?;
        download(&checked, MAX_AVATAR_BYTES, DEFAULT_TIMEOUT)
    }
}

fn download(url: &str, max_bytes: usize, timeout: Duration) -> Result<Vec<u8>, AvatarFailure> {
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(5))
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .build()
        .map_err(|_| AvatarFailure::Transport)?;
    let response = client
        .get(url)
        .send()
        .map_err(|_| AvatarFailure::Transport)?;
    if !response.status().is_success() {
        return Err(AvatarFailure::Blocked);
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(AvatarFailure::Blocked);
    }
    let mut bytes = Vec::new();
    response
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| AvatarFailure::Transport)?;
    if bytes.is_empty() {
        return Err(AvatarFailure::Transport);
    }
    if bytes.len() > max_bytes {
        return Err(AvatarFailure::Blocked);
    }
    Ok(bytes)
}

/// Decode and resize a public avatar on its worker before it reaches the UI.
pub(crate) fn decode_avatar(bytes: &[u8]) -> Option<image::RgbaImage> {
    let image = decode_limited(bytes, MAX_AVATAR_BYTES, MAX_SOURCE_EDGE)?;
    Some(image.resize_to_fill(AVATAR_EDGE, AVATAR_EDGE, image::imageops::FilterType::Triangle).into_rgba8())
}

fn decode_limited(bytes: &[u8], max_bytes: usize, max_edge: u32) -> Option<image::DynamicImage> {
    if bytes.is_empty() || bytes.len() > max_bytes {
        return None;
    }
    // Inspect the container before creating a pixel decoder. Decoder limits
    // below also bound allocation when a format carries inconsistent metadata.
    let dimensions = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format().ok()?.into_dimensions().ok()?;
    if dimensions.0 == 0 || dimensions.1 == 0 || dimensions.0 > max_edge || dimensions.1 > max_edge {
        return None;
    }
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(max_edge);
    limits.max_image_height = Some(max_edge);
    limits.max_alloc = Some(24 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().ok()?;
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 || width > max_edge || height > max_edge {
        return None;
    }
    Some(image)
}

fn normalize_avatar_png(bytes: &[u8], max_bytes: usize) -> Option<(Arc<Vec<u8>>, u32, u32)> {
    let image = decode_limited(bytes, max_bytes, MAX_SOURCE_EDGE)?;
    let resized = image.resize_to_fill(
        AVATAR_EDGE,
        AVATAR_EDGE,
        image::imageops::FilterType::Triangle,
    );
    let mut png = Vec::new();
    resized
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    (!png.is_empty()).then(|| (Arc::new(png), AVATAR_EDGE, AVATAR_EDGE))
}

fn write_atomic(directory: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(directory)?;
    let temporary = directory.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    std::fs::write(&temporary, bytes)?;
    match std::fs::rename(&temporary, directory.join(name)) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error)
        }
    }
}

fn component(value: &str) -> String {
    let mut result: String = value
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
        .take(64)
        .collect();
    if result.is_empty() {
        result.push_str("room");
    }
    result
}

fn avatar_file_name(platform: Platform, room: &str) -> String {
    format!("{}-{}.png", platform, component(room))
}

#[derive(Clone, Serialize, Deserialize)]
struct IndexEntry {
    url: String,
    file: String,
    #[serde(default)]
    checked_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Serialize, Deserialize)]
struct AvatarIndex {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    entries: BTreeMap<String, IndexEntry>,
    /// Public avatar URLs learned from lookup candidates. They are persisted even
    /// when no PNG has been downloaded yet so an offline Douyin web_rid or a Huya
    /// alias room can paint a face after a restart without a fresh platform
    /// lookup. Values are already normalized/allowlisted by remember().
    #[serde(default)]
    remembered: BTreeMap<String, String>,
}

impl Default for AvatarIndex {
    fn default() -> Self {
        Self {
            version: INDEX_VERSION,
            entries: BTreeMap::new(),
            remembered: BTreeMap::new(),
        }
    }
}

struct Job {
    platform: Platform,
    room: String,
}

#[derive(Default)]
struct ServiceState {
    entries: HashMap<String, AvatarEntry>,
    recency: VecDeque<String>,
    pending: HashSet<String>,
    retry_at: HashMap<String, Instant>,
    last_failure: HashMap<String, AvatarFailure>,
    index: AvatarIndex,
    index_loaded: bool,
}

struct Shared {
    root: PathBuf,
    limits: AvatarLimits,
    transport: Arc<dyn AvatarTransport>,
    state: Mutex<ServiceState>,
    queue: Mutex<VecDeque<Job>>,
    wake: Condvar,
    shutdown: AtomicBool,
    updates: Mutex<VecDeque<AvatarUpdate>>,
    observer: RwLock<Option<Arc<dyn Fn(AvatarUpdate) + Send + Sync>>>,
    failures: Mutex<VecDeque<AvatarFailureEvent>>,
    failure_observer: RwLock<Option<Arc<dyn Fn(AvatarFailureEvent) + Send + Sync>>>,
    active: AtomicUsize,
}

/// Handle to the shared avatar service. Cloning it is cheap; the last clone
/// shuts the worker pool down.
#[derive(Clone)]
pub struct AvatarService {
    inner: Arc<Inner>,
}

struct Inner {
    shared: Arc<Shared>,
    workers: Mutex<Vec<thread::JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.wake.notify_all();
        // Workers only hold Arc<Shared>, never this Inner, so joining here cannot
        // deadlock. A worker inside a transport call finishes at the transport's
        // own timeout.
        let mut workers = self
            .workers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for worker in workers.drain(..) {
            let _ = worker.join();
        }
    }
}

impl AvatarService {
    /// Production service rooted at the launcher data directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_transport(root, Arc::new(OfficialTransport), AvatarLimits::default())
    }

    /// Service with an injected transport and limits (offline tests/diagnostics).
    pub fn with_transport(
        root: impl Into<PathBuf>,
        transport: Arc<dyn AvatarTransport>,
        limits: AvatarLimits,
    ) -> Self {
        let root = root.into().join("avatars");
        let limits = AvatarLimits {
            concurrency: limits.concurrency.clamp(1, 8),
            memory_cache: limits.memory_cache.max(1),
            ..limits
        };
        let shared = Arc::new(Shared {
            root,
            limits,
            transport,
            state: Mutex::new(ServiceState::default()),
            queue: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            shutdown: AtomicBool::new(false),
            updates: Mutex::new(VecDeque::new()),
            observer: RwLock::new(None),
            failures: Mutex::new(VecDeque::new()),
            failure_observer: RwLock::new(None),
            active: AtomicUsize::new(0),
        });
        let mut workers = Vec::new();
        for index in 0..limits.concurrency {
            let worker_shared = Arc::clone(&shared);
            let handle = thread::Builder::new()
                .name(format!("avatar-worker-{index}"))
                .spawn(move || worker_loop(worker_shared));
            if let Ok(handle) = handle {
                workers.push(handle);
            }
        }
        Self {
            inner: Arc::new(Inner {
                shared,
                workers: Mutex::new(workers),
            }),
        }
    }

    /// Synchronous cache lookup (memory, then the persisted index). Never blocks
    /// on the network and never schedules work.
    pub fn cached(&self, platform: Platform, room: &str) -> Option<AvatarEntry> {
        self.inner.shared.cached(platform, room)
    }

    /// Remember a public avatar URL for this platform/room pair.
    ///
    /// The lookup page calls this when a candidate is selected/verified or added
    /// to favourites. The URL goes through platforms::normalize_avatar_url
    /// (HTTPS + platform CDN allowlist) and is persisted immediately; the next
    /// favourites request can fetch the image without re-resolving the platform
    /// account. Returns the normalized URL when accepted.
    pub fn remember(&self, platform: Platform, room: &str, avatar_url: &str) -> Option<String> {
        if room.trim().is_empty() {
            return None;
        }
        let url = crate::platforms::normalize_avatar_url(avatar_url)?;
        let key = avatar_key(platform, room);
        {
            let mut state = self
                .inner
                .shared
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            self.inner.shared.ensure_index(&mut state);
            state.index.version = INDEX_VERSION;
            state.index.remembered.insert(key.clone(), url.clone());
            if let Some(entry) = state.index.entries.get_mut(&key) {
                entry.url = url.clone();
            }
            if let Some(entry) = state.entries.get_mut(&key) {
                entry.url = url.clone();
            }
        }
        self.inner.shared.save_index();
        Some(url)
    }

    /// Previously remembered public avatar URL, if any (offline read).
    pub fn remembered(&self, platform: Platform, room: &str) -> Option<String> {
        if room.trim().is_empty() {
            return None;
        }
        let mut state = self
            .inner
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.inner.shared.ensure_index(&mut state);
        state
            .index
            .remembered
            .get(&avatar_key(platform, room))
            .cloned()
    }

    /// Schedule an anonymous resolve/download unless a cached avatar, a pending
    /// request or the failure retry window already covers this room.
    pub fn request(&self, platform: Platform, room: &str) {
        self.inner.shared.enqueue(platform, room, false);
    }

    /// Re-check one room even when it has a cached image (the user re-selected
    /// it); an unchanged URL costs no download.
    pub fn refresh(&self, platform: Platform, room: &str) {
        self.inner.shared.enqueue(platform, room, true);
    }

    /// Drain one completed request, if any (call from the UI frame loop).
    pub fn try_recv(&self) -> Option<AvatarUpdate> {
        self.inner
            .shared
            .updates
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop_front()
    }

    /// Install an observer invoked from a worker thread after each new avatar.
    /// The UI must marshal it back to its own thread (or keep using try_recv).
    pub fn set_observer(&self, observer: Option<Arc<dyn Fn(AvatarUpdate) + Send + Sync>>) {
        *self
            .inner
            .shared
            .observer
            .write()
            .unwrap_or_else(|error| error.into_inner()) = observer;
    }

    /// Drain one classified failure, if any (diagnostics/log consumer).
    pub fn try_recv_failure(&self) -> Option<AvatarFailureEvent> {
        self.inner
            .shared
            .failures
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop_front()
    }

    /// Install an observer invoked from a worker thread for classified failures.
    pub fn set_failure_observer(
        &self,
        observer: Option<Arc<dyn Fn(AvatarFailureEvent) + Send + Sync>>,
    ) {
        *self
            .inner
            .shared
            .failure_observer
            .write()
            .unwrap_or_else(|error| error.into_inner()) = observer;
    }

    /// Last classified failure for one room (cleared on a successful refresh).
    pub fn last_failure(&self, platform: Platform, room: &str) -> Option<AvatarFailure> {
        let key = avatar_key(platform, room);
        self.inner
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .last_failure
            .get(&key)
            .copied()
    }

    /// Number of requests currently being resolved/downloaded (diagnostics).
    pub fn active_requests(&self) -> usize {
        self.inner.shared.active.load(Ordering::Acquire)
    }

    /// Failure retry window applied to every failed room request.
    pub fn retry_after(&self) -> Duration {
        self.inner.shared.limits.retry_after
    }
}

fn worker_loop(shared: Arc<Shared>) {
    loop {
        let job = {
            let mut queue = shared
                .queue
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            loop {
                if shared.shutdown.load(Ordering::Acquire) {
                    return;
                }
                if let Some(job) = queue.pop_front() {
                    break job;
                }
                let Ok(woken) = shared.wake.wait(queue) else {
                    return;
                };
                queue = woken;
            }
        };
        shared.run_job(job);
    }
}

impl Shared {
    fn ensure_index(&self, state: &mut ServiceState) {
        if state.index_loaded {
            return;
        }
        state.index_loaded = true;
        let Ok(bytes) = std::fs::read(self.root.join(INDEX_FILE)) else {
            return;
        };
        let Ok(index) = serde_json::from_slice::<AvatarIndex>(&bytes) else {
            return;
        };
        if index.version == INDEX_VERSION {
            state.index = index;
        }
    }

    fn cached(&self, platform: Platform, room: &str) -> Option<AvatarEntry> {
        if room.is_empty() {
            return None;
        }
        let key = avatar_key(platform, room);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(entry) = state.entries.get(&key).cloned() {
            touch(&mut state, &key);
            return Some(entry);
        }
        self.ensure_index(&mut state);
        let index_entry = state.index.entries.get(&key)?.clone();
        // Cached files are generated 48px thumbnails. Never trust index paths,
        // file sizes or compressed image dimensions merely because they are local.
        let bytes = (|| {
            if index_entry.file != avatar_file_name(platform, room) {
                return None;
            }
            let path = self.root.join(&index_entry.file);
            if std::fs::symlink_metadata(&path).ok()?.file_type().is_symlink() {
                return None;
            }
            let file = std::fs::File::open(path).ok()?;
            let metadata = file.metadata().ok()?;
            if !metadata.is_file() || metadata.len() > MAX_AVATAR_BYTES as u64 {
                return None;
            }
            let mut bytes = Vec::new();
            file.take(MAX_AVATAR_BYTES as u64 + 1).read_to_end(&mut bytes).ok()?;
            let image = decode_limited(&bytes, MAX_AVATAR_BYTES, AVATAR_EDGE)?;
            (image.width() == AVATAR_EDGE && image.height() == AVATAR_EDGE).then_some(bytes)
        })();
        let Some(bytes) = bytes else {
            state.index.entries.remove(&key);
            drop(state);
            self.save_index();
            return None;
        };
        let entry = AvatarEntry {
            url: index_entry.url,
            png: Arc::new(bytes),
            width: AVATAR_EDGE,
            height: AVATAR_EDGE,
        };
        state.entries.insert(key.clone(), entry.clone());
        touch(&mut state, &key);
        trim_memory(&mut state, self.limits.memory_cache);
        Some(entry)
    }

    fn enqueue(&self, platform: Platform, room: &str, refresh: bool) {
        if room.is_empty() || self.shutdown.load(Ordering::Acquire) {
            return;
        }
        let key = avatar_key(platform, room);
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if state.pending.contains(&key) {
                return;
            }
            if !refresh {
                if state.entries.contains_key(&key) {
                    return;
                }
                self.ensure_index(&mut state);
                if state.index.entries.contains_key(&key) {
                    return;
                }
                if state
                    .retry_at
                    .get(&key)
                    .is_some_and(|retry| Instant::now() < *retry)
                {
                    return;
                }
            }
            state.pending.insert(key);
        }
        self.queue
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_back(Job {
                platform,
                room: room.to_owned(),
            });
        self.wake.notify_one();
    }

    fn run_job(self: &Arc<Self>, job: Job) {
        let key = avatar_key(job.platform, &job.room);
        self.active.fetch_add(1, Ordering::AcqRel);
        let outcome = self.resolve_avatar(job.platform, &job.room);
        self.active.fetch_sub(1, Ordering::AcqRel);

        let entry = match outcome {
            Ok(entry) => entry,
            Err(failure) => {
                {
                    let mut state = self
                        .state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    state.pending.remove(&key);
                    state.last_failure.insert(key, failure);
                    state.retry_at.insert(
                        avatar_key(job.platform, &job.room),
                        Instant::now() + self.limits.retry_after,
                    );
                }
                self.publish_failure(AvatarFailureEvent {
                    platform: job.platform,
                    room: job.room,
                    failure,
                    at: Utc::now(),
                });
                return;
            }
        };

        let file = avatar_file_name(job.platform, &job.room);
        let persisted = write_atomic(&self.root, &file, &entry.png).is_ok();
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.pending.remove(&key);
            state.retry_at.remove(&key);
            state.last_failure.remove(&key);
            if persisted {
                state.index.version = INDEX_VERSION;
                state.index.entries.insert(
                    key.clone(),
                    IndexEntry {
                        url: entry.url.clone(),
                        file,
                        checked_at: Some(Utc::now()),
                    },
                );
                trim_index(&mut state);
            }
            state.entries.insert(key.clone(), entry.clone());
            touch(&mut state, &key);
            trim_memory(&mut state, self.limits.memory_cache);
        }
        self.save_index();
        self.publish(AvatarUpdate {
            platform: job.platform,
            room: job.room,
            entry,
        });
    }

    /// Remembered URL for one platform/room, loaded from the persisted index.
    fn remembered_url(&self, platform: Platform, room: &str) -> Option<String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.ensure_index(&mut state);
        state
            .index
            .remembered
            .get(&avatar_key(platform, room))
            .cloned()
    }

    fn download_avatar(&self, url: String) -> Result<AvatarEntry, AvatarFailure> {
        let url = crate::platforms::normalize_avatar_url(&url).ok_or(AvatarFailure::Disallowed)?;
        let bytes = self.transport.fetch_detailed(&url)?;
        normalize_avatar_png(&bytes, self.limits.max_bytes)
            .map(|(png, width, height)| AvatarEntry {
                url,
                png,
                width,
                height,
            })
            .ok_or(AvatarFailure::InvalidImage)
    }

    /// Resolve, download and downscale one avatar, keeping the failure reason.
    ///
    /// A remembered candidate URL is tried first (covers offline Douyin web_rid
    /// rooms and Huya alias rooms whose platform lookup may fail). If that image
    /// cannot be fetched, fall back to the normal platform resolver.
    fn resolve_avatar(&self, platform: Platform, room: &str) -> Result<AvatarEntry, AvatarFailure> {
        if let Some(url) = self.remembered_url(platform, room) {
            if let Ok(entry) = self.download_avatar(url) {
                return Ok(entry);
            }
        }
        let url = self.transport.resolve_detailed(platform, room)?;
        self.download_avatar(url)
    }

    fn publish_failure(&self, failure: AvatarFailureEvent) {
        if let Some(observer) = self
            .failure_observer
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .cloned()
        {
            observer(failure.clone());
        }
        let mut failures = self
            .failures
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if failures.len() >= MAX_QUEUED_UPDATES {
            failures.pop_front();
        }
        failures.push_back(failure);
    }

    fn save_index(&self) {
        let index = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.index.clone()
        };
        if let Ok(bytes) = serde_json::to_vec(&index) {
            let _ = write_atomic(&self.root, INDEX_FILE, &bytes);
        }
    }

    fn publish(&self, update: AvatarUpdate) {
        if let Some(observer) = self
            .observer
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .cloned()
        {
            observer(update.clone());
        }
        let mut updates = self
            .updates
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if updates.len() >= MAX_QUEUED_UPDATES {
            updates.pop_front();
        }
        updates.push_back(update);
    }
}

fn touch(state: &mut ServiceState, key: &str) {
    state.recency.retain(|current| current != key);
    state.recency.push_back(key.to_owned());
}

fn trim_memory(state: &mut ServiceState, limit: usize) {
    while state.entries.len() > limit {
        let Some(oldest) = state.recency.pop_front() else {
            break;
        };
        if !state.recency.iter().any(|current| current == &oldest) {
            state.entries.remove(&oldest);
        }
    }
}

fn trim_index(state: &mut ServiceState) {
    while state.index.entries.len() > MAX_INDEX_ENTRIES {
        let Some(oldest) = state
            .index
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.checked_at)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        state.index.entries.remove(&oldest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    struct FakeTransport {
        avatar: Option<String>,
        payload: Vec<u8>,
        calls: AtomicUsize,
    }

    impl AvatarTransport for FakeTransport {
        fn resolve(&self, _platform: Platform, _room: &str) -> Option<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.avatar.clone()
        }
        fn fetch(&self, _url: &str) -> Option<Vec<u8>> {
            Some(self.payload.clone())
        }
    }

    #[test]
    fn rejected_or_oversized_images_never_become_entries() {
        assert!(normalize_avatar_png(b"", MAX_AVATAR_BYTES).is_none());
        assert!(normalize_avatar_png(b"not-an-image", MAX_AVATAR_BYTES).is_none());
        let oversized = vec![0_u8; MAX_AVATAR_BYTES + 1];
        assert!(normalize_avatar_png(&oversized, MAX_AVATAR_BYTES).is_none());
        let huge = png_bytes(MAX_SOURCE_EDGE + 1, 16);
        assert!(normalize_avatar_png(&huge, MAX_AVATAR_BYTES).is_none());
        let (png, width, height) =
            normalize_avatar_png(&png_bytes(96, 96), MAX_AVATAR_BYTES).unwrap();
        assert_eq!((width, height), (AVATAR_EDGE, AVATAR_EDGE));
        let decoded = image::load_from_memory(&png).unwrap();
        assert_eq!(
            (decoded.width(), decoded.height()),
            (AVATAR_EDGE, AVATAR_EDGE)
        );
    }

    #[test]
    fn regression_avatar_decode_checks_dimensions_and_normalizes_all_formats() {
        for format in [image::ImageFormat::Png, image::ImageFormat::Jpeg, image::ImageFormat::WebP] {
            let mut bytes = Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(96, 64).write_to(&mut bytes, format).unwrap();
            let decoded = decode_avatar(bytes.get_ref()).expect("bounded avatar");
            assert_eq!(decoded.dimensions(), (AVATAR_EDGE, AVATAR_EDGE));
            assert_eq!(decoded.as_raw().len(), (AVATAR_EDGE * AVATAR_EDGE * 4) as usize);
        }
        let oversized = png_bytes(4096, 1);
        assert!(oversized.len() < MAX_AVATAR_BYTES);
        assert!(decode_avatar(&oversized).is_none());
        assert!(normalize_avatar_png(&oversized, MAX_AVATAR_BYTES).is_none());
        assert!(decode_avatar(b"corrupt").is_none());
    }

    #[test]
    fn regression_avatar_missing_or_corrupt_cache_is_downloaded_again() {
        for content in [None, Some(b"corrupt".to_vec()), Some(png_bytes(4096, 1))] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("avatars");
            std::fs::create_dir(&root).unwrap();
            let file = avatar_file_name(Platform::Bili, "123");
            let mut index = AvatarIndex::default();
            index.entries.insert("bili:123".into(), IndexEntry {
                url: "https://i0.hdslb.com/synthetic.png".into(), file: file.clone(), checked_at: None,
            });
            std::fs::write(root.join(INDEX_FILE), serde_json::to_vec(&index).unwrap()).unwrap();
            if let Some(bytes) = content { std::fs::write(root.join(file), bytes).unwrap(); }
            let transport = Arc::new(FakeTransport {
                avatar: Some("https://i0.hdslb.com/synthetic.png".into()),
                payload: png_bytes(96, 96), calls: AtomicUsize::new(0),
            });
            let service = AvatarService::with_transport(directory.path(), transport.clone(), AvatarLimits::default());
            assert!(service.cached(Platform::Bili, "123").is_none());
            let saved: AvatarIndex = serde_json::from_slice(&std::fs::read(root.join(INDEX_FILE)).unwrap()).unwrap();
            assert!(!saved.entries.contains_key("bili:123"), "invalid index must be removed durably");
            service.request(Platform::Bili, "123");
            let deadline = Instant::now() + Duration::from_secs(3);
            let update = loop {
                if let Some(update) = service.try_recv() { break update; }
                assert!(Instant::now() < deadline, "recovery download did not complete");
                thread::sleep(Duration::from_millis(10));
            };
            assert_eq!(update.entry.width, AVATAR_EDGE);
            assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
            let reloaded = AvatarService::with_transport(directory.path(), transport, AvatarLimits::default());
            assert!(reloaded.cached(Platform::Bili, "123").is_some());
        }
    }

    #[test]
    fn remembered_url_is_fetched_before_platform_resolution_and_persists() {
        let directory = tempfile::tempdir().unwrap();
        let transport = Arc::new(FakeTransport {
            avatar: Some("https://i0.hdslb.com/bfs/face/fallback.jpg".into()),
            payload: png_bytes(96, 96),
            calls: AtomicUsize::new(0),
        });
        let service =
            AvatarService::with_transport(directory.path(), transport.clone(), AvatarLimits::default());
        assert_eq!(
            service.remember(Platform::Douyin, "989125753255", "//p3-pc.douyinpic.com/aweme/100x100/synthetic.jpeg"),
            Some("https://p3-pc.douyinpic.com/aweme/100x100/synthetic.jpeg".into())
        );
        assert!(service
            .remember(Platform::Douyin, "989125753255", "https://example.com/avatar.jpg")
            .is_none());
        service.request(Platform::Douyin, "989125753255");
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut update = None;
        while Instant::now() < deadline && update.is_none() {
            update = service.try_recv();
            if update.is_none() {
                thread::sleep(Duration::from_millis(10));
            }
        }
        let update = update.expect("remembered avatar update");
        assert_eq!(
            update.entry.url,
            "https://p3-pc.douyinpic.com/aweme/100x100/synthetic.jpeg"
        );
        assert_eq!(
            transport.calls.load(Ordering::SeqCst),
            0,
            "remembered URL must skip the platform resolver"
        );
        drop(service);

        let reloaded_transport = Arc::new(FakeTransport {
            avatar: None,
            payload: Vec::new(),
            calls: AtomicUsize::new(0),
        });
        let reloaded = AvatarService::with_transport(
            directory.path(),
            reloaded_transport,
            AvatarLimits::default(),
        );
        assert_eq!(
            reloaded.remembered(Platform::Douyin, "989125753255").as_deref(),
            Some("https://p3-pc.douyinpic.com/aweme/100x100/synthetic.jpeg")
        );
    }

    #[test]
    fn index_roundtrip_and_corruption_are_tolerated() {
        let directory = tempfile::tempdir().unwrap();
        let transport = Arc::new(FakeTransport {
            avatar: Some("https://i0.hdslb.com/bfs/face/example.jpg".into()),
            payload: png_bytes(96, 96),
            calls: AtomicUsize::new(0),
        });
        let service =
            AvatarService::with_transport(directory.path(), transport, AvatarLimits::default());
        service.request(Platform::Bili, "7777");
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut update = None;
        while Instant::now() < deadline && update.is_none() {
            update = service.try_recv();
            if update.is_none() {
                thread::sleep(Duration::from_millis(10));
            }
        }
        let update = update.expect("avatar update");
        assert_eq!(update.room, "7777");
        assert_eq!(update.entry.width, AVATAR_EDGE);
        assert!(update.entry.url.starts_with("https://i0.hdslb.com/"));

        // A new service paints from disk without resolving again.
        let transport = Arc::new(FakeTransport {
            avatar: Some("https://example.invalid/none.png".into()),
            payload: png_bytes(16, 16),
            calls: AtomicUsize::new(0),
        });
        let reloaded = AvatarService::with_transport(
            directory.path(),
            Arc::clone(&transport) as Arc<dyn AvatarTransport>,
            AvatarLimits::default(),
        );
        let cached = reloaded.cached(Platform::Bili, "7777").expect("disk cache");
        assert_eq!(cached.width, AVATAR_EDGE);
        assert_eq!(transport.calls.load(Ordering::SeqCst), 0);

        // Corrupted index: ignored, no panic, cache miss.
        std::fs::write(
            directory.path().join("avatars").join(INDEX_FILE),
            b"{oops",
        )
        .unwrap();
        let transport = Arc::new(FakeTransport {
            avatar: None,
            payload: Vec::new(),
            calls: AtomicUsize::new(0),
        });
        let broken = AvatarService::with_transport(
            directory.path(),
            Arc::clone(&transport) as Arc<dyn AvatarTransport>,
            AvatarLimits::default(),
        );
        assert!(broken.cached(Platform::Bili, "7777").is_none());
    }
}
