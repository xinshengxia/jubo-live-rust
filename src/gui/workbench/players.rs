//! First-play setup, with detection and save results next to the selected tool.
use super::*;
use crate::core::{BufferMode, Enhancement, Player, Quality};
use crate::gui::{
    file_dialog::{self, ToolKind},
    tool_setup,
};
use eframe::egui::Pos2;

impl Launcher {
    fn tool_value(&self, tool: ToolKind) -> String {
        match tool {
            ToolKind::Mpv => self.mpv_path.clone(),
            ToolKind::PotPlayer => self.potplayer_path.clone(),
            ToolKind::YtDlp => self.ytdlp_path.clone(),
        }
    }
    fn set_tool_value(&mut self, tool: ToolKind, value: String) {
        match tool {
            ToolKind::Mpv => self.mpv_path = value,
            ToolKind::PotPlayer => self.potplayer_path = value,
            ToolKind::YtDlp => self.ytdlp_path = value,
        }
    }
    fn start_tool_setup(&mut self, tool: ToolKind, download: bool, context: &egui::Context) {
        if self.tool_setup_busy || self.storage_pending > 0 {
            return;
        }
        self.tool_setup_generation = self.tool_setup_generation.wrapping_add(1);
        let id = self.tool_setup_generation;
        let original = self.tool_value(tool);
        let root = self.snapshot.root.clone();
        self.tool_setup_busy = true;
        self.tool_feedback_kind = Some(tool);
        self.tool_feedback = Some((
            if download {
                "正在从官方 Release 下载并校验…"
            } else {
                "正在解析程序位置、检查版本和能力…"
            }
            .into(),
            false,
        ));
        let cancel = Arc::new(AtomicBool::new(false));
        self.tool_cancel = cancel.clone();
        self.tool_progress.store(0, Ordering::Release);
        let progress = self.tool_progress.clone();
        let sender = self.event_sender.clone();
        let context = context.clone();
        thread::spawn(move || {
            let result = (|| -> anyhow::Result<_> {
                let path = if download {
                    tool_setup::install_ytdlp(&root, cancel.clone(), progress)?
                } else {
                    file_dialog::validate_tool_path(tool, &original)?.ok_or_else(|| {
                        anyhow!("尚未选择程序，请自动查找、选择文件或拖入快捷方式")
                    })?
                };
                ensure!(!cancel.load(Ordering::Acquire), "操作已取消");
                let report = tool_setup::probe(tool, &path)?;
                ensure!(!cancel.load(Ordering::Acquire), "操作已取消");
                Ok((path, report.chars().take(240).collect::<String>()))
            })()
            .map_err(|error| format!("{error:#}"));
            let _ = sender.send(Event::ToolSetup {
                id,
                tool,
                original,
                result,
            });
            context.request_repaint();
        });
    }
    fn browse_tool(&mut self, tool: ToolKind, context: &egui::Context) {
        if let Some(path) = file_dialog::pick_executable(tool, &self.tool_value(tool)) {
            self.set_tool_value(tool, path.display().to_string());
            self.start_tool_setup(tool, false, context);
        }
    }

    fn discover_tool(&mut self, tool: ToolKind, context: &egui::Context) {
        let current = self.tool_value(tool);
        self.tool_feedback_kind = Some(tool);
        if !current.trim().is_empty() && PathBuf::from(current.trim_matches('"')).is_file() {
            self.start_tool_setup(tool, false, context);
            return;
        }
        self.tool_choices = file_dialog::detect_tools(tool)
            .into_iter()
            .map(|path| (tool, path))
            .collect();
        if self.tool_choices.len() == 1 {
            let (_, path) = self.tool_choices.remove(0);
            self.set_tool_value(tool, path.display().to_string());
            self.start_tool_setup(tool, false, context);
        } else if self.tool_choices.is_empty() {
            self.tool_feedback = Some((
                format!(
                    "尚未找到 {}。可选择已有程序，或使用获取入口。",
                    tool.label()
                ),
                true,
            ));
        }
    }

