#![cfg(feature = "gui")]
//! Headless widget gallery: real epaint output rasterised to PNG, no window.
//!
//! This is evidence, not a real-window look acceptance. It runs a normal egui
//! frame (same `theme` / `widgets` code the launcher uses), tessellates the paint
//! commands and rasterises them with a small CPU triangle rasteriser, so the PNG
//! shows the actual shapes and the actual system font atlas — no desktop capture,
//! no window, no network, no user data.
//!
//! Set `LIVE_HUB_WIDGET_GALLERY_DIR` to an existing directory to write
//! `gallery-dark.png` / `gallery-light.png` (2x physical pixels, same scale as the
//! original 2480x1440 reference screenshot). Without the variable the test only
//! renders the frames and asserts they contain paint commands.

use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};
use live_hub_rust::gui::theme::{self, Palette, metrics, space, ts};
use live_hub_rust::gui::widgets::{self, BadgeKind, ButtonKind, Card, DotKind, Glyph};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Logical canvas size in points (1240 = the original launcher width).
const CANVAS_W: f32 = 1240.0;
/// Logical canvas height in points.
const CANVAS_H: f32 = 880.0;
/// Physical pixels per logical point: 2x, matching the 200% DPI reference.
const SCALE: f32 = 2.0;

// ============================================================================================
// CPU rasteriser for epaint output (rects, meshes, font atlas, AA text)
// ============================================================================================

/// One uploaded texture (in practice only the font atlas).
#[derive(Clone)]
struct Texture {
    width: usize,
    height: usize,
    pixels: Vec<Color32>,
}

/// Apply one texture delta to the local texture map.
fn apply_delta(
    textures: &mut HashMap<egui::TextureId, Texture>,
    id: egui::TextureId,
    delta: &egui::epaint::ImageDelta,
) {
    let egui::epaint::ImageData::Color(image) = &delta.image;
    let width = image.size[0];
    let height = image.size[1];
    match delta.pos {
        None => {
            textures.insert(
                id,
                Texture {
                    width,
                    height,
                    pixels: image.pixels.clone(),
                },
            );
        }
        Some([x0, y0]) => {
            if let Some(target) = textures.get_mut(&id) {
                for y in 0..height {
                    for x in 0..width {
                        let tx = x0 + x;
                        let ty = y0 + y;
                        if tx < target.width && ty < target.height {
                            target.pixels[ty * target.width + tx] = image.pixels[y * width + x];
                        }
                    }
                }
            }
        }
    }
}

/// Bilinear sample of a premultiplied sRGBA texture, returning premultiplied f32.
fn sample(tex: &Texture, u: f32, v: f32) -> [f32; 4] {
    let fx = (u * tex.width as f32 - 0.5).clamp(0.0, (tex.width - 1) as f32);
    let fy = (v * tex.height as f32 - 0.5).clamp(0.0, (tex.height - 1) as f32);
    let x0 = fx.floor() as usize;
    let y0 = fy.floor() as usize;
    let x1 = (x0 + 1).min(tex.width - 1);
    let y1 = (y0 + 1).min(tex.height - 1);
    let tx = fx - x0 as f32;
    let ty = fy - y0 as f32;
    let load = |x: usize, y: usize| -> [f32; 4] {
        let pixel = tex.pixels[y * tex.width + x];
        [
            pixel.r() as f32 / 255.0,
            pixel.g() as f32 / 255.0,
            pixel.b() as f32 / 255.0,
            pixel.a() as f32 / 255.0,
        ]
    };
    let p00 = load(x0, y0);
    let p10 = load(x1, y0);
    let p01 = load(x0, y1);
    let p11 = load(x1, y1);
    let mut out = [0.0_f32; 4];
    for channel in 0..4 {
        let top = p00[channel] * (1.0 - tx) + p10[channel] * tx;
        let bottom = p01[channel] * (1.0 - tx) + p11[channel] * tx;
        out[channel] = top * (1.0 - ty) + bottom * ty;
    }
    out
}

/// Signed area helper for barycentric coordinates.
fn edge(a: Pos2, b: Pos2, c: Pos2) -> f32 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

/// Composite one premultiplied source colour over the canvas pixel.
fn blend_pixel(canvas: &mut image::RgbaImage, x: i32, y: i32, src: [f32; 4]) {
    if x < 0 || y < 0 {
        return;
    }
    let (x, y) = (x as u32, y as u32);
    if x >= canvas.width() || y >= canvas.height() {
        return;
    }
    let alpha = src[3].clamp(0.0, 1.0);
    if alpha <= 0.0005 {
        return;
    }
    let dst = canvas.get_pixel_mut(x, y);
    let out = [
        src[0] + dst[0] as f32 / 255.0 * (1.0 - alpha),
        src[1] + dst[1] as f32 / 255.0 * (1.0 - alpha),
        src[2] + dst[2] as f32 / 255.0 * (1.0 - alpha),
        dst[3] as f32 / 255.0,
    ];
    dst[0] = (out[0].clamp(0.0, 1.0) * 255.0).round() as u8;
    dst[1] = (out[1].clamp(0.0, 1.0) * 255.0).round() as u8;
    dst[2] = (out[2].clamp(0.0, 1.0) * 255.0).round() as u8;
    dst[3] = (out[3].clamp(0.0, 1.0) * 255.0).round() as u8;
}

