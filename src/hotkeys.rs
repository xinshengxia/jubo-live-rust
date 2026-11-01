//! Legacy-compatible global hotkeys for the danmaku overlay.
//!
//! The legacy C# overlay registers eleven global hotkeys through RegisterHotKey
//! (danmaku-1-config.cs / danmaku-4-overlay.cs). This module owns the
//! accelerator-string grammar, the default binding table, duplicate / reserved-key
//! validation and the Win32 registration wrapper. It never touches the overlay
//! window: the overlay follow thread calls GlobalHotkeys::apply once, then polls
//! GlobalHotkeys::poll_events from the same thread each tick and forwards the
//! returned HotkeyEvent to its normal hotkey channel.
//!
//! The fixed F7-F10 overlay keys are owned by this module too, but in a second
//! registration range (FIXED_HOTKEY_ID_BASE) and only while the player owns the
//! foreground: the overlay calls GlobalHotkeys::set_fixed_active(foreground)
//! every follow tick. While a key is registered Windows consumes it before the
//! player sees it; a key whose registration failed stays unheld and is reported
//! through fixed_registered() so the overlay can keep its GetAsyncKeyState
//! polling fallback for exactly that key.
//!
//! Pure helpers are always available (all platforms, offline tests); the
//! registration wrapper is Windows-only and degrades to a stub elsewhere.
//!
//! Bindings are stored in crate::core::HotkeyBindings as human-readable
//! accelerator strings ("Ctrl+Alt+D"). An empty string disables one binding.

use crate::core::HotkeyBindings;

/// One user-visible overlay hotkey action (legacy danmaku.json Hotkey* fields).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HotkeyAction {
    ToggleDanmaku,
    CycleSc,
    ToggleOverlay,
    FontUp,
    FontDown,
    AreaUp,
    AreaDown,
    SpeedUp,
    SpeedDown,
    DelayLess,
    DelayMore,
}

impl HotkeyAction {
    /// Every configurable action, in legacy HotkeyBindings::entries() order.
    pub const ALL: [Self; 11] = [
        Self::ToggleDanmaku,
        Self::CycleSc,
        Self::ToggleOverlay,
        Self::FontUp,
        Self::FontDown,
        Self::AreaUp,
        Self::AreaDown,
        Self::SpeedUp,
        Self::SpeedDown,
        Self::DelayLess,
        Self::DelayMore,
    ];

    /// Stable index, also used to derive the Win32 hotkey id.
    pub const fn index(self) -> usize {
        match self {
            Self::ToggleDanmaku => 0,
            Self::CycleSc => 1,
            Self::ToggleOverlay => 2,
            Self::FontUp => 3,
            Self::FontDown => 4,
            Self::AreaUp => 5,
            Self::AreaDown => 6,
            Self::SpeedUp => 7,
            Self::SpeedDown => 8,
            Self::DelayLess => 9,
            Self::DelayMore => 10,
        }
    }

    /// The core::HotkeyBindings::entries() field name for this action.
    pub const fn field_name(self) -> &'static str {
        match self {
            Self::ToggleDanmaku => "toggle_danmaku",
            Self::CycleSc => "cycle_sc",
            Self::ToggleOverlay => "toggle_overlay",
            Self::FontUp => "font_up",
            Self::FontDown => "font_down",
            Self::AreaUp => "area_up",
            Self::AreaDown => "area_down",
            Self::SpeedUp => "speed_up",
            Self::SpeedDown => "speed_down",
            Self::DelayLess => "delay_less",
            Self::DelayMore => "delay_more",
        }
    }

    pub fn from_field_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|action| action.field_name() == name)
    }

    /// Short Chinese label for the settings page / diagnostics.
    pub const fn label_zh(self) -> &'static str {
        match self {
            Self::ToggleDanmaku => "开关弹幕",
            Self::CycleSc => "切换 SC 形态",
            Self::ToggleOverlay => "隐藏/显示覆盖层",
            Self::FontUp => "字号变大",
            Self::FontDown => "字号变小",
            Self::AreaUp => "显示区域变大",
            Self::AreaDown => "显示区域变小",
            Self::SpeedUp => "弹幕变快",
            Self::SpeedDown => "弹幕变慢",
            Self::DelayLess => "弹幕延迟减少",
            Self::DelayMore => "弹幕延迟增加",
        }
    }
}

/// Fixed overlay keys kept in addition to the configurable hotkeys: F7/F8 font
/// size, F9 danmaku layer, F10 paid messages. GlobalHotkeys registers them as
/// bare keys through set_fixed_active only while the player is foreground, so
/// the player never receives them; the mapping is shared by the settings/docs
/// and the overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FixedOverlayAction {
    FontSmaller,
    FontLarger,
    ToggleDanmaku,
    ToggleSuperChat,
}

impl FixedOverlayAction {
    /// F7, F8, F9, F10 in registration-id order.
    pub const ALL: [Self; 4] = [
        Self::FontSmaller,
        Self::FontLarger,
        Self::ToggleDanmaku,
        Self::ToggleSuperChat,
    ];

    /// Stable index, also used to derive the fixed Win32 hotkey id.
    pub const fn index(self) -> usize {
        match self {
            Self::FontSmaller => 0,
            Self::FontLarger => 1,
            Self::ToggleDanmaku => 2,
            Self::ToggleSuperChat => 3,
        }
    }

    /// Bare accelerator text ("F7") for registration errors and diagnostics.
    pub const fn spec(self) -> &'static str {
        match self {
            Self::FontSmaller => "F7",
            Self::FontLarger => "F8",
            Self::ToggleDanmaku => "F9",
            Self::ToggleSuperChat => "F10",
        }
    }

    /// Win32 virtual-key code (one of VK_F7..VK_F10).
    pub const fn vk(self) -> u32 {
        match self {
            Self::FontSmaller => VK_F7,
            Self::FontLarger => VK_F8,
            Self::ToggleDanmaku => VK_F9,
            Self::ToggleSuperChat => VK_F10,
        }
    }

    /// Short Chinese label for the overlay log / settings hint.
    pub const fn label_zh(self) -> &'static str {
        match self {
            Self::FontSmaller => "字号变小",
            Self::FontLarger => "字号变大",
            Self::ToggleDanmaku => "开关弹幕",
            Self::ToggleSuperChat => "开关 SC",
        }
    }
}

