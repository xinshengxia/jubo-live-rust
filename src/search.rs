//! Independent bounded platform searches: per-platform cache, hint-aware ranking
//! and honest display helpers. Partial failures remain visible and no name match
//! is ever treated as proof of identity.

use crate::{
    auth::CookieVault,
    core::Platform,
    platforms::{self, LookupCandidate, LookupSource},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, thread};

/// A cached platform result is shown for at most this long; a refresh bypasses it.
pub const SEARCH_CACHE_TTL_SECS: u64 = 600;
const SEARCH_CACHE_ENTRIES: usize = 64;
const MAX_CANDIDATES: usize = 80;
const MAX_QUERY_CHARS: usize = 200;
const MAX_HINT_CHARS: usize = 80;
/// User-supplied disambiguation hints appended to the original query text.
pub const HINT_GAME: &str = "游戏/领域";
pub const HINT_IDENTITY: &str = "战队/身份";
pub const HINT_ALIAS: &str = "外号/曾用名";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformOutcome {
    pub platform: Platform,
    pub success: bool,
    pub candidates: usize,
    pub note: String,
    /// True when this platform's list came from the recent per-platform cache.
    #[serde(default)]
    pub cached: bool,
    /// Unix seconds of the original query that produced this list.
    #[serde(default)]
    pub searched_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchBundle {
    pub candidates: Vec<LookupCandidate>,
    pub outcomes: Vec<PlatformOutcome>,
}

#[derive(Debug, Clone)]
pub struct CachedSearch {
    pub candidates: Vec<LookupCandidate>,
    pub searched_at: u64,
}

/// Recent per-platform search results. Failed and empty platform responses are
/// never cached, so the next query can retry only that platform.
#[derive(Debug, Default)]
pub struct SearchCache {
    entries: BTreeMap<String, CachedSearch>,
}

impl SearchCache {
    pub fn get(&self, key: &str, now: u64) -> Option<&CachedSearch> {
        let entry = self.entries.get(key)?;
        if now >= entry.searched_at && now - entry.searched_at < SEARCH_CACHE_TTL_SECS {
            Some(entry)
        } else {
            None
        }
    }

    pub fn insert(&mut self, key: String, candidates: Vec<LookupCandidate>, searched_at: u64) {
        if candidates.is_empty() {
            return;
        }
        if self.entries.len() >= SEARCH_CACHE_ENTRIES && !self.entries.contains_key(&key) {
            self.entries.clear();
        }
        self.entries.insert(
            key,
            CachedSearch {
                candidates,
                searched_at,
            },
        );
    }

    pub fn remove(&mut self, key: &str) -> bool {
        self.entries.remove(key).is_some()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// The user's nickname plus optional public disambiguation hints.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchQuery {
    pub name: String,
    pub game: String,
    pub identity: String,
    pub alias: String,
}

struct HintMarker {
    start: usize,
    value_start: usize,
    label: &'static str,
}

fn hint_markers(text: &str) -> Vec<HintMarker> {
    let mut markers = Vec::new();
    for (index, _) in text.char_indices() {
        let at_boundary = index == 0
            || text[..index]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
        if !at_boundary {
            continue;
        }
        for label in [HINT_GAME, HINT_IDENTITY, HINT_ALIAS] {
            let marker = format!("{label}：");
            if text[index..].starts_with(&marker) {
                markers.push(HintMarker {
                    start: index,
                    value_start: index + marker.len(),
                    label,
                });
                break;
            }
        }
    }
    markers.sort_by_key(|marker| marker.start);
    markers
}

impl SearchQuery {
    pub fn new(name: &str, game: &str, identity: &str, alias: &str) -> Self {
        let bound =
            |value: &str, max: usize| -> String { value.trim().chars().take(max).collect() };
        Self {
            name: bound(name, MAX_QUERY_CHARS),
            game: bound(game, MAX_HINT_CHARS),
            identity: bound(identity, MAX_HINT_CHARS),
            alias: bound(alias, MAX_HINT_CHARS),
        }
    }

    /// Splits a single input into the nickname and the three optional hints.
    /// Hint labels must start a whitespace-separated segment, matching the
    /// documented "昵称 游戏/领域：…" input shape.
    pub fn from_input(input: &str) -> Self {
        let text = input.trim();
        let markers = hint_markers(text);
        if markers.is_empty() {
            return Self::new(text, "", "", "");
        }
        let name = text[..markers[0].start].trim();
        let mut query = Self::new(name, "", "", "");
        for (position, marker) in markers.iter().enumerate() {
            let end = markers
                .get(position + 1)
                .map(|next| next.start)
                .unwrap_or(text.len());
            let value: String = text[marker.value_start..end]
                .trim()
                .chars()
                .take(MAX_HINT_CHARS)
                .collect();
            match marker.label {
                HINT_GAME if query.game.is_empty() => query.game = value,
                HINT_IDENTITY if query.identity.is_empty() => query.identity = value,
                HINT_ALIAS if query.alias.is_empty() => query.alias = value,
                _ => {}
            }
        }
        query
    }

    /// The text sent to a platform's own search: the nickname, falling back to
    /// the alias when no nickname was typed.
    pub fn platform_terms(&self) -> String {
        if !self.name.trim().is_empty() {
            self.name.trim().to_owned()
        } else {
            self.alias.trim().to_owned()
        }
    }

    /// Full query text including hints; used for ranking and as cache identity.
    pub fn text(&self) -> String {
        let mut text = self.name.trim().to_owned();
        for (label, value) in [
            (HINT_GAME, &self.game),
            (HINT_IDENTITY, &self.identity),
            (HINT_ALIAS, &self.alias),
        ] {
            if value.trim().is_empty() {
                continue;
            }
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(label);
            text.push('：');
            text.push_str(value.trim());
        }
        text
    }

    pub fn is_empty(&self) -> bool {
        self.platform_terms().is_empty()
    }

    /// Cache identity: the query scope plus the full query text.
    pub fn cache_key(&self, scope: &str) -> String {
        format!("{scope}\n{}", self.text())
    }
}

/// Extracts one documented hint value from free text (single-label helper).
pub fn hint<'a>(text: &'a str, label: &str) -> Option<&'a str> {
    let markers = hint_markers(text);
    let marker = markers.iter().find(|marker| marker.label == label)?;
    let end = markers
        .iter()
        .find(|next| next.start > marker.start)
        .map(|next| next.start)
        .unwrap_or(text.len());
    Some(text[marker.value_start..end].trim())
}

