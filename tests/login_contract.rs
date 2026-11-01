use anyhow::Result;
use live_hub_rust::{
    auth::{CapturedCookie, CookieVault, LocalLoginState, captured_header},
    core::Platform,
    login::{
        self, BiliQrLogin, Cancellation, HealthState, LoginHealth, LoginReply, LoginTransport,
        QrStatus, browser,
    },
};
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use url::Url;

#[test]
fn browser_cookie_capture_is_scoped_to_selected_platform_urls() {
    for platform in Platform::ALL {
        let scopes = browser::capture_urls(platform);
        assert!(!scopes.is_empty());
        for scope in scopes {
            let url = Url::parse(&scope).unwrap();
            assert_eq!(url.scheme(), "https");
            let host = url.host_str().unwrap();
            assert!(match platform {
                Platform::Bili => host.ends_with(".bilibili.com"),
                Platform::Huya => host.ends_with(".huya.com"),
                Platform::Douyu => host.ends_with(".douyu.com"),
                Platform::Douyin => host.ends_with(".douyin.com"),
            });
            assert!(url.username().is_empty() && url.password().is_none());
        }
    }
}

fn reply(body: serde_json::Value) -> LoginReply {
    LoginReply {
        status: 200,
        body: body.to_string(),
        set_cookies: Vec::new(),
        location: None,
    }
}
fn generation() -> LoginReply {
    reply(
        json!({"code":0,"data":{"url":"https://passport.bilibili.com/h5-app/passport/login/scan?qrcode_key=SYNTHETIC_QR","qrcode_key":"SYNTHETIC_QR"}}),
    )
}

#[test]
fn official_account_qr_route_is_supported_without_widening_origin_or_key() {
    for address in [
        "https://account.bilibili.com/h5/account-h5/auth/scan-web?navhide=1&qrcode_key=SYNTHETIC_QR&from=web",
        "https://passport.bilibili.com/h5-app/passport/login/scan?qrcode_key=SYNTHETIC_QR",
    ] {
        login::validate_qr_challenge_url(address, "SYNTHETIC_QR").unwrap();
    }
    for address in [
        "http://account.bilibili.com/h5/account-h5/auth/scan-web?qrcode_key=SYNTHETIC_QR",
        "https://account.bilibili.com.attacker.invalid/h5/account-h5/auth/scan-web?qrcode_key=SYNTHETIC_QR",
        "https://account.bilibili.com/unrelated?qrcode_key=SYNTHETIC_QR",
        "https://account.bilibili.com/h5/account-h5/auth/scan-web?qrcode_key=DIFFERENT",
        "https://account.bilibili.com/h5/account-h5/auth/scan-web?qrcode_key=SYNTHETIC_QR&qrcode_key=SYNTHETIC_QR",
    ] {
        assert!(login::validate_qr_challenge_url(address, "SYNTHETIC_QR").is_err());
    }
}
fn poll(code: i64) -> LoginReply {
    reply(json!({"code":0,"data":{"code":code}}))
}
fn success() -> LoginReply {
    let mut result = poll(0);
    result.set_cookies = vec![
        "SESSDATA=SYNTHETIC_CONFIRMED; Domain=.bilibili.com; Path=/; Secure; HttpOnly".into(),
        "DedeUserID=123; Domain=.bilibili.com; Path=/; Secure".into(),
    ];
    result
}
fn nav(valid: bool) -> LoginReply {
    reply(json!({"code":0,"data":{"isLogin":valid,"mid":if valid{123}else{0}}}))
}

struct Mock {
    replies: Mutex<VecDeque<LoginReply>>,
    requests: Mutex<Vec<(String, Option<String>)>>,
}
impl Mock {
    fn new(replies: Vec<LoginReply>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(Vec::new()),
        }
    }
}
impl LoginTransport for Mock {
    fn get(&self, url: &Url, cookie: Option<&str>, _: &Cancellation) -> Result<LoginReply> {
        self.requests
            .lock()
            .unwrap()
            .push((url.path().into(), cookie.map(str::to_owned)));
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("synthetic transport exhausted"))
    }
}

