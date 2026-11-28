use super::*;
use eframe::egui_glow::CallbackFn;
use egui::epaint::{ClippedShape, Shape};
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub(in crate::overlay) struct SpriteBatch {
    items: Vec<Item>,
    clip: [f32; 4],
    clock: Option<Instant>,
}

struct Item {
    id: String,
    track: dcomp::Track,
    shapes: Arc<Vec<ClippedShape>>,
    size: [i32; 2],
}

impl SpriteBatch {
    pub fn shape_count(painter: &egui::Painter) -> usize {
        painter.ctx().graphics(|layers| {
            layers
                .get(painter.layer_id())
                .map_or(0, |list| list.next_idx().0)
        })
    }

    pub fn set_canvas(&mut self, rect: egui::Rect, height: f32, scale: f32, clock: Instant) {
        self.clip = [
            rect.left() * scale,
            rect.top() * scale,
            rect.right() * scale,
            (rect.top() + height).min(rect.bottom()) * scale,
        ];
        self.clock = Some(clock);
    }

    pub fn capture(
        &mut self,
        painter: &egui::Painter,
        first: usize,
        item: &crate::danmaku::FlyingDanmaku,
        x: f32,
        y: f32,
    ) {
        use crate::danmaku::DanmakuMode;
        let shapes = painter.ctx().graphics_mut(|layers| {
            let list = layers.entry(painter.layer_id());
            let shapes: Vec<_> = list.all_entries().skip(first).cloned().collect();
            for index in first..first + shapes.len() {
                list.reset_shape(egui::layers::ShapeIdx(index));
            }
            shapes
        });
        let scale = painter.ctx().pixels_per_point();
        let bounds = shapes.iter().fold(egui::Rect::NOTHING, |bounds, cs| {
            bounds.union(cs.shape.visual_bounding_rect())
        });
        if !bounds.is_finite() || !bounds.is_positive() {
            return;
        }
        // Add a physical-pixel border for antialiasing. The compositor clips the
        // running visual against the current danmaku area instead of clipping its raster.
        let min = (bounds.min.to_vec2() * scale).floor() - egui::Vec2::splat(2.0);
        let max = (bounds.max.to_vec2() * scale).ceil() + egui::Vec2::splat(2.0);
        let size = [(max.x - min.x) as i32, (max.y - min.y) as i32];
        let sprite_clip = egui::Rect::from_min_size(egui::Pos2::ZERO, (max - min) / scale);
        let shapes = Arc::new(
            shapes
                .into_iter()
                .map(|mut cs| {
                    cs.shape.translate(-min / scale);
                    cs.clip_rect = sprite_clip;
                    cs
                })
                .collect(),
        );
        let velocity = match item.message.mode {
            DanmakuMode::Scroll => -item.speed,
            DanmakuMode::Reverse => item.speed,
            DanmakuMode::Top | DanmakuMode::Bottom => 0.0,
        };
        let track = dcomp::Track {
            x: (x + item.x(item.start)) * scale + min.x,
            y: y * scale + min.y,
            velocity: velocity * scale,
            start: item.start,
            duration: item.duration.max(0.01),
        };
        self.items.push(Item {
            id: item.message.id.clone(),
            track,
            shapes,
            size,
        });
    }
}

#[derive(Default)]
pub(super) struct Frames {
    device: Option<dcomp::SpritePresenter>,
    rasters: HashMap<String, ([i32; 2], Arc<Vec<ClippedShape>>)>,
    pixels: Vec<u8>,
    uploads: u64,
}

fn crop_readback(
    gl: &glow::Context,
    output: &mut Vec<u8>,
    height: i32,
    rect: Rect,
) -> anyhow::Result<()> {
    let width = rect.right - rect.left;
    let rows = rect.bottom - rect.top;
    let row_bytes = width as usize * 4;
    output.resize(row_bytes * rows as usize, 0);
    let gles = gl.version().is_embedded;
    unsafe {
        gl.pixel_store_i32(glow::PACK_ALIGNMENT, 4);
        if !gles || gl.version().major >= 3 {
            gl.pixel_store_i32(glow::PACK_ROW_LENGTH, 0);
        }
        gl.read_pixels(
            rect.left,
            height - rect.bottom,
            width,
            rows,
            if gles { glow::RGBA } else { glow::BGRA },
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(output)),
        );
        let error = gl.get_error();
        anyhow::ensure!(
            error == glow::NO_ERROR,
            "sprite pixel readback failed ({error:#x})"
        );
    }
    for row in 0..rows as usize / 2 {
        let opposite = rows as usize - row - 1;
        let (before, after) = output.split_at_mut(opposite * row_bytes);
        before[row * row_bytes..(row + 1) * row_bytes].swap_with_slice(&mut after[..row_bytes]);
    }
    if gles {
        for pixel in output.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
    }
    Ok(())
}

