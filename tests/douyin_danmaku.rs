//! Offline synthetic tests for the Douyin danmaku migration.
//!
//! Every fixture here is synthesised in-process: no network, no real platform
//! connection, no legacy user data. The signing vectors are the two public
//! upstream vectors recorded by the legacy test suite (DanmakuTests.TestDouyinSign).

use flate2::{Compression, write::GzEncoder};
use live_hub_rust::danmaku::{
    DanmakuEvent, DanmakuMode, LiveEventKind, MAX_EVENTS_PER_FRAME, MAX_EXPANDED_BYTES,
    MAX_TEXT_CHARS, MAX_WIRE_BYTES,
};
use live_hub_rust::danmaku_douyin::*;
use std::io::Write;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Synthetic protobuf fixtures
// ---------------------------------------------------------------------------

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn proto_bytes(tag: u32, bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut key = u64::from(tag) << 3 | 2;
    while key >= 0x80 {
        output.push((key as u8) | 0x80);
        key >>= 7;
    }
    output.push(key as u8);
    let mut length = bytes.len() as u64;
    while length >= 0x80 {
        output.push((length as u8) | 0x80);
        length >>= 7;
    }
    output.push(length as u8);
    output.extend_from_slice(bytes);
    output
}

fn proto_varint(tag: u32, value: u64) -> Vec<u8> {
    let mut output = Vec::new();
    let mut key = u64::from(tag) << 3;
    while key >= 0x80 {
        output.push((key as u8) | 0x80);
        key >>= 7;
    }
    output.push(key as u8);
    let mut value = value;
    while value >= 0x80 {
        output.push((value as u8) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
    output
}

fn chat_payload(text: &str, nickname: &str) -> Vec<u8> {
    let user = proto_bytes(3, nickname.as_bytes());
    let mut chat = proto_bytes(2, &user);
    chat.extend(proto_bytes(3, text.as_bytes()));
    chat
}

fn message(method: &str, payload: &[u8]) -> Vec<u8> {
    let mut message = proto_bytes(1, method.as_bytes());
    message.extend(proto_bytes(2, payload));
    message
}

fn response(messages: &[Vec<u8>], internal_ext: Option<&str>) -> Vec<u8> {
    let mut output = Vec::new();
    for message in messages {
        output.extend(proto_bytes(1, message));
    }
    if let Some(internal_ext) = internal_ext {
        output.extend(proto_bytes(5, internal_ext.as_bytes()));
    }
    output
}

fn push_frame(payload: &[u8], payload_type: &str) -> Vec<u8> {
    encode_push_frame(0, payload_type, &[], payload)
}

fn chat_frame(text: &str, nickname: &str) -> Vec<u8> {
    let body = gzip(&response(
        &[message("WebcastChatMessage", &chat_payload(text, nickname))],
        None,
    ));
    push_frame(&body, "msg")
}

// ---------------------------------------------------------------------------
// DouyinSign
// ---------------------------------------------------------------------------

const STUB: &str = "704f436b2558b0d7a1c7e758527dd8f1";
const SIGN_ALPHABET: &str =
    "Dkdpgh4ZKsQB80/Mfvw36XI1R25+WUAlEi7NLboqYTOPuzmFjJnryx9HVGcaStCe";

#[test]
fn signing_matches_the_two_public_upstream_vectors() {
    assert_eq!(
        sign_with_values(STUB, 1, true, 63, 63).unwrap(),
        "6pt0VC0aRZDP7n07"
    );
    assert_eq!(
        sign_with_values(STUB, 0, false, 0, 6).unwrap(),
        "fD+evFuzasjOC8Uu"
    );
    // The signature alphabet deliberately contains + and /, so the join URL
    // must percent-encode it (covered below).
    assert!(sign_with_values(STUB, 0, false, 0, 6).unwrap().contains('+'));
}

#[test]
fn online_signing_is_short_alphabet_bounded_and_never_cached() {
    let first = sign(STUB).unwrap();
    let second = sign(STUB).unwrap();
    assert_eq!(first.chars().count(), 16);
    assert_eq!(second.chars().count(), 16);
    assert!(first.chars().all(|character| SIGN_ALPHABET.contains(character)));
    assert!(second.chars().all(|character| SIGN_ALPHABET.contains(character)));
    assert_ne!(first, second, "counter/random values must change each signature");
}

#[test]
fn signing_rejects_malformed_stub_hex() {
    assert!(sign_with_values("abc", 0, false, 0, 0).is_err());
    assert!(sign_with_values("zz", 0, false, 0, 0).is_err());
    assert!(sign_with_values("", 0, false, 0, 0).is_ok());
}

#[test]
fn percent_encoding_matches_uri_escape_data_string_semantics() {
    assert_eq!(percent_encode("fD+evFuzasjOC8Uu"), "fD%2BevFuzasjOC8Uu");
    assert_eq!(percent_encode("abcXYZ-._~09"), "abcXYZ-._~09");
    assert_eq!(percent_encode("a b&c=d/e"), "a%20b%26c%3Dd%2Fe");
    assert_eq!(percent_encode("中文"), "%E4%B8%AD%E6%96%87");
    assert_eq!(percent_encode("/"), "%2F");
}

#[test]
fn join_stub_matches_the_legacy_parameter_block() {
    let stub = join_stub("723910165684", "7600000000000000001");
    assert_eq!(
        stub,
        "live_id=1,aid=6383,version_code=180800,webcast_sdk_version=1.0.15,\
         room_id=723910165684,sub_room_id=,sub_channel_id=,did_rule=3,\
         user_unique_id=7600000000000000001,device_platform=web,device_type=,ac=,identity=audience"
    );
    // md5 of that exact block becomes X-MS-STUB; pinned so the signing input
    // cannot drift silently.
    assert_eq!(md5_hex(stub.as_bytes()), "8ead2ee9e944690e33da5c59d92aabe5");
}

#[test]
fn join_url_embeds_parameters_and_percent_encodes_the_signature() {
    let request = JoinRequest {
        host: "webcast100-ws-web-lq.douyin.com",
        room_id: "723910165684",
        user_unique_id: "7600000000000000001",
        signature: "fD+evFuzasjOC8Uu",
        now_ms: 1_750_000_000_000,
        cursor_random: 1234,
        wrds_random: 5678,
    };
    let url = build_join_url(&request).unwrap();
    assert!(url.starts_with(
        "wss://webcast100-ws-web-lq.douyin.com/webcast/im/push/v2/?app_name=douyin_web&version_code=180800"
    ));
    assert!(url.contains("&webcast_sdk_version=1.0.15&update_version_code=1.0.15"));
    assert!(url.contains("&compress=gzip&device_platform=web&cookie_enabled=true"));
    assert!(url.contains("&room_id=723910165684"));
    assert!(url.contains("&user_unique_id=7600000000000000001"));
    assert!(url.contains("&heartbeatDuration=0"));
    // signature contains '+' and '/': both must be escaped.
    assert!(url.contains("&signature=fD%2BevFuzasjOC8Uu"));
    // internal_ext ':' and '|' are escaped, the fixed 'host=' value is not.
    assert!(url.contains("&internal_ext=internal_src%3Adim%7Cwss_push_room_id%3A723910165684"));
    assert!(url.contains("&host=https://live.douyin.com&aid=6383"));
    // cursor / internal_ext random components and browser fingerprint.
    let base = (1_750_000_000_000_u64 / 1000) * 4_294_967_296;
    assert!(url.contains(&format!(
        "&cursor=t-1750000000000_r-{}_d-1_u-1_h-{}",
        base + 1234,
        base + 5678
    )));
    assert!(url.contains(&format!("wrds_v%3A{}", base + 5678)));
    assert!(url.contains("&internal_ext=internal_src%3Adim%7C"));
    assert!(url.contains(&format!("fetch_time%3A1750000000000%7Cseq%3A1")));
    assert!(url.contains("&browser_version=5.0%20%28Windows"));
}

#[test]
fn join_url_rejects_unsafe_hosts_and_ids() {
    let base = JoinRequest {
        host: "webcast100-ws-web-lq.douyin.com",
        room_id: "723910165684",
        user_unique_id: "7600000000000000001",
        signature: "fD+evFuzasjOC8Uu",
        now_ms: 1_750_000_000_000,
        cursor_random: 0,
        wrds_random: 0,
    };
    assert!(build_join_url(&JoinRequest { host: "evil.example", ..base }).is_err());
    assert!(
        build_join_url(&JoinRequest { host: "webcast.douyin.com.evil.com", ..base }).is_err()
    );
    assert!(build_join_url(&JoinRequest { room_id: "room-1", ..base }).is_err());
    assert!(build_join_url(&JoinRequest { user_unique_id: "", ..base }).is_err());
    assert!(build_join_url(&JoinRequest { signature: "bad signature", ..base }).is_err());
    assert!(build_join_url(&JoinRequest { signature: "", ..base }).is_err());
    assert_eq!(WEBCAST_HOSTS.len(), 3);
    for host in WEBCAST_HOSTS {
        assert!(build_join_url(&JoinRequest { host, ..base }).is_ok(), "{host}");
    }
}

// ---------------------------------------------------------------------------
// PushFrame / protobuf codecs
// ---------------------------------------------------------------------------

#[test]
fn heartbeat_and_ack_packets_match_the_legacy_wire_layout() {
    assert_eq!(heartbeat_packet(), vec![0x3A, 0x02, 0x68, 0x62]);
    assert_eq!(
        ack_packet(0x1234, "x"),
        vec![0x10, 0xB4, 0x24, 0x3A, 0x03, 0x61, 0x63, 0x6B, 0x42, 0x01, 0x78]
    );
    let ack = ack_packet(77, "ext|value");
    let parsed = parse_push_frame(&ack).unwrap();
    assert_eq!(parsed.log_id, 77);
    assert_eq!(parsed.payload_type, "ack");
    assert_eq!(parsed.payload, b"ext|value");
}

#[test]
fn push_frame_roundtrip_preserves_headers_and_payload() {
    let encoded = encode_push_frame(424242, "msg", &[("key", "value")], b"payload-bytes");
    let parsed = parse_push_frame(&encoded).unwrap();
    assert_eq!(parsed.log_id, 424242);
    assert_eq!(parsed.payload_type, "msg");
    assert_eq!(parsed.headers, vec![("key", "value")]);
    assert_eq!(parsed.payload, b"payload-bytes");
    // Zero log_id and empty payload are omitted, keeping the heartbeat minimal.
    assert_eq!(
        encode_push_frame(0, "hb", &[], b""),
        vec![0x3A, 0x02, 0x68, 0x62]
    );
}

#[test]
fn decode_chat_from_gzipped_response_matches_the_legacy_fixture() {
    let events = decode(&chat_frame("Hello", "Synthetic")).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        DanmakuEvent::Message(message) => {
            assert_eq!(message.text, "Hello");
            assert_eq!(message.user, "Synthetic");
            assert_eq!(message.color, 0xFFFFFF);
            assert_eq!(message.mode, DanmakuMode::Scroll);
            assert_eq!(message.font_size, 25);
            assert_eq!(message.id, "");
        }
        other => panic!("expected a chat message, got {other:?}"),
    }
}

#[test]
fn decode_chat_from_plain_response_and_bounds_text() {
    let long_text = "合".repeat(2000);
    let body = response(
        &[message(
            "WebcastChatMessage",
            &chat_payload(&long_text, "Synthetic"),
        )],
        None,
    );
    let events = decode(&push_frame(&body, "msg")).unwrap();
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        DanmakuEvent::Message(message) if message.text.chars().count() == MAX_TEXT_CHARS
    ));
    // Empty content produces no event.
    let empty = response(&[message("WebcastChatMessage", &proto_bytes(3, b""))], None);
    assert!(decode(&push_frame(&empty, "msg")).unwrap().is_empty());
    // An empty frame is a valid no-op PushFrame.
    assert!(decode(&[]).unwrap().is_empty());
}