/// Rasterise one tessellated mesh, honouring the clip rectangle.
fn rasterize_mesh(
    canvas: &mut image::RgbaImage,
    mesh: &egui::Mesh,
    textures: &HashMap<egui::TextureId, Texture>,
    clip: Rect,
) {
    if mesh.indices.len() < 3 {
        return;
    }
    let texture = textures.get(&mesh.texture_id);
    let clip_min = Pos2::new(clip.min.x * SCALE, clip.min.y * SCALE);
    let clip_max = Pos2::new(clip.max.x * SCALE, clip.max.y * SCALE);
    let to_px = |p: Pos2| Pos2::new(p.x * SCALE, p.y * SCALE);
    for triangle in mesh.indices.chunks_exact(3) {
        let v0 = &mesh.vertices[triangle[0] as usize];
        let v1 = &mesh.vertices[triangle[1] as usize];
        let v2 = &mesh.vertices[triangle[2] as usize];
        let p0 = to_px(v0.pos);
        let p1 = to_px(v1.pos);
        let p2 = to_px(v2.pos);
        let area = edge(p0, p1, p2);
        if area.abs() < 1e-9 {
            continue;
        }
        let min_x = p0.x.min(p1.x).min(p2.x).floor().max(clip_min.x).max(0.0) as i32;
        let max_x =
            p0.x.max(p1.x)
                .max(p2.x)
                .ceil()
                .min(clip_max.x)
                .min(canvas.width() as f32 - 1.0) as i32;
        let min_y = p0.y.min(p1.y).min(p2.y).floor().max(clip_min.y).max(0.0) as i32;
        let max_y =
            p0.y.max(p1.y)
                .max(p2.y)
                .ceil()
                .min(clip_max.y)
                .min(canvas.height() as f32 - 1.0) as i32;
        for y in min_y..=max_y {
            for x in min_x..=max_x {
                let p = Pos2::new(x as f32 + 0.5, y as f32 + 0.5);
                let w0 = edge(p1, p2, p) / area;
                let w1 = edge(p2, p0, p) / area;
                let w2 = edge(p0, p1, p) / area;
                if w0 < -0.0001 || w1 < -0.0001 || w2 < -0.0001 {
                    continue;
                }
                let uv = egui::pos2(
                    v0.uv.x * w0 + v1.uv.x * w1 + v2.uv.x * w2,
                    v0.uv.y * w0 + v1.uv.y * w1 + v2.uv.y * w2,
                );
                let color = |channel: fn(Color32) -> u8| -> f32 {
                    (channel(v0.color) as f32 * w0
                        + channel(v1.color) as f32 * w1
                        + channel(v2.color) as f32 * w2)
                        / 255.0
                };
                let mut src = [
                    color(|c| c.r()),
                    color(|c| c.g()),
                    color(|c| c.b()),
                    color(|c| c.a()),
                ];
                if let Some(tex) = texture {
                    let texel = sample(tex, uv.x, uv.y);
                    for channel in 0..4 {
                        src[channel] *= texel[channel];
                    }
                }
                blend_pixel(canvas, x, y, src);
            }
        }
    }
}

fn gallery_dir() -> Option<PathBuf> {
    std::env::var_os("LIVE_HUB_WIDGET_GALLERY_DIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

/// Render one gallery scene headless and return the rasterised PNG.
fn render_scene(dark: bool) -> image::RgbaImage {
    let context = egui::Context::default();
    context.set_pixels_per_point(SCALE);
    let report = theme::install_fonts(&context);
    theme::apply(&context, dark);
    let palette = Palette::for_mode(dark);
    let mut textures: HashMap<egui::TextureId, Texture> = HashMap::new();
    let mut last = None;
    // Two frames: the first builds the font atlas, the second is the settled paint.
    for _ in 0..2 {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                Pos2::ZERO,
                Vec2::new(CANVAS_W, CANVAS_H),
            )),
            ..Default::default()
        };
        let output = context.run(input, |context| draw_gallery(context, &palette, &report));
        for (id, delta) in output.textures_delta.set.iter() {
            apply_delta(&mut textures, *id, delta);
        }
        last = Some(output);
    }
    let output = last.expect("frame output");
    let primitives = context.tessellate(output.shapes, SCALE);
    let mut canvas = image::RgbaImage::from_pixel(
        (CANVAS_W * SCALE) as u32,
        (CANVAS_H * SCALE) as u32,
        image::Rgba([
            palette.surface.r(),
            palette.surface.g(),
            palette.surface.b(),
            255,
        ]),
    );
    for primitive in primitives {
        if let egui::epaint::Primitive::Mesh(mesh) = primitive.primitive {
            rasterize_mesh(&mut canvas, &mesh, &textures, primitive.clip_rect);
        }
    }
    canvas
}

fn save(canvas: &image::RgbaImage, directory: &Path, name: &str) -> PathBuf {
    std::fs::create_dir_all(directory).expect("gallery directory");
    let path = directory.join(name);
    canvas.save(&path).expect("save gallery png");
    eprintln!("widget gallery: {}", path.display());
    path
}

// ============================================================================================
// Gallery scene
// ============================================================================================

/// Paint one line of text with a custom font, left aligned + vertically centred.
fn text_left(painter: &egui::Painter, rect: Rect, text: &str, font: egui::FontId, color: Color32) {
    let galley = painter.layout_no_wrap(text.to_owned(), font, Color32::PLACEHOLDER);
    let pos = Pos2::new(rect.left(), rect.center().y - galley.size().y / 2.0);
    painter.galley(pos, galley, color);
}

