//! Contract tests for the global-hotkey grammar/validation/registration module.
//! Offline only: nothing is registered with the OS in these tests. The real
//! RegisterHotKey round trip lives in the #[ignore] unit tests in src/hotkeys.rs.
//!
//! The fixed F7-F10 foreground keys add a second registration range; its id
//! mapping, event classification and activation state machine are exercised
//! below without touching the OS.

use live_hub_rust::core::HotkeyBindings;
use live_hub_rust::hotkeys::{
    FIXED_HOTKEY_ID_BASE, FixedHotkeyState, FixedHotkeyStep, FixedOverlayAction, HOTKEY_ID_BASE,
    HotkeyAction, HotkeyConfigError, HotkeyEvent, HotkeyParseError, ParsedHotkey,
    action_for_hotkey_id, event_for_hotkey_id, fixed_action_for_hotkey_id, fixed_hotkey_id,
    fixed_overlay_action, format_hotkey, normalize_spec, parse_spec,
};

#[cfg(not(windows))]
use live_hub_rust::hotkeys::GlobalHotkeys;

fn defaults() -> HotkeyBindings {
    HotkeyBindings::default()
}

#[test]
fn eleven_default_bindings_parse_and_round_trip() {
    let bindings = defaults();
    let parsed = bindings.parse_all().expect("defaults parse");
    assert_eq!(parsed.len(), 11);
    for binding in &parsed {
        assert_eq!(format_hotkey(binding.hotkey), normalize_spec(&binding.spec).unwrap());
    }
    let specs: Vec<&str> = parsed.iter().map(|binding| binding.spec.as_str()).collect();
    for expected in [
        "Ctrl+Alt+D",
        "Ctrl+Alt+S",
        "Ctrl+Alt+O",
        "Ctrl+Alt+Up",
        "Ctrl+Alt+Down",
        "Ctrl+Alt+Right",
        "Ctrl+Alt+Left",
        "Ctrl+Alt+PgUp",
        "Ctrl+Alt+PgDn",
        "Ctrl+Alt+[",
        "Ctrl+Alt+]",
    ] {
        assert!(specs.contains(&expected), "missing default {expected}");
    }
}

#[test]
fn parser_accepts_aliases_and_normalises() {
    let parsed = parse_spec("ALT+Control+PageDown").unwrap();
    assert_eq!(format_hotkey(parsed), "Ctrl+Alt+PgDn");
    assert_eq!(normalize_spec("win+f7").unwrap(), "Win+F7");
    assert_eq!(normalize_spec("shift+space").unwrap(), "Shift+Space");
    assert_eq!(normalize_spec("ctrl+alt+escape").unwrap(), "Ctrl+Alt+Escape");
}

#[test]
fn parser_errors_are_typed() {
    assert_eq!(parse_spec("  "), Err(HotkeyParseError::Empty));
    assert_eq!(parse_spec("Ctrl++D"), Err(HotkeyParseError::TrailingSeparator));
    assert_eq!(
        parse_spec("Shift+Shift+A"),
        Err(HotkeyParseError::DuplicateModifier("Shift".into()))
    );
    assert_eq!(parse_spec("Ctrl+Alt"), Err(HotkeyParseError::MissingKey));
    assert!(matches!(
        parse_spec("Ctrl+Unknown"),
        Err(HotkeyParseError::UnknownToken(_))
    ));
    assert!(matches!(
        parse_spec("Ctrl+A+B"),
        Err(HotkeyParseError::MultipleKeys(_))
    ));
}

#[test]
fn empty_binding_is_disabled() {
    let mut bindings = defaults();
    bindings.set_spec(HotkeyAction::CycleSc, "").unwrap();
    let parsed = bindings.parse_all().unwrap();
    assert_eq!(parsed.len(), 10);
    assert!(!parsed.iter().any(|binding| binding.action == HotkeyAction::CycleSc));
    assert_eq!(bindings.spec(HotkeyAction::CycleSc), "");
}

#[test]
fn duplicate_and_reserved_bindings_are_rejected() {
    let mut bindings = defaults();
    bindings.set_spec(HotkeyAction::ToggleOverlay, "ctrl+alt+d").unwrap();
    assert!(matches!(
        bindings.parse_all(),
        Err(HotkeyConfigError::Duplicate { .. })
    ));

    let mut bindings = defaults();
    assert!(matches!(
        bindings.set_spec(HotkeyAction::FontUp, "F8"),
        Err(HotkeyConfigError::Reserved { .. })
    ));
    bindings.set_spec(HotkeyAction::FontUp, "Ctrl+F8").unwrap();
    assert_eq!(bindings.reserved_conflicts().len(), 0);

    // A hand-edited config file bypasses set_spec; parse_all still rejects it.
    let mut bindings = defaults();
    bindings.font_down = "F9".to_owned();
    assert!(matches!(
        bindings.parse_all(),
        Err(HotkeyConfigError::Reserved { .. })
    ));
}

