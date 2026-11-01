//! Original-workbench shell: 34px chrome, 164px sidebar, the content frame and
//! the eight pages.
//!
//! Geometry is copied from the C# workbench's logical-pixel layout
//! (livehub-workbench-ui.cs / livehub-workbench-controls.cs): the favourites
//! launch card, the 3-column room grid, the recent list, the session list and
//! the player page all use the same constants as the original, so the window at
//! its 1240x720 design size matches the reference screenshots.
//!
//! Everything here is view code. Actions call the launcher's existing methods
//! (play, search, store actions); no persistence or platform logic lives in
//! this module.

use super::*;

pub(super) use super::theme::{Palette, blend, font, font_bold as bold, with_alpha};
use super::theme::{self, ts};
use eframe::egui::{
    Align2, Color32, FontId, Id, Painter, Rect, Response, Sense, Stroke, StrokeKind, TextureHandle,
    Ui, Vec2, pos2, vec2,
};

/// The shared design-system palette; page code keeps the short alias.
pub(super) type Pal = Palette;

mod accounts;
mod danmaku;
mod data;
mod favorites;
mod lookup;
mod players;
mod recent;
mod sessions;

pub(super) const CHROME_H: f32 = 34.0;
pub(super) const SIDEBAR_W: f32 = 164.0;
pub(super) const NAV_W: f32 = 148.0;
pub(super) const NAV_H: f32 = 36.0;
pub(super) const NAV_H_TALL: f32 = 52.0;
pub(super) const NAV_INDENT: f32 = 8.0;
pub(super) const NAV_GAP: f32 = 4.0;
pub(super) const FOOTER_H: f32 = 32.0;
pub(super) const CONTENT_PAD: f32 = 24.0;

// ---------------------------------------------------------------------------
// Palette: the C# ModernTheme dark/light values (danmaku-15-ui.cs).
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Fonts / small text helpers.
// ---------------------------------------------------------------------------

/// Paint one clipped single-line text run.
pub(super) fn text_in(
    painter: &Painter,
    rect: Rect,
    text: impl ToString,
    font: FontId,
    color: Color32,
    align: Align2,
) {
    let painter = painter.with_clip_rect(rect);
    let pos = match align {
        Align2::LEFT_TOP => rect.left_top(),
        Align2::LEFT_CENTER => rect.left_center(),
        Align2::LEFT_BOTTOM => rect.left_bottom(),
        Align2::CENTER_TOP => rect.center_top(),
        Align2::CENTER_CENTER => rect.center(),
        Align2::CENTER_BOTTOM => rect.center_bottom(),
        Align2::RIGHT_TOP => rect.right_top(),
        Align2::RIGHT_CENTER => rect.right_center(),
        Align2::RIGHT_BOTTOM => rect.right_bottom(),
    };
    painter.text(pos, align, text.to_string(), font, color);
}

/// Paint a wrapped text block (used for hints that must not clip).
pub(super) fn wrap_text(painter: &Painter, rect: Rect, text: impl ToString, font: FontId, color: Color32) {
    let painter = painter.with_clip_rect(rect);
    let galley = painter.layout(text.to_string(), font, color, rect.width());
    painter.galley(rect.left_top(), galley, color);
}

pub(super) fn measure(painter: &Painter, text: &str, font: &FontId) -> f32 {
    painter
        .layout_no_wrap(text.to_owned(), font.clone(), Color32::WHITE)
        .size()
        .x
}

// ---------------------------------------------------------------------------
// Painting primitives (rounded fills/strokes/gradients).
// ---------------------------------------------------------------------------

pub(super) fn rounded(painter: &Painter, rect: Rect, radius: f32, fill: Color32) {
    if fill.a() > 0 {
        theme::round_rect(painter, rect, radius, fill, Stroke::NONE);
    }
}

pub(super) fn rounded_stroke(painter: &Painter, rect: Rect, radius: f32, stroke: Stroke) {
    if stroke.color.a() > 0 && stroke.width > 0.0 {
        theme::round_rect(painter, rect, radius, Color32::TRANSPARENT, stroke);
    }
}

pub(super) fn hline(painter: &Painter, rect: Rect, y: f32, color: Color32) {
    painter.hline(rect.x_range(), y, Stroke::new(1.0, color));
}

/// Vertical gradient clipped to a rounded rectangle shape.
///
/// The card covers in the original are painted with a 22 degree
/// LinearGradientBrush; at 64px high the angle is visually vertical, so a
/// scanline gradient reproduces it. Only the two top corners are rounded: the
/// cover continues into the card body below.
pub(super) fn gradient_cover(painter: &Painter, rect: Rect, radius: f32, top: Color32, bottom: Color32) {
    let corner = if radius > 0.0 {
        egui::CornerRadius {
            nw: radius as u8,
            ne: radius as u8,
            sw: 0,
            se: 0,
        }
    } else {
        egui::CornerRadius::ZERO
    };
    theme::gradient_round_rect(painter, rect, corner, top, bottom);
}

// ---------------------------------------------------------------------------
// Glyphs (copied proportionally from WorkbenchNavigationButton.DrawGlyph).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Glyph {
    Favorites,
    History,
    Search,
    Sessions,
    Players,
    Danmaku,
    Accounts,
    Data,
    Monitor,
    MonitorPlay,
    Drive,
    Play,
    Check,
    Close,
    Min,
    Max,
    Restore,
}

impl Glyph {
    pub(super) fn for_page(page: Page) -> Self {
        match page {
            Page::Favorites => Self::Favorites,
            Page::Recent => Self::History,
            Page::Lookup => Self::Search,
            Page::Sessions => Self::Monitor,
            Page::Players => Self::Data,
            Page::Danmaku => Self::Danmaku,
            Page::Accounts => Self::Accounts,
            Page::Data => Self::Data,
        }
    }
}

