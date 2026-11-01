//! 「弹幕与 SC 设置」page (original 8-page workbench, page 6).
//!
//! Five original tabs in the C# order (显示与内容 / SC 醒目留言 / 过滤与开关 /
//! 更多弹幕设置 / 界面与热键), the two-column 484px form grid inside the
//! 1028px card, and the fixed bottom action row (status + 重新读取 + 保存弹幕设置).
//! Every control edits the launcher's existing OverlaySettings draft; saving goes
//! through StoreAction::SaveOverlay, exactly like the original page wrote
//! danmaku.json. This file owns no persistence or overlay logic.

use super::*;
use crate::core::{HotkeyBindings, ScCorner, ScMode};
use crate::hotkeys::{HotkeyAction, HotkeyConfigError};
use crate::gui::{parse_reserved_areas, reserved_areas_text};
use eframe::egui::Pos2;

// Original page geometry (logical pixels, 1240x720 design size).
const TAB_TITLES: [&str; 5] = [
    "显示与内容",
    "SC 醒目留言",
    "过滤与开关",
    "更多弹幕设置",
    "界面与热键",
];
const TAB_LEFT: f32 = 14.0;
const TAB_TOP: f32 = 4.0;
const TAB_H: f32 = 34.0;
const TAB_GAP: f32 = 6.0;
const TAB_PAD: f32 = 30.0;
const TAB_MIN_W: f32 = 80.0;
const CARD_RADIUS: f32 = 10.0;
const CARD_PAD: f32 = 16.0;
const CARD_HEAD_TOP: f32 = 12.0;
const FORM_TOP: f32 = 42.0;
const ROW_PITCH: f32 = 72.0;
const FIELD_W: f32 = 484.0;
/// Column delta between the two form columns (left label x = card+16, right = +514).
const COL_DX: f32 = 498.0;
const DROPDOWN_H: f32 = 25.0;
const ACTION_H: f32 = 32.0;
const ACTION_GAP: f32 = 10.0;
const CHECK_TOP: f32 = 48.0;
const CHECK_PITCH: f32 = 30.5;
const CHECK_COL_DX: f32 = 500.0;
const CHECK_H: f32 = 24.0;
const MORE_LABEL_W: f32 = 420.0;
const MORE_CTRL_X: f32 = 455.0;
const MORE_ROW: f32 = 31.0;
const SCROLL_ID: &str = "danmaku-settings-scroll";
/// Tab 5 (界面与热键) card height: font section + 11 hotkey editor rows.
const TAB5_CARD_H: f32 = 566.0;
/// Hotkey editor row pitch (label + 24px input).
const HOTKEY_ROW: f32 = 28.0;
const HOTKEY_LABEL_W: f32 = 140.0;
const HOTKEY_INPUT_LEFT: f32 = 150.0;
const HOTKEY_INPUT_W: f32 = 420.0;

// Dropdown presets. Values are the persisted numeric fields; labels follow the
// original page wording where a value exists in the C# list.
const FONT_SIZE_ITEMS: [&str; 5] = [
    "超小（15 像素）",
    "小（20 像素）",
    "标准（同 B站）",
    "大（33 像素）",
    "超大（43 像素）",
];
const FONT_SIZE_VALUES: [u16; 5] = [15, 20, 25, 33, 43];
const SPEED_ITEMS: [&str; 5] = [
    "很慢（60%）",
    "慢（80%）",
    "标准（100%）",
    "快（130%）",
    "很快（170%）",
];
const SPEED_VALUES: [u16; 5] = [60, 80, 100, 130, 170];
const OPACITY_ITEMS: [&str; 4] = ["100%", "75%", "50%", "25%"];
const OPACITY_VALUES: [u16; 4] = [100, 75, 50, 25];
const AREA_ITEMS: [&str; 4] = ["全屏", "3/4 屏", "半屏", "1/4 屏"];
const AREA_VALUES: [u16; 4] = [100, 75, 50, 25];
const DELAY_ITEMS: [&str; 7] = [
    "关闭（不补偿）",
    "2.0 秒",
    "2.5 秒",
    "3.0 秒",
    "3.5 秒",
    "4.0 秒",
    "5.0 秒",
];
const DELAY_VALUES: [u16; 7] = [0, 2000, 2500, 3000, 3500, 4000, 5000];
const DENSITY_ITEMS: [&str; 4] = ["不限", "正常", "较少", "最少"];
const DENSITY_VALUES: [u16; 4] = [400, 150, 80, 40];
const MERGE_ITEMS: [&str; 2] = ["逐条显示", "合并重复（×N）"];
const MERGE_VALUES: [u16; 2] = [0, 1];
const MERGE_WINDOW_ITEMS: [&str; 4] = ["3 秒", "5 秒", "8 秒", "10 秒"];
const MERGE_WINDOW_VALUES: [u16; 4] = [3000, 5000, 8000, 10000];
const LINE_GAP_ITEMS: [&str; 7] = ["60%", "80%", "100%", "120%", "150%", "200%", "250%"];
const LINE_GAP_VALUES: [u16; 7] = [60, 80, 100, 120, 150, 200, 250];

const SC_MODE_ITEMS: [&str; 4] = ["完整卡片", "折叠成一行", "只留角标", "不显示"];
const SC_STACK_ITEMS: [&str; 7] = [
    "不限（放不下才滚）",
    "超过 1 张",
    "超过 2 张",
    "超过 3 张",
    "超过 4 张",
    "超过 6 张",
    "超过 12 张",
];
const SC_STACK_VALUES: [u16; 7] = [0, 1, 2, 3, 4, 6, 12];
const SC_CORNER_ITEMS: [&str; 4] = ["右上（同 B站）", "右下", "左上", "左下"];
const SC_SCALE_ITEMS: [&str; 6] = ["60%", "80%", "100%", "120%", "150%", "200%"];
const SC_SCALE_VALUES: [u16; 6] = [60, 80, 100, 120, 150, 200];
const SC_DURATION_ITEMS: [&str; 7] = [
    "20%",
    "50%",
    "100%",
    "150%",
    "200%",
    "300%",
    "400%",
];
const SC_DURATION_VALUES: [u16; 7] = [20, 50, 100, 150, 200, 300, 400];

const MIN_LEVEL_ITEMS: [&str; 9] = [
    "不过滤（0）",
    "1 级",
    "5 级",
    "10 级",
    "20 级",
    "30 级",
    "40 级",
    "50 级",
    "60 级",
];
const MIN_LEVEL_VALUES: [u16; 9] = [0, 1, 5, 10, 20, 30, 40, 50, 60];
const MIN_MEDAL_ITEMS: [&str; 7] = ["不过滤（0）", "1 级", "5 级", "10 级", "20 级", "30 级", "40 级"];
const MIN_MEDAL_VALUES: [u16; 7] = [0, 1, 5, 10, 20, 30, 40];

