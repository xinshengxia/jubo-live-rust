//! Reproducible per-page screenshot diagnostic for the native launcher.
//!
//! This module is a diagnostic only: a normal launch never instantiates a
//! [`Driver`]. The caller opens the window off-screen and inactive, selects each
//! page through [`super::Launcher::shot_select_page`], and the renderer's own
//! framebuffer is captured with `egui::ViewportCommand::Screenshot`. No desktop
//! capture, third-party tool or platform API is involved, so the pixels are
//! exactly what the Rust renderer produced.
//!
//! Writes are confined to the explicit output directory. `capture.json` records
//! page keys and bare file names only: no user data, no absolute paths, no room
//! titles or credentials. What this evidence proves is the renderer output of
//! each page at one logical size on this host; it does not prove the window is
//! correct on a real desktop, at another DPI scale, or with live data.

use super::Launcher;
use eframe::egui;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Final page keys (the 8-page shell). Kept here so the CLI and the capture
/// report share one list; unknown keys are reported and skipped, never guessed.
pub(super) const DEFAULT_PAGE_KEYS: [&str; 8] = [
    "favorites",
    "recent",
    "lookup",
    "sessions",
    "players",
    "danmaku",
    "accounts",
    "data",
];

/// A page that never produced a rendered frame within this window aborts the
/// run: waiting longer only hides a broken renderer behind a timeout.
const PAGE_TIMEOUT: Duration = Duration::from_secs(20);
/// Whole-run guard so a headless CI job can never hang on a hidden window.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(180);

/// Diagnostic options. `pages` accepts page keys and the literal `all`.
pub(super) struct ShotOptions {
    pub dir: PathBuf,
    pub pages: Vec<String>,
    pub width: f32,
    pub height: f32,
    pub settle_frames: u32,
}

/// One saved page capture (bare file name on purpose: the report is portable).
struct Capture {
    key: String,
    file: String,
    pixel_width: u32,
    pixel_height: u32,
}

/// Frame-driven capture state machine.
///
/// The caller drives it from `eframe::App::update`:
///
/// ```ignore
/// ui_shot::begin_frame(driver, self, context);
/// self.render(context);
/// ui_shot::end_frame(driver, self, context);
/// ```
pub(super) struct Driver {
    dir: PathBuf,
    pages: Vec<String>,
    index: usize,
    settle_frames: u32,
    settle_remaining: u32,
    selected: bool,
    requested: bool,
    requested_at: Instant,
    started: Instant,
    pixels_per_point: f32,
    captures: Vec<Capture>,
    failures: Vec<String>,
    finished: bool,
}

impl Driver {
    pub(super) fn new(options: ShotOptions) -> Self {
        Self {
            dir: options.dir,
            pages: expand_pages(&options.pages),
            index: 0,
            settle_frames: options.settle_frames.max(1),
            settle_remaining: 0,
            selected: false,
            requested: false,
            requested_at: Instant::now(),
            started: Instant::now(),
            pixels_per_point: 1.0,
            captures: Vec::new(),
            failures: Vec::new(),
            finished: false,
        }
    }

