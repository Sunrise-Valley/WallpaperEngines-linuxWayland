// SPDX-License-Identifier: MIT
//
//! 界面皮肤：**跟随系统主题** + WE 的布局尺寸。
//!
//! 这里只回答「长什么样」（颜色 / 描边 / 圆角 / 控件外观），
//! 交互逻辑一律留在 `app.rs` —— 皮肤层不理解业务。
//!
//! ## 取色策略（本次改动）
//!
//! * **COSMIC 会话**（`XDG_CURRENT_DESKTOP` 含 `COSMIC`）：直接用
//! `cosmic::theme::system_preference` —— 原生 COSMIC 观感，深浅色 / 强调色 /
//! 圆角 / 密度全部跟随系统；系统切换主题时 libcosmic 自己会重发（它订阅了
//! `ThemeMode` 配置），本程序不用再插手。
//!
//! * **其它会话（GNOME / KDE / 无桌面）**：退化成**中性主题** ——
//! `ThemeBuilder` 打底 + Adwaita 蓝强调色 `#3584e4` + 更大的圆角与间距，
//! 深浅色按系统偏好挑（cosmic-config → GTK settings.ini → kdeglobals → 浅色）。
//!
//! * 上次那套**写死的 WE 浅色板已删除**。所有容器 / 按钮 / 滑块 / 勾选框样式都在
//! 渲染时**从当前主题取色**（`t.cosmic`），所以换主题不用改任何调用点。
//! WE 只留下**布局尺寸**（属性行高、控件列宽、16:9 网格、分段线的位置）。
//!
//! ## 尺寸
//!
//! 行高 / 控件列宽 / 图标列这些**布局常量**取样自参考截图（7-1~7-4），
//! 与配色无关，因此保留。

use crate::app::Message;
use cosmic::iced::widget::container::Style as ContStyle;
use cosmic::iced::{Alignment, Background, Border, Color, Length};
use cosmic::theme::{Button as CButton, Theme as CTheme};
use cosmic::widget;
use cosmic::widget::button::Style as BtnStyle;
use cosmic::widget::icon;
use cosmic::Element;

// ── 界面尺寸（取样自参考图，按面板宽度等比落到本程序）──────────────────
/// 属性行的行高与行间距
pub const ROW_H: f32 = 36.0;
pub const ROW_GAP: f32 = 4.0;
/// 控件列宽：滑块（轨道 + 数值列）/ 下拉框 / 勾选框**共用一列并右对齐**。
pub const CTRL_W: f32 = 168.0;
/// 滑块轨道宽（`TRACK_W + 间距 + NUM_W = CTRL_W`）
pub const TRACK_W: f32 = 122.0;
/// 滑块右侧数值列宽（右对齐）
pub const NUM_W: f32 = 40.0;
/// 面板滚动条是**叠在内容上**画的 ⇒ 行内容右侧留这么多
pub const SCROLL_PAD: f32 = 12.0;
/// 每行最左侧图标列
pub const ICON_W: f32 = 16.0;
pub const ICON_SIZE: u16 = 16;
/// 图标与标签之间的间距
pub const ICON_GAP: f32 = 12.0;
/// 属性行 / 分组标题的字号
pub const ROW_TEXT: f32 = 12.5;
/// 勾选框边长
pub const CHECK: f32 = 18.0;
/// 灰底控件（下拉框 / 色块箭头条）高度
pub const FIELD_H: f32 = 28.0;
/// 色块尺寸（起是**圆的直径**）
pub const DOT: f32 = 22.0;
/// 「您的预设」那种小按钮高度
pub const BTN_H: f32 = 26.0;