const ADMISSION_ITEMS: [&str; 8] = [
    "自动（0）",
    "10 条/秒",
    "20 条/秒",
    "30 条/秒",
    "45 条/秒",
    "60 条/秒",
    "90 条/秒",
    "120 条/秒",
];
const ADMISSION_VALUES: [u16; 8] = [0, 10, 20, 30, 45, 60, 90, 120];
const QUEUE_WAIT_ITEMS: [&str; 7] = [
    "不等待（0）",
    "100 毫秒",
    "200 毫秒",
    "300 毫秒",
    "500 毫秒",
    "800 毫秒",
    "1000 毫秒",
];
const QUEUE_WAIT_VALUES: [u16; 7] = [0, 100, 200, 300, 500, 800, 1000];
const QUEUE_CAPACITY_ITEMS: [&str; 7] = ["1 条", "8 条", "16 条", "32 条", "64 条", "128 条", "200 条"];
const QUEUE_CAPACITY_VALUES: [u16; 7] = [1, 8, 16, 32, 64, 128, 200];
const CROWDED_SCALE_ITEMS: [&str; 7] = ["70%", "75%", "80%", "85%", "90%", "95%", "100%"];
const CROWDED_SCALE_VALUES: [u16; 7] = [70, 75, 80, 85, 90, 95, 100];
const EVENT_RATE_ITEMS: [&str; 5] = ["1 条/秒", "2 条/秒", "3 条/秒", "4 条/秒", "5 条/秒"];
const EVENT_RATE_VALUES: [u16; 5] = [1, 2, 3, 4, 5];
const MERGED_SCALE_ITEMS: [&str; 9] = [
    "100%", "110%", "120%", "130%", "140%", "150%", "160%", "170%", "180%",
];
const MERGED_SCALE_VALUES: [u16; 9] = [100, 110, 120, 130, 140, 150, 160, 170, 180];

fn sc_mode_slot(mode: ScMode) -> u16 {
    match mode {
        ScMode::Full => 0,
        ScMode::Compact => 1,
        ScMode::Badge => 2,
        ScMode::Off => 3,
    }
}

fn sc_mode_at(slot: u16) -> ScMode {
    match slot {
        1 => ScMode::Compact,
        2 => ScMode::Badge,
        3 => ScMode::Off,
        _ => ScMode::Full,
    }
}

fn sc_corner_slot(corner: ScCorner) -> u16 {
    match corner {
        ScCorner::TopRight => 0,
        ScCorner::BottomRight => 1,
        ScCorner::TopLeft => 2,
        ScCorner::BottomLeft => 3,
    }
}

fn sc_corner_at(slot: u16) -> ScCorner {
    match slot {
        1 => ScCorner::BottomRight,
        2 => ScCorner::TopLeft,
        3 => ScCorner::BottomLeft,
        _ => ScCorner::TopRight,
    }
}

impl Launcher {
    pub(super) fn danmaku_page(
        &mut self,
        ui: &mut Ui,
        host: Rect,
        _context: &egui::Context,
        pal: &Pal,
    ) {
        self.danmaku_tab_strip(ui, host, pal);
        let card_top = host.top() + FORM_TOP;
        let action_top = host.bottom() - 40.0;
        let viewport_h = (action_top - ACTION_GAP - card_top).max(160.0);
        let content = Rect::from_min_size(
            pos2(host.left(), card_top),
            vec2(host.width(), viewport_h),
        );
        let tab = self.danmaku_tab.min(TAB_TITLES.len() - 1);
        let mut area = child(ui, content);
        scroll_area(&mut area, SCROLL_ID, |ui| {
            let origin = ui.cursor().min;
            let width = ui.available_width().max(360.0);
            let height = match tab {
                0 => self.danmaku_tab_display(ui, origin, width, viewport_h, pal),
                1 => self.danmaku_tab_sc(ui, origin, width, pal),
                2 => self.danmaku_tab_filter(ui, origin, width, pal),
                3 => self.danmaku_tab_more(ui, origin, width, pal),
                _ => self.danmaku_tab_hotkeys(ui, origin, width, viewport_h, pal),
            };
            ui.allocate_space(vec2(width, height));
        });
        self.danmaku_action_row(ui, host, action_top, pal);
    }

    fn danmaku_tab_strip(&mut self, ui: &mut Ui, host: Rect, pal: &Pal) {
        let mut x = host.left() + TAB_LEFT;
        let y = host.top() + TAB_TOP;
        for (index, title) in TAB_TITLES.iter().enumerate() {
            let title: &str = title;
            let width = (measure(ui.painter(), title, &font(12.0)) + TAB_PAD).max(TAB_MIN_W);
            let rect = Rect::from_min_size(pos2(x, y), vec2(width, TAB_H));
            if pill(ui, rect, title, self.danmaku_tab == index, pal, true).clicked() {
                self.danmaku_tab = index;
            }
            x += width + TAB_GAP;
        }
    }

    fn danmaku_action_row(&mut self, ui: &mut Ui, host: Rect, action_top: f32, pal: &Pal) {
        let save_rect = Rect::from_min_size(
            pos2(host.right() - 8.0 - 104.0, action_top + 1.0),
            vec2(104.0, ACTION_H),
        );
        let reload_rect = Rect::from_min_size(
            pos2(save_rect.left() - 8.0 - 94.0, action_top + 1.0),
            vec2(94.0, ACTION_H),
        );
        let status_rect = Rect::from_min_size(
            pos2(host.left(), action_top + 5.0),
            vec2((reload_rect.left() - host.left() - 16.0).max(180.0), 24.0),
        );
        let dirty = self.danmaku_dirty();
        let (status, color) = if dirty {
            ("有未保存的更改。".to_owned(), pal.accent)
        } else {
            (
                "保存后用于新启动的覆盖层；旧配置字段自动兼容。".to_owned(),
                pal.text_secondary,
            )
        };
        text_in(
            ui.painter(),
            status_rect,
            status,
            font(11.0),
            color,
            Align2::LEFT_CENTER,
        );
        if button(ui, reload_rect, "重新读取", Btn::Secondary, pal, true).clicked() {
            self.danmaku_reload();
        }
        if button(
            ui,
            save_rect,
            "保存弹幕设置",
            Btn::Primary,
            pal,
            self.storage_pending == 0,
        )
        .clicked()
        {
            self.danmaku_save();
        }
    }

