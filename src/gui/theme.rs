//! 设计系统（Design System）——原版 C# ModernTheme 的 Rust 1:1 移植。
//!
//! 本模块只负责**外观常量与绘制原语**，不含任何业务状态：
//!
//! - Palette：深/浅两套调色板，逐项对应 danmaku-15-ui.cs 里的 D* / L* 常量
//!   （深色 surface #101217 / card #1B1E27 / border #2C3140 / text #F2F2F2 /
//!   secondary #A8B0C4 / accent #7962DF / onAccent #FFFFFF / selection #FFC53D）。
//! - ts：字号（逻辑 px）。原版 probe 是 200% DPI 的物理值，这里已 /2。
//! - metrics：窗口外壳与控件的逻辑尺寸（chrome 34、侧栏 164、卡片 160/封面 64…）。
//! - space：4 的倍数间距。
//! - install_fonts：**运行时**从系统字体目录加载 Microsoft YaHei UI + Segoe UI
//!   （原版 WinForms 的 UI 字体），TTC 容器自己解析并抽出单个 face；缺字体/加载失败
//!   时安全回退到内置 HarmonyOS Sans SC / egui 默认字体，**不 panic**。
//! - apply：把调色板/字体/间距写进 egui::Style（egui::Visuals）。
//!
//! ## 用法
//!
//! 进程启动时 install_fonts(ctx) 一次；切主题时 apply(ctx, dark)。
//! 页面代码不应再硬编码色值：一律 Palette::for_mode(ui.visuals().dark_mode) 取当前
//! 调色板，尺寸一律走 metrics / space。

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Mesh, Painter,
    Rect, Shadow, Stroke, StrokeKind, TextStyle, Vec2,
};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ============================================================================================
// 主题模式与调色板
// ============================================================================================

/// 深色 / 浅色主题模式。
///
/// 原版 ModernTheme.Apply(bool dark) 用 bool；这里包一层枚举给调用方更清楚的语义。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ThemeMode {
    Dark,
    Light,
}

impl ThemeMode {
    /// true = 深色。
    pub const fn is_dark(self) -> bool {
        matches!(self, Self::Dark)
    }

    /// 从 egui 的 dark_mode 反推。
    pub const fn from_dark(dark: bool) -> Self {
        if dark { Self::Dark } else { Self::Light }
    }
}

/// 全局调色板：深/浅两套**独立挑过**的值（深色不是浅色取反）。
///
/// 字段名与原版 ModernTheme 属性一一对应，页面代码按语义取色，不要自己拼色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    /// 当前调色板是否深色。
    pub dark: bool,
    /// 页面底色（原版 Surface）。
    pub surface: Color32,
    /// 卡片/输入框/下拉框底色（原版 Card）。
    pub card: Color32,
    /// 常规描边（原版 Border）。
    pub border: Color32,
    /// 悬停描边（原版 BorderHover）。
    pub border_hover: Color32,
    /// 中性悬停底（原版 Hover）。
    pub hover: Color32,
    /// 侧栏 / 标题栏的合成底色：原版 WorkbenchPalette.Side = blend(Surface, Card, 0.65)；
    /// 深色实测 #171A21。
    pub side: Color32,
    /// 标题栏专用底（与 side 同色，语义区分）。
    pub chrome: Color32,
    /// 弹出层（下拉列表/菜单）底色。
    pub popup: Color32,
    /// 强调色（主按钮、链接、进度）。原版 Accent。
    pub accent: Color32,
    /// 强调色悬停档。
    pub accent_hover: Color32,
    /// 强调色按下档。
    pub accent_pressed: Color32,
    /// 压在强调色上的前景色（主按钮文字）。
    pub on_accent: Color32,
    /// 正文色。
    pub text: Color32,
    /// 次要文字（说明/元信息）。
    pub text_secondary: Color32,
    /// 禁用文字。
    pub text_disabled: Color32,
    /// 选中态亮橙底（导航选中行、平台 tab、勾选框）。
    pub selection: Color32,
    /// 选中态悬停档。
    pub selection_hover: Color32,
    /// 选中态按下档。
    pub selection_pressed: Color32,
    /// 压在亮橙底上的正文色（深棕；白字压亮橙只有 1.58:1，**不要**用白字）。
    pub selection_text: Color32,
    /// 压在亮橙底上的次要文字色。
    pub selection_text_secondary: Color32,
    /// 选中态描边（卡片/整行选中）。
    pub selection_border: Color32,
    /// 整卡选中的浅琥珀底。
    pub selection_soft: Color32,
    /// 主按钮禁用时的填充。
    pub disabled_fill: Color32,
    /// 成功语义色（深底可读，不用 SeaGreen）。
    pub success: Color32,
    /// 错误语义色。
    pub danger: Color32,
    /// 警告语义色。
    pub warning: Color32,
    /// 平台色：B站。
    pub bilibili: Color32,
    /// 平台色：虎牙。
    pub huya: Color32,
    /// 平台色：斗鱼。
    pub douyu: Color32,
    /// 平台色：抖音。
    pub douyin: Color32,
}

impl Default for Palette {
    fn default() -> Self {
        Self::dark()
    }
}

impl Palette {
    /// 深色调色板（原版 D* 常量）。
    pub const fn dark() -> Self {
        Self {
            dark: true,
            surface: Color32::from_rgb(0x10, 0x12, 0x17),
            card: Color32::from_rgb(0x1B, 0x1E, 0x27),
            border: Color32::from_rgb(0x2C, 0x31, 0x40),
            border_hover: Color32::from_rgb(0x5A, 0x5A, 0x5A),
            hover: Color32::from_rgb(0x24, 0x28, 0x35),
            side: Color32::from_rgb(0x17, 0x1A, 0x21),
            chrome: Color32::from_rgb(0x17, 0x1A, 0x21),
            popup: Color32::from_rgb(0x1B, 0x1E, 0x27),
            accent: Color32::from_rgb(0x79, 0x62, 0xDF),
            accent_hover: Color32::from_rgb(0x87, 0x72, 0xE5),
            accent_pressed: Color32::from_rgb(0x66, 0x50, 0xCA),
            on_accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            text: Color32::from_rgb(0xF2, 0xF2, 0xF2),
            text_secondary: Color32::from_rgb(0xA8, 0xB0, 0xC4),
            text_disabled: Color32::from_rgb(0x66, 0x66, 0x66),
            selection: Color32::from_rgb(0xFF, 0xC5, 0x3D),
            selection_hover: Color32::from_rgb(0xFF, 0xD6, 0x66),
            selection_pressed: Color32::from_rgb(0xF7, 0xB4, 0x0A),
            selection_text: Color32::from_rgb(0x2A, 0x1C, 0x00),
            selection_text_secondary: Color32::from_rgb(0x4A, 0x36, 0x00),
            selection_border: Color32::from_rgb(0xFF, 0xD9, 0x7A),
            selection_soft: Color32::from_rgb(0x52, 0x3C, 0x00),
            disabled_fill: Color32::from_rgb(0x3A, 0x3A, 0x3A),
            success: Color32::from_rgb(0x6C, 0xCB, 0x5F),
            danger: Color32::from_rgb(0xFF, 0x99, 0xA4),
            warning: Color32::from_rgb(0xFE, 0xBC, 0x2E),
            bilibili: Color32::from_rgb(0x7A, 0x5C, 0xFF),
            huya: Color32::from_rgb(0xF5, 0xA6, 0x23),
            douyu: Color32::from_rgb(0xFF, 0x6B, 0x35),
            douyin: Color32::from_rgb(0x22, 0xD3, 0xEE),
        }
    }

    /// 浅色调色板（原版 L* 常量：Windows 11 中性灰阶）。
    pub const fn light() -> Self {
        Self {
            dark: false,
            surface: Color32::from_rgb(0xF5, 0xF6, 0xFA),
            card: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            border: Color32::from_rgb(0xE4, 0xE6, 0xEF),
            border_hover: Color32::from_rgb(0xB0, 0xB0, 0xB0),
            hover: Color32::from_rgb(0xF0, 0xF0, 0xF0),
            // blend(#F5F6FA, #FFFFFF, 0.65) = #FCFCFD
            side: Color32::from_rgb(0xFC, 0xFC, 0xFD),
            chrome: Color32::from_rgb(0xFC, 0xFC, 0xFD),
            popup: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            accent: Color32::from_rgb(0x79, 0x62, 0xDF),
            accent_hover: Color32::from_rgb(0x87, 0x72, 0xE5),
            accent_pressed: Color32::from_rgb(0x66, 0x50, 0xCA),
            on_accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),
            text: Color32::from_rgb(0x1A, 0x1A, 0x1A),
            text_secondary: Color32::from_rgb(0x66, 0x66, 0x66),
            text_disabled: Color32::from_rgb(0xA0, 0xA0, 0xA0),
            selection: Color32::from_rgb(0xFF, 0xC5, 0x3D),
            selection_hover: Color32::from_rgb(0xFF, 0xD6, 0x66),
            selection_pressed: Color32::from_rgb(0xF0, 0xA8, 0x00),
            selection_text: Color32::from_rgb(0x3A, 0x2A, 0x00),
            selection_text_secondary: Color32::from_rgb(0x6B, 0x4A, 0x00),
            selection_border: Color32::from_rgb(0xD9, 0x77, 0x06),
            selection_soft: Color32::from_rgb(0xFF, 0xE2, 0x9A),
            disabled_fill: Color32::from_rgb(0xC8, 0xD6, 0xE6),
            success: Color32::from_rgb(0x10, 0x7C, 0x10),
            danger: Color32::from_rgb(0xC4, 0x2B, 0x1C),
            warning: Color32::from_rgb(0xB4, 0x53, 0x09),
            bilibili: Color32::from_rgb(0x7A, 0x5C, 0xFF),
            huya: Color32::from_rgb(0xF5, 0xA6, 0x23),
            douyu: Color32::from_rgb(0xFF, 0x6B, 0x35),
            douyin: Color32::from_rgb(0x22, 0xD3, 0xEE),
        }
    }

    /// 按 dark 选择调色板。
    pub const fn for_mode(dark: bool) -> Self {
        if dark { Self::dark() } else { Self::light() }
    }

    /// 按 ThemeMode 选择调色板。
    pub const fn of(mode: ThemeMode) -> Self {
        Self::for_mode(mode.is_dark())
    }

    /// 平台品牌色。
    pub const fn platform(&self, platform: crate::core::Platform) -> Color32 {
        match platform {
            crate::core::Platform::Bili => self.bilibili,
            crate::core::Platform::Huya => self.huya,
            crate::core::Platform::Douyu => self.douyu,
            crate::core::Platform::Douyin => self.douyin,
        }
    }

    /// 滚动条滑块色：原版 ModernScrollbarSkin =
    /// (surface * 2 + textSecondary) / 3（**整数除法**，深色实测 #424650）。
    pub fn scroll_thumb(&self) -> Color32 {
        let mix = |surface: u8, secondary: u8| -> u8 {
            ((surface as u16 * 2 + secondary as u16) / 3) as u8
        };
        Color32::from_rgb(
            mix(self.surface.r(), self.text_secondary.r()),
            mix(self.surface.g(), self.text_secondary.g()),
            mix(self.surface.b(), self.text_secondary.b()),
        )
    }

    /// 滚动条槽底色（= 页面底色）。
    pub const fn scroll_track(&self) -> Color32 {
        self.surface
    }

    /// 卡片阴影：深色用更重的黑，浅色用极淡的蓝灰。
    pub const fn card_shadow(&self) -> Shadow {
        if self.dark {
            Shadow {
                offset: [0, 6],
                blur: 18,
                spread: 0,
                color: Color32::from_rgba_premultiplied(0, 0, 0, 70),
            }
        } else {
            Shadow {
                offset: [0, 4],
                blur: 14,
                spread: 0,
                color: Color32::from_rgba_premultiplied(0, 0, 0, 26),
            }
        }
    }
}

