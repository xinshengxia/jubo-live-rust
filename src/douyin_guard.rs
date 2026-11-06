//! Douyin verification recovery.
//!
//! The platform returns a small "verification interstitial" page (captcha SDK)
//! instead of room data when it distrusts the request. A normal HTTP retry can
//! never fix that: the page has to be solved by a human in a real browser, and
//! the resulting cookies have to be captured through the DevTools protocol
//! (`ttwid`/`__ac_signature` are HttpOnly and never visible to `document.cookie`).
//!
//! This module owns two entry points:
//!
//! * [try_silent_refresh] - the playback path's automatic recovery. It opens one
//!   hidden, screen-positioned-away browser window with this application's own
//!   dedicated profile, waits for the page to stop being a captcha interstitial,
//!   and stores the captured cookies in the encrypted [CookieVault]. A global
//!   single-flight slot plus a 60 s cooldown keep a failing playback from
//!   launching browsers in a loop.
//! * [capture_visible] - the accounts page's explicit "Douyin web verification"
//!   flow. The user solves the challenge (or logs in) in a visible dedicated
//!   window and clicks the existing capture button; the same predicate decides
//!   whether the cookies are worth saving. Login health is not required, because
//!   a solved slider alone already restores room-page access.
//!
//! Only this application's profile is ever touched; the user's everyday browser
//! profile and the legacy application are never read. The profile directory is
//! kept on success and on failure (it carries the session used by later silent
//! refreshing), and the browser process this module started is always closed.

use crate::auth::{CapturedCookie, CookieVault};
use crate::core::Platform;
use crate::login::browser;
use anyhow::{Context, Result, anyhow, ensure};
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Automatic silent refreshes are throttled to one per minute: starting a
/// browser is expensive and repeating a failed attempt only provokes the
/// platform's risk control further. Mirrors the legacy launcher's guard.
pub const SILENT_COOLDOWN: Duration = Duration::from_secs(60);
/// One silent attempt may wait this long for the interstitial to clear.
const SILENT_DEADLINE: Duration = Duration::from_secs(25);
/// Polling interval while the page is loading / the user is solving the slider.
const POLL_INTERVAL: Duration = Duration::from_millis(700);
/// Fixed name of this application's dedicated verification profile. It lives
/// next to the encrypted credential directory, never in a user browser profile.
const VERIFY_PROFILE_DIR: &str = "douyin-verify-profile";
/// The live-site origin used for both the silent and the user-visible window.
const DOUYIN_PAGE_ROOT: &str = "https://live.douyin.com/";
/// Cross-process lock for the shared dedicated profile. The player process and
/// each relay child process have their own static gate, so only an OS file lock
/// can stop two of them from launching Chromium on the same profile.
const VERIFY_LOCK_FILE: &str = ".douyin-verify.lock";
/// Cross-process cooldown stamp (epoch milliseconds of the last attempt).
const VERIFY_COOLDOWN_FILE: &str = ".douyin-verify-last";

/// Cancellation probe used by the browser abstraction. It is deliberately a
/// plain closure (not the login flow's Cancellation token) so the GUI worker and
/// the playback path can share it without inventing a second token type.
/// The object lifetime is tied to the reference (a plain type alias would
/// default to 'static and reject the locally scoped closures the GUI/test
/// paths pass in).
pub(crate) type CancelProbe<'a> = dyn Fn() -> bool + Send + Sync + 'a;

/// `true` when the page has clearly passed the human verification: it is no
/// longer the risk-control interstitial and the device cookie `ttwid` exists.
///
/// This is the pure contract from the design document. Callers also require a
/// loaded (non-empty) page title before saving, because `ttwid` can already be
/// present while the slider is still waiting; keeping that extra guard in the
/// capture loop (not here) preserves the documented predicate exactly.
pub(crate) fn verification_passed(title: &str, cookies: &[CapturedCookie]) -> bool {
    !title.contains("验证码中间页")
        && cookies
            .iter()
            .any(|cookie| cookie.name == "ttwid" && !cookie.value.trim().is_empty())
}

/// Whether the page title has been populated by the loaded document. An empty
/// title means "still about:blank / still loading", not "verification passed".
fn page_title_loaded(title: &str) -> bool {
    !title.trim().is_empty()
}

