//! Real-window overlay acceptance. The default test below runs offline and only
//! asserts that the diagnostic switches are opt-in; the real-window harness is
//! #[ignore]d because it creates an off-screen synthetic player window and starts
//! the overlay process (never stealing foreground focus).
//!
//!   LIVE_HUB_ACCEPTANCE_OVERLAY_SCRIPT  path to scripts/Acceptance-Overlay.ps1
//!   LIVE_HUB_ACCEPTANCE_OVERLAY_EXE     path to the built live-hub-rust.exe
//!   LIVE_HUB_ACCEPTANCE_OVERLAY_EVIDENCE directory for JSON/PNG evidence

#[cfg(feature = "gui")]
#[test]
fn diagnostic_overlay_switches_are_opt_in_and_offline() {
    use live_hub_rust::danmaku::DanmakuEvent;
    use live_hub_rust::overlay;
    assert!(!overlay::diag_force_show_from(|_| None));
    for value in ["0", "", "false", "true", "yes"] {
        assert!(!overlay::diag_force_show_from(|_| Some(value.to_string())), "{value}");
    }
    assert!(overlay::diag_force_show_from(|_| Some("1".to_string())));
    let plan = overlay::parse_fixture(r#"{"events":[{"Status":"合成状态"}]}"#).expect("fixture parses");
    assert_eq!(plan.events.len(), 1);
    assert!(matches!(plan.events[0], DanmakuEvent::Status(_)));
    assert!(overlay::parse_fixture("not json").is_err());
    assert!(overlay::parse_fixture(r#"{"events":[]}"#).is_err());
}

#[cfg(windows)]
#[test]
#[ignore = "real-window overlay harness: off-screen synthetic player window, no focus change, ~30s"]
fn overlay_window_acceptance_harness() {
    use std::path::PathBuf;
    use std::process::Command;
    let script = std::env::var_os("LIVE_HUB_ACCEPTANCE_OVERLAY_SCRIPT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("scripts").join("Acceptance-Overlay.ps1"));
    assert!(script.is_file(), "overlay acceptance script missing: {}", script.display());
    let evidence = std::env::var_os("LIVE_HUB_ACCEPTANCE_OVERLAY_EVIDENCE")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let mut command = Command::new("powershell.exe");
    command.args(["-NoProfile", "-STA", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .arg("-EvidenceDir").arg(&evidence);
    if let Some(executable) = std::env::var_os("LIVE_HUB_ACCEPTANCE_OVERLAY_EXE") {
        command.arg("-Exe").arg(executable);
    }
    let status = command.status().expect("start the overlay acceptance harness");
    assert!(status.success(), "overlay acceptance harness failed: {status}");
}
