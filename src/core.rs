//! Domain values and an explicitly located, transactional local store.
//!
//! Opening a store never searches another application's configuration. Imports
//! accept caller-provided JSON only; credentials and browser sessions are not
//! part of the import format.

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::thread;
use std::time::{Duration, Instant};
use url::Url;

pub const HISTORY_LIMIT: usize = 100;
const FAVORITES_LIMIT: usize = 10_000;
const MAX_STATE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ROOM_BYTES: usize = 128;
const MAX_NAME_CHARS: usize = 256;
const SCHEMA_VERSION: u32 = 1;
const STATE_FILE: &str = "state.json";
const LOCK_FILE: &str = ".store.lock";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    #[serde(alias = "bilibili", alias = "Bili")]
    Bili,
    #[serde(alias = "Huya")]
    Huya,
    #[serde(alias = "Douyu")]
    Douyu,
    #[serde(alias = "Douyin")]
    Douyin,
}

impl Platform {
    pub const ALL: [Self; 4] = [Self::Bili, Self::Huya, Self::Douyu, Self::Douyin];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Bili => "B站",
            Self::Huya => "虎牙",
            Self::Douyu => "斗鱼",
            Self::Douyin => "抖音",
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Bili => "bili",
            Self::Huya => "huya",
            Self::Douyu => "douyu",
            Self::Douyin => "douyin",
        })
    }
}

impl FromStr for Platform {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "bili" | "bilibili" | "b站" => Ok(Self::Bili),
            "huya" | "虎牙" => Ok(Self::Huya),
            "douyu" | "斗鱼" => Ok(Self::Douyu),
            "douyin" | "抖音" => Ok(Self::Douyin),
            _ => bail!("Unsupported platform; use bili, huya, douyu, or douyin"),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    #[default]
    #[serde(alias = "Best", alias = "source")]
    Best,
    #[serde(alias = "High")]
    High,
    #[serde(alias = "Low")]
    Low,
}

impl fmt::Display for Quality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Best => "best",
            Self::High => "high",
            Self::Low => "low",
        })
    }
}

impl FromStr for Quality {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "best" | "source" => Ok(Self::Best),
            "high" => Ok(Self::High),
            "low" => Ok(Self::Low),
            _ => bail!("Unsupported quality; use best, high, or low"),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Player {
    #[default]
    #[serde(alias = "Mpv")]
    Mpv,
    #[serde(alias = "PotPlayer")]
    PotPlayer,
}

impl fmt::Display for Player {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Mpv => "mpv",
            Self::PotPlayer => "potplayer",
        })
    }
}

impl FromStr for Player {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "mpv" => Ok(Self::Mpv),
            "potplayer" => Ok(Self::PotPlayer),
            _ => bail!("Unsupported player; use mpv or potplayer"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Target {
    pub platform: Platform,
    pub room: String,
    pub quality: Quality,
}

impl Target {
    pub fn new(platform: Platform, room: impl Into<String>) -> Result<Self> {
        let room = room.into().trim().to_owned();
        let target = Self {
            platform,
            room,
            quality: Quality::default(),
        };
        target.validate()?;
        Ok(target)
    }

    /// Parse a platform-prefixed room, an official room URL, or a bare Bili ID.
    ///
    /// Official URL hosts and room segments mirror the legacy launcher
    /// (livehub-storage.cs Parse): Bilibili also exposes `/blanc/<room>` and
    /// `/h5/<room>`; Douyu accepts the mobile host `m.douyu.com`; Douyin accepts
    /// `www.douyin.com`. Extra path segments are ignored exactly like the legacy
    /// `parts[0]` rule, except the Bilibili prefixed forms that use `parts[1]`.
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        ensure!(!input.is_empty(), "Enter a room ID or an official room URL");
        ensure!(input.len() <= 4096, "Room input is too long");
        if input.contains("://") {
            let url = Url::parse(input).context("Invalid room URL")?;
            ensure!(
                matches!(url.scheme(), "http" | "https"),
                "Room URL must use HTTP or HTTPS"
            );
            ensure!(
                url.username().is_empty() && url.password().is_none(),
                "Room URLs cannot contain credentials"
            );
            ensure!(
                url.port().is_none(),
                "Room URL must use the platform's default port"
            );
            let platform = match url.host_str() {
                Some("live.bilibili.com") => Platform::Bili,
                Some("huya.com" | "www.huya.com") => Platform::Huya,
                Some("douyu.com" | "www.douyu.com" | "m.douyu.com") => Platform::Douyu,
                Some("live.douyin.com" | "www.douyin.com") => Platform::Douyin,
                _ => bail!("Use an official live-room URL or platform:room"),
            };
            return Self::new(platform, room_from_official_path(platform, url.path())?);
        }
        if let Some((platform, room)) = input.split_once(':') {
            return Self::new(platform.parse()?, room);
        }
        Self::new(Platform::Bili, input)
    }

    /// Validate again at an I/O boundary because callers can construct fields.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.room.is_empty() && self.room.len() <= MAX_ROOM_BYTES,
            "Room ID is empty or too long"
        );
        match self.platform {
            Platform::Bili | Platform::Douyin => {
                ensure!(
                    self.room.bytes().all(|c| c.is_ascii_digit()),
                    "This platform requires a numeric room ID"
                );
            }
            Platform::Huya | Platform::Douyu => {
                ensure!(
                    self.room
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-')),
                    "Room ID may contain only ASCII letters, numbers, underscores, and hyphens"
                );
            }
        }
        Ok(())
    }
}

/// Room segment of an official live-room URL, following the legacy launcher's
/// `AbsolutePath` split: the first non-empty segment, except Bilibili's
/// `/blanc/<room>` and `/h5/<room>` page forms that carry the room second.
fn room_from_official_path(platform: Platform, path: &str) -> Result<String> {
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let segment = match platform {
        Platform::Bili if matches!(parts.first().copied(), Some("blanc" | "h5")) => {
            parts.get(1).copied()
        }
        _ => parts.first().copied(),
    };
    match segment {
        Some(segment) => Ok(segment.to_owned()),
        None => bail!("Room URL must include a room ID"),
    }
}