/// Paint one line of text centred in a rectangle.
fn text_center(
    painter: &egui::Painter,
    rect: Rect,
    text: &str,
    font: egui::FontId,
    color: Color32,
) {
    let galley = painter.layout_no_wrap(text.to_owned(), font, Color32::PLACEHOLDER);
    let pos = Pos2::new(
        rect.center().x - galley.size().x / 2.0,
        rect.center().y - galley.size().y / 2.0,
    );
    painter.galley(pos, galley, color);
}

/// Original card action button ("▷ mpv" / "PotPlayer" / "重新打开").
fn small_button(painter: &egui::Painter, rect: Rect, text: &str, palette: &Palette) {
    small_button_icon(painter, rect, None, text, palette);
}

/// Card action button with an optional leading vector glyph (the play triangle).
fn small_button_icon(
    painter: &egui::Painter,
    rect: Rect,
    glyph: Option<Glyph>,
    text: &str,
    palette: &Palette,
) {
    let radius = CornerRadius::same(6);
    painter.rect_filled(rect, radius, palette.card);
    painter.rect_stroke(
        rect,
        radius,
        Stroke::new(1.0_f32, palette.border),
        StrokeKind::Inside,
    );
    let galley =
        painter.layout_no_wrap(text.to_owned(), theme::font(ts::PILL), Color32::PLACEHOLDER);
    let icon_space = if glyph.is_some() { 10.0 + 5.0 } else { 0.0 };
    let content = galley.size().x + icon_space;
    let left = rect.center().x - content / 2.0;
    if let Some(glyph) = glyph {
        widgets::glyph(
            painter,
            Rect::from_center_size(Pos2::new(left + 5.0, rect.center().y), Vec2::splat(10.0)),
            glyph,
            palette.text,
            1.1,
        );
    }
    painter.galley(
        Pos2::new(left + icon_space, rect.center().y - galley.size().y / 2.0),
        galley,
        palette.text,
    );
}

fn chrome_bar(ui: &mut Ui, palette: &Palette) {
    let chrome = Rect::from_min_size(ui.min_rect().min, Vec2::new(CANVAS_W, metrics::CHROME_H));
    ui.painter().rect_filled(chrome, 0.0, palette.side);
    ui.spacing_mut().item_spacing = Vec2::ZERO;
    ui.horizontal_centered(|ui| {
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        ui.add_space(metrics::CHROME_ICON_LEFT);
        let (icon, _) = ui.allocate_exact_size(Vec2::splat(metrics::CHROME_ICON), Sense::hover());
        ui.painter()
            .rect_filled(icon, CornerRadius::same(5), palette.accent);
        widgets::glyph(
            ui.painter(),
            icon.shrink(2.5),
            Glyph::MonitorPlay,
            Color32::WHITE,
            1.0,
        );
        ui.add_space(space::SM);
        text_left(
            ui.painter(),
            Rect::from_min_size(
                Pos2::new(ui.cursor().min.x, chrome.top()),
                Vec2::new(80.0, metrics::CHROME_H),
            ),
            "看直播",
            theme::font(ts::CHROME),
            palette.text_secondary,
        );
        ui.add_space(80.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            ui.add_space(metrics::CHROME_PAD);
            widgets::traffic_dot(ui, DotKind::Close, false);
            ui.add_space(metrics::DOT_GAP);
            widgets::traffic_dot(ui, DotKind::Max, false);
            ui.add_space(metrics::DOT_GAP);
            widgets::traffic_dot(ui, DotKind::Min, false);
            ui.add_space(space::SM);
            let (hint, _) =
                ui.allocate_exact_size(Vec2::new(160.0, metrics::CHROME_H), Sense::hover());
            text_left(
                ui.painter(),
                hint,
                "本地播放器 · 四平台",
                theme::font(ts::CHROME),
                palette.text_secondary,
            );
        });
    });
    theme::edge_line(
        ui.painter(),
        Rect::from_min_size(
            Pos2::new(0.0, metrics::CHROME_H - 1.0),
            Vec2::new(CANVAS_W, 1.0),
        ),
        theme::Edge::Bottom,
        palette.border,
    );
}