    fn setup_feedback(
        &mut self,
        ui: &mut Ui,
        origin: Pos2,
        width: f32,
        context: &egui::Context,
        pal: &Pal,
    ) -> f32 {
        let Some((message, error)) = self.tool_feedback.clone() else {
            return 0.0;
        };
        let reserve = if self.tool_setup_busy { 100.0 } else { 0.0 };
        let text_width = (width - 32.0 - reserve).max(180.0);
        let text_height = ui
            .painter()
            .layout(message.clone(), font(12.0), pal.text, text_width)
            .size()
            .y;
        let height = (text_height + 26.0).clamp(52.0, 124.0);
        let rect = Rect::from_min_size(origin, vec2(width, height));
        rounded(ui.painter(), rect, 10.0, pal.card);
        rounded_stroke(
            ui.painter(),
            rect,
            10.0,
            Stroke::new(
                1.0,
                if error {
                    with_alpha(pal.danger, 0.6)
                } else {
                    pal.border
                },
            ),
        );
        wrap_text(
            ui.painter(),
            Rect::from_min_size(origin + vec2(16.0, 12.0), vec2(text_width, height - 20.0)),
            message,
            font(12.0),
            if error {
                pal.danger
            } else {
                pal.text_secondary
            },
        );
        if self.tool_setup_busy {
            let cancel = Rect::from_min_size(
                pos2(rect.right() - 84.0, rect.top() + 10.0),
                vec2(68.0, 30.0),
            );
            if button(ui, cancel, "取消", Btn::Secondary, pal, true).clicked() {
                self.tool_cancel.store(true, Ordering::Release);
            }
            let bytes = self.tool_progress.load(Ordering::Acquire);
            if bytes > 0 {
                let track = Rect::from_min_size(
                    pos2(rect.left() + 16.0, rect.bottom() - 5.0),
                    vec2(width - 32.0, 2.0),
                );
                rounded(ui.painter(), track, 1.0, pal.border);
                rounded(
                    ui.painter(),
                    Rect::from_min_size(
                        track.min,
                        vec2(
                            track.width() * (bytes as f32 / tool_setup::YTDLP_SIZE as f32).min(1.0),
                            2.0,
                        ),
                    ),
                    1.0,
                    pal.accent,
                );
            }
            context.request_repaint_after(Duration::from_millis(200));
        }
        height + 16.0
    }

