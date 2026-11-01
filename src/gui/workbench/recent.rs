//! 最近播放 page: platform filter tabs, the "use mpv / PotPlayer" actions and the
//! row browser from LayoutWorkbenchRecent / WorkbenchRoomBrowser (cards=false).

use super::*;
use crate::core::Player;

const ROW_H: f32 = 90.0;
const ROW_H_NARROW: f32 = 122.0;
const ROW_INSET: f32 = 2.0;
const ROW_GAP: f32 = 12.0;
const ROW_RADIUS: f32 = 11.0;
const ACTION_W: f32 = 102.0;
const ACTION_H: f32 = 29.0;

impl Launcher {
    pub(super) fn recent_page(
        &mut self,
        ui: &mut Ui,
        host: Rect,
        context: &egui::Context,
        pal: &Pal,
    ) {
        let width = host.width();
        let height = host.height();
        let mut y = 6.0;
        let tab_gap = 8.0;
        let tab_width = 76.0_f32.min((width / 6.0).max(58.0));
        let tabs_width = 4.0 * tab_width + 3.0 * tab_gap;
        let _tabs_width = tabs_width;
        let mut tabs_x = 0.0;
        for platform in Platform::ALL {
            let pill_rect = Rect::from_min_size(host.min + vec2(tabs_x, y), vec2(tab_width, 32.0));
            if pill(ui, pill_rect, platform.label(), self.platform == platform, pal, true).clicked() {
                self.platform = platform;
                self.recent_selected = None;
            }
            tabs_x += tab_width + tab_gap;
        }
        let action_width = 210.0_f32.min(((width - 10.0) / 2.0).max(120.0));
        let single_row = width >= tabs_width + 24.0 + action_width * 2.0 + 10.0;
        let (mpv_button, pot_button);
        if single_row {
            mpv_button = Rect::from_min_size(
                host.min + vec2(width - action_width * 2.0 - 10.0, y),
                vec2(action_width, 32.0),
            );
            pot_button = Rect::from_min_size(
                host.min + vec2(width - action_width, y),
                vec2(action_width, 32.0),
            );
            y += 32.0 + 8.0;
        } else {
            y += 32.0 + 8.0;
            mpv_button = Rect::from_min_size(host.min + vec2(0.0, y), vec2(action_width, 32.0));
            pot_button = Rect::from_min_size(
                host.min + vec2(action_width + 10.0, y),
                vec2(action_width, 32.0),
            );
            y += 32.0 + 8.0;
        }
        let rows: Vec<(String, Target, String)> = self
            .snapshot
            .history
            .iter()
            .filter(|entry| entry.target.platform == self.platform)
            .map(|entry| (entry.name.clone(), entry.target.clone(), history_time(entry)))
            .collect();
        let selected = self
            .recent_selected
            .filter(|index| *index < rows.len());
        let action_enabled = selected.is_some();
        if button(ui, mpv_button, "用 mpv 打开选中房间", Btn::Secondary, pal, action_enabled).clicked()
            && let Some(index) = selected
            && let Some((_, target, _)) = rows.get(index).cloned()
        {
            self.recent_selected = Some(index);
            let settings = self.launch_settings();
            self.start_playback_with(target, Player::Mpv, settings, context);
        }
        if button(
            ui,
            pot_button,
            "用 PotPlayer 打开选中房间",
            Btn::Secondary,
            pal,
            action_enabled,
        )
        .clicked()
            && let Some(index) = selected
            && let Some((_, target, _)) = rows.get(index).cloned()
        {
            self.recent_selected = Some(index);
            let settings = self.launch_settings();
            self.start_playback_with(target, Player::PotPlayer, settings, context);
        }
        let label_width = 430.0_f32.min((width / 2.0).max(160.0));
        section_header(
            ui,
            Rect::from_min_size(host.min + vec2(0.0, y), vec2(label_width, 24.0)),
            &format!("最近播放（{}，双击用 mpv 打开）", self.platform.label()),
            pal,
        );
        hint(
            ui,
            Rect::from_min_size(
                host.min + vec2(label_width + 12.0, y),
                vec2((width - label_width - 12.0).max(80.0), 24.0),
            ),
            "每条记录可选择播放器；双击默认用 mpv 打开。",
            pal,
        );
        y += 24.0 + 8.0;
        let browser = Rect::from_min_size(host.min + vec2(0.0, y), vec2(width, (height - y).max(150.0)));
        self.recent_rows(ui, browser, context, pal, &rows);
        // Keyboard: arrows move the selection, Enter opens with mpv, Shift+Enter
        // with PotPlayer (the original form's AcceptButton behaviour).
        if !context.wants_keyboard_input() {
            let (up, down, enter, shift) = ui.input(|input| {
                (
                    input.key_pressed(egui::Key::ArrowUp),
                    input.key_pressed(egui::Key::ArrowDown),
                    input.key_pressed(egui::Key::Enter),
                    input.modifiers.shift,
                )
            });
            if up {
                let next = match self.recent_selected {
                    Some(index) if index > 0 => index - 1,
                    Some(_) => 0,
                    None if !rows.is_empty() => 0,
                    None => 0,
                };
                self.recent_selected = Some(next);
                if let Some((name, target, _)) = rows.get(next) {
                    self.fill_recent(&name.clone(), target);
                }
            }
            if down && !rows.is_empty() {
                let next = self
                    .recent_selected
                    .map(|index| (index + 1).min(rows.len() - 1))
                    .unwrap_or(0);
                self.recent_selected = Some(next);
                if let Some((name, target, _)) = rows.get(next) {
                    self.fill_recent(&name.clone(), target);
                }
            }
            if enter && let Some(index) = self.recent_selected
                && let Some((_, target, _)) = rows.get(index).cloned()
            {
                let settings = self.launch_settings();
                self.start_playback_with(
                    target,
                    if shift { Player::PotPlayer } else { Player::Mpv },
                    settings,
                    context,
                );
            }
        }
    }