impl<'de> Deserialize<'de> for Target {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            platform: Platform,
            room: String,
            #[serde(default)]
            quality: Quality,
        }
        let fields = Fields::deserialize(deserializer)?;
        let target = Target {
            platform: fields.platform,
            room: fields.room,
            quality: fields.quality,
        };
        target.validate().map_err(serde::de::Error::custom)?;
        Ok(target)
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.platform, self.room)
    }
}

impl FromStr for Target {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub mpv: Option<PathBuf>,
    pub potplayer: Option<PathBuf>,
    pub yt_dlp: Option<PathBuf>,
    pub legacy_exe: Option<PathBuf>,
    pub overlay: OverlaySettings,
    pub playback: PlaybackPreferences,
    /// Persisted launcher theme (legacy DanmakuConfig.DarkMode). The UI applies
    /// it on start-up and saves it when the user toggles the theme.
    pub dark_mode: bool,
    /// Start the danmaku overlay together with the player (legacy AutoStart).
    pub auto_start: bool,
    /// Launcher/UI font family key: auto / harmony / yahei-ui / yahei / system
    /// (legacy DanmakuConfig.UiFontKey).
    pub ui_font_key: String,
    /// Launcher/UI font size percentage (legacy UiFontPercent, 85-130).
    pub ui_font_percent: u8,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            mpv: None,
            potplayer: None,
            yt_dlp: None,
            legacy_exe: None,
            overlay: OverlaySettings::default(),
            playback: PlaybackPreferences::default(),
            dark_mode: true,
            auto_start: true,
            ui_font_key: "auto".into(),
            ui_font_percent: 100,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BufferMode {
    #[default]
    LowLatency,
    Stable,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Enhancement {
    #[default]
    Off,
    Light,
    High,
}

/// How the glyph edge is painted. Mirrors the legacy outline / heavy / shadow
/// choices; the overlay maps these to concrete paint offsets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextBorderStyle {
    #[default]
    Outline,
    Heavy,
    Shadow,
}

/// How a server-provided image emoticon (B站 large 表情) is sized next to text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmoteSizeMode {
    /// No larger than the surrounding glyph height.
    Small,
    /// Follow the size implied by the platform image dimensions (default).
    #[default]
    Platform,
    /// Allow a large emoticon to occupy several text rows.
    Large,
}

/// Paid-message (SC) presentation. `Off` hides the card stack completely.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScMode {
    /// Full card with user, price and body text (default).
    #[default]
    Full,
    /// One compact line per paid message.
    Compact,
    /// Only a count badge in the chosen corner.
    Badge,
    /// Paid messages are not drawn.
    Off,
}

/// Which screen corner the paid-message cards occupy. Serialized as the legacy
/// short ids `tr`/`br`/`tl`/`bl` so older hand-edited files keep working.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScCorner {
    #[default]
    #[serde(rename = "tr", alias = "top_right")]
    TopRight,
    #[serde(rename = "br", alias = "bottom_right")]
    BottomRight,
    #[serde(rename = "tl", alias = "top_left")]
    TopLeft,
    #[serde(rename = "bl", alias = "bottom_left")]
    BottomLeft,
}

impl ScCorner {
    pub const fn left(self) -> bool {
        matches!(self, Self::TopLeft | Self::BottomLeft)
    }
    pub const fn bottom(self) -> bool {
        matches!(self, Self::BottomLeft | Self::BottomRight)
    }
}

/// A rectangle in integer percentages of the player picture. Used for the
/// "reserved area" bands where danmaku must not draw (subtitles, title bars).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PercentRect {
    pub x: u8,
    pub y: u8,
    pub width: u8,
    pub height: u8,
}

impl PercentRect {
    pub const MAX_AREAS: usize = 12;

    /// Parses the legacy `x,y,width,height` form. Coordinates are clamped the
    /// same way the legacy canvas encoded them, so a bad drag cannot produce an
    /// area outside the picture.
    pub fn parse(text: &str) -> Option<Self> {
        let values: Vec<u8> = text
            .split(',')
            .map(|part| part.trim().parse::<u8>())
            .collect::<std::result::Result<_, _>>()
            .ok()?;
        let [x, y, width, height] = values.as_slice() else {
            return None;
        };
        if *width == 0 || *height == 0 {
            return None;
        }
        let x = (*x).min(99);
        let y = (*y).min(99);
        Some(Self {
            x,
            y,
            width: (*width).min(100 - x),
            height: (*height).min(100 - y),
        })
    }

    pub fn encode(self) -> String {
        format!("{},{},{},{}", self.x, self.y, self.width, self.height)
    }

    /// Whether a band [top, bottom) of the picture, given in percent, overlaps
    /// this reserved rectangle. The renderer uses it to skip danmaku lanes.
    pub fn overlaps_band(self, top_percent: f32, bottom_percent: f32) -> bool {
        let area_top = f32::from(self.y);
        let area_bottom = f32::from(self.y) + f32::from(self.height);
        top_percent < area_bottom && bottom_percent > area_top
    }
}

/// Playback controls use their own persisted group, independent of credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlaybackPreferences {
    pub buffer_mode: BufferMode,
    pub enhancement: Enhancement,
    pub wait_for_live: bool,
    pub monitor_room_state: bool,
    pub relay_bili: bool,
    /// Explicit local-player flags. Entries are separate arguments, never shell
    /// text, and are never populated from remote platform responses.
    pub mpv_extra_args: Vec<String>,
    pub potplayer_extra_args: Vec<String>,
}
impl Default for PlaybackPreferences {
    fn default() -> Self {
        Self {
            buffer_mode: BufferMode::LowLatency,
            enhancement: Enhancement::Off,
            wait_for_live: true,
            monitor_room_state: true,
            relay_bili: true,
            mpv_extra_args: Vec::new(),
            potplayer_extra_args: Vec::new(),
        }
    }
}
impl PlaybackPreferences {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.mpv_extra_args.len() <= 64, "mpv高级参数数量超过64项");
        for argument in &self.mpv_extra_args {
            ensure!(
                argument.starts_with("--")
                    && argument != "--"
                    && argument.len() <= 1024
                    && !argument.chars().any(char::is_control),
                "mpv高级参数必须每项一个以--开头的选项，且不含控制字符"
            );
        }
        ensure!(
            self.potplayer_extra_args.len() <= 64,
            "PotPlayer高级参数数量超过64项"
        );
        for argument in &self.potplayer_extra_args {
            ensure!(
                argument.starts_with('/')
                    && argument.len() <= 1024
                    && !argument.chars().any(char::is_control),
                "PotPlayer高级参数必须每项一个以/开头的选项"
            );
        }
        Ok(())
    }
}

