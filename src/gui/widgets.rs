//! 自绘控件层：原版 C# ModernButton / ModernInput / ModernComboBox / WorkbenchNavigationButton /
//! TrafficDot / 卡片 / 徽章 等的 Rust 对应实现。
//!
//! 所有几何都是**逻辑 px**（原版 probe 的物理值 /2），颜色只从 [theme::Palette] 取，
//! 不允许在本模块外硬编码色值。
//!
//! 控件用 egui 的"分配矩形 + 自己画"模式实现（不是 egui 内置按钮换皮），这样圆角、
//! 描边、悬停/按下的色档能和原版 GDI+ 自绘逐像素对齐。每个控件的绘制规格都抽成了
//! 纯函数（[button_spec] / [tab_spec] / [nav_spec] / [pill_spec]），单元测试直接断言三态颜色。
//!
//! ## 典型用法
//!
//! - 主/次/描边按钮：button(ui, "用 mpv 打开（回车）", ButtonKind::Primary)
//! - 平台切换：platform_tabs(ui, &["B站", "虎牙", "斗鱼", "抖音"], &mut selected)
//! - 侧栏：nav_button(ui, Glyph::Favorites, "我的收藏", selected)
//! - 输入：input(ui, &mut room, "输入房间号，或粘贴官方直播间链接")
//! - 下拉：dropdown(ui, "quality", &mut index, &["原画 / 最佳", "高清", "流畅"], 191.0)
//! - 面板：card(ui, |ui| { ... })
//! - 标题栏圆钮：traffic_dot(ui, DotKind::Close, false)
//!
//! 尺寸常量统一来自 [theme::metrics] / [theme::space]；换主题时控件不需要重建，
//! 每帧从 ui.visuals().dark_mode 取当前 Palette 即可。

use super::theme::{self, Palette, lh, metrics, space, ts};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontId, Id, Rect, Response, Sense, Stroke,
    StrokeKind, Ui, Vec2,
};
use std::f32::consts::{FRAC_PI_2, TAU};
use std::sync::Arc;

// ============================================================================================
// 状态与绘制规格（纯函数，便于测试断言）
// ============================================================================================

/// 控件的交互状态（对应原版 _hover / _pressed / !Enabled）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WidgetState {
    /// 普通。
    Normal,
    /// 悬停。
    Hovered,
    /// 按下。
    Pressed,
    /// 禁用。
    Disabled,
}

impl WidgetState {
    /// 由 enabled / hovered / pressed 组合出状态。
    pub const fn resolve(enabled: bool, hovered: bool, pressed: bool) -> Self {
        if !enabled {
            Self::Disabled
        } else if pressed {
            Self::Pressed
        } else if hovered {
            Self::Hovered
        } else {
            Self::Normal
        }
    }
}

/// 一个控件的绘制规格：填充色 + 描边 + 文字/图标色。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaintSpec {
    /// 背景填充（透明 = 不画）。
    pub fill: Color32,
    /// 描边（宽度 0 = 不画）。
    pub stroke: Stroke,
    /// 前景（文字/图标）。
    pub text: Color32,
}

/// 按钮样式（对应原版 ModernButton.Kind + Tab/Selected 变体）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonKind {
    /// 强调色实心（"用 mpv 打开"）。
    Primary,
    /// 卡片底 + 描边（"用 PotPlayer 打开" / "加入收藏"）。
    Secondary,
    /// 透明底 + 描边（放在 surface 上用）。
    Outline,
    /// 无底无边的弱按钮，悬停有中性底，文字强调色（"格式说明" / "弹幕设置…"）。
    Subtle,
    /// 纯链接外观（无悬停底），文字强调色（"全部会话 →"）。
    Link,
}

/// 按钮三态规格（逐档对应原版 ModernButton.OnPaint 的分支）。
pub fn button_spec(kind: ButtonKind, state: WidgetState, palette: &Palette) -> PaintSpec {
    let no_stroke = Stroke::NONE;
    if state == WidgetState::Disabled {
        return PaintSpec {
            fill: if kind == ButtonKind::Primary {
                palette.disabled_fill
            } else if kind == ButtonKind::Subtle || kind == ButtonKind::Link {
                Color32::TRANSPARENT
            } else {
                palette.hover
            },
            stroke: no_stroke,
            text: palette.text_disabled,
        };
    }
    match kind {
        ButtonKind::Primary => PaintSpec {
            fill: match state {
                WidgetState::Pressed => palette.accent_pressed,
                WidgetState::Hovered => palette.accent_hover,
                _ => palette.accent,
            },
            stroke: no_stroke,
            text: palette.on_accent,
        },
        ButtonKind::Secondary => PaintSpec {
            fill: match state {
                WidgetState::Pressed => palette.border,
                WidgetState::Hovered => palette.hover,
                _ => palette.card,
            },
            stroke: Stroke::new(
                1.0_f32,
                if state == WidgetState::Hovered || state == WidgetState::Pressed {
                    palette.border_hover
                } else {
                    palette.border
                },
            ),
            text: palette.text,
        },
        ButtonKind::Outline => PaintSpec {
            fill: match state {
                WidgetState::Pressed => palette.border,
                WidgetState::Hovered => palette.hover,
                _ => Color32::TRANSPARENT,
            },
            stroke: Stroke::new(
                1.0_f32,
                if state == WidgetState::Hovered || state == WidgetState::Pressed {
                    palette.border_hover
                } else {
                    palette.border
                },
            ),
            text: palette.text,
        },
        ButtonKind::Subtle => PaintSpec {
            fill: if state == WidgetState::Hovered {
                palette.hover
            } else {
                Color32::TRANSPARENT
            },
            stroke: no_stroke,
            text: palette.accent,
        },
        ButtonKind::Link => PaintSpec {
            fill: Color32::TRANSPARENT,
            stroke: no_stroke,
            text: palette.accent,
        },
    }
}

/// 平台/分类 tab 规格（选中 = 亮橙底 + 深棕字；未选中悬停中性灰）。
pub fn tab_spec(selected: bool, state: WidgetState, palette: &Palette) -> PaintSpec {
    if state == WidgetState::Disabled {
        return PaintSpec {
            fill: Color32::TRANSPARENT,
            stroke: Stroke::NONE,
            text: palette.text_disabled,
        };
    }
    if selected {
        PaintSpec {
            fill: match state {
                WidgetState::Pressed => palette.selection_pressed,
                WidgetState::Hovered => palette.selection_hover,
                _ => palette.selection,
            },
            stroke: Stroke::NONE,
            text: palette.selection_text,
        }
    } else {
        PaintSpec {
            fill: if state == WidgetState::Hovered {
                palette.hover
            } else {
                Color32::TRANSPARENT
            },
            stroke: Stroke::NONE,
            text: if state == WidgetState::Hovered {
                palette.text
            } else {
                palette.text_secondary
            },
        }
    }
}

/// 侧栏导航项规格（选中 = 整行亮橙底 + 深棕字/图标，原版 2026-10-22 改版）。
pub fn nav_spec(selected: bool, state: WidgetState, palette: &Palette) -> PaintSpec {
    if state == WidgetState::Disabled {
        return PaintSpec {
            fill: Color32::TRANSPARENT,
            stroke: Stroke::NONE,
            text: palette.text_disabled,
        };
    }
    if selected {
        PaintSpec {
            fill: match state {
                WidgetState::Pressed => palette.selection_pressed,
                WidgetState::Hovered => palette.selection_hover,
                _ => palette.selection,
            },
            stroke: Stroke::NONE,
            text: palette.selection_text,
        }
    } else {
        PaintSpec {
            fill: if state == WidgetState::Hovered || state == WidgetState::Pressed {
                palette.hover
            } else {
                Color32::TRANSPARENT
            },
            stroke: Stroke::NONE,
            text: palette.text,
        }
    }
}

/// 小胶囊（"选择"丸）规格：选中橙底深棕字，未选中卡底 + 描边。
pub fn pill_spec(selected: bool, state: WidgetState, palette: &Palette) -> PaintSpec {
    if selected {
        PaintSpec {
            fill: palette.selection,
            stroke: Stroke::new(1.0_f32, palette.selection_border),
            text: palette.selection_text,
        }
    } else {
        PaintSpec {
            fill: if state == WidgetState::Hovered {
                palette.hover
            } else {
                palette.card
            },
            stroke: Stroke::new(1.0_f32, palette.border),
            text: palette.text,
        }
    }
}

// ============================================================================================
// 文本与测量辅助
// ============================================================================================

/// 当前调色板（跟随 egui 的 dark_mode）。
pub fn palette_of(ui: &Ui) -> Palette {
    Palette::for_mode(ui.visuals().dark_mode)
}

/// 用指定字体排版单行文本（颜色用占位色，实际颜色绘制时给）。
fn galley(ui: &Ui, text: &str, font: FontId) -> Arc<egui::Galley> {
    ui.painter()
        .layout_no_wrap(text.to_owned(), font, Color32::PLACEHOLDER)
}

/// 在 painter 上画一段已排版好的文本（颜色在绘制时覆盖，便于悬停变色不重排）。
fn paint_galley(
    painter: &egui::Painter,
    galley: &Arc<egui::Galley>,
    pos: egui::Pos2,
    color: Color32,
) {
    painter.galley(pos, galley.clone(), color);
}

/// 左对齐 + 垂直居中绘制单行文本，返回实际占用的矩形。
fn text_left(
    painter: &egui::Painter,
    rect: Rect,
    text: &str,
    font: FontId,
    color: Color32,
) -> Rect {
    let galley = painter.layout_no_wrap(text.to_owned(), font, Color32::PLACEHOLDER);
    let size = galley.size();
    let pos = egui::pos2(rect.left(), rect.center().y - size.y / 2.0);
    paint_galley(painter, &galley, pos, color);
    Rect::from_min_size(pos, size)
}

/// 居中绘制单行文本。
fn text_center(painter: &egui::Painter, rect: Rect, text: &str, font: FontId, color: Color32) {
    let galley = painter.layout_no_wrap(text.to_owned(), font, Color32::PLACEHOLDER);
    let pos = rect.center() - galley.size() / 2.0;
    paint_galley(painter, &galley, pos, color);
}

/// 右对齐 + 垂直居中绘制单行文本。
fn text_right(painter: &egui::Painter, rect: Rect, text: &str, font: FontId, color: Color32) {
    let galley = painter.layout_no_wrap(text.to_owned(), font, Color32::PLACEHOLDER);
    let size = galley.size();
    let pos = egui::pos2(rect.right() - size.x, rect.center().y - size.y / 2.0);
    paint_galley(painter, &galley, pos, color);
}

/// 一行的文字高度（字体度量），用于输入框内垂直居中。
fn row_height(ui: &Ui, font: &FontId) -> f32 {
    galley(ui, "国Wgj", font.clone()).size().y
}

// ============================================================================================
// 按钮
// ============================================================================================

/// 主/次/描边/弱按钮，宽度按文字自适应，高度默认 [metrics::BTN_H]。
pub fn button(ui: &mut Ui, text: &str, kind: ButtonKind) -> Response {
    button_enabled(ui, text, kind, true)
}

