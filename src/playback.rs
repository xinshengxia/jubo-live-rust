//! Playback planning is pure; spawning is restricted to the launch entry point.
//!
//! Launching outside the Bili platform starts a loopback relay child and waits
//! for its single 'READY <port>' stdout line before the player is spawned.
//! The relay acknowledges its own process-handle watch before launch succeeds,
//! so closing the launcher does not remove player-lifetime supervision.
use crate::auth::CookieVault;
use crate::core::{
    BufferMode, Enhancement, Platform, PlaybackPreferences, Player, ResolvedStream, Settings,
    Target,
};
use crate::platforms;
use crate::room_state::{RoomObservation, RoomState};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};
use url::Url;

/// Upper bound for the relay's one-line READY handshake.
const RELAY_READY_TIMEOUT: Duration = Duration::from_secs(10);
/// Relay stdout is a single protocol line; anything else is not our protocol.
const RELAY_READY_BYTES: u64 = 4096;
const RELAY_READY_LINES: usize = 16;
/// How often the supervisor checks whether the player or relay already exited.
const SUPERVISE_POLL: Duration = Duration::from_millis(100);
/// The original launcher's mpv helpers: dynamic window title (resolution, fps,
/// codec, bitrate) and the refresh / jump-to-live keys. The launcher keeps
/// --load-scripts=no, so both scripts are passed explicitly.
const TITLE_SCRIPT_RELATIVE: &str = "assets/lua/live-title.lua";
const RESYNC_SCRIPT_RELATIVE: &str = "assets/lua/live-resync.lua";
/// Per-session hint files live in the temp root; bound the directory name so a
/// stale entry is harmless. Lua reads them with Windows path rules, so a
/// non-ASCII temp root disables the helpers instead of risking a broken title.
const TITLE_HINT_DIR: &str = "live-hub-rust-title";
const TITLE_HINT_MAX_CHARS: usize = 512;
/// A hint directory older than this cannot belong to a live player process.
const TITLE_HINT_STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
/// The legacy launcher's Bilibili --watch child polled the room every 10 seconds.
const BILI_ROOM_STATE_POLL: Duration = Duration::from_secs(10);
/// Failed room checks back off instead of hammering a rate-limited platform.
const BILI_ROOM_STATE_MAX_BACKOFF: Duration = Duration::from_secs(60);
/// How often the room watcher re-checks its stop flag while sleeping.
const ROOM_STATE_STOP_CHECK: Duration = Duration::from_millis(250);

pub struct PlaybackPlan {
    pub player: Player,
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
}

/// The GUI-facing launch contract: exactly 'title' and 'player_pid'.
#[derive(Debug, Serialize)]
pub struct LaunchOutcome {
    pub title: String,
    pub player_pid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackEvent {
    CheckingPlayer,
    Resolving,
    RoomObserved {
        state: crate::room_state::RoomState,
        title: Option<String>,
        note: String,
    },
    WaitingForLive {
        note: String,
    },
    WaitingRelay,
    StartingPlayer,
    RegisteringPlayer,
    Playing {
        title: String,
        player_pid: u32,
    },
    RelayStatistics {
        telemetry: RelayTelemetry,
    },
    RelayExited {
        exit_code: Option<i32>,
    },
    Ended {
        exit_code: Option<i32>,
    },
    Stopped,
}

pub type PlaybackObserver = Arc<dyn Fn(PlaybackEvent) + Send + Sync>;

/// A bounded, public-safe view of relay counters. Never carries the private
/// loopback URL, its session token, platform headers or upstream media URL.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayTelemetry {
    pub active_streams: usize,
    pub total_streams: u64,
    pub reconnects: u64,
    pub failures: u64,
    pub consecutive_failures: u64,
    pub stable_segments: u64,
    pub forwarded_bytes: u64,
    pub forwarded_tags: u64,
    pub budget_exhaustions: u64,
    pub state: String,
    pub waiting_state: Option<String>,
    /// FLV onMetaData video bitrate reported by the relay. The legacy launcher
    /// probed the same metadata before launch; the relay already owns that tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_bitrate_kbps: Option<u32>,
}

impl RelayTelemetry {
    pub fn from_statistics(statistics: &crate::relay::RelayStatistics) -> Self {
        use crate::relay::RelayConnectionState as State;
        let selected = statistics
            .connections
            .iter()
            .filter(|connection| {
                matches!(
                    connection.state,
                    State::Streaming | State::Waiting | State::Connecting | State::Retrying
                )
            })
            .max_by_key(|connection| match connection.state {
                State::Streaming => 4,
                State::Waiting => 3,
                State::Retrying => 2,
                _ => 1,
            })
            .or_else(|| {
                statistics
                    .connections
                    .iter()
                    .max_by_key(|connection| connection.id)
            });
        let state = selected
            .map(|connection| match connection.state {
                State::Connecting => "connecting",
                State::Streaming => "streaming",
                State::Retrying => "retrying",
                State::Waiting => "waiting",
                State::Ended => "ended",
                State::Exhausted => "exhausted",
                State::PolicyRejected => "policy_rejected",
                State::PlayerDisconnected => "player_disconnected",
                State::WriteFailed => "write_failed",
            })
            .unwrap_or("idle")
            .to_owned();
        let waiting_state = selected
            .and_then(|connection| connection.waiting_state.as_deref())
            .filter(|state| matches!(*state, "offline" | "replay_unavailable"))
            .map(str::to_owned);
        Self {
            active_streams: statistics.active_streams,
            total_streams: statistics.total_streams,
            reconnects: statistics.reconnects,
            failures: statistics.failures,
            consecutive_failures: statistics
                .connections
                .iter()
                .map(|connection| connection.consecutive_failures)
                .max()
                .unwrap_or(0),
            stable_segments: statistics.stable_segments,
            forwarded_bytes: statistics.forwarded_bytes,
            forwarded_tags: statistics.forwarded_tags,
            budget_exhaustions: statistics.budget_exhaustions,
            state,
            waiting_state,
            stream_bitrate_kbps: statistics
                .stream_info
                .as_ref()
                .map(|info| info.video_kbps)
                .filter(|kbps| *kbps > 0),
        }
    }
}

#[derive(Default)]
struct ControlState {
    // Starting -> Running or Cancelled. Cancellation cannot cross the atomic
    // transition to Running and accidentally stop an already playing session.
    lifecycle: AtomicU8,
    stop: AtomicBool,
}

#[derive(Clone, Default)]
pub struct PlaybackControl(Arc<ControlState>);

impl PlaybackControl {
    pub fn cancel_start(&self) -> bool {
        self.0
            .lifecycle
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    /// Only controls processes owned by this launch; no PID lookup/kill is used.
    pub fn stop(&self) {
        self.0.stop.store(true, Ordering::Release);
    }
    pub fn is_start_cancelled(&self) -> bool {
        self.0.lifecycle.load(Ordering::Acquire) == 2
    }
    pub fn is_stop_requested(&self) -> bool {
        self.0.stop.load(Ordering::Acquire)
    }
    pub fn is_running(&self) -> bool {
        self.0.lifecycle.load(Ordering::Acquire) == 1
    }
    fn check_start(&self) -> Result<()> {
        if self.is_start_cancelled() || self.is_stop_requested() {
            return Err(LaunchCancelled.into());
        }
        Ok(())
    }
    fn commit_running(&self) -> Result<()> {
        self.check_start()?;
        self.0
            .lifecycle
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow::anyhow!(LaunchCancelled))?;
        Ok(())
    }
    fn finish(&self) {
        if !self.is_start_cancelled() {
            self.0.lifecycle.store(3, Ordering::Release);
        }
    }
}

#[derive(Debug)]
pub struct LaunchCancelled;
impl std::fmt::Display for LaunchCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("已取消本次启动")
    }
}
impl std::error::Error for LaunchCancelled {}

fn notify(observer: &PlaybackObserver, event: PlaybackEvent) {
    // A UI callback must not unwind across child ownership and leak processes.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observer(event)));
}

