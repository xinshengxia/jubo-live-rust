//! Loopback-only HTTP/FLV relay with bounded connections and reconnect framing.

use crate::core::ResolvedStream;
use crate::flv::{self, ReconnectTimeline};
use anyhow::{Context, Result, bail};
use reqwest::blocking::{Client, Response};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use crate::diag;
use std::io::{self, BufReader, ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

pub trait StreamResolver: Send + Sync {
    /// Implementations must bound their own network operations.
    fn resolve(&self) -> Result<ResolvedStream>;
}

impl<F> StreamResolver for F
where
    F: Fn() -> Result<ResolvedStream> + Send + Sync,
{
    fn resolve(&self) -> Result<ResolvedStream> {
        self()
    }
}

#[derive(Clone, Debug)]
pub struct RelayConfig {
    pub bind: SocketAddr,
    pub token: String,
    pub max_connections: usize,
    pub idle_timeout: Duration,
    pub request_timeout: Duration,
    pub upstream_timeout: Duration,
    pub write_timeout: Duration,
    pub retry_delay: Duration,
    /// Maximum consecutive unproductive reconnects, not a lifetime limit.
    /// Zero explicitly disables reconnection after the first upstream attempt.
    pub max_reconnects: usize,
    /// Test harnesses only. The CLI must never expose this switch.
    pub allow_loopback_fixture: bool,
    /// Launcher-owned relays require a player registration before this deadline.
    /// The registered process handle lives in the relay, independently of the GUI.
    pub require_player_registration: bool,
    pub player_registration_timeout: Duration,
    /// Component log directory for the bounded, redacted relay diagnostics.
    /// Tests pass an explicit tempdir; the launcher initialises the global diag
    /// directory in the relay child process, so None then falls back to diag::log.
    pub log_dir: Option<PathBuf>,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            token: String::new(),
            max_connections: 8,
            idle_timeout: Duration::from_secs(180),
            request_timeout: Duration::from_secs(5),
            upstream_timeout: Duration::from_secs(5),
            // The legacy launcher used a 15 s send timeout. A live player
            // (PotPlayer) can stop reading for a few seconds while it rebuilds
            // its own cache; a shorter timeout turned that pause into a closed
            // connection, a fresh upstream request and a visible re-buffer.
            write_timeout: Duration::from_secs(15),
            retry_delay: Duration::from_secs(1),
            max_reconnects: 500,
            allow_loopback_fixture: false,
            require_player_registration: false,
            player_registration_timeout: Duration::from_secs(15),
            log_dir: None,
        }
    }
}

/// Component name used by the bounded diagnostic log for relay events.
const RELAY_LOG_COMPONENT: &str = "relay";

/// Writes one relay diagnostic line: explicit directory when given (tests,
/// launcher child), otherwise the process-global diag directory when initialised.
fn relay_log(log_dir: Option<&Path>, level: diag::Level, message: &str) {
    match log_dir {
        Some(directory) => {
            let _ = diag::append(directory, RELAY_LOG_COMPONENT, level, message);
        }
        None => diag::log(RELAY_LOG_COMPONENT, level, message),
    }
}

pub struct RelayServer {
    listener: TcpListener,
    config: RelayConfig,
    resolver: Arc<dyn StreamResolver>,
    player_lifetime: Arc<PlayerLifetime>,
    statistics: Arc<Mutex<RelayStatistics>>,
}

/// These recovery thresholds count only complete, forwarded audio/video media
/// tags. FLV/script headers, duplicate overlap and a successful HTTP status alone
/// cannot clear the consecutive-failure budget.
pub const STABLE_MEDIA_TAGS: u64 = 24;
pub const STABLE_MEDIA_BYTES: u64 = 8 * 1024;
pub const STABLE_MEDIA_SPAN_MS: u32 = 1_000;
const MIN_RETRY_DELAY: Duration = Duration::from_millis(5);
/// A CDN segment that ended after forwarding stable media is a normal boundary
/// (huya rotates short responses). Reconnect promptly instead of leaving the
/// player without media for the whole failure backoff; duplicate overlap is
/// trimmed by the reconnect timeline either way.
const MEDIA_SEGMENT_RETRY_DELAY: Duration = Duration::from_millis(150);
const RECENT_CONNECTIONS: usize = 8;
/// Upstream response bytes are copied through this buffer once. Without it the
/// blocking reqwest Response reader was entered once per 1-11 byte header read,
/// which dominated per-tag cost on Windows loopback (see notes/perf-optimizations.md).
const UPSTREAM_READ_BUFFER: usize = 64 * 1024;
/// A closed player is detected by the next media write immediately; this poll is
/// only a backstop for a stalled upstream, so it need not run per forwarded tag.
const PLAYER_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Framing buffers at or below this size are recycled across tags. A rare larger
/// tag is released instead of pinning its allocation for the connection lifetime.
const TAG_RECYCLE_LIMIT: usize = 1024 * 1024;
/// Forwarded tags are coalesced into this many bytes before one player write.
/// A flush also happens whenever the upstream read buffer empties, which bounds
/// added latency to data that has already arrived; a live stream therefore goes
/// out within one buffered read of being received.
const WRITE_BATCH_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayConnectionState {
    Connecting,
    Streaming,
    Retrying,
    Waiting,
    Ended,
    Exhausted,
    PolicyRejected,
    PlayerDisconnected,
    WriteFailed,
}