/// A Douyin live-room web id: bounded ASCII identifier so it can be placed in
/// the URL path without encoding or injection.
fn room_is_valid(room: &str) -> bool {
    !room.is_empty()
        && room.len() <= 64
        && room
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Page URL for a room, or the platform root when `room` is empty (the
/// user-visible verification window does not need a specific room).
fn douyin_page_url(room: &str) -> String {
    if room.is_empty() {
        DOUYIN_PAGE_ROOT.to_owned()
    } else {
        format!("{DOUYIN_PAGE_ROOT}{room}")
    }
}

/// The exact persistent profile directory shared by silent and user-visible
/// verification: `<data dir>/douyin-verify-profile`, i.e. a sibling of the
/// encrypted `credentials` directory owned by the vault.
pub(crate) fn verification_profile(vault: &CookieVault) -> Result<PathBuf> {
    let data_root = vault
        .root()
        .parent()
        .ok_or_else(|| anyhow!("凭据目录缺少数据根目录"))?;
    Ok(data_root.join(VERIFY_PROFILE_DIR))
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// Current wall-clock in milliseconds; the GUI worker takes the same slot as
/// the silent refresh and therefore needs the same clock source.
pub(crate) fn now_ms() -> u64 {
    epoch_millis()
}

/// Injectable clock for [refresh_core].
pub(crate) trait VerificationClock {
    fn now_ms(&self) -> u64;
    fn sleep(&self, duration: Duration);
}

struct SystemClock;
impl VerificationClock for SystemClock {
    fn now_ms(&self) -> u64 {
        epoch_millis()
    }
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[derive(Debug, Default)]
struct SilentState {
    in_flight: bool,
    cooldown_until_ms: u64,
}

/// Serializes attempts that would launch the same dedicated browser profile at
/// the same time. A silent attempt reserves the slot and starts the 60 s
/// attempt-based cooldown immediately (like the legacy guard); the explicit
/// user-visible window reserves the slot without the automatic cooldown, since
/// the user asked for it and may take minutes to solve the challenge.
#[derive(Debug)]
pub(crate) struct SilentGate {
    state: Mutex<SilentState>,
}

impl SilentGate {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(SilentState::default()),
        })
    }

    pub(crate) fn try_begin_silent(self: &Arc<Self>, now_ms: u64) -> Option<SilentPermit> {
        self.try_begin(now_ms, true)
    }

    pub(crate) fn try_begin_user_verification(
        self: &Arc<Self>,
        now_ms: u64,
    ) -> Option<SilentPermit> {
        self.try_begin(now_ms, false)
    }

    fn try_begin(self: &Arc<Self>, now_ms: u64, automatic: bool) -> Option<SilentPermit> {
        let cooldown = SILENT_COOLDOWN.as_millis().min(u64::MAX as u128) as u64;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.in_flight || now_ms < state.cooldown_until_ms {
            return None;
        }
        state.in_flight = true;
        if automatic {
            state.cooldown_until_ms = now_ms.saturating_add(cooldown);
        }
        Some(SilentPermit {
            gate: Arc::clone(self),
            automatic,
        })
    }
}

/// Holds one verification attempt's slot; always released on drop, so an early
/// return or a panic cannot wedge later playback attempts forever.
pub(crate) struct SilentPermit {
    gate: Arc<SilentGate>,
    automatic: bool,
}

impl SilentPermit {
    /// Reports the outcome. A successful automatic attempt pushes the cooldown
    /// to a full minute from completion, so a quick second failure cannot
    /// immediately start another browser.
    pub(crate) fn finish(self, success: bool, now_ms: u64) {
        if self.automatic && success {
            let cooldown = SILENT_COOLDOWN.as_millis().min(u64::MAX as u128) as u64;
            let mut state = self
                .gate
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.cooldown_until_ms =
                state.cooldown_until_ms.max(now_ms.saturating_add(cooldown));
        }
    }
}

impl Drop for SilentPermit {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.in_flight = false;
    }
}

fn silent_gate() -> &'static Arc<SilentGate> {
    static GATE: OnceLock<Arc<SilentGate>> = OnceLock::new();
    GATE.get_or_init(SilentGate::new)
}

/// The process-wide silent-refresh gate: at most one attempt at a time and at
/// most one attempt per [SILENT_COOLDOWN] once a previous attempt started.
pub(crate) fn try_begin_silent(now_ms: u64) -> Option<SilentPermit> {
    silent_gate().try_begin_silent(now_ms)
}

/// The process-wide slot for the explicit user-visible verification window.
pub(crate) fn try_begin_user_verification(now_ms: u64) -> Option<SilentPermit> {
    silent_gate().try_begin_user_verification(now_ms)
}

/// Cross-process reservation of the dedicated verification profile.
///
/// The in-process [SilentGate] only serializes callers inside one process; the
/// player and a relay child process each own a separate copy, so both could
/// otherwise launch Chromium against the same profile (and each keep retrying
/// once per minute). This lease is the actual cross-process arbitration: an OS
/// file lock under the application's data directory, released by the OS when
/// the handle is dropped or the process exits, plus the persistent cooldown
/// stamp file beside it.
pub(crate) struct ProfileLease {
    _lock: File,
    timestamp_path: PathBuf,
}

