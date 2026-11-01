//! Real-environment acceptance checks for real player playback.
//!
//! Ignored by default: the offline suite must not start a player or open a
//! window. Run on a machine with a local mpv build:
//!
//!   pwsh -NoProfile -File scripts/Invoke-Cargo.ps1 -ToolchainRoot <root> \
//!       -TargetDirectory <ascii-target> test --locked --offline \
//!       --test acceptance_playback -- --ignored --test-threads=1
//!
//! Environment: LIVE_HUB_MPV = path to a local mpv executable.
//!
//! The full live path (anonymous resolve -> plan -> real stream decode over mpv
//! JSON IPC, three platforms) is executed by scripts/Acceptance-Mpv.ps1, because
//! process-wide named-pipe IPC is more reliable outside the libtest harness on
//! this machine. This test covers the plan-option contract offline.

use anyhow::{Context, Result};
use live_hub_rust::core::{Player, ResolvedStream};
use live_hub_rust::playback;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn mpv_path() -> Option<PathBuf> {
    match std::env::var_os("LIVE_HUB_MPV") {
        Some(path) => {
            let path = PathBuf::from(path);
            assert!(
                path.is_file(),
                "LIVE_HUB_MPV does not exist: {}",
                path.display()
            );
            Some(path)
        }
        None => {
            eprintln!("skipped: set LIVE_HUB_MPV to a local mpv executable");
            None
        }
    }
}

fn synthetic_stream() -> ResolvedStream {
    ResolvedStream {
        url: "https://media.example.test/live.flv".into(),
        headers: BTreeMap::new(),
        title: "Acceptance 合成直播间".into(),
    }
}

/// Kills and reaps the child on drop so a failed assertion cannot leave an mpv
/// process behind or block on an open stderr pipe.
struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        match self.child.as_mut() {
            Some(child) => child.try_wait(),
            None => Ok(None),
        }
    }
    fn kill_wait(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill_wait();
    }
}

fn plan_arguments(plan: &[std::ffi::OsString], extra: &[String]) -> Vec<String> {
    let mut arguments: Vec<String> = plan
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    if let Some(index) = arguments
        .iter()
        .position(|argument| argument == "--force-window=immediate")
    {
        arguments[index] = "--force-window=no".into();
    }
    let separator = arguments
        .iter()
        .position(|argument| argument == "--")
        .expect("plan has a media separator");
    let mut result: Vec<String> = extra.to_vec();
    result.extend(arguments.drain(..separator));
    result.extend(arguments);
    result
}

fn spawn_mpv(executable: &std::path::Path, arguments: &[String]) -> Result<Child> {
    let mut command = Command::new(executable);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command.spawn().context("spawn mpv")
}

fn read_stderr(stderr: &mut Option<std::process::ChildStderr>) -> String {
    let mut text = String::new();
    if let Some(stderr) = stderr.as_mut() {
        let _ = stderr.take(64 * 1024).read_to_string(&mut text);
    }
    text
}

fn assert_no_option_errors(stderr: &str) {
    for marker in [
        "unknown option",
        "Error parsing option",
        "option not found",
        "Invalid value",
    ] {
        assert!(
            !stderr.contains(marker),
            "mpv rejected a planned option ({marker}):\n{stderr}"
        );
    }
}

/// Every option playback::plan emits must be accepted by the real mpv build.
/// No live media and no window: the planned media URL is a synthetic unreachable
/// host, so mpv parses every option and then fails to load (a load failure is
/// expected; an option-parse failure is the defect this test catches).
#[test]
#[ignore = "real mpv option acceptance; needs LIVE_HUB_MPV, opens no window"]
fn mpv_accepts_the_playback_plan_flags_without_a_window() -> Result<()> {
    let Some(executable) = mpv_path() else {
        return Ok(());
    };
    let stream = synthetic_stream();
    let plan = playback::plan(Player::Mpv, executable.clone(), &stream)?;
    let extras = vec![
        "--vo=null".to_string(),
        "--ao=null".to_string(),
        "--no-config".to_string(),
        "--no-terminal".to_string(),
        "--idle=yes".to_string(),
    ];
    let arguments = plan_arguments(&plan.arguments, &extras);
    let child = spawn_mpv(&executable, &arguments)?;
    let mut child = ChildGuard::new(child);
    let mut stderr = child.child.as_mut().and_then(|child| child.stderr.take());
    // A rejected option exits immediately with a usage error; a load failure for
    // the unreachable synthetic URL may keep mpv alive in --idle mode, which is fine.
    std::thread::sleep(Duration::from_secs(3));
    let early = child.try_wait().ok().flatten();
    let exit_code = early.and_then(|status| status.code());
    if early.is_none() {
        child.kill_wait();
    }
    let text = read_stderr(&mut stderr);
    assert_no_option_errors(&text);
    assert_ne!(
        exit_code,
        Some(2),
        "mpv rejected a planned option (exit 2); stderr:\n{text}"
    );
    Ok(())
}
