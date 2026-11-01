//! Native launcher UI. Network and persistence work run on worker threads.
//!
//! The launcher only reads files the user explicitly selects. It never scans
//! browser profiles, cookie databases, or the legacy application's directory.

use crate::auth::CookieVault;
use crate::core::{
    BufferMode, EmoteSizeMode, Enhancement, Favorite, HistoryEntry, OverlaySettings, PercentRect,
    Platform, PlaybackPreferences, Player, Quality, ScCorner, ScMode, Settings, Store, Target,
    TextBorderStyle,
};
use crate::{
    login, platforms,
    sessions::{SessionManager, SessionPhase, SessionSnapshot},
};
use anyhow::{Context, Result, anyhow, ensure};
use chrono::TimeZone;
use eframe::egui::{
    self, Align, Color32, FontData, FontDefinitions, FontFamily, FontId, Layout, RichText,
    TextStyle,
};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

const ACCENT: Color32 = Color32::from_rgb(244, 137, 67);
const SIDEBAR: Color32 = Color32::from_rgb(20, 23, 31);
const MAX_IMPORT_BYTES: u64 = 2 * 1024 * 1024;
/// Hard cap for explicitly chosen credential text files, checked before parsing.
const MAX_CREDENTIAL_BYTES: u64 = 2 * 1024 * 1024;
mod accounts;
mod ai_panel;
pub mod badges;
pub mod data_page;
pub mod file_dialog;
pub mod observations;
mod sessions_page;
pub mod theme;
pub mod widgets;
mod ui_shot;
mod window_frame;
mod workbench;
/// How often saved credentials are re-confirmed while the accounts page is
/// open. Network checks never run more often than this.
pub const AUTO_HEALTH_INTERVAL_SECS: u64 = 600;
/// Windows CREATE_NO_WINDOW: the overlay child must never open a console.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Original workbench design size: 1240x720 logical pixels.
pub const DESIGN_SIZE: [f32; 2] = [1240.0, 720.0];
/// Original workbench minimum size: 760x520 logical pixels.
pub const MIN_SIZE: [f32; 2] = [760.0, 520.0];

/// Launcher window title. Also the exact identity the Windows 11 corner install
/// uses to find the launcher's own HWND on its UI thread (see window_frame), so
/// a second launcher instance can never be mistaken for this one.
const WINDOW_TITLE: &str = "看直播 · Rust";

pub fn run(data_root: PathBuf) -> Result<()> {
    let store = Store::open(data_root)?;
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(DESIGN_SIZE)
        .with_min_inner_size(MIN_SIZE)
        // The original C# workbench draws its own 34px chrome and traffic
        // lights, so the native title bar is removed; dragging, maximise and
        // close are handled by the custom shell. Windows 11 still rounds the
        // window corners through DWM (see window_frame::install).
        .with_decorations(false)
        .with_icon(app_icon());
    // Diagnostic-only seam used by the corner screenshot probe: create the real
    // window inactive and at a known position so the probe can photograph its
    // rounded corners without taking the user's foreground. A normal launch
    // never sets LIVE_HUB_WINDOW_PROBE and keeps the centered placement.
    let mut centered = true;
    if let Some(position) =
        window_frame::probe_placement(std::env::var("LIVE_HUB_WINDOW_PROBE").ok().as_deref())
    {
        viewport = viewport.with_position(position).with_active(false);
        centered = false;
    }
    let options = eframe::NativeOptions {
        viewport,
        // The default placement can put the window partly below the work area on
        // high-DPI displays; centering keeps the whole launcher visible on start.
        centered,
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(move |context| Ok(Box::new(Launcher::new(context, store)))),
    )
    .map_err(|error| anyhow!("无法打开应用窗口：{error}"))
}

/// One ui-shot capture request. The window is opened off-screen and inactive;
/// the renderer's own framebuffer is saved per page, so a diagnostic run never
/// steals the user's foreground window.
pub struct UiShotOptions {
    pub dir: PathBuf,
    pub pages: Vec<String>,
    pub width: f32,
    pub height: f32,
    pub settle_frames: u32,
    /// Optional forced theme for the capture: Some(true) = dark, Some(false) =
    /// light, None = the launcher default (dark).
    pub dark: Option<bool>,
}

pub fn run_ui_shot(data_root: PathBuf, options: UiShotOptions) -> Result<()> {
    let store = Store::open(data_root)?;
    let width = options.width.max(320.0);
    let height = options.height.max(240.0);
    let dark = options.dark;
    let driver = ui_shot::Driver::new(ui_shot::ShotOptions {
        dir: options.dir,
        pages: options.pages,
        width,
        height,
        settle_frames: options.settle_frames,
    });
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([width, height])
            .with_min_inner_size(MIN_SIZE)
            .with_decorations(false)
            .with_active(false)
            .with_position([-30000.0, -30000.0])
            .with_taskbar(false)
            .with_icon(app_icon()),
        centered: false,
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "看直播 · Rust (ui-shot)",
        native,
        Box::new(move |context| {
            // from_context (not new) keeps badge networking off: a capture never
            // contacts a platform.
            let mut launcher = Launcher::from_context(&context.egui_ctx, store);
            // Diagnostic seam: pre-select the first favourite so the selected-card
            // state can be captured off-screen. Never used by a normal launch.
            if std::env::var_os("LIVE_HUB_UI_SHOT_SELECT").is_some()
                && let Some(entry) = launcher.snapshot.favorites.first().cloned()
            {
                launcher.favorite_selected = Some(0);
                launcher.platform = entry.target.platform;
                launcher.room_input = entry.target.room.clone();
                launcher.favorite_name = entry.name;
            }
            // Diagnostic seam: inject synthetic live states so a screenshot shows
            // the room-state pill without any network request. The states cycle
            // live/offline/replay and are never used by a normal launch.
            if std::env::var_os("LIVE_HUB_UI_SHOT_FAKE_BADGES").is_some() {
                let states = [
                    crate::room_state::RoomState::Live,
                    crate::room_state::RoomState::Offline,
                    crate::room_state::RoomState::Replay,
                ];
                for (index, entry) in launcher.snapshot.favorites.iter().enumerate() {
                    let key = badges::room_key(&entry.target);
                    launcher
                        .badges
                        .insert(key, badges::Badge::from_state(states[index % states.len()]));
                }
            }
            if let Some(dark) = dark {
                launcher.dark = dark;
                crate::gui::theme::apply(&context.egui_ctx, dark);
            }
            launcher.ui_shot = Some(driver);
            launcher.shot_mode = true;
            // Explicit diagnostic opt-in for real avatar/badge screenshots; a
            // normal capture stays fully offline.
            if std::env::var_os("LIVE_HUB_UI_SHOT_NETWORK").is_some() {
                launcher.badge_networking = true;
            }
            // Diagnostic opt-ins for lookup-page layout captures. They only exist in
            // the off-screen shot mode; a normal launch never reaches this code.
            if std::env::var_os("LIVE_HUB_UI_SHOT_LOOKUP_FAKE").is_some() {
                launcher.page = Page::Lookup;
                launcher.search_input = "花小龙".into();
                launcher.search_results = shot_lookup_candidates();
                launcher.search_caption =
                    "全部平台 · “花小龙” · 6 个候选 · B站 2 / 虎牙 1 / 斗鱼 1 / 抖音 2".into();
            }
            if std::env::var_os("LIVE_HUB_UI_SHOT_LOOKUP_PANEL").is_some() {
                launcher.page = Page::Lookup;
                launcher.lookup_ai_panel = true;
            }
            if std::env::var_os("LIVE_HUB_UI_SHOT_FAVORITE_SELECTED").is_some()
                && !launcher.snapshot.favorites.is_empty()
            {
                launcher.page = Page::Favorites;
                launcher.favorite_selected = Some(0);
            }
            Ok(Box::new(launcher))
        }),
    )
    .map_err(|error| anyhow!("无法打开截图窗口：{error}"))
}

/// Synthetic candidates used only by the off-screen shot mode to capture the
/// lookup candidate-card layout (three lines, avatar placeholder, elision).
fn shot_lookup_candidates() -> Vec<platforms::LookupCandidate> {
    use platforms::LookupCandidate;
    let row = |platform, name: &str, room: &str, followers: u64, description: &str| {
        LookupCandidate {
            name: name.to_owned(),
            room: room.to_owned(),
            platform,
            follower_count: Some(followers),
            description: description.to_owned(),
            account_id: (!room.is_empty()).then(|| room.to_owned()),
            ..LookupCandidate::default()
        }
    };
    vec![
        row(
            Platform::Bili,
            "花小龙",
            "7777",
            12_345_678,
            "B站合成候选：明显更长的简介用于检查候选行第三行与省略号，不应压到下一行或溢出右边缘。",
        ),
        row(
            Platform::Bili,
            "花小龙的剪辑日常（粉丝向合成名）",
            "22839724",
            345_678,
            "B站合成候选：超长账号名与简介同时出现时的省略号行为。",
        ),
        row(Platform::Huya, "花小龙", "196645", 4_567_890, "虎牙合成候选。"),
        row(Platform::Douyu, "花小龙", "93589", 987_654, "斗鱼合成候选。"),
        row(
            Platform::Douyin,
            "花小龙",
            "",
            41_537_602,
            "抖音合成候选：仅有账号线索、房间待解析，第三行应显示身份与核验状态。",
        ),
        row(
            Platform::Douyin,
            "花小龙（合成账号）",
            "",
            12_345,
            "抖音合成候选：账号阶段行。",
        ),
    ]
}

/// The eight workbench pages of the original C# launcher.
///
/// The Rust preview's old "开始看直播" page is folded into the favourites
/// launch card, the old combined settings page is split into the original
/// "播放器与画质" and "弹幕与 SC 设置" pages, and the old "关于" page is folded
/// into the data/status page, so navigation keeps the original structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Favorites,
    Recent,
    Lookup,
    Sessions,
    Players,
    Danmaku,
    Accounts,
    Data,
}

impl Page {
    /// Navigation order of the original sidebar.
    pub const ALL: [Self; 8] = [
        Self::Favorites,
        Self::Recent,
        Self::Lookup,
        Self::Sessions,
        Self::Players,
        Self::Danmaku,
        Self::Accounts,
        Self::Data,
    ];
    /// Index of the first "设置与账号" group entry in Self::ALL.
    const SETTINGS_GROUP: usize = 4;

    /// Stable key used by the ui-shot diagnostic and by tests.
    pub fn key(self) -> &'static str {
        match self {
            Self::Favorites => "favorites",
            Self::Recent => "recent",
            Self::Lookup => "lookup",
            Self::Sessions => "sessions",
            Self::Players => "players",
            Self::Danmaku => "danmaku",
            Self::Accounts => "accounts",
            Self::Data => "data",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        let key = key.trim();
        Self::ALL.into_iter().find(|page| page.key() == key)
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Favorites => "我的收藏",
            Self::Recent => "最近播放",
            Self::Lookup => "查找主播",
            Self::Sessions => "观看会话",
            Self::Players => "播放器与画质",
            Self::Danmaku => "弹幕与 SC 设置",
            Self::Accounts => "账号与登录",
            Self::Data => "本地数据 / 平台状态",
        }
    }

    pub fn subtitle(self) -> &'static str {
        match self {
            Self::Favorites => "喜欢的主播，打开就能看。",
            Self::Recent => "选择熟悉的播放器，从最近的房间继续。",
            Self::Lookup => "查询、核验、收藏，找到喜欢的主播。",
            Self::Sessions => "查看正在观看的房间，按上次选项重新打开。",
            Self::Players => "选择程序路径，管理画质和播放偏好。",
            Self::Danmaku => "显示、过滤、SC 与更多覆盖层设置。",
            Self::Accounts => "四个平台的登录与 Cookie 状态，一处管理。",
            Self::Data => "浏览本地数据，查看解析、取流和弹幕运行状态。",
        }
    }

    /// Idle footer hint shown while no action has produced a status message.
    pub fn idle_hint(self) -> &'static str {
        match self {
            Self::Favorites => "输入房间或选择收藏，回车用 mpv 打开。",
            Self::Recent => "选择一条记录，可用 mpv 或 PotPlayer 打开。",
            Self::Lookup => "输入昵称、房间链接或抖音号，回车查询。",
            Self::Sessions => "选择会话后可按原播放器和参数重新打开。",
            Self::Players => "两种播放器分别指定路径；画质增强仅对 mpv 生效。",
            Self::Danmaku => "编辑后保存，运行中的覆盖层会读取更新。",
            Self::Accounts => "登录与 Cookie 检测按平台独立管理。",
            Self::Data => "只读查看本地数据；清理仅作用于缓存和日志。",
        }
    }
}

/// The transparent overlay now drives the shared danmaku receiver, which dispatches
/// to every platform transport; keep one gate here so a future platform change stays explicit.
pub fn overlay_platform_supported(platform: Platform) -> bool {
    Platform::ALL.contains(&platform)
}

/// Exact child arguments for the overlay subcommand:
/// overlay <platform:room> --player-pid <pid>
///
/// Pure on purpose: the command contract is unit-tested without launching any process.
pub fn overlay_arguments(target: &Target, player_pid: u32) -> Result<Vec<String>> {
    ensure!(player_pid != 0, "弹幕覆盖层需要有效播放器进程号");
    ensure!(
        overlay_platform_supported(target.platform),
        "{} 的弹幕覆盖层尚未接入",
        target.platform.label()
    );
    Ok(vec![
        "overlay".to_owned(),
        format!("{}:{}", target.platform, target.room),
        "--player-pid".to_owned(),
        player_pid.to_string(),
    ])
}

/// An automatically launched overlay must read this launcher's own saved
/// preferences and credential vault, including when a custom data root is used.
pub fn overlay_arguments_with_data_root(
    target: &Target,
    player_pid: u32,
    data_root: &Path,
) -> Result<Vec<OsString>> {
    ensure!(
        !data_root.as_os_str().is_empty(),
        "弹幕覆盖层需要明确的数据目录"
    );
    let mut arguments = vec![
        OsString::from("--data-dir"),
        data_root.as_os_str().to_owned(),
    ];
    arguments.extend(
        overlay_arguments(target, player_pid)?
            .into_iter()
            .map(OsString::from),
    );
    Ok(arguments)
}

/// Replace only the selected account after verification, never an unrelated
/// search result. The caller also rejects responses from an older search.
pub fn apply_verified_candidate(
    candidates: &mut [platforms::LookupCandidate],
    selected: &platforms::LookupCandidate,
    verified: platforms::LookupCandidate,
) -> Result<()> {
    ensure!(
        selected.platform == verified.platform,
        "核验结果平台与所选账号不一致"
    );
    let profile = selected
        .profile_url
        .as_deref()
        .context("所选候选没有可核验的账号主页")?;
    ensure!(
        verified.profile_url.as_deref() == Some(profile),
        "核验结果与所选账号主页不一致"
    );
    Target::new(verified.platform, verified.room.clone()).context("核验尚未得到有效网页房间号")?;
    let index = candidates
        .iter()
        .position(|candidate| {
            candidate.platform == selected.platform
                && candidate.profile_url.as_deref() == Some(profile)
        })
        .context("搜索结果已变化，请重新选择账号")?;
    candidates[index] = verified;
    Ok(())
}

/// Result of the automatic overlay launch that follows a successful playback launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayLaunch {
    /// The user turned the automatic overlay off.
    Disabled,
    /// The overlay child process was spawned; it decides on its own whether to show.
    Launched,
    /// This platform's danmaku receiver is not migrated yet.
    UnsupportedPlatform(Platform),
    /// The transparent overlay needs Windows APIs.
    UnsupportedSystem,
    Failed(String),
}

