//! Long-duration and concurrency stress harness for the loopback FLV relay.
//!
//! Every test here is #[ignore]d: the default offline suite stays fast, uses no
//! network, no player window and no user data. Run through
//! scripts/Stress-Relay.ps1 or directly:
//!
//!     cargo test --locked --offline --no-default-features --test relay_stress \
//!         -- --ignored --nocapture --test-threads=1 stress_
//!
//! The relay under test runs as a real child process of this test binary's
//! synthetic fixture_process helper. All upstream data comes from a loopback
//! TcpListener in this process; the relay's loopback-fixture escape hatch is
//! enabled only for that child. No platform request, player executable,
//! credential, GUI or foreground input is involved.
#![cfg(windows)]

use anyhow::{Context, Result, bail};
use live_hub_rust::{
    core::ResolvedStream,
    flv::{self, FlvTag},
    relay::{self, RelayConfig, RelayServer},
};
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const FLV_HEADER: &[u8] = b"FLV\x01\x05\0\0\0\x09\0\0\0\0";
const WAIT_TIMEOUT: u32 = 258;
const MODE: &str = "STRESS_MODE";

// ---------------------------------------------------------------------------
// Env helpers
// ---------------------------------------------------------------------------

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_ms(name: &str, default_ms: u64) -> Duration {
    Duration::from_millis(env_u64(name, default_ms))
}

fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref(),
        Some("1" | "true" | "TRUE")
    )
}

fn env_string(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

// ---------------------------------------------------------------------------
// Win32 process inspection (RSS / handles / threads): no new dependency.
// ---------------------------------------------------------------------------

unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn GetProcessHandleCount(handle: *mut c_void, count: *mut u32) -> i32;
    fn K32GetProcessMemoryInfo(
        process: *mut c_void,
        counters: *mut ProcessMemoryCounters,
        cb: u32,
    ) -> i32;
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> *mut c_void;
    fn Thread32First(snapshot: *mut c_void, entry: *mut ThreadEntry32) -> i32;
    fn Thread32Next(snapshot: *mut c_void, entry: *mut ThreadEntry32) -> i32;
}

const PROCESS_QUERY_INFORMATION: u32 = 0x0400;
const PROCESS_VM_READ: u32 = 0x0010;
const PROCESS_SYNCHRONIZE: u32 = 0x0010_0000;
const PROCESS_QUERY_LIMITED: u32 = 0x1000;
const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;

#[repr(C)]
struct ProcessMemoryCounters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_usage: usize,
}

#[repr(C)]
struct ThreadEntry32 {
    dw_size: u32,
    cnt_usage: u32,
    th32_thread_id: u32,
    th32_owner_process_id: u32,
    tp_base_pri: i32,
    tp_delta_pri: i32,
    dw_flags: u32,
}

#[derive(Clone, Copy, Debug)]
struct ProcessMetrics {
    rss_bytes: usize,
    private_bytes: usize,
    handles: u32,
    threads: u32,
}

struct ProcessHandle {
    raw: *mut c_void,
    pid: u32,
}

unsafe impl Send for ProcessHandle {}
unsafe impl Sync for ProcessHandle {}

impl ProcessHandle {
    fn open(pid: u32) -> Result<Self> {
        let access = PROCESS_QUERY_INFORMATION
            | PROCESS_VM_READ
            | PROCESS_SYNCHRONIZE
            | PROCESS_QUERY_LIMITED;
        let raw = unsafe { OpenProcess(access, 0, pid) };
        if raw.is_null() {
            return Err(io::Error::last_os_error()).context(format!("cannot open process {pid}"));
        }
        Ok(Self { raw, pid })
    }

    /// 0 means the process has exited; WAIT_TIMEOUT (258) means it is running.
    fn wait(&self, milliseconds: u32) -> u32 {
        unsafe { WaitForSingleObject(self.raw, milliseconds) }
    }

    fn is_running(&self) -> bool {
        self.wait(0) == WAIT_TIMEOUT
    }

    fn metrics(&self) -> Result<ProcessMetrics> {
        let mut counters = std::mem::MaybeUninit::<ProcessMemoryCounters>::zeroed();
        let size = std::mem::size_of::<ProcessMemoryCounters>() as u32;
        let memory_ok = unsafe {
            let pointer = counters.as_mut_ptr();
            (*pointer).cb = size;
            K32GetProcessMemoryInfo(self.raw, pointer, size) != 0
        };
        if !memory_ok {
            return Err(io::Error::last_os_error()).context("cannot read process memory counters");
        }
        let counters = unsafe { counters.assume_init() };
        let mut handles = 0_u32;
        if unsafe { GetProcessHandleCount(self.raw, &mut handles) } == 0 {
            return Err(io::Error::last_os_error()).context("cannot read process handle count");
        }
        Ok(ProcessMetrics {
            rss_bytes: counters.working_set_size,
            private_bytes: counters.private_usage,
            handles,
            threads: thread_count(self.pid),
        })
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.raw) };
    }
}