/// 带 enabled 的按钮（禁用走原版 DisabledFill / TextDisabled）。
pub fn button_enabled(ui: &mut Ui, text: &str, kind: ButtonKind, enabled: bool) -> Response {
    let font = theme::font(ts::BODY);
    let galley = galley(ui, text, font);
    let width = galley.size().x + metrics::BTN_PAD_X * 2.0;
    button_sized(ui, text, kind, Vec2::new(width, metrics::BTN_H), enabled)
}

/// 指定高度的按钮（原版有 24 / 30 / 34 / 38 四档）。
pub fn button_h(ui: &mut Ui, text: &str, kind: ButtonKind, height: f32) -> Response {
    let font = theme::font(ts::BODY);
    let galley = galley(ui, text, font);
    let width = galley.size().x + metrics::BTN_PAD_X * 2.0;
    button_sized(ui, text, kind, Vec2::new(width, height), true)
}

/// 完全指定尺寸的按钮（页面按原版 probe 的精确宽高调用，例如 155x38）。
pub fn button_sized(
    ui: &mut Ui,
    text: &str,
    kind: ButtonKind,
    size: Vec2,
    enabled: bool,
) -> Response {
    button_inner(ui, text, kind, size, enabled, None, theme::font(ts::BODY))
}

/// 指定尺寸 + 字体的按钮（launch 卡大按钮用 13px，小按钮用 11px）。
pub fn button_sized_font(
    ui: &mut Ui,
    text: &str,
    kind: ButtonKind,
    size: Vec2,
    enabled: bool,
    font: FontId,
) -> Response {
    button_inner(ui, text, kind, size, enabled, None, font)
}

/// 带图标 glyph 的按钮（原版 "▷ mpv" 之类用文字三角，这里支持矢量图标）。
pub fn button_icon(
    ui: &mut Ui,
    glyph: Glyph,
    text: &str,
    kind: ButtonKind,
    size: Vec2,
    enabled: bool,
) -> Response {
    button_inner(
        ui,
        text,
        kind,
        size,
        enabled,
        Some(glyph),
        theme::font(ts::BODY),
    )
}

fn button_inner(
    ui: &mut Ui,
    text: &str,
    kind: ButtonKind,
    size: Vec2,
    enabled: bool,
    icon: Option<Glyph>,
    font: FontId,
) -> Response {
    let palette = palette_of(ui);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let state = WidgetState::resolve(
        enabled,
        response.hovered(),
        response.is_pointer_button_down_on(),
    );
    let spec = button_spec(kind, state, &palette);
    let painter = ui.painter();
    let radius = CornerRadius::same(metrics::BTN_RADIUS);
    if spec.fill != Color32::TRANSPARENT {
        painter.rect_filled(rect, radius, spec.fill);
    }
    if spec.stroke.width > 0.0 {
        painter.rect_stroke(rect, radius, spec.stroke, StrokeKind::Inside);
    }
    let galley = painter.layout_no_wrap(text.to_owned(), font.clone(), Color32::PLACEHOLDER);
    let content_width = galley.size().x + icon.map_or(0.0, |_| metrics::NAV_ICON + space::XS);
    let mut cursor = rect.center().x - content_width / 2.0;
    let clip = painter.with_clip_rect(rect.shrink(2.0));
    if let Some(glyph) = icon {
        let icon_rect = Rect::from_center_size(
            egui::pos2(cursor + metrics::NAV_ICON / 2.0, rect.center().y),
            Vec2::splat(metrics::NAV_ICON),
        );
        paint_glyph(&clip, icon_rect, glyph, spec.text, metrics::GLYPH_STROKE);
        cursor += metrics::NAV_ICON + space::XS;
    }
    let pos = egui::pos2(cursor, rect.center().y - galley.size().y / 2.0);
    paint_galley(&clip, &galley, pos, spec.text);
    if enabled {
        response.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        response
    }
}

// ============================================================================================
// Tab / 平台切换
// ============================================================================================

/// 单独的 tab 胶囊（选中 = 亮橙底 + 深棕字），宽度按文字自适应，高度 [metrics::TAB_H]。
pub fn tab_pill(ui: &mut Ui, text: &str, selected: bool) -> Response {
    let width = tab_width(ui, text);
    tab_pill_sized(ui, text, selected, Vec2::new(width, metrics::TAB_H), true)
}

/// 指定尺寸的 tab 胶囊。
pub fn tab_pill_sized(
    ui: &mut Ui,
    text: &str,
    selected: bool,
    size: Vec2,
    enabled: bool,
) -> Response {
    let palette = palette_of(ui);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let state = WidgetState::resolve(
        enabled,
        response.hovered(),
        response.is_pointer_button_down_on(),
    );
    let spec = tab_spec(selected, state, &palette);
    let painter = ui.painter();
    let radius = CornerRadius::same(metrics::BTN_RADIUS);
    if spec.fill != Color32::TRANSPARENT {
        painter.rect_filled(rect, radius, spec.fill);
    }
    text_center(
        &painter.with_clip_rect(rect.shrink(4.0)),
        rect,
        text,
        theme::font(ts::BODY),
        spec.text,
    );
    if enabled {
        response.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        response
    }
}

fn tab_width(ui: &Ui, text: &str) -> f32 {
    let galley = galley(ui, text, theme::font(ts::BODY));
    (galley.size().x + metrics::BTN_PAD_X * 2.0).max(metrics::PLATFORM_PILL_W)
}

/// 平台切换组：原版 4 个 72x30 tab（间距 6）。
///
/// ⚠️ 原版收藏/最近页的 WorkbenchFillStrip 是 **Visible=false**（截图里 tab 直接坐在
/// 卡片/页面底上，没有独立分组底），所以默认**不画**组合底；需要那条装饰底（其它页面
/// 若启用）用 [platform_tabs_strip]。
///
/// 行高占用 30px；点击会直接更新 selected 并在响应上 mark_changed()。
pub fn platform_tabs(ui: &mut Ui, labels: &[&str], selected: &mut usize) -> Response {
    platform_tabs_impl(ui, labels, selected, false)
}

/// 带 WorkbenchFillStrip 分组底（Hover 底 + 圆角 9，向外出血 3px）的平台 tab 组。
///
/// 原版当前只在 FillStrip.Visible=true 的页面画出来；收藏/最近页不要用这个。
pub fn platform_tabs_strip(ui: &mut Ui, labels: &[&str], selected: &mut usize) -> Response {
    platform_tabs_impl(ui, labels, selected, true)
}

fn platform_tabs_impl(ui: &mut Ui, labels: &[&str], selected: &mut usize, strip: bool) -> Response {
    let palette = palette_of(ui);
    let tab_width = metrics::PLATFORM_PILL_W;
    let total_width = tab_width * labels.len() as f32
        + metrics::TAB_GAP * (labels.len().saturating_sub(1)) as f32;
    let (rect, mut group) =
        ui.allocate_exact_size(Vec2::new(total_width, metrics::TAB_H), Sense::click());
    let painter = ui.painter();
    // 组合底（默认不画；原版 WorkbenchFillStrip 在收藏/最近页 Visible=false）。
    if strip {
        painter.rect_filled(
            rect.expand(metrics::STRIP_BLEED),
            CornerRadius::same(metrics::STRIP_RADIUS),
            palette.hover,
        );
    }
    let mut clicked = None;
    for (index, label) in labels.iter().enumerate() {
        let left = rect.left() + index as f32 * (tab_width + metrics::TAB_GAP);
        let tab_rect = Rect::from_min_size(
            egui::pos2(left, rect.top()),
            Vec2::new(tab_width, metrics::TAB_H),
        );
        let response = ui.interact(
            tab_rect,
            group.id.with(("platform-tab", index)),
            Sense::click(),
        );
        let state = WidgetState::resolve(
            true,
            response.hovered(),
            response.is_pointer_button_down_on(),
        );
        let spec = tab_spec(index == *selected, state, &palette);
        if spec.fill != Color32::TRANSPARENT {
            painter.rect_filled(tab_rect, CornerRadius::same(metrics::BTN_RADIUS), spec.fill);
        }
        text_center(
            &painter.with_clip_rect(tab_rect.shrink(4.0)),
            tab_rect,
            label,
            theme::font(ts::BODY),
            spec.text,
        );
        if response.clicked() {
            clicked = Some(index);
        }
    }
    if let Some(index) = clicked {
        if *selected != index {
            *selected = index;
            group.mark_changed();
        }
    }
    group.on_hover_cursor(CursorIcon::PointingHand)
}

// ============================================================================================
// 侧栏导航
// ============================================================================================

/// 侧栏分组标题（"观看" / "设置与账号"）：10px 次要色，高 16。
pub fn nav_group(ui: &mut Ui, text: &str) -> Response {
    let palette = palette_of(ui);
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), metrics::GROUP_H),
        Sense::hover(),
    );
    text_left(
        &ui.painter(),
        rect,
        text,
        theme::font(ts::CAPTION),
        palette.text_secondary,
    );
    response
}

/// 侧栏导航项：图标 + 文字；选中时整行亮橙底 + 深棕字（原版 C# 2026-10-22 改版）。
///
/// 默认宽 = 可用宽度、高 [metrics::NAV_H]；两行项（"本地数据 / 平台状态"）用
/// [nav_button_size] 传 [metrics::NAV_H_TALL]。
pub fn nav_button(ui: &mut Ui, glyph: Glyph, text: &str, selected: bool) -> Response {
    let size = Vec2::new(ui.available_width(), metrics::NAV_H);
    nav_button_size(ui, glyph, text, selected, size)
}

/// 指定尺寸的导航项（原版 148x36 / 148x52）。
pub fn nav_button_size(
    ui: &mut Ui,
    glyph: Glyph,
    text: &str,
    selected: bool,
    size: Vec2,
) -> Response {
    nav_button_impl(ui, glyph, text, selected, size, false)
}

/// 导航项变体：透明底 + 左侧 3px 亮橙竖条（另一种"选中条"表达；原版未用，备用）。
pub fn nav_button_bar(ui: &mut Ui, glyph: Glyph, text: &str, selected: bool) -> Response {
    nav_button_impl(
        ui,
        glyph,
        text,
        selected,
        Vec2::new(ui.available_width(), metrics::NAV_H),
        true,
    )
}