fn sidebar(ui: &mut Ui, palette: &Palette) {
    let width = metrics::SIDEBAR_W;
    let base = ui.min_rect().min;
    ui.painter().rect_filled(
        Rect::from_min_size(base, Vec2::new(width, CANVAS_H)),
        0.0,
        palette.side,
    );
    ui.spacing_mut().item_spacing = Vec2::ZERO;
    ui.add_space(metrics::BRAND_TOP);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        ui.add_space(metrics::BRAND_LEFT);
        let (mark, _) = ui.allocate_exact_size(Vec2::splat(metrics::BRAND_MARK), Sense::hover());
        // WorkbenchBrandMark: accent 圆角方块 + 白色显示器 glyph。
        let radius = CornerRadius::same(9);
        ui.painter().rect_filled(mark, radius, palette.accent);
        let pad = 5.0;
        widgets::glyph(
            ui.painter(),
            mark.shrink(pad),
            Glyph::MonitorPlay,
            Color32::WHITE,
            1.6,
        );
        widgets::text_at(
            ui,
            "看直播",
            theme::font_bold(metrics::BRAND_FONT),
            palette.text,
            Vec2::new(width - 50.0, metrics::BRAND_MARK),
            space::SM,
        );
    });
    ui.add_space(24.0);
    widgets::nav_group(ui, "观看");
    ui.add_space(space::SM);
    let items = [
        (Glyph::Favorites, "我的收藏", true),
        (Glyph::History, "最近播放", false),
        (Glyph::Search, "查找主播", false),
        (Glyph::Sessions, "观看会话", false),
    ];
    for (glyph, label, selected) in items {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            ui.add_space(metrics::NAV_INSET);
            widgets::nav_button_size(
                ui,
                glyph,
                label,
                selected,
                Vec2::new(width - metrics::NAV_INSET * 2.0, metrics::NAV_H),
            );
        });
        ui.add_space(metrics::NAV_GAP);
    }
    ui.add_space(6.0);
    widgets::nav_group(ui, "设置与账号");
    ui.add_space(3.0);
    let items = [
        (Glyph::Players, "播放器与画质", metrics::NAV_H),
        (Glyph::Danmaku, "弹幕与 SC 设置", metrics::NAV_H),
        (Glyph::Accounts, "账号与登录", metrics::NAV_H),
        (Glyph::Data, "本地数据 / 平台状态", metrics::NAV_H_TALL),
    ];
    for (glyph, label, height) in items {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            ui.add_space(metrics::NAV_INSET);
            widgets::nav_button_size(
                ui,
                glyph,
                label,
                false,
                Vec2::new(width - metrics::NAV_INSET * 2.0, height),
            );
        });
        ui.add_space(metrics::NAV_GAP);
    }
    // Footer: bottom aligned (原版 footerTop = shellHeight - 56)。
    let footer_space = (ui.available_height() - 40.0).max(0.0);
    ui.add_space(footer_space);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        ui.add_space(12.0);
        widgets::glyph_widget(ui, Glyph::Drive, 16.0, palette.text_secondary);
        ui.add_space(6.0);
        widgets::hint(ui, "本地播放器 · 四平台");
    });
    ui.add_space(14.0);
    theme::edge_line(
        ui.painter(),
        Rect::from_min_size(Pos2::new(width - 1.0, base.y), Vec2::new(1.0, CANVAS_H)),
        theme::Edge::Right,
        palette.border,
    );
}

fn room_card(
    ui: &mut Ui,
    palette: &Palette,
    accent: Color32,
    name: &str,
    meta: &str,
    live: bool,
    selected: bool,
) {
    let size = Vec2::new(234.0, metrics::CARD_H);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let painter = ui.painter();
    let card = Rect::from_min_size(rect.min, size - Vec2::splat(1.0));
    let radius = CornerRadius::same(metrics::CARD_RADIUS);
    let fill = if selected {
        palette.selection_soft
    } else {
        palette.card
    };
    painter.rect_filled(card, radius, fill);
    let border = if selected {
        palette.selection_border
    } else if response.hovered() {
        palette.border_hover
    } else {
        palette.border
    };
    painter.rect_stroke(
        card,
        radius,
        Stroke::new(if selected { 2.0_f32 } else { 1.0_f32 }, border),
        StrokeKind::Inside,
    );
    // Cover gradient (原版 LinearGradientBrush，22°≈竖直，圆角裁切)。
    let cover = Rect::from_min_size(
        card.min + Vec2::new(1.0, 1.0),
        Vec2::new(card.width() - 2.0, metrics::CARD_COVER),
    );
    let top = theme::blend(accent, fill, metrics::COVER_TOP_BLEND);
    let bottom = theme::blend(accent, fill, metrics::COVER_BOTTOM_BLEND);
    theme::gradient_round_rect(
        painter,
        cover,
        CornerRadius {
            nw: metrics::CARD_RADIUS,
            ne: metrics::CARD_RADIUS,
            sw: 0,
            se: 0,
        },
        top,
        bottom,
    );
    widgets::avatar(
        painter,
        Rect::from_min_size(
            card.min + Vec2::new(metrics::CARD_AVATAR_PAD, metrics::CARD_AVATAR_PAD),
            Vec2::splat(metrics::CARD_AVATAR),
        ),
        None,
        name,
        palette,
    );
    // State pill (top right, 22 high).
    let state_text = if live { "直播中" } else { "已下播" };
    let state_color = if live {
        palette.success
    } else {
        palette.text_secondary
    };
    let state_pill = Rect::from_min_size(
        Pos2::new(card.right() - 9.0 - 52.0, card.top() + 9.0),
        Vec2::new(52.0, metrics::STATE_PILL_H),
    );
    painter.rect_filled(
        state_pill,
        CornerRadius::same(metrics::STATE_PILL_RADIUS),
        if selected {
            palette.selection_soft
        } else {
            palette.card
        },
    );
    painter.circle_filled(
        Pos2::new(state_pill.left() + 10.5, state_pill.center().y),
        2.5,
        state_color,
    );
    text_left(
        painter,
        Rect::from_min_max(
            Pos2::new(state_pill.left() + 18.0, state_pill.top()),
            state_pill.max,
        ),
        state_text,
        theme::font(ts::PILL),
        state_color,
    );
    // Select pill (52x24), below the state pill.
    let choose = Rect::from_min_size(
        Pos2::new(
            card.right() - metrics::CARD_INNER - metrics::PILL_W,
            card.top() + metrics::CARD_COVER - 6.0 - metrics::PILL_H,
        ),
        Vec2::new(metrics::PILL_W, metrics::PILL_H),
    );
    painter.rect_filled(
        choose,
        CornerRadius::same(metrics::PILL_RADIUS),
        if selected {
            palette.selection
        } else {
            palette.card
        },
    );
    painter.rect_stroke(
        choose,
        CornerRadius::same(metrics::PILL_RADIUS),
        Stroke::new(
            1.0_f32,
            if selected {
                palette.selection_border
            } else {
                palette.border
            },
        ),
        StrokeKind::Inside,
    );
    text_center(
        painter,
        choose,
        "选择",
        theme::font(ts::PILL),
        if selected {
            palette.selection_text
        } else {
            palette.text
        },
    );
    if selected {
        let badge = Rect::from_min_size(card.min + Vec2::new(40.0, 42.0), Vec2::splat(14.0));
        painter.circle_filled(badge.center(), 7.0, palette.selection);
        // 原版用 "✓" 字符（GDI+ 会自动做字体链接）；egui 没有字体链接，画矢量对勾。
        widgets::glyph(
            painter,
            badge.shrink(2.0),
            Glyph::Check,
            palette.selection_text,
            1.8,
        );
    }
    // Body text.
    let body_top = card.top() + metrics::CARD_COVER;
    text_left(
        painter,
        Rect::from_min_size(
            Pos2::new(card.left() + metrics::CARD_INNER, body_top + 8.0),
            Vec2::new(card.width() - metrics::CARD_INNER * 2.0, 20.0),
        ),
        name,
        theme::font_bold(ts::CARD_TITLE),
        palette.text,
    );
    text_left(
        painter,
        Rect::from_min_size(
            Pos2::new(card.left() + metrics::CARD_INNER, body_top + 29.0),
            Vec2::new(card.width() - metrics::CARD_INNER * 2.0, 17.0),
        ),
        meta,
        theme::font(ts::PILL),
        palette.text_secondary,
    );
    // Bottom action buttons (30 high, radius 6).
    let button_width = (card.width() - metrics::CARD_INNER * 2.0 - 8.0) / 2.0;
    let top_left = card.bottom() - metrics::CARD_INNER - metrics::CARD_BTN_H;
    let mpv = Rect::from_min_size(
        Pos2::new(card.left() + metrics::CARD_INNER, top_left),
        Vec2::new(button_width, metrics::CARD_BTN_H),
    );
    let pot = Rect::from_min_size(
        Pos2::new(mpv.right() + 8.0, top_left),
        Vec2::new(button_width, metrics::CARD_BTN_H),
    );
    small_button_icon(painter, mpv, Some(Glyph::Play), "mpv", palette);
    small_button(painter, pot, "PotPlayer", palette);
}

