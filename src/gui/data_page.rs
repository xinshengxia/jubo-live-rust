//! Local-data and platform-status pages: a read-only view over the isolated
//! data directory plus the launcher's own local observations.
//!
//! Nothing here contacts a platform, starts a player or reads a credential file.
//! Only the files this application owns are listed, and protected file names are
//! never shown or previewed.

use super::observations::{OBSERVATION_STALE_SECS, ObservationLog};
use super::*;
use chrono::TimeZone;
use std::fs::{self, File};

/// The local-data page has two tabs, matching the original workbench's
/// "本地数据 / 平台运行状态" split.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DataTab {
    #[default]
    Local,
    Platform,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DataCategory {
    #[default]
    Favorites,
    History,
    Config,
    Cache,
    Logs,
}

impl DataCategory {
    pub const ALL: [Self; 5] = [
        Self::Favorites,
        Self::History,
        Self::Config,
        Self::Cache,
        Self::Logs,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Favorites => "收藏",
            Self::History => "播放历史",
            Self::Config => "配置",
            Self::Cache => "缓存",
            Self::Logs => "日志",
        }
    }

    fn unit(self, count: usize) -> String {
        match self {
            Self::Favorites | Self::History => format!("{count} 条记录"),
            Self::Config => format!("{count} 项"),
            Self::Cache | Self::Logs => format!("{count} 个文件"),
        }
    }
}

/// One listed file under the launcher's own runtime directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataFile {
    pub path: PathBuf,
    pub name: String,
    pub bytes: u64,
    pub modified: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataCategoryView {
    pub category: DataCategory,
    pub count: usize,
    pub unit: String,
    pub location: String,
    pub bytes: u64,
    pub modified: Option<u64>,
    /// Read-only detail rows; never includes credentials or raw config.
    pub rows: Vec<Vec<String>>,
    pub files: Vec<DataFile>,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataSnapshot {
    pub categories: Vec<DataCategoryView>,
    pub warning: Option<String>,
}

const MAX_DATA_FILES: usize = 200;
const MAX_LOG_PREVIEW_BYTES: u64 = 64 * 1024;
/// Diagnostics are bounded so a long run cannot grow one file without limit.
pub const MAX_DIAGNOSTIC_BYTES: u64 = 256 * 1024;
const MAX_LINE_CHARS: usize = 512;

/// File or directory names that must never appear in the data list or preview.
fn protected_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        "cookie",
        "credential",
        "token",
        "password",
        "profile",
        "sessionid",
        "sessdata",
        "dy_auth",
        "udb_",
        "api-key",
        "api_key",
        "apikey",
        "secret",
        "health-login",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

pub fn size_text(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.2} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.2} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

pub fn modified_text(modified: Option<u64>) -> String {
    let Some(at) = modified else {
        return "尚无文件".to_owned();
    };
    chrono::Local
        .timestamp_opt(at as i64, 0)
        .single()
        .map(|time| time.format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "时间未知".to_owned())
}

fn modified_secs(metadata: &std::fs::Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_secs())
}

fn file_metadata(path: &Path) -> Option<(u64, Option<u64>)> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    Some((metadata.len(), modified_secs(&metadata)))
}

fn relative_location(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.display().to_string())
}

/// Paths whose rows are projected. Only the state file and the launcher's own
/// runtime directory are inspected; no recursive scan happens anywhere.
pub fn state_path(root: &Path) -> PathBuf {
    root.join("state.json")
}

pub fn runtime_dir(root: &Path) -> PathBuf {
    root.join("runtime")
}

/// Builds the read-only data snapshot shown on the local-data tab.
pub fn build_snapshot(
    root: &Path,
    favorites: &[Favorite],
    history: &[HistoryEntry],
    settings: &Settings,
) -> DataSnapshot {
    let state_path = state_path(root);
    let state_metadata = file_metadata(&state_path);
    let state_location = relative_location(root, &state_path);
    let mut warning: Option<String> = None;

    let favorite_rows = favorites
        .iter()
        .map(|entry| {
            vec![
                entry.name.clone(),
                entry.target.platform.label().to_owned(),
                entry.target.room.clone(),
            ]
        })
        .collect();
    let history_rows = history
        .iter()
        .map(|entry| {
            vec![
                super::history_time(entry),
                entry.name.clone(),
                entry.target.platform.label().to_owned(),
                entry.target.room.clone(),
            ]
        })
        .collect();
    let config_rows = config_rows(settings);
    let (cache_files, log_files, scan_warning) = scan_runtime(root);
    if scan_warning.is_some() {
        warning = scan_warning;
    }

    let state_view = |category: DataCategory, count: usize, rows: Vec<Vec<String>>, note: &str| {
        DataCategoryView {
            category,
            count,
            unit: category.unit(count),
            location: state_location.clone(),
            bytes: state_metadata.map(|(bytes, _)| bytes).unwrap_or(0),
            modified: state_metadata.and_then(|(_, modified)| modified),
            rows,
            files: Vec::new(),
            note: note.to_owned(),
        }
    };
    let files_view = |category: DataCategory, files: Vec<DataFile>, note: &str| {
        let rows = files
            .iter()
            .map(|file| {
                vec![
                    file.name.clone(),
                    size_text(file.bytes),
                    modified_text(file.modified),
                ]
            })
            .collect();
        DataCategoryView {
            category,
            count: files.len(),
            unit: category.unit(files.len()),
            location: relative_location(root, &runtime_dir(root)),
            bytes: files.iter().map(|file| file.bytes).sum(),
            modified: files.iter().filter_map(|file| file.modified).max(),
            rows,
            files,
            note: note.to_owned(),
        }
    };

    DataSnapshot {
        categories: vec![
            state_view(
                DataCategory::Favorites,
                favorites.len(),
                favorite_rows,
                "只读查看；收藏保存在 state.json，本页不修改文件。",
            ),
            state_view(
                DataCategory::History,
                history.len(),
                history_rows,
                &format!(
                    "只读查看；按最近使用排序，最多保留 {} 个房间。",
                    crate::core::HISTORY_LIMIT
                ),
            ),
            state_view(
                DataCategory::Config,
                config_rows.len(),
                config_rows,
                "仅投影公开的显示与播放器选项；不展示 Cookie、AI 密钥或原始配置。",
            ),
            files_view(
                DataCategory::Cache,
                cache_files,
                "仅列本程序运行目录中的缓存/状态文件；不遍历 profile、凭据或子目录。",
            ),
            files_view(
                DataCategory::Logs,
                log_files,
                "只读查看本程序诊断日志；预览会过滤凭据字段与 URL，最多显示末尾 64 KiB。",
            ),
        ],
        warning,
    }
}

