//! Bounded local "last observed" records for the platform-status page.
//!
//! The launcher only records what it observed itself: room-state polls, playback
//! launch phases and overlay launches. These are local observations, never a
//! platform probe and never login evidence. Records live under the isolated data
//! directory so a restart can still report when each stage last ran.

use crate::core::Platform;
use crate::sessions::{SessionPhase, SessionRoomState};
use anyhow::{Context, Result, ensure};
use chrono::TimeZone;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// Records older than this only mean the observation is stale; they never mean
/// a saved credential expired.
pub const OBSERVATION_STALE_SECS: u64 = 300;
const FILE_VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_ROOM_CHARS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationStage {
    Room,
    Playback,
    Danmaku,
}

impl ObservationStage {
    pub const ALL: [Self; 3] = [Self::Room, Self::Playback, Self::Danmaku];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Room => "房间状态",
            Self::Playback => "播放解析",
            Self::Danmaku => "弹幕连接",
        }
    }

    pub const fn key(self) -> &'static str {
        match self {
            Self::Room => "room",
            Self::Playback => "playback",
            Self::Danmaku => "danmaku",
        }
    }
}

/// One observed state. The wording of a state may differ per stage, so labels
/// are resolved with the stage in hand instead of being serialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationState {
    Live,
    Offline,
    Replay,
    Unknown,
    Waiting,
    Playing,
    Ready,
    Retrying,
    Stopped,
    Failed,
    ParseError,
    NetworkError,
    AuthRequired,
    Timeout,
    RateLimited,
    OverlayStarted,
    OverlayDisabled,
    OverlayUnsupported,
}

