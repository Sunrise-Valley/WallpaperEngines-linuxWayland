// SPDX-License-Identifier: MIT
//
//! **音频谱探针**（离线、无 GL）：把一段真实音频（裸 f32le 交错立体声 @48k，或先用
//! ffmpeg/parec 解出来）喂进生产分析器 [`we_scene_render::audio::Analyzer`]，打印
//! 16 段能量与总能量 —— 用来
//! ① 验证 DSP（对拍 `WE_AUDIO_BANDS` 的复现口径）；
//! ② 给渲染器造**可复现的真实音频频谱**（`WE_AUDIO_BANDS="…"` 直接回灌）。
//! 用法：
//! ```sh
//!
//! # 真实音频：抓 5 秒默认输出（= 壁纸/音乐真的在响），再分析
//! parec -d "$(pactl info | sed -n 's/^Default Sink: //p').monitor" \
//! format=float32le --rate=48000 --channels=2 > cap.f32 # 播放期间
//! audio_probe cap.f32 # 每窗 16 段 + 汇总
//! audio_probe cap.f32 --json # 汇总行（JSON，便于脚本喂 WE_AUDIO_BANDS）
//! audio_probe cap.f32 --quiet # 只打最后一行汇总
//! ```
//! `--rate` 默认 48000（本文件只做 f32le，不做重采样）。

use we_scene_render::audio::{self, Spectrum, BANDS, CHANNELS, SAMPLE_RATE};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut path = String::new();
    let mut dt = 1.0f32 / 30.0;
    let mut json = false;
    let mut quiet = false;
    let mut lines = 20usize;
    let mut i = 1;
    while i < a.len() {
        let get = |i: usize| a.get(i + 1).cloned().unwrap_or_default();
        match a[i].as_str() {
            "--dt" => {
                dt = get(i).parse().unwrap_or(dt);
                i += 2;
            }
            "--json" => {
                json = true;
                i += 1;
            }
            "--quiet" => {
                quiet = true;
                i += 1;
            }
            "--lines" => {
                lines = get(i).parse().unwrap_or(lines);
                i += 2;
            }
            "-h" | "--help" => {
                println!("audio_probe <pcm.f32> [--dt 0.0333] [--json] [--quiet] [--lines N]");
                return;
            }
            other => {
                if path.is_empty() {
                    path = other.to_string();
                }
                i += 1;
            }
        }
    }
    if path.is_empty() {
        eprintln!("缺 PCM 文件（裸 f32le 交错立体声 @48k）");
        std::process::exit(2);
    }

    let frames = match audio::analyze_pcm_file(std::path::Path::new(&path), Some(dt)) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    if frames.is_empty() {
        eprintln!("PCM 太短（不足一个 hop = {} 帧）", audio::FFT_N / 2);
        std::process::exit(1);
    }

    // 汇总：**峰值窗**（每段取全段最大）与**均值窗** —— 回灌给渲染器时用峰值窗最能代表"这段音频有多响"
    let mut peak = [0.0f32; BANDS];
    let mut mean = [0.0f32; BANDS];
    for f in &frames {
        for i in 0..BANDS {
            peak[i] = peak[i].max(f.bands[i]);
            mean[i] += f.bands[i] / frames.len() as f32;
        }
    }
    let pct = |s: &Spectrum| {
        let mut n = 0;
        for b in s.bands.iter() {
            if *b > 0.0 {
                n += 1;
            }
        }
        n
    };

    if !quiet {
        println!(
            "文件 {}（{} 窗，dt={:.4}s，48kHz/{}ch，{} 点 FFT，hop={}）",
            path,
            frames.len(),
            dt,
            CHANNELS,
            audio::FFT_N,
            audio::FFT_N / 2
        );
        println!("窗#\t活段\tlevel\t16 段");
        let step = (frames.len() / lines).max(1);
        for (n, f) in frames.iter().enumerate() {
            if n % step != 0 && n + 1 != frames.len() {
                continue;
            }
            let bands: Vec<String> = f.bands.iter().map(|b| format!("{b:.3}")).collect();
            println!(
                "{n}\t{}\t{:.3}\t[{}]",
                pct(f),
                f.level,
                bands.join(" ")
            );
        }
    }
    let peak_s = Spectrum {
        bands: peak,
        level: peak.iter().sum::<f32>() / BANDS as f32,
        generation: frames.len() as u64,
    };
    let mean_s = Spectrum {
        bands: mean,
        level: mean.iter().sum::<f32>() / BANDS as f32,
        generation: frames.len() as u64,
    };
    let fmt = |s: &Spectrum| -> String {
        s.bands
            .iter()
            .map(|b| format!("{b:.4}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    if json {
        println!(
            "{{\"frames\":{},\"active_peak\":{},\"level_peak\":{:.4},\"peak\":\"{}\",\"mean\":\"{}\"}}",
            frames.len(),
            pct(&peak_s),
            peak_s.level,
            fmt(&peak_s),
            fmt(&mean_s)
        );
    } else {
        println!("\n峰值窗（回灌用）：WE_AUDIO_BANDS=\"{}\"", fmt(&peak_s));
        println!("均值窗（参考）  ：{}", fmt(&mean_s));
        println!(
            "总能量：峰值 {:.4} / 均值 {:.4}；活段（>0）峰值 {} / {}",
            peak_s.level,
            mean_s.level,
            pct(&peak_s),
            pct(&mean_s)
        );
        println!(
            "（采样率常量 {}，样本数 = 文件字节 / 4 / {}）",
            SAMPLE_RATE, CHANNELS
        );
    }
}
