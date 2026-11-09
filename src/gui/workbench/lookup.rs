//! 查找主播 page: query bar, action row, platform result filter, candidate list,
//! details / sources / excerpt panels and the favourite action row
//! (livehub-lookup-ui.cs and the workbench lookup layout).

use super::*;
use crate::search::{
    SearchQuery, account_page, candidate_room_page, candidate_stage, candidate_summary,
    relevance_label,
};

const ROW1_H: f32 = 34.0;
const ROW2_H: f32 = 34.0;
const GAP: f32 = 6.0;
/// Candidate row height and pitch: three text lines (name/platform, summary,
/// identity/verification) plus the 32px avatar, matching the original card.
const CANDIDATE_ROW_H: f32 = 64.0;
const CANDIDATE_ROW_PITCH: f32 = 68.0;
const LOOKUP_AVATAR_CACHE_LIMIT: usize = 128;
const LOOKUP_AVATAR_MAX_REQUESTS: usize = 4;
const LOOKUP_AVATAR_RETRY: Duration = Duration::from_secs(30);

/// UI-only text sanitizer for platform nicknames/signatures. The bundled UI
/// fonts have no colour-emoji coverage, so emoji would paint as tofu boxes
/// (the legacy WinForms app fell back to Segoe UI Emoji). Keep the stored
/// platform text untouched and drop only emoji presentation characters from
/// the rendered copy; CJK/ASCII/common punctuation are preserved.
fn display_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| {
            let code = *character as u32;
            !matches!(
                code,
                0x1F000..=0x1FAFF
                    | 0x2600..=0x27BF
                    | 0xFE00..=0xFE0F
                    | 0x200D
                    | 0x1F1E6..=0x1F1FF
            )
        })
        .collect()
}