/// Win32 virtual-key codes for the fixed F7-F10 keys.
pub const VK_F7: u32 = 0x76;
pub const VK_F8: u32 = 0x77;
pub const VK_F9: u32 = 0x78;
pub const VK_F10: u32 = 0x79;

/// Maps a virtual-key code to the fixed overlay action (F7-F10 only).
pub const fn fixed_overlay_action(vk: u32) -> Option<FixedOverlayAction> {
    match vk {
        VK_F7 => Some(FixedOverlayAction::FontSmaller),
        VK_F8 => Some(FixedOverlayAction::FontLarger),
        VK_F9 => Some(FixedOverlayAction::ToggleDanmaku),
        VK_F10 => Some(FixedOverlayAction::ToggleSuperChat),
        _ => None,
    }
}

/// The first global-hotkey id used on Win32. The range below 0xBFFF is the
/// documented application range for RegisterHotKey.
pub const HOTKEY_ID_BASE: i32 = 0x4C00;

/// Resolves a registered hotkey id back to its action (wndproc bridge helper).
pub fn action_for_hotkey_id(id: i32) -> Option<HotkeyAction> {
    let index: usize = id.checked_sub(HOTKEY_ID_BASE)?.try_into().ok()?;
    HotkeyAction::ALL.get(index).copied()
}

/// The first Win32 hotkey id used for the fixed F7-F10 keys. The two
/// registration ranges never overlap, so a WM_HOTKEY id alone identifies the
/// event kind: 0x4C00..0x4C00+11 are configurable bindings, 0x4D00..0x4D03 are
/// the fixed overlay keys. Both are inside the documented application range
/// (below 0xBFFF).
pub const FIXED_HOTKEY_ID_BASE: i32 = 0x4D00;

/// Registration id for one fixed overlay key (F7=0x4D00 ... F10=0x4D03).
pub const fn fixed_hotkey_id(action: FixedOverlayAction) -> i32 {
    FIXED_HOTKEY_ID_BASE + action.index() as i32
}

/// Resolves a fixed-key registration id back to its key.
pub fn fixed_action_for_hotkey_id(id: i32) -> Option<FixedOverlayAction> {
    let index: usize = id.checked_sub(FIXED_HOTKEY_ID_BASE)?.try_into().ok()?;
    FixedOverlayAction::ALL.get(index).copied()
}

/// One drained WM_HOTKEY event: either a configurable binding action or one of
/// the fixed overlay keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    Action(HotkeyAction),
    Fixed(FixedOverlayAction),
}

impl HotkeyEvent {
    /// The configurable action, if this event is not a fixed overlay key.
    pub const fn action(self) -> Option<HotkeyAction> {
        match self {
            Self::Action(action) => Some(action),
            Self::Fixed(_) => None,
        }
    }

    /// The fixed overlay key, if this event is not a configurable binding.
    pub const fn fixed(self) -> Option<FixedOverlayAction> {
        match self {
            Self::Fixed(action) => Some(action),
            Self::Action(_) => None,
        }
    }
}

/// Classifies one WM_HOTKEY wparam. Ids outside both registration ranges (for
/// example a hotkey registered by some other manager on the same thread) are
/// ignored instead of being guessed.
pub fn event_for_hotkey_id(id: i32) -> Option<HotkeyEvent> {
    if let Some(action) = action_for_hotkey_id(id) {
        return Some(HotkeyEvent::Action(action));
    }
    fixed_action_for_hotkey_id(id).map(HotkeyEvent::Fixed)
}

/// One OS call produced by a fixed-key activation transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixedHotkeyStep {
    Register(FixedOverlayAction),
    Unregister(FixedOverlayAction),
}

/// Pure bookkeeping for the fixed F7-F10 registrations.
///
/// GlobalHotkeys keeps one of these and only talks to the OS when the requested
/// activation state differs from the current one, which lets the overlay follow
/// loop call set_fixed_active on every tick. The type is platform independent
/// and covered by the offline test suite, so the idempotency rules do not need
/// a real system-wide registration to be tested.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FixedHotkeyState {
    active: bool,
    held: [bool; 4],
}

impl FixedHotkeyState {
    pub const fn new() -> Self {
        Self {
            active: false,
            held: [false; 4],
        }
    }

    /// Applies one activation transition and returns the OS calls it requires.
    ///
    /// Empty when the state already matches `active`. Activating yields
    /// Register steps for every key not currently held; deactivating yields
    /// Unregister steps for every held key and forgets them. The bookkeeping is
    /// updated as if the returned steps succeeded; the platform layer must call
    /// record() with the real outcome of every registration attempt.
    pub fn transition(&mut self, active: bool) -> Vec<FixedHotkeyStep> {
        if self.active == active {
            return Vec::new();
        }
        self.active = active;
        let mut steps = Vec::with_capacity(FixedOverlayAction::ALL.len());
        if active {
            for action in FixedOverlayAction::ALL {
                if !self.held[action.index()] {
                    steps.push(FixedHotkeyStep::Register(action));
                }
            }
        } else {
            for action in FixedOverlayAction::ALL {
                if self.held[action.index()] {
                    self.held[action.index()] = false;
                    steps.push(FixedHotkeyStep::Unregister(action));
                }
            }
        }
        steps
    }

    /// Records whether one RegisterHotKey attempt actually took effect.
    pub fn record(&mut self, action: FixedOverlayAction, registered: bool) {
        self.held[action.index()] = registered;
    }

    /// Whether this key is currently held through RegisterHotKey.
    pub const fn holds(&self, action: FixedOverlayAction) -> bool {
        self.held[action.index()]
    }

    /// Whether the fixed keys are in their "registered while foreground" state.
    pub const fn is_active(&self) -> bool {
        self.active
    }

    /// Number of keys currently held through RegisterHotKey.
    pub fn held_count(&self) -> usize {
        self.held.iter().filter(|held| **held).count()
    }

    /// Forgets every registration (the platform layer unregisters first).
    pub fn clear(&mut self) {
        self.active = false;
        self.held = [false; 4];
    }
}

// ---- accelerator grammar --------------------------------------------------

