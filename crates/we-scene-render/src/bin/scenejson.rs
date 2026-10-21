//! 把 scene.pkg 里的 raw scene.json 落盘（诊断用）：
//! `scenejson <场景目录> <out.json>`
use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("用法: scenejson <场景目录> <out.json>");
    let out = PathBuf::from(args.next().expect("缺 out.json"));
    let proj = we_scene::Project::read_dir(&dir).expect("读 project.json 失败");
    let bytes = std::fs::read(proj.scene_pkg()).expect("读 scene.pkg 失败");
    let pkg = we_scene::pkg::Pkg::parse(&bytes).expect("解析 scene.pkg 失败");
    let sj = pkg
        .scene_json_entry()
        .and_then(|e| pkg.slice(&bytes, e))
        .expect("包内没有 scene.json");
    std::fs::write(&out, sj).expect("写 out.json 失败");
    eprintln!("已写出 {}（{} 字节）", out.display(), sj.len());
    let _ = dir;
}