impl Launcher {
    pub(super) fn lookup_page(
        &mut self,
        ui: &mut Ui,
        host: Rect,
        context: &egui::Context,
        pal: &Pal,
    ) {
        let width = host.width();
        let mut area = child(ui, host);
        scroll_area(&mut area, "lookup-page", |ui| {
            let origin = ui.cursor().min;
            if self.shot_mode {
                eprintln!("[lookup] origin={origin:?}");
            }
            // ---- row 1: platform scope, query, search / cancel -----------
            let platform_items = [
                "全部平台",
                Platform::Bili.label(),
                Platform::Huya.label(),
                Platform::Douyu.label(),
                Platform::Douyin.label(),
            ];
            let mut platform_index = if self.search_all {
                0
            } else {
                Platform::ALL
                    .iter()
                    .position(|platform| *platform == self.platform)
                    .map(|index| index + 1)
                    .unwrap_or(0)
            };
            let combo = Rect::from_min_size(origin + vec2(12.0, 12.0), vec2(94.0, ROW1_H));
            if dropdown(
                ui,
                combo,
                "lookup-scope",
                &mut platform_index,
                &platform_items,
                pal,
            )
            .changed()
            {
                self.cancel_search();
                if platform_index == 0 {
                    self.search_all = true;
                } else {
                    self.search_all = false;
                    self.platform = Platform::ALL[platform_index - 1];
                }
            }
            let search_width = (width - 94.0 - 80.0 - 86.0 - GAP * 4.0 - 6.0).max(120.0);
            let input_rect = Rect::from_min_size(
                pos2(combo.right() + 6.0, combo.top()),
                vec2(search_width, ROW1_H),
            );
            let input_response = input(
                ui,
                input_rect,
                &mut self.search_input,
                "输入主播名称、房间链接或分享文案",
                pal,
            );
            let query_rect = Rect::from_min_size(
                pos2(input_rect.right() + GAP, combo.top()),
                vec2(80.0, ROW1_H),
            );
            if button(ui, query_rect, "查询", Btn::Primary, pal, true).clicked()
                || (input_response.lost_focus()
                    && ui.input(|input| input.key_pressed(egui::Key::Enter)))
            {
                self.start_search(context, false);
            }
            let cancel_rect = Rect::from_min_size(
                pos2(query_rect.right() + GAP, combo.top()),
                vec2(86.0, ROW1_H),
            );
            if button(
                ui,
                cancel_rect,
                "取消",
                Btn::Secondary,
                pal,
                self.searching || self.verification_pending || self.ai_busy,
            )
            .clicked()
            {
                self.cancel_search();
                self.set_status("已取消当前查询；进行中的请求结果会被忽略。", false);
            }
            // ---- row 2: action buttons ----------------------------------
            let row2_top = origin.y + 54.0;
            // AI research actions live in the persistent AI section below (the
            // production ai_controls() renderer), so this row keeps only the
            // non-AI actions and fits narrow windows without wrapping.
            let actions: [(&str, f32, bool); 5] = [
                ("刷新结果", 80.0, !self.searching),
                ("补充线索", 80.0, true),
                ("抖音网页辅助", 96.0, !self.web_assist_pending),
                ("平台网页搜索", 96.0, !self.search_input.trim().is_empty()),
                ("DeepSeek 设置", 111.0, true),
            ];
            let mut x = 12.0_f32;
            let mut action_top = row2_top;
            for (index, (label, action_width, enabled)) in actions.into_iter().enumerate() {
                if x > 12.0 && x + action_width > width - 12.0 {
                    x = 12.0;
                    action_top += ROW2_H + GAP;
                }
                let rect =
                    Rect::from_min_size(pos2(origin.x + x, action_top), vec2(action_width.min(width - 24.0), ROW2_H));
                if button(ui, rect, label, Btn::Secondary, pal, enabled).clicked() {
                    match index {
                        0 => self.start_search(context, true),
                        1 => self.lookup_clues = !self.lookup_clues,
                        2 => self.start_douyin_web_assist(context),
                        3 => self.lookup_web_search_open = !self.lookup_web_search_open,
                        // A dialog is used so the API Key field is visible without
                        // scrolling the page or recognising a bare hint line.
                        4 => self.ai_settings_open = true,
                        _ => {}
                    }
                }
                x += action_width + GAP;
            }
            // y is page-relative from here on; row1+row2 occupy their own space.
            let mut y = action_top - origin.y + ROW2_H + 12.0;
            if self.lookup_web_search_open {
                for (index, platform) in Platform::ALL.into_iter().enumerate() {
                    let step = (width - 24.0) / 4.0;
                    let rect = Rect::from_min_size(origin + vec2(12.0 + index as f32 * step, y), vec2(step - GAP, 30.0));
                    if button(ui, rect, platform.label(), Btn::Secondary, pal, true).clicked() {
                        self.open_platform_search_for(platform);
                    }
                }
                y += 38.0;
            }
            // AI research/summary/usage/cited sources and the per-candidate
            // verify action are a persistent, scrollable section produced by the
            // production ai_controls() renderer. It must stay visible after the
            // result arrives instead of only on the click that started it.
            let ai_height = if self.ai_result.is_some() || self.ai_busy {
                320.0
            } else {
                104.0
            };
            let ai_panel =
                Rect::from_min_size(origin + vec2(0.0, y), vec2(width, ai_height));
            rounded(ui.painter(), ai_panel, 8.0, pal.card);
            rounded_stroke(ui.painter(), ai_panel, 8.0, Stroke::new(1.0, pal.border));
            let mut ai_ui = child(ui, ai_panel.shrink(10.0));
            egui::ScrollArea::vertical()
                .id_salt("lookup-ai-controls")
                .max_height(ai_height - 20.0)
                .show(&mut ai_ui, |ui| {
                    self.ai_controls(ui, context);
                });
            y += ai_height + 10.0;
            if self.lookup_clues {
                let clues = Rect::from_min_size(origin + vec2(0.0, y), vec2(width, 34.0));
                let field_width = (width - 2.0 * GAP - 120.0) / 3.0;
                for (index, (label, value)) in [
                    ("游戏/领域", &mut self.search_game),
                    ("身份线索", &mut self.search_identity),
                    ("外号/曾用名", &mut self.search_alias),
                ]
                .into_iter()
                .enumerate()
                {
                    let rect = Rect::from_min_size(
                        pos2(
                            clues.left() + index as f32 * (field_width + GAP),
                            clues.top(),
                        ),
                        vec2(field_width, 34.0),
                    );
                    input(ui, rect, value, label, pal);
                }
                if button(
                    ui,
                    Rect::from_min_size(
                        pos2(clues.right() - 120.0, clues.top()),
                        vec2(120.0, 34.0),
                    ),
                    "用线索查询",
                    Btn::Secondary,
                    pal,
                    true,
                )
                .clicked()
                {
                    self.start_search(context, false);
                }
                y += 34.0 + 8.0;
            }
            // ---- status line --------------------------------------------
            wrap_text(
                ui.painter(),
                Rect::from_min_size(origin + vec2(0.0, y), vec2(width, 46.0)),
                if self.search_caption.trim().is_empty() {
                    "输入昵称、官方直播间链接、抖音分享文案或账号线索：抖音号写“抖音号：123”，B站 UID 写“uid:123”，纯数字房间号请先选平台。"
                } else {
                    &self.search_caption
                },
                font(12.0),
                pal.text_secondary,
            );
            y += 46.0;
            // ---- result platform filter ---------------------------------
            text_in(
                ui.painter(),
                Rect::from_min_size(origin + vec2(12.0, y), vec2(68.0, 34.0)),
                "结果平台",
                font(12.0),
                pal.text_secondary,
                Align2::LEFT_CENTER,
            );
            let mut filter_x = 86.0;
            let total = self.search_results.len();
            let counts = |platform: Option<Platform>| {
                self.search_results
                    .iter()
                    .filter(|candidate| platform.is_none() || Some(candidate.platform) == platform)
                    .count()
            };
            let filters = [
                (None, format!("全部 ({total})")),
                (
                    Some(Platform::Bili),
                    format!("B站 ({})", counts(Some(Platform::Bili))),
                ),
                (
                    Some(Platform::Huya),
                    format!("虎牙 ({})", counts(Some(Platform::Huya))),
                ),
                (
                    Some(Platform::Douyu),
                    format!("斗鱼 ({})", counts(Some(Platform::Douyu))),
                ),
                (
                    Some(Platform::Douyin),
                    format!("抖音 ({})", counts(Some(Platform::Douyin))),
                ),
            ];
            for (platform, label) in filters.into_iter() {
                let rect = Rect::from_min_size(origin + vec2(filter_x, y), vec2(80.0, 34.0));
                if pill(ui, rect, &label, self.lookup_filter == platform, pal, true).clicked() {
                    self.lookup_filter = platform;
                    self.clear_lookup_selection();
                }
                filter_x += 80.0 + GAP;
            }
            y += 34.0 + 12.0;
            // ---- candidates + details / sources --------------------------
            let side_by_side = width >= 780.0;
            let list_width = if side_by_side { (width - 36.0) * 0.48 } else { width - 24.0 };
            let list = Rect::from_min_size(origin + vec2(12.0, y), vec2(list_width.max(80.0), 340.0));
            rounded(ui.painter(), list, 10.0, pal.card);
            rounded_stroke(ui.painter(), list, 10.0, Stroke::new(1.0, pal.border));
            text_in(
                ui.painter(),
                Rect::from_min_size(list.min + vec2(12.0, 6.0), vec2(list.width() - 24.0, 26.0)),
                "候选主播 · 选择后核验",
                font(12.0),
                pal.text_secondary,
                Align2::LEFT_CENTER,
            );
            let list_area = Rect::from_min_max(
                pos2(list.left() + 4.0, list.top() + 36.0),
                pos2(list.right() - 4.0, list.bottom() - 4.0),
            );
            self.lookup_candidates(ui, list_area, context, pal);
            let right_panel = if side_by_side {
                Rect::from_min_size(pos2(list.right() + 12.0, list.top()), vec2((width - list_width - 36.0).max(80.0), 340.0))
            } else {
                Rect::from_min_size(pos2(list.left(), list.bottom() + 12.0), vec2(list_width.max(80.0), 340.0))
            };
            self.lookup_evidence_panel(ui, right_panel);
            // ---- bottom favourite / room row ----------------------------
            // ---- bottom favourite / room row ----------------------------
            let bottom_y = right_panel.bottom() + 8.0;
            text_in(
                ui.painter(),
                Rect::from_min_size(pos2(origin.x + 12.0, bottom_y), vec2(68.0, 34.0)),
                "收藏名称",
                font(12.0),
                pal.text_secondary,
                Align2::LEFT_CENTER,
            );
            let name_rect = Rect::from_min_size(pos2(origin.x + 86.0, bottom_y), vec2(220.0, 34.0));
            input(
                ui,
                name_rect,
                &mut self.favorite_name,
                "留空使用房间号",
                pal,
            );
            let buttons = [
                ("加入收藏", 80.0),
                ("重新核验", 96.0),
                ("打开直播间", 84.0),
                ("账号主页", 80.0),
                ("查看所选来源", 96.0),
            ];
            let mut bx = name_rect.right() + GAP + 6.0;
            let mut action_y = bottom_y;
            for (index, (label, button_width)) in buttons.into_iter().enumerate() {
                let selected = self.selected_lookup_candidate().cloned();
                let enabled = match index {
                    // The add gate is the verified-at + target-validity check:
                    // selecting a row only starts verification, it never enables
                    // saving an unverified name/room pair.
                    0 => self.lookup_can_add(),
                    1 => selected.is_some(),
                    2 => selected.as_ref().and_then(candidate_room_page).is_some(),
                    // Only an official account page counts here; a direct room
                    // candidate stores its room page and must not be mislabeled
                    // as an account homepage.
                    3 => selected.as_ref().and_then(account_page).is_some(),
                    _ => self
                        .lookup_source
                        .is_some_and(|index| index < self.lookup_source_items().len()),
                };
                if bx + button_width > origin.x + width - 12.0 {
                    bx = origin.x + 12.0;
                    action_y += 34.0 + GAP;
                }
                let rect = Rect::from_min_size(pos2(bx, action_y), vec2(button_width, 34.0));
                if button(ui, rect, label, Btn::Secondary, pal, enabled).clicked() {
                    match index {
                        0 => match selected.as_ref() {
                            Some(candidate) if !candidate.room.trim().is_empty() => {
                                self.remember_candidate_avatar(candidate);
                                let name = if self.favorite_name.trim().is_empty() {
                                    candidate.name.clone()
                                } else {
                                    self.favorite_name.trim().to_owned()
                                };
                                match Target::new(candidate.platform, candidate.room.clone()) {
                                    Ok(target) => {
                                        self.send_store(StoreAction::AddFavorite(name, target));
                                    }
                                    Err(error) => {
                                        self.set_status(format!("不能收藏该候选：{error:#}"), true)
                                    }
                                }
                            }
                            _ => self.set_status("请先选择并完成平台核验。", true),
                        },
                        1 => {
                            if let Some(candidate) = selected.clone() {
                                self.start_verification(candidate, context);
                            }
                        }
                        2 => {
                            if let Some(url) = selected.as_ref().and_then(candidate_room_page) {
                                self.open_external_url(&url);
                            }
                        }
                        3 => {
                            if let Some(url) = selected.as_ref().and_then(account_page) {
                                self.open_external_url(&url);
                            }
                        }
                        _ => {
                            if let Some(url) = self
                                .lookup_source
                                .and_then(|index| self.lookup_source_items().get(index).cloned())
                                .map(|(_, url, _)| url)
                            {
                                self.open_external_url(&url);
                            }
                        }
                    }
                }
                bx += button_width + GAP;
            }
            let more = Rect::from_min_size(pos2(origin.x + 12.0, action_y + 44.0), vec2(132.0, 32.0));
            if button(ui, more, "加载下一页", Btn::Secondary, pal, self.search_has_more && !self.searching).clicked() {
                self.search_next_page(context);
            }
            text_in(ui.painter(), Rect::from_min_size(pos2(more.right() + 8.0, more.top()), vec2((width - 172.0).max(80.0), 32.0)),
                format!("已加载至第{}页；最多400个候选，新页自动去重。", self.search_page), font(11.0), pal.text_secondary, Align2::LEFT_CENTER);
            ui.allocate_space(vec2(width, action_y + 84.0 - origin.y));
        });
    }