#[test]
fn heartbeat_frame_is_reported_without_events() {
    let inspection = inspect_frame(&heartbeat_packet()).unwrap();
    assert!(inspection.heartbeat);
    assert!(inspection.events.is_empty());
    assert!(inspection.internal_ext.is_none());
    assert!(decode(&heartbeat_packet()).unwrap().is_empty());
}

#[test]
fn ack_internal_ext_and_log_id_are_exposed_for_the_ack_packet() {
    let body = response(
        &[message(
            "WebcastChatMessage",
            &chat_payload("Hi", "Synthetic"),
        )],
        Some("internal_src:dim|seq:1"),
    );
    let frame = encode_push_frame(987_654_321, "msg", &[], &gzip(&body));
    let inspection = inspect_frame(&frame).unwrap();
    assert_eq!(inspection.log_id, 987_654_321);
    assert_eq!(inspection.internal_ext.as_deref(), Some("internal_src:dim|seq:1"));
    assert_eq!(inspection.events.len(), 1);
    let ack = ack_packet(
        inspection.log_id,
        inspection.internal_ext.as_deref().unwrap(),
    );
    let parsed = parse_push_frame(&ack).unwrap();
    assert_eq!(parsed.payload_type, "ack");
    assert_eq!(parsed.payload, b"internal_src:dim|seq:1");
}