// ============================================================================================
// 字号 / 行高
// ============================================================================================

/// 字号（逻辑 px）。
///
/// 来源：原版 probe（2480x1440 @ dpiScale 2）控件字体物理值 / 2，逐项核对：
/// 页面标题 23、品牌字 19、区块标题 14、卡片标题 13、正文 12、副标题 12、
/// 侧栏导航 10.5、分组标题 10、辅助 11/10。
pub mod ts {
    /// 页面标题（"我的收藏"）：23px，加粗。
    pub const TITLE: f32 = 23.0;
    /// 侧栏品牌字（"看直播"）：19px，加粗。
    pub const BRAND: f32 = 19.0;
    /// 区块标题（"B站 收藏 6 个 · 本地收藏" / "正在观看 / 最近打开"）：14px，加粗。
    pub const HEADING: f32 = 14.0;
    /// 卡片标题（主播名）：13px，加粗。
    pub const CARD_TITLE: f32 = 13.0;
    /// 正文：12px。
    pub const BODY: f32 = 12.0;
    /// 副标题（页面标题右侧说明）：12px。
    pub const SUBTITLE: f32 = 12.0;
    /// 辅助正文：11px（表单标签、提示行、卡片元信息）。
    pub const SMALL: f32 = 11.0;
    /// 侧栏导航：10.5px。
    pub const NAV: f32 = 10.5;
    /// 标题栏品牌/状态：11px。
    pub const CHROME: f32 = 11.0;
    /// 分组标题 / "本次"范围提示：10px。
    pub const CAPTION: f32 = 10.0;
    /// 小标签（"选择"丸、状态丸、行内小按钮）：11px。
    pub const PILL: f32 = 11.0;
    /// 等宽（诊断/日志）。
    pub const MONO: f32 = 11.0;
}

/// 行高（逻辑 px）：文本容器的最小高度，用于垂直居中布局。
pub mod lh {
    /// 标题行。
    pub const TITLE: f32 = 32.0;
    /// 区块标题行。
    pub const HEADING: f32 = 20.0;
    /// 正文行。
    pub const BODY: f32 = 18.0;
    /// 辅助行。
    pub const SMALL: f32 = 17.0;
    /// 分组标题行。
    pub const GROUP: f32 = 16.0;
    /// 标题栏行（= chrome 高度）。
    pub const CHROME: f32 = 34.0;
}

// ============================================================================================
// 尺寸与间距
// ============================================================================================

/// 控件与外壳的逻辑尺寸（全部已 /2 对齐原版 probe）。
pub mod metrics {
    // ---- 窗口外壳 ----
    /// 自绘标题栏高度。
    pub const CHROME_H: f32 = 34.0;
    /// 侧栏宽度（宽窗）。
    pub const SIDEBAR_W: f32 = 164.0;
    /// 侧栏宽度（中窗，<1000 逻辑 px）。
    pub const SIDEBAR_W_MID: f32 = 144.0;
    /// 侧栏宽度（窄窗，<760 逻辑 px）。
    pub const SIDEBAR_W_NARROW: f32 = 124.0;
    /// 内容区左右留白（宽窗）。
    pub const PAD: f32 = 24.0;
    /// 内容区左右留白（窄窗）。
    pub const PAD_NARROW: f32 = 20.0;
    /// 标题到页面的顶部距离（内容面板内）。
    pub const PAGE_TOP: f32 = 50.0;
    /// 标题行高度。
    pub const TITLE_H: f32 = 32.0;
    /// 标题与副标题之间的水平间隔。
    pub const TITLE_GAP: f32 = 14.0;
    /// 副标题行高度。
    pub const SUBTITLE_H: f32 = 18.0;
    /// 窗口最小逻辑宽度。
    pub const MIN_W: f32 = 760.0;
    /// 窗口最小逻辑高度。
    pub const MIN_H: f32 = 520.0;
    /// 默认窗口逻辑尺寸。
    pub const DEFAULT_W: f32 = 1240.0;
    /// 默认窗口逻辑高度。
    pub const DEFAULT_H: f32 = 720.0;

    // ---- 标题栏 ----
    /// 红黄绿圆钮的绘制直径。
    pub const DOT: f32 = 18.0;
    /// 圆钮的点击热区（原版 30x30）。
    pub const DOT_HIT: f32 = 30.0;
    /// 圆钮间距。
    pub const DOT_GAP: f32 = 4.0;
    /// 标题栏右侧留白。
    pub const CHROME_PAD: f32 = 8.0;
    /// 标题栏 App 图标尺寸。
    pub const CHROME_ICON: f32 = 18.0;
    /// 标题栏图标左边距。
    pub const CHROME_ICON_LEFT: f32 = 14.0;

    // ---- 侧栏 ----
    /// 导航项高度（单行）。
    pub const NAV_H: f32 = 36.0;
    /// 导航项高度（两行，如 "本地数据 / 平台状态"）。
    pub const NAV_H_TALL: f32 = 52.0;
    /// 导航项左右缩进（相对侧栏）。
    pub const NAV_INSET: f32 = 8.0;
    /// 导航项之间的垂直间距。
    pub const NAV_GAP: f32 = 4.0;
    /// 导航项圆角。
    pub const NAV_RADIUS: u8 = 7;
    /// 导航图标尺寸。
    pub const NAV_ICON: f32 = 15.0;
    /// 导航图标左边距。
    pub const NAV_ICON_LEFT: f32 = 11.0;
    /// 导航文字左边距。
    pub const NAV_TEXT_LEFT: f32 = 31.0;
    /// 侧栏品牌块左边距。
    pub const BRAND_LEFT: f32 = 12.0;
    /// 侧栏品牌块上边距。
    pub const BRAND_TOP: f32 = 22.0;
    /// 品牌图标尺寸。
    pub const BRAND_MARK: f32 = 30.0;
    /// 品牌文字尺寸。
    pub const BRAND_FONT: f32 = 19.0;
    /// 分组标题左边距。
    pub const GROUP_LEFT: f32 = 12.0;
    /// 分组标题高度。
    pub const GROUP_H: f32 = 16.0;

    // ---- 按钮 / 输入 ----
    /// 常规按钮高度。
    pub const BTN_H: f32 = 34.0;
    /// 小按钮高度（"加入收藏"/"删除收藏"/工具条）。
    pub const BTN_H_SM: f32 = 30.0;
    /// 大按钮高度（launch 卡主按钮）。
    pub const BTN_H_LG: f32 = 38.0;
    /// 迷你按钮高度（"弹幕设置…"/"全部会话 →"）。
    pub const BTN_H_XS: f32 = 24.0;
    /// 按钮圆角。
    pub const BTN_RADIUS: u8 = 8;
    /// 按钮左右内边距。
    pub const BTN_PAD_X: f32 = 12.0;
    /// 输入框高度。
    pub const INPUT_H: f32 = 34.0;
    /// 大输入框高度（launch 卡房间输入）。
    pub const INPUT_H_LG: f32 = 38.0;
    /// 输入框圆角。
    pub const INPUT_RADIUS: u8 = 5;
    /// 输入框左右内边距。
    pub const INPUT_PAD_X: f32 = 8.0;
    /// 下拉框高度。
    pub const DROPDOWN_H: f32 = 25.0;
    /// 下拉框（大）高度。
    pub const DROPDOWN_H_LG: f32 = 27.0;
    /// 下拉选项行高。
    pub const DROPDOWN_ROW: f32 = 22.0;
    /// 下拉框圆角。
    pub const DROPDOWN_RADIUS: u8 = 8;
    /// 下拉箭头热区宽度。
    pub const DROPDOWN_CHEVRON: f32 = 22.0;
    /// 输入/下拉文字左内边距。
    pub const FIELD_PAD_X: f32 = 7.0;

    // ---- 平台 tab ----
    /// 平台 tab 宽度。
    pub const PLATFORM_PILL_W: f32 = 72.0;
    /// 平台 tab 高度。
    pub const TAB_H: f32 = 30.0;
    /// 平台 tab 之间的水平间距。
    pub const TAB_GAP: f32 = 6.0;
    /// 平台 tab 背后那条组合底（WorkbenchFillStrip）圆角。
    pub const STRIP_RADIUS: u8 = 9;
    /// 平台 tab 背后组合底的向外扩量。
    pub const STRIP_BLEED: f32 = 3.0;

    // ---- 面板 / 卡片 ----
    /// 面板（WorkbenchPanel）圆角。
    pub const PANEL_RADIUS: u8 = 10;
    /// 面板描边宽度。
    pub const PANEL_BORDER: f32 = 1.0;
    /// 房间卡片高度。
    pub const CARD_H: f32 = 160.0;
    /// 房间卡片封面高度。
    pub const CARD_COVER: f32 = 64.0;
    /// 房间卡片内边距。
    pub const CARD_INNER: f32 = 12.0;
    /// 房间卡片圆角。
    pub const CARD_RADIUS: u8 = 11;
    /// 卡片间距。
    pub const CARD_GAP: f32 = 12.0;
    /// 卡片列表左侧/顶部内缩。
    pub const CARD_INSET: f32 = 2.0;
    /// 卡片网格列数阈值：可用宽度 >= 该值时 3 列。
    pub const CARD_3COL: f32 = 660.0;
    /// 卡片网格列数阈值：可用宽度 >= 该值时 2 列。
    pub const CARD_2COL: f32 = 420.0;
    /// 卡片头像尺寸。
    pub const CARD_AVATAR: f32 = 36.0;
    /// 卡片头像内边距。
    pub const CARD_AVATAR_PAD: f32 = 14.0;
    /// "选择"丸宽度。
    pub const PILL_W: f32 = 52.0;
    /// "选择"丸高度。
    pub const PILL_H: f32 = 24.0;
    /// "选择"丸圆角。
    pub const PILL_RADIUS: u8 = 6;
    /// 状态丸高度。
    pub const STATE_PILL_H: f32 = 22.0;
    /// 状态丸圆角。
    pub const STATE_PILL_RADIUS: u8 = 5;
    /// 卡片底部动作按钮高度。
    pub const CARD_BTN_H: f32 = 30.0;
    /// 会话卡高度（右栏窄版）。
    pub const SESSION_H: f32 = 94.0;
    /// 会话卡（宽版单行高度）。
    pub const SESSION_H_WIDE: f32 = 64.0;
    /// 会话卡圆角。
    pub const SESSION_RADIUS: u8 = 10;
    /// 会话卡头像尺寸。
    pub const SESSION_AVATAR: f32 = 32.0;
    /// 会话卡内边距。
    pub const SESSION_PAD: f32 = 12.0;
    /// 会话卡间距。
    pub const SESSION_GAP: f32 = 8.0;

    // ---- 其它 ----
    /// 滚动条占位宽度（细条）。
    pub const SCROLLBAR_W: f32 = 6.0;
    /// 滚动条滑块实际厚度（原版 4px 细圆角拇指，ui-spec §1.6）。
    pub const SCROLLBAR_THICK: f32 = 4.0;
    /// 滚动条滑块最小长度。
    pub const SCROLLBAR_MIN: f32 = 24.0;
    /// 进度条高度。
    pub const PROGRESS_H: f32 = 6.0;
    /// 徽章/标签高度。
    pub const BADGE_H: f32 = 20.0;
    /// 徽章/标签圆角。
    pub const BADGE_RADIUS: u8 = 5;
    /// 复选框方框尺寸。
    pub const CHECKBOX: f32 = 16.0;
    /// 复选框行高。
    pub const CHECKBOX_ROW: f32 = 24.0;
    /// 复选框圆角。
    pub const CHECKBOX_RADIUS: u8 = 4;
    /// 分隔线粗细。
    pub const DIVIDER: f32 = 1.0;
    /// glyph 笔画宽度（逻辑 px）。
    pub const GLYPH_STROKE: f32 = 1.25;
    /// 封面渐变起点混合权重（accent -> card）。
    pub const COVER_TOP_BLEND: f32 = 0.55;
    /// 封面渐变终点混合权重。
    pub const COVER_BOTTOM_BLEND: f32 = 0.88;

