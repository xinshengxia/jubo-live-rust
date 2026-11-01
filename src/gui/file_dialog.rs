//! Native "browse for executable" support and player/tool path validation.
//!
//! The dialog is a raw FFI call into comdlg32 so no new crate or Cargo feature is
//! needed. Everything else here is pure and offline-testable: UTF-16 filter
//! construction, candidate ordering for auto-detection and path validation.

use anyhow::{Result, bail, ensure};
use std::path::{Path, PathBuf};

/// Which configurable external tool a path field refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Mpv,
    PotPlayer,
    YtDlp,
}

impl ToolKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Mpv => "mpv",
            Self::PotPlayer => "PotPlayer",
            Self::YtDlp => "yt-dlp",
        }
    }

    pub fn dialog_title(self) -> &'static str {
        match self {
            Self::Mpv => "选择 mpv 可执行文件",
            Self::PotPlayer => "选择 PotPlayer 可执行文件",
            Self::YtDlp => "选择 yt-dlp 可执行文件",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Mpv => "players/mpv.exe",
            Self::PotPlayer => "players/PotPlayerMini64.exe",
            Self::YtDlp => "tools/yt-dlp.exe",
        }
    }

    /// Executable names checked by auto-detection, most specific first.
    pub fn names(self) -> &'static [&'static str] {
        match self {
            Self::Mpv => &["mpv.exe", "mpv"],
            Self::PotPlayer => &["PotPlayerMini64.exe", "PotPlayerMini.exe"],
            Self::YtDlp => &["yt-dlp.exe", "yt-dlp"],
        }
    }
}

/// UTF-16 filter string for the Win32 dialog: description\0pattern\0 ... \0\0.
/// Split so the format is unit-testable without opening a dialog.
pub fn build_filter(pairs: &[(&str, &str)]) -> Vec<u16> {
    let mut output = Vec::new();
    for (description, pattern) in pairs {
        output.extend(description.encode_utf16());
        output.push(0);
        output.extend(pattern.encode_utf16());
        output.push(0);
    }
    output.push(0);
    output
}

/// The concrete filters shown for one tool. Keeping the specific file first is
/// friendlier, but any executable remains selectable through the second entry.
pub fn tool_filter(tool: ToolKind) -> Vec<u16> {
    match tool {
        ToolKind::Mpv => build_filter(&[
            ("mpv (mpv.exe)", "mpv.exe"),
            ("可执行文件 (*.exe)", "*.exe"),
        ]),
        ToolKind::PotPlayer => build_filter(&[
            (
                "PotPlayer (PotPlayerMini64.exe; PotPlayerMini.exe)",
                "PotPlayer*.exe",
            ),
            ("可执行文件 (*.exe)", "*.exe"),
        ]),
        ToolKind::YtDlp => build_filter(&[
            ("yt-dlp (yt-dlp.exe)", "yt-dlp.exe"),
            ("可执行文件 (*.exe)", "*.exe"),
        ]),
    }
}

/// One tool path from the settings draft:
/// - empty means "auto-detect later" and is allowed;
/// - otherwise it must be absolute, exist, be a regular file and be large enough
///   to be a real executable (a 0-byte or truncated download is not usable).
pub fn validate_tool_path(tool: ToolKind, value: &str) -> Result<Option<PathBuf>> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(trimmed);
    ensure!(
        path.is_absolute(),
        "{} 路径必须是绝对路径（例如 C:\\tools\\{}），或留空自动查找；请使用“浏览…”选择",
        tool.label(),
        tool.names()[0]
    );
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(_) => bail!(
            "{} 路径不存在：{}；请使用“浏览…”或“自动检测”重新选择",
            tool.label(),
            path.display()
        ),
    };
    ensure!(
        metadata.is_file(),
        "{} 路径不是普通文件：{}",
        tool.label(),
        path.display()
    );
    ensure!(
        metadata.len() > 1024,
        "{} 文件过小（{} 字节），看起来不像可执行文件：{}",
        tool.label(),
        metadata.len(),
        path.display()
    );
    Ok(Some(path))
}

/// Validates all three settings-page paths before a save. The returned Settings
/// fields are exactly the validated values, so a failed save cannot replace the
/// committed configuration.
pub fn validate_tool_paths(
    mpv: &str,
    potplayer: &str,
    yt_dlp: &str,
) -> Result<(Option<PathBuf>, Option<PathBuf>, Option<PathBuf>)> {
    Ok((
        validate_tool_path(ToolKind::Mpv, mpv)?,
        validate_tool_path(ToolKind::PotPlayer, potplayer)?,
        validate_tool_path(ToolKind::YtDlp, yt_dlp)?,
    ))
}

