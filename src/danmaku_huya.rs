//! 虎牙 Web 弹幕客户端（迁移工作流 danmaku-huya）。
//!
//! 协议来源：旧 C# 工程 src/Danmaku/danmaku-9-huya.cs 的实测协议观测
//! （决策记录 docs/decisions/003-虎牙-tars.md、004-弹幕连接与窗口恢复.md），
//! 以及该文件内 CommandRegisterGroup / CommandHeartbeat / ParseMessageNotice 的字段布局。
//! 旧实现与本工程同属 live-hub（GPL-3.0，已保留原 LICENSE），本模块是协议行为迁移，
//! 不是对第三方协议库的移植。
//!
//! 已冻结的约定（旧实现实测结论，不要“顺手改成常识写法”）：
//! - 弹幕走 wss://cdnws.api.huya.com：无路径、无 query、无子协议、无签名、完全匿名。
//!   wsSecret 只属于虎牙推流地址，与弹幕无关。
//! - 真正的订阅包是 cmd 16（RegisterGroupReq）；只发 cmd 1 服务器不报错但也不推送。
//! - 订阅组名必须用房间的 lChannelId，不能用主播 yyuid；用错时服务器静默回空组。
//! - 推送有 cmd 7（V1）与 cmd 22（V2）两条通道，只认 cmd 7 会漏消息。
//! - 心跳是 cmd 3 的 WUP 请求（onlineui/OnUserHeartBeat，60 秒一次），服务端回 cmd 4 确认。
//! - 载荷是腾讯 Tars/JCE 二进制，没有 varint：写入器使用类型窄化
//!   （值落在 int16 范围就写 INT16、落在 int8 就写 INT8、0 只写 ZERO 头）；
//!   BYTE_ARRAY 是两个头字节（BYTES 头 + INT8 tag0 占位 + INT32 长度），只读一个会整体错位。
//! - 聊天正文在 uri 1400 的 MessageNotice 里：tag0=tUserInfo{tag2=sNickName, tag5=iNobleLevel}、
//!   tag3=sContent、tag5=ContentFormat{tag0=iFontColor}。读完 tUserInfo 必须吃掉
//!   STRUCTEND，否则外层 tag3 会先命中 tUserInfo 内部的 tag3（iGender），正文永远为空。
//! - 虎牙没有 SC 等价物：官方“超级弹幕”内测且仅限受邀主播，141 个结构体 IDL 里没有
//!   付费留言消息类型。旧实现刻意不做“大额礼物升格成 SC”的近似，本模块同样只解析聊天；
//!   礼物（uri 6501）、进场（6110/6111）、在线数（8006）等通道保持忽略。
//!
//! 公共契约（stub 起就冻结，danmaku::connect / danmaku::decode_frame 会转接过来）：
//! decode(payload) 解码一条 WebSocket 二进制消息（外层 Tars 命令包）；
//! connect(target, sender, cancellation) 在调用方线程上阻塞运行，直到取消、UI 通道关闭
//! 或（有界的）重连次数用尽。所有长度/深度/元素数均有上限，畸形输入返回 Err 而不是 panic。

use crate::core::{Platform, Target};
use crate::danmaku::{
    Cancellation, DanmakuEvent, DanmakuMessage, DanmakuMode, Delivery, MAX_EVENTS_PER_FRAME,
    MAX_TEXT_CHARS, MAX_WIRE_BYTES, emit,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::{
    io::{ErrorKind, Read},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    sync::mpsc::SyncSender,
    thread,
    time::{Duration, Instant},
};
use tungstenite::{
    Message, WebSocket, client::IntoClientRequest, protocol::WebSocketConfig,
    stream::MaybeTlsStream,
};

// ------------------------------------------------------------------ 协议常量

/// 匿名弹幕 WebSocket。保留末尾 /：服务端只接受 GET / 请求行。
const WS_URL: &str = "wss://cdnws.api.huya.com/";
const WS_HOST: &str = "cdnws.api.huya.com";
/// 旧实现 HeartbeatSeconds = 60：订阅后立即发一次，之后每 60 秒一次。
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);
/// 旧实现 HeartbeatAckWatch.TimeoutMs = 15000：发出心跳后最多等这么久。
const HEARTBEAT_ACK_TIMEOUT: Duration = Duration::from_secs(15);
/// 旧实现“连续两次缺回包才主动重连”。
const MAX_MISSED_HEARTBEATS: u32 = 2;
/// 读超时 <= 500ms：安静房间也要能及时响应 Cancellation。
const READ_POLL: Duration = Duration::from_millis(500);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// 断线重连上限（旧实现无限重连；本工程要求有界，最多 3 次、退避 <= 2 秒）。
const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY_CAP: Duration = Duration::from_secs(2);
/// 同一连接持续解析失败的上限：超过即认为协议已经错位，交给有界重连。
const MAX_CONSECUTIVE_PARSE_ERRORS: u32 = 8;

/// Tars 容器元素数上限（防止畸形长度触发长时间循环）。
const MAX_ELEMENTS: usize = 4096;
/// Tars 嵌套深度上限（旧实现 MaxDepth = 64）。
const MAX_DEPTH: usize = 64;
/// 本模块主动解析的字符串字节上限；超长字符串按畸形输入拒绝。
const MAX_STRING_BYTES: usize = 64 * 1024;
/// 房间页/缓存接口响应上限（虎牙房间页含大段脚本，留 2 MiB）。
const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;
/// Bound on room-page redirects; the mobile page redirects to the canonical
/// www page, which is the one carrying the ids.
const MAX_PAGE_REDIRECTS: usize = 3;
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/130.0.0.0 Safari/537.36";

/// 外层命令类型（Tars 命令包 tag0 = iCmdType）。
const CMD_HEARTBEAT_ACK: i64 = 4;
const CMD_PUSH_V1: i64 = 7;
const CMD_REGISTER_GROUP_RSP: i64 = 17;
const CMD_PUSH_V2: i64 = 22;
const CMD_REGISTER_GROUP: i32 = 16;
const CMD_HEARTBEAT: i32 = 3;
/// 唯一承载聊天正文的 uri（MessageNotice）。
const URI_MESSAGE_NOTICE: i64 = 1400;

// Same platform mappings as the legacy client: Huya's default web package,
// followed by the public CDN overrides observed in the official chat view.
// Only URLs are stored; platform artwork is downloaded at runtime.
const DEFAULT_EMOTICONS: &[(&str, &str)] = &[
    ("大笑", "dx"),
    ("送花", "sh"),
    ("偷笑", "tx"),
    ("大哭", "dk"),
    ("嘿哈", "hh"),
    ("666", "66"),
    ("感动", "gd"),
    ("疑问", "yw"),
    ("喜欢", "xh"),
    ("奸笑", "jx"),
    ("赞", "zan"),
    ("可爱", "ka"),
    ("傲慢", "am"),
    ("开心", "kx"),
    ("拜拜", "88"),
    ("害羞", "hx"),
    ("衰", "zs"),
    ("吐血", "pu"),
    ("嘴馋", "zc"),
    ("生气", "sq"),
    ("扶额", "fe"),
    ("闭嘴", "bz"),
    ("枯萎", "kw"),
    ("嘘", "xu"),
    ("笑哭", "xk"),
    ("流汗", "lh"),
    ("不看", "bk"),
    ("哈欠", "hq"),
    ("调皮", "tp"),
    ("鬼脸", "gl"),
    ("戳脸", "cl"),
    ("大哥", "dg"),
    ("困", "kun"),
    ("拥抱", "yb"),
    ("猪头", "zt"),
    ("骷髅", "kl"),
    ("臭臭", "cc"),
    ("心动", "xd"),
    ("刀", "dao"),
];
const EMOTICON_OVERRIDES: &[(&str, &str)] = &[
    (
        "满头问号",
        "https://fileserver.cdn.huya.com/web_admin_material_zip_url/92ad65721cac4f79beae2ae9b3d58d09/expressconfig/steam_3.png",
    ),
    (
        "吐血",
        "https://cdnfile2.msstatic.com/cdnfile/material_manage/web_base_material_16141735632789_pic.png",
    ),
    (
        "偷笑",
        "https://cdnfile2.msstatic.com/cdnfile/material_manage/web_base_material_15790913779295_pic.png",
    ),
    (
        "大哥",
        "https://cdnfile2.msstatic.com/cdnfile/material_manage/web_base_material_16141742503895_pic.png",
    ),
    (
        "送花",
        "https://cdnfile2.msstatic.com/cdnfile/material_manage/web_base_material_16141715844572_pic.png",
    ),
];

/// Credential-free fallback package, available before room lookup or socket I/O.
/// Unknown/server-only packages retain their original text; never guess a URL.
pub fn emoticon_dictionary() -> crate::danmaku::rich::EmoticonDictionary {
    let mut dictionary = crate::danmaku::rich::EmoticonDictionary::new();
    for (name, file) in DEFAULT_EMOTICONS {
        dictionary.add(
            name,
            &format!("https://a.msstatic.com/huya/main/emot_png/{file}.png"),
        );
    }
    for (name, url) in EMOTICON_OVERRIDES {
        dictionary.add(name, url);
    }
    dictionary
}