// ── 会话判定 ──────────────────────────────────────────────────────────
/// 是不是 COSMIC 会话（决定用「原生 COSMIC」还是「中性回退」）。
pub fn is_cosmic_session() -> bool {
    ["XDG_CURRENT_DESKTOP", "XDG_SESSION_DESKTOP", "DESKTOP_SESSION"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .any(|v| v.to_ascii_uppercase().contains("COSMIC"))
}

fn home() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

/// 系统是不是深色偏好（非 COSMIC 会话才用得到；COSMIC 走 `system_preference`）。
///
/// 顺序：cosmic-config 主题模式文件 → GTK `settings.ini` → KDE `kdeglobals` → 浅色。
pub fn system_prefers_dark() -> bool {
    let cfg = home().join(".config/cosmic/com.system76.CosmicTheme.Mode/v1/is_dark");
    if let Ok(s) = std::fs::read_to_string(cfg) {
        return s.trim().trim_matches('"') == "true";
    }
    if let Ok(s) = std::fs::read_to_string(home().join(".config/gtk-3.0/settings.ini")) {
        let l = s.to_ascii_lowercase();
        if l.contains("gtk-application-prefer-dark-theme=1")
            || l.contains("gtk-application-prefer-dark-theme=true")
        {
            return true;
        }
    }
    if let Ok(s) = std::fs::read_to_string(home().join(".config/kdeglobals")) {
        if s.lines().any(|l| {
            let l = l.to_ascii_lowercase();
            l.starts_with("colorscheme=") && l.contains("dark")
        }) {
            return true;
        }
    }
    false
}

/// 非 COSMIC 会话的中性回退：`ThemeBuilder` 打底 + Adwaita 蓝 + 更大圆角/间距。
fn neutral(dark: bool) -> cosmic::Theme {
    use cosmic::cosmic_theme::palette::Srgb;
    use cosmic::cosmic_theme::{CornerRadii, Spacing, ThemeBuilder};

    let mut b = if dark {
        ThemeBuilder::dark()
    } else {
        ThemeBuilder::light()
    };
    // Adwaita 的蓝（GNOME 强调色），中性、不刺眼
    b.accent = Some(Srgb::new(0.208, 0.518, 0.894));
    // Adwaita 的圆角比 COSMIC 大（卡片 12 / 控件 8）
    b.corner_radii = CornerRadii {
        radius_0: [0.0; 4],
        radius_xs: [6.0; 4],
        radius_s: [8.0; 4],
        radius_m: [10.0; 4],
        radius_l: [12.0; 4],
        radius_xl: [14.0; 4],
    };
    // Adwaita 的间距比 COSMIC 松一点
    b.spacing = Spacing {
        space_none: 0,
        space_xxxs: 2,
        space_xxs: 4,
        space_xs: 6,
        space_s: 8,
        space_m: 12,
        space_l: 16,
        space_xl: 24,
        space_xxl: 32,
        space_xxxl: 48,
    };
    // 给容器底一点点区分度由主题派生自动完成，这里不动调色板
    cosmic::theme::Theme::custom(std::sync::Arc::new(b.build()))
}

/// 应用主题：**跟随系统**（COSMIC 上原生，其它桌面上退化为中性）。
///
/// 注意这是**应用级**覆盖：只作用本程序，不写系统配置、不动系统主题。
///
/// **半透明**：`Theme::transparent` 打开后，每一处取色都走 cosmic-theme 的
/// 「模糊档」变体（`background(true)` / `primary(true)` / `secondary(true)`），
/// 那一档的 alpha 是主题按它的模糊强度给的（见 `AlphaMap::blurred_alpha`），
/// 于是窗口/卡片能透出桌面而不是刷一层不透明底色 —— 这就是 COSMIC 自己的观感。
pub fn theme() -> cosmic::Theme {
    let mut t = if is_cosmic_session() {
        cosmic::theme::system_preference()
    } else {
        neutral(system_prefers_dark())
    };
    t.transparent = true;
    t
}

// ── 取色（全部来自**当前主题**）────────────────────────────────────────
/// 当前生效的主题（= 上面 `theme` 交出去的那一份）。
fn cur() -> CTheme {
    cosmic::theme::active()
}

/// **诊断用**：`WPHUB_DUMP_THEME=1` 启动时把「半透明到底用的是哪一档」打到 stderr。
///
/// 三个 alpha 来自 cosmic-theme 的 `background/primary/secondary(transparent)`，
/// 它们的值是主题按自己的模糊强度（`AlphaMap::blurred_alpha(frosted)`）算的，
/// 我们一个数字都不写死 —— 打印出来是为了核对「窗口/卡片确实是半透明的」。
pub fn dump_theme_layers() {
    if std::env::var("WPHUB_DUMP_THEME").as_deref() != Ok("1") {
        return;
    }
    let t = theme();
    let c = t.cosmic();
    eprintln!(
        "[theme] 会话={} Theme::transparent={} is_dark={} 强调色={:?}",
        if is_cosmic_session() { "COSMIC" } else { "其它(中性回退)" },
        t.transparent,
        c.is_dark,
        c.accent_color()
    );
    for (name, a) in [
        ("窗口背景层 bg(false)", c.background(false).base.alpha),
        ("窗口背景层 bg(true) ", c.background(true).base.alpha),
        ("主容器 primary(false)", c.primary(false).base.alpha),
        ("主容器 primary(true) ", c.primary(true).base.alpha),
        ("次级容器 secondary(false)", c.secondary(false).base.alpha),
        ("次级容器 secondary(true) ", c.secondary(true).base.alpha),
        ("控件底 bg.component(true)", c.background(true).component.base.alpha),
    ] {
        eprintln!("[theme]   {name} alpha = {a:.5}");
    }
}

fn mix(a: Color, b: Color, t: f32) -> Color {
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

// ── 三种容器层（全部走主题的**半透明变体**）──────────────────────────────
//
// cosmic-theme 对每一层都准备了两份：`background(false)` 是给不透明窗口用的，
// `background(true)` 是给**开了模糊/半透明**的窗口用的（alpha 由主题的
// `AlphaMap::blurred_alpha(frosted)` 决定，深色默认 0.76154）。
// 本程序一律用后者（`theme` 把 `Theme::transparent` 置为 true）。
/// 窗口背景层（`bg`）
fn bg_layer(t: &CTheme) -> &cosmic::cosmic_theme::Container {
    t.cosmic().background(t.transparent)
}

/// 主容器层（卡片 / 按钮底）
fn primary_layer(t: &CTheme) -> &cosmic::cosmic_theme::Container {
    t.cosmic().primary(t.transparent)
}

/// 次级容器层（右侧详情面板）
fn secondary_layer(t: &CTheme) -> &cosmic::cosmic_theme::Container {
    t.cosmic().secondary(t.transparent)
}

/// 正文色（当前容器层的 on 色）
pub fn ink_text() -> Color {
    bg_layer(&cur()).on.into()
}

/// 次要文字：正文色往底色方向退一截（两个主题下都成立，不用写死灰）
pub fn ink_dim() -> Color {
    let t = cur();
    mix(bg_layer(&t).on.into(), bg_layer(&t).base.into(), 0.42)
}

/// 相对亮度（sRGB，粗略够用）—— 主题没给对比色时用来挑黑/白。
fn luminance(c: Color) -> f32 {
    0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b
}

/// 主题给的「压在底色上的字色」能不能用。
/// COSMIC 的**系统主题**里 `accent_text` 可能是 `None`，而 `accent_text_color` 在那种情况下
/// 直接退回 `accent.base` ⇒ **绿底绿字、按钮文字整个看不见**（实测踩过）。
/// 这里加一道对比检查：太接近就自己按亮度挑黑/白。
fn readable_on(bg: Color, themed: Color) -> Color {
    let diff = (themed.r - bg.r).abs() + (themed.g - bg.g).abs() + (themed.b - bg.b).abs();
    if diff > 0.15 {
        themed
    } else if luminance(bg) > 0.55 {
        Color::from_rgb(0.06, 0.06, 0.07)
    } else {
        WHITE
    }
}

/// 强调色（选中标签 / 勾选框填充 / 滑块手柄 / 链接）
pub fn ink_accent() -> Color {
    cur().cosmic().accent_color().into()
}

/// 压在强调色上的文字（强调色的对比色；主题没给就按亮度算）
pub fn ink_on_accent() -> Color {
    let c = cur();
    let bg: Color = c.cosmic().accent_color().into();
    readable_on(bg, c.cosmic().accent_text_color().into())
}

/// 危险色（取消订阅 / 重置那种通栏红按钮）
pub fn ink_danger() -> Color {
    cur().cosmic().destructive_color().into()
}

/// 压在危险色上的文字（同样带对比兜底）
pub fn ink_on_danger() -> Color {
    let c = cur();
    let bg: Color = c.cosmic().destructive_color().into();
    readable_on(bg, c.cosmic().destructive_text_color().into())
}

/// 纯白（只用在**半透明黑条**上：缩略图标题、角标 —— 与主题无关）
pub const WHITE: Color = Color::from_rgb(1.0, 1.0, 1.0);
/// 缩略图底部标题条（半透明黑）
pub const BAR: Color = Color::from_rgba(0.0, 0.0, 0.0, 0.55);

/// 主题里的小圆角（跟随主题：COSMIC 小、Adwaita 大）
fn radius_s(t: &CTheme) -> f32 {
    t.cosmic().radius_s()[0]
}

// ── 容器样式（函数形式，可直接当 `.style(f)` 用）──────────────────────
/// 窗口底：**不刷不透明底色**。
///
/// libcosmic 自己会给窗口画一层「主题背景层」（`background(theme.transparent).base`
/// 带 alpha 的半透明变体 + 窗口圆角 + 一圈描边，见 libcosmic `app/mod.rs`）。
/// 我们这一层再刷不透明色就会把它盖掉、窗口又变成不透明方块，所以在里面留空；
/// 只在框架**不**画那层时（`core.window.content_container == false`，本程序不会）才补色。
pub fn window_style(t: &CTheme) -> ContStyle {
    ContStyle {
        background: None,
        text_color: Some(bg_layer(t).on.into()),
        ..ContStyle::default()
    }
}

/// 卡片（主容器）—— 主题的 primary 半透明档
pub fn card_style(t: &CTheme) -> ContStyle {
    let c = primary_layer(t);
    ContStyle {
        background: Some(Background::Color(c.base.into())),
        border: Border {
            color: c.component.border.into(),
            width: 1.0,
            radius: radius_s(t).into(),
        },
        text_color: Some(c.on.into()),
        ..ContStyle::default()
    }
}

/// 面板（次级容器，比卡片低一档）—— 主题的 secondary 半透明档
pub fn panel_style(t: &CTheme) -> ContStyle {
    let c = secondary_layer(t);
    ContStyle {
        background: Some(Background::Color(c.base.into())),
        border: Border {
            color: c.component.border.into(),
            width: 1.0,
            radius: radius_s(t).into(),
        },
        text_color: Some(c.on.into()),
        ..ContStyle::default()
    }
}

/// 圆角 chip（类型 / 分辨率 / 分级…）
pub fn chip_style(t: &CTheme) -> ContStyle {
    let c = primary_layer(t);
    ContStyle {
        background: Some(Background::Color(c.component.base.into())),
        border: Border {
            color: c.component.border.into(),
            width: 1.0,
            radius: radius_s(t).into(),
        },
        text_color: Some(c.component.on.into()),
        ..ContStyle::default()
    }
}

/// 灰底控件（下拉框 / 「您的预设」那排按钮）：控件底色
pub fn field_style(t: &CTheme) -> ContStyle {
    let c = primary_layer(t);
    ContStyle {
        background: Some(Background::Color(c.component.base.into())),
        border: Border {
            color: c.component.border.into(),
            width: 0.0,
            radius: radius_s(t).into(),
        },
        text_color: Some(c.component.on.into()),
        ..ContStyle::default()
    }
}

/// 缩略图底部的半透明黑标题条：白字（与主题无关）
pub fn bar_style(_t: &CTheme) -> ContStyle {
    ContStyle {
        background: Some(Background::Color(BAR)),
        text_color: Some(WHITE),
        ..ContStyle::default()
    }
}

/// 按钮样式
fn btn_style(bg: Option<Color>, text: Color, border: Option<Color>, border_w: f32, radius: f32) -> BtnStyle {
    let mut s = BtnStyle::default();
    s.background = bg.map(Background::Color);
    s.text_color = Some(text);
    s.icon_color = Some(text);
    s.border_radius = radius.into();
    if let Some(c) = border {
        s.border_color = c;
        s.border_width = border_w;
    }
    s
}

/// 一个「颜色角色」，渲染时按当前主题解析 —— 这样换主题不用改调用点。
#[derive(Clone, Copy)]
enum Role {
    Accent,
    Danger,
    Component,
    Ghost,
    Bare,
}

impl Role {
    fn colors(self, t: &CTheme) -> (Option<Color>, Color, Option<Color>, f32) {
        let c = t.cosmic();
        let comp = &bg_layer(t).component;
        match self {
            // 强调色底 + 对比字色（对比色主题可能没给 ⇒ 走 readable_on 兜底）
            Role::Accent => {
                let bg: Color = c.accent_color().into();
                (
                    Some(bg),
                    readable_on(bg, c.accent_text_color().into()),
                    None,
                    0.0,
                )
            }
            // 危险色底
            Role::Danger => {
                let bg: Color = c.destructive_color().into();
                (
                    Some(bg),
                    readable_on(bg, c.destructive_text_color().into()),
                    None,
                    0.0,
                )
            }
            // 灰控件底（列表底上的按钮）：用 **primary 层的 component** 而不是窗口层的
            // 顶栏那几个按钮直接浮在（更透的）窗口背景层上，用窗口层的 0.28 alpha
            // 会被桌面压得读不出来；primary 层的 component 是主题给「容器上的控件」那一档。
            Role::Component => {
                let p = &primary_layer(t).component;
                (Some(p.base.into()), p.on.into(), None, 0.0)
            }
            // 主容器底 + 描边
            Role::Ghost => {
                let p = primary_layer(t);
                (
                    Some(p.base.into()),
                    p.on.into(),
                    Some(comp.border.into()),
                    1.0,
                )
            }
            // 全透明（工具条上的文字按钮）
            Role::Bare => (None, bg_layer(t).on.into(), None, 0.0),
        }
    }

    fn hover_bg(self, t: &CTheme) -> Option<Color> {
        let c = t.cosmic();
        match self {
            Role::Accent => Some(c.accent_color().into()),
            Role::Danger => Some(c.destructive_color().into()),
            Role::Component | Role::Bare => Some(primary_layer(t).component.hover.into()),
            Role::Ghost => Some(primary_layer(t).component.hover.into()),
        }
    }

    fn pressed_bg(self, t: &CTheme) -> Option<Color> {
        let c = t.cosmic();
        match self {
            Role::Accent => Some(c.accent_color().into()),
            Role::Danger => Some(c.destructive_color().into()),
            Role::Component | Role::Bare => Some(primary_layer(t).component.pressed.into()),
            Role::Ghost => Some(primary_layer(t).component.pressed.into()),
        }
    }
}

fn custom_class(role: Role) -> CButton {
    fn role_style(r: Role, t: &CTheme) -> BtnStyle {
        let (bg, text, border, bw) = r.colors(t);
        btn_style(bg, text, border, bw, radius_s(t))
    }
    CButton::Custom {
        active: Box::new(move |_sel, t| role_style(role, t)),
        hovered: Box::new(move |_sel, t| {
            let (bg, text, border, bw) = role.colors(t);
            btn_style(role.hover_bg(t).or(bg), text, border, bw, radius_s(t))
        }),
        pressed: Box::new(move |_sel, t| {
            let (bg, text, border, bw) = role.colors(t);
            btn_style(role.pressed_bg(t).or(bg), text, border, bw, radius_s(t))
        }),
        disabled: Box::new(move |t| {
            let (bg, text, border, bw) = role.colors(t);
            btn_style(
                bg.map(|c| Color { a: c.a * 0.45, ..c }),
                text,
                border,
                bw,
                radius_s(t),
            )
        }),
    }
}

/// 强调色底（确认 / 应用 / 选中标签）
pub fn filled_class() -> CButton {
    custom_class(Role::Accent)
}

/// 灰底小按钮（顶栏右侧、属性面板那一排）
pub fn gray_class() -> CButton {
    custom_class(Role::Component)
}

/// 通栏红（取消订阅 / 重置）
pub fn red_class() -> CButton {
    custom_class(Role::Danger)
}

/// 容器底 + 描边（未选中标签 / 次级按钮）
pub fn ghost_class() -> CButton {
    custom_class(Role::Ghost)
}

/// 完全透明（工具条上的文字按钮）
pub fn bare_class() -> CButton {
    custom_class(Role::Bare)
}

/// 顶栏标签：选中 = 强调色实心，未选中 = 容器底描边
pub fn tab_class(selected: bool) -> CButton {
    if selected {
        custom_class(Role::Accent)
    } else {
        ghost_class()
    }
}

/// 勾选框：勾上 = 强调色实心方块（对比色勾），未勾 = 容器底 + 细描边
pub fn check_class(on: bool) -> CButton {
    if on {
        custom_class(Role::Accent)
    } else {
        custom_class(Role::Ghost)
    }
}

/// 勾选框：勾上 = 强调色实心 + 对比色勾，未勾 = 容器底细描边方块。
pub fn checkbox<'a>(on: bool, msg: Message) -> Element<'a, Message> {
    let glyph = if on { "✓" } else { "" };
    let ink = if on { ink_on_accent() } else { Color::TRANSPARENT };
    let inner = widget::container(label(glyph, 12.0, ink))
        .width(Length::Fixed(CHECK))
        .height(Length::Fixed(CHECK))
        .align_x(Alignment::Center)
        .align_y(Alignment::Center);
    widget::button::custom(inner)
        .class(check_class(on))
        .padding(0)
        .on_press_down(msg)
        .into()
}

/// 取色板里的一格纯色（**圆形**色点按钮；`d` = 直径，radius 就是 d/2）
/// 原来是圆角矩形色块；预设色统一成圆点，和色盘本身同一个视觉语言。
pub fn dot_class(r: f32, g: f32, b: f32, d: f32) -> CButton {
    let c = Color::from_rgb(r, g, b);
    let rad = d / 2.0;
    CButton::Custom {
        active: Box::new(move |_sel, _t| btn_style(Some(c), WHITE, Some(c), 1.0, rad)),
        hovered: Box::new(move |_sel, _t| btn_style(Some(c), WHITE, Some(WHITE), 2.0, rad)),
        pressed: Box::new(move |_sel, _t| btn_style(Some(c), WHITE, Some(WHITE), 2.0, rad)),
        disabled: Box::new(move |_t| {
            btn_style(Some(Color { a: c.a * 0.45, ..c }), WHITE, Some(c), 1.0, rad)
        }),
    }
}

/// 一个**不可点**的圆形色点（当前色预览）。
pub fn color_dot<'a>(d: f32, r: f32, g: f32, b: f32) -> Element<'a, Message> {
    widget::container(cosmic::iced::widget::space::horizontal())
        .width(Length::Fixed(d))
        .height(Length::Fixed(d))
        .style(move |t: &CTheme| ContStyle {
            background: Some(Background::Color(Color::from_rgb(
                r.clamp(0.0, 1.0),
                g.clamp(0.0, 1.0),
                b.clamp(0.0, 1.0),
            ))),
            border: Border {
                color: bg_layer(t).component.border.into(),
                width: 1.0,
                radius: (d / 2.0).into(),
            },
            ..ContStyle::default()
        })
        .into()
}

/// 预设色的圆点按钮（点击 ⇒ 选中这个颜色）
pub fn dot_button<'a>(d: f32, r: f32, g: f32, b: f32, msg: Message) -> Element<'a, Message> {
    widget::button::custom(widget::container(cosmic::iced::widget::space::horizontal()))
        .width(Length::Fixed(d))
        .height(Length::Fixed(d))
        .class(dot_class(r, g, b, d))
        .padding(0)
        .on_press_down(msg)
        .into()
}

/// 滑块：细轨道 + 小矩形手柄，颜色全部取自主题。
///
/// 注意：libcosmic 的主题里 `Slider` 的样式类是 `Slider::Custom { active, hovered, dragging }`
/// （三个 `Rc<dyn Fn>`），**不能**像裸 iced 那样直接传一个闭包 —— 这里包一层。
fn slider_appearance(t: &CTheme, hot: bool) -> widget::slider::Style {
    use cosmic::iced::widget::slider::{Breakpoint, Handle, HandleShape, Rail, Style};
    let c = t.cosmic();
    let rail = bg_layer(t).component.base;
    let handle = if hot {
        bg_layer(t).component.pressed
    } else {
        c.accent_color()
    };
    Style {
        rail: Rail {
            backgrounds: (Background::Color(rail.into()), Background::Color(rail.into())),
            width: 4.0,
            border: Border::default(),
        },
        handle: Handle {
            shape: HandleShape::Rectangle {
                width: 8,
                height: 24,
                border_radius: cosmic::iced::border::Radius::from(2.0),
            },
            background: Background::Color(handle.into()),
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
        },
        breakpoint: Breakpoint {
            color: Color::TRANSPARENT,
        },
    }
}

/// 给 `.style(...)` 用的滑块样式类（见 `slider_appearance` 的说明）。
pub fn slider_class() -> cosmic::theme::iced::Slider {
    use cosmic::theme::iced::Slider;
    Slider::Custom {
        active: std::rc::Rc::new(|t: &CTheme| slider_appearance(t, false)),
        hovered: std::rc::Rc::new(|t: &CTheme| slider_appearance(t, true)),
        dragging: std::rc::Rc::new(|t: &CTheme| slider_appearance(t, true)),
    }
}

/// 属性面板里的下拉框：**控件底色那块是容器**（`field_style`），pick_list 自身在
/// libcosmic 里是透明无边的 —— 两层叠起来正好是 combo 行那块方底。
pub fn dropdown<'a>(pick: Element<'a, Message>) -> Element<'a, Message> {
    widget::container(pick)
        .width(Length::Fill)
        .height(Length::Fixed(FIELD_H))
        .align_y(Alignment::Center)
        .style(field_style)
        .into()
}