/// A short red/green label for a path field, without touching the filesystem
/// beyond the one metadata probe the validation already does.
pub fn path_status(tool: ToolKind, value: &str) -> Option<(bool, String)> {
    if value.trim().is_empty() {
        return None;
    }
    match validate_tool_path(tool, value) {
        Ok(Some(_)) => Some((true, "路径有效".to_owned())),
        Ok(None) => None,
        Err(error) => Some((false, format!("{error:#}"))),
    }
}

/// Ordered candidate directories for auto-detection. Pure: the caller injects the
/// executable directory, PATH entries, program-files roots and (optionally)
/// explicit extra directories, so tests never depend on the real machine.
pub fn tool_candidates(
    tool: ToolKind,
    executable_dir: &Path,
    path_dirs: &[PathBuf],
    program_files: &[PathBuf],
    extra_dirs: &[PathBuf],
) -> Vec<PathBuf> {
    let mut directories: Vec<PathBuf> = Vec::new();
    let mut push = |directory: PathBuf| {
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    };
    // Same order as the existing runtime find_player fallback: next to the app,
    // then bin/ and bin/mpv, then PATH, then well-known install locations.
    push(executable_dir.to_path_buf());
    push(executable_dir.join("bin"));
    if tool == ToolKind::Mpv {
        push(executable_dir.join("bin").join("mpv"));
    }
    for directory in path_dirs {
        push(directory.clone());
    }
    for root in program_files {
        match tool {
            ToolKind::Mpv => push(root.join("mpv")),
            ToolKind::PotPlayer => push(root.join("DAUM").join("PotPlayer")),
            ToolKind::YtDlp => {
                push(root.join("yt-dlp"));
                push(root.join("yt-dlp").join("bin"));
            }
        }
    }
    for directory in extra_dirs {
        push(directory.clone());
    }
    directories
}