// ------------------------------------------------------------------ Tars/JCE 子集

/// Tars 类型编号，与旧实现 TarsType 一致。
mod tars {
    pub const INT8: u8 = 0;
    pub const INT16: u8 = 1;
    pub const INT32: u8 = 2;
    pub const INT64: u8 = 3;
    pub const FLOAT: u8 = 4;
    pub const DOUBLE: u8 = 5;
    pub const STRING1: u8 = 6;
    pub const STRING4: u8 = 7;
    pub const MAP: u8 = 8;
    pub const LIST: u8 = 9;
    pub const STRUCT_BEGIN: u8 = 10;
    pub const STRUCT_END: u8 = 11;
    pub const ZERO: u8 = 12;
    pub const BYTES: u8 = 13;
}

/// 最小 Tars 写入器，只保留虎牙命令包需要的类型。
///
/// 与旧实现逐条对齐，尤其是类型窄化：这是接入真实服务器时用实测报文核对过的行为，
/// 不能换成“统一写 INT32”的直觉写法。
#[derive(Default)]
struct TarsWriter {
    buffer: Vec<u8>,
}

impl TarsWriter {
    fn new() -> Self {
        Self {
            buffer: Vec::with_capacity(256),
        }
    }

    fn finish(self) -> Vec<u8> {
        self.buffer
    }

    fn raw(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }

    /// tag < 15 用单字节头（高 4 位 tag、低 4 位类型）；否则 0xF0|type + tag 字节。
    fn head(&mut self, tag: u8, kind: u8) {
        if tag < 15 {
            self.buffer.push((tag << 4) | (kind & 0x0F));
        } else {
            self.buffer.push(0xF0 | (kind & 0x0F));
            self.buffer.push(tag);
        }
    }

    fn i8(&mut self, tag: u8, value: i8) {
        if value == 0 {
            self.head(tag, tars::ZERO);
        } else {
            self.head(tag, tars::INT8);
            self.buffer.push(value as u8);
        }
    }

    fn i16(&mut self, tag: u8, value: i16) {
        if (-128..=127).contains(&value) {
            self.i8(tag, value as i8);
        } else {
            self.head(tag, tars::INT16);
            self.raw(&value.to_be_bytes());
        }
    }

    fn i32(&mut self, tag: u8, value: i32) {
        if (-32768..=32767).contains(&value) {
            self.i16(tag, value as i16);
        } else {
            self.head(tag, tars::INT32);
            self.raw(&value.to_be_bytes());
        }
    }

    fn i64(&mut self, tag: u8, value: i64) {
        if (i64::from(i32::MIN)..=i64::from(i32::MAX)).contains(&value) {
            self.i32(tag, value as i32);
        } else {
            self.head(tag, tars::INT64);
            self.raw(&value.to_be_bytes());
        }
    }

    fn str(&mut self, tag: u8, value: &str) -> Result<()> {
        let bytes = value.as_bytes();
        ensure!(
            bytes.len() <= MAX_STRING_BYTES,
            "虎牙 Tars 字符串超过编码上限"
        );
        if bytes.len() <= 255 {
            self.head(tag, tars::STRING1);
            self.buffer.push(bytes.len() as u8);
        } else {
            self.head(tag, tars::STRING4);
            self.raw(&(bytes.len() as u32).to_be_bytes());
        }
        self.raw(bytes);
        Ok(())
    }

    /// BYTES = BYTES 头 + INT8 tag0 占位 + INT32 长度 + 数据（两个头字节，旧实现已实测）。
    fn bytes(&mut self, tag: u8, data: &[u8]) -> Result<()> {
        ensure!(
            data.len() <= MAX_WIRE_BYTES,
            "虎牙 Tars 字节数组超过编码上限"
        );
        self.head(tag, tars::BYTES);
        self.head(0, tars::INT8);
        self.i32(
            0,
            i32::try_from(data.len()).context("虎牙 Tars 字节数组长度溢出")?,
        );
        self.raw(data);
        Ok(())
    }

    fn struct_begin(&mut self, tag: u8) {
        self.head(tag, tars::STRUCT_BEGIN);
    }

    fn struct_end(&mut self) {
        self.head(0, tars::STRUCT_END);
    }

    fn list_head(&mut self, tag: u8, count: i32) {
        self.head(tag, tars::LIST);
        self.i32(0, count);
    }

    fn map_head(&mut self, tag: u8, count: i32) {
        self.head(tag, tars::MAP);
        self.i32(0, count);
    }

    /// Float 仅用于编解码往返测试；虎牙聊天字段目前不使用浮点。
    #[allow(dead_code)]
    fn float(&mut self, tag: u8, value: f32) {
        self.head(tag, tars::FLOAT);
        self.raw(&value.to_bits().to_be_bytes());
    }

    /// Double 仅用于编解码往返测试。
    #[allow(dead_code)]
    fn double(&mut self, tag: u8, value: f64) {
        self.head(tag, tars::DOUBLE);
        self.raw(&value.to_bits().to_be_bytes());
    }
}

