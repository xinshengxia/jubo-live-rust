//! Four explicit room states. Unknown responses never become "offline".
use crate::{
    auth::CookieVault,
    core::{Platform, Target},
    platforms,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoomState {
    Live,
    Offline,
    Replay,
    Unknown,
}
impl RoomState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Live => "直播中",
            Self::Offline => "未开播",
            Self::Replay => "重播中",
            Self::Unknown => "状态待确认",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomObservation {
    pub state: RoomState,
    pub title: Option<String>,
    pub note: String,
}
impl RoomObservation {
    fn new(state: RoomState, title: Option<String>) -> Self {
        Self {
            state,
            title,
            note: state.label().into(),
        }
    }
    pub fn unknown(note: impl Into<String>) -> Self {
        Self {
            state: RoomState::Unknown,
            title: None,
            note: note.into(),
        }
    }
}
fn number(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str()?.parse().ok())
}
fn title(v: &Value) -> Option<String> {
    v.as_str()
        .map(|s| {
            s.chars()
                .filter(|c| !c.is_control())
                .take(250)
                .collect::<String>()
        })
        .filter(|s| !s.is_empty())
}
pub fn parse_bili(root: &Value) -> RoomObservation {
    if number(&root["code"]) != Some(0) {
        return RoomObservation::unknown("B站未返回有效房间状态");
    }
    let state = match number(&root["data"]["live_status"]) {
        Some(1) => RoomState::Live,
        Some(0) => RoomState::Offline,
        Some(2) => RoomState::Replay,
        _ => RoomState::Unknown,
    };
    RoomObservation::new(state, title(&root["data"]["title"]))
}
pub fn parse_huya(root: &Value) -> RoomObservation {
    if number(&root["status"]) != Some(200) {
        return RoomObservation::unknown("虎牙状态接口未确认房间；短号需网页核验");
    }
    let value = root["data"]["liveStatus"]
        .as_str()
        .unwrap_or("")
        .to_ascii_uppercase();
    let state = match value.as_str() {
        "ON" => RoomState::Live,
        "OFF" => RoomState::Offline,
        "REPLAY" => RoomState::Replay,
        _ => RoomState::Unknown,
    };
    RoomObservation::new(
        state,
        title(&root["data"]["roomName"]).or_else(|| title(&root["data"]["introduction"])),
    )
}
pub fn parse_douyu(root: &Value) -> RoomObservation {
    let room = &root["room"];
    let state = match number(&room["show_status"]) {
        Some(1) => RoomState::Live,
        Some(0 | 2) => {
            if number(&room["videoLoop"]) == Some(1) {
                RoomState::Replay
            } else {
                RoomState::Offline
            }
        }
        _ => RoomState::Unknown,
    };
    RoomObservation::new(state, title(&room["room_name"]))
}
pub fn parse_douyin_room(room: &Value) -> RoomObservation {
    let state = match number(&room["status"]) {
        Some(2) => RoomState::Live,
        Some(4) => RoomState::Offline,
        _ => RoomState::Unknown,
    };
    RoomObservation::new(state, title(&room["title"]))
}
pub fn parse_douyin_page(page: &str) -> RoomObservation {
    for payload in platforms::pace_payloads(page) {
        let json = payload
            .find(':')
            .filter(|i| payload[..*i].chars().all(|c| c.is_ascii_alphanumeric()))
            .map_or(payload.as_str(), |i| &payload[i + 1..]);
        let Ok(root) = serde_json::from_str::<Value>(json) else {
            continue;
        };
        let entries: Vec<&Value> = root
            .as_array()
            .map(|a| a.iter().collect())
            .unwrap_or_else(|| vec![&root]);
        for entry in entries {
            if let Some(room) = entry.pointer("/state/roomStore/roomInfo/room") {
                return parse_douyin_room(room);
            }
        }
    }
    RoomObservation::unknown("抖音未返回可识别房间状态，可能需要网页验证")
}
/// 短号（a16789 安德罗妮、clkimmy 等）在 mp.huya.com 的 profileRoom 接口里回
/// status=422「该主播不存在」。房间页 HTML 里带着数字房号（"profileRoom":528300），
/// 拿数字号再问一次接口才能读到真实开播状态。页面改版/取不到时返回 None，
/// 调用方退回页面标记判断（页面 liveStatus-on 对重播也成立，不能单独当「在播」）。
fn huya_numeric_room_id(page: &str) -> Option<String> {
    const KEY: &str = "\"profileRoom\"";
    let start = page.find(KEY)? + KEY.len();
    let rest = page[start..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"').unwrap_or(rest);
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    (!digits.is_empty()).then_some(digits)
}

/// 问一次虎牙四态接口；网络失败/结构异常一律 Unknown，交给调用方页面兜底。
fn huya_mobile_observation(room: &str) -> RoomObservation {
    let Ok(mut url) = Url::parse("https://mp.huya.com/cache.php") else {
        return RoomObservation::unknown("虎牙状态接口地址无效");
    };
    url.query_pairs_mut()
        .extend_pairs([("m", "Live"), ("do", "profileRoom"), ("roomid", room)]);
    match platforms::json_get(url, "https://www.huya.com/") {
        Ok(root) => parse_huya(&root),
        Err(_) => RoomObservation::unknown("虎牙状态接口查询失败"),
    }
}

fn query(target: &Target) -> Result<RoomObservation> {
    Target::new(target.platform, target.room.clone())?;
    match target.platform {
        Platform::Bili => {
            let mut url = Url::parse("https://api.live.bilibili.com/room/v1/Room/get_info")?;
            url.query_pairs_mut().append_pair("room_id", &target.room);
            Ok(parse_bili(&platforms::json_get(
                url,
                "https://live.bilibili.com/",
            )?))
        }
        Platform::Huya => {
            let observed = huya_mobile_observation(&target.room);
            if observed.state != RoomState::Unknown {
                return Ok(observed);
            }
            let page = platforms::get(
                Url::parse(&format!("https://www.huya.com/{}", target.room))?,
                "https://www.huya.com/",
                None,
            )?;
            // 2026-09-19 的原版坑：短号房间（a16789 安德罗妮、clkimmy 等）在
            // mp.huya.com/cache.php 里直接回 status=422「该主播不存在」。原版
            // 的做法是先从房间页取数字房号（"profileRoom":528300）再问接口——
            // 只看页面标记不行：页面把「重播中」也标成 liveStatus-on（2026-10-16
            // 实测），把「在播」当结论就会把重播谎报成直播。
            if let Some(numeric) = huya_numeric_room_id(&page) {
                if numeric != target.room {
                    let resolved = huya_mobile_observation(&numeric);
                    if resolved.state != RoomState::Unknown {
                        return Ok(RoomObservation::new(
                            resolved.state,
                            resolved.title.or(observed.title),
                        ));
                    }
                }
            }
            // Only unambiguous replay/off markers may be used as fallback;
            // Huya's web "on" marker also appears on replay pages.
            let state = if page.contains("liveStatus-replay") {
                RoomState::Replay
            } else if page.contains("liveStatus-off") {
                RoomState::Offline
            } else {
                RoomState::Unknown
            };
            Ok(RoomObservation::new(state, observed.title))
        }
        Platform::Douyu => Ok(parse_douyu(&platforms::json_get(
            Url::parse(&format!("https://www.douyu.com/betard/{}", target.room))?,
            "https://www.douyu.com/",
        )?)),
        Platform::Douyin => {
            // Status/title lookups share the same gated, route-failover read as
            // playback (with the 4 s same-room short reuse); a bare pinned GET
            // would miss the captcha/shell recovery and the request gate.
            let page = platforms::douyin_room_page_shared(&target.room, None)?;
            Ok(parse_douyin_page(&page))
        }
    }
}
pub fn check(target: &Target, vault: Option<&CookieVault>) -> Result<RoomObservation> {
    ensure!(!target.room.is_empty(), "房间号不能为空");
    match vault {
        Some(vault) => {
            platforms::with_authenticated_session(target.platform, vault, || query(target))
        }
        None => query(target),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn unknown_is_not_offline() {
        for root in [
            json!({}),
            json!({"code":0,"data":{}}),
            json!({"code":0,"data":{"live_status":777}}),
        ] {
            assert_eq!(parse_bili(&root).state, RoomState::Unknown)
        }
        assert_eq!(
            parse_douyu(&json!({"room":{"show_status":999}})).state,
            RoomState::Unknown
        );
        assert_eq!(
            parse_douyin_room(&json!({"status":0})).state,
            RoomState::Unknown
        );
    }
    #[test]
    fn replay_is_distinct_from_offline() {
        assert_eq!(
            parse_bili(&json!({"code":0,"data":{"live_status":2}})).state,
            RoomState::Replay
        );
        assert_eq!(
            parse_huya(&json!({"status":200,"data":{"liveStatus":"REPLAY"}})).state,
            RoomState::Replay
        );
        assert_eq!(
            parse_douyu(&json!({"room":{"show_status":2,"videoLoop":1}})).state,
            RoomState::Replay
        );
        assert_eq!(
            parse_douyu(&json!({"room":{"show_status":1,"videoLoop":1}})).state,
            RoomState::Live
        );
    }
    #[test]
    fn huya_short_room_page_yields_the_numeric_room_id() {
        // 真实页面片段：短号房间只在 HTML 里带数字房号，接口按短号会回 422。
        let quoted = r#"<script>window.HNF_GLOBAL_INIT={"profileRoom":"528300","x":1}</script>"#;
        assert_eq!(huya_numeric_room_id(quoted).as_deref(), Some("528300"));
        let unquoted = r#"{"profileRoom":528300,"other":2}"#;
        assert_eq!(huya_numeric_room_id(unquoted).as_deref(), Some("528300"));
        assert_eq!(huya_numeric_room_id("no marker here"), None);
        assert_eq!(huya_numeric_room_id(r#"{"profileRoom":}"#), None);
        assert_eq!(huya_numeric_room_id(r#"{"profileRoom":"abc"}"#), None);
        assert_eq!(huya_numeric_room_id(""), None);
    }

    #[test]
    #[ignore = "访问真实虎牙接口；仅人工验收时运行"]
    fn manual_huya_short_room_reads_the_live_state_through_the_page_room_id() {
        // a16789（安德罗妮）是短号：profileRoom 按短号查询回 422，必须回退到页面
        // 里的数字房号再查，才能读到真实开播状态（2026-11-01 本机实测）。
        let target = Target::new(Platform::Huya, "a16789".to_string()).expect("valid target");
        let observed = check(&target, None).expect("real Huya query");
        println!(
            "a16789 -> {:?} title={:?} note={}",
            observed.state, observed.title, observed.note
        );
        assert_ne!(
            observed.state,
            RoomState::Unknown,
            "短号房间必须通过页面数字房号读到真实状态"
        );
    }
}
