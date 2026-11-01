//! 虎牙 Tars 弹幕解码的离线合成测试。
//!
//! 约束：不联网、不读取真实用户数据、不打开播放器窗口；所有报文由本文件内
//! 独立的 Tars 小写入器构造（与生产写入器分开实现，避免“写入器有 bug 掩盖读取器问题”）。
//! 字节布局依据旧 C# 实现推导：src/Danmaku/danmaku-9-huya.cs 的
//! TarsWriter/CommandRegisterGroup/CommandHeartbeat/HandlePush/HandlePushV2/
//! ParseMessageNotice，以及 docs/decisions/003-虎牙-tars.md。

use live_hub_rust::danmaku::{DanmakuEvent, DanmakuMode, MAX_TEXT_CHARS, MAX_WIRE_BYTES};
use live_hub_rust::danmaku_huya::decode;

/// 测试用最小 Tars 写入器：独立实现，只覆盖构造合成报文需要的类型。
struct TarsWriter {
    buffer: Vec<u8>,
}

impl TarsWriter {
    fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    fn finish(self) -> Vec<u8> {
        self.buffer
    }

    fn head(&mut self, tag: u8, kind: u8) {
        if tag < 15 {
            self.buffer.push((tag << 4) | (kind & 0x0F));
        } else {
            self.buffer.push(0xF0 | (kind & 0x0F));
            self.buffer.push(tag);
        }
    }

    /// 类型窄化：0 只写 ZERO 头，落在 int8 范围写 INT8。
    fn i8(&mut self, tag: u8, value: i8) {
        if value == 0 {
            self.head(tag, 12);
        } else {
            self.head(tag, 0);
            self.buffer.push(value as u8);
        }
    }

    fn i32(&mut self, tag: u8, value: i32) {
        if (-128..=127).contains(&value) {
            self.i8(tag, value as i8);
        } else {
            self.head(tag, 2);
            self.buffer.extend_from_slice(&value.to_be_bytes());
        }
    }

    fn raw_str(&mut self, tag: u8, bytes: &[u8]) {
        // <=255 字节走 String1，否则 String4（与 Tars 规则一致）。
        if bytes.len() <= 255 {
            self.head(tag, 6);
            self.buffer.push(bytes.len() as u8);
        } else {
            self.head(tag, 7);
            self.buffer
                .extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        }
        self.buffer.extend_from_slice(bytes);
    }

    fn str(&mut self, tag: u8, value: &str) {
        self.raw_str(tag, value.as_bytes());
    }

    /// BYTES 是两个头字节：BYTES 头 + INT8 tag0 占位 + INT32 长度 + 数据。
    fn bytes(&mut self, tag: u8, data: &[u8]) {
        self.head(tag, 13);
        self.head(0, 0);
        self.i32(0, data.len() as i32);
        self.buffer.extend_from_slice(data);
    }

    fn struct_begin(&mut self, tag: u8) {
        self.head(tag, 10);
    }

    fn struct_end(&mut self) {
        self.head(0, 11);
    }

    fn list_head(&mut self, tag: u8, count: i32) {
        self.head(tag, 9);
        self.i32(0, count);
    }
}

/// 外层命令包 { iCmdType tag0, vData tag1 }，对应旧实现 CommandRegisterGroup 的包装。
fn command(command: i32, data: &[u8]) -> Vec<u8> {
    let mut writer = TarsWriter::new();
    writer.i32(0, command);
    writer.bytes(1, data);
    writer.finish()
}

/// uri 1400 MessageNotice：tag0 tUserInfo、tag3 sContent、可选 tag5 ContentFormat。
fn message_notice(user: &str, text: &str, color: Option<i32>) -> Vec<u8> {
    let mut writer = TarsWriter::new();
    writer.struct_begin(0);
    writer.i32(0, 123); // tUserInfo.tag0（旧实现的 iUid）
    writer.str(2, user); // sNickName
    writer.i8(5, 3); // iNobleLevel
    writer.struct_end();
    writer.str(3, text);
    if let Some(color) = color {
        writer.struct_begin(5);
        writer.i32(0, color); // iFontColor
        writer.struct_end();
    }
    writer.finish()
}

fn message_notice_with_raw_user(text: &str, raw_user: &[u8]) -> Vec<u8> {
    let mut writer = TarsWriter::new();
    writer.struct_begin(0);
    writer.raw_str(2, raw_user);
    writer.i8(5, 3);
    writer.struct_end();
    writer.str(3, text);
    writer.finish()
}

