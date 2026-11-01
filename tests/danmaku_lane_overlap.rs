//! Offline probe for the same-lane collision invariant of the danmaku
//! scheduler. Runs without the GUI feature so it can be executed while other
//! workstreams build the UI layer.
//!
//!   cargo test --no-default-features --locked --offline --test danmaku_lane_overlap

use live_hub_rust::danmaku::{DanmakuMessage, DanmakuMode, LaneScheduler};

fn message(index: usize) -> DanmakuMessage {
    DanmakuMessage {
        image_url: None,
        id: format!("m{index}"),
        user: "合成".into(),
        text: format!("resize 场景弹幕 {index} · 验证缩放后保持轨道与进度"),
        color: 0xffffff,
        mode: DanmakuMode::Scroll,
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

fn overlaps(scheduler: &LaneScheduler, now: f64) -> Vec<String> {
    // Only messages that have already started: a message placed for a later
    // instant sits at the entry edge and would look like a collision.
    let items: Vec<_> = scheduler
        .active()
        .iter()
        .filter(|item| item.start <= now && item.visible(now))
        .collect();
    let mut problems = Vec::new();
    for (index, left) in items.iter().enumerate() {
        for right in items.iter().skip(index + 1) {
            if left.lane != right.lane {
                continue;
            }
            let (l0, l1) = (left.x(now), left.x(now) + left.width);
            let (r0, r1) = (right.x(now), right.x(now) + right.width);
            if !(l1 <= r0 + 0.5 || r1 <= l0 + 0.5) {
                problems.push(format!(
                    "lane {} t={now}: {:.1}..{:.1} vs {:.1}..{:.1}",
                    left.lane, l0, l1, r0, r1
                ));
            }
        }
    }
    problems
}

#[test]
fn timed_scrolls_never_overlap_on_a_lane() {
    let mut scheduler = LaneScheduler::new(1280.0, 576.0, 25.0, 8.0);
    let mut now = 0.0_f64;
    for index in 0..48 {
        let _ = scheduler.push_measured(message(index), 620.0, now);
        now += 0.26;
        scheduler.advance(now);
    }
    let mut all = Vec::new();
    for sample in [0.5_f64, 1.0, 2.0, 4.0, 6.5, 9.0, 11.0] {
        all.extend(overlaps(&scheduler, sample));
    }
    assert!(all.is_empty(), "{:#?}", all);
}

#[test]
fn resize_keeps_lanes_and_the_no_overlap_invariant() {
    let mut scheduler = LaneScheduler::new(960.0, 432.0, 25.0, 8.0);
    let mut now = 0.0_f64;
    for index in 0..30 {
        let _ = scheduler.push_measured(message(index), 480.0, now);
        now += 0.3;
        scheduler.advance(now);
    }
    scheduler.refit(1280.0, 576.0, now);
    let mut all = Vec::new();
    for sample in [now, now + 0.5, now + 1.5, now + 3.0] {
        all.extend(overlaps(&scheduler, sample));
    }
    assert!(all.is_empty(), "{:#?}", all);
}