    /// State machine prologue: collect the previous frame's screenshot, advance
    /// to the next page, and close the window once every page is captured.
    pub(super) fn begin_frame(&mut self, launcher: &mut Launcher, ctx: &egui::Context) {
        if self.finished {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        self.pixels_per_point = ctx.pixels_per_point();
        if self.started.elapsed() > TOTAL_TIMEOUT {
            self.failures.push(format!(
                "整轮截图超过 {} 秒未完成",
                TOTAL_TIMEOUT.as_secs()
            ));
            self.finish(ctx);
            return;
        }

        // 1) The renderer answers a previous frame's Screenshot command with an
        //    egui event carrying the framebuffer bytes.
        let mut image = None;
        ctx.input(|input| {
            for event in &input.events {
                if let egui::Event::Screenshot { image: shot, .. } = event {
                    image = Some(shot.clone());
                }
            }
        });
        if let Some(image) = image {
            self.store_current(&image);
            self.requested = false;
            self.selected = false;
            self.index += 1;
        }
        if self.finished {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if self.requested && self.requested_at.elapsed() > PAGE_TIMEOUT {
            self.failures.push(format!(
                "页面 {:?} 在 {} 秒内没有返回渲染帧",
                self.current_key(),
                PAGE_TIMEOUT.as_secs()
            ));
            self.finish(ctx);
            return;
        }

        // 2) Select the page that this frame will render.
        while !self.selected {
            let Some(key) = self.pages.get(self.index).cloned() else {
                self.finish(ctx);
                return;
            };
            if launcher.shot_select_page(&key) {
                self.selected = true;
                self.settle_remaining = self.settle_frames;
                self.requested = false;
            } else {
                self.failures
                    .push(format!("页面 {key:?} 不受当前构建支持，已跳过"));
                self.index += 1;
            }
        }
        ctx.request_repaint();
    }

    /// State machine epilogue: after the page was rendered, wait for the layout
    /// to settle and ask the renderer for the framebuffer.
    pub(super) fn end_frame(&mut self, _launcher: &mut Launcher, ctx: &egui::Context) {
        if self.finished || !self.selected || self.requested {
            return;
        }
        if self.settle_remaining > 0 {
            self.settle_remaining -= 1;
            ctx.request_repaint();
            return;
        }
        self.requested = true;
        self.requested_at = Instant::now();
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        ctx.request_repaint();
    }

    fn current_key(&self) -> &str {
        self.pages.get(self.index).map(String::as_str).unwrap_or("")
    }

    fn store_current(&mut self, image: &egui::ColorImage) {
        if !self.selected {
            self.failures
                .push("收到截图但当前没有已选页面，已丢弃".to_owned());
            return;
        }
        let key = self.current_key().to_owned();
        let file = format!("{key}.png");
        match write_png(&self.dir.join(&file), image) {
            Ok(()) => self.captures.push(Capture {
                key,
                file,
                pixel_width: image.size[0] as u32,
                pixel_height: image.size[1] as u32,
            }),
            Err(error) => self
                .failures
                .push(format!("页面 {key:?} 截图写入失败：{error}")),
        }
    }

    fn finish(&mut self, ctx: &egui::Context) {
        if self.finished {
            return;
        }
        self.finished = true;
        if let Err(error) = self.write_report() {
            self.failures.push(format!("capture.json 写入失败：{error}"));
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn write_report(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let pages: Vec<serde_json::Value> = self
            .captures
            .iter()
            .map(|capture| {
                serde_json::json!({
                    "key": capture.key,
                    "file": capture.file,
                    "pixel_width": capture.pixel_width,
                    "pixel_height": capture.pixel_height,
                })
            })
            .collect();
        let document = serde_json::json!({
            "stage": "ui-shot",
            "version": env!("CARGO_PKG_VERSION"),
            "ok": self.failures.is_empty() && self.captures.len() == self.pages.len(),
            "requested_pages": self.pages,
            "pixels_per_point": self.pixels_per_point,
            "settle_frames": self.settle_frames,
            "pages": pages,
            "failures": self.failures,
            "elapsed_ms": self.started.elapsed().as_millis() as u64,
            "note": "窗口以非激活方式打开在屏幕外，截图取自渲染器自身帧缓冲；本报告仅记录页面键与文件名。",
        });
        let bytes = serde_json::to_vec_pretty(&document).map_err(std::io::Error::other)?;
        std::fs::write(self.dir.join("capture.json"), bytes)
    }
}

/// Expand `all`, de-duplicate and drop blanks. An empty request means every page.
fn expand_pages(pages: &[String]) -> Vec<String> {
    let mut expanded: Vec<String> = Vec::new();
    for page in pages {
        let page = page.trim();
        if page.is_empty() {
            continue;
        }
        if page.eq_ignore_ascii_case("all") {
            for key in DEFAULT_PAGE_KEYS {
                push_unique(&mut expanded, key);
            }
        } else {
            push_unique(&mut expanded, page);
        }
    }
    if expanded.is_empty() {
        for key in DEFAULT_PAGE_KEYS {
            push_unique(&mut expanded, key);
        }
    }
    expanded
}

fn push_unique(pages: &mut Vec<String>, page: &str) {
    let normalized = page.to_ascii_lowercase();
    if !pages.iter().any(|existing| existing == &normalized) {
        pages.push(normalized);
    }
}

fn write_png(path: &Path, image: &egui::ColorImage) -> std::io::Result<()> {
    use image::ImageEncoder;
    let file = std::fs::File::create(path)?;
    let bytes: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(|color| color.to_array())
        .collect();
    image::codecs::png::PngEncoder::new(std::io::BufWriter::new(file))
        .write_image(
            &bytes,
            image.size[0] as u32,
            image.size[1] as u32,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_PAGE_KEYS, expand_pages};

    fn pages(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn all_expands_to_the_documented_page_order() {
        assert_eq!(expand_pages(&pages(&["all"])), DEFAULT_PAGE_KEYS);
        assert_eq!(expand_pages(&pages(&["ALL"])), DEFAULT_PAGE_KEYS);
    }

    #[test]
    fn empty_request_means_every_page() {
        assert_eq!(expand_pages(&[]), DEFAULT_PAGE_KEYS);
        assert_eq!(expand_pages(&pages(&["", "  "])), DEFAULT_PAGE_KEYS);
    }

    #[test]
    fn explicit_pages_are_normalized_and_deduplicated() {
        assert_eq!(
            expand_pages(&pages(&[" Data", "data", "FAVORITES"])),
            ["data".to_owned(), "favorites".to_owned()]
        );
    }

    #[test]
    fn a_mixed_request_keeps_explicit_pages_and_all_once() {
        // Explicit keys keep their request order; "all" then appends the remaining
        // shell pages in canonical order and never duplicates a key.
        assert_eq!(
            expand_pages(&pages(&["lookup", "all", "LOOKUP"])),
            [
                "lookup".to_owned(),
                "favorites".to_owned(),
                "recent".to_owned(),
                "sessions".to_owned(),
                "players".to_owned(),
                "danmaku".to_owned(),
                "accounts".to_owned(),
                "data".to_owned(),
            ]
        );
    }
}