fn session_card(ui: &mut Ui, palette: &Palette, name: &str, meta: &str, state: &str) {
    let width = 280.0;
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(width, metrics::SESSION_H - metrics::SESSION_GAP),
        Sense::hover(),
    );
    let painter = ui.painter();
    let card = Rect::from_min_size(rect.min, rect.size() - Vec2::splat(1.0));
    painter.rect_filled(
        card,
        CornerRadius::same(metrics::SESSION_RADIUS),
        palette.card,
    );
    painter.rect_stroke(
        card,
        CornerRadius::same(metrics::SESSION_RADIUS),
        Stroke::new(1.0_f32, palette.border),
        StrokeKind::Inside,
    );
    widgets::avatar(
        painter,
        Rect::from_min_size(
            card.min + Vec2::new(metrics::SESSION_PAD, 8.0),
            Vec2::splat(metrics::SESSION_AVATAR),
        ),
        None,
        name,
        palette,
    );
    text_left(
        painter,
        Rect::from_min_size(
            Pos2::new(card.left() + 54.0, card.top() + 7.0),
            Vec2::new(216.0, 19.0),
        ),
        name,
        theme::font_bold(ts::CARD_TITLE),
        palette.text,
    );
    text_left(
        painter,
        Rect::from_min_size(
            Pos2::new(card.left() + 54.0, card.top() + 26.0),
            Vec2::new(216.0, 17.0),
        ),
        meta,
        theme::font(ts::PILL),
        palette.text_secondary,
    );
    painter.circle_filled(
        Pos2::new(card.left() + 14.5, card.top() + 55.0),
        2.5,
        palette.text_secondary,
    );
    text_left(
        painter,
        Rect::from_min_size(
            Pos2::new(card.left() + 22.0, card.top() + 46.0),
            Vec2::new(160.0, 18.0),
        ),
        state,
        theme::font(ts::PILL),
        palette.text_secondary,
    );
    let reopen = Rect::from_min_size(
        Pos2::new(
            card.right() - metrics::SESSION_PAD - 84.0,
            card.top() + 62.0,
        ),
        Vec2::new(84.0, 24.0),
    );
    small_button(painter, reopen, "重新打开", palette);
}

fn home_band(ui: &mut Ui, palette: &Palette) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(280.0, 20.0), Sense::hover());
    {
        let painter = ui.painter();
        widgets::glyph(
            painter,
            Rect::from_min_size(rect.min + Vec2::new(0.0, 3.0), Vec2::splat(16.0)),
            Glyph::MonitorPlay,
            palette.text_secondary,
            1.25,
        );
        text_left(
            painter,
            Rect::from_min_size(rect.min + Vec2::new(22.0, 0.0), Vec2::new(170.0, 20.0)),
            "正在观看 / 最近打开",
            theme::font_bold(ts::HEADING),
            palette.text,
        );
        text_left(
            painter,
            Rect::from_min_size(rect.min + Vec2::new(210.0, 2.0), Vec2::new(70.0, 18.0)),
            "全部会话 →",
            theme::font(ts::BODY),
            palette.accent,
        );
    }
    ui.add_space(5.0);
    session_card(
        ui,
        palette,
        "B站 7777",
        "PotPlayer · 原画 · 播放器默认",
        "播放器已退出",
    );
    session_card(
        ui,
        palette,
        "银色虞姬",
        "抖音 · 6753235366",
        "最近 · 10-30 18:30",
    );
}