static RESOLVERS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
const MAX_RESOLVERS_IN_FLIGHT: usize = 8;
struct ResolverPermit;
impl Drop for ResolverPermit {
    fn drop(&mut self) {
        RESOLVERS_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Cancellation returns promptly even when a blocking platform request is still
/// finishing its bounded timeout. A global permit caps those abandoned workers.
fn cancellable_request<T, F>(control: &PlaybackControl, work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    loop {
        control.check_start()?;
        let count = RESOLVERS_IN_FLIGHT.load(Ordering::Acquire);
        if count < MAX_RESOLVERS_IN_FLIGHT
            && RESOLVERS_IN_FLIGHT
                .compare_exchange(count, count + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let permit = ResolverPermit;
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("playback-platform-request".into())
        .spawn(move || {
            let _permit = permit;
            let _ = sender.send(work());
        })
        .context("无法启动平台解析线程")?;
    loop {
        control.check_start()?;
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(result) => {
                control.check_start()?;
                return result;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => (),
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("平台解析线程异常结束"),
        }
    }
}

fn cancellable_delay(control: &PlaybackControl, duration: Duration) -> Result<()> {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        control.check_start()?;
        thread::sleep(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(25)),
        );
    }
    control.check_start()
}

type StreamLookup =
    Arc<dyn Fn() -> Result<(ResolvedStream, crate::platforms::StreamHints)> + Send + Sync>;
type RoomLookup = Arc<dyn Fn() -> Result<crate::room_state::RoomObservation> + Send + Sync>;

fn resolve_until_ready(
    control: &PlaybackControl,
    observer: &PlaybackObserver,
    wait_for_live: bool,
    resolve: StreamLookup,
    room: RoomLookup,
    retry: Duration,
) -> Result<(ResolvedStream, crate::platforms::StreamHints)> {
    loop {
        notify(observer, PlaybackEvent::Resolving);
        let worker = resolve.clone();
        match cancellable_request(control, move || worker()) {
            Ok(result) => return Ok(result),
            Err(error) if error.downcast_ref::<LaunchCancelled>().is_some() => return Err(error),
            Err(error)
                if wait_for_live
                    && error.downcast_ref::<platforms::AdapterError>()
                        == Some(&platforms::AdapterError::ReplayUnavailable) =>
            {
                let worker = room.clone();
                let observation = cancellable_request(control, move || worker())?;
                notify(
                    observer,
                    PlaybackEvent::RoomObserved {
                        state: observation.state,
                        title: observation.title.clone(),
                        note: observation.note.clone(),
                    },
                );
                if observation.state != crate::room_state::RoomState::Replay {
                    return Err(error).context("未确认可用重播流，未按下播处理");
                }
                notify(
                    observer,
                    PlaybackEvent::WaitingForLive {
                        note: "房间处于重播，暂未取得媒体；等待直播恢复，可随时取消".into(),
                    },
                );
                cancellable_delay(control, retry)?;
            }
            Err(error)
                if wait_for_live
                    && error.downcast_ref::<platforms::AdapterError>()
                        == Some(&platforms::AdapterError::Offline) =>
            {
                let worker = room.clone();
                let observation = cancellable_request(control, move || worker())?;
                notify(
                    observer,
                    PlaybackEvent::RoomObserved {
                        state: observation.state,
                        title: observation.title.clone(),
                        note: observation.note.clone(),
                    },
                );
                match observation.state {
                    crate::room_state::RoomState::Offline => {
                        notify(
                            observer,
                            PlaybackEvent::WaitingForLive {
                                note: "房间尚未开播，等待重开；可随时取消".into(),
                            },
                        );
                        cancellable_delay(control, retry)?;
                    }
                    crate::room_state::RoomState::Replay => {
                        return Err(error).context("房间正在重播，尚未取得可播放的重播流");
                    }
                    crate::room_state::RoomState::Unknown => {
                        return Err(error).context("尚不能确认房间状态，未按下播等待处理");
                    }
                    crate::room_state::RoomState::Live => {
                        return Err(error).context("平台显示直播中，但本次取流未成功，请重试");
                    }
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn command(program: &Path) -> Command {
    let mut result = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        result.creation_flags(0x08000000); // CREATE_NO_WINDOW for subprocess console tools.
    }
    result
}

pub fn find_player(player: Player, settings: &Settings) -> Result<PathBuf> {
    let explicit = match player {
        Player::Mpv => &settings.mpv,
        Player::PotPlayer => &settings.potplayer,
    };
    if let Some(path) = explicit {
        ensure!(path.is_file(), "播放器路径不存在，请在设置中重新指定");
        return Ok(path.clone());
    }
    let names: &[&str] = match player {
        Player::Mpv => &["mpv.exe", "mpv"],
        Player::PotPlayer => &["PotPlayerMini64.exe", "PotPlayerMini.exe"],
    };
    let executable_dir = std::env::current_exe()?
        .parent()
        .context("missing executable directory")?
        .to_path_buf();
    for directory in [
        executable_dir.clone(),
        executable_dir.join("bin"),
        executable_dir.join("bin/mpv"),
    ] {
        for name in names {
            let path = directory.join(name);
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&paths) {
            if !directory.is_absolute() {
                continue;
            }
            for name in names {
                let path = directory.join(name);
                if path.is_file() {
                    return Ok(path);
                }
            }
        }
    }
    bail!("尚未设置播放器，请在设置中填写 mpv 或 PotPlayer 程序路径")
}

pub fn plan(player: Player, executable: PathBuf, stream: &ResolvedStream) -> Result<PlaybackPlan> {
    plan_with_preferences(player, executable, stream, &PlaybackPreferences::default())
}

pub fn plan_with_preferences(
    player: Player,
    executable: PathBuf,
    stream: &ResolvedStream,
    preferences: &PlaybackPreferences,
) -> Result<PlaybackPlan> {
    preferences.validate()?;
    let url = Url::parse(&stream.url).context("invalid media URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "媒体地址必须是 HTTP/HTTPS"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "媒体地址不能包含登录信息"
    );
    let title: String = stream
        .title
        .chars()
        .filter(|c| !c.is_control())
        .take(512)
        .collect();
    let mut args: Vec<OsString> = Vec::new();
    match player {
        Player::Mpv => {
            args.extend(
                [
                    "--force-window=immediate",
                    "--keep-open=yes",
                    "--load-scripts=no",
                    "--ytdl=no",
                    "--hwdec=auto-safe",
                    // Compatibility profile from jubo-live v1.0.3–v1.1.2 (the
                    // versions the user reports as sync-stable): they did NOT
                    // force --video-sync or --framedrop. mpv already defaults to
                    // video-sync=audio; the extra --framedrop=vo added in v1.1.3
                    // is the only launch difference before the cumulative drift
                    // reports, so do not force it here. Advanced users can still
                    // add it through mpv extra args if their machine needs it.
                    //
                    // autosync=30 is a targeted safety net for sound drivers
                    // whose audio-delay measurement is imperfect: at mpv's
                    // default autosync=0 that error is used directly and a
                    // growing video offset can accumulate over hours. mpv docs
                    // say 30 lets large offsets settle in about 1-2 seconds.
                    "--autosync=30",
                    "--cache=yes",
                    "--network-timeout=15",
                    "--stream-lavf-o=reconnect=1,reconnect_streamed=1,reconnect_delay_max=5",
                ]
                .iter()
                .map(OsString::from),
            );
            match preferences.buffer_mode {
                BufferMode::LowLatency => args.extend(
                    ["--cache-secs=3", "--demuxer-readahead-secs=2"]
                        .iter()
                        .map(OsString::from),
                ),
                BufferMode::Stable => args.extend(
                    ["--cache-secs=15", "--demuxer-readahead-secs=10"]
                        .iter()
                        .map(OsString::from),
                ),
            }
            match preferences.enhancement {
                Enhancement::Off => (),
                Enhancement::Light => args.push("--scale-antiring=0.7".into()),
                Enhancement::High => args.extend(
                    ["--vo=gpu-next,gpu", "--profile=high-quality"]
                        .iter()
                        .map(OsString::from),
                ),
            }
            args.push(format!("--title={title}").into());
            args.push(format!("--force-media-title={title}").into());
            for (name, value) in &stream.headers {
                ensure!(
                    !name.contains(['\r', '\n', ':']) && !value.contains(['\r', '\n']),
                    "非法媒体请求头"
                );
                if name.eq_ignore_ascii_case("user-agent") {
                    args.push(format!("--user-agent={value}").into());
                } else if name.eq_ignore_ascii_case("referer") {
                    args.push(format!("--referrer={value}").into());
                } else {
                    // mpv string-list options use doubled backslashes and escaped commas.
                    let header = format!("{name}: {value}")
                        .replace('\\', "\\\\")
                        .replace(',', "\\,");
                    args.push(format!("--http-header-fields-append={header}").into());
                }
            }
            preferences.validate()?;
            args.extend(preferences.mpv_extra_args.iter().map(OsString::from));
            args.push("--".into());
            args.push(stream.url.clone().into());
        }
        Player::PotPlayer => {
            args.push("/new".into());
            args.push(stream.url.clone().into());
            if !preferences
                .potplayer_extra_args
                .iter()
                .any(|arg| arg.to_ascii_lowercase().starts_with("/title="))
            {
                args.push(format!("/title={title}").into());
            }
            args.extend(preferences.potplayer_extra_args.iter().map(OsString::from));
        }
    }
    Ok(PlaybackPlan {
        player,
        executable,
        arguments: args,
    })
}

pub fn spawn_plan(plan: &PlaybackPlan) -> Result<Child> {
    let mut process = command(&plan.executable);
    #[cfg(windows)]
    if plan.player == Player::PotPlayer {
        use std::os::windows::process::CommandExt;
        for argument in &plan.arguments {
            process.raw_arg(potplayer_windows_argument(
                argument.to_str().context("PotPlayer参数编码无效")?,
            )?);
        }
    } else {
        process.args(&plan.arguments);
    }
    #[cfg(not(windows))]
    process.args(&plan.arguments);
    process
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("无法启动播放器，请检查程序路径")
}

/// PotPlayer parses its raw command line rather than the usual Windows argv:
/// `/title="two words"` works, but `"/title=two words"` becomes part of the URL.
/// Quote only each option's value, exactly as the legacy launcher does. This is
/// passed directly to CreateProcess, never to a shell.
fn potplayer_windows_argument(argument: &str) -> Result<String> {
    fn quoted(value: &str) -> String {
        let mut output = String::from("\"");
        let mut slashes = 0usize;
        for character in value.chars() {
            if character == '\\' {
                slashes += 1;
                continue;
            }
            if character == '"' {
                output.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                output.push('"');
            } else {
                output.extend(std::iter::repeat_n('\\', slashes));
                output.push(character);
            }
            slashes = 0;
        }
        output.extend(std::iter::repeat_n('\\', slashes * 2));
        output.push('"');
        output
    }
    ensure!(
        !argument.chars().any(char::is_control),
        "PotPlayer参数不能含控制字符"
    );
    if argument.starts_with('/') {
        if let Some((name, value)) = argument.split_once('=') {
            ensure!(
                name.len() > 1
                    && name[1..]
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_:-".contains(&b)),
                "PotPlayer选项名无效"
            );
            Ok(format!("{name}={}", quoted(value)))
        } else {
            ensure!(
                argument[1..]
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_:-".contains(&b)),
                "PotPlayer选项名无效"
            );
            Ok(argument.into())
        }
    } else {
        Ok(quoted(argument))
    }
}

/// Kills a child and blocks until it is reaped. Safe when it already exited and
/// always waits, so no process handle is left unreaped after a failed launch.
fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Builds the relay child command: target, quality and session token on the
/// command line, resolved stream and settings through the environment.
fn relay_command(
    executable: &Path,
    target: &Target,
    token: &str,
    settings: &Settings,
    initial: &ResolvedStream,
    credential_root: Option<&Path>,
) -> Result<Command> {
    let mut relay = command(executable);
    relay
        .args([
            "relay",
            &format!("{}:{}", target.platform, target.room),
            "--quality",
            &target.quality.to_string(),
            "--token",
            token,
            "--watch-player",
        ])
        .env("LIVE_HUB_INITIAL_STREAM", serde_json::to_string(initial)?)
        .env("LIVE_HUB_RELAY_SETTINGS", serde_json::to_string(settings)?)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Reconnects inside the relay must resolve with the same explicit vault.
    // Only the directory path crosses the process boundary; cookie values do not.
    if let Some(root) = credential_root {
        relay.env("LIVE_HUB_CREDENTIAL_ROOT", root);
        // <data>/credentials -> <data>: relay.log and relay state must follow the
        // launcher's explicit data directory (the relay has no --data-dir arg).
        if let Some(data_root) = root.parent() {
            relay.env("LIVE_HUB_DATA_DIR", data_root);
        }
    }
    Ok(relay)
}

/// Spawns the independent relay child and waits for its explicit READY line.
///
/// Every failure path terminates and reaps the child before returning an
/// actionable error, so a failed launch never leaves a relay process behind.
#[cfg(test)]
fn await_relay_ready(child: Child, token: &str, timeout: Duration) -> Result<(Child, String)> {
    await_relay_ready_controlled(child, token, timeout, &PlaybackControl::default())
}

fn await_relay_ready_controlled(
    mut child: Child,
    token: &str,
    timeout: Duration,
    control: &PlaybackControl,
) -> Result<(Child, String)> {
    let stdout = child.stdout.take().context("无法读取中转进程的输出")?;
    let (sender, receiver) = mpsc::sync_channel::<std::result::Result<u16, &'static str>>(1);
    let reader_thread = thread::Builder::new()
        .name("relay-ready".into())
        .spawn(move || {
            // Bound the handshake even if a broken child streams arbitrary output.
            let mut reader = BufReader::new(stdout).take(RELAY_READY_BYTES);
            let mut line = String::new();
            for _ in 0..RELAY_READY_LINES {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        let _ = sender.send(Err(
                            "中转进程在输出就绪信息前退出，请检查端口占用或平台连接",
                        ));
                        return;
                    }
                    Ok(_) => {
                        if let Some(port) = line
                            .trim()
                            .strip_prefix("READY ")
                            .and_then(|port| port.parse::<u16>().ok())
                        {
                            let _ = sender.send(Ok(port));
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = sender.send(Err("无法读取中转进程的就绪输出"));
                        return;
                    }
                }
            }
            let _ = sender.send(Err("中转进程输出了无效的就绪信息"));
        });
    if let Err(error) = reader_thread {
        terminate(&mut child);
        return Err(error).context("无法启动中转就绪读取线程");
    }
    let deadline = Instant::now() + timeout;
    let readiness = loop {
        if let Err(error) = control.check_start() {
            terminate(&mut child);
            return Err(error);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break Err(mpsc::RecvTimeoutError::Timeout);
        }
        match receiver.recv_timeout(remaining.min(Duration::from_millis(25))) {
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            value => break value,
        }
    };
    match readiness {
        Ok(Ok(port)) if port > 0 => {
            // Best effort: a relay that exits right after READY must not hand the
            // player a dead loopback address.
            if let Ok(Some(status)) = child.try_wait() {
                bail!("中转进程在就绪后立即退出（{status}），请检查端口占用或网络设置");
            }
            Ok((child, format!("http://127.0.0.1:{port}/live.flv?t={token}")))
        }
        Ok(Ok(_)) => {
            terminate(&mut child);
            bail!("中转进程报告了无效的监听端口")
        }
        Ok(Err(message)) => {
            terminate(&mut child);
            bail!("{message}")
        }
        Err(_) => {
            terminate(&mut child);
            bail!(
                "等待中转进程就绪超时（{} 秒），已终止该进程",
                timeout.as_secs()
            )
        }
    }
}

fn start_relay_controlled(
    target: &Target,
    settings: &Settings,
    initial: &ResolvedStream,
    credential_root: Option<&Path>,
    control: &PlaybackControl,
) -> Result<(Child, String)> {
    control.check_start()?;
    let token = uuid::Uuid::new_v4().simple().to_string();
    let executable = std::env::current_exe().context("无法定位启动器程序")?;
    let mut relay = relay_command(
        &executable,
        target,
        &token,
        settings,
        initial,
        credential_root,
    )?;
    let child = relay.spawn().context("无法启动 Rust 中转进程")?;
    await_relay_ready_controlled(child, &token, RELAY_READY_TIMEOUT, control)
}

struct Children {
    player: Child,
    relay: Option<Child>,
}

/// Watches the player for the whole session. When it exits the relay is killed,
/// and a relay that died on its own is reaped as soon as it is noticed.
fn supervise_controlled(
    mut player: Child,
    mut relay: Option<Child>,
    relay_url: Option<String>,
    control: PlaybackControl,
    observer: PlaybackObserver,
    room_watch_stop: Arc<AtomicBool>,
) {
    let telemetry_stop = Arc::new(AtomicBool::new(false));
    if let Some(relay_url) = relay_url {
        let stop = telemetry_stop.clone();
        let observer = observer.clone();
        let control = control.clone();
        // Statistics HTTP has its own bounded socket timeout. Keeping it off the
        // process-owner thread means a slow response cannot delay explicit stop.
        let _ = thread::Builder::new()
            .name("relay-session-statistics".into())
            .spawn(move || {
                while !stop.load(Ordering::Acquire)
                    && control.is_running()
                    && !control.is_stop_requested()
                {
                    if let Ok(statistics) = crate::relay::query_statistics(&relay_url) {
                        if !stop.load(Ordering::Acquire) && control.is_running() {
                            notify(
                                &observer,
                                PlaybackEvent::RelayStatistics {
                                    telemetry: RelayTelemetry::from_statistics(&statistics),
                                },
                            );
                        }
                    }
                    let next = Instant::now() + Duration::from_secs(2);
                    while Instant::now() < next
                        && !stop.load(Ordering::Acquire)
                        && !control.is_stop_requested()
                    {
                        thread::sleep(Duration::from_millis(50));
                    }
                }
            });
    }
    loop {
        if control.is_stop_requested() {
            telemetry_stop.store(true, Ordering::Release);
            terminate(&mut player);
            if let Some(mut child) = relay.take() {
                terminate(&mut child);
            }
            control.finish();
            room_watch_stop.store(true, Ordering::Release);
            notify(&observer, PlaybackEvent::Stopped);
            return;
        }
        let player_status = player.try_wait();
        let player_exited = matches!(player_status, Ok(Some(_)) | Err(_));
        if let Some(child) = relay.as_mut() {
            let relay_status = child.try_wait();
            if matches!(relay_status, Ok(Some(_)) | Err(_)) {
                telemetry_stop.store(true, Ordering::Release);
                let exit_code = relay_status.ok().flatten().and_then(|status| status.code());
                notify(&observer, PlaybackEvent::RelayExited { exit_code });
                relay = None; // try_wait already reaped this child.
            }
        }
        if player_exited {
            telemetry_stop.store(true, Ordering::Release);
            if let Some(mut child) = relay.take() {
                terminate(&mut child);
            }
            control.finish();
            room_watch_stop.store(true, Ordering::Release);
            notify(
                &observer,
                PlaybackEvent::Ended {
                    exit_code: player_status
                        .ok()
                        .flatten()
                        .and_then(|status| status.code()),
                },
            );
            return;
        }
        thread::sleep(SUPERVISE_POLL);
    }
}

/// Resolves the plan and starts the player, cleaning up the relay on every
/// failure and supervising both children afterwards.
#[cfg(test)]
fn launch_resolved<F, B>(
    target: &Target,
    player: Player,
    executable: PathBuf,
    resolved: &ResolvedStream,
    start_relay: F,
    bind_player: B,
) -> Result<LaunchOutcome>
where
    F: FnOnce(&ResolvedStream) -> Result<(Child, String)>,
    B: FnOnce(&str, u32) -> Result<()> + Send + 'static,
{
    let direct_compatibility = PlaybackPreferences {
        relay_bili: false,
        ..Default::default()
    };
    launch_resolved_controlled(
        target,
        player,
        executable,
        resolved,
        None,
        None,
        &crate::platforms::StreamHints::default(),
        start_relay,
        bind_player,
        &direct_compatibility,
        &PlaybackControl::default(),
        false,
        Arc::new(|_| {}),
    )
}

pub fn uses_relay(
    target: &Target,
    stream: &ResolvedStream,
    preferences: &PlaybackPreferences,
) -> bool {
    if target.platform != Platform::Bili {
        return true;
    }
    preferences.relay_bili
        && !Url::parse(&stream.url)
            .is_ok_and(|url| url.path().to_ascii_lowercase().contains(".m3u8"))
}

// ---- mpv title / OSD hints ------------------------------------------------
//
// live-title.lua reads these UTF-8 text files through mpv script-opts, exactly
// like the legacy launcher: base (window title), media (bottom control bar),
// bitrate, room state, relay state and the autoreplay OSD wording. File names
// mirror the original runtime directory; the directory itself is session-scoped
// under the temp root so a finished session can never leak its OFFLINE/UNKNOWN
// hint into the next one.

/// The two title texts the legacy launcher handed to live-title.lua.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TitleHintTexts {
    /// "主播 - 直播间标题": mpv window title base.
    pub base: String,
    /// 直播间标题 only: bottom control bar plus date/time.
    pub media: String,
}

/// Split a resolved stream title into the original base/media hints.
///
/// Bilibili/Huya/Douyu resolve "anchor - room title" (or just one part); the room
/// title is everything after the first separator. Douyin resolves the room title
/// alone, so both hints carry that text. The authoritative 直播间标题 replaces the
/// media hint as soon as a room observation is available.
pub fn title_hint_texts(platform: Platform, title: &str) -> TitleHintTexts {
    let base = sanitize_title_text(title);
    let base = if base.is_empty() {
        "直播".to_owned()
    } else {
        base
    };
    let media = match platform {
        Platform::Douyin => base.clone(),
        _ => match base.split_once(" - ") {
            Some((_, room)) if !room.trim().is_empty() => sanitize_title_text(room),
            _ => base.clone(),
        },
    };
    TitleHintTexts { base, media }
}

/// Text the original launcher wrote into session-<id>.state.txt; live-title.lua
/// turns it into the title suffix and the OSD notice.
pub fn room_state_hint_text(platform: Platform, state: RoomState) -> String {
    match state {
        RoomState::Live => "LIVE".to_owned(),
        RoomState::Offline => "OFFLINE".to_owned(),
        RoomState::Replay => match platform {
            Platform::Bili => "LOOP".to_owned(),
            Platform::Douyu => "RECORDING".to_owned(),
            _ => "REPLAY".to_owned(),
        },
        // A fresh UNKNOWN must overwrite a stale LIVE; the timestamp proves it is new.
        RoomState::Unknown => format!("UNKNOWN|{}", utc_ticks()),
    }
}

fn utc_ticks() -> i64 {
    // .NET DateTime.UtcNow.Ticks: 100 ns units since 0001-01-01.
    const UNIX_EPOCH_TICKS: i64 = 621_355_968_000_000_000;
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .map_or(UNIX_EPOCH_TICKS, |nanos| {
            UNIX_EPOCH_TICKS.saturating_add(nanos / 100)
        })
}

/// Turns the relay's cumulative forwarded-byte counter into a smoothed kbps
/// value for platforms whose stream metadata carries no bitrate.
///
/// The legacy launcher wrote Douyu's nominal multirates[].bit; when that value is
/// unavailable this measured fallback follows live-title.lua's own smoothing
/// (100 kbps rounding and a 500 kbps hysteresis step) so the title does not
/// flicker at rounding boundaries.
#[derive(Debug, Default)]
pub struct BitrateEstimator {
    last_bytes: Option<u64>,
    smoothed: Option<f64>,
    shown: Option<u32>,
}

impl BitrateEstimator {
    /// Shorter windows are dominated by scheduling noise.
    const MIN_WINDOW: Duration = Duration::from_millis(1200);
    const SMOOTHING: f64 = 0.35;
    const STEP_KBPS: u32 = 500;

    /// Feed one cumulative counter sample. Returns a new publishable value only
    /// when it moved by at least one display step.
    pub fn observe(&mut self, forwarded_bytes: u64, elapsed: Duration) -> Option<u32> {
        let Some(previous) = self.last_bytes.replace(forwarded_bytes) else {
            return None;
        };
        if elapsed < Self::MIN_WINDOW {
            return None;
        }
        // A restarted relay reports a smaller counter: a gap, not a negative rate.
        let bytes = forwarded_bytes.saturating_sub(previous) as f64;
        let kbps = bytes * 8.0 / 1000.0 / elapsed.as_secs_f64();
        let smoothed = match self.smoothed {
            Some(previous) => previous + (kbps - previous) * Self::SMOOTHING,
            None => kbps,
        };
        self.smoothed = Some(smoothed);
        let rounded = ((smoothed / 100.0).round() * 100.0).clamp(0.0, u32::MAX as f64) as u32;
        match self.shown {
            Some(shown) if rounded.abs_diff(shown) < Self::STEP_KBPS => None,
            _ => {
                self.shown = Some(rounded);
                Some(rounded)
            }
        }
    }
}

/// Files live-title.lua reads for one playback session.
pub struct HintPaths {
    pub directory: PathBuf,
    pub base: PathBuf,
    pub media: PathBuf,
    pub bitrate: PathBuf,
    pub state: PathBuf,
    pub relay: Option<PathBuf>,
}

/// Inputs for SessionHints::prepare.
pub struct HintSpec<'a> {
    pub platform: Platform,
    pub room: &'a str,
    /// Full title: 主播 - 直播间标题 when both parts are known.
    pub base: &'a str,
    /// 直播间标题 for the bottom control bar.
    pub media: &'a str,
    /// True when the player reads from the loopback relay.
    pub relay: bool,
    /// Nominal bitrate from the platform response (Douyu multirates).
    pub bitrate_kbps: Option<u32>,
    /// Room observation collected while resolving: authoritative media title
    /// and initial room state.
    pub room_observation: Option<&'a RoomObservation>,
}

impl<'a> HintSpec<'a> {
    pub fn new(platform: Platform, room: &'a str, base: &'a str, media: &'a str) -> Self {
        Self {
            platform,
            room,
            base,
            media,
            relay: false,
            bitrate_kbps: None,
            room_observation: None,
        }
    }
}

struct HintState {
    paths: HintPaths,
    platform: Platform,
    media: Mutex<String>,
    state: Mutex<String>,
    bitrate: Mutex<Option<u32>>,
    estimator: Mutex<BitrateEstimator>,
    last_sample: Mutex<Option<Instant>>,
}

impl Drop for HintState {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.paths.directory);
    }
}

/// One playback session's mpv hint files, shared by the launch path, the
/// supervisor/relay observer and the Bilibili room watcher.
///
/// Dropping the last clone removes the session directory. A crashed launcher can
/// leave at most one directory, which the next launch prunes after 24 hours.
#[derive(Clone)]
pub struct SessionHints(Arc<HintState>);

impl SessionHints {
    /// Prepare hint files under the per-user temp root.
    pub fn prepare(spec: HintSpec<'_>) -> Option<Self> {
        let root = std::env::temp_dir().join(TITLE_HINT_DIR);
        Self::prepare_in(&root, spec)
    }

