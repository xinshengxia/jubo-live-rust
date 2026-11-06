//! 本地数据 / 平台状态 page: the original two-tab read-only data table
//! (BuildData in livehub-workbench-data.cs) with the Rust diagnostics preview
//! kept inside the same structure.

use super::*;
use crate::gui::data_page::{
    self, DataCategory, DataSnapshot, DataTab, build_time_text, build_version, executable_modified,
    size_text,
};

const TAB_H: f32 = 44.0;
const CARD_H: f32 = 640.0;
const HEADER_H: f32 = 20.0;
const ROW_H: f32 = 52.0;

impl Launcher {
    pub(super) fn data_workbench_page(
        &mut self,
        ui: &mut Ui,
        host: Rect,
        _context: &egui::Context,
        pal: &Pal,
    ) {
        if self.data_snapshot.is_none() {
            self.refresh_data_snapshot();
        }
        let width = host.width();
        // ---- tab row -----------------------------------------------------
        let local_tab = Rect::from_min_size(host.min, vec2(72.0, 36.0));
        let selected_local = self.data_tab == DataTab::Local;
        if button(
            ui,
            local_tab,
            "本地数据",
            if selected_local {
                Btn::Secondary
            } else {
                Btn::Tab
            },
            pal,
            true,
        )
        .clicked()
        {
            self.data_tab = DataTab::Local;
        }
        let diagnostic_tab = Rect::from_min_size(host.min + vec2(80.0, 0.0), vec2(96.0, 36.0));
        if button(
            ui,
            diagnostic_tab,
            "平台运行状态",
            if selected_local {
                Btn::Tab
            } else {
                Btn::Secondary
            },
            pal,
            true,
        )
        .clicked()
        {
            self.data_tab = DataTab::Platform;
        }
        // ---- scrolling content below the tabs ----------------------------
        let body = Rect::from_min_size(
            host.min + vec2(0.0, TAB_H),
            vec2(width, (host.height() - TAB_H).max(140.0)),
        );
        let mut area = child(ui, body);
        scroll_area(&mut area, "data-page", |ui| {
            let origin = ui.cursor().min;
            match self.data_tab {
                DataTab::Local => self.data_local_card(ui, origin, pal),
                DataTab::Platform => self.data_platform_card(ui, origin, pal),
            }
        });
    }