fn showcase(ui: &mut Ui, palette: &Palette, report: &theme::FontReport) {
    widgets::divider(ui);
    widgets::section_header(ui, "控件画廊 · widgets");
    ui.add_space(space::SM);
    ui.spacing_mut().item_spacing = Vec2::new(space::SM, 0.0);
    ui.horizontal(|ui| {
        widgets::button(ui, "主按钮", ButtonKind::Primary);
        widgets::button(ui, "次按钮", ButtonKind::Secondary);
        widgets::button(ui, "描边按钮", ButtonKind::Outline);
        widgets::button(ui, "格式说明", ButtonKind::Subtle);
        widgets::button(ui, "全部会话 →", ButtonKind::Link);
        widgets::button_enabled(ui, "禁用按钮", ButtonKind::Secondary, false);
        widgets::button_enabled(ui, "禁用主按钮", ButtonKind::Primary, false);
    });
    ui.add_space(space::SM);
    ui.horizontal(|ui| {
        let mut selected = 1usize;
        widgets::platform_tabs(ui, &["B站", "虎牙", "斗鱼", "抖音"], &mut selected);
        ui.add_space(space::LG);
        widgets::badge(ui, "直播中", BadgeKind::Live);
        widgets::badge(ui, "已下播", BadgeKind::Offline);
        widgets::badge(ui, "警告", BadgeKind::Warning);
        widgets::badge(ui, "错误", BadgeKind::Danger);
        widgets::platform_badge(ui, live_hub_rust::core::Platform::Bili);
        widgets::platform_badge(ui, live_hub_rust::core::Platform::Douyu);
    });
    ui.add_space(space::SM);
    ui.horizontal(|ui| {
        let mut room = String::from("7777");
        widgets::input_with_width(ui, &mut room, "房间号 / 直播间链接", 300.0);
        let mut error_text = String::from("房间号无效");
        widgets::TextInput::new(&mut error_text)
            .width(200.0)
            .error(true)
            .show(ui);
        let mut search = String::new();
        widgets::TextInput::new(&mut search)
            .width(240.0)
            .hint("搜索主播")
            .icon(Glyph::Search)
            .show(ui);
        let mut quality = 0usize;
        widgets::dropdown(
            ui,
            "gallery-quality",
            &mut quality,
            &["原画 / 最佳", "高清", "流畅"],
            160.0,
        );
        let mut checked = true;
        widgets::checkbox(ui, "启动时恢复上次页面", &mut checked);
    });
    ui.add_space(space::SM);
    ui.horizontal(|ui| {
        widgets::progress(ui, 0.42, 220.0);
        ui.add_space(space::MD);
        widgets::spinner(ui, 20.0);
        ui.add_space(space::MD);
        widgets::traffic_dot(ui, DotKind::Min, false);
        widgets::traffic_dot(ui, DotKind::Max, true);
        widgets::traffic_dot(ui, DotKind::Close, false);
        ui.add_space(space::MD);
        for glyph in [
            Glyph::Favorites,
            Glyph::History,
            Glyph::Search,
            Glyph::Sessions,
            Glyph::Players,
            Glyph::Danmaku,
            Glyph::Accounts,
            Glyph::Data,
            Glyph::MonitorPlay,
            Glyph::Drive,
        ] {
            widgets::glyph_widget(ui, glyph, 20.0, palette.text_secondary);
        }
    });
    ui.add_space(space::SM);
    ui.horizontal(|ui| {
        // 滚动条 skin 证据：内容溢出时露出的细滑块（槽=surface、滑块=#424650）。
        ui.allocate_ui_with_layout(
            Vec2::new(280.0, 64.0),
            egui::Layout::top_down(egui::Align::LEFT),
            |ui| {
                widgets::scroll_vertical(ui, "gallery-scroll", |ui| {
                    for index in 0..12 {
                        widgets::hint(ui, format!("滚动条示例行 {index}").as_str());
                        ui.add_space(2.0);
                    }
                });
            },
        );
        ui.add_space(space::MD);
        widgets::empty_state(ui, "还没有收藏", "输入房间号后可加入收藏。");
    });
    widgets::divider(ui);
    widgets::caption(ui, &report.describe());
}

