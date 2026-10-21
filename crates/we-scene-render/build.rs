// SPDX-License-Identifier: MIT
//! 编译 `c/av_decode.c`（视频 0 拷贝用的进程内 VAAPI 解码）并链接 libav*。
//! 为什么解码这半边放在 C：`AVCodecContext` / `AVHWFramesContext` 的字段布局随 ffmpeg
//! 版本变，手抄成 Rust FFI 既长又没法校验。放 C 里由**真头文件**保证正确，Rust 侧只看到
//! 一个 5 函数的稳定 ABI（`c/av_decode.h`）。
//! 头文件是无 root 提的 dev 包（`apt-get download` + `dpkg -x`，做法写在
//! 说明 的常用命令里）。路径可用 `FFMPEG_DEV_ROOT` 覆盖。

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let dev = std::env::var("FFMPEG_DEV_ROOT").unwrap_or_else(|_| {
        format!(
            "{}/.local/opt/ffmpeg-dev",
            std::env::var("HOME").unwrap_or_else(|_| ".".into())
        )
    });
    let inc = PathBuf::from(&dev).join("usr/include/x86_64-linux-gnu");

    let src = manifest.join("c/av_decode.c");
    let hdr = manifest.join("c/av_decode.h");
    println!("cargo:rerun-if-changed={}", src.display());
    println!("cargo:rerun-if-changed={}", hdr.display());
    println!("cargo:rerun-if-env-changed=FFMPEG_DEV_ROOT");

    if !inc.join("libavcodec/avcodec.h").exists() {
        panic!(
            "找不到 libav 头文件：{}\n\
             无 root 取法（见 docs/BUILDING.md）：\n\
               apt-get download libavcodec-dev libavformat-dev libavutil-dev libdrm-dev\n\
               mkdir -p ~/.local/opt/ffmpeg-dev && for f in *.deb; do dpkg -x \"$f\" ~/.local/opt/ffmpeg-dev/; done\n\
             或者设 FFMPEG_DEV_ROOT 指向已有的 prefix。",
            inc.display()
        );
    }

    let obj = out.join("av_decode.o");
    let lib = out.join("libav_decode.a");
    let st = Command::new("gcc")
        .args(["-O2", "-fPIC", "-Wall", "-Wno-unused-parameter", "-I"])
        .arg(&inc)
        .arg("-c")
        .arg(&src)
        .arg("-o")
        .arg(&obj)
        .status()
        .expect("调不起 gcc");
    assert!(st.success(), "编译 c/av_decode.c 失败");
    let st = Command::new("ar")
        .arg("rcs")
        .arg(&lib)
        .arg(&obj)
        .status()
        .expect("调不起 ar");
    assert!(st.success(), "打包 libav_decode.a 失败");

    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=av_decode");
    // 系统里只有 libavcodec.so.63 这种 soname、没有链接器要的 .so ⇒ 用 verbatim 直连
    println!("cargo:rustc-link-lib=dylib:+verbatim=libavformat.so.63");
    println!("cargo:rustc-link-lib=dylib:+verbatim=libavcodec.so.63");
    println!("cargo:rustc-link-lib=dylib:+verbatim=libavutil.so.61");
}
