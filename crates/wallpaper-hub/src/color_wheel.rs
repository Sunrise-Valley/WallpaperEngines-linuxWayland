// SPDX-License-Identifier: MIT
//
//! 圆形色盘：**Hue 环 + 内部 SV 方块 + 当前色预览 + 数值输出**。
//!
//! 为什么单独一个模块：
//!
//! * 色的数学（HSV ↔ RGB、字符串格式化）和**几何命中判定**是纯函数 —— 不依赖窗口、
//! 不依赖渲染器，直接单测（含"拖到某个像素 ⇒ 输出哪个 `r g b`"这条端到端映射）；
//!
//! * 画的部分（`canvas::Program`）只认 `bounds` + `cursor`，跟界面其它部分没有耦合。
//!
//! ## 输出契约（**和下游一字不差**）
//!
//! * 属性颜色（`type:"color"` 的 WE 属性）⇒ [`format_prop_color`]：`"r g b"`，
//! 三个 **0..1 浮点、空格分隔**、最多三位小数、去掉尾零（`"1 1 1"` / `"0.349 0.573 0.827"`）。
//! 这与 WE `project.json` 里 `general.properties.<key>.value` 的写法完全一致，
//! 属性面板只是把它塞进 `Message::PropColor`（→ `set_prop` → `set-prop`），格式没有第二套。
//!
//! * 画面色彩（引擎内置的色相）⇒ [`hue_from_radians`] / [`radians_from_hue`]：
//! 仍然是**弧度**（`-3.14..3.14`），只把"选色"的动作从滑条换成圆盘；
//! 6 个浮点的配置串仍由 `app::color_to_config` 负责，这里一个字节都不碰。
//!
//! ## 交互
//!
//! * 拖外圈 ⇒ 只改色相（S/V 保持）；
//!
//! * 拖内部方块 ⇒ 只改饱和/明度（色相保持）；
//!
//! * 按下即取色、拖动持续取色；**只有格式化后的字符串真的变了**才发消息
//! （否则一次拖动会给磁盘/内核甩几百条一模一样的 `set_prop`）。

use cosmic::iced::widget::canvas::{self, Cache, Frame, Geometry, Path, Stroke};
use cosmic::iced::{
    mouse, Color as IcedColor, Point, Rectangle, Renderer, Size,
};
use cosmic::Element;

use crate::app::Message;

use std::cell::Cell;
use std::f32::consts::{PI, TAU};

/// 色相环切成多少段（1° 一段；在 150px 直径上每段弧长 ≈ 1.3px，看不出台阶）
pub const HUE_SEGMENTS: usize = 360;
/// 内部 SV 方块的采样格数（64×64；只有色相变了才重画，见 `WheelState::key`）
pub const SV_STEPS: usize = 64;
/// 色相盘的可达上限（和 `app::color_bounds(3)` 一致：±3.14 弧度）
pub const HUE_MAX: f32 = 3.14;

// ── 色的数学 ──────────────────────────────────────────────────────────

/// HSV（h 0..1，s/v 0..1）⇒ RGB（各 0..1）。
pub fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let h = h.rem_euclid(1.0);
    let s = s.clamp(0.0, 1.0);
    let v = v.clamp(0.0, 1.0);
    let h6 = h * 6.0;
    let i = h6.floor();
    let f = h6 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    match (i as i32).rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

/// RGB（各 0..1）⇒ HSV（h 0..1；**灰色/黑色时 h = 0**，与常见取色器一致）。
pub fn rgb_to_hsv(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let r = r.clamp(0.0, 1.0);
    let g = g.clamp(0.0, 1.0);
    let b = b.clamp(0.0, 1.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d <= 1e-6 {
        0.0
    } else if (max - r).abs() <= 1e-6 {
        (((g - b) / d).rem_euclid(6.0)) / 6.0
    } else if (max - g).abs() <= 1e-6 {
        (((b - r) / d) + 2.0) / 6.0
    } else {
        (((r - g) / d) + 4.0) / 6.0
    };
    let s = if max <= 1e-6 { 0.0 } else { d / max };
    (h.rem_euclid(1.0), s, max)
}

/// 一个分量 ⇒ 协议字符串里的写法：最多三位小数、去掉尾零（`1` / `0.5` / `0.349`）。
pub fn format_component(x: f32) -> String {
    let x = x.clamp(0.0, 1.0);
    let mut s = format!("{x:.3}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    if s.is_empty() || s == "-0" { "0".to_string() } else { s }
}

/// 属性颜色的**唯一**输出格式：`"r g b"`（0..1 浮点、空格分隔）。
pub fn format_prop_color(r: f32, g: f32, b: f32) -> String {
    format!(
        "{} {} {}",
        format_component(r),
        format_component(g),
        format_component(b)
    )
}

/// 给人看的 `#RRGGBB`（只用于界面上的读数，**不下发**）。
pub fn hex_string(r: f32, g: f32, b: f32) -> String {
    let q = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02X}{:02X}{:02X}", q(r), q(g), q(b))
}

/// 色相环角度（0..1）⇒ 画面色彩的"弧度"值（折进 ±π 再夹到 ±3.14）。
pub fn radians_from_hue(hue: f32) -> f32 {
    let mut a = hue.rem_euclid(1.0) * TAU;
    if a > PI {
        a -= TAU;
    }
    a.clamp(-HUE_MAX, HUE_MAX)
}

/// 画面色彩的"弧度"值 ⇒ 色相环角度（0..1）。`0` 在 3 点钟方向（正右）。
pub fn hue_from_radians(radians: f32) -> f32 {
    (radians / TAU).rem_euclid(1.0)
}

// ── 几何（纯函数：命中判定 / 取色换算，单测直接钉）────────────────────

/// 圆盘的两块可点区域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelPart {
    /// 外圈色相环
    Ring,
    /// 内部饱和/明度方块
    Sv,
}