    /// Prepare hint files in an explicit root (contract tests and diagnostics).
    pub fn prepare_in(root: &Path, spec: HintSpec<'_>) -> Option<Self> {
        // mpv's script-opts list cannot carry a non-ASCII directory reliably, so a
        // non-ASCII temp root disables the helpers instead of producing a broken title.
        root.as_os_str().to_str().filter(|text| text.is_ascii())?;
        prune_stale_hint_directories(root, TITLE_HINT_STALE_AFTER);
        let session = uuid::Uuid::new_v4().simple().to_string();
        let directory = root.join(&session);
        fs::create_dir_all(&directory).ok()?;
        let platform = platform_slug(spec.platform);
        let room = hint_file_component(spec.room);
        let paths = HintPaths {
            base: directory.join(format!("{platform}-{room}.anchor.txt")),
            media: directory.join(format!("session-{session}.media-title.txt")),
            bitrate: directory.join(format!("{platform}-{room}.bitrate.txt")),
            state: directory.join(format!("session-{session}.state.txt")),
            relay: spec
                .relay
                .then(|| directory.join(format!("session-{session}.relay.txt"))),
            directory,
        };
        let fallback = title_hint_texts(spec.platform, spec.base);
        let authoritative = spec
            .room_observation
            .and_then(|observation| observation.title.as_deref())
            .map(sanitize_title_text)
            .filter(|title| !title.is_empty());
        let media = authoritative
            .clone()
            .unwrap_or_else(|| sanitize_title_text(spec.media));
        let media = if media.is_empty() {
            fallback.media
        } else {
            media
        };
        let base_title = if fallback.base.is_empty() {
            media.clone()
        } else {
            fallback.base
        };
        let initial_state = spec
            .room_observation
            .map(|observation| room_state_hint_text(spec.platform, observation.state));
        let initial_bitrate = spec.bitrate_kbps.filter(|kbps| *kbps > 0);
        write_hint(&paths.base, &base_title);
        write_hint(&paths.media, &media);
        if let Some(kbps) = initial_bitrate {
            write_hint(&paths.bitrate, &kbps.to_string());
        }
        if let Some(state) = initial_state.as_deref() {
            write_hint(&paths.state, state);
        }
        if let Some(relay) = paths.relay.as_deref() {
            write_hint(relay, "");
        }
        Some(Self(Arc::new(HintState {
            paths,
            platform: spec.platform,
            media: Mutex::new(media),
            state: Mutex::new(initial_state.unwrap_or_default()),
            bitrate: Mutex::new(initial_bitrate),
            estimator: Mutex::new(BitrateEstimator::default()),
            last_sample: Mutex::new(None),
        })))
    }

