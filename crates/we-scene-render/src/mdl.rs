//! WE 的 **puppet（MDLV）网格**解析。
//! 背景：
//!
//! * 文件头 9 字节是版本串：本机场景里是 `MDLV0017`（5 个）与 `MDLV0023`（`хало`）。
//! ⚠ 参照实现（linux-wallpaperengine `CImage::loadPuppetMesh`）**只认 0021/0023**
//! ⇒ 它在这个场景上直接报错，所以我们没有 oracle，格式是按数据推的。
//!
//! * 结构：头 →（若干块）→ `MDLS` 标记；网格块用**启发式扫描**定位（同参照）：
//! 在偏移处读一个 uint32 当"顶点字节数"，要求非 0 且是 **stride 80** 的整数倍；
//! 其后紧跟的 uint32 当"索引字节数"，要求非 0 且是 6（=uint16×3）的整数倍。
//!
//! * 顶点 stride = 80 字节：位置 @0（3×f32）、法线 @12、切线 @24、**@40 一个 uint32**
//! （实测 0~12，疑为"主骨骼索引"）、@52 两个 f32、uv @72（2×f32）。
//!
//! * 索引：uint16 三元组。
//!
//! ⚠ 这只是 **M1：静态网格**。骨骼动画（M2）需要 @40 那个索引指向的骨骼/动画块，
//! 尚未逆向。

/// 解析出来的网格（模型自身坐标系：x 右、y 上、单位与图层 `size` 同尺度、原点在图层中心）。
#[derive(Debug, Clone, Default)]
pub struct PuppetMesh {
    pub version: String,
    /// **未装配**的顶点（= 作者在贴图上直接摆的"散件"布局，与贴图 uv 一一对应；
    /// MDLV0017 实测 `pos == (uv-0.5)*size`，z 恒 0）。
    pub positions: Vec<[f32; 3]>,
    /// **装配后**的顶点（用 MDLSv2 的 `animation_reference` 骨矩阵做 4 骨蒙皮；
    /// "老 MDLS 的 bind 姿态描述的是散件、装配姿态另有其表" —— 见 `parse_rig`）。
    /// 拿不到骨架时为 `None`，调用方退回 `positions`。
    pub assembled: Option<Vec<[f32; 3]>>,
    pub uvs: Vec<[f32; 2]>,
    /// 逐顶点 @40 的第一个 uint32（保留旧字段语义：贴图通道/首骨索引）
    pub blend: Vec<u32>,
    /// 逐顶点 @40 的 4 个 uint32 = 蒙皮用的**骨骼索引**（stride 内偏移 40）
    pub blend_indices: Vec<[u32; 4]>,
    /// 逐顶点 @56 的 4 个 f32 = 蒙皮**权重**（stride 内偏移 56，和恒 ≈1）
    pub blend_weights: Vec<[f32; 4]>,
    pub indices: Vec<u16>,
    /// **时间动画**（骨架 + `animationlayers` 的动画轨道）：有它就能按时间 `t` 重算装配顶点
    /// （见 [`PuppetMesh::assemble_at`]）。拿不到骨架/动画时为 `None` ⇒ 只能静态装配。
    /// puppet 的"动起来"（腿部晃动/摇头）靠它。
    pub pose: Option<PuppetPose>,
}

impl PuppetMesh {
    /// 按**动画时间** `t`（秒，与 `g_Time` 同一只钟）重算装配后的顶点。
///
    /// `t = 0` 时返回的顶点与建网格时（[`parse_with_layers`] 传 `t = 0`）**逐位相同**，
    /// 所以"上屏/评分在 t=0 不变"是构造性保证，不靠巧合。
    /// 没有动画数据（`pose == None`）时返回 `None` ⇒ 调用方用 `geometry`。
    pub fn assemble_at(&self, t: f32) -> Option<Vec<[f32; 3]>> {
        self.pose.as_ref().map(|p| {
            p.assemble(
                &self.positions,
                &self.blend_indices,
                &self.blend_weights,
                t,
            )
        })
    }
}

impl PuppetMesh {
    pub fn vertex_count(&self) -> usize {
        self.positions.len()
    }
    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }
    /// 实际用于渲染的顶点：优先"装配后"的几何，没有就退回原始散件。
    pub fn geometry(&self) -> &[[f32; 3]] {
        self.assembled.as_deref().unwrap_or(&self.positions)
    }
}

const MARKER_SIZE: usize = 9;
const VERTEX_STRIDE: usize = 80;
const UV_OFFSET: usize = 72;
const BLEND_OFFSET: usize = 40;
const WEIGHT_OFFSET: usize = 56;
const MESH_HEADER_SIZE: usize = 8; // 顶点字节数 + 索引字节数

fn u32_at(data: &[u8], off: usize) -> Option<u32> {
    if off + 4 > data.len() {
        return None;
    }
    Some(u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]))
}

fn f32_at(data: &[u8], off: usize) -> Option<f32> {
    u32_at(data, off).map(f32::from_bits)
}

fn struct_i32(data: &[u8], off: usize) -> Option<i32> {
    u32_at(data, off).map(|v| v as i32)
}

/// 找 `MDLS` 标记（网格区之后的块；参照用它当扫描上界）。
fn find_mdls(data: &[u8]) -> usize {
    for off in MARKER_SIZE..data.len().saturating_sub(4) {
        if &data[off..off + 4] == b"MDLS" {
            return off;
        }
    }
    data.len()
}

/// 解析网格。`version` 保留原文（调用方决定是否接受）。
pub fn parse(data: &[u8]) -> Result<PuppetMesh, String> {
    parse_with_layers(data, &[], 0.0)
}

/// 同 [`parse`]，但按 `animationlayers` 摆姿态（的"让 puppet 动起来"）：
///
/// * `layers` 里 `additive == false` 的层**替换**参考姿态（OWE `PuppetLayer::genFrame`
/// 的 `trans = trans*(1-blend) + frame*blend` 分支）；
///
/// * `additive == true` 的层在参考姿态上**叠加**相对该动画第 0 帧的增量
/// （`trans += blend*Δ`、`quat *= nlerp(ident, Δquat, blend)`、`scale += blend*Δ`）——
/// 见下面 [`PuppetPose::final_mats`] 的注释（OWE 的 additive 语义与我们的取舍）。
///
/// * `t` = 动画时间（秒，与 `g_Time` 同一只钟）。**`t = 0` 时输出与改前逐位一致**：
/// 非加法层精确落在第 0 帧（走欧拉角直出，不经过四元数），加法层的增量按定义为零。
///
/// * 回退：调用方传空 `layers`（`WE_ANIM_POSE=0`）⇒ 退回纯参考姿态。
pub fn parse_with_layers(data: &[u8], layers: &[PoseLayer], t: f32) -> Result<PuppetMesh, String> {
    let mut mesh = parse_mesh(data)?;
    // 装配：MDLV 的 `bind` 姿态存的是**散件**（贴图布局），真正的"装配姿态"在
    // MDLSv2 的 `animation_reference` 骨矩阵里（OWE Puppet.cpp 原话：the mesh bind pose may still
    // describe the unpacked pieces, not the assembled puppet）。不套这层蒙皮，网格就停在
    // 散件布局 ⇒ 头只有一块皮肤、眉眼散落在别处。默认装配；`WE_MESH_NO_ASSEMBLE=1` 可关。
    if std::env::var("WE_MESH_NO_ASSEMBLE").is_err() {
        match parse_rig_data(data) {
            Ok(rd) => {
                // 动画块读不出来（版本/尾巴不符）**不要静默**：日志里说清原因，时间动画退化成静态。
                let anims = match parse_animations(data) {
                    Ok(a) => a,
                    Err(e) => {
                        if std::env::var("WE_MESH_DEBUG").is_ok() {
                            eprintln!("[动画] MDLA 解析失败（{e}）⇒ 不做时间动画（保持 t=0 姿态）");
                        }
                        Vec::new()
                    }
                };
                let pose = PuppetPose::new(&rd, &anims, layers);
                mesh.assembled = Some(pose.assemble(
                    &mesh.positions,
                    &mesh.blend_indices,
                    &mesh.blend_weights,
                    t,
                ));
                if std::env::var("WE_MESH_DEBUG").is_ok() {
                    eprintln!(
                        "[装配] t={t:.3}s 骨={} 动画层={:?} (id, mode, rate)={:?} ⇒ 装配 {} 顶点",
                        pose.bones(),
                        layers.iter().map(|l| l.id).collect::<Vec<_>>(),
                        pose.layer_playback(),
                        mesh.positions.len()
                    );
                }
                mesh.pose = Some(pose);
            }
            Err(e) => {
                // 骨架缺失/格式不符时**不要报错**（很多 puppet 没有骨架）——静默退回散件。
                if std::env::var("WE_MESH_DEBUG").is_ok() {
                    eprintln!("[装配] 跳过（{e}）");
                }
            }
        }
    }
    Ok(mesh)
}

/// 只解**静态网格**（顶点 / uv / 蒙皮属性 / 索引），不做装配。
fn parse_mesh(data: &[u8]) -> Result<PuppetMesh, String> {
    // ⚠ 顶点步长默认 80（参照实现只认 MDLV0021/0023 用的就是这个值），
    // 但本机模型是 **MDLV0017** ⇒ 步长未必相同。`WE_MESH_STRIDE` 可覆盖，
    // 用来一次编译扫多个候选、看哪个能把三角连成片（碎片 = 步长错）。
    let stride: usize = std::env::var("WE_MESH_STRIDE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(VERTEX_STRIDE);
    if data.len() < MARKER_SIZE + MESH_HEADER_SIZE {
        return Err("文件太小".into());
    }
    let version = String::from_utf8_lossy(&data[..MARKER_SIZE])
        .trim_end_matches('\0')
        .to_string();
    if !version.starts_with("MDLV") {
        return Err(format!("不是 MDLV（头 = {version:?}）"));
    }
    let mdls = find_mdls(data);
    let limit = mdls.min(data.len());

    // 启发式扫网格块：顶点字节数 %80==0、索引字节数 %6==0（与参照同法）
    let mut found: Option<(usize, u32, u32)> = None;
    for off in MARKER_SIZE..limit.saturating_sub(MESH_HEADER_SIZE) {
        let Some(vb) = u32_at(data, off + 4) else { break };
        if vb == 0 || vb as usize % stride != 0 {
            continue;
        }
        let vertices_offset = off + MESH_HEADER_SIZE;
        let index_len_offset = vertices_offset + vb as usize;
        if index_len_offset + 4 > limit {
            continue;
        }
        let Some(ib) = u32_at(data, index_len_offset) else { continue };
        if ib == 0 || ib % 6 != 0 {
            continue;
        }
        let indices_offset = index_len_offset + 4;
        if indices_offset + ib as usize > limit {
            continue;
        }
        found = Some((off, vb, ib));
        break;
    }
    let (header_off, vertex_bytes, index_bytes) = found.ok_or("没找到可用的网格块")?;

    let vertex_count = vertex_bytes as usize / stride;
    let vertices_offset = header_off + MESH_HEADER_SIZE;
    let indices_offset = vertices_offset + vertex_bytes as usize + 4;
    let index_count = index_bytes as usize / 2;
    // 顶点里有两套 uv（对应 WE 的 `a_TexCoordVec4`: xy=albedo, zw=base）——
    // 真实场景：`puppettexturechannels.vert` 里
    // v_TexCoordBlend.xy = a_TexCoordVec4.xy; （albedo 主 uv）
    // v_TexCoordBase.xy = a_TexCoordVec4.zw * (res.zw/res.xy); （base uv，按分辨率比缩放）
    // 我们原本固定读 stride 内的 @72 ⇒ 若那其实是 *base* 那套，就会采到"空白皮肤"（= 头没有脸）。
    // `WE_MESH_UV_AT` 可切换读取位置（默认 72）；`WE_MESH_UV_AT=52` 试另一套。
    let uv_at: usize = std::env::var("WE_MESH_UV_AT")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|v| *v + 8 <= stride)
        .unwrap_or(72);

    let mut mesh = PuppetMesh {
        version,
        positions: Vec::with_capacity(vertex_count),
        assembled: None,
        uvs: Vec::with_capacity(vertex_count),
        blend: Vec::with_capacity(vertex_count),
        blend_indices: Vec::with_capacity(vertex_count),
        blend_weights: Vec::with_capacity(vertex_count),
        indices: Vec::with_capacity(index_count),
        pose: None,
    };
    for i in 0..vertex_count {
        let vo = vertices_offset + i * stride;
        let (Some(x), Some(y), Some(z)) = (
            f32_at(data, vo),
            f32_at(data, vo + 4),
            f32_at(data, vo + 8),
        ) else {
            return Err("顶点数据被截断".into());
        };
        let (Some(u), Some(v)) = (f32_at(data, vo + uv_at), f32_at(data, vo + uv_at + 4))
        else {
            return Err("uv 被截断".into());
        };
        mesh.positions.push([x, y, z]);
        // uv 轴交换开关：贴图里脸在 u 0.25~0.50 × v 0.00~0.75 那条竖带
        // （眉毛 r2c1、眼珠 r3c1 都看得出），而网格 uv 全域覆盖它却渲不出脸
        // ⇒ 最后一根假设：u/v 读反了。`WE_MESH_UV_SWAP=1` 交换。
        let (u, v) = if std::env::var("WE_MESH_UV_SWAP").is_ok() {
            (v, u)
        } else {
            (u, v)
        };
        // uv 垂直翻转开关：rt000 底图里"脸板"的眉眼暗痕贴在**下缘**，
        // 而一张脸的眼睛应在上半 ⇒ 疑为 V 方向反了（此前只扫过水平位移 du，从没试过翻 V）。
        // `WE_MESH_UV_FLIPV=1` 启用。
        let (u, v) = if std::env::var("WE_MESH_UV_FLIPV").is_ok() {
            (u, 1.0 - v)
        } else {
            (u, v)
        };
        mesh.uvs.push([u, v]);
        // 顶点的 4 骨蒙皮属性（MDLV0017 实测布局，与 OWE `MdlParser::ParseMesh` 一致）：
        // @40 = 4×u32 骨骼索引；@56 = 4×f32 权重（和恒 ≈1）。以前只读了 @40 的第一个、
        // 且**完全没用** ⇒ 网格停在"散件"姿态 ⇒ 头只剩一块皮肤。
        let bi = [
            u32_at(data, vo + BLEND_OFFSET).unwrap_or(0),
            u32_at(data, vo + BLEND_OFFSET + 4).unwrap_or(0),
            u32_at(data, vo + BLEND_OFFSET + 8).unwrap_or(0),
            u32_at(data, vo + BLEND_OFFSET + 12).unwrap_or(0),
        ];
        let bw = [
            f32_at(data, vo + WEIGHT_OFFSET).unwrap_or(0.0),
            f32_at(data, vo + WEIGHT_OFFSET + 4).unwrap_or(0.0),
            f32_at(data, vo + WEIGHT_OFFSET + 8).unwrap_or(0.0),
            f32_at(data, vo + WEIGHT_OFFSET + 12).unwrap_or(0.0),
        ];
        mesh.blend.push(bi[0]);
        mesh.blend_indices.push(bi);
        mesh.blend_weights.push(bw);
    }
    for i in 0..index_count {
        let o = indices_offset + i * 2;
        if o + 2 > data.len() {
            return Err("索引被截断".into());
        }
        let idx = u16::from_le_bytes([data[o], data[o + 1]]);
        if idx as usize >= vertex_count {
            return Err(format!("索引越界 {idx} >= {vertex_count}").into());
        }
        mesh.indices.push(idx);
    }
    // 诊断开关 `WE_MESH_ONLY_FACE=1`：只保留"脸那块补丁"的三角形。
    // 脸在贴图上的 uv 区约 u∈[0.20,0.30] × v∈[0.45,0.60]（实测 263 个顶点、位置聚在
    // x∈[−761,−521] y∈[−146,9]）。用途：判断"脸看不见"是**被盖住**还是**根本没画**。
    // `WE_MESH_FACE_UV="u0,u1,v0,v1"` 可改区域。
    if std::env::var("WE_MESH_ONLY_FACE").is_ok() {
        let (u0, u1, v0, v1) = match std::env::var("WE_MESH_FACE_UV")
            .ok()
            .and_then(|s| {
                let n: Vec<f32> = s.split(',').filter_map(|t| t.trim().parse().ok()).collect();
                (n.len() == 4).then(|| (n[0], n[1], n[2], n[3]))
            }) {
            Some(r) => r,
            None => (0.20, 0.30, 0.45, 0.60),
        };
        let keep: Vec<bool> = mesh
            .uvs
            .iter()
            .map(|q| q[0] >= u0 && q[0] <= u1 && q[1] >= v0 && q[1] <= v1)
            .collect();
        let kept_verts = keep.iter().filter(|b| **b).count();
        let mut out: Vec<u16> = Vec::new();
        let mut tri = 0;
        for t in mesh.indices.chunks_exact(3) {
            if keep[t[0] as usize] && keep[t[1] as usize] && keep[t[2] as usize] {
                out.extend_from_slice(t);
                tri += 1;
            }
        }
        eprintln!(
            "[脸补丁] 只保留 uv∈[{u0},{u1}]×[{v0},{v1}] 的三角形：顶点 {kept_verts}/{vertex_count}，三角 {tri}/{}",
            mesh.indices.len() / 3
        );
        mesh.indices = out;
    }
    // 遮挡实验开关 `WE_MESH_FACE_LAST=1`：
    // 材质 depthtest=disabled ⇒ **后画的盖先画的**。把"脸补丁"的三角形排到索引最后，
    // 若脸出现在头上 ⇒ 确认是**绘制顺序/遮挡**问题（而不是几何/采样）。
    // 区域默认 u∈[0.20,0.30]×v∈[0.45,0.60]（= 实测那 263 个顶点的 uv 区）。
    if std::env::var("WE_MESH_FACE_LAST").is_ok() {
        let (u0, u1, v0, v1) = (0.20f32, 0.30f32, 0.45f32, 0.60f32);
        let is_face: Vec<bool> = mesh
            .uvs
            .iter()
            .map(|q| q[0] >= u0 && q[0] <= u1 && q[1] >= v0 && q[1] <= v1)
            .collect();
        let (mut rest, mut faces): (Vec<u16>, Vec<u16>) = (Vec::new(), Vec::new());
        for t in mesh.indices.chunks_exact(3) {
            if is_face[t[0] as usize] && is_face[t[1] as usize] && is_face[t[2] as usize] {
                faces.extend_from_slice(t);
            } else {
                rest.extend_from_slice(t);
            }
        }
        eprintln!(
            "[脸置后] 补丁三角 {} 个排到最后（其余 {} 个）",
            faces.len() / 3,
            rest.len() / 3
        );
        rest.extend_from_slice(&faces);
        mesh.indices = rest;
    }
    // 装配统一在 [`parse_with_layers`] 里做（这里只出静态网格）。
    Ok(mesh)
}

