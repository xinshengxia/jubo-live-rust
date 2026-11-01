//! Pure overlay window visibility / z-order policy.
//!
//! The legacy C# overlay (danmaku-4-overlay.cs) decides every frame whether to
//! show its layered window and how it is stacked:
//!   * default (AlwaysOnTop=false): the overlay is glued directly above the
//!     player window and stays visible even when another window has focus -
//!     whatever covers the player naturally covers the overlay too;
//!   * AlwaysOnTop=true: the overlay is HWND_TOPMOST and HideWhenNotForeground
//!     decides whether it disappears while the player is not the foreground
//!     window (the overlay itself / its tray menu counts as us);
//!   * minimized or hidden player, missing/valid-less client area, or a dead
//!     player process always hide it.
//!
//! This module is deliberately free of Windows/egui types so the truth table is
//! testable offline. The overlay follow thread feeds it plain values, then maps
//! the returned ZOrderAction onto SetWindowPos.

use crate::core::OverlaySettings;

/// A client area smaller than this cannot host the overlay (legacy used 16).
pub const MIN_CLIENT_SIDE: i32 = 16;
/// Upper bound for a sane client area; protects layout math from absurd values.
pub const MAX_CLIENT_SIDE: i32 = 16384;
/// How long a missing player window may stay missing before the overlay exits
/// (legacy PlayerFollowPolicy.WindowGraceMs = 30000).
pub const WINDOW_MISSING_GRACE_MS: u64 = 30_000;

/// Persisted window-policy switches (legacy AlwaysOnTop /
/// HideWhenNotForeground). Both live in crate::core::OverlaySettings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowPolicy {
    pub always_on_top: bool,
    pub hide_when_not_foreground: bool,
}

impl Default for WindowPolicy {
    fn default() -> Self {
        // Legacy defaults: AlwaysOnTop=false, HideWhenNotForeground=true.
        Self {
            always_on_top: false,
            hide_when_not_foreground: true,
        }
    }
}

impl From<&OverlaySettings> for WindowPolicy {
    fn from(settings: &OverlaySettings) -> Self {
        Self {
            always_on_top: settings.always_on_top,
            hide_when_not_foreground: settings.hide_when_not_foreground,
        }
    }
}

/// A player client rectangle in virtual-screen pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientArea {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

impl ClientArea {
    pub const fn new(left: i32, top: i32, width: i32, height: i32) -> Self {
        Self {
            left,
            top,
            width,
            height,
        }
    }

    pub const fn is_usable(self) -> bool {
        self.width >= MIN_CLIENT_SIDE && self.height >= MIN_CLIENT_SIDE
    }

    /// Returns a layout-safe area, or None when the values are unusable
    /// (negative/too small/too large/overflowing i32 when offset by origin).
    pub fn sanitized(self) -> Option<Self> {
        if !self.is_usable() || self.width > MAX_CLIENT_SIDE || self.height > MAX_CLIENT_SIDE {
            return None;
        }
        if self.left.checked_add(self.width).is_none() || self.top.checked_add(self.height).is_none()
        {
            return None;
        }
        Some(self)
    }

    pub const fn origin(self) -> (i32, i32) {
        (self.left, self.top)
    }

    pub const fn size(self) -> (i32, i32) {
        (self.width, self.height)
    }
}

/// Snapshot of the followed player window for one policy tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayerWindowState {
    pub process_alive: bool,
    pub window_exists: bool,
    pub window_visible: bool,
    pub minimized: bool,
    /// Player window (or one of its owned popups) is the foreground window.
    pub foreground_player: bool,
    /// Our overlay window / tray menu is the foreground window.
    pub foreground_overlay: bool,
}

impl Default for PlayerWindowState {
    fn default() -> Self {
        Self {
            process_alive: true,
            window_exists: true,
            window_visible: true,
            minimized: false,
            foreground_player: false,
            foreground_overlay: false,
        }
    }
}