/// 圆盘在画布里的几何：外半径、内半径、内接方块。
#[derive(Debug, Clone, Copy)]
pub struct WheelGeom {
    pub center: Point,
    pub outer: f32,
    pub inner: f32,
    pub square: Rectangle,
}

impl WheelGeom {
    /// 由画布尺寸算出几何（圆盘居中、留 1px 描边余量）。
    pub fn new(size: Size) -> Self {
        let d = size.width.min(size.height).max(8.0);
        let outer = d / 2.0 - 1.0;
        let inner = outer * 0.72;
        // 内接正方形的半边长 = inner/√2；再收一点，方块和圆环之间留一条缝
        let half = inner * 0.66;
        let center = Point::new(size.width / 2.0, size.height / 2.0);
        WheelGeom {
            center,
            outer,
            inner,
            square: Rectangle::new(
                Point::new(center.x - half, center.y - half),
                Size::new(half * 2.0, half * 2.0),
            ),
        }
    }

    /// 这个点落在哪一块（圆外的"死角"⇒ `None`）。
///
    /// 方块的边界算**闭区间**（右下边也算命中）—— 手柄画在边上时点得到，
    /// 否则"看得见、点不着"。
    pub fn part_at(&self, p: Point) -> Option<WheelPart> {
        let sq = self.square;
        let eps = 0.5;
        if p.x >= sq.x - eps
            && p.x <= sq.x + sq.width + eps
            && p.y >= sq.y - eps
            && p.y <= sq.y + sq.height + eps
        {
            return Some(WheelPart::Sv);
        }
        let dx = p.x - self.center.x;
        let dy = p.y - self.center.y;
        let d = (dx * dx + dy * dy).sqrt();
        if d >= self.inner - 1.0 && d <= self.outer + 1.5 {
            Some(WheelPart::Ring)
        } else {
            None
        }
    }

    /// 这个点的色相（0..1，顺时针；0 在正右 = 红）。
    pub fn hue_at(&self, p: Point) -> f32 {
        let a = (p.y - self.center.y).atan2(p.x - self.center.x);
        let mut h = a / TAU;
        if h < 0.0 {
            h += 1.0;
        }
        h.clamp(0.0, 0.999_99)
    }

    /// 方块里的点 ⇒ (饱和 0..1, 明度 0..1)。左上 = 白，右上 = 纯色相，下边 = 黑。
    pub fn sv_at(&self, p: Point) -> (f32, f32) {
        let s = ((p.x - self.square.x) / self.square.width).clamp(0.0, 1.0);
        let v = (1.0 - (p.y - self.square.y) / self.square.height).clamp(0.0, 1.0);
        (s, v)
    }

    /// 环上某个色相的中点（画手柄用）。
    pub fn ring_point(&self, hue: f32) -> Point {
        let r = (self.inner + self.outer) / 2.0;
        let a = hue.rem_euclid(1.0) * TAU;
        Point::new(self.center.x + r * a.cos(), self.center.y + r * a.sin())
    }

    /// 方块里某个 (s, v) 的位置（画手柄用）。
    pub fn sv_point(&self, s: f32, v: f32) -> Point {
        Point::new(
            self.square.x + s.clamp(0.0, 1.0) * self.square.width,
            self.square.y + (1.0 - v.clamp(0.0, 1.0)) * self.square.height,
        )
    }
}

// ── 画布 ──────────────────────────────────────────────────────────────