/// 装配用的骨架：逐骨 `final = Rw·Wb⁻¹`（列向量约定）。
///
/// * `Wb` = 沿 `parent` 复合 `local_bind`（bind 世界矩阵，把散件摆回原位 ⇒ 恒等）
///
/// * `Rw` = 沿 `parent` 复合 `animation_reference`（MDLSv2 的"装配姿态"）
/// 顶点 `p' = Σ w_k · final[idx_k] · p`。
#[derive(Debug, Clone, Default)]
pub struct Rig {
    pub bones: usize,
    pub final_mats: Vec<[f32; 16]>,
}

impl Rig {
    /// 对单个顶点做 4 骨线性混合蒙皮。
    pub fn skin(&self, p: [f32; 3], idx: [u32; 4], w: [f32; 4]) -> [f32; 3] {
        let mut acc = [0.0f32; 3];
        let mut wsum = 0.0f32;
        for k in 0..4 {
            let wk = w[k];
            if wk <= 0.0 {
                continue;
            }
            let i = idx[k] as usize;
            if i >= self.final_mats.len() {
                continue;
            }
            let m = &self.final_mats[i];
            acc[0] += (m[0] * p[0] + m[4] * p[1] + m[8] * p[2] + m[12]) * wk;
            acc[1] += (m[1] * p[0] + m[5] * p[1] + m[9] * p[2] + m[13]) * wk;
            acc[2] += (m[2] * p[0] + m[6] * p[1] + m[10] * p[2] + m[14]) * wk;
            wsum += wk;
        }
        if wsum.abs() < 1e-8 {
            p
        } else {
            [acc[0] / wsum, acc[1] / wsum, acc[2] / wsum]
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 动画（MDLA）：按时间采样 + 装配姿态（"让网格人物动起来"）
// 帧布局**照参照实现核过**（`open-wallpaper-engine/.../Puppet/MdlParser.cpp`
// `ParseAnimation`）：每骨轨道 = `i32 unk` + `u32 字节数` + N 帧 × 36 字节
// （`3×f32 位置 + 3×f32 欧拉角 + 3×f32 缩放`）。本机模型全部 **MDLA0004**：
// 骨轨道之后还有 `trans_flag`、`bone curves`、`has_v4_events`、事件表、记录对齐 ——
// 顺序走完这些尾巴才能定位**第 2 个及以后的动画**（`tou` 有 2 个：239/204）。
// 本机 6 个模型的 5 个只有 1 个动画（id 465/453/368/246/251，fps=30、length=180 ⇒ 181 帧），
// `tou` 的是 239（length=180）+ 204（length=360）。
// ═══════════════════════════════════════════════════════════════════════════

/// 一帧的局部 TRS（欧拉角顺序同 OWE `ToQuaternion`：`Rz·Ry·Rx`）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Trs {
    pub pos: [f32; 3],
    pub ang: [f32; 3],
    pub scl: [f32; 3],
}

/// 一次动画采样的结果。
#[derive(Debug, Clone, Copy)]
pub struct Sampled {
    pub pos: [f32; 3],
    pub scl: [f32; 3],
    /// 旋转的四元数 `[x, y, z, w]`
    pub quat: [f32; 4],
    /// 落在哪一帧（两帧之间的插值取左帧）
    pub idx: usize,
    /// 帧内插值系数；`0.0` ⇒ 正好落在 `idx` 帧上
    pub frac: f32,
    /// 正好落在某一帧上（`frac == 0`）——此时 `pos/scl/ang` 逐位等于该帧的原始 f32
    pub exact: bool,
    /// 该帧的欧拉角（`exact` 时用来走"欧拉直出"的老路径 ⇒ 与改前逐位一致）
    pub ang: [f32; 3],
}

/// 一根骨的动画轨道（`unk & 1 != 0` 表示该骨没有变换采样）。
#[derive(Debug, Clone, Default)]
pub struct AnimTrack {
    pub unk: i32,
    pub frames: Vec<Trs>,
}

impl AnimTrack {
    pub fn has_samples(&self) -> bool {
        (self.unk & 1) == 0 && !self.frames.is_empty()
    }
}

/// 一个 MDLA 动画。
#[derive(Debug, Clone, Default)]
pub struct Animation {
    pub id: i64,
    pub name: String,
    pub mode: String,
    pub fps: f32,
    /// 区间数（帧数 = `length + 1`）
    pub length: i32,
    pub flags: u32,
    /// 逐骨的 TRS 轨道（下标 = 骨序号）
    pub tracks: Vec<AnimTrack>,
}

/// MDLA 动画的播放模式（OWE `Puppet::PlayMode`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayMode {
    #[default]
    Loop,
    /// 往返：`0,1,…,length,length-1,…,0`（周期 = 2×时长）
    Mirror,
    /// 单次：播完停在末帧
    Single,
}

impl PlayMode {
    /// 大小写不敏感；空/未知/非法 ⇒ `Loop`（OWE `ToPlayMode` 的默认分支）。
    pub fn from_str(s: &str) -> PlayMode {
        let t = s.trim();
        if t.eq_ignore_ascii_case("mirror") {
            PlayMode::Mirror
        } else if t.eq_ignore_ascii_case("single") {
            PlayMode::Single
        } else {
            PlayMode::Loop
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            PlayMode::Loop => "loop",
            PlayMode::Mirror => "mirror",
            PlayMode::Single => "single",
        }
    }
}

impl Animation {
    /// **播放模式**：MDLA 动画头里的一个字符串（`"loop"` / `"mirror"` / `"single"`；
    /// 空或未知 ⇒ `loop`）。依据 = OWE `MdlParser.cpp::ToPlayMode`
    /// （`loop`/空 → Loop、`mirror` → Mirror、`single` → Single，其它报错后回落 Loop）。
///
    /// ⚠ 这条**不是** `scene.json` 的 `animationlayers[].*`（那里只有
    /// `{additive, animation, blend, blendin, blendout, blendtime, id, name, rate, visible}`，
    /// **没有 mode**，实测 Ice Princess 全部 15 条都没有）—— mode 属于**动画文件本身**
    /// （MDLA），所以只有解析 MDLA 才能拿到。
    pub fn play_mode(&self) -> PlayMode {
        PlayMode::from_str(&self.mode)
    }

    pub fn duration(&self) -> f32 {
        if self.fps > 0.0 && self.length > 0 {
            self.length as f32 / self.fps
        } else {
            0.0
        }
    }

    /// 该骨的第 0 帧（"加法层"的增量基准）。
    pub fn ref_frame(&self, bone: usize) -> Option<Trs> {
        let t = self.tracks.get(bone)?;
        if !t.has_samples() {
            return None;
        }
        Some(t.frames[0])
    }

    /// 按**播放位置** `position`（秒，= `动画时间 × rate`）采样该骨。
///
    /// 帧号映射**照参照实现**（OWE `Puppet.cpp::Animation::getInterpolationInfo`：
    /// `Loop` / `Single` / `Mirror` 三条分支）：
///
    /// * `Loop`（默认，空/未知模式也走它）：`cur = position % 时长`，
    /// `frame_a = ⌊cur·fps⌋ mod length`、`frame_b = frame_a + 1`（第 `length` 帧是**闭环帧**）；
///
    /// * `Mirror`：周期 = **2×时长**，帧下标走**三角波** `0,1,…,length,length-1,…,0`
    /// （`_get_frame(f) = f ≤ length ? f : 2·length − f`）⇒ **端点连续**（不存在回跳）；
///
    /// * `Single`：`cur` 夹到 `[0, 时长]`，播完**停在末帧**、不绕回。
///
    /// 位置/缩放线性插值、旋转**最短路径 nlerp**（OWE `NlerpShortest`）。
    /// 正好落在某一帧上时**不做任何算术**（直接返回该帧的原始 f32）⇒ 逐位可复现。
    pub fn sample(&self, bone: usize, position: f32) -> Option<Sampled> {
        self.sample_mode(bone, position, None)
    }

    /// 同 [`Animation::sample`]，但可**强制**播放模式（`Some("loop")` = 不实现 mode 的旧行为，
    /// 给 `WE_ANIM_MODE=legacy` / 单测做 A/B 用）。
    pub fn sample_mode(
        &self,
        bone: usize,
        position: f32,
        mode_override: Option<&str>,
    ) -> Option<Sampled> {
        let tr = self.tracks.get(bone)?;
        if !tr.has_samples() {
            return None;
        }
        let n = tr.frames.len();
        let len = self.length.max(0) as usize; // 区间数；帧下标 0..=len
        let fps = if self.fps > 0.0 { self.fps } else { 30.0 };
        let dur = self.duration();
        let mode = match mode_override {
            Some(m) => PlayMode::from_str(m),
            None => self.play_mode(),
        };
        let cycle = match mode {
            PlayMode::Mirror => dur * 2.0,
            _ => dur,
        };
        // `position ≤ 0` 时恒为 0（`t = 0` 逐位不变是构造性保证）。
        // ⚠ `single` **不取模**（OWE 的 `genSingleInterpolationInfo` 直接把 `cur` 夹到 [0, 时长]），
        // loop / mirror 才取模。
        let cur = match mode {
            PlayMode::Single => position.max(0.0),
            _ => {
                if position > 0.0 && cycle > 0.0 {
                    position % cycle
                } else {
                    0.0
                }
            }
        };
        let rate = cur * fps;
        let fi = rate.floor();
        let mut frac = rate - fi;
        let (a, b) = match mode {
            PlayMode::Mirror => {
                // 三角波下标映射（OWE）：2·len 个区间，f ≤ len 走正向、否则镜像回来
                let half = len.max(1);
                let ia = (fi as i64).rem_euclid(2 * half as i64) as usize;
                let map = |f: usize| if f <= half { f } else { 2 * half - f };
                (
                    map(ia).min(n - 1),
                    map(ia + 1).min(n - 1),
                )
            }
            PlayMode::Single => {
                let fr = fi.max(0.0) as usize;
                if fr >= len {
                    frac = 1.0;
                    (len.saturating_sub(1).min(n - 1), len.min(n - 1))
                } else {
                    (fr.min(n - 1), (fr + 1).min(n - 1))
                }
            }
            PlayMode::Loop => {
                let a0 = if len > 0 {
                    (fi as i64).rem_euclid(len as i64) as usize % n
                } else {
                    0
                };
                (a0, (a0 + 1).min(n - 1))
            }
        };
        let f = tr.frames[a];
        if frac <= 0.0 || n == 1 {
            return Some(Sampled {
                pos: f.pos,
                scl: f.scl,
                quat: quat_from_ang(f.ang),
                idx: a,
                frac: 0.0,
                exact: true,
                ang: f.ang,
            });
        }
        let g = tr.frames[b];
        let lerp = |x: f32, y: f32| x * (1.0 - frac) + y * frac;
        let mut pos = [0.0f32; 3];
        let mut scl = [1.0f32; 3];
        for k in 0..3 {
            pos[k] = lerp(f.pos[k], g.pos[k]);
            scl[k] = lerp(f.scl[k], g.scl[k]);
        }
        Some(Sampled {
            pos,
            scl,
            quat: quat_nlerp_shortest(quat_from_ang(f.ang), quat_from_ang(g.ang), frac),
            idx: a,
            frac,
            exact: false,
            ang: f.ang,
        })
    }
}

// ── 四元数（`[x, y, z, w]`，与 OWE 的 Eigen 一致：`q = qz·qy·qx`）────────────

fn quat_mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [
        a[3] * b[0] + a[0] * b[3] + a[1] * b[2] - a[2] * b[1],
        a[3] * b[1] - a[0] * b[2] + a[1] * b[3] + a[2] * b[0],
        a[3] * b[2] + a[0] * b[1] - a[1] * b[0] + a[2] * b[3],
        a[3] * b[3] - a[0] * b[0] - a[1] * b[1] - a[2] * b[2],
    ]
}