/// First existing candidate file for the tool, in the documented order.
pub fn detect_tool_in(tool: ToolKind, directories: &[PathBuf]) -> Option<PathBuf> {
    for directory in directories {
        for name in tool.names() {
            let path = directory.join(name);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

/// Auto-detection using the real process environment. Separated from the pure
/// candidate builder above so tests can stay hermetic.
pub fn detect_tool(tool: ToolKind) -> Option<PathBuf> {
    let executable_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    let path_dirs = std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .filter(|directory| directory.is_absolute())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut program_files = Vec::new();
    for key in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Some(value) = std::env::var_os(key) {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                program_files.push(path);
            }
        }
    }
    let directories = tool_candidates(tool, &executable_dir, &path_dirs, &program_files, &[]);
    detect_tool_in(tool, &directories)
}

/// Opens the native Windows "open file" dialog. Returns None when the user
/// cancels, the platform is not Windows, or the dialog cannot be shown (the UI
/// must keep working). The call blocks the UI: this is a native modal dialog.
pub fn pick_executable(tool: ToolKind, current: &str) -> Option<PathBuf> {
    let initial = PathBuf::from(current.trim());
    let initial_dir = initial
        .parent()
        .filter(|parent| parent.is_dir())
        .map(Path::to_path_buf);
    native_pick(
        &tool_filter(tool),
        tool.dialog_title(),
        initial_dir.as_deref(),
    )
}

/// Opens the native Windows "open file" dialog with a caller-supplied filter.
///
/// Same contract as [pick_executable] (returns None when the user cancels, the
/// platform is not Windows, or the dialog cannot be shown) but without the
/// executable-only filter. The favourites/history import UI supplies the UTF-16
/// filter through [build_filter], for example
/// `build_filter(&[("收藏 JSON (*.json)", "*.json")])`.
pub fn open_file_name(filter: &[u16], title: &str, initial_dir: Option<&Path>) -> Option<PathBuf> {
    native_pick(filter, title, initial_dir)
}

/// Raw Win32 OPENFILENAMEW. Field order and pointer-sized fields follow the SDK
/// declaration so the struct size matches what comdlg32 validates (152 on 64-bit).
#[cfg(windows)]
#[repr(C)]
struct OpenFileNameW {
    l_struct_size: u32,
    hwnd_owner: *mut std::ffi::c_void,
    h_instance: *mut std::ffi::c_void,
    lpstr_filter: *const u16,
    lpstr_custom_filter: *mut u16,
    n_max_cust_filter: u32,
    n_filter_index: u32,
    lpstr_file: *mut u16,
    n_max_file: u32,
    lpstr_file_title: *mut u16,
    n_max_file_title: u32,
    lpstr_initial_dir: *const u16,
    lpstr_title: *const u16,
    flags: u32,
    n_file_offset: u16,
    n_file_extension: u16,
    lpstr_def_ext: *const u16,
    l_cust_data: isize,
    lpfn_hook: *mut std::ffi::c_void,
    lp_template_name: *const u16,
    pv_reserved: *mut std::ffi::c_void,
    dw_reserved: u32,
    flags_ex: u32,
}

#[cfg(windows)]
fn native_pick(filter: &[u16], title: &str, initial_dir: Option<&Path>) -> Option<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    #[link(name = "comdlg32")]
    unsafe extern "system" {
        fn GetOpenFileNameW(ofn: *mut OpenFileNameW) -> i32;
        fn CommDlgExtendedError() -> u32;
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetActiveWindow() -> *mut std::ffi::c_void;
    }

    const OFN_PATH_MUST_EXIST: u32 = 0x0000_0800;
    const OFN_FILE_MUST_EXIST: u32 = 0x0000_1000;
    const OFN_EXPLORER: u32 = 0x0008_0000;
    const OFN_NO_CHANGE_DIR: u32 = 0x0000_0008;
    const OFN_DONT_ADD_TO_RECENT: u32 = 0x0200_0000;

    let mut file_buffer = vec![0u16; 32_768];
    let title_wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let initial_wide: Vec<u16> = initial_dir
        .map(|path| {
            path.as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect()
        })
        .unwrap_or_default();
    let mut dialog = OpenFileNameW {
        l_struct_size: std::mem::size_of::<OpenFileNameW>() as u32,
        hwnd_owner: unsafe { GetActiveWindow() },
        h_instance: std::ptr::null_mut(),
        lpstr_filter: filter.as_ptr(),
        lpstr_custom_filter: std::ptr::null_mut(),
        n_max_cust_filter: 0,
        n_filter_index: 1,
        lpstr_file: file_buffer.as_mut_ptr(),
        n_max_file: file_buffer.len() as u32,
        lpstr_file_title: std::ptr::null_mut(),
        n_max_file_title: 0,
        lpstr_initial_dir: if initial_wide.is_empty() {
            std::ptr::null()
        } else {
            initial_wide.as_ptr()
        },
        lpstr_title: title_wide.as_ptr(),
        flags: OFN_EXPLORER
            | OFN_PATH_MUST_EXIST
            | OFN_FILE_MUST_EXIST
            | OFN_NO_CHANGE_DIR
            | OFN_DONT_ADD_TO_RECENT,
        n_file_offset: 0,
        n_file_extension: 0,
        lpstr_def_ext: std::ptr::null(),
        l_cust_data: 0,
        lpfn_hook: std::ptr::null_mut(),
        lp_template_name: std::ptr::null(),
        pv_reserved: std::ptr::null_mut(),
        dw_reserved: 0,
        flags_ex: 0,
    };
    let shown = unsafe { GetOpenFileNameW(&mut dialog) };
    if shown == 0 {
        // 0 with no extended error means the user cancelled: not a failure.
        let _ = unsafe { CommDlgExtendedError() };
        return None;
    }
    let length = file_buffer.iter().position(|unit| *unit == 0)?;
    let path = PathBuf::from(std::ffi::OsString::from_wide(&file_buffer[..length]));
    path.is_file().then_some(path)
}

#[cfg(not(windows))]
fn native_pick(_filter: &[u16], _title: &str, _initial_dir: Option<&Path>) -> Option<PathBuf> {
    // The native file dialog is Windows-only; other platforms keep the text field.
    None
}

/// The Win32 OPENFILENAMEW size, exposed for an offline layout assertion. A wrong
/// size makes the native dialog refuse to open, so it is checked on 64-bit targets.
#[cfg(windows)]
pub fn open_file_name_size() -> usize {
    std::mem::size_of::<OpenFileNameW>()
}

/// Fills the Win32 save-dialog file buffer with `default_name` (UTF-16 + NUL).
///
/// Split out from the FFI call so the encoding and truncation behaviour is
/// testable without opening a dialog. The buffer stays NUL-terminated even when
/// the requested name is longer than the buffer.
fn fill_save_buffer(buffer: &mut [u16], default_name: &str) {
    for unit in buffer.iter_mut() {
        *unit = 0;
    }
    let capacity = buffer.len().saturating_sub(1);
    let encoded: Vec<u16> = default_name
        .encode_utf16()
        .filter(|unit| *unit != 0)
        .take(capacity)
        .collect();
    buffer[..encoded.len()].copy_from_slice(&encoded);
}