    // ---- 收藏页 launch 卡（原版 LayoutWorkbenchFavorites 的实测值）----
    /// launch 卡内边距。
    pub const LAUNCH_PAD: f32 = 16.0;
    /// launch 卡房间输入与按钮之间的水平间距。
    pub const LAUNCH_GAP: f32 = 8.0;
    /// launch 卡表单标签高度。
    pub const LAUNCH_LABEL_H: f32 = 14.0;
    /// launch 卡表单标签到下拉框的间距。
    pub const LAUNCH_LABEL_GAP: f32 = 3.0;
    /// launch 卡 5 列表单的列间距。
    pub const LAUNCH_COL_GAP: f32 = 10.0;
}

/// 间距阶梯（4 的倍数）。
pub mod space {
    /// 4px。
    pub const XS: f32 = 4.0;
    /// 8px。
    pub const SM: f32 = 8.0;
    /// 10px（表单行内小间隔）。
    pub const SM2: f32 = 10.0;
    /// 12px。
    pub const MD: f32 = 12.0;
    /// 14px。
    pub const MD2: f32 = 14.0;
    /// 16px。
    pub const LG: f32 = 16.0;
    /// 24px。
    pub const XL: f32 = 24.0;
    /// 32px。
    pub const XXL: f32 = 32.0;
}

// ============================================================================================
// 绘制原语
// ============================================================================================

/// 线性混合两个颜色（weight = 0 返回 a，1 返回 b）。
///
/// 对应原版 WorkbenchPalette.Blend；四舍五入（+0.5 取整），不做 sRGB 线性化 ——
/// 原版就是简单插值，保持一致。
pub fn blend(a: Color32, b: Color32, weight: f32) -> Color32 {
    let w = weight.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| -> u8 {
        let value = x as f32 * (1.0 - w) + y as f32 * w + 0.5;
        value.clamp(0.0, 255.0) as u8
    };
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

/// 给颜色套一个不透明度（0..=1），保留 RGB。
pub fn with_alpha(color: Color32, alpha: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(
        color.r(),
        color.g(),
        color.b(),
        (alpha.clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
    )
}

/// 画一个圆角矩形（填充 + 可选描边）。描边画在形状内侧（与原版 GDI+ DrawPath 一致）。
pub fn round_rect(
    painter: &Painter,
    rect: Rect,
    radius: impl Into<CornerRadius>,
    fill: impl Into<Color32>,
    stroke: impl Into<Stroke>,
) {
    painter.rect(rect, radius, fill, stroke, StrokeKind::Inside);
}

/// 竖直渐变（顶部 top -> 底部 bottom），填充整个矩形（不裁剪圆角）。
///
/// 卡片封面的圆角渐变请用 gradient_round_rect。
pub fn gradient_v(painter: &Painter, rect: Rect, top: Color32, bottom: Color32) {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }
    let mut mesh = Mesh::default();
    let uv = egui::Pos2::ZERO;
    mesh.vertices.push(egui::epaint::Vertex {
        pos: rect.left_top(),
        uv,
        color: top,
    });
    mesh.vertices.push(egui::epaint::Vertex {
        pos: rect.right_top(),
        uv,
        color: top,
    });
    mesh.vertices.push(egui::epaint::Vertex {
        pos: rect.left_bottom(),
        uv,
        color: bottom,
    });
    mesh.vertices.push(egui::epaint::Vertex {
        pos: rect.right_bottom(),
        uv,
        color: bottom,
    });
    mesh.indices.extend_from_slice(&[0, 1, 2, 2, 1, 3]);
    painter.add(egui::Shape::mesh(mesh));
}

/// 水平渐变（左侧 left -> 右侧 right）。
pub fn gradient_h(painter: &Painter, rect: Rect, left: Color32, right: Color32) {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }
    let mut mesh = Mesh::default();
    let uv = egui::Pos2::ZERO;
    mesh.vertices.push(egui::epaint::Vertex {
        pos: rect.left_top(),
        uv,
        color: left,
    });
    mesh.vertices.push(egui::epaint::Vertex {
        pos: rect.right_top(),
        uv,
        color: right,
    });
    mesh.vertices.push(egui::epaint::Vertex {
        pos: rect.left_bottom(),
        uv,
        color: left,
    });
    mesh.vertices.push(egui::epaint::Vertex {
        pos: rect.right_bottom(),
        uv,
        color: right,
    });
    mesh.indices.extend_from_slice(&[0, 1, 2, 2, 1, 3]);
    painter.add(egui::Shape::mesh(mesh));
}

/// 圆角矩形内的竖直渐变（原版卡片封面：LinearGradientBrush 裁进圆角路径）。
///
/// 实现上按 2px 横带逐条填充，并按四角半径收缩左右边界 —— 形状与原版视觉一致，
/// 每张封面约 32 条横带，开销可忽略。
pub fn gradient_round_rect(
    painter: &Painter,
    rect: Rect,
    corner_radius: CornerRadius,
    top: Color32,
    bottom: Color32,
) {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }
    let band = 2.0_f32;
    let mut y = rect.top();
    while y < rect.bottom() - 0.01 {
        let y2 = (y + band).min(rect.bottom());
        let t = ((y + y2) / 2.0 - rect.top()) / rect.height().max(0.001);
        let color = blend(top, bottom, t);
        let top_inset = round_rect_inset(rect, y - rect.top(), corner_radius);
        let bottom_inset = round_rect_inset(rect, y2 - rect.top(), corner_radius);
        let left = top_inset.0.max(bottom_inset.0);
        let right = top_inset.1.max(bottom_inset.1);
        let band_rect = Rect::from_min_max(
            egui::pos2(rect.left() + left, y),
            egui::pos2(rect.right() - right, y2),
        );
        if band_rect.width() > 0.0 && band_rect.height() > 0.0 {
            painter.rect_filled(band_rect, 0.0, color);
        }
        y = y2;
    }
}

/// 计算圆角矩形在相对高度 y 处的左右内缩量，支持四角独立半径。
pub fn round_rect_inset(rect: Rect, y: f32, radius: CornerRadius) -> (f32, f32) {
    let h = rect.height();
    let inset = |dy: f32, r: f32| -> f32 {
        if r <= 0.5 || dy < 0.0 || dy >= r {
            0.0
        } else {
            (r - (r * r - (r - dy) * (r - dy)).max(0.0).sqrt()).max(0.0)
        }
    };
    let left = if y < radius.nw as f32 {
        inset(y, radius.nw as f32)
    } else if y > h - radius.sw as f32 {
        inset(h - y, radius.sw as f32)
    } else {
        0.0
    };
    let right = if y < radius.ne as f32 {
        inset(y, radius.ne as f32)
    } else if y > h - radius.se as f32 {
        inset(h - y, radius.se as f32)
    } else {
        0.0
    };
    (left, right)
}

/// 画一条水平分隔线。
pub fn divider_line(painter: &Painter, rect: Rect, color: Color32) {
    let y = rect.center().y.round() + 0.5;
    painter.hline(rect.x_range(), y, Stroke::new(metrics::DIVIDER, color));
}

/// 窗口外壳的边线类型（原版 WorkbenchStrip：侧栏右侧 / 标题栏底侧各一条 hairline）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    /// 右侧竖线（侧栏）。
    Right,
    /// 底部横线（标题栏）。
    Bottom,
}

/// 在 rect 的指定边画 1px 描边。
pub fn edge_line(painter: &Painter, rect: Rect, edge: Edge, color: Color32) {
    let stroke = Stroke::new(metrics::PANEL_BORDER, color);
    match edge {
        Edge::Right => {
            let x = rect.right().round() - 0.5;
            painter.vline(x, rect.y_range(), stroke);
        }
        Edge::Bottom => {
            let y = rect.bottom().round() - 0.5;
            painter.hline(rect.x_range(), y, stroke);
        }
    }
}

/// 常规文本字体（egui Proportional 族 = 系统 Microsoft YaHei UI）。
pub fn font(size: f32) -> FontId {
    FontId::new(scaled_font_size(size), FontFamily::Proportional)
}

/// **粗体**文本字体（原版 Microsoft YaHei UI Bold；缺失时回退常规字重）。
pub fn font_bold(size: f32) -> FontId {
    FontId::new(
        scaled_font_size(size),
        FontFamily::Name(Arc::from(FAMILY_BOLD)),
    )
}

/// 等宽字体（诊断/日志）。
pub fn font_mono(size: f32) -> FontId {
    FontId::new(scaled_font_size(size), FontFamily::Monospace)
}

/// egui 字体族里粗体族的名字。
pub const FAMILY_BOLD: &str = "live-hub-bold";

// ============================================================================================
// Style 应用
// ============================================================================================

/// 把主题写进 egui 全局样式（Visuals + Spacing + TextStyle）。
///
/// 页面切换深浅色时调用即可；egui 会在下一帧用新样式重绘。
pub fn apply(context: &egui::Context, dark: bool) {
    let palette = Palette::for_mode(dark);
    let mut style = (*context.style()).clone();
    apply_style(&mut style, &palette);
    context.set_style(style);
}

