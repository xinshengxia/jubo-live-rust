use super::*;
use crate::platforms::{LookupCandidate, parse_douyin_search};
use base64::Engine;
use std::collections::HashSet;

fn matches_search_response(address: &str, keyword: &str) -> bool {
    let Ok(url) = Url::parse(address) else { return false };
    let mut keys = HashSet::new();
    url.scheme() == "https" && url.host_str() == Some("www.douyin.com")
        && url.port().is_none() && url.username().is_empty() && url.password().is_none()
        && url.path().trim_end_matches('/') == "/aweme/v1/web/discover/search"
        && url.query_pairs().all(|(key, value)| {
            (key != "offset" || value == "0") && keys.insert(key.into_owned())
        })
        && url.query_pairs().any(|(key, value)| key == "keyword" && value == keyword)
        && url.query_pairs().any(|(key, value)| key == "search_channel" && value == "aweme_user_web")
}

impl BrowserLogin {
    /// Read only completed user-search responses for the requested keyword in this owned window.
    pub fn search_douyin(&mut self, keyword: &str, cancellation: &Cancellation) -> Result<Vec<LookupCandidate>> {
        ensure!(self.platform == Platform::Douyin, "网页搜索需要抖音专属窗口");
        let mut page = Url::parse(&crate::search::platform_search_url(Platform::Douyin, keyword)?)?;
        page.query_pairs_mut().append_pair("type", "user");
        let cancel = CancelCheck::Token(cancellation);
        let ready_by = Instant::now() + Duration::from_secs(30);
        let (mut socket, session) = loop {
            cancel.check()?;
            let ready = self.process.endpoint(cancel).and_then(|endpoint| {
                let mut socket = self.process.open_debugger(&endpoint, cancel)?;
                let (_, session) = attach_platform_page(&mut socket, self.platform, cancel)?;
                Ok((socket, session))
            });
            match ready {
                Ok(ready) => break ready,
                Err(error) if Instant::now() >= ready_by => return Err(error),
                Err(_) => thread::sleep(Duration::from_millis(200)),
            }
        };
        rpc(&mut socket, 3, "Network.enable", serde_json::json!({}), Some(&session), cancel, CDP_TIMEOUT)?;
        // Events can arrive before Page.navigate acknowledges, so process both on the same loop.
        let navigate = serde_json::json!({"id":4,"method":"Page.navigate","sessionId":session,
            "params":{"url":page.as_str()}});
        socket.send(Message::Text(navigate.to_string().into()))?;
        let mut pending = HashSet::new();
        let deadline = Instant::now() + Duration::from_secs(180);
        while Instant::now() < deadline {
            cancel.check()?;
            let message = match socket.read() {
                Ok(Message::Text(text)) => text,
                Ok(Message::Close(_)) => bail!("抖音网页搜索窗口已关闭"),
                Ok(_) => continue,
                Err(tungstenite::Error::Io(error)) if matches!(error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
                Err(_) => bail!("抖音网页搜索窗口连接已结束"),
            };
            let event: Value = serde_json::from_str(&message)?;
            if event["sessionId"].as_str() != Some(session.as_str()) { continue; }
            if event["id"] == 4 {
                ensure!(event.get("error").is_none() && event["result"].get("errorText").is_none(), "无法打开抖音用户搜索页");
            }
            let params = &event["params"];
            let Some(id) = params["requestId"].as_str() else { continue };
            match event["method"].as_str() {
                Some("Network.responseReceived") if pending.len() < 32 => {
                    let response = &params["response"];
                    if response["status"].as_f64() == Some(200.0)
                        && response["url"].as_str().is_some_and(|url| matches_search_response(url, keyword)) {
                        pending.insert(id.to_owned());
                    }
                }
                Some("Network.loadingFailed") => { pending.remove(id); }
                Some("Network.loadingFinished") if pending.remove(id) => {
                    let reply = rpc(&mut socket, 5, "Network.getResponseBody",
                        serde_json::json!({"requestId":id}), Some(&session), cancel, CDP_TIMEOUT)?;
                    let body = reply["body"].as_str().context("网页搜索响应没有正文")?;
                    let bytes = if reply["base64Encoded"] == true {
                        base64::engine::general_purpose::STANDARD.decode(body)?
                    } else { body.as_bytes().to_vec() };
                    if let Ok(data) = serde_json::from_slice(&bytes)
                        && let Ok(mut rows) = parse_douyin_search(&data) {
                        for row in &mut rows {
                            row.provider = "douyin_browser".into();
                            row.sources.push(crate::platforms::LookupSource {
                                url: page.to_string(), title: "抖音网页用户搜索".into(),
                                retrieved_at: crate::platforms::retrieval_timestamp(),
                                supports: vec!["账号主页".into()], ..Default::default()
                            });
                        }
                        return Ok(rows);
                    }
                }
                _ => {}
            }
        }
        bail!("等待当前关键词的抖音用户搜索响应超时，可重新打开网页辅助")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_only_the_current_official_user_search() {
        let url = "https://www.douyin.com/aweme/v1/web/discover/search/?keyword=a%20b&search_channel=aweme_user_web";
        assert!(matches_search_response(url, "a b"));
        assert!(!matches_search_response(url, "b"));
        assert!(!matches_search_response(&url.replace("user_web", "video_web"), "a b"));
        assert!(!matches_search_response(&url.replace("www.douyin.com", "example.invalid"), "a b"));
        assert!(!matches_search_response(&format!("{url}&offset=10"), "a b"));
        assert!(!matches_search_response(&format!("{url}&keyword=other"), "a b"));
    }
}