    fn danmaku_dirty(&self) -> bool {
        let saved = &self.snapshot.settings.overlay;
        if &self.overlay_draft != saved {
            return true;
        }
        if self.blocked_keyword_text != saved.behavior.blocked_keywords.join("\n") {
            return true;
        }
        if self.reserved_areas_text != reserved_areas_text(&saved.reserved_areas) {
            return true;
        }
        false
    }

    fn danmaku_save(&mut self) {
        let keywords: Vec<String> = self
            .blocked_keyword_text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .take(201)
            .collect();
        let areas = match parse_reserved_areas(&self.reserved_areas_text) {
            Ok(areas) => areas,
            Err(error) => {
                self.set_status(format!("画面安全区格式未保存：{error:#}"), true);
                return;
            }
        };
        if let Err(error) = self.overlay_draft.hotkeys.parse_all() {
            self.set_status(format!("热键未保存：{error}"), true);
            return;
        }
        let mut overlay = self.overlay_draft.clone();
        overlay.behavior.blocked_keywords = keywords;
        overlay.reserved_areas = areas;
        self.overlay_draft = overlay;
        self.reserved_areas_text = reserved_areas_text(&self.overlay_draft.reserved_areas);
        // 与 store 相同的完整校验（含 11 个热键的格式校验），在 UI 里先给中文错误。
        if let Err(error) = self.overlay_draft.validate() {
            self.set_status(format!("弹幕设置未保存：{error}"), true);
            return;
        }
        self.send_store(StoreAction::SaveOverlay(self.overlay_draft.clone()));
        self.set_status("已提交保存：弹幕与 SC 设置。", false);
    }

    fn danmaku_reload(&mut self) {
        self.overlay_draft = self.snapshot.settings.overlay.clone();
        self.blocked_keyword_text = self.overlay_draft.behavior.blocked_keywords.join("\n");
        self.blocked_user_text = self.overlay_draft.behavior.blocked_users.join("\n");
        self.reserved_areas_text = reserved_areas_text(&self.overlay_draft.reserved_areas);
        self.set_status("已重新读取已保存的弹幕设置。", false);
    }
}

// ---------------------------------------------------------------------------
// Local painting helpers (geometry taken from the C# workbench page).
// ---------------------------------------------------------------------------

fn danmaku_card(ui: &Ui, rect: Rect, pal: &Pal) {
    rounded(ui.painter(), rect, CARD_RADIUS, pal.card);
    rounded_stroke(
        ui.painter(),
        rect,
        CARD_RADIUS,
        Stroke::new(1.0_f32, pal.border),
    );
}

fn danmaku_heading(ui: &Ui, card: Rect, text: &str, pal: &Pal) {
    text_in(
        ui.painter(),
        Rect::from_min_size(
            pos2(card.left() + CARD_PAD, card.top() + CARD_HEAD_TOP),
            vec2((card.width() - CARD_PAD * 2.0).max(120.0), 20.0),
        ),
        text,
        bold(14.0),
        pal.text,
        Align2::LEFT_CENTER,
    );
}

/// Label rect (h 22) and dropdown rect (h 25) for one form cell.
fn form_slot(card: Rect, column: usize, row: usize) -> (Rect, Rect) {
    let x = card.left() + CARD_PAD + column as f32 * COL_DX;
    let y = card.top() + FORM_TOP + row as f32 * ROW_PITCH;
    (
        Rect::from_min_size(pos2(x, y), vec2(FIELD_W, 22.0)),
        Rect::from_min_size(pos2(x, y + 37.0), vec2(FIELD_W, DROPDOWN_H)),
    )
}

fn check_slot(card: Rect, column: usize, row: usize) -> Rect {
    let x = card.left() + CARD_PAD + column as f32 * CHECK_COL_DX;
    let y = card.top() + CHECK_TOP + row as f32 * CHECK_PITCH;
    Rect::from_min_size(pos2(x, y), vec2(FIELD_W.min(480.0), CHECK_H))
}

/// Numeric preset dropdown that also shows a custom current value instead of
/// silently snapping it to the nearest preset (the original page does the same
/// with a "（当前自定义值）" row).
fn combo_numeric(
    ui: &mut Ui,
    rect: Rect,
    id_salt: &str,
    items: &[&str],
    values: &[u16],
    current: u16,
    pal: &Pal,
) -> Option<u16> {
    let found = values.iter().position(|value| *value == current);
    let custom = found
        .is_none()
        .then(|| format!("{current}（当前自定义值）"));
    let mut labels: Vec<&str> = items.to_vec();
    if let Some(custom) = custom.as_deref() {
        labels.push(custom);
    }
    let mut selected = found.unwrap_or_else(|| labels.len().saturating_sub(1));
    let response = dropdown(ui, rect, id_salt, &mut selected, &labels, pal);
    if !response.changed() {
        return None;
    }
    Some(values.get(selected).copied().unwrap_or(current))
}