fn scan_runtime(root: &Path) -> (Vec<DataFile>, Vec<DataFile>, Option<String>) {
    let directory = runtime_dir(root);
    let mut cache = Vec::new();
    let mut logs = Vec::new();
    let mut warning = None;
    let Ok(entries) = fs::read_dir(&directory) else {
        return (cache, logs, warning);
    };
    let mut visited = 0;
    for entry in entries.flatten() {
        if visited >= MAX_DATA_FILES {
            warning = Some("运行文件较多，仅显示前 200 个；未修改任何文件。".to_owned());
            break;
        }
        visited += 1;
        let name = entry.file_name().to_string_lossy().to_string();
        if protected_name(&name) {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        let file = DataFile {
            name: name.clone(),
            bytes: metadata.len(),
            modified: modified_secs(&metadata),
            path,
        };
        if name.ends_with(".log") {
            logs.push(file);
        } else {
            cache.push(file);
        }
    }
    cache.sort_by(|left, right| left.name.cmp(&right.name));
    logs.sort_by(|left, right| left.name.cmp(&right.name));
    (cache, logs, warning)
}

/// Public, credential-free projection of persisted settings.
pub fn config_rows(settings: &Settings) -> Vec<Vec<String>> {
    let path_or = |path: &Option<PathBuf>| {
        path.as_ref()
            .map(|value| value.display().to_string())
            .unwrap_or_else(|| "未设置".to_owned())
    };
    let on_off = |value: bool| if value { "开启" } else { "关闭" };
    let mut rows = vec![
        vec!["mpv 程序路径".into(), path_or(&settings.mpv)],
        vec!["PotPlayer 程序路径".into(), path_or(&settings.potplayer)],
        vec!["yt-dlp 程序路径".into(), path_or(&settings.yt_dlp)],
        vec![
            "缓冲模式".into(),
            match settings.playback.buffer_mode {
                BufferMode::LowLatency => "延迟更低".into(),
                BufferMode::Stable => "播放更稳定".into(),
            },
        ],
        vec![
            "画质增强".into(),
            match settings.playback.enhancement {
                Enhancement::Off => "关闭".into(),
                Enhancement::Light => "轻度".into(),
                Enhancement::High => "高质量".into(),
            },
        ],
        vec![
            "未开播时等待".into(),
            on_off(settings.playback.wait_for_live).into(),
        ],
        vec![
            "显示开播/下播/重播状态".into(),
            on_off(settings.playback.monitor_room_state).into(),
        ],
        vec![
            "B站 FLV 中转恢复".into(),
            on_off(settings.playback.relay_bili).into(),
        ],
        vec![
            "mpv 高级参数".into(),
            format!("{} 项", settings.playback.mpv_extra_args.len()),
        ],
        vec![
            "PotPlayer 高级参数".into(),
            format!("{} 项", settings.playback.potplayer_extra_args.len()),
        ],
        vec!["弹幕覆盖层".into(), on_off(settings.overlay.enabled).into()],
        vec![
            "显示 SC 卡片".into(),
            on_off(settings.overlay.superchat_enabled).into(),
        ],
        vec![
            "弹幕字号".into(),
            format!("{} 像素", settings.overlay.font_size),
        ],
        vec![
            "滚动速度".into(),
            format!("{} 像素/秒", settings.overlay.speed_px_per_sec),
        ],
        vec![
            "不透明度".into(),
            format!("{}%", settings.overlay.opacity_percent),
        ],
        vec![
            "显示区域".into(),
            format!("{}%", settings.overlay.display_area_percent),
        ],
        vec![
            "同屏弹幕上限".into(),
            format!("{} 条", settings.overlay.max_visible),
        ],
        vec![
            "延迟补偿".into(),
            format!("{} 毫秒", settings.overlay.behavior.delay_ms),
        ],
        vec![
            "底部字幕避让".into(),
            format!("{}%", settings.overlay.behavior.bottom_safe_percent),
        ],
        vec![
            "合并重复弹幕".into(),
            on_off(settings.overlay.behavior.merge_duplicates).into(),
        ],
        vec![
            "显示图片表情".into(),
            on_off(settings.overlay.behavior.show_images).into(),
        ],
        vec![
            "显示礼物与守护".into(),
            on_off(settings.overlay.behavior.show_gifts).into(),
        ],
        vec![
            "显示入场与关注".into(),
            on_off(settings.overlay.behavior.show_enters).into(),
        ],
        vec![
            "F7–F10 快捷键".into(),
            on_off(settings.overlay.behavior.hotkeys_enabled).into(),
        ],
        vec![
            "屏蔽关键词".into(),
            format!("{} 项", settings.overlay.behavior.blocked_keywords.len()),
        ],
        vec![
            "屏蔽用户名".into(),
            format!("{} 项", settings.overlay.behavior.blocked_users.len()),
        ],
        vec![
            "文字不透明度".into(),
            format!("{}%", settings.overlay.text_opacity_percent),
        ],
        vec![
            "描边".into(),
            if settings.overlay.stroke_enabled {
                format!(
                    "开启（{} 像素，{}）",
                    settings.overlay.stroke_width,
                    match settings.overlay.border_style {
                        TextBorderStyle::Outline => "描边",
                        TextBorderStyle::Heavy => "加粗描边",
                        TextBorderStyle::Shadow => "阴影",
                    }
                )
            } else {
                "关闭".to_owned()
            },
        ],
        vec![
            "字号随画面缩放".into(),
            if settings.overlay.scale_with_window {
                format!(
                    "开启（基准 {} 像素）",
                    settings.overlay.scale_reference_height
                )
            } else {
                "关闭".to_owned()
            },
        ],
        vec![
            "行距".into(),
            format!("{}%", settings.overlay.line_gap_percent),
        ],
        vec![
            "画面安全区".into(),
            format!("{} 个", settings.overlay.reserved_areas.len()),
        ],
        vec![
            "用户信息前缀".into(),
            [
                (settings.overlay.show_user_name, "用户名"),
                (settings.overlay.show_user_level, "等级"),
                (settings.overlay.show_medal, "勋章"),
                (settings.overlay.show_guard, "舰长/守护"),
            ]
            .into_iter()
            .filter_map(|(enabled, label)| enabled.then_some(label))
            .collect::<Vec<_>>()
            .join(" / "),
        ],
        vec![
            "图片表情尺寸".into(),
            match settings.overlay.emote_size {
                EmoteSizeMode::Small => "不高于文字".into(),
                EmoteSizeMode::Platform => "按平台尺寸".into(),
                EmoteSizeMode::Large => "大表情占多行".into(),
            },
        ],
        vec![
            "彩色 Unicode 表情".into(),
            on_off(settings.overlay.show_color_emoji).into(),
        ],
        vec![
            "SC 显示方式".into(),
            match settings.overlay.sc_mode {
                ScMode::Full => "完整卡片".into(),
                ScMode::Compact => "紧凑一行".into(),
                ScMode::Badge => "计数角标".into(),
                ScMode::Off => "不显示".into(),
            },
        ],
        vec![
            "SC 缩放 / 停留倍率".into(),
            format!(
                "{}% / {}%",
                settings.overlay.sc_scale_percent, settings.overlay.sc_duration_scale_percent
            ),
        ],
        vec![
            "SC 角落 / 头像".into(),
            format!(
                "{} / {}",
                match settings.overlay.sc_corner {
                    ScCorner::TopRight => "右上",
                    ScCorner::BottomRight => "右下",
                    ScCorner::TopLeft => "左上",
                    ScCorner::BottomLeft => "左下",
                },
                on_off(settings.overlay.sc_show_avatar)
            ),
        ],
        vec![
            "弹幕类型".into(),
            [
                (settings.overlay.behavior.show_scrolling, "滚动"),
                (settings.overlay.behavior.show_top, "顶部"),
                (settings.overlay.behavior.show_bottom, "底部"),
                (settings.overlay.behavior.show_colored, "彩色"),
            ]
            .into_iter()
            .filter_map(|(enabled, label)| enabled.then_some(label))
            .collect::<Vec<_>>()
            .join(" / "),
        ],
        vec![
            "等级过滤（用户/勋章）".into(),
            format!(
                "{} / {}",
                settings.overlay.behavior.min_user_level, settings.overlay.behavior.min_medal_level
            ),
        ],
        vec![
            "富事件停留 / 同屏".into(),
            format!(
                "{} 秒 / {} 条",
                settings.overlay.behavior.event_duration_secs,
                settings.overlay.behavior.event_max_visible
            ),
        ],
        vec![
            "逆向弹幕".into(),
            on_off(settings.overlay.behavior.show_reverse).into(),
        ],
        vec![
            "固定弹幕区域（顶/底）".into(),
            format!(
                "{}% / {}%",
                settings.overlay.behavior.top_fixed_zone_percent,
                settings.overlay.behavior.bottom_fixed_zone_percent
            ),
        ],
        vec![
            "固定弹幕同屏上限".into(),
            match settings.overlay.behavior.max_fixed_per_mode {
                0 => "不限".to_owned(),
                value => format!("{value} 条"),
            },
        ],
        vec![
            "过长固定弹幕转滚动".into(),
            on_off(settings.overlay.behavior.fixed_overflow_to_scroll).into(),
        ],
        vec![
            "平滑准入 / 每秒上限".into(),
            format!(
                "{} / {}",
                on_off(settings.overlay.behavior.smooth_admission),
                if settings.overlay.behavior.admission_per_second == 0 {
                    "自动".to_owned()
                } else {
                    format!("{}/秒", settings.overlay.behavior.admission_per_second)
                }
            ),
        ],
        vec![
            "短队列等待 / 容量".into(),
            format!(
                "{} 毫秒 / {} 条",
                settings.overlay.behavior.queue_wait_ms, settings.overlay.behavior.queue_capacity
            ),
        ],
        vec![
            "拥挤缩小".into(),
            if settings.overlay.behavior.shrink_when_crowded {
                format!(
                    "开启（{}%）",
                    settings.overlay.behavior.crowded_scale_percent
                )
            } else {
                "关闭".to_owned()
            },
        ],
    ];
    rows.truncate(64);
    rows
}

/// Credential-free line filter for the diagnostic log and its preview.
pub fn sanitize_line(line: &str) -> String {
    let cleaned: String = line
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let lower = cleaned.to_ascii_lowercase();
    let protected = [
        "cookie",
        "authorization",
        "bearer ",
        "api-key",
        "api_key",
        "api key",
        "password",
        "private key",
        "private_key",
        "sessionid",
        "sessdata",
        "dy_auth",
        "udb_",
        "secret",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    if protected {
        return "[已隐藏包含凭据字段的日志行]".to_owned();
    }
    let mut output = String::new();
    let mut rest = cleaned.as_str();
    while let Some(index) = rest.find("http://").or_else(|| rest.find("https://")) {
        output.push_str(&rest[..index]);
        let after = &rest[index..];
        let end = after
            .char_indices()
            .find(|(_, character)| {
                character.is_whitespace()
                    || matches!(character, '"' | '\'' | '<' | '>' | ')' | '（' | '）')
            })
            .map(|(offset, _)| offset)
            .unwrap_or(after.len());
        output.push_str("[URL]");
        rest = &after[end..];
    }
    output.push_str(rest);
    let mut chars = output.chars();
    let bounded: String = chars.by_ref().take(MAX_LINE_CHARS).collect();
    if chars.next().is_some() {
        format!("{bounded}…")
    } else {
        bounded
    }
}

/// Best-effort, bounded local diagnostics. Never fails a UI action; every I/O
/// error is ignored on purpose because this file exists only to help the user.
#[derive(Debug, Clone)]
pub struct DiagnosticLog {
    root: PathBuf,
}

impl DiagnosticLog {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
        }
    }

    pub fn path(&self) -> PathBuf {
        runtime_dir(&self.root).join("diagnostics.log")
    }

    pub fn record(&self, message: &str) {
        let line = sanitize_line(message);
        if line.is_empty() {
            return;
        }
        let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let path = self.path();
        let Some(directory) = path.parent().map(Path::to_owned) else {
            return;
        };
        if fs::create_dir_all(&directory).is_err() {
            return;
        }
        let entry = format!("[{timestamp}] {line}\n");
        let current = fs::metadata(&path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if current + entry.len() as u64 > MAX_DIAGNOSTIC_BYTES {
            // Keep the newest tail instead of failing: the log is diagnostics only.
            let text = read_tail_text(&path, MAX_DIAGNOSTIC_BYTES / 2);
            let combined = format!("{text}{entry}");
            let _ = fs::write(&path, combined);
        } else {
            use std::io::Write;
            if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
                let _ = file.write_all(entry.as_bytes());
            }
        }
    }

    pub fn preview(&self) -> String {
        let path = self.path();
        if !path.is_file() {
            return "尚无诊断日志。应用在遇到错误时才会写入本地日志。".to_owned();
        }
        let text = read_tail_text(&path, MAX_LOG_PREVIEW_BYTES);
        if text.is_empty() {
            return "诊断日志为空。".to_owned();
        }
        let mut output = String::new();
        if fs::metadata(&path)
            .map(|metadata| metadata.len())
            .unwrap_or(0)
            > MAX_LOG_PREVIEW_BYTES
        {
            output.push_str("（仅显示日志末尾）\n");
        }
        for line in text.lines() {
            output.push_str(&sanitize_line(line));
            output.push('\n');
        }
        output
    }

    pub fn clear(&self) -> Result<()> {
        let path = self.path();
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("无法清除诊断日志"),
        }
    }
}

