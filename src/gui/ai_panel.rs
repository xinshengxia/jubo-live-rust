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
            self.set_status("请先到设置页配置自己的 DeepSeek API Key。", true);
            return;
        }
        let query = self.search_input.trim().to_owned();
        if query.is_empty() {
            self.set_status("请输入研究的主播昵称。", true);
            return;
        }
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
                    if let Some(cancel) = &self.ai_cancel {
                        cancel.cancel()
                    }
                    self.set_status("已请求取消；已发出的服务请求可能仍会计费。", false);
                }
            }
        });
        ui.label(RichText::new("主动点击 AI 会将当前查询与公开候选信息发送给 DeepSeek，并按你自己的接口账户计费。平台 Cookie 不发送。AI 不自动收藏或播放。").small().weak());
        if let Some(result) = self.ai_result.clone() {
            ui.collapsing("AI 研究结果与来源", |ui| {
                ui.label(&result.summary);
                ui.label(format!(
                    "用量：输入 {} / 输出 {} tokens",
                    result.input_tokens, result.output_tokens
                ));
                for source in &result.sources {
                    ui.hyperlink_to(&source.title, &source.url);
                }
                for candidate in result.candidates {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(format!(
                            "{} · {} · {}",
                            candidate.platform.label(),
                            candidate.name,
                            if candidate.room.is_empty() {
                                "待解析房间".into()
                            } else {
                                candidate.room.clone()
                            }
                        ));
                        if ui
                            .add_enabled(
                                !self.verification_pending,
                                egui::Button::new("平台核验后填入"),
                            )
                            .clicked()
                        {
                            self.verification_pending = true;
                            let id = self.search_generation;
                            let root = self.snapshot.root.clone();
                            let sender = self.event_sender.clone();
                            let context = context.clone();
                            thread::spawn(move || {
                                let result = CookieVault::open(root.join("credentials"))
                                    .and_then(|vault| {
                                        platforms::verify_authenticated(&candidate, &vault)
                                    })
                                    .map_err(|e| e.to_string());
                                let _ = sender.send(Event::AiVerified { id, result });
                                context.request_repaint();
                            });
                        }
                    });
                }
                for term in result.terms {
                    if ui.button(format!("用线索“{term}”查询平台")).clicked() {
                        self.search_input = term;
                        self.start_search(context, false);
                    }
                }
            });
        }
        ui.add_space(14.0);
    }
}