fn nav_button_impl(
    ui: &mut Ui,
    glyph: Glyph,
    text: &str,
    selected: bool,
    size: Vec2,
    bar: bool,
) -> Response {
    let palette = palette_of(ui);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let state = WidgetState::resolve(
        true,
        response.hovered(),
        response.is_pointer_button_down_on(),
    );
    let spec = nav_spec(selected, state, &palette);
    let painter = ui.painter();
    let radius = CornerRadius::same(metrics::NAV_RADIUS);
    if bar {
        // 变体：透明底 + 左 3px 亮橙条；文字仍是正文色，避免整行高饱和。
        if selected || state == WidgetState::Hovered {
            painter.rect_filled(
                rect,
                radius,
                if selected {
                    palette.hover
                } else {
                    Color32::TRANSPARENT
                },
            );
        }
        if selected {
            let bar_rect = Rect::from_min_size(
                egui::pos2(rect.left() + 2.0, rect.top() + 7.0),
                Vec2::new(3.0, (rect.height() - 14.0).max(6.0)),
            );
            painter.rect_filled(bar_rect, CornerRadius::same(2), palette.selection);
        }
    } else if spec.fill != Color32::TRANSPARENT {
        painter.rect_filled(rect, radius, spec.fill);
    }
    // 图标：左 11、居中，15x15（原版 DrawGlyph 的比例）。
    let icon_rect = Rect::from_center_size(
        egui::pos2(
            rect.left() + metrics::NAV_ICON_LEFT + metrics::NAV_ICON / 2.0,
            rect.center().y,
        ),
        Vec2::splat(metrics::NAV_ICON),
    );
    let icon_color = if bar {
        if selected {
            palette.selection
        } else {
            spec.text
        }
    } else {
        spec.text
    };
    paint_glyph(painter, icon_rect, glyph, icon_color, metrics::GLYPH_STROKE);
    // 文字：左 31，选中用粗体（原版 EmphasisFont）。
    let text_rect = Rect::from_min_max(
        egui::pos2(rect.left() + metrics::NAV_TEXT_LEFT, rect.top() + 2.0),
        egui::pos2(rect.right() - 6.0, rect.bottom() - 2.0),
    );
    let font = if selected {
        theme::font_bold(ts::NAV)
    } else {
        theme::font(ts::NAV)
    };
    text_left(
        &painter.with_clip_rect(rect.shrink(2.0)),
        text_rect,
        text,
        font,
        if bar && selected {
            palette.text
        } else {
            spec.text
        },
    );
    response.on_hover_cursor(CursorIcon::PointingHand)
}

// ============================================================================================
// 文本标签
// ============================================================================================

/// 正文（12px，正文色）。
pub fn label(ui: &mut Ui, text: &str) -> Response {
    let palette = palette_of(ui);
    text_line(ui, text, theme::font(ts::BODY), palette.text, lh::BODY)
}

/// 次要说明（12px，次要色）。
pub fn subtitle(ui: &mut Ui, text: &str) -> Response {
    let palette = palette_of(ui);
    text_line(
        ui,
        text,
        theme::font(ts::BODY),
        palette.text_secondary,
        lh::BODY,
    )
}

/// 辅助行（11px，次要色）。
pub fn hint(ui: &mut Ui, text: &str) -> Response {
    let palette = palette_of(ui);
    text_line(
        ui,
        text,
        theme::font(ts::SMALL),
        palette.text_secondary,
        lh::SMALL,
    )
}

/// 小字（10px，次要色）。
pub fn caption(ui: &mut Ui, text: &str) -> Response {
    let palette = palette_of(ui);
    text_line(
        ui,
        text,
        theme::font(ts::CAPTION),
        palette.text_secondary,
        lh::GROUP,
    )
}

/// 页面标题（23px 粗体，正文色）。
pub fn title(ui: &mut Ui, text: &str) -> Response {
    let palette = palette_of(ui);
    text_line(
        ui,
        text,
        theme::font_bold(ts::TITLE),
        palette.text,
        metrics::TITLE_H,
    )
}

/// 区块标题（14px 粗体，正文色；原版 "B站 收藏 6 个 · 本地收藏"）。
pub fn section_header(ui: &mut Ui, text: &str) -> Response {
    let palette = palette_of(ui);
    text_line(
        ui,
        text,
        theme::font_bold(ts::HEADING),
        palette.text,
        lh::HEADING,
    )
}

/// 表单字段标签行：左 11px 标签 + 右对齐 10px 范围提示（原版 launch 卡的"画质：/本次"）。
pub fn field_label(ui: &mut Ui, label: &str, scope: Option<&str>) -> Response {
    let palette = palette_of(ui);
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), metrics::LAUNCH_LABEL_H),
        Sense::hover(),
    );
    let painter = ui.painter();
    text_left(
        &painter,
        rect,
        label,
        theme::font(ts::SMALL),
        palette.text_secondary,
    );
    if let Some(scope) = scope {
        text_right(
            &painter,
            rect,
            scope,
            theme::font(ts::CAPTION),
            palette.text_secondary,
        );
    }
    response
}

/// 页面标题行：左侧 23px 粗体标题 + 紧随其后的 12px 次要副标题。
///
/// 返回 (标题响应, 副标题响应)；行高 [metrics::TITLE_H]。
pub fn page_header(ui: &mut Ui, title: &str, subtitle: &str) -> (Response, Response) {
    let palette = palette_of(ui);
    let (rect, title_response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), metrics::TITLE_H),
        Sense::hover(),
    );
    let painter = ui.painter();
    let title_galley = painter.layout_no_wrap(
        title.to_owned(),
        theme::font_bold(ts::TITLE),
        Color32::PLACEHOLDER,
    );
    let title_pos = egui::pos2(rect.left(), rect.center().y - title_galley.size().y / 2.0);
    paint_galley(&painter, &title_galley, title_pos, palette.text);
    let sub_left = rect.left() + title_galley.size().x + metrics::TITLE_GAP;
    let sub_rect = Rect::from_min_max(
        egui::pos2(sub_left, rect.top()),
        egui::pos2(rect.right(), rect.bottom()),
    );
    let sub_galley = painter.layout_no_wrap(
        subtitle.to_owned(),
        theme::font(ts::SUBTITLE),
        Color32::PLACEHOLDER,
    );
    let sub_pos = egui::pos2(
        sub_rect.left(),
        sub_rect.center().y - sub_galley.size().y / 2.0,
    );
    paint_galley(&painter, &sub_galley, sub_pos, palette.text_secondary);
    let subtitle_response =
        ui.interact(sub_rect, title_response.id.with("subtitle"), Sense::hover());
    (title_response, subtitle_response)
}

/// 自定义字号/字重的一行文字（用于品牌字 19px、卡片小标题等不常见字号）。
///
/// 左对齐 + 垂直居中；size 决定占位，left_pad 是文字距左侧的间距。
pub fn text_at(
    ui: &mut Ui,
    text: &str,
    font: FontId,
    color: Color32,
    size: Vec2,
    left_pad: f32,
) -> Response {
    let galley = galley(ui, text, font);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let content = Rect::from_min_max(
        egui::pos2(rect.left() + left_pad, rect.top()),
        egui::pos2(rect.right(), rect.bottom()),
    );
    let pos = egui::pos2(content.left(), content.center().y - galley.size().y / 2.0);
    paint_galley(&ui.painter().with_clip_rect(rect), &galley, pos, color);
    response
}

fn text_line(ui: &mut Ui, text: &str, font: FontId, color: Color32, height: f32) -> Response {
    let galley = galley(ui, text, font);
    let width = galley.size().x;
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let pos = egui::pos2(rect.left(), rect.center().y - galley.size().y / 2.0);
    paint_galley(&ui.painter(), &galley, pos, color);
    response
}

/// 分组标题左侧的强调色竖条（原版 ModernSectionBar：3x30 的 Accent 竖条，设置对话框用）。
pub fn section_bar(ui: &mut Ui, height: f32) -> Response {
    let palette = palette_of(ui);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(3.0, height), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(1), palette.accent);
    response
}

/// 一条水平分隔线（1px 边框色，上下各留 4px）。
pub fn divider(ui: &mut Ui) -> Response {
    let palette = palette_of(ui);
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), space::SM + 1.0),
        Sense::hover(),
    );
    theme::divider_line(&ui.painter(), rect, palette.border);
    response
}

// ============================================================================================
// 卡片 / 面板
// ============================================================================================

/// 圆角卡片面板（原版 WorkbenchPanel.IsCard = true：card 底 + 1px border + 圆角 10）。
///
/// 默认占满可用宽度；内部 padding 16（原版 launch 卡 16）、无阴影（原版卡片是纯描边）。
#[derive(Clone, Copy, Debug)]
pub struct Card {
    fill: Option<Color32>,
    stroke: Option<Color32>,
    radius: u8,
    padding: f32,
    outer: f32,
    shadow: bool,
}

impl Default for Card {
    fn default() -> Self {
        Self {
            fill: None,
            stroke: None,
            radius: metrics::PANEL_RADIUS,
            padding: space::LG,
            outer: 0.0,
            shadow: false,
        }
    }
}

impl Card {
    /// 新建卡片面板。
    pub fn new() -> Self {
        Self::default()
    }

    /// 覆盖填充色（默认 card）。
    pub fn fill(mut self, color: Color32) -> Self {
        self.fill = Some(color);
        self
    }

    /// 覆盖描边色（默认 border）。
    pub fn stroke(mut self, color: Color32) -> Self {
        self.stroke = Some(color);
        self
    }

    /// 覆盖圆角（默认 10）。
    pub fn radius(mut self, radius: u8) -> Self {
        self.radius = radius;
        self
    }

    /// 覆盖内边距（默认 16）。
    pub fn padding(mut self, padding: f32) -> Self {
        self.padding = padding;
        self
    }

    /// 覆盖外边距（默认 0）。
    pub fn outer_margin(mut self, outer: f32) -> Self {
        self.outer = outer;
        self
    }

    /// 是否画卡片阴影（原版不画；浮层/对话框可开）。
    pub fn shadow(mut self, shadow: bool) -> Self {
        self.shadow = shadow;
        self
    }

    /// 渲染卡片内容。
    pub fn show<R>(self, ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
        let palette = palette_of(ui);
        let width = ui.available_width();
        let mut frame = egui::Frame::new()
            .fill(self.fill.unwrap_or(palette.card))
            .stroke(Stroke::new(1.0_f32, self.stroke.unwrap_or(palette.border)))
            .corner_radius(CornerRadius::same(self.radius))
            .inner_margin(egui::Margin::same(self.padding.round() as i8))
            .outer_margin(egui::Margin::same(self.outer.round() as i8));
        if self.shadow {
            frame = frame.shadow(palette.card_shadow());
        }
        frame.show(ui, |ui| {
            let inner_width = (width - self.padding * 2.0 - 2.0).max(0.0);
            ui.set_min_width(inner_width);
            add(ui)
        })
    }
}

/// 圆角卡片面板（等价 Card::new().show）。
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    Card::new().show(ui, add)
}

// ============================================================================================
// 输入框
// ============================================================================================

/// 表单控件（输入框/下拉框）的描边规则：错误 1.6px danger、聚焦 1.6px accent、
/// 悬停 1px border_hover、否则 1px border。
///
/// 抽成纯函数是为了让"输入焦点/错误态"能被单元测试直接断言（原版 ModernInput.OnPaint）。
pub fn field_stroke(error: bool, focused: bool, hovered: bool, palette: &Palette) -> Stroke {
    if error {
        Stroke::new(1.6_f32, palette.danger)
    } else if focused {
        Stroke::new(1.6_f32, palette.accent)
    } else if hovered {
        Stroke::new(1.0_f32, palette.border_hover)
    } else {
        Stroke::new(1.0_f32, palette.border)
    }
}

