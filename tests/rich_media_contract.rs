use base64::Engine;
use live_hub_rust::danmaku::rich::{
    EmojiManifest, EmoticonDictionary, TextSegment, image_dimensions, parse_bili_emoticons,
    parse_douyin_emoticons, sanitize_event_text, split_text_segments,
};
use live_hub_rust::danmaku::*;
use serde_json::{Value, json};
fn bili(value: Value) -> Vec<DanmakuEvent> {
    decode_bili(&bili_packet(5, 0, &serde_json::to_vec(&value).unwrap()).unwrap()).unwrap()
}

#[test]
fn emotes_accept_platform_cdn_only_and_keep_the_text_fallback() {
    let mut info = json!([
        [0, 1, 25, 16777215, 0, 123],
        "[synthetic emote]",
        [7, "synthetic"]
    ]);
    let meta = info[0].as_array_mut().unwrap();
    while meta.len() < 13 {
        meta.push(Value::Null)
    }
    for (address, allowed) in [
        ("https://i0.hdslb.com/synthetic.webp", true),
        ("https://127.0.0.1/private", false),
        ("https://i0.hdslb.com.attacker.invalid/track", false),
    ] {
        info[0][12] = json!({"url":address});
        let events = bili(json!({"cmd":"DANMU_MSG","info":info}));
        let DanmakuEvent::Message(message) = &events[0] else {
            panic!("expected message")
        };
        assert_eq!(message.image_url.is_some(), allowed);
        assert_eq!(message.text, "[synthetic emote]");
    }
}

#[test]
fn bili_rich_message_fields_keep_medal_guard_level_and_emoticon_geometry() {
    // Real layout: head[12] is dm_type, head[13] is the image object.
    let mut head = vec![
        json!(0),
        json!(1),
        json!(25),
        json!(16777215),
        json!(0),
        json!("9001"),
    ];
    while head.len() < 12 {
        head.push(Value::Null);
    }
    head.push(json!(1));
    head.push(json!({
        "url": "https://i0.hdslb.com/synthetic-emote.png",
        "width": "120",
        "height": "60",
        "bulge_display": "1"
    }));
    while head.len() < 15 {
        head.push(Value::Null);
    }
    head.push(json!({"user": {"guard": {"level": 1}, "base": {"name": "新布局昵称"}}}));
    let info = json!([
        head,
        "[合成表情]",
        [42, "合成用户"],
        [12, "合成勋章", 0, 0, 16711680],
        [33, 0, 0],
        Value::Null,
        Value::Null,
        3
    ]);
    let events = bili(json!({"cmd":"DANMU_MSG:4:0:2:2:2:0","info":info}));
    let DanmakuEvent::Message(message) = &events[0] else {
        panic!("expected message")
    };
    assert_eq!(message.user, "合成用户");
    assert_eq!(message.user_id, 42);
    assert_eq!(message.user_level, 33);
    // head[15] guard level wins over the info[7] fallback.
    assert_eq!(message.guard_level, 1);
    let medal = message.medal.as_ref().expect("medal");
    assert_eq!(
        (medal.name.as_str(), medal.level, medal.color),
        ("合成勋章", 12, 0xff0000)
    );
    assert_eq!(message.image_width, 120);
    assert_eq!(message.image_height, 60);
    assert!(message.image_large);
    assert!(message.image_url.is_some());
    assert_eq!(message.text, "[合成表情]");

    // An empty user name falls back to the new-layout base name.
    let mut info = info;
    info[2] = json!([42, ""]);
    info[0][15] = json!({"user": {"guard": {"level": 2}}});
    let events = bili(json!({"cmd":"DANMU_MSG","info":info}));
    let DanmakuEvent::Message(message) = &events[0] else {
        panic!("expected message")
    };
    assert_eq!(message.user, "");
    assert_eq!(message.guard_level, 2);

    // A message without text or image is noise and produces no event.
    let blank = json!({"cmd":"DANMU_MSG","info":[[0,1,25,0],"",[0,"合成"]]});
    assert!(bili(blank).is_empty());
}

