//! Explicit, bounded tool import. Never execute a shortcut or inherit its flags.
use super::file_dialog::ToolKind;
use anyhow::{Context, Result, bail, ensure};
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

pub const YTDLP_VERSION: &str = "2026.10.26";
pub const YTDLP_SIZE: u64 = 23_575_744;
const YTDLP_HASH: &str = "f5389d3698ecde4ef06f4e6b1ce0294fe9ac1396228c8990b8d3416d6d177a7d";
const YTDLP_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/download/2026.10.26/yt-dlp.exe";

fn validate_download_integrity(size: u64, digest: &str) -> Result<()> {
    ensure!(size == YTDLP_SIZE && digest == YTDLP_HASH, "下载完整性校验失败，未启用此文件");
    Ok(())
}

pub fn run(command: &mut Command, timeout: Duration) -> Result<Vec<u8>> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command
        .spawn()
        .context("程序无法启动，请检查架构、文件完整性和运行库")?;
    let streams: [Box<dyn Read + Send>; 2] = [
        Box::new(child.stdout.take().unwrap()),
        Box::new(child.stderr.take().unwrap()),
    ];
    let (tx, rx) = std::sync::mpsc::channel();
    for stream in streams {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = stream.take(65537).read_to_end(&mut bytes).map(|_| bytes);
            let _ = tx.send(result);
        });
    }
    drop(tx);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("程序检测超时，请重新选择或重试");
            }
        }
    };
    let mut output = Vec::new();
    for _ in 0..2 {
        let bytes = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .context("程序检测响应超时")??;
        ensure!(bytes.len() <= 65536, "程序检测响应超过限额");
        output.extend(bytes);
        output.push(b'\n');
    }
    ensure!(status.success(), "程序检测失败，请确认所选文件和依赖完整");
    Ok(output)
}

#[cfg(windows)]
fn powershell() -> PathBuf {
    PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
        .join("System32/WindowsPowerShell/v1.0/powershell.exe")
}

pub fn resolve_shortcut(path: &Path) -> Result<PathBuf> {
    #[cfg(windows)]
    {
        // WScript.Shell's read-only TargetPath property does not call Resolve,
        // launch the target, search for a moved file, or display repair dialogs.
        let mut command = Command::new(powershell());
        command.env("LIVE_HUB_SHORTCUT", path).args(["-NoProfile", "-NonInteractive", "-Command",
            "$ErrorActionPreference='Stop'; [Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); $s=(New-Object -ComObject WScript.Shell).CreateShortcut($env:LIVE_HUB_SHORTCUT); [Console]::Write($s.TargetPath)"]);
        let bytes = run(&mut command, Duration::from_secs(5))?;
        let target = PathBuf::from(String::from_utf8(bytes)?.trim());
        ensure!(
            target.is_absolute() && target.is_file(),
            "快捷方式目标已失效或属于特殊入口，请重新选择程序"
        );
        ensure!(
            target
                .extension()
                .is_some_and(|s| s.eq_ignore_ascii_case("exe")),
            "快捷方式必须直接指向播放器或 yt-dlp.exe；不支持脚本、网页、安装器包装或嵌套链接"
        );
        Ok(target)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        bail!("快捷方式解析仅支持 Windows");
    }
}

pub fn probe(tool: ToolKind, path: &Path) -> Result<String> {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    ensure!(
        tool.names().iter().any(|s| name.eq_ignore_ascii_case(s)),
        "请选择 {} 程序本体或直接指向它的快捷方式；不支持安装器、脚本和启动包装",
        tool.label()
    );
    if tool == ToolKind::PotPlayer {
        // PotPlayer has no safe documented console version switch. Check its
        // PE version resource without starting its graphical player.
        #[cfg(windows)]
        {
            let mut command = Command::new(powershell());
            command.env("LIVE_HUB_TOOL", path).args(["-NoProfile", "-NonInteractive", "-Command",
                "$ErrorActionPreference='Stop'; [Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); $v=[Diagnostics.FileVersionInfo]::GetVersionInfo($env:LIVE_HUB_TOOL); if($v.ProductName -notmatch 'PotPlayer' -or !$v.FileVersion){exit 2}; [Console]::Write($v.FileVersion)"]);
            let version = run(&mut command, Duration::from_secs(5))?;
            return Ok(format!(
                "PotPlayer {}；程序身份已识别，实际播放待测试",
                String::from_utf8_lossy(&version).trim()
            ));
        }
        #[cfg(not(windows))]
        bail!("PotPlayer 检测仅支持 Windows");
    }
    let args: &[&str] = if tool == ToolKind::Mpv {
        &["--no-config", "--load-scripts=no", "--version"]
    } else {
        &["--ignore-config", "--version"]
    };
    let output = run(Command::new(path).args(args), Duration::from_secs(5))?;
    let text = String::from_utf8_lossy(&output);
    let version = if tool == ToolKind::Mpv {
        text.lines()
            .find(|line| line.starts_with("mpv "))
            .context("未识别为 mpv 程序")?
            .to_owned()
    } else {
        text.lines()
            .map(str::trim)
            .find(|line| {
                line.len() >= 10
                    && line
                        .bytes()
                        .all(|b| b.is_ascii_digit() || b".-_".contains(&b))
            })
            .context("未识别 yt-dlp 版本")?
            .to_owned()
    };
    if tool == ToolKind::Mpv {
        #[cfg(windows)]
        crate::mpv_audio::check_capabilities(path)?;
        return Ok(format!("{version}；包含 OpenAL，实际播放待测试"));
    }
    Ok(format!("yt-dlp {version}；版本检测通过，平台解析待测试"))
}