impl OverlayLaunch {
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

/// One status-bar sentence that reports the player first and the overlay second.
///
/// Playback success is never hidden: even a failed overlay spawn says playback
/// is unaffected.
pub fn overlay_status_text(title: &str, launch: &OverlayLaunch) -> String {
    match launch {
        OverlayLaunch::Disabled => format!("已启动播放器：{title}。"),
        OverlayLaunch::Launched => {
            format!("已启动播放器：{title}。弹幕覆盖层已启动，播放器窗口在前台时会跟随显示。")
        }
        OverlayLaunch::UnsupportedPlatform(platform) => format!(
            "已启动播放器：{title}。{} 的弹幕覆盖层尚未迁移，本次未启动；播放不受影响。",
            platform.label()
        ),
        OverlayLaunch::UnsupportedSystem => format!(
            "已启动播放器：{title}。弹幕覆盖层目前仅支持 Windows，本次未启动；播放不受影响。"
        ),
        OverlayLaunch::Failed(reason) => {
            format!("已启动播放器：{title}。弹幕覆盖层启动失败：{reason}；播放不受影响。")
        }
    }
}

/// Detects the platform of a pasted official room URL or an explicit platform:room
/// input. Bare room numbers return None so the visible platform picker stays
/// authoritative for them.
pub fn detected_platform(input: &str) -> Option<Platform> {
    let input = input.trim();
    if input.contains("://") {
        return Target::parse(input).ok().map(|target| target.platform);
    }
    let (prefix, room) = input.split_once(':')?;
    if room.trim().is_empty() {
        return None;
    }
    prefix.trim().parse::<Platform>().ok()
}

/// Localized, actionable replacement for the domain layer's English room errors.
pub fn room_input_hint(input: &str, platform: Platform, error: &str) -> String {
    let input = input.trim();
    let detail = if input.is_empty() {
        "请输入房间号，或从浏览器粘贴官方直播间链接。".to_owned()
    } else if error.contains("numeric room ID") {
        format!(
            "{} 的房间号必须是纯数字，例如官方链接 https://live.bilibili.com/12345 里的 12345。",
            platform.label()
        )
    } else if error.contains("Room ID may contain only") {
        format!(
            "{} 的房间号只能包含字母、数字、下划线和连字符。",
            platform.label()
        )
    } else if error.contains("official live-room URL") {
        "只支持 http(s) 官方直播间链接，或“平台:房间号”写法。".to_owned()
    } else if error.contains("too long") || error.contains("Too long") || error.contains("too many")
    {
        "输入过长，请只填写房间号或完整直播间链接。".to_owned()
    } else if error.contains("Unsupported platform") {
        "无法识别平台；请先选平台再输入房间号，或粘贴官方直播间链接。".to_owned()
    } else {
        error.to_owned()
    };
    format!("房间输入有误：{detail}")
}

/// Maps a stored session error to platform-specific replay wording so a looping
/// room is never reported with another platform's terminology.
pub fn session_error_text(platform: Platform, error: &str) -> String {
    if crate::platforms::is_replay_error(error) {
        crate::platforms::replay_unavailable_message(platform, "")
    } else {
        error.to_owned()
    }
}

/// Whether a finished login flow is a successful Douyin web assist that should
/// trigger a one-shot search refresh. Pure so the decision is testable offline.
pub fn web_assist_refresh_allowed(assist: bool, error: bool, platform: Platform) -> bool {
    assist && !error && platform == Platform::Douyin
}

/// One extra line for a session that is currently observing a replay.
pub fn replay_wait_hint(platform: Platform) -> String {
    crate::platforms::replay_unavailable_message(platform, "房间处于重播，暂未取得可用媒体")
}

/// Cached credential availability read from the isolated vault only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CredentialState {
    /// Whether encrypted (Windows DPAPI) credential storage is usable here.
    pub supported: bool,
    /// Per-platform "has unexpired imported cookies" flags.
    pub imported: BTreeMap<Platform, bool>,
    /// First vault read problem, if any; never includes cookie material.
    pub error: Option<String>,
}

impl CredentialState {
    pub fn imported_for(&self, platform: Platform) -> bool {
        self.imported.get(&platform).copied().unwrap_or(false)
    }
}

/// Chooses the next saved platform whose login state should be confirmed with the
/// platform's own endpoint. Only imported credentials are checked, one platform at
/// a time, and a platform stays quiet for `interval_secs` after an attempt even
/// when that attempt failed.
pub fn next_health_check(
    credentials: &CredentialState,
    attempts: &BTreeMap<Platform, u64>,
    now: u64,
    interval_secs: u64,
) -> Option<Platform> {
    Platform::ALL.into_iter().find(|platform| {
        credentials.imported_for(*platform)
            && attempts.get(platform).is_none_or(|checked| {
                now < *checked || now.saturating_sub(*checked) >= interval_secs
            })
    })
}

/// Human-readable age of the last completed login health check.
pub fn health_age_text(checked_at: u64, now: u64) -> String {
    if checked_at > now {
        return "检查时间未知".to_owned();
    }
    let elapsed = now - checked_at;
    match elapsed {
        0..=59 => "刚刚检查".to_owned(),
        60..=3_599 => format!("{} 分钟前检查", elapsed / 60),
        3_600..=86_399 => format!("{} 小时前检查", elapsed / 3_600),
        _ => format!("{} 天前检查", elapsed / 86_400),
    }
}

/// Encrypted-credential fingerprint used only to discard outdated health results.
pub fn credential_stamp(data_root: &Path, platform: Platform) -> Option<String> {
    let vault_root = data_root.join("credentials");
    if !vault_root.is_dir() {
        return None;
    }
    CookieVault::open(vault_root)
        .ok()?
        .credential_stamp(platform)
        .ok()
        .flatten()
}

/// Clears attempt timestamps that are old enough for an explicit re-check, so the
/// automatic loop may run them now; recent attempts stay to enforce the 10 second
/// manual floor.
pub fn clear_due_health_attempts(
    attempts: &mut BTreeMap<Platform, u64>,
    credentials: &CredentialState,
    now: u64,
) -> usize {
    let mut cleared = 0;
    for platform in Platform::ALL {
        if !credentials.imported_for(platform) {
            // Attempts for a platform without saved credentials are stale state.
            attempts.remove(&platform);
            continue;
        }
        if login::check_allowed(
            attempts.get(&platform).copied(),
            now,
            login::MANUAL_CHECK_MIN_SECS,
        ) && attempts.remove(&platform).is_some()
        {
            cleared += 1;
        }
    }
    cleared
}

fn health_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// One line of the per-platform credential status list.
pub fn credential_platform_label(platform: Platform, state: &CredentialState) -> String {
    if state.imported_for(platform) {
        format!("{}：已导入（DPAPI 加密保存）", platform.label())
    } else if state.supported {
        format!("{}：未导入", platform.label())
    } else {
        format!(
            "{}：当前系统不支持加密存储（需要 Windows DPAPI）",
            platform.label()
        )
    }
}

/// Reads one explicitly selected credential text file.
///
/// Limit and encoding are checked before the vault parser sees the text, so a
/// huge or binary file cannot stall or reach the DPAPI path.
pub fn read_credential_text(path: &Path) -> Result<String> {
    let file = File::open(path).context("无法打开所选凭据文件")?;
    let metadata = file.metadata().context("无法读取所选凭据文件信息")?;
    ensure!(metadata.is_file(), "请选择普通凭据文本文件");
    ensure!(
        metadata.len() <= MAX_CREDENTIAL_BYTES,
        "凭据文件超过 2 MiB 限额"
    );
    let mut bytes = Vec::new();
    file.take(MAX_CREDENTIAL_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("读取凭据文件失败")?;
    ensure!(
        bytes.len() as u64 <= MAX_CREDENTIAL_BYTES,
        "凭据文件超过 2 MiB 限额"
    );
    let text = String::from_utf8(bytes).context("凭据文件必须使用 UTF-8 编码")?;
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned())
}

/// Feature summary shared by Settings and About so both pages stay consistent.
pub fn implemented_summary() -> &'static [&'static str] {
    &[
        "平台取流：B站、虎牙、斗鱼、抖音解析器与平台公开搜索；四个平台均完成匿名真实链路验证",
        "播放：mpv / PotPlayer 实际视频与弹幕已通过；FLV 默认经中转恢复，观看会话支持并行启动、取消、停止与故障状态",
        "弹幕覆盖层：四平台接收、SC、窗口跟随；mpv 实际像素、窗口/全屏鼠标穿透及退出回收已验收",
        "数据：独立数据目录、收藏、最近播放、显式 JSON 导入、本地数据只读清单与运行缓存/日志清理",
        "登录凭据：B站二维码、四平台专属浏览器与官方健康检测（启动/新凭据自动复核，有效 10 分钟后转待复查），DPAPI加密；B站扫码/官方有效/登录弹幕已实测",
        "富媒体与搜索：图片/WebP、礼物/进场、过滤合并延迟、跨平台搜索、补充线索排序与按平台近期缓存、可选DeepSeek原生联网研究",
        "稳定性：默认连续失败预算500跨503次有效重连回归、一小时8路合成测试通过",
        "启动器：Enter 播放、粘贴官方链接自动识别平台、设置与关于页状态信息；真实窗口启动与图标已实机验收（单显示器、单倍率）",
    ]
}

/// Known gaps, stated plainly so no page claims more than the code does.
pub fn pending_summary() -> &'static [&'static str] {
    &[
        "弹幕：原版全部表情映射、徽章/头像、复杂SC样式及虎牙/抖音全部富事件未迁移",
        "登录：虎牙、斗鱼、抖音专属浏览器登录需真实平台验收；账号页自动复核真实账号触发未单独观察",
        "搜索：AI请求与来源校验已有离线回归，真实付费接口尚未验收",
        "播放：全部重播资源与交互设置尚未对齐；PotPlayer全屏/SC穿透、多显示器与真实下播重开全周期待验收",
        "稳定性：一小时合成通过；24小时真实直播、网络切换和休眠恢复尚未验收",
        "界面：本地数据/平台状态页与登录时效语义有离线/headless 回归；真实桌面交互与真实账号健康转态未单独验收",
    ]
}

#[derive(Clone)]
struct Snapshot {
    root: PathBuf,
    settings: Settings,
    favorites: Vec<Favorite>,
    history: Vec<HistoryEntry>,
    credentials: CredentialState,
    ai_config: Option<crate::ai::AiConfig>,
    ai_error: Option<String>,
}

impl Snapshot {
    fn from_store(store: &Store) -> Self {
        let ai = crate::ai::AiVault::new(store.root().join("ai")).configuration();
        let (ai_config, ai_error) = match ai {
            Ok(config) => (config, None),
            Err(error) => (None, Some(error.to_string())),
        };
        Self {
            root: store.root().to_owned(),
            settings: store.settings().clone(),
            favorites: store.favorites().to_vec(),
            history: store.history().to_vec(),
            credentials: credential_state(store.root()),
            ai_config,
            ai_error,
        }
    }
}

/// Launch-time configuration kept per session so "重新打开" reproduces the
/// original player and buffer preference even after the global settings change.
#[derive(Clone)]
struct SessionLaunch {
    player: Player,
    settings: Settings,
}

enum StoreAction {
    AddFavorite(String, Target),
    RemoveFavorite(Platform, String),
    RecordPlay(String, Target),
    SaveSettings(Settings),
    SaveOverlay(OverlaySettings),
    SavePlayback(PlaybackPreferences),
    SaveAi(crate::ai::AiConfig, String),
    ClearAi,
    Import(PathBuf, ImportKind),
    CredentialImport(Platform, PathBuf),
    CredentialRemove(Platform),
    /// Remove this platform's dedicated browser-assist profile directories.
    ClearBrowserProfiles(Platform),
    Refresh,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ImportKind {
    FavoritesOrExport,
    LegacyHistory,
}

enum Event {
    Ai {
        id: u64,
        optimize: bool,
        result: std::result::Result<crate::ai::ResearchResult, String>,
    },
    AiVerified {
        id: u64,
        result: std::result::Result<platforms::LookupCandidate, String>,
    },
    Storage {
        snapshot: Snapshot,
        message: std::result::Result<String, String>,
        quiet: bool,
    },
    Search {
        id: u64,
        platform: Platform,
        query: crate::search::SearchQuery,
        key: String,
        result: std::result::Result<Vec<platforms::LookupCandidate>, String>,
    },
    SearchAll {
        id: u64,
        query: crate::search::SearchQuery,
        /// Per-platform responses for the platforms that were not served from cache.
        responses: Vec<(
            Platform,
            std::result::Result<Vec<platforms::LookupCandidate>, String>,
        )>,
    },
    Verification {
        id: u64,
        selected: platforms::LookupCandidate,
        result: std::result::Result<platforms::LookupCandidate, String>,
    },
    Login {
        id: u64,
        platform: Platform,
        update: LoginUpdate,
    },
    /// Result of one room-live badge query.
    Badge {
        key: String,
        result: std::result::Result<crate::room_state::RoomState, String>,
    },
    /// A bounded 48px avatar decoded on the download worker, ready for upload.
    LookupAvatar {
        url: String,
        result: std::result::Result<image::RgbaImage, String>,
    },
    /// Result of the automatic server-backed login check for saved credentials.
    Health {
        generation: u64,
        platform: Platform,
        /// Encrypted-credential fingerprint captured before the request started.
        stamp: Option<String>,
        result: std::result::Result<login::LoginHealth, String>,
    },
}

enum LoginUpdate {
    Qr(String),
    Progress(String),
    Health(login::LoginHealth),
    Done { message: String, error: bool },
}

struct Launcher {
    page: Page,
    /// Selected card in the favourites grid.
    favorite_selected: Option<usize>,
    /// Selected row in the recent-play list.
    recent_selected: Option<usize>,
    /// Selected candidate in the lookup result list.
    lookup_selected: Option<usize>,
    /// Launch-card danmaku override: 0 follows the saved setting, 1 forces on,
    /// 2 forces off for this playback only.
    launch_danmaku: u8,
    /// Selected category tab on the "弹幕与 SC 设置" page.
    danmaku_tab: usize,
    /// Which player the "播放器与画质" page is configuring.
    player_edit: Player,
    /// Selected row on the "观看会话" page (session id).
    session_selected: Option<String>,
    /// "查找主播" page: show the extra clue fields (game / identity / alias).
    lookup_clues: bool,
    /// "查找主播" page: show the DeepSeek configuration panel inline.
    lookup_ai_panel: bool,
    /// Selected AI research source on the "查找主播" page.
    lookup_source: Option<usize>,
    /// Whether the players page's Rust-only advanced cards are expanded.
    players_advanced_open: bool,
    /// Result platform filter on the "查找主播" page (None = all platforms).
    lookup_filter: Option<Platform>,
    /// "状态详情" result shown under the account cards (explicit user request).
    account_details: Option<String>,
    /// Frame-driven ui-shot capture driver; None on a normal launch.
    ui_shot: Option<ui_shot::Driver>,
    /// True only for a ui-shot run; render-time diagnostics read this instead of
    /// ui_shot (the driver is temporarily moved out of self during update).
    shot_mode: bool,
    snapshot: Snapshot,
    platform: Platform,
    room_input: String,
    favorite_name: String,
    quality: Quality,
    player: Player,
    dark: bool,
    status: String,
    status_error: bool,
    playing: bool,
    overlay_draft: OverlaySettings,
    storage_pending: usize,
    events: Receiver<Event>,
    event_sender: Sender<Event>,
    store_sender: Sender<StoreAction>,
    search_input: String,
    search_generation: u64,
    searching: bool,
    search_all: bool,
    search_caption: String,
    search_results: Vec<platforms::LookupCandidate>,
    verification_pending: bool,
    mpv_path: String,
    potplayer_path: String,
    ytdlp_path: String,
    import_path: String,
    import_kind: ImportKind,
    credential_path: String,
    sessions: SessionManager,
    session_events: Receiver<SessionSnapshot>,
    handled_sessions: BTreeSet<String>,
    session_overlay: BTreeMap<String, bool>,
    session_launch: BTreeMap<String, SessionLaunch>,
    playback_draft: PlaybackPreferences,
    mpv_args_text: String,
    potplayer_args_text: String,
    login_generation: u64,
    login_cancel: Option<login::Cancellation>,
    login_capture: Arc<AtomicBool>,
    login_busy: bool,
    login_message: String,
    login_qr: Option<egui::TextureHandle>,
    login_image: Option<egui::ColorImage>,
    login_health: BTreeMap<Platform, login::LoginHealth>,
    health_attempts: BTreeMap<Platform, u64>,
    health_generation: u64,
    health_busy: bool,
    health_cancel: Option<login::Cancellation>,
    browser_path: String,
    image_loader: crate::images::ImageLoader,
    avatar_textures: BTreeMap<String, egui::TextureHandle>,
    /// Lookup candidate avatars keyed by their normalized avatar URL, plus the
    /// URLs already queued (one fetch per URL) and bounded decoded thumbnails
    /// waiting for the next frame to turn them into a texture.
    lookup_avatar_textures: BTreeMap<String, egui::TextureHandle>,
    lookup_avatar_pending: std::collections::BTreeSet<String>,
    lookup_avatar_images: BTreeMap<String, image::RgbaImage>,
    lookup_avatar_retry_at: BTreeMap<String, std::time::Instant>,
    /// Platform avatars for favourites/recent/session cards: disk cache first,
    /// bounded background fetches on a real launch (never during a ui-shot run).
    avatar_service: crate::avatars::AvatarService,
    blocked_keyword_text: String,
    blocked_user_text: String,
    reserved_areas_text: String,
    ai_model: String,
    ai_max_tokens: u16,
    ai_key_input: String,
    ai_busy: bool,
    ai_cancel: Option<login::Cancellation>,
    ai_result: Option<crate::ai::ResearchResult>,
    observations: observations::ObservationLog,
    diagnostics: data_page::DiagnosticLog,
    data_tab: data_page::DataTab,
    data_category: data_page::DataCategory,
    data_detail: bool,
    data_snapshot: Option<data_page::DataSnapshot>,
    data_log_index: Option<usize>,
    cleanup_confirm: bool,
    logout_confirm: Option<Platform>,
    search_cache: crate::search::SearchCache,
    search_outcomes: Vec<crate::search::PlatformOutcome>,
    search_cached_at: Option<u64>,
    search_pending_cache: Option<(
        u64,
        BTreeMap<Platform, (Vec<platforms::LookupCandidate>, u64)>,
    )>,
    search_game: String,
    search_identity: String,
    search_alias: String,
    health_all_requested: bool,
    badges: BTreeMap<String, badges::Badge>,
    badge_times: BTreeMap<String, u64>,
    badge_pending: BTreeSet<String>,
    /// Test seam: headless constructors keep this off so no platform request is
    /// ever made by a test; the native launcher turns it on.
    badge_networking: bool,
    /// A Douyin page-assist browser session started from the search page.
    web_assist_pending: bool,
    /// Set after a successful Douyin web assist; consumed by the search page to
    /// re-run the current keyword once credentials were saved.
    search_refresh_after_login: bool,
    /// Two-step confirmation for clearing the Douyin assist profile and login.
    douyin_profile_confirm: bool,
}

impl Launcher {
    fn new(context: &eframe::CreationContext<'_>, store: Store) -> Self {
        // Windows 11 rounds the borderless, self-drawn frame through DWM only
        // after it is opted in; do it before the first frame so the window never
        // flashes square corners. No-op on Windows 10 and other platforms.
        window_frame::install(WINDOW_TITLE);
        let mut launcher = Self::from_context(&context.egui_ctx, store);
        // Only the real window may query platforms for live badges; tests use the
        // headless constructor and stay fully offline.
        launcher.badge_networking = true;
        launcher
    }