#[test]
fn health_requires_server_evidence_and_distinguishes_restriction_from_expiry() {
    let cases = [
        (
            Platform::Bili,
            json!({"code":0,"data":{"isLogin":true,"mid":123}}),
        ),
        (Platform::Huya, json!({"isLogined":true,"uid":123})),
        (Platform::Douyu, json!({"error":0,"data":{}})),
        (
            Platform::Douyin,
            json!({"status_code":0,"user":{"uid":"123"}}),
        ),
    ];
    for (platform, body) in cases {
        assert_eq!(
            login::evaluate_health(platform, &reply(body)).state,
            HealthState::Valid
        );
        assert_eq!(
            login::evaluate_health(platform, &reply(json!({}))).state,
            HealthState::Unknown
        );
        let mut blocked = reply(json!({}));
        blocked.status = 403;
        assert_eq!(
            login::evaluate_health(platform, &blocked).state,
            HealthState::VerificationRequired
        );
        blocked.status = 503;
        assert_eq!(
            login::evaluate_health(platform, &blocked).state,
            HealthState::NetworkUnknown
        );
    }
    assert_eq!(
        login::evaluate_health(Platform::Bili, &nav(false)).state,
        HealthState::Invalid
    );
    assert_eq!(
        login::evaluate_health(
            Platform::Bili,
            &reply(json!({"code":0,"data":{"isLogin":true}}))
        )
        .state,
        HealthState::Unknown
    );
    assert_eq!(
        login::evaluate_health(
            Platform::Douyin,
            &reply(json!({"status_code":8,"status_msg":"用户未登录"}))
        )
        .state,
        HealthState::Invalid
    );
    assert_eq!(
        login::evaluate_health(
            Platform::Douyin,
            &reply(json!({"status_code":8,"status_msg":"synthetic limit"}))
        )
        .state,
        HealthState::Unknown
    );
}

#[test]
fn douyu_sso_redirect_requires_exact_https_callback_and_no_duplicate_parameters() {
    let mut response = reply(json!({}));
    response.status = 302;
    response.location=Some("https://www.douyu.com/api/passport/login?code=SYNTHETIC&uid=123&client_id=1&callback=livehubLoginCheck".into());
    assert_eq!(
        login::evaluate_health(Platform::Douyu, &response).state,
        HealthState::Valid
    );
    for bad in [
        "https://www.douyu.com.attacker.invalid/api/passport/login?code=x&uid=123&client_id=1&callback=livehubLoginCheck",
        "https://www.douyu.com/api/passport/login?code=x&uid=123&uid=456&client_id=1&callback=livehubLoginCheck",
        "http://www.douyu.com/api/passport/login?code=x&uid=123&client_id=1&callback=livehubLoginCheck",
    ] {
        response.location = Some(bad.into());
        assert_eq!(
            login::evaluate_health(Platform::Douyu, &response).state,
            HealthState::VerificationRequired
        );
    }
}

#[test]
fn response_cookie_scope_retains_host_path_expiration_and_deletion() {
    let origin = Url::parse("https://passport.bilibili.com/x/login/finish").unwrap();
    let mut jar = Vec::new();
    login::merge_set_cookie(
        Platform::Bili,
        &mut jar,
        &origin,
        "HOST=SYNTHETIC_HOST; Secure; HttpOnly",
        1000,
    )
    .unwrap();
    assert!(jar[0].host_only);
    assert_eq!(jar[0].path, "/x/login");
    assert!(
        captured_header(
            Platform::Bili,
            &jar,
            "https://api.bilibili.com/x/login/finish",
            1001
        )
        .unwrap()
        .is_none()
    );
    login::merge_set_cookie(Platform::Bili,&mut jar,&origin,"SESSDATA=SYNTHETIC_SHARED; Domain=.bilibili.com; Path=/; Max-Age=20; Expires=Thu, 01 Jan 1970 00:00:00 GMT; Secure",1000).unwrap();
    assert_eq!(
        captured_header(Platform::Bili, &jar, "https://api.bilibili.com/", 1019)
            .unwrap()
            .as_deref(),
        Some("SESSDATA=SYNTHETIC_SHARED")
    );
    assert!(
        captured_header(Platform::Bili, &jar, "https://api.bilibili.com/", 1020)
            .unwrap()
            .is_none()
    );
    login::merge_set_cookie(
        Platform::Bili,
        &mut jar,
        &origin,
        "SESSDATA=deleted; Domain=.bilibili.com; Path=/; Max-Age=0",
        1001,
    )
    .unwrap();
    assert!(!jar.iter().any(|cookie| cookie.name == "SESSDATA"));
    for header in [
        "SESSDATA=x; Domain=.huya.com; Path=/",
        "SESSDATA=x; Domain=.bilibili.com.attacker.invalid",
        "SESSDATA=x\r\nInjected:y",
        "__Host-SESSION=x; Domain=.bilibili.com; Path=/; Secure",
    ] {
        assert!(login::merge_set_cookie(Platform::Bili, &mut jar, &origin, header, 1001).is_err());
    }
}