fn quat_conj(q: [f32; 4]) -> [f32; 4] {
    [-q[0], -q[1], -q[2], q[3]]
}

/// 欧拉角 → 四元数（与 [`trs_col`] 的 `Rz·Ry·Rx` 完全一致；有单测钉死）。
pub fn quat_from_ang(a: [f32; 3]) -> [f32; 4] {
    let half = 0.5;
    let (sx, cx) = (a[0] * half).sin_cos();
    let (sy, cy) = (a[1] * half).sin_cos();
    let (sz, cz) = (a[2] * half).sin_cos();
    let qx = [sx, 0.0, 0.0, cx];
    let qy = [0.0, sy, 0.0, cy];
    let qz = [0.0, 0.0, sz, cz];
    quat_mul(quat_mul(qz, qy), qx)
}

/// 四元数的旋转角（度）—— 诊断用。
pub fn quat_angle_deg(q: [f32; 4]) -> f32 {
    let n = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    if n <= 0.0 {
        return 0.0;
    }
    let w = (q[3] / n).clamp(-1.0, 1.0);
    2.0 * w.acos().to_degrees()
}

/// 最短路径 nlerp（同 OWE `NlerpShortest`）。
pub fn quat_nlerp_shortest(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    let dot = a[0] * b[0] + a[1] * b[1] + a[2] * b[2] + a[3] * b[3];
    let st = if dot < 0.0 { -t } else { t };
    let mut q = [0.0f32; 4];
    for k in 0..4 {
        q[k] = a[k] * (1.0 - t) + b[k] * st;
    }
    let n = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    if n > 1e-12 {
        for k in 0..4 {
            q[k] /= n;
        }
    }
    q
}

/// 四元数 + 平移/缩放 → **列主序** 4×4（旋转部分与 [`trs_col`] 同一约定）。
pub fn trs_quat_col(q: [f32; 4], pos: [f32; 3], scl: [f32; 3]) -> [f32; 16] {
    let (x, y, z, w) = (q[0], q[1], q[2], q[3]);
    // 行主序的 3×3 旋转（列向量约定）
    let r = [
        [
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y - z * w),
            2.0 * (x * z + y * w),
        ],
        [
            2.0 * (x * y + z * w),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z - x * w),
        ],
        [
            2.0 * (x * z - y * w),
            2.0 * (y * z + x * w),
            1.0 - 2.0 * (x * x + y * y),
        ],
    ];
    let mut m = [0.0f32; 16];
    for c in 0..3 {
        for row in 0..3 {
            m[c * 4 + row] = r[row][c] * scl[c];
        }
    }
    m[12] = pos[0];
    m[13] = pos[1];
    m[14] = pos[2];
    m[15] = 1.0;
    m
}

/// 旋转矩阵（行主序 3×3）→ 四元数（Shepperd 法）。
fn quat_from_mat3(r: &[[f32; 3]; 3]) -> [f32; 4] {
    let tr = r[0][0] + r[1][1] + r[2][2];
    let q = if tr > 0.0 {
        let s = ((tr + 1.0).max(0.0)).sqrt() * 2.0;
        [
            (r[2][1] - r[1][2]) / s,
            (r[0][2] - r[2][0]) / s,
            (r[1][0] - r[0][1]) / s,
            0.25 * s,
        ]
    } else if r[0][0] > r[1][1] && r[0][0] > r[2][2] {
        let s = ((1.0 + r[0][0] - r[1][1] - r[2][2]).max(0.0)).sqrt() * 2.0;
        [
            0.25 * s,
            (r[0][1] + r[1][0]) / s,
            (r[0][2] + r[2][0]) / s,
            (r[2][1] - r[1][2]) / s,
        ]
    } else if r[1][1] > r[2][2] {
        let s = ((1.0 + r[1][1] - r[0][0] - r[2][2]).max(0.0)).sqrt() * 2.0;
        [
            (r[0][1] + r[1][0]) / s,
            0.25 * s,
            (r[1][2] + r[2][1]) / s,
            (r[0][2] - r[2][0]) / s,
        ]
    } else {
        let s = ((1.0 + r[2][2] - r[0][0] - r[1][1]).max(0.0)).sqrt() * 2.0;
        [
            (r[0][2] + r[2][0]) / s,
            (r[1][2] + r[2][1]) / s,
            0.25 * s,
            (r[1][0] - r[0][1]) / s,
        ]
    };
    let n = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    if n > 1e-12 {
        [q[0] / n, q[1] / n, q[2] / n, q[3] / n]
    } else {
        [0.0, 0.0, 0.0, 1.0]
    }
}

/// 列主序仿射矩阵 → (平移, 四元数, 缩放)。用于"加法层"在参考姿态上叠加增量。
fn decompose_col(m: &[f32; 16]) -> ([f32; 3], [f32; 4], [f32; 3]) {
    let mut cols = [[0.0f32; 3]; 3];
    for c in 0..3 {
        for r in 0..3 {
            cols[c][r] = m[c * 4 + r];
        }
    }
    let mut s = [1.0f32; 3];
    for c in 0..3 {
        let n = (cols[c][0] * cols[c][0] + cols[c][1] * cols[c][1] + cols[c][2] * cols[c][2]).sqrt();
        if n > 1e-6 {
            s[c] = n;
            for r in 0..3 {
                cols[c][r] /= n;
            }
        }
    }
    // 行列式 < 0（镜像）⇒ 把 X 轴取反，保证得到合法旋转
    let det = cols[0][0] * (cols[1][1] * cols[2][2] - cols[1][2] * cols[2][1])
        - cols[1][0] * (cols[0][1] * cols[2][2] - cols[0][2] * cols[2][1])
        + cols[2][0] * (cols[0][1] * cols[1][2] - cols[0][2] * cols[1][1]);
    if det < 0.0 {
        for r in 0..3 {
            cols[0][r] = -cols[0][r];
        }
        s[0] = -s[0];
    }
    let mut r = [[0.0f32; 3]; 3];
    for c in 0..3 {
        for row in 0..3 {
            r[row][c] = cols[c][row];
        }
    }
    ([m[12], m[13], m[14]], quat_from_mat3(&r), s)
}

// ── MDLA 顺序走查（定长帧 + 变长尾巴）────────────────────────────────────────

struct Cur<'a> {
    d: &'a [u8],
    o: usize,
    end: usize,
}

impl<'a> Cur<'a> {
    fn need(&self, n: usize) -> Result<(), String> {
        if self.o + n > self.end.min(self.d.len()) {
            return Err(format!("读越界：offset={} 需要 {} 字节（段尾 {}）", self.o, n, self.end));
        }
        Ok(())
    }
    fn u8(&mut self) -> Result<u8, String> {
        self.need(1)?;
        let v = self.d[self.o];
        self.o += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16, String> {
        self.need(2)?;
        let v = u16::from_le_bytes([self.d[self.o], self.d[self.o + 1]]);
        self.o += 2;
        Ok(v)
    }
    fn u32(&mut self) -> Result<u32, String> {
        self.need(4)?;
        let v = u32_at(self.d, self.o).ok_or("读 u32 失败")?;
        self.o += 4;
        Ok(v)
    }
    fn peek_u32_at(&self, rel: usize) -> u32 {
        u32_at(self.d, self.o + rel).unwrap_or(u32::MAX)
    }
    fn peek_u8_at(&self, rel: usize) -> u8 {
        *self.d.get(self.o + rel).unwrap_or(&0)
    }
    fn i32(&mut self) -> Result<i32, String> {
        Ok(self.u32()? as i32)
    }
    fn f32(&mut self) -> Result<f32, String> {
        Ok(f32::from_bits(self.u32()?))
    }
    fn skip(&mut self, n: usize) -> Result<(), String> {
        self.need(n)?;
        self.o += n;
        Ok(())
    }
    fn cstr(&mut self) -> Result<String, String> {
        let (s, next) = cstr_at(self.d, self.o).ok_or_else(|| format!("字符串被截断 @{}", self.o))?;
        if next > self.end {
            return Err(format!("字符串越过段尾 @{}", self.o));
        }
        self.o = next;
        Ok(s)
    }
}

/// 解析整个 `MDLA` 块的全部动画（顺序走查，尾巴照参照实现核过）。
pub fn parse_animations(data: &[u8]) -> Result<Vec<Animation>, String> {
    let at = data
        .windows(4)
        .position(|w| w == b"MDLA")
        .ok_or("没有 MDLA 动画块")?;
    let ver: i32 = if at + 8 <= data.len() {
        String::from_utf8_lossy(&data[at + 4..at + 8])
            .trim()
            .parse()
            .unwrap_or(0)
    } else {
        0
    };
    let mut c = Cur { d: data, o: at + 9, end: data.len() };
    let end_off = c.u32()? as usize;
    let anims = c.u32()? as usize;
    if anims > 512 {
        return Err(format!("动画数不合理：{anims}"));
    }
    let limit = if end_off == 0 { data.len() } else { end_off.min(data.len()) };
    c.end = limit;
    let mut out = Vec::with_capacity(anims);
    for i in 0..anims {
        let a = parse_one_anim(&mut c, ver, limit)
            .map_err(|e| format!("MDLA 第 {} 个动画（ver={ver}）：{e}", i + 1))?;
        out.push(a);
    }
    Ok(out)
}

fn parse_one_anim(c: &mut Cur, ver: i32, limit: usize) -> Result<Animation, String> {
    let id = c.i32()? as i64;
    let _unk_after_id = c.u32()?;
    let mut name = c.cstr()?;
    if name.is_empty() {
        name = c.cstr()?;
    }
    let mode = c.cstr()?;
    let fps = c.f32()?;
    let length = c.i32()?;
    let flags = c.u32()?;
    let b_num = c.u32()? as usize;
    if b_num > 4096 {
        return Err(format!("骨轨道数不合理：{b_num}"));
    }
    let mut tracks = Vec::with_capacity(b_num);
    for _ in 0..b_num {
        let unk = c.i32()?;
        let bytes = c.u32()? as usize;
        if bytes % 36 != 0 {
            return Err(format!("骨轨道字节数 {bytes} 不是 36 的整数倍"));
        }
        let n = bytes / 36;
        let mut frames = Vec::with_capacity(n);
        for _ in 0..n {
            let o = c.o;
            c.need(36)?;
            let take = |rel: usize| -> f32 { f32::from_bits(u32_at(c.d, o + rel).unwrap_or(0)) };
            frames.push(Trs {
                pos: [take(0), take(4), take(8)],
                ang: [take(12), take(16), take(20)],
                scl: [take(24), take(28), take(32)],
            });
            c.o = o + 36;
        }
        tracks.push(AnimTrack { unk, frames });
    }
    // MDLA >= 3 的尾巴：trans_flag（+ 变长 trans 轨道）+ 逐骨曲线
    if ver >= 3 {
        let tf = c.u32()?;
        if tf == 1 {
            let extra = c.u32()? as usize;
            if extra > 0 {
                if extra % 4 != 0 {
                    return Err(format!("trans extra_size={extra} 不是 4 的倍数"));
                }
                c.skip(extra)?;
                c.skip(4)?;
            }
            let main = c.u32()? as usize;
            if main % 4 != 0 {
                return Err(format!("trans main_size={main} 不是 4 的倍数"));
            }
            c.skip(main)?;
            if extra > 0 {
                c.skip(4)?;
            }
        } else if tf == 0 {
            let samples = length.max(0) as u64 + 1;
            let is_main = |sz: u32| sz as u64 == samples * 36 || sz as u64 == samples * 4;
            if is_main(c.peek_u32_at(0)) {
                let sz = c.u32()? as usize;
                c.skip(sz)?;
                while c.peek_u32_at(0) == 0 && is_main(c.peek_u32_at(4)) {
                    c.skip(4)?;
                    let sz = c.u32()? as usize;
                    c.skip(sz)?;
                }
            }
        } else {
            return Err(format!("trans_flag 应为 0/1，读到 {tf}"));
        }
        // ParseAnimBoneCurves：u8 has_curves + 逐骨 (u32 zero + u32 字节数 + 数据)
        let has = c.u8()?;
        if has != 0 {
            for _ in 0..b_num {
                let _zero = c.u32()?;
                let bs = c.u32()? as usize;
                if bs % 4 != 0 {
                    return Err(format!("BoneFrameCurve 字节数 {bs} 不是 4 的倍数"));
                }
                c.skip(bs)?;
            }
        }
    }
    // MDLA >= 4：定时形变事件
    if ver >= 4 {
        let hv = c.u8()?;
        if hv == 1 {
            let cnt = c.u32()? as usize;
            for _ in 0..cnt {
                c.skip(4)?; // time
                let cc = c.u16()? as usize;
                c.skip(2)?; // flags
                if cc == 0 {
                    return Err("AnimV4Event 的 curve_count 为 0".into());
                }
                for ci in 0..cc {
                    if ci != 0 {
                        c.skip(2)?;
                    }
                    let bs = c.u32()? as usize;
                    if bs % 4 != 0 {
                        return Err(format!("AnimV4Curve 字节数 {bs} 不是 4 的倍数"));
                    }
                    c.skip(bs)?;
                }
            }
        } else if hv != 0 {
            return Err(format!("has_v4_events 应为 0/1，读到 {hv}"));
        }
    }
    // MDLA >= 5：动画包围盒（3+3 f32）
    if ver >= 5 {
        c.skip(24)?;
    }
    // MDLA == 6（本机 `3704273480` 的 `身体---拆分` 就是 0023/0006）：可选的**标量曲线**组
    // （每骨 draw order 之类），布局与 v3 的 blend curves 相同。
    // 参照 `next_is_anim_bone_curves`：只有 `has==0` 或（`off+1` 处为 0 且 `off+5` 处字节数是 4 的倍数）
    // 时才把它当作曲线块**吃掉**；否则这一块根本不存在。
    if ver == 6 {
        let has = c.peek_u8_at(0);
        let take = has == 0 || (c.peek_u32_at(1) == 0 && c.peek_u32_at(5) % 4 == 0);
        if take {
            let has = c.u8()?;
            if has != 0 {
                for _ in 0..b_num {
                    let _zero = c.u32()?;
                    let bs = c.u32()? as usize;
                    if bs % 4 != 0 {
                        return Err(format!("v6 标量曲线字节数 {bs} 不是 4 的倍数"));
                    }
                    c.skip(bs)?;
                }
            }
        }
    }
    // source clip（flags bit0）
    if flags & 1 != 0 {
        if c.o + 18 > limit {
            return Err("source clip 越过 MDLA 段尾".into());
        }
        c.skip(18)?;
    }
    // 事件表：u32 数量 + 逐事件 (u32 时间 + JSON 字符串)
    let cnt = c.u32()? as usize;
    let remain = limit.saturating_sub(c.o);
    if cnt as u64 > remain as u64 / 5 {
        return Err(format!("事件数 {cnt} 不合理（剩余 {remain} 字节）"));
    }
    for _ in 0..cnt {
        c.skip(4)?;
        let _ = c.cstr()?;
    }
    // 记录之间的对齐：一个 u32 零 + 下一个动画的 id
    if limit > 0 && c.o + 12 <= limit && c.peek_u32_at(0) == 0 {
        let nid = c.peek_u32_at(4);
        if nid > 0 && nid <= 100_000 && c.peek_u32_at(8) == 0 {
            c.skip(4)?;
        }
    }
    Ok(Animation { id, name, mode, fps, length, flags, tracks })
}

/// 一个 `animationlayers` 条目（上层从 scene.json 传进来）。
#[derive(Debug, Clone, Copy)]
pub struct PoseLayer {
    /// 动画 id（MDLA 的 `id`）
    pub id: i64,
    /// 加法层（`tou` 的 239/204、B 身体层的全部）
    pub additive: bool,
    /// 混合权重（实测都是 1.0）
    pub blend: f32,
    /// **播放速率**（`animationlayers[].rate`，WE 的默认 1.0）：
    /// 播放位置 `position = t · rate`（秒），所以**周期 = 片长 / rate**。
    /// 依据（OWE `Puppet.cpp::prepared`）：`SceneAnimationPlayback(clip, rate<=0)` + `SetRate(rate)`，
    /// `updateInterpolation` 拿的是 `playback->PositionSeconds`。以前我们**完全没读这个字段**
    /// （`rate` 只是 scene.json 里一个被忽略的键）⇒ 所有层都按 1.0 播。
    pub rate: f32,
    /// **起始帧**（`animationlayers[].visible` 脚本里的 `setFrame(...)`）。
    /// WE/OWE 语义（一手依据）：`PuppetAnimationLayer.visible` 是**字段脚本**，`init` 里
    /// `ani.setFrame(ani.frameCount * scriptProperties.percentage)`（本场景 842 = 0.80、
    /// 869 = 0.70）⇒ 该层**从片长的 80% / 70% 处开始播**，不是从第 0 帧。
    /// OWE `SceneAnimationPlayback::SetFrame(f)`：`position_seconds = clamp(f, 0, End) / fps`
    /// （`End` = MDLA 的 `length`，与 `frameCount` 同值）；之后 `play` 从那里继续。
    /// 这里存的是**帧表达式** `(coef, constant)`（单位：帧）：
    /// `start_frame = coef · End + constant` ⇒ `start_pos = clamp(start_frame,0,End)/fps`。
    /// 这样 `setFrame(frameCount*0.8)`（coef=0.8）与 `setFrame(45)`（constant=45）两种形态
    /// 都能表达，`End/fps` 由数据（MDLA 头）给，不含任何场景常数。
    /// `(0.0, 0.0)` = 从 0 帧（旧行为，逐位不变）。
    pub start: (f32, f32),
}

/// 骨架原始数据（`MDLS`）：局部 bind + 可选的"装配参考姿态"（MDLSv2）。
#[derive(Debug, Clone, Default)]
pub struct RigData {
    pub parents: Vec<i32>,
    pub local: Vec<[f32; 16]>,
    pub reference: Option<Vec<[f32; 16]>>,
}

/// 沿 `parent` 链复合世界矩阵（列向量约定）。**与装配前的实现逐位相同**（同一套归约顺序）。
fn chain_col(n: usize, parents: &[i32], mats: &[[f32; 16]]) -> Vec<[f32; 16]> {
    fn go(
        i: usize,
        parents: &[i32],
        mats: &[[f32; 16]],
        w: &mut Vec<Option<[f32; 16]>>,
    ) -> [f32; 16] {
        if let Some(m) = w[i] {
            return m;
        }
        let p = parents[i];
        let m = if p < 0 || p as usize >= parents.len() {
            mats[i]
        } else {
            mat4_mul_col(&go(p as usize, parents, mats, w), &mats[i])
        };
        w[i] = Some(m);
        m
    }
    let mut w: Vec<Option<[f32; 16]>> = vec![None; n];
    for i in 0..n {
        go(i, parents, mats, &mut w);
    }
    w.into_iter().map(|x| x.unwrap_or_default()).collect()
}

/// 时间动画的姿态数据：参考姿态 + 各动画层 ⇒ `t` ⇒ 逐骨的 `final = Rw·Wb⁻¹`。
#[derive(Debug, Clone, Default)]
pub struct PuppetPose {
    parents: Vec<i32>,
    base_mat: Vec<[f32; 16]>,
    base_pos: Vec<[f32; 3]>,
    base_quat: Vec<[f32; 4]>,
    base_scl: Vec<[f32; 3]>,
    inv_wb: Vec<[f32; 16]>,
    /// MDLS 的 **bind 局部矩阵**（= `rd.local`）——挂接的 `bind_xform` 与
    /// `genFrame(t)[k] = final·Wb` 都要用它。
    bind_local: Vec<[f32; 16]>,
    layers: Vec<LayerCfg>,
    /// `true` = 不实现播放模式（把 `mirror`/`single` 也当 `loop` 播）——
    /// 即 之前的行为，`WE_ANIM_MODE=legacy` 时才置位（A/B 用）。
    legacy_loop: bool,
    /// 加法层的增量基准（`WE_ANIM_ADDITIVE`，默认 `Auto`；见 [`AddRef`]）。
    add_ref: AddRef,
}

/// 一个播放中的动画层：动画本体 + 该层的混合参数。
#[derive(Debug, Clone, Default)]
struct LayerCfg {
    anim: Animation,
    additive: bool,
    blend: f32,
    /// `animationlayers[].rate`（播放速率，位置 = t·rate）
    rate: f32,
    /// **起始播放位置**（秒）；0 = 从第 0 帧开始（旧行为）。
    /// 来自 `animationlayers[].visible` 脚本的 `setFrame(...)`（见 `PoseLayer::start`）。
    start_pos: f32,
}

/// 加法层的**增量基准**取哪一个（`WE_ANIM_ADDITIVE` 开关；默认 `Auto`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AddRef {
    /// 默认：骨 rest 基准；仅当动画第 0 帧与 rest 恰好同姿态时沿用"第 0 帧"算式
    /// （两条算式在那种数据上逐位等价 ⇒ 老场景零变化）。
    #[default]
    Auto,
    /// `WE_ANIM_ADDITIVE=frame0`：一律用动画第 0 帧（= 之前的旧口径，回退用）。
    Frame0,
    /// `WE_ANIM_ADDITIVE=rest`：一律用骨 rest（诊断用，会移动老场景的 t>0 帧）。
    Rest,
}