    pub(super) fn players_page(
        &mut self,
        ui: &mut Ui,
        host: Rect,
        context: &egui::Context,
        pal: &Pal,
    ) {
        let mut children = child(ui, host);
        scroll_area(&mut children, "players-page", |ui| {
            let origin = ui.cursor().min;
            let width = ui.available_width().min(host.width());
            let pad = 20.0;
            let inner = width - pad * 2.0;
            let enabled = !self.tool_setup_busy && self.storage_pending == 0;
            let mut y = 0.0;
            text_in(
                ui.painter(),
                Rect::from_min_size(origin, vec2(width, 22.0)),
                "选好播放器，就可以开始观看。",
                font(12.0),
                pal.text_secondary,
                Align2::LEFT_CENTER,
            );
            y += 36.0;

            // One player selector, one clear file summary, and a small action row.
            let player_card = Rect::from_min_size(origin + vec2(0.0, y), vec2(width, 208.0));
            setup_panel(ui, player_card, pal);
            text_in(
                ui.painter(),
                Rect::from_min_size(player_card.min + vec2(pad, 16.0), vec2(90.0, 26.0)),
                "播放器",
                bold(15.0),
                pal.text,
                Align2::LEFT_CENTER,
            );
            if inner > 620.0 {
                text_in(
                    ui.painter(),
                    Rect::from_min_size(
                        player_card.min + vec2(pad + 96.0, 16.0),
                        vec2(230.0, 26.0),
                    ),
                    "mpv 与 PotPlayer 任选其一",
                    font(11.0),
                    pal.text_secondary,
                    Align2::LEFT_CENTER,
                );
            }
            let mut selected = usize::from(self.player_edit == Player::PotPlayer);
            let selector = Rect::from_min_size(
                pos2(player_card.right() - pad - 148.0, player_card.top() + 14.0),
                vec2(148.0, 30.0),
            );
            if dropdown(
                ui,
                selector,
                "player-edit",
                &mut selected,
                &["mpv", "PotPlayer"],
                pal,
            )
            .changed()
            {
                self.player_edit = if selected == 0 {
                    Player::Mpv
                } else {
                    Player::PotPlayer
                };
            }
            let tool = if self.player_edit == Player::Mpv {
                ToolKind::Mpv
            } else {
                ToolKind::PotPlayer
            };
            let value = self.tool_value(tool);
            let saved = match tool {
                ToolKind::Mpv => self.snapshot.settings.mpv.as_ref(),
                _ => self.snapshot.settings.potplayer.as_ref(),
            }
            .map(|p| p.display().to_string());
            setup_file_summary(
                ui,
                player_card.min + vec2(pad, 62.0),
                inner - 142.0,
                &value,
                saved.as_deref(),
                pal,
            );
            let browse = Rect::from_min_size(
                pos2(player_card.right() - pad - 120.0, player_card.top() + 71.0),
                vec2(120.0, 34.0),
            );
            if !value.is_empty()
                && button(ui, browse, "更换程序…", Btn::Secondary, pal, enabled).clicked()
            {
                self.browse_tool(tool, context);
            }
            let actions = player_card.min + vec2(pad, 130.0);
            if button(
                ui,
                Rect::from_min_size(actions, vec2(112.0, 34.0)),
                if value.is_empty() {
                    "选择播放器…"
                } else {
                    "检测并保存"
                },
                Btn::Primary,
                pal,
                enabled,
            )
            .clicked()
            {
                if value.is_empty() {
                    self.browse_tool(tool, context);
                } else {
                    self.start_tool_setup(tool, false, context);
                }
            }
            if button(
                ui,
                Rect::from_min_size(actions + vec2(124.0, 0.0), vec2(84.0, 34.0)),
                "自动查找",
                Btn::Secondary,
                pal,
                enabled,
            )
            .clicked()
            {
                self.discover_tool(tool, context);
            }
            if button(
                ui,
                Rect::from_min_size(actions + vec2(220.0, 0.0), vec2(80.0, 34.0)),
                "手动填写",
                Btn::Subtle,
                pal,
                enabled,
            )
            .clicked()
            {
                self.players_path_dialog = Some(tool);
            }
            if button(
                ui,
                Rect::from_min_size(actions + vec2(308.0, 0.0), vec2(112.0, 34.0)),
                if tool == ToolKind::Mpv {
                    "获取兼容 mpv"
                } else {
                    "获取 PotPlayer"
                },
                Btn::Subtle,
                pal,
                true,
            )
            .clicked()
            {
                self.open_external_url(if tool == ToolKind::Mpv {
                    "https://github.com/xinshengxia/jubo-live-rust/blob/main/docs/tool-setup.md#mpv"
                } else {
                    "https://potplayer.daum.net/"
                });
            }
            text_in(
                ui.painter(),
                Rect::from_min_size(player_card.min + vec2(pad, 177.0), vec2(inner, 18.0)),
                if tool == ToolKind::Mpv {
                    "支持拖入程序或快捷方式 · mpv 将检查 OpenAL 兼容性"
                } else {
                    "支持拖入程序或快捷方式 · PotPlayer 无需 OpenAL"
                },
                font(11.0),
                pal.text_secondary,
                Align2::LEFT_CENTER,
            );
            y += player_card.height() + 16.0;
            if self.tool_feedback_kind != Some(ToolKind::YtDlp) {
                y += self.setup_feedback(ui, origin + vec2(0.0, y), width, context, pal);
            }

            // Optional tools stay quiet until the user needs to change them.
            let tools_height = if self.players_tools_open { 210.0 } else { 76.0 };
            let tools_card = Rect::from_min_size(origin + vec2(0.0, y), vec2(width, tools_height));
            setup_panel(ui, tools_card, pal);
            text_in(
                ui.painter(),
                Rect::from_min_size(tools_card.min + vec2(pad, 12.0), vec2(84.0, 26.0)),
                "yt-dlp",
                bold(14.0),
                pal.text,
                Align2::LEFT_CENTER,
            );
            let badge =
                Rect::from_min_size(tools_card.min + vec2(pad + 70.0, 15.0), vec2(38.0, 20.0));
            rounded(ui.painter(), badge, 5.0, pal.hover);
            text_in(
                ui.painter(),
                badge,
                "可选",
                font(10.0),
                pal.text_secondary,
                Align2::CENTER_CENTER,
            );
            let expand = Rect::from_min_size(
                pos2(tools_card.right() - pad - 76.0, tools_card.top() + 16.0),
                vec2(76.0, 30.0),
            );
            if button(
                ui,
                expand,
                if self.players_tools_open {
                    "收起"
                } else {
                    "管理…"
                },
                Btn::Subtle,
                pal,
                true,
            )
            .clicked()
            {
                self.players_tools_open = !self.players_tools_open;
            }
            if !self.players_tools_open {
                text_in(
                    ui.painter(),
                    Rect::from_min_size(tools_card.min + vec2(pad, 42.0), vec2(inner - 90.0, 20.0)),
                    if self.ytdlp_path.is_empty() {
                        "未配置 · 仅在 B站部分解析失败时使用，不影响其他功能。"
                    } else {
                        "已配置 · 仅作为 B站部分解析失败时的回退。"
                    },
                    font(11.0),
                    pal.text_secondary,
                    Align2::LEFT_CENTER,
                );
            } else {
                let value = self.ytdlp_path.clone();
                let saved = self
                    .snapshot
                    .settings
                    .yt_dlp
                    .as_ref()
                    .map(|p| p.display().to_string());
                setup_file_summary(
                    ui,
                    tools_card.min + vec2(pad, 57.0),
                    inner - 142.0,
                    &value,
                    saved.as_deref(),
                    pal,
                );
                let browse = Rect::from_min_size(
                    pos2(tools_card.right() - pad - 120.0, tools_card.top() + 67.0),
                    vec2(120.0, 34.0),
                );
                if button(ui, browse, "选择已有程序…", Btn::Secondary, pal, enabled).clicked()
                {
                    self.browse_tool(ToolKind::YtDlp, context);
                }
                let actions = tools_card.min + vec2(pad, 126.0);
                if button(
                    ui,
                    Rect::from_min_size(actions, vec2(112.0, 34.0)),
                    "检测并保存",
                    Btn::Primary,
                    pal,
                    enabled,
                )
                .clicked()
                {
                    self.start_tool_setup(ToolKind::YtDlp, false, context);
                }
                if button(
                    ui,
                    Rect::from_min_size(actions + vec2(124.0, 0.0), vec2(84.0, 34.0)),
                    "自动查找",
                    Btn::Secondary,
                    pal,
                    enabled,
                )
                .clicked()
                {
                    self.discover_tool(ToolKind::YtDlp, context);
                }
                if button(
                    ui,
                    Rect::from_min_size(actions + vec2(220.0, 0.0), vec2(112.0, 34.0)),
                    "下载并配置…",
                    Btn::Subtle,
                    pal,
                    enabled,
                )
                .clicked()
                {
                    self.tool_download_confirm = true;
                }
                if button(
                    ui,
                    Rect::from_min_size(actions + vec2(340.0, 0.0), vec2(80.0, 34.0)),
                    "GitHub 项目",
                    Btn::Subtle,
                    pal,
                    true,
                )
                .clicked()
                {
                    self.open_external_url("https://github.com/yt-dlp/yt-dlp");
                }
                let manual =
                    Rect::from_min_size(tools_card.min + vec2(pad, 174.0), vec2(90.0, 22.0));
                if button(ui, manual, "手动填写路径", Btn::Subtle, pal, enabled).clicked() {
                    self.players_path_dialog = Some(ToolKind::YtDlp);
                }
                text_in(
                    ui.painter(),
                    Rect::from_min_size(
                        tools_card.min + vec2(pad + 104.0, 174.0),
                        vec2(inner - 104.0, 22.0),
                    ),
                    "已有程序可以直接使用，无需重新下载。",
                    font(11.0),
                    pal.text_secondary,
                    Align2::LEFT_CENTER,
                );
            }
            y += tools_card.height() + 16.0;
            if self.tool_feedback_kind == Some(ToolKind::YtDlp) {
                y += self.setup_feedback(ui, origin + vec2(0.0, y), width, context, pal);
            }

            // Four equally sized preference controls; wrap as a grid on small windows.
            let columns = if inner >= 700.0 { 4 } else { 2 };
            let pref_height = if columns == 4 { 132.0 } else { 204.0 };
            let prefs = Rect::from_min_size(origin + vec2(0.0, y), vec2(width, pref_height));
            setup_panel(ui, prefs, pal);
            text_in(
                ui.painter(),
                Rect::from_min_size(prefs.min + vec2(pad, 14.0), vec2(130.0, 26.0)),
                "播放偏好",
                bold(14.0),
                pal.text,
                Align2::LEFT_CENTER,
            );
            text_in(
                ui.painter(),
                Rect::from_min_size(
                    prefs.min + vec2(pad + 100.0, 14.0),
                    vec2(inner - 100.0, 26.0),
                ),
                "用于下一次播放",
                font(11.0),
                pal.text_secondary,
                Align2::LEFT_CENTER,
            );
            let old = self.playback_draft.clone();
            let cell = (inner - 16.0 * (columns as f32 - 1.0)) / columns as f32;
            for (index, label) in ["画质", "mpv 缓冲", "弹幕", "mpv 画质增强"]
                .iter()
                .enumerate()
            {
                let at = prefs.min
                    + vec2(
                        pad + (index % columns) as f32 * (cell + 16.0),
                        52.0 + (index / columns) as f32 * 72.0,
                    );
                text_in(
                    ui.painter(),
                    Rect::from_min_size(at, vec2(cell, 18.0)),
                    label,
                    font(11.0),
                    pal.text_secondary,
                    Align2::LEFT_CENTER,
                );
                let rect = Rect::from_min_size(at + vec2(0.0, 24.0), vec2(cell, 32.0));
                match index {
                    0 => {
                        let mut value = match self.quality {
                            Quality::Best => 0,
                            Quality::High => 1,
                            Quality::Low => 2,
                        };
                        if dropdown(
                            ui,
                            rect,
                            "players-quality",
                            &mut value,
                            &["原画优先", "高画质", "流畅"],
                            pal,
                        )
                        .changed()
                        {
                            self.quality = [Quality::Best, Quality::High, Quality::Low][value];
                        }
                    }
                    1 => {
                        let mut value =
                            usize::from(self.playback_draft.buffer_mode == BufferMode::Stable);
                        if dropdown(
                            ui,
                            rect,
                            "players-buffer",
                            &mut value,
                            &["延迟更低", "播放更稳定"],
                            pal,
                        )
                        .changed()
                        {
                            self.playback_draft.buffer_mode =
                                [BufferMode::LowLatency, BufferMode::Stable][value];
                            self.launch_buffer = None;
                        }
                    }
                    2 => {
                        let mut value = self.launch_danmaku.min(2) as usize;
                        if dropdown(
                            ui,
                            rect,
                            "players-danmaku",
                            &mut value,
                            &["跟随设置", "开启", "关闭"],
                            pal,
                        )
                        .changed()
                        {
                            self.launch_danmaku = value as u8;
                        }
                    }
                    _ => {
                        let mut value = match self.playback_draft.enhancement {
                            Enhancement::Off => 0,
                            Enhancement::Light => 1,
                            Enhancement::High => 2,
                        };
                        if dropdown(
                            ui,
                            rect,
                            "players-enhance",
                            &mut value,
                            &["无", "轻度", "高质量"],
                            pal,
                        )
                        .changed()
                        {
                            self.playback_draft.enhancement =
                                [Enhancement::Off, Enhancement::Light, Enhancement::High][value];
                        }
                    }
                }
            }
            if old != self.playback_draft {
                self.send_store(StoreAction::SavePlayback(self.playback_draft.clone()));
            }
            y += prefs.height() + 16.0;

            let advanced = Rect::from_min_size(
                origin + vec2(0.0, y),
                vec2(
                    width,
                    if self.players_recovery_open {
                        188.0
                    } else {
                        52.0
                    },
                ),
            );
            setup_panel(ui, advanced, pal);
            text_in(
                ui.painter(),
                Rect::from_min_size(advanced.min + vec2(pad, 12.0), vec2(inner - 96.0, 28.0)),
                "播放与恢复设置",
                font(12.0),
                pal.text_secondary,
                Align2::LEFT_CENTER,
            );
            if button(
                ui,
                Rect::from_min_size(
                    pos2(advanced.right() - pad - 76.0, advanced.top() + 10.0),
                    vec2(76.0, 30.0),
                ),
                if self.players_recovery_open {
                    "收起"
                } else {
                    "展开"
                },
                Btn::Subtle,
                pal,
                true,
            )
            .clicked()
            {
                self.players_recovery_open = !self.players_recovery_open;
            }
            if self.players_recovery_open {
                let old = self.playback_draft.clone();
                let mut body = child(
                    ui,
                    Rect::from_min_size(advanced.min + vec2(pad, 54.0), vec2(inner, 120.0)),
                );
                body.checkbox(
                    &mut self.playback_draft.wait_for_live,
                    "未开播时等待，重新开播后继续尝试",
                );
                body.checkbox(
                    &mut self.playback_draft.monitor_room_state,
                    "显示直播间状态变化",
                );
                body.checkbox(
                    &mut self.playback_draft.relay_bili,
                    "为 B站 FLV 启用断流恢复",
                );
                body.add_space(4.0);
                body.label(
                    egui::RichText::new("登录浏览器（留空自动查找 Edge / Chrome）")
                        .size(11.0)
                        .color(pal.text_secondary),
                );
                body.add(
                    egui::TextEdit::singleline(&mut self.browser_path).desired_width(f32::INFINITY),
                );
                if old != self.playback_draft {
                    self.send_store(StoreAction::SavePlayback(self.playback_draft.clone()));
                }
            }
            y += advanced.height() + 8.0;

            if !self.tool_setup_busy {
                let dropped = context.input(|i| i.raw.dropped_files.clone());
                if !dropped.is_empty() {
                    let position = context.input(|i| i.pointer.latest_pos());
                    let target = position.and_then(|p| {
                        if player_card.contains(p) {
                            Some(tool)
                        } else if tools_card.contains(p) {
                            Some(ToolKind::YtDlp)
                        } else {
                            None
                        }
                    });
                    if let (1, Some(kind), Some(path)) = (
                        dropped.len(),
                        target,
                        dropped.first().and_then(|f| f.path.as_ref()),
                    ) {
                        self.set_tool_value(kind, path.display().to_string());
                        if kind == ToolKind::YtDlp {
                            self.players_tools_open = true;
                        }
                        self.start_tool_setup(kind, false, context);
                    } else {
                        self.tool_feedback_kind = Some(tool);
                        self.tool_feedback = Some((
                            "请将一个程序或快捷方式拖入对应的播放器 / yt-dlp 卡片。".into(),
                            true,
                        ));
                    }
                }
            }
            if context.input(|i| !i.raw.hovered_files.is_empty()) {
                if let Some(position) = context.input(|i| i.pointer.latest_pos()) {
                    for rect in [player_card, tools_card] {
                        if rect.contains(position) {
                            rounded_stroke(ui.painter(), rect, 10.0, Stroke::new(2.0, pal.accent));
                        }
                    }
                }
            }
            ui.allocate_space(vec2(width, y));
        });
        self.setup_dialogs(context, pal);
    }