impl Launcher {
    /// Tab 1: 显示与内容. The form keeps the original 5-row two-column grid.
    fn danmaku_tab_display(
        &mut self,
        ui: &mut Ui,
        origin: Pos2,
        width: f32,
        available_h: f32,
        pal: &Pal,
    ) -> f32 {
        let card_h = available_h.max(462.0);
        let card = Rect::from_min_size(origin, vec2(width, card_h));
        danmaku_card(ui, card, pal);
        danmaku_heading(ui, card, "显示与内容", pal);

        let (label, control) = form_slot(card, 0, 0);
        field_label(ui, label, "弹幕字号", None, pal);
        if let Some(value) = combo_numeric(
            ui,
            control,
            "dm-font-size",
            &FONT_SIZE_ITEMS,
            &FONT_SIZE_VALUES,
            self.overlay_draft.font_size,
            pal,
        ) {
            self.overlay_draft.font_size = value;
        }
        let (label, control) = form_slot(card, 1, 0);
        field_label(ui, label, "弹幕速度", None, pal);
        if let Some(value) = combo_numeric(
            ui,
            control,
            "dm-speed",
            &SPEED_ITEMS,
            &SPEED_VALUES,
            self.overlay_draft.speed_percent,
            pal,
        ) {
            self.overlay_draft.speed_percent = value;
        }

        let (label, control) = form_slot(card, 0, 1);
        field_label(ui, label, "不透明度", None, pal);
        if let Some(value) = combo_numeric(
            ui,
            control,
            "dm-opacity",
            &OPACITY_ITEMS,
            &OPACITY_VALUES,
            self.overlay_draft.opacity_percent.into(),
            pal,
        ) {
            self.overlay_draft.opacity_percent = value as u8;
        }
        let (label, control) = form_slot(card, 1, 1);
        field_label(ui, label, "显示区域", None, pal);
        if let Some(value) = combo_numeric(
            ui,
            control,
            "dm-area",
            &AREA_ITEMS,
            &AREA_VALUES,
            self.overlay_draft.display_area_percent.into(),
            pal,
        ) {
            self.overlay_draft.display_area_percent = value as u8;
        }

        let (label, control) = form_slot(card, 0, 2);
        field_label(ui, label, "弹幕延迟补偿", None, pal);
        if let Some(value) = combo_numeric(
            ui,
            control,
            "dm-delay",
            &DELAY_ITEMS,
            &DELAY_VALUES,
            self.overlay_draft.behavior.delay_ms,
            pal,
        ) {
            self.overlay_draft.behavior.delay_ms = value;
        }
        let (label, control) = form_slot(card, 1, 2);
        field_label(ui, label, "弹幕密度", None, pal);
        if let Some(value) = combo_numeric(
            ui,
            control,
            "dm-density",
            &DENSITY_ITEMS,
            &DENSITY_VALUES,
            self.overlay_draft.max_visible,
            pal,
        ) {
            self.overlay_draft.max_visible = value;
        }

        let (label, control) = form_slot(card, 0, 3);
        field_label(ui, label, "重复弹幕", None, pal);
        if let Some(value) = combo_numeric(
            ui,
            control,
            "dm-merge",
            &MERGE_ITEMS,
            &MERGE_VALUES,
            u16::from(self.overlay_draft.behavior.merge_duplicates),
            pal,
        ) {
            self.overlay_draft.behavior.merge_duplicates = value == 1;
        }
        let (label, control) = form_slot(card, 1, 3);
        field_label(ui, label, "合并时间", None, pal);
        if let Some(value) = combo_numeric(
            ui,
            control,
            "dm-merge-window",
            &MERGE_WINDOW_ITEMS,
            &MERGE_WINDOW_VALUES,
            self.overlay_draft.behavior.merge_window_ms,
            pal,
        ) {
            self.overlay_draft.behavior.merge_window_ms = value;
        }

        let (label, control) = form_slot(card, 0, 4);
        field_label(ui, label, "行距", None, pal);
        if let Some(value) = combo_numeric(
            ui,
            control,
            "dm-line-gap",
            &LINE_GAP_ITEMS,
            &LINE_GAP_VALUES,
            self.overlay_draft.line_gap_percent,
            pal,
        ) {
            self.overlay_draft.line_gap_percent = value;
        }

        card_h
    }

    /// Tab 2: SC 醒目留言.
    fn danmaku_tab_sc(
        &mut self,
        ui: &mut Ui,
        origin: Pos2,
        width: f32,
        pal: &Pal,
    ) -> f32 {
        let card_h = 292.0;
        let card = Rect::from_min_size(origin, vec2(width, card_h));
        danmaku_card(ui, card, pal);
        danmaku_heading(ui, card, "SC 醒目留言", pal);

        let (label, control) = form_slot(card, 0, 0);
        field_label(ui, label, "SC 醒目留言", None, pal);
        if let Some(value) = combo_numeric(
            ui,
            control,
            "dm-sc-mode",
            &SC_MODE_ITEMS,
            &[0, 1, 2, 3],
            sc_mode_slot(self.overlay_draft.sc_mode),
            pal,
        ) {
            self.overlay_draft.sc_mode = sc_mode_at(value);
        }
        let (label, control) = form_slot(card, 1, 0);
        field_label(ui, label, "超过几张启用滚动", None, pal);
        if let Some(value) = combo_numeric(
            ui,
            control,
            "dm-sc-stack",
            &SC_STACK_ITEMS,
            &SC_STACK_VALUES,
            self.overlay_draft.sc_max_stack,
            pal,
        ) {
            self.overlay_draft.sc_max_stack = value;
        }

        let (label, control) = form_slot(card, 0, 1);
        field_label(ui, label, "SC 贴哪个角", None, pal);
        if let Some(value) = combo_numeric(
            ui,
            control,
            "dm-sc-corner",
            &SC_CORNER_ITEMS,
            &[0, 1, 2, 3],
            sc_corner_slot(self.overlay_draft.sc_corner),
            pal,
        ) {
            self.overlay_draft.sc_corner = sc_corner_at(value);
        }
        let (_, control) = form_slot(card, 1, 1);
        checkbox(
            ui,
            control,
            "显示 SC 头像",
            &mut self.overlay_draft.sc_show_avatar,
            pal,
        );

        let (label, control) = form_slot(card, 0, 2);
        field_label(ui, label, "SC 整体缩放", None, pal);
        if let Some(value) = combo_numeric(
            ui,
            control,
            "dm-sc-scale",
            &SC_SCALE_ITEMS,
            &SC_SCALE_VALUES,
            self.overlay_draft.sc_scale_percent,
            pal,
        ) {
            self.overlay_draft.sc_scale_percent = value;
        }
        let (label, control) = form_slot(card, 1, 2);
        field_label(ui, label, "SC 停留时间倍率", None, pal);
        if let Some(value) = combo_numeric(
            ui,
            control,
            "dm-sc-duration",
            &SC_DURATION_ITEMS,
            &SC_DURATION_VALUES,
            self.overlay_draft.sc_duration_scale_percent,
            pal,
        ) {
            self.overlay_draft.sc_duration_scale_percent = value;
        }

        let (_, click_control) = form_slot(card, 0, 3);
        checkbox(
            ui,
            click_control,
            "点 SC 面板切换形态 / 关闭",
            &mut self.overlay_draft.sc_clickable,
            pal,
        );
        let hint_rect = Rect::from_min_size(
            pos2(card.left() + CARD_PAD, card.top() + FORM_TOP + ROW_PITCH * 4.0 + 2.0),
            vec2((card.width() - CARD_PAD * 2.0).max(200.0), 18.0),
        );
        hint(
            ui,
            hint_rect,
            "超过指定张数后改为滚轮翻看，不会据此丢弃旧卡片；0 表示不按张数收窄。",
            pal,
        );
        card_h + ROW_PITCH
    }
}

