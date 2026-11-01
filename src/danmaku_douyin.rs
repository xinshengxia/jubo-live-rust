//! Douyin webcast danmaku transport (migration workstream: danmaku-douyin).
//!
//! Ported from the legacy C# implementation (`danmaku-10-douyin.cs`,
//! `danmaku-17-connection.cs`) by behaviour, not by importing its data:
//!
//! * `signature` is the pure-algorithm `frontierSign({"X-MS-STUB": md5})["X-Bogus"]`
//!   value (MD5 two hops + RC4 + custom-alphabet base64), adapted for Rust from
//!   jwwsjlm/douyinLive (MIT, Copyright (c) 2025 今晚打打). See
//!   `licenses/douyinLive-LICENSE.txt` and `THIRD_PARTY_NOTICES.md`. No JS engine,
//!   browser, or third-party signing service is used.
//! * A WebSocket binary frame is a `PushFrame` with field 8 = payload. The payload
//!   is a single layer of gzip over a `Response` protobuf. `Response.field1` is a
//!   repeated `Message` (`method`, `payload`); only `WebcastChatMessage` is turned
//!   into events.
//! * No super-chat equivalent exists on Douyin: the legacy research checked every
//!   protobuf message and found no paid-message amount/duration fields. Gift and
//!   entry messages are decoded only far enough to bound them and are not
//!   forwarded, because the shared `DanmakuEvent` contract has no gift variant.
//!
//! Scope: pure signing, bounded PushFrame/gzip/protobuf codecs, join-URL assembly,
//! heartbeat/ACK wire packets and a bounded WebSocket lifecycle. The default test
//! suite covers all of it with synthetic offline fixtures only. The anonymous
//! live path (room page → signature → Webcast WebSocket → chat events) was
//! verified manually on 2026-10-29 against public rooms; see
//! `docs/danmaku-douyin.md` for the evidence matrix and the ignored live
//! acceptance test. Explicit credential imports are scoped to every HTTP/WSS
//! destination; browser-based risk verification remains a separate feature.

use crate::auth::CookieVault;
use crate::core::{Platform, Target};
use crate::danmaku::session::{DouyinSessionCookies, now_seconds};
use crate::danmaku::{
    Cancellation, DanmakuEvent, DanmakuMessage, DanmakuMode, Delivery, MAX_EVENTS_PER_FRAME,
    MAX_EXPANDED_BYTES, MAX_TEXT_CHARS, MAX_WIRE_BYTES, emit,
};
use crate::platforms::CookieSource;
use anyhow::{Result, anyhow, bail, ensure};
use flate2::read::GzDecoder;
use serde_json::Value;
use std::io::{ErrorKind, Read};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::SyncSender;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// `webcast_sdk_version` sent on the join URL and folded into the X-MS-STUB md5.
pub const SDK_VERSION: &str = "1.0.15";
/// A bounded number of reconnect attempts after the first connection, like the
/// legacy client but explicitly finite.
pub const MAX_RECONNECTS: u32 = 3;
/// Heartbeat pacing of the legacy client; a heartbeat reply is expected within
/// `HEARTBEAT_ACK_TIMEOUT_MS`, and two consecutive misses end the connection.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
/// Upper bound for the heartbeat ACK watch (the legacy value is exactly 5 s).
pub const HEARTBEAT_ACK_TIMEOUT_MS: u64 = 5000;
/// Camera/query fingerprint hosts verified by the legacy application.
pub const WEBCAST_HOSTS: [&str; 3] = [
    "webcast100-ws-web-lq.douyin.com",
    "webcast5-ws-web-lf.douyin.com",
    "webcast5-ws-web-lq.douyin.com",
];

const SIGN_ALPHABET: &[u8; 64] =
    b"Dkdpgh4ZKsQB80/Mfvw36XI1R25+WUAlEi7NLboqYTOPuzmFjJnryx9HVGcaStCe";
const WS_MODE_FLAG: u8 = 1 << 6;
const MAX_PROTO_FIELDS: usize = 4096;
const MAX_PROTO_DEPTH: usize = 6;
const MAX_HEADERS: usize = 64;
const MAX_RESPONSE_MESSAGES: usize = MAX_EVENTS_PER_FRAME * 4;
const MAX_ROOM_PAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_FLIGHT_PAYLOADS: usize = 256;
const MAX_REDIRECTS: usize = 5;
const MAX_ROOM_FIELD_CHARS: usize = 256;
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
const BROWSER_VERSION: &str = "5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
const DOUYIN_HOST: &str = "live.douyin.com";

// ---------------------------------------------------------------------------
// Pure signing: md5(X-MS-STUB) -> frontierSign / X-Bogus
// ---------------------------------------------------------------------------

fn md5_bytes(data: &[u8]) -> [u8; 16] {
    md5::compute(data).0
}

/// Lowercase hex md5 of `data` (the legacy `Md5Hex` helper).
pub fn md5_hex(data: &[u8]) -> String {
    md5_bytes(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn hex_to_bytes(hex: &str) -> Result<Vec<u8>> {
    ensure!(hex.len() % 2 == 0, "md5 stub 必须是偶数长度十六进制");
    ensure!(hex.len() <= 64, "md5 stub 过长");
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().chunks_exact(2) {
        let text = std::str::from_utf8(pair).map_err(|_| anyhow!("md5 stub 含非法字符"))?;
        let value = u8::from_str_radix(text, 16).map_err(|_| anyhow!("md5 stub 含非法十六进制"))?;
        bytes.push(value);
    }
    Ok(bytes)
}

fn rc4(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    ensure!(!key.is_empty(), "RC4 key 不能为空");
    let mut state = [0_u8; 256];
    for (index, slot) in state.iter_mut().enumerate() {
        *slot = index as u8;
    }
    let mut j = 0_usize;
    for index in 0..256 {
        j = (j + usize::from(state[index]) + usize::from(key[index % key.len()])) & 0xFF;
        state.swap(index, j);
    }
    let mut output = Vec::with_capacity(data.len());
    let (mut a, mut b) = (0_usize, 0_usize);
    for byte in data {
        a = (a + 1) & 0xFF;
        b = (b + usize::from(state[a])) & 0xFF;
        state.swap(a, b);
        let keystream = state[(usize::from(state[a]) + usize::from(state[b])) & 0xFF];
        output.push(byte ^ keystream);
    }
    Ok(output)
}

fn custom_base64(data: &[u8]) -> String {
    let mut output = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let mut window = 0_u32;
        for index in 0..3 {
            window = (window << 8) | u32::from(*chunk.get(index).unwrap_or(&0));
        }
        for shift in [18, 12, 6, 0] {
            output.push(char::from(SIGN_ALPHABET[((window >> shift) & 63) as usize]));
        }
    }
    output
}

/// Parameterised entry point: the same deterministic core the legacy tests use.
///
/// `stub_hex` is the lowercase md5 hex of the comma-joined join parameters
/// (`X-MS-STUB`), which this function md5s again before building the payload.
pub fn sign_with_values(
    stub_hex: &str,
    counter: u8,
    random_flag: bool,
    payload_random: u8,
    key_random: u8,
) -> Result<String> {
    let stub_digest = md5_bytes(&hex_to_bytes(stub_hex)?);
    let empty_body_digest = md5_bytes(&md5_bytes(&[]));
    let mut payload = [0_u8; 10];
    payload[0] = counter & 0x3F;
    payload[1] = 0; // environmentCode
    payload[2] = 1; // environmentCode (web)
    payload[3] = 14; // userBehaviorCode
    payload[4] = empty_body_digest[14];
    payload[5] = empty_body_digest[15];
    payload[6] = stub_digest[14];
    payload[7] = stub_digest[15];
    payload[8] = payload_random;
    let mut checksum = 0_u8;
    for byte in &payload[..9] {
        checksum ^= byte;
    }
    payload[9] = checksum;

    let mut raw = Vec::with_capacity(2 + payload.len());
    raw.push(WS_MODE_FLAG | if random_flag { 0x10 } else { 0 });
    raw.push(key_random);
    raw.extend(rc4(&[key_random], &payload)?);
    Ok(custom_base64(&raw))
}

static SIGN_COUNTER: AtomicU8 = AtomicU8::new(0);

fn random_sign_values() -> (bool, u8, u8) {
    let uuid = uuid::Uuid::new_v4();
    let bytes = uuid.as_bytes();
    (bytes[0] & 1 == 0, bytes[1], bytes[2])
}

/// Online signing entry point: the counter wraps inside 0..=63 and the other
/// values are random, so every reconnect produces a fresh signature. Nothing is
/// cached.
pub fn sign(stub_hex: &str) -> Result<String> {
    let previous = SIGN_COUNTER.fetch_add(1, Ordering::Relaxed);
    let counter = previous.wrapping_add(1) & 0x3F;
    let (random_flag, payload_random, key_random) = random_sign_values();
    sign_with_values(stub_hex, counter, random_flag, payload_random, key_random)
}

/// RFC 3986 percent-encoding compatible with .NET `Uri.EscapeDataString`.
/// The signature contains `+` and `/`, which must be escaped or the handshake is
/// rejected by the platform.
pub fn percent_encode(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                output.push(char::from(*byte));
            }
            _ => {
                output.push('%');
                output.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
                output.push(char::from(HEX_DIGITS[usize::from(byte & 0x0F)]));
            }
        }
    }
    output
}

