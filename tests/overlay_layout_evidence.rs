//! Windowless layout evidence for danmaku/paid-message rendering.
//!
//! The assertions always run offline. When LIVE_HUB_LAYOUT_EVIDENCE_DIR is set,
//! the same scenes are rasterised to PNG (rectangles only: they show occupancy,
//! not real font rendering) as layout evidence. This is not a real-window look
//! acceptance and does not open any player or overlay window.

// The overlay module only exists with the default gui feature; without it this
// target must compile to an empty test crate instead of failing the no-GUI matrix.
#![cfg(feature = "gui")]

use live_hub_rust::core::ScCorner;
use live_hub_rust::danmaku::{DanmakuMessage, DanmakuMode, LaneScheduler};
use live_hub_rust::overlay::{crowd_scale, sc_card_width_points, sc_layout, sc_points_scale};
use std::path::{Path, PathBuf};

fn message(index: usize, mode: DanmakuMode) -> DanmakuMessage {
    DanmakuMessage {
        image_url: None,
        id: format!("m{index}"),
        user: format!("合成用户{index}"),
        text: format!("合成弹幕 {index} 测试布局"),
        color: 0xffffff,
        mode,
        font_size: 25,
        user_id: 0,
        user_level: 0,
        guard_level: 0,
        medal: None,
        image_width: 0,
        image_height: 0,
        image_large: false,
    }
}

