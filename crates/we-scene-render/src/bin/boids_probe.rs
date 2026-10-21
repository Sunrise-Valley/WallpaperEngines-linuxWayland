// SPDX-License-Identifier: MIT
//
//! **`boids` 算子的离线可测探针**（无 GL，直接驱动生产模拟器 `particles::ParticleSystem`）。
//!
//! 为什么单独一个 bin：`particle_probe` 打的是"整层聚合值"（存活/尺寸/速度/bbox），
//! 而判断 `boids` 三条力**各自**有没有在起作用需要**同一批粒子**的
//! ① 配对距离分布（分离项）② 速度同向度/散布（对齐项）③ 相对质心半径（聚集项）
//! 以及"到底有没有邻居"（`neighborthreshold` 内的配对计数）。这些量对"改前/改后"这种
//! 单列对照不够用，得逐帧看时序。
//!
//! 用法：
//! ```sh
//! boids_probe <preset.json> [--frames 300] [--dt 0.033333] [--origin "x y z"] \
//! [--scale "sx sy"] [--ov '<instanceoverride json>'] [--label 名] [--every 30]
//! ```
//! 输出（行式，便于 grep/聚合）：
//! · 每个采样帧一行 `[帧] …`
//! · 末尾 `[汇总] …`（峰值存活 / 出现邻居的帧数 / 全程最近配对距离 / 半径收缩率 /
//! 同向度变化 / 速度散布变化 / 质心位移）
//! ⚠ 阈值取自**预设 JSON 本身**（`operator[name=boids]` 的 `neighborthreshold` /
//! `separationthreshold`）⇒ 量出来的"有邻居"就是**实现里那条判据**用的同一个半径。

use std::path::Path;

use we_scene_render::particles::{InstanceOverride, ParticlePreset, ParticleSystem};

fn parse_vec3(s: &str, d: [f32; 3]) -> [f32; 3] {
    let p: Vec<f32> = s.split_whitespace().filter_map(|x| x.parse().ok()).collect();
    if p.len() >= 3 {
        [p[0], p[1], p[2]]
    } else {
        d
    }
}

fn len3(v: [f32; 3]) -> f32 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

/// 一帧的可测快照（`boids` 三条力各自的判据都在这里）。
struct Snap {
    alive: usize,
    with_nb: usize,
    mean_nb: f32,
    min_pd: f32,
    mean_pd: f32,
    rms_r: f32,
    coherence: f32,
    vel_spread: f32,
    inward: f32,
    centroid: [f32; 3],
    /// **分离窗口**（`separationthreshold`）内的**无序配对数** —— 分离项真正作用的对数。
    sep_pairs: usize,
    /// **平均最近邻距离**（每颗粒子到它最近邻居的距离，再取平均）—— 分离项该把它推大、
    /// 聚集项该把它压小；这是"集群紧不紧"最直接的单一数字。
    mean_nn: f32,
}

