use super::{Rect, egui};
use egui::epaint::{ClippedShape, Shape};

const TILE_W: i32 = 128;
const TILE_H: i32 = 64;

pub(super) struct Scene {
    pub pixels_per_point: f32,
    shapes: Vec<ClippedShape>,
}

impl Scene {
    pub fn collect(ctx: &egui::Context) -> Self {
        // Take the same paint order, transforms and clips as egui, before our
        // own framebuffer callbacks are inserted into the background/debug layers.
        let (order, transforms) =
            ctx.memory(|m| (m.layer_ids().collect::<Vec<_>>(), m.to_global.clone()));
        Self {
            pixels_per_point: ctx.pixels_per_point(),
            shapes: ctx.graphics(|g| g.clone().drain(&order, &transforms)),
        }
    }
}

fn volatile(shape: &Shape) -> bool {
    match shape {
        // The owner may update a texture without changing its ID or quad.
        Shape::Vec(children) => children.iter().any(volatile),
        Shape::Callback(_) => true,
        _ => shape.texture_id() != egui::TextureId::default(),
    }
}

fn same(a: &ClippedShape, b: &ClippedShape) -> bool {
    if a.clip_rect != b.clip_rect || volatile(&b.shape) {
        return false;
    }
    match (&a.shape, &b.shape) {
        (Shape::Text(a), Shape::Text(b)) => {
            a.pos == b.pos
                && std::sync::Arc::ptr_eq(&a.galley, &b.galley)
                && a.angle == b.angle
                && a.underline == b.underline
                && a.fallback_color == b.fallback_color
                && a.override_text_color == b.override_text_color
                && a.opacity_factor == b.opacity_factor
        }
        _ => a.shape == b.shape,
    }
}

pub(super) fn bounds(a: Rect, b: Rect) -> Rect {
    Rect {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    }
}

pub(super) fn full([w, h]: [i32; 2]) -> Rect {
    Rect {
        left: 0,
        top: 0,
        right: w,
        bottom: h,
    }
}

pub(super) fn area(r: Rect) -> i64 {
    i64::from(r.right - r.left) * i64::from(r.bottom - r.top)
}

struct Tiles {
    size: [i32; 2],
    columns: usize,
    bits: Vec<bool>,
}

impl Tiles {
    fn new([w, h]: [i32; 2]) -> Self {
        let columns = ((w - 1) / TILE_W + 1) as usize;
        let rows = ((h - 1) / TILE_H + 1) as usize;
        Self {
            size: [w, h],
            columns,
            bits: vec![false; columns * rows],
        }
    }

    fn mark(&mut self, shape: &ClippedShape, scale: f32) {
        let shape_rect = shape.shape.visual_bounding_rect();
        if shape_rect.is_negative() {
            return;
        }
        let [w, h] = self.size;
        // Feathering and glyph rounding extend slightly beyond mathematical bounds.
        let r = shape_rect
            .expand(3.0 / scale)
            .intersect(shape.clip_rect.expand(1.0 / scale));
        if r.is_negative() {
            return;
        }
        if !r.is_finite() {
            self.bits.fill(true);
            return;
        }
        let left = ((r.left() * scale).floor() as i32).clamp(0, w);
        let right = ((r.right() * scale).ceil() as i32).clamp(0, w);
        let top = ((r.top() * scale).floor() as i32).clamp(0, h);
        let bottom = ((r.bottom() * scale).ceil() as i32).clamp(0, h);
        if left >= right || top >= bottom {
            return;
        }
        for y in top / TILE_H..=(bottom - 1) / TILE_H {
            let start = y as usize * self.columns;
            self.bits[start + (left / TILE_W) as usize..=start + ((right - 1) / TILE_W) as usize]
                .fill(true);
        }
    }

