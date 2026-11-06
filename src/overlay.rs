//! A separate, mouse-transparent native overlay for the danmaku receiver.
//! Window following uses only this process's own overlay window and read-only
//! player queries; it never changes the foreground window or sends input.
//!
//! The rendering policy below is split into pure functions so visibility,
//! DPI conversion, card lifetime and font clamping can be tested offline
//! without creating a window or contacting a live platform.

use crate::auth::CookieVault;
use crate::chat_pipeline::{ChatPipeline, PendingMessage};
use crate::core::{EmoteSizeMode, OverlaySettings, PercentRect, ScMode, Target, TextBorderStyle};
use crate::danmaku::{
    self, Cancellation, DanmakuEvent, LaneScheduler, LiveEvent, LiveEventKind, SuperChat,
};
use anyhow::{Context, Result, anyhow, ensure};
use eframe::egui::{self, Color32, FontId, Pos2, Rect, Stroke, StrokeKind, Vec2};
use std::sync::{
    Arc, Mutex,
    mpsc::{Receiver, SyncSender},
};
use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};

// ---- display policy constants (pure, offline-testable) --------------------

pub const MIN_FONT_SIZE: f32 = 12.0;
pub const MAX_FONT_SIZE: f32 = 72.0;
pub const DEFAULT_FONT_SIZE: f32 = 25.0;
pub const FONT_STEP: f32 = 2.0;
const MIN_DPI: f32 = 0.5;
const MAX_DPI: f32 = 4.0;
const LINE_HEIGHT_FACTOR: f32 = 1.35;
/// Hard cap for simultaneously animating danmaku; bursts are dropped, never queued forever.
/// The effective cap comes from the user setting (MaxOnScreen, 10-600 in the
/// legacy renderer); this is only the absolute ceiling.
pub const MAX_ACTIVE_DANMAKU: usize = 600;
const MAX_PENDING_DANMAKU: usize = 128;
const MAX_DANMAKU_PER_FRAME: usize = 32;
/// Hard cap of retained paid-message cards. The legacy renderer kept up to 200
/// cards and scrolled the overflow instead of dropping cards.
pub const MAX_SC_CARDS: usize = 200;
/// Legacy renderer geometry (B站 full-screen SC widget): card width 270 px,
/// margin 10 px, card gap 4 px, at 100% scale.
pub const SC_CARD_BASE_WIDTH: f32 = 270.0;
pub const SC_CARD_MARGIN: f32 = 10.0;
pub const SC_CARD_GAP: f32 = 4.0;
pub const SC_TEXT_MAX_CHARS: usize = 240;
pub const SC_USER_MAX_CHARS: usize = 24;
const SC_ID_MAX_CHARS: usize = 64;
/// Upper bound for the user scale; also the hard bound used by the settings UI.
pub const SC_SCALE_MIN_PERCENT: u16 = 60;
pub const SC_SCALE_MAX_PERCENT: u16 = 200;
/// Legacy renderer kept the newest message's first 12 characters in the badge.
pub const SC_BADGE_PREVIEW_CHARS: usize = 12;
const STATUS_VISIBLE_SECS: f32 = 5.0;
const FEEDBACK_VISIBLE_SECS: f32 = 2.5;
/// A terminal connection error is shown this long, then the overlay closes.
pub const FATAL_DISPLAY_SECS: f32 = 5.0;
/// Safety net: the follow thread always exits this long after a fatal error,
/// even if the hidden overlay never received a repaint to process it.
const FOLLOW_FATAL_EXIT_SECS: f32 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientArea {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

/// Pure policy, tested without creating or manipulating a real window.
/// The overlay only follows a live player that owns the foreground; a minimized
/// player or a degenerate client area keeps it hidden. Coordinates are virtual
/// screen coordinates, so negative origins (a monitor left of the primary) work.
pub fn should_show(
    process_alive: bool,
    window_exists: bool,
    minimized: bool,
    foreground_player: bool,
    area: Option<ClientArea>,
) -> bool {
    process_alive
        && window_exists
        && !minimized
        && foreground_player
        && area.is_some_and(|a| a.width >= 16 && a.height >= 16)
}

/// Live overlay settings shared between the egui app (hot reload / hotkeys)
/// and the native follow thread (window policy + global hotkey registration).
#[derive(Debug, Clone)]
pub struct LiveOptions {
    pub version: u64,
    pub options: OverlaySettings,
}
pub type SharedOptions = Arc<Mutex<LiveOptions>>;

pub fn shared_options(options: OverlaySettings) -> SharedOptions {
    Arc::new(Mutex::new(LiveOptions {
        version: 0,
        options,
    }))
}

/// Isolated data directory from the global --data-dir argument.
pub fn data_dir_from_args() -> Option<std::path::PathBuf> {
    std::env::args()
        .skip(1)
        .collect::<Vec<_>>()
        .windows(2)
        .find(|pair| pair[0] == "--data-dir")
        .map(|pair| std::path::PathBuf::from(&pair[1]))
}

/// Initialises the bounded, redacted diagnostic log for this overlay process
/// (<data>/logs/overlay.log). No-op when the caller did not pass --data-dir.
fn init_overlay_diag(dir: Option<&std::path::Path>) {
    if let Some(dir) = dir {
        crate::diag::init(dir.join("logs"));
    }
}

fn overlay_log(level: crate::diag::Level, message: &str) {
    crate::diag::log("overlay", level, message);
}

// ---- font, DPI and hint policy -------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplaySettings {
    /// Danmaku glyph height in physical pixels, independent of Windows scaling.
    pub font_size: f32,
    /// F9: show the scrolling danmaku layer.
    pub danmaku_visible: bool,
    /// F10: show the paid-message (SC) card stack.
    pub super_chat_visible: bool,
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self {
            font_size: DEFAULT_FONT_SIZE,
            danmaku_visible: true,
            super_chat_visible: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayHotkey {
    FontSmaller,
    FontLarger,
    ToggleDanmaku,
    ToggleSuperChat,
    /// Configurable global actions (legacy 11 Hotkey* fields).
    CycleSuperChat,
    ToggleOverlay,
    AreaLarger,
    AreaSmaller,
    SpeedFaster,
    SpeedSlower,
    DelayLess,
    DelayMore,
}

/// Maps one configurable legacy hotkey action onto the overlay event channel.
pub fn overlay_hotkey_for(action: crate::hotkeys::HotkeyAction) -> OverlayHotkey {
    use crate::hotkeys::HotkeyAction as A;
    match action {
        A::ToggleDanmaku => OverlayHotkey::ToggleDanmaku,
        A::CycleSc => OverlayHotkey::CycleSuperChat,
        A::ToggleOverlay => OverlayHotkey::ToggleOverlay,
        A::FontUp => OverlayHotkey::FontLarger,
        A::FontDown => OverlayHotkey::FontSmaller,
        A::AreaUp => OverlayHotkey::AreaLarger,
        A::AreaDown => OverlayHotkey::AreaSmaller,
        A::SpeedUp => OverlayHotkey::SpeedFaster,
        A::SpeedDown => OverlayHotkey::SpeedSlower,
        A::DelayLess => OverlayHotkey::DelayLess,
        A::DelayMore => OverlayHotkey::DelayMore,
    }
}

/// Maps one fixed F7-F10 action (registered by the overlay while the player is
/// foreground) onto the same overlay event channel as the configurable ones.
pub fn fixed_overlay_hotkey_for(action: crate::hotkeys::FixedOverlayAction) -> OverlayHotkey {
    use crate::hotkeys::FixedOverlayAction as F;
    match action {
        F::FontSmaller => OverlayHotkey::FontSmaller,
        F::FontLarger => OverlayHotkey::FontLarger,
        F::ToggleDanmaku => OverlayHotkey::ToggleDanmaku,
        F::ToggleSuperChat => OverlayHotkey::ToggleSuperChat,
    }
}

/// Win32 can leave a WM_HOTKEY queued after its registration is released. Gate
/// both ranges at delivery time so a focus change or disabling hotkeys cannot
/// apply a stale F7-F10 event to the previous session.
pub fn active_overlay_hotkey(
    event: crate::hotkeys::HotkeyEvent,
    active: bool,
) -> Option<OverlayHotkey> {
    if !active {
        return None;
    }
    Some(match event {
        crate::hotkeys::HotkeyEvent::Action(action) => overlay_hotkey_for(action),
        crate::hotkeys::HotkeyEvent::Fixed(action) => fixed_overlay_hotkey_for(action),
    })
}

/// Display-area hotkey steps (legacy AreaSteps: full / 3/4 / half / quarter).
pub const DISPLAY_AREA_STEPS: [u8; 4] = [100, 75, 50, 25];
/// Speed hotkey steps (legacy SpeedSteps + labels).
pub const SPEED_STEPS: [u16; 5] = [60, 80, 100, 130, 170];
/// Delay hotkey step in milliseconds (legacy StepDelay).
pub const DELAY_STEP_MS: u16 = 500;

/// Native hotkeys are registered only while this overlay's player is foreground.
pub fn hotkey_for(key: egui::Key) -> Option<OverlayHotkey> {
    match key {
        egui::Key::F7 => Some(OverlayHotkey::FontSmaller),
        egui::Key::F8 => Some(OverlayHotkey::FontLarger),
        egui::Key::F9 => Some(OverlayHotkey::ToggleDanmaku),
        egui::Key::F10 => Some(OverlayHotkey::ToggleSuperChat),
        _ => None,
    }
}

impl DisplaySettings {
    /// Apply one shortcut at runtime (settings are intentionally not persisted).
    pub fn apply(&mut self, hotkey: OverlayHotkey) -> String {
        match hotkey {
            OverlayHotkey::FontSmaller => {
                self.adjust_font(-1.0);
                format!("字号 {} px", self.font_size.round() as i32)
            }
            OverlayHotkey::FontLarger => {
                self.adjust_font(1.0);
                format!("字号 {} px", self.font_size.round() as i32)
            }
            OverlayHotkey::ToggleDanmaku => {
                self.danmaku_visible = !self.danmaku_visible;
                if self.danmaku_visible {
                    "弹幕已显示".into()
                } else {
                    "弹幕已隐藏（F9 恢复）".into()
                }
            }
            OverlayHotkey::ToggleSuperChat => {
                self.super_chat_visible = !self.super_chat_visible;
                if self.super_chat_visible {
                    "SC 卡片已显示".into()
                } else {
                    "SC 卡片已隐藏（F10 恢复）".into()
                }
            }
            // The remaining actions need the overlay settings/pipeline and are
            // handled by Overlay::apply_extended_hotkey.
            _ => String::new(),
        }
    }
    pub fn adjust_font(&mut self, steps: f32) -> f32 {
        self.font_size = step_font_size(self.font_size, steps);
        self.font_size
    }
}

pub const ENV_FONT_PX: &str = "LIVE_HUB_OVERLAY_FONT_PX";
pub const ENV_DANMAKU: &str = "LIVE_HUB_OVERLAY_DANMAKU";
pub const ENV_SUPER_CHAT: &str = "LIVE_HUB_OVERLAY_SC";

/// Diagnostic acceptance switches. Both are opt-in, default-off environment
/// variables used only by the local real-window acceptance harness; a product
/// launch never sets them. The fixture path replaces the platform receiver with
/// a bounded, synthetic event list (no network), and the force-show switch
/// relaxes only the foreground-player requirement so a window can be inspected
/// without stealing the user's foreground focus. Minimize, hidden-window,
/// process-exit and client-area gates keep their production behaviour.
pub const ENV_FIXTURE: &str = "LIVE_HUB_OVERLAY_FIXTURE";
pub const ENV_DIAG_FORCE_SHOW: &str = "LIVE_HUB_OVERLAY_DIAG_FORCE_SHOW";
/// Diagnostic stderr trace for acceptance runs; off by default.
pub const ENV_DIAG_LOG: &str = "LIVE_HUB_OVERLAY_DIAG_LOG";
/// Diagnostic freeze: after N milliseconds the overlay paints one static frame
/// (time stops advancing). Only for offline screenshot evidence: PrintWindow on
/// an off-screen layered/GL window can otherwise blend consecutive frames and
/// show fake motion trails. Never set by a product launch.
pub const ENV_DIAG_FREEZE_MS: &str = "LIVE_HUB_OVERLAY_DIAG_FREEZE_MS";

fn diag_log(message: &str) {
    if std::env::var_os(ENV_DIAG_LOG).is_some() {
        eprintln!("[overlay-diag] {message}");
    }
}

/// Optional launch-time overrides. The window is NOACTIVATE, so the F7-F10
/// shortcuts are only reachable when it holds keyboard focus; these variables
/// make the same display settings reachable without touching the player.
pub fn settings_from_env(get: impl Fn(&str) -> Option<String>) -> DisplaySettings {
    let mut settings = DisplaySettings::default();
    if let Some(value) = get(ENV_FONT_PX) {
        if let Ok(parsed) = value.trim().parse::<f32>() {
            settings.font_size = clamp_font_size(parsed);
        }
    }
    if let Some(value) = get(ENV_DANMAKU) {
        settings.danmaku_visible = parse_visible(&value, settings.danmaku_visible);
    }
    if let Some(value) = get(ENV_SUPER_CHAT) {
        settings.super_chat_visible = parse_visible(&value, settings.super_chat_visible);
    }
    settings
}

fn parse_visible(value: &str, fallback: bool) -> bool {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "on" | "true" | "show" => true,
        "0" | "off" | "false" | "hide" => false,
        _ => fallback,
    }
}

/// Diagnostic-only force-show switch; with it unset the foreground gate is
/// unchanged. Parsed through a caller-supplied getter so the policy is testable
/// without mutating the process environment.
pub fn diag_force_show_from(get: impl Fn(&str) -> Option<String>) -> bool {
    get(ENV_DIAG_FORCE_SHOW).is_some_and(|value| value.trim() == "1")
}

/// Parses the opt-in freeze switch; invalid or zero values disable it.
pub fn diag_freeze_ms_from(get: impl Fn(&str) -> Option<String>) -> Option<f64> {
    get(ENV_DIAG_FREEZE_MS)
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|millis| (100..=600_000).contains(millis))
        .map(|millis| millis as f64)
}

// ---- diagnostic acceptance fixture (no network) ---------------------------

const MAX_FIXTURE_BYTES: u64 = 256 * 1024;
const MAX_FIXTURE_EVENTS: usize = 256;
const FIXTURE_INTERVAL_MS: u64 = 320;
const FIXTURE_MAX_DELAY_MS: u64 = 120_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixturePlan {
    pub initial_delay_ms: u64,
    pub interval_ms: u64,
    pub events: Vec<DanmakuEvent>,
}

#[derive(serde::Deserialize)]
struct FixtureFile {
    #[serde(default)]
    initial_delay_ms: u64,
    #[serde(default)]
    interval_ms: Option<u64>,
    events: Vec<DanmakuEvent>,
}

/// Parses the diagnostic fixture. It stays local and bounded: a strict size and
/// event cap, clamped timing and no partial playback on malformed input.
pub fn parse_fixture(text: &str) -> Result<FixturePlan> {
    ensure!(
        text.len() as u64 <= MAX_FIXTURE_BYTES,
        "诊断夹具超过 256 KiB"
    );
    let file: FixtureFile = serde_json::from_str(text).context("诊断夹具不是合法 JSON")?;
    ensure!(!file.events.is_empty(), "诊断夹具没有事件");
    ensure!(
        file.events.len() <= MAX_FIXTURE_EVENTS,
        "诊断夹具事件超过 256 条"
    );
    Ok(FixturePlan {
        initial_delay_ms: file.initial_delay_ms.min(FIXTURE_MAX_DELAY_MS),
        interval_ms: file
            .interval_ms
            .unwrap_or(FIXTURE_INTERVAL_MS)
            .clamp(1, 10_000),
        events: file.events,
    })
}

/// Sleeps in small slices so cancellation shuts the fixture thread down promptly.
fn fixture_sleep(cancellation: &Cancellation, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        if cancellation.is_cancelled() {
            return false;
        }
        thread::sleep(remaining.min(Duration::from_millis(25)));
    }
    !cancellation.is_cancelled()
}

/// Replaces the platform receiver for acceptance runs only. Events flow through
/// the same bounded channel the real decoders use.
fn run_fixture(
    path: &Path,
    sender: crate::danmaku::EventSender,
    cancellation: Cancellation,
) -> Result<()> {
    let metadata = std::fs::metadata(path).context("无法读取覆盖层诊断夹具")?;
    ensure!(metadata.is_file(), "覆盖层诊断夹具不是普通文件");
    ensure!(metadata.len() <= MAX_FIXTURE_BYTES, "诊断夹具超过 256 KiB");
    let text = std::fs::read_to_string(path).context("覆盖层诊断夹具不是 UTF-8 文本")?;
    let plan = parse_fixture(&text)?;
    if !fixture_sleep(&cancellation, Duration::from_millis(plan.initial_delay_ms)) {
        return Ok(());
    }
    for event in plan.events {
        if !matches!(danmaku::emit(&sender, event), danmaku::Delivery::Sent) {
            return Ok(());
        }
        if !fixture_sleep(&cancellation, Duration::from_millis(plan.interval_ms)) {
            return Ok(());
        }
    }
    Ok(())
}

/// One-line disposable hint rendered in the overlay corner.
pub fn hint_text(settings: &DisplaySettings) -> String {
    let danmaku = if settings.danmaku_visible {
        "弹幕开"
    } else {
        "弹幕关"
    };
    let super_chat = if settings.super_chat_visible {
        "SC开"
    } else {
        "SC关"
    };
    format!(
        "F7/F8 字号 {}px · F9 {danmaku} · F10 {super_chat}",
        settings.font_size.round() as i32
    )
}

pub fn clamp_font_size(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
    } else {
        DEFAULT_FONT_SIZE
    }
}

pub fn step_font_size(current: f32, steps: f32) -> f32 {
    let base = clamp_font_size(current);
    if !steps.is_finite() {
        return base;
    }
    clamp_font_size(base + steps * FONT_STEP)
}

/// Windows display scaling, clamped so a bogus value can never explode layout math.
pub fn dpi_scale(pixels_per_point: f32) -> f32 {
    if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
        pixels_per_point.clamp(MIN_DPI, MAX_DPI)
    } else {
        1.0
    }
}

/// The configured font size is a physical-pixel height, so the danmaku keeps the
/// same on-screen size next to the video regardless of Windows scaling. egui
/// renders in points, therefore divide by the display scale.
pub fn font_points(font_size: f32, pixels_per_point: f32) -> f32 {
    (clamp_font_size(font_size) / dpi_scale(pixels_per_point)).clamp(8.0, 96.0)
}

pub fn line_height_points(font_size: f32, pixels_per_point: f32) -> f32 {
    font_points(font_size, pixels_per_point) * LINE_HEIGHT_FACTOR
}

/// Cross-screen duration for the current speed setting, in seconds. The legacy
/// renderer crosses the picture in a fixed duration, not at a fixed pixel
/// speed, so long messages move faster and every message stays on screen for
/// the same amount of time.
pub fn danmaku_duration_secs(speed_percent: u16) -> f64 {
    crate::danmaku::crossing_duration_secs(speed_percent)
}

/// Vertical band used by scrolling danmaku: the configured picture-height
/// percentage, minus the anti-block-subtitle strip. Mirrors the legacy
/// areaHeight rule (only applied when the display area is larger than 40%).
pub fn danmaku_area_points(
    height_points: f32,
    display_area_percent: u8,
    bottom_safe_percent: u8,
) -> f32 {
    if !height_points.is_finite() || height_points <= 0.0 {
        return 0.0;
    }
    let display = f32::from(display_area_percent.clamp(10, 100)) / 100.0;
    let mut area = height_points * display;
    if display_area_percent > 40 && bottom_safe_percent > 0 {
        area *= 1.0 - f32::from(bottom_safe_percent.min(50)) / 100.0;
    }
    area.max(1.0)
}

/// Points scale of the paid-message widget. The legacy widget sizes itself in
/// physical pixels: 270*ScScale%/100*uiScale, where uiScale is the picture
/// height over the reference height (ScaleWithWindow) or the display scale.
/// Converting to egui points divides the display scale back out, so the scale
/// below is already in points.
pub fn sc_points_scale(
    scale_percent: u16,
    picture_height_points: f32,
    reference_height: u16,
    scale_with_window: bool,
) -> f32 {
    let percent =
        f32::from(scale_percent.clamp(SC_SCALE_MIN_PERCENT, SC_SCALE_MAX_PERCENT)) / 100.0;
    let window_scale = if scale_with_window && picture_height_points.is_finite() {
        (picture_height_points / f32::from(reference_height.max(180))).clamp(0.45, 4.5)
    } else {
        1.0
    };
    (percent * window_scale).clamp(0.2, 8.0)
}

/// Whether the animated danmaku layer may accept one more entry.
pub fn admit_danmaku(active: usize) -> bool {
    active < MAX_ACTIVE_DANMAKU
}

pub fn message_color(rgb: u32) -> Color32 {
    Color32::from_rgb(
        ((rgb >> 16) & 0xff) as u8,
        ((rgb >> 8) & 0xff) as u8,
        (rgb & 0xff) as u8,
    )
}

/// Prefix label for one danmaku, mirroring the legacy BuildLabel order:
/// guard tag, fan medal, user level, user name, then the message text. Every
/// part is governed by its own display setting and all text is clipped.
pub fn message_label(message: &danmaku::DanmakuMessage, options: &OverlaySettings) -> String {
    let mut label = String::new();
    if options.show_guard && message.guard_level > 0 {
        label.push_str(match message.guard_level {
            1 => "[总督] ",
            2 => "[提督] ",
            _ => "[舰长] ",
        });
    }
    if options.show_medal {
        if let Some(medal) = &message.medal {
            if !medal.name.is_empty() && medal.level > 0 {
                label.push('[');
                label.push_str(&truncate_chars(&medal.name, 24));
                label.push_str(&medal.level.to_string());
                label.push_str("] ");
            }
        }
    }
    if options.show_user_level && message.user_level > 0 {
        label.push('U');
        label.push_str(&message.user_level.to_string());
        label.push(' ');
    }
    if options.show_user_name && !message.user.is_empty() {
        label.push_str(&truncate_chars(&message.user, 32));
        label.push_str(": ");
    }
    label.push_str(&truncate_chars(&message.text, 512));
    label
}

/// Prefix parts only (guard tag, medal, user level, user name), matching the
/// legacy BuildLabel(includeText: false). Image emoticon messages draw the image
/// first and this prefix after it, exactly like the legacy renderer.
pub fn message_prefix(message: &danmaku::DanmakuMessage, options: &OverlaySettings) -> String {
    let mut label = String::new();
    if options.show_guard && message.guard_level > 0 {
        label.push_str(match message.guard_level {
            1 => "[总督] ",
            2 => "[提督] ",
            _ => "[舰长] ",
        });
    }
    if options.show_medal {
        if let Some(medal) = &message.medal {
            if !medal.name.is_empty() && medal.level > 0 {
                label.push('[');
                label.push_str(&truncate_chars(&medal.name, 24));
                label.push_str(&medal.level.to_string());
                label.push_str("] ");
            }
        }
    }
    if options.show_user_level && message.user_level > 0 {
        label.push('U');
        label.push_str(&message.user_level.to_string());
        label.push(' ');
    }
    if options.show_user_name && !message.user.is_empty() {
        label.push_str(&truncate_chars(&message.user, 32));
        label.push_str(": ");
    }
    label
}

/// Physical danmaku font size. With window-relative scaling enabled the size
/// follows the player picture height so danmaku keep the same ratio to the
/// video, exactly like the legacy ScaleWithWindow option.
pub fn effective_font_px(
    base: f32,
    client_height_px: f32,
    reference_height: u16,
    scale_with_window: bool,
) -> f32 {
    let base = clamp_font_size(base);
    if !scale_with_window || !client_height_px.is_finite() || client_height_px <= 0.0 {
        return base;
    }
    let reference = f32::from(reference_height.max(360));
    clamp_font_size(base * (client_height_px / reference).clamp(0.5, 3.0))
}

/// Packs a platform 0xRRGGBB value into a Color32 with the given alpha.
pub fn color_with_alpha(rgb: u32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(
        ((rgb >> 16) & 0xff) as u8,
        ((rgb >> 8) & 0xff) as u8,
        (rgb & 0xff) as u8,
        alpha,
    )
}

/// Multiplies a colour's alpha by the text-opacity percentage.
pub fn apply_text_opacity(color: Color32, percent: u8) -> Color32 {
    let alpha = u16::from(color.a()) * u16::from(percent.clamp(10, 100)) / 100;
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha as u8)
}

/// Offsets painted under the glyphs for the selected edge style. The legacy
/// renderer scales the pen with the glyph size (penWidth = strokeWidth *
/// max(1, fontSize*0.05)), so a fixed 2 px outline would swallow small glyphs and
/// look thin on large ones; the offsets below follow the same rule. Shadow is a
/// single offset copy with no outline, heavy doubles the pen, outline is the
/// normal pen.
pub fn stroke_offsets(style: TextBorderStyle, width: f32, font_points: f32) -> Vec<(f32, f32)> {
    let stroke = if width.is_finite() {
        width.clamp(1.0, 4.0)
    } else {
        1.0
    };
    let font = if font_points.is_finite() {
        font_points.clamp(6.0, 128.0)
    } else {
        25.0
    };
    let unit = (font * 0.05).max(1.0);
    match style {
        TextBorderStyle::Shadow => {
            let shift = unit.max(1.0);
            vec![(shift, shift)]
        }
        TextBorderStyle::Heavy => {
            let radius = (stroke * 2.0 * unit * 0.5).max(1.0);
            eight_directions(radius)
        }
        TextBorderStyle::Outline => {
            let radius = (stroke * unit * 0.5).max(1.0);
            eight_directions(radius)
        }
    }
}

fn eight_directions(radius: f32) -> Vec<(f32, f32)> {
    vec![
        (-radius, 0.0),
        (radius, 0.0),
        (0.0, -radius),
        (0.0, radius),
        (radius, radius),
        (-radius, -radius),
        (radius, -radius),
        (-radius, radius),
    ]
}

/// Painted size of one image emoticon in points. Small never exceeds the glyph
/// height, Platform follows the platform dimensions with a modest boost, and
/// Large may use more than one row.
pub fn emoticon_size(
    font_points: f32,
    mode: EmoteSizeMode,
    intrinsic_width: u16,
    intrinsic_height: u16,
) -> (f32, f32) {
    let font = if font_points.is_finite() {
        font_points.max(1.0)
    } else {
        DEFAULT_FONT_SIZE
    };
    let (height_factor, max_width_factor) = match mode {
        EmoteSizeMode::Small => (1.25, 4.0),
        EmoteSizeMode::Platform => (1.25, 4.0),
        EmoteSizeMode::Large => (1.8, 6.0),
    };
    let height = font * height_factor;
    let aspect = if intrinsic_width > 0 && intrinsic_height > 0 {
        f32::from(intrinsic_width) / f32::from(intrinsic_height)
    } else {
        1.0
    };
    let width = (height * aspect.max(0.05)).clamp(1.0, font * max_width_factor);
    (width, height)
}

/// Effective emote size for one message. Legacy semantics
/// (danmaku-3-render.cs `RefreshFlyingVisual`:
/// `large = EmoteSizeMode == "large" || (EmoteSizeMode == "platform" && LargeEmoticon)`)
/// mean the platform's large marker only applies in `platform` mode; an explicit
/// Small or Large choice always wins, so a user Small preference is never
/// overridden by a platform large marker.
pub fn effective_emote_size(configured: EmoteSizeMode, platform_large: bool) -> EmoteSizeMode {
    if platform_large && configured == EmoteSizeMode::Platform {
        EmoteSizeMode::Large
    } else {
        configured
    }
}

/// Text rows reserved for one image message. Only the effective Large size paints
/// taller than one row (height factor 1.8 vs a 1.25 row), so the reserved rows
/// must follow the same effective mode as the painter; otherwise a Small choice
/// still reserves two rows and a Platform message without the large marker
/// over-reserves.
fn emote_rows(
    show_images: bool,
    has_image: bool,
    configured: EmoteSizeMode,
    platform_large: bool,
) -> usize {
    let large = show_images
        && has_image
        && effective_emote_size(configured, platform_large) == EmoteSizeMode::Large;
    if large { 2 } else { 1 }
}

/// Records a highlight against a message id that has already passed admission.
///
/// The admission pipeline rewrites ids to `local-merge-N` when duplicate merging
/// is on, while the painter looks highlights up on the queued/scheduled item.
/// Storing the wire id therefore silently dropped the highlight colour; every
/// writer must pass the id of the message it actually queued.
fn remember_highlight(
    ids: &mut std::collections::BTreeSet<String>,
    highlight: bool,
    admitted_id: &str,
) {
    if highlight && ids.len() < 4096 {
        ids.insert(admitted_id.to_owned());
    }
}

/// Converts the configured opacity percentage into a byte alpha. The legacy
/// renderer multiplies the source colour's alpha by OpacityPercent/100 (rounded),
/// so 90 means "90 % opaque" (alpha 230), not "alpha byte 90".
pub fn surface_alpha(opacity_percent: u8, solid_text: bool) -> u8 {
    if solid_text {
        let percent = u16::from(opacity_percent.clamp(10, 100));
        ((percent * 255 + 50) / 100) as u8
    } else {
        255
    }
}

/// Converts reserved percent rectangles into scheduler bands in points.
pub fn reserved_bands_from_areas(areas: &[PercentRect], height_points: f32) -> Vec<(f32, f32)> {
    if !height_points.is_finite() || height_points <= 0.0 {
        return Vec::new();
    }
    areas
        .iter()
        .take(PercentRect::MAX_AREAS)
        .map(|area| {
            let top = f32::from(area.y) * height_points / 100.0;
            let bottom = (f32::from(area.y) + f32::from(area.height)) * height_points / 100.0;
            (top, bottom)
        })
        .filter(|(top, bottom)| bottom > top)
        .collect()
}

/// F10 (runtime) and the persisted mode combine here.
pub fn effective_sc_mode(super_chat_visible: bool, mode: ScMode) -> ScMode {
    if super_chat_visible {
        mode
    } else {
        ScMode::Off
    }
}

/// Visible card window without scrolling. Mirrors the legacy ScMaxStack rule:
/// up to N cards stack normally; once more than N exist the view height shrinks
/// to the first N cards and the rest are reachable with the wheel. No card is
/// ever discarded here (the 200-card hard cap lives in ScCardStack).
pub fn sc_visible_count(mode: ScMode, total: usize, max_stack: u16) -> usize {
    if total == 0 {
        return 0;
    }
    match mode {
        ScMode::Off => 0,
        ScMode::Badge => 1,
        ScMode::Full | ScMode::Compact => {
            let threshold = usize::from(max_stack);
            if threshold == 0 {
                total
            } else {
                total.min(threshold)
            }
        }
    }
}

/// Legacy merge heat level: floor(log2(count)) capped at 5.
pub fn merge_level(repeat_count: u32) -> u32 {
    let mut count = repeat_count.max(1);
    let mut level = 0;
    while count >= 2 && level < 5 {
        count /= 2;
        level += 1;
    }
    level
}

/// Legacy merged-danmaku growth: 1.0 at ×1, up to max_percent/100 at ×32+.
pub fn merge_scale(repeat_count: u32, merged_max_scale_percent: u16) -> f32 {
    let max = f32::from(merged_max_scale_percent.clamp(100, 180)) / 100.0;
    1.0 + (max - 1.0) * merge_level(repeat_count) as f32 / 5.0
}

/// Platform duration with the user multiplier applied; bounded so a hostile
/// or broken platform value can never keep a card forever.
pub fn sc_card_lifetime(duration_secs: u32, duration_scale_percent: u16) -> f64 {
    let scale = f64::from(duration_scale_percent.clamp(20, 400)) / 100.0;
    (card_lifetime_secs(duration_secs) * scale).clamp(0.5, 3600.0)
}

/// Per-message glyph scale when the screen is crowded (legacy
/// ShrinkWhenCrowded / CrowdedScalePercent). The budget is the legacy
/// densityBudget = min(MaxOnScreen, trackCount*2); once 80% of it is on screen
/// ordinary messages shrink. One is full size.
pub fn crowd_scale(active: usize, budget: usize, shrink: bool, crowded_percent: u8) -> f32 {
    if !shrink || budget == 0 {
        return 1.0;
    }
    if active * 10 >= budget * 8 {
        f32::from(crowded_percent.clamp(70, 100)) / 100.0
    } else {
        1.0
    }
}