    fn from_context(context: &egui::Context, store: Store) -> Self {
        // The shared design system installs the runtime system fonts
        // (Microsoft YaHei UI / Segoe UI) and applies the theme; a missing font
        // falls back without panicking.
        let snapshot = Snapshot::from_store(&store);
        // The saved theme preference decides the startup palette (default dark).
        let dark = snapshot.settings.dark_mode;
        // The saved UI font preference decides which installed family the whole
        // workbench uses (and the UI font scale percentage).
        let font_options = crate::gui::theme::FontOptions::from_settings(
            &snapshot.settings.ui_font_key,
            snapshot.settings.ui_font_percent.into(),
        );
        let _font_report =
            crate::gui::theme::apply_fonts_themed(context, font_options, dark);
        let (event_sender, events) = mpsc::channel();
        let (store_sender, store_receiver) = mpsc::channel();
        let sessions = SessionManager::new();
        let session_events = sessions.subscribe();
        let observations = observations::ObservationLog::load(&snapshot.root);
        let avatar_service = crate::avatars::AvatarService::new(snapshot.root.clone());
        let diagnostics = data_page::DiagnosticLog::new(&snapshot.root);
        spawn_store_worker(store, store_receiver, event_sender.clone(), context.clone());
        let mut launcher = Self {
            page: Page::Favorites,
            favorite_selected: None,
            recent_selected: None,
            lookup_selected: None,
            launch_danmaku: 0,
            danmaku_tab: 0,
            player_edit: Player::Mpv,
            session_selected: None,
            lookup_clues: false,
            lookup_ai_panel: false,
            lookup_source: None,
            players_advanced_open: false,
            lookup_filter: None,
            account_details: None,
            ui_shot: None,
            shot_mode: false,
            platform: Platform::Bili,
            room_input: String::new(),
            favorite_name: String::new(),
            quality: Quality::Best,
            player: Player::Mpv,
            dark,
            status: "准备就绪。选择平台并输入房间号，或粘贴官方直播间链接。".into(),
            status_error: false,
            playing: false,
            overlay_draft: snapshot.settings.overlay.clone(),
            ai_model: snapshot.ai_config.clone().unwrap_or_default().model,
            ai_max_tokens: snapshot.ai_config.clone().unwrap_or_default().max_tokens,
            ai_key_input: String::new(),
            ai_busy: false,
            ai_cancel: None,
            ai_result: None,
            blocked_keyword_text: snapshot
                .settings
                .overlay
                .behavior
                .blocked_keywords
                .join("\n"),
            blocked_user_text: snapshot.settings.overlay.behavior.blocked_users.join("\n"),
            reserved_areas_text: reserved_areas_text(&snapshot.settings.overlay.reserved_areas),
            playback_draft: snapshot.settings.playback.clone(),
            mpv_args_text: snapshot.settings.playback.mpv_extra_args.join("\n"),
            potplayer_args_text: snapshot.settings.playback.potplayer_extra_args.join("\n"),
            sessions,
            session_events,
            handled_sessions: BTreeSet::new(),
            session_overlay: BTreeMap::new(),
            session_launch: BTreeMap::new(),
            login_generation: 0,
            login_cancel: None,
            login_capture: Arc::new(AtomicBool::new(false)),
            login_busy: false,
            login_message: "选择平台后扫码或在独立浏览器窗口中完成登录（Edge 隐私窗口/Chrome 临时资料，不会登录或同步你的 Microsoft/Google 账号）。".into(),
            login_qr: None,
            login_image: None,
            login_health: BTreeMap::new(),
            health_attempts: BTreeMap::new(),
            health_generation: 0,
            health_busy: false,
            health_cancel: None,
            browser_path: login::browser::find_browser()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            storage_pending: 0,
            mpv_path: path_text(&snapshot.settings.mpv),
            potplayer_path: path_text(&snapshot.settings.potplayer),
            ytdlp_path: path_text(&snapshot.settings.yt_dlp),
            observations,
            diagnostics,
            data_tab: data_page::DataTab::default(),
            data_category: data_page::DataCategory::default(),
            data_detail: false,
            data_snapshot: None,
            data_log_index: None,
            cleanup_confirm: false,
            logout_confirm: None,
            search_cache: crate::search::SearchCache::default(),
            search_outcomes: Vec::new(),
            search_cached_at: None,
            search_pending_cache: None,
            search_game: String::new(),
            search_identity: String::new(),
            search_alias: String::new(),
            health_all_requested: false,
            badges: BTreeMap::new(),
            badge_times: BTreeMap::new(),
            badge_pending: BTreeSet::new(),
            badge_networking: false,
            web_assist_pending: false,
            search_refresh_after_login: false,
            douyin_profile_confirm: false,
            snapshot,
            events,
            event_sender,
            store_sender,
            search_input: String::new(),
            search_generation: 0,
            searching: false,
            search_all: true,
            image_loader: crate::images::ImageLoader::new(),
            avatar_textures: BTreeMap::new(),
            lookup_avatar_textures: BTreeMap::new(),
            lookup_avatar_pending: std::collections::BTreeSet::new(),
            lookup_avatar_images: BTreeMap::new(),
            lookup_avatar_retry_at: BTreeMap::new(),
            avatar_service,
            search_caption: "输入昵称后搜索所选平台。".into(),
            search_results: Vec::new(),
            verification_pending: false,
            import_path: String::new(),
            import_kind: ImportKind::FavoritesOrExport,
            credential_path: String::new(),
        };
        // Diagnostic-only: let a ui-shot run render one specific danmaku tab.
        if let Ok(tab) = std::env::var("LIVE_HUB_SHOT_DANMAKU_TAB")
            && let Ok(index) = tab.trim().parse::<usize>()
        {
            launcher.danmaku_tab = index.min(4);
        }
        launcher
    }

    /// Save any pending settings, then relaunch this executable with the same
    /// arguments. Returns a Chinese reason when the new process cannot start.
    ///
    /// The caller (the danmaku interface tab) shows a "正在重启" line before this
    /// exits the current process.
    pub(crate) fn restart_launcher(&mut self) -> std::result::Result<(), String> {
        if self.storage_pending > 0 {
            return Err("还有未保存的设置，请稍后重试。".to_owned());
        }
        let executable =
            std::env::current_exe().map_err(|error| format!("找不到当前程序：{error}"))?;
        let mut command = std::process::Command::new(&executable);
        command.args(std::env::args_os().skip(1));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        command
            .spawn()
            .map_err(|error| format!("无法重新启动程序：{error}"))?;
        std::process::exit(0);
    }

    fn set_status(&mut self, text: impl Into<String>, error: bool) {
        self.status = text.into();
        self.status_error = error;
        if error {
            // Bounded, filtered local diagnostics; never blocks a UI action.
            self.diagnostics.record(&self.status);
        }
    }