#[test]
fn browser_endpoint_is_bound_to_literal_loopback_port_and_exact_owned_identity() {
    let path = "/devtools/browser/synthetic-123";
    assert!(
        browser::validate_browser_endpoint(
            "ws://127.0.0.1:12345/devtools/browser/synthetic-123",
            12345,
            path
        )
        .is_ok()
    );
    for bad in [
        "ws://localhost:12345/devtools/browser/synthetic-123",
        "ws://127.0.0.1:12346/devtools/browser/synthetic-123",
        "ws://127.0.0.1:12345/devtools/browser/other",
        "wss://127.0.0.1:12345/devtools/browser/synthetic-123",
        "ws://user:password@127.0.0.1:12345/devtools/browser/synthetic-123",
    ] {
        assert!(browser::validate_browser_endpoint(bad, 12345, path).is_err());
    }
}

#[test]
fn browser_capture_keeps_original_scope_filters_other_platforms_and_redacts_debug() {
    let result = json!({"cookies":[{"name":"SESSDATA","value":"SYNTHETIC_SECRET_VALUE","domain":".bilibili.com","path":"/","secure":true,"httpOnly":true,"expires":-1},{"name":"host","value":"SYNTHETIC_HOST","domain":"passport.bilibili.com","path":"/login","secure":true,"expires":-1},{"name":"sessionid","value":"SYNTHETIC_OTHER","domain":".douyin.com","path":"/","expires":-1}]});
    let captured = browser::decode_browser_cookies(Platform::Bili, &result).unwrap();
    assert_eq!(captured.len(), 2);
    assert!(!captured[0].host_only);
    assert!(captured[1].host_only);
    assert!(captured[0].http_only);
    assert!(!format!("{captured:?}").contains("SYNTHETIC_SECRET_VALUE"));
    assert!(browser::decode_browser_cookies(Platform::Huya, &result).is_err());
}

#[cfg(windows)]
#[test]
fn qr_wait_confirm_expire_and_cancel_never_change_existing_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let vault = CookieVault::open(temp.path().join("vault")).unwrap();
    vault
        .import(Platform::Bili, "SESSDATA=SYNTHETIC_OLD")
        .unwrap();
    let before = std::fs::read(vault.root().join("bili.cookies.dpapi")).unwrap();
    let transport = Arc::new(Mock::new(vec![
        generation(),
        poll(86101),
        poll(86090),
        poll(86038),
    ]));
    let cancel = Cancellation::new();
    let mut session = BiliQrLogin::start_with_transport(transport, &cancel).unwrap();
    assert!(!format!("{:?}", session.challenge()).contains("SYNTHETIC_QR"));
    assert_eq!(
        session.poll(&vault, &cancel).unwrap(),
        QrStatus::WaitingScan
    );
    assert_eq!(
        session.poll(&vault, &cancel).unwrap(),
        QrStatus::WaitingConfirm
    );
    assert_eq!(session.poll(&vault, &cancel).unwrap(), QrStatus::Expired);
    cancel.cancel();
    assert_eq!(session.poll(&vault, &cancel).unwrap(), QrStatus::Cancelled);
    assert_eq!(
        std::fs::read(vault.root().join("bili.cookies.dpapi")).unwrap(),
        before
    );
}

#[cfg(windows)]
#[test]
fn qr_confirmation_is_server_verified_before_atomic_dpapi_save() {
    let temp = tempfile::tempdir().unwrap();
    let vault = CookieVault::open(temp.path().join("vault")).unwrap();
    let transport = Arc::new(Mock::new(vec![generation(), success(), nav(true)]));
    let cancel = Cancellation::new();
    let mut session = BiliQrLogin::start_with_transport(transport.clone(), &cancel).unwrap();
    assert!(matches!(
        session.poll(&vault, &cancel).unwrap(),
        QrStatus::LoggedIn(_)
    ));
    assert_eq!(
        vault.local_login_state(Platform::Bili).unwrap(),
        LocalLoginState::Ready
    );
    assert!(
        vault
            .header_for_url(
                Platform::Bili,
                "https://api.bilibili.com/x/web-interface/nav"
            )
            .unwrap()
            .unwrap()
            .contains("SYNTHETIC_CONFIRMED")
    );
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[2]
            .1
            .as_deref()
            .unwrap()
            .contains("SESSDATA=SYNTHETIC_CONFIRMED")
    );
    let encrypted = std::fs::read(vault.root().join("bili.cookies.dpapi")).unwrap();
    assert!(
        !encrypted
            .windows(b"SYNTHETIC_CONFIRMED".len())
            .any(|bytes| bytes == b"SYNTHETIC_CONFIRMED")
    );
}

