//! 观看会话 page: header row, the reopen action, the column header strip and the
//! session list (LayoutWorkbenchSessions / ModernListView in the original).

use super::*;
use crate::core::Player;

const HEADER_H: f32 = 20.0;
const ROW_H: f32 = 40.0;

impl Launcher {
    pub(super) fn sessions_workbench_page(
        &mut self,
        ui: &mut Ui,
        host: Rect,
        context: &egui::Context,
        pal: &Pal,
    ) {
        let width = host.width();
        let height = host.height();
        let mut y = 2.0;
        let reopen_width = 190.0_f32.min((width / 4.0).max(130.0));
        section_header(
            ui,
            Rect::from_min_size(host.min + vec2(0.0, y), vec2((width - reopen_width - 12.0).max(90.0), 30.0)),
            "正在观看 / 最近打开",
            pal,
        );
        let snapshots: Vec<_> = self.sessions.snapshots().to_vec();
        let selected_id = self
            .session_selected
            .clone()
            .filter(|id| snapshots.iter().any(|session| &session.id == id));
        let reopen_rect = Rect::from_min_size(
            host.min + vec2(width - reopen_width, y),
            vec2(reopen_width, 30.0),
        );
        if button(
            ui,
            reopen_rect,
            "重新打开选中房间",
            Btn::Secondary,
            pal,
            selected_id.is_some(),
        )
        .clicked()
            && let Some(id) = selected_id.clone()
        {
            self.reopen_session(&id, context);
        }
        y += 30.0 + 4.0;
        hint(
            ui,
            Rect::from_min_size(
                host.min + vec2(0.0, y),
                vec2((width - 12.0).max(80.0), 18.0),
            ),
            "重新打开将恢复该会话的播放器、画质、缓冲和弹幕选项。",
            pal,
        );
        y += 18.0 + 8.0;
        let table = Rect::from_min_size(
            host.min + vec2(0.0, y),
            vec2(width, (height - y - 2.0).max(150.0)),
        );
        self.session_table(ui, table, context, pal, &snapshots, selected_id.as_deref());
    }