impl Launcher {
    /// Tab 3: 过滤与开关. Two-column checkbox grid + level filters + blocked words.
    fn danmaku_tab_filter(
        &mut self,
        ui: &mut Ui,
        origin: Pos2,
        width: f32,
        pal: &Pal,
    ) -> f32 {
        let card_h = 540.0 + CHECK_PITCH * 2.0;
        let card = Rect::from_min_size(origin, vec2(width, card_h));
        danmaku_card(ui, card, pal);
        danmaku_heading(ui, card, "过滤与开关", pal);

        // Settings-level switch: launch the overlay with the player. Saved through
        // the settings path, not the overlay draft.
        let rect = check_slot(card, 0, 9);
        let mut auto_start = self.snapshot.settings.auto_start;
        if checkbox(ui, rect, "打开直播自动带起弹幕", &mut auto_start, pal).changed()
            && auto_start != self.snapshot.settings.auto_start
        {
            let mut settings = self.snapshot.settings.clone();
            settings.auto_start = auto_start;
            self.send_store(StoreAction::SaveSettings(settings));
        }

        let draft = &mut self.overlay_draft;
        let behavior = &mut draft.behavior;
        let rect = check_slot(card, 1, 9);
        checkbox(
            ui,
            rect,
            "始终压在所有窗口之上（仅覆盖层）",
            &mut draft.always_on_top,
            pal,
        );
        let rect = check_slot(card, 0, 10);
        checkbox(
            ui,
            rect,
            "「始终最前」时非前台隐藏",
            &mut draft.hide_when_not_foreground,
            pal,
        );

        let rect = check_slot(card, 0, 0);
        checkbox(ui, rect, "弹幕总开关", &mut draft.enabled, pal);
        let rect = check_slot(card, 1, 0);
        checkbox(ui, rect, "显示 SC 卡片", &mut draft.superchat_enabled, pal);
        let rect = check_slot(card, 0, 1);
        checkbox(ui, rect, "黑色描边", &mut draft.stroke_enabled, pal);
        let rect = check_slot(card, 1, 1);
        checkbox(ui, rect, "弹幕前显示粉丝勋章", &mut draft.show_medal, pal);
        let rect = check_slot(card, 0, 2);
        checkbox(ui, rect, "弹幕前显示用户名", &mut draft.show_user_name, pal);
        let rect = check_slot(card, 1, 2);
        checkbox(ui, rect, "显示用户等级 / 舰长", &mut draft.show_user_level, pal);
        let rect = check_slot(card, 0, 3);
        checkbox(ui, rect, "舰长 / 守护标识", &mut draft.show_guard, pal);
        let rect = check_slot(card, 1, 3);
        checkbox(ui, rect, "显示表情图片", &mut behavior.show_images, pal);
        let rect = check_slot(card, 0, 4);
        checkbox(ui, rect, "彩色 Unicode 表情", &mut draft.show_color_emoji, pal);
        let rect = check_slot(card, 1, 4);
        checkbox(
            ui,
            rect,
            "实心字芯（透明度仅影响 SC 背景）",
            &mut draft.solid_text,
            pal,
        );
        let rect = check_slot(card, 0, 5);
        checkbox(ui, rect, "字号随画面大小缩放", &mut draft.scale_with_window, pal);
        let rect = check_slot(card, 1, 5);
        checkbox(
            ui,
            rect,
            "播放器前台时启用弹幕快捷键",
            &mut behavior.hotkeys_enabled,
            pal,
        );
        let rect = check_slot(card, 0, 6);
        checkbox(
            ui,
            rect,
            "显示左下角快捷键提示行",
            &mut draft.show_shortcut_hint,
            pal,
        );
        let rect = check_slot(card, 1, 6);
        checkbox(ui, rect, "显示滚动弹幕", &mut behavior.show_scrolling, pal);
        let rect = check_slot(card, 0, 7);
        checkbox(ui, rect, "显示顶部固定弹幕", &mut behavior.show_top, pal);
        let rect = check_slot(card, 1, 7);
        checkbox(ui, rect, "显示底部固定弹幕", &mut behavior.show_bottom, pal);
        let rect = check_slot(card, 0, 8);
        checkbox(
            ui,
            rect,
            "显示逆向弹幕（B站模式 6）",
            &mut behavior.show_reverse,
            pal,
        );
        let rect = check_slot(card, 1, 8);
        checkbox(ui, rect, "保留平台弹幕颜色", &mut behavior.show_colored, pal);

        // Numeric level filters: label above the dropdown, like the form grid.
        let numeric_top = card.top() + CHECK_TOP + CHECK_PITCH * 11.0 + 10.0;
        for (column, label, items, values, current) in [
            (
                0usize,
                "最低用户等级（0=不过滤）",
                &MIN_LEVEL_ITEMS[..],
                &MIN_LEVEL_VALUES[..],
                u16::from(behavior.min_user_level),
            ),
            (
                1,
                "最低粉丝勋章等级（0=不过滤）",
                &MIN_MEDAL_ITEMS[..],
                &MIN_MEDAL_VALUES[..],
                u16::from(behavior.min_medal_level),
            ),
        ] {
            let x = card.left() + CARD_PAD + column as f32 * CHECK_COL_DX;
            let label_rect = Rect::from_min_size(pos2(x, numeric_top), vec2(FIELD_W, 22.0));
            field_label(ui, label_rect, label, None, pal);
            let control = Rect::from_min_size(
                pos2(x, numeric_top + 28.0),
                vec2(FIELD_W, DROPDOWN_H),
            );
            let id = if column == 0 { "dm-min-user" } else { "dm-min-medal" };
            if let Some(value) = combo_numeric(ui, control, id, items, values, current, pal) {
                if column == 0 {
                    behavior.min_user_level = value as u8;
                } else {
                    behavior.min_medal_level = value as u8;
                }
            }
        }

        let keyword_top = numeric_top + 28.0 + DROPDOWN_H + 16.0;
        let keyword_label = Rect::from_min_size(
            pos2(card.left() + CARD_PAD, keyword_top),
            vec2((card.width() - CARD_PAD * 2.0).max(200.0), 22.0),
        );
        field_label(
            ui,
            keyword_label,
            "屏蔽词（每行一个，包含匹配；re: 正则尚未支持）",
            None,
            pal,
        );
        let keyword_input = Rect::from_min_size(
            pos2(card.left() + CARD_PAD, keyword_top + 24.0),
            vec2((card.width() - CARD_PAD * 2.0).max(200.0), 92.0),
        );
        multiline(ui, keyword_input, &mut self.blocked_keyword_text, pal);
        card_h
    }