    fn poll_events(&mut self) {
        let updates: Vec<_> = self.session_events.try_iter().collect();
        for snapshot in updates {
            self.record_observations_from_snapshot(&snapshot);
            if snapshot.phase == SessionPhase::Playing
                && self.handled_sessions.insert(snapshot.id.clone())
            {
                let enabled = self
                    .session_overlay
                    .get(&snapshot.id)
                    .copied()
                    .unwrap_or(self.snapshot.settings.overlay.enabled);
                let overlay = if enabled {
                    snapshot
                        .player_pid
                        .map(|pid| launch_overlay(&snapshot.target, pid, &self.snapshot.root))
                        .unwrap_or_else(|| OverlayLaunch::Failed("播放器进程号不可用".into()))
                } else {
                    OverlayLaunch::Disabled
                };
                self.send_store(StoreAction::RecordPlay(
                    snapshot.title.clone(),
                    snapshot.target.clone(),
                ));
                self.set_status(
                    overlay_status_text(&snapshot.title, &overlay),
                    overlay.is_failure(),
                );
                let danmaku = match &overlay {
                    OverlayLaunch::Launched => observations::ObservationState::OverlayStarted,
                    OverlayLaunch::Disabled => observations::ObservationState::OverlayDisabled,
                    OverlayLaunch::UnsupportedPlatform(_) | OverlayLaunch::UnsupportedSystem => {
                        observations::ObservationState::OverlayUnsupported
                    }
                    OverlayLaunch::Failed(_) => observations::ObservationState::Failed,
                };
                self.record_observation(
                    snapshot.target.platform,
                    observations::ObservationStage::Danmaku,
                    danmaku,
                    health_now(),
                    &snapshot.target.room,
                );
            } else if snapshot.phase == SessionPhase::Failed
                || snapshot.phase == SessionPhase::Interrupted
            {
                let error = snapshot.error.as_deref().unwrap_or(snapshot.phase.label());
                self.set_status(
                    format!(
                        "{}：{}",
                        snapshot.title,
                        session_error_text(snapshot.target.platform, error)
                    ),
                    true,
                );
            }
        }
        let current = self.sessions.snapshots();
        self.playing = current.iter().any(|session| session.phase.can_cancel());
        let ids: BTreeSet<_> = current.iter().map(|s| s.id.clone()).collect();
        self.handled_sessions.retain(|id| ids.contains(id));
        self.session_overlay.retain(|id, _| ids.contains(id));
        self.session_launch.retain(|id, _| ids.contains(id));
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::Ai {
                    id,
                    optimize,
                    result,
                } => {
                    if id != self.search_generation {
                        continue;
                    }
                    self.ai_busy = false;
                    self.ai_cancel = None;
                    match result {
                        Ok(result) => {
                            if optimize {
                                self.search_results = result.candidates.clone();
                            }
                            self.set_status(
                                format!(
                                    "AI研究完成：{}条可追溯来源。新候选须先通过平台核验。",
                                    result.sources.len()
                                ),
                                false,
                            );
                            self.ai_result = Some(result);
                        }
                        Err(error) => self
                            .set_status(format!("AI请求未完成：{error}。原平台结果已保留。"), true),
                    }
                }
                Event::AiVerified { id, result } => {
                    if id != self.search_generation {
                        continue;
                    }
                    self.verification_pending = false;
                    match result {
                        Ok(candidate) => {
                            match Target::new(candidate.platform, candidate.room.clone()) {
                                Ok(target) => {
                                    self.fill_room(&candidate.name, &target);
                                    self.set_status(
                                        "平台核验完成，已填入直播间；不会自动收藏或播放。",
                                        false,
                                    );
                                }
                                Err(error) => self.set_status(error.to_string(), true),
                            }
                        }
                        Err(error) => self.set_status(format!("AI线索尚未核验：{error}"), true),
                    }
                }
                Event::Storage {
                    snapshot,
                    message,
                    quiet,
                } => {
                    self.storage_pending = self.storage_pending.saturating_sub(1);
                    if self.overlay_draft == self.snapshot.settings.overlay
                        && self.blocked_keyword_text
                            == self
                                .snapshot
                                .settings
                                .overlay
                                .behavior
                                .blocked_keywords
                                .join("\n")
                        && self.blocked_user_text
                            == self
                                .snapshot
                                .settings
                                .overlay
                                .behavior
                                .blocked_users
                                .join("\n")
                        && self.reserved_areas_text
                            == reserved_areas_text(&self.snapshot.settings.overlay.reserved_areas)
                    {
                        self.overlay_draft = snapshot.settings.overlay.clone();
                        self.reserved_areas_text =
                            reserved_areas_text(&snapshot.settings.overlay.reserved_areas);
                    }
                    if self.playback_draft == self.snapshot.settings.playback
                        && self.mpv_args_text
                            == self.snapshot.settings.playback.mpv_extra_args.join("\n")
                        && self.potplayer_args_text
                            == self
                                .snapshot
                                .settings
                                .playback
                                .potplayer_extra_args
                                .join("\n")
                    {
                        self.playback_draft = snapshot.settings.playback.clone();
                        self.mpv_args_text = snapshot.settings.playback.mpv_extra_args.join("\n");
                        self.potplayer_args_text =
                            snapshot.settings.playback.potplayer_extra_args.join("\n");
                    }
                    self.snapshot = snapshot;
                    match message {
                        Ok(message) if !quiet => self.set_status(message, false),
                        Err(error) => self.set_status(error, true),
                        _ => (),
                    }
                }
                Event::Search {
                    id,
                    platform,
                    query,
                    key,
                    result,
                } => {
                    if id != self.search_generation {
                        continue;
                    }
                    self.searching = false;
                    self.verification_pending = false;
                    match result {
                        Ok(mut candidates) => {
                            let now = health_now();
                            // Single-platform responses arrive in the platform's own
                            // order; apply the shared relevance ranking (exact name /
                            // alias / supported clues first) before caching.
                            crate::search::rank_candidates(&mut candidates, &query, false);
                            // Only successful, non-empty platform lists are cached:
                            // a transient empty response must not make a later query
                            // reuse "0 results" from memory (Douyin can answer an
                            // empty user_list while the page is still rate limiting).
                            if !candidates.is_empty() {
                                self.search_cache.insert(key, candidates.clone(), now);
                            }
                            self.search_cached_at = Some(now);
                            self.search_outcomes = vec![crate::search::PlatformOutcome {
                                platform,
                                success: true,
                                candidates: candidates.len(),
                                note: "平台原生结果".into(),
                                cached: false,
                                searched_at: now,
                            }];
                            self.search_caption = search_caption(
                                platform.label(),
                                &query,
                                candidates.len(),
                                Some(now),
                            );
                            self.search_results = candidates;
                            self.set_status(
                                if self.search_results.is_empty() {
                                    "本次搜索没有返回主播。可尝试完整昵称、补充线索或直接输入房间号。"
                                } else {
                                    "搜索完成。核对主播和房间后，可填入工作台或直接播放。"
                                },
                                false,
                            );
                        }
                        Err(error) => {
                            self.search_results.clear();
                            self.search_outcomes.clear();
                            self.search_cached_at = None;
                            self.search_caption =
                                format!("{} · “{}” · 查询失败", platform.label(), query.name);
                            let hint = crate::search::recovery_hint(&error);
                            let assist = crate::search::douyin_web_assist_hint(platform, &error)
                                .unwrap_or("");
                            self.set_status(
                                format!("{error} {hint} {assist}").trim_end().to_owned(),
                                true,
                            );
                        }
                    }
                }
                Event::SearchAll {
                    id,
                    query,
                    responses,
                } => {
                    if id != self.search_generation {
                        continue;
                    }
                    self.searching = false;
                    self.verification_pending = false;
                    let pending = self
                        .search_pending_cache
                        .take()
                        .filter(|(pending_id, _)| *pending_id == id)
                        .map(|(_, cached)| cached)
                        .unwrap_or_default();
                    let now = health_now();
                    let mut fresh = responses;
                    let mut merged = Vec::new();
                    let mut cached_times: BTreeMap<Platform, u64> = BTreeMap::new();
                    for platform in Platform::ALL {
                        if let Some((candidates, at)) = pending.get(&platform) {
                            cached_times.insert(platform, *at);
                            merged.push((platform, Ok(candidates.clone())));
                            continue;
                        }
                        if let Some(index) = fresh.iter().position(|(value, _)| *value == platform)
                        {
                            merged.push(fresh.remove(index));
                        }
                    }
                    for (platform, result) in &merged {
                        if cached_times.contains_key(platform) {
                            continue;
                        }
                        if let Ok(candidates) = result {
                            if !candidates.is_empty() {
                                self.search_cache.insert(
                                    query.cache_key(&platform.to_string()),
                                    candidates.clone(),
                                    now,
                                );
                            }
                        }
                    }
                    let mut bundle = crate::search::combine(&query, merged, true);
                    for outcome in &mut bundle.outcomes {
                        if let Some(at) = cached_times.get(&outcome.platform) {
                            if outcome.success {
                                outcome.cached = true;
                                outcome.searched_at = *at;
                                outcome.note = "显示近期缓存；点击刷新结果可重新检索。".to_owned();
                            }
                        } else {
                            outcome.searched_at = now;
                        }
                    }
                    let blocked = bundle
                        .outcomes
                        .iter()
                        .filter(|outcome| !outcome.success)
                        .count();
                    let any_cached = bundle.outcomes.iter().any(|outcome| outcome.cached);
                    let total = bundle.candidates.len();
                    let results = bundle.candidates;
                    let outcomes = bundle.outcomes;
                    self.search_cached_at = if any_cached {
                        outcomes
                            .iter()
                            .filter(|outcome| outcome.cached)
                            .map(|outcome| outcome.searched_at)
                            .min()
                    } else {
                        Some(now)
                    };
                    self.search_results = results;
                    self.search_outcomes = outcomes;
                    self.search_caption = search_all_caption(&query, &self.search_outcomes, total);
                    let empty = self.search_results.is_empty();
                    let douyin_blocked = self
                        .search_outcomes
                        .iter()
                        .any(|outcome| !outcome.success && outcome.platform == Platform::Douyin);
                    let douyin_empty = self.search_outcomes.iter().any(|outcome| {
                        outcome.platform == Platform::Douyin
                            && outcome.success
                            && outcome.candidates == 0
                    });
                    self.set_status(
                        if douyin_blocked {
                            "抖音查询受阻（可能需要登录或网页验证），其他平台结果已保留；可点击“抖音网页辅助”在本程序独立浏览器完成验证后重试。"
                        } else if douyin_empty {
                            "抖音本次返回空结果（可能限流或登录失效）；其他平台结果已保留，可点“刷新结果”重试或使用“抖音网页辅助”。"
                        } else if blocked > 0 {
                            "部分平台查询受阻，其他平台结果已保留；受阻平台可单独重试。"
                        } else if empty && any_cached {
                            "已读取近期缓存，但没有候选；可刷新结果重新检索。"
                        } else if empty {
                            "本次没有返回可用候选；可修改线索或刷新结果重试。"
                        } else {
                            "跨平台查询完成，请核对同名账号与房间。"
                        },
                        false,
                    );
                }
                Event::Verification {
                    id,
                    selected,
                    result,
                } => {
                    if id != self.search_generation {
                        continue;
                    }
                    self.verification_pending = false;
                    match result.and_then(|verified| {
                        let room = verified.room.clone();
                        let platform = verified.platform;
                        let avatar_url = verified.avatar_url.clone();
                        apply_verified_candidate(&mut self.search_results, &selected, verified)
                            .map(|()| (room, platform, avatar_url))
                            .map_err(|error| format!("{error:#}"))
                    }) {
                        Ok((room, platform, avatar_url)) => {
                            // Verification enriched the candidate with its real room
                            // and avatar URL; keep the latter in the avatar index so
                            // the favourites card can use it after a restart.
                            if let Some(url) = avatar_url.as_deref() {
                                let _ = self.avatar_service.remember(platform, &room, url);
                            }
                            self.set_status(
                                format!("账号与直播间核验完成：房间 {room}。核对后可填入或播放。"),
                                false,
                            );
                        }
                        Err(error) => {
                            self.set_status(format!("核验未完成，已保留所选账号：{error}"), true)
                        }
                    }
                }
                Event::Login {
                    id,
                    platform,
                    update,
                } => {
                    if id != self.login_generation {
                        continue;
                    }
                    match update {
                        LoginUpdate::Qr(payload) => match accounts::qr_image(&payload) {
                            Ok(image) => self.login_image = Some(image),
                            Err(error) => {
                                self.login_message = format!("二维码显示失败：{error}");
                                if let Some(cancel) = &self.login_cancel {
                                    cancel.cancel()
                                }
                            }
                        },
                        LoginUpdate::Progress(message) => self.login_message = message,
                        LoginUpdate::Health(health) => {
                            self.login_message =
                                format!("{}：{}", platform.label(), health.label());
                            self.health_attempts.insert(platform, health.checked_at);
                            self.login_health.insert(platform, health);
                            self.send_store(StoreAction::Refresh);
                        }
                        LoginUpdate::Done { message, error } => {
                            self.login_busy = false;
                            self.login_qr = None;
                            self.login_image = None;
                            self.login_message = message.clone();
                            self.set_status(message, error);
                            self.send_store(StoreAction::Refresh);
                            let assist = self.web_assist_pending;
                            self.web_assist_pending = false;
                            if web_assist_refresh_allowed(assist, error, platform) {
                                // Captured Douyin cookies change which queries can
                                // succeed; old cached candidates must not outlive the
                                // new session. The search page consumes this flag and
                                // re-runs the current keyword.
                                self.search_cache.clear();
                                self.search_refresh_after_login = true;
                                self.login_message =
                                    "抖音网页辅助完成，正在用当前关键词重新查询…".into();
                            }
                        }
                    }
                }
                Event::Badge { key, result } => {
                    self.apply_badge_result(key, result);
                }
                Event::LookupAvatar { url, result } => {
                    self.apply_lookup_avatar(url, result);
                }
                Event::Health {
                    generation,
                    platform,
                    stamp,
                    result,
                } => {
                    // A newer user login or a newer automatic check owns the busy
                    // flag and cancellation handle; a stale result must not clear
                    // either of them before the generation comparison.
                    if generation != self.health_generation {
                        continue;
                    }
                    let cancelled = self
                        .health_cancel
                        .as_ref()
                        .is_some_and(|cancel| cancel.is_cancelled());
                    self.health_busy = false;
                    self.health_cancel = None;
                    self.health_attempts.insert(platform, health_now());
                    // The stored credential may have been replaced or removed while
                    // this request was in flight; its answer describes the old login
                    // and must never revive an outdated status.
                    let current_stamp = credential_stamp(&self.snapshot.root, platform);
                    let credentials_changed = stamp.is_some() && stamp != current_stamp;
                    let credentials_removed =
                        stamp.is_some() && !self.snapshot.credentials.imported_for(platform);
                    if credentials_changed || credentials_removed {
                        self.login_health.remove(&platform);
                        if !self.login_busy && !cancelled {
                            self.login_message = format!(
                                "{} 的凭据已更换或移除，已丢弃旧检查结果。",
                                platform.label()
                            );
                        }
                        continue;
                    }
                    match result {
                        Ok(health) => {
                            let previous = self.login_health.get(&platform).cloned();
                            let health = health.carry_verified(previous.as_ref());
                            if health.state != login::HealthState::Valid
                                && health.verified_at > 0
                                && !self.login_busy
                                && !cancelled
                            {
                                self.login_message = format!(
                                    "{}：{}；上次明确有效：{}",
                                    platform.label(),
                                    health.current_label(health_now()),
                                    login::time_text(health.verified_at)
                                );
                            }
                            self.login_health.insert(platform, health);
                        }
                        Err(error) if !self.login_busy && !cancelled => {
                            self.login_message =
                                format!("{} 登录状态自动检查未完成：{error}", platform.label());
                        }
                        Err(_) => {}
                    }
                }
            }
        }
    }

    /// Drops a platform's confirmed login state after its stored credentials were
    /// imported or removed. Any in-flight automatic check is cancelled and its
    /// result invalidated; the next accounts-page visit may check the new state.
    fn invalidate_login_health(&mut self, platform: Platform) {
        self.login_health.remove(&platform);
        self.health_attempts.remove(&platform);
        self.health_generation = self.health_generation.wrapping_add(1);
        if let Some(cancel) = self.health_cancel.take() {
            cancel.cancel();
        }
        self.health_busy = false;
    }

    fn send_store(&mut self, action: StoreAction) {
        match self.store_sender.send(action) {
            Ok(()) => self.storage_pending += 1,
            Err(_) => self.set_status("数据工作线程已退出，请重新打开程序后重试。", true),
        }
    }

    fn current_target(&self) -> Result<Target> {
        target_from_input(&self.room_input, self.platform, self.quality)
    }

    fn play_current(&mut self, context: &egui::Context) {
        match self.current_target() {
            Ok(target) => self.start_playback(target, context),
            Err(error) => {
                let message =
                    room_input_hint(&self.room_input, self.platform, &format!("{error:#}"));
                self.set_status(message, true);
            }
        }
    }

    fn start_playback(&mut self, target: Target, context: &egui::Context) {
        self.start_playback_with(target, self.player, self.snapshot.settings.clone(), context);
    }

    /// Shared launch path for the current preferences and for "重新打开", which
    /// reuses the original player and buffer preference instead of current UI state.
    fn start_playback_with(
        &mut self,
        target: Target,
        player: Player,
        settings: Settings,
        context: &egui::Context,
    ) {
        self.request_room_badge(&target);
        let overlay_enabled = settings.overlay.enabled;
        let launch = SessionLaunch {
            player,
            settings: settings.clone(),
        };
        let result = CookieVault::open(self.snapshot.root.join("credentials")).and_then(|vault| {
            self.sessions
                .start(target.clone(), player, settings, Some(vault))
        });
        match result {
            Ok(id) => {
                self.session_overlay.insert(id.clone(), overlay_enabled);
                self.session_launch.insert(id.clone(), launch);
                self.set_status(
                    format!(
                        "正在启动 {} {}，可在播放页或观看会话页取消；会话编号已记录。",
                        target.platform.label(),
                        target.room
                    ),
                    false,
                );
            }
            Err(error) => self.set_status(format!("无法创建播放会话：{error:#}"), true),
        }
        let _ = context;
        context.request_repaint();
    }

    fn add_current_favorite(&mut self) {
        match self.current_target() {
            Ok(target) => {
                let name = if self.favorite_name.trim().is_empty() {
                    target.to_string()
                } else {
                    self.favorite_name.trim().to_owned()
                };
                self.send_store(StoreAction::AddFavorite(name, target));
            }
            Err(error) => self.set_status(format!("不能收藏这个输入：{error:#}"), true),
        }
    }

    fn fill_room(&mut self, name: &str, target: &Target) {
        // Selecting a room is an explicit request for its current live state.
        self.request_room_badge(target);
        self.platform = target.platform;
        self.room_input = target.room.clone();
        self.quality = target.quality;
        self.favorite_name = name.to_owned();
        self.page = Page::Favorites;
        self.set_status(
            format!(
                "已填入 {} · {}，点击“打开直播”开始播放。",
                target.platform.label(),
                name
            ),
            false,
        );
    }

    fn cancel_search(&mut self) {
        self.search_generation = self.search_generation.wrapping_add(1);
        self.searching = false;
        self.verification_pending = false;
        self.search_pending_cache = None;
        if let Some(cancel) = self.ai_cancel.take() {
            cancel.cancel();
        }
        self.ai_busy = false;
    }

    fn start_search(&mut self, context: &egui::Context, refresh: bool) {
        let query = crate::search::SearchQuery::new(
            &self.search_input,
            &self.search_game,
            &self.search_identity,
            &self.search_alias,
        );
        if query.is_empty() || query.platform_terms().chars().count() > 200 {
            self.set_status("请输入 1–200 字的主播昵称，或补充外号/曾用名。", true);
            return;
        }
        // Explicit Douyin account clues are recognized locally (no network) and
        // stay in the account stage: no room or play action is invented.
        if !self.search_all && self.platform == Platform::Douyin {
            if let Some(candidate) =
                crate::search::douyin_account_candidate(self.search_input.trim())
            {
                self.search_generation = self.search_generation.wrapping_add(1);
                if let Some(cancel) = self.ai_cancel.take() {
                    cancel.cancel();
                }
                self.ai_busy = false;
                self.ai_result = None;
                self.searching = false;
                self.verification_pending = false;
                self.search_results = vec![candidate.clone()];
                self.search_outcomes.clear();
                self.search_cached_at = None;
                self.search_caption = format!("抖音账号线索 · {} · 房间待解析", candidate.name);
                self.set_status(
                    "已识别抖音账号线索：账号阶段不能播放或收藏；请补充主页/直播分享链接或房间号后核验。",
                    false,
                );
                return;
            }
        }
        self.search_generation = self.search_generation.wrapping_add(1);
        if let Some(cancel) = self.ai_cancel.take() {
            cancel.cancel();
        }
        self.ai_busy = false;
        self.ai_result = None;
        // A cache hit also supersedes any verification from the previous query.
        self.verification_pending = false;
        let id = self.search_generation;
        let platform = self.platform;
        let scope = if self.search_all {
            "all".to_owned()
        } else {
            platform.to_string()
        };
        let now = health_now();
        let key = query.cache_key(&scope);
        if refresh {
            self.search_cache.remove(&key);
        } else if let Some(hit) = self.search_cache.get(&key, now) {
            let mut candidates = hit.candidates.clone();
            crate::search::rank_candidates(&mut candidates, &query, self.search_all);
            let at = hit.searched_at;
            self.searching = false;
            self.search_results = candidates;
            self.search_outcomes.clear();
            self.search_cached_at = Some(at);
            self.search_caption = search_caption(
                if self.search_all {
                    "全部平台"
                } else {
                    platform.label()
                },
                &query,
                self.search_results.len(),
                Some(at),
            );
            self.set_status(
                format!(
                    "已读取近期搜索缓存（原检索时间 {}）；点击“刷新结果”可重新检索，结果仍需平台核验。",
                    search_time_text(at)
                ),
                false,
            );
            return;
        }
        self.searching = true;
        self.verification_pending = false;
        self.search_results.clear();
        self.search_outcomes.clear();
        self.search_cached_at = None;
        self.search_caption = format!(
            "正在搜索 {} · “{}”…",
            if self.search_all {
                "全部平台"
            } else {
                platform.label()
            },
            query.text()
        );
        self.set_status("正在查询平台公开搜索接口…", false);
        let sender = self.event_sender.clone();
        let context = context.clone();
        let root = self.snapshot.root.clone();
        if self.search_all {
            let pending: BTreeMap<Platform, (Vec<platforms::LookupCandidate>, u64)> = if refresh {
                BTreeMap::new()
            } else {
                Platform::ALL
                    .into_iter()
                    .filter_map(|platform| {
                        let key = query.cache_key(&platform.to_string());
                        self.search_cache
                            .get(&key, now)
                            .map(|hit| (platform, (hit.candidates.clone(), hit.searched_at)))
                    })
                    .collect()
            };
            let missing: Vec<Platform> = Platform::ALL
                .into_iter()
                .filter(|platform| !pending.contains_key(platform))
                .collect();
            self.search_pending_cache = Some((id, pending));
            thread::spawn(move || {
                let mut responses = match CookieVault::open(root.join("credentials")) {
                    Ok(vault) => {
                        crate::search::query_platforms(&missing, &query.platform_terms(), &vault)
                    }
                    Err(error) => missing
                        .into_iter()
                        .map(|platform| (platform, Err(format!("{error:#}"))))
                        .collect(),
                };
                // One bounded retry for an empty/failed Douyin result: its web
                // endpoint sometimes answers an empty user_list while the page is
                // still rate limiting, and a transient failure must not stick as a
                // cached zero. Other platforms have stable native search APIs.
                if let Some(index) = responses
                    .iter()
                    .position(|(platform, _)| *platform == Platform::Douyin)
                {
                    let retry = match &responses[index].1 {
                        Ok(candidates) => candidates.is_empty(),
                        Err(_) => true,
                    };
                    if retry {
                        thread::sleep(std::time::Duration::from_millis(350));
                        if let Ok(vault) = CookieVault::open(root.join("credentials")) {
                            responses[index].1 = platforms::search_authenticated(
                                Platform::Douyin,
                                &query.platform_terms(),
                                &vault,
                            )
                            .map_err(|error| format!("{error:#}"));
                        }
                    }
                }
                for (platform, response) in &responses {
                    match response {
                        Ok(candidates) => crate::diag::log(
                            "search",
                            crate::diag::Level::Info,
                            &format!(
                                "all-search platform={platform:?} count={} keyword_chars={}",
                                candidates.len(),
                                query.platform_terms().chars().count()
                            ),
                        ),
                        Err(error) => crate::diag::log(
                            "search",
                            crate::diag::Level::Warn,
                            &format!("all-search platform={platform:?} error={error}"),
                        ),
                    }
                }
                let _ = sender.send(Event::SearchAll {
                    id,
                    query,
                    responses,
                });
                context.request_repaint();
            });
        } else {
            let worker_query = query.clone();
            thread::spawn(move || {
                let result = CookieVault::open(root.join("credentials"))
                    .and_then(|vault| {
                        platforms::search_authenticated(
                            platform,
                            &worker_query.platform_terms(),
                            &vault,
                        )
                    })
                    .map_err(|error| format!("{error:#}"));
                match &result {
                    Ok(candidates) => crate::diag::log(
                        "search",
                        crate::diag::Level::Info,
                        &format!(
                            "search platform={platform:?} count={} keyword_chars={}",
                            candidates.len(),
                            worker_query.platform_terms().chars().count()
                        ),
                    ),
                    Err(error) => crate::diag::log(
                        "search",
                        crate::diag::Level::Warn,
                        &format!("search platform={platform:?} error={error}"),
                    ),
                }
                let _ = sender.send(Event::Search {
                    id,
                    platform,
                    query: worker_query,
                    key,
                    result,
                });
                context.request_repaint();
            });
        }
    }

    fn start_verification(
        &mut self,
        selected: platforms::LookupCandidate,
        context: &egui::Context,
    ) {
        if self.verification_pending {
            return;
        }
        self.verification_pending = true;
        self.set_status(format!("正在核验 {} 的账号与直播间…", selected.name), false);
        let id = self.search_generation;
        let root = self.snapshot.root.clone();
        let sender = self.event_sender.clone();
        let context = context.clone();
        thread::spawn(move || {
            let result = CookieVault::open(root.join("credentials"))
                .and_then(|vault| platforms::verify_authenticated(&selected, &vault))
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send(Event::Verification {
                id,
                selected,
                result,
            });
            context.request_repaint();
        });
    }
}

