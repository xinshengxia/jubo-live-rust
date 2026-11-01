//! 账号与登录 page: the four platform cards in a 2x2 grid
//! (BuildAccounts in livehub-workbench-data.cs), each with the Cookie status tag,
//! the health summary, a reason line and four ghost actions.

use super::*;
use crate::login::{self, HealthState};

const CARD_H: f32 = 141.0;
const CARD_GAP: f32 = 12.0;
const TAG_W: f32 = 84.0;
const TAG_H: f32 = 24.0;

impl Launcher {
    pub(super) fn accounts_workbench_page(
        &mut self,
        ui: &mut Ui,
        host: Rect,
        context: &egui::Context,
    ) {
        let pal = self.palette();
        if let Some(image) = self.login_image.take() {
            self.login_qr = Some(context.load_texture(
                "bilibili-login-qr",
                image,
                egui::TextureOptions::NEAREST,
            ));
        }
        let width = host.width();
        // ---- toolbar -----------------------------------------------------
        text_in(
            ui.painter(),
            Rect::from_min_size(host.min, vec2(120.0, 32.0)),
            "四平台账号",
            bold(14.0),
            pal.text,
            Align2::LEFT_CENTER,
        );
        let check_all = Rect::from_min_size(host.min + vec2(128.0, 0.0), vec2(101.0, 32.0));
        if button(ui, check_all, "检测全部平台", Btn::Secondary, &pal, !self.health_busy).clicked() {
            self.health_all_requested = true;
            self.login_message = "将按平台顺序逐个检查已保存的凭据。".to_owned();
        }
        // ---- 2x2 platform cards -----------------------------------------
        let grid_top = 43.0;
        let card_width = ((width - CARD_GAP) / 2.0).max(200.0);
        let now = health_now();
        for (index, platform) in Platform::ALL.into_iter().enumerate() {
            let column = index % 2;
            let row = index / 2;
            let card = Rect::from_min_size(
                host.min
                    + vec2(
                        column as f32 * (card_width + CARD_GAP),
                        grid_top + row as f32 * (CARD_H + CARD_GAP),
                    ),
                vec2(card_width, CARD_H),
            );
            rounded(ui.painter(), card, 10.0, pal.card);
            rounded_stroke(ui.painter(), card, 10.0, Stroke::new(1.0, pal.border));
            let pad = 14.0;
            text_in(
                ui.painter(),
                Rect::from_min_size(card.min + vec2(pad, 8.0), vec2(card_width - pad * 2.0, 31.5)),
                platform.label(),
                bold(15.0),
                pal.text,
                Align2::LEFT_CENTER,
            );
            let tag = Rect::from_min_size(
                pos2(card.right() - pad - TAG_W, card.top() + 10.5),
                vec2(TAG_W, TAG_H),
            );
            rounded(ui.painter(), tag, 6.0, Color32::TRANSPARENT);
            rounded_stroke(ui.painter(), tag, 6.0, Stroke::new(1.0, pal.border));
            text_in(
                ui.painter(),
                tag,
                "Cookie 状态",
                font(11.0),
                pal.text_secondary,
                Align2::CENTER_CENTER,
            );
            let (summary, summary_color, reason) = self.account_summary(platform, now, &pal);
            text_in(
                ui.painter(),
                Rect::from_min_size(card.min + vec2(pad, 34.5), vec2(card_width - pad * 2.0, 19.0)),
                &summary,
                font(12.0),
                summary_color,
                Align2::LEFT_CENTER,
            );
            let check_line = format!(
                "最近检测：{}",
                self.login_health
                    .get(&platform)
                    .map(|health| login::time_text(health.checked_at))
                    .unwrap_or_else(|| "尚未在线验证".to_owned())
            );
            text_in(
                ui.painter(),
                Rect::from_min_size(card.min + vec2(pad, 53.5), vec2(card_width - pad * 2.0, 17.0)),
                &check_line,
                font(12.0),
                summary_color,
                Align2::LEFT_CENTER,
            );
            text_in(
                ui.painter(),
                Rect::from_min_size(card.min + vec2(pad, 72.0), vec2(card_width - pad * 2.0, 16.5)),
                &reason,
                font(11.0),
                pal.text_secondary,
                Align2::LEFT_CENTER,
            );
            // Four ghost action buttons, 25% columns.
            let actions = [
                format!("{}扫码", platform.label()),
                "检测 Cookie".to_owned(),
                "状态详情".to_owned(),
                "退出登录".to_owned(),
            ];
            let action_width = (card_width - pad * 2.0) / 4.0;
            for (action_index, label) in actions.iter().enumerate() {
                let rect = Rect::from_min_size(
                    pos2(card.left() + pad + action_index as f32 * action_width, card.top() + 94.0),
                    vec2(action_width - 6.0, 24.0),
                );
                let id = ui.id().with(("account-action", platform.to_string(), action_index));
                let response = ui.interact(rect, id, Sense::click());
                if response.hovered() {
                    rounded(ui.painter(), rect, 6.0, pal.hover);
                }
                let color = if action_index == 3 {
                    pal.danger
                } else {
                    pal.text
                };
                text_in(ui.painter(), rect, label, font(12.0), color, Align2::CENTER_CENTER);
                if response.clicked() {
                    self.platform = platform;
                    match action_index {
                        0 => {
                            let kind = if platform == Platform::Bili { 0 } else { 1 };
                            self.start_login(context, kind);
                        }
                        1 => self.start_login(context, 2),
                        2 => {
                            let detail = match self.login_health.get(&platform) {
                                Some(health) => health.detail_text(now),
                                None => self.observations.detail_text(platform, now),
                            };
                            self.account_details = Some(detail);
                            self.set_status(
                                format!("{} · 已显示状态详情（见卡片下方说明）", platform.label()),
                                false,
                            );
                        }
                        _ => {
                            self.logout_confirm = Some(platform);
                        }
                    }
                }
            }
        }
        // ---- bottom note --------------------------------------------------
        let note_top = grid_top + CARD_H * 2.0 + CARD_GAP + 12.0;
        text_in(
            ui.painter(),
            Rect::from_min_size(host.min + vec2(0.0, note_top), vec2(width, 19.0)),
            "检测超时、网页核验或格式变化不会直接判定登录失效；网页登录使用 Edge 隐私窗口（Chrome 备选），不会登录或同步你的 Microsoft/Google 账号。",
            font(11.0),
            pal.text_secondary,
            Align2::LEFT_CENTER,
        );
        // ---- logout confirmation (when requested) -------------------------
        let mut y = note_top + 26.0;
        if let Some(platform) = self.logout_confirm {
            let dialog = Rect::from_min_size(host.min + vec2(0.0, y), vec2(width, 48.0));
            rounded(ui.painter(), dialog, 8.0, pal.card);
            rounded_stroke(ui.painter(), dialog, 8.0, Stroke::new(1.0, pal.border));
            text_in(
                ui.painter(),
                Rect::from_min_size(dialog.min + vec2(14.0, 0.0), vec2(width - 280.0, 48.0)),
                format!(
                    "退出 {} 并清除该平台 Cookie 与独立登录资料？现有播放/弹幕会话需自行关闭。",
                    platform.label()
                ),
                font(12.0),
                pal.text,
                Align2::LEFT_CENTER,
            );
            let cancel = Rect::from_min_size(
                pos2(dialog.right() - 14.0 - 88.0, dialog.top() + 8.0),
                vec2(88.0, 32.0),
            );
            let confirm = Rect::from_min_size(
                pos2(cancel.left() - 8.0 - 88.0, dialog.top() + 8.0),
                vec2(88.0, 32.0),
            );
            if button(ui, cancel, "取消", Btn::Secondary, &pal, true).clicked() {
                self.logout_confirm = None;
            }
            if button(ui, confirm, "确认退出", Btn::Primary, &pal, true).clicked() {
                self.send_store(StoreAction::CredentialRemove(platform));
                self.send_store(StoreAction::ClearBrowserProfiles(platform));
                self.logout_confirm = None;
                self.set_status(
                    format!("已请求清除 {} 的登录凭据与独立资料目录。", platform.label()),
                    false,
                );
            }
            y += 56.0;
        }
        // ---- Rust extras: login activity, QR and status details -----------
        let show_activity = self.login_busy || self.login_qr.is_some();
        let detail_text = self.account_details.clone();
        if show_activity {
            let panel_height = if self.login_qr.is_some() { 176.0 } else { 88.0 };
            let panel = Rect::from_min_size(host.min + vec2(0.0, y), vec2(width, panel_height));
            rounded(ui.painter(), panel, 10.0, pal.card);
            rounded_stroke(ui.painter(), panel, 10.0, Stroke::new(1.0, pal.border));
            if let Some(texture) = &self.login_qr {
                let qr_rect = Rect::from_min_size(panel.min + vec2(16.0, 16.0), Vec2::splat(144.0));
                ui.painter().image(
                    texture.id(),
                    qr_rect,
                    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            let text_left = if self.login_qr.is_some() { 176.0 } else { 16.0 };
            wrap_text(
                ui.painter(),
                Rect::from_min_size(
                    panel.min + vec2(text_left, 16.0),
                    vec2(panel.width() - text_left - 230.0, 60.0),
                ),
                &self.login_message,
                font(12.0),
                pal.text_secondary,
            );
            if self.login_busy {
                let capture = Rect::from_min_size(
                    pos2(panel.right() - 16.0 - 220.0, panel.top() + 16.0),
                    vec2(220.0, 34.0),
                );
                if button(ui, capture, "我已登录，检查并保存", Btn::Primary, &pal, true).clicked() {
                    self.login_capture.store(true, Ordering::Release);
                }
                let cancel = Rect::from_min_size(
                    pos2(panel.right() - 16.0 - 220.0, panel.top() + 58.0),
                    vec2(220.0, 34.0),
                );
                if button(ui, cancel, "取消本次登录", Btn::Secondary, &pal, true).clicked() {
                    self.cancel_login();
                }
            }
            y += panel_height + 10.0;
        } else if let Some(detail) = detail_text {
            wrap_text(
                ui.painter(),
                Rect::from_min_size(host.min + vec2(0.0, y), vec2(width, 44.0)),
                &detail,
                font(11.0),
                pal.text_secondary,
            );
        }
        let _ = y;
    }

    /// Current Cookie verdict + reason for one platform card.
    fn account_summary(&self, platform: Platform, now: u64, pal: &Pal) -> (String, Color32, String) {
        if let Some(health) = self.login_health.get(&platform) {
            let color = match health.state {
                HealthState::Valid => {
                    if health.is_stale(now) {
                        pal.warning
                    } else {
                        pal.success
                    }
                }
                HealthState::Invalid | HealthState::Expired => pal.danger,
                HealthState::VerificationRequired | HealthState::NetworkUnknown => pal.warning,
                _ => pal.text_secondary,
            };
            return (
                format!(
                    "{} Cookie：{}",
                    platform.label(),
                    health.current_label(now)
                ),
                color,
                health.current_reason(now),
            );
        }
        if self.snapshot.credentials.imported_for(platform) {
            (
                format!("{} Cookie：凭据已保存，尚未向平台确认", platform.label()),
                pal.text_secondary,
                "点「检测 Cookie」向平台确认当前登录状态。".to_owned(),
            )
        } else {
            (
                format!("{} Cookie：未保存登录凭据", platform.label()),
                pal.text_secondary,
                "未保存本程序可用的登录凭据。".to_owned(),
            )
        }
    }
}