/// 缩略图左上角那种小角标（纯色圆角，白字）
pub fn badge_style(bg: Color) -> impl Fn(&CTheme) -> ContStyle + Copy {
    move |t: &CTheme| ContStyle {
        background: Some(Background::Color(bg)),
        text_color: Some(WHITE),
        border: Border {
            radius: radius_s(t).into(),
            ..Border::default()
        },
        ..ContStyle::default()
    }
}

// ── 小组件 ────────────────────────────────────────────────────────────
// （真机实测）：这些按钮以前用 `on_press`（按下→**松开**才发消息）。
// 在自研内核这个壳里，**详情面板里的按钮松开时永远不触发**：按下那一刻
// `cursor.is_over(bounds)` 为真（内核日志里能打到按钮自己算出来的矩形，
// 例：x=852 y=828.8 258x26.8，指针正好在中心），松开时同一个坐标却判定
// "不在按钮上"（事件仍被判为 Captured ⇒ 走的是 is_pressed 分支，只是 is_over 为假）。
// 改成 `on_press_down`（按下即触发）后同一个坐标立刻生效（实测两次切换都成功），
// 而且对按钮来说"按下去就生效"本来就是用户预期。全部按钮统一走这条路，
// 免得同一个界面里一半按钮按一下有反应、一半没反应。
/// 顶栏/工具条上那种带图标的小按钮
pub fn icon_button<'a>(
    label: impl Into<std::borrow::Cow<'a, str>> + 'a,
    icon_name: &'a str,
    class: CButton,
    msg: Message,
) -> Element<'a, Message> {
    let content = widget::row::with_capacity(2)
        .push(icon::from_name(icon_name).size(14))
        .push(widget::text(label).size(12))
        .spacing(4)
        .align_y(Alignment::Center);
    widget::button::custom(content)
        .class(class)
        .padding([3, 8])
        .on_press_down(msg)
        .into()
}

