//! 诊断 bin（**不影响渲染路径**）：定位 puppet 装配在时间推进时"几何消失/塌陷"的根因。
//!
//! 打印：
//!
//! * 每个采样时刻 `t` 的**非有限**骨矩阵个数与下标、非有限顶点个数；
//!
//! * 有限顶点构成的包围盒（网格单位）；
//!
//! * 逐层归因：只启用前 k 个动画层时，非有限骨/顶点是否出现（找出"哪一层开始坏"）；
//!
//! * 逐骨归因：哪些骨的 `final` 矩阵含 NaN/Inf，及其蒙皮权重覆盖的顶点数。
//! 用法：
//! `puppet_diag <场景目录> [--assets DIR] [--id 27 | --name 子串] [--t 0,0.1,0.5,5] [--bounds]`
use std::path::PathBuf;

use we_scene::model::Model;
use we_scene_render::mdl::{self, PoseLayer, PuppetPose};

fn arg1(args: &[String], k: &str) -> Option<String> {
    args.iter()
        .position(|a| a == k)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn fin(v: &[f32; 16]) -> bool {
    v.iter().all(|x| x.is_finite())
}

/// 标准 4×4 乘积（列主序存储，与 `mdl::chain_col` 同一约定）：`C = A·B`。
fn mul_col(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut c = [0.0f32; 16];
    for col in 0..4 {
        for r in 0..4 {
            let mut s = 0.0;
            for k in 0..4 {
                s += a[k * 4 + r] * b[col * 4 + k];
            }
            c[col * 4 + r] = s;
        }
    }
    c
}

/// 沿 `parents` 复合 `local` ⇒ bind 世界矩阵（= `mdl::chain_col` 同一算式，列向量约定）。
fn bind_world(n: usize, parents: &[i32], local: &[[f32; 16]]) -> Vec<[f32; 16]> {
    let mut w: Vec<[f32; 16]> = vec![[0.0; 16]; n];
    for i in 0..n {
        let p = parents[i];
        w[i] = if p < 0 || p as usize >= n {
            local[i]
        } else {
            mul_col(&w[p as usize], &local[i])
        };
    }
    w
}

/// 一个对象的**原始 local frame**（`T(origin)·Rz(az)·Ry(ay)·Rx(ax)·S(scale)`）——
/// 与渲染器 `lib.rs::local_raw_of` **同一算式**（那边还要乘父世界矩阵，这里只用相对量）。
fn local_raw_of(obj: &we_scene::scene::Object) -> [f32; 16] {
    let t = we_scene::scene::parse_vec3(&obj.origin).unwrap_or([0.0, 0.0, 0.0]);
    let s = we_scene::scene::parse_vec3(&obj.scale).unwrap_or([1.0, 1.0, 1.0]);
    let a = we_scene::scene::parse_vec3(&obj.angles).unwrap_or([0.0, 0.0, 0.0]);
    mul_col(
        &mul_col(
            &mul_col(
                &mul_col(
                    &{
                        let mut m = [0.0f32; 16];
                        m[0] = 1.0;
                        m[5] = 1.0;
                        m[10] = 1.0;
                        m[15] = 1.0;
                        m[12] = t[0];
                        m[13] = t[1];
                        m[14] = t[2];
                        m
                    },
                    &{
                        let (si, c) = a[2].sin_cos();
                        let mut m = [0.0f32; 16];
                        m[0] = c;
                        m[1] = si;
                        m[4] = -si;
                        m[5] = c;
                        m[10] = 1.0;
                        m[15] = 1.0;
                        m
                    },
                ),
                &{
                    let (si, c) = a[1].sin_cos();
                    let mut m = [0.0f32; 16];
                    m[0] = c;
                    m[2] = -si;
                    m[5] = 1.0;
                    m[8] = si;
                    m[10] = c;
                    m[15] = 1.0;
                    m
                },
            ),
            &{
                let (si, c) = a[0].sin_cos();
                let mut m = [0.0f32; 16];
                m[0] = 1.0;
                m[5] = c;
                m[6] = si;
                m[9] = -si;
                m[10] = c;
                m[15] = 1.0;
                m
            },
        ),
        &{
            let mut m = [0.0f32; 16];
            m[0] = s[0];
            m[5] = s[1];
            m[10] = s[2];
            m[15] = 1.0;
            m
        },
    )
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(1).cloned().unwrap_or_else(|| {
        eprintln!("用法: puppet_diag <场景目录> [--assets DIR] [--id N|--name 子串] [--t a,b,c]");
        std::process::exit(2);
    });
    let mut assets = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join("Downloads/wallpaper_engine/assets");
    if let Some(a) = arg1(&args, "--assets") {
        assets = PathBuf::from(a);
    }
    let want_id: Option<i64> = arg1(&args, "--id").and_then(|s| s.trim().parse().ok());
    let want_name: Option<String> = arg1(&args, "--name");
    let dump_anim: Option<i64> = arg1(&args, "--dumpanim").and_then(|s| s.trim().parse().ok());
    // `--bonedump 66,65,67`：逐骨打「父子 / rest / 每层每 t 的采样 TRS / final 平移」，
    // 用来和邻骨对比找异常点（纯诊断，不影响渲染）。
    let bone_dump: Vec<usize> = arg1(&args, "--bonedump")
        .map(|s| {
            s.split(',')
                .filter_map(|x| x.trim().parse::<usize>().ok())
                .collect()
        })
        .unwrap_or_default();
    // `--layerrot 66,65`：逐骨逐层打印**旋转**贡献（采样角 / 单层增量角 / 累加角）。
    let rot_dump: Vec<usize> = arg1(&args, "--layerrot")
        .map(|s| {
            s.split(',')
                .filter_map(|x| x.trim().parse::<usize>().ok())
                .collect()
        })
        .unwrap_or_default();
    let ts: Vec<f32> = arg1(&args, "--t")
        .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![0.0, 0.1, 0.2, 0.25, 0.3, 0.5, 1.0, 5.0]);

    let proj = we_scene::Project::read_dir(&dir).expect("读 project.json 失败");
    let pkg_bytes = std::fs::read(proj.scene_pkg()).expect("读 scene.pkg 失败");
    let pkg = we_scene::pkg::Pkg::parse(&pkg_bytes).expect("解析 scene.pkg 失败");
    let sj = pkg
        .scene_json_entry()
        .and_then(|e| pkg.slice(&pkg_bytes, e))
        .expect("包内没有 scene.json");
    let doc = we_scene::scene::parse_scene(sj).expect("解析 scene.json 失败");
    let (cw, ch) = (doc.general.orthographic.width, doc.general.orthographic.height);
    let read = |rel: &str| -> Option<Vec<u8>> {
        if let Some(b) = pkg.get(&pkg_bytes, rel) {
            return Some(b.to_vec());
        }
        std::fs::read(assets.join(rel)).ok()
    };

    println!("== {dir}  画布 {cw}x{ch}");
    let mut hit = 0usize;
    for obj in &doc.objects {
        if let Some(id) = want_id {
            if obj.id != id {
                continue;
            }
        }
        if let Some(n) = &want_name {
            if !obj.name.contains(n.as_str()) {
                continue;
            }
        }
        let Some(image) = obj.image.as_deref() else { continue };
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
                println!("\n[{}] id={} ✗ 解析失败: {e}", obj.name, obj.id);
                continue;
            }
        };
        let Some(pose) = mesh.pose.clone() else {
            println!("\n[{}] id={} 无骨架（跳过）", obj.name, obj.id);
            continue;
        };
        hit += 1;
        println!(
            "\n[{}] id={} 文件={pup} 顶点={} 三角={} size={} 动画层={:?}",
            obj.name,
            obj.id,
            mesh.positions.len(),
            mesh.indices.len() / 3,
            obj.size,
            pose.layer_playback()
        );
        let rd = mdl::parse_rig_data(&md).expect("rig");
        let anims = mdl::parse_animations(&md).unwrap_or_default();
        for &t in &ts {
            let f = pose.final_mats(t);
            let bad_bones: Vec<usize> = (0..f.len()).filter(|&i| !fin(&f[i])).collect();
            let asm = pose.assemble(&mesh.positions, &mesh.blend_indices, &mesh.blend_weights, t);
            let bad_v = asm
                .iter()
                .filter(|p| !p.iter().all(|x| x.is_finite()))
                .count();
            let (mut mnx, mut mxx, mut mny, mut mxy, mut mnz, mut mxz) = (
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::INFINITY,
                f32::NEG_INFINITY,
            );
            let mut nf = 0usize;
            for p in &asm {
                if p.iter().all(|x| x.is_finite()) {
                    nf += 1;
                    mnx = mnx.min(p[0]);
                    mxx = mxx.max(p[0]);
                    mny = mny.min(p[1]);
                    mxy = mxy.max(p[1]);
                    mnz = mnz.min(p[2]);
                    mxz = mxz.max(p[2]);
                }
            }
            println!(
                "  t={t:5.3}s 骨 NaN/Inf {} / {}  {:?}  顶点 NaN/Inf {} / {}  有限包围盒 x[{mnx:.1},{mxx:.1}] y[{mny:.1},{mxy:.1}] z[{mnz:.1},{mxz:.1}]",
                bad_bones.len(),
                f.len(),
                bad_bones.iter().take(12).collect::<Vec<_>>(),
                bad_v,
                asm.len(),
            );
            if !bad_bones.is_empty() {
                // 哪些顶点被坏骨影响（按 blend_indices + 权重）
                let mut affected = 0usize;
                let mut wsum = 0.0f32;
                for (i, p) in asm.iter().enumerate() {
                    if p.iter().all(|x| x.is_finite()) {
                        continue;
                    }
                    affected += 1;
                    if let (Some(bi), Some(bw)) = (mesh.blend_indices.get(i), mesh.blend_weights.get(i))
                    {
                        for k in 0..4 {
                            if bad_bones.contains(&(bi[k] as usize)) {
                                wsum += bw[k];
                            }
                        }
                    }
                }
                println!(
                    "          被坏骨覆盖的坏顶点 {affected} 个（这些顶点在坏骨上的权重和 = {wsum:.1}）"
                );
                for &b in bad_bones.iter().take(8) {
                    println!("            骨 {b}: {:?}", f[b]);
                }
            }
        }
        // `--dumpanim <id>`：把某条动画的"位移最大的骨"标出来，并列出它的 pos 曲线
        // （同时给出该骨在网格里被多少顶点、以多大权重使用 ⇒ 判"这条动画动的是不是人物本体"）
        if let Some(aid) = dump_anim {
            if let Some(a) = anims.iter().find(|a| a.id == aid) {
                let len = a.length.max(0) as usize;
                let mut best = (0.0f32, 0usize);
                for (bi, tr) in a.tracks.iter().enumerate() {
                    if tr.frames.len() <= len {
                        continue;
                    }
                    let f0 = tr.frames[0];
                    for k in 0..3 {
                        for f in 0..=len {
                            let d = (tr.frames[f].pos[k] - f0.pos[k]).abs();
                            if d > best.0 {
                                best = (d, bi);
                            }
                        }
                    }
                }
                let (dmax, bi) = best;
                println!(
                    "  [dumpanim {aid}] mode={:?} fps={} length={} ⇒ 位移最大骨={bi} |Δpos|={dmax:.1}",
                    a.mode, a.fps, a.length
                );
                let tr = &a.tracks[bi];
                let f0 = tr.frames[0];
                for f in [0usize, 1, 10, 30, 45, 60, 80, 88, 89, 90] {
                    if f >= tr.frames.len() {
                        continue;
                    }
                    let p = tr.frames[f].pos;
                    println!(
                        "      frame {f:3}: pos=({:.2},{:.2},{:.2}) Δ=({:.2},{:.2},{:.2})",
                        p[0],
                        p[1],
                        p[2],
                        p[0] - f0.pos[0],
                        p[1] - f0.pos[1],
                        p[2] - f0.pos[2]
                    );
                }
                // 该骨在网格里的使用情况
                let mut nv = 0usize;
                let mut wsum = 0.0f32;
                for (i, idx) in mesh.blend_indices.iter().enumerate() {
                    let w = mesh.blend_weights.get(i).copied().unwrap_or([0.0; 4]);
                    for k in 0..4 {
                        if idx[k] as usize == bi {
                            nv += 1;
                            wsum += w[k];
                        }
                    }
                }
                println!("      该骨被 {nv} 个顶点引用（权重和 {wsum:.1}）");
                // 谁的父链上有它
                let mut kids = Vec::new();
                for (i, p) in rd.parents.iter().enumerate() {
                    if *p == bi as i32 {
                        kids.push(i);
                    }
                }
                println!("      父={} 直接子骨={:?}", rd.parents.get(bi).copied().unwrap_or(-1), kids);
            }
        }
        // `--bonedump`：逐骨把「父子链 / rest / 每层每 t 的采样」打出来，和邻骨对比
        if !bone_dump.is_empty() {
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
            for &i in &bone_dump {
                if i >= rd.parents.len() {
                    println!("  骨 {i}: 超出骨架范围（{} 根）", rd.parents.len());
                    continue;
                }
                let kids: Vec<usize> = rd
                    .parents
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| **p == i as i32)
                    .map(|(j, _)| j)
                    .collect();
                let r = pose.rest_trs(i);
                println!(
                    "\n  ── 骨 {i}: parent={} 子={:?}",
                    rd.parents[i], kids
                );
                println!(
                    "     local_bind 平移=({:.2},{:.2},{:.2})  rest 平移=({:.2},{:.2},{:.2}) rest 缩放=({:.3},{:.3},{:.3})",
                    rd.local[i][12], rd.local[i][13], rd.local[i][14],
                    r.0[0], r.0[1], r.0[2], r.2[0], r.2[1], r.2[2]
                );
                if let Some(rf) = rd.reference.as_ref().and_then(|v| v.get(i)) {
                    println!("     reference 平移=({:.2},{:.2},{:.2})", rf[12], rf[13], rf[14]);
                }
                // 该骨在网格里的使用（顶点数 / 权重和）
                let mut nv = 0usize;
                let mut wsum = 0.0f32;
                for (vi, idx) in mesh.blend_indices.iter().enumerate() {
                    let w = mesh.blend_weights.get(vi).copied().unwrap_or([0.0; 4]);
                    for k in 0..4 {
                        if idx[k] as usize == i {
                            nv += 1;
                            wsum += w[k];
                        }
                    }
                }
                println!("     网格引用: {nv} 个顶点，权重和 {wsum:.1}");
                // bind 世界矩阵（一次算全链） ⇒ 每 t 的**骨枢轴实际位移**（这是"哪根骨带着
                // 那一块顶点整体飞走"的直接量）。
                let wb = bind_world(rd.parents.len(), &rd.parents, &rd.local);
                let bp = [wb[i][12], wb[i][13], wb[i][14]];
                print!("     bind pivot=({:.2},{:.2},{:.2})", bp[0], bp[1], bp[2]);
                for &t in &ts {
                    let f = pose.final_mats(t);
                    let rw = mul_col(&f[i], &wb[i]);
                    let ap = [rw[12], rw[13], rw[14]];
                    let d = [ap[0] - bp[0], ap[1] - bp[1], ap[2] - bp[2]];
                    print!(
                        "   t={t:.2}: pivot=({:.2},{:.2},{:.2}) Δ=({:.2},{:.2},{:.2}) |Δ|={:.2}",
                        ap[0], ap[1], ap[2], d[0], d[1], d[2],
                        (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
                    );
                }
                println!();
                for l in &layers {
                    let Some(a) = anims.iter().find(|a| a.id == l.id) else { continue };
                    let tr = a.tracks.get(i);
                    let has = tr.map(|t| t.has_samples()).unwrap_or(false);
                    let refp = tr
                        .and_then(|t| t.frames.first())
                        .map(|f| (f.pos, f.ang, f.scl));
                    print!(
                        "     anim {:5} ({} blend={:.2} rate={:.2}) has_samples={} 帧数={}",
                        l.id,
                        if l.additive { "add" } else { "base" },
                        l.blend,
                        l.rate,
                        has,
                        tr.map(|t| t.frames.len()).unwrap_or(0)
                    );
                    if let Some((p0, _, _)) = refp {
                        print!(" frame0 pos=({:.2},{:.2},{:.2})", p0[0], p0[1], p0[2]);
                    }
                    println!();
                    if !has {
                        continue;
                    }
                    for &t in &ts {
                        if let Some(s) = a.sample(i, t * l.rate) {
                            print!(
                                "        t={t:5.2} → pos=({:.2},{:.2},{:.2}) scl=({:.3},{:.3},{:.3}) 帧{} frac={:.2}",
                                s.pos[0], s.pos[1], s.pos[2], s.scl[0], s.scl[1], s.scl[2], s.idx, s.frac
                            );
                            if t == *ts.last().unwrap() {
                                let f = pose.final_mats(t);
                                if let Some(m) = f.get(i) {
                                    print!("   final 平移=({:.2},{:.2},{:.2})", m[12], m[13], m[14]);
                                }
                            }
                            println!();
                        }
                    }
                }
            }
        }
        // `--attach`：MDAT 挂接表 + bind_xform + 逐 t 的 attachmentTransform（诊断）
        // （第 6 轮：锚点**逐帧**求值）新增：
        // 对**每一条**挂接（不只是第一条）给出 t 时刻的锚点世界位（模型空间）——
        // 「逐帧真值」`attachmentTransform(t)` vs「修复前（烘 t=0）」`attachmentTransform(0)`
        // 的误差，以及它换到输出像素的量级（× 父对象 scale × out_w/画布宽）。
        // 另外把\"用这条挂接的子对象\"也一起算：子对象在**父模型空间**里的世界 T
        // = `anchor(t) · local_raw(子)`（与渲染器 `local_raw_of` 同一算式）——
        // 烘 t=0 与逐帧的差就是上次交底的那份漂移。
        if args.iter().any(|a| a == "--attach") {
            match mdl::parse_attachments(&md) {
                Ok(atts) if atts.is_empty() => println!("\n  MDAT：无（该 mdl 没有挂接表）"),
                Ok(atts) => {
                    let wb = bind_world(rd.parents.len(), &rd.parents, &rd.local);
                    println!("\n  MDAT：{} 条挂接", atts.len());
                    for a in &atts {
                        let b = a.bone_index as usize;
                        let direct = a.local_xform;
                        let bx = mdl::attachment_bind_xform(a, &rd.parents, &rd.local)
                            .unwrap_or([0.0; 16]);
                        // 交叉验证：bind_xform 应当 == Wb[bone] · local_xform
                        let cross = if b < wb.len() {
                            mul_col(&wb[b], &a.local_xform)
                        } else {
                            [0.0; 16]
                        };
                        let dx = (0..16).map(|k| (bx[k] - cross[k]).abs()).fold(0.0f32, f32::max);
                        println!(
                            "    {:<12} 骨={:<3} local T=({:.2},{:.2})  bind_xform T=({:.2},{:.2})  与 Wb[骨]·local 最大差={:.2e}",
                            a.name, a.bone_index, direct[12], direct[13], bx[12], bx[13], dx
                        );
                    }
                    // 父对象的 scale（模型单位 → 场景单位）与画布宽（场景单位 → 像素）。
                    // `--outw W`（默认 2560）用来把误差换成输出像素，与渲染器 CLI 的 `--size` 对齐。
                    let outw: f32 = arg1(&args, "--outw")
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(2560.0);
                    let sc = we_scene::scene::parse_vec3(&obj.scale).unwrap_or([1.0, 1.0, 1.0]);
                    let scl = (sc[0].abs() + sc[1].abs()) * 0.5;
                    let px = scl * outw / cw.max(1.0);
                    // 用这条挂接的子对象（数据驱动：obj.attachment == 名字 && obj.parent == 本对象 id）
                    for a0 in &atts {
                        let kids: Vec<&we_scene::scene::Object> = doc
                            .objects
                            .iter()
                            .filter(|o| {
                                o.parent == Some(obj.id)
                                    && o.attachment.as_deref() == Some(a0.name.as_str())
                            })
                            .collect();
                        let Some(m0) = pose.attachment_transform(a0, 0.0) else {
                            continue;
                        };
                        println!(
                            "\n  ══ 挂接「{}」（骨 {}）逐 t：逐帧真值 vs 烘 t=0（模型单位；px@{outw:.0} 用父 scale {scl:.5} × {outw:.0}/{cw:.0}）",
                            a0.name, a0.bone_index
                        );
                        for &t in &ts {
                            let Some(mt) = pose.attachment_transform(a0, t) else {
                                continue;
                            };
                            let d = [
                                mt[12] - m0[12],
                                mt[13] - m0[13],
                                mt[14] - m0[14],
                            ];
                            let dl = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
                            println!(
                                "     t={t:6.3}s 锚点世界位 真值(逐帧)=({:.2},{:.2},{:.2})  烘t0=({:.2},{:.2},{:.2})  误差|Δ|={dl:.4} 模型单位 = {:.2}px",
                                mt[12], mt[13], mt[14], m0[12], m0[13], m0[14], dl * px
                            );
                        }
                        // 子对象（挂在这条挂接上的层）在**父模型空间**里的世界 T
                        for k in &kids {
                            let lr = local_raw_of(k);
                            let lt = [lr[12], lr[13], lr[14]];
                            println!(
                                "     ↳ 子层「{}」id={} local_raw T=({:.2},{:.2})  attachments={}",
                                k.name,
                                k.id,
                                lt[0],
                                lt[1],
                                if k.attachment.is_some() { "有" } else { "无" }
                            );
                            for &t in &ts {
                                let Some(mt) = pose.attachment_transform(a0, t) else {
                                    continue;
                                };
                                let wt = mul_col(&mt, &lr);
                                let w0 = mul_col(&m0, &lr);
                                let d = [wt[12] - w0[12], wt[13] - w0[13], wt[14] - w0[14]];
                                let dl = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
                                println!(
                                    "        t={t:6.3}s 对象世界位(父模型空间) 真值=({:.2},{:.2})  烘t0=({:.2},{:.2})  误差|Δ|={dl:.4} = {:.2}px",
                                    wt[12], wt[13], w0[12], w0[13], dl * px
                                );
                            }
                        }
                    }
                }
                Err(e) => println!("\n  MDAT 解析失败：{e}"),
            }
        }
        // `--layerrot`：逐骨逐层打印旋转贡献（采样角 / 单层增量角 / 累加角）
        if !rot_dump.is_empty() {
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
            for &i in &rot_dump {
                if i >= rd.parents.len() {
                    continue;
                }
                println!("\n  ══ 骨 {i} 逐层旋转贡献（parent={}）", rd.parents[i]);
                for &t in &ts {
                    println!("     t={t:.2}");
                    for (id, pos, smp, inc, acc) in pose.layer_rot_probe(i, t) {
                        println!(
                            "        anim {id:5} pos={pos:7.1}s 采样角={smp:8.2}° 本层增量={inc:8.2}° 累加={acc:8.2}°"
                        );
                    }
                }
            }
        }
        // `--rig`：骨架/参考姿态的裸数据（判"动画值是不是相对 rest"）
        if args.iter().any(|a| a == "--rig") {
            let p = mdl::PuppetPose::new(&rd, &anims, &[]);
            let f0 = p.final_mats(0.0);
            for i in 0..rd.parents.len().min(6) {
                println!(
                    "  骨 {i}: parent={} local_bind 平移=({:.2},{:.2},{:.2}) 世界bind平移=({:.2},{:.2},{:.2}) final(0)平移=({:.2},{:.2},{:.2})",
                    rd.parents[i],
                    rd.local[i][12], rd.local[i][13], rd.local[i][14],
                    if i == 0 { rd.local[i][12] } else { 0.0 },
                    if i == 0 { rd.local[i][13] } else { 0.0 },
                    if i == 0 { rd.local[i][14] } else { 0.0 },
                    f0[i][12], f0[i][13], f0[i][14]
                );
                if let Some(r) = rd.reference.as_ref().and_then(|r| r.get(i)) {
                    println!("       reference 平移=({:.2},{:.2},{:.2})", r[12], r[13], r[14]);
                }
            }
            println!("   has reference(MDLSv2 装配参考) = {}", rd.reference.is_some());
            for a in &anims {
                if let Some(tr) = a.tracks.first() {
                    if !tr.frames.is_empty() {
                        let p0 = tr.frames[0].pos;
                        let pn = tr.frames.last().unwrap().pos;
                        println!(
                            "    anim {:5} mode={:8} 骨0 frame0 pos=({:.2},{:.2},{:.2}) 末帧 pos=({:.2},{:.2},{:.2})",
                            a.id, a.mode, p0[0], p0[1], p0[2], pn[0], pn[1], pn[2]
                        );
                    }
                }
            }
        }
        // `--refcheck`：逐动画统计"第 0 帧 ≠ rest"的骨数（= Auto 基准会切到新算式的骨）
        if args.iter().any(|a| a == "--refcheck") {
            for a in &anims {
                let mut bad: Vec<usize> = Vec::new();
                for i in 0..rd.parents.len() {
                    if let Some(tr) = a.tracks.get(i) {
                        if tr.frames.is_empty() {
                            continue;
                        }
                        let (p, q, s) = pose.rest_trs(i);
                        if !mdl::frame0_is_rest(&tr.frames[0], &p, &q, &s) {
                            bad.push(i);
                        }
                    }
                }
                if !bad.is_empty() {
                    println!(
                        "  anim {:5} mode={:8} 第0帧≠rest 的骨 {} 个（前几个：{:?}）",
                        a.id,
                        a.mode,
                        bad.len(),
                        bad.iter().take(8).collect::<Vec<_>>()
                    );
                }
            }
        }
        // 逐层归因：单层 / 前缀 k 层 的装配包围盒（看是谁把几何整体平移出画面）
        if args.iter().any(|a| a == "--layers") {
            let bbox_of = |p: &PuppetPose| -> (f32, f32) {
                let a = p.assemble(
                    &mesh.positions,
                    &mesh.blend_indices,
                    &mesh.blend_weights,
                    ts.get(1).copied().unwrap_or(0.5),
                );
                let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
                for q in &a {
                    if q[1].is_finite() {
                        lo = lo.min(q[1]);
                        hi = hi.max(q[1]);
                    }
                }
                (lo, hi)
            };
            for k in 1..=layers.len() {
                let p = PuppetPose::new(&rd, &anims, &layers[..k]);
                let (lo, hi) = bbox_of(&p);
                println!(
                    "  前缀 {:2} 层 [{:?}] y[{lo:.0},{hi:.0}]",
                    k,
                    layers[..k].iter().map(|l| l.id).collect::<Vec<_>>(),
                );
            }
            for (i, l) in layers.iter().enumerate() {
                let p = PuppetPose::new(&rd, &anims, std::slice::from_ref(l));
                let (lo, hi) = bbox_of(&p);
                println!(
                    "  单独层 {:2} [{:?}] y[{lo:.0},{hi:.0}]",
                    i,
                    (l.id, l.additive, l.blend, l.rate)
                );
            }
        }
    }
    if hit == 0 {
        println!("（没有匹配的 puppet 对象）");
    }
}