    fn setup_dialogs(&mut self, context: &egui::Context, pal: &Pal) {
        if let Some(tool) = self.players_path_dialog {
            let mut open = true;
            let mut close = false;
            egui::Window::new(format!("填写 {} 程序位置", tool.label()))
                .id(Id::new("tool-path-dialog"))
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(560.0)
                .show(context, |ui| {
                    ui.add_space(6.0);
                    ui.label("可粘贴程序或 .lnk 快捷方式的完整路径。");
                    ui.add_space(10.0);
                    let mut value = self.tool_value(tool);
                    if ui
                        .add_sized(
                            [ui.available_width(), 32.0],
                            egui::TextEdit::singleline(&mut value),
                        )
                        .changed()
                    {
                        self.set_tool_value(tool, value);
                    }
                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(!self.tool_setup_busy, egui::Button::new("检测并保存"))
                            .clicked()
                        {
                            self.start_tool_setup(tool, false, context);
                            close = true;
                        }
                        if ui.button("取消").clicked() {
                            close = true;
                        }
                    });
                    ui.add_space(6.0);
                });
            if !open || close {
                self.players_path_dialog = None;
            }
        }
        if self.tool_download_confirm {
            let mut open = true;
            let mut close = false;
            egui::Window::new("下载 yt-dlp").id(Id::new("tool-download-dialog")).open(&mut open).collapsible(false).resizable(false).default_width(470.0).show(context, |ui| {
                ui.add_space(8.0);
                ui.label(format!("官方 GitHub Release · Windows x64 · {} · 约 23.6 MB", tool_setup::YTDLP_VERSION));
                ui.add_space(10.0);
                ui.label("下载校验后保存到本应用的专用目录。失败时保留现有设置，不安装 Deno / FFmpeg，也不修改系统 PATH。");
                ui.add_space(8.0);
                ui.colored_label(pal.text_secondary, "如果已经安装，返回选择已有程序即可。");
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    if ui.button("开始官方下载").clicked() { self.players_tools_open = true; self.start_tool_setup(ToolKind::YtDlp, true, context); close = true; }
                    if ui.button("取消").clicked() { close = true; }
                });
            });
            if !open || close {
                self.tool_download_confirm = false;
            }
        }
        if !self.tool_choices.is_empty() {
            let mut open = true;
            egui::Window::new("选择找到的程序")
                .id(Id::new("tool-discovery-dialog"))
                .open(&mut open)
                .collapsible(false)
                .default_width(640.0)
                .show(context, |ui| {
                    ui.label("找到多个位置，请选择要使用的程序。");
                    ui.add_space(10.0);
                    for (tool, path) in self.tool_choices.clone() {
                        if ui.button(path.display().to_string()).clicked() {
                            self.tool_choices.clear();
                            self.set_tool_value(tool, path.display().to_string());
                            self.start_tool_setup(tool, false, context);
                        }
                    }
                });
            if !open {
                self.tool_choices.clear();
            }
        }
    }
}