impl ObservationState {
    pub const fn label(self, stage: ObservationStage) -> &'static str {
        match self {
            Self::Live => "正在直播",
            Self::Offline => "房间未开播",
            Self::Replay => "重播 / 录播",
            Self::Unknown => "本次未查到状态",
            Self::Waiting => match stage {
                ObservationStage::Playback => "解析 / 连接中",
                _ => "连接中",
            },
            Self::Playing => "播放中",
            Self::Ready => match stage {
                ObservationStage::Danmaku => "曾连接 / 收到弹幕",
                _ => "曾取得播放地址",
            },
            Self::Retrying => "正在重试",
            Self::Stopped => "连接已停止",
            Self::Failed => match stage {
                ObservationStage::Danmaku => "覆盖层启动失败",
                ObservationStage::Playback => "启动失败",
                ObservationStage::Room => "状态查询失败",
            },
            Self::ParseError => "响应解析失败",
            Self::NetworkError => "网络异常",
            Self::AuthRequired => "认证 / 权限受阻，可能需网页验证",
            Self::Timeout => "请求超时",
            Self::RateLimited => "平台限流（风控），稍后自动重试",
            Self::OverlayStarted => "弹幕覆盖层已启动（弹幕由覆盖层进程接收）",
            Self::OverlayDisabled => "本次未启动弹幕覆盖层（设置已关闭）",
            Self::OverlayUnsupported => "此平台或系统未启动弹幕覆盖层",
        }
    }

    /// Whether an old record of this state claims current activity. Only these
    /// states get the "记录超过 5 分钟，待重新观测" suffix.
    pub const fn is_active(self) -> bool {
        matches!(
            self,
            Self::Live
                | Self::Replay
                | Self::Waiting
                | Self::Playing
                | Self::Ready
                | Self::Retrying
                | Self::OverlayStarted
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub state: ObservationState,
    /// Unix seconds of the local observation.
    pub at: u64,
    #[serde(default)]
    pub room: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformObservation {
    #[serde(default)]
    room: Option<Observation>,
    #[serde(default)]
    playback: Option<Observation>,
    #[serde(default)]
    danmaku: Option<Observation>,
}

impl PlatformObservation {
    fn slot(&self, stage: ObservationStage) -> Option<&Observation> {
        match stage {
            ObservationStage::Room => self.room.as_ref(),
            ObservationStage::Playback => self.playback.as_ref(),
            ObservationStage::Danmaku => self.danmaku.as_ref(),
        }
    }

    fn slot_mut(&mut self, stage: ObservationStage) -> &mut Option<Observation> {
        match stage {
            ObservationStage::Room => &mut self.room,
            ObservationStage::Playback => &mut self.playback,
            ObservationStage::Danmaku => &mut self.danmaku,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationLog {
    #[serde(default)]
    platforms: BTreeMap<Platform, PlatformObservation>,
}

#[derive(Serialize, Deserialize)]
struct StoredObservationLog {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    platforms: BTreeMap<Platform, PlatformObservation>,
}

/// One row of the platform-status table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationRow {
    pub stage: ObservationStage,
    pub result: String,
    pub time: String,
    pub room: String,
    pub stale: bool,
}

impl ObservationLog {
    /// Records one observation and reports whether the stored record changed.
    /// An identical state and room within 30 seconds is folded into the previous
    /// record so play/pause polls do not churn the file.
    pub fn record(
        &mut self,
        platform: Platform,
        stage: ObservationStage,
        state: ObservationState,
        at: u64,
        room: &str,
    ) -> bool {
        let room: String = room.chars().take(MAX_ROOM_CHARS).collect();
        let slot = self.platforms.entry(platform).or_default().slot_mut(stage);
        if let Some(previous) = slot {
            if previous.state == state
                && previous.room == room
                && at.saturating_sub(previous.at) < 30
            {
                return false;
            }
        }
        *slot = Some(Observation { state, at, room });
        true
    }

    pub fn get(&self, platform: Platform, stage: ObservationStage) -> Option<&Observation> {
        self.platforms.get(&platform)?.slot(stage)
    }

    /// A phase-only convenience for callers that do not carry a room string.
    pub fn record_state(
        &mut self,
        platform: Platform,
        stage: ObservationStage,
        state: ObservationState,
        at: u64,
    ) -> bool {
        self.record(platform, stage, state, at, "")
    }

    /// The three status-table rows for one platform at the given time.
    pub fn rows(&self, platform: Platform, now: u64) -> [ObservationRow; 3] {
        ObservationStage::ALL.map(|stage| {
            let Some(observation) = self.get(platform, stage) else {
                return ObservationRow {
                    stage,
                    result: "尚无记录".to_owned(),
                    time: "--".to_owned(),
                    room: "--".to_owned(),
                    stale: false,
                };
            };
            let stale = observation.state.is_active()
                && now > observation.at
                && now - observation.at > OBSERVATION_STALE_SECS;
            let mut result = observation.state.label(stage).to_owned();
            if stale {
                result.push_str("（记录超过 5 分钟，待重新观测；不表示 Cookie 过期）");
            }
            ObservationRow {
                stage,
                result,
                time: observation_time_text(observation.at),
                room: if observation.room.is_empty() {
                    "--".to_owned()
                } else {
                    observation.room.clone()
                },
                stale,
            }
        })
    }

    /// Multi-line detail for the account page; one line per stage.
    pub fn detail_text(&self, platform: Platform, now: u64) -> String {
        let mut text = platform.label().to_owned();
        for row in self.rows(platform, now) {
            text.push_str(&format!("\n  {}：{}", row.stage.label(), row.result));
            if row.time != "--" {
                text.push_str(" · ");
                text.push_str(&row.time);
            }
            if row.room != "--" {
                text.push_str(&format!(" · 房间 {}", row.room));
            }
        }
        text
    }

    pub fn is_empty(&self) -> bool {
        self.platforms.is_empty()
    }

    pub fn load(root: &Path) -> Self {
        let path = observation_path(root);
        let Ok(metadata) = fs::metadata(&path) else {
            return Self::default();
        };
        if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
            return Self::default();
        }
        let Ok(text) = fs::read_to_string(&path) else {
            return Self::default();
        };
        let Ok(stored) = serde_json::from_str::<StoredObservationLog>(&text) else {
            return Self::default();
        };
        if stored.version > FILE_VERSION {
            return Self::default();
        }
        Self {
            platforms: stored.platforms,
        }
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        let path = observation_path(root);
        let directory = path.parent().context("观测记录目录无效")?;
        fs::create_dir_all(directory).context("无法创建运行记录目录")?;
        let stored = StoredObservationLog {
            version: FILE_VERSION,
            platforms: self.platforms.clone(),
        };
        let text = serde_json::to_string(&stored).context("无法编码运行观测记录")?;
        ensure!(
            text.len() as u64 <= MAX_FILE_BYTES,
            "运行观测记录超过 256 KiB 限额"
        );
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, text).context("无法写入运行观测记录")?;
        fs::rename(&temporary, &path).context("无法替换运行观测记录")?;
        Ok(())
    }
}

/// The only file this module owns under the data root.
pub fn observation_path(root: &Path) -> PathBuf {
    root.join("runtime").join("platform-observations.json")
}

/// Local display time for an observation; keeps seconds because stages change
/// during one minute.
pub fn observation_time_text(at: u64) -> String {
    if at == 0 {
        return "--".to_owned();
    }
    chrono::Local
        .timestamp_opt(at as i64, 0)
        .single()
        .map(|time| time.format("%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "时间未知".to_owned())
}

pub fn room_state(state: SessionRoomState) -> ObservationState {
    match state {
        SessionRoomState::Live => ObservationState::Live,
        SessionRoomState::Offline => ObservationState::Offline,
        SessionRoomState::Replay => ObservationState::Replay,
        SessionRoomState::Unknown => ObservationState::Unknown,
    }
}

pub fn playback_state(phase: SessionPhase) -> ObservationState {
    match phase {
        SessionPhase::Queued | SessionPhase::CheckingPlayer | SessionPhase::Resolving => {
            ObservationState::Waiting
        }
        SessionPhase::WaitingForLive => ObservationState::Offline,
        SessionPhase::WaitingRelay
        | SessionPhase::StartingPlayer
        | SessionPhase::RegisteringPlayer => ObservationState::Waiting,
        SessionPhase::Playing => ObservationState::Playing,
        SessionPhase::WaitingBroadcast => ObservationState::Retrying,
        SessionPhase::Interrupted => ObservationState::NetworkError,
        SessionPhase::Stopping | SessionPhase::Ended | SessionPhase::Stopped => {
            ObservationState::Stopped
        }
        SessionPhase::Failed => ObservationState::Failed,
        SessionPhase::Cancelled => ObservationState::Stopped,
    }
}

/// Snapshot-agnostic observation capture used by the launcher event loop.
pub fn record_snapshot(log: &mut ObservationLog, snapshot: &crate::sessions::SessionSnapshot) {
    let platform = snapshot.target.platform;
    if let Some(observed) = snapshot.room_observed_at {
        log.record(
            platform,
            ObservationStage::Room,
            room_state(snapshot.room_state),
            observed.timestamp().max(0) as u64,
            &snapshot.target.room,
        );
    }
    let at = snapshot.updated_at.timestamp().max(0) as u64;
    log.record(
        platform,
        ObservationStage::Playback,
        playback_state(snapshot.phase),
        at,
        &snapshot.target.room,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_records_are_folded_but_state_changes_are_kept() {
        let mut log = ObservationLog::default();
        log.record(
            Platform::Bili,
            ObservationStage::Room,
            ObservationState::Live,
            1_000,
            "123",
        );
        log.record(
            Platform::Bili,
            ObservationStage::Room,
            ObservationState::Live,
            1_010,
            "123",
        );
        assert_eq!(
            log.get(Platform::Bili, ObservationStage::Room).unwrap().at,
            1_000
        );
        log.record(
            Platform::Bili,
            ObservationStage::Room,
            ObservationState::Live,
            1_040,
            "123",
        );
        assert_eq!(
            log.get(Platform::Bili, ObservationStage::Room).unwrap().at,
            1_040
        );
        log.record(
            Platform::Bili,
            ObservationStage::Room,
            ObservationState::Offline,
            1_050,
            "123",
        );
        assert_eq!(
            log.get(Platform::Bili, ObservationStage::Room)
                .unwrap()
                .state,
            ObservationState::Offline
        );
        assert!(log.get(Platform::Huya, ObservationStage::Room).is_none());
    }

    #[test]
    fn stale_rule_only_marks_active_observations_and_never_claims_expiry() {
        let mut log = ObservationLog::default();
        log.record_state(
            Platform::Bili,
            ObservationStage::Playback,
            ObservationState::Playing,
            1_000,
        );
        let fresh = log.rows(Platform::Bili, 1_299);
        assert!(!fresh[1].stale);
        assert!(!fresh[1].result.contains("待重新观测"));
        let stale = log.rows(Platform::Bili, 1_301);
        assert!(stale[1].stale);
        assert!(stale[1].result.contains("待重新观测"));
        assert!(stale[1].result.contains("不表示 Cookie 过期"));
        log.record_state(
            Platform::Huya,
            ObservationStage::Playback,
            ObservationState::Stopped,
            1_000,
        );
        assert!(!log.rows(Platform::Huya, 9_999)[1].stale);
        assert_eq!(log.rows(Platform::Douyu, 1_000)[0].result, "尚无记录");
    }

    #[test]
    fn stage_labels_are_honest_and_distinct() {
        assert_eq!(
            ObservationState::Failed.label(ObservationStage::Danmaku),
            "覆盖层启动失败"
        );
        assert_eq!(
            ObservationState::Ready.label(ObservationStage::Danmaku),
            "曾连接 / 收到弹幕"
        );
        assert_eq!(
            ObservationState::OverlayStarted.label(ObservationStage::Danmaku),
            "弹幕覆盖层已启动（弹幕由覆盖层进程接收）"
        );
        assert_eq!(ObservationStage::Playback.label(), "播放解析");
    }

    #[test]
    fn session_phases_map_to_observation_states_without_claiming_success() {
        assert_eq!(
            playback_state(SessionPhase::Resolving),
            ObservationState::Waiting
        );
        assert_eq!(
            playback_state(SessionPhase::Playing),
            ObservationState::Playing
        );
        assert_eq!(
            playback_state(SessionPhase::Interrupted),
            ObservationState::NetworkError
        );
        assert_eq!(
            playback_state(SessionPhase::Failed),
            ObservationState::Failed
        );
        assert_eq!(
            playback_state(SessionPhase::WaitingBroadcast),
            ObservationState::Retrying
        );
        assert_eq!(
            room_state(SessionRoomState::Replay),
            ObservationState::Replay
        );
    }

    #[test]
    fn the_record_round_trips_and_rejects_corrupt_or_future_files() {
        let directory = tempfile::tempdir().unwrap();
        let mut log = ObservationLog::default();
        log.record(
            Platform::Bili,
            ObservationStage::Room,
            ObservationState::Live,
            5_000,
            "123",
        );
        log.save(directory.path()).unwrap();
        let loaded = ObservationLog::load(directory.path());
        assert_eq!(loaded.rows(Platform::Bili, 5_001)[0].result, "正在直播");
        assert_eq!(loaded.rows(Platform::Bili, 5_001)[0].room, "123");
        std::fs::write(observation_path(directory.path()), "not json").unwrap();
        assert!(ObservationLog::load(directory.path()).is_empty());
        std::fs::write(
            observation_path(directory.path()),
            r#"{"version":99,"platforms":{}}"#,
        )
        .unwrap();
        assert!(ObservationLog::load(directory.path()).is_empty());
        assert!(ObservationLog::load(&directory.path().join("missing")).is_empty());
    }

    #[test]
    fn detail_text_lists_every_stage_and_marks_the_observation_time() {
        let mut log = ObservationLog::default();
        log.record_state(
            Platform::Douyu,
            ObservationStage::Danmaku,
            ObservationState::OverlayStarted,
            1_000,
        );
        let text = log.detail_text(Platform::Douyu, 1_001);
        assert!(text.starts_with("斗鱼"));
        assert!(text.contains("房间状态：尚无记录"));
        assert!(text.contains("弹幕连接：弹幕覆盖层已启动"));
        assert!(text.contains(" · "));
    }
}
