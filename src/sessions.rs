//! An in-memory workbench for independently owned playback sessions.
//!
//! Persistence, GUI widgets and platform cookies are not owned here. Every
//! cancellation/stop request addresses one UUID and only that launch's children.
use crate::auth::CookieVault;
use crate::core::{Player, Settings, Target};
use crate::playback::{
    self, LaunchCancelled, LaunchOutcome, PlaybackControl, PlaybackEvent, PlaybackObserver,
    RelayTelemetry,
};
use anyhow::{Result, anyhow, bail, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    mpsc::{self, Receiver, SyncSender, TrySendError},
};
use std::thread;
use std::time::{Duration, Instant};

pub const MAX_ACTIVE_SESSIONS: usize = 16;
pub const MAX_TRACKED_SESSIONS: usize = 128;
const MONITOR_INTERVAL: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Queued,
    CheckingPlayer,
    Resolving,
    WaitingForLive,
    WaitingRelay,
    StartingPlayer,
    RegisteringPlayer,
    Playing,
    WaitingBroadcast,
    Interrupted,
    Stopping,
    Failed,
    Cancelled,
    Ended,
    Stopped,
}

impl SessionPhase {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Queued => "排队中",
            Self::CheckingPlayer => "检查播放器",
            Self::Resolving => "解析取流",
            Self::WaitingForLive => "等待开播",
            Self::WaitingRelay => "等待中转",
            Self::StartingPlayer => "启动播放器",
            Self::RegisteringPlayer => "绑定播放进程",
            Self::Playing => "播放中",
            Self::Interrupted => "中转已中断",
            Self::WaitingBroadcast => "等待重开",
            Self::Stopping => "正在停止",
            Self::Failed => "启动失败",
            Self::Cancelled => "已取消",
            Self::Ended => "已结束",
            Self::Stopped => "已停止",
        }
    }
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Failed | Self::Cancelled | Self::Ended | Self::Stopped
        )
    }
    pub const fn can_cancel(self) -> bool {
        matches!(
            self,
            Self::Queued
                | Self::CheckingPlayer
                | Self::Resolving
                | Self::WaitingForLive
                | Self::WaitingRelay
                | Self::StartingPlayer
                | Self::RegisteringPlayer
        )
    }
    pub const fn can_stop(self) -> bool {
        matches!(
            self,
            Self::Playing | Self::WaitingBroadcast | Self::Interrupted
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRoomState {
    Live,
    Offline,
    Replay,
    Unknown,
}

impl SessionRoomState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Live => "直播中",
            Self::Offline => "已下播",
            Self::Replay => "重播中",
            Self::Unknown => "状态未知",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub id: String,
    pub target: Target,
    pub player: Player,
    pub title: String,
    pub player_pid: Option<u32>,
    pub phase: SessionPhase,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub room_state: SessionRoomState,
    pub room_note: String,
    pub room_observed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub relay: Option<RelayTelemetry>,
}

struct Entry {
    snapshot: SessionSnapshot,
    control: PlaybackControl,
}
struct State {
    entries: BTreeMap<String, Entry>,
    subscribers: Vec<SyncSender<SessionSnapshot>>,
}
struct Inner {
    state: Mutex<State>,
    launcher: Arc<dyn SessionLauncher>,
}

/// Injectable launch contract, also used by embedders and offline regression
/// tests. Production uses the same `playback::launch_controlled` as the CLI.
pub trait SessionLauncher: Send + Sync {
    fn launch(
        &self,
        target: &Target,
        player: Player,
        settings: &Settings,
        vault: Option<&CookieVault>,
        control: &PlaybackControl,
        observer: PlaybackObserver,
    ) -> Result<LaunchOutcome>;
}

struct NativeLauncher;
impl SessionLauncher for NativeLauncher {
    fn launch(
        &self,
        target: &Target,
        player: Player,
        settings: &Settings,
        vault: Option<&CookieVault>,
        control: &PlaybackControl,
        observer: PlaybackObserver,
    ) -> Result<LaunchOutcome> {
        playback::launch_controlled(target, player, settings, vault, control, observer)
    }
}

#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<Inner>,
}
impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    pub fn new() -> Self {
        Self::with_launcher(Arc::new(NativeLauncher))
    }
    pub fn with_launcher(launcher: Arc<dyn SessionLauncher>) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    entries: BTreeMap::new(),
                    subscribers: Vec::new(),
                }),
                launcher,
            }),
        }
    }

    /// Loss-tolerant notifications are hints; `snapshots` remains the source of
    /// truth if a subscriber falls behind. This never blocks a playback worker.
    pub fn subscribe(&self) -> Receiver<SessionSnapshot> {
        let (sender, receiver) = mpsc::sync_channel(128);
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.subscribers.len() >= 16 {
            state.subscribers.remove(0);
        }
        state.subscribers.push(sender);
        receiver
    }

    pub fn snapshots(&self) -> Vec<SessionSnapshot> {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut result: Vec<_> = state
            .entries
            .values()
            .map(|entry| entry.snapshot.clone())
            .collect();
        result.sort_by(|a, b| {
            b.started_at
                .cmp(&a.started_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        result
    }

    pub fn snapshot(&self, id: &str) -> Option<SessionSnapshot> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entries
            .get(id)
            .map(|entry| entry.snapshot.clone())
    }

    /// Return immediately after scheduling a bounded asynchronous launch.
    pub fn start(
        &self,
        target: Target,
        player: Player,
        settings: Settings,
        vault: Option<CookieVault>,
    ) -> Result<String> {
        target.validate()?;
        settings.validate()?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        let control = PlaybackControl::default();
        {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            ensure!(
                state
                    .entries
                    .values()
                    .filter(|entry| !entry.snapshot.phase.is_terminal())
                    .count()
                    < MAX_ACTIVE_SESSIONS,
                "正在运行或启动的会话过多，请先取消或停止部分会话"
            );
            while state.entries.len() >= MAX_TRACKED_SESSIONS {
                let oldest = state
                    .entries
                    .values()
                    .filter(|entry| entry.snapshot.phase.is_terminal())
                    .min_by_key(|entry| entry.snapshot.started_at)
                    .map(|entry| entry.snapshot.id.clone());
                if let Some(oldest) = oldest {
                    state.entries.remove(&oldest);
                } else {
                    bail!("会话列表已满");
                }
            }
            let now = Utc::now();
            let snapshot = SessionSnapshot {
                id: id.clone(),
                target: target.clone(),
                player,
                title: format!("{} · {}", target.platform.label(), target.room),
                player_pid: None,
                phase: SessionPhase::Queued,
                error: None,
                started_at: now,
                updated_at: now,
                ended_at: None,
                room_state: SessionRoomState::Unknown,
                room_note: String::new(),
                room_observed_at: None,
                relay: None,
            };
            state.entries.insert(
                id.clone(),
                Entry {
                    snapshot: snapshot.clone(),
                    control: control.clone(),
                },
            );
            broadcast(&mut state, &snapshot);
        }
        let manager = self.clone();
        let worker_id = id.clone();
        let observer_manager = self.clone();
        let observer_id = id.clone();
        let observer: PlaybackObserver =
            Arc::new(move |event| observer_manager.on_playback(&observer_id, event));
        let worker = thread::Builder::new()
            .name("playback-session".into())
            .spawn(move || {
                let launched = manager.inner.launcher.launch(
                    &target,
                    player,
                    &settings,
                    vault.as_ref(),
                    &control,
                    observer,
                );
                match launched {
                    Ok(_) => {
                        if settings.playback.monitor_room_state {
                            manager.monitor_room(&worker_id, target, vault);
                        }
                    }
                    Err(error) => {
                        manager.launch_failed(&worker_id, error, control.is_start_cancelled())
                    }
                }
            });
        if let Err(error) = worker {
            self.launch_failed(&id, error.into(), false);
            return Err(anyhow!("无法启动会话线程"));
        }
        Ok(id)
    }

    pub fn cancel_start(&self, id: &str) -> Result<()> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = state
            .entries
            .get_mut(id)
            .ok_or_else(|| anyhow!("找不到这个观看会话"))?;
        ensure!(
            entry.snapshot.phase.can_cancel(),
            "会话已开始播放或结束；播放中请使用停止"
        );
        ensure!(
            entry.control.cancel_start(),
            "会话刚刚开始播放，请刷新后使用停止"
        );
        entry.snapshot.phase = SessionPhase::Cancelled;
        entry.snapshot.updated_at = Utc::now();
        entry.snapshot.ended_at = Some(entry.snapshot.updated_at);
        let snapshot = entry.snapshot.clone();
        broadcast(&mut state, &snapshot);
        Ok(())
    }

    pub fn stop(&self, id: &str) -> Result<()> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = state
            .entries
            .get_mut(id)
            .ok_or_else(|| anyhow!("找不到这个观看会话"))?;
        ensure!(
            entry.snapshot.phase.can_stop(),
            "会话当前未在播放；启动中请使用取消"
        );
        entry.control.stop();
        entry.snapshot.phase = SessionPhase::Stopping;
        entry.snapshot.updated_at = Utc::now();
        let snapshot = entry.snapshot.clone();
        broadcast(&mut state, &snapshot);
        Ok(())
    }

    /// Never removes or stops a live session; it only clears finished rows.
    pub fn clear_finished(&self) -> usize {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let before = state.entries.len();
        state
            .entries
            .retain(|_, entry| !entry.snapshot.phase.is_terminal());
        before - state.entries.len()
    }

    pub fn update_room_status(
        &self,
        id: &str,
        room_state: SessionRoomState,
        title: Option<String>,
        note: String,
    ) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(entry) = state.entries.get_mut(id) else {
            return;
        };
        if entry.snapshot.phase.is_terminal() {
            return;
        }
        entry.snapshot.room_state = room_state;
        entry.snapshot.room_note = bounded_text(&note, 256);
        if let Some(title) = title.filter(|title| !title.trim().is_empty()) {
            entry.snapshot.title = bounded_text(&title, 512);
        }
        entry.snapshot.room_observed_at = Some(Utc::now());
        entry.snapshot.updated_at = Utc::now();
        let snapshot = entry.snapshot.clone();
        broadcast(&mut state, &snapshot);
    }

    fn on_playback(&self, id: &str, event: PlaybackEvent) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(entry) = state.entries.get_mut(id) else {
            return;
        };
        if entry.snapshot.phase.is_terminal() {
            return;
        }
        match event {
            PlaybackEvent::CheckingPlayer => entry.snapshot.phase = SessionPhase::CheckingPlayer,
            PlaybackEvent::Resolving => entry.snapshot.phase = SessionPhase::Resolving,
            PlaybackEvent::RoomObserved {
                state: observed,
                title,
                note,
            } => {
                entry.snapshot.room_state = match observed {
                    crate::room_state::RoomState::Live => SessionRoomState::Live,
                    crate::room_state::RoomState::Offline => SessionRoomState::Offline,
                    crate::room_state::RoomState::Replay => SessionRoomState::Replay,
                    crate::room_state::RoomState::Unknown => SessionRoomState::Unknown,
                };
                entry.snapshot.room_note = bounded_text(&note, 256);
                entry.snapshot.room_observed_at = Some(Utc::now());
                if let Some(title) = title.filter(|title| !title.trim().is_empty()) {
                    entry.snapshot.title = bounded_text(&title, 512);
                }
            }
            PlaybackEvent::WaitingForLive { note } => {
                entry.snapshot.phase = SessionPhase::WaitingForLive;
                entry.snapshot.room_state = SessionRoomState::Offline;
                entry.snapshot.room_note = bounded_text(&note, 256);
            }
            PlaybackEvent::WaitingRelay => entry.snapshot.phase = SessionPhase::WaitingRelay,
            PlaybackEvent::StartingPlayer => entry.snapshot.phase = SessionPhase::StartingPlayer,
            PlaybackEvent::RegisteringPlayer => {
                entry.snapshot.phase = SessionPhase::RegisteringPlayer
            }
            PlaybackEvent::Playing { title, player_pid } => {
                entry.snapshot.phase = SessionPhase::Playing;
                entry.snapshot.player_pid = Some(player_pid);
                entry.snapshot.title = bounded_text(&title, 512);
            }
            PlaybackEvent::RelayStatistics { telemetry } => {
                if telemetry.state == "waiting" {
                    if entry.snapshot.player_pid.is_some()
                        && matches!(
                            entry.snapshot.phase,
                            SessionPhase::Playing | SessionPhase::WaitingBroadcast
                        )
                    {
                        entry.snapshot.phase = SessionPhase::WaitingBroadcast;
                    }
                    match telemetry.waiting_state.as_deref() {
                        Some("offline") => {
                            entry.snapshot.room_state = SessionRoomState::Offline;
                            entry.snapshot.room_note = "已下播，中转保留连接等待重开".into();
                        }
                        Some("replay_unavailable") => {
                            entry.snapshot.room_state = SessionRoomState::Replay;
                            entry.snapshot.room_note =
                                "平台正在重播，等待可用重播流或重新开播".into();
                        }
                        _ => (),
                    }
                    entry.snapshot.room_observed_at = Some(Utc::now());
                } else if telemetry.state == "streaming" {
                    if entry.snapshot.phase == SessionPhase::WaitingBroadcast {
                        entry.snapshot.phase = SessionPhase::Playing;
                        entry.snapshot.room_state = SessionRoomState::Unknown;
                        entry.snapshot.room_note = "媒体已恢复，等待确认直播或重播状态".into();
                    }
                    if entry.snapshot.phase == SessionPhase::Interrupted
                        && entry
                            .snapshot
                            .error
                            .as_deref()
                            .is_some_and(|error| error.starts_with("中转连接"))
                    {
                        entry.snapshot.phase = SessionPhase::Playing;
                        entry.snapshot.error = None;
                    }
                } else if telemetry.state == "exhausted"
                    && entry.snapshot.player_pid.is_some()
                    && entry.snapshot.phase == SessionPhase::Playing
                {
                    entry.snapshot.phase = SessionPhase::Interrupted;
                    entry.snapshot.error =
                        Some("中转连接的连续失败预算已耗尽，等待播放器重连或手动重试".into());
                }
                entry.snapshot.relay = Some(telemetry);
            }
            PlaybackEvent::RelayExited { exit_code } => {
                entry.snapshot.phase = SessionPhase::Interrupted;
                entry.snapshot.error = Some(format!(
                    "中转进程已退出（{}），播放器窗口保留，可停止后重试",
                    exit_code
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "状态未知".into())
                ));
            }
            PlaybackEvent::Ended { exit_code } => {
                entry.snapshot.phase = SessionPhase::Ended;
                if let Some(code) = exit_code.filter(|code| *code != 0) {
                    entry.snapshot.error = Some(format!("播放器已退出，退出码 {code}"));
                }
            }
            PlaybackEvent::Stopped => entry.snapshot.phase = SessionPhase::Stopped,
        }
        entry.snapshot.updated_at = Utc::now();
        if entry.snapshot.phase.is_terminal() {
            entry.snapshot.ended_at = Some(entry.snapshot.updated_at);
        }
        let snapshot = entry.snapshot.clone();
        broadcast(&mut state, &snapshot);
    }

    fn launch_failed(&self, id: &str, error: anyhow::Error, cancelled: bool) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(entry) = state.entries.get_mut(id) else {
            return;
        };
        if entry.snapshot.phase.is_terminal() {
            return;
        }
        if cancelled || error.downcast_ref::<LaunchCancelled>().is_some() {
            entry.snapshot.phase = SessionPhase::Cancelled;
        } else {
            entry.snapshot.phase = SessionPhase::Failed;
            entry.snapshot.error = Some(bounded_text(&format!("{error:#}"), 512));
        }
        entry.snapshot.updated_at = Utc::now();
        entry.snapshot.ended_at = Some(entry.snapshot.updated_at);
        let snapshot = entry.snapshot.clone();
        broadcast(&mut state, &snapshot);
    }

    fn monitor_room(&self, id: &str, target: Target, vault: Option<CookieVault>) {
        let mut next = Instant::now() + MONITOR_INTERVAL;
        loop {
            let Some(snapshot) = self.snapshot(id) else {
                return;
            };
            if snapshot.phase.is_terminal() || snapshot.phase == SessionPhase::Stopping {
                return;
            }
            if Instant::now() < next {
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            match crate::room_state::check(&target, vault.as_ref()) {
                Ok(observation) => {
                    let state = match observation.state {
                        crate::room_state::RoomState::Live => SessionRoomState::Live,
                        crate::room_state::RoomState::Offline => SessionRoomState::Offline,
                        crate::room_state::RoomState::Replay => SessionRoomState::Replay,
                        crate::room_state::RoomState::Unknown => SessionRoomState::Unknown,
                    };
                    self.update_room_status(id, state, observation.title, observation.note);
                }
                Err(_) => self.update_room_status(
                    id,
                    SessionRoomState::Unknown,
                    None,
                    "平台状态暂时无法确认；当前播放不受影响".into(),
                ),
            }
            next = Instant::now() + MONITOR_INTERVAL;
        }
    }
}

fn bounded_text(text: &str, max: usize) -> String {
    text.chars()
        .filter(|character| !character.is_control() || *character == '\n')
        .take(max)
        .collect()
}
fn broadcast(state: &mut State, snapshot: &SessionSnapshot) {
    state
        .subscribers
        .retain(|sender| match sender.try_send(snapshot.clone()) {
            Ok(()) | Err(TrySendError::Full(_)) => true,
            Err(TrySendError::Disconnected(_)) => false,
        });
}