#[test]
fn decode_rejects_truncated_and_oversized_input_without_panicking() {
    // Truncated PushFrame payload field.
    assert!(decode(&[0x42, 0x7F, 0x01]).is_err());
    // Frame over the wire budget.
    assert!(decode(&vec![0u8; MAX_WIRE_BYTES + 1]).is_err());
    // Response field length exceeds the remaining bytes.
    assert!(decode(&push_frame(&[0x0A, 0x7F, 0x00], "msg")).is_err());
    // Unterminated varint inside the response.
    assert!(decode(&push_frame(&[0x0A], "msg")).is_err());
    // wire type 3 (deprecated groups) is rejected.
    assert!(decode(&push_frame(&[0x0B], "msg")).is_err());
    // Field number zero.
    assert!(decode(&push_frame(&[0x00], "msg")).is_err());
    // Varint overflow (10th byte greater than one).
    let mut overflow = vec![0x08];
    overflow.extend_from_slice(&[0xFF; 9]);
    overflow.push(0x02);
    assert!(decode(&push_frame(&overflow, "msg")).is_err());
    // Gzip expansion over the shared budget.
    let bomb = gzip(&vec![0u8; MAX_EXPANDED_BYTES + 1]);
    assert!(decode(&push_frame(&bomb, "msg")).is_err());
}

