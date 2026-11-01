#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use live_hub_rust::{
    auth::CookieVault,
    core::{Platform, Player, Quality, Settings, Store, Target},
    danmaku, flv, platforms, playback, relay,
};
use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(version, about = "看直播 · Rust")]
struct Cli {
    /// Explicit isolated Rust data directory.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Reproducible per-page screenshot diagnostic: open the window off-screen
    /// and inactive, render every page, save PNGs and exit. Requires --data-dir.
    #[arg(long, global = true, hide = true)]
    ui_shot: Option<PathBuf>,
    /// Pages to capture for --ui-shot: "all" or a comma separated list of page
    /// keys (favorites, recent, lookup, sessions, players, danmaku, accounts, data).
    #[arg(long, global = true, default_value = "all", hide = true)]
    page: String,
    /// Logical window size for --ui-shot, e.g. 1240x720.
    #[arg(long, global = true, default_value = "1240x720", hide = true)]
    size: String,
    /// Theme for --ui-shot: dark (default) or light.
    #[arg(long, global = true, hide = true)]
    theme: Option<String>,
    #[command(subcommand)]
    command: Option<Action>,
}

#[derive(Subcommand)]
enum Action {
    /// Open the native desktop launcher.
    Gui,
    /// Parse a room without network or persistent writes.
    Parse { target: String },
    /// Resolve a media address; may contact the selected platform.
    Resolve {
        target: String,
        #[arg(long, default_value = "best")]
        quality: Quality,
    },
    /// Show a player argument plan with a supplied synthetic/local media URL.
    Plan {
        target: String,
        #[arg(long, default_value = "mpv")]
        player: Player,
        #[arg(long)]
        media_url: String,
        #[arg(long)]
        executable: PathBuf,
    },
    /// Resolve and launch the selected external player.
    Play {
        target: String,
        #[arg(long, default_value = "best")]
        quality: Quality,
        #[arg(long, default_value = "mpv")]
        player: Player,
        #[arg(long)]
        with_overlay: bool,
        #[arg(long, hide = true)]
        credential_dir: Option<PathBuf>,
    },
    /// Configure explicit local player programs and per-argument mpv flags.
    ConfigurePlayers {
        #[arg(long)]
        mpv: Option<PathBuf>,
        #[arg(long)]
        potplayer: Option<PathBuf>,
        #[arg(long)]
        yt_dlp: Option<PathBuf>,
        #[arg(long = "mpv-arg", allow_hyphen_values = true)]
        mpv_args: Vec<String>,
        #[arg(long = "potplayer-arg", allow_hyphen_values = true)]
        potplayer_args: Vec<String>,
    },
    /// Query public platform search.
    Search { platform: Platform, keyword: String },
    /// Run an authenticated loopback FLV relay process.
    Relay {
        target: String,
        #[arg(long, default_value = "best")]
        quality: Quality,
        #[arg(long)]
        token: String,
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Bind this relay to its spawned player's lifetime over the authenticated control endpoint.
        #[arg(long, hide = true)]
        watch_player: bool,
    },
    /// Follow a selected player process with the native transparent chat overlay.
    Overlay {
        target: String,
        #[arg(long)]
        player_pid: u32,
    },
    /// Explicitly import a selected favorites/history JSON into Rust's own store.
    Import {
        file: PathBuf,
        #[arg(long)]
        history: bool,
    },
    /// Explicitly manage platform cookies encrypted with Windows DPAPI.
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// Follow one platform's live danmaku for a bounded window and print counts (network).
    DanmakuProbe {
        target: String,
        #[arg(long, default_value_t = 10)]
        seconds: u64,
    },
    /// Inspect a resolved FLV stream's onMetaData and a short live window (network).
    #[command(alias = "streaminfo")]
    StreamInfo {
        target: String,
        #[arg(long, default_value = "best")]
        quality: Quality,
        /// Live measurement window in seconds; 0 skips the window.
        #[arg(long, default_value_t = 3)]
        seconds: u64,
    },
    /// Run offline installation self-checks (no network, no player, no user data).
    SelfTest,
    /// Show implemented and pending migration capabilities without network.
    Capabilities,
    /// Write a windowless structural probe document (no window, no network).
    #[command(hide = true)]
    UiProbe { output: PathBuf },
}

#[derive(Subcommand)]
enum AuthAction {
    /// Report which platforms currently have imported credentials (no values printed).
    Status,
    /// Ask the selected platform to validate this program's saved login session.
    Check { platform: Platform },
    /// Import a caller-selected Cookie-header or Netscape file for one platform.
    Import { platform: Platform, file: PathBuf },
    /// Remove imported credentials for one platform.
    Clear { platform: Platform },
}