pub(super) fn draw_glyph(painter: &Painter, rect: Rect, glyph: Glyph, color: Color32, width: f32) {
    let x = rect.left();
    let y = rect.top();
    let size = rect.width().min(rect.height());
    let center = size / 2.0;
    let stroke = Stroke::new(width.max(1.0), color);
    let line = |a: (f32, f32), b: (f32, f32)| {
        painter.line_segment(
            [pos2(x + a.0 * size, y + a.1 * size), pos2(x + b.0 * size, y + b.1 * size)],
            stroke,
        );
    };
    let poly = |points: &[(f32, f32)], closed: bool| {
        let mut pts: Vec<egui::Pos2> = points
            .iter()
            .map(|(px, py)| pos2(x + px * size, y + py * size))
            .collect();
        if closed && pts.len() > 2 {
            pts.push(pts[0]);
        }
        painter.add(egui::Shape::line(pts, stroke));
    };
    match glyph {
        Glyph::Favorites => {
            let mut points = Vec::new();
            for index in 0..10 {
                let angle = -std::f32::consts::FRAC_PI_2
                    + index as f32 * std::f32::consts::PI / 5.0;
                let radius = size * if index % 2 == 0 { 0.43 } else { 0.20 };
                points.push(pos2(
                    x + center + angle.cos() * radius,
                    y + center + angle.sin() * radius,
                ));
            }
            points.push(points[0]);
            painter.add(egui::Shape::line(points, stroke));
        }
        Glyph::History => {
            painter.circle_stroke(pos2(x + center, y + center), size * 0.36, stroke);
            line((0.09, 0.15), (0.10, 0.40));
            line((0.10, 0.40), (0.33, 0.39));
            line((0.50, 0.29), (0.50, 0.50));
            line((0.50, 0.50), (0.67, 0.60));
        }
        Glyph::Search => {
            painter.circle_stroke(pos2(x + size * 0.40, y + size * 0.40), size * 0.30, stroke);
            line((0.61, 0.61), (0.91, 0.91));
        }
        Glyph::Sessions | Glyph::Monitor => {
            // Original WorkbenchNavigationButton monitor: screen + stand + base,
            // no play triangle for 观看会话.
            painter.rect_stroke(
                Rect::from_min_size(
                    pos2(x + size * 0.08, y + size * 0.10),
                    Vec2::new(size * 0.84, size * 0.60),
                ),
                0.0,
                stroke,
                StrokeKind::Middle,
            );
            line((0.50, 0.71), (0.50, 0.88));
            line((0.30, 0.90), (0.70, 0.90));
        }
        Glyph::MonitorPlay => {
            painter.rect_stroke(
                Rect::from_min_size(
                    pos2(x + size * 0.08, y + size * 0.10),
                    Vec2::new(size * 0.84, size * 0.60),
                ),
                0.0,
                stroke,
                StrokeKind::Middle,
            );
            line((0.50, 0.71), (0.50, 0.88));
            line((0.30, 0.90), (0.70, 0.90));
            poly(&[(0.41, 0.27), (0.41, 0.58), (0.66, 0.42)], true);
        }
        Glyph::Danmaku => {
            painter.rect_stroke(
                Rect::from_min_size(
                    pos2(x + size * 0.10, y + size * 0.10),
                    Vec2::new(size * 0.80, size * 0.60),
                ),
                2.0,
                stroke,
                StrokeKind::Middle,
            );
            poly(&[(0.25, 0.71), (0.25, 0.91), (0.47, 0.71)], false);
            line((0.26, 0.31), (0.74, 0.31));
            line((0.26, 0.49), (0.59, 0.49));
        }
        Glyph::Accounts => {
            // Head circle at (0.5, 0.28) r .16 plus the upper shoulder arc
            // centered at (0.5, 0.95) r .36: PI..2PI leaves the bottom open,
            // matching the original user glyph.
            painter.circle_stroke(pos2(x + size * 0.50, y + size * 0.28), size * 0.16, stroke);
            let shoulders: Vec<egui::Pos2> = (0..=16)
                .map(|step| {
                    let angle =
                        std::f32::consts::PI + step as f32 * std::f32::consts::PI / 16.0;
                    pos2(
                        x + size * 0.50 + angle.cos() * size * 0.36,
                        y + size * 0.95 + angle.sin() * size * 0.36,
                    )
                })
                .collect();
            painter.add(egui::Shape::line(shoulders, stroke));
        }
        Glyph::Players | Glyph::Data => {
            for row in 0..3 {
                let top = 0.09 + row as f32 * 0.29;
                painter.rect_stroke(
                    Rect::from_min_size(
                        pos2(x + size * 0.11, y + size * top),
                        Vec2::new(size * 0.78, size * 0.20),
                    ),
                    1.0,
                    stroke,
                    StrokeKind::Middle,
                );
                line((0.23, top + 0.10), (0.30, top + 0.10));
            }
        }
        Glyph::Drive => {
            painter.rect_stroke(
                Rect::from_min_size(
                    pos2(x + size * 0.08, y + size * 0.22),
                    Vec2::new(size * 0.84, size * 0.56),
                ),
                2.0,
                stroke,
                StrokeKind::Middle,
            );
            line((0.22, 0.50), (0.44, 0.50));
            painter.circle_filled(pos2(x + size * 0.68, y + size * 0.50), size * 0.06, color);
        }
        Glyph::Play => {
            painter.add(egui::Shape::convex_polygon(
                vec![
                    pos2(x + size * 0.34, y + size * 0.20),
                    pos2(x + size * 0.34, y + size * 0.80),
                    pos2(x + size * 0.80, y + size * 0.50),
                ],
                color,
                Stroke::NONE,
            ));
        }
        Glyph::Check => {
            poly(&[(0.20, 0.52), (0.42, 0.74), (0.80, 0.28)], false);
        }
        Glyph::Close => {
            line((0.10, 0.10), (0.90, 0.90));
            line((0.90, 0.10), (0.10, 0.90));
        }
        Glyph::Min => line((0.12, 0.50), (0.88, 0.50)),
        Glyph::Max => {
            painter.rect_stroke(
                Rect::from_min_size(
                    pos2(x + size * 0.12, y + size * 0.12),
                    Vec2::splat(size * 0.76),
                ),
                0.0,
                stroke,
                StrokeKind::Middle,
            );
        }
        Glyph::Restore => {
            painter.rect_stroke(
                Rect::from_min_size(
                    pos2(x + size * 0.24, y + size * 0.10),
                    Vec2::splat(size * 0.66),
                ),
                0.0,
                stroke,
                StrokeKind::Middle,
            );
            painter.rect_stroke(
                Rect::from_min_size(
                    pos2(x + size * 0.10, y + size * 0.24),
                    Vec2::splat(size * 0.66),
                ),
                0.0,
                stroke,
                StrokeKind::Middle,
            );
        }
    }
}

