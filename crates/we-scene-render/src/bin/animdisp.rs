//! 逐帧位移曲线（**几何口径**，无需 GL）：把一个场景里每个 puppet 部件在 1/30 秒步长上的
//! 装配顶点序列算出来，打印
//!
//! * `step`：相邻两帧之间的逐顶点位移（mean / max）—— **跳变就是"瞬间回跳"**；
//!
//! * `rel`：相对 t=0 的逐顶点位移（mean / max）—— 看摆动幅度与往返形状。
//! 单位给出两套：网格单位（= 场景单位）与输出像素（× out_w / 画布宽）。
//!
//! 用法：`animdisp <场景目录> [--assets DIR] [--fps 30] [--secs 6] [--out-w 1600] [--legacy]
//! [--name <子串>] [--curve]`
//! `--legacy` 会置 `WE_ANIM_MODE=legacy`（= 之前：不读 mode / 不读 rate）。
use std::path::PathBuf;

use we_scene::model::Model;
use we_scene_render::mdl::{self, PoseLayer};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(1).cloned().unwrap_or_else(|| {
        eprintln!("用法: animdisp <场景目录> [--assets DIR] [--fps 30] [--secs 6] [--legacy] [--curve]");
        std::process::exit(2);
    });
    let arg1 = |k: &str| -> Option<String> {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let mut assets = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join("Downloads/wallpaper_engine/assets");
    if let Some(a) = arg1("--assets") {
        assets = PathBuf::from(a);
    }
    let fps: f32 = arg1("--fps").and_then(|s| s.parse().ok()).unwrap_or(30.0);
    let secs: f32 = arg1("--secs").and_then(|s| s.parse().ok()).unwrap_or(6.0);
    let out_w: f32 = arg1("--out-w").and_then(|s| s.parse().ok()).unwrap_or(1600.0);
    let name_filter = arg1("--name");
    let curve = args.iter().any(|a| a == "--curve");
    if args.iter().any(|a| a == "--legacy") {
        std::env::set_var("WE_ANIM_MODE", "legacy");
    }

    let proj = we_scene::Project::read_dir(&dir).expect("读 project.json 失败");
    let pkg_bytes = std::fs::read(proj.scene_pkg()).expect("读 scene.pkg 失败");
    let pkg = we_scene::pkg::Pkg::parse(&pkg_bytes).expect("解析 scene.pkg 失败");
    let sj = pkg
        .scene_json_entry()
        .and_then(|e| pkg.slice(&pkg_bytes, e))
        .expect("包内没有 scene.json");
    let doc = we_scene::scene::parse_scene(sj).expect("解析 scene.json 失败");
    let (cw, ch) = (doc.general.orthographic.width, doc.general.orthographic.height);
    let px_per_unit = if cw > 0.0 { out_w / cw } else { 1.0 };

    let read = |rel: &str| -> Option<Vec<u8>> {
        if let Some(b) = pkg.get(&pkg_bytes, rel) {
            return Some(b.to_vec());
        }
        std::fs::read(assets.join(rel)).ok()
    };

    println!("== {dir}");
    println!(
        "画布 {}x{}  输出宽 {} ⇒ 1 网格单位 = {:.4} px；步长 1/{:.0}s；时长 {:.1}s；{}",
        cw,
        ch,
        out_w,
        px_per_unit,
        fps,
        secs,
        if std::env::var("WE_ANIM_MODE").ok().as_deref() == Some("legacy") {
            "【legacy：不读 mode/rate = 改前行为】"
        } else {
            "【默认：按 MDLA 的 mode + animationlayers 的 rate】"
        }
    );

    for obj in &doc.objects {
        let Some(image) = obj.image.as_deref() else { continue };
        if let Some(f) = &name_filter {
            if !obj.name.contains(f.as_str()) {
                continue;
            }
        }
        let Some(mj) = read(image) else { continue };
        let Some(m) = Model::parse(image, &mj) else { continue };
        let Some(pup) = m.puppet.as_deref() else { continue };
        let Some(md) = read(pup) else { continue };
        let layers: Vec<PoseLayer> = obj
            .animation_layers
            .iter()
            .filter(|l| l.visible && l.blend_weight() > 0.0)
            .map(|l| PoseLayer {
                id: l.animation,
                additive: l.is_additive(),
                blend: l.blend_weight(),
                rate: l.rate(),
                start: (0.0, 0.0),
            })
            .collect();
        let mesh = match mdl::parse_with_layers(&md, &layers, 0.0) {
            Ok(m) => m,
            Err(e) => {
                println!("\n[{}] id={} ✗ {e}", obj.name, obj.id);
                continue;
            }
        };
        let pose = match mesh.pose.as_ref() {
            Some(p) => p,
            None => {
                println!("\n[{}] id={} 无骨架/动画（跳过）", obj.name, obj.id);
                continue;
            }
        };
        println!(
            "\n[{}] id={} 顶点={} 动画层={:?}",
            obj.name,
            obj.id,
            mesh.positions.len(),
            pose.layer_playback()
        );
        let n = (secs * fps).round() as usize;
        let mut prev = pose.assemble(&mesh.positions, &mesh.blend_indices, &mesh.blend_weights, 0.0);
        let p0 = prev.clone();
        let mut worst = (0.0f32, 0usize, 0.0f32);
        let mut rows: Vec<(usize, f32, f32, f32, f32, f32)> = Vec::new();
        for k in 1..=n {
            let t = k as f32 / fps;
            let cur = pose.assemble(&mesh.positions, &mesh.blend_indices, &mesh.blend_weights, t);
            let mut sm = 0.0f32;
            let mut sx = 0.0f32;
            let mut rm = 0.0f32;
            let mut rx = 0.0f32;
            for i in 0..cur.len().min(prev.len()) {
                let d = ((cur[i][0] - prev[i][0]).powi(2)
                    + (cur[i][1] - prev[i][1]).powi(2)
                    + (cur[i][2] - prev[i][2]).powi(2))
                .sqrt();
                sm += d;
                sx = sx.max(d);
                let r = ((cur[i][0] - p0[i][0]).powi(2)
                    + (cur[i][1] - p0[i][1]).powi(2)
                    + (cur[i][2] - p0[i][2]).powi(2))
                .sqrt();
                rm += r;
                rx = rx.max(r);
            }
            let cnt = cur.len().max(1) as f32;
            sm /= cnt;
            rm /= cnt;
            rows.push((k, t, sm, sx, rm, rx));
            if sx > worst.0 {
                worst = (sx, k, t);
            }
            prev = cur;
        }
        if curve {
            println!("  帧(1/{fps:.0}s)  step_mean  step_max   rel_mean  rel_max   [单位=网格单位]");
            for (k, t, sm, sx, rm, rx) in &rows {
                println!(
                    "  {:4} t={:5.3}s  {:9.5} {:9.5}  {:9.5} {:9.5}",
                    k, t, sm, sx, rm, rx
                );
            }
        }
        let (sx, k, t) = worst;
        let max_rel = rows.iter().map(|r| r.5).fold(0.0f32, f32::max);
        let max_step_mean = rows.iter().map(|r| r.2).fold(0.0f32, f32::max);
        let med_step: f32 = {
            let mut v: Vec<f32> = rows.iter().map(|r| r.3).collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v.get(v.len() / 2).copied().unwrap_or(0.0)
        };
        println!(
            "  ★ 最大单帧跳变量 step_max = {:.4} 网格单位 = {:.3} px（第 {} 帧 / t={:.3}s）",
            sx,
            sx * px_per_unit,
            k,
            t
        );
        println!(
            "    中位单帧 step_max = {:.4} 单位 = {:.3} px；比值 = {:.1}×",
            med_step,
            med_step * px_per_unit,
            if med_step > 0.0 { sx / med_step } else { f32::INFINITY }
        );
        println!(
            "    最大相对幅度 rel_max = {:.4} 单位 = {:.3} px；step_mean 峰值 = {:.5} 单位",
            max_rel,
            max_rel * px_per_unit,
            max_step_mean
        );
    }
}
