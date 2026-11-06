//! Windowless resize/DPI acceptance for running danmaku tracks.
//!
//! User-visible regression this guards: resizing the player/overlay window must
//! not compress messages that are already flying into one line, reset them to the
//! right edge, or clear the layer. The C# original preserves the completed flight
//! fraction, re-derives the crossing speed from the new picture width and re-uses
//! the previous track whenever it still fits.
//!
//! The assertions always run offline: no window, no player, no network. When
//! `LIVE_HUB_LAYOUT_EVIDENCE_DIR` is set the same scenes are rasterised to PNG
//! (rectangles only: they show occupancy, not real font rendering) as before/after
//! layout evidence. This is not a real-window look acceptance.

use live_hub_rust::danmaku::{DanmakuMessage, DanmakuMode, LaneScheduler};
use std::path::{Path, PathBuf};

const FONT: f32 = 27.0;
const DURATION: f32 = 8.0;

fn message(index: usize, mode: DanmakuMode) -> DanmakuMessage {
    DanmakuMessage {
        image_url: None,
        id: format!("r{index}"),
        user: format!("合成用户{index}"),
        text: format!("合成弹幕 {index} 缩放保持轨道"),
        color: 0xffffff,
        mode,
        font_size: 27,
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

fn palette(index: usize) -> [u8; 4] {
    const COLORS: [[u8; 4]; 8] = [
        [235, 120, 96, 255],
        [96, 190, 235, 255],
        [235, 200, 96, 255],
        [150, 220, 120, 255],
        [200, 140, 235, 255],
        [120, 220, 200, 255],
        [240, 160, 200, 255],
        [170, 180, 240, 255],
    ];
    COLORS[index % COLORS.len()]
}

/// One rasterised frame at the scheduler's current viewport size. Every active
/// message becomes a filled rectangle at its current position; the left edge is
/// marked so a human can see progress, not just occupancy.
fn save_scene(
    scheduler: &LaneScheduler,
    width: u32,
    height: u32,
    now: f64,
    directory: &Path,
    name: &str,
) {
    let mut image = canvas(width, height);
    for (index, item) in scheduler.active().iter().enumerate() {
        let x = item.x(now).round() as i64;
        let y = item.y.round() as i64;
        let item_width = item.width.round().max(2.0) as u32;
        let item_height = item.height.round().max(2.0) as u32;
        fill(&mut image, (x, y, item_width, item_height), palette(index));
        fill(&mut image, (x, y, 3, item_height), [255, 255, 255, 255]);
        fill(&mut image, (x, y, item_width, 1), [255, 255, 255, 255]);
    }
    std::fs::create_dir_all(directory).expect("evidence directory");
    let path = directory.join(format!("{name}.png"));
    image.save(&path).expect("save evidence png");
    eprintln!("overlay resize evidence: {}", path.display());
}

#[derive(Debug, Clone)]
struct Snapshot {
    id: String,
    lane: usize,
    y: f32,
    x: f32,
    width: f32,
    progress: f32,
    speed: f32,
    duration: f64,
    viewport_width: f32,
    mode: DanmakuMode,
}

fn snapshot(scheduler: &LaneScheduler, now: f64) -> Vec<Snapshot> {
    scheduler
        .active()
        .iter()
        .map(|item| Snapshot {
            id: item.message.id.clone(),
            lane: item.lane,
            y: item.y,
            x: item.x(now),
            width: item.width,
            progress: item.progress(now),
            speed: item.speed,
            duration: item.duration,
            viewport_width: item.viewport_width,
            mode: item.message.mode,
        })
        .collect()
}

fn distinct_lanes(items: &[Snapshot]) -> usize {
    let mut lanes: Vec<usize> = items.iter().map(|item| item.lane).collect();
    lanes.sort_unstable();
    lanes.dedup();
    lanes.len()
}

fn distinct_positive_y(items: &[Snapshot]) -> usize {
    let mut rows: Vec<i64> = items.iter().map(|item| item.y.round() as i64).collect();
    rows.sort_unstable();
    rows.dedup();
    rows.len()
}

/// Seconds this scroll message still needs to leave the picture.
fn remaining_seconds(item: &Snapshot) -> f32 {
    match item.mode {
        DanmakuMode::Scroll => ((item.x + item.width) / item.speed.max(1.0)).max(0.0),
        DanmakuMode::Reverse => ((item.viewport_width - item.x) / item.speed.max(1.0)).max(0.0),
        DanmakuMode::Top | DanmakuMode::Bottom => 0.0,
    }
}

/// Two messages may share a track only while their painted boxes stay clear.
fn assert_no_same_track_overlap(items: &[Snapshot]) {
    for (index, left) in items.iter().enumerate() {
        for right in items.iter().skip(index + 1) {
            if left.lane != right.lane || left.mode != right.mode {
                continue;
            }
            if matches!(left.mode, DanmakuMode::Top | DanmakuMode::Bottom) {
                continue;
            }
            let left_end = left.x + left.width;
            let right_end = right.x + right.width;
            let clear = left_end <= right.x || right_end <= left.x;
            assert!(
                clear,
                "two messages overlap in track {}: {} x=[{:.1},{:.1}] {} x=[{:.1},{:.1}]",
                left.lane, left.id, left.x, left_end, right.id, right.x, right_end
            );
        }
    }
}

fn push_scroll_batch(scheduler: &mut LaneScheduler, count: usize) {
    for index in 0..count {
        let now = index as f64 * 0.45;
        let width = 210.0 + index as f32 * 13.0;
        scheduler
            .push_measured(message(index, DanmakuMode::Scroll), width, now)
            .unwrap_or_else(|| panic!("synthetic message {index} did not fit"));
    }
}

fn assert_pair(old: &Snapshot, new: &Snapshot, tolerance: f32) {
    assert_eq!(new.mode, old.mode, "mode changed for {}", old.id);
    assert!(
        (new.progress - old.progress).abs() <= tolerance,
        "{} progress changed {:.4} -> {:.4} on resize",
        old.id,
        old.progress,
        new.progress
    );
    assert!(
        (new.width - old.width).abs() <= 0.5,
        "{} measured width changed {:.2} -> {:.2}",
        old.id,
        old.width,
        new.width
    );
    if matches!(old.mode, DanmakuMode::Scroll | DanmakuMode::Reverse) {
        assert!(
            (new.duration - f64::from(DURATION)).abs() < 0.01,
            "{} crossing duration changed {:.2} -> {:.2}",
            old.id,
            old.duration,
            new.duration
        );
        // Speck: the C# resize keeps the remaining flight time within 50 ms.
        let before = remaining_seconds(old);
        let after = remaining_seconds(new);
        assert!(
            (after - before).abs() <= 0.05,
            "{} remaining flight time changed {:.3}s -> {:.3}s on resize",
            old.id,
            before,
            after
        );
    }
}

#[test]
fn shrinking_the_window_preserves_progress_pace_and_tracks() {
    let mut scheduler = LaneScheduler::new(1600.0, 900.0, FONT, DURATION);
    push_scroll_batch(&mut scheduler, 12);
    let now = 5.0;
    scheduler.advance(now);
    let before = snapshot(&scheduler, now);
    assert!(
        before.len() >= 8,
        "synthetic scene too small: {}",
        before.len()
    );
    let lanes_before = distinct_lanes(&before);
    assert!(lanes_before >= 3, "synthetic scene must use several tracks");
    if let Some(directory) = evidence_dir() {
        save_scene(
            &scheduler,
            1600,
            900,
            now,
            &directory,
            "resize-before-1600x900",
        );
    }

    scheduler.refit(1000.0, 700.0, now);
    let after = snapshot(&scheduler, now);
    if let Some(directory) = evidence_dir() {
        save_scene(
            &scheduler,
            1000,
            700,
            now,
            &directory,
            "resize-after-1000x700",
        );
    }

    assert_eq!(
        after.len(),
        before.len(),
        "a smaller window must keep already-running messages"
    );
    for old in &before {
        let new = after
            .iter()
            .find(|candidate| candidate.id == old.id)
            .unwrap_or_else(|| panic!("{} disappeared on resize", old.id));
        assert_pair(old, new, 0.02);
        assert!(
            (new.viewport_width - 1000.0).abs() < 0.5,
            "{} still uses the old viewport width {:.1}",
            old.id,
            new.viewport_width
        );
        if old.mode == DanmakuMode::Scroll {
            let expected_speed = (1000.0 + new.width) / f64::from(DURATION) as f32;
            assert!(
                (new.speed - expected_speed).abs() < 1.5,
                "{} speed {:.2} was not re-derived from the new width (expected {:.2})",
                old.id,
                new.speed,
                expected_speed
            );
        }
    }
    let lanes_after = distinct_lanes(&after);
    assert!(
        lanes_after > 1,
        "existing messages collapsed into a single track after resize"
    );
    assert_eq!(
        distinct_positive_y(&after),
        lanes_after,
        "every track must keep its own vertical position"
    );
    assert!(
        lanes_after + 1 >= lanes_before,
        "tracks collapsed {lanes_before} -> {lanes_after}"
    );
    assert!(
        after
            .iter()
            .all(|item| item.y >= -0.5 && item.y + 1.0 <= 700.0),
        "a running message was left outside the new viewport"
    );
    assert_no_same_track_overlap(&after);
}

#[test]
fn growing_the_window_keeps_every_message_flying() {
    let mut scheduler = LaneScheduler::new(1000.0, 700.0, FONT, DURATION);
    push_scroll_batch(&mut scheduler, 10);
    let now = 4.0;
    scheduler.advance(now);
    let before = snapshot(&scheduler, now);
    scheduler.refit(2400.0, 1300.0, now);
    let after = snapshot(&scheduler, now);
    assert_eq!(after.len(), before.len(), "growing must not drop messages");
    for old in &before {
        let new = after
            .iter()
            .find(|candidate| candidate.id == old.id)
            .unwrap_or_else(|| panic!("{} disappeared on grow", old.id));
        assert_pair(old, new, 0.02);
        let expected_speed = (2400.0 + new.width) / f64::from(DURATION) as f32;
        if old.mode == DanmakuMode::Scroll {
            assert!(
                (new.speed - expected_speed).abs() < 1.5,
                "{} speed {:.2} was not re-derived for the wider picture (expected {:.2})",
                old.id,
                new.speed,
                expected_speed
            );
        }
    }
    if let Some(directory) = evidence_dir() {
        save_scene(
            &scheduler,
            2400,
            1300,
            now,
            &directory,
            "grow-after-2400x1300",
        );
    }
}

#[test]
fn a_very_small_window_drops_only_what_cannot_fit() {
    let mut scheduler = LaneScheduler::new(1600.0, 900.0, FONT, DURATION);
    push_scroll_batch(&mut scheduler, 16);
    let now = 4.0;
    scheduler.advance(now);
    let before = snapshot(&scheduler, now);
    scheduler.refit(640.0, 360.0, now);
    let after = snapshot(&scheduler, now);
    let lanes_capacity = (360.0 / (43.0_f32.max(scheduler.line_height())))
        .floor()
        .max(1.0) as usize;
    if let Some(directory) = evidence_dir() {
        save_scene(
            &scheduler,
            640,
            360,
            now,
            &directory,
            "shrink-after-640x360",
        );
    }
    assert!(
        !after.is_empty(),
        "a 8-track window must still show some running messages"
    );
    for old in &before {
        if let Some(new) = after.iter().find(|candidate| candidate.id == old.id) {
            assert_pair(old, new, 0.05);
        }
    }
    assert!(
        distinct_lanes(&after) <= lanes_capacity.max(1),
        "messages were placed on tracks that do not exist in the small window"
    );
    assert!(
        distinct_lanes(&after) > 1,
        "the shrink collapsed every surviving message into one track"
    );
    assert!(
        after
            .iter()
            .all(|item| item.y >= -0.5 && item.y + 1.0 <= 360.0),
        "a surviving message was left outside the small viewport"
    );
    assert_no_same_track_overlap(&after);
}

#[test]
fn dpi_change_is_the_same_progress_preserving_path() {
    // The same physical window at 150% DPI reports a 1/1.5 logical size: the
    // overlay feeds the new point-size through the same refit path.
    let mut scheduler = LaneScheduler::new(1600.0, 900.0, FONT, DURATION);
    push_scroll_batch(&mut scheduler, 10);
    let now = 3.5;
    scheduler.advance(now);
    let before = snapshot(&scheduler, now);
    scheduler.refit(1067.0, 600.0, now);
    let after = snapshot(&scheduler, now);
    assert_eq!(
        after.len(),
        before.len(),
        "a DPI change must keep the layer"
    );
    for old in &before {
        let new = after
            .iter()
            .find(|candidate| candidate.id == old.id)
            .unwrap_or_else(|| panic!("{} disappeared on a DPI change", old.id));
        assert_pair(old, new, 0.02);
    }
    assert_no_same_track_overlap(&after);
    if let Some(directory) = evidence_dir() {
        save_scene(&scheduler, 1067, 600, now, &directory, "dpi-after-1067x600");
    }
}

#[test]
fn fixed_messages_survive_a_resize_and_stay_centred() {
    let mut scheduler = LaneScheduler::new(1600.0, 900.0, FONT, DURATION);
    let now = 1.0;
    for (index, mode) in [DanmakuMode::Top, DanmakuMode::Bottom]
        .into_iter()
        .enumerate()
    {
        let mut entry = message(index, mode);
        entry.text = format!("合成固定弹幕 {index}");
        scheduler
            .push_measured(entry, 300.0, 0.5)
            .expect("fixed message fits");
    }
    scheduler.advance(now);
    let before = snapshot(&scheduler, now);
    assert_eq!(before.len(), 2);
    scheduler.refit(900.0, 500.0, now);
    let after = snapshot(&scheduler, now);
    assert_eq!(after.len(), 2, "fixed messages must survive a resize");
    for old in &before {
        let new = after
            .iter()
            .find(|candidate| candidate.id == old.id)
            .unwrap_or_else(|| panic!("{} disappeared on resize", old.id));
        assert_pair(old, new, 0.02);
        let centred = (900.0 - new.width) / 2.0;
        assert!(
            (new.x - centred).abs() < 1.0,
            "{} was not re-centred: x={:.1} expected {:.1}",
            old.id,
            new.x,
            centred
        );
    }
}

#[test]
fn messages_pushed_after_a_resize_use_the_new_geometry() {
    let mut scheduler = LaneScheduler::new(1600.0, 900.0, FONT, DURATION);
    push_scroll_batch(&mut scheduler, 4);
    let now = 3.0;
    scheduler.advance(now);
    scheduler.refit(1000.0, 700.0, now);
    let mut fresh = message(99, DanmakuMode::Scroll);
    fresh.text = "合成新弹幕".to_owned();
    let placed = scheduler
        .push_measured(fresh, 180.0, now)
        .expect("a new message fits after the resize");
    assert!(
        (placed.viewport_width - 1000.0).abs() < 0.5,
        "new message used stale geometry {:.1}",
        placed.viewport_width
    );
    assert!(
        (placed.speed - (1000.0 + 180.0) / f64::from(DURATION) as f32).abs() < 1.5,
        "new message did not use the new crossing duration"
    );
    assert!(
        (placed.x(now) - 1000.0).abs() < 0.5,
        "a new message must enter from the right edge of the new picture"
    );
}