/// 通栏小按钮（带图标、居中对齐）—— 「您的预设」那一排
pub fn wide_button<'a>(
    label: impl Into<std::borrow::Cow<'a, str>> + 'a,
    icon_name: &'a str,
    class: CButton,
    msg: Message,
) -> Element<'a, Message> {
    let row = widget::row::with_capacity(2)
        .push(icon::from_name(icon_name).size(14))
        .push(widget::text(label).size(12))
        .spacing(5)
        .align_y(Alignment::Center);
    widget::button::custom(widget::container(row).align_x(Alignment::Center))
        .class(class)
        .padding([4, 8])
        .height(Length::Fixed(BTN_H))
        .width(Length::Fill)
        .on_press_down(msg)
        .into()
}

/// 圆角浅灰 chip（类型 / 分辨率 / 分级…）
pub fn chip<'a>(label: impl Into<String>) -> Element<'a, Message> {
    let label = label.into();
    widget::container(widget::text(label).size(11))
        .padding([2, 7])
        .style(chip_style)
        .into()
}

/// 指定颜色/字号的文本（libcosmic 的 Text 不能直接 .color，得走样式类）
pub fn label<'a>(
    s: impl Into<std::borrow::Cow<'a, str>> + 'a,
    size: f32,
    color: Color,
) -> Element<'a, Message> {
    widget::text(s)
        .size(size)
        .class(cosmic::theme::Text::Color(color))
        .into()
}