const HEX_DIGITS: &[u8; 16] = b"0123456789ABCDEF";

// ---------------------------------------------------------------------------
// Join URL assembly (room id + parameters + signature)
// ---------------------------------------------------------------------------

/// The comma-joined parameter block whose md5 becomes `X-MS-STUB`.
pub fn join_stub(room_id: &str, user_unique_id: &str) -> String {
    format!(
        "live_id=1,aid=6383,version_code=180800,webcast_sdk_version={SDK_VERSION},\
         room_id={room_id},sub_room_id=,sub_channel_id=,did_rule=3,\
         user_unique_id={user_unique_id},device_platform=web,device_type=,ac=,identity=audience"
    )
}

/// Inputs of a join URL. `cursor_random`/`wrds_random` are explicit so tests can
/// assert byte-exact URLs; `connect` fills them from the random source.
#[derive(Debug, Clone, Copy)]
pub struct JoinRequest<'a> {
    pub host: &'a str,
    pub room_id: &'a str,
    pub user_unique_id: &'a str,
    pub signature: &'a str,
    pub now_ms: u64,
    pub cursor_random: u32,
    pub wrds_random: u32,
}

fn valid_webcast_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.ends_with(".douyin.com")
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
}

fn valid_numeric_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 64 && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// `wss://<host>/webcast/im/push/v2/?…&signature=…` as assembled by the legacy
/// client. Only the signature, cursor, internal_ext and browser version need
/// percent-encoding; everything else is a fixed literal.
pub fn build_join_url(request: &JoinRequest<'_>) -> Result<String> {
    ensure!(valid_webcast_host(request.host), "抖音弹幕节点域名无效");
    ensure!(valid_numeric_id(request.room_id), "抖音房间号必须是数字串");
    ensure!(
        valid_numeric_id(request.user_unique_id),
        "抖音 user_unique_id 必须是数字串"
    );
    ensure!(
        !request.signature.is_empty()
            && request.signature.len() <= 128
            && request
                .signature
                .bytes()
                .all(|byte| SIGN_ALPHABET.contains(&byte)),
        "抖音签名非法"
    );

    let base = (request.now_ms / 1000).saturating_mul(4_294_967_296);
    let cursor = format!(
        "t-{now}_r-{cursor}_d-1_u-1_h-{wrds}",
        now = request.now_ms,
        cursor = base.saturating_add(u64::from(request.cursor_random)),
        wrds = base.saturating_add(u64::from(request.wrds_random)),
    );
    let internal_ext = format!(
        "internal_src:dim|wss_push_room_id:{room}|wss_push_did:{did}|first_req_ms:{now}|\
         fetch_time:{now}|seq:1|wss_info:0-{now}-0-0|wrds_v:{wrds}",
        room = request.room_id,
        did = request.user_unique_id,
        now = request.now_ms,
        wrds = base.saturating_add(u64::from(request.wrds_random)),
    );

    let mut url = String::with_capacity(1024);
    url.push_str("wss://");
    url.push_str(request.host);
    url.push_str("/webcast/im/push/v2/?app_name=douyin_web&version_code=180800");
    url.push_str("&webcast_sdk_version=");
    url.push_str(SDK_VERSION);
    url.push_str("&update_version_code=");
    url.push_str(SDK_VERSION);
    url.push_str("&compress=gzip&device_platform=web&cookie_enabled=true");
    url.push_str("&screen_width=1920&screen_height=1080");
    url.push_str("&browser_language=zh-CN&browser_platform=Win32&browser_name=Mozilla");
    url.push_str("&browser_version=");
    url.push_str(&percent_encode(BROWSER_VERSION));
    url.push_str("&browser_online=true&tz_name=Asia/Shanghai");
    url.push_str("&cursor=");
    url.push_str(&percent_encode(&cursor));
    url.push_str("&internal_ext=");
    url.push_str(&percent_encode(&internal_ext));
    url.push_str("&host=https://live.douyin.com&aid=6383&live_id=1&did_rule=3");
    url.push_str("&endpoint=live_pc&support_wrds=1");
    url.push_str("&user_unique_id=");
    url.push_str(request.user_unique_id);
    url.push_str("&im_path=/webcast/im/fetch/&identity=audience");
    url.push_str("&need_persist_msg_count=15&insert_task_id=&live_reason=");
    url.push_str("&room_id=");
    url.push_str(request.room_id);
    url.push_str("&heartbeatDuration=0&signature=");
    url.push_str(&percent_encode(request.signature));
    Ok(url)
}

// ---------------------------------------------------------------------------
// Minimal bounded protobuf reader/writer
// ---------------------------------------------------------------------------