impl Presenter {
    fn disable_sprites(&mut self, ctx: &egui::Context, error: anyhow::Error) {
        self.report(Err(
            error.context("DirectComposition sprites; switching to bitmap")
        ));
        self.sprites.take();
        self.painting = false;
        self.previous_scene = None;
        self.cleared_while_hidden = false;
        invalidate_sc_frame();
        enable_bitmap_window(self.window);
        ctx.request_repaint();
    }

    fn submit_static(
        &mut self,
        gl: &glow::Context,
        scene: &Scene,
        clip: [f32; 4],
        ids: &HashSet<&str>,
    ) -> anyhow::Result<()> {
        let frames = self.sprites.as_mut().unwrap();
        if frames.device.is_none() {
            frames.device = Some(dcomp::SpritePresenter::new(self.window)?);
        }
        let device = frames.device.as_mut().unwrap();
        device.sync(ids, clip)?;
        frames.rasters.retain(|id, _| ids.contains(id.as_str()));
        let size = self.frame.as_ref().unwrap().size;
        if let Some(dirty) = damage::changed(self.previous_scene.as_deref(), scene, size)
            .into_iter()
            .reduce(damage::bounds)
        {
            crop_readback(gl, &mut frames.pixels, size[1], dirty)?;
            device.static_surface(size, dirty, &frames.pixels)?;
        }
        Ok(())
    }

    fn sprite_pass(
        &mut self,
        gl: &glow::Context,
        item: &Item,
        needs_pixels: bool,
        clock: Instant,
    ) -> anyhow::Result<()> {
        let frames = self.sprites.as_mut().unwrap();
        let device = frames.device.as_mut().unwrap();
        let pixels = if needs_pixels {
            let frame = self.frame.as_ref().unwrap();
            let area = damage::full(item.size);
            crop_readback(gl, &mut frames.pixels, frame.size[1], area)?;
            frames
                .rasters
                .insert(item.id.clone(), (item.size, item.shapes.clone()));
            frames.uploads += 1;
            Some((item.size, frames.pixels.as_slice()))
        } else {
            None
        };
        let now = clock.elapsed().as_secs_f64();
        device.sprite(&item.id, item.track, pixels, now, Instant::now())
    }
}