/// Brand mark: accent rounded square with a white monitor-play glyph.
pub(super) fn brand_mark(painter: &Painter, rect: Rect, pal: &Pal) {
    let radius = (rect.height() / 4.0).clamp(4.0, 9.0);
    rounded(painter, rect, radius, pal.accent);
    // The original WorkbenchBrandMark draws an 18px-wide white monitor inside
    // the 30px tile (pad = 22% => screen width = Width - 2*pad), so the glyph
    // box is 72% of the tile, not the tiny centred square we used before.
    let glyph_size = (rect.width() * 0.72).max(6.0);
    let glyph = Rect::from_center_size(
        rect.center() + vec2(0.0, rect.height() * 0.03),
        Vec2::splat(glyph_size),
    );
    draw_glyph(painter, glyph, Glyph::Monitor, Color32::WHITE, 1.5);
}

// ---------------------------------------------------------------------------
// Widgets (local implementations until the shared widget layer lands).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Btn {
    Primary,
    Secondary,
    Subtle,
    Tab,
}

pub(super) fn button(ui: &mut Ui, rect: Rect, text: &str, kind: Btn, pal: &Pal, enabled: bool) -> Response {
    let id = ui.id().with(("wb-button", rect.left() as i32, rect.top() as i32, text));
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let response = ui.interact(rect, id, sense);
    let hovered = enabled && response.hovered();
    let pressed = enabled && response.is_pointer_button_down_on();
    let radius = 8.0;
    let (fill, border, fore) = if !enabled {
        (
            if kind == Btn::Primary { pal.disabled_fill } else { pal.hover },
            Color32::TRANSPARENT,
            pal.text_disabled,
        )
    } else {
        match kind {
            Btn::Tab => (
                if hovered { pal.hover } else { Color32::TRANSPARENT },
                Color32::TRANSPARENT,
                if hovered { pal.text } else { pal.text_secondary },
            ),
            Btn::Primary => (
                if pressed {
                    pal.accent_pressed
                } else if hovered {
                    pal.accent_hover
                } else {
                    pal.accent
                },
                Color32::TRANSPARENT,
                pal.on_accent,
            ),
            Btn::Subtle => (
                if hovered { pal.hover } else { Color32::TRANSPARENT },
                Color32::TRANSPARENT,
                pal.accent,
            ),
            Btn::Secondary => (
                if pressed {
                    pal.border
                } else if hovered {
                    pal.hover
                } else {
                    pal.card
                },
                if hovered { pal.border_hover } else { pal.border },
                pal.text,
            ),
        }
    };
    rounded(ui.painter(), rect, radius, fill);
    rounded_stroke(ui.painter(), rect, radius, Stroke::new(1.0, border));
    text_in(ui.painter(), rect, text, font(12.0), fore, Align2::CENTER_CENTER);
    response
}

/// Selected/plain pill (original ModernButton with Selected / Tab style).
pub(super) fn pill(ui: &mut Ui, rect: Rect, text: &str, selected: bool, pal: &Pal, tab: bool) -> Response {
    let id = ui.id().with(("wb-pill", rect.left() as i32, rect.top() as i32, text));
    let response = ui.interact(rect, id, Sense::click());
    let hovered = response.hovered();
    let pressed = response.is_pointer_button_down_on();
    let radius = 8.0;
    let (fill, border, fore) = if selected {
        (
            if pressed {
                pal.selection_pressed
            } else if hovered {
                pal.selection_hover
            } else {
                pal.selection
            },
            if tab { pal.selection_border } else { Color32::TRANSPARENT },
            pal.selection_text,
        )
    } else if hovered {
        (pal.hover, Color32::TRANSPARENT, pal.text)
    } else {
        (
            Color32::TRANSPARENT,
            Color32::TRANSPARENT,
            if tab { pal.text_secondary } else { pal.text },
        )
    };
    rounded(ui.painter(), rect, radius, fill);
    rounded_stroke(ui.painter(), rect, radius, Stroke::new(1.0, border));
    text_in(ui.painter(), rect, text, font(12.0), fore, Align2::CENTER_CENTER);
    response
}

pub(super) fn group_label(ui: &mut Ui, rect: Rect, text: &str, pal: &Pal) {
    text_in(ui.painter(), rect, text, font(10.0), pal.text_secondary, Align2::LEFT_CENTER);
}

pub(super) fn hint(ui: &mut Ui, rect: Rect, text: &str, pal: &Pal) {
    text_in(ui.painter(), rect, text, font(11.0), pal.text_secondary, Align2::LEFT_CENTER);
}

pub(super) fn field_label(ui: &mut Ui, rect: Rect, label: &str, scope: Option<&str>, pal: &Pal) {
    text_in(ui.painter(), rect, label, font(11.0), pal.text_secondary, Align2::LEFT_CENTER);
    if let Some(scope) = scope {
        text_in(
            ui.painter(),
            rect,
            scope,
            font(10.0),
            pal.text_secondary,
            Align2::RIGHT_CENTER,
        );
    }
}