/// 输入框构建器（原版 ModernInput：card 底 + 圆角 5 + 1px 描边；聚焦时 accent 1.6px）。
///
/// 支持 hover 描边、焦点强调、错误态（danger 1.6px）、左侧放大镜图标与 hint。
pub struct TextInput<'a, 'h> {
    text: &'a mut String,
    hint: &'h str,
    width: Option<f32>,
    height: f32,
    error: bool,
    icon: Option<Glyph>,
}

impl<'a, 'h> TextInput<'a, 'h> {
    /// 绑定到一段可变字符串。
    pub fn new(text: &'a mut String) -> Self {
        Self {
            text,
            hint: "",
            width: None,
            height: metrics::INPUT_H,
            error: false,
            icon: None,
        }
    }

    /// 占位提示文字。
    pub fn hint(mut self, hint: &'h str) -> Self {
        self.hint = hint;
        self
    }

    /// 固定宽度（默认占满可用宽度）。
    pub fn width(mut self, width: f32) -> Self {
        self.width = Some(width);
        self
    }

    /// 高度（默认 34；launch 卡用 38）。
    pub fn height(mut self, height: f32) -> Self {
        self.height = height;
        self
    }

    /// 错误态：描边改 danger 1.6px。
    pub fn error(mut self, error: bool) -> Self {
        self.error = error;
        self
    }

    /// 左侧图标（例如搜索放大镜）。
    pub fn icon(mut self, glyph: Glyph) -> Self {
        self.icon = Some(glyph);
        self
    }

    /// 渲染输入框，返回内部 TextEdit 的响应（可用 has_focus / lost_focus 等）。
    pub fn show(self, ui: &mut Ui) -> Response {
        let palette = palette_of(ui);
        let width = self.width.unwrap_or_else(|| ui.available_width());
        let (rect, outer) = ui.allocate_exact_size(Vec2::new(width, self.height), Sense::hover());
        let radius = CornerRadius::same(metrics::INPUT_RADIUS);
        let painter = ui.painter();
        painter.rect_filled(rect, radius, palette.card);

        let icon_space = if self.icon.is_some() {
            metrics::NAV_ICON + space::XS
        } else {
            0.0
        };
        let font = theme::font(ts::BODY);
        let inner_left = rect.left() + metrics::INPUT_PAD_X + icon_space;
        let inner_right = rect.right() - metrics::INPUT_PAD_X;
        let row = row_height(ui, &font);
        let text_rect = Rect::from_center_size(
            egui::pos2((inner_left + inner_right) / 2.0, rect.center().y),
            Vec2::new((inner_right - inner_left).max(10.0), row.max(16.0)),
        );
        let edit = egui::TextEdit::singleline(self.text)
            .frame(false)
            .font(font)
            .text_color(palette.text)
            .hint_text(
                egui::RichText::new(self.hint)
                    .font(theme::font(ts::BODY))
                    .color(palette.text_secondary),
            )
            .desired_width(text_rect.width())
            .margin(egui::Margin::ZERO);
        let edit_response = ui.put(text_rect, edit);
        let focused = edit_response.has_focus();
        let hovered = outer.hovered() || edit_response.hovered();

        if let Some(glyph) = self.icon {
            let icon_rect = Rect::from_center_size(
                egui::pos2(
                    rect.left() + metrics::INPUT_PAD_X + metrics::NAV_ICON / 2.0,
                    rect.center().y,
                ),
                Vec2::splat(metrics::NAV_ICON),
            );
            paint_glyph(
                ui.painter(),
                icon_rect,
                glyph,
                if focused {
                    palette.accent
                } else {
                    palette.text_secondary
                },
                metrics::GLYPH_STROKE,
            );
        }
        // 描边最后画，保证压在文字/图标之上（内侧 1px 不会挡字）。
        let (stroke_color, stroke_width) = if self.error {
            (palette.danger, 1.6)
        } else if focused {
            (palette.accent, 1.6)
        } else if hovered {
            (palette.border_hover, 1.0)
        } else {
            (palette.border, 1.0_f32)
        };
        ui.painter().rect_stroke(
            rect,
            radius,
            Stroke::new(stroke_width, stroke_color),
            StrokeKind::Inside,
        );
        // 返回内部 TextEdit 的响应（has_focus / changed 可用），但把 rect 扩成整个
        // 输入框外框，调用方拿它做布局与命中区域断言时看到的是输入框本体。
        let mut response = edit_response;
        response.rect = rect;
        response
    }
}

/// 便捷输入框：占满宽度、高 34。
pub fn input(ui: &mut Ui, text: &mut String, hint: &str) -> Response {
    input_with_width(ui, text, hint, ui.available_width())
}

/// 指定宽度的便捷输入框。
pub fn input_with_width(ui: &mut Ui, text: &mut String, hint: &str, width: f32) -> Response {
    TextInput::new(text).width(width).hint(hint).show(ui)
}

/// 带放大镜图标的搜索输入框（占满宽度、高 34）。
pub fn input_search(ui: &mut Ui, text: &mut String, hint: &str) -> Response {
    let width = ui.available_width();
    TextInput::new(text)
        .width(width)
        .hint(hint)
        .icon(Glyph::Search)
        .show(ui)
}

// ============================================================================================
// 下拉框
// ============================================================================================

/// 下拉框（原版 ModernComboBox：高 25、圆角 8、card 底、右侧 V 形箭头，悬停换描边色）。
///
/// 列表行高 22；选中行亮橙底 + 深棕字。点击行更新 selected 并 mark_changed()。
pub fn dropdown(
    ui: &mut Ui,
    id_salt: impl std::hash::Hash,
    selected: &mut usize,
    items: &[&str],
    width: f32,
) -> Response {
    dropdown_sized(ui, id_salt, selected, items, width, metrics::DROPDOWN_H)
}

/// 指定高度的下拉框（原版 launch 卡是 25 / 27）。
pub fn dropdown_sized(
    ui: &mut Ui,
    id_salt: impl std::hash::Hash,
    selected: &mut usize,
    items: &[&str],
    width: f32,
    height: f32,
) -> Response {
    let palette = palette_of(ui);
    let id = ui.make_persistent_id(id_salt);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    let open = ui.data(|data| data.get_temp::<bool>(id).unwrap_or(false));
    if response.clicked() {
        ui.data_mut(|data| data.insert_temp(id, !open));
    }
    let open = ui.data(|data| data.get_temp::<bool>(id).unwrap_or(false));

    // 闭合态外观
    let painter = ui.painter();
    let radius = CornerRadius::same(metrics::DROPDOWN_RADIUS);
    painter.rect_filled(rect, radius, palette.card);
    let text_rect = Rect::from_min_max(
        egui::pos2(rect.left() + metrics::FIELD_PAD_X, rect.top()),
        egui::pos2(rect.right() - metrics::DROPDOWN_CHEVRON, rect.bottom()),
    );
    let selected_text = items.get(*selected).copied().unwrap_or("");
    text_left(
        &painter.with_clip_rect(text_rect),
        text_rect,
        selected_text,
        theme::font(ts::BODY),
        palette.text,
    );
    let chevron_rect = Rect::from_min_size(
        egui::pos2(rect.right() - metrics::DROPDOWN_CHEVRON, rect.top()),
        Vec2::new(metrics::DROPDOWN_CHEVRON, rect.height()),
    );
    paint_glyph(
        painter,
        chevron_rect,
        Glyph::ChevronDown,
        palette.text_secondary,
        1.6,
    );
    painter.rect_stroke(
        rect,
        radius,
        Stroke::new(
            1.0_f32,
            if response.hovered() || open {
                palette.border_hover
            } else {
                palette.border
            },
        ),
        StrokeKind::Inside,
    );

    if open {
        dropdown_popup(ui, id, rect, selected, items, &palette);
    }
    response.on_hover_cursor(CursorIcon::PointingHand)
}

/// 动态选项版本（画质 / 缓冲 / 平台列表等运行时拼出来的数组）。
pub fn dropdown_owned(
    ui: &mut Ui,
    id_salt: impl std::hash::Hash,
    selected: &mut usize,
    items: &[String],
    width: f32,
) -> Response {
    let borrowed: Vec<&str> = items.iter().map(String::as_str).collect();
    dropdown(ui, id_salt, selected, &borrowed, width)
}

/// 动态选项 + 指定高度的下拉框。
pub fn dropdown_owned_sized(
    ui: &mut Ui,
    id_salt: impl std::hash::Hash,
    selected: &mut usize,
    items: &[String],
    width: f32,
    height: f32,
) -> Response {
    let borrowed: Vec<&str> = items.iter().map(String::as_str).collect();
    dropdown_sized(ui, id_salt, selected, &borrowed, width, height)
}

fn dropdown_popup(
    ui: &mut Ui,
    id: Id,
    anchor: Rect,
    selected: &mut usize,
    items: &[&str],
    palette: &Palette,
) {
    let context = ui.ctx().clone();
    let popup_id = id.with("popup");
    let mut chosen: Option<usize> = None;
    let area = egui::Area::new(popup_id)
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(anchor.left(), anchor.bottom() + 2.0))
        .show(&context, |ui| {
            let frame = egui::Frame::new()
                .fill(palette.popup)
                .stroke(Stroke::new(1.0_f32, palette.border))
                .corner_radius(CornerRadius::same(metrics::DROPDOWN_RADIUS))
                .inner_margin(egui::Margin::symmetric(4, 4))
                .shadow(palette.card_shadow());
            frame.show(ui, |ui| {
                ui.set_min_width((anchor.width() - 8.0).max(40.0));
                egui::ScrollArea::vertical()
                    .id_salt(id.with("popup-scroll"))
                    .max_height(260.0)
                    .show(ui, |ui| {
                        for (index, item) in items.iter().enumerate() {
                            let (row_rect, row_response) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width(), metrics::DROPDOWN_ROW),
                                Sense::click(),
                            );
                            let is_selected = index == *selected;
                            let row_painter = ui.painter();
                            if is_selected {
                                row_painter.rect_filled(
                                    row_rect,
                                    CornerRadius::same(4),
                                    palette.selection,
                                );
                            } else if row_response.hovered() {
                                row_painter.rect_filled(
                                    row_rect,
                                    CornerRadius::same(4),
                                    palette.hover,
                                );
                            }
                            text_left(
                                &row_painter.with_clip_rect(row_rect),
                                row_rect.shrink2(Vec2::new(metrics::FIELD_PAD_X, 0.0)),
                                item,
                                theme::font(ts::BODY),
                                if is_selected {
                                    palette.selection_text
                                } else {
                                    palette.text
                                },
                            );
                            if row_response.clicked() {
                                chosen = Some(index);
                            }
                        }
                    });
            });
        });
    let popup_rect = area.response.rect;
    let pointer = context.input(|input| input.pointer.interact_pos());
    let clicked_outside = context.input(|input| input.pointer.any_click())
        && pointer.map_or(false, |position| {
            !popup_rect.contains(position) && !anchor.contains(position)
        });
    let escape = context.input(|input| input.key_pressed(egui::Key::Escape));
    if let Some(index) = chosen {
        *selected = index;
        context.data_mut(|data| data.insert_temp::<bool>(id, false));
    } else if clicked_outside || escape {
        context.data_mut(|data| data.insert_temp::<bool>(id, false));
    }
}

