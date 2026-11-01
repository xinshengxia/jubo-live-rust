//! 播放器与画质 page: the player path card and the four per-play preference
//! dropdowns (LayoutWorkbenchPlayers). Rust's extra parsing tools (yt-dlp, the
//! login browser) and the playback toggles keep their place in a second card so
//! the richer Rust feature set is preserved inside the original page.

use super::*;
use crate::core::{BufferMode, Enhancement, Player, Quality};
use crate::gui::file_dialog::ToolKind;

const QUALITY_ITEMS: [&str; 3] = ["原画优先", "高画质", "流畅"];
const BUFFER_ITEMS: [&str; 2] = ["延迟更低", "播放更稳定"];
const DANMAKU_ITEMS: [&str; 3] = ["跟随设置", "开启", "关闭"];
const ENHANCE_ITEMS: [&str; 3] = ["无", "轻度", "高质量"];

impl Launcher {
    pub(super) fn players_page(
        &mut self,
        ui: &mut Ui,
        host: Rect,
        _context: &egui::Context,
        pal: &Pal,
    ) {
        let width = host.width();
        let pad = 16.0;
        let inner = (width - pad * 2.0).max(200.0);
        let mut children = child(ui, host);
        scroll_area(&mut children, "players-page", |ui| {
            let origin = ui.cursor().min;
            // ---- card 1: player path (fixed 170px card in the original) ----
            let card = Rect::from_min_size(origin, vec2(width, 170.0));
            rounded(ui.painter(), card, 10.0, pal.card);
            rounded_stroke(ui.painter(), card, 10.0, Stroke::new(1.0, pal.border));
            text_in(
                ui.painter(),
                Rect::from_min_size(card.min + vec2(pad, 12.0), vec2(inner, 26.0)),
                "播放器路径",
                font(14.0),
                pal.text_secondary,
                Align2::LEFT_CENTER,
            );
            let combo_width = 120.0_f32.min((inner / 3.0).max(90.0));
            let combo = Rect::from_min_size(card.min + vec2(pad, 46.0), vec2(combo_width, 28.0));
            let mut selected = match self.player_edit {
                Player::Mpv => 0,
                Player::PotPlayer => 1,
            };
            if dropdown(ui, combo, "player-edit", &mut selected, &["mpv", "PotPlayer"], pal).changed()
            {
                self.player_edit = if selected == 0 { Player::Mpv } else { Player::PotPlayer };
            }
            let pick = Rect::from_min_size(
                pos2(combo.right() + 10.0, card.top() + 44.0),
                vec2(140.0_f32.min((inner / 3.0).max(90.0)), 32.0),
            );
            if button(ui, pick, "选择程序…", Btn::Secondary, pal, true).clicked() {
                let tool = if self.player_edit == Player::Mpv {
                    ToolKind::Mpv
                } else {
                    ToolKind::PotPlayer
                };
                let current = if self.player_edit == Player::Mpv {
                    self.mpv_path.clone()
                } else {
                    self.potplayer_path.clone()
                };
                match crate::gui::file_dialog::pick_executable(tool, &current) {
                    Some(path) => {
                        let display = path.display().to_string();
                        if self.player_edit == Player::Mpv {
                            self.mpv_path = display.clone();
                        } else {
                            self.potplayer_path = display.clone();
                        }
                        self.set_status(format!("已选择 {}：{display}", tool.label()), false);
                    }
                    None => self.set_status("未选择文件（原生对话框取消或当前系统不支持）", false),
                }
            }
            let (path, ready) = match self.player_edit {
                Player::Mpv => (self.mpv_path.clone(), !self.mpv_path.trim().is_empty()),
                Player::PotPlayer => (self.potplayer_path.clone(), !self.potplayer_path.trim().is_empty()),
            };
            let path_text = if path.trim().is_empty() {
                format!(
                    "当前：未指定 {} 路径　·　点「选择程序…」指向你自己的 {}",
                    self.player_edit,
                    if self.player_edit == Player::Mpv { "mpv.exe" } else { "PotPlayerMini64.exe" }
                )
            } else {
                format!("当前：{path}　·　{}", if ready { "已就绪" } else { "路径不可用" })
            };
            let path_color = if path.trim().is_empty() {
                pal.danger
            } else if ready {
                pal.success
            } else {
                pal.danger
            };
            wrap_text(
                ui.painter(),
                Rect::from_min_size(card.min + vec2(pad, 84.0), vec2(inner, 40.0)),
                &path_text,
                font(11.0),
                path_color,
            );
            wrap_text(
                ui.painter(),
                Rect::from_min_size(card.min + vec2(pad, 132.0), vec2(inner, 26.0)),
                "这里选择要配置路径的程序；播放时可在收藏或最近播放中选择 mpv / PotPlayer。",
                font(12.0),
                pal.text_secondary,
            );
            // ---- card 2: the four per-play dropdowns ------------------------
            let prefs = Rect::from_min_size(origin + vec2(0.0, 184.0), vec2(width, 92.0));
            rounded(ui.painter(), prefs, 10.0, pal.card);
            rounded_stroke(ui.painter(), prefs, 10.0, Stroke::new(1.0, pal.border));
            let columns = if inner >= 760.0 {
                4
            } else if inner >= 600.0 {
                3
            } else {
                2
            };
            let gap = 16.0;
            let cell = (inner - gap * (columns as f32 - 1.0)) / columns as f32;
            let captions = [
                "画质 · 本次播放",
                "mpv 缓冲 · 本次播放",
                "弹幕 · 本次播放",
                "mpv 画质增强 · 保存偏好",
            ];
            let mut boxes: [Rect; 4] = [Rect::NOTHING; 4];
            for index in 0..4 {
                let column = index % columns;
                let row = index / columns;
                let left = pad + column as f32 * (cell + gap);
                let top = 12.0 + row as f32 * 76.0;
                text_in(
                    ui.painter(),
                    Rect::from_min_size(prefs.min + vec2(left, top), vec2(cell, 22.0)),
                    captions[index],
                    font(11.0),
                    pal.text_secondary,
                    Align2::LEFT_CENTER,
                );
                boxes[index] =
                    Rect::from_min_size(prefs.min + vec2(left, top + 26.0), vec2(cell, 27.0));
            }
            let mut quality = match self.quality {
                Quality::Best => 0,
                Quality::High => 1,
                Quality::Low => 2,
            };
            if dropdown(ui, boxes[0], "players-quality", &mut quality, &QUALITY_ITEMS, pal).changed() {
                self.quality = [Quality::Best, Quality::High, Quality::Low][quality];
            }
            let mut buffer = match self.playback_draft.buffer_mode {
                BufferMode::LowLatency => 0,
                BufferMode::Stable => 1,
            };
            if dropdown(ui, boxes[1], "players-buffer", &mut buffer, &BUFFER_ITEMS, pal).changed() {
                let mode = [BufferMode::LowLatency, BufferMode::Stable][buffer];
                if mode != self.playback_draft.buffer_mode {
                    self.playback_draft.buffer_mode = mode;
                    self.send_store(StoreAction::SavePlayback(self.playback_draft.clone()));
                    self.set_status("mpv 缓冲偏好已保存。", false);
                }
            }
            let mut danmaku = self.launch_danmaku as usize;
            if dropdown(ui, boxes[2], "players-danmaku", &mut danmaku, &DANMAKU_ITEMS, pal).changed()
            {
                self.launch_danmaku = danmaku.min(2) as u8;
            }
            let mut enhance = match self.playback_draft.enhancement {
                Enhancement::Off => 0,
                Enhancement::Light => 1,
                Enhancement::High => 2,
            };
            if dropdown(ui, boxes[3], "players-enhance", &mut enhance, &ENHANCE_ITEMS, pal).changed() {
                let value = [Enhancement::Off, Enhancement::Light, Enhancement::High][enhance];
                if value != self.playback_draft.enhancement {
                    self.playback_draft.enhancement = value;
                    self.send_store(StoreAction::SavePlayback(self.playback_draft.clone()));
                    self.set_status("mpv 画质增强已保存为偏好。", true == false);
                }
            }
            // ---- Rust-only advanced cards, collapsed by default -----------
            if !self.players_advanced_open {
                let row = Rect::from_min_size(origin + vec2(0.0, 290.0), vec2(width, 48.0));
                rounded(ui.painter(), row, 10.0, pal.card);
                rounded_stroke(ui.painter(), row, 10.0, Stroke::new(1.0, pal.border));
                text_in(
                    ui.painter(),
                    Rect::from_min_size(row.min + vec2(pad, 0.0), vec2(row.width() - 180.0, 48.0)),
                    "Rust 增强设置（解析工具路径 / 播放与恢复）",
                    font(12.0),
                    pal.text_secondary,
                    Align2::LEFT_CENTER,
                );
                let expand = Rect::from_min_size(
                    pos2(row.right() - pad - 96.0, row.top() + 8.0),
                    vec2(96.0, 32.0),
                );
                if button(ui, expand, "展开设置", Btn::Secondary, pal, true).clicked() {
                    self.players_advanced_open = true;
                }
                ui.allocate_space(vec2(width, 48.0 + 12.0));
                return;
            }
            let collapse = Rect::from_min_size(
                origin + vec2(width - pad - 96.0, 290.0 + 12.0),
                vec2(96.0, 26.0),
            );
            if button(ui, collapse, "收起", Btn::Secondary, pal, true).clicked() {
                self.players_advanced_open = false;
            }
            // ---- card 3: parsing tools (Rust-only capability, kept) --------
            let tools_card = Rect::from_min_size(origin + vec2(0.0, 290.0), vec2(width, 196.0));
            rounded(ui.painter(), tools_card, 10.0, pal.card);
            rounded_stroke(ui.painter(), tools_card, 10.0, Stroke::new(1.0, pal.border));
            text_in(
                ui.painter(),
                Rect::from_min_size(tools_card.min + vec2(pad, 12.0), vec2(inner, 22.0)),
                "解析工具与浏览器",
                font(14.0),
                pal.text_secondary,
                Align2::LEFT_CENTER,
            );
            hint(
                ui,
                Rect::from_min_size(tools_card.min + vec2(pad, 32.0), vec2(inner, 16.0)),
                "yt-dlp 用于抖音取流；浏览器用于独立窗口登录。留空会按程序目录与 PATH 查找。",
                pal,
            );
            let mut y = 58.0;
            for (label, kind) in [
                ("yt-dlp 可执行文件", Some(ToolKind::YtDlp)),
                ("Edge / Chrome 可执行文件", None),
            ] {
                let field = Rect::from_min_size(tools_card.min + vec2(pad, y), vec2(inner, 34.0));
                text_in(
                    ui.painter(),
                    Rect::from_min_size(field.min, vec2(150.0, 34.0)),
                    label,
                    font(11.0),
                    pal.text_secondary,
                    Align2::LEFT_CENTER,
                );
                let actions = if kind.is_some() { 88.0 + 8.0 + 96.0 + 16.0 } else { 0.0 };
                let input_rect = Rect::from_min_size(
                    pos2(field.left() + 160.0, field.top()),
                    vec2((inner - 160.0 - actions).max(60.0), 34.0),
                );
                match kind {
                    Some(tool) => {
                        let draft = &mut self.ytdlp_path;
                        input(ui, input_rect, draft, tool.hint(), pal);
                        let browse = Rect::from_min_size(
                            pos2(input_rect.right() + 8.0, field.top()),
                            vec2(88.0, 34.0),
                        );
                        if button(ui, browse, "选择程序…", Btn::Secondary, pal, true).clicked() {
                            match crate::gui::file_dialog::pick_executable(
                                ToolKind::YtDlp,
                                &self.ytdlp_path.clone(),
                            ) {
                                Some(path) => {
                                    let display = path.display().to_string();
                                    self.ytdlp_path = display.clone();
                                    self.set_status(format!("已选择 yt-dlp：{display}"), false);
                                }
                                None => {
                                    self.set_status("未选择文件（原生对话框取消或当前系统不支持）", false)
                                }
                            }
                        }
                        let detect = Rect::from_min_size(
                            pos2(browse.right() + 8.0, field.top()),
                            vec2(96.0, 34.0),
                        );
                        if button(ui, detect, "自动检测", Btn::Secondary, pal, true).clicked() {
                            match crate::gui::file_dialog::detect_tool(ToolKind::YtDlp) {
                                Some(path) => {
                                    let display = path.display().to_string();
                                    self.ytdlp_path = display.clone();
                                    self.set_status(format!("已找到 yt-dlp：{display}"), false);
                                }
                                None => self.set_status(
                                    "未自动找到 yt-dlp；请用“选择程序…”指定",
                                    true,
                                ),
                            }
                        }
                    }
                    None => {
                        input(ui, input_rect, &mut self.browser_path, "自动查找 Edge / Chrome", pal);
                    }
                }
                y += 40.0;
            }
            let save = Rect::from_min_size(tools_card.min + vec2(pad, 146.0), vec2(120.0, 32.0));
            if button(ui, save, "保存路径", Btn::Primary, pal, self.storage_pending == 0).clicked() {
                match crate::gui::file_dialog::validate_tool_paths(
                    &self.mpv_path,
                    &self.potplayer_path,
                    &self.ytdlp_path,
                ) {
                    Ok((mpv, potplayer, yt_dlp)) => {
                        let mut settings = self.snapshot.settings.clone();
                        settings.mpv = mpv;
                        settings.potplayer = potplayer;
                        settings.yt_dlp = yt_dlp;
                        self.send_store(StoreAction::SaveSettings(settings));
                    }
                    Err(error) => {
                        self.set_status(format!("播放器路径未保存：{error:#}"), true);
                    }
                }
            }
            // ---- card 4: playback toggles (kept from the Rust settings page)
            let toggles = Rect::from_min_size(origin + vec2(0.0, 500.0), vec2(width, 132.0));
            rounded(ui.painter(), toggles, 10.0, pal.card);
            rounded_stroke(ui.painter(), toggles, 10.0, Stroke::new(1.0, pal.border));
            text_in(
                ui.painter(),
                Rect::from_min_size(toggles.min + vec2(pad, 12.0), vec2(inner, 22.0)),
                "播放与恢复",
                font(14.0),
                pal.text_secondary,
                Align2::LEFT_CENTER,
            );
            let mut toggles_changed = false;
            let labels = [
                "未开播时等待，重开后继续尝试播放",
                "在会话页显示开播、下播与重播状态",
                "为B站 FLV 直播启用断流恢复与等待重开",
            ];
            for (row, label) in labels.into_iter().enumerate() {
                let rect = Rect::from_min_size(
                    toggles.min + vec2(pad, 40.0 + row as f32 * 26.0),
                    vec2(inner, 24.0),
                );
                let value = match row {
                    0 => &mut self.playback_draft.wait_for_live,
                    1 => &mut self.playback_draft.monitor_room_state,
                    _ => &mut self.playback_draft.relay_bili,
                };
                if checkbox(ui, rect, label, value, pal).changed() {
                    toggles_changed = true;
                }
            }
            if toggles_changed {
                self.send_store(StoreAction::SavePlayback(self.playback_draft.clone()));
            }
            ui.allocate_space(vec2(width, 640.0));
        });
    }
}
