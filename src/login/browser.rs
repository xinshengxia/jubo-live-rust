//! A browser session created only by an explicit user login action. It has a
//! fresh private profile and a process-owned DevTools endpoint; no discovery of
//! a user's running browsers, unknown ports or existing profiles takes place.

use super::*;
use std::{
    fs,
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
};
use tungstenite::{Message, client::IntoClientRequest};

const CDP_LIMIT: usize = 2 * 1024 * 1024;

pub struct BrowserLogin {
    platform: Platform,
    child: Child,
    profile: PathBuf,
    endpoint: Option<Url>,
    started: Instant,
}
impl std::fmt::Debug for BrowserLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserLogin")
            .field("platform", &self.platform)
            .field("pid", &self.child.id())
            .finish()
    }
}

pub fn login_url(platform: Platform) -> &'static str {
    match platform {
        Platform::Bili => "https://passport.bilibili.com/login",
        Platform::Huya => "https://www.huya.com/",
        Platform::Douyu => "https://www.douyu.com/",
        Platform::Douyin => "https://www.douyin.com/",
    }
}

/// Locate executable files only; never inspect a browser's settings/profile.
pub fn find_browser() -> Option<PathBuf> {
    let mut bases = Vec::new();
    for key in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
        if let Some(path) = std::env::var_os(key) {
            bases.push(PathBuf::from(path));
        }
    }
    // The legacy launcher embedded WebView2 (no external browser account at
    // all). This external-browser implementation cannot embed WebView2 yet, so
    // use Edge in InPrivate mode (no account sync/implicit sign-in) and keep the
    // dedicated temporary profile for the DevTools endpoint; Chrome stays a
    // fallback for machines without Edge.
    for relative in [
        "Microsoft/Edge/Application/msedge.exe",
        "Google/Chrome/Application/chrome.exe",
    ] {
        for base in &bases {
            let path = base.join(relative);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

impl BrowserLogin {
    /// Starts a visible, dedicated login window in response to the user's
    /// button. The user completes all login, CAPTCHA and consent interactions.
    pub fn start(
        platform: Platform,
        browser_executable: &Path,
        profile_root: &Path,
        cancellation: &Cancellation,
    ) -> Result<Self> {
        cancellation.check()?;
        #[cfg(not(windows))]
        bail!("网页登录进程与调试端口所有权验证当前仅支持Windows");
        ensure!(
            browser_executable.is_absolute() && browser_executable.is_file(),
            "请选择已安装浏览器的绝对程序路径"
        );
        let file = browser_executable
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        ensure!(
            file.eq_ignore_ascii_case("msedge.exe") || file.eq_ignore_ascii_case("chrome.exe"),
            "网页登录当前支持已安装Edge或Chrome"
        );
        ensure!(profile_root.is_absolute(), "网页登录资料根目录必须明确指定");
        reject_profile_link(profile_root)?;
        fs::create_dir_all(profile_root).context("无法创建本程序网页登录资料目录")?;
        let profile = profile_root.join(format!("{}-{}", platform, uuid::Uuid::new_v4().simple()));
        fs::create_dir(&profile).context("无法创建独立网页登录资料")?;
        let mut command = Command::new(browser_executable);
        command
            .args(login_browser_arguments(browser_executable, &profile, login_url(platform)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        cancellation.check()?;
        let child = command.spawn().context("无法启动独立网页登录窗口")?;
        let mut session = Self {
            platform,
            child,
            profile,
            endpoint: None,
            started: Instant::now(),
        };
        if cancellation.is_cancelled() {
            let _ = session.close();
            bail!("网页登录已取消");
        }
        Ok(session)
    }
    pub fn platform(&self) -> Platform {
        self.platform
    }
    pub fn process_id(&self) -> u32 {
        self.child.id()
    }
    /// The profile is user session data, not a disposable build/cache artifact.
    /// Cancellation/Drop never recursively removes it.
    pub fn profile_directory(&self) -> &Path {
        &self.profile
    }

    fn endpoint(&mut self, cancellation: &Cancellation) -> Result<Url> {
        cancellation.check()?;
        ensure!(
            self.child.try_wait()?.is_none(),
            "独立登录浏览器已退出，请重新打开"
        );
        if let Some(endpoint) = &self.endpoint {
            return Ok(endpoint.clone());
        }
        ensure!(
            self.started.elapsed() <= Duration::from_secs(180),
            "登录浏览器调试端点等待已超时，请重新打开"
        );
        let active = self.profile.join("DevToolsActivePort");
        reject_profile_link(&active)?;
        let metadata = fs::metadata(&active).context("浏览器正在启动，请稍后点击完成登录")?;
        ensure!(
            metadata.is_file() && metadata.len() <= 4096,
            "浏览器调试端点文件无效"
        );
        let text = fs::read_to_string(&active).context("无法读取本次浏览器调试端点")?;
        let mut lines = text.lines();
        let port = lines
            .next()
            .and_then(|line| line.parse::<u16>().ok())
            .filter(|port| *port > 0)
            .ok_or_else(|| anyhow!("浏览器没有提供有效调试端口"))?;
        let path = lines
            .next()
            .ok_or_else(|| anyhow!("浏览器调试端点缺少身份"))?;
        let expected =
            validate_browser_endpoint(&format!("ws://127.0.0.1:{port}{path}"), port, path)?;
        ensure!(
            listener_owned_by(self.child.id(), port)?,
            "浏览器调试端口不属于本次独立进程，已拒绝读取Cookie"
        );
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(3))
            .build()?;
        let response = client
            .get(format!("http://127.0.0.1:{port}/json/version"))
            .send()
            .map_err(|_| anyhow!("独立浏览器调试端点暂不可用"))?;
        ensure!(response.status().is_success(), "独立浏览器调试端点未就绪");
        let mut bytes = Vec::new();
        response.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 64 * 1024, "浏览器版本响应超过限额");
        let root: Value =
            serde_json::from_slice(&bytes).map_err(|_| anyhow!("浏览器端点响应格式无效"))?;
        let actual = validate_browser_endpoint(
            root["webSocketDebuggerUrl"]
                .as_str()
                .ok_or_else(|| anyhow!("浏览器缺少调试身份"))?,
            port,
            path,
        )?;
        ensure!(actual == expected, "浏览器端点身份与专属profile不一致");
        cancellation.check()?;
        self.endpoint = Some(actual.clone());
        Ok(actual)
    }

    /// User clicks this only after completing login in the dedicated window.
    /// An inconclusive/invalid server response does not overwrite old cookies.
    pub fn capture_and_verify(
        &mut self,
        vault: &CookieVault,
        cancellation: &Cancellation,
    ) -> Result<LoginHealth> {
        let endpoint = self.endpoint(cancellation)?;
        let (result, browser_health) = self.scoped_capture(&endpoint, cancellation)?;
        let cookies = decode_browser_cookies(self.platform, &result)?;
        let mut checked =
            verify_captured(self.platform, &cookies, cancellation, &OfficialTransport)?;
        if !checked.is_valid() {
            if let Some(status) = browser_health {
                if status.is_valid() {
                    checked = status;
                }
            }
        }
        cancellation.check()?;
        if checked.is_valid() {
            vault.import_captured(self.platform, &cookies)?;
        }
        Ok(checked)
    }

    fn open_debugger(
        &self,
        endpoint: &Url,
        cancellation: &Cancellation,
    ) -> Result<tungstenite::WebSocket<TcpStream>> {
        cancellation.check()?;
        let port = endpoint.port().ok_or_else(|| anyhow!("调试端口缺失"))?;
        ensure!(
            listener_owned_by(self.child.id(), port)?,
            "本次浏览器调试端口所有者已改变"
        );
        let address: SocketAddr = format!("127.0.0.1:{port}").parse()?;
        let socket = TcpStream::connect_timeout(&address, Duration::from_secs(3))
            .map_err(|_| anyhow!("无法连接专属浏览器端点"))?;
        socket.set_read_timeout(Some(Duration::from_secs(3)))?;
        socket.set_write_timeout(Some(Duration::from_secs(3)))?;
        let request = endpoint.as_str().into_client_request()?;
        let config = tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(CDP_LIMIT))
            .max_frame_size(Some(CDP_LIMIT));
        let (websocket, _) = tungstenite::client::client_with_config(request, socket, Some(config))
            .map_err(|_| anyhow!("独立浏览器调试握手失败"))?;
        Ok(websocket)
    }
    fn cdp(&self, endpoint: &Url, method: &str, cancellation: &Cancellation) -> Result<Value> {
        let mut websocket = self.open_debugger(endpoint, cancellation)?;
        rpc(
            &mut websocket,
            1,
            method,
            serde_json::json!({}),
            None,
            cancellation,
        )
    }
    fn scoped_capture(
        &self,
        endpoint: &Url,
        cancellation: &Cancellation,
    ) -> Result<(Value, Option<LoginHealth>)> {
        let mut websocket = self.open_debugger(endpoint, cancellation)?;
        let targets = rpc(
            &mut websocket,
            1,
            "Target.getTargets",
            serde_json::json!({}),
            None,
            cancellation,
        )?;
        let selected = targets["targetInfos"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|target| {
                target["type"] == "page"
                    && target["url"]
                        .as_str()
                        .and_then(|a| Url::parse(a).ok())
                        .is_some_and(|url| {
                            url.scheme() == "https"
                                && url
                                    .host_str()
                                    .is_some_and(|host| platform_contains_host(self.platform, host))
                        })
            })
            .ok_or_else(|| anyhow!("请先完成登录，并回到该平台页面"))?;
        let target = selected["targetId"]
            .as_str()
            .ok_or_else(|| anyhow!("平台页面身份缺失"))?;
        let attached = rpc(
            &mut websocket,
            2,
            "Target.attachToTarget",
            serde_json::json!({"targetId":target,"flatten":true}),
            None,
            cancellation,
        )?;
        let session = attached["sessionId"]
            .as_str()
            .ok_or_else(|| anyhow!("无法绑定本次平台页面"))?;
        // Request only selected-platform URL scopes, never the whole browser's
        // storage (which can also hold third-party OAuth session cookies).
        let mut cookies = rpc(
            &mut websocket,
            3,
            "Network.getCookies",
            serde_json::json!({"urls":capture_urls(self.platform)}),
            Some(session),
            cancellation,
        )?;
        let mut browser_health = None;
        if self.platform == Platform::Douyin
            && selected["url"]
                .as_str()
                .and_then(|a| Url::parse(a).ok())
                .is_some_and(|url| {
                    url.host_str() == Some("www.douyin.com") && !url.path().contains("login")
                })
        {
            let endpoint = serde_json::to_string(health_endpoint(self.platform).as_str())?;
            let expression = format!(
                "(async()=>{{const c=new AbortController();const t=setTimeout(()=>c.abort(),8000);try{{const r=await fetch({endpoint},{{credentials:'include',signal:c.signal}});const d=await r.json();const u=d.user||d.data?.user;return{{status:r.status,body:{{status_code:d.status_code,status_msg:d.status_msg,user:u?{{uid:String(u.uid||u.uid_str||'')}}:null}}}};}}catch(e){{return{{status:0,body:{{}}}}}}finally{{clearTimeout(t)}}}})()"
            );
            if let Ok(result) = rpc(
                &mut websocket,
                4,
                "Runtime.evaluate",
                serde_json::json!({"expression":expression,"awaitPromise":true,"returnByValue":true}),
                Some(session),
                cancellation,
            ) {
                let value = &result["result"]["value"];
                if let Some(status) = value["status"].as_u64().filter(|n| *n > 0 && *n <= 599) {
                    browser_health = Some(evaluate_health(
                        self.platform,
                        &LoginReply {
                            status: status as u16,
                            body: serde_json::to_string(&value["body"])?,
                            set_cookies: Vec::new(),
                            location: None,
                        },
                    ));
                }
            }
        }
        if browser_health.is_some() {
            cookies = rpc(
                &mut websocket,
                5,
                "Network.getCookies",
                serde_json::json!({"urls":capture_urls(self.platform)}),
                Some(session),
                cancellation,
            )?;
        }
        Ok((cookies, browser_health))
    }
    /// Close only the process and profile created by this object. Profile data
    /// is deliberately retained; clearing it requires a separate user operation.
    pub fn close(&mut self) -> Result<()> {
        if self.child.try_wait()?.is_some() {
            return Ok(());
        }
        if let Some(endpoint) = self.endpoint.clone() {
            let _ = self.cdp(&endpoint, "Browser.close", &Cancellation::new());
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if self.child.try_wait()?.is_some() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(25));
        }
        self.child.kill().context("无法关闭本次网页登录进程")?;
        self.child.wait()?;
        Ok(())
    }
}
impl Drop for BrowserLogin {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

pub fn capture_urls(platform: Platform) -> Vec<String> {
    let mut urls = vec![health_endpoint(platform).to_string()];
    let hosts: &[&str] = match platform {
        Platform::Bili => &[
            "www.bilibili.com",
            "live.bilibili.com",
            "api.live.bilibili.com",
            "passport.bilibili.com",
            "account.bilibili.com",
        ],
        Platform::Huya => &["www.huya.com", "l.huya.com", "mp.huya.com"],
        Platform::Douyu => &["www.douyu.com", "passport.douyu.com"],
        Platform::Douyin => &["www.douyin.com", "live.douyin.com", "sso.douyin.com"],
    };
    urls.extend(hosts.iter().map(|host| format!("https://{host}/")));
    urls
}
fn rpc(
    socket: &mut tungstenite::WebSocket<TcpStream>,
    id: u64,
    method: &str,
    params: Value,
    session: Option<&str>,
    cancel: &Cancellation,
) -> Result<Value> {
    cancel.check()?;
    let mut request = serde_json::json!({"id":id,"method":method,"params":params});
    if let Some(session) = session {
        request["sessionId"] = Value::String(session.into());
    }
    socket
        .send(Message::Text(request.to_string().into()))
        .map_err(|_| anyhow!("浏览器调试请求失败"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    for _ in 0..128 {
        cancel.check()?;
        ensure!(Instant::now() < deadline, "浏览器调试响应超过等待上限");
        let frame = socket
            .read()
            .map_err(|_| anyhow!("浏览器调试连接超时或已关闭"))?;
        let text = match frame {
            Message::Text(text) => text,
            Message::Ping(bytes) => {
                socket.send(Message::Pong(bytes))?;
                continue;
            }
            Message::Close(_) => bail!("登录浏览器已关闭"),
            _ => continue,
        };
        let response: Value =
            serde_json::from_str(&text).map_err(|_| anyhow!("浏览器调试响应格式无效"))?;
        if response["id"].as_u64() == Some(id) {
            ensure!(response.get("error").is_none(), "浏览器拒绝本次平台检查");
            return Ok(response["result"].clone());
        }
    }
    bail!("浏览器调试事件超过限额")
}

pub fn validate_browser_endpoint(address: &str, port: u16, expected_path: &str) -> Result<Url> {
    ensure!(
        expected_path.starts_with("/devtools/browser/")
            && expected_path.len() > 18
            && expected_path.len() <= 200
            && expected_path[18..]
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "浏览器调试身份格式无效"
    );
    let url = Url::parse(address).map_err(|_| anyhow!("浏览器调试地址格式无效"))?;
    ensure!(
        url.scheme() == "ws"
            && url.host_str() == Some("127.0.0.1")
            && url.port() == Some(port)
            && url.path() == expected_path
            && url.query().is_none()
            && url.fragment().is_none()
            && url.username().is_empty()
            && url.password().is_none(),
        "拒绝非本次浏览器的调试地址"
    );
    Ok(url)
}

pub fn decode_browser_cookies(platform: Platform, result: &Value) -> Result<Vec<CapturedCookie>> {
    let rows = result["cookies"]
        .as_array()
        .ok_or_else(|| anyhow!("浏览器没有返回Cookie清单"))?;
    ensure!(rows.len() <= MAX_COOKIES, "浏览器Cookie清单超过限额");
    let mut cookies = Vec::new();
    let mut skipped = 0usize;
    for row in rows {
        let domain = row["domain"]
            .as_str()
            .ok_or_else(|| anyhow!("浏览器Cookie缺少域"))?;
        let normalized = domain.trim_start_matches('.').to_ascii_lowercase();
        if !platform_contains_host(platform, &normalized) {
            continue;
        }
        let expiration = if row["session"].as_bool() == Some(true) {
            None
        } else {
            row["expires"]
                .as_f64()
                .filter(|value| value.is_finite() && *value >= 0.0)
        };
        let cookie = CapturedCookie {
            name: row["name"]
                .as_str()
                .ok_or_else(|| anyhow!("Cookie缺少名称"))?
                .into(),
            value: row["value"]
                .as_str()
                .ok_or_else(|| anyhow!("Cookie缺少值"))?
                .into(),
            domain: normalized,
            host_only: !domain.starts_with('.'),
            path: row["path"].as_str().unwrap_or("/").into(),
            secure: row["secure"].as_bool().unwrap_or(false),
            http_only: row["httpOnly"].as_bool().unwrap_or(false),
            expires_at: expiration.map(|value| value as u64),
        };
        // Chromium can retain a cookie whose name is empty or otherwise not a
        // valid RFC 6265 token (Douyin's ads/SSO pages have produced such an
        // entry). One malformed cookie must not discard the whole login: skip
        // it, keep the session cookies and let the platform health check decide.
        if let Err(error) = cookie.validate(platform) {
            skipped += 1;
            crate::diag::log(
                "login",
                crate::diag::Level::Warn,
                &format!(
                    "ignored invalid browser cookie for {platform}: {error} (name={})",
                    describe_cookie_name(&cookie.name)
                ),
            );
            continue;
        }
        if cookie.expires_at.is_none_or(|expiry| expiry > now()) {
            cookies.push(cookie);
        }
    }
    ensure!(
        !cookies.is_empty(),
        if skipped == 0 {
            "尚未取得本平台有效会话，请先在专属窗口完成登录".to_owned()
        } else {
            format!(
                "浏览器返回的Cookie均不可用（已忽略 {skipped} 个格式异常项），请在专属窗口重新登录后重试"
            )
        }
    );
    Ok(cookies)
}

/// Renders a cookie name for diagnostics without leaking values: printable
/// ASCII is kept, other characters are escaped, and the field is bounded.
fn describe_cookie_name(name: &str) -> String {
    if name.is_empty() {
        return "<empty>".to_owned();
    }
    let mut out = String::new();
    for character in name.chars().take(64) {
        if character.is_ascii_graphic() {
            out.push(character);
        } else {
            out.push_str(&format!("\\u{{{:X}}}", character as u32));
        }
    }
    if name.chars().count() > 64 {
        out.push('…');
    }
    out
}

/// Removes every dedicated browser profile this platform created under the
/// launcher's own browser-login root. Only direct child directories named
/// "<platform>-<32 hex>" are removed; symlinks and other files are refused.
/// Call only after that platform's browser window has been closed by the user.
pub fn remove_platform_profiles(profile_root: &Path, platform: Platform) -> Result<usize> {
    reject_profile_link(profile_root)?;
    let entries = match fs::read_dir(profile_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error).context("无法读取网页辅助资料目录"),
    };
    let prefix = format!("{platform}-");
    let mut wanted = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(suffix) = name.strip_prefix(&prefix) else {
            continue;
        };
        if suffix.len() != 32 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        ensure!(
            !metadata.file_type().is_symlink(),
            "网页辅助资料路径不能是链接"
        );
        if metadata.is_dir() {
            wanted.push(path);
        }
    }
    let mut removed = 0;
    for path in wanted {
        fs::remove_dir_all(&path).context("无法清理网页辅助资料目录")?;
        removed += 1;
    }
    Ok(removed)
}

fn reject_profile_link(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(!metadata.file_type().is_symlink(), "登录资料路径不能是链接"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error).context("无法检查登录资料路径"),
    };
    Ok(())
}