// ============================================================================================
// 复选框
// ============================================================================================

/// 扁平复选框（原版 ModernCheckBox：16px 圆角方块 + 亮橙勾选 + 深棕对勾）。
pub fn checkbox(ui: &mut Ui, text: &str, checked: &mut bool) -> Response {
    checkbox_enabled(ui, text, checked, true)
}

/// 带 enabled 的复选框：禁用时勾选块用 TextDisabled、未勾选描边 Border、文字 TextSecondary
/// （见 ui-spec §3.4）。
pub fn checkbox_enabled(ui: &mut Ui, text: &str, checked: &mut bool, enabled: bool) -> Response {
    let palette = palette_of(ui);
    let font = theme::font(ts::BODY);
    let label_galley = galley(ui, text, font.clone());
    let width = metrics::CHECKBOX + space::SM + label_galley.size().x;
    let (rect, mut response) =
        ui.allocate_exact_size(Vec2::new(width, metrics::CHECKBOX_ROW), Sense::click());
    if enabled && response.clicked() {
        *checked = !*checked;
        response.mark_changed();
    }
    let painter = ui.painter();
    let box_rect = Rect::from_center_size(
        egui::pos2(rect.left() + metrics::CHECKBOX / 2.0, rect.center().y),
        Vec2::splat(metrics::CHECKBOX),
    );
    let radius = CornerRadius::same(metrics::CHECKBOX_RADIUS);
    if *checked {
        painter.rect_filled(
            box_rect,
            radius,
            if enabled {
                palette.selection
            } else {
                palette.text_disabled
            },
        );
        paint_glyph(
            painter,
            box_rect.shrink(1.0),
            Glyph::Check,
            palette.selection_text,
            1.9,
        );
    } else {
        painter.rect_filled(box_rect, radius, palette.card);
        let border = if !enabled {
            palette.border
        } else if response.hovered() {
            palette.border_hover
        } else {
            palette.text_disabled
        };
        painter.rect_stroke(
            box_rect,
            radius,
            Stroke::new(1.2_f32, border),
            StrokeKind::Inside,
        );
    }
    let text_rect = Rect::from_min_max(
        egui::pos2(box_rect.right() + space::SM, rect.top()),
        egui::pos2(rect.right(), rect.bottom()),
    );
    let pos = egui::pos2(
        text_rect.left(),
        text_rect.center().y - label_galley.size().y / 2.0,
    );
    paint_galley(
        &painter.with_clip_rect(rect),
        &label_galley,
        pos,
        if enabled {
            palette.text
        } else {
            palette.text_secondary
        },
    );
    if enabled {
        response.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        response
    }
}

// ============================================================================================
// 图标 glyph 表（路径比例照抄 C# WorkbenchNavigationButton.DrawGlyph / WorkbenchGlyph）
// ============================================================================================

/// 矢量图标集合。比例取自原版 C# 的 DrawGlyph（归一化到 0..1 的方框）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyph {
    /// 我的收藏：五角星。
    Favorites,
    /// 最近播放：圆形时钟 + 逆时针箭头。
    History,
    /// 查找主播：放大镜。
    Search,
    /// 观看会话：显示器 + 播放三角。
    Sessions,
    /// 播放器与画质：显示器（无三角）。
    Players,
    /// 弹幕与 SC：对话气泡。
    Danmaku,
    /// 账号与登录：人像。
    Accounts,
    /// 本地数据 / 平台状态：三层抽屉。
    Data,
    /// 品牌图标 / 会话头像占位：显示器 + 播放三角。
    MonitorPlay,
    /// 本地播放器：抽屉 + 指示灯。
    Drive,
    /// 播放三角。
    Play,
    /// 胶片（备用）。
    Film,
    /// 齿轮（设置）。
    Settings,
    /// 下拉箭头。
    ChevronDown,
    /// 右箭头。
    ChevronRight,
    /// 对勾。
    Check,
    /// 关闭叉。
    Close,
    /// 最小化横线。
    Min,
    /// 最大化方框。
    Max,
}

/// 画一个矢量图标到 rect（取短边为基准，居中）。
///
/// stroke_width 是逻辑 px；原版 GDI+ 用 1.25*scale，这里默认传
/// [metrics::GLYPH_STROKE]。
pub fn glyph(painter: &egui::Painter, rect: Rect, glyph: Glyph, color: Color32, stroke_width: f32) {
    paint_glyph(painter, rect, glyph, color, stroke_width);
}

pub(crate) fn paint_glyph(
    painter: &egui::Painter,
    rect: Rect,
    glyph: Glyph,
    color: Color32,
    stroke_width: f32,
) {
    let stroke = Stroke::new(stroke_width, color);
    let size = rect.width().min(rect.height());
    let x = rect.center().x - size / 2.0;
    let y = rect.center().y - size / 2.0;
    let p = |fx: f32, fy: f32| egui::pos2(x + fx * size, y + fy * size);
    let c = |fx: f32, fy: f32| egui::pos2(x + fx * size, y + fy * size);
    match glyph {
        Glyph::Favorites => {
            let mut points = Vec::with_capacity(10);
            for index in 0..10 {
                let angle = -FRAC_PI_2 + index as f32 * std::f32::consts::PI / 5.0;
                let radius = size * if index % 2 == 0 { 0.43 } else { 0.20 };
                points.push(egui::pos2(
                    x + size / 2.0 + angle.cos() * radius,
                    y + size / 2.0 + angle.sin() * radius,
                ));
            }
            painter.add(egui::Shape::closed_line(points, stroke));
        }
        Glyph::History => {
            let center = c(0.51, 0.52);
            let radius = size * 0.36;
            let points = arc_points(center, radius, radius, 205.0, 515.0, 28);
            painter.add(egui::Shape::line(points, stroke));
            painter.add(egui::Shape::line(
                vec![p(0.09, 0.15), p(0.10, 0.40), p(0.33, 0.39)],
                stroke,
            ));
            painter.add(egui::Shape::line(
                vec![c(0.51, 0.29), c(0.51, 0.52), c(0.67, 0.60)],
                stroke,
            ));
        }
        Glyph::Search => {
            painter.circle_stroke(c(0.40, 0.40), size * 0.30, stroke);
            painter.line_segment([p(0.61, 0.61), p(0.91, 0.91)], stroke);
        }
        Glyph::Sessions => {
            painter.rect_stroke(
                Rect::from_min_max(p(0.08, 0.12), p(0.92, 0.72)),
                CornerRadius::same(1),
                stroke,
                StrokeKind::Inside,
            );
            painter.line_segment([p(0.50, 0.73), p(0.50, 0.89)], stroke);
            painter.line_segment([p(0.30, 0.90), p(0.70, 0.90)], stroke);
            painter.add(egui::Shape::closed_line(
                vec![p(0.41, 0.27), p(0.41, 0.58), p(0.66, 0.42)],
                stroke,
            ));
        }
        Glyph::Players => {
            painter.rect_stroke(
                Rect::from_min_max(p(0.08, 0.12), p(0.92, 0.72)),
                CornerRadius::same(1),
                stroke,
                StrokeKind::Inside,
            );
            painter.line_segment([p(0.50, 0.73), p(0.50, 0.89)], stroke);
            painter.line_segment([p(0.30, 0.90), p(0.70, 0.90)], stroke);
        }
        Glyph::Danmaku => {
            painter.rect_stroke(
                Rect::from_min_max(p(0.10, 0.10), p(0.90, 0.70)),
                CornerRadius::same(2),
                stroke,
                StrokeKind::Inside,
            );
            painter.add(egui::Shape::line(
                vec![p(0.25, 0.71), p(0.25, 0.91), p(0.47, 0.71)],
                stroke,
            ));
            painter.line_segment([p(0.26, 0.31), p(0.74, 0.31)], stroke);
            painter.line_segment([p(0.26, 0.49), p(0.59, 0.49)], stroke);
        }
        Glyph::Accounts => {
            painter.circle_stroke(c(0.50, 0.23), size * 0.18, stroke);
            let center = c(0.50, 0.90);
            let points = arc_points(center, size * 0.38, size * 0.40, 180.0, 360.0, 20);
            painter.add(egui::Shape::line(points, stroke));
        }
        Glyph::Data => {
            for index in 0..3 {
                let top = 0.09 + index as f32 * 0.29;
                painter.rect_stroke(
                    Rect::from_min_max(p(0.11, top), p(0.89, top + 0.20)),
                    CornerRadius::same(1),
                    stroke,
                    StrokeKind::Inside,
                );
                painter.line_segment([p(0.23, top + 0.10), p(0.30, top + 0.10)], stroke);
            }
        }
        Glyph::MonitorPlay => {
            // 品牌图标（WorkbenchGlyph.DrawGlyph）：屏幕 + 底座 + 播放三角。
            painter.rect_stroke(
                Rect::from_min_max(p(0.08, 0.10), p(0.92, 0.70)),
                CornerRadius::same(1),
                stroke,
                StrokeKind::Inside,
            );
            painter.line_segment([p(0.50, 0.71), p(0.50, 0.87)], stroke);
            painter.line_segment([p(0.30, 0.90), p(0.70, 0.90)], stroke);
            painter.add(egui::Shape::closed_line(
                vec![p(0.42, 0.24), p(0.42, 0.56), p(0.64, 0.40)],
                stroke,
            ));
        }
        Glyph::Drive => {
            // 本地播放器：抽屉 + 一道横线 + 指示灯。
            painter.rect_stroke(
                Rect::from_min_max(p(0.08, 0.22), p(0.92, 0.78)),
                CornerRadius::same(1),
                stroke,
                StrokeKind::Inside,
            );
            painter.line_segment([p(0.22, 0.50), p(0.44, 0.50)], stroke);
            painter.circle_filled(p(0.68, 0.50), size * 0.06, color);
        }
        Glyph::Play => {
            painter.add(egui::Shape::closed_line(
                vec![p(0.32, 0.18), p(0.32, 0.82), p(0.80, 0.50)],
                stroke,
            ));
        }
        Glyph::Film => {
            painter.rect_stroke(
                Rect::from_min_max(p(0.10, 0.18), p(0.90, 0.82)),
                CornerRadius::same(2),
                stroke,
                StrokeKind::Inside,
            );
            for index in 0..3 {
                let fy = 0.26 + index as f32 * 0.20;
                painter.line_segment([p(0.18, fy), p(0.24, fy)], stroke);
                painter.line_segment([p(0.76, fy), p(0.82, fy)], stroke);
            }
        }
        Glyph::Settings => {
            painter.circle_stroke(c(0.50, 0.50), size * 0.18, stroke);
            for index in 0..8 {
                let angle = index as f32 * TAU / 8.0;
                let inner = c(0.50 + angle.cos() * 0.26, 0.50 + angle.sin() * 0.26);
                let outer = c(0.50 + angle.cos() * 0.40, 0.50 + angle.sin() * 0.40);
                painter.line_segment([inner, outer], stroke);
            }
        }
        Glyph::ChevronDown => {
            let center = c(0.5, 0.5);
            painter.add(egui::Shape::line(
                vec![
                    center + egui::vec2(-4.0, -2.0),
                    center + egui::vec2(0.0, 2.0),
                    center + egui::vec2(4.0, -2.0),
                ],
                Stroke::new(stroke_width.max(1.6), color),
            ));
        }
        Glyph::ChevronRight => {
            let center = c(0.5, 0.5);
            painter.add(egui::Shape::line(
                vec![
                    center + egui::vec2(-2.0, -4.0),
                    center + egui::vec2(2.0, 0.0),
                    center + egui::vec2(-2.0, 4.0),
                ],
                Stroke::new(stroke_width.max(1.6), color),
            ));
        }
        Glyph::Check => {
            painter.add(egui::Shape::line(
                vec![p(0.18, 0.52), p(0.44, 0.78), p(0.82, 0.22)],
                Stroke::new(stroke_width.max(1.9), color),
            ));
        }
        Glyph::Close => {
            painter.line_segment([p(0.15, 0.15), p(0.85, 0.85)], stroke);
            painter.line_segment([p(0.85, 0.15), p(0.15, 0.85)], stroke);
        }
        Glyph::Min => {
            painter.line_segment([p(0.10, 0.50), p(0.90, 0.50)], stroke);
        }
        Glyph::Max => {
            painter.rect_stroke(
                Rect::from_min_max(p(0.12, 0.12), p(0.88, 0.88)),
                CornerRadius::ZERO,
                stroke,
                StrokeKind::Inside,
            );
        }
    }
}