/// Single-line text input in the original ModernInput style: card fill, 5px
/// radius, border (accent while focused), 8px inner padding.
pub(super) fn input(ui: &mut Ui, rect: Rect, text: &mut String, hint_text: &str, pal: &Pal) -> Response {
    let id = ui.id().with(("wb-input", rect.left() as i32, rect.top() as i32));
    let inner = rect.shrink2(vec2(8.0, 2.0));
    // The surface and border are painted before the text widget so the text
    // (including the placeholder) stays visible.
    rounded(ui.painter(), rect, 5.0, pal.card);
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let response = child.add(
        egui::TextEdit::singleline(text)
            .id(id)
            .frame(false)
            .hint_text(hint_text)
            .font(font(12.0))
            .desired_width(inner.width())
            .margin(egui::Margin::ZERO),
    );
    let focused = response.has_focus();
    rounded_stroke(
        ui.painter(),
        rect,
        5.0,
        Stroke::new(if focused { 1.6 } else { 1.0 }, if focused { pal.accent } else { pal.border }),
    );
    response
}

pub(super) fn multiline(ui: &mut Ui, rect: Rect, text: &mut String, pal: &Pal) -> Response {
    let id = ui.id().with(("wb-multiline", rect.left() as i32, rect.top() as i32));
    let inner = rect.shrink(6.0);
    rounded(ui.painter(), rect, 5.0, pal.card);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner));
    let response = child.add(
        egui::TextEdit::multiline(text)
            .id(id)
            .frame(false)
            .font(font(12.0))
            .desired_width(inner.width())
            .desired_rows(3)
            .margin(egui::Margin::ZERO),
    );
    let focused = response.has_focus();
    rounded_stroke(
        ui.painter(),
        rect,
        5.0,
        Stroke::new(if focused { 1.6 } else { 1.0 }, if focused { pal.accent } else { pal.border }),
    );
    response
}

fn draw_chevron_down(ui: &Ui, rect: Rect, color: Color32) {
    ui.painter().line_segment(
        [rect.left_top(), rect.center_bottom()],
        Stroke::new(1.2, color),
    );
    ui.painter().line_segment(
        [rect.center_bottom(), rect.right_top()],
        Stroke::new(1.2, color),
    );
}

/// Dropdown: card fill + border + chevron, with an explicitly managed popup so
/// the open state is deterministic across frames:
///
/// - the trigger click toggles the state;
/// - while open the popup redraws every frame (a popup that is not redrawn is
///   discarded by egui at the end of the pass);
/// - a click outside the popup, an Esc press, or a row click closes it;
/// - picking a row updates the value and reports a change.
pub(super) fn dropdown(
    ui: &mut Ui,
    rect: Rect,
    id_salt: &str,
    selected: &mut usize,
    items: &[&str],
    pal: &Pal,
) -> Response {
    let id = ui.id().with(("wb-dropdown", id_salt));
    let mut response = ui.interact(rect, id, Sense::click());
    let popup_id = egui::Popup::default_response_id(&response);
    let context = ui.ctx().clone();
    let open_before = egui::Popup::is_id_open(&context, popup_id);
    if response.clicked() {
        if open_before {
            egui::Popup::close_id(&context, popup_id);
        } else {
            egui::Popup::open_id(&context, popup_id);
        }
    }
    let mut open = egui::Popup::is_id_open(&context, popup_id);
    let hovered = response.hovered();
    rounded(ui.painter(), rect, 5.0, pal.card);
    rounded_stroke(
        ui.painter(),
        rect,
        5.0,
        Stroke::new(
            1.0,
            if open || hovered {
                pal.border_hover
            } else {
                pal.border
            },
        ),
    );
    let label = items.get(*selected).copied().unwrap_or("");
    text_in(
        ui.painter(),
        rect.shrink2(vec2(9.0, 0.0)),
        label,
        font(12.0),
        pal.text,
        Align2::LEFT_CENTER,
    );
    let chevron = Rect::from_center_size(
        pos2(rect.right() - 14.0, rect.center().y),
        Vec2::new(8.0, 5.0),
    );
    draw_chevron_down(
        ui,
        chevron,
        if open || hovered {
            pal.text
        } else {
            pal.text_secondary
        },
    );
    let mut popup_rect: Option<Rect> = None;
    let mut picked = false;
    if open {
        // Keep the popup alive for this pass; egui discards popups that are not
        // redrawn, which is exactly what broke the previous implementation.
        ui.memory_mut(|memory| memory.keep_popup_open(popup_id));
        if let Some(inner) = egui::Popup::from_response(&response)
            .open(true)
            .align(egui::RectAlign::BOTTOM_START)
            .gap(2.0)
            .width(rect.width())
            .show(|ui| {
                ui.spacing_mut().item_spacing = vec2(0.0, 2.0);
                for (index, item) in items.iter().enumerate() {
                    let selected_row = index == *selected;
                    let row = ui.add_sized(
                        vec2((rect.width() - 8.0).max(20.0), 24.0),
                        egui::Button::new(
                            egui::RichText::new(*item)
                                .size(12.0)
                                .color(if selected_row { pal.selection_text } else { pal.text }),
                        )
                        .fill(if selected_row { pal.selection } else { pal.card })
                        .stroke(Stroke::NONE)
                        .corner_radius(4),
                    );
                    if row.clicked() {
                        *selected = index;
                        response.mark_changed();
                        picked = true;
                    }
                }
            })
        {
            popup_rect = Some(inner.response.rect);
        }
        if picked {
            egui::Popup::close_id(&context, popup_id);
            open = false;
        } else {
            let clicked = context.input(|input| input.pointer.any_click());
            let click_pos = context.input(|input| input.pointer.interact_pos());
            let outside = clicked
                && click_pos.is_some_and(|pos| popup_rect.is_none_or(|popup| !popup.contains(pos)))
                && !response.hovered()
                && open_before;
            let escape = context.input(|input| input.key_pressed(egui::Key::Escape));
            if outside || escape {
                egui::Popup::close_id(&context, popup_id);
                open = false;
            }
        }
    }
    let _ = open;
    response
}