/// Opens the native Windows "save file" dialog and returns the chosen path.
///
/// Returns `None` when the user cancels, the platform is not Windows, or the
/// dialog cannot be shown; the caller keeps running and shows its own status
/// message. The dialog uses `OFN_OVERWRITEPROMPT` (the shell asks before
/// replacing an existing file), `OFN_PATH_MUST_EXIST` and `OFN_DONT_ADD_TO_RECENT`.
/// The caller supplies the UTF-16 filter (see [build_filter]) and a safe default
/// name (see `crate::favorites_transfer::export_file_name`); the chosen file is
/// not created here, so the caller must handle the write error.
pub fn save_file_name(
    filter: &[u16],
    title: &str,
    default_name: &str,
    initial_dir: Option<&Path>,
) -> Option<PathBuf> {
    native_save(filter, title, default_name, initial_dir)
}

/// Raw Win32 `GetSaveFileNameW` call. Reuses the SDK-validated
/// `OPENFILENAMEW` layout (152 bytes on 64-bit, asserted in tests); only the
/// flag set and the pre-filled file buffer differ from the open dialog.
#[cfg(windows)]
fn native_save(
    filter: &[u16],
    title: &str,
    default_name: &str,
    initial_dir: Option<&Path>,
) -> Option<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    #[link(name = "comdlg32")]
    unsafe extern "system" {
        fn GetSaveFileNameW(ofn: *mut OpenFileNameW) -> i32;
        fn CommDlgExtendedError() -> u32;
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetActiveWindow() -> *mut std::ffi::c_void;
    }

    const OFN_OVERWRITEPROMPT: u32 = 0x0000_0002;
    const OFN_NO_CHANGE_DIR: u32 = 0x0000_0008;
    const OFN_PATH_MUST_EXIST: u32 = 0x0000_0800;
    const OFN_EXPLORER: u32 = 0x0008_0000;
    const OFN_DONT_ADD_TO_RECENT: u32 = 0x0200_0000;

    let mut file_buffer = vec![0u16; 32_768];
    fill_save_buffer(&mut file_buffer, default_name);
    let title_wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let initial_wide: Vec<u16> = initial_dir
        .map(|path| {
            path.as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect()
        })
        .unwrap_or_default();
    let mut dialog = OpenFileNameW {
        l_struct_size: std::mem::size_of::<OpenFileNameW>() as u32,
        hwnd_owner: unsafe { GetActiveWindow() },
        h_instance: std::ptr::null_mut(),
        lpstr_filter: filter.as_ptr(),
        lpstr_custom_filter: std::ptr::null_mut(),
        n_max_cust_filter: 0,
        n_filter_index: 1,
        lpstr_file: file_buffer.as_mut_ptr(),
        n_max_file: file_buffer.len() as u32,
        lpstr_file_title: std::ptr::null_mut(),
        n_max_file_title: 0,
        lpstr_initial_dir: if initial_wide.is_empty() {
            std::ptr::null()
        } else {
            initial_wide.as_ptr()
        },
        lpstr_title: title_wide.as_ptr(),
        flags: OFN_EXPLORER
            | OFN_OVERWRITEPROMPT
            | OFN_PATH_MUST_EXIST
            | OFN_NO_CHANGE_DIR
            | OFN_DONT_ADD_TO_RECENT,
        n_file_offset: 0,
        n_file_extension: 0,
        lpstr_def_ext: std::ptr::null(),
        l_cust_data: 0,
        lpfn_hook: std::ptr::null_mut(),
        lp_template_name: std::ptr::null(),
        pv_reserved: std::ptr::null_mut(),
        dw_reserved: 0,
        flags_ex: 0,
    };
    if unsafe { GetSaveFileNameW(&mut dialog) } == 0 {
        // 0 without an extended error means the user cancelled, not a failure.
        let _ = unsafe { CommDlgExtendedError() };
        return None;
    }
    let length = file_buffer.iter().position(|unit| *unit == 0)?;
    if length == 0 {
        return None;
    }
    Some(PathBuf::from(std::ffi::OsString::from_wide(
        &file_buffer[..length],
    )))
}

