use super::*;
impl Launcher {
    pub(super) fn sessions_page(&mut self, ui: &mut egui::Ui) {
        ui.label(
            "每个播放会话独立运行。取消只影响尚未完成的启动；停止只关闭这一会话拥有的播放器。",
        );
        if ui.button("清除已结束记录").clicked() {
            self.sessions.clear_finished();
        }
        let snapshots = self.sessions.snapshots();
        if snapshots.is_empty() {
            ui.add_space(24.0);
            ui.label("还没有观看会话，可从播放工作台或收藏打开直播。");
        }
        for session in snapshots {
            ui.add_space(10.0);
            card(ui, self.dark, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(&session.title).strong().size(18.0));
                    ui.label(RichText::new(session.phase.label()).color(
                        if session.phase == SessionPhase::Failed
                            || session.phase == SessionPhase::Interrupted
                        {
                            Color32::LIGHT_RED
                        } else {
                            ACCENT
                        },
                    ));
                });
                let launch = self.session_launch.get(&session.id);
                let player = launch.map(|launch| launch.player).unwrap_or(session.player);
                let buffer = launch
                    .map(|launch| launch.settings.playback.buffer_mode)
                    .unwrap_or(self.snapshot.settings.playback.buffer_mode);
                ui.label(format!(
                    "{} · {} · 画质 {} · 缓冲 {} · {} · {}",
                    session.target,
                    player,
                    quality_label(session.target.quality),
                    match buffer {
                        BufferMode::LowLatency => "延迟更低",
                        BufferMode::Stable => "播放更稳定",
                    },
                    session.room_state.label(),
                    session
                        .started_at
                        .with_timezone(&chrono::Local)
                        .format("%m-%d %H:%M")
                ));
                if !session.room_note.is_empty() {
                    ui.label(RichText::new(&session.room_note).small().weak());
                }
                if session.room_state == crate::sessions::SessionRoomState::Replay {
                    ui.label(
                        RichText::new(replay_wait_hint(session.target.platform))
                            .small()
                            .color(ACCENT),
                    );
                }
                if let Some(error) = &session.error {
                    ui.colored_label(
                        Color32::LIGHT_RED,
                        session_error_text(session.target.platform, error),
                    );
                }
                ui.horizontal_wrapped(|ui| {
                    if session.phase.can_cancel() && ui.button("取消启动").clicked() {
                        match self.sessions.cancel_start(&session.id) {
                            Ok(()) => self.set_status("已请求取消本次启动。", false),
                            Err(e) => self.set_status(e.to_string(), true),
                        }
                    }
                    if session.phase.can_stop() && ui.button("停止此会话").clicked() {
                        match self.sessions.stop(&session.id) {
                            Ok(()) => self.set_status("正在停止所选会话。", false),
                            Err(e) => self.set_status(e.to_string(), true),
                        }
                    }
                    if session.phase.is_terminal() && ui.button("重新打开").clicked() {
                        // Reuse the player and buffer preference this session was
                        // launched with; a global settings change does not silently
                        // switch an existing room to another player.
                        let target = session.target.clone();
                        let context = ui.ctx().clone();
                        match self.session_launch.get(&session.id).cloned() {
                            Some(launch) => self.start_playback_with(
                                target,
                                launch.player,
                                launch.settings,
                                &context,
                            ),
                            None => self.start_playback(target, &context),
                        }
                    }
                });
            });
        }
    }
}