/// A fixed message wider than the viewport becomes scrolling when the legacy
/// FixedOverflowToScroll option is enabled.
pub fn effective_mode(
    mode: danmaku::DanmakuMode,
    measured_width: f32,
    viewport_width: f32,
    overflow_to_scroll: bool,
) -> danmaku::DanmakuMode {
    match mode {
        danmaku::DanmakuMode::Top | danmaku::DanmakuMode::Bottom
            if overflow_to_scroll && measured_width > viewport_width.max(1.0) =>
        {
            danmaku::DanmakuMode::Scroll
        }
        other => other,
    }
}

/// Vertical band occupied by the paid-message stack. Scrolling lanes avoid it.
/// The legacy renderer checks real flight paths; reserving whole lanes is a
/// conservative approximation that can never overlap the cards.
pub fn sc_reserved_band(
    mode: ScMode,
    corner: crate::core::ScCorner,
    cards: usize,
    height_points: f32,
    margin: f32,
    estimated_card_height: f32,
) -> Option<(f32, f32)> {
    if mode == ScMode::Off
        || cards == 0
        || !height_points.is_finite()
        || height_points <= 0.0
        || !estimated_card_height.is_finite()
    {
        return None;
    }
    let band_height = (estimated_card_height * cards as f32)
        .max(1.0)
        .min(height_points * 0.6);
    if corner.bottom() {
        Some((
            (height_points - margin - band_height).max(0.0),
            height_points,
        ))
    } else {
        Some((0.0, (margin + band_height).min(height_points)))
    }
}

/// Remaining lifetime fraction of one paid-message card, drawn as the 4px
/// progress bar below the card.
pub fn sc_progress_ratio(created: f64, expires: f64, now: f64) -> f32 {
    if !created.is_finite() || !expires.is_finite() || !now.is_finite() || expires <= created {
        return 1.0;
    }
    (((expires - now) / (expires - created)) as f32).clamp(0.0, 1.0)
}

/// One paid-message card rectangle plus its close-button hotspot (points).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScCardRect {
    pub rect: Rect,
    /// Empty rect when the card has no close button (badge mode).
    pub close: Rect,
}

/// Minimal synthetic message used to run the user/text rules against a live
/// event (gift / enter / notice rows).
pub fn rule_probe_message(user: &str, text: &str, user_id: u64) -> danmaku::DanmakuMessage {
    danmaku::DanmakuMessage {
        image_url: None,
        id: String::new(),
        user: truncate_chars(user, 64),
        text: truncate_chars(text, 256),
        color: 0xffffff,
        mode: danmaku::DanmakuMode::Scroll,
        font_size: 25,
        user_id,
        user_level: 0,
        guard_level: 0,
        medal: None,
        image_width: 0,
        image_height: 0,
        image_large: false,
    }
}

/// A click resolved against the last painted paid-message layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScClick {
    None,
    /// Close the card at this painted index.
    Close(usize),
    /// Cycle the paid-message display form (legacy: full -> compact -> badge).
    Toggle(usize),
}

/// Full geometry of the paid-message stack for one frame. Cards are newest
/// first; entries outside the view keep their (possibly off-screen) geometry so
/// painting and hit testing agree, and callers clip to view.
#[derive(Debug, Clone, PartialEq)]
pub struct ScLayout {
    pub view: Rect,
    pub cards: Vec<ScCardRect>,
    /// Content height of every retained card, gaps included.
    pub total: f32,
    pub max_scroll: f32,
    pub scroll: f32,
    /// Draggable scrollbar track/thumb, when the stack overflows the view.
    pub scrollbar: Option<Rect>,
    pub thumb: Option<Rect>,
    pub width: f32,
}

/// Legacy viewport rule: top corners start at 12 px; bottom corners stick to
/// the lower edge. The content anchors to the top for top corners and to the
/// bottom for bottom corners, which makes new cards push the group up.
pub fn sc_view_top(
    area: Rect,
    bottom: bool,
    margin: f32,
    view_height: f32,
    px_to_points: f32,
) -> f32 {
    let height = area.height().max(0.0);
    let px = if px_to_points.is_finite() {
        px_to_points.clamp(0.01, 8.0)
    } else {
        1.0
    };
    if bottom {
        (height - margin - view_height).max(6.0 * px)
    } else {
        (12.0 * px).min(height)
    }
}

/// One complete paid-message layout (legacy DrawSuperChats geometry, pure).
pub fn sc_layout(
    area: Rect,
    corner: crate::core::ScCorner,
    scale: f32,
    heights: &[f32],
    scroll_threshold: u16,
    scroll: f32,
    px_to_points: f32,
) -> ScLayout {
    let px = if px_to_points.is_finite() {
        px_to_points.clamp(0.01, 8.0)
    } else {
        1.0
    };
    let scale = if scale.is_finite() { scale } else { 1.0 };
    let margin = SC_CARD_MARGIN * scale;
    let gap = SC_CARD_GAP * scale;
    let width = sc_card_width_points(scale);
    let left = corner.left();
    let bottom = corner.bottom();
    let height = area.height().max(1.0);
    let mut total = 0.0;
    let heights: Vec<f32> = heights
        .iter()
        .map(|value| {
            if value.is_finite() {
                value.max(1.0)
            } else {
                1.0
            }
        })
        .collect();
    for card_height in &heights {
        total += card_height + gap;
    }
    let natural = (height - 12.0 - margin).max(40.0);
    let threshold = usize::from(scroll_threshold);
    let visible = sc_visible_count(ScMode::Full, heights.len(), scroll_threshold);
    let mut cap = 0.0;
    if threshold > 0 && heights.len() > threshold {
        for card_height in heights.iter().take(visible) {
            cap += card_height + gap;
        }
    }
    let view_height = if cap > 0.0 && cap < natural {
        cap.max(40.0)
    } else {
        natural
    };
    let view_top = sc_view_top(area, bottom, margin, view_height, px);
    let max_scroll = (total - view_height).max(0.0);
    let scroll = if scroll.is_finite() {
        scroll.clamp(0.0, max_scroll)
    } else {
        0.0
    };
    let content_top = if bottom && total < view_height {
        view_top + (view_height - total)
    } else {
        view_top
    };
    let x = if left {
        area.left() + margin
    } else {
        area.right() - margin - width
    };
    let mut cursor = content_top - scroll;
    let mut cards = Vec::with_capacity(heights.len());
    let close_size = 16.0 * scale;
    for card_height in heights.iter().copied() {
        let y = area.top() + cursor;
        let rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(width, card_height));
        let close = Rect::from_min_size(
            Pos2::new(
                rect.right() - close_size - 6.0 * scale,
                rect.top() + 6.0 * scale,
            ),
            Vec2::splat(close_size),
        );
        cards.push(ScCardRect { rect, close });
        cursor += card_height + gap;
    }
    let view = Rect::from_min_size(
        Pos2::new(area.left(), area.top() + view_top),
        Vec2::new(area.width().max(1.0), view_height),
    );
    let (scrollbar, thumb) = if max_scroll > 0.5 && total > 0.0 {
        let bar_width = 8.0 * scale;
        let bar_x = if left {
            x - (bar_width + 2.0 * scale)
        } else {
            x + width + 2.0 * scale
        };
        let track = Rect::from_min_size(
            Pos2::new(bar_x, area.top() + view_top),
            Vec2::new(bar_width, view_height),
        );
        let thumb_height = (24.0 * scale).max(view_height * (view_height / total));
        let thumb_y = view_top + (view_height - thumb_height) * (scroll / max_scroll);
        let thumb = Rect::from_min_size(
            Pos2::new(bar_x, area.top() + thumb_y),
            Vec2::new(bar_width, thumb_height),
        );
        (Some(track), Some(thumb))
    } else {
        (None, None)
    };
    ScLayout {
        view,
        cards,
        total,
        max_scroll,
        scroll,
        scrollbar,
        thumb,
        width,
    }
}

/// Wheel scroll distance per notch, in logical points.
///
/// The legacy Win32 handler received the raw wheel delta (120 per notch) and
/// applied `scroll -= delta * 0.6`, i.e. 72 px per notch. This build records
/// notches (delta / 120) instead of raw deltas, so reusing the old 0.6
/// coefficient made one notch move **0.6 points** — a full wheel turn barely
/// moved a card. Scaling by 120 restores the legacy feel (72 points/notch).
pub const SC_WHEEL_STEP_POINTS: f32 = 72.0;

/// Wheel scrolling: notches move the stack by [`SC_WHEEL_STEP_POINTS`] and the
/// result never goes negative; a positive delta shows newer cards.
pub fn sc_scroll_after(current: f32, delta: f32, max_scroll: f32) -> f32 {
    let current = if current.is_finite() { current } else { 0.0 };
    let delta = if delta.is_finite() { delta } else { 0.0 };
    (current - delta * SC_WHEEL_STEP_POINTS).clamp(0.0, max_scroll.max(0.0))
}

/// Resolves a click against the painted card rectangles (newest first).
pub fn resolve_sc_click(cards: &[ScCardRect], point: Pos2, viewport: Rect) -> ScClick {
    if !point.x.is_finite() || !point.y.is_finite() || !viewport.contains(point) {
        return ScClick::None;
    }
    for (index, card) in cards.iter().enumerate() {
        if card.close != Rect::NOTHING && card.close.contains(point) {
            return ScClick::Close(index);
        }
    }
    for (index, card) in cards.iter().enumerate() {
        if card.rect.contains(point) {
            return ScClick::Toggle(index);
        }
    }
    ScClick::None
}

/// Badge-mode pill geometry: the legacy widget draws a content-sized pill whose
/// right edge sits on the card's right edge (left corner: on the left edge) and
/// which hugs the same 12 px top offset / bottom margin as the cards.
pub fn sc_badge_rect(
    area: Rect,
    corner: crate::core::ScCorner,
    scale: f32,
    text_width: f32,
    text_height: f32,
    px_to_points: f32,
) -> Rect {
    let scale = if scale.is_finite() {
        scale.max(0.05)
    } else {
        1.0
    };
    let px = if px_to_points.is_finite() {
        px_to_points.clamp(0.01, 8.0)
    } else {
        1.0
    };
    let margin = SC_CARD_MARGIN * scale;
    let width = (text_width + 20.0 * scale).max(1.0);
    let height = (text_height + 13.0 * scale).max(1.0);
    let x = if corner.left() {
        area.left() + margin
    } else {
        area.right() - margin - width
    };
    let y = if corner.bottom() {
        (area.height() - margin - height).max(6.0 * px) + area.top()
    } else {
        area.top() + 12.0 * px
    };
    Rect::from_min_size(Pos2::new(x, y), Vec2::new(width, height))
}

/// Interaction published to the native window procedure. All rectangles are in
/// physical client pixels so WM_NCHITTEST and WM_MOUSEWHEEL can use them.
#[derive(Debug, Default)]
pub struct InteractionState {
    pub hit: Vec<[f32; 4]>,
    pub close: Vec<[f32; 4]>,
    pub enabled: bool,
    /// Pending click in physical client pixels.
    pub click: Option<(f32, f32)>,
    /// Accumulated wheel notches.
    pub wheel: f32,
    /// Paid-message scrollbar track/thumb in physical client pixels.
    pub bar_track: Option<[f32; 4]>,
    pub bar_thumb: Option<[f32; 4]>,
    /// Active scrollbar drag: (grab offset from thumb top, current y) in physical px.
    pub bar_drag: Option<(f32, f32)>,
}

pub type SharedInteraction = std::sync::Arc<std::sync::Mutex<InteractionState>>;

static INTERACTION: std::sync::OnceLock<SharedInteraction> = std::sync::OnceLock::new();

/// Registers the interaction state read by the native window procedure. Called
/// once when the overlay app starts.
pub fn install_interaction(state: SharedInteraction) {
    let _ = INTERACTION.set(state);
}

pub fn shared_interaction() -> SharedInteraction {
    INTERACTION
        .get_or_init(|| std::sync::Arc::new(std::sync::Mutex::new(InteractionState::default())))
        .clone()
}

/// Whether the native window currently has any clickable paid-message area.
pub fn interaction_enabled() -> bool {
    INTERACTION
        .get()
        .and_then(|state| state.lock().ok().map(|state| state.enabled))
        .unwrap_or(false)
}

/// Records one click in physical client pixels. Called by the window procedure.
fn record_native_click(x: f32, y: f32) {
    if let Some(state) = INTERACTION.get() {
        if let Ok(mut state) = state.lock() {
            state.click = Some((x, y));
        }
    }
}

/// Accumulates wheel notches from the window procedure.
fn record_native_wheel(notches: f32) {
    if let Some(state) = INTERACTION.get() {
        if let Ok(mut state) = state.lock() {
            state.wheel += notches;
        }
    }
}

/// Publishes the painted paid-message rectangles in physical client pixels.
fn publish_sc_hits(
    hits: &[(Rect, Rect, String)],
    pixels_per_point: f32,
    viewport: Rect,
    enabled: bool,
) {
    if let Some(state) = INTERACTION.get() {
        if let Ok(mut state) = state.lock() {
            state.hit.clear();
            state.close.clear();
            state.enabled = enabled;
            if !enabled {
                return;
            }
            for (rect, close, _) in hits {
                // Clip to the visible band before publishing: a card that is
                // scrolled partially out of view must not reserve the whole
                // off-screen rectangle for the mouse (that made the overlay seem
                // to stop being click-through around a tall stack).
                let visible = rect.intersect(viewport);
                if !visible.is_positive() {
                    continue;
                }
                let clamp = |value: f32| if value.is_finite() { value } else { 0.0 };
                state.hit.push([
                    clamp(visible.left()) * pixels_per_point,
                    clamp(visible.top()) * pixels_per_point,
                    clamp(visible.right()) * pixels_per_point,
                    clamp(visible.bottom()) * pixels_per_point,
                ]);
                if *close != Rect::NOTHING && close.intersects(viewport) {
                    let close_visible = close.intersect(viewport);
                    state.close.push([
                        clamp(close_visible.left()) * pixels_per_point,
                        clamp(close_visible.top()) * pixels_per_point,
                        clamp(close_visible.right()) * pixels_per_point,
                        clamp(close_visible.bottom()) * pixels_per_point,
                    ]);
                }
            }
        }
    }
}

/// Publishes the paid-message scrollbar rectangles (physical pixels) so the
/// window procedure can start a drag and the cursor can change over the bar.
fn publish_sc_bar(track: Option<Rect>, thumb: Option<Rect>, pixels_per_point: f32) {
    if let Some(state) = INTERACTION.get() {
        if let Ok(mut state) = state.lock() {
            let to_physical = |rect: Rect| {
                [
                    rect.left() * pixels_per_point,
                    rect.top() * pixels_per_point,
                    rect.right() * pixels_per_point,
                    rect.bottom() * pixels_per_point,
                ]
            };
            state.bar_track = track.map(to_physical);
            state.bar_thumb = thumb.map(to_physical);
        }
    }
}

/// Grab offset (physical px from the thumb top) when the point is on the
/// scrollbar track, or None.
fn interaction_bar_grab(x: f32, y: f32) -> Option<f32> {
    let state = INTERACTION.get()?.lock().ok()?;
    if !state.enabled {
        return None;
    }
    let track = state.bar_track?;
    // A few pixels of horizontal tolerance: the 8px bar is thin for a mouse.
    if x < track[0] - 4.0 || x > track[2] + 4.0 || y < track[1] || y > track[3] {
        return None;
    }
    let thumb = state.bar_thumb?;
    Some((y - thumb[1]).max(0.0))
}

fn begin_native_bar_drag(offset: f32) {
    if let Some(state) = INTERACTION.get() {
        if let Ok(mut state) = state.lock() {
            state.bar_drag = Some((offset, offset));
        }
    }
}

fn move_native_bar_drag(y: f32) {
    if let Some(state) = INTERACTION.get() {
        if let Ok(mut state) = state.lock() {
            if let Some((offset, _)) = state.bar_drag {
                state.bar_drag = Some((offset, y));
            }
        }
    }
}

/// Ends a scrollbar drag; returns true when a drag was active (the caller must
/// then suppress the click action).
fn end_native_bar_drag() -> bool {
    if let Some(state) = INTERACTION.get() {
        if let Ok(mut state) = state.lock() {
            return state.bar_drag.take().is_some();
        }
    }
    false
}

/// Drag mapping (pure): mouse y on the scrollbar track maps to the scroll offset.
pub fn sc_bar_scroll_for(
    y: f32,
    grab_offset: f32,
    track_y: f32,
    track_height: f32,
    thumb_height: f32,
    max_scroll: f32,
) -> f32 {
    let travel = track_height - thumb_height;
    if !y.is_finite()
        || !grab_offset.is_finite()
        || !travel.is_finite()
        || travel <= 0.0
        || !max_scroll.is_finite()
        || max_scroll <= 0.0
    {
        return 0.0;
    }
    let ratio = ((y - grab_offset - track_y) / travel).clamp(0.0, 1.0);
    ratio * max_scroll
}

/// Disables native hit testing (full mouse passthrough), e.g. no SC cards.
fn clear_sc_hits() {
    if let Some(state) = INTERACTION.get() {
        if let Ok(mut state) = state.lock() {
            state.hit.clear();
            state.close.clear();
            state.enabled = false;
            state.bar_track = None;
            state.bar_thumb = None;
            state.bar_drag = None;
        }
    }
}

/// Whether a physical client pixel is inside a clickable paid-message area.
fn interaction_contains(x: f32, y: f32) -> bool {
    INTERACTION
        .get()
        .and_then(|state| {
            state.lock().ok().map(|state| {
                state.enabled
                    && state
                        .hit
                        .iter()
                        .chain(state.close.iter())
                        .any(|rect| x >= rect[0] && x <= rect[2] && y >= rect[1] && y <= rect[3])
            })
        })
        .unwrap_or(false)
}

/// Whether a physical client pixel is on the paid-message scrollbar track.
fn interaction_bar_hit(x: f32, y: f32) -> bool {
    INTERACTION
        .get()
        .and_then(|state| {
            state.lock().ok().map(|state| {
                if !state.enabled {
                    return false;
                }
                match state.bar_track {
                    Some(track) => {
                        x >= track[0] - 4.0 && x <= track[2] + 4.0 && y >= track[1] && y <= track[3]
                    }
                    None => false,
                }
            })
        })
        .unwrap_or(false)
}

/// Physical client rectangles that must answer the mouse: painted cards, their
/// close buttons and the scrollbar track. The transparent input window is
/// clipped to exactly this union, so every other pixel reaches the player.
fn interaction_regions() -> Vec<[f32; 4]> {
    INTERACTION
        .get()
        .and_then(|state| {
            state.lock().ok().map(|state| {
                if !state.enabled {
                    return Vec::new();
                }
                let mut regions: Vec<[f32; 4]> = Vec::with_capacity(state.hit.len() + 1);
                regions.extend(state.hit.iter().copied());
                // A close button lives inside its card rect; only add it when
                // it is not already covered, to keep the region small.
                for close in &state.close {
                    let covered = regions.iter().any(|rect| {
                        close[0] >= rect[0]
                            && close[2] <= rect[2]
                            && close[1] >= rect[1]
                            && close[3] <= rect[3]
                    });
                    if !covered {
                        regions.push(*close);
                    }
                }
                if let Some(track) = state.bar_track {
                    regions.push(track);
                }
                regions
            })
        })
        .unwrap_or_default()
}

/// Whether one live event kind is enabled by the current behaviour settings.
pub fn live_event_visible(event: &LiveEvent, behavior: &crate::core::DanmakuBehavior) -> bool {
    match event.kind {
        LiveEventKind::Gift => behavior.show_gifts,
        LiveEventKind::Guard => behavior.show_guards,
        LiveEventKind::Enter | LiveEventKind::Follow => behavior.show_enters,
        LiveEventKind::Notice => behavior.show_notice,
        LiveEventKind::Status => behavior.show_live_stats,
    }
}

/// One display line for a live event, matching the legacy EventLabel wording.
pub fn live_event_label(event: &LiveEvent) -> String {
    let user = if event.user.is_empty() {
        "观众"
    } else {
        event.user.as_str()
    };
    let count = if event.count > 1 {
        format!(" ×{}", event.count)
    } else {
        String::new()
    };
    let text = match event.kind {
        LiveEventKind::Gift => format!("{user} 赠送 {}{count}", event.text),
        LiveEventKind::Guard => format!("{user} {}", event.text),
        LiveEventKind::Enter => format!("{user} 进入直播间"),
        LiveEventKind::Follow => format!("{user} 关注了主播"),
        LiveEventKind::Notice => format!("公告：{}", event.text),
        LiveEventKind::Status => event.text.clone(),
    };
    truncate_chars(&text, 256)
}

// ---- SC (super chat / paid message) card stack ---------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ScCard {
    pub id: String,
    pub user: String,
    pub text: String,
    pub price_cents: u64,
    pub created: f64,
    pub expires: f64,
    /// Platform translation of the body text, when supplied.
    pub message_trans: String,
    /// Approved avatar URL; empty when the platform sent none.
    pub face: String,
    /// Optional avatar frame image URL (B站 face_frame).
    pub face_frame: String,
    pub user_level: u16,
    pub guard_level: u8,
    pub medal_name: String,
    pub medal_level: u16,
    /// Platform card colours (0xRRGGBB); fallbacks are the documented defaults.
    /// The legacy widget paints the header band with background_color and the
    /// content band with background_bottom_color.
    pub background_color: u32,
    pub background_bottom_color: u32,
    pub background_color_start: u32,
    pub message_font_color: u32,
    pub name_color: u32,
}

/// Paid-message display time. The platform gives the real duration (observed
/// 5 s for a 2 yuan message, 60 s for 30 yuan, 300 s for 2000 yuan); the legacy
/// overlay showed the card for exactly that long, so the overlay does too. The
/// user duration multiplier is applied by sc_card_lifetime.
pub fn card_lifetime_secs(duration_secs: u32) -> f64 {
    f64::from(duration_secs.clamp(1, 3600))
}

/// Character-aware truncation: never splits a UTF-8 sequence and marks the cut.
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let mut result: String = text.chars().take(max_chars - 1).collect();
    result.push('…');
    result
}

/// Legacy paid-message price label: the widget prints the integer yuan value
/// followed by the character 元 (for example 30元), never a currency symbol.
pub fn price_label(price_cents: u64) -> String {
    let yuan = price_cents / 100;
    if price_cents % 100 == 0 {
        format!("{yuan}元")
    } else {
        format!("{yuan}.{:02}元", price_cents % 100)
    }
}

/// Kept for older callers/tests: same value without the unit.
pub fn format_price(price_cents: u64) -> String {
    price_label(price_cents)
}

/// Simple, bounded price tiers: low -> blue/green, high -> orange/magenta.
pub fn sc_tier_color(price_cents: u64) -> [u8; 3] {
    match price_cents {
        0 => [130, 175, 255],
        1..=4_999 => [90, 200, 140],
        5_000..=19_999 => [255, 190, 80],
        20_000..=99_999 => [255, 130, 90],
        _ => [240, 90, 210],
    }
}

/// Contrast choice used by the legacy paid-message widget: when the platform
/// name/price colour does not contrast with the card header, pick the darker or
/// lighter fallback (legacy getReadableTextColor, WCAG ratio 4.5).
pub fn readable_text_color(text_rgb: u32, background_rgb: u32) -> Color32 {
    let text = rgb_channels(text_rgb);
    let background = rgb_channels(background_rgb);
    if contrast_ratio(text, background) >= 4.5 {
        return Color32::from_rgb(text[0], text[1], text[2]);
    }
    let dark = [51_u8, 51, 51];
    let light = [255_u8, 255, 255];
    if contrast_ratio(dark, background) >= contrast_ratio(light, background) {
        Color32::from_rgb(dark[0], dark[1], dark[2])
    } else {
        Color32::from_rgb(light[0], light[1], light[2])
    }
}

pub fn rgb_channels(rgb: u32) -> [u8; 3] {
    [
        ((rgb >> 16) & 0xff) as u8,
        ((rgb >> 8) & 0xff) as u8,
        (rgb & 0xff) as u8,
    ]
}

fn relative_luminance(rgb: [u8; 3]) -> f64 {
    fn channel(value: u8) -> f64 {
        let value = f64::from(value) / 255.0;
        if value <= 0.03928 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(rgb[0]) + 0.7152 * channel(rgb[1]) + 0.0722 * channel(rgb[2])
}

pub fn contrast_ratio(first: [u8; 3], second: [u8; 3]) -> f64 {
    let a = relative_luminance(first);
    let b = relative_luminance(second);
    let (lighter, darker) = if a > b { (a, b) } else { (b, a) };
    (lighter + 0.05) / (darker + 0.05)
}

/// Width of one paid-message card in egui points: the legacy widget is
/// 270 px wide at 100% scale (405 px at 1080p), so the point size is
/// 270 * sc_points_scale.
pub fn sc_card_width_points(scale: f32) -> f32 {
    if !scale.is_finite() || scale <= 0.0 {
        return SC_CARD_BASE_WIDTH;
    }
    (SC_CARD_BASE_WIDTH * scale).clamp(80.0, 2000.0)
}

#[derive(Debug, Clone)]
pub struct ScCardStack {
    cards: Vec<ScCard>,
    capacity: usize,
}

impl Default for ScCardStack {
    fn default() -> Self {
        Self::new()
    }
}

impl ScCardStack {
    pub fn new() -> Self {
        Self::with_capacity(MAX_SC_CARDS)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            cards: Vec::new(),
            capacity: capacity.max(1),
        }
    }
    pub fn len(&self) -> usize {
        self.cards.len()
    }
    pub fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }
    /// Oldest first; the renderer walks this in reverse so the newest card sits on top.
    pub fn cards(&self) -> &[ScCard] {
        &self.cards
    }

    /// Insert a card, dropping expired ones first and evicting the oldest card when
    /// the stack is full. Re-sent platform ids (reconnect replays) are ignored.
    pub fn push(&mut self, now: f64, super_chat: &SuperChat) -> Option<ScCard> {
        self.push_with_scale(now, super_chat, 100)
    }

    /// Same as push, with the persisted SC duration scale applied to the
    /// card lifetime (bounded so a bad value cannot keep a card forever).
    pub fn push_with_scale(
        &mut self,
        now: f64,
        super_chat: &SuperChat,
        duration_scale_percent: u16,
    ) -> Option<ScCard> {
        if !now.is_finite() {
            return None;
        }
        self.prune(now);
        let id = truncate_chars(&super_chat.id, SC_ID_MAX_CHARS);
        if !id.is_empty() && self.cards.iter().any(|card| card.id == id) {
            return None;
        }
        while self.cards.len() >= self.capacity {
            self.cards.remove(0);
        }
        let card = ScCard {
            id,
            user: truncate_chars(&super_chat.user, SC_USER_MAX_CHARS),
            text: truncate_chars(&super_chat.text, SC_TEXT_MAX_CHARS),
            price_cents: super_chat.price_cents,
            created: now,
            expires: now + sc_card_lifetime(super_chat.duration_secs, duration_scale_percent),
            message_trans: truncate_chars(&super_chat.message_trans, SC_TEXT_MAX_CHARS),
            face: super_chat.face.clone(),
            face_frame: super_chat.face_frame.clone(),
            user_level: super_chat.user_level,
            guard_level: super_chat.guard_level,
            medal_name: truncate_chars(
                super_chat
                    .medal
                    .as_ref()
                    .map(|medal| medal.name.as_str())
                    .unwrap_or(""),
                24,
            ),
            medal_level: super_chat
                .medal
                .as_ref()
                .map(|medal| medal.level)
                .unwrap_or(0),
            background_color: super_chat.background_color,
            background_bottom_color: super_chat.background_bottom_color,
            background_color_start: super_chat.background_color_start,
            message_font_color: super_chat.message_font_color,
            name_color: super_chat.name_color,
        };
        self.cards.push(card.clone());
        Some(card)
    }

    /// Delete messages only address real platform ids; an empty id never matches.
    pub fn remove(&mut self, id: &str) -> bool {
        if id.is_empty() {
            return false;
        }
        let before = self.cards.len();
        self.cards.retain(|card| card.id != id);
        self.cards.len() != before
    }

    /// Time-based eviction in insertion order; returns how many cards expired.
    pub fn prune(&mut self, now: f64) -> usize {
        if !now.is_finite() {
            return 0;
        }
        let before = self.cards.len();
        self.cards.retain(|card| now < card.expires);
        before - self.cards.len()
    }

    pub fn clear(&mut self) {
        self.cards.clear();
    }
}

// ---- terminal connection failure policy ----------------------------------

#[derive(Debug, Clone)]
struct FatalNotice {
    message: String,
    since: Instant,
}
type FatalSlot = Arc<Mutex<Option<FatalNotice>>>;

fn fatal_notice(slot: &FatalSlot) -> Option<FatalNotice> {
    slot.lock().ok().and_then(|guard| guard.clone())
}

/// The overlay closes itself once the error has been on screen long enough.
pub fn fatal_expired(age: Duration) -> bool {
    age.as_secs_f32() >= FATAL_DISPLAY_SECS
}

/// The window-follow thread stops the overlay even if it is hidden and never paints.
pub fn follow_exit_after_fatal(age: Duration) -> bool {
    age.as_secs_f32() >= FOLLOW_FATAL_EXIT_SECS
}

// ---- runtime -------------------------------------------------------------

pub fn run(target: Target, player_pid: u32) -> Result<()> {
    let legacy = settings_from_env(|key| std::env::var(key).ok());
    let mut options = OverlaySettings::default();
    options.font_size = legacy.font_size as u16;
    options.superchat_enabled = legacy.super_chat_visible;
    run_configured(target, player_pid, None, options, legacy.danmaku_visible)
}

pub fn run_authenticated(target: Target, player_pid: u32, vault: &CookieVault) -> Result<()> {
    run_with_options(
        target,
        player_pid,
        Some(vault.clone()),
        OverlaySettings::default(),
    )
}

/// Owns a clone of the explicit vault's directory, never a discovered profile.
/// Settings are a validated snapshot from the same selected Rust data root.
pub fn run_with_options(
    target: Target,
    player_pid: u32,
    vault: Option<CookieVault>,
    options: OverlaySettings,
) -> Result<()> {
    run_configured(target, player_pid, vault, options, true)
}