fn read_tail_text(path: &Path, max_bytes: u64) -> String {
    let Ok(metadata) = fs::metadata(path) else {
        return String::new();
    };
    let length = metadata.len();
    let start = length.saturating_sub(max_bytes);
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    if start > 0 && std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
    if file.take(max_bytes + 1).read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&bytes).to_string();
    if start > 0 {
        text.split_once('\n')
            .map(|(_, rest)| rest.to_owned())
            .unwrap_or_default()
    } else {
        text
    }
}

/// Names this application itself writes under runtime/. Cleanup never walks
/// directories and never touches state.json, credentials or browser profiles.
fn disposable_runtime_names() -> [&'static str; 4] {
    [
        "platform-observations.json",
        "platform-observations.json.tmp",
        "diagnostics.log",
        "diagnostics.log.tmp",
    ]
}

/// Deletes only the launcher's own cache/log files. Returns freed bytes.
pub fn cleanup_runtime(root: &Path) -> Result<u64> {
    let directory = runtime_dir(root);
    if !directory.is_dir() {
        return Ok(0);
    }
    let mut freed = 0_u64;
    for name in disposable_runtime_names() {
        let path = directory.join(name);
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        if fs::remove_file(&path).is_ok() {
            freed += metadata.len();
        }
    }
    Ok(freed)
}