#[test]
fn action_ids_and_fixed_keys_are_stable() {
    for action in HotkeyAction::ALL {
        let id = live_hub_rust::hotkeys::HOTKEY_ID_BASE + action.index() as i32;
        assert_eq!(action_for_hotkey_id(id), Some(action));
        assert_eq!(event_for_hotkey_id(id), Some(HotkeyEvent::Action(action)));
    }
    assert!(fixed_overlay_action(0x76).is_some());
    assert!(fixed_overlay_action(0x7A).is_none()); // F11 is not a fixed overlay key
    assert_eq!(ParsedHotkey { modifiers: 0, vk: 0x76 }.has_ctrl(), false);
}

#[test]
fn fixed_hotkey_ids_use_a_separate_contiguous_range() {
    assert_eq!(FIXED_HOTKEY_ID_BASE, 0x4D00);
    assert_eq!(FixedOverlayAction::ALL.len(), 4);
    for (index, action) in FixedOverlayAction::ALL.iter().enumerate() {
        assert_eq!(action.index(), index);
        assert_eq!(fixed_hotkey_id(*action), FIXED_HOTKEY_ID_BASE + index as i32);
        assert_eq!(
            fixed_action_for_hotkey_id(fixed_hotkey_id(*action)),
            Some(*action)
        );
    }
    // The ranges must not overlap: the WM_HOTKEY id alone decides the event kind.
    let last_action_id = HOTKEY_ID_BASE + (HotkeyAction::ALL.len() - 1) as i32;
    assert!(last_action_id < FIXED_HOTKEY_ID_BASE);
    assert_eq!(fixed_action_for_hotkey_id(last_action_id), None);
    assert_eq!(fixed_action_for_hotkey_id(HOTKEY_ID_BASE), None);
    assert_eq!(fixed_action_for_hotkey_id(FIXED_HOTKEY_ID_BASE - 1), None);
    assert_eq!(fixed_action_for_hotkey_id(FIXED_HOTKEY_ID_BASE + 4), None);
    // Both ranges stay inside the documented application range (< 0xBFFF).
    assert!(FIXED_HOTKEY_ID_BASE + 4 < 0xBFFF);
}

#[test]
fn hotkey_events_classify_both_ranges_and_ignore_unknown_ids() {
    for action in FixedOverlayAction::ALL {
        assert_eq!(
            event_for_hotkey_id(fixed_hotkey_id(action)),
            Some(HotkeyEvent::Fixed(action))
        );
        // action_for_hotkey_id keeps its configurable-only contract.
        assert_eq!(action_for_hotkey_id(fixed_hotkey_id(action)), None);
    }
    for id in [
        0,
        HOTKEY_ID_BASE - 1,
        FIXED_HOTKEY_ID_BASE - 1,
        FIXED_HOTKEY_ID_BASE + 4,
        i32::MIN,
        i32::MAX,
    ] {
        assert_eq!(event_for_hotkey_id(id), None, "id {id:#X} must not classify");
    }
    assert_eq!(
        HotkeyEvent::Action(HotkeyAction::CycleSc).action(),
        Some(HotkeyAction::CycleSc)
    );
    assert_eq!(HotkeyEvent::Action(HotkeyAction::CycleSc).fixed(), None);
    assert_eq!(
        HotkeyEvent::Fixed(FixedOverlayAction::ToggleDanmaku).action(),
        None
    );
    assert_eq!(
        HotkeyEvent::Fixed(FixedOverlayAction::ToggleDanmaku).fixed(),
        Some(FixedOverlayAction::ToggleDanmaku)
    );
}

#[test]
fn hotkey_event_boundaries_match_the_documented_ranges() {
    assert_eq!(
        event_for_hotkey_id(HOTKEY_ID_BASE),
        Some(HotkeyEvent::Action(HotkeyAction::ToggleDanmaku))
    );
    assert_eq!(
        event_for_hotkey_id(HOTKEY_ID_BASE + 10),
        Some(HotkeyEvent::Action(HotkeyAction::DelayMore))
    );
    assert_eq!(event_for_hotkey_id(HOTKEY_ID_BASE + 11), None);
    assert_eq!(
        event_for_hotkey_id(FIXED_HOTKEY_ID_BASE),
        Some(HotkeyEvent::Fixed(FixedOverlayAction::FontSmaller))
    );
    assert_eq!(
        event_for_hotkey_id(FIXED_HOTKEY_ID_BASE + 3),
        Some(HotkeyEvent::Fixed(FixedOverlayAction::ToggleSuperChat))
    );
    assert_eq!(event_for_hotkey_id(FIXED_HOTKEY_ID_BASE + 4), None);
}