impl AddRef {
    fn from_env() -> AddRef {
        match std::env::var("WE_ANIM_ADDITIVE").ok().as_deref() {
            Some("frame0") | Some("first") | Some("legacy") => AddRef::Frame0,
            Some("rest") | Some("reference") => AddRef::Rest,
            _ => AddRef::Auto,
        }
    }
}

/// 判据：某骨某动画的**第 0 帧**是不是就是该骨的 **rest 姿态**（参考/绑定姿态）。
///
/// 两条加法基准（第 0 帧 vs rest）在数据上重合时算出的增量**逐位相同** ⇒ 可以安全地
/// 沿用老算式（含"正好落在第 0 帧就不做算术"的跳过），老场景逐字节不变。
/// 阈值取 1e-2 个网格单位（模型量级是数千单位；实测"不重合"的样本差 2380 单位，
/// 而"重合"的样本差 ~1e-7）⇒ 判据有 5 个数量级余量。纯数据判据，不看 id/名字。
pub fn frame0_is_rest(f: &Trs, pos: &[f32; 3], quat: &[f32; 4], scl: &[f32; 3]) -> bool {
    const EPS: f32 = 1e-2;
    for k in 0..3 {
        if (f.pos[k] - pos[k]).abs() > EPS || (f.scl[k] - scl[k]).abs() > EPS {
            return false;
        }
    }
    let q = quat_from_ang(f.ang);
    let dot = q[0] * quat[0] + q[1] * quat[1] + q[2] * quat[2] + q[3] * quat[3];
    dot.abs() >= 1.0 - 1e-5
}

impl PuppetPose {
    pub fn bones(&self) -> usize {
        self.parents.len()
    }
    pub fn layer_ids(&self) -> Vec<(i64, bool, f32)> {
        self.layers
            .iter()
            .map(|l| (l.anim.id, l.additive, l.blend))
            .collect()
    }

    /// 逐帧改写某层的**混合权重**（`animationlayers[].blend` 脚本的输出）。
    /// 下标 = 该 puppet 的 `PoseLayer` 表顺序（`PuppetPose::new` 收到的那个顺序）。
    /// 越界或非有限值忽略（保持上一帧的值）⇒ 无脚本的层永远走不到这里。
    pub fn set_layer_blend(&mut self, i: usize, blend: f32) {
        if let Some(l) = self.layers.get_mut(i) {
            if blend.is_finite() {
                l.blend = blend;
            }
        }
    }

    /// 某层当前的混合权重（诊断/单测用）。
    pub fn layer_blend(&self, i: usize) -> Option<f32> {
        self.layers.get(i).map(|l| l.blend)
    }

    /// 逐层的 `(id, 播放模式字符串, rate)` —— 诊断/日志用（`WE_MESH_DEBUG`）。
    pub fn layer_playback(&self) -> Vec<(i64, &'static str, f32)> {
        self.layers
            .iter()
            .map(|l| (l.anim.id, l.anim.play_mode().as_str(), l.rate))
            .collect()
    }

    /// 是否走"不实现播放模式"的老行为（`WE_ANIM_MODE=legacy`）。
    pub fn legacy_loop(&self) -> bool {
        self.legacy_loop
    }

    /// 第 `i` 根骨的 **rest 姿态**（参考/绑定姿态）的 (平移, 四元数, 缩放)。
    /// 诊断用（`puppet_diag --refcheck`）；也是加法层增量基准的来源。
    pub fn rest_trs(&self, i: usize) -> ([f32; 3], [f32; 4], [f32; 3]) {
        (
            self.base_pos.get(i).copied().unwrap_or([0.0; 3]),
            self.base_quat.get(i).copied().unwrap_or([0.0, 0.0, 0.0, 1.0]),
            self.base_scl.get(i).copied().unwrap_or([1.0; 3]),
        )
    }

    /// 组装：参考姿态（`reference` 优先，否则 `local_bind` —— 与改前一致）+ 层覆盖 + 父链 + `inv_bind`。
    pub fn new(rd: &RigData, anims: &[Animation], layers: &[PoseLayer]) -> PuppetPose {
        let n = rd.parents.len();
        let base_mat: Vec<[f32; 16]> = (0..n)
            .map(|i| match &rd.reference {
                Some(r) => r[i],
                None => rd.local[i],
            })
            .collect();
        let wb = chain_col(n, &rd.parents, &rd.local);
        let mut inv_wb = Vec::with_capacity(n);
        for m in &wb {
            inv_wb.push(mat4_inv(m).unwrap_or([
                1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
            ]));
        }
        let mut base_pos = Vec::with_capacity(n);
        let mut base_quat = Vec::with_capacity(n);
        let mut base_scl = Vec::with_capacity(n);
        for m in &base_mat {
            let (p, q, s) = decompose_col(m);
            base_pos.push(p);
            base_quat.push(q);
            base_scl.push(s);
        }
        let mut ls = Vec::new();
        // 诊断旋钮 `WE_SKIP_ANIM=<id,id>`：建姿态时**跳过多条动画层**（按 MDLA 动画 id）。
        // 用途：把"人物本体在动"与"某条附属动画（例如滑板/眼镜的独立摆动）在动"分开量 ——
        // 对象级 `WE_SKIP_ID` 只能整层摘掉，摘不出"是哪一条 animationlayers 在动"。
        // 不设 ⇒ 一条都不过滤（逐字节旧行为）。
        let skip_anims: Vec<i64> = std::env::var("WE_SKIP_ANIM")
            .map(|s| s.split(',').filter_map(|x| x.trim().parse::<i64>().ok()).collect())
            .unwrap_or_default();
        for l in layers {
            if skip_anims.contains(&l.id) {
                if std::env::var("WE_MESH_DEBUG").is_ok() {
                    eprintln!("[动画] 层 id={} 被 WE_SKIP_ANIM 跳过", l.id);
                }
                continue;
            }
            match anims.iter().find(|a| a.id == l.id) {
                Some(a) => {
                    // 起始帧 → 起始播放位置（OWE `SceneAnimationPlayback::SetFrame`）：
                    // `frame = clamp(coef·End + const, 0, End)`，`pos = frame / fps`。
                    let end = a.length.max(0) as f32;
                    let fps = if a.fps > 0.0 { a.fps } else { 30.0 };
                    let f0 = (l.start.0 * end + l.start.1).clamp(0.0, end);
                    ls.push(LayerCfg {
                        anim: a.clone(),
                        additive: l.additive,
                        blend: l.blend,
                        // `rate <= 0` 时 OWE 会走"暂停"语义（`SceneAnimationPlayback(clip, true)`）；
                        // 这里退化成 1.0，避免把作者写坏的 0/负数变成"冻在 position=0"。
                        rate: if l.rate > 0.0 { l.rate } else { 1.0 },
                        start_pos: if f0 > 0.0 { f0 / fps } else { 0.0 },
                    })
                }
                None => {
                    if std::env::var("WE_MESH_DEBUG").is_ok() {
                        eprintln!("[动画] 层 id={} 在 MDLA 里找不到（跳过）", l.id);
                    }
                }
            }
        }
        // `WE_ANIM_MODE=legacy` = **不读 mode、也不读 rate**（= 之前的行为），A/B 用。
        // 默认 = 按 MDLA 里的 mode 播 + 按 `animationlayers[].rate` 推进
        // （依据：OWE `Puppet::Animation::getInterpolationInfo` / `PuppetLayer::prepared`）。
        let legacy_loop = std::env::var("WE_ANIM_MODE").ok().as_deref() == Some("legacy");
        if legacy_loop {
            for l in ls.iter_mut() {
                l.rate = 1.0;
            }
        }
        if std::env::var("WE_MESH_DEBUG").is_ok() && !ls.is_empty() {
            eprintln!(
                "[动画] 层(id, mode, rate, blend) = {:?}{}",
                ls.iter()
                    .map(|l| (
                        l.anim.id,
                        l.anim.play_mode().as_str(),
                        l.rate,
                        (l.blend * 1000.0).round() / 1000.0
                    ))
                    .collect::<Vec<_>>(),
                if legacy_loop { "（WE_ANIM_MODE=legacy：全部按 loop 播）" } else { "" }
            );
        }
        PuppetPose {
            parents: rd.parents.clone(),
            base_mat,
            base_pos,
            base_quat,
            base_scl,
            inv_wb,
            bind_local: rd.local.clone(),
            layers: ls,
            legacy_loop,
            add_ref: AddRef::from_env(),
        }
    }