/// Resolve the Rust data root. A portable package (bundled bin/mpv/mpv.exe or a
/// portable.marker file next to the executable) keeps its data in that same
/// directory; every other layout keeps the per-user default. The C# reference
/// program lives in its own directory and is never resolved or written here.
fn resolve_data_root(
    explicit: Option<PathBuf>,
    executable_directory: Option<&std::path::Path>,
    local_appdata: Option<std::ffi::OsString>,
    xdg_data_home: Option<std::ffi::OsString>,
) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    if let Some(directory) = executable_directory {
        if directory.join("bin").join("mpv").join("mpv.exe").is_file()
            || directory.join("portable.marker").is_file()
        {
            return Ok(directory.join("data"));
        }
    }
    if let Some(root) = local_appdata {
        return Ok(PathBuf::from(root).join("LiveHubRust"));
    }
    if let Some(root) = xdg_data_home {
        return Ok(PathBuf::from(root).join("live-hub-rust"));
    }
    executable_directory
        .map(|directory| directory.join("data"))
        .context("executable directory missing")
}

fn data_root(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if explicit.is_none()
        && let Some(directory) = std::env::var_os("LIVE_HUB_DATA_DIR")
    {
        // The launcher passes its real --data-dir to relay children through the
        // environment: relay arguments stay free of user paths, but relay.log and
        // relay state must land beside the session they belong to.
        return Ok(PathBuf::from(directory));
    }
    let executable = std::env::current_exe().ok();
    resolve_data_root(
        explicit,
        executable.as_deref().and_then(std::path::Path::parent),
        std::env::var_os("LOCALAPPDATA"),
        std::env::var_os("XDG_DATA_HOME"),
    )
}

/// Panic reports are bounded to 64 KiB; a crash keeps the tail of the log so
/// one report always fits even after earlier incidents.
const PANIC_LOG_MAX_BYTES: u64 = 64 * 1024;

fn append_panic_log(data_root: &std::path::Path, text: &str) {
    let directory = data_root.join("logs");
    let _ = (|| -> Result<()> {
        use fs2::FileExt;
        use std::io::{Read, Write};
        std::fs::create_dir_all(&directory)?;
        let path = directory.join("panic.log");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(directory.join("panic.log.lock"))?;
        lock.lock_exclusive()?;
        let mut existing = Vec::new();
        if let Ok(mut file) = std::fs::File::open(&path) {
            let _ = file.read_to_end(&mut existing);
        }
        existing.extend_from_slice(text.as_bytes());
        if existing.len() as u64 > PANIC_LOG_MAX_BYTES {
            let keep = (PANIC_LOG_MAX_BYTES / 2) as usize;
            let start = existing.len().saturating_sub(keep);
            let tail = existing[start..].to_vec();
            let _ = std::fs::write(&path, &tail);
        } else {
            let _ = std::fs::write(&path, &existing);
        }
        let _ = lock.unlock();
        Ok(())
    })();
}

/// Install a panic hook that records time, message and location under the
/// launcher data root. The previous hook still runs, so the exit status is
/// unchanged; a panic before the data root is known simply prints as before.
fn install_panic_log(data_root: Option<PathBuf>) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(root) = &data_root {
            let location = info
                .location()
                .map(|location| {
                    format!(
                        "{}:{}:{}",
                        location.file(),
                        location.line(),
                        location.column()
                    )
                })
                .unwrap_or_else(|| "unknown location".to_owned());
            let message = if let Some(text) = info.payload().downcast_ref::<&str>() {
                (*text).to_owned()
            } else if let Some(text) = info.payload().downcast_ref::<String>() {
                text.clone()
            } else {
                "non-string panic payload".to_owned()
            };
            let thread = std::thread::current()
                .name()
                .map(|name| name.to_owned())
                .unwrap_or_else(|| "unnamed".to_owned());
            let entry = format!(
                "[{}] panic in thread {}: {}\n  at {}\n",
                chrono::Local::now().to_rfc3339(),
                thread,
                message,
                location
            );
            append_panic_log(root, &entry);
        }
        previous(info);
    }));
}

/// Append one bounded, redacted run line under <data>/logs. Only the command
/// name, outcome and duration are stored: never URLs, room IDs, tokens or
/// clipboard contents. Rotation keeps one previous file.
const RUN_LOG_MAX_BYTES: u64 = 256 * 1024;

fn append_run_log(
    data_root: &std::path::Path,
    command: &str,
    ok: bool,
    elapsed: std::time::Duration,
) {
    let directory = data_root.join("logs");
    let _ = (|| -> Result<()> {
        use fs2::FileExt;
        use std::io::Write;
        std::fs::create_dir_all(&directory)?;
        let path = directory.join("live-hub-rust.log");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(directory.join("live-hub-rust.log.lock"))?;
        lock.lock_exclusive()?;
        let size = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        if size >= RUN_LOG_MAX_BYTES {
            let rotated = directory.join("live-hub-rust.log.1");
            let _ = std::fs::remove_file(&rotated);
            let _ = std::fs::rename(&path, &rotated);
        }
        let outcome = if ok { "ok" } else { "error" };
        let line = format!(
            "{} run command={} result={} duration_ms={}\n",
            chrono::Local::now().to_rfc3339(),
            command,
            outcome,
            elapsed.as_millis()
        );
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        file.write_all(line.as_bytes())?;
        file.flush()?;
        let _ = lock.unlock();
        Ok(())
    })();
}