pub(super) fn schedule(ctx: &egui::Context, hits: InteractionState, mut batch: SpriteBatch) {
    let rect = ctx.content_rect();
    let layout = ctx.viewport_rect().size() * ctx.pixels_per_point();
    let size = [layout.x.round() as i32, layout.y.round() as i32];
    let scene = Arc::new(Scene::collect(ctx));
    let background = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Background,
        egui::Id::new("sprite-begin"),
    ));
    let debug = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Debug,
        egui::Id::new("sprite-finish"),
    ));
    let ctx_owned = ctx.clone();
    background.add(egui::PaintCallback {
        rect,
        callback: Arc::new(CallbackFn::new(move |info, painter| {
            PRESENTER.with(|slot| {
                if let Some(p) = slot.borrow_mut().as_mut() {
                    if p.sprites.is_none() {
                        return;
                    }
                    let screen = info.screen_size_px.map(|value| value as i32);
                    if screen != size || scene_scale_mismatch(&ctx_owned, info.pixels_per_point) {
                        invalidate_sc_frame();
                        p.painting = false;
                        ctx_owned.request_repaint();
                    } else if let Err(error) = p.prepare(painter.gl(), screen) {
                        p.disable_sprites(&ctx_owned, error);
                    }
                }
            });
        })),
    });
    if batch.clip[2] <= batch.clip[0] || batch.clip[3] <= batch.clip[1] {
        batch.clip = [0.0, 0.0, size[0] as f32, size[1] as f32];
    }
    // A sprite larger than the viewport needs only the corresponding visible
    // bounds. Oversized content uses the compatibility renderer for full parity.
    let oversize = batch
        .items
        .iter()
        .any(|item| item.size[0] > size[0] || item.size[1] > size[1]);
    let ids: Vec<_> = batch.items.iter().map(|item| item.id.clone()).collect();
    let scene_for_frame = scene.clone();
    let ctx_owned = ctx.clone();
    debug.add(egui::PaintCallback {
        rect,
        callback: Arc::new(CallbackFn::new(move |_, painter| {
            PRESENTER.with(|slot| {
                if let Some(p) = slot.borrow_mut().as_mut() {
                    if !p.painting || p.sprites.is_none() {
                        return;
                    }
                    let result = if oversize {
                        Err(anyhow::anyhow!(
                            "danmaku raster exceeds the overlay framebuffer"
                        ))
                    } else {
                        let ids = ids.iter().map(String::as_str).collect();
                        p.submit_static(painter.gl(), &scene_for_frame, batch.clip, &ids)
                    };
                    if let Err(error) = result {
                        p.disable_sprites(&ctx_owned, error);
                    }
                }
            });
        })),
    });
    if let Some(clock) = batch.clock.filter(|_| !oversize) {
        for item in batch.items {
            let needs_pixels = PRESENTER.with(|slot| {
                let guard = slot.borrow();
                let frames = guard.as_ref().unwrap().sprites.as_ref().unwrap();
                frames.rasters.get(&item.id).is_none_or(|(size, shapes)| {
                    *size != item.size || shapes.as_ref() != item.shapes.as_ref()
                })
            });
            if needs_pixels {
                let [width, height] = item.size;
                debug.add(egui::PaintCallback {
                    rect,
                    callback: Arc::new(CallbackFn::new(move |_, painter| {
                        PRESENTER.with(|slot| {
                            if let Some(p) = slot.borrow().as_ref().filter(|p| p.painting) {
                                let frame = p.frame.as_ref().unwrap();
                                let gl = painter.gl();
                                unsafe {
                                    gl.bind_framebuffer(glow::FRAMEBUFFER, Some(frame.fbo));
                                    gl.enable(glow::SCISSOR_TEST);
                                    gl.scissor(0, frame.size[1] - height, width, height);
                                    gl.clear_color(0.0, 0.0, 0.0, 0.0);
                                    gl.clear(glow::COLOR_BUFFER_BIT);
                                }
                            }
                        });
                    })),
                });
                debug.ctx().graphics_mut(|layers| {
                    let list = layers.entry(debug.layer_id());
                    for ClippedShape { clip_rect, shape } in item.shapes.iter().cloned() {
                        if !matches!(shape, Shape::Noop) {
                            list.add(clip_rect, shape);
                        }
                    }
                });
            }
            let ctx_owned = ctx.clone();
            debug.add(egui::PaintCallback {
                rect,
                callback: Arc::new(CallbackFn::new(move |_, painter| {
                    PRESENTER.with(|slot| {
                        if let Some(p) = slot.borrow_mut().as_mut().filter(|p| p.painting) {
                            if let Err(error) =
                                p.sprite_pass(painter.gl(), &item, needs_pixels, clock)
                            {
                                p.disable_sprites(&ctx_owned, error);
                            }
                        }
                    });
                })),
            });
        }
    }
    let ctx_owned = ctx.clone();
    debug.add(egui::PaintCallback { rect, callback: Arc::new(CallbackFn::new(move |info, painter| {
        let gl = painter.gl();
        PRESENTER.with(|slot| {
            let mut guard = slot.borrow_mut();
            let Some(p) = guard.as_mut() else { return; };
            if std::mem::take(&mut p.painting) {
                let frames = p.sprites.as_mut().unwrap();
                let result = if p.gpu_fail_after == Some(p.gpu_submitted) {
                    Err(anyhow::anyhow!("injected DirectComposition sprite submission failure"))
                } else {
                    frames.device.as_mut().unwrap().commit()
                };
                match result {
                    Ok(submitted) => {
                        if submitted {
                            LAST_MS.store(elapsed_ms(), Ordering::Relaxed);
                            SUBMITTED.fetch_add(1, Ordering::Relaxed);
                            if p.gpu_submitted == 0 || p.diagnostic && p.gpu_submitted % 30 == 0 {
                                diag_log(&format!("DirectComposition sprite commit: count={} uploads={} active={}", p.gpu_submitted + 1, frames.uploads, frames.rasters.len()));
                            }
                            p.gpu_submitted += 1;
                        }
                        if client_area(p.window).is_some_and(|area| [area.width, area.height] == size) {
                            p.previous_scene = Some(scene.clone());
                            accept_sc_frame(p.sc_generation, size, info.pixels_per_point, &hits, submitted);
                        } else {
                            p.previous_scene = None;
                            invalidate_sc_frame();
                        }
                    }
                    Err(error) => p.disable_sprites(&ctx_owned, error),
                }
            }
        });
        unsafe { gl.bind_framebuffer(glow::FRAMEBUFFER, painter.intermediate_fbo()); }
    })) });
}

fn scene_scale_mismatch(ctx: &egui::Context, actual: f32) -> bool {
    ctx.pixels_per_point() != actual
}
