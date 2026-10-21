// SPDX-License-Identifier: MIT
//
//! **粒子算子测量探针**（离线、无 GL）：把一份真实的 WE 粒子预设喂进生产模拟器
//! `particles::ParticleSystem`，跑 N 帧，打出"单粒子/整层"的可测数字
//! （存活数、尺寸 p50/p90、速度均值/最大、场景 bbox）。
//!
//! 为什么单独写一个 bin：CLI 的 `WE_DEBUG_PARTICLES` 每帧日志**只打有顶点的系统**，
//! 而且它跟着 draw_frame 走（改 window/分辨率才看得见），不适合做"算子前后"的定量对照；
//! 这个探针直接调模拟器，与渲染解耦 ⇒ 数字稳定、可复现。
//!
//! 用法：
//! ```sh
//! particle-probe <preset.json> [--frames 300] [--dt 0.033333] \
//! [--origin "x y z"] [--scale "sx sy"] [--ov '<instanceoverride json>'] \
//! [--label 名字] [--origin-abs "x y z"]
//! ```
//! 每次打印**三列**（同一份预设、同一个 override）：
//! · `改前(LEGACY 两轮都关)` = `WE_PARTICLE_LEGACY=1` —— 上次的 A/B 口径（两轮一起关）
//! · `改前(只关本次)` = `WE_PARTICLE_OPS_OFF=1` —— **本次真正的改前**
//! · `改后(本次全开)` = 默认
//! ⚠ `size` 是顶点里那个"屏幕全宽（场景单位）"，与 `WE_DEBUG_PARTICLES` 的 p50/p90 同口径（顶点 idx 6）。

use std::path::Path;

use we_scene_render::particles::{InstanceOverride, ParticlePreset, ParticleSystem};

struct Args {
    preset: String,
    frames: u32,
    dt: f32,
    origin: [f32; 3],
    scale: [f32; 2],
    ov: InstanceOverride,
    label: String,
    /// `instanceoverride.rate` 的策略脚本（`{"script": …}`）—— 从文件读。
    /// 给了就额外打三行"脚本开/关"对照（见 `main` 末尾）。
    rate_script: Option<String>,
    /// 脚本 `init(value)` 的入参（WE 传该字段的静态值）。缺省 = `--ov` 的 rate（没有则 1.0）。
    rate_init: Option<f32>,
    /// 静态频谱（"音频开"那一列）；`None` 用默认 16 段全 0.9。
    bands: Option<String>,
}

/// 把 `--bands "b0 b1 …"` 解析成 16 段（1 个值 ⇒ 全等；少于 16 个 ⇒ 后面补最后一个）。
fn parse_bands(s: &str) -> [f32; we_scene_render::audio::BANDS] {
    let vals: Vec<f32> = s.split_whitespace().filter_map(|x| x.parse().ok()).collect();
    let mut b = [0.0f32; we_scene_render::audio::BANDS];
    match vals.len() {
        0 => {}
        1 => b = [vals[0].clamp(0.0, 1.0); we_scene_render::audio::BANDS],
        n if n < we_scene_render::audio::BANDS => {
            let mut last = 0.0;
            for (i, x) in b.iter_mut().enumerate() {
                if i < n {
                    last = vals[i].clamp(0.0, 1.0);
                }
                *x = last;
            }
        }
        _ => {
            for (i, x) in b.iter_mut().enumerate() {
                *x = vals[i].clamp(0.0, 1.0);
            }
        }
    }
    b
}