fn snapshot(ps: &ParticleSystem, nt: f32, _st: f32) -> Snap {
    let n = ps.particles.len();
    let mut s = Snap {
        alive: n,
        with_nb: 0,
        mean_nb: 0.0,
        min_pd: 0.0,
        mean_pd: 0.0,
        rms_r: 0.0,
        coherence: 0.0,
        vel_spread: 0.0,
        inward: 0.0,
        centroid: [0.0; 3],
        sep_pairs: 0,
        mean_nn: 0.0,
    };
    if n == 0 {
        return s;
    }
    let mut c = [0.0f32; 3];
    let mut vsum = [0.0f32; 3];
    let mut sp_sum = 0.0f32;
    for p in &ps.particles {
        for k in 0..3 {
            c[k] += p.pos[k];
            vsum[k] += p.vel[k];
        }
        sp_sum += len3(p.vel);
    }
    for k in 0..3 {
        c[k] /= n as f32;
        vsum[k] /= n as f32;
    }
    s.centroid = c;
    let mut spread = 0.0f32;
    for p in &ps.particles {
        let d = [p.vel[0] - vsum[0], p.vel[1] - vsum[1], p.vel[2] - vsum[2]];
        spread += len3(d) * len3(d);
    }
    s.vel_spread = (spread / n as f32).sqrt();
    s.coherence = if sp_sum > 1e-9 { len3(vsum) * n as f32 / sp_sum } else { 0.0 };
    let mut r2 = 0.0f32;
    let mut inward = 0.0f32;
    let mut nb_sum = 0usize;
    for (i, p) in ps.particles.iter().enumerate() {
        let to_c = [c[0] - p.pos[0], c[1] - p.pos[1], c[2] - p.pos[2]];
        let d = len3(to_c);
        r2 += d * d;
        if d > 1e-6 {
            inward += (p.vel[0] * to_c[0] + p.vel[1] * to_c[1] + p.vel[2] * to_c[2]) / d;
        }
        let mut nb = 0usize;
        for (j, q) in ps.particles.iter().enumerate() {
            if i == j {
                continue;
            }
            let dd = [q.pos[0] - p.pos[0], q.pos[1] - p.pos[1], q.pos[2] - p.pos[2]];
            let dist = len3(dd);
            if dist <= 1e-4 {
                continue;
            }
            if nt > 0.0 && dist < nt {
                nb += 1;
            }
        }
        if nb > 0 {
            s.with_nb += 1;
        }
        nb_sum += nb;
    }
    s.mean_nb = nb_sum as f32 / n as f32;
    s.rms_r = (r2 / n as f32).sqrt();
    s.inward = inward / n as f32;
    // 配对距离（无序对）+ 每颗粒子到最近邻居的距离
    let (mut dmin, mut dsum, mut pairs) = (f32::MAX, 0.0f32, 0usize);
    let mut nn = vec![f32::MAX; n];
    for i in 0..n {
        for j in (i + 1)..n {
            let a = &ps.particles[i];
            let b = &ps.particles[j];
            let dd = [b.pos[0] - a.pos[0], b.pos[1] - a.pos[1], b.pos[2] - a.pos[2]];
            let dist = len3(dd);
            if dist <= 1e-4 {
                continue;
            }
            if _st > 0.0 && dist < _st {
                s.sep_pairs += 1;
            }
            if dist < nn[i] {
                nn[i] = dist;
            }
            if dist < nn[j] {
                nn[j] = dist;
            }
            dmin = dmin.min(dist);
            dsum += dist;
            pairs += 1;
        }
    }
    let mut nn_sum = 0.0f32;
    let mut nn_cnt = 0usize;
    for v in nn.iter() {
        if *v < f32::MAX {
            nn_sum += *v;
            nn_cnt += 1;
        }
    }
    s.mean_nn = if nn_cnt > 0 { nn_sum / nn_cnt as f32 } else { 0.0 };
    if pairs > 0 {
        s.min_pd = dmin;
        s.mean_pd = dsum / pairs as f32;
    }
    s
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut preset_path = String::new();
    let mut frames: u32 = 300;
    let mut dt = 1.0f32 / 30.0;
    let mut origin = [0.0f32; 3];
    let mut scale = [1.0f32; 2];
    let mut ov = InstanceOverride::default();
    let mut label = String::new();
    let mut every: u32 = 30;
    let mut dump = false;
    let mut i = 1;
    while i < a.len() {
        let g = |i: usize| a.get(i + 1).cloned().unwrap_or_default();
        match a[i].as_str() {
            "--frames" => {
                frames = g(i).parse().unwrap_or(300);
                i += 2;
            }
            "--dt" => {
                dt = g(i).parse().unwrap_or(1.0 / 30.0);
                i += 2;
            }
            "--origin" => {
                origin = parse_vec3(&g(i), [0.0, 0.0, 0.0]);
                i += 2;
            }
            "--scale" => {
                let p: Vec<f32> = g(i).split_whitespace().filter_map(|x| x.parse().ok()).collect();
                if p.len() >= 2 {
                    scale = [p[0], p[1]];
                }
                i += 2;
            }
            "--ov" => {
                ov = serde_json::from_str(&g(i)).unwrap_or_default();
                i += 2;
            }
            "--label" => {
                label = g(i);
                i += 2;
            }
            "--every" => {
                every = g(i).parse().unwrap_or(30);
                i += 2;
            }
            "--dump" => {
                dump = true;
                i += 1;
            }
            other => {
                if i == 1 {
                    preset_path = other.to_string();
                }
                i += 1;
            }
        }
    }
    if preset_path.is_empty() {
        eprintln!("用法：boids_probe <preset.json> [--frames N] [--dt d] [--origin \"x y z\"] [--scale \"sx sy\"] [--ov json] [--label 名] [--every N]");
        std::process::exit(2);
    }
    let raw = match std::fs::read_to_string(&preset_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("读不到预设 {preset_path}：{e}");
            std::process::exit(2);
        }
    };
    let preset: ParticlePreset = match serde_json::from_str(&raw) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("预设解析失败（{}）：{e}", Path::new(&preset_path).display());
            std::process::exit(3);
        }
    };
    // 阈值**直接取自 JSON**（与实现读的是同一个字段名）。
    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
    let (mut nt, mut st) = (0.0f32, 0.0f32);
    let mut boids_ops = 0usize;
    if let Some(ops) = doc.get("operator").and_then(|v| v.as_array()) {
        for op in ops {
            if op.get("name").and_then(|v| v.as_str()) == Some("boids") {
                boids_ops += 1;
                nt = op.get("neighborthreshold").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                st = op
                    .get("separationthreshold")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(nt as f64) as f32;
            }
        }
    }
    let label = if label.is_empty() {
        Path::new(&preset_path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "preset".into())
    } else {
        label
    };
    println!(
        "=== boids_probe 「{label}」 算子 boids×{boids_ops}  邻居阈值 {nt:.1} 分离阈值 {st:.1}  \
         帧数 {frames} 步长 {dt:.5}s 原点 {origin:?} scale {scale:?}  ==="
    );
    if boids_ops == 0 {
        println!("（这份预设里没有 boids 算子 ⇒ 下面每个数字都是\"空转\"基线）");
    }
    let mut ps = ParticleSystem::new(preset, origin, scale, &ov, 1);
    let mut peak_alive = 0usize;
    let mut frames_with_nb = 0u32;
    let mut peak_with_nb = 0usize;
    let mut peak_mean_nb = 0.0f32;
    let mut run_min_pd = f32::MAX;
    let mut run_min_r = f32::MAX;
    let mut run_min_nn = f32::MAX;
    let mut run_max_nn = 0.0f32;
    let mut first: Option<Snap> = None;
    let mut last: Option<Snap> = None;
    let mut inward_tail = 0.0f32;
    let mut tail = 0u32;
    for f in 0..frames {
        ps.update(dt);
        let s = snapshot(&ps, nt, st);
        peak_alive = peak_alive.max(s.alive);
        if s.with_nb > 0 {
            frames_with_nb += 1;
        }
        peak_with_nb = peak_with_nb.max(s.with_nb);
        peak_mean_nb = peak_mean_nb.max(s.mean_nb);
        if s.alive >= 2 && s.min_pd > 0.0 {
            run_min_pd = run_min_pd.min(s.min_pd);
        }
        if s.rms_r > 0.0 {
            run_min_r = run_min_r.min(s.rms_r);
        }
        if s.mean_nn > 0.0 {
            run_min_nn = run_min_nn.min(s.mean_nn);
            run_max_nn = run_max_nn.max(s.mean_nn);
        }
        if f == 0 {
            first = Some(Snap { ..s });
        }
        if f + 1 == frames {
            last = Some(Snap { ..s });
        }
        if f + 1 > frames.saturating_sub(30) {
            inward_tail += s.inward;
            tail += 1;
        }
        if every > 0 && (f % every == 0 || f + 1 == frames) {
            if dump {
                let mut line = format!("[转储] 帧 {} t={:.3}s 存活 {}", f + 1, (f + 1) as f32 * dt, ps.particles.len());
                for p in &ps.particles {
                    line.push_str(&format!(
                        " | age={:.3} pos=({:.2},{:.2},{:.2}) vel=({:.1},{:.1},{:.2})",
                        p.age, p.pos[0], p.pos[1], p.pos[2], p.vel[0], p.vel[1], p.vel[2]
                    ));
                }
                println!("{line}");
            }
            println!(
                "[帧 {:4}] t={:6.2}s 存活 {:4}  有邻居 {:4}（平均 {:6.2}）  分离窗口内配对 {:4}  最近配对 {:9.2} 平均最近邻 {:9.2} 平均配对 {:9.2}  \
                 半径 {:9.2}  同向度 {:.4}  速度散布 {:8.2}  向心速度 {:9.2}  质心 ({:.0},{:.0},{:.0})",
                f + 1,
                (f + 1) as f32 * dt,
                s.alive,
                s.with_nb,
                s.mean_nb,
                s.sep_pairs,
                s.min_pd,
                s.mean_nn,
                s.mean_pd,
                s.rms_r,
                s.coherence,
                s.vel_spread,
                s.inward,
                s.centroid[0],
                s.centroid[1],
                s.centroid[2]
            );
        }
    }
    let f0 = first.unwrap_or(Snap {
        alive: 0,
        with_nb: 0,
        mean_nb: 0.0,
        min_pd: 0.0,
        mean_pd: 0.0,
        rms_r: 0.0,
        coherence: 0.0,
        vel_spread: 0.0,
        inward: 0.0,
        centroid: [0.0; 3],
        sep_pairs: 0,
        mean_nn: 0.0,
    });
    let f1 = last.unwrap_or(Snap { ..f0 });
    let shrink = if f0.rms_r > 1e-6 { f1.rms_r / f0.rms_r } else { 0.0 };
    let cd = [
        f1.centroid[0] - f0.centroid[0],
        f1.centroid[1] - f0.centroid[1],
        f1.centroid[2] - f0.centroid[2],
    ];
    println!(
        "[汇总  {label}] 存活峰值 {peak_alive} 末帧 {} | 有邻居的帧 {frames_with_nb}/{frames} 峰值 {peak_with_nb} 平均 {:5.2} | \
         分离窗口内配对 {}→{} | 全程最近配对 {:9.2} | 最近邻 {:9.2}→{:9.2}（min {:9.2} / max {:9.2}） | \
         半径 {:9.2}→{:9.2}（收缩率 {:.3}，全程最小 {:9.2}） | \
         同向度 {:.4}→{:.4} | 速度散布 {:8.2}→{:8.2} | 末 30 帧平均向心速度 {:9.2} | 质心位移 {:.2}",
        f1.alive,
        peak_mean_nb,
        f0.sep_pairs,
        f1.sep_pairs,
        run_min_pd.min(9.9999e6),
        f0.mean_nn,
        f1.mean_nn,
        run_min_nn.min(9.9999e6),
        run_max_nn,
        f0.rms_r,
        f1.rms_r,
        shrink,
        run_min_r.min(9.9999e6),
        f0.coherence,
        f1.coherence,
        f0.vel_spread,
        f1.vel_spread,
        if tail > 0 { inward_tail / tail as f32 } else { 0.0 },
        len3(cd)
    );
}