pub(super) fn checkbox(ui: &mut Ui, rect: Rect, label: &str, value: &mut bool, pal: &Pal) -> Response {
    let id = ui.id().with(("wb-checkbox", rect.left() as i32, rect.top() as i32, label));
    let mut response = ui.interact(rect, id, Sense::click());
    if response.clicked() {
        *value = !*value;
        response.mark_changed();
    }
    let box_size = 14.0;
    let box_rect = Rect::from_min_size(
        pos2(rect.left(), rect.center().y - box_size / 2.0),
        Vec2::splat(box_size),
    );
    if *value {
        rounded(ui.painter(), box_rect, 3.0, pal.accent);
        draw_glyph(ui.painter(), box_rect.shrink(3.0), Glyph::Check, Color32::WHITE, 1.8);
    } else {
        rounded_stroke(ui.painter(), box_rect, 3.0, Stroke::new(1.0, pal.border));
    }
    text_in(
        ui.painter(),
        Rect::from_min_max(pos2(box_rect.right() + 8.0, rect.top()), rect.max),
        label,
        font(12.0),
        if response.hovered() { pal.text } else { pal.text_secondary },
        Align2::LEFT_CENTER,
    );
    response
}

pub(super) fn link(ui: &mut Ui, rect: Rect, text: &str, pal: &Pal) -> Response {
    let id = ui.id().with(("wb-link", rect.left() as i32, rect.top() as i32, text));
    let response = ui.interact(rect, id, Sense::click());
    let color = if response.hovered() { pal.accent_hover } else { pal.accent };
    text_in(ui.painter(), rect, text, font(11.0), color, Align2::CENTER_CENTER);
    response
}

pub(super) fn section_header(ui: &mut Ui, rect: Rect, text: &str, pal: &Pal) {
    text_in(ui.painter(), rect, text, font(14.0), pal.text, Align2::LEFT_CENTER);
}

pub(super) fn empty_state(ui: &mut Ui, rect: Rect, title: &str, subtitle: &str, pal: &Pal) {
    let title_rect = Rect::from_min_size(
        pos2(rect.center().x - rect.width() / 2.0, rect.center().y - 18.0),
        vec2(rect.width(), 20.0),
    );
    text_in(ui.painter(), title_rect, title, font(13.0), pal.text_secondary, Align2::CENTER_CENTER);
    let sub_rect = Rect::from_min_size(title_rect.left_top() + vec2(0.0, 24.0), vec2(rect.width(), 18.0));
    text_in(ui.painter(), sub_rect, subtitle, font(11.0), pal.text_disabled, Align2::CENTER_CENTER);
}

/// Round avatar: cached texture when available, otherwise the first glyph.
pub(super) fn avatar(
    painter: &Painter,
    rect: Rect,
    texture: Option<&TextureHandle>,
    name: &str,
    pal: &Pal,
) {
    match texture {
        Some(texture) => {
            let mut mesh = egui::Mesh::with_texture(texture.id());
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            mesh.add_rect_with_uv(rect, uv, Color32::WHITE);
            painter.add(egui::Shape::mesh(mesh));
        }
        None => {
            painter.circle_filled(rect.center(), rect.width() / 2.0, pal.selection_soft);
            let initial = name.chars().next().unwrap_or('主').to_string();
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                initial,
                font(rect.width() * 0.45),
                if pal.dark { pal.text } else { pal.selection_text },
            );
        }
    }
}

pub(super) fn spinner(ui: &mut Ui, center: egui::Pos2, pal: &Pal) {
    ui.painter()
        .circle_stroke(center, 7.0, Stroke::new(1.6, with_alpha(pal.accent, 0.23)));
    ui.painter().circle_stroke(center, 7.0, Stroke::new(1.6, pal.accent));
}

// ---------------------------------------------------------------------------
// Children helpers.
// ---------------------------------------------------------------------------

/// Create a clipped child Ui at an absolute rect.
pub(super) fn child(ui: &mut Ui, rect: Rect) -> Ui {
    ui.new_child(egui::UiBuilder::new().max_rect(rect))
}

pub(super) fn scroll_area(ui: &mut Ui, id_salt: &str, add: impl FnOnce(&mut Ui)) {
    let mut area = egui::ScrollArea::vertical()
        .id_salt(id_salt)
        .auto_shrink([false, false]);
    // Diagnostic-only: a ui-shot run can capture a specific scroll position
    // (for example the font/hotkey sections of one tab) without user input.
    if let Ok(offset) = std::env::var("LIVE_HUB_SHOT_SCROLL")
        && let Ok(value) = offset.trim().parse::<f32>()
    {
        area = area.vertical_scroll_offset(value);
    }
    area.show(ui, |ui| {
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        add(ui);
    });
}

// ---------------------------------------------------------------------------
// Launcher view entry points.
// ---------------------------------------------------------------------------

impl Launcher {
    fn palette(&self) -> Pal {
        Palette::for_mode(self.dark)
    }