#[test]
fn fixed_activation_is_idempotent() {
    let mut state = FixedHotkeyState::new();
    assert!(!state.is_active());
    assert_eq!(state.held_count(), 0);

    let registering = state.transition(true);
    assert!(state.is_active());
    assert_eq!(
        registering,
        FixedOverlayAction::ALL.map(FixedHotkeyStep::Register).to_vec()
    );
    for action in FixedOverlayAction::ALL {
        assert!(!state.holds(action), "transition alone must not claim ownership");
        state.record(action, true);
    }
    assert_eq!(state.held_count(), 4);

    // Same state again: no OS calls, keys stay held.
    assert!(state.transition(true).is_empty());
    assert_eq!(state.held_count(), 4);

    let unregistering = state.transition(false);
    assert!(!state.is_active());
    assert_eq!(
        unregistering,
        FixedOverlayAction::ALL.map(FixedHotkeyStep::Unregister).to_vec()
    );
    assert_eq!(state.held_count(), 0);
    assert!(FixedOverlayAction::ALL.iter().all(|action| !state.holds(*action)));

    // Same state again: still no OS calls.
    assert!(state.transition(false).is_empty());
}

#[test]
fn failed_fixed_registration_is_not_retried_within_one_activation() {
    let mut state = FixedHotkeyState::new();
    assert_eq!(state.transition(true).len(), 4);
    state.record(FixedOverlayAction::FontSmaller, true);
    state.record(FixedOverlayAction::FontLarger, true);
    state.record(FixedOverlayAction::ToggleDanmaku, false);
    state.record(FixedOverlayAction::ToggleSuperChat, true);

    // The overlay calls set_fixed_active every follow tick; a key whose
    // registration failed must not trigger a new RegisterHotKey attempt each
    // tick (the overlay polls exactly that key with GetAsyncKeyState instead).
    assert!(state.transition(true).is_empty());
    assert!(!state.holds(FixedOverlayAction::ToggleDanmaku));
    assert_eq!(state.held_count(), 3);

    // A foreground round trip (leave and return) is the retry point.
    let unregistering = state.transition(false);
    assert_eq!(
        unregistering,
        vec![
            FixedHotkeyStep::Unregister(FixedOverlayAction::FontSmaller),
            FixedHotkeyStep::Unregister(FixedOverlayAction::FontLarger),
            FixedHotkeyStep::Unregister(FixedOverlayAction::ToggleSuperChat),
        ]
    );
    assert_eq!(
        state.transition(true),
        FixedOverlayAction::ALL.map(FixedHotkeyStep::Register).to_vec()
    );
}

#[test]
fn clear_forgets_fixed_registrations() {
    let mut state = FixedHotkeyState::new();
    assert_eq!(state.transition(true).len(), 4);
    for action in FixedOverlayAction::ALL {
        state.record(action, true);
    }
    assert!(state.is_active());
    state.clear();
    assert!(!state.is_active());
    assert_eq!(state.held_count(), 0);
    assert!(FixedOverlayAction::ALL.iter().all(|action| !state.holds(*action)));
    // Reactivating after clear registers all four keys again.
    assert_eq!(state.transition(true).len(), 4);
}

#[test]
fn fixed_keys_map_to_stable_vk_and_spec_names() {
    assert_eq!(
        FixedOverlayAction::ALL.map(FixedOverlayAction::spec),
        ["F7", "F8", "F9", "F10"]
    );
    assert_eq!(
        FixedOverlayAction::ALL.map(FixedOverlayAction::vk),
        [0x76, 0x77, 0x78, 0x79]
    );
    for action in FixedOverlayAction::ALL {
        assert_eq!(fixed_overlay_action(action.vk()), Some(action));
        assert_eq!(
            parse_spec(action.spec()).unwrap(),
            ParsedHotkey {
                modifiers: 0,
                vk: action.vk()
            }
        );
        assert!(!action.label_zh().is_empty());
    }
}

#[cfg(not(windows))]
#[test]
fn non_windows_stub_reports_fixed_keys_unsupported_once() {
    let mut manager = GlobalHotkeys::new();
    let errors = manager.set_fixed_active(true);
    assert_eq!(errors.len(), 4);
    let specs: Vec<&str> = errors.iter().map(|error| error.spec.as_str()).collect();
    assert_eq!(specs, ["F7", "F8", "F9", "F10"]);
    assert!(errors.iter().all(|error| error.action.is_none()));
    assert_eq!(manager.fixed_errors().len(), 4);
    // Idempotent: the same state reports nothing further.
    assert!(manager.set_fixed_active(true).is_empty());
    assert_eq!(manager.fixed_errors().len(), 4);
    assert!(!manager.fixed_registered(FixedOverlayAction::FontSmaller));
    assert_eq!(manager.fixed_registered_count(), 0);
    assert!(manager.poll_events().is_empty());
    manager.set_fixed_active(false);
    assert!(manager.fixed_errors().is_empty());
    manager.clear();
    assert!(manager.fixed_errors().is_empty());
}
