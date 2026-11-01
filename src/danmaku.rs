//! Bounded danmaku events, protocol decoders and blocking receiver workers.
//!
//! Protocol/model reference: xfgryujk/blivedm (MIT), see
//! `licenses/blivedm-LICENSE.txt` and `docs/danmaku-protocol-sources.md`.
//! The new Rust implementation does not import the legacy application's data.

use crate::auth::CookieVault;
use crate::core::{Platform, Target};
use crate::platforms::CookieSource;
#[path = "danmaku_rich.rs"]
pub mod rich;
#[path = "danmaku_session.rs"]
pub mod session;
use anyhow::{Context, Result, bail, ensure};
use flate2::read::{GzDecoder, ZlibDecoder};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{ErrorKind, Read, Write},
    net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};
use tungstenite::{
    Message, WebSocket, client::IntoClientRequest, protocol::WebSocketConfig,
    stream::MaybeTlsStream,
};

pub const MAX_WIRE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_EXPANDED_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_EVENTS_PER_FRAME: usize = 1024;
pub const MAX_QUEUE_CAPACITY: usize = 1024;
pub const MAX_TEXT_CHARS: usize = 512;
/// Fixed (top/bottom) danmaku leave the screen after this many seconds;
/// the legacy renderer used FixedUntil = createdAt + 4s.
pub const FIXED_LIFETIME_SECS: f64 = 4.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DanmakuMode {
    Scroll,
    Top,
    Bottom,
    /// Right-to-left mirror of Scroll (B站 mode 6, the "逆向弹幕" option).
    Reverse,
}

/// Fan medal attached to one B站/斗鱼 message. The name and level are the
/// platform's own values; color is the platform-provided 0xRRGGBB badge color
/// (0 when the platform did not send one).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Medal {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub level: u16,
    #[serde(default)]
    pub color: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DanmakuMessage {
    #[serde(default)]
    pub image_url: Option<String>,
    pub id: String,
    pub user: String,
    pub text: String,
    /// 0xRRGGBB, independent of any rendering framework.
    pub color: u32,
    pub mode: DanmakuMode,
    pub font_size: u16,
    /// Platform user id when the protocol supplies one (0 otherwise).
    #[serde(default)]
    pub user_id: u64,
    /// Platform user level when supplied (0 otherwise).
    #[serde(default)]
    pub user_level: u16,
    /// 0 = none, 1 = 总督, 2 = 提督, 3 = 舰长, as the platform encodes it.
    #[serde(default)]
    pub guard_level: u8,
    #[serde(default)]
    pub medal: Option<Medal>,
    /// Intrinsic size of a server-provided image emoticon (0 when unknown).
    #[serde(default)]
    pub image_width: u16,
    #[serde(default)]
    pub image_height: u16,
    /// A "bulge" emoticon may occupy several text rows.
    #[serde(default)]
    pub image_large: bool,
}