    /// Frame entry point: background shell, page content and footer status.
    pub(super) fn render(&mut self, context: &egui::Context) {
        self.poll_events();
        self.maybe_start_health_checks(context);
        self.schedule_room_badges();
        if self.search_refresh_after_login {
            self.search_refresh_after_login = false;
            if self.page == Page::Lookup && !self.searching && !self.search_input.trim().is_empty() {
                self.start_search(context, true);
            }
        }
        if !self.badge_pending.is_empty() {
            context.request_repaint_after(Duration::from_millis(200));
        }
        if self.playing || self.searching || self.verification_pending || self.storage_pending > 0 {
            context.request_repaint_after(Duration::from_millis(100));
        }
        if self.health_busy {
            context.request_repaint_after(Duration::from_millis(250));
        } else if self.snapshot.credentials.imported.values().any(|imported| *imported) {
            context.request_repaint_after(Duration::from_secs(60));
        }
        let pal = self.palette();
        self.apply_visuals(context, &pal);
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(pal.surface))
            .show(context, |ui| {
                let full = ui.max_rect();
                let chrome = Rect::from_min_size(full.min, vec2(full.width(), CHROME_H));
                let body = Rect::from_min_max(pos2(full.left(), full.top() + CHROME_H), full.max);
                self.chrome(ui, chrome, context, &pal);
                let sidebar = Rect::from_min_size(body.min, vec2(SIDEBAR_W, body.height()));
                self.sidebar(ui, sidebar, &pal);
                let content = Rect::from_min_max(pos2(body.left() + SIDEBAR_W, body.top()), body.max);
                self.content(ui, content, context, &pal);
                self.window_edges(ui, full, context);
            });
        // Avatar updates arrive from the background pool; drop the stale texture
        // so the next card paint re-uploads the cached 48px PNG.
        let mut avatar_updates = 0;
        while let Some(update) = self.avatar_service.try_recv() {
            self.avatar_textures.remove(&update.key());
            avatar_updates += 1;
        }
        if avatar_updates > 0 {
            context.request_repaint();
        }
        // The 深色模式 checkbox on the danmaku page only flips a runtime flag;
        // persist it so the choice survives a restart (original DarkMode).
        if self.dark != self.snapshot.settings.dark_mode && self.storage_pending == 0 {
            let mut settings = self.snapshot.settings.clone();
            settings.dark_mode = self.dark;
            self.send_store(StoreAction::SaveSettings(settings));
        }
    }

    fn apply_visuals(&self, context: &egui::Context, _pal: &Pal) {
        theme::apply(context, self.dark);
    }

    fn chrome(&mut self, ui: &mut Ui, rect: Rect, context: &egui::Context, pal: &Pal) {
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, pal.side);
        hline(painter, rect, rect.bottom() - 0.5, pal.border);
        let icon = Rect::from_min_size(pos2(rect.left() + 14.0, rect.top() + 8.0), Vec2::splat(18.0));
        brand_mark(ui.painter(), icon, pal);
        text_in(
            painter,
            Rect::from_min_size(pos2(rect.left() + 40.0, rect.top()), vec2(80.0, CHROME_H)),
            "看直播",
            font(11.0),
            pal.text_secondary,
            Align2::LEFT_CENTER,
        );
        let dots_left = rect.right() - 8.0 - 30.0 - 34.0 * 2.0;
        let hint_rect = Rect::from_min_max(
            pos2(dots_left - 12.0 - 240.0, rect.top()),
            pos2(dots_left - 12.0, rect.bottom()),
        );
        text_in(
            painter,
            hint_rect,
            "本地播放器 · 四平台",
            font(11.0),
            pal.text_secondary,
            Align2::RIGHT_CENTER,
        );
        let maximized = context.input(|input| input.viewport().maximized.unwrap_or(false));
        let mut drag_rect = rect;
        drag_rect.set_right(dots_left);
        let drag = ui.interact(
            drag_rect,
            Id::new("workbench-chrome-drag"),
            Sense::click_and_drag(),
        );
        if drag.drag_started() && !maximized {
            context.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if drag.double_clicked() {
            context.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
        }
        for (index, kind) in [Glyph::Min, Glyph::Max, Glyph::Close].into_iter().enumerate() {
            let dot_rect = Rect::from_min_size(
                pos2(dots_left + index as f32 * 34.0, rect.top() + 2.0),
                Vec2::splat(30.0),
            );
            let response = ui.interact(dot_rect, Id::new(("traffic-dot", index)), Sense::click());
            let color = match kind {
                Glyph::Close => Color32::from_rgb(0xFF, 0x5F, 0x57),
                Glyph::Max => Color32::from_rgb(0x28, 0xC8, 0x40),
                _ => Color32::from_rgb(0xFE, 0xBC, 0x2E),
            };
            let color = if response.hovered() {
                blend(color, Color32::WHITE, 0.16)
            } else {
                color
            };
            // The user asked for larger dots: 20px circle with a 10px glyph.
            ui.painter()
                .circle_filled(dot_rect.center(), 10.0, color);
            let glyph = match kind {
                Glyph::Max if maximized => Glyph::Restore,
                other => other,
            };
            draw_glyph(
                ui.painter(),
                Rect::from_center_size(dot_rect.center(), Vec2::splat(10.0)),
                glyph,
                Color32::from_rgb(0x36, 0x26, 0x20),
                1.6,
            );
            if response.clicked() {
                match kind {
                    Glyph::Close => context.send_viewport_cmd(egui::ViewportCommand::Close),
                    Glyph::Min => context.send_viewport_cmd(egui::ViewportCommand::Minimized(true)),
                    _ => context.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized)),
                }
            }
        }
    }

    fn sidebar(&mut self, ui: &mut Ui, rect: Rect, pal: &Pal) {
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, pal.side);
        painter.vline(rect.right() - 0.5, rect.y_range(), Stroke::new(1.0, pal.border));
        let mark = Rect::from_min_size(pos2(rect.left() + 12.0, rect.top() + 22.0), Vec2::splat(30.0));
        brand_mark(ui.painter(), mark, pal);
        text_in(
            painter,
            Rect::from_min_size(pos2(rect.left() + 50.0, rect.top() + 24.0), vec2(106.0, 26.0)),
            "看直播",
            bold(19.0),
            pal.text,
            Align2::LEFT_CENTER,
        );
        let mut group_rect = Rect::from_min_size(
            pos2(rect.left() + 12.0, rect.top() + 76.0),
            vec2(140.0, 16.0),
        );
        group_label(ui, group_rect, "观看", pal);
        let mut y = rect.top() + 100.0;
        for (index, page) in Page::ALL.into_iter().enumerate() {
            if index == Page::SETTINGS_GROUP {
                group_rect = Rect::from_min_size(pos2(rect.left() + 12.0, y + 10.0), vec2(140.0, 16.0));
                group_label(ui, group_rect, "设置与账号", pal);
                y += 29.0;
            }
            let height = if page == Page::Data { NAV_H_TALL } else { NAV_H };
            let item = Rect::from_min_size(pos2(rect.left() + NAV_INDENT, y), vec2(NAV_W, height));
            let selected = self.page == page;
            let response = ui.interact(item, Id::new(("nav", page.key())), Sense::click());
            let hovered = response.hovered();
            let fill = if selected {
                if response.is_pointer_button_down_on() {
                    pal.selection_pressed
                } else if hovered {
                    pal.selection_hover
                } else {
                    pal.selection
                }
            } else if hovered {
                pal.hover
            } else {
                Color32::TRANSPARENT
            };
            if fill.a() > 0 {
                rounded(ui.painter(), item, 7.0, fill);
            }
            let fore = if selected { pal.selection_text } else { pal.text };
            let glyph_rect = Rect::from_min_size(
                pos2(item.left() + 11.0, item.center().y - 7.5),
                Vec2::splat(15.0),
            );
            draw_glyph(ui.painter(), glyph_rect, Glyph::for_page(page), fore, 1.25);
            text_in(
                ui.painter(),
                Rect::from_min_max(
                    pos2(item.left() + 31.0, item.top() + 2.0),
                    pos2(item.right() - 6.0, item.bottom() - 2.0),
                ),
                page.title(),
                if selected { bold(10.5) } else { font(10.5) },
                fore,
                Align2::LEFT_CENTER,
            );
            if response.clicked() {
                self.page = page;
            }
            y += height + NAV_GAP;
        }
        let footer_y = (rect.bottom() - 53.0).max(y + 8.0);
        draw_glyph(
            ui.painter(),
            Rect::from_min_size(pos2(rect.left() + 12.0, footer_y + 3.0), Vec2::splat(16.0)),
            Glyph::Drive,
            pal.text_secondary,
            1.2,
        );
        text_in(
            ui.painter(),
            Rect::from_min_size(pos2(rect.left() + 34.0, footer_y), vec2(122.0, 20.0)),
            "本地播放器 · 四平台",
            font(11.0),
            pal.text_secondary,
            Align2::LEFT_CENTER,
        );
    }

    fn content(&mut self, ui: &mut Ui, rect: Rect, context: &egui::Context, pal: &Pal) {
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, pal.surface);
        let title_font = bold(23.0);
        // The original GDI label has a small left bearing and the subtitle starts
        // 14px after the title *control* (whose width is the ink width + 19).
        let title_ink_width = measure(ui.painter(), self.page.title(), &title_font).max(60.0);
        let title_width = title_ink_width + 19.0;
        let title_rect = Rect::from_min_size(
            pos2(rect.left() + CONTENT_PAD + 5.0, rect.top() + 8.0),
            vec2(title_width, 32.0),
        );
        text_in(ui.painter(), title_rect, self.page.title(), title_font, pal.text, Align2::LEFT_CENTER);
        let subtitle_rect = Rect::from_min_size(
            pos2(title_rect.left() + title_width + 14.0, rect.top() + 15.0),
            vec2((rect.right() - CONTENT_PAD - title_rect.left() - title_width - 14.0).max(60.0), 18.0),
        );
        text_in(
            ui.painter(),
            subtitle_rect,
            self.page.subtitle(),
            font(12.0),
            pal.text_secondary,
            Align2::LEFT_CENTER,
        );
        let footer = Rect::from_min_size(
            pos2(rect.left() + CONTENT_PAD, rect.bottom() - FOOTER_H - 8.0),
            vec2(rect.width() - CONTENT_PAD * 2.0, FOOTER_H),
        );
        let host = Rect::from_min_max(
            pos2(rect.left() + CONTENT_PAD, rect.top() + 50.0),
            pos2(rect.right() - CONTENT_PAD, footer.top() - 10.0),
        );
        if self.shot_mode {
            eprintln!(
                "[layout] content={:?} host={:?} page={:?}",
                rect,
                host,
                self.page.key()
            );
        }
        // Give the page its own clipped Ui: on the minimum window size a tall
        // page must be cut at the host rect instead of painting over the footer.
        let mut page_ui = child(ui, host);
        page_ui.set_clip_rect(host);
        match self.page {
            Page::Favorites => self.favorites_page(&mut page_ui, host, context, pal),
            Page::Recent => self.recent_page(&mut page_ui, host, context, pal),
            Page::Lookup => self.lookup_page(&mut page_ui, host, context, pal),
            Page::Sessions => self.sessions_workbench_page(&mut page_ui, host, context, pal),
            Page::Players => self.players_page(&mut page_ui, host, context, pal),
            Page::Danmaku => self.danmaku_page(&mut page_ui, host, context, pal),
            Page::Accounts => self.accounts_workbench_page(&mut page_ui, host, context),
            Page::Data => self.data_workbench_page(&mut page_ui, host, context, pal),
        }
        self.footer(ui, footer, context, pal);
    }

    fn footer(&mut self, ui: &mut Ui, rect: Rect, _context: &egui::Context, pal: &Pal) {
        let idle = self.status.is_empty() || self.status.starts_with("准备就绪");
        let (text, color) = if idle {
            (self.page.idle_hint().to_owned(), pal.text_secondary)
        } else if self.status_error {
            (self.status.clone(), pal.danger)
        } else {
            (self.status.clone(), pal.accent)
        };
        let cancel_width = if self.playing { 118.0 } else { 0.0 };
        let status_rect = Rect::from_min_max(rect.min, pos2(rect.right() - cancel_width - 8.0, rect.bottom()));
        if self.playing || self.searching || self.verification_pending || self.storage_pending > 0 {
            spinner(ui, pos2(status_rect.left() + 6.0, status_rect.center().y), pal);
        } else {
            let circle = Rect::from_center_size(
                pos2(status_rect.left() + 6.5, status_rect.center().y),
                Vec2::splat(13.0),
            );
            ui.painter()
                .circle_stroke(circle.center(), 6.5, Stroke::new(1.2, pal.text_secondary));
            draw_glyph(
                ui.painter(),
                circle.shrink(3.0),
                Glyph::Check,
                if self.status_error { pal.danger } else { pal.text_secondary },
                1.4,
            );
        }
        text_in(
            ui.painter(),
            status_rect.shrink2(vec2(16.0, 0.0)),
            &text,
            font(11.0),
            color,
            Align2::LEFT_CENTER,
        );
        if self.playing {
            let button_rect = Rect::from_min_size(
                pos2(rect.right() - cancel_width, rect.top() + (rect.height() - 29.0) / 2.0),
                vec2(cancel_width, 29.0),
            );
            if button(ui, button_rect, "取消启动", Btn::Secondary, pal, true).clicked() {
                let cancellable = self
                    .sessions
                    .snapshots()
                    .iter()
                    .find(|session| session.phase.can_cancel())
                    .map(|session| session.id.clone());
                if let Some(id) = cancellable {
                    match self.sessions.cancel_start(&id) {
                        Ok(()) => self.set_status("正在取消启动…", false),
                        Err(error) => self.set_status(error.to_string(), true),
                    }
                }
            }
        }
    }

    /// Invisible 6px window resize strips for the frameless window.
    fn window_edges(&mut self, ui: &mut Ui, full: Rect, context: &egui::Context) {
        let maximized = context.input(|input| input.viewport().maximized.unwrap_or(false));
        if maximized {
            return;
        }
        let hover = context.pointer_hover_pos();
        let mut cursor: Option<egui::CursorIcon> = None;
        let strip = 6.0;
        let regions = [
            (
                Rect::from_min_size(full.min, vec2(full.width(), strip)),
                egui::ResizeDirection::North,
                egui::CursorIcon::ResizeNorth,
            ),
            (
                Rect::from_min_size(pos2(full.left(), full.bottom() - strip), vec2(full.width(), strip)),
                egui::ResizeDirection::South,
                egui::CursorIcon::ResizeSouth,
            ),
            (
                Rect::from_min_size(full.min, vec2(strip, full.height())),
                egui::ResizeDirection::West,
                egui::CursorIcon::ResizeWest,
            ),
            (
                Rect::from_min_size(pos2(full.right() - strip, full.top()), vec2(strip, full.height())),
                egui::ResizeDirection::East,
                egui::CursorIcon::ResizeEast,
            ),
            (
                Rect::from_min_size(full.left_top(), Vec2::splat(strip)),
                egui::ResizeDirection::NorthWest,
                egui::CursorIcon::ResizeNorthWest,
            ),
            (
                Rect::from_min_size(pos2(full.right() - strip, full.top()), Vec2::splat(strip)),
                egui::ResizeDirection::NorthEast,
                egui::CursorIcon::ResizeNorthEast,
            ),
            (
                Rect::from_min_size(pos2(full.left(), full.bottom() - strip), Vec2::splat(strip)),
                egui::ResizeDirection::SouthWest,
                egui::CursorIcon::ResizeSouthWest,
            ),
            (
                Rect::from_min_size(pos2(full.right() - strip, full.bottom() - strip), Vec2::splat(strip)),
                egui::ResizeDirection::SouthEast,
                egui::CursorIcon::ResizeSouthEast,
            ),
        ];
        for (index, (region, direction, icon)) in regions.into_iter().enumerate() {
            let response = ui.interact(region, Id::new(("window-resize", index)), Sense::drag());
            if let Some(position) = hover
                && region.contains(position)
            {
                cursor = Some(icon);
            }
            if response.drag_started() {
                context.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
            }
        }
        if let Some(icon) = cursor {
            context.set_cursor_icon(icon);
        }
    }
}

