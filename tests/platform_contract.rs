use live_hub_rust::{
    core::{Platform, Quality},
    platforms::*,
};
use serde_json::json;

#[test]
fn media_addresses_reject_private_untrusted_and_credential_urls() {
    for bad in [
        "http://127.0.0.1/live.flv",
        "https://bilivideo.com.attacker.invalid/live.flv",
        "https://user:pass@a.bilivideo.com/live.flv",
        "file:///video.flv",
    ] {
        assert!(
            validate_media_url(bad).is_err(),
            "unexpectedly accepted {bad}"
        );
    }
    assert_eq!(
        validate_media_url("http://cdn.bilivideo.com/live.flv")
            .unwrap()
            .scheme(),
        "https"
    );
    // Douyu's getH5PlayV1 rtmp_url can carry an explicit CDN port; the host
    // allowlist still applies, so this is a legitimate media address.
    assert_eq!(
        validate_media_url("https://hw-tct.douyucdn.cn:8443/live/room.flv")
            .unwrap()
            .port(),
        Some(8443)
    );
}

#[test]
fn bili_prefers_flv_avc_and_preserves_cdn_query() {
    let root = json!({"code":0,"data":{"playurl_info":{"playurl":{"stream":[{"format":[
        {"format_name":"ts","codec":[{"codec_name":"avc","base_url":"/hls","url_info":[{"host":"https://cdn.bilivideo.com","extra":"?x=1"}]}]},
        {"format_name":"flv","codec":[{"codec_name":"hevc","base_url":"/hevc","url_info":[{"host":"https://cdn.bilivideo.com","extra":""}]},{"codec_name":"avc","base_url":"/live.flv","url_info":[{"host":"https://cdn.bilivideo.com","extra":"?synthetic=1"}]}]}
    ]}]}}}});
    let output = parse_bili_play(&root, "Synthetic title").unwrap();
    assert_eq!(output.url, "https://cdn.bilivideo.com/live.flv?synthetic=1");
    assert_eq!(output.headers["Referer"], "https://live.bilibili.com/");
    assert!(
        parse_bili_play(&json!({"code":-352}), "")
            .unwrap_err()
            .downcast_ref::<AdapterError>()
            .is_some()
    );
}

#[test]
fn huya_signed_url_is_deterministic_and_selects_quality() {
    let entry = json!({"sFlvUrl":"https://hs.flv.huya.com/src","sStreamName":"synthetic-room","sFlvUrlSuffix":"flv","sFlvAntiCode":"wsTime=synthetic-time&fm=cHJlZml4X3N5bnRoZXRpYw%3D%3D&ctype=huya_live&fs="});
    let address = huya_signed_url(&entry, 2000, 1_700_000_000_000, 12_345_678).unwrap();
    let url = url::Url::parse(&address).unwrap();
    let pairs: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(pairs["ratio"], "2000");
    assert_eq!(pairs["wsSecret"], "c66e57fd6cd9dbac464afb9d6f009c51");
    assert_eq!(
        address,
        huya_signed_url(&entry, 2000, 1_700_000_000_000, 12_345_678).unwrap()
    );
    let configuration = json!({"data":[{"gameStreamInfoList":[entry],"vMultiStreamInfo":[{"iBitRate":0},{"iBitRate":1000},{"iBitRate":2000},{"iBitRate":4000}]}]});
    let page = format!("var hyPlayerConfig = {{stream:{configuration}, other:1}};");
    let selected = parse_huya_page(&page, &Quality::Low, 1_700_000_000_000, 12_345_678).unwrap();
    assert!(selected.url.contains("ratio=1000"));
}

#[test]
fn douyu_original_rate_does_not_shift_transcoded_midpoint() {
    let rates = json!([{"rate":0,"bit":8456},{"rate":1,"bit":1000},{"rate":2,"bit":2000},{"rate":3,"bit":4000}]);
    assert_eq!(pick_douyu_rate(&rates, &Quality::Best), 0);
    assert_eq!(pick_douyu_rate(&rates, &Quality::High), 2);
    assert_eq!(pick_douyu_rate(&rates, &Quality::Low), 1);
    assert!(douyu_auth("123", 100, "key", "random", 1001, false).is_err());
    assert_ne!(
        douyu_auth("123", 100, "key", "random", 2, false).unwrap(),
        douyu_auth("123", 100, "key", "random", 2, true).unwrap()
    );
}