impl PlayerWindowState {
    /// True when either the player or our own overlay owns the foreground.
    pub const fn foreground_ours_or_player(self) -> bool {
        self.foreground_player || self.foreground_overlay
    }
}

/// How the overlay window should be stacked this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZOrderAction {
    /// No window operation needed / window hidden.
    None,
    /// Insert directly above the player window (default legacy mode).
    AbovePlayer,
    /// HWND_TOPMOST.
    Topmost,
}

/// Why the overlay is hidden this tick (for diagnostics / logs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HideReason {
    ProcessExited,
    WindowMissing,
    Minimized,
    InvalidClientArea,
    NotForeground,
}

/// One frame decision: paint and place the overlay iff visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayWindowDecision {
    pub visible: bool,
    pub topmost: bool,
    pub z_order: ZOrderAction,
    pub hide_reason: Option<HideReason>,
    pub area: Option<ClientArea>,
}

impl OverlayWindowDecision {
    fn hidden(reason: HideReason) -> Self {
        Self {
            visible: false,
            topmost: false,
            z_order: ZOrderAction::None,
            hide_reason: Some(reason),
            area: None,
        }
    }

    fn shown(area: ClientArea, topmost: bool) -> Self {
        Self {
            visible: true,
            topmost,
            z_order: if topmost {
                ZOrderAction::Topmost
            } else {
                ZOrderAction::AbovePlayer
            },
            hide_reason: None,
            area: Some(area),
        }
    }
}

/// Decides visibility and stacking for one tick.
///
/// Legacy semantics (danmaku-4-overlay.cs):
/// 1. dead player process / missing or hidden window / minimized -> hidden;
/// 2. invalid client area -> hidden;
/// 3. AlwaysOnTop=false -> visible, glue above player, foreground irrelevant;
/// 4. AlwaysOnTop=true -> topmost; hide only when HideWhenNotForeground is on
///    and neither the player nor our own window is foreground.
pub fn decide(
    policy: WindowPolicy,
    player: &PlayerWindowState,
    area: Option<ClientArea>,
) -> OverlayWindowDecision {
    if !player.process_alive {
        return OverlayWindowDecision::hidden(HideReason::ProcessExited);
    }
    if !player.window_exists || !player.window_visible {
        return OverlayWindowDecision::hidden(HideReason::WindowMissing);
    }
    if player.minimized {
        return OverlayWindowDecision::hidden(HideReason::Minimized);
    }
    let Some(area) = area.and_then(ClientArea::sanitized) else {
        return OverlayWindowDecision::hidden(HideReason::InvalidClientArea);
    };
    if policy.always_on_top {
        if policy.hide_when_not_foreground && !player.foreground_ours_or_player() {
            return OverlayWindowDecision::hidden(HideReason::NotForeground);
        }
        return OverlayWindowDecision::shown(area, true);
    }
    OverlayWindowDecision::shown(area, false)
}

