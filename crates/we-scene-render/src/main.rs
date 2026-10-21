// SPDX-License-Identifier: MIT
//
//! CLI：把某个 WE 壁纸目录的 `scene` 渲染成一张 PNG（用来肉眼验收 / 回归）。
//!
//! ```sh
//! export LD_LIBRARY_PATH=~/.local/opt/gl/root/usr/lib/x86_64-linux-gnu
//! we-scene-render <壁纸目录> out.png [宽] [--size WxH] [--frames N]
//! ```
//!
//! `--size 2560x1440` 用来和**实况**逐像素对比（内核按物理尺寸渲染）；不给就按场景比例推高度。
//! `--frames N` 连续渲 N 帧只留末帧（时钟每帧 +1/30 秒）⇒ 采样动画相位就靠它。
//! `--mouse X Y` 喂**指针位置**（归一化 [0,1]，y 向下；也接受 `--mouse X,Y`）—— 与内核
//! `wl_pointer` / `we-ctl mouse` **同一条通路**（`SceneRenderer::set_mouse`）⇒
//! 「鼠标驱动的特效」（X-Ray 透视、视差）落点可以**离线判**。

use std::path::Path;

use we_scene_render::SceneRenderer;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    // `--help` / `-h`：先认它，别把 `--help` 当场景目录（那会报
    // `读不了 --help/project.json`，既误导又难查）。
    if matches!(args.get(1).map(|s| s.as_str()), Some("--help") | Some("-h")) {
        eprintln!("用法: we-scene-render <壁纸目录> out.png [宽] [--size WxH] [--frames N] [--t 秒]");
        eprintln!("                    [--mouse X Y | --mouse X,Y] [--prop 名=值 ...]");
        eprintln!("  --mouse  X Y   指针位置（屏幕归一化 [0,1]，y 向下）。与内核 `wl_pointer` /");
        eprintln!("                 `we-ctl mouse` **同一条通路** ⇒ 鼠标驱动的特效（X-Ray/视差）可离线判。");
        eprintln!("  --prop   名=值  复现\"用户在 WE 界面里改的壁纸参数\"（可重复；等价 WE_PROP_<名>=<值>）");
        eprintln!("  --frames N     连续渲 N 帧只留末帧（固定步长 1/30 秒 ⇒ 相位可复现）");
        eprintln!("  --t      秒    定格在该场景时刻（逐位可复现，粒子不推进）");
        eprintln!("  墙钟     离线默认钉在 2025-01-01T00:00:00Z + t（时间/日期文字静态）；");
        eprintln!("          WE_NOW_EPOCH=<unix秒> 覆盖它（要对齐 win 参照帧的日期就设它）");
        eprintln!("  --dt     秒    **固定步长覆盖**（诊断：模拟慢帧，如 --dt 0.25；默认 1/30）");
        eprintln!("  --size   WxH   固定输出尺寸（默认按场景比例推高）");
        eprintln!("  --bench N / --opbench N  微基准（每帧 CPU 毫秒 / 单次 GL 调用耗时）");
        eprintln!("环境变量: WE_MOUSE=x,y（与 --mouse 同义）、WE_POINTER=0（关指针）、");
        eprintln!("          WE_POINTER_DEFAULT=zero（无输入时取 (0,0)，旧口径）");
        eprintln!("          ── 语义修正（默认开新行为，=0 回旧，可逐项 A/B）──");
        eprintln!("          WE_TEXT_NATURAL=0   字色只作用于字形，不再 multiply 到链接 RT 的人物上");
        eprintln!("          WE_TEXT_TINT=0.35   字色给字内人物染色的**强度**（1=旧的全染、0=完全不染，默认 0.35=轻）");
        eprintln!("          WE_TEXT_LAYOUT=0    文字回旧排版（块心/首行锚 + ascent−descent）；默认按场景排版框：");
        eprintln!("                              可用区=size−2·padding、块高=ascent+(n−1)·line_h（全部由场景参数推导）");
        eprintln!("          WE_TEXT_EM_FIT=<f>  字号标定覆盖（默认 1.0297 = win/我方同一 pointsize 的实测比；A/B 用）");
        eprintln!("          WE_TEXT_FIRSTLINE=0 仅旧排版有效：文字回到「块心居中」；默认按**首行**锚定");
        eprintln!("          WE_TEXT_DY=-5..+N   文字整层**屏幕像素**垂直微调（正数=向下，默认 0；位置已由场景排版框推导）");
        eprintln!("          WE_LINK_RT_FLIP=1   链接 RT 上下翻转（默认关＝屏上正立）");
        eprintln!("          WE_DOT_RAMP=0       点阵整段修正关闭（遮罩+半径，回旧；护栏用）");
        eprintln!("          WE_DOT_SHRINK=0     点阵半径不接遮罩渐变（**默认 0 = 作者口径**；\n                              0.6 = 旧默认，会点整体缩小 6.8%）");
        eprintln!("          WE_DOT_MIRROR=1     点阵遮罩 U 镜像（第一轮那条，方向与 win 相反，A/B 用）");
        eprintln!("          ── 点阵**时间驱动项** A/B（默认全关 = 现状，逐字节不变）──");
        eprintln!("          WE_DOT_MOVSIGN=-1   点阵平移方向反号（g_Time 项取负）");
        eprintln!("          WE_DOT_MOVRATE=0.5  点阵平移速率缩放（×0.5 / ×2 …）");
        eprintln!("          WE_DOT_NRATE=0      噪声场漂移速率缩放（0 = 相位冻在 0）");
        eprintln!("          WE_DOT_NPHASE=zero|norm  噪声相位冻结在 0 / 按 2s 周期归一化");
        eprintln!("          WE_DOT_RAMPSPACE=lattice 遮罩挂在点阵格子上（随点流），而不是屏固定");
        eprintln!("          WE_BLEND_SPACE=0     effects/blend 的 uv 按**画布**空间用（旧行为；默认 layer = 图层矩形，官方口径）");
        eprintln!("          WE_FX_TIME_SCALE=1   shader 的 g_Time 倍率（**偏好旋钮**，默认 1.0 = 官方；不碰网格/粒子时间）");
        eprintln!("          WE_LINK_RT_CONTENT=1 链接 RT 内容缩放（**默认 1.0 = 官方**：blend.vert 里没有这一项）");
        eprintln!("          WE_BLEND_YOFF=flip   blend 的 y 偏移项取反（A/B 实验；默认 official = 原文照抄）");
        eprintln!("          ── 性能（逐 pass 剖析 / 省带宽；**全部默认关**，开了才有像素变化）──");
        eprintln!("          WE_PROFILE_PASSES=1  逐趟 GL_TIME_ELAPSED（异步，不夹流水）→ 按 ms/帧降序打表");
        eprintln!("                              配 WE_PROFILE_OUT=<tsv> 落盘；报表含每帧全屏趟数");
        eprintln!("          WE_PERF_SCISSOR=1    逐趟剪到该 RT 的内容包围盒（**输出逐字节不变**）");
        eprintln!("          WE_PERF_SCISSOR_PAD=0.03  剪裁 padding（占短边比例；另有 96px 固定余量）");
        eprintln!("          WE_PERF_MASK_SCALE=0.75   遮罩烘焙缓冲边长系数（1.0=不缩）");
        eprintln!("          WE_PERF_RES_SCALE=1  超大链 RT（≥6MP）按可见分辨率降采样（上报尺寸仍是逻辑尺寸）");
        eprintln!("          WE_PERF_SCALE_TARGET=max|display|source  降采样上限口径（默认 max）");
        eprintln!("          WE_PERF_SCALE_MESH=1 允许对骨骼（puppet）层降采样（默认不动，发丝是高频）");
        eprintln!("          WE_PERF_SUPERSAMPLE=1.35  骨骼层链缓冲的超采样系数（缓冲像素/屏上脚印像素；本场景现值 1.5）");
        eprintln!("          WE_PERF_SUPERSAMPLE_MASK=1.8  遮罩烘焙缓冲的超采样系数（缓冲像素/源贴图像素；本场景现值 2.37）");
        eprintln!("          WE_ANIM_STARTPHASE=0 动画层起始相位关（默认读 visible 脚本的 setFrame）");
        return Ok(());
    }
    let dir = args.get(1).cloned().unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.local/share/Steam/steamapps/workshop/content/431960/1689515439")
    });
    let out = args.get(2).cloned().unwrap_or_else(|| "frame.png".into());
    let out_w: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1280);
    // frames N：连续渲染 N 帧只留末帧（用来验证"粒子真的在动"，也用来采样时间相位）
    let frames: u32 = args
        .iter()
        .position(|a| a == "--frames")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    // size WxH：**固定输出尺寸**（默认按场景比例推高度）。
    // 逐像素对比实况（内核按物理尺寸 2560x1440 渲）时必须用这个，否则宽高比都对不上。
    let size: Option<(u32, u32)> = args
        .iter()
        .position(|a| a == "--size")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.split_once(['x', 'X']))
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
    // t <秒>：把**场景时间**精确设到该时刻（动画/粒子相位）。用在"看 puppet 在 t=0.5s 的姿态"
    // 这类采样上：单帧渲染落在**精确**的时刻，而不是靠 `--frames N` 凑到最近的 1/30 秒。
    let at_t: Option<f32> = args
        .iter()
        .position(|a| a == "--t")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok());
    // dt <秒>：**固定步长覆盖**（诊断用）。离线默认口径是 1/30；设成 0.25 就等于
    // "每帧卡 250 ms"（内核转储/合成器卡顿时的慢帧就是这个量级）⇒ 用来验粒子在
    // 慢帧下是否丢时间。不给 = 老口径，逐字节不变。
    let dt_override: Option<f32> = args
        .iter()
        .position(|a| a == "--dt")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok());
    // bench N：只跑 N 帧 `draw_frame`（**内核每帧 CPU 侧的全部工作**，不含回读/swap），
    // 打印每帧毫秒。这台机器上 perf 不可用（paranoid=3、无 root）⇒ 只能这样量。
    // 配 `WE_FRAME_TIMING=1` 还会给出分相（清屏/网格/趟链/粒子/glow/present）。
    let bench: Option<u32> = args
        .iter()
        .position(|a| a == "--bench")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok());
    // opbench N：微基准（单次 `glGetError` / 一趟的状态调用串）——回答"GL 调用本身贵不贵"
    let opbench: Option<u32> = args
        .iter()
        .position(|a| a == "--opbench")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok());
    // mouse X Y（或 `--mouse X,Y`）：**喂指针位置**（归一化 [0,1]，y 从顶往下）。
    // 与内核完全同一条通路：内核 `wl_pointer.motion` → `set_mouse`、控制面 `we-ctl mouse` → `set_mouse`，
    // 这里也是 `set_mouse` ⇒ 离线图 = 屏上该指针位置下的画面（判「透镜/视差跟手」就靠它）。
    // 配合 `--t <秒>` 逐位置各渲一张 ⇒ 出一组"胶片"，逐格人眼判跟随与落点。
    let mouse_xy: Option<(f32, f32)> = args
        .iter()
        .position(|a| a == "--mouse")
        .and_then(|i| {
            let a = args.get(i + 1)?;
            if let Some((x, y)) = a.split_once(',') {
                Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
            } else {
                let y = args.get(i + 2)?;
                Some((a.trim().parse().ok()?, y.trim().parse().ok()?))
            }
        });

    // `--prop <名>=<值>`（可重复）：复现"用户在 WE 界面里改了壁纸参数"。
    // 场景里写成 `"visible": {"user":"<名>","value":<默认值>}` 的**对象/特效**随之显隐
    // （默认值在没给 `--prop` 时生效）。例：A(2887537617) 的丝袜 ——
    // `--prop newproperty=true` 开右腿丝袜、`--prop newproperty1=true` 开左腿丝袜。
    // 等价的环境变量形式是 `WE_PROP_<名>=<值>`（内核/别处用那个）。
    {
        let mut i = 0;
        while i < args.len() {
            if args[i] == "--prop" {
                if let Some(kv) = args.get(i + 1) {
                    if let Some((k, v)) = kv.split_once('=') {
                        we_scene::scene::userprop::set(k.trim(), we_scene::scene::userprop::parse_value(v));
                    } else {
                        eprintln!("[参数] --prop 的格式应是 <名>=<值>，忽略：{kv:?}");
                    }
                }
                i += 1;
            }
            i += 1;
        }
        if we_scene::scene::userprop::len() > 0 {
            println!("用户属性覆盖：{:?}", we_scene::scene::userprop::all());
        }
    }

    // 离线渲染默认**钉死墙钟**（`WE_NOW_EPOCH` 显式设置时以它为准）。
    // 场景里的时间/日期文字（脚本里的 `new Date`）每帧重跑一次：墙钟每秒都在变 ⇒
    // 同一个 `--t` / `--frames` 两次跑出的**文字画布宽度**不同（实测 2823687015 的
    // 「Time and Date」层链 RT 在四次运行里是 1776 / 1835 / 1842 / 1855 宽，全帧
    // 370 px 有差、最大 32/255）⇒ 「逐字节闸门」对这些场景永远不成立。
    // 钉在 `2025-01-01T00:00:00Z + t`：同参数 ⇒ 同像素；要让日期文字与 win 参照帧
    // 一致就显式设 `WE_NOW_EPOCH`（覆盖这里）。内核（live）不走这条路，仍是墙钟。
    if std::env::var_os("WE_NOW_EPOCH").is_none() {
        we_scene_render::text::pin_clock(1735689600.0 + at_t.unwrap_or(0.0) as f64);
    }

    // 先开一次只为拿场景尺寸（决定 PNG 高度按场景比例）
    let scene_size = SceneRenderer::open(Path::new(&dir), out_w, 1)?.scene_size;
    let (out_w, out_h) = match size {
        Some((w, h)) => (w, h),
        None => (out_w, ((out_w as f32 * scene_size.1 / scene_size.0).round() as u32).max(1)),
    };

    let mut r = SceneRenderer::open(Path::new(&dir), out_w, out_h)?;
    // `WE_SETPROP_REFRESH=<名>=<值>[;...]`：**建完之后**走"局部刷新"这条路改属性
    // （= 内核 `WE_SETPROP_FAST=1` 收到 `set-prop` 时做的事），用于 A/B 像素：
    // ① 重建路径：`--prop <名>=<值>`（值在建场景前就生效）
    // ② 局部刷新：`--prop <名>=<旧值>` + `WE_SETPROP_REFRESH=<名>=<值>`
    // 两条路的输出**必须逐字节相同**。
    if let Ok(spec) = std::env::var("WE_SETPROP_REFRESH") {
        for kv in spec.split(';').filter(|s| !s.trim().is_empty()) {
            let (k, v) = kv.split_once('=').unwrap_or((kv.trim(), ""));
            let t0 = std::time::Instant::now();
            match r.refresh_user_prop(k.trim(), v.trim()) {
                Ok(true) => println!(
                    "局部刷新 [{k}]={v}（未重建场景，{:.3} ms）",
                    t0.elapsed().as_secs_f64() * 1000.0
                ),
                Ok(false) => println!("局部刷新 [{k}]={v} ⇒ 不支持，未改动"),
                Err(e) => println!("局部刷新 [{k}]={v} ⇒ 失败：{e}"),
            }
        }
    }
    let (renderer, version) = r.tile_gl_info();
    println!("GL: {renderer} / {version}");
    println!(
        "壁纸: {}  场景 {}x{}  图层 {}（文字 {}）  显示裁切 {:?}",
        r.title, scene_size.0, scene_size.1, r.layer_count, r.text_count, r.cover
    );
    // 指针：在**任何一帧之前**喂进去（与内核"每帧渲染前 set_mouse"的语义一致；
    // 不给 `--mouse` 时完全不碰 ⇒ 老口径逐字节不变）。
    if let Some((mx, my)) = mouse_xy {
        r.set_mouse(mx, my);
        println!("鼠标喂入 ({mx:.4}, {my:.4})（归一化 [0,1]，y 向下）");
    }

    let mut buf = vec![0u8; (out_w * out_h * 4) as usize];
    // 基准模式：固定步长（可复现）+ 预热 20 帧（首帧含缓存/惰性初始化）后再量。
    if let Some(n) = opbench {
        r.set_fixed_dt(Some(1.0 / 30.0));
        let _ = r.bench_frames(20);
        let (us, acc) = r.bench_get_error(n);
        println!("glGetError：单次 {us:.3} µs（跑 {n} 次，校验和 {acc}）");
        println!("一趟状态串（绑目标+viewport+关/开混合+blend_func）：单次 {:.3} µs", r.bench_state_calls(n));
        for (name, us) in r.bench_ops(n) {
            println!("  {name:<28} {us:.4} µs/次");
        }
        return Ok(());
    }
    if let Some(n) = bench {
        r.set_fixed_dt(Some(1.0 / 30.0));
        let _ = r.bench_frames(20);
        let (n, ms) = r.bench_frames(n);
        let per = ms / n as f64;
        println!(
            "基准 {n} 帧：每帧 CPU {per:.3} ms（30fps 预算 33.3ms ⇒ 占 {:.0}%；换算 @30fps = {:.4} 核）",
            per / 33.333 * 100.0,
            per / 1000.0 * 30.0
        );
        return Ok(());
    }
    // ⚠ 内核走**墙钟**（WE 的 `g_Time`），但 CLI 采样动画相位必须是**确定性**的：
    // 同一个 `--frames N` 每次都要落在同一相位上。所以这里显式要求固定步长 1/30 秒。
    if frames > 1 {
        r.set_fixed_dt(Some(1.0 / 30.0));
    }
    if let Some(d) = dt_override {
        r.set_fixed_dt(Some(d));
    }
    if let Some(t) = at_t {
        // `--t` = **定格**在 t 秒（固定步长 0）⇒ `g_Time` 与网格动画用同一个 t，
        // 结果逐位可复现；粒子不推进（定格语义）。
        r.set_time_frozen(t);
        println!("（场景时间定格在 {t:.3} 秒）");
    }
    for i in 0..frames.max(1) {
        r.render_to(&mut buf, false)?; // PNG 要 RGBA；固定步长下每帧时钟前进 1/30 秒
        if i + 1 == frames && frames > 1 {
            println!("（连跑 {frames} 帧 ≈ {:.1} 秒）", frames as f32 / 30.0);
        }
    }
    image::save_buffer(&out, &buf, out_w, out_h, image::ColorType::Rgba8)?;
    println!("已写出 {out}（{out_w}x{out_h}）");
    // 音频谱状态：离线验收要能证明"采集线程真的跑了 N 个分析窗"（而不是只有一个环境变量）
    if we_scene_render::audio::enabled() {
        let s = we_scene_render::audio::spectrum();
        let top = s
            .bands
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, v)| format!("#{i}={v:.3}"))
            .unwrap_or_else(|| "-".into());
        println!(
            "[音频谱] {}  末帧总能量 {:.3} 峰值段 {}",
            we_scene_render::audio::capture_status(),
            s.level,
            top
        );
    }
    Ok(())
}