#[derive(Default)]
struct SelfTestReport {
    checks: Vec<(String, bool, String)>,
}

impl SelfTestReport {
    fn record(&mut self, name: &str, result: Result<()>) {
        match result {
            Ok(()) => self.checks.push((name.into(), true, String::new())),
            Err(error) => self.checks.push((name.into(), false, format!("{error:#}"))),
        }
    }
    fn finish(&self) -> Result<()> {
        let failed = self.checks.iter().filter(|(_, ok, _)| !ok).count();
        let rows: Vec<serde_json::Value> = self
            .checks
            .iter()
            .map(|(name, ok, detail)| {
                serde_json::json!({
                    "name": name, "ok": ok, "detail": detail,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "stage": "self-test", "passed": self.checks.len() - failed, "failed": failed, "checks": rows,
            }))?
        );
        if failed > 0 {
            bail!("自检有 {failed} 项失败");
        }
        Ok(())
    }
}

struct SyntheticResolver;
impl relay::StreamResolver for SyntheticResolver {
    fn resolve(&self) -> Result<live_hub_rust::core::ResolvedStream> {
        Ok(live_hub_rust::core::ResolvedStream {
            url: "https://media.example.test/live.flv".into(),
            headers: Default::default(),
            title: "synthetic title".into(),
        })
    }
}

/// Offline checks a user can run after installing the preview build. This never
/// touches the network, a player window, legacy data, or the configured data root.
fn collect_self_test() -> Result<SelfTestReport> {
    let mut report = SelfTestReport::default();
    report.record(
        "target_parse",
        (|| {
            for (input, platform, room) in [
                (
                    "https://live.bilibili.com/7734200",
                    Platform::Bili,
                    "7734200",
                ),
                ("huya:333003", Platform::Huya, "333003"),
                ("douyu:9999", Platform::Douyu, "9999"),
                ("douyin:123456", Platform::Douyin, "123456"),
            ] {
                let target = Target::parse(input)?;
                anyhow::ensure!(
                    target.platform == platform && target.room == room,
                    "unexpected target for {input}"
                );
            }
            anyhow::ensure!(
                Target::parse("bili:../etc").is_err(),
                "invalid room accepted"
            );
            Ok(())
        })(),
    );
    report.record(
        "playback_plan_mpv",
        (|| {
            let stream = live_hub_rust::core::ResolvedStream {
                url: "https://media.example.test/live.flv".into(),
                headers: Default::default(),
                title: "synthetic".into(),
            };
            let plan = playback::plan(Player::Mpv, PathBuf::from("mpv.exe"), &stream)?;
            anyhow::ensure!(
                plan.arguments.iter().any(|a| a == "--"),
                "separator missing"
            );
            anyhow::ensure!(
                plan.arguments
                    .iter()
                    .any(|a| a.to_string_lossy().starts_with("--title=")),
                "title missing"
            );
            Ok(())
        })(),
    );
    report.record(
        "playback_plan_potplayer",
        (|| {
            let stream = live_hub_rust::core::ResolvedStream {
                url: "https://media.example.test/live.flv".into(),
                headers: Default::default(),
                title: "synthetic".into(),
            };
            let plan = playback::plan(
                Player::PotPlayer,
                PathBuf::from("PotPlayerMini64.exe"),
                &stream,
            )?;
            anyhow::ensure!(
                plan.arguments.iter().any(|a| a == "/new"),
                "PotPlayer flag missing"
            );
            Ok(())
        })(),
    );
    // Own a freshly created directory rather than deleting a predictable PID
    // path that could contain another run's data or a caller-created link.
    let temporary = tempfile::Builder::new()
        .prefix("live-hub-rust-selftest-")
        .tempdir()?;
    let root = temporary.path();
    let result = (|| -> Result<()> {
        std::fs::create_dir_all(&root)?;
        report.record(
            "store_round_trip",
            (|| {
                let mut store = Store::open(root.join("store"))?;
                let target = Target::parse("bili:7734200")?;
                store.add_favorite("synthetic".into(), target.clone())?;
                store.record_play("synthetic".into(), target)?;
                let reopened = Store::open(root.join("store"))?;
                anyhow::ensure!(reopened.favorites().len() == 1, "favorite not persisted");
                anyhow::ensure!(reopened.history().len() == 1, "history not persisted");
                store.remove_favorite(Platform::Bili, "7734200")?;
                anyhow::ensure!(
                    Store::open(root.join("store"))?.favorites().is_empty(),
                    "favorite not removed"
                );
                Ok(())
            })(),
        );
        report.record(
            "relay_bind",
            (|| {
                let config = relay::RelayConfig {
                    bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                    token: "synthetic-token".into(),
                    ..Default::default()
                };
                let server = relay::RelayServer::bind(config, Arc::new(SyntheticResolver))?;
                anyhow::ensure!(server.local_addr()?.port() > 0, "relay did not bind a port");
                Ok(())
            })(),
        );
        report.record(
            "credential_vault",
            (|| {
                let vault = CookieVault::open(root.join("credentials"))?;
                anyhow::ensure!(!vault.has_credentials(Platform::Bili)?, "vault not empty");
                vault.import(Platform::Bili, "SESSION=synthetic")?;
                anyhow::ensure!(
                    vault.has_credentials(Platform::Bili)?,
                    "synthetic credential not stored"
                );
                vault.remove(Platform::Bili)?;
                anyhow::ensure!(
                    !vault.has_credentials(Platform::Bili)?,
                    "synthetic credential not removed"
                );
                Ok(())
            })(),
        );
        report.record(
            "capabilities_document",
            (|| {
                let document: serde_json::Value =
                    serde_json::from_str(include_str!("../docs/capabilities.json"))?;
                anyhow::ensure!(document["stage"].is_string(), "capabilities stage missing");
                Ok(())
            })(),
        );
        Ok(())
    })();
    result?;
    Ok(report)
}