/// 把调色板写进已有 egui::Style（gallery / 测试可直接构造样式）。
pub fn apply_style(style: &mut egui::Style, palette: &Palette) {
    let mut visuals = if palette.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    visuals.dark_mode = palette.dark;
    visuals.override_text_color = Some(palette.text);
    visuals.weak_text_color = Some(palette.text_secondary);
    visuals.selection.bg_fill = with_alpha(palette.accent, 0.55);
    visuals.selection.stroke = Stroke::new(1.0_f32, palette.on_accent);
    visuals.hyperlink_color = palette.accent;
    visuals.faint_bg_color = palette.hover;
    // egui 的滚动条 = extreme_bg_color(槽) + widgets.inactive.bg_fill(滑块)，
    // 原版 ModernScrollbarSkin 是「页面底色槽 + (surface*2+secondary)/3 滑块」。
    visuals.extreme_bg_color = palette.surface;
    visuals.text_edit_bg_color = Some(palette.card);
    visuals.code_bg_color = palette.card;
    visuals.warn_fg_color = palette.warning;
    visuals.error_fg_color = palette.danger;
    visuals.window_corner_radius = CornerRadius::same(metrics::PANEL_RADIUS);
    visuals.window_shadow = palette.card_shadow();
    visuals.window_fill = palette.card;
    visuals.window_stroke = Stroke::new(1.0_f32, palette.border);
    visuals.menu_corner_radius = CornerRadius::same(metrics::DROPDOWN_RADIUS);
    visuals.panel_fill = palette.surface;
    visuals.popup_shadow = palette.card_shadow();
    visuals.button_frame = true;
    visuals.striped = false;
    visuals.indent_has_left_vline = false;
    visuals.text_cursor.stroke = Stroke::new(1.5_f32, palette.text);

    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = palette.card;
    widgets.noninteractive.weak_bg_fill = palette.card;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, palette.border);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, palette.text);
    widgets.noninteractive.corner_radius = CornerRadius::same(metrics::BTN_RADIUS);
    // 滑块色走原版配方；按钮/下拉等"弱底"控件仍用 card。
    widgets.inactive.bg_fill = palette.scroll_thumb();
    widgets.inactive.weak_bg_fill = palette.card;
    widgets.inactive.bg_stroke = Stroke::new(1.0_f32, palette.border);
    widgets.inactive.fg_stroke = Stroke::new(1.0_f32, palette.text);
    widgets.inactive.corner_radius = CornerRadius::same(metrics::BTN_RADIUS);
    widgets.hovered.bg_fill = palette.hover;
    widgets.hovered.weak_bg_fill = palette.hover;
    widgets.hovered.bg_stroke = Stroke::new(1.0_f32, palette.border_hover);
    widgets.hovered.fg_stroke = Stroke::new(1.0_f32, palette.text);
    widgets.hovered.corner_radius = CornerRadius::same(metrics::BTN_RADIUS);
    widgets.active.bg_fill = palette.border;
    widgets.active.weak_bg_fill = palette.border;
    widgets.active.bg_stroke = Stroke::new(1.0_f32, palette.border_hover);
    widgets.active.fg_stroke = Stroke::new(1.0_f32, palette.text);
    widgets.active.corner_radius = CornerRadius::same(metrics::BTN_RADIUS);
    widgets.open.bg_fill = palette.hover;
    widgets.open.weak_bg_fill = palette.hover;
    widgets.open.bg_stroke = Stroke::new(1.0_f32, palette.border);
    widgets.open.fg_stroke = Stroke::new(1.0_f32, palette.text);
    widgets.open.corner_radius = CornerRadius::same(metrics::BTN_RADIUS);

    style.visuals = visuals;
    style.animation_time = 0.08;

    style.spacing.item_spacing = Vec2::new(space::SM, space::SM);
    style.spacing.button_padding = Vec2::new(metrics::BTN_PAD_X, 6.0);
    style.spacing.window_margin = egui::Margin::same(space::MD as i8);
    style.spacing.menu_margin = egui::Margin::same(space::SM as i8);
    style.spacing.indent = 16.0;
    style.spacing.interact_size = Vec2::new(40.0, metrics::BTN_H);
    style.spacing.slider_width = 160.0;
    style.spacing.combo_width = 120.0;
    style.spacing.combo_height = 260.0;
    style.spacing.text_edit_width = 200.0;
    style.spacing.icon_width = 16.0;
    style.spacing.icon_width_inner = 11.0;
    style.spacing.icon_spacing = 6.0;
    style.spacing.menu_width = 160.0;
    style.spacing.menu_spacing = 4.0;
    style.spacing.tooltip_width = 480.0;

    let mut scroll = style.spacing.scroll.clone();
    scroll.floating = false;
    scroll.bar_width = metrics::SCROLLBAR_W;
    scroll.handle_min_length = metrics::SCROLLBAR_MIN;
    // 原版拇指 4px（条宽 6 - 两侧各 1px 内边距）。
    scroll.bar_inner_margin = (metrics::SCROLLBAR_W - metrics::SCROLLBAR_THICK) / 2.0;
    scroll.bar_outer_margin = 1.0;
    scroll.dormant_background_opacity = 0.0;
    scroll.dormant_handle_opacity = 0.0;
    scroll.foreground_color = false;
    style.spacing.scroll = scroll;

    style.text_styles.insert(TextStyle::Body, font(ts::BODY));
    style.text_styles.insert(TextStyle::Button, font(ts::BODY));
    style.text_styles.insert(TextStyle::Small, font(ts::SMALL));
    style
        .text_styles
        .insert(TextStyle::Heading, font_bold(ts::TITLE));
    style
        .text_styles
        .insert(TextStyle::Monospace, font_mono(ts::MONO));
    style
        .text_styles
        .insert(TextStyle::Name(Arc::from("title")), font_bold(ts::TITLE));
    style.text_styles.insert(
        TextStyle::Name(Arc::from("heading")),
        font_bold(ts::HEADING),
    );
    style
        .text_styles
        .insert(TextStyle::Name(Arc::from("nav")), font(ts::NAV));
    style
        .text_styles
        .insert(TextStyle::Name(Arc::from("caption")), font(ts::CAPTION));
}

// ============================================================================================
// 字体：运行时加载系统 Microsoft YaHei UI + Segoe UI
// ============================================================================================

/// 内置回退字体（HarmonyOS Sans SC Regular，随仓库携带、Apache-2.0，见
/// assets/fonts/LICENSE.txt）。**系统字体绝不打包进仓库**，仓库里只有它在。
pub const BUNDLED_SANS: &[u8] = include_bytes!("../../assets/fonts/HarmonyOS_Sans_SC_Regular.ttf");

/// 字体族偏好（对应原版 danmaku-15-ui.cs 的 UiFontKey：auto / yahei-ui / yahei / system）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FontPreference {
    /// 系统 Microsoft YaHei UI 优先 —— 与 C# WorkbenchFonts 完全一致（主窗口默认）。
    #[default]
    SystemUi,
    /// 仓库内置 HarmonyOS Sans SC 优先 —— 原版 ModernTheme 的 auto 行为
    /// （弹幕设置/覆盖层那套 UI；仓库里只有 Regular，没有 Medium 字重）。
    BundledSans,
}

/// 界面字体下拉的持久化键（core::Settings::ui_font_key 必须落在
/// auto / harmony / yahei-ui / yahei / system 之内，见 core.rs Settings::validate）。
///
/// 这里只暴露三个用户可选项：自动（系统优先）/ 系统雅黑 / 内置鸿蒙。
pub const UI_FONT_KEYS: [(&str, &str); 3] = [
    ("auto", "自动（系统优先）"),
    ("system", "系统雅黑（Microsoft YaHei UI）"),
    ("harmony", "内置鸿蒙（HarmonyOS Sans SC）"),
];

/// 界面字号百分比下拉档位（原版 UiFontPercent 四档）。
pub const UI_FONT_PERCENT_STEPS: [u16; 4] = [85, 100, 115, 130];

/// 把任意历史/大小写写法归一成 core 校验接受的三个键之一
/// （auto / system / harmony）。未知值按 auto 处理。
pub fn normalize_ui_font_key(key: &str) -> &'static str {
    match key.trim().to_ascii_lowercase().as_str() {
        "system" | "yahei-ui" | "yahei" => "system",
        "harmony" | "harmonyos" | "bundled" => "harmony",
        _ => "auto",
    }
}

/// 持久化键在 [UI_FONT_KEYS] 里的下标（给设置页下拉用）。
pub fn ui_font_key_index(key: &str) -> usize {
    let normalized = normalize_ui_font_key(key);
    UI_FONT_KEYS
        .iter()
        .position(|(value, _)| *value == normalized)
        .unwrap_or(0)
}

/// 当前百分比的规范化值：落在 85..=130，非有限值按 100 处理。
pub fn normalize_ui_font_percent(percent: u16) -> u16 {
    percent.clamp(85, 130)
}

/// 字体安装选项。
#[derive(Clone, Copy, Debug)]
pub struct FontOptions {
    /// 字体族偏好。
    pub preference: FontPreference,
    /// UI 字号百分比：原版 UiFontPercent 的 85 / 100 / 115 / 130 四档（限幅 85..=130）。
    pub scale_percent: f32,
}

impl Default for FontOptions {
    fn default() -> Self {
        Self {
            preference: FontPreference::SystemUi,
            scale_percent: 100.0,
        }
    }
}

impl FontOptions {
    /// 从持久化设置构造（这是设置页 / 启动路径的**唯一**入口）。
    ///
    /// - ui_font_key：core::Settings::ui_font_key（auto / harmony / yahei-ui /
    ///   yahei / system；其它历史写法也接受，未知按 auto）
    /// - ui_font_percent：core::Settings::ui_font_percent（85..=130，越界会限幅）
    ///
    /// auto 与 system 在 Rust 主窗口里行为相同（系统 Microsoft YaHei UI 优先，
    /// 对应 C# WorkbenchFonts）；harmony 切到仓库内置 HarmonyOS Sans SC 优先。
    pub fn from_settings(ui_font_key: &str, ui_font_percent: u16) -> Self {
        let preference = if normalize_ui_font_key(ui_font_key) == "harmony" {
            FontPreference::BundledSans
        } else {
            FontPreference::SystemUi
        };
        Self {
            preference,
            scale_percent: normalize_ui_font_percent(ui_font_percent) as f32,
        }
    }
}

impl FontPreference {
    /// 对应 [UI_FONT_KEYS] 里的持久化键（SystemUi 写 "system"；自动档由调用方保留 "auto"）。
    pub const fn settings_key(self) -> &'static str {
        match self {
            Self::SystemUi => "system",
            Self::BundledSans => "harmony",
        }
    }

    /// 设置页中文短标签。
    pub const fn label_zh(self) -> &'static str {
        match self {
            Self::SystemUi => "系统雅黑（Microsoft YaHei UI）",
            Self::BundledSans => "内置鸿蒙（HarmonyOS Sans SC）",
        }
    }
}

/// 按 [FontOptions] 安装字体并立刻重建主题样式（启动/设置页切换都走这里）。
///
/// 等价于 install_fonts_with + apply(当前 dark)；字体缺失或解析失败时内部回退，
/// 不会 panic。
pub fn apply_fonts(context: &egui::Context, options: FontOptions) -> FontReport {
    let dark = context.style().visuals.dark_mode;
    apply_fonts_themed(context, options, dark)
}

/// [apply_fonts] 的显式主题版本（启动时已知 dark_mode，避免依赖默认样式）。
pub fn apply_fonts_themed(context: &egui::Context, options: FontOptions, dark: bool) -> FontReport {
    let report = install_fonts_with(context, options);
    // 字体装好后重建 text_styles：字号百分比会同时作用到固定 TextStyle 上。
    apply(context, dark);
    report
}

/// 全局 UI 字号倍数（f32 位模式；0 表示"还没设置"，按 1.0 处理）。
static UI_FONT_SCALE_BITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// 设置 UI 字号百分比（原版 UiFontPercent；85..=130 之外会被限幅）。
///
/// 只影响字体尺寸，不改布局常量（与原版一致）。改完请重新调用 [apply]，
/// 让 egui 的 text_styles 用新字号重建。
pub fn set_ui_font_scale(percent: f32) {
    let factor = clamp_scale_percent(percent) / 100.0;
    UI_FONT_SCALE_BITS.store(factor.to_bits(), std::sync::atomic::Ordering::Relaxed);
}

/// 把 UI 字号百分比限幅到原版四档范围（85..=130）。纯函数，便于单测。
pub fn clamp_scale_percent(percent: f32) -> f32 {
    if percent.is_finite() {
        percent.clamp(85.0, 130.0)
    } else {
        100.0
    }
}

/// 纯函数：把基础字号按百分比换算（不受全局状态影响，便于单测）。
pub fn scale_font_size(base: f32, percent: f32) -> f32 {
    base * clamp_scale_percent(percent) / 100.0
}

/// 当前 UI 字号倍数（默认 1.0）。
pub fn ui_font_scale() -> f32 {
    let bits = UI_FONT_SCALE_BITS.load(std::sync::atomic::Ordering::Relaxed);
    if bits == 0 { 1.0 } else { f32::from_bits(bits) }
}

/// 字号缩放计算（纯函数，便于单测）：基础字号 * 当前倍数。
pub fn scaled_font_size(base: f32) -> f32 {
    base * ui_font_scale()
}

/// 系统 UI 字体候选（按优先级）。原版 WinForms 用 Microsoft YaHei UI。
const UI_CANDIDATES: &[&str] = &["msyh.ttc", "msyh.ttf"];
/// 系统 UI 粗体候选。
const UI_BOLD_CANDIDATES: &[&str] = &["msyhbd.ttc", "msyhbd.ttf"];
/// 西文 UI 字体候选（原版浏览器稿的 Segoe UI 栈）。
const LATIN_CANDIDATES: &[&str] = &["segoeui.ttf"];
/// 西文粗体候选。
const LATIN_BOLD_CANDIDATES: &[&str] = &["segoeuib.ttf"];
/// 系统 UI 字体要匹配的家族名（TTC 里挑 face 用）。
const UI_FAMILY_NAMES: &[&str] = &["Microsoft YaHei UI", "Microsoft YaHei"];
/// 系统字体文件的体积上限（msyh.ttc 约 19.7MB，留足余量）。
const MAX_FONT_BYTES: u64 = 64 * 1024 * 1024;

