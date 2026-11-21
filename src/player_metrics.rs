use chrono::{DateTime, Local, Utc};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Default)]
pub(crate) struct PlayerMetrics {
    pub(crate) state_file: Option<PathBuf>,
    last_read: Option<(Instant, u32, String)>,
}

impl PlayerMetrics {
    pub(crate) fn summary(&mut self, pid: u32, force: bool, player: crate::core::Player) -> String {
        if !force
            && let Some((at, cached_pid, text)) = &self.last_read
            && *cached_pid == pid
            && at.elapsed() < Duration::from_millis(500)
        {
            return text.clone();
        }
        let now = Utc::now().timestamp();
        let text = if player == crate::core::Player::PotPlayer {
            format!(
                "PotPlayer {}  ·  A-V —  ·  丢帧 解码 — / 输出 —  ·  缓存 —（PotPlayer 未提供）",
                pot_status(pid)
            )
        } else {
            self.state_file
                .as_deref()
                .and_then(|p| {
                    (0..2)
                        .filter_map(|slot| read_slot(p, slot, pid, now))
                        .max_by_key(|item| item.0)
                })
                .map(|(_, text)| text)
                .unwrap_or_else(|| "mpv 暂无新采样".into())
        };
        self.last_read = Some((Instant::now(), pid, text.clone()));
        text
    }
}

fn read_slot(path: &Path, slot: u8, pid: u32, now: i64) -> Option<(i64, String)> {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".metrics{slot}"));
    let mut text = String::new();
    File::open(name)
        .ok()?
        .take(256)
        .read_to_string(&mut text)
        .ok()?;
    if text.len() >= 256 {
        return None;
    }
    parse(text.strip_suffix('\n')?.trim_end_matches('\r'), pid, now)
}

fn parse(line: &str, pid: u32, now: i64) -> Option<(i64, String)> {
    let mut parts = line.split(' ');
    if parts.next()? != "v1" {
        return None;
    }
    let stamp = parts.next()?.parse::<i64>().ok()?;
    if parts.next()?.parse::<u32>().ok()? != pid || !(now.saturating_sub(4)..=now).contains(&stamp)
    {
        return None;
    }
    let av = number(parts.next()?, true);
    let decoder = count(parts.next()?);
    let output = count(parts.next()?);
    let cache = number(parts.next()?, false);
    if parts.next().is_some() {
        return None;
    }
    let time = DateTime::from_timestamp(stamp, 0)?.with_timezone(&Local);
    Some((
        stamp,
        format!(
            "A-V {av} ms  ·  丢帧 解码 {decoder} / 输出 {output}  ·  缓存 {cache} s  ·  采样 {}",
            time.format("%H:%M:%S")
        ),
    ))
}

fn number(field: &str, signed: bool) -> String {
    match field.parse::<f64>() {
        Ok(n) if n.is_finite() && n.abs() <= 1e10 && (signed || n >= 0.0) => {
            if signed && n != 0.0 {
                format!("{n:+.2}")
            } else {
                format!("{n:.2}")
            }
        }
        _ => "—".into(),
    }
}

fn count(field: &str) -> String {
    field
        .parse::<u64>()
        .map(|n| n.to_string())
        .unwrap_or_else(|_| "—".into())
}

#[cfg(windows)]
fn pot_status(pid: u32) -> String {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetWindowThreadProcessId, SMTO_ABORTIFHUNG,
        SendMessageTimeoutW, WM_USER,
    };
    struct Search {
        pid: u32,
        window: HWND,
    }
    unsafe extern "system" fn visit(window: HWND, context: LPARAM) -> i32 {
        let search = unsafe { &mut *(context as *mut Search) };
        let mut owner = 0;
        unsafe { GetWindowThreadProcessId(window, &mut owner) };
        if owner == search.pid {
            let mut class = [0u16; 64];
            let n = unsafe { GetClassNameW(window, class.as_mut_ptr(), 64) };
            if matches!(
                String::from_utf16_lossy(&class[..n.max(0) as usize]).as_str(),
                "PotPlayer64" | "PotPlayer"
            ) {
                search.window = window;
                return 0;
            }
        }
        1
    }
    let mut search = Search {
        pid,
        window: std::ptr::null_mut(),
    };
    unsafe { EnumWindows(Some(visit), (&mut search as *mut Search) as LPARAM) };
    let read = |command| -> Option<usize> {
        let mut result = 0;
        if search.window.is_null() {
            return None;
        }
        let ok = unsafe {
            SendMessageTimeoutW(
                search.window,
                WM_USER,
                command,
                0,
                SMTO_ABORTIFHUNG,
                25,
                &mut result,
            )
        };
        (ok != 0).then_some(result)
    };
    let state = match read(0x5006) {
        Some(1) => "暂停",
        Some(2) => "播放中",
        Some(_) => return "停止或无媒体".into(),
        None => return "状态不可用".into(),
    };
    match read(0x5004).filter(|p| *p <= i32::MAX as usize) {
        Some(pos) => format!("{state} · 播放位置 {:.1} s", pos as f64 / 1000.0),
        None => state.into(),
    }
}

#[cfg(not(windows))]
fn pot_status(_: u32) -> String {
    "状态不可用".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn player_metrics_require_current_session_and_fresh_complete_sample() {
        let sample = "v1 100 9 +2.63 0 31 1.8";
        let (_, label) = parse(sample, 9, 101).unwrap();
        assert!(
            label.contains("+2.63 ms")
                && label.contains("解码 0 / 输出 31")
                && label.contains("1.80 s")
        );
        assert!(parse(sample, 8, 101).is_none() && parse(sample, 9, 105).is_none());
        assert!(parse(sample, 9, 99).is_none() && parse("v1 100 9 1 2", 9, 100).is_none());
        let (_, unknown) = parse("v1 100 9 NaN - 0 -", 9, 100).unwrap();
        assert!(unknown.contains("A-V —") && unknown.contains("解码 — / 输出 0"));
    }
}