    pub fn paths(&self) -> &HintPaths {
        &self.0.paths
    }

    /// mpv script-opts for the packaged Lua helpers.
    pub fn script_opts(&self, title_script: &Path, resync: Option<&Path>) -> Vec<OsString> {
        let mut arguments = Vec::new();
        let (Some(script), Some(base), Some(media), Some(state)) = (
            mpv_option_path(title_script),
            mpv_option_path(&self.0.paths.base),
            mpv_option_path(&self.0.paths.media),
            mpv_option_path(&self.0.paths.state),
        ) else {
            return arguments;
        };
        arguments.push(format!("--script={script}").into());
        arguments.push(format!("--script-opts-append=live_title_base_file={base}").into());
        arguments.push(format!("--script-opts-append=live_title_media_file={media}").into());
        arguments.push(format!("--script-opts-append=live_title_state_file={state}").into());
        if let Some(relay) = self.0.paths.relay.as_deref().and_then(mpv_option_path) {
            arguments.push(format!("--script-opts-append=live_title_relay_file={relay}").into());
            arguments.push("--script-opts-append=live_title_autoreplay=yes".into());
            if let Some(bitrate) = mpv_option_path(&self.0.paths.bitrate) {
                arguments.push(format!(
                    "--script-opts-append=live_title_bitrate_file={bitrate}"
                )
                .into());
            }
        }
        if let Some(resync) = resync.and_then(mpv_option_path) {
            arguments.push(format!("--script={resync}").into());
        }
        arguments
    }

    /// Inserts the hint options before the -- separator so they stay options.
    pub fn apply_to(&self, plan: &mut PlaybackPlan, title_script: &Path, resync: Option<&Path>) {
        let arguments = self.script_opts(title_script, resync);
        if arguments.is_empty() {
            return;
        }
        let position = plan
            .arguments
            .iter()
            .position(|argument| argument == "--")
            .unwrap_or(plan.arguments.len());
        plan.arguments.splice(position..position, arguments);
    }

    /// Feed a playback event into the relay/bitrate/state hint files.
    pub fn observe(&self, event: &PlaybackEvent) {
        self.observe_at(event, Instant::now());
    }

    /// Deterministic entry point used by the contract tests.
    pub fn observe_at(&self, event: &PlaybackEvent, at: Instant) {
        match event {
            PlaybackEvent::RelayStatistics { telemetry } => self.observe_relay(telemetry, at),
            PlaybackEvent::RelayExited { .. } => self.write_relay("RECONNECTING"),
            PlaybackEvent::RoomObserved { state, title, .. } => {
                self.write_state(&room_state_hint_text(self.0.platform, *state));
                if let Some(title) = title {
                    let title = sanitize_title_text(title);
                    if !title.is_empty() {
                        self.write_media(&title);
                    }
                }
            }
            _ => {}
        }
    }

    /// Write the authoritative room state / title learned by a room observation.
    pub fn observe_room(&self, observation: &RoomObservation) {
        self.write_state(&room_state_hint_text(self.0.platform, observation.state));
        if let Some(title) = observation.title.as_deref() {
            let title = sanitize_title_text(title);
            if !title.is_empty() {
                self.write_media(&title);
            }
        }
    }

    fn observe_relay(&self, telemetry: &RelayTelemetry, at: Instant) {
        if self.0.paths.relay.is_none() {
            return;
        }
        if let Some(text) = relay_hint(telemetry) {
            self.write_relay(text);
        }
        // A positively confirmed room state is usable even without a Bili watcher.
        match telemetry.waiting_state.as_deref() {
            Some("offline") => {
                self.write_state(&room_state_hint_text(self.0.platform, RoomState::Offline))
            }
            Some("replay_unavailable") => {
                self.write_state(&room_state_hint_text(self.0.platform, RoomState::Replay))
            }
            _ => {}
        }
        if telemetry.state == "streaming" {
            let resumed = {
                self.0
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_str()
                    == "OFFLINE"
            };
            if resumed {
                self.write_state(&room_state_hint_text(self.0.platform, RoomState::Live));
            }
        }
        if let Some(kbps) = telemetry.stream_bitrate_kbps.filter(|kbps| *kbps > 0) {
            self.write_bitrate(kbps);
            return;
        }
        if !bitrate_hint_platform(self.0.platform) {
            return;
        }
        let elapsed = {
            let mut last = self
                .0
                .last_sample
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let elapsed = last.map_or(Duration::ZERO, |previous| {
                at.saturating_duration_since(previous)
            });
            *last = Some(at);
            elapsed
        };
        if elapsed.is_zero() {
            return;
        }
        let measured = self
            .0
            .estimator
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .observe(telemetry.forwarded_bytes, elapsed);
        if let Some(kbps) = measured {
            self.fill_bitrate(kbps);
        }
    }

    fn write_media(&self, text: &str) {
        let mut current = self
            .0
            .media
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *current == text {
            return;
        }
        *current = text.to_owned();
        write_hint(&self.0.paths.media, text);
    }

    fn write_state(&self, text: &str) {
        let mut current = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *current == text {
            return;
        }
        *current = text.to_owned();
        write_hint(&self.0.paths.state, text);
    }

    fn write_bitrate(&self, kbps: u32) {
        if kbps == 0 {
            return;
        }
        let mut current = self
            .0
            .bitrate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *current == Some(kbps) {
            return;
        }
        *current = Some(kbps);
        write_hint(&self.0.paths.bitrate, &kbps.to_string());
    }