/// Win32 modifier bit flags (MOD_ALT etc.). MOD_NOREPEAT is added by the
/// registration wrapper, not stored in the parsed accelerator.
pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CONTROL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_WIN: u32 = 0x0008;
/// MOD_NOREPEAT: holding the combo down must not repeat the action.
pub const MOD_NOREPEAT: u32 = 0x4000;

/// A parsed accelerator: modifier mask plus virtual-key code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedHotkey {
    pub modifiers: u32,
    pub vk: u32,
}

impl ParsedHotkey {
    pub const fn has_ctrl(self) -> bool {
        self.modifiers & MOD_CONTROL != 0
    }
    pub const fn has_alt(self) -> bool {
        self.modifiers & MOD_ALT != 0
    }
    pub const fn has_shift(self) -> bool {
        self.modifiers & MOD_SHIFT != 0
    }
    pub const fn has_win(self) -> bool {
        self.modifiers & MOD_WIN != 0
    }
    /// Modifier mask passed to RegisterHotKey (MOD_NOREPEAT always on).
    pub const fn register_modifiers(self) -> u32 {
        self.modifiers | MOD_NOREPEAT
    }
}

/// Parse failures are explicit: the settings page can show the offending spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyParseError {
    Empty,
    TrailingSeparator,
    UnknownToken(String),
    DuplicateModifier(String),
    MissingKey,
    MultipleKeys(String),
}

impl std::fmt::Display for HotkeyParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(formatter, "热键为空"),
            Self::TrailingSeparator => write!(formatter, "热键以 + 结尾，缺少按键"),
            Self::UnknownToken(token) => write!(formatter, "无法识别的热键片段：{token}"),
            Self::DuplicateModifier(modifier) => write!(formatter, "修饰键重复：{modifier}"),
            Self::MissingKey => write!(formatter, "热键缺少按键（只有修饰键）"),
            Self::MultipleKeys(key) => write!(formatter, "热键只能有一个按键，多余：{key}"),
        }
    }
}

impl std::error::Error for HotkeyParseError {}

/// Parses one accelerator string: modifiers (Ctrl/Control, Alt, Shift,
/// Win/Windows/Meta/Super; case-insensitive, any order, at most once) followed
/// by exactly one key. Keys: A-Z, 0-9, F1-F24, named keys
/// (Up/Down/Left/Right/PgUp/PgDn/Home/End/Insert/Delete/Backspace/Tab/Enter/
/// Escape/Space), and single punctuation keys [ ] ; ' , . / \ - = backtick.
pub fn parse_spec(spec: &str) -> Result<ParsedHotkey, HotkeyParseError> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(HotkeyParseError::Empty);
    }
    if spec.ends_with('+') {
        return Err(HotkeyParseError::TrailingSeparator);
    }
    let mut modifiers = 0_u32;
    let mut key: Option<u32> = None;
    for raw in spec.split('+') {
        let token = raw.trim();
        if token.is_empty() {
            return Err(HotkeyParseError::TrailingSeparator);
        }
        let lower = token.to_ascii_lowercase();
        let modifier = match lower.as_str() {
            "ctrl" | "control" => Some(MOD_CONTROL),
            "alt" => Some(MOD_ALT),
            "shift" => Some(MOD_SHIFT),
            "win" | "windows" | "meta" | "super" => Some(MOD_WIN),
            // Accepted for compatibility with hand-edited configs; the wrapper
            // always registers with MOD_NOREPEAT so the flag has no effect.
            "norepeat" | "no_repeat" => None,
            _ => None,
        };
        if let Some(bit) = modifier {
            if modifiers & bit != 0 {
                return Err(HotkeyParseError::DuplicateModifier(token.to_string()));
            }
            modifiers |= bit;
            continue;
        }
        let Some(vk) = virtual_key_for(token) else {
            return Err(HotkeyParseError::UnknownToken(token.to_string()));
        };
        if key.replace(vk).is_some() {
            return Err(HotkeyParseError::MultipleKeys(token.to_string()));
        }
    }
    let Some(vk) = key else {
        return Err(HotkeyParseError::MissingKey);
    };
    Ok(ParsedHotkey { modifiers, vk })
}

/// Canonical accelerator text, e.g. "Ctrl+Alt+Up", "F7", "Ctrl+Alt+[".
pub fn format_hotkey(hotkey: ParsedHotkey) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if hotkey.has_ctrl() {
        parts.push("Ctrl");
    }
    if hotkey.has_alt() {
        parts.push("Alt");
    }
    if hotkey.has_shift() {
        parts.push("Shift");
    }
    if hotkey.has_win() {
        parts.push("Win");
    }
    parts.push(key_name(hotkey.vk));
    parts.join("+")
}

/// Parses Modifier+Key into canonical format_hotkey text (or the original error).
pub fn normalize_spec(spec: &str) -> Result<String, HotkeyParseError> {
    parse_spec(spec).map(format_hotkey)
}

fn key_name(vk: u32) -> &'static str {
    match vk {
        0x08 => "Backspace",
        0x09 => "Tab",
        0x0D => "Enter",
        0x1B => "Escape",
        0x20 => "Space",
        0x21 => "PgUp",
        0x22 => "PgDn",
        0x23 => "End",
        0x24 => "Home",
        0x25 => "Left",
        0x26 => "Up",
        0x27 => "Right",
        0x28 => "Down",
        0x2D => "Insert",
        0x2E => "Delete",
        0x30..=0x39 => DIGITS[(vk - 0x30) as usize],
        0x41..=0x5A => LETTERS[(vk - 0x41) as usize],
        0x70..=0x87 => F_KEYS[(vk - 0x70) as usize],
        0xBA => ";",
        0xBB => "=",
        0xBC => ",",
        0xBD => "-",
        0xBE => ".",
        0xBF => "/",
        0xC0 => "\u{0060}",
        0xDB => "[",
        0xDC => "\\",
        0xDD => "]",
        0xDE => "'",
        _ => "?",
    }
}

const LETTERS: [&str; 26] = [
    "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R",
    "S", "T", "U", "V", "W", "X", "Y", "Z",
];
const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
const F_KEYS: [&str; 24] = [
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14",
    "F15", "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24",
];

