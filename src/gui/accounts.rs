use super::*;

pub(super) fn qr_image(payload: &str) -> Result<egui::ColorImage> {
    ensure!(payload.len() <= 4096, "二维码内容过长");
    let qr = qrcode::QrCode::new(payload.as_bytes()).map_err(|_| anyhow!("无法编码登录二维码"))?;
    let width = qr.width();
    let side = (width + 8) * 4;
    let mut image = egui::ColorImage::new([side, side], vec![Color32::WHITE; side * side]);
    for y in 0..width {
        for x in 0..width {
            if qr[(x, y)] == qrcode::Color::Dark {
                for dy in 0..4 {
                    for dx in 0..4 {
                        image.pixels[((y + 4) * 4 + dy) * side + (x + 4) * 4 + dx] = Color32::BLACK;
                    }
                }
            }
        }
    }
    Ok(image)
}

impl Launcher {
    pub(super) fn start_login(&mut self, context: &egui::Context, kind: u8) {
        if self.login_busy {
            return;
        }
        if kind == 2 {
            let now = health_now();
            if !login::check_allowed(
                self.health_attempts.get(&self.platform).copied(),
                now,
                login::MANUAL_CHECK_MIN_SECS,
            ) {
                self.login_message = format!(
                    "{} 刚刚检查过，请 {} 秒后再试；避免连续请求同一平台。",
                    self.platform.label(),
                    login::MANUAL_CHECK_MIN_SECS
                );
                return;
            }
            // A manual check supersedes a running automatic one: only one request
            // per platform may be in flight.
            self.health_generation = self.health_generation.wrapping_add(1);
            if let Some(cancel) = self.health_cancel.take() {
                cancel.cancel();
            }
            self.health_busy = false;
        }
        // A user-started login supersedes any automatic check: its result could
        // describe the credential being replaced right now.
        self.health_generation = self.health_generation.wrapping_add(1);
        if let Some(cancel) = self.health_cancel.take() {
            cancel.cancel();
        }
        self.health_busy = false;
        if kind == 2 {
            // The user just asked for this platform's check; do not repeat the same
            // request automatically the moment the login flow finishes.
            self.health_attempts.insert(self.platform, health_now());
        }
        self.login_generation = self.login_generation.wrapping_add(1);
        let id = self.login_generation;
        let platform = self.platform;
        let cancel = login::Cancellation::new();
        self.login_cancel = Some(cancel.clone());
        let capture = Arc::new(AtomicBool::new(false));
        self.login_capture = capture.clone();
        self.login_busy = true;
        self.login_qr = None;
        self.login_image = None;
        self.login_message = "正在准备登录流程…".into();
        let root = self.snapshot.root.clone();
        let browser = PathBuf::from(self.browser_path.trim());
        let sender = self.event_sender.clone();
        let context = context.clone();
        thread::spawn(move || {
            let send = |update| {
                let _ = sender.send(Event::Login {
                    id,
                    platform,
                    update,
                });
                context.request_repaint();
            };
            let result = (|| -> Result<String> {
                let vault = CookieVault::open(root.join("credentials"))?;
                if kind == 3 {
                    ensure!(
                        platform == Platform::Douyin,
                        "抖音网页验证入口仅支持抖音"
                    );
                    // The explicit window and the silent refresh share the same
                    // dedicated profile; reserve the one in-flight slot so two
                    // browsers never open it at the same time.
                    let Some(permit) = crate::douyin_guard::try_begin_user_verification(
                        crate::douyin_guard::now_ms(),
                    ) else {
                        return Err(anyhow!("抖音自动验证正在进行，请稍候再试。"));
                    };
                    let profile = crate::douyin_guard::verification_profile(&vault)?;
                    let mut window =
                        crate::douyin_guard::DouyinBrowser::new(browser.clone(), profile, true);
                    let cancel_check = || cancel.is_cancelled();
                    let mut saved = false;
                    let outcome = (|| -> Result<String> {
                        window.open_root_page(&cancel_check)?;
                        send(LoginUpdate::Progress(
                            "已打开本程序的抖音专属验证窗口（不会读取日常浏览器资料）；完成滑块或登录后点击“我已登录，检查并保存”。".into(),
                        ));
                        while !cancel.is_cancelled() {
                            if capture.swap(false, Ordering::AcqRel) {
                                match crate::douyin_guard::capture_visible(
                                    &mut window,
                                    &vault,
                                    &cancel_check,
                                ) {
                                    Ok(true) => {
                                        saved = true;
                                        send(LoginUpdate::Progress(
                                            "抖音网页验证已通过，Cookie 已加密保存。".into(),
                                        ));
                                        return Ok("抖音网页验证已通过，Cookie 已加密保存；请重新播放当前房间。".into());
                                    }
                                    Ok(false) => send(LoginUpdate::Progress(
                                        "尚未确认验证完成（可能仍是验证码中间页或缺少 ttwid）；请在窗口中完成验证后再次点击。".into(),
                                    )),
                                    Err(error) => send(LoginUpdate::Progress(format!(
                                        "暂未保存：{error}。完成正常验证后可重试。"
                                    ))),
                                }
                            }
                            thread::sleep(Duration::from_millis(200));
                        }
                        Ok("已取消抖音网页验证，原有凭据保持不变。".into())
                    })();
                    window.close_now();
                    permit.finish(saved, crate::douyin_guard::now_ms());
                    return outcome;
                }
                if kind == 2 {
                    let status = login::check_health(platform, &vault, &cancel)?;
                    let message = format!("{}：{}", platform.label(), status.label());
                    send(LoginUpdate::Health(status));
                    return Ok(message);
                }
                if kind == 0 {
                    ensure!(
                        platform == Platform::Bili,
                        "此二维码入口仅支持B站，其他平台请使用网页登录"
                    );
                    let mut session = login::BiliQrLogin::start(&cancel)?;
                    send(LoginUpdate::Qr(session.challenge().url));
                    while !cancel.is_cancelled() {
                        match session.poll(&vault, &cancel)? {
                            login::QrStatus::WaitingScan => send(LoginUpdate::Progress(
                                "请用哔哩哔哩手机应用扫描二维码。".into(),
                            )),
                            login::QrStatus::WaitingConfirm => {
                                send(LoginUpdate::Progress("已扫描，请在手机上确认登录。".into()))
                            }
                            login::QrStatus::VerificationPending(status) => {
                                send(LoginUpdate::Progress(status.reason))
                            }
                            login::QrStatus::LoggedIn(status) => {
                                send(LoginUpdate::Health(status));
                                return Ok("B站登录成功，凭据已加密保存。".into());
                            }
                            login::QrStatus::Expired => {
                                return Ok("二维码已到期，请重新获取。".into());
                            }
                            login::QrStatus::Cancelled => return Ok("已取消登录。".into()),
                        }
                        for _ in 0..10 {
                            if cancel.is_cancelled() {
                                break;
                            }
                            thread::sleep(Duration::from_millis(200));
                        }
                    }
                    return Ok("已取消登录。".into());
                }
                let mut session = login::browser::BrowserLogin::start(
                    platform,
                    &browser,
                    &root.join("browser-login"),
                    &cancel,
                )?;
                send(LoginUpdate::Progress("已打开本程序的独立登录窗口（Edge 隐私模式/Chrome 全新临时资料，不会登录或同步你的 Microsoft/Google 账号）。完成登录后点击“我已登录，检查并保存”。".into()));
                let result = (|| -> Result<String> {
                    while !cancel.is_cancelled() {
                        if capture.swap(false, Ordering::AcqRel) {
                            match session.capture_and_verify(&vault, &cancel) {
                                Ok(status) => {
                                    let valid = status.is_valid();
                                    let reason = status.reason.clone();
                                    send(LoginUpdate::Health(status));
                                    if valid {
                                        return Ok("网页登录已验证，凭据已加密保存。".into());
                                    }
                                    send(LoginUpdate::Progress(format!(
                                        "尚未确认登录：{reason}。可在浏览器完成后再检查。"
                                    )));
                                }
                                Err(error) => send(LoginUpdate::Progress(format!(
                                    "暂未保存：{error}。完成正常登录后可重试。"
                                ))),
                            }
                        }
                        thread::sleep(Duration::from_millis(200));
                    }
                    Ok("已取消网页登录，原有凭据保持不变。".into())
                })();
                let _ = session.close();
                result
            })();
            match result {
                Ok(message) => send(LoginUpdate::Done {
                    message,
                    error: false,
                }),
                Err(error) => send(LoginUpdate::Done {
                    message: if cancel.is_cancelled() {
                        "登录操作已取消。".into()
                    } else {
                        format!("登录未完成：{error}")
                    },
                    error: !cancel.is_cancelled(),
                }),
            }
        });
    }