/// 有界 Tars 读取器：所有读取先校验剩余字节，畸形输入返回 Err。
///
/// 与旧实现相同，字段按 tag 升序；遇到未知类型不猜测长度，直接报错。
struct TarsReader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> TarsReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn at_end(&self) -> bool {
        self.position >= self.data.len()
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.position)
    }

    fn require(&self, count: usize) -> Result<()> {
        ensure!(
            count <= self.remaining(),
            "虎牙 Tars 报文被截断：需要 {count} 字节，仅剩 {} 字节",
            self.remaining()
        );
        Ok(())
    }

    fn advance(&mut self, count: usize) -> Result<()> {
        self.require(count)?;
        self.position += count;
        Ok(())
    }

    /// 读一个字段头，返回（类型, tag）；tag 为 15 时再读一个扩展 tag 字节。
    fn head(&mut self) -> Result<(u8, u8)> {
        self.require(1)?;
        let head = self.data[self.position];
        self.position += 1;
        let mut tag = head >> 4;
        if tag == 15 {
            self.require(1)?;
            tag = self.data[self.position];
            self.position += 1;
        }
        Ok((head & 0x0F, tag))
    }

    fn peek_head(&self) -> Result<(u8, u8)> {
        let mut probe = TarsReader {
            data: self.data,
            position: self.position,
        };
        probe.head()
    }

    fn integer(&mut self, kind: u8) -> Result<Option<i64>> {
        let size = match kind {
            tars::ZERO => return Ok(Some(0)),
            tars::INT8 => 1,
            tars::INT16 => 2,
            tars::INT32 => 4,
            tars::INT64 => 8,
            _ => return Ok(None),
        };
        self.require(size)?;
        let mut value: i64 = 0;
        for _ in 0..size {
            value = (value << 8) | i64::from(self.data[self.position]);
            self.position += 1;
        }
        let value = match size {
            1 => i64::from(value as i8),
            2 => i64::from(value as i16),
            4 => i64::from(value as i32),
            _ => value,
        };
        Ok(Some(value))
    }

    /// 读容器长度头：必须是 tag0 的整数（Zero/Int8/Int16/Int32），且不超过 limit。
    fn count(&mut self, limit: usize) -> Result<usize> {
        let (kind, tag) = self.head()?;
        ensure!(
            tag == 0 && matches!(kind, tars::ZERO | tars::INT8 | tars::INT16 | tars::INT32),
            "虎牙 Tars 容器/字节长度头无效"
        );
        let value = self
            .integer(kind)?
            .ok_or_else(|| anyhow!("虎牙 Tars 缺少容器长度"))?;
        ensure!(value >= 0, "虎牙 Tars 容器长度为负");
        let value = usize::try_from(value).context("虎牙 Tars 容器长度超出范围")?;
        ensure!(
            value <= limit,
            "虎牙 Tars 容器长度 {value} 超过上限 {limit}"
        );
        Ok(value)
    }

    /// 跳过当前值（类型已从字段头读出）。depth 用于限制嵌套。
    fn skip_value(&mut self, kind: u8, depth: usize) -> Result<()> {
        ensure!(depth <= MAX_DEPTH, "虎牙 Tars 嵌套层数超过上限 {MAX_DEPTH}");
        match kind {
            tars::ZERO | tars::STRUCT_END => Ok(()),
            tars::INT8 => self.advance(1),
            tars::INT16 => self.advance(2),
            tars::INT32 | tars::FLOAT => self.advance(4),
            tars::INT64 | tars::DOUBLE => self.advance(8),
            tars::STRING1 => {
                self.require(1)?;
                let count = usize::from(self.data[self.position]);
                self.position += 1;
                self.advance(count)
            }
            tars::STRING4 => {
                let count = self.string4_length()?;
                self.advance(count)
            }
            tars::BYTES => {
                let (placeholder, tag) = self.head()?;
                ensure!(
                    placeholder == tars::INT8 && tag == 0,
                    "虎牙 Tars 字节数组缺少 tag0 占位头"
                );
                let count = self.count(MAX_WIRE_BYTES)?;
                self.advance(count)
            }
            tars::STRUCT_BEGIN => {
                // 结构体：一直读到 STRUCTEND 为界。
                loop {
                    ensure!(!self.at_end(), "虎牙 Tars 结构体缺少结束标记");
                    let (nested, _) = self.head()?;
                    if nested == tars::STRUCT_END {
                        return Ok(());
                    }
                    self.skip_value(nested, depth + 1)?;
                }
            }
            tars::LIST | tars::MAP => {
                let count = self.count(MAX_ELEMENTS)?;
                let elements = if kind == tars::MAP {
                    count
                        .checked_mul(2)
                        .ok_or_else(|| anyhow!("虎牙 Tars Map 元素数溢出"))?
                } else {
                    count
                };
                // 每个元素至少一个头字节；先做剩余字节预检，避免畸形数量导致长时间空转。
                ensure!(
                    elements <= self.remaining(),
                    "虎牙 Tars 容器元素数超过剩余字节"
                );
                for _ in 0..elements {
                    let (element, _) = self.head()?;
                    ensure!(element != tars::STRUCT_END, "虎牙 Tars 容器元素缺少类型");
                    self.skip_value(element, depth + 1)?;
                }
                Ok(())
            }
            _ => bail!("虎牙 Tars 使用了未定义的类型 {kind}"),
        }
    }

    fn string4_length(&mut self) -> Result<usize> {
        self.require(4)?;
        let value = u32::from_be_bytes([
            self.data[self.position],
            self.data[self.position + 1],
            self.data[self.position + 2],
            self.data[self.position + 3],
        ]);
        self.position += 4;
        usize::try_from(value).context("虎牙 Tars 字符串长度超出范围")
    }

    /// 前进到目标 tag（字段按 tag 升序）：命中则消费字段头并返回类型；未命中返回 None。
    fn field(&mut self, tag: u8) -> Result<Option<u8>> {
        loop {
            if self.at_end() {
                return Ok(None);
            }
            let (_, current) = self.peek_head()?;
            if current > tag {
                return Ok(None);
            }
            let (kind, _) = self.head()?;
            if current == tag {
                return Ok(Some(kind));
            }
            self.skip_value(kind, 0)?;
        }
    }

    /// 读完当前结构体直到 STRUCTEND（旧实现里“必须吃掉 STRUCTEND”那一步）。
    fn skip_to_end(&mut self) -> Result<()> {
        loop {
            ensure!(!self.at_end(), "虎牙 Tars 结构体缺少结束标记");
            let (kind, _) = self.head()?;
            if kind == tars::STRUCT_END {
                return Ok(());
            }
            self.skip_value(kind, 0)?;
        }
    }

    /// 读取 tag 对应的整数字段；类型不匹配时跳过该字段并返回 None（旧实现同样跳过）。
    fn num(&mut self, tag: u8) -> Result<Option<i64>> {
        match self.field(tag)? {
            None => Ok(None),
            Some(kind) => match self.integer(kind)? {
                Some(value) => Ok(Some(value)),
                None => {
                    self.skip_value(kind, 0)?;
                    Ok(None)
                }
            },
        }
    }

    /// 读取 tag 对应的字符串字段；非法 UTF-8 返回 Err，长度超过上限返回 Err。
    fn text(&mut self, tag: u8) -> Result<Option<String>> {
        match self.field(tag)? {
            None => Ok(None),
            Some(kind) => {
                let count = match kind {
                    tars::STRING1 => {
                        self.require(1)?;
                        let count = usize::from(self.data[self.position]);
                        self.position += 1;
                        count
                    }
                    tars::STRING4 => self.string4_length()?,
                    _ => {
                        self.skip_value(kind, 0)?;
                        return Ok(None);
                    }
                };
                ensure!(count <= MAX_STRING_BYTES, "虎牙 Tars 字符串超过长度上限");
                self.require(count)?;
                let bytes = &self.data[self.position..self.position + count];
                self.position += count;
                let text = std::str::from_utf8(bytes).context("虎牙 Tars 字符串不是合法 UTF-8")?;
                Ok(Some(text.to_owned()))
            }
        }
    }

    /// 读取 BYTES 字段（两个头字节），返回借用切片。
    fn bytes(&mut self, tag: u8) -> Result<Option<&'a [u8]>> {
        match self.field(tag)? {
            None => Ok(None),
            Some(tars::BYTES) => {
                let (placeholder, placeholder_tag) = self.head()?;
                ensure!(
                    placeholder == tars::INT8 && placeholder_tag == 0,
                    "虎牙 Tars 字节数组缺少 tag0 占位头"
                );
                let count = self.count(MAX_WIRE_BYTES)?;
                self.require(count)?;
                let bytes = &self.data[self.position..self.position + count];
                self.position += count;
                Ok(Some(bytes))
            }
            Some(kind) => {
                self.skip_value(kind, 0)?;
                Ok(None)
            }
        }
    }

    /// 判断 tag 处是否 STRUCTBEGIN（可选结构体，如 ContentFormat），不改变游标。
    fn peek_is_struct(&mut self, tag: u8) -> Result<bool> {
        let saved = self.position;
        let result = self.field(tag).map(|kind| kind == Some(tars::STRUCT_BEGIN));
        self.position = saved;
        result
    }

    /// 进入 tag 处的结构体；类型不匹配时跳过并返回 false。
    fn enter_struct(&mut self, tag: u8) -> Result<bool> {
        match self.field(tag)? {
            None => Ok(false),
            Some(tars::STRUCT_BEGIN) => Ok(true),
            Some(kind) => {
                self.skip_value(kind, 0)?;
                Ok(false)
            }
        }
    }

    /// 进入 tag 处的 LIST 并返回元素数；类型不匹配时跳过并返回 None。
    fn enter_list(&mut self, tag: u8) -> Result<Option<usize>> {
        match self.field(tag)? {
            None => Ok(None),
            Some(tars::LIST) => {
                let count = self.count(MAX_ELEMENTS)?;
                ensure!(
                    count <= self.remaining(),
                    "虎牙 Tars 列表元素数超过剩余字节"
                );
                Ok(Some(count))
            }
            Some(kind) => {
                self.skip_value(kind, 0)?;
                Ok(None)
            }
        }
    }

    /// 进入 tag 处的 MAP 并返回条目数；类型不匹配时跳过并返回 None。
    /// （虎牙聊天字段未使用 Map，此方法供 Tars 子集往返测试与旧实现对齐。）
    #[allow(dead_code)]
    fn enter_map(&mut self, tag: u8) -> Result<Option<usize>> {
        match self.field(tag)? {
            None => Ok(None),
            Some(tars::MAP) => {
                let count = self.count(MAX_ELEMENTS)?;
                Ok(Some(count))
            }
            Some(kind) => {
                self.skip_value(kind, 0)?;
                Ok(None)
            }
        }
    }

    /// 读取 FLOAT 字段；类型不匹配时跳过并返回 None。聊天字段未使用，供子集往返测试。
    #[allow(dead_code)]
    fn float(&mut self, tag: u8) -> Result<Option<f32>> {
        match self.field(tag)? {
            None => Ok(None),
            Some(tars::FLOAT) => {
                self.require(4)?;
                let bytes = [
                    self.data[self.position],
                    self.data[self.position + 1],
                    self.data[self.position + 2],
                    self.data[self.position + 3],
                ];
                self.position += 4;
                Ok(Some(f32::from_bits(u32::from_be_bytes(bytes))))
            }
            Some(kind) => {
                self.skip_value(kind, 0)?;
                Ok(None)
            }
        }
    }

    /// 读取 DOUBLE 字段；类型不匹配时跳过并返回 None。聊天字段未使用，供子集往返测试。
    #[allow(dead_code)]
    fn double(&mut self, tag: u8) -> Result<Option<f64>> {
        match self.field(tag)? {
            None => Ok(None),
            Some(tars::DOUBLE) => {
                self.require(8)?;
                let mut bytes = [0_u8; 8];
                bytes.copy_from_slice(&self.data[self.position..self.position + 8]);
                self.position += 8;
                Ok(Some(f64::from_bits(u64::from_be_bytes(bytes))))
            }
            Some(kind) => {
                self.skip_value(kind, 0)?;
                Ok(None)
            }
        }
    }
}

// ------------------------------------------------------------------ 命令包解码

/// 一条 WebSocket 命令包的解码结果。decode 只暴露事件；
/// 连接循环需要额外知道心跳确认与订阅回显，所以内部保留这些字段。
#[derive(Debug, Default)]
struct Decoded {
    heartbeat_ack: bool,
    /// Some：收到 cmd 17 回显（可能为空列表 = 组名/频道 id 错误）。
    subscribe_groups: Option<Vec<String>>,
    events: Vec<DanmakuEvent>,
}