struct ProtoReader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> ProtoReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn at_end(&self) -> bool {
        self.position >= self.data.len()
    }

    fn read_tag(&mut self) -> Result<Option<(u32, u8)>> {
        if self.at_end() {
            return Ok(None);
        }
        let key = self.read_varint()?;
        let field = key >> 3;
        ensure!(field != 0 && field <= 0x1FFF_FFFF, "protobuf：非法字段号");
        let wire = (key & 7) as u8;
        ensure!(
            matches!(wire, 0 | 1 | 2 | 5),
            "protobuf：不支持的 wire type {wire}"
        );
        Ok(Some((u32::try_from(field)?, wire)))
    }

    fn read_varint(&mut self) -> Result<u64> {
        let mut result = 0_u64;
        for shift in (0..70).step_by(7) {
            let byte = *self
                .data
                .get(self.position)
                .ok_or_else(|| anyhow!("protobuf：varint 截断"))?;
            self.position += 1;
            ensure!(shift != 63 || byte <= 1, "protobuf：varint 溢出");
            result |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
        }
        bail!("protobuf：varint 未终止")
    }

    fn read_length(&mut self) -> Result<usize> {
        let length = self.read_varint()?;
        let length = usize::try_from(length).map_err(|_| anyhow!("protobuf：字段长度溢出"))?;
        ensure!(
            length <= self.data.len().saturating_sub(self.position),
            "protobuf：字段长度超出报文"
        );
        Ok(length)
    }

    fn read_bytes(&mut self) -> Result<&'a [u8]> {
        let length = self.read_length()?;
        let start = self.position;
        let end = start
            .checked_add(length)
            .ok_or_else(|| anyhow!("protobuf：长度溢出"))?;
        let bytes = self
            .data
            .get(start..end)
            .ok_or_else(|| anyhow!("protobuf：字段被截断"))?;
        self.position = end;
        Ok(bytes)
    }

    fn read_string(&mut self) -> Result<&'a str> {
        std::str::from_utf8(self.read_bytes()?).map_err(|_| anyhow!("protobuf：字符串不是 UTF-8"))
    }

    fn skip(&mut self, wire: u8) -> Result<()> {
        match wire {
            0 => {
                self.read_varint()?;
                Ok(())
            }
            1 => self.skip_fixed(8),
            2 => {
                let length = self.read_length()?;
                self.position += length;
                Ok(())
            }
            5 => self.skip_fixed(4),
            _ => bail!("protobuf：不支持的 wire type {wire}"),
        }
    }

    fn skip_fixed(&mut self, length: usize) -> Result<()> {
        ensure!(
            length <= self.data.len().saturating_sub(self.position),
            "protobuf：固定长度字段截断"
        );
        self.position += length;
        Ok(())
    }
}

/// Tiny protobuf writer for heartbeat/ACK packets only.
#[derive(Debug, Default)]
pub struct ProtoWriter {
    buffer: Vec<u8>,
}

impl ProtoWriter {
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    pub fn varint(mut self, field: u32, value: u64) -> Self {
        self.push_tag(field, 0);
        self.push_varint(value);
        self
    }

    pub fn bytes(mut self, field: u32, data: &[u8]) -> Self {
        self.push_tag(field, 2);
        self.push_varint(data.len() as u64);
        self.buffer.extend_from_slice(data);
        self
    }

    pub fn string(self, field: u32, value: &str) -> Self {
        self.bytes(field, value.as_bytes())
    }

    pub fn finish(self) -> Vec<u8> {
        self.buffer
    }

    fn push_tag(&mut self, field: u32, wire: u8) {
        debug_assert!((1..=0x1FFF_FFFF).contains(&field));
        self.push_varint((u64::from(field) << 3) | u64::from(wire));
    }

    fn push_varint(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.buffer.push((value as u8) | 0x80);
            value >>= 7;
        }
        self.buffer.push(value as u8);
    }
}

// ---------------------------------------------------------------------------
// PushFrame codec (header / payloadType / payload)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushFrame<'a> {
    pub log_id: u64,
    pub headers: Vec<(&'a str, &'a str)>,
    pub payload_type: &'a str,
    pub payload: &'a [u8],
}

fn parse_header_entry(entry: &[u8]) -> Result<(&str, &str)> {
    let mut reader = ProtoReader::new(entry);
    let mut key = "";
    let mut value = "";
    let mut fields = 0_usize;
    while let Some((field, wire)) = reader.read_tag()? {
        fields += 1;
        ensure!(
            fields <= MAX_PROTO_FIELDS,
            "protobuf：PushFrame header 字段过多"
        );
        match (field, wire) {
            (1, 2) => key = reader.read_string()?,
            (2, 2) => value = reader.read_string()?,
            _ => reader.skip(wire)?,
        }
    }
    Ok((key, value))
}

/// Parse one complete `PushFrame`. Unknown fields are skipped; every length is
/// bounded by the remaining bytes and every collection has a hard cap.
pub fn parse_push_frame(frame: &[u8]) -> Result<PushFrame<'_>> {
    ensure!(frame.len() <= MAX_WIRE_BYTES, "抖音 PushFrame 超出报文上限");
    let mut reader = ProtoReader::new(frame);
    let mut log_id = 0_u64;
    let mut headers = Vec::new();
    let mut payload_type = "";
    let mut payload: &[u8] = &[];
    let mut fields = 0_usize;
    while let Some((field, wire)) = reader.read_tag()? {
        fields += 1;
        ensure!(
            fields <= MAX_PROTO_FIELDS,
            "抖音 PushFrame 字段数量超出上限"
        );
        match (field, wire) {
            (2, 0) => log_id = reader.read_varint()?,
            (5, 2) => {
                ensure!(
                    headers.len() < MAX_HEADERS,
                    "抖音 PushFrame header 数量超出上限"
                );
                headers.push(parse_header_entry(reader.read_bytes()?)?);
            }
            (7, 2) => payload_type = reader.read_string()?,
            (8, 2) => payload = reader.read_bytes()?,
            _ => reader.skip(wire)?,
        }
    }
    Ok(PushFrame {
        log_id,
        headers,
        payload_type,
        payload,
    })
}

/// Encode a `PushFrame`. A zero `log_id` is omitted (heartbeat has no log id) and
/// an empty payload omits field 8, so `encode_push_frame(0, "hb", &[], b"")`
/// produces the legacy four-byte heartbeat exactly.
pub fn encode_push_frame(
    log_id: u64,
    payload_type: &str,
    headers: &[(&str, &str)],
    payload: &[u8],
) -> Vec<u8> {
    let mut writer = ProtoWriter::new();
    if log_id != 0 {
        writer = writer.varint(2, log_id);
    }
    for (key, value) in headers.iter().take(MAX_HEADERS) {
        let entry = ProtoWriter::new().string(1, key).string(2, value).finish();
        writer = writer.bytes(5, &entry);
    }
    writer = writer.string(7, payload_type);
    if !payload.is_empty() {
        writer = writer.bytes(8, payload);
    }
    writer.finish()
}

/// `PushFrame{payload_type:"hb"}` — the legacy client sends these as binary
/// frames because it cannot send a custom WebSocket PING.
pub fn heartbeat_packet() -> Vec<u8> {
    ProtoWriter::new().string(7, "hb").finish()
}

/// `PushFrame{log_id, payload_type:"ack", payload:internal_ext}`.
pub fn ack_packet(log_id: u64, internal_ext: &str) -> Vec<u8> {
    ProtoWriter::new()
        .varint(2, log_id)
        .string(7, "ack")
        .bytes(8, internal_ext.as_bytes())
        .finish()
}

// ---------------------------------------------------------------------------
// Response / Message / Chat decoding
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct FrameInspection {
    pub heartbeat: bool,
    pub log_id: u64,
    pub internal_ext: Option<String>,
    pub events: Vec<DanmakuEvent>,
    pub messages: usize,
}

fn limit(text: &str) -> String {
    text.chars().take(MAX_TEXT_CHARS).collect()
}

fn limit_room_field(text: &str) -> String {
    text.chars().take(MAX_ROOM_FIELD_CHARS).collect()
}

fn is_gzip(payload: &[u8]) -> bool {
    payload.len() > 2 && payload[0] == 0x1F && payload[1] == 0x8B
}

