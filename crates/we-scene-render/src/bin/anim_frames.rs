// SPDX-License-Identifier: MIT
//
//! `anim_frames`：把同一条壁纸在**多个时间点**的渲染拼成一张横向拼图（"看 puppet 在动"）。
//!
//! ```sh
//! export LD_LIBRARY_PATH=~/.local/opt/gl/root/usr/lib/x86_64-linux-gnu
//! we-scene-anim-frames <壁纸目录> out.png --size 1600x900 --times 0,0.5,1.0 \
//! [--crop x0,y0,x1,y1] [--zoom 2]
//! ```
//!
//! * `--times` 用 `SceneRenderer::set_time` 把场景时间**精确**设到该秒数（动画相位确定）。
//!
//! * `--crop` + `--zoom` 用来放大"腿部那块"这种局部（`zoom` 是整数最近邻放大）。
//!
//! * 每格左上角画 `t=<秒>s` 的标签（内置 5×7 点阵，不依赖系统字体）。
//!
//! * `--assets` 只为命令习惯而接受（渲染器内部已按默认路径找 WE assets）。

use std::path::Path;

use we_scene_render::SceneRenderer;

/// 5×7 点阵（每行 5 bit，最高位在最左）。只需要 `0-9 . = s t`。
fn glyph(c: char) -> [u8; 7] {
    match c {
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ],
        '.' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b01100, 0b01100,
        ],
        '=' => [
            0b00000, 0b00000, 0b11111, 0b00000, 0b11111, 0b00000, 0b00000,
        ],
        't' => [
            0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0b00011, 0b00000,
        ],
        's' => [
            0b00000, 0b01111, 0b10000, 0b01110, 0b00001, 0b11110, 0b00000,
        ],
        _ => [0; 7],
    }
}

/// 在 `img` 的 `(x, y)` 处画一行文字（`scale` 倍放大，白字 + 1px 黑描边）。
fn draw_text(img: &mut image::RgbaImage, mut x: u32, y: u32, s: &str, scale: u32) {
    for ch in s.chars() {
        let g = glyph(ch);
        for (row, bits) in g.iter().enumerate() {
            for col in 0..5u32 {
                if (bits >> (4 - col)) & 1 == 0 {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        let (px, py) = (x + col * scale + dx, y + row as u32 * scale + dy);
                        // 1px 黑描边（相邻格先涂黑再涂白，保证在亮背景上也看得见）
                        for (ox, oy) in [(px as i64 - 1, py as i64), (px as i64 + 1, py as i64)] {
                            if ox >= 0 && oy >= 0 && (ox as u32) < img.width() && (oy as u32) < img.height() {
                                img.put_pixel(ox as u32, oy as u32, image::Rgba([0, 0, 0, 255]));
                            }
                        }
                        if px < img.width() && py < img.height() {
                            img.put_pixel(px, py, image::Rgba([255, 255, 255, 255]));
                        }
                    }
                }
            }
        }
        x += 6 * scale;
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(1).cloned().ok_or("用法: we-scene-anim-frames <壁纸目录> out.png --size WxH --times 0,0.5,1.0")?;
    let out = args.get(2).cloned().unwrap_or_else(|| "anim.png".into());
    let arg1 = |key: &str| -> Option<String> {
        args.iter()
            .position(|a| a == key)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let size: Option<(u32, u32)> = arg1("--size")
        .and_then(|s| s.split_once(['x', 'X']).map(|(a, b)| (a.to_string(), b.to_string())))
        .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)));
    let times: Vec<f32> = arg1("--times")
        .unwrap_or_else(|| "0,0.5,1.0".into())
        .split(',')
        .filter_map(|s| s.trim().parse::<f32>().ok())
        .collect();
    let crop: Option<(u32, u32, u32, u32)> = arg1("--crop").and_then(|s| {
        let n: Vec<u32> = s.split(',').filter_map(|t| t.trim().parse().ok()).collect();
        (n.len() == 4).then(|| (n[0], n[1], n[2], n[3]))
    });
    let zoom: u32 = arg1("--zoom").and_then(|s| s.parse().ok()).unwrap_or(1).max(1);
    let label_scale: u32 = arg1("--label-scale").and_then(|s| s.parse().ok()).unwrap_or(2).max(1);
    // `--mesh`：场景时钟**停在 0**、只让**网格动画**按 `--times` 走 ⇒ 拼图里除了人物本体
    // 没有任何东西在动（把"腿在动"从水波/摆动/粒子/特效里隔离出来）。
    let mesh_only = args.iter().any(|a| a == "--mesh");

    let scene_size = SceneRenderer::open(Path::new(&dir), 1024, 1)?.scene_size;
    let (w, h) = match size {
        Some((w, h)) => (w, h),
        None => (1600, ((1600.0 * scene_size.1 / scene_size.0).round() as u32).max(1)),
    };
    let mut r = SceneRenderer::open(Path::new(&dir), w, h)?;

    let mut tiles: Vec<(f32, Vec<u8>)> = Vec::new();
    for &t in &times {
        // 定格在 t（固定步长 0）⇒ 每一格都可复现，且 `g_Time` 与网格动画看到同一时刻
        if mesh_only {
            r.set_time_frozen(0.0);
            r.set_mesh_time(Some(t));
        } else {
            r.set_mesh_time(None);
            r.set_time_frozen(t);
        }
        let mut buf = vec![0u8; (w * h * 4) as usize];
        r.render_to(&mut buf, false)?;
        tiles.push((t, buf));
        println!("渲了一帧 t={t:.3}s{}", if mesh_only { "（只动网格）" } else { "" });
    }

    // 裁剪（可选）→ 缩放 → 拼接
    let (cx0, cy0, cx1, cy1) = crop.unwrap_or((0, 0, w, h));
    let (cx0, cy0) = (cx0.min(w.saturating_sub(1)), cy0.min(h.saturating_sub(1)));
    let (cx1, cy1) = (cx1.clamp(cx0 + 1, w), cy1.clamp(cy0 + 1, h));
    let (cw, ch) = ((cx1 - cx0) * zoom, (cy1 - cy0) * zoom);
    let pad = 6u32;
    let lab = 9 * label_scale;
    let total_w = pad + tiles.len() as u32 * (cw + pad);
    let total_h = pad + lab + ch + pad;
    let mut img = image::RgbaImage::from_pixel(total_w, total_h, image::Rgba([24, 24, 28, 255]));

    for (i, (t, buf)) in tiles.iter().enumerate() {
        let ox = pad + i as u32 * (cw + pad);
        let oy = pad + lab;
        for y in 0..ch {
            for x in 0..cw {
                let sx = cx0 + x / zoom;
                let sy = cy0 + y / zoom;
                let o = ((sy * w + sx) * 4) as usize;
                img.put_pixel(
                    ox + x,
                    oy + y,
                    image::Rgba([buf[o], buf[o + 1], buf[o + 2], 255]),
                );
            }
        }
        draw_text(&mut img, ox, pad, &format!("t={t:.2}s"), label_scale);
    }
    image::save_buffer(&out, img.as_raw(), total_w, total_h, image::ColorType::Rgba8)?;
    println!(
        "已写出拼图 {out}（{total_w}x{total_h}，{} 格：{:?}，裁剪=({cx0},{cy0},{cx1},{cy1}) zoom={zoom}）",
        tiles.len(),
        times
    );
    Ok(())
}
