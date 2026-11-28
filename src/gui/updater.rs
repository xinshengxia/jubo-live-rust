use eframe::egui;
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
    worker: Option<Child>,
    installing: bool,
    downloading_online: bool,
    download: Option<DownloadProgress>,
    newer_version: bool,
    automatic: Option<bool>,
    message: String,
    last_poll: Option<Instant>,
    next_check: Option<Instant>,
    closing: bool,
}

const DOWNLOAD_POLL: Duration = Duration::from_millis(250);

#[derive(serde::Deserialize)]
struct DownloadProgress {
    downloaded: u64,
    total: u64,
}

impl DownloadProgress {
    fn read(dir: &Path, status: Option<&str>, online: bool) -> Option<Self> {
        if !online || status != Some("正在下载更新包…") || dir.join("cancel").exists() {
            return None;
        }
        let bytes = fs::read(dir.join("download.json")).ok()?;
        let progress: Self = serde_json::from_slice(&bytes).ok()?;
        (progress.total > 0 && progress.downloaded <= progress.total).then_some(progress)
    }

    fn label(&self) -> String {
        let percent = u128::from(self.downloaded) * 100 / u128::from(self.total);
        format!(
            "{percent}%  ·  {} / {}",
            super::data_page::size_text(self.downloaded),
            super::data_page::size_text(self.total)
        )
    }
}

fn cache() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("JuboLive/Updates/rust"))
}