/// Room facts the subscription reply is interpreted against: a room that is
/// currently offline answers with an empty group as well, which is not a
/// channel-id error.
#[derive(Debug, Clone, Copy, Default)]
struct DecodeContext {
    live: Option<bool>,
    channel_id: i64,
}

impl Decoded {
    fn into_events(self, context: DecodeContext) -> Vec<DanmakuEvent> {
        let mut events = Vec::new();
        if let Some(groups) = self.subscribe_groups {
            if groups.is_empty() {
                let message = match context.live {
                    Some(false) => format!(
                        "虎牙：房间当前未开播，服务端不推送弹幕（订阅组为空；频道 {}）",
                        context.channel_id
                    ),
                    _ if context.channel_id > 0 => format!(
                        "虎牙：订阅返回空组，lChannelId 可能取错了（当前 {}）",
                        context.channel_id
                    ),
                    _ => "虎牙：订阅返回空组，lChannelId 可能取错了".to_owned(),
                };
                events.push(DanmakuEvent::Status(message));
            } else {
                events.push(DanmakuEvent::Status(format!(
                    "虎牙：订阅成功 {}",
                    groups.join(",")
                )));
            }
        }
        events.extend(self.events);
        events
    }
}

fn limit_text(value: &str) -> String {
    value.chars().take(MAX_TEXT_CHARS).collect()
}

/// 解码一条完整的虎牙弹幕 WebSocket 二进制消息（外层 Tars 命令包）。
///
/// 返回的 DanmakuEvent::Message 只覆盖聊天（uri 1400）。虎牙协议没有付费留言
/// （SC）消息类型，参考实现也明确拒绝用大额礼物伪造 SC，所以这里不会产出
/// SuperChat；Status 用于订阅回显，便于上层显示“订阅是否成功”。
pub fn decode(payload: &[u8]) -> Result<Vec<DanmakuEvent>> {
    let context = DecodeContext::default();
    Ok(decode_command_with(payload, context)?.into_events(context))
}

fn decode_command_with(payload: &[u8], _context: DecodeContext) -> Result<Decoded> {
    ensure!(
        payload.len() <= MAX_WIRE_BYTES,
        "虎牙弹幕帧超过 {MAX_WIRE_BYTES} 字节上限"
    );
    ensure!(!payload.is_empty(), "虎牙弹幕帧为空");
    let mut reader = TarsReader::new(payload);
    let command = reader
        .num(0)?
        .ok_or_else(|| anyhow!("虎牙弹幕帧缺少 iCmdType（tag0 整数）"))?;
    let mut decoded = Decoded::default();
    match command {
        // cmd 4：心跳回包，没有业务事件；连接循环用它清除缺回包计数。
        CMD_HEARTBEAT_ACK => {
            decoded.heartbeat_ack = true;
            return Ok(decoded);
        }
        // cmd 0：空包噪声（旧实现实测 150 秒 347 个），直接丢弃。
        0 => return Ok(decoded),
        _ => {}
    }
    let data = reader.bytes(1)?;
    match command {
        // 这三类命令必须带 vData；缺失说明报文被截断，不能当成“无事件”静默成功。
        CMD_PUSH_V1 | CMD_PUSH_V2 | CMD_REGISTER_GROUP_RSP => {
            let data = data.ok_or_else(|| anyhow!("虎牙命令 {command} 缺少 vData（tag1）字段"))?;
            match command {
                CMD_PUSH_V1 => decode_push_v1(data, &mut decoded.events)?,
                CMD_PUSH_V2 => decode_push_v2(data, &mut decoded.events)?,
                CMD_REGISTER_GROUP_RSP => {
                    decoded.subscribe_groups = Some(decode_register_group_rsp(data)?)
                }
                _ => unreachable!(),
            }
        }
        // cmd 2（WUP 响应）等其余命令：本端不发送 WUP 请求，保持忽略。
        _ => {}
    }
    ensure!(
        decoded.events.len() <= MAX_EVENTS_PER_FRAME,
        "单帧虎牙弹幕事件数 {} 超过上限 {MAX_EVENTS_PER_FRAME}",
        decoded.events.len()
    );
    Ok(decoded)
}

/// cmd 7：WSPushMessage{ePushType tag0, iUri tag1, sMsg tag2, iProtocolType tag3}。
fn decode_push_v1(data: &[u8], output: &mut Vec<DanmakuEvent>) -> Result<()> {
    let mut reader = TarsReader::new(data);
    let uri = reader.num(1)?.unwrap_or(-1);
    if let Some(message) = reader.bytes(2)? {
        decode_uri(uri, message, output)?;
    }
    Ok(())
}

/// cmd 22：V2 推送，tag1 是 list<{iUri tag0, sMsg tag1, lMsgId tag2}>。
/// 每个元素是独立的 Tars 结构体，读完字段必须消费 STRUCTEND 再进入下一个。
fn decode_push_v2(data: &[u8], output: &mut Vec<DanmakuEvent>) -> Result<()> {
    let mut reader = TarsReader::new(data);
    let Some(count) = reader.enter_list(1)? else {
        return Ok(());
    };
    for _ in 0..count {
        let (kind, _) = reader.head()?;
        ensure!(kind == tars::STRUCT_BEGIN, "虎牙 V2 推送元素不是结构体");
        let uri = reader.num(0)?.unwrap_or(-1);
        let message = reader.bytes(1)?;
        // lMsgId：旧实现不用于渲染，这里同样忽略（事件模型没有稳定消息 id）。
        let _message_id = reader.num(2)?;
        reader.skip_to_end()?;
        if let Some(message) = message {
            decode_uri(uri, message, output)?;
        }
        ensure!(
            output.len() <= MAX_EVENTS_PER_FRAME,
            "单帧虎牙弹幕事件数 {} 超过上限 {MAX_EVENTS_PER_FRAME}",
            output.len()
        );
    }
    Ok(())
}

/// cmd 17：RegisterGroupRsp，tag1 是 list<string>。空列表意味着组名/频道 id 错了
/// （旧实现实测：用 yyuid 当组名时服务器静默回空组，不报错）。
fn decode_register_group_rsp(data: &[u8]) -> Result<Vec<String>> {
    let mut reader = TarsReader::new(data);
    let Some(count) = reader.enter_list(1)? else {
        return Ok(Vec::new());
    };
    let mut groups = Vec::new();
    for _ in 0..count {
        // 列表元素本身是 Str(0, 值) 的完整编码；不要再额外读一次头，
        // 否则会把“长度字节”当成新头（旧实现踩过，组名永远解析成空串）。
        if let Some(group) = reader.text(0)? {
            if groups.len() < 64 {
                groups.push(limit_text(&group));
            }
        }
    }
    Ok(groups)
}

/// 按 uri 分发消息体。只处理 1400（聊天）；礼物/进场/在线数等通道保持忽略，
/// 并且不做“大额礼物升格 SC”的近似（虎牙协议没有 SC 等价物）。
fn decode_uri(uri: i64, message: &[u8], output: &mut Vec<DanmakuEvent>) -> Result<()> {
    if uri != URI_MESSAGE_NOTICE {
        return Ok(());
    }
    if let Some(item) = decode_message_notice(message)? {
        output.push(DanmakuEvent::Message(item));
    }
    Ok(())
}

/// uri 1400 MessageNotice：
///   tag0 tUserInfo{ tag2 sNickName, tag4 sAvatarUrl, tag5 iNobleLevel }
///   tag3 sContent（正文）
///   tag5 ContentFormat{ tag0 iFontColor }
///
/// 两个旧实现踩过的坑在这里变成硬性步骤：
/// 1. 读完 tUserInfo 必须 skip_to_end() 吃掉 STRUCTEND，否则外层 tag3 会先命中
///    tUserInfo 内部的 tag3（iGender），正文取错或取空；
/// 2. ContentFormat 是可选字段，用 peek_is_struct 判断存在后再读颜色。
///
/// GuardLevel（tUserInfo.tag5）是虎牙的 iNobleLevel（贵族等级 1–7），与 B站 的
/// 舰长/提督/总督语义不同，所以这里读取后丢弃，不塞进 guard_level 冒充。
fn decode_message_notice(message: &[u8]) -> Result<Option<DanmakuMessage>> {
    ensure!(message.len() <= MAX_WIRE_BYTES, "虎牙聊天报文超过大小上限");
    let mut reader = TarsReader::new(message);
    let mut user = String::new();
    if reader.peek_is_struct(0)? {
        reader.enter_struct(0)?;
        user = reader.text(2)?.unwrap_or_default();
        let _noble_level = reader.num(5)?;
        reader.skip_to_end()?;
    }
    let Some(text) = reader.text(3)? else {
        return Ok(None);
    };
    if text.is_empty() {
        return Ok(None);
    }
    let mut color = 0x00FF_FFFF_u32;
    // ContentFormat 用一个新的 reader 从报文头开始探测（旧实现同样做法），
    // 避免正文读取后游标位置影响可选字段。
    let mut format = TarsReader::new(message);
    if format.peek_is_struct(5)? {
        format.enter_struct(5)?;
        if let Some(value) = format.num(0)? {
            if value > 0 {
                color = (value as u64 & 0xFF_FFFF) as u32;
            }
        }
        format.skip_to_end()?;
    }
    Ok(Some(DanmakuMessage {
        image_url: None,
        // 旧实现不产出消息 id，事件模型也没有稳定 id 来源，这里保持空串。
        id: String::new(),
        user: limit_text(&user),
        text: limit_text(&text),
        color,
        mode: DanmakuMode::Scroll,
        font_size: 25,
        user_id: 0,
        user_level: 0,
        guard_level: 0,
        medal: None,
        image_width: 0,
        image_height: 0,
        image_large: false,
    }))
}