/// Build version shown next to the build time. The package version is the only
/// build stamp without a build script, so the time falls back to the executable's
/// own modification time and is labeled as such in the UI.
pub fn build_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn build_time_text(modified: Option<std::time::SystemTime>) -> String {
    if let Some(embedded) = option_env!("LIVE_HUB_BUILD_TIME") {
        let trimmed = embedded.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    let Some(modified) = modified else {
        return "未知时间".to_owned();
    };
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| {
            chrono::DateTime::<chrono::Utc>::from_timestamp(elapsed.as_secs() as i64, 0)
        })
        .map(|time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "未知时间".to_owned())
}

pub fn executable_modified() -> Option<std::time::SystemTime> {
    std::env::current_exe()
        .ok()
        .and_then(|path| fs::metadata(path).ok())
        .and_then(|metadata| metadata.modified().ok())
}

impl Launcher {
    pub(super) fn refresh_data_snapshot(&mut self) {
        self.data_snapshot = Some(build_snapshot(
            &self.snapshot.root,
            &self.snapshot.favorites,
            &self.snapshot.history,
            &self.snapshot.settings,
        ));
        self.data_log_index = None;
    }

    pub(super) fn record_observation(
        &mut self,
        platform: Platform,
        stage: super::observations::ObservationStage,
        state: super::observations::ObservationState,
        at: u64,
        room: &str,
    ) {
        if !self.observations.record(platform, stage, state, at, room) {
            return;
        }
        if let Err(error) = self.observations.save(&self.snapshot.root) {
            self.diagnostics
                .record(&format!("运行观测记录保存失败：{error:#}"));
        }
    }