/// Legacy-configurable global hotkeys (danmaku-1-config.cs Hotkey* fields).
/// Empty strings disable one binding; the window-policy/hotkey module parses
/// and registers them. Stored as human-readable accelerator strings
/// ("Ctrl+Alt+D") so the settings page can round-trip them unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HotkeyBindings {
    pub toggle_danmaku: String,
    pub cycle_sc: String,
    pub toggle_overlay: String,
    pub font_up: String,
    pub font_down: String,
    pub area_up: String,
    pub area_down: String,
    pub speed_up: String,
    pub speed_down: String,
    pub delay_less: String,
    pub delay_more: String,
}
impl Default for HotkeyBindings {
    fn default() -> Self {
        Self {
            toggle_danmaku: "Ctrl+Alt+D".into(),
            cycle_sc: "Ctrl+Alt+S".into(),
            toggle_overlay: "Ctrl+Alt+O".into(),
            font_up: "Ctrl+Alt+Up".into(),
            font_down: "Ctrl+Alt+Down".into(),
            area_up: "Ctrl+Alt+Right".into(),
            area_down: "Ctrl+Alt+Left".into(),
            speed_up: "Ctrl+Alt+PgUp".into(),
            speed_down: "Ctrl+Alt+PgDn".into(),
            delay_less: "Ctrl+Alt+[".into(),
            delay_more: "Ctrl+Alt+]".into(),
        }
    }
}
impl HotkeyBindings {
    /// All bindings in UI order (label, value).
    pub fn entries(&self) -> [(&'static str, &str); 11] {
        [
            ("toggle_danmaku", &self.toggle_danmaku),
            ("cycle_sc", &self.cycle_sc),
            ("toggle_overlay", &self.toggle_overlay),
            ("font_up", &self.font_up),
            ("font_down", &self.font_down),
            ("area_up", &self.area_up),
            ("area_down", &self.area_down),
            ("speed_up", &self.speed_up),
            ("speed_down", &self.speed_down),
            ("delay_less", &self.delay_less),
            ("delay_more", &self.delay_more),
        ]
    }
    pub fn validate(&self) -> Result<()> {
        for (name, value) in self.entries() {
            ensure!(
                value.chars().count() <= 64
                    && !value.chars().any(char::is_control)
                    && value.chars().all(|c| c.is_ascii_graphic() || c == ' '),
                "热键 {name} 格式无效"
            );
        }
        Ok(())
    }
}

/// Persisted overlay preferences. All values are validated before committing
/// the store and on load, so older state files receive safe defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OverlaySettings {
    pub enabled: bool,
    pub font_size: u16,
    /// Legacy fixed pixel speed. Deprecated: rendering now uses
    /// [`speed_percent`](Self::speed_percent) (the legacy renderer crosses the
    /// screen in a fixed duration, not at a fixed pixel speed). The field is
    /// kept so older state files still validate and round-trip.
    pub speed_px_per_sec: u16,
    /// Scroll speed as a percentage of the standard crossing time. 100 is the
    /// legacy standard (8 seconds to cross the picture); 25–400 is accepted.
    pub speed_percent: u16,
    pub opacity_percent: u8,
    pub display_area_percent: u8,
    pub max_visible: u16,
    pub superchat_enabled: bool,
    pub behavior: DanmakuBehavior,
    /// Keep glyph interiors opaque while only the card background uses opacity.
    pub solid_text: bool,
    /// Opacity of danmaku glyphs, independent of the whole-window opacity.
    pub text_opacity_percent: u8,
    pub stroke_enabled: bool,
    pub stroke_width: u8,
    pub border_style: TextBorderStyle,
    /// Scale danmaku size with the player picture instead of a fixed logical size.
    pub scale_with_window: bool,
    /// Picture height that corresponds to the standard (100%) danmaku size.
    pub scale_reference_height: u16,
    /// Line spacing percentage applied to the computed line height.
    pub line_gap_percent: u16,
    /// Prefix each message with the fan medal name and level.
    pub show_medal: bool,
    /// Prefix each message with the admiral/guard tag.
    pub show_guard: bool,
    /// Prefix each message with the user level.
    pub show_user_level: bool,
    /// Prefix each message with the user name.
    pub show_user_name: bool,
    pub emote_size: EmoteSizeMode,
    /// Draw Unicode emoji from a local colour sprite sheet when one is loaded.
    pub show_color_emoji: bool,
    /// Percent rectangles where danmaku lanes are not placed.
    pub reserved_areas: Vec<PercentRect>,
    pub sc_mode: ScMode,
    /// How many paid-message cards are shown before the oldest ones stop being
    /// painted; 0 means "as many as fit". No card is discarded early.
    pub sc_max_stack: u16,
    pub sc_scale_percent: u16,
    /// Multiplier for the platform-provided paid-message duration.
    pub sc_duration_scale_percent: u16,
    pub sc_show_avatar: bool,
    pub sc_corner: ScCorner,
    /// Draw the persistent F7/F8/F9/F10 shortcut hint in the lower-left corner.
    /// Default on; the overlay reads it live on every frame.
    pub show_shortcut_hint: bool,
    /// Paid-message cards react to clicks (close button + form cycle).
    pub sc_clickable: bool,
    /// Keep the overlay above every other window (legacy AlwaysOnTop). Default
    /// off: the overlay is glued just above the player window instead.
    pub always_on_top: bool,
    /// Only show the overlay while the player is the foreground window. The
    /// legacy default is on; window-policy module owns the concrete rule.
    pub hide_when_not_foreground: bool,
    /// Legacy-configurable global hotkeys (11 bindings).
    pub hotkeys: HotkeyBindings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DanmakuBehavior {
    pub delay_ms: u16,
    pub bottom_safe_percent: u8,
    pub blocked_keywords: Vec<String>,
    pub blocked_users: Vec<String>,
    pub merge_duplicates: bool,
    pub merge_window_ms: u16,
    pub show_gifts: bool,
    pub show_enters: bool,
    pub show_images: bool,
    pub hotkeys_enabled: bool,
    pub show_scrolling: bool,
    pub show_top: bool,
    pub show_bottom: bool,
    /// Draw B站 reverse (right-to-left mirror) danmaku.
    pub show_reverse: bool,
    /// Keep platform-coloured messages; when off only white messages are drawn.
    pub show_colored: bool,
    /// Top fixed danmaku may only use this percentage of the picture height.
    pub top_fixed_zone_percent: u8,
    /// Bottom fixed danmaku may only use this percentage of the picture height.
    pub bottom_fixed_zone_percent: u8,
    /// Maximum simultaneously visible fixed messages per mode; 0 = unlimited.
    pub max_fixed_per_mode: u8,
    /// Convert a fixed message wider than the viewport into a scrolling one.
    pub fixed_overflow_to_scroll: bool,
    /// Rate-limit admission into short waiting slots instead of dropping.
    pub smooth_admission: bool,
    /// Accepted messages per second; 0 lets the renderer derive the rate.
    pub admission_per_second: u8,
    /// How long a rate-limited message may wait for an admission slot.
    pub queue_wait_ms: u16,
    /// Bounded short queue length for rate-limited messages.
    pub queue_capacity: u8,
    /// Shrink ordinary danmaku when the screen is crowded.
    pub shrink_when_crowded: bool,
    /// Font percentage used for crowded messages (70–100).
    pub crowded_scale_percent: u8,
    /// Minimum B站/斗鱼 user level required to display (0 = no filter).
    pub min_user_level: u8,
    /// Minimum fan-medal level required to display (0 = no filter).
    pub min_medal_level: u8,
    pub show_guards: bool,
    pub show_notice: bool,
    pub show_live_stats: bool,
    /// Lifetime of one gift/enter/notice hint line, in seconds.
    pub event_duration_secs: u8,
    pub event_max_visible: u8,
    /// Maximum new live-event lines admitted per second (guard/notice may use
    /// one extra slot, mirroring the legacy EventRatePerSecond rule).
    pub event_rate_per_second: u8,
    /// Keyword rules whose matches are highlighted (plain text or re: regex).
    pub highlight_keywords: Vec<String>,
    /// Keyword rules whose matches become pinned top fixed messages.
    pub pin_keywords: Vec<String>,
    /// Highlight colour (0xRRGGBB, legacy HighlightColor default 0xFFD166).
    pub highlight_color: u32,
    /// Grow a merged (duplicate) danmaku as its repeat count climbs.
    pub enlarge_merged: bool,
    /// Upper bound for the merged-danmaku growth, in percent (100–180).
    pub merged_max_scale_percent: u16,
}
impl Default for DanmakuBehavior {
    fn default() -> Self {
        Self {
            delay_ms: 0,
            // Legacy "anti block subtitle": the bottom 20% of the picture is
            // kept free for subtitles/player UI (only when the display area is
            // larger than 40%, exactly like the legacy renderer).
            bottom_safe_percent: 20,
            blocked_keywords: Vec::new(),
            blocked_users: Vec::new(),
            merge_duplicates: true,
            merge_window_ms: 5000,
            show_gifts: false,
            show_enters: false,
            show_images: true,
            hotkeys_enabled: true,
            show_scrolling: true,
            show_top: true,
            show_bottom: true,
            show_reverse: true,
            show_colored: true,
            top_fixed_zone_percent: 40,
            bottom_fixed_zone_percent: 40,
            max_fixed_per_mode: 6,
            fixed_overflow_to_scroll: true,
            smooth_admission: true,
            admission_per_second: 0,
            queue_wait_ms: 800,
            queue_capacity: 64,
            shrink_when_crowded: true,
            crowded_scale_percent: 85,
            min_user_level: 0,
            min_medal_level: 0,
            show_guards: false,
            show_notice: false,
            show_live_stats: false,
            event_duration_secs: 5,
            event_max_visible: 3,
            event_rate_per_second: 2,
            highlight_keywords: Vec::new(),
            pin_keywords: Vec::new(),
            highlight_color: 0x00ff_d166,
            enlarge_merged: true,
            merged_max_scale_percent: 140,
        }
    }
}
impl DanmakuBehavior {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.delay_ms <= 15000
                && self.bottom_safe_percent <= 50
                && self.merge_window_ms <= 15000,
            "弹幕延迟、避让或合并时间超限"
        );
        for list in [
            &self.blocked_keywords,
            &self.blocked_users,
            &self.highlight_keywords,
            &self.pin_keywords,
        ] {
            ensure!(list.len() <= 200, "屏蔽条目最多200项");
            for item in list {
                ensure!(
                    !item.trim().is_empty()
                        && item.chars().count() <= 80
                        && !item.chars().any(char::is_control),
                    "屏蔽条目应为1–80字且不含控制字符"
                );
            }
        }
        ensure!(
            self.min_user_level <= 60 && self.min_medal_level <= 40,
            "最低用户等级应在0–60、最低勋章等级应在0–40之间"
        );
        ensure!(
            (2..=10).contains(&self.event_duration_secs),
            "富事件停留时间应在2–10秒之间"
        );
        ensure!(
            (1..=5).contains(&self.event_max_visible),
            "同屏富事件条数应在1–5条之间"
        );
        ensure!(
            (1..=5).contains(&self.event_rate_per_second),
            "富事件每秒上限应在1–5条之间"
        );
        ensure!(
            (100..=180).contains(&self.merged_max_scale_percent),
            "合并弹幕放大上限应在100–180%之间"
        );
        ensure!(
            (10..=100).contains(&self.top_fixed_zone_percent)
                && (10..=100).contains(&self.bottom_fixed_zone_percent),
            "顶部/底部固定弹幕区域应在10–100%之间"
        );
        ensure!(self.max_fixed_per_mode <= 60, "每类固定弹幕上限为60条");
        ensure!(self.admission_per_second <= 120, "每秒弹幕准入上限为120条");
        ensure!(self.queue_wait_ms <= 1000, "短队列等待不得超过1000毫秒");
        ensure!(
            (1..=200).contains(&self.queue_capacity),
            "短队列容量应在1–200条之间"
        );
        ensure!(
            (70..=100).contains(&self.crowded_scale_percent),
            "拥挤字号应在70–100%之间"
        );
        Ok(())
    }
}