/// 一个成功加载的字体 face。
#[derive(Clone, Debug)]
pub struct LoadedFace {
    /// 字体文件路径（诊断用，不含用户数据）。
    pub path: PathBuf,
    /// 原容器内的 face 序号（单文件字体为 0）。
    pub index: u32,
    /// 从 name 表读到的家族名。
    pub family: String,
    /// 子家族名（Regular / Bold …）。
    pub style: String,
    /// 抽出的独立 sfnt 字节（可直接交给 epaint）。
    pub data: Vec<u8>,
    /// 抽出的 sfnt 字节数（data 被交给 epaint 之后仍可读，用于诊断/断言）。
    pub bytes: usize,
    /// 这个 face 是否真的粗体。
    pub bold: bool,
}

/// 字体加载结果：给启动诊断 / 测试断言用。
#[derive(Clone, Debug, Default)]
pub struct FontReport {
    /// 常规 UI face（Microsoft YaHei UI Regular）。
    pub regular: Option<LoadedFace>,
    /// 粗体 UI face（Microsoft YaHei UI Bold）。
    pub bold: Option<LoadedFace>,
    /// 西文 UI face（Segoe UI）。
    pub latin: Option<LoadedFace>,
    /// 西文粗体 face（Segoe UI Bold）。
    pub latin_bold: Option<LoadedFace>,
    /// 是否用了内置 HarmonyOS Sans SC 兜底。
    pub bundled_fallback: bool,
    /// 实际生效的 UI 字体描述（中文，给"关于/诊断"页显示）。
    pub ui_family: String,
    /// 加载过程中的非致命说明（缺字体、回退原因等）。
    pub notes: Vec<String>,
}

impl FontReport {
    /// 是否拿到了系统中文 UI 字体（Microsoft YaHei UI）。
    pub fn system_ui(&self) -> bool {
        self.regular.is_some()
    }

    /// 是否有真正的粗体 face（否则标题会用常规字重顶替）。
    pub fn bold_ready(&self) -> bool {
        self.bold.is_some()
    }

    /// 一行中文诊断摘要。
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        parts.push(format!("UI 字体：{}", self.ui_family));
        if let Some(bold) = &self.bold {
            parts.push(format!("粗体：{} {}", bold.family, bold.style));
        } else {
            parts.push("粗体：无（回退常规字重）".to_owned());
        }
        if self.bundled_fallback {
            parts.push("使用内置 HarmonyOS Sans SC 兜底".to_owned());
        }
        if !self.notes.is_empty() {
            parts.push(format!("说明：{}", self.notes.join("；")));
        }
        parts.join("；")
    }
}

/// 从系统字体目录加载 UI 字体并安装进 egui（进程启动时调用一次）。
///
/// 行为：
/// 1. 读 %WINDIR%\Fonts（可用环境变量 LIVE_HUB_FONT_DIR 覆盖，测试用）下的
///    msyh.ttc，自行解析 TTC 头与 name 表，挑出 "Microsoft YaHei UI" face，
///    把该 face 的所有表重建成一个独立 sfnt 字节块（epaint 只吃单字体，不碰 TTC）。
/// 2. 同样加载 msyhbd.ttc（Bold）与 segoeui.ttf / segoeuib.ttf。
/// 3. 任一步失败都只记录到 FontReport::notes，继续用内置 HarmonyOS Sans SC / egui 默认字体。
/// 4. 在真正 set_fonts 前，用一次性探针 Context 试排版一段中英文；
///    探针 panic 时回退到"内置字体 + 默认字体"再装（**对外不 panic**）。
///
/// 注意：不要频繁调用（每次会读约 36MB 系统字体）。
pub fn install_fonts(context: &egui::Context) -> FontReport {
    install_fonts_with(context, FontOptions::default())
}

/// 带偏好的字体安装（NAV-06）：可切"系统雅黑优先 / 内置鸿蒙优先"与 UI 字号百分比。
///
/// 92% 的用法与 [install_fonts] 相同；设置页切换字体族/字号后重新调用即可。
pub fn install_fonts_with(context: &egui::Context, options: FontOptions) -> FontReport {
    set_ui_font_scale(options.scale_percent);
    let mut report = FontReport::default();
    let font_dir = system_font_dir();
    let definitions = build_font_definitions(&font_dir, options.preference, &mut report);
    let definitions = verify_or_fallback(definitions, &mut report);
    context.set_fonts(definitions);
    FONTS_READY.store(true, std::sync::atomic::Ordering::Relaxed);
    report
}

/// 进程内是否已经安装过字体定义（install_fonts / install_preview_fonts）。
static FONTS_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 字体是否已经安装（用于诊断；控件绘制前应保证为 true，否则粗体族未绑定）。
pub fn fonts_installed() -> bool {
    FONTS_READY.load(std::sync::atomic::Ordering::Relaxed)
}

/// 只绑定仓库内置的 HarmonyOS Sans SC（零拷贝，不读系统字体），供**测试/预览**使用。
///
/// 生产路径必须用 [install_fonts]（系统 Microsoft YaHei UI 优先）。这个函数存在的
/// 原因是：任何使用 font_bold 的控件都要求 egui 里存在 "live-hub-bold" 字体族，
/// 而裸的 egui::Context::default() 没有它 —— 那样会 panic，测试和预览不能踩这个坑。
pub fn install_preview_fonts(context: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "live-hub-bundled-sans".to_owned(),
        Arc::new(FontData::from_static(BUNDLED_SANS)),
    );
    prepend_family(
        &mut fonts,
        FontFamily::Proportional,
        vec!["live-hub-bundled-sans".to_owned()],
    );
    fonts.families.insert(
        FontFamily::Name(Arc::from(FAMILY_BOLD)),
        vec!["live-hub-bundled-sans".to_owned()],
    );
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .push("live-hub-bundled-sans".to_owned());
    context.set_fonts(fonts);
    FONTS_READY.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// 系统字体目录：%WINDIR%\Fonts，可用 LIVE_HUB_FONT_DIR 覆盖（测试/便携环境）。
pub fn system_font_dir() -> PathBuf {
    if let Some(custom) = std::env::var_os("LIVE_HUB_FONT_DIR") {
        if !custom.is_empty() {
            return PathBuf::from(custom);
        }
    }
    let windir = std::env::var_os("WINDIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\Windows"));
    windir.join("Fonts")
}

fn build_font_definitions(
    font_dir: &Path,
    preference: FontPreference,
    report: &mut FontReport,
) -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "live-hub-bundled-sans".to_owned(),
        Arc::new(FontData::from_static(BUNDLED_SANS)),
    );

    // ---- 常规 UI 字体 ----
    match load_system_face(font_dir, UI_CANDIDATES, UI_FAMILY_NAMES, false) {
        Ok(mut face) => {
            report.ui_family = format!("{} {}", face.family, face.style).trim().to_owned();
            let data = std::mem::take(&mut face.data);
            fonts.font_data.insert(
                "live-hub-ui".to_owned(),
                Arc::new(FontData::from_owned(data)),
            );
            report.regular = Some(face);
        }
        Err(error) => {
            report.bundled_fallback = true;
            report.ui_family = "HarmonyOS Sans SC（内置）".to_owned();
            report.notes.push(format!("系统 UI 字体不可用：{error}"));
        }
    }

    // ---- 粗体 UI 字体 ----
    match load_system_face(font_dir, UI_BOLD_CANDIDATES, UI_FAMILY_NAMES, true) {
        Ok(mut face) if face.bold => {
            let data = std::mem::take(&mut face.data);
            fonts.font_data.insert(
                "live-hub-ui-bold".to_owned(),
                Arc::new(FontData::from_owned(data)),
            );
            report.bold = Some(face);
        }
        Ok(face) => {
            report.notes.push(format!(
                "{} 只有常规字重，标题用常规字重顶替",
                face.path.display()
            ));
        }
        Err(error) => {
            report.notes.push(format!("系统粗体不可用：{error}"));
        }
    }

    // ---- 西文（Segoe UI）----
    if let Ok(mut face) = load_system_face(font_dir, LATIN_CANDIDATES, &["Segoe UI"], false) {
        let data = std::mem::take(&mut face.data);
        fonts.font_data.insert(
            "live-hub-latin".to_owned(),
            Arc::new(FontData::from_owned(data)),
        );
        report.latin = Some(face);
    }
    if let Ok(mut face) = load_system_face(font_dir, LATIN_BOLD_CANDIDATES, &["Segoe UI"], true) {
        if face.bold {
            let data = std::mem::take(&mut face.data);
            fonts.font_data.insert(
                "live-hub-latin-bold".to_owned(),
                Arc::new(FontData::from_owned(data)),
            );
            report.latin_bold = Some(face);
        }
    }

    // ---- 家族装配 ----
    // Proportional：按偏好排序。SystemUi = 系统雅黑 -> 西文 -> 内置中文 -> egui 默认；
    // BundledSans = 内置中文 -> 系统雅黑 -> 西文 -> egui 默认（原版 ModernTheme auto）。
    let bundled_first = preference == FontPreference::BundledSans;
    if bundled_first {
        report.bundled_fallback = true;
        report.ui_family = "HarmonyOS Sans SC（内置，优先）".to_owned();
    }
    let mut proportional: Vec<String> = Vec::new();
    let mut push_regular = |names: &mut Vec<String>| {
        if bundled_first {
            names.push("live-hub-bundled-sans".to_owned());
        }
        if report.regular.is_some() {
            names.push("live-hub-ui".to_owned());
        }
        if report.latin.is_some() {
            names.push("live-hub-latin".to_owned());
        }
        if !bundled_first {
            names.push("live-hub-bundled-sans".to_owned());
        }
    };
    push_regular(&mut proportional);
    if proportional.is_empty() {
        proportional.push("live-hub-bundled-sans".to_owned());
    }
    prepend_family(&mut fonts, FontFamily::Proportional, proportional);

    // 粗体族：真粗体缺失时用常规字体顶替（egui 没有合成粗体）。
    let mut bold: Vec<String> = Vec::new();
    if bundled_first {
        // 仓库只带 HarmonyOS Regular：没有 Medium/Bold face，标题用常规字重。
        bold.push("live-hub-bundled-sans".to_owned());
        report
            .notes
            .push("内置 HarmonyOS Sans SC 只有 Regular 字重，粗体标题按常规字重渲染".to_owned());
    }
    if report.bold.is_some() {
        bold.push("live-hub-ui-bold".to_owned());
    }
    if report.latin_bold.is_some() {
        bold.push("live-hub-latin-bold".to_owned());
    }
    if report.regular.is_some() {
        bold.push("live-hub-ui".to_owned());
    }
    if !bundled_first {
        bold.push("live-hub-bundled-sans".to_owned());
    }
    fonts
        .families
        .insert(FontFamily::Name(Arc::from(FAMILY_BOLD)), bold);

    // 等宽：保留 egui 默认等宽，再追加中文兜底（诊断里的中文日志能显示）。
    let monospace = fonts.families.entry(FontFamily::Monospace).or_default();
    monospace.push("live-hub-bundled-sans".to_owned());
    if report.regular.is_some() {
        monospace.push("live-hub-ui".to_owned());
    }

    fonts
}

fn prepend_family(fonts: &mut FontDefinitions, family: FontFamily, names: Vec<String>) {
    let existing = fonts.families.entry(family).or_default();
    let mut merged = names;
    merged.extend(existing.iter().cloned());
    *existing = merged;
}