    fn regions(&self) -> Vec<Rect> {
        let [w, h] = self.size;
        let mut patches: Vec<Rect> = Vec::new();
        let mut bands: Vec<Rect> = Vec::new();
        for (row, tiles) in self.bits.chunks_exact(self.columns).enumerate() {
            let top = row as i32 * TILE_H;
            let mut x = 0;
            let mut span = None;
            while x < tiles.len() {
                if !tiles[x] {
                    x += 1;
                    continue;
                }
                let start = x;
                while x < tiles.len() && tiles[x] {
                    x += 1;
                }
                let patch = Rect {
                    left: start as i32 * TILE_W,
                    right: (x as i32 * TILE_W).min(w),
                    top,
                    bottom: (top + TILE_H).min(h),
                };
                if let Some(prior) = patches
                    .iter_mut()
                    .rev()
                    .find(|p| p.bottom == top && p.left == patch.left && p.right == patch.right)
                {
                    prior.bottom = patch.bottom;
                } else {
                    patches.push(patch);
                }
                span = Some(span.map_or(patch, |prev| bounds(prev, patch)));
            }
            if let Some(span) = span {
                if let Some(prior) = bands.last_mut().filter(|b| b.bottom == top) {
                    *prior = bounds(*prior, span);
                } else {
                    bands.push(span);
                }
            }
        }
        // Many tiny synchronous GPU reads can cost more than a few larger bands.
        let regions = if patches.len() > 10 { bands } else { patches };
        if regions.len() > 10
            || regions.iter().copied().map(area).sum::<i64>() * 3 > i64::from(w) * i64::from(h) * 2
        {
            vec![full(self.size)]
        } else {
            regions
        }
    }
}

pub(super) fn changed(previous: Option<&Scene>, next: &Scene, size: [i32; 2]) -> Vec<Rect> {
    let Some(prior) = previous else {
        return vec![full(size)];
    };
    if prior.pixels_per_point != next.pixels_per_point {
        return vec![full(size)];
    }
    let mut tiles = Tiles::new(size);
    let (mut a, mut b) = (prior.shapes.as_slice(), next.shapes.as_slice());
    // Keep a stationary status line or card clean if earlier shapes appear/disappear.
    while let (Some((x, rest_a)), Some((y, rest_b))) = (a.split_last(), b.split_last()) {
        if !same(x, y) {
            break;
        }
        a = rest_a;
        b = rest_b;
    }
    for i in 0..a.len().max(b.len()) {
        let (old, new) = (a.get(i), b.get(i));
        if let (Some(old), Some(new)) = (old, new)
            && same(old, new)
        {
            continue;
        }
        for shape in old.into_iter().chain(new) {
            tiles.mark(shape, next.pixels_per_point);
        }
    }
    tiles.regions()
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Color32, pos2};

    fn block(x: f32, y: f32) -> ClippedShape {
        ClippedShape {
            clip_rect: egui::Rect::EVERYTHING,
            shape: Shape::rect_filled(
                egui::Rect::from_min_max(pos2(x, y), pos2(x + 12.0, y + 8.0)),
                0.0,
                Color32::WHITE,
            ),
        }
    }
    fn scene(shapes: Vec<ClippedShape>) -> Scene {
        Scene {
            pixels_per_point: 1.0,
            shapes,
        }
    }
    fn covered(rects: &[Rect], x: i32, y: i32) -> bool {
        rects
            .iter()
            .any(|r| r.left <= x && x < r.right && r.top <= y && y < r.bottom)
    }
    #[test]
    fn movement_and_removal_clear_old_pixels_without_touching_static_hint() {
        let old = scene(vec![block(30., 35.), block(800., 300.), block(300., 700.)]);
        let new = scene(vec![block(200., 35.), block(300., 700.)]);
        let dirty = changed(Some(&old), &new, [1024, 768]);
        assert!(covered(&dirty, 29, 34) && covered(&dirty, 211, 41) && covered(&dirty, 805, 304));
        assert!(!covered(&dirty, 303, 702));
        assert!(changed(Some(&new), &new, [1024, 768]).is_empty());
    }
    #[test]
    fn first_frame_dpi_and_disappearing_last_shape() {
        let old = scene(vec![block(1000., 760.)]);
        let mut new = scene(vec![]);
        let size = [1013, 767];
        assert_eq!(area(changed(None, &new, size)[0]), 1013 * 767);
        let dirty = changed(Some(&old), &new, size);
        assert!(covered(&dirty, 1011, 766) && !covered(&dirty, 0, 0));
        new.pixels_per_point = 1.5;
        assert_eq!(area(changed(Some(&old), &new, size)[0]), 1013 * 767);
    }
}