    /// Cancels the current login/web-assist flow without saving anything. The
    /// owned browser window is closed by the worker when it observes the token.
    pub(super) fn cancel_login(&mut self) {
        if let Some(cancel) = &self.login_cancel {
            cancel.cancel();
        }
        self.login_generation = self.login_generation.wrapping_add(1);
        self.login_busy = false;
        self.web_assist_pending = false;
        self.login_qr = None;
        self.login_image = None;
        self.login_message = "已请求取消登录。".into();
    }

    /// Starts the Douyin page assist from the search page: a dedicated browser
    /// profile opens the official Douyin page, the user completes login/verification,
    /// then existing CDP capture + DPAPI save is reused. Only Douyin is accepted.
    pub(super) fn start_douyin_web_assist(&mut self, context: &egui::Context) {
        if self.login_busy {
            self.login_message = "已有登录或网页辅助窗口进行中，请先完成或取消。".into();
            return;
        }
        self.platform = Platform::Douyin;
        self.web_assist_pending = true;
        self.login_message =
            "正在打开抖音网页辅助窗口；请在窗口中完成登录或验证，然后点击“我已登录/验证，检查并保存”。"
                .into();
        self.start_login(context, 1);
    }

    /// Confirms saved logins with the platform's own endpoint, one platform at a
    /// time. It only runs while the accounts page is open, and only when the
    /// previous attempt for that platform is old enough. Network work stays on a
    /// worker thread; a stale result cannot overwrite a newer login flow.
    pub(super) fn maybe_start_health_checks(&mut self, context: &egui::Context) {
        if self.health_busy || self.login_busy {
            return;
        }
        let now = health_now();
        if self.health_all_requested {
            self.health_all_requested = false;
            // Remove attempt timestamps that are already past the manual floor so the
            // automatic loop may visit them now; recent attempts keep the 10 s limit.
            clear_due_health_attempts(&mut self.health_attempts, &self.snapshot.credentials, now);
        }
        let Some(platform) = next_health_check(
            &self.snapshot.credentials,
            &self.health_attempts,
            now,
            AUTO_HEALTH_INTERVAL_SECS,
        ) else {
            return;
        };
        // Record the attempt before the network call so a failure cannot retry
        // on every frame; the server-backed result may take several seconds.
        self.health_attempts.insert(platform, now);
        self.health_busy = true;
        let cancel = login::Cancellation::new();
        self.health_cancel = Some(cancel.clone());
        let generation = self.health_generation;
        let root = self.snapshot.root.clone();
        let stamp = credential_stamp(&root, platform);
        let sender = self.event_sender.clone();
        let context = context.clone();
        thread::spawn(move || {
            let result = (|| -> Result<login::LoginHealth> {
                let vault = CookieVault::open(root.join("credentials"))?;
                login::check_health(platform, &vault, &cancel)
            })()
            .map_err(|error| format!("{error:#}"));
            let _ = sender.send(Event::Health {
                generation,
                platform,
                stamp,
                result,
            });
            context.request_repaint();
        });
    }