fn setup_panel(ui: &Ui, rect: Rect, pal: &Pal) {
    rounded(ui.painter(), rect, 12.0, pal.card);
    rounded_stroke(ui.painter(), rect, 12.0, Stroke::new(1.0, pal.border));
}

fn setup_file_summary(
    ui: &mut Ui,
    origin: Pos2,
    width: f32,
    value: &str,
    saved: Option<&str>,
    pal: &Pal,
) {
    let file = Path::new(value)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    text_in(
        ui.painter(),
        Rect::from_min_size(origin, vec2((width - 80.0).max(100.0), 25.0)),
        file.unwrap_or_else(|| "尚未选择程序".into()),
        bold(14.0),
        pal.text,
        Align2::LEFT_CENTER,
    );
    if !value.is_empty() {
        let committed = saved == Some(value);
        let badge = Rect::from_min_size(origin + vec2(width - 70.0, 2.0), vec2(68.0, 21.0));
        rounded(ui.painter(), badge, 5.0, pal.hover);
        text_in(
            ui.painter(),
            badge,
            if committed { "已保存" } else { "待检测" },
            font(10.0),
            if committed {
                pal.text_secondary
            } else {
                pal.warning
            },
            Align2::CENTER_CENTER,
        );
    }
    let detail = if value.is_empty() {
        "选择已安装的程序，或从兼容来源获取。".to_owned()
    } else {
        value.to_owned()
    };
    let detail_rect = Rect::from_min_size(origin + vec2(0.0, 30.0), vec2(width, 20.0));
    text_in(
        ui.painter(),
        detail_rect,
        &detail,
        font(11.0),
        pal.text_secondary,
        Align2::LEFT_CENTER,
    );
    ui.interact(
        detail_rect,
        ui.id().with(("tool-full-path", origin.y as i32)),
        Sense::hover(),
    )
    .on_hover_text(if value.is_empty() {
        "支持程序文件或桌面 / 开始菜单快捷方式"
    } else {
        value
    });
}