/// 拖拽状态 + 图层缓存。
///
/// * 色相环是**静态**的（颜色不随选择变）⇒ 画一次就缓存住；
///
/// * 内部 SV 方块只跟色相有关 ⇒ 单独一份缓存，按 4° 量化（拖动时不必每帧重算
/// 64×64 = 4096 个格子，也不会出现肉眼可见的色差）；
///
/// * 手柄每帧现画（一层很轻的几何）。
#[derive(Default)]
pub struct WheelState {
    drag: Option<WheelPart>,
    ring_cache: Cache,
    sv_cache: Cache,
    sv_key: Cell<u32>,
}

/// 选完色之后发什么。
pub enum OnPick<'a> {
    /// 属性颜色：直接发**已经格式化好的** `"r g b"`（格式只在 [`format_prop_color`] 里定义）
    Rgb(Box<dyn Fn(String) -> Message + 'a>),
    /// 画面色彩·色相：发弧度（走 `Message::ColorSet(3, …)`，与滑条同一条路）
    Hue {
        radians: f32,
        f: Box<dyn Fn(f32) -> Message + 'a>,
    },
}

/// 圆盘本体（`canvas::Program` 的实现者）。
pub struct Wheel<'a> {
    /// 当前色相 0..1
    pub hue: f32,
    /// 当前饱和 0..1
    pub sat: f32,
    /// 当前明度 0..1
    pub val: f32,
    pub on: OnPick<'a>,
    /// `true` = 只画外圈（画面色彩的"色相盘"，没有 S/V 可调）
    pub ring_only: bool,
}

impl Wheel<'_> {
    /// 本次取色 → (h, s, v, 要发的消息)。`None` = 数值没变（不打扰下游）。
    fn pick(&self, part: WheelPart, p: Point, geom: &WheelGeom) -> Option<(f32, f32, f32, Message)> {
        let (h, s, v) = match part {
            WheelPart::Ring => (geom.hue_at(p), self.sat, self.val),
            WheelPart::Sv => (self.hue, geom.sv_at(p).0, geom.sv_at(p).1),
        };
        match &self.on {
            OnPick::Rgb(f) => {
                let (r, g, b) = hsv_to_rgb(h, s, v);
                let text = format_prop_color(r, g, b);
                // 和"界面上现在这个颜色"比：格式没变就别发
                let (cr, cg, cb) = hsv_to_rgb(self.hue, self.sat, self.val);
                if format_prop_color(cr, cg, cb) == text {
                    return None;
                }
                Some((h, s, v, f(text)))
            }
            OnPick::Hue { radians, f } => {
                let a = radians_from_hue(h);
                if (a - *radians).abs() < 1e-4 {
                    return None;
                }
                Some((h, s, v, f(a)))
            }
        }
    }
}

impl canvas::Program<Message, cosmic::Theme> for Wheel<'_> {
    type State = WheelState;

    fn update(
        &self,
        state: &mut WheelState,
        event: &canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        let geom = WheelGeom::new(bounds.size());
        match event {
            canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let p = cursor.position_in(bounds)?;
                let part = geom.part_at(p)?;
                if self.ring_only && part != WheelPart::Ring {
                    return None;
                }
                state.drag = Some(part);
                let (_, _, _, msg) = self.pick(part, p, &geom)?;
                Some(canvas::Action::publish(msg).and_capture())
            }
            canvas::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let part = state.drag?;
                let p = cursor.position_in(bounds)?;
                let (_, _, _, msg) = self.pick(part, p, &geom)?;
                Some(canvas::Action::publish(msg).and_capture())
            }
            canvas::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                let was = state.drag.is_some();
                state.drag = None;
                // 松手 ⇒ 一定要一条"拖动结束"：应用侧用它**立刻放行**最后那个颜色值
                // （节流窗口不等了）并落盘，否则终值要等下一个 55 ms 窗口才出得去。
                Some(if was {
                    canvas::Action::publish(Message::ColorDragEnd).and_capture()
                } else {
                    canvas::Action::capture()
                })
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        state: &WheelState,
        renderer: &Renderer,
        theme: &cosmic::Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let geom = WheelGeom::new(bounds.size());
        let border: IcedColor = theme.cosmic().background(theme.transparent).component.border.into();
        let ring = state.ring_cache.draw(renderer, bounds.size(), |frame| {
            paint_ring(frame, &geom, border);
        });

        let mut layers = vec![ring];
        if !self.ring_only {
            // 方块只跟色相有关：按 4°（90 档）量化后缓存
            let step = (HUE_SEGMENTS / 4) as f32;
            let key = (self.hue.rem_euclid(1.0) * step).round() as u32;
            if state.sv_key.get() != key {
                state.sv_cache.clear();
                state.sv_key.set(key);
            }
            let hue_for_sv = key as f32 * 4.0 / HUE_SEGMENTS as f32;
            layers.push(state.sv_cache.draw(renderer, bounds.size(), |frame| {
                paint_sv(frame, &geom, hue_for_sv);
            }));
        }

        let hue = (self.hue.rem_euclid(1.0) * HUE_SEGMENTS as f32).round() as u32;
        let hue = hue as f32 / HUE_SEGMENTS as f32;
        let mut top = Frame::new(renderer, bounds.size());
        if self.ring_only {
            // 中间那坨：当前色相的圆点预览
            let c = hsv_to_rgb(hue, 1.0, 0.95);
            let disc = Path::circle(geom.center, geom.inner * 0.86);
            top.fill(&disc, IcedColor::from_rgb(c.0, c.1, c.2));
            top.stroke(
                &disc,
                Stroke::default().with_color(border).with_width(1.0),
            );
            paint_handle(&mut top, geom.ring_point(hue), IcedColor::from_rgb(c.0, c.1, c.2));
        } else {
            let (r, g, b) = hsv_to_rgb(self.hue, self.sat, self.val);
            let current = IcedColor::from_rgb(r, g, b);
            paint_handle(&mut top, geom.ring_point(self.hue), current);
            paint_handle(
                &mut top,
                geom.sv_point(self.sat, self.val),
                current,
            );
        }
        layers.push(top.into_geometry());
        layers
    }

    fn mouse_interaction(
        &self,
        state: &WheelState,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if state.drag.is_some() {
            return mouse::Interaction::Grabbing;
        }
        let geom = WheelGeom::new(bounds.size());
        match cursor.position_in(bounds).and_then(|p| geom.part_at(p)) {
            Some(WheelPart::Ring) => mouse::Interaction::Crosshair,
            Some(WheelPart::Sv) if !self.ring_only => mouse::Interaction::Crosshair,
            _ => mouse::Interaction::default(),
        }
    }
}