impl RelayConnectionState {
    fn label(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Streaming => "streaming",
            Self::Retrying => "retrying",
            Self::Waiting => "waiting",
            Self::Ended => "ended",
            Self::Exhausted => "exhausted",
            Self::PolicyRejected => "policy_rejected",
            Self::PlayerDisconnected => "player_disconnected",
            Self::WriteFailed => "write_failed",
        }
    }

    fn active(self) -> bool {
        matches!(
            self,
            Self::Connecting | Self::Streaming | Self::Retrying | Self::Waiting
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayFailure {
    ResolveFailed,
    ConnectFailed,
    InvalidFlv,
    ReadFailed,
    UpstreamEof,
    RetryBudgetExhausted,
    UpstreamPolicyRejected,
    PlayerDisconnected,
    PlayerWriteFailed,
}

impl RelayFailure {
    pub fn label(self) -> &'static str {
        match self {
            Self::ResolveFailed => "resolve_failed",
            Self::ConnectFailed => "connect_failed",
            Self::InvalidFlv => "invalid_flv",
            Self::ReadFailed => "read_failed",
            Self::UpstreamEof => "upstream_eof",
            Self::RetryBudgetExhausted => "retry_budget_exhausted",
            Self::UpstreamPolicyRejected => "upstream_policy_rejected",
            Self::PlayerDisconnected => "player_disconnected",
            Self::PlayerWriteFailed => "player_write_failed",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RelayConnectionStatistics {
    pub id: u64,
    pub state: RelayConnectionState,
    pub upstream_attempts: u64,
    pub reconnects: u64,
    pub consecutive_failures: u64,
    pub stable_segments: u64,
    pub forwarded_tags: u64,
    pub forwarded_bytes: u64,
    pub last_error: Option<RelayFailure>,
    pub waiting_state: Option<String>,
    /// In-response timeline discontinuities rebased so the player clock stays
    /// continuous (the CDN dropped queued media without reconnecting).
    #[serde(default)]
    pub timeline_jumps: u64,
}

/// A resolver has positively identified a room state rather than a network
/// failure. The player connection stays open while the resolver is polled again.
/// Only `offline` and `replay_unavailable` are recognized waiting states.
#[derive(Debug)]
pub struct ResolveWaiting {
    pub state: String,
    pub retry_after: Duration,
}

impl std::fmt::Display for ResolveWaiting {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("room is waiting for an available broadcast")
    }
}
impl std::error::Error for ResolveWaiting {}

/// FLV onMetaData summary of the forwarded stream. The legacy launcher probed
/// the upstream file for the same values before starting mpv; the relay already
/// reads every tag, so the launch-time title hints need no extra connection.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RelayStreamInfo {
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub fps: f64,
    /// Video bitrate in kbps from `videodatarate` (0 when the stream omits it,
    /// which is normal for Douyu).
    #[serde(default)]
    pub video_kbps: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
}

/// Aggregate counters and at most eight active/eight recent connection records.
/// Deliberately excludes media URLs, request headers, room IDs and session tokens.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RelayStatistics {
    pub media_get_requests: u64,
    pub rejected_requests: u64,
    pub active_streams: usize,
    pub total_streams: u64,
    pub upstream_attempts: u64,
    pub reconnects: u64,
    pub failures: u64,
    pub stable_segments: u64,
    pub forwarded_tags: u64,
    pub forwarded_bytes: u64,
    pub budget_exhaustions: u64,
    /// In-response timeline discontinuities rebased across every connection.
    #[serde(default)]
    pub timeline_jumps: u64,
    /// Bounded onMetaData summary of the stream currently being forwarded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_info: Option<RelayStreamInfo>,
    pub connections: Vec<RelayConnectionStatistics>,
}

struct StreamStatistics {
    store: Arc<Mutex<RelayStatistics>>,
    id: u64,
    log_dir: Option<PathBuf>,
}

impl StreamStatistics {
    fn begin(store: Arc<Mutex<RelayStatistics>>, log_dir: Option<PathBuf>) -> Self {
        let id;
        {
            let mut stats = store.lock().unwrap_or_else(|error| error.into_inner());
            // Only completed records are evicted; the eight live data workers
            // retain their records even if player probe connections churn.
            while stats
                .connections
                .iter()
                .filter(|record| !record.state.active())
                .count()
                >= RECENT_CONNECTIONS
            {
                let Some(index) = stats
                    .connections
                    .iter()
                    .position(|record| !record.state.active())
                else {
                    break;
                };
                stats.connections.remove(index);
            }
            stats.total_streams = stats.total_streams.saturating_add(1);
            stats.active_streams += 1;
            id = stats.total_streams;
            stats.connections.push(RelayConnectionStatistics {
                id,
                state: RelayConnectionState::Connecting,
                upstream_attempts: 0,
                reconnects: 0,
                consecutive_failures: 0,
                stable_segments: 0,
                forwarded_tags: 0,
                forwarded_bytes: 0,
                last_error: None,
                waiting_state: None,
                timeline_jumps: 0,
            });
        }
        let statistics = Self {
            store,
            id,
            log_dir,
        };
        statistics.log(diag::Level::Info, format!("connection #{id} opened"));
        statistics
    }

    fn log(&self, level: diag::Level, message: impl AsRef<str>) {
        relay_log(self.log_dir.as_deref(), level, message.as_ref());
    }

    fn update(&self, change: impl FnOnce(&mut RelayStatistics, &mut RelayConnectionStatistics)) {
        let mut stats = self.store.lock().unwrap_or_else(|error| error.into_inner());
        let Some(index) = stats
            .connections
            .iter()
            .position(|record| record.id == self.id)
        else {
            return;
        };
        // Take the bounded record out briefly to borrow aggregate and per-stream
        // counters without unsafe aliases. Order is restored before unlocking.
        let mut record = stats.connections.remove(index);
        change(&mut stats, &mut record);
        stats.connections.insert(index, record);
    }

    fn attempt(&self, reconnect: bool) {
        self.update(|stats, record| {
            stats.upstream_attempts = stats.upstream_attempts.saturating_add(1);
            record.upstream_attempts = record.upstream_attempts.saturating_add(1);
            if reconnect {
                stats.reconnects = stats.reconnects.saturating_add(1);
                record.reconnects = record.reconnects.saturating_add(1);
            }
            record.state = RelayConnectionState::Connecting;
            record.waiting_state = None;
        });
        if reconnect {
            self.log(
                diag::Level::Info,
                format!("connection #{} reconnect attempt", self.id),
            );
        }
    }

    fn forwarded(&self, bytes: usize, media: bool) {
        self.forwarded_batch(bytes, u64::from(media));
    }

    fn forwarded_batch(&self, bytes: usize, media_tags: u64) {
        self.update(|stats, record| {
            stats.forwarded_bytes = stats.forwarded_bytes.saturating_add(bytes as u64);
            record.forwarded_bytes = record.forwarded_bytes.saturating_add(bytes as u64);
            if media_tags > 0 {
                stats.forwarded_tags = stats.forwarded_tags.saturating_add(media_tags);
                record.forwarded_tags = record.forwarded_tags.saturating_add(media_tags);
                record.state = RelayConnectionState::Streaming;
            }
        });
    }

    fn stable(&self) {
        self.update(|stats, record| {
            stats.stable_segments = stats.stable_segments.saturating_add(1);
            record.stable_segments = record.stable_segments.saturating_add(1);
            record.consecutive_failures = 0;
            record.last_error = None;
        });
    }

    fn interrupted(&self, failure: RelayFailure, failures: usize, productive: bool) {
        self.update(|stats, record| {
            if !productive {
                stats.failures = stats.failures.saturating_add(1);
            }
            record.consecutive_failures = failures as u64;
            record.last_error = Some(failure);
            record.state = RelayConnectionState::Retrying;
        });
        if productive {
            self.log(
                diag::Level::Info,
                format!(
                    "connection #{} media segment ended, reconnecting (consecutive={failures})",
                    self.id
                ),
            );
        } else {
            self.log(
                diag::Level::Warn,
                format!(
                    "connection #{} upstream segment failed: {} (consecutive={failures})",
                    self.id,
                    failure.label()
                ),
            );
        }
    }

    fn finish(&self, state: RelayConnectionState, failure: Option<RelayFailure>) {
        self.update(|stats, record| {
            if state == RelayConnectionState::Exhausted {
                stats.budget_exhaustions = stats.budget_exhaustions.saturating_add(1);
            }
            record.state = state;
            record.waiting_state = None;
            if failure.is_some() {
                record.last_error = failure;
            }
        });
        let (level, message) = match state {
            RelayConnectionState::PlayerDisconnected => {
                (diag::Level::Info, "player disconnected, connection closed".to_owned())
            }
            RelayConnectionState::WriteFailed => (
                diag::Level::Warn,
                "player write failed, connection closed".to_owned(),
            ),
            RelayConnectionState::PolicyRejected => (
                diag::Level::Warn,
                format!(
                    "upstream policy rejected ({})",
                    failure.map_or("unknown", RelayFailure::label)
                ),
            ),
            RelayConnectionState::Exhausted => (
                diag::Level::Error,
                "reconnect budget exhausted, connection closed".to_owned(),
            ),
            RelayConnectionState::Ended => {
                (diag::Level::Info, "connection ended".to_owned())
            }
            _ => (
                diag::Level::Info,
                format!("connection #{} state {}", self.id, state.label()),
            ),
        };
        self.log(level, format!("connection #{}: {message}", self.id));
    }

    fn waiting(&self, state: &str) {
        self.update(|_, record| {
            record.state = RelayConnectionState::Waiting;
            record.waiting_state = Some(state.to_owned());
            record.last_error = None;
        });
        match state {
            "offline" => self.log(
                diag::Level::Info,
                "room offline; waiting for the next broadcast (player connection kept)",
            ),
            "replay_unavailable" => self.log(
                diag::Level::Warn,
                "replay feed unavailable; waiting before retrying (player connection kept)",
            ),
            _ => {}
        }
    }

    /// Record the stream's onMetaData summary once per relay process.
    fn stream_info(&self, info: RelayStreamInfo) {
        self.update(|stats, _| {
            if stats.stream_info.is_none() {
                stats.stream_info = Some(info);
            }
        });
    }

    /// One in-response media timeline discontinuity was rebased so the player
    /// clock stays continuous. The skipped content is gone (the CDN dropped it);
    /// the player must not rebuild its demuxer cache over the raw jump.
    fn timeline_jump(&self, raw_jump_ms: i64) {
        self.update(|stats, record| {
            stats.timeline_jumps = stats.timeline_jumps.saturating_add(1);
            record.timeline_jumps = record.timeline_jumps.saturating_add(1);
        });
        self.log(
            diag::Level::Warn,
            format!(
                "connection #{} media timeline jump {raw_jump_ms:+}ms rebased to keep the player clock continuous",
                self.id
            ),
        );
    }
}

impl Drop for StreamStatistics {
    fn drop(&mut self) {
        let mut closed_active = false;
        self.update(|stats, record| {
            stats.active_streams = stats.active_streams.saturating_sub(1);
            if record.state.active() {
                record.state = RelayConnectionState::Ended;
                closed_active = true;
            }
            record.waiting_state = None;
        });
        if closed_active {
            self.log(
                diag::Level::Info,
                format!("connection #{} closed", self.id),
            );
        }
    }
}

#[derive(Default)]
struct MediaProgress {
    tags: u64,
    bytes: u64,
    first: [Option<u32>; 2],
    last: [Option<u32>; 2],
    stable: bool,
}

impl MediaProgress {
    /// Returns true only on the first transition to a productive segment.
    fn observe(&mut self, tag: &flv::FlvTag) -> bool {
        if self.stable || tag.is_codec_header() || tag.payload().is_empty() {
            return false;
        }
        let slot = match tag.kind() {
            8 => 0,
            9 => 1,
            _ => return false,
        };
        self.tags = self.tags.saturating_add(1);
        self.bytes = self.bytes.saturating_add(tag.payload().len() as u64);
        let timestamp = tag.timestamp();
        self.first[slot].get_or_insert(timestamp);
        self.last[slot] = Some(self.last[slot].unwrap_or(timestamp).max(timestamp));
        let advancing = (0..2).any(|track| {
            self.first[track]
                .zip(self.last[track])
                .is_some_and(|(first, last)| last.saturating_sub(first) >= STABLE_MEDIA_SPAN_MS)
        });
        self.stable =
            self.tags >= STABLE_MEDIA_TAGS && self.bytes >= STABLE_MEDIA_BYTES && advancing;
        self.stable
    }
}

struct PlayerLifetime {
    required: bool,
    registration_deadline: Instant,
    process: Mutex<Option<WatchedProcess>>,
}

impl PlayerLifetime {
    fn register(&self, pid: u32) -> Result<()> {
        if !self.required {
            bail!("player registration is not enabled");
        }
        let mut process = self
            .process
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(current) = process.as_ref() {
            if current.pid == pid {
                return Ok(());
            }
            bail!("relay is already bound to another player");
        }
        let watched = WatchedProcess::open(pid)?;
        if !watched.is_running() {
            bail!("player has already exited");
        }
        *process = Some(watched);
        Ok(())
    }

    fn should_exit(&self) -> bool {
        let process = self
            .process
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match process.as_ref() {
            Some(process) => !process.is_running(),
            None => self.required && Instant::now() >= self.registration_deadline,
        }
    }

    fn registered(&self) -> bool {
        self.process
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some()
    }
}

/// An open Windows process handle identifies this process even if its numeric
/// PID is later reused. No termination or foreground-window rights are granted.
struct WatchedProcess {
    pid: u32,
    #[cfg(windows)]
    handle: usize,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
    fn WaitForSingleObject(handle: *mut std::ffi::c_void, milliseconds: u32) -> u32;
    fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
}

impl WatchedProcess {
    fn open(pid: u32) -> Result<Self> {
        if pid == 0 {
            bail!("player PID must be positive");
        }
        #[cfg(windows)]
        {
            // SYNCHRONIZE only. The spawning launcher still owns Child while
            // registration is acknowledged, so Windows cannot recycle this PID.
            let handle = unsafe { OpenProcess(0x0010_0000, 0, pid) };
            if handle.is_null() {
                return Err(io::Error::last_os_error()).context("cannot watch player process");
            }
            Ok(Self {
                pid,
                handle: handle as usize,
            })
        }
        #[cfg(not(windows))]
        bail!("independent player supervision currently requires Windows")
    }

    fn is_running(&self) -> bool {
        #[cfg(windows)]
        {
            unsafe { WaitForSingleObject(self.handle as *mut std::ffi::c_void, 0) == 258 }
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
}

impl Drop for WatchedProcess {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            CloseHandle(self.handle as *mut std::ffi::c_void);
        }
    }
}

impl RelayServer {
    /// Binding separately makes the zero-port handshake testable without a race.
    pub fn bind(config: RelayConfig, resolver: Arc<dyn StreamResolver>) -> Result<Self> {
        if !config.bind.ip().is_loopback() {
            bail!("relay must bind a loopback address");
        }
        if config.token.is_empty()
            || config.token.len() > 128
            || !config
                .token
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        {
            bail!("relay requires a nonempty URL-safe session token (maximum 128 bytes)");
        }
        if !(1..=8).contains(&config.max_connections) {
            bail!("relay connection budget must be between 1 and 8");
        }
        if config.idle_timeout.is_zero()
            || config.request_timeout.is_zero()
            || config.upstream_timeout.is_zero()
            || config.write_timeout.is_zero()
            || config.player_registration_timeout.is_zero()
        {
            bail!("relay timeouts must be positive");
        }
        let listener = TcpListener::bind(config.bind).context("bind relay")?;
        listener.set_nonblocking(true)?;
        let player_lifetime = Arc::new(PlayerLifetime {
            required: config.require_player_registration,
            registration_deadline: Instant::now() + config.player_registration_timeout,
            process: Mutex::new(None),
        });
        Ok(Self {
            listener,
            config,
            resolver,
            player_lifetime,
            statistics: Arc::new(Mutex::new(RelayStatistics::default())),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub fn statistics(&self) -> RelayStatistics {
        self.statistics
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn run(self) -> Result<()> {
        let active = Arc::new(AtomicUsize::new(0));
        let last_client = Arc::new(Mutex::new(Instant::now()));
        loop {
            if self.player_lifetime.should_exit() {
                relay_log(
                    self.config.log_dir.as_deref(),
                    diag::Level::Info,
                    "player lifetime ended; relay stopping",
                );
                return Ok(());
            }
            match self.listener.accept() {
                Ok((mut stream, peer)) => {
                    // The listener is non-blocking so the loop can enforce its idle
                    // deadline. Windows inherits that mode into accepted sockets
                    // (unlike Unix accept), while every relay path below assumes
                    // blocking I/O bounded by socket timeouts. Without this reset an
                    // idle player socket fails its first read immediately and the
                    // connection budget is released before the player is rejected.
                    stream
                        .set_nonblocking(false)
                        .context("configure relay connection")?;
                    if !peer.ip().is_loopback() {
                        continue;
                    }
                    if active.load(Ordering::Acquire) >= self.config.max_connections {
                        reject_unread(&mut stream, "503 Service Unavailable");
                        continue;
                    }
                    active.fetch_add(1, Ordering::AcqRel);
                    *last_client.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
                    let guard = ActiveConnection {
                        active: Arc::clone(&active),
                        last_client: Arc::clone(&last_client),
                    };
                    let config = self.config.clone();
                    let resolver = Arc::clone(&self.resolver);
                    let player_lifetime = Arc::clone(&self.player_lifetime);
                    let statistics = Arc::clone(&self.statistics);
                    thread::Builder::new()
                        .name("flv-relay-client".into())
                        .spawn(move || {
                            let _guard = guard;
                            // User/platform URLs or headers must not appear in errors.
                            if let Err(error) = pump(
                                &mut stream,
                                &config,
                                resolver.as_ref(),
                                &player_lifetime,
                                statistics,
                            ) {
                                relay_log(
                                    config.log_dir.as_deref(),
                                    diag::Level::Warn,
                                    &format!(
                                        "relay client stopped: {}",
                                        safe_error_kind(&error)
                                    ),
                                );
                            }
                        })
                        .context("start relay worker")?;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    // Check the idle deadline only after the listener has no pending
                    // connection. If the accept loop was descheduled past the deadline
                    // while a player connected, that player is served instead of being
                    // dropped by an otherwise needless shutdown. This keeps the
                    // connection-budget contract deterministic under heavy load.
                    if !self.player_lifetime.registered()
                        && active.load(Ordering::Acquire) == 0
                        && last_client
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .elapsed()
                            >= self.config.idle_timeout
                    {
                        relay_log(
                            self.config.log_dir.as_deref(),
                            diag::Level::Info,
                            "idle deadline reached; relay stopping",
                        );
                        return Ok(());
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("accept relay connection"),
            }
        }
    }
}

struct ActiveConnection {
    active: Arc<AtomicUsize>,
    last_client: Arc<Mutex<Instant>>,
}
impl Drop for ActiveConnection {
    fn drop(&mut self) {
        *self.last_client.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The only stdout protocol line; it is emitted before any upstream resolution.
pub fn serve(config: RelayConfig, resolver: Arc<dyn StreamResolver>) -> Result<()> {
    let server = RelayServer::bind(config, resolver)?;
    let log_dir = server.config.log_dir.clone();
    {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        writeln!(stdout, "READY {}", server.local_addr()?.port())?;
        stdout.flush()?;
    }
    relay_log(
        log_dir.as_deref(),
        diag::Level::Info,
        &format!("relay listening on loopback port {}", server.local_addr()?.port()),
    );
    let result = server.run();
    match &result {
        Ok(()) => relay_log(log_dir.as_deref(), diag::Level::Info, "relay stopped"),
        Err(error) => relay_log(
            log_dir.as_deref(),
            diag::Level::Error,
            &format!("relay stopped with error: {}", safe_error_kind(error)),
        ),
    }
    result
}

fn safe_error_kind(error: &anyhow::Error) -> &'static str {
    if let Some(policy) = error.downcast_ref::<UpstreamPolicy>() {
        policy.0
    } else if error.downcast_ref::<io::Error>().is_some() {
        "I/O error"
    } else {
        "upstream unavailable"
    }
}

#[derive(Debug)]
struct UpstreamPolicy(&'static str);
impl std::fmt::Display for UpstreamPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}
impl std::error::Error for UpstreamPolicy {}

pub(crate) fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let b = ip.octets();
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_multicast()
                && !ip.is_unspecified()
                && !ip.is_broadcast()
                && !ip.is_documentation()
                && b[0] != 0
                && b[0] < 224
                && !(b[0] == 100 && (64..128).contains(&b[1]))
                && !(b[0] == 192 && b[1] == 0)
                && !(b[0] == 198 && (b[1] == 18 || b[1] == 19))
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            // Global-unicast range only; exclude documentation and embedded IPv4.
            s[0] & 0xe000 == 0x2000
                && !(s[0] == 0x2001 && s[1] == 0x0db8)
                && !(s[0] == 0x2001 && s[1] == 2)
                && ip.to_ipv4_mapped().is_none()
        }
    }
}

/// Local TUN proxies (e.g. Clash fake-IP mode) hand out 198.18.0.0/15 answers
/// for every hostname. The TLS peer identity still has to match the CDN host, so
/// dialling a proxy-mapped address does not bypass the upstream CDN allowlist.
fn is_proxy_fake_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            octets[0] == 198 && (octets[1] & 0xfe) == 18
        }
        IpAddr::V6(_) => false,
    }
}

fn forwarding_headers(headers: &std::collections::BTreeMap<String, String>) -> Result<HeaderMap> {
    let mut output = HeaderMap::new();
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if !matches!(lower.as_str(), "user-agent" | "referer" | "origin") {
            return Err(UpstreamPolicy(
                "upstream headers outside User-Agent/Referer/Origin are unsupported",
            )
            .into());
        }
        let name = HeaderName::from_bytes(lower.as_bytes())
            .map_err(|_| UpstreamPolicy("invalid upstream header"))?;
        let value =
            HeaderValue::from_str(value).map_err(|_| UpstreamPolicy("invalid upstream header"))?;
        output.insert(name, value);
    }
    Ok(output)
}

const MAX_MEDIA_REDIRECTS: usize = 5;
const MAX_DNS_LOOKUPS: usize = 8;
static DNS_LOOKUPS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

struct DnsLookupSlot;
impl Drop for DnsLookupSlot {
    fn drop(&mut self) {
        DNS_LOOKUPS_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
    }
}

fn public_media_addresses(host: &str, port: u16, timeout: Duration) -> Result<Vec<SocketAddr>> {
    let mut count = DNS_LOOKUPS_IN_FLIGHT.load(Ordering::Acquire);
    loop {
        if count >= MAX_DNS_LOOKUPS {
            bail!("upstream DNS resolver is busy");
        }
        match DNS_LOOKUPS_IN_FLIGHT.compare_exchange_weak(
            count,
            count + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(current) => count = current,
        }
    }
    let slot = DnsLookupSlot;
    let name = host.to_owned();
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("relay-upstream-dns".into())
        .spawn(move || {
            let _slot = slot;
            let result = (name.as_str(), port).to_socket_addrs().map(|answers| {
                answers
                    .filter(|address| is_public(address.ip()) || is_proxy_fake_ip(address.ip()))
                    .take(16)
                    .collect::<Vec<_>>()
            });
            let _ = sender.send(result);
        })
        .map_err(|_| anyhow::anyhow!("upstream DNS resolver could not start"))?;
    let addresses = receiver
        .recv_timeout(timeout)
        .map_err(|_| anyhow::anyhow!("upstream DNS lookup timed out"))?
        .map_err(|_| anyhow::anyhow!("upstream DNS lookup failed"))?;
    if addresses.is_empty() {
        return Err(UpstreamPolicy("upstream DNS contains no permitted public address").into());
    }
    Ok(addresses)
}

fn redirect_target(current: &url::Url, location: &str) -> Result<url::Url> {
    if location.is_empty() || location.len() > 4096 {
        return Err(UpstreamPolicy("invalid upstream redirect target").into());
    }
    let target = current
        .join(location)
        .map_err(|_| UpstreamPolicy("invalid upstream redirect target"))?;
    // validate_media_url upgrades legacy initial HTTP URLs, but a redirect must
    // never downgrade an already validated HTTPS media connection.
    if target.scheme() != "https" {
        return Err(UpstreamPolicy("upstream redirect must preserve HTTPS").into());
    }
    crate::platforms::validate_media_url(target.as_str())
        .map_err(|_| UpstreamPolicy("upstream redirect is not an approved public CDN").into())
}

fn next_redirect(
    current: &url::Url,
    location: &str,
    visited: &mut std::collections::HashSet<String>,
    followed: usize,
) -> Result<url::Url> {
    if followed >= MAX_MEDIA_REDIRECTS {
        return Err(UpstreamPolicy("upstream redirect limit exceeded").into());
    }
    let next = redirect_target(current, location)?;
    if !visited.insert(next.as_str().to_owned()) {
        return Err(UpstreamPolicy("upstream redirect loop rejected").into());
    }
    Ok(next)
}

fn open_upstream(resolved: &ResolvedStream, config: &RelayConfig) -> Result<Response> {
    // The development escape hatch accepts literal loopback addresses only,
    // so enabling fixture mode never turns off validation for other addresses.
    let parsed = url::Url::parse(&resolved.url)
        .map_err(|_| UpstreamPolicy("invalid upstream media address"))?;
    let fixture = config.allow_loopback_fixture
        && match parsed.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
    let mut url = if fixture {
        if !matches!(parsed.scheme(), "http" | "https")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.fragment().is_some()
        {
            return Err(UpstreamPolicy("invalid loopback fixture address").into());
        }
        parsed
    } else {
        crate::platforms::validate_media_url(&resolved.url)
            .map_err(|_| UpstreamPolicy("upstream media address is not an approved public CDN"))?
    };
    let headers = forwarding_headers(&resolved.headers)?;
    let mut visited = std::collections::HashSet::new();
    visited.insert(url.as_str().to_owned());
    for hop in 0..=MAX_MEDIA_REDIRECTS {
        let host = url
            .host_str()
            .ok_or(UpstreamPolicy("upstream address lacks host"))?;
        let port = url
            .port_or_known_default()
            .ok_or(UpstreamPolicy("upstream address lacks port"))?;
        let addresses: Vec<SocketAddr> = if fixture {
            let ip = match url.host().unwrap() {
                url::Host::Ipv4(ip) => IpAddr::V4(ip),
                url::Host::Ipv6(ip) => IpAddr::V6(ip),
                _ => unreachable!(),
            };
            vec![SocketAddr::new(ip, port)]
        } else {
            // Each hop has independently approved CDN host, bounded DNS and an
            // exact pinned address set; redirects cannot evade the same policy.
            public_media_addresses(host, port, config.upstream_timeout)?
        };
        // A fresh client per hop prevents pooled connections or a second DNS
        // lookup from evading the validated address set. Blocking response reads
        // each have their own timeout, permitting long-lived healthy streams.
        let client = Client::builder()
            .no_proxy()
            .min_tls_version(reqwest::tls::Version::TLS_1_2)
            .connect_timeout(config.upstream_timeout)
            .timeout(config.upstream_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(host, &addresses)
            .build()
            .map_err(|_| anyhow::anyhow!("upstream HTTP client initialization failed"))?;
        let response = client
            .get(url.clone())
            .headers(headers.clone())
            .send()
            .map_err(|_| anyhow::anyhow!("upstream request failed"))?;
        if !response.status().is_redirection() {
            return response
                .error_for_status()
                .map_err(|_| anyhow::anyhow!("upstream HTTP status rejected"));
        }
        if fixture {
            return Err(
                UpstreamPolicy("loopback fixtures cannot follow upstream redirects").into(),
            );
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(UpstreamPolicy("upstream redirect lacks a valid location"))?;
        url = next_redirect(&url, location, &mut visited, hop)?;
    }
    unreachable!("bounded redirect loop always returns")
}

/// Open one validated upstream media response for the diagnostic streaminfo
/// command. Uses the same CDN allowlist, redirect policy and pinned DNS lookup
/// as the relay; no listener, token or player connection is involved.
pub fn open_media_stream(resolved: &ResolvedStream) -> Result<Response> {
    open_upstream(resolved, &RelayConfig::default())
}

fn reject(stream: &mut TcpStream, status: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    )?;
    stream.flush()
}

/// Send a terminal rejection to a connection that has not been read, then close
/// it without a TCP reset overtaking the status line.
///
/// Windows sends RST instead of FIN when a socket is closed while its receive
/// buffer still holds unread peer data. The connection-budget rejection happens
/// before the request line is read, so a plain close can make the player observe
/// "connection reset" instead of the 503 that explains the refusal. Half-closing
/// the write side after the response and draining the already-arrived request
/// bytes keeps the status observable; the drain is bounded by a short timeout.
fn reject_unread(stream: &mut TcpStream, status: &str) {
    let _ = stream.set_write_timeout(Some(Duration::from_millis(50)));
    if reject(stream, status).is_err() {
        return;
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    let mut sink = [0_u8; 512];
    let _ = stream.read(&mut sink);
}

fn read_request(stream: &mut TcpStream, deadline: Duration) -> io::Result<String> {
    let start = Instant::now();
    let mut bytes = Vec::with_capacity(1024);
    loop {
        let remaining = deadline
            .checked_sub(start.elapsed())
            .ok_or_else(|| io::Error::new(ErrorKind::TimedOut, "request deadline"))?;
        stream.set_read_timeout(Some(remaining))?;
        let mut chunk = [0; 512];
        let count = stream.read(&mut chunk)?;
        if count == 0 {
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "request incomplete",
            ));
        }
        if bytes.len() + count > 8192 {
            return Err(io::Error::new(ErrorKind::InvalidData, "request too large"));
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
            return String::from_utf8(bytes)
                .map_err(|_| io::Error::new(ErrorKind::InvalidData, "invalid HTTP request"));
        }
    }
}

fn request_query<'a>(request: &'a str, method: &str, path: &str) -> Option<&'a str> {
    let mut parts = request.lines().next().unwrap_or("").split_whitespace();
    if parts.next() != Some(method) {
        return None;
    }
    let target = parts.next()?;
    if !matches!(parts.next(), Some("HTTP/1.0" | "HTTP/1.1")) || parts.next().is_some() {
        return None;
    }
    let (actual_path, query) = target.split_once('?')?;
    (actual_path == path).then_some(query)
}