/// 用一次性探针 Context 验证字体定义能被 epaint/ab_glyph 解析；失败时回退内置字体。
///
/// epaint 解析字体失败是 panic!（ab_glyph_font_from_font_data），所以这里在装进真实
/// Context 之前先探一遍，把崩溃挡在 catch_unwind 里。
fn verify_or_fallback(
    mut definitions: FontDefinitions,
    report: &mut FontReport,
) -> FontDefinitions {
    if probe_fonts(&definitions) {
        return definitions;
    }
    report
        .notes
        .push("系统字体解析失败，已回退内置 HarmonyOS Sans SC（不影响使用）".to_owned());
    report.regular = None;
    report.bold = None;
    report.latin = None;
    report.latin_bold = None;
    report.bundled_fallback = true;
    report.ui_family = "HarmonyOS Sans SC（内置）".to_owned();

    definitions = FontDefinitions::default();
    definitions.font_data.insert(
        "live-hub-bundled-sans".to_owned(),
        Arc::new(FontData::from_static(BUNDLED_SANS)),
    );
    prepend_family(
        &mut definitions,
        FontFamily::Proportional,
        vec!["live-hub-bundled-sans".to_owned()],
    );
    definitions.families.insert(
        FontFamily::Name(Arc::from(FAMILY_BOLD)),
        vec!["live-hub-bundled-sans".to_owned()],
    );
    definitions
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .push("live-hub-bundled-sans".to_owned());
    if probe_fonts(&definitions) {
        return definitions;
    }
    // 连内置字体都解析不了：返回 egui 默认定义，宁可丑也不能崩。
    report
        .notes
        .push("内置字体也无法解析，使用 egui 默认字体".to_owned());
    FontDefinitions::default()
}

/// 在独立 Context 里排版一段中英文，验证字体可解析（失败 = panic = false）。
fn probe_fonts(definitions: &FontDefinitions) -> bool {
    let probe = egui::Context::default();
    probe.set_fonts(definitions.clone());
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = probe.run(egui::RawInput::default(), |ctx| {
            let _ = ctx.fonts_mut(|fonts| {
                fonts.layout_no_wrap(
                    "看直播 Rust UI 0123".to_owned(),
                    font(ts::BODY),
                    Color32::WHITE,
                )
            });
            let _ = ctx.fonts_mut(|fonts| {
                fonts.layout_no_wrap(
                    "粗体 Bold 标题".to_owned(),
                    font_bold(ts::TITLE),
                    Color32::WHITE,
                )
            });
        });
    }))
    .is_ok()
}

/// 从候选中挑出第一个可加载的字体脸。
fn load_system_face(
    font_dir: &Path,
    candidates: &[&str],
    wanted_families: &[&str],
    bold: bool,
) -> Result<LoadedFace, String> {
    let mut errors = Vec::new();
    for name in candidates {
        let path = font_dir.join(name);
        if !path.is_file() {
            errors.push(format!("{} 不存在", path.display()));
            continue;
        }
        match extract_face(&path, wanted_families, bold) {
            Ok(face) => return Ok(face),
            Err(error) => errors.push(format!("{}：{error}", path.display())),
        }
    }
    Err(errors.join("；"))
}

// ============================================================================================
// TTC / sfnt 解析与单 face 提取
// ============================================================================================

/// sfnt 表目录项。
#[derive(Clone, Copy, Debug)]
struct TableRecord {
    tag: [u8; 4],
    checksum: u32,
    offset: u32,
    length: u32,
}

impl TableRecord {
    fn tag_str(&self) -> String {
        String::from_utf8_lossy(&self.tag).to_string()
    }
}

/// 一个字体脸的 sfnt 结构描述。
struct FaceLayout {
    /// sfnt 版本（0x00010000 / 'true' / 'OTTO'）。
    version: u32,
    /// 表目录。
    tables: Vec<TableRecord>,
}

/// TTC 头信息（普通字体视为单 face 的 TTC）。
struct Collection {
    /// 各 face 的 sfnt 目录偏移。
    face_offsets: Vec<u32>,
}

/// 解析字体文件头：TTC 返回多 face，普通字体返回单 face（偏移 0）。
fn read_collection(file: &mut File, file_len: u64) -> Result<Collection, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek 失败：{error}"))?;
    let mut head = [0u8; 12];
    file.read_exact(&mut head)
        .map_err(|error| format!("读取文件头失败：{error}"))?;
    let tag = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
    if tag == 0x7474_6366 {
        // 'ttcf'
        let num_fonts = u32::from_be_bytes([head[8], head[9], head[10], head[11]]) as usize;
        if num_fonts == 0 || num_fonts > 64 {
            return Err(format!("TTC face 数量异常：{num_fonts}"));
        }
        let mut offsets = vec![0u8; num_fonts * 4];
        file.read_exact(&mut offsets)
            .map_err(|error| format!("读取 TTC 偏移表失败：{error}"))?;
        let mut face_offsets = Vec::with_capacity(num_fonts);
        for index in 0..num_fonts {
            let bytes = &offsets[index * 4..index * 4 + 4];
            let offset = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            if offset as u64 + 12 > file_len {
                return Err(format!("TTC face {index} 偏移越界"));
            }
            face_offsets.push(offset);
        }
        Ok(Collection { face_offsets })
    } else {
        Ok(Collection {
            face_offsets: vec![0],
        })
    }
}

/// 读取一个 face 的 sfnt 表目录。
fn read_face_layout(file: &mut File, face_offset: u64) -> Result<FaceLayout, String> {
    file.seek(SeekFrom::Start(face_offset))
        .map_err(|error| format!("seek 失败：{error}"))?;
    let mut header = [0u8; 12];
    file.read_exact(&mut header)
        .map_err(|error| format!("读取 sfnt 头失败：{error}"))?;
    let version = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    if !matches!(version, 0x0001_0000 | 0x7472_7565 | 0x4F54_544F) {
        return Err(format!("不是合法的 sfnt 版本 0x{version:08X}"));
    }
    let num_tables = u16::from_be_bytes([header[4], header[5]]) as usize;
    if num_tables == 0 || num_tables > 512 {
        return Err(format!("表数量异常：{num_tables}"));
    }
    let mut directory = vec![0u8; num_tables * 16];
    file.read_exact(&mut directory)
        .map_err(|error| format!("读取表目录失败：{error}"))?;
    let mut tables = Vec::with_capacity(num_tables);
    for index in 0..num_tables {
        let entry = &directory[index * 16..index * 16 + 16];
        tables.push(TableRecord {
            tag: [entry[0], entry[1], entry[2], entry[3]],
            checksum: u32::from_be_bytes([entry[4], entry[5], entry[6], entry[7]]),
            offset: u32::from_be_bytes([entry[8], entry[9], entry[10], entry[11]]),
            length: u32::from_be_bytes([entry[12], entry[13], entry[14], entry[15]]),
        });
    }
    Ok(FaceLayout { version, tables })
}

/// 读取 name 表里的家族名/子家族名（只取 Windows 英文平台的记录）。
fn read_name_table(file: &mut File, record: &TableRecord) -> Result<(String, String), String> {
    if record.length > 1_000_000 {
        return Err("name 表异常大".to_owned());
    }
    file.seek(SeekFrom::Start(record.offset as u64))
        .map_err(|error| format!("seek name 表失败：{error}"))?;
    let mut data = vec![0u8; record.length as usize];
    file.read_exact(&mut data)
        .map_err(|error| format!("读取 name 表失败：{error}"))?;
    if data.len() < 6 {
        return Err("name 表过短".to_owned());
    }
    let count = u16::from_be_bytes([data[2], data[3]]) as usize;
    let string_offset = u16::from_be_bytes([data[4], data[5]]) as usize;
    let mut family = String::new();
    let mut style = String::new();
    for index in 0..count {
        let start = 6 + index * 12;
        if start + 12 > data.len() {
            break;
        }
        let entry = &data[start..start + 12];
        let platform = u16::from_be_bytes([entry[0], entry[1]]);
        let encoding = u16::from_be_bytes([entry[2], entry[3]]);
        let language = u16::from_be_bytes([entry[4], entry[5]]);
        let name_id = u16::from_be_bytes([entry[6], entry[7]]);
        let length = u16::from_be_bytes([entry[8], entry[9]]) as usize;
        let offset = u16::from_be_bytes([entry[10], entry[11]]) as usize;
        // 0x409 = 英文（美国）；中文平台记录不影响字体选择。
        if platform != 3 || encoding != 1 || (language != 0x409 && language != 0) {
            continue;
        }
        let start = string_offset + offset;
        if start + length > data.len() || length % 2 != 0 {
            continue;
        }
        let decoded: String = data[start..start + length]
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .filter_map(|unit| char::from_u32(unit as u32))
            .collect();
        match name_id {
            1 if family.is_empty() => family = decoded,
            2 if style.is_empty() => style = decoded,
            _ => {}
        }
        if !family.is_empty() && !style.is_empty() {
            break;
        }
    }
    Ok((family, style))
}

/// 在字体文件里挑出想要的 face，并把它重建成独立 sfnt 字节块。
fn extract_face(
    path: &Path,
    wanted_families: &[&str],
    want_bold: bool,
) -> Result<LoadedFace, String> {
    let file_len = std::fs::metadata(path)
        .map_err(|error| format!("无法读取字体元数据：{error}"))?
        .len();
    if file_len == 0 || file_len > MAX_FONT_BYTES {
        return Err(format!("字体文件尺寸异常：{file_len} 字节"));
    }
    let mut file = File::open(path).map_err(|error| format!("打开字体失败：{error}"))?;
    let collection = read_collection(&mut file, file_len)?;

    // 先按"想要的家族名"顺序找（Microsoft YaHei UI 优先于 Microsoft YaHei），
    // 再在同一家族里按字重匹配 —— 否则 msyh.ttc 的 face 0（Microsoft YaHei）会被先挑中。
    let mut fallback_choice: Option<(u32, FaceLayout, String, String)> = None;
    for wanted in wanted_families {
        for (index, offset) in collection.face_offsets.iter().enumerate() {
            let Ok(layout) = read_face_layout(&mut file, *offset as u64) else {
                continue;
            };
            let Some(name_record) = layout.tables.iter().find(|table| table.tag == *b"name") else {
                continue;
            };
            let (family, style) = read_name_table(&mut file, name_record).unwrap_or_default();
            if !family.eq_ignore_ascii_case(wanted) {
                continue;
            }
            let style_bold = style.to_ascii_lowercase().contains("bold");
            if style_bold == want_bold {
                return rebuild_face(&mut file, &layout, path, index as u32, family, style);
            }
            if fallback_choice.is_none() {
                fallback_choice = Some((index as u32, layout, family, style));
            }
        }
    }
    if let Some((index, layout, family, style)) = fallback_choice {
        // 家族命中但字重不符：用该 face 顶替（loaded.bold 会如实标为 false）。
        return rebuild_face(&mut file, &layout, path, index, family, style);
    }
    Err(format!("未找到字体族 {:?}", wanted_families.join(" / ")))
}