fn expand_gzip(payload: &[u8], budget: &mut usize) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    GzDecoder::new(payload)
        .take(budget.saturating_add(1) as u64)
        .read_to_end(&mut output)?;
    ensure!(output.len() <= *budget, "抖音 gzip 解压超出大小预算");
    *budget -= output.len();
    Ok(output)
}

/// Inspect one complete WebSocket binary frame: heartbeat flag, ACK material and
/// bounded events. `Ok` never implies unknown/unsupported messages failed — those
/// are skipped by the method whitelist.
pub fn inspect_frame(frame: &[u8]) -> Result<FrameInspection> {
    ensure!(frame.len() <= MAX_WIRE_BYTES, "抖音弹幕帧超出报文上限");
    let frame = parse_push_frame(frame)?;
    let mut inspection = FrameInspection {
        log_id: frame.log_id,
        ..FrameInspection::default()
    };
    if frame.payload_type == "hb" {
        inspection.heartbeat = true;
        return Ok(inspection);
    }
    if frame.payload.is_empty() {
        return Ok(inspection);
    }
    let mut budget = MAX_EXPANDED_BYTES;
    let decoded = if is_gzip(frame.payload) {
        expand_gzip(frame.payload, &mut budget)?
    } else {
        frame.payload.to_vec()
    };
    parse_response(&decoded, &mut inspection)?;
    Ok(inspection)
}

/// Decode one complete Douyin PushFrame payload into bounded events.
pub fn decode(frame: &[u8]) -> Result<Vec<DanmakuEvent>> {
    Ok(inspect_frame(frame)?.events)
}

fn parse_response(bytes: &[u8], inspection: &mut FrameInspection) -> Result<()> {
    ensure!(bytes.len() <= MAX_EXPANDED_BYTES, "抖音弹幕响应超出预算");
    let mut reader = ProtoReader::new(bytes);
    let mut fields = 0_usize;
    while let Some((field, wire)) = reader.read_tag()? {
        fields += 1;
        ensure!(fields <= MAX_PROTO_FIELDS, "抖音弹幕响应字段数量超出上限");
        match (field, wire) {
            (1, 2) => {
                inspection.messages = inspection.messages.saturating_add(1);
                ensure!(
                    inspection.messages <= MAX_RESPONSE_MESSAGES,
                    "抖音弹幕响应消息数量超出上限"
                );
                parse_message(reader.read_bytes()?, 1, inspection)?;
            }
            (5, 2) => inspection.internal_ext = Some(limit(reader.read_string()?)),
            _ => reader.skip(wire)?,
        }
        ensure!(
            inspection.events.len() <= MAX_EVENTS_PER_FRAME,
            "抖音单帧弹幕事件数量超出上限"
        );
    }
    Ok(())
}

fn parse_message(raw: &[u8], depth: usize, inspection: &mut FrameInspection) -> Result<()> {
    ensure!(depth <= MAX_PROTO_DEPTH, "抖音 protobuf 嵌套层级超出上限");
    let mut reader = ProtoReader::new(raw);
    let mut method = "";
    let mut payload: Option<&[u8]> = None;
    let mut fields = 0_usize;
    while let Some((field, wire)) = reader.read_tag()? {
        fields += 1;
        ensure!(fields <= MAX_PROTO_FIELDS, "抖音 Message 字段数量超出上限");
        match (field, wire) {
            (1, 2) => method = reader.read_string()?,
            (2, 2) => payload = Some(reader.read_bytes()?),
            _ => reader.skip(wire)?,
        }
    }
    // Whitelist early-drop: real rooms are dominated by entry/commerce messages,
    // exactly as the legacy client observed.
    if method != "WebcastChatMessage" {
        return Ok(());
    }
    if let Some(payload) = payload {
        parse_chat(payload, depth + 1, &mut inspection.events)?;
    }
    Ok(())
}

fn parse_chat(raw: &[u8], depth: usize, events: &mut Vec<DanmakuEvent>) -> Result<()> {
    ensure!(depth <= MAX_PROTO_DEPTH, "抖音 protobuf 嵌套层级超出上限");
    let mut reader = ProtoReader::new(raw);
    let mut content = "";
    let mut user = ChatUser::default();
    let mut fields = 0_usize;
    while let Some((field, wire)) = reader.read_tag()? {
        fields += 1;
        ensure!(
            fields <= MAX_PROTO_FIELDS,
            "抖音 ChatMessage 字段数量超出上限"
        );
        match (field, wire) {
            (2, 2) => user = parse_chat_user(reader.read_bytes()?, depth + 1)?,
            (3, 2) => content = reader.read_string()?,
            _ => reader.skip(wire)?,
        }
    }
    if content.is_empty() {
        return Ok(());
    }
    events.push(DanmakuEvent::Message(DanmakuMessage {
        image_url: None,
        id: String::new(),
        user: limit(&user.nickname),
        text: limit(content),
        color: 0xFFFFFF,
        mode: DanmakuMode::Scroll,
        font_size: 25,
        user_id: user.id,
        user_level: user.level,
        guard_level: 0,
        medal: None,
        image_width: 0,
        image_height: 0,
        image_large: false,
    }));
    ensure!(
        events.len() <= MAX_EVENTS_PER_FRAME,
        "抖音单帧弹幕事件数量超出上限"
    );
    Ok(())
}

#[derive(Debug, Default)]
struct ChatUser {
    id: u64,
    nickname: String,
    level: u16,
}

/// `User { uint64 id = 1; ... string nickname = 3; ... uint32 level = 6;
/// Image avatar_thumb = 9 }`. The avatar is not rendered on danmaku, so only
/// the identity fields the overlay can display are kept.
fn parse_chat_user(raw: &[u8], depth: usize) -> Result<ChatUser> {
    ensure!(depth <= MAX_PROTO_DEPTH, "抖音 protobuf 嵌套层级超出上限");
    let mut reader = ProtoReader::new(raw);
    let mut user = ChatUser::default();
    let mut fields = 0_usize;
    while let Some((field, wire)) = reader.read_tag()? {
        fields += 1;
        ensure!(fields <= MAX_PROTO_FIELDS, "抖音 User 字段数量超出上限");
        match (field, wire) {
            (1, 0) => user.id = reader.read_varint()?,
            (3, 2) => user.nickname = limit(reader.read_string()?),
            (6, 0) => user.level = reader.read_varint()?.min(600) as u16,
            _ => reader.skip(wire)?,
        }
    }
    Ok(user)
}

// ---------------------------------------------------------------------------
// Heartbeat ACK watch and reconnect policy (pure, offline-testable)
// ---------------------------------------------------------------------------

/// The legacy `HeartbeatAckWatch`: a heartbeat reply must arrive within the
/// configured timeout (max 5 s); two consecutive misses end the connection.
#[derive(Debug, Clone)]
pub struct HeartbeatAckWatch {
    timeout_ms: u64,
    pending_since: Option<u64>,
    missed: u32,
}

impl HeartbeatAckWatch {
    pub fn new(timeout_ms: u64) -> Self {
        Self {
            timeout_ms: timeout_ms.clamp(1, HEARTBEAT_ACK_TIMEOUT_MS),
            pending_since: None,
            missed: 0,
        }
    }

    pub fn begin(&mut self, now_ms: u64) {
        self.pending_since = Some(now_ms);
    }

    /// A late reply still proves the connection is alive and clears the streak.
    pub fn acknowledge(&mut self) {
        self.pending_since = None;
        self.missed = 0;
    }