    fn session_table(
        &mut self,
        ui: &mut Ui,
        table: Rect,
        context: &egui::Context,
        pal: &Pal,
        snapshots: &[crate::sessions::SessionSnapshot],
        selected_id: Option<&str>,
    ) {
        // The original list is a rounded card: header strip, rows, and an empty
        // state drawn on top of the list when no session exists.
        rounded(ui.painter(), table, 10.0, pal.card);
        rounded_stroke(ui.painter(), table, 10.0, Stroke::new(1.0, pal.border));
        let headers = ["房间", "播放器", "直播状态", "画质", "缓冲"];
        let fractions = [0.40_f32, 0.20, 0.18, 0.11, 0.11];
        let inner_width = table.width() - 2.0;
        let mut columns = [Rect::NOTHING; 5];
        let mut x = table.left() + 1.0;
        for (index, fraction) in fractions.into_iter().enumerate() {
            let column_width = inner_width * fraction;
            columns[index] = Rect::from_min_size(
                pos2(x, table.top()),
                vec2(column_width, HEADER_H),
            );
            x += column_width;
        }
        for (index, header) in headers.into_iter().enumerate() {
            if index > 0 {
                ui.painter().vline(
                    columns[index].left(),
                    (table.top() + 4.0)..=(table.bottom() - 4.0),
                    Stroke::new(1.0, with_alpha(pal.border, 0.7)),
                );
            }
            text_in(
                ui.painter(),
                columns[index].shrink2(vec2(12.0, 0.0)),
                header,
                font(11.0),
                pal.text_secondary,
                Align2::LEFT_CENTER,
            );
        }
        hint(ui, Rect::from_min_size(table.min, vec2(table.width(), HEADER_H)), "", pal);
        let list = Rect::from_min_max(
            pos2(table.left() + 1.0, table.top() + HEADER_H),
            pos2(table.right() - 1.0, table.bottom() - 1.0),
        );
        ui.painter().rect_filled(list, 8.0, pal.card);
        let mut row_area = child(ui, list);
        let mut reopen: Option<String> = None;
        let mut select: Option<String> = None;
        let has_rows = !snapshots.is_empty();
        scroll_area(&mut row_area, "sessions-list", |ui| {
            let origin = ui.cursor().min;
            for (index, session) in snapshots.iter().enumerate() {
                let row = Rect::from_min_size(
                    origin + vec2(0.0, index as f32 * ROW_H),
                    vec2(list.width(), ROW_H),
                );
                let id = ui.id().with(("session-row", index));
                let response = ui.interact(row, id, Sense::click());
                let is_selected = selected_id == Some(session.id.as_str());
                if is_selected {
                    rounded(ui.painter(), row, 6.0, pal.selection_soft);
                } else if response.hovered() {
                    rounded(ui.painter(), row, 6.0, pal.hover);
                }
                if response.clicked() {
                    select = Some(session.id.clone());
                }
                if response.double_clicked() {
                    reopen = Some(session.id.clone());
                }
                // Column cells: the player and buffer come from the recorded
                // launch options when available, otherwise from the session and
                // current preferences.
                let launch = self.session_launch.get(&session.id);
                let player = launch.map(|launch| launch.player).unwrap_or(session.player);
                let buffer = launch
                    .map(|launch| launch.settings.playback.buffer_mode)
                    .unwrap_or(self.snapshot.settings.playback.buffer_mode);
                let buffer_label = match buffer {
                    crate::core::BufferMode::LowLatency => "延迟更低",
                    crate::core::BufferMode::Stable => "播放更稳定",
                };
                let cells = [
                    (
                        format!("{}", session.target),
                        format!("{} · {}", session.target.platform.label(), session.target.room),
                    ),
                    (player.to_string(), player.to_string()),
                    (session.phase.label().to_owned(), session.phase.label().to_owned()),
                    (
                        quality_label(session.target.quality).to_owned(),
                        quality_label(session.target.quality).to_owned(),
                    ),
                    (buffer_label.to_owned(), buffer_label.to_owned()),
                ];
                for (column_index, (_cell, second)) in cells.into_iter().enumerate() {
                    let cell_rect = Rect::from_min_size(
                        pos2(
                            columns[column_index].left() + 12.0,
                            row.top(),
                        ),
                        vec2(columns[column_index].width() - 20.0, ROW_H),
                    );
                    if column_index == 0 {
                        text_in(
                            ui.painter(),
                            Rect::from_min_max(cell_rect.min, pos2(cell_rect.right(), cell_rect.center().y)),
                            &session.title,
                            bold(12.0),
                            pal.text,
                            Align2::LEFT_BOTTOM,
                        );
                        text_in(
                            ui.painter(),
                            Rect::from_min_max(pos2(cell_rect.left(), cell_rect.center().y), cell_rect.max),
                            format!("{} · {}", session.target.platform.label(), session.target.room),
                            font(10.5),
                            pal.text_secondary,
                            Align2::LEFT_TOP,
                        );
                    } else {
                        let color = if column_index == 2 {
                            if session.phase.can_stop() {
                                pal.success
                            } else if matches!(session.phase, crate::sessions::SessionPhase::Failed | crate::sessions::SessionPhase::Interrupted) {
                                pal.danger
                            } else {
                                pal.text_secondary
                            }
                        } else {
                            pal.text_secondary
                        };
                        text_in(ui.painter(), cell_rect, &second, font(11.0), color, Align2::LEFT_CENTER);
                    }
                }
            }
            if !has_rows {
                let empty = Rect::from_min_size(
                    origin + vec2(8.0, 60.0),
                    vec2((list.width() - 16.0).max(80.0), 96.0),
                );
                rounded(ui.painter(), empty, 6.0, pal.surface);
                text_in(
                    ui.painter(),
                    Rect::from_min_size(empty.min + vec2(0.0, 26.0), vec2(empty.width(), 22.0)),
                    "暂无观看会话",
                    font(13.0),
                    pal.text_secondary,
                    Align2::CENTER_CENTER,
                );
                text_in(
                    ui.painter(),
                    Rect::from_min_size(empty.min + vec2(0.0, 50.0), vec2(empty.width(), 22.0)),
                    "打开一个房间后，这里会显示会话记录。",
                    font(13.0),
                    pal.text_secondary,
                    Align2::CENTER_CENTER,
                );
            }
            let content = (snapshots.len() as f32 * ROW_H).max(1.0);
            ui.allocate_space(vec2(list.width(), content));
        });
        if let Some(id) = select {
            self.session_selected = Some(id);
        }
        if let Some(id) = reopen {
            self.reopen_session(&id, context);
        }
    }
}