    fn fill_recent(&mut self, name: &str, target: &Target) {
        self.platform = target.platform;
        self.room_input = target.room.clone();
        self.quality = target.quality;
        self.favorite_name = name.to_owned();
        self.set_status(
            format!(
                "已选中 {} · {}；双击或回车用 mpv 打开。",
                target.platform.label(),
                name
            ),
            false,
        );
    }

    fn recent_rows(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        context: &egui::Context,
        pal: &Pal,
        rows: &[(String, Target, String)],
    ) {
        let mut area = child(ui, rect);
        let mut open: Option<(usize, Player)> = None;
        let mut select: Option<usize> = None;
        scroll_area(&mut area, "recent-rows", |ui| {
            let client_width = ui.available_width();
            let narrow = client_width < 540.0;
            let row_height = if narrow { ROW_H_NARROW } else { ROW_H };
            let width = (client_width - ROW_INSET * 2.0).max(1.0);
            let origin = ui.cursor().min;
            if rows.is_empty() {
                empty_state(
                    ui,
                    Rect::from_min_size(origin + vec2(0.0, 18.0), vec2(client_width, 60.0)),
                    "还没有最近播放记录。",
                    "",
                    pal,
                );
            }
            for (index, (name, target, time)) in rows.iter().enumerate() {
                let card = Rect::from_min_size(
                    origin + vec2(ROW_INSET, ROW_INSET + index as f32 * (row_height + ROW_GAP)),
                    vec2(width, row_height),
                );
                let id = ui.id().with(("recent-row", index));
                let response = ui.interact(card, id, Sense::click());
                let is_selected = self.recent_selected == Some(index);
                rounded(
                    ui.painter(),
                    card,
                    ROW_RADIUS,
                    if is_selected { pal.selection_soft } else { pal.card },
                );
                rounded_stroke(
                    ui.painter(),
                    card,
                    ROW_RADIUS,
                    Stroke::new(
                        if is_selected { 2.0 } else { 1.0 },
                        if is_selected {
                            pal.selection_border
                        } else if response.hovered() {
                            pal.border_hover
                        } else {
                            pal.border
                        },
                    ),
                );
                if response.clicked() {
                    select = Some(index);
                }
                if response.double_clicked() {
                    open = Some((index, Player::Mpv));
                }
                let inner = 12.0;
                let avatar_rect = Rect::from_min_size(card.min + vec2(inner, 10.0), Vec2::splat(36.0));
                let texture = self.avatar_texture(ui.ctx(), target.platform, &target.room);
                avatar(ui.painter(), avatar_rect, texture.as_ref(), name, pal);
                let text_left = inner + 36.0 + 10.0;
                // Right side: two action buttons (102x29, 8px gap, 12px inset).
                let button_right = card.right() - inner;
                let pot = Rect::from_min_size(
                    pos2(button_right - ACTION_W, card.top() + (row_height - ACTION_H) / 2.0),
                    vec2(ACTION_W, ACTION_H),
                );
                let mpv = Rect::from_min_size(
                    pos2(pot.left() - 8.0 - ACTION_W, pot.top()),
                    vec2(ACTION_W, ACTION_H),
                );
                let state_right = mpv.left() - 14.0;
                let text_width = (state_right - 12.0 - text_left).max(80.0);
                let state_width = 100.0_f32.min((text_width / 3.0).max(0.0));
                text_in(
                    ui.painter(),
                    Rect::from_min_size(card.min + vec2(text_left, 10.0), vec2(text_width - state_width - 6.0, 23.0)),
                    name,
                    bold(13.0),
                    pal.text,
                    Align2::LEFT_CENTER,
                );
                text_in(
                    ui.painter(),
                    Rect::from_min_size(card.min + vec2(text_left, 36.0), vec2(text_width, 18.0)),
                    format!("{} · {} · {}", target.platform.label(), target.room, time),
                    font(11.0),
                    pal.text_secondary,
                    Align2::LEFT_CENTER,
                );
                text_in(
                    ui.painter(),
                    Rect::from_min_size(card.min + vec2(text_left, 57.0), vec2(text_width, 18.0)),
                    format!("房间 {}", target.room),
                    font(11.0),
                    pal.text_secondary,
                    Align2::LEFT_CENTER,
                );
                if let Some(badge) = self.badge_label(target) {
                    let live = badge.contains("播") && !badge.contains("休");
                    let color = if live { pal.success } else { pal.text_secondary };
                    let label = if live { "直播中" } else { "已下播" };
                    let dot_x = state_right - state_width;
                    ui.painter().circle_filled(
                        pos2(dot_x + 3.0, card.top() + row_height / 2.0),
                        2.5,
                        color,
                    );
                    text_in(
                        ui.painter(),
                        Rect::from_min_size(
                            pos2(dot_x + 10.0, card.top() + (row_height - 20.0) / 2.0),
                            vec2(state_width - 10.0, 20.0),
                        ),
                        label,
                        font(11.0),
                        color,
                        Align2::LEFT_CENTER,
                    );
                }
                let mpv_hot = ui.interact(mpv, id.with("mpv"), Sense::click()).hovered();
                let pot_hot = ui.interact(pot, id.with("pot"), Sense::click()).hovered();
                Self::paint_row_button(ui, mpv, "mpv", true, pal, mpv_hot);
                Self::paint_row_button(ui, pot, "PotPlayer", false, pal, pot_hot);
                if ui.interact(mpv, id.with("mpv"), Sense::click()).clicked() {
                    open = Some((index, Player::Mpv));
                }
                if ui.interact(pot, id.with("pot"), Sense::click()).clicked() {
                    open = Some((index, Player::PotPlayer));
                }
            }
            let content = ROW_INSET * 2.0 + rows.len() as f32 * (row_height + ROW_GAP);
            ui.allocate_space(vec2(client_width, content));
        });
        if let Some(index) = select {
            self.recent_selected = Some(index);
            if let Some((name, target, _)) = rows.get(index).cloned() {
                self.fill_recent(&name, &target);
            }
        }
        if let Some((index, player)) = open
            && let Some((_, target, _)) = rows.get(index).cloned()
        {
            self.recent_selected = Some(index);
            let settings = self.launch_settings();
            self.start_playback_with(target, player, settings, context);
        }
    }

    fn paint_row_button(ui: &mut Ui, rect: Rect, text: &str, play_icon: bool, pal: &Pal, hovered: bool) {
        rounded(ui.painter(), rect, 6.0, if hovered { pal.hover } else { pal.card });
        rounded_stroke(
            ui.painter(),
            rect,
            6.0,
            Stroke::new(1.0, if hovered { pal.border_hover } else { pal.border }),
        );
        if play_icon {
            let icon = Rect::from_center_size(
                pos2(
                    rect.center().x - measure(ui.painter(), text, &font(11.0)) / 2.0 - 10.0,
                    rect.center().y,
                ),
                Vec2::splat(9.0),
            );
            draw_glyph(ui.painter(), icon, Glyph::Play, pal.text, 1.0);
            text_in(
                ui.painter(),
                rect.translate(vec2(5.0, 0.0)),
                text,
                font(11.0),
                pal.text,
                Align2::CENTER_CENTER,
            );
        } else {
            text_in(ui.painter(), rect, text, font(11.0), pal.text, Align2::CENTER_CENTER);
        }
    }
}