    pub fn should_reconnect(&mut self, now_ms: u64) -> bool {
        if let Some(sent) = self.pending_since {
            if now_ms.saturating_sub(sent) >= self.timeout_ms {
                self.pending_since = None;
                self.missed = self.missed.saturating_add(1);
            }
        }
        self.missed >= 2
    }

    pub fn missed(&self) -> u32 {
        self.missed
    }
}

/// Bounded reconnect backoff: 3 s, 6 s, 9 s, then capped at 30 s (legacy shape).
pub fn reconnect_delay(reconnects: u32) -> Duration {
    Duration::from_secs((3_u64.saturating_mul(u64::from(reconnects.max(1)))).min(30))
}

// ---------------------------------------------------------------------------
// Room page parsing (pure; the HTTP fetch around it is live-only)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoomInfo {
    pub room_id: String,
    pub user_unique_id: String,
    pub ttwid: String,
    /// 2 = live; 4 = ended. Danmaku stays connected either way, like the legacy
    /// client, but callers may surface the state.
    pub status: i64,
    pub title: String,
    pub owner: String,
}

const PACE_MARKER: &str = "self.__pace_f.push([";
const NEXT_MARKER: &str = "self.__next_f.push([";
const USER_UNIQUE_KEY: &str = "user_unique_id";

fn find_flight_marker(text: &str) -> Option<(usize, usize)> {
    let pace = text.find(PACE_MARKER);
    let next = text.find(NEXT_MARKER);
    match (pace, next) {
        (Some(pace), Some(next)) if pace <= next => Some((pace, PACE_MARKER.len())),
        (_, Some(next)) => Some((next, NEXT_MARKER.len())),
        (Some(pace), None) => Some((pace, PACE_MARKER.len())),
        (None, None) => None,
    }
}

fn json_string_end(bytes: &[u8], quote: usize) -> Option<usize> {
    let mut escaped = false;
    let mut cursor = quote.saturating_add(1);
    while let Some(byte) = bytes.get(cursor) {
        if *byte == b'\\' {
            escaped = !escaped;
            cursor += 1;
            continue;
        }
        if *byte == b'"' && !escaped {
            return Some(cursor);
        }
        escaped = false;
        cursor += 1;
    }
    None
}

/// Linear scan for `self.__pace_f.push([<digits>,"<JSON string literal>"])`.
/// The page has two script-ending variants, so no single regex is assumed.
fn flight_literals(page: &str) -> Vec<&str> {
    let bytes = page.as_bytes();
    let mut literals = Vec::new();
    let mut index = 0_usize;
    while index < page.len() && literals.len() < MAX_FLIGHT_PAYLOADS {
        let Some((found, marker_len)) = find_flight_marker(&page[index..]) else {
            break;
        };
        let cursor = index + found + marker_len;
        let Some(offset) = page[cursor..].find('"') else {
            break;
        };
        if offset > 64 {
            index = cursor;
            continue;
        }
        let quote = cursor + offset;
        let head = page[cursor..quote].trim().trim_end_matches(',');
        if head.is_empty() || !head.bytes().all(|byte| byte.is_ascii_digit()) {
            index = cursor;
            continue;
        }
        let Some(end) = json_string_end(bytes, quote) else {
            break;
        };
        literals.push(&page[quote..=end]);
        index = end.saturating_add(1);
    }
    literals
}

fn strip_flight_prefix(payload: &str) -> &str {
    match payload.find(':') {
        Some(index)
            if index > 0
                && payload[..index]
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') =>
        {
            &payload[index + 1..]
        }
        _ => payload,
    }
}

fn parse_flight_payload(literal: &str) -> Option<Value> {
    let payload: String = serde_json::from_str(literal).ok()?;
    if !payload.contains("roomStore") {
        return None;
    }
    serde_json::from_str(strip_flight_prefix(&payload)).ok()
}

fn room_from_value(value: &Value) -> Option<RoomInfo> {
    let entries: Vec<&Value> = match value {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    for entry in entries {
        let Some(room) = entry.pointer("/state/roomStore/roomInfo/room") else {
            continue;
        };
        if !room.is_object() {
            continue;
        }
        let status = room
            .get("status")
            .and_then(|value| {
                value
                    .as_i64()
                    .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
            })
            .unwrap_or(0);
        let room_id = room
            .get("id_str")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                room.get("id").and_then(|value| {
                    value
                        .as_u64()
                        .map(|number| number.to_string())
                        .or_else(|| value.as_str().map(str::to_owned))
                })
            })
            .unwrap_or_default();
        if !valid_numeric_id(&room_id) {
            continue;
        }
        let title = room.get("title").and_then(Value::as_str).unwrap_or("");
        let owner = room
            .get("owner")
            .or_else(|| room.get("anchor"))
            .and_then(|owner| owner.get("nickname"))
            .and_then(Value::as_str)
            .unwrap_or("");
        return Some(RoomInfo {
            room_id,
            user_unique_id: String::new(),
            ttwid: String::new(),
            status,
            title: limit_room_field(title),
            owner: limit_room_field(owner),
        });
    }
    None
}

fn find_user_unique_id(page: &str) -> Option<String> {
    let bytes = page.as_bytes();
    let mut index = 0_usize;
    while index < page.len() {
        let found = page[index..].find(USER_UNIQUE_KEY)?;
        let mut cursor = index + found + USER_UNIQUE_KEY.len();
        while matches!(
            bytes.get(cursor),
            Some(b'"' | b'\\' | b':' | b' ' | b'\t' | b'\r' | b'\n')
        ) {
            cursor += 1;
        }
        let start = cursor;
        while cursor < bytes.len()
            && bytes[cursor].is_ascii_digit()
            && cursor.saturating_sub(start) < 32
        {
            cursor += 1;
        }
        if cursor.saturating_sub(start) >= 6 {
            return Some(page[start..cursor].to_owned());
        }
        index = index + found + USER_UNIQUE_KEY.len();
    }
    None
}

/// Why an anonymous Douyin room-page request/resolve failed, classified so the
/// caller can tell "needs a human credential" from "retry later" from "page
/// changed". Messages never include page content, cookies or account fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomFailure {
    /// HTTP 401/403, an interstitial redirect, or a captcha/verification page: a
    /// human login/verification step is required. The explicit credential import
    /// (`auth import douyin <file>`) is the supported recovery path.
    LoginRequired,
    /// Rate limit / anti-bot signal: 429, an empty (0-byte) JSON response, or a
    /// captcha/verification page. Retrying immediately makes it worse.
    RiskControl,
    /// Page structure changed or no room data at all.
    Malformed,
}

/// Typed wrapper so the reconnect loop can distinguish a human-verification
/// requirement from an ordinary transport failure instead of retrying blindly.
#[derive(Debug, Clone, Copy)]
pub struct DouyinRoomFailure(pub RoomFailure);

impl std::fmt::Display for DouyinRoomFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0.message())
    }
}

impl std::error::Error for DouyinRoomFailure {}

fn failure_error(failure: RoomFailure) -> anyhow::Error {
    anyhow::Error::new(DouyinRoomFailure(failure))
}

impl RoomFailure {
    pub fn message(self) -> &'static str {
        match self {
            RoomFailure::LoginRequired => {
                "抖音要求登录或网页验证；可用 `auth import douyin <cookie 文件>` 显式导入凭据后重试"
            }
            RoomFailure::RiskControl => {
                "抖音返回空响应或空壳页（房间不存在与临时限流页面相同，无法区分）；稍后重试，避免高频请求"
            }
            RoomFailure::Malformed => "抖音房间页格式已变化或未返回房间信息",
        }
    }
}