    /// 逐骨的局部姿态（`t` 秒，与 `g_Time` 同一只钟）。
///
    /// 语义（照 OWE `PuppetLayer::genFrame`）：
///
    /// * **播放位置** `position = t · rate`（`rate` = 该层的 `animationlayers[].rate`）；
    /// 再按该动画自己的 `mode`（MDLA 头）取样 —— `loop` 取模、`mirror` 三角波往返、
    /// `single` 停在末帧（见 [`Animation::sample`]）。
///
    /// * `additive == false`：**替换** —— `pos/scale` 在参考姿态与帧之间按 `blend` 线性插值、
    /// 旋转按最短路径 nlerp；`blend == 1` 且正好落在帧上时**走欧拉直出**（与改前的静态装配逐位一致）。
///
    /// * `additive == true`：**增量** —— `Δ = frame(t) − <参考姿态>`，按 `blend` 叠加到参考姿态。
    /// 参考姿态照 OWE `PuppetLayer::genFrame` 的 **`use_reference_pose`** 分支取
    /// **骨自己的 rest**（`animation_reference`，无该块时即 `local_bind`）⇒
    /// `pose(t) = rest + blend·(frame(t) − rest)`（= 把这条动画"绝对"叠在 rest 上）。
    /// ⚠ 改口径（修"人物/月亮在 t ≳ 0.5s 整块消失"）：我们原来用的是 OWE 里
    /// 另一条 `additive_uses_first_frame`（= `has_world_binds == 0`）的基准 —— **动画自己的
    /// 第 0 帧**。OWE 源码自己在那一行注明它只是 "legacy first-frame compatibility path …
    /// **is not the Android reference rule**"（一条没定论的兼容分支），不是真规则。
    /// 那条基准假设"动画从 rest 起步"，遇到"第 0 帧 ≠ rest"的动画就崩：
    /// 本场景 `девка розбив`(id 27) 的 `Анимация 9`(MDLA id 1300, mode=single) 第 0 帧把
    /// **根骨**放在 (41.49, −1314.89) = rest **之外 2380 单位**（层高只有 2250），末帧才回到
    /// rest；以第 0 帧为基准时末帧姿态 = `rest + (rest − frame0)` = **向下 2380** ⇒ 人物整块被
    /// 推出图层缓冲（实测对象 27 对全帧的贡献 = **0 像素**，`WE_SKIP_ID=27` 与基线逐字节相同）。
    /// 同一形态还命中 `луна`(id 377, 动画 1282, mode=single)：t=0 在天空（非黑 101700 px），
    /// t=5 变成 **0 像素**。
    /// 两条基准在数据上**几乎总是重合**（作者把动画画在 rest 上）：重合时
    /// `frame(t) − rest ≡ frame(t) − frame0`（逐位相同）⇒ 保留老算式（含"正好落在第 0 帧
    /// 就不做任何算术"的跳过），老场景逐字节不变；只有"第 0 帧 ≠ rest"的**层/骨的采样**
    /// 才走新算式（判据见 [`frame0_is_rest`]，纯数据，不看 id/名字）。
    /// A/B 回退：`WE_ANIM_ADDITIVE=frame0`（= 改前口径）；`=rest` 强制全部走 rest 基准。
    pub fn final_mats(&self, t: f32) -> Vec<[f32; 16]> {
        let n = self.parents.len();
        // **诊断（默认关，行为不变）**：`WE_SKIN_OFF=1` ⇒ 全部骨矩阵取单位阵，
        // 即**完全不蒙皮**地渲染原始绑定顶点（用来判定「扇形发丝」是动画/蒙皮造成的，
        // 还是网格数据本身如此）。
        if std::env::var("WE_SKIN_OFF").ok().as_deref() == Some("1") {
            return (0..n)
                .map(|_| {
                    [
                        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0,
                        0.0, 1.0,
                    ]
                })
                .collect();
        }
        let mut local: Vec<[f32; 16]> = Vec::with_capacity(n);
        for i in 0..n {
            let mut touched = false;
            let mut pos = self.base_pos[i];
            let mut quat = self.base_quat[i];
            let mut scl = self.base_scl[i];
            let mut euler: Option<[f32; 3]> = None;
            for l in &self.layers {
                let (anim, additive, blend) = (&l.anim, &l.additive, &l.blend);
                if *blend <= 0.0 {
                    continue;
                }
                // 起始相位（`setFrame` 脚本）+ 播放推进；`start_pos` 缺省 0 ⇒ 与旧算式逐位相同
                let position = l.start_pos + t * l.rate;
                let mode_override = if self.legacy_loop { Some("loop") } else { None };
                let Some(s) = anim.sample_mode(i, position, mode_override) else {
                    continue;
                };
                if *additive {
                    // 参考姿态：默认取**骨自己的 rest**；仅当动画第 0 帧与 rest
                    // 恰好是同一姿态时，才沿用老的"第 0 帧"算式（逐位不变）。
                    let r0 = anim.ref_frame(i);
                    let use_frame0 = match self.add_ref {
                        AddRef::Frame0 => true,
                        AddRef::Rest => false,
                        AddRef::Auto => r0
                            .as_ref()
                            .is_some_and(|f| frame0_is_rest(f, &self.base_pos[i], &self.base_quat[i], &self.base_scl[i])),
                    };
                    if use_frame0 {
                        // 老算式（文本保持原样，保证数值路径逐位一致）——
                        if s.exact && s.idx == 0 {
                            continue;
                        }
                        let Some(r0) = r0 else { continue };
                        let b = *blend;
                        let dq = quat_mul(quat_conj(quat_from_ang(r0.ang)), s.quat);
                        quat = quat_mul(quat, quat_nlerp_shortest([0.0, 0.0, 0.0, 1.0], dq, b));
                        for k in 0..3 {
                            pos[k] += b * (s.pos[k] - r0.pos[k]);
                            scl[k] += b * (s.scl[k] - r0.scl[k]);
                        }
                        euler = None;
                        touched = true;
                    } else {
                        // 新算式：Δ = frame(t) − rest（rest = 本骨的参考/绑定姿态）——
                        // ⚠ 唯一例外（**基线保护**，不是语义）：动画**正好落在第 0 帧**时本层
                        // 贡献仍取零（= 旧口径），因为 17 条 640x360 `--t 0` 基线就是在那个采样点
                        // 上量的 —— 新算式在"第 0 帧 ≠ rest"的层上会在 t=0 立刻改变该层姿态
                        // （本场景：人物/月亮在 t=0 就被抬到画布上方）。这个例外**只影响
                        // `t = 0` 这一个瞬间**（`single` 的播放位置 t>0 后不再回到 0；
                        // `loop`/`mirror` 回到第 0 帧时两种算式的增量都恰好为 0）。
                        // 要整条回旧口径：`WE_ANIM_ADDITIVE=frame0`。
                        if s.exact && s.idx == 0 {
                            continue;
                        }
                        let b = *blend;
                        let dq = quat_mul(quat_conj(self.base_quat[i]), s.quat);
                        quat = quat_mul(quat, quat_nlerp_shortest([0.0, 0.0, 0.0, 1.0], dq, b));
                        for k in 0..3 {
                            pos[k] += b * (s.pos[k] - self.base_pos[i][k]);
                            scl[k] += b * (s.scl[k] - self.base_scl[i][k]);
                        }
                        euler = None;
                        touched = true;
                    }
                } else if *blend >= 1.0 && s.exact {
                    pos = s.pos;
                    scl = s.scl;
                    euler = Some(s.ang);
                    quat = s.quat;
                    touched = true;
                } else {
                    let b = *blend;
                    for k in 0..3 {
                        pos[k] = pos[k] * (1.0 - b) + s.pos[k] * b;
                        scl[k] = scl[k] * (1.0 - b) + s.scl[k] * b;
                    }
                    quat = quat_nlerp_shortest(quat, s.quat, b);
                    euler = None;
                    touched = true;
                }
            }
            local.push(if !touched {
                self.base_mat[i]
            } else if let Some(e) = euler {
                trs_col(pos, e, scl)
            } else {
                trs_quat_col(quat, pos, scl)
            });
        }
        let rw = chain_col(n, &self.parents, &local);
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            out.push(mat4_mul_col(&rw[i], &self.inv_wb[i]));
        }
        out
    }

    /// **诊断（不改渲染行为）**：逐层打印某根骨在 `t` 时刻的**旋转**贡献。
///
    /// 返回 `(动画 id, 播放位置 s, 采样角°, 该层增量角°, 累加角°)` —— 用来回答
    /// 「9 条加法层里哪一层把叶骨推歪、累加是不是爆炸」。算式与 [`Self::final_mats`]
    /// 的加法支**逐行走同一套**（含 `AddRef::Auto` 的 `use_frame0` 选择）。
    pub fn layer_rot_probe(&self, bone: usize, t: f32) -> Vec<(i64, f32, f32, f32, f32)> {
        let mut out = Vec::new();
        if bone >= self.parents.len() {
            return out;
        }
        let mut quat = self.base_quat[bone];
        for l in &self.layers {
            if l.blend <= 0.0 {
                continue;
            }
            // 同 `bones_at`：起始相位 + 播放推进
            let position = l.start_pos + t * l.rate;
            let mode_override = if self.legacy_loop { Some("loop") } else { None };
            let Some(s) = l.anim.sample_mode(bone, position, mode_override) else {
                continue;
            };
            if !l.additive {
                continue;
            }
            let r0 = l.anim.ref_frame(bone);
            let use_frame0 = match self.add_ref {
                AddRef::Frame0 => true,
                AddRef::Rest => false,
                AddRef::Auto => r0.as_ref().is_some_and(|f| {
                    frame0_is_rest(f, &self.base_pos[bone], &self.base_quat[bone], &self.base_scl[bone])
                }),
            };
            let dq = if use_frame0 {
                match r0 {
                    Some(r0) => quat_mul(quat_conj(quat_from_ang(r0.ang)), s.quat),
                    None => continue,
                }
            } else {
                quat_mul(quat_conj(self.base_quat[bone]), s.quat)
            };
            let inc = quat_nlerp_shortest([0.0, 0.0, 0.0, 1.0], dq, l.blend);
            quat = quat_mul(quat, inc);
            out.push((
                l.anim.id,
                position,
                quat_angle_deg(s.quat),
                quat_angle_deg(inc),
                quat_angle_deg(quat),
            ));
        }
        out
    }

    /// 该骨的 **bind 世界矩阵** `Wb[i]`（= 父链 `local_bind` 的复合）。
    /// 与 `puppet_diag` 的 `bind_world` 同一算式（可交叉验证）。
    pub fn world_bind(&self, i: usize) -> Option<[f32; 16]> {
        self.inv_wb.get(i).and_then(mat4_inv)
    }

    /// 该骨的**姿态矩阵**（puppet 空间）= [`Self::final_mats`] 的第 `i` 个 ——
    /// 也就是 OWE `PuppetLayer::genFrame(time)` 返回的那张表（它在返回前已经乘过
    /// `inv_bind`，所以 rest 时是**单位阵**）。
    pub fn pose_matrix(&self, i: usize, t: f32) -> Option<[f32; 16]> {
        self.final_mats(t).get(i).copied()
    }

    /// 挂接在 `t` 时刻的世界变换：`genFrame(t)[a.bone_index] · bind_xform`
    /// （OWE `Puppet.cpp:496-506` `PuppetLayer::attachmentTransform`）。
    /// ⚠ `genFrame` 返回的是**已经乘过 `inv_bind` 的表**（见 [`Self::pose_matrix`]）——
    /// 不要再乘一次 `Wb`（初版就踩了这个坑：rest 时给出 `Wb·bind_xform` 而不是 `bind_xform`，
    /// 实测 `цепь право` 的平移从 (462.98, 88.97) 被推成 (872.04, 225.81)）。
    /// 子对象的最终 local frame = 这个矩阵 · 它自己的原始 local frame
    /// （OWE `SceneFinalize.cpp:358-390`）。
    pub fn attachment_transform(&self, a: &Attachment, t: f32) -> Option<[f32; 16]> {
        let b = a.bone_index as usize;
        let pm = self.pose_matrix(b, t)?;
        let bx = attachment_bind_xform(a, &self.parents, &self.bind_local)?;
        Some(mat4_mul_col(&pm, &bx))
    }

    /// 按名字找挂接（OWE `Puppet::attachmentIndex`，线性查表、不做大小写折叠）。
    pub fn attachment_index<'a>(atts: &'a [Attachment], name: &str) -> Option<&'a Attachment> {
        atts.iter().find(|a| a.name == name)
    }

    /// 4 骨线性混合蒙皮：顶点 → 装配后的模型坐标。
    pub fn assemble(
        &self,
        bind: &[[f32; 3]],
        bi: &[[u32; 4]],
        bw: &[[f32; 4]],
        t: f32,
    ) -> Vec<[f32; 3]> {
        let f = self.final_mats(t);
        let n = bind.len().min(bi.len()).min(bw.len());
        let mut out = Vec::with_capacity(bind.len());
        for i in 0..bind.len() {
            if i < n {
                out.push(skin_vert(&f, bind[i], bi[i], bw[i]));
            } else {
                out.push(bind[i]);
            }
        }
        out
    }
}

/// 4 骨线性混合蒙皮（与 [`Rig::skin`] 同一算式）。
fn skin_vert(final_mats: &[[f32; 16]], p: [f32; 3], idx: [u32; 4], w: [f32; 4]) -> [f32; 3] {
    let mut acc = [0.0f32; 3];
    let mut wsum = 0.0;
    for k in 0..4 {
        let wk = w[k];
        if wk <= 0.0 {
            continue;
        }
        let i = idx[k] as usize;
        if i >= final_mats.len() {
            continue;
        }
        let m = &final_mats[i];
        acc[0] += (m[0] * p[0] + m[4] * p[1] + m[8] * p[2] + m[12]) * wk;
        acc[1] += (m[1] * p[0] + m[5] * p[1] + m[9] * p[2] + m[13]) * wk;
        acc[2] += (m[2] * p[0] + m[6] * p[1] + m[10] * p[2] + m[14]) * wk;
        wsum += wk;
    }
    if wsum.abs() < 1e-8 {
        p
    } else {
        [acc[0] / wsum, acc[1] / wsum, acc[2] / wsum]
    }
}

/// 列向量约定的 4×4 乘法（`C = A·B`，即先 B 后 A）。
fn mat4_mul_col(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut c = [0.0f32; 16];
    for col in 0..4 {
        for row in 0..4 {
            c[col * 4 + row] = (0..4).map(|k| a[k * 4 + row] * b[col * 4 + k]).sum();
        }
    }
    c
}

/// 一般 4×4 求逆（列主序 f32 列表 → 列主序 f32 列表），用 f64 高斯-约当。
fn mat4_inv(m: &[f32; 16]) -> Option<[f32; 16]> {
    // a[r][c] 为行主序副本，右半边是单位阵
    let mut a = [[0.0f64; 8]; 4];
    for r in 0..4 {
        for c in 0..4 {
            a[r][c] = m[c * 4 + r] as f64;
        }
        a[r][4 + r] = 1.0;
    }
    for col in 0..4 {
        let mut piv = col;
        for r in col + 1..4 {
            if a[r][col].abs() > a[piv][col].abs() {
                piv = r;
            }
        }
        if a[piv][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, piv);
        let d = a[col][col];
        for k in 0..8 {
            a[col][k] /= d;
        }
        for r in 0..4 {
            if r != col && a[r][col] != 0.0 {
                let f = a[r][col];
                for k in 0..8 {
                    a[r][k] -= f * a[col][k];
                }
            }
        }
    }
    let mut out = [0.0f32; 16];
    for r in 0..4 {
        for c in 0..4 {
            out[c * 4 + r] = a[r][4 + c] as f32;
        }
    }
    Some(out)
}