fn virtual_key_for(token: &str) -> Option<u32> {
    let upper = token.to_ascii_uppercase();
    match upper.as_str() {
        "BACKSPACE" => return Some(0x08),
        "TAB" => return Some(0x09),
        "ENTER" | "RETURN" => return Some(0x0D),
        "ESCAPE" | "ESC" => return Some(0x1B),
        "SPACE" => return Some(0x20),
        "PAGEUP" | "PGUP" => return Some(0x21),
        "PAGEDOWN" | "PGDN" => return Some(0x22),
        "END" => return Some(0x23),
        "HOME" => return Some(0x24),
        "LEFT" | "ARROWLEFT" => return Some(0x25),
        "UP" | "ARROWUP" => return Some(0x26),
        "RIGHT" | "ARROWRIGHT" => return Some(0x27),
        "DOWN" | "ARROWDOWN" => return Some(0x28),
        "INSERT" | "INS" => return Some(0x2D),
        "DELETE" | "DEL" => return Some(0x2E),
        _ => {}
    }
    if upper.len() == 1 {
        let byte = upper.as_bytes()[0];
        return match byte {
            b'A'..=b'Z' => Some(u32::from(byte)),
            b'0'..=b'9' => Some(u32::from(byte)),
            _ => punctuation_vk(byte),
        };
    }
    if let Some(number) = upper.strip_prefix('F') {
        if let Ok(index) = number.parse::<u32>() {
            if (1..=24).contains(&index) {
                return Some(0x70 + index - 1);
            }
        }
    }
    None
}

fn punctuation_vk(byte: u8) -> Option<u32> {
    match byte {
        b';' => Some(0xBA),
        b'=' => Some(0xBB),
        b',' => Some(0xBC),
        b'-' => Some(0xBD),
        b'.' => Some(0xBE),
        b'/' => Some(0xBF),
        0x60 => Some(0xC0),
        b'[' => Some(0xDB),
        b'\\' => Some(0xDC),
        b']' => Some(0xDD),
        b'\'' => Some(0xDE),
        _ => None,
    }
}

// ---- binding tables and validation ---------------------------------------

/// One validated binding ready for registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyBinding {
    pub action: HotkeyAction,
    pub spec: String,
    pub hotkey: ParsedHotkey,
}

/// Configuration-level failures (as opposed to a single spec syntax error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyConfigError {
    Parse {
        action: HotkeyAction,
        spec: String,
        error: HotkeyParseError,
    },
    Duplicate {
        first: HotkeyAction,
        second: HotkeyAction,
        spec: String,
    },
    /// The bare F7-F10 keys are reserved by the fixed overlay keys (hint line);
    /// binding them globally would make one of the two registrations fail.
    Reserved {
        action: HotkeyAction,
        spec: String,
    },
}

impl std::fmt::Display for HotkeyConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse {
                action,
                spec,
                error,
            } => write!(
                formatter,
                "热键 {}（{}）无效：{error}",
                action.label_zh(),
                if spec.is_empty() { "空" } else { spec.as_str() }
            ),
            Self::Duplicate {
                first,
                second,
                spec,
            } => write!(
                formatter,
                "热键冲突：{} 与 {} 都绑定了 {spec}",
                first.label_zh(),
                second.label_zh()
            ),
            Self::Reserved { action, spec } => write!(
                formatter,
                "热键 {}（{spec}）与固定键 F7-F10 冲突，请加修饰键或换一个键",
                action.label_zh()
            ),
        }
    }
}

impl std::error::Error for HotkeyConfigError {}

impl HotkeyBindings {
    /// The raw spec for one action.
    pub fn spec(&self, action: HotkeyAction) -> &str {
        self.entries()
            .iter()
            .find(|(name, _)| *name == action.field_name())
            .map(|(_, value)| *value)
            .unwrap_or("")
    }

    /// Replaces one binding spec (empty string disables it).
    pub fn set_spec(&mut self, action: HotkeyAction, spec: &str) -> Result<(), HotkeyConfigError> {
        if !spec.trim().is_empty() {
            let parsed = parse_spec(spec).map_err(|error| HotkeyConfigError::Parse {
                action,
                spec: spec.to_owned(),
                error,
            })?;
            if is_bare_fixed_key(parsed) {
                return Err(HotkeyConfigError::Reserved {
                    action,
                    spec: spec.to_owned(),
                });
            }
        }
        match action {
            HotkeyAction::ToggleDanmaku => self.toggle_danmaku = spec.to_owned(),
            HotkeyAction::CycleSc => self.cycle_sc = spec.to_owned(),
            HotkeyAction::ToggleOverlay => self.toggle_overlay = spec.to_owned(),
            HotkeyAction::FontUp => self.font_up = spec.to_owned(),
            HotkeyAction::FontDown => self.font_down = spec.to_owned(),
            HotkeyAction::AreaUp => self.area_up = spec.to_owned(),
            HotkeyAction::AreaDown => self.area_down = spec.to_owned(),
            HotkeyAction::SpeedUp => self.speed_up = spec.to_owned(),
            HotkeyAction::SpeedDown => self.speed_down = spec.to_owned(),
            HotkeyAction::DelayLess => self.delay_less = spec.to_owned(),
            HotkeyAction::DelayMore => self.delay_more = spec.to_owned(),
        }
        Ok(())
    }

    /// Parses and validates every non-empty binding. Empty strings disable a
    /// binding and are skipped. Duplicate combos and the reserved bare F7-F10
    /// keys are rejected.
    pub fn parse_all(&self) -> Result<Vec<HotkeyBinding>, HotkeyConfigError> {
        let mut parsed: Vec<HotkeyBinding> = Vec::with_capacity(HotkeyAction::ALL.len());
        for (name, spec) in self.entries() {
            let Some(action) = HotkeyAction::from_field_name(name) else {
                continue;
            };
            let trimmed = spec.trim();
            if trimmed.is_empty() {
                continue;
            }
            let hotkey = parse_spec(trimmed).map_err(|error| HotkeyConfigError::Parse {
                action,
                spec: trimmed.to_owned(),
                error,
            })?;
            if is_bare_fixed_key(hotkey) {
                return Err(HotkeyConfigError::Reserved {
                    action,
                    spec: trimmed.to_owned(),
                });
            }
            if let Some(first) = parsed.iter().find(|binding| binding.hotkey == hotkey) {
                return Err(HotkeyConfigError::Duplicate {
                    first: first.action,
                    second: action,
                    spec: trimmed.to_owned(),
                });
            }
            parsed.push(HotkeyBinding {
                action,
                spec: trimmed.to_owned(),
                hotkey,
            });
        }
        Ok(parsed)
    }