    /// Only a missing bitrate is replaced by the measured fallback; a nominal
    /// platform value stays until the relay reports exact metadata.
    fn fill_bitrate(&self, kbps: u32) {
        if self
            .0
            .bitrate
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some()
        {
            return;
        }
        self.write_bitrate(kbps);
    }

    fn write_relay(&self, text: &str) {
        if let Some(path) = self.0.paths.relay.as_deref() {
            write_hint(path, text);
        }
    }
}

/// Platforms whose stream metadata carries no bitrate for mpv; the legacy
/// launcher wrote hint files for these (Huya uses demux-bitrate instead).
fn bitrate_hint_platform(platform: Platform) -> bool {
    matches!(platform, Platform::Douyu | Platform::Douyin)
}

fn platform_slug(platform: Platform) -> &'static str {
    match platform {
        Platform::Bili => "bili",
        Platform::Huya => "huya",
        Platform::Douyu => "douyu",
        Platform::Douyin => "douyin",
    }
}

/// The original file names embed the room; keep the component filesystem-safe.
fn hint_file_component(value: &str) -> String {
    let mut component: String = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(64)
        .collect();
    if component.is_empty() {
        component.push_str("room");
    }
    component
}

/// Remove session directories left behind by a crashed launcher.
pub fn prune_stale_hint_directories(root: &Path, max_age: Duration) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        let stale = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > max_age);
        if stale {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Bilibili has no relay to report the room state, so the legacy launcher ran a
/// --watch child that polled the room every 10 seconds and wrote the state hint.
/// The Rust launcher keeps that watcher inside the playback supervisor: one thread
/// per launch, stopped with the player, with exponential backoff on errors.
struct RoomWatch {
    target: Target,
    vault: Option<CookieVault>,
}

fn spawn_room_state_watcher(hints: SessionHints, watch: RoomWatch, stop: Arc<AtomicBool>) {
    let _ = thread::Builder::new()
        .name("playback-room-state".into())
        .spawn(move || {
            let mut interval = BILI_ROOM_STATE_POLL;
            loop {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                match crate::room_state::check(&watch.target, watch.vault.as_ref()) {
                    Ok(observation) => {
                        hints.observe_room(&observation);
                        interval = BILI_ROOM_STATE_POLL;
                    }
                    Err(_) => interval = (interval * 2).min(BILI_ROOM_STATE_MAX_BACKOFF),
                }
                let deadline = Instant::now() + interval;
                while Instant::now() < deadline {
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    thread::sleep(
                        ROOM_STATE_STOP_CHECK
                            .min(deadline.saturating_duration_since(Instant::now())),
                    );
                }
            }
        });
}

struct TitleScripts {
    title: PathBuf,
    resync: Option<PathBuf>,
}

/// Locates the packaged Lua helpers next to the launcher executable.
fn title_scripts() -> Option<TitleScripts> {
    let executable = std::env::current_exe().ok()?;
    let directory = executable.parent()?;
    let title = directory.join(TITLE_SCRIPT_RELATIVE);
    if !title.is_file() {
        return None;
    }
    let resync = directory.join(RESYNC_SCRIPT_RELATIVE);
    Some(TitleScripts {
        title,
        resync: resync.is_file().then_some(resync),
    })
}

/// mpv options use commas as list separators and accept forward slashes on
/// Windows; reject anything that cannot be represented unambiguously.
fn mpv_option_path(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    if text.contains([',', '\n', '\r']) {
        return None;
    }
    Some(text.replace('\\', "/"))
}

fn sanitize_title_text(title: &str) -> String {
    let single: String = title
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    single.trim().chars().take(TITLE_HINT_MAX_CHARS).collect()
}

/// Atomic-ish replacement for the tiny hint files the Lua script polls.
fn write_hint(path: &Path, text: &str) {
    let temporary = path.with_extension("tmp");
    if fs::write(&temporary, text.as_bytes()).is_ok() {
        let _ = fs::rename(&temporary, path);
    }
}

/// The original launcher's relay hint vocabulary: ONLINE / RECONNECTING /
/// OFFLINE / REPLAY-UNAVAILABLE|reason. Only positively known states are written.
fn relay_hint(telemetry: &RelayTelemetry) -> Option<&'static str> {
    match telemetry.waiting_state.as_deref() {
        Some("offline") => return Some("OFFLINE"),
        Some("replay_unavailable") => return Some("REPLAY-UNAVAILABLE|重播流不可用"),
        _ => {}
    }
    match telemetry.state.as_str() {
        "streaming" => Some("ONLINE"),
        "connecting" | "retrying" | "waiting" => Some("RECONNECTING"),
        _ => None,
    }
}

/// Keeps the Lua title script's hint files current without changing the event
/// stream the launcher UI already consumes.
fn observer_with_hints(observer: PlaybackObserver, hints: Option<SessionHints>) -> PlaybackObserver {
    let Some(hints) = hints else {
        return observer;
    };
    Arc::new(move |event: PlaybackEvent| {
        hints.observe(&event);
        observer(event);
    })
}

/// Probe the first FLV onMetaData tag for PotPlayer's title suffix. The legacy
/// launcher read the same metadata synchronously before spawn and appended
/// "<w>x<h> <fps>fps <codec> <kbps>kbps" to the window title; mpv gets the
/// equivalent information from its Lua script instead.
fn probe_flv_stream_info(resolved: &ResolvedStream) -> Option<crate::flv::StreamInfo> {
    let response = crate::relay::open_media_stream(resolved).ok()?;
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, response);
    crate::flv::read_stream_info(&mut reader).ok().flatten()
}

/// Probe the relay's first media connection for PotPlayer's title suffix.
///
/// This runs before the player is spawned but after the relay is ready. It opens
/// one loopback connection, reads the FLV header/onMetaData and disconnects; the
/// relay then serves PotPlayer's own connections. Using the relay avoids a
/// second upstream connection (which some CDNs account for per URL) and keeps
/// the relay's reconnect/CDN redirection policy in one place.
fn probe_relay_stream_info(relay_url: &str) -> Option<crate::flv::StreamInfo> {
    use std::io::{BufRead, BufReader, Write};
    let parsed = url::Url::parse(relay_url).ok()?;
    let host = parsed.host_str()?;
    let port = parsed.port()?;
    let path = match parsed.query() {
        Some(query) => format!("{}?{query}", parsed.path()),
        None => parsed.path().to_owned(),
    };
    let mut stream = std::net::TcpStream::connect((host, port)).ok()?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(6)))
        .ok()?;
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(3)))
        .ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut reader = BufReader::with_capacity(64 * 1024, stream);
    let mut line = String::new();
    let mut header_bytes = 0usize;
    loop {
        line.clear();
        let read = reader.read_line(&mut line).ok()?;
        header_bytes += read;
        if read == 0 || header_bytes > 16 * 1024 || line == "\r\n" || line == "\n" {
            break;
        }
    }
    crate::flv::read_stream_info(&mut reader).ok().flatten()
}

fn compose_potplayer_title(base: &str, info: &crate::flv::StreamInfo) -> String {
    let mut parts = Vec::new();
    if info.width > 0 && info.height > 0 {
        parts.push(format!("{}x{}", info.width, info.height));
    }
    if info.fps > 0.0 {
        parts.push(format!("{}fps", info.fps.round() as u32));
    }
    if let Some(codec) = info.codec {
        parts.push(codec.label().to_owned());
    }
    if info.video_kbps > 0 {
        parts.push(format!("{}kbps", info.video_kbps));
    }
    if parts.is_empty() {
        base.to_owned()
    } else {
        format!("{base}  ·  {}", parts.join(" "))
    }
}

fn launch_resolved_controlled<F, B>(
    target: &Target,
    player: Player,
    executable: PathBuf,
    resolved: &ResolvedStream,
    initial_room: Option<RoomObservation>,
    room_watch: Option<RoomWatch>,
    stream_hints: &crate::platforms::StreamHints,
    start_relay: F,
    bind_player: B,
    preferences: &PlaybackPreferences,
    control: &PlaybackControl,
    probe_potplayer_title: bool,
    observer: PlaybackObserver,
) -> Result<LaunchOutcome>
where
    F: FnOnce(&ResolvedStream) -> Result<(Child, String)>,
    B: FnOnce(&str, u32) -> Result<()> + Send + 'static,
{
    control.check_start()?;
    let mut relay = None;
    let relayed = uses_relay(target, resolved, preferences);
    let mut playable = if relayed {
        notify(&observer, PlaybackEvent::WaitingRelay);
        let (child, url) = start_relay(resolved)?;
        relay = Some(child);
        ResolvedStream {
            url,
            title: resolved.title.clone(),
            headers: Default::default(),
        }
    } else {
        resolved.clone()
    };
    if probe_potplayer_title && player == Player::PotPlayer {
        let probed = if relayed {
            // The relay is already serving the resolved URL; prefer reading the
            // metadata through it. If that connection could not be established
            // (CDN edge/redirect race), fall back to one direct probe so the
            // title does not silently lose its resolution/fps/codec suffix.
            probe_relay_stream_info(&playable.url).or_else(|| probe_flv_stream_info(resolved))
        } else {
            probe_flv_stream_info(resolved)
        };
        if let Some(info) = probed {
            playable.title = compose_potplayer_title(&playable.title, &info);
        } else {
            crate::diag::log(
                "playback",
                crate::diag::Level::Warn,
                "potplayer title metadata probe returned no FLV onMetaData",
            );
        }
    }
    let mut plan = match control
        .check_start()
        .and_then(|_| plan_with_preferences(player, executable, &playable, preferences))
    {
        Ok(plan) => plan,
        Err(error) => {
            if let Some(mut child) = relay.take() {
                terminate(&mut child);
            }
            return Err(error);
        }
    };
    // The original launcher's Lua helpers add the dynamic window title and the
    // refresh / jump-to-live keys. They are opt-in by package layout: without the
    // packaged assets the launcher keeps the plain static title.
    let title_hints = match (plan.player, title_scripts()) {
        (Player::Mpv, Some(scripts)) => {
            // The original splits the window title (anchor - room title) from the
            // bottom bar title (room title only). Resolve hints are authoritative;
            // the resolved full title is the fallback when a lookup failed.
            let fallback = title_hint_texts(target.platform, &playable.title);
            let base = match (
                stream_hints.anchor.as_deref(),
                stream_hints.room_title.as_deref(),
            ) {
                (Some(anchor), Some(room)) => format!("{anchor} - {room}"),
                (Some(part), None) | (None, Some(part)) => part.to_owned(),
                (None, None) => fallback.base,
            };
            let media = stream_hints.room_title.clone().unwrap_or(fallback.media);
            SessionHints::prepare(HintSpec {
                platform: target.platform,
                room: &target.room,
                base: &base,
                media: &media,
                relay: relayed,
                bitrate_kbps: stream_hints.bitrate_kbps,
                room_observation: initial_room.as_ref(),
            })
            .map(|hints| {
                hints.apply_to(&mut plan, &scripts.title, scripts.resync.as_deref());
                hints
            })
        }
        _ => None,
    };
    let observer = observer_with_hints(observer, title_hints.clone());
    notify(&observer, PlaybackEvent::StartingPlayer);
    let mut player_child = match spawn_plan(&plan) {
        Ok(child) => child,
        Err(error) => {
            if let Some(mut child) = relay.take() {
                terminate(&mut child);
            }
            return Err(error);
        }
    };
    let player_pid = player_child.id();
    if let Err(error) = control.check_start() {
        terminate(&mut player_child);
        if let Some(mut child) = relay.take() {
            terminate(&mut child);
        }
        return Err(error);
    }
    if relay.is_some() {
        notify(&observer, PlaybackEvent::RegisteringPlayer);
        let relay_url = playable.url.clone();
        if let Err(error) =
            cancellable_request(control, move || bind_player(&relay_url, player_pid))
        {
            terminate(&mut player_child);
            if let Some(mut child) = relay.take() {
                terminate(&mut child);
            }
            return Err(error).context("无法绑定播放器生命周期，已停止本次播放");
        }
    }
    let title = resolved.title.clone();
    let telemetry_url = relay.is_some().then(|| playable.url.clone());
    // The children move into a supervisor thread. The shared slot keeps them
    // recoverable if that thread cannot be created, so no child outlives launch
    // and no process handle is leaked.
    let slot = Arc::new(Mutex::new(Some(Children {
        player: player_child,
        relay,
    })));
    let worker = Arc::clone(&slot);
    let (start_sender, start_receiver) = mpsc::sync_channel(1);
    let supervisor_control = control.clone();
    let supervisor_observer = observer.clone();
    let supervisor_hints = title_hints;
    let spawned = thread::Builder::new()
        .name("playback-supervisor".into())
        .spawn(move || {
            let Some(Children { player, relay }) = worker
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            else {
                return;
            };
            if start_receiver.recv() != Ok(true) {
                let mut player = player;
                terminate(&mut player);
                if let Some(mut relay) = relay {
                    terminate(&mut relay);
                }
                return;
            }
            // Bilibili has no relay, so its state hint comes from a bounded room
            // watcher (the legacy launcher's --watch child). It lives exactly as
            // long as this supervisor and stops with the player; no second watcher
            // is ever registered for the same session.
            let room_watch_stop = Arc::new(AtomicBool::new(false));
            if let (Some(hints), Some(watch)) = (supervisor_hints, room_watch) {
                spawn_room_state_watcher(hints, watch, Arc::clone(&room_watch_stop));
            }
            supervise_controlled(
                player,
                relay,
                telemetry_url,
                supervisor_control,
                supervisor_observer,
                room_watch_stop,
            );
        });
    if let Err(error) = spawned {
        if let Some(Children { mut player, relay }) = slot
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            terminate(&mut player);
            if let Some(mut child) = relay {
                terminate(&mut child);
            }
        }
        bail!("无法启动播放监管线程，已终止本次播放：{error}");
    }
    if let Err(error) = control.commit_running() {
        let _ = start_sender.send(false);
        if let Ok(worker) = spawned {
            let _ = worker.join();
        }
        return Err(error);
    }
    notify(
        &observer,
        PlaybackEvent::Playing {
            title: title.clone(),
            player_pid,
        },
    );
    let _ = start_sender.send(true);
    Ok(LaunchOutcome { title, player_pid })
}