fn start(
    mode: &str,
    ticket: &str,
    data: &Path,
    package: Option<(&Path, &Path)>,
) -> Result<Child, String> {
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
    command
        .env_remove("JUBO_UPDATE_OWNER_START")
        .env_remove("JUBO_UPDATE_OFFLINE_ZIP")
        .env_remove("JUBO_UPDATE_OFFLINE_CATALOG");
    if let Some((zip, catalog)) = package {
        command
            .env("JUBO_UPDATE_OFFLINE_ZIP", zip)
            .env("JUBO_UPDATE_OFFLINE_CATALOG", catalog);
    }
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
    pub(super) fn check(&mut self, data: &Path) {
        if self.enabled && self.worker.is_none() {
            self.run(data, false, None);
        }
    }

    fn run(&mut self, data: &Path, install: bool, package: Option<(&Path, &Path)>) {
        if self.installing || (self.worker.is_some() && package.is_none()) {
            return;
        }
        if let Some(mut previous) = self.worker.take() {
            std::thread::spawn(move || {
                let _ = previous.wait();
            });
        }
        self.newer_version = false;
        self.download = None;
        self.downloading_online = install && package.is_none();
        self.last_poll = None;
        let ticket = uuid::Uuid::new_v4().simple().to_string();
        match start(
            if install { "Install" } else { "Check" },
            &ticket,
            data,
            package,
        ) {
            Ok(child) => {
                self.worker = Some(child);
                self.ticket = Some(ticket);
                self.installing = install;
                self.message = if install {
                    "正在准备安装并校验签名…"
                } else {
                    "正在检查更新，直连失败会尝试备用线路…"
                }
                .into();
            }
            Err(err) => {
                self.downloading_online = false;
                self.message = format!("无法启动更新任务：{err}");
            }
        }
    }

    fn progress(&mut self, dir: &Path) {
        let Some(child) = self.worker.as_mut() else {
            return;
        };
        let finished = child.try_wait();
        let text = fs::read_to_string(dir.join("status.txt")).ok();
        self.download = DownloadProgress::read(dir, text.as_deref(), self.downloading_online);
        if let Some(s) = &text {
            self.message.clone_from(s);
        }
        match finished {
            Ok(None) => return,
            Ok(Some(exit)) => {
                self.newer_version = !self.installing
                    && exit.success()
                    && fs::read_to_string(dir.join("version.txt"))
                        .ok()
                        .and_then(|s| version(&s))
                        .is_some_and(|v| Some(v) > version(env!("CARGO_PKG_VERSION")));
                if text.is_none() {
                    self.message = "更新任务已停止。请重新检查。".into();
                }
            }
            Err(err) => self.message = format!("读取更新进度失败：{err}"),
        }
        self.worker = None;
        self.installing = false;
        self.downloading_online = false;
        self.download = None;
    }

    pub(super) fn page(&mut self, ui: &mut egui::Ui, data: &Path, card: egui::Color32) {
        use super::file_dialog::{build_filter, open_file_name};
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Frame::new()
                .fill(card)
                .corner_radius(8)
                .inner_margin(18)
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.heading(concat!(
                        "看直播 Rust  ·  当前版本 ",
                        env!("CARGO_PKG_VERSION")
                    ));
                    ui.add_space(12.0);
                    ui.label(if self.message.is_empty() {
                        "准备检查正式版更新。"
                    } else {
                        &self.message
                    });
                    if self.downloading_online {
                        ui.ctx().request_repaint_after(DOWNLOAD_POLL);
                    }
                    if let Some(progress) = &self.download {
                        let fraction = (progress.downloaded as f64 / progress.total as f64) as f32;
                        ui.add_space(6.0);
                        super::widgets::progress(ui, fraction, ui.available_width());
                        ui.small(progress.label());
                    }
                    ui.add_space(16.0);
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add_enabled(self.worker.is_none(), egui::Button::new("重新检查"))
                            .clicked()
                        {
                            self.check(data);
                        }
                        if self.installing {
                            if ui.button("取消安装").clicked()
                                && let Some(dir) = self.request_dir()
                            {
                                match fs::create_dir_all(&dir)
                                    .and_then(|_| fs::write(dir.join("cancel"), b"cancel"))
                                {
                                    Ok(()) => {
                                        self.download = None;
                                        self.message = "已请求取消安装。".into();
                                    }
                                    Err(err) => self.message = format!("无法取消：{err}"),
                                }
                            }
                        } else if ui
                            .add_enabled(self.newer_version, egui::Button::new("更新"))
                            .clicked()
                        {
                            self.run(data, true, None);
                        }
                    });
                    ui.add_space(16.0);
                    if let Some(dir) = cache() {
                        let marker = dir.join("auto-off");
                        let auto = self.automatic.get_or_insert_with(|| !marker.exists());
                        if ui.checkbox(auto, "自动检查正式版更新").changed() {
                            let saved = if *auto {
                                self.next_check = None;
                                fs::remove_file(&marker).or_else(|err| {
                                    if err.kind() == std::io::ErrorKind::NotFound {
                                        Ok(())
                                    } else {
                                        Err(err)
                                    }
                                })
                            } else {
                                fs::create_dir_all(dir).and_then(|_| fs::write(&marker, b"off"))
                            };
                            if let Err(err) = saved {
                                *auto = !marker.exists();
                                self.message = format!("无法保存设置：{err}");
                            }
                        }
                    }
                    ui.add_space(20.0);
                    ui.separator();
                    ui.add_space(8.0);
                    ui.strong("离线安装包");
                    ui.label("从 Rust 版同一次发布的附件下载两个文件，也可从另一台电脑拷来。");
                    ui.label("先选以 win-x64-lite.zip 结尾的安装包（无需解压），再选签名清单 channel.json。");
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add_enabled(!self.installing, egui::Button::new("从本地安装…"))
                            .clicked()
                            && let Some(zip) = open_file_name(
                                &build_filter(&[("正式版 ZIP", "*.zip")]),
                                "第 1 步，共 2 步 · 选择 Rust 正式版 ZIP 安装包",
                                None,
                            )
                            && let Some(catalog) = open_file_name(
                                &build_filter(&[("签名清单", "*.json")]),
                                "第 2 步，共 2 步 · 选择同一版本的 channel.json",
                                zip.parent(),
                            )
                        {
                            self.run(data, true, Some((&zip, &catalog)));
                        }
                        ui.hyperlink_to("打开发布页", "https://github.com/xinshengxia/jubo-live-rust/releases/latest");
                    });
                });
        });
    }

    fn request_dir(&self) -> Option<PathBuf> {
        Some(cache()?.join("requests").join(self.ticket.as_ref()?))
    }

    pub(super) fn poll(
        &mut self,
        data: &Path,
        idle: impl FnOnce() -> bool,
    ) -> (bool, Option<String>) {
        let interval = if self.downloading_online {
            DOWNLOAD_POLL
        } else {
            Duration::from_secs(2)
        };
        if !self.enabled
            || self.closing
            || self
                .last_poll
                .is_some_and(|t| t.elapsed() < interval)
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
                if let Ok(mut child) = start(
                    "Auto",
                    &uuid::Uuid::new_v4().simple().to_string(),
                    data,
                    None,
                ) {
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
        if let Some(dir) = self.request_dir() {
            self.progress(&dir);
            if self.installing
                && dir.join("ready").is_file()
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

#[cfg(test)]
mod download_tests {
    use super::*;

    #[test]
    fn download_values_follow_only_the_active_online_download() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let path = dir.join("download.json");
        let status = Some("正在下载更新包…");
        assert!(DownloadProgress::read(dir, status, true).is_none());
        for invalid in [
            r#"{"downloaded":10,"total":0}"#,
            r#"{"downloaded":21,"total":20}"#,
            "{",
        ] {
            fs::write(&path, invalid).unwrap();
            assert!(DownloadProgress::read(dir, status, true).is_none());
        }
        fs::write(&path, r#"{"downloaded":256,"total":1024}"#).unwrap();
        let row = DownloadProgress::read(dir, status, true).unwrap();
        assert_eq!(row.label(), "25%  ·  256 B / 1.00 KiB");
        assert!(DownloadProgress::read(dir, status, false).is_none());
        assert!(DownloadProgress::read(dir, Some("正在校验安装包…"), true).is_none());
        fs::write(dir.join("cancel"), b"cancel").unwrap();
        assert!(DownloadProgress::read(dir, status, true).is_none());
    }
}

#[cfg(test)]
mod sidebar_hit_test {
    use super::super::*;
    #[test]
    fn icon_and_label_activate_from_each_page_at_both_sizes() {
        for (width, height) in [(1240., 720.), (760., 520.)] {
            let ctx = egui::Context::default();
            let dir = tempfile::tempdir().unwrap();
            let mut app =
                Launcher::from_context(&ctx, Store::open(dir.path().join("data")).unwrap());
            let mut tick = 0;
            for page in Page::ALL {
                for x in [28., 82.] {
                    app.page = page;
                    app.updater.available = None;
                    let pos = egui::pos2(x, height - 30.);
                    for phase in [0, 0, 1, 2] {
                        tick += 1;
                        let mut events = vec![egui::Event::PointerMoved(pos)];
                        if phase != 0 {
                            events.push(egui::Event::PointerButton {
                                pos,
                                button: egui::PointerButton::Primary,
                                pressed: phase == 1,
                                modifiers: Default::default(),
                            });
                        }
                        let _ = ctx.run(
                            egui::RawInput {
                                screen_rect: Some(egui::Rect::from_min_size(
                                    egui::Pos2::ZERO,
                                    egui::vec2(width, height),
                                )),
                                time: Some(tick as f64 * 0.2),
                                events,
                                ..Default::default()
                            },
                            |ctx| app.render(ctx),
                        );
                    }
                    assert_eq!(app.page, Page::Updates, "{} {} x {}", page.key(), width, x);
                }
            }
        }
    }
}
