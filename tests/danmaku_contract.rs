use flate2::{
    Compression,
    write::{GzEncoder, ZlibEncoder},
};
use live_hub_rust::{core::Platform, danmaku::*};
use std::io::Write;

fn chat(text: &str) -> DanmakuMessage {
    DanmakuMessage {
        image_url: None,
        id: "synthetic".into(),
        user: "Synthetic User".into(),
        text: text.into(),
        color: 0xffffff,
        mode: DanmakuMode::Scroll,
        font_size: 25,
        user_id: 0,
        user_level: 0,
        guard_level: 0,
        medal: None,
        image_width: 0,
        image_height: 0,
        image_large: false,
    }
}

#[test]
fn bounded_queue_drops_without_blocking_and_reports_disconnection() {
    let (sender, receiver) = bounded_channel(1);
    assert_eq!(
        emit(&sender, DanmakuEvent::Message(chat("first"))),
        Delivery::Sent
    );
    assert_eq!(
        emit(&sender, DanmakuEvent::Message(chat("second"))),
        Delivery::Dropped
    );
    assert_eq!(
        receiver.try_recv().unwrap(),
        DanmakuEvent::Message(chat("first"))
    );
    drop(receiver);
    assert_eq!(
        emit(&sender, DanmakuEvent::Message(chat("third"))),
        Delivery::Closed
    );
    let cancellation = Cancellation::new();
    let clone = cancellation.clone();
    clone.cancel();
    assert!(cancellation.is_cancelled());
}

fn bili_chat() -> Vec<u8> {
    let body = serde_json::to_vec(&serde_json::json!({"cmd":"DANMU_MSG:4:0:2:2:2:0","info":[[0,1,25,16711680,0,123],"合成消息",[7,"Synthetic User"]]})).unwrap();
    bili_packet(5, 0, &body).unwrap()
}

#[test]
fn bilibili_decodes_concatenated_and_zlib_packets() {
    let mut raw = bili_chat();
    raw.extend_from_slice(&bili_chat());
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&raw).unwrap();
    let compressed = bili_packet(5, 2, &encoder.finish().unwrap()).unwrap();
    let events = decode_bili(&compressed).unwrap();
    assert_eq!(events.len(), 2);
    match &events[0] {
        DanmakuEvent::Message(message) => {
            assert_eq!(message.text, "合成消息");
            assert_eq!(message.user, "Synthetic User");
            assert_eq!(message.color, 0xff0000);
        }
        _ => panic!("expected chat"),
    }
}

#[test]
fn bilibili_rejects_truncation_bad_auth_and_brotli_instead_of_silent_success() {
    let packet = bili_chat();
    for length in [1, 8, packet.len() - 1] {
        assert!(decode_bili(&packet[..length]).is_err());
    }
    assert!(decode_bili(&bili_packet(8, 1, br#"{"code":-101}"#).unwrap()).is_err());
    assert!(decode_bili(&bili_packet(5, 3, b"not-brotli").unwrap()).is_err());
    let mut bad = packet;
    bad[0..4].copy_from_slice(&0_u32.to_be_bytes());
    assert!(decode_bili(&bad).is_err());
}

#[test]
fn bilibili_compression_budget_is_shared_and_strings_are_bounded() {
    let text = "合".repeat(2000);
    let body = serde_json::to_vec(
        &serde_json::json!({"cmd":"DANMU_MSG","info":[[0,1,25,0],text,[0,"Synthetic"]]}),
    )
    .unwrap();
    let events = decode_bili(&bili_packet(5, 0, &body).unwrap()).unwrap();
    assert!(
        matches!(&events[0], DanmakuEvent::Message(message) if message.text.chars().count() == MAX_TEXT_CHARS)
    );
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&vec![0; MAX_EXPANDED_BYTES + 1]).unwrap();
    assert!(decode_bili(&bili_packet(5, 2, &encoder.finish().unwrap()).unwrap()).is_err());
}

#[test]
fn stt_escaping_is_single_pass_in_the_correct_order_and_tcp_is_incremental() {
    let payload = encode_stt(&[
        ("type", "chatmsg"),
        ("txt", "literal @S and @ / /"),
        ("nn", "Synthetic"),
    ]);
    assert_eq!(decode_stt(&payload).unwrap()["txt"], "literal @S and @ / /");
    let frame = douyu_packet(&payload, true).unwrap();
    for split in 0..frame.len() {
        let mut decoder = DouyuDecoder::default();
        let first = decoder.push(&frame[..split]).unwrap();
        assert!(first.events.is_empty());
        let batch = decoder.push(&frame[split..]).unwrap();
        assert!(
            matches!(&batch.events[0],DanmakuEvent::Message(message) if message.text == "literal @S and @ / /")
        );
    }
}

#[test]
fn douyu_checks_repeated_length_and_decodes_nested_superchat_once() {
    let nested = encode_stt(&[
        ("type", "chatmsg"),
        ("txt", "Synthetic paid message"),
        ("nn", "Synthetic"),
    ]);
    let body = encode_stt(&[
        ("btype", "voiceDanmu"),
        ("chatmsg", &nested),
        ("cprice", "3000"),
        ("cet", "60"),
        ("type", "comm_chatmsg"),
    ]);
    let events = DouyuDecoder::default()
        .push(&douyu_packet(&body, true).unwrap())
        .unwrap()
        .events;
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0],DanmakuEvent::SuperChat(sc) if sc.price_cents == 3000 && sc.text == "Synthetic paid message")
    );
    let mut bad = douyu_packet("type@=pingreq/", true).unwrap();
    bad[4] ^= 1;
    assert!(DouyuDecoder::default().push(&bad).is_err());
    assert!(
        DouyuDecoder::default()
            .push(&douyu_packet("type@=pingreq/", true).unwrap())
            .unwrap()
            .ping_requested
    );
}