/// Classify a room-page HTTP response before parsing. Pure and offline-testable.
///
/// Observed 2026-10-29: an anonymous request with a non-browser User-Agent got
/// `HTTP 200` + `Content-Type: application/json` + 0 bytes, while the same
/// request with a browser User-Agent got the 1.2 MB room page — so an empty or
/// JSON-only body here is an anti-bot signal, not a transport failure.
pub fn classify_room_response(
    status: u16,
    content_type: Option<&str>,
    body: &str,
) -> Option<RoomFailure> {
    match status {
        401 | 403 => return Some(RoomFailure::LoginRequired),
        429 => return Some(RoomFailure::RiskControl),
        _ => {}
    }
    if !(200..300).contains(&status) {
        return None;
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Some(RoomFailure::RiskControl);
    }
    let is_json = content_type.is_some_and(|value| value.to_ascii_lowercase().contains("json"));
    if is_json && trimmed.starts_with('{') && trimmed.len() <= 1024 {
        return Some(RoomFailure::RiskControl);
    }
    None
}

/// `true` for a real captcha/intermediate page. A normal room page is ~1 MB and
/// merely mentions "verify"/"captcha" inside script names (e.g. `verifyFp`), so a
/// size guard plus the known page markers is required before claiming a human
/// verification wall. The marker set comes from the legacy decision record
/// docs/decisions/002 (6.3 KB 「验证码中间页」, `sec_sdk_build/.../captcha/index.js`).
fn is_captcha_page(page: &str) -> bool {
    let small = page.len() < 64 * 1024;
    page.contains("验证码中间页")
        || page.contains("sec_sdk_build")
        || (small
            && (page.contains("captcha") || page.contains("verify") || page.contains("验证码")))
}