impl Default for OverlaySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            font_size: 25,
            speed_px_per_sec: 125,
            speed_percent: 100,
            // Legacy default OpacityPercent = 90 (applies to SC surfaces while
            // SolidText keeps glyph interiors opaque).
            opacity_percent: 90,
            display_area_percent: 100,
            max_visible: 150,
            superchat_enabled: true,
            behavior: DanmakuBehavior::default(),
            solid_text: true,
            text_opacity_percent: 100,
            stroke_enabled: true,
            stroke_width: 1,
            border_style: TextBorderStyle::Outline,
            scale_with_window: true,
            scale_reference_height: 720,
            line_gap_percent: 100,
            show_medal: false,
            show_guard: false,
            show_user_level: false,
            show_user_name: false,
            emote_size: EmoteSizeMode::Platform,
            show_color_emoji: true,
            reserved_areas: Vec::new(),
            sc_mode: ScMode::Full,
            sc_max_stack: 6,
            sc_scale_percent: 100,
            sc_duration_scale_percent: 100,
            sc_show_avatar: true,
            sc_corner: ScCorner::TopRight,
            show_shortcut_hint: true,
            sc_clickable: true,
            always_on_top: false,
            hide_when_not_foreground: true,
            hotkeys: HotkeyBindings::default(),
        }
    }
}