/// 跑 N 帧并打出"整层"的可测数字（与上面三列同口径）。
/// `script` 给出时每帧先跑脚本 → `set_rate_mul`（= 渲染器 `draw_frame` 里的同一条路）。
fn measure(
    mut ps: ParticleSystem,
    frames: u32,
    dt: f32,
    label: &str,
    rate_note: &str,
    mut script: Option<&mut we_scene_render::propscript::NumScript>,
) {
    let mut peak = 0usize;
    let mut t = 0.0f32;
    for _ in 0..frames {
        if let Some(sc) = script.as_mut() {
            t += dt;
            let v = sc.update(dt, t, &we_scene_render::audio::spectrum());
            ps.set_rate_mul(v);
        }
        ps.update(dt);
        peak = peak.max(ps.particles.len());
    }
    let mut verts = Vec::new();
    ps.build(&mut verts);
    let mut sizes: Vec<f32> = verts.chunks(6).map(|c| c[0][6]).collect();
    sizes.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
    let pct = |p: f32| -> f32 {
        if sizes.is_empty() {
            0.0
        } else {
            sizes[((sizes.len() as f32 - 1.0) * p).round() as usize]
        }
    };
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for v in &verts {
        x0 = x0.min(v[0]);
        y0 = y0.min(v[1]);
        x1 = x1.max(v[0]);
        y1 = y1.max(v[1]);
    }
    let (mut sp_sum, mut sp_max) = (0.0f32, 0.0f32);
    for p in &ps.particles {
        let s = (p.vel[0] * p.vel[0] + p.vel[1] * p.vel[1] + p.vel[2] * p.vel[2]).sqrt();
        sp_sum += s;
        sp_max = sp_max.max(s);
    }
    let n = ps.particles.len().max(1) as f32;
    println!(
        "  {label:14} 存活 {:5}  峰值 {:5}  尺寸 p50 {:8.1} p90 {:8.1}  速度 均值 {:9.1} 最大 {:9.1}  bbox x[{:.0},{:.0}] y[{:.0},{:.0}]  {rate_note}",
        ps.particles.len(),
        peak,
        pct(0.5),
        pct(0.9),
        sp_sum / n,
        sp_max,
        if verts.is_empty() { 0.0 } else { x0 },
        if verts.is_empty() { 0.0 } else { x1 },
        if verts.is_empty() { 0.0 } else { y0 },
        if verts.is_empty() { 0.0 } else { y1 },
    );
}