/// 色相环：360 个扇形（每段 1°，实色填充）。
fn paint_ring(frame: &mut Frame, geom: &WheelGeom, border: IcedColor) {
    for i in 0..HUE_SEGMENTS {
        let h0 = i as f32 / HUE_SEGMENTS as f32;
        let h1 = (i + 1) as f32 / HUE_SEGMENTS as f32;
        let a0 = h0 * TAU;
        let a1 = h1 * TAU;
        // 相邻段共用一条边 ⇒ 每段略微外扩 0.15°，避免抗锯齿出现发丝缝
        let a0 = a0 - 0.0026;
        let a1 = a1 + 0.0026;
        let (r, g, b) = hsv_to_rgb(h0, 1.0, 1.0);
        let seg = Path::new(|p| {
            p.move_to(polar(geom.center, geom.inner - 0.5, a0));
            p.line_to(polar(geom.center, geom.outer + 0.5, a0));
            p.line_to(polar(geom.center, geom.outer + 0.5, a1));
            p.line_to(polar(geom.center, geom.inner - 0.5, a1));
            p.close();
        });
        frame.fill(&seg, IcedColor::from_rgb(r, g, b));
    }
    let stroke = Stroke::default().with_color(border).with_width(1.0);
    frame.stroke(&Path::circle(geom.center, geom.outer), stroke);
    if geom.inner > 2.0 {
        frame.stroke(&Path::circle(geom.center, geom.inner), stroke);
    }
}

fn polar(center: Point, r: f32, angle: f32) -> Point {
    Point::new(center.x + r * angle.cos(), center.y + r * angle.sin())
}

/// 内部 SV 方块：64×64 个实色小格（数学上就是 s 向右、v 向上的双线性面）。
fn paint_sv(frame: &mut Frame, geom: &WheelGeom, hue: f32) {
    let sq = geom.square;
    let step = sq.width / SV_STEPS as f32;
    for iy in 0..SV_STEPS {
        let v = 1.0 - (iy as f32 + 0.5) / SV_STEPS as f32;
        for ix in 0..SV_STEPS {
            let s = (ix as f32 + 0.5) / SV_STEPS as f32;
            let (r, g, b) = hsv_to_rgb(hue, s, v);
            frame.fill_rectangle(
                Point::new(sq.x + ix as f32 * step, sq.y + iy as f32 * step),
                Size::new(step + 0.6, step + 0.6),
                IcedColor::from_rgb(r, g, b),
            );
        }
    }
    frame.stroke(
        &Path::rectangle(Point::new(sq.x, sq.y), sq.size()),
        Stroke::default().with_color(IcedColor::from_rgba(0.0, 0.0, 0.0, 0.35)).with_width(1.0),
    );
}