#[test]
fn decode_rejects_too_many_messages_and_fields() {
    let mut messages = Vec::new();
    for index in 0..=MAX_EVENTS_PER_FRAME {
        messages.push(message(
            "WebcastChatMessage",
            &chat_payload(&format!("m{index}"), "Synthetic"),
        ));
    }
    let frame = push_frame(&response(&messages, None), "msg");
    assert!(decode(&frame).is_err(), "event count must be bounded");

    let mut fields = Vec::new();
    for _ in 0..5000 {
        fields.extend(proto_varint(99, 1));
    }
    let frame = push_frame(&fields, "msg");
    assert!(decode(&frame).is_err(), "field count must be bounded");
}

#[test]
fn decode_skips_unknown_fields_and_non_whitelisted_methods() {
    // Unknown fields inside Response are skipped (varint, fixed32, fixed64 and
    // length-delimited).
    let mut body = proto_varint(9, 42);
    body.extend_from_slice(&[0x4D, 1, 2, 3, 4]); // field 9, wire type 5
    body.extend_from_slice(&[0x49, 1, 2, 3, 4, 5, 6, 7, 8]); // field 9, wire type 1
    body.extend(proto_bytes(11, b"ignored"));
    assert!(decode(&push_frame(&body, "msg")).unwrap().is_empty());

    // Douyin has no super-chat equivalent in the legacy reference, and the
    // remaining unported rich types (social follow/share, room sequence, emoji
    // chat) are still dropped by the method whitelist.
    let mut messages = Vec::new();
    for method in [
        "WebcastSuperChatMessage",
        "WebcastSocialMessage",
        "WebcastRoomUserSeqMessage",
        "WebcastEmojiChatMessage",
    ] {
        messages.push(message(method, &proto_varint(1, 42)));
    }
    messages.push(message(
        "WebcastChatMessage",
        &chat_payload("kept", "Synthetic"),
    ));
    let events = decode(&push_frame(&gzip(&response(&messages, None)), "msg")).unwrap();
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], DanmakuEvent::Message(message) if message.text == "kept"));
}