fn run_self_test() -> Result<()> {
    collect_self_test()?.finish()
}

/// Manual live validation helper: connect to one platform's danmaku for a short
/// bounded window and report event counts plus a few synthetic-safe samples. This
/// command is intentionally outside the offline test suite because it uses network.
fn run_danmaku_probe(target: &str, seconds: u64, vault: CookieVault) -> Result<()> {
    let target = Target::parse(target)?;
    let seconds = seconds.clamp(1, 120);
    let (sender, receiver) = danmaku::bounded_channel(512);
    let cancellation = danmaku::Cancellation::new();
    let worker_cancel = cancellation.clone();
    let worker_target = target.clone();
    let (result_sender, result_receiver) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("danmaku-probe".into())
        .spawn(move || {
            let result = live_hub_rust::danmaku::connect_authenticated(
                &worker_target,
                sender,
                worker_cancel,
                &vault,
            );
            let _ = result_sender.send(result.map(|_| ()).map_err(|error| format!("{error:#}")));
        })?;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut samples: Vec<String> = Vec::new();
    let mut statuses: Vec<String> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        match receiver.recv_timeout(Duration::from_millis(200)) {
            Ok(danmaku::DanmakuEvent::Message(message)) => {
                *counts.entry("message".into()).or_default() += 1;
                if samples.len() < 5 {
                    let text: String = message.text.chars().take(80).collect();
                    samples.push(format!(
                        "{}: {}",
                        message.user.chars().take(24).collect::<String>(),
                        text
                    ));
                }
            }
            Ok(danmaku::DanmakuEvent::SuperChat(sc)) => {
                *counts.entry("super_chat".into()).or_default() += 1;
                if samples.len() < 5 {
                    samples.push(format!(
                        "SC {}: {}",
                        sc.price_cents,
                        sc.text.chars().take(60).collect::<String>()
                    ));
                }
            }
            Ok(danmaku::DanmakuEvent::Status(status)) => {
                *counts.entry("status".into()).or_default() += 1;
                let text: String = status.chars().take(160).collect();
                if !statuses.contains(&text) {
                    statuses.push(text);
                }
            }
            Ok(danmaku::DanmakuEvent::DeleteSuperChat(_)) => {
                *counts.entry("delete_super_chat".into()).or_default() += 1
            }
            Ok(danmaku::DanmakuEvent::Live(_)) => {
                *counts.entry("live_event".into()).or_default() += 1
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    cancellation.cancel();
    // The receiver observes cancellation within its read timeout; report the final
    // connect error when it surfaces promptly instead of hiding it behind counts.
    let connect_result = result_receiver.recv_timeout(Duration::from_secs(2)).ok();
    drop(worker);
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "target": format!("{}:{}", target.platform, target.room), "seconds": seconds,
            "counts": counts, "statuses": statuses, "samples": samples,
            "connect_result": connect_result,
        }))?
    );
    Ok(())
}

fn parse_shot_size(value: &str) -> Result<(f32, f32)> {
    let (width, height) = value
        .split_once(['x', 'X'])
        .context("--size 需要 WxH 形式，例如 1240x720")?;
    let width: f32 = width.trim().parse().context("--size 宽度不是数字")?;
    let height: f32 = height.trim().parse().context("--size 高度不是数字")?;
    ensure!(
        width >= 320.0 && height >= 240.0 && width <= 10000.0 && height <= 10000.0,
        "--size 超出允许范围（320x240 到 10000x10000）"
    );
    Ok((width, height))
}