/// Legacy inline platform picker used by the accounts and data pages until
/// those pages are re-laid-out: one pill row in the current layout flow.
impl Launcher {
    pub(super) fn platform_picker(&mut self, ui: &mut Ui) {
        let pal = self.palette();
        ui.horizontal(|ui| {
            for platform in Platform::ALL {
                let selected = self.platform == platform;
                let (rect, _) = ui.allocate_exact_size(vec2(88.0, 30.0), Sense::hover());
                let response = ui.interact(rect, Id::new(("legacy-platform", platform)), Sense::click());
                let fill = if selected {
                    pal.selection
                } else if response.hovered() {
                    pal.hover
                } else {
                    pal.card
                };
                rounded(ui.painter(), rect, 6.0, fill);
                rounded_stroke(
                    ui.painter(),
                    rect,
                    6.0,
                    Stroke::new(1.0, if selected { pal.selection_border } else { pal.border }),
                );
                text_in(
                    ui.painter(),
                    rect,
                    platform.label(),
                    font(12.0),
                    if selected { pal.selection_text } else { pal.text },
                    Align2::CENTER_CENTER,
                );
                if response.clicked() {
                    self.platform = platform;
                }
            }
        });
    }
}

impl Launcher {
    /// Texture for one room's platform avatar, or None while only the initial
    /// placeholder is available.
    ///
    /// The disk/memory cache is synchronous and offline; the first miss schedules
    /// one de-duplicated background fetch on a real launch. badge_networking is
    /// off for tests and ui-shot runs, so a capture never uses the network.
    pub(super) fn avatar_texture(
        &mut self,
        context: &egui::Context,
        platform: Platform,
        room: &str,
    ) -> Option<egui::TextureHandle> {
        let key = crate::avatars::avatar_key(platform, room);
        if let Some(texture) = self.avatar_textures.get(&key) {
            return Some(texture.clone());
        }
        if let Some(entry) = self.avatar_service.cached(platform, room) {
            let decoded = crate::avatars::decode_avatar(&entry.png)?;
            let size = [decoded.width() as usize, decoded.height() as usize];
            let image = egui::ColorImage::from_rgba_unmultiplied(size, decoded.as_raw());
            let texture = context.load_texture(key.clone(), image, egui::TextureOptions::LINEAR);
            self.avatar_textures.insert(key, texture.clone());
            return Some(texture);
        }
        if self.badge_networking {
            self.avatar_service.request(platform, room);
        }
        None
    }
}