fn token_matches(query: &str, expected: &str) -> bool {
    let mut tokens = url::form_urlencoded::parse(query.as_bytes()).filter(|(key, _)| key == "t");
    let Some((_, token)) = tokens.next() else {
        return false;
    };
    if tokens.next().is_some() || token.len() != expected.len() {
        return false;
    }
    token
        .bytes()
        .zip(expected.bytes())
        .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

fn authorized(request: &str, expected: &str) -> bool {
    request_query(request, "GET", "/live.flv").is_some_and(|query| token_matches(query, expected))
}

/// Bind the player's lifetime to its own relay. This round trip completes before
/// launch returns; it sends only the PID and existing session token over loopback.
pub fn register_player(relay_url: &str, player_pid: u32) -> Result<()> {
    if player_pid == 0 {
        bail!("播放器进程号无效");
    }
    let url = url::Url::parse(relay_url).context("中转地址无效")?;
    if url.scheme() != "http"
        || url.path() != "/live.flv"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        bail!("生命周期绑定只能连接本机中转");
    }
    let ip = match url.host() {
        Some(url::Host::Ipv4(ip)) => IpAddr::V4(ip),
        Some(url::Host::Ipv6(ip)) => IpAddr::V6(ip),
        _ => bail!("生命周期绑定需要本机回环地址"),
    };
    if !ip.is_loopback() {
        bail!("生命周期绑定需要本机回环地址");
    }
    let port = url.port().context("中转端口缺失")?;
    let tokens: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| key == "t")
        .map(|(_, value)| value.into_owned())
        .collect();
    if tokens.len() != 1
        || tokens[0].is_empty()
        || tokens[0].len() > 128
        || !tokens[0]
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        bail!("中转会话令牌无效");
    }
    let timeout = Duration::from_secs(5);
    let mut stream = TcpStream::connect_timeout(&SocketAddr::new(ip, port), timeout)
        .context("无法连接中转生命周期通道")?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write!(
        stream,
        "POST /__livehub/watch?t={}&pid={player_pid} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        tokens[0]
    )?;
    let mut status = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(stream).take(1024), &mut status)?;
    if status.trim() != "HTTP/1.1 204 No Content" {
        bail!("中转未能绑定播放器生命周期");
    }
    Ok(())
}

