//! Bind Ctrl+R to the playback window of one PotPlayer launched by this app.
//! The helper exits with that process, even if the launcher is closed first.

#[cfg(windows)]
pub(super) use native::start;
#[cfg(windows)]
pub use native::watch;

#[cfg(not(windows))]
pub fn watch(_: u32, _: u64) -> anyhow::Result<()> {
    anyhow::bail!("PotPlayer 需要 Windows")
}

#[cfg(windows)]
mod native {
    use anyhow::{Context, Result, ensure};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::{
        io,
        process::{Child, Stdio},
        ptr,
    };
    use windows_sys::Win32::{
        Foundation::{FILETIME, HANDLE, HWND, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::{
            GetCurrentThreadId, GetProcessTimes, INFINITE, OpenProcess,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        },
        UI::{
            Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
            Input::KeyboardAndMouse::{
                MOD_CONTROL, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey,
            },
            WindowsAndMessaging::{
                EVENT_SYSTEM_FOREGROUND, GetClassNameW, GetForegroundWindow,
                GetWindowThreadProcessId, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx,
                PM_REMOVE, PeekMessageW, PostMessageW, PostThreadMessageW, QS_ALLINPUT,
                WINEVENT_OUTOFCONTEXT, WM_APP, WM_COMMAND, WM_HOTKEY,
            },
        },
    };

    const VK_R: u32 = b'R' as u32;
    // PotPlayer's native "Reopen Current/Last File" menu command (also bound to Ctrl+Y).
    const REOPEN_FILE: usize = 10385;

    fn started(handle: HANDLE) -> io::Result<u64> {
        let (mut creation, mut exit, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        if unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
    }

    pub(in crate::playback) fn start(player: &Child) -> Result<()> {
        super::super::command(&std::env::current_exe()?)
            .arg("potplayer-reload")
            .arg(player.id().to_string())
            .arg(started(player.as_raw_handle())?.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(())
    }

    fn foreground_player(pid: u32) -> HWND {
        let hwnd = unsafe { GetForegroundWindow() };
        let mut owner = 0;
        if hwnd.is_null()
            || unsafe { GetWindowThreadProcessId(hwnd, &mut owner) } == 0
            || owner != pid
        {
            return ptr::null_mut();
        }
        let mut class = [0u16; 64];
        let len = unsafe { GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32) };
        let name = String::from_utf16_lossy(&class[..len.max(0) as usize]);
        if name == "PotPlayer64" || name == "PotPlayer" {
            hwnd
        } else {
            ptr::null_mut()
        }
    }

    unsafe extern "system" fn focus_changed(
        _: HWINEVENTHOOK,
        _: u32,
        _: HWND,
        _: i32,
        _: i32,
        _: u32,
        _: u32,
    ) {
        // Out-of-context callbacks run on this helper's message-pumping thread.
        unsafe { PostThreadMessageW(GetCurrentThreadId(), WM_APP, 0, 0) };
    }

    pub fn watch(pid: u32, expected_start: u64) -> Result<()> {
        let handle = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error()).context("PotPlayer 已退出或无法访问");
        }
        let process = unsafe { OwnedHandle::from_raw_handle(handle) };
        ensure!(
            started(process.as_raw_handle())? == expected_start,
            "PotPlayer 会话已更换"
        );

        let mut msg = MSG::default();
        unsafe { PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, 0) }; // create the thread queue
        let hook = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                ptr::null_mut(),
                Some(focus_changed),
                0,
                0,
                WINEVENT_OUTOFCONTEXT,
            )
        };
        let mut bound: HWND = ptr::null_mut();
        let mut id: i32 = 1;
        let mut result = Ok(());
        while unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT {
            let target = foreground_player(pid);
            if !bound.is_null() && bound != target {
                unsafe { UnregisterHotKey(ptr::null_mut(), id) };
                bound = ptr::null_mut();
                // A queued event from an earlier registration cannot dispatch after focus returns.
                id = id % 0xbfff + 1;
            }
            if bound.is_null()
                && !target.is_null()
                && unsafe { RegisterHotKey(ptr::null_mut(), id, MOD_CONTROL | MOD_NOREPEAT, VK_R) }
                    != 0
            {
                bound = target;
            }
            let mut focus_updated = false;
            while unsafe { PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
                if msg.message == WM_APP {
                    focus_updated = true;
                }
                if msg.message == WM_HOTKEY
                    && msg.wParam == id as usize
                    && !bound.is_null()
                    && foreground_player(pid) == bound
                {
                    unsafe { PostMessageW(bound, WM_COMMAND, REOPEN_FILE, 0) };
                }
            }
            if focus_updated {
                continue;
            }
            // Retry after a competing registration is released. Normally the thread
            // sleeps until a foreground-change notification, a hotkey, or player exit.
            let retry = if hook.is_null() || bound != target {
                50
            } else {
                INFINITE
            };
            match unsafe {
                MsgWaitForMultipleObjectsEx(1, &handle, retry, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
            } {
                WAIT_OBJECT_0 => break,
                WAIT_FAILED => {
                    result = Err(io::Error::last_os_error().into());
                    break;
                }
                _ => (),
            }
        }
        if !bound.is_null() {
            unsafe { UnregisterHotKey(ptr::null_mut(), id) };
        }
        if !hook.is_null() {
            unsafe { UnhookWinEvent(hook) };
        }
        result
    }
}
