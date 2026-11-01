//! Contract tests for the pure overlay window-policy module.

use live_hub_rust::core::OverlaySettings;
use live_hub_rust::window_policy::{
    ClientArea, HideReason, OverlayWindowDecision, PlayerWindowState, WINDOW_MISSING_GRACE_MS,
    WindowPolicy, ZOrderAction, decide, should_exit_after_missing_window,
};

fn policy_default() -> WindowPolicy {
    WindowPolicy::default()
}

fn player() -> PlayerWindowState {
    PlayerWindowState::default()
}

fn area() -> Option<ClientArea> {
    Some(ClientArea::new(0, 0, 1240, 720))
}

#[test]
fn settings_map_to_policy_with_legacy_defaults() {
    let settings = OverlaySettings::default();
    let policy = WindowPolicy::from(&settings);
    assert!(!policy.always_on_top);
    assert!(policy.hide_when_not_foreground);

    let customized = OverlaySettings {
        always_on_top: true,
        hide_when_not_foreground: false,
        ..Default::default()
    };
    let policy = WindowPolicy::from(&customized);
    assert!(policy.always_on_top);
    assert!(!policy.hide_when_not_foreground);
}

#[test]
fn default_mode_visibility_does_not_depend_on_foreground() {
    let mut state = player();
    state.foreground_player = false;
    state.foreground_overlay = false;
    let decision = decide(policy_default(), &state, area());
    assert!(decision.visible);
    assert_eq!(decision.z_order, ZOrderAction::AbovePlayer);
    assert_eq!(decision.hide_reason, None);
    assert_eq!(decision.area, area());
}

#[test]
fn topmost_mode_obeys_hide_when_not_foreground() {
    let mut state = player();
    state.foreground_player = false;

    let hidden = decide(
        WindowPolicy {
            always_on_top: true,
            hide_when_not_foreground: true,
        },
        &state,
        area(),
    );
    assert!(!hidden.visible);
    assert_eq!(hidden.hide_reason, Some(HideReason::NotForeground));
    assert_eq!(hidden.z_order, ZOrderAction::None);

    let visible = decide(
        WindowPolicy {
            always_on_top: true,
            hide_when_not_foreground: false,
        },
        &state,
        area(),
    );
    assert!(visible.visible);
    assert!(visible.topmost);
    assert_eq!(visible.z_order, ZOrderAction::Topmost);

    state.foreground_overlay = true;
    let ours = decide(
        WindowPolicy {
            always_on_top: true,
            hide_when_not_foreground: true,
        },
        &state,
        area(),
    );
    assert!(ours.visible, "clicking our own overlay/tray menu must keep it visible");
}

#[test]
fn hidden_reasons_cover_player_lifecycle() {
    let policy = policy_default();

    let mut state = player();
    state.minimized = true;
    assert_eq!(
        decide(policy, &state, area()).hide_reason,
        Some(HideReason::Minimized)
    );

    let mut state = player();
    state.window_exists = false;
    assert_eq!(
        decide(policy, &state, area()).hide_reason,
        Some(HideReason::WindowMissing)
    );

    let mut state = player();
    state.process_alive = false;
    assert_eq!(
        decide(policy, &state, area()).hide_reason,
        Some(HideReason::ProcessExited)
    );

    let decision: OverlayWindowDecision = decide(policy, &player(), None);
    assert_eq!(decision.hide_reason, Some(HideReason::InvalidClientArea));
}

#[test]
fn invalid_areas_and_off_screen_origins() {
    let policy = policy_default();
    let state = player();
    assert!(decide(policy, &state, Some(ClientArea::new(0, 0, 10, 10))).hide_reason.is_some());
    assert!(decide(policy, &state, Some(ClientArea::new(0, 0, 20000, 720))).hide_reason.is_some());

    let off_screen = Some(ClientArea::new(-4000, -300, 1600, 900));
    assert!(decide(policy, &state, off_screen).visible);
}

#[test]
fn exit_grace_matches_legacy_window_policy() {
    // C# PlayerFollowPolicy: exit when the process is confirmed dead, or when the
    // player window has been missing for the full grace period. A failed process
    // query (None) is not by itself an exit signal, but it does not suppress the
    // missing-window timeout either.
    assert!(should_exit_after_missing_window(Some(false), 0));
    assert!(!should_exit_after_missing_window(None, 0));
    assert!(!should_exit_after_missing_window(None, WINDOW_MISSING_GRACE_MS - 1));
    assert!(should_exit_after_missing_window(None, WINDOW_MISSING_GRACE_MS));
    assert!(!should_exit_after_missing_window(Some(true), WINDOW_MISSING_GRACE_MS - 1));
    assert!(should_exit_after_missing_window(Some(true), WINDOW_MISSING_GRACE_MS));
}