/// Read a bounded, token-authenticated statistics snapshot from this relay only.
/// This never follows redirects, uses a proxy, or exposes the session token in
/// an error. Busy relays may reject the short-lived control request with 503.
pub fn query_statistics(relay_url: &str) -> Result<RelayStatistics> {
    let url =
        url::Url::parse(relay_url).map_err(|_| anyhow::anyhow!("invalid local relay address"))?;
    if url.scheme() != "http"
        || url.path() != "/live.flv"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        bail!("statistics require a local relay address");
    }
    let ip = match url.host() {
        Some(url::Host::Ipv4(ip)) => IpAddr::V4(ip),
        Some(url::Host::Ipv6(ip)) => IpAddr::V6(ip),
        _ => bail!("statistics require a loopback IP address"),
    };
    if !ip.is_loopback() {
        bail!("statistics require a loopback IP address");
    }
    let port = url.port().context("local relay port is missing")?;
    let tokens: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| key == "t")
        .map(|(_, value)| value.into_owned())
        .collect();
    if tokens.len() != 1
        || tokens[0].is_empty()
        || tokens[0].len() > 128
        || !tokens[0]
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        bail!("invalid relay statistics token");
    }
    let timeout = Duration::from_secs(3);
    let mut stream = TcpStream::connect_timeout(&SocketAddr::new(ip, port), timeout)
        .context("local relay statistics unavailable")?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write!(
        stream,
        "GET /__livehub/status?t={} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        tokens[0]
    )?;
    let mut bytes = Vec::new();
    stream.take(65_537).read_to_end(&mut bytes)?;
    if bytes.len() > 65_536 {
        bail!("relay statistics response exceeds limit");
    }
    let boundary = bytes
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .context("invalid relay statistics response")?;
    let header =
        std::str::from_utf8(&bytes[..boundary]).context("invalid relay statistics header")?;
    if !header.starts_with("HTTP/1.1 200 OK\r\n") {
        bail!("relay statistics unavailable or not authorized");
    }
    serde_json::from_slice(&bytes[boundary + 4..]).context("invalid relay statistics payload")
}

