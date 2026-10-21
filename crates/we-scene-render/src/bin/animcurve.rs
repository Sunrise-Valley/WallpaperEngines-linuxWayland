//! 打印某个 MDLA 动画的**逐帧姿态曲线**（每帧相对 frame[0] 的逐骨最大差），
//! 用来判断作者做的是"无缝循环"还是"半程（mirror）"。
//!
//! 用法：`animcurve <场景目录> <puppet 文件相对路径> <anim id>` [--assets DIR] [--bone N]
use std::path::PathBuf;

use we_scene::model::Model;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("用法: animcurve <场景目录> <puppet> <anim id>");
    let want = args.next().unwrap_or_default();
    let anim_id: i64 = args.next().unwrap_or_default().parse().expect("anim id");
    let mut assets = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join("Downloads/wallpaper_engine/assets");
    let mut bone = 0usize;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--assets" => assets = PathBuf::from(args.next().unwrap_or_default()),
            "--bone" => bone = args.next().unwrap_or_default().parse().unwrap_or(0),
            _ => {}
        }
    }

    let proj = we_scene::Project::read_dir(&dir).expect("读 project.json 失败");
    let pkg_bytes = std::fs::read(proj.scene_pkg()).expect("读 scene.pkg 失败");
    let pkg = we_scene::pkg::Pkg::parse(&pkg_bytes).expect("解析 scene.pkg 失败");
    let read = |rel: &str| -> Option<Vec<u8>> {
        if let Some(b) = pkg.get(&pkg_bytes, rel) {
            return Some(b.to_vec());
        }
        std::fs::read(assets.join(rel)).ok()
    };

    let md = read(&want).unwrap_or_else(|| panic!("读不到 {want}"));
    let anims = we_scene_render::mdl::parse_animations(&md).expect("MDLA 解析失败");
    let a = anims.iter().find(|a| a.id == anim_id).expect("没有这个 anim id");
    println!(
        "anim {} {:?} mode={:?} fps={} length={} 轨道={}",
        a.id,
        a.name,
        a.mode,
        a.fps,
        a.length,
        a.tracks.len()
    );
    let len = a.length.max(0) as usize;
    // 每个帧号上：所有骨相对 frame[0] 的最大 pos / ang / scl 差
    let mut worst_bone = 0usize;
    let mut worst = 0.0f32;
    for (bi, t) in a.tracks.iter().enumerate() {
        if !t.has_samples() || t.frames.len() <= len {
            continue;
        }
        let f0 = t.frames[0];
        for f in 0..=len {
            let d = (f0.ang[0] - t.frames[f].ang[0])
                .abs()
                .max((f0.ang[1] - t.frames[f].ang[1]).abs())
                .max((f0.ang[2] - t.frames[f].ang[2]).abs());
            if d > worst {
                worst = d;
                worst_bone = bi;
            }
        }
    }
    println!("振幅最大的骨 = {worst_bone}（最大相对角度差 {worst:.6} rad）");
    println!("f    pos|d|      ang|d|      scl|d|   | 骨的 ang.z（弧度）");
    for f in 0..=len {
        let mut dp = 0.0f32;
        let mut da = 0.0f32;
        let mut ds = 0.0f32;
        for t in a.tracks.iter().filter(|t| t.has_samples() && t.frames.len() > len) {
            let f0 = t.frames[f];
            let g = t.frames[0];
            for k in 0..3 {
                dp = dp.max((f0.pos[k] - g.pos[k]).abs());
                da = da.max((f0.ang[k] - g.ang[k]).abs());
                ds = ds.max((f0.scl[k] - g.scl[k]).abs());
            }
        }
        let az = a
            .tracks
            .get(bone)
            .and_then(|t| t.frames.get(f))
            .map(|t| t.ang[2])
            .unwrap_or(f32::NAN);
        let _ = Model::parse;
        println!("{f:3}  {dp:9.4}  {da:9.4}  {ds:9.4}   | {az:9.5}");
    }
}