    /// Bindings that would collide with the fixed F7-F10 overlay keys if those
    /// were registered at the same time (normally the fixed keys are only
    /// registered while the player is foreground, but surfacing the warning is
    /// friendlier than a silent registration failure).
    pub fn reserved_conflicts(&self) -> Vec<HotkeyBinding> {
        let mut conflicts = Vec::new();
        for (name, spec) in self.entries() {
            let Some(action) = HotkeyAction::from_field_name(name) else {
                continue;
            };
            let Ok(hotkey) = parse_spec(spec) else {
                continue;
            };
            if is_bare_fixed_key(hotkey) {
                conflicts.push(HotkeyBinding {
                    action,
                    spec: spec.trim().to_owned(),
                    hotkey,
                });
            }
        }
        conflicts
    }
}

fn is_bare_fixed_key(hotkey: ParsedHotkey) -> bool {
    hotkey.modifiers == 0 && fixed_overlay_action(hotkey.vk).is_some()
}

// ---- Win32 registration ---------------------------------------------------

/// One registration failure; the mod can keep running with the other bindings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyRegisterError {
    pub action: Option<HotkeyAction>,
    pub spec: String,
    pub code: u32,
    pub message: String,
}

impl std::fmt::Display for HotkeyRegisterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = self.action.map(|action| action.label_zh()).unwrap_or("热键");
        write!(
            formatter,
            "{label}（{}）注册失败：{} (code {})",
            self.spec, self.message, self.code
        )
    }
}

#[cfg(windows)]
pub use windows_impl::GlobalHotkeys;

#[cfg(not(windows))]
pub use stub_impl::GlobalHotkeys;

#[cfg(windows)]
mod windows_impl {
    use super::{
        FixedHotkeyState, FixedHotkeyStep, FixedOverlayAction, HotkeyAction, HotkeyBindings,
        HotkeyEvent, HotkeyRegisterError, MOD_NOREPEAT, event_for_hotkey_id, fixed_hotkey_id,
    };
    use std::ffi::c_void;

    const WM_HOTKEY: u32 = 0x0312;
    const PM_REMOVE: u32 = 0x0001;
    const HOTKEY_ID_BASE: i32 = super::HOTKEY_ID_BASE;