/// 生成椭圆弧上的点（角度制，0 度 = 3 点钟方向，顺时针为正，与 GDI+ 一致）。
fn arc_points(
    center: egui::Pos2,
    radius_x: f32,
    radius_y: f32,
    start_deg: f32,
    end_deg: f32,
    segments: usize,
) -> Vec<egui::Pos2> {
    let mut points = Vec::with_capacity(segments + 1);
    for index in 0..=segments {
        let t = index as f32 / segments as f32;
        let angle = (start_deg + (end_deg - start_deg) * t).to_radians();
        points.push(egui::pos2(
            center.x + angle.cos() * radius_x,
            center.y + angle.sin() * radius_y,
        ));
    }
    points
}

/// 图标控件：分配 size x size 的矩形并画 glyph，返回响应。
pub fn glyph_widget(ui: &mut Ui, glyph: Glyph, size: f32, color: Color32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    paint_glyph(ui.painter(), rect, glyph, color, metrics::GLYPH_STROKE);
    response
}

// ============================================================================================
// 标题栏红黄绿圆钮（原版 TrafficDot）
// ============================================================================================

/// 圆钮种类（原版 TrafficDot(kind)）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DotKind {
    /// 关闭：红色 #FF5F57。
    Close,
    /// 最小化：黄色 #FEBC2E。
    Min,
    /// 最大化/还原：绿色 #28C840。
    Max,
}

impl DotKind {
    /// 圆钮填充色（原版 Color.FromArgb 常量）。
    pub const fn fill(self) -> Color32 {
        match self {
            Self::Close => Color32::from_rgb(0xFF, 0x5F, 0x57),
            Self::Min => Color32::from_rgb(0xFE, 0xBC, 0x2E),
            Self::Max => Color32::from_rgb(0x28, 0xC8, 0x40),
        }
    }

    /// 圆钮内符号色（原版 #362620 深棕）。
    pub const fn symbol(self) -> Color32 {
        Color32::from_rgb(0x36, 0x26, 0x20)
    }
}

/// 标题栏圆钮：30x30 热区、18px 圆、悬停提亮 16%、8x8 深棕符号。
///
/// restore = true 时最大化钮画"还原"两层方框（原版 TrafficDot.Restore）。
pub fn traffic_dot(ui: &mut Ui, kind: DotKind, restore: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(metrics::DOT_HIT), Sense::click());
    let painter = ui.painter();
    let fill = if response.hovered() {
        theme::blend(kind.fill(), Color32::WHITE, 0.16)
    } else {
        kind.fill()
    };
    let diameter = metrics::DOT
        .min(rect.width() - 2.0)
        .min(rect.height() - 2.0)
        .max(4.0);
    painter.circle_filled(rect.center(), diameter / 2.0, fill);
    let symbol = Rect::from_center_size(rect.center(), Vec2::splat(8.0));
    let stroke = Stroke::new(1.4_f32, kind.symbol());
    match kind {
        DotKind::Close => {
            painter.line_segment([symbol.left_top(), symbol.right_bottom()], stroke);
            painter.line_segment([symbol.right_top(), symbol.left_bottom()], stroke);
        }
        DotKind::Min => {
            painter.line_segment(
                [
                    egui::pos2(symbol.left(), symbol.center().y),
                    egui::pos2(symbol.right(), symbol.center().y),
                ],
                stroke,
            );
        }
        DotKind::Max => {
            if restore {
                let offset = 2.0;
                let back = Rect::from_min_size(
                    egui::pos2(symbol.left() + offset, symbol.top()),
                    Vec2::new(symbol.width() - offset, symbol.height() - offset),
                );
                painter.rect_stroke(back, CornerRadius::ZERO, stroke, StrokeKind::Inside);
                let front = Rect::from_min_size(
                    egui::pos2(symbol.left(), symbol.top() + offset),
                    Vec2::new(symbol.width() - offset, symbol.height() - offset),
                );
                painter.rect_filled(front, 0.0, fill);
                painter.rect_stroke(front, CornerRadius::ZERO, stroke, StrokeKind::Inside);
            } else {
                painter.rect_stroke(symbol, CornerRadius::ZERO, stroke, StrokeKind::Inside);
            }
        }
    }
    response.on_hover_cursor(CursorIcon::PointingHand)
}

// ============================================================================================
// 徽章 / 标签
// ============================================================================================

/// 徽章种类（原版状态丸 + 平台标签）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BadgeKind {
    /// 直播中：绿色圆点 + 绿字（原版 item.Live = ModernTheme.Success）。
    Live,
    /// 已下播：灰点 + 次要色字。
    Offline,
    /// 信息：强调色字。
    Info,
    /// 成功。
    Success,
    /// 警告。
    Warning,
    /// 错误。
    Danger,
    /// 强调（紫色）。
    Accent,
    /// 平台色（带品牌色圆点）。
    Platform(Color32),
}

impl BadgeKind {
    fn color(self, palette: &Palette) -> Color32 {
        match self {
            Self::Live | Self::Success => palette.success,
            Self::Offline => palette.text_secondary,
            Self::Info | Self::Accent => palette.accent,
            Self::Warning => palette.warning,
            Self::Danger => palette.danger,
            Self::Platform(color) => color,
        }
    }

    fn has_dot(self) -> bool {
        matches!(self, Self::Live | Self::Offline | Self::Platform(_))
    }
}

/// 徽章/状态丸：card 底（原版状态丸 fill = Card）、11px 文字、5px 圆点可选。
pub fn badge(ui: &mut Ui, text: &str, kind: BadgeKind) -> Response {
    let palette = palette_of(ui);
    let color = kind.color(&palette);
    let font = theme::font(ts::PILL);
    let galley = galley(ui, text, font.clone());
    let dot_space = if kind.has_dot() { 5.0 + 6.0 } else { 0.0 };
    let width = (galley.size().x + dot_space + 16.0).max(40.0);
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(width, metrics::BADGE_H), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        CornerRadius::same(metrics::BADGE_RADIUS),
        palette.card,
    );
    if kind.has_dot() {
        let dot = egui::pos2(rect.left() + 8.0 + 2.5, rect.center().y);
        painter.circle_filled(dot, 2.5, color);
    }
    let text_rect = Rect::from_min_max(
        egui::pos2(rect.left() + 8.0 + dot_space, rect.top()),
        egui::pos2(rect.right() - 4.0, rect.bottom()),
    );
    text_left(&painter.with_clip_rect(rect), text_rect, text, font, color);
    response
}

/// 平台标签（平台品牌色 + 品牌色圆点）。
pub fn platform_badge(ui: &mut Ui, platform: crate::core::Platform) -> Response {
    let palette = palette_of(ui);
    badge(
        ui,
        platform.label(),
        BadgeKind::Platform(palette.platform(platform)),
    )
}

// ============================================================================================
// 空状态 / 进度条 / Spinner
// ============================================================================================

/// 空状态：居中标题（13px 粗体）+ 副标题（11px 次要色），占满可用宽度。
pub fn empty_state(ui: &mut Ui, title: &str, subtitle: &str) -> Response {
    let palette = palette_of(ui);
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 56.0), Sense::hover());
    let painter = ui.painter();
    let title_galley = painter.layout_no_wrap(
        title.to_owned(),
        theme::font_bold(ts::CARD_TITLE),
        Color32::PLACEHOLDER,
    );
    let title_pos = egui::pos2(
        rect.center().x - title_galley.size().x / 2.0,
        rect.top() + 10.0,
    );
    paint_galley(&painter, &title_galley, title_pos, palette.text);
    let sub_galley = painter.layout_no_wrap(
        subtitle.to_owned(),
        theme::font(ts::SMALL),
        Color32::PLACEHOLDER,
    );
    let sub_pos = egui::pos2(
        rect.center().x - sub_galley.size().x / 2.0,
        title_pos.y + title_galley.size().y + space::XS,
    );
    paint_galley(&painter, &sub_galley, sub_pos, palette.text_secondary);
    response
}

/// 进度条：轨道 border 色、填充 accent 色、圆角 = 高度一半。
pub fn progress(ui: &mut Ui, fraction: f32, width: f32) -> Response {
    let palette = palette_of(ui);
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(width, metrics::PROGRESS_H), Sense::hover());
    let radius = CornerRadius::same((metrics::PROGRESS_H / 2.0) as u8);
    let painter = ui.painter();
    painter.rect_filled(rect, radius, palette.border);
    let fraction = fraction.clamp(0.0, 1.0);
    if fraction > 0.0 {
        let fill = Rect::from_min_size(
            rect.min,
            Vec2::new(
                (rect.width() * fraction).max(metrics::PROGRESS_H),
                rect.height(),
            ),
        );
        painter.rect_filled(fill, radius, palette.accent);
    }
    response
}

