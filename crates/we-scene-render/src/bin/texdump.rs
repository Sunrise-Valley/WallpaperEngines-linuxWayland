// SPDX-License-Identifier: MIT
//
// 诊断小工具：把 scene.pkg 里的一张 `.tex` 解出来存成 PNG，用来肉眼验收解码器
// （DXT/LZ4/裸像素这些"能出图但颜色可能错"的路径，必须看图）。
//
// 用法：texdump <壁纸目录或 scene.pkg> <贴图名子串> <输出.png>

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("用法：texdump <壁纸目录|scene.pkg> <贴图名子串> <输出.png>");
        return ExitCode::from(2);
    }
    let pkg_path = if args[1].ends_with(".pkg") {
        std::path::PathBuf::from(&args[1])
    } else {
        std::path::Path::new(&args[1]).join("scene.pkg")
    };
    let data = match std::fs::read(&pkg_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("读不了 {}：{e}", pkg_path.display());
            return ExitCode::FAILURE;
        }
    };
    let pkg = match we_scene::pkg::Pkg::parse(&data) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("pkg 解析失败：{e}");
            return ExitCode::FAILURE;
        }
    };

    // 名字里含子串的所有 .tex（`--all` 时不限 .tex）；默认只取第一个
    let mut hits: Vec<&we_scene::pkg::Entry> = pkg
        .entries
        .iter()
        .filter(|e| e.name.contains(&args[2]) && e.name.ends_with(".tex"))
        .collect();
    hits.sort_by_key(|e| e.name.clone());
    if hits.is_empty() {
        eprintln!("包里没有名字含 {:?} 的 .tex", args[2]);
        return ExitCode::FAILURE;
    }
    let e = hits[0];
    let bytes = pkg.slice(&data, e).unwrap();
    println!("贴图 {}  {} 字节", e.name, bytes.len());

    match we_scene::tex::decode(bytes) {
        Ok(we_scene::tex::Decoded::Rgba(p)) => {
            println!("  裸像素 {}x{}", p.width, p.height);
            let img = image::RgbaImage::from_raw(p.width, p.height, p.rgba).unwrap();
            img.save(&args[3]).unwrap();
            println!("  已写出 {}", args[3]);
        }
        Ok(we_scene::tex::Decoded::Container(kind, data)) => {
            println!("  容器 {:?} {} 字节", kind, data.len());
            let img = image::load_from_memory(&data).unwrap().to_rgba8();
            println!("  解出 {}x{}", img.width(), img.height());
            img.save(&args[3]).unwrap();
            println!("  已写出 {}", args[3]);
        }
        Err(err) => {
            eprintln!("  解不开：{err}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}