/// Write one coalesced batch to the player and account it atomically.
fn flush_player(
    stream: &mut TcpStream,
    statistics: &StreamStatistics,
    write_buffer: &mut Vec<u8>,
    pending_media_tags: &mut u64,
) -> io::Result<()> {
    if write_buffer.is_empty() {
        return Ok(());
    }
    stream.write_all(write_buffer)?;
    statistics.forwarded_batch(write_buffer.len(), *pending_media_tags);
    write_buffer.clear();
    *pending_media_tags = 0;
    Ok(())
}

fn client_gone(stream: &TcpStream) -> bool {
    // peek does not consume player bytes. This is checked before every resolve
    // and every complete tag; a stalled upstream is bounded by upstream_timeout.
    //
    // Poll with a non-blocking peek instead of a short blocking read timeout:
    // Windows rounds socket timeouts up to the system timer granularity (about
    // 15.6 ms), so a 1 ms blocking peek before every relayed tag both waited a
    // full timer tick and capped forwarding at roughly 15 tags/second. A live
    // FLV stream needs well over 50 tags/second (video and audio interleaved),
    // so that polling style made the relay fall permanently behind. The mode
    // flip is cheap (FIONBIO) and scoped to this one connection thread.
    let _ = stream.set_nonblocking(true);
    let mut byte = [0];
    let gone = match stream.peek(&mut byte) {
        Ok(0) => true,
        Ok(_) => false,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
            ) =>
        {
            false
        }
        Err(_) => true,
    };
    let _ = stream.set_nonblocking(false);
    gone
}