fn evidence_dir() -> Option<PathBuf> {
    std::env::var_os("LIVE_HUB_LAYOUT_EVIDENCE_DIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn canvas(width: u32, height: u32) -> image::RgbaImage {
    image::RgbaImage::from_pixel(width, height, image::Rgba([16, 18, 24, 255]))
}

fn fill(image: &mut image::RgbaImage, rect: (i64, i64, u32, u32), color: [u8; 4]) {
    let (x, y, width, height) = rect;
    for py in y.max(0)..(y + height as i64).min(image.height() as i64) {
        for px in x.max(0)..(x + width as i64).min(image.width() as i64) {
            image.put_pixel(px as u32, py as u32, image::Rgba(color));
        }
    }
}

fn save(image: &image::RgbaImage, directory: &Path, name: &str) {
    std::fs::create_dir_all(directory).expect("evidence directory");
    let path = directory.join(format!("{name}.png"));
    image.save(&path).expect("save evidence png");
    eprintln!("layout evidence: {}", path.display());
}

fn assert_rows_do_not_collide(items: &[live_hub_rust::danmaku::FlyingDanmaku]) {
    for (index, left) in items.iter().enumerate() {
        assert!(left.y >= -0.5, "y below viewport: {}", left.y);
        for right in items.iter().skip(index + 1) {
            let vertical = left.y < right.y + right.height && left.y + left.height > right.y;
            if !vertical {
                continue;
            }
            // Overlapping rows are only legal for same-direction scrolling
            // messages that keep a clearing gap at their entry edge.
            let same_scroll = left.message.mode == right.message.mode
                && matches!(
                    left.message.mode,
                    DanmakuMode::Scroll | DanmakuMode::Reverse
                );
            assert!(same_scroll, "different rows overlap: {left:?} vs {right:?}");
        }
    }
}

#[test]
fn normal_scene_lanes_are_inside_the_viewport() {
    let mut scheduler = LaneScheduler::new(1280.0, 720.0, 25.0, 8.0);
    for index in 0..14 {
        let mode = match index % 7 {
            0 => DanmakuMode::Top,
            1 => DanmakuMode::Bottom,
            2 => DanmakuMode::Reverse,
            _ => DanmakuMode::Scroll,
        };
        let _ = scheduler.push_measured(message(index, mode), 320.0, 0.0);
    }
    assert!(!scheduler.active().is_empty());
    assert_rows_do_not_collide(scheduler.active());
    for item in scheduler.active() {
        assert!(item.start <= 0.0);
        assert!(item.y + item.height <= 720.0 + 0.5);
        assert!(item.width > 0.0 && item.height > 0.0);
        // Legacy speed model: every scrolling message crosses in the same fixed
        // duration, so its speed depends on its own width; fixed rows keep 0.
        if matches!(item.message.mode, DanmakuMode::Scroll | DanmakuMode::Reverse) {
            assert!((item.speed - (1280.0 + item.width) / 8.0).abs() < 1e-3);
        } else {
            assert_eq!(item.speed, 0.0);
        }
    }

    if let Some(directory) = evidence_dir() {
        let mut image = canvas(1280, 720);
        for item in scheduler.active() {
            let x = item.x(5.0);
            let color = match item.message.mode {
                DanmakuMode::Top => [90, 200, 140, 255],
                DanmakuMode::Bottom => [255, 190, 80, 255],
                DanmakuMode::Reverse => [240, 90, 210, 255],
                DanmakuMode::Scroll => [130, 175, 255, 255],
            };
            fill(
                &mut image,
                (x as i64, item.y as i64, item.width as u32, item.height as u32),
                color,
            );
        }
        // Two paid-message cards in the default top-right corner (legacy
        // geometry: 270*scale wide, 10*scale margin, 4*scale gap).
        let heights = [96.0, 88.0];
        let scale = sc_points_scale(100, 720.0, 720, true);
        let width = sc_card_width_points(scale);
        let area = eframe::egui::Rect::from_min_size(
            eframe::egui::Pos2::ZERO,
            eframe::egui::vec2(1280.0, 720.0),
        );
        let layout = sc_layout(area, ScCorner::TopRight, scale, &heights, 6, 0.0, 1.0);
        for card in layout.cards {
            fill(
                &mut image,
                (
                    card.rect.left() as i64,
                    card.rect.top() as i64,
                    card.rect.width() as u32,
                    card.rect.height() as u32,
                ),
                [42, 96, 178, 210],
            );
        }
        let _ = width;
        save(&image, &directory, "rich-media-layout-normal");
    }
}

#[test]
fn crowded_scene_applies_the_crowded_scale_and_stays_bounded() {
    // A small capacity makes the 80% crowded threshold reachable on a bounded
    // synthetic screen.
    let max_visible: u16 = 20;
    let mut scheduler = LaneScheduler::new(960.0, 540.0, 24.0, 8.0);
    scheduler.set_capacity(usize::from(max_visible));
    let mut accepted = 0;
    let mut rejected = 0;
    for index in 0..(usize::from(max_visible) + 120) {
        if scheduler
            .push_measured(message(index, DanmakuMode::Scroll), 280.0, 0.0)
            .is_some()
        {
            accepted += 1;
        } else {
            rejected += 1;
        }
    }
    assert_eq!(accepted + rejected, usize::from(max_visible) + 120);
    assert!(accepted <= usize::from(max_visible));
    assert!(accepted >= 10, "accepted={accepted}");
    assert!(rejected > 100, "rejected={rejected}");
    assert_eq!(scheduler.active().len(), accepted);
    assert_rows_do_not_collide(scheduler.active());
    // The crowded policy shrinks ordinary messages once occupancy reaches 80%.
    let budget = usize::from(max_visible).min(scheduler.track_count() * 2);
    assert_eq!(crowd_scale(0, budget, true, 85), 1.0);
    let expected_scale = if accepted * 10 >= budget * 8 { 0.85 } else { 1.0 };
    assert!(
        (crowd_scale(accepted, budget, true, 85) - expected_scale).abs() < 1e-4,
        "accepted={accepted}"
    );

    if let Some(directory) = evidence_dir() {
        let mut image = canvas(960, 540);
        for item in scheduler.active() {
            let scale = crowd_scale(accepted, budget, true, 85);
            let height = (item.height * scale).max(1.0);
            fill(
                &mut image,
                (
                    item.x(5.0) as i64,
                    item.y as i64,
                    (item.width * scale) as u32,
                    height as u32,
                ),
                if scale < 1.0 {
                    [255, 130, 90, 200]
                } else {
                    [130, 175, 255, 200]
                },
            );
        }
        save(&image, &directory, "rich-media-layout-crowded");
    }
}

#[test]
fn window_shrink_refits_without_resetting_flight_progress() {
    let mut scheduler = LaneScheduler::new(1280.0, 720.0, 25.0, 8.0);
    for index in 0..12 {
        let mode = if index % 3 == 0 {
            DanmakuMode::Top
        } else {
            DanmakuMode::Scroll
        };
        let _ = scheduler.push_measured(message(index, mode), 260.0, 0.0);
    }
    let before = scheduler.active().len();
    assert!(before >= 6, "too few messages placed: {before}");
    // Progress is compared per message id: refit keeps every message but may
    // reorder the internal list.
    let progress_before: std::collections::BTreeMap<String, f32> = scheduler
        .active()
        .iter()
        .map(|item| (item.message.id.clone(), item.progress(0.5)))
        .collect();
    scheduler.refit(800.0, 450.0, 0.5);
    // Legacy Resize keeps the completed fraction and only drops an item when
    // no track can hold it; never clears the layer.
    let after_first = scheduler.active().len();
    assert!(
        after_first + 1 >= before && after_first >= before / 2,
        "refit discarded too many: before={before} after={after_first}"
    );
    assert_rows_do_not_collide(scheduler.active());
    for item in scheduler.active() {
        if let Some(previous) = progress_before.get(&item.message.id) {
            assert!(
                (item.progress(0.5) - previous).abs() < 0.05,
                "flight progress reset after resize: {}",
                item.message.id
            );
        }
        assert!(item.y + item.height <= 450.0 + 0.5);
        assert!(item.viewport_width <= 800.0 + 0.5);
    }
    scheduler.refit(640.0, 360.0, 1.0);
    let after_second = scheduler.active().len();
    assert!(
        after_second >= before / 2 && after_second > 0,
        "second refit cleared the layer: {after_second}"
    );
    assert_rows_do_not_collide(scheduler.active());
    for item in scheduler.active() {
        assert!(item.y + item.height <= 360.0 + 0.5);
    }

    if let Some(directory) = evidence_dir() {
        let mut image = canvas(640, 360);
        for item in scheduler.active() {
            let color = if item.message.mode == DanmakuMode::Top {
                [90, 200, 140, 255]
            } else {
                [130, 175, 255, 255]
            };
            fill(
                &mut image,
                (
                    item.x(1.0) as i64,
                    item.y as i64,
                    item.width as u32,
                    item.height as u32,
                ),
                color,
            );
        }
        save(&image, &directory, "rich-media-layout-shrunk");
    }
}
