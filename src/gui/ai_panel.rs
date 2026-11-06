use super::*;
impl Launcher {
    pub(super) fn ai_settings(&mut self, ui: &mut egui::Ui) {
        card(ui, self.dark, |ui| {
            ui.label(RichText::new("AI 搜索（可选）").size(18.0).strong());
            ui.label("仅使用你在本程序配置的 DeepSeek API Key，Windows DPAPI 加密保存。不会读取原版、DSH 或浏览器中的密钥。");
            if let Some(error) = &self.snapshot.ai_error {
                ui.colored_label(Color32::LIGHT_RED, error);
            }
            ui.label(if self.snapshot.ai_config.is_some() {
                "密钥已保存。只有主动点击 AI 搜索才会发送请求。"
            } else {
                "尚未配置，普通平台搜索仍可使用。"
            });
            path_field(ui, "模型", &mut self.ai_model, "deepseek-flash");
            ui.add(
                egui::Slider::new(&mut self.ai_max_tokens, 512..=8192).text("输出上限（tokens）"),
            );
            ui.add(
                egui::TextEdit::singleline(&mut self.ai_key_input)
                    .password(true)
                    .hint_text("API Key，留空保留已保存值"),
            );
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(
                        self.storage_pending == 0,
                        primary_button("加密保存 AI 设置"),
                    )
                    .clicked()
                {
                    let config = crate::ai::AiConfig {
                        model: self.ai_model.trim().into(),
                        max_tokens: self.ai_max_tokens,
                    };
                    let key = std::mem::take(&mut self.ai_key_input);
                    self.send_store(StoreAction::SaveAi(config, key));
                }
                if ui
                    .add_enabled(
                        self.storage_pending == 0 && self.snapshot.ai_config.is_some(),
                        egui::Button::new("清除本程序 AI 密钥"),
                    )
                    .clicked()
                {
                    self.send_store(StoreAction::ClearAi);
                }
                if ui.button("收起设置").clicked() {
                    self.lookup_ai_panel = false;
                }
            });
        });
    }
    pub(super) fn start_ai(&mut self, context: &egui::Context, optimize: bool) {
        if self.ai_busy {
            return;
        }
        if self.snapshot.ai_config.is_none() {
            self.set_status("请先点“DeepSeek 设置”配置自己的 API Key。", true);
            return;
        }
        let query = crate::search::SearchQuery::new(&self.search_input, &self.search_game, &self.search_identity, &self.search_alias).text();
        if query.is_empty() {
            self.set_status("请输入研究的主播昵称。", true);
            return;
        }
        self.cancel_search();
        self.search_has_more = false;
        self.ai_busy = true;
        let cancel = login::Cancellation::new();
        self.ai_cancel = Some(cancel.clone());
        let id = self.search_generation;
        let existing = self.search_results.clone();
        let vault = crate::ai::AiVault::new(self.snapshot.root.join("ai"));
        let sender = self.event_sender.clone();
        let context = context.clone();
        self.set_status("正在请求 DeepSeek 原生联网研究，普通平台结果会保留…", false);
        thread::spawn(move || {
            let result = vault
                .research(&query, &existing, optimize, &cancel)
                .map_err(|e| e.to_string());
            let _ = sender.send(Event::Ai {
                id,
                optimize,
                result,
            });
            context.request_repaint();
        });
    }
    /// The production AI research section rendered by the "查找主播" page:
    /// research actions, the paid-request disclosure and the persistent result
    /// (summary, usage, cited sources and candidates with a select-and-verify
    /// action). External links never reach the OS shell directly; they go through
    /// the validated open_external_url path.
    pub(super) fn ai_controls(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    !self.ai_busy && !self.search_results.is_empty(),
                    egui::Button::new("AI 优化现有结果"),
                )
                .clicked()
            {
                self.start_ai(context, true);
            }
            if ui
                .add_enabled(!self.ai_busy, egui::Button::new("AI 重新搜索"))
                .clicked()
            {
                self.start_ai(context, false);
            }
            if self.ai_busy {
                ui.spinner();
                if ui.button("取消 AI 等待").clicked() {
                    // Releasing the busy flag immediately also invalidates the
                    // in-flight request (cancel_search bumps the generation), so
                    // a late paid response can never repopulate a stale query.
                    self.cancel_search();
                    self.set_status("已请求取消；已发出的服务请求可能仍会计费。", false);
                }
                // Spinner animation is the only reason this section repaints
                // while a request is outstanding.
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(100));
            }
        });
        ui.label(RichText::new("主动点击 AI 会把当前查询与公开候选信息发送给 DeepSeek，并按你自己的接口账户计费；平台 Cookie 不发送，AI 不自动收藏或播放。").small().weak());
        match self.ai_result.clone() {
            Some(result) => {
                ui.separator();
                ui.label(RichText::new(&result.summary).small());
                ui.label(
                    RichText::new(format!(
                        "用量：输入 {} / 输出 {} tokens · {} 条可追溯来源",
                        result.input_tokens,
                        result.output_tokens,
                        result.sources.len()
                    ))
                    .small()
                    .weak(),
                );
                if !result.sources.is_empty() {
                    ui.label(RichText::new("可追溯来源（点击经系统浏览器打开）：").small().strong());
                    for source in &result.sources {
                        if ui
                            .button(format!("打开来源 · {}", source.title))
                            .on_hover_text(&source.url)
                            .clicked()
                        {
                            self.open_external_url(&source.url);
                        }
                    }
                }
                if result.candidates.is_empty() {
                    ui.label(RichText::new("本次没有返回可核验的新候选；原平台结果已保留。").small());
                } else {
                    ui.label(
                        RichText::new(format!("研究候选（{}）：", result.candidates.len()))
                            .small()
                            .strong(),
                    );
                    for candidate in &result.candidates {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new(format!(
                                    "{} · {} · {}",
                                    candidate.platform.label(),
                                    candidate.name,
                                    if candidate.room.is_empty() {
                                        "房间待解析".to_owned()
                                    } else {
                                        format!("房间 {}", candidate.room)
                                    }
                                ))
                                .small(),
                            );
                            if ui
                                .add_enabled(
                                    !self.verification_pending,
                                    egui::Button::new("选中并核验"),
                                )
                                .clicked()
                            {
                                self.select_and_verify_ai_candidate(candidate, context);
                            }
                        });
                    }
                }
                if !result.terms.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        for term in &result.terms {
                            if ui.button(format!("用线索“{term}”查询")).clicked() {
                                self.search_input = term.clone();
                                self.start_search(context, false);
                            }
                        }
                    });
                }
            }
            None => {
                ui.label(
                    RichText::new(if self.ai_busy {
                        "正在研究；已有平台结果会保留。"
                    } else {
                        "尚无 AI 研究结果。普通平台查询不会调用 AI；点击上方按钮才会发送请求。"
                    })
                    .small()
                    .weak(),
                );
            }
        }
    }

    /// Select the matching visible row (AI re-search results become the visible
    /// list) and start the same platform verification a native search row uses,
    /// so the verified-at add gate is reached by exactly one path.
    fn select_and_verify_ai_candidate(
        &mut self,
        candidate: &platforms::LookupCandidate,
        context: &egui::Context,
    ) {
        let position = self.search_results.iter().position(|row| {
            row.platform == candidate.platform
                && row.room == candidate.room
                && row.name == candidate.name
                && row.profile_url == candidate.profile_url
        });
        if let Some(index) = position {
            self.select_lookup_candidate(index);
            self.favorite_name = candidate.name.clone();
            self.lookup_filter = None;
            self.start_verification(candidate.clone(), context);
        } else {
            // Defensive fallback for a result kept from an older render (for
            // example after the list was replaced): the legacy AiVerified path
            // fills the room only after a successful platform verification.
            self.verification_pending = true;
            self.verification_generation = self.verification_generation.wrapping_add(1);
            let id = self.verification_generation;
            let root = self.snapshot.root.clone();
            let sender = self.event_sender.clone();
            let context = context.clone();
            let selected = candidate.clone();
            thread::spawn(move || {
                let result = CookieVault::open(root.join("credentials"))
                    .and_then(|vault| platforms::verify_authenticated(&selected, &vault))
                    .map_err(|error| format!("{error:#}"));
                let _ = sender.send(Event::AiVerified { id, result });
                context.request_repaint();
            });
        }
    }
}
