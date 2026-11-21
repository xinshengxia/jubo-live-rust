use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Default)]
pub(super) struct Updater {
    pub(super) enabled: bool,
    pub(super) available: Option<String>,
    ticket: Option<String>,
    dialog: Option<Child>,
    last_poll: Option<Instant>,
    next_check: Option<Instant>,
    closing: bool,
}

fn cache() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("JuboLive/Updates/rust"))
}

fn start(mode: &str, ticket: &str, data: &Path) -> Result<Child, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let root = exe.parent().ok_or("无法定位启动器")?;
    let shell = PathBuf::from(std::env::var_os("SystemRoot").ok_or("需要 Windows PowerShell")?)
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut command = Command::new(shell);
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-STA",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(root.join("assets/updater/Start-Update.ps1"))
        .args(["-Product", "rust", "-Mode", mode])
        .env("JUBO_UPDATE_VERSION", env!("CARGO_PKG_VERSION"))
        .env("JUBO_UPDATE_EXE", "live-hub-rust.exe")
        .env("JUBO_UPDATE_OWNER", std::process::id().to_string())
        .env("JUBO_UPDATE_TICKET", ticket)
        .env(
            "JUBO_UPDATE_ARGS",
            serde_json::to_string(&[
                "--data-dir",
                data.to_str().ok_or("数据目录编码无效")?,
                "gui",
            ])
            .map_err(|e| e.to_string())?,
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command.spawn().map_err(|e| e.to_string())
}

fn version(s: &str) -> Option<[u32; 3]> {
    let mut p = s.trim().split('.');
    let parts = [
        p.next()?.parse().ok()?,
        p.next()?.parse().ok()?,
        p.next()?.parse().ok()?,
    ];
    p.next().is_none().then_some(parts)
}

impl Updater {
    pub(super) fn open(&mut self, data: &Path) -> Result<(), String> {
        if let Some(child) = self.dialog.as_mut()
            && child.try_wait().map_err(|e| e.to_string())?.is_none()
        {
            return Ok(());
        }
        let ticket = uuid::Uuid::new_v4().simple().to_string();
        self.dialog = Some(start("Dialog", &ticket, data)?);
        self.ticket = Some(ticket);
        Ok(())
    }

    pub(super) fn poll(
        &mut self,
        data: &Path,
        idle: impl FnOnce() -> bool,
    ) -> (bool, Option<String>) {
        if !self.enabled
            || self.closing
            || self
                .last_poll
                .is_some_and(|t| t.elapsed() < Duration::from_secs(2))
        {
            return (false, None);
        }
        let now = Instant::now();
        self.last_poll = Some(now);
        let Some(cache) = cache() else {
            return (false, None);
        };
        if self.next_check.is_none_or(|t| now >= t) {
            self.next_check = Some(now + Duration::from_secs(12 * 3600));
            let recent = fs::metadata(cache.join("last-attempt"))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|d| d < Duration::from_secs(12 * 3600));
            if !recent && !cache.join("auto-off").exists() {
                // This short-lived worker checks freshness, writes the local cache and exits.
                if let Ok(mut child) =
                    start("Auto", &uuid::Uuid::new_v4().simple().to_string(), data)
                {
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                }
            }
        }
        let mut notice = None;
        let file = cache.join("latest.txt");
        if fs::metadata(&file).is_ok_and(|m| m.len() < 32)
            && let Ok(body) = fs::read_to_string(file)
        {
            let s = body.trim();
            if version(s).is_some_and(|v| Some(v) > version(env!("CARGO_PKG_VERSION")))
                && self.available.as_deref() != Some(s)
            {
                self.available = Some(s.to_owned());
                notice = Some(format!("发现新版 {s}，可点主界面左下角的按钮更新。"));
            }
        }
        if let Some(ticket) = &self.ticket {
            let dir = cache.join("requests").join(ticket);
            if dir.join("ready").is_file()
                && !dir.join("cancel").exists()
                && idle()
                && fs::write(dir.join("idle-ok"), b"idle").is_ok()
            {
                self.closing = true;
            }
        }
        (self.closing, notice)
    }
}