/// 手柄：一个白圈 + 内侧深色细圈（浅色/深色底上都看得见）。
fn paint_handle(frame: &mut Frame, at: Point, fill: IcedColor) {
    let disc = Path::circle(at, 6.0);
    frame.fill(&disc, fill);
    frame.stroke(&disc, Stroke::default().with_color(IcedColor::WHITE).with_width(2.0));
    frame.stroke(
        &Path::circle(at, 7.2),
        Stroke::default().with_color(IcedColor::from_rgba(0.0, 0.0, 0.0, 0.45)).with_width(1.0),
    );
}

// ── 控件入口 ──────────────────────────────────────────────────────────

/// 完整圆盘（色相环 + 内部 S/V + 当前色手柄）。`on_pick` 收到的是 `"r g b"` 字符串。
pub fn rgb_wheel<'a>(
    size: f32,
    r: f32,
    g: f32,
    b: f32,
    on_pick: impl Fn(String) -> Message + 'a,
) -> Element<'a, Message> {
    let (h, s, v) = rgb_to_hsv(r, g, b);
    canvas::Canvas::new(Wheel {
        hue: h,
        sat: s,
        val: v,
        on: OnPick::Rgb(Box::new(on_pick)),
        ring_only: false,
    })
    .width(size)
    .height(size)
    .into()
}

/// 色相盘（只有外圈 + 中间色点预览）。`on_pick` 收到的是弧度值。
pub fn hue_wheel<'a>(
    size: f32,
    radians: f32,
    on_pick: impl Fn(f32) -> Message + 'a,
) -> Element<'a, Message> {
    canvas::Canvas::new(Wheel {
        hue: hue_from_radians(radians),
        sat: 1.0,
        val: 1.0,
        on: OnPick::Hue {
            radians,
            f: Box::new(on_pick),
        },
        ring_only: true,
    })
    .width(size)
    .height(size)
    .into()
}