/// The nickname part of a query with hints stripped.
pub fn search_terms(text: &str) -> &str {
    let markers = hint_markers(text);
    match markers.first() {
        Some(marker) => text[..marker.start].trim(),
        None => text.trim(),
    }
}

fn decode_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

/// Removes markup and surrounding whitespace from platform-provided labels.
pub fn clean(value: &str) -> String {
    let mut output = String::new();
    let mut in_tag = false;
    for character in value.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => output.push(character),
            _ => {}
        }
    }
    decode_entities(&output).trim().to_owned()
}

/// Full-width/half-width and punctuation-insensitive comparison form. This is a
/// pragmatic normalization (no Unicode NFKC table), applied to short names only.
pub fn normalize(value: &str) -> String {
    let mut output = String::new();
    for character in clean(value).chars() {
        let mapped = match character {
            '\u{3000}' => ' ',
            '\u{FF01}'..='\u{FF5E}' => {
                char::from_u32(character as u32 - 0xFEE0).unwrap_or(character)
            }
            other => other,
        };
        if mapped.is_whitespace()
            || mapped.is_ascii_punctuation()
            || matches!(mapped, '_' | '·' | '•' | '・' | '—' | '–' | '～')
            || ('\u{3001}'..='\u{303F}').contains(&mapped)
        {
            continue;
        }
        for lower in mapped.to_lowercase() {
            output.push(lower);
        }
    }
    output
}

/// Whole-name match that also accepts a guild/team prefix separated from the
/// name. Arbitrary substring matches never qualify, and fan/clip prefixes are
/// rejected instead of being treated as the streamer.
pub fn alias_match(name: &str, term: &str) -> bool {
    let wanted = normalize(term);
    let full = normalize(name);
    if wanted.is_empty() {
        return false;
    }
    if wanted == full {
        return true;
    }
    let cleaned = clean(name);
    let parts: Vec<&str> = cleaned
        .split(|character: char| {
            matches!(
                character,
                '丶' | '·' | '•' | '・' | '|' | '｜' | ':' | '：' | '/' | '\\' | '—' | '–' | '-'
            )
        })
        .collect();
    if parts.len() < 2 {
        return false;
    }
    let prefix = normalize(parts[0]);
    if ["粉丝", "剪辑", "搬运", "高仿", "非本人", "fan"]
        .iter()
        .any(|rejected| prefix.contains(rejected))
    {
        return false;
    }
    (1..parts.len()).any(|index| normalize(&parts[index..].join("")) == wanted)
}