#[cfg(windows)]
#[test]
fn failed_candidate_health_preserves_the_previous_account() {
    let temp = tempfile::tempdir().unwrap();
    let vault = CookieVault::open(temp.path().join("vault")).unwrap();
    vault
        .import(Platform::Bili, "SESSDATA=SYNTHETIC_OLD")
        .unwrap();
    let before = std::fs::read(vault.root().join("bili.cookies.dpapi")).unwrap();
    let transport = Arc::new(Mock::new(vec![generation(), success(), nav(false)]));
    let cancel = Cancellation::new();
    let mut session = BiliQrLogin::start_with_transport(transport, &cancel).unwrap();
    assert!(
        matches!(session.poll(&vault,&cancel).unwrap(),QrStatus::VerificationPending(value) if value.state==HealthState::Invalid)
    );
    assert_eq!(
        std::fs::read(vault.root().join("bili.cookies.dpapi")).unwrap(),
        before
    );
}

#[cfg(windows)]
#[test]
fn auxiliary_cookies_are_not_reported_as_logged_in_and_health_uses_the_exact_scope() {
    let temp = tempfile::tempdir().unwrap();
    let vault = CookieVault::open(temp.path().join("vault")).unwrap();
    let transport = Mock::new(vec![nav(true)]);
    let cancel = Cancellation::new();
    assert_eq!(
        login::check_health_with(Platform::Bili, &vault, &cancel, &transport)
            .unwrap()
            .state,
        HealthState::Missing
    );
    vault
        .import(Platform::Bili, "buvid3=SYNTHETIC_VISITOR")
        .unwrap();
    assert_eq!(
        login::check_health_with(Platform::Bili, &vault, &cancel, &transport)
            .unwrap()
            .state,
        HealthState::AuxiliaryOnly
    );
    assert!(transport.requests.lock().unwrap().is_empty());
    vault
        .import(Platform::Bili, "SESSDATA=SYNTHETIC_HOST_ONLY")
        .unwrap();
    assert_eq!(
        login::check_health_with(Platform::Bili, &vault, &cancel, &transport)
            .unwrap()
            .state,
        HealthState::Unknown
    );
    assert!(transport.requests.lock().unwrap().is_empty());
    let cookies = vec![CapturedCookie {
        name: "SESSDATA".into(),
        value: "SYNTHETIC_EXPLICIT".into(),
        domain: "bilibili.com".into(),
        host_only: false,
        path: "/".into(),
        secure: true,
        http_only: true,
        expires_at: None,
    }];
    vault.import_captured(Platform::Bili, &cookies).unwrap();
    assert_eq!(
        login::check_health_with(Platform::Bili, &vault, &cancel, &transport)
            .unwrap()
            .state,
        HealthState::Valid
    );
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}

#[cfg(windows)]
#[test]
fn invalid_structured_capture_is_transactional() {
    let temp = tempfile::tempdir().unwrap();
    let vault = CookieVault::open(temp.path().join("vault")).unwrap();
    vault
        .import(Platform::Bili, "SESSDATA=SYNTHETIC_OLD")
        .unwrap();
    let before = std::fs::read(vault.root().join("bili.cookies.dpapi")).unwrap();
    let bad = CapturedCookie {
        name: "sessionid".into(),
        value: "SYNTHETIC_FOREIGN".into(),
        domain: "douyin.com".into(),
        host_only: false,
        path: "/".into(),
        secure: true,
        http_only: true,
        expires_at: None,
    };
    assert!(vault.import_captured(Platform::Bili, &[bad]).is_err());
    assert_eq!(
        std::fs::read(vault.root().join("bili.cookies.dpapi")).unwrap(),
        before
    );
}