fn proto(tag: u8, bytes: &[u8]) -> Vec<u8> {
    assert!(bytes.len() < 128);
    let mut field = vec![(tag << 3) | 2, bytes.len() as u8];
    field.extend_from_slice(bytes);
    field
}

#[test]
fn douyin_offline_protobuf_gzip_chat_and_explicit_huya_boundary() {
    let user = proto(3, b"Synthetic");
    let mut chat = proto(2, &user);
    chat.extend(proto(3, b"Hello"));
    let mut message = proto(1, b"WebcastChatMessage");
    message.extend(proto(2, &chat));
    let response = proto(1, &message);
    let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
    gzip.write_all(&response).unwrap();
    let frame = proto(8, &gzip.finish().unwrap());
    let events = decode_frame(Platform::Douyin, &frame).unwrap();
    assert!(
        matches!(&events[0],DanmakuEvent::Message(m) if m.text=="Hello" && m.user=="Synthetic")
    );
    assert!(decode_frame(Platform::Huya, b"synthetic").is_err());
    assert!(decode_douyin(&[0x42, 0x7f, 1]).is_err());
}

#[test]
fn rich_message_and_super_chat_documents_stay_backward_compatible() {
    // Older fixture documents that predate the rich fields must still load;
    // every added field carries an explicit default.
    let legacy_message: DanmakuMessage = serde_json::from_str(
        r#"{"id":"x","user":"u","text":"t","color":0,"mode":"Scroll","font_size":25}"#,
    )
    .unwrap();
    assert_eq!(legacy_message.user_id, 0);
    assert_eq!(legacy_message.user_level, 0);
    assert_eq!(legacy_message.guard_level, 0);
    assert!(legacy_message.medal.is_none());
    assert_eq!(
        (legacy_message.image_width, legacy_message.image_height),
        (0, 0)
    );
    assert!(!legacy_message.image_large);

    let legacy_sc: SuperChat = serde_json::from_str(
        r#"{"id":"s","user":"u","text":"t","price_cents":100,"duration_secs":60}"#,
    )
    .unwrap();
    assert_eq!(legacy_sc.background_bottom_color, 0x2a60b2);
    assert_eq!(legacy_sc.message_font_color, 0xa3f6ff);
    assert_eq!(legacy_sc.name_color, 0x666666);
    assert!(legacy_sc.face.is_empty() && legacy_sc.medal.is_none());

    // Every event kind round-trips through the local inspection fixture format.
    for kind in [
        LiveEventKind::Gift,
        LiveEventKind::Guard,
        LiveEventKind::Enter,
        LiveEventKind::Follow,
        LiveEventKind::Notice,
        LiveEventKind::Status,
    ] {
        let event = DanmakuEvent::Live(LiveEvent {
            id: "e".into(),
            kind,
            user: "u".into(),
            text: "t".into(),
            count: 2,
        });
        let encoded = serde_json::to_string(&event).unwrap();
        assert_eq!(
            serde_json::from_str::<DanmakuEvent>(&encoded).unwrap(),
            event
        );
    }
}

#[test]
fn bili_reverse_messages_parse_and_fly_in_the_reverse_direction() {
    let body = serde_json::to_vec(&serde_json::json!({
        "cmd":"DANMU_MSG",
        "info":[[0,6,25,16777215,0,7],"reverse text",[1,"合成"]]
    }))
    .unwrap();
    let events = decode_bili(&bili_packet(5, 0, &body).unwrap()).unwrap();
    let DanmakuEvent::Message(message) = &events[0] else {
        panic!("expected message")
    };
    assert_eq!(message.mode, DanmakuMode::Reverse);
    let mut scheduler = LaneScheduler::new(500.0, 200.0, 20.0, 8.0);
    let placed = scheduler
        .push_measured(message.clone(), 100.0, 0.0)
        .expect("reverse placed");
    let start_x = placed.x(0.0);
    assert!(start_x <= 0.0);
    assert!(placed.x(1.0) > start_x);
    assert!(placed.visible(0.0));
    assert!(!placed.visible(100.0));
}