pub fn launch(target: &Target, player: Player, settings: &Settings) -> Result<LaunchOutcome> {
    launch_controlled(
        target,
        player,
        settings,
        None,
        &PlaybackControl::default(),
        Arc::new(|_| {}),
    )
}

/// Resolve through the explicit per-platform credential vault (imported cookies
/// apply only to their own platform) and launch exactly like the anonymous path.
pub fn launch_authenticated(
    target: &Target,
    player: Player,
    settings: &Settings,
    vault: &CookieVault,
) -> Result<LaunchOutcome> {
    launch_controlled(
        target,
        player,
        settings,
        Some(vault),
        &PlaybackControl::default(),
        Arc::new(|_| {}),
    )
}

/// Launch used by the session workbench. Cancellation is a startup operation;
/// stopping an existing session is a distinct explicit control request.
pub fn launch_controlled(
    target: &Target,
    player: Player,
    settings: &Settings,
    vault: Option<&CookieVault>,
    control: &PlaybackControl,
    observer: PlaybackObserver,
) -> Result<LaunchOutcome> {
    notify(&observer, PlaybackEvent::CheckingPlayer);
    control.check_start()?;
    target.validate()?;
    let executable = find_player(player, settings)?;
    let worker_target = target.clone();
    let worker_settings = settings.clone();
    let worker_vault = vault.cloned();
    let lookup: StreamLookup = Arc::new(move || match worker_vault.as_ref() {
        Some(vault) => platforms::resolve_authenticated_with_hints(
            &worker_target,
            &worker_settings,
            vault,
        ),
        None => platforms::resolve_with_hints(&worker_target, &worker_settings, None),
    });
    let worker_target = target.clone();
    let worker_vault = vault.cloned();
    let room: RoomLookup =
        Arc::new(move || crate::room_state::check(&worker_target, worker_vault.as_ref()));
    // Keep the last room observation made while waiting so the title script starts
    // from the authoritative room title/state instead of a generic label.
    let observed: Arc<Mutex<Option<RoomObservation>>> = Arc::new(Mutex::new(None));
    let capture = Arc::clone(&observed);
    let forwarding = observer.clone();
    let capturing: PlaybackObserver = Arc::new(move |event: PlaybackEvent| {
        if let PlaybackEvent::RoomObserved { state, title, note } = &event {
            *capture.lock().unwrap_or_else(|error| error.into_inner()) = Some(RoomObservation {
                state: *state,
                title: title.clone(),
                note: note.clone(),
            });
        }
        forwarding(event);
    });
    let (resolved, stream_hints) = resolve_until_ready(
        control,
        &capturing,
        settings.playback.wait_for_live,
        lookup,
        room,
        Duration::from_secs(20),
    )?;
    let initial_room = observed
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    // Bilibili has no relay, so the legacy launcher relied on its --watch child for
    // the room state hint. Relay platforms report their state through the relay
    // hint file instead.
    let room_watch = (target.platform == Platform::Bili).then(|| RoomWatch {
        target: target.clone(),
        vault: vault.cloned(),
    });
    let credential_root = vault.map(|vault| vault.root().to_owned());
    launch_resolved_controlled(
        target,
        player,
        executable,
        &resolved,
        initial_room,
        room_watch,
        &stream_hints,
        move |initial| {
            start_relay_controlled(
                target,
                settings,
                initial,
                credential_root.as_deref(),
                control,
            )
        },
        crate::relay::register_player,
        &settings.playback,
        control,
        true,
        observer,
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn potplayer_quotes_values_without_turning_title_into_media_url() {
        assert_eq!(
            super::potplayer_windows_argument("/title=主播 & test").unwrap(),
            "/title=\"主播 & test\""
        );
        assert_eq!(
            super::potplayer_windows_argument("https://media.example.test/live.flv?t=synthetic")
                .unwrap(),
            "\"https://media.example.test/live.flv?t=synthetic\""
        );
        assert_eq!(
            super::potplayer_windows_argument("/title=含\"引号\"\\").unwrap(),
            "/title=\"含\\\"引号\\\"\\\\\""
        );
        assert_eq!(super::potplayer_windows_argument("/new").unwrap(), "/new");
        assert!(super::potplayer_windows_argument("/title=bad\nvalue").is_err());
        assert!(super::potplayer_windows_argument("/bad option=1").is_err());
    }
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Write as _;
    use std::time::Instant;

    const SYNTHETIC_CHILD_ENV: &str = "LIVE_HUB_SYNTHETIC_CHILD";
    const SYNTHETIC_LOCK_ENV: &str = "LIVE_HUB_SYNTHETIC_LOCK";

    #[test]
    fn potplayer_title_appends_the_legacy_resolution_fps_codec_bitrate_suffix() {
        let info = crate::flv::StreamInfo {
            width: 1920,
            height: 1080,
            fps: 59.94,
            video_kbps: 4000,
            codec: Some(crate::flv::CodecKind::H264),
        };
        assert_eq!(
            compose_potplayer_title("主播 - 标题", &info),
            "主播 - 标题  ·  1920x1080 60fps h264 4000kbps"
        );
        assert_eq!(
            compose_potplayer_title(
                "只有标题",
                &crate::flv::StreamInfo::default()
            ),
            "只有标题"
        );
    }

    #[test]
    fn player_arguments_remain_separate_for_metacharacters() {
        let stream = ResolvedStream {
            url: "https://media.example.test/live.flv?a=1&b=2".into(),
            title: "主播 \"标题\" & 测试\n尾部".into(),
            headers: Default::default(),
        };
        let plan = plan(Player::Mpv, PathBuf::from("player path/mpv.exe"), &stream).unwrap();
        assert_eq!(plan.arguments.last().unwrap(), &OsString::from(&stream.url));
        assert!(
            plan.arguments
                .iter()
                .any(|x| x == "--title=主播 \"标题\" & 测试尾部")
        );
        assert_eq!(plan.arguments.iter().filter(|x| *x == "--").count(), 1);
    }

    #[test]
    fn media_cannot_be_an_option_or_local_file() {
        for url in [
            "--script=bad.lua",
            "file:///private/file",
            "https://user:secret@example.test/live.flv",
        ] {
            let stream = ResolvedStream {
                url: url.into(),
                title: String::new(),
                headers: Default::default(),
            };
            assert!(plan(Player::Mpv, "mpv.exe".into(), &stream).is_err());
        }
    }

    #[test]
    fn relay_command_carries_target_quality_token_and_serialized_inputs() {
        let target = Target::new(Platform::Huya, "synthetic-room").unwrap();
        let initial = ResolvedStream {
            url: "https://media.example.test/live.flv".into(),
            headers: BTreeMap::new(),
            title: "Synthetic".into(),
        };
        let relay = relay_command(
            Path::new("relay.exe"),
            &target,
            "synthetic-token",
            &Settings::default(),
            &initial,
            None,
        )
        .unwrap();
        let arguments: Vec<String> = relay
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            arguments,
            [
                "relay",
                "huya:synthetic-room",
                "--quality",
                "best",
                "--token",
                "synthetic-token",
                "--watch-player"
            ]
        );
        let expected_stream = serde_json::to_string(&initial).unwrap();
        let environment: BTreeMap<_, _> = relay
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(
            environment["LIVE_HUB_INITIAL_STREAM"].as_deref(),
            Some(expected_stream.as_str())
        );
        assert_eq!(
            environment["LIVE_HUB_RELAY_SETTINGS"].as_deref(),
            Some(
                serde_json::to_string(&Settings::default())
                    .unwrap()
                    .as_str()
            ),
        );
    }

    /// Synthetic child process used by the offline startup/cleanup tests.
    /// It is spawned from this same test binary; see synthetic_child_command.
    #[test]
    #[ignore = "spawned as a synthetic relay process by the playback contract tests"]
    fn synthetic_child() {
        let Ok(mode) = std::env::var(SYNTHETIC_CHILD_ENV) else {
            return;
        };
        let lock = std::env::var_os(SYNTHETIC_LOCK_ENV).map(PathBuf::from);
        match mode.as_str() {
            // The libtest harness writes the test name without a trailing newline
            // before the test body runs; start the protocol line on its own line.
            "ready_lock" => {
                println!();
                println!("READY 45678");
                let _ = std::io::stdout().flush();
                block_forever(lock);
            }
            "no_ready_lock" => block_forever(lock),
            "exit_no_ready" => {}
            _ => {}
        }
    }

    fn block_forever(lock: Option<PathBuf>) {
        // Holding an exclusive file lock lets the parent test prove that the
        // process was terminated (the lock is released when the process dies).
        let _held = lock.and_then(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(path)
                .ok()
                .map(|file| {
                    let _ = fs2::FileExt::lock_exclusive(&file);
                    file
                })
        });
        loop {
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn synthetic_child_command(mode: &str, lock: &Path) -> Command {
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "playback::tests::synthetic_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(SYNTHETIC_CHILD_ENV, mode)
            .env(SYNTHETIC_LOCK_ENV, lock)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        child
    }

    /// Waits until the synthetic child holds (locked = true) or released its
    /// exclusive lock (locked = false).
    fn wait_for_lock(path: &Path, locked: bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let free = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .open(path)
                .map(|file| fs2::FileExt::try_lock_exclusive(&file).is_ok())
                .unwrap_or(false);
            if free != locked {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn title_lua_helpers_get_utf8_hint_files_before_the_media_url() {
        let directory = tempfile::tempdir().unwrap();
        let hints = SessionHints::prepare_in(
            directory.path(),
            HintSpec {
                platform: Platform::Bili,
                room: "12345",
                base: "主播 \"标题\" & 测试\n尾部 - 直播间",
                media: "直播间",
                relay: true,
                bitrate_kbps: Some(4000),
                room_observation: None,
            },
        )
        .unwrap();
        let scripts = TitleScripts {
            title: PathBuf::from("C:/package/assets/lua/live-title.lua"),
            resync: Some(PathBuf::from("C:/package/assets/lua/live-resync.lua")),
        };
        let mut plan = plan(Player::Mpv, PathBuf::from("mpv.exe"), &synthetic_stream()).unwrap();
        hints.apply_to(&mut plan, &scripts.title, scripts.resync.as_deref());
        let arguments: Vec<String> = plan
            .arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        let script_at = arguments
            .iter()
            .position(|argument| argument == "--script=C:/package/assets/lua/live-title.lua")
            .expect("title script argument");
        let separator = arguments
            .iter()
            .position(|argument| argument == "--")
            .unwrap();
        assert!(
            script_at < separator,
            "scripts must stay before the media URL separator"
        );
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "--script=C:/package/assets/lua/live-resync.lua")
        );
        for option in [
            "live_title_base_file=",
            "live_title_media_file=",
            "live_title_state_file=",
            "live_title_relay_file=",
            "live_title_bitrate_file=",
        ] {
            assert!(
                arguments
                    .iter()
                    .any(|argument| argument.starts_with(&format!("--script-opts-append={option}"))),
                "missing script-opt {option}"
            );
        }
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "--script-opts-append=live_title_autoreplay=yes")
        );
        assert_eq!(
            arguments.last().unwrap(),
            "https://media.example.test/live.flv"
        );
        // The original launcher's file names, with base and media kept apart.
        assert!(hints.paths().base.ends_with("bili-12345.anchor.txt"));
        let base = fs::read_to_string(&hints.paths().base).unwrap();
        assert_eq!(base, "主播 \"标题\" & 测试 尾部 - 直播间");
        let media = fs::read_to_string(&hints.paths().media).unwrap();
        assert_eq!(media, "直播间");
        let bitrate = fs::read_to_string(&hints.paths().bitrate).unwrap();
        assert_eq!(bitrate, "4000");
        // Dropping the last handle removes the session directory.
        let session_directory = hints.paths().directory.clone();
        drop(hints);
        assert!(!session_directory.exists());
    }

    #[test]
    fn title_hint_text_stays_single_line_and_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let long = format!("{}\n{}", "标".repeat(2000), "题");
        let hints = SessionHints::prepare_in(
            directory.path(),
            HintSpec {
                platform: Platform::Huya,
                room: "synthetic-room",
                base: &long,
                media: &long,
                relay: false,
                bitrate_kbps: None,
                room_observation: None,
            },
        )
        .unwrap();
        let text = fs::read_to_string(&hints.paths().base).unwrap();
        assert!(text.chars().count() <= TITLE_HINT_MAX_CHARS);
        assert!(!text.contains('\n') && !text.contains('\r'));
        // Without a relay the mpv command line must not receive relay hints.
        let options = hints.script_opts(
            Path::new("C:/package/assets/lua/live-title.lua"),
            None,
        );
        assert!(
            !options
                .iter()
                .any(|argument| argument.to_string_lossy().contains("live_title_relay_file"))
        );
    }

    #[test]
    fn relay_state_hints_track_positive_relay_states() {
        let telemetry = |state: &str, waiting: Option<&str>| RelayTelemetry {
            state: state.to_owned(),
            waiting_state: waiting.map(str::to_owned),
            ..Default::default()
        };
        assert_eq!(relay_hint(&telemetry("streaming", None)), Some("ONLINE"));
        assert_eq!(
            relay_hint(&telemetry("connecting", None)),
            Some("RECONNECTING")
        );
        assert_eq!(
            relay_hint(&telemetry("retrying", None)),
            Some("RECONNECTING")
        );
        assert_eq!(
            relay_hint(&telemetry("waiting", Some("offline"))),
            Some("OFFLINE")
        );
        assert_eq!(
            relay_hint(&telemetry("waiting", Some("replay_unavailable"))),
            Some("REPLAY-UNAVAILABLE|重播流不可用")
        );
        assert_eq!(relay_hint(&telemetry("ended", None)), None);
        assert_eq!(relay_hint(&telemetry("player_disconnected", None)), None);
    }

    #[test]
    fn automatic_relay_state_hints_stay_inside_the_session_directory() {
        let directory = tempfile::tempdir().unwrap();
        let hints = SessionHints::prepare_in(
            directory.path(),
            HintSpec {
                platform: Platform::Douyu,
                room: "synthetic/room",
                base: "合成直播",
                media: "合成直播",
                relay: true,
                bitrate_kbps: None,
                room_observation: None,
            },
        )
        .unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let observer: PlaybackObserver = Arc::new(move |event| {
            sink.lock().unwrap().push(event);
        });
        let observer = observer_with_hints(observer, Some(hints.clone()));
        let relay = hints.paths().relay.clone().unwrap();
        assert_eq!(fs::read_to_string(&relay).unwrap(), "");
        observer(PlaybackEvent::RelayStatistics {
            telemetry: RelayTelemetry {
                state: "streaming".into(),
                ..Default::default()
            },
        });
        assert_eq!(fs::read_to_string(&relay).unwrap(), "ONLINE");
        observer(PlaybackEvent::RelayExited { exit_code: Some(1) });
        assert_eq!(fs::read_to_string(&relay).unwrap(), "RECONNECTING");
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "the wrapped observer must still deliver every event"
        );
        let session_directory = hints.paths().directory.clone();
        drop(observer);
        drop(hints);
        assert!(!session_directory.exists());
    }

    fn synthetic_stream() -> ResolvedStream {
        ResolvedStream {
            url: "https://media.example.test/live.flv".into(),
            headers: BTreeMap::new(),
            title: "Synthetic 直播间".into(),
        }
    }

    #[test]
    fn relay_ready_handshake_accepts_the_protocol_line() {
        let directory = tempfile::tempdir().unwrap();
        let lock = directory.path().join("ready.lock");
        let child = synthetic_child_command("ready_lock", &lock)
            .spawn()
            .unwrap();
        let (mut child, url) =
            await_relay_ready(child, "synthetic-token", Duration::from_secs(10)).unwrap();
        assert_eq!(url, "http://127.0.0.1:45678/live.flv?t=synthetic-token");
        terminate(&mut child);
        assert!(
            wait_for_lock(&lock, false, Duration::from_secs(5)),
            "terminated relay child kept running"
        );
    }

    #[test]
    fn relay_ready_handshake_reports_early_exit_and_kills_on_timeout() {
        let directory = tempfile::tempdir().unwrap();
        // EOF before READY: actionable error naming the readiness handshake.
        let lock = directory.path().join("eof.lock");
        let child = synthetic_child_command("exit_no_ready", &lock)
            .spawn()
            .unwrap();
        let error =
            await_relay_ready(child, "synthetic-token", Duration::from_secs(10)).unwrap_err();
        assert!(format!("{error:#}").contains("就绪"), "{error:#}");

        // READY never arrives: the child is terminated and reaped, not leaked.
        let lock = directory.path().join("timeout.lock");
        let child = synthetic_child_command("no_ready_lock", &lock)
            .spawn()
            .unwrap();
        assert!(
            wait_for_lock(&lock, true, Duration::from_secs(10)),
            "synthetic relay did not start"
        );
        let started = Instant::now();
        let error =
            await_relay_ready(child, "synthetic-token", Duration::from_millis(300)).unwrap_err();
        assert!(format!("{error:#}").contains("超时"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            wait_for_lock(&lock, false, Duration::from_secs(5)),
            "timed-out relay child was not terminated"
        );
    }

    #[test]
    fn launch_terminates_the_relay_when_the_player_cannot_start() {
        let directory = tempfile::tempdir().unwrap();
        let lock = directory.path().join("player-failure.lock");
        let relay = synthetic_child_command("ready_lock", &lock)
            .spawn()
            .unwrap();
        assert!(wait_for_lock(&lock, true, Duration::from_secs(10)));
        let target = Target::new(Platform::Douyu, "synthetic-room").unwrap();
        let missing = directory.path().join("missing").join("mpv.exe");
        let error = launch_resolved(
            &target,
            Player::Mpv,
            missing,
            &synthetic_stream(),
            |_| {
                Ok((
                    relay,
                    "http://127.0.0.1:45678/live.flv?t=synthetic".to_owned(),
                ))
            },
            |_, _| Ok(()),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("无法启动播放器"), "{error:#}");
        assert!(
            wait_for_lock(&lock, false, Duration::from_secs(5)),
            "relay was left running after the player failed to start"
        );
    }

    #[test]
    fn supervisor_terminates_the_relay_after_the_player_exits() {
        let directory = tempfile::tempdir().unwrap();
        let lock = directory.path().join("supervisor.lock");
        let relay = synthetic_child_command("ready_lock", &lock)
            .spawn()
            .unwrap();
        assert!(wait_for_lock(&lock, true, Duration::from_secs(10)));
        let target = Target::new(Platform::Douyu, "synthetic-room").unwrap();
        // The synthetic player is this test binary: libtest rejects the mpv
        // arguments and exits immediately, which is exactly what is needed.
        let outcome = launch_resolved(
            &target,
            Player::Mpv,
            std::env::current_exe().unwrap(),
            &synthetic_stream(),
            |_| {
                Ok((
                    relay,
                    "http://127.0.0.1:45678/live.flv?t=synthetic".to_owned(),
                ))
            },
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(outcome.title, "Synthetic 直播间");
        assert!(outcome.player_pid > 0);
        assert!(
            wait_for_lock(&lock, false, Duration::from_secs(10)),
            "relay child outlived the player it served"
        );
    }

    #[test]
    fn failed_independent_registration_cleans_up_the_owned_relay() {
        let directory = tempfile::tempdir().unwrap();
        let lock = directory.path().join("registration-failure.lock");
        let relay = synthetic_child_command("ready_lock", &lock)
            .spawn()
            .unwrap();
        assert!(wait_for_lock(&lock, true, Duration::from_secs(10)));
        let target = Target::new(Platform::Douyu, "synthetic-room").unwrap();
        let error = launch_resolved(
            &target,
            Player::Mpv,
            std::env::current_exe().unwrap(),
            &synthetic_stream(),
            |_| {
                Ok((
                    relay,
                    "http://127.0.0.1:45678/live.flv?t=synthetic".to_owned(),
                ))
            },
            |_, pid| {
                assert!(pid > 0);
                bail!("synthetic registration rejection")
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("绑定播放器生命周期"));
        assert!(
            wait_for_lock(&lock, false, Duration::from_secs(5)),
            "failed registration left its relay running"
        );
    }

    #[test]
    fn bili_explicit_direct_mode_uses_the_stream_without_starting_a_relay() {
        let target = Target::new(Platform::Bili, "123456").unwrap();
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&started);
        let outcome = launch_resolved(
            &target,
            Player::Mpv,
            std::env::current_exe().unwrap(),
            &synthetic_stream(),
            move |_| {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                bail!("Bili playback must not start a relay")
            },
            |_, _| Ok(()),
        )
        .unwrap();
        assert!(!started.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(outcome.title, "Synthetic 直播间");
    }

    #[test]
    fn cancelling_a_slow_platform_operation_returns_before_it_finishes() {
        let control = PlaybackControl::default();
        let cancel = control.clone();
        let (completed_sender, completed_receiver) = mpsc::sync_channel(1);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(25));
            assert!(cancel.cancel_start());
        });
        let start = Instant::now();
        let result = cancellable_request(&control, move || -> Result<()> {
            thread::sleep(Duration::from_millis(350));
            let _ = completed_sender.send(());
            Ok(())
        });
        assert!(
            result
                .unwrap_err()
                .downcast_ref::<LaunchCancelled>()
                .is_some()
        );
        assert!(start.elapsed() < Duration::from_millis(250));
        assert!(
            completed_receiver
                .recv_timeout(Duration::from_secs(2))
                .is_ok()
        );
    }

    #[test]
    fn cancellation_cannot_cross_the_atomic_playing_boundary() {
        let cancelled = PlaybackControl::default();
        assert!(cancelled.cancel_start());
        assert!(cancelled.commit_running().is_err());
        let playing = PlaybackControl::default();
        playing.commit_running().unwrap();
        assert!(!playing.cancel_start());
        assert!(playing.is_running());
        assert!(!playing.is_stop_requested());
        playing.stop();
        assert!(playing.is_stop_requested());
    }

    #[test]
    fn ready_wait_cancellation_kills_and_reaps_only_the_owned_relay() {
        let directory = tempfile::tempdir().unwrap();
        let lock = directory.path().join("cancel-ready.lock");
        let child = synthetic_child_command("no_ready_lock", &lock)
            .spawn()
            .unwrap();
        assert!(wait_for_lock(&lock, true, Duration::from_secs(10)));
        let control = PlaybackControl::default();
        let cancel = control.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            cancel.cancel_start();
        });
        let start = Instant::now();
        let error =
            await_relay_ready_controlled(child, "synthetic", Duration::from_secs(10), &control)
                .unwrap_err();
        assert!(error.downcast_ref::<LaunchCancelled>().is_some());
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(wait_for_lock(&lock, false, Duration::from_secs(2)));
    }

    #[test]
    fn supervisor_reports_relay_failure_then_stops_the_owned_player() {
        let directory = tempfile::tempdir().unwrap();
        let lock = directory.path().join("stoppable-player.lock");
        let player = synthetic_child_command("ready_lock", &lock)
            .spawn()
            .unwrap();
        assert!(wait_for_lock(&lock, true, Duration::from_secs(10)));
        let relay = synthetic_child_command("exit_no_ready", &directory.path().join("unused.lock"))
            .spawn()
            .unwrap();
        let control = PlaybackControl::default();
        control.commit_running().unwrap();
        let worker_control = control.clone();
        let (sender, receiver) = mpsc::channel();
        let observer: PlaybackObserver = Arc::new(move |event| {
            let _ = sender.send(event);
        });
        let room_watch_stop = Arc::new(AtomicBool::new(false));
        let worker = thread::spawn(move || {
            supervise_controlled(
                player,
                Some(relay),
                None,
                worker_control,
                observer,
                room_watch_stop,
            )
        });
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(10)).unwrap(),
            PlaybackEvent::RelayExited { .. }
        ));
        assert!(
            wait_for_lock(&lock, true, Duration::from_secs(1)),
            "relay failure should preserve the player window"
        );
        control.stop();
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(5)).unwrap(),
            PlaybackEvent::Stopped
        );
        worker.join().unwrap();
        assert!(wait_for_lock(&lock, false, Duration::from_secs(2)));
    }

    #[test]
    fn preferences_change_buffer_and_enhancement_without_changing_media_argument() {
        let stream = synthetic_stream();
        let preferences = PlaybackPreferences {
            buffer_mode: BufferMode::Stable,
            enhancement: Enhancement::High,
            ..Default::default()
        };
        let plan = plan_with_preferences(
            Player::Mpv,
            "synthetic-mpv.exe".into(),
            &stream,
            &preferences,
        )
        .unwrap();
        assert!(
            plan.arguments
                .iter()
                .any(|argument| argument == "--cache-secs=15")
        );
        assert!(
            plan.arguments
                .iter()
                .any(|argument| argument == "--demuxer-readahead-secs=10")
        );
        assert!(
            plan.arguments
                .iter()
                .any(|argument| argument == "--vo=gpu-next,gpu")
        );
        assert!(
            plan.arguments
                .iter()
                .any(|argument| argument == "--profile=high-quality")
        );
        assert_eq!(plan.arguments.last().unwrap(), &OsString::from(stream.url));
    }

    #[test]
    fn an_explicit_offline_room_reopens_without_a_second_user_start() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let calls = attempts.clone();
        let lookup: StreamLookup = Arc::new(move || {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(platforms::AdapterError::Offline.into())
            } else {
                Ok((synthetic_stream(), platforms::StreamHints::default()))
            }
        });
        let room: RoomLookup = Arc::new(|| {
            Ok(crate::room_state::RoomObservation {
                state: crate::room_state::RoomState::Offline,
                title: None,
                note: "synthetic offline".into(),
            })
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        let observer_events = events.clone();
        let observer: PlaybackObserver =
            Arc::new(move |event| observer_events.lock().unwrap().push(event));
        let stream = resolve_until_ready(
            &PlaybackControl::default(),
            &observer,
            true,
            lookup,
            room,
            Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(stream.0.title, "Synthetic 直播间");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, PlaybackEvent::WaitingForLive { .. }))
        );
    }

    #[test]
    fn replay_unknown_or_failed_state_queries_do_not_become_offline_waiting() {
        for state in [
            crate::room_state::RoomState::Replay,
            crate::room_state::RoomState::Unknown,
            crate::room_state::RoomState::Live,
        ] {
            let events = Arc::new(Mutex::new(Vec::new()));
            let copied = events.clone();
            let observer: PlaybackObserver =
                Arc::new(move |event| copied.lock().unwrap().push(event));
            let lookup: StreamLookup = Arc::new(|| Err(platforms::AdapterError::Offline.into()));
            let room: RoomLookup = Arc::new(move || {
                Ok(crate::room_state::RoomObservation {
                    state,
                    title: None,
                    note: "synthetic".into(),
                })
            });
            assert!(
                resolve_until_ready(
                    &PlaybackControl::default(),
                    &observer,
                    true,
                    lookup,
                    room,
                    Duration::from_millis(1)
                )
                .is_err()
            );
            let observed = events.lock().unwrap();
            assert!(observed.iter().any(
                |event| matches!(event,PlaybackEvent::RoomObserved{state:seen,..} if *seen==state)
            ));
            assert!(
                !observed
                    .iter()
                    .any(|event| matches!(event, PlaybackEvent::WaitingForLive { .. }))
            );
        }
        let lookup: StreamLookup = Arc::new(|| Err(platforms::AdapterError::Offline.into()));
        let room: RoomLookup =
            Arc::new(|| Err(platforms::AdapterError::VerificationRequired.into()));
        let observer: PlaybackObserver = Arc::new(|_| {});
        let error = resolve_until_ready(
            &PlaybackControl::default(),
            &observer,
            true,
            lookup,
            room,
            Duration::from_millis(1),
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<platforms::AdapterError>(),
            Some(&platforms::AdapterError::VerificationRequired)
        );
    }

    #[test]
    fn offline_wait_is_cancellable_and_disabled_wait_does_not_probe_room() {
        let lookup: StreamLookup = Arc::new(|| Err(platforms::AdapterError::Offline.into()));
        let room_calls = Arc::new(AtomicUsize::new(0));
        let calls = room_calls.clone();
        let room: RoomLookup = Arc::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(crate::room_state::RoomObservation {
                state: crate::room_state::RoomState::Offline,
                title: None,
                note: "synthetic".into(),
            })
        });
        let observer: PlaybackObserver = Arc::new(|_| {});
        assert!(
            resolve_until_ready(
                &PlaybackControl::default(),
                &observer,
                false,
                lookup.clone(),
                room.clone(),
                Duration::from_secs(60)
            )
            .is_err()
        );
        assert_eq!(room_calls.load(Ordering::SeqCst), 0);
        let control = PlaybackControl::default();
        let cancel = control.clone();
        let start = Instant::now();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(35));
            cancel.cancel_start();
        });
        let error = resolve_until_ready(
            &control,
            &observer,
            true,
            lookup,
            room,
            Duration::from_secs(60),
        )
        .unwrap_err();
        assert!(error.downcast_ref::<LaunchCancelled>().is_some());
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(room_calls.load(Ordering::SeqCst), 1);
    }
}