impl OverlaySettings {
    pub fn validate(&self) -> Result<()> {
        self.behavior.validate()?;
        ensure!(
            (12..=72).contains(&self.font_size),
            "弹幕字号应在 12–72 像素之间"
        );
        ensure!(
            (20..=600).contains(&self.speed_px_per_sec),
            "旧弹幕速度应在 20–600 像素/秒之间"
        );
        ensure!(
            (25..=400).contains(&self.speed_percent),
            "弹幕速度百分比应在 25–400% 之间"
        );
        ensure!(
            (10..=100).contains(&self.opacity_percent),
            "弹幕不透明度应在 10–100% 之间"
        );
        ensure!(
            (10..=100).contains(&self.display_area_percent),
            "弹幕显示区域应在 10–100% 之间"
        );
        ensure!(
            (10..=500).contains(&self.max_visible),
            "同屏弹幕上限应在 10–500 条之间"
        );
        ensure!(
            (10..=100).contains(&self.text_opacity_percent),
            "弹幕自身不透明度应在 10–100% 之间"
        );
        ensure!(
            (1..=4).contains(&self.stroke_width),
            "描边宽度应在 1–4 像素之间"
        );
        ensure!(
            (360..=2160).contains(&self.scale_reference_height),
            "随画面缩放基准高度应在 360–2160 像素之间"
        );
        ensure!(
            (60..=250).contains(&self.line_gap_percent),
            "行距百分比应在 60–250 之间"
        );
        ensure!(
            self.reserved_areas.len() <= PercentRect::MAX_AREAS,
            "画面安全区最多12个"
        );
        for area in &self.reserved_areas {
            ensure!(
                area.width > 0
                    && area.height > 0
                    && u16::from(area.x) + u16::from(area.width) <= 100
                    && u16::from(area.y) + u16::from(area.height) <= 100,
                "画面安全区必须落在0–100%范围内"
            );
        }
        ensure!(self.sc_max_stack <= 200, "SC显示条数上限为200");
        ensure!(
            (60..=200).contains(&self.sc_scale_percent),
            "SC缩放应在 60–200% 之间"
        );
        ensure!(
            (20..=400).contains(&self.sc_duration_scale_percent),
            "SC停留时间倍率应在 20–400% 之间"
        );
        self.hotkeys.validate()?;
        Ok(())
    }
}

/// Reads only the persisted overlay preferences from a data directory
/// (state.json). Used by the overlay's hot-reload poll: a malformed or
/// half-written document simply keeps the previous in-memory options.
pub fn load_overlay_settings(dir: &std::path::Path) -> Option<OverlaySettings> {
    let text = std::fs::read(dir.join("state.json")).ok()?;
    #[derive(serde::Deserialize)]
    struct Document {
        #[serde(default)]
        settings: Settings,
    }
    let document: Document = serde_json::from_slice(&text).ok()?;
    document
        .settings
        .overlay
        .validate()
        .ok()
        .map(|_| document.settings.overlay)
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            matches!(
                self.ui_font_key.as_str(),
                "auto" | "harmony" | "yahei-ui" | "yahei" | "system"
            ),
            "界面字体档位无效"
        );
        ensure!(
            (85..=130).contains(&self.ui_font_percent),
            "界面字号百分比应在 85–130 之间"
        );
        self.overlay.validate()?;
        self.playback.validate()?;
        for path in [&self.mpv, &self.potplayer, &self.yt_dlp, &self.legacy_exe]
            .into_iter()
            .flatten()
        {
            ensure!(
                !path.as_os_str().is_empty(),
                "An executable path cannot be empty"
            );
            ensure!(
                !path.to_string_lossy().contains('\0'),
                "An executable path contains a null character"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedStream {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Favorite {
    pub name: String,
    pub target: Target,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryEntry {
    pub name: String,
    pub target: Target,
    /// Missing for imported entries whose original format has no full timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub played_at: Option<DateTime<Utc>>,
    /// Preserve an incomplete legacy display time without inventing its year.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_time: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredState {
    schema_version: u32,
    #[serde(default)]
    settings: Settings,
    #[serde(default)]
    favorites: Vec<Favorite>,
    #[serde(default)]
    history: Vec<HistoryEntry>,
}

impl Default for StoredState {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            settings: Settings::default(),
            favorites: Vec::new(),
            history: Vec::new(),
        }
    }
}

impl StoredState {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == SCHEMA_VERSION,
            "Unsupported store schema; the existing file was left intact"
        );
        self.settings.validate()?;
        ensure!(
            self.favorites.len() <= FAVORITES_LIMIT,
            "Too many favorites"
        );
        ensure!(
            self.history.len() <= HISTORY_LIMIT,
            "History exceeds the configured limit"
        );
        let mut favorites = BTreeSet::new();
        for entry in &self.favorites {
            validate_entry(&entry.name, &entry.target)?;
            ensure!(
                favorites.insert(identity(&entry.target)),
                "Duplicate favorite in store"
            );
        }
        let mut history = BTreeSet::new();
        for entry in &self.history {
            validate_history(entry)?;
            ensure!(
                history.insert(identity(&entry.target)),
                "Duplicate history entry in store"
            );
        }
        Ok(())
    }
}