fn run_ui_shot_mode(cli: &Cli, dir: &std::path::Path) -> Result<()> {
    #[cfg(feature = "gui")]
    {
        let data_dir = cli
            .data_dir
            .clone()
            .context("--ui-shot 必须同时提供 --data-dir（隔离数据目录）")?;
        let (width, height) = parse_shot_size(&cli.size)?;
        let pages: Vec<String> = cli
            .page
            .split(',')
            .map(|page| page.trim().to_owned())
            .filter(|page| !page.is_empty())
            .collect();
        let dark = match cli.theme.as_deref() {
            None | Some("dark") | Some("DARK") | Some("Dark") => None,
            Some("light") | Some("LIGHT") | Some("Light") => Some(false),
            Some(other) => bail!("--theme 只支持 dark 或 light（收到 {other}）"),
        };
        live_hub_rust::gui::run_ui_shot(
            data_dir,
            live_hub_rust::gui::UiShotOptions {
                dir: dir.to_owned(),
                pages,
                width,
                height,
                settle_frames: 4,
                dark,
            },
        )?;
        return Ok(());
    }
    #[cfg(not(feature = "gui"))]
    {
        let _ = (cli, dir);
        bail!("此构建不含桌面界面，--ui-shot 不可用");
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    // Crash reports are local and bounded; install before any GUI or worker
    // thread starts so a panic anywhere in this run is captured.
    install_panic_log(data_root(cli.data_dir.clone()).ok());
    if let Some(dir) = cli.ui_shot.clone() {
        return run_ui_shot_mode(&cli, &dir);
    }
    let action = cli.command.unwrap_or(Action::Gui);
    let command = action_name(&action);
    let data_dir = cli.data_dir.clone();
    // Read-only offline commands keep their promise to leave the data root
    // untouched; every other command records one bounded, redacted run line.
    let log_root = if action_writes_data(&action) {
        data_root(data_dir.clone()).ok()
    } else {
        None
    };
    let started = Instant::now();
    let result = dispatch(data_dir, action);
    if let Some(root) = log_root {
        append_run_log(&root, command, result.is_ok(), started.elapsed());
    }
    result
}

fn action_name(action: &Action) -> &'static str {
    match action {
        Action::Gui => "gui",
        Action::Parse { .. } => "parse",
        Action::Resolve { .. } => "resolve",
        Action::Plan { .. } => "plan",
        Action::Play { .. } => "play",
        Action::ConfigurePlayers { .. } => "configure-players",
        Action::Search { .. } => "search",
        Action::Relay { .. } => "relay",
        Action::Overlay { .. } => "overlay",
        Action::Import { .. } => "import",
        Action::Auth { .. } => "auth",
        Action::DanmakuProbe { .. } => "danmaku-probe",
        Action::StreamInfo { .. } => "streaminfo",
        Action::SelfTest => "self-test",
        Action::Capabilities => "capabilities",
        Action::UiProbe { .. } => "ui-probe",
    }
}

fn action_writes_data(action: &Action) -> bool {
    !matches!(
        action,
        Action::Parse { .. }
            | Action::Plan { .. }
            | Action::Capabilities
            | Action::SelfTest
            | Action::UiProbe { .. }
    )
}