impl ProfileLease {
    /// Tries to reserve the profile for this process. `data_root` is the
    /// application data directory that contains the dedicated profile and the
    /// encrypted credentials directory. `Ok(None)` means another process holds
    /// the profile right now.
    pub(crate) fn try_acquire(data_root: &Path) -> Result<Option<Self>> {
        let lock_path = data_root.join(VERIFY_LOCK_FILE);
        reject_state_link(&lock_path)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .context("无法打开抖音验证锁文件")?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self {
                _lock: file,
                timestamp_path: data_root.join(VERIFY_COOLDOWN_FILE),
            })),
            Err(error) if lock_is_busy(&error) => Ok(None),
            Err(error) => Err(error).context("无法锁定抖音验证资料"),
        }
    }

    /// Remaining time of the persistent cooldown. The window grows with the
    /// number of consecutive failures (60 s -> 120 s -> 300 s cap) so a relay
    /// reconnect loop cannot relaunch a hidden browser every minute. Missing,
    /// corrupt or future-dated stamps (clock regression) are treated as
    /// "no cooldown", so a broken state file can never disable recovery
    /// permanently.
    pub(crate) fn cooldown_remaining_ms(&self, now_ms: u64) -> u64 {
        match read_cooldown_stamp(&self.timestamp_path) {
            Some(stamp) if now_ms >= stamp.last_attempt_ms => {
                backoff_ms(stamp.failure_streak)
                    .saturating_sub(now_ms - stamp.last_attempt_ms)
            }
            _ => 0,
        }
    }

    /// Returns true when an automatic attempt may start now and records the
    /// attempt start. The current failure streak is kept: if this attempt also
    /// fails it advances to the next backoff step.
    pub(crate) fn begin_automatic_attempt(&self, now_ms: u64) -> bool {
        if self.cooldown_remaining_ms(now_ms) > 0 {
            return false;
        }
        let streak = read_cooldown_stamp(&self.timestamp_path)
            .map(|stamp| stamp.failure_streak)
            .unwrap_or(0);
        write_cooldown_stamp(&self.timestamp_path, now_ms, streak);
        true
    }

    /// Records a failed attempt: the next automatic attempt waits one backoff
    /// step longer (60 s -> 120 s -> 300 s cap).
    pub(crate) fn note_failure(&self, now_ms: u64) {
        let streak = read_cooldown_stamp(&self.timestamp_path)
            .map(|stamp| stamp.failure_streak)
            .unwrap_or(0)
            .saturating_add(1)
            .min(MAX_FAILURE_STREAK);
        write_cooldown_stamp(&self.timestamp_path, now_ms, streak);
    }

    /// A success restarts the cooldown from the completion time and clears the
    /// failure streak. The same reset is applied whenever verified credentials
    /// are written, because a fresh session must not inherit an old backoff.
    pub(crate) fn note_success(&self, now_ms: u64) {
        write_cooldown_stamp(&self.timestamp_path, now_ms, 0);
    }
}

const MAX_FAILURE_STREAK: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CooldownStamp {
    /// Epoch milliseconds of the last attempt start/failure/success.
    last_attempt_ms: u64,
    /// Number of consecutive failed attempts (capped).
    failure_streak: u32,
}

/// Base 60 s; consecutive failures double to 120 s and then cap at 300 s.
fn backoff_ms(failure_streak: u32) -> u64 {
    let base = SILENT_COOLDOWN.as_millis().min(u64::MAX as u128) as u64;
    match failure_streak {
        0 | 1 => base,
        2 => base.saturating_mul(2),
        _ => base.saturating_mul(5),
    }
}

fn read_cooldown_stamp(path: &Path) -> Option<CooldownStamp> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    let mut fields = text.split_whitespace();
    let last_attempt_ms = fields.next()?.parse::<u64>().ok()?;
    // Older stamps contain only the attempt timestamp, which means streak 0.
    let failure_streak = fields
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0)
        .min(MAX_FAILURE_STREAK);
    Some(CooldownStamp {
        last_attempt_ms,
        failure_streak,
    })
}

fn write_cooldown_stamp(path: &Path, last_attempt_ms: u64, failure_streak: u32) {
    // Never write through a link: a corrupted/attacker-planted state file must
    // not turn into an arbitrary-file write.
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return;
    }
    let _ = fs::write(
        path,
        format!("{last_attempt_ms} {}", failure_streak.min(MAX_FAILURE_STREAK)),
    );
}

fn lock_is_busy(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || error.raw_os_error() == Some(33) // ERROR_LOCK_VIOLATION on Windows
}

fn reject_state_link(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            !metadata.file_type().is_symlink(),
            "抖音验证状态文件不能是链接"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error).context("无法检查抖音验证状态文件"),
    }
    Ok(())
}