fn douyin_page_with_flv(status: i32, address: &str) -> String {
    let room = json!({"room":{"status":status,"title":"Synthetic \"title\"","stream_url":{"flv_pull_url":{"FULL_HD1":address}}}});
    let payload = format!("7:{}", json!({"state":{"roomStore":{"roomInfo":room}}}));
    format!(
        "self.__pace_f.push([0]); self.__pace_f.push([1,{}])</script>",
        serde_json::to_string(&payload).unwrap()
    )
}

fn douyin_page(status: i32, shell: bool) -> String {
    let room = if shell {
        json!({})
    } else {
        json!({"room":{"status":status,"title":"Synthetic \"title\"","stream_url":{"flv_pull_url":{"FULL_HD1":"https://pull.douyincdn.com/full.flv","SD1":"https://pull.douyincdn.com/low.flv"}}}})
    };
    let payload = format!("7:{}", json!({"state":{"roomStore":{"roomInfo":room}}}));
    format!(
        "self.__pace_f.push([0]); self.__pace_f.push([1,{}])</script>",
        serde_json::to_string(&payload).unwrap()
    )
}

#[test]
fn douyin_distinguishes_live_offline_rate_limit_and_verification() {
    let output = parse_douyin_page(&douyin_page(2, false), &Quality::Low).unwrap();
    assert_eq!(output.url, "https://pull.douyincdn.com/low.flv");
    assert!(matches!(
        parse_douyin_page(&douyin_page(4, false), &Quality::Best)
            .unwrap_err()
            .downcast_ref::<AdapterError>(),
        Some(AdapterError::Offline)
    ));
    assert!(matches!(
        parse_douyin_page(&douyin_page(2, true), &Quality::Best)
            .unwrap_err()
            .downcast_ref::<AdapterError>(),
        Some(AdapterError::RateLimited)
    ));
    assert!(matches!(
        parse_douyin_page("<html>captcha verify</html>", &Quality::Best)
            .unwrap_err()
            .downcast_ref::<AdapterError>(),
        Some(AdapterError::VerificationRequired)
    ));
}

#[test]
fn douyin_accepts_observed_douyinliving_cdn_and_reports_rejected_hosts() {
    // 2026-10-29 anonymous observation: a live room handed out
    // http://pull-flv-*.douyinliving.com (whose HTTPS edge answers video/x-flv),
    // so the CDN allowlist must cover that family and the http→https upgrade
    // stays valid.
    let page = douyin_page_with_flv(
        2,
        "http://pull-flv-f13.douyinliving.com/thirdgame/stream-synthetic.flv?sign=synthetic",
    );
    let output = parse_douyin_page(&page, &Quality::Best).unwrap();
    assert_eq!(
        output.url,
        "https://pull-flv-f13.douyinliving.com/thirdgame/stream-synthetic.flv?sign=synthetic"
    );
    assert_eq!(
        validate_media_url("http://pull-flv-f13.douyinliving.com/x.flv")
            .unwrap()
            .scheme(),
        "https"
    );
    // A genuinely unknown CDN family must surface the concrete rejection instead
    // of being hidden behind the generic "no usable FLV address" message.
    let rejected = douyin_page_with_flv(2, "https://pull-flv.invalid.example/stream.flv");
    let error = parse_douyin_page(&rejected, &Quality::Best).unwrap_err();
    assert!(error.to_string().contains("尚未允许的媒体主机"), "{error}");
}

#[test]
fn search_only_returns_valid_deduplicated_room_candidates() {
    let root = json!({"code":0,"data":{"result":[
        {"uname":"<em>Synthetic</em> &amp; test","roomid":123,"title":"<em>Live</em>"},
        {"uname":"duplicate","roomid":"123"},
        {"uname":"missing room","roomid":"0"},
        {"uname":"hostile","roomid":"../invalid"}
    ]}});
    let results = parse_search(Platform::Bili, &root).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "Synthetic & test");
    assert_eq!(results[0].room, "123");
}