#[test]
fn chat_message_tolerates_extra_nested_user_fields() {
    let mut user = proto_varint(1, 7); // user id
    user.extend(proto_bytes(3, b"Synthetic"));
    user.extend(proto_varint(6, 5)); // level
    let image = proto_bytes(1, b"https://example.invalid/avatar.png");
    user.extend(proto_bytes(9, &image)); // avatar image (skipped)
    let mut chat = proto_bytes(2, &user);
    chat.extend(proto_bytes(3, b"Hi with extras"));
    let events = decode(&push_frame(
        &response(&[message("WebcastChatMessage", &chat)], None),
        "msg",
    ))
    .unwrap();
    assert!(matches!(
        &events[0],
        DanmakuEvent::Message(message) if message.text == "Hi with extras" && message.user == "Synthetic"
    ));
}

// ---------------------------------------------------------------------------
// Rich live events: gift / member-enter / like (synthetic protobuf bytes)
// ---------------------------------------------------------------------------
//
// The bytes below are hand-built from the field numbers pinned in the
// danmaku_douyin.rs module header (two MIT-licensed public Douyin webcast
// definitions, verified but not live-tested). If a field number ever drifts,
// these fixtures fail here instead of only on the real platform.

/// `Common.msg_id` (field 2), the shared id used for overlay de-duplication.
fn common_msg_id(msg_id: u64) -> Vec<u8> {
    proto_varint(2, msg_id)
}

/// `User { uint64 id = 1; string nickname = 3; ... }`
fn user_payload(id: u64, nickname: &str) -> Vec<u8> {
    let mut user = proto_varint(1, id);
    user.extend(proto_bytes(3, nickname.as_bytes()));
    user
}

/// `GiftMessage` with a nested `GiftStruct.name` (field 16).
fn gift_payload(msg_id: u64, nickname: &str, gift: &str, repeat: u64, combo: u64) -> Vec<u8> {
    let mut gift_struct = proto_bytes(16, gift.as_bytes());
    gift_struct.extend(proto_varint(12, 9)); // GiftStruct.diamond_count (not read)
    let mut payload = proto_bytes(1, &common_msg_id(msg_id));
    payload.extend(proto_varint(2, 4242)); // gift_id
    payload.extend(proto_varint(5, repeat)); // repeat_count
    payload.extend(proto_varint(6, combo)); // combo_count
    payload.extend(proto_bytes(7, &user_payload(7, nickname))); // user
    payload.extend(proto_varint(9, 1)); // repeat_end
    payload.extend(proto_bytes(15, &gift_struct)); // gift
    payload
}

/// `MemberMessage`.
fn member_payload(msg_id: u64, nickname: &str) -> Vec<u8> {
    let mut payload = proto_bytes(1, &common_msg_id(msg_id));
    payload.extend(proto_bytes(2, &user_payload(11, nickname))); // user
    payload.extend(proto_varint(3, 88)); // member_count (not read)
    payload.extend(proto_varint(10, 1)); // action (deliberately not interpreted)
    payload
}

/// `LikeMessage`.
fn like_payload(count: u64, total: u64) -> Vec<u8> {
    let mut payload = proto_bytes(1, &common_msg_id(9));
    payload.extend(proto_varint(2, count)); // count
    payload.extend(proto_varint(3, total)); // total
    payload.extend(proto_bytes(5, &user_payload(5, "Synthetic"))); // user
    payload
}

