use anyhow::Result;
use live_hub_rust::{
    auth::CookieVault,
    core::{Platform, Target},
    danmaku::{
        self, Cancellation,
        session::{self, BiliCookieSource, DouyinSessionCookies},
    },
    platforms::CookieSource,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct FakeSource {
    headers: BTreeMap<String, String>,
    calls: Mutex<Vec<(Platform, String)>>,
}
impl CookieSource for FakeSource {
    fn header_for_url(&self, platform: Platform, address: &str) -> Result<Option<String>> {
        self.calls
            .lock()
            .unwrap()
            .push((platform, address.to_owned()));
        Ok(self.headers.get(address).cloned())
    }
}

#[test]
fn wss_uses_its_own_https_cookie_origin_and_keeps_path() {
    let mut source = FakeSource::default();
    source.headers.insert(
        "https://live.douyin.com/123".into(),
        "host_only=page".into(),
    );
    source.headers.insert(
        "https://webcast5-ws-web-lq.douyin.com/webcast/im/push/v2/?synthetic=1".into(),
        "socket_only=socket".into(),
    );
    let header = session::scoped_cookie(
        Some(&source),
        Platform::Douyin,
        "wss://webcast5-ws-web-lq.douyin.com/webcast/im/push/v2/?synthetic=1",
    )
    .unwrap();
    assert_eq!(header.as_deref(), Some("socket_only=socket"));
    assert_eq!(
        source.calls.lock().unwrap()[0].1,
        "https://webcast5-ws-web-lq.douyin.com/webcast/im/push/v2/?synthetic=1"
    );
    for unsafe_url in [
        "ws://webcast5-ws-web-lq.douyin.com/",
        "https://user:synthetic@live.douyin.com/",
        "wss://live.douyin.com:9443/",
        "https://live.douyin.com/#fragment",
    ] {
        assert!(session::scoped_cookie(Some(&source), Platform::Douyin, unsafe_url).is_err());
    }
}

#[test]
fn bili_login_uid_must_match_the_cookie_sent_to_danmaku_api() {
    assert_eq!(session::bili_uid_for_auth(None, None).unwrap(), 0);
    assert_eq!(
        session::bili_uid_for_auth(Some("buvid3=synthetic-anonymous"), Some(42)).unwrap(),
        0
    );
    assert_eq!(
        session::bili_uid_for_auth(Some("SESSDATA=synthetic-session; DedeUserID=42"), Some(42))
            .unwrap(),
        42
    );
    assert!(
        session::bili_uid_for_auth(Some("SESSDATA=synthetic-session; DedeUserID=41"), Some(42))
            .is_err()
    );
    assert!(session::bili_uid_for_auth(Some("SESSDATA=synthetic-session"), None).is_err());
    assert!(
        session::bili_uid_for_auth(
            Some("SESSDATA=synthetic-session; DedeUserID=not-a-number"),
            Some(42)
        )
        .is_err()
    );
}

#[test]
fn bili_ephemeral_identity_does_not_copy_cookies_between_origins() {
    let mut base = FakeSource::default();
    base.headers.insert(
        session::BILI_DANMAKU_INFO_URL.into(),
        "SESSDATA=synthetic-api-session; buvid3=synthetic-device".into(),
    );
    base.headers.insert(
        "https://broadcastlv.chat.bilibili.com/sub".into(),
        "socket_only=synthetic".into(),
    );
    let source = BiliCookieSource::new(Arc::new(base), "synthetic-device".into()).unwrap();
    let socket = source
        .header_for_url(Platform::Bili, "https://broadcastlv.chat.bilibili.com/sub")
        .unwrap()
        .unwrap();
    assert_eq!(socket, "socket_only=synthetic; buvid3=synthetic-device");
    assert!(!socket.contains("SESSDATA"));
    assert!(
        source
            .header_for_url(Platform::Douyin, "https://live.douyin.com/")
            .is_err()
    );
    assert!(
        source
            .header_for_url(Platform::Bili, "https://bilibili.com.attacker.invalid/")
            .is_err()
    );
    let mut conflicting = FakeSource::default();
    conflicting.headers.insert(
        session::BILI_DANMAKU_INFO_URL.into(),
        "buvid3=other-synthetic-device".into(),
    );
    let source = BiliCookieSource::new(Arc::new(conflicting), "synthetic-device".into()).unwrap();
    assert!(
        source
            .header_for_url(Platform::Bili, session::BILI_DANMAKU_INFO_URL)
            .is_err()
    );
}

#[test]
fn douyin_nonce_and_host_only_ttwid_do_not_escape_live_origin() {
    let mut jar = DouyinSessionCookies::default();
    jar.absorb(
        "https://live.douyin.com/123",
        "ttwid=synthetic-host-cookie; Path=/; Secure; HttpOnly",
        100,
    )
    .unwrap();
    let live = jar
        .header_for(
            None,
            "https://live.douyin.com/456",
            Some("synthetic-nonce"),
            101,
        )
        .unwrap()
        .unwrap();
    assert!(live.contains("ttwid=synthetic-host-cookie"));
    assert!(live.contains("__ac_nonce=synthetic-nonce"));
    assert!(
        jar.header_for(
            None,
            "wss://webcast5-ws-web-lq.douyin.com/webcast/im/push/v2/",
            Some("synthetic-nonce"),
            101
        )
        .unwrap()
        .is_none()
    );
    assert!(
        jar.absorb(
            "https://live.douyin.com/123",
            "ttwid=synthetic; Domain=.amemv.com; Path=/",
            101
        )
        .is_err()
    );
    assert!(
        jar.absorb(
            "https://live.douyin.com/123",
            "ttwid=synthetic; Domain=.douyin.com.attacker.invalid; Path=/",
            101
        )
        .is_err()
    );
}

#[test]
fn douyin_parent_domain_ttwid_keeps_path_and_expiry() {
    let mut jar = DouyinSessionCookies::default();
    jar.absorb(
        "https://live.douyin.com/123",
        "ttwid=synthetic-shared; Domain=.douyin.com; Path=/webcast; Secure; Max-Age=20",
        100,
    )
    .unwrap();
    let socket = "wss://webcast5-ws-web-lq.douyin.com/webcast/im/push/v2/";
    assert_eq!(
        jar.header_for(None, socket, None, 119).unwrap().as_deref(),
        Some("ttwid=synthetic-shared")
    );
    assert!(jar.header_for(None, socket, None, 120).unwrap().is_none());
    assert!(
        jar.header_for(
            None,
            "wss://webcast5-ws-web-lq.douyin.com/webcast-other",
            None,
            101
        )
        .unwrap()
        .is_none()
    );
    jar.absorb(
        "https://live.douyin.com/123",
        "ttwid=deleted; Domain=.douyin.com; Path=/webcast; Max-Age=0",
        102,
    )
    .unwrap();
    assert!(jar.header_for(None, socket, None, 103).unwrap().is_none());
    jar.absorb(
        "https://live.douyin.com/123",
        "ttwid=expired; Domain=.douyin.com; Path=/webcast; Expires=Wed, 31 Dec 1969 23:59:59 GMT",
        104,
    )
    .unwrap();
    assert!(jar.header_for(None, socket, None, 105).unwrap().is_none());
}

#[test]
fn douyin_requests_lookup_imported_cookies_for_each_destination() {
    let mut source = FakeSource::default();
    source.headers.insert(
        "https://live.douyin.com/123".into(),
        "live_only=synthetic".into(),
    );
    source.headers.insert(
        "https://webcast5-ws-web-lq.douyin.com/webcast/im/push/v2/".into(),
        "ttwid=synthetic-imported".into(),
    );
    let jar = DouyinSessionCookies::default();
    let page = jar
        .header_for(
            Some(&source),
            "https://live.douyin.com/123",
            Some("synthetic-nonce"),
            100,
        )
        .unwrap()
        .unwrap();
    let socket = jar
        .header_for(
            Some(&source),
            "wss://webcast5-ws-web-lq.douyin.com/webcast/im/push/v2/",
            None,
            100,
        )
        .unwrap()
        .unwrap();
    assert_eq!(page, "live_only=synthetic; __ac_nonce=synthetic-nonce");
    assert_eq!(socket, "ttwid=synthetic-imported");
    assert_eq!(source.calls.lock().unwrap().len(), 2);
}

#[test]
fn cancelled_authenticated_receivers_do_not_read_credentials_or_open_network() {
    let source = Arc::new(FakeSource::default());
    for target in ["bili:123", "douyu:123", "huya:123", "douyin:123"] {
        let cancellation = Cancellation::new();
        cancellation.cancel();
        let (sender, _receiver) = danmaku::bounded_channel(1);
        danmaku::connect_with_cookie_source(
            &Target::parse(target).unwrap(),
            sender,
            cancellation,
            source.clone(),
        )
        .unwrap();
    }
    assert!(source.calls.lock().unwrap().is_empty());
}

#[cfg(windows)]
#[test]
fn dpapi_cookie_vault_preserves_live_host_and_ws_scope() {
    let temporary = tempfile::tempdir().unwrap();
    let vault = CookieVault::open(temporary.path().join("synthetic-vault")).unwrap();
    vault.import(Platform::Douyin,"# Netscape HTTP Cookie File\n#HttpOnly_live.douyin.com\tFALSE\t/\tTRUE\t0\tlive_only\tsynthetic-page\n.douyin.com\tTRUE\t/webcast\tTRUE\t0\tttwid\tsynthetic-socket\n").unwrap();
    let jar = DouyinSessionCookies::default();
    let page = jar
        .header_for(
            Some(&vault),
            "https://live.douyin.com/123",
            Some("synthetic-nonce"),
            100,
        )
        .unwrap()
        .unwrap();
    let ws = jar
        .header_for(
            Some(&vault),
            "wss://webcast5-ws-web-lq.douyin.com/webcast/im/push/v2/",
            None,
            100,
        )
        .unwrap()
        .unwrap();
    assert!(page.contains("live_only=synthetic-page"));
    assert!(!page.contains("ttwid="));
    assert_eq!(ws, "ttwid=synthetic-socket");
}