/// `true` when a flight payload carries `state.roomStore.roomInfo` but no room
/// object: the "empty shell" template Douyin returns both for a nonexistent room
/// and for a short-term rate limit (the legacy client could not tell them apart
/// either, see docs/decisions/010).
fn is_room_shell(value: &Value) -> bool {
    let entries: Vec<&Value> = match value {
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    entries.iter().any(|entry| {
        entry
            .pointer("/state/roomStore/roomInfo")
            .is_some_and(|info| {
                info.is_object() && info.get("room").is_none_or(|room| !room.is_object())
            })
    })
}

/// Pure, classified parser for `https://live.douyin.com/<web_rid>` pages.
pub fn parse_room_page_classified(page: &str) -> std::result::Result<RoomInfo, RoomFailure> {
    if page.is_empty() || page.len() > MAX_ROOM_PAGE_BYTES {
        return Err(RoomFailure::Malformed);
    }
    let mut shell = false;
    for literal in flight_literals(page) {
        let Some(value) = parse_flight_payload(literal) else {
            continue;
        };
        if let Some(mut info) = room_from_value(&value) {
            info.user_unique_id = find_user_unique_id(page).unwrap_or_default();
            return Ok(info);
        }
        shell = shell || is_room_shell(&value);
    }
    if is_captcha_page(page) {
        // A captcha/interstitial page needs a human step (browser verification or
        // an imported credential), so it is classified with login-required rather
        // than the retryable empty-shell rate limit.
        return Err(RoomFailure::LoginRequired);
    }
    // A roomStore shell is the same template for "room does not exist" and
    // "temporarily rate limited"; classify it as retryable risk control instead
    // of claiming the page format changed.
    if shell {
        return Err(RoomFailure::RiskControl);
    }
    Err(RoomFailure::Malformed)
}

/// Pure parser for `https://live.douyin.com/<web_rid>` pages. Extracts the real
/// room id, live status, title/owner and `user_unique_id`; `ttwid` comes from the
/// response `Set-Cookie` header (see `cookie_value`).
pub fn parse_room_page(page: &str) -> Result<RoomInfo> {
    parse_room_page_classified(page).map_err(|failure| anyhow!("{}", failure.message()))
}

/// Extract a named cookie value from one `Set-Cookie` header value.
pub fn cookie_value(header: &str, name: &str) -> Option<String> {
    for part in header.split(';') {
        let part = part.trim();
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case(name) {
            let value = value.trim();
            if value.len() > 4096 {
                return None;
            }
            return Some(value.to_owned());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Randomness (uuid-based; no extra dependency)
// ---------------------------------------------------------------------------

fn random_bytes() -> [u8; 16] {
    let uuid = uuid::Uuid::new_v4();
    *uuid.as_bytes()
}

fn random_u32() -> u32 {
    let bytes = random_bytes();
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn random_hex(length: usize) -> String {
    let bytes = random_bytes();
    let mut text = String::with_capacity(32);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text.truncate(length);
    text
}

fn random_anonymous_id() -> String {
    let bytes = random_bytes();
    let mut id = String::from("76");
    for index in 0..17 {
        id.push(char::from(b'0' + bytes[index % bytes.len()] % 10));
    }
    id
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Live room resolution and WebSocket lifecycle (not covered by offline tests)
// ---------------------------------------------------------------------------

fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(8))
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .https_only(true)
        .gzip(true)
        .deflate(true)
        .build()
        .map_err(|_| anyhow!("抖音 HTTP 客户端初始化失败"))
}

fn safe_redirect(current: &str, location: &str) -> Result<String> {
    let base = url::Url::parse(current)?;
    let next = base.join(location)?;
    ensure!(
        next.scheme() == "https",
        "抖音房间页重定向到了非 HTTPS 地址"
    );
    ensure!(
        next.host_str() == Some(DOUYIN_HOST),
        "抖音房间页被重定向到非直播页（可能需要登录或网页验证）"
    );
    ensure!(
        next.username().is_empty() && next.password().is_none(),
        "抖音房间页重定向包含凭据"
    );
    Ok(next.to_string())
}

fn resolve_room(
    target: &Target,
    cancellation: &Cancellation,
    source: Option<&dyn CookieSource>,
) -> Result<(RoomInfo, DouyinSessionCookies)> {
    ensure!(
        matches!(target.platform, Platform::Douyin),
        "该弹幕实现仅支持抖音"
    );
    target.validate()?;
    if cancellation.is_cancelled() {
        bail!("已取消");
    }
    let client = http_client()?;
    let nonce = random_hex(21);
    let mut url = format!("https://{DOUYIN_HOST}/{}", target.room);
    let mut ttwid = String::new();
    let mut html = String::new();
    let mut cookies = DouyinSessionCookies::default();
    for _ in 0..=MAX_REDIRECTS {
        if cancellation.is_cancelled() {
            bail!("已取消");
        }
        let mut request = client
            .get(&url)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("Accept-Language", "zh-CN,zh;q=0.9");
        if let Some(cookie) = cookies.header_for(source, &url, Some(&nonce), now_seconds())? {
            request = request.header("Cookie", cookie);
        }
        let response = request
            .send()
            .map_err(|_| anyhow!("抖音房间页请求失败（网络不通或被拦截）"))?;
        // Response cookies keep their Domain/HostOnly/Path/Secure/expiry.
        // A host-only live-page ttwid is never moved to a WebSocket subdomain.
        for header in response.headers().get_all("set-cookie") {
            if let Ok(text) = header.to_str() {
                cookies.absorb(&url, text, now_seconds())?;
                if let Some(value) = cookie_value(text, "ttwid") {
                    ttwid = value;
                }
            }
        }
        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if status.is_redirection() {
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| anyhow!("抖音房间页重定向缺少 Location"))?;
            url = safe_redirect(&url, location)?;
            continue;
        }
        if !status.is_success() {
            // 401/403 means a human login/verification step; 429 is a rate limit.
            // Both are classified so retry loops surface the right message.
            if let Some(failure) = classify_room_response(status.as_u16(), None, "") {
                return Err(failure_error(failure));
            }
            bail!("抖音房间页返回 HTTP {}", status.as_u16());
        }
        let mut body = Vec::new();
        response
            .take((MAX_ROOM_PAGE_BYTES + 1) as u64)
            .read_to_end(&mut body)?;
        ensure!(body.len() <= MAX_ROOM_PAGE_BYTES, "抖音房间页超过大小上限");
        html = String::from_utf8_lossy(&body).into_owned();
        // Observed anti-bot shape (2026-10-29): HTTP 200 + application/json + an
        // empty body instead of the room page. Classify it instead of parsing.
        if let Some(failure) =
            classify_room_response(status.as_u16(), content_type.as_deref(), &html)
        {
            return Err(failure_error(failure));
        }
        break;
    }
    ensure!(!html.is_empty(), "抖音房间页请求失败（重定向次数过多）");
    let mut info = parse_room_page_classified(&html).map_err(failure_error)?;
    info.ttwid = ttwid;
    if info.user_unique_id.is_empty() {
        info.user_unique_id = random_anonymous_id();
    }
    Ok((info, cookies))
}

fn tcp_connect(host: &str, port: u16, cancellation: &Cancellation) -> Result<TcpStream> {
    let addresses: Vec<SocketAddr> = (host, port).to_socket_addrs()?.take(8).collect();
    for address in addresses {
        if cancellation.is_cancelled() {
            bail!("已取消");
        }
        // Only known platform endpoints reach this function; local TUN fake-IP
        // answers are allowed because TLS still verifies the endpoint hostname.
        if !crate::danmaku::is_allowed_danmaku_address(address.ip()) {
            continue;
        }
        if let Ok(stream) = TcpStream::connect_timeout(&address, Duration::from_secs(5)) {
            stream.set_nodelay(true)?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            stream.set_write_timeout(Some(Duration::from_secs(5)))?;
            return Ok(stream);
        }
    }
    bail!("抖音弹幕节点连接失败")
}

type DouyinSocket = WebSocket<MaybeTlsStream<TcpStream>>;

fn set_socket_read_timeout(socket: &mut DouyinSocket, timeout: Duration) -> Result<()> {
    let tcp = match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => stream,
        MaybeTlsStream::NativeTls(stream) => stream.get_mut(),
        _ => bail!("不支持的 WebSocket 传输层"),
    };
    tcp.set_read_timeout(Some(timeout))?;
    Ok(())
}

/// Builds the WebSocket Cookie header. The webcast endpoint lives on a
/// `*.douyin.com` subdomain while the room page's ttwid is host-only, so it is
/// added explicitly here (the legacy client passes a ttwid cookie scoped to
/// `.douyin.com`). Without it the server accepts the signed handshake but never
/// pushes frames. The value is validated as one cookie token.
fn webcast_cookie_header(
    cookies: &DouyinSessionCookies,
    source: Option<&dyn CookieSource>,
    url: &str,
    ttwid: &str,
    now: u64,
) -> Result<Option<String>> {
    let mut header = cookies
        .header_for(source, url, None, now)?
        .unwrap_or_default();
    let ttwid = ttwid.trim();
    // Validate the caller-supplied value unconditionally: a malformed ttwid must
    // be rejected even when another cookie already supplies one.
    if !ttwid.is_empty() {
        ensure!(
            ttwid.len() <= 4096
                && ttwid
                    .bytes()
                    .all(|byte| (0x21..=0x7e).contains(&byte) && byte != b';'),
            "抖音 ttwid 格式无效"
        );
    }
    let already_present = header
        .split(';')
        .any(|part| part.trim_start().starts_with("ttwid="));
    if !ttwid.is_empty() && !already_present {
        if !header.is_empty() {
            header.push_str("; ");
        }
        header.push_str("ttwid=");
        header.push_str(ttwid);
    }
    Ok((!header.is_empty()).then_some(header))
}

fn open_socket(
    host: &str,
    url: &str,
    cookie: Option<&str>,
    cancellation: &Cancellation,
) -> Result<DouyinSocket> {
    let stream = tcp_connect(host, 443, cancellation)?;
    let mut request = url
        .into_client_request()
        .map_err(|_| anyhow!("抖音 WebSocket 地址无效"))?;
    if let Some(cookie) = cookie {
        let value = cookie
            .parse()
            .map_err(|_| anyhow!("抖音 WebSocket Cookie 无效"))?;
        request.headers_mut().insert("Cookie", value);
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_WIRE_BYTES))
        .max_frame_size(Some(MAX_WIRE_BYTES));
    let (mut socket, _) = tungstenite::client_tls_with_config(request, stream, Some(config), None)
        .map_err(|_| anyhow!("抖音 WebSocket 握手失败（签名可能被拒或网络受阻）"))?;
    set_socket_read_timeout(&mut socket, Duration::from_millis(500))?;
    Ok(socket)
}

fn send_status(sender: &SyncSender<DanmakuEvent>, text: impl Into<String>) -> bool {
    matches!(
        emit(sender, DanmakuEvent::Status(text.into())),
        Delivery::Closed
    )
}

fn cancellable_wait(cancellation: &Cancellation, duration: Duration) {
    let deadline = Instant::now() + duration;
    while !cancellation.is_cancelled() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        thread::sleep(remaining.min(Duration::from_millis(50)));
    }
}