#[cfg(windows)]
fn sha256(path: &Path) -> Result<String> {
    let mut command = Command::new(powershell());
    command.env("LIVE_HUB_HASH_FILE", path).args(["-NoProfile", "-NonInteractive", "-Command",
        "$ErrorActionPreference='Stop'; [Console]::Write((Get-FileHash -LiteralPath $env:LIVE_HUB_HASH_FILE -Algorithm SHA256).Hash.ToLowerInvariant())"]);
    Ok(
        String::from_utf8(run(&mut command, Duration::from_secs(10))?)?
            .trim()
            .to_owned(),
    )
}

pub fn install_ytdlp(
    root: &Path,
    cancel: Arc<AtomicBool>,
    progress: Arc<AtomicU64>,
) -> Result<PathBuf> {
    #[cfg(not(windows))]
    {
        let _ = (root, cancel, progress);
        bail!("下载配置仅支持 Windows");
    }
    #[cfg(windows)]
    {
        let check = || -> Result<()> {
            ensure!(
                !cancel.load(Ordering::Acquire),
                "下载已取消，旧配置保持不变"
            );
            Ok(())
        };
        check()?;
        let directory = root.join("tools").join(format!("yt-dlp-{YTDLP_VERSION}"));
        std::fs::create_dir_all(&directory).context("无法创建应用专用工具目录")?;
        let mut temp = tempfile::NamedTempFile::new_in(&directory)?;
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(180))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                let url = attempt.url();
                if attempt.previous().len() >= 5 {
                    return attempt.error("redirect limit");
                }
                if url.scheme() == "https"
                    && matches!(
                        url.host_str(),
                        Some(
                            "github.com"
                                | "release-assets.githubusercontent.com"
                                | "objects.githubusercontent.com"
                        )
                    )
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.port_or_known_default() == Some(443)
                    && url.fragment().is_none()
                {
                    attempt.follow()
                } else {
                    attempt.error("untrusted download redirect")
                }
            }))
            .build()?;
        let mut response = client
            .get(YTDLP_URL)
            .send()
            .map_err(|_| anyhow::anyhow!("无法连接官方 Release，请重试或离线导入"))?;
        ensure!(
            response.status().is_success(),
            "官方下载失败，请重试或离线导入"
        );
        ensure!(
            response.content_length().is_none_or(|n| n == YTDLP_SIZE),
            "下载大小与受维护清单不符"
        );
        let mut total = 0;
        let mut buffer = [0u8; 65536];
        loop {
            check()?;
            let n = response
                .read(&mut buffer)
                .map_err(|_| anyhow::anyhow!("下载中断，请重试或离线导入"))?;
            if n == 0 {
                break;
            }
            total += n as u64;
            ensure!(total <= YTDLP_SIZE, "下载超出受维护清单大小");
            std::io::Write::write_all(&mut temp, &buffer[..n])?;
            progress.store(total, Ordering::Release);
        }
        temp.as_file().sync_all()?;
        validate_download_integrity(total, &sha256(temp.path())?)?;
        check()?;
        let destination = directory.join("yt-dlp.exe");
        if destination.exists() {
            ensure!(
                sha256(&destination)? == YTDLP_HASH,
                "已存在同版本的不同文件，已保留；请选择已有程序或移开后重试"
            );
        } else {
            temp.persist_noclobber(&destination)
                .map_err(|_| anyhow::anyhow!("无法保存已校验工具，旧配置保持不变"))?;
        }
        check()?;
        probe(ToolKind::YtDlp, &destination)?;
        check()?;
        Ok(destination)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn installers_and_scripts_are_rejected_without_execution() {
        for name in ["setup.exe", "mpv.cmd", "powershell.exe", "yt-dlp.exe.lnk"] {
            assert!(probe(ToolKind::Mpv, Path::new(name)).is_err());
        }
    }
    #[test]
    fn incomplete_or_modified_downloads_cannot_be_enabled() {
        assert!(validate_download_integrity(YTDLP_SIZE - 1, YTDLP_HASH).is_err());
        assert!(validate_download_integrity(YTDLP_SIZE + 1, YTDLP_HASH).is_err());
        assert!(validate_download_integrity(YTDLP_SIZE, &"0".repeat(64)).is_err());
        assert!(validate_download_integrity(YTDLP_SIZE, YTDLP_HASH).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn cancelled_download_never_creates_an_installation() {
        let temp = tempfile::tempdir().unwrap();
        assert!(
            install_ytdlp(
                temp.path(),
                Arc::new(AtomicBool::new(true)),
                Arc::new(AtomicU64::new(0))
            )
            .is_err()
        );
        assert!(!temp.path().join("tools").exists());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "explicit installed-tool and shortcut probe; no player window or media"]
    fn installed_tools_and_unicode_shortcuts_probe_without_playback() {
        for (tool, key) in [
            (ToolKind::Mpv, "LIVE_HUB_TEST_MPV"),
            (ToolKind::YtDlp, "LIVE_HUB_TEST_YTDLP"),
        ] {
            let path = PathBuf::from(std::env::var_os(key).expect("explicit installed tool path"));
            assert!(!probe(tool, &path).unwrap().is_empty());
        }
        let shortcut = PathBuf::from(
            std::env::var_os("LIVE_HUB_TEST_SHORTCUT").expect("explicit synthetic shortcut"),
        );
        let target = resolve_shortcut(&shortcut).unwrap();
        assert_eq!(
            target.canonicalize().unwrap(),
            PathBuf::from(std::env::var_os("LIVE_HUB_TEST_MPV").unwrap())
                .canonicalize()
                .unwrap()
        );
        assert!(!probe(ToolKind::Mpv, &target).unwrap().is_empty());
    }
}