/// 一像素细线（颜色取自主题的分割色）
pub fn divider<'a>() -> Element<'a, Message> {
    use cosmic::iced::widget::space;
    widget::container(space::horizontal())
        .width(Length::Fill)
        .height(Length::Fixed(1.0))
        .style(|t: &CTheme| ContStyle {
            background: Some(Background::Color(primary_layer(t).divider.into())),
            ..ContStyle::default()
        })
        .into()
}

/// 占**一整行**高的分割线：上下各留出和属性行一样的间距
pub fn row_divider<'a>() -> Element<'a, Message> {
    widget::container(divider())
        .width(Length::Fill)
        .height(Length::Fixed(ROW_H))
        .align_y(Alignment::Center)
        .into()
}

/// 属性面板里的取色控件：**一个圆形色点**（当前色预览）+ 展开箭头，整体右对齐。
/// 原来是 `[24x20 矩形色块][▾ 条]`；收起状态也是圆的（直径 `DOT`），
/// 点开才是完整色盘（`crate::color_wheel`）。
pub fn color_field<'a>(r: f32, g: f32, b: f32, msg: Message) -> Element<'a, Message> {
    let arrow = widget::container(label("▾", 10.0, ink_dim()))
        .width(Length::Fixed(14.0))
        .height(Length::Fixed(DOT))
        .align_x(Alignment::Center)
        .align_y(Alignment::Center);
    widget::button::custom(
        widget::row::with_capacity(2)
            .push(color_dot(DOT, r, g, b))
            .push(arrow)
            .spacing(2)
            .align_y(Alignment::Center),
    )
    .class(bare_class())
    .padding(0)
    .on_press_down(msg)
    .into()
}