#[test]
fn bili_status_notice_and_guard_events_use_typed_kinds() {
    let watched = bili(json!({"cmd":"WATCHED_CHANGE","data":{"num":1200,"text_large":"1200"}}));
    let DanmakuEvent::Live(event) = &watched[0] else {
        panic!("expected live event")
    };
    assert_eq!(event.kind, LiveEventKind::Status);
    assert_eq!(event.text, "看过 1200");
    let likes = bili(json!({"cmd":"LIKE_INFO_V3_UPDATE","data":{"click_count":4321}}));
    let DanmakuEvent::Live(event) = &likes[0] else {
        panic!("expected live event")
    };
    assert_eq!(event.text, "点赞 4321");

    // NOTICE_MSG carries its payload at the root and tags are stripped.
    let notice = bili(json!({"cmd":"NOTICE_MSG","msg_common":"<b>合成公告</b> &amp; 提示"}));
    let DanmakuEvent::Live(event) = &notice[0] else {
        panic!("expected notice")
    };
    assert_eq!(event.kind, LiveEventKind::Notice);
    assert_eq!(event.text, "合成公告 & 提示");

    // Auto-renew guard purchases are duplicate noise.
    let renew = bili(json!({"cmd":"GUARD_BUY","data":{"uname":"合成","guard_level":1,"source":2}}));
    assert!(renew.is_empty());
    let guard = bili(json!({"cmd":"GUARD_BUY","data":{"uname":"合成","guard_level":2}}));
    let DanmakuEvent::Live(event) = &guard[0] else {
        panic!("expected guard")
    };
    assert_eq!(event.kind, LiveEventKind::Guard);
    assert_eq!(event.text, "开通提督");

    // COMBO_SEND reports the cumulative total, not the single batch count.
    let combo = bili(
        json!({"cmd":"COMBO_SEND","data":{"uname":"合成","giftName":"礼物","num":2,"total_num":9}}),
    );
    let DanmakuEvent::Live(event) = &combo[0] else {
        panic!("expected combo")
    };
    assert_eq!(event.count, 9);

    assert_eq!(sanitize_event_text("<i>a</i>&0", 40), "a&0");
}

#[test]
fn bili_super_chat_keeps_avatar_translation_medal_and_colours() {
    let events = bili(json!({"cmd":"SUPER_CHAT_MESSAGE","data":{
        "id":"sc-1",
        "price":30,
        "time":60,
        "message":"合成付费留言",
        "message_trans":"synthetic translation",
        "background_bottom_color":"#2A60B2",
        "background_color_start":"#3171D2",
        "message_font_color":"#A3F6FF",
        "user_info":{
            "uname":"合成舰长",
            "face":"https://i0.hdslb.com/synthetic-face.png",
            "user_level":41,
            "guard_level":3,
            "name_color":"#FF6699"
        },
        "medal_info":{"medal_name":"合成牌子","medal_level":21}
    }}));
    let DanmakuEvent::SuperChat(sc) = &events[0] else {
        panic!("expected super chat")
    };
    assert_eq!(sc.user, "合成舰长");
    assert_eq!(sc.price_cents, 3000);
    assert_eq!(sc.message_trans, "synthetic translation");
    assert!(sc.face.ends_with("synthetic-face.png"));
    assert_eq!(sc.user_level, 41);
    assert_eq!(sc.guard_level, 3);
    assert_eq!(sc.medal.as_ref().map(|m| m.level), Some(21));
    assert_eq!(sc.background_bottom_color, 0x2a60b2);
    assert_eq!(sc.message_font_color, 0xa3f6ff);
    assert_eq!(sc.name_color, 0xff6699);

    // Missing optional fields fall back to the documented platform defaults.
    let plain = bili(
        json!({"cmd":"SUPER_CHAT_MESSAGE","data":{"id":"sc-2","price":1,"time":1,"message":"x"}}),
    );
    let DanmakuEvent::SuperChat(sc) = &plain[0] else {
        panic!("expected super chat")
    };
    assert_eq!(sc.background_bottom_color, 0x2a60b2);
    assert_eq!(sc.name_color, 0x666666);
    assert!(sc.face.is_empty() && sc.medal.is_none());
}

#[test]
fn douyu_chat_keeps_colour_level_and_medal_from_the_stt_fields() {
    let mut decoder = DouyuDecoder::default();
    let body = "type@=chatmsg/col@=1/level@=27/nn@=合成/bnn@=合成牌子/bl@=14/uid@=12345/txt@=合成弹幕/cid@=c1/";
    let events = decoder
        .push(&douyu_packet(body, true).unwrap())
        .unwrap()
        .events;
    let DanmakuEvent::Message(message) = &events[0] else {
        panic!("expected message")
    };
    assert_eq!(message.color, 0xff0000);
    assert_eq!(message.user_level, 27);
    assert_eq!(message.user_id, 12345);
    let medal = message.medal.as_ref().expect("medal");
    assert_eq!((medal.name.as_str(), medal.level), ("合成牌子", 14));
    decoder.finish().unwrap();
}