    pub(super) fn accounts_page(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        if let Some(image) = self.login_image.take() {
            self.login_qr = Some(context.load_texture(
                "bilibili-login-qr",
                image,
                egui::TextureOptions::NEAREST,
            ));
        }
        self.platform_picker(ui);
        ui.add_space(14.0);
        card(ui, self.dark, |ui| {
            ui.label(RichText::new("登录与会话健康").size(20.0).strong());
            ui.label("扫码或在本程序独立浏览器中完成正常登录；完成后由平台接口核验。原版账号与浏览器资料不读取，网页登录不会同步你的 Microsoft/Google 账号。");
            ui.add_space(12.0);
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(
                        !self.login_busy && self.platform == Platform::Bili,
                        primary_button("B站扫码登录"),
                    )
                    .clicked()
                {
                    self.start_login(context, 0);
                }
                if ui
                    .add_enabled(!self.login_busy, egui::Button::new("打开网页登录"))
                    .clicked()
                {
                    self.start_login(context, 1);
                }
                let manual_allowed = login::check_allowed(
                    self.health_attempts.get(&self.platform).copied(),
                    health_now(),
                    login::MANUAL_CHECK_MIN_SECS,
                );
                if ui
                    .add_enabled(
                        !self.login_busy && manual_allowed,
                        egui::Button::new("检查此平台登录"),
                    )
                    .on_disabled_hover_text(format!(
                        "同一平台 {} 秒内只检查一次，避免连续请求",
                        login::MANUAL_CHECK_MIN_SECS
                    ))
                    .clicked()
                {
                    self.start_login(context, 2);
                }
                if ui
                    .add_enabled(
                        !self.login_busy && !self.health_busy,
                        egui::Button::new("检测全部已保存平台"),
                    )
                    .clicked()
                {
                    self.health_all_requested = true;
                    self.login_message = "将按平台顺序逐个检查已保存的凭据。".to_owned();
                }
            });
            if !login::check_allowed(
                self.health_attempts.get(&self.platform).copied(),
                health_now(),
                login::MANUAL_CHECK_MIN_SECS,
            ) {
                let last = self
                    .health_attempts
                    .get(&self.platform)
                    .copied()
                    .unwrap_or(0);
                ui.label(
                    RichText::new(format!(
                        "{} 刚检查过（{}）；请稍候再手动重试。",
                        self.platform.label(),
                        login::time_text(last)
                    ))
                    .small()
                    .weak(),
                );
            }
            if self.login_busy {
                ui.horizontal_wrapped(|ui| {
                    ui.spinner();
                    if ui.button("我已登录，检查并保存").clicked() {
                        self.login_capture.store(true, Ordering::Release);
                    }
                    if ui.button("取消本次登录").clicked() {
                        self.cancel_login();
                    }
                });
            }
            ui.label(&self.login_message);
            if let Some(texture) = &self.login_qr {
                ui.add(egui::Image::new(texture).fit_to_exact_size(egui::vec2(280.0, 280.0)));
            }
            ui.add_space(10.0);
            ui.label(
                RichText::new(
                    "已保存凭据的平台会自动检查登录状态，每 10 分钟最多一次；有效结果超过 10 分钟显示为“上次有效，待复查”。",
                )
                .size(12.0)
                .weak(),
            );
            if self.health_busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("正在自动检查已保存账号的登录状态…");
                });
            }
            let now = health_now();
            for platform in Platform::ALL {
                ui.add_space(4.0);
                if let Some(status) = self.login_health.get(&platform) {
                    ui.label(
                        RichText::new(format!(
                            "{} · {} · {}",
                            platform.label(),
                            status.current_label(now),
                            status.current_reason(now)
                        ))
                        .strong(),
                    );
                    ui.label(
                        RichText::new(format!(
                            "最近检测：{}{} · {}",
                            login::time_text(status.checked_at),
                            if status.verified_at > 0 {
                                format!("；上次明确有效：{}", login::time_text(status.verified_at))
                            } else {
                                String::new()
                            },
                            health_age_text(status.checked_at, now)
                        ))
                        .small()
                        .weak(),
                    );
                } else {
                    ui.label(format!(
                        "{} · {}",
                        platform.label(),
                        if self.snapshot.credentials.imported_for(platform) {
                            "凭据已保存，尚未向平台确认"
                        } else {
                            "尚未登录"
                        }
                    ));
                }
                ui.label(
                    RichText::new(self.observations.detail_text(platform, now))
                        .small()
                        .weak(),
                );
            }
            ui.collapsing("浏览器路径与凭据导入",|ui|{
                path_field(ui,"Edge / Chrome 可执行文件",&mut self.browser_path,"选择已安装的浏览器程序");
                ui.label("网页登录使用 Edge 隐私模式（无 Edge 时用 Chrome 独立临时资料），不会打开或扫描日常浏览器资料，也不会登录或同步你的 Microsoft/Google 账号。也可在设置页显式导入凭据文本。");
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn qr_has_quiet_border_and_bounded_size() {
        let image = qr_image("https://passport.example.test/synthetic").unwrap();
        assert!(image.size[0] >= 100 && image.size[0] < 1000);
        assert!(
            image
                .pixels
                .iter()
                .take(image.size[0] * 16)
                .all(|c| *c == Color32::WHITE)
        );
        assert!(image.pixels.contains(&Color32::BLACK));
        assert!(qr_image(&"x".repeat(5000)).is_err());
    }
}