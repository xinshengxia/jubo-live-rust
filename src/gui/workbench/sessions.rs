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
        let mut area = child(ui, host);
        egui::ScrollArea::vertical().id_salt("sessions-workbench-all").show(&mut area, |ui| {
            ui.heading("正在观看 / 最近打开");
            let snapshots = self.sessions.snapshots();
            let selected = self.session_selected.as_ref().and_then(|id| snapshots.iter().find(|row| &row.id == id)).cloned();
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(selected.is_some(), egui::Button::new("重新打开选中房间")).clicked()
                    && let Some(session) = &selected { self.reopen_session(&session.id, context); }
                if ui.add_enabled(selected.as_ref().is_some_and(|s| s.phase.can_cancel()), egui::Button::new("取消启动")).clicked()
                    && let Some(session) = &selected {
                    match self.sessions.cancel_start(&session.id) { Ok(()) => self.set_status("已请求取消所选启动。", false), Err(e) => self.set_status(e.to_string(), true) }
                }
                if ui.add_enabled(selected.as_ref().is_some_and(|s| s.phase.can_stop()), egui::Button::new("停止此会话")).clicked()
                    && let Some(session) = &selected {
                    match self.sessions.stop(&session.id) { Ok(()) => self.set_status("正在停止所选会话。", false), Err(e) => self.set_status(e.to_string(), true) }
                }
                if ui.add_enabled(selected.as_ref().is_some_and(|s| s.phase.can_stop()), egui::Button::new("一次重新同步")).on_hover_text("仅重置所选会话，可能短暂停顿或跳过内容；不是永久防复发保证。").clicked()
                    && let Some(session) = &selected {
                    match self.sessions.resync(&session.id) { Ok(()) => self.set_status("已请求所选会话重新同步，可能短暂停顿。", false), Err(e) => self.set_status(e.to_string(), true) }
                }
                if ui.add_enabled(selected.is_some(), egui::Button::new("标记音画异常")).clicked()
                    && let Some(session) = &selected {
                    let counters = session.relay.as_ref().map(sync_diagnostics)
                        .unwrap_or_else(|| "尚无中继遥测".into());
                    let message = format!("session={} player={} phase={} {}", session.id,
                        session.player, session.phase.label(), counters);
                    let saved = crate::diag::append(&self.snapshot.root.join("logs"), "avsync",
                        crate::diag::Level::Info, &message);
                    self.set_status(if saved { "已记录异常时间与当前中继计数，可在诊断日志查看。" }
                        else { "异常标记未保存，请检查日志目录写入权限。" }, !saved);
                }
                if ui.button("查看诊断日志").clicked() {
                    self.page = Page::Data;
                    self.data_tab = crate::gui::data_page::DataTab::Local;
                    self.data_category = crate::gui::data_page::DataCategory::Logs;
                    self.data_detail = true;
                    self.refresh_data_snapshot();
                }
                if ui.button("清除已结束记录").clicked() { self.sessions.clear_finished(); }
            });
            ui.small("重开保留原播放器、画质、缓冲和弹幕选项；停止与重新同步仅影响所选本程序会话。");
            ui.add_space(8.0);
            let height = (host.height() * 0.55).clamp(180.0, 360.0);
            let table = Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), height));
            self.session_table(ui, table, context, pal, &snapshots, self.session_selected.clone().as_deref());
            ui.allocate_space(vec2(table.width(), height + 8.0));
            if let Some(session) = selected {
                ui.strong(format!("{} · {}", session.room_state.label(), session.phase.label()));
                // The backend already reports an explicit resync through
                // room_note ("正在停止本会话…/已重新打开为新的观看会话（第N次）"),
                // so the page only has to render that note and the relay counters.
                if !session.room_note.is_empty() { ui.label(&session.room_note); }
                if let Some(error) = &session.error { ui.colored_label(pal.danger, session_error_text(session.target.platform, error)); }
                if let Some(relay) = &session.relay {
                    // Public-safe counters only: no loopback URL, token or upstream
                    // media address is ever carried by RelayTelemetry.
                    ui.label(format!("中继：{} · 活跃{} · 累计{} · 重连{} · 失败{}（连续{}） · 稳定片段{} · 转发{} tags / {} bytes · 预算耗尽{}",
                        relay.state, relay.active_streams, relay.total_streams, relay.reconnects, relay.failures, relay.consecutive_failures,
                        relay.stable_segments, relay.forwarded_tags, relay.forwarded_bytes, relay.budget_exhaustions));
                    ui.label(sync_diagnostics(relay));
                    ui.small("时钟差来自中继最新音视频包的时间戳，不是实际扬声器与屏幕的同步测量。标记异常后再重新同步，可保留故障当时的计数。");
                    if let Some(waiting) = &relay.waiting_state {
                        let label = match waiting.as_str() {
                            "offline" => "等待主播重新开播",
                            "replay_unavailable" => "重播暂不可取流",
                            other => other,
                        };
                        ui.label(format!("中继等待：{label}"));
                    }
                } else {
                    ui.small("该会话尚未回报中继遥测（未启用中转或会话已结束）。");
                }
            }
        });
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
            columns[index] =
                Rect::from_min_size(pos2(x, table.top()), vec2(column_width, HEADER_H));
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
        hint(
            ui,
            Rect::from_min_size(table.min, vec2(table.width(), HEADER_H)),
            "",
            pal,
        );
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
                        format!(
                            "{} · {}",
                            session.target.platform.label(),
                            session.target.room
                        ),
                    ),
                    (player.to_string(), player.to_string()),
                    (
                        session.room_state.label().to_owned(),
                        format!("{} · {}", session.room_state.label(), session.phase.label()),
                    ),
                    (
                        quality_label(session.target.quality).to_owned(),
                        quality_label(session.target.quality).to_owned(),
                    ),
                    (buffer_label.to_owned(), buffer_label.to_owned()),
                ];
                for (column_index, (_cell, second)) in cells.into_iter().enumerate() {
                    let cell_rect = Rect::from_min_size(
                        pos2(columns[column_index].left() + 12.0, row.top()),
                        vec2(columns[column_index].width() - 20.0, ROW_H),
                    );
                    if column_index == 0 {
                        text_in(
                            ui.painter(),
                            Rect::from_min_max(
                                cell_rect.min,
                                pos2(cell_rect.right(), cell_rect.center().y),
                            ),
                            &session.title,
                            bold(12.0),
                            pal.text,
                            Align2::LEFT_BOTTOM,
                        );
                        text_in(
                            ui.painter(),
                            Rect::from_min_max(
                                pos2(cell_rect.left(), cell_rect.center().y),
                                cell_rect.max,
                            ),
                            format!(
                                "{} · {}",
                                session.target.platform.label(),
                                session.target.room
                            ),
                            font(10.5),
                            pal.text_secondary,
                            Align2::LEFT_TOP,
                        );
                    } else {
                        let color = if column_index == 2 {
                            if session.phase.can_stop() {
                                pal.success
                            } else if matches!(
                                session.phase,
                                crate::sessions::SessionPhase::Failed
                                    | crate::sessions::SessionPhase::Interrupted
                            ) {
                                pal.danger
                            } else {
                                pal.text_secondary
                            }
                        } else {
                            pal.text_secondary
                        };
                        text_in(
                            ui.painter(),
                            cell_rect,
                            &second,
                            font(11.0),
                            color,
                            Align2::LEFT_CENTER,
                        );
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

/// Only typed counters reach the page/log, never media addresses or player arguments.
fn sync_diagnostics(relay: &crate::playback::RelayTelemetry) -> String {
    let delta = |value: Option<i64>| value.map(|v| format!("{v}ms")).unwrap_or_else(|| "未知".into());
    format!("时钟：跳变{} · 代际{} · 丢弃旧包{} · 修正{}ms · 视频减音频（输入{} / 输出{}） · 时钟用尽{} · 线路拒绝{}",
        relay.timeline_jumps, relay.timeline_epochs, relay.timeline_discarded_tags,
        relay.clock_shift_ms, delta(relay.av_delta_in_ms), delta(relay.av_delta_out_ms),
        relay.timeline_exhausted, relay.policy_rejections)
}