fn hint_matches(candidate: &LookupCandidate, query: &SearchQuery) -> bool {
    let game = normalize(&query.game);
    if !game.is_empty() && normalize(&candidate.category).contains(&game) {
        return true;
    }
    let identity = normalize(&query.identity);
    if !identity.is_empty() && normalize(&candidate.description).contains(&identity) {
        return true;
    }
    false
}

/// Ranking tier: lower is more relevant. A hint match only means the public
/// profile is consistent with the user's clue, never that the account is proven.
pub fn candidate_tier(candidate: &LookupCandidate, query: &SearchQuery) -> u8 {
    if hint_matches(candidate, query) {
        return 1;
    }
    let name = normalize(&query.platform_terms());
    if let Some(handle) = &candidate.account_handle {
        if !handle.trim().is_empty() && normalize(handle) == name {
            return 2;
        }
    }
    let display = normalize(&candidate.name);
    if display == name {
        return 2;
    }
    if alias_match(&candidate.name, &query.platform_terms()) {
        return 3;
    }
    if !query.alias.trim().is_empty() && alias_match(&candidate.name, &query.alias) {
        return 3;
    }
    if !name.is_empty() && display.contains(&name) {
        return 4;
    }
    5
}

pub fn relevance_label(candidate: &LookupCandidate, query: &SearchQuery) -> &'static str {
    // Keep the legacy C# ordering (Tier) but expose the possibility level so the
    // list order is understandable instead of looking arbitrary.
    match candidate_tier(candidate, query) {
        0 | 1 => "可能性高 · 线索更相符",
        2 => "可能性高 · 同名/抖音号一致",
        3 => "可能性中 · 别名相近",
        4 => "可能性低 · 关键词相近",
        _ => "可能性低 · 需人工核对",
    }
}

/// Stable ranking: tier, then platform order when grouped, then original order.
pub fn rank_candidates(candidates: &mut Vec<LookupCandidate>, query: &SearchQuery, grouped: bool) {
    let mut scored: Vec<(u8, Option<Platform>, usize, LookupCandidate)> = candidates
        .drain(..)
        .enumerate()
        .map(|(order, candidate)| {
            (
                candidate_tier(&candidate, query),
                grouped.then_some(candidate.platform),
                order,
                candidate,
            )
        })
        .collect();
    scored.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(&right.2))
    });
    candidates.extend(scored.into_iter().map(|(_, _, _, candidate)| candidate));
}

/// The display label for one candidate's verification stage.
pub fn candidate_stage(candidate: &LookupCandidate) -> &'static str {
    if candidate.room.trim().is_empty() {
        "找到账号，房间待解析"
    } else {
        "已取得房间"
    }
}

pub fn identity_text(candidate: &LookupCandidate) -> String {
    let mut text = candidate.platform.label().to_owned();
    if candidate.room.trim().is_empty() {
        text.push_str(" · 房间待解析");
    } else {
        text.push_str(&format!(" · 房间 {}", candidate.room));
    }
    if let Some(account) = &candidate.account_id {
        if !account.trim().is_empty() {
            text.push_str(&format!(" · 账号 {account}"));
        }
    } else if let Some(handle) = &candidate.account_handle {
        if !handle.trim().is_empty() {
            text.push_str(&format!(" · 账号 {handle}"));
        }
    }
    text
}

pub fn candidate_summary(candidate: &LookupCandidate) -> String {
    let mut parts: Vec<String> = Vec::new();
    let category = clean(&candidate.category);
    if !category.is_empty() {
        parts.push(category);
    }
    if candidate.room.trim().is_empty() {
        parts.push(candidate_stage(candidate).to_owned());
    } else {
        parts.push(format!("房间 {}", candidate.room));
    }
    if let Some(followers) = candidate.follower_count {
        parts.push(format!("粉丝 {}", followers_text(followers)));
    }
    let description = clean(&candidate.description);
    if !description.is_empty() {
        parts.push(description.chars().take(60).collect());
    } else if !candidate.title.trim().is_empty() {
        parts.push(clean(&candidate.title).chars().take(60).collect());
    }
    parts.join(" · ")
}