fn read_loop(
    mut socket: DouyinSocket,
    sender: &SyncSender<DanmakuEvent>,
    cancellation: &Cancellation,
) -> Result<()> {
    let started = Instant::now();
    let mut watch = HeartbeatAckWatch::new(HEARTBEAT_ACK_TIMEOUT_MS);
    let mut last_heartbeat = Instant::now();
    let mut saw_frame = false;
    let mut silence_notice = false;
    while !cancellation.is_cancelled() {
        let now = Instant::now();
        let elapsed_ms = now.duration_since(started).as_millis() as u64;
        // A signed handshake can succeed while the platform withholds pushes
        // (anonymous risk control or a pending web verification). Say that
        // precisely once instead of looking like an ordinary reconnect loop.
        if !saw_frame && !silence_notice && now.duration_since(started).as_secs() >= 12 {
            silence_notice = true;
            if send_status(
                sender,
                "抖音：连接已建立但 12 秒内没有收到服务端推送；匿名接收可能被风控，若平台要求登录/网页验证请先在程序内登录抖音",
            ) {
                return Ok(());
            }
        }
        if now.duration_since(last_heartbeat) >= HEARTBEAT_INTERVAL {
            socket.send(Message::Binary(heartbeat_packet().into()))?;
            watch.begin(elapsed_ms);
            last_heartbeat = now;
        }
        if watch.should_reconnect(elapsed_ms) {
            bail!("抖音弹幕连续两次心跳没有回包");
        }
        match socket.read() {
            Ok(Message::Binary(bytes)) => match inspect_frame(&bytes) {
                Ok(inspection) => {
                    if !saw_frame {
                        saw_frame = true;
                        if send_status(sender, "抖音：已收到服务端推送，弹幕开始") {
                            return Ok(());
                        }
                    }
                    if inspection.heartbeat {
                        watch.acknowledge();
                    }
                    if let Some(internal_ext) = inspection.internal_ext
                        && inspection.log_id != 0
                    {
                        socket.send(Message::Binary(
                            ack_packet(inspection.log_id, &internal_ext).into(),
                        ))?;
                    }
                    for event in inspection.events {
                        if emit(sender, event) == Delivery::Closed {
                            return Ok(());
                        }
                    }
                }
                Err(_) => {
                    // One malformed frame must not kill the connection; the legacy
                    // client logs and continues in the same situation.
                    if send_status(sender, "抖音弹幕解析出错，已跳过该帧") {
                        return Ok(());
                    }
                }
            },
            Ok(Message::Close(_)) => bail!("抖音弹幕服务端关闭了连接"),
            Ok(Message::Ping(_)) => {
                socket.flush()?;
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => bail!("抖音 WebSocket 读取失败或连接已关闭"),
        }
    }
    let _ = socket.close(None);
    Ok(())
}

fn run_once(
    target: &Target,
    sender: &SyncSender<DanmakuEvent>,
    cancellation: &Cancellation,
    source: Option<&dyn CookieSource>,
) -> Result<()> {
    let (room, cookies) = resolve_room(target, cancellation, source)?;
    if cancellation.is_cancelled() {
        return Ok(());
    }
    let stub = join_stub(&room.room_id, &room.user_unique_id);
    let signature = sign(&md5_hex(stub.as_bytes()))?;
    if send_status(sender, format!("抖音：正在连接 {} …", target.room)) {
        return Ok(());
    }
    let mut last_error = None;
    for host in WEBCAST_HOSTS {
        if cancellation.is_cancelled() {
            return Ok(());
        }
        let request = JoinRequest {
            host,
            room_id: &room.room_id,
            user_unique_id: &room.user_unique_id,
            signature: &signature,
            now_ms: epoch_millis(),
            cursor_random: random_u32() & 0xFFFF,
            wrds_random: random_u32(),
        };
        let url = match build_join_url(&request) {
            Ok(url) => url,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        let cookie = webcast_cookie_header(&cookies, source, &url, &room.ttwid, now_seconds())?;
        match open_socket(host, &url, cookie.as_deref(), cancellation) {
            Ok(socket) => {
                let status = if room.status == 2 {
                    format!(
                        "抖音弹幕已连接{}",
                        if room.owner.is_empty() {
                            String::new()
                        } else {
                            format!(" · {}", room.owner)
                        }
                    )
                } else {
                    "抖音房间未开播，仍在等待弹幕".to_owned()
                };
                if send_status(sender, status) {
                    return Ok(());
                }
                return read_loop(socket, sender, cancellation);
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("抖音弹幕节点都不可用")))
}

#[cfg(test)]
mod cookie_tests {
    use super::*;

    #[test]
    fn webcast_cookie_adds_the_room_ttwid_without_duplicating_it() {
        let cookies = DouyinSessionCookies::default();
        let header = webcast_cookie_header(
            &cookies,
            None,
            "https://webcast100-ws-web-lq.douyin.com/webcast/im/push/v2/",
            "synthetic-ttwid",
            1,
        )
        .unwrap()
        .expect("header");
        assert_eq!(header, "ttwid=synthetic-ttwid");

        // No ttwid and no scoped cookies means no Cookie header at all.
        assert!(
            webcast_cookie_header(&cookies, None, "https://live.douyin.com/1", "", 1)
                .unwrap()
                .is_none()
        );

        // A host-scoped ttwid is not duplicated.
        let mut cookies = DouyinSessionCookies::default();
        cookies
            .absorb("https://live.douyin.com/1", "ttwid=old; Path=/", 1)
            .unwrap();
        let header = webcast_cookie_header(&cookies, None, "https://live.douyin.com/1", "new", 1)
            .unwrap()
            .expect("header");
        assert_eq!(header.matches("ttwid=").count(), 1);
        assert!(header.contains("ttwid=old"));

        // Control characters and separators never reach the header.
        assert!(
            webcast_cookie_header(&cookies, None, "https://live.douyin.com/1", "bad; value", 1)
                .is_err()
        );
    }
}

/// Open the Douyin webcast WebSocket with signature/ACK/heartbeat lifecycle.
///
/// Blocking worker function, matching `danmaku::connect`: it polls
/// `Cancellation` between reads (500 ms socket timeout) and never waits for a
/// slow event consumer. Reconnects are finite (`MAX_RECONNECTS`); after that a
/// final error is returned. The default offline suite never contacts the
/// platform; `tests/douyin_live_acceptance.rs` documents the manually verified
/// anonymous handshake (opt-in, ignored). Browser-based risk verification remains
/// outside this receiver.
pub fn connect(
    target: &Target,
    sender: SyncSender<DanmakuEvent>,
    cancellation: Cancellation,
) -> Result<()> {
    connect_with_cookie_source(target, sender, cancellation, None)
}

pub fn connect_authenticated(
    target: &Target,
    sender: SyncSender<DanmakuEvent>,
    cancellation: Cancellation,
    vault: &CookieVault,
) -> Result<()> {
    connect_with_cookie_source(target, sender, cancellation, Some(Arc::new(vault.clone())))
}

/// Explicit scoped source; no legacy-profile or credential discovery occurs.
pub fn connect_with_cookie_source(
    target: &Target,
    sender: SyncSender<DanmakuEvent>,
    cancellation: Cancellation,
    source: Option<Arc<dyn CookieSource>>,
) -> Result<()> {
    ensure!(
        matches!(target.platform, Platform::Douyin),
        "该弹幕实现仅支持抖音"
    );
    target.validate()?;
    let mut reconnects = 0_u32;
    loop {
        if cancellation.is_cancelled() {
            return Ok(());
        }
        match run_once(target, &sender, &cancellation, source.as_deref()) {
            Ok(()) => return Ok(()),
            Err(error) => {
                if cancellation.is_cancelled() {
                    return Ok(());
                }
                if let Some(failure) = error.downcast_ref::<DouyinRoomFailure>() {
                    // A human login / verification requirement cannot be fixed by
                    // reconnecting; surface it once and stop instead of looping.
                    if matches!(failure.0, RoomFailure::LoginRequired) {
                        let _ = send_status(
                            &sender,
                            format!("抖音弹幕无法连接：{}", failure.0.message()),
                        );
                        return Err(error.context("抖音弹幕需要人工登录或网页验证"));
                    }
                    if send_status(
                        &sender,
                        format!("抖音弹幕暂不可用：{}", failure.0.message()),
                    ) {
                        return Ok(());
                    }
                }
                reconnects = reconnects.saturating_add(1);
                if reconnects > MAX_RECONNECTS {
                    let _ = send_status(
                        &sender,
                        format!("抖音弹幕重连 {MAX_RECONNECTS} 次后仍失败，停止重试"),
                    );
                    return Err(
                        error.context(format!("抖音弹幕重连 {MAX_RECONNECTS} 次后仍然失败"))
                    );
                }
                let delay = reconnect_delay(reconnects);
                if send_status(
                    &sender,
                    format!(
                        "抖音弹幕连接中断，{} 秒后重连（{reconnects}/{MAX_RECONNECTS}）",
                        delay.as_secs()
                    ),
                ) {
                    return Ok(());
                }
                cancellable_wait(&cancellation, delay);
            }
        }
    }
}