    fn data_local_card(&mut self, ui: &mut Ui, origin: egui::Pos2, pal: &Pal) {
        let width = (ui.available_width() - 17.0).max(320.0);
        let card = Rect::from_min_size(origin, vec2(width, CARD_H));
        rounded(ui.painter(), card, 10.0, pal.card);
        rounded_stroke(ui.painter(), card, 10.0, Stroke::new(1.0, pal.border));
        let pad = 14.0;
        text_in(
            ui.painter(),
            Rect::from_min_size(card.min + vec2(pad, 12.0), vec2(400.0, 20.0)),
            "本地数据",
            font(12.0),
            pal.text,
            Align2::LEFT_CENTER,
        );
        text_in(
            ui.painter(),
            Rect::from_min_size(card.min + vec2(pad, 33.0), vec2(400.0, 20.0)),
            "收藏、历史、配置和运行文件",
            font(12.0),
            pal.text_secondary,
            Align2::LEFT_CENTER,
        );
        // Four header buttons (two Rust import entries + the original refresh
        // and open-folder actions). On a narrow window they move to a second row
        // so they never overlap the card title.
        let narrow = width < 720.0;
        let header_button_top = if narrow {
            card.top() + 54.0
        } else {
            card.top() + 10.0
        };
        let view = Rect::from_min_size(
            pos2(card.right() - pad - 96.0, header_button_top),
            vec2(96.0, 36.0),
        );
        let refresh = Rect::from_min_size(
            pos2(view.left() - 8.0 - 72.0, header_button_top),
            vec2(72.0, 36.0),
        );
        let import_legacy = Rect::from_min_size(
            pos2(refresh.left() - 8.0 - 110.0, header_button_top),
            vec2(110.0, 36.0),
        );
        let import_export = Rect::from_min_size(
            pos2(import_legacy.left() - 8.0 - 110.0, header_button_top),
            vec2(110.0, 36.0),
        );
        if button(
            ui,
            import_export,
            "导入原版数据…",
            Btn::Secondary,
            pal,
            self.storage_pending == 0,
        )
        .clicked()
        {
            self.import_original_data_dialog(ImportKind::FavoritesOrExport);
        }
        if button(
            ui,
            import_legacy,
            "导入原版历史…",
            Btn::Secondary,
            pal,
            self.storage_pending == 0,
        )
        .clicked()
        {
            self.import_original_data_dialog(ImportKind::LegacyHistory);
        }
        if button(ui, refresh, "刷新列表", Btn::Secondary, pal, true).clicked() {
            self.refresh_data_snapshot();
            self.set_status("已刷新本地数据只读列表。", false);
        }
        if button(ui, view, "查看本地数据", Btn::Secondary, pal, true).clicked() {
            match open_data_folder(&self.snapshot.root) {
                Ok(()) => self.set_status("已在资源管理器中打开本程序的数据目录。", false),
                Err(error) => self.set_status(format!("无法打开数据目录：{error:#}"), true),
            }
        }
        let table_x = card.left() + 17.0;
        let table_width = width - 34.0;
        let table_top = card.top() + if narrow { 121.0 } else { 81.0 };
        let columns = [
            ("数据类别", 0.19_f32),
            ("相对位置", 0.31),
            ("占用", 0.12),
            ("最近更新", 0.21),
            ("操作", 0.17),
        ];
        let header = Rect::from_min_size(pos2(table_x, table_top), vec2(table_width, HEADER_H));
        rounded(ui.painter(), header, 4.0, pal.hover);
        let mut column_x = [0.0_f32; 5];
        let mut x = table_x;
        for (index, (label, fraction)) in columns.into_iter().enumerate() {
            column_x[index] = x;
            text_in(
                ui.painter(),
                Rect::from_min_size(
                    pos2(x + 8.0, table_top),
                    vec2(table_width * fraction - 12.0, HEADER_H),
                ),
                label,
                font(11.0),
                pal.text_secondary,
                Align2::LEFT_CENTER,
            );
            x += table_width * fraction;
        }
        let snapshot = self.data_snapshot.clone().unwrap_or(DataSnapshot {
            categories: Vec::new(),
            warning: None,
        });
        let mut clicked_detail: Option<DataCategory> = None;
        for (index, category) in DataCategory::ALL.into_iter().enumerate() {
            let row = Rect::from_min_size(
                pos2(table_x, table_top + HEADER_H + 18.0 + index as f32 * ROW_H),
                vec2(table_width, ROW_H),
            );
            if index % 2 == 0 {
                rounded(ui.painter(), row, 4.0, with_alpha(pal.hover, 0.35));
            }
            let view_data = snapshot
                .categories
                .iter()
                .find(|view| view.category == category);
            let (count_text, location, bytes, modified) = match view_data {
                Some(view) => (
                    view.unit.clone(),
                    view.location.clone(),
                    view.bytes,
                    view.modified,
                ),
                None => ("读取中…".to_owned(), "—".to_owned(), 0, None),
            };
            let icons = ["★", "↺", "≡", "▤", "⌘"];
            let icon = icons.get(index).copied().unwrap_or("");
            text_in(
                ui.painter(),
                Rect::from_min_size(
                    pos2(column_x[0] + 8.0, row.top() + 8.0),
                    vec2(table_width * 0.19 - 12.0, 22.0),
                ),
                format!("{icon}  {}", category.title()),
                font(12.0),
                pal.text,
                Align2::LEFT_CENTER,
            );
            text_in(
                ui.painter(),
                Rect::from_min_size(
                    pos2(column_x[0] + 8.0, row.top() + 27.0),
                    vec2(table_width * 0.19 - 12.0, 20.0),
                ),
                &count_text,
                font(11.0),
                pal.text_secondary,
                Align2::LEFT_CENTER,
            );
            text_in(
                ui.painter(),
                Rect::from_min_size(
                    pos2(column_x[1] + 8.0, row.top()),
                    vec2(table_width * 0.31 - 12.0, ROW_H),
                ),
                &location,
                font(11.0),
                pal.text_secondary,
                Align2::LEFT_CENTER,
            );
            text_in(
                ui.painter(),
                Rect::from_min_size(
                    pos2(column_x[2] + 8.0, row.top()),
                    vec2(table_width * 0.12 - 12.0, ROW_H),
                ),
                if bytes == 0 {
                    "—".to_owned()
                } else {
                    size_text(bytes)
                },
                font(11.0),
                pal.text_secondary,
                Align2::LEFT_CENTER,
            );
            text_in(
                ui.painter(),
                Rect::from_min_size(
                    pos2(column_x[3] + 8.0, row.top()),
                    vec2(table_width * 0.21 - 12.0, ROW_H),
                ),
                data_page::modified_text(modified),
                font(11.0),
                pal.text_secondary,
                Align2::LEFT_CENTER,
            );
            let detail_button = Rect::from_min_size(
                pos2(column_x[4] + 8.0, row.top() + (ROW_H - 30.0) / 2.0),
                vec2(92.0, 30.0),
            );
            if button(ui, detail_button, "查看明细", Btn::Secondary, pal, true).clicked() {
                clicked_detail = Some(category);
            }
        }
        let note = match &snapshot.warning {
            Some(warning) => warning.clone(),
            None => "就绪 · 本地数据只读预览，未修改任何文件。".to_owned(),
        };
        text_in(
            ui.painter(),
            Rect::from_min_size(card.min + vec2(17.0, 425.0), vec2(width - 34.0, 19.0)),
            &note,
            font(11.0),
            if snapshot.warning.is_some() {
                pal.warning
            } else {
                pal.text_secondary
            },
            Align2::LEFT_CENTER,
        );
        // Maintenance strip.
        let maintenance =
            Rect::from_min_size(card.min + vec2(17.0, 466.0), vec2(width - 34.0, 74.0));
        rounded(ui.painter(), maintenance, 8.0, pal.hover);
        text_in(
            ui.painter(),
            Rect::from_min_size(
                maintenance.min + vec2(12.0, 10.0),
                vec2(maintenance.width() - 160.0, 20.0),
            ),
            "缓存与日志维护",
            font(12.0),
            pal.text,
            Align2::LEFT_CENTER,
        );
        text_in(
            ui.painter(),
            Rect::from_min_size(
                maintenance.min + vec2(12.0, 37.0),
                vec2(maintenance.width() - 160.0, 20.0),
            ),
            "清理运行缓存和日志，保留收藏、历史、配置和登录资料。",
            font(11.0),
            pal.text_secondary,
            Align2::LEFT_CENTER,
        );
        let cleanup = Rect::from_min_size(
            pos2(
                maintenance.right() - 12.0 - 108.0,
                maintenance.top() + (74.0 - 36.0) / 2.0,
            ),
            vec2(108.0, 36.0),
        );
        if button(
            ui,
            cleanup,
            "清理缓存和日志",
            Btn::Secondary,
            pal,
            self.storage_pending == 0,
        )
        .clicked()
        {
            self.cleanup_cache();
        }
        // Build info.
        let build_top = card.top() + 550.0;
        text_in(
            ui.painter(),
            Rect::from_min_size(pos2(table_x, build_top), vec2(table_width, 19.0)),
            "构建信息",
            font(12.0),
            pal.text,
            Align2::LEFT_CENTER,
        );
        text_in(
            ui.painter(),
            Rect::from_min_size(pos2(table_x, build_top + 27.0), vec2(table_width, 19.0)),
            format!("Build 版本：{}", build_version()),
            font(11.0),
            pal.text_secondary,
            Align2::LEFT_CENTER,
        );
        text_in(
            ui.painter(),
            Rect::from_min_size(pos2(table_x, build_top + 54.0), vec2(table_width, 19.0)),
            format!(
                "构建来源时间（UTC）：{}",
                build_time_text(executable_modified())
            ),
            font(11.0),
            pal.text_secondary,
            Align2::LEFT_CENTER,
        );
        // Rust diagnostics preview for the selected category.
        let mut height = CARD_H;
        if self.data_detail {
            // The detail drawer is taller than the old 190px stub: every row of
            // the selected category (not only the first eight) must be reachable
            // with its own scrollbar.
            let panel =
                Rect::from_min_size(card.min + vec2(17.0, 640.0), vec2(width - 34.0, 330.0));
            rounded(ui.painter(), panel, 8.0, pal.hover);
            let view = snapshot
                .categories
                .iter()
                .find(|view| view.category == self.data_category);
            let title = format!(
                "{} · 只读明细（{}）",
                self.data_category.title(),
                view.map(|view| view.note.as_str()).unwrap_or("—")
            );
            text_in(
                ui.painter(),
                Rect::from_min_size(
                    panel.min + vec2(12.0, 8.0),
                    vec2(panel.width() - 120.0, 20.0),
                ),
                &title,
                font(12.0),
                pal.text,
                Align2::LEFT_CENTER,
            );
            let close = Rect::from_min_size(
                pos2(panel.right() - 12.0 - 64.0, panel.top() + 8.0),
                vec2(64.0, 26.0),
            );
            if button(ui, close, "关闭", Btn::Secondary, pal, true).clicked() {
                self.data_detail = false;
            }
            let detail_rect = Rect::from_min_size(
                panel.min + vec2(8.0, 32.0),
                vec2(panel.width() - 16.0, 290.0),
            );
            let mut detail_ui = child(ui, detail_rect);
            detail_ui.set_clip_rect(detail_rect);
            egui::ScrollArea::vertical()
                .id_salt("workbench-all-data-details")
                .max_height(290.0)
                .show(&mut detail_ui, |ui| {
                    self.data_detail_card(ui);
                });
            height += 340.0;
        }
        if let Some(category) = clicked_detail {
            self.data_category = category;
            self.data_detail = true;
        }
        ui.allocate_space(vec2(
            (card.width() + 17.0).max(ui.available_width()),
            height,
        ));
    }