fn i32_at(data: &[u8], off: usize) -> Option<i32> {
    u32_at(data, off).map(|v| v as i32)
}

/// 读一个以 `\0` 结尾的字符串，返回 (字符串, 终点偏移)。
fn cstr_at(data: &[u8], off: usize) -> Option<(String, usize)> {
    let end = data[off..].iter().position(|&b| b == 0)? + off;
    Some((String::from_utf8_lossy(&data[off..end]).to_string(), end + 1))
}

/// 由 WE 动画帧的 `(位置, 欧拉角, 缩放)` 组成**列主序**局部 4×4。
/// 旋转顺序与 OWE `Puppet::ToQuaternion` 一致：`Rz·Ry·Rx`（列向量约定，先 x 后 y 后 z）。
fn trs_col(pos: [f32; 3], euler: [f32; 3], scale: [f32; 3]) -> [f32; 16] {
    let (sx, cx) = euler[0].sin_cos();
    let (sy, cy) = euler[1].sin_cos();
    let (sz, cz) = euler[2].sin_cos();
    // R = Rz·Ry·Rx（行主序的 3×3）
    let r = [
        [cz * cy, cz * sy * sx - sz * cx, cz * sy * cx + sz * sx],
        [sz * cy, sz * sy * sx + cz * cx, sz * sy * cx - cz * sx],
        [-sy, cy * sx, cy * cx],
    ];
    let mut m = [0.0f32; 16];
    for c in 0..3 {
        for row in 0..3 {
            m[c * 4 + row] = r[row][c] * scale[c];
        }
    }
    m[12] = pos[0];
    m[13] = pos[1];
    m[14] = pos[2];
    m[15] = 1.0;
    m
}

/// 从 `MDLA` 里取**指定动画**在时间 `t = 0` 的逐骨局部矩阵（老 API，保留给 A/B 与单测）。
///
/// `t = 0` 精确落在第 0 帧 ⇒ 走**欧拉直出**（[`trs_col`]），与"只读第 0 帧"的旧实现逐位一致。
/// 返回 `None` 的元素 = 该骨没有变换采样（`unk & 1 != 0` 或轨道为空）⇒ 用参考姿态兜底。
pub fn parse_animation_pose(
    data: &[u8],
    want_id: i64,
    bones_num: usize,
) -> Result<Vec<Option<[f32; 16]>>, String> {
    let anims = parse_animations(data)?;
    let a = anims.iter().find(|a| a.id == want_id).ok_or_else(|| {
        format!(
            "MDLA 里没有动画 id={want_id}（有 {:?}）",
            anims.iter().map(|a| a.id).collect::<Vec<_>>()
        )
    })?;
    let mut pose: Vec<Option<[f32; 16]>> = vec![None; bones_num];
    for (i, slot) in pose.iter_mut().enumerate() {
        if let Some(s) = a.sample(i, 0.0) {
            *slot = Some(if s.exact {
                trs_col(s.pos, s.ang, s.scl)
            } else {
                trs_quat_col(s.quat, s.pos, s.scl)
            });
        }
    }
    Ok(pose)
}

/// 解析装配骨架（OWE `MdlParser::ParseMDLS` 的布局）：
/// `MDLS<ver>\0` + `end u32` + `bones_num u16` + `pad u16` + 逐骨
/// `{name\0, sim_type i32, parent u32, size u32(=64), local_bind 16×f32, sim_json\0}`
/// 之后（MDLSv2）`extras u16` + `has_world_binds u8`(+ 逐骨 16×f32 reference) + `pad[8]`。
///
/// 关键：`animation_reference` 是"装配姿态"，`local_bind` 是"散件姿态"。
pub fn parse_rig(data: &[u8]) -> Result<Rig, String> {
    parse_rig_with(data, None)
}

/// 解析骨架的**原始数据**（`MDLS`）：逐骨 `parent` / `local_bind` / （MDLSv2）`animation_reference`。
/// 装配的复合与蒙皮见 [`PuppetPose`]。
pub fn parse_rig_data(data: &[u8]) -> Result<RigData, String> {
    let mdls = data
        .windows(4)
        .position(|w| w == b"MDLS")
        .ok_or("没有 MDLS 骨架块")?;
    let version = String::from_utf8_lossy(&data[mdls..(mdls + 8).min(data.len())]).to_string();
    let bones_num = u32_at(data, mdls + 13).ok_or("MDLS 头被截断")? as usize & 0xffff;
    if bones_num == 0 || bones_num > 512 {
        return Err(format!("{version} 骨骼数不合理：{bones_num}"));
    }
    let mut o = mdls + 17;
    let mut parents = Vec::with_capacity(bones_num);
    let mut local = Vec::with_capacity(bones_num);
    for _ in 0..bones_num {
        let (_, next) = cstr_at(data, o).ok_or("骨骼名被截断")?;
        o = next + 4; // 跳过 sim_type i32
        let parent = i32_at(data, o).ok_or("骨骼 parent 截断")?;
        o += 4;
        let size = u32_at(data, o).ok_or("骨骼 size 截断")?;
        o += 4;
        if size != 64 {
            return Err(format!("骨骼 size 非 64：{size}"));
        }
        let mut m = [0.0f32; 16];
        for (i, slot) in m.iter_mut().enumerate() {
            *slot = f32_at(data, o + 4 * i).ok_or("骨骼矩阵截断")?;
        }
        o += 64;
        let (_, next) = cstr_at(data, o).ok_or("骨骼 JSON 截断")?;
        o = next;
        parents.push(parent);
        local.push(m);
    }
    // MDLSv2 尾部：extras_flag u16、has_world_binds u8。只有 has_world_binds 时才有装配矩阵。
    let extras = u32_at(data, o).ok_or("MDLS 尾部截断")? as u16;
    o += 2;
    let has_world_binds = *data.get(o).ok_or("MDLS 尾部截断")?;
    o += 1;
    let _ = extras;
    let mut reference: Option<Vec<[f32; 16]>> = None;
    if has_world_binds != 0 {
        let mut r = Vec::with_capacity(bones_num);
        for _ in 0..bones_num {
            let mut m = [0.0f32; 16];
            for (i, slot) in m.iter_mut().enumerate() {
                *slot = f32_at(data, o + 4 * i).ok_or("reference 矩阵截断")?;
            }
            o += 64;
            r.push(m);
        }
        reference = Some(r);
    }
    Ok(RigData { parents, local, reference })
}

/// MDAT 块里的一条**挂接**（场景里的 `attachment` 就用它的 `name` 查表）。
///
/// 布局依据 OWE `Scene/Pkg/Parse/Puppet/MdlParser.cpp::ParseMDAT`（612-631）：
/// `end_offset:u32` + `num:u16`，逐条 `bone_index:u16` + `name:cstr` +
/// **64 字节列主序 4×4 仿射**（linear 在 col 0-2、translation 在 col 3），
/// 注释原文是「64-byte payload = column-major 4x4 affine in the anchored bone's
/// local space」。名字是**以 NUL 结尾**的字符串（实测本模型 `model/девка розбив_puppet.mdl`
/// 的 MDAT0001：4 条 = `цепь право`(骨12) / `цепь лево`(骨21) / `жопец`(骨3) / `ж2`(骨2)）。
#[derive(Debug, Clone, Default)]
pub struct Attachment {
    pub bone_index: u16,
    pub name: String,
    pub local_xform: [f32; 16],
}

/// 解析 MDAT 块。没有 MDAT（例如 `цепь рука_puppet.mdl` 就没有）⇒ 返回空表。
///
/// 解析完**必须正好落在 `end_offset`**，否则报错 —— 与 OWE 的 end_offset 检查同法，
/// 避免错位后静默产出垃圾。`end_offset == 0`（作者没写）时不检查。
pub fn parse_attachments(data: &[u8]) -> Result<Vec<Attachment>, String> {
    let Some(at) = data.windows(4).position(|w| w == b"MDAT") else {
        return Ok(Vec::new());
    };
    let mut o = at + 9; // 9 字节 tag（"MDAT0001\0"，与 OWE `consume_tag` 一致）
    let end = u32_at(data, o).ok_or("MDAT 头截断")? as usize;
    o += 4;
    let num = u32_at(data, o).ok_or("MDAT 头截断")? as usize & 0xffff;
    o += 2;
    if num > 1024 {
        return Err(format!("MDAT 挂接数不合理：{num}"));
    }
    let mut out = Vec::with_capacity(num);
    for i in 0..num {
        let bone_index = u32_at(data, o).ok_or("MDAT 骨号截断")? as u16;
        o += 2;
        let (name, next) = cstr_at(data, o).ok_or("MDAT 名字截断")?;
        o = next;
        let mut m = [0.0f32; 16];
        for (k, slot) in m.iter_mut().enumerate() {
            *slot = f32_at(data, o + 4 * k).ok_or("MDAT 矩阵截断")?;
        }
        o += 64;
        out.push(Attachment {
            bone_index,
            name,
            local_xform: m,
        });
        let _ = i;
    }
    if end != 0 && o != end {
        return Err(format!("MDAT 解析落在 {o}，end_offset={end}（布局不符）"));
    }
    Ok(out)
}

/// 挂接的**绑定变换**：`bind_xform = Π local_bind(父链：根 → 该骨) · local_xform`
/// 与 OWE `Puppet.cpp:119-136`（`prepared`）逐行一致（它先用 `file_parent` 收集
/// 根→骨的链，再从链尾往链首乘 `local_bind`）。
pub fn attachment_bind_xform(
    a: &Attachment,
    parents: &[i32],
    local: &[[f32; 16]],
) -> Option<[f32; 16]> {
    let mut bone = a.bone_index as usize;
    if bone >= parents.len() {
        return None;
    }
    let mut chain = Vec::new();
    let mut guard = 0;
    loop {
        chain.push(bone);
        let p = parents[bone];
        if p < 0 || p as usize >= parents.len() || guard > parents.len() {
            break;
        }
        bone = p as usize;
        guard += 1;
    }
    let mut acc = [
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ];
    for &b in chain.iter().rev() {
        acc = mat4_mul_col(&acc, &local[b]);
    }
    Some(mat4_mul_col(&acc, &a.local_xform))
}

/// 同 [`parse_rig`]，但可用 `anim_id` 指定动画：该动画的第 0 帧替换参考姿态。
///
/// 保留给"静态装配"的 A/B（时间动画走 [`parse_with_layers`]）。
pub fn parse_rig_with(data: &[u8], anim_id: Option<i64>) -> Result<Rig, String> {
    let rd = parse_rig_data(data)?;
    let bones_num = rd.parents.len();
    // 每根骨的"基础局部姿态"：有 MDLSv2 参考矩阵就用它，否则用 `local_bind`。
    let mut pose: Vec<[f32; 16]> = (0..bones_num)
        .map(|i| match &rd.reference {
            Some(r) => r[i],
            None => rd.local[i],
        })
        .collect();
    // 动画姿态：`animationlayers` 里 `additive:false` 的层**替换**基础姿态
    // ⇒ 部件的装配落点由动画帧决定，而不是 MDLS 的参考姿态。
    // 这一步顺带修掉"没有参考矩阵"的模型（`has_world_binds=0`：`头发1`/`B沙发`）：
    // 它们以前拿**未链过**的 local 当 Rw ⇒ `final = local_i·Wb_i⁻¹`，把顶点整体平移
    // ~867 单位（马尾/刘海因此飞到肩上、沙发镜像）。链过之后这类模型退化成
    // `final = Wb·Wb⁻¹ = I`（= 保持作者摆好的图集布局），与 OWE 一致。
    if let Some(id) = anim_id {
        match parse_animation_pose(data, id, bones_num) {
            Ok(p) => {
                let mut n = 0;
                for i in 0..bones_num {
                    if let Some(m) = p[i] {
                        pose[i] = m;
                        n += 1;
                    }
                }
                if std::env::var("WE_MESH_DEBUG").is_ok() {
                    eprintln!("[装配] 动画 {id} 覆盖 {n}/{bones_num} 根骨的姿态");
                }
            }
            Err(e) => {
                if std::env::var("WE_MESH_DEBUG").is_ok() {
                    eprintln!("[装配] 动画 {id} 读不出（{e}）⇒ 用参考姿态");
                }
            }
        }
    }
    let wb = chain_col(bones_num, &rd.parents, &rd.local);
    let rw = chain_col(bones_num, &rd.parents, &pose);
    let mut final_mats = Vec::with_capacity(bones_num);
    for i in 0..bones_num {
        let inv = mat4_inv(&wb[i]).ok_or("bind 世界矩阵不可逆")?;
        final_mats.push(mat4_mul_col(&rw[i], &inv));
    }
    Ok(Rig { bones: bones_num, final_mats })
}

/// 一根骨骼。
/// 记录布局：`[flags u32][parent i32][msize=64 u32][4x4 f32 矩阵][变长尾巴]`，
/// 矩阵前 4 字节恒为 `64` —— 这条是**硬自检**，用来在变长尾巴里找下一根骨。
#[derive(Debug, Clone, Default)]
pub struct Bone {
    /// 记录头里的 parent 字段（实测常常是 1,3,5,7… 可疑，暂不当层级用）
    pub parent_field: i32,
    /// 局部（父相对）4×4，**行向量约定**：平移在第 4 行、第 4 列 (0,0,0,1)
    pub local: [f32; 16],
    /// 按骨骼顺序累积出的世界矩阵（`world_i = local_i · world_{i-1}`，已用"沿身体单调"验证过）
    pub world: [f32; 16],
    /// 世界上锚点（= `world` 的平移，行向量约定在第 4 行）
    pub pivot: [f32; 3],
    /// 记录尾巴里的明文 JSON（WE 的每骨 puppet 参数：`lamax/lamin/rax/ik/tm/tp`…）
    pub props: Option<String>,
}

/// 解析出来的骨架（`MDLS` 块）。
#[derive(Debug, Clone, Default)]
pub struct Skeleton {
    pub version: String,
    pub bones: Vec<Bone>,
}

fn mat4_mul(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    // 行向量约定：p' = p · M ⇒ C = A · B 表示"先 A 后 B"的复合（与 Python 版一致）
    let mut c = [0.0f32; 16];
    for r in 0..4 {
        for col in 0..4 {
            c[r * 4 + col] = (0..4).map(|k| a[r * 4 + k] * b[k * 4 + col]).sum();
        }
    }
    c
}

