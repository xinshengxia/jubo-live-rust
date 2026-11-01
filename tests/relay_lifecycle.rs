//! Real synthetic subprocesses, local control sockets only. No platform request,
//! actual media player, credentials, GUI, or foreground input is involved.
#![cfg(windows)]
use anyhow::Result;
use live_hub_rust::{
    core::ResolvedStream,
    relay::{self, RelayConfig, RelayServer},
};
use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const MODE: &str = "LIVEHUB_LIFECYCLE_TEST_MODE";
const REPORT: &str = "LIVEHUB_LIFECYCLE_TEST_REPORT";
const PLAYER_PID: &str = "LIVEHUB_LIFECYCLE_TEST_PLAYER";
const DATA: &str = "LIVEHUB_LIFECYCLE_TEST_DATA";

#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
    fn WaitForSingleObject(handle: *mut std::ffi::c_void, milliseconds: u32) -> u32;
    fn TerminateProcess(handle: *mut std::ffi::c_void, exit_code: u32) -> i32;
    fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
}

fn hidden(program: impl AsRef<std::ffi::OsStr>) -> Command {
    use std::os::windows::process::CommandExt;
    let mut command = Command::new(program);
    command
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// This guard holds an already-opened handle to the exact spawned fixture relay.
/// Cleanup never looks up another process by a potentially recycled PID.
struct RelayGuard(usize);
impl RelayGuard {
    fn open(pid: u32) -> Self {
        let handle = unsafe { OpenProcess(0x0010_0000 | 1, 0, pid) };
        assert!(
            !handle.is_null(),
            "fixture relay exited before the test could hold its handle"
        );
        Self(handle as usize)
    }
    fn wait(&self, ms: u32) -> u32 {
        unsafe { WaitForSingleObject(self.0 as *mut _, ms) }
    }
}
impl Drop for RelayGuard {
    fn drop(&mut self) {
        unsafe {
            if self.wait(0) == 258 {
                TerminateProcess(self.0 as *mut _, 1);
                WaitForSingleObject(self.0 as *mut _, 3000);
            }
            CloseHandle(self.0 as *mut _);
        }
    }
}

fn fixture(mode: &str) -> Command {
    let mut result = hidden(std::env::current_exe().unwrap());
    result
        .args([
            "--exact",
            "lifecycle_fixture_process",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(MODE, mode);
    result
}

#[test]
#[ignore = "spawned only as a synthetic launcher/player helper"]
fn lifecycle_fixture_process() {
    match std::env::var(MODE).unwrap_or_default().as_str() {
        "player" => loop {
            thread::sleep(Duration::from_millis(50));
        },
        "launcher" => {
            let player = std::env::var(PLAYER_PID).unwrap().parse::<u32>().unwrap();
            let report = PathBuf::from(std::env::var_os(REPORT).unwrap());
            let data = PathBuf::from(std::env::var_os(DATA).unwrap());
            let mut relay = hidden(env!("CARGO_BIN_EXE_live-hub-rust"))
                .arg("--data-dir")
                .arg(data)
                .args([
                    "relay",
                    "huya:synthetic-room",
                    "--token",
                    "synthetic-lifetime-token",
                    "--watch-player",
                ])
                .env_remove("LIVE_HUB_INITIAL_STREAM")
                .env_remove("LIVE_HUB_CREDENTIAL_ROOT")
                .env_remove("LIVE_HUB_RELAY_SETTINGS")
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut line = String::new();
            BufReader::new(relay.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            let port = line
                .trim()
                .strip_prefix("READY ")
                .unwrap()
                .parse::<u16>()
                .unwrap();
            let url = format!("http://127.0.0.1:{port}/live.flv?t=synthetic-lifetime-token");
            relay::register_player(&url, player).unwrap();
            fs::write(
                report,
                serde_json::to_vec(&serde_json::json!({"relay_pid":relay.id(),"port":port}))
                    .unwrap(),
            )
            .unwrap();
            // Drop the Child handle, then let this launcher helper exit. The relay
            // must continue to own the player's lifetime itself.
        }
        _ => panic!("test fixture requires an explicit mode"),
    }
}

fn wait_report(path: &Path, launcher: &mut Child) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(bytes) = fs::read(path) {
            if let Ok(json) = serde_json::from_slice(&bytes) {
                return json;
            }
        }
        if let Some(status) = launcher.try_wait().unwrap() {
            assert!(
                status.success(),
                "synthetic launcher failed before registration: {status}"
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("synthetic launcher did not register its player");
}

#[test]
fn relay_lifetime_survives_launcher_exit_and_ends_when_player_exits() {
    let temporary = tempfile::tempdir().unwrap();
    let report = temporary.path().join("fixture.json");
    let mut player = ChildGuard(fixture("player").spawn().unwrap());
    let mut launcher = ChildGuard(
        fixture("launcher")
            .env(PLAYER_PID, player.0.id().to_string())
            .env(REPORT, &report)
            .env(DATA, temporary.path().join("isolated-data"))
            .spawn()
            .unwrap(),
    );
    let report = wait_report(&report, &mut launcher.0);
    let relay = RelayGuard::open(report["relay_pid"].as_u64().unwrap() as u32);
    let started = Instant::now();
    loop {
        if let Some(status) = launcher.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "launcher did not exit"
        );
        thread::sleep(Duration::from_millis(20));
    }
    // A mere inherited launcher thread cannot satisfy this: its process has
    // already ended, and the relay still serves its live player.
    assert_eq!(relay.wait(350), 258, "relay stopped with the launcher");
    assert!(player.0.try_wait().unwrap().is_none());
    player.0.kill().unwrap();
    player.0.wait().unwrap();
    assert_eq!(
        relay.wait(5000),
        0,
        "relay did not stop promptly with its player"
    );
}

#[test]
fn unregistered_owned_relay_exits_at_the_registration_deadline() {
    let resolver =
        Arc::new(|| -> Result<ResolvedStream> { anyhow::bail!("upstream must never be resolved") });
    let server = RelayServer::bind(
        RelayConfig {
            token: "synthetic-registration".into(),
            require_player_registration: true,
            player_registration_timeout: Duration::from_millis(150),
            idle_timeout: Duration::from_secs(180),
            ..Default::default()
        },
        resolver,
    )
    .unwrap();
    let started = Instant::now();
    server.run().unwrap();
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn registration_is_authenticated_single_player_and_does_not_resolve_media() {
    let resolver =
        Arc::new(|| -> Result<ResolvedStream> { anyhow::bail!("upstream must never be resolved") });
    let server = RelayServer::bind(
        RelayConfig {
            token: "synthetic-register-auth".into(),
            require_player_registration: true,
            player_registration_timeout: Duration::from_secs(3),
            ..Default::default()
        },
        resolver,
    )
    .unwrap();
    let address = server.local_addr().unwrap();
    let worker = thread::spawn(move || server.run());
    let player = ChildGuard(fixture("player").spawn().unwrap());
    let other = ChildGuard(fixture("player").spawn().unwrap());
    let wrong = format!("http://{address}/live.flv?t=incorrect");
    assert!(relay::register_player(&wrong, player.0.id()).is_err());
    let url = format!("http://{address}/live.flv?t=synthetic-register-auth");
    relay::register_player(&url, player.0.id()).unwrap();
    relay::register_player(&url, player.0.id()).unwrap();
    assert!(relay::register_player(&url, other.0.id()).is_err());
    drop(player);
    worker.join().unwrap().unwrap();
}
