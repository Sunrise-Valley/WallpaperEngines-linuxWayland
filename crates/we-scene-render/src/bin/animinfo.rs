//! 打印一个场景里所有 puppet 模型的 MDLA 动画信息：
//! id / name / **mode** / fps / length / 轨道数，以及
//! **端点连续性**（frame[0] ↔ frame[length]、frame[0] ↔ frame[length-1] 的逐分量最大差）。
//!
//! 用法：`animinfo <场景目录> [--assets <WE assets>]`
use std::path::PathBuf;

use we_scene::model::Model;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("用法: animinfo <场景目录> [--assets <目录>]");
    let mut assets = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join("Downloads/wallpaper_engine/assets");
    while let Some(a) = args.next() {
        if a == "--assets" {
            if let Some(v) = args.next() {
                assets = PathBuf::from(v);
            }
        }
    }

    let proj = we_scene::Project::read_dir(&dir).expect("读 project.json 失败");
    let pkg_bytes = std::fs::read(proj.scene_pkg()).expect("读 scene.pkg 失败");
    let pkg = we_scene::pkg::Pkg::parse(&pkg_bytes).expect("解析 scene.pkg 失败");
    let sj = pkg
        .scene_json_entry()
        .and_then(|e| pkg.slice(&pkg_bytes, e))
        .expect("包内没有 scene.json");
    let doc = we_scene::scene::parse_scene(sj).expect("解析 scene.json 失败");
    // 原始 JSON：`scene.rs` 的 `AnimationLayer` 还没解析 `rate`，这里直接读原值（只为诊断）。
    let raw: serde_json::Value = serde_json::from_slice(sj).unwrap_or_default();
    let rate_of = |oi: usize, li: usize| -> Option<f64> {
        raw.get("objects")?
            .get(oi)?
            .get("animationlayers")?
            .get(li)?
            .get("rate")?
            .as_f64()
    };

    let read = |rel: &str| -> Option<Vec<u8>> {
        if let Some(b) = pkg.get(&pkg_bytes, rel) {
            return Some(b.to_vec());
        }
        std::fs::read(assets.join(rel)).ok()
    };

    println!("== 场景 {} ==", dir);
    for (oi, obj) in doc.objects.iter().enumerate() {
        let Some(image) = obj.image.as_deref() else { continue };
        let Some(mj) = read(image) else { continue };
        let Some(m) = Model::parse(image, &mj) else { continue };
        let Some(pup) = m.puppet.as_deref() else { continue };
        let Some(md) = read(pup) else {
            println!("[{}] id={} puppet={} **文件读不到**", obj.name, obj.id, pup);
            continue;
        };
        println!(
            "\n[{}] id={} model={} puppet={} 层数={}",
            obj.name,
            obj.id,
            image,
            pup,
            obj.animation_layers.len()
        );
        for (li, l) in obj.animation_layers.iter().enumerate() {
            println!(
                "   layer animation={} additive={} blend={:?} visible={} rate={:?}",
                l.animation,
                l.is_additive(),
                l.blend,
                l.visible,
                rate_of(oi, li)
            );
        }
        match we_scene_render::mdl::parse_animations(&md) {
            Ok(anims) => {
                for a in &anims {
                    let ntr = a.tracks.iter().filter(|t| t.has_samples()).count();
                    println!(
                        "   anim id={} name={:?} mode={:?} fps={} length={} 有采样的骨={}/{}",
                        a.id,
                        a.name,
                        a.mode,
                        a.fps,
                        a.length,
                        ntr,
                        a.tracks.len()
                    );
                    // 端点连续性：frame[0] vs frame[len]、frame[0] vs frame[len-1]
                    let len = a.length.max(0) as usize;
                    let mut d_end = (0.0f32, 0.0f32, 0.0f32); // pos, ang, scl
                    let mut d_prev = (0.0f32, 0.0f32, 0.0f32);
                    let mut worst = String::new();
                    for t in a.tracks.iter().filter(|t| t.has_samples()) {
                        let f0 = t.frames[0];
                        if t.frames.len() > len {
                            let fe = t.frames[len];
                            let d = max3(&f0, &fe);
                            if d.0 > d_end.0 {
                                d_end = d;
                                worst = format!("骨 frame[0]↔frame[{}]", len);
                            }
                        }
                        if len > 0 && t.frames.len() > len - 1 {
                            let fp = t.frames[len - 1];
                            let d = max3(&f0, &fp);
                            d_prev = (d_prev.0.max(d.0), d_prev.1.max(d.1), d_prev.2.max(d.2));
                        }
                    }
                    println!(
                        "      端点差 frame[0]↔frame[{}]: pos {:.6} ang {:.6} scl {:.6}  ({})",
                        len, d_end.0, d_end.1, d_end.2, worst
                    );
                    println!(
                        "      峰值差 frame[0]↔frame[{}]: pos {:.6} ang {:.6} scl {:.6}",
                        len.saturating_sub(1),
                        d_prev.0,
                        d_prev.1,
                        d_prev.2
                    );
                }
            }
            Err(e) => println!("   MDLA 解析失败：{e}"),
        }
    }
}

/// (pos, ang, scl) 三个通道的逐分量最大绝对差
fn max3(a: &we_scene_render::mdl::Trs, b: &we_scene_render::mdl::Trs) -> (f32, f32, f32) {
    let m = |x: &[f32; 3], y: &[f32; 3]| {
        (0..3).map(|i| (x[i] - y[i]).abs()).fold(0.0f32, f32::max)
    };
    (m(&a.pos, &b.pos), m(&a.ang, &b.ang), m(&a.scl, &b.scl))
}