    /// Tab 4: 更多弹幕设置. Label-left / control-right rows, one card with three
    /// sections (original "更多弹幕设置" flat content).
    fn danmaku_tab_more(
        &mut self,
        ui: &mut Ui,
        origin: Pos2,
        width: f32,
        pal: &Pal,
    ) -> f32 {
        let card_h = 668.0;
        let card = Rect::from_min_size(origin, vec2(width, card_h));
        danmaku_card(ui, card, pal);
        danmaku_heading(ui, card, "更多弹幕设置", pal);

        let mut y = card.top() + 44.0;
        let label_w = MORE_LABEL_W;
        let ctrl_x = card.left() + MORE_CTRL_X;
        let ctrl_w = (card.width() - MORE_CTRL_X - CARD_PAD).max(140.0);

        more_group(ui, card, y, "拥挤与准入", pal);
        y += 26.0;
        more_label(ui, card, y, "平滑弹幕准入", label_w, pal);
        more_check(ui, ctrl_x, y, ctrl_w, "启用", &mut self.overlay_draft.behavior.smooth_admission, pal);
        y += MORE_ROW;
        more_label(ui, card, y, "每秒准入上限", label_w, pal);
        if let Some(value) = combo_numeric(
            ui,
            Rect::from_min_size(pos2(ctrl_x, y - 1.0), vec2(ctrl_w, DROPDOWN_H)),
            "dm-admission",
            &ADMISSION_ITEMS,
            &ADMISSION_VALUES,
            u16::from(self.overlay_draft.behavior.admission_per_second),
            pal,
        ) {
            self.overlay_draft.behavior.admission_per_second = value as u8;
        }
        y += MORE_ROW;
        more_label(ui, card, y, "短队列最大等待", label_w, pal);
        if let Some(value) = combo_numeric(
            ui,
            Rect::from_min_size(pos2(ctrl_x, y - 1.0), vec2(ctrl_w, DROPDOWN_H)),
            "dm-queue-wait",
            &QUEUE_WAIT_ITEMS,
            &QUEUE_WAIT_VALUES,
            self.overlay_draft.behavior.queue_wait_ms,
            pal,
        ) {
            self.overlay_draft.behavior.queue_wait_ms = value;
        }
        y += MORE_ROW;
        more_label(ui, card, y, "短队列容量", label_w, pal);
        if let Some(value) = combo_numeric(
            ui,
            Rect::from_min_size(pos2(ctrl_x, y - 1.0), vec2(ctrl_w, DROPDOWN_H)),
            "dm-queue-capacity",
            &QUEUE_CAPACITY_ITEMS,
            &QUEUE_CAPACITY_VALUES,
            u16::from(self.overlay_draft.behavior.queue_capacity),
            pal,
        ) {
            self.overlay_draft.behavior.queue_capacity = value as u8;
        }
        y += MORE_ROW;
        more_label(ui, card, y, "拥挤时缩小普通弹幕", label_w, pal);
        more_check(ui, ctrl_x, y, ctrl_w, "启用", &mut self.overlay_draft.behavior.shrink_when_crowded, pal);
        y += MORE_ROW;
        more_label(ui, card, y, "拥挤时字号", label_w, pal);
        if let Some(value) = combo_numeric(
            ui,
            Rect::from_min_size(pos2(ctrl_x, y - 1.0), vec2(ctrl_w, DROPDOWN_H)),
            "dm-crowded-scale",
            &CROWDED_SCALE_ITEMS,
            &CROWDED_SCALE_VALUES,
            u16::from(self.overlay_draft.behavior.crowded_scale_percent),
            pal,
        ) {
            self.overlay_draft.behavior.crowded_scale_percent = value as u8;
        }
        y += MORE_ROW;

        more_group(ui, card, y, "事件提示", pal);
        y += 26.0;
        more_label(ui, card, y, "富事件每秒上限", label_w, pal);
        if let Some(value) = combo_numeric(
            ui,
            Rect::from_min_size(pos2(ctrl_x, y - 1.0), vec2(ctrl_w, DROPDOWN_H)),
            "dm-event-rate",
            &EVENT_RATE_ITEMS,
            &EVENT_RATE_VALUES,
            u16::from(self.overlay_draft.behavior.event_rate_per_second),
            pal,
        ) {
            self.overlay_draft.behavior.event_rate_per_second = value as u8;
        }
        y += MORE_ROW;
        more_label(ui, card, y, "合并重复时随 ×N 放大", label_w, pal);
        more_check(ui, ctrl_x, y, ctrl_w, "启用", &mut self.overlay_draft.behavior.enlarge_merged, pal);
        y += MORE_ROW;
        more_label(ui, card, y, "合并放大上限", label_w, pal);
        if let Some(value) = combo_numeric(
            ui,
            Rect::from_min_size(pos2(ctrl_x, y - 1.0), vec2(ctrl_w, DROPDOWN_H)),
            "dm-merged-scale",
            &MERGED_SCALE_ITEMS,
            &MERGED_SCALE_VALUES,
            self.overlay_draft.behavior.merged_max_scale_percent,
            pal,
        ) {
            self.overlay_draft.behavior.merged_max_scale_percent = value;
        }
        y += MORE_ROW;
        more_label(ui, card, y, "送礼提示", label_w, pal);
        more_check(ui, ctrl_x, y, ctrl_w, "启用", &mut self.overlay_draft.behavior.show_gifts, pal);
        y += MORE_ROW;
        more_label(ui, card, y, "进场 / 关注提示", label_w, pal);
        more_check(ui, ctrl_x, y, ctrl_w, "启用", &mut self.overlay_draft.behavior.show_enters, pal);
        y += MORE_ROW;
        more_label(ui, card, y, "平台公告", label_w, pal);
        more_check(ui, ctrl_x, y, ctrl_w, "启用", &mut self.overlay_draft.behavior.show_notice, pal);
        y += MORE_ROW;
        more_label(ui, card, y, "看过 / 点赞 / 高能榜人数", label_w, pal);
        more_check(ui, ctrl_x, y, ctrl_w, "启用", &mut self.overlay_draft.behavior.show_live_stats, pal);
        y += MORE_ROW;

        more_group(ui, card, y, "画面安全区", pal);
        y += 26.0;
        more_label(
            ui,
            card,
            y,
            "安全区（每行 x,y,宽,高；0-100 整数百分比，最多 12 项）",
            card.width() - CARD_PAD * 2.0,
            pal,
        );
        y += 24.0;
        let areas = Rect::from_min_size(
            pos2(card.left() + CARD_PAD, y),
            vec2((card.width() - CARD_PAD * 2.0).max(200.0), 84.0),
        );
        multiline(ui, areas, &mut self.reserved_areas_text, pal);
        y += 84.0 + 8.0;
        hint(
            ui,
            Rect::from_min_size(pos2(card.left() + CARD_PAD, y), vec2(card.width() - CARD_PAD * 2.0, 18.0)),
            "拖动避让位置以百分比保存，缩放后自动跟随；格式错误会拒绝保存。",
            pal,
        );
        card_h
    }