/// 分组标题：左侧标题文字 + 一条**延伸到右边的引线**
pub fn section<'a>(title: impl Into<std::borrow::Cow<'a, str>> + 'a) -> Element<'a, Message> {
    use cosmic::iced::widget::space;
    let rule = widget::container(space::horizontal())
        .width(Length::Fill)
        .height(Length::Fixed(1.0))
        .style(|t: &CTheme| ContStyle {
            background: Some(Background::Color(primary_layer(t).divider.into())),
            ..ContStyle::default()
        });
    widget::container(
        widget::row::with_capacity(2)
            .push(label(title, ROW_TEXT, ink_text()))
            .push(rule)
            .spacing(6)
            .align_y(Alignment::Center)
            .width(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fixed(ROW_H))
    .align_y(Alignment::Center)
    .into()
}

/// 属性面板里的一行：左「[小图标] + 标签」，右「控件」。
///
/// * **引擎内置项**传 `Some(图标名)` —— 每行带头像；
///
/// * **作者自定义项**传 `None` —— 只有标签 + 控件、不带图标，标签落在图标列左缘。
///
/// 控件统一放进 `CTRL_W` 宽、**右对齐**的一列。
pub fn prop_row<'a>(
    icon_name: Option<&'a str>,
    text: impl Into<std::borrow::Cow<'a, str>> + 'a,
    control: Element<'a, Message>,
) -> Element<'a, Message> {
    prop_row_h(icon_name, text, control, ROW_H)
}

