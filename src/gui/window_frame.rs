//! Windows 11 rounded corners for the launcher's borderless main window.
//!
//! The workbench removes the native title bar (`with_decorations(false)`) and
//! draws its own 34 px chrome, so the whole non-client area is empty. Windows 11
//! does not round such a custom frame by policy; it provides an explicit opt-in
//! instead: `DwmSetWindowAttribute` with `DWMWA_WINDOW_CORNER_PREFERENCE = 33`
//! and `DWMWCP_ROUND = 2`. DWM keeps maximised and snapped windows square by
//! design, so the preference is requested once and never toggled per frame.
//!
//! `dwmapi.dll` is resolved dynamically instead of through an import library,
//! for two reasons:
//! * the GNU toolchain used for this build ships no `dwmapi` import library;
//! * Windows 10 does not support this attribute, and the launcher must keep
//!   starting there. There the symbol lookup or the call fails, the preference
//!   is skipped, and the window keeps the platform's square corners.
//!
//! The HWND is resolved with `EnumThreadWindows(GetCurrentThreadId())` plus an
//! exact window-title match, from the UI thread while eframe builds the app.
//! eframe's `CreationContext` only exposes the raw handle through the
//! `raw-window-handle` trait (a transitive dependency this crate does not
//! depend on directly), and a process-wide `FindWindow` by title would be
//! ambiguous with several launcher instances running. Enumerating windows of
//! *this thread* cannot pick a window owned by another instance.
//!
//! Every failure path is a no-op plus one diagnostic line: rounded corners are
//! a visual detail, never a launch requirement.

/// `DWMWA_WINDOW_CORNER_PREFERENCE` from dwmapi.h: the `DWMWINDOWATTRIBUTE`
/// member that selects the corner rounding of a top-level window.
///
/// This is the Windows 11 ABI value; it must not drift.
pub const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;

/// `DWMWCP_ROUND` from the `DWM_WINDOW_CORNER_PREFERENCE` enum (dwmapi.h):
/// round the corners with the standard Windows 11 radius.
///
/// The other enum members are `DWMWCP_DEFAULT = 0` (system decides),
/// `DWMWCP_DONOTROUND = 1` and `DWMWCP_ROUNDSMALL = 3`.
pub const DWMWCP_ROUND: u32 = 2;

/// Size in bytes of the `DWORD` preference passed to `DwmSetWindowAttribute`.
pub const CORNER_PREFERENCE_SIZE: u32 = 4;

/// The `DWM_WINDOW_CORNER_PREFERENCE` member name for a raw value, for
/// diagnostics and for the ABI contract test.
pub const fn corner_preference_name(value: u32) -> &'static str {
    match value {
        0 => "DWMWCP_DEFAULT",
        1 => "DWMWCP_DONOTROUND",
        2 => "DWMWCP_ROUND",
        3 => "DWMWCP_ROUNDSMALL",
        _ => "unknown",
    }
}

/// Parses the diagnostic `LIVE_HUB_WINDOW_PROBE` value (`x,y`, physical screen
/// pixels; negative coordinates are valid on a multi-monitor desktop).
///
/// The value exists for the corner screenshot probe only: it makes `gui::run`
/// create the real window inactive and at a known position, so the probe can
/// photograph the real corners without ever taking the user's foreground.
/// Malformed or absent values return `None` and a launch stays completely
/// normal.
pub fn probe_placement(value: Option<&str>) -> Option<[f32; 2]> {
    let (x, y) = value?.trim().split_once(',')?;
    let x: f32 = x.trim().parse().ok()?;
    let y: f32 = y.trim().parse().ok()?;
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    Some([x, y])
}

/// Requests Windows 11 rounded corners for the launcher's top-level window.
///
/// Must be called on the UI thread from `Launcher::new`, before the first frame
/// is rendered; the window already exists at that point. `expected_title` is the
/// exact title eframe was started with (see `gui::WINDOW_TITLE`). No-op outside
/// Windows, on Windows 10, and when the window cannot be resolved.
pub fn install(expected_title: &str) {
    #[cfg(windows)]
    platform::install(expected_title);
    #[cfg(not(windows))]
    let _ = expected_title;
}