// ------------------------------------------------------------------ 发送帧

/// cmd 16：RegisterGroupReq。组名必须用 lChannelId（订阅 chat + live 两组）。
fn register_group_frame(channel_id: i64) -> Result<Vec<u8>> {
    ensure!(channel_id > 0, "虎牙频道 id 无效，无法订阅弹幕");
    let mut groups = TarsWriter::new();
    groups.list_head(0, 2);
    groups.str(0, &format!("live:{channel_id}"))?;
    groups.str(0, &format!("chat:{channel_id}"))?;
    groups.str(1, "")?; // sToken：匿名留空
    let mut command = TarsWriter::new();
    command.i32(0, CMD_REGISTER_GROUP);
    command.bytes(1, &groups.finish())?;
    Ok(command.finish())
}

/// cmd 3：EWSCmd_WupReq 心跳（WUP onlineui/OnUserHeartBeat）。
///
/// 报文 = 外层 Tars 命令包 { tag0=3, tag1=BYTES }；BYTES 内容 = 4 字节大端总长
/// （含这 4 字节）+ WUP 请求体。旧实现用这套结构实测 60 秒一次、0~0.1 秒回 cmd 4。
fn heartbeat_frame(yyid: i64, channel_id: i64, sub_channel_id: i64) -> Result<Vec<u8>> {
    // tUserInfo
    let mut body = TarsWriter::new();
    body.struct_begin(0);
    body.i64(0, yyid);
    body.str(1, "")?;
    body.str(2, "")?;
    body.str(3, "adr_wap")?; // sHuYaUA
    body.str(4, "")?;
    body.i32(5, 0);
    body.struct_end();
    body.i64(1, channel_id);
    body.i64(2, sub_channel_id);
    body.i64(3, 0);
    body.i64(4, yyid);
    body.i8(5, 1);
    body.i32(6, 1);
    body.i32(7, 0);
    body.i32(8, 0);
    body.i32(9, 0);
    body.i32(10, 0);

    // WUP 请求：iVersion/cPacketType/iMessageType/iRequestId/sServantName/sFuncName/
    // sBuffer(Bytes)/iTimeout/context(Map)/status(Map)
    let mut request = TarsWriter::new();
    request.i16(1, 3);
    request.i8(2, 0);
    request.i32(3, 0);
    request.i32(4, 0);
    request.str(5, "onlineui")?;
    request.str(6, "OnUserHeartBeat")?;
    let mut fields = TarsWriter::new();
    fields.map_head(0, 1);
    fields.str(0, "tReq")?;
    fields.bytes(1, &body.finish())?;
    request.bytes(7, &fields.finish())?;
    request.i32(8, 0);
    request.map_head(9, 0);
    request.map_head(10, 0);

    let payload = request.finish();
    let total = u32::try_from(payload.len() + 4).context("虎牙心跳包超过长度上限")?;
    let mut framed = Vec::with_capacity(payload.len() + 4);
    framed.extend_from_slice(&total.to_be_bytes());
    framed.extend_from_slice(&payload);

    let mut command = TarsWriter::new();
    command.i32(0, CMD_HEARTBEAT);
    command.bytes(1, &framed)?;
    Ok(command.finish())
}

// ------------------------------------------------------------------ 房间解析

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RoomIds {
    /// 房间频道 id：订阅组名与心跳 lChannelId 都用它。
    channel_id: i64,
    /// 主播 yyuid：心跳 lYyid 用；拿不到时旧实现也会带 0 发送。
    yyid: i64,
    /// Observed live state from the room cache; None means unknown. An offline
    /// room legitimately answers the subscription with an empty group.
    live: Option<bool>,
}

/// 从房间页的脚本片段里抠数字，语义对齐旧实现 JsonLong：
/// 找到 key 后跳过空格/引号，只读连续数字。
fn json_number_after(text: &str, key: &str) -> Option<i64> {
    let start = text.find(key)? + key.len();
    let bytes = text.as_bytes();
    let mut cursor = start;
    while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'"') {
        cursor += 1;
    }
    let digits = cursor;
    while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
        cursor += 1;
    }
    if cursor == digits {
        return None;
    }
    text[digits..cursor].parse().ok()
}

/// 从页面里抠一个带引号的字符串值（liveStatus 等），只接受单层普通文本。
fn json_string_after(text: &str, key: &str) -> Option<String> {
    let start = text.find(key)? + key.len();
    let bytes = text.as_bytes();
    let mut cursor = start;
    while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'"') {
        cursor += 1;
    }
    let begin = cursor;
    while cursor < bytes.len() && bytes[cursor] != b'"' && bytes[cursor] != b'\n' {
        cursor += 1;
    }
    if cursor == begin || cursor >= bytes.len() {
        return None;
    }
    let value = std::str::from_utf8(&bytes[begin..cursor]).ok()?.trim();
    (!value.is_empty() && value.len() <= 64).then(|| value.to_owned())
}

fn live_status_from(value: &str) -> Option<bool> {
    match value.to_ascii_uppercase().as_str() {
        "ON" | "LIVE" | "1" | "2" => Some(true),
        "OFF" | "CLOSE" | "REPLAY" | "0" | "4" => Some(false),
        _ => None,
    }
}

/// 依次用房间页、移动页、手机接口缓存补全 id（对齐旧实现 ResolveRoom 的优先级）。
fn room_ids_from_pages(pc: Option<&str>, mobile: Option<&str>, cache: Option<&str>) -> RoomIds {
    let mut ids = RoomIds::default();
    if let Some(page) = pc {
        ids.yyid = json_number_after(page, "\"yyid\":")
            .filter(|value| *value != 0)
            .or_else(|| json_number_after(page, "\"privateHost\":"))
            .unwrap_or(0);
        ids.channel_id = json_number_after(page, "\"uid\":")
            .filter(|value| *value != 0)
            .or_else(|| json_number_after(page, "\"liveChannel\":"))
            .unwrap_or(0);
        ids.live =
            json_string_after(page, "\"liveStatus\":").and_then(|value| live_status_from(&value));
    }
    if ids.channel_id == 0 || ids.yyid == 0 {
        if let Some(page) = mobile {
            if ids.channel_id == 0 {
                ids.channel_id = json_number_after(page, "\"lChannelId\":").unwrap_or(0);
            }
            if ids.yyid == 0 {
                ids.yyid = json_number_after(page, "\"lYyid\":").unwrap_or(0);
            }
            ids.live = ids.live.or_else(|| {
                json_string_after(page, "\"liveStatus\":")
                    .and_then(|value| live_status_from(&value))
            });
        }
    }
    if let Some(page) = cache {
        if ids.channel_id == 0 {
            ids.channel_id = json_number_after(page, "\"lChannelId\":")
                .filter(|value| *value != 0)
                .or_else(|| json_number_after(page, "\"liveChannel\":").filter(|value| *value != 0))
                .or_else(|| json_number_after(page, "\"channel\":").filter(|value| *value != 0))
                .unwrap_or(0);
        }
        if ids.yyid == 0 {
            ids.yyid = json_number_after(page, "\"lYyid\":")
                .filter(|value| *value != 0)
                .or_else(|| json_number_after(page, "\"yyid\":").filter(|value| *value != 0))
                .or_else(|| json_number_after(page, "\"privateHost\":").filter(|value| *value != 0))
                .unwrap_or(0);
        }
        ids.live = ids
            .live
            .or_else(|| {
                json_string_after(page, "\"liveStatus\":")
                    .and_then(|value| live_status_from(&value))
            })
            .or_else(|| {
                json_string_after(page, "\"realLiveStatus\":")
                    .and_then(|value| live_status_from(&value))
            });
    }
    ids
}

fn room_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .user_agent(USER_AGENT)
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(12))
        // 不跟随跳转：房间页只会是固定主机，跳转视为异常响应。
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("无法初始化虎牙房间页 HTTP 客户端")
}