impl Launcher {
    /// Select a page by its stable key for the ui-shot diagnostic. Unknown keys
    /// return false so the caller reports them instead of guessing a page.
    #[allow(dead_code)]
    pub(super) fn shot_select_page(&mut self, key: &str) -> bool {
        match Page::from_key(key) {
            Some(page) => {
                self.page = page;
                true
            }
            None => false,
        }
    }
}

impl eframe::App for Launcher {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        // The screenshot driver is temporarily moved out of self so it can
        // receive a mutable launcher without aliasing the field it lives in.
        let mut shot = self.ui_shot.take();
        if let Some(driver) = shot.as_mut() {
            driver.begin_frame(self, context);
        }
        self.render(context);
        if let Some(driver) = shot.as_mut() {
            driver.end_frame(self, context);
        }
        self.ui_shot = shot;
    }
}

fn target_from_input(input: &str, platform: Platform, quality: Quality) -> Result<Target> {
    let input = input.trim();
    let mut target = if input.contains(':') {
        Target::parse(input)?
    } else {
        Target::new(platform, input)?
    };
    target.quality = quality;
    Ok(target)
}

/// Window/taskbar icon drawn from the launcher accent colour so the native
/// window does not keep eframe's placeholder icon. Pure and testable: no files,
/// no native window, no system state.
pub fn app_icon() -> egui::IconData {
    const SIZE: u32 = 64;
    const CORNER: f32 = 14.0;
    let mut rgba = vec![0_u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            if !inside_rounded_square(px, py, SIZE as f32, CORNER) {
                continue;
            }
            let (r, g, b) = if point_in_triangle(px, py, (24.0, 17.0), (24.0, 47.0), (49.0, 32.0)) {
                (30, 24, 20) // dark play mark on the accent plate
            } else {
                (244, 137, 67) // ACCENT
            };
            let index = ((y * SIZE + x) * 4) as usize;
            rgba[index..index + 4].copy_from_slice(&[r, g, b, 255]);
        }
    }
    egui::IconData {
        rgba,
        width: SIZE,
        height: SIZE,
    }
}

fn inside_rounded_square(x: f32, y: f32, size: f32, radius: f32) -> bool {
    let nearest_x = x.clamp(radius, size - radius);
    let nearest_y = y.clamp(radius, size - radius);
    let dx = x - nearest_x;
    let dy = y - nearest_y;
    dx * dx + dy * dy <= radius * radius
}

fn point_in_triangle(px: f32, py: f32, a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> bool {
    let sign = |p: (f32, f32), q: (f32, f32), r: (f32, f32)| -> f32 {
        (p.0 - r.0) * (q.1 - r.1) - (q.0 - r.0) * (p.1 - r.1)
    };
    let point = (px, py);
    let d1 = sign(point, a, b);
    let d2 = sign(point, b, c);
    let d3 = sign(point, c, a);
    let has_negative = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_positive = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(has_negative && has_positive)
}

pub(crate) fn install_fonts(context: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "HarmonyOS Sans SC".to_owned(),
        FontData::from_static(include_bytes!(
            "../assets/fonts/HarmonyOS_Sans_SC_Regular.ttf"
        ))
        .into(),
    );
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "HarmonyOS Sans SC".to_owned());
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .push("HarmonyOS Sans SC".to_owned());
    context.set_fonts(fonts);
}

fn apply_theme(context: &egui::Context, dark: bool) {
    let mut style = egui::Style::default();
    style.visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    style.visuals.selection.bg_fill = ACCENT.gamma_multiply(0.55);
    style.visuals.hyperlink_color = ACCENT;
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(14.0, 9.0);
    style
        .text_styles
        .insert(TextStyle::Body, FontId::proportional(15.0));
    style
        .text_styles
        .insert(TextStyle::Button, FontId::proportional(14.0));
    style
        .text_styles
        .insert(TextStyle::Small, FontId::proportional(12.0));
    style
        .text_styles
        .insert(TextStyle::Heading, FontId::proportional(22.0));
    context.set_style(style);
}

fn primary_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(
        RichText::new(text)
            .color(Color32::from_rgb(30, 24, 20))
            .strong(),
    )
    .fill(ACCENT)
    .corner_radius(8)
    .stroke(egui::Stroke::NONE)
}

fn card(ui: &mut egui::Ui, dark: bool, content: impl FnOnce(&mut egui::Ui)) {
    let width = ui.available_width();
    egui::Frame::new()
        .fill(if dark {
            Color32::from_rgb(27, 31, 40)
        } else {
            Color32::WHITE
        })
        .stroke(egui::Stroke::new(
            1.0_f32,
            if dark {
                Color32::from_rgb(41, 46, 58)
            } else {
                Color32::from_rgb(228, 231, 238)
            },
        ))
        .corner_radius(12)
        .inner_margin(18)
        .show(ui, |ui| {
            ui.set_min_width((width - 38.0).max(0.0));
            content(ui);
        });
}

fn empty_state(ui: &mut egui::Ui, title: &str, subtitle: &str) {
    ui.add_space(12.0);
    ui.label(RichText::new(title).size(17.0).strong());
    ui.label(RichText::new(subtitle).weak());
    ui.add_space(18.0);
}

fn quality_label(quality: Quality) -> &'static str {
    match quality {
        Quality::Best => "原画 / 最佳",
        Quality::High => "高清",
        Quality::Low => "流畅",
    }
}

fn history_time(entry: &HistoryEntry) -> String {
    entry
        .played_at
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M")
                .to_string()
        })
        .or_else(|| entry.legacy_time.clone())
        .unwrap_or_else(|| "导入记录".into())
}

pub fn followers_text(count: u64) -> String {
    crate::search::followers_text(count)
}

