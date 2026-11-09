//! 我的收藏 page: the original launch card, the 3-column room card grid and the
//! "正在观看 / 最近打开" column.
//!
//! Geometry follows LayoutWorkbenchFavorites / WorkbenchRoomBrowser from the C#
//! workbench. The launch card is a compressed version of the Rust preview's old
//! "开始看直播" page, so no separate play page is needed.

use super::*;
use crate::core::{BufferMode, Enhancement, Player, Quality, ScMode, Settings};
use crate::sessions::SessionPhase;

const CARD_HEIGHT: f32 = 160.0;
const CARD_COVER: f32 = 64.0;
const CARD_INSET: f32 = 2.0;
const CARD_GAP: f32 = 12.0;
const CARD_RADIUS: f32 = 11.0;
const CARD_INNER: f32 = 12.0;

const QUALITY_ITEMS: [&str; 3] = ["原画优先", "高画质", "流畅"];
const BUFFER_ITEMS: [&str; 2] = ["延迟更低", "播放更稳定"];
const DANMAKU_ITEMS: [&str; 3] = ["跟随设置", "开启", "关闭"];
const SC_ITEMS: [&str; 4] = ["完整卡片", "紧凑模式", "角标模式", "关闭"];
const ENHANCE_ITEMS: [&str; 3] = ["无", "轻度", "高质量"];

impl Launcher {
    /// Launch settings for this playback: the two "本次" overrides (buffer,
    /// danmaku on/off) are applied on top of the saved preferences.
    pub(crate) fn launch_settings(&self) -> Settings {
        let mut settings = self.snapshot.settings.clone();
        settings.playback.buffer_mode = self.playback_draft.buffer_mode;
        settings.playback.enhancement = self.playback_draft.enhancement;
        settings.overlay.sc_mode = self.overlay_draft.sc_mode;
        match self.launch_danmaku {
            1 => settings.overlay.enabled = true,
            2 => settings.overlay.enabled = false,
            _ => {}
        }
        settings
    }

    fn launch_with(&mut self, player: Player, context: &egui::Context) {
        match self.current_target() {
            Ok(target) => {
                let settings = self.launch_settings();
                self.start_playback_with(target, player, settings, context);
            }
            Err(error) => {
                let message =
                    room_input_hint(&self.room_input, self.platform, &format!("{error:#}"));
                self.set_status(message, true);
            }
        }
    }

    fn cancel_newest_launch(&mut self) {
        let cancellable = self
            .sessions
            .snapshots()
            .iter()
            .find(|session| session.phase.can_cancel())
            .map(|session| session.id.clone());
        if let Some(id) = cancellable {
            match self.sessions.cancel_start(&id) {
                Ok(()) => self.set_status("正在取消启动…", false),
                Err(error) => self.set_status(error.to_string(), true),
            }
        } else {
            self.set_status("当前没有可取消的启动。", false);
        }
    }

    /// "格式说明" dialog: the full room-input help, opened from the launch card.
    /// A centred dialog is used because the footer status line was both far from
    /// the click target and identical to text the user may already have seen.
    pub(super) fn format_help_dialog(&mut self, context: &egui::Context) {
        if !self.format_help_open {
            return;
        }
        let mut close = false;
        egui::Window::new("房间输入格式说明")
            .id(egui::Id::new("favorites-format-help"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .default_width(560.0)
            .show(context, |ui| {
                ui.set_max_width(560.0);
                ui.label(RichText::new(room_format_help_text()).size(13.0));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("关闭").clicked() {
                        close = true;
                    }
                });
                if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
            });
        if close {
            self.format_help_open = false;
        }
    }

    pub(super) fn favorites_page(
        &mut self,
        ui: &mut Ui,
        host: Rect,
        context: &egui::Context,
        pal: &Pal,
    ) {
        // Deletion is a two-step action: the dialog defaults to cancel (Escape and
        // the first button both cancel) and a successful delete leaves a short
        // undo entry instead of making the user re-enter the room.
        if let Some(entry) = self.favorite_delete_confirm.clone() {
            let mut cancel = false;
            egui::Window::new("删除收藏确认")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(context, |ui| {
                    ui.label(format!(
                        "确认删除“{}”？此操作不会停止正在播放的会话。",
                        entry.name
                    ));
                    let escape = ui.input(|input| input.key_pressed(egui::Key::Escape));
                    ui.horizontal(|ui| {
                        if ui.button("取消（默认）").clicked() {
                            cancel = true;
                        }
                        if ui.button("确认删除").clicked() {
                            self.send_store(StoreAction::RemoveFavorite(
                                entry.target.platform,
                                entry.target.room.clone(),
                            ));
                            self.favorite_undo =
                                Some((entry.clone(), std::time::Instant::now()));
                            self.favorite_selected = None;
                            cancel = true;
                        }
                    });
                    if escape {
                        cancel = true;
                    }
                });
            if cancel {
                self.favorite_delete_confirm = None;
            }
        }
        if let Some((entry, removed_at)) = self.favorite_undo.clone() {
            if removed_at.elapsed() < std::time::Duration::from_secs(20) {
                egui::Window::new("已删除收藏")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::RIGHT_BOTTOM, egui::Vec2::new(-18.0, -18.0))
                    .show(context, |ui| {
                        ui.label(format!("已删除“{}”。", entry.name));
                        ui.horizontal(|ui| {
                            if ui.button("撤销删除").clicked() {
                                self.send_store(StoreAction::AddFavorite(
                                    entry.name.clone(),
                                    entry.target.clone(),
                                ));
                                self.favorite_undo = None;
                                self.set_status("已撤销删除并恢复收藏。", false);
                            }
                            if ui.button("关闭").clicked() {
                                self.favorite_undo = None;
                            }
                        });
                    });
                // Keep the undo window's auto-expiry visible without a busy loop.
                context.request_repaint_after(std::time::Duration::from_millis(500));
            } else {
                self.favorite_undo = None;
            }
        }