fn wait_retry(stream: &TcpStream, delay: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < delay {
        if client_gone(stream) {
            return false;
        }
        thread::sleep(
            delay
                .saturating_sub(start.elapsed())
                .min(Duration::from_millis(50)),
        );
    }
    !client_gone(stream)
}

/// Upper bound on the onMetaData payload the relay is willing to parse.
const MAX_METADATA_BYTES: usize = 64 * 1024;

fn codec_from_id(id: u32) -> Option<String> {
    match id {
        2 => Some("h263".to_owned()),
        4 => Some("vp6".to_owned()),
        7 => Some("h264".to_owned()),
        12 => Some("hevc".to_owned()),
        _ => None,
    }
}

fn codec_from_name(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    if lower.contains("avc") || lower.contains("h264") {
        Some("h264".to_owned())
    } else if lower.contains("hev") || lower.contains("h265") {
        Some("hevc".to_owned())
    } else {
        None
    }
}

/// Parse an FLV onMetaData AMF0 script payload into the bounded summary the mpv
/// title needs. This mirrors the diagnostic stream parser in flv.rs but works on
/// the tag the relay already read, so the launch-time hints cost no extra
/// connection. Unsupported AMF0 kinds end the scan instead of guessing.
fn parse_metadata(payload: &[u8]) -> Option<RelayStreamInfo> {
    if payload.first() != Some(&0x02) {
        return None;
    }
    let mut p = 1usize;
    let name_len = usize::from(u16::from_be_bytes([*payload.get(p)?, *payload.get(p + 1)?]));
    p += 2 + name_len;
    let table = *payload.get(p)?;
    p += match table {
        0x08 => 5,
        0x03 => 1,
        _ => return None,
    };
    let mut info = RelayStreamInfo::default();
    let mut found = false;
    while p + 3 <= payload.len() {
        if payload[p..p + 3] == [0, 0, 9] {
            break;
        }
        let key_len = usize::from(u16::from_be_bytes([payload[p], payload[p + 1]]));
        p += 2;
        if key_len == 0 || p + key_len >= payload.len() {
            break;
        }
        let key = String::from_utf8_lossy(&payload[p..p + key_len]).into_owned();
        p += key_len;
        let marker = payload[p];
        p += 1;
        match marker {
            0x00 => {
                if p + 8 > payload.len() {
                    break;
                }
                let value = f64::from_be_bytes(payload[p..p + 8].try_into().unwrap());
                p += 8;
                match key.as_str() {
                    "width" => {
                        info.width = value.max(0.0).round() as u32;
                        found = true;
                    }
                    "height" => {
                        info.height = value.max(0.0).round() as u32;
                        found = true;
                    }
                    "framerate" => {
                        info.fps = value.max(0.0);
                        found = true;
                    }
                    "videodatarate" => {
                        info.video_kbps = value.max(0.0).round() as u32;
                        found = true;
                    }
                    "videocodecid" => {
                        info.codec = codec_from_id(value.max(0.0).round() as u32);
                        found |= info.codec.is_some();
                    }
                    _ => {}
                }
            }
            0x01 => p += 1,
            0x02 => {
                if p + 2 > payload.len() {
                    break;
                }
                let len = usize::from(u16::from_be_bytes([payload[p], payload[p + 1]]));
                if p + 2 + len > payload.len() {
                    break;
                }
                if key.as_str() == "codecname" && info.codec.is_none() {
                    info.codec = codec_from_name(&String::from_utf8_lossy(
                        &payload[p + 2..p + 2 + len],
                    ));
                }
                p += 2 + len;
            }
            0x0c => {
                if p + 4 > payload.len() {
                    break;
                }
                let len = u32::from_be_bytes(payload[p..p + 4].try_into().unwrap()) as usize;
                if p + 4 + len > payload.len() {
                    break;
                }
                p += 4 + len;
            }
            0x05 | 0x06 => {}
            0x0b => p += 10,
            _ => break,
        }
    }
    (found || info.codec.is_some()).then_some(info)
}