#[cfg(windows)]
#[test]
fn qr_cross_domain_completion_reads_real_scope_instead_of_inventing_it() {
    let temp = tempfile::tempdir().unwrap();
    let vault = CookieVault::open(temp.path().join("vault")).unwrap();
    let mut complete = poll(0);
    complete.body=json!({"code":0,"data":{"code":0,"url":"https://passport.bilibili.com/x/passport-login/web/crossDomain?SESSDATA=SYNTHETIC_QUERY"}}).to_string();
    let mut cookie_reply = reply(json!({}));
    cookie_reply.status = 302;
    cookie_reply.set_cookies = success().set_cookies;
    let transport = Arc::new(Mock::new(vec![
        generation(),
        complete,
        cookie_reply,
        nav(true),
    ]));
    let cancel = Cancellation::new();
    let mut session = BiliQrLogin::start_with_transport(transport.clone(), &cancel).unwrap();
    assert!(matches!(
        session.poll(&vault, &cancel).unwrap(),
        QrStatus::LoggedIn(_)
    ));
    assert_eq!(
        transport.requests.lock().unwrap()[2].0,
        "/x/passport-login/web/crossDomain"
    );
    assert!(
        !vault
            .header_for(Platform::Bili)
            .unwrap()
            .unwrap()
            .contains("SYNTHETIC_QUERY")
    );
}

#[cfg(windows)]
#[test]
fn qr_cross_domain_completion_rejects_foreign_targets_without_requesting_them() {
    let temp = tempfile::tempdir().unwrap();
    let vault = CookieVault::open(temp.path().join("vault")).unwrap();
    let mut complete = poll(0);
    complete.body=json!({"code":0,"data":{"code":0,"url":"https://passport.bilibili.com.attacker.invalid/x/passport-login/web/crossDomain?SESSDATA=SYNTHETIC"}}).to_string();
    let transport = Arc::new(Mock::new(vec![generation(), complete]));
    let cancel = Cancellation::new();
    let mut session = BiliQrLogin::start_with_transport(transport.clone(), &cancel).unwrap();
    assert!(session.poll(&vault, &cancel).is_err());
    assert_eq!(transport.requests.lock().unwrap().len(), 2);
    assert!(!vault.has_credentials(Platform::Bili).unwrap());
}