    #[repr(C)]
    #[derive(Default)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    struct Msg {
        hwnd: *mut c_void,
        message: u32,
        // Rust inserts the same padding the C struct has before wparam.
        wparam: usize,
        lparam: isize,
        time: u32,
        pt: Point,
        l_private: u32,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn RegisterHotKey(window: *mut c_void, id: i32, modifiers: u32, virtual_key: u32) -> i32;
        fn UnregisterHotKey(window: *mut c_void, id: i32) -> i32;
        fn PeekMessageW(
            message: *mut Msg,
            window: *mut c_void,
            filter_min: u32,
            filter_max: u32,
            remove: u32,
        ) -> i32;
    }

    /// Forces the calling thread's message queue into existence. A thread only
    /// receives WM_HOTKEY once it has used a message function, and the fixed
    /// F7-F10 keys may be the first thing this thread registers.
    fn ensure_message_queue() {
        // SAFETY: MSG is a plain C struct; zeroed is the documented "empty
        // message" initialization before PeekMessageW fills it.
        let mut message: Msg = unsafe { std::mem::zeroed() };
        unsafe {
            PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, 0);
        }
    }

    /// Thread-affine Win32 global hotkey manager.
    ///
    /// Register with hwnd = null so WM_HOTKEY lands in the calling thread's
    /// message queue; GlobalHotkeys::poll must be called from that same thread
    /// (the overlay follow thread). Registering/unregistering is best-effort:
    /// one unavailable combination never stops the others and the failure is
    /// returned as HotkeyRegisterError instead of panicking.
    #[derive(Debug, Default)]
    pub struct GlobalHotkeys {
        registered: Vec<i32>,
        errors: Vec<HotkeyRegisterError>,
        owner_thread: Option<std::thread::ThreadId>,
        fixed: FixedHotkeyState,
        fixed_errors: Vec<HotkeyRegisterError>,
    }

    impl GlobalHotkeys {
        pub fn new() -> Self {
            Self::default()
        }

        /// Replaces every registration with the given bindings. Returns failures
        /// (empty when everything registered). Invalid spec text is reported
        /// instead of panicking; a valid table registers every entry.
        pub fn apply(&mut self, bindings: &HotkeyBindings) -> Vec<HotkeyRegisterError> {
            // Only the configurable range is replaced here; fixed F7-F10
            // registrations are owned by set_fixed_active and must survive a
            // settings re-apply (clear() drops both).
            self.clear_configurable();
            self.owner_thread = Some(std::thread::current().id());
            // RegisterHotKey(NULL, ...) posts WM_HOTKEY to the calling thread queue;
            // a thread that has not used a message function yet has no queue, so
            // force-create it before the first registration.
            ensure_message_queue();
            let parsed = match bindings.parse_all() {
                Ok(parsed) => parsed,
                Err(error) => {
                    // A config-level error (duplicate/reserved) rejects the whole
                    // table; report it once and keep the overlay running.
                    let failure = HotkeyRegisterError {
                        action: None,
                        spec: String::new(),
                        code: 0,
                        message: error.to_string(),
                    };
                    self.errors = vec![failure.clone()];
                    return vec![failure];
                }
            };
            let mut errors = Vec::new();
            for binding in parsed {
                let id = HOTKEY_ID_BASE + binding.action.index() as i32;
                let modifiers = binding.hotkey.register_modifiers();
                let registered =
                    unsafe { RegisterHotKey(std::ptr::null_mut(), id, modifiers, binding.hotkey.vk) };
                if registered == 0 {
                    errors.push(HotkeyRegisterError {
                        action: Some(binding.action),
                        spec: binding.spec.clone(),
                        code: 0,
                        message: "RegisterHotKey 失败（组合被其他程序占用或权限不足）".into(),
                    });
                    continue;
                }
                self.registered.push(id);
            }
            self.errors = errors.clone();
            errors
        }

        /// Drains pending WM_HOTKEY messages for this thread, classifying both
        /// the configurable range and the fixed F7-F10 range. Safe to call when
        /// nothing is registered; never blocks.
        ///
        /// Must run on the thread that registered the hotkeys (the overlay
        /// follow thread): RegisterHotKey(NULL, ...) posts WM_HOTKEY to the
        /// calling thread's message queue.
        pub fn poll_events(&mut self) -> Vec<HotkeyEvent> {
            debug_assert!(
                self.owner_thread
                    .is_none_or(|owner| owner == std::thread::current().id()),
                "GlobalHotkeys::poll_events must run on the thread that registered the hotkeys"
            );
            let mut events = Vec::new();
            loop {
                // SAFETY: MSG is a plain C struct; zeroed is the documented
                // "empty message" initialization before PeekMessageW fills it.
                let mut message: Msg = unsafe { std::mem::zeroed() };
                let found = unsafe {
                    PeekMessageW(
                        &mut message,
                        std::ptr::null_mut(),
                        WM_HOTKEY,
                        WM_HOTKEY,
                        PM_REMOVE,
                    )
                };
                if found == 0 {
                    break;
                }
                if let Some(event) = event_for_hotkey_id(message.wparam as i32) {
                    events.push(event);
                }
            }
            events
        }

        /// Configurable-hotkey view of poll_events(); fixed F7-F10 events are
        /// dropped (the overlay consumes them through poll_events instead).
        pub fn poll(&mut self) -> Vec<HotkeyAction> {
            self.poll_events()
                .into_iter()
                .filter_map(HotkeyEvent::action)
                .collect()
        }

        /// Whether a registered hotkey id belongs to this manager.
        pub fn is_registered(&self, action: HotkeyAction) -> bool {
            let id = HOTKEY_ID_BASE + action.index() as i32;
            self.registered.contains(&id)
        }

        /// Registers (active = true) or unregisters (active = false) the four
        /// bare fixed keys F7/F8/F9/F10 on this thread's message queue, with
        /// MOD_NOREPEAT. While a key is registered the system consumes it, so
        /// the foreground player (PotPlayer) never sees it.
        ///
        /// Only a state transition touches the OS: calling this with the
        /// current state again is a no-op, so the overlay follow loop can call
        /// it every tick with the current foreground status. Every failed
        /// RegisterHotKey is returned (HotkeyRegisterError.action is None here,
        /// spec is "F7".."F10"); the key stays unregistered, is reported by
        /// fixed_registered() as not held, and is retried on the next
        /// activation transition. The overlay's GetAsyncKeyState polling keeps
        /// working as the fallback for exactly the unregistered keys.
        ///
        /// Thread constraint (same as apply/poll_events): RegisterHotKey(NULL)
        /// posts WM_HOTKEY to the calling thread's message queue, so the
        /// registration and the PeekMessageW drain must happen on the same
        /// thread (the overlay follow thread).
        pub fn set_fixed_active(&mut self, active: bool) -> Vec<HotkeyRegisterError> {
            let steps = self.fixed.transition(active);
            if steps.is_empty() {
                return Vec::new();
            }
            self.owner_thread = Some(std::thread::current().id());
            if active {
                ensure_message_queue();
            }
            let mut errors = Vec::new();
            for step in steps {
                match step {
                    FixedHotkeyStep::Register(action) => {
                        let registered = unsafe {
                            RegisterHotKey(
                                std::ptr::null_mut(),
                                fixed_hotkey_id(action),
                                MOD_NOREPEAT,
                                action.vk(),
                            )
                        } != 0;
                        self.fixed.record(action, registered);
                        if !registered {
                            errors.push(HotkeyRegisterError {
                                action: None,
                                spec: action.spec().to_owned(),
                                code: 0,
                                message: "RegisterHotKey 失败（固定键可能被其他程序占用或权限不足）"
                                    .into(),
                            });
                        }
                    }
                    FixedHotkeyStep::Unregister(action) => {
                        unsafe {
                            UnregisterHotKey(std::ptr::null_mut(), fixed_hotkey_id(action));
                        }
                    }
                }
            }
            if active {
                self.fixed_errors = errors.clone();
            } else {
                self.fixed_errors.clear();
            }
            errors
        }

        /// Whether one fixed overlay key is currently held through
        /// RegisterHotKey. False means the overlay must keep polling that key
        /// with GetAsyncKeyState (registration failed or is not active).
        pub fn fixed_registered(&self, action: FixedOverlayAction) -> bool {
            self.fixed.holds(action)
        }

        /// Number of fixed keys currently held through RegisterHotKey (0..=4).
        pub fn fixed_registered_count(&self) -> usize {
            self.fixed.held_count()
        }

        /// Last fixed-key registration failures; kept separate from errors() so
        /// the configurable-hotkey retry loop is not affected by them.
        pub fn fixed_errors(&self) -> &[HotkeyRegisterError] {
            &self.fixed_errors
        }

        pub fn registered_count(&self) -> usize {
            self.registered.len()
        }

        pub fn errors(&self) -> &[HotkeyRegisterError] {
            &self.errors
        }

        /// Unregisters every configurable binding registered by apply().
        fn clear_configurable(&mut self) {
            for id in self.registered.drain(..) {
                unsafe {
                    UnregisterHotKey(std::ptr::null_mut(), id);
                }
            }
        }

        /// Unregisters every hotkey this manager owns: the configurable
        /// bindings and the fixed F7-F10 keys. Idempotent.
        pub fn clear(&mut self) {
            self.clear_configurable();
            for action in FixedOverlayAction::ALL {
                if self.fixed.holds(action) {
                    unsafe {
                        UnregisterHotKey(std::ptr::null_mut(), fixed_hotkey_id(action));
                    }
                }
            }
            self.fixed.clear();
            self.fixed_errors.clear();
        }
    }

    impl Drop for GlobalHotkeys {
        fn drop(&mut self) {
            self.clear();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        #[ignore = "registers real system-wide hotkeys; run manually with a user session"]
        fn manual_register_poll_unregister_roundtrip() {
            let mut manager = GlobalHotkeys::new();
            let mut bindings = HotkeyBindings::default();
            bindings
                .set_spec(HotkeyAction::ToggleDanmaku, "Ctrl+Alt+Shift+F12")
                .unwrap();
            let errors = manager.apply(&bindings);
            assert!(errors.is_empty(), "{errors:?}");
            assert_eq!(manager.registered_count(), 10);
            let _ = manager.poll();
            manager.clear();
            assert_eq!(manager.registered_count(), 0);
        }

        #[test]
        #[ignore = "registers real system-wide hotkeys; run manually with a user session"]
        fn manual_fixed_register_poll_unregister_roundtrip() {
            let mut manager = GlobalHotkeys::new();
            // F7-F10 are free even when another instance already owns the
            // configurable combos, so a clean run reports no failures.
            let errors = manager.set_fixed_active(true);
            assert!(errors.is_empty(), "{errors:?}");
            for action in FixedOverlayAction::ALL {
                assert!(manager.fixed_registered(action), "{}", action.spec());
            }
            assert_eq!(manager.fixed_registered_count(), 4);
            // Same state again: no OS calls, no errors.
            assert!(manager.set_fixed_active(true).is_empty());
            assert_eq!(manager.fixed_registered_count(), 4);
            // A configurable re-apply (settings reload) keeps the fixed keys.
            let mut bindings = HotkeyBindings::default();
            bindings
                .set_spec(HotkeyAction::ToggleDanmaku, "Ctrl+Alt+Shift+F12")
                .unwrap();
            manager.apply(&bindings);
            assert_eq!(manager.fixed_registered_count(), 4);
            assert!(manager.poll_events().is_empty());
            // Deactivation releases all four.
            assert!(manager.set_fixed_active(false).is_empty());
            assert_eq!(manager.fixed_registered_count(), 0);
            assert!(
                FixedOverlayAction::ALL
                    .iter()
                    .all(|action| !manager.fixed_registered(*action))
            );
        }
    }
}