/// Player liveness is no longer polled from inside every socket read: the old
/// wrapper cost three syscalls per read (non-blocking flip, peek, flip back) and
/// a single 1 KiB tag is read in at least three calls. A player write failure is
/// immediate, and the forwarding loop polls a closed player on a fixed interval
/// so a stalled upstream still ends the session promptly.
fn pump(
    stream: &mut TcpStream,
    config: &RelayConfig,
    resolver: &dyn StreamResolver,
    lifetime: &PlayerLifetime,
    statistics: Arc<Mutex<RelayStatistics>>,
) -> Result<()> {
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(config.write_timeout))?;
    let request = match read_request(stream, config.request_timeout) {
        Ok(request) => request,
        Err(_) => {
            let _ = reject(stream, "400 Bad Request");
            return Ok(());
        }
    };
    if let Some(query) = request_query(&request, "POST", "/__livehub/watch") {
        if !token_matches(query, &config.token) {
            reject(stream, "403 Forbidden")?;
            return Ok(());
        }
        let pids: Vec<_> = url::form_urlencoded::parse(query.as_bytes())
            .filter(|(key, _)| key == "pid")
            .map(|(_, value)| value.parse::<u32>())
            .collect();
        let pid = match pids.as_slice() {
            [Ok(pid)] if *pid > 0 => *pid,
            _ => {
                reject(stream, "400 Bad Request")?;
                return Ok(());
            }
        };
        let status = if lifetime.register(pid).is_ok() {
            "204 No Content"
        } else {
            "409 Conflict"
        };
        relay_log(
            config.log_dir.as_deref(),
            if status == "204 No Content" {
                diag::Level::Info
            } else {
                diag::Level::Warn
            },
            &format!("player lifetime registration pid={pid}: {status}"),
        );
        reject(stream, status)?;
        return Ok(());
    }
    if let Some(query) = request_query(&request, "GET", "/__livehub/status") {
        if !token_matches(query, &config.token) {
            reject(stream, "403 Forbidden")?;
            return Ok(());
        }
        let snapshot = statistics
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let body = serde_json::to_vec(&snapshot)?;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
            body.len()
        )?;
        stream.write_all(&body)?;
        return Ok(());
    }
    if !authorized(&request, &config.token) {
        let mut snapshot = statistics.lock().unwrap_or_else(|error| error.into_inner());
        snapshot.rejected_requests = snapshot.rejected_requests.saturating_add(1);
        drop(snapshot);
        relay_log(
            config.log_dir.as_deref(),
            diag::Level::Warn,
            "rejected relay request (missing or mismatched session token)",
        );
        reject(stream, "403 Forbidden")?;
        return Ok(());
    }
    {
        let mut snapshot = statistics.lock().unwrap_or_else(|error| error.into_inner());
        snapshot.media_get_requests = snapshot.media_get_requests.saturating_add(1);
    }
    let statistics = StreamStatistics::begin(statistics, config.log_dir.clone());
    // A streaming response has neither Content-Length nor chunked encoding.
    if let Err(error) = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: video/x-flv\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n") {
        statistics.finish(RelayConnectionState::WriteFailed, Some(RelayFailure::PlayerWriteFailed));
        return Err(error.into());
    }
    let mut timeline = ReconnectTimeline::default();
    let mut header_sent = false;
    let mut reconnect = false;
    let mut consecutive_failures = 0usize;
    // A productive segment that ends is a normal CDN boundary, not a failure:
    // reconnect quickly so the player is not starved for the whole backoff.
    let mut last_segment_stable = false;
    let mut reported_jumps = 0_u64;
    // The stream's onMetaData tag arrives immediately after the FLV header; read it
    // once so the title hints can show a bitrate without a second upstream probe.
    let mut metadata_checked = false;
    'attempts: loop {
        if client_gone(stream) {
            statistics.finish(
                RelayConnectionState::PlayerDisconnected,
                Some(RelayFailure::PlayerDisconnected),
            );
            return Ok(());
        }
        if reconnect {
            timeline.begin_reconnect();
            let reconnect_delay = if last_segment_stable {
                config.retry_delay.min(MEDIA_SEGMENT_RETRY_DELAY)
            } else {
                config.retry_delay
            };
            if !wait_retry(stream, reconnect_delay.max(MIN_RETRY_DELAY)) {
                statistics.finish(
                    RelayConnectionState::PlayerDisconnected,
                    Some(RelayFailure::PlayerDisconnected),
                );
                return Ok(());
            }
        }
        statistics.attempt(reconnect);
        reconnect = true;
        let mut progress = MediaProgress::default();
        let segment_failure = 'segment: {
            let resolved = match resolver.resolve() {
                Ok(resolved) => resolved,
                Err(error) => {
                    if let Some(waiting) = error.downcast_ref::<ResolveWaiting>() {
                        if matches!(waiting.state.as_str(), "offline" | "replay_unavailable") {
                            statistics.waiting(&waiting.state);
                            let delay = waiting
                                .retry_after
                                .clamp(Duration::from_secs(1), Duration::from_secs(60));
                            if !wait_retry(stream, delay) {
                                statistics.finish(
                                    RelayConnectionState::PlayerDisconnected,
                                    Some(RelayFailure::PlayerDisconnected),
                                );
                                return Ok(());
                            }
                            // This known room state is not a failed media retry.
                            // Keep timeline watermarks and its pending catch-up,
                            // but do not add another generic retry delay.
                            reconnect = false;
                            continue 'attempts;
                        }
                    }
                    break 'segment RelayFailure::ResolveFailed;
                }
            };
            if client_gone(stream) {
                statistics.finish(
                    RelayConnectionState::PlayerDisconnected,
                    Some(RelayFailure::PlayerDisconnected),
                );
                return Ok(());
            }
            let response = match open_upstream(&resolved, config) {
                Ok(response) => response,
                Err(error) if error.downcast_ref::<UpstreamPolicy>().is_some() => {
                    statistics.finish(
                        RelayConnectionState::PolicyRejected,
                        Some(RelayFailure::UpstreamPolicyRejected),
                    );
                    return Err(error);
                }
                Err(_) => break 'segment RelayFailure::ConnectFailed,
            };
            let mut upstream = BufReader::with_capacity(UPSTREAM_READ_BUFFER, response);
            let header = match flv::read_header(&mut upstream) {
                Ok(header) => header,
                Err(_) => break 'segment RelayFailure::InvalidFlv,
            };
            if !header_sent {
                // A player write error always ends the session, never a retry.
                if let Err(error) = stream.write_all(&header) {
                    statistics.finish(
                        RelayConnectionState::WriteFailed,
                        Some(RelayFailure::PlayerWriteFailed),
                    );
                    return Err(error.into());
                }
                statistics.forwarded(header.len(), false);
                header_sent = true;
            }
            let mut tag_buffer = Vec::new();
            let mut write_buffer = Vec::with_capacity(WRITE_BATCH_BYTES * 2);
            let mut pending_media_tags = 0_u64;
            let mut next_player_poll = Instant::now();
            loop {
                if Instant::now() >= next_player_poll {
                    next_player_poll = Instant::now() + PLAYER_POLL_INTERVAL;
                    if client_gone(stream) {
                        statistics.finish(
                            RelayConnectionState::PlayerDisconnected,
                            Some(RelayFailure::PlayerDisconnected),
                        );
                        return Ok(());
                    }
                }
                let mut tag = match flv::read_tag_reusing(&mut upstream, &mut tag_buffer) {
                    Ok(Some(tag)) => {
                        if !metadata_checked {
                            metadata_checked = true;
                            if tag.kind() == 18 && tag.payload().len() <= MAX_METADATA_BYTES {
                                if let Some(info) = parse_metadata(tag.payload()) {
                                    statistics.stream_info(info);
                                }
                            }
                        }
                        tag
                    }
                    Ok(None) => {
                        if let Err(error) = flush_player(
                            stream,
                            &statistics,
                            &mut write_buffer,
                            &mut pending_media_tags,
                        ) {
                            statistics.finish(
                                RelayConnectionState::WriteFailed,
                                Some(RelayFailure::PlayerWriteFailed),
                            );
                            return Err(error.into());
                        }
                        break 'segment RelayFailure::UpstreamEof;
                    }
                    Err(_) => {
                        let _ = flush_player(
                            stream,
                            &statistics,
                            &mut write_buffer,
                            &mut pending_media_tags,
                        );
                        break 'segment RelayFailure::ReadFailed;
                    }
                };
                if timeline.accept(&mut tag) {
                    if timeline.jumps() != reported_jumps {
                        reported_jumps = timeline.jumps();
                        statistics.timeline_jump(timeline.last_jump_ms());
                    }
                    if matches!(tag.kind(), 8 | 9) && !tag.is_codec_header() {
                        pending_media_tags = pending_media_tags.saturating_add(1);
                    }
                    write_buffer.extend_from_slice(tag.as_bytes());
                    if progress.observe(&tag) {
                        consecutive_failures = 0;
                        statistics.stable();
                    }
                }
                // Recycle the framing allocation (the common case); a rare
                // very large tag is freed instead of pinning its buffer.
                if tag.as_bytes().len() <= TAG_RECYCLE_LIMIT {
                    tag_buffer = tag.into_bytes();
                }
                if write_buffer.len() >= WRITE_BATCH_BYTES || upstream.buffer().is_empty() {
                    if let Err(error) = flush_player(
                        stream,
                        &statistics,
                        &mut write_buffer,
                        &mut pending_media_tags,
                    ) {
                        statistics.finish(
                            RelayConnectionState::WriteFailed,
                            Some(RelayFailure::PlayerWriteFailed),
                        );
                        return Err(error.into());
                    }
                }
            }
        };
        if progress.stable {
            consecutive_failures = 0;
        } else {
            consecutive_failures = consecutive_failures.saturating_add(1);
        }
        last_segment_stable = progress.stable;
        statistics.interrupted(segment_failure, consecutive_failures, progress.stable);
        if config.max_reconnects == 0 {
            statistics.finish(RelayConnectionState::Ended, None);
            return Ok(());
        }
        if consecutive_failures > config.max_reconnects {
            statistics.finish(
                RelayConnectionState::Exhausted,
                Some(RelayFailure::RetryBudgetExhausted),
            );
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_wrong_method_path_token_and_duplicate_tokens() {
        for target in [
            "/live.flv",
            "/other?t=secret",
            "/live.flv?t=wrong",
            "/live.flv?t=secret&t=wrong",
        ] {
            assert!(!authorized(
                &format!("GET {target} HTTP/1.1\r\n\r\n"),
                "secret"
            ));
        }
        assert!(!authorized(
            "POST /live.flv?t=secret HTTP/1.1\r\n\r\n",
            "secret"
        ));
        assert!(authorized(
            "GET /live.flv?t=secret HTTP/1.1\r\n\r\n",
            "secret"
        ));
    }

    #[test]
    fn dns_policy_excludes_local_reserved_and_mapped_addresses() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "198.18.0.1",
            "192.0.2.1",
            "224.0.0.1",
            "0.0.0.0",
            "::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_public(address.parse().unwrap()), "{address}");
        }
        assert!(is_public("8.8.8.8".parse().unwrap()));
        assert!(is_public("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn redirect_targets_are_revalidated_and_cannot_downgrade_or_escape_the_cdn() {
        let current = url::Url::parse("https://a.bilivideo.com/live.flv?synthetic=1").unwrap();
        assert_eq!(
            redirect_target(&current, "https://b.bilivideo.com/media.flv")
                .unwrap()
                .host_str(),
            Some("b.bilivideo.com")
        );
        assert_eq!(
            redirect_target(&current, "/next.flv?synthetic=2")
                .unwrap()
                .host_str(),
            Some("a.bilivideo.com")
        );
        for target in [
            "http://b.bilivideo.com/live.flv",
            "https://127.0.0.1/live.flv",
            "https://10.0.0.1/live.flv",
            "https://bilivideo.com.attacker.invalid/live.flv",
            "https://user:password@b.bilivideo.com/live.flv",
            "file:///private/media.flv",
        ] {
            assert!(
                redirect_target(&current, target).is_err(),
                "accepted redirect {target}"
            );
        }
    }

    #[test]
    fn redirects_reject_cycles_and_allow_at_most_five_validated_hops() {
        let initial = url::Url::parse("https://a.bilivideo.com/live.flv").unwrap();
        let mut visited = std::collections::HashSet::from([initial.as_str().to_owned()]);
        let next = next_redirect(&initial, "/next.flv", &mut visited, 0).unwrap();
        assert!(next_redirect(&next, initial.as_str(), &mut visited, 1).is_err());
        let mut visited = std::collections::HashSet::from([initial.as_str().to_owned()]);
        let mut current = initial;
        for hop in 0..MAX_MEDIA_REDIRECTS {
            current = next_redirect(
                &current,
                &format!("/hop-{}.flv", hop + 1),
                &mut visited,
                hop,
            )
            .unwrap();
        }
        assert!(next_redirect(&current, "/sixth.flv", &mut visited, MAX_MEDIA_REDIRECTS).is_err());
    }

    #[test]
    fn stable_progress_requires_real_media_bytes_tags_and_time_advance() {
        let mut progress = MediaProgress::default();
        let script = flv::FlvTag::new(18, 0, &vec![0; STABLE_MEDIA_BYTES as usize]).unwrap();
        let codec = flv::FlvTag::new(9, 0, &[0x17, 0, 1]).unwrap();
        for _ in 0..100 {
            assert!(!progress.observe(&script));
            assert!(!progress.observe(&codec));
        }
        let mut payload = vec![0; 512];
        payload[0] = 0x27;
        payload[1] = 1;
        for _ in 0..STABLE_MEDIA_TAGS {
            assert!(!progress.observe(&flv::FlvTag::new(9, 0, &payload).unwrap()));
        }
        assert!(
            !progress.stable,
            "static timestamps cannot recover a failure budget"
        );
        assert!(progress.observe(&flv::FlvTag::new(9, STABLE_MEDIA_SPAN_MS, &payload).unwrap()));
        assert!(
            !progress.observe(&flv::FlvTag::new(9, STABLE_MEDIA_SPAN_MS + 1, &payload).unwrap()),
            "one recovery per segment"
        );
    }

    #[test]
    fn completed_connection_statistics_have_a_fixed_retention_bound() {
        let store = Arc::new(Mutex::new(RelayStatistics::default()));
        for _ in 0..100 {
            let stats = StreamStatistics::begin(Arc::clone(&store), None);
            stats.finish(RelayConnectionState::Ended, None);
            drop(stats);
        }
        let snapshot = store.lock().unwrap();
        assert_eq!(snapshot.total_streams, 100);
        assert_eq!(snapshot.active_streams, 0);
        assert_eq!(snapshot.connections.len(), RECENT_CONNECTIONS);
    }
}