/// 有界读取一个房间页。网络/HTTP 失败返回 None（旧实现 TryGet 同样吞掉异常），
/// 只有取消才向上返回 Err。
/// Validates one room-page redirect: only standard HTTPS huya.com pages are
/// followed (the mobile page redirects to the canonical www page; the legacy
/// client followed redirects and resolved ids from the final page).
fn next_page_url(current: &str, location: &str) -> Result<String> {
    let base = url::Url::parse(current)?;
    let next = base.join(location)?;
    ensure!(
        next.scheme() == "https",
        "虎牙房间页重定向到了非 HTTPS 地址"
    );
    let host = next.host_str().unwrap_or("");
    ensure!(
        host == "huya.com" || host.ends_with(".huya.com"),
        "虎牙房间页被重定向到非预期主机"
    );
    ensure!(
        next.username().is_empty()
            && next.password().is_none()
            && next.port().is_none()
            && next.fragment().is_none(),
        "虎牙房间页重定向包含凭据、非标准端口或片段"
    );
    Ok(next.to_string())
}

fn fetch_page(
    client: &reqwest::blocking::Client,
    url: &str,
    cancellation: &Cancellation,
) -> Result<Option<String>> {
    let mut current = url.to_owned();
    for _ in 0..=MAX_PAGE_REDIRECTS {
        if cancellation.is_cancelled() {
            bail!("虎牙弹幕解析房间已取消");
        }
        let response = match client.get(&current).send() {
            Ok(response) => response,
            Err(_) => return Ok(None),
        };
        if response.status().is_redirection() {
            let Some(location) = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
            else {
                return Ok(None);
            };
            match next_page_url(&current, location) {
                Ok(next) => {
                    current = next;
                    continue;
                }
                Err(_) => return Ok(None),
            }
        }
        if !response.status().is_success() {
            return Ok(None);
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_PAGE_BYTES as u64)
        {
            return Ok(None);
        }
        let mut body = Vec::new();
        if response
            .take(MAX_PAGE_BYTES as u64 + 1)
            .read_to_end(&mut body)
            .is_err()
        {
            return Ok(None);
        }
        if body.len() > MAX_PAGE_BYTES {
            return Ok(None);
        }
        return Ok(Some(String::from_utf8_lossy(&body).into_owned()));
    }
    Ok(None)
}

/// 匿名解析房间号 -> (lChannelId, yyid)。只访问虎牙公开页面/接口，不读取登录态。
///
/// 注意：页面结构变化会让解析失败；这种情况返回可操作错误，由上层提示用户，
/// 不能猜一个 id 继续（用错组名服务器只回空组，反而更难排查）。
fn resolve_room(room: &str, cancellation: &Cancellation) -> Result<RoomIds> {
    // 复用域层校验：手工构造的 Target 也不能绕过房间号字符集限制拼进 URL。
    Target::parse(&format!("huya:{room}")).context("虎牙房间号无效")?;
    let client = room_client()?;
    let pc = fetch_page(
        &client,
        &format!("https://www.huya.com/{room}"),
        cancellation,
    )?;
    let first = room_ids_from_pages(pc.as_deref(), None, None);
    let mut mobile = None;
    if first.channel_id == 0 || first.yyid == 0 || first.live.is_none() {
        mobile = fetch_page(&client, &format!("https://m.huya.com/{room}"), cancellation)?;
    }
    // The cache API is also the authoritative live/offline flag, so it is read
    // on every connect (the legacy ResolveRoom did the same) even when the page
    // already yielded the ids. An offline room answers the subscription with an
    // empty group; that is not a channel-id error.
    let mut cache_url =
        url::Url::parse("https://mp.huya.com/cache.php").context("虎牙房间缓存接口地址无效")?;
    cache_url.query_pairs_mut().extend_pairs([
        ("m", "Live"),
        ("do", "profileRoom"),
        ("roomid", room),
    ]);
    let cache = fetch_page(&client, cache_url.as_str(), cancellation)?;
    let ids = room_ids_from_pages(pc.as_deref(), mobile.as_deref(), cache.as_deref());
    ensure!(
        ids.channel_id > 0,
        "虎牙：无法从房间页解析出 lChannelId（房间 {room} 可能不存在，或页面结构已变化）；请检查网络后重试"
    );
    Ok(ids)
}

// ------------------------------------------------------------------ 连接

fn tcp_connect(host: &str, port: u16, cancellation: &Cancellation) -> Result<TcpStream> {
    let addresses: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("虎牙弹幕域名解析失败：{host}"))?
        .take(8)
        .collect();
    ensure!(!addresses.is_empty(), "虎牙弹幕域名没有可用地址：{host}");
    for address in addresses {
        if cancellation.is_cancelled() {
            bail!("虎牙弹幕连接已取消");
        }
        // 只连已知平台端点；本机 TUN 代理的 fake-IP 段由 TLS 主机名校验兜底。
        if !crate::danmaku::is_allowed_danmaku_address(address.ip()) {
            continue;
        }
        if let Ok(stream) = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            stream
                .set_nodelay(true)
                .context("无法设置虎牙弹幕 TCP_NODELAY")?;
            return Ok(stream);
        }
    }
    bail!("无法连接虎牙弹幕服务器 {host}:{port}；请检查网络或代理是否允许 WSS 出站")
}

/// 把读超时调到读轮询周期（<= 500ms），让安静房间也能及时响应取消/心跳。
fn set_read_poll(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>) -> Result<()> {
    let stream = match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => stream,
        MaybeTlsStream::NativeTls(stream) => stream.get_mut(),
        _ => bail!("虎牙弹幕使用了不支持的 TLS 传输"),
    };
    stream
        .set_read_timeout(Some(READ_POLL))
        .context("无法设置虎牙弹幕读超时")?;
    Ok(())
}

fn cancellable_wait(cancellation: &Cancellation, duration: Duration) {
    let deadline = Instant::now() + duration;
    while !cancellation.is_cancelled() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        thread::sleep(left.min(Duration::from_millis(50)));
    }
}

/// 单次连接生命周期：握手 -> 订阅 -> 心跳 -> 收包，直到取消/服务端关闭/出错。
fn huya_once(
    room: &RoomIds,
    sender: &SyncSender<DanmakuEvent>,
    cancellation: &Cancellation,
) -> Result<()> {
    let stream = tcp_connect(WS_HOST, 443, cancellation)?;
    // 握手阶段需要比读轮询更宽的读超时；TLS 握手完成后立即调回 500ms。
    stream
        .set_read_timeout(Some(CONNECT_TIMEOUT))
        .context("无法设置虎牙弹幕握手超时")?;
    stream
        .set_write_timeout(Some(CONNECT_TIMEOUT))
        .context("无法设置虎牙弹幕写超时")?;

    let request = WS_URL
        .into_client_request()
        .context("虎牙弹幕 WebSocket 请求无效")?;
    // 单条消息上限与 danmaku 模块一致：超限由 tungstenite 中止连接，交给有界重连。
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_WIRE_BYTES))
        .max_frame_size(Some(MAX_WIRE_BYTES));
    let (mut socket, _response) =
        tungstenite::client_tls_with_config(request, stream, Some(config), None).map_err(|_| {
            anyhow!("虎牙弹幕 WebSocket 握手失败（{WS_URL}）；请检查网络是否允许 WSS 出站")
        })?;
    set_read_poll(&mut socket)?;

    let _ = emit(
        sender,
        DanmakuEvent::Status(format!("虎牙：已连接，正在订阅 chat:{}", room.channel_id)),
    );
    socket
        .send(Message::Binary(
            register_group_frame(room.channel_id)?.into(),
        ))
        .map_err(|_| anyhow!("虎牙弹幕订阅包发送失败"))?;
    socket
        .send(Message::Binary(
            heartbeat_frame(room.yyid, room.channel_id, room.channel_id)?.into(),
        ))
        .map_err(|_| anyhow!("虎牙弹幕心跳包发送失败"))?;
    socket.flush().context("虎牙弹幕握手后刷新发送缓冲失败")?;

    let decode_context = DecodeContext {
        live: room.live,
        channel_id: room.channel_id,
    };
    let mut last_heartbeat = Instant::now();
    let mut pending_ack = Some(Instant::now());
    let mut missed_acks = 0_u32;
    let mut consecutive_parse_errors = 0_u32;
    loop {
        if cancellation.is_cancelled() {
            let _ = socket.close(None);
            return Ok(());
        }
        // 心跳：60 秒一次；发送后开始等待 cmd 4，连续两次超时才主动重连。
        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            let frame = heartbeat_frame(room.yyid, room.channel_id, room.channel_id)?;
            socket
                .send(Message::Binary(frame.into()))
                .map_err(|_| anyhow!("虎牙弹幕心跳发送失败，连接可能已被服务端关闭"))?;
            socket.flush().context("虎牙弹幕心跳刷新发送缓冲失败")?;
            last_heartbeat = Instant::now();
            pending_ack = Some(Instant::now());
        }
        if let Some(sent_at) = pending_ack {
            if sent_at.elapsed() >= HEARTBEAT_ACK_TIMEOUT {
                pending_ack = None;
                missed_acks += 1;
                if missed_acks >= MAX_MISSED_HEARTBEATS {
                    bail!(
                        "虎牙弹幕连续 {missed_acks} 次心跳在 {} 秒内没有收到确认，连接可能已失效",
                        HEARTBEAT_ACK_TIMEOUT.as_secs()
                    );
                }
            }
        }
        match socket.read() {
            Ok(Message::Binary(bytes)) => match decode_command_with(&bytes, decode_context) {
                Ok(decoded) => {
                    consecutive_parse_errors = 0;
                    if decoded.heartbeat_ack {
                        // 迟到的心跳回包也证明连接有响应，清除连续缺回包计数。
                        pending_ack = None;
                        missed_acks = 0;
                    }
                    for event in decoded.into_events(decode_context) {
                        if emit(sender, event) == Delivery::Closed {
                            return Ok(());
                        }
                    }
                }
                Err(error) => {
                    // 单条坏报文不应该拖垮整条连接（旧实现同样只上报继续收），
                    // 但连续失败说明协议已经错位，交给有界重连。
                    consecutive_parse_errors += 1;
                    if consecutive_parse_errors >= MAX_CONSECUTIVE_PARSE_ERRORS {
                        return Err(error.context(format!(
                            "虎牙弹幕连续 {consecutive_parse_errors} 条报文无法解析"
                        )));
                    }
                    let _ = emit(
                        sender,
                        DanmakuEvent::Status(format!(
                            "虎牙：丢弃一条无法解析的弹幕报文（{error}）"
                        )),
                    );
                }
            },
            Ok(Message::Close(_)) => return Ok(()),
            Ok(Message::Ping(_)) => {
                // tungstenite 已把 Pong 放入发送队列，这里推动写出去。
                socket.flush().context("虎牙弹幕回复 Pong 失败")?;
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Ok(());
            }
            Err(_) => bail!("虎牙弹幕 WebSocket 连接中断"),
        }
    }
}