#[cfg(not(windows))]
mod stub_impl {
    use super::{
        FixedHotkeyState, FixedHotkeyStep, FixedOverlayAction, HotkeyAction, HotkeyBindings,
        HotkeyEvent, HotkeyRegisterError,
    };

    /// Non-Windows stub: keeps the crate cross-platform and the overlay code
    /// compiling; registration is a reported no-op.
    #[derive(Debug, Default)]
    pub struct GlobalHotkeys {
        errors: Vec<HotkeyRegisterError>,
        fixed: FixedHotkeyState,
        fixed_errors: Vec<HotkeyRegisterError>,
    }

    impl GlobalHotkeys {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn apply(&mut self, _bindings: &HotkeyBindings) -> Vec<HotkeyRegisterError> {
            self.errors = vec![HotkeyRegisterError {
                action: None,
                spec: String::new(),
                code: 0,
                message: "全局热键仅支持 Windows".into(),
            }];
            self.errors.clone()
        }

        /// Same transition contract as the Windows implementation, but every
        /// registration attempt is reported as unsupported. The transition is
        /// still tracked, so repeated calls with the same state stay silent.
        pub fn set_fixed_active(&mut self, active: bool) -> Vec<HotkeyRegisterError> {
            let steps = self.fixed.transition(active);
            let mut errors = Vec::new();
            for step in steps {
                if let FixedHotkeyStep::Register(action) = step {
                    self.fixed.record(action, false);
                    errors.push(HotkeyRegisterError {
                        action: None,
                        spec: action.spec().to_owned(),
                        code: 0,
                        message: "固定热键仅支持 Windows".into(),
                    });
                }
            }
            self.fixed_errors = errors.clone();
            errors
        }

        pub fn fixed_registered(&self, _action: FixedOverlayAction) -> bool {
            false
        }

        pub fn fixed_registered_count(&self) -> usize {
            0
        }

        pub fn fixed_errors(&self) -> &[HotkeyRegisterError] {
            &self.fixed_errors
        }

        pub fn poll_events(&mut self) -> Vec<HotkeyEvent> {
            Vec::new()
        }

        pub fn poll(&mut self) -> Vec<HotkeyAction> {
            Vec::new()
        }

        pub fn is_registered(&self, _action: HotkeyAction) -> bool {
            false
        }

        pub fn registered_count(&self) -> usize {
            0
        }

        pub fn errors(&self) -> &[HotkeyRegisterError] {
            &self.errors
        }

        pub fn clear(&mut self) {
            self.fixed.clear();
            self.fixed_errors.clear();
        }
    }
}