/// Legacy follow-exit rule: exit after the player process died, or after its
/// window has been missing longer than the grace period. A failed process query
/// (None) is not evidence and must not trigger an exit.
pub fn should_exit_after_missing_window(process_alive: Option<bool>, missing_ms: u64) -> bool {
    process_alive == Some(false) || missing_ms >= WINDOW_MISSING_GRACE_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alive() -> PlayerWindowState {
        PlayerWindowState::default()
    }

    fn area() -> Option<ClientArea> {
        Some(ClientArea::new(100, 50, 1280, 720))
    }

    #[test]
    fn defaults_match_legacy() {
        let policy = WindowPolicy::default();
        assert!(!policy.always_on_top);
        assert!(policy.hide_when_not_foreground);
        let settings = OverlaySettings::default();
        assert_eq!(WindowPolicy::from(&settings), policy);
    }

    #[test]
    fn default_mode_stays_visible_when_player_is_not_foreground() {
        let mut player = alive();
        player.foreground_player = false;
        let decision = decide(WindowPolicy::default(), &player, area());
        assert!(decision.visible, "default mode must not require foreground");
        assert!(!decision.topmost);
        assert_eq!(decision.z_order, ZOrderAction::AbovePlayer);
        assert!(decision.hide_reason.is_none());
        assert_eq!(decision.area, area());
    }

    #[test]
    fn topmost_mode_hides_when_not_foreground_only_with_the_flag() {
        let mut player = alive();
        player.foreground_player = false;
        let hide = WindowPolicy {
            always_on_top: true,
            hide_when_not_foreground: true,
        };
        let decision = decide(hide, &player, area());
        assert!(!decision.visible);
        assert_eq!(decision.hide_reason, Some(HideReason::NotForeground));

        let keep = WindowPolicy {
            always_on_top: true,
            hide_when_not_foreground: false,
        };
        let decision = decide(keep, &player, area());
        assert!(decision.visible);
        assert!(decision.topmost);
        assert_eq!(decision.z_order, ZOrderAction::Topmost);
    }

    #[test]
    fn our_own_overlay_window_counts_as_foreground() {
        let mut player = alive();
        player.foreground_overlay = true;
        let policy = WindowPolicy {
            always_on_top: true,
            hide_when_not_foreground: true,
        };
        assert!(decide(policy, &player, area()).visible);
    }

    #[test]
    fn minimized_missing_or_dead_player_hides_overlay() {
        let policy = WindowPolicy::default();
        let mut player = alive();
        player.minimized = true;
        assert_eq!(
            decide(policy, &player, area()).hide_reason,
            Some(HideReason::Minimized)
        );

        let mut player = alive();
        player.window_exists = false;
        assert_eq!(
            decide(policy, &player, area()).hide_reason,
            Some(HideReason::WindowMissing)
        );

        let mut player = alive();
        player.window_visible = false;
        assert_eq!(
            decide(policy, &player, area()).hide_reason,
            Some(HideReason::WindowMissing)
        );

        let mut player = alive();
        player.process_alive = false;
        assert_eq!(
            decide(policy, &player, area()).hide_reason,
            Some(HideReason::ProcessExited)
        );
    }

    #[test]
    fn invalid_client_areas_are_rejected() {
        let policy = WindowPolicy::default();
        let player = alive();
        for bad in [
            ClientArea::new(0, 0, 0, 720),
            ClientArea::new(0, 0, 1280, 0),
            ClientArea::new(0, 0, 15, 720),
            ClientArea::new(0, 0, 1280, 15),
            ClientArea::new(0, 0, MAX_CLIENT_SIDE + 1, 720),
            ClientArea::new(i32::MAX, 0, 1280, 720),
        ] {
            assert_eq!(
                decide(policy, &player, Some(bad)).hide_reason,
                Some(HideReason::InvalidClientArea),
                "{bad:?}"
            );
        }
        assert_eq!(
            decide(policy, &player, None).hide_reason,
            Some(HideReason::InvalidClientArea)
        );
    }

    #[test]
    fn negative_virtual_screen_origin_is_supported() {
        let area = Some(ClientArea::new(-3000, -200, 1600, 900));
        let decision = decide(WindowPolicy::default(), &alive(), area);
        assert!(decision.visible);
        assert_eq!(decision.area, area);
    }

    #[test]
    fn follow_exit_respects_grace_and_unknown_process_state() {
        assert!(should_exit_after_missing_window(Some(false), 0));
        assert!(!should_exit_after_missing_window(Some(true), 0));
        assert!(!should_exit_after_missing_window(None, 0));
        assert!(!should_exit_after_missing_window(Some(true), WINDOW_MISSING_GRACE_MS - 1));
        assert!(should_exit_after_missing_window(Some(true), WINDOW_MISSING_GRACE_MS));
    }
}