#[test]
fn douyu_super_chat_uses_price_tiers_avatar_and_levels() {
    let nested = encode_stt(&[
        ("type", "chatmsg"),
        ("nn", "合成"),
        ("txt", "合成高能弹幕"),
        ("level", "12"),
        ("bnn", "合成牌子"),
        ("bl", "8"),
        ("ic", "synthetic-avatar"),
    ]);
    let body = encode_stt(&[
        ("btype", "voiceDanmu"),
        ("chatmsg", &nested),
        ("cprice", "12000"),
        ("cet", "40"),
        ("type", "comm_chatmsg"),
        ("vrid", "77"),
    ]);
    let events = DouyuDecoder::default()
        .push(&douyu_packet(&body, true).unwrap())
        .unwrap()
        .events;
    let DanmakuEvent::SuperChat(sc) = &events[0] else {
        panic!("expected super chat")
    };
    assert_eq!(sc.price_cents, 12000);
    assert_eq!(sc.user_level, 12);
    assert_eq!(sc.medal.as_ref().map(|m| m.level), Some(8));
    assert!(sc.face.contains("apic.douyucdn.cn"));
    assert!(sc.face.contains("synthetic-avatar"));
    // 120 yuan selects the pink tier, not the base blue tier.
    assert_ne!(sc.background_color_start, 0xc1c1ff);
}

#[test]
fn douyin_chat_keeps_user_identity_and_level() {
    fn varint(tag: u8, value: u64) -> Vec<u8> {
        let mut out = vec![(tag << 3) | 0];
        let mut value = value;
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return out;
            }
            out.push(byte | 0x80);
        }
    }
    fn bytes(tag: u8, value: &[u8]) -> Vec<u8> {
        let mut out = vec![(tag << 3) | 2, value.len() as u8];
        out.extend_from_slice(value);
        out
    }
    let mut user = varint(1, 987654);
    user.extend(bytes(3, b"Synthetic Douyin"));
    user.extend(varint(6, 22));
    let mut chat = bytes(2, &user);
    chat.extend(bytes(3, b"Hello Douyin"));
    let mut message = bytes(1, b"WebcastChatMessage");
    message.extend(bytes(2, &chat));
    let response = bytes(1, &message);
    let frame = bytes(8, &response);
    let events = decode_frame(live_hub_rust::core::Platform::Douyin, &frame).unwrap();
    let DanmakuEvent::Message(message) = &events[0] else {
        panic!("expected message")
    };
    assert_eq!(message.user, "Synthetic Douyin");
    assert_eq!(message.user_id, 987654);
    assert_eq!(message.user_level, 22);
}

#[test]
fn remote_image_headers_and_emoticon_dictionaries_stay_bounded() {
    // A minimal 3x2 lossless WebP container is enough to exercise the parser.
    let mut webp = b"RIFF".to_vec();
    webp.extend_from_slice(&17_u32.to_le_bytes());
    webp.extend_from_slice(b"WEBPVP8L");
    webp.extend_from_slice(&5_u32.to_le_bytes());
    webp.push(0x2f);
    webp.extend_from_slice(&(2_u32 | (1_u32 << 14)).to_le_bytes());
    assert_eq!(
        image_dimensions(&webp),
        Some(live_hub_rust::danmaku::rich::ImageDimensions {
            width: 3,
            height: 2
        })
    );
    assert!(image_dimensions(b"junk").is_none());

    let payload = r#"{"code":0,"data":{"packages":[{"emoticons":[
        {"emoji":"[合成笑]","url":"https://i0.hdslb.com/bfs/live/synthetic.png"}]}]}}"#;
    let dictionary = parse_bili_emoticons(payload).unwrap();
    assert_eq!(dictionary.len(), 1);
    assert_eq!(
        split_text_segments("hi[合成笑]!", Some(&dictionary), None),
        vec![
            TextSegment::Text("hi".into()),
            TextSegment::Emoticon {
                code: "[合成笑]".into(),
                url: "https://i0.hdslb.com/bfs/live/synthetic.png".into()
            },
            TextSegment::Text("!".into())
        ]
    );
    let douyin = r#"{"emoji_list":[{"display_name":"[微笑]","emoji_url":{"url_list":["https://p3.douyinpic.com/a.png"]}}]}"#;
    assert_eq!(parse_douyin_emoticons(douyin).unwrap().len(), 1);
    assert!(parse_bili_emoticons("not json").is_err());
    assert!(EmoticonDictionary::new().is_empty());
}