/// 同 [`prop_row`]，但行高可指定 —— 色盘这类**比一般控件高**的控件用
/// （36px 的固定行高塞不下圆盘；其余行高口径保持不变）。
pub fn prop_row_h<'a>(
    icon_name: Option<&'a str>,
    text: impl Into<std::borrow::Cow<'a, str>> + 'a,
    control: Element<'a, Message>,
    height: f32,
) -> Element<'a, Message> {
    let mut row = widget::row::with_capacity(3)
        .spacing(ICON_GAP)
        .align_y(Alignment::Center)
        .width(Length::Fill);
    if let Some(name) = icon_name {
        row = row.push(
            widget::container(icon::from_name(name).size(ICON_SIZE))
                .width(Length::Fixed(ICON_W))
                .align_x(Alignment::Center),
        );
    }
    row = row.push(widget::container(label(text, ROW_TEXT, ink_text())).width(Length::Fill));
    row = row.push(
        widget::container(control)
            .width(Length::Fixed(CTRL_W))
            .align_x(Alignment::End)
            .align_y(Alignment::Center),
    );
    widget::container(row)
        .width(Length::Fill)
        .height(Length::Fixed(height))
        .align_y(Alignment::Center)
        .into()
}

/// 滑块那一行控件：`[轨道 TRACK_W] [数值列 NUM_W 右对齐]`。
pub fn slider_row<'a>(slider: Element<'a, Message>, value_text: String) -> Element<'a, Message> {
    widget::row::with_capacity(2)
        .push(widget::container(slider).width(Length::Fixed(TRACK_W)))
        .push(
            widget::container(label(value_text, 12.0, ink_dim()))
                .width(Length::Fixed(NUM_W))
                .align_x(Alignment::End),
        )
        .spacing(6)
        .align_y(Alignment::Center)
        .into()
}

/// 万一框架不画窗口背景层（`core.window.content_container == false`）时用的兜底：
/// 自己刷一层**主题的半透明背景层**（不是不透明色）。
pub fn window_style_fallback(t: &CTheme) -> ContStyle {
    ContStyle {
        background: Some(Background::Color(bg_layer(t).base.into())),
        text_color: Some(bg_layer(t).on.into()),
        ..ContStyle::default()
    }
}

/// **作者自己的分组/横幅标题**：属性 `text` 是一整块文字型排版（`<h3>🎚️全局 Global</h3>`、
/// `<h4>💷…赞助…</h4>` 这种）时用它 —— 只读、加粗、下面压一条细线。
///
/// 作者把 `general.properties` 当排版区用，这些条目**不是给用户填的**，
/// 所以一律不出可编辑控件（判据见 `app::classify_prop`，只看类型与 text 的内容，不看属性名）。
pub fn author_heading<'a>(title: impl Into<std::borrow::Cow<'a, str>> + 'a) -> Element<'a, Message> {
    let head = widget::text(title)
        .size(ROW_TEXT)
        .font(cosmic::font::semibold())
        .class(cosmic::theme::Text::Color(ink_text()))
        .width(Length::Fill);
    widget::container(
        widget::column::with_capacity(2)
            .push(head)
            .push(divider())
            .spacing(4.0)
            .width(Length::Fill),
    )
    .width(Length::Fill)
    .padding([6.0, 0.0, 2.0, 0.0])
    .into()
}