#[cfg(windows)]
mod platform {
    use super::{
        CORNER_PREFERENCE_SIZE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
        corner_preference_name,
    };
    use crate::diag::{self, Level};
    use std::ffi::c_void;
    use std::sync::OnceLock;

    type Hwnd = *mut c_void;
    type Bool = i32;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn EnumThreadWindows(
            thread: u32,
            callback: unsafe extern "system" fn(Hwnd, isize) -> Bool,
            data: isize,
        ) -> Bool;
        fn GetWindowTextW(window: Hwnd, text: *mut u16, max: i32) -> i32;
        fn IsZoomed(window: Hwnd) -> Bool;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }

    /// `dwmapi!DwmSetWindowAttribute`, resolved at runtime.
    type DwmSetWindowAttribute = unsafe extern "system" fn(Hwnd, u32, *const c_void, u32) -> i32;
    /// `dwmapi!DwmGetWindowAttribute`, used to read the preference back as
    /// launch evidence; optional because it is not available everywhere.
    type DwmGetWindowAttribute = unsafe extern "system" fn(Hwnd, u32, *mut c_void, u32) -> i32;

    struct DwmApi {
        set: DwmSetWindowAttribute,
        get: Option<DwmGetWindowAttribute>,
    }

    /// Resolves `dwmapi.dll` and its two exports once per process.
    fn dwm_api() -> Option<&'static DwmApi> {
        static API: OnceLock<Option<DwmApi>> = OnceLock::new();
        API.get_or_init(load_dwm_api).as_ref()
    }

    fn load_dwm_api() -> Option<DwmApi> {
        let module_name: Vec<u16> = "dwmapi.dll\0".encode_utf16().collect();
        // SAFETY: NUL-terminated UTF-16 module name; the handle is kept for the
        // lifetime of the process (the module is never freed).
        let module = unsafe { LoadLibraryW(module_name.as_ptr()) };
        if module.is_null() {
            diag::log("window", Level::Debug, "corner: dwmapi.dll not available");
            return None;
        }
        // SAFETY: dwmapi exports both functions on every Windows version that
        // ships dwmapi.dll; a missing export is reported as a null pointer.
        let set = unsafe { GetProcAddress(module, b"DwmSetWindowAttribute\0".as_ptr()) };
        let get = unsafe { GetProcAddress(module, b"DwmGetWindowAttribute\0".as_ptr()) };
        if set.is_null() {
            diag::log(
                "window",
                Level::Debug,
                "corner: DwmSetWindowAttribute not exported; keeping square corners",
            );
            return None;
        }
        // SAFETY: the exported symbols have the documented dwmapi signatures.
        Some(DwmApi {
            set: unsafe { std::mem::transmute::<*mut c_void, DwmSetWindowAttribute>(set) },
            get: if get.is_null() {
                None
            } else {
                Some(unsafe { std::mem::transmute::<*mut c_void, DwmGetWindowAttribute>(get) })
            },
        })
    }

    struct Search {
        expected: Vec<u16>,
        found: Hwnd,
    }

    unsafe extern "system" fn find_window(window: Hwnd, data: isize) -> Bool {
        // SAFETY: EnumThreadWindows calls this synchronously while the Search
        // it was handed is alive; the title buffer is stack local.
        let search = unsafe { &mut *(data as *mut Search) };
        let mut buffer = [0u16; 256];
        let length = unsafe { GetWindowTextW(window, buffer.as_mut_ptr(), buffer.len() as i32) };
        if length > 0 && buffer[..length as usize] == search.expected[..] {
            search.found = window;
            return 0; // stop enumerating
        }
        1 // continue
    }

    /// The launcher's own top-level window, matched by exact title on this
    /// thread only (never another process's window).
    fn launcher_window(title: &str) -> Option<Hwnd> {
        let mut search = Search {
            expected: title.encode_utf16().collect(),
            found: std::ptr::null_mut(),
        };
        // SAFETY: the callback only touches `search`, which outlives the
        // synchronous enumeration.
        unsafe {
            EnumThreadWindows(
                GetCurrentThreadId(),
                find_window,
                &mut search as *mut Search as isize,
            );
        }
        (!search.found.is_null()).then_some(search.found)
    }

    pub fn install(expected_title: &str) {
        let Some(window) = launcher_window(expected_title) else {
            diag::log(
                "window",
                Level::Warn,
                "corner: launcher window not found on the UI thread; keeping square corners",
            );
            return;
        };
        let Some(api) = dwm_api() else {
            diag::log(
                "window",
                Level::Debug,
                "corner: DWM corner preference unavailable; keeping square corners",
            );
            return;
        };
        let preference = DWMWCP_ROUND;
        // SAFETY: the preference is a live local DWORD of the documented size and
        // the window handle is the launcher's own top-level window.
        let set_result = unsafe {
            (api.set)(
                window,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                (&preference as *const u32).cast::<c_void>(),
                CORNER_PREFERENCE_SIZE,
            )
        };
        let mut read_back: u32 = u32::MAX;
        let get_result = api.get.as_ref().map(|get| {
            // SAFETY: same handle and size contract as the set call above.
            unsafe {
                (get)(
                    window,
                    DWMWA_WINDOW_CORNER_PREFERENCE,
                    (&mut read_back as *mut u32).cast::<c_void>(),
                    CORNER_PREFERENCE_SIZE,
                )
            }
        });
        let read_back = if read_back == u32::MAX {
            "unavailable".to_owned()
        } else {
            format!("{read_back} ({})", corner_preference_name(read_back))
        };
        // SAFETY: read-only query of this window's zoom state.
        let maximized = unsafe { IsZoomed(window) } != 0;
        let level = if set_result >= 0 {
            Level::Info
        } else {
            Level::Warn
        };
        diag::log(
            "window",
            level,
            &format!(
                "corner: hwnd=0x{:X} attribute={} preference={} ({}) set_hresult=0x{:08X} get_hresult={} readback={} maximized={maximized}",
                window as usize,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                preference,
                corner_preference_name(preference),
                set_result as u32,
                match get_result {
                    Some(result) => format!("0x{:08X}", result as u32),
                    None => "unavailable".to_owned(),
                },
                read_back,
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dwm_attribute_values_match_the_win32_contract() {
        // dwmapi.h: DWMWINDOWATTRIBUTE.DWMWA_WINDOW_CORNER_PREFERENCE is 33 and
        // DWM_WINDOW_CORNER_PREFERENCE.DWMWCP_ROUND is 2; DwmSetWindowAttribute
        // takes a DWORD (cbAttribute = sizeof(DWORD)). These are Windows ABI
        // values and must never drift.
        assert_eq!(DWMWA_WINDOW_CORNER_PREFERENCE, 33);
        assert_eq!(DWMWCP_ROUND, 2);
        assert_eq!(CORNER_PREFERENCE_SIZE, std::mem::size_of::<u32>() as u32);
        assert_eq!(corner_preference_name(0), "DWMWCP_DEFAULT");
        assert_eq!(corner_preference_name(1), "DWMWCP_DONOTROUND");
        assert_eq!(corner_preference_name(2), "DWMWCP_ROUND");
        assert_eq!(corner_preference_name(3), "DWMWCP_ROUNDSMALL");
        assert_eq!(corner_preference_name(4), "unknown");
    }

    #[test]
    fn probe_placement_accepts_only_a_finite_screen_coordinate_pair() {
        assert_eq!(probe_placement(None), None);
        assert_eq!(probe_placement(Some("")), None);
        assert_eq!(probe_placement(Some("   ")), None);
        assert_eq!(probe_placement(Some("100,200")), Some([100.0, 200.0]));
        assert_eq!(
            probe_placement(Some(" -3000 , 40 ")),
            Some([-3000.0, 40.0]),
            "left/above monitors use negative virtual-screen coordinates"
        );
        assert_eq!(probe_placement(Some("100")), None);
        assert_eq!(probe_placement(Some("100,")), None);
        assert_eq!(probe_placement(Some(",200")), None);
        assert_eq!(probe_placement(Some("100,abc")), None);
        assert_eq!(probe_placement(Some("100,200,300")), None);
        assert_eq!(probe_placement(Some("nan,0")), None);
        assert_eq!(probe_placement(Some("inf,0")), None);
        assert_eq!(probe_placement(Some("1e999,0")), None, "infinity is rejected");
    }
}