/// Real-network acceptance for the B站 room emoticon API. The default suite runs
/// it offline: it returns immediately unless LIVE_HUB_REAL_ACCEPTANCE=1 is set.
/// Optional LIVE_HUB_REAL_BILI_ROOM selects the display room (default 7777).
#[test]
fn real_bili_room_emoticon_dictionary_fetches_and_parses() {
    if std::env::var("LIVE_HUB_REAL_ACCEPTANCE").as_deref() != Ok("1") {
        return;
    }
    let room = std::env::var("LIVE_HUB_REAL_BILI_ROOM")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(7777);
    // The API answers -101 for anonymous requests, so an isolated data directory
    // with the program's imported B站 login is used when provided. Only the
    // DPAPI vault is read; no plaintext cookie is passed through the environment.
    let result: anyhow::Result<live_hub_rust::danmaku::rich::EmoticonDictionary> =
        match std::env::var("LIVE_HUB_REAL_DATA_DIR") {
            Ok(directory) => {
                use live_hub_rust::auth::CookieVault;
                use live_hub_rust::platforms::CookieSource;
                use std::sync::Arc;
                let root = std::path::PathBuf::from(&directory);
                let credential_root = if root.join("credentials").is_dir() {
                    root.join("credentials")
                } else {
                    root
                };
                eprintln!(
                    "real emoticon acceptance: room={room}, credential root={}",
                    credential_root.display()
                );
                match CookieVault::open(credential_root) {
                    Ok(vault) => {
                        let has_bili = vault
                            .credential_stamp(live_hub_rust::core::Platform::Bili)
                            .ok()
                            .flatten()
                            .is_some();
                        eprintln!(
                            "real emoticon acceptance: B站 DPAPI credential present={has_bili}"
                        );
                        let source: Arc<dyn CookieSource> = Arc::new(vault);
                        live_hub_rust::danmaku::fetch_bili_emoticons_authenticated_result(
                            source, room,
                        )
                    }
                    Err(error) => Err(error.context("打开 DPAPI 凭据目录失败")),
                }
            }
            Err(_) => {
                eprintln!(
                    "real emoticon acceptance: LIVE_HUB_REAL_DATA_DIR not set, using anonymous"
                );
                live_hub_rust::danmaku::fetch_bili_emoticons_anonymous_result(room)
            }
        };
    let dictionary = match result {
        Ok(dictionary) => dictionary,
        Err(error) => {
            eprintln!("real emoticon acceptance FAILED: {error:#}");
            panic!("real GetEmoticons failed; see stderr for the full error chain");
        }
    };
    assert!(
        dictionary.len() >= 10,
        "real room {room}: only {} emoticon codes",
        dictionary.len()
    );
    eprintln!("real room {room}: {} emoticon codes", dictionary.len());
}

#[test]
fn sprite_manifest_and_text_segmentation_match_the_legacy_tables() {
    let manifest = EmojiManifest::parse(
        "CELL=2\nCOLUMNS=2\nPLAINCOUNT=2\nPLAIN=2764-2765\nVS=2764-2764\n\
         FLAGCOUNT=4\nFLAGSTART=4\nFLAGBASE=1F1E6\nSEQSTART=8\nSEQCOUNT=1\nSEQ=1F468-200D-1F469\n",
    )
    .unwrap();
    assert_eq!(manifest.cell, 2);
    assert_eq!(manifest.cell_for(0x2764, false), Some((0, 0)));
    assert_eq!(manifest.cell_for(0x2764, true), Some((0, 2)));
    assert!(manifest.flag_cell(0x1F1E6, 0x1F1E7).is_some());
    assert_eq!(
        split_text_segments("a❤️b", None, Some(&manifest)),
        vec![
            TextSegment::Text("a".into()),
            TextSegment::EmojiCell { x: 0, y: 2 },
            TextSegment::Text("b".into())
        ]
    );
    // Without a manifest the same text stays identical plain text.
    assert_eq!(
        split_text_segments("a❤️b", None, None),
        vec![TextSegment::Text("a❤️b".into())]
    );
}