    /// Tab 5: 界面与热键 — 界面字体/字号（需重启启动器生效）+ 11 个全局热键编辑器。
    ///
    /// 字体/字号写 core::Settings.ui_font_key / ui_font_percent（下拉即保存）；
    /// 热键编辑 OverlaySettings.hotkeys 的 11 个 spec，保存时统一校验
    /// （非法 / 重复 / 裸 F7-F10 保留键都会拒绝并提示）。
    fn danmaku_tab_hotkeys(
        &mut self,
        ui: &mut Ui,
        origin: Pos2,
        width: f32,
        _available_h: f32,
        pal: &Pal,
    ) -> f32 {
        let card_h = TAB5_CARD_H;
        let card = Rect::from_min_size(origin, vec2(width, card_h));
        danmaku_card(ui, card, pal);
        danmaku_heading(ui, card, "界面与热键", pal);

        let left_x = card.left() + CARD_PAD;
        let right_x = card.left() + CARD_PAD + COL_DX;
        let label_y = card.top() + FORM_TOP;

        // ---- 界面字体 / 界面字号（重启启动器生效）----
        field_label(
            ui,
            Rect::from_min_size(pos2(left_x, label_y), vec2(FIELD_W, 18.0)),
            "界面字体",
            None,
            pal,
        );
        field_label(
            ui,
            Rect::from_min_size(pos2(right_x, label_y), vec2(FIELD_W, 18.0)),
            "界面字号",
            None,
            pal,
        );
        let control_y = label_y + 20.0;
        let settings = self.snapshot.settings.clone();

        let key_labels: Vec<&str> = crate::gui::theme::UI_FONT_KEYS
            .iter()
            .map(|(_, label)| *label)
            .collect();
        let mut key_index = crate::gui::theme::ui_font_key_index(&settings.ui_font_key);
        let key_rect = Rect::from_min_size(pos2(left_x, control_y), vec2(FIELD_W, DROPDOWN_H));
        if dropdown(ui, key_rect, "ui-font-key", &mut key_index, &key_labels, pal).changed() {
            let key = crate::gui::theme::UI_FONT_KEYS
                .get(key_index)
                .map(|(value, _)| *value)
                .unwrap_or("auto");
            self.save_ui_font_setting(Some(key), None);
        }

        let percent_labels = ["85%", "100%", "115%", "130%"];
        let percent_rect = Rect::from_min_size(pos2(right_x, control_y), vec2(FIELD_W, DROPDOWN_H));
        if let Some(percent) = combo_numeric(
            ui,
            percent_rect,
            "ui-font-percent",
            &percent_labels,
            &crate::gui::theme::UI_FONT_PERCENT_STEPS,
            u16::from(settings.ui_font_percent),
            pal,
        ) {
            self.save_ui_font_setting(None, Some(percent));
        }

        let note_y = control_y + DROPDOWN_H + 10.0;
        let restart_rect = Rect::from_min_size(
            pos2(card.right() - CARD_PAD - 132.0, note_y),
            vec2(132.0, 28.0),
        );
        hint(
            ui,
            Rect::from_min_size(
                pos2(left_x, note_y),
                vec2((restart_rect.left() - left_x - 12.0).max(160.0), 28.0),
            ),
            "修改界面字体/字号后需重启启动器生效。",
            pal,
        );
        if button(ui, restart_rect, "重启启动器", Btn::Secondary, pal, true).clicked() {
            match self.restart_launcher() {
                Ok(()) => self.set_status("正在重启启动器…", false),
                Err(error) => self.set_status(format!("重启启动器失败：{error}"), true),
            }
        }

        // ---- 全局热键（11 项，留空禁用）----
        let hotkey_top = note_y + 28.0 + 14.0;
        text_in(
            ui.painter(),
            Rect::from_min_size(pos2(left_x, hotkey_top), vec2(560.0, 24.0)),
            "全局热键（播放器在前台时生效，留空表示禁用）",
            bold(12.0),
            pal.text,
            Align2::LEFT_CENTER,
        );
        let restore_rect = Rect::from_min_size(
            pos2(card.right() - CARD_PAD - 88.0, hotkey_top - 4.0),
            vec2(88.0, 26.0),
        );
        if button(ui, restore_rect, "恢复默认", Btn::Secondary, pal, true).clicked() {
            self.overlay_draft.hotkeys = HotkeyBindings::default();
            self.set_status("已恢复默认全局热键（尚未保存）。", false);
        }

        // 每帧只解析一次：把校验错误定位到具体编辑器行。
        let hotkey_error = self.overlay_draft.hotkeys.parse_all().err();
        let error_row = hotkey_error.as_ref().and_then(hotkey_error_row);
        let mut row_y = hotkey_top + 26.0;
        for (index, action) in HotkeyAction::ALL.iter().copied().enumerate() {
            let label_rect =
                Rect::from_min_size(pos2(left_x, row_y), vec2(HOTKEY_LABEL_W, 24.0));
            text_in(
                ui.painter(),
                label_rect,
                action.label_zh(),
                font(11.0),
                pal.text_secondary,
                Align2::LEFT_CENTER,
            );
            let input_rect = Rect::from_min_size(
                pos2(left_x + HOTKEY_INPUT_LEFT, row_y),
                vec2(HOTKEY_INPUT_W, 24.0),
            );
            let spec = hotkey_spec_mut(&mut self.overlay_draft.hotkeys, action);
            input(ui, input_rect, spec, "留空 = 不绑定", pal);
            if error_row == Some(index) {
                rounded_stroke(
                    ui.painter(),
                    input_rect,
                    5.0,
                    Stroke::new(1.6, pal.danger),
                );
                if let Some(error) = hotkey_error.as_ref() {
                    text_in(
                        ui.painter(),
                        Rect::from_min_size(
                            pos2(input_rect.right() + 10.0, row_y),
                            vec2(200.0, 24.0),
                        ),
                        hotkey_error_text(error),
                        font(10.0),
                        pal.danger,
                        Align2::LEFT_CENTER,
                    );
                }
            }
            row_y += HOTKEY_ROW;
        }
        hint(
            ui,
            Rect::from_min_size(
                pos2(left_x, row_y + 4.0),
                vec2((card.width() - CARD_PAD * 2.0).max(200.0), 18.0),
            ),
            "支持 Ctrl / Alt / Shift / Win + 单个按键；重复、非法组合或裸 F7–F10 会在保存时被拒绝。",
            pal,
        );

        let version_y = row_y + 26.0;
        text_in(
            ui.painter(),
            Rect::from_min_size(
                pos2(left_x, version_y),
                vec2((card.width() - CARD_PAD * 2.0).max(200.0), 18.0),
            ),
            format!(
                "看直播 · Rust 版 {}（设置保存在数据目录 state.json）",
                env!("CARGO_PKG_VERSION")
            ),
            font(11.0),
            pal.text_secondary,
            Align2::LEFT_CENTER,
        );
        card_h
    }