#[test]
fn decode_gift_member_like_live_events_from_verified_field_numbers() {
    let events = decode(&push_frame(
        &gzip(&response(
            &[
                message(
                    "WebcastGiftMessage",
                    &gift_payload(77, "合成观众", "合成礼物", 3, 7),
                ),
                message("WebcastMemberMessage", &member_payload(78, "合成新观众")),
                message("WebcastLikeMessage", &like_payload(2, 4321)),
                message("WebcastChatMessage", &chat_payload("kept", "Synthetic")),
            ],
            None,
        )),
        "msg",
    ))
    .unwrap();
    assert_eq!(events.len(), 4, "gift, enter, like and chat all decode");
    assert!(
        matches!(&events[0], DanmakuEvent::Live(event)
            if event.kind == LiveEventKind::Gift
                && event.user == "合成观众"
                && event.text == "合成礼物"
                && event.count == 3
                && event.id == "77"),
        "gift: {:?}",
        events[0]
    );
    assert!(
        matches!(&events[1], DanmakuEvent::Live(event)
            if event.kind == LiveEventKind::Enter
                && event.user == "合成新观众"
                && event.text == "进入直播间"
                && event.count == 1
                && event.id == "78"),
        "enter: {:?}",
        events[1]
    );
    assert!(
        matches!(&events[2], DanmakuEvent::Live(event)
            if event.kind == LiveEventKind::Status
                && event.user.is_empty()
                && event.text == "点赞 4321"
                && event.id.is_empty()),
        "like: {:?}",
        events[2]
    );
    assert!(matches!(&events[3], DanmakuEvent::Message(message) if message.text == "kept"));
}

#[test]
fn gift_count_prefers_repeat_count_and_falls_back_to_combo_count() {
    let events = decode(&push_frame(
        &response(
            &[
                message("WebcastGiftMessage", &gift_payload(1, "u", "g", 4, 9)),
                message("WebcastGiftMessage", &gift_payload(2, "u", "g", 0, 9)),
                message("WebcastGiftMessage", &gift_payload(3, "u", "g", 0, 0)),
            ],
            None,
        ),
        "msg",
    ))
    .unwrap();
    let counts: Vec<u32> = events
        .iter()
        .filter_map(|event| match event {
            DanmakuEvent::Live(event) if event.kind == LiveEventKind::Gift => Some(event.count),
            _ => None,
        })
        .collect();
    assert_eq!(counts, vec![4, 9, 1], "repeat_count wins, combo_count backs it up, never 0");
}

#[test]
fn malformed_or_incomplete_rich_payloads_are_skipped_without_dropping_chat() {
    // A length-delimited field whose declared length runs past the payload end:
    // the gift parser fails, but the failure is contained per message.
    let mut truncated_gift = vec![0x0A, 0x7F]; // Common (field 1), len 127
    truncated_gift.extend(proto_bytes(15, b"x"));
    let events = decode(&push_frame(
        &response(
            &[
                // No user and no gift name: no row is invented.
                message("WebcastGiftMessage", &proto_varint(1, 42)),
                message("WebcastGiftMessage", &truncated_gift),
                // No user nickname: the member row is dropped.
                message("WebcastMemberMessage", &proto_varint(1, 1)),
                // count = total = 0: no empty "点赞 0" row.
                message("WebcastLikeMessage", &proto_varint(2, 0)),
                message("WebcastChatMessage", &chat_payload("kept", "Synthetic")),
            ],
            None,
        ),
        "msg",
    ))
    .unwrap();
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], DanmakuEvent::Message(message) if message.text == "kept"));
}

#[test]
fn rich_live_events_are_bounded_by_the_single_frame_event_budget() {
    let mut messages = Vec::new();
    for index in 0..=MAX_EVENTS_PER_FRAME {
        messages.push(message(
            "WebcastGiftMessage",
            &gift_payload(index as u64 + 1, "u", "g", 1, 1),
        ));
    }
    assert!(
        decode(&push_frame(&response(&messages, None), "msg")).is_err(),
        "per-frame live-event count must stay bounded"
    );
}

// ---------------------------------------------------------------------------
// Heartbeat watch / reconnect policy
// ---------------------------------------------------------------------------

#[test]
fn heartbeat_watch_requires_two_missed_acks_and_ack_resets() {
    let mut watch = HeartbeatAckWatch::new(HEARTBEAT_ACK_TIMEOUT_MS);
    assert_eq!(watch.missed(), 0);
    watch.begin(0);
    assert!(!watch.should_reconnect(4_999));
    assert!(!watch.should_reconnect(5_000));
    assert_eq!(watch.missed(), 1);
    // A single miss is not fatal; send/ack another heartbeat.
    watch.begin(6_000);
    assert!(!watch.should_reconnect(10_999));
    assert!(watch.should_reconnect(11_000));
    watch.acknowledge();
    assert_eq!(watch.missed(), 0);
    assert!(!watch.should_reconnect(60_000));
}