/// Count threads owned by pid through a Toolhelp snapshot.
fn thread_count(pid: u32) -> u32 {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot.is_null() || snapshot as isize == -1 {
        return 0;
    }
    let mut entry = ThreadEntry32 {
        dw_size: std::mem::size_of::<ThreadEntry32>() as u32,
        cnt_usage: 0,
        th32_thread_id: 0,
        th32_owner_process_id: 0,
        tp_base_pri: 0,
        tp_delta_pri: 0,
        dw_flags: 0,
    };
    let mut count = 0_u32;
    let mut ok = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while ok {
        if entry.th32_owner_process_id == pid {
            count += 1;
        }
        ok = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    count
}

// ---------------------------------------------------------------------------
// Synthetic upstream FLV source
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct UpstreamConfig {
    min_session: Duration,
    max_session: Duration,
    tag_interval: Duration,
    partial_tail: bool,
}

impl UpstreamConfig {
    /// A reconnect storm: the upstream drops every 5-10 seconds, some sessions
    /// overlap their predecessor and every third restarts its clock at zero.
    /// Sessions always end in the middle of a declared tag so the relay must
    /// never splice a partial tag into the next response.
    fn storm() -> Self {
        Self {
            min_session: Duration::from_secs(5),
            max_session: Duration::from_secs(10),
            tag_interval: Duration::from_millis(20),
            partial_tail: true,
        }
    }
}

struct SyntheticUpstream {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    accepted: Arc<AtomicUsize>,
    completed: Arc<AtomicUsize>,
    media_clock: Arc<AtomicU64>,
    listener: Option<JoinHandle<()>>,
}

impl SyntheticUpstream {
    fn start(config: UpstreamConfig) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind synthetic upstream");
        listener
            .set_nonblocking(true)
            .expect("configure synthetic upstream");
        let address = listener.local_addr().expect("synthetic upstream address");
        let stop = Arc::new(AtomicBool::new(false));
        let accepted = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let media_clock = Arc::new(AtomicU64::new(1000));
        let worker_stop = Arc::clone(&stop);
        let worker_accepted = Arc::clone(&accepted);
        let worker_completed = Arc::clone(&completed);
        let worker_clock = Arc::clone(&media_clock);
        let listener_thread = thread::spawn(move || {
            loop {
                if worker_stop.load(Ordering::Acquire) {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        let index = worker_accepted.fetch_add(1, Ordering::AcqRel);
                        let config = config.clone();
                        let stop = Arc::clone(&worker_stop);
                        let completed = Arc::clone(&worker_completed);
                        let media_clock = Arc::clone(&worker_clock);
                        thread::spawn(move || {
                            let _ = serve_upstream_session(
                                stream,
                                index,
                                &config,
                                &stop,
                                &media_clock,
                                &completed,
                            );
                        });
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        });
        Self {
            address,
            stop,
            accepted,
            completed,
            media_clock,
            listener: Some(listener_thread),
        }
    }

    fn url(&self) -> String {
        format!("http://{}/synthetic.flv", self.address)
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    fn completed(&self) -> usize {
        self.completed.load(Ordering::SeqCst)
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.listener.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for SyntheticUpstream {
    fn drop(&mut self) {
        self.stop();
    }
}

fn serve_upstream_session(
    mut stream: TcpStream,
    session: usize,
    config: &UpstreamConfig,
    stop: &AtomicBool,
    media_clock: &AtomicU64,
    completed: &AtomicUsize,
) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut request = Vec::with_capacity(256);
    let mut byte = [0_u8; 1];
    loop {
        if request.len() > 8192 {
            return Ok(());
        }
        match stream.read(&mut byte) {
            Ok(0) => return Ok(()),
            Ok(_) => {
                request.push(byte[0]);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                if stop.load(Ordering::Acquire) {
                    return Ok(());
                }
            }
            Err(_) => return Ok(()),
        }
    }
    stream.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")?;
    // Session clocks deliberately exercise every reconnect path:
    //   overlap  -> a short duplicate tail the relay must drop;
    //   reset    -> an upstream clock restarting at zero that must be rebased;
    //   continue -> a clean continuation that must pass immediately.
    let previous = media_clock.load(Ordering::Acquire);
    let start = if session == 0 {
        1000
    } else {
        match session % 3 {
            0 => previous.saturating_sub(2000),
            1 => 0,
            _ => previous.saturating_add(40),
        }
    };
    stream.write_all(FLV_HEADER)?;
    // Per-session codec sequence headers (timestamp zero) must survive every
    // reconnect with a rebased timestamp.
    stream.write_all(FlvTag::new(8, 0, &[0xaf, 0, 0]).unwrap().as_bytes())?;
    stream.write_all(FlvTag::new(9, 0, &[0x17, 0, 0, 0, 0]).unwrap().as_bytes())?;

    let span_ms = config
        .max_session
        .saturating_sub(config.min_session)
        .as_millis() as u64;
    let extra_ms = if span_ms == 0 {
        0
    } else {
        ((session as u64 * 37) % 100) * span_ms / 100
    };
    let duration = config.min_session + Duration::from_millis(extra_ms);
    let deadline = Instant::now() + duration;
    let mut sequence = 0_u64;
    while Instant::now() < deadline {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let timestamp = start.saturating_add(sequence.saturating_mul(40));
        media_clock.fetch_max(timestamp, Ordering::AcqRel);
        // Carry a realistic nontrivial payload so successful storm segments
        // satisfy the same stable-media byte budget used by production.
        let mut audio_payload = [0x11_u8; 192];
        audio_payload[0] = 0xaf;
        audio_payload[1] = 1;
        let mut video_payload = [0x22_u8; 512];
        video_payload[0] = 0x27;
        video_payload[1] = 1;
        let audio = FlvTag::new(8, timestamp as u32, &audio_payload).unwrap();
        let video = FlvTag::new(9, timestamp as u32, &video_payload).unwrap();
        if stream.write_all(audio.as_bytes()).is_err()
            || stream.write_all(video.as_bytes()).is_err()
        {
            return Ok(());
        }
        sequence += 1;
        thread::sleep(config.tag_interval);
    }
    if config.partial_tail {
        // Drop the upstream in the middle of a well-formed tag declaration. The
        // relay must discard it instead of splicing it into the next session.
        let partial = FlvTag::new(
            9,
            start.saturating_add(sequence.saturating_mul(40)) as u32,
            &[0_u8; 48],
        )
        .unwrap();
        let _ = stream.write_all(&partial.as_bytes()[..12]);
    }
    let _ = stream.shutdown(Shutdown::Both);
    completed.fetch_add(1, Ordering::AcqRel);
    Ok(())
}

// ---------------------------------------------------------------------------
// Synthetic player
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct PlayerResult {
    label: usize,
    tags: usize,
    codec_headers: usize,
    bytes: u64,
    audio_last: u32,
    video_last: u32,
    stalls: usize,
    errors: Vec<String>,
}

/// Read one relay player stream until stop is signalled or the socket ends.
///
/// control exposes a duplicate socket handle so the coordinator can shut the
/// read down deterministically at the end of a run; the player never blocks the
/// runner for a whole read timeout.
fn failed(mut result: PlayerResult, message: String) -> PlayerResult {
    result.errors.push(message);
    result
}

fn run_player(
    address: SocketAddr,
    token: &str,
    label: usize,
    stop: Arc<AtomicBool>,
    control: Arc<Mutex<Vec<Option<TcpStream>>>>,
) -> PlayerResult {
    let mut result = PlayerResult {
        label,
        ..Default::default()
    };
    let mut stream = match TcpStream::connect_timeout(&address, Duration::from_secs(5)) {
        Ok(stream) => stream,
        Err(error) => return failed(result, format!("connect: {error}")),
    };
    if let Err(error) = stream.set_read_timeout(Some(Duration::from_secs(10))) {
        return failed(result, format!("read timeout: {error}"));
    }
    let reader_stream = match stream.try_clone() {
        Ok(clone) => clone,
        Err(error) => return failed(result, format!("clone: {error}")),
    };
    if let Ok(mut slots) = control.lock() {
        if let Some(slot) = slots.get_mut(label) {
            *slot = stream.try_clone().ok();
        }
    }
    let request = format!("GET /live.flv?t={token} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    if let Err(error) = stream.write_all(request.as_bytes()) {
        return failed(result, format!("request: {error}"));
    }
    let mut reader = BufReader::new(reader_stream);
    let mut header = Vec::with_capacity(256);
    let mut byte = [0_u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => return failed(result, "relay closed before the response header".into()),
            Ok(_) => {
                header.push(byte[0]);
                if header.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Err(error) => return failed(result, format!("response header: {error}")),
        }
    }
    let first_line = String::from_utf8_lossy(&header)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    if !first_line.starts_with("HTTP/1.1 200") {
        return failed(result, format!("unexpected relay response: {first_line}"));
    }
    if let Err(error) = flv::read_header(&mut reader) {
        return failed(result, format!("FLV header: {error}"));
    }
    result.bytes = FLV_HEADER.len() as u64;
    let mut audio_last: Option<u32> = None;
    let mut video_last: Option<u32> = None;
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        match flv::read_tag(&mut reader) {
            Ok(Some(tag)) => {
                if tag.is_codec_header() {
                    result.codec_headers += 1;
                } else {
                    let timestamp = tag.timestamp();
                    match tag.kind() {
                        8 => {
                            if let Some(previous) = audio_last {
                                if timestamp <= previous {
                                    result.errors.push(format!(
                                        "audio timestamp regression {previous} -> {timestamp}"
                                    ));
                                    break;
                                }
                            }
                            audio_last = Some(timestamp);
                            result.audio_last = timestamp;
                        }
                        9 => {
                            if let Some(previous) = video_last {
                                if timestamp <= previous {
                                    result.errors.push(format!(
                                        "video timestamp regression {previous} -> {timestamp}"
                                    ));
                                    break;
                                }
                            }
                            video_last = Some(timestamp);
                            result.video_last = timestamp;
                        }
                        _ => {}
                    }
                }
                result.tags += 1;
                result.bytes += tag.as_bytes().len() as u64;
            }
            Ok(None) => {
                if !stop.load(Ordering::Acquire) {
                    result.errors.push("relay ended the stream early".into());
                }
                break;
            }
            Err(error) => {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) {
                    result.stalls += 1;
                    if result.stalls > 5 {
                        result.errors.push(format!("relay stalled: {error}"));
                        break;
                    }
                    continue;
                }
                result.errors.push(format!("FLV tag: {error}"));
                break;
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Fixture child process (relay / launcher / player)
// ---------------------------------------------------------------------------

fn hidden_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
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

impl ChildGuard {
    fn id(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_fixture(mode: &str) -> Command {
    let mut command = hidden_command(std::env::current_exe().expect("test exe"));
    command
        .args([
            "--exact",
            "fixture_process",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(MODE, mode);
    command
}

/// The helper process entry point. It is #[ignore]d and only ever spawned by the
/// stress tests, always with an explicit STRESS_MODE. A blanket --ignored run
/// starts it without a mode and it becomes a no-op.
#[test]
#[ignore = "spawned only as a synthetic relay/launcher/player helper process"]
fn fixture_process() {
    match std::env::var(MODE).as_deref() {
        Ok("relay") => run_relay_fixture(),
        Ok("launcher") => run_launcher_fixture(),
        Ok("player") => loop {
            thread::sleep(Duration::from_millis(50));
        },
        _ => {}
    }
}

fn run_relay_fixture() -> ! {
    let token = env_string("STRESS_TOKEN", "stress-session-token");
    let upstream =
        std::env::var("STRESS_UPSTREAM").expect("relay fixture requires STRESS_UPSTREAM");
    let config = RelayConfig {
        bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        token,
        max_connections: env_usize("STRESS_MAX_CONNECTIONS", 8),
        idle_timeout: env_ms("STRESS_IDLE_TIMEOUT_MS", 180_000),
        request_timeout: env_ms("STRESS_REQUEST_TIMEOUT_MS", 3_000),
        upstream_timeout: env_ms("STRESS_UPSTREAM_TIMEOUT_MS", 2_000),
        write_timeout: env_ms("STRESS_WRITE_TIMEOUT_MS", 3_000),
        retry_delay: env_ms("STRESS_RETRY_DELAY_MS", 50),
        // Do not hide production policy boundaries behind a raised budget.
        max_reconnects: env_usize(
            "STRESS_MAX_RECONNECTS",
            RelayConfig::default().max_reconnects,
        ),
        // The child relay may only reach the synthetic loopback source.
        allow_loopback_fixture: true,
        require_player_registration: env_flag("STRESS_REQUIRE_PLAYER"),
        player_registration_timeout: env_ms("STRESS_PLAYER_DEADLINE_MS", 15_000),
        // Synthetic fixture processes do not write component diagnostics.
        log_dir: None,
    };
    let resolver: Arc<dyn relay::StreamResolver> = Arc::new(move || {
        Ok(ResolvedStream {
            url: upstream.clone(),
            headers: BTreeMap::new(),
            title: "Synthetic stress".into(),
        })
    });
    let server = match RelayServer::bind(config, resolver) {
        Ok(server) => server,
        Err(error) => {
            eprintln!("stress relay bind failed: {error:#}");
            std::process::exit(3);
        }
    };
    let port = match server.local_addr() {
        Ok(address) => address.port(),
        Err(error) => {
            eprintln!("stress relay address failed: {error:#}");
            std::process::exit(3);
        }
    };
    println!("READY {port}");
    let _ = io::stdout().flush();
    if let Err(error) = server.run() {
        eprintln!("stress relay stopped with an error: {error:#}");
        std::process::exit(4);
    }
    std::process::exit(0)
}

fn run_launcher_fixture() -> ! {
    let player: u32 = std::env::var("STRESS_LAUNCHER_PLAYER_PID")
        .expect("launcher fixture requires STRESS_LAUNCHER_PLAYER_PID")
        .parse()
        .expect("player pid");
    let report = PathBuf::from(
        std::env::var_os("STRESS_LAUNCHER_REPORT")
            .expect("launcher fixture requires STRESS_LAUNCHER_REPORT"),
    );
    let token = env_string("STRESS_TOKEN", "stress-session-token");
    let mut relay_command = spawn_fixture("relay");
    relay_command.stdout(Stdio::piped());
    let mut relay = match relay_command.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("launcher could not spawn stress relay: {error}");
            std::process::exit(5);
        }
    };
    let stdout = relay.stdout.take().expect("stress relay stdout");
    let port = match wait_ready_line(BufReader::new(stdout), Duration::from_secs(30)) {
        Some(port) => port,
        None => {
            eprintln!("launcher never saw the stress relay READY line");
            std::process::exit(5);
        }
    };
    let url = format!("http://127.0.0.1:{port}/live.flv?t={token}");
    if let Err(error) = relay::register_player(&url, player) {
        eprintln!("launcher registration failed: {error:#}");
        std::process::exit(5);
    }
    let report_value = serde_json::json!({ "relay_pid": relay.id(), "port": port });
    if let Err(error) = fs::write(
        &report,
        serde_json::to_vec(&report_value).expect("report json"),
    ) {
        eprintln!("launcher could not write its report: {error}");
        std::process::exit(5);
    }
    // Exit without waiting for the relay child: the relay owns the player
    // lifetime itself and must keep running after its launcher is gone.
    std::process::exit(0)
}

// ---------------------------------------------------------------------------
// Relay child helpers
// ---------------------------------------------------------------------------

struct FixtureRelay {
    child: ChildGuard,
    port: u16,
    token: String,
}

impl FixtureRelay {
    fn address(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.port))
    }
}

/// Read lines from a fixture child until one carries the READY protocol line.
/// The child is this same test binary, so libtest's own "running 1 test" banner
/// can precede (or share a line with) the relay's protocol line.
fn wait_ready_line(reader: impl BufRead + Send + 'static, timeout: Duration) -> Option<u16> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = reader;
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => {
                    let _ = sender.send(None);
                    return;
                }
                Ok(_) => {
                    if let Some(position) = line.find("READY ") {
                        if let Ok(port) = line[position + "READY ".len()..]
                            .split_whitespace()
                            .next()
                            .unwrap_or_default()
                            .parse::<u16>()
                        {
                            let _ = sender.send(Some(port));
                            return;
                        }
                    }
                }
            }
        }
    });
    receiver.recv_timeout(timeout).ok().flatten()
}

fn read_ready(child: &mut Child) -> u16 {
    let stdout = child.stdout.take().expect("relay stdout is piped");
    wait_ready_line(BufReader::new(stdout), Duration::from_secs(30))
        .expect("stress relay never printed READY")
}

fn spawn_relay(upstream: &str, token: &str, require_player: bool) -> FixtureRelay {
    let mut command = spawn_fixture("relay");
    command
        .stdout(Stdio::piped())
        .env("STRESS_TOKEN", token)
        .env("STRESS_UPSTREAM", upstream);
    if require_player {
        command.env("STRESS_REQUIRE_PLAYER", "1");
    }
    let mut child = command.spawn().expect("spawn stress relay");
    let port = read_ready(&mut child);
    FixtureRelay {
        child: ChildGuard(child),
        port,
        token: token.to_owned(),
    }
}

fn wait_launcher_report(path: &Path, launcher: &mut Child) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(bytes) = fs::read(path) {
            if let Ok(value) = serde_json::from_slice(&bytes) {
                return value;
            }
        }
        if let Some(status) = launcher.try_wait().expect("launcher status") {
            panic!("synthetic launcher exited before registration: {status}");
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("synthetic launcher did not report its relay");
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child.try_wait().expect("child status").is_some() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

fn send_request(address: SocketAddr, request: &str) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.write_all(request.as_bytes())?;
    Ok(stream)
}

fn read_status(stream: &mut TcpStream) -> io::Result<String> {
    let mut header = Vec::with_capacity(256);
    let mut byte = [0_u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                header.push(byte[0]);
                if header.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            // A terminal rejection may race with a reset that discards the
            // remaining receive queue; a status already read is still valid.
            Err(error) if error.kind() == io::ErrorKind::ConnectionReset && !header.is_empty() => {
                break;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(String::from_utf8_lossy(&header)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned())
}

fn get_request(token: &str) -> String {
    format!("GET /live.flv?t={token} HTTP/1.1\r\nHost: localhost\r\n\r\n")
}

// ---------------------------------------------------------------------------
// Test 1: token policy and the ninth idle connection, process level
// ---------------------------------------------------------------------------

#[test]
#[ignore = "spawns a real relay child; run via scripts/Stress-Relay.ps1"]
fn stress_rejects_bad_tokens_and_ninth_idle_connection() -> Result<()> {
    let mut upstream = SyntheticUpstream::start(UpstreamConfig {
        min_session: Duration::from_secs(3600),
        max_session: Duration::from_secs(3600),
        tag_interval: Duration::from_millis(20),
        partial_tail: false,
    });
    let token = "stress-token-a";
    let relay = spawn_relay(&upstream.url(), token, false);
    let address = relay.address();

    // Bad tokens are rejected before any upstream connection is made.
    for (label, request) in [
        ("wrong", get_request("stress-token-b")),
        (
            "missing",
            "GET /live.flv HTTP/1.1\r\nHost: localhost\r\n\r\n".to_owned(),
        ),
        ("duplicate", get_request("stress-token-a&t=stress-token-a")),
        (
            "method",
            format!("POST /live.flv?t={token} HTTP/1.1\r\nHost: localhost\r\n\r\n"),
        ),
    ] {
        let mut stream =
            send_request(address, &request).with_context(|| format!("{label} token connect"))?;
        let status = read_status(&mut stream).with_context(|| format!("{label} token status"))?;
        assert!(
            status.starts_with("HTTP/1.1 403"),
            "{label} token was not rejected: {status}"
        );
        assert_eq!(upstream.accepted(), 0, "{label} token reached the upstream");
    }
    // A request that never completes is rejected after request_timeout (400).
    let mut incomplete = send_request(
        address,
        &format!("GET /live.flv?t={token} HTTP/1.1\r\nHost: localhost\r\n"),
    )
    .context("incomplete request connect")?;
    let status = read_status(&mut incomplete).context("incomplete request status")?;
    assert!(
        status.starts_with("HTTP/1.1 400"),
        "incomplete request was not rejected: {status}"
    );

    // Hold all eight connection slots with sockets that never send a request.
    let mut held = Vec::new();
    for _ in 0..8 {
        held.push(TcpStream::connect(address)?);
    }
    thread::sleep(Duration::from_millis(400));
    let started = Instant::now();
    let mut ninth = send_request(address, &get_request(token)).context("ninth connect")?;
    let status = read_status(&mut ninth).context("ninth status")?;
    let elapsed = started.elapsed();
    assert!(
        status.starts_with("HTTP/1.1 503"),
        "ninth idle connection was not rejected: {status}"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "ninth idle connection took {elapsed:?} to reject"
    );
    assert_eq!(
        upstream.accepted(),
        0,
        "rejected connection resolved the upstream"
    );
    drop(held);
    drop(ninth);

    // Once the idle slots are released a real player is served.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut player = loop {
        let mut stream =
            send_request(address, &get_request(token)).context("admission retry connect")?;
        let status = read_status(&mut stream).context("admission retry status")?;
        if status.starts_with("HTTP/1.1 200") {
            break stream;
        }
        assert!(
            Instant::now() < deadline,
            "relay never admitted a player after the idle sockets closed: {status}"
        );
        thread::sleep(Duration::from_millis(100));
    };
    // read_status already consumed the complete HTTP response header.
    let mut flv_header = [0_u8; 13];
    player
        .read_exact(&mut flv_header)
        .context("read FLV header from authorized player")?;
    assert_eq!(&flv_header[..3], b"FLV");
    assert!(
        upstream.accepted() >= 1,
        "authorized player never resolved the upstream"
    );
    drop(player);

    // Expired token: the relay has no TTL, its token lives exactly as long as
    // the process. A restarted relay must refuse the old token.
    drop(relay);
    let restarted = spawn_relay(&upstream.url(), "stress-token-b", false);
    let mut old = send_request(restarted.address(), &get_request(token))?;
    let status = read_status(&mut old)?;
    assert!(
        status.starts_with("HTTP/1.1 403"),
        "old token was accepted by a restarted relay: {status}"
    );
    drop(restarted);
    upstream.stop();
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 2: eight concurrent players across a reconnect storm
// ---------------------------------------------------------------------------

#[test]
#[ignore = "spawns a real relay child; run via scripts/Stress-Relay.ps1"]
fn stress_eight_players_survive_reconnect_storm() -> Result<()> {
    let seconds = env_u64("STRESS_STORM_SECS", 60);
    let mut upstream = SyntheticUpstream::start(UpstreamConfig::storm());
    let relay = spawn_relay(&upstream.url(), "stress-storm-token", false);
    let stop = Arc::new(AtomicBool::new(false));
    let control: Arc<Mutex<Vec<Option<TcpStream>>>> =
        Arc::new(Mutex::new((0..8).map(|_| None).collect()));
    let players: Vec<JoinHandle<PlayerResult>> = (0..8)
        .map(|label| {
            let address = relay.address();
            let stop = Arc::clone(&stop);
            let control = Arc::clone(&control);
            thread::spawn(move || run_player(address, "stress-storm-token", label, stop, control))
        })
        .collect();

    thread::sleep(Duration::from_secs(seconds));
    stop.store(true, Ordering::Release);
    if let Ok(mut slots) = control.lock() {
        for slot in slots.iter_mut() {
            if let Some(stream) = slot.take() {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
    }
    let results: Vec<PlayerResult> = players
        .into_iter()
        .map(|handle| handle.join().expect("player thread"))
        .collect();

    let session_count = upstream.accepted();
    let summary = serde_json::json!({
        "players": results.iter().map(|result| serde_json::json!({
            "label": result.label, "tags": result.tags, "codec_headers": result.codec_headers,
            "bytes": result.bytes, "audio_last": result.audio_last, "video_last": result.video_last,
            "stalls": result.stalls, "errors": result.errors,
        })).collect::<Vec<_>>(),
        "upstream_sessions": session_count,
        "upstream_completed": upstream.completed(),
        "seconds": seconds,
    });
    println!("RELAY_STRESS_STORM {}", serde_json::to_string(&summary)?);
    if let Some(path) = std::env::var_os("STRESS_EVIDENCE") {
        fs::write(PathBuf::from(path), serde_json::to_vec_pretty(&summary)?)?;
    }
    for result in &results {
        assert!(
            result.errors.is_empty(),
            "player {}: {:?}",
            result.label,
            result.errors
        );
        assert!(
            result.tags > 200,
            "player {} only received {} tags",
            result.label,
            result.tags
        );
        assert!(
            result.codec_headers >= 4,
            "player {} saw {} codec headers (no reconnect?)",
            result.label,
            result.codec_headers
        );
        assert!(
            result.audio_last > 0 && result.video_last > 0,
            "player {} received no media",
            result.label
        );
    }
    assert!(
        session_count >= 16,
        "only {session_count} upstream sessions were opened; reconnect storm did not happen"
    );
    upstream.stop();
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 3: long soak with launcher/player supervision and resource sampling
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Sample {
    elapsed_ms: u64,
    rss_bytes: usize,
    private_bytes: usize,
    handles: u32,
    threads: u32,
}

#[test]
#[ignore = "20-30 minute soak; run via scripts/Stress-Relay.ps1"]
fn stress_soak_supervision_and_resources() -> Result<()> {
    let seconds = env_u64("STRESS_SOAK_SECS", 120);
    let sample_interval = env_ms("STRESS_SAMPLE_MS", 30_000);
    let player_count = env_usize("STRESS_PLAYERS", 8);
    let evidence = std::env::var_os("STRESS_EVIDENCE").map(PathBuf::from);
    let mut upstream = SyntheticUpstream::start(UpstreamConfig::storm());
    let temporary = tempfile::tempdir().context("soak temp dir")?;
    let report_path = temporary.path().join("launcher-report.json");
    let token = "stress-soak-token";

    // The synthetic player keeps the relay's independently owned lifetime open.
    let mut fake_player = ChildGuard(
        spawn_fixture("player")
            .spawn()
            .context("spawn synthetic player")?,
    );
    let fake_player_pid = fake_player.id();
    let mut launcher_command = spawn_fixture("launcher");
    launcher_command
        .env("STRESS_TOKEN", token)
        .env("STRESS_UPSTREAM", upstream.url())
        .env("STRESS_REQUIRE_PLAYER", "1")
        .env("STRESS_PLAYER_DEADLINE_MS", "30000")
        .env("STRESS_LAUNCHER_PLAYER_PID", fake_player_pid.to_string())
        .env("STRESS_LAUNCHER_REPORT", &report_path);
    let mut launcher = ChildGuard(
        launcher_command
            .spawn()
            .context("spawn synthetic launcher")?,
    );
    let report = wait_launcher_report(&report_path, &mut launcher.0);
    let relay_pid = report["relay_pid"]
        .as_u64()
        .context("relay pid in report")? as u32;
    let port = report["port"].as_u64().context("relay port in report")? as u16;
    assert!(
        wait_for_exit(&mut launcher.0, Duration::from_secs(10)),
        "synthetic launcher did not exit"
    );
    let relay = ProcessHandle::open(relay_pid).context("open soak relay process")?;
    thread::sleep(Duration::from_millis(500));
    assert!(
        relay.is_running(),
        "relay exited together with its launcher"
    );

    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let stop = Arc::new(AtomicBool::new(false));
    let control: Arc<Mutex<Vec<Option<TcpStream>>>> =
        Arc::new(Mutex::new((0..player_count).map(|_| None).collect()));
    let players: Vec<JoinHandle<PlayerResult>> = (0..player_count)
        .map(|label| {
            let stop = Arc::clone(&stop);
            let control = Arc::clone(&control);
            thread::spawn(move || run_player(address, token, label, stop, control))
        })
        .collect();

    let started = Instant::now();
    let mut samples = Vec::new();
    let mut next_sample = Duration::ZERO;
    while started.elapsed() < Duration::from_secs(seconds) {
        if started.elapsed() >= next_sample {
            match relay.metrics() {
                Ok(metrics) => {
                    let sample = Sample {
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        rss_bytes: metrics.rss_bytes,
                        private_bytes: metrics.private_bytes,
                        handles: metrics.handles,
                        threads: metrics.threads,
                    };
                    println!(
                        "RELAY_STRESS_SAMPLE t={}s rss={:.1}MiB private={:.1}MiB handles={} threads={}",
                        sample.elapsed_ms / 1000,
                        sample.rss_bytes as f64 / (1024.0 * 1024.0),
                        sample.private_bytes as f64 / (1024.0 * 1024.0),
                        sample.handles,
                        sample.threads,
                    );
                    samples.push(sample);
                }
                Err(error) => {
                    bail!(
                        "relay resource sampling failed after {:?}: {error:#}",
                        started.elapsed()
                    );
                }
            }
            next_sample = started.elapsed() + sample_interval;
        }
        thread::sleep(Duration::from_millis(100));
    }
    stop.store(true, Ordering::Release);
    if let Ok(mut slots) = control.lock() {
        for slot in slots.iter_mut() {
            if let Some(stream) = slot.take() {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
    }
    let results: Vec<PlayerResult> = players
        .into_iter()
        .map(|handle| handle.join().expect("player thread"))
        .collect();

    // Kill the synthetic player; the relay owns this lifetime and must exit.
    fake_player.0.kill().context("kill synthetic player")?;
    let _ = fake_player.0.wait();
    let exit_wait = relay.wait(15_000);
    assert_eq!(
        exit_wait, 0,
        "relay did not exit within 15s after its player ended"
    );
    assert!(
        !relay.is_running(),
        "relay process is still running after its player ended"
    );
    upstream.stop();

    let mut worst_errors = Vec::new();
    for result in &results {
        if !result.errors.is_empty() {
            worst_errors
                .push(serde_json::json!({ "label": result.label, "errors": result.errors }));
        }
    }
    let minimum_tags = (seconds * 10) as usize;
    for result in &results {
        assert!(
            result.tags > minimum_tags,
            "player {} only received {} tags in {}s",
            result.label,
            result.tags,
            seconds
        );
        assert!(
            result.audio_last > 0 && result.video_last > 0,
            "player {} received no media",
            result.label
        );
    }
    assert!(
        worst_errors.is_empty(),
        "player stream errors: {worst_errors:?}"
    );

    let progress = seconds * 1000 / 10;
    let base_index = samples
        .iter()
        .position(|sample| sample.elapsed_ms >= progress)
        .unwrap_or(0);
    let base = samples.get(base_index).copied().unwrap_or(Sample {
        elapsed_ms: 0,
        rss_bytes: 0,
        private_bytes: 0,
        handles: 0,
        threads: 0,
    });
    let max_rss = samples
        .iter()
        .map(|sample| sample.rss_bytes)
        .max()
        .unwrap_or(0);
    let max_handles = samples
        .iter()
        .map(|sample| sample.handles)
        .max()
        .unwrap_or(0);
    let max_threads = samples
        .iter()
        .map(|sample| sample.threads)
        .max()
        .unwrap_or(0);
    let max_rss_mb = env_u64("STRESS_MAX_RSS_MB", 512) as usize;
    let growth_mb = env_u64("STRESS_MAX_RSS_GROWTH_MB", 128) as usize;
    let max_handle_limit = env_u64("STRESS_MAX_HANDLES", 1024) as u32;
    let handle_growth_limit = env_u64("STRESS_MAX_HANDLE_GROWTH", 128) as u32;
    let thread_limit = env_u64("STRESS_MAX_THREADS", 96) as u32;
    let summary = serde_json::json!({
        "mode": "synthetic-soak",
        "seconds": seconds,
        "sample_interval_ms": sample_interval.as_millis() as u64,
        "players": results.iter().map(|result| serde_json::json!({
            "label": result.label, "tags": result.tags, "codec_headers": result.codec_headers,
            "bytes": result.bytes, "audio_last": result.audio_last, "video_last": result.video_last,
            "stalls": result.stalls, "errors": result.errors,
        })).collect::<Vec<_>>(),
        "upstream_sessions": upstream.accepted(),
        "upstream_completed": upstream.completed(),
        "samples": samples.iter().map(|sample| serde_json::json!({
            "elapsed_ms": sample.elapsed_ms,
            "rss_bytes": sample.rss_bytes,
            "private_bytes": sample.private_bytes,
            "handles": sample.handles,
            "threads": sample.threads,
        })).collect::<Vec<_>>(),
        "summary": {
            "baseline_rss_bytes": base.rss_bytes,
            "max_rss_bytes": max_rss,
            "rss_growth_bytes": max_rss.saturating_sub(base.rss_bytes),
            "baseline_handles": base.handles,
            "max_handles": max_handles,
            "handle_growth": max_handles.saturating_sub(base.handles),
            "baseline_threads": base.threads,
            "max_threads": max_threads,
            "relay_exited_after_player": true,
            "launcher_exited_first": true,
        },
        "limits": {
            "max_rss_mb": max_rss_mb, "max_rss_growth_mb": growth_mb,
            "max_handles": max_handle_limit, "max_handle_growth": handle_growth_limit,
            "max_threads": thread_limit,
        },
    });
    println!("RELAY_STRESS_SOAK {}", serde_json::to_string(&summary)?);
    if let Some(path) = evidence {
        fs::write(&path, serde_json::to_vec_pretty(&summary)?).context("write soak evidence")?;
    }
    assert!(
        max_rss <= max_rss_mb * 1024 * 1024,
        "relay RSS peaked at {} MiB",
        max_rss / (1024 * 1024)
    );
    assert!(
        max_rss.saturating_sub(base.rss_bytes) <= growth_mb * 1024 * 1024,
        "relay RSS grew by {} MiB",
        max_rss.saturating_sub(base.rss_bytes) / (1024 * 1024)
    );
    assert!(
        max_handles <= max_handle_limit,
        "relay handle count peaked at {max_handles}"
    );
    assert!(
        max_handles.saturating_sub(base.handles) <= handle_growth_limit,
        "relay handle count grew by {}",
        max_handles.saturating_sub(base.handles)
    );
    assert!(
        max_threads <= thread_limit,
        "relay thread count peaked at {max_threads}"
    );
    Ok(())
}