        let width = host.width();
        let height = host.height();
        let pad = 16.0;
        let gap = 8.0;
        let inner = width - pad * 2.0;
        let mut y = 8.0;
        // ---- launch card -------------------------------------------------
        let tab_gap = 6.0;
        let tab_width = 72.0_f32.min(((inner - 120.0) / 4.0).max(46.0));
        let launch_origin = host.min;
        // The card is taller than the first rows, so its background is painted
        // first: the launch card height is deterministic (copied from the C#
        // LayoutWorkbenchFavorites maths) and every child paints on top.
        let launch_height = launch_card_height(inner, compact_guess(inner));
        let launch_card = Rect::from_min_size(launch_origin, vec2(width, launch_height));
        rounded(ui.painter(), launch_card, 10.0, pal.card);
        rounded_stroke(
            ui.painter(),
            launch_card,
            10.0,
            Stroke::new(1.0, pal.border),
        );
        let strip_rect = Rect::from_min_size(
            launch_origin + vec2(pad - 3.0, y - 3.0),
            vec2(4.0 * tab_width + 3.0 * tab_gap + 6.0, 36.0),
        );
        rounded(ui.painter(), strip_rect, 9.0, pal.hover);
        let mut tabs_x = pad;
        for platform in Platform::ALL {
            let pill_rect =
                Rect::from_min_size(launch_origin + vec2(tabs_x, y), vec2(tab_width, 30.0));
            if pill(
                ui,
                pill_rect,
                platform.label(),
                self.platform == platform,
                pal,
                true,
            )
            .clicked()
            {
                self.platform = platform;
            }
            tabs_x += tab_width + tab_gap;
        }
        let help_rect = Rect::from_min_size(
            launch_origin + vec2(width - pad - 88.0, y),
            vec2(88.0, 30.0),
        );
        // The footer status line is easy to miss from the launch card, so a
        // click opens the full help dialog instead of only writing one line.
        if link(ui, help_rect, "格式说明", pal)
            .on_hover_text("查看支持的房间号 / 链接 / 平台:房间写法")
            .clicked()
        {
            self.format_help_open = true;
        }
        y += 30.0 + 8.0;
        let compact = inner < 700.0;
        let mut input_rect;
        let mut mpv_rect;
        let mut pot_rect;
        let mut cancel_rect;
        if !compact {
            let cancel_width = 72.0;
            let pot_width = 160.0;
            let mpv_width = 155.0;
            let input_width = (inner - mpv_width - pot_width - cancel_width - gap * 3.0).max(120.0);
            input_rect = Rect::from_min_size(launch_origin + vec2(pad, y), vec2(input_width, 38.0));
            mpv_rect = Rect::from_min_size(
                pos2(input_rect.right() + gap, input_rect.top()),
                vec2(mpv_width, 38.0),
            );
            pot_rect = Rect::from_min_size(
                pos2(mpv_rect.right() + gap, input_rect.top()),
                vec2(pot_width, 38.0),
            );
            cancel_rect = Rect::from_min_size(
                pos2(pot_rect.right() + gap, input_rect.top()),
                vec2((width - pad - pot_rect.right() - gap).max(56.0), 38.0),
            );
            y += 38.0 + 10.0;
        } else {
            input_rect = Rect::from_min_size(launch_origin + vec2(pad, y), vec2(inner, 36.0));
            y += 36.0 + 6.0;
            let cancel_width = 56.0;
            let play_width = ((inner - gap * 2.0 - cancel_width) / 2.0).max(110.0);
            mpv_rect = Rect::from_min_size(launch_origin + vec2(pad, y), vec2(play_width, 32.0));
            pot_rect = Rect::from_min_size(
                pos2(mpv_rect.right() + gap, mpv_rect.top()),
                vec2(play_width, 32.0),
            );
            cancel_rect = Rect::from_min_size(
                pos2(pot_rect.right() + gap, mpv_rect.top()),
                vec2((width - pad - pot_rect.right() - gap).max(48.0), 32.0),
            );
            y += 32.0 + 10.0;
        }
        let input_response = input(
            ui,
            input_rect,
            &mut self.room_input,
            "输入房间号，或粘贴官方直播间链接",
            pal,
        );
        if input_response.changed()
            && let Some(platform) = detected_platform(self.room_input.as_str())
        {
            self.platform = platform;
        }
        let labels = ["画质：", "mpv 缓冲：", "弹幕：", "SC：", "画质增强"];
        let scopes = ["本次", "本次", "本次", "保存 / 实时", "保存 / mpv"];
        let columns = if inner >= 680.0 {
            5
        } else if inner >= 420.0 {
            3
        } else {
            2
        };
        let column_gap = 10.0;
        let cell = (inner - column_gap * (columns as f32 - 1.0)) / columns as f32;
        let label_height = 14.0;
        let combo_height = 27.0;
        let mut combos: Vec<(Rect, Rect, Rect)> = Vec::new();
        for index in 0..5 {
            let row = index / columns;
            let column = index % columns;
            let left = pad + column as f32 * (cell + column_gap);
            let top = y + row as f32 * (label_height + 3.0 + combo_height + 8.0);
            let scope_font = font(10.0);
            let scope_width =
                (cell / 2.0).min(measure(ui.painter(), scopes[index], &scope_font) + 4.0);
            let scope_rect = Rect::from_min_size(
                launch_origin + vec2(left + cell - scope_width, top),
                vec2(scope_width, label_height),
            );
            let label_rect = Rect::from_min_size(
                launch_origin + vec2(left, top),
                vec2((cell - scope_width - 4.0).max(40.0), label_height),
            );
            field_label(ui, label_rect, labels[index], None, pal);
            text_in(
                ui.painter(),
                scope_rect,
                scopes[index],
                scope_font,
                pal.text_secondary,
                Align2::RIGHT_CENTER,
            );
            let combo_rect = Rect::from_min_size(
                launch_origin + vec2(left, top + label_height + 3.0),
                vec2(cell, combo_height),
            );
            combos.push((label_rect, combo_rect, scope_rect));
        }
        let rows = 5_usize.div_ceil(columns);
        y += rows as f32 * (label_height + 3.0 + combo_height) + (rows as f32 - 1.0) * 8.0 + 6.0;
        let rows_height = y;
        let _ = rows_height;
        // Quality / buffer / danmaku / SC / enhancement dropdowns.
        let mut quality = match self.quality {
            Quality::Best => 0,
            Quality::High => 1,
            Quality::Low => 2,
        };
        if dropdown(
            ui,
            combos[0].1,
            "launch-quality",
            &mut quality,
            &QUALITY_ITEMS,
            pal,
        )
        .changed()
        {
            self.quality = [Quality::Best, Quality::High, Quality::Low][quality];
        }
        let mut buffer = match self.playback_draft.buffer_mode {
            BufferMode::LowLatency => 0,
            BufferMode::Stable => 1,
        };
        if dropdown(
            ui,
            combos[1].1,
            "launch-buffer",
            &mut buffer,
            &BUFFER_ITEMS,
            pal,
        )
        .changed()
        {
            self.playback_draft.buffer_mode = [BufferMode::LowLatency, BufferMode::Stable][buffer];
        }
        let launch_danmaku_text = ["跟随设置", "开启", "关闭"];
        let mut danmaku = self.launch_danmaku as usize;
        if dropdown(
            ui,
            combos[2].1,
            "launch-danmaku",
            &mut danmaku,
            &launch_danmaku_text,
            pal,
        )
        .changed()
        {
            self.launch_danmaku = danmaku.min(2) as u8;
        }
        let sc_items = SC_ITEMS;
        let mut sc = match self.overlay_draft.sc_mode {
            ScMode::Full => 0,
            ScMode::Compact => 1,
            ScMode::Badge => 2,
            ScMode::Off => 3,
        };
        if dropdown(ui, combos[3].1, "launch-sc", &mut sc, &sc_items, pal).changed() {
            let mode = [ScMode::Full, ScMode::Compact, ScMode::Badge, ScMode::Off][sc];
            if mode != self.overlay_draft.sc_mode {
                self.overlay_draft.sc_mode = mode;
                self.send_store(StoreAction::SaveOverlay(self.overlay_draft.clone()));
                self.set_status("SC 显示模式已保存为偏好。", false);
            }
        }
        let mut enhance = match self.playback_draft.enhancement {
            Enhancement::Off => 0,
            Enhancement::Light => 1,
            Enhancement::High => 2,
        };
        if dropdown(
            ui,
            combos[4].1,
            "launch-enhance",
            &mut enhance,
            &ENHANCE_ITEMS,
            pal,
        )
        .changed()
        {
            let value = [Enhancement::Off, Enhancement::Light, Enhancement::High][enhance];
            if value != self.playback_draft.enhancement {
                self.playback_draft.enhancement = value;
                self.send_store(StoreAction::SavePlayback(self.playback_draft.clone()));
                self.set_status("mpv 画质增强已保存为偏好。", false);
            }
        }
        let note_height = 24.0_f32.max(18.0);
        let hint_rect = Rect::from_min_size(
            launch_origin + vec2(pad, y),
            vec2((inner - 140.0).max(100.0), note_height),
        );
        hint(
            ui,
            hint_rect,
            "画质、缓冲、弹幕用于本次播放；SC 与画质增强保存为偏好。",
            pal,
        );
        let settings_rect = Rect::from_min_size(
            launch_origin + vec2(width - pad - 104.0, y + (note_height - 24.0) / 2.0),
            vec2(104.0, 24.0),
        );
        if link(ui, settings_rect, "弹幕设置…", pal).clicked() {
            self.show_page(Page::Danmaku);
        }
        let launch_bottom = y + note_height + 8.0;
        debug_assert!((launch_bottom - launch_height).abs() < 0.5);
        // Launch buttons (painted after the card so they stay visible).
        if button(
            ui,
            mpv_rect,
            "用 mpv 打开（回车）",
            Btn::Primary,
            pal,
            self.storage_pending == 0,
        )
        .clicked()
        {
            self.launch_with(Player::Mpv, context);
        }
        if button(
            ui,
            pot_rect,
            "用 PotPlayer 打开",
            Btn::Secondary,
            pal,
            self.storage_pending == 0,
        )
        .clicked()
        {
            self.launch_with(Player::PotPlayer, context);
        }
        if button(
            ui,
            cancel_rect,
            "取消启动",
            Btn::Secondary,
            pal,
            self.playing,
        )
        .clicked()
        {
            self.cancel_newest_launch();
        }
        // ---- favourites section header ----------------------------------
        y = launch_bottom + 12.0;
        let mut platform_favorites: Vec<(usize, String, Target)> = self
            .snapshot
            .favorites
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.target.platform == self.platform)
            .map(|(index, entry)| (index, entry.name.clone(), entry.target.clone()))
            .collect();
        // Legacy FavoriteRank: 正在直播 → 重播 → 未开播 → 状态未知；同档按名称稳定排序。
        // Badge results repaint the page, so this order updates as checks return.
        platform_favorites.sort_by(|left, right| {
            let rank = |target: &Target| {
                self.badges
                    .get(&crate::gui::badges::room_key(target))
                    .map_or(3, |badge| badge.favorite_rank())
            };
            rank(&left.2)
                .cmp(&rank(&right.2))
                .then_with(|| left.1.cmp(&right.1))
        });
        let heading = format!(
            "{} 收藏 {} 个 · 本地收藏",
            self.platform.label(),
            platform_favorites.len()
        );
        // Four toolbar buttons need a compact row on the narrow window, so the
        // action row geometry is computed before the heading is drawn.
        let tight = width < 760.0;
        let action_width = if tight { 80.0 } else { 104.0 };
        let action_gap = 6.0;
        let action_span = action_width * 4.0 + action_gap * 3.0;
        let first_action = width - action_span;
        section_header(
            ui,
            Rect::from_min_size(
                host.min + vec2(0.0, y + 5.0),
                vec2((first_action - 10.0).max(80.0), 20.0),
            ),
            &heading,
            pal,
        );
        // Import / export sit next to the favourite actions like the original
        // data page; both use the native file dialogs.
        let import_rect =
            Rect::from_min_size(host.min + vec2(first_action, y), vec2(action_width, 30.0));
        if button(
            ui,
            import_rect,
            if tight { "导入" } else { "导入收藏" },
            Btn::Secondary,
            pal,
            self.storage_pending == 0,
        )
        .clicked()
        {
            self.import_favorites_dialog();
        }
        let export_rect = Rect::from_min_size(
            host.min + vec2(first_action + action_width + action_gap, y),
            vec2(action_width, 30.0),
        );
        if button(
            ui,
            export_rect,
            if tight { "导出" } else { "导出收藏" },
            Btn::Secondary,
            pal,
            !self.snapshot.favorites.is_empty(),
        )
        .clicked()
        {
            self.export_favorites_dialog();
        }
        let add_rect = Rect::from_min_size(
            host.min + vec2(first_action + (action_width + action_gap) * 2.0, y),
            vec2(action_width, 30.0),
        );
        if button(
            ui,
            add_rect,
            "加入收藏",
            Btn::Secondary,
            pal,
            self.storage_pending == 0,
        )
        .clicked()
        {
            self.add_current_favorite();
        }
        let del_rect = Rect::from_min_size(
            host.min + vec2(first_action + (action_width + action_gap) * 3.0, y),
            vec2(action_width, 30.0),
        );
        let delete_enabled = self
            .favorite_selected
            .filter(|index| *index < self.snapshot.favorites.len())
            .is_some();
        if button(
            ui,
            del_rect,
            "删除收藏",
            Btn::Secondary,
            pal,
            delete_enabled && self.storage_pending == 0,
        )
        .clicked()
            && let Some(index) = self.favorite_selected
            && let Some(entry) = self.snapshot.favorites.get(index).cloned()
        {
            self.favorite_delete_confirm = Some(entry);
        }
        y += 30.0 + 6.0;
        // ---- grid + home column -----------------------------------------
        let wide_tail = inner >= 960.0;
        let home_width = if wide_tail {
            320.0_f32.min((inner / 4.0).max(280.0))
        } else {
            width
        };
        let browser_width = if wide_tail {
            width - home_width - 16.0
        } else {
            width
        };
        let hint_line = 18.0;
        let two_rows = CARD_INSET * 2.0 + CARD_HEIGHT * 2.0 + CARD_GAP;
        let mut browser_height = CARD_INSET * 2.0 + CARD_HEIGHT;
        let available = height - y - 4.0 - hint_line - 4.0;
        if available >= two_rows {
            browser_height = available;
        }
        let browser_rect =
            Rect::from_min_size(host.min + vec2(0.0, y), vec2(browser_width, browser_height));
        self.favorite_grid(ui, browser_rect, context, pal, &platform_favorites);
        let hint_top = (browser_rect.bottom() + 4.0)
            .min(host.bottom() - hint_line - 2.0)
            .max(host.top());
        let favorites_hint = Rect::from_min_size(
            host.min + vec2(0.0, hint_top - host.min.y),
            vec2(browser_width, hint_line),
        );
        hint(
            ui,
            favorites_hint,
            "选择卡片回填房间；双击或回车用 mpv，Shift+回车用 PotPlayer。",
            pal,
        );
        let home_top = if wide_tail {
            y
        } else {
            favorites_hint.bottom() + 10.0 - host.min.y
        };
        let home_rect = Rect::from_min_size(
            host.min + vec2(if wide_tail { width - home_width } else { 0.0 }, home_top),
            vec2(home_width, if wide_tail { browser_height } else { 190.0 }),
        );
        self.home_band(ui, home_rect, context, pal);
        // ---- keyboard shortcuts -----------------------------------------
        // The original form uses an AcceptButton: Enter starts mpv and
        // Shift+Enter starts PotPlayer even while the room input has focus.
        // Arrow keys move the card selection when the user is not typing.
        let (enter, shift, up, down, left, right, escape) = ui.input(|input| {
            (
                input.key_pressed(egui::Key::Enter),
                input.modifiers.shift,
                input.key_pressed(egui::Key::ArrowUp),
                input.key_pressed(egui::Key::ArrowDown),
                input.key_pressed(egui::Key::ArrowLeft),
                input.key_pressed(egui::Key::ArrowRight),
                input.key_pressed(egui::Key::Escape),
            )
        });
        let popup_open = ui.memory(|memory| memory.any_popup_open());
        // The delete confirmation must swallow Enter/arrow shortcuts: launching a
        // player while answering a destructive dialog is never intended.
        let confirm_open = self.favorite_delete_confirm.is_some();
        if escape && !confirm_open {
            self.favorite_selected = None;
        }
        if enter && !popup_open && !confirm_open {
            if shift {
                self.launch_with(Player::PotPlayer, context);
            } else {
                self.launch_with(Player::Mpv, context);
            }
        }
        if !ui.ctx().wants_keyboard_input()
            && !popup_open
            && !confirm_open
            && !platform_favorites.is_empty()
        {
            let columns = if inner >= 660.0 {
                3
            } else if inner >= 420.0 {
                2
            } else {
                1
            };
            let current = self.favorite_selected.and_then(|selected| {
                platform_favorites
                    .iter()
                    .position(|(index, _, _)| *index == selected)
            });
            let next = if up {
                current.map(|position| position.saturating_sub(columns))
            } else if down {
                current.map(|position| (position + columns).min(platform_favorites.len() - 1))
            } else if left {
                current.map(|position| position.saturating_sub(1))
            } else if right {
                current.map(|position| (position + 1).min(platform_favorites.len() - 1))
            } else {
                None
            };
            if let Some(position) = next
                && let Some((index, name, target)) = platform_favorites.get(position).cloned()
            {
                self.favorite_selected = Some(index);
                self.fill_room(&name, &target);
            }
        }
    }

    /// 3-column card grid (WorkbenchRoomBrowser with Cards=true).
    fn favorite_grid(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        context: &egui::Context,
        pal: &Pal,
        favorites: &[(usize, String, Target)],
    ) {
        let painter = ui.painter();
        rounded(painter, rect, 0.0, pal.surface);
        let mut area = child(ui, rect);
        let mut action: Option<(usize, bool)> = None; // (index, shift)
        let mut hovered_text: Option<&'static str> = None;
        scroll_area(&mut area, "favorites-grid", |ui| {
            let client_width = ui.available_width();
            let inset = CARD_INSET;
            let gap = CARD_GAP;
            let available = (client_width - inset * 2.0).max(1.0);
            let columns = if available >= 660.0 {
                3
            } else if available >= 420.0 {
                2
            } else {
                1
            };
            let card_width = ((available - (columns as f32 - 1.0) * gap) / columns as f32).max(1.0);
            let rows = favorites.len().div_ceil(columns);
            let content_height = if rows == 0 {
                0.0
            } else {
                inset * 2.0 + rows as f32 * CARD_HEIGHT + (rows as f32 - 1.0) * gap
            };
            let origin = ui.cursor().min;
            if favorites.is_empty() {
                empty_state(
                    ui,
                    Rect::from_min_size(origin, vec2(client_width, 120.0)),
                    "还没有收藏",
                    "输入房间号后可加入收藏。",
                    pal,
                );
            }
            for (position, (favorite_index, name, target)) in favorites.iter().enumerate() {
                let row = position / columns;
                let column = position % columns;
                let card = Rect::from_min_size(
                    origin
                        + vec2(
                            inset + column as f32 * (card_width + gap),
                            inset + row as f32 * (CARD_HEIGHT + gap),
                        ),
                    vec2(card_width, CARD_HEIGHT),
                );
                let id = ui.id().with(("favorite-card", *favorite_index));
                let response = ui.interact(card, id, Sense::click());
                let selected = self.favorite_selected == Some(*favorite_index);
                let badge = self.badge_label(target);
                let texture = self.avatar_texture(ui.ctx(), target.platform, &target.room);
                self.paint_room_card(
                    ui,
                    card,
                    name,
                    target,
                    badge,
                    texture,
                    selected,
                    response.hovered(),
                    pal,
                );
                // Action buttons inside the card.
                let button_width = ((card_width - CARD_INNER * 2.0 - 8.0) / 2.0).max(1.0);
                let button_top = card.bottom() - CARD_INNER - 30.0;
                let mpv = Rect::from_min_size(
                    pos2(card.left() + CARD_INNER, button_top),
                    vec2(button_width, 30.0),
                );
                let pot = Rect::from_min_size(
                    pos2(mpv.right() + 8.0, button_top),
                    vec2(button_width, 30.0),
                );
                let choose = Rect::from_min_size(
                    pos2(
                        card.right() - CARD_INNER - 52.0,
                        card.top() + CARD_COVER - 6.0 - 24.0,
                    ),
                    vec2(52.0, 24.0),
                );
                let mpv_hot = ui
                    .interact(mpv, id.with("mpv"), Sense::click())
                    .on_hover_text("用 mpv 打开")
                    .hovered();
                let pot_hot = ui
                    .interact(pot, id.with("pot"), Sense::click())
                    .on_hover_text("用 PotPlayer 打开")
                    .hovered();
                let choose_hot = ui
                    .interact(choose, id.with("choose"), Sense::click())
                    .hovered();
                self.paint_card_button(ui, mpv, "mpv", true, pal, selected, mpv_hot);
                self.paint_card_button(ui, pot, "PotPlayer", false, pal, selected, pot_hot);
                // The 选择 pill paints above the state pill slot.
                let choose_fill = if selected { pal.selection } else { pal.card };
                rounded(ui.painter(), choose, 6.0, choose_fill);
                rounded_stroke(
                    ui.painter(),
                    choose,
                    6.0,
                    Stroke::new(
                        1.0,
                        if selected {
                            pal.selection_border
                        } else {
                            pal.border
                        },
                    ),
                );
                text_in(
                    ui.painter(),
                    choose,
                    "选择",
                    font(11.0),
                    if selected {
                        pal.selection_text
                    } else {
                        pal.text
                    },
                    Align2::CENTER_CENTER,
                );
                if choose_hot {
                    hovered_text = Some("填入房间号");
                }
                if response.double_clicked() {
                    action = Some((*favorite_index, false));
                } else if response.clicked() {
                    self.favorite_selected = Some(*favorite_index);
                    self.fill_room(name, target);
                }
                if ui
                    .interact(mpv, id.with("mpv"), Sense::click())
                    .double_clicked()
                    || ui.interact(mpv, id.with("mpv"), Sense::click()).clicked()
                {
                    action = Some((*favorite_index, false));
                }
                if ui.interact(pot, id.with("pot"), Sense::click()).clicked() {
                    action = Some((*favorite_index, true));
                }
                if ui
                    .interact(choose, id.with("choose"), Sense::click())
                    .clicked()
                {
                    self.favorite_selected = Some(*favorite_index);
                    self.fill_room(name, target);
                }
            }
            if content_height > 0.0 {
                ui.allocate_space(vec2(client_width, content_height));
            }
        });
        let _ = hovered_text;
        if let Some((index, potplayer)) = action
            && let Some((_, _, target)) = favorites.iter().find(|(i, _, _)| *i == index).cloned()
        {
            self.favorite_selected = Some(index);
            let settings = self.launch_settings();
            let player = if potplayer {
                Player::PotPlayer
            } else {
                Player::Mpv
            };
            self.start_playback_with(target, player, settings, context);
        }
    }

    fn paint_room_card(
        &self,
        ui: &mut Ui,
        card: Rect,
        name: &str,
        target: &Target,
        badge: Option<&str>,
        texture: Option<egui::TextureHandle>,
        selected: bool,
        hovered: bool,
        pal: &Pal,
    ) {
        let painter = ui.painter();
        let box_rect = Rect::from_min_size(card.min, vec2(card.width() - 1.0, card.height() - 1.0));
        rounded(
            painter,
            box_rect,
            CARD_RADIUS,
            if selected {
                pal.selection_soft
            } else {
                pal.card
            },
        );
        rounded_stroke(
            painter,
            box_rect,
            CARD_RADIUS,
            Stroke::new(
                1.0,
                if hovered {
                    pal.border_hover
                } else {
                    pal.border
                },
            ),
        );
        // Cover gradient: blend(accent, card, .55) -> blend(accent, card, .88).
        let accent = pal.platform(target.platform);
        let tone = if selected {
            pal.selection_soft
        } else {
            pal.card
        };
        let cover = Rect::from_min_size(
            card.min + vec2(1.0, 1.0),
            vec2(card.width() - 2.0, CARD_COVER),
        );
        gradient_cover(
            painter,
            cover,
            CARD_RADIUS,
            blend(accent, tone, 0.55),
            blend(accent, tone, 0.88),
        );
        // Avatar 36px at (14,14).
        let avatar_rect = Rect::from_min_size(card.min + vec2(14.0, 14.0), Vec2::splat(36.0));
        avatar(painter, avatar_rect, texture.as_ref(), name, pal);
        // Live state pill: 22px high, right aligned, dot + label.
        if let Some(state) = badge.map(str::trim).filter(|state| !state.is_empty()) {
            let live = matches!(state, "[播]" | "直播中");
            let text = match state {
                "[播]" => "直播中",
                "[休]" => "已下播",
                "[重播]" => "重播中",
                other => other,
            };
            let state_font = font(10.5);
            let text_width = measure(painter, text, &state_font);
            let pill_width = (text_width + 24.0)
                .max(52.0)
                .min((card.width() - CARD_INNER * 2.0 - 52.0).max(40.0));
            let state_rect = Rect::from_min_size(
                pos2(card.right() - 9.0 - pill_width, card.top() + 9.0),
                vec2(pill_width, 22.0),
            );
            rounded(painter, state_rect, 5.0, pal.card);
            let color = if live {
                pal.success
            } else {
                pal.text_secondary
            };
            painter.circle_filled(
                pos2(state_rect.left() + 10.5, state_rect.center().y),
                2.5,
                color,
            );
            text_in(
                painter,
                Rect::from_min_max(
                    pos2(state_rect.left() + 16.0, state_rect.top()),
                    state_rect.max,
                ),
                text,
                state_font,
                color,
                Align2::LEFT_CENTER,
            );
        }
        if selected {
            let badge_rect = Rect::from_min_size(card.min + vec2(40.0, 42.0), Vec2::splat(14.0));
            painter.circle_filled(badge_rect.center(), 7.0, pal.selection);
            draw_glyph(
                painter,
                badge_rect.shrink(2.5),
                Glyph::Check,
                pal.selection_text,
                1.6,
            );
        }
        let body_top = card.top() + CARD_COVER;
        text_in(
            painter,
            Rect::from_min_size(
                pos2(card.left() + CARD_INNER, body_top + 8.0),
                vec2((card.width() - CARD_INNER * 2.0).max(0.0), 20.0),
            ),
            name,
            bold(13.0),
            pal.text,
            Align2::LEFT_CENTER,
        );
        text_in(
            painter,
            Rect::from_min_size(
                pos2(card.left() + CARD_INNER, body_top + 29.0),
                vec2((card.width() - CARD_INNER * 2.0).max(0.0), 17.0),
            ),
            format!("{} · {}", target.platform.label(), target.room),
            font(11.0),
            pal.text_secondary,
            Align2::LEFT_CENTER,
        );
        // Paint the selection ring last: the cover gradient and the inner pills
        // otherwise cover its top/right halves (the incomplete orange frame).
        if selected {
            rounded_stroke(
                painter,
                Rect::from_min_size(card.min - Vec2::splat(1.0), card.size() + Vec2::splat(2.0)),
                CARD_RADIUS + 1.0,
                Stroke::new(2.0, pal.selection_border),
            );
        }
    }

    fn paint_card_button(
        &self,
        ui: &mut Ui,
        rect: Rect,
        text: &str,
        play_icon: bool,
        pal: &Pal,
        _selected: bool,
        hovered: bool,
    ) {
        let painter = ui.painter();
        let fill = if hovered { pal.hover } else { pal.card };
        rounded(painter, rect, 6.0, fill);
        rounded_stroke(
            painter,
            rect,
            6.0,
            Stroke::new(
                1.0,
                if hovered {
                    pal.border_hover
                } else {
                    pal.border
                },
            ),
        );
        // The original card open buttons keep the normal text colour even when
        // the card itself is selected (only the card fill/border/check change).
        let color = pal.text;
        if play_icon {
            let icon = Rect::from_center_size(
                pos2(
                    rect.center().x - measure(painter, text, &font(11.0)) / 2.0 - 10.0,
                    rect.center().y,
                ),
                Vec2::splat(9.0),
            );
            draw_glyph(painter, icon, Glyph::Play, color, 1.0);
            text_in(
                painter,
                rect.translate(vec2(5.0, 0.0)),
                text,
                font(11.0),
                color,
                Align2::CENTER_CENTER,
            );
        } else {
            text_in(
                painter,
                rect,
                text,
                font(11.0),
                color,
                Align2::CENTER_CENTER,
            );
        }
    }

    /// "正在观看 / 最近打开": header row with the all-sessions link and a scroll
    /// list of session rows (original LayoutWorkbenchHomeBand).
    fn home_band(&mut self, ui: &mut Ui, rect: Rect, context: &egui::Context, pal: &Pal) {
        let sessions_width = 100.0_f32.min((rect.width() / 3.0).max(76.0));
        draw_glyph(
            ui.painter(),
            Rect::from_min_size(rect.min + vec2(0.0, 3.0), Vec2::splat(16.0)),
            Glyph::MonitorPlay,
            pal.text_secondary,
            1.2,
        );
        text_in(
            ui.painter(),
            Rect::from_min_size(
                rect.min + vec2(22.0, 0.0),
                vec2((rect.width() - sessions_width - 26.0).max(80.0), 20.0),
            ),
            "正在观看 / 最近打开",
            bold(14.0),
            pal.text,
            Align2::LEFT_CENTER,
        );
        let all_rect = Rect::from_min_size(
            rect.min + vec2(rect.width() - sessions_width, -2.0),
            vec2(sessions_width, 24.0),
        );
        if link(ui, all_rect, "全部会话 →", pal).clicked() {
            self.show_page(Page::Sessions);
        }
        let card_top = rect.top() + 25.0;
        let compact_card = rect.width() < 420.0;
        let card_height = if compact_card { 94.0 } else { 64.0 };
        let list_rect = Rect::from_min_size(
            pos2(rect.left(), card_top),
            vec2(rect.width(), (rect.height() - 25.0).max(card_height)),
        );
        let mut reopen_session: Option<String> = None;
        let mut reopen_history: Option<Target> = None;
        let snapshots = self.sessions.snapshots();
        let mut area = child(ui, list_rect);
        scroll_area(&mut area, "home-session-list", |ui| {
            let mut rows: Vec<(String, String, String, bool, Option<String>, Option<Target>)> =
                Vec::new();
            // sessions.snapshots() is newest-first already; reversing it pushed the
            // session the user is watching to the bottom of an overflowing list
            // (user report: "正在看的不在右边显示"). Keep the source order, exactly
            // like the legacy list (newest/active first).
            let mut seen_rooms: Vec<String> = Vec::new();
            for session in snapshots.iter() {
                // One row per room, newest session wins — the legacy list built a
                // seen-set from Platform:Room before appending history, so opening
                // the same room twice never duplicated a row.
                let key = format!("{}:{}", session.target.platform, session.target.room);
                if seen_rooms.iter().any(|seen| seen == &key) {
                    continue;
                }
                seen_rooms.push(key);
                let launch = self.session_launch.get(&session.id);
                let player = launch.map(|launch| launch.player).unwrap_or(session.player);
                let phase = session.phase;
                rows.push((
                    session.title.clone(),
                    format!(
                        "{} · {} · 播放器默认",
                        player,
                        quality_label(session.target.quality)
                    ),
                    phase.label().to_owned(),
                    phase == SessionPhase::Playing || phase.can_stop(),
                    Some(session.id.clone()),
                    None,
                ));
            }
            for entry in self.snapshot.history.iter() {
                // Deduplicate by room identity, not by display name: two rooms can
                // share the same title and the name check could also let the same
                // room appear twice (legacy used Platform:Room).
                let key = format!("{}:{}", entry.target.platform, entry.target.room);
                if seen_rooms.iter().any(|seen| seen == &key) {
                    continue;
                }
                seen_rooms.push(key);
                rows.push((
                    entry.name.clone(),
                    format!("{} · {}", entry.target.platform.label(), entry.target.room),
                    format!("最近 · {}", history_time(entry)),
                    false,
                    None,
                    Some(entry.target.clone()),
                ));
            }
            rows.truncate(24);
            if rows.is_empty() {
                let empty_card =
                    Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), 94.0));
                rounded(ui.painter(), empty_card, 10.0, pal.card);
                rounded_stroke(ui.painter(), empty_card, 10.0, Stroke::new(1.0, pal.border));
                avatar(
                    ui.painter(),
                    Rect::from_min_size(empty_card.min + vec2(12.0, 8.0), Vec2::splat(32.0)),
                    None,
                    "主",
                    pal,
                );
                text_in(
                    ui.painter(),
                    Rect::from_min_size(
                        empty_card.min + vec2(54.0, 8.0),
                        vec2(empty_card.width() - 64.0, 19.0),
                    ),
                    "暂无播放记录",
                    bold(13.0),
                    pal.text,
                    Align2::LEFT_CENTER,
                );
                text_in(
                    ui.painter(),
                    Rect::from_min_size(
                        empty_card.min + vec2(54.0, 27.0),
                        vec2(empty_card.width() - 64.0, 17.0),
                    ),
                    "打开房间后，这里保留最近播放。",
                    font(11.0),
                    pal.text_secondary,
                    Align2::LEFT_CENTER,
                );
                ui.allocate_space(vec2(ui.available_width(), 94.0));
                return;
            }
            for (index, (name, meta, room, active, session_id, history_target)) in
                rows.into_iter().enumerate()
            {
                let texture = session_id
                    .as_ref()
                    .and_then(|id| {
                        self.sessions
                            .snapshots()
                            .iter()
                            .find(|session| &session.id == id)
                            .map(|session| (session.target.platform, session.target.room.clone()))
                    })
                    .or_else(|| {
                        history_target
                            .as_ref()
                            .map(|target| (target.platform, target.room.clone()))
                    })
                    .and_then(|(platform, room)| self.avatar_texture(ui.ctx(), platform, &room));
                // Allocate the row before painting. A ScrollArea can only scroll
                // what the layout reports; painting at a computed offset without
                // allocating kept the content height near zero, so the overflowing
                // list had no scrollbar and the wheel did nothing (user report).
                let row_rect = ui
                    .allocate_exact_size(
                        vec2(ui.available_width(), card_height + 8.0),
                        Sense::hover(),
                    )
                    .0;
                let card = Rect::from_min_size(row_rect.min, vec2(row_rect.width(), card_height));
                let id = ui.id().with(("home-session", index));
                let response = ui.interact(card, id, Sense::click());
                rounded(ui.painter(), card, 10.0, pal.card);
                rounded_stroke(
                    ui.painter(),
                    card,
                    10.0,
                    Stroke::new(
                        1.0,
                        if response.hovered() {
                            pal.border_hover
                        } else {
                            pal.border
                        },
                    ),
                );
                let card_pad = 12.0;
                let avatar_size = 32.0;
                if compact_card {
                    avatar(
                        ui.painter(),
                        Rect::from_min_size(
                            card.min + vec2(card_pad, 8.0),
                            Vec2::splat(avatar_size),
                        ),
                        texture.as_ref(),
                        &name,
                        pal,
                    );
                    let text_left = card_pad + avatar_size + 10.0;
                    text_in(
                        ui.painter(),
                        Rect::from_min_size(
                            card.min + vec2(text_left, 7.0),
                            vec2(card.width() - text_left - 10.0, 19.0),
                        ),
                        &name,
                        bold(13.0),
                        pal.text,
                        Align2::LEFT_CENTER,
                    );
                    text_in(
                        ui.painter(),
                        Rect::from_min_size(
                            card.min + vec2(text_left, 26.0),
                            vec2(card.width() - text_left - 10.0, 17.0),
                        ),
                        &meta,
                        font(11.0),
                        pal.text_secondary,
                        Align2::LEFT_CENTER,
                    );
                    let state = room;
                    let state_color = if active {
                        pal.success
                    } else {
                        pal.text_secondary
                    };
                    ui.painter().circle_filled(
                        pos2(card.left() + card_pad + 3.0, card.top() + 55.0),
                        2.5,
                        state_color,
                    );
                    text_in(
                        ui.painter(),
                        Rect::from_min_size(
                            card.min + vec2(card_pad + 10.0, 46.0),
                            vec2(card.width() - card_pad * 2.0 - 10.0, 18.0),
                        ),
                        &state,
                        font(11.0),
                        state_color,
                        Align2::LEFT_CENTER,
                    );
                    let reopen_width = (card.width() / 3.0).min(84.0);
                    let reopen_rect = Rect::from_min_size(
                        pos2(card.right() - card_pad - reopen_width, card.top() + 66.0),
                        vec2(reopen_width, 24.0),
                    );
                    if button(ui, reopen_rect, "重新打开", Btn::Secondary, pal, true).clicked()
                    {
                        if let Some(id) = session_id.clone() {
                            reopen_session = Some(id);
                        } else if let Some(target) = history_target.clone() {
                            reopen_history = Some(target);
                        }
                    }
                } else {
                    avatar(
                        ui.painter(),
                        Rect::from_min_size(
                            card.min + vec2(card_pad, (card_height - avatar_size) / 2.0),
                            Vec2::splat(avatar_size),
                        ),
                        texture.as_ref(),
                        &name,
                        pal,
                    );
                    let text_left = card_pad + avatar_size + 10.0;
                    let state_width = (card.width() / 4.0)
                        .clamp(96.0, 150.0)
                        .min(card.width() / 3.0);
                    let reopen_width = (card.width() / 4.0).min(92.0);
                    let text_width =
                        (card.width() - text_left - reopen_width - state_width - 26.0).max(80.0);
                    text_in(
                        ui.painter(),
                        Rect::from_min_size(
                            card.min + vec2(text_left, 9.0),
                            vec2(text_width, 19.0),
                        ),
                        &name,
                        bold(13.0),
                        pal.text,
                        Align2::LEFT_CENTER,
                    );
                    text_in(
                        ui.painter(),
                        Rect::from_min_size(
                            card.min + vec2(text_left, 29.0),
                            vec2(text_width, 17.0),
                        ),
                        &meta,
                        font(11.0),
                        pal.text_secondary,
                        Align2::LEFT_CENTER,
                    );
                    let state_color = if active {
                        pal.success
                    } else {
                        pal.text_secondary
                    };
                    ui.painter().circle_filled(
                        pos2(card.left() + text_left + 3.0, card.top() + 44.0),
                        2.5,
                        state_color,
                    );
                    text_in(
                        ui.painter(),
                        Rect::from_min_size(
                            card.min + vec2(text_left + 10.0, 35.0),
                            vec2((text_width - 10.0).max(0.0), 18.0),
                        ),
                        &room,
                        font(10.5),
                        state_color,
                        Align2::LEFT_CENTER,
                    );
                    let reopen_rect = Rect::from_min_size(
                        pos2(card.right() - card_pad - reopen_width, card.top() + 17.0),
                        vec2(reopen_width, 28.0),
                    );
                    if button(ui, reopen_rect, "重新打开", Btn::Secondary, pal, true).clicked()
                    {
                        if let Some(id) = session_id.clone() {
                            reopen_session = Some(id);
                        } else if let Some(target) = history_target.clone() {
                            reopen_history = Some(target);
                        }
                    }
                }
            }
        });
        if let Some(id) = reopen_session {
            self.reopen_session(&id, context);
        } else if let Some(target) = reopen_history {
            let settings = self.snapshot.settings.clone();
            self.start_playback_with(target, self.player, settings, context);
        }
    }

    /// Re-open a recorded session with its original player and settings.
    pub(super) fn reopen_session(&mut self, id: &str, context: &egui::Context) {
        let Some(session) = self
            .sessions
            .snapshots()
            .iter()
            .find(|session| session.id == id)
            .cloned()
        else {
            return;
        };
        let launch = self.session_launch.get(id).cloned();
        let player = launch
            .as_ref()
            .map(|launch| launch.player)
            .unwrap_or(session.player);
        let settings = launch
            .map(|launch| launch.settings)
            .unwrap_or_else(|| self.snapshot.settings.clone());
        self.start_playback_with(session.target.clone(), player, settings, context);
    }
}

