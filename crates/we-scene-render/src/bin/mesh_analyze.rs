//! 诊断 bin（不影响渲染路径）：看 puppet 网格的**三角形几何**，回答
//! 「屏幕上的细长黑线是网格本身的长条三角，还是索引/UV 读错的产物」。
//!
//! 用法： `mesh_analyze <场景目录> --id 27 [--assets DIR] [--top 20]`
//!
//! 打印：
//!
//! * 顶点/三角数、边长分位（p50/p90/p99/max）；
//!
//! * 最长的 N 条边的两端点位置、长度、uv、骨索引/权重；
//!
//! * 「细长三角」计数（最长边 / 最短边 > 8）。
use std::path::PathBuf;

use we_scene::model::Model;
use we_scene_render::mdl;

fn arg1(args: &[String], k: &str) -> Option<String> {
    args.iter()
        .position(|a| a == k)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(1).cloned().unwrap_or_else(|| {
        eprintln!("用法: mesh_analyze <场景目录> --id N [--assets DIR]");
        std::process::exit(2);
    });
    let mut assets = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join("Downloads/wallpaper_engine/assets");
    if let Some(a) = arg1(&args, "--assets") {
        assets = PathBuf::from(a);
    }
    let want_id: Option<i64> = arg1(&args, "--id").and_then(|s| s.trim().parse().ok());
    let top_n: usize = arg1(&args, "--top")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(20);

    let proj = we_scene::Project::read_dir(&dir).expect("读 project.json 失败");
    let pkg_bytes = std::fs::read(proj.scene_pkg()).expect("读 scene.pkg 失败");
    let pkg = we_scene::pkg::Pkg::parse(&pkg_bytes).expect("解析 scene.pkg 失败");
    let sj = pkg
        .scene_json_entry()
        .and_then(|e| pkg.slice(&pkg_bytes, e))
        .expect("包内没有 scene.json");
    let doc = we_scene::scene::parse_scene(sj).expect("解析 scene.json 失败");
    let read = |rel: &str| -> Option<Vec<u8>> {
        if let Some(b) = pkg.get(&pkg_bytes, rel) {
            return Some(b.to_vec());
        }
        std::fs::read(assets.join(rel)).ok()
    };

    for obj in &doc.objects {
        if let Some(id) = want_id {
            if obj.id != id {
                continue;
            }
        }
        let Some(image) = obj.image.as_deref() else { continue };
        let Some(mj) = read(image) else { continue };
        let Some(m) = Model::parse(image, &mj) else { continue };
        let Some(pup) = m.puppet.as_deref() else { continue };
        let Some(md) = read(pup) else { continue };
        let mesh = match mdl::parse_with_layers(&md, &[], 0.0) {
            Ok(m) => m,
            Err(e) => {
                println!("[{}] id={} ✗ {e}", obj.name, obj.id);
                continue;
            }
        };
        println!(
            "== [{}] id={} 顶点={} 三角={}",
            obj.name,
            obj.id,
            mesh.positions.len(),
            mesh.indices.len() / 3
        );
        let p = &mesh.positions;
        let dist = |a: [f32; 3], b: [f32; 3]| {
            ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
        };
        let mut edges: Vec<(f32, usize, usize)> = Vec::new();
        let mut thin = 0usize;
        for t in mesh.indices.chunks_exact(3) {
            let (i0, i1, i2) = (t[0] as usize, t[1] as usize, t[2] as usize);
            let e = [
                dist(p[i0], p[i1]),
                dist(p[i1], p[i2]),
                dist(p[i2], p[i0]),
            ];
            let mx = e.iter().cloned().fold(0.0f32, f32::max);
            let mn = e.iter().cloned().fold(f32::INFINITY, f32::min);
            if mn > 1e-6 && mx / mn > 8.0 {
                thin += 1;
            }
            let mut idx = [i0, i1, i2];
            let mut j = [(0usize, 1usize), (1, 2), (2, 0)];
            // 记录该三角的最长边
            let mut best = 0usize;
            for k in 1..3 {
                if e[k] > e[best] {
                    best = k;
                }
            }
            let (a, b) = j[best];
            edges.push((e[best], idx[a], idx[b]));
            idx = [0, 0, 0];
            j = [(0, 0), (0, 0), (0, 0)];
        }
        let mut lens: Vec<f32> = edges.iter().map(|e| e.0).collect();
        lens.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let q = |f: f64| lens[((lens.len() - 1) as f64 * f) as usize];
        println!(
            "   三角最长边: p50={:.1} p90={:.1} p99={:.1} max={:.1}   细长三角(最长/最短>8) = {}/{}",
            q(0.5),
            q(0.9),
            q(0.99),
            lens[lens.len() - 1],
            thin,
            mesh.indices.len() / 3
        );
        edges.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        for (d, a, b) in edges.iter().take(top_n) {
            let (pa, pb) = (p[*a], p[*b]);
            let (ua, ub) = (
                mesh.uvs.get(*a).copied().unwrap_or([0.0; 2]),
                mesh.uvs.get(*b).copied().unwrap_or([0.0; 2]),
            );
            let (ba, bb) = (
                mesh.blend_indices.get(*a).copied().unwrap_or([0; 4]),
                mesh.blend_indices.get(*b).copied().unwrap_or([0; 4]),
            );
            let (wa, wb) = (
                mesh.blend_weights.get(*a).copied().unwrap_or([0.0; 4]),
                mesh.blend_weights.get(*b).copied().unwrap_or([0.0; 4]),
            );
            println!(
                "   边 {d:8.1}  v{a}({:.0},{:.0})  uv=({:.3},{:.3}) bones={:?} w={:?}  --  v{b}({:.0},{:.0}) uv=({:.3},{:.3}) bones={:?} w={:?}",
                pa[0], pa[1], ua[0], ua[1], ba, wa, pb[0], pb[1], ub[0], ub[1], bb, wb
            );
        }
    }
}