    pub(super) fn cleanup_cache(&mut self) {
        let result = cleanup_runtime(&self.snapshot.root)
            .and_then(|freed| self.diagnostics.clear().map(|()| freed));
        match result {
            Ok(freed) => {
                self.refresh_data_snapshot();
                self.set_status(
                    format!(
                        "已清理本程序的缓存与日志（{}）。收藏、历史、配置和登录资料保持不变。",
                        size_text(freed)
                    ),
                    false,
                );
            }
            Err(error) => self.set_status(format!("清理未完成：{error:#}"), true),
        }
    }

    fn local_data_tab(&mut self, ui: &mut egui::Ui) {
        if self.data_snapshot.is_none() {
            self.refresh_data_snapshot();
        }
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(self.storage_pending == 0, egui::Button::new("刷新列表"))
                .clicked()
            {
                self.refresh_data_snapshot();
                self.set_status("已重新读取本地数据清单；未修改任何文件。", false);
            }
            if ui.button("查看本地数据").clicked() {
                self.data_detail = true;
            }
        });
        ui.add_space(10.0);
        let snapshot = self.data_snapshot.clone().unwrap_or(DataSnapshot {
            categories: Vec::new(),
            warning: Some("本地数据清单尚未读取。".to_owned()),
        });
        for category in &snapshot.categories {
            card(ui, self.dark, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(format!("{}  {}", category.category.title(), category.unit))
                            .size(16.0)
                            .strong(),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("查看明细").clicked() {
                            self.data_category = category.category;
                            self.data_detail = true;
                        }
                    });
                });
                ui.label(
                    RichText::new(format!(
                        "位置 {} · 占用 {} · 最近更新 {}",
                        category.location,
                        size_text(category.bytes),
                        modified_text(category.modified)
                    ))
                    .small()
                    .weak(),
                );
            });
            ui.add_space(8.0);
        }
        if let Some(warning) = &snapshot.warning {
            ui.label(RichText::new(warning).small().color(ACCENT));
        }
        ui.add_space(12.0);
        card(ui, self.dark, |ui| {
            ui.label(RichText::new("缓存与日志维护").size(16.0).strong());
            ui.label("清理运行缓存和诊断日志，保留收藏、历史、配置、登录凭据与登录 profile。");
            ui.add_space(8.0);
            if self.cleanup_confirm {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("确认清理本程序的缓存与日志？").color(ACCENT));
                    if ui.button("确认清理").clicked() {
                        self.cleanup_confirm = false;
                        self.cleanup_cache();
                    }
                    if ui.button("取消").clicked() {
                        self.cleanup_confirm = false;
                    }
                });
            } else if ui.button("清理缓存和日志").clicked() {
                self.cleanup_confirm = true;
            }
        });
        ui.add_space(12.0);
        card(ui, self.dark, |ui| {
            ui.label(RichText::new("构建信息").size(16.0).strong());
            ui.label(format!("Build 版本：{}", build_version()));
            ui.label(format!(
                "构建来源时间（UTC）：{}",
                build_time_text(executable_modified())
            ));
            ui.label(
                RichText::new("本版未嵌入构建戳时显示程序文件时间；两者都不代表平台或登录验收。")
                    .small()
                    .weak(),
            );
        });
        ui.add_space(12.0);
        ui.horizontal_wrapped(|ui| {
            if ui.button("在资源管理器中打开数据目录").clicked() {
                let path = self.snapshot.root.clone();
                match open_data_folder(&path) {
                    Ok(()) => self.set_status("已在资源管理器中打开数据根目录。", false),
                    Err(error) => self.set_status(format!("无法打开数据目录：{error:#}"), true),
                }
            }
            ui.label(
                RichText::new("清单为只读；仅“清理缓存和日志”会删除本程序运行文件。")
                    .small()
                    .weak(),
            );
        });
        if self.data_detail {
            self.data_detail_card(ui);
        }
    }

    fn data_detail_card(&mut self, ui: &mut egui::Ui) {
        let Some(snapshot) = self.data_snapshot.clone() else {
            return;
        };
        let Some(category) = snapshot
            .categories
            .iter()
            .find(|category| category.category == self.data_category)
        else {
            return;
        };
        ui.add_space(14.0);
        card(ui, self.dark, |ui| {
            ui.horizontal_wrapped(|ui| {
                for value in DataCategory::ALL {
                    if ui
                        .selectable_label(self.data_category == value, value.title())
                        .clicked()
                    {
                        self.data_category = value;
                        self.data_log_index = None;
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("关闭明细").clicked() {
                        self.data_detail = false;
                    }
                });
            });
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!(
                    "{} · {} · 位置 {}",
                    category.category.title(),
                    category.unit,
                    category.location
                ))
                .small()
                .weak(),
            );
            let mut selected_log = None;
            egui::ScrollArea::vertical()
                .id_salt("local_data_detail")
                .max_height(260.0)
                .show(ui, |ui| {
                    for (index, row) in category.rows.iter().enumerate() {
                        let selected = category.category == DataCategory::Logs
                            && self.data_log_index == Some(index);
                        let text = row.join(" · ");
                        if category.category == DataCategory::Logs {
                            if ui.selectable_label(selected, text).clicked() {
                                selected_log = Some(index);
                            }
                        } else {
                            ui.label(RichText::new(text).small());
                        }
                    }
                    if category.rows.is_empty() {
                        ui.label(RichText::new("尚无可显示的记录。").small().weak());
                    }
                });
            if let Some(index) = selected_log {
                self.data_log_index = Some(index);
            }
            if category.category == DataCategory::Logs {
                ui.add_space(8.0);
                let preview = match self
                    .data_log_index
                    .and_then(|index| category.files.get(index))
                {
                    Some(file) => read_log_preview(&file.path),
                    None => "选择日志文件查看末尾内容。".to_owned(),
                };
                egui::ScrollArea::vertical()
                    .id_salt("local_data_log_preview")
                    .max_height(180.0)
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(preview).monospace().small()).wrap());
                    });
            }
            ui.label(RichText::new(&category.note).small().weak());
        });
    }

    fn platform_status_tab(&mut self, ui: &mut egui::Ui) {
        let now = health_now();
        self.platform_picker(ui);
        ui.add_space(12.0);
        card(ui, self.dark, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!("{} · 本地最近观测", self.platform.label()))
                        .size(16.0)
                        .strong(),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("刷新运行状态").clicked() {
                        self.observations = ObservationLog::load(&self.snapshot.root);
                        self.set_status("已重新读取运行观测记录；不发起平台请求。", false);
                    }
                });
            });
            ui.add_space(8.0);
            let rows = self.observations.rows(self.platform, now);
            for row in rows {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(row.stage.label()).strong().size(13.0));
                    let color = if row.stale {
                        ACCENT
                    } else {
                        ui.visuals().text_color()
                    };
                    ui.label(RichText::new(row.result).color(color).size(13.0));
                    ui.label(
                        RichText::new(format!("更新 {} · 房间 {}", row.time, row.room))
                            .small()
                            .weak(),
                    );
                });
            }
            ui.label(
                RichText::new(format!(
                    "超过 {} 秒的记录显示为待重新观测；这只表示观测陈旧，不表示登录凭据过期。",
                    OBSERVATION_STALE_SECS
                ))
                .small()
                .weak(),
            );
        });
        ui.add_space(12.0);
        card(ui, self.dark, |ui| {
            ui.label(RichText::new("登录状态").size(16.0).strong());
            if let Some(health) = self.login_health.get(&self.platform) {
                for line in health.detail_text(now).lines() {
                    ui.label(RichText::new(line).small());
                }
            } else if self.snapshot.credentials.imported_for(self.platform) {
                ui.label("凭据已保存，尚未向平台确认登录状态。");
            } else {
                ui.label("此平台尚未保存登录凭据。");
            }
            ui.label(
                RichText::new("打开“账号与登录”页可检测登录状态；本页不发起平台请求。")
                    .small()
                    .weak(),
            );
        });
        ui.add_space(12.0);
        card(ui, self.dark, |ui| {
            ui.label(RichText::new("维护").size(16.0).strong());
            ui.add_space(6.0);
            if self.logout_confirm == Some(self.platform) {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "确认退出 {} 并清除该平台凭据？播放/弹幕会话请先手动关闭。",
                            self.platform.label()
                        ))
                        .color(ACCENT),
                    );
                    if ui.button("确认退出登录").clicked() {
                        let platform = self.platform;
                        self.logout_confirm = None;
                        self.invalidate_login_health(platform);
                        self.send_store(StoreAction::CredentialRemove(platform));
                        self.diagnostics
                            .record(&format!("已请求清除 {} 的登录凭据。", platform.label()));
                        self.set_status(
                            format!(
                                "已请求清除 {} 的凭据；现有播放会话请手动关闭。",
                                platform.label()
                            ),
                            false,
                        );
                    }
                    if ui.button("取消").clicked() {
                        self.logout_confirm = None;
                    }
                });
            } else {
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(
                            self.storage_pending == 0
                                && self.snapshot.credentials.imported_for(self.platform),
                            egui::Button::new(format!("退出 {} 登录", self.platform.label())),
                        )
                        .clicked()
                    {
                        self.logout_confirm = Some(self.platform);
                    }
                    if ui.button("查看诊断日志").clicked() {
                        self.data_tab = DataTab::Local;
                        self.data_category = DataCategory::Logs;
                        self.data_detail = true;
                        self.refresh_data_snapshot();
                    }
                    if ui.button("清理缓存和日志").clicked() {
                        self.data_tab = DataTab::Local;
                        self.cleanup_confirm = true;
                    }
                });
            }
            ui.add_space(8.0);
            ui.label(
                RichText::new(
                    "退出登录只清除本程序保存的该平台凭据；不读取或删除原版程序、日常浏览器或独立登录 profile。",
                )
                .small()
                .weak(),
            );
        });
        ui.add_space(12.0);
        ui.label(
            RichText::new(
                "这里展示本地最近观测，不发起平台请求；历史结果不能代表当前状态，也不能代替登录检查。",
            )
            .small()
            .weak(),
        );
    }

    pub(super) fn data_page(&mut self, ui: &mut egui::Ui) {
        ui.label(
            RichText::new("收藏、历史、配置与运行文件只读预览；平台状态展示本机最近观测。")
                .color(ui.visuals().weak_text_color()),
        );
        ui.add_space(14.0);
        ui.horizontal_wrapped(|ui| {
            if ui
                .selectable_label(self.data_tab == DataTab::Local, "本地数据")
                .clicked()
            {
                self.data_tab = DataTab::Local;
            }
            if ui
                .selectable_label(self.data_tab == DataTab::Platform, "平台运行状态")
                .clicked()
            {
                self.data_tab = DataTab::Platform;
            }
        });
        ui.add_space(12.0);
        match self.data_tab {
            DataTab::Local => self.local_data_tab(ui),
            DataTab::Platform => self.platform_status_tab(ui),
        }
    }

    /// Records the room and playback stages of a session snapshot. Danmaku is
    /// recorded from the actual overlay launch result, never assumed.
    pub(super) fn record_observations_from_snapshot(&mut self, snapshot: &SessionSnapshot) {
        let platform = snapshot.target.platform;
        if let Some(observed) = snapshot.room_observed_at {
            let state = super::observations::room_state(snapshot.room_state);
            self.record_observation(
                platform,
                super::observations::ObservationStage::Room,
                state,
                observed.timestamp().max(0) as u64,
                &snapshot.target.room,
            );
        }
        let phase = super::observations::playback_state(snapshot.phase);
        let at = snapshot.updated_at.timestamp().max(0) as u64;
        self.record_observation(
            platform,
            super::observations::ObservationStage::Playback,
            phase,
            at,
            &snapshot.target.room,
        );
    }
}