fn run_configured(
    target: Target,
    player_pid: u32,
    vault: Option<CookieVault>,
    options: OverlaySettings,
    danmaku_visible: bool,
) -> Result<()> {
    ensure!(player_pid != 0, "弹幕覆盖层需要有效播放器进程号");
    options.validate()?;
    let state_dir = data_dir_from_args();
    init_overlay_diag(state_dir.as_deref());
    // The receiver is the generic danmaku::connect entry point. Once the
    // platform workstreams land it dispatches to Bilibili/Douyu/Huya/Douyin; a
    // platform whose transport is unavailable returns an error instead of
    // panicking, and the overlay shows that error and closes.
    #[cfg(not(windows))]
    {
        let _ = target;
        anyhow::bail!("播放器窗口跟随当前仅支持 Windows");
    }
    #[cfg(windows)]
    {
        let process = native::PlayerProcess::open(player_pid)?;
        let shutdown_process = native::PlayerProcess::open(player_pid)?;
        let (sender, receiver) = danmaku::bounded_channel(512);
        let cancellation = Cancellation::new();
        // A graphics/event-loop deadlock must not leave an orphan overlay.
        // This independent guard owns only a SYNCHRONIZE handle and may exit
        // only this dedicated overlay process, never the player's process.
        let shutdown_cancel = cancellation.clone();
        thread::Builder::new()
            .name("overlay-exit-guard".into())
            .spawn(move || native::shutdown_guard(shutdown_process, shutdown_cancel))?;
        let fatal: FatalSlot = Arc::new(Mutex::new(None));
        let worker_cancel = cancellation.clone();
        let worker_fatal = fatal.clone();
        // Diagnostic acceptance runs replace the network receiver with a local
        // synthetic event list; unset in every product launch.
        let fixture = std::env::var(ENV_FIXTURE)
            .ok()
            .map(|path| path.trim().to_owned())
            .filter(|path| !path.is_empty());
        let worker = thread::Builder::new()
            .name("danmaku-receiver".into())
            .spawn(move || {
                let result = match fixture {
                    Some(path) => run_fixture(Path::new(&path), sender, worker_cancel)
                        .map_err(|error| anyhow!("覆盖层诊断夹具失败：{error:#}")),
                    None => match vault {
                        Some(vault) => {
                            danmaku::connect_authenticated(&target, sender, worker_cancel, &vault)
                        }
                        None => danmaku::connect(&target, sender, worker_cancel),
                    },
                };
                if let Err(error) = result {
                    overlay_log(
                        crate::diag::Level::Error,
                        &format!("弹幕连接失败：{error:#}"),
                    );
                    if let Ok(mut slot) = worker_fatal.lock() {
                        *slot = Some(FatalNotice {
                            message: format!("弹幕连接失败：{error:#}"),
                            since: Instant::now(),
                        });
                    }
                }
            })?;
        let title = format!("LiveHub Rust Overlay {}", uuid::Uuid::new_v4().simple());
        let window_title = title.clone();
        let app_cancel = cancellation.clone();
        let app_fatal = fatal.clone();
        let shared = shared_options(options.clone());
        let native_options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([640.0, 360.0])
                .with_transparent(true)
                .with_decorations(false)
                .with_resizable(false)
                .with_active(false)
                .with_visible(false)
                .with_mouse_passthrough(true),
            renderer: eframe::Renderer::Glow,
            ..Default::default()
        };
        let result = eframe::run_native(
            &title,
            native_options,
            Box::new(move |context| {
                // A hidden window still needs its first paint; the watcher will show
                // it only when a suitable player client area is in the foreground.
                let (hotkey_sender, hotkey_receiver) = std::sync::mpsc::sync_channel(16);
                let app = Overlay::new(
                    context,
                    receiver,
                    app_cancel.clone(),
                    app_fatal.clone(),
                    options,
                    danmaku_visible,
                    hotkey_receiver,
                    shared.clone(),
                    state_dir.clone(),
                );
                let ctx = context.egui_ctx.clone();
                let follow_cancel = app_cancel.clone();
                let follow_fatal = app_fatal;
                thread::Builder::new()
                    .name("overlay-window-follow".into())
                    .spawn(move || {
                        native::follow(
                            process,
                            &window_title,
                            &ctx,
                            &follow_cancel,
                            &follow_fatal,
                            hotkey_sender,
                            shared,
                        );
                    })
                    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
                Ok(Box::new(app))
            }),
        )
        .map_err(|_| anyhow!("无法创建透明弹幕窗口"));
        diag_log(&format!("run_native returned: {result:?}"));
        cancellation.cancel();
        // Receiver observes cancellation on a 500ms socket timeout; a bounded
        // HTTP/TLS handshake may still be completing. Never block UI shutdown.
        if worker.is_finished() {
            let _ = worker.join();
        }
        result
    }
}

/// One GPU texture tile of the sprite sheet. egui caps a texture side at 2048,
/// while the shipped sheet is 1920x3360, so it is split into row tiles aligned
/// to whole cell rows (no cell ever straddles a tile boundary).
struct EmojiTile {
    texture: egui::TextureHandle,
    top: u32,
    height: u32,
}

/// Optional local colour-emoji sprite sheet. The image is never bundled with
/// the build; it is loaded from an explicit local path so users who generated
/// a sheet (as the legacy build did) get coloured emoji, and everyone else
/// keeps the monochrome font fallback.
struct ColorEmojiSheet {
    manifest: crate::danmaku::rich::EmojiManifest,
    tiles: Vec<EmojiTile>,
    width: usize,
    height: usize,
    rows_per_tile: u32,
}

/// egui refuses textures whose longer side exceeds this; the loader splits the
/// sheet instead of downscaling so emoji keep their generated resolution.
const MAX_TEXTURE_SIDE: usize = 2048;

pub const ENV_EMOJI_SHEET: &str = "LIVE_HUB_EMOJI_SHEET";
pub const ENV_EMOJI_MANIFEST: &str = "LIVE_HUB_EMOJI_SHEET_MANIFEST";
/// Optional local emoticon dictionary JSON (B站 GetEmoticons response shape),
/// used to wire literal-code images for offline acceptance and smoke tests.
pub const ENV_EMOTICON_JSON: &str = "LIVE_HUB_EMOTICON_JSON";
const MAX_LOCAL_EMOTICON_BYTES: u64 = 1024 * 1024;
const MAX_SHEET_BYTES: u64 = 32 * 1024 * 1024;
const MAX_SHEET_DIMENSION: usize = 8192;

impl ColorEmojiSheet {
    fn load(ctx: &egui::Context, path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        if !metadata.is_file() || metadata.len() > MAX_SHEET_BYTES {
            return None;
        }
        let bytes = std::fs::read(path).ok()?;
        let manifest = std::env::var_os(ENV_EMOJI_MANIFEST)
            .map(std::path::PathBuf::from)
            .or_else(|| {
                let mut sibling = path.to_path_buf();
                sibling.set_extension("txt");
                sibling.is_file().then_some(sibling)
            })
            .and_then(|manifest| std::fs::read_to_string(manifest).ok())
            .and_then(|text| crate::danmaku::rich::EmojiManifest::parse(&text).ok())
            .unwrap_or_default();
        let image = crate::images::decode_sheet(&bytes).ok()?;
        if image.width == 0
            || image.height == 0
            || image.width > MAX_SHEET_DIMENSION
            || image.height > MAX_SHEET_DIMENSION
            || image.width > MAX_TEXTURE_SIDE
        {
            return None;
        }
        let cell = manifest.cell.max(1);
        // Whole cell rows per tile keeps every emoji inside exactly one texture.
        let max_rows = (MAX_TEXTURE_SIDE as u32 / cell).max(1);
        let rows_per_tile = max_rows * cell;
        let mut tiles = Vec::new();
        let mut top = 0_u32;
        while (top as usize) < image.height {
            let height = (rows_per_tile).min(image.height as u32 - top);
            let start = top as usize * image.width * 4;
            let end = start + height as usize * image.width * 4;
            let slice = image.rgba.get(start..end)?;
            let texture = ctx.load_texture(
                format!("danmaku-color-emoji-sheet-{}", tiles.len()),
                egui::ColorImage::from_rgba_unmultiplied([image.width, height as usize], slice),
                egui::TextureOptions::LINEAR,
            );
            tiles.push(EmojiTile {
                texture,
                top,
                height,
            });
            top += height;
        }
        if tiles.is_empty() {
            return None;
        }
        Some(Self {
            manifest,
            tiles,
            width: image.width,
            height: image.height,
            rows_per_tile,
        })
    }

    /// Texture and normalised source rectangle for one cell, or None when the
    /// cell lies outside the loaded sheet.
    fn cell(&self, x: u32, y: u32) -> Option<(egui::TextureId, Rect)> {
        let cell = self.manifest.cell.max(1);
        if x.saturating_add(cell) as usize > self.width
            || y.saturating_add(cell) as usize > self.height
        {
            return None;
        }
        let index = (y / self.rows_per_tile) as usize;
        let tile = self.tiles.get(index)?;
        let local_y = y.checked_sub(tile.top)?;
        if local_y.saturating_add(cell) > tile.height {
            return None;
        }
        let left = x as f32 / self.width as f32;
        let right = (x + cell) as f32 / self.width as f32;
        let top = local_y as f32 / tile.height as f32;
        let bottom = (local_y + cell) as f32 / tile.height as f32;
        Some((
            tile.texture.id(),
            Rect::from_min_max(Pos2::new(left, top), Pos2::new(right, bottom)),
        ))
    }
}