#[test]
fn heartbeat_watch_timeout_is_clamped_to_five_seconds() {
    let mut watch = HeartbeatAckWatch::new(60_000);
    watch.begin(0);
    assert!(!watch.should_reconnect(4_999));
    assert!(!watch.should_reconnect(5_000));
    assert_eq!(watch.missed(), 1);
}

#[test]
fn reconnect_policy_is_finite_and_backoff_is_bounded() {
    assert_eq!(MAX_RECONNECTS, 3);
    assert_eq!(reconnect_delay(0), Duration::from_secs(3));
    assert_eq!(reconnect_delay(1), Duration::from_secs(3));
    assert_eq!(reconnect_delay(2), Duration::from_secs(6));
    assert_eq!(reconnect_delay(3), Duration::from_secs(9));
    assert_eq!(reconnect_delay(10), Duration::from_secs(30));
    assert!(reconnect_delay(u32::MAX) <= Duration::from_secs(30));
}

// ---------------------------------------------------------------------------
// Room page parsing (offline synthetic fixtures)
// ---------------------------------------------------------------------------

fn flight_page(payload: &str, prefix: &str, marker: &str, extra: &str) -> String {
    let flight = format!("{prefix}{payload}");
    let literal = serde_json::to_string(&flight).unwrap();
    format!("<html><script>self.{marker}.push([1,{literal}])</script>{extra}</html>")
}

fn room_entry(status: i64, id: &str) -> String {
    serde_json::json!({
        "state": {
            "roomStore": {
                "roomInfo": {
                    "room": {
                        "status": status,
                        "id_str": id,
                        "title": "合成房间",
                        "owner": { "nickname": "合成主播" }
                    }
                }
            }
        }
    })
    .to_string()
}

#[test]
fn room_page_parsing_extracts_identity_from_flight_payload() {
    let extra = "<script>var data = {\"user_unique_id\":\"7600000000000000001\"}</script>";
    let page = flight_page(&room_entry(2, "723910165684"), "7:", "__pace_f", extra);
    let info = parse_room_page(&page).unwrap();
    assert_eq!(info.room_id, "723910165684");
    assert_eq!(info.user_unique_id, "7600000000000000001");
    assert_eq!(info.status, 2);
    assert_eq!(info.title, "合成房间");
    assert_eq!(info.owner, "合成主播");
}

#[test]
fn room_page_parsing_handles_array_payload_next_marker_and_escaped_uid() {
    let extra = "<script>var data = {\"user_unique_id\":7600000000000000002}</script>";
    let array = format!("[{}]", room_entry(4, "723910165685"));
    let page = flight_page(&array, "1:", "__next_f", extra);
    let info = parse_room_page(&page).unwrap();
    assert_eq!(info.room_id, "723910165685");
    assert_eq!(info.status, 4, "ended rooms still expose a room id");
    // Escaped JSON inside a script attributes string also yields a uid.
    let escaped_page = flight_page(
        &room_entry(2, "723910165684"),
        "7:",
        "__pace_f",
        "<script>var x = \\\"user_unique_id\\\":\\\"7600000000000000003\\\";</script>",
    );
    let escaped = parse_room_page(&escaped_page).unwrap();
    assert_eq!(escaped.user_unique_id, "7600000000000000003");
}

#[test]
fn room_page_parsing_reports_missing_room_and_captcha_pages() {
    let shell = serde_json::json!({ "state": { "roomStore": { "roomInfo": {} } } });
    let page = flight_page(&shell.to_string(), "7:", "__pace_f", "");
    assert!(parse_room_page(&page).is_err());
    assert!(parse_room_page("<html>captcha verify 验证码</html>").is_err());
    assert!(parse_room_page("").is_err());
    let error = parse_room_page("<html>captcha 验证码</html>").unwrap_err();
    assert!(error.to_string().contains("验证"));
}