/// Getters expose the last successfully loaded/committed snapshot. Each mutation
/// locks and reloads the disk state before applying its individual change.
#[derive(Debug)]
pub struct Store {
    root: PathBuf,
    state: StoredState,
}

impl Store {
    pub fn open(root: PathBuf) -> Result<Self> {
        ensure!(
            !root.as_os_str().is_empty(),
            "Choose an explicit store directory"
        );
        reject_link(&root)?;
        fs::create_dir_all(&root).context("Cannot create store directory")?;
        let _lock = StoreLock::acquire(&root)?;
        let state = read_state(&root)?;
        Ok(Self { root, state })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn settings(&self) -> &Settings {
        &self.state.settings
    }
    pub fn favorites(&self) -> &[Favorite] {
        &self.state.favorites
    }
    pub fn history(&self) -> &[HistoryEntry] {
        &self.state.history
    }
    pub fn reload(&mut self) -> Result<()> {
        let _lock = StoreLock::acquire(&self.root)?;
        let state = read_state(&self.root)?;
        self.state = state;
        Ok(())
    }

    pub fn save_settings(&mut self, settings: Settings) -> Result<()> {
        settings.validate()?;
        self.transaction(move |state| {
            state.settings = settings;
            Ok(())
        })
    }

    /// Update only overlay preferences after reloading under the write lock.
    /// A stale settings editor cannot replace another instance's player paths.
    pub fn save_overlay_settings(&mut self, overlay: OverlaySettings) -> Result<()> {
        overlay.validate()?;
        self.transaction(move |state| {
            state.settings.overlay = overlay;
            Ok(())
        })
    }

    /// Merge only playback controls under the store lock; do not revert another
    /// launcher's credential paths or overlay preferences.
    pub fn save_playback_preferences(&mut self, preferences: PlaybackPreferences) -> Result<()> {
        preferences.validate()?;
        self.transaction(move |state| {
            state.settings.playback = preferences;
            Ok(())
        })
    }

    /// Save the path editor's fields without reverting overlay preferences
    /// committed by another running instance after this editor was opened.
    pub fn save_player_paths(&mut self, settings: Settings) -> Result<()> {
        self.transaction(move |state| {
            state.settings.mpv = settings.mpv;
            state.settings.potplayer = settings.potplayer;
            state.settings.yt_dlp = settings.yt_dlp;
            state.settings.legacy_exe = settings.legacy_exe;
            state.settings.validate()
        })
    }

    pub fn add_favorite(&mut self, name: String, target: Target) -> Result<()> {
        let name = normalized_name(name)?;
        target.validate()?;
        self.transaction(move |state| {
            upsert_favorite(&mut state.favorites, Favorite { name, target });
            Ok(())
        })
    }

    pub fn remove_favorite(&mut self, platform: Platform, room: &str) -> Result<()> {
        let target = Target::new(platform, room)?;
        self.transaction(move |state| {
            state
                .favorites
                .retain(|entry| !same_room(&entry.target, &target));
            Ok(())
        })
    }

    pub fn record_play(&mut self, name: String, target: Target) -> Result<()> {
        let name = normalized_name(name)?;
        target.validate()?;
        self.transaction(move |state| {
            state
                .history
                .retain(|entry| !same_room(&entry.target, &target));
            state.history.insert(
                0,
                HistoryEntry {
                    name,
                    target,
                    played_at: Some(Utc::now()),
                    legacy_time: None,
                },
            );
            state.history.truncate(HISTORY_LIMIT);
            Ok(())
        })
    }

    /// Import only explicitly supplied favorites/history, never application
    /// settings, credentials, profiles, or files discovered on disk.
    ///
    /// Accepted formats: `{ "favorites": [...], "history": [...] }` with domain
    /// entries, a legacy `[{"name":"Example","room":"huya:example"}]` list,
    /// or a legacy `{ "Example": "bili:123" }` favorite map.
    pub fn import_json(&mut self, json: &str) -> Result<ImportSummary> {
        ensure!(json.len() as u64 <= MAX_STATE_BYTES, "Import is too large");
        let value: serde_json::Value = serde_json::from_str(json).context("Invalid import JSON")?;
        let import = if value.get("favorites").is_some() || value.get("history").is_some() {
            serde_json::from_value::<ImportData>(value).context("Invalid structured import")?
        } else {
            ImportData {
                favorites: parse_legacy_favorites(value)?,
                history: Vec::new(),
            }
        };
        self.import_data(import)
    }

    /// Import the legacy platform-keyed history document. Its short display time
    /// is retained as text because the format omits the year and time zone.
    pub fn import_legacy_history_json(&mut self, json: &str) -> Result<ImportSummary> {
        ensure!(json.len() as u64 <= MAX_STATE_BYTES, "Import is too large");
        let groups: BTreeMap<String, Vec<LegacyHistory>> =
            serde_json::from_str(json).context("Invalid legacy history JSON")?;
        let mut history = Vec::new();
        for (platform, entries) in groups {
            let platform: Platform = platform.parse()?;
            for entry in entries {
                let target = if entry.room.contains(':') || entry.room.contains("://") {
                    Target::parse(&entry.room)?
                } else {
                    Target::new(platform, entry.room)?
                };
                ensure!(
                    target.platform == platform,
                    "Imported history platform does not match its room"
                );
                history.push(HistoryEntry {
                    name: normalized_name(entry.name)?,
                    target,
                    played_at: None,
                    legacy_time: entry.at,
                });
            }
        }
        self.import_data(ImportData {
            favorites: Vec::new(),
            history,
        })
    }

    fn import_data(&mut self, mut import: ImportData) -> Result<ImportSummary> {
        ensure!(
            import.favorites.len() <= FAVORITES_LIMIT,
            "Too many imported favorites"
        );
        for favorite in &mut import.favorites {
            favorite.name = normalized_name(std::mem::take(&mut favorite.name))?;
            favorite.target.validate()?;
        }
        for entry in &mut import.history {
            entry.name = normalized_name(std::mem::take(&mut entry.name))?;
            validate_history(entry)?;
        }
        self.transaction(move |state| {
            let mut summary = ImportSummary::default();
            for entry in import.favorites {
                if upsert_favorite(&mut state.favorites, entry) {
                    summary.favorites_updated += 1;
                } else {
                    summary.favorites_added += 1;
                }
            }
            // Imported history is older than already recorded local playback.
            for entry in import.history {
                if state.history.len() >= HISTORY_LIMIT
                    || state
                        .history
                        .iter()
                        .any(|old| same_room(&old.target, &entry.target))
                {
                    summary.history_skipped += 1;
                } else {
                    state.history.push(entry);
                    summary.history_added += 1;
                }
            }
            Ok(summary)
        })
    }

    fn transaction<T>(
        &mut self,
        operation: impl FnOnce(&mut StoredState) -> Result<T>,
    ) -> Result<T> {
        reject_link(&self.root)?;
        let _lock = StoreLock::acquire(&self.root)?;
        let mut latest = read_state(&self.root)?;
        let result = operation(&mut latest)?;
        latest.validate()?;
        atomic_write_state(&self.root, &latest)?;
        self.state = latest;
        Ok(result)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportSummary {
    pub favorites_added: usize,
    pub favorites_updated: usize,
    pub history_added: usize,
    pub history_skipped: usize,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ImportData {
    favorites: Vec<Favorite>,
    history: Vec<HistoryEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyFavorite {
    name: String,
    room: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyHistory {
    #[serde(rename = "Room", alias = "room")]
    room: String,
    #[serde(rename = "Name", alias = "name")]
    name: String,
    #[serde(default, rename = "At", alias = "at")]
    at: Option<String>,
}

fn parse_legacy_favorites(value: serde_json::Value) -> Result<Vec<Favorite>> {
    match value {
        serde_json::Value::Array(values) => values
            .into_iter()
            .map(|entry| {
                // A list may also be a direct export of the new favorite entries.
                if entry.get("target").is_some() {
                    Ok(serde_json::from_value::<Favorite>(entry)
                        .context("Invalid favorite entry")?)
                } else {
                    let entry: LegacyFavorite =
                        serde_json::from_value(entry).context("Invalid legacy favorite entry")?;
                    Ok(Favorite {
                        name: normalized_name(entry.name)?,
                        target: Target::parse(&entry.room)?,
                    })
                }
            })
            .collect(),
        serde_json::Value::Object(values) => values
            .into_iter()
            .map(|(name, room)| {
                let room = room
                    .as_str()
                    .context("Legacy favorite rooms must be strings")?;
                Ok(Favorite {
                    name: normalized_name(name)?,
                    target: Target::parse(room)?,
                })
            })
            .collect(),
        _ => bail!("Import must contain a favorite list, favorite map, or structured export"),
    }
}

fn normalized_name(name: String) -> Result<String> {
    let name = name.trim().to_owned();
    ensure!(!name.is_empty(), "A display name cannot be empty");
    ensure!(
        name.chars().count() <= MAX_NAME_CHARS,
        "Display name is too long"
    );
    ensure!(
        !name.chars().any(char::is_control),
        "Display name cannot contain control characters"
    );
    Ok(name)
}

fn validate_entry(name: &str, target: &Target) -> Result<()> {
    target.validate()?;
    ensure!(
        normalized_name(name.to_owned())? == name,
        "Stored display name must not have surrounding whitespace"
    );
    Ok(())
}

fn validate_history(entry: &HistoryEntry) -> Result<()> {
    validate_entry(&entry.name, &entry.target)?;
    if let Some(time) = &entry.legacy_time {
        ensure!(
            time.len() <= 128 && !time.chars().any(char::is_control),
            "Invalid legacy display time"
        );
    }
    Ok(())
}

fn identity(target: &Target) -> (Platform, &str) {
    (target.platform, target.room.as_str())
}
fn same_room(left: &Target, right: &Target) -> bool {
    identity(left) == identity(right)
}

/// Returns whether an existing favorite was updated. Quality is an attribute of
/// a room entry, so choosing another quality cannot create a duplicate favorite.
fn upsert_favorite(favorites: &mut Vec<Favorite>, entry: Favorite) -> bool {
    if let Some(position) = favorites
        .iter()
        .position(|old| same_room(&old.target, &entry.target))
    {
        favorites[position] = entry;
        true
    } else {
        favorites.push(entry);
        false
    }
}

fn reject_link(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            !metadata.file_type().is_symlink(),
            "Store paths cannot be symbolic links"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error).context("Cannot inspect store path"),
    }
    Ok(())
}

fn read_state(root: &Path) -> Result<StoredState> {
    let path = root.join(STATE_FILE);
    reject_link(&path)?;
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StoredState::default());
        }
        Err(error) => {
            return Err(error).context("Cannot read store; the existing file was left intact");
        }
    };
    let metadata = file.metadata().context("Cannot inspect state file")?;
    ensure!(metadata.is_file(), "Store state must be a regular file");
    ensure!(
        metadata.len() <= MAX_STATE_BYTES,
        "Store is too large; the existing file was left intact"
    );
    let mut bytes = Vec::new();
    file.take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("Cannot read store")?;
    ensure!(
        bytes.len() as u64 <= MAX_STATE_BYTES,
        "Store is too large; the existing file was left intact"
    );
    let state: StoredState = serde_json::from_slice(&bytes)
        .context("Invalid store JSON; the existing file was left intact")?;
    state.validate()?;
    Ok(state)
}

fn atomic_write_state(root: &Path, state: &StoredState) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(state).context("Cannot serialize store")?;
    ensure!(
        bytes.len() as u64 <= MAX_STATE_BYTES,
        "Store would exceed its size limit"
    );
    let destination = root.join(STATE_FILE);
    reject_link(&destination)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".state-")
        .suffix(".tmp")
        .tempfile_in(root)
        .context("Cannot prepare atomic store write")?;
    temporary
        .write_all(&bytes)
        .context("Cannot write temporary store file")?;
    temporary
        .as_file()
        .sync_all()
        .context("Cannot flush temporary store file")?;
    // persist replaces a destination atomically. Never delete or truncate the
    // previous state first; a failed replacement retains that original file.
    let mut attempt = 0u32;
    loop {
        match temporary.persist(&destination) {
            Ok(_) => break,
            Err(error) => {
                // Windows scanners can briefly hold a just-written destination
                // without delete-sharing. Retry the same durable temporary file;
                // never remove the committed state to make replacement succeed.
                let transient =
                    cfg!(windows) && matches!(error.error.raw_os_error(), Some(5 | 32 | 33));
                if !transient || attempt >= 6 {
                    return Err(error.error)
                        .context("Cannot replace store; the previous state was left intact");
                }
                temporary = error.file;
                thread::sleep(Duration::from_millis(10u64 << attempt));
                attempt += 1;
            }
        }
    }
    Ok(())
}