/// The browser/page seam used by [refresh_core] and [capture_visible]. The real
/// implementation owns a dedicated Chromium process; tests provide a fake page
/// so the default suite never starts a browser.
pub(crate) trait VerificationBrowser {
    /// Opens the verification page for `room` (empty room = platform root page).
    fn open(&mut self, room: &str, cancel: &CancelProbe<'_>) -> Result<()>;
    /// One page probe. `Ok(None)` means the page target is not ready yet.
    fn snapshot(
        &mut self,
        cancel: &CancelProbe<'_>,
    ) -> Result<Option<browser::VerificationSnapshot>>;
    /// Closes only the process this object started. Must be idempotent.
    fn close(&mut self);
}

fn import_verified_cookies(vault: &CookieVault, cookies: &[CapturedCookie]) -> Result<()> {
    ensure!(!cookies.is_empty(), "验证结果没有可保存的Cookie");
    vault.import_captured(Platform::Douyin, cookies)?;
    // Writing verified credentials is a reset event: the next automatic attempt
    // must not inherit a failure backoff from the session that just recovered.
    reset_cross_process_cooldown(vault, now_ms());
    Ok(())
}

/// Clears the shared failure backoff (and restarts the base delay from now)
/// after verified credentials were written. Best effort: a missing data root or
/// an unwritable stamp only means the next attempt may be throttled longer.
fn reset_cross_process_cooldown(vault: &CookieVault, now_ms: u64) {
    let Some(data_root) = vault.root().parent() else {
        return;
    };
    write_cooldown_stamp(&data_root.join(VERIFY_COOLDOWN_FILE), now_ms, 0);
}

/// A successful verification restores direct access: cached room pages and the
/// proxy-first preference must not survive it.
fn note_verification_success() {
    crate::platforms::invalidate_douyin_page_cache();
    crate::platforms::note_douyin_verification_success();
}

/// Decision loop shared by the fake-browser tests: open one page, poll until the
/// title/cookies prove the verification passed, persist the cookies, and always
/// close the browser exactly once (including cancellation and poll failures).
fn refresh_core<B>(
    browser: &mut B,
    vault: &CookieVault,
    room: &str,
    cancel: &CancelProbe<'_>,
    clock: &dyn VerificationClock,
) -> Result<bool>
where
    B: VerificationBrowser + ?Sized,
{
    if cancel() {
        browser.close();
        return Ok(false);
    }
    let started = clock.now_ms();
    if let Err(error) = browser.open(room, cancel) {
        browser.close();
        crate::diag::log(
            "login",
            crate::diag::Level::Warn,
            &format!("douyin silent verification could not open the dedicated window: {error}"),
        );
        return Ok(false);
    }
    let result = (|| -> Result<bool> {
        let deadline = SILENT_DEADLINE.as_millis().min(u64::MAX as u128) as u64;
        loop {
            if cancel() {
                return Ok(false);
            }
            if clock.now_ms().saturating_sub(started) >= deadline {
                return Ok(false);
            }
            match browser.snapshot(cancel) {
                Ok(Some(snapshot)) => {
                    if page_title_loaded(&snapshot.title)
                        && verification_passed(&snapshot.title, &snapshot.cookies)
                    {
                        import_verified_cookies(vault, &snapshot.cookies)?;
                        note_verification_success();
                        crate::diag::log(
                            "login",
                            crate::diag::Level::Info,
                            &format!(
                                "douyin silent verification captured a verified session (cookies: {})",
                                snapshot.cookies.len()
                            ),
                        );
                        return Ok(true);
                    }
                }
                Ok(None) => (),
                Err(error) => {
                    // A browser that is still starting may refuse the probe for a
                    // while; keep polling until the deadline instead of giving up.
                    crate::diag::log(
                        "login",
                        crate::diag::Level::Debug,
                        &format!("douyin silent verification probe not ready: {error}"),
                    );
                }
            }
            clock.sleep(POLL_INTERVAL);
        }
    })();
    browser.close();
    result
}

/// One-shot capture for the user-visible window: save only when the page has
/// already passed verification. Returns `Ok(false)` (never an error) when the
/// page is still the interstitial / the title is not loaded yet / no `ttwid`.
pub(crate) fn capture_visible<B>(
    browser: &mut B,
    vault: &CookieVault,
    cancel: &CancelProbe<'_>,
) -> Result<bool>
where
    B: VerificationBrowser + ?Sized,
{
    if cancel() {
        return Ok(false);
    }
    let Some(snapshot) = browser.snapshot(cancel)? else {
        return Ok(false);
    };
    if !page_title_loaded(&snapshot.title)
        || !verification_passed(&snapshot.title, &snapshot.cookies)
    {
        return Ok(false);
    }
    import_verified_cookies(vault, &snapshot.cookies)?;
    note_verification_success();
    crate::diag::log(
        "login",
        crate::diag::Level::Info,
        &format!(
            "douyin user verification captured a verified session (cookies: {})",
            snapshot.cookies.len()
        ),
    );
    Ok(true)
}

/// Production browser adapter. Each object owns at most one browser process and
/// opens the shared dedicated profile; closing is idempotent and never removes
/// the profile.
pub(crate) struct DouyinBrowser {
    executable: PathBuf,
    profile: PathBuf,
    visible: bool,
    session: Option<browser::BrowserVerification>,
    /// Only the user-visible window takes the cross-process lease itself; the
    /// silent path already holds one for the whole refresh attempt (taking it
    /// twice in the same process would deadlock on the OS lock).
    lease: Option<ProfileLease>,
}

impl DouyinBrowser {
    pub(crate) fn new(executable: PathBuf, profile: PathBuf, visible: bool) -> Self {
        Self {
            executable,
            profile,
            visible,
            session: None,
            lease: None,
        }
    }

    /// Visible verification window starts on the platform root page; the user
    /// can log in or solve the challenge there.
    pub(crate) fn open_root_page(&mut self, cancel: &CancelProbe<'_>) -> Result<()> {
        <Self as VerificationBrowser>::open(self, "", cancel)
    }