/// 轻量 spinner：accent 色圆环 + 旋转缺口；显示期间持续请求重绘。
pub fn spinner(ui: &mut Ui, size: f32) -> Response {
    let palette = palette_of(ui);
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let time = ui.input(|input| input.time) as f32;
    let center = rect.center();
    let radius = size / 2.0 - 1.5;
    let base = (time * 2.5) % TAU;
    let painter = ui.painter();
    for index in 0..12 {
        let angle = base + index as f32 * TAU / 12.0;
        let alpha = 0.15 + 0.85 * (index as f32 / 12.0);
        let inner = center + egui::vec2(angle.cos(), angle.sin()) * (radius - 3.0);
        let outer = center + egui::vec2(angle.cos(), angle.sin()) * radius;
        painter.line_segment(
            [inner, outer],
            Stroke::new(1.8_f32, theme::with_alpha(palette.accent, alpha)),
        );
    }
    ui.ctx().request_repaint();
    response
}

// ============================================================================================
// 头像
// ============================================================================================

/// 圆形头像：有纹理时按圆形网格贴图；否则画浅琥珀圆底 + 名字首字（原版 DrawAvatar）。
///
/// avatar 必须是正方形；传 painter 方便卡片在自绘流程里直接调用。
pub fn avatar(
    painter: &egui::Painter,
    rect: Rect,
    texture: Option<&egui::TextureHandle>,
    fallback: &str,
    palette: &Palette,
) {
    let center = rect.center();
    let radius = rect.width().min(rect.height()) / 2.0;
    if radius <= 0.5 {
        return;
    }
    if let Some(texture) = texture {
        let mut mesh = egui::Mesh::default();
        mesh.texture_id = texture.id();
        mesh.vertices.push(egui::epaint::Vertex {
            pos: center,
            uv: egui::pos2(0.5, 0.5),
            color: Color32::WHITE,
        });
        let segments = 48usize;
        for index in 0..segments {
            let angle = index as f32 / segments as f32 * TAU - FRAC_PI_2;
            let offset = egui::vec2(angle.cos(), angle.sin());
            mesh.vertices.push(egui::epaint::Vertex {
                pos: center + offset * radius,
                uv: egui::pos2(0.5 + offset.x * 0.5, 0.5 + offset.y * 0.5),
                color: Color32::WHITE,
            });
        }
        for index in 0..segments {
            let current = 1 + index as u32;
            let next = 1 + ((index + 1) % segments) as u32;
            mesh.indices.extend_from_slice(&[0, current, next]);
        }
        painter.add(egui::Shape::mesh(mesh));
        return;
    }
    painter.circle_filled(center, radius, palette.selection_soft);
    let initial = fallback
        .trim()
        .chars()
        .next()
        .map(|character| character.to_string())
        .unwrap_or_else(|| "主".to_owned());
    let color = if palette.dark {
        palette.text
    } else {
        palette.selection_text
    };
    painter.text(
        center,
        Align2::CENTER_CENTER,
        initial,
        theme::font_bold(ts::HEADING),
        color,
    );
}

// ============================================================================================
// 滚动区
// ============================================================================================

/// 竖向滚动区（细滚动条 skin 已在 theme::apply 里配好）。
pub fn scroll_vertical<R>(
    ui: &mut Ui,
    id_salt: impl std::hash::Hash,
    add: impl FnOnce(&mut Ui) -> R,
) -> egui::scroll_area::ScrollAreaOutput<R> {
    egui::ScrollArea::vertical()
        .id_salt(id_salt)
        .auto_shrink([false, false])
        .show(ui, add)
}

/// 横向滚动区。
pub fn scroll_horizontal<R>(
    ui: &mut Ui,
    id_salt: impl std::hash::Hash,
    add: impl FnOnce(&mut Ui) -> R,
) -> egui::scroll_area::ScrollAreaOutput<R> {
    egui::ScrollArea::horizontal()
        .id_salt(id_salt)
        .auto_shrink([false, false])
        .show(ui, add)
}