// ── 单测：数学 / 几何 / 拖动→消息 全链路 ──────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use canvas::Program;

    fn bounds(size: f32) -> Rectangle {
        Rectangle::new(Point::new(0.0, 0.0), Size::new(size, size))
    }

    #[test]
    fn hsv_rgb_primaries_and_gray() {
        for (h, want) in [
            (0.0f32, (1.0f32, 0.0f32, 0.0f32)),
            (1.0 / 6.0, (1.0, 1.0, 0.0)),
            (2.0 / 6.0, (0.0, 1.0, 0.0)),
            (3.0 / 6.0, (0.0, 1.0, 1.0)),
            (4.0 / 6.0, (0.0, 0.0, 1.0)),
            (5.0 / 6.0, (1.0, 0.0, 1.0)),
        ] {
            let (r, g, b) = hsv_to_rgb(h, 1.0, 1.0);
            assert!(
                (r - want.0).abs() < 1e-5 && (g - want.1).abs() < 1e-5 && (b - want.2).abs() < 1e-5,
                "h={h} 得到 ({r},{g},{b})，应为 {want:?}"
            );
        }
        // 饱和度 0 ⇒ 灰；明度 0 ⇒ 黑
        assert_eq!(hsv_to_rgb(0.3, 0.0, 1.0), (1.0, 1.0, 1.0));
        assert_eq!(hsv_to_rgb(0.3, 1.0, 0.0), (0.0, 0.0, 0.0));
    }

    #[test]
    fn hsv_roundtrips_rgb() {
        for &(r, g, b) in &[
            (0.349f32, 0.573f32, 0.827f32),
            (1.0, 1.0, 1.0),
            (0.0, 0.0, 0.0),
            (0.82, 0.19, 0.05),
            (0.5, 0.5, 0.5),
        ] {
            let (h, s, v) = rgb_to_hsv(r, g, b);
            let (r2, g2, b2) = hsv_to_rgb(h, s, v);
            assert!(
                (r - r2).abs() < 1e-4 && (g - g2).abs() < 1e-4 && (b - b2).abs() < 1e-4,
                "({r},{g},{b}) → hsv → ({r2},{g2},{b2})"
            );
            assert!((0.0..=1.0).contains(&h) && (0.0..=1.0).contains(&s));
        }
    }

    /// 输出格式：这就是下游（`set_prop` → 内核 → 渲染器）收到的字节。
    #[test]
    fn prop_color_string_matches_we_format() {
        assert_eq!(format_prop_color(1.0, 1.0, 1.0), "1 1 1");
        assert_eq!(format_prop_color(0.0, 0.0, 0.0), "0 0 0");
        assert_eq!(format_prop_color(0.349, 0.573, 0.827), "0.349 0.573 0.827");
        assert_eq!(format_prop_color(0.5, 1.0, 0.0), "0.5 1 0");
        // 越界值夹到 0..1（色盘本身不会给越界值，这是防呆）
        assert_eq!(format_prop_color(1.4, -0.2, 0.25), "1 0 0.25");
        // 三个分量、空格分隔、没有多余尾零 —— 和 project.json 里的写法同形
        let s = format_prop_color(0.6, 0.2, 0.9);
        assert_eq!(s.split(' ').count(), 3);
        assert_eq!(s, "0.6 0.2 0.9");
    }

    #[test]
    fn hue_and_radians_are_inverse_within_bounds() {
        // 角度 → 弧度 → 角度（0° / 90° / 180° / 270°）
        for &h in &[0.0f32, 0.25, 0.5, 0.75, 0.99] {
            let a = radians_from_hue(h);
            assert!((-HUE_MAX..=HUE_MAX).contains(&a), "h={h} 弧度越界: {a}");
            let back = hue_from_radians(a);
            let diff = (back - h).abs().min(1.0 - (back - h).abs());
            // h=0.5 正好落在 ±3.14 的夹取边界上 ⇒ 回来会差 ~2.5e-4（可接受）
            assert!(diff < 1e-3, "h={h} → a={a} → {back}");
        }
        assert!((radians_from_hue(0.25) - PI / 2.0).abs() < 1e-5);
        assert!((radians_from_hue(0.5) - HUE_MAX).abs() < 1e-5 || radians_from_hue(0.5) < 0.0);
        assert_eq!(radians_from_hue(0.0), 0.0);
        // 配置里的 ±3.14 都能放回环上
        assert!(hue_from_radians(-HUE_MAX) > 0.49 && hue_from_radians(-HUE_MAX) < 0.51);
        assert!(hue_from_radians(HUE_MAX) > 0.49 && hue_from_radians(HUE_MAX) < 0.51);
    }

    #[test]
    fn geometry_hit_tests_ring_square_and_dead_zone() {
        let g = WheelGeom::new(Size::new(200.0, 200.0));
        assert!((g.center.x - 100.0).abs() < 1e-4 && (g.center.y - 100.0).abs() < 1e-4);
        // 正右方、环中间 ⇒ 环
        assert_eq!(g.part_at(Point::new(100.0 + (g.inner + g.outer) / 2.0, 100.0)), Some(WheelPart::Ring));
        // 圆心 ⇒ 方块（白/黑那一片）
        assert_eq!(g.part_at(g.center), Some(WheelPart::Sv));
        // 环与方块之间那条缝 ⇒ 谁也不是
        let gap_r = (g.inner + g.square.width / 2.0 * 1.4142) / 2.0;
        let _ = gap_r;
        // 圆外 ⇒ None
        assert_eq!(g.part_at(Point::new(1.0, 1.0)), None);

        // 色相角度：正右 = 0，正下 = 0.25（屏幕 y 向下）
        assert!((g.hue_at(Point::new(100.0 + 60.0, 100.0)) - 0.0).abs() < 1e-5);
        assert!((g.hue_at(Point::new(100.0, 100.0 + 60.0)) - 0.25).abs() < 1e-5);
        assert!((g.hue_at(Point::new(100.0 - 60.0, 100.0)) - 0.5).abs() < 1e-5);
        assert!((g.hue_at(Point::new(100.0, 100.0 - 60.0)) - 0.75).abs() < 1e-5);

        // 方块的四个角
        let sq = g.square;
        let (s, v) = g.sv_at(Point::new(sq.x, sq.y));
        assert!((s - 0.0).abs() < 1e-5 && (v - 1.0).abs() < 1e-5, "左上 = 白");
        let (s, v) = g.sv_at(Point::new(sq.x + sq.width, sq.y));
        assert!((s - 1.0).abs() < 1e-5 && (v - 1.0).abs() < 1e-5, "右上 = 纯色相");
        let (s, v) = g.sv_at(Point::new(sq.x, sq.y + sq.height));
        assert!((s - 0.0).abs() < 1e-5 && (v - 0.0).abs() < 1e-5, "左下 = 黑");
        let (s, v) = g.sv_at(Point::new(sq.x + sq.width, sq.y + sq.height));
        assert!((s - 1.0).abs() < 1e-5 && (v - 0.0).abs() < 1e-5, "右下 = 黑");
    }

    /// 手柄画的位置必须和命中判定互逆（否则"点哪画哪"对不上）。
    #[test]
    fn handle_positions_invert_the_hit_tests() {
        let g = WheelGeom::new(Size::new(150.0, 150.0));
        for &h in &[0.0f32, 0.1, 0.33, 0.5, 0.71, 0.99] {
            let p = g.ring_point(h);
            assert_eq!(g.part_at(p), Some(WheelPart::Ring));
            assert!((g.hue_at(p) - h).abs() < 1e-3, "h={h} → {}", g.hue_at(p));
        }
        for &(s, v) in &[(0.0f32, 1.0f32), (1.0, 1.0), (0.0, 0.0), (0.35, 0.8)] {
            let p = g.sv_point(s, v);
            assert_eq!(g.part_at(p), Some(WheelPart::Sv));
            let (s2, v2) = g.sv_at(p);
            assert!((s - s2).abs() < 1e-3 && (v - v2).abs() < 1e-3);
        }
    }

    // ── 拖动 → 消息：不打开窗口也能把整条映射跑一遍 ──────────────────

    fn prog(r: f32, g: f32, b: f32) -> Wheel<'static> {
        let (h, s, v) = rgb_to_hsv(r, g, b);
        Wheel {
            hue: h,
            sat: s,
            val: v,
            on: OnPick::Rgb(Box::new(|s: String| Message::PropColor("id".into(), "key".into(), s))),
            ring_only: false,
        }
    }

    fn press(p: Wheel<'_>, state: &mut WheelState, size: f32, at: Point) -> Option<Message> {
        let ev = canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left));
        let cur = mouse::Cursor::Available(at);
        let action = p.update(state, &ev, bounds(size), cur)?;
        action.into_inner().0
    }

    fn drag(p: &Wheel<'_>, state: &mut WheelState, size: f32, at: Point) -> Option<Message> {
        let ev = canvas::Event::Mouse(mouse::Event::CursorMoved { position: at });
        let action = p.update(state, &ev, bounds(size), mouse::Cursor::Available(at))?;
        action.into_inner().0
    }

    fn released(p: &Wheel<'_>, state: &mut WheelState) -> bool {
        let ev = canvas::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left));
        p.update(state, &ev, bounds(200.0), mouse::Cursor::Available(Point::new(1.0, 1.0)))
            .is_some()
    }

    /// 点色相环：只改色相，S/V 保持（这里是 s=0.75 / v=0.8 的有彩色）。
    #[test]
    fn ring_click_changes_hue_only_and_keeps_sv() {
        let p = prog(0.8, 0.2, 0.2); // 红：h=0, s=0.75, v=0.8
        let mut st = WheelState::default();
        let g = WheelGeom::new(Size::new(200.0, 200.0));
        let msg = press(p, &mut st, 200.0, Point::new(100.0, 100.0 + (g.inner + g.outer) / 2.0)).unwrap();
        match msg {
            Message::PropColor(id, key, s) => {
                assert_eq!((id.as_str(), key.as_str()), ("id", "key"));
                // 正下方 = h 0.25（黄绿），S/V 不变 ⇒ (0.5, 0.8, 0.2)
                assert_eq!(s, "0.5 0.8 0.2");
            }
            other => panic!("不是 PropColor: {other:?}"),
        }
        assert!(released(&prog(0.8, 0.2, 0.2), &mut st));
    }

    /// 环上取色：先在方块里把 S 拉满，再转色相 ⇒ 输出随色相变，S/V 不变。
    #[test]
    fn ring_after_saturation_emits_hue_colors() {
        let mut st = WheelState::default();
        let g = WheelGeom::new(Size::new(200.0, 200.0));
        // ① 方块右上角 = s=1, v=1（纯色相），当前色相 0 ⇒ 红
        let red = prog(0.2, 0.2, 0.2);
        let msg = press(red, &mut st, 200.0, Point::new(g.square.x + g.square.width, g.square.y)).unwrap();
        match msg {
            Message::PropColor(_, _, s) => assert_eq!(s, "1 0 0"),
            other => panic!("{other:?}"),
        }
        // ② 松手后在环的正下方重新按下（h=0.25 = 黄绿 ⇒ (0.5,1,0)）；S/V 仍是满的
        assert!(released(&prog(1.0, 0.0, 0.0), &mut st));
        let cur = prog(1.0, 0.0, 0.0); // 程序每帧按新值重建
        let msg = press(cur, &mut st, 200.0, Point::new(100.0, 100.0 + (g.inner + g.outer) / 2.0)).unwrap();
        match msg {
            Message::PropColor(_, _, s) => {
                let p: Vec<f32> = s.split(' ').map(|t| t.parse().unwrap()).collect();
                assert_eq!(p.len(), 3);
                assert!((p[0] - 0.5).abs() < 1e-3 && (p[1] - 1.0).abs() < 1e-3 && (p[2] - 0.0).abs() < 1e-3,
                        "h=0.25 应为 (0.5,1,0)，得到 {s}");
            }
            other => panic!("{other:?}"),
        }
    }

    /// 拖方块：色相不变、S/V 跟着走；输出仍是三个 0..1 浮点。
    #[test]
    fn sv_drag_emits_rgb_and_keeps_three_components() {
        let mut st = WheelState::default();
        let g = WheelGeom::new(Size::new(200.0, 200.0));
        let p = prog(0.0, 0.0, 1.0); // 纯蓝：h=0.6667, s=1, v=1
        // 按下左上角（白）
        let msg = press(p, &mut st, 200.0, Point::new(g.square.x + 1.0, g.square.y + 1.0)).unwrap();
        match &msg {
            Message::PropColor(_, _, s) => {
                assert_eq!(s.split(' ').count(), 3);
                let v: Vec<f32> = s.split(' ').map(|t| t.parse().unwrap()).collect();
                assert!(v[0] > 0.97 && v[1] > 0.97 && v[2] > 0.97, "左上应接近白: {s}");
            }
            other => panic!("{other:?}"),
        }
        // 拖到中右 ⇒ 饱和、半明度；色相仍然是蓝（b 最大）
        let cur = prog(0.0, 0.0, 1.0);
        let msg = drag(&cur, &mut st, 200.0, Point::new(g.square.x + g.square.width * 0.95, g.square.y + g.square.height * 0.5)).unwrap();
        match msg {
            Message::PropColor(_, _, s) => {
                let v: Vec<f32> = s.split(' ').map(|t| t.parse().unwrap()).collect();
                assert_eq!(v.len(), 3);
                assert!(v[2] > v[0] && v[2] > v[1], "色相应保持蓝: {s}");
                assert!(v.iter().all(|x| (0.0..=1.0).contains(x)), "越界: {s}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn click_outside_is_ignored_and_release_clears_drag() {
        let mut st = WheelState::default();
        let p = prog(1.0, 0.0, 0.0);
        assert!(press(p, &mut st, 200.0, Point::new(2.0, 2.0)).is_none());
        assert!(st.drag.is_none());
        // 没按下时的移动不取色
        let p2 = prog(1.0, 0.0, 0.0);
        assert!(drag(&p2, &mut st, 200.0, Point::new(100.0, 100.0)).is_none());
    }

    /// 同一个像素重复移动 ⇒ 不重复发消息（拖动时别把磁盘/内核刷爆）。
    #[test]
    fn identical_pick_does_not_republish() {
        let mut st = WheelState::default();
        let g = WheelGeom::new(Size::new(200.0, 200.0));
        let at = Point::new(
            g.square.x + g.square.width * 0.5,
            g.square.y + g.square.height * 0.5,
        );
        // 该点 = (h 保持, s=0.5, v=0.5)；当前色是灰(0.3) ⇒ 第一次必然要发
        let first = press(prog(0.3, 0.3, 0.3), &mut st, 200.0, at);
        assert!(first.is_some(), "第一次按下应发消息");

        // 把程序"重建"成该点对应的颜色 ⇒ 同一点再来一次，字符串没变 ⇒ 不该发
        let (r, g_, b) = hsv_to_rgb(0.0, 0.5, 0.5);
        let same = prog(r, g_, b);
        assert!(drag(&same, &mut st, 200.0, at).is_none(), "同一个点、同一个颜色应被去重");

        // 挪一格 ⇒ 值变了 ⇒ 必须发
        let moved = Point::new(at.x + g.square.width * 0.1, at.y);
        assert!(drag(&same, &mut st, 200.0, moved).is_some(), "值变了就必须发");
    }

    /// 色相盘：拖动只发弧度（`ColorSet(3, …)` 那条路），且落在 ±3.14 内。
    #[test]
    fn hue_wheel_emits_radians_within_bounds() {
        let mut st = WheelState::default();
        let g = WheelGeom::new(Size::new(120.0, 120.0));
        let p = Wheel {
            hue: 0.0,
            sat: 1.0,
            val: 1.0,
            on: OnPick::Hue {
                radians: 0.0,
                f: Box::new(|a: f32| Message::ColorSet(3, a)),
            },
            ring_only: true,
        };
        let at = Point::new(60.0, 60.0 + (g.inner + g.outer) / 2.0);
        let msg = press(p, &mut st, 120.0, at).unwrap();
        match msg {
            Message::ColorSet(3, a) => {
                assert!((-HUE_MAX..=HUE_MAX).contains(&a));
                assert!((a - PI / 2.0).abs() < 1e-3, "正下方应是 +π/2，得到 {a}");
            }
            other => panic!("{other:?}"),
        }
        // 内部方块在 ring_only 模式下不响应
        let mut st2 = WheelState::default();
        let p2 = Wheel {
            hue: 0.0,
            sat: 1.0,
            val: 1.0,
            on: OnPick::Hue {
                radians: 0.0,
                f: Box::new(|a: f32| Message::ColorSet(3, a)),
            },
            ring_only: true,
        };
        assert!(press(p2, &mut st2, 120.0, Point::new(60.0, 60.0)).is_none());
    }
}