fn sc_default_top_color() -> u32 {
    0xedf5ff
}
fn sc_default_bottom_color() -> u32 {
    0x2a60b2
}
fn sc_default_start_color() -> u32 {
    0x3171d2
}
fn sc_default_font_color() -> u32 {
    0xa3f6ff
}
fn sc_default_name_color() -> u32 {
    0x666666
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuperChat {
    pub id: String,
    pub user: String,
    pub text: String,
    pub price_cents: u64,
    pub duration_secs: u32,
    /// Platform-provided translation of the message, when present.
    #[serde(default)]
    pub message_trans: String,
    /// Avatar URL; never fetched unless the overlay loads it through the
    /// bounded image pipeline.
    #[serde(default)]
    pub face: String,
    /// Optional avatar frame image URL (B站 face_frame); empty when the room
    /// does not use one.
    #[serde(default)]
    pub face_frame: String,
    #[serde(default)]
    pub user_level: u16,
    #[serde(default)]
    pub guard_level: u8,
    #[serde(default)]
    pub medal: Option<Medal>,
    /// Upper header band colour (legacy background_color); the content band
    /// uses background_bottom_color.
    #[serde(default = "sc_default_top_color")]
    pub background_color: u32,
    #[serde(default = "sc_default_bottom_color")]
    pub background_bottom_color: u32,
    #[serde(default = "sc_default_start_color")]
    pub background_color_start: u32,
    #[serde(default = "sc_default_font_color")]
    pub message_font_color: u32,
    #[serde(default = "sc_default_name_color")]
    pub name_color: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DanmakuEvent {
    Message(DanmakuMessage),
    SuperChat(SuperChat),
    DeleteSuperChat(String),
    Status(String),
    Live(LiveEvent),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LiveEventKind {
    Gift,
    Guard,
    Enter,
    Follow,
    Notice,
    /// Passive room statistics (watched count, likes, popularity rank).
    Status,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveEvent {
    pub id: String,
    pub kind: LiveEventKind,
    pub user: String,
    pub text: String,
    pub count: u32,
}

#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

pub fn bounded_channel(capacity: usize) -> (SyncSender<DanmakuEvent>, Receiver<DanmakuEvent>) {
    mpsc::sync_channel(capacity.clamp(1, MAX_QUEUE_CAPACITY))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Sent,
    Dropped,
    Closed,
}

/// No network worker waits for a slow UI. Queue pressure drops the newest event.
pub fn emit(sender: &SyncSender<DanmakuEvent>, event: DanmakuEvent) -> Delivery {
    match sender.try_send(event) {
        Ok(()) => Delivery::Sent,
        Err(TrySendError::Full(_)) => Delivery::Dropped,
        Err(TrySendError::Disconnected(_)) => Delivery::Closed,
    }
}

fn limit(text: &str) -> String {
    text.chars().take(MAX_TEXT_CHARS).collect()
}
fn text(value: &Value) -> String {
    match value {
        Value::String(v) => limit(v),
        Value::Number(v) => v.to_string(),
        _ => String::new(),
    }
}
fn number(value: &Value) -> u64 {
    value
        .as_u64()
        .or_else(|| value.as_str()?.parse().ok())
        .unwrap_or(0)
}

/// Parses a platform colour string such as "#FF00AA" or "#80FF00AA".
fn hex_color(value: &Value) -> Option<u32> {
    let text = value.as_str()?.trim();
    let digits = text.trim_start_matches('#');
    let digits = if digits.len() == 8 {
        &digits[2..]
    } else {
        digits
    };
    (digits.len() == 6)
        .then(|| u32::from_str_radix(digits, 16).ok())
        .flatten()
        .map(|color| color & 0xffffff)
}

/// Guard level 0/1/2/3 from an integer field; anything outside stays 0.
fn guard_level_from(value: &Value) -> u8 {
    number(value).min(3) as u8
}

/// Builds one live-event user name, preferring the first non-empty source.
fn first_text(primary: &Value, fallback: &Value) -> String {
    let primary = text(primary);
    if primary.is_empty() {
        text(fallback)
    } else {
        primary
    }
}

fn decode_bili_json(bytes: &[u8], output: &mut Vec<DanmakuEvent>) -> Result<()> {
    let value: Value = serde_json::from_slice(bytes)?;
    let cmd = value["cmd"]
        .as_str()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    match cmd {
        "DANMU_MSG" => {
            let info = &value["info"];
            let head = &info[0];
            let mut body = text(&info[1]);
            // dm_type == 1 carries a server-provided image emoticon at head[13]
            // ({url,width,height,bulge_display}). Older local fixtures put the
            // image object directly at head[12]; accept that only when it really
            // carries a URL, so a real numeric head[12] is never misread.
            let emoticon = if number(&head[12]) == 1 {
                &head[13]
            } else if head[12]["url"].is_string() {
                &head[12]
            } else {
                &Value::Null
            };
            let image_url = emoticon["url"]
                .as_str()
                .and_then(|url| crate::images::approved_url(url).ok())
                .map(|url| url.to_string());
            let image_width = number(&emoticon["width"]).min(4096) as u16;
            let image_height = number(&emoticon["height"]).min(4096) as u16;
            let image_large = match &emoticon["bulge_display"] {
                Value::Bool(value) => *value,
                Value::Number(value) => value.as_u64() == Some(1),
                Value::String(value) => value == "1" || value.eq_ignore_ascii_case("true"),
                _ => false,
            };
            let mut user = text(&info[2][1]);
            let guard_from_new_layout = number(&head[15]["user"]["guard"]["level"]);
            if user.is_empty() {
                user = text(&head[15]["user"]["base"]["name"]);
            }
            // The new head[15] layout wins when present; info[7] is the legacy
            // fallback, exactly like the C# guard-level resolution order.
            let guard_level = if guard_from_new_layout > 0 {
                guard_from_new_layout.min(3) as u8
            } else {
                number(&info[7]).min(3) as u8
            };
            let user_level = number(&info[4][0]).min(600) as u16;
            let medal = info[3]
                .as_array()
                .filter(|medal| medal.len() >= 5)
                .map(|medal| Medal {
                    name: text(&medal[1]),
                    level: number(&medal[0]).min(999) as u16,
                    color: number(&medal[4]) as u32 & 0xffffff,
                })
                .filter(|medal| !medal.name.is_empty());
            if body.is_empty() && image_url.is_none() {
                return Ok(());
            }
            if body.is_empty() {
                // Keep a text fallback for the "images off" mode and for width
                // measurement; the image itself is drawn over this placeholder.
                body = "[表情]".into();
            }
            let mode = match number(&head[1]) {
                4 => DanmakuMode::Bottom,
                5 => DanmakuMode::Top,
                6 => DanmakuMode::Reverse,
                _ => DanmakuMode::Scroll,
            };
            output.push(DanmakuEvent::Message(DanmakuMessage {
                image_url,
                id: text(&head[5]),
                user,
                text: body,
                color: head[3].as_u64().unwrap_or(0xffffff) as u32 & 0xffffff,
                mode,
                font_size: number(&head[2]).clamp(8, 96) as u16,
                user_id: number(&info[2][0]),
                user_level,
                guard_level,
                medal,
                image_width,
                image_height,
                image_large,
            }));
        }
        "SUPER_CHAT_MESSAGE" => {
            let data = &value["data"];
            let user_info = &data["user_info"];
            let medal = if data["medal_info"].is_object() {
                let medal = &data["medal_info"];
                Some(Medal {
                    name: text(&medal["medal_name"]),
                    level: number(&medal["medal_level"]).min(999) as u16,
                    color: number(&medal["medal_color"]).min(0xffff_ffff) as u32 & 0xffffff,
                })
                .filter(|medal| !medal.name.is_empty())
            } else {
                None
            };
            output.push(DanmakuEvent::SuperChat(SuperChat {
                id: text(&data["id"]),
                user: text(&user_info["uname"]),
                text: text(&data["message"]),
                price_cents: number(&data["price"]).saturating_mul(100),
                duration_secs: number(&data["time"]).clamp(1, 3600) as u32,
                message_trans: text(&data["message_trans"]),
                face: user_info["face"]
                    .as_str()
                    .and_then(|url| crate::images::approved_url(url).ok())
                    .map(|url| url.to_string())
                    .unwrap_or_default(),
                face_frame: user_info["face_frame"]
                    .as_str()
                    .and_then(|url| crate::images::approved_url(url).ok())
                    .map(|url| url.to_string())
                    .unwrap_or_default(),
                user_level: number(&user_info["user_level"]).min(600) as u16,
                guard_level: number(&user_info["guard_level"]).min(3) as u8,
                medal,
                background_color: hex_color(&data["background_color"])
                    .unwrap_or_else(sc_default_top_color),
                background_bottom_color: hex_color(&data["background_bottom_color"])
                    .unwrap_or_else(sc_default_bottom_color),
                background_color_start: hex_color(&data["background_color_start"])
                    .unwrap_or_else(sc_default_start_color),
                message_font_color: hex_color(&data["message_font_color"])
                    .unwrap_or_else(sc_default_font_color),
                name_color: hex_color(&user_info["name_color"])
                    .unwrap_or_else(sc_default_name_color),
            }));
        }
        "SUPER_CHAT_MESSAGE_DELETE" => {
            if let Some(ids) = value["data"]["ids"].as_array() {
                for id in ids
                    .iter()
                    .take(MAX_EVENTS_PER_FRAME.saturating_sub(output.len()))
                {
                    output.push(DanmakuEvent::DeleteSuperChat(text(id)));
                }
            }
        }
        "SEND_GIFT" | "COMBO_SEND" | "GUARD_BUY" | "USER_TOAST_MSG" | "USER_TOAST_MSG_V2"
        | "INTERACT_WORD" | "WELCOME" | "WELCOME_GUARD" | "ENTRY_EFFECT" => {
            if cmd == "INTERACT_WORD" && !matches!(number(&value["data"]["msg_type"]), 1 | 2) {
                return Ok(());
            }
            let data = &value["data"];
            // Guard purchases from the auto-renew channel are duplicate noise.
            if matches!(cmd, "GUARD_BUY" | "USER_TOAST_MSG" | "USER_TOAST_MSG_V2")
                && number(&data["source"]) == 2
            {
                return Ok(());
            }
            let mut user = first_text(&data["uname"], &data["username"]);
            if user.is_empty() {
                user = text(&data["sender_uinfo"]["base"]["name"]);
            }
            let kind = match cmd {
                "GUARD_BUY" | "USER_TOAST_MSG" | "USER_TOAST_MSG_V2" => LiveEventKind::Guard,
                "SEND_GIFT" | "COMBO_SEND" => LiveEventKind::Gift,
                "INTERACT_WORD" if number(&data["msg_type"]) == 2 => LiveEventKind::Follow,
                _ => LiveEventKind::Enter,
            };
            let guard = guard_level_from(&data["guard_level"]);
            let name = if kind == LiveEventKind::Gift {
                let name = first_text(&data["giftName"], &data["gift_name"]);
                if name.is_empty() {
                    "礼物".to_owned()
                } else {
                    name
                }
            } else {
                String::new()
            };
            let event_text = match kind {
                LiveEventKind::Enter => "进入直播间".to_owned(),
                LiveEventKind::Follow => "关注了主播".to_owned(),
                LiveEventKind::Guard => match guard {
                    1 => "开通总督".to_owned(),
                    2 => "开通提督".to_owned(),
                    _ => "开通舰长".to_owned(),
                },
                _ => name,
            };
            let count = if cmd == "COMBO_SEND" {
                number(&data["total_num"]).max(number(&data["num"]))
            } else {
                number(&data["num"])
            };
            output.push(DanmakuEvent::Live(LiveEvent {
                id: text(&data["id"]),
                kind,
                user,
                text: rich::sanitize_event_text(&event_text, MAX_TEXT_CHARS),
                count: count.max(1).min(999999) as u32,
            }));
        }
        "WATCHED_CHANGE" | "ONLINE_RANK_COUNT" | "LIKE_INFO_V3_UPDATE" => {
            let data = &value["data"];
            let topic = match cmd {
                "WATCHED_CHANGE" => "watched",
                "ONLINE_RANK_COUNT" => "rank-count",
                _ => "likes",
            };
            let value_text = match cmd {
                "WATCHED_CHANGE" => first_text(&data["text_large"], &data["num"]),
                "ONLINE_RANK_COUNT" => text(&data["count"]),
                _ => first_text(&data["click_count"], &data["total_like"]),
            };
            if value_text.is_empty() {
                return Ok(());
            }
            let label = match topic {
                "watched" => "看过",
                "rank-count" => "高能榜",
                _ => "点赞",
            };
            output.push(DanmakuEvent::Live(LiveEvent {
                id: String::new(),
                kind: LiveEventKind::Status,
                user: String::new(),
                text: rich::sanitize_event_text(&format!("{label} {value_text}"), MAX_TEXT_CHARS),
                count: 1,
            }));
        }
        "NOTICE_MSG" => {
            let data = if value["data"].is_object() {
                &value["data"]
            } else {
                &value
            };
            let notice = first_text(&data["msg_common"], &data["msg_self"]);
            if !notice.is_empty() {
                output.push(DanmakuEvent::Live(LiveEvent {
                    id: text(&data["id"]),
                    kind: LiveEventKind::Notice,
                    user: String::new(),
                    text: rich::sanitize_event_text(&notice, MAX_TEXT_CHARS),
                    count: 1,
                }));
            }
        }
        "POPULARITY_RED_POCKET_START" | "POPULARITY_RED_POCKET_NEW" => {
            output.push(DanmakuEvent::Live(LiveEvent {
                id: text(&value["data"]["lot_id"]),
                kind: LiveEventKind::Notice,
                user: String::new(),
                text: "直播间红包活动开始".into(),
                count: 1,
            }));
        }
        "SEND_GIFT_V2" | "INTERACT_WORD_V2" => {
            if let Some(encoded) = value["data"]["pb"]
                .as_str()
                .filter(|s| s.len() <= 256 * 1024)
            {
                use base64::Engine;
                if let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(encoded) {
                    if let Ok(fields) = proto_fields(&raw) {
                        let user = proto_bytes(&fields, 2)
                            .and_then(|b| std::str::from_utf8(b).ok())
                            .map(limit)
                            .unwrap_or_default();
                        if cmd == "INTERACT_WORD_V2" {
                            let kind = fields.iter().find(|f| f.tag == 5).and_then(|f| f.number);
                            if matches!(kind, Some(1 | 2)) {
                                output.push(DanmakuEvent::Live(LiveEvent {
                                    id: String::new(),
                                    kind: if kind == Some(1) {
                                        LiveEventKind::Enter
                                    } else {
                                        LiveEventKind::Follow
                                    },
                                    user,
                                    text: if kind == Some(1) {
                                        "进入直播间"
                                    } else {
                                        "关注了主播"
                                    }
                                    .into(),
                                    count: 1,
                                }));
                            }
                        } else {
                            for gift in fields
                                .iter()
                                .filter(|f| f.tag == 10)
                                .filter_map(|f| f.bytes)
                                .take(64)
                            {
                                if let Ok(g) = proto_fields(gift) {
                                    let name = proto_bytes(&g, 2)
                                        .and_then(|b| std::str::from_utf8(b).ok())
                                        .map(limit)
                                        .unwrap_or_default();
                                    if !name.is_empty() {
                                        output.push(DanmakuEvent::Live(LiveEvent {
                                            id: proto_bytes(&g, 9)
                                                .and_then(|b| std::str::from_utf8(b).ok())
                                                .map(limit)
                                                .unwrap_or_default(),
                                            kind: LiveEventKind::Gift,
                                            user: user.clone(),
                                            text: name,
                                            count: g
                                                .iter()
                                                .find(|f| f.tag == 3)
                                                .and_then(|f| f.number)
                                                .unwrap_or(1)
                                                .clamp(1, 999999)
                                                as u32,
                                        }));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }
    ensure!(
        output.len() <= MAX_EVENTS_PER_FRAME,
        "too many danmaku events in one frame"
    );
    Ok(())
}

fn expand<R: Read>(input: R, budget: &mut usize) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    input.take((*budget + 1) as u64).read_to_end(&mut output)?;
    ensure!(output.len() <= *budget, "expanded danmaku exceeds budget");
    *budget -= output.len();
    Ok(output)
}

pub fn bili_packet(operation: u32, version: u16, body: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        body.len() <= MAX_WIRE_BYTES - 16,
        "Bilibili packet too large"
    );
    let mut packet = Vec::with_capacity(16 + body.len());
    packet.extend_from_slice(&((body.len() + 16) as u32).to_be_bytes());
    packet.extend_from_slice(&16_u16.to_be_bytes());
    packet.extend_from_slice(&version.to_be_bytes());
    packet.extend_from_slice(&operation.to_be_bytes());
    packet.extend_from_slice(&1_u32.to_be_bytes());
    packet.extend_from_slice(body);
    Ok(packet)
}

/// Includes concatenated packets and bounded nested zlib packets (protover=2).
/// Brotli/protover=3 is explicitly rejected: the client requests protover=2.
pub fn decode_bili(bytes: &[u8]) -> Result<Vec<DanmakuEvent>> {
    ensure!(
        bytes.len() <= MAX_WIRE_BYTES,
        "Bilibili frame exceeds wire budget"
    );
    let mut output = Vec::new();
    let mut budget = MAX_EXPANDED_BYTES;
    bili_packets(bytes, 0, &mut budget, &mut output)?;
    Ok(output)
}

fn bili_packets(
    mut bytes: &[u8],
    depth: usize,
    budget: &mut usize,
    output: &mut Vec<DanmakuEvent>,
) -> Result<()> {
    ensure!(depth <= 4, "danmaku compression nesting exceeds limit");
    while !bytes.is_empty() {
        ensure!(bytes.len() >= 16, "truncated Bilibili packet header");
        let length = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
        let header = u16::from_be_bytes(bytes[4..6].try_into().unwrap()) as usize;
        let version = u16::from_be_bytes(bytes[6..8].try_into().unwrap());
        let operation = u32::from_be_bytes(bytes[8..12].try_into().unwrap());
        ensure!(
            length >= header && header >= 16 && length <= bytes.len(),
            "invalid Bilibili packet length"
        );
        let payload = &bytes[header..length];
        match (operation, version) {
            (5, 2) => {
                let decoded = expand(ZlibDecoder::new(payload), budget)?;
                bili_packets(&decoded, depth + 1, budget, output)?;
            }
            (5, 0 | 1) => decode_bili_json(payload, output)?,
            (5, _) => bail!("unsupported Bilibili compression version"),
            (8, _) => {
                let value: Value = serde_json::from_slice(payload)?;
                ensure!(
                    value["code"].as_i64() == Some(0),
                    "Bilibili rejected danmaku authentication"
                );
                output.push(DanmakuEvent::Status("B站弹幕认证成功".into()));
            }
            _ => {}
        }
        ensure!(
            output.len() <= MAX_EVENTS_PER_FRAME,
            "too many danmaku events"
        );
        bytes = &bytes[length..];
    }
    Ok(())
}

pub fn encode_stt(fields: &[(&str, &str)]) -> String {
    let escape = |value: &str| value.replace('@', "@A").replace('/', "@S");
    fields
        .iter()
        .map(|(key, value)| format!("{}@={}/", escape(key), escape(value)))
        .collect()
}

pub fn decode_stt(body: &str) -> Result<BTreeMap<String, String>> {
    ensure!(body.len() <= MAX_WIRE_BYTES, "STT message exceeds budget");
    let mut result = BTreeMap::new();
    for entry in body.split('/').filter(|entry| !entry.is_empty()) {
        ensure!(result.len() < 256, "too many STT fields");
        let Some((key, value)) = entry.split_once("@=") else {
            continue;
        };
        let decode = |part: &str| part.replace("@S", "/").replace("@A", "@");
        result.insert(decode(key), decode(value));
    }
    Ok(result)
}

pub fn douyu_packet(body: &str, server: bool) -> Result<Vec<u8>> {
    ensure!(
        body.len() <= MAX_WIRE_BYTES - 13 && !body.contains('\0'),
        "invalid Douyu payload"
    );
    let length = (body.len() + 9) as u32;
    let mut packet = Vec::with_capacity(length as usize + 4);
    packet.extend_from_slice(&length.to_le_bytes());
    packet.extend_from_slice(&length.to_le_bytes());
    packet.extend_from_slice(&(if server { 690_u16 } else { 689_u16 }).to_le_bytes());
    packet.extend_from_slice(&[0, 0]);
    packet.extend_from_slice(body.as_bytes());
    packet.push(0);
    Ok(packet)
}

#[derive(Default)]
pub struct DouyuDecoder {
    pending: Vec<u8>,
}
#[derive(Default, Debug)]
pub struct DouyuBatch {
    pub events: Vec<DanmakuEvent>,
    pub ping_requested: bool,
}

impl DouyuDecoder {
    pub fn finish(&self) -> Result<()> {
        ensure!(self.pending.is_empty(), "truncated Douyu frame");
        Ok(())
    }
    /// TCP reads may split or concatenate frames at arbitrary byte positions.
    pub fn push(&mut self, bytes: &[u8]) -> Result<DouyuBatch> {
        ensure!(
            self.pending.len().saturating_add(bytes.len()) <= MAX_WIRE_BYTES,
            "Douyu pending frame exceeds budget"
        );
        self.pending.extend_from_slice(bytes);
        let mut consumed = 0;
        let mut batch = DouyuBatch::default();
        while self.pending.len() - consumed >= 12 {
            let frame = &self.pending[consumed..];
            let length = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
            ensure!(
                length >= 9 && length <= MAX_WIRE_BYTES - 4,
                "invalid Douyu frame length"
            );
            ensure!(frame[..4] == frame[4..8], "Douyu repeated length mismatch");
            ensure!(
                u16::from_le_bytes(frame[8..10].try_into().unwrap()) == 690
                    && frame[10..12] == [0, 0],
                "invalid Douyu frame type"
            );
            if frame.len() < length + 4 {
                break;
            }
            ensure!(frame[length + 3] == 0, "Douyu frame lacks terminator");
            let body = std::str::from_utf8(&frame[12..length + 3])?;
            let fields = decode_stt(body)?;
            match fields.get("type").map(String::as_str) {
                Some("pingreq") => batch.ping_requested = true,
                Some("loginres") => batch
                    .events
                    .push(DanmakuEvent::Status("斗鱼弹幕已登录".into())),
                Some("chatmsg") => {
                    if let Some(event) = douyu_message(&fields) {
                        batch.events.push(DanmakuEvent::Message(event));
                    }
                }
                Some("dgb") | Some("uenter") => {
                    let gift = fields.get("type").is_some_and(|s| s == "dgb");
                    let user = limit(fields.get("nn").map(String::as_str).unwrap_or(""));
                    batch.events.push(DanmakuEvent::Live(LiveEvent {
                        id: limit(fields.get("cid").map(String::as_str).unwrap_or("")),
                        kind: if gift {
                            LiveEventKind::Gift
                        } else {
                            LiveEventKind::Enter
                        },
                        user,
                        text: if gift {
                            limit(fields.get("gn").map(String::as_str).unwrap_or("赠送礼物"))
                        } else {
                            "进入直播间".into()
                        },
                        count: fields
                            .get("gfcnt")
                            .and_then(|s| s.parse::<u32>().ok())
                            .unwrap_or(1)
                            .clamp(1, 999999),
                    }));
                }
                Some("comm_chatmsg") if fields.get("btype").is_some_and(|s| s == "voiceDanmu") => {
                    let nested = fields
                        .get("chatmsg")
                        .map(|s| decode_stt(s))
                        .transpose()?
                        .unwrap_or_default();
                    let value = |key: &str| nested.get(key).cloned().unwrap_or_default();
                    let n = |key: &str| {
                        fields
                            .get(key)
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or(0)
                    };
                    let price_cents = n("cprice").max(n("crealPrice"));
                    let (
                        background_bottom_color,
                        background_color_start,
                        message_font_color,
                        name_color,
                    ) = douyu_super_chat_colors(price_cents);
                    batch.events.push(DanmakuEvent::SuperChat(SuperChat {
                        id: limit(&fields.get("vrid").cloned().unwrap_or_default()),
                        user: limit(&value("nn")),
                        text: limit(&value("txt")),
                        price_cents,
                        duration_secs: if n("cet") == 0 {
                            60
                        } else {
                            n("cet").min(3600) as u32
                        },
                        message_trans: String::new(),
                        face: douyu_face(&value("ic")),
                        face_frame: String::new(),
                        user_level: value("level").parse::<u64>().unwrap_or(0).min(600) as u16,
                        guard_level: 0,
                        medal: douyu_medal(&value("bnn"), &value("bl")),
                        background_color: 0xedf5ff,
                        background_bottom_color,
                        background_color_start,
                        message_font_color,
                        name_color,
                    }));
                }
                _ => {}
            }
            ensure!(
                batch.events.len() <= MAX_EVENTS_PER_FRAME,
                "too many Douyu events in one batch"
            );
            consumed += length + 4;
        }
        self.pending.drain(..consumed);
        Ok(batch)
    }
}

fn douyu_message(fields: &BTreeMap<String, String>) -> Option<DanmakuMessage> {
    let body = fields.get("txt")?;
    if body.is_empty() {
        return None;
    }
    let field = |key: &str| fields.get(key).map(String::as_str).unwrap_or("");
    Some(DanmakuMessage {
        image_url: None,
        id: limit(field("cid")),
        user: limit(field("nn")),
        text: limit(body),
        color: douyu_color(field("col")),
        mode: DanmakuMode::Scroll,
        font_size: 25,
        user_id: field("uid").parse::<u64>().unwrap_or(0),
        user_level: field("level").parse::<u64>().unwrap_or(0).min(600) as u16,
        guard_level: 0,
        medal: douyu_medal(field("bnn"), field("bl")),
        image_width: 0,
        image_height: 0,
        image_large: false,
    })
}

/// Platform chat color index (0 = white). Table cross-checked against the
/// legacy DouyuColor switch and the referenced colour tables.
fn douyu_color(index: &str) -> u32 {
    match index.parse::<u8>().unwrap_or(0) {
        1 => 0xff0000,
        2 => 0x1e87f0,
        3 => 0x7ac84b,
        4 => 0xff7f00,
        5 => 0x9b39f4,
        6 => 0xff69b4,
        _ => 0xffffff,
    }
}

fn douyu_medal(name: &str, level: &str) -> Option<Medal> {
    let name = limit(name);
    if name.is_empty() {
        return None;
    }
    Some(Medal {
        name,
        level: level.parse::<u64>().unwrap_or(0).min(999) as u16,
        color: 0,
    })
}

/// Douyu avatar keys are plain upload names; anything else is rejected rather
/// than pasted into a CDN URL.
fn douyu_face(name: &str) -> String {
    if name.is_empty()
        || name.len() > 256
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return String::new();
    }
    let address = format!("https://apic.douyucdn.cn/upload/{name}_small.jpg");
    crate::images::approved_url(&address)
        .map(|url| url.to_string())
        .unwrap_or_default()
}

/// Local price tiers for Douyu's paid messages (the platform sends no colours).
fn douyu_super_chat_colors(price_cents: u64) -> (u32, u32, u32, u32) {
    let yuan = price_cents / 100;
    if yuan >= 1000 {
        (0x962814, 0x962814, 0x5a140a, 0x781e0a)
    } else if yuan >= 100 {
        (0xbe2850, 0xbe2850, 0x781432, 0xa01e46)
    } else if yuan >= 50 {
        (0xc87814, 0xc87814, 0x824600, 0xaa6414)
    } else {
        (0x292a60, 0xc1c1ff, 0xf5f5ff, 0xe1e1ff)
    }
}

/// Minimal bounded protobuf field view; groups are rejected, unknown fields skipped.
#[derive(Debug)]
struct ProtoField<'a> {
    tag: u32,
    bytes: Option<&'a [u8]>,
    number: Option<u64>,
}

fn varint(bytes: &[u8], cursor: &mut usize) -> Result<u64> {
    let mut value = 0_u64;
    for shift in (0..70).step_by(7) {
        let byte = *bytes
            .get(*cursor)
            .ok_or_else(|| anyhow::anyhow!("truncated protobuf varint"))?;
        *cursor += 1;
        ensure!(shift != 63 || byte <= 1, "overflowing protobuf varint");
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    bail!("overflowing protobuf varint")
}

fn proto_fields(bytes: &[u8]) -> Result<Vec<ProtoField<'_>>> {
    let mut cursor = 0;
    let mut fields = Vec::new();
    while cursor < bytes.len() {
        ensure!(fields.len() < 8192, "too many protobuf fields");
        let key = varint(bytes, &mut cursor)?;
        let tag = u32::try_from(key >> 3)?;
        ensure!(tag != 0, "invalid protobuf field zero");
        let start;
        let length;
        match key & 7 {
            0 => {
                let number = varint(bytes, &mut cursor)?;
                fields.push(ProtoField {
                    tag,
                    bytes: None,
                    number: Some(number),
                });
                continue;
            }
            1 => {
                start = cursor;
                length = 8;
            }
            2 => {
                length = usize::try_from(varint(bytes, &mut cursor)?)?;
                start = cursor;
            }
            5 => {
                start = cursor;
                length = 4;
            }
            _ => bail!("unsupported protobuf wire type"),
        }
        cursor = start
            .checked_add(length)
            .ok_or_else(|| anyhow::anyhow!("protobuf length overflow"))?;
        ensure!(cursor <= bytes.len(), "truncated protobuf field");
        fields.push(ProtoField {
            tag,
            bytes: if key & 7 == 2 {
                Some(&bytes[start..cursor])
            } else {
                None
            },
            number: None,
        });
    }
    Ok(fields)
}

fn proto_bytes<'a>(fields: &[ProtoField<'a>], tag: u32) -> Option<&'a [u8]> {
    fields.iter().find(|f| f.tag == tag).and_then(|f| f.bytes)
}

/// Offline Douyin PushFrame/gzip/Response decoder. Live signing/ACK is not enabled.
pub fn decode_douyin(bytes: &[u8]) -> Result<Vec<DanmakuEvent>> {
    ensure!(
        bytes.len() <= MAX_WIRE_BYTES,
        "Douyin frame exceeds wire budget"
    );
    let outer = proto_fields(bytes)?;
    let Some(payload) = proto_bytes(&outer, 8) else {
        return Ok(Vec::new());
    };
    let mut budget = MAX_EXPANDED_BYTES;
    let expanded = if payload.starts_with(&[0x1f, 0x8b]) {
        expand(GzDecoder::new(payload), &mut budget)?
    } else {
        payload.to_vec()
    };
    let response = proto_fields(&expanded)?;
    let mut output = Vec::new();
    for raw in response
        .iter()
        .filter(|f| f.tag == 1)
        .filter_map(|f| f.bytes)
    {
        let message = proto_fields(raw)?;
        if proto_bytes(&message, 1) != Some(b"WebcastChatMessage".as_slice()) {
            continue;
        }
        let Some(payload) = proto_bytes(&message, 2) else {
            continue;
        };
        let chat = proto_fields(payload)?;
        let content = proto_bytes(&chat, 3)
            .map(std::str::from_utf8)
            .transpose()?
            .unwrap_or("");
        let user = proto_bytes(&chat, 2)
            .map(proto_fields)
            .transpose()?
            .unwrap_or_default();
        let nickname = proto_bytes(&user, 3)
            .map(std::str::from_utf8)
            .transpose()?
            .unwrap_or("");
        if !content.is_empty() {
            output.push(DanmakuEvent::Message(DanmakuMessage {
                image_url: None,
                id: String::new(),
                user: limit(nickname),
                text: limit(content),
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
            }));
        }
        ensure!(
            output.len() <= MAX_EVENTS_PER_FRAME,
            "too many Douyin chat events"
        );
    }
    Ok(output)
}

/// Huya Tars is a separate explicit boundary, with no permissive false support.
pub fn decode_huya(bytes: &[u8]) -> Result<Vec<DanmakuEvent>> {
    ensure!(
        bytes.len() <= MAX_WIRE_BYTES,
        "Huya frame exceeds wire budget"
    );
    bail!("虎牙 Tars 弹幕协议尚未迁移；当前 Rust 版不能接收虎牙弹幕")
}

pub fn decode_frame(platform: Platform, bytes: &[u8]) -> Result<Vec<DanmakuEvent>> {
    match platform {
        Platform::Bili => decode_bili(bytes),
        Platform::Douyu => {
            let mut decoder = DouyuDecoder::default();
            let events = decoder.push(bytes)?.events;
            decoder.finish()?;
            Ok(events)
        }
        // Huya/Douyin live receivers live in dedicated modules; dispatch keeps one
        // public entry point for the overlay and tests.
        Platform::Douyin => crate::danmaku_douyin::decode(bytes),
        Platform::Huya => crate::danmaku_huya::decode(bytes),
    }
}

/// Start on a caller-owned worker thread. Cancellation is polled during receive
/// and backoff; bounded platform handshakes may finish before cancellation returns.
pub fn connect(
    target: &Target,
    sender: SyncSender<DanmakuEvent>,
    cancellation: Cancellation,
) -> Result<()> {
    connect_inner(target, sender, cancellation, None)
}

/// Uses only the explicitly chosen credential vault; each request obtains its
/// cookie for its exact destination. The anonymous entry point remains usable.
pub fn connect_authenticated(
    target: &Target,
    sender: SyncSender<DanmakuEvent>,
    cancellation: Cancellation,
    vault: &CookieVault,
) -> Result<()> {
    connect_with_cookie_source(target, sender, cancellation, Arc::new(vault.clone()))
}

/// Explicit request-scoped source, also usable by offline protocol fixtures.
pub fn connect_with_cookie_source(
    target: &Target,
    sender: SyncSender<DanmakuEvent>,
    cancellation: Cancellation,
    source: Arc<dyn CookieSource>,
) -> Result<()> {
    connect_inner(target, sender, cancellation, Some(source))
}

fn connect_inner(
    target: &Target,
    sender: SyncSender<DanmakuEvent>,
    cancellation: Cancellation,
    source: Option<Arc<dyn CookieSource>>,
) -> Result<()> {
    target.validate()?;
    // Huya and Douyin own their subscribe/heartbeat/reconnect lifecycle inside
    // their module; Bilibili and Douyu share the retry loop below.
    match target.platform {
        // Huya's public chat transport is anonymous. Do not expose credentials
        // merely because they were imported for the platform's other features.
        Platform::Huya => return crate::danmaku_huya::connect(target, sender, cancellation),
        Platform::Douyin => {
            // The platform-wide Douyin emoji dictionary is resolved in the
            // background exactly like B站's room dictionary: a slow, rejected or
            // unapproved request must never delay or break the WebSocket
            // handshake. A failed spawn is ignored for the same reason.
            let emoticon_source = source.clone();
            let emoticon_cancel = cancellation.clone();
            let _ = thread::Builder::new()
                .name("douyin-emoticons".into())
                .spawn(move || {
                    if let Some(dictionary) = fetch_douyin_emoticons(emoticon_source.as_deref()) {
                        if !emoticon_cancel.is_cancelled() {
                            cache_emoticons(dictionary);
                        }
                    }
                });
            return crate::danmaku_douyin::connect_with_cookie_source(
                target,
                sender,
                cancellation,
                source,
            );
        }
        Platform::Bili | Platform::Douyu => {}
    }
    let mut attempt = 0_u32;
    while !cancellation.is_cancelled() {
        let result = match target.platform {
            Platform::Bili => bili_once(target, &sender, &cancellation, source.clone()),
            Platform::Douyu => douyu_once(target, &sender, &cancellation),
            _ => unreachable!(),
        };
        if cancellation.is_cancelled() {
            return Ok(());
        }
        // Keep the concrete cause in the status event so users and the probe can
        // tell TLS/handshake failures from a clean server-side close.
        let status = match &result {
            Ok(()) => "弹幕连接已关闭，正在重试".to_string(),
            Err(error) => format!("弹幕连接中断或认证受阻，正在重试：{error:#}"),
        };
        if emit(&sender, DanmakuEvent::Status(status)) == Delivery::Closed {
            return Ok(());
        }
        attempt = attempt.saturating_add(1);
        cancellable_wait(&cancellation, Duration::from_secs(1_u64 << attempt.min(5)));
    }
    Ok(())
}

fn cancellable_wait(cancel: &Cancellation, duration: Duration) {
    let deadline = Instant::now() + duration;
    while !cancel.is_cancelled() && Instant::now() < deadline {
        thread::sleep(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(50)),
        );
    }
}

/// Sockets dial fixed platform endpoints. WSS transports additionally verify
/// their TLS hostname; Douyu's anonymous port-8601 protocol is plain TCP and
/// relies on the chosen DNS/proxy route instead. The local TUN fake-IP range
/// (198.18.0.0/15) is permitted for those fixed endpoints, never as a user URL.
pub(crate) fn is_allowed_danmaku_address(address: IpAddr) -> bool {
    if crate::relay::is_public(address) {
        return true;
    }
    match address {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            octets[0] == 198 && (octets[1] & 0xfe) == 18
        }
        IpAddr::V6(_) => false,
    }
}

fn tcp_connect(host: &str, port: u16, cancel: &Cancellation) -> Result<TcpStream> {
    let mut addresses: Vec<SocketAddr> = (host, port).to_socket_addrs()?.take(8).collect();
    // Try IPv4 first: proxy fake-IP v6 answers can burn the whole connect budget.
    addresses.sort_by_key(|address| !address.is_ipv4());
    for address in addresses {
        if cancel.is_cancelled() {
            bail!("cancelled");
        }
        // Only known platform endpoints call this function.
        if !is_allowed_danmaku_address(address.ip()) {
            continue;
        }
        if let Ok(stream) = TcpStream::connect_timeout(&address, Duration::from_secs(5)) {
            stream.set_nodelay(true)?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            stream.set_write_timeout(Some(Duration::from_secs(5)))?;
            return Ok(stream);
        }
    }
    bail!("danmaku endpoint connection failed")
}

fn ws_timeout(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>, timeout: Duration) -> Result<()> {
    let tcp = match socket.get_mut() {
        MaybeTlsStream::Plain(s) => s,
        MaybeTlsStream::NativeTls(s) => s.get_mut(),
        _ => bail!("unsupported TLS stream"),
    };
    tcp.set_read_timeout(Some(timeout))?;
    Ok(())
}

fn bili_once(
    target: &Target,
    sender: &SyncSender<DanmakuEvent>,
    cancel: &Cancellation,
    source: Option<Arc<dyn CookieSource>>,
) -> Result<()> {
    let (buvid, uid, info, active_source) = if let Some(source) = source {
        let header = session::scoped_cookie(
            Some(source.as_ref()),
            Platform::Bili,
            session::BILI_DANMAKU_INFO_URL,
        )?;
        let buvid = match session::cookie_named(header.as_deref(), "buvid3") {
            Some(value) => value,
            None => crate::platforms::with_cookie_source(
                Platform::Bili,
                source.clone(),
                crate::platforms::bili_anonymous_identity,
            )?,
        };
        let merged: Arc<dyn CookieSource> =
            Arc::new(session::BiliCookieSource::new(source, buvid.clone())?);
        let uid = if session::cookie_named(header.as_deref(), "SESSDATA").is_some() {
            crate::platforms::with_cookie_source(Platform::Bili, merged.clone(), || {
                let verified_uid = crate::platforms::bili_login_uid()?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "B站已导入的登录凭据无法核验；请重新导入包含 API 域作用域的有效凭据"
                    )
                })?;
                session::bili_uid_for_auth(header.as_deref(), Some(verified_uid))
            })?
        } else {
            session::bili_uid_for_auth(header.as_deref(), None)?
        };
        let info = crate::platforms::with_cookie_source(Platform::Bili, merged.clone(), || {
            crate::platforms::bili_danmaku_info(&target.room, None)
        })?;
        (buvid, uid, info, Some(merged))
    } else {
        let buvid = crate::platforms::bili_anonymous_identity()?;
        let cookie = format!("buvid3={buvid}");
        let info = crate::platforms::bili_danmaku_info(&target.room, Some(&cookie))?;
        (buvid, 0, info, None)
    };
    if cancel.is_cancelled() {
        return Ok(());
    }
    let room = info["real_room_id"]
        .as_str()
        .unwrap_or(&target.room)
        .parse::<u64>()?;
    let token = info["token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing Bilibili danmaku token"))?;
    let host = info["host_list"]
        .as_array()
        .and_then(|hosts| hosts.first())
        .and_then(|h| h["host"].as_str())
        .unwrap_or("broadcastlv.chat.bilibili.com");
    ensure!(
        host.ends_with(".bilibili.com")
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-')),
        "unexpected Bilibili danmaku endpoint"
    );
    let stream = tcp_connect(host, 443, cancel)?;
    let ws_url = format!("wss://{host}/sub");
    let mut request = ws_url.clone().into_client_request()?;
    request
        .headers_mut()
        .insert("Origin", "https://live.bilibili.com".parse()?);
    if let Some(cookie) = session::scoped_cookie(active_source.as_deref(), Platform::Bili, &ws_url)?
    {
        request.headers_mut().insert(
            "Cookie",
            cookie
                .parse()
                .map_err(|_| anyhow::anyhow!("B站 WebSocket 凭据格式无效"))?,
        );
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_WIRE_BYTES))
        .max_frame_size(Some(MAX_WIRE_BYTES));
    let (mut socket, _) = tungstenite::client_tls_with_config(request, stream, Some(config), None)
        .map_err(|_| anyhow::anyhow!("Bilibili WebSocket handshake failed"))?;
    ws_timeout(&mut socket, Duration::from_millis(500))?;
    let auth = serde_json::to_vec(
        &serde_json::json!({"uid":uid,"roomid":room,"protover":2,"platform":"web","type":2,"buvid":buvid,"key":token}),
    )?;
    // Emoticon dictionaries are resolved in the background: a slow or rejected
    // room-emoticon request must never delay or break the danmaku handshake.
    let emoticon_source = active_source.clone();
    let emoticon_cancel = cancel.clone();
    let _ = thread::Builder::new()
        .name("bili-emoticons".into())
        .spawn(move || {
            if let Some(dictionary) = fetch_bili_emoticons(emoticon_source.as_deref(), room) {
                if !emoticon_cancel.is_cancelled() {
                    cache_emoticons(dictionary);
                }
            }
        });
    socket.send(Message::Binary(bili_packet(7, 1, &auth)?.into()))?;
    socket.send(Message::Binary(
        bili_packet(2, 1, b"[object Object]")?.into(),
    ))?;
    let mut last_heartbeat = Instant::now();
    while !cancel.is_cancelled() {
        if last_heartbeat.elapsed() >= Duration::from_secs(30) {
            socket.send(Message::Binary(
                bili_packet(2, 1, b"[object Object]")?.into(),
            ))?;
            last_heartbeat = Instant::now();
        }
        match socket.read() {
            Ok(Message::Binary(bytes)) => {
                for event in decode_bili(&bytes)? {
                    if emit(sender, event) == Delivery::Closed {
                        return Ok(());
                    }
                }
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(Message::Ping(_)) => {
                socket.flush()?;
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => bail!("Bilibili WebSocket closed"),
        }
    }
    let _ = socket.close(None);
    Ok(())
}

/// The overlay runs as its own process: its receiver thread and painter share
/// this bounded snapshot instead of widening the public event channel (the CLI
/// probe matches every event variant exhaustively). The dictionary is a
/// best-effort supplement; a failed fetch keeps the previous snapshot.
static EMOTICON_CACHE: Mutex<Option<rich::EmoticonDictionary>> = Mutex::new(None);
/// Generation of the snapshot above. Bumped exactly once per stored
/// dictionary so the overlay repaints when the first (or a replacement)
/// dictionary arrives, without cloning the dictionary every frame.
static EMOTICON_CACHE_VERSION: AtomicU64 = AtomicU64::new(0);

/// Latest room emoticon dictionary received by this process, if any.
pub fn cached_emoticons() -> Option<rich::EmoticonDictionary> {
    EMOTICON_CACHE.lock().ok().and_then(|guard| guard.clone())
}

/// Generation of the cached dictionary; `0` while nothing was published.
pub fn cached_emoticons_version() -> u64 {
    EMOTICON_CACHE_VERSION.load(Ordering::Acquire)
}

/// Replaces the snapshot; an empty dictionary never erases a working one, and
/// only an actual store bumps `cached_emoticons_version`.
pub fn cache_emoticons(dictionary: rich::EmoticonDictionary) {
    if dictionary.is_empty() {
        return;
    }
    if let Ok(mut guard) = EMOTICON_CACHE.lock() {
        *guard = Some(dictionary);
        EMOTICON_CACHE_VERSION.fetch_add(1, Ordering::AcqRel);
    }
}

/// Detailed GetEmoticons request used by the real acceptance test. Every step
/// keeps its own error context (URL approval, DNS/client, cookie assembly, HTTP
/// status, platform JSON code, parse result) so a failure is diagnosable without
/// exposing cookie values. Only "has a cookie" is reported as a boolean.
fn fetch_bili_emoticons_result(
    source: Option<&dyn CookieSource>,
    room: u64,
) -> Result<rich::EmoticonDictionary> {
    let address = format!(
        "https://api.live.bilibili.com/xlive/web-ucenter/v2/emoticon/GetEmoticons?platform=pc&room_id={room}"
    );
    let approved = crate::network::approved_url(&address, &["api.live.bilibili.com"], &[])
        .context("表情接口地址未通过白名单")?;
    let client = crate::network::client(
        &approved,
        &["api.live.bilibili.com"],
        &[],
        Duration::from_secs(6),
    )
    .context("表情接口客户端/DNS 初始化失败")?;
    let cookie = session::scoped_cookie(source, Platform::Bili, address.as_str())
        .context("表情接口 Cookie 组装失败")?;
    let has_cookie = cookie.as_ref().is_some_and(|value| !value.is_empty());
    let mut request = client
        .get(approved.clone())
        .header("User-Agent", "LiveHubRust/0.2 (+local desktop client)")
        .header("Referer", "https://live.bilibili.com/");
    if let Some(cookie) = cookie {
        request = request.header("Cookie", cookie);
    }
    let response = request.send().map_err(|error| {
        anyhow::anyhow!("表情接口请求失败（已带B站 cookie={has_cookie}）: {error}")
    })?;
    let status = response.status();
    ensure!(
        status.is_success(),
        "表情接口 HTTP {status}（已带B站 cookie={has_cookie}）"
    );
    if response
        .content_length()
        .is_some_and(|length| length > rich::MAX_EMOTICON_JSON_BYTES as u64)
    {
        bail!("表情接口响应超过大小上限");
    }
    let mut bytes = Vec::new();
    response
        .take(rich::MAX_EMOTICON_JSON_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .context("读取表情接口响应失败")?;
    ensure!(
        bytes.len() <= rich::MAX_EMOTICON_JSON_BYTES,
        "表情接口响应超过大小上限"
    );
    let text = std::str::from_utf8(&bytes).context("表情接口响应不是 UTF-8")?;
    let value: serde_json::Value = serde_json::from_str(text).context("表情接口响应不是 JSON")?;
    if let Some(code) = value["code"].as_i64() {
        ensure!(
            code == 0,
            "表情接口返回 code={code} message={}（room_id={room}，已带B站 cookie={has_cookie}）",
            value["message"].as_str().unwrap_or("")
        );
    }
    let dictionary = rich::parse_bili_emoticons(text).context("表情字典解析失败")?;
    ensure!(
        !dictionary.is_empty(),
        "表情接口返回空字典（room_id={room}）"
    );
    Ok(dictionary)
}

/// Best-effort fetch of the current room's bracketed emoticon dictionary
/// (B站 GetEmoticons). The real room id is required; the display id returns an
/// error, so the caller passes the resolved room. Failures are silent by design
/// and only cost the literal-code images, never the danmaku stream.
fn fetch_bili_emoticons(
    source: Option<&dyn CookieSource>,
    room: u64,
) -> Option<rich::EmoticonDictionary> {
    fetch_bili_emoticons_result(source, room).ok()
}

/// Resolves the display room id to the real room id with the given credential
/// source. Returns the display id as a fallback and a note describing which id
/// was used, so the acceptance test can print what actually happened.
fn resolve_bili_real_room(
    source: Option<std::sync::Arc<dyn CookieSource>>,
    room: u64,
) -> (u64, String) {
    let display = room.to_string();
    let info = match source {
        Some(source) => crate::platforms::with_cookie_source(
            Platform::Bili,
            std::sync::Arc::new(CookieSourceArc(source)),
            || crate::platforms::bili_danmaku_info(&display, None),
        ),
        None => crate::platforms::bili_anonymous_identity().and_then(|buvid| {
            crate::platforms::bili_danmaku_info(&display, Some(&format!("buvid3={buvid}")))
        }),
    };
    match info {
        Ok(info) => match info["real_room_id"]
            .as_str()
            .and_then(|value| value.parse::<u64>().ok())
        {
            Some(real) => (
                real,
                format!("弹幕信息解析真实房间号成功（展示号 {room} → 真实 {real}）"),
            ),
            None => (room, "弹幕信息缺少 real_room_id，回退展示号".to_owned()),
        },
        Err(error) => (room, format!("解析真实房间号失败，回退展示号：{error:#}")),
    }
}

/// Adapter so the owned request-scoped source can be handed to the platform
/// helper, which expects its own Arc value.
struct CookieSourceArc(std::sync::Arc<dyn CookieSource>);
impl CookieSource for CookieSourceArc {
    fn header_for_url(&self, platform: Platform, address: &str) -> Result<Option<String>> {
        self.0.header_for_url(platform, address)
    }
}

/// Authenticated room-emoticon fetch with full diagnostics for the real
/// acceptance test (the API answers -101 without a login cookie).
pub fn fetch_bili_emoticons_authenticated_result(
    source: std::sync::Arc<dyn CookieSource>,
    room: u64,
) -> Result<rich::EmoticonDictionary> {
    let (real, note) = resolve_bili_real_room(Some(source.clone()), room);
    let dictionary = fetch_bili_emoticons_result(Some(source.as_ref()), real)
        .with_context(|| format!("GetEmoticons 失败；{note}"))?;
    Ok(dictionary)
}

pub fn fetch_bili_emoticons_authenticated(
    source: std::sync::Arc<dyn CookieSource>,
    room: u64,
) -> Option<rich::EmoticonDictionary> {
    fetch_bili_emoticons_authenticated_result(source, room).ok()
}

/// Anonymous room-emoticon fetch with diagnostics (normally unavailable because
/// the platform requires a login: code -101).
pub fn fetch_bili_emoticons_anonymous_result(room: u64) -> Result<rich::EmoticonDictionary> {
    let (real, note) = resolve_bili_real_room(None, room);
    fetch_bili_emoticons_result(None, real).with_context(|| format!("GetEmoticons 失败；{note}"))
}

pub fn fetch_bili_emoticons_anonymous(room: u64) -> Option<rich::EmoticonDictionary> {
    fetch_bili_emoticons_anonymous_result(room).ok()
}

/// Detailed Douyin `emoji/list` fetch. The dedicated platform module owns the
/// approved request and bounded parsing; the dictionary is platform-wide, so
/// no room id is involved.
pub fn fetch_douyin_emoticons_result(
    source: Option<&dyn CookieSource>,
) -> Result<rich::EmoticonDictionary> {
    crate::danmaku_douyin::fetch_emoticons_result(source)
}

/// Best-effort Douyin dictionary: failures are silent by design and only cost
/// the literal bracketed codes, never the danmaku stream.
pub fn fetch_douyin_emoticons(
    source: Option<&dyn CookieSource>,
) -> Option<rich::EmoticonDictionary> {
    fetch_douyin_emoticons_result(source).ok()
}

/// Anonymous variant used by the opt-in live acceptance test; the default
/// offline suite never contacts the platform.
pub fn fetch_douyin_emoticons_anonymous_result() -> Result<rich::EmoticonDictionary> {
    fetch_douyin_emoticons_result(None)
}

pub fn fetch_douyin_emoticons_anonymous() -> Option<rich::EmoticonDictionary> {
    fetch_douyin_emoticons_anonymous_result().ok()
}

fn douyu_once(
    target: &Target,
    sender: &SyncSender<DanmakuEvent>,
    cancel: &Cancellation,
) -> Result<()> {
    ensure!(
        !target.room.is_empty() && target.room.bytes().all(|b| b.is_ascii_digit()),
        "斗鱼弹幕暂需数字房间号，请先从搜索中选择主播房间"
    );
    let mut stream = tcp_connect("danmuproxy.douyu.com", 8601, cancel)?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    let login = encode_stt(&[("type", "loginreq"), ("roomid", &target.room)]);
    stream.write_all(&douyu_packet(&login, false)?)?;
    let group = encode_stt(&[
        ("type", "joingroup"),
        ("gid", "-9999"),
        ("rid", &target.room),
    ]);
    stream.write_all(&douyu_packet(&group, false)?)?;
    let mut decoder = DouyuDecoder::default();
    let mut buffer = [0; 8192];
    let heartbeat = douyu_packet("type@=mrkl/", false)?;
    let mut last_heartbeat = Instant::now();
    while !cancel.is_cancelled() {
        if last_heartbeat.elapsed() >= Duration::from_secs(40) {
            stream.write_all(&heartbeat)?;
            last_heartbeat = Instant::now();
        }
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                let batch = decoder.push(&buffer[..count])?;
                if batch.ping_requested {
                    stream.write_all(&heartbeat)?;
                }
                for event in batch.events {
                    if emit(sender, event) == Delivery::Closed {
                        return Ok(());
                    }
                }
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => bail!("Douyu TCP closed"),
        }
    }
    Ok(())
}

/// One flying danmaku as the scheduler knows it: geometry in scheduler points.
/// Painted independently of the window: the overlay only converts positions into
/// egui points, so this type stays unit-testable and offline.
#[derive(Debug, Clone)]
pub struct FlyingDanmaku {
    pub message: DanmakuMessage,
    pub lane: usize,
    pub y: f32,
    /// Painted height; a large image emoticon may occupy several text rows.
    pub height: f32,
    pub width: f32,
    /// Per-message glyph scale (crowded-screen shrink / merged growth); 1.0 is
    /// the configured size.
    pub scale: f32,
    pub start: f64,
    /// Horizontal speed in points per second (always positive; the direction
    /// comes from the message mode). Fixed messages keep 0.
    pub speed: f32,
    pub viewport_width: f32,
    /// Duration of one full crossing at the speed this item was placed with.
    pub duration: f64,
}

impl FlyingDanmaku {
    pub fn x(&self, now: f64) -> f32 {
        let elapsed = (now - self.start).max(0.0) as f32;
        match self.message.mode {
            DanmakuMode::Scroll => self.viewport_width - elapsed * self.speed,
            DanmakuMode::Reverse => elapsed * self.speed - self.width,
            DanmakuMode::Top | DanmakuMode::Bottom => (self.viewport_width - self.width) / 2.0,
        }
    }

    /// Whether the flight is still visible; fixed messages expire on time.
    pub fn visible(&self, now: f64) -> bool {
        match self.message.mode {
            DanmakuMode::Scroll => self.x(now) + self.width > 0.0,
            DanmakuMode::Reverse => self.x(now) < self.viewport_width,
            DanmakuMode::Top | DanmakuMode::Bottom => now - self.start < FIXED_LIFETIME_SECS,
        }
    }

    /// Fraction of the flight already completed (0 = just entering the right
    /// edge, 1 = fully gone). The legacy renderer preserves this fraction when
    /// the picture is resized, so running messages keep their pace instead of
    /// jumping back to the entrance.
    pub fn progress(&self, now: f64) -> f32 {
        if !now.is_finite() || !self.start.is_finite() {
            return 0.0;
        }
        let elapsed = (now - self.start).max(0.0);
        let duration = match self.message.mode {
            DanmakuMode::Scroll | DanmakuMode::Reverse => self.duration.max(0.01),
            DanmakuMode::Top | DanmakuMode::Bottom => FIXED_LIFETIME_SECS,
        };
        (elapsed / duration).clamp(0.0, 1.0) as f32
    }

}

/// Track height for one row, measured the way the legacy renderer measured it:
/// the real text box (glyph height plus stroke padding) wins over the colour
/// emoji box. The line-gap percentage is applied on top of the rounded value.
pub fn track_height_points(font_size: f32, line_gap_percent: u16) -> f32 {
    let font = if font_size.is_finite() {
        font_size.clamp(6.0, 128.0)
    } else {
        25.0
    };
    let text_box = font * 1.33 + 6.0;
    let emoji_box = font * 1.18 + 6.0;
    let base = text_box.max(emoji_box).ceil();
    base * (f32::from(line_gap_percent.clamp(60, 250)) / 100.0)
}

/// Cross-screen duration for one speed percentage. The legacy renderer crosses
/// the screen in a fixed duration (8 s at 100%, clamped to at least 2 s); a
/// fixed pixel speed makes long messages feel sluggish.
pub fn crossing_duration_secs(speed_percent: u16) -> f64 {
    let percent = f64::from(speed_percent.clamp(25, 400)) / 100.0;
    (8.0 / percent).clamp(2.0, 32.0)
}

pub struct LaneScheduler {
    width: f32,
    height: f32,
    font_size: f32,
    base_line_height: f32,
    line_height: f32,
    line_gap_percent: u16,
    duration: f64,
    /// Horizontal clearance kept between two messages whose paths overlap.
    gap: f32,
    active: Vec<FlyingDanmaku>,
    max_active: usize,
    reserved_bands: Vec<(f32, f32)>,
    /// Bands only fixed messages avoid (legacy paid-message reserved band).
    fixed_reserved_bands: Vec<(f32, f32)>,
    /// Real paid-message card rectangle (y0, y1, x0, x1 in scheduler points).
    /// Fixed messages avoid it only when their centred row actually crosses it,
    /// matching the legacy HitsSc path check instead of blocking whole lanes.
    fixed_obstacle: Option<(f32, f32, f32, f32)>,
    top_zone_percent: u8,
    bottom_zone_percent: u8,
    max_fixed_per_mode: u8,
}

impl LaneScheduler {
    /// Constructs a scheduler where one message crosses the viewport in
    /// duration_secs seconds. The overlay derives the duration from the speed
    /// percentage; tests can pass 8.0 for the legacy standard.
    pub fn new(width: f32, height: f32, font_size: f32, duration_secs: f32) -> Self {
        let font_size = if font_size.is_finite() {
            font_size.clamp(6.0, 128.0)
        } else {
            25.0
        };
        let base_line_height = track_height_points(font_size, 100);
        Self {
            width: width.max(1.0),
            height: height.max(1.0),
            font_size,
            base_line_height,
            line_height: base_line_height,
            line_gap_percent: 100,
            duration: if duration_secs.is_finite() {
                f64::from(duration_secs).clamp(2.0, 32.0)
            } else {
                8.0
            },
            gap: 14.0,
            active: Vec::new(),
            max_active: 300,
            reserved_bands: Vec::new(),
            fixed_reserved_bands: Vec::new(),
            fixed_obstacle: None,
            top_zone_percent: 40,
            bottom_zone_percent: 40,
            max_fixed_per_mode: 6,
        }
    }

    /// Updates the glyph size used for row metrics (font-size changes re-lay the
    /// layer through take_active + push_measured_progress).
    pub fn set_font(&mut self, font_size: f32) {
        if !font_size.is_finite() {
            return;
        }
        self.font_size = font_size.clamp(6.0, 128.0);
        self.base_line_height = track_height_points(self.font_size, 100);
        self.line_height = self.base_line_height * f32::from(self.line_gap_percent) / 100.0;
    }

    pub fn font_size(&self) -> f32 {
        self.font_size
    }

    /// Sets the cross-screen duration (from the speed percentage).
    pub fn set_duration(&mut self, duration_secs: f32) {
        if duration_secs.is_finite() {
            self.duration = f64::from(duration_secs).clamp(2.0, 32.0);
        }
    }

    pub fn duration(&self) -> f64 {
        self.duration
    }

    /// Updates the viewport geometry without touching the running messages.
    /// Needed when only the glyph size changed (the caller then re-measures and
    /// re-places through take_active/push_measured_progress): without this the
    /// scheduler would keep its construction-time width (640) and every new
    /// message would enter from the middle of a wider window instead of its
    /// right edge.
    pub fn set_viewport(&mut self, width: f32, height: f32) {
        if width.is_finite() && width > 0.0 {
            self.width = width.max(1.0);
        }
        if height.is_finite() && height > 0.0 {
            self.height = height.max(1.0);
        }
    }

    pub fn viewport(&self) -> (f32, f32) {
        (self.width, self.height)
    }

    /// Clearance between two overlapping flights, in points.
    pub fn set_gap(&mut self, gap: f32) {
        if gap.is_finite() {
            self.gap = gap.clamp(0.0, 64.0);
        }
    }

    /// Independent fixed-message zones and per-mode limits, mirroring the
    /// legacy TopFixedZonePercent / BottomFixedZonePercent / MaxFixedPerMode.
    /// A zero per-mode limit means unlimited.
    pub fn set_fixed_layout(
        &mut self,
        top_zone_percent: u8,
        bottom_zone_percent: u8,
        max_fixed_per_mode: u8,
    ) {
        self.top_zone_percent = top_zone_percent.clamp(10, 100);
        self.bottom_zone_percent = bottom_zone_percent.clamp(10, 100);
        self.max_fixed_per_mode = max_fixed_per_mode.min(60);
    }

    /// Line spacing percentage around the base line height (60-250). The caller
    /// re-lays the active items (the legacy renderer also re-places them).
    pub fn set_line_gap_percent(&mut self, percent: u16) {
        self.line_gap_percent = percent.clamp(60, 250);
        self.line_height = self.base_line_height * f32::from(self.line_gap_percent) / 100.0;
    }

    /// Bands (top, bottom in scheduler points) that no danmaku lane may use.
    pub fn set_reserved_bands(&mut self, mut bands: Vec<(f32, f32)>) {
        bands.retain(|(top, bottom)| top.is_finite() && bottom.is_finite() && bottom > top);
        bands.sort_by(|left, right| left.0.total_cmp(&right.0));
        if bands != self.reserved_bands {
            self.reserved_bands = bands;
        }
    }

    pub fn reserved_bands(&self) -> &[(f32, f32)] {
        &self.reserved_bands
    }

    /// Sets/clears the paid-message card rectangle that fixed messages avoid
    /// when their centred row crosses it. None when no SC card is painted.
    pub fn set_fixed_obstacle(&mut self, obstacle: Option<(f32, f32, f32, f32)>) {
        self.fixed_obstacle = obstacle.filter(|(y0, y1, x0, x1)| {
            y0.is_finite()
                && y1.is_finite()
                && x0.is_finite()
                && x1.is_finite()
                && y1 > y0
                && x1 > x0
        });
    }

    /// Bands only fixed messages avoid (paid-message card area).
    pub fn set_fixed_reserved_bands(&mut self, mut bands: Vec<(f32, f32)>) {
        bands.retain(|(top, bottom)| top.is_finite() && bottom.is_finite() && bottom > top);
        bands.sort_by(|left, right| left.0.total_cmp(&right.0));
        if bands != self.fixed_reserved_bands {
            self.fixed_reserved_bands = bands;
        }
    }

    fn lane_y_span(&self, lane: usize, rows: usize, mode: DanmakuMode) -> f32 {
        if mode == DanmakuMode::Bottom {
            self.height - (lane + rows) as f32 * self.line_height
        } else {
            lane as f32 * self.line_height
        }
    }

    fn lane_reserved_span(&self, top: f32, height: f32) -> bool {
        let bottom = top + height;
        self.reserved_bands
            .iter()
            .any(|(band_top, band_bottom)| top < *band_bottom && bottom > *band_top)
    }

    fn fixed_lane_reserved_span(&self, top: f32, height: f32) -> bool {
        let bottom = top + height;
        self.fixed_reserved_bands
            .iter()
            .any(|(band_top, band_bottom)| top < *band_bottom && bottom > *band_top)
    }

    fn zone_lanes(&self, mode: DanmakuMode) -> usize {
        let full = (self.height / self.line_height).floor().max(1.0) as usize;
        let percent = match mode {
            DanmakuMode::Top => self.top_zone_percent,
            DanmakuMode::Bottom => self.bottom_zone_percent,
            DanmakuMode::Scroll | DanmakuMode::Reverse => 100,
        };
        let zone = ((self.height * f32::from(percent) / 100.0) / self.line_height)
            .floor()
            .max(1.0) as usize;
        zone.min(full).max(1)
    }

    /// Number of one-row tracks that fit the current area (legacy trackCount).
    pub fn track_count(&self) -> usize {
        (self.height / self.line_height).floor().max(1.0) as usize
    }

    fn fixed_count(&self, mode: DanmakuMode) -> usize {
        self.active
            .iter()
            .filter(|item| item.message.mode == mode)
            .count()
    }

    fn fixed_limit_reached(&self, mode: DanmakuMode) -> bool {
        if !matches!(mode, DanmakuMode::Top | DanmakuMode::Bottom) {
            return false;
        }
        self.max_fixed_per_mode > 0
            && self.fixed_count(mode) >= usize::from(self.max_fixed_per_mode)
    }

    pub fn line_height(&self) -> f32 {
        self.line_height
    }

    pub fn active(&self) -> &[FlyingDanmaku] {
        &self.active
    }

    /// Removes and returns every active item so the caller can re-measure and
    /// re-place them at a new font size (legacy ApplyConfig + Resize path).
    pub fn take_active(&mut self) -> Vec<FlyingDanmaku> {
        std::mem::take(&mut self.active)
    }

    pub fn set_capacity(&mut self, capacity: usize) {
        self.max_active = capacity.clamp(1, 500);
        if self.active.len() > self.max_active {
            let keep = self.max_active;
            self.active.truncate(keep);
        }
    }

    pub fn advance(&mut self, now: f64) {
        self.active.retain(|item| item.visible(now));
    }

    fn speed_for(&self, width: f32) -> f32 {
        let width = if width.is_finite() { width.max(1.0) } else { 1.0 };
        ((self.width + width) / self.duration.max(0.01) as f32).clamp(1.0, 20_000.0)
    }

    fn remaining(&self, item: &FlyingDanmaku, now: f64) -> f32 {
        match item.message.mode {
            DanmakuMode::Scroll => ((item.x(now) + item.width) / item.speed.max(1.0)).max(0.0),
            DanmakuMode::Reverse => ((self.width - item.x(now)) / item.speed.max(1.0)).max(0.0),
            DanmakuMode::Top | DanmakuMode::Bottom => {
                ((item.start + FIXED_LIFETIME_SECS - now).max(0.0)) as f32
            }
        }
    }

    fn signed_velocity(item: &FlyingDanmaku) -> f32 {
        match item.message.mode {
            DanmakuMode::Scroll => -item.speed,
            DanmakuMode::Reverse => item.speed,
            DanmakuMode::Top | DanmakuMode::Bottom => 0.0,
        }
    }

    /// Full-flight path intersection (legacy PathsOverlap): two messages may
    /// share a track only if their positions stay clear of each other for as
    /// long as both are on screen.
    fn paths_overlap(
        &self,
        candidate: &FlyingDanmaku,
        other: &FlyingDanmaku,
        seconds: f32,
        now: f64,
    ) -> bool {
        // Sample both flights at the current instant: comparing their entry
        // positions would make every newly placed message look clear of the
        // message already on that lane.
        let start = candidate.x(now) - other.x(now);
        // Separation changes by (candidate velocity - other velocity), matching
        // the legacy PathsOverlap where X -= Speed*dt for leftward motion.
        let relative = Self::signed_velocity(candidate) - Self::signed_velocity(other);
        let end = start + relative * seconds.max(0.0);
        let gap = self.gap;
        !(start.min(end) >= other.width + gap || start.max(end) + candidate.width + gap <= 0.0)
    }

    fn can_place(&self, candidate: &FlyingDanmaku, y: f32, now: f64) -> bool {
        if y < -0.5 || y + candidate.height > self.height + 0.5 {
            return false;
        }
        let fixed = matches!(
            candidate.message.mode,
            DanmakuMode::Top | DanmakuMode::Bottom
        );
        if fixed {
            // Legacy CanPlace: a fixed message wider than the picture is
            // refused (the caller converts it to a scrolling one instead).
            if candidate.width > self.width {
                return false;
            }
            if candidate.message.mode == DanmakuMode::Top
                && y + candidate.height > self.height * f32::from(self.top_zone_percent) / 100.0
            {
                return false;
            }
            if candidate.message.mode == DanmakuMode::Bottom
                && y < self.height * (1.0 - f32::from(self.bottom_zone_percent) / 100.0)
            {
                return false;
            }
            if self.max_fixed_per_mode > 0
                && self.fixed_count(candidate.message.mode) >= usize::from(self.max_fixed_per_mode)
            {
                return false;
            }
            if self.lane_reserved_span(y, candidate.height) {
                return false;
            }
            // Legacy HitsSc: a fixed (centred) message avoids the paid cards only
            // when its own row rectangle intersects them. Blocking the whole
            // vertical band rejected every centred top/bottom message whenever a
            // card stack was on screen.
            if let Some((top, bottom, left, right)) = self.fixed_obstacle {
                if y < bottom && y + candidate.height > top {
                    let x0 = (self.width - candidate.width) / 2.0;
                    let x1 = x0 + candidate.width;
                    if x0 < right && x1 > left {
                        return false;
                    }
                }
            }
        } else if self.lane_reserved_span(y, candidate.height) {
            return false;
        }
        let life = self.remaining(candidate, now);
        for other in &self.active {
            if y >= other.y + other.height || y + candidate.height <= other.y {
                continue;
            }
            let other_life = self.remaining(other, now);
            let seconds = life.min(other_life).max(0.0);
            if self.paths_overlap(candidate, other, seconds, now) {
                return false;
            }
        }
        true
    }

    /// Re-fits every active message into the new viewport without discarding it
    /// and without resetting its flight: the legacy renderer keeps the completed
    /// fraction of each flight, recomputes the speed for the new width and tries
    /// to keep each message on its previous track.
    pub fn refit(&mut self, width: f32, height: f32, now: f64) {
        let previous = std::mem::take(&mut self.active);
        self.width = if width.is_finite() { width.max(1.0) } else { 1.0 };
        self.height = if height.is_finite() { height.max(1.0) } else { 1.0 };
        for item in previous {
            let fixed = matches!(
                item.message.mode,
                DanmakuMode::Top | DanmakuMode::Bottom
            );
            let rows = ((item.height / self.line_height).round() as usize).clamp(1, 8);
            let progress = item.progress(now);
            let start = if fixed {
                item.start
            } else {
                now - f64::from(progress) * self.duration
            };
            let _ = self.place_at(
                item.message,
                item.width,
                rows,
                item.scale,
                start,
                now,
                Some(item.lane),
            );
        }
    }

    fn truncate_message(message: &mut DanmakuMessage) {
        message.text = limit(&message.text);
        message.user = limit(&message.user);
    }

    pub fn push(&mut self, message: DanmakuMessage, now: f64) -> Option<FlyingDanmaku> {
        let approximate = message
            .text
            .chars()
            .map(|c| if c.is_ascii() { 0.6 } else { 1.0 })
            .sum::<f32>()
            * self.line_height
            / 1.35
            + 8.0;
        self.push_measured(message, approximate, now)
    }

    pub fn push_measured(
        &mut self,
        message: DanmakuMessage,
        measured_width: f32,
        now: f64,
    ) -> Option<FlyingDanmaku> {
        self.push_measured_rows(message, measured_width, 1, now)
    }

    /// Places one message that needs rows stacked text rows. A large image
    /// emoticon asks for more than one row so neighbouring lanes stay clear.
    pub fn push_measured_rows(
        &mut self,
        message: DanmakuMessage,
        measured_width: f32,
        rows: usize,
        now: f64,
    ) -> Option<FlyingDanmaku> {
        self.place_at(message, measured_width, rows, 1.0, now, now, None)
    }

    /// Places one message with an explicit flight start time.
    pub fn push_measured_at(
        &mut self,
        message: DanmakuMessage,
        measured_width: f32,
        rows: usize,
        start: f64,
        now: f64,
    ) -> Option<FlyingDanmaku> {
        self.place_at(message, measured_width, rows, 1.0, start, now, None)
    }

    /// Places one message with an explicit per-message glyph scale.
    pub fn push_measured_scaled(
        &mut self,
        message: DanmakuMessage,
        measured_width: f32,
        rows: usize,
        scale: f32,
        now: f64,
    ) -> Option<FlyingDanmaku> {
        self.place_at(message, measured_width, rows, scale, now, now, None)
    }

    /// Re-places one already-running message at a completed-flight fraction.
    /// Used when the font size or DPI changes: the message keeps its progress
    /// and keeps its previous track whenever that track still fits.
    pub fn push_measured_progress(
        &mut self,
        message: DanmakuMessage,
        measured_width: f32,
        rows: usize,
        scale: f32,
        progress: f32,
        preferred_lane: Option<usize>,
        now: f64,
    ) -> Option<FlyingDanmaku> {
        let fixed = matches!(message.mode, DanmakuMode::Top | DanmakuMode::Bottom);
        let duration = if fixed { FIXED_LIFETIME_SECS } else { self.duration };
        let progress = if progress.is_finite() {
            progress.clamp(0.0, 1.2)
        } else {
            0.0
        };
        let start = now - f64::from(progress) * duration;
        self.place_at(
            message,
            measured_width,
            rows,
            scale,
            start,
            now,
            preferred_lane,
        )
    }

    fn place_at(
        &mut self,
        mut message: DanmakuMessage,
        measured_width: f32,
        rows: usize,
        scale: f32,
        start: f64,
        now: f64,
        preferred_lane: Option<usize>,
    ) -> Option<FlyingDanmaku> {
        if !now.is_finite()
            || !start.is_finite()
            || !measured_width.is_finite()
            || measured_width <= 0.0
        {
            return None;
        }
        self.advance(now);
        if self.active.len() >= self.max_active {
            return None;
        }
        if self.fixed_limit_reached(message.mode) {
            return None;
        }
        Self::truncate_message(&mut message);
        let width = measured_width.min(16384.0);
        let rows = rows.clamp(1, 8);
        let height = self.line_height * rows as f32;
        let fixed = matches!(message.mode, DanmakuMode::Top | DanmakuMode::Bottom);
        let speed = if fixed { 0.0 } else { self.speed_for(width) };
        let scale = if scale.is_finite() {
            scale.clamp(0.3, 1.8)
        } else {
            1.0
        };
        let mut candidate = FlyingDanmaku {
            message,
            lane: 0,
            y: 0.0,
            height,
            width,
            scale,
            start,
            speed,
            viewport_width: self.width,
            duration: if fixed { FIXED_LIFETIME_SECS } else { self.duration },
        };
        let lanes = self.zone_lanes(candidate.message.mode);
        if let Some(lane) = preferred_lane {
            if lane < lanes {
                let y = self.lane_y_span(lane, rows, candidate.message.mode);
                if self.can_place(&candidate, y, now) {
                    candidate.lane = lane;
                    candidate.y = y;
                    self.active.push(candidate.clone());
                    return Some(candidate);
                }
            }
        }
        for lane in 0..lanes {
            let y = self.lane_y_span(lane, rows, candidate.message.mode);
            if self.can_place(&candidate, y, now) {
                candidate.lane = lane;
                candidate.y = y;
                self.active.push(candidate.clone());
                return Some(candidate);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The process cache is shared with the overlay painter, so the generation
    /// must move exactly once per stored dictionary and never for an empty one.
    /// This module is the only writer in the lib test binary.
    #[test]
    fn emoticon_cache_version_tracks_stored_dictionaries() {
        let start = cached_emoticons_version();
        let mut dictionary = rich::EmoticonDictionary::new();
        assert!(dictionary.add("[微笑]", "https://p3.douyinpic.com/a.webp"));
        cache_emoticons(dictionary);
        assert_eq!(cached_emoticons_version(), start + 1);
        let cached = cached_emoticons().expect("cached dictionary");
        assert_eq!(
            cached.get("[微笑]"),
            Some("https://p3.douyinpic.com/a.webp")
        );

        // An empty dictionary must neither erase the working snapshot nor bump
        // the generation.
        cache_emoticons(rich::EmoticonDictionary::new());
        assert_eq!(cached_emoticons_version(), start + 1);
        assert_eq!(cached_emoticons().expect("still cached").len(), 1);
    }
}