#[test]
fn wbi_signing_sorts_and_filters_values_without_sending_a_request() {
    let mixin = bili_mixin(
        "0123456789abcdef0123456789abcdef",
        "fedcba9876543210fedcba9876543210",
    )
    .unwrap();
    assert_eq!(mixin.len(), 32);
    let params = std::collections::BTreeMap::from([
        ("z".into(), "!'()*tail".into()),
        ("a".into(), "hello world".into()),
    ]);
    let url = bili_signed_url(
        "https://api.bilibili.com/x/test",
        &params,
        &mixin,
        1_700_000_000,
    )
    .unwrap();
    assert!(
        url.query()
            .unwrap()
            .starts_with("a=hello+world&wts=1700000000&z=tail&w_rid=")
    );
    assert_eq!(
        url.query_pairs()
            .find(|(key, _)| key == "w_rid")
            .unwrap()
            .1
            .len(),
        32
    );
    assert!(bili_signed_url("https://127.0.0.1/x/test", &params, &mixin, 1).is_err());
    assert!(
        bili_signed_url(
            "https://api.bilibili.com/x/test?existing=1",
            &params,
            &mixin,
            1
        )
        .is_err()
    );
    assert!(bili_mixin("short", "invalid").is_err());
}

#[test]
fn douyin_account_search_preserves_identity_without_inventing_a_room() {
    let root = json!({"status_code":0,"user_list":[
        {"user_info":{"sec_uid":"MS4wLjSynthetic_A","uid":9007199254740993u64,"nickname":"Synthetic A","room_id":12345}},
        {"user_info":{"sec_uid":"MS4wLjSynthetic_A","nickname":"duplicate"}},
        {"user_info":{"sec_uid":"invalid","nickname":"bad"}}
    ]});
    let rows = parse_douyin_search(&root).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].room.is_empty());
    assert_eq!(rows[0].account_id.as_deref(), Some("9007199254740993"));
    assert_eq!(
        rows[0].profile_url.as_deref(),
        Some("https://www.douyin.com/user/MS4wLjSynthetic_A")
    );
    let reflow = json!({"status_code":0,"data":{"room":{"id_str":"12345","title":"Synthetic live","owner":{"sec_uid":"MS4wLjSynthetic_A","web_rid":"67890","nickname":"Synthetic A"}}}});
    let verified = apply_douyin_reflow(&rows[0], "12345", &reflow).unwrap();
    assert_eq!(verified.room, "67890");
    assert!(apply_douyin_reflow(&rows[0], "different-session", &reflow).is_err());
    let wrong_owner = json!({"status_code":0,"data":{"room":{"id_str":"12345","owner":{"sec_uid":"MS4wLjSynthetic_B","web_rid":"67890"}}}});
    assert!(apply_douyin_reflow(&rows[0], "12345", &wrong_owner).is_err());
    assert!(parse_douyin_search(&json!({"status_code":2483})).is_err());
}

#[test]
fn explicit_cookie_rejects_header_injection_before_network() {
    let error = search_with_cookie(
        Platform::Douyin,
        "Synthetic",
        Some("synthetic=x\r\nInjected: y"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("Cookie"));
}

#[test]
fn playback_titles_parse_from_documented_platform_shapes() {
    // Synthetic responses shaped like the documented public endpoints.
    assert_eq!(
        bili_title_from_info(&json!({"code":0,"data":{"title":"Synthetic room"}})).as_deref(),
        Some("Synthetic room")
    );
    assert_eq!(
        bili_anchor_from_info(&json!({"code":0,"data":{"info":{"uname":"Synthetic anchor"}}}))
            .as_deref(),
        Some("Synthetic anchor")
    );
    assert!(bili_title_from_info(&json!({"code":-400,"data":{}})).is_none());
    assert_eq!(
        douyu_title_from_json(
            &json!({"room":{"nickname":"Synthetic anchor","room_name":"Synthetic room"}})
        )
        .as_deref(),
        Some("Synthetic anchor - Synthetic room")
    );
    assert_eq!(
        huya_title_from_json(
            &json!({"data":{"nick":"Synthetic anchor","roomName":"Synthetic room"}})
        )
        .as_deref(),
        Some("Synthetic anchor - Synthetic room")
    );
    // Control characters are stripped rather than passed to the player command line.
    assert_eq!(
        douyu_title_from_json(&json!({"room":{"nickname":"a\u{1}b","room_name":"room"}}))
            .as_deref(),
        Some("ab - room")
    );
}