    /// 立即持久化界面字体/字号（Settings 字段；重启启动器后生效）。
    fn save_ui_font_setting(&mut self, key: Option<&str>, percent: Option<u16>) {
        let mut settings = self.snapshot.settings.clone();
        if let Some(key) = key {
            settings.ui_font_key = key.to_owned();
        }
        if let Some(percent) = percent {
            settings.ui_font_percent = crate::gui::theme::normalize_ui_font_percent(percent) as u8;
        }
        if settings.ui_font_key == self.snapshot.settings.ui_font_key
            && settings.ui_font_percent == self.snapshot.settings.ui_font_percent
        {
            return;
        }
        self.snapshot.settings = settings.clone();
        self.send_store(StoreAction::SaveSettings(settings));
        self.set_status("界面字体设置已保存：重启启动器后生效。", false);
    }
}

/// Mutable access to one hotkey spec by action (fields mirror core::HotkeyBindings).
fn hotkey_spec_mut(bindings: &mut HotkeyBindings, action: HotkeyAction) -> &mut String {
    match action {
        HotkeyAction::ToggleDanmaku => &mut bindings.toggle_danmaku,
        HotkeyAction::CycleSc => &mut bindings.cycle_sc,
        HotkeyAction::ToggleOverlay => &mut bindings.toggle_overlay,
        HotkeyAction::FontUp => &mut bindings.font_up,
        HotkeyAction::FontDown => &mut bindings.font_down,
        HotkeyAction::AreaUp => &mut bindings.area_up,
        HotkeyAction::AreaDown => &mut bindings.area_down,
        HotkeyAction::SpeedUp => &mut bindings.speed_up,
        HotkeyAction::SpeedDown => &mut bindings.speed_down,
        HotkeyAction::DelayLess => &mut bindings.delay_less,
        HotkeyAction::DelayMore => &mut bindings.delay_more,
    }
}

/// Which editor row does the current hotkey validation error belong to?
fn hotkey_error_row(error: &HotkeyConfigError) -> Option<usize> {
    match error {
        HotkeyConfigError::Parse { action, .. } | HotkeyConfigError::Reserved { action, .. } => {
            Some(action.index())
        }
        HotkeyConfigError::Duplicate { second, .. } => Some(second.index()),
    }
}

/// Short Chinese hint shown next to the offending hotkey row.
fn hotkey_error_text(error: &HotkeyConfigError) -> &'static str {
    match error {
        HotkeyConfigError::Parse { .. } => "格式无效",
        HotkeyConfigError::Duplicate { .. } => "与其他热键重复",
        HotkeyConfigError::Reserved { .. } => "与固定键 F7–F10 冲突",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_error_rows_map_to_editor_rows() {
        let error = HotkeyConfigError::Reserved {
            action: HotkeyAction::FontUp,
            spec: "F7".to_owned(),
        };
        assert_eq!(hotkey_error_row(&error), Some(HotkeyAction::FontUp.index()));
        assert_eq!(hotkey_error_text(&error), "与固定键 F7–F10 冲突");
        let duplicate = HotkeyConfigError::Duplicate {
            first: HotkeyAction::ToggleDanmaku,
            second: HotkeyAction::CycleSc,
            spec: "Ctrl+Alt+D".to_owned(),
        };
        assert_eq!(
            hotkey_error_row(&duplicate),
            Some(HotkeyAction::CycleSc.index())
        );
        let parse = HotkeyConfigError::Parse {
            action: HotkeyAction::DelayMore,
            spec: "Ctrl++".to_owned(),
            error: crate::hotkeys::parse_spec("Ctrl++").expect_err("invalid"),
        };
        assert_eq!(
            hotkey_error_row(&parse),
            Some(HotkeyAction::DelayMore.index())
        );
        assert_eq!(hotkey_error_text(&parse), "格式无效");
    }

    #[test]
    fn hotkey_spec_mut_edits_the_schema_fields() {
        let mut bindings = HotkeyBindings::default();
        for action in HotkeyAction::ALL {
            let spec = hotkey_spec_mut(&mut bindings, action);
            assert!(!spec.is_empty(), "default binding for {action:?} is empty");
            *spec = String::new();
        }
        for (name, value) in bindings.entries() {
            assert!(value.is_empty(), "{name} was not cleared");
        }
        assert!(
            bindings.parse_all().is_ok(),
            "all-empty bindings must validate (disabled)"
        );
    }

    #[test]
    fn default_hotkeys_validate_and_reject_reserved_or_duplicate() {
        let defaults = HotkeyBindings::default();
        assert_eq!(defaults.parse_all().expect("defaults").len(), 11);
        let mut reserved = defaults.clone();
        reserved.font_up = "F7".to_owned();
        let error = reserved.parse_all().expect_err("bare F7 is reserved");
        assert_eq!(hotkey_error_text(&error), "与固定键 F7–F10 冲突");
        let mut duplicate = defaults.clone();
        duplicate.cycle_sc = defaults.toggle_danmaku.clone();
        let error = duplicate.parse_all().expect_err("duplicate must fail");
        assert_eq!(hotkey_error_text(&error), "与其他热键重复");
        let mut invalid = defaults;
        invalid.area_up = "Ctrl+Alt+Nope".to_owned();
        let error = invalid.parse_all().expect_err("unknown key must fail");
        assert_eq!(hotkey_error_text(&error), "格式无效");
    }
}

fn more_group(ui: &Ui, card: Rect, top: f32, text: &str, pal: &Pal) {
    text_in(
        ui.painter(),
        Rect::from_min_size(pos2(card.left() + CARD_PAD, top), vec2(card.width() - CARD_PAD * 2.0, 24.0)),
        text,
        bold(12.0),
        pal.text,
        Align2::LEFT_CENTER,
    );
}

fn more_label(ui: &Ui, card: Rect, top: f32, text: &str, width: f32, pal: &Pal) {
    text_in(
        ui.painter(),
        Rect::from_min_size(pos2(card.left() + CARD_PAD, top), vec2(width, 24.0)),
        text,
        font(12.0),
        pal.text_secondary,
        Align2::LEFT_CENTER,
    );
}

fn more_check(
    ui: &mut Ui,
    x: f32,
    top: f32,
    width: f32,
    text: &str,
    value: &mut bool,
    pal: &Pal,
) {
    let rect = Rect::from_min_size(pos2(x, top), vec2(width, 24.0));
    checkbox(ui, rect, text, value, pal);
}