fn draw_gallery(context: &egui::Context, palette: &Palette, report: &theme::FontReport) {
    egui::TopBottomPanel::top("gallery-chrome")
        .exact_height(metrics::CHROME_H)
        .frame(egui::Frame::new().fill(palette.side))
        .show(context, |ui| chrome_bar(ui, palette));
    egui::SidePanel::left("gallery-sidebar")
        .exact_width(metrics::SIDEBAR_W)
        .resizable(false)
        .frame(egui::Frame::new().fill(palette.side))
        .show(context, |ui| sidebar(ui, palette));
    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(palette.surface)
                .inner_margin(egui::Margin {
                    left: metrics::PAD as i8,
                    right: metrics::PAD as i8,
                    top: space::SM as i8,
                    bottom: space::SM as i8,
                }),
        )
        .show(context, |ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            widgets::page_header(ui, "我的收藏", "喜欢的主播，打开就能看。");
            ui.add_space(10.0);
            Card::new().padding(metrics::LAUNCH_PAD).show(ui, |ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::ZERO;
                    let mut selected = 0usize;
                    widgets::platform_tabs(ui, &["B站", "虎牙", "斗鱼", "抖音"], &mut selected);
                    ui.add_space(10.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        widgets::button_sized_font(
                            ui,
                            "格式说明",
                            ButtonKind::Subtle,
                            Vec2::new(88.0, metrics::TAB_H),
                            true,
                            theme::font(ts::SMALL),
                        );
                    });
                });
                ui.add_space(space::SM);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::ZERO;
                    let mut room = String::new();
                    widgets::TextInput::new(&mut room)
                        .hint("输入房间号，或粘贴官方直播间链接")
                        .width(585.0)
                        .height(metrics::INPUT_H_LG)
                        .show(ui);
                    ui.add_space(metrics::LAUNCH_GAP);
                    widgets::button_sized_font(
                        ui,
                        "用 mpv 打开（回车）",
                        ButtonKind::Primary,
                        Vec2::new(155.0, metrics::INPUT_H_LG),
                        true,
                        theme::font(ts::CARD_TITLE),
                    );
                    ui.add_space(metrics::LAUNCH_GAP);
                    widgets::button_sized_font(
                        ui,
                        "用 PotPlayer 打开",
                        ButtonKind::Secondary,
                        Vec2::new(160.0, metrics::INPUT_H_LG),
                        true,
                        theme::font(ts::CARD_TITLE),
                    );
                    ui.add_space(metrics::LAUNCH_GAP);
                    widgets::button_sized_font(
                        ui,
                        "取消启动",
                        ButtonKind::Secondary,
                        Vec2::new(72.0, metrics::INPUT_H_LG),
                        false,
                        theme::font(ts::CARD_TITLE),
                    );
                });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::ZERO;
                    let fields: [(&str, &str, [&str; 3]); 5] = [
                        ("画质：", "本次", ["原画优先", "高清优先", "流畅优先"]),
                        ("mpv 缓冲：", "本次", ["延迟更低", "画质优先", "跟随设置"]),
                        ("弹幕：", "本次", ["跟随设置", "显示", "隐藏"]),
                        ("SC：", "保存 / 实时", ["关闭", "完整卡片", "紧凑卡片"]),
                        ("画质增强：", "保存 / mpv", ["无", "锐化", "降噪"]),
                    ];
                    for (index, (label, scope, options)) in fields.iter().enumerate() {
                        if index > 0 {
                            ui.add_space(metrics::LAUNCH_COL_GAP);
                        }
                        ui.allocate_ui_with_layout(
                            Vec2::new(
                                191.0,
                                metrics::LAUNCH_LABEL_H
                                    + metrics::LAUNCH_LABEL_GAP
                                    + metrics::DROPDOWN_H,
                            ),
                            egui::Layout::top_down(egui::Align::LEFT),
                            |ui| {
                                ui.spacing_mut().item_spacing = Vec2::ZERO;
                                widgets::field_label(ui, label, Some(scope));
                                ui.add_space(metrics::LAUNCH_LABEL_GAP);
                                let mut selected = 0usize;
                                widgets::dropdown(
                                    ui,
                                    ("gallery-field", index),
                                    &mut selected,
                                    options,
                                    191.0,
                                );
                            },
                        );
                    }
                });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::ZERO;
                    widgets::hint(
                        ui,
                        "画质、缓冲、弹幕用于本次播放；SC 与画质增强保存为偏好。",
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        widgets::button_sized_font(
                            ui,
                            "弹幕设置…",
                            ButtonKind::Secondary,
                            Vec2::new(104.0, metrics::BTN_H_XS),
                            true,
                            theme::font(ts::SMALL),
                        );
                    });
                });
            });
            ui.add_space(space::MD);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                widgets::section_header(ui, "B站 收藏 6 个 · 本地收藏");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing = Vec2::ZERO;
                    widgets::button_sized_font(
                        ui,
                        "删除收藏",
                        ButtonKind::Secondary,
                        Vec2::new(104.0, metrics::BTN_H_SM),
                        true,
                        theme::font(ts::BODY),
                    );
                    ui.add_space(space::SM);
                    widgets::button_sized_font(
                        ui,
                        "加入收藏",
                        ButtonKind::Secondary,
                        Vec2::new(104.0, metrics::BTN_H_SM),
                        true,
                        theme::font(ts::BODY),
                    );
                });
            });
            ui.add_space(6.0);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                ui.allocate_ui_with_layout(
                    Vec2::new(732.0, metrics::CARD_H + 30.0),
                    egui::Layout::top_down(egui::Align::LEFT),
                    |ui| {
                        ui.spacing_mut().item_spacing = Vec2::ZERO;
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing = Vec2::ZERO;
                            room_card(
                                ui,
                                palette,
                                palette.bilibili,
                                "LPL赛事",
                                "B站 · 6",
                                true,
                                true,
                            );
                            ui.add_space(metrics::CARD_GAP);
                            room_card(
                                ui,
                                palette,
                                palette.bilibili,
                                "LPL赛事副直播间",
                                "B站 · 616",
                                true,
                                false,
                            );
                            ui.add_space(metrics::CARD_GAP);
                            room_card(
                                ui,
                                palette,
                                palette.bilibili,
                                "德云色",
                                "B站 · 7777",
                                true,
                                false,
                            );
                        });
                        ui.add_space(4.0);
                        widgets::hint(
                            ui,
                            "选择卡片回填房间；双击或回车用 mpv，Shift+回车用 PotPlayer。",
                        );
                    },
                );
                ui.add_space(space::LG);
                ui.allocate_ui_with_layout(
                    Vec2::new(280.0, metrics::CARD_H + 30.0),
                    egui::Layout::top_down(egui::Align::LEFT),
                    |ui| {
                        ui.spacing_mut().item_spacing = Vec2::ZERO;
                        home_band(ui, palette);
                    },
                );
            });
            ui.add_space(space::SM);
            showcase(ui, palette, report);
        });
}