fn parse_vec3(s: &str, d: [f32; 3]) -> [f32; 3] {
    let p: Vec<f32> = s.split_whitespace().filter_map(|x| x.parse().ok()).collect();
    if p.len() >= 3 {
        [p[0], p[1], p[2]]
    } else {
        d
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut args = Args {
        preset: a.get(1).cloned().unwrap_or_default(),
        frames: 300,
        dt: 1.0 / 30.0,
        origin: [0.0, 0.0, 0.0],
        scale: [1.0, 1.0],
        ov: InstanceOverride::default(),
        label: String::new(),
        rate_script: None,
        rate_init: None,
        bands: None,
    };
    let mut i = 2;
    while i < a.len() {
        let get = |i: usize| a.get(i + 1).cloned().unwrap_or_default();
        match a[i].as_str() {
            "--frames" => {
                args.frames = get(i).parse().unwrap_or(300);
                i += 2;
            }
            "--dt" => {
                args.dt = get(i).parse().unwrap_or(1.0 / 30.0);
                i += 2;
            }
            "--origin" => {
                args.origin = parse_vec3(&get(i), [0.0, 0.0, 0.0]);
                i += 2;
            }
            "--scale" => {
                let p: Vec<f32> = get(i).split_whitespace().filter_map(|x| x.parse().ok()).collect();
                if p.len() >= 2 {
                    args.scale = [p[0], p[1]];
                }
                i += 2;
            }
            "--ov" => {
                args.ov = serde_json::from_str(&get(i)).unwrap_or_default();
                i += 2;
            }
            "--label" => {
                args.label = get(i);
                i += 2;
            }
            "--rate-script" => {
                args.rate_script = Some(get(i));
                i += 2;
            }
            "--rate-init" => {
                args.rate_init = get(i).parse().ok();
                i += 2;
            }
            "--bands" => {
                args.bands = Some(get(i));
                i += 2;
            }
            _ => i += 1,
        }
    }
    let raw = match std::fs::read_to_string(&args.preset) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("读不到预设 {}：{e}", args.preset);
            std::process::exit(2);
        }
    };
    let preset: ParticlePreset = match serde_json::from_str(&raw) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("预设解析失败（{}）：{e}", Path::new(&args.preset).display());
            std::process::exit(3);
        }
    };
    let label = if args.label.is_empty() {
        args.preset.split('/').last().unwrap_or("preset").to_string()
    } else {
        args.label.clone()
    };
    println!(
        "预设 {label}  ({} operator / {} initializer / {} emitter)  帧数 {}  步长 {:.4}s  原点 {:?}  scale {:?}",
        preset.operator.len(),
        preset.initializer.len(),
        preset.emitter.len(),
        args.frames,
        args.dt,
        args.origin,
        args.scale
    );
    for (name, legacy, ops_off) in [
        ("改前(LEGACY 两轮都关)", true, true),
        ("改前(只关本轮)", false, true),
        ("改后(本轮全开)", false, false),
    ] {
        std::env::set_var("WE_PARTICLE_LEGACY", if legacy { "1" } else { "0" });
        std::env::set_var("WE_PARTICLE_OPS_OFF", if ops_off { "1" } else { "0" });
        let ps = ParticleSystem::new(preset.clone(), args.origin, args.scale, &args.ov, 1);
        measure(ps, args.frames, args.dt, name, "", None);
    }
    std::env::remove_var("WE_PARTICLE_LEGACY");
    std::env::remove_var("WE_PARTICLE_OPS_OFF");

    // ── 场景属性脚本（`instanceoverride.rate` 的 `{"script": …}`）对照 ─────────────
    // `--rate-script <file>` 给出时打三行：脚本关（静态 rate，= 改动前） /
    // 脚本开 + 静音（average[i]=0 ⇒ WE 静音行为） / 脚本开 + 频谱（真响应音频）。
    if let Some(path) = args.rate_script.clone() {
        let js = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("读不到脚本 {path}：{e}");
                return;
            }
        };
        // `init(value)`：WE 传该字段解析出来的静态值（= `--ov` 里的 rate；没写就 1.0）
        let init = args.rate_init.or(args.ov.rate).unwrap_or(1.0);
        let bands_on = args
            .bands
            .as_deref()
            .map(parse_bands)
            .unwrap_or([0.9f32; we_scene_render::audio::BANDS]);
        println!(
            "脚本对照（WE 音频响应模板；脚本 {} 字节；`init({init})`；频谱 {}）",
            js.len(),
            args.bands.as_deref().unwrap_or("默认 16 段全 0.9")
        );
        // ① 脚本关：`WE_SCRIPT_AUDIO` 不设 ⇒ `field_script` 不建脚本 ⇒ 静态 rate（= 改动前那条路）
        std::env::remove_var("WE_SCRIPT_AUDIO");
        std::env::remove_var("WE_AUDIO_BANDS");
        let ps = ParticleSystem::new(preset.clone(), args.origin, args.scale, &args.ov, 1);
        measure(ps, args.frames, args.dt, "脚本关", "rate=静态 ov.rate", None);
        // ② 脚本开 + 静音
        std::env::set_var("WE_SCRIPT_AUDIO", "1");
        we_scene_render::audio::set_static_bands([0.0; we_scene_render::audio::BANDS]);
        let mut sc = we_scene_render::propscript::NumScript::new(&js, init, None).expect("脚本应当能跑");
        let ps = ParticleSystem::new(preset.clone(), args.origin, args.scale, &args.ov, 1);
        measure(ps, args.frames, args.dt, "脚本开+静音", "rate=rate×0.1", Some(&mut sc));
        // ③ 脚本开 + 频谱（"音频开"）
        we_scene_render::audio::set_static_bands(bands_on);
        let mut sc = we_scene_render::propscript::NumScript::new(&js, init, None).expect("脚本应当能跑");
        let ps = ParticleSystem::new(preset.clone(), args.origin, args.scale, &args.ov, 1);
        measure(ps, args.frames, args.dt, "脚本开+频谱", "rate 随频谱上升到 ×1.0", Some(&mut sc));
        std::env::remove_var("WE_SCRIPT_AUDIO");
    }
}
