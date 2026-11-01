//! Offline contract tests for the candidate detail fields (SRH-02).
//!
//! Pure platform fixtures only: no network, no account data. The tests pin which
//! platform fields become which detail value and that missing platform data stays
//! None/empty instead of being fabricated.

use live_hub_rust::core::Platform;
use live_hub_rust::platforms::{
    apply_douyin_reflow, candidate_live_status, parse_douyin_search, parse_search,
    retrieval_timestamp,
};
use serde_json::json;

const SEC_UID: &str = "MS4wLjABAAAAsynthetic_sec_uid_0001";

#[test]
fn platform_search_rows_populate_only_returned_detail_fields() {
    let bili = json!({
        "code": 0,
        "data": {"result": [{
            "uname": "合成B站主播",
            "roomid": "12345",
            "title": "B站标题",
            "uid": "1001",
            "attentions": 10,
            "live_status": 1
        }]}
    });
    let candidates = parse_search(Platform::Bili, &bili).unwrap();
    let candidate = &candidates[0];
    assert_eq!(candidate.room, "12345");
    assert_eq!(candidate.live_status.as_deref(), Some("live"));
    assert_eq!(candidate.provider, "platform_search");
    assert!(!candidate.match_reasons.is_empty());
    assert_eq!(candidate.verified_at, 0, "search results are not verified rooms");
    assert!(candidate.sources.is_empty());
    assert!(candidate.sec_uid.is_none());
    assert!(candidate.platform_update.is_none());

    let huya = json!({
        "responseHeader": {"status": 0},
        "response": {"1": {"docs": [{
            "game_nick": "合成虎牙主播",
            "room_id": "910323",
            "game_introduction": "虎牙简介",
            "liveStatus": "ON",
            "game_activityCount": "12"
        }]}}
    });
    let candidate = &parse_search(Platform::Huya, &huya).unwrap()[0];
    assert_eq!(candidate.live_status.as_deref(), Some("live"));
    assert_eq!(candidate.provider, "platform_search");

    let douyu = json!({
        "error": 0,
        "data": {"relateUser": [{"anchorInfo": {
            "nickName": "合成斗鱼主播",
            "rid": "9999",
            "roomName": "斗鱼标题",
            "show_status": 0,
            "videoLoop": 1,
            "fansNum": "34"
        }}]}
    });
    let candidate = &parse_search(Platform::Douyu, &douyu).unwrap()[0];
    assert_eq!(
        candidate.live_status.as_deref(),
        Some("replay"),
        "videoLoop marks a recording/replay room"
    );

    let douyin = json!({
        "status_code": 0,
        "user_list": [{"user_info": {
            "nickname": "合成抖音主播",
            "sec_uid": SEC_UID,
            "uid": "2002",
            "signature": "抖音签名",
            "live_status": 2
        }}]
    });
    let candidate = &parse_douyin_search(&douyin).unwrap()[0];
    assert!(candidate.room.is_empty(), "account rows never invent a web room");
    assert_eq!(candidate.sec_uid.as_deref(), Some(SEC_UID));
    assert_eq!(candidate.live_status.as_deref(), Some("live"));
    assert_eq!(candidate.provider, "platform_search");
}

#[test]
fn missing_platform_fields_stay_empty_instead_of_being_invented() {
    let row = json!({
        "uname": "无状态主播",
        "roomid": "5555",
        "title": "标题"
    });
    assert!(candidate_live_status(Platform::Bili, &row).is_none());
    assert!(candidate_live_status(Platform::Huya, &row).is_none());
    assert!(candidate_live_status(Platform::Douyu, &row).is_none());
    assert!(candidate_live_status(Platform::Douyin, &row).is_none());

    let root = json!({
        "code": 0,
        "data": {"result": [{"uname": "无状态主播", "roomid": "5555", "title": "标题"}]}
    });
    let candidate = &parse_search(Platform::Bili, &root).unwrap()[0];
    assert!(candidate.live_status.is_none());
    assert!(candidate.platform_update.is_none());
    assert_eq!(candidate.verified_at, 0);
    assert!(candidate.verification_note.is_empty());
    assert!(candidate.sources.is_empty());
}

#[test]
fn unknown_live_status_values_are_not_guessed() {
    let row = json!({"live_status": 777});
    assert!(candidate_live_status(Platform::Bili, &row).is_none());
    let row = json!({"liveStatus": "MAYBE"});
    assert!(candidate_live_status(Platform::Huya, &row).is_none());
    let row = json!({"status": 99});
    assert!(candidate_live_status(Platform::Douyin, &row).is_none());
}