#[cfg(windows)]
fn listener_owned_by(pid: u32, port: u16) -> Result<bool> {
    #[link(name = "iphlpapi")]
    unsafe extern "system" {
        fn GetExtendedTcpTable(
            table: *mut std::ffi::c_void,
            size: *mut u32,
            order: i32,
            family: u32,
            class: u32,
            reserved: u32,
        ) -> u32;
    }
    let mut size = 0u32;
    let status = unsafe { GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, 2, 3, 0) };
    ensure!(status == 122 || status == 0, "无法检查专属浏览器端口所有者");
    ensure!(size >= 4 && size <= 4 * 1024 * 1024, "系统TCP表大小无效");
    let mut bytes = vec![0u8; size as usize];
    let status = unsafe { GetExtendedTcpTable(bytes.as_mut_ptr().cast(), &mut size, 0, 2, 3, 0) };
    ensure!(status == 0, "无法确认专属浏览器端口所有者");
    let count = u32::from_ne_bytes(bytes[0..4].try_into().unwrap()) as usize;
    ensure!(
        4usize.saturating_add(count.saturating_mul(24)) <= size as usize,
        "系统TCP表长度无效"
    );
    for index in 0..count {
        let row = &bytes[4 + index * 24..4 + (index + 1) * 24];
        let address = u32::from_ne_bytes(row[4..8].try_into().unwrap());
        let local = u32::from_ne_bytes(row[8..12].try_into().unwrap());
        let owner = u32::from_ne_bytes(row[20..24].try_into().unwrap());
        if address == 0x0100_007f && u16::from_be(local as u16) == port && owner == pid {
            return Ok(true);
        }
    }
    Ok(false)
}
#[cfg(not(windows))]
fn listener_owned_by(_pid: u32, _port: u16) -> Result<bool> {
    bail!("网页登录端口所有权验证当前仅支持Windows")
}