/// Resolves the colour emoji sheet: an explicit path wins, otherwise the
/// bundled copy next to the executable (preview layout ships assets/) and then
/// the current directory (development/tests).
pub fn resolve_emoji_sheet(
    explicit: Option<&Path>,
    exe_dir: Option<&Path>,
    cwd: Option<&Path>,
) -> Option<std::path::PathBuf> {
    if let Some(path) = explicit {
        if path.is_file() {
            return Some(path.to_path_buf());
        }
    }
    let mut bases: Vec<&Path> = Vec::new();
    if let Some(dir) = exe_dir {
        bases.push(dir);
    }
    if let Some(dir) = cwd {
        if !bases.contains(&dir) {
            bases.push(dir);
        }
    }
    for base in bases {
        for candidate in [
            base.join("assets").join("emoji-sheet.png"),
            base.join("emoji-sheet.png"),
        ] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Default sheet path for this process; LIVE_HUB_EMOJI_SHEET still overrides.
pub fn default_emoji_sheet_path() -> Option<std::path::PathBuf> {
    let explicit = std::env::var_os(ENV_EMOJI_SHEET)
        .map(std::path::PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty());
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    let cwd = std::env::current_dir().ok();
    resolve_emoji_sheet(explicit.as_deref(), exe_dir.as_deref(), cwd.as_deref())
}

/// Loads a local emoticon dictionary used by offline acceptance runs. Both the
/// B站 GetEmoticons shape and the raw Douyin `emoji/list` shape are accepted
/// (the Douyin endpoint is what the live receiver fetches). Bounded and silent:
/// a missing or malformed file simply leaves literal codes as text.
fn load_local_emoticons(path: &Path) -> Option<crate::danmaku::rich::EmoticonDictionary> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_LOCAL_EMOTICON_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    crate::danmaku::rich::parse_bili_emoticons(&text)
        .ok()
        .filter(|dictionary| !dictionary.is_empty())
        .or_else(|| {
            crate::danmaku::rich::parse_douyin_emoticons(&text)
                .ok()
                .filter(|dictionary| !dictionary.is_empty())
        })
}

/// The painter replaces its dictionary snapshot only when the receiver publishes
/// a new cache generation. Comparing generations (instead of "is the current
/// dictionary empty?") also refreshes after switching rooms or platforms.
fn emoticons_refresh_due(current_version: u64, cache_version: u64) -> bool {
    cache_version != current_version
}

struct Overlay {
    receiver: crate::danmaku::EventReceiver,
    cancellation: Cancellation,
    fatal: FatalSlot,
    settings: DisplaySettings,
    options: OverlaySettings,
    super_chats: ScCardStack,
    scheduler: LaneScheduler,
    metrics: SchedulerMetrics,
    start: Instant,
    pending: Vec<PendingMessage>,
    pipeline: ChatPipeline,
    image_loader: crate::images::ImageLoader,
    image_textures: std::collections::BTreeMap<String, egui::TextureHandle>,
    color_emoji: Option<ColorEmojiSheet>,
    emoticons: crate::danmaku::rich::EmoticonDictionary,
    /// Cache generation of `emoticons`; the receiver thread bumps it whenever it
    /// publishes a new room/platform dictionary, and a change re-reads the cache.
    emoticons_version: u64,
    live_events: std::collections::VecDeque<(f64, LiveEvent)>,
    native_hotkeys: Receiver<OverlayHotkey>,
    status: String,
    status_at: Instant,
    feedback: String,
    feedback_at: Instant,
    diag_capture_requested: bool,
    /// Set when a font size change requires re-measuring every painted message.
    reflow_pending: bool,
    /// Native hit-testing state shared with the window procedure.
    interaction: SharedInteraction,
    /// Pixel scroll offset of the paid-message stack (0 = newest cards visible).
    sc_scroll: f32,
    /// Maximum scroll offset painted on the last frame.
    sc_max_scroll: f32,
    /// Last painted card rectangles (points) for click resolution.
    sc_cards: Vec<(ScCardRect, String)>,
    /// Last paid-message viewport (points), used for hit testing.
    sc_area: Rect,
    /// Last painted scrollbar track/thumb (points) for the drag mapping.
    sc_bar_track: Option<Rect>,
    sc_bar_thumb: Option<Rect>,
    /// Real vertical extent (points) of the last painted paid-message stack, so
    /// fixed danmaku avoid the cards that are actually on screen.
    sc_band: Option<(f32, f32)>,
    /// Paid-message card rectangle (y0, y1, x0, x1 relative to the overlay
    /// origin) used for the fixed-message obstacle check.
    sc_obstacle: Option<(f32, f32, f32, f32)>,
    /// Live-status rows merged by topic (watched / likes / rank-count).
    live_status: std::collections::BTreeMap<String, (String, f64)>,
    /// Live-event rate window (start, count).
    event_window: (f64, u8),
    /// Recently displayed event ids -> timestamp, for reconnect de-duplication.
    event_ids: std::collections::BTreeMap<String, f64>,
    /// Upper-left live-event strip in danmaku-area points (for lane avoidance).
    event_band: Option<(f32, f32)>,
    /// Receiver diagnostics: received / admitted / dropped danmaku messages.
    stats_received: u64,
    stats_admitted: u64,
    stats_dropped: u64,
    stats_live_events: u64,
    /// Last periodic diagnostics flush.
    last_stats_log: Instant,
    /// Diagnostic frozen paint time (milliseconds); None in product launches.
    diag_freeze_ms: Option<f64>,
    /// Settings shared with the follow thread (applied live by hot reload).
    shared: SharedOptions,
    /// Legacy ToggleOverlay: the user hid the whole overlay (danmaku + SC).
    user_hidden: bool,
    /// Compiled filter/highlight/pin rules (rebuilt on every settings change).
    rules: crate::danmaku_rules::CompiledRules,
    /// Ids still on screen whose text matched a highlight or pin rule.
    highlighted_ids: std::collections::BTreeSet<String>,
    /// Isolated data directory (from --data-dir) used for the hot-reload poll.
    state_dir: Option<std::path::PathBuf>,
    /// Last observed state.json modification time.
    state_mtime: Option<std::time::SystemTime>,
    /// Last time the settings document was polled.
    last_config_poll: Instant,
}

#[derive(Debug, Clone, Copy)]
struct SchedulerMetrics {
    font_points: f32,
    width: f32,
    height: f32,
    dpi: f32,
}
impl Default for SchedulerMetrics {
    fn default() -> Self {
        Self {
            font_points: -1.0,
            width: -1.0,
            height: -1.0,
            dpi: 1.0,
        }
    }
}

impl Overlay {
    fn new(
        context: &eframe::CreationContext<'_>,
        receiver: crate::danmaku::EventReceiver,
        cancellation: Cancellation,
        fatal: FatalSlot,
        options: OverlaySettings,
        danmaku_visible: bool,
        native_hotkeys: Receiver<OverlayHotkey>,
        shared: SharedOptions,
        state_dir: Option<std::path::PathBuf>,
    ) -> Self {
        // Shared embedded HarmonyOS Sans font and its license are listed in the
        // main launcher's About page and bundled assets/fonts/LICENSE.txt.
        crate::gui::install_fonts(&context.egui_ctx);
        context.egui_ctx.set_visuals(egui::Visuals {
            window_fill: Color32::TRANSPARENT,
            panel_fill: Color32::TRANSPARENT,
            ..egui::Visuals::dark()
        });
        let settings = DisplaySettings {
            font_size: f32::from(options.font_size),
            danmaku_visible,
            super_chat_visible: options.superchat_enabled,
        };
        let mut scheduler = LaneScheduler::new(
            640.0,
            240.0,
            font_points(settings.font_size, 1.0),
            danmaku_duration_secs(options.speed_percent) as f32,
        );
        scheduler.set_capacity(usize::from(options.max_visible));
        scheduler.set_line_gap_percent(options.line_gap_percent);
        scheduler.set_fixed_layout(
            options.behavior.top_fixed_zone_percent,
            options.behavior.bottom_fixed_zone_percent,
            options.behavior.max_fixed_per_mode,
        );
        // Legacy admission rate: when the user did not pin AdmissionPerSecond,
        // the legacy renderer derived it from the on-screen cap and the crossing
        // duration (MaxOnScreen / max(2, duration)).
        let pipeline = ChatPipeline::new_for_display(
            options.behavior.clone(),
            options.max_visible,
            danmaku_duration_secs(options.speed_percent),
        );
        let rules = crate::danmaku_rules::compile_with_users(
            &options.behavior.blocked_keywords,
            &options.behavior.blocked_users,
            &options.behavior.highlight_keywords,
            &options.behavior.pin_keywords,
        );
        for error in &rules.errors {
            let line = format!("danmaku rule skipped: {error}");
            diag_log(&line);
            overlay_log(crate::diag::Level::Warn, &line);
        }
        install_interaction(shared_interaction());
        let color_emoji = default_emoji_sheet_path()
            .and_then(|path| ColorEmojiSheet::load(&context.egui_ctx, &path));
        if color_emoji.is_none() {
            diag_log("colour emoji sheet not enabled");
        }
        let emoticons = std::env::var_os(ENV_EMOTICON_JSON)
            .map(std::path::PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
            .and_then(|path| load_local_emoticons(&path))
            .unwrap_or_default();
        Self {
            receiver,
            cancellation,
            fatal,
            settings,
            options,
            pipeline,
            native_hotkeys,
            image_loader: crate::images::ImageLoader::new(),
            image_textures: Default::default(),
            color_emoji,
            emoticons,
            emoticons_version: 0,
            live_events: Default::default(),
            super_chats: ScCardStack::new(),
            scheduler,
            metrics: SchedulerMetrics::default(),
            start: Instant::now(),
            pending: Vec::new(),
            status: "正在连接弹幕…".into(),
            status_at: Instant::now(),
            feedback: String::new(),
            feedback_at: Instant::now(),
            diag_capture_requested: false,
            reflow_pending: false,
            interaction: shared_interaction(),
            sc_scroll: 0.0,
            sc_max_scroll: 0.0,
            sc_cards: Vec::new(),
            sc_area: Rect::NOTHING,
            sc_bar_track: None,
            sc_bar_thumb: None,
            sc_band: None,
            sc_obstacle: None,
            live_status: Default::default(),
            event_window: (f64::NEG_INFINITY, 0),
            event_ids: Default::default(),
            event_band: None,
            stats_received: 0,
            stats_admitted: 0,
            stats_dropped: 0,
            stats_live_events: 0,
            last_stats_log: Instant::now(),
            diag_freeze_ms: diag_freeze_ms_from(|key| std::env::var(key).ok()),
            state_dir,
            state_mtime: None,
            last_config_poll: Instant::now(),
            shared,
            user_hidden: false,
            rules,
            highlighted_ids: Default::default(),
        }
    }

    /// Polls state.json (about once a second) and applies changed overlay
    /// preferences live, mirroring the legacy renderer's mtime hot reload. A
    /// failed or half-written read keeps the current options.
    fn poll_settings(&mut self) {
        let Some(dir) = self.state_dir.clone() else {
            return;
        };
        if self.last_config_poll.elapsed() < Duration::from_millis(1000) {
            return;
        }
        self.last_config_poll = Instant::now();
        let mtime = std::fs::metadata(dir.join("state.json"))
            .and_then(|metadata| metadata.modified())
            .ok();
        if mtime.is_none() || mtime == self.state_mtime {
            return;
        }
        if let Some(options) = crate::core::load_overlay_settings(&dir) {
            self.state_mtime = mtime;
            if options != self.options {
                self.apply_options(options);
            }
        }
    }

    /// Applies a new settings snapshot to the running overlay.
    fn apply_options(&mut self, options: OverlaySettings) {
        self.settings.font_size = f32::from(options.font_size);
        self.settings.super_chat_visible = options.superchat_enabled;
        self.pipeline = ChatPipeline::new_for_display(
            options.behavior.clone(),
            options.max_visible,
            danmaku_duration_secs(options.speed_percent),
        );
        self.rules = crate::danmaku_rules::compile_with_users(
            &options.behavior.blocked_keywords,
            &options.behavior.blocked_users,
            &options.behavior.highlight_keywords,
            &options.behavior.pin_keywords,
        );
        for error in &self.rules.errors {
            let line = format!("danmaku rule skipped: {error}");
            diag_log(&line);
            overlay_log(crate::diag::Level::Warn, &line);
        }
        self.highlighted_ids.clear();
        self.scheduler
            .set_capacity(usize::from(options.max_visible));
        self.scheduler
            .set_duration(danmaku_duration_secs(options.speed_percent) as f32);
        self.scheduler
            .set_line_gap_percent(options.line_gap_percent);
        self.scheduler.set_fixed_layout(
            options.behavior.top_fixed_zone_percent,
            options.behavior.bottom_fixed_zone_percent,
            options.behavior.max_fixed_per_mode,
        );
        self.reflow_pending = true;
        self.sc_scroll = 0.0;
        self.options = options;
        self.publish_options();
        self.feedback = "设置已更新".into();
        self.feedback_at = Instant::now();
    }

    /// Publishes the current options to the follow thread (window policy +
    /// configurable global hotkeys) and bumps the version it watches.
    fn publish_options(&mut self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.version = shared.version.wrapping_add(1);
            shared.options = self.options.clone();
        }
    }

    /// Persists the current overlay options into the isolated data directory
    /// (legacy hotkeys call SaveConfig). Best effort: the overlay keeps running
    /// with the new values even when the write fails.
    fn persist_options(&mut self) {
        let Some(dir) = self.state_dir.clone() else {
            return;
        };
        if let Ok(mut store) = crate::core::Store::open(dir) {
            if store.save_overlay_settings(self.options.clone()).is_ok() {
                // Keep the polled mtime in sync so our own write does not look
                // like an external change on the next tick.
                self.state_mtime =
                    std::fs::metadata(self.state_dir.as_ref().unwrap().join("state.json"))
                        .and_then(|metadata| metadata.modified())
                        .ok();
            }
        }
    }

    /// Handles the configurable legacy hotkey actions that need overlay state
    /// (display area / speed / delay / SC form / overlay visibility).
    fn apply_extended_hotkey(&mut self, hotkey: OverlayHotkey) {
        let message = match hotkey {
            OverlayHotkey::ToggleOverlay => {
                self.user_hidden = !self.user_hidden;
                if self.user_hidden {
                    self.super_chats.clear();
                    self.pending.clear();
                    clear_sc_hits();
                }
                if self.user_hidden {
                    "覆盖层已隐藏（Ctrl+Alt+O 恢复）".into()
                } else {
                    "覆盖层已显示".into()
                }
            }
            OverlayHotkey::CycleSuperChat => {
                self.options.sc_mode = match self.options.sc_mode {
                    ScMode::Full => ScMode::Compact,
                    ScMode::Compact => ScMode::Badge,
                    ScMode::Badge => ScMode::Full,
                    ScMode::Off => ScMode::Full,
                };
                self.settings.super_chat_visible = true;
                self.sc_scroll = 0.0;
                format!(
                    "SC：{}",
                    match self.options.sc_mode {
                        ScMode::Full => "完整卡片",
                        ScMode::Compact => "折叠成一行",
                        ScMode::Badge => "只留计数角标",
                        ScMode::Off => "关闭",
                    }
                )
            }
            OverlayHotkey::AreaLarger | OverlayHotkey::AreaSmaller => {
                let index = DISPLAY_AREA_STEPS
                    .iter()
                    .position(|step| *step == self.options.display_area_percent)
                    .unwrap_or(0);
                let next = if matches!(hotkey, OverlayHotkey::AreaLarger) {
                    index.saturating_sub(1)
                } else {
                    (index + 1).min(DISPLAY_AREA_STEPS.len() - 1)
                };
                self.options.display_area_percent = DISPLAY_AREA_STEPS[next];
                format!("弹幕显示区域：{}%", self.options.display_area_percent)
            }
            OverlayHotkey::SpeedFaster | OverlayHotkey::SpeedSlower => {
                let index = SPEED_STEPS
                    .iter()
                    .position(|step| *step == self.options.speed_percent)
                    .unwrap_or(2);
                let next = if matches!(hotkey, OverlayHotkey::SpeedFaster) {
                    (index + 1).min(SPEED_STEPS.len() - 1)
                } else {
                    index.saturating_sub(1)
                };
                self.options.speed_percent = SPEED_STEPS[next];
                self.scheduler
                    .set_duration(danmaku_duration_secs(self.options.speed_percent) as f32);
                self.reflow_pending = true;
                format!("弹幕速度：{}%", self.options.speed_percent)
            }
            OverlayHotkey::DelayLess | OverlayHotkey::DelayMore => {
                let current = self.options.behavior.delay_ms;
                let next = if matches!(hotkey, OverlayHotkey::DelayLess) {
                    current.saturating_sub(DELAY_STEP_MS)
                } else {
                    current.saturating_add(DELAY_STEP_MS).min(15_000)
                };
                self.options.behavior.delay_ms = next;
                self.pipeline.set_delay_ms(next);
                format!("弹幕延迟：{next} ms")
            }
            _ => return,
        };
        self.publish_options();
        self.persist_options();
        self.feedback = message;
        self.feedback_at = Instant::now();
    }

    /// Display label used for both measuring and painting, so the merge counter
    /// ( ×N) is always included in the width the scheduler reserves.
    fn display_label(&self, message: &danmaku::DanmakuMessage) -> String {
        let mut label = message_label(message, &self.options);
        let count = self.pipeline.count(&message.id);
        if count > 1 {
            label.push_str(&format!(" ×{count}"));
        }
        label
    }

    /// Legacy merge heat level: every doubling of the repeat count adds a step,
    /// capped at level 5 (danmaku-3-render.cs MergeLevel).
    /// Glyph scale contributed by the platform font size (server normally sends
    /// 25), clamped so a huge platform value cannot exceed one track row.
    fn message_font_scale(&self, message: &danmaku::DanmakuMessage) -> f32 {
        let platform = f32::from(message.font_size.max(1)) / 25.0;
        platform.clamp(0.6, 1.8)
    }

    /// Read every queued shortcut event. This only works while the overlay has
    /// keyboard focus; normal playback keeps the window NOACTIVATE so the player
    /// keeps focus and no global hotkey is registered.
    fn apply_hotkeys(&mut self, ctx: &egui::Context) {
        let mut hotkeys: Vec<OverlayHotkey> = self.native_hotkeys.try_iter().collect();
        ctx.input(|input| {
            for event in &input.events {
                if let egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    ..
                } = event
                {
                    if let Some(hotkey) = hotkey_for(*key) {
                        hotkeys.push(hotkey);
                    }
                }
            }
        });
        for hotkey in hotkeys {
            match hotkey {
                OverlayHotkey::FontSmaller
                | OverlayHotkey::FontLarger
                | OverlayHotkey::ToggleDanmaku
                | OverlayHotkey::ToggleSuperChat => {
                    let message = self.settings.apply(hotkey);
                    if !message.is_empty() {
                        self.feedback = message;
                        self.feedback_at = Instant::now();
                    }
                }
                other => self.apply_extended_hotkey(other),
            }
        }
    }

    fn drain_events(&mut self, now: f64) {
        // The receiver thread shares the room emoticon dictionary through the
        // process cache. It bumps the cache generation each time it publishes a
        // dictionary, so this painter picks up the first snapshot *and* later
        // replacements (another room or another platform) instead of freezing
        // the first one forever.
        let cache_version = danmaku::cached_emoticons_version();
        if emoticons_refresh_due(self.emoticons_version, cache_version) {
            if let Some(dictionary) = danmaku::cached_emoticons() {
                self.emoticons = dictionary;
                self.emoticons_version = cache_version;
            }
        }
        // Both a count and a time budget prevent bursts from monopolizing a frame.
        let deadline = Instant::now() + Duration::from_millis(2);
        for _ in 0..MAX_DANMAKU_PER_FRAME * 16 {
            if Instant::now() >= deadline {
                break;
            }
            match self.receiver.try_recv() {
                Ok(DanmakuEvent::Message(mut message)) => {
                    self.stats_received = self.stats_received.saturating_add(1);
                    let verdict = self.rules.verdict(&message);
                    if verdict.blocked {
                        self.stats_dropped = self.stats_dropped.saturating_add(1);
                        continue;
                    }
                    // Pinned messages become top fixed messages (legacy
                    // PrepareDanmaku), still subject to the ShowTop switch. The
                    // legacy colour rule is `highlight || item.Pinned`, and Pinned
                    // is only set while ShowTop is on, so a pin keyword must not
                    // colour messages when top danmaku are hidden.
                    let pinned = verdict.pinned && self.options.behavior.show_top;
                    if pinned {
                        message.mode = danmaku::DanmakuMode::Top;
                    }
                    let highlight = verdict.highlighted || pinned;
                    let admitted = self.pipeline.admit(message, now);
                    if admitted.is_some() {
                        self.stats_admitted = self.stats_admitted.saturating_add(1);
                    } else {
                        self.stats_dropped = self.stats_dropped.saturating_add(1);
                    }
                    if let Some(message) = admitted {
                        remember_highlight(
                            &mut self.highlighted_ids,
                            highlight,
                            &message.message.id,
                        );
                        if self.pending.len() < MAX_PENDING_DANMAKU {
                            self.pending.push(message);
                        }
                    }
                }
                Ok(DanmakuEvent::SuperChat(super_chat)) => {
                    let _ = self.super_chats.push_with_scale(
                        now,
                        &super_chat,
                        self.options.sc_duration_scale_percent,
                    );
                }
                Ok(DanmakuEvent::DeleteSuperChat(id)) => {
                    let _ = self.super_chats.remove(&id);
                }
                Ok(DanmakuEvent::Status(status)) => {
                    self.status = status;
                    self.status_at = Instant::now();
                }
                Ok(DanmakuEvent::Live(event)) => {
                    self.admit_live_event(event, now);
                }
                Err(_) => break,
            }
        }
        self.super_chats.prune(now);
        while self
            .live_events
            .front()
            .is_some_and(|(until, _)| *until <= now)
        {
            self.live_events.pop_front();
        }
        self.live_status.retain(|_, (_, until)| *until > now);
    }

    /// Legacy live-event admission: visibility switch, keyword/user filter,
    /// per-second rate limit (2/s default, guard/notice get one extra slot),
    /// event-id de-duplication (15 s), gift combo merging, and the merged
    /// watched/likes/rank status line.
    fn admit_live_event(&mut self, event: LiveEvent, now: f64) {
        self.stats_live_events = self.stats_live_events.saturating_add(1);
        if !live_event_visible(&event, &self.options.behavior)
            || !self.pipeline.allows(&event.user, &event.text)
            || self
                .rules
                .is_blocked(&rule_probe_message(&event.user, &event.text, 0))
        {
            return;
        }
        let lifetime = f64::from(self.options.behavior.event_duration_secs);
        if event.kind == LiveEventKind::Status {
            let topic = event
                .text
                .split_whitespace()
                .next()
                .unwrap_or("状态")
                .to_string();
            self.live_status
                .insert(topic, (truncate_chars(&event.text, 64), now + 30.0));
            if self.live_status.len() > 8 {
                if let Some(key) = self
                    .live_status
                    .iter()
                    .min_by(|a, b| a.1.1.total_cmp(&b.1.1))
                    .map(|(key, _)| key.clone())
                {
                    self.live_status.remove(&key);
                }
            }
            return;
        }
        if !event.id.is_empty() {
            if let Some(seen) = self.event_ids.get(&event.id) {
                if now - seen < 15.0 {
                    return;
                }
            }
            self.event_ids.insert(event.id.clone(), now);
            if self.event_ids.len() > 512 {
                self.event_ids.clear();
            }
        }
        let priority = matches!(event.kind, LiveEventKind::Guard | LiveEventKind::Notice);
        // Merge repeated gift/combo events into the visible row.
        let key = (
            event.kind,
            truncate_chars(&event.user, 32),
            truncate_chars(&event.text, 64),
        );
        for (until, existing) in self.live_events.iter_mut() {
            if (
                existing.kind,
                truncate_chars(&existing.user, 32),
                truncate_chars(&existing.text, 64),
            ) == key
                && *until > now
            {
                existing.count = existing.count.max(event.count);
                if event.count > 1 {
                    existing.count = existing.count.max(event.count);
                }
                let extended = (now + lifetime).min(*until + lifetime);
                *until = extended;
                return;
            }
        }
        // Per-second rate limit: the legacy renderer drops excess events.
        if now - self.event_window.0 >= 1.0 {
            self.event_window = (now, 0);
        }
        let limit =
            u16::from(self.options.behavior.event_rate_per_second.max(1)) + u16::from(priority);
        if u16::from(self.event_window.1) >= limit {
            return;
        }
        self.event_window.1 = self.event_window.1.saturating_add(1);
        let max_visible = usize::from(self.options.behavior.event_max_visible.max(1));
        if self.live_events.len() >= max_visible {
            if !priority {
                return;
            }
            self.live_events.pop_front();
        }
        self.live_events.push_back((now + lifetime, event));
    }

    /// Update lane geometry when the window size, DPI or font size changes.
    /// A size change re-fits the running messages and preserves their flight
    /// progress; a font/DPI change re-measures them on the next paint (see
    /// reflow_active). Neither path ever resets the layer, mirroring the legacy
    /// renderer.
    fn update_metrics(&mut self, rect: Rect, pixels_per_point: f32, now: f64) {
        let dpi = dpi_scale(pixels_per_point);
        let physical_height = if rect.height().is_finite() {
            rect.height() * dpi
        } else {
            0.0
        };
        let base_px = clamp_font_size(self.settings.font_size);
        let scaled_px = effective_font_px(
            base_px,
            physical_height,
            self.options.scale_reference_height,
            self.options.scale_with_window,
        );
        let font = (scaled_px / dpi).clamp(8.0, 96.0);
        let area_height = danmaku_area_points(
            rect.height(),
            self.options.display_area_percent,
            self.options.behavior.bottom_safe_percent,
        );
        let width = rect.width().max(1.0);
        let duration = danmaku_duration_secs(self.options.speed_percent) as f32;
        let margin = (12.0 / dpi).clamp(6.0, 20.0);
        // User safety areas are expressed as a percentage of the picture; the
        // lanes live inside the (smaller) danmaku area, so bands outside it are
        // simply never intersected.
        let mut bands = reserved_bands_from_areas(&self.options.reserved_areas, rect.height());
        if let Some(band) = self.event_band {
            bands.push(band);
        }
        // Fixed messages additionally keep clear of the real paid-message card
        // rectangle (horizontal + vertical); scrolling messages only pass under
        // the cards, like the legacy ExcludeClip behaviour.
        let _ = margin;
        self.scheduler.set_font(font);
        self.scheduler.set_duration(duration);
        self.scheduler
            .set_line_gap_percent(self.options.line_gap_percent);
        self.scheduler
            .set_capacity(usize::from(self.options.max_visible));
        self.scheduler.set_fixed_layout(
            self.options.behavior.top_fixed_zone_percent,
            self.options.behavior.bottom_fixed_zone_percent,
            self.options.behavior.max_fixed_per_mode,
        );
        self.scheduler.set_reserved_bands(bands);
        self.scheduler.set_fixed_obstacle(self.sc_obstacle);
        let next = SchedulerMetrics {
            font_points: font,
            width,
            height: area_height,
            dpi,
        };
        let font_changed = (self.metrics.font_points - next.font_points).abs() > 0.01;
        let resized = (self.metrics.width - next.width).abs() > 1.0
            || (self.metrics.height - next.height).abs() > 1.0
            || (self.metrics.dpi - next.dpi).abs() > 0.001;
        if font_changed {
            // Re-measure at paint time; the running messages keep their progress
            // and their preferred track. The viewport must still be synced first:
            // otherwise the scheduler keeps its construction-time width (640) and
            // new messages enter from the middle of the picture instead of the
            // right edge (user-visible "danmaku pop in mid-screen / never on the
            // right" bug).
            self.scheduler.set_viewport(width, area_height);
            self.reflow_pending = true;
        } else if resized {
            self.scheduler.refit(width, area_height, now);
        } else {
            // Defensive: a mismatch can only come from a first frame that took
            // the font-change path before the window was resized.
            let (scheduler_width, _) = self.scheduler.viewport();
            if (scheduler_width - width).abs() > 0.5 {
                self.scheduler.set_viewport(width, area_height);
            }
        }
        self.metrics = next;
    }

    /// Re-measures every running message with the current font and places it
    /// again at the same completed-flight fraction (legacy Resize keeps
    /// progress). Used after a font-size or DPI change.
    fn reflow_active(&mut self, painter: &egui::Painter, now: f64) {
        let previous = self.scheduler.take_active();
        let metrics = self.metrics;
        let show_images = self.options.behavior.show_images;
        let dictionary = show_images.then_some(&self.emoticons);
        let manifest = if self.options.show_color_emoji {
            self.color_emoji.as_ref().map(|sheet| &sheet.manifest)
        } else {
            None
        };
        for item in previous {
            let message = item.message.clone();
            let font_points =
                (metrics.font_points * item.scale * self.message_font_scale(&message)).max(6.0);
            let item_font = FontId::proportional(font_points);
            let label = self.display_label(&message);
            let rows = if matches!(
                message.mode,
                danmaku::DanmakuMode::Top | danmaku::DanmakuMode::Bottom
            ) {
                1
            } else {
                emote_rows(
                    show_images,
                    message.image_url.is_some(),
                    self.options.emote_size,
                    message.image_large,
                )
            };
            let width = if show_images && message.image_url.is_some() {
                let (image_width, _) = emoticon_size(
                    item_font.size,
                    effective_emote_size(self.options.emote_size, message.image_large),
                    message.image_width,
                    message.image_height,
                );
                image_width.max(item_font.size)
            } else {
                measure_label_width(
                    painter,
                    &label,
                    &item_font,
                    &self.options,
                    self.color_emoji.as_ref(),
                    dictionary,
                    manifest,
                )
            };
            let _ = self.scheduler.push_measured_progress(
                message,
                width + 8.0,
                rows,
                item.scale,
                item.progress(now),
                Some(item.lane),
                now,
            );
        }
        self.reflow_pending = false;
    }

    /// Crowded-screen budget: the legacy renderer derives it from the real track
    /// count (min(MaxOnScreen, trackCount*2)), so a short window shrinks earlier.
    fn crowded_budget(&self) -> usize {
        let tracks = self.scheduler.track_count();
        usize::from(self.options.max_visible).min(tracks.saturating_mul(2).max(1))
    }

    fn paint_danmaku(&mut self, painter: &egui::Painter, rect: Rect, now: f64) {
        if !self.settings.danmaku_visible {
            self.pending.clear();
            return;
        }
        let clipped = painter.with_clip_rect(Rect::from_min_size(
            rect.min,
            Vec2::new(self.metrics.width, self.metrics.height),
        ));
        let painter = &clipped;
        if self.reflow_pending {
            self.reflow_active(painter, now);
        }
        let base_font = self.metrics.font_points;
        let budget = self.crowded_budget();
        let active = self.scheduler.active().len();
        let due = self
            .pending
            .iter()
            .take_while(|message| message.release_at <= now)
            .count();
        let take = due.min(MAX_DANMAKU_PER_FRAME).min(
            usize::from(self.options.max_visible)
                .saturating_sub(active)
                .max(1),
        );
        let show_images = self.options.behavior.show_images;
        let max_size = (self.scheduler.line_height() / 1.35).max(8.0);
        if take > 0 {
            let batch: Vec<_> = self.pending.drain(..take).collect();
            for pending in batch {
                let mut message = pending.message;
                let platform = self.message_font_scale(&message);
                let priority = message.guard_level > 0;
                let crowd = if priority {
                    1.0
                } else {
                    crowd_scale(
                        self.scheduler.active().len(),
                        budget,
                        self.options.behavior.shrink_when_crowded,
                        self.options.behavior.crowded_scale_percent,
                    )
                };
                let font_points = (base_font * platform * crowd).clamp(6.0, max_size);
                let effective = font_points / base_font.max(1.0);
                let item_font = FontId::proportional(font_points);
                let dictionary = show_images.then_some(&self.emoticons);
                let manifest = if self.options.show_color_emoji {
                    self.color_emoji.as_ref().map(|sheet| &sheet.manifest)
                } else {
                    None
                };
                let image_rows = emote_rows(
                    show_images,
                    message.image_url.is_some(),
                    self.options.emote_size,
                    message.image_large,
                );
                let prefix = message_prefix(&message, &self.options);
                let width = if show_images && message.image_url.is_some() {
                    let (image_width, _) = emoticon_size(
                        font_points,
                        effective_emote_size(self.options.emote_size, message.image_large),
                        message.image_width,
                        message.image_height,
                    );
                    image_width.max(font_points)
                        + measure_label_width(
                            painter,
                            &prefix,
                            &item_font,
                            &self.options,
                            self.color_emoji.as_ref(),
                            dictionary,
                            manifest,
                        )
                } else {
                    let label = self.display_label(&message);
                    measure_label_width(
                        painter,
                        &label,
                        &item_font,
                        &self.options,
                        self.color_emoji.as_ref(),
                        dictionary,
                        manifest,
                    )
                };
                // Merged messages grow while on screen; reserve room for the
                // counter so a growing "×N" cannot push into a neighbour.
                let counter = if self.options.behavior.merge_duplicates {
                    item_font.size * 2.2
                } else {
                    0.0
                };
                let scheduled_width = (width + 8.0 + counter).max(1.0);
                message.mode = effective_mode(
                    message.mode,
                    scheduled_width,
                    self.metrics.width,
                    self.options.behavior.fixed_overflow_to_scroll,
                );
                let rows = if matches!(
                    message.mode,
                    danmaku::DanmakuMode::Top | danmaku::DanmakuMode::Bottom
                ) {
                    1
                } else {
                    image_rows
                };
                let _ = self.scheduler.push_measured_scaled(
                    message,
                    scheduled_width,
                    rows,
                    effective.clamp(0.3, 1.8),
                    now,
                );
            }
        }
        let items: Vec<danmaku::FlyingDanmaku> = self.scheduler.active().to_vec();
        let dictionary = show_images.then_some(&self.emoticons);
        let manifest = if self.options.show_color_emoji {
            self.color_emoji.as_ref().map(|sheet| &sheet.manifest)
        } else {
            None
        };
        for item in &items {
            let repeat = self.pipeline.count(&item.message.id);
            let mut merge = 1.0_f32;
            if repeat > 1 && self.options.behavior.enlarge_merged {
                // Only use the width reserved for the counter so growth cannot
                // push into a neighbour (the legacy renderer re-lays out instead).
                let base = (base_font * item.scale).max(1.0);
                let reserved = base * 2.2;
                let cap = 1.0 + reserved / item.width.max(1.0);
                merge = merge_scale(repeat, self.options.behavior.merged_max_scale_percent)
                    .min(cap)
                    .max(1.0);
            }
            let font_points = (base_font * item.scale * merge).clamp(6.0, 96.0);
            let item_font = FontId::proportional(font_points);
            let highlighted = self.highlighted_ids.contains(&item.message.id);
            let base_color = if highlighted {
                color_with_alpha(self.options.behavior.highlight_color, 255)
            } else {
                message_color(item.message.color)
            };
            let color = apply_text_opacity(base_color, self.options.text_opacity_percent);
            let text_height = font_points * 1.2;
            let top = rect.top() + item.y + ((item.height - text_height) / 2.0).max(0.0);
            let left = rect.left() + item.x(now);
            let mut painted_image = false;
            if show_images {
                if let Some(url) = &item.message.image_url {
                    if let Some(texture) = request_image_texture(
                        painter,
                        &mut self.image_textures,
                        &self.image_loader,
                        url,
                    ) {
                        let (_, height) = emoticon_size(
                            font_points,
                            effective_emote_size(self.options.emote_size, item.message.image_large),
                            item.message.image_width,
                            item.message.image_height,
                        );
                        let width = (height * texture.aspect_ratio()).min(font_points * 8.0);
                        painter.image(
                            texture.id(),
                            Rect::from_min_size(Pos2::new(left, top), Vec2::new(width, height)),
                            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                            Color32::WHITE,
                        );
                        painted_image = true;
                        let prefix = message_prefix(&item.message, &self.options);
                        if !prefix.is_empty() {
                            let outline = danmaku_outline(&self.options, item_font.size);
                            paint_label_segments(
                                painter,
                                Pos2::new(left + width, top),
                                &prefix,
                                &item_font,
                                color,
                                &self.options,
                                &outline,
                                self.color_emoji.as_ref(),
                                dictionary,
                                manifest,
                                &mut self.image_textures,
                                &self.image_loader,
                            );
                        }
                    }
                }
            }
            if !painted_image {
                let label = self.display_label(&item.message);
                let outline = danmaku_outline(&self.options, item_font.size);
                paint_label_segments(
                    painter,
                    Pos2::new(left, top),
                    &label,
                    &item_font,
                    color,
                    &self.options,
                    &outline,
                    self.color_emoji.as_ref(),
                    dictionary,
                    manifest,
                    &mut self.image_textures,
                    &self.image_loader,
                );
            }
        }
        let live_ids: std::collections::BTreeSet<&str> = self
            .scheduler
            .active()
            .iter()
            .map(|item| item.message.id.as_str())
            .chain(self.pending.iter().map(|item| item.message.id.as_str()))
            .collect();
        self.pipeline.retain(live_ids.iter().copied());
        if self.highlighted_ids.len() > live_ids.len() {
            self.highlighted_ids
                .retain(|id| live_ids.contains(id.as_str()));
        }
    }

    /// Applies a pending native click/wheel against the last painted SC layout.
    fn drain_sc_interaction(&mut self, pixels_per_point: f32) {
        let (click, wheel, drag) = {
            let Ok(mut state) = self.interaction.lock() else {
                return;
            };
            (
                state.click.take(),
                std::mem::take(&mut state.wheel),
                state.bar_drag,
            )
        };
        let scale = if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
            pixels_per_point
        } else {
            1.0
        };
        // Scrollbar drag: map the thumb position back to the pixel scroll offset.
        if let (Some((grab_offset, y)), Some(track), Some(thumb)) =
            (drag, self.sc_bar_track, self.sc_bar_thumb)
        {
            self.sc_scroll = sc_bar_scroll_for(
                y / scale - grab_offset / scale,
                0.0,
                track.top(),
                track.height(),
                thumb.height(),
                self.sc_max_scroll,
            );
            return;
        }
        let mode = effective_sc_mode(self.settings.super_chat_visible, self.options.sc_mode);
        if wheel != 0.0 && matches!(mode, ScMode::Full | ScMode::Compact) {
            self.sc_scroll = sc_scroll_after(self.sc_scroll, wheel, self.sc_max_scroll);
        }
        let Some((x, y)) = click else {
            return;
        };
        if !self.options.sc_clickable {
            return;
        }
        let point = Pos2::new(x / scale, y / scale);
        let rects: Vec<ScCardRect> = self.sc_cards.iter().map(|(rect, _)| *rect).collect();
        match resolve_sc_click(&rects, point, self.sc_area) {
            ScClick::Close(index) => {
                if let Some((_, id)) = self.sc_cards.get(index) {
                    if !id.is_empty() {
                        let _ = self.super_chats.remove(id);
                    }
                }
            }
            ScClick::Toggle(_) => {
                // Legacy widget cycles the display form on click. The legacy
                // order ends with "off"; this build cycles full -> compact ->
                // badge -> full so a click can never leave an invisible state
                // that only a settings round-trip can undo.
                self.options.sc_mode = match self.options.sc_mode {
                    ScMode::Full => ScMode::Compact,
                    ScMode::Compact => ScMode::Badge,
                    ScMode::Badge => ScMode::Full,
                    ScMode::Off => ScMode::Full,
                };
                self.settings.super_chat_visible = true;
                self.sc_scroll = 0.0;
            }
            ScClick::None => {}
        }
    }

    /// Surface alpha for paid-message fills: solid-text mode keeps glyphs
    /// opaque and only the card surfaces use the opacity setting.
    fn sc_surface_alpha(&self) -> u8 {
        surface_alpha(self.options.opacity_percent, self.options.solid_text)
    }

    /// Legacy close button: translucent round background with a white cross.
    fn paint_sc_close(painter: &egui::Painter, rect: Rect, _scale: f32) {
        if !rect.is_positive() {
            return;
        }
        painter.circle_filled(
            rect.center(),
            rect.width() / 2.0,
            Color32::from_black_alpha(90),
        );
        let pad = rect.width() * 0.32;
        let stroke = Stroke::new(
            (rect.width() * 0.12).max(1.0),
            Color32::from_white_alpha(235),
        );
        painter.line_segment(
            [
                Pos2::new(rect.left() + pad, rect.top() + pad),
                Pos2::new(rect.right() - pad, rect.bottom() - pad),
            ],
            stroke,
        );
        painter.line_segment(
            [
                Pos2::new(rect.right() - pad, rect.top() + pad),
                Pos2::new(rect.left() + pad, rect.bottom() - pad),
            ],
            stroke,
        );
    }

    /// Truncates one text run to max_width using the same measurement as the
    /// painter, appending an ellipsis when characters had to be dropped.
    fn truncate_to_width(
        &self,
        painter: &egui::Painter,
        text: &str,
        font: &FontId,
        max_width: f32,
    ) -> String {
        if max_width <= 0.0 {
            return String::new();
        }
        if self.sc_text_width(painter, text, font) <= max_width {
            return text.to_string();
        }
        let mut out = String::new();
        for character in text.chars() {
            out.push(character);
            if self.sc_text_width(painter, &out, font) > max_width {
                out.pop();
                out.push('…');
                break;
            }
        }
        if out.is_empty() { "…".into() } else { out }
    }

    /// Width of one paid-message text fragment, measured with the same segment
    /// renderer that draws it (colour emoji cells and platform emoticon images
    /// included), so wrapping and painting agree.
    fn sc_text_width(&self, painter: &egui::Painter, text: &str, font: &FontId) -> f32 {
        let show_images = self.options.behavior.show_images;
        let manifest = if self.options.show_color_emoji {
            self.color_emoji.as_ref().map(|sheet| &sheet.manifest)
        } else {
            None
        };
        measure_label_width(
            painter,
            text,
            font,
            &self.options,
            self.color_emoji.as_ref(),
            show_images.then_some(&self.emoticons),
            manifest,
        )
    }

    /// Wraps paid-message body text into at most max_lines lines measured with
    /// the same painter that draws it, so emoji/emoticon widths agree.
    fn wrap_sc_lines(
        &self,
        painter: &egui::Painter,
        text: &str,
        font: &FontId,
        width: f32,
        max_lines: usize,
    ) -> Vec<String> {
        let flat = text.replace('\r', "").replace('\n', " ");
        if flat.trim().is_empty() {
            return Vec::new();
        }
        let max_width = width.max(8.0);
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();
        for character in flat.chars() {
            current.push(character);
            let measured = self.sc_text_width(painter, &current, font);
            if measured >= max_width {
                lines.push(std::mem::take(&mut current));
                if lines.len() >= max_lines {
                    if let Some(last) = lines.last_mut() {
                        last.push('…');
                    }
                    return lines;
                }
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
        lines
    }
    /// Avatar: platform square image masked into a circle by the texture loader;
    /// falls back to a white circle with the first character, like the legacy
    /// widget. Images are downloaded through the existing bounded pipeline.
    fn paint_sc_avatar(&mut self, painter: &egui::Painter, rect: Rect, card: &ScCard) {
        let full_uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
        let mut painted = false;
        if !card.face.is_empty() {
            if let Some(texture) = request_image_texture_round(
                painter,
                &mut self.image_textures,
                &self.image_loader,
                &card.face,
            ) {
                painter.image(texture.id(), rect, full_uv, Color32::WHITE);
                painted = true;
            }
        }
        if painted {
            // Optional B站 avatar frame (face_frame), drawn on top of the avatar
            // at 34/30 of the avatar box, like the legacy widget.
            if !card.face_frame.is_empty() {
                if let Some(frame) = request_image_texture(
                    painter,
                    &mut self.image_textures,
                    &self.image_loader,
                    &card.face_frame,
                ) {
                    let frame_size = rect.width() * (34.0 / 30.0);
                    let frame_rect = Rect::from_center_size(rect.center(), Vec2::splat(frame_size));
                    painter.image(frame.id(), frame_rect, full_uv, Color32::WHITE);
                }
            }
            return;
        }
        painter.circle_filled(
            rect.center(),
            rect.width() / 2.0,
            Color32::from_white_alpha(235),
        );
        let _ = full_uv;
        let initial = card
            .user
            .chars()
            .next()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".into());
        let font = FontId::proportional((rect.width() * 0.5).max(8.0));
        let galley = painter.layout_no_wrap(
            initial,
            font,
            color_with_alpha(card.background_bottom_color, 255),
        );
        painter.galley(
            rect.center() - galley.size() / 2.0,
            galley,
            color_with_alpha(card.background_bottom_color, 255),
        );
    }

    /// Body text + optional platform translation lines for one full card, plus
    /// the resulting content-band height. Shared by the height measurement pass
    /// and the painter so both always agree.
    fn sc_full_content(
        &self,
        painter: &egui::Painter,
        card: &ScCard,
        scale: f32,
    ) -> (Vec<String>, Vec<String>, f32) {
        let padx = 10.0 * scale;
        let pady = 8.0 * scale;
        let line_height = 20.0 * scale;
        let avatar = if self.options.sc_show_avatar {
            30.0 * scale
        } else {
            0.0
        };
        let text_x = padx
            + if avatar > 0.0 {
                avatar + 6.0 * scale
            } else {
                0.0
            };
        let text_width = (sc_card_width_points(scale) - padx - text_x).max(30.0);
        let body_font = FontId::proportional((12.0 * scale).max(6.0));
        let lines = self.wrap_sc_lines(painter, &card.text, &body_font, text_width, 8);
        let trans_font = FontId::proportional((10.8 * scale).max(6.0));
        let trans_lines = if card.message_trans.trim().is_empty() {
            Vec::new()
        } else {
            self.wrap_sc_lines(painter, &card.message_trans, &trans_font, text_width, 4)
        };
        let rows = lines.len().max(1) + trans_lines.len();
        let content_height = (rows as f32 * line_height + pady * 2.0).max(38.0 * scale);
        (lines, trans_lines, content_height)
    }

    /// Full paid-message card, mirroring the B站 full-screen widget: a header
    /// band (background_color) with the circular avatar, name and price, a
    /// content band (background_bottom_color) with the message text, and a
    /// white progress track below the card.
    fn paint_sc_full(
        &mut self,
        painter: &egui::Painter,
        card: &ScCard,
        rect: Rect,
        scale: f32,
        now: f64,
    ) {
        let alpha = self.sc_surface_alpha();
        let padx = 10.0 * scale;
        let pady = 8.0 * scale;
        let top_height = 50.0 * scale;
        let line_height = 20.0 * scale;
        let avatar = if self.options.sc_show_avatar {
            30.0 * scale
        } else {
            0.0
        };
        let text_x = rect.left()
            + padx
            + if avatar > 0.0 {
                avatar + 6.0 * scale
            } else {
                0.0
            };
        let body_size = (12.0 * scale).max(6.0);
        let body_font = FontId::proportional(body_size);
        let (lines, trans_lines, content_height) = self.sc_full_content(painter, card, scale);
        let _ = text_x;
        let radius = egui::CornerRadius::same((6.0 * scale).round().clamp(1.0, 24.0) as u8);
        // ---- header band ----
        let header = Rect::from_min_size(rect.min, Vec2::new(rect.width(), top_height));
        painter.rect_filled(
            header,
            radius,
            color_with_alpha(card.background_color, alpha),
        );
        painter.rect_stroke(
            header,
            radius,
            Stroke::new(
                1.0_f32.max(scale),
                color_with_alpha(card.background_bottom_color, 255),
            ),
            StrokeKind::Inside,
        );
        if avatar > 0.0 {
            let avatar_rect = Rect::from_min_size(
                Pos2::new(rect.left() + padx, rect.top() + (top_height - avatar) / 2.0),
                Vec2::splat(avatar),
            );
            self.paint_sc_avatar(painter, avatar_rect, card);
        }
        let name_color = readable_text_color(card.name_color, card.background_color);
        let price_color = readable_text_color(0x333333, card.background_color);
        let name_font = FontId::proportional(body_size);
        let name = truncate_chars(&card.user, SC_USER_MAX_CHARS);
        let name_galley = painter.layout_no_wrap(name, name_font.clone(), name_color);
        painter.galley(
            Pos2::new(text_x, rect.top() + pady + 2.0 * scale),
            name_galley,
            name_color,
        );
        let price_galley = painter.layout_no_wrap(
            price_label(card.price_cents),
            name_font.clone(),
            price_color,
        );
        let name_height = painter
            .layout_no_wrap(
                truncate_chars(&card.user, SC_USER_MAX_CHARS),
                name_font.clone(),
                name_color,
            )
            .size()
            .y;
        painter.galley(
            Pos2::new(
                text_x,
                rect.top() + pady + 2.0 * scale + name_height + 2.0 * scale,
            ),
            price_galley,
            price_color,
        );
        // ---- content band (only the bottom corners are rounded) ----
        let content = Rect::from_min_size(
            Pos2::new(rect.left(), rect.top() + top_height),
            Vec2::new(rect.width(), content_height),
        );
        let content_radius = egui::CornerRadius {
            nw: 0,
            ne: 0,
            sw: radius.sw,
            se: radius.se,
        };
        painter.rect_filled(
            content,
            content_radius,
            color_with_alpha(card.background_bottom_color, alpha),
        );
        let body_color = color_with_alpha(card.message_font_color, 255);
        let show_images = self.options.behavior.show_images;
        let dictionary = show_images.then_some(&self.emoticons);
        let manifest = if self.options.show_color_emoji {
            self.color_emoji.as_ref().map(|sheet| &sheet.manifest)
        } else {
            None
        };
        for (index, line) in lines.iter().enumerate() {
            paint_label_segments(
                painter,
                Pos2::new(
                    rect.left() + padx,
                    rect.top() + top_height + pady + index as f32 * line_height,
                ),
                line,
                &body_font,
                body_color,
                &self.options,
                &[],
                self.color_emoji.as_ref(),
                dictionary,
                manifest,
                &mut self.image_textures,
                &self.image_loader,
            );
        }
        if !trans_lines.is_empty() {
            let trans_font = FontId::proportional((10.8 * scale).max(6.0));
            let trans_color = Color32::from_rgba_unmultiplied(
                body_color.r(),
                body_color.g(),
                body_color.b(),
                200,
            );
            for (index, line) in trans_lines.iter().enumerate() {
                paint_label_segments(
                    painter,
                    Pos2::new(
                        rect.left() + padx,
                        rect.top()
                            + top_height
                            + pady
                            + (lines.len().max(1) + index) as f32 * line_height,
                    ),
                    line,
                    &trans_font,
                    trans_color,
                    &self.options,
                    &[],
                    self.color_emoji.as_ref(),
                    dictionary,
                    manifest,
                    &mut self.image_textures,
                    &self.image_loader,
                );
            }
        }
        // ---- progress track below the card ----
        let track_y = rect.top() + top_height + content_height + 4.0 * scale;
        let track_height = 4.0 * scale;
        let track = Rect::from_min_size(
            Pos2::new(rect.left(), track_y),
            Vec2::new(rect.width(), track_height),
        );
        painter.rect_filled(
            track,
            egui::CornerRadius::same((track_height / 2.0).round().max(1.0) as u8),
            Color32::WHITE,
        );
        let ratio = sc_progress_ratio(card.created, card.expires, now);
        if ratio > 0.001 {
            let fill = Rect::from_min_size(
                track.min,
                Vec2::new((track.width() * ratio).max(1.0), track.height()),
            );
            painter.rect_filled(
                fill,
                egui::CornerRadius::same((track_height / 2.0).round().max(1.0) as u8),
                color_with_alpha(card.background_bottom_color, alpha),
            );
        }
    }

    /// Compact paid-message card: a single 36*scale row with the circular
    /// avatar, a bold-ish "name price元" head and the truncated message body.
    fn paint_sc_compact(&mut self, painter: &egui::Painter, card: &ScCard, rect: Rect, scale: f32) {
        let alpha = self.sc_surface_alpha();
        let height = 36.0 * scale;
        let radius = egui::CornerRadius::same((6.0 * scale).round().clamp(1.0, 24.0) as u8);
        let card_rect = Rect::from_min_size(rect.min, Vec2::new(rect.width(), height));
        painter.rect_filled(
            card_rect,
            radius,
            color_with_alpha(card.background_color, alpha),
        );
        painter.rect_stroke(
            card_rect,
            radius,
            Stroke::new(
                1.0_f32.max(scale),
                color_with_alpha(card.background_bottom_color, 255),
            ),
            StrokeKind::Inside,
        );
        let pad = 6.0 * scale;
        let mut content_left = rect.left() + pad;
        if self.options.sc_show_avatar {
            let avatar = height - pad * 2.0;
            let avatar_rect = Rect::from_min_size(
                Pos2::new(content_left, rect.top() + pad),
                Vec2::splat(avatar),
            );
            self.paint_sc_avatar(painter, avatar_rect, card);
            content_left += avatar + pad;
        }
        let font_size = (12.0 * scale).max(6.0);
        let font = FontId::proportional(font_size);
        let name_color = readable_text_color(card.name_color, card.background_color);
        let body_color = readable_text_color(card.background_bottom_color, card.background_color);
        // Reserve room for the × close button so neither head nor body can slide
        // underneath it.
        let close_reserve = if self.options.sc_clickable {
            16.0 * scale + 6.0 * scale + 4.0 * scale
        } else {
            2.0 * scale
        };
        let right_limit = rect.right() - pad - close_reserve;
        let available = (right_limit - content_left).max(8.0);
        let price = price_label(card.price_cents);
        // The head keeps its price; the name shrinks (with ellipsis) when needed.
        let mut user = truncate_chars(&card.user, SC_USER_MAX_CHARS);
        let head_for = |name: &str| format!("{name} {price} ");
        if self.sc_text_width(painter, &head_for(&user), &font) > available * 0.6 {
            let budget = (available * 0.6 - self.sc_text_width(painter, &head_for(""), &font))
                .max(font_size * 2.0);
            user = self.truncate_to_width(painter, &user, &font, budget);
        }
        let head = head_for(&user);
        let head_galley = painter.layout_no_wrap(head.clone(), font.clone(), name_color);
        let line_y = rect.top() + (height - head_galley.size().y) / 2.0;
        let head_width = head_galley.size().x;
        painter.galley(Pos2::new(content_left, line_y), head_galley, name_color);
        let room = (right_limit - content_left - head_width).max(0.0);
        if room > font_size * 1.5 {
            let body_text = card.text.replace('\n', " ");
            let body = self.truncate_to_width(painter, &body_text, &font, room - 2.0);
            let body_galley = painter.layout_no_wrap(body, font, body_color);
            painter.galley(
                Pos2::new(content_left + head_width, line_y),
                body_galley,
                body_color,
            );
        }
    }

    /// Badge form: a content-sized pill showing
    /// "SC ×N  <first 12 characters of the newest message>", anchored to the
    /// card's right edge (left edge for the left corners) with the platform
    /// card colours. Returns the painted rect for hit testing.
    fn paint_sc_badge(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        scale: f32,
        px_to_points: f32,
    ) -> Option<Rect> {
        let newest = self.super_chats.cards().last()?;
        let count = self.super_chats.cards().len();
        let text = format!(
            "SC ×{count}  {}",
            truncate_chars(&newest.text, SC_BADGE_PREVIEW_CHARS)
        );
        let font = FontId::proportional((13.0 * scale).max(6.0));
        let color = color_with_alpha(newest.message_font_color, 255);
        let galley = painter.layout_no_wrap(truncate_chars(&text, 40), font, color);
        let mut badge = sc_badge_rect(
            rect,
            self.options.sc_corner,
            scale,
            galley.size().x,
            galley.size().y,
            px_to_points,
        );
        // Snap to whole physical pixels: rounding the content-sized pill in
        // points can leave it 1-3 px short of the card's right edge.
        if px_to_points.is_finite() && px_to_points > 0.0 {
            let ppp = 1.0 / px_to_points;
            let width = (badge.width() * ppp).round().max(1.0) / ppp;
            let right = (badge.right() * ppp).round() / ppp;
            let height = (badge.height() * ppp).round().max(1.0) / ppp;
            let top = (badge.top() * ppp).round() / ppp;
            badge = Rect::from_min_size(Pos2::new(right - width, top), Vec2::new(width, height));
        }
        let radius = egui::CornerRadius::same((badge.height() / 2.0).round().max(1.0) as u8);
        painter.rect_filled(
            badge,
            radius,
            color_with_alpha(newest.background_bottom_color, 255),
        );
        painter.galley(
            Pos2::new(
                badge.left() + 10.0 * scale,
                badge.center().y - galley.size().y / 2.0,
            ),
            galley,
            color,
        );
        Some(badge)
    }

    fn paint_super_chats(
        &mut self,
        painter: &egui::Painter,
        rect: Rect,
        pixels_per_point: f32,
        now: f64,
    ) {
        let mode = effective_sc_mode(self.settings.super_chat_visible, self.options.sc_mode);
        if mode == ScMode::Off || self.super_chats.cards().is_empty() {
            clear_sc_hits();
            self.sc_cards.clear();
            self.sc_scroll = 0.0;
            self.sc_max_scroll = 0.0;
            self.sc_band = None;
            self.sc_obstacle = None;
            self.sc_bar_track = None;
            self.sc_bar_thumb = None;
            publish_sc_bar(None, None, pixels_per_point);
            return;
        }
        let scale = sc_points_scale(
            self.options.sc_scale_percent,
            rect.height(),
            self.options.scale_reference_height,
            self.options.scale_with_window,
        );
        let px_to_points = 1.0 / dpi_scale(pixels_per_point);
        if mode == ScMode::Badge {
            let Some(badge) = self.paint_sc_badge(painter, rect, scale, px_to_points) else {
                clear_sc_hits();
                return;
            };
            self.sc_area = rect;
            self.sc_cards = vec![(
                ScCardRect {
                    rect: badge,
                    close: Rect::NOTHING,
                },
                String::new(),
            )];
            self.sc_band = Some((badge.top() - rect.top(), badge.bottom() - rect.top()));
            self.sc_obstacle = Some((
                badge.top() - rect.top(),
                badge.bottom() - rect.top(),
                badge.left() - rect.left(),
                badge.right() - rect.left(),
            ));
            self.sc_bar_track = None;
            self.sc_bar_thumb = None;
            publish_sc_bar(None, None, pixels_per_point);
            let hits = vec![(badge, Rect::NOTHING, String::new())];
            publish_sc_hits(&hits, pixels_per_point, rect, self.options.sc_clickable);
            return;
        }
        // Newest first: the legacy stack paints the newest card closest to the
        // corner and pushes older cards away from it.
        let cards: Vec<ScCard> = self.super_chats.cards().iter().rev().cloned().collect();
        let body_size = (12.0 * scale).max(6.0);
        let body_font = FontId::proportional(body_size);
        let line_height = 20.0 * scale;
        let padx = 10.0 * scale;
        let pady = 8.0 * scale;
        let avatar = if self.options.sc_show_avatar {
            30.0 * scale
        } else {
            0.0
        };
        let text_x = padx
            + if avatar > 0.0 {
                avatar + 6.0 * scale
            } else {
                0.0
            };
        let text_width = (sc_card_width_points(scale) - padx - text_x).max(30.0);
        let heights: Vec<f32> = cards
            .iter()
            .map(|card| {
                if mode == ScMode::Compact {
                    36.0 * scale
                } else {
                    let (_, _, content_height) = self.sc_full_content(painter, card, scale);
                    50.0 * scale + content_height + 4.0 * scale + 4.0 * scale + 10.0 * scale
                }
            })
            .collect();
        let layout = sc_layout(
            rect,
            self.options.sc_corner,
            scale,
            &heights,
            self.options.sc_max_stack,
            self.sc_scroll,
            px_to_points,
        );
        self.sc_area = layout.view;
        self.sc_max_scroll = layout.max_scroll;
        self.sc_scroll = layout.scroll;
        let clickable = self.options.sc_clickable;
        let scrollable = layout.max_scroll > 0.5;
        let clipped = painter.with_clip_rect(layout.view);
        let painter = &clipped;
        let mut hits: Vec<(Rect, Rect, String)> = Vec::new();
        for (index, card) in cards.iter().enumerate() {
            let card_rect = layout.cards[index].rect;
            if !card_rect.intersects(layout.view) {
                continue;
            }
            if mode == ScMode::Compact {
                self.paint_sc_compact(painter, card, card_rect, scale);
            } else {
                self.paint_sc_full(painter, card, card_rect, scale, now);
            }
            let close = if clickable {
                Self::paint_sc_close(painter, layout.cards[index].close, scale);
                layout.cards[index].close
            } else {
                Rect::NOTHING
            };
            hits.push((card_rect, close, card.id.clone()));
        }
        let dragging = self
            .interaction
            .lock()
            .map(|state| state.bar_drag.is_some())
            .unwrap_or(false);
        if let (Some(track), Some(thumb)) = (layout.scrollbar, layout.thumb) {
            painter.rect_filled(
                track,
                egui::CornerRadius::same(2),
                Color32::from_white_alpha(70),
            );
            painter.rect_filled(
                thumb,
                egui::CornerRadius::same(2),
                Color32::from_white_alpha(if dragging { 235 } else { 190 }),
            );
        }
        self.sc_bar_track = layout.scrollbar;
        self.sc_bar_thumb = layout.thumb;
        publish_sc_bar(layout.scrollbar, layout.thumb, pixels_per_point);
        self.sc_cards = cards
            .iter()
            .enumerate()
            .filter(|(index, _)| layout.cards[*index].rect.intersects(layout.view))
            .map(|(index, card)| (layout.cards[index], card.id.clone()))
            .collect();
        self.sc_band = Some((
            (layout.view.top() - rect.top()).max(0.0),
            (layout.view.bottom() - rect.top()).max(0.0),
        ));
        let mut obstacle_left = f32::INFINITY;
        let mut obstacle_right = f32::NEG_INFINITY;
        for hit in &hits {
            obstacle_left = obstacle_left.min(hit.0.left() - rect.left());
            obstacle_right = obstacle_right.max(hit.0.right() - rect.left());
        }
        self.sc_obstacle =
            (obstacle_left.is_finite() && obstacle_right > obstacle_left).then_some((
                (layout.view.top() - rect.top()).max(0.0),
                (layout.view.bottom() - rect.top()).max(0.0),
                obstacle_left,
                obstacle_right,
            ));
        publish_sc_hits(
            &hits,
            pixels_per_point,
            layout.view,
            clickable || scrollable,
        );
    }

    /// Window scale used by the live-event strip (legacy _uiScale): picture
    /// height over the reference height, or 1.0 at a fixed logical size.
    fn event_ui_scale(&self, rect: Rect) -> f32 {
        sc_points_scale(
            100,
            rect.height(),
            self.options.scale_reference_height,
            self.options.scale_with_window,
        )
        .clamp(0.45, 4.5)
    }

    /// Viewport rectangle of the upper-left live-event strip. It avoids the
    /// paid-message cards and the user safety areas, like the legacy
    /// MeasureEventReservation search.
    fn event_band_rect(&self, rect: Rect) -> Option<Rect> {
        if rect.width() < 40.0 || rect.height() < 40.0 {
            return None;
        }
        let event_rows = self.live_events.len();
        let status_rows = self.live_status.len().min(1);
        if event_rows == 0 && status_rows == 0 {
            return None;
        }
        let ui = self.event_ui_scale(rect);
        let width = (rect.width() - 20.0 * ui).min(350.0 * ui).max(80.0);
        let height = (event_rows as f32 * 30.0 + status_rows as f32 * 24.0) * ui;
        if height > rect.height() {
            return None;
        }
        let obstacle = self.sc_obstacle.map(|(y0, y1, x0, x1)| {
            (
                rect.top() + y0,
                rect.top() + y1,
                rect.left() + x0,
                rect.left() + x1,
            )
        });
        let reserved: Vec<Rect> = self
            .options
            .reserved_areas
            .iter()
            .map(|area| {
                Rect::from_min_size(
                    Pos2::new(
                        rect.left() + f32::from(area.x) * rect.width() / 100.0,
                        rect.top() + f32::from(area.y) * rect.height() / 100.0,
                    ),
                    Vec2::new(
                        f32::from(area.width) * rect.width() / 100.0,
                        f32::from(area.height) * rect.height() / 100.0,
                    ),
                )
            })
            .collect();
        let step = (8.0 * ui).max(4.0);
        let mut y = rect.top() + 10.0 * ui;
        while y + height <= rect.bottom() {
            let candidate = Rect::from_min_size(
                Pos2::new(rect.left() + 10.0 * ui, y),
                Vec2::new(width, height),
            );
            let blocked_by_sc = obstacle.is_some_and(|(top, bottom, left, right)| {
                candidate.top() < bottom
                    && candidate.bottom() > top
                    && candidate.left() < right
                    && candidate.right() > left
            });
            let blocked_by_area = reserved.iter().any(|area| area.intersects(candidate));
            if !blocked_by_sc && !blocked_by_area {
                return Some(candidate);
            }
            y += step;
        }
        None
    }

    /// Legacy live-event strip: upper-left rows with the dark translucent
    /// background, a white 13*ui label per event, and the merged room-status
    /// line at the bottom of the strip.
    fn paint_live_events(&self, painter: &egui::Painter, rect: Rect) {
        let Some(band) = self.event_band_rect(rect) else {
            return;
        };
        let ui = self.event_ui_scale(rect);
        let background = Color32::from_rgba_unmultiplied(22, 35, 52, 180);
        let row_height = 28.0 * ui;
        let row_gap = 2.0 * ui;
        let font = FontId::proportional((13.0 * ui).max(8.0));
        let mut y = band.top();
        for (_, event) in self.live_events.iter() {
            let row = Rect::from_min_size(
                Pos2::new(band.left(), y),
                Vec2::new(band.width(), row_height),
            );
            painter.rect_filled(row, egui::CornerRadius::same(2), background);
            let label = self.truncate_to_width(
                painter,
                &live_event_label(event),
                &font,
                (row.width() - 12.0 * ui).max(8.0),
            );
            let galley = painter.layout_no_wrap(label.clone(), font.clone(), Color32::WHITE);
            let _ = galley;
            painter.text(
                Pos2::new(row.left() + 6.0 * ui, row.center().y),
                egui::Align2::LEFT_CENTER,
                label,
                font.clone(),
                Color32::WHITE,
            );
            y += row_height + row_gap;
        }
        if !self.live_status.is_empty() {
            let mut values: Vec<&str> = Vec::new();
            for topic in ["看过", "点赞", "高能榜"] {
                if let Some((text, _)) = self.live_status.get(topic) {
                    values.push(text);
                }
            }
            if values.is_empty() {
                values = self
                    .live_status
                    .values()
                    .map(|(text, _)| text.as_str())
                    .collect();
            }
            let row = Rect::from_min_size(
                Pos2::new(band.left(), y),
                Vec2::new(band.width(), 22.0 * ui),
            );
            painter.rect_filled(row, egui::CornerRadius::same(2), background);
            let text = self.truncate_to_width(
                painter,
                &values.join(" · "),
                &font,
                (row.width() - 12.0 * ui).max(8.0),
            );
            painter.text(
                Pos2::new(row.left() + 6.0 * ui, row.center().y),
                egui::Align2::LEFT_CENTER,
                text,
                font,
                Color32::WHITE,
            );
        }
    }

    fn paint_status(&self, painter: &egui::Painter, rect: Rect, pixels_per_point: f32) {
        let dpi = dpi_scale(pixels_per_point);
        let margin = (12.0 / dpi).clamp(6.0, 20.0);
        let status_font = FontId::proportional((15.0 / dpi).clamp(10.0, 19.0));
        let hint_font = FontId::proportional((13.0 / dpi).clamp(9.0, 17.0));
        // The persistent shortcut hint is a Rust-only addition; users can turn
        // it off in the danmaku/SC settings without losing status feedback.
        let hint = self.options.show_shortcut_hint.then(|| {
            painter.layout_no_wrap(
                hint_text(&self.settings),
                hint_font,
                Color32::from_white_alpha(150),
            )
        });
        let mut top = rect.bottom() - margin;
        if let Some(hint) = hint {
            top -= hint.size().y;
            paint_galley_with_outline(
                painter,
                Pos2::new(rect.left() + margin, top),
                hint,
                Color32::from_white_alpha(150),
                1.0,
            );
        }
        if self.status_at.elapsed().as_secs_f32() < STATUS_VISIBLE_SECS && !self.status.is_empty() {
            let status = painter.layout_no_wrap(
                self.status.clone(),
                status_font.clone(),
                Color32::from_white_alpha(200),
            );
            top -= status.size().y + 3.0;
            paint_galley_with_outline(
                painter,
                Pos2::new(rect.left() + margin, top),
                status,
                Color32::from_white_alpha(200),
                1.0,
            );
        }
        if !self.feedback.is_empty()
            && self.feedback_at.elapsed().as_secs_f32() < FEEDBACK_VISIBLE_SECS
        {
            let feedback = painter.layout_no_wrap(
                self.feedback.clone(),
                status_font,
                Color32::from_rgb(255, 214, 102),
            );
            top -= feedback.size().y + 3.0;
            paint_galley_with_outline(
                painter,
                Pos2::new(rect.left() + margin, top),
                feedback,
                Color32::from_rgb(255, 214, 102),
                1.0,
            );
        }
    }

    fn paint_fatal(&self, ctx: &egui::Context, notice: &FatalNotice) {
        let rect = ctx.content_rect();
        let dpi = dpi_scale(ctx.pixels_per_point());
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("live-hub-chat-overlay"),
        ));
        let remaining = (FATAL_DISPLAY_SECS - notice.since.elapsed().as_secs_f32()).max(0.0);
        let text = format!(
            "{}\n覆盖层将在 {:.0} 秒后关闭",
            truncate_chars(&notice.message, 240),
            remaining.ceil()
        );
        let font = FontId::proportional((18.0 / dpi).clamp(12.0, 26.0));
        let galley = painter.layout(
            text,
            font,
            Color32::from_rgb(255, 214, 214),
            (rect.width() * 0.8).max(240.0),
        );
        let size = galley.size();
        let center = rect.center();
        let background = Rect::from_center_size(center, size + Vec2::splat(24.0));
        painter.rect_filled(
            background,
            egui::CornerRadius::same(10),
            Color32::from_black_alpha(200),
        );
        painter.rect_stroke(
            background,
            egui::CornerRadius::same(10),
            Stroke::new(1.0_f32, Color32::from_rgb(235, 110, 110)),
            StrokeKind::Inside,
        );
        painter.galley(center - size / 2.0, galley, Color32::WHITE);
    }
}