    fn lookup_evidence_panel(&mut self, ui: &mut Ui, rect: Rect) {
        let mut area = child(ui, rect);
        egui::ScrollArea::vertical().id_salt("lookup-evidence-scroll").max_height(rect.height()).show(&mut area, |ui| {
            ui.set_width(rect.width().max(80.0) - 12.0);
            ui.strong("账号详情与核验");
            ui.label(self.lookup_details_text());
            ui.separator();
            ui.strong("搜索来源 · 选择可查看摘录");
            let sources = self.lookup_source_items();
            if sources.is_empty() { ui.label("暂无来源。平台核验或AI研究后会显示依据。"); }
            for (index, (title, _, _)) in sources.iter().enumerate() {
                if ui.selectable_label(self.lookup_source == Some(index), title).clicked() { self.lookup_source = Some(index); }
            }
            if let Some((title, address, excerpt)) = self.lookup_source.and_then(|index| sources.get(index)) {
                ui.separator();
                ui.strong("所选来源摘录");
                ui.label(title);
                let mut body = if excerpt.is_empty() { address.clone() } else { format!("{excerpt}\n\n{address}") };
                ui.add(egui::TextEdit::multiline(&mut body).desired_rows(7).desired_width(f32::INFINITY).interactive(false));
                if ui.button("打开所选来源").clicked() { self.open_external_url(address); }
            }
        });
    }