#[cfg(windows)]
#[test]
fn cancellation_after_response_prevents_candidate_persistence() {
    struct CancelsOnPoll {
        inner: Mock,
        cancel: Cancellation,
    }
    impl LoginTransport for CancelsOnPoll {
        fn get(&self, url: &Url, cookie: Option<&str>, token: &Cancellation) -> Result<LoginReply> {
            let reply = self.inner.get(url, cookie, token)?;
            if url.path().ends_with("/poll") {
                self.cancel.cancel();
            }
            Ok(reply)
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let vault = CookieVault::open(temp.path().join("vault")).unwrap();
    let cancel = Cancellation::new();
    let transport = Arc::new(CancelsOnPoll {
        inner: Mock::new(vec![generation(), success()]),
        cancel: cancel.clone(),
    });
    let mut session = BiliQrLogin::start_with_transport(transport, &cancel).unwrap();
    assert!(session.poll(&vault, &cancel).is_err());
    assert!(!vault.has_credentials(Platform::Bili).unwrap());
}

#[cfg(windows)]
#[test]
fn mixed_bili_account_identifiers_are_not_persisted_as_a_valid_candidate() {
    let temp = tempfile::tempdir().unwrap();
    let vault = CookieVault::open(temp.path().join("vault")).unwrap();
    vault
        .import(Platform::Bili, "SESSDATA=SYNTHETIC_OLD")
        .unwrap();
    let before = std::fs::read(vault.root().join("bili.cookies.dpapi")).unwrap();
    let other = reply(json!({"code":0,"data":{"isLogin":true,"mid":456}}));
    let transport = Arc::new(Mock::new(vec![generation(), success(), other]));
    let token = Cancellation::new();
    let mut session = BiliQrLogin::start_with_transport(transport, &token).unwrap();
    assert!(
        matches!(session.poll(&vault,&token).unwrap(),QrStatus::VerificationPending(value) if value.state==HealthState::Unknown)
    );
    assert_eq!(
        std::fs::read(vault.root().join("bili.cookies.dpapi")).unwrap(),
        before
    );
}

fn synthetic_health(state: HealthState, checked_at: u64) -> LoginHealth {
    LoginHealth {
        platform: Platform::Bili,
        state,
        reason: "合成检查".into(),
        checked_at,
        verified_at: if state == HealthState::Valid {
            checked_at
        } else {
            0
        },
    }
}

#[test]
fn a_confirmed_valid_result_goes_stale_after_ten_minutes_without_claiming_expiry() {
    let checked = 1_700_000_000;
    let health = synthetic_health(HealthState::Valid, checked);
    // Exactly ten minutes old is still current; only older results are stale.
    assert_eq!(health.current_label(checked), "登录有效");
    assert_eq!(
        health.current_label(checked + login::HEALTH_FRESH_SECS),
        "登录有效"
    );
    let stale_at = checked + login::HEALTH_FRESH_SECS + 1;
    assert_eq!(health.current_label(stale_at), "上次有效，待复查");
    let reason = health.current_reason(stale_at);
    assert!(reason.contains("超过 10 分钟"), "{reason}");
    assert!(reason.contains("不代表 Cookie 过期"), "{reason}");
    assert!(health.is_stale(stale_at));
    assert!(!health.is_stale(checked + 5));
    // Only an explicit platform confirmation may move the last-valid time.
    assert_eq!(health.clone().carry_verified(None).verified_at, checked);
    let network = synthetic_health(HealthState::NetworkUnknown, checked + 1_000);
    let carried = network.carry_verified(Some(&health));
    assert_eq!(carried.state, HealthState::NetworkUnknown);
    assert_eq!(carried.verified_at, checked);
    assert_eq!(
        carried.current_label(stale_at),
        "网络异常，无法确认",
        "a failure is shown as a failure, not as stale/expired"
    );
    assert_eq!(
        synthetic_health(HealthState::Valid, checked + 2_000)
            .carry_verified(Some(&carried))
            .verified_at,
        checked + 2_000
    );
    // The detail line keeps both times and never hides the check age.
    let detail = health.detail_text(checked + 700);
    assert!(detail.contains("B站"), "{detail}");
    assert!(detail.contains("上次明确有效"), "{detail}");
    assert!(detail.contains("最近检测"), "{detail}");
    assert_eq!(login::time_text(0), "尚未在线验证");
    assert!(login::time_text(checked).contains(':'));
}

#[test]
fn manual_checks_are_limited_to_one_per_ten_seconds_per_platform() {
    let floor = login::MANUAL_CHECK_MIN_SECS;
    assert_eq!(floor, 10);
    assert!(login::check_allowed(None, 1_000, floor));
    assert!(!login::check_allowed(Some(1_000), 1_000, floor));
    assert!(!login::check_allowed(Some(1_000), 1_009, floor));
    assert!(login::check_allowed(Some(1_000), 1_010, floor));
    // A clock that moved backwards must not suppress an explicit retry forever.
    assert!(login::check_allowed(Some(2_000), 1_000, floor));
    // Staleness applies only to a previously confirmed valid result.
    let invalid = synthetic_health(HealthState::Invalid, 1_000);
    assert!(!invalid.is_stale(99_999));
    assert_eq!(invalid.current_label(99_999), "登录失效");
}

#[test]
fn assist_profile_cleanup_only_removes_that_platforms_own_directories() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("browser-login");
    std::fs::create_dir_all(&root).unwrap();
    let douyin_a = root.join("douyin-0123456789abcdef0123456789abcdef");
    let douyin_b = root.join("douyin-fedcba9876543210fedcba9876543210");
    let huya = root.join("huya-0123456789abcdef0123456789abcdef");
    for path in [&douyin_a, &douyin_b, &huya] {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join("Preferences"), "synthetic").unwrap();
    }
    // Unexpected shapes are never removed: wrong platform, wrong length, plain file.
    let short = root.join("douyin-short");
    std::fs::create_dir_all(&short).unwrap();
    let plain = root.join("douyin-0123456789abcdef0123456789abcdef.txt");
    std::fs::write(&plain, "not a directory").unwrap();
    let removed = browser::remove_platform_profiles(&root, Platform::Douyin).unwrap();
    assert_eq!(removed, 2);
    assert!(!douyin_a.exists() && !douyin_b.exists());
    assert!(huya.exists(), "another platform's profile is untouched");
    assert!(short.exists(), "an unexpected profile name is kept");
    assert!(plain.exists(), "plain files are never removed");
    // A missing root is a no-op rather than an error.
    assert_eq!(
        browser::remove_platform_profiles(&directory.path().join("missing"), Platform::Douyin)
            .unwrap(),
        0
    );
}