/// 把一个 face 的表数据重建成独立 sfnt（表偏移改为新文件内的偏移，4 字节对齐）。
fn rebuild_face(
    file: &mut File,
    layout: &FaceLayout,
    path: &Path,
    index: u32,
    family: String,
    style: String,
) -> Result<LoadedFace, String> {
    // 必需的度量/映射表；轮廓则 glyf+loca 或 CFF 二选一。
    let has = |tag: &[u8; 4]| layout.tables.iter().any(|table| table.tag == *tag);
    for tag in [
        b"head", b"hhea", b"hmtx", b"maxp", b"cmap", b"name", b"OS/2", b"post",
    ] {
        if !has(tag) {
            return Err(format!("缺少必需的表 {}", String::from_utf8_lossy(tag)));
        }
    }
    if !has(b"glyf") && !has(b"CFF ") {
        return Err("既没有 glyf 也没有 CFF 轮廓表".to_owned());
    }
    if has(b"glyf") && !has(b"loca") {
        return Err("glyf 存在但缺少 loca".to_owned());
    }

    let num_tables = layout.tables.len();
    let header_len = 12 + num_tables * 16;
    let mut header = vec![0u8; header_len];
    header[0..4].copy_from_slice(&layout.version.to_be_bytes());
    header[4..6].copy_from_slice(&(num_tables as u16).to_be_bytes());
    let entry_selector = (num_tables as f32).log2().floor() as u16;
    let search_range = (1u16 << entry_selector) * 16;
    header[6..8].copy_from_slice(&search_range.to_be_bytes());
    header[8..10].copy_from_slice(&entry_selector.to_be_bytes());
    header[10..12]
        .copy_from_slice(&((num_tables * 16 - search_range as usize) as u16).to_be_bytes());

    let mut data: Vec<u8> = Vec::new();
    for (position, table) in layout.tables.iter().enumerate() {
        if table.length as u64 > MAX_FONT_BYTES {
            return Err(format!("表 {} 尺寸异常", table.tag_str()));
        }
        file.seek(SeekFrom::Start(table.offset as u64))
            .map_err(|error| format!("seek 表 {} 失败：{error}", table.tag_str()))?;
        let mut buffer = vec![0u8; table.length as usize];
        file.read_exact(&mut buffer)
            .map_err(|error| format!("读取表 {} 失败：{error}", table.tag_str()))?;
        let offset = (header_len + data.len()) as u32;
        let entry = 12 + position * 16;
        header[entry..entry + 4].copy_from_slice(&table.tag);
        header[entry + 4..entry + 8].copy_from_slice(&table.checksum.to_be_bytes());
        header[entry + 8..entry + 12].copy_from_slice(&offset.to_be_bytes());
        header[entry + 12..entry + 16].copy_from_slice(&table.length.to_be_bytes());
        data.extend_from_slice(&buffer);
        while data.len() % 4 != 0 {
            data.push(0);
        }
    }

    let mut bytes = header;
    bytes.extend_from_slice(&data);
    let size = bytes.len();
    let bold = style.to_ascii_lowercase().contains("bold");
    Ok(LoadedFace {
        path: path.to_path_buf(),
        index,
        family,
        style,
        data: bytes,
        bytes: size,
        bold,
    })
}