/// Local display time for cached search results (seconds matter for freshness).
fn search_time_text(at: u64) -> String {
    if at == 0 {
        return "时间未知".to_owned();
    }
    chrono::Local
        .timestamp_opt(at as i64, 0)
        .single()
        .map(|time| time.format("%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "时间未知".to_owned())
}

fn search_caption(
    scope: &str,
    query: &crate::search::SearchQuery,
    candidates: usize,
    cached_at: Option<u64>,
) -> String {
    let mut caption = format!("{scope} · “{}” · {candidates} 个候选", query.text());
    if let Some(at) = cached_at {
        caption.push_str(&format!(" · 缓存（原检索时间 {}）", search_time_text(at)));
    }
    caption
}

fn search_all_caption(
    query: &crate::search::SearchQuery,
    outcomes: &[crate::search::PlatformOutcome],
    candidates: usize,
) -> String {
    let details = outcomes
        .iter()
        .map(|outcome| {
            let state = if !outcome.success {
                "受阻".to_owned()
            } else if outcome.cached {
                format!(
                    "{}条·缓存 {}",
                    outcome.candidates,
                    search_time_text(outcome.searched_at)
                )
            } else {
                format!("{}条", outcome.candidates)
            };
            format!("{} {state}", outcome.platform.label())
        })
        .collect::<Vec<_>>()
        .join(" / ");
    format!(
        "全部平台 · “{}” · {candidates} 个候选 · {details}",
        query.text()
    )
}
fn path_text(path: &Option<PathBuf>) -> String {
    path.as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_default()
}
/// Renders reserved rectangles back into the documented one-per-line text form.
pub fn reserved_areas_text(areas: &[PercentRect]) -> String {
    areas
        .iter()
        .map(|area| area.encode())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parses the documented x,y,width,height lines; an invalid line rejects the
/// whole save instead of silently dropping a reserved area.
pub fn parse_reserved_areas(text: &str) -> Result<Vec<PercentRect>> {
    let mut areas = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        ensure!(
            areas.len() < PercentRect::MAX_AREAS,
            "画面安全区最多 {} 项",
            PercentRect::MAX_AREAS
        );
        let area = PercentRect::parse(line).with_context(|| {
            format!(
                "第 {} 行安全区格式无效，应为 x,y,宽,高（宽高大于 0）",
                index + 1
            )
        })?;
        areas.push(area);
    }
    Ok(areas)
}

/// Parses one-argument-per-line player flags and mirrors the store validation,
/// so an invalid draft never reaches disk.
pub fn parse_extra_args(text: &str, prefix: &str, limit: usize) -> Result<Vec<String>> {
    let mut arguments = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        ensure!(arguments.len() < limit, "播放器高级参数最多 {limit} 项");
        ensure!(
            line.starts_with(prefix) && line.len() <= 1024 && !line.chars().any(char::is_control),
            "播放器高级参数必须每项以 {prefix} 开头、不超过 1024 字节且不含控制字符"
        );
        arguments.push(line.to_owned());
    }
    Ok(arguments)
}

fn path_field(ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str) {
    ui.label(label);
    ui.add_sized(
        [ui.available_width(), 36.0],
        egui::TextEdit::singleline(value).hint_text(hint),
    );
    ui.add_space(8.0);
}

/// Player/tool path editor with a native "浏览…" dialog, auto-detection and
/// inline validity feedback. Empty means "find automatically"; a non-empty value
/// is checked for absolute path, existence and executable-like size before save.
fn tool_path_field(
    ui: &mut egui::Ui,
    tool: file_dialog::ToolKind,
    label: &str,
    value: &mut String,
) {
    ui.label(label);
    let mut action_message: Option<(bool, String)> = None;
    ui.horizontal(|ui| {
        let width = (ui.available_width() - 190.0).max(120.0);
        ui.add_sized(
            [width, 36.0],
            egui::TextEdit::singleline(value).hint_text(tool.hint()),
        );
        if ui
            .button("浏览…")
            .on_hover_text("用 Windows 原生文件对话框选择已安装的程序")
            .clicked()
        {
            match file_dialog::pick_executable(tool, value) {
                Some(path) => {
                    let display = path.display().to_string();
                    *value = display.clone();
                    action_message = Some((false, format!("已选择 {display}")));
                }
                None => {
                    action_message =
                        Some((false, "未选择文件（原生对话框取消或当前系统不支持）".into()));
                }
            }
        }
        if ui
            .button("自动检测")
            .on_hover_text("按程序目录、bin、PATH 与常见安装目录查找")
            .clicked()
        {
            match file_dialog::detect_tool(tool) {
                Some(path) => {
                    let display = path.display().to_string();
                    *value = display.clone();
                    action_message = Some((false, format!("已找到 {display}")));
                }
                None => {
                    action_message = Some((
                        true,
                        format!(
                            "未自动找到 {}；请用“浏览…”选择，或留空稍后自动查找",
                            tool.label()
                        ),
                    ));
                }
            }
        }
    });
    match file_dialog::path_status(tool, value) {
        Some((true, text)) => {
            ui.label(
                RichText::new(text)
                    .small()
                    .color(Color32::from_rgb(126, 201, 145)),
            );
        }
        Some((false, text)) => {
            ui.label(RichText::new(text).small().color(Color32::LIGHT_RED));
        }
        None => {
            ui.label(
                RichText::new("留空：按程序目录与 PATH 自动查找；非空必须是绝对路径且文件存在")
                    .small()
                    .weak(),
            );
        }
    }
    if let Some((error, text)) = action_message {
        ui.label(RichText::new(text).small().color(if error {
            Color32::LIGHT_RED
        } else {
            ACCENT
        }));
    }
    ui.add_space(8.0);
}

fn read_import(path: &Path) -> Result<String> {
    let file = File::open(path).context("无法打开所选 JSON 文件")?;
    let metadata = file.metadata().context("无法读取所选文件信息")?;
    ensure!(metadata.is_file(), "请选择普通 JSON 文件");
    ensure!(
        metadata.len() <= MAX_IMPORT_BYTES,
        "导入文件超过 2 MiB 限额"
    );
    let mut bytes = Vec::new();
    file.take(MAX_IMPORT_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("读取导入文件失败")?;
    ensure!(
        bytes.len() as u64 <= MAX_IMPORT_BYTES,
        "导入文件超过 2 MiB 限额"
    );
    let text = String::from_utf8(bytes).context("导入文件必须使用 UTF-8 编码")?;
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned())
}

/// Reads only the isolated credential directory; never scans anywhere else.
fn credential_state(data_root: &Path) -> CredentialState {
    let vault_root = data_root.join("credentials");
    if !vault_root.is_dir() {
        // Do not create the vault just to render its status.
        return CredentialState {
            supported: cfg!(windows),
            imported: BTreeMap::new(),
            error: None,
        };
    }
    match CookieVault::open(vault_root) {
        Ok(vault) => {
            let mut imported = BTreeMap::new();
            let mut error: Option<String> = None;
            for platform in Platform::ALL {
                match vault.has_credentials(platform) {
                    Ok(flag) => {
                        imported.insert(platform, flag);
                    }
                    Err(failure) => {
                        imported.insert(platform, false);
                        error.get_or_insert_with(|| format!("{failure:#}"));
                    }
                }
            }
            CredentialState {
                supported: true,
                imported,
                error,
            }
        }
        Err(failure) => CredentialState {
            supported: cfg!(windows),
            imported: BTreeMap::new(),
            error: Some(format!("{failure:#}")),
        },
    }
}

/// Spawns the detached transparent overlay for a just-launched player.
///
/// The overlay process decides on its own whether it should become visible; the
/// launcher only creates it and reports the outcome without blocking playback.
fn launch_overlay(target: &Target, player_pid: u32, data_root: &Path) -> OverlayLaunch {
    if !cfg!(windows) {
        return OverlayLaunch::UnsupportedSystem;
    }
    if !overlay_platform_supported(target.platform) {
        return OverlayLaunch::UnsupportedPlatform(target.platform);
    }
    let arguments = match overlay_arguments_with_data_root(target, player_pid, data_root) {
        Ok(arguments) => arguments,
        Err(error) => return OverlayLaunch::Failed(format!("{error:#}")),
    };
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => return OverlayLaunch::Failed(format!("无法定位当前程序：{error}")),
    };
    spawn_overlay_child(&executable, &arguments)
}

#[cfg(windows)]
fn spawn_overlay_child(executable: &Path, arguments: &[OsString]) -> OverlayLaunch {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let mut command = Command::new(executable);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    match command.spawn() {
        // The overlay is independent: dropping the handle leaves it running, so
        // it survives launcher exit exactly like the external player process.
        Ok(_child) => OverlayLaunch::Launched,
        Err(error) => OverlayLaunch::Failed(format!("无法启动弹幕覆盖层进程：{error}")),
    }
}

#[cfg(not(windows))]
fn spawn_overlay_child(_executable: &Path, _arguments: &[OsString]) -> OverlayLaunch {
    OverlayLaunch::UnsupportedSystem
}

#[cfg(windows)]
fn open_data_folder(path: &Path) -> Result<()> {
    use std::process::{Command, Stdio};
    ensure!(path.is_dir(), "数据目录还不存在");
    Command::new("explorer.exe")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("无法启动资源管理器")?;
    Ok(())
}

#[cfg(not(windows))]
fn open_data_folder(path: &Path) -> Result<()> {
    ensure!(path.is_dir(), "数据目录还不存在");
    Err(anyhow!("当前系统没有可用的资源管理器"))
}