#[test]
fn emoticon_dictionary_cache_keeps_the_last_working_snapshot() {
    use live_hub_rust::danmaku::rich::{EmoticonDictionary, parse_bili_emoticons};
    let payload = r#"{"code":0,"data":{"packages":[{"emoticons":[
        {"emoji":"[合成笑]","url":"https://i0.hdslb.com/bfs/live/synthetic.png"}]}]}}"#;
    let dictionary = parse_bili_emoticons(payload).unwrap();
    cache_emoticons(dictionary);
    assert_eq!(cached_emoticons().map(|value| value.len()), Some(1));
    // An empty update must not erase a working dictionary.
    cache_emoticons(EmoticonDictionary::new());
    assert!(cached_emoticons().is_some());
}

#[test]
fn scheduler_honours_line_gap_reserved_bands_and_multi_row_height() {
    let mut scheduler = LaneScheduler::new(500.0, 200.0, 20.0, 8.0);
    // Legacy track height: the measured text box plus stroke padding, rounded up
    // (danmaku-3-render.cs Resize/RefreshFlying), then scaled by the line gap.
    let base = (20.0_f32 * 1.33 + 6.0).max(20.0 * 1.18 + 6.0).ceil();
    assert_eq!(base, 33.0);
    assert!((scheduler.line_height() - base).abs() < 1e-3);
    scheduler.set_line_gap_percent(200);
    assert!((scheduler.line_height() - base * 2.0).abs() < 1e-3);
    scheduler.set_line_gap_percent(10);
    // f32 rounding: 33.0 * 0.6 differs from the f32 product in the last bit.
    assert!((scheduler.line_height() - base * 0.6).abs() < 1e-3);
    scheduler.set_line_gap_percent(100);

    // A subtitle band at the bottom keeps scroll lanes above it.
    scheduler.set_reserved_bands(vec![(162.0, 200.0)]);
    let placed = scheduler
        .push_measured(chat("above subtitle"), 100.0, 0.0)
        .expect("place above band");
    assert!(placed.y + placed.height <= 162.0 + 1e-3);
    // A reserved band covering everything refuses new messages.
    scheduler.set_reserved_bands(vec![(0.0, 200.0)]);
    assert!(
        scheduler
            .push_measured(chat("blocked"), 100.0, 0.0)
            .is_none()
    );
    scheduler.set_reserved_bands(Vec::new());

    // A two-row request claims vertical space the next message must avoid.
    let mut scheduler = LaneScheduler::new(500.0, 200.0, 20.0, 8.0);
    let large = scheduler
        .push_measured_rows(chat("large"), 100.0, 2, 0.0)
        .expect("large placed");
    assert!((large.height - 2.0 * base).abs() < 1e-3);
    let next = scheduler
        .push_measured(chat("next"), 100.0, 0.0)
        .expect("placed");
    assert!(
        next.y >= large.y + large.height - 0.1 || next.y + next.height <= large.y + 0.1,
        "{} vs {}",
        next.y,
        large.y
    );
}

#[test]
fn scheduler_uses_multiple_lanes_then_waits_for_the_entry_gap_and_expires() {
    // Track height for 25px is 40 points, so a 120-point picture fits three
    // single-row tracks: the scheduler fills them in order.
    let mut scheduler = LaneScheduler::new(500.0, 120.0, 25.0, 8.0);
    for (index, expected_lane) in [(0, 0_usize), (1, 1), (2, 2)] {
        let lane = scheduler
            .push_measured(chat(&format!("lane{index}")), 100.0, 0.0)
            .unwrap_or_else(|| panic!("lane {expected_lane} was refused"))
            .lane;
        assert_eq!(lane, expected_lane);
    }
    // No track is left; a fourth message waits for the whole flight to clear
    // (legacy PathsOverlap: overlap must be avoided for the remaining life).
    assert!(
        scheduler
            .push_measured(chat("too soon"), 100.0, 0.0)
            .is_none()
    );

    // A single-track picture keeps the entry gap: at 8s per crossing a 500-point
    // picture with 100-point messages needs (100+gap)/speed seconds before the
    // running message has cleared the entry edge (gap = 14 points).
    let mut narrow = LaneScheduler::new(500.0, 70.0, 25.0, 8.0);
    assert_eq!(
        narrow
            .push_measured(chat("first"), 100.0, 0.0)
            .unwrap()
            .lane,
        0
    );
    assert!(
        narrow
            .push_measured(chat("too soon"), 100.0, 0.0)
            .is_none()
    );
    // 1.5s only travels 112.5 points of the required 114.
    assert!(
        narrow
            .push_measured(chat("still too soon"), 100.0, 1.5)
            .is_none()
    );
    assert!(
        narrow
            .push_measured(chat("entry cleared"), 100.0, 1.6)
            .is_some()
    );
    narrow.advance(10.0);
    assert!(narrow.active().is_empty());

    scheduler.advance(10.0);
    assert!(scheduler.active().is_empty());
    assert!(
        scheduler
            .push_measured(chat("bad"), f32::NAN, 10.0)
            .is_none()
    );
}