// ============================================================================================
// 测试
// ============================================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 这两个测试会改进程级 UI 字号倍数；并行跑互相干扰，用一把锁串行化。
    /// （其它模块的测试不写这个全局值，只可能短暂观察到 1.15，断言都留了余量。）
    static FONT_SCALE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn dark_palette_matches_original_modern_theme() {
        let palette = Palette::dark();
        assert_eq!(palette.surface, Color32::from_rgb(0x10, 0x12, 0x17));
        assert_eq!(palette.card, Color32::from_rgb(0x1B, 0x1E, 0x27));
        assert_eq!(palette.border, Color32::from_rgb(0x2C, 0x31, 0x40));
        assert_eq!(palette.text, Color32::from_rgb(0xF2, 0xF2, 0xF2));
        assert_eq!(palette.text_secondary, Color32::from_rgb(0xA8, 0xB0, 0xC4));
        assert_eq!(palette.accent, Color32::from_rgb(0x79, 0x62, 0xDF));
        assert_eq!(palette.on_accent, Color32::WHITE);
        assert_eq!(palette.selection, Color32::from_rgb(0xFF, 0xC5, 0x3D));
        assert_eq!(palette.hover, Color32::from_rgb(0x24, 0x28, 0x35));
        assert_eq!(palette.side, Color32::from_rgb(0x17, 0x1A, 0x21));
        assert_eq!(palette.selection_soft, Color32::from_rgb(0x52, 0x3C, 0x00));
        assert_eq!(
            palette.selection_border,
            Color32::from_rgb(0xFF, 0xD9, 0x7A)
        );
        assert_eq!(palette.success, Color32::from_rgb(0x6C, 0xCB, 0x5F));
        assert_eq!(palette.danger, Color32::from_rgb(0xFF, 0x99, 0xA4));
        assert_eq!(palette.warning, Color32::from_rgb(0xFE, 0xBC, 0x2E));
    }

    #[test]
    fn light_palette_matches_original_modern_theme() {
        let palette = Palette::light();
        assert_eq!(palette.surface, Color32::from_rgb(0xF5, 0xF6, 0xFA));
        assert_eq!(palette.card, Color32::WHITE);
        assert_eq!(palette.border, Color32::from_rgb(0xE4, 0xE6, 0xEF));
        assert_eq!(palette.text, Color32::from_rgb(0x1A, 0x1A, 0x1A));
        assert_eq!(palette.text_secondary, Color32::from_rgb(0x66, 0x66, 0x66));
        assert_eq!(palette.accent, Color32::from_rgb(0x79, 0x62, 0xDF));
        assert_eq!(palette.selection_text, Color32::from_rgb(0x3A, 0x2A, 0x00));
        assert_eq!(palette.selection_soft, Color32::from_rgb(0xFF, 0xE2, 0x9A));
        assert_eq!(palette.disabled_fill, Color32::from_rgb(0xC8, 0xD6, 0xE6));
        assert_eq!(palette.success, Color32::from_rgb(0x10, 0x7C, 0x10));
        assert_eq!(palette.danger, Color32::from_rgb(0xC4, 0x2B, 0x1C));
    }

    #[test]
    fn side_color_is_blend_of_surface_and_card() {
        for palette in [Palette::dark(), Palette::light()] {
            let expected = blend(palette.surface, palette.card, 0.65);
            assert!(
                (palette.side.r() as i16 - expected.r() as i16).abs() <= 1
                    && (palette.side.g() as i16 - expected.g() as i16).abs() <= 1
                    && (palette.side.b() as i16 - expected.b() as i16).abs() <= 1,
                "side={:?} expected={expected:?}",
                palette.side
            );
        }
    }

    #[test]
    fn platform_colors_match_the_original_palette() {
        let palette = Palette::dark();
        use crate::core::Platform;
        assert_eq!(
            palette.platform(Platform::Bili),
            Color32::from_rgb(0x7A, 0x5C, 0xFF)
        );
        assert_eq!(
            palette.platform(Platform::Huya),
            Color32::from_rgb(0xF5, 0xA6, 0x23)
        );
        assert_eq!(
            palette.platform(Platform::Douyu),
            Color32::from_rgb(0xFF, 0x6B, 0x35)
        );
        assert_eq!(
            palette.platform(Platform::Douyin),
            Color32::from_rgb(0x22, 0xD3, 0xEE)
        );
    }

    #[test]
    fn blend_hits_both_ends() {
        let a = Color32::from_rgb(0x10, 0x20, 0x30);
        let b = Color32::from_rgb(0xA0, 0xB0, 0xC0);
        assert_eq!(blend(a, b, 0.0), a);
        assert_eq!(blend(a, b, 1.0), b);
        assert_eq!(blend(a, b, 0.5), Color32::from_rgb(0x58, 0x68, 0x78));
    }

    #[test]
    fn scroll_thumb_matches_original_scrollbar_skin() {
        let palette = Palette::dark();
        // 原版：(surface*2 + textSecondary)/3 = (0x10*2+0xA8)/3, (0x12*2+0xB0)/3, (0x17*2+0xC4)/3
        assert_eq!(palette.scroll_thumb(), Color32::from_rgb(0x42, 0x46, 0x50));
    }

    #[test]
    fn text_sizes_and_metrics_match_probe_values() {
        assert_eq!(ts::TITLE, 23.0);
        assert_eq!(ts::BRAND, 19.0);
        assert_eq!(ts::HEADING, 14.0);
        assert_eq!(ts::CARD_TITLE, 13.0);
        assert_eq!(ts::BODY, 12.0);
        assert_eq!(ts::SMALL, 11.0);
        assert_eq!(ts::NAV, 10.5);
        assert_eq!(ts::CAPTION, 10.0);
        assert_eq!(metrics::CHROME_H, 34.0);
        assert_eq!(metrics::SIDEBAR_W, 164.0);
        assert_eq!(metrics::NAV_H, 36.0);
        assert_eq!(metrics::NAV_H_TALL, 52.0);
        assert_eq!(metrics::NAV_INSET, 8.0);
        assert_eq!(metrics::CARD_H, 160.0);
        assert_eq!(metrics::CARD_COVER, 64.0);
        assert_eq!(metrics::CARD_INNER, 12.0);
        assert_eq!(metrics::CARD_GAP, 12.0);
        assert_eq!(metrics::PLATFORM_PILL_W, 72.0);
        assert_eq!(metrics::TAB_H, 30.0);
        assert_eq!(metrics::DROPDOWN_H, 25.0);
        assert_eq!(metrics::INPUT_H_LG, 38.0);
        assert_eq!(metrics::BTN_H_LG, 38.0);
        assert_eq!(metrics::DOT, 18.0);
        assert_eq!(metrics::DOT_HIT, 30.0);
    }

    #[test]
    fn apply_style_installs_palette_colors() {
        let palette = Palette::dark();
        let mut style = egui::Style::default();
        apply_style(&mut style, &palette);
        assert!(style.visuals.dark_mode);
        assert_eq!(style.visuals.panel_fill, palette.surface);
        assert_eq!(style.visuals.override_text_color, Some(palette.text));
        assert_eq!(style.visuals.hyperlink_color, palette.accent);
        assert_eq!(style.visuals.window_fill, palette.card);
        assert_eq!(
            style.visuals.widgets.noninteractive.bg_stroke.color,
            palette.border
        );
        assert_eq!(style.visuals.widgets.hovered.bg_fill, palette.hover);
        assert!(!style.spacing.scroll.floating);
        assert_eq!(style.spacing.scroll.bar_width, metrics::SCROLLBAR_W);
        // 滚动条 skin：槽=页面底色，滑块=原版 blend 灰（#424650）。
        assert_eq!(style.visuals.extreme_bg_color, palette.surface);
        assert_eq!(
            style.visuals.widgets.inactive.bg_fill,
            palette.scroll_thumb()
        );
        assert_eq!(style.visuals.widgets.inactive.weak_bg_fill, palette.card);
        // ⚠️ ui_font_scale 是进程级全局量，并行测试会改它：apply_style 在调用瞬间
        // 读到一个合法倍数即可，这里只断言结果落在四档之一（不写死、不读全局）。
        let allowed: Vec<f32> = UI_FONT_PERCENT_STEPS
            .iter()
            .map(|percent| scale_font_size(ts::TITLE, f32::from(*percent)))
            .collect();
        let heading = style
            .text_styles
            .get(&TextStyle::Heading)
            .map(|font| font.size)
            .expect("heading text style");
        let titled = style
            .text_styles
            .get(&TextStyle::Name(Arc::from("title")))
            .map(|font| font.size)
            .expect("title text style");
        assert!(
            allowed.iter().any(|size| (heading - size).abs() < 0.05),
            "heading {heading} is not one of the four UiFontPercent steps {allowed:?}"
        );
        assert!(
            allowed.iter().any(|size| (titled - size).abs() < 0.05),
            "title {titled} is not one of the four UiFontPercent steps {allowed:?}"
        );
    }

    #[test]
    fn reject_garbage_font_bytes_without_panicking() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("broken.ttf");
        std::fs::write(&path, b"not a font at all, but definitely not a panic").expect("write");
        let error = extract_face(&path, &["Microsoft YaHei UI"], false).expect_err("must fail");
        assert!(!error.is_empty());
        let missing = extract_face(&directory.path().join("absent.ttf"), &["x"], false)
            .expect_err("missing file must fail");
        assert!(missing.contains("元数据") || missing.contains("打开"));
    }

    #[test]
    fn empty_font_directory_falls_back_without_panicking() {
        let directory = tempfile::tempdir().expect("temp dir");
        let report = report_for_font_dir(directory.path());
        assert!(!report.system_ui(), "empty dir cannot provide system fonts");
        assert!(report.bundled_fallback);
        assert!(report.ui_family.contains("HarmonyOS"));
        assert!(!report.notes.is_empty());

        // 回退也必须真的能排版中英文（否则页面会全空白）。
        let probe = egui::Context::default();
        let report = install_fonts(&probe);
        let _ = probe.run(egui::RawInput::default(), |ctx| {
            let galley = ctx.fonts_mut(|fonts| {
                fonts.layout_no_wrap("看直播 fallback".to_owned(), font(ts::BODY), Color32::WHITE)
            });
            assert!(
                galley.size().x > 20.0,
                "fallback layout produced empty text (report: {})",
                report.describe()
            );
        });
    }

    #[test]
    fn system_fonts_load_when_present() {
        // 本机没有系统字体（CI/离线容器）时不算失败，但必须走安全回退。
        let font_dir = system_font_dir();
        let expected = font_dir.join("msyh.ttc").is_file();
        let report = report_for_font_dir(&font_dir);
        if expected {
            assert!(
                report.system_ui(),
                "msyh.ttc exists but was not loaded: {}",
                report.describe()
            );
            let regular = report.regular.as_ref().expect("regular face");
            assert!(
                regular.family.eq_ignore_ascii_case("Microsoft YaHei UI"),
                "expected Microsoft YaHei UI, got {}",
                regular.family
            );
            assert_eq!(
                regular.index, 1,
                "Microsoft YaHei UI is TTC face #1 in msyh.ttc"
            );
            assert!(!regular.bold);
            assert!(
                regular.bytes > 10_000_000,
                "extracted face looks truncated: {} bytes",
                regular.bytes
            );
            assert!(
                report.bold_ready(),
                "msyhbd.ttc exists but bold face missing"
            );
        }
    }

    #[test]
    fn system_fonts_render_chinese_and_latin_at_probe_sizes() {
        let context = egui::Context::default();
        let report = install_fonts(&context);
        let mut saw_heading = false;
        let _ = context.run(egui::RawInput::default(), |ctx| {
            let body = ctx.fonts_mut(|fonts| {
                fonts.layout_no_wrap(
                    "四个平台，一个播放入口".to_owned(),
                    font(ts::BODY),
                    Color32::WHITE,
                )
            });
            assert!(
                body.size().x > 100.0,
                "body text must be measurable: {}",
                report.describe()
            );
            let title = ctx.fonts_mut(|fonts| {
                fonts.layout_no_wrap("我的收藏".to_owned(), font_bold(ts::TITLE), Color32::WHITE)
            });
            assert!(title.size().x > 60.0);
            saw_heading = title.size().x > body.size().x / 4.0;
        });
        assert!(saw_heading);
    }

    #[test]
    fn ui_font_scale_matches_the_original_four_steps() {
        // 原版 UiFontPercent 只有 85 / 100 / 115 / 130 四档，超出要限幅。
        assert_eq!(clamp_scale_percent(100.0), 100.0);
        assert_eq!(clamp_scale_percent(50.0), 85.0);
        assert_eq!(clamp_scale_percent(500.0), 130.0);
        assert_eq!(clamp_scale_percent(f32::NAN), 100.0);
        assert!((scale_font_size(12.0, 115.0) - 13.8).abs() < 1e-5);
        assert!((scale_font_size(23.0, 130.0) - 29.9).abs() < 1e-4);
        assert!((scale_font_size(12.0, 85.0) - 10.2).abs() < 1e-5);
    }

    #[test]
    fn bundled_sans_preference_reports_harmony_first_and_notes_missing_medium() {
        let context = egui::Context::default();
        let report = install_fonts_with(
            &context,
            FontOptions {
                preference: FontPreference::BundledSans,
                scale_percent: 115.0,
            },
        );
        assert!(
            report.ui_family.contains("HarmonyOS"),
            "bundled preference must report HarmonyOS first: {}",
            report.describe()
        );
        assert!(report.bundled_fallback);
        assert!(
            report.notes.iter().any(|note| note.contains("Regular")),
            "missing Medium weight must be recorded: {:?}",
            report.notes
        );
        // 不改全局字号倍数：并行测试共享进程状态，只验证纯函数换算。
        assert!((scale_font_size(12.0, 115.0) - 13.8).abs() < 1e-5);
    }

    #[test]
    fn font_options_from_settings_maps_keys_and_clamps_percent() {
        // auto/system/雅黑写法都落到系统优先（C# WorkbenchFonts 行为）。
        assert_eq!(
            FontOptions::from_settings("auto", 100).preference,
            FontPreference::SystemUi
        );
        assert_eq!(
            FontOptions::from_settings("system", 115).preference,
            FontPreference::SystemUi
        );
        assert_eq!(
            FontOptions::from_settings("yahei-ui", 100).preference,
            FontPreference::SystemUi
        );
        assert_eq!(
            FontOptions::from_settings("yahei", 100).preference,
            FontPreference::SystemUi
        );
        // harmony / bundled 落到内置鸿蒙优先。
        assert_eq!(
            FontOptions::from_settings("harmony", 130).preference,
            FontPreference::BundledSans
        );
        assert_eq!(
            FontOptions::from_settings("bundled", 85).preference,
            FontPreference::BundledSans
        );
        // 空串/未知按 auto。
        assert_eq!(
            FontOptions::from_settings("", 100).preference,
            FontPreference::SystemUi
        );
        assert_eq!(
            FontOptions::from_settings("nonsense", 100).preference,
            FontPreference::SystemUi
        );
        // 字号百分比按原版四档范围限幅。
        assert_eq!(FontOptions::from_settings("auto", 50).scale_percent, 85.0);
        assert_eq!(FontOptions::from_settings("auto", 500).scale_percent, 130.0);
        assert_eq!(FontOptions::from_settings("auto", 115).scale_percent, 115.0);
        assert_eq!(FontOptions::from_settings("auto", 0).scale_percent, 85.0);
    }

    #[test]
    fn ui_font_key_helpers_only_emit_core_accepted_keys() {
        // core::Settings::validate 只接受 auto / harmony / yahei-ui / yahei / system。
        assert_eq!(normalize_ui_font_key("AUTO"), "auto");
        assert_eq!(normalize_ui_font_key(" System "), "system");
        assert_eq!(normalize_ui_font_key("YaHei-UI"), "system");
        assert_eq!(normalize_ui_font_key("harmonyos"), "harmony");
        assert_eq!(normalize_ui_font_key("bundled"), "harmony");
        assert_eq!(normalize_ui_font_key("???"), "auto");
        assert_eq!(normalize_ui_font_key(""), "auto");
        for (key, label) in UI_FONT_KEYS {
            assert!(
                matches!(key, "auto" | "system" | "harmony"),
                "settings key {key} would be rejected by Settings::validate"
            );
            assert!(!label.is_empty());
        }
        assert_eq!(ui_font_key_index("auto"), 0);
        assert_eq!(ui_font_key_index("yahei-ui"), 1);
        assert_eq!(ui_font_key_index("harmony"), 2);
        assert_eq!(UI_FONT_PERCENT_STEPS, [85, 100, 115, 130]);
        assert_eq!(normalize_ui_font_percent(0), 85);
        assert_eq!(normalize_ui_font_percent(100), 100);
        assert_eq!(normalize_ui_font_percent(300), 130);
        assert_eq!(FontPreference::SystemUi.settings_key(), "system");
        assert_eq!(FontPreference::BundledSans.settings_key(), "harmony");
        assert!(!FontPreference::SystemUi.label_zh().is_empty());
        assert!(!FontPreference::BundledSans.label_zh().is_empty());
    }

    #[test]
    fn apply_fonts_themed_rebuilds_styles_with_a_valid_scale() {
        let _guard = FONT_SCALE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let context = egui::Context::default();
        let options = FontOptions::from_settings("system", 115);
        // 纯函数层先把设置映射锁死（确定性）。
        assert!((options.scale_percent - 115.0).abs() < f32::EPSILON);
        assert!(
            (scale_font_size(ts::TITLE, options.scale_percent) - ts::TITLE * 1.15).abs() < 1e-3
        );
        let report = apply_fonts_themed(&context, options, true);
        assert!(
            report.system_ui() || report.bundled_fallback,
            "font report must describe a usable fallback: {}",
            report.describe()
        );
        assert!(context.style().visuals.dark_mode);
        // ⚠️ ui_font_scale 是**进程级**全局量：其它 gui 测试（构造 Launcher、
        // install_fonts）会在并行线程里改它，所以这里不能写死 1.15。只断言
        // 样式被重建成了“四档之一”（任何合法倍数都算），设置→倍数的精确映射
        // 由上面的纯函数断言与 from_settings 单测覆盖。
        let heading = context
            .style()
            .text_styles
            .get(&TextStyle::Heading)
            .map(|font| font.size)
            .expect("heading text style");
        let allowed: Vec<f32> = UI_FONT_PERCENT_STEPS
            .iter()
            .map(|percent| scale_font_size(ts::TITLE, f32::from(*percent)))
            .collect();
        assert!(
            allowed.iter().any(|size| (heading - size).abs() < 0.05),
            "heading {heading} is not one of the four UiFontPercent steps {allowed:?}"
        );
        // 粗体族必须已绑定，并且真的能排版（不 panic）。
        let _ = context.run(egui::RawInput::default(), |ctx| {
            ctx.fonts_mut(|fonts| {
                fonts.layout_no_wrap(
                    "界面字体设置 100%".to_owned(),
                    font_bold(ts::HEADING),
                    Color32::WHITE,
                )
            });
        });
        // 恢复默认倍数，避免影响同进程内的其它测试。
        set_ui_font_scale(100.0);
    }

    #[test]
    fn apply_fonts_keeps_the_current_dark_mode() {
        let _guard = FONT_SCALE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let context = egui::Context::default();
        let _ = apply_fonts_themed(&context, FontOptions::from_settings("auto", 85), false);
        assert!(!context.style().visuals.dark_mode);
        let report = apply_fonts(&context, FontOptions::from_settings("auto", 100));
        assert!(
            !context.style().visuals.dark_mode,
            "apply_fonts must keep light"
        );
        assert!(!report.notes.is_empty() || report.system_ui() || report.bundled_fallback);
        set_ui_font_scale(100.0);
    }

    #[test]
    fn round_rect_inset_matches_corner_geometry() {
        let rect = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(100.0, 50.0));
        let radius = CornerRadius::same(10);
        let (left, right) = round_rect_inset(rect, 0.0, radius);
        assert!((left - 10.0).abs() < 0.01, "left inset at top edge: {left}");
        assert!(
            (right - 10.0).abs() < 0.01,
            "right inset at top edge: {right}"
        );
        let (left, right) = round_rect_inset(rect, 25.0, radius);
        assert!(left.abs() < 0.01 && right.abs() < 0.01);
        let (left, _) = round_rect_inset(rect, 10.0, radius);
        assert!(left.abs() < 0.01);
        let (left, _) = round_rect_inset(rect, 45.0, radius);
        assert!(left > 0.5, "bottom-left corner must inset: {left}");
    }

    /// 测试专用：用指定字体目录构建定义并做探针校验，不安装进全局 Context。
    fn report_for_font_dir(dir: &Path) -> FontReport {
        let mut report = FontReport::default();
        let definitions = build_font_definitions(dir, FontPreference::SystemUi, &mut report);
        let _ = verify_or_fallback(definitions, &mut report);
        report
    }
}