    fn data_platform_card(&mut self, ui: &mut Ui, origin: egui::Pos2, pal: &Pal) {
        let width = (ui.available_width() - 17.0).max(320.0);
        let now = health_now();
        let observations = self.observations.rows(self.platform, now);
        let mut height = 360.0;
        if self.data_detail {
            height += 220.0;
        }
        let card = Rect::from_min_size(origin, vec2(width, height));
        rounded(ui.painter(), card, 10.0, pal.card);
        rounded_stroke(ui.painter(), card, 10.0, Stroke::new(1.0, pal.border));
        let pad = 14.0;
        text_in(
            ui.painter(),
            Rect::from_min_size(card.min + vec2(pad, 12.0), vec2(300.0, 20.0)),
            "平台运行状态",
            font(12.0),
            pal.text,
            Align2::LEFT_CENTER,
        );
        let platform_items = [
            Platform::Bili.label(),
            Platform::Huya.label(),
            Platform::Douyu.label(),
            Platform::Douyin.label(),
        ];
        let mut selected = Platform::ALL
            .iter()
            .position(|platform| *platform == self.platform)
            .unwrap_or(0);
        let select = Rect::from_min_size(
            pos2(card.right() - pad - 112.0, card.top() + 10.0),
            vec2(112.0, 32.0),
        );
        if dropdown(
            ui,
            select,
            "data-platform",
            &mut selected,
            &platform_items,
            pal,
        )
        .changed()
        {
            self.platform = Platform::ALL[selected];
        }
        let table_x = card.left() + 17.0;
        let table_width = width - 34.0;
        let table_top = card.top() + 52.0;
        let header = Rect::from_min_size(pos2(table_x, table_top), vec2(table_width, 18.0));
        rounded(ui.painter(), header, 4.0, pal.hover);
        let columns = [("环节", 0.26_f32), ("最近结果", 0.54), ("更新时间", 0.20)];
        let mut x = table_x;
        let mut column_x = [0.0_f32; 3];
        for (index, (label, fraction)) in columns.into_iter().enumerate() {
            column_x[index] = x;
            text_in(
                ui.painter(),
                Rect::from_min_size(
                    pos2(x + 8.0, table_top),
                    vec2(table_width * fraction - 12.0, 18.0),
                ),
                label,
                font(11.0),
                pal.text_secondary,
                Align2::LEFT_CENTER,
            );
            x += table_width * fraction;
        }
        for (index, row) in observations.iter().enumerate() {
            let row_rect = Rect::from_min_size(
                pos2(table_x, table_top + 18.0 + index as f32 * 40.0),
                vec2(table_width, 40.0),
            );
            text_in(
                ui.painter(),
                Rect::from_min_size(
                    pos2(column_x[0] + 8.0, row_rect.top()),
                    vec2(table_width * 0.26 - 12.0, 40.0),
                ),
                row.stage.label(),
                font(11.0),
                pal.text_secondary,
                Align2::LEFT_CENTER,
            );
            text_in(
                ui.painter(),
                Rect::from_min_size(
                    pos2(column_x[1] + 8.0, row_rect.top()),
                    vec2(table_width * 0.54 - 12.0, 40.0),
                ),
                &row.result,
                font(11.0),
                if row.stale {
                    pal.warning
                } else {
                    pal.text_secondary
                },
                Align2::LEFT_CENTER,
            );
            text_in(
                ui.painter(),
                Rect::from_min_size(
                    pos2(column_x[2] + 8.0, row_rect.top()),
                    vec2(table_width * 0.20 - 12.0, 40.0),
                ),
                &row.time,
                font(11.0),
                pal.text_secondary,
                Align2::LEFT_CENTER,
            );
            ui.painter().hline(
                row_rect.x_range(),
                row_rect.bottom() - 0.5,
                Stroke::new(1.0, with_alpha(pal.border, 0.6)),
            );
        }
        let refresh = Rect::from_min_size(
            pos2(table_x, table_top + 18.0 + 3.0 * 40.0 + 8.0),
            vec2(96.0, 36.0),
        );
        let logs =
            Rect::from_min_size(pos2(refresh.right() + 8.0, refresh.top()), vec2(96.0, 36.0));
        if button(ui, refresh, "刷新运行状态", Btn::Secondary, pal, true).clicked() {
            self.set_status("已刷新本地观测；此页不发起平台请求。", false);
        }
        if button(ui, logs, "查看诊断日志", Btn::Secondary, pal, true).clicked() {
            self.data_detail = !self.data_detail;
        }
        text_in(
            ui.painter(),
            Rect::from_min_size(
                pos2(table_x, refresh.bottom() + 8.0),
                vec2(table_width, 19.0),
            ),
            "这里展示本地最近观测，不发起平台请求；历史结果不能代表当前状态。",
            font(11.0),
            pal.text_secondary,
            Align2::LEFT_CENTER,
        );
        if self.data_detail {
            let panel = Rect::from_min_size(
                pos2(table_x, refresh.bottom() + 36.0),
                vec2(table_width, 170.0),
            );
            rounded(ui.painter(), panel, 8.0, pal.hover);
            text_in(
                ui.painter(),
                Rect::from_min_size(
                    panel.min + vec2(12.0, 8.0),
                    vec2(panel.width() - 24.0, 20.0),
                ),
                "诊断日志 · 最近记录（已脱敏）",
                font(12.0),
                pal.text,
                Align2::LEFT_CENTER,
            );
            wrap_text(
                ui.painter(),
                Rect::from_min_size(
                    panel.min + vec2(12.0, 32.0),
                    vec2(panel.width() - 24.0, 130.0),
                ),
                &self.diagnostics.preview(),
                font(11.0),
                pal.text_secondary,
            );
        }
        ui.allocate_space(vec2(
            (card.width() + 17.0).max(ui.available_width()),
            height,
        ));
    }
}

impl Launcher {
    /// Import a C# launcher export (favourites array / structured document) or
    /// its platform-keyed history document through the shared dialogs.
    pub(super) fn import_original_data_dialog(&mut self, kind: ImportKind) {
        let filter = crate::gui::file_dialog::build_filter(&[("原版数据 JSON (*.json)", "*.json")]);
        let root = self.snapshot.root.clone();
        let title = match kind {
            ImportKind::FavoritesOrExport => "导入原版收藏/导出 JSON",
            ImportKind::LegacyHistory => "导入原版播放历史 JSON",
        };
        let Some(path) =
            crate::gui::file_dialog::open_file_name(&filter, title, Some(root.as_path()))
        else {
            self.set_status("已取消导入。", false);
            return;
        };
        self.send_store(StoreAction::Import(path, kind));
        self.set_status("正在导入原版数据…", false);
    }
}