    fn lookup_details_text(&self) -> String {
        let now = health_now();
        if let Some(candidate) = self
            .lookup_selected
            .and_then(|index| self.search_results.get(index))
        {
            let mut text = String::new();
            text.push_str(&format!(
                "{} · {}\n",
                display_text(&candidate.name),
                candidate.platform.label()
            ));
            text.push_str(&display_text(&candidate_summary(candidate)));
            text.push('\n');
            text.push_str(candidate_stage(candidate));
            text.push('\n');
            if !candidate.title.is_empty() {
                text.push_str(&format!("直播间：{}\n", display_text(&candidate.title)));
            }
            if let Some(account) = &candidate.account_id {
                text.push_str(&format!("账号：{account}\n"));
            }
            if let Some(handle) = &candidate.account_handle {
                text.push_str(&format!("抖音号：{handle}\n"));
            }
            if !candidate.category.is_empty() {
                text.push_str(&format!("分区：{}\n", display_text(&candidate.category)));
            }
            if !candidate.description.is_empty() {
                // Keep the detail block one wrapped paragraph: raw newlines made
                // consecutive platform lines look stacked.
                let description = display_text(
                    &candidate
                        .description
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" "),
                );
                text.push_str(&format!("简介：{description}\n"));
            }
            if let Some(followers) = candidate.follower_count {
                text.push_str(&format!("粉丝：{}\n", followers_text(followers)));
            }
            if let Some(status) = candidate.live_status.as_deref() {
                let label = match status {
                    "live" => "直播中",
                    "offline" => "未开播",
                    "replay" => "轮播中",
                    other => other,
                };
                text.push_str(&format!("直播状态：{label}\n"));
            }
            if let Some(update) = candidate.platform_update.as_deref() {
                text.push_str(&format!("平台更新：{update}\n"));
            }
            if !candidate.match_reasons.is_empty() {
                text.push_str(&format!(
                    "命中依据：{}\n",
                    candidate.match_reasons.join("；")
                ));
            }
            if !candidate.verification_note.is_empty() {
                text.push_str(&format!("核验：{}\n", candidate.verification_note));
            }
            if candidate.verified_at > 0 {
                text.push_str(&format!(
                    "核验时间：{}\n",
                    login::time_text(candidate.verified_at)
                ));
            }
            if !candidate.sources.is_empty() {
                text.push_str(&format!("可追溯来源：{} 条\n", candidate.sources.len()));
            }
            if self.verification_pending {
                text.push_str("正在核验账号与直播间…\n");
            }
            if let Some(health) = self.login_health.get(&candidate.platform) {
                text.push_str(&format!(
                    "{} 登录态：{}\n",
                    candidate.platform.label(),
                    health.current_label(now)
                ));
            }
            text.push_str("候选不会自动选中或加入收藏。");
            text
        } else if self.search_results.is_empty() {
            "先输入主播名称或链接，再点击查询。\n\n选择候选后，这里显示账号、匹配理由与平台核验结果。\n候选不会自动选中或加入收藏。".to_owned()
        } else {
            "已返回候选：左侧选择候选后，这里显示账号、匹配理由与平台核验结果。\n选择即自动核验账号与直播间，候选不会自动加入收藏。".to_owned()
        }
    }

    fn lookup_candidates(&mut self, ui: &mut Ui, rect: Rect, context: &egui::Context, pal: &Pal) {
        let filter = self.lookup_filter;
        let query = SearchQuery::from_input(&self.search_input);
        let rows: Vec<(usize, platforms::LookupCandidate)> = self
            .search_results
            .iter()
            .enumerate()
            .filter(|(_, candidate)| filter.is_none() || Some(candidate.platform) == filter)
            .map(|(index, candidate)| (index, candidate.clone()))
            .collect();
        let mut area = child(ui, rect);
        let mut select: Option<usize> = None;
        let mut verify: Option<platforms::LookupCandidate> = None;
        scroll_area(&mut area, "lookup-candidates", |ui| {
            let origin = ui.cursor().min;
            if rows.is_empty() {
                text_in(
                    ui.painter(),
                    Rect::from_min_size(origin + vec2(8.0, 12.0), vec2(rect.width() - 16.0, 24.0)),
                    if self.searching {
                        "正在查询平台公开搜索接口…"
                    } else if self.search_results.is_empty() {
                        "先输入主播名称或链接，再点击查询。"
                    } else {
                        "当前筛选没有候选。"
                    },
                    font(12.0),
                    pal.text_secondary,
                    Align2::LEFT_TOP,
                );
            }
            let text_left = 50.0_f32;
            let text_width = (rect.width() - text_left - 12.0).max(60.0);
            for (position, (index, candidate)) in rows.iter().enumerate() {
                let row = Rect::from_min_size(
                    origin + vec2(2.0, 2.0 + position as f32 * CANDIDATE_ROW_PITCH),
                    vec2(rect.width() - 8.0, CANDIDATE_ROW_H),
                );
                let id = ui.id().with(("lookup-candidate", index));
                let response = ui.interact(row, id, Sense::click());
                let selected = self.lookup_selected == Some(*index);
                rounded(
                    ui.painter(),
                    row,
                    8.0,
                    if selected {
                        pal.selection_soft
                    } else if response.hovered() {
                        pal.hover
                    } else {
                        Color32::TRANSPARENT
                    },
                );
                if selected {
                    rounded_stroke(
                        ui.painter(),
                        row,
                        8.0,
                        Stroke::new(1.5, pal.selection_border),
                    );
                }
                let texture = self.lookup_avatar(ui.ctx(), candidate);
                avatar(
                    ui.painter(),
                    Rect::from_min_size(row.min + vec2(8.0, 16.0), Vec2::splat(32.0)),
                    texture.as_ref(),
                    &candidate.name,
                    pal,
                );
                // Three text lines per row, each elided to the row width so a long
                // platform title/description can never overflow into the next row.
                let name_font = bold(13.0);
                let name_text = elide(
                    ui.painter(),
                    &format!(
                        "{} · {}",
                        display_text(&candidate.name),
                        candidate.platform.label()
                    ),
                    &name_font,
                    text_width,
                );
                text_in(
                    ui.painter(),
                    Rect::from_min_size(row.min + vec2(text_left, 4.0), vec2(text_width, 20.0)),
                    name_text,
                    name_font,
                    pal.text,
                    Align2::LEFT_CENTER,
                );
                let summary_font = font(11.0);
                let summary = elide(
                    ui.painter(),
                    &display_text(&candidate_summary(candidate)),
                    &summary_font,
                    text_width,
                );
                text_in(
                    ui.painter(),
                    Rect::from_min_size(row.min + vec2(text_left, 24.0), vec2(text_width, 18.0)),
                    summary,
                    summary_font,
                    pal.text_secondary,
                    Align2::LEFT_CENTER,
                );
                let stage_font = font(10.5);
                let stage = elide(
                    ui.painter(),
                    &format!(
                        "{} · {}",
                        relevance_label(candidate, &query),
                        candidate_stage(candidate)
                    ),
                    &stage_font,
                    text_width,
                );
                text_in(
                    ui.painter(),
                    Rect::from_min_size(row.min + vec2(text_left, 44.0), vec2(text_width, 16.0)),
                    stage,
                    stage_font,
                    pal.text_secondary,
                    Align2::LEFT_CENTER,
                );
                if response.clicked() {
                    select = Some(*index);
                    // The legacy C# list filled the favourite-name box with the
                    // selected candidate name; keep that behaviour here.
                    self.favorite_name = candidate.name.clone();
                    // Remember the platform avatar URL for this room so favourites
                    // and later launches reuse it instead of re-resolving.
                    self.remember_candidate_avatar(candidate);
                    // The original workbench resolves the account and room as soon
                    // as a candidate is selected ("候选主播 · 选择后核验").
                    if self.lookup_selected != Some(*index) {
                        verify = Some(candidate.clone());
                    }
                }
                if response.double_clicked() {
                    verify = Some(candidate.clone());
                }
            }
            ui.allocate_space(vec2(
                rect.width(),
                (rows.len() as f32 * CANDIDATE_ROW_PITCH).max(1.0),
            ));
        });
        if let Some(index) = select {
            self.select_lookup_candidate(index);
        }
        if let Some(candidate) = verify {
            self.start_verification(candidate, context);
        }
    }

    /// Remember a candidate's platform avatar URL for its room (normalized and
    /// allowlisted by the avatar service). Used on selection, verification and
    /// before adding a favourite; no-op when either value is unavailable.
    fn remember_candidate_avatar(&mut self, candidate: &platforms::LookupCandidate) {
        let (Some(url), room) = (candidate.avatar_url.as_deref(), candidate.room.trim()) else {
            return;
        };
        if url.trim().is_empty() || room.is_empty() {
            return;
        }
        let _ = self
            .avatar_service
            .remember(candidate.platform, room, url.trim());
    }

    /// Download completion releases its slot even if the old row is never drawn
    /// again. Only small, worker-decoded thumbnails wait for the next UI frame.
    pub(crate) fn apply_lookup_avatar(
        &mut self,
        url: String,
        result: std::result::Result<image::RgbaImage, String>,
    ) {
        self.lookup_avatar_pending.remove(&url);
        match result {
            Ok(image)
                if image.dimensions()
                    == (crate::avatars::AVATAR_EDGE, crate::avatars::AVATAR_EDGE) =>
            {
                self.lookup_avatar_retry_at.remove(&url);
                if self.lookup_avatar_images.len() >= LOOKUP_AVATAR_CACHE_LIMIT {
                    self.lookup_avatar_images.pop_first();
                }
                self.lookup_avatar_images.insert(url, image);
            }
            _ => {
                if self.lookup_avatar_retry_at.len() >= LOOKUP_AVATAR_CACHE_LIMIT {
                    self.lookup_avatar_retry_at.pop_first();
                }
                self.lookup_avatar_retry_at
                    .insert(url, std::time::Instant::now() + LOOKUP_AVATAR_RETRY);
            }
        }
    }

    /// Texture for a candidate row. Room-stage candidates reuse the cached
    /// platform avatar service; account-only candidates load their reported
    /// avatar URL once, in the background, with the initial-letter placeholder
    /// kept on failure.
    fn lookup_avatar(
        &mut self,
        context: &egui::Context,
        candidate: &platforms::LookupCandidate,
    ) -> Option<egui::TextureHandle> {
        if !candidate.room.trim().is_empty() {
            // Room-stage rows use the platform avatar service (deduplicated disk
            // cache) exactly like favourites/recent cards; only account-stage rows
            // fall back to their reported avatar URL.
            return self.avatar_texture(context, candidate.platform, &candidate.room);
        }
        let url = candidate.avatar_url.as_deref()?.trim();
        if url.is_empty() {
            return None;
        }
        if let Some(texture) = self.lookup_avatar_textures.get(url) {
            return Some(texture.clone());
        }
        if let Some(decoded) = self.lookup_avatar_images.remove(url) {
            let size = [decoded.width() as usize, decoded.height() as usize];
            let image = egui::ColorImage::from_rgba_unmultiplied(size, decoded.as_raw());
            let texture = context.load_texture(url.to_owned(), image, egui::TextureOptions::LINEAR);
            if self.lookup_avatar_textures.len() >= LOOKUP_AVATAR_CACHE_LIMIT {
                self.lookup_avatar_textures.pop_first();
            }
            self.lookup_avatar_textures
                .insert(url.to_owned(), texture.clone());
            return Some(texture);
        }
        if self.lookup_avatar_pending.contains(url)
            || self.lookup_avatar_pending.len() >= LOOKUP_AVATAR_MAX_REQUESTS
            || self
                .lookup_avatar_retry_at
                .get(url)
                .is_some_and(|retry| *retry > std::time::Instant::now())
            || !self.badge_networking
        {
            return None;
        }
        let url = url.to_owned();
        self.lookup_avatar_pending.insert(url.clone());
        let sender = self.event_sender.clone();
        let context = context.clone();
        thread::spawn(move || {
            // The transport re-checks the HTTPS host allowlist and the size
            // bound; failures only keep the placeholder.
            let result = crate::avatars::AvatarTransport::fetch_detailed(
                &crate::avatars::OfficialTransport,
                &url,
            )
            .and_then(|bytes| {
                crate::avatars::decode_avatar(&bytes)
                    .ok_or(crate::avatars::AvatarFailure::InvalidImage)
            })
            .map_err(|error| format!("{error:?}"));
            let _ = sender.send(Event::LookupAvatar {
                url: url.clone(),
                result,
            });
            context.request_repaint();
        });
        None
    }

    /// One merged, ordered source list: AI research sources first, then the
    /// platform-verified sources reported for the selected candidate.
    fn lookup_source_items(&self) -> Vec<(String, String, String)> {
        let mut items: Vec<(String, String, String)> = Vec::new();
        if let Some(result) = &self.ai_result {
            for source in &result.sources {
                items.push((source.title.clone(), source.url.clone(), String::new()));
            }
        }
        if let Some(candidate) = self
            .lookup_selected
            .and_then(|index| self.search_results.get(index))
        {
            for source in &candidate.sources {
                if !items.iter().any(|(_, url, _)| *url == source.url) {
                    items.push((
                        source.title.clone(),
                        source.url.clone(),
                        source.excerpt.clone(),
                    ));
                }
            }
        }
        items
    }


    fn open_platform_search_for(&mut self, platform: Platform) {
        match crate::search::platform_search_url(platform, self.search_input.trim()) {
            Ok(url) => self.open_external_url(&url),
            Err(error) => self.set_status(error.to_string(), true),
        }
    }

    /// Opens one already-validated public link through the OS shell entry point
    /// (never through cmd/start). Exposed to the AI panel so every external link
    /// in the workbench shares this single validation path.
    pub(crate) fn open_external_url(&mut self, address: &str) {
        let result = crate::search::public_external_url(address).and_then(|address| {
            #[cfg(windows)]
            {
                use std::os::windows::ffi::OsStrExt;
                #[link(name = "shell32")]
                unsafe extern "system" {
                    fn ShellExecuteW(hwnd: isize, operation: *const u16, file: *const u16,
                        parameters: *const u16, directory: *const u16, show: i32) -> isize;
                }
                let operation: Vec<u16> = "open".encode_utf16().chain(Some(0)).collect();
                let target: Vec<u16> = std::ffi::OsStr::new(&address).encode_wide().chain(Some(0)).collect();
                let result = unsafe { ShellExecuteW(0, operation.as_ptr(), target.as_ptr(), std::ptr::null(), std::ptr::null(), 1) };
                anyhow::ensure!(result > 32, "系统无法打开此链接（错误码{result}）");
            }
            #[cfg(not(windows))]
            {
                let program = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
                std::process::Command::new(program).arg(&address).spawn().map_err(|_| anyhow::anyhow!("系统无法打开此链接"))?;
            }
            Ok(())
        });
        if let Err(error) = result { self.set_status(error.to_string(), true); }
    }

}