/// Official account homepage only; room links are never labeled as accounts.
pub fn account_page(candidate: &LookupCandidate) -> Option<String> {
    if let Some(profile) = &candidate.profile_url {
        let account_url = profile.starts_with("https://space.bilibili.com/")
            || profile.starts_with("https://www.douyin.com/user/");
        if account_url && profile.len() <= 512 {
            return Some(profile.clone());
        }
    }
    if candidate.platform == Platform::Bili {
        if let Some(account) = &candidate.account_id {
            if !account.is_empty()
                && account.len() <= 24
                && account.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Some(format!("https://space.bilibili.com/{account}"));
            }
        }
    }
    None
}

/// Official public live page built from a verified numeric room only.
pub fn room_page(candidate: &LookupCandidate) -> Option<String> {
    if candidate.room.trim().is_empty()
        || candidate.room.len() > 24
        || !candidate.room.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    Some(match candidate.platform {
        Platform::Bili => format!("https://live.bilibili.com/{}", candidate.room),
        Platform::Huya => format!("https://www.huya.com/{}", candidate.room),
        Platform::Douyu => format!("https://www.douyu.com/{}", candidate.room),
        Platform::Douyin => format!("https://live.douyin.com/{}", candidate.room),
    })
}

pub fn followers_text(count: u64) -> String {
    if count >= 100_000_000 {
        format!("{:.1}亿", count as f64 / 100_000_000.0)
    } else if count >= 10_000 {
        format!("{:.1}万", count as f64 / 10_000.0)
    } else {
        count.to_string()
    }
}

/// Actionable next step for a failed search, without inventing a cause.
pub fn recovery_hint(message: &str) -> String {
    let lower = message.to_lowercase();
    let hint = if lower.contains("限流") || lower.contains("rate") {
        "平台临时限流或风控，请稍后刷新结果；不要连续重复请求。"
    } else if lower.contains("登录")
        || lower.contains("验证")
        || lower.contains("auth")
        || lower.contains("受限")
        || lower.contains("sign")
    {
        "可到“账号与登录”页完成正常验证后重试，或打开平台网页。"
    } else if lower.contains("超时")
        || lower.contains("timeout")
        || lower.contains("网络")
        || lower.contains("network")
        || lower.contains("connect")
    {
        "网络受限或超时，可刷新结果重试；已有候选会保留。"
    } else if lower.contains("房间") {
        "可核对官方房间号，或粘贴直播间链接后重试。"
    } else {
        "可刷新结果重试、打开平台网页，或用 AI 搜索补充线索。"
    };
    hint.to_owned()
}

/// One platform's raw result; the message is already display-safe.
pub type PlatformSearchResult = std::result::Result<Vec<LookupCandidate>, String>;