    pub(crate) fn close_now(&mut self) {
        <Self as VerificationBrowser>::close(self);
    }
}

impl VerificationBrowser for DouyinBrowser {
    fn open(&mut self, room: &str, cancel: &CancelProbe<'_>) -> Result<()> {
        ensure!(
            room.is_empty() || room_is_valid(room),
            "抖音房间号格式无效"
        );
        let url = douyin_page_url(room);
        if self.visible {
            let data_root = self
                .profile
                .parent()
                .ok_or_else(|| anyhow!("抖音验证资料目录缺少数据根目录"))?;
            self.lease = Some(ProfileLease::try_acquire(data_root)?.ok_or_else(|| {
                anyhow!("另一进程正在使用抖音验证资料，请稍后重试")
            })?);
        }
        let session = browser::BrowserVerification::start(
            Platform::Douyin,
            &self.executable,
            &self.profile,
            &url,
            self.visible,
            cancel,
        )?;
        self.session = Some(session);
        Ok(())
    }

    fn snapshot(
        &mut self,
        cancel: &CancelProbe<'_>,
    ) -> Result<Option<browser::VerificationSnapshot>> {
        match self.session.as_mut() {
            Some(session) => session.poll(cancel),
            None => Ok(None),
        }
    }

    fn close(&mut self) {
        if let Some(mut session) = self.session.take() {
            let _ = session.close();
        }
        // Release the cross-process lease only after the browser window is closed.
        self.lease = None;
    }
}

impl Drop for DouyinBrowser {
    fn drop(&mut self) {
        self.close_now();
    }
}

/// Automatic silent recovery used by the playback path.
///
/// Returns `Ok(true)` only when a verified cookie set was encrypted into the
/// vault by this call. `Ok(false)` covers "another attempt is running", "still
/// cooling down", "no supported browser", "canceled" and "deadline reached" -
/// callers must not loop on this. `Err` is reserved for storage failures.
pub fn try_silent_refresh(
    vault: &CookieVault,
    room: &str,
    cancel: &(dyn Fn() -> bool + Send + Sync),
) -> Result<bool> {
    if cancel() {
        return Ok(false);
    }
    if !room_is_valid(room) {
        crate::diag::log(
            "login",
            crate::diag::Level::Warn,
            "douyin silent verification skipped: invalid room identifier",
        );
        return Ok(false);
    }
    let Some(permit) = try_begin_silent(now_ms()) else {
        return Ok(false);
    };
    let result = (|| -> Result<bool> {
        let Some(executable) = browser::find_browser() else {
            crate::diag::log(
                "login",
                crate::diag::Level::Warn,
                "douyin silent verification unavailable: no supported browser executable found",
            );
            return Ok(false);
        };
        let profile = verification_profile(vault)?;
        let data_root = profile
            .parent()
            .ok_or_else(|| anyhow!("抖音验证资料目录缺少数据根目录"))?;
        // Player and relay processes have independent in-process gates, so the
        // shared profile and the 60 s cooldown must be arbitrated on disk.
        let Some(lease) = ProfileLease::try_acquire(data_root)? else {
            crate::diag::log(
                "login",
                crate::diag::Level::Warn,
                "douyin silent verification skipped: another process owns the verification profile",
            );
            return Ok(false);
        };
        if !lease.begin_automatic_attempt(now_ms()) {
            crate::diag::log(
                "login",
                crate::diag::Level::Warn,
                "douyin silent verification skipped: cross-process cooldown is still active",
            );
            return Ok(false);
        }
        let mut window = DouyinBrowser::new(executable, profile, false);
        let captured = refresh_core(&mut window, vault, room, cancel, &SystemClock)?;
        if captured {
            lease.note_success(now_ms());
        } else if !cancel() {
            // A deadline / unverified page is a failure and increases the shared
            // backoff; a user cancellation must not.
            lease.note_failure(now_ms());
        }
        Ok(captured)
    })();
    let success = result.as_ref().is_ok_and(|captured| *captured);
    permit.finish(success, now_ms());
    if let Err(error) = &result {
        crate::diag::log(
            "login",
            crate::diag::Level::Warn,
            &format!("douyin silent verification failed: {error}"),
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::sync::atomic::{AtomicBool, Ordering};

    fn cookie(name: &str, value: &str) -> CapturedCookie {
        CapturedCookie {
            name: name.into(),
            value: value.into(),
            domain: "douyin.com".into(),
            host_only: false,
            path: "/".into(),
            secure: true,
            http_only: true,
            expires_at: None,
        }
    }
    fn ttwid() -> CapturedCookie {
        cookie("ttwid", "SYNTHETIC_TTWID")
    }
    fn sessionid() -> CapturedCookie {
        cookie("sessionid", "SYNTHETIC_SESSION")
    }

    struct FakeClock {
        now: Cell<u64>,
    }
    impl FakeClock {
        fn new(now: u64) -> Self {
            Self { now: Cell::new(now) }
        }
    }
    impl VerificationClock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.now.get()
        }
        fn sleep(&self, duration: Duration) {
            self.now
                .set(self.now.get().saturating_add(duration.as_millis() as u64));
        }
    }

    struct FakeBrowser {
        snapshots: RefCell<Vec<(String, Vec<CapturedCookie>)>>,
        index: Cell<usize>,
        opens: Cell<usize>,
        polls: Cell<usize>,
        closes: Cell<usize>,
        /// The cancellation flag is shared with the caller through Arc, matching
        /// the Send + Sync contract of the real cancellation probe.
        cancel: Option<Arc<AtomicBool>>,
        cancel_after_polls: usize,
    }
    impl FakeBrowser {
        fn new(snapshots: Vec<(String, Vec<CapturedCookie>)>) -> Self {
            Self {
                snapshots: RefCell::new(snapshots),
                index: Cell::new(0),
                opens: Cell::new(0),
                polls: Cell::new(0),
                closes: Cell::new(0),
                cancel: None,
                cancel_after_polls: 0,
            }
        }
        fn cancel_after(mut self, flag: Arc<AtomicBool>, polls: usize) -> Self {
            self.cancel = Some(flag);
            self.cancel_after_polls = polls;
            self
        }
    }
    impl VerificationBrowser for FakeBrowser {
        fn open(&mut self, room: &str, cancel: &CancelProbe<'_>) -> Result<()> {
            assert!(!cancel(), "open must not run after cancellation");
            assert!(room_is_valid(room), "silent refresh must request one room");
            self.opens.set(self.opens.get() + 1);
            Ok(())
        }
        fn snapshot(
            &mut self,
            cancel: &CancelProbe<'_>,
        ) -> Result<Option<browser::VerificationSnapshot>> {
            assert!(!cancel());
            let polls = self.polls.get() + 1;
            self.polls.set(polls);
            if let Some(flag) = &self.cancel {
                if polls >= self.cancel_after_polls {
                    flag.store(true, Ordering::Release);
                }
            }
            let index = self.index.get();
            let snapshot = self.snapshots.borrow().get(index).cloned();
            let Some((title, cookies)) = snapshot else {
                return Ok(None);
            };
            self.index.set(index + 1);
            Ok(Some(browser::VerificationSnapshot { title, cookies }))
        }
        fn close(&mut self) {
            self.closes.set(self.closes.get() + 1);
        }
    }

    fn temporary_vault() -> (tempfile::TempDir, CookieVault) {
        let directory = tempfile::tempdir().unwrap();
        let vault = CookieVault::open(directory.path().join("credentials")).unwrap();
        (directory, vault)
    }

    #[test]
    fn verification_requires_a_room_title_and_the_device_cookie() {
        assert!(verification_passed("合成直播间标题", &[ttwid()]));
        assert!(verification_passed("合成直播间标题", &[ttwid(), sessionid()]));
        assert!(!verification_passed("验证码中间页", &[ttwid()]));
        assert!(!verification_passed("合成直播间标题", &[sessionid()]));
        assert!(!verification_passed("合成直播间标题", &[]));
        assert!(!verification_passed(
            "合成直播间标题",
            &[cookie("ttwid", "   ")]
        ));
        // The interstitial marker wins even when a normal-looking suffix is added.
        assert!(!verification_passed(
            "验证码中间页 - 合成直播间",
            &[ttwid()]
        ));
    }

    #[test]
    fn verification_page_urls_only_accept_bounded_room_identifiers() {
        assert!(room_is_valid("123456789"));
        assert!(room_is_valid("synthetic-room"));
        assert!(!room_is_valid(""));
        assert!(!room_is_valid("../etc"));
        assert!(!room_is_valid("room?x=1"));
        assert!(!room_is_valid(&"0".repeat(65)));
        assert_eq!(douyin_page_url(""), "https://live.douyin.com/");
        assert_eq!(
            douyin_page_url("123456789"),
            "https://live.douyin.com/123456789"
        );
    }

    #[test]
    fn silent_gate_is_single_flight_and_uses_an_attempt_based_cooldown() {
        let gate = SilentGate::new();
        let first = gate.try_begin_silent(1_000).expect("first attempt starts");
        assert!(
            gate.try_begin_silent(1_000).is_none(),
            "a second automatic attempt must not overlap the first"
        );
        drop(first);
        assert!(
            gate.try_begin_silent(2_000).is_none(),
            "the 60 s attempt cooldown must still block a quick retry"
        );
        let second = gate
            .try_begin_silent(61_000)
            .expect("cooldown from the first attempt expired");
        assert!(
            gate.try_begin_silent(61_500).is_none(),
            "the second attempt is in flight"
        );
        // A success resets the cooldown to a full minute from completion.
        second.finish(true, 66_000);
        assert!(
            gate.try_begin_silent(70_000).is_none(),
            "success must restart the cooldown from completion"
        );
        let third = gate
            .try_begin_silent(126_000)
            .expect("the reset cooldown expired after 60 s");
        drop(third);
    }

    #[test]
    fn user_verification_slot_does_not_impose_the_automatic_cooldown() {
        let gate = SilentGate::new();
        let permit = gate
            .try_begin_user_verification(1_000)
            .expect("user slot starts");
        assert!(
            gate.try_begin_silent(2_000).is_none(),
            "silent refresh must not start a second browser while the user window owns the profile"
        );
        assert!(
            gate.try_begin_user_verification(3_000).is_none(),
            "only one verification window at a time"
        );
        permit.finish(false, 4_000);
        let retry = gate
            .try_begin_user_verification(5_000)
            .expect("a canceled user window must be immediately retryable");
        drop(retry);
    }

    #[test]
    fn cross_process_profile_lease_is_exclusive_and_releases_on_drop() {
        let directory = tempfile::tempdir().unwrap();
        let first = ProfileLease::try_acquire(directory.path())
            .unwrap()
            .expect("first process-like handle takes the profile");
        assert!(
            ProfileLease::try_acquire(directory.path())
                .unwrap()
                .is_none(),
            "a second independent handle must be refused while the first is alive"
        );
        drop(first);
        assert!(
            ProfileLease::try_acquire(directory.path())
                .unwrap()
                .is_some(),
            "releasing the lease lets another attempt through"
        );
    }

    #[test]
    fn cross_process_cooldown_stamp_uses_60_120_300_backoff_and_success_resets_it() {
        let directory = tempfile::tempdir().unwrap();
        let lease = ProfileLease::try_acquire(directory.path()).unwrap().unwrap();
        // The first attempt is allowed; its start stamp gives the base 60 s window.
        assert!(lease.begin_automatic_attempt(1_000));
        assert!(
            !lease.begin_automatic_attempt(60_000),
            "the persistent 60 s window must block a quick retry"
        );
        // The first failure keeps the 60 s step, measured from the failure time.
        lease.note_failure(10_000);
        // The stamp survives the OS lock being released, so a different process
        // that acquires the lease later is throttled by the same state.
        drop(lease);
        let lease = ProfileLease::try_acquire(directory.path()).unwrap().unwrap();
        assert!(
            !lease.begin_automatic_attempt(69_000),
            "the persistent stamp must throttle a second process too"
        );
        assert!(lease.begin_automatic_attempt(70_000));
        // The second consecutive failure waits 120 s.
        lease.note_failure(80_000);
        assert!(!lease.begin_automatic_attempt(199_000));
        assert!(lease.begin_automatic_attempt(200_000));
        // The third and later failures cap at 300 s.
        lease.note_failure(210_000);
        assert!(!lease.begin_automatic_attempt(509_000));
        assert!(lease.begin_automatic_attempt(510_000));
        lease.note_failure(520_000);
        assert_eq!(lease.cooldown_remaining_ms(819_000), 1_000);
        assert!(lease.begin_automatic_attempt(820_000));
        // A success resets the streak: the next failure is back to the 60 s step.
        lease.note_success(830_000);
        assert!(!lease.begin_automatic_attempt(889_000));
        assert!(lease.begin_automatic_attempt(890_000));
        lease.note_failure(900_000);
        assert!(!lease.begin_automatic_attempt(959_999));
        assert!(lease.begin_automatic_attempt(960_000));
        // A missing/corrupt stamp must never disable recovery forever.
        std::fs::write(directory.path().join(VERIFY_COOLDOWN_FILE), "not-a-timestamp").unwrap();
        assert_eq!(lease.cooldown_remaining_ms(1_000_000), 0);
        assert!(lease.begin_automatic_attempt(1_000_000));
    }

    #[cfg(windows)]
    #[test]
    fn silent_refresh_captures_a_verified_snapshot_into_the_encrypted_vault() {
        let (_directory, vault) = temporary_vault();
        let mut window = FakeBrowser::new(vec![
            ("验证码中间页".into(), vec![ttwid()]),
            ("合成直播间标题".into(), vec![ttwid(), sessionid()]),
        ]);
        let clock = FakeClock::new(5_000);
        let cancel = || false;
        let captured =
            refresh_core(&mut window, &vault, "123456789", &cancel, &clock).unwrap();
        assert!(captured);
        assert_eq!(window.opens.get(), 1, "exactly one dedicated window");
        assert_eq!(window.closes.get(), 1, "the owned browser must be closed");
        assert_eq!(
            window.polls.get(),
            2,
            "capture only after the page title changed"
        );
        // At rest the vault must be encrypted, not plaintext.
        let encrypted = std::fs::read(vault.root().join("douyin.cookies.dpapi")).unwrap();
        for marker in [&b"SYNTHETIC_TTWID"[..], &b"SYNTHETIC_SESSION"[..]] {
            assert!(
                !encrypted.windows(marker.len()).any(|row| row == marker),
                "cookie values must not appear in the vault file"
            );
        }
        let header = vault
            .header_for_url(Platform::Douyin, "https://live.douyin.com/123456789")
            .unwrap()
            .expect("saved cookies apply to the room page");
        assert!(header.contains("ttwid=SYNTHETIC_TTWID"));
        assert!(header.contains("sessionid=SYNTHETIC_SESSION"));
    }

    #[cfg(windows)]
    #[test]
    fn silent_refresh_never_saves_the_captcha_page_or_a_missing_ttwid() {
        let cancel = || false;
        for snapshots in [
            vec![("验证码中间页".to_owned(), vec![ttwid(), sessionid()])],
            vec![("合成直播间标题".to_owned(), vec![sessionid()])],
            vec![("合成直播间标题".to_owned(), vec![])],
        ] {
            let (_directory, vault) = temporary_vault();
            let mut window = FakeBrowser::new(snapshots);
            let clock = FakeClock::new(0);
            let captured =
                refresh_core(&mut window, &vault, "123456789", &cancel, &clock).unwrap();
            assert!(!captured);
            assert_eq!(window.closes.get(), 1);
            assert!(
                !vault.has_credentials(Platform::Douyin).unwrap(),
                "an unverified page must not replace the stored credentials"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn cancellation_returns_within_two_seconds_and_closes_the_window_once() {
        let (_directory, vault) = temporary_vault();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut window = FakeBrowser::new(vec![(
            "验证码中间页".into(),
            vec![ttwid()],
        )])
        .cancel_after(cancel.clone(), 1);
        let probe = {
            let cancel = cancel.clone();
            move || cancel.load(Ordering::Acquire)
        };
        let clock = FakeClock::new(0);
        let started = clock.now.get();
        let captured = refresh_core(&mut window, &vault, "123456789", &probe, &clock).unwrap();
        assert!(!captured);
        assert_eq!(window.closes.get(), 1, "closed exactly once on cancel");
        assert_eq!(window.opens.get(), 1);
        assert!(
            clock.now.get().saturating_sub(started) < 2_000,
            "cancellation must stop the poll loop well inside two seconds"
        );
        assert!(
            !vault.has_credentials(Platform::Douyin).unwrap(),
            "cancellation must never save anything"
        );
    }

    #[cfg(windows)]
    #[test]
    fn global_gate_blocks_a_second_silent_refresh_before_it_can_launch_a_browser() {
        let (_directory, vault) = temporary_vault();
        let permit = try_begin_silent(now_ms()).expect("global gate starts free");
        // The second call must be refused by the shared single-flight slot; this
        // is what the playback path relies on when several sessions fail at once.
        assert!(!try_silent_refresh(&vault, "123456789", &|| false).unwrap());
        permit.finish(false, now_ms());
        assert!(
            !vault.has_credentials(Platform::Douyin).unwrap(),
            "a refused attempt must not touch the vault"
        );
    }

    #[cfg(windows)]
    #[test]
    fn user_verification_capture_saves_only_after_the_page_passed() {
        let (directory, vault) = temporary_vault();
        let cancel = || false;
        let mut captcha = FakeBrowser::new(vec![(
            "验证码中间页".into(),
            vec![ttwid(), sessionid()],
        )]);
        assert!(!capture_visible(&mut captcha, &vault, &cancel).unwrap());
        assert!(!vault.has_credentials(Platform::Douyin).unwrap());

        // A stale failure backoff must be cleared by the verified credential write.
        std::fs::write(directory.path().join(VERIFY_COOLDOWN_FILE), "1 3").unwrap();
        let mut verified = FakeBrowser::new(vec![(
            "合成直播间标题".into(),
            vec![ttwid(), sessionid()],
        )]);
        assert!(capture_visible(&mut verified, &vault, &cancel).unwrap());
        let header = vault
            .header_for_url(Platform::Douyin, "https://live.douyin.com/")
            .unwrap()
            .expect("saved cookies apply to the live root page");
        assert!(header.contains("ttwid=SYNTHETIC_TTWID"));
        let stamp = std::fs::read_to_string(directory.path().join(VERIFY_COOLDOWN_FILE)).unwrap();
        assert_eq!(
            stamp.split_whitespace().nth(1),
            Some("0"),
            "writing verified credentials must clear the failure streak"
        );
    }

    /// Explicit live acceptance (opt-in): real network + a real hidden browser +
    /// a real dedicated profile. Ignored by default so the offline suite never
    /// launches a browser or touches the platform. Run with:
    ///   LIVE_HUB_ACCEPT_DOUYIN_VERIFY_ROOM=<web_rid> cargo test --lib \
    ///     live_silent_refresh_acceptance -- --ignored --nocapture
    #[test]
    #[ignore = "uses the real Douyin site and a real hidden browser"]
    fn live_silent_refresh_acceptance() {
        let Ok(room) = std::env::var("LIVE_HUB_ACCEPT_DOUYIN_VERIFY_ROOM") else {
            return;
        };
        // Default: throw-away temp root. LIVE_HUB_ACCEPT_DATA_ROOT points at a
        // persistent root so the hardened profile ACL can be inspected after the
        // run (the temp root is deleted with the test).
        let persistent_root = std::env::var_os("LIVE_HUB_ACCEPT_DATA_ROOT").map(PathBuf::from);
        let temporary;
        let data_root = match persistent_root {
            Some(path) => path,
            None => {
                temporary = tempfile::tempdir().expect("tempdir");
                temporary.path().to_owned()
            }
        };
        std::fs::create_dir_all(&data_root).expect("data root");
        let vault = CookieVault::open(data_root.join("credentials")).expect("vault");
        let refreshed = try_silent_refresh(&vault, &room, &|| false).expect("silent refresh run");
        assert!(refreshed, "silent refresh did not capture a verified session");
        let header = vault
            .header_for_url(Platform::Douyin, "https://live.douyin.com/")
            .expect("scoped header")
            .unwrap_or_default();
        // Do not print cookie values; only the device-cookie property is asserted.
        assert!(header.contains("ttwid="), "captured cookie set lacks ttwid");
        assert!(!header.contains("sessionid="), "acceptance must not require a logged-in account");
    }
}