fn retry_delay(attempt: u32) -> Duration {
    let factor = 1_u64 << attempt.saturating_sub(1).min(2);
    (Duration::from_millis(500) * factor as u32).min(RETRY_DELAY_CAP)
}

/// 打开匿名虎牙弹幕 WebSocket，订阅聊天房间，按旧实现周期心跳，解码后发送事件。
///
/// 语义：
/// - 阻塞运行在调用方线程（上层用工作线程启动），每 500ms 至少检查一次取消；
/// - 断线只做有界重连：最多 3 次尝试、退避 500ms/1s、上限 2 秒；
/// - 取消时返回 Ok(())；UI 通道关闭时尽快返回；重连次数用尽返回可操作错误；
/// - 本函数不打日志、不读用户数据，只访问虎牙公开房间页与匿名 WSS。
pub fn connect(
    target: &Target,
    sender: SyncSender<DanmakuEvent>,
    cancellation: Cancellation,
) -> Result<()> {
    ensure!(
        target.platform == Platform::Huya,
        "虎牙弹幕客户端只接受虎牙房间"
    );
    target.validate().context("虎牙房间号无效")?;
    if cancellation.is_cancelled() {
        return Ok(());
    }
    // Publish before connecting so early messages use the normal image path.
    crate::danmaku::cache_emoticons(emoticon_dictionary());
    let mut last_error: Option<anyhow::Error> = None;
    for attempt in 1..=MAX_ATTEMPTS {
        if cancellation.is_cancelled() {
            return Ok(());
        }
        if emit(
            &sender,
            DanmakuEvent::Status(format!(
                "虎牙：正在解析房间并连接弹幕服务器（第 {attempt}/{MAX_ATTEMPTS} 次）"
            )),
        ) == Delivery::Closed
        {
            return Ok(());
        }
        let outcome = resolve_room(&target.room, &cancellation)
            .and_then(|room| huya_once(&room, &sender, &cancellation));
        if cancellation.is_cancelled() {
            return Ok(());
        }
        match outcome {
            Ok(()) => last_error = Some(anyhow!("虎牙弹幕连接被服务端关闭")),
            Err(error) => last_error = Some(error),
        }
        let error = last_error.as_ref().expect("连结果分支总会写入错误");
        if emit(
            &sender,
            DanmakuEvent::Status(format!(
                "虎牙弹幕连接中断（第 {attempt}/{MAX_ATTEMPTS} 次）：{error}"
            )),
        ) == Delivery::Closed
        {
            return Ok(());
        }
        if attempt < MAX_ATTEMPTS {
            if emit(
                &sender,
                DanmakuEvent::Status(format!(
                    "虎牙：{} 毫秒后重连…",
                    retry_delay(attempt).as_millis()
                )),
            ) == Delivery::Closed
            {
                return Ok(());
            }
            cancellable_wait(&cancellation, retry_delay(attempt));
        }
    }
    let error = last_error.unwrap_or_else(|| anyhow!("虎牙弹幕连接失败"));
    if emit(
        &sender,
        DanmakuEvent::Status(format!(
            "虎牙弹幕已停止重试（上限 {MAX_ATTEMPTS} 次）：{error}"
        )),
    ) == Delivery::Closed
    {
        return Ok(());
    }
    Err(error.context(format!(
        "虎牙弹幕连续 {MAX_ATTEMPTS} 次连接失败，已停止重连"
    )))
}