// ---- tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_default_binding() {
        let bindings = HotkeyBindings::default();
        let parsed = bindings.parse_all().expect("defaults must parse");
        assert_eq!(parsed.len(), 11);
        for binding in &parsed {
            assert!(!binding.spec.is_empty());
        }
        let toggle = parsed
            .iter()
            .find(|binding| binding.action == HotkeyAction::ToggleDanmaku)
            .unwrap();
        assert!(toggle.hotkey.has_ctrl() && toggle.hotkey.has_alt());
        assert_eq!(toggle.hotkey.vk, u32::from(b'D'));
        let up = parsed
            .iter()
            .find(|binding| binding.action == HotkeyAction::FontUp)
            .unwrap();
        assert_eq!(up.hotkey.vk, 0x26);
        let bracket = parsed
            .iter()
            .find(|binding| binding.action == HotkeyAction::DelayLess)
            .unwrap();
        assert_eq!(bracket.hotkey.vk, 0xDB);
    }

    #[test]
    fn parse_is_case_and_space_insensitive_and_order_independent() {
        let left = parse_spec("ctrl + alt + d").unwrap();
        let right = parse_spec("ALT+Control+D").unwrap();
        assert_eq!(left, right);
        assert_eq!(format_hotkey(left), "Ctrl+Alt+D");
    }

    #[test]
    fn formats_named_and_punctuation_keys_canonically() {
        assert_eq!(normalize_spec("ctrl+alt+pageup").unwrap(), "Ctrl+Alt+PgUp");
        assert_eq!(normalize_spec("shift+f10").unwrap(), "Shift+F10");
        assert_eq!(normalize_spec("ctrl+alt+[ ").unwrap(), "Ctrl+Alt+[");
        assert_eq!(normalize_spec("win+space").unwrap(), "Win+Space");
        assert_eq!(
            normalize_spec("Ctrl+Shift+Backspace").unwrap(),
            "Ctrl+Shift+Backspace"
        );
    }

    #[test]
    fn rejects_bad_specs_with_explicit_errors() {
        assert_eq!(parse_spec(""), Err(HotkeyParseError::Empty));
        assert_eq!(
            parse_spec("Ctrl+Alt+"),
            Err(HotkeyParseError::TrailingSeparator)
        );
        assert_eq!(
            parse_spec("Ctrl+Ctrl+D"),
            Err(HotkeyParseError::DuplicateModifier("Ctrl".into()))
        );
        assert_eq!(parse_spec("Ctrl+Alt"), Err(HotkeyParseError::MissingKey));
        assert!(matches!(
            parse_spec("Ctrl+Alt+Foo"),
            Err(HotkeyParseError::UnknownToken(_))
        ));
        assert!(matches!(
            parse_spec("Ctrl+A+B"),
            Err(HotkeyParseError::MultipleKeys(_))
        ));
        assert!(matches!(
            parse_spec("Ctrl+Alt+F25"),
            Err(HotkeyParseError::UnknownToken(_))
        ));
    }

    #[test]
    fn disabled_binding_is_skipped_and_empty_is_allowed() {
        let mut bindings = HotkeyBindings::default();
        bindings.set_spec(HotkeyAction::ToggleOverlay, "").unwrap();
        let parsed = bindings.parse_all().unwrap();
        assert_eq!(parsed.len(), 10);
        assert!(
            parsed
                .iter()
                .all(|binding| binding.action != HotkeyAction::ToggleOverlay)
        );
    }

    #[test]
    fn duplicate_combos_are_rejected() {
        let mut bindings = HotkeyBindings::default();
        bindings
            .set_spec(HotkeyAction::ToggleOverlay, "Ctrl+Alt+D")
            .unwrap();
        let error = bindings.parse_all().unwrap_err();
        assert!(matches!(
            error,
            HotkeyConfigError::Duplicate {
                first: HotkeyAction::ToggleDanmaku,
                second: HotkeyAction::ToggleOverlay,
                ..
            }
        ));
    }

    #[test]
    fn bare_f7_f10_are_reserved_for_fixed_overlay_keys() {
        let mut bindings = HotkeyBindings::default();
        let error = bindings
            .set_spec(HotkeyAction::ToggleDanmaku, "F9")
            .unwrap_err();
        assert!(matches!(error, HotkeyConfigError::Reserved { .. }));
        // With a modifier the same key is fine (fixed keys are registered bare).
        bindings
            .set_spec(HotkeyAction::ToggleDanmaku, "Ctrl+F9")
            .unwrap();
        assert_eq!(bindings.reserved_conflicts().len(), 0);
    }

    #[test]
    fn action_ids_are_stable_and_unique() {
        for (index, action) in HotkeyAction::ALL.iter().enumerate() {
            assert_eq!(action.index(), index);
            assert_eq!(
                action_for_hotkey_id(HOTKEY_ID_BASE + index as i32),
                Some(*action)
            );
        }
        assert_eq!(action_for_hotkey_id(HOTKEY_ID_BASE - 1), None);
        assert_eq!(action_for_hotkey_id(HOTKEY_ID_BASE + 11), None);
    }

    #[test]
    fn fixed_overlay_keys_map_to_display_actions() {
        assert_eq!(
            fixed_overlay_action(VK_F7),
            Some(FixedOverlayAction::FontSmaller)
        );
        assert_eq!(
            fixed_overlay_action(VK_F8),
            Some(FixedOverlayAction::FontLarger)
        );
        assert_eq!(
            fixed_overlay_action(VK_F9),
            Some(FixedOverlayAction::ToggleDanmaku)
        );
        assert_eq!(
            fixed_overlay_action(VK_F10),
            Some(FixedOverlayAction::ToggleSuperChat)
        );
        assert_eq!(fixed_overlay_action(0x41), None);
    }

    #[test]
    fn register_modifiers_always_disable_auto_repeat() {
        let hotkey = parse_spec("Ctrl+Alt+D").unwrap();
        assert_eq!(
            hotkey.register_modifiers(),
            MOD_CONTROL | MOD_ALT | MOD_NOREPEAT
        );
    }

    #[test]
    fn fixed_hotkeys_are_classified_by_their_own_id_range() {
        for action in FixedOverlayAction::ALL {
            assert_eq!(
                fixed_hotkey_id(action),
                FIXED_HOTKEY_ID_BASE + action.index() as i32
            );
            // The configurable resolver must not claim the fixed range.
            assert_eq!(action_for_hotkey_id(fixed_hotkey_id(action)), None);
            assert_eq!(
                event_for_hotkey_id(fixed_hotkey_id(action)),
                Some(HotkeyEvent::Fixed(action))
            );
        }
        assert_eq!(
            event_for_hotkey_id(HOTKEY_ID_BASE),
            Some(HotkeyEvent::Action(HotkeyAction::ToggleDanmaku))
        );
        assert_eq!(event_for_hotkey_id(FIXED_HOTKEY_ID_BASE + 4), None);
        assert_eq!(event_for_hotkey_id(i32::MIN), None);
    }

    #[test]
    fn fixed_hotkeys_activate_idempotently_and_clear_resets_state() {
        let mut state = FixedHotkeyState::new();
        assert_eq!(state.transition(true).len(), 4);
        for action in FixedOverlayAction::ALL {
            assert!(!state.holds(action));
            state.record(action, true);
        }
        assert_eq!(state.held_count(), 4);
        assert!(state.transition(true).is_empty());
        assert_eq!(state.transition(false).len(), 4);
        assert_eq!(state.held_count(), 0);
        // clear() must forget registrations even without a deactivation first.
        let _ = state.transition(true);
        state.record(FixedOverlayAction::FontSmaller, true);
        state.clear();
        assert!(!state.is_active());
        assert_eq!(state.held_count(), 0);
    }
}
