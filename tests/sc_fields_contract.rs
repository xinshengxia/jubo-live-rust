//! Offline contract tests for SuperChat protocol fields (face_frame / message_trans).

use live_hub_rust::danmaku::{bili_packet, decode_bili, DanmakuEvent, SuperChat};

fn decode_one(json: &str) -> SuperChat {
    let frame = bili_packet(5, 0, json.as_bytes()).unwrap();
    let events = decode_bili(&frame).unwrap();
    events
        .into_iter()
        .find_map(|event| match event {
            DanmakuEvent::SuperChat(card) => Some(card),
            _ => None,
        })
        .expect("super chat event")
}

const FULL_JSON: &str = r##"{
  "cmd": "SUPER_CHAT_MESSAGE",
  "data": {
    "id": "sc-1",
    "price": 30,
    "time": 60,
    "message": "测试留言",
    "message_trans": "translated message",
    "user_info": {
      "uname": "观众甲",
      "face": "https://i0.hdslb.com/bfs/face/a.jpg",
      "face_frame": "https://i0.hdslb.com/bfs/garb/item/f.png",
      "user_level": 12,
      "guard_level": 3,
      "name_color": "#8C1A1A"
    },
    "medal_info": {"medal_name": "德云色", "medal_level": 21, "medal_color": 6067854},
    "background_color": "#FFE9E6",
    "background_bottom_color": "#A81E1E",
    "background_color_start": "#D4574F",
    "message_font_color": "#FFEDEA"
  }
}"##;

#[test]
fn bilibili_super_chat_parses_face_frame_and_translation() {
    let card = decode_one(FULL_JSON);
    assert_eq!(card.price_cents, 3000, "protocol price is yuan, stored in cents");
    assert_eq!(card.duration_secs, 60);
    assert_eq!(card.message_trans, "translated message");
    assert!(card.face.contains("i0.hdslb.com"), "face={}", card.face);
    assert!(card.face_frame.contains("i0.hdslb.com"), "face_frame={}", card.face_frame);
    assert_eq!(card.user, "观众甲");
    assert_eq!(card.user_level, 12);
    assert_eq!(card.guard_level, 3);
    assert_eq!(card.background_color, 0xFFE9E6);
    assert_eq!(card.background_bottom_color, 0xA81E1E);
    assert_eq!(card.message_font_color, 0xFFEDEA);
    let medal = card.medal.expect("medal");
    assert_eq!(medal.name, "德云色");
    assert_eq!(medal.level, 21);
}

#[test]
fn missing_optional_fields_degrade_to_empty_and_defaults() {
    let json = r#"{"cmd":"SUPER_CHAT_MESSAGE","data":{"id":"sc-2","price":2,"time":5,"message":"简短","user_info":{"uname":"观众乙","face":""}}}"#;
    let card = decode_one(json);
    assert_eq!(card.price_cents, 200);
    assert_eq!(card.message_trans, "");
    assert_eq!(card.face_frame, "");
    assert!(card.medal.is_none());
    assert_eq!(card.background_color, 0xEDF5FF);
    assert_eq!(card.background_bottom_color, 0x2A60B2);
    assert_eq!(card.message_font_color, 0xA3F6FF);
    assert_eq!(card.name_color, 0x666666);
}

#[test]
fn untrusted_avatar_urls_are_dropped_but_the_card_survives() {
    let json = r#"{"cmd":"SUPER_CHAT_MESSAGE","data":{"id":"sc-3","price":30,"time":60,"message":"x","user_info":{"uname":"观众丙","face":"http://evil.example.com/a.jpg","face_frame":"http://evil.example.com/f.png"}}}"#;
    let card = decode_one(json);
    assert_eq!(card.face, "");
    assert_eq!(card.face_frame, "");
}