fn dispatch(data_dir: Option<PathBuf>, action: Action) -> Result<()> {
    match action {
        Action::Parse { target } => println!(
            "{}",
            serde_json::to_string_pretty(&Target::parse(&target)?)?
        ),
        Action::DanmakuProbe { target, seconds } => {
            let vault = CookieVault::open(data_root(data_dir.clone())?.join("credentials"))?;
            run_danmaku_probe(&target, seconds, vault)?;
        }
        Action::StreamInfo {
            target,
            quality,
            seconds,
        } => {
            let mut target = Target::parse(&target)?;
            target.quality = quality;
            let root = data_root(data_dir.clone())?;
            let store = Store::open(root.clone())?;
            let vault = CookieVault::open(root.join("credentials"))?;
            let resolved = platforms::resolve_authenticated(&target, store.settings(), &vault)?;
            let reader = relay::open_media_stream(&resolved)?;
            let mut upstream = std::io::BufReader::with_capacity(64 * 1024, reader);
            let metadata = flv::read_stream_info(&mut upstream)?;
            let window = if seconds == 0 {
                None
            } else {
                flv::measure_video_window(
                    &mut upstream,
                    8 * 1024 * 1024,
                    Instant::now() + Duration::from_secs(seconds.min(60)),
                )?
            };
            let metadata = metadata.map(|info| {
                serde_json::json!({
                    "width": info.width,
                    "height": info.height,
                    "fps": info.fps,
                    "video_kbps": info.video_kbps,
                    "codec": info.codec.map(|codec| codec.label()),
                })
            });
            let window = window.map(|window| {
                // Media span is robust against CDN timestamp splices; when the
                // timeline is still implausible, fps/kbps are explicitly marked
                // unavailable instead of publishing a meaningless number.
                serde_json::json!({
                    "reliable": !window.timeline_suspect,
                    "frames": window.frames,
                    "intervals": window.intervals,
                    "video_bytes": window.video_bytes,
                    "span_ms": window.span_ms,
                    "raw_timestamp_span_ms": window.raw_span_ms,
                    "wall_ms": window.wall_ms,
                    "fps": if window.timeline_suspect { serde_json::Value::Null } else { serde_json::json!(window.fps) },
                    "video_kbps": if window.timeline_suspect { serde_json::Value::Null } else { serde_json::json!(window.kbps) },
                    "median_delta_ms": window.median_delta_ms,
                    "anomalies": window.anomalies,
                    "note": if window.timeline_suspect { "时间戳不连续或单位异常：fps/码率不可用" } else { "" },
                })
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "stage": "streaminfo",
                    "target": format!("{}:{}", target.platform, target.room),
                    "title": resolved.title,
                    "quality": target.quality.to_string(),
                    "metadata": metadata,
                    "window": window,
                }))?
            );
        }
        Action::SelfTest => run_self_test()?,
        Action::Capabilities => println!("{}", include_str!("../docs/capabilities.json")),
        Action::UiProbe { output } => {
            // The Rust GUI is egui immediate mode: there is no retained widget
            // tree to export like the C# WinForms probe. This windowless
            // equivalent records the offline checks plus resolved data root
            // instead of building a window, so it works on headless sessions.
            let report = collect_self_test()?;
            let failed = report.checks.iter().filter(|(_, ok, _)| !ok).count();
            let checks: Vec<serde_json::Value> = report
                .checks
                .iter()
                .map(|(name, ok, detail)| {
                    serde_json::json!({"name": name, "ok": ok, "detail": detail})
                })
                .collect();
            let root = data_root(data_dir.clone())?;
            let document = serde_json::json!({
                "stage": "ui-probe",
                "windowed": false,
                "version": env!("CARGO_PKG_VERSION"),
                "data_root": root.to_string_lossy(),
                "passed": report.checks.len() - failed,
                "failed": failed,
                "checks": checks,
                "implementation_note": "egui 即时模式界面无可导出的控件树；本探针以无窗口离线自检代替",
            });
            std::fs::write(&output, serde_json::to_vec_pretty(&document)?)
                .with_context(|| format!("无法写入界面探针文件 {}", output.display()))?;
            println!("{}", output.display());
            if failed > 0 {
                bail!("界面探针自检有 {failed} 项失败");
            }
        }
        Action::Plan {
            target,
            player,
            media_url,
            executable,
        } => {
            let target = Target::parse(&target)?;
            let stream = live_hub_rust::core::ResolvedStream {
                url: media_url,
                headers: Default::default(),
                title: format!("{} · {}", target.platform.label(), target.room),
            };
            let plan = playback::plan(player, executable, &stream)?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"executable":plan.executable,"arguments":plan.arguments.iter().map(|x|x.to_string_lossy()).collect::<Vec<_>>()})
                )?
            );
        }
        Action::Gui => {
            #[cfg(feature = "gui")]
            live_hub_rust::gui::run(data_root(data_dir.clone())?)?;
            #[cfg(not(feature = "gui"))]
            bail!("此构建不含桌面界面，请启用默认 gui feature");
        }
        Action::Overlay { target, player_pid } => {
            #[cfg(feature = "gui")]
            {
                let root = data_root(data_dir.clone())?;
                let store = Store::open(root.clone())?;
                let credential_root = std::env::var_os("LIVE_HUB_CREDENTIAL_ROOT")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| root.join("credentials"));
                let vault = CookieVault::open(credential_root)?;
                live_hub_rust::overlay::run_with_options(
                    Target::parse(&target)?,
                    player_pid,
                    Some(vault),
                    store.settings().overlay.clone(),
                )?;
            }
            #[cfg(not(feature = "gui"))]
            {
                let _ = (target, player_pid);
                bail!("此构建不含透明弹幕窗口");
            }
        }
        Action::Auth { action } => {
            let vault = CookieVault::open(data_root(data_dir.clone())?.join("credentials"))?;
            match action {
                AuthAction::Check { platform } => {
                    let health = live_hub_rust::login::check_health(
                        platform,
                        &vault,
                        &live_hub_rust::login::Cancellation::new(),
                    )?;
                    println!("{}", serde_json::to_string_pretty(&health)?);
                }
                AuthAction::Status => {
                    let mut rows = Vec::new();
                    for platform in Platform::ALL {
                        rows.push(serde_json::json!({ "platform": platform.to_string(), "label": platform.label(), "imported": vault.has_credentials(platform)? }));
                    }
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                }
                AuthAction::Import { platform, file } => {
                    use std::io::Read;
                    let mut bytes = Vec::new();
                    std::fs::File::open(&file)?
                        .take(512 * 1024 + 1)
                        .read_to_end(&mut bytes)?;
                    if bytes.len() > 512 * 1024 {
                        bail!("凭据文件超过 512 KiB 上限");
                    }
                    let text = std::str::from_utf8(&bytes)?.trim_start_matches('\u{feff}');
                    vault.import(platform, text)?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &serde_json::json!({ "platform": platform.to_string(), "imported": true, "encrypted": "windows-dpapi" })
                        )?
                    );
                }
                AuthAction::Clear { platform } => {
                    vault.remove(platform)?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &serde_json::json!({ "platform": platform.to_string(), "imported": false })
                        )?
                    );
                }
            }
        }
        Action::Relay {
            target,
            quality,
            token,
            port,
            watch_player,
        } => {
            let mut target = Target::parse(&target)?;
            target.quality = quality;
            let root = data_root(data_dir.clone())?;
            // Relay children use the global diagnostic log sink; point it at this
            // data root so relay.log stays under <data>/logs (bounded, redacted).
            live_hub_rust::diag::init(root.join("logs"));
            let settings: Settings = if let Ok(json) = std::env::var("LIVE_HUB_RELAY_SETTINGS") {
                serde_json::from_str(&json).context("invalid relay settings")?
            } else {
                Store::open(root.clone())?.settings().clone()
            };
            let credential_root = std::env::var_os("LIVE_HUB_CREDENTIAL_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|| root.join("credentials"));
            let vault = CookieVault::open(credential_root)?;
            let initial = std::env::var("LIVE_HUB_INITIAL_STREAM")
                .ok()
                .map(|s| serde_json::from_str::<live_hub_rust::core::ResolvedStream>(&s))
                .transpose()?;
            let resolver = Arc::new(live_hub_rust::broadcast::BroadcastResolver::new(
                target, settings, vault, initial,
            ));
            relay::serve(
                relay::RelayConfig {
                    bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
                    token,
                    idle_timeout: Duration::from_secs(180),
                    require_player_registration: watch_player,
                    ..Default::default()
                },
                resolver,
            )?;
        }
        Action::Search { platform, keyword } => {
            let root = data_root(data_dir.clone())?;
            let vault = CookieVault::open(root.join("credentials"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&platforms::search_authenticated(
                    platform, &keyword, &vault
                )?)?
            );
        }
        Action::Resolve { target, quality } => {
            let mut target = Target::parse(&target)?;
            target.quality = quality;
            let root = data_root(data_dir.clone())?;
            let store = Store::open(root.clone())?;
            // Imported per-platform cookies are applied only to that platform's scoped requests.
            let vault = CookieVault::open(root.join("credentials"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&platforms::resolve_authenticated(
                    &target,
                    store.settings(),
                    &vault
                )?)?
            );
        }
        Action::ConfigurePlayers {
            mpv,
            potplayer,
            yt_dlp,
            mpv_args,
            potplayer_args,
        } => {
            let mut store = Store::open(data_root(data_dir.clone())?)?;
            let mut settings = store.settings().clone();
            if let Some(path) = mpv {
                settings.mpv = Some(path)
            }
            if let Some(path) = potplayer {
                settings.potplayer = Some(path)
            }
            if let Some(path) = yt_dlp {
                settings.yt_dlp = Some(path)
            }
            store.save_player_paths(settings)?;
            if !mpv_args.is_empty() {
                let mut preferences = store.settings().playback.clone();
                preferences.mpv_extra_args = mpv_args;
                store.save_playback_preferences(preferences)?;
            }
            if !potplayer_args.is_empty() {
                let mut preferences = store.settings().playback.clone();
                preferences.potplayer_extra_args = potplayer_args;
                store.save_playback_preferences(preferences)?;
            }
            println!("{}", serde_json::json!({"configured":true}));
        }
        Action::Play {
            target,
            quality,
            player,
            with_overlay,
            credential_dir,
        } => {
            let mut target = Target::parse(&target)?;
            target.quality = quality;
            let root = data_root(data_dir.clone())?;
            let mut store = Store::open(root.clone())?;
            let vault =
                CookieVault::open(credential_dir.unwrap_or_else(|| root.join("credentials")))?;
            let outcome =
                playback::launch_authenticated(&target, player, store.settings(), &vault)?;
            let mut output = serde_json::to_value(&outcome)?;
            if with_overlay {
                let mut command = std::process::Command::new(std::env::current_exe()?);
                command.arg("--data-dir").arg(&root).args([
                    "overlay",
                    &target.to_string(),
                    "--player-pid",
                    &outcome.player_pid.to_string(),
                ]);
                command.env("LIVE_HUB_CREDENTIAL_ROOT", vault.root());
                command
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    command.creation_flags(0x08000000);
                }
                match command.spawn() {
                    Ok(child) => output["overlay_pid"] = serde_json::json!(child.id()),
                    Err(_) => {
                        output["overlay_error"] = serde_json::json!("覆盖层未能启动，播放器仍运行")
                    }
                }
            }
            store.record_play(outcome.title.clone(), target)?;
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        Action::Import { file, history } => {
            use std::io::Read;
            let mut bytes = Vec::new();
            std::fs::File::open(&file)?
                .take(2 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > 2 * 1024 * 1024 {
                bail!("导入文件超过2MiB");
            }
            let json = std::str::from_utf8(&bytes)?.trim_start_matches('\u{feff}');
            let mut store = Store::open(data_root(data_dir.clone())?)?;
            let summary = if history {
                store.import_legacy_history_json(json)?
            } else {
                store.import_json(json)?
            };
            println!("{summary:?}");
        }
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PANIC_LOG_MAX_BYTES, RUN_LOG_MAX_BYTES, append_panic_log, append_run_log,
        install_panic_log, resolve_data_root,
    };
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    fn portable_directory(root: &Path) {
        std::fs::create_dir_all(root.join("bin/mpv")).unwrap();
        std::fs::write(root.join("bin/mpv/mpv.exe"), b"synthetic").unwrap();
    }

    #[test]
    fn portable_package_keeps_data_next_to_the_executable() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("portable");
        std::fs::create_dir_all(&directory).unwrap();
        portable_directory(&directory);
        let local = temporary.path().join("local");
        let resolved = resolve_data_root(
            None,
            Some(&directory),
            Some(local.clone().into_os_string()),
            None,
        )
        .unwrap();
        assert_eq!(resolved, directory.join("data"));
    }

    #[test]
    fn portable_marker_alone_marks_a_portable_package() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("portable");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("portable.marker"), b"lite").unwrap();
        let resolved =
            resolve_data_root(None, Some(&directory), None, Some(OsString::from("xdg"))).unwrap();
        assert_eq!(resolved, directory.join("data"));
    }

    #[test]
    fn explicit_data_directory_wins_over_a_portable_package() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("portable");
        std::fs::create_dir_all(&directory).unwrap();
        portable_directory(&directory);
        let explicit = temporary.path().join("explicit");
        let resolved = resolve_data_root(
            Some(explicit.clone()),
            Some(&directory),
            Some(OsString::from("local")),
            None,
        )
        .unwrap();
        assert_eq!(resolved, explicit);
    }

    #[test]
    fn non_portable_layout_prefers_local_app_data_then_xdg_then_executable() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("installed");
        std::fs::create_dir_all(&directory).unwrap();
        let local = temporary.path().join("local");
        let resolved = resolve_data_root(
            None,
            Some(&directory),
            Some(local.clone().into_os_string()),
            Some(OsString::from("ignored")),
        )
        .unwrap();
        assert_eq!(resolved, local.join("LiveHubRust"));
        let resolved = resolve_data_root(
            None,
            Some(&directory),
            None,
            Some(OsString::from("xdg-root")),
        )
        .unwrap();
        assert_eq!(resolved, PathBuf::from("xdg-root").join("live-hub-rust"));
        let resolved = resolve_data_root(None, Some(&directory), None, None).unwrap();
        assert_eq!(resolved, directory.join("data"));
    }

    #[test]
    fn run_log_is_bounded_and_rotates_once() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("data");
        std::fs::create_dir_all(root.join("logs")).unwrap();
        let log = root.join("logs/live-hub-rust.log");
        std::fs::write(&log, vec![b'x'; RUN_LOG_MAX_BYTES as usize]).unwrap();
        append_run_log(&root, "self-test", true, Duration::from_millis(7));
        let rotated = root.join("logs/live-hub-rust.log.1");
        assert_eq!(
            std::fs::metadata(&rotated).unwrap().len(),
            RUN_LOG_MAX_BYTES,
            "one previous file keeps the rotated log"
        );
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("command=self-test"));
        assert!(text.contains("result=ok"));
        assert!(!text.contains("http"), "run log must stay redacted");
        assert!(std::fs::metadata(&log).unwrap().len() < 4096);
        append_run_log(&root, "resolve", false, Duration::from_millis(2));
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("command=resolve"));
        assert!(text.contains("result=error"));
    }

    #[test]
    fn panic_log_is_bounded_and_keeps_the_latest_report() {
        let directory = tempfile::tempdir().unwrap();
        append_panic_log(directory.path(), &"old-report ".repeat(5_000));
        let first = std::fs::metadata(directory.path().join("logs/panic.log"))
            .unwrap()
            .len();
        assert!(first > 0 && first <= PANIC_LOG_MAX_BYTES, "{first}");
        append_panic_log(
            directory.path(),
            "the newest panic report must survive rotation",
        );
        let path = directory.path().join("logs/panic.log");
        let length = std::fs::metadata(&path).unwrap().len();
        assert!(
            length <= PANIC_LOG_MAX_BYTES,
            "panic log must stay bounded: {length}"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("the newest panic report must survive rotation"),
            "the tail of the log keeps the latest report"
        );
    }

    #[test]
    fn panic_log_records_time_message_and_location_for_a_real_panic() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_owned();
        let previous = std::panic::take_hook();
        // Reuse the production hook, then restore the test harness hook.
        install_panic_log(Some(root.clone()));
        let result = std::panic::catch_unwind(|| panic!("synthetic panic for the log test"));
        std::panic::set_hook(previous);
        assert!(result.is_err(), "catch_unwind must observe the panic");
        let text = std::fs::read_to_string(root.join("logs/panic.log")).unwrap();
        assert!(text.contains("panic in thread"), "{text}");
        assert!(text.contains("synthetic panic for the log test"), "{text}");
        assert!(text.contains(" at "), "the report keeps the source location");
    }
}