// ============================================================================================
// 测试：颜色规格 / 尺寸 / 命中区域 / 全控件渲染不 panic
// ============================================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Event, Key, Modifiers, PointerButton, Pos2, RawInput, Rect, Vec2};

    /// 测试用 Context：先绑定内置字体，保证 "live-hub-bold" 字体族存在
    /// （裸 Context::default() 没有它，画粗体文字会 panic）。
    fn test_context() -> egui::Context {
        let context = egui::Context::default();
        theme::install_preview_fonts(&context);
        context
    }

    fn viewport() -> Rect {
        Rect::from_min_size(Pos2::ZERO, Vec2::new(600.0, 400.0))
    }

    fn run_frame(context: &egui::Context, events: Vec<Event>, build: impl FnMut(&egui::Context)) {
        let input = RawInput {
            screen_rect: Some(viewport()),
            events,
            ..Default::default()
        };
        let _ = context.run(input, build);
    }

    fn press_release_events(position: Pos2) -> Vec<Vec<Event>> {
        vec![
            vec![Event::PointerMoved(position)],
            vec![Event::PointerButton {
                pos: position,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::default(),
            }],
            vec![Event::PointerButton {
                pos: position,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::default(),
            }],
        ]
    }

    #[test]
    fn primary_button_three_states_match_original() {
        let palette = Palette::dark();
        let normal = button_spec(ButtonKind::Primary, WidgetState::Normal, &palette);
        assert_eq!(normal.fill, palette.accent);
        assert_eq!(normal.text, palette.on_accent);
        assert_eq!(normal.stroke.width, 0.0);
        let hovered = button_spec(ButtonKind::Primary, WidgetState::Hovered, &palette);
        assert_eq!(hovered.fill, palette.accent_hover);
        let pressed = button_spec(ButtonKind::Primary, WidgetState::Pressed, &palette);
        assert_eq!(pressed.fill, palette.accent_pressed);
        let disabled = button_spec(ButtonKind::Primary, WidgetState::Disabled, &palette);
        assert_eq!(disabled.fill, palette.disabled_fill);
        assert_eq!(disabled.text, palette.text_disabled);
    }

    #[test]
    fn secondary_button_three_states_match_original() {
        let palette = Palette::dark();
        let normal = button_spec(ButtonKind::Secondary, WidgetState::Normal, &palette);
        assert_eq!(normal.fill, palette.card);
        assert_eq!(normal.stroke.color, palette.border);
        let hovered = button_spec(ButtonKind::Secondary, WidgetState::Hovered, &palette);
        assert_eq!(hovered.fill, palette.hover);
        assert_eq!(hovered.stroke.color, palette.border_hover);
        let pressed = button_spec(ButtonKind::Secondary, WidgetState::Pressed, &palette);
        assert_eq!(pressed.fill, palette.border);
        assert_eq!(pressed.text, palette.text);
    }

    #[test]
    fn subtle_and_outline_buttons_match_original() {
        let palette = Palette::dark();
        let subtle = button_spec(ButtonKind::Subtle, WidgetState::Normal, &palette);
        assert_eq!(subtle.fill, Color32::TRANSPARENT);
        assert_eq!(subtle.text, palette.accent);
        let subtle_hover = button_spec(ButtonKind::Subtle, WidgetState::Hovered, &palette);
        assert_eq!(subtle_hover.fill, palette.hover);
        let outline = button_spec(ButtonKind::Outline, WidgetState::Normal, &palette);
        assert_eq!(outline.fill, Color32::TRANSPARENT);
        assert_eq!(outline.stroke.color, palette.border);
        let link = button_spec(ButtonKind::Link, WidgetState::Hovered, &palette);
        assert_eq!(link.fill, Color32::TRANSPARENT);
        assert_eq!(link.text, palette.accent);
    }

    #[test]
    fn tab_and_nav_selection_match_original() {
        let palette = Palette::dark();
        let selected = tab_spec(true, WidgetState::Normal, &palette);
        assert_eq!(selected.fill, palette.selection);
        assert_eq!(selected.text, palette.selection_text);
        let selected_hover = tab_spec(true, WidgetState::Hovered, &palette);
        assert_eq!(selected_hover.fill, palette.selection_hover);
        let unselected = tab_spec(false, WidgetState::Normal, &palette);
        assert_eq!(unselected.fill, Color32::TRANSPARENT);
        assert_eq!(unselected.text, palette.text_secondary);
        let nav_selected = nav_spec(true, WidgetState::Normal, &palette);
        assert_eq!(nav_selected.fill, palette.selection);
        assert_eq!(nav_selected.text, palette.selection_text);
        let nav_hover = nav_spec(false, WidgetState::Hovered, &palette);
        assert_eq!(nav_hover.fill, palette.hover);
        assert_eq!(nav_hover.text, palette.text);
        let nav_disabled = nav_spec(true, WidgetState::Disabled, &palette);
        assert_eq!(nav_disabled.fill, Color32::TRANSPARENT);
        assert_eq!(nav_disabled.text, palette.text_disabled);
    }

    #[test]
    fn pill_spec_matches_selection_colors() {
        let palette = Palette::dark();
        let chosen = pill_spec(true, WidgetState::Normal, &palette);
        assert_eq!(chosen.fill, palette.selection);
        assert_eq!(chosen.text, palette.selection_text);
        assert_eq!(chosen.stroke.color, palette.selection_border);
        let plain = pill_spec(false, WidgetState::Normal, &palette);
        assert_eq!(plain.fill, palette.card);
        assert_eq!(plain.stroke.color, palette.border);
    }

    #[test]
    fn widget_state_resolution_prefers_disabled_and_pressed() {
        assert_eq!(
            WidgetState::resolve(true, false, false),
            WidgetState::Normal
        );
        assert_eq!(
            WidgetState::resolve(true, true, false),
            WidgetState::Hovered
        );
        assert_eq!(WidgetState::resolve(true, true, true), WidgetState::Pressed);
        assert_eq!(
            WidgetState::resolve(false, true, true),
            WidgetState::Disabled
        );
    }

    #[test]
    fn button_height_and_hit_area_match_metrics() {
        let context = test_context();
        let slot = std::cell::Cell::new(Rect::ZERO);
        run_frame(&context, Vec::new(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let response = button(ui, "用 mpv 打开（回车）", ButtonKind::Primary);
                slot.set(response.rect);
            });
        });
        let rect = slot.get();
        assert_eq!(rect.height(), metrics::BTN_H);
        assert!(
            rect.width() > 100.0 && rect.width() < 220.0,
            "button width follows the text: {}",
            rect.width()
        );
        let slot = std::cell::Cell::new(Rect::ZERO);
        run_frame(&context, Vec::new(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let response = button_sized(
                    ui,
                    "取消启动",
                    ButtonKind::Secondary,
                    Vec2::new(72.0, 38.0),
                    false,
                );
                slot.set(response.rect);
            });
        });
        let rect = slot.get();
        assert_eq!(rect.size(), Vec2::new(72.0, 38.0));
    }

    #[test]
    fn nav_button_geometry_matches_probe() {
        let context = test_context();
        let slot = std::cell::Cell::new(Rect::ZERO);
        for (size, expected) in [
            (Vec2::new(148.0, metrics::NAV_H), metrics::NAV_H),
            (Vec2::new(148.0, metrics::NAV_H_TALL), metrics::NAV_H_TALL),
        ] {
            run_frame(&context, Vec::new(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let response = nav_button_size(ui, Glyph::Favorites, "我的收藏", true, size);
                    slot.set(response.rect);
                });
            });
            let rect = slot.get();
            assert_eq!(rect.size(), Vec2::new(148.0, expected));
        }
        // 导航图标/文字的水平位置与原版一致（11 / 31）。
        assert_eq!(metrics::NAV_ICON_LEFT, 11.0);
        assert_eq!(metrics::NAV_TEXT_LEFT, 31.0);
        assert_eq!(metrics::NAV_RADIUS, 7);
    }

    #[test]
    fn traffic_dot_hit_area_and_colors_match_original() {
        let context = test_context();
        let slot = std::cell::Cell::new(Rect::ZERO);
        run_frame(&context, Vec::new(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let response = traffic_dot(ui, DotKind::Close, false);
                slot.set(response.rect);
            });
        });
        assert_eq!(slot.get().size(), Vec2::splat(metrics::DOT_HIT));
        assert_eq!(metrics::DOT_HIT, 30.0);
        assert_eq!(metrics::DOT, 18.0);
        assert_eq!(DotKind::Close.fill(), Color32::from_rgb(0xFF, 0x5F, 0x57));
        assert_eq!(DotKind::Min.fill(), Color32::from_rgb(0xFE, 0xBC, 0x2E));
        assert_eq!(DotKind::Max.fill(), Color32::from_rgb(0x28, 0xC8, 0x40));
        assert_eq!(DotKind::Close.symbol(), Color32::from_rgb(0x36, 0x26, 0x20));
    }

    #[test]
    fn input_and_dropdown_sizes_match_original() {
        let context = test_context();
        let mut text = String::from("7777");
        let input_slot = std::cell::Cell::new(Rect::ZERO);
        let dropdown_slot = std::cell::Cell::new(Rect::ZERO);
        run_frame(&context, Vec::new(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let response = TextInput::new(&mut text).width(585.0).height(38.0).show(ui);
                input_slot.set(response.rect);
                let mut selected = 0usize;
                let response = dropdown(
                    ui,
                    "quality",
                    &mut selected,
                    &["原画 / 最佳", "高清", "流畅"],
                    191.0,
                );
                dropdown_slot.set(response.rect);
            });
        });
        assert_eq!(input_slot.get().height(), metrics::INPUT_H_LG);
        assert_eq!(
            dropdown_slot.get().size(),
            Vec2::new(191.0, metrics::DROPDOWN_H)
        );
    }

    #[test]
    fn button_click_hit_area_triggers_response() {
        let context = test_context();
        let hit = std::cell::Cell::new(Rect::ZERO);
        let slot = std::cell::Cell::new(Rect::ZERO);
        let clicked = std::cell::Cell::new(false);
        run_frame(&context, Vec::new(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let response = button(ui, "打开直播", ButtonKind::Primary);
                hit.set(response.rect);
                if response.clicked() {
                    clicked.set(true);
                }
            });
        });
        let position = hit.get().center();
        for events in press_release_events(position) {
            run_frame(&context, events, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let response = button(ui, "打开直播", ButtonKind::Primary);
                    slot.set(response.rect);
                    if response.clicked() {
                        clicked.set(true);
                    }
                });
            });
        }
        assert!(clicked.get(), "a click inside the button must register");
        assert_eq!(slot.get().height(), metrics::BTN_H);
    }

    #[test]
    fn platform_tabs_click_changes_selection() {
        let context = test_context();
        let mut selected = 0usize;
        let slot = std::cell::Cell::new(Rect::ZERO);
        run_frame(&context, Vec::new(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let response = platform_tabs(ui, &["B站", "虎牙", "斗鱼", "抖音"], &mut selected);
                slot.set(response.rect);
            });
        });
        let rect = slot.get();
        assert_eq!(
            rect.width(),
            metrics::PLATFORM_PILL_W * 4.0 + metrics::TAB_GAP * 3.0
        );
        assert_eq!(rect.height(), metrics::TAB_H);
        // 点击第 3 个 tab（斗鱼）。
        let target = Pos2::new(
            rect.left() + (metrics::PLATFORM_PILL_W + metrics::TAB_GAP) * 2.0 + 10.0,
            rect.center().y,
        );
        for events in press_release_events(target) {
            run_frame(&context, events, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let _ = platform_tabs(ui, &["B站", "虎牙", "斗鱼", "抖音"], &mut selected);
                });
            });
        }
        assert_eq!(selected, 2, "third tab must become selected");
    }

    #[test]
    fn checkbox_click_toggles_and_hit_area_matches_row_height() {
        let context = test_context();
        let mut checked = false;
        let slot = std::cell::Cell::new(Rect::ZERO);
        run_frame(&context, Vec::new(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let response = checkbox(ui, "播放时启动透明弹幕覆盖层", &mut checked);
                slot.set(response.rect);
            });
        });
        assert_eq!(slot.get().height(), metrics::CHECKBOX_ROW);
        let target = slot.get().center();
        for events in press_release_events(target) {
            run_frame(&context, events, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let _ = checkbox(ui, "播放时启动透明弹幕覆盖层", &mut checked);
                });
            });
        }
        assert!(checked, "clicking the checkbox row must toggle it");
    }

    #[test]
    fn every_glyph_paints_without_panicking() {
        let context = test_context();
        let glyphs = [
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
            Glyph::Play,
            Glyph::Film,
            Glyph::Settings,
            Glyph::ChevronDown,
            Glyph::ChevronRight,
            Glyph::Check,
            Glyph::Close,
            Glyph::Min,
            Glyph::Max,
        ];
        run_frame(&context, Vec::new(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                for glyph in glyphs {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::hover());
                    paint_glyph(ui.painter(), rect, glyph, Color32::WHITE, 1.25);
                }
            });
        });
        // 每种 glyph 至少画出了图元（通过形状数量间接确认）。
        let output = context.run(
            RawInput {
                screen_rect: Some(viewport()),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::hover());
                    paint_glyph(ui.painter(), rect, Glyph::Favorites, Color32::WHITE, 1.25);
                });
            },
        );
        assert!(!output.shapes.is_empty(), "glyph paint produced no shapes");
    }

    #[test]
    fn field_stroke_covers_focus_error_and_hover() {
        let palette = Palette::dark();
        let normal = field_stroke(false, false, false, &palette);
        assert_eq!(normal.color, palette.border);
        assert_eq!(normal.width, 1.0);
        let hovered = field_stroke(false, false, true, &palette);
        assert_eq!(hovered.color, palette.border_hover);
        let focused = field_stroke(false, true, false, &palette);
        assert_eq!(focused.color, palette.accent);
        assert_eq!(focused.width, 1.6);
        let error = field_stroke(true, true, true, &palette);
        assert_eq!(error.color, palette.danger);
        assert_eq!(error.width, 1.6);
    }

    #[test]
    fn card_cover_gradient_uses_original_formula() {
        // C#: Blend(accent, cardTone, .55) / Blend(accent, cardTone, .88)
        let palette = Palette::dark();
        let top = theme::blend(palette.bilibili, palette.card, metrics::COVER_TOP_BLEND);
        let bottom = theme::blend(palette.bilibili, palette.card, metrics::COVER_BOTTOM_BLEND);
        // 手算：0.45*accent + 0.55*card
        assert_eq!(top, Color32::from_rgb(0x46, 0x3A, 0x88));
        // 手算：0.12*accent + 0.88*card
        assert_eq!(bottom, Color32::from_rgb(0x26, 0x25, 0x41));
        // 渐变必须由上到下变暗（B 通道随 card 权重升高而降低）。
        assert!(top.b() > bottom.b());
        assert_eq!(metrics::COVER_TOP_BLEND, 0.55);
        assert_eq!(metrics::COVER_BOTTOM_BLEND, 0.88);
    }

    #[test]
    fn card_panel_uses_original_fill_and_radius() {
        let context = test_context();
        let slot = std::cell::Cell::new(Rect::ZERO);
        run_frame(&context, Vec::new(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let response = Card::new().padding(16.0).show(ui, |ui| {
                    ui.label("内容");
                });
                slot.set(response.response.rect);
            });
        });
        assert_eq!(metrics::PANEL_RADIUS, 10);
        assert!(slot.get().width() > 0.0);
    }

    #[test]
    fn labels_produce_expected_heights() {
        let context = test_context();
        let title_slot = std::cell::Cell::new(Rect::ZERO);
        let heading_slot = std::cell::Cell::new(Rect::ZERO);
        let hint_slot = std::cell::Cell::new(Rect::ZERO);
        run_frame(&context, Vec::new(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                title_slot.set(title(ui, "我的收藏").rect);
                heading_slot.set(section_header(ui, "B站 收藏 6 个 · 本地收藏").rect);
                hint_slot.set(hint(ui, "输入房间或选择收藏，回车用 mpv 打开。").rect);
            });
        });
        assert_eq!(title_slot.get().height(), metrics::TITLE_H);
        assert_eq!(heading_slot.get().height(), theme::lh::HEADING);
        assert_eq!(hint_slot.get().height(), theme::lh::SMALL);
        assert!(title_slot.get().width() > 80.0);
    }

    #[test]
    fn avatar_without_texture_draws_initial_without_panicking() {
        let context = test_context();
        run_frame(&context, Vec::new(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
                let palette = palette_of(ui);
                avatar(ui.painter(), rect, None, "主播", &palette);
                let (empty, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
                avatar(ui.painter(), empty, None, "", &palette);
            });
        });
    }

    #[test]
    fn badge_and_empty_state_and_progress_render() {
        let context = test_context();
        let badge_slot = std::cell::Cell::new(Rect::ZERO);
        let progress_slot = std::cell::Cell::new(Rect::ZERO);
        run_frame(&context, Vec::new(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                badge_slot.set(badge(ui, "直播中", BadgeKind::Live).rect);
                empty_state(ui, "还没有收藏", "输入房间号后可加入收藏。");
                progress_slot.set(progress(ui, 0.42, 200.0).rect);
            });
        });
        assert_eq!(badge_slot.get().height(), metrics::BADGE_H);
        assert_eq!(progress_slot.get().height(), metrics::PROGRESS_H);
        assert_eq!(metrics::BADGE_RADIUS, 5);
        assert_eq!(metrics::STATE_PILL_H, 22.0);
    }

    #[test]
    fn escape_closes_an_open_dropdown() {
        let context = test_context();
        let mut selected = 0usize;
        let slot = std::cell::Cell::new(Rect::ZERO);
        run_frame(&context, Vec::new(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                slot.set(dropdown(ui, "escape-test", &mut selected, &["一", "二"], 120.0).rect);
            });
        });
        // 打开
        let position = slot.get().center();
        for events in press_release_events(position) {
            run_frame(&context, events, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let _ = dropdown(ui, "escape-test", &mut selected, &["一", "二"], 120.0);
                });
            });
        }
        // 打开状态下按 Esc 必须只关闭弹层、不 panic、不改选中项。
        run_frame(
            &context,
            vec![Event::Key {
                key: Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::default(),
            }],
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let _ = dropdown(ui, "escape-test", &mut selected, &["一", "二"], 120.0);
                });
            },
        );
    }
}