/// cmd 7 WSPushMessage { ePushType tag0, iUri tag1, sMsg tag2, iProtocolType tag3 }。
fn push_v1(uri: i32, message: &[u8]) -> Vec<u8> {
    let mut writer = TarsWriter::new();
    writer.i32(0, 0);
    writer.i32(1, uri);
    writer.bytes(2, message);
    writer.i32(3, 0);
    writer.finish()
}

/// cmd 22 V2 推送：tag1 = list<{iUri tag0, sMsg tag1, lMsgId tag2}>。
fn push_v2(items: &[(i32, Vec<u8>)]) -> Vec<u8> {
    let mut writer = TarsWriter::new();
    writer.list_head(1, items.len() as i32);
    for (uri, message) in items {
        writer.struct_begin(0);
        writer.i32(0, *uri);
        writer.bytes(1, message);
        writer.i32(2, 0x1234); // lMsgId：解码端应忽略
        writer.struct_end();
    }
    writer.finish()
}

/// cmd 17 RegisterGroupRsp：tag1 = list<string>。
fn register_group_rsp(groups: &[&str]) -> Vec<u8> {
    let mut writer = TarsWriter::new();
    writer.list_head(1, groups.len() as i32);
    for group in groups {
        writer.str(0, group);
    }
    writer.finish()
}

fn message(event: &DanmakuEvent) -> &live_hub_rust::danmaku::DanmakuMessage {
    match event {
        DanmakuEvent::Message(message) => message,
        other => panic!("期望聊天事件，实际是 {other:?}"),
    }
}

#[test]
fn chat_message_decodes_from_v1_push() {
    let notice = message_notice("合成用户", "离线合成的虎牙弹幕", Some(0xFF0000));
    let frame = command(7, &push_v1(1400, &notice));
    let events = decode(&frame).unwrap();
    assert_eq!(events.len(), 1, "一帧只应解出一条聊天弹幕");
    let item = message(&events[0]);
    assert_eq!(item.user, "合成用户");
    assert_eq!(item.text, "离线合成的虎牙弹幕");
    assert_eq!(item.color, 0xFF0000, "颜色应来自 ContentFormat.tag0");
    assert_eq!(item.mode, DanmakuMode::Scroll, "旧实现固定当滚动弹幕");
    assert_eq!(item.font_size, 25);
    assert!(item.id.is_empty(), "旧实现不产出消息 id");
}

#[test]
fn chat_message_decodes_from_v2_push_and_ignores_other_uris() {
    let notice = message_notice("合成用户", "V2 通道弹幕", None);
    // 6501=礼物、6111=进场：旧实现只解析不渲染，且虎牙没有 SC 等价物。
    let frame = command(
        22,
        &push_v2(&[
            (6111, b"synthetic-enter".to_vec()),
            (1400, notice),
            (6501, b"synthetic-gift".to_vec()),
        ]),
    );
    let events = decode(&frame).unwrap();
    assert_eq!(events.len(), 1, "只有 uri 1400 产生事件");
    let item = message(&events[0]);
    assert_eq!(item.text, "V2 通道弹幕");
    assert_eq!(item.color, 0x00FF_FFFF, "没有 ContentFormat 时用白色");
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, DanmakuEvent::SuperChat(_))),
        "虎牙协议没有付费留言，不能伪造 SuperChat"
    );
}

#[test]
fn message_without_body_is_dropped_instead_of_failing() {
    let notice = message_notice("合成用户", "", None);
    let events = decode(&command(7, &push_v1(1400, &notice))).unwrap();
    assert!(events.is_empty(), "空正文不是可渲染弹幕，但报文本身合法");
}

#[test]
fn message_without_user_info_still_yields_text() {
    // tUserInfo（tag0 结构体）缺失时，正文仍应能被解出，用户名留空。
    let mut writer = TarsWriter::new();
    writer.str(3, "只有正文的合成弹幕");
    let notice = writer.finish();
    let events = decode(&command(7, &push_v1(1400, &notice))).unwrap();
    assert_eq!(events.len(), 1);
    let item = message(&events[0]);
    assert_eq!(item.text, "只有正文的合成弹幕");
    assert!(item.user.is_empty());
}

#[test]
fn long_text_and_user_are_truncated_to_the_event_budget() {
    let long_text = "弹".repeat(600);
    let long_user = "虎".repeat(600);
    let notice = message_notice(&long_user, &long_text, None);
    let events = decode(&command(7, &push_v1(1400, &notice))).unwrap();
    let item = message(&events[0]);
    assert_eq!(item.text.chars().count(), MAX_TEXT_CHARS);
    assert_eq!(item.user.chars().count(), MAX_TEXT_CHARS);
}