/// Returns a cached GPU texture for one approved remote image, starting the
/// bounded background download on first use. Never blocks the paint thread.
fn request_image_texture(
    painter: &egui::Painter,
    textures: &mut std::collections::BTreeMap<String, egui::TextureHandle>,
    loader: &crate::images::ImageLoader,
    url: &str,
) -> Option<egui::TextureHandle> {
    request_image_texture_masked(painter, textures, loader, url, false)
}

/// Circular avatar variant used by paid-message cards: the alpha channel is
/// masked to a circle when the texture is created so avatars look round without
/// extra draw calls.
fn request_image_texture_round(
    painter: &egui::Painter,
    textures: &mut std::collections::BTreeMap<String, egui::TextureHandle>,
    loader: &crate::images::ImageLoader,
    url: &str,
) -> Option<egui::TextureHandle> {
    request_image_texture_masked(painter, textures, loader, url, true)
}

fn request_image_texture_masked(
    painter: &egui::Painter,
    textures: &mut std::collections::BTreeMap<String, egui::TextureHandle>,
    loader: &crate::images::ImageLoader,
    url: &str,
    round: bool,
) -> Option<egui::TextureHandle> {
    let key = if round {
        format!("round:{url}")
    } else {
        url.to_owned()
    };
    if let Some(texture) = textures.get(&key) {
        return Some(texture.clone());
    }
    let ctx = painter.ctx().clone();
    let image = loader.request(url, move || ctx.request_repaint())?;
    if textures.len() >= 96 {
        if let Some(oldest) = textures.keys().next().cloned() {
            textures.remove(&oldest);
        }
    }
    let mut rgba = image.rgba.clone();
    if round {
        let width = image.width as f32;
        let height = image.height as f32;
        let center = (width / 2.0, height / 2.0);
        let radius = width.min(height) / 2.0;
        for y in 0..image.height {
            for x in 0..image.width {
                let dx = x as f32 + 0.5 - center.0;
                let dy = y as f32 + 0.5 - center.1;
                let distance = (dx * dx + dy * dy).sqrt();
                let coverage = (radius - distance + 0.5).clamp(0.0, 1.0);
                let index = (y * image.width + x) * 4 + 3;
                if let Some(alpha) = rgba.get_mut(index) {
                    *alpha = (u16::from(*alpha) * (coverage * 255.0) as u16 / 255) as u8;
                }
            }
        }
    }
    let texture = painter.ctx().load_texture(
        "danmaku-remote-image",
        egui::ColorImage::from_rgba_unmultiplied([image.width, image.height], &rgba),
        egui::TextureOptions::LINEAR,
    );
    textures.insert(key, texture.clone());
    Some(texture)
}

/// Outline offsets for the configured edge style (empty when stroke is off).
fn danmaku_outline(options: &OverlaySettings, font_points: f32) -> Vec<(f32, f32)> {
    if options.stroke_enabled {
        stroke_offsets(
            options.border_style,
            f32::from(options.stroke_width),
            font_points,
        )
    } else {
        Vec::new()
    }
}

fn label_segments<'a>(
    label: &'a str,
    dictionary: Option<&crate::danmaku::rich::EmoticonDictionary>,
    manifest: Option<&crate::danmaku::rich::EmojiManifest>,
) -> Vec<crate::danmaku::rich::TextSegment> {
    crate::danmaku::rich::split_text_segments(label, dictionary, manifest)
}

fn measure_label_width(
    painter: &egui::Painter,
    label: &str,
    font: &FontId,
    options: &OverlaySettings,
    _emoji: Option<&ColorEmojiSheet>,
    dictionary: Option<&crate::danmaku::rich::EmoticonDictionary>,
    manifest: Option<&crate::danmaku::rich::EmojiManifest>,
) -> f32 {
    use crate::danmaku::rich::TextSegment;
    let mut width = 0.0;
    for segment in label_segments(label, dictionary, manifest) {
        match segment {
            TextSegment::Text(run) => {
                if !run.is_empty() {
                    width += painter
                        .layout_no_wrap(run, font.clone(), Color32::WHITE)
                        .size()
                        .x;
                }
            }
            TextSegment::EmojiCell { .. } => width += font.size * 1.18,
            TextSegment::Emoticon { .. } => {
                width += emoticon_size(font.size, options.emote_size, 0, 0).0
            }
        }
    }
    width
}