// ---------------------------------------------------------------------------
// Failure classification (offline synthetic; mirrors the live observations)
// ---------------------------------------------------------------------------

#[test]
fn room_response_classification_separates_login_risk_and_transport() {
    use RoomFailure::*;
    // Human login/verification required.
    assert_eq!(classify_room_response(401, None, ""), Some(LoginRequired));
    assert_eq!(classify_room_response(403, None, ""), Some(LoginRequired));
    // Rate limit / anti-bot.
    assert_eq!(classify_room_response(429, None, ""), Some(RiskControl));
    // Observed 2026-10-29: a non-browser User-Agent got HTTP 200,
    // application/json and a 0-byte body instead of the room page.
    assert_eq!(classify_room_response(200, Some("application/json"), ""), Some(RiskControl));
    assert_eq!(
        classify_room_response(200, Some("application/json; charset=utf-8"), "{\"status_code\":0}"),
        Some(RiskControl)
    );
    // A normal page or a server-side error is not risk control.
    assert_eq!(classify_room_response(200, Some("text/html"), "<html>room</html>"), None);
    assert_eq!(classify_room_response(500, None, ""), None);
    assert!(LoginRequired.message().contains("登录"));
    assert!(LoginRequired.message().contains("auth import"));
    assert!(RiskControl.message().contains("限流"));
    assert!(Malformed.message().contains("格式"));
}

#[test]
fn room_page_classification_tells_empty_shell_rate_limit_from_format_change() {
    use RoomFailure::*;
    // Live fixture still parses.
    let live = flight_page(&room_entry(2, "723910165684"), "7:", "__pace_f", "");
    assert!(parse_room_page_classified(&live).is_ok());
    // roomStore with an empty roomInfo object: nonexistent room OR short-term
    // rate limit (same template) -> retryable risk control, not "page changed".
    let shell = flight_page(
        &serde_json::json!({ "state": { "roomStore": { "roomInfo": {} } } }).to_string(),
        "7:",
        "__pace_f",
        "",
    );
    assert_eq!(parse_room_page_classified(&shell), Err(RiskControl));
    // Genuine small captcha/intermediate page needs a human step.
    assert_eq!(
        parse_room_page_classified("<html>验证码中间页 captcha verify</html>"),
        Err(LoginRequired)
    );
    // Regression: normal ~1 MB pages mention "verify"/"captcha" in script names
    // (verifyFp); a large empty-shell page must stay retryable risk control and
    // must not be misreported as a captcha wall.
    let padding = "x".repeat(80 * 1024);
    let big_shell = flight_page(
        &serde_json::json!({ "state": { "roomStore": { "roomInfo": {} } } }).to_string(),
        "7:",
        "__pace_f",
        &format!("<script>var verifyFp='verify_'; // captcha {padding}</script>"),
    );
    assert_eq!(parse_room_page_classified(&big_shell), Err(RiskControl));
    assert!(parse_room_page("<html>captcha 验证码</html>").unwrap_err().to_string().contains("验证"));
    // Genuinely changed/unrecognised page.
    assert_eq!(
        parse_room_page_classified("<html><body>totally new layout</body></html>"),
        Err(Malformed)
    );
    assert_eq!(parse_room_page_classified(""), Err(Malformed));
}

#[test]
fn offline_room_status_is_parsed_state_not_a_failure() {
    // Danmaku keeps waiting on an ended room (legacy behaviour); the parser must
    // expose status 4 instead of classifying it as a transport failure.
    let ended = flight_page(&room_entry(4, "723910165685"), "7:", "__pace_f", "");
    let info = parse_room_page_classified(&ended).unwrap();
    assert_eq!(info.status, 4);
}

#[test]
fn cookie_value_extracts_ttwid() {
    assert_eq!(
        cookie_value("ttwid=abc123; Path=/; Domain=.douyin.com", "ttwid").as_deref(),
        Some("abc123")
    );
    assert_eq!(
        cookie_value("other=1; ttwid=xyz", "ttwid").as_deref(),
        Some("xyz")
    );
    assert_eq!(cookie_value("TTWID=upper", "ttwid").as_deref(), Some("upper"));
    assert!(cookie_value("nope=1", "ttwid").is_none());
}