/// Whether the launch card uses the compact two-line button layout.
fn compact_guess(inner: f32) -> bool {
    inner < 700.0
}

/// Deterministic launch-card height, mirroring LayoutWorkbenchFavorites.
fn launch_card_height(inner: f32, compact: bool) -> f32 {
    let pad = 16.0;
    let mut y = 8.0;
    y += 30.0 + 8.0;
    y += if compact {
        36.0 + 6.0 + 32.0 + 10.0
    } else {
        38.0 + 10.0
    };
    let columns = if inner >= 680.0 {
        5
    } else if inner >= 420.0 {
        3
    } else {
        2
    };
    let rows = 5_usize.div_ceil(columns) as f32;
    y += rows * (14.0 + 3.0 + 27.0) + (rows - 1.0) * 8.0 + 6.0;
    y += 24.0_f32.max(18.0) + 8.0;
    y
}

impl Launcher {
    /// Ask for a JSON file and hand it to the bounded store-worker importer.
    /// The worker enforces the 2 MiB / UTF-8 / schema limits and reports the
    /// added/updated counts through the normal status line.
    pub(super) fn import_favorites_dialog(&mut self) {
        let filter = crate::gui::file_dialog::build_filter(&[("收藏 JSON (*.json)", "*.json")]);
        let root = self.snapshot.root.clone();
        let Some(path) = crate::gui::file_dialog::open_file_name(
            &filter,
            "导入收藏（JSON）",
            Some(root.as_path()),
        ) else {
            self.set_status("已取消导入。", false);
            return;
        };
        self.send_store(StoreAction::Import(path, ImportKind::FavoritesOrExport));
        self.set_status("正在导入收藏…", false);
    }