struct StoreLock(File);

impl StoreLock {
    fn acquire(root: &Path) -> Result<Self> {
        let path = root.join(LOCK_FILE);
        reject_link(&path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .context("Cannot open store lock")?;
        ensure!(
            file.metadata()?.is_file(),
            "Store lock must be a regular file"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Self(file)),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
                {
                    ensure!(
                        Instant::now() < deadline,
                        "Store is busy; retry the operation"
                    );
                    thread::sleep(Duration::from_millis(25));
                }
                Err(error) => return Err(error).context("Cannot lock store"),
            }
        }
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_settings_keep_older_documents_loadable_with_new_defaults() {
        // A state file written before the richer overlay model must still load;
        // every added field falls back to its documented default.
        let legacy = r#"{"enabled":true,"font_size":25,"superchat_enabled":true}"#;
        let overlay: OverlaySettings = serde_json::from_str(legacy).unwrap();
        assert_eq!(overlay, OverlaySettings::default());
        assert_eq!(overlay.sc_mode, ScMode::Full);
        assert_eq!(overlay.sc_corner, ScCorner::TopRight);
        assert_eq!(overlay.emote_size, EmoteSizeMode::Platform);
        assert!(overlay.stroke_enabled && overlay.scale_with_window);
        assert!(overlay.reserved_areas.is_empty());
        assert_eq!(overlay.behavior.event_duration_secs, 5);

        let customized = serde_json::from_str::<OverlaySettings>(
            r#"{"sc_mode":"compact","sc_corner":"bl","border_style":"shadow",
                "emote_size":"large","reserved_areas":[{"x":0,"y":85,"width":100,"height":15}],
                "behavior":{"min_user_level":6,"show_top":false}}"#,
        )
        .unwrap();
        assert_eq!(customized.sc_mode, ScMode::Compact);
        assert_eq!(customized.sc_corner, ScCorner::BottomLeft);
        assert_eq!(customized.border_style, TextBorderStyle::Shadow);
        assert_eq!(customized.emote_size, EmoteSizeMode::Large);
        assert_eq!(customized.reserved_areas[0].encode(), "0,85,100,15");
        assert_eq!(customized.behavior.min_user_level, 6);
        assert!(!customized.behavior.show_top);
        customized.validate().unwrap();