/// Shorten one line of text with an ellipsis so it cannot overflow its rect.
fn elide(painter: &Painter, text: &str, font: &FontId, max_width: f32) -> String {
    if max_width <= 0.0 {
        return String::new();
    }
    // Platform descriptions carry embedded newlines; a single-line row must not
    // lay them out as a multi-line galley (that painted over the next row).
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if measure(painter, &text, font) <= max_width {
        return text;
    }
    let mut out = String::new();
    for character in text.chars() {
        let mut candidate = out.clone();
        candidate.push(character);
        candidate.push('…');
        if measure(painter, &candidate, font) > max_width {
            break;
        }
        out.push(character);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_launcher() -> (Launcher, tempfile::TempDir, egui::Context) {
        let directory = tempfile::tempdir().unwrap();
        let context = egui::Context::default();
        let app = Launcher::from_context(
            &context,
            Store::open(directory.path().join("synthetic")).unwrap(),
        );
        (app, directory, context)
    }

    fn frame_input(width: f32, height: f32, events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(width, height))),
            events,
            ..Default::default()
        }
    }

    fn frame_input_at(time: f64, events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(1200.0, 700.0))),
            time: Some(time),
            events,
            ..Default::default()
        }
    }

    #[test]
    fn regression_avatar_completions_release_slots_without_redrawing_old_rows() {
        let (mut app, _directory, _context) = synthetic_launcher();
        for index in 0..64 {
            let url = format!("https://p3.douyinpic.com/synthetic-{index}.png");
            app.lookup_avatar_pending.insert(url.clone());
            app.event_sender
                .send(Event::LookupAvatar {
                    url,
                    result: Ok(image::RgbaImage::new(48, 48)),
                })
                .unwrap();
        }
        app.poll_events();
        assert!(app.lookup_avatar_pending.is_empty());
        assert_eq!(app.lookup_avatar_images.len(), 64);
        let failed = "https://p3.douyinpic.com/failed.png".to_owned();
        app.lookup_avatar_pending.insert(failed.clone());
        app.apply_lookup_avatar(failed.clone(), Err("synthetic failure".into()));
        assert!(app.lookup_avatar_pending.is_empty());
        assert!(app.lookup_avatar_retry_at[&failed] > std::time::Instant::now());
    }

    #[test]
    fn regression_avatar_ready_and_texture_caches_are_bounded_and_small() {
        let (mut app, _directory, context) = synthetic_launcher();
        let oversized = "https://p3.douyinpic.com/oversized.png".to_owned();
        app.apply_lookup_avatar(oversized.clone(), Ok(image::RgbaImage::new(4096, 1)));
        assert!(!app.lookup_avatar_images.contains_key(&oversized));
        for index in 0..256 {
            app.apply_lookup_avatar(
                format!("https://p3.douyinpic.com/queued-{index}.png"),
                Ok(image::RgbaImage::new(48, 48)),
            );
        }
        assert_eq!(app.lookup_avatar_images.len(), LOOKUP_AVATAR_CACHE_LIMIT);
        for index in 0..256 {
            let url = format!("https://p3.douyinpic.com/visible-{index}.png");
            app.apply_lookup_avatar(url.clone(), Ok(image::RgbaImage::new(48, 48)));
            let candidate = platforms::LookupCandidate {
                avatar_url: Some(url),
                ..Default::default()
            };
            let texture = app
                .lookup_avatar(&context, &candidate)
                .expect("small thumbnail texture");
            assert_eq!(texture.size(), [48, 48]);
        }
        assert_eq!(app.lookup_avatar_textures.len(), LOOKUP_AVATAR_CACHE_LIMIT);
        assert!(app.lookup_avatar_images.len() <= LOOKUP_AVATAR_CACHE_LIMIT);
    }

    #[test]
    fn regression_cancel_button_cancels_ai_and_releases_busy_state() {
        let (mut app, _directory, context) = synthetic_launcher();
        app.search_input = "synthetic".into();
        app.searching = true;
        app.verification_pending = true;
        app.ai_busy = true;
        let cancel = login::Cancellation::new();
        app.ai_cancel = Some(cancel.clone());
        let old_id = app.search_generation;
        let pos = egui::pos2(1150.0, 29.0);
        for (time, events) in [
            (0.0, vec![]),
            (
                0.1,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: Default::default(),
                    },
                ],
            ),
            (
                0.2,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: Default::default(),
                    },
                ],
            ),
        ] {
            let _ = context.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(1200.0, 700.0))),
                    time: Some(time),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default()
                        .frame(egui::Frame::NONE)
                        .show(ctx, |ui| {
                            app.lookup_page(
                                ui,
                                Rect::from_min_size(egui::Pos2::ZERO, vec2(1200.0, 700.0)),
                                ctx,
                                &Pal::of(theme::ThemeMode::Dark),
                            );
                        });
                },
            );
        }
        assert!(
            app.search_generation != old_id,
            "production cancel button must have been clicked"
        );
        assert!(cancel.is_cancelled());
        assert!(app.ai_cancel.is_none());
        assert!(!app.searching && !app.verification_pending && !app.ai_busy);
        app.event_sender
            .send(Event::Ai {
                id: old_id,
                optimize: false,
                result: Err("stale completion".into()),
            })
            .unwrap();
        app.poll_events();
        assert!(!app.ai_busy);
    }


    #[test]
    fn regression_cancel_button_cancels_ai_when_only_ai_is_busy() {
        let (mut app, _directory, context) = synthetic_launcher();
        app.search_input = "synthetic".into();
        app.ai_busy = true;
        let cancel = login::Cancellation::new();
        app.ai_cancel = Some(cancel.clone());
        let old_id = app.search_generation;
        let pos = egui::pos2(1150.0, 29.0);
        for (time, events) in [
            (0.0, vec![]),
            (
                0.1,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: Default::default(),
                    },
                ],
            ),
            (
                0.2,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: Default::default(),
                    },
                ],
            ),
        ] {
            let _ = context.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(1200.0, 700.0))),
                    time: Some(time),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default()
                        .frame(egui::Frame::NONE)
                        .show(ctx, |ui| {
                            app.lookup_page(
                                ui,
                                Rect::from_min_size(egui::Pos2::ZERO, vec2(1200.0, 700.0)),
                                ctx,
                                &Pal::of(theme::ThemeMode::Dark),
                            );
                        });
                },
            );
        }
        assert!(
            app.search_generation != old_id,
            "the production cancel button must work while only AI is busy"
        );
        assert!(cancel.is_cancelled());
        assert!(app.ai_cancel.is_none() && !app.ai_busy);
        app.event_sender
            .send(Event::Ai {
                id: old_id,
                optimize: false,
                result: Err("stale completion".into()),
            })
            .unwrap();
        app.poll_events();
        assert!(!app.ai_busy);
    }

    #[test]
    fn missing_ai_key_opens_the_settings_dialog_instead_of_only_reporting_status() {
        let (mut app, _directory, context) = synthetic_launcher();
        assert!(app.snapshot.ai_config.is_none());
        app.start_ai(&context, false);
        assert!(
            app.ai_settings_open,
            "a missing key must open a visible place to paste it"
        );
        assert!(!app.ai_busy, "no paid request may start without a key");
        assert!(
            app.status.contains("API Key"),
            "unexpected status: {}",
            app.status
        );
        // Render one real egui frame with the dialog open: the form (including
        // the labelled API Key input) must paint without panicking.
        let _ = context.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(1200.0, 700.0))),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ctx, |ui| {
                        app.lookup_page(
                            ui,
                            Rect::from_min_size(egui::Pos2::ZERO, vec2(1200.0, 700.0)),
                            ctx,
                            &Pal::of(theme::ThemeMode::Dark),
                        );
                    });
                app.ai_settings_dialog(ctx);
            },
        );
    }

    #[test]
    fn ai_settings_dialog_api_key_field_accepts_typed_text() {
        let (mut app, _directory, context) = synthetic_launcher();
        app.ai_settings_open = true;
        let key_field = egui::Id::new("lookup-ai-key-input");
        // Frame 1: build the dialog once, then focus the labelled key field.
        // Focus is requested after the window is painted: a request made before
        // the area exists is dropped by egui's per-viewport focus bookkeeping.
        let _ = context.run(frame_input(1200.0, 700.0, vec![]), |ctx| {
            app.ai_settings_dialog(ctx);
            ctx.memory_mut(|memory| memory.request_focus(key_field));
        });
        // Frame 2: the focused field consumes text exactly like real typing.
        let _ = context.run(
            frame_input(
                1200.0,
                700.0,
                vec![egui::Event::Text("sk-synthetic-key".into())],
            ),
            |ctx| app.ai_settings_dialog(ctx),
        );
        assert_eq!(app.ai_key_input, "sk-synthetic-key");
        // Frame 3: Escape closes the dialog instead of leaving hidden state.
        let escape = egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        };
        let _ = context.run(frame_input(1200.0, 700.0, vec![escape]), |ctx| {
            app.ai_settings_dialog(ctx)
        });
        assert!(!app.ai_settings_open);
    }

    #[test]
    fn format_help_dialog_renders_and_escape_closes_it() {
        let (mut app, _directory, context) = synthetic_launcher();
        app.format_help_open = true;
        let _ = context.run(frame_input(1240.0, 720.0, vec![]), |ctx| {
            app.format_help_dialog(ctx)
        });
        assert!(app.format_help_open, "opening frame must keep it visible");
        let escape = egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        };
        let _ = context.run(frame_input(1240.0, 720.0, vec![escape]), |ctx| {
            app.format_help_dialog(ctx)
        });
        assert!(!app.format_help_open, "Escape must close the help dialog");
    }

    #[test]
    fn deepseek_settings_button_opens_the_ai_dialog() {
        let (mut app, _directory, context) = synthetic_launcher();
        // Mirror the action-row maths: 12px start, widths [80,80,96,96,111],
        // 6px gap, second row at y=54 with a 34px height.
        let widths = [80.0_f32, 80.0, 96.0, 96.0, 111.0];
        let gap = 6.0_f32;
        let button = egui::pos2(
            12.0 + widths[..4].iter().sum::<f32>() + gap * 4.0 + widths[4] / 2.0,
            54.0 + 34.0 / 2.0,
        );
        let click = |pressed: bool, time: f64| {
            frame_input_at(
                time,
                vec![
                    egui::Event::PointerMoved(button),
                    egui::Event::PointerButton {
                        pos: button,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::default(),
                    },
                ],
            )
        };
        for input in [frame_input_at(0.0, vec![]), click(true, 0.1), click(false, 0.2)] {
            let _ = context.run(input, |ctx| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ctx, |ui| {
                        app.lookup_page(
                            ui,
                            Rect::from_min_size(egui::Pos2::ZERO, vec2(1200.0, 700.0)),
                            ctx,
                            &Pal::of(theme::ThemeMode::Dark),
                        );
                    });
            });
        }
        assert!(
            app.ai_settings_open,
            "clicking the DeepSeek 设置 button must open the dialog"
        );
        assert!(!app.ai_busy, "the settings action must not start a paid request");
    }

    #[test]
    fn ai_storage_result_is_surfaced_inside_the_dialog() {
        let (mut app, _directory, _context) = synthetic_launcher();
        app.ai_settings_open = true;
        app.ai_action_pending = true;
        let snapshot = app.snapshot.clone();
        app.event_sender
            .send(Event::Storage {
                snapshot,
                message: Err("合成保存失败".into()),
                quiet: false,
            })
            .unwrap();
        app.poll_events();
        assert_eq!(
            app.ai_dialog_message
                .as_ref()
                .map(|(message, error)| (message.as_str(), *error)),
            Some(("合成保存失败", true)),
            "AI save failures must be visible inside the dialog, not only in the footer"
        );
        assert!(!app.ai_action_pending);
        assert!(app.status_error);
    }

    #[test]
    fn page_switch_closes_both_page_dialogs() {
        let (mut app, _directory, _context) = synthetic_launcher();
        app.ai_settings_open = true;
        app.format_help_open = true;
        app.ai_dialog_message = Some(("合成结果".into(), false));
        app.show_page(Page::Data);
        assert!(!app.ai_settings_open);
        assert!(!app.format_help_open);
        assert!(
            app.ai_dialog_message.is_none(),
            "navigation must not replay the previous AI result on the next open"
        );
    }

    #[test]
    fn display_text_drops_emoji_but_keeps_cjk_and_punctuation() {
        assert_eq!(
            display_text("coke老师 🧣 直播：很不蒋道理"),
            "coke老师  直播：很不蒋道理"
        );
        assert_eq!(display_text("🛰️商务：cokey886"), "商务：cokey886");
        assert_eq!(display_text("普通昵称·B站"), "普通昵称·B站");
    }
}
