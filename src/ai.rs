//! Explicit, paid, source-backed DeepSeek research. Platform cookies are never
//! loaded here. Model output is data, cannot trigger tools, and cannot favorite
//! or launch a room without the normal platform verification/user action.
use crate::{
    auth,
    core::{Platform, Target},
    login::Cancellation,
    network,
    platforms::LookupCandidate,
};
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{Read, Write},
    path::PathBuf,
    time::{Duration, Instant},
};
use url::Url;
const ENDPOINT: &str = "https://api.deepseek.com/anthropic/v1/messages";
const ENTROPY: &[u8] = b"LiveHubRust.DeepSeek.api.deepseek.com.v1";
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct AiConfig {
    pub model: String,
    pub max_tokens: u16,
}
impl Default for AiConfig {
    fn default() -> Self {
        Self {
            model: "deepseek-flash".into(),
            max_tokens: 4096,
        }
    }
}
impl AiConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.model.starts_with("deepseek-")
                && self.model.len() <= 80
                && self
                    .model
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
            "AI模型名称无效"
        );
        ensure!(
            (512..=8192).contains(&self.max_tokens),
            "AI输出上限应为512–8192 tokens"
        );
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtectedConfig {
    version: u32,
    config: AiConfig,
    key: String,
}
impl Drop for ProtectedConfig {
    fn drop(&mut self) {
        unsafe { self.key.as_bytes_mut().fill(0) }
    }
}
#[derive(Clone)]
pub struct AiVault {
    root: PathBuf,
}
impl AiVault {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    fn path(&self) -> PathBuf {
        self.root.join("deepseek.key.dpapi")
    }
    fn check_links(&self) -> Result<()> {
        for path in [&self.root, &self.path()] {
            if let Ok(meta) = fs::symlink_metadata(path) {
                ensure!(!meta.file_type().is_symlink(), "AI配置不能通过符号链接访问");
            }
        }
        Ok(())
    }
    fn read(&self) -> Result<Option<ProtectedConfig>> {
        self.check_links()?;
        let file = match File::open(self.path()) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(anyhow!("AI配置暂不可读")),
        };
        ensure!(file.metadata()?.len() <= 64 * 1024, "AI配置大小异常");
        let mut encrypted = Vec::new();
        file.take(64 * 1024 + 1).read_to_end(&mut encrypted)?;
        ensure!(encrypted.len() <= 64 * 1024, "AI配置大小异常");
        let mut plain = auth::unprotect(&encrypted, ENTROPY)?;
        let parsed = serde_json::from_slice::<ProtectedConfig>(&plain);
        plain.fill(0);
        let stored = parsed.map_err(|_| anyhow!("AI加密配置格式无效"))?;
        ensure!(stored.version == 1, "AI配置版本尚不支持");
        stored.config.validate()?;
        validate_key(&stored.key)?;
        Ok(Some(stored))
    }
    pub fn configuration(&self) -> Result<Option<AiConfig>> {
        Ok(self.read()?.map(|stored| stored.config.clone()))
    }
    pub fn save(&self, config: AiConfig, new_key: &str) -> Result<()> {
        config.validate()?;
        self.check_links()?;
        let key = if new_key.trim().is_empty() {
            self.read()?
                .ok_or_else(|| anyhow!("首次配置需要API Key"))?
                .key
                .clone()
        } else {
            new_key.trim().to_owned()
        };
        validate_key(&key)?;
        let stored = ProtectedConfig {
            version: 1,
            config,
            key,
        };
        let mut plain = serde_json::to_vec(&stored)?;
        let encrypted = auth::protect(&plain, ENTROPY);
        plain.fill(0);
        let encrypted = encrypted?;
        fs::create_dir_all(&self.root)?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".ai-key-")
            .tempfile_in(&self.root)?;
        temporary.write_all(&encrypted)?;
        temporary.as_file().sync_all()?;
        for delay in [5, 10, 20, 40, 80, 0] {
            match temporary.persist(self.path()) {
                Ok(_) => return Ok(()),
                Err(e) => {
                    if delay == 0 || !matches!(e.error.raw_os_error(), Some(5 | 32 | 33)) {
                        return Err(anyhow!("AI配置保存失败，原配置保留"));
                    }
                    temporary = e.file;
                    std::thread::sleep(Duration::from_millis(delay));
                }
            }
        }
        unreachable!()
    }
    pub fn clear(&self) -> Result<()> {
        self.check_links()?;
        match fs::remove_file(self.path()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(anyhow!("AI密钥清除失败")),
        }
    }
    pub fn research(
        &self,
        query: &str,
        existing: &[LookupCandidate],
        optimize: bool,
        cancel: &Cancellation,
    ) -> Result<ResearchResult> {
        let stored = self
            .read()?
            .ok_or_else(|| anyhow!("请先在AI设置中保存自己的DeepSeek API Key"))?;
        let body = request_body(&stored.config, query, existing, optimize)?;
        ensure!(!cancel.is_cancelled(), "AI请求已取消");
        let url = Url::parse(ENDPOINT)?;
        let client = network::client(&url, &["api.deepseek.com"], &[], Duration::from_secs(120))?;
        let mut value = reqwest::header::HeaderValue::from_str(&stored.key)
            .map_err(|_| anyhow!("AI密钥格式无效"))?;
        value.set_sensitive(true);
        let response = client
            .post(url)
            .header("x-api-key", value)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .map_err(|_| anyhow!("AI服务请求失败或超时，未采用部分结果"))?;
        ensure!(!cancel.is_cancelled(), "AI请求已取消");
        match response.status().as_u16() {
            401 | 403 => return Err(anyhow!("AI密钥无效或接口权限不足")),
            429 => return Err(anyhow!("AI服务限流，请稍后重试")),
            _ => ensure!(response.status().is_success(), "AI服务暂时不可用"),
        }
        ensure!(
            response.content_length().is_none_or(|n| n <= 1024 * 1024),
            "AI响应过大"
        );
        let started = Instant::now();
        let mut response = response;
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            ensure!(!cancel.is_cancelled(), "AI请求已取消");
            ensure!(
                started.elapsed() < Duration::from_secs(120),
                "AI响应读取超时"
            );
            let n = response
                .read(&mut buffer)
                .map_err(|_| anyhow!("AI响应读取失败"))?;
            if n == 0 {
                break;
            }
            ensure!(bytes.len() + n <= 1024 * 1024, "AI响应过大");
            bytes.extend_from_slice(&buffer[..n]);
        }
        parse_response(
            &serde_json::from_slice(&bytes).map_err(|_| anyhow!("AI服务未返回原生JSON"))?,
            existing,
            optimize,
        )
    }
}
fn validate_key(key: &str) -> Result<()> {
    ensure!(
        (8..=4096).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_graphic()),
        "API Key应为8–4096个不含空白的ASCII字符"
    );
    Ok(())
}
pub fn request_body(
    config: &AiConfig,
    query: &str,
    existing: &[LookupCandidate],
    optimize: bool,
) -> Result<Value> {
    config.validate()?;
    ensure!(
        !query.trim().is_empty() && query.chars().count() <= 512,
        "AI查询及补充线索合计应为1–512字"
    );
    let hints:Vec<_>=existing.iter().take(40).map(|c|json!({"platform":c.platform,"name":c.name,"room":c.room,"profile_url":c.profile_url})).collect();
    Ok(
        json!({"model":config.model,"max_tokens":config.max_tokens,"thinking":{"type":"disabled"},"stream":false,
        "system":"你是直播主播身份研究助手。只根据原生web_search结果提供线索，保留同名账号歧义，不猜测数字房间号。只返回JSON：{summary:string,candidates:[{platform:bili|huya|douyu|douyin,name:string,room:string,source_url:官方直播间或抖音账号主页URL,reason:有来源支持的匹配意见}],terms:[string]}。source_url必须实际出现在搜索结果。候选只是线索，客户端会独立查询平台核验，不自动收藏或播放。",
        "messages":[{"role":"user","content":[{"type":"text","text":serde_json::to_string(&json!({"query":query,"mode":if optimize{"优化并核对现有候选"}else{"重新搜索真实平台账号"},"existing_candidates":hints}))?}]}],
        "tools":[{"type":"web_search_20250305","name":"web_search","max_uses":5}]}),
    )
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchSource {
    pub title: String,
    pub url: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchResult {
    pub summary: String,
    pub candidates: Vec<LookupCandidate>,
    pub sources: Vec<ResearchSource>,
    pub terms: Vec<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
}
fn public_source(address: &str) -> Option<Url> {
    let url = Url::parse(address).ok()?;
    let host = url.host_str()?;
    if url.scheme() != "https"
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || host.parse::<std::net::IpAddr>().is_ok()
        || !host.contains('.')
        || host.ends_with(".local")
        || host.ends_with(".localhost")
        || address.len() > 2048
    {
        return None;
    }
    Some(url)
}
pub fn parse_response(
    root: &Value,
    existing: &[LookupCandidate],
    optimize: bool,
) -> Result<ResearchResult> {
    ensure!(
        root["stop_reason"].as_str() == Some("end_turn"),
        "AI研究没有完整结束，未采用截断结果"
    );
    let blocks = root["content"]
        .as_array()
        .ok_or_else(|| anyhow!("AI内容格式无效"))?;
    let mut sources = Vec::new();
    let mut source_urls = BTreeSet::new();
    let mut text = String::new();
    for block in blocks.iter().take(128) {
        match block["type"].as_str() {
            Some("text") => {
                if let Some(part) = block["text"].as_str() {
                    ensure!(text.len() + part.len() <= 128 * 1024, "AI文本超过限额");
                    text.push_str(part)
                }
            }
            Some("web_search_tool_result") => {
                if let Some(rows) = block["content"].as_array() {
                    for row in rows.iter().take(32) {
                        if row["type"] == "web_search_result" {
                            if let Some(url) = row["url"].as_str().and_then(public_source) {
                                if sources.len() < 32 && source_urls.insert(url.to_string()) {
                                    sources.push(ResearchSource {
                                        title: row["title"]
                                            .as_str()
                                            .unwrap_or("搜索来源")
                                            .chars()
                                            .take(200)
                                            .collect(),
                                        url: url.to_string(),
                                    });
                                }
                            }
                        }
                    }
                }
            }
            _ => (),
        }
    }
    ensure!(
        !sources.is_empty(),
        "AI未返回实际原生联网来源，未把模型记忆当作搜索结果"
    );
    let text = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let parsed: Value =
        serde_json::from_str(text).map_err(|_| anyhow!("AI结果不是完整候选JSON，原结果保留"))?;
    let mut candidates = Vec::new();
    let mut seen = BTreeSet::new();
    for row in parsed["candidates"]
        .as_array()
        .into_iter()
        .flatten()
        .take(40)
    {
        let Some(address) = row["source_url"].as_str().and_then(public_source) else {
            continue;
        };
        if !source_urls.contains(address.as_str()) {
            continue;
        }
        let platform = match row["platform"]
            .as_str()
            .and_then(|p| p.parse::<Platform>().ok())
        {
            Some(p) => p,
            None => continue,
        };
        let name = row["name"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(200)
            .collect::<String>();
        if name.is_empty() {
            continue;
        }
        if let Ok(target) = Target::parse(address.as_str()) {
            if target.platform != platform
                || row["room"]
                    .as_str()
                    .is_some_and(|room| !room.is_empty() && room != target.room)
            {
                continue;
            }
            if !seen.insert((platform, target.room.clone())) {
                continue;
            }
            let source = crate::platforms::LookupSource {
                url: address.to_string(), title: "AI研究引用（待平台核验）".into(),
                excerpt: row["reason"].as_str().unwrap_or("").chars().take(1000).collect(),
                ..Default::default()
            };
            let mut candidate = if optimize {
                // Optimising only re-ranks and annotates candidates the platform
                // search already returned; an unknown room is not appended here
                // because the caller keeps its own verified list.
                let Some(native) = existing
                    .iter()
                    .find(|native| native.platform == platform && native.room == target.room)
                    .cloned()
                else {
                    continue;
                };
                native
            } else {
                // A cited official room address becomes a direct-room candidate:
                // "input" selects the same verification path as a pasted link so
                // the AI citation is checked against the platform page, and the
                // canonical page is what verification/selection compares.
                LookupCandidate {
                    name,
                    room: target.room.clone(),
                    platform,
                    profile_url: Some(crate::search::room_input_url(platform, &target.room)),
                    provider: "input".into(),
                    ..Default::default()
                }
            };
            if !source.excerpt.is_empty() { candidate.match_reasons.push(format!("AI匹配意见（需核对）：{}", source.excerpt)); }
            if !candidate.sources.iter().any(|old| old.url == source.url) { candidate.sources.push(source); }
            candidates.push(candidate);
        } else if platform == Platform::Douyin
            && address.host_str() == Some("www.douyin.com")
        {
            if let Some(sec) = address.path().strip_prefix("/user/").filter(|s| {
                !s.is_empty()
                    && s.len() <= 256
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            }) {
                let profile_url = format!("https://www.douyin.com/user/{sec}");
                candidates.push(LookupCandidate {
                    name,
                    platform,
                    profile_url: Some(profile_url.clone()),
                    sec_uid: Some(sec.to_owned()),
                    provider: "input".into(),
                    match_reasons: vec!["AI引用的抖音账号主页；直播间归属待平台核验".into()],
                    sources: vec![crate::platforms::LookupSource {
                        url: profile_url,
                        title: "AI研究引用（抖音账号页）".into(),
                        excerpt: row["reason"].as_str().unwrap_or("").chars().take(1000).collect(),
                        retrieved_at: crate::platforms::retrieval_timestamp(),
                        supports: vec!["账号身份".into()],
                    }],
                    ..Default::default()
                });
            }
        }
    }
    if optimize {
        for native in existing {
            if !candidates.iter().any(|c| {
                c.platform == native.platform
                    && c.room == native.room
                    && c.profile_url == native.profile_url
            }) {
                candidates.push(native.clone());
            }
        }
    }
    let terms = parsed["terms"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .take(6)
        .map(|s| s.chars().take(80).collect())
        .collect();
    Ok(ResearchResult {
        summary: parsed["summary"]
            .as_str()
            .unwrap_or("研究完成，请核对来源与平台账号")
            .chars()
            .take(3000)
            .collect(),
        candidates,
        sources,
        terms,
        input_tokens: root["usage"]["input_tokens"].as_u64().unwrap_or(0),
        output_tokens: root["usage"]["output_tokens"].as_u64().unwrap_or(0),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn truncated_research_and_uncited_candidates_never_replace_native_results() {
        let native = LookupCandidate {
            platform: Platform::Bili,
            name: "synthetic same name".into(),
            room: "123".into(),
            ..Default::default()
        };
        let mut reply = response("https://live.bilibili.com/123", "123");
        reply["stop_reason"] = json!("max_tokens");
        assert!(parse_response(&reply, std::slice::from_ref(&native), true).is_err());
        let result = parse_response(
            &response("https://live.bilibili.com/456", "456"),
            std::slice::from_ref(&native),
            true,
        )
        .unwrap();
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].room, "123");
        let mut reply = response("https://live.bilibili.com/456", "456");
        reply["content"][1]["text"]=json!(json!({"candidates":[{"platform":"bili","name":"synthetic","room":"789","source_url":"https://live.bilibili.com/789"}]}).to_string());
        assert!(
            parse_response(&reply, &[], false)
                .unwrap()
                .candidates
                .is_empty()
        );
    }
    #[test]
    fn body_sends_only_public_hints_and_fixed_native_tool() {
        let body = request_body(&AiConfig::default(), "synthetic", &[], false).unwrap();
        assert_eq!(body["tools"][0]["type"], "web_search_20250305");
        assert!(!body.to_string().contains("Cookie"));
        assert!(request_body(&Default::default(), "", &[], false).is_err());
    }
    fn response(source: &str, room: &str) -> Value {
        json!({"stop_reason":"end_turn","content":[{"type":"web_search_tool_result","content":[{"type":"web_search_result","url":source,"title":"synthetic"}]},{"type":"text","text":json!({"summary":"synthetic","candidates":[{"platform":"bili","room":room,"name":"synthetic","source_url":source}]}).to_string()}]})
    }
    #[test]
    fn invented_rooms_or_uncited_model_urls_are_rejected() {
        assert!(
            parse_response(
                &json!({"stop_reason":"end_turn","content":[{"type":"text","text":"{}"}]}),
                &[],
                false
            )
            .is_err()
        );
        assert!(
            parse_response(
                &response("https://live.bilibili.com/123", "999"),
                &[],
                false
            )
            .unwrap()
            .candidates
            .is_empty()
        );
        assert_eq!(
            parse_response(
                &response("https://live.bilibili.com/123", "123"),
                &[],
                false
            )
            .unwrap()
            .candidates
            .len(),
            1
        );
        assert!(parse_response(&response("https://127.0.0.1/123", "123"), &[], false).is_err());
    }
    #[cfg(windows)]
    #[test]
    fn private_config_is_encrypted_and_invalid_save_keeps_original() {
        let dir = tempfile::tempdir().unwrap();
        let vault = AiVault::new(dir.path().join("private"));
        vault
            .save(Default::default(), "sk-SYNTHETIC-NOT-A-REAL-KEY")
            .unwrap();
        let bytes = fs::read(vault.path()).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("SYNTHETIC"));
        assert!(vault.save(Default::default(), "bad\nkey").is_err());
        assert_eq!(fs::read(vault.path()).unwrap(), bytes);
        assert_eq!(vault.configuration().unwrap(), Some(AiConfig::default()));
        vault.clear().unwrap();
        assert!(vault.configuration().unwrap().is_none());
    }
}