    /// Serialize favourites through the shared transfer module, then ask for a
    /// destination with the native save dialog. Cancelling is not an error.
    pub(super) fn export_favorites_dialog(&mut self) {
        if self.snapshot.favorites.is_empty() {
            self.set_status("没有可导出的收藏。", true);
            return;
        }
        let favorites = self.snapshot.favorites.clone();
        let text = match crate::favorites_transfer::export_favorites_json(&favorites) {
            Ok(text) => text,
            Err(error) => {
                self.set_status(format!("无法导出收藏：{error:#}"), true);
                return;
            }
        };
        let filter = crate::gui::file_dialog::build_filter(&[("收藏 JSON (*.json)", "*.json")]);
        let default_name = crate::favorites_transfer::export_file_name(chrono::Local::now());
        let Some(path) =
            crate::gui::file_dialog::save_file_name(&filter, "导出收藏", &default_name, None)
        else {
            self.set_status("已取消导出。", false);
            return;
        };
        match std::fs::write(&path, text.as_bytes()) {
            Ok(()) => self.set_status(
                format!("已导出 {} 条收藏到 {}。", favorites.len(), path.display()),
                false,
            ),
            Err(error) => self.set_status(format!("写入导出文件失败：{error}"), true),
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Store;

    fn synthetic_launcher() -> (Launcher, tempfile::TempDir, egui::Context) {
        let directory = tempfile::tempdir().unwrap();
        let context = egui::Context::default();
        let app = Launcher::from_context(
            &context,
            Store::open(directory.path().join("synthetic")).unwrap(),
        );
        (app, directory, context)
    }

    fn raw_input(time: f64, events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1200.0, 700.0))),
            time: Some(time),
            events,
            ..Default::default()
        }
    }

    fn click(pos: egui::Pos2, pressed: bool, time: f64) -> egui::RawInput {
        raw_input(
            time,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::default(),
                },
            ],
        )
    }

    fn draw(app: &mut Launcher, ctx: &egui::Context) {
        let host = Rect::from_min_size(pos2(0.0, 0.0), vec2(1200.0, 700.0));
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ctx, |ui| {
                app.favorites_page(ui, host, ctx, &Pal::of(theme::ThemeMode::Dark));
            });
    }

    #[test]
    fn format_help_link_opens_and_escape_closes_the_dialog() {
        let (mut app, _directory, context) = synthetic_launcher();
        // Same maths as the launch card: host width 1200, 16px pad, an 88x30
        // link at y=8. Keep it derived so a layout change fails loudly here.
        let host_width = 1200.0_f32;
        let pad = 16.0_f32;
        let link = pos2(host_width - pad - 88.0 / 2.0, 8.0 + 30.0 / 2.0);
        for input in [
            raw_input(0.0, vec![]),
            click(link, true, 0.1),
            click(link, false, 0.2),
        ] {
            let _ = context.run(input, |ctx| draw(&mut app, ctx));
        }
        assert!(
            app.format_help_open,
            "clicking the 格式说明 link must open the visible dialog"
        );
        let escape = egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        };
        let _ = context.run(raw_input(0.3, vec![escape]), |ctx| {
            app.format_help_dialog(ctx)
        });
        assert!(!app.format_help_open, "Escape must close the help dialog");
    }
}