/// **只读一行**：作者项有可读文字、但没有可编辑语义（不是 bool/slider/color/combo/textinput，
/// 也没有用户填过的文本值）时用它 —— 不给输入框，只把作者的原话淡淡地摆出来。
pub fn readonly_note<'a>(text: impl Into<std::borrow::Cow<'a, str>> + 'a) -> Element<'a, Message> {
    widget::container(
        widget::row::with_capacity(1)
            .push(label(text, ROW_TEXT, ink_dim()))
            .width(Length::Fill),
    )
    .width(Length::Fill)
    .height(Length::Fixed(ROW_H))
    .align_y(Alignment::Center)
    .padding([0.0, SCROLL_PAD, 0.0, 0.0])
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::theme::Theme as T;

    /// sRGB → 相对亮度（WCAG 那一套）
    fn lin(c: f32) -> f32 {
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    fn lum(c: Color) -> f32 {
        0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
    }
    /// 对比度（WCAG，1:1 ~ 21:1）
    fn contrast(a: Color, b: Color) -> f32 {
        let (l1, l2) = (lum(a), lum(b));
        let (hi, lo) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
        (hi + 0.05) / (lo + 0.05)
    }
    /// 把半透明的 `fg` 叠在 `bg` 上（alpha 混合），得到实际看到的颜色
    fn over(fg: Color, bg: Color) -> Color {
        Color {
            r: fg.r * fg.a + bg.r * (1.0 - fg.a),
            g: fg.g * fg.a + bg.g * (1.0 - fg.a),
            b: fg.b * fg.a + bg.b * (1.0 - fg.a),
            a: 1.0,
        }
    }
    const DESKTOPS: [(&str, Color); 3] = [
        ("桌面纯黑", Color::from_rgb(0.0, 0.0, 0.0)),
        ("桌面中灰", Color::from_rgb(0.5, 0.5, 0.5)),
        ("桌面纯白", Color::from_rgb(1.0, 1.0, 1.0)),
    ];

    /// ② 半透明：窗口底**不许再刷不透明色**，窗口/卡片/面板全部走主题的带 alpha 变体；
    /// 并且「字压在叠了桌面的底上」仍然够对比度（WCAG AA 4.5:1）。
///
    /// 数字用 `cargo test --release -- --nocapture transparency` 复现。
    #[test]
    fn transparency_uses_theme_alpha_and_stays_readable() {
        // 1) **运行时的那一份主题**：就是 app 交给框架的 `theme`（系统主题 + 半透明档）
        let sys = theme();
        assert!(sys.transparent, "Theme::transparent 没打开，样式会退回不透明档");
        let c = sys.cosmic();
        eprintln!(
            "[theme] 会话={} is_dark={} | 窗口 bg (false/true) {:.5}/{:.5} | primary {:.5}/{:.5} | secondary {:.5}/{:.5} | 控件底(true) {:.5}",
            if is_cosmic_session() { "COSMIC" } else { "中性回退" },
            c.is_dark,
            c.background(false).base.alpha,
            c.background(true).base.alpha,
            c.primary(false).base.alpha,
            c.primary(true).base.alpha,
            c.secondary(false).base.alpha,
            c.secondary(true).base.alpha,
            c.background(true).component.base.alpha,
        );
        assert!(
            c.background(true).base.alpha < 0.999,
            "窗口背景层还是全不透明：半透明档没生效"
        );

        // 2) 窗口那一层样式不再刷底色（框架自己画主题的半透明层）
        let style = window_style(&sys);
        assert!(style.background.is_none(), "窗口底还在刷底色，会把半透明层盖掉");
        match window_style_fallback(&sys).background {
            Some(Background::Color(col)) => assert!(
                col.a < 0.999,
                "兜底层也必须带 alpha，实际 {}",
                col.a
            ),
            other => panic!("兜底层必须有底色，实际 {other:?}"),
        }

        // 3) 对比度：深色（系统当前）+ 浅色（内置那套，alpha 同机制）两个主题，三种极端桌面
        for (name, mut t) in [("深色/系统当前", sys.clone()), ("浅色/内置", T::light())] {
            t.transparent = true;
            let cc = t.cosmic();
            let win: Color = cc.background(true).base.into();
            let card: Color = cc.primary(true).base.into();
            let panel: Color = cc.secondary(true).base.into();
            let chip: Color = cc.primary(true).component.base.into();
            for (case, desk) in DESKTOPS {
                let win_over_desk = over(win, desk);
                let card_bg = over(card, win_over_desk);
                let panel_bg = over(panel, win_over_desk);
                let chip_bg = over(chip, panel_bg);
                let r_card = contrast(cc.primary(true).on.into(), card_bg);
                let r_panel = contrast(cc.secondary(true).on.into(), panel_bg);
                let r_chip = contrast(cc.primary(true).component.on.into(), chip_bg);
                eprintln!(
                    "[{name}] {case}: 窗口底 alpha={:.3} | 卡片字 {r_card:.2}:1 | 面板字 {r_panel:.2}:1 | 控件字 {r_chip:.2}:1",
                    win.a
                );
                assert!(r_card >= 4.5, "{name}/{case}: 卡片正文 {r_card:.2}:1 不够");
                assert!(r_panel >= 4.5, "{name}/{case}: 面板正文 {r_panel:.2}:1 不够");
                assert!(r_chip >= 3.0, "{name}/{case}: 控件文字 {r_chip:.2}:1 不够");
            }
        }
    }

    /// 本程序的主题必须**显式打开**半透明档（否则所有样式都会退回不透明变体）。
    #[test]
    fn theme_opts_into_the_transparent_layer() {
        assert!(theme().transparent, "Theme::transparent 没打开");
    }
}