/// 一个窗口是不是"合法骨骼矩阵"：16 个 f32 有限、第 4 列 ≈ (0,0,0,1)、3×3 行范数 ≈ 1，
/// 且**矩阵前 4 字节 == 64**（记录头自检）。
fn is_bone_matrix(d: &[u8], o: usize, lim: usize) -> bool {
    if o + 64 > lim || o < 4 {
        return false;
    }
    if u32_at(d, o - 4) != Some(64) {
        return false;
    }
    let v: Vec<f32> = (0..16).filter_map(|i| f32_at(d, o + 4 * i)).collect();
    if v.len() != 16 || v.iter().any(|x| !x.is_finite() || x.abs() > 1e5) {
        return false;
    }
    if v[3].abs() + v[7].abs() + v[11].abs() + (v[15] - 1.0).abs() > 0.02 {
        return false;
    }
    (0..3).all(|r| {
        let n = (0..3).map(|c| v[r * 4 + c] * v[r * 4 + c]).sum::<f32>().sqrt();
        (n - 1.0).abs() <= 0.05
    })
}

/// 从记录尾巴里抠出第一段平衡的 JSON（`{...}`），供上层读作者参数。
fn extract_json(d: &[u8], start: usize, lim: usize) -> Option<String> {
    let mut o = start;
    while o < lim && d[o] != b'{' {
        o += 1;
    }
    if o >= lim {
        return None;
    }
    let mut depth = 0i32;
    let mut i = o;
    while i < lim {
        match d[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return String::from_utf8(d[o..=i].to_vec()).ok();
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// 解析 `MDLS` 骨架块。
///
/// ⚠ 每骨记录**变长**（尾巴里是 JSON），所以不能按固定 stride 平推：读一个矩阵后
/// 从它末尾逐 4 字节前扫，直到下一个"前 4 字节 == 64 且矩阵合法"的窗口。
pub fn parse_skeleton(data: &[u8]) -> Result<Skeleton, String> {
    let mdls = data
        .windows(4)
        .position(|w| w == b"MDLS")
        .ok_or("没有 MDLS 骨架块")?;
    let version = String::from_utf8_lossy(&data[mdls..(mdls + 8).min(data.len())]).to_string();
    let next_off = u32_at(data, mdls + 9).ok_or("MDLS 头被截断")? as usize;
    let count = u32_at(data, mdls + 13).ok_or("MDLS 头被截断")? as usize;
    if count == 0 || count > 256 {
        return Err(format!("骨骼数不合理：{count}"));
    }
    let lim = next_off.min(data.len());
    if mdls + 30 >= lim {
        return Err("MDLS 段太短".into());
    }

    let mut bones: Vec<Bone> = Vec::with_capacity(count);
    let mut o = mdls + 30; // 第一根骨矩阵的起点（实测 MDLS+30）
    while o + 64 <= lim && bones.len() < count {
        if !is_bone_matrix(data, o, lim) {
            break;
        }
        let rec = o - 13; // 记录头起点
        let parent_field = struct_i32(data, rec + 5).unwrap_or(-1);
        let mut local = [0.0f32; 16];
        for (i, slot) in local.iter_mut().enumerate() {
            *slot = f32_at(data, o + 4 * i).unwrap_or(0.0);
        }
        // 世界 = 自己的局部 · 上一根的世界（顺序即链条，实测"沿身体单调"成立）
        let world = match bones.last() {
            Some(prev) => mat4_mul(&local, &prev.world),
            None => local,
        };
        let pivot = [world[12], world[13], world[14]];
        bones.push(Bone {
            parent_field,
            local,
            world,
            pivot,
            props: None,
        });
        // 找下一根骨的矩阵
        let mut o2 = o + 64;
        while o2 + 64 <= lim && !is_bone_matrix(data, o2, lim) {
            o2 += 4;
        }
        // 尾巴里的 JSON（本根骨与下一根之间）
        if let Some(seg) = bones.last_mut() {
            seg.props = extract_json(data, o + 64, o2.min(lim));
        }
        o = o2;
    }
    if bones.is_empty() {
        return Err("一根骨也没解出来".into());
    }
    Ok(Skeleton { version, bones })
}

#[cfg(test)]
mod anim_tests {
    use super::*;

    fn f32b(v: f32) -> Vec<u8> {
        v.to_le_bytes().to_vec()
    }
    fn u32b(v: u32) -> Vec<u8> {
        v.to_le_bytes().to_vec()
    }
    fn cstrb(s: &str) -> Vec<u8> {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        v
    }
    fn frame(pos: [f32; 3], ang: [f32; 3], scl: [f32; 3]) -> Vec<u8> {
        let mut v = Vec::new();
        for x in pos.iter().chain(ang.iter()).chain(scl.iter()) {
            v.extend_from_slice(&f32b(*x));
        }
        v
    }

    fn synth_mdla() -> Vec<u8> {
        synth_mdla_ver(4)
    }

    /// 造一个 2 个动画的 MDLA 块（走全部尾巴分支，`ver` 可切 4/6）。
    /// anim1：`trans_flag=0` 且有 trans main 轨道 + bone curves + v4 events + 记录对齐；
    /// anim2：最简形态（无 trans / 无 curves / 无 events）。
    /// `ver >= 5` 时每个动画多一段 AABB（6×f32）；`ver == 6` 时 anim1 还多一组标量曲线
    /// （`3704273480` 的模型就是 MDLV0023/MDLA0006）。
    fn synth_mdla_ver(ver: i32) -> Vec<u8> {
        synth_mdla_mode(ver, "loop")
    }

    /// 同 [`synth_mdla_ver`]，但可指定 anim2（id=204、length=4）的**播放模式字符串**
    /// （`loop` / `mirror` / `single`）—— 给播放模式语义的单测用。
    fn synth_mdla_mode(ver: i32, mode2: &str) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(format!("MDLA{ver:04}\0").as_bytes());
        let end_pos = d.len();
        d.extend_from_slice(&u32b(0)); // end_offset 占位
        d.extend_from_slice(&u32b(2)); // anims
        // ── anim 1：id=465，name="动画 1"，loop，fps=30，length=2（3 帧）
        d.extend_from_slice(&u32b(465));
        d.extend_from_slice(&u32b(0)); // unk_after_id
        d.extend_from_slice(&cstrb("动画 1"));
        d.extend_from_slice(&cstrb("loop"));
        d.extend_from_slice(&f32b(30.0));
        d.extend_from_slice(&u32b(2)); // length
        d.extend_from_slice(&u32b(0)); // flags
        d.extend_from_slice(&u32b(2)); // 骨轨道数
        for b in 0..2u32 {
            d.extend_from_slice(&u32b(0)); // unk（&1==0 ⇒ 有采样）
            d.extend_from_slice(&u32b(3 * 36));
            for k in 0..3 {
                let x = b as f32 * 10.0 + k as f32;
                d.extend_from_slice(&frame([x, 0.0, 0.0], [0.0, 0.0, 0.0], [1.0, 1.0, 1.0]));
            }
        }
        // trans_flag = 0 且有 trans main 轨道（3 个采样 × 4 字节 = 12）
        d.extend_from_slice(&u32b(0));
        d.extend_from_slice(&u32b(12));
        d.extend_from_slice(&[0u8; 12]);
        // bone curves：has=1，2 根骨各 (u32 zero + u32 8 + 8 字节)
        d.push(1);
        for _ in 0..2 {
            d.extend_from_slice(&u32b(0));
            d.extend_from_slice(&u32b(8));
            d.extend_from_slice(&[0u8; 8]);
        }
        // v4 events：has=1，1 个事件、1 条曲线
        d.push(1);
        d.extend_from_slice(&u32b(1));
        d.extend_from_slice(&f32b(0.5));
        d.extend_from_slice(&u16::to_le_bytes(1u16));
        d.extend_from_slice(&u16::to_le_bytes(0u16));
        d.extend_from_slice(&u32b(4));
        d.extend_from_slice(&[0u8; 4]);
        if ver >= 5 {
            d.extend_from_slice(&[0u8; 24]); // 动画 AABB
        }
        if ver == 6 {
            d.push(1); // 标量曲线：has=1
            for _ in 0..2 {
                d.extend_from_slice(&u32b(0));
                d.extend_from_slice(&u32b(8));
                d.extend_from_slice(&[0u8; 8]);
            }
        }
        // 事件表：1 条
        d.extend_from_slice(&u32b(1));
        d.extend_from_slice(&u32b(7));
        d.extend_from_slice(&cstrb("{}"));
        // 记录对齐：**一个** u32 零 + 紧跟着的下一个动画的 (id, unk_after_id=0)
        // （参照 `next_is_anim_record_padding` 就是看这三处，然后只吃掉那个零）
        d.extend_from_slice(&u32b(0));
        // ── anim 2：id=204，length=4（5 帧）
        d.extend_from_slice(&u32b(204));
        d.extend_from_slice(&u32b(0));
        d.extend_from_slice(&cstrb("摇头"));
        d.extend_from_slice(&cstrb(mode2));
        d.extend_from_slice(&f32b(30.0));
        d.extend_from_slice(&u32b(4));
        d.extend_from_slice(&u32b(0));
        d.extend_from_slice(&u32b(1));
        d.extend_from_slice(&u32b(0));
        d.extend_from_slice(&u32b(5 * 36));
        for k in 0..5 {
            d.extend_from_slice(&frame([k as f32, 1.0, 0.0], [0.0, 0.0, 0.0], [1.0, 1.0, 1.0]));
        }
        d.extend_from_slice(&u32b(0)); // trans_flag
        d.push(0); // bone curves: 无
        d.push(0); // v4 events: 无
        if ver >= 5 {
            d.extend_from_slice(&[0u8; 24]); // 动画 AABB
        }
        if ver == 6 {
            d.push(0); // 标量曲线：has=0（lookahead 会吃掉这 1 字节）
        }
        d.extend_from_slice(&u32b(0)); // 事件表：0 条
        let end = d.len() as u32;
        d[end_pos..end_pos + 4].copy_from_slice(&u32b(end));
        d
    }

    #[test]
    fn walks_mdla_tail_and_finds_both_animations() {
        let d = synth_mdla();
        let anims = parse_animations(&d).expect("应能顺序走完两个动画");
        assert_eq!(anims.len(), 2);
        assert_eq!(anims[0].id, 465);
        assert_eq!(anims[0].length, 2);
        assert_eq!(anims[0].tracks.len(), 2);
        assert_eq!(anims[0].tracks[0].frames.len(), 3);
        assert_eq!(anims[1].id, 204, "第二个动画必须能被定位到（尾巴跳对了）");
        assert_eq!(anims[1].length, 4);
        assert_eq!(anims[1].tracks[0].frames.len(), 5);
        // 帧数据逐位对
        assert_eq!(anims[0].tracks[0].frames[1].pos[0].to_bits(), 1.0f32.to_bits());
        assert_eq!(anims[1].tracks[0].frames[4].pos[0].to_bits(), 4.0f32.to_bits());
    }

    /// MDLA0006（`3704273480` 的模型版本）：v5 的 AABB + v6 的标量曲线都要跳对，
    /// 否则第 2 个动画之后就找不到、整块解析失败（实测过：静默失败 ⇒ B 的人物不动）。
    #[test]
    fn walks_mdla_v6_tail() {
        let d = synth_mdla_ver(6);
        let anims = parse_animations(&d).expect("MDLA0006 也要能走完");
        assert_eq!(anims.len(), 2);
        assert_eq!(anims[0].id, 465);
        assert_eq!(anims[1].id, 204);
        assert_eq!(anims[1].tracks[0].frames.len(), 5);
    }

    #[test]
    fn sample_exactly_on_a_frame_returns_the_raw_floats() {
        let anims = parse_animations(&synth_mdla()).unwrap();
        let a = &anims[1]; // length = 4, fps = 30
        // t = 2/30 s ⇒ 正好第 2 帧
        let s = a.sample(0, 2.0 / 30.0).unwrap();
        assert!(s.exact, "落在帧上必须是 exact（走欧拉直出 ⇒ 逐位可复现）");
        assert_eq!(s.idx, 2);
        assert_eq!(s.frac, 0.0);
        assert_eq!(s.pos[0].to_bits(), 2.0f32.to_bits());
        // t = 3/30 + 半个帧 ⇒ 在 3 与 4 之间插值
        let s2 = a.sample(0, 3.5 / 30.0).unwrap();
        assert!(!s2.exact);
        assert_eq!(s2.idx, 3);
        assert!((s2.frac - 0.5).abs() < 1e-6);
        assert!((s2.pos[0] - 3.5).abs() < 1e-6, "pos={}", s2.pos[0]);
        let s3 = a.sample(0, 5.0 / 30.0).unwrap();
        assert_eq!(s3.idx, 0, "超过 duration 要取模回到第 0 帧");
    }

    #[test]
    fn quaternion_matches_euler_matrix() {
        for a in [
            [0.0f32, 0.0, 0.0],
            [0.3, 0.0, 0.0],
            [0.0, -0.7, 0.2],
            [0.11, 0.22, -0.33],
            [1.0, 2.0, 3.0],
        ] {
            let m_e = trs_col([1.0, -2.0, 3.0], a, [1.5, 0.5, 1.0]);
            let m_q = trs_quat_col(quat_from_ang(a), [1.0, -2.0, 3.0], [1.5, 0.5, 1.0]);
            for k in 0..16 {
                assert!(
                    (m_e[k] - m_q[k]).abs() < 1e-4,
                    "欧拉矩阵与四元数矩阵不一致（euler={a:?} k={k} {} vs {}）",
                    m_e[k],
                    m_q[k]
                );
            }
        }
    }

    #[test]
    fn decompose_roundtrips_through_quaternion() {
        for a in [[0.0f32; 3], [0.4, -0.2, 0.9], [2.5, 0.1, -1.0]] {
            let scl = [2.0f32, 0.75, 1.25];
            let m = trs_col([3.0, -4.0, 5.0], a, scl);
            let (p, q, s) = decompose_col(&m);
            let back = trs_quat_col(q, p, s);
            for k in 0..16 {
                assert!((m[k] - back[k]).abs() < 1e-3, "k={k}: {} vs {}", m[k], back[k]);
            }
        }
    }

    /// 造一份 1 骨的 RigData + 一个"加法层"，验证 **t = 0 时增量恒为零**（这是 t=0 逐位不变的地基）。
    fn one_bone_rig() -> RigData {
        let mut id = [0.0f32; 16];
        id[0] = 1.0;
        id[5] = 1.0;
        id[10] = 1.0;
        id[15] = 1.0;
        RigData { parents: vec![-1], local: vec![id], reference: Some(vec![id]) }
    }

    #[test]
    fn additive_layer_contributes_nothing_at_t_zero() {
        let anims = parse_animations(&synth_mdla()).unwrap();
        let anim = anims[1].clone(); // pos[0] = 0,1,2,3,4（帧号）
        let static_pose = PuppetPose::new(&one_bone_rig(), &[], &[]);
        let add_pose = PuppetPose::new(
            &one_bone_rig(),
            std::slice::from_ref(&anim),
            &[PoseLayer { id: 204, additive: true, blend: 1.0, rate: 1.0, start: (0.0, 0.0) }],
        );
        assert_eq!(add_pose.layer_ids().len(), 1, "层必须被解析到");
        let a0 = static_pose.final_mats(0.0);
        let b0 = add_pose.final_mats(0.0);
        for k in 0..16 {
            assert_eq!(
                a0[0][k].to_bits(),
                b0[0][k].to_bits(),
                "t=0 时加法层必须一个 bit 都不改（k={k}）"
            );
        }
        // t = 1/30 s ⇒ 第 1 帧 ⇒ 位移 +1（加法增量）
        let b1 = add_pose.final_mats(1.0 / 30.0);
        assert!((b1[0][12] - 1.0).abs() < 1e-5, "x={}", b1[0][12]);
    }

    #[test]
    fn additive_and_replace_layers_differ_in_semantics() {
        let anims = parse_animations(&synth_mdla()).unwrap();
        let anim = anims[1].clone();
        let replace = PuppetPose::new(
            &one_bone_rig(),
            std::slice::from_ref(&anim),
            &[PoseLayer { id: 204, additive: false, blend: 1.0, rate: 1.0, start: (0.0, 0.0) }],
        );
        // 替换式：t=1/30 ⇒ 姿态变成第 1 帧的**绝对值**（pos[0] = 1）
        let r1 = replace.final_mats(1.0 / 30.0);
        assert!((r1[0][12] - 1.0).abs() < 1e-5, "x={}", r1[0][12]);
        // 而 t=0 时替换式 = 第 0 帧（pos[0]=0）⇒ 与参考姿态一致（参考也全是 0 平移）
        let r0 = replace.final_mats(0.0);
        assert!(r0[0][12].abs() < 1e-6);
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // 播放模式（MDLA 的 mode 字符串）+ rate ——
    // 依据：OWE `Puppet.cpp::Animation::getInterpolationInfo`（loop / single / mirror
    // 三条分支）与 `PuppetLayer::prepared`（rate 驱动 playback）。
    // ══════════════════════════════════════════════════════════════════════════════

    #[test]
    fn play_mode_strings_match_the_reference() {
        assert_eq!(PlayMode::from_str("loop"), PlayMode::Loop);
        assert_eq!(PlayMode::from_str("mirror"), PlayMode::Mirror);
        assert_eq!(PlayMode::from_str("single"), PlayMode::Single);
        assert_eq!(PlayMode::from_str(""), PlayMode::Loop, "空 ⇒ loop（OWE 默认分支）");
        assert_eq!(PlayMode::from_str("Mirror"), PlayMode::Mirror, "大小写不敏感");
        assert_eq!(PlayMode::from_str("weird"), PlayMode::Loop, "未知 ⇒ loop");
    }

    /// **mirror 的端点必须是连续的**：往返的转折点上，转折前后两帧的姿态差应当极小
    /// （同一个帧区间、只是方向反转）—— 这正是"摆动后瞬间回到初始位置"的判据。
    /// 合成动画：anim2 的 pos[0] = 帧号（0,1,2,3,4），length=4，fps=30。
    #[test]
    fn mirror_is_continuous_at_both_ends_while_loop_snaps() {
        let anims = parse_animations(&synth_mdla_mode(4, "mirror")).unwrap();
        let a = &anims[1];
        assert_eq!(a.play_mode(), PlayMode::Mirror);
        let dur = a.duration(); // 4/30 秒
        let dt = 1.0 / 30.0;
        // 正向半程：0 → 4
        assert!((a.sample(0, 0.0).unwrap().pos[0] - 0.0).abs() < 1e-6);
        assert!((a.sample(0, 2.0 * dt).unwrap().pos[0] - 2.0).abs() < 1e-6);
        assert!((a.sample(0, dur).unwrap().pos[0] - 4.0).abs() < 1e-6, "半程端点 = 末帧");
        // 转折点（position = dur）前后各一帧：3 与 4-ε，**方向反转但位置连续**
        let before = a.sample(0, dur - dt).unwrap().pos[0]; // 3
        let after = a.sample(0, dur + dt).unwrap().pos[0]; // 镜像 ⇒ 3
        assert!(
            (after - before).abs() < 1e-6,
            "mirror 转折点必须连续：{before} vs {after}"
        );
        // 周期端点（position = 2·dur）前后：0 与 1（回程），也连续
        let near_end = a.sample(0, 2.0 * dur - dt).unwrap().pos[0];
        let after_wrap = a.sample(0, 2.0 * dur + dt).unwrap().pos[0];
        assert!((near_end - 1.0).abs() < 1e-6 && (after_wrap - 1.0).abs() < 1e-6,
                "周期端点也必须连续：{near_end} vs {after_wrap}");
        // 对照：同一动画按旧行为（loop）播 ⇒ 在 position = dur 处**瞬间回跳到 0 附近**
        let l_before = a.sample_mode(0, dur - dt, Some("loop")).unwrap().pos[0]; // 3
        let l_after = a.sample_mode(0, dur + dt, Some("loop")).unwrap().pos[0]; // 1（取模）
        assert!(
            (l_after - l_before).abs() > 1.5,
            "旧行为（loop 取模）在端点确实有 ~2 帧的跳变：{l_before} → {l_after}"
        );
    }

    #[test]
    fn single_mode_stops_at_the_last_frame() {
        let anims = parse_animations(&synth_mdla_mode(4, "single")).unwrap();
        let a = &anims[1];
        assert_eq!(a.play_mode(), PlayMode::Single);
        let dur = a.duration();
        assert!((a.sample(0, dur * 10.0).unwrap().pos[0] - 4.0).abs() < 1e-6, "播完停在末帧");
        assert!(
            (a.sample(0, dur * 2.0).unwrap().pos[0] - 4.0).abs() < 1e-6,
            "single 不绕回"
        );
    }

    /// `rate` = 播放速率（位置 = t·rate）：同一个 `t`，rate=2 应当走到 rate=1 的两倍位置。
    #[test]
    fn rate_scales_the_playback_position() {
        let anims = parse_animations(&synth_mdla()).unwrap();
        let anim = anims[1].clone();
        let mk = |rate: f32| {
            PuppetPose::new(
                &one_bone_rig(),
                std::slice::from_ref(&anim),
                &[PoseLayer { id: 204, additive: false, blend: 1.0, rate, start: (0.0, 0.0) }],
            )
        };
        let r1 = mk(1.0).final_mats(1.0 / 30.0);
        let r2 = mk(2.0).final_mats(1.0 / 30.0);
        assert!((r1[0][12] - 1.0).abs() < 1e-5, "rate=1 ⇒ 第 1 帧 x={}", r1[0][12]);
        assert!((r2[0][12] - 2.0).abs() < 1e-5, "rate=2 ⇒ 第 2 帧 x={}", r2[0][12]);
        // rate 不影响 t=0（t=0 逐位不变的构造性保证）
        assert!(mk(2.0).final_mats(0.0)[0][12].abs() < 1e-6);
    }
}

#[cfg(test)]
mod skeleton_tests {
    use super::*;

    fn f32b(v: f32) -> [u8; 4] {
        v.to_le_bytes()
    }

    /// 造一个 2 骨骨架：第一根平移 (10,0,0)，第二根相对父再平移 (0,5,0)
    fn synth() -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(b"MDLV0017\0");
        // 网格区随便占点位置（MDLS 之前）
        d.extend_from_slice(&[0xAB; 13]);
        let mdls_at = d.len();
        d.extend_from_slice(b"MDLS0002\0");
        let mut out = Vec::new();
        out.extend_from_slice(&0u32.to_le_bytes()); // next_off 占位
        out.extend_from_slice(&2u32.to_le_bytes()); // 骨骼数
        for (i, t) in [(10.0f32, 0.0f32), (0.0, 5.0)].iter().enumerate() {
            let mut rec = Vec::new();
            rec.extend_from_slice(&0x100u32.to_le_bytes()); // flags
            rec.extend_from_slice(&[0u8]); // 1 字节填充（实测 parent 在 rec+5）
            rec.extend_from_slice(&(-1i32).to_le_bytes()); // parent @ rec+5
            rec.extend_from_slice(&64u32.to_le_bytes()); // msize @ rec+9（矩阵前一字段）
            let mut m = [0.0f32; 16];
            m[0] = 1.0;
            m[5] = 1.0;
            m[10] = 1.0;
            m[15] = 1.0;
            m[12] = t.0;
            m[13] = t.1;
            for v in m.iter() {
                rec.extend_from_slice(&f32b(*v));
            }
            if i == 0 {
                rec.extend_from_slice(b"{\"tm\":100.0}");
            }
            out.extend_from_slice(&rec);
        }
        // 补足 next_off：留 8 字节余量，否则最后一根骨的矩阵会被"越过段尾"判掉
        let next = mdls_at + 9 + out.len() + 8;
        out[0..4].copy_from_slice(&(next as u32).to_le_bytes());
        d.extend_from_slice(&out);
        d.extend_from_slice(&[0u8; 8]);
        d
    }

    #[test]
    fn parses_skeleton_and_chains_world() {
        let sk = parse_skeleton(&synth()).expect("应能解析");
        assert_eq!(sk.version, "MDLS0002");
        assert!(!sk.bones.is_empty(), "至少解出一根骨");
        assert!((sk.bones[0].pivot[0] - 10.0).abs() < 1e-5);
        assert!(sk.bones[0].props.as_deref().unwrap_or("").contains("tm"));
        // 链条：若第二根读出来，其世界位置应是 (10,5)（在父的世界上再挪 5）
        if sk.bones.len() > 1 {
            assert!((sk.bones[1].pivot[0] - 10.0).abs() < 1e-4);
            assert!((sk.bones[1].pivot[1] - 5.0).abs() < 1e-4);
        }
    }
}

#[cfg(test)]
mod mesh_tests {
    use super::*;

    /// 造一个最小合法 MDLV：头 + 网格块（2 个三角形）+ MDLS 尾巴
    fn synth() -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(b"MDLV0017\0");
        // 填充一点"别的块"，让网格块不在固定偏移上（考验扫描）
        d.extend_from_slice(&[0xAB; 7]);
        let vertex_count = 4usize;
        let index_count = 6usize;
        let vb = (vertex_count * VERTEX_STRIDE) as u32;
        let ib = (index_count * 2) as u32;
        d.extend_from_slice(&0u32.to_le_bytes()); // 网格块前 4 字节（无意义）
        d.extend_from_slice(&vb.to_le_bytes()); // 顶点字节数
        for i in 0..vertex_count {
            let mut v = [0u8; VERTEX_STRIDE];
            let p = [i as f32 - 1.0, i as f32 * 0.5, 0.0];
            for (k, val) in p.iter().enumerate() {
                v[k * 4..k * 4 + 4].copy_from_slice(&val.to_le_bytes());
            }
            v[BLEND_OFFSET..BLEND_OFFSET + 4].copy_from_slice(&(i as u32).to_le_bytes());
            let uv = [0.1 * i as f32, 0.2 * i as f32];
            v[UV_OFFSET..UV_OFFSET + 4].copy_from_slice(&uv[0].to_le_bytes());
            v[UV_OFFSET + 4..UV_OFFSET + 8].copy_from_slice(&uv[1].to_le_bytes());
            d.extend_from_slice(&v);
        }
        d.extend_from_slice(&ib.to_le_bytes()); // 索引字节数
        for idx in [0u16, 1, 2, 0, 2, 3] {
            d.extend_from_slice(&idx.to_le_bytes());
        }
        d.extend_from_slice(b"MDLS");
        d
    }

    #[test]
    fn parses_synthetic_mesh() {
        let m = parse(&synth()).expect("应能解析");
        assert_eq!(m.version, "MDLV0017");
        assert_eq!(m.vertex_count(), 4);
        assert_eq!(m.triangle_count(), 2);
        assert_eq!(m.positions[1], [0.0, 0.5, 0.0]);
        assert!((m.uvs[3][0] - 0.3).abs() < 1e-6);
        assert_eq!(m.blend, vec![0, 1, 2, 3]);
        assert_eq!(m.indices, vec![0, 1, 2, 0, 2, 3]);
    }

    #[test]
    fn rejects_non_mdlv() {
        assert!(parse(b"NOTMDLV\0\0\0\0\0\0\0\0").is_err());
        assert!(parse(b"MD").is_err());
    }

    /// 造一个最小 MDLSv2 骨架：1 根骨，`local_bind` = I（散件姿态），
    /// `animation_reference` = 平移 (10,5,0)（装配姿态）。
    fn synth_mdls() -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(b"MDLS0002\0"); // 9
        d.extend_from_slice(&0u32.to_le_bytes()); // end_offset @9
        d.extend_from_slice(&1u16.to_le_bytes()); // bones_num @13
        d.extend_from_slice(&0u16.to_le_bytes()); // pad @15
        // 骨 0 @17
        d.extend_from_slice(b"\0"); // name ""
        d.extend_from_slice(&0i32.to_le_bytes()); // sim_type
        d.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // parent = none
        d.extend_from_slice(&64u32.to_le_bytes()); // size
        for i in 0..16 {
            d.extend_from_slice(&(if i % 5 == 0 { 1.0f32 } else { 0.0f32 }).to_le_bytes());
        }
        d.extend_from_slice(b"\0"); // sim json
        d.extend_from_slice(&0u16.to_le_bytes()); // extras
        d.push(1); // has_world_binds
        let mut r = [0.0f32; 16];
        r[0] = 1.0;
        r[5] = 1.0;
        r[10] = 1.0;
        r[15] = 1.0;
        r[12] = 10.0; // 列主序：平移在 [12],[13],[14]
        r[13] = 5.0;
        for v in r.iter() {
            d.extend_from_slice(&v.to_le_bytes());
        }
        d.extend_from_slice(&[0u8; 8]); // pad
        d
    }

    #[test]
    fn rig_assembles_from_animation_reference() {
        let rig = parse_rig(&synth_mdls()).expect("应能解出装配骨架");
        assert_eq!(rig.bones, 1);
        // 单骨：final = Rw·Wb⁻¹ = T(10,5)·I = T(10,5)。权重全给骨 0 ⇒ 点被平移。
        let q = rig.skin([0.0, 0.0, 0.0], [0, 0, 0, 0], [1.0, 0.0, 0.0, 0.0]);
        assert!((q[0] - 10.0).abs() < 1e-4, "x={}", q[0]);
        assert!((q[1] - 5.0).abs() < 1e-4, "y={}", q[1]);
        assert!(q[2].abs() < 1e-4);
        // 权重为 0 ⇒ 原样返回
        let q2 = rig.skin([3.0, 4.0, 0.0], [0, 0, 0, 0], [0.0; 4]);
        assert_eq!(q2, [3.0, 4.0, 0.0]);
    }

    #[test]
    fn mat4_inv_roundtrip() {
        // 一个带旋转+平移的仿射，验证 inv(m)·m == I
        let mut m = [0.0f32; 16];
        let (s, c) = (0.6f32, 0.8f32);
        m[0] = c;
        m[1] = s;
        m[4] = -s;
        m[5] = c;
        m[10] = 1.0;
        m[15] = 1.0;
        m[12] = 3.0;
        m[13] = -2.0;
        let inv = mat4_inv(&m).expect("可逆");
        let p = mat4_mul_col(&inv, &m);
        for i in 0..4 {
            for j in 0..4 {
                let want = if i == j { 1.0 } else { 0.0 };
                assert!((p[i * 4 + j] - want).abs() < 1e-4, "p[{i},{j}]={}", p[i * 4 + j]);
            }
        }
    }
}