// ------------------------------------------------------------------ 离线单元测试

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// 旧实现 tests/DanmakuTests.cs 的 Tars 基础规则向量：
    /// 0 -> ZERO 头、100 -> INT8、40000 -> INT32、tag16 -> 两字节头。
    /// 向量来自协议回归测试（公开房间参数，不含真实用户数据）。
    #[test]
    fn writer_reproduces_upstream_tars_narrowing_vector() {
        let mut writer = TarsWriter::new();
        writer.i32(0, 0);
        writer.i32(1, 100);
        writer.i32(2, 40000);
        writer.str(16, "x").unwrap();
        assert_eq!(hex(&writer.finish()), "0c10642200009c40f6100178");
    }

    /// 旧实现 CommandRegisterGroup 的逐字节回归向量（房间 333003 的公开频道 id）。
    #[test]
    fn subscribe_frame_matches_upstream_bytes() {
        let frame = register_group_frame(2_367_547_387).unwrap();
        assert_eq!(
            hex(&frame),
            "00101d000027090002060f6c6976653a32333637353437333837060f636861743a323336373534373338371600"
        );
    }

    /// 旧实现 CommandHeartbeat 的逐字节回归向量（同上公开房间参数）。
    #[test]
    fn heartbeat_frame_matches_upstream_bytes() {
        let frame = heartbeat_frame(2_369_247_112, 2_367_547_387, 2_367_547_387).unwrap();
        assert_eq!(
            hex(&frame),
            concat!(
                "00031d0000790000007910032c3c4c56086f6e6c696e657569660f4f6e55736572486561727442656174",
                "7d00004c0800010604745265711d00003f0a03000000008d37d7881600260036076164725f7761704600",
                "5c0b13000000008d1de7fb23000000008d1de7fb3c43000000008d37d788500160017c8c9cac8c980ca80c"
            )
        );
    }

    #[test]
    fn invalid_ids_and_bounds_are_rejected_before_encoding() {
        assert!(register_group_frame(0).is_err());
        assert!(register_group_frame(-1).is_err());
        assert!(
            heartbeat_frame(0, 0, 0).is_ok(),
            "心跳允许 yyid=0（旧实现也这样发）"
        );
    }

    /// Tars 子集往返：Zero/Int8/Int16/Int32/Int64/Float/Double/String1/String4/
    /// Bytes/Struct/List/Map 全部经过同一套 writer->reader。
    #[test]
    fn tars_round_trip_covers_required_types() {
        let long_text = "长".repeat(300); // >255 字节 -> String4
        let mut writer = TarsWriter::new();
        writer.i8(0, -5);
        writer.i16(1, -300);
        writer.i32(2, 70_000);
        writer.i64(3, -5_000_000_000);
        writer.float(4, 1.5);
        writer.double(5, -2.25);
        writer.str(6, "短字符串").unwrap();
        writer.str(7, &long_text).unwrap();
        writer.bytes(8, &[1, 2, 3, 4, 5]).unwrap();
        writer.struct_begin(9);
        writer.i32(0, 42);
        writer.str(1, "inner").unwrap();
        writer.struct_end();
        writer.list_head(10, 3);
        writer.i32(0, 1);
        writer.i32(0, 2);
        writer.i32(0, 3);
        writer.map_head(11, 2);
        writer.str(0, "a").unwrap();
        writer.i32(1, 10);
        writer.str(0, "b").unwrap();
        writer.i32(1, 20);
        let data = writer.finish();

        let mut reader = TarsReader::new(&data);
        assert_eq!(reader.num(0).unwrap(), Some(-5));
        assert_eq!(reader.num(1).unwrap(), Some(-300));
        assert_eq!(reader.num(2).unwrap(), Some(70_000));
        assert_eq!(reader.num(3).unwrap(), Some(-5_000_000_000));
        assert_eq!(reader.float(4).unwrap(), Some(1.5));
        assert_eq!(reader.double(5).unwrap(), Some(-2.25));
        assert_eq!(reader.text(6).unwrap().as_deref(), Some("短字符串"));
        assert_eq!(reader.text(7).unwrap().as_deref(), Some(long_text.as_str()));
        assert_eq!(reader.bytes(8).unwrap(), Some(&[1_u8, 2, 3, 4, 5][..]));
        assert!(reader.enter_struct(9).unwrap());
        assert_eq!(reader.num(0).unwrap(), Some(42));
        assert_eq!(reader.text(1).unwrap().as_deref(), Some("inner"));
        reader.skip_to_end().unwrap();
        assert_eq!(reader.enter_list(10).unwrap(), Some(3));
        let mut list = Vec::new();
        for _ in 0..3 {
            list.push(reader.num(0).unwrap().unwrap());
        }
        assert_eq!(list, vec![1, 2, 3]);
        assert_eq!(reader.enter_map(11).unwrap(), Some(2));
        let mut entries = Vec::new();
        for _ in 0..2 {
            let key = reader.text(0).unwrap().unwrap();
            let value = reader.num(1).unwrap().unwrap();
            entries.push((key, value));
        }
        assert_eq!(entries, vec![("a".to_owned(), 10), ("b".to_owned(), 20)]);
        assert!(reader.at_end(), "所有字段都应被消费完");
    }

    #[test]
    fn malformed_tars_inputs_return_errors_instead_of_panicking() {
        // 空输入
        let mut empty = TarsReader::new(&[]);
        assert!(empty.head().is_err());

        // 截断的 INT32
        let mut truncated = TarsReader::new(&[0x02, 0x00]);
        assert!(truncated.num(0).is_err());

        // 非法 UTF-8 字符串
        let mut invalid = TarsReader::new(&[0x06, 0x02, 0xFF, 0xFE]);
        assert!(invalid.text(0).is_err());

        // 字节数组缺少 tag0 占位头
        let mut bad_bytes = TarsReader::new(&[0x0D, 0x16, 0x00]);
        assert!(bad_bytes.bytes(0).is_err());

        // 列表数量超过元素上限
        let mut bad_list = TarsReader::new(&[0x09, 0x02, 0x00, 0x00, 0xFF, 0xFF]);
        assert!(bad_list.enter_list(0).is_err());

        // 未定义类型
        let mut unknown = TarsReader::new(&[0x0E]);
        assert!(unknown.num(0).is_err());

        // 结构体缺少 STRUCTEND
        let mut unterminated = TarsReader::new(&[0x0A, 0x00, 0x01]);
        assert!(unterminated.skip_to_end().is_err());
    }

    #[test]
    fn deep_nesting_is_bounded() {
        // 75 层嵌套结构体：超过 MAX_DEPTH=64 必须被拒绝，不能爆栈。
        let mut writer = TarsWriter::new();
        for level in 0..(MAX_DEPTH + 11) {
            writer.struct_begin((level % 15) as u8);
        }
        for _ in 0..(MAX_DEPTH + 11) {
            writer.struct_end();
        }
        let data = writer.finish();
        let mut reader = TarsReader::new(&data);
        let (kind, _) = reader.head().unwrap();
        let error = reader.skip_value(kind, 0).unwrap_err();
        assert!(
            format!("{error:#}").contains("嵌套层数超过上限"),
            "实际错误：{error:#}"
        );
    }

    #[test]
    fn room_ids_follow_the_legacy_precedence() {
        let pc = r#"<script>var hyPlayerConfig={"stream":{"data":{"yyid":2369247112,"uid":2367547387}}};</script>"#;
        assert_eq!(
            room_ids_from_pages(Some(pc), None, None),
            RoomIds {
                channel_id: 2_367_547_387,
                yyid: 2_369_247_112,
                live: None
            }
        );

        // 房间页只有 uid：yyid 由移动页补，channel 不从缓存覆盖。
        let pc = r#"{"uid":777}"#;
        let mobile = r#"{"lChannelId":1234,"lYyid":5678}"#;
        let cache = r#"{"lChannelId":9911,"lYyid":0}"#;
        assert_eq!(
            room_ids_from_pages(Some(pc), Some(mobile), Some(cache)),
            RoomIds {
                channel_id: 777,
                yyid: 5678,
                live: None
            }
        );

        // 房间页没有数字时，全部靠缓存接口补。
        assert_eq!(
            room_ids_from_pages(
                None,
                Some(r#"<html>no ids</html>"#),
                Some(r#"{"lChannelId":42,"lYyid":7}"#)
            ),
            RoomIds {
                channel_id: 42,
                yyid: 7,
                live: None
            }
        );

        // 拿不到 channel id 时不能猜一个值。
        assert_eq!(room_ids_from_pages(None, None, None), RoomIds::default());
        assert_eq!(
            room_ids_from_pages(Some(r#"{"uid":0}"#), None, None),
            RoomIds::default()
        );
    }

    #[test]
    fn cache_live_status_and_channel_fallbacks_are_read() {
        // No page ids at all: liveChannel/yyid in the cache API are enough.
        let cache = r#"{"data":{"liveStatus":"OFF","liveChannel":2367547387,"yyid":2369247112,"realLiveStatus":"OFF"}}"#;
        assert_eq!(
            room_ids_from_pages(None, None, Some(cache)),
            RoomIds {
                channel_id: 2_367_547_387,
                yyid: 2_369_247_112,
                live: Some(false)
            }
        );
        // A page's liveStatus wins over the cache when both are present.
        let page = r#"{"uid":"11","yyid":"22","liveStatus":"ON"}"#;
        assert_eq!(
            room_ids_from_pages(Some(page), None, Some(cache)),
            RoomIds {
                channel_id: 11,
                yyid: 22,
                live: Some(true)
            }
        );
        // Unknown wording never guesses live state.
        assert_eq!(
            room_ids_from_pages(None, None, Some(r#"{"liveStatus":"MAYBE"}"#)).live,
            None
        );
    }

    #[test]
    fn room_page_redirects_are_host_and_scheme_scoped() {
        let base = "https://m.huya.com/333003";
        assert_eq!(
            next_page_url(base, "//www.huya.com/333003").unwrap(),
            "https://www.huya.com/333003"
        );
        for bad in [
            "http://www.huya.com/333003",
            "https://evil.invalid/333003",
            "https://user:pass@www.huya.com/333003",
            "https://www.huya.com:8443/333003",
            "https://www.huya.com/333003#frag",
        ] {
            assert!(next_page_url(base, bad).is_err(), "{bad}");
        }
        // A relative redirect stays on the canonical host.
        assert_eq!(
            next_page_url("https://www.huya.com/a", "/b").unwrap(),
            "https://www.huya.com/b"
        );
    }

    #[test]
    fn empty_group_message_distinguishes_offline_from_misconfiguration() {
        let decoded = Decoded {
            heartbeat_ack: false,
            subscribe_groups: Some(Vec::new()),
            events: Vec::new(),
        };
        let offline = decoded.into_events(DecodeContext {
            live: Some(false),
            channel_id: 2_367_547_387,
        });
        assert!(matches!(&offline[0], DanmakuEvent::Status(text) if text.contains("未开播")));
        let decoded = Decoded {
            heartbeat_ack: false,
            subscribe_groups: Some(Vec::new()),
            events: Vec::new(),
        };
        let unknown = decoded.into_events(DecodeContext {
            live: None,
            channel_id: 123,
        });
        assert!(matches!(&unknown[0], DanmakuEvent::Status(text)
                if text.contains("空组") && text.contains("123")));
    }

    #[test]
    fn retry_backoff_is_bounded() {
        assert_eq!(retry_delay(1), Duration::from_millis(500));
        assert_eq!(retry_delay(2), Duration::from_secs(1));
        assert_eq!(retry_delay(3), Duration::from_secs(2));
        assert!(retry_delay(9) <= RETRY_DELAY_CAP);
    }

    /// 旧实现 ParseMessageNotice 的十六进制报文（手工拼接，用户名/正文都是合成文本）。
    #[test]
    fn message_notice_matches_upstream_parse_vector() {
        let raw = concat!(
            "0a",
            "02",
            "0000007b",
            "1c",
            "26",
            "0c",
            "e6b58be8af95e794a8e688b7",
            "3c",
            "46",
            "0d",
            "687474703a2f2f617661746172",
            "50",
            "03",
            "0b",
            "36",
            "18",
            "e8bf99e698afe4b880e69da1e8998ee78999e5bcb9e5b995",
            "5a",
            "02",
            "00ff0000",
            "0b"
        );
        let bytes: Vec<u8> = (0..raw.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&raw[index..index + 2], 16).unwrap())
            .collect();
        let message = decode_message_notice(&bytes)
            .unwrap()
            .expect("应解出一条弹幕");
        assert_eq!(message.text, "这是一条虎牙弹幕");
        assert_eq!(message.user, "测试用户");
        assert_eq!(message.color, 0xFF0000);
        assert_eq!(message.mode, DanmakuMode::Scroll);
        assert_eq!(message.font_size, 25);
    }
}