fn read_log_preview(path: &Path) -> String {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) => return format!("日志暂不可读：{error}"),
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => return format!("日志暂不可读：{error}"),
    };
    if !metadata.is_file() {
        return "该条目不是普通日志文件。".to_owned();
    }
    let length = metadata.len();
    let start = length.saturating_sub(MAX_LOG_PREVIEW_BYTES);
    let mut file = file;
    if start > 0 && std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(start)).is_err() {
        return "日志暂不可读。".to_owned();
    }
    let mut bytes = Vec::new();
    if file
        .take(MAX_LOG_PREVIEW_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return "日志暂不可读。".to_owned();
    }
    let text = String::from_utf8_lossy(&bytes);
    let text = if start > 0 {
        text.split_once('\n').map(|(_, rest)| rest).unwrap_or("")
    } else {
        &text
    };
    let mut output = String::new();
    if start > 0 {
        output.push_str("（仅显示日志末尾）\n");
    }
    for line in text.lines() {
        output.push_str(&sanitize_line(line));
        output.push('\n');
    }
    if output.trim().is_empty() {
        "日志为空。".to_owned()
    } else {
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_settings() -> Settings {
        Settings {
            mpv: Some(PathBuf::from("players/mpv.exe")),
            overlay: OverlaySettings {
                enabled: false,
                font_size: 33,
                ..Default::default()
            },
            playback: PlaybackPreferences {
                enhancement: Enhancement::Light,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn sizes_are_bounded_and_never_negative() {
        assert_eq!(size_text(0), "0 B");
        assert_eq!(size_text(1023), "1023 B");
        assert_eq!(size_text(1024), "1.00 KiB");
        assert_eq!(size_text(1024 * 1024), "1.00 MiB");
        assert_eq!(modified_text(None), "尚无文件");
        assert!(modified_text(Some(1_700_000_000)).contains(':'));
    }

    #[test]
    fn config_projection_lists_public_values_only() {
        let rows = config_rows(&synthetic_settings());
        let flat: Vec<String> = rows.iter().map(|row| row.join("=")).collect();
        assert!(flat.iter().any(|row| row == "mpv 程序路径=players/mpv.exe"));
        assert!(flat.iter().any(|row| row == "弹幕覆盖层=关闭"));
        assert!(flat.iter().any(|row| row == "画质增强=轻度"));
        assert!(flat.iter().any(|row| row == "弹幕字号=33 像素"));
        let joined = flat.join("\n").to_lowercase();
        assert!(!joined.contains("cookie"));
        assert!(!joined.contains("key"));
        assert!(!joined.contains("token"));
        assert!(rows.len() >= 20 && rows.len() <= 64);
    }

    #[test]
    fn snapshot_lists_only_known_runtime_files_and_never_protected_names() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(state_path(root), r#"{"synthetic":true}"#).unwrap();
        fs::create_dir_all(runtime_dir(root)).unwrap();
        fs::write(runtime_dir(root).join("platform-observations.json"), "{}").unwrap();
        fs::write(
            runtime_dir(root).join("diagnostics.log"),
            "synthetic line\n",
        )
        .unwrap();
        fs::write(runtime_dir(root).join("notes.txt"), "not ours").unwrap();
        fs::write(runtime_dir(root).join("cookie-backup.txt"), "synthetic").unwrap();
        fs::create_dir_all(runtime_dir(root).join("nested")).unwrap();

        let snapshot = build_snapshot(root, &[], &[], &synthetic_settings());
        let cache = snapshot
            .categories
            .iter()
            .find(|category| category.category == DataCategory::Cache)
            .unwrap();
        let logs = snapshot
            .categories
            .iter()
            .find(|category| category.category == DataCategory::Logs)
            .unwrap();
        assert!(
            cache
                .rows
                .iter()
                .any(|row| row[0] == "platform-observations.json")
        );
        assert!(cache.rows.iter().any(|row| row[0] == "notes.txt"));
        assert!(!cache.rows.iter().any(|row| row[0] == "cookie-backup.txt"));
        assert_eq!(logs.rows.len(), 1);
        assert_eq!(logs.rows[0][0], "diagnostics.log");
        assert!(cache.unit.ends_with("个文件"));
    }

    #[test]
    fn snapshot_rows_project_favorites_history_and_state_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(state_path(root), "{}").unwrap();
        let target = Target::new(Platform::Bili, "123").unwrap();
        let favorites = vec![Favorite {
            name: "合成主播".into(),
            target: target.clone(),
        }];
        let history = vec![HistoryEntry {
            name: "合成主播".into(),
            target,
            played_at: None,
            legacy_time: Some("10-30 18:00".into()),
        }];
        let snapshot = build_snapshot(root, &favorites, &history, &synthetic_settings());
        let favorites_view = &snapshot.categories[0];
        assert_eq!(favorites_view.count, 1);
        assert_eq!(favorites_view.rows[0], vec!["合成主播", "B站", "123"]);
        assert_eq!(favorites_view.location, "state.json");
        assert!(favorites_view.bytes > 0);
        let history_view = &snapshot.categories[1];
        assert_eq!(history_view.rows[0][3], "123");
    }

    #[test]
    fn cleanup_removes_only_launcher_owned_runtime_files() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(runtime_dir(root)).unwrap();
        fs::write(
            runtime_dir(root).join("platform-observations.json"),
            "cached",
        )
        .unwrap();
        fs::write(runtime_dir(root).join("diagnostics.log"), "log").unwrap();
        fs::write(runtime_dir(root).join("unrelated.bin"), "keep").unwrap();
        fs::write(runtime_dir(root).join("state.json"), "keep").unwrap();
        fs::write(state_path(root), "keep state").unwrap();
        let freed = cleanup_runtime(root).unwrap();
        assert!(freed > 0);
        assert!(
            !runtime_dir(root)
                .join("platform-observations.json")
                .exists()
        );
        assert!(!runtime_dir(root).join("diagnostics.log").exists());
        assert!(runtime_dir(root).join("unrelated.bin").exists());
        assert_eq!(cleanup_runtime(root).unwrap(), 0);
        assert_eq!(fs::read_to_string(state_path(root)).unwrap(), "keep state");
    }

    #[test]
    fn diagnostics_are_bounded_filtered_and_previewed() {
        let directory = tempfile::tempdir().unwrap();
        let log = DiagnosticLog::new(directory.path());
        log.record("登录失败 Cookie: SESSION=SYNTHETIC_SECRET");
        log.record("请求 https://example.invalid/private?token=SYNTHETIC 超时");
        log.record("普通诊断行 synthetic");
        let text = log.preview();
        assert!(!text.contains("SYNTHETIC_SECRET"));
        assert!(!text.contains("https://example.invalid"));
        assert!(text.contains("[已隐藏包含凭据字段的日志行]"));
        assert!(text.contains("[URL]"));
        assert!(text.contains("普通诊断行 synthetic"));
        // Writing a large amount never exceeds the documented cap.
        for index in 0..3_000 {
            log.record(&format!("synthetic diagnostic line {index}"));
        }
        let bytes = fs::metadata(log.path()).unwrap().len();
        assert!(
            bytes <= MAX_DIAGNOSTIC_BYTES + 1024,
            "diagnostic log grew past its cap: {bytes}"
        );
        log.clear().unwrap();
        assert!(!log.path().exists());
        assert!(log.preview().contains("尚无诊断日志"));
    }

    #[test]
    fn sanitize_keeps_plain_lines_and_redacts_urls_and_credentials() {
        assert_eq!(sanitize_line("普通行"), "普通行");
        assert!(sanitize_line("Cookie: a=b").contains("已隐藏"));
        assert!(sanitize_line("authorization bearer synthetic").contains("已隐藏"));
        assert!(!sanitize_line("打开 https://example.invalid/x 失败").contains("https"));
        assert!(sanitize_line("打开 https://example.invalid/x 失败").contains("[URL]"));
        assert!(!sanitize_line("control\u{7}character").contains('\u{7}'));
        assert_eq!(sanitize_line(&"x".repeat(2_000)).chars().count(), 513);
    }

    #[test]
    fn build_time_falls_back_to_a_labeled_unknown_or_executable_time() {
        assert_eq!(build_time_text(None), "未知时间");
        let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let text = build_time_text(Some(modified));
        assert!(text.contains("2023-11-14"), "unexpected build time: {text}");
        assert!(!build_version().is_empty());
    }
}