#[allow(clippy::too_many_arguments)]
fn paint_label_segments(
    painter: &egui::Painter,
    position: Pos2,
    label: &str,
    font: &FontId,
    color: Color32,
    options: &OverlaySettings,
    outline: &[(f32, f32)],
    emoji: Option<&ColorEmojiSheet>,
    dictionary: Option<&crate::danmaku::rich::EmoticonDictionary>,
    manifest: Option<&crate::danmaku::rich::EmojiManifest>,
    textures: &mut std::collections::BTreeMap<String, egui::TextureHandle>,
    loader: &crate::images::ImageLoader,
) -> f32 {
    use crate::danmaku::rich::TextSegment;
    let outline_color = Color32::from_black_alpha(230);
    let full_uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    let mut cursor = position.x;
    for segment in label_segments(label, dictionary, manifest) {
        match segment {
            TextSegment::Text(run) => {
                if run.is_empty() {
                    continue;
                }
                let galley = painter.layout_no_wrap(run, font.clone(), color);
                let width = galley.size().x;
                for (dx, dy) in outline.iter().copied() {
                    painter.galley_with_override_text_color(
                        Pos2::new(cursor + dx, position.y + dy),
                        galley.clone(),
                        outline_color,
                    );
                }
                painter.galley_with_override_text_color(
                    Pos2::new(cursor, position.y),
                    galley,
                    color,
                );
                cursor += width;
            }
            TextSegment::EmojiCell { x, y } => {
                // Legacy emoji sprite cells are drawn at 1.18x the glyph size and
                // centred on the text line, so the image and the glyphs share a
                // baseline instead of sitting on the top edge.
                let cell = font.size * 1.18;
                let top = position.y + (font.size - cell) / 2.0;
                if let Some(sheet) = emoji {
                    if let Some((texture, uv)) = sheet.cell(x, y) {
                        painter.image(
                            texture,
                            Rect::from_min_size(Pos2::new(cursor, top), Vec2::splat(cell)),
                            uv,
                            Color32::WHITE,
                        );
                    }
                }
                cursor += cell;
            }
            TextSegment::Emoticon { code, url } => {
                if let Some(texture) = request_image_texture(painter, textures, loader, &url) {
                    let (_, height) = emoticon_size(font.size, options.emote_size, 0, 0);
                    let width = (height * texture.aspect_ratio()).min(font.size * 6.0);
                    let top = position.y + (font.size - height) / 2.0;
                    painter.image(
                        texture.id(),
                        Rect::from_min_size(Pos2::new(cursor, top), Vec2::new(width, height)),
                        full_uv,
                        Color32::WHITE,
                    );
                    cursor += width;
                } else {
                    // Text fallback until the bounded image request completes.
                    let galley = painter.layout_no_wrap(code, font.clone(), color);
                    let width = galley.size().x;
                    for (dx, dy) in outline.iter().copied() {
                        painter.galley_with_override_text_color(
                            Pos2::new(cursor + dx, position.y + dy),
                            galley.clone(),
                            outline_color,
                        );
                    }
                    painter.galley_with_override_text_color(
                        Pos2::new(cursor, position.y),
                        galley,
                        color,
                    );
                    cursor += width;
                }
            }
        }
    }
    cursor - position.x
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl eframe::App for Overlay {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Opt-in renderer evidence contains only this overlay's RGBA pixels,
        // never a desktop capture. Refuse to replace any existing file.
        for event in ctx.input(|input| input.events.clone()) {
            if let egui::Event::Screenshot { image, .. } = event {
                if let Some(path) = std::env::var_os("LIVE_HUB_OVERLAY_DIAG_SCREENSHOT") {
                    use image::ImageEncoder;
                    let result = (|| -> std::io::Result<()> {
                        let file = std::fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(path)?;
                        let bytes: Vec<u8> = image
                            .pixels
                            .iter()
                            .flat_map(|color| color.to_array())
                            .collect();
                        image::codecs::png::PngEncoder::new(file)
                            .write_image(
                                &bytes,
                                image.size[0] as u32,
                                image.size[1] as u32,
                                image::ExtendedColorType::Rgba8,
                            )
                            .map_err(std::io::Error::other)
                    })();
                    diag_log(if result.is_ok() {
                        "renderer screenshot saved"
                    } else {
                        "renderer screenshot not saved"
                    });
                }
            }
        }
        // A terminal receiver error is shown once, then the overlay exits instead
        // of spinning on a platform whose transport cannot work.
        if let Some(notice) = fatal_notice(&self.fatal) {
            self.paint_fatal(ctx, &notice);
            ctx.request_repaint_after(Duration::from_millis(100));
            if fatal_expired(notice.since.elapsed()) {
                self.cancellation.cancel();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            return;
        }
        if self.cancellation.is_cancelled() {
            static CLOSE_LOGGED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !CLOSE_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                diag_log("app: cancellation observed; sending Close");
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        self.apply_hotkeys(ctx);
        self.poll_settings();
        if self.last_stats_log.elapsed() >= Duration::from_secs(30) {
            self.last_stats_log = Instant::now();
            overlay_log(
                crate::diag::Level::Info,
                &format!(
                    "danmaku stats: received={} admitted={} dropped={} active={} sc_cards={} live_events={}",
                    self.stats_received,
                    self.stats_admitted,
                    self.stats_dropped,
                    self.scheduler.active().len(),
                    self.super_chats.cards().len(),
                    self.stats_live_events
                ),
            );
            // Be honest instead of looking broken: after 15 s without a single
            // message the room may simply restrict who can chat.
            if self.stats_received == 0 && self.start.elapsed().as_secs() >= 15 {
                self.status = "已连接弹幕 · 房间暂无新消息（B站可能限制发言等级/粉丝）".into();
                self.status_at = Instant::now();
            }
        }
        let real_now = self.start.elapsed().as_secs_f64();
        let now = match self.diag_freeze_ms {
            Some(freeze) if real_now * 1000.0 >= freeze => freeze / 1000.0,
            _ => real_now,
        };
        let pixels_per_point = ctx.pixels_per_point();
        let rect = ctx.content_rect();
        self.drain_sc_interaction(pixels_per_point);
        self.drain_events(now);
        self.event_band = self
            .event_band_rect(rect)
            .map(|band| (band.top() - rect.top(), band.bottom() - rect.top()));
        self.update_metrics(rect, pixels_per_point, now);
        self.scheduler.advance(now);
        // Legacy ToggleOverlay hides every overlay layer (the connection keeps
        // running); the shortcut hint is part of the overlay and hides too.
        if self.user_hidden {
            clear_sc_hits();
            self.sc_cards.clear();
            self.sc_band = None;
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }
        let mut painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("live-hub-chat-overlay"),
        ));
        if !self.options.solid_text {
            painter.set_opacity(f32::from(self.options.opacity_percent) / 100.0);
        }
        self.paint_danmaku(&painter, rect, now);
        self.paint_super_chats(&painter, rect, pixels_per_point, now);
        // Legacy draw order: danmaku, paid cards, live events, transient status.
        self.paint_live_events(&painter, rect);
        self.paint_status(&painter, rect, pixels_per_point);
        if !self.diag_capture_requested
            && std::env::var_os("LIVE_HUB_OVERLAY_DIAG_SCREENSHOT").is_some()
            && now >= 4.0
        {
            self.diag_capture_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        if std::env::var_os("LIVE_HUB_OVERLAY_DIAG_LOG").is_some() && (now.fract() < 0.05) {
            diag_log(&format!(
                "render frame: t={now:.2} pending={} active={} cards={} canvas={}x{}",
                self.pending.len(),
                self.scheduler.active().len(),
                self.super_chats.len(),
                rect.width(),
                rect.height()
            ));
        }
        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

/// Paint a laid-out galley with a black outline/shadow, then the text itself.
/// Reusing one galley keeps the outline cheap even with hundreds of danmaku.
fn paint_galley_with_outline(
    painter: &egui::Painter,
    position: Pos2,
    galley: Arc<egui::Galley>,
    text_color: Color32,
    outline: f32,
) {
    if galley.is_empty() {
        return;
    }
    let outline_color = Color32::from_black_alpha(230);
    for (dx, dy) in [
        (-1.0_f32, 0.0_f32),
        (1.0, 0.0),
        (0.0, -1.0),
        (0.0, 1.0),
        (1.0, 1.0),
    ] {
        painter.galley_with_override_text_color(
            position + Vec2::new(dx * outline, dy * outline),
            galley.clone(),
            outline_color,
        );
    }
    painter.galley_with_override_text_color(position, galley, text_color);
}

#[cfg(windows)]
mod native {
    use super::*;
    use crate::hotkeys::FixedOverlayAction;
    use std::ffi::c_void;
    type Hwnd = *mut c_void;
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct Point {
        x: i32,
        y: i32,
    }
    #[repr(C)]
    struct HotkeyMessage {
        window: Hwnd,
        message: u32,
        wparam: usize,
        lparam: isize,
        time: u32,
        point: Point,
        private: u32,
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn EnumWindows(callback: unsafe extern "system" fn(Hwnd, isize) -> i32, data: isize)
        -> i32;
        fn GetWindowThreadProcessId(window: Hwnd, process: *mut u32) -> u32;
        fn GetClientRect(window: Hwnd, rect: *mut Rect) -> i32;
        fn ClientToScreen(window: Hwnd, point: *mut Point) -> i32;
        fn GetForegroundWindow() -> Hwnd;
        fn IsIconic(window: Hwnd) -> i32;
        fn IsWindow(window: Hwnd) -> i32;
        fn IsWindowVisible(window: Hwnd) -> i32;
        fn GetWindow(window: Hwnd, command: u32) -> Hwnd;
        fn FindWindowW(class: *const u16, title: *const u16) -> Hwnd;
        fn GetWindowLongPtrW(window: Hwnd, index: i32) -> isize;
        fn SetWindowLongPtrW(window: Hwnd, index: i32, value: isize) -> isize;
        fn SetLayeredWindowAttributes(window: Hwnd, color_key: u32, alpha: u8, flags: u32) -> i32;
        fn SetWindowPos(
            window: Hwnd,
            after: Hwnd,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            flags: u32,
        ) -> i32;
        fn ShowWindowAsync(window: Hwnd, command: i32) -> i32;
        fn PostMessageW(window: Hwnd, message: u32, wparam: usize, lparam: isize) -> i32;
        fn RegisterHotKey(window: Hwnd, id: i32, modifiers: u32, key: u32) -> i32;
        fn UnregisterHotKey(window: Hwnd, id: i32) -> i32;
        fn GetAsyncKeyState(key: i32) -> i16;
        fn PeekMessageW(
            message: *mut HotkeyMessage,
            window: Hwnd,
            min: u32,
            max: u32,
            remove: u32,
        ) -> i32;
        fn ScreenToClient(window: Hwnd, point: *mut Point) -> i32;
        fn CallWindowProcW(
            previous: unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize,
            window: Hwnd,
            message: u32,
            wparam: usize,
            lparam: isize,
        ) -> isize;
        fn DefWindowProcW(window: Hwnd, message: u32, wparam: usize, lparam: isize) -> isize;
        fn GetModuleHandleW(name: *const u16) -> *mut c_void;
        fn RegisterClassExW(class: *const WndClassExW) -> u16;
        fn CreateWindowExW(
            ex_style: u32,
            class: *const u16,
            window_name: *const u16,
            style: u32,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            parent: Hwnd,
            menu: *mut c_void,
            instance: *mut c_void,
            param: *mut c_void,
        ) -> Hwnd;
        fn DestroyWindow(window: Hwnd) -> i32;
        fn GetMessageW(message: *mut HotkeyMessage, window: Hwnd, min: u32, max: u32) -> i32;
        fn TranslateMessage(message: *const HotkeyMessage) -> i32;
        fn DispatchMessageW(message: *const HotkeyMessage) -> isize;
        fn SetCapture(window: Hwnd) -> Hwnd;
        fn ReleaseCapture() -> i32;
        fn CreateRectRgn(left: i32, top: i32, right: i32, bottom: i32) -> *mut c_void;
        fn CombineRgn(dst: *mut c_void, first: *mut c_void, second: *mut c_void, mode: i32) -> i32;
        fn SetWindowRgn(window: Hwnd, region: *mut c_void, redraw: i32) -> i32;
        fn DeleteObject(object: *mut c_void) -> i32;
        fn LoadCursorW(instance: *mut c_void, name: *const u16) -> *mut c_void;
        fn SetCursor(cursor: *mut c_void) -> *mut c_void;
        fn GetCursorPos(point: *mut Point) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, process: u32) -> *mut c_void;
        fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    use std::sync::atomic::{AtomicIsize, Ordering as AtomicOrdering};

    static ORIGINAL_WNDPROC: AtomicIsize = AtomicIsize::new(0);

    fn client_point(window: Hwnd, lparam: isize) -> Point {
        let mut point = Point {
            x: (lparam & 0xffff) as u16 as i16 as i32,
            y: ((lparam >> 16) & 0xffff) as u16 as i16 as i32,
        };
        unsafe {
            ScreenToClient(window, &mut point);
        }
        point
    }

    /// Selective mouse hit testing: the overlay is normally fully transparent to
    /// the mouse; only the painted paid-message cards answer HTCLIENT. Everything
    /// else stays HTTRANSPARENT so mpv/PotPlayer keep receiving their input.
    unsafe extern "system" fn overlay_window_proc(
        window: Hwnd,
        message: u32,
        wparam: usize,
        lparam: isize,
    ) -> isize {
        const WM_MOUSEACTIVATE: u32 = 0x0021;
        const WM_NCHITTEST: u32 = 0x0084;
        const WM_MOUSEMOVE: u32 = 0x0200;
        const WM_LBUTTONDOWN: u32 = 0x0201;
        const WM_LBUTTONUP: u32 = 0x0202;
        const WM_MOUSEWHEEL: u32 = 0x020A;
        const HTCLIENT: isize = 1;
        const HTTRANSPARENT: isize = -1;
        match message {
            // Never activate/steal focus from the player window.
            WM_MOUSEACTIVATE => return 3, // MA_NOACTIVATE
            WM_NCHITTEST => {
                let point = client_point(window, lparam);
                return if super::interaction_contains(point.x as f32, point.y as f32) {
                    HTCLIENT
                } else {
                    HTTRANSPARENT
                };
            }
            WM_LBUTTONDOWN => {
                let point = client_point(window, lparam);
                if let Some(offset) = super::interaction_bar_grab(point.x as f32, point.y as f32) {
                    // The scrollbar starts a drag; never treat it as a card click.
                    super::begin_native_bar_drag(offset);
                    super::move_native_bar_drag(point.y as f32);
                    return 0;
                }
                if super::interaction_contains(point.x as f32, point.y as f32) {
                    super::record_native_click(point.x as f32, point.y as f32);
                    return 0;
                }
            }
            WM_MOUSEMOVE => {
                let point = client_point(window, lparam);
                super::move_native_bar_drag(point.y as f32);
                if super::interaction_contains(point.x as f32, point.y as f32) {
                    return 0;
                }
            }
            WM_LBUTTONUP => {
                let point = client_point(window, lparam);
                // Finishing a scrollbar drag must not trigger the card click.
                if super::end_native_bar_drag() {
                    return 0;
                }
                if super::interaction_contains(point.x as f32, point.y as f32) {
                    return 0;
                }
            }
            WM_MOUSEWHEEL => {
                let point = client_point(window, lparam);
                if super::interaction_contains(point.x as f32, point.y as f32) {
                    let notches = ((wparam >> 16) & 0xffff) as u16 as i16 as f32 / 120.0;
                    super::record_native_wheel(notches);
                    return 0;
                }
            }
            _ => {}
        }
        let previous = ORIGINAL_WNDPROC.load(AtomicOrdering::Acquire);
        if previous != 0 {
            let previous: unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize =
                unsafe { std::mem::transmute(previous) };
            return unsafe { CallWindowProcW(previous, window, message, wparam, lparam) };
        }
        unsafe { DefWindowProcW(window, message, wparam, lparam) }
    }

    /// Installs the hit-testing window procedure once. A recreated HWND (DPI or
    /// visibility changes) is re-subclassed by the follow loop.
    pub fn install_interaction_subclass(overlay: Hwnd) {
        if overlay.is_null() || ORIGINAL_WNDPROC.load(AtomicOrdering::Acquire) != 0 {
            return;
        }
        let current = unsafe { GetWindowLongPtrW(overlay, -4) };
        if current != 0 {
            ORIGINAL_WNDPROC.store(current, AtomicOrdering::Release);
            unsafe {
                SetWindowLongPtrW(
                    overlay,
                    -4,
                    overlay_window_proc as *const () as usize as isize,
                );
            }
        }
    }

    fn restore_interaction_subclass(overlay: Hwnd) {
        let previous = ORIGINAL_WNDPROC.swap(0, AtomicOrdering::AcqRel);
        if !overlay.is_null() && previous != 0 {
            unsafe {
                SetWindowLongPtrW(overlay, -4, previous);
            }
        }
    }

    // ---- paid-message input window -------------------------------------------
    //
    // The main overlay window stays WS_EX_TRANSPARENT for its whole lifetime.
    // WM_NCHITTEST / HTTRANSPARENT only forwards to windows owned by the same
    // *thread*, so it never reliably reached PotPlayer (a different process):
    // the player lost mouse hover (its fullscreen control bar never appeared)
    // and the overlay felt "sometimes click-through". Paid-message input now
    // lives in a separate, fully transparent (alpha 0) layered window whose
    // *window region* is the union of the painted card rectangles. The system
    // skips a window outside its region, so every other pixel reaches the
    // player natively regardless of process boundaries.

    const SC_INPUT_CLASS: &str = "LiveHubRustScInput";
    static SC_INPUT_HWND: AtomicIsize = AtomicIsize::new(0);
    static SC_PLAYER_HWND: AtomicIsize = AtomicIsize::new(0);

    #[repr(C)]
    struct WndClassExW {
        size: u32,
        style: u32,
        wnd_proc: Option<unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize>,
        class_extra: i32,
        window_extra: i32,
        instance: *mut c_void,
        icon: *mut c_void,
        cursor: *mut c_void,
        background: *mut c_void,
        menu_name: *const u16,
        class_name: *const u16,
        icon_small: *mut c_void,
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Posts a WM_MOUSEMOVE to the player so a card never swallows the hover
    /// that shows PotPlayer's fullscreen control bar.
    unsafe fn forward_player_mouse_move(window: Hwnd, local: Point) {
        let player = SC_PLAYER_HWND.load(AtomicOrdering::Acquire) as Hwnd;
        if player.is_null() {
            return;
        }
        let mut screen = local;
        let mut player_local = local;
        unsafe {
            if ClientToScreen(window, &mut screen) == 0 {
                return;
            }
            player_local = screen;
            if ScreenToClient(player, &mut player_local) == 0 {
                return;
            }
        }
        let packed =
            ((player_local.x as isize) & 0xffff) | (((player_local.y as isize) & 0xffff) << 16);
        unsafe {
            PostMessageW(player, 0x0200, 0, packed);
        }
    }

    unsafe extern "system" fn sc_input_proc(
        window: Hwnd,
        message: u32,
        wparam: usize,
        lparam: isize,
    ) -> isize {
        const WM_SETCURSOR: u32 = 0x0020;
        const WM_MOUSEACTIVATE: u32 = 0x0021;
        const WM_ERASEBKGND: u32 = 0x0014;
        const WM_MOUSEMOVE: u32 = 0x0200;
        const WM_LBUTTONDOWN: u32 = 0x0201;
        const WM_LBUTTONUP: u32 = 0x0202;
        const WM_MOUSEWHEEL: u32 = 0x020A;
        const HTCLIENT_WORD: usize = 1;
        match message {
            WM_MOUSEACTIVATE => return 3, // MA_NOACTIVATE
            WM_ERASEBKGND => return 1,
            WM_SETCURSOR => {
                if (lparam as usize) & 0xffff == HTCLIENT_WORD {
                    let mut point = Point::default();
                    unsafe {
                        if GetCursorPos(&mut point) != 0 {
                            let mut local = point;
                            if ScreenToClient(window, &mut local) != 0 {
                                let x = local.x as f32;
                                let y = local.y as f32;
                                let cursor = if super::interaction_bar_hit(x, y) {
                                    LoadCursorW(std::ptr::null_mut(), 32645 as *const u16) // IDC_SIZENS
                                } else if super::interaction_contains(x, y) {
                                    LoadCursorW(std::ptr::null_mut(), 32649 as *const u16) // IDC_HAND
                                } else {
                                    LoadCursorW(std::ptr::null_mut(), 32512 as *const u16) // IDC_ARROW
                                };
                                SetCursor(cursor);
                            }
                        }
                    }
                    return 1;
                }
            }
            WM_LBUTTONDOWN => {
                let point = client_point(window, lparam);
                if let Some(offset) = super::interaction_bar_grab(point.x as f32, point.y as f32) {
                    super::begin_native_bar_drag(offset);
                    super::move_native_bar_drag(point.y as f32);
                    unsafe { SetCapture(window) };
                    return 0;
                }
                if super::interaction_contains(point.x as f32, point.y as f32) {
                    super::record_native_click(point.x as f32, point.y as f32);
                    return 0;
                }
            }
            WM_MOUSEMOVE => {
                let point = client_point(window, lparam);
                super::move_native_bar_drag(point.y as f32);
                unsafe { forward_player_mouse_move(window, point) };
                if super::interaction_contains(point.x as f32, point.y as f32) {
                    return 0;
                }
            }
            WM_LBUTTONUP => {
                let point = client_point(window, lparam);
                unsafe { ReleaseCapture() };
                if super::end_native_bar_drag() {
                    return 0;
                }
                if super::interaction_contains(point.x as f32, point.y as f32) {
                    return 0;
                }
            }
            WM_MOUSEWHEEL => {
                let point = client_point(window, lparam);
                if super::interaction_contains(point.x as f32, point.y as f32)
                    || super::interaction_bar_hit(point.x as f32, point.y as f32)
                {
                    let notches = ((wparam >> 16) & 0xffff) as u16 as i16 as f32 / 120.0;
                    super::record_native_wheel(notches);
                    return 0;
                }
            }
            0x0010 => {
                // WM_CLOSE: end the message loop through DestroyWindow.
                unsafe { DestroyWindow(window) };
                return 0;
            }
            _ => {}
        }
        unsafe { DefWindowProcW(window, message, wparam, lparam) }
    }

    /// Spawns the dedicated thread that owns the transparent input window. The
    /// window needs its own blocking message pump, otherwise clicks and wheel
    /// messages would only be handled once every follow-loop tick.
    pub fn spawn_sc_input_thread() {
        let _ = thread::Builder::new()
            .name("overlay-sc-input".into())
            .spawn(|| unsafe {
                let instance = GetModuleHandleW(std::ptr::null());
                let class_name = wide(SC_INPUT_CLASS);
                let mut class: WndClassExW = std::mem::zeroed();
                class.size = std::mem::size_of::<WndClassExW>() as u32;
                class.wnd_proc = Some(sc_input_proc);
                class.instance = instance;
                class.class_name = class_name.as_ptr();
                RegisterClassExW(&class);
                let window_name = wide("LiveHub SC input");
                // WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE
                let ex_style = 0x0008_0000 | 0x0000_0080 | 0x0800_0000;
                let window = CreateWindowExW(
                    ex_style,
                    class_name.as_ptr(),
                    window_name.as_ptr(),
                    0x8000_0000, // WS_POPUP
                    0,
                    0,
                    0,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    instance,
                    std::ptr::null_mut(),
                );
                if window.is_null() {
                    diag_log("sc-input: CreateWindowExW failed");
                    return;
                }
                SC_INPUT_HWND.store(window as isize, AtomicOrdering::Release);
                // Alpha 0: invisible yet still hit-testable; only LWA_COLORKEY
                // would make the window click-through.
                SetLayeredWindowAttributes(window, 0, 0, 2);
                let mut message: HotkeyMessage = std::mem::zeroed();
                loop {
                    let got = GetMessageW(&mut message, std::ptr::null_mut(), 0, 0);
                    if got <= 0 {
                        break;
                    }
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                SC_INPUT_HWND.store(0, AtomicOrdering::Release);
                diag_log("sc-input: message loop ended");
            });
    }

    fn region_signature(regions: &[[f32; 4]]) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for rect in regions {
            for value in rect {
                for byte in value.to_bits().to_le_bytes() {
                    hash ^= u64::from(byte);
                    hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
                }
            }
        }
        hash ^ (regions.len() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
    }

    /// Keeps the transparent input window glued to the overlay client area and
    /// clips it to the painted card rectangles. `last_signature` lives in the
    /// follow loop so an unchanged layout does not rebuild the region.
    pub fn sync_sc_input(
        overlay: Hwnd,
        player: Hwnd,
        area: &ClientArea,
        visible: bool,
        last_signature: &mut u64,
    ) {
        SC_PLAYER_HWND.store(player as isize, AtomicOrdering::Release);
        let window = SC_INPUT_HWND.load(AtomicOrdering::Acquire) as Hwnd;
        if window.is_null() {
            return;
        }
        if !visible {
            unsafe { ShowWindowAsync(window, 0) };
            return;
        }
        unsafe {
            SetWindowPos(
                window,
                overlay,
                area.left,
                area.top,
                area.width,
                area.height,
                0x0010, // SWP_NOACTIVATE: keep it directly above the overlay
            );
        }
        let regions = super::interaction_regions();
        let signature = region_signature(&regions);
        if signature != *last_signature {
            *last_signature = signature;
            if regions.is_empty() {
                unsafe { ShowWindowAsync(window, 0) };
                return;
            }
            let region = unsafe { CreateRectRgn(0, 0, 0, 0) };
            if region.is_null() {
                return;
            }
            for rect in &regions {
                let piece = unsafe {
                    CreateRectRgn(
                        rect[0].round() as i32,
                        rect[1].round() as i32,
                        rect[2].round() as i32,
                        rect[3].round() as i32,
                    )
                };
                if piece.is_null() {
                    continue;
                }
                unsafe {
                    CombineRgn(region, region, piece, 2); // RGN_OR
                    DeleteObject(piece);
                }
            }
            if unsafe { SetWindowRgn(window, region, 1) } == 0 {
                unsafe { DeleteObject(region) };
            }
        }
        unsafe { ShowWindowAsync(window, 4) }; // SW_SHOWNOACTIVATE
    }

    pub fn close_sc_input_window() {
        let window = SC_INPUT_HWND.load(AtomicOrdering::Acquire) as Hwnd;
        if window.is_null() {
            return;
        }
        unsafe { PostMessageW(window, 0x0010, 0, 0) }; // WM_CLOSE
    }

    pub struct PlayerProcess {
        handle: usize,
        pid: u32,
    }
    impl PlayerProcess {
        pub fn open(pid: u32) -> Result<Self> {
            // SYNCHRONIZE only: enough to detect termination, no process mutation.
            let handle = unsafe { OpenProcess(0x0010_0000, 0, pid) };
            ensure!(!handle.is_null(), "无法监视播放器进程；播放器可能已经退出");
            Ok(Self {
                handle: handle as usize,
                pid,
            })
        }
        fn alive(&self) -> bool {
            unsafe { WaitForSingleObject(self.handle as *mut c_void, 0) == 0x102 }
        }
    }
    impl Drop for PlayerProcess {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.handle as *mut c_void);
            }
        }
    }

    pub fn shutdown_guard(process: PlayerProcess, cancel: Cancellation) {
        loop {
            if !process.alive() {
                cancel.cancel();
                thread::sleep(Duration::from_secs(2));
                std::process::exit(0);
            }
            if cancel.is_cancelled() {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    struct Hotkeys {
        down: [bool; 4],
    }
    impl Hotkeys {
        fn new() -> Self {
            Self { down: [false; 4] }
        }
        /// Fixed F7/F8/F9/F10 keys.
        ///
        /// The legacy C# overlay registered these with RegisterHotKey, but
        /// PotPlayer (and other foreground apps) can already own some of the
        /// combos; the old best-effort registration then failed silently. Polling
        /// the asynchronous key state from the 50 ms follow loop is independent of
        /// who owns focus or a conflicting registration; only presses while our
        /// player is foreground are turned into actions.
        fn update(
            &mut self,
            active: bool,
            registered: [bool; 4],
            sender: &SyncSender<OverlayHotkey>,
        ) {
            for index in 0..4 {
                // A key owned by RegisterHotKey already arrived as WM_HOTKEY and
                // must not be counted twice; polling it here would fire the action
                // a second time (and is unnecessary because the OS swallowed it).
                if registered[index] {
                    self.down[index] = false;
                    continue;
                }
                let down = unsafe { GetAsyncKeyState(0x76 + index as i32) as u16 & 0x8000 != 0 };
                if active && down && !self.down[index] {
                    let action = match index {
                        0 => Some(OverlayHotkey::FontSmaller),
                        1 => Some(OverlayHotkey::FontLarger),
                        2 => Some(OverlayHotkey::ToggleDanmaku),
                        3 => Some(OverlayHotkey::ToggleSuperChat),
                        _ => None,
                    };
                    if let Some(action) = action {
                        let _ = sender.try_send(action);
                    }
                }
                self.down[index] = down;
            }
        }
        fn clear(&mut self) {
            self.down = [false; 4];
        }
    }
    impl Drop for Hotkeys {
        fn drop(&mut self) {
            self.clear();
        }
    }

    struct Search {
        pid: u32,
        window: Hwnd,
        area: i64,
    }
    unsafe extern "system" fn find_callback(window: Hwnd, data: isize) -> i32 {
        // EnumWindows invokes this synchronously while Search is alive.
        let search = unsafe { &mut *(data as *mut Search) };
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(window, &mut pid);
        }
        if pid == search.pid
            && unsafe { IsWindowVisible(window) } != 0
            && unsafe { GetWindow(window, 4) }.is_null()
        {
            if let Some(area) = client_area(window) {
                let size = i64::from(area.width) * i64::from(area.height);
                if size > search.area {
                    search.area = size;
                    search.window = window;
                }
            }
        }
        1
    }
    fn player_window(pid: u32) -> Hwnd {
        let mut search = Search {
            pid,
            window: std::ptr::null_mut(),
            area: 0,
        };
        unsafe {
            EnumWindows(find_callback, (&mut search as *mut Search) as isize);
        }
        search.window
    }
    /// Client rect origin in virtual-screen coordinates: monitors left of the
    /// primary produce negative coordinates and SetWindowPos accepts them as-is.
    fn client_area(window: Hwnd) -> Option<ClientArea> {
        let mut rect = Rect::default();
        let mut point = Point::default();
        if unsafe { GetClientRect(window, &mut rect) } == 0
            || unsafe { ClientToScreen(window, &mut point) } == 0
        {
            return None;
        }
        Some(ClientArea {
            left: point.x,
            top: point.y,
            width: rect.right - rect.left,
            height: rect.bottom - rect.top,
        })
    }

    /// Follow the player window using only this process's overlay window.
    /// The player's window handle is re-resolved after minimize/restore, fullscreen
    /// transitions or a window recreation; the client rect is re-read every tick so
    /// resizes are picked up without touching the player. Mouse input passes
    /// through the overlay (WS_EX_TRANSPARENT) and it never activates.
    pub fn follow(
        process: PlayerProcess,
        title: &str,
        ctx: &egui::Context,
        cancel: &Cancellation,
        fatal: &FatalSlot,
        hotkey_sender: SyncSender<OverlayHotkey>,
        shared: super::SharedOptions,
    ) {
        let mut hotkeys = Hotkeys::new();
        // Configurable global hotkeys (legacy 11 Hotkey* bindings). Registered and
        // polled from this same thread: RegisterHotKey posts WM_HOTKEY to the
        // calling thread's queue.
        let mut global_hotkeys = crate::hotkeys::GlobalHotkeys::new();
        let mut configured_active = false;
        let mut applied_hotkey_version = u64::MAX;
        let mut last_hotkey_retry = Instant::now();
        let mut last_fixed_active = false;
        let mut last_fixed_retry = Instant::now() - Duration::from_secs(3);
        let mut live_options = super::OverlaySettings::default();
        let mut last_hide_reason: Option<crate::window_policy::HideReason> = None;
        let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        let diag_force_show = super::diag_force_show_from(|key| std::env::var(key).ok());
        let mut overlay: Hwnd = std::ptr::null_mut();
        let mut target: Hwnd = std::ptr::null_mut();
        let mut style_set = false;
        let mut shown = false;
        let started = Instant::now();
        let mut window_missing_since = None;
        // Dedicated thread + window for paid-message input; the main window keeps
        // WS_EX_TRANSPARENT so the player always receives its own mouse input.
        spawn_sc_input_thread();
        let mut sc_input_signature: u64 = 0;
        while !cancel.is_cancelled() {
            let fatal_notice = super::fatal_notice(fatal);
            if let Some(notice) = &fatal_notice {
                if super::follow_exit_after_fatal(notice.since.elapsed()) {
                    cancel.cancel();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    break;
                }
            }
            if !process.alive() {
                diag_log("follow: player process exited; requesting close");
                cancel.cancel();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                break;
            }
            if overlay.is_null() {
                overlay = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
            }
            if overlay.is_null() {
                if started.elapsed() > Duration::from_secs(15) {
                    cancel.cancel();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    break;
                }
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            let mut owner_pid = 0;
            unsafe {
                GetWindowThreadProcessId(overlay, &mut owner_pid);
            }
            if owner_pid != std::process::id() {
                cancel.cancel();
                break;
            }
            install_interaction_subclass(overlay);
            let native_style = unsafe { GetWindowLongPtrW(overlay, -20) };
            // The overlay window is mouse-transparent for its whole lifetime.
            // Paid-message hit testing used to clear WS_EX_TRANSPARENT and answer
            // WM_NCHITTEST with HTTRANSPARENT outside the cards, but that only
            // forwards inside the same thread: PotPlayer (another process) lost
            // the hover that reveals its fullscreen control bar. Input for cards
            // now lives in a separate alpha-0 window clipped to the card rects
            // (see spawn_sc_input_thread), so the main window can stay transparent.
            let passthrough = true;
            let mut desired = native_style & !(0x20 | 0x0004_0000);
            desired |= 0x0800_0000 | 0x80; // NOACTIVATE | TOOLWINDOW
            if passthrough {
                desired |= 0x20; // TRANSPARENT
            }
            let tracked = 0x0800_0000 | 0x20 | 0x80;
            if !style_set
                || (native_style & tracked) != (desired & tracked)
                || (native_style & 0x0004_0000) != 0
            {
                unsafe {
                    SetWindowLongPtrW(overlay, -20, desired);
                    // winit cursor passthrough adds WS_EX_LAYERED. Windows does
                    // not composite a newly layered window until its attributes
                    // are initialized, even when OpenGL has rendered pixels.
                    // Keep full window alpha; egui supplies per-pixel opacity.
                    SetLayeredWindowAttributes(overlay, 0, 255, 0x2); // LWA_ALPHA
                }
                style_set = true;
            }
            // A hidden player window is not a followable target: re-resolve it so
            // the overlay hides and re-attaches when the player becomes visible.
            let target_gone = target.is_null()
                || unsafe { IsWindow(target) } == 0
                || unsafe { IsWindowVisible(target) } == 0;
            if target_gone {
                target = player_window(process.pid);
            }
            if target.is_null() {
                let missing = window_missing_since.get_or_insert_with(Instant::now);
                if missing.elapsed() >= Duration::from_secs(30) {
                    cancel.cancel();
                    break;
                }
            } else {
                window_missing_since = None;
            }
            let mut foreground_pid = 0;
            unsafe {
                GetWindowThreadProcessId(GetForegroundWindow(), &mut foreground_pid);
            }
            // Pick up settings published by the egui app (hot reload / hotkeys).
            let (version, options) = {
                match shared.lock() {
                    Ok(guard) => (guard.version, guard.options.clone()),
                    Err(_) => (applied_hotkey_version, live_options.clone()),
                }
            };
            live_options = options;
            let owns_hotkeys = live_options.behavior.hotkeys_enabled && foreground_pid == process.pid;
            if version != applied_hotkey_version || configured_active != owns_hotkeys {
                applied_hotkey_version = version;
                configured_active = owns_hotkeys;
                if owns_hotkeys {
                    let errors = global_hotkeys.apply(&live_options.hotkeys);
                    for error in errors {
                        let line = format!("global hotkey: {error}");
                        diag_log(&line);
                        overlay_log(crate::diag::Level::Warn, &line);
                    }
                } else {
                    global_hotkeys.clear();
                }
            }
            // A registration can fail transiently (another program holds the
            // combo during a fullscreen transition). Retry instead of leaving the
            // hotkeys dead for the whole session.
            if owns_hotkeys
                && !global_hotkeys.errors().is_empty()
                && last_hotkey_retry.elapsed() >= Duration::from_secs(2)
            {
                last_hotkey_retry = Instant::now();
                for error in global_hotkeys.apply(&live_options.hotkeys) {
                    let line = format!("global hotkey retry: {error}");
                    diag_log(&line);
                    overlay_log(crate::diag::Level::Warn, &line);
                }
            }
            // Fixed F7-F10: register them with RegisterHotKey **only while the
            // player owns the foreground**, so the OS swallows them from PotPlayer
            // (F9 used to open the player's browser panel, F7 its playback control)
            // while they do not disturb any other application. For a key the
            // registration could not take over (another program owns it), the
            // asynchronous poll below keeps the action working as a fallback.
            let fixed_active =
                live_options.behavior.hotkeys_enabled && foreground_pid == process.pid;
            if fixed_active != last_fixed_active
                || (fixed_active && last_fixed_retry.elapsed() >= Duration::from_secs(2))
            {
                last_fixed_active = fixed_active;
                last_fixed_retry = Instant::now();
                for error in global_hotkeys.set_fixed_active(fixed_active) {
                    let line = format!("fixed hotkey: {error}");
                    diag_log(&line);
                    overlay_log(crate::diag::Level::Warn, &line);
                }
            }
            let fixed_registered = [
                global_hotkeys.fixed_registered(FixedOverlayAction::FontSmaller),
                global_hotkeys.fixed_registered(FixedOverlayAction::FontLarger),
                global_hotkeys.fixed_registered(FixedOverlayAction::ToggleDanmaku),
                global_hotkeys.fixed_registered(FixedOverlayAction::ToggleSuperChat),
            ];
            for event in global_hotkeys.poll_events() {
                let Some(mapped) = super::active_overlay_hotkey(event, owns_hotkeys) else {
                    continue;
                };
                if hotkey_sender.try_send(mapped).is_err() {
                    break;
                }
            }
            hotkeys.update(fixed_active, fixed_registered, &hotkey_sender);
            let area = if target.is_null() {
                None
            } else {
                client_area(target)
            };
            // A terminal receiver error force-shows the overlay so the reason is
            // visible even when the player is hidden or not foreground; the
            // overlay still never activates.
            let force_visible = fatal_notice.is_some();
            // Legacy window policy: default mode shows the overlay whenever the
            // player window is visible (foreground irrelevant); AlwaysOnTop may
            // hide it when HideWhenNotForeground is on. The diagnostic force-show
            // switch counts as "our own window is foreground" so offline
            // acceptance runs keep the old relaxed behaviour.
            let policy = crate::window_policy::WindowPolicy::from(&live_options);
            let state = crate::window_policy::PlayerWindowState {
                process_alive: true,
                window_exists: !target.is_null(),
                window_visible: !target.is_null() && unsafe { IsWindowVisible(target) } != 0,
                minimized: !target.is_null() && unsafe { IsIconic(target) } != 0,
                foreground_player: foreground_pid == process.pid,
                foreground_overlay: diag_force_show || foreground_pid == std::process::id(),
            };
            let decision = crate::window_policy::decide(
                policy,
                &state,
                area.map(|area| {
                    crate::window_policy::ClientArea::new(
                        area.left,
                        area.top,
                        area.width,
                        area.height,
                    )
                }),
            );
            if decision.hide_reason != last_hide_reason {
                last_hide_reason = decision.hide_reason;
                if let Some(reason) = decision.hide_reason {
                    let line = format!("window policy: hidden ({reason:?})");
                    diag_log(&line);
                    overlay_log(crate::diag::Level::Debug, &line);
                }
            }
            let visible = force_visible || decision.visible;
            if visible {
                if let Some(area) = area {
                    unsafe {
                        let mut flags = 0x0010 | 0x0040 | 0x4000; // NOACTIVATE | SHOWWINDOW | ASYNCWINDOWPOS
                        let mut insert_after = std::ptr::null_mut();
                        match decision.z_order {
                            crate::window_policy::ZOrderAction::Topmost => {
                                insert_after = -1_isize as Hwnd; // HWND_TOPMOST
                            }
                            crate::window_policy::ZOrderAction::AbovePlayer => {
                                let mut above = GetWindow(target, 3); // GW_HWNDPREV
                                if above == overlay {
                                    flags |= 0x0004; // NOZORDER
                                    above = std::ptr::null_mut();
                                }
                                insert_after = above;
                            }
                            crate::window_policy::ZOrderAction::None => {
                                flags |= 0x0004; // NOZORDER
                            }
                        }
                        SetWindowPos(
                            overlay,
                            insert_after,
                            area.left,
                            area.top,
                            area.width,
                            area.height,
                            flags,
                        );
                    }
                }
                if !shown {
                    unsafe {
                        ShowWindowAsync(overlay, 4);
                    }
                } // SHOWNOACTIVATE
                shown = true;
            } else if shown {
                unsafe {
                    ShowWindowAsync(overlay, 0);
                }
                shown = false;
            }
            let sc_area = match area {
                Some(area) => area,
                None => ClientArea {
                    left: 0,
                    top: 0,
                    width: 0,
                    height: 0,
                },
            };
            sync_sc_input(
                overlay,
                target,
                &sc_area,
                visible && area.is_some(),
                &mut sc_input_signature,
            );
            ctx.request_repaint_after(Duration::from_millis(33));
            thread::sleep(Duration::from_millis(50));
        }
        if overlay.is_null() {
            overlay = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
        }
        close_sc_input_window();
        restore_interaction_subclass(overlay);
        let mut final_owner_pid = 0;
        if !overlay.is_null() {
            unsafe {
                GetWindowThreadProcessId(overlay, &mut final_owner_pid);
            }
        }
        if !overlay.is_null() && final_owner_pid == std::process::id() {
            // Close before hiding: winit/eframe can leave the event loop waiting
            // for a repaint once the window is hidden, so a WM_CLOSE posted after
            // the hide may never be processed and the process would linger.
            unsafe {
                PostMessageW(overlay, 0x0010, 0, 0);
            }
            diag_log("follow: posted WM_CLOSE");
            let deadline = Instant::now() + Duration::from_millis(1500);
            while Instant::now() < deadline && unsafe { IsWindow(overlay) } != 0 {
                thread::sleep(Duration::from_millis(50));
            }
            diag_log(if unsafe { IsWindow(overlay) } != 0 {
                "follow: window still alive after WM_CLOSE; hiding"
            } else {
                "follow: window closed"
            });
            if unsafe { IsWindow(overlay) } != 0 {
                unsafe {
                    ShowWindowAsync(overlay, 0);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ScCorner;
    use crate::danmaku::DanmakuMessage;

    fn area() -> Option<ClientArea> {
        Some(ClientArea {
            left: -1920,
            top: 10,
            width: 1920,
            height: 1080,
        })
    }
    fn super_chat(
        id: &str,
        user: &str,
        text: &str,
        price_cents: u64,
        duration_secs: u32,
    ) -> SuperChat {
        SuperChat {
            id: id.into(),
            user: user.into(),
            text: text.into(),
            price_cents,
            duration_secs,
            message_trans: String::new(),
            face: String::new(),
            face_frame: String::new(),
            user_level: 0,
            guard_level: 0,
            medal: None,
            background_color: 0xedf5ff,
            background_bottom_color: 0x2a60b2,
            background_color_start: 0x3171d2,
            message_font_color: 0xa3f6ff,
            name_color: 0x666666,
        }
    }

    fn rich_message() -> DanmakuMessage {
        DanmakuMessage {
            image_url: None,
            id: "id".into(),
            user: "观众甲".into(),
            text: "合成弹幕".into(),
            color: 0xffffff,
            mode: danmaku::DanmakuMode::Scroll,
            font_size: 25,
            user_id: 42,
            user_level: 18,
            guard_level: 3,
            medal: Some(danmaku::Medal {
                name: "合成".into(),
                level: 12,
                color: 0x123456,
            }),
            image_width: 0,
            image_height: 0,
            image_large: false,
        }
    }

    #[test]
    fn visibility_requires_foreground_live_player_and_useful_client_area() {
        assert!(should_show(true, true, false, true, area()));
        assert!(!should_show(false, true, false, true, area()));
        assert!(!should_show(true, false, false, true, area()));
        assert!(!should_show(true, true, true, true, area()));
        assert!(!should_show(true, true, false, false, area()));
        assert!(!should_show(true, true, false, true, None));
        assert!(!should_show(
            true,
            true,
            false,
            true,
            Some(ClientArea {
                left: 0,
                top: 0,
                width: 0,
                height: 100
            })
        ));
        assert!(!should_show(
            true,
            true,
            false,
            true,
            Some(ClientArea {
                left: 0,
                top: 0,
                width: 15,
                height: 100
            })
        ));
        // Multi-monitor: the origin may be negative (monitor left of the primary).
        assert!(should_show(
            true,
            true,
            false,
            true,
            Some(ClientArea {
                left: -3840,
                top: -200,
                width: 3840,
                height: 2160
            })
        ));
        assert!(should_show(
            true,
            true,
            false,
            true,
            Some(ClientArea {
                left: 1920,
                top: 0,
                width: 16,
                height: 16
            })
        ));
    }

    #[test]
    fn font_size_steps_are_clamped_to_the_supported_range() {
        assert_eq!(clamp_font_size(f32::NAN), DEFAULT_FONT_SIZE);
        assert_eq!(clamp_font_size(f32::NEG_INFINITY), DEFAULT_FONT_SIZE);
        assert_eq!(clamp_font_size(0.0), MIN_FONT_SIZE);
        assert_eq!(clamp_font_size(999.0), MAX_FONT_SIZE);
        assert_eq!(clamp_font_size(27.0), 27.0);
        assert_eq!(step_font_size(MAX_FONT_SIZE, 1.0), MAX_FONT_SIZE);
        assert_eq!(step_font_size(MIN_FONT_SIZE, -1.0), MIN_FONT_SIZE);
        assert_eq!(step_font_size(27.0, 1.0), 29.0);
        assert_eq!(step_font_size(27.0, -1.0), 25.0);
        assert_eq!(step_font_size(27.0, f32::NAN), 27.0);
        assert_eq!(
            step_font_size(f32::INFINITY, -1.0),
            DEFAULT_FONT_SIZE - FONT_STEP
        );
        assert_eq!(step_font_size(18.0, 0.5), 19.0);
    }

    #[test]
    fn dpi_scale_is_clamped_and_falls_back_for_invalid_values() {
        assert!((dpi_scale(1.0) - 1.0).abs() < f32::EPSILON);
        assert!((dpi_scale(1.5) - 1.5).abs() < f32::EPSILON);
        assert_eq!(dpi_scale(0.0), 1.0);
        assert_eq!(dpi_scale(-2.0), 1.0);
        assert_eq!(dpi_scale(f32::NAN), 1.0);
        assert_eq!(dpi_scale(0.1), MIN_DPI);
        assert_eq!(dpi_scale(10.0), MAX_DPI);
    }

    #[test]
    fn physical_font_size_maps_to_egui_points_per_dpi() {
        assert!((font_points(27.0, 1.0) - 27.0).abs() < 1e-4);
        assert!((font_points(27.0, 1.5) - 18.0).abs() < 1e-4);
        assert!((font_points(27.0, 2.0) - 13.5).abs() < 1e-4);
        assert!((font_points(27.0, 0.0) - 27.0).abs() < 1e-4);
        assert!((font_points(9999.0, 1.0) - MAX_FONT_SIZE).abs() < 1e-4);
        // The physical on-screen size stays constant when Windows scaling changes.
        for pixels_per_point in [1.0_f32, 1.25, 1.5, 2.0] {
            let scale = dpi_scale(pixels_per_point);
            let physical = font_points(30.0, pixels_per_point) * scale;
            assert!(
                (physical - 30.0).abs() < 1e-3,
                "dpi {pixels_per_point}: {physical}"
            );
        }
        assert!((line_height_points(27.0, 2.0) - 13.5 * LINE_HEIGHT_FACTOR).abs() < 1e-4);
        // Speed is now a crossing duration: 100 % -> 8 s, 25 % -> 32 s,
        // 400 % -> 2 s (legacy SpeedPercent steps 60/80/100/130/170 are inside).
        assert!((danmaku_duration_secs(100) - 8.0).abs() < 1e-6);
        assert!((danmaku_duration_secs(25) - 32.0).abs() < 1e-6);
        assert!((danmaku_duration_secs(400) - 2.0).abs() < 1e-6);
        assert!((danmaku_duration_secs(170) - 8.0 / 1.7).abs() < 1e-6);
    }

    #[test]
    fn hotkeys_map_only_f7_to_f10_and_mutate_settings() {
        assert_eq!(hotkey_for(egui::Key::F7), Some(OverlayHotkey::FontSmaller));
        assert_eq!(hotkey_for(egui::Key::F8), Some(OverlayHotkey::FontLarger));
        assert_eq!(
            hotkey_for(egui::Key::F9),
            Some(OverlayHotkey::ToggleDanmaku)
        );
        assert_eq!(
            hotkey_for(egui::Key::F10),
            Some(OverlayHotkey::ToggleSuperChat)
        );
        assert_eq!(hotkey_for(egui::Key::F6), None);
        assert_eq!(hotkey_for(egui::Key::A), None);
        let mut settings = DisplaySettings::default();
        let message = settings.apply(OverlayHotkey::FontLarger);
        let expected = DEFAULT_FONT_SIZE + FONT_STEP;
        assert!(
            message.contains(&format!("{}", expected as i32)),
            "{message}"
        );
        assert_eq!(settings.font_size, expected);
        settings.font_size = MAX_FONT_SIZE;
        settings.apply(OverlayHotkey::FontLarger);
        assert_eq!(settings.font_size, MAX_FONT_SIZE);
        settings.font_size = MIN_FONT_SIZE;
        settings.apply(OverlayHotkey::FontSmaller);
        assert_eq!(settings.font_size, MIN_FONT_SIZE);
        settings.apply(OverlayHotkey::ToggleSuperChat);
        assert!(!settings.super_chat_visible);
        assert!(settings.danmaku_visible);
        settings.apply(OverlayHotkey::ToggleDanmaku);
        assert!(!settings.danmaku_visible);
        settings.apply(OverlayHotkey::ToggleSuperChat);
        assert!(settings.super_chat_visible);
        assert!(!settings.danmaku_visible);
    }

    #[test]
    fn hint_line_mentions_all_shortcuts_and_current_state() {
        let settings = DisplaySettings::default();
        let hint = hint_text(&settings);
        for token in ["F7", "F8", "F9", "F10", "25"] {
            assert!(hint.contains(token), "{hint}");
        }
        assert!(hint.contains("弹幕开") && hint.contains("SC开"), "{hint}");
        let mut hidden = DisplaySettings {
            danmaku_visible: false,
            super_chat_visible: false,
            ..DisplaySettings::default()
        };
        hidden.adjust_font(1.0);
        let hint = hint_text(&hidden);
        assert!(hint.contains("弹幕关") && hint.contains("SC关"), "{hint}");
        assert!(hint.contains("27px"), "{hint}");
    }

    #[test]
    fn super_chat_text_truncation_is_character_aware() {
        assert_eq!(truncate_chars("short", 10), "short");
        assert_eq!(truncate_chars("", 10), "");
        assert_eq!(truncate_chars("abcdef", 0), "");
        assert_eq!(truncate_chars("abcdef", 1), "…");
        assert_eq!(truncate_chars("abcdef", 3), "ab…");
        let cjk = "直".repeat(SC_TEXT_MAX_CHARS + 50);
        let truncated = truncate_chars(&cjk, SC_TEXT_MAX_CHARS);
        assert_eq!(truncated.chars().count(), SC_TEXT_MAX_CHARS);
        assert!(truncated.ends_with('…'));
        assert_eq!(
            truncated
                .chars()
                .filter(|character| *character == '直')
                .count(),
            SC_TEXT_MAX_CHARS - 1
        );
        let emoji = "😀".repeat(10);
        let truncated = truncate_chars(&emoji, 5);
        assert_eq!(truncated.chars().count(), 5);
        assert!(truncated.ends_with('…'));
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn super_chat_stack_keeps_every_card_up_to_the_hard_cap() {
        // The legacy widget never dropped cards at the scroll threshold: it kept
        // up to 200 and scrolled the overflow. The hard cap only guards memory.
        let mut stack = ScCardStack::new();
        for index in 0..12 {
            let pushed = stack.push(
                index as f64,
                &super_chat(&format!("id-{index}"), "用户", "内容", 1000, 3600),
            );
            assert!(pushed.is_some());
        }
        assert_eq!(stack.len(), 12);
        let ids: Vec<&str> = stack.cards().iter().map(|card| card.id.as_str()).collect();
        assert_eq!(ids[0], "id-0");
        assert_eq!(ids[11], "id-11");
        for index in 12..(MAX_SC_CARDS + 5) {
            let _ = stack.push(
                index as f64,
                &super_chat(&format!("id-{index}"), "用户", "内容", 1000, 3600),
            );
        }
        assert_eq!(stack.len(), MAX_SC_CARDS);
        assert_eq!(MAX_SC_CARDS, 200);
    }

    #[test]
    fn super_chat_cards_expire_after_the_platform_duration() {
        // The legacy widget showed a card for exactly the server duration
        // (2 yuan -> 5 s, 30 yuan -> 60 s, 2000 yuan -> 300 s); no 8 s cap.
        assert_eq!(card_lifetime_secs(0), 1.0);
        assert_eq!(card_lifetime_secs(3), 3.0);
        assert_eq!(card_lifetime_secs(60), 60.0);
        assert_eq!(card_lifetime_secs(300), 300.0);
        assert_eq!(card_lifetime_secs(u32::MAX), 3600.0);
        let mut stack = ScCardStack::new();
        assert!(
            stack
                .push(0.0, &super_chat("a", "u", "t", 100, 60))
                .is_some()
        );
        assert!(
            stack
                .push(1.0, &super_chat("b", "u", "t", 100, 60))
                .is_some()
        );
        assert_eq!(stack.prune(59.999), 0);
        assert_eq!(stack.prune(60.0), 1);
        assert_eq!(stack.len(), 1);
        assert_eq!(stack.cards()[0].id, "b");
        assert_eq!(stack.prune(60.999), 0);
        assert_eq!(stack.prune(61.0), 1);
        assert!(stack.is_empty());
        assert_eq!(stack.prune(f64::NAN), 0);
    }

    #[test]
    fn super_chat_replays_are_deduplicated_and_delete_removes_exact_card() {
        let mut stack = ScCardStack::new();
        assert!(
            stack
                .push(0.0, &super_chat("dup", "u", "t", 100, 60))
                .is_some()
        );
        assert!(
            stack
                .push(0.5, &super_chat("dup", "u", "t", 100, 60))
                .is_none()
        );
        assert_eq!(stack.len(), 1);
        // Empty ids are not deduplicated and cannot be deleted by address.
        assert!(
            stack
                .push(0.0, &super_chat("", "u2", "t2", 100, 60))
                .is_some()
        );
        assert!(
            stack
                .push(0.0, &super_chat("", "u3", "t3", 100, 60))
                .is_some()
        );
        assert_eq!(stack.len(), 3);
        assert!(!stack.remove("missing"));
        assert!(!stack.remove(""));
        assert!(stack.remove("dup"));
        assert_eq!(stack.len(), 2);
        assert!(
            stack
                .push(f64::NAN, &super_chat("x", "u", "t", 100, 60))
                .is_none()
        );
        assert_eq!(stack.len(), 2);
        stack.clear();
        assert!(stack.is_empty());
    }

    #[test]
    fn super_chat_cards_store_truncated_text_and_bounded_ids() {
        let mut stack = ScCardStack::new();
        let card = stack
            .push(
                0.0,
                &super_chat(
                    &"i".repeat(300),
                    &"粉".repeat(100),
                    &"字".repeat(500),
                    500,
                    60,
                ),
            )
            .expect("card");
        assert_eq!(card.user.chars().count(), SC_USER_MAX_CHARS);
        assert_eq!(card.text.chars().count(), SC_TEXT_MAX_CHARS);
        assert_eq!(card.id.chars().count(), SC_ID_MAX_CHARS);
        assert!(card.text.ends_with('…'));
        assert_eq!(card.created, 0.0);
        assert_eq!(card.expires, 60.0);
        assert_eq!(card.background_color, 0xedf5ff);
        // Empty text is allowed (some platforms send price-only messages).
        let empty = stack
            .push(1.0, &super_chat("empty", "u", "", 0, 60))
            .expect("card");
        assert!(empty.text.is_empty());
        assert_eq!(format_price(empty.price_cents), "0元");
    }

    #[test]
    fn price_label_matches_the_legacy_yuan_wording() {
        // The legacy widget printed the integer yuan value plus 元.
        assert_eq!(format_price(0), "0元");
        assert_eq!(format_price(1), "0.01元");
        assert_eq!(format_price(100), "1元");
        assert_eq!(format_price(3000), "30元");
        assert_eq!(format_price(1234), "12.34元");
        assert_eq!(
            format_price(u64::MAX),
            format!("{}.{:02}元", u64::MAX / 100, u64::MAX % 100)
        );
        // Tier colours are still bounded for diagnostics, but the card no
        // longer paints them (server colours are authoritative).
        assert_ne!(sc_tier_color(0), sc_tier_color(1));
        assert_eq!(sc_tier_color(1), sc_tier_color(4_999));
        assert_ne!(sc_tier_color(4_999), sc_tier_color(5_000));
    }

    #[test]
    fn paid_message_surface_alpha_uses_percentage_semantics() {
        // OpacityPercent=90 means 90 % opaque (alpha 230), not alpha byte 90.
        assert_eq!(surface_alpha(90, true), 230);
        assert_eq!(surface_alpha(100, true), 255);
        assert_eq!(surface_alpha(10, true), 26);
        assert_eq!(surface_alpha(0, true), surface_alpha(10, true));
        // Without SolidText the whole window opacity is applied elsewhere.
        assert_eq!(surface_alpha(90, false), 255);
    }

    #[test]
    fn paid_message_geometry_matches_the_legacy_270px_widget() {
        // 270 px at 100 % scale in the legacy widget: 720p -> 270, 1080p -> 405.
        assert!((sc_card_width_points(1.0) - 270.0).abs() < 1e-3);
        assert!((sc_card_width_points(1.5) - 405.0).abs() < 1e-3);
        assert!((sc_card_width_points(2.0) - 540.0).abs() < 1e-3);
        assert!((sc_card_width_points(f32::NAN) - 270.0).abs() < 1e-3);
        // scale = ScScale% * pictureHeight/referenceHeight (ScaleWithWindow).
        let scale = sc_points_scale(100, 720.0, 720, true);
        assert!((scale - 1.0).abs() < 1e-3);
        let scale_hd = sc_points_scale(100, 1080.0, 720, true);
        assert!((scale_hd - 1.5).abs() < 1e-3);
        assert!((sc_points_scale(200, 720.0, 720, true) - 2.0).abs() < 1e-3);
        assert!((sc_points_scale(60, 720.0, 720, true) - 0.6).abs() < 1e-3);
        assert!((sc_points_scale(100, 720.0, 720, false) - 1.0).abs() < 1e-3);
        assert_eq!(sc_points_scale(0, f32::NAN, 720, true), 0.6);
        // Legacy corner margins/gaps: 10 px and 4 px at 100 %.
        assert!((SC_CARD_MARGIN - 10.0).abs() < 1e-6);
        assert!((SC_CARD_GAP - 4.0).abs() < 1e-6);
    }

    #[test]
    fn danmaku_admission_is_bounded() {
        assert!(admit_danmaku(0));
        assert!(admit_danmaku(MAX_ACTIVE_DANMAKU - 1));
        assert!(!admit_danmaku(MAX_ACTIVE_DANMAKU));
        assert!(!admit_danmaku(usize::MAX));
        assert!(MAX_ACTIVE_DANMAKU > 0 && MAX_ACTIVE_DANMAKU <= 600);
    }

    #[test]
    fn fatal_connection_error_is_shown_briefly_and_then_closes() {
        assert!(!fatal_expired(Duration::from_secs_f32(
            FATAL_DISPLAY_SECS - 0.1
        )));
        assert!(fatal_expired(Duration::from_secs_f32(FATAL_DISPLAY_SECS)));
        assert!(fatal_expired(Duration::from_secs(60)));
        assert!(!follow_exit_after_fatal(Duration::from_secs_f32(
            FATAL_DISPLAY_SECS
        )));
        assert!(follow_exit_after_fatal(Duration::from_secs_f32(
            FOLLOW_FATAL_EXIT_SECS
        )));
        assert!(FOLLOW_FATAL_EXIT_SECS > FATAL_DISPLAY_SECS);
    }

    #[test]
    fn platform_message_colour_is_normalized_to_rgb() {
        let colour = message_color(0x12_34_56);
        assert_eq!(
            (colour.r(), colour.g(), colour.b(), colour.a()),
            (0x12, 0x34, 0x56, 255)
        );
        assert_eq!(message_color(0xff_ff_ff), Color32::WHITE);
    }

    #[test]
    fn environment_overrides_are_clamped_and_ignore_invalid_values() {
        let defaults = settings_from_env(|_| None);
        assert_eq!(defaults, DisplaySettings::default());
        let settings = settings_from_env(|key| match key {
            ENV_FONT_PX => Some(" 33 ".into()),
            ENV_DANMAKU => Some("OFF".into()),
            ENV_SUPER_CHAT => Some("show".into()),
            _ => None,
        });
        assert_eq!(settings.font_size, 33.0);
        assert!(!settings.danmaku_visible);
        assert!(settings.super_chat_visible);
        let clamped = settings_from_env(|key| match key {
            ENV_FONT_PX => Some("9999".into()),
            _ => None,
        });
        assert_eq!(clamped.font_size, MAX_FONT_SIZE);
        let invalid = settings_from_env(|key| match key {
            ENV_FONT_PX => Some("not-a-number".into()),
            ENV_DANMAKU => Some("maybe".into()),
            ENV_SUPER_CHAT => Some("".into()),
            _ => None,
        });
        assert_eq!(invalid, DisplaySettings::default());
    }

    #[test]
    fn diagnostic_fixture_parses_bounded_synthetic_events_offline() {
        let text = r#"{"initial_delay_ms":1234567,"interval_ms":0,"events":[
            {"Message":{"id":"m1","user":"合成","text":"弹幕","color":16711680,"mode":"Scroll","font_size":25}},
            {"SuperChat":{"id":"sc1","user":"舰长","text":"sc","price_cents":3000,"duration_secs":60}},
            {"Status":"合成状态"}
        ]}"#;
        let plan = parse_fixture(text).expect("valid fixture");
        assert_eq!(plan.initial_delay_ms, FIXTURE_MAX_DELAY_MS);
        assert_eq!(plan.interval_ms, 1);
        assert_eq!(plan.events.len(), 3);
        assert!(matches!(plan.events[0], DanmakuEvent::Message(_)));
        assert!(matches!(plan.events[1], DanmakuEvent::SuperChat(_)));
        assert!(matches!(plan.events[2], DanmakuEvent::Status(_)));
        let defaults = parse_fixture(r#"{"events":[{"Status":"x"}]}"#).expect("defaults");
        assert_eq!(defaults.initial_delay_ms, 0);
        assert_eq!(defaults.interval_ms, FIXTURE_INTERVAL_MS);
        for invalid in [r#"{"events":[]}"#, r#"{}"#, "not json", ""] {
            assert!(parse_fixture(invalid).is_err(), "{invalid}");
        }
        let too_many = serde_json::json!({
            "events": vec![serde_json::json!({"Status": "x"}); MAX_FIXTURE_EVENTS + 1],
        });
        assert!(parse_fixture(&too_many.to_string()).is_err());
    }

    #[test]
    fn diagnostic_force_show_is_opt_in_and_default_off() {
        assert!(!diag_force_show_from(|_| None));
        for value in ["0", "", "false", "yes", "1 2", "true"] {
            assert!(
                !diag_force_show_from(|_| Some(value.to_string())),
                "{value}"
            );
        }
        assert!(diag_force_show_from(
            |key| (key == ENV_DIAG_FORCE_SHOW).then(|| "1".to_string())
        ));
        assert!(diag_force_show_from(|_| Some(" 1 ".to_string())));
        assert_eq!(ENV_FIXTURE, "LIVE_HUB_OVERLAY_FIXTURE");
        assert_eq!(ENV_DIAG_FORCE_SHOW, "LIVE_HUB_OVERLAY_DIAG_FORCE_SHOW");
    }

    #[test]
    fn danmaku_label_follows_the_legacy_prefix_order_and_settings() {
        let message = rich_message();
        let options = OverlaySettings::default();
        assert_eq!(message_label(&message, &options), "合成弹幕");
        let all = OverlaySettings {
            show_guard: true,
            show_medal: true,
            show_user_level: true,
            show_user_name: true,
            ..OverlaySettings::default()
        };
        assert_eq!(
            message_label(&message, &all),
            "[舰长] [合成12] U18 观众甲: 合成弹幕"
        );
        let guard_only = OverlaySettings {
            show_guard: true,
            ..OverlaySettings::default()
        };
        for (level, tag) in [(1, "[总督] "), (2, "[提督] "), (3, "[舰长] ")] {
            let mut message = rich_message();
            message.guard_level = level;
            assert_eq!(
                message_label(&message, &guard_only),
                format!("{tag}合成弹幕")
            );
        }
        let mut no_medal_level = rich_message();
        no_medal_level.medal = Some(danmaku::Medal {
            name: "合成".into(),
            level: 0,
            color: 0,
        });
        assert_eq!(
            message_label(&no_medal_level, &all),
            "[舰长] U18 观众甲: 合成弹幕"
        );
        let long = DanmakuMessage {
            text: "字".repeat(900),
            ..rich_message()
        };
        assert_eq!(
            message_label(&long, &options).chars().count(),
            overlay_truncate_limit()
        );
    }

    fn overlay_truncate_limit() -> usize {
        512
    }

    #[test]
    fn window_relative_font_text_opacity_and_edges_are_bounded() {
        // A 1080p picture at the 720 reference height scales the base 25px up.
        assert!((effective_font_px(25.0, 1080.0, 720, true) - 37.5).abs() < 1e-3);
        assert!((effective_font_px(25.0, 360.0, 720, true) - 12.5).abs() < 1e-3);
        assert_eq!(effective_font_px(25.0, 1080.0, 720, false), 25.0);
        assert_eq!(effective_font_px(25.0, f32::NAN, 720, true), 25.0);
        assert_eq!(effective_font_px(999.0, 1080.0, 720, true), MAX_FONT_SIZE);

        let color = Color32::from_rgb(10, 20, 30);
        assert_eq!(apply_text_opacity(color, 100).a(), 255);
        assert_eq!(apply_text_opacity(color, 50).a(), 127);
        assert_eq!(apply_text_opacity(color, 0).a(), 25);
        // Colour32 stores premultiplied alpha, so only the alpha channel is a
        // stable contract here; the RGB channels are derived by egui.
        assert_eq!(apply_text_opacity(color, 100), color);

        // The outline is a pen around the glyphs: 8 directions, radius bounded
        // by the font size (penWidth = strokeWidth * max(1, fontSize*0.05)).
        assert_eq!(stroke_offsets(TextBorderStyle::Outline, 1.0, 25.0).len(), 8);
        assert_eq!(stroke_offsets(TextBorderStyle::Heavy, 1.0, 25.0).len(), 8);
        assert_eq!(
            stroke_offsets(TextBorderStyle::Shadow, 1.0, 25.0),
            vec![(1.25, 1.25)]
        );
        // A larger glyph gets a proportionally larger pen; heavy doubles it.
        let outline = stroke_offsets(TextBorderStyle::Outline, 1.0, 100.0);
        assert!((outline[0].0.abs() - 2.5).abs() < 1e-3);
        let heavy = stroke_offsets(TextBorderStyle::Heavy, 2.0, 100.0);
        assert!((heavy[0].0.abs() - 10.0).abs() < 1e-3);
        // Degenerate values fall back to the minimum pen and never explode.
        assert_eq!(
            stroke_offsets(TextBorderStyle::Outline, f32::NAN, f32::NAN).len(),
            8
        );
        for (dx, dy) in stroke_offsets(TextBorderStyle::Heavy, 999.0, 999.0) {
            assert!(dx.abs() <= 32.0 && dy.abs() <= 32.0);
        }

        // Platform/small emoticons are 1.25x the glyph height; only the
        // platform "large" flag uses the 1.8x bulge.
        let (small, small_height) = emoticon_size(20.0, EmoteSizeMode::Small, 32, 32);
        assert!((small_height - 25.0).abs() < 1e-3);
        assert!((small - 25.0).abs() < 1e-3);
        let (wide, _) = emoticon_size(20.0, EmoteSizeMode::Platform, 100, 20);
        assert!(wide <= 20.0 * 4.0);
        let (_, large_height) = emoticon_size(20.0, EmoteSizeMode::Large, 0, 0);
        assert!((large_height - 36.0).abs() < 1e-3);
    }

    #[test]
    fn platform_large_marker_never_overrides_a_small_emote_choice() {
        // Legacy: large = mode==large || (mode==platform && platform large).
        assert_eq!(
            effective_emote_size(EmoteSizeMode::Small, true),
            EmoteSizeMode::Small
        );
        assert_eq!(
            effective_emote_size(EmoteSizeMode::Small, false),
            EmoteSizeMode::Small
        );
        assert_eq!(
            effective_emote_size(EmoteSizeMode::Platform, true),
            EmoteSizeMode::Large
        );
        assert_eq!(
            effective_emote_size(EmoteSizeMode::Platform, false),
            EmoteSizeMode::Platform
        );
        assert_eq!(
            effective_emote_size(EmoteSizeMode::Large, false),
            EmoteSizeMode::Large
        );
        // Reserved rows follow the same effective size as the painter.
        assert_eq!(emote_rows(true, true, EmoteSizeMode::Small, true), 1);
        assert_eq!(emote_rows(true, true, EmoteSizeMode::Platform, true), 2);
        assert_eq!(emote_rows(true, true, EmoteSizeMode::Platform, false), 1);
        assert_eq!(emote_rows(true, true, EmoteSizeMode::Large, false), 2);
        assert_eq!(emote_rows(false, true, EmoteSizeMode::Large, true), 1);
        assert_eq!(emote_rows(true, false, EmoteSizeMode::Large, true), 1);
    }

    #[test]
    fn highlight_survives_the_duplicate_merge_id_rewrite() {
        // R08 regression: the pipeline rewrites ids to local-merge-N; storing the
        // wire id made the painter's lookup miss and dropped the highlight colour.
        let mut pipeline =
            crate::chat_pipeline::ChatPipeline::new(crate::core::DanmakuBehavior::default());
        let wire = rich_message();
        let first = pipeline.admit(wire.clone(), 0.0).expect("first admitted");
        assert_ne!(first.message.id, wire.id);
        assert!(first.message.id.starts_with("local-merge-"));
        let mut highlighted = std::collections::BTreeSet::new();
        remember_highlight(&mut highlighted, true, &first.message.id);
        // The painter resolves the colour on the scheduled item, i.e. this id.
        assert!(highlighted.contains(&first.message.id));
        assert!(!highlighted.contains(&wire.id));
        // A repeat merges into the same group: no new item, same highlighted id.
        assert!(pipeline.admit(wire.clone(), 0.1).is_none());
        assert_eq!(pipeline.count(&first.message.id), 2);
        assert!(highlighted.contains(&first.message.id));
    }

    #[test]
    fn local_emoticon_dictionary_wires_the_render_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("emoticons.json");
        std::fs::write(
            &path,
            r#"{"code":0,"data":{"packages":[{"emoticons":[
                {"emoji":"[合成笑]","url":"https://i0.hdslb.com/bfs/live/synthetic.png"}]}]}}"#,
        )
        .unwrap();
        let dictionary = load_local_emoticons(&path).expect("dictionary");
        assert_eq!(dictionary.len(), 1);
        // The text-code segment becomes an image segment in the render path.
        let segments = label_segments("hi[合成笑]", Some(&dictionary), None);
        assert!(
            matches!(
                segments[1],
                crate::danmaku::rich::TextSegment::Emoticon { .. }
            ),
            "{segments:?}"
        );
        // Malformed / missing files degrade to plain text without failing.
        std::fs::write(&path, "not json").unwrap();
        assert!(load_local_emoticons(&path).is_none());
        assert!(load_local_emoticons(&directory.path().join("missing.json")).is_none());
    }

    #[test]
    fn local_douyin_emoji_dictionary_wires_the_render_path_too() {
        // Offline acceptance can point LIVE_HUB_EMOTICON_JSON at the raw
        // Douyin emoji/list response shape, which is what the live receiver
        // caches; both shapes must reach the same render path.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("emoji-list.json");
        std::fs::write(
            &path,
            r#"{"status_code":0,"emoji_list":[
                {"display_name":"[合成捂脸]","emoji_url":{"url_list":[
                    "https://p3.douyinpic.com/obj/synthetic.webp"]}}]}"#,
        )
        .unwrap();
        let dictionary = load_local_emoticons(&path).expect("douyin dictionary");
        assert_eq!(dictionary.len(), 1);
        assert_eq!(
            dictionary.get("[合成捂脸]"),
            Some("https://p3.douyinpic.com/obj/synthetic.webp")
        );
        let segments = label_segments("[合成捂脸]", Some(&dictionary), None);
        assert!(
            matches!(
                segments.as_slice(),
                [crate::danmaku::rich::TextSegment::Emoticon { .. }]
            ),
            "{segments:?}"
        );
    }

    #[test]
    fn dictionary_cache_generation_changes_trigger_a_painter_refresh() {
        // The first snapshot and every later room/platform replacement arrive
        // as a new cache generation; only then does the painter swap.
        assert!(!emoticons_refresh_due(0, 0));
        assert!(emoticons_refresh_due(0, 1));
        assert!(emoticons_refresh_due(2, 3));
        assert!(!emoticons_refresh_due(3, 3));
    }

    #[test]
    fn emoji_sheet_default_path_prefers_the_explicit_override_then_executable_assets() {
        use image::ImageEncoder;
        let directory = tempfile::tempdir().unwrap();
        let exe_dir = directory.path().join("exe");
        let cwd = directory.path().join("cwd");
        std::fs::create_dir_all(exe_dir.join("assets")).unwrap();
        std::fs::create_dir_all(cwd.join("assets")).unwrap();
        let write_png = |path: &Path| {
            let mut pixels = vec![0u8; 2 * 2 * 4];
            for (index, byte) in pixels.iter_mut().enumerate() {
                if index % 4 == 3 {
                    *byte = 255;
                }
            }
            let file = std::fs::File::create(path).unwrap();
            image::codecs::png::PngEncoder::new(file)
                .write_image(&pixels, 2, 2, image::ExtendedColorType::Rgba8)
                .unwrap();
        };
        // Nothing present yet.
        assert_eq!(resolve_emoji_sheet(None, Some(&exe_dir), Some(&cwd)), None);
        // The executable's assets/ wins over the current directory.
        write_png(&exe_dir.join("assets").join("emoji-sheet.png"));
        write_png(&cwd.join("assets").join("emoji-sheet.png"));
        assert_eq!(
            resolve_emoji_sheet(None, Some(&exe_dir), Some(&cwd)),
            Some(exe_dir.join("assets").join("emoji-sheet.png"))
        );
        // An explicit path still wins, but only when it exists.
        let explicit = cwd.join("custom.png");
        assert_eq!(
            resolve_emoji_sheet(Some(&explicit), Some(&exe_dir), Some(&cwd)),
            Some(exe_dir.join("assets").join("emoji-sheet.png"))
        );
        write_png(&explicit);
        assert_eq!(
            resolve_emoji_sheet(Some(&explicit), Some(&exe_dir), Some(&cwd)),
            Some(explicit)
        );
        // A plain emoji-sheet.png next to the executable is also accepted.
        let bare = directory.path().join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        write_png(&bare.join("emoji-sheet.png"));
        assert_eq!(
            resolve_emoji_sheet(None, Some(&bare), None),
            Some(bare.join("emoji-sheet.png"))
        );
    }

    #[test]
    fn bundled_colour_emoji_sheet_loads_with_its_manifest() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets")
            .join("emoji-sheet.png");
        if !path.is_file() {
            // The asset is supplied by the packaging step; absence is not a failure
            // of this unit test.
            return;
        }
        let context = egui::Context::default();
        let sheet = ColorEmojiSheet::load(&context, &path).expect("bundled sheet decodes");
        assert!(
            sheet.width >= 1024 && sheet.height >= 1024,
            "{}x{}",
            sheet.width,
            sheet.height
        );
        assert_eq!(sheet.manifest.cell, 40);
        assert_eq!(sheet.manifest.columns, 48);
        assert!(sheet.manifest.plain_count > 3000);
        let (x, y) = sheet.manifest.cell_for(0x1F600, false).expect("known cell");
        assert!(sheet.cell(x, y).is_some());
        assert!(sheet.manifest.sequence_count > 100);
        // egui caps texture side at 2048: the 1920x3360 sheet must be tiled, and
        // a cell in a later tile must still resolve to a valid texture.
        assert_eq!(sheet.rows_per_tile, 51 * 40);
        assert_eq!(sheet.tiles.len(), 2);
        assert!(
            sheet
                .tiles
                .iter()
                .all(|tile| tile.height as usize <= MAX_TEXTURE_SIDE
                    && tile.top + tile.height <= sheet.height as u32)
        );
        let (last_x, last_y) = sheet
            .manifest
            .cell_at(sheet.manifest.plain_count.saturating_sub(1))
            .expect("last plain cell");
        assert!(last_y >= sheet.rows_per_tile, "y={last_y}");
        assert!(sheet.cell(last_x, last_y).is_some());
    }

    #[test]
    fn colour_emoji_sheet_loads_from_an_explicit_local_file_with_manifest() {
        use image::ImageEncoder;
        let directory = tempfile::tempdir().unwrap();
        let sheet_path = directory.path().join("emoji-sheet.png");
        let manifest_path = directory.path().join("emoji-sheet.txt");
        let mut pixels = vec![0u8; 4 * 4 * 4];
        for (index, byte) in pixels.iter_mut().enumerate() {
            if index % 4 == 3 {
                *byte = 255;
            }
        }
        let file = std::fs::File::create(&sheet_path).unwrap();
        image::codecs::png::PngEncoder::new(file)
            .write_image(&pixels, 4, 4, image::ExtendedColorType::Rgba8)
            .unwrap();
        std::fs::write(
            &manifest_path,
            "CELL=2
COLUMNS=2
PLAINCOUNT=2
PLAIN=2764-2765
VS=2764-2764
",
        )
        .unwrap();
        let ctx = egui::Context::default();
        let sheet = ColorEmojiSheet::load(&ctx, &sheet_path).expect("sheet");
        assert_eq!((sheet.width, sheet.height), (4, 4));
        // The sibling manifest is picked up automatically.
        assert_eq!(sheet.manifest.cell, 2);
        let (_, uv) = sheet.cell(0, 0).expect("cell");
        assert!(uv.min.x == 0.0 && uv.max.x > uv.min.x);
        assert!(sheet.cell(2, 0).is_some());
        assert!(sheet.cell(4, 4).is_none());
        // An undecodable sheet stays disabled instead of crashing.
        std::fs::write(&sheet_path, b"not an image").unwrap();
        assert!(ColorEmojiSheet::load(&ctx, &sheet_path).is_none());
    }

    #[test]
    fn crowded_scaling_and_fixed_overflow_policies_are_pure() {
        assert_eq!(crowd_scale(0, 150, true, 85), 1.0);
        assert_eq!(crowd_scale(10, 150, true, 85), 1.0);
        // 80% occupancy or more shrinks to the configured percentage.
        assert!((crowd_scale(120, 150, true, 85) - 0.85).abs() < 1e-4);
        assert_eq!(crowd_scale(150, 150, false, 85), 1.0);
        assert_eq!(crowd_scale(150, 0, true, 85), 1.0);
        assert!((crowd_scale(150, 150, true, 255) - 1.0).abs() < 1e-4);
        assert!((crowd_scale(150, 150, true, 0) - 0.7).abs() < 1e-4);

        let scroll = danmaku::DanmakuMode::Scroll;
        let top = danmaku::DanmakuMode::Top;
        let bottom = danmaku::DanmakuMode::Bottom;
        let reverse = danmaku::DanmakuMode::Reverse;
        assert_eq!(effective_mode(top, 100.0, 500.0, true), top);
        assert_eq!(effective_mode(top, 600.0, 500.0, true), scroll);
        assert_eq!(effective_mode(bottom, 600.0, 500.0, true), scroll);
        assert_eq!(effective_mode(bottom, 600.0, 500.0, false), bottom);
        assert_eq!(effective_mode(scroll, 600.0, 500.0, true), scroll);
        assert_eq!(effective_mode(reverse, 600.0, 500.0, true), reverse);
    }

    #[test]
    fn paid_message_layout_matches_the_legacy_corner_geometry() {
        let area = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(400.0, 300.0));
        let heights = [60.0, 50.0];
        let layout = sc_layout(area, ScCorner::TopRight, 1.0, &heights, 6, 0.0, 1.0);
        assert_eq!(layout.cards.len(), 2);
        // 270 wide, 10 margin, 12 top gap (legacy ScViewTop for top corners).
        assert!((layout.cards[0].rect.right() - 390.0).abs() < 1e-3);
        assert!((layout.cards[0].rect.left() - 120.0).abs() < 1e-3);
        assert!((layout.cards[0].rect.top() - 12.0).abs() < 1e-3);
        assert!((layout.cards[1].rect.top() - (12.0 + 60.0 + 4.0)).abs() < 1e-3);
        // 16 px close button inset 6 px from the card top-right corner.
        let close = layout.cards[0].close;
        assert!((close.width() - 16.0).abs() < 1e-3);
        assert!((close.right() - 384.0).abs() < 1e-3);
        assert!((close.top() - 18.0).abs() < 1e-3);
        // A click inside a card toggles the form; the close hotspot wins.
        assert_eq!(
            resolve_sc_click(&layout.cards, layout.cards[0].rect.center(), layout.view),
            ScClick::Toggle(0)
        );
        assert_eq!(
            resolve_sc_click(&layout.cards, close.center(), layout.view),
            ScClick::Close(0)
        );
        assert_eq!(
            resolve_sc_click(&layout.cards, Pos2::new(5.0, 250.0), layout.view),
            ScClick::None
        );
        assert_eq!(
            resolve_sc_click(&layout.cards, Pos2::new(900.0, 10.0), layout.view),
            ScClick::None
        );
        // Bottom corners stick to the lower edge and the content anchors bottom.
        let bottom = sc_layout(area, ScCorner::BottomLeft, 1.0, &heights, 6, 0.0, 1.0);
        // The legacy stack total includes a trailing gap, so the last card
        // bottom sits one gap above the view bottom.
        assert!((bottom.cards[1].rect.bottom() - 286.0).abs() < 1e-3);
        assert!((bottom.cards[0].rect.bottom() - (286.0 - 50.0 - 4.0)).abs() < 1e-3);
        assert!((bottom.cards[0].rect.left() - 10.0).abs() < 1e-3);
        assert!((bottom.view.bottom() - 290.0).abs() < 1e-3);
    }

    #[test]
    fn paid_message_scroll_threshold_and_wheel_are_bounded() {
        let area = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(400.0, 300.0));
        // 10 cards of 40 + 4 gap = 440 content; threshold 3 => 132 view height.
        let heights: Vec<f32> = (0..10).map(|_| 40.0).collect();
        let layout = sc_layout(area, ScCorner::TopRight, 1.0, &heights, 3, 0.0, 1.0);
        assert!((layout.view.height() - 132.0).abs() < 1e-3);
        assert!(layout.max_scroll > 0.0);
        assert!(layout.scrollbar.is_some() && layout.thumb.is_some());
        // Positive wheel delta shows newer cards (scroll offset decreases), at
        // 0.6 px per notch, clamped to [0, max].
        // One legacy notch (raw delta 120) moves a card 72 points; the recorded
        // value is notches, so 1.0 must move 72 points and 10 notches hit the cap.
        assert!((sc_scroll_after(100.0, 1.0, 200.0) - 28.0).abs() < 1e-3);
        assert_eq!(sc_scroll_after(0.0, -1.0, 200.0), 72.0);
        assert_eq!(sc_scroll_after(100.0, 10.0, 200.0), 0.0);
        assert_eq!(sc_scroll_after(500.0, 0.0, 200.0), 200.0);
        assert_eq!(sc_scroll_after(f32::NAN, f32::NAN, 200.0), 0.0);
        // Two cards fit: no scrollbar and no scrolling.
        let short = sc_layout(area, ScCorner::TopRight, 1.0, &[40.0, 40.0], 6, 0.0, 1.0);
        assert!(short.scrollbar.is_none());
        assert_eq!(short.max_scroll, 0.0);
        // A scrolled layout moves the newest card up with the offset.
        let scrolled = sc_layout(area, ScCorner::TopRight, 1.0, &heights, 3, 20.0, 1.0);
        assert!((scrolled.cards[0].rect.top() - (12.0 - 20.0)).abs() < 1e-3);
        assert_eq!(scrolled.scroll, 20.0);
    }

    #[test]
    fn paid_message_badge_is_content_sized_and_hugs_the_chosen_corner() {
        let area = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(400.0, 300.0));
        // The legacy badge is a content-sized pill: text + 20 px / 10 px padding.
        let badge = sc_badge_rect(area, ScCorner::BottomRight, 1.0, 100.0, 20.0, 1.0);
        assert!((badge.width() - 120.0).abs() < 1e-3);
        // Legacy drawn pill height: text + 13*scale padding (~57 px at 2x).
        assert!((badge.height() - 33.0).abs() < 1e-3);
        assert!((badge.right() - 390.0).abs() < 1e-3);
        assert!((badge.bottom() - 290.0).abs() < 1e-3);
        let top_left = sc_badge_rect(area, ScCorner::TopLeft, 1.0, 100.0, 20.0, 1.0);
        assert!((top_left.left() - 10.0).abs() < 1e-3);
        assert!((top_left.top() - 12.0).abs() < 1e-3);
        // 200 % scale doubles text and margins; the 12 px top offset is a fixed
        // physical pixel value, so it becomes 6 points at 200 % DPI.
        let scaled = sc_badge_rect(area, ScCorner::TopLeft, 2.0, 100.0, 20.0, 0.5);
        assert!((scaled.width() - 140.0).abs() < 1e-3);
        assert!((scaled.height() - 46.0).abs() < 1e-3);
        assert!((scaled.left() - 20.0).abs() < 1e-3);
        assert!((scaled.top() - 6.0).abs() < 1e-3);
    }

    #[test]
    fn paid_message_reserved_band_and_progress_are_bounded() {
        assert_eq!(
            sc_reserved_band(ScMode::Off, ScCorner::TopRight, 2, 500.0, 10.0, 20.0),
            None
        );
        assert_eq!(
            sc_reserved_band(ScMode::Full, ScCorner::TopRight, 0, 500.0, 10.0, 20.0),
            None
        );
        let top = sc_reserved_band(ScMode::Full, ScCorner::TopRight, 2, 500.0, 10.0, 20.0).unwrap();
        assert_eq!(top.0, 0.0);
        assert!(top.1 <= 500.0);
        let bottom_band =
            sc_reserved_band(ScMode::Compact, ScCorner::BottomLeft, 2, 500.0, 10.0, 20.0).unwrap();
        assert_eq!(bottom_band.1, 500.0);
        assert!(bottom_band.0 >= 0.0);
        // A tall stack never reserves more than 60% of the picture.
        let huge =
            sc_reserved_band(ScMode::Full, ScCorner::TopRight, 100, 500.0, 10.0, 40.0).unwrap();
        assert!(huge.1 - huge.0 <= 300.0 + 10.0 + 1e-3);

        assert!((sc_progress_ratio(0.0, 10.0, 0.0) - 1.0).abs() < 1e-6);
        assert!((sc_progress_ratio(0.0, 10.0, 5.0) - 0.5).abs() < 1e-6);
        assert_eq!(sc_progress_ratio(0.0, 10.0, 20.0), 0.0);
        assert_eq!(sc_progress_ratio(10.0, 5.0, 6.0), 1.0);
        assert_eq!(sc_progress_ratio(f64::NAN, 5.0, 1.0), 1.0);
    }

    fn chat_message(text: &str) -> DanmakuMessage {
        DanmakuMessage {
            image_url: None,
            id: text.into(),
            user: "合成".into(),
            text: text.into(),
            color: 0xffffff,
            mode: danmaku::DanmakuMode::Scroll,
            font_size: 25,
            user_id: 0,
            user_level: 0,
            guard_level: 0,
            medal: None,
            image_width: 0,
            image_height: 0,
            image_large: false,
        }
    }

    #[test]
    fn time_sequenced_scrolls_never_share_a_lane_while_visible() {
        // Regression: the path check must compare both messages at the same
        // instant. Sampling their entry positions made every later message look
        // clear of the one already flying on that lane (the user's "same lane
        // overlap" screenshot).
        let mut scheduler = LaneScheduler::new(1280.0, 720.0, 25.0, 8.0);
        let mut now = 0.0_f64;
        for index in 0..48 {
            let _ = scheduler.push_measured(
                chat_message(&format!("msg {index} 长时间文案用于占用轨道与验证")),
                420.0,
                now,
            );
            now += 0.26;
            scheduler.advance(now);
        }
        for sample in [0.5_f64, 2.0, 4.0, 6.5, 9.0] {
            let items: Vec<&danmaku::FlyingDanmaku> = scheduler
                .active()
                .iter()
                .filter(|item| item.start <= sample && item.visible(sample))
                .collect();
            for (index, left) in items.iter().enumerate() {
                for right in items.iter().skip(index + 1) {
                    if left.lane != right.lane {
                        continue;
                    }
                    let (l0, l1) = (left.x(sample), left.x(sample) + left.width);
                    let (r0, r1) = (right.x(sample), right.x(sample) + right.width);
                    assert!(
                        l1 <= r0 + 0.5 || r1 <= l0 + 0.5,
                        "lane {} overlaps at t={sample}: {l0}..{l1} vs {r0}..{r1}",
                        left.lane
                    );
                }
            }
        }
        // A resize keeps the lanes and must not create same-lane overlap.
        scheduler.refit(900.0, 540.0, now);
        for sample in [now, now + 0.5, now + 1.5] {
            let items: Vec<&danmaku::FlyingDanmaku> = scheduler
                .active()
                .iter()
                .filter(|item| item.visible(sample))
                .collect();
            let mut lanes = std::collections::BTreeMap::new();
            for item in items {
                if let Some(left) = lanes.insert(item.lane, item) {
                    let right = item;
                    let (l0, l1) = (left.x(sample), left.x(sample) + left.width);
                    let (r0, r1) = (right.x(sample), right.x(sample) + right.width);
                    assert!(
                        l1 <= r0 + 0.5 || r1 <= l0 + 0.5,
                        "lane {} overlaps after resize",
                        item.lane
                    );
                }
            }
        }
    }

    #[test]
    fn viewport_sync_makes_new_messages_enter_from_the_right_edge() {
        // The scheduler is constructed with the bootstrap size; a later viewport
        // sync (font-change path) must move the entry edge with the picture.
        let mut scheduler = LaneScheduler::new(640.0, 240.0, 25.0, 8.0);
        let early = scheduler
            .push_measured(chat_message("early"), 100.0, 0.0)
            .expect("placed");
        assert!((early.x(0.0) - 640.0).abs() < 1e-3);
        scheduler.set_viewport(1280.0, 576.0);
        assert_eq!(scheduler.viewport(), (1280.0, 576.0));
        let later = scheduler
            .push_measured(chat_message("later"), 100.0, 0.0)
            .expect("placed after sync");
        assert!(
            (later.x(0.0) - 1280.0).abs() < 1e-3,
            "new message must enter at the right edge, x={}",
            later.x(0.0)
        );
        assert!((later.speed - (1280.0 + 100.0) / 8.0).abs() < 1e-3);
    }

    #[test]
    fn merged_growth_follows_the_legacy_heat_levels() {
        assert_eq!(merge_level(1), 0);
        assert_eq!(merge_level(2), 1);
        assert_eq!(merge_level(8), 3);
        assert_eq!(merge_level(32), 5);
        assert_eq!(merge_level(501), 5);
        assert!((merge_scale(1, 140) - 1.0).abs() < 1e-6);
        assert!((merge_scale(2, 140) - 1.08).abs() < 1e-6);
        assert!((merge_scale(8, 140) - 1.24).abs() < 1e-6);
        assert!((merge_scale(32, 140) - 1.4).abs() < 1e-6);
        assert!((merge_scale(32, 999) - 1.8).abs() < 1e-6);
    }

    #[test]
    fn scrollbar_drag_maps_to_the_pixel_scroll_offset() {
        // Track 100..300, thumb 60 high, max scroll 240: drag to the middle.
        // travel = 200-60 = 140; y=170 is half way -> half of max_scroll.
        assert!((sc_bar_scroll_for(170.0, 0.0, 100.0, 200.0, 60.0, 240.0) - 120.0).abs() < 1e-3);
        assert_eq!(sc_bar_scroll_for(0.0, 0.0, 100.0, 200.0, 60.0, 240.0), 0.0);
        assert_eq!(
            sc_bar_scroll_for(9999.0, 0.0, 100.0, 200.0, 60.0, 240.0),
            240.0
        );
        // Grab offset keeps the thumb from jumping on mouse-down.
        // Grabbing 40 px below the thumb top keeps the thumb from jumping.
        assert!(
            (sc_bar_scroll_for(180.0, 40.0, 100.0, 200.0, 60.0, 240.0) - 68.571_43).abs() < 1e-2
        );
        assert_eq!(sc_bar_scroll_for(120.0, 0.0, 100.0, 60.0, 60.0, 240.0), 0.0);
        assert_eq!(
            sc_bar_scroll_for(f32::NAN, 0.0, 100.0, 200.0, 60.0, 240.0),
            0.0
        );
    }

    #[test]
    fn global_hotkey_actions_map_to_overlay_events() {
        use crate::hotkeys::HotkeyAction;
        assert_eq!(
            overlay_hotkey_for(HotkeyAction::ToggleDanmaku),
            OverlayHotkey::ToggleDanmaku
        );
        assert_eq!(
            overlay_hotkey_for(HotkeyAction::CycleSc),
            OverlayHotkey::CycleSuperChat
        );
        assert_eq!(
            overlay_hotkey_for(HotkeyAction::ToggleOverlay),
            OverlayHotkey::ToggleOverlay
        );
        assert_eq!(
            overlay_hotkey_for(HotkeyAction::AreaUp),
            OverlayHotkey::AreaLarger
        );
        assert_eq!(
            overlay_hotkey_for(HotkeyAction::SpeedDown),
            OverlayHotkey::SpeedSlower
        );
        assert_eq!(
            overlay_hotkey_for(HotkeyAction::DelayMore),
            OverlayHotkey::DelayMore
        );
        // Legacy steps stay sorted so the hotkeys walk them in order.
        assert!(DISPLAY_AREA_STEPS.windows(2).all(|pair| pair[0] > pair[1]));
        assert!(SPEED_STEPS.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn queued_hotkeys_from_both_ranges_require_the_current_player() {
        use crate::hotkeys::{FixedOverlayAction, HotkeyAction, HotkeyEvent};
        for action in HotkeyAction::ALL {
            let event = HotkeyEvent::Action(action);
            assert_eq!(active_overlay_hotkey(event, false), None);
            assert_eq!(active_overlay_hotkey(event, true), Some(overlay_hotkey_for(action)));
        }
        for action in FixedOverlayAction::ALL {
            let event = HotkeyEvent::Fixed(action);
            assert_eq!(active_overlay_hotkey(event, false), None);
            assert_eq!(active_overlay_hotkey(event, true), Some(fixed_overlay_hotkey_for(action)));
        }
    }

    #[test]
    fn scheduler_uses_the_legacy_crossing_duration_and_track_height() {
        // Standard speed: every message crosses the picture in 8 s, so a long
        // message moves faster than a short one (legacy speed model).
        let mut scheduler = LaneScheduler::new(1000.0, 600.0, 20.0, 8.0);
        let short = scheduler
            .push_measured(chat_message("short"), 100.0, 0.0)
            .expect("short");
        let long = scheduler
            .push_measured(chat_message("a much longer message"), 400.0, 0.0)
            .expect("long");
        assert!((short.speed - (1000.0 + 100.0) / 8.0).abs() < 1e-3);
        assert!((long.speed - (1000.0 + 400.0) / 8.0).abs() < 1e-3);
        assert!(long.speed > short.speed);
        // Entry positions: scroll starts just off the right edge, reverse off
        // the left edge (legacy 4 px offset).
        assert!(short.x(0.0) >= 1000.0);
        let reverse = DanmakuMessage {
            mode: danmaku::DanmakuMode::Reverse,
            ..chat_message("reverse")
        };
        let placed = scheduler
            .push_measured(reverse, 100.0, 0.0)
            .expect("reverse");
        assert!(placed.x(0.0) <= 0.0);
        assert!(placed.x(1.0) > placed.x(0.0));
        // Track height is measured from the font box, not a bare 1.35 factor.
        assert!((danmaku::track_height_points(20.0, 100) - 33.0).abs() < 1e-3);
        assert!((danmaku::track_height_points(20.0, 150) - 49.5).abs() < 1e-3);
        assert!((danmaku::crossing_duration_secs(100) - 8.0).abs() < 1e-6);
        assert!((danmaku::crossing_duration_secs(200) - 4.0).abs() < 1e-6);
        assert!((danmaku::crossing_duration_secs(25) - 32.0).abs() < 1e-6);
    }

    #[test]
    fn resize_preserves_flight_progress_and_track_instead_of_collapsing() {
        let mut scheduler = LaneScheduler::new(1240.0, 720.0, 20.0, 8.0);
        scheduler.set_fixed_layout(100, 100, 6);
        let mut placed = Vec::new();
        for index in 0..6 {
            placed.push(
                scheduler
                    .push_measured(chat_message(&format!("keep-{index}")), 180.0, 0.0)
                    .expect("six distinct lanes"),
            );
        }
        let lanes: Vec<usize> = placed.iter().map(|item| item.lane).collect();
        assert_eq!(lanes, vec![0, 1, 2, 3, 4, 5]);
        // Advance three seconds: each message has travelled 3/8 of its path.
        let now = 3.0;
        let progress_before: Vec<f32> = placed.iter().map(|item| item.progress(now)).collect();
        assert!((progress_before[0] - 3.0 / 8.0).abs() < 1e-3);
        scheduler.refit(1920.0, 1080.0, now);
        assert_eq!(scheduler.active().len(), 6, "resize must not drop danmaku");
        let after: Vec<&danmaku::FlyingDanmaku> = scheduler.active().iter().collect();
        let after_lanes: Vec<usize> = after.iter().map(|item| item.lane).collect();
        assert_eq!(after_lanes, vec![0, 1, 2, 3, 4, 5], "lanes must be kept");
        for (index, item) in after.iter().enumerate() {
            // Same completed fraction, but the speed belongs to the new width.
            assert!(
                (item.progress(now) - progress_before[index]).abs() < 0.02,
                "progress moved: {} vs {}",
                item.progress(now),
                progress_before[index]
            );
            assert!((item.viewport_width - 1920.0).abs() < 1e-3);
            assert!(
                (item.speed - (1920.0 + item.width) / 8.0).abs() < 1e-3,
                "speed not recomputed: {}",
                item.speed
            );
        }
        // No two messages on the same lane overlap after the resize.
        for (index, left) in after.iter().enumerate() {
            for right in after.iter().skip(index + 1) {
                if left.lane != right.lane {
                    continue;
                }
                let (left_x, right_x) = (left.x(now), right.x(now));
                let (l0, l1) = (
                    left_x.min(left_x + left.width),
                    left_x.max(left_x + left.width),
                );
                let (r0, r1) = (
                    right_x.min(right_x + right.width),
                    right_x.max(right_x + right.width),
                );
                assert!(l1 <= r0 || r1 <= l0, "same-lane overlap after resize");
            }
        }
        assert_eq!(scheduler.active()[0].y, 0.0);
        // A shrink then restore keeps the layer bounded and the lanes unique.
        scheduler.refit(800.0, 450.0, now + 0.5);
        scheduler.refit(1240.0, 720.0, now + 1.0);
        let mut seen = std::collections::BTreeSet::new();
        for item in scheduler.active() {
            assert!(seen.insert(item.lane), "duplicate lane after resize");
            assert!(item.y + item.height <= 720.0 + 0.5);
        }
    }

    #[test]
    fn fixed_zones_limits_and_four_second_lifetime_are_enforced() {
        // Font 20 => 33 pt tracks. A 100 % top zone keeps every row legal.
        let mut scheduler = LaneScheduler::new(500.0, 400.0, 20.0, 8.0);
        scheduler.set_fixed_layout(100, 100, 1);
        let top = DanmakuMessage {
            mode: danmaku::DanmakuMode::Top,
            ..chat_message("top")
        };
        let placed = scheduler
            .push_measured(top.clone(), 100.0, 0.0)
            .expect("first top");
        // Fixed rows live for the legacy 4 seconds.
        assert!(placed.visible(3.9));
        assert!(!placed.visible(4.0));
        assert!(scheduler.push_measured(top, 100.0, 0.0).is_none());

        let mut scheduler = LaneScheduler::new(500.0, 400.0, 20.0, 8.0);
        scheduler.set_fixed_layout(100, 100, 6);
        let bottom = DanmakuMessage {
            mode: danmaku::DanmakuMode::Bottom,
            ..chat_message("bottom")
        };
        for _ in 0..6 {
            assert!(scheduler.push_measured(bottom.clone(), 80.0, 0.0).is_some());
        }
        assert!(scheduler.push_measured(bottom, 80.0, 0.0).is_none());
        // Bottom rows hug the lower edge.
        let lowest = scheduler
            .active()
            .iter()
            .max_by(|a, b| a.y.total_cmp(&b.y))
            .expect("bottom rows");
        assert!((lowest.y + lowest.height - 400.0).abs() < 0.5);
    }

    #[test]
    fn reserved_areas_become_lane_bands_in_points() {
        let areas = vec![
            PercentRect {
                x: 0,
                y: 0,
                width: 100,
                height: 8,
            },
            PercentRect {
                x: 0,
                y: 85,
                width: 100,
                height: 15,
            },
        ];
        let bands = reserved_bands_from_areas(&areas, 1000.0);
        assert_eq!(bands.len(), 2);
        assert!((bands[0].0 - 0.0).abs() < 1e-3 && (bands[0].1 - 80.0).abs() < 1e-3);
        assert!((bands[1].0 - 850.0).abs() < 1e-3 && (bands[1].1 - 1000.0).abs() < 1e-3);
        assert!(reserved_bands_from_areas(&areas, 0.0).is_empty());
        assert!(reserved_bands_from_areas(&areas, f32::NAN).is_empty());

        let mut scheduler = LaneScheduler::new(500.0, 200.0, 20.0, 8.0);
        scheduler.set_line_gap_percent(100);
        scheduler.set_reserved_bands(vec![(0.0, 33.0)]);
        let placed = scheduler
            .push_measured(chat_message("reserved"), 100.0, 0.0)
            .expect("lane below the reserved band");
        assert!(placed.y >= 33.0, "y={}", placed.y);
        scheduler.set_reserved_bands(vec![(0.0, 200.0)]);
        assert!(
            scheduler
                .push_measured(chat_message("blocked"), 100.0, 0.0)
                .is_none()
        );
        scheduler.set_reserved_bands(Vec::new());
        scheduler.set_line_gap_percent(200);
        assert!((scheduler.line_height() - 33.0 * 2.0).abs() < 1e-3);
    }

    #[test]
    fn multi_row_messages_reserve_every_row_they_occupy() {
        let mut scheduler = LaneScheduler::new(500.0, 300.0, 20.0, 8.0);
        let rows = scheduler
            .push_measured_rows(chat_message("large emote"), 120.0, 2, 0.0)
            .expect("place multi-row");
        assert!((rows.height - 2.0 * 33.0).abs() < 1e-3);
        // A second message must not overlap the two rows it claimed.
        let next = scheduler
            .push_measured(chat_message("next"), 120.0, 0.0)
            .expect("place after multi-row");
        assert!(
            next.y >= rows.y + rows.height - 0.1 || next.y + next.height <= rows.y + 0.1,
            "rows overlap: {} vs {}",
            next.y,
            rows.y
        );
        // A fixed message wider than the picture is refused (legacy converts it
        // to a scrolling message before this point).
        let wide = DanmakuMessage {
            mode: danmaku::DanmakuMode::Top,
            ..chat_message("too wide")
        };
        assert!(scheduler.push_measured(wide, 9999.0, 0.0).is_none());
    }

    #[test]
    fn super_chat_mode_visibility_count_lifetime_and_corners_are_bounded() {
        assert_eq!(effective_sc_mode(false, ScMode::Full), ScMode::Off);
        assert_eq!(effective_sc_mode(true, ScMode::Compact), ScMode::Compact);
        assert_eq!(sc_visible_count(ScMode::Off, 9, 6), 0);
        assert_eq!(sc_visible_count(ScMode::Badge, 9, 6), 1);
        assert_eq!(sc_visible_count(ScMode::Full, 9, 6), 6);
        assert_eq!(sc_visible_count(ScMode::Full, 9, 0), 9);
        assert_eq!(sc_visible_count(ScMode::Compact, 0, 6), 0);
        // The platform duration is used as-is at 100 % and scales in bounds.
        assert!((sc_card_lifetime(60, 100) - 60.0).abs() < 1e-6);
        assert!((sc_card_lifetime(4, 100) - 4.0).abs() < 1e-6);
        assert!((sc_card_lifetime(4, 50) - 2.0).abs() < 1e-6);
        assert!((sc_card_lifetime(4, 999) - 16.0).abs() < 1e-6);
        assert!((sc_card_lifetime(1, 20) - 0.5).abs() < 1e-6);
        // The visible window is a scroll threshold, not a drop cap.
        assert_eq!(sc_visible_count(ScMode::Full, 9, 6), 6);
        assert_eq!(sc_visible_count(ScMode::Full, 3, 6), 3);
        assert!(ScCorner::TopRight.left() == false);
        assert!(ScCorner::BottomLeft.left() && ScCorner::BottomLeft.bottom());
        assert!(ScCorner::TopLeft.left() && !ScCorner::TopLeft.bottom());
    }

    #[test]
    fn scaled_super_chat_push_keeps_rich_fields_and_lifetime() {
        let mut stack = ScCardStack::new();
        let card = stack
            .push_with_scale(0.0, &super_chat("id", "用户", "内容", 3000, 8), 50)
            .expect("card");
        assert!((card.expires - 4.0).abs() < 1e-6);
        assert!(
            stack
                .push_with_scale(f64::NAN, &super_chat("x", "u", "t", 1, 1), 100)
                .is_none()
        );
    }

    #[test]
    fn live_event_labels_match_the_legacy_wording_and_settings_gate_them() {
        let event = |kind, user: &str, text: &str, count| LiveEvent {
            id: String::new(),
            kind,
            user: user.into(),
            text: text.into(),
            count,
        };
        assert_eq!(
            live_event_label(&event(LiveEventKind::Gift, "甲", "礼物", 3)),
            "甲 赠送 礼物 ×3"
        );
        assert_eq!(
            live_event_label(&event(LiveEventKind::Enter, "甲", "进入直播间", 1)),
            "甲 进入直播间"
        );
        assert_eq!(
            live_event_label(&event(LiveEventKind::Follow, "甲", "", 1)),
            "甲 关注了主播"
        );
        assert_eq!(
            live_event_label(&event(LiveEventKind::Guard, "甲", "开通提督", 1)),
            "甲 开通提督"
        );
        assert_eq!(
            live_event_label(&event(LiveEventKind::Notice, "", "合成公告", 1)),
            "公告：合成公告"
        );
        assert_eq!(
            live_event_label(&event(LiveEventKind::Status, "", "看过 1234", 1)),
            "看过 1234"
        );
        let behavior = crate::core::DanmakuBehavior::default();
        // Legacy defaults: gift/guard/notice hint cards are off.
        assert!(!live_event_visible(
            &event(LiveEventKind::Gift, "甲", "礼物", 1),
            &behavior
        ));
        let gifts_on = crate::core::DanmakuBehavior {
            show_gifts: true,
            ..crate::core::DanmakuBehavior::default()
        };
        assert!(live_event_visible(
            &event(LiveEventKind::Gift, "甲", "礼物", 1),
            &gifts_on
        ));
        assert!(!live_event_visible(
            &event(LiveEventKind::Enter, "甲", "进入", 1),
            &behavior
        ));
        assert!(!live_event_visible(
            &event(LiveEventKind::Notice, "", "公告", 1),
            &behavior
        ));
        assert!(!live_event_visible(
            &event(LiveEventKind::Status, "", "看过 1", 1),
            &behavior
        ));
        let all = crate::core::DanmakuBehavior {
            show_gifts: true,
            show_enters: true,
            show_guards: true,
            show_notice: true,
            show_live_stats: true,
            ..crate::core::DanmakuBehavior::default()
        };
        for kind in [
            LiveEventKind::Gift,
            LiveEventKind::Enter,
            LiveEventKind::Guard,
            LiveEventKind::Notice,
            LiveEventKind::Status,
            LiveEventKind::Follow,
        ] {
            assert!(
                live_event_visible(&event(kind, "甲", "x", 1), &all),
                "{kind:?}"
            );
        }
    }
}