fn valid_sec_uid(value: &str) -> bool {
    value.starts_with("MS4wLj")
        && (7..=186).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn valid_douyin_handle(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

/// Recognizes an explicitly supplied Douyin account clue without any network
/// request: a profile URL, a sec_uid, or the documented "抖音号:"/"douyin:"
/// prefix. The candidate stays in the account stage (no invented room).
pub fn douyin_account_candidate(input: &str) -> Option<LookupCandidate> {
    let text = input.trim();
    let profile = |sec: &str| -> Option<LookupCandidate> {
        if !valid_sec_uid(sec) {
            return None;
        }
        Some(LookupCandidate {
            name: sec.chars().take(64).collect(),
            room: String::new(),
            platform: Platform::Douyin,
            title: String::new(),
            profile_url: Some(format!("https://www.douyin.com/user/{sec}")),
            account_id: None,
            avatar_url: None,
            follower_count: None,
            category: String::new(),
            description: String::new(),
            account_handle: None,
            sec_uid: Some(sec.to_owned()),
            provider: "input".into(),
            match_reasons: vec!["用户提供的抖音账号主页线索，直播房间待解析".into()],
            sources: vec![LookupSource {
                url: format!("https://www.douyin.com/user/{sec}"),
                title: "用户提供的抖音账号主页".into(),
                excerpt: String::new(),
                retrieved_at: platforms::retrieval_timestamp(),
                supports: vec!["账号身份".into()],
            }],
            ..Default::default()
        })
    };
    if let Some(rest) = text.strip_prefix("https://www.douyin.com/user/") {
        let sec = rest
            .split(['?', '#'])
            .next()
            .unwrap_or("")
            .trim_end_matches('/');
        return profile(sec);
    }
    if valid_sec_uid(text) {
        return profile(text);
    }
    let handle = text
        .strip_prefix("douyin:")
        .or_else(|| text.strip_prefix("抖音号:"))
        .or_else(|| text.strip_prefix("抖音号："))
        .map(str::trim)?;
    if !valid_douyin_handle(handle) {
        return None;
    }
    Some(LookupCandidate {
        name: handle.to_owned(),
        room: String::new(),
        platform: Platform::Douyin,
        title: "抖音号线索；需主页链接或直播分享链接核验".into(),
        profile_url: None,
        account_id: None,
        avatar_url: None,
        follower_count: None,
        category: String::new(),
        description: String::new(),
        account_handle: Some(handle.to_owned()),
        provider: "input".into(),
        match_reasons: vec!["用户输入的抖音号线索，需主页或直播分享链接核验".into()],
        ..Default::default()
    })
}

/// Actionable hint for a Douyin search failure that the page-based assist can fix.
/// Only Douyin has that path; other platforms keep their existing recovery text.
pub fn douyin_web_assist_hint(platform: Platform, error: &str) -> Option<&'static str> {
    if platform != Platform::Douyin {
        return None;
    }
    let lower = error.to_lowercase();
    let verification = ["验证", "登录", "auth", "verify", "风控", "受限", "sign"]
        .iter()
        .any(|marker| lower.contains(marker));
    if verification {
        return Some(
            "可点击“抖音网页辅助”：在本程序独立浏览器完成登录/验证后，用同一关键词重新搜索。",
        );
    }
    let transient = ["限流", "rate", "超时", "timeout", "网络", "network", "空"]
        .iter()
        .any(|marker| lower.contains(marker));
    transient.then_some("可稍后刷新；若反复失败，可用“抖音网页辅助”在独立浏览器完成验证后重试。")
}

/// Queries the given platforms concurrently; per-platform failures stay separate.
pub fn query_platforms(
    platforms: &[Platform],
    keyword: &str,
    vault: &CookieVault,
) -> Vec<(Platform, PlatformSearchResult)> {
    if platforms.is_empty() {
        return Vec::new();
    }
    let query = keyword.trim().to_owned();
    thread::scope(|scope| {
        let jobs: Vec<_> = platforms
            .iter()
            .map(|platform| {
                let platform = *platform;
                let vault = vault.clone();
                let query = query.clone();
                scope.spawn(move || {
                    (
                        platform,
                        platforms::search_authenticated(platform, &query, &vault)
                            .map_err(|error| format!("{error:#}")),
                    )
                })
            })
            .collect();
        jobs.into_iter()
            .enumerate()
            .map(|(index, job)| {
                job.join()
                    .unwrap_or_else(|_| (platforms[index], Err("平台查询工作线程异常".to_owned())))
            })
            .collect()
    })
}

pub fn all(keyword: &str, vault: &CookieVault) -> Result<SearchBundle> {
    let query = SearchQuery::from_input(keyword);
    ensure!(
        !query.is_empty() && query.platform_terms().chars().count() <= MAX_QUERY_CHARS,
        "请输入1–200字的查询词"
    );
    let responses = query_platforms(&Platform::ALL, &query.platform_terms(), vault);
    Ok(combine(&query, responses, true))
}

/// All-platform search with per-platform cache reuse. Only successful,
/// non-empty platform lists are cached; a refresh bypasses every entry.
pub fn all_cached(
    query: &SearchQuery,
    vault: &CookieVault,
    cache: &mut SearchCache,
    now: u64,
    refresh: bool,
) -> Result<SearchBundle> {
    ensure!(
        !query.is_empty() && query.platform_terms().chars().count() <= MAX_QUERY_CHARS,
        "请输入1–200字的查询词"
    );
    let mut responses: Vec<(Platform, PlatformSearchResult)> = Vec::new();
    let mut cached_at = BTreeMap::new();
    let mut missing = Vec::new();
    for platform in Platform::ALL {
        if !refresh {
            let key = query.cache_key(&platform.to_string());
            if let Some(hit) = cache.get(&key, now) {
                cached_at.insert(platform, hit.searched_at);
                responses.push((platform, Ok(hit.candidates.clone())));
                continue;
            }
        }
        missing.push(platform);
    }
    responses.extend(query_platforms(&missing, &query.platform_terms(), vault));
    // Cache fresh successful platform lists before any combined truncation.
    if !refresh {
        for (platform, response) in &responses {
            if cached_at.contains_key(platform) {
                continue;
            }
            if let Ok(candidates) = response {
                cache.insert(
                    query.cache_key(&platform.to_string()),
                    candidates.clone(),
                    now,
                );
            }
        }
    }
    let mut bundle = combine(query, responses, true);
    for outcome in &mut bundle.outcomes {
        if let Some(at) = cached_at.get(&outcome.platform) {
            if outcome.success {
                outcome.cached = true;
                outcome.searched_at = *at;
                outcome.note = "显示近期缓存；点击刷新结果可重新检索。".to_owned();
            }
        } else {
            outcome.searched_at = now;
        }
    }
    Ok(bundle)
}

/// Combines per-platform responses, ranks the retained candidates and reports
/// every platform separately. Failed platforms never discard other results.
pub fn combine(
    query: &SearchQuery,
    responses: Vec<(Platform, PlatformSearchResult)>,
    grouped: bool,
) -> SearchBundle {
    let mut output = SearchBundle {
        candidates: Vec::new(),
        outcomes: Vec::new(),
    };
    for (platform, response) in responses {
        match response {
            Ok(mut candidates) => {
                candidates.retain(|candidate| candidate.platform == platform);
                output.outcomes.push(PlatformOutcome {
                    platform,
                    success: true,
                    candidates: candidates.len(),
                    note: if candidates.is_empty() {
                        "平台返回空结果".into()
                    } else {
                        "平台原生结果".into()
                    },
                    cached: false,
                    searched_at: 0,
                });
                output.candidates.extend(candidates);
            }
            Err(_) => output.outcomes.push(PlatformOutcome {
                platform,
                success: false,
                candidates: 0,
                note: "查询受阻，可单独重试或检查此平台登录状态".into(),
                cached: false,
                searched_at: 0,
            }),
        }
    }
    rank_candidates(&mut output.candidates, query, grouped);
    output.candidates.truncate(MAX_CANDIDATES);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn candidate(platform: Platform, name: &str, room: &str) -> LookupCandidate {
        LookupCandidate {
            name: name.into(),
            room: room.into(),
            platform,
            ..Default::default()
        }
    }

    #[test]
    fn hints_are_parsed_only_at_whitespace_separated_segments() {
        let query = SearchQuery::from_input(
            "水晶哥 游戏/领域：英雄联盟 战队/身份：职业选手 外号/曾用名：K神",
        );
        assert_eq!(query.name, "水晶哥");
        assert_eq!(query.game, "英雄联盟");
        assert_eq!(query.identity, "职业选手");
        assert_eq!(query.alias, "K神");
        assert_eq!(query.platform_terms(), "水晶哥");
        assert!(query.text().contains("游戏/领域：英雄联盟"));
        assert!(query.cache_key("bili").starts_with("bili\n"));
        // A label at the start of the input is still a hint (same as the C# regex);
        // without a nickname it cannot fall back to an alias, so it is unusable.
        let plain = SearchQuery::from_input("游戏/领域：合成");
        assert!(plain.name.is_empty());
        assert_eq!(plain.game, "合成");
        assert!(plain.platform_terms().is_empty());
        // Alias-only input is still searchable.
        let alias = SearchQuery::from_input("外号/曾用名：K神");
        assert_eq!(alias.platform_terms(), "K神");
        assert_eq!(hint("浪D 外号/曾用名：浪弟", HINT_ALIAS), Some("浪弟"));
        assert_eq!(search_terms("浪D 游戏/领域：合成"), "浪D");
        assert_eq!(
            SearchQuery::new(" a ", " b ", "", "").text(),
            "a 游戏/领域：b"
        );
    }

    #[test]
    fn normalization_handles_case_width_and_punctuation() {
        assert_eq!(normalize("ＳＹＮＴＨＥＴＩＣ"), "synthetic");
        assert_eq!(normalize("Ａｂｃ·Ｄ"), "abcd");
        assert_eq!(normalize("<em>Name</em> &amp; X"), "namex");
        assert_eq!(normalize("   "), "");
        assert_eq!(clean("<em>A</em> &quot;B&quot;"), "A \"B\"");
    }

    #[test]
    fn alias_matching_separates_guild_prefixes_and_rejects_fan_pages() {
        assert!(alias_match("微竞-浪D", "浪D"));
        assert!(alias_match("浪D", "浪D"));
        assert!(alias_match("公会丶浪D", "浪D"));
        assert!(!alias_match("浪D粉丝团", "浪D"));
        assert!(!alias_match("剪辑-浪D", "浪D"));
        assert!(!alias_match("小浪D", "浪D"));
        assert!(!alias_match("浪D", ""));
        assert!(alias_match("SYNTHETIC.Name", "syntheticname"));
    }

    #[test]
    fn ranking_puts_hint_matches_first_but_keeps_every_account() {
        let mut same_name = candidate(Platform::Huya, "浪D", "196645");
        same_name.category = "英雄联盟".into();
        same_name.description = "职业选手".into();
        // The alias candidate has no public clue match, so it ranks by alias only.
        let alias = candidate(Platform::Huya, "微竞-浪D", "2");
        let fan = candidate(Platform::Bili, "浪D粉丝团", "3");
        let mut candidates = vec![fan, alias, same_name];
        let query = SearchQuery::new("浪D", "英雄联盟", "职业选手", "");
        rank_candidates(&mut candidates, &query, false);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].name, "浪D");
        assert_eq!(candidate_tier(&candidates[0], &query), 1);
        assert_eq!(
            relevance_label(&candidates[0], &query),
            "可能性高 · 线索更相符"
        );
        assert_eq!(candidates[2].name, "浪D粉丝团");
        assert_eq!(
            relevance_label(&candidates[2], &query),
            "可能性低 · 关键词相近"
        );
        let mut grouped = vec![
            candidate(Platform::Douyin, "浪D", "9"),
            candidate(Platform::Bili, "浪D", "1"),
        ];
        rank_candidates(&mut grouped, &query, true);
        assert_eq!(grouped[0].platform, Platform::Bili);
    }

    #[test]
    fn cache_respects_ttl_refresh_and_never_stores_empty_results() {
        let mut cache = SearchCache::default();
        let key = "bili\nsynthetic".to_owned();
        assert!(cache.get(&key, 1_000).is_none());
        cache.insert(
            key.clone(),
            vec![candidate(Platform::Bili, "synthetic", "1")],
            1_000,
        );
        assert_eq!(cache.get(&key, 1_000).unwrap().candidates.len(), 1);
        assert!(cache.get(&key, 1_599).is_some());
        assert!(cache.get(&key, 1_600).is_none());
        assert!(cache.remove(&key));
        assert!(cache.is_empty());
        cache.insert(key.clone(), Vec::new(), 1_000);
        assert!(cache.get(&key, 1_001).is_none());
        for index in 0..(SEARCH_CACHE_ENTRIES + 5) {
            cache.insert(
                format!("key-{index}"),
                vec![candidate(Platform::Huya, "x", "1")],
                1_000,
            );
        }
        assert!(cache.len() <= SEARCH_CACHE_ENTRIES);
    }

    #[test]
    fn partial_platform_failure_does_not_discard_verified_results() {
        let source = candidate(Platform::Bili, "Synthetic", "123");
        let bundle = combine(
            &SearchQuery::new("synthetic", "", "", ""),
            vec![
                (Platform::Bili, Ok(vec![source])),
                (Platform::Douyin, Err("synthetic".to_owned())),
            ],
            true,
        );
        assert_eq!(bundle.candidates.len(), 1);
        assert!(bundle.outcomes[0].success);
        assert!(!bundle.outcomes[1].success);
        assert!(!bundle.outcomes[1].cached);
    }

    #[test]
    fn display_helpers_stay_honest_about_account_only_results() {
        let mut account = candidate(Platform::Douyin, "Synthetic", "");
        account.account_id = Some("123456".into());
        account.category = "游戏".into();
        assert_eq!(candidate_stage(&account), "找到账号，房间待解析");
        assert!(identity_text(&account).contains("房间待解析"));
        assert!(candidate_summary(&account).contains("找到账号，房间待解析"));
        assert!(candidate_summary(&account).contains("游戏"));
        let mut room = candidate(Platform::Bili, "Synthetic", "123");
        assert_eq!(candidate_stage(&room), "已取得房间");
        room.account_id = Some("456".into());
        assert_eq!(
            account_page(&room).as_deref(),
            Some("https://space.bilibili.com/456")
        );
        assert!(account_page(&room).as_deref() == Some("https://space.bilibili.com/456"));
        room.profile_url = Some("https://live.bilibili.com/123".into());
        assert!(
            account_page(&room).is_some(),
            "B站 UID still yields the space page"
        );
        assert_eq!(
            room_page(&room).as_deref(),
            Some("https://live.bilibili.com/123")
        );
        assert!(room_page(&candidate(Platform::Douyin, "x", "")).is_none());
        assert_eq!(followers_text(0), "0");
        assert_eq!(followers_text(12_345), "1.2万");
        assert_eq!(followers_text(250_000_000), "2.5亿");
        assert!(recovery_hint("平台限流").contains("限流"));
        assert!(recovery_hint("登录失效").contains("账号与登录"));
        assert!(recovery_hint("请求超时").contains("刷新"));
        assert!(recovery_hint("unknown").contains("刷新"));
    }

    #[test]
    fn numeric_followers_preserve_zero_but_never_guess_from_formatted_heat() {
        assert_eq!(
            platforms::candidate_followers(Platform::Bili, &json!({"attentions":0})),
            Some(0)
        );
        assert_eq!(
            platforms::candidate_followers(Platform::Douyin, &json!({"follower_count":"12345"})),
            Some(12345)
        );
        assert_eq!(
            platforms::candidate_followers(Platform::Huya, &json!({"fans":"9万"})),
            None
        );
    }

    #[test]
    fn ranking_tiers_prefer_exact_handle_then_alias_then_substring() {
        let query = SearchQuery::new("Synthetic", "", "", "");
        let mut handle = candidate(Platform::Douyin, "Nickname", "");
        handle.account_handle = Some("synthetic".into());
        let exact = candidate(Platform::Bili, "Synthetic", "1");
        let alias = candidate(Platform::Huya, "公会-Synthetic", "2");
        let substring = candidate(Platform::Douyu, "SyntheticFan", "3");
        let mut all = vec![substring.clone(), alias.clone(), exact, handle];
        rank_candidates(&mut all, &query, false);
        assert_eq!(candidate_tier(&all[0], &query), 2);
        assert_eq!(candidate_tier(&all[1], &query), 2);
        assert_eq!(candidate_tier(&alias, &query), 3);
        assert_eq!(candidate_tier(&substring, &query), 4);
    }

    #[test]
    fn douyin_account_inputs_become_account_stage_candidates_without_a_room() {
        let sec = "MS4wLjSyntheticAccount_A";
        for input in [
            format!("https://www.douyin.com/user/{sec}"),
            format!("https://www.douyin.com/user/{sec}?from=web"),
            sec.to_owned(),
        ] {
            let candidate = douyin_account_candidate(&input).unwrap();
            assert_eq!(candidate.platform, Platform::Douyin);
            assert!(candidate.room.is_empty());
            assert_eq!(
                candidate.profile_url.as_deref(),
                Some(format!("https://www.douyin.com/user/{sec}").as_str())
            );
            assert_eq!(candidate_stage(&candidate), "找到账号，房间待解析");
        }
        let handle = douyin_account_candidate("抖音号:synthetic_handle").unwrap();
        assert_eq!(handle.account_handle.as_deref(), Some("synthetic_handle"));
        assert!(handle.room.is_empty());
        assert!(douyin_account_candidate("https://live.douyin.com/123").is_none());
        assert!(douyin_account_candidate("https://example.invalid/user/MS4wLjX").is_none());
        assert!(douyin_account_candidate("抖音号:bad handle").is_none());
    }

    #[test]
    fn douyin_failures_offer_the_page_assist_only_for_that_platform() {
        let hint = douyin_web_assist_hint(Platform::Douyin, "平台要求登录或网页验证")
            .expect("verification is what the page assist can solve");
        assert!(hint.contains("抖音网页辅助"));
        assert!(
            douyin_web_assist_hint(Platform::Douyin, "平台临时限流")
                .unwrap()
                .contains("网页辅助")
        );
        assert!(douyin_web_assist_hint(Platform::Huya, "平台要求登录").is_none());
        assert!(douyin_web_assist_hint(Platform::Douyin, "unexpected shape").is_none());
    }
}
