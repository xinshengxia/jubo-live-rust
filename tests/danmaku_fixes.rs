use live_hub_rust::{danmaku::*, danmaku_rules, chat_pipeline::ChatPipeline, core::DanmakuBehavior};
use std::{io::Write, time::Duration};
fn message(user: &str, text: &str, uid: u64) -> DanmakuMessage {
    DanmakuMessage { image_url:None,id:"server".into(),user:user.into(),text:text.into(),color:0xffffff,
        mode:DanmakuMode::Scroll,font_size:25,user_id:uid,user_level:0,guard_level:0,medal:None,
        image_width:0,image_height:0,image_large:false }
}
#[test]
fn production_admission_uses_user_rule_syntax_not_literal_user_set() {
    for rule in ["uid:42", "name:Alice", "re:^ali.*$", "Li"] {
        let mut pipe=ChatPipeline::new(DanmakuBehavior { blocked_users:vec![rule.into()], ..Default::default() });
        assert!(pipe.admit(message("Alice","normal",42),0.0).is_none(),"{rule}");
    }
    let mut pipe=ChatPipeline::new(DanmakuBehavior { blocked_users:vec!["re:^alice$".into()], ..Default::default() });
    assert!(pipe.admit(message("Bob","Alice",7),0.0).is_some());
}
#[test]
fn user_regex_and_keyword_regex_use_separate_targets() {
    let rules=danmaku_rules::compile_with_users(&[r"re:^\d+$".into()],&[r"re:^(alice|bob)\d{2}$".into()],&[],&[]);
    assert!(rules.errors.is_empty());
    assert!(rules.is_blocked(&message("ordinary","123",0)));
    assert!(!rules.is_blocked(&message("ordinary","ddd",0)));
    assert!(rules.is_blocked(&message("ALICE42","normal",0)));
    assert!(!rules.is_blocked(&message("ordinary","Alice42",0)));
}
#[test]
fn chat_pressure_preserves_paid_message_delete_order_without_growing_queue() {
    let (sender,receiver)=bounded_channel(4);
    for n in 0..4 {assert_eq!(emit(&sender,DanmakuEvent::Message(message("u",&n.to_string(),0))),Delivery::Sent);}
    let sc:SuperChat=serde_json::from_value(serde_json::json!({"id":"paid","user":"u","text":"test","price_cents":100,"duration_secs":10})).unwrap();
    assert_eq!(emit(&sender,DanmakuEvent::SuperChat(sc)),Delivery::Sent);
    assert_eq!(emit(&sender,DanmakuEvent::DeleteSuperChat("paid".into())),Delivery::Sent);
    let mut kinds=Vec::new();
    while let Ok(event)=receiver.try_recv(){kinds.push(match event{DanmakuEvent::SuperChat(_)=>"sc",DanmakuEvent::DeleteSuperChat(_)=>"delete",_=>"chat"});}
    assert_eq!(kinds,vec!["chat","chat","sc","delete"]);
    assert_eq!(receiver.dropped_count(),2);
    drop(sender);
    assert!(matches!(receiver.recv_timeout(Duration::from_millis(1)),Err(std::sync::mpsc::RecvTimeoutError::Disconnected)));
}
#[test]
fn bilibili_brotli_is_decoded_with_the_same_nested_packet_budget() {
    let chat=serde_json::to_vec(&serde_json::json!({"cmd":"DANMU_MSG","info":[[0,1,25,16777215,0,0],"synthetic",[1,"u"]]})).unwrap();
    let packet=bili_packet(5,0,&chat).unwrap();
    let mut compressed=Vec::new();
    { let mut writer=brotli::CompressorWriter::new(&mut compressed,4096,4,22);writer.write_all(&packet).unwrap(); }
    let frame=bili_packet(5,3,&compressed).unwrap();
    assert!(decode_bili(&frame).unwrap().iter().any(|e|matches!(e,DanmakuEvent::Message(m) if m.text=="synthetic")));
    let mut compressed=Vec::new();
    { let mut writer=brotli::CompressorWriter::new(&mut compressed,4096,4,22);writer.write_all(&vec![0;MAX_EXPANDED_BYTES+1]).unwrap(); }
    assert!(decode_bili(&bili_packet(5,3,&compressed).unwrap()).is_err());
}

#[test]
fn static_gif_and_bmp_emotes_decode_through_the_overlay_image_path() {
    // Cargo enables the image crate's gif/bmp decoders for the overlay image
    // loader; the loader keeps using `with_guessed_format` + bounded limits, so
    // these containers must decode to a static first frame instead of falling
    // back to plain text. Fixtures are synthetic 1x1 GIF and 2x1 BMP images.
    let gif: [u8; 43] = [
        0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00,
        0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
        0xff, 0xff, 0xff, 0x21, 0xf9, 0x04, 0x01, 0x00,
        0x00, 0x00, 0x00, 0x2c, 0x00, 0x00, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44,
        0x01, 0x00, 0x3b,
    ];
    let bmp: [u8; 62] = [
        0x42, 0x4d, 0x3e, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x36, 0x00, 0x00, 0x00, 0x28, 0x00,
        0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x00, 0x01, 0x00, 0x18, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0xff, 0x00, 0xff, 0x00, 0x00, 0x00,
    ];
    assert!(live_hub_rust::danmaku::rich::image_within_limits(&gif));
    assert!(live_hub_rust::danmaku::rich::image_within_limits(&bmp));
    let decoded = live_hub_rust::images::decode(&gif).expect("static GIF decodes");
    assert_eq!((decoded.width, decoded.height), (1, 1));
    let decoded = live_hub_rust::images::decode(&bmp).expect("BMP decodes");
    assert_eq!((decoded.width, decoded.height), (2, 1));
    assert_eq!(decoded.rgba.len(), 2 * 1 * 4);
}