        // Unknown keys stay rejected so a typo cannot silently fall back.
        assert!(serde_json::from_str::<OverlaySettings>(r#"{"mystery":true}"#).is_err());
        assert!(serde_json::from_str::<OverlaySettings>(r#"{"sc_mode":"bogus"}"#).is_err());
        assert!(serde_json::from_str::<OverlaySettings>(r#"{"sc_corner":"middle"}"#).is_err());
    }

    #[test]
    fn new_overlay_ranges_are_validated_before_they_reach_the_store() {
        let mut overlay = OverlaySettings::default();
        for invalid in [
            OverlaySettings {
                text_opacity_percent: 9,
                ..overlay.clone()
            },
            OverlaySettings {
                stroke_width: 0,
                ..overlay.clone()
            },
            OverlaySettings {
                scale_reference_height: 0,
                ..overlay.clone()
            },
            OverlaySettings {
                line_gap_percent: 59,
                ..overlay.clone()
            },
            OverlaySettings {
                sc_scale_percent: 59,
                ..overlay.clone()
            },
            OverlaySettings {
                sc_duration_scale_percent: 401,
                ..overlay.clone()
            },
            OverlaySettings {
                sc_max_stack: 201,
                ..overlay.clone()
            },
        ] {
            assert!(invalid.validate().is_err());
        }
        overlay.reserved_areas = vec![PercentRect {
            x: 90,
            y: 0,
            width: 20,
            height: 10,
        }];
        assert!(overlay.validate().is_err());
        overlay.reserved_areas = vec![PercentRect {
            x: 0,
            y: 85,
            width: 100,
            height: 15,
        }];
        overlay.validate().unwrap();
        overlay.behavior.min_user_level = 61;
        assert!(overlay.validate().is_err());
        overlay.behavior.min_user_level = 60;
        overlay.behavior.event_duration_secs = 11;
        assert!(overlay.validate().is_err());
        overlay.behavior.event_duration_secs = 10;
        overlay.behavior.event_max_visible = 0;
        assert!(overlay.validate().is_err());
        overlay.behavior.event_max_visible = 3;
        overlay.validate().unwrap();
    }

    #[test]
    fn percent_rectangles_round_trip_and_clamp_to_the_picture() {
        let parsed = PercentRect::parse(" 0 , 85 , 100 , 15 ").unwrap();
        assert_eq!(parsed.encode(), "0,85,100,15");
        // Legacy canvas clamped before encoding; values beyond the picture are
        // narrowed instead of producing an off-screen band.
        assert_eq!(
            PercentRect::parse("90,90,20,20").unwrap(),
            PercentRect {
                x: 90,
                y: 90,
                width: 10,
                height: 10
            }
        );
        for invalid in ["", "1,2,3", "1,2,3,4,5", "0,0,0,10", "0,0,10,0", "x,1,2,3"] {
            assert!(PercentRect::parse(invalid).is_none(), "{invalid}");
        }
        let subtitle = PercentRect::parse("0,85,100,15").unwrap();
        assert!(subtitle.overlaps_band(80.0, 90.0));
        assert!(!subtitle.overlaps_band(10.0, 40.0));
        // A band touching the reserved edge does not overlap it.
        assert!(!subtitle.overlaps_band(70.0, 85.0));
    }
}
