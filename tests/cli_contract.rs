use std::process::Command;
use tempfile::tempdir;

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_live-hub-rust"))
}

#[test]
fn parse_and_plan_are_offline_and_do_not_initialize_data() {
    let temporary = tempdir().unwrap();
    let root = temporary.path().join("must-not-be-created");
    let parsed = cli()
        .args([
            "--data-dir",
            root.to_str().unwrap(),
            "parse",
            "https://live.bilibili.com/123456",
        ])
        .output()
        .unwrap();
    assert!(
        parsed.status.success(),
        "{}",
        String::from_utf8_lossy(&parsed.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&parsed.stdout).unwrap();
    assert_eq!(json["room"], "123456");
    assert_eq!(json["platform"], "bili");
    let planned = cli()
        .args([
            "--data-dir",
            root.to_str().unwrap(),
            "plan",
            "douyu:12345",
            "--media-url",
            "http://127.0.0.1:1234/live.flv?t=synthetic",
            "--executable",
            "synthetic player/mpv.exe",
        ])
        .output()
        .unwrap();
    assert!(
        planned.status.success(),
        "{}",
        String::from_utf8_lossy(&planned.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&planned.stdout).unwrap();
    assert_eq!(
        json["arguments"].as_array().unwrap().last().unwrap(),
        "http://127.0.0.1:1234/live.flv?t=synthetic"
    );
    assert!(!root.exists());
}

#[test]
fn invalid_room_and_missing_relay_token_fail_before_starting() {
    for args in [
        vec!["parse", "https://127.0.0.1/private"],
        vec!["relay", "bili:123456"],
    ] {
        let output = cli().args(args).output().unwrap();
        assert!(!output.status.success());
    }
}

#[test]
fn capabilities_report_pending_migration_work_explicitly() {
    let output = cli().arg("capabilities").output().unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    // 1.0.0 shipped as a release build, so the capability document's stage moved
    // from the migration-preview marker to "release"; this contract kept asserting
    // the old marker and only surfaced once the full matrix was run again.
    assert_eq!(json["stage"], "release");
    assert!(
        json["pending"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x == "long-duration-live-validation")
    );
}
#[test]
fn offline_self_test_reports_each_check() {
    let output = cli().arg("self-test").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["failed"], 0);
    assert_eq!(json["stage"], "self-test");
    assert!(
        json["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|check| check["ok"] == true)
    );
}
#[test]
fn auth_status_reports_all_platforms_without_network_or_values() {
    let temporary = tempdir().unwrap();
    let root = temporary.path().join("data");
    let output = cli()
        .args(["--data-dir", root.to_str().unwrap(), "auth", "status"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().all(|row| row["imported"] == false));
    // No cookie value or header text ever appears in the status output.
    assert!(!String::from_utf8_lossy(&output.stdout).contains("SESSION"));
}