fn spawn_store_worker(
    mut store: Store,
    receiver: Receiver<StoreAction>,
    sender: Sender<Event>,
    context: egui::Context,
) {
    thread::spawn(move || {
        while let Ok(action) = receiver.recv() {
            let quiet = matches!(&action, StoreAction::RecordPlay(_, _));
            let result: std::result::Result<String, String> = match action {
                StoreAction::AddFavorite(name, target) => store
                    .add_favorite(name, target)
                    .map(|_| "收藏已保存。".to_owned())
                    .map_err(|error| format!("保存收藏失败：{error:#}")),
                StoreAction::RemoveFavorite(platform, room) => store
                    .remove_favorite(platform, &room)
                    .map(|_| "已移除收藏。".to_owned())
                    .map_err(|error| format!("移除收藏失败：{error:#}")),
                StoreAction::RecordPlay(name, target) => store
                    .record_play(name, target)
                    .map(|_| "播放历史已更新。".to_owned())
                    .map_err(|error| format!("更新播放历史失败：{error:#}")),
                StoreAction::SaveSettings(settings) => store
                    .save_player_paths(settings)
                    .map(|_| "播放器路径已保存。下次播放使用新设置。".to_owned())
                    .map_err(|error| format!("保存设置失败：{error:#}")),
                StoreAction::SavePlayback(settings) => store
                    .save_playback_preferences(settings)
                    .map(|()| "播放设置已保存，将用于新会话。".to_owned())
                    .map_err(|error| format!("播放设置保存失败：{error:#}")),
                StoreAction::SaveOverlay(settings) => store
                    .save_overlay_settings(settings)
                    .map(|_| "弹幕设置已保存，将用于新启动的覆盖层。".to_owned())
                    .map_err(|error| format!("弹幕设置保存失败；仍使用已保存配置：{error:#}")),
                StoreAction::Import(path, kind) => read_import(&path)
                    .and_then(|json| match kind {
                        ImportKind::FavoritesOrExport => store.import_json(&json),
                        ImportKind::LegacyHistory => store.import_legacy_history_json(&json),
                    })
                    .map(|summary| {
                        format!(
                            "导入完成：新增收藏 {}，更新收藏 {}，新增历史 {}，跳过历史 {}。",
                            summary.favorites_added,
                            summary.favorites_updated,
                            summary.history_added,
                            summary.history_skipped
                        )
                    })
                    .map_err(|error| format!("导入失败：{error:#}")),
                StoreAction::CredentialImport(platform, path) => read_credential_text(&path)
                    .and_then(|text| {
                        let vault = CookieVault::open(store.root().join("credentials"))?;
                        vault.import(platform, &text)
                    })
                    .map(|()| {
                        format!(
                            "已导入 {} 凭据（Windows DPAPI 加密保存）。",
                            platform.label()
                        )
                    })
                    .map_err(|error| format!("导入凭据失败：{error:#}")),
                StoreAction::CredentialRemove(platform) => {
                    CookieVault::open(store.root().join("credentials"))
                        .and_then(|vault| vault.remove(platform))
                        .map(|()| format!("已清除 {} 的凭据。", platform.label()))
                        .map_err(|error| format!("清除凭据失败：{error:#}"))
                }
                StoreAction::ClearBrowserProfiles(platform) => {
                    // Only this platform's dedicated browser-assist directories are
                    // removed; other profiles and unrelated files are left untouched.
                    crate::login::browser::remove_platform_profiles(
                        &store.root().join("browser-login"),
                        platform,
                    )
                    .map(|removed| {
                        format!(
                            "已清理 {} 的独立浏览器辅助资料目录 {removed} 个。",
                            platform.label()
                        )
                    })
                    .map_err(|error| format!("清理网页辅助资料失败：{error:#}"))
                }
                StoreAction::SaveAi(config, mut key) => {
                    let result = crate::ai::AiVault::new(store.root().join("ai"))
                        .save(config, &key)
                        .map(|()| "AI设置已加密保存；尚未发送付费请求。".into())
                        .map_err(|e| e.to_string());
                    unsafe { key.as_bytes_mut().fill(0) };
                    result
                }
                StoreAction::ClearAi => crate::ai::AiVault::new(store.root().join("ai"))
                    .clear()
                    .map(|()| "本程序AI密钥已清除。".into())
                    .map_err(|e| e.to_string()),
                StoreAction::Refresh => store
                    .reload()
                    .map(|()| "已刷新数据与凭据状态。".to_owned())
                    .map_err(|e| format!("数据刷新失败：{e}")),
            };
            if sender
                .send(Event::Storage {
                    snapshot: Snapshot::from_store(&store),
                    message: result,
                    quiet,
                })
                .is_err()
            {
                break;
            }
            context.request_repaint();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::DanmakuBehavior;

    #[test]
    fn late_ai_results_cannot_fill_a_new_query_or_clear_its_busy_state() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("synthetic-ui")).unwrap();
        let context = egui::Context::default();
        let mut app = Launcher::from_context(&context, store);
        app.search_generation = 2;
        app.ai_busy = true;
        app.verification_pending = true;
        app.room_input = "456".into();
        app.event_sender
            .send(Event::Ai {
                id: 1,
                optimize: false,
                result: Err("stale result".into()),
            })
            .unwrap();
        app.event_sender
            .send(Event::AiVerified {
                id: 1,
                result: Ok(platforms::LookupCandidate {
                    platform: Platform::Bili,
                    room: "123".into(),
                    name: "synthetic".into(),
                    ..Default::default()
                }),
            })
            .unwrap();
        app.poll_events();
        assert!(app.ai_busy && app.verification_pending);
        assert_eq!(app.room_input, "456");
        assert!(app.ai_result.is_none());
    }

    #[test]
    fn an_automatic_health_result_never_overwrites_a_newer_login_flow() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("synthetic-ui")).unwrap();
        let context = egui::Context::default();
        let mut app = Launcher::from_context(&context, store);
        let health = |state| login::LoginHealth {
            platform: Platform::Bili,
            state,
            reason: "合成检查".into(),
            checked_at: 1_000,
            verified_at: 0,
        };
        app.health_generation = 2;
        app.event_sender
            .send(Event::Health {
                generation: 1,
                platform: Platform::Bili,
                stamp: None,
                result: Ok(health(login::HealthState::Invalid)),
            })
            .unwrap();
        app.poll_events();
        assert!(
            app.login_health.is_empty(),
            "a superseded automatic check must not be recorded"
        );
        app.event_sender
            .send(Event::Health {
                generation: 2,
                platform: Platform::Bili,
                stamp: None,
                result: Ok(health(login::HealthState::Valid)),
            })
            .unwrap();
        app.poll_events();
        assert_eq!(
            app.login_health
                .get(&Platform::Bili)
                .map(|health| health.state),
            Some(login::HealthState::Valid)
        );
        assert!(app.health_attempts.contains_key(&Platform::Bili));
        assert!(!app.health_busy);
    }

    #[test]
    fn a_stale_health_result_cannot_clear_a_newer_check() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("synthetic-ui")).unwrap();
        let context = egui::Context::default();
        let mut app = Launcher::from_context(&context, store);
        let cancel = login::Cancellation::new();
        app.health_generation = 2;
        app.health_busy = true;
        app.health_cancel = Some(cancel.clone());
        app.event_sender
            .send(Event::Health {
                generation: 1,
                platform: Platform::Bili,
                stamp: None,
                result: Ok(login::LoginHealth {
                    platform: Platform::Bili,
                    state: login::HealthState::Invalid,
                    reason: "合成检查".into(),
                    checked_at: 1_000,
                    verified_at: 0,
                }),
            })
            .unwrap();
        app.poll_events();
        assert!(
            app.health_busy,
            "a stale result must not hide the newer check"
        );
        assert!(!cancel.is_cancelled());
        assert!(app.login_health.is_empty());
        app.event_sender
            .send(Event::Health {
                generation: 2,
                platform: Platform::Bili,
                stamp: None,
                result: Ok(login::LoginHealth {
                    platform: Platform::Bili,
                    state: login::HealthState::Valid,
                    reason: "合成检查".into(),
                    checked_at: 1_000,
                    verified_at: 1_000,
                }),
            })
            .unwrap();
        app.poll_events();
        assert!(!app.health_busy);
        assert!(app.health_cancel.is_none());
        assert_eq!(
            app.login_health
                .get(&Platform::Bili)
                .map(|health| health.state),
            Some(login::HealthState::Valid)
        );
    }

    #[test]
    fn changed_credentials_drop_the_old_platform_health() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("synthetic-ui")).unwrap();
        let context = egui::Context::default();
        let mut app = Launcher::from_context(&context, store);
        app.health_generation = 3;
        app.health_busy = true;
        app.health_cancel = Some(login::Cancellation::new());
        app.health_attempts.insert(Platform::Bili, 5_000);
        app.login_health.insert(
            Platform::Bili,
            login::LoginHealth {
                platform: Platform::Bili,
                state: login::HealthState::Valid,
                reason: "合成检查".into(),
                checked_at: 4_000,
                verified_at: 4_000,
            },
        );
        app.invalidate_login_health(Platform::Bili);
        assert!(app.login_health.is_empty());
        assert!(app.health_attempts.is_empty());
        assert_eq!(app.health_generation, 4);
        assert!(!app.health_busy);
        assert!(app.health_cancel.is_none());
    }

    #[test]
    fn all_pages_render_headless_egui_frames_at_supported_window_sizes() {
        // This renders actual widgets and tessellates their paint commands, but
        // never creates a native window or contacts a platform/player.
        for size in [
            egui::vec2(DESIGN_SIZE[0], DESIGN_SIZE[1]),
            egui::vec2(MIN_SIZE[0], MIN_SIZE[1]),
            egui::vec2(1100.0, 740.0),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let mut store = Store::open(directory.path().join("synthetic-ui")).unwrap();
            let target = Target::new(Platform::Bili, "12345").unwrap();
            store
                .add_favorite("合成主播".into(), target.clone())
                .unwrap();
            store.record_play("合成主播".into(), target).unwrap();
            let context = egui::Context::default();
            let mut app = Launcher::from_context(&context, store);
            let mut texture_updates = 0;
            for (index, page) in Page::ALL.into_iter().enumerate() {
                app.page = page;
                app.dark = index % 2 == 0;
                apply_theme(&context, app.dark);
                let output = context.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                        time: Some(index as f64 / 60.0),
                        ..Default::default()
                    },
                    |ctx| app.render(ctx),
                );
                assert!(!output.shapes.is_empty(), "page produced no paint commands");
                texture_updates += output.textures_delta.set.len();
                let primitives = context.tessellate(output.shapes, output.pixels_per_point);
                assert!(!primitives.is_empty(), "page produced no visible mesh");
                for primitive in primitives {
                    assert!(
                        primitive.clip_rect.min.x.is_finite()
                            && primitive.clip_rect.min.y.is_finite()
                    );
                    assert!(
                        primitive.clip_rect.max.x.is_finite()
                            && primitive.clip_rect.max.y.is_finite()
                    );
                    if let egui::epaint::Primitive::Mesh(mesh) = primitive.primitive {
                        assert!(
                            mesh.vertices
                                .iter()
                                .all(|vertex| vertex.pos.x.is_finite() && vertex.pos.y.is_finite())
                        );
                        assert!(
                            mesh.indices
                                .iter()
                                .all(|index| (*index as usize) < mesh.vertices.len())
                        );
                    }
                }
            }
            assert!(
                texture_updates > 0,
                "embedded font atlas was never prepared"
            );
        }
    }

    #[test]
    fn failed_settings_write_keeps_draft_and_reports_the_committed_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("synthetic-ui")).unwrap();
        let context = egui::Context::default();
        let mut app = Launcher::from_context(&context, store);
        app.overlay_draft.enabled = false;
        app.send_store(StoreAction::SaveOverlay(OverlaySettings {
            max_visible: 0,
            ..app.overlay_draft.clone()
        }));
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while app.storage_pending > 0 && std::time::Instant::now() < deadline {
            app.poll_events();
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(app.storage_pending, 0);
        assert!(app.status_error);
        assert!(app.status.contains("保存失败") && app.status.contains("已保存配置"));
        assert!(
            !app.overlay_draft.enabled,
            "the unsaved user draft must remain recoverable"
        );
        assert!(
            app.snapshot.settings.overlay.enabled,
            "a failed save cannot change the committed toggle"
        );
    }

    #[test]
    fn launcher_initializes_the_toggle_from_saved_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(directory.path().join("synthetic-ui")).unwrap();
        store
            .save_overlay_settings(OverlaySettings {
                enabled: false,
                font_size: 39,
                ..Default::default()
            })
            .unwrap();
        let context = egui::Context::default();
        let app = Launcher::from_context(&context, store);
        assert!(!app.overlay_draft.enabled);
        assert_eq!(app.overlay_draft.font_size, 39);
        assert_eq!(app.overlay_draft, app.snapshot.settings.overlay);
    }

    #[test]
    fn selected_platform_applies_to_bare_rooms_but_explicit_links_keep_their_platform() {
        let bare = target_from_input("example", Platform::Huya, Quality::Low).unwrap();
        assert_eq!(bare.platform, Platform::Huya);
        assert_eq!(bare.quality, Quality::Low);
        let explicit = target_from_input(
            "https://live.bilibili.com/123",
            Platform::Huya,
            Quality::High,
        )
        .unwrap();
        assert_eq!(explicit.platform, Platform::Bili);
        assert_eq!(explicit.quality, Quality::High);
        assert!(target_from_input("", Platform::Bili, Quality::Best).is_err());
    }

    #[test]
    fn explicit_import_reads_only_the_selected_synthetic_file() {
        let directory = tempfile::tempdir().unwrap();
        let selected = directory.path().join("selected.json");
        std::fs::write(&selected, r#"{"Synthetic selected":"bili:123"}"#).unwrap();
        std::fs::write(directory.path().join("favorites.json"), "unselected marker").unwrap();
        let root = directory.path().join("new-store");
        let mut store = Store::open(root.clone()).unwrap();
        assert!(store.favorites().is_empty());
        let json = read_import(&selected).unwrap();
        store.import_json(&json).unwrap();
        assert_eq!(store.favorites().len(), 1);
        assert_eq!(store.favorites()[0].name, "Synthetic selected");
        assert_eq!(
            std::fs::read_to_string(directory.path().join("favorites.json")).unwrap(),
            "unselected marker"
        );
    }

    #[test]
    fn import_rejects_oversized_and_non_utf8_files_before_parsing() {
        let directory = tempfile::tempdir().unwrap();
        let oversized = directory.path().join("oversized.json");
        let file = File::create(&oversized).unwrap();
        file.set_len(MAX_IMPORT_BYTES + 1).unwrap();
        drop(file);
        assert!(read_import(&oversized).is_err());
        let invalid = directory.path().join("invalid.json");
        std::fs::write(&invalid, [0xff, 0xfe]).unwrap();
        assert!(read_import(&invalid).is_err());
    }

    #[test]
    fn overlay_command_plan_is_platform_scoped_and_requires_a_player_pid() {
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
        assert_eq!(
            overlay_arguments(&Target::new(Platform::Douyu, "room_1").unwrap(), 7).unwrap()[1],
            "douyu:room_1"
        );
        assert!(overlay_arguments(&bili, 0).is_err());
        assert_eq!(
            overlay_arguments(&Target::new(Platform::Huya, "abc").unwrap(), 1).unwrap()[1],
            "huya:abc"
        );
        assert_eq!(
            overlay_arguments(&Target::new(Platform::Douyin, "987654").unwrap(), 1).unwrap()[1],
            "douyin:987654"
        );
        assert!(
            Platform::ALL
                .iter()
                .all(|platform| overlay_platform_supported(*platform))
        );
    }

    #[test]
    fn overlay_status_text_stays_honest_about_partial_and_failed_launches() {
        assert_eq!(
            overlay_status_text("合成主播", &OverlayLaunch::Disabled),
            "已启动播放器：合成主播。"
        );
        let launched = overlay_status_text("合成主播", &OverlayLaunch::Launched);
        assert!(launched.contains("弹幕覆盖层已启动") && launched.contains("合成主播"));
        let unsupported = overlay_status_text(
            "合成主播",
            &OverlayLaunch::UnsupportedPlatform(Platform::Huya),
        );
        assert!(
            unsupported.contains("虎牙")
                && unsupported.contains("未启动")
                && unsupported.contains("播放不受影响")
        );
        let system = overlay_status_text("合成主播", &OverlayLaunch::UnsupportedSystem);
        assert!(system.contains("Windows") && system.contains("播放不受影响"));
        let failed = overlay_status_text("合成主播", &OverlayLaunch::Failed("合成失败".to_owned()));
        assert!(
            failed.contains("启动失败")
                && failed.contains("合成失败")
                && failed.contains("播放不受影响")
        );
        assert!(OverlayLaunch::Failed("合成失败".to_owned()).is_failure());
        assert!(!OverlayLaunch::Launched.is_failure());
        assert!(!OverlayLaunch::Disabled.is_failure());
    }

    #[test]
    fn credential_text_reader_accepts_synthetic_text_and_rejects_bad_files() {
        let directory = tempfile::tempdir().unwrap();
        let valid = directory.path().join("cookies.txt");
        std::fs::write(
            &valid,
            "\u{feff}SESSION=SYNTHETIC_ONE; token=SYNTHETIC_TWO\n",
        )
        .unwrap();
        assert_eq!(
            read_credential_text(&valid).unwrap(),
            "SESSION=SYNTHETIC_ONE; token=SYNTHETIC_TWO\n"
        );
        assert!(read_credential_text(directory.path()).is_err());
        assert!(read_credential_text(&directory.path().join("missing.txt")).is_err());
        let invalid = directory.path().join("invalid.txt");
        std::fs::write(&invalid, [0xff, 0xfe, 0xfd]).unwrap();
        assert!(read_credential_text(&invalid).is_err());
        let oversized = directory.path().join("oversized.txt");
        let file = File::create(&oversized).unwrap();
        file.set_len(MAX_CREDENTIAL_BYTES + 1).unwrap();
        drop(file);
        assert!(read_credential_text(&oversized).is_err());
    }

    #[test]
    fn credential_state_reads_only_the_isolated_directory() {
        let directory = tempfile::tempdir().unwrap();
        let data_root = directory.path().join("synthetic-data");
        std::fs::create_dir_all(&data_root).unwrap();
        let state = credential_state(&data_root);
        assert!(!data_root.join("credentials").exists());
        assert!(!state.imported_for(Platform::Bili));
        assert!(state.error.is_none());
        assert_eq!(state.supported, cfg!(windows));
    }

    #[cfg(windows)]
    #[test]
    fn credential_state_after_explicit_import_and_removal() {
        let directory = tempfile::tempdir().unwrap();
        let data_root = directory.path().join("synthetic-data");
        std::fs::create_dir_all(&data_root).unwrap();
        let vault = CookieVault::open(data_root.join("credentials")).unwrap();
        vault
            .import(Platform::Bili, "SESSION=SYNTHETIC_STATUS_MARKER")
            .unwrap();
        let state = credential_state(&data_root);
        assert!(state.supported);
        assert!(state.imported_for(Platform::Bili));
        assert!(!state.imported_for(Platform::Douyu));
        vault.remove(Platform::Bili).unwrap();
        assert!(!credential_state(&data_root).imported_for(Platform::Bili));
    }

    #[test]
    fn credential_labels_distinguish_imported_missing_and_unsupported() {
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
        assert!(credential_platform_label(Platform::Huya, &state).contains("未导入"));
        let unsupported = CredentialState {
            supported: false,
            imported: BTreeMap::new(),
            error: None,
        };
        assert!(credential_platform_label(Platform::Douyin, &unsupported).contains("不支持"));
    }

    #[test]
    fn pasted_links_select_platform_but_bare_room_input_keeps_the_picker() {
        assert_eq!(
            detected_platform("https://live.bilibili.com/123"),
            Some(Platform::Bili)
        );
        assert_eq!(
            detected_platform("https://www.huya.com/example_room-1"),
            Some(Platform::Huya)
        );
        assert_eq!(
            detected_platform("https://live.douyin.com/987654"),
            Some(Platform::Douyin)
        );
        assert_eq!(detected_platform("douyu:abc123"), Some(Platform::Douyu));
        assert_eq!(detected_platform("12345"), None);
        assert_eq!(detected_platform(""), None);
        assert_eq!(detected_platform("https://example.invalid/123"), None);
    }

    #[test]
    fn room_errors_gain_platform_specific_hints() {
        let numeric = Target::new(Platform::Bili, "abc").unwrap_err();
        let hint = room_input_hint("abc", Platform::Bili, &format!("{numeric:#}"));
        assert!(hint.contains("B站") && hint.contains("纯数字"));
        let huya = Target::new(Platform::Huya, "bad/room").unwrap_err();
        let hint = room_input_hint("bad/room", Platform::Huya, &format!("{huya:#}"));
        assert!(hint.contains("虎牙") && hint.contains("只能包含"));
        let empty = room_input_hint(
            "",
            Platform::Bili,
            "Enter a room ID or an official room URL",
        );
        assert!(empty.contains("房间号"));
        let link = room_input_hint(
            "https://example.invalid/1",
            Platform::Bili,
            "Use an official live-room URL or platform:room",
        );
        assert!(link.contains("官方直播间链接"));
    }

    #[test]
    fn feature_summaries_state_overlay_and_credential_limits() {
        let implemented = implemented_summary().join("\n");
        assert!(implemented.contains("弹幕覆盖层"));
        assert!(implemented.contains("DPAPI"));
        assert!(implemented.contains("平台取流"));
        assert!(implemented.contains("虎牙") && implemented.contains("抖音"));
        let pending = pending_summary().join("\n");
        assert!(
            pending.contains("SC")
                && pending.contains("未迁移")
                && pending.contains("真实平台验收")
        );
        assert!(!implemented_summary().is_empty() && !pending_summary().is_empty());
    }

    fn synthetic_context_launcher() -> (Launcher, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("synthetic-ui")).unwrap();
        let context = egui::Context::default();
        let app = Launcher::from_context(&context, store);
        (app, directory)
    }

    #[test]
    fn a_later_health_failure_keeps_the_last_explicit_valid_time() {
        let (mut app, _directory) = synthetic_context_launcher();
        app.login_health.insert(
            Platform::Bili,
            login::LoginHealth {
                platform: Platform::Bili,
                state: login::HealthState::Valid,
                reason: "合成有效".into(),
                checked_at: 1_000,
                verified_at: 1_000,
            },
        );
        app.event_sender
            .send(Event::Health {
                generation: app.health_generation,
                platform: Platform::Bili,
                stamp: None,
                result: Ok(login::LoginHealth {
                    platform: Platform::Bili,
                    state: login::HealthState::NetworkUnknown,
                    reason: "合成网络".into(),
                    checked_at: 2_000,
                    verified_at: 0,
                }),
            })
            .unwrap();
        app.poll_events();
        let stored = app.login_health.get(&Platform::Bili).unwrap();
        assert_eq!(stored.state, login::HealthState::NetworkUnknown);
        assert_eq!(
            stored.verified_at, 1_000,
            "a network blip must not erase the last confirmed-valid time"
        );
        assert!(
            app.login_message.contains("上次明确有效"),
            "unexpected message: {}",
            app.login_message
        );
    }

    #[test]
    fn a_health_result_for_replaced_credentials_is_discarded() {
        let (mut app, _directory) = synthetic_context_launcher();
        app.login_health.insert(
            Platform::Bili,
            login::LoginHealth {
                platform: Platform::Bili,
                state: login::HealthState::Valid,
                reason: "合成有效".into(),
                checked_at: 1_000,
                verified_at: 1_000,
            },
        );
        app.event_sender
            .send(Event::Health {
                generation: app.health_generation,
                platform: Platform::Bili,
                stamp: Some("synthetic-older-credential".into()),
                result: Ok(login::LoginHealth {
                    platform: Platform::Bili,
                    state: login::HealthState::Valid,
                    reason: "合成旧结果".into(),
                    checked_at: 2_000,
                    verified_at: 2_000,
                }),
            })
            .unwrap();
        app.poll_events();
        assert!(
            app.login_health.is_empty(),
            "a result describing an older credential must not revive a status"
        );
        assert!(
            app.login_message.contains("已丢弃旧检查结果"),
            "unexpected message: {}",
            app.login_message
        );
    }

    #[test]
    fn a_cached_search_result_avoids_a_new_request_and_shows_the_original_time() {
        let (mut app, _directory) = synthetic_context_launcher();
        let query = crate::search::SearchQuery::new("合成主播", "", "", "");
        let now = health_now();
        app.search_cache.insert(
            query.cache_key(&Platform::Bili.to_string()),
            vec![platforms::LookupCandidate {
                name: "合成主播".into(),
                room: "123".into(),
                platform: Platform::Bili,
                ..Default::default()
            }],
            now,
        );
        app.search_input = "合成主播".into();
        app.search_all = false;
        app.searching = true;
        let context = egui::Context::default();
        app.start_search(&context, false);
        assert!(
            !app.searching,
            "a cache hit must not leave the spinner running"
        );
        assert_eq!(app.search_results.len(), 1);
        assert!(app.search_caption.contains("缓存（原检索时间"));
        assert!(app.status.contains("近期搜索缓存"));
    }

    #[test]
    fn regression_cached_search_clears_previous_verification() {
        let (mut app, _directory) = synthetic_context_launcher();
        app.search_input = "synthetic cached".into();
        app.search_all = false;
        app.platform = Platform::Bili;
        let query = crate::search::SearchQuery::new(&app.search_input, "", "", "");
        app.search_cache.insert(query.cache_key("bili"), vec![platforms::LookupCandidate {
            platform: Platform::Bili, room: "123".into(), name: "synthetic".into(), ..Default::default()
        }], health_now());
        app.verification_pending = true;
        let old_id = app.search_generation;
        app.start_search(&egui::Context::default(), false);
        assert!(!app.searching && !app.verification_pending);
        app.event_sender.send(Event::Verification { id: old_id, selected: Default::default(), result: Err("stale".into()) }).unwrap();
        app.poll_events();
        assert!(!app.verification_pending);
        assert_eq!(app.search_results[0].room, "123");
    }

    #[test]
    fn a_failed_search_is_not_cached_and_suggests_a_recovery_step() {
        let (mut app, _directory) = synthetic_context_launcher();
        app.search_generation = 3;
        app.searching = true;
        let query = crate::search::SearchQuery::new("合成主播", "", "", "");
        let key = query.cache_key(&Platform::Bili.to_string());
        app.event_sender
            .send(Event::Search {
                id: 3,
                platform: Platform::Bili,
                query,
                key,
                result: Err("平台限流，合成失败".into()),
            })
            .unwrap();
        app.poll_events();
        assert!(!app.searching);
        assert!(app.search_results.is_empty());
        assert!(app.search_cache.is_empty(), "failures are never cached");
        assert!(
            app.status.contains("限流"),
            "unexpected status: {}",
            app.status
        );
    }

    #[test]
    fn launcher_initializes_second_round_layout_fields_from_saved_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(directory.path().join("synthetic-ui")).unwrap();
        store
            .save_overlay_settings(OverlaySettings {
                behavior: DanmakuBehavior {
                    show_reverse: false,
                    top_fixed_zone_percent: 33,
                    max_fixed_per_mode: 4,
                    smooth_admission: false,
                    queue_wait_ms: 350,
                    queue_capacity: 17,
                    shrink_when_crowded: false,
                    crowded_scale_percent: 72,
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap();
        let context = egui::Context::default();
        let app = Launcher::from_context(&context, store);
        let behavior = &app.overlay_draft.behavior;
        assert!(!behavior.show_reverse);
        assert_eq!(behavior.top_fixed_zone_percent, 33);
        assert_eq!(behavior.max_fixed_per_mode, 4);
        assert!(!behavior.smooth_admission);
        assert_eq!(behavior.queue_wait_ms, 350);
        assert_eq!(behavior.queue_capacity, 17);
        assert!(!behavior.shrink_when_crowded);
        assert_eq!(behavior.crowded_scale_percent, 72);
        assert_eq!(app.overlay_draft, app.snapshot.settings.overlay);
    }

    #[test]
    fn badge_results_keep_pending_bookkeeping_and_never_guess_unknown_rooms() {
        let (mut app, _directory) = synthetic_context_launcher();
        let target = Target::new(Platform::Huya, "196645").unwrap();
        let key = badges::room_key(&target);
        app.badge_pending.insert(key.clone());
        app.apply_badge_result(key.clone(), Ok(crate::room_state::RoomState::Replay));
        assert!(!app.badge_pending.contains(&key));
        assert_eq!(app.badges.get(&key), Some(&badges::Badge::Replay));
        assert_eq!(app.badge_label(&target), Some("[重播] "));
        let other = Target::new(Platform::Bili, "1").unwrap();
        let other_key = badges::room_key(&other);
        app.apply_badge_result(other_key.clone(), Err("synthetic failure".into()));
        assert_eq!(app.badges.get(&other_key), Some(&badges::Badge::Unknown));
        assert!(app.badge_label(&other).is_none());
        assert!(
            app.badge_times.contains_key(&other_key),
            "an unknown result must get the short retry timestamp"
        );
    }

    #[test]
    fn a_session_snapshot_records_room_and_playback_observations() {
        let (mut app, _directory) = synthetic_context_launcher();
        let now = chrono::Utc::now();
        let snapshot = SessionSnapshot {
            id: "synthetic-session".into(),
            target: Target::new(Platform::Huya, "196645").unwrap(),
            player: Player::Mpv,
            title: "合成主播".into(),
            player_pid: None,
            phase: SessionPhase::Playing,
            error: None,
            started_at: now,
            updated_at: now,
            ended_at: None,
            room_state: crate::sessions::SessionRoomState::Live,
            room_note: String::new(),
            room_observed_at: Some(now),
            relay: None,
        };
        app.record_observations_from_snapshot(&snapshot);
        let rows = app.observations.rows(Platform::Huya, health_now());
        assert!(rows[0].result.contains("正在直播"));
        assert!(rows[1].result.contains("播放中"));
        assert_eq!(rows[0].room, "196645");
        assert_eq!(rows[2].result, "尚无记录");
    }

    fn synthetic_search_candidate() -> platforms::LookupCandidate {
        platforms::LookupCandidate {
            name: "合成主播".into(),
            room: "123".into(),
            platform: Platform::Douyin,
            ..Default::default()
        }
    }

    #[test]
    fn a_successful_douyin_web_assist_requests_one_search_refresh() {
        let (mut app, _directory) = synthetic_context_launcher();
        app.page = Page::Lookup;
        app.platform = Platform::Douyin;
        app.search_input = "合成主播".into();
        app.web_assist_pending = true;
        app.login_generation = 7;
        app.search_cache.insert(
            "douyin\n合成主播".into(),
            vec![synthetic_search_candidate()],
            health_now(),
        );
        app.event_sender
            .send(Event::Login {
                id: 7,
                platform: Platform::Douyin,
                update: LoginUpdate::Done {
                    message: "网页登录已验证，凭据已加密保存。".into(),
                    error: false,
                },
            })
            .unwrap();
        app.poll_events();
        assert!(
            app.search_refresh_after_login,
            "the search page must re-run the current keyword exactly once"
        );
        assert!(!app.web_assist_pending);
        assert!(
            app.search_cache.is_empty(),
            "candidates captured before the new session must not survive it"
        );
        assert!(app.login_message.contains("重新查询"));
    }

    #[test]
    fn a_failed_or_unrelated_login_never_triggers_the_assist_refresh() {
        let (mut app, _directory) = synthetic_context_launcher();
        app.page = Page::Lookup;
        app.platform = Platform::Douyin;
        app.login_generation = 2;
        app.web_assist_pending = true;
        app.event_sender
            .send(Event::Login {
                id: 2,
                platform: Platform::Douyin,
                update: LoginUpdate::Done {
                    message: "登录未完成：合成失败".into(),
                    error: true,
                },
            })
            .unwrap();
        app.poll_events();
        assert!(!app.search_refresh_after_login);
        assert!(!app.web_assist_pending);
        // A successful ordinary page login (not started from the assist card) does
        // not silently re-run a search.
        app.login_generation = 3;
        app.event_sender
            .send(Event::Login {
                id: 3,
                platform: Platform::Douyin,
                update: LoginUpdate::Done {
                    message: "网页登录已验证，凭据已加密保存。".into(),
                    error: false,
                },
            })
            .unwrap();
        app.poll_events();
        assert!(!app.search_refresh_after_login);
    }

    #[test]
    fn clearing_douyin_assist_profiles_only_touches_its_own_directories() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("synthetic-ui")).unwrap();
        let data_root = store.root().to_owned();
        let browser_root = data_root.join("browser-login");
        let douyin = browser_root.join("douyin-0123456789abcdef0123456789abcdef");
        let huya = browser_root.join("huya-0123456789abcdef0123456789abcdef");
        std::fs::create_dir_all(&douyin).unwrap();
        std::fs::create_dir_all(&huya).unwrap();
        std::fs::write(douyin.join("Preferences"), "synthetic").unwrap();
        let context = egui::Context::default();
        let mut app = Launcher::from_context(&context, store);
        app.send_store(StoreAction::ClearBrowserProfiles(Platform::Douyin));
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while app.storage_pending > 0 && std::time::Instant::now() < deadline {
            app.poll_events();
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(app.storage_pending, 0);
        assert!(!douyin.exists(), "the Douyin assist profile is removed");
        assert!(huya.exists(), "another platform's profile is untouched");
        assert!(
            app.status.contains("清理") && app.status.contains("1 个"),
            "unexpected status: {}",
            app.status
        );
    }

    #[test]
    fn page_keys_match_the_original_eight_page_navigation() {
        // The sidebar order is the original navigation order; the old Rust-only
        // pages (开始看直播 / 设置与凭据 / 关于) are folded into these eight.
        let keys: Vec<&str> = Page::ALL.iter().map(|page| page.key()).collect();
        assert_eq!(
            keys,
            [
                "favorites",
                "recent",
                "lookup",
                "sessions",
                "players",
                "danmaku",
                "accounts",
                "data"
            ]
        );
        for page in Page::ALL {
            assert_eq!(Page::from_key(page.key()), Some(page));
            assert!(!page.title().is_empty());
            assert!(!page.subtitle().is_empty());
            assert!(!page.idle_hint().is_empty());
        }
        for removed in ["play", "history", "settings", "about", "search"] {
            assert_eq!(Page::from_key(removed), None, "{removed} must not page");
        }
        assert_eq!(Page::from_key(" FAVORITES "), None, "keys are exact literals");
    }

    #[test]
    fn ui_shot_page_selection_is_keyed_and_rejects_unknown_pages() {
        let (mut app, _directory) = synthetic_context_launcher();
        assert!(app.shot_select_page("data"));
        assert_eq!(app.page, Page::Data);
        assert!(app.shot_select_page("favorites"));
        assert_eq!(app.page, Page::Favorites);
        assert!(!app.shot_select_page("nope"));
        assert_eq!(app.page, Page::Favorites, "an unknown key must not page");
    }

    #[test]
    fn launch_overrides_only_touch_this_playback() {
        let (mut app, _directory) = synthetic_context_launcher();
        app.snapshot.settings.playback.buffer_mode = BufferMode::LowLatency;
        app.snapshot.settings.overlay.enabled = true;
        app.playback_draft.buffer_mode = BufferMode::Stable;
        app.overlay_draft.sc_mode = ScMode::Off;
        app.launch_danmaku = 2;
        let settings = app.launch_settings();
        assert_eq!(settings.playback.buffer_mode, BufferMode::Stable);
        assert!(!settings.overlay.enabled, "本次播放 can disable danmaku");
        assert_eq!(settings.overlay.sc_mode, ScMode::Off);
        // The saved snapshot is never mutated by the per-play overrides.
        assert_eq!(
            app.snapshot.settings.playback.buffer_mode,
            BufferMode::LowLatency
        );
        assert!(app.snapshot.settings.overlay.enabled);
        // "跟随设置" keeps the saved value, and the force-on override wins.
        app.launch_danmaku = 0;
        assert!(app.launch_settings().overlay.enabled);
        app.snapshot.settings.overlay.enabled = false;
        app.launch_danmaku = 1;
        assert!(app.launch_settings().overlay.enabled);
    }

    /// Drive the production workbench dropdown inside headless egui frames: a
    /// click opens the popup, the popup must survive following frames (the old
    /// Memory::toggle_popup implementation was cleared by egui every frame), a
    /// row click selects and closes it, and an outside click also closes it.
    #[test]
    fn workbench_dropdown_persists_opens_selects_and_closes() {
        use crate::gui::theme;
        use crate::gui::workbench::{Btn, Pal, dropdown};
        let context = egui::Context::default();
        theme::install_preview_fonts(&context);
        theme::apply(&context, true);
        let pal = Pal::of(theme::ThemeMode::Dark);
        let rect = egui::Rect::from_min_size(egui::pos2(40.0, 40.0), egui::vec2(140.0, 28.0));
        let items = ["甲", "乙", "丙"];
        let mut selected = 0usize;
        let mut popup_id: Option<egui::Id> = None;
        let mut popup_pos: Option<egui::Pos2> = None;
        let mut response_changed = false;
        let last_clicked = std::cell::Cell::new(false);
        let last_open_inside = std::cell::Cell::new(false);
        let trace = std::cell::RefCell::new(Vec::<String>::new());
        let mut frame = |time: f64, events: Vec<egui::Event>, selected: &mut usize, popup_id: &mut Option<egui::Id>, popup_pos: &mut Option<egui::Pos2>, changed: &mut bool| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                time: Some(time),
                events,
                ..Default::default()
            };
            let _ = context.run(input, |ctx| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ctx, |ui| {
                        let response = dropdown(ui, rect, "test-dropdown", selected, &items, &pal);
                        if response.changed() {
                            *changed = true;
                        }
                        let id = egui::Popup::default_response_id(&response);
                        *popup_id = Some(id);
                        *popup_pos = egui::Popup::position_of_id(ctx, id);
                        last_clicked.set(response.clicked());
                        last_open_inside.set(egui::Popup::is_id_open(ctx, id));
                        trace.borrow_mut().push(format!(
                            "clicked={} open={} any_click={} hovered={}",
                            response.clicked(),
                            egui::Popup::is_id_open(ctx, id),
                            ctx.input(|i| i.pointer.any_click()),
                            response.hovered()
                        ));
                    });
            });
        };
        let press = |pos: egui::Pos2| {
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
            ]
        };
        let release = |pos: egui::Pos2| {
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }]
        };
        // Frame 0: build the widget so it exists before any pointer input.
        frame(0.0, Vec::new(), &mut selected, &mut popup_id, &mut popup_pos, &mut response_changed);
        let id = popup_id.expect("dropdown must register a popup id");
        assert!(!egui::Popup::is_id_open(&context, id));
        // Frames 1-2: press then release on the closed dropdown opens it.
        frame(0.1, press(rect.center()), &mut selected, &mut popup_id, &mut popup_pos, &mut response_changed);
        frame(0.2, release(rect.center()), &mut selected, &mut popup_id, &mut popup_pos, &mut response_changed);
        assert!(
            last_clicked.get(),
            "the press/release sequence must register as a click"
        );
        assert!(
            egui::Popup::is_id_open(&context, id),
            "a click must open the dropdown popup; trace={:?}",
            trace.borrow()
        );
        // Frame 3: no input at all. The popup must still be open (regression:
        // the previous implementation closed itself after one frame).
        frame(0.3, Vec::new(), &mut selected, &mut popup_id, &mut popup_pos, &mut response_changed);
        assert!(
            egui::Popup::is_id_open(&context, id),
            "the popup must survive frames without input"
        );
        // Frames 4-5: press then release on the second row selects it and closes.
        // The popup is anchored to the trigger's bottom-left with a 2px gap and
        // a 6px menu margin, then 24px rows with 2px spacing.
        let margin = f32::from(context.style().spacing.menu_margin.top);
        let row_center = egui::pos2(
            rect.center().x,
            rect.bottom() + 2.0 + margin + 24.0 + 2.0 + 12.0,
        );
        frame(0.4, press(row_center), &mut selected, &mut popup_id, &mut popup_pos, &mut response_changed);
        frame(0.5, release(row_center), &mut selected, &mut popup_id, &mut popup_pos, &mut response_changed);
        assert_eq!(selected, 1, "clicking a row must select that item");
        assert!(response_changed, "the selection change must be reported");
        assert!(
            !egui::Popup::is_id_open(&context, id),
            "selecting a row must close the popup"
        );
        // Frames 6-7: reopen with the button itself.
        frame(0.6, press(rect.center()), &mut selected, &mut popup_id, &mut popup_pos, &mut response_changed);
        frame(0.7, release(rect.center()), &mut selected, &mut popup_id, &mut popup_pos, &mut response_changed);
        assert!(egui::Popup::is_id_open(&context, id), "second click reopens");
        // Frames 8-9: click far outside closes it without changing the value.
        let outside = egui::pos2(700.0, 550.0);
        frame(0.8, press(outside), &mut selected, &mut popup_id, &mut popup_pos, &mut response_changed);
        frame(0.9, release(outside), &mut selected, &mut popup_id, &mut popup_pos, &mut response_changed);
        assert!(
            !egui::Popup::is_id_open(&context, id),
            "an outside click must close the dropdown"
        );
        assert_eq!(selected, 1, "outside clicks must not change the value");
        let _ = Btn::Secondary;
    }
}