// ============================================================================================
// Tests
// ============================================================================================

/// 打印字体加载证据（只看文件名，不输出本机绝对路径）。
fn print_font_report(report: &theme::FontReport) {
    eprintln!("[font] {}", report.describe());
    let face = |label: &str, face: &Option<theme::LoadedFace>| {
        if let Some(face) = face {
            eprintln!(
                "[font] {label}: family={} style={} ttc_index={} extracted_bytes={} file={}",
                face.family,
                face.style,
                face.index,
                face.bytes,
                face.path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default()
            );
        } else {
            eprintln!("[font] {label}: <none>");
        }
    };
    face("regular", &report.regular);
    face("bold", &report.bold);
    face("latin", &report.latin);
    face("latin-bold", &report.latin_bold);
    eprintln!("[font] bundled_fallback={}", report.bundled_fallback);
    for note in &report.notes {
        eprintln!("[font] note: {note}");
    }
}

#[test]
fn gallery_renders_all_controls_headless() {
    let context = egui::Context::default();
    let report = theme::install_fonts(&context);
    print_font_report(&report);
    theme::apply(&context, true);
    let palette = Palette::dark();
    let mut shape_count = 0_usize;
    let mut triangle_count = 0_usize;
    for _ in 0..2 {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                Pos2::ZERO,
                Vec2::new(CANVAS_W, CANVAS_H),
            )),
            ..Default::default()
        };
        let output = context.run(input, |context| draw_gallery(context, &palette, &report));
        shape_count = output.shapes.len();
        let primitives = context.tessellate(output.shapes, SCALE);
        triangle_count = primitives
            .iter()
            .map(|primitive| match &primitive.primitive {
                egui::epaint::Primitive::Mesh(mesh) => mesh.indices.len() / 3,
                egui::epaint::Primitive::Callback(_) => 0,
            })
            .sum();
    }
    assert!(
        shape_count > 100,
        "gallery must produce many paint commands, got {shape_count}"
    );
    assert!(
        triangle_count > 2_000,
        "gallery must tessellate into real triangles, got {triangle_count}"
    );
    // 字体确实装上了（系统字体或安全回退都必须能排版中英文）。
    assert!(
        report.system_ui() || report.bundled_fallback,
        "font report must describe a usable fallback: {}",
        report.describe()
    );
}

#[test]
fn gallery_writes_png_evidence_when_requested() {
    let Some(directory) = gallery_dir() else {
        eprintln!("LIVE_HUB_WIDGET_GALLERY_DIR 未设置，跳过 PNG 输出（仍会渲染帧）。");
        return;
    };
    // 字体加载证据落盘（文件名即可复现的机器可读记录）。
    let context = egui::Context::default();
    let report = theme::install_fonts(&context);
    print_font_report(&report);
    let mut evidence = String::new();
    let _ = writeln!(evidence, "describe={}", report.describe());
    for (label, face) in [
        ("regular", report.regular.as_ref()),
        ("bold", report.bold.as_ref()),
        ("latin", report.latin.as_ref()),
        ("latin-bold", report.latin_bold.as_ref()),
    ] {
        match face {
            Some(face) => {
                let file = face
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default();
                let _ = writeln!(
                    evidence,
                    "{label}: family={} style={} ttc_index={} extracted_bytes={} file={file}",
                    face.family, face.style, face.index, face.bytes
                );
            }
            None => {
                let _ = writeln!(evidence, "{label}: <none>");
            }
        }
    }
    // R2（task-19）：界面字体设置 UiFontKey/UiFontPercent -> FontOptions 的映射证据。
    for (key, percent) in [("auto", 100u16), ("system", 115), ("harmony", 130)] {
        let options = theme::FontOptions::from_settings(key, percent);
        let _ = writeln!(
            evidence,
            "settings ui_font_key={key} ui_font_percent={percent} => preference={:?} scale_percent={}",
            options.preference, options.scale_percent
        );
    }
    let _ = writeln!(evidence, "bundled_fallback={}", report.bundled_fallback);
    for note in &report.notes {
        let _ = writeln!(evidence, "note: {note}");
    }
    std::fs::write(directory.join("font-report.txt"), evidence).expect("font evidence");
    for (dark, name) in [(true, "gallery-dark.png"), (false, "gallery-light.png")] {
        let canvas = render_scene(dark);
        assert_eq!(canvas.width(), (CANVAS_W * SCALE) as u32);
        assert_eq!(canvas.height(), (CANVAS_H * SCALE) as u32);
        // 画面必须有内容：统计出现的不同颜色数量。
        let mut colors = std::collections::HashSet::new();
        for pixel in canvas.pixels() {
            colors.insert(pixel.0);
            if colors.len() > 64 {
                break;
            }
        }
        assert!(
            colors.len() > 64,
            "{name} looks blank: only {} colors",
            colors.len()
        );
        let path = save(&canvas, &directory, name);
        assert!(path.is_file());
    }
}