#[test]
fn invalid_utf8_is_rejected_in_body_and_user_name() {
    // 正文里的非法 UTF-8：必须返回 Err，不能替换成 U+FFFD 冒充成功。
    let mut writer = TarsWriter::new();
    writer.struct_begin(0);
    writer.str(2, "合成用户");
    writer.struct_end();
    writer.raw_str(3, &[0xFF, 0xFE, 0x41]);
    let notice = writer.finish();
    assert!(decode(&command(7, &push_v1(1400, &notice))).is_err());

    // 昵称里的非法 UTF-8：同样拒绝。
    let notice = message_notice_with_raw_user("正文", &[0xFF, 0xFE]);
    assert!(decode(&command(7, &push_v1(1400, &notice))).is_err());
}

#[test]
fn truncated_frames_are_rejected_at_every_prefix() {
    let notice = message_notice("合成用户", "合成弹幕正文", Some(0x00FF00));
    let frame = command(7, &push_v1(1400, &notice));
    assert!(!frame.is_empty());
    for cut in 1..frame.len() {
        assert!(
            decode(&frame[..cut]).is_err(),
            "截断到 {cut} 字节时仍返回 Ok，应拒绝"
        );
    }
    assert!(decode(&frame).is_ok(), "完整帧必须能解");
}

#[test]
fn oversized_declared_lengths_are_rejected_without_allocation() {
    // 超预算单帧直接拒绝；声明 0xFFFFFFF0 长度的 BYTES 字段也必须在读取前失败。
    assert!(decode(&vec![0_u8; MAX_WIRE_BYTES + 1]).is_err());
    let frame = vec![0x00, 0x07, 0x1D, 0x00, 0x02, 0xFF, 0xFF, 0xFF, 0xF0];
    assert!(decode(&frame).is_err());

    // BYTES 字段缺少 tag0 占位头（只读一个头会整体错位）。
    let frame = command(7, &[0x1D, 0x16, 0x00]);
    assert!(decode(&frame).is_err());
}

#[test]
fn deeply_nested_structs_are_rejected() {
    // tUserInfo.tag0 里塞 70+ 层嵌套结构体：超过深度上限必须返回 Err 而不是爆栈。
    let mut writer = TarsWriter::new();
    writer.struct_begin(0); // tUserInfo
    for _ in 0..70 {
        writer.struct_begin(0);
    }
    for _ in 0..70 {
        writer.struct_end();
    }
    writer.str(2, "合成用户");
    writer.struct_end(); // tUserInfo
    writer.str(3, "正文");
    let notice = writer.finish();
    let error = decode(&command(7, &push_v1(1400, &notice))).unwrap_err();
    assert!(
        format!("{error:#}").contains("嵌套层数超过上限"),
        "实际错误：{error:#}"
    );
}

#[test]
fn register_group_ack_becomes_status() {
    let events = decode(&command(17, &register_group_rsp(&["live:123", "chat:123"]))).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        DanmakuEvent::Status(text) => {
            assert!(text.contains("订阅成功"), "状态文案：{text}");
            assert!(
                text.contains("live:123") && text.contains("chat:123"),
                "状态文案：{text}"
            );
        }
        other => panic!("期望订阅状态，实际是 {other:?}"),
    }

    // 空组回显 = 组名/频道 id 错了（旧实现实测：用 yyid 时服务器静默回空组）。
    let events = decode(&command(17, &register_group_rsp(&[]))).unwrap();
    match &events[0] {
        DanmakuEvent::Status(text) => assert!(text.contains("空组"), "状态文案：{text}"),
        other => panic!("期望空组状态，实际是 {other:?}"),
    }
}

#[test]
fn heartbeat_ack_and_empty_noise_produce_no_events() {
    // cmd 4：心跳确认（连接循环内部消费，公开 decode 不产出事件）。
    assert!(decode(&command(4, &[])).unwrap().is_empty());
    // cmd 0：空包噪声。
    assert!(decode(&[0x0C]).unwrap().is_empty());
}

#[test]
fn event_budget_is_enforced_per_frame() {
    let notice = message_notice("合成用户", "批量弹幕", None);
    let items: Vec<(i32, Vec<u8>)> = (0..(1024 + 1)).map(|_| (1400, notice.clone())).collect();
    let error = decode(&command(22, &push_v2(&items))).unwrap_err();
    assert!(
        format!("{error:#}").contains("超过上限"),
        "实际错误：{error:#}"
    );
}