/// Launch flags for the dedicated login browser.
///
/// The window must never inherit the user's Edge/Chrome account: Edge on
/// Windows performs implicit sign-in with the signed-in Microsoft account and
/// then offers to sync browsing data. That is confusing and privacy-sensitive,
/// so the login window uses a brand-new profile and disables sync; for Edge we
/// additionally disable the implicit sign-in/first-run/sync features. The
/// caller strictly talks to this process's DevTools endpoint only after
/// verifying its ownership, and the profile is private to this app.
fn login_browser_arguments(browser: &Path, profile: &Path, url: &str) -> Vec<String> {
    let file = browser
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let is_edge = file.eq_ignore_ascii_case("msedge.exe");
    let mut args = vec![
        format!("--user-data-dir={}", profile.display()),
        "--remote-debugging-port=0".into(),
        "--remote-debugging-address=127.0.0.1".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-sync".into(),
        "--disable-background-networking".into(),
        "--disable-component-update".into(),
        "--disable-default-apps".into(),
        "--disable-extensions".into(),
        "--new-window".into(),
        url.into(),
    ];
    if is_edge {
        // InPrivate is the closest external-browser equivalent of the legacy
        // embedded WebView2 window: the Microsoft account is not signed in and
        // browsing data is not synced. The dedicated user-data dir still owns
        // the DevTools endpoint and keeps the app's cookie capture scoped.
        args.insert(args.len() - 2, "--inprivate".into());
        // Keep the edge-specific switches in a single feature string. Unknown
        // feature names are ignored by Chromium, so this list is harmless even
        // on builds that renamed one of them.
        args.insert(
            args.len() - 2,
            "--disable-features=msImplicitSignin,msEdgeIdentity,msEdgeSync,msEdgeFirstRunExperience,msEdgeSidebar".into(),
        );
    }
    args
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn login_browser_never_signs_into_the_user_account() {
        let profile = PathBuf::from(r"C:\private-login-profile");
        let chrome = login_browser_arguments(
            Path::new(r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
            &profile,
            "https://www.huya.com/",
        );
        assert!(chrome.iter().any(|arg| arg == "--disable-sync"));
        assert!(chrome.iter().any(|arg| arg == "--no-first-run"));
        assert!(chrome
            .iter()
            .any(|arg| arg == r"--user-data-dir=C:\private-login-profile"));
        assert!(chrome.iter().any(|arg| arg == "--disable-extensions"));
        assert!(!chrome.iter().any(|arg| arg.starts_with("--disable-features=")));
        assert!(!chrome.iter().any(|arg| arg == "--inprivate"));
        assert_eq!(chrome.last().map(String::as_str), Some("https://www.huya.com/"));

        let edge = login_browser_arguments(
            Path::new(r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"),
            &profile,
            "https://www.huya.com/",
        );
        assert!(edge.iter().any(|arg| arg == "--inprivate"));
        let features = edge
            .iter()
            .find(|arg| arg.starts_with("--disable-features="))
            .expect("Edge must disable implicit sign-in features");
        assert!(features.contains("msImplicitSignin"));
        assert!(features.contains("msEdgeSync"));
        assert!(features.contains("msEdgeFirstRunExperience"));
    }

    #[test]
    fn decode_skips_malformed_cookies_and_keeps_the_session() {
        let result = serde_json::json!({"cookies":[
            {"name":"","value":"x","domain":".douyin.com","path":"/","secure":true,"httpOnly":false,"session":true},
            {"name":"sessionid","value":"synthetic-session","domain":".douyin.com","path":"/","secure":true,"httpOnly":true,"session":true},
            {"name":"ttwid","value":"synthetic-ttwid","domain":".douyin.com","path":"/","secure":true,"httpOnly":false,"session":true}
        ]});
        let cookies = decode_browser_cookies(Platform::Douyin, &result).expect("valid cookie kept");
        assert_eq!(cookies.len(), 2);
        assert!(cookies.iter().any(|cookie| cookie.name == "sessionid"));
        assert!(!cookies.iter().any(|cookie| cookie.name.is_empty()));
    }

    #[test]
    fn decode_reports_when_every_cookie_is_unusable() {
        let result = serde_json::json!({"cookies":[
            {"name":"","value":"x","domain":".douyin.com","path":"/","secure":true,"httpOnly":false,"session":true}
        ]});
        let error = decode_browser_cookies(Platform::Douyin, &result).expect_err("no usable cookie");
        assert!(error.to_string().contains("格式异常"), "{error}");
    }

    #[test]
    fn devtools_listener_requires_the_actual_owning_process() {

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(listener_owned_by(std::process::id(), port).unwrap());
        assert!(!listener_owned_by(u32::MAX, port).unwrap());
    }
}