#[cfg(not(windows))]
fn native_save(
    _filter: &[u16],
    _title: &str,
    _default_name: &str,
    _initial_dir: Option<&Path>,
) -> Option<PathBuf> {
    // The native save dialog is Windows-only; the UI keeps its text/export path.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![0x4d_u8; bytes]).unwrap();
    }

    #[test]
    fn filters_are_double_null_terminated_utf16_pairs() {
        let filter = build_filter(&[("可执行文件 (*.exe)", "*.exe")]);
        assert_eq!(filter.last(), Some(&0));
        assert_eq!(filter[filter.len() - 2], 0);
        let text = String::from_utf16_lossy(&filter);
        assert!(text.contains("可执行文件 (*.exe)"));
        assert!(text.contains("*.exe"));
        assert!(!tool_filter(ToolKind::Mpv).is_empty());
        assert!(!tool_filter(ToolKind::PotPlayer).is_empty());
        assert!(!tool_filter(ToolKind::YtDlp).is_empty());
        // The last two units form the required double-NUL terminator.
        assert_eq!(
            tool_filter(ToolKind::YtDlp)
                .iter()
                .rev()
                .take(2)
                .copied()
                .collect::<Vec<_>>(),
            vec![0, 0]
        );
    }

    #[test]
    fn path_validation_accepts_empty_or_real_files_and_rejects_broken_drafts() {
        assert!(validate_tool_path(ToolKind::Mpv, "").unwrap().is_none());
        assert!(validate_tool_path(ToolKind::Mpv, "   ").unwrap().is_none());
        let error = validate_tool_path(ToolKind::YtDlp, "tools/yt-dlp.exe").unwrap_err();
        assert!(error.to_string().contains("绝对路径"), "{error}");
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing.exe");
        let error = validate_tool_path(ToolKind::YtDlp, missing.to_str().unwrap()).unwrap_err();
        assert!(error.to_string().contains("不存在"), "{error}");
        let directory_path = directory.path().join("a-folder");
        std::fs::create_dir_all(&directory_path).unwrap();
        let error =
            validate_tool_path(ToolKind::Mpv, directory_path.to_str().unwrap()).unwrap_err();
        assert!(error.to_string().contains("不是普通文件"), "{error}");
        let tiny = directory.path().join("tiny.exe");
        write_file(&tiny, 100);
        let error = validate_tool_path(ToolKind::PotPlayer, tiny.to_str().unwrap()).unwrap_err();
        assert!(error.to_string().contains("过小"), "{error}");
        let valid = directory.path().join("yt-dlp.exe");
        write_file(&valid, 2048);
        assert_eq!(
            validate_tool_path(ToolKind::YtDlp, valid.to_str().unwrap())
                .unwrap()
                .unwrap(),
            valid
        );
        let (mpv, pot, ytdlp) = validate_tool_paths("", valid.to_str().unwrap(), "").unwrap();
        assert!(mpv.is_none() && ytdlp.is_none());
        assert_eq!(pot.unwrap(), valid);
        assert!(
            validate_tool_paths("relative.exe", "", "").is_err(),
            "one broken path rejects the whole save"
        );
    }

    #[test]
    fn candidate_order_prefers_the_program_directory_then_path_then_installs() {
        let tool = ToolKind::Mpv;
        let candidates = tool_candidates(
            tool,
            Path::new("C:/app"),
            &[PathBuf::from("C:/tools")],
            &[PathBuf::from("C:/PF"), PathBuf::from("C:/PF86")],
            &[PathBuf::from("C:/extra")],
        );
        assert_eq!(candidates[0], PathBuf::from("C:/app"));
        assert_eq!(candidates[1], PathBuf::from("C:/app/bin"));
        assert_eq!(candidates[2], PathBuf::from("C:/app/bin/mpv"));
        assert!(candidates.contains(&PathBuf::from("C:/tools")));
        assert!(candidates.contains(&PathBuf::from("C:/PF/mpv")));
        assert!(candidates.contains(&PathBuf::from("C:/PF86/mpv")));
        assert_eq!(candidates.last(), Some(&PathBuf::from("C:/extra")));
        let potplayer = tool_candidates(
            ToolKind::PotPlayer,
            Path::new("C:/app"),
            &[],
            &[PathBuf::from("C:/PF")],
            &[],
        );
        assert!(potplayer.contains(&PathBuf::from("C:/PF/DAUM/PotPlayer")));
        let yt = tool_candidates(
            ToolKind::YtDlp,
            Path::new("C:/app"),
            &[],
            &[PathBuf::from("C:/PF")],
            &[],
        );
        assert!(yt.contains(&PathBuf::from("C:/PF/yt-dlp")));
        // No hard-coded reference-project path is ever produced.
        assert!(
            candidates
                .iter()
                .all(|path| !path.to_string_lossy().contains("live-hub"))
        );
    }

    #[test]
    fn detection_returns_the_first_existing_candidate_only() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("bin");
        let second = directory.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let found = first.join("mpv.exe");
        write_file(&found, 2048);
        write_file(&second.join("mpv.exe"), 2048);
        let directories = vec![first, second];
        let detected = detect_tool_in(ToolKind::Mpv, &directories).unwrap();
        assert_eq!(detected, found);
        assert!(detect_tool_in(ToolKind::YtDlp, &directories).is_none());
        assert!(detect_tool_in(ToolKind::PotPlayer, &directories).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn win32_open_file_name_layout_matches_the_documented_size() {
        assert_eq!(std::mem::size_of::<[usize; 19]>(), 152);
        assert_eq!(open_file_name_size(), 152);
    }

    #[test]
    fn status_labels_are_quiet_for_empty_and_specific_for_broken_paths() {
        assert!(path_status(ToolKind::Mpv, "").is_none());
        assert_eq!(path_status(ToolKind::Mpv, "relative.exe").unwrap().0, false);
        let directory = tempfile::tempdir().unwrap();
        let valid = directory.path().join("mpv.exe");
        write_file(&valid, 2048);
        let (ok, text) = path_status(ToolKind::Mpv, valid.to_str().unwrap()).unwrap();
        assert!(ok);
        assert_eq!(text, "路径有效");
    }

    #[test]
    fn save_dialog_buffer_keeps_the_default_name_and_nul_termination() {
        let mut buffer = vec![0xFFFFu16; 64];
        fill_save_buffer(&mut buffer, "live-hub-favorites-20261031-1010.json");
        let end = buffer.iter().position(|unit| *unit == 0).unwrap();
        assert_eq!(
            String::from_utf16_lossy(&buffer[..end]),
            "live-hub-favorites-20261031-1010.json"
        );
        assert!(
            buffer[end..].iter().all(|unit| *unit == 0),
            "the rest of the buffer must be zeroed"
        );

        // An over-long default name is truncated but the buffer is still valid
        // UTF-16 with a NUL terminator (comdlg32 must never read past it).
        let mut small = vec![0u16; 8];
        fill_save_buffer(&mut small, "非常长的默认文件名0123456789.json");
        assert_eq!(small[7], 0);
        assert_eq!(String::from_utf16_lossy(&small[..7]).chars().count(), 7);
    }

    #[test]
    fn save_dialog_filter_uses_the_shared_utf16_filter_builder() {
        let filter = build_filter(&[("收藏 JSON (*.json)", "*.json")]);
        assert_eq!(filter.last(), Some(&0));
        let text = String::from_utf16_lossy(&filter);
        assert!(text.contains("收藏 JSON (*.json)"));
        assert!(text.contains("*.json"));
    }

    #[cfg(windows)]
    #[test]
    fn win32_save_dialog_reuses_the_open_file_name_layout() {
        // Both dialogs share OpenFileNameW; a wrong size makes comdlg32 refuse
        // to open the dialog, so the save path must keep the same 152-byte
        // layout (19 pointer-sized fields on 64-bit).
        assert_eq!(std::mem::size_of::<[usize; 19]>(), 152);
        assert_eq!(open_file_name_size(), 152);
    }

    #[test]
    fn open_file_name_uses_the_shared_filter_builder() {
        // The import UI builds this exact filter; a malformed filter string is
        // what makes comdlg32 refuse the dialog on a real desktop.
        let filter = build_filter(&[("收藏 JSON (*.json)", "*.json")]);
        assert_eq!(filter.last(), Some(&0));
        let text = String::from_utf16_lossy(&filter);
        assert!(text.contains("收藏 JSON (*.json)") && text.contains("*.json"));
    }

    #[cfg(not(windows))]
    #[test]
    fn open_file_name_is_a_no_op_off_windows() {
        let filter = build_filter(&[("收藏 JSON (*.json)", "*.json")]);
        assert!(
            open_file_name(&filter, "导入收藏", None).is_none(),
            "non-Windows builds must keep the text/import path working"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn save_dialog_is_a_no_op_off_windows() {
        let filter = build_filter(&[("JSON", "*.json")]);
        assert!(
            save_file_name(&filter, "导出收藏", "favorites.json", None).is_none(),
            "non-Windows builds must keep the text/export path working"
        );
    }
}