#[test]
fn verified_douyin_candidate_carries_identity_live_state_and_sources() {
    let search = json!({
        "status_code": 0,
        "user_list": [{"user_info": {
            "nickname": "抖音主播",
            "sec_uid": SEC_UID,
            "uid": "2002",
            "signature": "签名"
        }}]
    });
    let candidate = parse_douyin_search(&search).unwrap().remove(0);
    assert_eq!(candidate.verified_at, 0);
    assert!(candidate.sources.is_empty());

    let reflow = json!({
        "status_code": 0,
        "data": {"room": {
            "id_str": "synthetic_session_1",
            "title": "直播标题",
            "status": 2,
            "web_rid": "987654",
            "owner": {"sec_uid": SEC_UID, "nickname": "抖音主播", "web_rid": "987654"}
        }}
    });
    let verified = apply_douyin_reflow(&candidate, "synthetic_session_1", &reflow).unwrap();
    assert_eq!(verified.room, "987654");
    assert_eq!(verified.sec_uid.as_deref(), Some(SEC_UID));
    assert_eq!(verified.provider, "platform_verify");
    assert!(verified.verified_at > 0);
    assert!(!verified.verification_note.is_empty());
    assert_eq!(verified.live_status.as_deref(), Some("live"));
    assert_eq!(verified.sources.len(), 2, "account profile + live room");
    assert!(verified.sources.iter().all(|source| source.url.starts_with("https://")));
    assert!(verified.sources.iter().all(|source| !source.retrieved_at.is_empty()));
    assert!(!verified.match_reasons.is_empty());
    assert!(verified.platform_update.is_none(), "no invented update time");
    let stamp = retrieval_timestamp();
    assert!(stamp.ends_with('Z') && stamp.contains('T'), "{stamp}");
}

#[test]
fn douyin_identity_or_session_mismatch_is_rejected() {
    let search = json!({
        "status_code": 0,
        "user_list": [{"user_info": {"nickname": "抖音主播", "sec_uid": SEC_UID, "uid": "2002"}}]
    });
    let candidate = parse_douyin_search(&search).unwrap().remove(0);
    let reflow = json!({
        "status_code": 0,
        "data": {"room": {
            "id_str": "another_session",
            "status": 2,
            "web_rid": "987654",
            "owner": {"sec_uid": SEC_UID, "nickname": "抖音主播"}
        }}
    });
    assert!(apply_douyin_reflow(&candidate, "synthetic_session_1", &reflow).is_err());
}
#[test]
fn search_rows_extract_platform_avatars_with_the_shared_allowlist() {
    let bili = json!({
        "code": 0,
        "data": {"result": [{
            "uname": "合成B站主播",
            "roomid": "12345",
            "title": "B站标题",
            "uface": "//i0.hdslb.com/bfs/face/synthetic_bili.jpg"
        }]}
    });
    assert_eq!(
        parse_search(Platform::Bili, &bili).unwrap()[0]
            .avatar_url
            .as_deref(),
        Some("https://i0.hdslb.com/bfs/face/synthetic_bili.jpg")
    );

    let huya = json!({
        "responseHeader": {"status": 0},
        "response": {"1": {"docs": [{
            "game_nick": "合成虎牙主播",
            "room_id": "910323",
            "avatar": "https://huyaimg.msstatic.com/avatar/synthetic_huya.jpg"
        }]}}
    });
    assert_eq!(
        parse_search(Platform::Huya, &huya).unwrap()[0]
            .avatar_url
            .as_deref(),
        Some("https://huyaimg.msstatic.com/avatar/synthetic_huya.jpg")
    );

    let douyu = json!({
        "error": 0,
        "data": {"relateUser": [{"anchorInfo": {
            "nickName": "合成斗鱼主播",
            "rid": "9999",
            "avatar": "https://apic.douyucdn.cn/upload/avatar_v3/synthetic_douyu_middle.jpg"
        }}]}
    });
    assert_eq!(
        parse_search(Platform::Douyu, &douyu).unwrap()[0]
            .avatar_url
            .as_deref(),
        Some("https://apic.douyucdn.cn/upload/avatar_v3/synthetic_douyu_middle.jpg")
    );

    let douyin = json!({
        "status_code": 0,
        "user_list": [{"user_info": {
            "nickname": "合成抖音主播",
            "sec_uid": SEC_UID,
            "uid": "2002",
            "avatar_thumb": {"url_list": [
                "https://p3-pc.douyinpic.com/aweme/100x100/synthetic_douyin.jpeg"
            ]}
        }}]
    });
    assert_eq!(
        parse_douyin_search(&douyin).unwrap()[0]
            .avatar_url
            .as_deref(),
        Some("https://p3-pc.douyinpic.com/aweme/100x100/synthetic_douyin.jpeg")
    );

    let untrusted = json!({
        "code": 0,
        "data": {"result": [{
            "uname": "被拒绝的头像",
            "roomid": "12346",
            "uface": "https://example.com/avatar.jpg"
        }]}
    });
    assert!(parse_search(Platform::Bili, &untrusted).unwrap()[0]
        .avatar_url
        .is_none());
}
