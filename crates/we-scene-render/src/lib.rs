// SPDX-License-Identifier: MIT
//! Wallpaper Engine `scene` 渲染器（可复用库）。
//! 能力：解析 `scene.pkg` / `scene.json` → 图层（图片→模型 json→材质 json→shader）→
//! **跑壁纸自带的 shader** → 逐趟特效链（`effect.json` 的 fbos / passes / bind / combos /
//! constantshadervalues）→ 回读。
//! 已定型的坑（每条都对应一个会让画面直接错掉的语义）：
//!
//! * GLSL ES 的 `#if 未定义宏` 非法 ⇒ combo 要补 0；但补 0 会让 `#ifdef` 变真 ⇒ 已补那批要 `#ifdef`→`#if`
//!
//! * combo 定义**必须两阶段共用**；**逐趟覆盖**要叠在 shader 声明的默认值之上
//!
//! * 矩阵**必须转置后上传**；`general.orthographic` **就是屏幕矩形**
//!
//! * `g_Texture{N}Resolution` 必须给（w,h,1/w,1/h），否则 UV 补偿全错
//!
//! * WE 的混合（`BLENDMODE=n`）大多在 **shader 里**算（`ApplyBlending`），不需要 GL blending
//!
//! * `effect.json` 的 `fbos[].scale=2` ⇒ 半分辨率；同一块"场景矩形"画到小 RT 上即可

pub mod gl_ctx;
pub mod math;
pub mod trace;
pub mod parallax;
pub mod mdl;
pub mod shader;
pub mod text;
pub mod video;

pub mod audio;
pub mod particles;
pub mod passtime;
mod profcls;
pub mod propscript;
pub mod fuse;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gl_ctx::Value;
use math::Mat4;

/// 对象在场景树里的**有效可见性**：自身可见 **且** 父链上每一级都可见。
/// WE 的场景对象是一棵**树**（`parent` 指向分组容器）。容器（`solid:true` + **没有** `image`）自己
/// 一个像素都不画 —— 它存在的唯一目的就是当分组：作者把一套图形挂在容器下，再把容器的 `visible`
/// 绑到某个用户属性（combo / bool）上，于是改属性 = **整棵子树**显隐。
/// 只判自身（[`we_scene::scene::Object::is_visible`]）会让“被隐藏容器的子对象”**照样画**。
/// 实测 `3704273480`：默认（`主题=1`）时容器 943 不可见，可它 16 个子对象照画，而它那套图形
/// 里有整屏背景/composelayer ⇒ 把 202（`主题=1`）那套**整块盖住**，表现为用户报的
/// “有两套图形但我们只有一个 + 参数-主题切换无效”（默认渲染与“只留 943 子树”**逐字节相同**）。
/// 这是**数据驱动**的：只看 `parent` 与每一级的 `visible`，不认任何对象名/属性名。
fn tree_visible(
    obj: &we_scene::scene::Object,
    by_id: &HashMap<i64, &we_scene::scene::Object>,
) -> bool {
    if !obj.is_visible() {
        return false;
    }
    let mut cur = obj;
    // 上限 64 级：链再深也不会是正常数据，且能防 `parent` 成环时死循环。
    for _ in 0..64 {
        let Some(pid) = cur.parent else { return true };
        let Some(p) = by_id.get(&pid) else { return true };
        if !p.is_visible() {
            return false;
        }
        cur = p;
    }
    true
}

/// 一趟的贴图来源
#[derive(Debug, Clone, Copy)]
enum Src {
    /// 离屏目标（按名字注册）
    Rt(usize),
    /// 上传好的图片贴图
    Tex(usize),
    /// 1x1 白：槽位没有对应资源时的兜底
    White,
}

struct Pass {
    layer: gl_ctx::GlLayer,
    /// None = 屏幕
    target: Option<usize>,
    units: Vec<(u32, Src)>,
    scalars: Vec<(String, Value)>,
    mvp: [f32; 16],
    /// 这一趟怎么和已有画面混合（对象图层用材质的 `blending`；写 RT 的内部趟一律 Opaque）
    blend: gl_ctx::Blend,
    /// 网格（puppet）图层的时间动画数据：每帧按 `t` 重算装配顶点并重传 VBO。
    /// `None` = 不是（或没有动画数据的）网格图层。
    mesh_anim: Option<MeshAnim>,
}

/// 网格图层的**时间动画**数据（"让网格人物动起来"）。
/// 顶点装配（蒙皮）本身与"建网格"是同一段代码 —— 差异只是时间：
/// `PuppetPose::assemble(t=0)` 给出建层时的几何，`t > 0` 给出动画帧的几何。
/// 层里存住原始 bind 顶点 + 蒙皮属性 + 归一化参数，就能每帧重算并重传（网格很小，
/// 本场景最大 5685 顶点 ⇒ CPU 代价可忽略）。
struct MeshAnim {
    name: String,
    pose: mdl::PuppetPose,
    bind: Vec<[f32; 3]>,
    bi: Vec<[u32; 4]>,
    bw: Vec<[f32; 4]>,
    /// 归一化参数（与建层时**同一算式**，见 [`mesh_norm_pos`]）
    hw: f32,
    hh: f32,
    dy: f32,
    flip_up: bool,
    /// `WE_CHAIN_TEXGRID`: 缓冲 = 贴图像素（没有富余）⇒ 越界顶点钳进 ±1。
    clamp01: bool,
    /// `animationlayers[].blend` 的脚本（`WE_SCRIPT_AUDIO=1` + `WE_BLEND_LIVE=1` 才有内容）：
    /// `(PoseLayer 下标, 脚本)`。每帧求值后写回 `pose` 的对应层。
    blend_scripts: Vec<(usize, propscript::NumScript)>,
    /// 上一次已经上传过的时间（同一 t 不重复算）
    last_t: f32,
}

impl MeshAnim {
    /// 归一化后的顶点（±1 空间，y 向下）——每帧上传给 VBO 的就是它。
    fn positions_at(&self, t: f32) -> Vec<[f32; 3]> {
        let asm = self.pose.assemble(&self.bind, &self.bi, &self.bw, t);
        asm.iter()
            .map(|p| mesh_norm_pos(p, self.hw, self.hh, self.dy, self.flip_up, self.clamp01))
            .collect()
    }
}

/// puppet 网格图层的**收尾合成几何**：**装配后**的顶点（每帧按 `t` 重算）+ 与底图趟
/// **共用**的 uv / 索引 / 摆锤参数。仅在 uv-空间方案（`mesh_uvspace_on`）下使用。
#[derive(Clone)]
struct MeshComp {
    pos: Vec<[f32; 3]>,
    uvs: Vec<[f32; 2]>,
    indices: Vec<u16>,
    sway: [f32; 5],
}

/// puppet 图层的**链缓冲（RT）是否跑在网格自己的 uv/图集空间**（默认**开**）。
///
/// # 为什么（用户报的 bug ②：「丝袜没有跟着腿动 + 右腿只有半截」）
/// puppet 图层有两套坐标，混用就出这个 bug：
///
/// * **RT/链空间** —— 效果链每一趟写/读的那张缓冲。参照实现
/// `linux-wallpaperengine CImage::updatePuppetPositionBuffer` 把网格顶点写成
/// `size/2 + raw`（raw 就是文件里的 bind 顶点），即 **`position == uv · size`** ⇒
/// **RT 里是"展开的图集布局"**，与图层自己的贴图、以及**作者画的混合贴图/遮罩同空间**。
///
/// * **显示空间** —— 逐帧蒙皮（装配）后摆进场景的样子。装配只该发生在**收尾合成趟**。
/// 我们以前把装配后的几何放进 RT ⇒ 混合贴图（在展开空间）与几何（在装配空间）**错位**。
/// 实测本场景（2887537617）：丝袜混合贴图 `底层2s` 的 alpha 包围盒归一化 x[-0.520,0.689]，
/// 与 **bind（展开）**几何 x[-0.536,0.711] 吻合（差 ≤0.022），而与**装配后**几何
/// 且网格一动几何就移、混合贴图（固定在展开空间）不动 = "腿一晃丝袜还在原地"。
fn mesh_uvspace_on() -> bool {
    std::env::var("WE_MESH_UVSPACE").ok().as_deref() != Some("0")
}

/// `WE_UVSPACE_ATLAS`（** 起默认开**）：uv-空间方案的**底图趟**不再画网格，而是
/// **平铺图集四边形 + 单位 MVP** ⇒ 链缓冲 **= 作者画布**（1 缓冲像素 = 1 贴图像素）。
///
/// # 为什么（承接 §2(b) 的定量结论）
/// 旧 uv-空间（以及"装配进 RT"的旧口径）里，底图趟画的是**网格**，而缓冲尺寸来自
/// 【对象矩形 ∪ 网格包围盒】的取景框。t 0 实测 `3704273480` `身体---拆分`：
///
/// * 网格取景框 ⇒ 缓冲 **2539×3570**；
///
/// * 作者的贴图/遮罩画布 ⇒ **2140×3500**（= `obj.size`）；
///
/// * 底图趟的 MVP 是「把取景框归一化到 ±1」⇒ 内容只占缓冲的 x∈[0.139, 0.861]；
///
/// * 链内每一趟的遮罩是**铺满缓冲**的全屏四边形（uv 0..1）。
/// ⇒ 内容与遮罩**不同源**：遮罩被横向拉宽 2539/2140 = 1.19 倍、且整体偏移 14%，
/// 于是滑板尖端那一点的遮罩被读到 240/255（原像素 0）。这是 里"翻 u/翻 v 都只是
/// 搬家"的机制解释：镜像不是主因，**空间不同源**才是。
///
/// # 修法（本开关）
/// 底图趟 = **整张图集**（`a_Position` = ±1 单位四边形、uv = 0..1、**单位 MVP**），
/// 缓冲 = 贴图像素尺寸 ⇒ 缓冲里就是作者画的那张画布，与遮罩/混合贴图**逐像素同空间**。
/// 于是 `WE_MASK_VFLIP` / `WE_MASK_UFLIP` 整族补偿**不再需要**（它们仍保留做 A/B）。
/// 关键点：这条路上**装配几何只出现在收尾合成趟**（用网格自己的 uv 采样图集缓冲）——
/// 这正是参照实现 `position == uv · size` 的语义，也顺手让"bind 顶点本来就是装配姿态"
/// 的模型（如 `3704273480`，残差 0.746）**不必再被 `mesh_is_atlas` 拒之门外**：
/// 网格 uv 本来就是图集坐标，装配后的顶点 × 图集 uv = 正确的蒙皮采样。
/// 子开关（A/B 用）：
///
/// * `WE_UVSPACE_ATLAS_FLIP=1` —— 底图四边形用 V 反向那一套（缓冲里存镜像的画）；
///
/// * `WE_UVSPACE_ATLAS_SIZE=obj` —— 缓冲退回 `obj.size`（默认取贴图像素尺寸）；
///
/// * `WE_UVSPACE_SCOPE=all|narrow|old` —— 作用域（见 [`uvspace_scope`]）。
///
/// # 为什么**默认开**（把主题1 的哈希变化认定为修 bug，不是回归）
/// 只知道"新路径治好了主题2 的滑板尖"，并留了个疑虑：它同时改了主题1（无文字层
/// 层 **382**），`b5bb6ba9` vs `5b3b1908`。 用**遮罩探针 + 缓冲转储**把两件事定死
///
/// 1. **遮罩是画在图集空间里的**（不是画在"装配后的画面"里）。证据是 iris 遮罩
/// `064d2302` 的两个亮点（图集 x[0.72,0.82] y[0.92,0.94]）正落在**眼睛**上；
/// 面侧遮罩 `2952a4b3/85ca92f1` 落在脸颊两侧；`84ece252` 落在那块**纱衣/披纱**上。
///
/// 2. **旧路径把遮罩当"画面坐标"用**：底图趟画的是装配后的网格（缓冲 = 网格取景框），
/// 而链内每一趟的遮罩是铺满缓冲的四边形 ⇒ 遮罩的图集坐标被当成**取景框坐标**。
/// 实测（`WE_DUMP_RT` + `WE_FX_PROBE=mask`，`WE_ONLY_ID=382`）：遮罩总能量只有
/// **4.9%** 落在内容像素上（新路径 **27.5%**）；单张看更刺眼——
/// `iris` **0.0% → 100%**、`waterripple` **0.0% → 100%**、纱衣 `84ece252`
/// **0.0% → 31.4%**（其余是软渐变落在图集的透明留白上）。
///
/// 3. **主题1 的旧路径本来就在抖**（本次新量，此前只量过主题2）：t=0→5 的动作量
/// 尖 **11.86**（新 0.83）、髋 **18.13**（新 0.11）。只因为旧路径把那张"板尖附近"
/// 的遮罩 `03fcd88f` 恰好糊在板缘上，主题2 才更早被人看见。
/// ⇒ 两层的遮罩都被读到了**不属于它的身体部位**；主题1 的像素变化是**修复的副作用**
/// （遮罩终于落在纱衣/头发/眼睛上），不是构图或位置变化（整数位移搜索最优 (0,0)，
/// 质心/锐度只升不降）。
fn uvspace_atlas_on() -> bool {
    // 肚子的肉/胯的特效也跟着错位。⇒ 恢复默认**关**；等"保留网格 + 只修遮罩空间"的新方案。
    match std::env::var("WE_UVSPACE_ATLAS").ok().as_deref() {
        Some("1") | Some("on") | Some("true") => true,
        _ => false,
    }
}


/// `WE_UVSPACE_SCOPE` —— `WE_UVSPACE_ATLAS` 的**作用域**（默认 `narrow`）。
///
/// * `narrow`（默认）—— **只**给"链把**另一个图层的画布**当输入"的层走（判据是**层次特征**，
/// 不是名字/id/壁纸 id）：该层任一特效率趟的纹理里有 `_rt_*`（链接 RT）。这类层的链里
/// 同时存在三种**画布空间**的东西——内容、作者画的遮罩、被引用层的画布；而链缓冲在我们的
/// 口径下是【对象矩形 ∪ 网格包围盒】的取景框 ⇒ 三者不同源，遮罩被读到别的身体部位上。
/// `3704273480` 的 382/1016 都命中（`effects/blend` 采样 `_rt_imageLayerComposite_350_a`）。
///
/// * `all` —— 每个够格的网格链层都走。**不是安全的默认**：实测反例 `3571777169` 的
/// `девка розбив`（同 97.4%×102.7% uv 覆盖、无链接 RT）在 `all` 下被**横向压扁、头发缺失**
/// ⇒ 图集底图对"没有一个已证的画布空间输入"的层没有收益，只有风险。
///
/// * `old` —— 等价于 `WE_UVSPACE_ATLAS=0`（保留这个名字是为了让回退只有一处开关语义）。
fn uvspace_scope() -> UvSpaceScope {
    match std::env::var("WE_UVSPACE_SCOPE")
        .ok()
        .map(|v| v.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("all") | Some("1") | Some("on") | Some("true") => UvSpaceScope::All,
        Some("old") | Some("0") | Some("off") | Some("false") | Some("no") => UvSpaceScope::Old,
        _ => UvSpaceScope::Narrow,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum UvSpaceScope {
    All,
    Narrow,
    Old,
}

/// 网格的 bind 顶点**是不是"展开图集布局"** —— 判据就是 skill/文档里那条实证等式
/// `pos == ((u−0.5)·sizeX, −(v−0.5)·sizeY)`（本机 A 的 6 个部件残差 0.000）。
/// 返回**按对象尺寸归一化后的最大残差**（0 = 完全吻合；0.75 = 差了 3/4 个对象尺寸）。
///
/// # 为什么必须拿它当门控（修 3704273480 的"人物散架/头乱/衣服没了"）
/// uv-空间方案的前提是：**文件里的 bind 顶点就是"把美术按图集平铺"的那套坐标**
/// ⇒ 链缓冲里放的就是"展开图集"，于是
/// ① 作者画的混合贴图/遮罩（丝袜 `底层2s` 这类）与几何**同空间**；
/// ② 收尾合成趟用**网格自己的 uv** 采样这张缓冲 = 正确的蒙皮采样。
/// 可**不是每个 puppet 都满足这条**。实测 `3704273480` 的 `身体---拆分`
/// （MDLV0023，11506 顶点）：最大残差 **2610.6 像素 = 0.746 个对象尺寸**、
/// 装配位移 **0.000** ⇒ 文件里的顶点**本来就是装配好的姿态**，不是图集。
/// 对这种模型套 uv-空间方案 ⇒ 链缓冲里是**装配好的画**，收尾趟却按**网格 uv** 去采样
/// ⇒ 每个部件的采样区整体错开（实测：头飞出去、比基尼错位、脚脱离，
/// 半透明衣物（`effects/blend` 的 `OPACITYMASK`）也一起错位 ⇒ "没了"）。
/// 旧口径（装配进 RT + 四边形收尾）对这类模型**本来就是对的**（逐字节对照见本次报告）。
/// ⇒ 门控按**文件自己陈述的事实**取，不按对象名、不按壁纸 id（其它 16 条壁纸零影响）。
fn mesh_atlas_residual(positions: &[[f32; 3]], uvs: &[[f32; 2]], size: [f32; 2]) -> f32 {
    let scale = size[0].abs().max(size[1].abs()).max(1.0);
    let mut maxr = 0.0f32;
    for (i, p) in positions.iter().enumerate() {
        if let Some(uv) = uvs.get(i) {
            let ex = (uv[0] - 0.5) * size[0];
            let ey = -(uv[1] - 0.5) * size[1];
            maxr = maxr.max((p[0] - ex).abs()).max((p[1] - ey).abs());
        }
    }
    maxr / scale
}

/// 残差门限：2%。实测两端相距极远（图集类 0.000，装配类 0.746）⇒ 阈值位置不敏感。
/// `WE_MESH_UV_ATLAS=force` 强制认为"是图集"（A/B 用），`=off` 强制认为"不是"。
fn mesh_is_atlas(residual: f32) -> bool {
    match std::env::var("WE_MESH_UV_ATLAS").ok().as_deref() {
        Some("force") => true,
        Some("off") => false,
        _ => residual < 0.02,
    }
}

/// `WE_FX_CARDSPACE`：把**特效链内趟**的顶点空间换成官方"卡片网格"空间。
///
/// # 缺口
/// 官方给图像对象**每一趟**的顶点几何是 `GenCardMesh` 的 **`±size/2` 对象像素空间**
/// （`open-wallpaper-engine/src/Scene/Pkg/Parse/Scene/SceneGeometryBuilder.cpp:19-38`；
/// 特效趟的网格在 `.../Scene/Scene/SceneNodeLayer.cpp:88-109` 被换成同一张卡片网格），
/// 我们一律用 **`±1` 单位四边形 + 把 `scale(size/2)` 折进 MVP**。
/// 顶点位移 `δ` 因此被放大 `(size/2)` 倍 ⇒ 在官方 `foliagesway` preview 自己的
/// `Strength=93.34` 下整帧被推出画外（`sweep/` 实测 `S≈6~8` 就全黑）。
///
/// # 等价关系（可证）
/// 设一趟的 `mvp` 数组为 `A`，则驱动实际算的是 `gl_Position = M(A)·v`，其中 `M(A)` 是
/// "以 `A` 为 `m` 的那个数学矩阵"（约定见 `math.rs` 顶注 + `gl_ctx::t4`）。
/// 卡片顶点 `v_card = D·v_unit`（`D = diag(size.x/2, size.y/2, 1)`）⇒
/// 要 `M(A')·D·v_unit = M(A)·v_unit`，只需 `M(A') = M(A)·D⁻¹`
/// ⇒ `A' = (Mat4{m:A} · Mat4::scale(2/size.x, 2/size.y, 1)).column_major`。
/// **没有顶点位移时两套几何逐位等价**（`size` 是 2 的幂时更是一个位都不差）。
///
/// # 边界（不在本开关内）
///
/// * `fullscreen` 的整屏后处理趟：官方文档 `open-wallpaper-engine/docs/scenejson.md:5`
/// 把"**signed unit quad**"写成 `fullscreen` 的**特例语义**，而且那种 shader 直接
/// `gl_Position = vec4(a_Position,1.0)`（`blur_precise_gaussian.vert` 的 `#else VERTICAL`）
/// ⇒ 只能保持单位四边形（`make_layer_card` 的 `card_ok` 会拒掉它们）；
///
/// * 画布空间的链（`WE_RECT_CHAIN=0`）：那时链缓冲 = **画布**，"1 个 shader 单位 = 1 个对象像素"
/// 无从谈起（官方根本没有这条路）⇒ 本开关**不动**它（作用域见调用点的 `fx_cards_here`）。
/// （↑ 以上是 `WE_FX_CARDSPACE` 的说明 —— 它历史上就误挂在这个位置上方，和本函数无关；
/// `fx_cardspace_on` 自己也有一行注释。）
/// WE_TEXRES（**默认开** = 新行为）：`g_Texture{N}Resolution` 按**逐贴图真实尺寸**上报。
///
/// * 布局 = 官方四分量 `(源宽, 源高, 采样宽, 采样高)`（`gl_ctx::UnitTex` 的注释列了一手出处）；
///
/// * 每张图各自的尺寸：贴图 = 上传尺寸；RT = **该图层自己的画布**（`rt_logical`，见 `build` 里
/// 的赋值点）而不是输出窗口/物理缓冲。
/// 缺口（本开关修的就是它）：`effects/blend` 的 `blend.vert` 拿 `g_BlendOffset`（**像素**）除以
/// `g_Texture0Resolution.zw`、并乘 `scaleA = g_Texture0Resolution.zw / g_Texture1Resolution.zw`
/// ⇒ 我们以前把**输出画布**（1080p = 1920x1080）当成图层缓冲尺寸上报，而 WE 报的是**图层自己
/// 的画布**（`3704273480` 的文字层 691 = 1804x1344）⇒ 字里透出的角色被放大（1080p 实测
/// 纵向 1.24×，且纵横比失真），表现就是"玻璃字里的角色比前景人物大一圈"。
/// `WE_TEXRES=0`（旧名 `WE_TEX_EXTENT=0` 同义）⇒ **逐字节**回旧行为。
pub fn per_tex_res_on() -> bool {
    static F: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *F.get_or_init(|| {
        for k in ["WE_TEXRES", "WE_TEX_EXTENT"] {
            if let Ok(v) = std::env::var(k) {
                return !matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "off" | "no" | ""
                );
            }
        }
        true
    })
}

/// WE_TEX_EXTENT（旧名，语义已被 [`per_tex_res_on`] 吸收）：是否让 RT 上报**图层自己的画布尺寸**。
fn tex_extent_on() -> bool {
    per_tex_res_on()
}

/// `WE_TEXRES_PASS`（**默认开**）：RT 槽上报**该 RT 的真实像素尺寸**（`WE_TEXRES_PASS=0` 回旧）。
/// 依据（两份重实现逐行同构)/(§3.3)）：
/// 一趟特效 pass 的"目标尺寸 / 四边形 / 上报的 `g_Texture0Resolution`"三者必须逐像素相等。
/// 我们以前让 RT 槽报 `rt_logical`（图层自己的 `obj.size`）而真实缓冲是输出分辨率 ⇒
/// `effects/blend` 的 `m = g_BlendScale · quad_h / T0_h` 同时错比例与位置。
fn texres_pass_on() -> bool {
    // **默认开**（= 官方铁律②：`T0` 就是本趟绑定的那张纹理的像素尺寸）。
    // 上次把它默认关掉是因为"两个开关同开时字带变纯白"，根因是当时文字层的 RT
    // 仍是 `obj.size`（=`g_Texture0Resolution` 与排版矩形无关）；本单把文字层 RT
    // 改成官方的 `墨迹 bbox + 2×padding` 之后，`T0 ≡ RT` 才是自洽的。
    // `WE_TEXRES_PASS=0` 仍可逐字节回旧（上报 `rt_logical`）。
    env_flag("WE_TEXRES_PASS", true)
}

/// `WE_TEXT_CHAIN_LAYER`（**默认开**）：文字层（挂特效链）的整条链跑在**文字画布空间**。
/// 官方构造（CaptSilver `WPSceneParser.cpp:4024-4034` + `:4094-4099` + `SceneImageEffectLayer.cpp:114-127`，
/// OWE `SceneTextObjectParser.cpp:98-123` 同构）：
/// 链缓冲 = 光栅化画布 `texW × texH`、特效相机 = `ortho(−W/2..W/2)`、特效卡 = 同尺寸居中卡片、
/// 收尾趟用作者的 node 变换摆回场景 ⇒ **RT 尺寸 ≡ 卡片尺寸 ≡ 上报的 `g_Texture0Resolution`**。
/// 我们以前把文字层的链跑在**画布空间**（缓冲 = 输出分辨率）却上报 `obj.size` ⇒ 同一条等式被破坏。
/// `WE_TEXT_CHAIN_LAYER=0` ⇒ 逐字节回旧（链跑画布空间）。
fn text_chain_layer_on() -> bool {
    // **默认开**。链缓冲 = 文字层自己的 RT（= 官方铁律①的
    // `墨迹 bbox + 2×padding`），首趟把该矩形铺满缓冲、收尾趟用同一矩形摆回场景
    // ⇒ `RT 尺寸 ≡ 卡片尺寸 ≡ T0`。`WE_TEXT_CHAIN_LAYER=0` 仍可逐字节回旧。
    env_flag("WE_TEXT_CHAIN_LAYER", true)
}

/// 官方铁律①（**默认开**）：文字层 RT = 「墨迹 bbox + 2×padding」。
/// 依据：
/// `TextLayer::vtable+0x128` = `FUN_140258ab0`：
/// ```c
/// w = [typesetter+0x98] - [typesetter+0x90]; // 排版矩形
/// h = [typesetter+0x9c] - [typesetter+0x94];
/// pad.x = min([this+0x4e8], 512); pad.y = min([this+0x4ec], 512);
/// out = (w + 2*pad.x, h + 2*pad.y); // 再 max(4, …)
/// ```
/// 函数里**没有**任何输出分辨率 / `backbuffer` / `renderScale` 参与 ⇒ 尺寸只来自层自己。
/// `obj.size` 只是**排版可用区**（对齐/换行用），不是 RT 尺寸。
/// `WE_TEXT_RT=legacy`（或 `0`/`off`）⇒ 回旧行为（RT = `obj.size × WE_TEXT_SCALE`）。
fn text_rt_official() -> bool {
    match std::env::var("WE_TEXT_RT") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "legacy" | "old" | "0" | "false" | "off" | "no"
        ),
        Err(_) => true,
    }
}

/// `WE_BLEND_SPACE`（**默认 `layer`** = 官方口径；`canvas` / `0` = 旧行为，逐字节可复现）。
/// 官方 `effects/blend`（`TRANSFORMUV=1`）的 `blend.vert` 把 `a_TexCoord`（0..1）**当作图层的 uv**
/// 用：那一趟在 WE 里跑在该图层自己的缓冲区上（`EnsureLinkRenderTarget` 用 `source_node.Size`
/// 分配 ⇒ `uv` 0..1 正好铺满图层矩形）。我们这条路上这一趟跑在**画布**四边形上
/// （实测 `WE_FX_PROBE=uv`：uv 从 (0,0) 到 (1,1) 铺满整屏）⇒ 同一个 uv 被当成图层 uv，
/// 内容被拉伸 `画布/图层` 倍，且映射的不动点从"图层中心"变成"画布中心（= 屏中心）"。
/// 换算只用场景数据：`anchor = 0.5 + origin/canvas`（y 轴向上）、`ratio = canvas/(size·scale)`。
/// `3704273480` 文字层 691：anchor=(0.42807, 0.21574)、ratio=(1.8229, 1.3764) ⇒ 只改这一处，
/// 字里人物纵向 **上移 ≈307px**（1080p），横向 1.82× 的拉伸同时被消掉。
fn blend_space_on() -> bool {
    env_flag("WE_BLEND_SPACE", true)
}

/// `WE_FX_TIME_SCALE`（**默认 1.0 = 官方口径**；>1 更快、<1 更慢、`0` = 把 shader 时间冻在 0）。
/// **这是"用户偏好"旋钮，不是正确性修复。** 依据：本场景所有时间驱动特效（`waterwaves`
/// `waterripple` `shake` `iris` `waterflow` `foliagesway` `shimmer` `pulse`）在官方 shader 里
/// **全部直接吃 `g_Time`（秒）**，一个 `×fps` / `×1/30` / `×const` 都没有：
/// ```glsl
/// waterwaves.frag : float distance = g_Time * g_Speed + dot(uv, dir) * g_Scale;
/// texCoord += sin(distance) * strength…… strength = g_Strength * g_Strength
/// waterripple.vert: v_TexCoordRipple.xy = uv + g_Time * g_AnimationSpeed² + scroll
/// shake.frag : float time = g_Speed * g_Time + flowPhase; offset = sin(frac(time/M_PI_2)*M_PI_2)
/// iris.vert : float time = g_Time * g_Speed + g_PhaseOffset
/// ```
/// 我们下发的 `g_Time` 就是 `self.time`（场景秒，墙钟驱动、`--t` 时冻结），**逐字一致** ⇒
/// 默认 1.0。这个旋钮只乘 **shader 的 `g_Time`**（网格动画用 `mesh_time`、粒子用 `dt`，都不受影响）。
fn fx_time_scale() -> f32 {
    env_f32("WE_FX_TIME_SCALE", 1.0).clamp(0.0, 8.0)
}

/// 通用布尔开关（缺省 `default`；`0/false/off/no/空` = 关）。
/// 语义与散落在各处的老写法一致（逐项 A/B 的老口径），只是收成一处，免得每个新开关
/// 都抄一遍 match 分支写错一个字符串。
fn env_flag(name: &str, default: bool) -> bool {
    std::env::var(name)
        .map(|v| {
            !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no" | ""
            )
        })
        .unwrap_or(default)
}

/// `WE_CHAIN_TEXGRID`（默认关）：puppet 网格层的链缓冲 = 贴图像素格子。
/// 官方口径：image 层的链/链接/合成 RT = 贴图像素
/// ÷ 场景降采样因子，从不读画布或输出分辨率。我们为修"腿上直线切边"用的【对象矩形 ∪ 网格
/// 包围盒】取景框（本场景 2539x3570）比图集格子（2140x3500）各向异性 1.186x1.02 ⇒ 遮罩/位移场
/// 被拉伸。这个开关取消取景框：缓冲恒 = obj.size（越界几何默认由缓冲裁掉，`_CLAMP=1` 才夹进 +-1）。
/// `WE_CHAIN_TEXGRID_CLAMP=1` 另把越界顶点钳进 +-1 做 A/B；默认 0 = 官方口径（越界部分由缓冲边界裁掉）。
fn chain_texgrid_on() -> bool {
    env_flag("WE_CHAIN_TEXGRID", false)
}

/// `WE_CHAIN_TEXGRID` 是否同时钳顶点（默认关 = 官方口径）。
fn chain_texgrid_clamp() -> bool {
    chain_texgrid_on() && env_flag("WE_CHAIN_TEXGRID_CLAMP", false)
}

/// `WE_PERF_SCISSOR`（默认**关**）：逐趟 `glScissor` 到该 RT 的**内容包围盒**。
/// 只跳过"本来就全是 0"的像素 ⇒ 输出与开关前**逐字节相同**（前提：包围盒保守，
/// 探针的 padding 见 [`SceneRenderer::probe_content_boxes`]）。省的是实打实的
/// 着色/混合带宽：本场景链 RT 的内容覆盖率 46%（1016）/ 20%（350）。
/// `WE_PERF_RES_SCALE`（默认**关**）：把**超大**链 RT 按"屏幕像素/画布像素"降采样，
/// 但 `g_Texture{N}Resolution` 仍按**逻辑**尺寸上报 ⇒ 视觉尺度不变，只少采样细节。
/// 判据（数据驱动，不看 id/名字）：RT 面积 ≥ 6 MP（本场景 350=7.5MP / 1016=9.1MP /
/// 2094=8.3MP 命中；1583=2.9MP、691=2.4MP、1496=3.7MP 不命中 ⇒ 文字遮罩那些层不受影响）。
/// 缩放系数 `s = min(1, 1.15 · out_w / 画布宽)`：本机 2560/3840 ⇒ s=0.767，也就是
/// 这类层的链缓冲**恰好略大于它在屏上的实际脚印**（1693 px 宽）——再高就是纯超采样。
/// 遮罩烘焙缓冲的边长系数（`WE_PERF_MASK_SCALE`，默认 1.0 = 不缩）。
/// `WE_PERF_MASK_HALF=1` 等价于 `WE_PERF_MASK_SCALE=0.5`（老开关，保留）。
fn perf_mask_scale() -> f32 {
    static V: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        if let Ok(s) = std::env::var("WE_PERF_MASK_SCALE") {
            if let Ok(v) = s.trim().parse::<f32>() {
                return v.clamp(0.1, 1.0);
            }
        }
        if env_flag("WE_PERF_MASK_HALF", false) {
            0.5
        } else {
            1.0
        }
    })
}

fn perf_res_scale_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| env_flag("WE_PERF_RES_SCALE", false) || perf_scale_post_on())
}

/// `WE_PERF_SCALE_POST`（默认**关**）：只给**整屏后处理层**（链缓冲 = 画布尺寸，
/// 画布 > 输出）按\"屏上 1:1\"降采样。
/// 为什么这一档比 [`perf_res_scale_on`] 的通用降采样安全得多：这类层的**输入**就是
/// 已经渲染好的场景 RT（= 输出尺寸），链缓冲却按**画布** 3840x2160 建 ⇒ 8.3 MP 里
/// 每一个像素都是从 2560x1440 上采样出来的，**不含任何源里没有的细节**；再把它
/// 合成回 2560x1440 时又全部丢掉。降回屏上尺寸后\"上采样→调色→下采样\"退化成
/// \"调色\"，丢掉的只有 `clamp`/LUT 之后再重采样的相位差，不动任何 1–2 px 的
/// 高频几何（发丝/细带/环扣都在**别的层**里）。
fn perf_scale_post_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| env_flag("WE_PERF_SCALE_POST", false))
}

/// `WE_PERF_MASK_SRC`（默认**关**）：遮罩烘焙缓冲**不超过源贴图的像素尺寸**
/// （逐轴独立，因为烘焙趟按归一化 uv 铺满缓冲 ⇒ 两轴互不影响）。
/// 为什么不是\"画质损失\"而是\"没必要的超采样\"：烘焙趟就是把作者画的遮罩贴图
/// （本场景 1070x1750）按网格几何 `texSample2D` 画一遍；旧缓冲 2539x3570 是它的
/// 2.37×1.99 倍 ⇒ 每个源纹素被**上采样**到 4.7 个缓冲像素，下游再按归一化 uv
/// 采回来。降到源尺寸后 1 缓冲像素 ≈ 1 源纹素，烘焙本身就是双线性采样，
/// 两次滤波变成一次。省的是遮罩烘焙那 9 趟的着色/带宽。
fn perf_mask_src_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| env_flag("WE_PERF_MASK_SRC", false))
}

/// `WE_PERF_SUPERSAMPLE=f`（默认**不设** = 现行为）：骨骼（puppet/网格）层链 RT 的
/// **超采样系数**，口径 = *链缓冲像素 / 该层在屏上的脚印像素*。
/// 本场景 obj1016 的链缓冲 2539x3570、屏上脚印 1693x2380（2560x1440 输出）⇒ 现值 **1.5**。
/// 设 `f` 后缓冲按 `s = f · 屏上宽 / 逻辑宽` 缩小，`g_Texture{N}Resolution` 仍报**逻辑**
/// 尺寸（和 [`perf_res_scale_on`] 同一套口径）⇒ blur/遮罩步长的相对尺度一分不变，
/// 变的只有"每个屏上像素被采几次"。`f = 1.0` 就是屏上 1:1（= 的 `RES_SCALE+SCALE_MESH`
/// 只作用于 `puppet_mesh_h.is_some` 的层；其余层一律不动（不隐含 `RES_SCALE`）。
fn perf_supersample_mesh() -> Option<f32> {
    static V: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        let f = env_f32("WE_PERF_SUPERSAMPLE", 0.0);
        if f.is_finite() && f > 0.0 {
            Some(f.clamp(0.2, 4.0))
        } else {
            None
        }
    })
}

/// `WE_PERF_SUPERSAMPLE_MASK=f`（默认**不设** = 现行为）：遮罩烘焙缓冲的**超采样系数**，
/// 口径 = *缓冲像素 / 源贴图像素*（逐轴独立，同 [`perf_mask_src_on`]）。
/// 现值 = 2539/1070 = **2.37**。
/// 数字越小缓冲越接近作者画的遮罩原图，烘焙越锐 —— 屏上\"画面不变\"的边界在哪由实测给。
/// 注意 `f` 先乘源尺寸再**向下夹到逻辑尺寸** ⇒ 只有 `f ≥ 2.38` 才逐字节等于现行为
/// （`f=2.37` 会得到 2536 ≠ 2539）。
fn perf_supersample_mask() -> Option<f32> {
    static V: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        let f = env_f32("WE_PERF_SUPERSAMPLE_MASK", 0.0);
        if f.is_finite() && f > 0.0 {
            Some(f.clamp(0.25, 4.0))
        } else {
            None
        }
    })
}

/// 超采样系数 → **骨骼层链缓冲的缩放**：`s = clamp(f · 屏上宽 / 逻辑宽, 0.05, 1)`。
/// `f` = 缓冲像素 / 屏上脚印像素（本场景 1.5 = 现行为 ⇒ `s=1`）；
/// 抽成纯函数是为了让"系数 → 物理尺寸"这条换算能被单测钉住（`build` 里只做调用）。
fn supersample_chain_scale(f: f32, disp_w: f32, lw: u32) -> f32 {
    (f * disp_w / lw.max(1) as f32).clamp(0.05, 1.0)
}

/// 超采样系数 → **遮罩烘焙缓冲尺寸**（逐轴 `f × 源贴图像素`，再夹到逻辑尺寸以内）。
fn supersample_mask_dims(f: f32, sw: u32, sh: u32, lw: u32, lh: u32) -> (u32, u32) {
    (
        ((sw as f32 * f).round() as u32).max(1).min(lw.max(1)),
        ((sh as f32 * f).round() as u32).max(1).min(lh.max(1)),
    )
}

/// `WE_PERF_MASK_DEDUPE`（默认**关**）：同一帧内"同一张遮罩烘焙 RT + 同一张源贴图"的
/// 重复烘焙只跑第一趟。
/// 本场景实测**没有收益**（9 趟烘焙 → 仍是 9 趟：12 个特效用到的遮罩是**不同贴图**，
/// 共用 1–2 张 RT 但源不同 ⇒ 键 (RT, 源) 两两不同）。留着是因为判据是数据驱动的、
/// 对"同一张遮罩被多个 pass 重复烘"的场景有效，且开/关在本场景逐字节相同；默认关。
fn perf_mask_dedupe_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| env_flag("WE_PERF_MASK_DEDUPE", false))
}

fn perf_scissor_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| env_flag("WE_PERF_SCISSOR", true))
}

/// `WE_PERF_SCISSOR_PP`（默认**关**）：在「逐 RT 内容包围盒」之上再加一层**逐趟**包围盒。
/// 为什么：`WE_PERF_SCISSOR` 给的是**整层**的内容范围（同一个对象的所有趟共用一个矩形）。
/// 但一层里不同特效趟真正落笔的范围可以差很多 —— 有的只画遮罩覆盖的那块，有的把上一趟
/// 整张搬过去。逐趟探一次「这一趟自己写到哪」，剪得比整层更紧 ⇒ 同样的 `glScissor` 语义
/// （跳过一定为 0 的像素）只省更多，不引入任何新的近似。
/// 探针在**第 1 帧画完之后**跑（见 [`SceneRenderer::probe_pass_boxes`]）：那时各链 RT 里
/// 是真实内容，而特效趟的输入正是那些 RT —— 在建树时探，输入还是全 0，探出来必然错。
fn perf_scissor_pp_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| env_flag("WE_PERF_SCISSOR_PP", false))
}

/// 逐趟剪裁的**固定余量**（像素，默认 32）：盖住探针格量化 + 一帧内的亚像素漂移。
fn perf_scissor_pp_pad() -> i32 {
    static V: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    *V.get_or_init(|| env_f32("WE_PERF_SCISSOR_PP_PAD", 32.0).clamp(0.0, 512.0).round() as i32)
}

/// 通用浮点开关（缺省 `default`；解析失败/非有限 ⇒ `default`）。
fn env_f32(name: &str, default: f32) -> f32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|f| f.is_finite())
        .unwrap_or(default)
}

/// `s` 里从 `marker` 起、到**第一个 `;`（含）**为止的字节区间 `(start, end)`；缺任一项 ⇒ `None`。
/// 用途：改一行 GLSL 语句而不猜它的缩进/换行（`fs.replace` 要求整串逐字匹配，
/// 而外部 shader 的缩进是 `\t` 还是空格、有没有尾随空格都不可控）。
fn stmt_span(s: &str, marker: &str) -> Option<(usize, usize)> {
    let start = s.find(marker)?;
    let end = s[start..].find(';')? + start + 1;
    Some((start, end))
}

/// `WE_T1_REPORT=WxH[@pass]`（**诊断开关，默认关 = 一个字节都不动**）：
/// 只覆盖 **texture unit 1** 的 `g_Texture1Resolution` **上报值** —— 不动缓冲、不动四边形、
/// 不动 RT 内容。语义上它改的正是 `effects/blend`（`TRANSFORMUV=1`）里的 **`T1`（遮罩槽）**：
/// ```text
/// v.zw -= (g_BlendOffset/red - (T0.zw - T1.zw)*0.5) / T0.zw // ← T1 只在这里出现
/// v.zw *= (T0.zw / T1.zw) / g_BlendScale
/// ```
/// ⇒ 平移项对 `T1` **线性**、放大率对 `T1` **无关**（换纹素后 `T1` 在 `T0/(T1·BS)·T1` 里互消）。
/// 这条开关就是用来量 `dy(T1_h)` 的：其余量全部固定，只看这一个数。
/// `@pass`（可选）= 只在该趟生效（本场景文字层 blend 趟 = 24；缺省 = 所有 unit1 绑 RT 的趟）。
fn t1_report_override() -> Option<(u32, u32, Option<usize>)> {
    static OV: std::sync::OnceLock<Option<(u32, u32, Option<usize>)>> = std::sync::OnceLock::new();
    *OV.get_or_init(|| {
        let raw = std::env::var("WE_T1_REPORT").ok()?;
        let (wh, pass) = match raw.split_once('@') {
            Some((a, b)) => (a, b.trim().parse::<usize>().ok()),
            None => (raw.as_str(), None),
        };
        let (w, h) = wh.trim().split_once(['x', 'X', '*'])?;
        let w: u32 = w.trim().parse().ok()?;
        let h: u32 = h.trim().parse().ok()?;
        (w > 0 && h > 0).then_some((w, h, pass))
    })
}

/// `WE_BLEND_T0`（**默认 `layer`** = 与之前逐字节一致；`scene` / 数字 = 实验口径）：
/// 文字层 `effects/blend` 那一趟的 `g_Texture0Resolution` 报**什么像素空间**。
/// 官方 `effects/blend/shaders/effects/blend.vert`（TRANSFORMUV=1 分支）整段数学是**像素**口径：
/// ```glsl
/// vec2 scaleA = g_Texture0Resolution.zw / g_Texture1Resolution.zw;
/// v_TexCoord.zw -= ((g_BlendOffset / g_TextureReductionScale)
/// (g_Texture0Resolution.zw - g_Texture1Resolution.zw) * 0.5)
/// / g_Texture0Resolution.zw;
/// v_TexCoord.zw *= scaleA / g_BlendScale;
/// ```
/// ⇒ 采样跨度 = `T0.zw / (T1.zw · blendscale)`，落在屏上的放大率 = `blendscale · quad_h / T0_h`。
/// 其中 **T1 = 图层链接 RT = 1 像素 = 1 场景单位**（OWE `EnsureLinkRenderTarget` 用
/// `source_node.Size` 分配 RT），所以 T0 只有报**同一量纲（场景画布）**，`blendscale` 才是
/// 作者在编辑器里看到的那一个倍数。报 `obj.size`（旧行为）会把遮罩内容**再多放大 obj.scale 倍**
/// （`3704273480` 文字层 691：`blendscale 2.2 × obj.scale 1.16768 = 2.569`）⇒ 字里透出的角色
/// 比前景人物大一圈（用户判据：win 版两者同比例）。
/// 返回值 = 要覆盖上报的 `(w, h)`；`None` = 不覆盖（回旧行为）。
fn blend_t0_report(size: (f32, f32), scene: (f32, f32)) -> Option<(f32, f32)> {
    // `WE_TEXRES=0`（旧口径总闸）⇒ 这一支也不动，保证旧基线可逐字节复现。
    if !per_tex_res_on() {
        return None;
    }
    let raw = std::env::var("WE_BLEND_T0").unwrap_or_else(|_| "layer".into());
    match raw.trim().to_ascii_lowercase().as_str() {
        "layer" | "obj" | "0" | "off" | "false" | "no" => None,
        "scene" | "" | "1" | "on" | "true" | "yes" => Some(scene),
        other => match other.parse::<f32>() {
            Ok(f) if f > 0.0 => Some((size.0 * f, size.1 * f)),
            _ => Some(scene),
        },
    }
}

/// WE_LINK_RT_NATIVE（ 实验开关，默认 0=关）：把被引用图层的链接 RT 按 WE 口径
/// 分配成该图层自己的尺寸（WE：_rt_imageLayerComposite_<id>_a/_b 的尺寸 = 图层 size，
/// 图层在 RT 里铺满 0..1）。旧行为 = 画布尺寸 + 画布空间摆位。
/// 1 = 图层尺寸 + 图层空间铺满 + 翻 V；2 = 同上但不翻 V；3 = 图层尺寸 + 仍画布空间摆位。
fn link_rt_native_mode() -> u32 {
    match std::env::var("WE_LINK_RT_NATIVE") {
        Ok(v) => match v.trim() {
            "1" => 1,
            "2" => 2,
            "3" => 3,
            _ => 0,
        },
        // 默认 = 2（与 数字验收的最佳档一致）。显式 `WE_LINK_RT_NATIVE=0` 仍关回旧行为。
        Err(_) => 2,
    }
}

/// WE_LINK_RT_CHAIN（ 实验开关，默认 0=关 ⇒ 逐字节旧行为）：把**不可见但被引用**的层
/// （= 只当遮罩/纹理喂给别的层那种；本场景 `3704273480` 的 `水珠` id=350）的特效链
/// 也放进**图层空间**（缓冲 = 图层自己的矩形、内容铺满 0..1）。
/// 依据：WE 的 `_rt_imageLayerComposite_<id>` 给消费方的就是"该图层自己的画布"
/// （ 实测入 RT 的落位由该趟 `obj_mvp` 决定； 实测把链接 RT 按图层尺寸 + 铺满 0..1
/// 分配后，消费方（文字层 691 的 `blend` 遮罩）字区颜色数 3810 → 10365）。
/// 而 `!published` 这条守卫把这类层按**画布空间**渲（缓冲 = 画布、内容按上屏摆位）
/// ⇒ **生产方空间与消费方采样口径不一致** = `lib.rs` 里那条"另一个待办"。
/// 取值：1 = 空间选择放开（`WE_RECT_CHAIN` / `WE_CHAIN_SPACE=layer` 那条 `layer_space`）；
/// 2 = 同 1，且卡片空间（`WE_FX_CARDSPACE` / `WE_BASE_CARDSPACE`）也放开；
/// 其余 / 未设 = 0 = 关。
fn link_rt_chain_mode() -> u32 {
    match std::env::var("WE_LINK_RT_CHAIN") {
        Ok(v) => match v.trim() {
            "1" => 1,
            "2" => 2,
            _ => 0,
        },
        // 默认 = 1（ 数字验收档）。显式 `WE_LINK_RT_CHAIN=0` 仍关回旧行为。
        Err(_) => 1,
    }
}

/// 作用域：**被引用 + 不可见**（可见的那种还牵着"回贴屏幕"那条路；自引用
/// （Sona / Clock 那几条 `_rt_imageLayerComposite_<自己>`）是 feedback，不在本项范围）。
fn link_rt_chain_here(published: bool, vis: bool) -> bool {
    published && !vis && link_rt_chain_mode() != 0
}

/// 的卡片空间子档（`WE_LINK_RT_CHAIN=2`）。
fn link_rt_chain_cards(published: bool, vis: bool) -> bool {
    link_rt_chain_here(published, vis) && link_rt_chain_mode() >= 2
}

/// `WE_LINK_SELF_RUN`（默认**开**）：图层自己的特效引用 `_rt_imageLayerComposite_<自己>`
/// 时，绑定**本链当前输出**（= 该特效开始时的 `prev`），而不是链接 RT 里那张静态底图。
/// 依据（A 级，`open-wallpaper-engine/src/Scene/Pkg/Parse/Image/SceneImageObjectParser.cpp:806-812`）：
/// ```cpp
/// // A layer's own effect referencing its composite wants this layer's running chain result.
/// for (auto& t : wpmat.textures) {
/// auto composite_id = ParseImageLayerCompositeId(t.as_str);
/// if (composite_id.is_some && *composite_id == wpimgobj.id) t = effect_composite.clone;
/// }
/// ```
/// `effect_composite` 就是本层特效链**运行中的结果**（同一函数 638/698 行：每个特效的输入都是它，
/// 输出也写回它）。旧行为把自引用解析到 `alias_rt` 登记的那张 `comp`（= 特效前的底图）
/// ⇒ 链上**第 2 个及以后**的自引用（`shine_combine` / `localcontrast` / `godrays`）拿到的是"没跑过
/// 前面特效的原图"，与官方语义不同。`WE_LINK_SELF_RUN=0` 精确回旧行为。
fn link_self_run_on() -> bool {
    std::env::var("WE_LINK_SELF_RUN")
        .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"))
        .unwrap_or(true)
}

/// `WE_LINK_RT_PUB`（默认 **tail** = 官方口径）：被引用层的链接 RT 发布什么。
/// 官方 `SceneImageObjectParser.cpp:1058-1078`：被引用层建一条内部透传趟
/// `make_internal_passthrough(effect_composite, link_output, "linked_layer_publish")`
/// ⇒ 发布的是**特效链的最终输出**（链尾），再一条 `linked_layer_visible_resolve`
/// 把链接 RT 搬上屏。旧行为（`base`）发布的是**底图**（特效前的本层合成），
/// 只对"被引用且不可见"的层（`off_rt` 那条路）巧合地等于链尾。
/// `WE_LINK_RT_PUB=base` 精确回旧行为。
fn link_rt_pub_tail() -> bool {
    std::env::var("WE_LINK_RT_PUB")
        .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "base" | "0" | "false" | "off" | "no"))
        .unwrap_or(true)
}

/// `WE_LINK_RT_CONTENT=<f>`（默认 **1.0 = 官方口径**；`<f>` 数值 / `0`/`off` = A/B 用）：
/// 被引用图层（链接 RT 的**生产方**）被消费方采样时，**内容**（不是 RT 尺寸、不是 `T0`、不是
/// `blendscale`）绕取景中心按 `f` 缩放。**官方公式里没有这一项** ⇒ 默认 1.0。
/// 推导（只用场景数据 + 官方 shader 原文）：
/// 官方 `assets/effects/blend/shaders/effects/blend.vert`（`TRANSFORMUV=1`）：
/// ```text
/// scaleA = T0.zw / T1.zw
/// maskUV.zw = 0.5 + [(uv - 0.5) + (T0.zw - T1.zw)/2 / T0.zw - offset/…/T0.zw] · scaleA/g_BlendScale
/// ```
/// 四边形 UV 从 0 走到 1 时，采到的遮罩 UV 跨度 = `scaleA/g_BlendScale`，
/// **换成遮罩纹素**（乘 `T1.zw`）= `T0.zw/g_BlendScale` ⇒ `T1` 与 `T0` 在"每单位 UV 采多少
/// 纹素"里互消。于是**屏幕像素 / 遮罩纹素**：
/// ```text
/// m = g_BlendScale · quad_h_px / T0_h
/// ```
/// 只要 `quad_h_px ≡ T0_h`（= 缓冲 / 卡片 / 上报三者同尺寸，见 [`texres_pass_on`] 与
/// [`text_chain_layer_on`]），`m` 就**恒等于**作者写的 `g_BlendScale`，不需要任何标定常数。
/// ⚠ 曾经的 `C = 1.42`（`f = C/(blendscale·scale_y)`）是在给"T0 ≠ 缓冲"那个错误**打补丁**的
/// 拟合常数，已按 说明 §3.3 第 3 步删除：
/// 只保留 `WE_LINK_RT_CONTENT` 这一个显式 A/B 旋钮，默认 1.0。

/// 采样某张链接 RT 的那个 pass 的参数（`WE_LINK_RT_CONTENT=auto` 的推导输入）。
/// 全部来自 scene.json：`blendscale` / `blendoffset` / 消费层自己的 `size`(=`T0`) 与 `scale`。
#[derive(Clone, Copy, Debug)]
struct LinkConsumer {
    blendscale: f32,
    scale_y: f32,
    /// `blendoffset`（像素，T0 口径）
    offset: [f32; 2],
    /// 消费层自己的画布 = `T0`（`obj.size`）
    canvas: [f32; 2],
}

/// 链接 RT 里"四边形中心采到的那一点"在 **RT 像素**坐标系里的位置。
/// 由 `blend.vert` 的 `TRANSFORMUV` 分支直接解出来（`uv = 0.5`）：
/// ```text
/// maskUV(0.5) = 0.5 + [((T0-T1)/2 - offset)/T0] · scaleA/BlendScale
/// = 0.5 + ((T0-T1)/2 - offset) / (T1·BlendScale) (scaleA=T0/T1)
/// ```
/// 乘 `T1` 换成纹素 ⇒ `pivot = T1/2 + ((T0-T1)/2 - offset)/BlendScale`。
/// **为什么缩放要绕这一点**：`blendoffset` 让四边形中心采到的**不是** RT 中心
/// （本场景 = RT 的 (0.63, 0.27)）。绕 RT 中心缩放会把内容搬出采样窗口（实测：字里直接变空），
/// 绕这一点缩放才是"同一取景、只改角色大小"。
fn link_rt_content_pivot(c: &LinkConsumer, rt: (f32, f32)) -> [f32; 2] {
    let bs = if c.blendscale.abs() > 1e-6 {
        c.blendscale
    } else {
        1.0
    };
    [
        rt.0 * 0.5 + ((c.canvas[0] - rt.0) * 0.5 - c.offset[0]) / bs,
        rt.1 * 0.5 + ((c.canvas[1] - rt.1) * 0.5 - c.offset[1]) / bs,
    ]
}

fn link_rt_content_scale() -> Option<f32> {
    // `WE_TEXRES=0`（旧口径总闸）⇒ 链接 RT 的内容一个像素都不动（护栏基线可逐字节复现）。
    if !per_tex_res_on() {
        return None;
    }
    match std::env::var("WE_LINK_RT_CONTENT") {
        // 默认 = **1.0 = 官方口径：没有任何内容缩放**。
        // `blend.vert` 里不存在"内容缩放"这一项（它只管 uv）；链接 RT 的内容密度由
        // `EnsureLinkRenderTarget` 的 `source_node.Size` 决定，1 RT px = 1 场景单位。
        // 所以 `f` 恒等于 1，旋钮只留作 A/B（`<f>` 数值）与"完全关"（`0/off`）。
        Err(_) => Some(1.0),
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(1.0),
            "0" | "off" | "false" | "no" => None,
            t => t
                .parse::<f32>()
                .ok()
                .filter(|f| f.is_finite() && *f > 0.0)
                .map(|f| f.clamp(0.05, 4.0)),
        },
    }
}

/// `effects/blend` 那条 `TRANSFORMUV` 分支里 **y 分量** 的 `(T0−T1)/2` 符号。
/// **默认 `official` = 原文照抄**（不 patch）。这一项是 A/B 实验开关：
/// 实测把 y 项取反会让字内人物**再上移 77 px**（161→84，win 是 247），方向相反 ⇒ 默认不用。
/// 见本次报告「剩下的 86 px」：它不是符号问题，最可能是 WE 的文字 RT 比 `obj.size` **高**
/// （文字墨水在 RT 里锚定，四边形跟着变高 ⇒ `uv` 平移但 `m` 不变），需要动 `build_text_layer`。
fn blend_yoff_on() -> bool {
    // 只有显式 `flip` 才开；缺省/`official`/`0` 一律原文。
    match std::env::var("WE_BLEND_YOFF") {
        Ok(v) => v.trim().eq_ignore_ascii_case("flip"),
        Err(_) => false,
    }
}

/// `WE_BLEND_CENTER=<f>`（**默认 -1.0（仅 y）；0/off = 逐字节回原文**）：
/// 给 `effects/blend` 的 `TRANSFORMUV` 分支**追加**一项"中心差"平移，口径 = **遮罩纹素**：
/// ```glsl
/// v_TexCoord.zw += (g_Texture1Resolution.zw - g_Texture0Resolution.zw) * 0.5
/// / g_Texture1Resolution.zw * f;
/// ```
/// 为什么需要这个旋钮（= 本单路 A 的结论）：
/// 官方原文那一项是 `-(g_BlendOffset - (T0-T1)*0.5)/T0`，随后整体 `* scaleA/BS`，
/// ⇒ 它进到**遮罩 uv** 的净量是 `(T0-T1)/(2·T1·BS)` 纹素。而"两张 RT 中心对齐"这个
/// **几何**条件要求的是 `(T0-T1)/(2·T1)` 纹素 —— 差一个 `BS`。`f` 就是用来扫这个差的：
///
/// * `f = 0`（不追加）⇒ 原文；
///
/// * `f = -(BS-1)/BS` ⇒ 净量 = `(T0-T1)/(2·T1)`（几何口径，**实测 dy→0**，见报告 §2.3）；
///
/// * 正负号两种都试过，只有负号把内容**往下**推（= 我们要的方向）。
///
/// * **默认 `-1.0`**：实测 dy −85 → 0、dx/scale 不变；`WE_BLEND_CENTER=0` 回旧字节。
fn blend_center_f() -> Option<f32> {
    let ev = std::env::var("WE_BLEND_CENTER").ok();
    match ev.as_deref().map(str::trim) {
        // 缺省 = 修后行为（-1.0，仅 y）；0/off = 逐字节回原文
        None => Some(-1.0),
        Some("0") | Some("off") | Some("false") | Some("no") => None,
        Some("on") | Some("true") | Some("yes") => Some(-1.0),
        Some(t) => t.parse::<f32>().ok().filter(|f| f.is_finite() && f.abs() > 1e-9),
    }
}

/// 链接 RT 内容缩放：**官方公式给的就是 1.0**（没有任何缩放）。
///
/// # 官方代数（`effects/blend/shaders/effects/blend.vert`，`TRANSFORMUV=1`）
/// ```text
/// scaleA = T0.zw / T1.zw // 采样跨度（T0/T1，像素口径）
/// v.zw -= (g_BlendOffset/g_TextureReductionScale - (T0.zw-T1.zw)*0.5) / T0.zw
/// v.zw -= 0.5; v.zw *= scaleA/g_BlendScale; v.zw += 0.5
/// ```
/// 四边形 uv 走完 0..1 时，采到的遮罩 uv 跨度 = `scaleA/g_BlendScale`，换成遮罩纹素
/// （乘 `T1.zw`）= **`T0.zw / g_BlendScale`** —— `T1` 在"每单位 uv 采多少纹素"里**互消**。
/// 于是"屏幕像素 / 遮罩纹素"：
/// ```text
/// m = g_BlendScale · quad_h_px / T0_h （quad_h_px = obj.size_y·obj.scale_y·k）
/// ```
/// 本场景：`m = 2.2 · (1344·1.16768·0.5) / 1344 = 2.2·1.16768·0.5 = 1.2846`
/// （1920×1080，`k = 0.5` 屏幕像素/场景单位）。
///
/// # 遮罩 RT 的内容密度（1 RT px = 1 场景单位）
/// `OWE src/Scene/Scene/Scene.cpp:2802 EnsureLinkRenderTarget`：RT 尺寸 = `source_node.Size`
/// = `obj.size`（1583 = 2000×1476）；`src/Scene/Pkg/Parse/Image/SceneImageObjectParser.cpp:600`
/// 的 `effect_target_size` 同样是 `obj.size`（`ImageEffectTargetSize`，第 46 行）。内容按图层
/// 矩形铺满 0..1（每趟的四边形 = `DefaultEffectMesh` 单位方阵，`effect` 相机 = 2×2 正交，
/// `SceneToRenderGraph.cpp:324`）⇒ 密度 = 1 px/单位。
/// ⇒ 遮罩里的世界内容落屏放大率 = `m/k = g_BlendScale·obj.scale_y = 2.569`（本场景）。
/// **这不是 1:1**，也不是任何"标定常数"——它是 shader 与 RT 尺寸直接算出来的。
/// 上次为了让字内人物与前景人物"1:1"而加的 `C=1.42`（`f = C/(blendscale·scale_y)`）
/// 是**补丁**，已删除：它把内容压小 1.42×，而 win 实测的 f 恒等于 1（见本次报告）。
/// 验证（本次实测，1920×1080，SIFT 对源图 `背景角色` 求相似变换）：
///
/// * `f=1.0` ⇒ 屏上 1.284 px/源px；win `3-1.jpg`(2560) 实测 1.7120 = 1.2840（×0.75）⇒ 差 **0.05%**；
///
/// * `f=0.5528`（旧 C=1.42）⇒ 0.710（同一把尺子），比 win 小 **1.81×**。
/// ⚠ 旋钮：`WE_LINK_RT_CONTENT=<f>` 只作 A/B；`0/off` ⇒ 连图层空间换算一起关（旧字节）。
pub fn link_rt_content_official() -> f32 {
    1.0
}

fn fx_cardspace_on() -> bool {
    // 默认关：不设 / 空 / "0" 一律关（与既有开关同口径、互相独立）
    crate::flag_default_on("WE_FX_CARDSPACE")
}

/// 见 [`fx_cardspace_on`]：把"单位四边形输入"的摆放矩阵换成"卡片网格输入"的那个。
fn fx_card_mvp(a: [f32; 16], card: [f32; 2]) -> [f32; 16] {
    if !(card[0].abs() > 0.5 && card[1].abs() > 0.5) {
        return a;
    }
    Mat4 { m: a }
        .mul(&Mat4::scale(2.0 / card[0], 2.0 / card[1], 1.0))
        .column_major()
}

/// 把 `diag(2/size)` 乘在**左边**：`u₁ = D⁻¹·A`。**这是底图趟默认用的那个**（理由）
/// [`base_card_mvp`] 的实测定标）。
/// 与 [`fx_card_mvp`]（右乘）的关系：`a` 是**单位阵**时两者**完全相同**
/// （`D⁻¹·I = I·D⁻¹`）⇒ 上次 `WE_FX_CARDSPACE` 那条路左右乘无差别、**不受影响**；
/// `a` 非单位阵时左乘才是对的（驱动约定 = `uᵀ·v`），右乘会引入 `D⁻¹·Aᵀ·D` 的亚像素偏移。
fn fx_card_mvp_left(a: [f32; 16], card: [f32; 2]) -> [f32; 16] {
    if !(card[0].abs() > 0.5 && card[1].abs() > 0.5) {
        return a;
    }
    Mat4::scale(2.0 / card[0], 2.0 / card[1], 1.0)
        .mul(&Mat4 { m: a })
        .column_major()
}

/// 底图趟用的换空间公式。**默认左乘**（`diag(2/size) · A`），`WE_BASE_CARD_ORDER=right`
/// 切到右乘做 A/B。
///
/// # 为什么是左乘（实测定标，本单最有价值的一条结论）
/// 本渲染器上传 MVP 时会先 `t4` 转置一次（`gl_ctx.rs:493`
/// `gl.uniform_matrix_4_f32_slice(Some(loc), false, &t4(&mvp))`），配合 GL"列主序数组"的规则，
/// 净效果是"**驱动实际算 `R(u)·v`，`R(u)` = 把上传数组 `u` 按行主序读出来的矩阵**"，
/// 而 `R(Mat4{m:X}.column_major) = Xᵀ`（这也是调用点一律先 `.transposed` 的原因）。
/// 原趟算的是 `R(a)·v_unit = Aᵀ·v_unit`（`A = Mat4{m:a}`），要换几何后逐位等价就需要
/// `R(u₁)·(D·v_unit) = Aᵀ·v_unit` ⇒ `R(u₁) = Aᵀ·D⁻¹` ⇒ `u₁ = (D⁻¹·A).column_major` —— **左乘**。
/// 右乘会得到 `R(u) = D⁻¹·Aᵀ` ⇒ `D⁻¹·Aᵀ·D ≠ Aᵀ`（`D` 与 `Aᵀ` 不交换）⇒ **亚像素级整体偏移**
/// （实测 `2861113973` 17.12% 像素 / `3571777169` 0.46% 像素在变，而左乘时**逐字节相同**）。
/// ⚠ 上次的 `WE_FX_CARDSPACE` 定不出这条约定：它那条路的 `A` 是**单位阵**，
/// `D⁻¹·I = I·D⁻¹` ⇒ 左右乘完全一样。**所以那条路不受本结论影响**（本单实测它逐字节不变）。
fn base_card_mvp(a: [f32; 16], card: [f32; 2]) -> [f32; 16] {
    match std::env::var("WE_BASE_CARD_ORDER").ok().as_deref() {
        Some("right") => fx_card_mvp(a, card),
        _ => fx_card_mvp_left(a, card),
    }
}

/// 把顶点极值 `extent`（顶点坐标系，`[±hx, ±hy]`）过一遍 MVP，返回四个角的**最大 |ndc|**。
/// 用途：融合趟的"静默画空"护栏 —— 顶点几何与 MVP 的空间口径一旦对不上（例如把 `±1`
/// 单位四边形配上卡片 MVP `diag(2/3840)`），极值顶点只到 |ndc| = 0.0005，这一趟就只在目标
/// RT 正中画 ~2×2 像素，其余保持清屏黑 ⇒ 整层全黑而**没有任何报错**（`3504393434` 实测）。
/// 正常的两套几何都是 |ndc| = 1.0：单位四边形 × 单位阵，或卡片 ±size/2 × `2/size`。
/// 数组按**列主序**（与 `uniform_matrix4fv(..., true, ...)` 同口径）：`ndc_r = Σ_c m[c*4+r]·v_c`。
fn mvp_max_ndc(m: &[f32; 16], extent: [f32; 2]) -> f32 {
    let mut best: f32 = 0.0;
    for sx in [-1.0f32, 1.0] {
        for sy in [-1.0f32, 1.0] {
            let v = [sx * extent[0], sy * extent[1], 0.0f32, 1.0f32];
            let mut out = [0.0f32; 4];
            for r in 0..4 {
                out[r] = (0..4).map(|c| m[c * 4 + r] * v[c]).sum();
            }
            if out[3].abs() < 1e-9 {
                return f32::INFINITY;
            }
            let nx = (out[0] / out[3]).abs();
            let ny = (out[1] / out[3]).abs();
            best = best.max(nx.max(ny));
        }
    }
    best
}

/// 官方 `GenCardMesh` 的 `size`（像素）—— 数据驱动地按官方语义解析。
/// 官方出处：`array<float,2> geometry_size = wpimgobj.size`
/// （`open-wallpaper-engine/src/Scene/Pkg/Parse/Image/SceneImageObjectParser.cpp:71`
/// `const auto geometry_size = wpimgobj.size;`），随即
/// `GenCardMesh(mesh, {geometry_size[0], geometry_size[1]}, mapRate, …)`（同文件 `:325/365/372`）。
/// 场景里没写 `size` 时，WE 用**模型自己的尺寸**（`autosize`）：
///
/// * 模型 json 可以写 `width`/`height`（单位 = **像素**，一手：
/// `open-wallpaper-engine/docs/scenejson.md:3-4`）；
///
/// * 都没写时按**底图贴图的像素尺寸**取 —— 依据是同一个 preview 工程的实测定标
/// （上次"补偿探针"把 `DirectionWeights` 除以 `(512,1024)/2`、`(1024,256)/2` 之后
/// 形状与官方语义逐条对上）；
/// ⚠ `autosize` 究竟取贴图的**填充后**尺寸还是**美术**尺寸（`TEXI` 的
/// `mapWidth/mapHeight`，本 preview 的 `plant1` 是 `512x1024` 纹理 / `512x875` 美术）
/// 落在**未 vendor** 的 `wpscene` 包里 ⇒ **无法判定**；这里取我们**真正采样**的那张
/// 贴图的尺寸（`Gl::texs` 的 w/h = 解码后的填充尺寸），与上次的定标一致。
/// 三项都拿不到 ⇒ 返回 `fallback`（= 我们的场景兜底，与现状逐位一致）。
fn fx_card_size(
    fallback: [f32; 2],
    obj_size: Option<[f32; 2]>,
    tex_px: Option<[f32; 2]>,
) -> [f32; 2] {
    let good = |c: [f32; 2]| c[0].is_finite() && c[1].is_finite() && c[0].abs() > 0.5 && c[1].abs() > 0.5;
    for c in [obj_size, tex_px].into_iter().flatten() {
        if good(c) {
            return [c[0].abs(), c[1].abs()];
        }
    }
    [fallback[0].abs(), fallback[1].abs()]
}

/// `WE_BASE_CARDSPACE`：把**底图趟（对象的基础/底图趟，类①）**的顶点空间
/// 也对齐官方"卡片网格"（`±size/2` 对象像素空间）。
///
/// # 缺口
/// 上次把**特效链内趟**（类②）换成了官方卡片网格（`WE_FX_CARDSPACE`，已上屏），
/// 并在 §8.2 明确交底：底图趟官方**也是**同一张卡片网格，那一单**没改**。本单补这一条。
/// 官方给图像对象**每一趟**（底图趟 + 特效趟 + 收尾趟）的顶点几何都是 `GenCardMesh`：
///
/// * ① 一手（几何构造）：`open-wallpaper-engine/src/Scene/Pkg/Parse/Scene/
/// SceneGeometryBuilder.cpp:19-48` —— `left = -(size.x/2)+off.x` … `right = size.x/2+off.x`
/// ⇒ 顶点是 **`±size/2` 的对象像素空间**、uv = `mapRate`；
///
/// * ② 一手（谁在用）：`.../Parse/Image/SceneImageObjectParser.cpp:325-328`（有 puppet）/
/// `:365-368`（无 puppet）`GenCardMesh(mesh, {geometry_size[0], geometry_size[1]}, mapRate,
/// source_alignment_offset)`，而 `geometry_size = wpimgobj.size`（同文件 `:71`）；
///
/// * ③ 一手（底图趟用哪台相机）：同文件 `:604-611` `SceneCamera::MakeOrthographic(
/// effect_extent.w, effect_extent.h, -1, 1)`（`Scene.cppm:1036`），而
/// `effect_extent = ImageEffectTargetSize`（`:46-53`）= **对象自己的 `size`**（非 fullscreen）
/// ⇒ `±size/2` 的卡片网格经这台正交相机**正好铺满整张 RT**。
/// 对照我们的实现：图层缓冲 = 对象矩形（`lw/lh` 见调用点），底图趟顶点 `±1`、把
/// `scale(size/2)` 折进 MVP（`layer_copy_mvp`）⇒ **数学上等价，但顶点空间不同**。
///
/// # 与 `WE_FX_CARDSPACE` 的关系（互相独立、可各自 A/B）
/// 两者共用同一个 [`fx_card_mvp`] 不变量与 `Gl::make_layer_card`，但作用**不同的趟**：
/// `WE_FX_CARDSPACE` 只管链内趟（类②），本开关只管底图趟（类①）。默认**都关**，
/// 任一开关不动另一条路的字节。
///
/// # 等价关系（可证 —— 这是"换空间不改画质"的根据）
/// 卡片顶点 `v_card = D·v_unit`（`D = diag(size.x/2, size.y/2, 1)`）。上传约定是
/// "上传数组 `u` ⇒ 驱动实际算 `R(u)·v`（`u` 的**行主序**读法）"（见 [`base_card_mvp`] 的实测定标）
/// ⇒ 只要 **`u₁ = D⁻¹·A`（左乘，`A = Mat4{m:a}`）** 就有 `R(u₁)·v_card = Aᵀ·D⁻¹·D·v_unit = Aᵀ·v_unit`，
/// **顶点着色器不改 `position` 时逐位等价**。
/// 实测（`17` 条真语料 640x360 `--t 0`）：**14 条逐字节相同**；`3299815325` 只有 5 个像素差
/// **1 个 LSB**（纯浮点尾差）；另 2 条落在**已知随机**场景（`2887537617`/`2961776805`）自己的
/// 漂移范围内。单测 `tests::base_card_mvp_preserves_ndc_for_pure_mvp_shaders`。
///
/// # 为什么 17 条真语料上"开/关"几乎没有像素差（不是开关没生效）
/// 普查（脚本口径见本次 NOTES §5.2）：17 个 `scene.pkg` 里带 `position.{x,y,z,w} =`
/// 逐分量改写的 `.vert` **只有 `shaders/effects/foliagesway.vert`**（且只出现在
/// `effects/*/effect.json` 的**链**里 ⇒ 那是类②，不是底图趟）；官方 `assets/shaders/` 54 个
/// `.vert` 里只有 `composelayer.vert`（= 合成/透传层，本开关作用域排除）与
/// `HLSL/dx11playlisttransition.vert` 改 `position`。
/// ⇒ 底图趟的 shader 全是"顶点只拿来乘 MVP"的趟 ⇒ 换空间**逐位等价**。
/// 开关**确实生效**：实测 `3704273480` 12 个、`3571777169` 28 个、`2887537617` 36 个
/// 作用域对象的底图趟**几何真的换了**（`WE_DEBUG_LAYERS=1` 的 `已换` 计数）。
///
/// # 边界（**不能**换的情形）
/// 与 `WE_FX_CARDSPACE` 同：整屏后处理层（`fullscreen`，官方文档
/// `open-wallpaper-engine/docs/scenejson.md:5` 把 "signed unit quad" 写成它的**特例语义**）、
/// 合成/透传层、`published`、shape、文字、网格取景框（`fit`）、mesh-uvspace、puppet 网格层；
/// 以及 `card_ok=false` 的趟 —— 那些趟的顶点**保持单位四边形**、MVP **保持原值**。
/// **画布空间**（没有特效链的普通图像对象）也一起对齐，但只在
/// **既没挂骨骼、也没视差**（`attach_hit`/`par_info` 都空、`WE_BASE_MVP != layer`）的对象上：
/// 那两条路每帧会**改写 `pass.mvp`**（`attach_passes` 的 `m_place_at`、`parallax_passes` 的
/// `mvp_eff`），覆盖后顶点仍是 `±size/2` 而 MVP 不再除 `D` ⇒ 整层被放大 `size/2` 倍。
/// 判据与理由见调用点的 `canvas_cards_ok`。
/// ⚠ 本开关的 `card_ok` 用 [`Gl::make_layer_card_vp`] 的**编译产物判据**，**不是**
/// `WE_FX_CARDSPACE` 用的源码字符串判据。原因：官方底图趟的 `assets/shaders/genericimage2.vert`
/// 把 `uniform mat4 g_ViewProjectionMatrix;` 放在 `#if LIGHTING` 里（`:55-56`），默认 LIGHTING=0
/// ⇒ 那一支**根本没编译进去**，但源码字符串里有 ⇒ 源码判据会把**所有底图趟**误拒
/// （实测 3704273480 的 12 个作用域对象全部 `card_ok=false`，开关等于没接）。
/// 编译产物判据另加两条**顶点位移**来源（`g_MorphWeights`/`g_Bones` active ⇒ 拒）。
/// `WE_FX_CARDSPACE` 的源码判据**保持不变**（已上屏 ⇒ 不得回退）。
fn base_cardspace_on() -> bool {
    // 默认关：不设 / 空 / "0" 一律关（与 `WE_FX_CARDSPACE` 等既有开关同口径、互相独立）
    crate::flag_default_on("WE_BASE_CARDSPACE")
}

/// 网格顶点 → 图层归一化坐标（**建层与逐帧重算共用**，保证同一 `t` 逐位一致）。
/// `y = ±p[1]/hh`（`WE_MESH_Y=up` 时翻号）+ 落点常量 `dy/hh`。
fn mesh_norm_pos(
    p: &[f32; 3],
    hw: f32,
    hh: f32,
    dy: f32,
    flip_up: bool,
    clamp01: bool,
) -> [f32; 3] {
    let y = if flip_up { -p[1] / hh } else { p[1] / hh };
    let (mut x, mut y) = (p[0] / hw, y + dy / hh);
    if clamp01 {
        x = x.clamp(-1.0, 1.0);
        y = y.clamp(-1.0, 1.0);
    }
    [x, y, p[2] / hw]
}

/// 图层空间的 y 落点修正（`Δy = scene_h − 2·world_y`）**是否适用于这一层**。
/// 只对**声明了特效**的层成立：只有那种层的内容会「绕一圈」经过图层缓冲（链内的趟是不带
/// MVP 的全屏四边形，采样 RT 时天然带一次 V 反向）⇒ 画布/平面路径与它才差出那次 y 镜像。
/// 特效列表为空的层**不绕圈**：底图趟写进图层缓冲、收尾趟再读回来，两趟的显式 V 反向互相
/// 抵消 ⇒ 修正对它是**凭空多出来的一次纯平移**（2887537617 的 `B沙发` 因此整块沉到人物身后，
/// 用户报的「雕花沙发靠背/侧翼不见」）。实测数字见调用点。
/// `WE_MESH_YFIX_ALL=1` 恢复旧行为（对所有无父链层一律加修正）做 A/B。
/// （`3571777169` 背景腰部横向接缝）：**「无特效」不等于「不需要修正」**。
/// 上一条结论的活样本（`2887537617` 的 `B沙发`/`B灯台`/`C桌子沙发`）全都是**作者给了
/// `cropoffset` 的模型** —— 那条 `+crop_offset.y·scale·2` 的绘制偏移（见 MVP 那一段的
/// `WE_CROP_MODE=10`）对它们恰好补上了这次落点差，所以「不绕圈 + 有 cropoffset」看着是对的。
/// 反过来，**模型没写 `cropoffset`** 的层没有任何东西补这一次镜像：实测本场景
/// `353 земля под ногами биг`（`origin=(1947.2, 380.5)`、`size=2000×556`、无特效、无
/// `cropoffset`）被摆在屏幕 y[-110,617] —— 它的**四边形下边缘**正好落在画面腰部
/// （全帧 skip-diff：`WE_SKIP_ID=353` 差异 566677px、bbox=(0,115,2559,617)、该行
/// 行梯度 42.4，而两张 Windows 参照的同一指标只有 8.3 / 7.2 ⇒ 参照里没有这条缝）。
/// 走这条修正后它的落点回到 y≈1186（= `origin.y` 从**画布底**量的那个位置），
/// 下边缘落到画面外，缝消失。
/// 判据（数据驱动，不看场景 id / 对象名）：该层**没有作者给的贴图偏移**（模型未写
/// `cropoffset`）、**没有对齐锚点**（`alignment` 空 —— 它自己有一套 `align_dy`，见调用点）、
/// **没有锁变换/脚本驱动**（`locktransforms`、`solid`），即「落点完全由 `origin` 决定」的
/// 普通图片层。`WE_YFIX_PLAIN=0` 关掉这条（回到「只对有特效的层修正」）做 A/B。
fn yfix_applies_for_layer(has_effect_chain: bool, plain_place: bool) -> bool {
    has_effect_chain
        || plain_place
            && std::env::var("WE_YFIX_PLAIN").ok().as_deref() != Some("0")
        || std::env::var("WE_MESH_YFIX_ALL").ok().as_deref() == Some("1")
}

/// **开关（默认开）**：`blend` 若是脚本绑定对象，取**脚本求值结果**
/// （静音 ⇒ `value × scriptproperties.minvalue`，见 `we_scene::scene::AnimationLayer::blend_weight`）。
/// `WE_BLEND_SCRIPT=0` ⇒ 改用脚本的**输入** `value`（= 之前的口径，逐字节回退）。
fn layer_blend(l: &we_scene::scene::AnimationLayer) -> f32 {
    if std::env::var("WE_BLEND_SCRIPT").ok().as_deref() == Some("0") {
        l.blend_raw()
    } else {
        l.blend_weight()
    }
}

/// **开关（默认关）**：`animationlayers[].blend` 的脚本**逐帧**求值并改写 puppet 混合权重。
/// 两个门同时开才生效：`WE_SCRIPT_AUDIO=1`（属性脚本总闸，脚本 API 与音频缓冲都在那条路上）
/// 与 `WE_BLEND_LIVE=1`（本支单独 A/B）。关时 `blend_scripts` 恒空，走
/// `AnimationLayer::blend_weight` 的**静默端**（value * scriptproperties.minvalue），
/// 与 之前的静态口径**逐字节相同**。
/// 语义依据（OWE `Scene/Script/ScriptBindingContext.cpp:295` 的 `ForAnimationLayer`：
/// 逐帧把脚本返回值写回 `layer.blend`；`Puppet.cpp` 的 `genFrame` 用 `blend` 做
/// `trans*(1-blend)+frame*blend`）。
fn blend_live_on() -> bool {
    propscript::enabled() && std::env::var("WE_BLEND_LIVE").ok().as_deref() == Some("1")
}

/// 哪些动画层带 `blend` 脚本，以及它们在 `PoseLayer` 表里的下标。
/// 过滤链必须与 [`anim_layer_kept`] **完全一致**（建网格、挂接锚点都用它），否则下标会偏。
/// 初值取 `blend_raw`（= 脚本的 `value`，也就是脚本 `init(value)` 的入参）；
/// 绑定级 `scriptproperties` 原样交给 [`propscript::NumScript::new`]（编辑器值优先于脚本默认）。
fn blend_live_scripts(obj: &we_scene::scene::Object) -> Vec<(usize, propscript::NumScript)> {
    if !blend_live_on() {
        return Vec::new();
    }
    let mut out: Vec<(usize, propscript::NumScript)> = Vec::new();
    let mut slot = 0usize;
    for l in obj.animation_layers.iter() {
        if !anim_layer_kept(l) {
            continue;
        }
        let idx = slot;
        slot += 1;
        let Some(serde_json::Value::Object(o)) = l.blend.as_ref() else {
            continue;
        };
        let Some(script) = o.get("script").and_then(|s| s.as_str()) else {
            continue;
        };
        match propscript::NumScript::new(script, l.blend_raw(), o.get("scriptproperties")) {
            Ok(sc) => out.push((idx, sc)),
            Err(e) => eprintln!("[脚本] blend 脚本装载失败（退回静态值）：{e}"),
        }
    }
    out
}

/// **开关（默认关）**：把场景对象声明的 `attachment`（挂骨骼）接进 scene-graph。
///
/// # 语义（一手依据 = OWE `open-wallpaper-engine`）
/// 带 `attachment` 的对象，其**世界变换 = 锚点骨骼的世界矩阵 × 它自己的原始局部变换**：
///
/// * `Scene/Pkg/Parse/Puppet/MdlParser.cpp:612-631`（`ParseMDAT`）—— 父 puppet 里的挂接表，
/// 逐条 = `bone_index(u16)` + `name(cstr)` + **64 字节列主序 4×4 仿射**（锚点在**该骨的
/// 局部空间**里）。`attachment` 字段的取值就是这里的 `name`（骨骼名），
/// `Puppet::attachmentIndex` 线性查表 ⇒ **名字→骨号是数据里的映射，没有外部魔数**。
///
/// * `Puppet.cpp:119-136`（`prepared`）：`bind_xform = bone_bind · local_xform`，其中
/// `bone_bind` = 沿 `file_parent` 链复合 `local_bind` = 该骨的 **bind 世界矩阵**；
/// `Puppet.cpp:155` `attachmentBindTransform` 返回它；
/// `Puppet.cpp:496-506` `PuppetLayer::attachmentTransform(i,t) = genFrame(t)[bone] · bind_xform`
/// （`genFrame(t)[k]` 已在返回前乘过 `inv_bind` ⇒ rest 时是**单位阵**；见 `mdl.rs` 的注释）。
///
/// * `Scene/Pkg/Parse/Scene/SceneFinalize.cpp:358-390`（挂接，含 `ride_parent_parallax`）：
/// 命中后 ① `context.ride_parent_parallax_ids.insert(id)`（子对象**骑父层视差**）；
/// ② 父层有 puppet_layer（会动）⇒ `node->SetLocalFrame(anchor.matrix * local_base)`
/// 并逐帧重算（`local_base` = 子节点**原始** local frame，首帧取一次）；
/// ③ 父层静态 ⇒ 一次性同式。
///
/// # 锚点**逐帧**求值（本文件第 6 轮）
/// ②里那句\"逐帧重算\"是真的逐帧：`anchor(t) = genFrame(t)[骨] · bind_xform`，`genFrame` 随 `t` 变
/// （`Puppet.cpp:496-506`）。我们的 scene-graph 矩阵是**建层时烘一次**的 ⇒ 上次锚点只能取
/// `t = 0`，实测锚点随 `t` 漂移上限 ≈40 模型单位（≈29px @2560，见 `AttachAnim` 的注释）。
/// 现在补上逐帧：见 [`AttachAnim`] / [`AttachHit::register`] / `draw_frame` 里的\"挂接逐帧\"那段。
/// **A/B 开关**：`WE_ATTACH_BAKE=1` 退回\"烘 `t = 0`\"的旧口径，
/// 用来量\"静态 vs 逐帧\"的差；默认（不设）就是**逐帧**（= 与 OWE 一致）。
/// 参照实现 **linux-wallpaperengine（Almamu 版）没有实现 attachment** ——
/// `grep -rn attachment linux-wallpaperengine/src` 只命中 SPIRV-Cross 的
/// `input_attachment`/`color_attachment`（图形 API 术语），**没有任何场景字段级的处理**。
/// ⇒ 这一条机制只有 OWE 一处出处（如实记录，不装成"两家一致"）。
/// `WE_ATTACHMENT=1` 才生效 ⇒ **默认关**时整块代码不执行，与未接这条机制**逐字节相同**。
fn attachment_on() -> bool {
    crate::flag_default_on("WE_ATTACHMENT")
}

/// `WE_ATTACH_BAKE=1` ⇒ 退回\"建层时按 `t = 0` 烘一次\"的旧口径做 A/B。
/// 默认（不设）= **逐帧**求值（与 OWE 一致）。只在 `WE_ATTACHMENT=1` 时有意义。
fn attach_bake() -> bool {
    std::env::var("WE_ATTACH_BAKE").ok().as_deref() == Some("1")
}

/// `WE_DEBUG_ATTACH=1` ⇒ 每帧给带 `attachment` 的层打一行：锚点/对象世界 T 的
/// \"烘 `t=0`\"vs\"逐帧\"两个值 + 差值（场景单位与 px）—— 验收\"逐帧求值是否生效\"的数字口。
fn attach_dbg() -> bool {
    std::env::var("WE_DEBUG_ATTACH").ok().as_deref() == Some("1")
}

/// 建层时**烘一次**的挂接数据（`WE_ATTACHMENT=1` 命中时才有）。
/// 它只用来在 `build` 里把\"摆放趟\"登记进 `attach_passes`；每帧的更新口径见 [`AttachAnim`]。
struct AttachHit {
    /// 父对象的世界矩阵（与 `t` 无关：父自己的变换是建层时烘的）
    w_parent: Mat4,
    /// 父 puppet 的姿态（含 `animationlayers`）—— 每帧按 `t` 求值
    pose: std::sync::Arc<mdl::PuppetPose>,
    /// 本对象用的那条挂接记录（名字→骨号是**数据里的映射**，不按 id/名字特判）
    att: mdl::Attachment,
    /// `(W_parent · anchor(0))⁻¹`
    a0_inv: Mat4,
    /// 建层时的对象世界矩阵 `M0 = W_parent · anchor(0) · local_raw`
    m0: Mat4,
}

/// 一趟\"摆放矩阵\"的**逐帧**更新数据（一个带 `attachment` 的层可能有多趟，每趟一份）。
///
/// # 数学（为什么不重跑建层、也不复制那一长串推导）
/// 建层烘的是 `M0 = W_parent · anchor(0) · local_raw`（`anchor(t) = genFrame(t)[骨] · bind_xform`，
/// `OWE Puppet.cpp:119-136/155/496-506`），逐帧真值是 `M(t) = W_parent · anchor(t) · local_raw`。
/// 两者只差一个**场景空间的左因子**：
/// `D(t) = (W_parent·anchor(t)) · (W_parent·anchor(0))⁻¹` ⇒ `M(t) = D(t)·M0`。
/// 本渲染器里每一趟\"把这一层摆进场景\"的矩阵都长成 `mvp = Q_L · M · S_R` 的形状
/// （`Q_L = proj·pre`，`pre` = cropoffset 平移或单位阵；`S_R` = `scale(size/2)` / 图层空间取景框
/// 两者都与 `t` 无关）⇒
/// `mvp(t) = Q_L·D(t)·M0·S_R = (Q_L·D(t)·Q_L⁻¹) · mvp(0)`
/// 所以只要在趟上记下 `Q_L`、`M0` 与那条挂接记录，每帧一次矩阵乘法就能把它更新到真值。
/// `t = 0` 时 `D = 单位阵` ⇒ 更新后**逐位**等于建层值（同一算式 + 单位阵乘法）。
struct AttachAnim {
    w_parent: Mat4,
    pose: std::sync::Arc<mdl::PuppetPose>,
    att: mdl::Attachment,
    a0_inv: Mat4,
    /// `Q_L` 与其逆（`Q_L = proj·pre`，见上）
    q_l: Mat4,
    q_l_inv: Mat4,
    /// 本趟建层时的摆放矩阵（未转置；上传口径 = `.transposed.column_major`）
    m0: Mat4,
    /// 建层时的**对象世界矩阵** `M0 = W_parent·anchor(0)·local_raw`（诊断：烘 t=0 的世界 T）
    m_world0: Mat4,
    /// 建层时的对象世界 T（诊断用：\"烘 t=0\"那一侧的数）
    world0: [f32; 3],
    /// 上一次已经应用过的 `t`（同一 `t` 不重复算；初值 NaN ⇒ 第一帧必算）
    last_t: f32,
}

impl AttachAnim {
    /// 本帧的摆放矩阵（未转置）= `(Q_L·D(t)·Q_L⁻¹)·M0`。挂接数据缺失（骨越界等）⇒ `None`
    /// （那时保持建层值 = 旧行为）。
    fn m_place_at(&self, t: f32) -> Option<Mat4> {
        let am = self.pose.attachment_transform(&self.att, t)?;
        let a = self.w_parent.mul(&Mat4 { m: am });
        let d = a.mul(&self.a0_inv);
        let delta = self.q_l.mul(&d).mul(&self.q_l_inv);
        Some(delta.mul(&self.m0))
    }

    /// 本帧的对象世界 T（= `M(t) = D(t)·M0` 的平移列）。**与趟矩阵无关**（诊断/交叉验证用）；
    /// 趟那边的更新走共轭式 `(Q_L·D·Q_L⁻¹)·m0`，两者数学等价（残差见 `WE_DEBUG_ATTACH` 的打印）。
    fn world_t_at(&self, t: f32) -> Option<[f32; 3]> {
        let am = self.pose.attachment_transform(&self.att, t)?;
        let d = self.w_parent.mul(&Mat4 { m: am }).mul(&self.a0_inv);
        let m = d.mul(&self.m_world0);
        Some([m.m[12], m.m[13], m.m[14]])
    }

    /// 本帧的 `D(t)` 与\"共轭残差\"（诊断用）：
///
    /// * `D` —— 场景空间左因子 `(W_parent·anchor(t))·(W_parent·anchor(0))⁻¹`；
///
    /// * `e_inv` —— `max|Q_L·Q_L⁻¹ − I|`（逆矩阵的数值质量）；
///
    /// * `e_conj` —— `max|Q_L⁻¹·(Q_L·D·Q_L⁻¹)·Q_L − D|`（\"趟上那两步共轭乘法\"相对真值的残差）。
    /// 两者都 ~1e-6 量级 ⇒ 趟上的更新等价于逐帧真值（`t = 0` 时更是直接沿用建层值、零残差）。
    fn delta_probe(&self, t: f32) -> Option<(Mat4, f32, f32)> {
        let am = self.pose.attachment_transform(&self.att, t)?;
        let d = self.w_parent.mul(&Mat4 { m: am }).mul(&self.a0_inv);
        let i = Mat4::identity();
        let e_inv = self
            .q_l
            .mul(&self.q_l_inv)
            .m
            .iter()
            .zip(i.m.iter())
            .map(|(x, y)| (x - y).abs())
            .fold(0.0f32, f32::max);
        let delta = self.q_l.mul(&d).mul(&self.q_l_inv);
        let back = self.q_l_inv.mul(&delta).mul(&self.q_l);
        let e_conj = back
            .m
            .iter()
            .zip(d.m.iter())
            .map(|(x, y)| (x - y).abs())
            .fold(0.0f32, f32::max);
        Some((d, e_inv, e_conj))
    }
}

impl AttachHit {
    /// 把\"第 `pass_idx` 趟是本层的**摆放趟**、它的未转置摆放矩阵是 `m0`\"登记成逐帧更新。
    /// `q_l` = 该对象的 `Q_L`（建层时算出；拿不到 ⇒ 不登记 = 保持旧行为）。
    fn register(
        out: &mut Vec<AttachPass>,
        hit: &Option<AttachHit>,
        q_l: Option<Mat4>,
        m0: Mat4,
        pass_idx: usize,
    ) {
        let (Some(h), Some(q)) = (hit.as_ref(), q_l) else {
            return;
        };
        let Some(q_inv) = q.invert() else {
            return;
        };
        out.push(AttachPass {
            pass_idx,
            anim: AttachAnim {
                w_parent: h.w_parent,
                pose: h.pose.clone(),
                att: h.att.clone(),
                a0_inv: h.a0_inv,
                q_l: q,
                q_l_inv: q_inv,
                m0,
                m_world0: h.m0,
                world0: [h.m0.m[12], h.m0.m[13], h.m0.m[14]],
                last_t: f32::NAN,
            },
        });
    }
}

/// \"第 `pass_idx` 趟的 `mvp` 要按场景时间 `t` 更新\"的登记项。
struct AttachPass {
    pass_idx: usize,
    anim: AttachAnim,
}

/// **负 `scale` 的镜像轴语义** —— 数据驱动、默认关。
/// 病征：`3704273480` 的 `1714 背景测试` 声明 `scale = "-2.06137 2.06137 2.06137"`。
/// 我们（以及两个开源参照实现）把负号当**绕对象自身中心的内容镜像**用；而 win 参照里
/// 右半屏的沙滩面板显示的是贴图**右半**（晴空+积云+远山，没有棕榈）⇒ 需要把
/// "镜像轴到底在哪"穷举一遍，让像素说话，而不是直接改默认语义（默认档
/// `9b7eed5b…` 是硬护栏）。
/// 每个候选都在**世界矩阵**上做一次闭合运算（不改网格、不改 shader），所以互相可比：
/// | 值 | 语义 | 数学 |
/// |----|------|------|
/// | *(不设)* / `signed` | 绕自身中心镜像（**改动前行为**） | `S(sx<0)` 直接进矩阵 |
/// | `abs` | 不镜像 | 右乘 `diag(-1,1,1)` 抵消那个负号 |
/// | `parent` | 绕**父坐标系原点**镜像（落点+内容一起翻） | `C·M·C`，`C = T(2·Px)·diag(-1,1,1)` |
/// | `place` | 先镜像再按 `size`/`alignment` 摆放（内容不翻、footprint 翻到锚点另一侧） | `M·diag(-1,1,1)·T(-w,0,0)` |
/// | `uv` | 只翻 uv（几何取 \|scale\|） | 与 `signed` 对 uv∈[0,1] 的整幅四边形**代数等价** |
/// 判据（本场景实测，2560x1440，屏上属性 `text="Mouse pad"` / `text5="0 1 1"`）：
///。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NegScaleMode {
    Signed,
    Abs,
    Parent,
    Place,
    UvOnly,
    /// 数据驱动版：**只在"整幅铺满画布"的层**上取 `abs`（判据见 [`neg_scale_fix`] 的
    /// `full_cover` 参数）。这样 17 条横切里只有真·全幅背景会变（本场景 = `1714`），
    /// 其它场景里作者手翻的小道具（如 `3571777169` 的 `тряпка`/`Petals`）一个像素都不动。
    Auto,
}

fn neg_scale_mode() -> NegScaleMode {
    match std::env::var("WE_NEG_SCALE")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "abs" => NegScaleMode::Abs,
        "parent" => NegScaleMode::Parent,
        "place" => NegScaleMode::Place,
        "uv" => NegScaleMode::UvOnly,
        "auto" => NegScaleMode::Auto,
        _ => NegScaleMode::Signed,
    }
}

/// 按 [`NegScaleMode`] 修正一个对象的世界矩阵（**只在 `scale.x < 0` 时动手**；
/// 正 scale 的对象无论哪个模式都是同一个矩阵 ⇒ 归零风险面）。
/// `sc` = 父链 `scale` 连乘（带符号），`parent_x` = 父坐标系原点的场景 x，
/// `half_w` = 对象在场景单位下的**半宽**（`size.x · |scale.x| / 2`，`Place` 用），
/// `full_cover` = 该层的四边行是否**完整覆盖画布**（`Auto` 的判据；只看几何，
/// 不看 id / 名字 / 贴图路径）。
fn neg_scale_fix(m: Mat4, sc: [f32; 3], parent_x: f32, half_w: f32, full_cover: bool) -> Mat4 {
    if sc[0] >= 0.0 {
        return m;
    }
    match neg_scale_mode() {
        NegScaleMode::Signed | NegScaleMode::UvOnly => m,
        // 抵消负号：M · diag(-1,1,1) 把 x 那一列的符号翻回来（= |scale.x|）
        NegScaleMode::Abs => m.mul(&Mat4::scale(-1.0, 1.0, 1.0)),
        // 绕父原点镜像**落点**（内容不翻）：中心 x → `2·Px − x`
        NegScaleMode::Parent => {
            let mut m2 = m.mul(&Mat4::scale(-1.0, 1.0, 1.0));
            m2.m[12] = 2.0 * parent_x - m.m[12];
            m2
        }
        // 先镜像再按 `size`/`alignment` 摆放：内容不翻，footprint 翻到锚点另一侧（中心 x → `x − w`）
        NegScaleMode::Place => {
            let mut m2 = m.mul(&Mat4::scale(-1.0, 1.0, 1.0));
            m2.m[12] = m.m[12] - 2.0 * half_w;
            m2
        }
        NegScaleMode::Auto => {
            if full_cover {
                m.mul(&Mat4::scale(-1.0, 1.0, 1.0))
            } else {
                m
            }
        }
    }
}

/// 一个对象的**原始 local frame**（`T(origin)·Rz(az)·Ry(ay)·Rx(ax)·S(scale)`，**不做**
/// 渲染器自己那套逐层 y 镜像）。
/// 为什么必须是"原始"：锚点矩阵就在**父 puppet 的模型空间**里，而我们的世界矩阵
/// `W_parent` 正是"父模型坐标 → 场景"的那张映射（网格顶点就是模型坐标，见 `mesh_norm_pos`
/// 的调用点）⇒ `W_parent · anchor · local_base` 与 OWE 的
/// `parent_world · anchor · LocalFrame` 同式。渲染器给非挂接子对象做的"逐层绕画布中心
/// 镜像"是本地实现口径，不能带进这个乘积（会把子对象整体推掉 `2·(scene_h/2 − y)`）。
fn local_raw_of(obj: &we_scene::scene::Object) -> Mat4 {
    let t = we_scene::scene::parse_vec3(&obj.origin).unwrap_or([0.0, 0.0, 0.0]);
    let s = we_scene::scene::parse_vec3(&obj.scale).unwrap_or([1.0, 1.0, 1.0]);
    let a = we_scene::scene::parse_vec3(&obj.angles).unwrap_or([0.0, 0.0, 0.0]);
    Mat4::translate(t)
        .mul(&Mat4::rotate_z(a[2]))
        .mul(&Mat4::rotate_y(a[1]))
        .mul(&Mat4::rotate_x(a[0]))
        .mul(&Mat4::scale(s[0], s[1], s[2]))
}

/// `animationlayers` 的一个条目是否进 `PoseLayer` 表。
/// **三处必须用同一条判据**：建网格（`build`）、挂接锚点（[`pose_layers_of`]）、
/// blend 逐帧脚本（[`blend_live_scripts`] 的下标映射）—— 下标对不上就会改错层。
fn anim_layer_kept(l: &we_scene::scene::AnimationLayer) -> bool {
    l.visible && l.blend_weight() > 0.0 && layer_sel_ok(l.animation)
}

/// 一个对象的 `animationlayers` → puppet 姿态层（与 `build` 里建网格时**同一套过滤/取值**；
/// 挂接锚点要用它才能跟着父 puppet 的动画走）。
fn pose_layers_of(obj: &we_scene::scene::Object) -> Vec<mdl::PoseLayer> {
    if std::env::var("WE_ANIM_POSE").ok().as_deref() == Some("0") {
        return Vec::new();
    }
    obj.animation_layers
        .iter()
        .filter(|l| anim_layer_kept(l))
        .map(|l| mdl::PoseLayer {
            id: l.animation,
            additive: l.is_additive(),
            blend: layer_blend(l) * blend_scale(),
            rate: l.rate(),
            start: anim_start_of(obj.id, l.animation),
        })
        .collect()
}

// ════════════════════════════════════════════════════════════════════════════
// 动画起始相位（`animationlayers[].visible` 的 `setFrame` 脚本）
// ════════════════════════════════════════════════════════════════════════════
// 现象（用户感知）：人物动作比 win 慢。速率本身没错（`fps=30`、`rate=1.0`、`position=t·rate`、
/// `frame = position·fps`、`g_Time` = 墙钟秒，t=12s 闭环）。
// 真因（数据）：`3704273480` 的物体 1016 有两条动画层，它们的 `visible` 不是布尔而是**字段脚本**：
// ```js
// export var scriptProperties = createScriptProperties
// .addSlider({ name: 'percentage', label: 'Initial progress', value: 1, min: 0, max: 1, ... })
// .finish;
// export function init(value) {
// const ani = 'addEndedCallback' in thisObject ? thisObject : thisObject.getAnimation
// ani.play
// ani.setFrame(ani.frameCount * scriptProperties.percentage) // ← 842: 0.80, 869: 0.70
// return value;
// }
// ```
// WE/OWE 语义（一手依据）：
// * 这条脚本在建场景时**跑一次**（`SceneScriptBinding.cpp:WirePuppetAnimationScripts` ⇒
// `MakeFieldScript(..., FieldKind::Bool, spec.ScriptProperties, ...)`）；
// * `ani.setFrame(f)` = `SceneAnimationPlayback::SetFrame`：`position = clamp(f,0,End)/fps`；
// * `frameCount` = `SceneAnimationPlayback::FrameCount` = `m_clip->End` = **MDLA 的 length**；
// * OWE 自己的单测就断言了这个形态（`scene_parse_tests.cpp` `VisibilityScriptsInitializeTheirOwnPlayback`：
// `offset 0.8 → Frame==96`、`0.36 → 43`，即 `len*f` 截断取整）。
// ⇒ win 的 842/869 从 **80%/70% 片长**处起步，我们从 0 帧 ⇒ 同一时刻的动作相位差 0.8/0.7 个周期。
// 实现（**从数据求，不写死 0.8/0.7**）：
// 1. 从 scene.json 原文里取每条 `animationlayers[].visible` 的 `script` + `scriptproperties`；
// 2. 抽出全部 `setFrame(<expr>)` 的实参，按"**帧表达式**"求值成 `(coef, const)`
// （`start_frame = coef·End + const`）—— 支持数字、`scriptProperties.<名>`、`frameCount`
// （可带前缀如 `ani.frameCount`）、`+ - * /` 与括号；**只**允许对 `frameCount` 线性
// （`frameCount*0.8`、`0.5+frameCount*0.3` 都行；`frameCount*frameCount` 报不支持）；
// 3. `End`（= MDLA 的 `length`）与 `fps` 由模型数据给 ⇒ 换成起始播放位置（见 `mdl::PoseLayer::start`）。
// 开关：`WE_ANIM_STARTPHASE=0` ⇒ 整表为空 = **逐字节回旧行为**（全部从 0 帧）。
static ANIM_START: std::sync::OnceLock<std::collections::HashMap<(i64, i64), (f32, f32)>> =
    std::sync::OnceLock::new();

fn anim_startphase_on() -> bool {
    env_flag("WE_ANIM_STARTPHASE", true)
}

/// `(对象 id, 动画 id)` → `setFrame` 帧表达式；没脚本 / 解析不出 ⇒ `(0,0)`（= 0 帧）。
fn anim_start_of(obj_id: i64, anim_id: i64) -> (f32, f32) {
    if !anim_startphase_on() {
        return (0.0, 0.0);
    }
    ANIM_START
        .get()
        .and_then(|m| m.get(&(obj_id, anim_id)).copied())
        .unwrap_or((0.0, 0.0))
}

/// 线性式 `(a, b)` = `a·frameCount + b`。
type Lin = (f32, f32);

/// 极小的四则表达式求值器（`frameCount` 当符号，其余必须是数字）。
/// 只需要覆盖 WE 字段脚本里出现的形态：`frameCount * sp.p`、`sp.p`、`12`、`frameCount*0.5+10`。
fn eval_frame_expr(s: &str, props: &serde_json::Value) -> Option<Lin> {
    struct P<'a> {
        b: &'a [u8],
        i: usize,
        props: &'a serde_json::Value,
    }
    impl<'a> P<'a> {
        fn ws(&mut self) {
            while self.i < self.b.len() && (self.b[self.i] as char).is_whitespace() {
                self.i += 1;
            }
        }
        fn peek(&mut self) -> Option<u8> {
            self.ws();
            self.b.get(self.i).copied()
        }
        /// expr := term (('+'|'-') term)*
        fn expr(&mut self) -> Option<Lin> {
            let mut v = self.term()?;
            loop {
                match self.peek() {
                    Some(b'+') => {
                        self.i += 1;
                        let r = self.term()?;
                        v = (v.0 + r.0, v.1 + r.1);
                    }
                    Some(b'-') => {
                        self.i += 1;
                        let r = self.term()?;
                        v = (v.0 - r.0, v.1 - r.1);
                    }
                    _ => return Some(v),
                }
            }
        }
        /// term := unary (('*'|'/') unary)*
        fn term(&mut self) -> Option<Lin> {
            let mut v = self.unary()?;
            loop {
                match self.peek() {
                    Some(b'*') => {
                        self.i += 1;
                        let r = self.unary()?;
                        // 只允许一侧是常数（对 frameCount 线性）
                        if v.0.abs() > 1e-9 && r.0.abs() > 1e-9 {
                            return None;
                        }
                        v = (v.0 * r.1 + r.0 * v.1, v.1 * r.1);
                    }
                    Some(b'/') => {
                        self.i += 1;
                        let r = self.unary()?;
                        if r.0.abs() > 1e-9 || r.1.abs() < 1e-12 {
                            return None;
                        }
                        v = (v.0 / r.1, v.1 / r.1);
                    }
                    _ => return Some(v),
                }
            }
        }
        /// unary := ('-')? atom
        fn unary(&mut self) -> Option<Lin> {
            if self.peek()? == b'-' {
                self.i += 1;
                let v = self.unary()?;
                return Some((-v.0, -v.1));
            }
            self.atom()
        }
        /// atom := number | '(' expr ')' | ident ('.' ident)*
        fn atom(&mut self) -> Option<Lin> {
            let c = self.peek()?;
            if c == b'(' {
                self.i += 1;
                let v = self.expr()?;
                if self.peek()? != b')' {
                    return None;
                }
                self.i += 1;
                return Some(v);
            }
            if c.is_ascii_digit() || c == b'.' {
                let st = self.i;
                while self.i < self.b.len()
                    && ((self.b[self.i] as char).is_ascii_digit() || self.b[self.i] == b'.')
                {
                    self.i += 1;
                }
                let t = std::str::from_utf8(&self.b[st..self.i]).ok()?;
                return Some((0.0, t.parse::<f32>().ok()?));
            }
            if c.is_ascii_alphabetic() || c == b'_' || c == b'$' {
                let st = self.i;
                while self.i < self.b.len() {
                    let ch = self.b[self.i] as char;
                    if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || ch == '.' {
                        self.i += 1;
                    } else {
                        break;
                    }
                }
                let id = std::str::from_utf8(&self.b[st..self.i]).ok()?;
                // `frameCount` / `ani.frameCount` / `thisObject.frameCount` ⇒ 符号
                if id.rsplit('.').next() == Some("frameCount") {
                    return Some((1.0, 0.0));
                }
                // `scriptProperties.<名>`
                if let Some(name) = id.strip_prefix("scriptProperties.") {
                    let v = self.props.get(name)?.as_f64()? as f32;
                    return Some((0.0, v));
                }
                return None;
            }
            None
        }
    }

    let mut p = P {
        b: s.as_bytes(),
        i: 0,
        props,
    };
    let v = p.expr()?;
    p.ws();
    if p.i != p.b.len() {
        return None;
    }
    Some(v)
}

/// 从脚本里抽出所有 `setFrame(<expr>)`，返回**第一个**解析成功的帧表达式。
fn parse_set_frame(script: &str, props: &serde_json::Value) -> Option<Lin> {
    let mut from = 0usize;
    while let Some(k) = script[from..].find("setFrame") {
        let start = from + k + "setFrame".len();
        let rest = &script[start..];
        let open = rest.find('(')?;
        // 配对括号（实参里可能有嵌套括号）
        let bytes = rest.as_bytes();
        let mut depth = 0i32;
        let mut end = None;
        for (i, &c) in bytes.iter().enumerate().skip(open) {
            match c {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else { return None };
        let arg = &rest[open + 1..end];
        if let Some(v) = eval_frame_expr(arg, props) {
            return Some(v);
        }
        from = start + end;
    }
    None
}

/// 解析 scene.json 原文，建"每条动画层的起始帧"表并装进 [`ANIM_START`]。
fn install_anim_start(json: &[u8]) -> usize {
    let mut map = std::collections::HashMap::new();
    let Ok(j) = serde_json::from_slice::<serde_json::Value>(json) else {
        return 0;
    };
    let Some(objs) = j.get("objects").and_then(|o| o.as_array()) else {
        return 0;
    };
    for obj in objs {
        let Some(oid) = obj.get("id").and_then(|v| v.as_i64()) else {
            continue;
        };
        let Some(layers) = obj.get("animationlayers").and_then(|v| v.as_array()) else {
            continue;
        };
        for l in layers {
            let Some(aid) = l.get("animation").and_then(|v| v.as_i64()) else {
                continue;
            };
            let Some(vis) = l.get("visible").filter(|v| v.is_object()) else {
                continue;
            };
            let Some(script) = vis.get("script").and_then(|v| v.as_str()) else {
                continue;
            };
            let empty = serde_json::Value::Object(Default::default());
            let props = vis.get("scriptproperties").unwrap_or(&empty);
            if let Some(e) = parse_set_frame(script, props) {
                if e.0.abs() > 1e-9 || e.1.abs() > 1e-9 {
                    map.insert((oid, aid), e);
                }
            }
        }
    }
    let n = map.len();
    let _ = ANIM_START.set(map);
    n
}

/// **诊断开关（默认 1.0 = 行为不变）**：统一缩放 puppet 加法层的 `blend`
/// （`WE_BLEND_SCALE`），用来做「blend 的取值口径」A/B。
fn blend_scale() -> f32 {
    use std::sync::OnceLock;
    static S: OnceLock<f32> = OnceLock::new();
    *S.get_or_init(|| {
        std::env::var("WE_BLEND_SCALE")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(1.0)
    })
}

/// **诊断开关（默认关，行为不变）**：按 MDLA 动画 id 过滤 puppet 的 `animationlayers`。
///
/// * `WE_LAYER_ONLY=1191,1300` —— 只启用列出的层（逐层归因：哪一层把骨链推歪）。
///
/// * `WE_LAYER_SKIP=609` —— 禁用列出的层（逐层 disable）。
/// 两者都不设时恒为 `true` ⇒ 与未加本函数时**逐位相同**。写它是为了给「9 条加法层叠加量」
/// 做逐层对照实验。
fn layer_sel_ok(id: i64) -> bool {
    use std::sync::OnceLock;
    let parse = |v: Option<String>| -> Option<Vec<i64>> {
        v.map(|s| {
            s.split(',')
                .filter_map(|x| x.trim().parse::<i64>().ok())
                .collect::<Vec<_>>()
        })
    };
    static ONLY: OnceLock<Option<Vec<i64>>> = OnceLock::new();
    static SKIP: OnceLock<Option<Vec<i64>>> = OnceLock::new();
    let only = ONLY.get_or_init(|| parse(std::env::var("WE_LAYER_ONLY").ok()));
    let skip = SKIP.get_or_init(|| parse(std::env::var("WE_LAYER_SKIP").ok()));
    if let Some(o) = only {
        if !o.contains(&id) {
            return false;
        }
    }
    if let Some(s) = skip {
        if s.contains(&id) {
            return false;
        }
    }
    true
}

/// `world_of` 的返回值：世界矩阵 + 这一层的**落点语义**（对齐偏移、粒子路径、调试都要用）。
struct WorldInfo {
    /// 世界矩阵（含父链、对象自己的 scale/angles；y 落点按下面的规则）。
    m: Mat4,
    /// resolveTransform 折叠出来的**未镜像**原点 —— 粒子系统要的正是这个
    /// （`CParticle` 的发射点是 y 向上的原始 resolve 结果，见粒子那一节的注释）。
    raw_origin: [f32; 3],
    /// **这个对象自己的 origin 有没有被那次 y 镜像翻过**。决定对齐偏移 `dy` 的符号：
    /// 只有被翻过的层才需要反号（WE `updateScenePosition` 是先算 quad 再镜像）。
    mirrored_here: bool,
    /// 父链 `scale` 的连乘 —— WE 的对齐偏移用 **`scaledSize = size × scale`**，不是裸 `size`。
    scale: [f32; 3],
}

/// 一个父链节点的 `origin.y` 落点：`scene_h − y` 那次镜像**只做在链的根上**
/// （见 `world_of` 里的长注释与录屏实测数字）。纯函数，便于单测。
fn chain_level_origin_y(
    origin_y: f32,
    is_root: bool,
    mirror_root_only: bool,
    flip_y: bool,
    scene_h: f32,
) -> f32 {
    if !flip_y {
        return origin_y;
    }
    if mirror_root_only && !is_root {
        origin_y // 子对象的 origin 是父框架里的偏移，不能再翻一次
    } else {
        scene_h - origin_y
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Bloom（WE 的 `general.bloom`）—— 照 linux-wallpaperengine 的做法
// ════════════════════════════════════════════════════════════════════════════
// 参考实现 `linux-wallpaperengine/src/WallpaperEngine/Render/Wallpapers/CScene.cpp`
// （`setupFramebuffers` 里注入 `effects/wpenginelinux/bloomeffect.json`）：
// 它用官方 assets 里现成的 4 个 shader 拼出一条"降采样 → 模糊 → 加回画面"的链，
// 目标 RT 是**画布/4**、**画布/8**、**画布/8**（`_rt_4FrameBuffer` / `_rt_8FrameBuffer` / `_rt_Bloom`），
// 最后一趟 `combine` 把泛光**加**到场景合成图上。参数取场景的
// `general.bloomstrength` / `bloomthreshold` / `bloomtint`。
// 这几段 GLSL 是**官方 shader 原文**（`assets/shaders/{downsample_quarter_bloom,
// downsample_eighth_blur_v,blur_h_bloom,combine}.{vert,frag}`），只是在预处理器里
// 保持和 WE 一致（`texSample2D` 由我们的前导块提供）。
const BLOOM_DSQ_VERT: &str = r#"
attribute vec3 a_Position;
attribute vec2 a_TexCoord;
uniform vec2 g_TexelSize;
varying vec2 v_TexCoord[4];
void main() {
    gl_Position = vec4(a_Position, 1.0);
    v_TexCoord[0] = a_TexCoord - g_TexelSize;
    v_TexCoord[1] = a_TexCoord + g_TexelSize;
    v_TexCoord[2] = a_TexCoord + vec2(-g_TexelSize.x, g_TexelSize.y);
    v_TexCoord[3] = a_TexCoord + vec2(g_TexelSize.x, -g_TexelSize.y);
}
"#;
const BLOOM_DSQ_FRAG: &str = r#"
varying vec2 v_TexCoord[4];
uniform sampler2D g_Texture0;
uniform float g_BloomStrength;
uniform float g_BloomThreshold;
uniform vec3 g_BloomTint;
void main() {
    vec3 albedo = texSample2D(g_Texture0, v_TexCoord[0]).rgb +
                  texSample2D(g_Texture0, v_TexCoord[1]).rgb +
                  texSample2D(g_Texture0, v_TexCoord[2]).rgb +
                  texSample2D(g_Texture0, v_TexCoord[3]).rgb;
    albedo *= 0.25;
    float scale = max(max(albedo.x, albedo.y), albedo.z);
    albedo *= saturate(scale - g_BloomThreshold);
    float grayscale = dot(vec3(0.2989, 0.5870, 0.1140), albedo);
    float sat = 1.0;
    albedo = -grayscale * sat + albedo * (1.0 + sat);
    gl_FragColor = vec4(max(CAST3(0), albedo * g_BloomStrength * g_BloomTint), 1.0);
}
"#;
const BLOOM_BLURV_VERT: &str = r#"
attribute vec3 a_Position;
attribute vec2 a_TexCoord;
uniform vec2 g_TexelSize;
varying vec2 v_TexCoord[13];
void main() {
    gl_Position = vec4(a_Position, 1);
    float localTexel = g_TexelSize.x * 8.0;
    v_TexCoord[0] = vec2(a_TexCoord.x - localTexel * 6.0, a_TexCoord.y);
    v_TexCoord[1] = vec2(a_TexCoord.x - localTexel * 5.0, a_TexCoord.y);
    v_TexCoord[2] = vec2(a_TexCoord.x - localTexel * 4.0, a_TexCoord.y);
    v_TexCoord[3] = vec2(a_TexCoord.x - localTexel * 3.0, a_TexCoord.y);
    v_TexCoord[4] = vec2(a_TexCoord.x - localTexel * 2.0, a_TexCoord.y);
    v_TexCoord[5] = vec2(a_TexCoord.x - localTexel, a_TexCoord.y);
    v_TexCoord[6] = vec2(a_TexCoord.x, a_TexCoord.y);
    v_TexCoord[7] = vec2(a_TexCoord.x + localTexel, a_TexCoord.y);
    v_TexCoord[8] = vec2(a_TexCoord.x + localTexel * 2.0, a_TexCoord.y);
    v_TexCoord[9] = vec2(a_TexCoord.x + localTexel * 3.0, a_TexCoord.y);
    v_TexCoord[10] = vec2(a_TexCoord.x + localTexel * 4.0, a_TexCoord.y);
    v_TexCoord[11] = vec2(a_TexCoord.x + localTexel * 5.0, a_TexCoord.y);
    v_TexCoord[12] = vec2(a_TexCoord.x + localTexel * 6.0, a_TexCoord.y);
}
"#;
const BLOOM_BLURH_VERT: &str = r#"
attribute vec3 a_Position;
attribute vec2 a_TexCoord;
uniform vec2 g_TexelSize;
varying vec2 v_TexCoord[13];
void main() {
    gl_Position = vec4(a_Position, 1);
    float localTexel = g_TexelSize.y * 8.0;
    v_TexCoord[0] = vec2(a_TexCoord.x, a_TexCoord.y - localTexel * 6.0);
    v_TexCoord[1] = vec2(a_TexCoord.x, a_TexCoord.y - localTexel * 5.0);
    v_TexCoord[2] = vec2(a_TexCoord.x, a_TexCoord.y - localTexel * 4.0);
    v_TexCoord[3] = vec2(a_TexCoord.x, a_TexCoord.y - localTexel * 3.0);
    v_TexCoord[4] = vec2(a_TexCoord.x, a_TexCoord.y - localTexel * 2.0);
    v_TexCoord[5] = vec2(a_TexCoord.x, a_TexCoord.y - localTexel);
    v_TexCoord[6] = vec2(a_TexCoord.x, a_TexCoord.y);
    v_TexCoord[7] = vec2(a_TexCoord.x, a_TexCoord.y + localTexel * 1.0);
    v_TexCoord[8] = vec2(a_TexCoord.x, a_TexCoord.y + localTexel * 2.0);
    v_TexCoord[9] = vec2(a_TexCoord.x, a_TexCoord.y + localTexel * 3.0);
    v_TexCoord[10] = vec2(a_TexCoord.x, a_TexCoord.y + localTexel * 4.0);
    v_TexCoord[11] = vec2(a_TexCoord.x, a_TexCoord.y + localTexel * 5.0);
    v_TexCoord[12] = vec2(a_TexCoord.x, a_TexCoord.y + localTexel * 6.0);
}
"#;
/// `downsample_eighth_blur_v.frag` 与 `blur_h_bloom.frag` 的正文**逐字节相同**
/// （13 抽头高斯核；方向由上面两个 vert 决定）—— 直接共用一份。
const BLOOM_BLUR_FRAG: &str = r#"
varying vec2 v_TexCoord[13];
uniform sampler2D g_Texture0;
void main() {
    vec3 albedo = texSample2D(g_Texture0, v_TexCoord[0]).rgb * 0.006299 +
                  texSample2D(g_Texture0, v_TexCoord[1]).rgb * 0.017298 +
                  texSample2D(g_Texture0, v_TexCoord[2]).rgb * 0.039533 +
                  texSample2D(g_Texture0, v_TexCoord[3]).rgb * 0.075189 +
                  texSample2D(g_Texture0, v_TexCoord[4]).rgb * 0.119007 +
                  texSample2D(g_Texture0, v_TexCoord[5]).rgb * 0.156756 +
                  texSample2D(g_Texture0, v_TexCoord[6]).rgb * 0.171834 +
                  texSample2D(g_Texture0, v_TexCoord[7]).rgb * 0.156756 +
                  texSample2D(g_Texture0, v_TexCoord[8]).rgb * 0.119007 +
                  texSample2D(g_Texture0, v_TexCoord[9]).rgb * 0.075189 +
                  texSample2D(g_Texture0, v_TexCoord[10]).rgb * 0.039533 +
                  texSample2D(g_Texture0, v_TexCoord[11]).rgb * 0.017298 +
                  texSample2D(g_Texture0, v_TexCoord[12]).rgb * 0.006299;
    gl_FragColor = vec4(albedo, 1.0);
}
"#;
const BLOOM_COMBINE_VERT: &str = r#"
attribute vec3 a_Position;
attribute vec2 a_TexCoord;
varying vec2 v_TexCoord;
void main() {
    gl_Position = vec4(a_Position, 1.0);
    v_TexCoord = a_TexCoord;
}
"#;
/// 泛光参数（场景 `general` + 环境变量覆盖）
#[derive(Debug, Clone, Copy)]
struct BloomCfg {
    on: bool,
    strength: f32,
    threshold: f32,
    tint: [f32; 3],
}

impl BloomCfg {
    /// `WE_BLOOM=1` 开（**默认关**：默认路径必须逐字节不变，见六张回归护栏）。
    /// `WE_BLOOM_STRENGTH` / `WE_BLOOM_THRESHOLD` 覆盖场景值（用来判"量够不够"）。
    fn from_general(g: &we_scene::scene::General) -> BloomCfg {
        let on = matches!(
            std::env::var("WE_BLOOM").ok().as_deref(),
            Some("1") | Some("on") | Some("true") | Some("yes")
        );
        let f = |k: &str, d: f32| {
            std::env::var(k)
                .ok()
                .and_then(|s| s.trim().parse::<f32>().ok())
                .unwrap_or(d)
        };
        BloomCfg {
            on,
            strength: f("WE_BLOOM_STRENGTH", g.bloom_strength),
            threshold: f("WE_BLOOM_THRESHOLD", g.bloom_threshold),
            tint: parse_tint(&g.bloom_tint),
        }
    }
}

/// `bloomtint`（`"r g b"`）→ 三元组；解析不出来就取白（WE 的默认染色）。
fn parse_tint(s: &str) -> [f32; 3] {
    let v: Vec<f32> = s
        .split_whitespace()
        .filter_map(|t| t.parse().ok())
        .collect();
    if v.len() >= 3 {
        [v[0], v[1], v[2]]
    } else {
        [1.0, 1.0, 1.0]
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Glow（"按光斑/亮部铺柔光"）—— 补 Windows 那团吊灯/烛光大光晕
// ════════════════════════════════════════════════════════════════════════════
// 为什么不是 bloom：本场景声明的 bloom 量（0.07）实测**等于没有**（单帧最大差 1/255），
// 把阈值降到 0.15、强度开到 4，吊灯窗内的暖色面积也只从 1.54% 升到 2.14%，
// 而 Windows 1.jpg 是 17.65% —— 差 8 倍。原因（本次量化确认）：
// · 我们的火苗是**小而锐**的 sprite；Windows 是**一大团柔光**（窗内最大暖色连通块
// 10226px / 等效半径 57px，占全部暖像素的 82% —— 一整片，不是几个小点）；
// · 阈值型 bloom 只会把"本来就很亮"的像素糊一点，半径也小（1/8 缓冲 13 抽头）。
// ⇒ 需要一条**"在每个发光光源处铺一朵大柔光斑"**的机制（不是全局阈值泛光）。
// 光源从哪来：场景的 9 个 `Torch` 粒子系统（`particle/fire/fire1`，加法混合）
// 就是那些烛火；它们的**粒子位置**已经是场景坐标（与绘制用的是同一套变换），
// 所以直接在每个"暖色粒子"处画一个加法大光斑即可 —— 数据驱动，不认对象名。
// 默认**关**：不开时 draw_frame 一字节不改（六张 scene_score 是护栏）。
const GLOW_VERT: &str = r#"
attribute vec3 a_Position;
attribute vec4 a_TexCoordVec4;
attribute vec4 a_Color;
uniform mat4 g_ModelViewProjectionMatrix;
varying vec2 v_TexCoord;
varying vec4 v_Color;
void main() {
    vec2 c = a_TexCoordVec4.xy;
    float sz = a_TexCoordVec4.w;
    vec3 p = a_Position + vec3((c.x - 0.5) * sz, (c.y - 0.5) * sz, 0.0);
    gl_Position = mul(vec4(p, 1.0), g_ModelViewProjectionMatrix);
    v_TexCoord = c;
    v_Color = a_Color;
}
"#;
const GLOW_FRAG: &str = r#"
varying vec2 v_TexCoord;
varying vec4 v_Color;
uniform sampler2D g_Texture0;
void main() {
    vec4 t = texSample2D(g_Texture0, v_TexCoord);
    // 加法混合（SRC_ALPHA/ONE）⇒ 贡献 = rgb * alpha
    gl_FragColor = vec4(v_Color.rgb, t.a * v_Color.a);
}
"#;

/// 光斑参数（`WE_GLOW=1` 开；其余旋钮用于标定）
#[derive(Debug, Clone, Copy)]
struct GlowCfg {
    on: bool,
    /// **全屏辉光**强度（暖色加权大半径模糊那一趟的幅度）
    strength: f32,
    /// 全屏辉光的亮度阈值（比 bloom 低得多：bloom 是 0.9，这里 0.15 起）
    threshold: f32,
    /// 降采样倍数：缓冲 = 输出/div，模糊步长又乘 8 ⇒ 等效半径 ≈ div×8×6 像素。
    div: f32,
    /// 暖色加权系数（`(R−B)×k` 越大越保留；冷白石拱被压掉）
    warmk: f32,
    /// 叠回屏幕时乘在 glow 上的**暖色调**（默认 `1,1,1` = 旧行为）。
    /// 为什么要它：Windows 那团光是**深橙**（窗内暖像素均值 RGB 126/81/56、luma 91.5），
    /// 而我们 blur 出来的源色偏黄白（G 高）⇒ 叠上去 G 抬太多、整片发白、窗内均值 luma 被
    /// 推到 62（Windows 48）。把 G/B 压下去 ⇒ **暖像素占比不变**而亮度回来 ⇒ 40×40 块差不再变差。
    tint: [f32; 3],
    /// 模糊半径倍数：模糊的 `g_TexelSize` 乘它 ⇒ 等效半径 ≈ 48×scale 像素（div 只影响缓冲分辨率）。
    scale: f32,
    /// 是否**同时**在每个暖色粒子处铺硬光斑（"每个火把一个光源"）
    particles: bool,
    /// 粒子光斑**半径**（场景单位；绘制时全宽 = 2×radius）
    radius: f32,
    /// 粒子光斑强度
    pstrength: f32,
    /// 径向衰减指数（贴图 alpha = (1-d)^falloff）
    falloff: f32,
    /// `true` = **glow 在曝光之前**（场景空间叠加，再走 present 的显示编码）。
    /// 为什么这样更对：曝光/色调映射是**显示变换**（HDR→SDR），它必须作用在**全部场景光**之上；
    /// glow 是场景光 ⇒ 必须排在编码之前。旧的"加在曝光之后"是加法叠在已编码的画面上，
    /// 既物理错位、又会过冲（实测 107.8 vs Windows 88.9）。`WE_GLOW_ORDER=post` 回旧行为做 A/B。
    order_pre: bool,
    /// **光源模型**（`WE_GLOW_SOURCE`，默认 `Local` = 点光源/局部分析）。见 `GlowSource`。
    source: GlowSource,
    /// 局部性判据的**大尺度半径**（像素；默认 140）：成片亮区的等效半径 ≥ 它 ⇒ 门归零。
    /// 实测掩膜连通块：IP 最大 r37px，A/B 的大片 r242–261 ⇒ 尺度放 60–120px 之间就能分开。
    loc_l: f32,
    /// 局部性判据里**背景**分量的抑制系数 `k`（默认 0.7；越大越严）。
    loc_k: f32,
    /// 局部性判据的增益 `gain`（默认 1.2；越大越接近硬门）。
    loc_gain: f32,
    /// 归一化分母的下限（默认 0.02）：防止掩膜接近 0 时噪声被放大。
    loc_floor: f32,
    /// **绝对背景门**的"归零"阈值（默认 0.11）：掩膜的软背景 `ph` 达到它 ⇒ 门 = 0。
    /// 依据（实测探针填充）：IP 的烛火 p50=0.027、A 的暖灯/皮肤 0.086、B 的白天 0.216。
    loc_bg: f32,
    /// 绝对背景门的"起压"阈值（默认 0.04）：`ph` ≤ 它 ⇒ 门 = 1（完全放行）。
    loc_bg0: f32,
}

/// 标定出的 glow 暖色调（见 `GlowCfg` 的注释）：`WE_GLOW_TINT=1,1,1` 可退回"不加色调"的旧行为。
const GLOW_TINT_DEFAULT: [f32; 3] = [1.0, 0.60, 0.35];

/// `WE_GLOW_TINT=r,g,b`（叠回时乘在 glow 上的暖色调）。缺省/写错 ⇒ `d`（本场景的标定值）。
/// 负值夹到 0（否则会从画面里**减光**）。纯函数 ⇒ 可单测。
fn parse_glow_tint(s: Option<&str>, d: [f32; 3]) -> [f32; 3] {
    let Some(s) = s else { return d };
    let p: Vec<f32> = s
        .split(',')
        .filter_map(|t| t.trim().parse::<f32>().ok())
        .collect();
    if p.len() == 3 {
        [p[0].max(0.0), p[1].max(0.0), p[2].max(0.0)]
    } else {
        d
    }
}

/// glow 的**光源模型**（`WE_GLOW_SOURCE`）—— 本次的核心改动。
/// 背景（为什么旧模型坏了）：旧实现把"画面里**又亮又暖**的像素"整片当光源。烛火暗场（Ice Princess）
/// 上够用（场里只有火苗/吊灯是亮暖的），但**换场景就坏**：
/// · 海滩白天（3704273480）：满屏天空/沙滩/皮肤都是亮暖像素 ⇒ 全被判成光源 ⇒ 整帧过曝；
/// · 室内暖灯夜场（2887537617）：被暖灯照亮的窗帘/窗是一大片亮暖区 ⇒ 出现不自然的橙色光晕。
/// `Local`（默认）给这条通道加**局部性约束**：只有"小、亮、**周围迅速变暗**"的孤立光斑
/// （烛火/火把/灯）才铺光，成片亮区几乎不铺 —— 这就是"点光源"模型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlowSource {
    /// 只按暖色粒子铺光斑（`WE_GLOW_PARTICLES` 那条实验通道；不跑全屏通道）
    Points,
    /// 全屏"亮暖像素"（**旧行为**；`WE_GLOW_SOURCE=screen` 可一键回退做 A/B）
    Screen,
    /// **默认**：全屏通道 + 局部性约束（窄/宽模糊的 DoG 门）＝ 点光源模型
    Local,
}

/// `WE_GLOW_SOURCE=screen|points|local`。缺省/写错 ⇒ `Local`（本次的默认）。
fn parse_glow_source(s: Option<&str>) -> GlowSource {
    match s {
        Some("screen") => GlowSource::Screen,
        Some("points") => GlowSource::Points,
        Some("local") => GlowSource::Local,
        _ => GlowSource::Local,
    }
}

impl GlowCfg {
    fn from_env() -> GlowCfg {
        let on = matches!(
            std::env::var("WE_GLOW").ok().as_deref(),
            Some("1") | Some("on") | Some("true") | Some("yes")
        );
        let f = |k: &str, d: f32| {
            std::env::var(k)
                .ok()
                .and_then(|s| s.trim().parse::<f32>().ok())
                .unwrap_or(d)
        };
        let src = parse_glow_source(std::env::var("WE_GLOW_SOURCE").ok().as_deref());
        GlowCfg {
            on,
            // 标定值（Ice Princess 2861113973，**吊灯归位后**重标，2560x1440 / 150 帧 = t5s）：
            // 基线（无 glow）吊灯窗 亮 4.26% / 暖 5.72% vs Windows 8.54% / 19.02%（差 2.00x / 3.33x）；
            // scale 3.5 + strength 3.2 + tint 1/0.60/0.35 ⇒ **亮 7.88% / 暖 18.15%**
            // （差 1.08x / 1.05x，两项都进 1.5 倍内），全图均值 78.6/80.4/81.1（Windows 88.9/93.4/96.7，仍在其下）、
            // 截顶 0.13%（Windows 3.19%）、整帧 40x40 块差 40.81 → 39.38（全图不变差）。
            // 屏上口径（glow 在曝光之前 + 重拟合曝光 `WE_EXPOSURE=1.0,1.19,-0.25`）：
            // 亮 12.24% / 暖 18.89%，全图 88.8/93.0/96.4（**三通道都不超 Windows**）。
            // 详数与依据。
            strength: f("WE_GLOW_STRENGTH", 3.2),
            threshold: f("WE_GLOW_THRESHOLD", 0.35),
            div: f("WE_GLOW_DIV", 8.0),
            warmk: f("WE_GLOW_WARMK", 3.0),
            tint: parse_glow_tint(std::env::var("WE_GLOW_TINT").ok().as_deref(), GLOW_TINT_DEFAULT),
            scale: f("WE_GLOW_SCALE", 3.5),
            particles: src == GlowSource::Points
                || std::env::var("WE_GLOW_PARTICLES").ok().as_deref() == Some("1"),
            radius: f("WE_GLOW_RADIUS", 210.0),
            pstrength: f("WE_GLOW_PSTRENGTH", 0.15),
            falloff: f("WE_GLOW_FALLOFF", 2.0),
            order_pre: std::env::var("WE_GLOW_ORDER").ok().as_deref() != Some("post"),
            source: src,
            // 局部性约束默认值：
            // 大尺度半径 140px（成片区 r242–261 归零、烛火 r≤37 保留）；
            // 门 = saturate((小 − 大×0.7) / max(小, 0.02) × 1.2)。
            loc_l: f("WE_GLOW_LOCL", 140.0),
            loc_k: f("WE_GLOW_LOCTH", 0.7),
            loc_gain: f("WE_GLOW_LOCGAIN", 1.2),
            loc_floor: f("WE_GLOW_LOCFLOOR", 0.02),
            loc_bg: f("WE_GLOW_BG", 0.11),
            loc_bg0: f("WE_GLOW_BG0", 0.04),
        }
    }
}

/// 全屏辉光的降采样趟：抽 4 点平均（复用 bloom 的 `downsample_quarter` 顶点，半径靠目标更小获得），
/// 然后**按暖色加权 + 低阈值**保留烛光种子（而不是 bloom 那种高阈值只留最亮）。
const GLOW_DS_FRAG: &str = r#"
varying vec2 v_TexCoord[4];
uniform sampler2D g_Texture0;
uniform float g_GlowThreshold;
uniform float g_GlowWarm;
void main() {
    vec3 c = (texSample2D(g_Texture0, v_TexCoord[0]).rgb +
              texSample2D(g_Texture0, v_TexCoord[1]).rgb +
              texSample2D(g_Texture0, v_TexCoord[2]).rgb +
              texSample2D(g_Texture0, v_TexCoord[3]).rgb) * 0.25;
    float l = max(max(c.r, c.g), c.b);
    float amt = smoothstep(g_GlowThreshold, g_GlowThreshold + 0.35, l);
    // 暖色加权：**冷/中性区完全不贡献**（不是留 30% 底）—— 否则整幅（天窗/雾/石拱）
    // 都会被抬亮，把全图均值从 68 推到 82，那是"打爆整帧"而不是光晕。
    float warm = saturate((c.r - c.b) * g_GlowWarm);
    gl_FragColor = vec4(c * amt * warm, 1.0);
}
"#;
/// 加法叠回屏幕的那一趟：强度在这里乘（混合 = SRC_ALPHA/ONE ⇒ 贡献 = rgb×alpha）。
const GLOW_COMBINE_FRAG: &str = r#"
varying vec2 v_TexCoord;
uniform sampler2D g_Texture0;
uniform float g_GlowStrength;
uniform vec3 g_GlowTint;
void main() {
    gl_FragColor = vec4(texSample2D(g_Texture0, v_TexCoord).rgb * g_GlowStrength * g_GlowTint, 1.0);
}
"#;
/// **局部性判据趟（点光源模型的核心）**：输入 `g_Texture0` = 候选掩膜（小尺度），
/// `g_Texture1` = 同一掩膜的**大尺度**模糊（= "局部背景"）。输出 = 小尺度 × 门。
/// 门 = `saturate((小 − 大×k) / max(小, floor) × gain)` —— **归一化**（除以"小"），
/// 所以它测的是**相对局部对比度**、不是绝对亮度：
/// · 小而亮的孤立光斑（烛火/火把/灯）：小尺度（近点）远高于大尺度（背景）⇒ 门 ≈ 1，保留；
/// · 成片的亮暖区（太阳/沙滩/皮肤/被暖灯照亮的窗帘）：小 ≈ 大 ⇒ 门 ≈ 0，压掉。
/// 为什么归一化 + 尺度要对：绝对阈值型判据（`小 − 大×k` 不除）在 IP 上会把烛火（种子低）也压掉，
/// 在 B 上又压不住皮肤（种子高）—— 实测两种场景的掩膜**灰度差一个量级**，
/// 但**连通块尺寸差两个量级**（IP 最大 4381px/r37 vs A、B 的大片 r242–261）⇒ 只有"相对对比度 ×
/// 合适尺度"能同时分开。尺度由 `WE_GLOW_LOCL`（大尺度半径像素）定：成片区半径 ≥ 它就归零。
const GLOW_LOCALITY_FRAG: &str = r#"
varying vec2 v_TexCoord;
uniform sampler2D g_Texture0;
uniform sampler2D g_Texture1;
uniform float g_GlowLocK;
uniform float g_GlowLocGain;
uniform float g_GlowLocFloor;
uniform float g_GlowLocBg;
uniform float g_GlowLocBg0;
void main() {
    vec3 s = texSample2D(g_Texture0, v_TexCoord).rgb;
    vec3 b = texSample2D(g_Texture1, v_TexCoord).rgb;
    float sm = max(max(s.r, s.g), s.b);
    float bm = max(max(b.r, b.g), b.b);
    float loc = saturate((sm - bm * g_GlowLocK) / max(sm, g_GlowLocFloor) * g_GlowLocGain);
    // 绝对背景门（smoothstep）：点光源坐在**低填充**的背景里。B（白天）连"暗部"都是亮的 ⇒ 整个被压掉。
    // 用 smoothstep 而不是线性斜坡：IP 的填充 ~0.03 要**完全放行**（bg=1），A/B 的 0.09–0.28 要**归零**。
    float bg = 1.0 - smoothstep(g_GlowLocBg0, g_GlowLocBg, bm);
    gl_FragColor = vec4(s * (loc * bg), 1.0);
}
"#;

/// 链内趟的**遮罩 V 补偿**（`WE_MASK_VFLIP`，默认开 = 新行为；`=0` 回旧）。
/// 为什么需要：链内每一趟的四边形是**全屏 [-1,1] + V 反向**（`WE_FX_QUAD_FLIP`，
/// 见 `draw_pass` 里那段注释 —— 它补偿的是"D3D 下 RT 与贴图朝向一致、GL 下相反"）。
/// V 反向对**本趟的输入 RT** 是对的（RT 内容按 GL 行序存 ⇔ 反向取样才正），
/// 但同一条 `v_TexCoord` 也被 `waterwaves` / `waterripple` / `iris` / `shake` 这类
/// 特效用来采样**普通遮罩贴图**（`uniform sampler2D g_Texture1; // {"combo":"MASK"}`）——
/// 普通贴图是**不反向**上传的 ⇒ 遮罩被读成**上下镜像** ⇒ 遮罩落在关于水平中线的镜像位置。
/// 实测（`3704273480` 「身体---拆分」id=1016，主题2）：
/// 滑板右尖（画布 uv≈(0.96,0.23)）处遮罩被读成 252/255（= 全强度），
/// 而遮罩贴图**同一 uv 的原始像素是 0**、把 v 取反（0.77）才是 251 ⇒ 镜像确凿。
/// 症状 = 用户的"滑板尖端水波/果冻"（特效本来该作用在她头发那一侧）。
/// 做法：只改**遮罩槽**的采样坐标（`v_TexCoord.zw` ⇒ `vec2(z, 1-w)`），
/// 不碰内容采样、不碰其它 uniform；`mask_units` 为空 ⇒ 一个字节都不动。
fn mask_vflip_rewrite(fs: &str, mask_units: &[u32]) -> (String, usize) {
    mask_axis_rewrite(fs, mask_units, true, false)
}

/// 按轴改写遮罩槽的采样坐标：`axis_u` / `axis_v` 任一为真即改写
/// （`vec2(u,v)` → `vec2(1-u, 1-v)` 的对应分量）。默认行为由
/// `mask_vflip_rewrite` 调用（只翻 v），`WE_MASK_UFLIP` 走 u 档。
fn mask_axis_rewrite(fs: &str, mask_units: &[u32], axis_u: bool, axis_v: bool) -> (String, usize) {
    if mask_units.is_empty() || !(axis_u || axis_v) {
        return (fs.to_string(), 0);
    }
    let repl = match (axis_u, axis_v) {
        (true, true) => "vec2(1.0 - v_TexCoord.z, 1.0 - v_TexCoord.w)",
        (true, false) => "vec2(1.0 - v_TexCoord.z, v_TexCoord.w)",
        _ => "vec2(v_TexCoord.z, 1.0 - v_TexCoord.w)",
    };
    let mut out = String::with_capacity(fs.len() + 64);
    let mut hits = 0usize;
    for line in fs.split_inclusive('\n') {
        let is_mask_line = mask_units
            .iter()
            .any(|u| line.contains(&format!("g_Texture{u}")));
        if is_mask_line && line.contains("v_TexCoord.zw") {
            hits += line.matches("v_TexCoord.zw").count();
            out.push_str(&line.replace("v_TexCoord.zw", repl));
        } else {
            out.push_str(line);
        }
    }
    (out, hits)
}

/// `WE_MASK_ATLAS_SPACE`（默认**关**）：把**遮罩采样**从「链缓冲/取景框空间」
/// 仿射到「图集（作者画布）空间」—— **底图趟照旧画网格**，动画/蒙皮一动不动。
///
/// # 为什么需要（承接 的定量结论）
/// 遮罩是**画在图集空间**里的：`iris_mask_064d2302` 的两个亮点正落在**眼睛**上、
/// `waterwaves_mask_84ece252` 落在**纱衣**上。
/// 但本层（`3704273480`「身体---拆分」）的链缓冲不是画布，而是
/// 【对象矩形 ∪ 网格包围盒】的**取景框**（实测 `2539×3570`，画布 `2140×3500`：
/// `fit = x[-1.314,1.059] y[-1.020,1.020]`）。链内每一趟的四边形铺满缓冲
/// ⇒ `v_TexCoord.zw ∈ [0,1]` 是**取景框 uv**，而作者把遮罩画在**画布 uv** 上。
/// 旧代码把两者当同一个东西 ⇒ 遮罩被横向拉宽 `2539/2140 = 1.19` 倍、整体偏移 14%
/// ⇒ 11 张遮罩并集只有 **6.7%** 落在内容像素上（ 同口径记 4.9%）
/// `iris` / `waterripple` / 纱衣三张**连一个内容像素都碰不到**（0.0%），
/// 而 `03fcd88f` 恰好压在**滑板右尖**上 ⇒ 用户的"板尖自激水波"。
/// 那条路把**底图趟**从"画网格"换成"平铺图集四边形" ⇒ 缓冲里是静止的画，
/// **网格动画整条被跳过** ⇒ 屏上人物不动、特效（iris/水波/纱衣）跑在静止画上，
/// 用户看到的就是"肚子上的肉在动、胯在扭"（特效有相位、内容没相位）。
/// **铁律：底图趟必须继续画网格（带动画）。** 本开关只改**遮罩的采样坐标**：
/// 顶点着色器里的 `v_TexCoord.xy`（内容/RT 采样）与网格顶点一个字节都不碰。
///
/// # 仿射怎么来
/// 取景框在**归一化对象坐标**里是 `fit = [x0,x1,y0,y1]`（±1 = 对象矩形的边），
/// 画布 uv 与归一化坐标的关系（网格 bind 顶点 `pos == (u−0.5)·size` 的同一套）：
/// ```text
/// u_canvas = (1 + x)/2
/// v_canvas = (1 − y)/2 ← y 向上、v 向下
/// ```
/// 链内趟的四边形是 **V 反向**那套（`WE_FX_QUAD_FLIP` 默认开，见 `mask_vflip_rewrite`），
/// 即 `v_TexCoord.w = 0` 落在**取景框下边** `y0`、`1` 落在 `y1`。于是：
/// ```text
/// u_m = (1 + x0)/2 + (x1−x0)/2 · u_f
/// v_m = (1 − y0)/2 − (y1−y0)/2 · v_f
/// ```
/// `WE_MASK_ATLAS_V=up` 换成"非反向四边形"的 `v` 那一支（A/B 用）。
/// **作用域**（默认）= 链里任一 pass 的纹理是 `_rt_*` 的层 —— 与 `SCOPE=narrow`
/// 同一条**数据依赖**判据（不按名字/id/壁纸 id）：这类层的链里内容、作者画的遮罩、
/// 被引用层的画布三者**本来就是三种空间**，只有它们的遮罩需要这次换算。
fn mask_atlas_space_on() -> bool {
    // 默认**开**：遮罩覆盖 6.6% → 57.1%（iris 0%→100%、纱衣 0%→83.6%）
    // 滑板尖 TIP 10.94 → 0.00、动画顶点位移与主版本逐位相同（底图趟一字节未动）。
    // `WE_MASK_ATLAS_SPACE=0` 回旧路径。
    match std::env::var("WE_MASK_ATLAS_SPACE").ok().as_deref() {
        Some("0") | Some("off") | Some("false") => false,
        _ => true,
    }
}

/// `WE_MASK_ATLAS_MODE` —— 开关打开后走哪条实现：
///
/// * `mesh`（默认）—— **把遮罩按同一份网格几何画进帧空间**（逐骨对齐，见字段
/// `cur_mask_warp_src` 的注释）；
///
/// * `affine` —— 只用取景框→画布的**全局仿射**改写采样坐标。留作 A/B 与反证：
/// 本模型的底图趟画的是**装配后**网格（bind 残差 0.746），图集↔帧不是一个仿射
/// ⇒ 实测 `iris` 落到画布外的空白/板缘（并集 27.3% 是"总量对、位置错"的假象）。
fn mask_atlas_mode() -> u8 {
    if !mask_atlas_space_on() {
        return 0;
    }
    match std::env::var("WE_MASK_ATLAS_MODE")
        .ok()
        .map(|v| v.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("affine") => 2,
        _ => 1,
    }
}

/// `WE_WAVE_NPHASE` —— 水波族 shader 的**时间项口径**（`g_Time × g_Speed` 那一项）。
///
/// # 一句话
/// 头发在抖**不是**帧率、**也不是**"水波里有个更快的噪声相位"。`effects/waterwaves`
/// 的全部时间输入只有一项 `g_Time * g_Speed`（本场景 7 条 = 2/2.6/3/3/3/4/4 rad/s
/// ⇒ **0.32–0.64 Hz、周期 1.57–3.14 s**，全是慢速行波，频谱
/// 说明）；毛病在于**这条慢波只压在头发/纱衣上、不压躯干**
/// （7 张遮罩在躯干像素上恒为 0）⇒ 同一个人身上两套互不相关的时间场
/// （骨骼 8 s/12 s 循环 vs 水波 1.6–3.1 s），交界处每帧相对错开 1–2 px。
///
/// # 出处（一手，逐行）
/// ```glsl
/// // assets/effects/waterwaves/shaders/effects/waterwaves.frag:47
/// float distance = g_Time * g_Speed + dot(texCoordMotion, v_Direction) * g_Scale;
/// ...
/// texCoord += val1 * s1 * offset * strength * mask; // strength = g_Strength²
/// ```
///
/// * **没有** `u_noiseMovement` / 噪声贴图（`effects/dotmatrix` 那条路的形态）——
/// `grep -c noise` 在该 shader 里 = 0。
///
/// * `TIMEOFFSET` 那一支（`distance += texSample2D(g_Texture2, …).r * 2π`）在本场景
/// 作者没给贴图（材质声明 `util/black`）⇒ 恒 0。实测
/// `WE_FX_COMBOS=waterwaves:TIMEOFFSET=0` 与默认 **md5 逐位相同**
/// （`d3b387cb…`，t=3.4、骨架冻结）⇒ "遮罩带着噪声相位"这一假设**已否证**。
///
/// # 与点点那条修法的异同（`WE_DOT_NPHASE=zero`）
/// 同：都是把"另一个与骨骼无关的时间场"冻住，**图案保留、动画去掉**。
/// 异：点点的相位项乘在**噪声贴图的 uv** 上（`u_noiseMovement * g_Time`，相位漂出
/// 贴图 ⇒ 尺寸场会永久塌平，所以"归一化"有意义）；水波的时间项本来就是 `sin` 的
/// **角度**，按自身周期归一化是**代数恒等**（`sin(θ+2πk)=sin θ`）⇒ `norm` 档只为
/// A/B 留证"归一化不改一个像素"，真正起作用的是 `hold`（时间项 → 0）。
///
/// # 档位
///
/// * `shape`（**默认，仅骨骼网格层**）—— ：把水波从**覆盖/轮廓**里摘掉
/// 内容颜色照旧按位移后的坐标采。`rgb` 取 `texCoord`（位移后）、`alpha` 取原 uv
/// `v_TexCoord.xy`，按预乘 alpha 的 over 合回原像素并夹到 `rgb ≤ a`
/// ⇒ **轮廓 = 静止形状**，水波的**颜色/折射流动保留**（`waterwaves` 是纯位移
/// shader，位移一归零就什么都不剩，见 `amp0`）。
///
/// * `amp0` —— 位移项直接归零（`texCoord += vec2(0.0)`）⇒ 该趟输出 = 输入，
/// 水波在**这一层上完全消失**。留作 A/B。
///
/// * `hold` —— 旧默认：只把 `g_Time` 冻成 0。**已证不够**：`sin(实测距离)`
/// 里时间项归零后，空间项 `dot(texCoordMotion, v_Direction) * g_Scale` 仍是**满幅**
/// ⇒ 头发被**永久**推成波浪形且不回位（用户原话："有撕裂的物体没有复位"）。
/// 保留档位只为 A/B。
///
/// * `live` —— 官方口径（`g_Time * g_Speed` 原样），即修前行为，护栏/A/B 用。
///
/// * `norm` —— 时间项按自身周期归一化（恒等，见上）。
/// 平铺图像层（非 puppet）默认仍是 `live`：那里没有骨骼动画，水波就是作者要的动效，
/// 没有"两套时间场打架"这回事。
///
/// # ：为什么 `hold` 修不掉"撕裂"（本档存在的原因）
/// `waterwaves.frag` 的位移是 `sin(距离)`，而
/// `距离 = g_Time * g_Speed + dot(texCoordMotion, v_Direction) * g_Scale`。
/// 把 `g_Time` 冻成 0 只删掉**第一项**，剩下一项是**空间相位**：同一条波在画面上
/// 仍然是一整幅 `sin(空间)` 的**静态**位移场（幅度 = `g_Strength²·mask`，与 live
/// 同幅）⇒ 发丝像素被永久采到偏 ~1–5 px 的位置 ⇒ 形状回不去。要"复位"必须让
/// **位移项**归零，而不是让时间项归零。
fn wave_nphase_mode(mesh_layer: bool) -> u8 {
    match std::env::var("WE_WAVE_NPHASE")
        .ok()
        .map(|v| v.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("live") | Some("on") => 0,
        Some("hold") | Some("frozen") => 1,
        Some("norm") | Some("normalize") => 2,
        Some("amp0") | Some("zero") | Some("displace0") => 3,
        Some("shape") | Some("pin") | Some("silhouette") => 4,
        _ => {
            if mesh_layer {
                4
            } else {
                0
            }
        }
    }
}

/// 水波族 shader 的**时间项/位移项**改写（判据 = shader 自己的那几行，不看特效名）。
///
/// # 判据（源码，`waterwaves.frag`）
/// ```glsl
/// float distance = g_Time * g_Speed + dot(texCoordMotion, v_Direction) * g_Scale;
/// ...
/// texCoord += val1 * s1 * offset * strength * mask;
/// gl_FragColor = texSample2D(g_Texture0, texCoord); // ← 唯一输出，纯位移
/// ```
/// 返回 `(新源码, 命中处数)`；命中 0 ⇒ 调用方原样用旧源码（其它 shader 零影响）。
///
/// * `mode == 1`（hold）：`g_Time` → `0.0`（只动 `float distance` 那两行）
///
/// * `mode == 2`（norm）：`g_Time` → `mod(g_Time, 6.28318530718 / max(g_Speed, 1e-4))`
/// （`distance2` 那行走 `g_Speed2`）
///
/// * `mode == 3`（amp0）：`texCoord += …` → `texCoord += vec2(0.0);`
/// ⇒ 输出 = 输入（水波在这一层上整趟等价于不存在）
///
/// * `mode == 4`（shape）：**位移只进颜色、不进覆盖**：
/// ```glsl
/// vec4 weDisp = texSample2D(g_Texture0, texCoord); // 位移后的内容（颜色/折射照旧流动）
/// vec4 weRest = texSample2D(g_Texture0, v_TexCoord.xy); // 原 uv（= 没接水波时的像素）
/// vec3 weRgb = mix(weRest.rgb, weDisp.rgb, weDisp.a); // 按位移采样的**覆盖**混合
/// gl_FragColor = vec4(min(weRgb, vec3(weRest.a)), weRest.a); // 夹到 rgb ≤ a ⇒ 轮廓外不泄漏
/// ```
/// ⇒ **alpha 逐像素等于原 uv 的 alpha** ⇒ 轮廓就是静止形状；`rgb` 来自位移采样
/// ⇒ 水波的颜色流动还在。`min(…)` 是**预乘不变式**（`rgb ≤ a`）的兜底，防止
/// 轮廓外出现"a=0 但有颜色"的加法泄漏。
/// ⚠ **别写成 over 复合**（`weDisp.rgb + weRest.rgb*(1-weDisp.a)`，本档第一版就是）：
/// 位移为 0 的像素（遮罩=0 的躯干）上 `weDisp == weRest`，over 会把**同一层的颜色
/// 算两遍**（半透明处 `rgb×(2−a)`）⇒ 躯干框实测 mean|Δ| 0.979 / 2.71% 像素变亮。
/// 判据正是"**位移=0 时必须恒等**"这条自检：`mix` 在 `weDisp==weRest` 时恒等，
/// over 不恒等。
/// 3/4 两档要求 shader **同时**有"位移累加行"和"位移后采样输出行"，缺一 ⇒ 命中 0
/// （例如后来某个版本把位移写成 `uv = …` 或输出写到别的变量 ⇒ 宁可不改，也不改错）。
fn wave_nphase_rewrite(fs: &str, mode: u8) -> (String, usize) {
    if mode == 0 {
        return (fs.to_string(), 0);
    }
    if mode >= 3 {
        return wave_amp_rewrite(fs, mode);
    }
    let mut out = String::with_capacity(fs.len() + 96);
    let mut hits = 0usize;
    for line in fs.split_inclusive('\n') {
        let is_dist = line.contains("float distance");
        if is_dist && line.contains("g_Time") {
            hits += line.matches("g_Time").count();
            let speed = if line.contains("distance2") {
                "g_Speed2"
            } else {
                "g_Speed"
            };
            let rep = match mode {
                1 => "0.0".to_string(),
                _ => format!("mod(g_Time, 6.28318530718 / max({speed}, 0.0001))"),
            };
            out.push_str(&line.replace("g_Time", &rep));
        } else {
            out.push_str(line);
        }
    }
    (out, hits)
}

/// `mode 3/4` 的实体：改**位移项**（`texCoord += …`）与**输出采样行**，不动时间项、
/// 不动空间项、不动遮罩采样。判据见 [`wave_nphase_rewrite`]。
/// 输出行的识别用 `texSample2D(g_Texture0, texCoord)`（位移后的坐标就是它唯一的
/// 特征）；`\r\n` 原样保留（本场景 `waterwaves.frag` 是 CRLF）。
fn wave_amp_rewrite(fs: &str, mode: u8) -> (String, usize) {
    let has_shift = fs
        .lines()
        .any(|l| l.contains("texCoord +=") && l.contains("mask"));
    let has_out = fs
        .lines()
        .any(|l| l.contains("gl_FragColor") && l.contains("texSample2D(g_Texture0, texCoord)"));
    if !has_shift || !has_out {
        return (fs.to_string(), 0);
    }
    let mut out = String::with_capacity(fs.len() + 320);
    let mut hits = 0usize;
    for line in fs.split_inclusive('\n') {
        // 行尾（`\r\n` / `\n`）原样保留
        let (body, eol) = if let Some(b) = line.strip_suffix("\r\n") {
            (b, "\r\n")
        } else if let Some(b) = line.strip_suffix('\n') {
            (b, "\n")
        } else {
            (line, "")
        };
        let indent: String = body.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        if mode == 3 && body.contains("texCoord +=") && body.contains("mask") {
            out.push_str(&format!("{indent}texCoord += vec2(0.0);{eol}"));
            hits += 1;
        } else if mode == 4
            && body.contains("gl_FragColor")
            && body.contains("texSample2D(g_Texture0, texCoord)")
        {
            out.push_str(&format!(
                "{indent}vec4 weDisp = texSample2D(g_Texture0, texCoord);{eol}\
                 {indent}vec4 weRest = texSample2D(g_Texture0, v_TexCoord.xy);{eol}\
                 {indent}vec3 weRgb = mix(weRest.rgb, weDisp.rgb, weDisp.a);{eol}\
                 {indent}gl_FragColor = vec4(min(weRgb, vec3(weRest.a)), weRest.a);{eol}"
            ));
            hits += 1;
        } else {
            out.push_str(body);
            out.push_str(eol);
        }
    }
    (out, hits)
}

/// `WE_FLOW_WARP`（默认关）：把注解里 `"mode"` 含 `flowmask` / `flow_mask` 的槽也
/// 当成「要按网格几何烘焙进帧空间」的槽。`effects/shake` 的位移场是作者画在**画布空间**
/// 的，直接拿帧空间 uv 采它 ⇒ 位移落到错的位置（ 实测：髋部被推）。
/// 只有 `1`/`on`/`true` 算开；不设或其它值 = 关 ⇒ 与加这个开关之前逐字节相同。
fn flow_warp_on() -> bool {
    let v = match std::env::var("WE_FLOW_WARP") {
        Ok(v) => v.trim().to_ascii_lowercase(),
        Err(_) => return false,
    };
    if v == "1" {
        return true;
    }
    if v == "on" {
        return true;
    }
    v == "true"
}

/// 把**遮罩**按网格几何画进「帧空间」用的两个极简着色器（与底图趟共用同一份
/// 顶点/uv/索引和同一个 MVP）⇒ 输出缓冲与内容缓冲**逐像素同空间**。
/// 材质 shader 不参与：这里只搬运遮罩的 `rgba`（效果只读 `.r`）。
const MASK_WARP_VS: &str = r#"
attribute vec3 a_Position;
attribute vec2 a_TexCoord;
uniform mat4 g_ModelViewProjectionMatrix;
varying vec2 v_TexCoord;
void main() {
    gl_Position = mul(vec4(a_Position, 1.0), g_ModelViewProjectionMatrix);
    v_TexCoord = a_TexCoord;
}
"#;
/// `WE_PERF_MASK_NOCLEAR`（默认**开**；`=0` 逐字节回旧路径）：遮罩烘焙趟改成「直接覆盖」写法
/// （输出预乘，混合关掉），于是**不需要每趟把整张烘焙缓冲清成 0**。
/// 等价性：混合开着、目标刚清成透明黑时，一次绘制的结果恒为
/// `(src.rgb·src.a, src.a·src.a)` —— 对 `translucent`/`additive` 都一样（因为它们对
/// 颜色的第一项都是 `SRC_ALPHA`、对 alpha 的第一项也都是 `SRC_ALPHA`，而 `dst = 0`）。
/// 所以把这一项**烘进片元**、再把混合关掉，逐像素与原路径相同；而且因为不再与上一趟
/// 的残留混合，"每趟清屏"这件事本身也就不需要了。
/// 前提：烘焙趟的网格铺满目标 RT（puppet 网格本来就是整张纹理的网格）；网格盖不到的
/// 边缘像素保持建 RT 时的 0，下游在那里没有内容 ⇒ 不可见。
fn mask_noclear_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| env_flag("WE_PERF_MASK_NOCLEAR", true))
}

/// `WE_MASK_FULLSCREEN`（默认**开**；`=0/false/off/no` 回退旧口径）：把遮罩烘焙改成 win 的口径 —— 用一张
/// **全屏尺寸**的 RT（对应 `_rt_FullAlphaMask`）跨趟复用、每趟烘焙都写它，而不是
/// 每趟一张「链缓冲尺寸」的 RT（本场景链 2539×3570，9 趟 = 81.6 MP/帧的填充）。
///
/// # 为什么换了尺寸却不用改任何采样坐标
/// 烘焙趟的顶点着色器只做 `gl_Position = mvp·pos; v_TexCoord = uv` ⇒ 输出内容是
/// **归一化 uv 空间**里的一张图；目标 RT 的像素尺寸 / 长宽比只决定采样网格的密度，
/// 不改变内容在 uv 里的位置。下游特效趟照旧拿 `v_TexCoord.zw` 采它 ⇒ 坐标一个字都不动。
///
/// # 为什么必须把 `g_Texture{N}Resolution` 钉在旧值
/// 特效 shader 的像素级算子（水波步长、blur 步长、`g_BlendOffset`）都以
/// `g_Texture{N}Resolution` 为单位。而 `WE_TEXRES_PASS` 默认开 ⇒ RT 槽上报**真实像素
/// 尺寸**。烘焙 RT 换尺寸后若照报新尺寸，这些算子的尺度会跟着变（画面会漂）。
/// 所以本模式在 `rt_report` 里显式写回**旧口径**（见 build 里 `rep` 那段）。
///
/// # 代价（这个模式下不许省的东西）
/// 全屏 RT 不再被网格铺满 ⇒ 每趟烘焙前**必须清屏**，不能走
/// `WE_PERF_MASK_NOCLEAR`（那套等价性依赖"目标被整块覆盖"）。见 draw_frame 的清屏分支。
/// 默认值在 06:33 由 会话翻成开（85fde55），随后按 win 口径装到桌面
/// 视觉代价，
/// 但 8×8 块差 12.2-17.2，历史判据 ≤4 不通过）。
fn mask_fullscreen_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| env_flag("WE_MASK_FULLSCREEN", true))
}

/// `WE_CHAIN_LAYER_SIZE`（默认**关**）：链缓冲尺寸回到**图层声明尺寸**
/// （win 的 `vtable+0x128` 口径：`_rt_imageLayerComposite_<id>` = 图层 `size`，下限 4），
/// 去掉「对象矩形 ∪ 网格包围盒」的外扩（本场景 `fit` = 1.19×宽 / 1.02×高 ⇒ 面积 1.21×）。
///
/// # 为什么只改像素密度、不改画面几何
/// 底图趟的 `mvp` 把「归一化对象坐标」映到 NDC `±1`，RT 的视口就是整张 RT ⇒
/// 目标 RT 的像素尺寸只决定**采样密度**，内容在归一化 uv 里逐点不变；收尾合成趟照旧
/// 用同一套 uv 把 RT 摆回场景 ⇒ 摆位、朝向、遮罩对齐都不动（和 `WE_MASK_FULLSCREEN` 同理）。
/// 代价是超采样倍率下降 ⇒ 用像素差验收，不靠推理。
fn chain_layer_size_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| env_flag("WE_CHAIN_LAYER_SIZE", false))
}

/// `WE_PERF_MASK_NOCLEAR` 用的片元：把「半透明混合到透明黑」的结果直接算出来。
/// 与 [`MASK_WARP_FS`] 只差最后一行的预乘。
const MASK_WARP_FS_PREMULT: &str = r#"
varying vec2 v_TexCoord;
uniform sampler2D g_Texture0;
void main() {
#ifdef WE_WARP_UVDBG
    gl_FragColor = vec4(v_TexCoord, 0.0, 1.0);
#else
    vec4 c = texSample2D(g_Texture0, v_TexCoord);
    gl_FragColor = vec4(c.rgb * c.a, c.a * c.a);
#endif
}
"#;

const MASK_WARP_FS: &str = r#"
varying vec2 v_TexCoord;
uniform sampler2D g_Texture0;
void main() {
#ifdef WE_WARP_UVDBG
    gl_FragColor = vec4(v_TexCoord, 0.0, 1.0);
#else
    gl_FragColor = texSample2D(g_Texture0, v_TexCoord);
#endif
}
"#;

/// `WE_MASK_ATLAS_V` —— 仿射的 V 方向（`down` 默认 = V 反向四边形；`up` = 非反向）。
fn mask_atlas_v_down() -> bool {
    !matches!(
        std::env::var("WE_MASK_ATLAS_V").ok().as_deref(),
        Some("up") | Some("0") | Some("off")
    )
}

/// 把**遮罩槽**的采样坐标 `v_TexCoord.zw` 仿射到图集空间（见 [`mask_atlas_space_on`]）。
/// 只改同时满足「行里出现该槽的 `g_Texture{N}`」与「行里有 `v_TexCoord.zw`」的行，
/// 与 `mask_axis_rewrite` 同一口径 ⇒ 内容采样（`v_TexCoord.xy`）与网格顶点不受影响。
fn mask_uv_affine_rewrite(
    fs: &str,
    mask_units: &[u32],
    (u0, du, v0, dv): (f32, f32, f32, f32),
) -> (String, usize) {
    if mask_units.is_empty() {
        return (fs.to_string(), 0);
    }
    let repl = format!(
        "vec2({u0:.8} + {du:.8} * v_TexCoord.z, {v0:.8} + {dv:.8} * v_TexCoord.w)"
    );
    let mut out = String::with_capacity(fs.len() + 64);
    let mut hits = 0usize;
    for line in fs.split_inclusive('\n') {
        let is_mask_line = mask_units
            .iter()
            .any(|u| line.contains(&format!("g_Texture{u}")));
        if is_mask_line && line.contains("v_TexCoord.zw") {
            hits += line.matches("v_TexCoord.zw").count();
            out.push_str(&line.replace("v_TexCoord.zw", &repl));
        } else {
            out.push_str(line);
        }
    }
    (out, hits)
}

/// `WE_MASK_VFLIP`：**默认关**（= 与改前逐字节一致；两处硬护栏 md5 与 17 条横切全绿）。
/// 为什么默认关而不是默认开：翻正之后**滑板整块**的 0.2s 高频晃动反而 6.71 → 8.31
/// （掩码是软团，翻正只是把能量从"尖端"搬到"板身"），所以它不是可默认上屏的修法 ——
/// 只作为**定位确凿的待决修法**留在这里（`=1` 开，尖端效果量 10.98 → 1.10）。
fn mask_vflip_on() -> bool {
    match std::env::var("WE_MASK_VFLIP").ok().as_deref() {
        None => false,
        Some(v) => !matches!(v, "" | "0" | "off" | "false"),
    }
}

/// `WE_LAYER_UPRIGHT`（默认**关**，A/B 用）：图层缓冲按**画布朝向**存
/// （RT 第 0 行 = 画布的顶行），而不是把"取到的贴图"上下镜像。
/// 为什么：GL 的 RT 第 0 行在 NDC 的**下**边，而我们的 `v_TexCoord` 约定 v=0 = 画面顶。
/// 现在 `WE_LAYER_VFLIP` 的做法是给底图趟换**V 反向的 uv**：贴图取对了，但缓冲里存下去的是
/// **镜像的画**，于是链上每一趟的 `v_TexCoord` 都是"镜像的画布 uv"——
/// 采样 RT 正确，采样**普通遮罩贴图**（作者按画布 uv 画的）就镜像了。
/// 实测（`3704273480` id=1016「身体---拆分」，滑板右尖 uv=(0.949,0.224)）：
/// 着色器读到的 `waterwaves_mask_480a6962` = **240/255**，
/// 而该 uv 的遮罩原像素 = **0**、`1−v` 处 = **251** ⇒ 镜像确凿（`WE_FX_PROBE=mask/maskuv` 实测）。
/// 本开关改为**翻几何**（把底图趟 MVP 的 y 行取负）⇒ 缓冲里存的是**正着**的画 ⇒
/// 内容、遮罩、收尾趟的 `v_TexCoord` 全部同一套画布 uv，一个遮罩补偿都不需要。
/// 收尾趟的朝向随之改回"不翻"（`WE_LAYER_UPRIGHT_BLIT` 可 A/B 钉住）。
fn layer_upright_on() -> bool {
    matches!(
        std::env::var("WE_LAYER_UPRIGHT").ok().as_deref(),
        Some("1") | Some("on") | Some("true")
    )
}

/// `WE_MASK_UFLIP`：把**指定遮罩**的采样 u 取反（逗号分隔的名字片段；空/未设 ⇒ 不动）。
/// 与 `WE_MASK_VFLIP` 独立：同一张遮罩同时出现在两个开关里 ⇒ u 与 v 都翻。
fn mask_uflip_filter() -> Option<Vec<String>> {
    let v = std::env::var("WE_MASK_UFLIP").ok()?;
    if matches!(v.as_str(), "" | "0" | "off" | "false") {
        return None;
    }
    if matches!(v.as_str(), "1" | "on" | "true") {
        // 全翻（A/B 用）：用不可能匹配的名字片段逐张放开，这里直接返回通配。
        return Some(vec![String::new()]);
    }
    Some(
        v.split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

/// `WE_MASK_VFLIP` 的**逐遮罩**选择：返回 `Some(片段列表)` ⇒ 只翻名字含这些片段的遮罩槽；
/// `None` ⇒ 全部（`1/on/true`）或全不翻（未设 / `0`）。
fn mask_vflip_filter() -> Option<Vec<String>> {
    let v = std::env::var("WE_MASK_VFLIP").ok()?;
    if matches!(v.as_str(), "" | "0" | "off" | "false" | "1" | "on" | "true") {
        return None;
    }
    Some(
        v.split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

/// -1：把**材质/特效 JSON 里的 `passes[].combos`** 并进某一趟的 combo 表。
/// 官方语义：一个材质趟的编译期开关有两个来源 —— shader 里的 `// [COMBO]` 注解默认值，
/// 以及材质/特效 JSON 里**显式写的那一趟**的 `combos`。后者覆盖前者。
/// 本函数的调用点决定优先级（在 `[COMBO]` 默认值之后、场景级 `passes[].combos` 之前）。
///
/// * 键按 `shader::canonical_combo_name` **对齐到 shader 自己的拼写**（大小写不敏感）——
/// 官方 `"VERSION"`/`"version"` 两种写法都有，而 GLSL 宏名大小写敏感；
///
/// * 值按 `shader::combo_json_i32` 折算（`true`⇒1）；认不出的值**跳过**，不塞 0；
///
/// * 已存在的同名项**就地改写**（保持顺序），不存在才 push —— 与 `WE_FX_COMBOS` 同一口径。
fn merge_pass_combos(
    combos: &mut Vec<(String, i32)>,
    kv: &std::collections::BTreeMap<String, serde_json::Value>,
    srcs: &[&str],
) -> usize {
    let mut n = 0;
    for (k, v) in kv {
        let Some(iv) = shader::combo_json_i32(v) else { continue };
        let key = srcs
            .iter()
            .find_map(|s| shader::canonical_combo_name(s, k))
            .unwrap_or_else(|| k.clone());
        match combos.iter_mut().find(|(ex, _)| ex.eq_ignore_ascii_case(&key)) {
            Some(slot) => slot.1 = iv,
            None => combos.push((key, iv)),
        }
        n += 1;
    }
    n
}

/// -1 的开关（**默认关**：不设或 ≠ "1" ⇒ 一个字节都不动）。
/// 打开后：材质趟 / 特效趟 / 粒子材质 / shape 材质的 `passes[].combos` 会真的并进
/// `#define` 列表（官方 `VERSION=2` / `VERTICAL=1` / `REFRACT=1` / `LIGHTING=1` …）。
fn mat_combos_on() -> bool {
    crate::flag_default_on("WE_MAT_COMBOS")
}

/// 生成一朵软光斑贴图：白色 + 径向 alpha 衰减（`(1-d)^falloff`，中心 1、边缘 0）。
fn glow_texture_rgba(n: u32, falloff: f32) -> Vec<u8> {
    let n = n.max(4);
    let mut v = vec![0u8; (n as usize) * (n as usize) * 4];
    let c = (n as f32 - 1.0) * 0.5;
    for y in 0..n {
        for x in 0..n {
            let dx = (x as f32 - c) / c;
            let dy = (y as f32 - c) / c;
            let d = (dx * dx + dy * dy).sqrt();
            let a = (1.0 - d).clamp(0.0, 1.0).powf(falloff.max(0.05));
            let o = ((y as usize) * (n as usize) + x as usize) * 4;
            v[o] = 255;
            v[o + 1] = 255;
            v[o + 2] = 255;
            v[o + 3] = (a * 255.0).round() as u8;
        }
    }
    v
}

/// 一个粒子算不算"发光光源"，以及它的光斑颜色。
/// 判据与度量窗内的暖色口径同向：`R` 明显高于 `B` 且整体够亮；颜色偏暖化
/// （R 保留、G 略降、B 大降）⇒ 得到橙色柔光，而不是把粒子的白心原样糊大。
fn glow_tint_for(rgb: [f32; 3]) -> Option<[f32; 3]> {
    let [r, g, b] = rgb;
    if r > b + 0.06 && r > 0.20 {
        Some([r.min(1.0), (g * 0.80).min(1.0), (b * 0.45).min(1.0)])
    } else {
        None
    }
}

/// 一个粒子对象：程序 + 系统状态 + 每帧顶点
struct ParticleInstance {
    batch: gl_ctx::GlBatch,
    sys: particles::ParticleSystem,
    verts: Vec<[f32; gl_ctx::PARTICLE_FLOATS]>,
    additive: bool,
    /// 对象名（只用于调试/报告：`WE_DEBUG_PARTICLES*` 打日志时能直接对上号）
    name: String,
    /// **这一路粒子该插在趟序列的哪个位置**（= 对象列表里排在它前面的对象
    /// 一共产生了多少趟）。WE 里粒子是**普通场景对象**，按 `objects` 次序渲进场景 FBO
    /// （linux-wallpaperengine `CScene.cpp:345-360`：所有对象 → 场景 FBO、按
    /// `m_objectsByRenderOrder` 顺序 `render`）⇒ 排在它后面的图层**会挡住/叠加它**。
    /// 我们以前把所有粒子排在全部图层趟之后 ⇒ 雨/雾盖在"前景房间层"之上 ⇒ 用户报的
    /// 「室内不应该有雨」（实测 `2080534540`：前景 alpha>0.9 的室内像素里仍有 6.16% 被雨改变）。
    /// `usize::MAX` = 没有位置信息（退回旧行为：画在最后）。
    insert_before: usize,
    /// `instanceoverride.rate` 的**场景属性脚本**（`{"script": "<JS>", "value": …}`；
    /// 见 [`crate::propscript`]）。默认（`WE_SCRIPT_AUDIO` 未设）恒为 `None` ⇒ 发射率仍取静态
    /// `ov.rate` ⇒ 与改动前逐字节相同。打开后每帧调一次 `update` 拿新的发射率乘子。
    rate_script: Option<propscript::NumScript>,
}

/// 一盏**场景灯**（`objects[]` 里带 `"light"` 的那个对象）—— 渲染侧需要的全部输入。
/// 官方语义（双出处，逐条见各字段注释）：
///
/// * **官方资产**：`assets/scenes/modeleditor/scene.json:46/48/49/52/54`（灯对象的字段与取值形态）
/// + `:35-37`（`general.lightconfig`）；
///
/// * **官方 shader 原文**：`assets/shaders/generic.vert:7-9`、`generic.frag:4`、
/// `genericimage4.frag:49/90/139-140`、`genericimage2.vert:57`、`genericimage2.frag:31-32`；
///
/// * **参照实现 OWE**（消费侧口径）：`Scene/Scene/Scene.cpp:969-995`（曲线求值）、
/// `Scene/Pkg/Parse/Scene/SceneObjectParsers.cpp:336-370`（灯类型映射）、
/// `Scene/Pkg/Parse/Uniform/UniformSource.cpp:848-925`（`LightUniformSource::Evaluate` = 打包顺序）、
/// `Scene/Lighting/Lighting.cppm:18-55`（类型枚举 0/1/2 与 `color=color*intensity`）。
#[derive(Debug, Clone)]
struct LightSrc {
    kind: LightKind,
    /// `"color"`（`"r g b"`；`{"user":…,"value":"r g b"}` 取 `value`）。
    color: [f32; 3],
    /// `"intensity"`（缺省 1.0）
    intensity: f32,
    /// `"radius"`（缺省 1000.0）
    radius: f32,
    /// 静态 `"origin"`（场景单位）
    origin: [f32; 3],
    /// `origin` 的官方动画曲线（有 ⇒ 按本帧场景时间求值）
    origin_anim: Option<we_scene::scene::anim::Curve>,
    /// `radius` 的官方动画曲线
    radius_anim: Option<we_scene::scene::anim::Curve>,
    /// `"castshadow"`
    cast_shadow: bool,
    /// 内外锥角的**余弦**（OWE `SceneObjectParsers.cpp:355-357`：`cos(角度)`；
    /// 字段缺省 0° ⇒ `cos(0)=1` = 无衰减）
    inner_cone_cos: f32,
    outer_cone_cos: f32,
    /// `"exponent"`（进 `g_LightsConeExponent[i].z`）
    exponent: f32,
    /// `"angles"`（弧度）—— 用来算 `g_LightsDirectionType[i].xyz`
    angles: [f32; 3],
}

/// 灯类型。取值 = `g_LightsDirectionType[i].w` 的**官方枚举**
/// （OWE `Scene/Lighting/Lighting.cppm:18-23`：`Point=0, Spot=1, Directional=2`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LightKind {
    Point,
    Spot,
    Directional,
}

impl LightKind {
    /// 字符串 → 类型。映射逐字照 OWE `SceneObjectParsers.cpp:345-350`
    /// （`spot`/`lspot` ⇒ Spot；`directional`/`ldirectional` ⇒ Directional；**其余（含 `point`/`lpoint`/
    /// 空串）⇒ Point**）。
    fn of(s: &str) -> LightKind {
        match s.trim().to_ascii_lowercase().as_str() {
            "spot" | "lspot" => LightKind::Spot,
            "directional" | "ldirectional" => LightKind::Directional,
            _ => LightKind::Point,
        }
    }
    fn value(self) -> f32 {
        match self {
            LightKind::Point => 0.0,
            LightKind::Spot => 1.0,
            LightKind::Directional => 2.0,
        }
    }
    /// 灯的**本地朝向**（OWE `UniformSource.cpp:886-887`：只有 Spot 取 `+X`，其余取 `-X`）。
    fn local_direction(self) -> [f32; 3] {
        if self == LightKind::Spot {
            [1.0, 0.0, 0.0]
        } else {
            [-1.0, 0.0, 0.0]
        }
    }
}

/// `angles`（弧度，作用顺序 z→y→x，与场景对象一致）作用在向量 `v` 上并归一化。
/// = OWE `Scene/Pkg/Parse/Uniform/UniformSource.cpp:888-890` 的
/// `(node.ModelTrans.block<3,3>(0,0) * local_direction).normalized`。
fn rot3(angles: [f32; 3], v: [f32; 3]) -> [f32; 3] {
    let m = Mat4::rotate_z(angles[2])
        .mul(&Mat4::rotate_y(angles[1]))
        .mul(&Mat4::rotate_x(angles[0]));
    let mut o = [0.0f32; 3];
    for (r, item) in o.iter_mut().enumerate() {
        *item = (0..3).map(|c| m.m[c * 4 + r] * v[c]).sum();
    }
    let n = (o[0] * o[0] + o[1] * o[1] + o[2] * o[2]).sqrt();
    if n > 1e-9 {
        [o[0] / n, o[1] / n, o[2] / n]
    } else {
        o
    }
}

/// 从场景里收集灯对象。
/// **数据驱动**，不看场景 id / 对象名：只要对象的 `light` 字段非空就是灯
/// （`we_scene::scene::ObjectKind::Light`，官方 `Object.cpp:89-90` 的 `DecodeObject<LightObject>`）。
/// `raw_scene`（`scene.json` 的原始 JSON）只为取 `origin`/`radius` 的 **`animation` 曲线** ——
/// `we-scene` 的 `Object::origin`/`radius` 只保留静态端（`bound::string` / `bound::opt_f32`
/// 都取 `value`），动画曲线不在类型化结构里。这里按 **对象 id** 对上（不是按数组下标，
/// 免得解析器跳过某个对象后错位）。
fn lights_from_scene(
    scene: &we_scene::scene::SceneDoc,
    raw_scene: Option<&serde_json::Value>,
) -> Vec<LightSrc> {
    let raw_by_id = |id: i64| -> Option<&serde_json::Value> {
        raw_scene?
            .get("objects")?
            .as_array()?
            .iter()
            .find(|o| o.get("id").and_then(|v| v.as_i64()) == Some(id))
    };
    let mut out = Vec::new();
    for o in scene.objects.iter() {
        let Some(light) = o.light.as_deref() else { continue };
        if !o.is_visible() {
            continue;
        }
        let (origin_anim, radius_anim) = match raw_by_id(o.id) {
            Some(r) => (
                r.get("origin").and_then(we_scene::scene::anim::Curve::from_field),
                r.get("radius").and_then(we_scene::scene::anim::Curve::from_field),
            ),
            None => (None, None),
        };
        let deg = std::f32::consts::PI / 180.0;
        out.push(LightSrc {
            kind: LightKind::of(light),
            color: o.color_rgb(),
            intensity: o.intensity.unwrap_or(1.0),
            radius: o.radius.unwrap_or(1000.0),
            origin: we_scene::scene::parse_vec3(&o.origin).unwrap_or([0.0, 0.0, 0.0]),
            origin_anim,
            radius_anim,
            cast_shadow: o.cast_shadow.unwrap_or(false),
            inner_cone_cos: (o.inner_cone.unwrap_or(0.0) * deg).cos(),
            outer_cone_cos: (o.outer_cone.unwrap_or(0.0) * deg).cos(),
            exponent: o.exponent.unwrap_or(1.0),
            angles: we_scene::scene::parse_vec3(&o.angles).unwrap_or([0.0, 0.0, 0.0]),
        });
    }
    out
}

/// Outcome of one frame in bounded-commit mode .
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameCommit {
    /// Drawn, and eglSwapBuffers returned within the deadline.
    Presented,
    /// Drawn, but the submit is still in flight: the caller must not draw again yet.
    Pending,
}

/// `WE_PERF_STATIC_CACHE=1` (default OFF): reuse chain RTs of *static* layers across frames.
/// A pass may be skipped on frame N>0 only if
///
/// 1. its shader reads no per-frame uniform ([`gl_ctx::dynamic_uniform`]);
///
/// 2. it is not a mesh-anim / pointer-projection / parallax / mask-bake / attach /
/// ApplyBlending-copy / script-scalar pass;
///
/// 3. every writer of its target RT satisfies 1+2, and every RT those writers read is
/// itself frozen (fixpoint).
/// Then that RT is neither cleared nor re-drawn after its first frame; the pass that
/// composites the chain into the parent still runs every frame, so the composite path is
/// untouched - only the repeated shading of the chain disappears.
/// Acceptance is byte-identical output (17 scenes, cache on vs off). There is also a
/// runtime guard: the (mvp, unit table) signature of every frozen RT is compared each
/// frame; if it moves, the whole cache is dropped and every pass is drawn again.
fn perf_static_cache_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| env_flag("WE_PERF_STATIC_CACHE", false))
}

/// Bookkeeping for `WE_PERF_STATIC_CACHE`.
struct StaticCache {
    /// per RT: content depends only on static inputs => draw once
    frozen: Vec<bool>,
    /// per RT: indices of the passes that write it (signature + stats)
    writers: Vec<Vec<usize>>,
    /// per pass: target RT is frozen => may be skipped this frame
    skip: Vec<bool>,
    /// per RT: signature recorded when it was last drawn (0 = never drawn)
    sig: Vec<u64>,
    /// cleared once the runtime guard sees a moving signature (never re-armed)
    live: bool,
    /// total passes skipped so far (reported at exit)
    skipped: u64,
}

impl StaticCache {
    /// FNV-1a over every writer (mvp, unit table). Any parent transform / texture change
    /// moves it, which is exactly what makes a frozen RT stale.
    fn rt_sig(&self, passes: &[Pass], t: usize) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &i in self.writers[t].iter() {
            let p = &passes[i];
            for w in p.mvp.iter() {
                h ^= w.to_bits() as u64;
                h = h.wrapping_mul(0x100_0000_01b3);
            }
            h ^= p.units.len() as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
            for (u, s) in p.units.iter() {
                let tag = match s {
                    Src::Rt(k) => 0x1000_0000 + *k as u64,
                    Src::Tex(k) => 0x2000_0000 + *k as u64,
                    Src::White => 0x3000_0000,
                };
                h ^= (*u as u64) << 32 | tag;
                h = h.wrapping_mul(0x100_0000_01b3);
            }
        }
        h
    }
}

impl SceneRenderer {
    /// See [`perf_static_cache_on`]. Returns None when the switch is off (zero cost).
    fn plan_static_cache(&self) -> Option<StaticCache> {
        if !perf_static_cache_on() {
            return None;
        }
        // g_Lights is only per-frame when a light source itself is animated.
        let lights_anim = self
            .light_srcs
            .iter()
            .any(|l| l.origin_anim.is_some() || l.radius_anim.is_some());
        let npass = self.passes.len();
        let nrt = self.rts.len();
        let mut own = vec![false; npass];
        for i in 0..npass {
            let p = &self.passes[i];
            own[i] = p.target.is_some()
                && p.layer.dyn_token.is_empty()
                && !(lights_anim && p.layer.reads_lights)
                && p.mesh_anim.is_none()
                && !self.pointer_proj.contains(&i)
                && !self.parallax_passes.contains_key(&i)
                && !self.mask_warp_passes.contains(&i)
                && !self.cbm_passes.contains_key(&i)
                && !self.fx_scripts.iter().any(|(pi, _)| *pi == i)
                && !self.attach_passes.iter().any(|a| a.pass_idx == i)
                && !p.units.iter().any(|(_, s)| match s {
                    Src::Rt(k) => Some(*k) == self.cbm_copy_rt,
                    _ => false,
                });
        }
        let mut writers: Vec<Vec<usize>> = vec![Vec::new(); nrt];
        for (i, p) in self.passes.iter().enumerate() {
            if let Some(t) = p.target {
                if t < nrt {
                    writers[t].push(i);
                }
            }
        }
        // Never freeze the scene composite RT (particles write into it) nor anything a
        // screen-targeted (present) pass reads.
        let mut banned = vec![false; nrt];
        if let Some(sr) = self.scene_rt {
            if sr < nrt {
                banned[sr] = true;
            }
        }
        for p in self.passes.iter() {
            if p.target.is_none() {
                for (_, s) in p.units.iter() {
                    if let Src::Rt(t) = s {
                        if *t < nrt {
                            banned[*t] = true;
                        }
                    }
                }
            }
        }
        if std::env::var_os("WE_STATIC_CACHE_DEBUG").is_some() {
            let mut names: Vec<String> = vec![String::new(); nrt];
            for (k, v) in self.rt_index.iter() {
                if *v < nrt {
                    names[*v] = k.clone();
                }
            }
            for i in 0..npass {
                let p = &self.passes[i];
                let tgt = match p.target {
                    Some(t) => format!("RT[{}]:{}", t, names.get(t).cloned().unwrap_or_default()),
                    None => "screen".to_string(),
                };
                let why = if !own[i] {
                    if !p.layer.dyn_token.is_empty() {
                        format!("shader reads {}", p.layer.dyn_token)
                    } else if p.mesh_anim.is_some() {
                        "mesh anim".to_string()
                    } else if self.pointer_proj.contains(&i) {
                        "pointer projection".to_string()
                    } else if self.parallax_passes.contains_key(&i) {
                        "parallax".to_string()
                    } else if self.mask_warp_passes.contains(&i) {
                        "mask bake".to_string()
                    } else if self.cbm_passes.contains_key(&i) {
                        "applyblending copy".to_string()
                    } else if self.fx_scripts.iter().any(|(pi, _)| *pi == i) {
                        "scripted scalar".to_string()
                    } else if self.attach_passes.iter().any(|a| a.pass_idx == i) {
                        "attachment".to_string()
                    } else {
                        "screen target".to_string()
                    }
                } else {
                    "static".to_string()
                };
                eprintln!("[static-cache] pass {i} -> {tgt}: {why}");
            }
        }


        let mut frozen = vec![false; nrt];
        loop {
            let mut changed = false;
            for t in 0..nrt {
                if frozen[t] || banned[t] || writers[t].is_empty() {
                    continue;
                }
                let ok = writers[t].iter().all(|&i| {
                    own[i]
                        && self.passes[i].units.iter().all(|(_, s)| match s {
                            Src::Rt(u) => frozen.get(*u).copied().unwrap_or(false),
                            _ => true,
                        })
                });
                if ok {
                    frozen[t] = true;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        if std::env::var_os("WE_STATIC_CACHE_DEBUG").is_some() {
            let mut names: Vec<String> = vec![String::new(); nrt];
            for (k, v) in self.rt_index.iter() {
                if *v < nrt {
                    names[*v] = k.clone();
                }
            }
            let list: Vec<String> = (0..nrt)
                .filter(|t| frozen[*t])
                .map(|t| format!("RT[{}]:{}", t, names[t]))
                .collect();
            eprintln!("[static-cache] frozen: {:?}", list);
        }
        let mut skip = vec![false; npass];
        for i in 0..npass {
            if !own[i] {
                continue;
            }
            if let Some(t) = self.passes[i].target {
                skip[i] = frozen.get(t).copied().unwrap_or(false);
            }
        }
        let n_frozen = frozen.iter().filter(|f| **f).count();
        let n_skip = skip.iter().filter(|s| **s).count();
        eprintln!(
            "[static-cache] WE_PERF_STATIC_CACHE: {} RTs frozen / {} of {} passes skipped per frame ({} RTs total)",
            n_frozen, n_skip, npass, nrt
        );
        if n_skip == 0 {
            return None;
        }
        Some(StaticCache {
            frozen,
            writers,
            skip,
            sig: vec![0; nrt],
            live: true,
            skipped: 0,
        })
    }
}


/// 文字层换色配方（`WE_SETPROP_FAST=1` 才有内容；见 [`SceneRenderer::refresh_user_prop`]）。
struct TextRec {
    /// `self.texs` 里的下标（那张光栅化贴图）
    tex: usize,
    w: u32,
    h: u32,
    /// 只留覆盖率（每像素 1 字节）：`text::rasterize` 写的是直通 alpha
    alpha: Vec<u8>,
    /// 这层带了白色底板（`WE_TEXT_PLATE=1`）⇒ 贴图里还有非字色的像素，
    /// 就不能靠"把 a>0 的像素全刷成新色"来换色（会连底板一起刷掉）⇒ 退回重建。
    plate: bool,
    /// 这张贴图**当前**的字色（量化到 8 位，与 [`recolor_from_alpha`] 同一口径）。
    /// 用途：拖色盘时同一档颜色会被反复下发（UI 每帧一条），值没变就**别再传一遍**
    /// 9.7 MB —— 实测这是拖动中约一半的无用上传。
    cur_rgb: [u8; 3],
    /// 等着在**下一帧开始**落地的字色（`Some` = 有待应用的新色）。
    /// 为什么不能当场传：EGL 的 current context 是**按线程**的，而内核把 swap 交给
    /// `we-commit` 线程后会在两帧之间 `eglMakeCurrent(None)` 放掉主线程的 context
    /// （见 `gl_ctx` 顶部那段）。控制面命令是在主循环里处理的，那时**通常没有 current
    /// context** ⇒ 当场 `glTexImage2D` 会被静默丢掉（实测：回复说"局部刷新"，屏幕一点没变）。
    /// 所以这里只记值，真正的上传放到 `draw_frame` 开头（那里 context 一定 current）。
    pending: Option<[f32; 3]>,
}

/// `set-prop` 的**局部刷新**开关。默认关（关时零行为差异、零内存），
/// 打开后 `set-prop` 只改纯颜色类的用户属性就不再重建整个场景。
pub fn setprop_fast_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("WE_SETPROP_FAST")
            .map(|v| v.trim() == "1")
            .unwrap_or(false)
    })
}

/// 局部刷新是否只重传"有字形的那一块"（`WE_SETPROP_RECT=0` ⇒ 整张重传，A/B 用）。
fn setprop_rect_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("WE_SETPROP_RECT").ok().as_deref() != Some("0"))
}

/// 把一张**只有覆盖率**的文字画布按新字色铺成 RGBA。
/// 口径必须与 `text::rasterize` 一致：`rgb = (c.clamp(0,1)*255).round`、`a = 覆盖率`、
/// `a == 0` 的像素全 0（那里 `rasterize` 一个字节都不写，画布本来就是 0）。
/// ⇒ 同一个覆盖掩膜 + 同一个色 ⇒ **逐字节相同**（这条由单测 `recolor_matches_rasterize`
/// 用真光栅化结果钉住）。
pub fn recolor_from_alpha(alpha: &[u8], rgb: [f32; 3]) -> Vec<u8> {
    let c = [
        (rgb[0].clamp(0.0, 1.0) * 255.0).round() as u8,
        (rgb[1].clamp(0.0, 1.0) * 255.0).round() as u8,
        (rgb[2].clamp(0.0, 1.0) * 255.0).round() as u8,
    ];
    let mut out = vec![0u8; alpha.len() * 4];
    for (i, a) in alpha.iter().enumerate() {
        if *a > 0 {
            out[i * 4] = c[0];
            out[i * 4 + 1] = c[1];
            out[i * 4 + 2] = c[2];
            out[i * 4 + 3] = *a;
        }
    }
    out
}

/// 覆盖率掩膜里"有字"的包围盒 `(x0, y0, sw, sh)`；`None` = 整张全透明。
/// 为什么需要它：换字色时**只有 bbox 内的像素会变**（bbox 外 `a == 0`，RGB 在两条路上
/// 都是不可见），所以可以只重传这一块 —— 本机 691 的画布 1804x1344、bbox 只占 **43.7%**。
pub fn alpha_bbox(alpha: &[u8], w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
    let (w, h) = (w as usize, h as usize);
    if alpha.len() < w * h || w == 0 || h == 0 {
        return None;
    }
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
    for y in 0..h {
        let row = &alpha[y * w..(y + 1) * w];
        if let Some(f) = row.iter().position(|a| *a > 0) {
            let l = row.iter().rposition(|a| *a > 0).unwrap_or(f);
            if f < x0 {
                x0 = f;
            }
            if l + 1 > x1 {
                x1 = l + 1;
            }
            if y < y0 {
                y0 = y;
            }
            y1 = y + 1;
        }
    }
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some((x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32))
}

/// 只铺子矩形（口径与 [`recolor_from_alpha`] **逐字节同源**：`a>0` ⇒ `rgb = 新色, a = 覆盖率`）。
pub fn recolor_rect(
    alpha: &[u8],
    w: u32,
    _h: u32,
    (x0, y0, sw, sh): (u32, u32, u32, u32),
    rgb: [f32; 3],
) -> Vec<u8> {
    let c = [
        (rgb[0].clamp(0.0, 1.0) * 255.0).round() as u8,
        (rgb[1].clamp(0.0, 1.0) * 255.0).round() as u8,
        (rgb[2].clamp(0.0, 1.0) * 255.0).round() as u8,
    ];
    let w = w as usize;
    let mut out = vec![0u8; (sw as usize) * (sh as usize) * 4];
    for y in 0..sh as usize {
        for x in 0..sw as usize {
            let a = alpha[(y0 as usize + y) * w + (x0 as usize + x)];
            if a > 0 {
                let i = (y * sw as usize + x) * 4;
                out[i] = c[0];
                out[i + 1] = c[1];
                out[i + 2] = c[2];
                out[i + 3] = a;
            }
        }
    }
    out
}

/// 解析属性值里的颜色三元组：`"0.349 0.573 0.827"`（WE `project.json` 的写法）。
/// 也接受 `"#RRGGBB"`。解析不出来 ⇒ `None`（调用方退回重建）。
pub fn parse_color_triplet(v: &str) -> Option<[f32; 3]> {
    let t = v.trim();
    if let Some(hex) = t.strip_prefix('#') {
        if hex.len() == 6 {
            let p = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok().map(|x| x as f32 / 255.0);
            return Some([p(0)?, p(2)?, p(4)?]);
        }
        return None;
    }
    let nums: Vec<f32> = t
        .split(|c: char| c == ' ' || c == ',')
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<f32>())
        .collect::<Result<_, _>>()
        .ok()?;
    if nums.len() < 3 {
        return None;
    }
    // 0..1 口径；出现 0..255 的口径（>1.0）时按 255 归一 —— 本机 17 条语料里只有 0..1
    let scale = if nums[..3].iter().any(|v| *v > 1.0) { 255.0 } else { 1.0 };
    Some([
        (nums[0] / scale).clamp(0.0, 1.0),
        (nums[1] / scale).clamp(0.0, 1.0),
        (nums[2] / scale).clamp(0.0, 1.0),
    ])
}

/// 从 `scene.json` 原文里找出「用户属性名 → 文字对象 id」的绑定。
/// 只认 `"color": {"user": "<名>"}` / `{"user":{"name":"<名>"}}` 这种**颜色类**绑定，
/// 且对象必须是**文字层**（有 `pointsize`/`text`）—— 文字层的字色是烘进贴图的，
/// 才存在"换色不用重建"这条路；其它对象（材质色、`visible`）仍走重建。
fn scan_text_color_bindings(scene_bytes: &[u8]) -> HashMap<String, i64> {
    let mut out = HashMap::new();
    let Ok(j) = serde_json::from_slice::<serde_json::Value>(scene_bytes) else {
        return out;
    };
    let Some(objs) = j.get("objects").and_then(|o| o.as_array()) else {
        return out;
    };
    for o in objs {
        let Some(col) = o.get("color") else { continue };
        let name = match col.get("user") {
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(serde_json::Value::Object(m)) => m
                .get("name")
                .and_then(|x| x.as_str())
                .map(str::to_string),
            _ => None,
        };
        let Some(name) = name else { continue };
        let is_text = o.get("pointsize").is_some() || o.get("text").is_some();
        let Some(id) = o.get("id").and_then(|x| x.as_i64()) else {
            continue;
        };
        if is_text {
            out.insert(name, id);
        }
    }
    out
}

pub struct SceneRenderer {
    gl: gl_ctx::Gl,
    rts: Vec<gl_ctx::Rt>,
    rt_logical: Vec<Option<(u32, u32)>>,
    rt_index: HashMap<String, usize>,
    /// 上传好的图片/资源贴图：(贴图, 宽, 高)
    texs: Vec<(glow::Texture, u32, u32)>,
    /// **文字层换色的配方**（`WE_SETPROP_FAST=1` 才填，默认空表 ⇒ 与旧行为逐字节相同）：
    /// 对象 id → (贴图下标, 宽, 高, 只留 alpha 的掩膜)。
    /// 为什么只留 alpha：`text::rasterize` 写的是**直通 alpha**（`rgb = 字色`、`a = 覆盖率`），
    /// 所以"换色"= 把 `a > 0` 的像素的 RGB 换成新色，覆盖率一位都不动 ⇒ 与重新光栅化
    /// 逐字节相同（同一个 `(c*255).round`），却不用重建整个场景（实测 620 ms/条）。
    text_recs: HashMap<i64, Vec<TextRec>>,
    /// 用户属性名 → 文字对象 id（同样只在 `WE_SETPROP_FAST=1` 时填）。
    /// 来源：`scene.json` 里 `"color": {"user": "<名>", ...}` 这种绑定 —— 也就是
    /// `set-prop text5=...` 真正作用到的那个字段。
    prop_text: HashMap<String, i64>,
    white: (glow::Texture, u32, u32),
    passes: Vec<Pass>,
    /// WE_PERF_STATIC_CACHE bookkeeping (None = switch off / nothing freezable)
    static_cache: Option<StaticCache>,
    particle_systems: Vec<ParticleInstance>,
    /// 粒子用的 MVP（只有投影，位置已经是场景坐标）
    proj_mvp: [f32; 16],
    time: f32,
    /// 墙钟起点 + 基准：`g_Time` = `time_base + t0.elapsed`。
    /// WE 的 `g_Time` 是**"壁纸开始以来的墙钟秒数"**。这里以前是"每帧 +1/30"（写死 30fps），
    /// 而内核实测跑 20fps ⇒ 场景时间比真实时间**快 1.5 倍** ⇒ 所有靠 `g_Time` 驱动的动画
    /// （`foliagesway` 的摆动、`shake` 的抖动、`waterwaves` 的水波）都偏快 ⇒
    /// 视觉上表现为"各部件各自乱摆、幅度过大"（用户 报的第 1、3 条）。
    t0: std::time::Instant,
    time_base: f32,
    /// 固定步长（秒/帧）。`Some(dt)` ⇒ 时钟按 `dt` 走（CLI 的 `--frames` 采样动画相位用，
    /// 必须**确定性**、与渲染快慢无关）；`None` ⇒ 走墙钟（内核/实时，WE 的 `g_Time` 语义）。
    fixed_dt: Option<f32>,
    /// 上一次 [`SceneRenderer::set_time`] 拿到的**墙钟**值（播放速度按增量累加要用它；
    /// 与 `time`（场景时刻）分开存 —— 倍率 ≠ 1 时两者会拉开）。
    wall_time: f32,
    /// **网格动画时间的独立覆盖**（秒）：`Some(t)` ⇒ 网格装配用 `t`，其余（`g_Time`/粒子/特效）
    /// 仍走场景时钟。用途：把"人物本体在动"从"整场都在动（水波/摆动/粒子）"里**隔离**出来
    /// （差分只反映网格位移）。`None` ⇒ 用场景时钟（正常行为）。
    /// 也可用环境变量 `WE_ANIM_TIME=<秒>` 设（等价，调试用）。
    mesh_time: Option<f32>,
    frames: u64,
    pub title: String,
    pub scene_size: (f32, f32),
    pub cover: (f32, f32, f32, f32),
    pub layer_count: usize,
    /// 诊断：建了多少离屏目标 / 总共多少趟 / 多少粒子对象
    pub rt_count: usize,
    /// 场景级合成 RT = `_rt_FullFrameBuffer`（壁纸级、常驻、画布尺寸）。
    /// 照 linux-wallpaperengine `CWallpaper::setupFramebuffers`：所有图层先合成进它，
    /// 最后一次性贴到屏幕；WE 的整屏后处理（bloom/blur/shine）都读它。
    pub scene_rt: Option<usize>,
    /// 屏幕清屏色 = `scene.json` 的 `general.clearcolor`（**不是**恒黑）。
    /// 依据（两路独立）：
    /// · linux-wallpaperengine `CScene.cpp:89-91`：`glClearColor(scene->colors.clear.r/g/b, 1.0f)`
    /// 屏幕用场景的 clearcolor 清；**中间 FBO** 才清 `(0,0,0,0)`（`CFBO.cpp:61`）。
    /// · OWE `CustomShaderPass.cpp:1108-1112`：clear value 取自 `scene.ClearColor`。
    /// 为什么重要：本机 17 条里多数是 `0 0 0`（黑色，与旧行为一致 ⇒ 逐字节不变），
    /// 但 `2861113973` / `3704273480` 写的是 `0.7 0.7 0.7` —— 亮底场景里"没被任何图层
    /// 盖住"的像素在 WE 是浅灰、在我们以前是纯黑（半个屏幕的底色差异）。
    clear_rgb: [f32; 3],
    /// 参照实现 CImage::render 的语义：对象的**特效链最后一趟**不写 alpha
    /// （glColorMask(1,1,1,0)）。这里记「这些 pass 下标要关掉 alpha 写入」。
    /// 不这么做，链尾会把 alpha 写进目标 RT ⇒ 污染后续半透明合成 ⇒ 雾/光晕发白。
    pub alpha_off: std::collections::HashSet<usize>,
    /// `WE_FUSE_SHIFT`（默认关）：融合候选（趟下标 → 该趟的融合模板）。关着时恒空。
    fuse_tpl: std::collections::HashMap<usize, FuseTpl>,
    /// 融合后**不绘制**的原始 stage 趟下标。不删趟：下标还被 `fx_scripts` /
    /// `mask_warp_passes` / `attach_passes` / `mask_warp_groups` 引用。
    fuse_skip: std::collections::HashSet<usize>,
    /// K1（`colorBlendMode` 1..30 的 shader 混合链路）：**趟下标 → "屏幕拷贝" RT 下标**。
    /// 这些趟在画之前必须先把**目标**（到目前为止的画面）在 GPU 侧拷进那张 RT，
    /// 再由 shader 里的 `ApplyBlending(BLENDMODE, A=拷贝, B=本层, opacity)` 做混合
    /// （官方 `common_blending.h:172-271`；对象趟调用点 `genericimage*.frag` 的 `#if BLENDMODE`）。
    /// 默认路径（`colorBlendMode` 只有 0/31，或 `WE_CBM_SHADER_BLEND=0`）这个表**恒空** ⇒ 一趟都不走、逐字节不变。
    pub cbm_passes: HashMap<usize, (usize, Option<usize>)>,
    /// 挂骨骼（`attachment`）的**逐帧**更新表：`(趟下标, AttachAnim)`。
    /// `WE_ATTACHMENT=1` 且不是 `WE_ATTACH_BAKE=1` 时，命中层的\"摆放趟\"会登记进来；
    /// `draw_frame` 每帧按场景时间 `t` 把这些趟的 `mvp` 更新到 `anchor(t)` 的真值
    /// （OWE 逐帧 `SetLocalFrame(anchor(t)·local_base)`，见 [`AttachAnim`]）。
    /// 默认关（`WE_ATTACHMENT` 未设）时这张表**恒空** ⇒ 每帧那段整块不执行、逐字节不变。
    attach_passes: Vec<AttachPass>,
    /// "屏幕拷贝"用的常驻临时 RT（`_rt_CBMScreenCopy`，画布尺寸）。按需建一次。
    cbm_copy_rt: Option<usize>,
    pub pass_count: usize,
    pub particle_count: usize,
    /// 建链时**被跳过的特效趟**（每条 = "出处｜第 i 趟｜原因"）。
    /// 铁律：任何一趟因为 shader 缺失 / 编译或链接失败 / 材质解析失败 / 特效文件读不到被跳过，
    /// 都必须记在这里并打印 —— 静默跳过会把\"这张壁纸的特效没生效\"伪装成\"作者本来就这样\"，
    /// 实测代价：Gaze(`1195626192`) 唯一的水波纹特效趟被静默跳过，用户报\"水面不波动\"。
    pub skipped_passes: Vec<String>,
    /// 链趟里**打包贴图**的 V 方向策略（从数据推导，见 `FxTexVFlip`）。
    /// 旧的“按注解 `mode:opacitymask` 特判槽位 + 全局开关”已删除（违背“不针对 item 名字/mode”的准则）。
    fx_vflip: FxTexVFlip,
    /// 当前正在解析的那个槽位是否要走反向 V。见 `fx_vflip`。
    fx_slot_flip: bool,
    /// 特效链开关（省电档关掉 ⇒ 只画底图那一刻、不画粒子）
    effects_on: bool,
    /// 逐 RT 的**上报尺寸**（`WE_PERF_RES_SCALE` 的降采样 RT 用逻辑尺寸上报）。
    rt_report: Vec<Option<(u32, u32)>>,
    /// 逐 RT 的**内容包围盒**（GL 像素坐标，y 从底；`WE_PERF_SCISSOR`）。
    /// 为什么要有它：链 RT 是按**对象矩形**分配的（`2539x3570` 这类），而美术内容往往
    /// 只占其中一部分（本场景 `_rt_chain_1016` 的 alpha 只覆盖 **46%**、`_rt_chain_350`
    /// 只有 **20%**）。透明的那些像素每一趟都在被全价着色/混合，属于纯浪费。
    /// 包围盒是**着色器无关**的物理事实（内容在哪），拿它当 `glScissor` 只跳过"每个通道
    /// 都已是 0"的像素 ⇒ 逐字节不变（见 `probe_content_boxes` 的推导）。
    rt_bbox: Vec<Option<[i32; 4]>>,
    /// 「底图趟下标 → 这一层拥有的链 RT」：开屏后逐个探一次内容包围盒。
    bbox_jobs: Vec<(usize, Vec<usize>)>,
    /// **逐趟**内容包围盒（`WE_PERF_SCISSOR_PP`；下标 = 趟号，`None` = 用所在 RT 的那个）。
    /// 在第 1 帧画完之后由 [`SceneRenderer::probe_pass_boxes`] 填一次；空 vec = 还没探过。
    pass_bbox: Vec<Option<[i32; 4]>>,
    /// 文字对象的文本缓存：`对象 id → (上一帧的文本, 求值时刻)`。
    /// 脚本里的时钟只到秒，所以一秒内不重算（boa 虽快，但没必要每帧跑 JS）。
    text_cache: HashMap<i64, (String, f32)>,
    /// 官方铁律①：文字层 RT 的裁切几何 —— `对象 id → (局部平移, 局部半尺寸)`。
    /// 由 `build_text_layer` 填：RT 裁到「墨迹 bbox + 2×padding」，四边形同步换成那块
    /// 矩形（平移 + 半尺寸，单位 = 对象局部场景单位）⇒ 字形在屏上的落点一分不变，
    /// 而缓冲尺寸 = 官方渲染尺寸（`T0`、卡片、RT 三者随之恒等）。
    text_quads: HashMap<i64, ([f32; 2], [f32; 2])>,
    /// 诊断：这次建了几层文字
    pub text_count: usize,
    /// 对象在 `scene.objects` 里的**序号**（判断"图层链接是不是前向引用"要用）
    obj_order: HashMap<i64, usize>,
    /// 当前正在处理的对象序号
    cur_obj_idx: usize,
    /// puppet 网格图层的**收尾合成几何**（装配后的逐帧顶点 + 网格 uv/索引）。
    /// 为什么要单独留一份：puppet 图层的链缓冲（RT）必须跑在**网格自己的 uv/图集空间**
    /// （装配只发生在收尾合成趟），所以底图/链趟用的是**展开**几何、收尾趟用的是**装配**几何，
    /// 两者共用同一套 uv/索引。见 `mesh_uvspace_on` 的长注释。
    cur_mesh_comp: Option<MeshComp>,
    /// 当前正在处理的 mesh 图层是否需要把链跑在 uv 空间（`WE_MESH_UVSPACE=0` 关）
    cur_mesh_uvspace: bool,
    /// 当前 mesh 图层**底图趟**最终的 uv 朝向（`raw_position`）——收尾合成趟要按同一朝向读回。
    cur_mesh_base_flip: bool,
    /// 当前层的**遮罩烘焙源**（把遮罩按网格蒙皮画进「帧空间」的那一趟用）。
    /// `(t=0 装配顶点, uv, 索引)`；`None` ⇒ 这一层不走烘焙（没有网格 / 走的是图集底图）。
    /// 为什么要 bake 而不是仿射：遮罩画在**图集空间**，而本模型的底图趟画的是**装配后**的网格
    /// （`mesh_atlas_residual` 0.746 ⇒ 装配空间），两者之间是**逐骨仿射**、不是一个全局仿射
    /// ⇒ 只有「用同一份网格几何把遮罩画一遍」才能逐像素对齐（见 `mask_atlas_space_on`）。
    cur_mask_warp_src: Option<(Vec<[f32; 3]>, Vec<[f32; 2]>, Vec<u16>)>,
    /// 当前层底图趟的 MVP —— 遮罩烘焙趟必须用**同一个**，遮罩才跟着网格走。
    cur_mask_warp_mvp: Option<[f32; 16]>,
    /// 当前层底图趟的混合方式 —— 烘焙趟必须一致：网格是多块**互相重叠**的碎块，
    /// 用 Opaque 画会把半透明碎块的 RGB 直接写进缓冲（实测：头发/披纱多出一堆
    /// 纯色四边形），用材质的 translucent 才与内容同覆盖。
    cur_mask_warp_blend: gl_ctx::Blend,
    /// 本层共享的遮罩烘焙 RT 池（`_rt_maskwarp<n>_<id>`，每趟前清空）。
    /// 一趟最多要搬两张画布空间贴图（如 `shake` 的位移场 + 遮罩、`waterripple` 的
    /// 法线图 + 遮罩）⇒ 池大小 = 该层单趟的最大不同贴图数（封顶 2）。
    cur_mask_warp_pool: Vec<usize>,
    /// 当前层**底图趟**的下标（烘焙趟每帧复用它算出的装配顶点）。
    cur_mask_warp_base: Option<usize>,
    /// `(底图趟下标, [烘焙趟下标…])` —— draw_frame 里清屏 + 复用底图趟顶点。
    mask_warp_groups: Vec<(usize, Vec<usize>)>,
    /// 已建出来的遮罩烘焙趟下标（draw_frame 里清屏 + 复用底图趟的顶点）。
    mask_warp_passes: Vec<usize>,
    /// 本帧底图趟算出的装配顶点（给烘焙趟复用，避免对同一网格重复蒙皮/装配）。
    mask_warp_pos: Option<Vec<[f32; 3]>>,
    /// 当前**特效开始时**的输入（= 这个特效要处理的那张画面）。
    /// `effect.json` 里 `bind: {name:"previous"}` 指的就是它，**不是"上一趟输出"**：
    /// `shine` 的中转全写在显式 RT 名里（`_rt_HalfCompoBuffer1/2`），而 `shine_combine`
    /// 的 slot1 用 `previous` 要的是**原图**（shader 里叫 `albedo`）⇒ 我们按"上一趟输出"
    /// 喂成光晕自己 ⇒ 自己和自己合成 ⇒ **整层全黑**（实测 `3504393434` 亮度 122→0）。
    cur_effect_input: Src,
    /// 贴图边长上限（px）：超过就等比降采样。**默认关**。
    /// ④-4 的结论：4000x2736 的贴图按画布尺寸压到 2560x1751，
    /// 名义显存 41.7→17.1MB/层（日志里有），画质差 0.85/255（察觉不到），
    /// **但内存收益在任何可测层面都没出现**：进程 Rss 174→158（反而多 16MB）、
    /// Pss 158→142（同样多 16）、机器 MemAvailable 2758→2827MB（反而少 69）。
    /// ⇒ 默认关，别拿名义显存当账；将来真遇到"贴图远大于画布 + 内存吃紧"再打开。
    /// `WE_TEX_CAP=canvas`（或 `on`）按画布压；`WE_TEX_CAP=<n>` 指定像素上限；`off`/`0` = 不限。
    tex_cap: u32,
    /// 光斑/柔光机制（`WE_GLOW=1` 开，**默认关**）。见文件上方 `GlowCfg` 的说明。
    glow: GlowCfg,
    /// 光斑批（程序 + 软光斑贴图）；粒子光斑关闭时是 `None`。
    glow_batch: Option<gl_ctx::GlBatch>,
    /// 光斑顶点复用缓冲（每帧重建）。
    glow_verts: Vec<[f32; gl_ctx::PARTICLE_FLOATS]>,
    /// 全屏辉光的 4 趟（降采样+暖色加权 → 纵/横模糊 → 加法叠回屏幕）。
    /// **执行放在粒子之后**（要读"含粒子在内的整帧屏幕"），所以不放进 `passes`。
    glow_passes: Vec<Pass>,
    /// 全屏辉光读屏用的源 RT（画布尺寸）；`None` = 全屏辉光没开。
    glow_src: Option<usize>,
    /// **glow 在曝光之前**那条路要把 present 趟推迟到"粒子+glow 之后"再跑 ⇒ 先存在这里。
    /// `None` = 走老路（present 仍在 `passes` 末尾、粒子直接画屏幕）。
    pending_present: Option<Pass>,

    /// ── 切换壁纸的过渡（旧帧 → 新场景 crossfade）───────────────────────────
    /// 旧壁纸最后一帧的纹理 + 尺寸。过渡结束就释放（不占显存）。
    fade_tex: Option<(glow::Texture, u32, u32)>,
    /// 过渡的开始时刻 + 总时长（ms）。`None` = 不在过渡中（**默认状态 ⇒ 零开销**）。
    fade: Option<(std::time::Instant, f32)>,
    /// 过渡覆盖层（全屏四边形 + `g_Alpha`）。**只有真发生过过渡才建**，之后留到渲染器销毁。
    fade_layer: Option<gl_ctx::GlLayer>,
    /// 离线验证钩子 `WE_FADE_TEST=<png>:<alpha>`：每帧按固定 alpha 叠这张图
    /// （α=1 时输出应**逐字节**等于那张图 ⇒ 一次性验证覆盖层的几何与朝向）。
    fade_test: Option<(glow::Texture, u32, u32, f32)>,

    /// ── 鼠标输入──────────────────────────────────────────────
    /// 当前鼠标位置，**屏幕归一化 [0,1]、y 从屏幕顶往下**（与 `WE_MOUSE=x,y` 同一口径）。
    /// 为什么是渲染器里的**运行时状态**而不是 `build` 时的常量：WE 把鼠标位当成**每帧**
    /// 下发的场景 uniform（参照实现 `CPass.cpp:879`：
    /// `addUniform("g_PointerPosition", scene.getMousePosition)`），它驱动
    /// `effects/xray`（鼠标处透视）与 `camera.parallax`（鼠标视差）。若烘进 build，
    /// 鼠标每动一下就要重建渲染器（本机实测几百毫秒）⇒ 不可用。
    /// `None` = 没有鼠标输入 ⇒ 走 `pointer_default`（**不是 0**，见那个字段的注释）。
    mouse: Option<(f32, f32)>,
    /// 上一帧的鼠标位（WE 的 `g_PointerPositionLast`，给"跟手速度"类 shader 用）。
    mouse_last: Option<(f32, f32)>,
    /// ── 指针在**没有输入**时的默认位（= `g_PointerPosition` 的兜底值）──
    /// 官方语义：`g_PointerPosition` 是**每帧下发**的场景级指针位置
    /// （LWE `CPass.cpp:879` `addUniform("g_PointerPosition", scene.getMousePosition)`），
    /// 而**没有输入时它不是 0** —— OWE 的 Scene 成员初始化就是**屏幕中心**：
    /// `open-wallpaper-engine/src/Scene/Scene/Scene.cppm:2872`
    /// `array<float,2> m_pointer_position { 0.5f, 0.5f };`
    /// （LWE 用 `glm::vec2 m_mousePosition = {}` = (0,0)；那是它的取值，不是 WE 的。）
    /// 为什么这一条是"X-Ray 透明层不显示"的**直接原因**：`effects/xray`（官方半明源码）
    /// 片元里
    /// `unprojectedUVs = saturate(texSource - v_PointerUV.xy/v_PointerUV.z)`
    /// 之后拿它去采样"光斑精灵"（`g_Texture2`）。pointer=(0,0) ⇒ `pointer*2-1 = (-1,-1)`
    /// ⇒ 采样点被推到贴图**角落**，而精灵边缘的 alpha ≈ 0 ⇒ `blendSample.x*y ≈ 0`
    /// ⇒ `blend = mask.a*g_Multiply*sprite ≈ 0` ⇒ `ApplyBlending` 恒等 ⇒
    /// **整层一丝变化都没有**。实测（崩坏3 `2564205376`，640x360，桌面 env）：
    /// `WE_PROP_xrayversion=true WE_PROP_sfwversion=false`（不给鼠标）
    /// vs 同配置 `WE_MAX_EFFECTS=0`（整条特效链不跑）⇒ **变化像素 0**、md5 相同
    /// （`6101f023ebe14fa5`），即"开了 X-Ray 等于没开"。
    /// 桌面内核拿不到指针时（指针压在窗口之上 ⇒ Background 层收不到 enter/motion）
    /// 就停在这个状态 ⇒ 用户反复报"xray 还是没有 / 透明层不显示"。
    /// 默认 = 中心 `(0.5,0.5)`（与 OWE 一致）；`WE_POINTER_DEFAULT=zero` 回旧口径
    /// `(0,0)` 供 A/B（两条口径对手上这 17 条场景的输出**逐字节相同**，见报告）。
    pointer_default: [f32; 2],
    /// 是否把指针注入每一趟（`WE_POINTER=0` 关 = 加这一路之前的逐字节行为）。
    pointer_on: bool,
    /// ── **"把指针反投影进图层纹理空间"的那一族趟**（pass 下标）──
    /// 判据是**本趟 shader 自己的声明**（数据驱动、不看 id/名字/effect 名）：
    /// 同时声明 `g_PointerPosition` 与 `g_EffectTextureProjectionMatrixInverse`
    /// ⇒ 就是官方 `effects/xray`（以及 `cursorripple` / `fluidsimulation` / `depthparallax`
    /// 这一族）的形态：指针不是"屏幕坐标"，而是**要被投影进这一层的纹理空间**的坐标。
    /// 对这一族来说 `(0,0)` **不是一个合法位置**（它落在图层角外）⇒ 整条效果静默失效
    /// （见 `pointer_default` 注释里的实测）。所以**只有这些趟**在"没有输入"时会拿到
    /// 兜底指针位。
    /// 为什么不做成"所有趟都拿兜底位"：只用 `g_PointerPosition` 当**屏幕坐标**的特效
    /// （本机实例：`3704273480` 主题2 的 `effects/dot_matrix`，`vec2 mouse = g_PointerPosition`）
    /// 会因此换一个落点 —— 那是**行为变化**，会动到"主题2 = `8f581dc1…`"这条护栏。
    /// 护栏优先：那一族保持旧口径（0），如实记在报告"未做"一节。
    pointer_proj: std::collections::HashSet<usize>,
    /// 逐趟把"每帧都在变的场景级 uniform"（鼠标）并进标量表时**复用的缓冲**。
    /// 复用 ⇒ 稳态零分配（`clear` 不还容量，只在容量不够时增长）。
    frame_scalars: Vec<(String, Value)>,
    /// 逐帧脚本驱动的**特效趟标量**（`constantshadervalues.<key>` 写成 `{script,value}`）。
    /// 存 `(pass 下标, [(uniform 名, 脚本)])`，不放进 `Pass`：`Pass` 有 18 个构造点，
    /// 加字段要全改。这张表只在 `WE_SCRIPT_AUDIO=1` 且场景真有脚本常量时非空
    /// （本机 = `just a girl v.2` 的 6 个特效 `strength`），默认空 ⇒ 零开销、逐字节不变。
    /// 依据（官方语义）：绑定 `{script, value}` 的**每一帧**都求值（OWE
    /// `Scene/Script/Script.cpp` 的 `RunFieldScriptInit` / `update`；`value` 只是初值），
    /// 我们以前只取 `value` ⇒ 音频响应永远不生效。
    fx_scripts: Vec<(usize, Vec<(String, propscript::NumScript)>)>,
    /// 上面那条的**本帧求值结果**（每帧重建；只有 scripted 的几趟有项）。
    /// 作为**追加**项并进该趟的标量表（`gl_ctx::draw_pass` 按序下发 ⇒ 同名后者覆盖）。
    fx_frame_scalars: Vec<(usize, Vec<(String, Value)>)>,
    /// 场景级鼠标视差的**平滑位移**（WE `CScene::m_parallaxDisplacement` 的同名物）
    /// **已接线**：现在指向 [`parallax::State`] 那个状态机里的位移，
    /// 见下面 `par`。
    parallax_disp: [f32; 2],
    /// 相机视差状态机（`general.cameraparallax*` + 对象 `parallaxDepth`，见 [`parallax`]）。
    /// **默认关**：只有 `WE_PARALLAX=1` **且**场景声明 `cameraparallax=true` 才被推动。
    par: parallax::State,
    /// 视差是否真的开着（`WE_PARALLAX=1` && `general.cameraparallax`）。关着时下面那张表是空的
    /// ⇒ 逐趟那条快路径一次都不查表（零开销）。
    par_on: bool,
    /// 「把这一层放进场景」的那一趟 ⇒ 它的 `parallaxDepth`：`pass 下标 → [base_x,base_y,k_x,k_y]`
    /// （`k = depth ⊙ amount`，`base = (node − camera) ⊙ k`；每帧的鼠标项再乘 `k` 加上去）。
    /// 用 **pass 下标**做键而不是给 `Pass` 加字段：`Pass` 有 18 处构造点，改结构体要全改一遍。
    parallax_passes: HashMap<usize, [f32; 4]>,
    /// 上一帧的场景时间（`cameraparallaxdelay` 的 `mix` 因子要 `g_Time - g_TimeLast`）。
    time_last: f32,
    /// 场景里的**灯对象**（`"light"`），顺序 = `objects[]` 次序
    /// （= OWE `SceneFinalize.cpp:146-152` 把 `scene.Lights` 按注册次序填进 4 个槽）。
    light_srcs: Vec<LightSrc>,
    /// `general.ambientcolor` ⇒ `g_LightAmbientColor`
    light_ambient: [f32; 3],
    /// `general.skylightcolor` ⇒ `g_LightSkylightColor`
    light_skylight: [f32; 3],
}

/// 链趟里**打包贴图**（非 RT）的 V 方向策略 —— 从数据推导，**不看 item 名字、不看注解 `mode`**。
/// 背景：
/// 链上每一趟读 RT 的全屏四边形是 **V 反向**的那套（`WE_FX_QUAD_FLIP`，默认开）
/// `Pass::layer.raw_position`）。这个翻转同时作用在**打包贴图**上。
/// 包内**没有任何字段写着“这张遮罩要翻”**：shader 注解（`mode`/label）、材质 `textures`、
/// `scene.json` 的对象字段里都不含 V 方向信息（逐文件核过 17 条场景）。
/// 真正决定“要不要翻”的是**链趟四边形相对底图趟的 V 约定差**，而这个差随链跑在
/// **画布空间**还是**图层空间**而变（图层空间那条路给底图趟多翻了一次，见 `layer_space`）。
/// 品系里唯一能把两种**正确**取向分开的**声明字段**是对象的 `copybackground`：
/// `1195626192`(Gaze, waterripple 遮罩必须落在水面) `copybackground=true` ⇒ 必须翻；
/// `2861113973`(Ice Princess, `рыцари` 的水波不该上到天空) 与 `3704273480`(夏日影，护栏)
/// 的那些层没有该字段 ⇒ 不能翻。实测（`WE_FX_PROBE=mask` 把遮罩画出来）：
/// Gaze 不翻 ⇒ 白带落在屏幕 v∈[0,0.3]（云）；翻 ⇒ v∈[0.7,0.9]（水面，与作者画的重合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FxTexVFlip {
    /// 永不翻（旧行为，A/B 用）。
    Off,
    /// **默认**：按数据规则 —— `copybackground` 且链跑在画布空间。
    CopyBackground,
    /// 永远翻（A/B 用）。
    All,
}

impl FxTexVFlip {
    /// `WE_FX_TEX_VFLIP=0|1|all|copybg`（缺省 = `copybg` 数据规则；`1`/`all` 都表示全翻，A/B 用）。
    fn from_env() -> Self {
        match std::env::var("WE_FX_TEX_VFLIP").ok().as_deref() {
            Some("0") | Some("off") => FxTexVFlip::Off,
            Some("1") | Some("all") => FxTexVFlip::All,
            _ => FxTexVFlip::CopyBackground,
        }
    }

    /// 该对象这一趟链的打包贴图是否要反向 V。
    /// `copy_background` 是 `scene.json` 对象的声明字段（`we_scene::scene::Object::copy_background`）；
    /// `layer_space` 是这条链自己的空间选择（图层空间那条路给底图趟多翻了一次 ⇒ 不需要再翻贴图）。
    fn want_flip(self, copy_background: bool, layer_space: bool) -> bool {
        match self {
            FxTexVFlip::Off => false,
            FxTexVFlip::All => true,
            FxTexVFlip::CopyBackground => copy_background && !layer_space,
        }
    }
}

/// 自己写的箱式降采样：目标像素覆盖到的源像素取平均。
/// **为什么不用 `image::imageops::resize`**：它内部按 f32 逐通道开中间缓冲，
/// 4000x2736 → 2560x1751 光中间态就要 ~112MB —— 实测 CLI 峰值 RSS 从 173MB 涨到 **298MB**，
/// 捡了稳态的那 49MB 显存却把峰值砸了，这笔账不划算。箱式只用 源+目标 两块缓冲（43+17MB）。
fn box_downsample(img: &image::RgbaImage, nw: u32, nh: u32) -> image::RgbaImage {
    let (w, h) = img.dimensions();
    if nw == 0 || nh == 0 || w == 0 || h == 0 {
        return image::RgbaImage::new(1, 1);
    }
    let src = img.as_raw();
    let mut out = vec![0u8; (nw as usize) * (nh as usize) * 4];
    let (wu, hu) = (w as u64, h as u64);
    for oy in 0..nh {
        let y0 = (oy as u64 * hu / nh as u64) as u32;
        // 覆盖范围右/下开区间（至少 1 个像素），保证不丢最后一行/一列
        let y1 = ((((oy as u64 + 1) * hu + nh as u64 - 1) / nh as u64) as u32).clamp(y0 + 1, h);
        for ox in 0..nw {
            let x0 = (ox as u64 * wu / nw as u64) as u32;
            let x1 = ((((ox as u64 + 1) * wu + nw as u64 - 1) / nw as u64) as u32).clamp(x0 + 1, w);
            let mut acc = [0u32; 4];
            let mut n = 0u32;
            for y in y0..y1 {
                let row = (y as usize) * (w as usize) * 4;
                for x in x0..x1 {
                    let i = row + (x as usize) * 4;
                    acc[0] += src[i] as u32;
                    acc[1] += src[i + 1] as u32;
                    acc[2] += src[i + 2] as u32;
                    acc[3] += src[i + 3] as u32;
                    n += 1;
                }
            }
            let o = ((oy as usize) * (nw as usize) + ox as usize) * 4;
            let n = n.max(1);
            for c in 0..4 {
                out[o + c] = (acc[c] / n) as u8;
            }
        }
    }
    image::RgbaImage::from_raw(nw, nh, out).unwrap_or_else(|| image::RgbaImage::new(1, 1))
}

/// 按上限等比降采样一张贴图（不改内容比例 ⇒ 归一化 UV 的采样数学完全不受影响）。
/// `origin` 是**锚点**：`alignment` 说明它落在对象矩形的哪个位置；返回"锚点 → 中心"的偏移。
/// WE 默认是 `center`（此时偏移为 0）；其余是四角/四边中点（Y 的符号见函数内注释）。
fn alignment_offset(align: Option<&str>, size: [f32; 2], scale: [f32; 3]) -> (f32, f32) {
    // 幅度按 WE `updateScenePosition`（CImage.cpp:987-1015）的 **`scaledSize = size × scale`**：
    // 以前用裸 `size/2`，对 scale≠1 的对象偏小（3704273480 的 `球` 差 175.5 vs 296.1）。
    // `scale` = 父链 scale 的连乘（容器子的父 scale 一般是 1，多级链才不同）。
    let (w, h) = (size[0] * scale[0] / 2.0, size[1] * scale[1] / 2.0);
    // 锚点在对象矩形的哪条边，中心就往**对侧**偏半个尺寸：`top` 锚 ⇒ 中心在下 ⇒ y = −h。
    // 投影是**普通的**正交（`Mat4::ortho(cx0,cx1,cy0,cy1,…)` ⇒ 场景 +y = 屏幕上方）——
    // 即这一处的符号**本来就对**；以前那句"投影对 Y 做了翻转补偿"的注释是过时的错误理由，
    // 已随投影修正一并改正（改投影时**不要**再顺手翻这里，会翻错）。
    match align.unwrap_or("center") {
        "topleft" => (w, -h),
        "topright" => (-w, -h),
        "bottomleft" => (w, h),
        "bottomright" => (-w, h),
        "left" => (w, 0.0),
        "right" => (-w, 0.0),
        "top" => (0.0, -h),
        "bottom" => (0.0, h),
        _ => (0.0, 0.0), // center（含未知值）
    }
}

/// 把一趟的 MVP **左乘**一个 NDC 平移（正交投影下"场景位移"就是 NDC 位移 ⇒ 直接加在裁剪空间）。
/// `uploaded` 是喂给 uniform 的那份数组（= `Mᵀ` 的列主序布局，见 `gl_ctx::t4` 的注释）；
/// 先换回数学矩阵 `M`，左乘 `T`，再按同一口径打包回去。
fn shift_mvp_ndc(uploaded: &[f32; 16], d: [f32; 2]) -> [f32; 16] {
    if d[0] == 0.0 && d[1] == 0.0 {
        return *uploaded;
    }
    let m = Mat4 { m: *uploaded }.transposed();
    Mat4::translate([d[0], d[1], 0.0])
        .mul(&m)
        .transposed()
        .column_major()
}

/// 一层的相机视差位移（场景像素）= `base + mouse ⊙ k`。
/// `pk = [base_x, base_y, k_x, k_y]`：`base` 是**场景像素**（`(node − camera) ⊙ k`，
/// 建场时算好），`k = depth ⊙ amount`（无量纲）。`par_shift` 是**鼠标项的 NDC 位移**
/// （`2·mouse/vis`），`vis` 是可见区尺寸。
/// 修的单位错：这里**必须**先把 `par_shift` 乘回场景像素
/// （`× vis/2`）再乘 `k`，否则鼠标项被 `vis/2` 除了一次 —— 本机 `vis=3840x2160` 时
/// 小 **1920 倍**（实测鼠标在两个对角上的差别小到 <24 级/通道、肉眼不可见）。
/// 恒等式（也是单测 `parallax_mouse_term_is_vis_independent` 断言的那条）：
/// `off == base + mouse_term ⊙ k`，**与 `vis` 无关**。
fn parallax_layer_off(pk: [f32; 4], par_shift: [f32; 2], vis: [f32; 2]) -> [f32; 2] {
    [
        pk[0] + par_shift[0] * 0.5 * vis[0] * pk[2],
        pk[1] + par_shift[1] * 0.5 * vis[1] * pk[3],
    ]
}

/// 从 `.tex` 头部读出"美术实际用到的尺寸"（第 2 组尺寸，`TEXI` 的 mapWidth/mapHeight）。
/// 有的 `.tex` 会**把美术塞进更大的纹理**（本机 `1195626192`：纹理 8192x4096、美术 6000x3375），
/// 这时按 UV 0..1 采样就会把空白也画上 ⇒ 表现为"美术只占一角、其余黑"。返回 None 表示头不合法。
fn art_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 42 || &bytes[0..8] != b"TEXV0005" || &bytes[9..17] != b"TEXI0001" {
        return None;
    }
    let rd = |o: usize| -> i32 {
        i32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]])
    };
    let (aw, ah) = (rd(34), rd(38));
    if aw > 0 && ah > 0 {
        Some((aw as u32, ah as u32))
    } else {
        None
    }
}

/// `.tex` 头部 `flags`（`bit1 = clampUVs`）是否参与采样状态的选择：**默认开**。
/// WE 里采样状态是**数据声明**的（`TexImageParser.cpp:161`：`clampUVs ? CLAMP_TO_EDGE : REPEAT`），
/// 不是渲染器的自由选择 ⇒ 默认按数据走。
/// `WE_TEX_FLAGS=0` 显式回退旧行为（恒定 CLAMP_TO_EDGE），保留它是为了能逐字节对照
/// 改动前后的输出（离线 CLI 与桌面口径因此一致，drop-in 里那行 `Environment=` 可撤）。
fn tex_flags_enabled_from(v: Option<&str>) -> bool {
    // 只有显式写 "0" 才关；未设置 / 空串 / 其它值一律按开（与旧开关的"!=0 即开"一致）
    !matches!(v, Some("0"))
}

/// 进程环境里的同一件事（`WE_TEX_FLAGS`）。
fn tex_flags_enabled() -> bool {
    tex_flags_enabled_from(std::env::var("WE_TEX_FLAGS").ok().as_deref())
}

/// 这张贴图该不该用 **REPEAT** 采样（否则 CLAMP_TO_EDGE）。
/// `flags.bit1` 解析不出来（不是 `.tex` / 头太短）时按 CLAMP 兜底 —— 与旧行为一致。
fn wrap_repeat(enabled: bool, repeat_flag: Option<bool>) -> bool {
    enabled && repeat_flag.unwrap_or(false)
}

fn cap_texture(img: image::RgbaImage, cap: u32, what: &str) -> image::RgbaImage {
    let (w, h) = img.dimensions();
    let long = w.max(h);
    if cap == 0 || long <= cap {
        return img;
    }
    let s = cap as f64 / long as f64;
    let nw = ((w as f64 * s).round() as u32).max(1);
    let nh = ((h as f64 * s).round() as u32).max(1);
    let out = box_downsample(&img, nw, nh);
    // 报出来：省了多少显存要看得见（不然"内存压了一档"没有证据）
    let mb = |w: u32, h: u32| (w as f64 * h as f64 * 4.0) / (1024.0 * 1024.0);
    println!(
        "[贴图] {what} {w}x{h} → {nw}x{nh}（上限 {cap}px，{:.1}MB → {:.1}MB）",
        mb(w, h),
        mb(nw, nh)
    );
    out
}

impl SceneRenderer {
    pub fn we_assets() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        Path::new(&home).join(".local/share/Steam/steamapps/common/wallpaper_engine/assets")
    }

    pub fn open(dir: &Path, w: u32, h: u32) -> Result<SceneRenderer, String> {
        Self::open_with_gl(dir, w, h, gl_ctx::Gl::new(w, h))
    }

    /// **0 拷贝模式**：用 EGL 窗口表面（wayland 平台 + `wl_egl_window`）渲染，
    /// 每帧末 `swap_buffers` 交给合成器 —— **CPU 完全不碰像素**。
    /// `display_ptr` / `surface_ptr` 是裸的 `wl_display*` / `wl_surface*`
    /// （`wl_display*` 需要 wayland-client 的 **sys 后端**）。
///
    /// # Safety
    /// 指针必须来自同一个活着的 Wayland 连接，且 surface 尚未销毁。
    pub unsafe fn open_window(
        dir: &Path,
        w: u32,
        h: u32,
        display_ptr: *mut std::ffi::c_void,
        surface_ptr: *mut std::ffi::c_void,
    ) -> Result<SceneRenderer, String> {
        Self::open_window_fade(dir, w, h, display_ptr, surface_ptr, None)
    }

    /// 同 `open_window`，但可带一张**预热帧**：建 EGL 窗口后那一次"清屏 + swap"
    /// 改成画这张图。切换壁纸时内核把**旧壁纸的最后一帧**传进来 ⇒
    /// 「旧渲染器已丢、新场景还在构建」这几百毫秒里屏幕上一直是旧画面，**没有黑屏**。
    /// `preload` = (图像取向 RGBA, w, h, flip?)。flip 只有调试时用（默认 false，与
    /// 过渡覆盖层 `make_fade_layer` 的朝向一致）。
///
    /// # Safety
    /// 同 `open_window`。
    pub unsafe fn open_window_fade(
        dir: &Path,
        w: u32,
        h: u32,
        display_ptr: *mut std::ffi::c_void,
        surface_ptr: *mut std::ffi::c_void,
        preload: Option<(&[u8], u32, u32, bool)>,
    ) -> Result<SceneRenderer, String> {
        Self::open_with_gl(
            dir,
            w,
            h,
            gl_ctx::Gl::new_window(w, h, display_ptr, surface_ptr, preload),
        )
    }

    fn open_with_gl(
        dir: &Path,
        w: u32,
        h: u32,
        gl: Result<gl_ctx::Gl, String>,
    ) -> Result<SceneRenderer, String> {
        let project = we_scene::Project::read_dir(dir).map_err(|e| e.to_string())?;
        // 音频频谱（默认关）：`WE_AUDIO_SPECTRUM=1` 时起采集线程 / 读静态频谱。
        // 放在这里 = 内核与离线 CLI **同一入口**；没设环境变量时一行都不做。
        audio::autostart();
        let pkg_bytes = std::fs::read(project.scene_pkg()).map_err(|e| e.to_string())?;
        let pkg = we_scene::pkg::Pkg::parse(&pkg_bytes).map_err(|e| e.to_string())?;
        let scene_entry = pkg.scene_json_entry().ok_or("包里没有 scene.json")?;
        let scene_bytes = pkg.slice(&pkg_bytes, scene_entry).ok_or("scene.json 越界")?;
        let scene = we_scene::scene::parse_scene(scene_bytes).map_err(|e| e.to_string())?;
        // `WE_SETPROP_FAST=1`：建"用户属性名 → 文字对象 id"表（默认空 ⇒ 零影响）
        let prop_text = if setprop_fast_on() {
            scan_text_color_bindings(scene_bytes)
        } else {
            HashMap::new()
        };
        // 动画起始相位表（`animationlayers[].visible` 的 `setFrame` 脚本；`WE_ANIM_STARTPHASE=0` 关）
        let n_start = install_anim_start(scene_bytes);
        if std::env::var_os("WE_DEBUG_ANIMPHASE").is_some() {
            eprintln!("[动画相位] 从 scene.json 解析出 {n_start} 条起始帧");
        }
        let assets = Self::we_assets();

        let scene_w = scene.general.orthographic.width;
        let scene_h = scene.general.orthographic.height;
        let fit_mode = current_fit();
        // 命令行/调试旋钮：`WE_COLOR=亮度,对比度,饱和度,色相,色温,伽马`（单位值 1,1,1,0,0,1）
        // 内核用 `set color=…` 走 set_global_color；这里只为命令行与脚本化验证服务。
        if let Ok(s) = std::env::var("WE_COLOR") {
            let v: Vec<f32> = s.split(',').filter_map(|t| t.trim().parse().ok()).collect();
            if v.len() == 6 {
                set_global_color([v[0], v[1], v[2], v[3], v[4], v[5]]);
            }
        }
        let (cx0, cy0, cx1, cy1) = fit_rect(scene_w, scene_h, w as f32, h as f32, fit_mode);
        if std::env::var_os("WE_DEBUG_PROJ").is_some() {
            eprintln!(
                "[投影] 目标 {w}x{h}（比 {:.4}）  场景 {scene_w}x{scene_h}（比 {:.4}）  取景={fit_mode:?}  ⇒ 可见区 x[{cx0},{cx1}] y[{cy0},{cy1}] = {:.0}x{:.0}",
                w as f32 / h as f32,
                scene_w / scene_h,
                cx1 - cx0,
                cy1 - cy0
            );
        }
        let gl = gl?;
        // ⚠ WE 的场景坐标是 **y 向下**（原点左上、y 越大越靠下），而 GL 的 NDC 是 y 向上。
        // ⚠ **这里不能翻 Y**（历史教训：以前为压住"上下颠倒"的症状，把 (bottom, top) 反着传，
        // 结果变成"投影翻一次 + 输出路径再翻一次"⇒ 不同壁纸表现不一致）。
        // 权威依据（三方一致）：
        // ① WE 原版 shader：全程不翻 V；唯一的 `-y` 在 `#ifdef HLSL` 的**屏幕坐标**上
        // （`~/Downloads/wallpaper_engine/assets/shaders/genericimage3.vert`）；
        // 原版特效 shader 更直接写了：`pointer.y = 1.0 - pointer.y; // Flip pointer screen
        // space Y to match texture space Y` ⇒ 屏幕空间与纹理空间 Y 相反，翻转只发生在
        // "进纹理空间"那一次；
        // ② Linux 移植项目（`linux-wallpaperengine`）`Camera.cpp:49`：
        // `glm::ortho(-w/2, w/2, -h/2, h/2, …)` —— 普通正交，一次不翻；
        // 那"一次翻转"落在**最终合成的 UV 端点**上（`WallpaperState.cpp` 的 `resetUVs`：
        // `vstart/vend` 按输出目标（默认帧缓冲 vs FBO）取 0/1 还是 1/0）。
        // ⇒ 结论：**投影保持普通（+y = 屏幕上方），翻转只在输出边界发生一次**。
        let proj = Mat4::ortho(cx0, cx1, cy0, cy1, -1000.0, 1000.0);
        let white = gl.make_white_texture()?;
        // ── 相机视差（`general.cameraparallax*`）────────────────────────────────
        // 公式/规则照 OWE（见 `parallax` 模块的文档注释）。规则与开关：
        // · **默认关**：`WE_PARALLAX=1` **且**场景声明 `cameraparallax=true` 才生效
        // · `cameraparallaxdelay` 在 `we_scene::General` 里没有解析（只有 amount/influence）
        // ⇒ 直接从包里的 scene.json 原文字段读。
        let par_cfg = parallax::Cfg {
            enable: scene.general.camera_parallax,
            amount: scene.general.camera_parallax_amount,
            delay: pkg
                .get(&pkg_bytes, "scene.json")
                .and_then(|b| we_scene::json::from_slice::<serde_json::Value>(b).ok())
                .and_then(|j| j.get("general")?.get("cameraparallaxdelay")?.as_f64())
                .map(|v| v as f32)
                .unwrap_or(0.1),
            mouse_influence: scene.general.camera_parallax_mouse_influence,
        };
        let par_on = std::env::var("WE_PARALLAX").ok().as_deref() == Some("1") && par_cfg.enable;
        if std::env::var_os("WE_PARALLAX").is_some() {
            eprintln!(
                "[视差] 场景声明 enable={} amount={} delay={} mouseinfluence={}  ⇒ {}（WE_PARALLAX=1 才开）",
                par_cfg.enable,
                par_cfg.amount,
                par_cfg.delay,
                par_cfg.mouse_influence,
                if par_on { "生效" } else { "不生效" }
            );
        }
        // 相机位置：取**可见区中心**（我们的正交投影以可见区为中心 ⇒ 相机就在那里）。
        // `ortho_implicit = true`：我们这条管线恒为正交投影（`Mat4::ortho`）⇒ 缺省
        // `parallaxDepth` 用 OWE 的 `kImplicitOrthographicParallaxDepth = {1,1}`。
        let par = parallax::State::new(
            par_cfg,
            [scene_w, scene_h],
            [(cx0 + cx1) * 0.5, (cy0 + cy1) * 0.5],
            true,
        );

        // ── 场景灯（`objects[]` 里的 `"light"` 对象）+ 光照环境色 ─────────────────
        // 打包/下发口径见 `gl_ctx::LightUniforms` 的注释（每一条都有官方或 OWE 出处）。
        // 这里只做两件事：①从类型化场景里收集灯；②把 `general.ambientcolor/skylightcolor`
        // 解析成 vec3。原始 `scene.json` 只有一个用途 —— 取 `origin`/`radius` 的
        // **`animation` 曲线**（`we-scene` 的类型化字段只保留静态端）。
        let raw_scene_json: Option<serde_json::Value> =
            pkg.get(&pkg_bytes, "scene.json").and_then(|b| we_scene::json::from_slice(b).ok());
        let light_srcs = lights_from_scene(&scene, raw_scene_json.as_ref());
        let light_ambient =
            we_scene::scene::parse_vec3(&scene.general.ambient_color).unwrap_or([0.0, 0.0, 0.0]);
        let light_skylight =
            we_scene::scene::parse_vec3(&scene.general.skylight_color).unwrap_or([0.0, 0.0, 0.0]);
        if std::env::var_os("WE_DEBUG_LIGHTS").is_some() {
            eprintln!(
                "[灯] 场景灯 {} 盏  lightconfig={:?}  ambient={:?}  skylight={:?}",
                light_srcs.len(),
                scene.general.light_config,
                light_ambient,
                light_skylight
            );
            for l in &light_srcs {
                eprintln!("[灯]   {l:?}");
            }
        }

        let mut r = SceneRenderer {
            white,
            gl,
            rts: Vec::new(),
            rt_logical: Vec::new(),
            scene_rt: None,
            clear_rgb: [0.0, 0.0, 0.0],
            alpha_off: std::collections::HashSet::new(),
            fuse_tpl: std::collections::HashMap::new(),
            fuse_skip: std::collections::HashSet::new(),
            cbm_passes: HashMap::new(),
            attach_passes: Vec::new(),
            cbm_copy_rt: None,
            rt_index: HashMap::new(),
            texs: Vec::new(),
            text_recs: HashMap::new(),
            prop_text,
            passes: Vec::new(),
            static_cache: None,
            particle_systems: Vec::new(),
            proj_mvp: proj.transposed().column_major(),
            effects_on: true,
            rt_report: Vec::new(),
            rt_bbox: Vec::new(),
            bbox_jobs: Vec::new(),
            pass_bbox: Vec::new(),
            text_cache: HashMap::new(),
            text_quads: HashMap::new(),
            text_count: 0,
            obj_order: HashMap::new(),
            cur_obj_idx: 0,
            cur_effect_input: Src::White,
            // 贴图上限：**默认关**（实测没有可测收益，见字段上的注释）
            tex_cap: match std::env::var("WE_TEX_CAP").ok().as_deref() {
                None | Some("off") | Some("0") => 0,
                Some("canvas") | Some("on") => w.max(h),
                Some(v) => v.parse().unwrap_or(0),
            },
            time: 0.0,
            t0: std::time::Instant::now(),
            time_base: 0.0,
            fixed_dt: None,
            wall_time: 0.0,
            mesh_time: None,
            frames: 0,
            title: project.title.clone(),
            scene_size: (scene_w, scene_h),
            cover: (cx0, cy0, cx1, cy1),
            layer_count: 0,
            rt_count: 0,
            pass_count: 0,
            particle_count: 0,
            skipped_passes: Vec::new(),
            // 链趟里**打包贴图**的 V 方向：从数据推导（不看 item 名字、不看注解 `mode`）。
            // 依据与数字
            // · 包内**没有任何字段直接写 V 方向**（已逐文件核过 shader 注解 / 材质 / scene 对象）；
            // · 真正决定方向的是「链趟四边形」相对「底图趟」的 V 约定差，而它随链跑在
            // 画布空间还是图层空间而变（图层空间那条路会给底图趟多翻一次）；
            // · 包内唯一能把两种正确取向分开的**声明字段**是对象的 `copybackground`
            // （Gaze=true ⇒ 必须翻；Ice Princess `рыцари` / B 的各层没有它 ⇒ 不能翻）。
            // `WE_FX_TEX_VFLIP=0|1|all|copybg` 可做 A/B（默认 `copybg` 规则）。
            fx_vflip: FxTexVFlip::from_env(),
            fx_slot_flip: false,
            cur_mesh_comp: None,
            cur_mesh_uvspace: false,
            cur_mesh_base_flip: false,
            cur_mask_warp_src: None,
            cur_mask_warp_mvp: None,
            cur_mask_warp_blend: gl_ctx::Blend::Opaque,
            cur_mask_warp_pool: Vec::new(),
            cur_mask_warp_base: None,
            mask_warp_groups: Vec::new(),
            mask_warp_passes: Vec::new(),
            mask_warp_pos: None,
            glow: GlowCfg::from_env(),
            glow_batch: None,
            glow_verts: Vec::new(),
            glow_passes: Vec::new(),
            glow_src: None,
            pending_present: None,
            fade_tex: None,
            fade: None,
            fade_layer: None,
            fade_test: None,
            // 鼠标输入（见字段注释）：默认 None（没输入）⇒ 走 `pointer_default`。
            // `WE_MOUSE=x,y` 在 build 之后设置（CLI/离线验证用），内核用 `set_mouse` 逐帧推。
            mouse: None,
            mouse_last: None,
            pointer_default: pointer_default_from_env(),
            pointer_on: std::env::var("WE_POINTER").map(|v| v != "0").unwrap_or(true),
            pointer_proj: std::collections::HashSet::new(),
            frame_scalars: Vec::new(),
            fx_scripts: Vec::new(),
            fx_frame_scalars: Vec::new(),
            parallax_disp: [0.0, 0.0],
            par_on,
            par,
            parallax_passes: HashMap::new(),
            time_last: 0.0,
            light_srcs,
            light_ambient,
            light_skylight,
        };
        r.build(&pkg, &pkg_bytes, &assets, &scene, proj, scene_w, scene_h)?;
        if r.passes.is_empty() {
            return Err("没有可渲染的图层（贴图可能是不支持的格式）".into());
        }
        r.pass_count = r.passes.len();
        r.rt_count = r.rts.len();
        r.particle_count = r.particle_systems.len();
        // 离线验证钩子：`WE_FADE_TEST=<png 路径>:<alpha>` ⇒ 每帧按固定 alpha 把这张图叠在画面上。
        // 用途只有一个：**逐像素证明过渡覆盖层的几何/朝向**（α=1 时输出应逐字节等于那张图；
        // α=0 时必须与不带该变量的输出逐字节相同）。不设它 ⇒ 什么也不做、零开销。
        if let Ok(spec) = std::env::var("WE_FADE_TEST") {
            let (p, a) = spec.rsplit_once(':').unwrap_or((spec.as_str(), "1.0"));
            let alpha: f32 = a.trim().parse().unwrap_or(1.0);
            match image::open(p) {
                Ok(img) => {
                    let rgba = img.to_rgba8();
                    let (tw, th) = (rgba.width(), rgba.height());
                    let mut px = rgba.into_raw();
                    for x in px.iter_mut().skip(3).step_by(4) {
                        *x = 255; // 覆盖层自己乘 g_Alpha，源图 alpha 不参与
                    }
                    match r.gl.make_texture_screen(&px, tw, th) {
                        Ok((t, w, h)) => {
                            r.fade_test = Some((t, w, h, alpha));
                            eprintln!("[过渡] WE_FADE_TEST 已加载 {p} {w}x{h} alpha={alpha}");
                        }
                        Err(e) => eprintln!("[过渡] WE_FADE_TEST 上传失败：{e}"),
                    }
                }
                Err(e) => eprintln!("[过渡] WE_FADE_TEST 读图失败：{e}"),
            }
        }
        // ── 离线鼠标输入：`WE_MOUSE=x,y`（屏幕归一化 0..1，**y 从屏幕顶往下**）────────────
        // 用途：① 离线（CLI）验收鼠标驱动的效果/视差，不需要真鼠标、不碰桌面；
        // ② 内核里也能当固定值复现（临时）。
        // 语义与内核控制面 `{"cmd":"mouse","x":..,"y":..}` **完全同一口径** ⇒ 离线验完就能上屏。
        if let Ok(spec) = std::env::var("WE_MOUSE") {
            match parse_mouse_spec(&spec) {
                Some((x, y)) => {
                    r.set_mouse(x, y);
                    eprintln!("[鼠标] WE_MOUSE 注入：x={x:.4} y={y:.4}");
                }
                None => eprintln!("[鼠标] WE_MOUSE 格式应为 `x,y`（屏幕归一化 0..1），忽略：{spec:?}"),
            }
        }
        // 指针兜底位：没有输入时 `g_PointerPosition` 取什么。
        // 默认 = 中心（官方语义，见 `pointer_default` 字段注释）；这一行**只在诊断时**打，
        // 但它是"开了 X-Ray 却什么也看不到"这条症状的**自证**：
        // 看到 `(0.000,0.000)` + `指针注入关` 就是旧口径。
        if std::env::var_os("WE_DEBUG_POINTER").is_some() {
            let d = r.pointer_default();
            eprintln!(
                "[指针] 无输入时 g_PointerPosition = ({:.3},{:.3})  注入={}  当前输入={:?}\n\
                 [指针]   · 默认 (0.5,0.5) = OWE `Scene.cppm:2872` 的取值（WE 的指针总是有位置）\n\
                 [指针]   · (0,0)（`WE_POINTER_DEFAULT=zero`）= 旧口径：xray 的透视区被推到贴图\n\
                 [指针]     角落 ⇒ 精灵 alpha≈0 ⇒ 整层零变化（= \"透明层不显示\"）",
                d[0],
                d[1],
                r.pointer_on,
                r.mouse
            );
        }
        // 绝不静默：本场景有几趟特效被跳过 + 每趟的原因，一律汇总打印（不是只在 debug 里）。
        if r.skipped_passes.is_empty() {
            eprintln!("[特效] ✅ 特效趟全部建成（共 {} 趟，0 趟被跳过）", r.pass_count);
        } else {
            eprintln!(
                "[特效] ⚠ 本场景有 {} 趟特效被跳过（共建成 {} 趟）——逐趟原因如下：",
                r.skipped_passes.len(),
                r.pass_count
            );
            for s in &r.skipped_passes {
                eprintln!("[特效]   ⏭ {s}");
            }
        }
        if std::env::var("WE_DEBUG_PASSES").is_ok() {
            let mut pairs: Vec<(usize, String)> = r
                .rt_index
                .iter()
                .map(|(k, v)| (*v, k.clone()))
                .collect();
            pairs.sort();
            let names: Vec<String> = pairs.iter().map(|(i, n)| format!("{i}={n}")).collect();
            eprintln!("[调试] RT {} 个：{:?}", r.rts.len(), names);
            for (i, p) in r.passes.iter().enumerate() {
                let tgt = match p.target {
                    Some(t) => format!("RT[{}]", t),
                    None => "屏幕".to_string(),
                };
                let us: Vec<String> = p
                    .units
                    .iter()
                    .map(|(u, s)| match s {
                        Src::Rt(t) => format!("{u}→RT[{t}]"),
                        Src::Tex(t) => format!("{u}→tex[{t}]"),
                        Src::White => format!("{u}→白"),
                    })
                    .collect();
                let sc: Vec<String> = p
                    .scalars
                    .iter()
                    .map(|(n, v)| format!("{n}={v:?}"))
                    .collect();
                eprintln!("[调试] 趟{i} 写{tgt} 读{us:?} 常量{sc:?}");
            }
        }
        // `WE_PERF_SCISSOR`：所有趟建完之后探一次"每层内容在链 RT 里占哪一块"。
        // 默认关 ⇒ 一行都不跑（`bbox_jobs` 空）。
        r.probe_content_boxes()?;
        r.static_cache = r.plan_static_cache();
        Ok(r)
    }

    /// 记一笔\"被跳过的特效趟\"（出处 + 原因），立刻打印并存入 `skipped_passes`（去重）。
    /// **铁律**：特效链上任何一条 `continue` 都必须先过这里 —— 静默跳过是本项目最贵的一类
    /// bug（用户看到\"效果没生效\"，日志里一个字都没有，只能靠逐字节 A/B 反推）。
    fn note_skipped(&mut self, what: &str, why: &str, pass: Option<usize>) {
        let msg = match pass {
            Some(i) => format!("{what}｜第 {i} 趟｜{why}"),
            None => format!("{what}｜{why}"),
        };
        if self.skipped_passes.iter().any(|s| s == &msg) {
            return;
        }
        eprintln!("[特效] ⏭ 跳过 {msg}");
        self.skipped_passes.push(msg);
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        &mut self,
        pkg: &we_scene::pkg::Pkg,
        pkg_bytes: &[u8],
        assets: &Path,
        scene: &we_scene::scene::SceneDoc,
        proj: Mat4,
        scene_w: f32,
        scene_h: f32,
    ) -> Result<(), String> {
        let (out_w, out_h) = (self.gl.w, self.gl.h);
        self.alpha_off.clear();
        self.fuse_tpl.clear();
        self.fuse_skip.clear();
        // K1：BLENDMODE 的"屏幕拷贝"趟表也每次 build 重算（与 alpha_off 同构）。
        self.cbm_passes.clear();
        // 挂接的逐帧更新表同理（见字段注释）：每次 build 重算，默认关时恒空。
        self.attach_passes.clear();
        // 「把指针反投影进图层纹理空间的趟」也是每次 build 重算（见 `pointer_proj` 字段注释）
        self.pointer_proj.clear();
        // 遮罩烘焙趟表每次 build 都要重算：`mask_warp_passes` / `mask_warp_groups` 里存的是
        // **趟下标**，而 `passes` 每次 build 都重建 ⇒ 不清就会留下指向旧序列的下标
        // （build 会跑两次：先是构造默认尺寸、再是真尺寸）。
        self.mask_warp_groups.clear();
        self.mask_warp_passes.clear();
        // 屏幕清屏色：按场景的 `general.clearcolor`（见字段注释）。解析失败/缺省 ⇒ 黑。
        // 只影响**屏幕**（默认帧缓冲）：中间 RT 仍旧清成透明黑（`CFBO.cpp:61` 就是这么做的）。
        // `WE_CLEAR_RGB=r,g,b`（0..1）可覆盖 —— 调试用，也是"旧行为（恒黑）"的 A/B 口：
        // 设 `WE_CLEAR_RGB=0,0,0` 就逐字节复现改动之前。
        self.clear_rgb = std::env::var("WE_CLEAR_RGB")
            .ok()
            .and_then(|s| {
                let v: Vec<f32> = s.split(',').filter_map(|x| x.trim().parse().ok()).collect();
                (v.len() == 3).then(|| [v[0], v[1], v[2]])
            })
            .or_else(|| {
                we_scene::scene::parse_vec3(&scene.general.clear_color)
                    .map(|v| [v[0], v[1], v[2]])
            })
            .unwrap_or([0.0, 0.0, 0.0]);
        // present 趟是"延迟执行"还是留在 `passes` 末尾，由本次 build 重新决定
        self.pending_present = None;
        // ── 场景级合成 RT（照 linux-wallpaperengine `CWallpaper::setupFramebuffers`）─────
        // `_rt_FullFrameBuffer` 是**壁纸级、常驻、画布尺寸**的一张 RT，**它就是场景的渲染目标**：
        // 所有图层先合成进它，最后再一次性贴到屏幕。`_rt_imageLayerComposite_-1_*` 是
        // "全部图层的总合成"的别名（id = -1，它源码里 combine 趟就 bind 这个）。默认关
        // **默认开**（`WE_SCENE_RT=0` 可关做 A/B）。
        // ⚠ `build` 会被调用**两次**：第一次 `gl.w/h` 还是构造时的默认值（实测 1280x1），
        // 第二次才是真尺寸。若在第一次就把 RT 建好并按名字登记，第二次会"名字已存在"直接
        // 复用 ⇒ 整条后处理链从此在 1 像素高的"场景"上工作（实测：Ice Princess 人物被糊掉）。
        // ⇒ 所以这里**按尺寸校验**：尺寸不符就重建并覆盖登记（顺带自愈 resize）。
        if std::env::var("WE_SCENE_RT").ok().as_deref() != Some("0") {
            let (ew, eh) = (out_w.max(1), out_h.max(1));
            let stale = match self.scene_rt.and_then(|i| self.rts.get(i)) {
                Some(rt) => rt.w != ew || rt.h != eh,
                None => true,
            };
            if stale {
                let sr = self.new_rt(ew, eh, "_rt_FullFrameBuffer")?;
                for a in [
                    "_rt_MipMappedFrameBuffer",
                    "_rt_imageLayerComposite_-1_a",
                    "_rt_imageLayerComposite_-1_b",
                ] {
                    self.rt_index.insert(a.to_string(), sr);
                }
                self.scene_rt = Some(sr);
            }
        }
        // 全屏四边形（特效趟用）：**纯 NDC 全屏**（单位阵）。
        // 顶点数据本身就是 [-1,1]（见 gl_ctx 里的 6 顶点 VAO）⇒ 单位阵恰好铺满当前目标、
        // UV 0..1 覆盖整张输入。这里**绝对不能**乘场景投影：那样四边形会变成"整块场景画布"
        // （Sona 是 4000×2736，屏幕只有 2560×1440），比屏幕大得多 ⇒ UV 0..1 只落在屏幕中间
        // 一小块 ⇒ **每一趟都放大一点，逐趟累积**，最后渲成贴脸特写。
        // 想复现旧行为做 A/B：WE_EFFECT_QUAD=scene
        let full_q = if std::env::var("WE_EFFECT_QUAD").as_deref() == Ok("scene") {
            proj.mul(&Mat4::translate([scene_w / 2.0, scene_h / 2.0, 0.0]))
                .mul(&Mat4::scale(scene_w / 2.0, scene_h / 2.0, 1.0))
                .transposed()
                .column_major()
        } else {
            Mat4::identity().column_major()
        };

        let dbg = std::env::var("WE_DEBUG_LAYERS").is_ok();
        // 特效 `visible` 的处理档位（`visible=false` 的特效在 WE 里不参与合成）：
        // * **默认 `all`（改）**：完全对齐 WE —— **普通布尔 `visible=false` 也跳**。
        // 为什么改默认：`2887537617`（礼服斯卡蒂）的右腿丝袜由 `底层2` 的
        // **`effects/blend`（`visible: false`，采样 `底层2s`）** 画的 —— 旧默认照画它
        // ⇒ **默认（两个参数都关）时右腿丝袜就已经显示**；用户把 `newproperty` 打开后
        // `blendgradient`（同一个 `底层2s`）再画一遍 ⇒ **两层叠加**。
        // 这正是用户报的 ②，与 WE 客户端截图（默认两条腿都不该有丝袜）矛盾。
        // * `WE_EFFECT_VISIBLE=bound`：**旧默认**（只处理用户属性绑定）—— 用来 A/B 复现旧输出。
        // * `WE_EFFECT_VISIBLE=0`（或 `off`）：两种都不跳（更老的行为）。
        // ⚠ 代价（已实测重定基线）：`2861113973` 有一个普通 `visible=false` 的
        // `волосики 3` waterwaves，跳掉它会让旧评分从 7.5470 → 7.5500（Δ0.003 = 噪声级）；
        // 逐像素影响见本次报告，17 条冒烟无新失败。
        let effect_visible_mode: u8 = match std::env::var("WE_EFFECT_VISIBLE").ok().as_deref() {
            Some("0") | Some("off") => 0,
            Some("bound") => 1,
            _ => 2,
        };

        // ── 谁被别人引用了？`_rt_imageLayerComposite_<层id>_<a|b>` = **另一个图层的合成输出** ──
        // WE 里这叫"图层链接"；参考实现 OWE 的对应物是
        // `SceneMaterialTextureSourceKind::LayerOutput` + `AppendLayerCompositePassthroughEffect`
        // （见 `open-wallpaper-engine/src/Scene/Scene/Scene.cpp` 的 `EnsureLinkRenderTarget`：
        // 给每个被引用的层建一张"链接 RT"，喂它的**最终输出**）。
        // 本机 17 条里 5 条用到：Sona 的 `blend`（`BLENDMODE=9`）、`localcontrast_combine`、
        // `COPYBG` 打底、文字层等。以前没实现 ⇒ 这些采样器拿到白/上一趟残留。
        let referenced: std::collections::HashSet<i64> = {
            let mut set = std::collections::HashSet::new();
            if let Some(j) = pkg
                .get(pkg_bytes, "scene.json")
                .and_then(|b| std::str::from_utf8(b).ok())
            {
                const TAG: &str = "_rt_imageLayerComposite_";
                let mut i = 0usize;
                while let Some(p) = j[i..].find(TAG) {
                    let s = i + p + TAG.len();
                    let digits: String = j[s..].chars().take_while(|c| c.is_ascii_digit()).collect();
                    if let Ok(v) = digits.parse::<i64>() {
                        set.insert(v);
                    }
                    i = s;
                }
            }
            set
        };
        if dbg && !referenced.is_empty() {
            let mut v: Vec<i64> = referenced.iter().copied().collect();
            v.sort_unstable();
            eprintln!("[场景] 有 {n} 个图层被别人用图层链接引用：{v:?}", n = v.len());
        }

        // ── 链接 RT 的**消费方参数**（`WE_LINK_RT_CONTENT=auto` 的推导输入）──────────
        // 对每张被引用的链接 RT，记下"谁在采样它、它的 `blendscale` 与消费层自己的 `obj.scale`"。
        // 全部来自 scene.json（`effects[].passes[].textures` + `constantshadervalues` + `scale`），
        // 没有任何按 id/名字的特判 —— 见 [`link_rt_content_scale`] 的推导。
        let link_consumer: std::collections::HashMap<i64, LinkConsumer> = {
            const TAG: &str = "_rt_imageLayerComposite_";
            let mut map = std::collections::HashMap::new();
            for o in &scene.objects {
                let Some(sc) = we_scene::scene::parse_vec3(&o.scale) else { continue };
                // `obj.size` 只有两个分量（"1804.00000 1344.00000"）⇒ 不能用 `parse_vec3`（要 3 个）。
                let sz: Vec<f32> = o
                    .size
                    .split_whitespace()
                    .filter_map(|t| t.parse::<f32>().ok())
                    .collect();
                if sz.len() < 2 {
                    continue;
                }
                for eff in &o.effects {
                    if !eff.is_visible() {
                        continue;
                    }
                    for pass in &eff.passes {
                        let num = |k: &str| -> Option<f32> {
                            pass.constant_shader_values.get(k).and_then(|v| match v {
                                serde_json::Value::Number(n) => n.as_f64().map(|x| x as f32),
                                _ => None,
                            })
                        };
                        let Some(bs) = num("blendscale") else { continue };
                        let off = pass
                            .constant_shader_values
                            .get("blendoffset")
                            .map(|v| {
                                let nums: Vec<f32> = match v {
                                    serde_json::Value::String(s) => s
                                        .split_whitespace()
                                        .filter_map(|t| t.parse::<f32>().ok())
                                        .collect(),
                                    serde_json::Value::Array(a) => a
                                        .iter()
                                        .filter_map(|x| x.as_f64().map(|f| f as f32))
                                        .collect(),
                                    _ => Vec::new(),
                                };
                                if nums.len() >= 2 {
                                    [nums[0], nums[1]]
                                } else {
                                    [0.0, 0.0]
                                }
                            })
                            .unwrap_or([0.0, 0.0]);
                        for t in pass.textures.iter().flatten() {
                            let Some(rest) = t.strip_prefix(TAG) else { continue };
                            let digits: String =
                                rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                            if let Ok(id) = digits.parse::<i64>() {
                                map.entry(id).or_insert(LinkConsumer {
                                    blendscale: bs,
                                    scale_y: sc[1],
                                    offset: off,
                                    canvas: [sz[0], sz[1]],
                                });
                            }
                        }
                    }
                }
            }
            map
        };
        if dbg && !link_consumer.is_empty() {
            let mut v: Vec<(i64, LinkConsumer)> = link_consumer.iter().map(|(k, x)| (*k, *x)).collect();
            v.sort_by_key(|x| x.0);
            for (id, c) in v {
                eprintln!(
                    "[场景] 链接 RT 消费方 id={id}：blendscale={} scale_y={} offset={:?} 画布={:?}",
                    c.blendscale, c.scale_y, c.offset, c.canvas
                );
            }
        }

        // 二分定位用：名字含任一子串的图层整个跳过（逗号分隔，如 `WE_SKIP_NAME=Lens,Butterfly`）
        // 只渲染名字含任一子串的图层（与 WE_SKIP_NAME 相反）——"单独量一层"用（bbox/几何校准）
        // 链接离屏（`WE_LINK_OFFSCREEN=1`，**默认关**：关时逐字走旧路径）：
        // 被图层链接引用的层**即使不可见也要渲**（它的合成 RT 是别人的遮罩/底图），但**不上屏**。
        // 语义依据：OWE `EnsureLinkRenderTarget` —— 给每个被引用的层建"链接 RT"、喂它的**最终输出**；
        // 屏上显隐是另一回事（作者把这类层设 `visible:false`，正是"只当遮罩用"）。
        // 实测（`3704273480` 主题2）：`背景角色`(id 1583, visible=false) 被文字层的
        // `effects/blend:OPACITYMASK` 引用 ⇒ 我们跳掉它 ⇒ 遮罩 RT 空/串 ⇒ 那块蓝字变成
        // **不透明板块**（正确的样子是"透过蓝字看到角色"）。RT 自查：60 张 RT 里没有一张是
        // "角色单独、周围透明" ⇒ 它确实一次都没被画过。
        // 默认**开**（与"源码默认行为 = 屏上所见的对齐行为"一致）；`=0`/`false`/`off`/`no` 关回旧路径。
        // 翻转依据：15/17 场景逐字节不变；变化的 2 条都是**去掉画面瑕疵**——
        // ① `2482408967` 不再漏出 "02:13" 数字（图层趟泄漏）、② `3704273480` 主题1 角色脸上的
        // 深色补丁消失、640x360 下主题2 的白色大板块消失。主题2@2560x1440 的基线哈希
        // `f5422c05417a9ad401777dfd2d7883f7` 在**关**时逐字节命中（默认路径零影响已验证）。
        // 链接 RT 朝向（`WE_LINK_RT_FLIP`，**默认关**；`=1` 开）。
        // 第二次复核：
        // 上次靠肉眼看 RT 里的"冲浪板文字可正读"判成"开 = 正立"，**错了**。可复现的数值判据
        // （`scripts/we_orient_probe.py`，同一特征在链接 RT 与前景 1016 上的 Y 相对位置）：
        // · 开：RT 里人物正立，但 `blend.vert` 的取样（`v_TexCoord.zw` 的 y 方向与场景 y 相反）
        // 会把它再翻一次 ⇒ **字里的人物倒立**（用户原话："腿在上面"）；
        // · 关：RT 里人物倒立，取样再翻一次 ⇒ **字里的人物正立**（墨镜在眼睛上方）。
        // ⇒ 关 = 屏上所见正确。理由/数字。
        let link_rt_flip = std::env::var("WE_LINK_RT_FLIP")
            .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"))
            .unwrap_or(false);

        let link_offscreen = std::env::var("WE_LINK_OFFSCREEN")
            .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"))
            .unwrap_or(true);

        let only_names: Vec<String> = std::env::var("WE_ONLY_NAME")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let skip_names: Vec<String> = std::env::var("WE_SKIP_NAME")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().to_string())
            .collect();

        // ── 对象的世界矩阵：沿 `parent` 链累乘 ──────────────────────────────
        // WE 的场景是**层级结构**（本机 294 个对象里 48 个有 `parent`，其中一条壁纸有 33 个），
        // 参考实现 OWE `SceneNode::GetLocalTrans` 的局部矩阵是
        // `T(origin) · Rz(angles.z) · Ry · Rx · S(scale)`，世界矩阵 = 父链累乘。
        // ⚠ 我们以前**完全没应用 `scale` 和 `angles`**（本机 211/294 个对象 scale≠1、81/294 个 angles≠0），
        // 也没管 `parent` ⇒ 这类图层的尺寸/角度/位置一直是错的。
        let by_id: std::collections::HashMap<i64, &we_scene::scene::Object> =
            scene.objects.iter().map(|o| (o.id, o)).collect();
        // 每个对象在列表里的序号（判断图层链接是不是"前向引用"）
        self.obj_order = scene
            .objects
            .iter()
            .enumerate()
            .map(|(i, o)| (o.id, i))
            .collect();
        // ── 对象 y 位置的映射（根因：场景整体上下镜像）────────────────────────
        // 参照实现 `CImage::setSize`（open-wallpaper-engine Objects/CImage.cpp:258-262）把 origin.y
        // **无条件翻一次**：
        // m_pos.y = sceneHeight/2 - m_pos.y; // 同理 m_pos.w
        // 即 WE 的 `origin.y` 与它内部的场景 y（进摄像机的那个）方向相反。我们一直
        // 直接把 `origin.y` 当世界 y 用 ⇒ **整个场景相对参照实现上下镜像**。
        // 为什么以前 5 条"零回归探测器"完全测不出来：那几条壁纸只有 1~2 个图层，
        // 且 origin.y 全部 = 画布高/2（正中心）⇒ 镜像是不动点，逐字节不变。
        // Ice Princess 有 28 层、25 层不居中 ⇒ 唯一能把这条暴露出来的场景
        // （实测：吊灯跑到屏幕下方、骑士团在顶部、光环在人物脚下）。
        // 开关：默认**开**；`WE_FLIP_Y=0` 回到旧的（镜像）行为做 A/B。
        let flip_y = std::env::var("WE_FLIP_Y").ok().as_deref() != Some("0");
        // A/B 总开关：`WE_LANDING=legacy` 一次性回到 之前的**全部落点口径**
        // （① 镜像对折叠后的和做；② 对齐偏移用裸 `size/2` 且 y 一律反号；③ 有父链网格的
        // `WE_MESH_DY` 默认 −197；④ 粒子路径"取 m13 再取消镜像"）。四条一起回 ⇒ 可与旧二进制
        // 逐场景对 md5，用来量本次影响面（单条细调仍可用 `WE_Y_MIRROR=fold` / `WE_MESH_DY=`）。
        let legacy_landing = std::env::var("WE_LANDING").ok().as_deref() == Some("legacy");
        // （3704273480 夏日影：椰子树/饮料机、人物与沙滩、球/螃蟹/沙全在错误一侧）
        // **那次 `scene_h - y` 镜像只该做在「链的根」上，不能做在折叠后的和上。**
        // 证据链（全部用官方录屏 `~/Downloads/22-34-13.mp4` 逐像素模板匹配量出来的，2560x1440）：
        // · 该场景 33 个图层的 `parent` 都是**同一个纯容器** id=202（`origin=(1920,1080)` = 画布中心，
        // 无 image/particle/font/sound/shape）⇒ 折叠后的和 = 1080 + 子 origin。
        // 旧代码对**和**镜像 ⇒ world_y = 2160 − (1080 + 子) = 1080 − 子；正确值是 1080 + 子
        // ⇒ 每个子对象都错了 `2·子 origin.y`（屏幕 2·子·2/3 px），随 origin 而变 ⇒
        // 「球」下移 1184px、「螃蟹」下移 1272px、「树」上移 760px、沙/土下移 1253px 全部命中。
        // · 单层链（根就是对象自己）镜像做在根上 = 与旧行为**逐位一致** ⇒ Ice Princess 等
        // 无父链场景的落点一个像素都不动（`WE_FLIP_Y=0` 仍可关掉整条镜像做 A/B）。
        // · 根不是容器、叶子是粒子那条（IP 的 `хало`→碎屑）保持旧口径 ⇒ 2861113973 逐字节不变。
        let world_of = |obj: &we_scene::scene::Object, legacy_anchor: bool| -> WorldInfo {
            let mut chain: Vec<&we_scene::scene::Object> = vec![obj];
            let mut cur = obj;
            for _ in 0..32 {
                let Some(pid) = cur.parent else { break };
                let Some(p) = by_id.get(&pid) else { break };
                if std::ptr::eq(*p, obj) || chain.iter().any(|c| std::ptr::eq(*c, *p)) {
                    break; // 防环
                }
                chain.push(p);
                cur = p;
            }
            let mut m = Mat4::identity();
            // 实验 1（默认关，`WE_PARENT_REL_ORIGIN=1`）：**父链里的子对象 origin 是否相对父**。
            // 动机：3704273480 的背景/云/山/草/树…共 33 个对象全是容器 id=202（origin=画布中心
            // 1920,1080）的子级，算出来的 world 平移 y≈3240~4188，而画布只有 0..2160 ⇒ 看着
            // 像"被父链整体推下去一个画布高"。**实测结论：不是这样** —— 打开这个开关后
            // 2887537617（无 parent）与 2861113973（评分 7.5470）都不变，但 3704273480 的
            // 人物**整个跑出画布**（只剩点点点和文字）⇒ 方向反了，真因未定案（见 docs）。
            let parent_rel_origin = std::env::var("WE_PARENT_REL_ORIGIN").ok().as_deref() == Some("1");
            // 实验 2（默认关，`WE_CONTAINER_NOOP=1`）：把"纯容器对象"（无 image/particle/font/
            // sound/shape 的分组锚点，如 id=202）当**不参与变换**。
            // **实测结论：也不是这样** —— 2887537617 逐字节不变，但 3704273480 的人物被推到
            // 左下角、背景依旧不出 ⇒ 一并记入"未定案"。
            let container_noop = std::env::var("WE_CONTAINER_NOOP").ok().as_deref() == Some("1");
            // 含容器的父链变换（默认 = `ref`）──────────────────────────────
            // 病征：`3704273480` 的 39 个对象里 **33 个的 `parent` 是同一个容器 id=202**
            // （`origin=(1920,1080)` = 画布中心），算出的 world 平移 y≈3240~4188，而画布只有
            // 0..2160 ⇒ 天空/海/云/山/草/树/背景**整块落在画布外**（`背景` ty=3240、
            // `背景测试`/`滚动` ty=3240、`山` 3601…）⇒ 渲染出来是黑底。
            // 真因：**上面那次"绕画布中心上下镜像"被逐层做了**（每一层各翻一次）。参照实现
            // `linux-wallpaperengine` 的 `CImage::resolveTransform`（CImage.cpp:111-168）是：
            // ① 沿父链把 **原始** origin/scale/angles 累加成一个 resolved 变换 ——
            // 子 origin 先按**父的 scale** 缩放、再按**父的 z 角度**旋转，然后**加到父 origin 上**；
            // `scale = Π 各级 scale`、`angle = Σ 各级 angle`；
            // ② 镜像**只做一次**，而且是在 `CImage::setSize`（CImage.cpp:258-262）里对
            // **最终 resolved origin** 做的那一次 `m_pos.y = sceneHeight/2 - m_pos.y`。
            // 旧实现把 ①② 混在一起（矩阵逐层乘 + 逐层翻）⇒ 有 N 层父链就翻了 N 次。
            // 实现上 `ref` 模式与旧模式在**无父链（chain 长度 1）时逐位等价**（所以 A 与
            // Ice Princess 的居中单层不受影响）；差别只在"翻几次"。
            // 开关：`WE_CHAIN_MODE=old` 回旧行为做 A/B。
            let chain_mode = std::env::var("WE_CHAIN_MODE").unwrap_or_default();
            // `root_is_container`（无 image/particle/font/sound/shape 的分组锚点，如 202/943）
            // 现在**只**供 `WE_CHAIN_MODE=old` 的逐层镜像口径使用；默认路径不再按拓扑选路。
            let root_is_container = chain.last().map(|r| {
                r.image.is_none()
                    && r.particle.is_none()
                    && r.font.is_none()
                    && r.sound.is_none()
                    && r.shape.is_none()
            }).unwrap_or(false);
            // 粒子叶（Ice Princess 的 83 → 913/1918）以前单独走折叠，现在也自动落在同一条路径：
            // 粒子发射点取 `raw_origin`（未镜像的 resolve 结果），所以本改动不移动 IP 的碎屑。
            // 镜像只做在**链的根**上（根是纯容器 ⇒ `scene_h − 1080 = 1080`，即对它是恒等），
            // 子对象的 origin 不再各自镜像一次。单层链（根=自己）行为与以前逐位一致。
            // `WE_Y_MIRROR=fold` 回到旧口径（对折叠后的**和**翻一次）做 A/B。
            let mirror_root_only = !legacy_landing
                && std::env::var("WE_Y_MIRROR").ok().as_deref() != Some("fold");
            // **统一父链**（DESIGN-AUDIT /）。以前只有「根是纯容器」
            // 或「叶子是粒子」的链走折叠 + 一次镜像，其余链走逐层矩阵 + 逐层镜像 —— 同一份
            // 数据换一个父级类型就换一套数学。现在**所有** chain.len>1 的链都走折叠：
            // ① 从根到叶把 origin 按父 scale 缩放、按父 z 角旋转后累加，scale 连乘、angle 相加；
            // ② 镜像只做一次，做在**根自己的 origin** 上（根是容器 ⇒ scene_h−1080 = 恒等；
            // 子对象的 origin 是父框架里的偏移，不再各翻一次）。
            // 显式 A/B 旋钮仍保留：`WE_CHAIN_MODE=old`（逐层矩阵 + 逐层镜像）、
            // `WE_Y_MIRROR=fold`（对折叠后的**和**翻一次）。`legacy_anchor`/`WE_LANDING=legacy`
            // 也仍走旧路。
            if chain_mode != "old" && chain.len() > 1 && !legacy_anchor && !legacy_landing {
                let p3 = |s: &str| we_scene::scene::parse_vec3(s).unwrap_or([0.0, 0.0, 0.0]);
                let root = *chain.last().unwrap();
                let mut o = p3(&root.origin);
                let mut sc = we_scene::scene::parse_vec3(&root.scale).unwrap_or([1.0, 1.0, 1.0]);
                let mut ang = we_scene::scene::parse_vec3(&root.angles).unwrap_or([0.0, 0.0, 0.0]);
                // 从"根的孩子"逐级折到叶（= 参照实现的 `for (int i = count-2; i >= 0; --i)`）
                for i in (0..chain.len() - 1).rev() {
                    let c = chain[i];
                    let co = p3(&c.origin);
                    let cs = we_scene::scene::parse_vec3(&c.scale).unwrap_or([1.0, 1.0, 1.0]);
                    let ca = p3(&c.angles);
                    // offset = rotateVec2(child.origin * parent.scale, parent.angle)
                    let (px, py) = (co[0] * sc[0], co[1] * sc[1]);
                    let (cz, sz) = (ang[2].cos(), ang[2].sin());
                    o = [o[0] + px * cz - py * sz, o[1] + px * sz + py * cz, o[2] + co[2] * sc[2]];
                    sc = [sc[0] * cs[0], sc[1] * cs[1], sc[2] * cs[2]];
                    ang = [ang[0] + ca[0], ang[1] + ca[1], ang[2] + ca[2]];
                }
                let raw_origin = o;
                if flip_y && mirror_root_only {
                    // 镜像做在**根自己的 origin** 上（容器根 = 画布中心 ⇒ 恒等），折叠出来的
                    // 子和保持原样。旧口径在这里对**和**翻 ⇒ 每个子对象多翻一次（= 2·子 origin.y 的错位）。
                    // 落地方式：只把"根那一段"从 `y` 换成 `scene_h − y`（= 加 `scene_h − 2·root_y`），
                    // 子那段增量 `o[1] − root_y` 一动不动。
                    let root_y = p3(&root.origin)[1];
                    o[1] += scene_h - 2.0 * root_y;
                    ang[2] = -ang[2];
                } else if flip_y {
                    // 根不是容器（= IP 的 `хало`→碎屑那条，实测基线就是"对和翻一次"）⇒ 保持旧口径。
                    o[1] = scene_h - o[1];
                    ang[2] = -ang[2];
                }
                let half_w = we_scene::scene::parse_vec2(&obj.size)
                    .unwrap_or([scene_w, scene_h])[0]
                    * sc[0].abs()
                    / 2.0;
                // `Auto` 的判据：这一层的四边形**完整盖住画布**（宽高都盖住）。
                // 只看几何（size × |scale| vs 正交投影尺寸），不看 id/名字/贴图。
                let sz = we_scene::scene::parse_vec2(&obj.size).unwrap_or([scene_w, scene_h]);
                let full_cover = sz[0] * sc[0].abs() >= scene_w && sz[1] * sc[1].abs() >= scene_h;
                let world_m = neg_scale_fix(
                    Mat4::translate(o)
                        .mul(&Mat4::rotate_z(ang[2]))
                        .mul(&Mat4::rotate_y(ang[1]))
                        .mul(&Mat4::rotate_x(ang[0]))
                        .mul(&Mat4::scale(sc[0], sc[1], sc[2])),
                    sc,
                    p3(&root.origin)[0],
                    half_w,
                    full_cover,
                );
                return WorldInfo {
                    m: world_m,
                    raw_origin,
                    mirrored_here: !mirror_root_only,
                    scale: sc,
                };
            }
            // `WE_CHAIN_MODE=old` 的逐层镜像口径（与 之前逐位一致）
            let mirror_root_only_old = root_is_container
                && !legacy_landing
                && std::env::var("WE_Y_MIRROR").ok().as_deref() != Some("fold");
            let mirrored_here = legacy_landing || !(mirror_root_only_old && chain.len() > 1);
            let mut raw_m = Mat4::identity();
            let mut sc_chain = [1.0f32, 1.0, 1.0];
            for (li, o) in chain.iter().rev().enumerate() {
                let is_container = o.image.is_none()
                    && o.particle.is_none()
                    && o.font.is_none()
                    && o.sound.is_none()
                    && o.shape.is_none();
                if container_noop && is_container {
                    continue;
                }
                let mut t = we_scene::scene::parse_vec3(&o.origin).unwrap_or([0.0, 0.0, 0.0]);
                let s = we_scene::scene::parse_vec3(&o.scale).unwrap_or([1.0, 1.0, 1.0]);
                let mut a = we_scene::scene::parse_vec3(&o.angles).unwrap_or([0.0, 0.0, 0.0]);
                // 未镜像的那份（粒子路径要的 raw resolve 结果）：同一套矩阵，只是不做那次 `scene_h − y`
                raw_m = raw_m.mul(
                    &Mat4::translate(t)
                        .mul(&Mat4::rotate_z(a[2]))
                        .mul(&Mat4::rotate_y(a[1]))
                        .mul(&Mat4::rotate_x(a[0]))
                        .mul(&Mat4::scale(s[0], s[1], s[2])),
                );
                sc_chain = [sc_chain[0] * s[0], sc_chain[1] * s[1], sc_chain[2] * s[2]];
                // 镜像只做在链的根上（`li == 0` 就是链的根，见 `chain.iter.rev`）
                // 调试旋钮 `WE_PARENT_REL_ORIGIN=1`：子 origin 当"相对父"试过（-t），保持可复现
                t[1] = if parent_rel_origin && flip_y && li > 0 {
                    -t[1]
                } else {
                    chain_level_origin_y(t[1], li == 0, mirror_root_only_old, flip_y, scene_h)
                };
                if flip_y && !(mirror_root_only_old && li > 0) {
                    // 绕 z 轴的旋转在上下镜像下反号（只对该层自己被镜像时）
                    a[2] = -a[2];
                }
                let local = Mat4::translate(t)
                    .mul(&Mat4::rotate_z(a[2]))
                    .mul(&Mat4::rotate_y(a[1]))
                    .mul(&Mat4::rotate_x(a[0]))
                    .mul(&Mat4::scale(s[0], s[1], s[2]));
                m = m.mul(&local);
            }
            // 单层链（根 = 自己）：「父坐标系」= 场景 ⇒ 原点 x = 0（画布左上角）
            let sz = we_scene::scene::parse_vec2(&obj.size).unwrap_or([scene_w, scene_h]);
            let half_w = sz[0] * sc_chain[0].abs() / 2.0;
            let full_cover =
                sz[0] * sc_chain[0].abs() >= scene_w && sz[1] * sc_chain[1].abs() >= scene_h;
            WorldInfo {
                m: neg_scale_fix(m, sc_chain, 0.0, half_w, full_cover),
                raw_origin: [raw_m.m[12], raw_m.m[13], raw_m.m[14]],
                mirrored_here,
                scale: sc_chain,
            }
        };

        // ── 挂骨骼（`attachment`）锚点缓存（`WE_ATTACHMENT=1` 才填，默认关）──────────
        // 键 = **父对象 id**；值 = 父 puppet 的 (MDAT 挂接表, 姿态)。懒填充：只有真出现
        // 带 `attachment` 且父对象有 `puppet` 的对象时才解析，且每个父对象只解析一次。
        // 依据见 `attachment_on` 的文档注释（OWE `SceneFinalize.cpp:358-390`）。
        let mut attach_cache: std::collections::HashMap<
            i64,
            Option<(Vec<mdl::Attachment>, std::sync::Arc<mdl::PuppetPose>)>,
        > = std::collections::HashMap::new();
        // 包内优先、官方 assets 兜底 —— 与循环里的 `read_json` 同一口径。
        let read_bytes = |rel: &str| -> Option<Vec<u8>> {
            if let Some(b) = pkg.get(pkg_bytes, rel) {
                return Some(b.to_vec());
            }
            std::fs::read(assets.join(rel)).ok()
        };

        // **每个对象开始时的"趟数"** —— 粒子要按对象次序插进趟序列里
        // （见 `ParticleInstance::insert_before`）。`obj_boundary[k]` = 排在对象 k 前面的对象
        // 一共 push 了多少趟；粒子对象自己也在 `objects` 里，所以查它那一格就是它的插入点。
        let mut obj_boundary: Vec<usize> = vec![0; scene.objects.len()];

        for (idx, obj) in scene.objects.iter().enumerate() {
            // 记边界必须在**任何 `continue` 之前**（跳过的对象不产生趟，边界自然沿用上一个值）。
            if idx < obj_boundary.len() {
                obj_boundary[idx] = self.passes.len();
            }
            // 模型级 `cropoffset`（贴图像素）—— 在下面读到模型时填，MVP 那步用它做绘制偏移。
            let mut crop_offset: [f32; 2] = [0.0, 0.0];
            // 模型 json **有没有写 `cropoffset`**（与 `crop_offset` 的值无关：
            // 作者也可能写 "0 0"）。`WE_YFIX_PLAIN` 的判据要用它 —— 见 `yfix_applies_for_layer`。
            let mut model_crop_declared = false;
            // 模型层（A 路线）：网格与"要不要用它"也提升到对象级 —— 特效链的每一趟都要用同一份几何。
            let mut puppet_mesh_h: Option<mdl::PuppetMesh> = None;
            let mut puppet_on_h = false;
            // 这一层的底图趟是不是"平铺图集四边形"（`WE_UVSPACE_ATLAS`）
            // `uvspace_atlas_on`）。决定缓冲尺寸（= 贴图像素）与底图 MVP（单位阵）。
            let mut atlas_base_h = false;
            // 这一层的网格时间动画数据（有网格 + 有动画 + `WE_MESH_ANIM != 0` 时才有）。
            let mut mesh_anim_h: Option<MeshAnim> = None;
            // 网格部件的 y 落点常量 / y 朝向：**只在这里读一次**，基础层、包围盒取景框、
            // 特效链各趟共用同一份（以前散在几处各读一次，容易互相漂移，）。
            // ⚠ 晚（"网格/精灵落点一致性"轮）：**默认改为 0**。
            // 原默认 −197 是对着 Windows 截图扫出来的"网格像素"常量；真相是它补偿的是
            // **图层空间那条路丢了一次 y 镜像**（见 `mesh_layer_space` 的注释）—— 那个偏移
            // 随对象 origin 而变（= 2·(scene_h/2 − world_y)），只对 origin≈980 的人物躯干成立，
            // 对吊灯（origin_y 1380~1834）差 600~800px。修好"网格不再强制走图层空间"之后，
            // dy 的标定值回到 **0**（实测：`люстра`/`хало`/`плащ` 的网格版与平面版逐件同位）。
            // `WE_MESH_DY` 仍可覆盖（配合 `WE_MESH_LAYER=1` 复现旧行为）。
            // 晚（补）：**有父链的网格对象**（`obj.parent.is_some`，例如
            // 3704273480 的 `身体---拆分`，parent=202 容器）不走上面那次 y 修正
            // （见 `mesh_ws_here` 的收窄条件）⇒ 当时仍吃旧的常量补偿 −197。
            // 那条 −197 补偿的是**父链落点那次多做的镜像**（见 `world_of` 的长注释）。
            // 现在镜像已经收窄成"只做在链的根上" ⇒ `身体---拆分` 的 world y 由 2394 回到 1925
            // （屏幕下移 313px），常量补偿也就该回 **0**（与无父链对象同一个默认值）。
            // 实测（2560x1440 录屏帧模板匹配）：人物膝盖/脚与沙面的接触点于是落到与原版同一像素带。
            // `WE_MESH_DY` 仍可覆盖（做 A/B / 万一某条场景要回旧常量）。
            let mesh_dy: f32 = std::env::var("WE_MESH_DY")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(if legacy_landing && obj.parent.is_some() {
                    -197.0 // 旧口径
                } else {
                    0.0
                });
            let mesh_flip_up = std::env::var("WE_MESH_Y").ok().as_deref() == Some("up");
            self.cur_obj_idx = idx;
            // 每个对象重置"当前 mesh 收尾几何"（只有 mesh 对象在下面填）
            self.cur_mesh_comp = None;
            self.cur_mesh_uvspace = false;
            self.cur_mask_warp_src = None;
            self.cur_mask_warp_mvp = None;
            self.cur_mask_warp_pool.clear();
            self.cur_mask_warp_base = None;
            self.cur_mesh_base_flip = false;
            // 调试旋钮 `WE_SKIP_ID=<id,id>`：按 **对象 id** 跳过（`WE_SKIP_NAME` 只能按名字，
            // 而 WE 场景里有**名字为空**的对象 —— 例如 `3704273480` 的那个文字层
            // （`id=691`，`name=""`）⇒ 无法用名字二分，必须能按 id 跳）。
            let skip_ids: Vec<i64> = std::env::var("WE_SKIP_ID")
                .unwrap_or_default()
                .split(',')
                .filter_map(|s| s.trim().parse::<i64>().ok())
                .collect();
            if skip_ids.contains(&obj.id) {
                if dbg {
                    eprintln!("[图层] ⏭ {}：被 WE_SKIP_ID 跳过（id={}）", obj.name, obj.id);
                }
                continue;
            }
            // 反向旋钮 `WE_ONLY_ID=<id,id>`：**只**渲染这些 id（与 `WE_SKIP_ID` 相反）。
            // 为什么需要它：`WE_SKIP_NAME`/`WE_ONLY_NAME` 只能按**名字**过滤，而 WE 场景里
            // 有**名字为空**的对象（如 `3704273480` 的文字层 `id=691`），且同一条场景里
            // 大量对象重名（"可调整组合层"×4、"身体---拆分"×2）⇒ 想"单独量一层"必须按 id。
            let only_ids: Vec<i64> = std::env::var("WE_ONLY_ID")
                .unwrap_or_default()
                .split(',')
                .filter_map(|s| s.trim().parse::<i64>().ok())
                .collect();
            if !only_ids.is_empty() && !only_ids.contains(&obj.id) {
                if dbg {
                    eprintln!("[图层] ⏭ {}：不在 WE_ONLY_ID 里（id={}）", obj.name, obj.id);
                }
                continue;
            }
            if skip_names.iter().any(|s| obj.name.contains(s.as_str()))
                || (!only_names.is_empty()
                    && !only_names.iter().any(|s| obj.name.contains(s.as_str())))
            {
                if dbg {
                    eprintln!("[图层] ⏭ {}：被 WE_SKIP_NAME 跳过", obj.name);
                }
                continue;
            }
            // 隐藏的对象不该画（`visible` 可能是 bool，也可能是绑定对象 `{user, value}` ——
            // 后者以前会让整条壁纸解析失败，见 we-scene 的 bool_bound）。
            // 判的是**树上的**可见性（`tree_visible`）：被隐藏**容器**的子对象也不该画
            // （否则容器一隐藏，它整套图形照样盖在别人上面 —— 3704273480 的“主题”就是这样失效的）。
            let vis = tree_visible(obj, &by_id);
            if !vis && !(link_offscreen && referenced.contains(&obj.id)) {
                if dbg {
                    let why = if obj.is_visible() { "父容器不可见" } else { "visible=false" };
                    eprintln!("[图层] ✗ {}：{why}", obj.name);
                }
                continue;
            }
            if !vis && dbg {
                eprintln!(
                    "[图层] ◐ {}（不可见，但被图层链接引用 ⇒ 只渲进 RT、不上屏）",
                    obj.name
                );
            }
            // ── 底图来源：shape 用"直接绘制四边形"；文字对象自己光栅化；图片对象走包里的材质 ──
            // 三者之后走**完全同一条路**（特效链、混合、图层链接都共用）。
            // `extra_scalars` 是 shape 的"首个特效趟常量"（含四角点与颜色），`is_shape` 用来把
            // 对象尺寸换成场景高（WE 的 shape 是"整场景高"的四边形，见 OWE `ParseShapeObj`）。
            // K1：`cbm_plan` = 对象级 `colorBlendMode` 1..30 的落地计划（**只有图片/模型层**
            // 那条分支会填它；shape/文字层的底图趟是我们自己的 shader、没有官方 `BLENDMODE`
            // 消费点 ⇒ 保持旧行为，并在下面出声说明）。元素 = (GL 混合, 画面槽, 拷贝 RT,
            // 拷贝源 RT（None = 窗口/当前目标）)。
            let mut cbm_plan: Option<(gl_ctx::Blend, u32, usize, Option<usize>)> = None;
            // `WE_BASE_CARDSPACE`：底图趟的**顶点 shader 源码**（只有"普通图像层的
            // 单位四边形那条路"会填它）。作用域（`layer_space` / `fit` / `puppet_on_h` …）在这里
            // 还没算出来 ⇒ 先把它存下来，等算完作用域再决定要不要**换成卡片网格重建**底图趟。
            // 开关**关**时一个字节都不动、也不 clone（零开销）。
            let mut base_plain_vs_fs: Option<(String, String)> = None;
            let (base_src, mut base_layer, base_dbg, base_blending, extra_scalars, is_shape, fullscreen_layer, compose_layer) =
                if obj.kind() == we_scene::scene::ObjectKind::Shape {
                    match self.build_shape_layer(obj, pkg, pkg_bytes, assets, scene_h) {
                        Ok((src, layer, scalars)) => (
                            src,
                            layer,
                            format!("shape({})", obj.shape.clone().unwrap_or_default()),
                            Some("additive".to_string()),
                            scalars,
                            true,
                            false,
                            false,
                        ),
                        Err(e) => {
                            if dbg {
                                eprintln!("[图层] ✗ {}：shape 构建失败（{e}）", obj.name);
                            }
                            continue;
                        }
                    }
                } else if obj.kind() == we_scene::scene::ObjectKind::Text {
                    match self.build_text_layer(obj, pkg, pkg_bytes, assets) {
                        Ok((idx, layer)) => (
                            Src::Tex(idx),
                            layer,
                            format!("文字({})", obj.font.clone().unwrap_or_default()),
                            None,
                            Vec::new(),
                            false,
                            false,
                            false,
                        ),
                        Err(e) => {
                            if dbg {
                                eprintln!("[图层] ✗ {}：文字层构建失败（{e}）", obj.name);
                            }
                            continue;
                        }
                    }
                } else {
                    let Some(image) = obj.image.as_deref() else {
                        if dbg {
                            eprintln!("[图层] ✗ {}：没有 image（{:?}）", obj.name, obj.kind());
                        }
                        continue;
                    };
                    let read_json = |rel: &str| -> Option<Vec<u8>> {
                        if let Some(b) = pkg.get(pkg_bytes, rel) {
                            return Some(b.to_vec());
                        }
                        std::fs::read(assets.join(rel)).ok()
                    };
                    // ⚠ WE 的**内置模型/材质**（`models/util/*.json`、`materials/util/*.json` …）**不在壁纸
                    // 包里**，只在官方 assets 目录里。以前这里只在 pkg 里找 ⇒ 内置层全部解析失败 ⇒ 其中最
                    // 要命的是 `models/util/fullscreenlayer.json`（"后处理层"，`fullscreen:true` +
                    // `passthrough:true`）—— 它是 blur 这类**整屏后处理特效的宿主**：被跳过后
                    // **整屏虚化从来不会发生**（实测 Ice Princess 日志：`✗ Слой постобработки：
                    // models/util/fullscreenlayer.json 读不出 material 字段`，而该对象正是挂
                    // `effects/blur/effect.json` 的那个）。
                    let model_json = read_json(image)
                        .and_then(|b| we_scene::json::from_slice::<serde_json::Value>(&b).ok());
                    // `fullscreen:true` = WE 的"全屏后处理层"（如 models/util/fullscreenlayer.json）：
                    // 它的输入是**到目前为止的场景合成图**，不是某张美术贴图。
                    let is_fullscreen = model_json
                        .as_ref()
                        .map(|v| v.get("fullscreen").and_then(|f| f.as_bool()).unwrap_or(false))
                        .unwrap_or(false);
                    // `cropoffset`（模型级，单位 = **贴图像素**）：这个模型相对对象原点的
                    // **绘制偏移**。实测 `2887537617`：带它的对象整块偏移（`B灯台` 偏 333 屏幕
                    // 像素，换算回贴图像素 ≈ 2×196 == cropoffset.y = -196），而不带它的
                    // （`B背景`）位置精确到 1 像素。以前完全没读这个字段。
                    // ⚠ 语义**尚未吃准**：实测只有 y 方向对得上（`B灯台` 294→645，应在 627），
                    // x 方向反而偏、且让 IP 的 MAD 从 7.5470 退到 15.87 ⇒ **默认关**，
                    // `WE_CROP=1` 才启用（继续用它的 A/B 数据反推真正语义）。
                    let crop_off_on = std::env::var("WE_CROP").ok().as_deref() != Some("0"); // 默认开（已定论）
                    crop_offset = model_json
                        .as_ref()
                        .and_then(|v| v.get("cropoffset").and_then(|c| c.as_str()))
                        .and_then(|s| {
                            let n: Vec<f32> = s
                                .split_whitespace()
                                .filter_map(|t| t.parse::<f32>().ok())
                                .collect();
                            (n.len() >= 2).then(|| [n[0], n[1]])
                        })
                        .unwrap_or([0.0, 0.0]);
                    // 记录「模型写没写 cropoffset」（与解析出的值无关，作者也可能写 "0 0"）。
                    model_crop_declared = model_json
                        .as_ref()
                        .map(|v| v.get("cropoffset").is_some())
                        .unwrap_or(false);
                    if !crop_off_on {
                        crop_offset = [0.0, 0.0];
                    }
                    let material_path = model_json
                        .as_ref()
                        .and_then(|v| v.get("material").and_then(|m| m.as_str()).map(str::to_string));
                    // 内置层语义（`models/util/*.json`，原文件在 WE 的 assets 里）：
                    // · `solidlayer.json` = {material:"materials/util/solidlayer.json", solidlayer:true}
                    // ⇒ `flat` shader（**无贴图**）+ `translucent`：用**对象自身的 color/alpha**
                    // 画一个纯色四边形（`flat.frag` 的 uniform 是 `vec3 g_Color` + `float g_Alpha`）。
                    // · `composelayer.json` = {material:"materials/util/composelayer.json", passthrough:true}
                    // ⇒ **透传/合成层**：材质采样 `_rt_FullFrameBuffer`（"到目前为止的场景合成图"），
                    // `composelayer.vert` 把 **a_TexCoord 当 NDC**（摆满整屏）、`composelayer.frag`
                    // 在 `v_ScreenCoord` 处采样合成图 ⇒ 它的语义是**整屏 1:1 透传**（"调整层"）。
                    // 我们以前把它的**底图几何**用对象自己的矩形 ⇒ 采样到的只是合成图的一小块，
                    // 再被拉伸铺满全屏 ⇒ `点点点` 变成"一层全屏点阵盖住背景"。
                    // · `projectlayer.json` = 同上 + `autosize` + `projectlayer`。
                    // 判据用**模型标志 + 材质路径**，不写死对象名。
                    let model_flag = |k: &str| {
                        model_json
                            .as_ref()
                            .map(|v| v.get(k).and_then(|f| f.as_bool()).unwrap_or(false))
                            .unwrap_or(false)
                    };
                    let is_solid = model_flag("solidlayer")
                        || material_path.as_deref() == Some("materials/util/solidlayer.json");
                    let is_compose = !is_fullscreen
                        && (model_flag("projectlayer")
                            || model_flag("passthrough")
                            || material_path.as_deref() == Some("materials/util/composelayer.json"));
                    // puppet（MDLV 骨骼网格）：M1 先**解析 + 核对**（真正接进渲染是下一步）。
                    // 没有 `puppet` 的图层仍是老路子（平面四边形）。
                    let puppet_mesh = model_json
                        .as_ref()
                        .and_then(|v| v.get("puppet").and_then(|p| p.as_str()))
                        .and_then(|p| {
                            let bytes = read_json(p)?;
                            // 顺带解骨架（M2）：打印骨数与"世界锚点链"（行向量约定，已按顺序复合）
                            if dbg {
                                match mdl::parse_skeleton(&bytes) {
                                    Ok(sk) => {
                                        let piv: Vec<String> = sk.bones
                                            .iter()
                                            .map(|b| format!("({:.0},{:.0})", b.pivot[0], b.pivot[1]))
                                            .collect();
                                        let props = sk.bones
                                            .iter()
                                            .filter(|b| b.props.is_some())
                                            .count();
                                        eprintln!(
                                            "[骨架] {} {} 骨数={} 带JSON={} 锚点链={}",
                                            obj.name,
                                            sk.version,
                                            sk.bones.len(),
                                            props,
                                            piv.join(" ")
                                        );
                                    }
                                    Err(e) => eprintln!("[骨架] {} ✗ {e}", obj.name),
                                }
                            }
                            // puppet 的**姿态来自动画**：`animationlayers` 里的层
                            // 决定装配落点。`additive:false` 的层**替换**参考姿态（OWE `genFrame`
                            // 的 `trans = trans*(1-blend)+frame*blend` 分支）；`additive:true` 的层
                            // 在参考姿态上**叠加**增量（`tou` 的 239「摇头」/204、B 身体层的全部）。
                            // 时间 `t` 在**每帧**重新采样（见 `draw_frame` 里的网格动画段），
                            // 所以这里是 `t = 0` 的建层姿态；`t = 0` 的输出与改前**逐位一致**。
                            // `WE_ANIM_POSE=0` 退回"只用 MDLS 参考姿态"的老行为做 A/B；
                            // `WE_MESH_ANIM=0` 只关"随时间动"（保持 t=0 静态姿态）。
                            let layers: Vec<mdl::PoseLayer> =
                                if std::env::var("WE_ANIM_POSE").ok().as_deref() == Some("0") {
                                    Vec::new()
                                } else {
                                    obj.animation_layers
                                        .iter()
                                        .filter(|l| anim_layer_kept(l))
                                        .map(|l| mdl::PoseLayer {
                                            id: l.animation,
                                            additive: l.is_additive(),
                                            // 诊断（默认 1.0 = 行为不变）：`WE_BLEND_SCALE` 统一缩放
                                            // 所有加法层的 blend，用来测「blend 的取值口径」对姿态的影响。
                                            blend: layer_blend(l) * blend_scale(),
                                            // `rate` 以前**完全没读** ⇒ 所有层按 1.0 播。
                                            // 依据：OWE `Puppet.cpp::prepared` 用 `rate` 驱动
                                            // `SceneAnimationPlayback`（位置 = t·rate）。
                                            rate: l.rate(),
                                            // `visible` 字段脚本里的 `setFrame(...)`
                                            // ⇒ 起始相位（win 的 80%/70%）。`WE_ANIM_STARTPHASE=0` 关。
                                            start: anim_start_of(obj.id, l.animation),
                                        })
                                        .collect()
                                };
                            match mdl::parse_with_layers(&bytes, &layers, 0.0) {
                                Ok(m) => {
                                    if dbg {
                                        let bmin = m.blend.iter().min().copied().unwrap_or(0);
                                        let bmax = m.blend.iter().max().copied().unwrap_or(0);
                                        eprintln!(
                                            "[骨骼] {} ✓ {} 顶点={} 三角={} 索引范围=[{bmin},{bmax}] 文件={p}",
                                            obj.name,
                                            m.version,
                                            m.vertex_count(),
                                            m.triangle_count()
                                        );
                                    }
                                    Some(m)
                                }
                                Err(e) => {
                                    eprintln!("[骨骼] {} ✗ 解析失败（{p}）：{e}", obj.name);
                                    None
                                }
                            }
                        });
                    let Some(material_path) = material_path else {
                        if dbg {
                            eprintln!("[图层] ✗ {}：{image} 读不出 material 字段", obj.name);
                        }
                        continue;
                    };
                    // （M1）网格已解析好，等渲染那步接上；先显式"用一下"避免未用告警。
                    let _ = &puppet_mesh;
                    let Some(mat_bytes) = read_json(&material_path) else {
                        if dbg {
                            eprintln!("[图层] ✗ {}：包里和 assets 里都没有 {material_path}", obj.name);
                        }
                        continue;
                    };
                    let Ok(mat) = we_scene::json::from_slice::<shader::MaterialFile>(&mat_bytes)
                    else {
                        if dbg {
                            eprintln!("[图层] ✗ {}：{material_path} 解析失败", obj.name);
                        }
                        continue;
                    };
                    let Some(base_pass) = mat.passes.first() else {
                        if dbg {
                            eprintln!("[图层] ✗ {}：材质没有 passes", obj.name);
                        }
                        continue;
                    };
                    // 材质第一趟**没有贴图**（`flat` / `solidlayer` 这种纯色材质）**不是错误**：
                    // 用内置白纹理当底图，颜色/透明度交给对象自身的 `color`/`alpha`（早已作为
                    // shader uniform 传入）。以前这里直接 continue ⇒ `纯色`/`横线`/`平整积雪`
                    // 这类元素整块消失（日志里一片"材质第一趟没有 textures"）。
                    let solid_base = base_pass.textures.is_empty() || is_solid;
                    let tex_name: &str = base_pass
                        .textures
                        .first()
                        .map(|s| s.as_str())
                        .unwrap_or("(纯色/白底)");
                    if solid_base && dbg {
                        eprintln!(
                            "[图层] ≈ {}：材质无贴图 ⇒ 纯色层（白底 + 对象 color/alpha）",
                            obj.name
                        );
                    }
                    // ── 全屏后处理层 / 名字本身就是 `_rt_*` 的底图 ────────────────────
                    // 它的输入 = "到目前为止的场景合成图"（`_rt_FullFrameBuffer`；blur/shine 这类
                    // 整屏后处理靠它）。我们没有场景级 RT ⇒ 现建一张画布尺寸的，把**当前屏幕内容**
                    // GPU 侧拷进去，再把这层的特效链输入指向它（不再要求它是一张"贴图"）。
                    // ⚠ 场景合成 RT 这条路**默认开**（`WE_SCENE_RT=0` 关）：结构已经跑通
                    // （日志能看到 `_rt_FullFrameBuffer` / `_rt_QuarterCompoBuffer*` 建出来、
                    // `effects/blur_downsample4` 真的执行），但 Ice Princess 上糊过头了：
                    // 1600x900 对参照帧 56.47 → 63.78（越糊越差），而 640x360 逐字节不变。
                    // 待查：**后处理层（对象 304）在对象序列里的位置** —— 若它排得靠前，
                    // `copy_screen_into_rt` 抓到的是还没画完的"半个场景"，糊的就是半成品。
                    let scene_rt_on = std::env::var("WE_SCENE_RT").ok().as_deref() != Some("0");
                    let base_src = if solid_base {
                        // 纯色材质没有输入贴图 ⇒ 直接白底（颜色由 color/alpha uniform 给）
                        Src::White
                    } else if scene_rt_on && (is_fullscreen || tex_name.starts_with("_rt_")) {
                        // 全屏后处理层 ⇒ 底图就是常驻的 `_rt_FullFrameBuffer`（场景合成图）；
                        // 名字本身就是 `_rt_*` 的 ⇒ 按名字取（场景 RT 已用别名注册过）。
                        let rt_name = if is_fullscreen {
                            "_rt_FullFrameBuffer".to_string()
                        } else {
                            tex_name.to_string()
                        };
                        let idx = match self.rt_index.get(&rt_name).copied() {
                            Some(i) => i,
                            None => self.new_rt(out_w.max(1), out_h.max(1), &rt_name)?,
                        };
                        if dbg {
                            eprintln!(
                                "[图层] ✓ {}：底图 = 场景合成图 {}（RT#{idx}，打印用 out={}x{}；RT 实尺寸={}x{}）⇒ 全屏后处理层",
                                obj.name, rt_name, out_w, out_h,
                                self.rts.get(idx).map(|r| r.w).unwrap_or(0),
                                self.rts.get(idx).map(|r| r.h).unwrap_or(0)
                            );
                        }
                        Src::Rt(idx)
                    } else {
                        let Some(tex_idx) = self.upload_pkg_tex(pkg, pkg_bytes, assets, tex_name) else {
                            // ⚠ 全屏后处理层（`fullscreen`/`_rt_*`）在 `WE_SCENE_RT=0` 时**只能**走到这里：
                            // 它的底图是"当前场景合成图"，没有场景 RT 就没有这张贴图 ⇒ 整层被跳过。
                            // 以前这是**静默**的 ⇒ `WE_SCENE_RT=0` 看着像"同一条路去掉一个 RT"的 A/B，
                            // 其实是**另一份画面**（少了全部整屏后处理）—— 拿它当 A/B 会得出错结论
                            // （的"沙发扶手"那轮就是这么被带偏的）。这里必须说出来。
                            let is_fs_rt = is_fullscreen || tex_name.starts_with("_rt_");
                            if is_fs_rt && !self.skipped_passes.iter().any(|s| s.starts_with("[无场景RT]")) {
                                eprintln!(
                                    "[特效] ⏭ [无场景RT] 整屏后处理层「{}」被跳过（底图 {tex_name} 需要场景合成 RT）⇒ 当前画面 ≠ 开了场景 RT 的那一份，不能当 A/B",
                                    obj.name
                                );
                                self.skipped_passes.push(format!("[无场景RT] {}", obj.name));
                            } else if dbg {
                                eprintln!("[图层] ✗ {}：贴图 {tex_name} 加载失败", obj.name);
                            }
                            continue;
                        };
                        Src::Tex(tex_idx)
                    };
                    if dbg {
                        eprintln!(
                            "[图层] ✓ {}：底图={tex_name}，shader={}，尺寸={} 原点={}，特效 {} 个",
                            obj.name,
                            base_pass.shader,
                            obj.size,
                            obj.origin,
                            obj.effects.len()
                        );
                    }
                    let v_raw =
                        shader::load_shader(pkg, pkg_bytes, assets, &base_pass.shader, "vert")?;
                    let f_raw =
                        shader::load_shader(pkg, pkg_bytes, assets, &base_pass.shader, "frag")?;
                    // ── K1（用户 拨正前提后的收窄范围）──────────────────────
                    // 官方 `BLENDMODE` 有**三条**来源：① shader 自带的 `// [COMBO]` 默认值、
                    // ② 场景 JSON 的 `objects[].effects[].passes[].combos`、③ **对象级
                    // `colorBlendMode`**。①② 早已接通（`shader.rs::parse_shader_meta` +
                    // `lib.rs` 的趟 combo 组装），**本块只补 ③**，一个字都不动 ①②。
                    // ③ 的官方语义（OWE `SceneMaterialBuilder.cpp:48-61 ApplyLayerColorBlend`）：
                    // `colorBlendMode` 1..30 ⇒ `material.combos["BLENDMODE"] = n`，
                    // **不动 GL 混合**（混合由 shader 内的 `ApplyBlending` 完成）；
                    // 31 ⇒ `blending = "additive"`（不塞 combo）；0 ⇒ 什么都不做。
                    // 落点 = **这一层的材质趟（底图趟）**，与官方一致（combo 塞在材质上）。
                    // 一手依据：`genericimage2.frag:56/162-166`、`genericimage4.frag:78/203-205`、
                    // `common_blending.h:172-271`（全表）、`common_composite.h:33`。
                    // 官方资产里 `colorBlendMode` 全是 0（42 处）⇒ "值→combo" 这一步**官方无样例**，
                    // 依据是第三方 OWE/LWE（两者在 `n>0 && n!=31` 上一致，见 `Blend::plan_of_object`）。
                    // 判据全部**数据驱动**（不看 id/名字）：场景字段 `colorBlendMode` + shader 自己
                    // 有没有 `BLENDMODE`/有没有声明"画面"槽（`shader::blend_screen_slot`）。
                    // 开关 **`WE_CBM_SHADER_BLEND`**（**默认开**＝官方语义；`=0` 逐字节回旧的「加法」路径）。
                    let cbm_on = flag_default_on("WE_CBM_SHADER_BLEND");
                    let cbm_slot = if cbm_on { shader::blend_screen_slot(&f_raw) } else { None };
                    let cbm_material = base_pass.blending.clone().or_else(|| {
                        // 与下面 `text_blend` 对**非文字**层的那一支完全同式（copy_background ⇒ translucent）
                        if obj.copy_background {
                            Some("translucent".to_string())
                        } else {
                            None
                        }
                    });
                    let (cbm_blend, cbm_combo) = gl_ctx::Blend::plan_of_object(
                        cbm_material.as_deref(),
                        obj.color_blend_mode,
                        cbm_on && cbm_slot.is_some(),
                    );
                    // 这一层要不要真的塞 combo：必须有一张"画面"槽可绑（否则塞了也没人消费）
                    let cbm_active = cbm_combo.is_some() && cbm_slot.is_some();
                    // -1：**层材质自己声明的** `passes[].combos`（官方 `VERSION=2` 就在层材质里
                    // 本机命中 10 条场景 / 66 个可见层；以前这一整条通道没接 ⇒ `#ifndef VERSION`
                    // 成立 ⇒ 走旧分支、丢掉 `g_Color4` 色偏）。开关 `WE_MAT_COMBOS=1`，默认关。
                    let mut base_combos: Vec<(String, i32)> = Vec::new();
                    if mat_combos_on() {
                        let n = merge_pass_combos(&mut base_combos, &base_pass.combos, &[&f_raw, &v_raw]);
                        if n > 0 && std::env::var_os("WE_DEBUG_LAYERS").is_some() {
                            eprintln!(
                                "[材质combo] 底图趟 {}（{}）：{} 条 ⇒ {:?}",
                                base_pass.shader, obj.name, n, base_combos
                            );
                        }
                    }
                    // 对象级 `colorBlendMode` 的 `BLENDMODE` **最后**写 ⇒ 压过材质里的同名项
                    // （官方 OWE 就是 `material.combos.insert(BLENDMODE, n)` —— 覆盖语义）。
                    if let (true, Some(n)) = (cbm_active, cbm_combo) {
                        match base_combos.iter_mut().find(|(k, _)| k == "BLENDMODE") {
                            Some(slot) => slot.1 = n,
                            None => base_combos.push(("BLENDMODE".to_string(), n)),
                        }
                    }
                    let (vs, fs) = shader::compose_stages_with(&v_raw, &f_raw, &base_combos);
                    if cbm_active {
                        let n = cbm_combo.unwrap_or(0);
                        let slot = cbm_slot.unwrap_or(0);
                        eprintln!(
                            "[场景] ✓ 「{}」colorBlendMode={} ⇒ `#define BLENDMODE {}`（shader 内 \
                             `ApplyBlending`，画面槽 = g_Texture{slot}），GL 混合 = 材质 blending={:?}",
                            obj.name, obj.color_blend_mode, n, cbm_blend
                        );
                        // "画面拷贝" RT：画这一层之前先把**当前场景合成图**拷一份（避免 feedback）。
                        let copy = self.cbm_screen_rt(out_w.max(1), out_h.max(1))?;
                        cbm_plan = Some((cbm_blend, slot, copy, self.scene_rt));
                    }
                    if let Ok(p) = std::env::var("WE_DUMP_SHADER") {
                        let _ = std::fs::write(format!("{p}.{}.base.vert", obj.id), &vs);
                        let _ = std::fs::write(format!("{p}.{}.base.frag", obj.id), &fs);
                    }
                    // 探针 `WE_DEBUG_UV=out`：把**底图层**的片元输出换成采样坐标（R=u, G=v），
                    // 于是"进片元的 V 到底是 0→1 还是 1→0"一眼可见（屏幕上方应当是 G 接近 1）。
                    // 方向类问题（整条壁纸倒置）用这个比任何推理都快。
                    let fs = if std::env::var("WE_DEBUG_UV").ok().as_deref() == Some("out") {
                        let mut out = fs.clone();
                        for pat in [
                            "gl_FragColor = color;",
                            "gl_FragColor = albedo;",
                            "gl_FragColor = sample;",
                        ] {
                            if out.contains(pat) {
                                out = out.replace(
                                    pat,
                                    "gl_FragColor = vec4(v_TexCoord.xy, 0.0, 1.0); // WE_DEBUG_UV=out",
                                );
                                break;
                            }
                        }
                        out
                    } else {
                        fs
                    };
                    // puppet（MDLV）：有网格就把**网格**当底图（M1 = 网格几何；M2 = 逐帧骨骼装配）。
                    // ⚠ 默认值以**下方代码为准**（`WE_PUPPET != "0"` ⇒ 默认**开**）。
                    // 顶点是模型自身坐标（±size/2 之内、y 向上）⇒ 归一化到 ±1 再复用现有 MVP
                    // （`obj_mvp` / 图层空间那套都是按 ±1 四边形推的）。
                    // 判定规则（不需要对象名清单）：模型 JSON 里有 `puppet` 字段、且网格能解析成
                    // MDLV ⇒ 该对象**有网格**，直接走网格路径。
                    // 为什么不再用"网格几何必须偏离四边形"当门槛（`mesh_geometry_dev > 0.15`）：
                    // bind 姿态下几何可能与四边形几乎等价，但**蒙皮装配**（MDLSv2 逐骨矩阵）才是
                    // 把它拼到身体上的那一步 —— 只看 bind 偏差的判据会漏掉这类 puppet，结果是
                    // 肩甲/手臂/链条散开浮空、与躯干断开。
                    // ⇒ 只要模型声明了 puppet 就走网格路径。`WE_MESH_MIN_DEV=<x>` 保留为可选的
                    // 收窄门槛（调试/兼容用）；`WE_PUPPET_NAME`（逗号分隔子串）可点名收窄；
                    // `WE_PUPPET=0` 整条关掉、`=1` 强制。
                    let puppet_on = puppet_mesh.is_some()
                        && std::env::var("WE_PUPPET").ok().as_deref() != Some("0")
                        && match std::env::var("WE_MESH_MIN_DEV")
                            .ok()
                            .and_then(|v| v.trim().parse::<f32>().ok())
                        {
                            Some(min) => puppet_mesh.as_ref().is_some_and(|m| {
                                let s = we_scene::scene::parse_vec2(&obj.size).unwrap_or([1.0, 1.0]);
                                mesh_geometry_dev(m, s) > min
                            }),
                            None => true,
                        }
                        && match std::env::var("WE_PUPPET_NAME") {
                            Ok(want) if !want.trim().is_empty() => want
                                .split(',')
                                .any(|k| !k.trim().is_empty() && obj.name.contains(k.trim())),
                            _ => true,
                        };
                    // 模型层：把网格与"是否启用"提升到对象级，供特效链每一趟复用。
                    // 只在真正要用时克隆（未命中网格的对象零开销）。
                    puppet_on_h = puppet_on;
                    if puppet_on {
                        puppet_mesh_h = puppet_mesh.clone();
                    }
                    let mut layer = match puppet_mesh.as_ref().filter(|_| puppet_on) {
                        Some(m) => {
                            let s = we_scene::scene::parse_vec2(&obj.size).unwrap_or([1.0, 1.0]);
                            let (mut hw, mut hh) = (
                                if s[0].abs() > 1e-3 { s[0] * 0.5 } else { 0.5 },
                                if s[1].abs() > 1e-3 { s[1] * 0.5 } else { 0.5 },
                            );
                            // 归一化尺度可切（查"关节空白/头偏大偏小"）：
                            // 默认按**对象尺寸/2**（= 贴图尺寸）。`WE_MESH_FIT=bind` 改成按
                            // **网格自身 bind 包围盒半尺寸** ⇒ 网格的图集布局正好铺满对象矩形
                            // ⇒ 网格版与平面贴图版**逐像素同位同尺寸**（平面版就是免费的落点参照）。
                            // 两者只在"网格没铺满贴图"的部件上不同（本机差距 1.06x/1.23x 量级）。
                            if std::env::var("WE_MESH_FIT").ok().as_deref() == Some("bind") {
                                let (mut mnx, mut mxx, mut mny, mut mxy) = (
                                    f32::INFINITY,
                                    f32::NEG_INFINITY,
                                    f32::INFINITY,
                                    f32::NEG_INFINITY,
                                );
                                for p in &m.positions {
                                    mnx = mnx.min(p[0]);
                                    mxx = mxx.max(p[0]);
                                    mny = mny.min(p[1]);
                                    mxy = mxy.max(p[1]);
                                }
                                if mxx > mnx {
                                    hw = (mxx - mnx) * 0.5;
                                }
                                if mxy > mny {
                                    hh = (mxy - mny) * 0.5;
                                }
                                if dbg {
                                    eprintln!(
                                        "[网格] {} bind 包围盒半尺寸 ({hw:.0},{hh:.0}) vs 对象半尺寸 ({:.0},{:.0})",
                                        obj.name,
                                        s[0] * 0.5,
                                        s[1] * 0.5
                                    );
                                }
                            }
                            let texgrid_clamp = chain_texgrid_clamp();
                            let pos: Vec<[f32; 3]> = m
                                .geometry()
                                .iter()
                                .map(|p| {
                                    // Y 翻转（A/B 定案）：网格坐标**默认不翻**。
                                    // 图层空间那条路自己会翻一次 V（见下方 `raw_position` 的注释），
                                    // 网格若再翻一次 ⇒ 部件整体上下颠倒（实测 `tou` 网格版的脸/发缕
                                    // 全跑到画面底部；关掉这层翻转后与平面版**逐件同位同向**）。
                                    // `WE_MESH_Y=up` 可回到翻转版做 A/B。
                                    // 落点常量（标定）：y 方向整体平移，单位 = 网格像素。
                                    // 实测 6 个部件里 5 个在 y 上偏同一常量（800x450 下 −42px），
                                    // `B沙发` 因几何居中恰好为 0 ⇒ 是常量而非随机错位。
                                    // 扫描（`WE_MESH_DY`，公式 dy/hh 加在 y 上）：
                                    // dy=0 → Δ−42.1px；dy=−276 → Δ+16.9px（斜率 ≈0.214px/单位）
                                    // ⇒ 内插 Δ=0 落在 **−197**（网格像素）。已设为默认。
                                    // 注：+276/+1000 反而更差（非单调）——dy 把部分顶点推出缓冲边界。
                                    // 归一化算式收敛到 [`mesh_norm_pos`]：逐帧动画重算
                                    // 用的是**同一个函数**，两处不可能漂移。
                                    mesh_norm_pos(p, hw, hh, mesh_dy, mesh_flip_up, texgrid_clamp)
                                    })
                                    .collect();
                            // （bug ② "丝袜半截 + 不跟腿动"）：链缓冲（RT）跑在
                            // **网格自己的 uv/图集空间**（展开几何），装配几何只在收尾合成趟画。
                            // 展开几何 = 文件里的 bind 顶点（实测 `pos == (u−0.5)·size`，残差 0.000）
                            // ⇒ 归一化后必然落在 ±1 内（不需要取景框），且与图层贴图/作者画的
                            // 混合贴图**同空间**。理由与实测数字见 `mesh_uvspace_on`。
                            let atlas_here = uvspace_atlas_on()
                                && mesh_uvspace_on()
                                && !obj.effects.is_empty()
                                && !referenced.contains(&obj.id)
                                // `WE_UVSPACE_SCOPE=narrow`（默认）：只给"链把**另一个
                                // 图层的画布**当输入"的层走（任一 pass 的纹理是 `_rt_*`）。
                                // 判据是链的**数据依赖**，不是名字/id；见 `uvspace_scope`。
                                && match uvspace_scope() {
                                    UvSpaceScope::Old => false,
                                    UvSpaceScope::All => true,
                                    UvSpaceScope::Narrow => obj.effects.iter().any(|e| {
                                        e.passes.iter().any(|p| {
                                            p.textures
                                                .iter()
                                                .flatten()
                                                .any(|t| t.starts_with("_rt_"))
                                        })
                                    }),
                                };
                            let mesh_uvspace = mesh_uvspace_on()
                                && !obj.effects.is_empty()
                                && !referenced.contains(&obj.id)
                                // `WE_UVSPACE_ATLAS` 下**底图趟画的是整张图集
                                // （不是网格）**，于是"bind 顶点是不是图集"这条门控**不再必要**：
                                // 网格 uv 本来就是图集坐标，装配后的顶点 × 图集 uv = 正确采样。
                                // 见 `uvspace_atlas_on`。
                                && (atlas_here
                                    // （修"人物散架/头乱/衣服没了"的回退）：再加一条
                                    // **数据门控** —— 只有"文件里的 bind 顶点就是展开图集"的模型
                                    // 才走 uv 空间（判据见 `mesh_atlas_residual`）。装配姿态的模型
                                    // （`3704273480` 的 `身体---拆分`，残差 0.746）走这条路会把
                                    // 每个部件的采样区整体错开 ⇒ 走旧口径（装配进 RT + 四边形收尾）。
                                    || mesh_is_atlas(mesh_atlas_residual(&m.positions, &m.uvs, s)));
                            if dbg {
                                eprintln!(
                                    "[网格] {} bind-图集残差={:.4} ⇒ 链空间={}",
                                    obj.name,
                                    mesh_atlas_residual(&m.positions, &m.uvs, s),
                                    if atlas_here {
                                        "uv 空间 + 图集四边形底图（WE_UVSPACE_ATLAS）"
                                    } else if mesh_uvspace {
                                        "uv 展开空间（作者混合贴图同空间）"
                                    } else {
                                        "旧口径（装配进 RT + 四边形收尾）"
                                    }
                                );
                                // uv 覆盖范围（= 该网格实际用到贴图的哪一块）。图集四边形底图
                                // 把**整张**贴图放进缓冲，收尾趟按 uv 采样 ⇒ 只有当 uv 覆盖整张画布时
                                // "整张画布 1:1" 才与作者的画布同义（见 `uvspace_scope`）。
                                let (mut u0, mut v0, mut u1, mut v1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
                                for uv in &m.uvs {
                                    u0 = u0.min(uv[0]);
                                    v0 = v0.min(uv[1]);
                                    u1 = u1.max(uv[0]);
                                    v1 = v1.max(uv[1]);
                                }
                                eprintln!(
                                    "[网格]    uv 范围 u[{u0:.3},{u1:.3}] v[{v0:.3},{v1:.3}] 覆盖 {:.1}%x{:.1}%",
                                    (u1 - u0) * 100.0,
                                    (v1 - v0) * 100.0
                                );
                            }
                            let pos_uv: Vec<[f32; 3]> = if mesh_uvspace {
                                m.positions
                                    .iter()
                                    .map(|p| mesh_norm_pos(p, hw, hh, 0.0, mesh_flip_up, false))
                                    .collect()
                            } else {
                                pos.clone()
                            };
                                    // 诊断：**bind 姿态是不是\"打散的图集\"**。
                                    // 判据（skill/puppet-mesh-mdlv.md）：`pos == ((u−0.5)·sizeX, −(v−0.5)·sizeY)`。
                                    // 成立 ⇒ 直接画原始几何 == 画整张图集（平面路径与它等价，网格路径**必需**）；
                                    // 不成立 ⇒ 该模型的 bind 已经是装配好的姿态，网格路径不是必需品。
                                    if std::env::var("WE_MESH_ATLAS_DEBUG").is_ok() {
                                        let (sx2, sy2) = (s[0], s[1]);
                                        let mut maxr = 0.0f32;
                                        for (i, p) in m.positions.iter().enumerate() {
                                            if let Some(uv) = m.uvs.get(i) {
                                                let ex = (uv[0] - 0.5) * sx2;
                                                let ey = -(uv[1] - 0.5) * sy2;
                                                maxr = maxr.max((p[0] - ex).abs()).max((p[1] - ey).abs());
                                            }
                                        }
                                        let scale = sx2.abs().max(sy2.abs()).max(1.0);
                                        // 装配位移：bind 顶点 → 装配后顶点 的最大位移（归一化）。
                                        let asm = m.assembled.as_ref();
                                        let mut dmax = 0.0f32;
                                        if let Some(a) = asm {
                                            for (i, p) in m.positions.iter().enumerate() {
                                                if let Some(q) = a.get(i) {
                                                    let d = ((q[0] - p[0]).powi(2)
                                                        + (q[1] - p[1]).powi(2))
                                                    .sqrt();
                                                    dmax = dmax.max(d);
                                                }
                                            }
                                        }
                                        eprintln!(
                                            "[图集判据] {} size=({:.0},{:.0}) 最大残差={:.3} 归一化={:.4} 装配位移={:.3} ⇒ {}",
                                            obj.name, sx2, sy2, maxr, maxr / scale, dmax / scale,
                                            if maxr / scale < 0.02 { "ATLAS(需网格)" } else { "已装配(网格非必需)" }
                                        );
                                    }
                            // 诊断（腿缝排查）：装配后的归一化范围 vs 缓冲的 ±1。
                            // 任何 |y|>1 的顶点都会被图层缓冲（= 对象矩形）**裁掉** ⇒ 直线切边。
                            if std::env::var("WE_MESH_BBOX").is_ok() {
                                let (mut mnx, mut mxx, mut mny, mut mxy) =
                                    (f32::INFINITY, f32::NEG_INFINITY, f32::INFINITY, f32::NEG_INFINITY);
                                let mut outx = 0usize;
                                let mut outy = 0usize;
                                let mut ymin_v = [0.0f32; 2];
                                let mut ymax_v = [0.0f32; 2];
                                for q in &pos {
                                    mnx = mnx.min(q[0]);
                                    mxx = mxx.max(q[0]);
                                    if q[1] < mny {
                                        mny = q[1];
                                        ymin_v = [q[0], q[1]];
                                    }
                                    if q[1] > mxy {
                                        mxy = q[1];
                                        ymax_v = [q[0], q[1]];
                                    }
                                    if q[0].abs() > 1.0 {
                                        outx += 1;
                                    }
                                    if q[1].abs() > 1.0 {
                                        outy += 1;
                                    }
                                }
                                eprintln!(
                                    "[BBOX] {} 归一化 x[{:.4},{:.4}] y[{:.4},{:.4}] 越界: |x|>1 {} 个, |y|>1 {} 个 / {}；y 最小点 {:?} y 最大点 {:?}",
                                    obj.name, mnx, mxx, mny, mxy, outx, outy, pos.len(), ymin_v, ymax_v
                                );
                            }
                            if dbg {
                                eprintln!(
                                    "[骨骼] {} → 索引网格（顶点 {} 三角 {}，size {}×{}）",
                                    obj.name,
                                    m.vertex_count(),
                                    m.triangle_count(),
                                    s[0],
                                    s[1]
                                );
                            }
                            // 摆锤参数：`WE_SWAY=1` 开，`WE_SWAY_NAME` 选对象。
                            // ⚠ 默认值**必须是空**：以前默认写死 `люстра`，那是
                            // 标定时用的某张壁纸的对象名 —— 与 WE_PUPPET_NAME 同一类壁纸专用硬编码。
                            // 空默认 = 不指定就不摆（对 2887537617/3704273480 无影响，它们没有该名）。
                            // 振幅/频率/滞后可用环境变量调，便于对 Windows 版目视校准。
                            let sway_on = std::env::var_os("WE_SWAY").is_some() && {
                                let want = std::env::var("WE_SWAY_NAME").unwrap_or_default();
                                want.split(',')
                                    .any(|k| !k.trim().is_empty() && obj.name.contains(k.trim()))
                            };
                            let sway = if sway_on {
                                let (y_top, y_bot) = pos.iter().fold(
                                    (f32::MAX, f32::MIN),
                                    |(lo, hi), p| (lo.min(p[1]), hi.max(p[1])),
                                );
                                let env_num = |k: &str, d: f32| {
                                    std::env::var(k)
                                        .ok()
                                        .and_then(|v| v.parse::<f32>().ok())
                                        .unwrap_or(d)
                                };
                                // 默认 0.20 rad（≈11°）：实测 0.10 rad 在 1600 宽下只移 ~1.5px（肉眼无感），
                                // 0.20 时吊灯底部 ≈39px、顶部仍钉死 —— "轻微摆动"的可感量级。
                                let amp = env_num("WE_SWAY_AMP", 0.20); // 弧度
                                let hz = env_num("WE_SWAY_HZ", 0.35);
                                let lag = env_num("WE_SWAY_LAG", 1.2);
                                if dbg {
                                    eprintln!(
                                        "[摆锤] {} 振幅={amp}rad 频率={hz}Hz 滞后={lag} y_top={y_top:.3} span={:.3}",
                                        obj.name,
                                        y_bot - y_top
                                    );
                                }
                                [amp, hz, lag, y_top, y_bot - y_top]
                            } else {
                                [0.0; 5]
                            };
                            // 骨骼驱动（M2）：从同一个 .mdl 解出骨架的**世界锚点**，
                            // 归一化到网格空间后**追加到 uv 数组尾部**（形变函数从那里取；
                            // 多出来的尾巴对 `update_layer_mesh` 的 zip 无害）。
                            let mut uvs = m.uvs.clone();
                            // 贴图 uv 校准（针对"头没有脸"）：
                            // 实测脸的五官在 tou.tex 的 u≈0.22~0.29 / v≈0.50~0.56，
                            // 而网格自带 uv 是 u≈0.29~0.36 / v≈0.07~0.51 ⇒ 两者相邻但错开一个常量
                            // （疑为 WE 贴图 padding / crop 的 remap，我们没做）。
                            // 两个旋钮做扫描：`WE_MESH_UV_DU` / `WE_MESH_UV_DV`（单位=uv，可正可负）。
                            {
                                let du: f32 = std::env::var("WE_MESH_UV_DU")
                                    .ok()
                                    .and_then(|s| s.parse().ok())
                                    .unwrap_or(0.0);
                                let dv: f32 = std::env::var("WE_MESH_UV_DV")
                                    .ok()
                                    .and_then(|s| s.parse().ok())
                                    .unwrap_or(0.0);
                                if du != 0.0 || dv != 0.0 {
                                    for q in uvs.iter_mut() {
                                        q[0] += du;
                                        q[1] += dv;
                                    }
                                }
                            }
                            if let Some(pb) = model_json
                                .as_ref()
                                .and_then(|v| v.get("puppet").and_then(|q| q.as_str()))
                                .and_then(|p| read_json(p))
                            {
                                match mdl::parse_skeleton(&pb) {
                                    Ok(sk) => {
                                        for b in &sk.bones {
                                            uvs.push([b.pivot[0] / hw, -b.pivot[1] / hh]);
                                        }
                                        if dbg {
                                            eprintln!(
                                                "[骨骼驱动] {} 锚点 {} 个接入摆锤（共 {} 顶点）",
                                                obj.name,
                                                sk.bones.len(),
                                                m.vertex_count()
                                            );
                                        }
                                    }
                                    Err(e) => eprintln!("[骨骼驱动] {} ✗ {e}", obj.name),
                                }
                            }
                            if dbg {
                                eprintln!(
                                    "[网格图层] {} ✓ 真走网格：顶点 {} 索引 {} uv {} ⇒ make_indexed_layer",
                                    obj.name,
                                    pos.len(),
                                    m.indices.len(),
                                    uvs.len()
                                );
                            }
                            // 时间动画数据：建层用的 `pos` 是 `t = 0` 的装配几何；
                            // 这里把"重算所需的一切"交给 `MeshAnim`，`draw_frame` 每帧按 `t` 重算并重传。
                            // 开关 `WE_MESH_ANIM=0` 关掉"随时间动"（保持 t=0 的静态姿态）。
                            // 默认**开**：t=0 的输出与改前逐位一致（有 md5/评分证据），
                            // 而"让 puppet 动起来"正是本次目标；不需要这功能的场景不受影响。
                            if std::env::var("WE_MESH_ANIM").ok().as_deref() != Some("0") {
                                if let Some(pose) = m.pose.clone() {
                                    if dbg {
                                        eprintln!(
                                            "[网格动画] {} 动画层={:?} 顶点 {} ⇒ 每帧按 t 重算装配",
                                            obj.name,
                                            pose.layer_ids(),
                                            m.positions.len()
                                        );
                                    }
                                    mesh_anim_h = Some(MeshAnim {
                                        name: obj.name.clone(),
                                        pose,
                                        bind: m.positions.clone(),
                                        bi: m.blend_indices.clone(),
                                        bw: m.blend_weights.clone(),
                                        hw,
                                        hh,
                                        dy: mesh_dy,
                                        flip_up: mesh_flip_up,
                                        clamp01: texgrid_clamp,
                                        blend_scripts: blend_live_scripts(obj),
                                        // NaN ⇒ 第一帧一定算一次（与 t=0 相同的结果，无害）
                                        last_t: f32::NAN,
                                    });
                                }
                            }
                            let layer = if atlas_here {
                                // 底图趟 = **平铺图集四边形**（±1 单位四边形 + uv 0..1），
                                // 配下面 mvp 里的**单位阵** ⇒ 缓冲 = 作者画布（1 px = 1 贴图 px）。
                                // 材质 shader 照用（保住 color/alpha 语义）；几何不再走网格。
                                if dbg {
                                    eprintln!(
                                        "[网格] {} ⇒ 底图趟改用**平铺图集四边形**（缓冲 = 贴图 {}x{}，单位 MVP）",
                                        obj.name, s[0], s[1]
                                    );
                                }
                                self.gl.make_layer(&vs, &fs, None)?
                            } else {
                                // 这一层的遮罩要 bake 进帧空间 ⇒ 记下**同一份**网格几何
                                // （底图趟建层用的是 `pos_uv`/`uvs`/`indices`）。
                                if mask_atlas_mode() == 1
                                    && !obj.effects.is_empty()
                                    && obj.effects.iter().any(|e| {
                                        e.passes.iter().any(|p| {
                                            p.textures
                                                .iter()
                                                .flatten()
                                                .any(|t| t.starts_with("_rt_"))
                                        })
                                    })
                                {
                                    self.cur_mask_warp_src =
                                        Some((pos_uv.clone(), uvs.clone(), m.indices.clone()));
                                }
                                self.gl
                                    .make_indexed_layer(&vs, &fs, &pos_uv, &uvs, &m.indices, sway)?
                            };
                            if mesh_uvspace {
                                // 收尾合成趟要用**装配**几何（同一个 uv/索引）——它自己会被
                                // `mesh_anim` 每帧重算（摆进场景的是"活"的人物，而 RT 里是静态展开图）。
                                self.cur_mesh_uvspace = true;
                                atlas_base_h = atlas_here;
                                self.cur_mesh_comp = Some(MeshComp {
                                    pos: pos.clone(),
                                    uvs: uvs.clone(),
                                    indices: m.indices.clone(),
                                    sway,
                                });
                            }
                            layer
                        }
                        None => {
                            // 见 `base_plain_vs_fs` 的声明：只有开关开时才存（关时零开销、不动字节）。
                            if base_cardspace_on() {
                                base_plain_vs_fs = Some((vs.clone(), fs.clone()));
                            }
                            self.gl.make_layer(&vs, &fs, None)?
                        }
                    };
                    // 整屏"透传层"（`fullscreen:true` + material `passthrough`）的**底图趟**：
                    // `passthrough.vert` 里没有 `TRANSFORM` 分支时走裸 `a_Position` ⇒ 我们那个
                    // "没有 MVP ⇒ 用 V 反向四边形" 的启发式会给它套上 V 反向 —— 但它这一趟干的是
                    // **把场景合成图原样搬进自己的缓冲**，V 反向就变成"搬了一份上下翻转的副本"，
                    // 之后所有链上趟（也都被 WE_FX_QUAD_FLIP 翻过）回来时奇偶不对 ⇒
                    // **整个场景被上下翻转地叠回去**（实测把遮罩强制成 0、本该等价于不生效时，
                    // 画面是翻转的；光度剖面也判定"翻转"更接近 16.97 vs 正常 24.34）。
                    // ⇒ 透传层的底图趟必须**不翻**。`WE_FULLSCREEN_BASE=flip` 可回退做 A/B。
                    if is_fullscreen
                        && std::env::var("WE_FULLSCREEN_BASE").ok().as_deref() != Some("flip")
                    {
                        layer.raw_position = false;
                    }
                    if std::env::var_os("WE_DEBUG_UV").is_some() {
                        // 底图层的四边形有"V 正常 / V 反向"两套，按 `raw_position` 二选一
                        // （= 链接后 MVP uniform 是否为 inactive）。倒置类问题先看这一行。
                        eprintln!(
                            "[UV] {} 层：raw_position={}（{}）shader={}",
                            obj.name,
                            layer.raw_position,
                            if layer.raw_position {
                                "用 V 反向的那套四边形"
                            } else {
                                "用 V 正常的那套四边形"
                            },
                            base_pass.shader
                        );
                    }
                    (
                        base_src,
                        layer,
                        base_pass.shader.clone(),
                        base_pass.blending.clone(),
                        Vec::new(),
                        false,
                        is_fullscreen,
                        is_compose,
                    )
                };
            self.layer_count += 1;

            let mut origin = we_scene::scene::parse_vec3(&obj.origin).unwrap_or([0.0, 0.0, 0.0]);
            // origin 强制覆盖（查"腿上有拼缝"）：实测角色各部件共用
            // origin=(2013.53113, 980)，**唯独 `底层2` 是 (1992, 937)**（差 21.5/43 单位）。
            // 用它验证"接缝是否来自各部件按各自 origin 摆放"：
            // WE_FORCE_ORIGIN="2013.53113,980" WE_FORCE_ORIGIN_ON=底层2
            if let (Ok(v), Ok(names)) = (
                std::env::var("WE_FORCE_ORIGIN"),
                std::env::var("WE_FORCE_ORIGIN_ON"),
            ) {
                if names
                    .split(',')
                    .any(|k| !k.trim().is_empty() && obj.name.contains(k.trim()))
                {
                    if let Some(o) = we_scene::scene::parse_vec3(&v) {
                        origin = o;
                    }
                }
            }
            // `fullscreen: true` 的层（`models/util/fullscreenlayer.json` —— 整屏后处理层的宿主）：
            // 参照实现 `CImage.cpp:226-231` 把 **尺寸和原点一起改成"整张画布 + 画布中心"**：
            // if (model->fullscreen) { size = {scene_width, scene_height}; origin = {scene_w/2, scene_h/2, 0}; }
            // 我们只把 `size` 兜底成了画布（`size=None` ⇒ 场景尺寸），**origin 却回落成 (0,0,0)** ⇒
            // 加上 y 镜像后世界 y = scene_h - 0 = 2160 ⇒ 整个四边形掉到画布下缘之外 ⇒
            // 这一层只把"半张画面"搬了一遍再叠回去 ⇒ 用户看到的"殿堂/军队那块不对"。
            // 铁证：整层跳过 = 8.44；把遮罩强制成 0（效果上应当≈跳过）却是 18.69
            // ⇒ 差异不在"模糊多少"，在**搬错了位置**。
            if fullscreen_layer {
                origin = [scene_w / 2.0, scene_h / 2.0, 0.0];
            }
            let size = if is_shape {
                // shape 的四边形按"整场景高"缩放（OWE：`image.size = {edge, edge}`，edge = ortho_h）
                [scene_h, scene_h]
            } else {
                we_scene::scene::parse_vec2(&obj.size).unwrap_or([scene_w, scene_h])
            };
            // 世界矩阵（含父链、对象自己的 scale/angles）+ 四边形半尺寸缩放
            // 调试二分：`WE_MVP_OLD=1` 用旧的"只平移"公式（隔离新层级变换的影响）
            let (mut mvp_world, mut world_info) = if std::env::var_os("WE_MVP_OLD").is_some() {
                (
                    Mat4::translate(origin),
                    WorldInfo {
                        m: Mat4::translate(origin),
                        raw_origin: origin,
                        mirrored_here: true,
                        scale: [1.0, 1.0, 1.0],
                    },
                )
            } else {
                // 网格（puppet）图层锚定在**旧约定**上：那条路的 y 落点由**网格几何 +
                // 取景框 + `WE_MESH_DY`** 决定，而那个常量是对着 Windows 版在旧父链约定下
                // 标定出来的（实测：把网格也切到 ref 约定后，人物整体上移一个画布高、
                // 需要 `WE_MESH_DY≈1600` 才能补回来）⇒ 保持旧约定 = 人物位置逐位不变。
                // `WE_MESH_CHAIN=ref` 可让网格也走 ref（做 A/B / 将来重标定）。
                // `mesh_legacy` 这条路现在也吃"镜像只做在链的根上"（`world_of`
                // 内部按根是不是容器决定）—— 3704273480 的人物 `身体---拆分` 的祖先正是容器
                // 202 ⇒ 落点由 2394 回到 1925（屏幕下移 313px，见 `WE_MESH_DY` 的新默认 0）。
                let mesh_legacy = puppet_on_h
                    && std::env::var("WE_MESH_CHAIN").ok().as_deref() != Some("ref");
                let wi = world_of(obj, mesh_legacy);
                (wi.m, wi)
            };
            // ── 挂骨骼（`attachment`）：世界变换 = 父世界矩阵 · 锚点骨骼世界矩阵 · 自身原始局部变换 ──
            // 见 `attachment_on` 的文档注释（OWE `SceneFinalize.cpp:358-390`）。
            // 记下"骑的父世界矩阵"给下面的相机视差用（OWE 的 `ride_parent_parallax`）。
            let mut attach_ride_parent: Option<Mat4> = None;
            // 建层时烘一次的那些数据（供下面把"摆放趟"登记成**逐帧**更新，见 `AttachAnim`）。
            let mut attach_hit: Option<AttachHit> = None;
            // `Q_L = proj·pre`（与 `t` 无关的左因子；算 `obj_mvp_m` 的那段填，见 `AttachAnim`）。
            let mut attach_q_l: Option<Mat4> = None;
            if attachment_on() {
                if let (Some(aname), Some(pid)) = (obj.attachment.as_deref(), obj.parent) {
                    let entry = attach_cache.entry(pid).or_insert_with(|| {
                        // 父对象必须自己声明了 `puppet`（OWE 的条件：`parent_ref->puppet.is_some`）
                        let pobj = by_id.get(&pid).copied()?;
                        let image = pobj.image.as_deref()?;
                        let mj: serde_json::Value =
                            we_scene::json::from_slice(&read_bytes(image)?).ok()?;
                        let ppath = mj.get("puppet").and_then(|p| p.as_str())?;
                        let bytes = read_bytes(ppath)?;
                        let mesh = mdl::parse_with_layers(&bytes, &pose_layers_of(pobj), 0.0).ok()?;
                        let pose = mesh.pose?;
                        let atts = mdl::parse_attachments(&bytes).ok()?;
                        if atts.is_empty() {
                            return None;
                        }
                        Some((atts, std::sync::Arc::new(pose)))
                    });
                    if let Some((atts, pose)) = entry {
                        if let Some(a) = mdl::PuppetPose::attachment_index(atts, aname) {
                            // 建层这里的锚点仍取 `t = 0`（与建网格姿态同一时刻）——它是**基准**：
                            // `draw_frame` 每帧用 `M(t) = D(t)·M0` 把它更新到当前场景时间的真值
                            // （OWE 逐帧 `SetLocalFrame(anchor(t)·local_base)`，见 `AttachAnim`）。
                            // `WE_ATTACH_BAKE=1` ⇒ 不登记逐帧更新 = 复现上次的"烘 t=0"口径。
                            if let Some(am) = pose.attachment_transform(a, 0.0) {
                                if let Some(pobj) = by_id.get(&pid).copied() {
                                    let wp = world_of(pobj, false).m;
                                    let w = wp
                                        .mul(&Mat4 { m: am })
                                        .mul(&local_raw_of(obj));
                                    if dbg {
                                        eprintln!(
                                            "[挂接] {} id={} attachment={:?} 父={} 骨={} 锚点T=({:.2},{:.2}) \
                                             ⇒ world T=({:.2},{:.2})（原 T=({:.2},{:.2})）",
                                            obj.name,
                                            obj.id,
                                            aname,
                                            pobj.name,
                                            a.bone_index,
                                            am[12],
                                            am[13],
                                            w.m[12],
                                            w.m[13],
                                            mvp_world.m[12],
                                            mvp_world.m[13]
                                        );
                                    }
                                    // ` = W_parent·anchor(0)`；它的逆是 `D(t)` 的一半（见 `AttachAnim`）。
                                    // 退化（不可逆）⇒ 不登记逐帧（保持建层值 = 旧行为），并如实报一句。
                                    let a0_inv = wp.mul(&Mat4 { m: am }).invert();
                                    if !attach_bake() {
                                        match a0_inv {
                                            Some(a0_inv) => {
                                                attach_hit = Some(AttachHit {
                                                    w_parent: wp,
                                                    pose: pose.clone(),
                                                    att: a.clone(),
                                                    a0_inv,
                                                    m0: w,
                                                });
                                            }
                                            None => eprintln!(
                                                "[挂接] {} id={} 锚点基准矩阵不可逆 ⇒ 保持 t=0（不逐帧）",
                                                obj.name, obj.id
                                            ),
                                        }
                                    }
                                    mvp_world = w;
                                    world_info.m = w;
                                    world_info.raw_origin = [w.m[12], w.m[13], w.m[14]];
                                    attach_ride_parent = Some(wp);
                                }
                            }
                        }
                    }
                }
            }
            // 图层空间的 y 落点修正（晚，"网格/精灵落点一致性"轮，默认开）——
            // 图层空间那条路的 y 落点相对画布/平面路径**少了一次 y 镜像**
            // （实测同一对象放进图层空间：sprite y170 → 纹理空间 y955；网格版同理）。
            // 误差在场景坐标里 = Δy = 2·world_y − scene_h，即把落点关于画布中心镜像回正确的一侧
            // （world_y = world 矩阵的平移列 y；scene_h = 画布高）。
            // · 它是**纯平移**：几何、朝向、贴图 uv 全不动，只把对象搬回正确位置。
            // （直接把输出镜像会把网格内容一起翻过来 —— 实测 A 场景腿部朝天，已否掉。）
            // · 为什么以前看着"没事"：这个偏移随 origin 变化，而 `WE_MESH_DY=-197` 那个常量
            // 恰好把 origin≈980 的人物躯干（A/B、Ice Princess 的 девка）搬回正确位置，
            // 于是"只对一类对象成立的巧合"被当成了全局正确。
            // · 修正后实测（2000x1125，只渲 `люстра`）：网格 bbox 中心 y169 / 平面路径 y170；
            // `хало` y133 / y135；A/B 人物仍在原位；Ice Princess 参照分 **8.6237→8.0792**；
            // 与 Windows 官方截图 1.jpg 的头部/悬剑区块差 99→28。
            // `WE_MESH_YFIX=0` 关掉（回到出问题的旧落点）做 A/B。
            // 非网格对象的“对象矩形链”（开关 `WE_RECT_CHAIN=1`，默认关）：
            // 参照实现把每个图层的特效链渲进**它自己尺寸的 FBO**（`CImage.cpp:271-276`，
            // 尺寸 = scene.json 的 `size`），只有收尾趟摆进场景。于是凡按 UV 算位移/波长的
            // 特效，屏幕幅度 ∝ **对象自己的尺寸**（`waterwaves` 的 `strength²`、
            // `shake` 的 `g_Amp`、`foliagesway` 的 aspect…）；我们把链跑在画布尺寸的缓冲上
            // ⇒ 幅度 ∝ **画布尺寸**。实测（Ice Princess 的 `крестик*`，2000x1125）：
            // 画布空间 t=2s 位移 25.7px / t=4s 59.4px，图层空间只有 3.9px / 7.0px
            // ⇒ **幅度 8.6 倍**（用户报的“背后的吊坠整体在摆、披饰幅度过大”）。
            // 这条开关把“有特效链的非网格对象”也放进图层空间 = WE 的语义。
            // ⚠ 默认**关**：它会改变 A/B 与六张护栏分数（那些场景里各有 19~20 个这类
            // 对象），按本次的硬要求（B 逐字节不变）不能动默认路径。
            let rect_chain_on = crate::flag_default_on("WE_RECT_CHAIN")
                && puppet_mesh_h.is_none()
                && !is_shape
                && obj.kind() != we_scene::scene::ObjectKind::Text;
            // （实测：2887537617 礼服斯卡蒂「女孩身后的雕花黑皮沙发靠背/两侧翼整块不见」）：
            // y 修正是给**有特效链的层**准备的 —— 只有那种层的内容会「绕一圈」经过图层缓冲
            // （链内的趟是不带 MVP 的全屏四边形，采样 RT 时天然带一次 V 反向），画布/平面路径
            // 与它才差出那次 y 镜像；`Δy = scene_h − 2·world_y` 补的正是这一条。
            // **特效列表为空的层不绕圈**：底图趟写进图层缓冲、收尾趟再读回来，两趟的显式 V 反向
            // 互相抵消（见 `base_layer.raw_position` / `blit.raw_position` 两处）⇒ 对它是**凭空
            // 多出来的一次纯平移**。
            // 实测（2887537617，1920x1080，t=0，WE_RECT_CHAIN=1；平面路径 = `WE_PUPPET=0` /
            // `WE_RECT_CHAIN=0` 的落点，也就是唯一的几何真值）：
            // · 0 特效的层被推走：`B沙发` 靠背顶 y259（平面路径，= Windows ~/Downloads/4.jpg 的
            // 红线段）→ 加修正后 **504**（整块沉到人物身后 ⇒ 画面里只剩扶手，用户报的 bug）；
            // `B灯台` 429→854；`C桌子沙发` 直接出画布（footprint 空）。
            // · 有链的层不受影响（修正本来就对）：`B珊瑚`(1) 332=332、`C桌子`(2) 567=567，
            // 网格的 `头发1`/`底层1`/`底层2`/`底层4`/`tou` 也各自与自己的平面路径一致
            // ⇒ 丝袜跟腿动那条链（`底层2` 有 3 条特效）**不在这条改动的射程内**。
            // ⇒ 门控条件 = **该对象自己声明了特效**（对象自带的事实，不是壁纸 id / 对象名 / 全局开关）。
            // `WE_MESH_YFIX_ALL=1` 可恢复旧行为（对所有无父链层一律加修正）做 A/B。
            // （`3571777169` 背景腰部接缝）：见 `yfix_applies_for_layer` 的注释 ——
            // 「无特效」的层里还有一类**没有任何东西替它补那次镜像**：模型未声明 `cropoffset`
            // （= `WE_CROP_MODE=10` 的那条 `+crop_offset.y·scale·2` 对它是 0）、也没有
            // `alignment`/`locktransforms`/`solid`（落点完全由 `origin` 决定）。这条修正对
            // 它们同样成立。`WE_YFIX_PLAIN=0` 可关掉这一类做 A/B。
            // （`3571777169` `череп лево2`(413)「飘在画面中部的骷髅」）：
            // **`cropoffset` 不是这一类里的例外** —— 上面那条「有 cropoffset ⇒ 不用补镜像」的
            // 假设只对**带特效链**的层成立（那种层本来就在图层空间里绕圈，`WE_CROP_MODE=10`
            // 又把补偿关掉了）。对**无特效链**的层，`cropoffset` 的 `+crop_y·scale·2` 只是
            // 一个**碰巧数值接近**的同向位移，不是精确的镜像补偿 —— 实测本场景 413
            // （`череп лево2`，无特效、`cropoffset=-638.5 -469`、scale 1.45015）：它在
            // 两种口径下的差异 `|Δy| − |crop_y·scale·2| = 2214 − 1360 = 854 场景单位`
            // （≈570px @2560），于是它停在屏幕中部（隔离 bbox y802..974），而**同批**骷髅
            // 409/417/423（**有**特效链 ⇒ 拿 Δy）都在底部（y1182..1439）。
            // 判据（数据驱动，不看 id/名字）：无特效链 + 无 alignment/locktransforms/solid +
            // 非整屏/合成 + 修正幅度 ≥ 画布高 1% ⇒ 与「无 cropoffset」那类**同一条路**。
            // 与它配套：这条修正一旦生效，`WE_CROP_MODE=10` 那条 crop 位移就**不再叠加**
            // （见 `obj_mvp_m` 里 `yfix_here` 的门），否则同一件事补两次。
            // 回退开关 **`WE_YFIX_CROP_PLAIN=0`**（这一类回到「不补镜像」的旧行为）。
            let crop_plain_on =
                std::env::var("WE_YFIX_CROP_PLAIN").ok().as_deref() != Some("0");
            let plain_place = (!model_crop_declared || crop_plain_on)
                && obj.alignment.is_none()
                && obj.lock_transforms != Some(true)
                && !obj.solid
                && !fullscreen_layer
                && !compose_layer
                // 修正本身是"绕画布中心镜像"，对**居中**的层是恒等 ⇒ 幅度小于画布高的 1%
                // （本场景 21.6 单位 / ~14px）时它只是白白多一次重栅格化（本场景 `373 город`
                // 的 Δy = −6 单位，就是这类）。判据用画布高做尺度，不看场景/对象名。
                && (scene_h - 2.0 * world_info.m.m[13]).abs() >= scene_h * 0.01;
            let layer_has_chain = yfix_applies_for_layer(!obj.effects.is_empty(), plain_place);
            let mesh_ws_here = puppet_on_h
                && puppet_mesh_h.is_some()
                && std::env::var("WE_MESH_LAYER").ok().as_deref() != Some("0")
                // ⚠ 只对有父链**无**的对象做修正：实测 3704273480 的人物 `身体---拆分`
                // （`parent=202` 容器、resolved world y≈2394）旧路径落点本来就是对的，
                // 再镜像一次会把它推到画布外（人物整块消失）⇒ 有父链的对象保持旧行为。
                && obj.parent.is_none()
                && layer_has_chain;
            // 非网格的“对象矩形链”对象走的是**同一条图层空间路**（同样的 y 落点偏差）
            // ⇒ 用同一个补偿公式（`Δy = scene_h − 2·world_y`，数据来自世界矩阵平移列）。
            // 同样只对**有链**的层成立（见上面 `layer_has_chain` 的实测）。
            let yfix_here =
                mesh_ws_here || (rect_chain_on && obj.parent.is_none() && layer_has_chain);
            // 对齐偏移**先**算出来（纯计算，数值与原来一致）：下面那条
            // "修正后的落点还在不在画布内" 的判据要用它。
            let align_scale = if legacy_landing {
                [1.0, 1.0, 1.0] // 旧口径：裸 `size/2`
            } else {
                world_info.scale
            };
            let (align_dx, mut align_dy, dy_flipped) = {
                let (dx, mut dy) = alignment_offset(obj.alignment.as_deref(), size, align_scale);
                let flipped = flip_y && world_info.mirrored_here;
                if flipped {
                    dy = -dy;
                }
                (dx, dy, flipped)
            };
            let yfix_dy: f32 = if yfix_here
                && std::env::var("WE_MESH_YFIX").ok().as_deref() != Some("0")
            {
                let ty = mvp_world.m[13];
                let mul: f32 = std::env::var("WE_MESH_YFIX_MUL")
                    .ok()
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(1.0);
                mul * (scene_h - 2.0 * ty)
            } else {
                0.0
            };
            // 这条 y 落点修正**不能把图层推到画布之外去**。
            // 它是一条"绕画布中心镜像"的补偿（`Δy = scene_h − 2·world_y` ⇒ `y' = scene_h − y`）：
            // 对**居中**的层是恒等（这也是它一直没被发现的原因）。但对**矩形贴满整块画布、
            // 锚点落在画布角上**的层，它会把整层送到画布外 —— 实测的活样本就是崩坏3
            // `2564205376` 的 X-Ray 层（`size` = 画布 5476×3080、`origin` = (5476,3080)、
            // `alignment` = `topright`）：`WE_RECT_CHAIN=1`（**桌面内核正是这个环境**）下
            // 那个层链跑在图层空间 ⇒ 落点被翻到画布外 ⇒ **一开 X-Ray 整屏全黑**
            // （`WE_MESH_YFIX=0` 一关就正常；两种口径的差分只有鼠标附近 0.29% 像素）。
            // 判据：**修正后的矩形要和画布有交叠**。不交叠时这条修正只会把一个"本来可见"或
            // "本来就不可见"的层送得更远 ⇒ 一律不加（不交叠的层对画面没有贡献，所以跳过它是
            // 视觉无害的；而恰好把它从画布外翻进画布内那种"修正"，来源不明，不该靠猜）。
            let yfix_dy = {
                let half = 0.5 * size[1] * align_scale[1].abs();
                let center_after = mvp_world.m[13] + yfix_dy + align_dy;
                let (lo, hi) = (center_after - half, center_after + half);
                let overlaps = lo < scene_h && hi > 0.0;
                let reject = yfix_dy != 0.0 && !overlaps;
                // （修 `xrayglasses` 落点，崩坏3 2564205376）：
                // 走到"不交叠"这一支，说明**这条补偿把层推到画布外**了。以前的做法是
                // **什么都不做**（`0.0`）—— 结果是这一层停在**被全局 y 镜像之后**的位置上
                // （= 绕画布中心镜像过的落点）。对**居中**的层那是恒等（= 之前没露馅的原因），
                // 对**锚在画布角上、尺寸又不到整块画布**的层就整体镜像 ⇒ 落点错一个
                // `2·(origin_y − 画布中心)`。活样本：`Glasses`（id=240，size 1826x1103、
                // `origin` y=3080.7、`alignment`=topright、`image`=models/util/solidlayer.json）
                // 作者把它摆在**画布右上角（人脸处）**，我们画到**右下角**
                // （实测 640x360：变化区 bbox x[427,639] y[231,359] = 右下；作者位置应是
                // y[0,129]；`WE_FLIP_Y=0`（不做那次全局镜像）就正好落在 y[0,128] ✓）。
                // 现在这一支改成**把这一层放回作者原样的落点**：origin 不镜像（`+Δy`，
                // 因为 `Δy = 画布高 − 2·ty` 正好是那次镜像的逆）+ 对齐偏移不反号。
                // 判据同时要求**还原后的矩形真的和画布相交**，否则维持旧行为（不动）。
                // 对崩坏3的 X-Ray 层（居中、size=画布）这个还原是**恒等**（`+Δy` 与 `−align_dy`
                // 恰好抵消）⇒ 它逐像素不变（见 artifacts 的回归数字）。
                let center_authored = mvp_world.m[13] + yfix_dy - align_dy;
                let authored_ok = center_authored > -half && center_authored < scene_h + half;
                let restore = reject && authored_ok;
                if std::env::var_os("WE_DEBUG_YFIX").is_some() {
                    eprintln!(
                        "[yfix] {} id={} size_y={:.0} ty={:.0} Δy={:.0} align_dy={:.0} 中心 {:.0}→{:.0} 矩形[{:.0},{:.0}] 画布[0,{:.0}] ⇒ {}",
                        obj.name,
                        obj.id,
                        size[1],
                        mvp_world.m[13],
                        yfix_dy,
                        align_dy,
                        mvp_world.m[13] + align_dy,
                        center_after,
                        lo,
                        hi,
                        scene_h,
                        if restore {
                            "跳过后改用作者原样落点"
                        } else if reject {
                            "跳过后修正（不交叠）"
                        } else if yfix_dy != 0.0 {
                            "应用"
                        } else {
                            "无修正"
                        }
                    );
                }
                if restore {
                    // 对齐偏移也还原（只在它当初被反过号时才要还原）
                    if dy_flipped {
                        align_dy = -align_dy;
                    }
                    yfix_dy
                } else if reject {
                    0.0
                } else {
                    yfix_dy
                }
            };
            let mvp_world = if yfix_dy != 0.0 {
                Mat4::translate([0.0, yfix_dy, 0.0]).mul(&mvp_world)
            } else {
                mvp_world
            };
            // 对齐：`origin` 是锚点，`alignment` 说它落在对象矩形的哪个位置 ⇒ 补回"锚点→中心"的偏移。
            // 不补的话，非中心的层会整体偏半个尺寸（实测 `2564205376` 的 `topright` 层被画到画布
            // 右下角 ⇒ 屏幕上"只剩一角、别处黑"）。`WE_NO_ALIGN=1` 可关掉做 A/B。
            // 两处修正（对着 WE `updateScenePosition`，CImage.cpp:987-1015）：
            // ① 幅度用 **`scaledSize = size × 父链 scale 连乘`**，不是裸 `size`。
            // 3704273480 的 `球`（size 400x351, scale 1.687, align=bottom）实测：
            // 旧幅度 175.5 → 正确 296.1（= 351×1.687/2）；配合"镜像只做在根上"，
            // 预测整层下移 1775.5 场景px（= 屏幕 1184px），
            // 2560x1440 录屏帧模板匹配量到 **屏幕 1184px**（残差 dy=-2）⇒ 两项都验上了。
            // ② y 的符号只在**这一层自己的 origin 被镜像过**时才反（`world_info.mirrored_here`）。
            // 容器子对象的落点现在没被镜像 ⇒ 偏移不该反号（WE 是先算 quad、最后才镜像）。
            let mvp_world = if (align_dx != 0.0 || align_dy != 0.0)
                && std::env::var_os("WE_NO_ALIGN").is_none()
            {
                Mat4::translate([align_dx, align_dy, 0.0]).mul(&mvp_world)
            } else {
                mvp_world
            };
            // （夏日影 3704273480 主题2 巨型排版字 `id=691`「mouse pad」的 y 落点定案）
            // **只反文字层的世界 y**（其余层一律不动），因为**文字层的四边形相对其它所有层
            // 多了一次 y 镜像**。这是同一帧内自证的事实，不依赖任何跨轮测量：
            // · 同一个父亲（纯容器 `id=943`）的两个 `信封点`（`id=603/624`，世界 y = 1080±1084）
            // 在**全帧** skip-diff 里的 footprint 是 `y[0,24]` / `y[1416,1436]`
            // ⇒ 普通层满足 `屏幕 y = (画布高 − 世界 y)·2/3`（603 世界 2163 → −2 屏上=顶）；
            // · 同一父亲的**文字层** 691（世界 y = 1694）却落在 `屏幕 y = 世界 y·2/3`
            // （footprint `y[836,1436]`；`WE_TEXT_MARKER` 也给同一矩形 `[606,1652]`、字形正立）
            // ⇒ 只有文字层多翻了一次。
            // 为什么这是"改对"而不是"改错"：WE 的 `3-1.jpg`（主题2 官方帧）里字块在**上方**。
            // 用同帧的底部小字（`wenzi1/2` = `id 1143/1161`，世界 y = 201）标定参照帧的比例
            // （我们 1306 → 参照 ~1376 ⇒ ×1.0536）后，参照字块中心折回我们的坐标 ≈347，
            // 而我们当前 1155、镜像后 **336.5** ⇒ 只有镜像这一支与官方帧一致（差 11px）。
            // 等价说法：把文字层搬成"折后镜像"的世界 y（`WE_Y_MIRROR=fold` 对文字层的效果），
            // 与其它层用 `(画布高 − world_y)` 落到**同一个屏幕位置**——两条口径同一个像素。
            // 只动平移列、不动 scale/旋转 ⇒ 四边形的朝向与字形都不翻（实测文字仍正立）。
            // 开关 `WE_TEXT_Y_MIRROR=0` 回旧行为做 A/B。
            let mvp_world = if obj.kind() == we_scene::scene::ObjectKind::Text
                && std::env::var("WE_TEXT_Y_MIRROR").ok().as_deref() != Some("0")
            {
                let ty = mvp_world.m[13];
                let mut m = mvp_world;
                m.m[13] = scene_h - ty;
                m
            } else {
                mvp_world
            };
            if std::env::var_os("WE_DEBUG_MVP").is_some() {
                let t = Mat4::translate(origin);
                let same = (0..16).all(|i| (t.m[i] - mvp_world.m[i]).abs() < 1e-4);
                eprintln!(
                    "[MVP] {} id={} origin={:?} scale={:?} angles={:?} parent={:?} 与纯平移一致={same}\n      world={:?}",
                    obj.name,
                    obj.id,
                    origin,
                    we_scene::scene::parse_vec3(&obj.scale),
                    we_scene::scene::parse_vec3(&obj.angles),
                    obj.parent,
                    mvp_world.m
                );
            }
            // ── 相机视差：这一层的 `parallaxDepth`（只算一次，下面记到「放进场景那一趟」上）──
            // 规则见 `parallax::depth_*`（OWE 的 `LogicalParallaxState` + 三条语义）。
            // `k = depth ⊙ amount`；`base = (node − camera) ⊙ k`；每帧再加 `mouse ⊙ k`。
            // 缺 `disablepropagation` 字段（we-scene 没解析）⇒ 一律按 `false`（= 不阻断传播）。
            let par_info: Option<[f32; 4]> = if self.par_on {
                // 骑父视差（OWE `SceneFinalize.cpp:358-390` 的 `ride_parent_parallax_ids`）：
                // 挂了骨骼的子对象**不再有独立的视差**，它的位移跟父层走 ⇒ 深度链从**父**起算、
                // 位移基准点用**父的世界位置**。非挂接对象一字不动（默认关时这段根本不执行）。
                let par_root = match attach_ride_parent {
                    Some(_) => by_id.get(&obj.parent.unwrap_or(obj.id)).copied(),
                    None => Some(obj),
                };
                let mut chain_depths: Vec<(String, bool)> = vec![(
                    par_root.map(|o| o.parallax_depth.clone()).unwrap_or_default(),
                    par_root.and_then(|o| o.disable_propagation).unwrap_or(false),
                )];
                let mut cur = par_root.and_then(|o| o.parent);
                while let Some(pid) = cur {
                    let Some(po) = by_id.get(&pid) else { break };
                    chain_depths.push((po.parallax_depth.clone(), po.disable_propagation.unwrap_or(false)));
                    cur = po.parent;
                }
                let depth = parallax::depth_effective(
                    parallax::depth_from_chain(&chain_depths),
                    self.par.ortho_implicit,
                );
                let (k, cam) = (
                    [depth[0] * self.par.cfg.amount, depth[1] * self.par.cfg.amount],
                    self.par.camera,
                );
                let node = match attach_ride_parent {
                    Some(wp) => [wp.m[12], wp.m[13]],
                    None => [mvp_world.m[12], mvp_world.m[13]],
                };
                Some([
                    (node[0] - cam[0]) * k[0],
                    (node[1] - cam[1]) * k[1],
                    k[0],
                    k[1],
                ])
            } else {
                None
            };
            // shape 的 MVP（改）：**不再用单位阵**。
            // 依据（OWE `ParseShapeObj`，SceneImageObjectParser.cpp:1244-1252 +
            // `GenDirectDrawQuadMesh`，SceneGeometryBuilder.cpp:64-88）：shape 会被改写成
            // **ImageObject** —— 顶点 = `((px-0.5)·edge, (0.5-py)·edge, 0)`（对象空间）、
            // `size = (edge, edge)`、`edge = ortho_h`，UV 直接用 `(px, py)`；之后走的是
            // **和普通图片层完全一样**的 `proj · world · scale(size/2)`，**不是** NDC。
            // 我们的单位四边形坐标 `(2px-1, 1-2py)` 乘 `scale(size/2)` 恰好等于那个对象空间
            // 顶点 ⇒ 直接用同一个公式即可。
            // 实测（3704273480 的 `光束 - 角`，`WE_SHAPE_GEOM=1` 打印四角的 NDC）：
            // 单位阵：p0(-1.941,-1.700) p1(-1.223,+0.722) p2(+2.577,+0.759) p3(+1.042,-1.215)
            // 配置： p0(-1.212,-1.514) p1(-0.808,+0.908) p2(+1.330,+0.945) p3(+0.466,-1.030)
            // ⇒ x 方向差 1.778 倍（= 3840/2160，单位阵把点坐标同时当 x/y 的 NDC），
            // 再加世界平移 (1689.97, 1280.62) 的场景单位（屏幕 +1126 / −133 px）；
            // 四角屏幕位置最大差 **1900 px**。`WE_SHAPE_QUAD=0` 可回退旧行为做 A/B。
            let shape_mvp_on = std::env::var("WE_SHAPE_QUAD").ok().as_deref() != Some("0");
            // 图层空间镜像补偿（`Δy`）与 `cropoffset` 的绘制位移是同一件事的两种
            // 写法 ⇒ 默认**互斥**（见 `obj_mvp_m` 里那条门）。`WE_CROP_ON_YFIX=1` 回旧行为。
            let crop_on_yfix_off = std::env::var("WE_CROP_ON_YFIX").ok().as_deref() != Some("1");
            // LIGHTING 路径要用的「模型矩阵」= 场景摆放（不含投影）。
            // 官方 `genericimage4.vert:60/282/287` + `:415`/`:451` 在 `#if LIGHTING` 下把
            // 世界坐标与最终位置**拆成两条矩阵**：
            // `worldPos = mul(vec4(localPos,1.0), g_ModelMatrix);`
            // `gl_Position = mul(worldPos, g_ViewProjectionMatrix);`
            // 两者相乘 = 原来的 `g_ModelViewProjectionMatrix`；下面把同一套 `proj`/`pre`/`world`/
            // `scale(size/2)` 按**与 `obj_mvp_m` 完全相同的运算顺序**再算一份模型矩阵。
            // ⚠ 运算顺序必须一致（浮点不满足结合律，换个顺序就不是逐字节同值了）。
            let mut obj_model_only_m = Mat4::identity();
            // 与 `obj_model_only_m` 配对的「视图投影矩阵」（= `proj`；shape 关 MVP 那一支
            // 几何本来就画在 NDC 上 ⇒ 用单位阵）。
            let mut obj_view_proj_m = proj;
            let obj_mvp_m = if is_shape && !shape_mvp_on {
                obj_view_proj_m = Mat4::identity();
                Mat4::identity()
            } else if is_shape {
                let scale_m = Mat4::scale(size[0] / 2.0, size[1] / 2.0, 1.0);
                obj_model_only_m = mvp_world.mul(&scale_m);
                // 挂接层：`Q_L = proj`（这一支的摆放矩阵 = `proj · world · scale`，见 `AttachAnim`）
                if attach_hit.is_some() {
                    attach_q_l = Some(proj);
                }
                proj.mul(&mvp_world).mul(&scale_m)
            } else {
                // `cropoffset`：**模型级绘制偏移**（贴图像素）⇒ 乘对象 scale 换成场景单位后
                // 叠加到世界矩阵的右侧（作用于已缩放的四边形）。y 符号用 `WE_CROP_FLIP`
                // 做一次 A/B 定标（WE 贴图 y 与场景 y 的朝向要对一下）。
                // 注意矩阵作用顺序（右到左）：先 `scale(size/2)` 把 ±1 四边形摊成贴图像素，
                // 再 `world` 变换到场景，最后才用 `pre` 做**场景单位**的偏移 —— 把偏移放进
                // `world` 里侧会被 `size/2` 二次放大（实测：整层爆成满屏、IP 的 MAD 7.55→15.85）。
                let (pre, world) = if crop_offset == [0.0, 0.0]
                    || (yfix_here && crop_on_yfix_off)
                {
                    // **这条 crop 位移与图层空间的镜像补偿（`Δy`）是同一件事的
                    // 两种写法**（`WE_CROP_MODE=10` 的注释就是「无特效对象缺这个位移」）。
                    // 于是当上面那条 `Δy` 已经对本层生效（`yfix_here`）时，这里必须**不再叠加**，
                    // 否则同一件事补两次（实测 413 会从「底部」再往下 1360 场景单位 ⇒ 出画布）。
                    // 两个都关的层（画布空间 / 有特效链）行为不变。
                    // 回退开关 **`WE_CROP_ON_YFIX=1`**（恢复「两个都补」的旧行为做 A/B）。
                    (Mat4::identity(), mvp_world)
                } else {
                    let sc = we_scene::scene::parse_vec3(&obj.scale).unwrap_or([1.0, 1.0, 1.0]);
                    let sy = if std::env::var("WE_CROP_FLIP").is_ok() {
                        -1.0
                    } else {
                        1.0
                    };
                    // `WE_CROP_MODE`：几种"cropoffset 到底是什么"的候选解释，一次编译全带上，
                    // 用 IP（有参照帧 + 16/19 个模型带这个字段，当前基线 MAD 7.5470）扫一遍，
                    // 谁把 MAD 打下去谁就是对 —— 让数据决定，不靠猜。
                    // 1 = 贴图像素 × 对象 scale（场景单位） 2 = 贴图像素（不乘 scale）
                    // 3 = 贴图像素 × scale ÷ 2 4 = 贴图像素 ÷ 2
                    let mode: i32 = std::env::var("WE_CROP_MODE")
                        .ok()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(10); // 10 = 已定论规则（无特效对象、只 y、×2）
                    let (ox, oy) = match mode {
                        2 => (crop_offset[0], crop_offset[1]),
                        3 => (crop_offset[0] * sc[0] / 2.0, crop_offset[1] * sc[1] / 2.0),
                        4 => (crop_offset[0] / 2.0, crop_offset[1] / 2.0),
                        // 5 = 贴图像素 × scale × 2；6 = **只取 y 分量** × scale。
                        // 实测依据（Win 截图 4.jpg 当 oracle）：B灯台 在 Win 版里 x≈82%、y≈72%，
                        // 而我们基线 x=1312/1600=82%（已对）、y=294/900=33%（偏上）。
                        // 加 x 偏移会把所有对象推出右侧（mode1 实测：大批对象直接出画），
                        // 只加 y 偏移则 x 保持不变、y 落到 645/900=72% —— 与 Win 截图吻合。
                        5 => (crop_offset[0] * sc[0] * 2.0, crop_offset[1] * sc[1] * 2.0),
                        6 => (0.0, crop_offset[1] * sc[1]),
                        // 7 = **只对"无特效链"的对象**应用 ×scale 位移。依据（field_diff 实测）：
                        // 偏的 3 个对象 effects 数全是 0；准的 6 个全是 1~2（唯一例外 B背景 是居中满画布）。
                        // ⇒ 带特效的对象走图层/RT 路径，位置本来就对；直接路径缺这个位移。
                        // 全局应用会把特效路对象再偏一次（= 之前 IP 7.55→15.87 的原因）。
                        7 => {
                            if obj.effects.is_empty() {
                                (crop_offset[0] * sc[0], crop_offset[1] * sc[1])
                            } else {
                                (0.0, 0.0) // 带特效链的对象走图层/RT 路径，位置本就正确，别动
                            }
                        }
                        // 8 = 无特效对象**只取 y**；9 = 无特效对象取 (-x, +y)。
                        // Win 截图显示 x 本来就对（灯台横向 82% 与我们对齐）⇒ x 分量不该动。
                        8 => {
                            if obj.effects.is_empty() {
                                (0.0, crop_offset[1] * sc[1])
                            } else {
                                (0.0, 0.0)
                            }
                        }
                        // 10 = **无特效对象、只取 y、×2**（= mode6 的幅度 + mode7 的适用范围）。
                        // 依据：mode8（只 y、×1）把 B灯台 y 从 294 修到 462，方向对但只走了一半；
                        // ×2 后应到 ~645，与 Win 截图 4.jpg 里灯台/矮柜的 ~72%(=648) 吻合；
                        // B石头2 → ~621（应在 632）、B沙发 → ~559（应在 552）——三者都在 ±20px 内。
                        // x 分量绝不动（mode9 实测：一翻就歪；Win 截图里横向本来就对）。
                        10 => {
                            if obj.effects.is_empty() {
                                (0.0, crop_offset[1] * sc[1] * 2.0)
                            } else {
                                (0.0, 0.0)
                            }
                        }
                        _ => (crop_offset[0] * sc[0], crop_offset[1] * sc[1]),
                    };
                    (
                        Mat4::translate([ox, oy * sy, 0.0]),
                        mvp_world,
                    )
                };
                // 官方铁律①（文字层）：RT 已裁到「墨迹 bbox + 2×padding」⇒ 四边形必须同步
                // 换成那块矩形（局部平移 + 半尺寸），否则字形会被拉伸到整个 `obj.size`。
                // 非文字层 / 没有裁切记录 ⇒ 与原 `scale(size/2)` **逐位相同**。
                let quad_m = match self.text_quads.get(&obj.id) {
                    Some((off, half)) if obj.kind() == we_scene::scene::ObjectKind::Text => {
                        Mat4::translate([off[0], off[1], 0.0])
                            .mul(&Mat4::scale(half[0], half[1], 1.0))
                    }
                    _ => Mat4::scale(size[0] / 2.0, size[1] / 2.0, 1.0),
                };
                obj_model_only_m = pre.mul(&world).mul(&quad_m);
                if attach_hit.is_some() {
                    attach_q_l = Some(proj.mul(&pre));
                }
                proj.mul(&pre).mul(&world).mul(&quad_m)
            };
            // shape 的四个角点（单位四边形坐标 + uv = 归一化点），供**收尾趟**用。
            // 为什么要它：WE 把 shape 改写成 ImageObject，`requires_source_draw = false`
            // ⇒ **最后一个输出趟是画在 `final_mesh`（= 直接绘制四边形）上**的
            // （`SceneNodeLayer::Resolve` 的清尾几何分支；`ParseShapeObj` 里
            // `.final_mesh = Some(direct_draw_mesh)`），链内其余趟才是全屏 [-1,1] 四边形。
            // 我们以前把 shape 的每一趟都当全屏趟 ⇒ 扇区的参数化退化成「屏幕归一化 uv 经 S⁻¹」，
            // 与作者那四个控制点**没有位置关系**（实测：只对四边形趟生效的探针
            // `fx/vfxy/mang/mrad/grad/gR/gF/rdmax` 对最终画面**逐字节无影响** ⇒ 四边形趟
            // 一个像素都没上屏）。⚠ 必须在 `obj_scalars.extend(extra_scalars)` **之前**取。
            // `WE_SHAPE_QUAD=0` 回退。
            let shape_quads: Option<Vec<[f32; 4]>> = if is_shape && shape_mvp_on {
                let mut pts: Vec<[f32; 2]> = Vec::new();
                for i in 0..4 {
                    let key = format!("g_Point{i}");
                    match extra_scalars
                        .iter()
                        .find(|(k, _)| *k == key)
                        .and_then(|(_, v)| match v {
                            Value::V2(a, b) => Some([*a, *b]),
                            _ => None,
                        }) {
                        Some(p) => pts.push(p),
                        None => break,
                    }
                }
                if pts.len() == 4 {
                    Some(
                        pts.iter()
                            .map(|p| [p[0] * 2.0 - 1.0, 1.0 - p[1] * 2.0, p[0], p[1]])
                            .collect(),
                    )
                } else {
                    None
                }
            } else {
                None
            };
            // 几何自证（`WE_SHAPE_GEOM=1`，默认关 ⇒ 逐字节不变）：
            // 把 shape 的「**实际画到的四角**（NDC）」与「**按 WE 语义从配置推出的四角**（NDC）」
            // 并排打出来，判据 = 「对象实际位置 == 配置推出的位置」。
            // 配置一侧的依据（OWE `ParseShapeObj`，SceneImageObjectParser.cpp:1244-1252 +
            // `GenDirectDrawQuadMesh`，SceneGeometryBuilder.cpp:64-88）：
            // 顶点 = `((px-0.5)·edge, (0.5-py)·edge, 0)`（**对象空间**）、`size = (edge, edge)`、
            // `edge = ortho_h`，然后走**标准** `proj·world·scale(size/2)`；
            // UV 直接是 `(px, py)`。注意它**不是** NDC。
            // 我们的单位四边形坐标 `(2px-1, 1-2py)` 乘 `scale(size/2)` 恰好也等于那个对象空间顶点，
            // ⇒ 两侧可以喂**同一个**单位坐标，只换矩阵，差异就纯粹是"有没有走世界+投影"。
            if is_shape && std::env::var_os("WE_SHAPE_GEOM").is_some() {
                let pts: Vec<[f32; 2]> = (0..4)
                    .filter_map(|i| {
                        let key = format!("g_Point{i}");
                        extra_scalars
                            .iter()
                            .find(|(k, _)| *k == key)
                            .and_then(|(_, v)| match v {
                                Value::V2(a, b) => Some([*a, *b]),
                                _ => None,
                            })
                    })
                    .collect();
                let clip_of = |m: &Mat4, c: [f32; 2]| -> [f32; 4] {
                    let v = [c[0], c[1], 0.0, 1.0];
                    let mut o = [0.0f32; 4];
                    for i in 0..4 {
                        o[i] = (0..4).map(|j| m.m[j * 4 + i] * v[j]).sum();
                    }
                    o
                };
                // 配置一侧的完整 MVP（= 非 shape 分支的公式，只是 size 换成 (edge,edge)）
                let cfg_m = proj
                    .mul(&mvp_world)
                    .mul(&Mat4::scale(size[0] / 2.0, size[1] / 2.0, 1.0));
                eprintln!(
                    "[SHAPE-GEOM] {} id={} edge={} 点数={}",
                    obj.name,
                    obj.id,
                    size[0],
                    pts.len()
                );
                eprintln!("[SHAPE-GEOM]   proj={:?}", proj.m);
                eprintln!("[SHAPE-GEOM]   world={:?}", mvp_world.m);
                for (i, p) in pts.iter().enumerate() {
                    let uq = [p[0] * 2.0 - 1.0, 1.0 - p[1] * 2.0];
                    let a = clip_of(&Mat4::identity(), uq);
                    let c = clip_of(&cfg_m, uq);
                    eprintln!(
                        "[SHAPE-GEOM]   p{i}=({:.5},{:.5}) 单位四边形=({:.5},{:.5}) \
                         实际NDC=({:+.5},{:+.5},w={:.4}) 配置NDC=({:+.5},{:+.5},w={:.4})",
                        p[0], p[1], uq[0], uq[1],
                        a[0] / a[3], a[1] / a[3], a[3],
                        c[0] / c[3], c[1] / c[3], c[3]
                    );
                }
            }
            // 上传用的（转置）字面量；未转置的 `obj_mvp_m` 留给"图层空间收尾趟"在归一化坐标里
            // 再叠一次取景框变换（见下方 `blit_mvp`）。
            let obj_mvp = obj_mvp_m.transposed().column_major();
            if dbg && base_dbg.starts_with("文字") {
                eprintln!(
                    "[图层] ✓ {}：{base_dbg}，尺寸={} 原点={}，特效 {} 个",
                    obj.name,
                    obj.size,
                    obj.origin,
                    obj.effects.len()
                );
            }

            // 对象级常量：染色 + 不透明度（OWE 把这三个放进 baseConstSvs，见 SceneImageObjectParser）
            // ⚠ 文字层例外：字色**已经烘进光栅化贴图**了（见 `build_text_layer`），
            // 再乘一次 `g_Color4` 就等于把颜色平方 —— 实测 `2823687015` 的 `color=(0.055,0.082,0.043)`
            // 平方后是 0.003 ⇒ 字全黑看不见。所以文字层这里给白，只保留 alpha。
            let rgb = if obj.kind() == we_scene::scene::ObjectKind::Text {
                [1.0, 1.0, 1.0]
            } else {
                obj.color_rgb()
            };
            let alpha = obj.alpha_f32();
            let mut obj_scalars: Vec<(String, Value)> = vec![
                ("g_Color4".into(), Value::V4(rgb[0], rgb[1], rgb[2], alpha)),
                // `flat` shader（`solidlayer` 用的那条：无贴图、纯色四边形）声明的 uniform 是
                // **`vec3 g_Color`**（不是 `g_Color4`）⇒ 以前 `g_Color` 从没下发过 ⇒ 对象色是 0
                // ⇒ 所有 solidlayer（`纯色`/`横线`/`信封点`…）都画成**黑色**。补上。
                ("g_Color".into(), Value::V3(rgb[0], rgb[1], rgb[2])),
                ("g_Alpha".into(), Value::F(alpha)),
                ("g_UserAlpha".into(), Value::F(alpha)),
            ];
            // 诊断（`WE_DBG_OBJ_COLOR=1`，默认关，无副作用）：把**非中性**的对象级颜色列出来
            // （`g_Color4 != (1,1,1,1)`，即对象 `color` 非白或 `alpha != 1`）。用来生成
            // "`WE_OBJ_COLOR=1` 会改变哪些对象"的清单；是否真有像素效果还要看该趟 shader
            // 有没有声明 `g_Color4`（`WE_DUMP_SHADER` 落盘的 `.base.frag` 里 grep）。
            if std::env::var("WE_DBG_OBJ_COLOR").ok().as_deref() == Some("1") {
                let neutral = (rgb[0] - 1.0).abs() < 1e-6
                    && (rgb[1] - 1.0).abs() < 1e-6
                    && (rgb[2] - 1.0).abs() < 1e-6
                    && (alpha - 1.0).abs() < 1e-6;
                if !neutral {
                    eprintln!(
                        "[对象色] id={} 「{}」kind={:?} color={:?} ⇒ g_Color4=({:.5},{:.5},{:.5},{:.5})",
                        obj.id, obj.name, obj.kind(), obj.color, rgb[0], rgb[1], rgb[2], alpha
                    );
                }
            }
            // 诊断（`WE_DBG_OBJ_ALPHA=1`，默认关，无副作用）：把**非中性 alpha** 的对象列出来
            // （`alpha_f32 != 1` ⇒ `g_Alpha`/`g_UserAlpha` 会拿到非 1 值）。这是
            // 「`WE_OBJ_ALPHA=1` 会改变哪些对象」的清单来源：**还要**该对象底图趟的 shader 真
            // 声明并使用 `g_Alpha`/`g_UserAlpha`（`WE_DUMP_SHADER` 落盘的 `*.base.frag` 里 grep）。
            // 注：`brightness` 一栏供参考 —— 本语料 17 条场景里对象 `brightness` 全为 1.0
            // （19 处显式写 1.0，其余无该字段 = 默认 1.0）⇒ `g_Brightness` 的对象值与当前
            // 强制值**逐值相同**、本开关对它不可观测；且 `we_scene` 未解析该字段（不在本次
            // 允许改动的目录内）⇒ 不改数值来源。
            if std::env::var("WE_DBG_OBJ_ALPHA").ok().as_deref() == Some("1") {
                if (alpha - 1.0).abs() > 1e-6 {
                    eprintln!(
                        "[对象alpha] id={} 「{}」kind={:?} alpha={:.5} ⇒ g_Alpha=g_UserAlpha={:.5}",
                        obj.id, obj.name, obj.kind(), alpha, alpha
                    );
                }
            }
            // shape：把"首个特效趟"的常量也当成对象级常量下发（四角点 point0..3、颜色…）
            obj_scalars.extend(extra_scalars);
            // ── `#require LightingV1` / `#if LIGHTING` 那条路要用的**引擎矩阵族** ──────────
            // 官方 `genericimage4.vert:60/282/287/415/451` 在 `#if LIGHTING` 下把最终位置从
            // `g_ModelViewProjectionMatrix` **换成** `g_ViewProjectionMatrix`（世界坐标改从
            // `g_ModelMatrix` 来）。我们以前只喂 `g_ModelViewProjectionMatrix` ⇒ 这两个名字在
            // GL 里是 0 ⇒ `gl_Position = mul(worldPos, 0) = 0` ⇒ **整个四边形塌成一个点、该层消失**
            // （实测 `3571777169`：`WE_MAT_COMBOS=1` 打开材质的 `LIGHTING=1` 后人物层直接不见了；
            // `WE_MAT_COMBOS=0` 时它走 `#else` 分支用 MVP，所以一直正常）。
            // 两者**相乘逐位等于**原来的 MVP（运算顺序见 `obj_model_only_m` 那处的注释），
            // 所以只影响真正走 LIGHTING 分支的趟 —— 不声明/不引用它们时 GL 里是 inactive ⇒ 无副作用。
            obj_scalars.push((
                "g_ModelMatrix".into(),
                Value::M4(obj_model_only_m.transposed().column_major()),
            ));
            obj_scalars.push((
                "g_ViewProjectionMatrix".into(),
                Value::M4(obj_view_proj_m.transposed().column_major()),
            ));
            // `g_NormalModelMatrix`（`mat3`，`genericimage2.vert:34`）：本管线里图层几何都平铺在
            // z 平面上（`scale` 的 z 恒 1），世界法线 = (0,0,1) ⇒ 单位阵即正确
            // （对象 `angles` 在本机 17 条里全是绕 z，绕 z 不改变 (0,0,1)）；
            // 有绕 x/y 的层时这里会偏（本机零命中），届时按 OWE 的 `ModelTrans` 派生即可。
            #[rustfmt::skip]
            let normal_m3: [f32; 9] = [
                1.0, 0.0, 0.0,
                0.0, 1.0, 0.0,
                0.0, 0.0, 1.0,
            ];
            obj_scalars.push(("g_NormalModelMatrix".into(), Value::M3(normal_m3)));

            // 混合方式：材质的 `blending`，被对象的 `colorBlendMode` 覆盖（31 ⇒ 加法）
            // （文字对象没有材质 ⇒ 以前 `base_blending=None` ⇒ **关混合 = 覆盖**）
            // 文字层必须**半透明**画：我们的文字贴图是"透明底 + 字形"（`text::rasterize` 从
            // `vec![0u8; …]` 起画），关混合会把**整个对象矩形**用贴图的 RGB（字形之外全 0 =
            // 黑）覆盖上去；再叠加"链的收尾趟铺满全屏"这条 ⇒ 实测 `3704273480` 的那个
            // **名字为空的文字层（id=691「mouse pad」）把整屏涂黑**，背景/人物全被压掉
            // （`WE_SKIP_ID=691` 一开，同一帧立刻恢复成正常的海滩画面）。
            // WE 里不会出事是因为它收尾趟只用**对象自己的矩形**。`WE_TEXT_BLEND=opaque` 回旧行为。
            let text_blend = if obj.kind() == we_scene::scene::ObjectKind::Text
                && std::env::var("WE_TEXT_BLEND").ok().as_deref() != Some("opaque")
            {
                Some("translucent".to_string())
            } else if obj.copy_background {
                Some("translucent".to_string())
            } else {
                None
            };
            let cbm_faithful = std::env::var("WE_CBM_MATERIAL").ok().as_deref() == Some("1");
            // K1：开着 `WE_CBM_SHADER_BLEND`（默认开） 且这一层真能塞 combo 时，用 `cbm_plan` 里的
            // GL 混合（= 材质 blending，**不动**）；其余一切情况逐字走老路径（不翻任何开关）。
            let blend = match cbm_plan {
                Some((b, _, _, _)) => b,
                None if cbm_faithful => gl_ctx::Blend::of_object_faithful(
                    base_blending.as_deref().or(text_blend.as_deref()),
                    obj.color_blend_mode,
                ),
                None => gl_ctx::Blend::of_object(
                    base_blending.as_deref().or(text_blend.as_deref()),
                    obj.color_blend_mode,
                ),
            };
            if obj.color_blend_mode != 0 && obj.color_blend_mode != 31 && cbm_plan.is_none() {
                eprintln!(
                    "[场景] ⚠ 「{}」的 colorBlendMode={} 是 shader 混合分支（需要把画面当纹理，\
                     OWE `ApplyLayerColorBlend` 只塞 combo、不动 GL 混合）⇒ {}\
                     （`WE_CBM_MATERIAL=1` 切到忠实语义：保持材质 blending；\
                     ★ `WE_CBM_SHADER_BLEND`（默认开，`=0` 关） 才真的把 `BLENDMODE` 塞给 shader）",
                    obj.name, obj.color_blend_mode,
                    if cbm_faithful { "走忠实语义\n" } else { "本机旧行为：先按加法画\n" }
                );
            }
            if dbg {
                eprintln!(
                    "[图层]   混合={:?}（材质 blending={:?}，colorBlendMode={}）alpha={alpha:.2} color={rgb:?}",
                    blend, base_blending, obj.color_blend_mode
                );
            }

            // 这一层被别人用 `_rt_imageLayerComposite_<id>_<a|b>` 引用了吗
            let published = referenced.contains(&obj.id);
            // 被引用 ⇒ 把这张 RT 同时登记成 `_a`/`_b`/不带后缀三种写法。
            // ⚠ 语义取舍：WE 的真名是"图层**发布**输出"（OWE：`LayerOutput` +
            // `PublishedEffect`，而且只有 `publishes_output` 的层才算数）。我按"链的最终输出"另建发布 RT
            // 试过：**Sona 立刻变暗**（亮度 201 → 87），而作者预览是高亮的 ⇒ 这条引用要的是
            // **自己本帧的合成底图**（旧的隐式行为，正好也是 `_b` 的名字所指向的）。跨层引用那几条
            // （2080534540 / 2823687015 / 3704273480）实测差异 ≤1.4 ⇒ 用"本层合成"这一种绑定足够。
            // 真要完整语义，得照 OWE 的 `RegisteredLayerLinkSource` + 发布口做。
            // 没有特效：直接画到屏幕（被引用时先落进"图层链接 RT"，再贴回屏幕）
            let link_native = link_rt_native_mode();
            if obj.effects.is_empty() {
                // （WE_LINK_RT_NATIVE，默认 0=关）：被引用图层的链接 RT 按 WE 口径分配成
                // 该图层自己的尺寸；只对不可见的被引用层生效（= 只当遮罩那种，目标 1583）。
                let native_dims: Option<(u32, u32)> = if published && !vis && (link_native != 0) {
                    // `WE_LINK_RT_DENSITY=<f>`（默认 1.0 = 不动）：链接 RT 的**每场景单位像素数**。
                    // `effects/blend` 的放大率 = `blendscale · quad_h / (T0_h · RT 像素/单位 …)` ——
                    // 严格说是 `blendscale · quad_h · density1 / (density0 · T0_units)`
                    // （density1 = 链接 RT 的 px/单位，density0 = T0 的 px/单位）。
                    // ⇒ **只有 density1（= 这张 RT 的分配尺寸 / 图层世界尺寸）能把字里透出的角色
                    // 缩放而不动 `blendscale`**（T0 那条路会连落位一起搬，实测）
                    // 说明）。
                    // win 3-1.jpg 的观察（字里取景更宽、更软）指向 WE 的链接 RT 分配得比图层小。
                    // 本开关就是那个自由量，默认 1.0（= 逐字节不变），实验值 0.5 / 0.58 / 0.72。
                    let dens: f32 = std::env::var("WE_LINK_RT_DENSITY")
                        .ok()
                        .and_then(|v| v.trim().parse::<f32>().ok())
                        .unwrap_or(1.0)
                        .clamp(0.1, 2.0);
                    Some((
                        (size[0].abs() * dens).round().max(1.0).min(8192.0) as u32,
                        (size[1].abs() * dens).round().max(1.0).min(8192.0) as u32,
                    ))
                } else {
                    None
                };
                let pub_rt = if published {
                    let (pw, ph) = native_dims.unwrap_or((out_w, out_h));
                    let i = self.new_rt(
                        pw,
                        ph,
                        &format!("_rt_imageLayerComposite_{}_b", obj.id),
                    )?;
                    self.alias_rt(obj.id, i);
                    Some(i)
                } else {
                    None
                };
                // `WE_LINK_RT_CONTENT`：链接 RT 里**内容**的缩放（默认 = 推导值）。
                // 只在"按图层自身尺寸分配 RT"这条路上有意义（RT 铺满 0..1 ⇒ 缩放单位框即可）。
                // 缩放绕 `link_rt_content_pivot`（= 四边形中心实际采到的那个 RT 点）⇒ 取景不动。
                // ⚠ 默认**不走这条**：RT 侧缩放需要知道"四边形中心采到 RT 的哪一点"，那个点由消费方
                // 的 `blendoffset` 决定，实测绕点缩放会把内容搬出取景窗（见本次报告）。默认走
                // **消费方 UV 侧**（`WE_LINK_RT_CONTENT_SPACE=uv`，在 blend 趟的顶点着色器里缩放
                // 遮罩 UV 跨度 ⇒ 取景中心天然不动）。要 A/B 时 `WE_LINK_RT_CONTENT_SPACE=rt` 走这条。
                let content_xform: Option<(f32, [f32; 2])> = if let Some((rw, rh)) = native_dims
                    .filter(|_| {
                        std::env::var("WE_LINK_RT_CONTENT_SPACE").ok().as_deref() == Some("rt")
                    })
                {
                    let c = link_consumer.get(&obj.id).copied();
                    let _ = c;
                    let f = link_rt_content_scale();
                    f.map(|f| {
                        let piv = c
                            .map(|c| link_rt_content_pivot(&c, (rw as f32, rh as f32)))
                            .unwrap_or([rw as f32 * 0.5, rh as f32 * 0.5]);
                        // RT 像素 → NDC（±1）。RT 里的内容在采样时按 `link_rt_flip` 翻 V
                        // （默认翻），pivot 的 y 必须走同一朝向，否则"绕采样点缩放"会偏到别处。
                        let mut ndc = [
                            piv[0] / (rw as f32 * 0.5) - 1.0,
                            piv[1] / (rh as f32 * 0.5) - 1.0,
                        ];
                        if link_rt_flip {
                            ndc[1] = -ndc[1];
                        }
                        // 诊断旋钮（默认不设）：`WE_LINK_RT_PIVOT=x,y` 直接指定 NDC 缩放中心，
                        // 用来离线扫"四边形中心到底采到 RT 的哪一点"（不用改代码重编）。
                        if let Ok(v) = std::env::var("WE_LINK_RT_PIVOT") {
                            let n: Vec<f32> =
                                v.split(',').filter_map(|t| t.trim().parse().ok()).collect();
                            if n.len() == 2 {
                                ndc = [n[0], n[1]];
                            }
                        }
                        (f, ndc)
                    })
                } else {
                    None
                };
                if published && dbg {
                    eprintln!(
                        "[链接RT] id={} 内容缩放={:?}（消费方={:?}；WE_LINK_RT_CONTENT=0/1/off 关）",
                        obj.id, content_xform, link_consumer.get(&obj.id)
                    );
                }
                // 朝向必须和 blit 那条路一致：blit 在闸门开时是翻过的（WE_FX_QUAD_FLIP 默认开），
                // 直接落 RT 若用原朝向就会和带特效的图层相反 ⇒ 整幅上下颠倒。
                if self.scene_rt.is_some() && (pub_rt.is_none() || link_rt_flip) {
                    base_layer.raw_position = true;
                }
                // 该对象的唯一/最后一趟 ⇒ 关 alpha 写入（参照 CImage::render）。
                // ⚠ 但**落进图层链接 RT 的那一趟绝对不能关**：那张 RT 是给别人采样用的
                // （`effects/blend` 的 `OPACITYMASK` 读它的 `.r`/alpha）。实测 `3704273480` 主题2：
                // `背景角色`(1583) 的链接 RT 被关掉 alpha 后 **alpha 全 0**（平均 R=62、不透明占比 0%）
                // ⇒ 文字层那趟 blend 的遮罩 ≈0 ⇒ 几乎不生效 ⇒ 字里透不出角色。
                // `WE_LINK_RT_ALPHA=0` 关回旧行为（默认**开**＝写 alpha）。
                // native 模式 1 下 RT 里是图层空间内容 ⇒ 按图层空间那条约定翻一次 V
                // （与 WE_LAYER_VFLIP 同一理由：两条路共用同一个 ortho）。
                if native_dims.is_some() && (link_native == 1) {
                    base_layer.raw_position = !base_layer.raw_position;
                }
                let link_rt_alpha = std::env::var("WE_LINK_RT_ALPHA")
                    .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"))
                    .unwrap_or(true);
                if pub_rt.is_none() || !link_rt_alpha {
                    self.alpha_off.insert(self.passes.len());
                }
                // K1：底图趟就是"把这一层写进场景"的那一趟 ⇒ 把 shader 声明的"画面"槽
                // 绑到**画这一层之前的画面拷贝**上（`ApplyBlending(BLENDMODE, A=拷贝, B=本层, a)`）。
                // 画之前的那次 GPU 拷贝由趟循环按 `self.cbm_passes` 里记的 (拷贝RT, 源RT) 做。
                let mut base_units: Vec<(u32, Src)> = vec![(0, base_src)];
                if let Some((_, slot, copy, src)) = cbm_plan {
                    base_units.push((slot, Src::Rt(copy)));
                    self.cbm_passes.insert(self.passes.len(), (copy, src));
                }
                // 探针（`WE_DBG_PUBRT=1`）：**被引用层**渲进链接 RT 时的几何自报。
                // 动机：实测该 RT 里内容"丢了世界摆位、被居中"（§65：1583 声明 x 0.043–0.564 →
                // 实测 0.283–0.803；350 声明 0.531–1.088 → 实测 0.280–0.574，尺寸都精确）。
                // 这里把能拿到的几何量全打出来，与实测包围盒互相解释。
                if published
                    && std::env::var("WE_DBG_PUBRT")
                        .map(|v| !matches!(v.trim(), "0" | "false" | "off" | "no"))
                        .unwrap_or(false)
                {
                    eprintln!(
                        "[发布RT] id={} 「{}」obj.size={:?} obj.origin={:?} obj.scale={:?} \
                         对象矩形=({:.1},{:.1}) RT={}x{} out={}x{} compose_layer={} raw_pos={} \
                         链接RT=Some({}) units={:?}",
                        obj.id,
                        obj.name,
                        obj.size,
                        obj.origin,
                        obj.scale,
                        size[0],
                        size[1],
                        out_w,
                        out_h,
                        out_w,
                        out_h,
                        compose_layer,
                        base_layer.raw_position,
                        pub_rt.map(|v| v as i64).unwrap_or(-1),
                        base_units.len()
                    );
                }
                // shouwei probe (WE_DBG_PUBRT=2): this pass's obj_mvp + the viewport/ortho at RT entry.
                if published
                    && std::env::var("WE_DBG_PUBRT")
                        .map(|v| v.trim() == "2" || v.trim() == "1")
                        .unwrap_or(false)
                {
                    let (rw, rh) = match pub_rt {
                        Some(pp) => (self.rts[pp].w, self.rts[pp].h),
                        None => (out_w, out_h),
                    };
                    let mut mm = String::new();
                    for rr in 0..4 {
                        for cc in 0..4 {
                            mm.push_str(&format!("{:.4} ", obj_mvp_m.m[rr * 4 + cc]));
                        }
                        mm.push('|');
                    }
                    eprintln!("[PUBRT-MVP] id={} name={} pass={} obj_mvp(relu)= {}", obj.id, obj.name, self.passes.len(), mm);
                    eprintln!(
                        "[PUBRT-MVP] proj m0={:.6} m5={:.6} t=({:.3},{:.3}) cover={:?} scene={:?} out={}x{} first_target_rt=#{} {}x{} viewport=(0,0,{},{})",
                        proj.m[0], proj.m[5], proj.m[12], proj.m[13], self.cover, self.scene_size,
                        out_w, out_h, pub_rt.map(|v| v as i64).unwrap_or(-1), rw, rh, rw, rh
                    );
                    let (mx, my) = (mvp_world.m[12], mvp_world.m[13]);
                    eprintln!(
                        "[PUBRT-MVP] world_trans=({:.3},{:.3}) scene_units => canvas_frac=({:.4},{:.4}); obj.size={:?} obj.origin={:?} obj.scale={:?} rect=({:.1},{:.1}) origin3=({:.2},{:.2})",
                        mx, my,
                        (mx - self.cover.0) / (self.cover.2 - self.cover.0).max(1e-6),
                        (my - self.cover.1) / (self.cover.3 - self.cover.1).max(1e-6),
                        obj.size, obj.origin, obj.scale, size[0], size[1], origin[0], origin[1]
                    );
                    let mut lo = (f32::MAX, f32::MAX);
                    let mut hi = (f32::MIN, f32::MIN);
                    let mut nd = String::new();
                    for &(x, y) in &[(-1.0f32, -1.0f32), (1.0f32, -1.0f32), (1.0f32, 1.0f32), (-1.0f32, 1.0f32)] {
                        let m = &obj_mvp_m.m;
                        let wq = m[3] * x + m[7] * y + m[15];
                        let wq = if wq.abs() < 1e-9 { 1.0 } else { wq };
                        let nx = (m[0] * x + m[4] * y + m[12]) / wq;
                        let ny = (m[1] * x + m[5] * y + m[13]) / wq;
                        let fx = (nx + 1.0) * 0.5;
                        let fy = (1.0 - ny) * 0.5;
                        lo.0 = lo.0.min(fx);
                        lo.1 = lo.1.min(fy);
                        hi.0 = hi.0.max(fx);
                        hi.1 = hi.1.max(fy);
                        nd.push_str(&format!("({x:.0},{y:.0})->NDC({nx:.4},{ny:.4}) "));
                    }
                    eprintln!(
                        "[PUBRT-MVP] unit_quad {} => predicted RT bbox x {:.3}-{:.3} y {:.3}-{:.3}",
                        nd, lo.0, hi.0, lo.1, hi.1
                    );
                }
                self.passes.push(Pass {
                    layer: base_layer,
                    // `pub_rt` 对"没有特效"的图层是 None（= 直接画屏幕）。闸门开时必须也落到
                    // 场景 RT —— 否则这些层画在屏幕上，又被最后那个不透明的 present 整片盖掉
                    // （实测：Ice Princess 的背景层 `фон мин` 特效=0 ⇒ 教堂整块变黑，只有带特效的
                    // 图层活下来）。闸门关时 self.scene_rt=None ⇒ 行为与以前完全一致。
                    target: match pub_rt {
                        Some(p) => Some(p),
                        None => self.scene_rt,
                    },
                    units: base_units,
                    scalars: obj_scalars.clone(),
                    // 合成/透传层（composelayer/projectlayer）：`composelayer.vert` 用 **a_TexCoord
                    // 当 NDC** 摆满整屏、用 `MVP·a_Position` 决定**采样**位置 ⇒ 要它成为"整屏 1:1
                    // 透传"就必须让 a_Position 直接落在 NDC（单位阵）—— 否则采样到的是合成图的
                    // 某一小块，再被拉伸铺满全屏（`点点点` 的全屏点阵就是这条）。`WE_COMPOSE=obj`
                    // 回旧行为做 A/B。
                    mvp: if compose_layer && std::env::var("WE_COMPOSE").ok().as_deref() != Some("obj") {
                        full_q
                    } else if native_dims.is_some() && (link_native != 3) {
                        // 单位框 ±1 铺满 RT 全幅（= WE 链接 RT 的内容口径）
                        // `WE_LINK_RT_CONTENT`：内容以 RT 中心缩放 `f`（同一张 RT、同一落位，
                        // 只改内容的放大率；推导见 [`link_rt_content_scale`]）。
                        match content_xform {
                            Some((f, p)) => {
                                // 直接按"行向量 × 列主序矩阵"布局写：`gl_Position = mul(vec4(a_Pos,1), M)`
                                // ⇒ 平移在 M[3]/M[7]（第 4 行），缩放在 M[0]/M[5]。
                                // 等价于 `ndc' = f·ndc + (1-f)·p`（绕 p 缩放）。
                                let mut m = [0.0f32; 16];
                                m[0] = f;
                                m[5] = f;
                                m[10] = 1.0;
                                m[15] = 1.0;
                                m[3] = (1.0 - f) * p[0];
                                m[7] = (1.0 - f) * p[1];
                                m
                            }
                            None => Mat4::identity().transposed().column_major(),
                        }                    } else {
                        obj_mvp
                    },
                    // 写进链接 RT 时不该和里面的残影混合
                    blend: if published {
                        gl_ctx::Blend::Opaque
                    } else {
                        blend
                    },
                    // 网格图层的时间动画（每帧按 t 重算装配顶点并重传）
                    mesh_anim: mesh_anim_h.take(),
                });
                // 相机视差：这一趟就是"把层放进场景"的那一趟（合成/透传层除外 —— 那类层的
                // 语义是整屏 1:1 采样，位移由 `composelayer` 自己的几何决定）。
                if let Some(pi) = par_info {
                    if !(compose_layer && std::env::var("WE_COMPOSE").ok().as_deref() != Some("obj")) {
                        self.parallax_passes.insert(self.passes.len() - 1, pi);
                    }
                }
                // 挂骨骼：这一趟就是"把这一层摆进场景"的趟（判据与上面 `mvp:` 那支一致）
                // ⇒ 登记成按场景时间逐帧更新（`WE_ATTACHMENT=1` 且非 `WE_ATTACH_BAKE=1`）。
                if !(compose_layer && std::env::var("WE_COMPOSE").ok().as_deref() != Some("obj")) {
                    AttachHit::register(
                        &mut self.attach_passes,
                        &attach_hit,
                        attach_q_l,
                        obj_mvp_m,
                        self.passes.len() - 1,
                    );
                }
                if let Some(p) = pub_rt {
                    // 不可见但被引用的层：**只喂 RT、不上屏**（否则会盖住画面 —— 作者的
                    // `visible:false` 就是"只当遮罩"）。见上面 `link_offscreen`。
                    if link_offscreen && !vis {
                        continue;
                    }
                    let blit = self.make_blit_layer()?;
                    self.passes.push(Pass {
                        layer: blit,
                        target: self.scene_rt,
                        units: vec![(0, Src::Rt(p))],
                        scalars: vec![],
                        // native mode keeps the layer rect in the RT, so the blit back
                        // to screen must use the object rect geometry (obj_mvp), not full_q.
                        mvp: if native_dims.is_some() && (link_native != 3) {
                            obj_mvp
                        } else {
                            full_q
                        },
                        blend,
                        mesh_anim: None,
                    });
                }
                continue;
            }

            // 有特效：图层先合成到一个全尺寸 RT（链上第一趟的 `previous` 就是它）。
            // 名字沿用 WE 的图层链接名 `..._b`：这样**这一层自己**（或别的层）引用
            // `_rt_imageLayerComposite_<id>_b` 时，拿到的就是本帧的合成底图（= 实测与作者预览一致的行为）。
            // `_a` / 不带后缀也在下面用 `alias_rt` 登记到同一张（WE 的 a/b 双缓冲，只读场景下等价）。
            // ── 特效链跑在【图层空间】还是【画布空间】────────────────────────────
            // 参照实现把每个图层写进**它自己尺寸的 FBO**（`CImage.cpp:271-276`：
            // `_rt_imageLayerComposite_<id>_a/_b`，尺寸 = scene.json 的 `size`，**不含 scale**），
            // 链上每趟的 `g_TextureNResolution` 因此是**图层尺寸**；只有收尾趟用 Screen 几何摆进场景。
            // 我们以前整条链跑在画布尺寸的缓冲上 ⇒ 一切"按 resolution 算量"的 shader 都偏：
            // · `foliagesway`：`aspect = Res.z/Res.w * g_Ratio` ⇒ 摆动各向异性（实测纵向差 3 倍）
            // · `blur_gaussian`：`offset = g_Scale / Res.z`（纹素步长）
            // · `waterwaves` 之类按纹素算波长的同理
            // 开关 `WE_CHAIN_SPACE=canvas` 回旧行为做 A/B。
            // ⚠ 被别的图层用 `_rt_imageLayerComposite_<id>` **引用**的层不切换：链接 RT 的消费方
            // 是按画布空间采样的，改成图层尺寸会让它们整体错位（这是另一个待办，别混进来）。
            let chain_off: Vec<String> = std::env::var("WE_CHAIN_SPACE_OFF")
                .unwrap_or_default()
                .split(',')
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().to_string())
                .collect();
            let chain_on: Vec<String> = std::env::var("WE_CHAIN_SPACE_ON")
                .unwrap_or_default()
                .split(',')
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().to_string())
                .collect();
            let layer_space_env = std::env::var("WE_CHAIN_SPACE").ok().as_deref() == Some("layer");
            // 模型层（A 路线）：**有网格的对象自动走"图层空间"** —— 那条路的 RT 按对象自身
            // 尺寸分配、首趟用对象自己的矩形投影，正是网格 uv 成立所需的语义
            // （WE 把每趟结果渲进"对象自己的纹理空间"）。以前网格对象走画布空间
            // ⇒ RT 是屏幕布局、网格 uv 对不上 ⇒ 人物变黑影。
            // 现在是**自动跟随** `puppet_on_h`（不再需要 `WE_MODEL_LAYER`）。
            // 晚（"网格/精灵落点一致性"轮）：
            // 图层空间那条路的 y 落点相对画布/平面路径**少一次 y 镜像**
            // （实测同一对象：平面 y170 → 图层 y955），误差 = 2·(scene_h/2 − world_y)，
            // 随对象 origin 变化 ⇒ 吊灯（origin_y 1380~1834）偏 600~800px、`хало` 偏 +854px。
            // 修法**不换这条链**（画布空间会把多片网格挤窄/互相覆盖 ⇒ A/B 人物散架，已否掉），
            // 而是在 `world_of` 之后对无父链的网格对象加一次**纯平移**补偿
            // （`Δy = scene_h − 2·world_y`，见上面 `mesh_ws_here`）⇒ 几何/朝向/uv 全不动。
            // 修后实测：`люстра` 中心 y169 / 平面 170；`хало` 135 / 135；
            // Ice Princess 参照分 8.6237→**7.6509**；与 Windows 1.jpg 头部/悬剑块差 99→28。
            // `WE_MESH_LAYER=0` 整个关掉图层空间（A/B 用；那样 A/B 人物会散架，见 docs）。
            let mesh_layer_space = puppet_on_h
                && puppet_mesh_h.is_some()
                && std::env::var("WE_MESH_LAYER").ok().as_deref() != Some("0");
            // ── `WE_TEXT_CHAIN_LAYER`（默认开）：文字层的特效链跑在**文字画布空间** ──────────
            // 官方构造见 [`text_chain_layer_on`] 的文档注释（= REPORT §3.3 第 2 步）。要点：
            // 链缓冲 = 文字光栅化画布（`self.texs[i]` 的真实像素尺寸）、首趟把对象矩形铺满它、
            // 收尾趟用作者的 node 变换（`obj_mvp`）摆回场景 ⇒ RT / 卡片 / T0 三者同尺寸。
            // 只对**挂特效链的文字层**生效（无特效的文字层没有"链空间"这回事，保持现状）。
            // `WE_TEXT_CHAIN_LAYER=0` ⇒ 这个 bool 恒 false ⇒ 下面每一处都走原分支（逐字节回旧）。
            let text_chain_space = text_chain_layer_on()
                && obj.kind() == we_scene::scene::ObjectKind::Text
                && !obj.effects.is_empty()
                && !is_shape
                && !fullscreen_layer
                && !compose_layer
                && (!published || link_rt_chain_here(published, vis));
            let layer_space = layer_space_env
                // ⚠ 别写 `kind == Image`：WE 的图片图层 `image` 是 `models/xxx.json`
                // ⇒ `kind` 返回的是 **Model**（见 we-scene `ObjectKind::kind`）。
                // 这里只要排除 shape / 文字 / 整屏透传层即可（它们在别的分支里另有处理）。
                && !is_shape
                && obj.kind() != we_scene::scene::ObjectKind::Text
                && (!published || link_rt_chain_here(published, vis))
                && !fullscreen_layer
                && (chain_on.is_empty() || chain_on.iter().any(|s| obj.name.contains(s.as_str())))
                && !chain_off.iter().any(|s| obj.name.contains(s.as_str()));
            // （用户报「切身体摇动参数桌面冒出一个缩览图」）：**透传/合成层
            // （`models/util/composelayer.json` / `projectlayer.json`，即 `passthrough`）必须排除
            // 在 rect-chain / 图层空间之外**。
            // 为什么：这类层的语义是「**整屏 1:1 采样当前场景合成图**」——
            // `composelayer.vert` 把 `a_TexCoord` 当 NDC（`position = 2*uv-1`）⇒ 四边形永远铺满整屏；
            // `composelayer.frag` 在 `v_ScreenCoord`（= MVP·a_Position 的屏幕位置）处采样
            // ⇒ 采样坐标 = **屏幕 UV**，与对象的 `size`/`origin` **无关**（WE 里这两趟恰好抵消成 1:1）。
            // 而 rect-chain / 图层空间那条路的前提是「缓冲 = 对象自己的矩形、收尾趟用对象几何摆进场景」
            // ⇒ 对透传层就变成「把整屏的合成图缩进对象那个小矩形」= **屏幕上多出一张缩览图**
            // （实测 `2080534540` 的 `Compose`：1080×1050 @ origin(2313.9,990)，屏幕 1600×900 下
            // 多出一块 x[739,1188] y[269,705] 的整幅缩略图，max|Δ|=235；桌面内核正是带
            // `WE_RECT_CHAIN=1` 跑的 ⇒ 用户看到的就是它）。
            // 这类层的缓冲改成**画布尺寸**、两趟都是整屏 1:1 之后，与 WE 的输出逐像素一致。
            // （`fullscreen_layer`/`published` 本来就被排除，这里补上 `compose_layer`。）
            // （用户报「夏日影 主题2 满屏横条纹/碎片」）：**合成/透传层
            // 带「局部特效」时必须跑图层空间**（缓冲 = 对象自己的矩形）。
            // 根因证据（3704273480，2560x1440，`--t 5`）：
            // · `id=167 信封圆`：`image=models/util/composelayer.json`（`passthrough`），
            // `size="89 2286"`、`origin="1924.84 0"`（父 943 在画布中心 ⇒ 绝对 x 3844.8
            // ⇒ 只露出最右边 40 单位）；挂 `effects/workshop/3082978660/
            // enhanced_simple_audio_bars`（`group=localeffects`，`Bar Count=18`、`SHAPE=2`）。
            // · `id=582 信封线`：同类型，`size="8 2286"`，同特效 `Bar Count=30`。
            // 旧行为：透传层的缓冲 = **画布尺寸**、底图趟单位阵（整屏 1:1），特效趟也是
            // 整屏四边形 ⇒ 「音频条」的 18/30 根条被铺满整屏 ⇒ 用户看到的满屏横条纹
            // （逐层隔离实测：`WE_ONLY_ID=167` 出 18 条 68px 高、周期 80px 的横带；
            // `WE_ONLY_ID=582` 出 30 条 24px 高、周期 48px；两者之和就是画面上的横纹）。
            // Win 参照 `~/Downloads/3-1.jpg` 里这两层各自只是一条**竖细条**：
            // · 167 的矩形 x[3800,3889] ⇒ 2560px 下 x[2533,2560]（参照图右缘那串圆角凸齿，18 个）；
            // · 582 的矩形 x[2736,2744] ⇒ 2560px 下 x[1824,1830]（参照图里那条竖细线）。
            // ⇒ 判据「对象实际位置/形状 == 配置推出的位置/形状」：旧行为两者都不符。
            // 修法（与 WE 的「图层缓冲 = 对象矩形、收尾趟摆回场景」同构）：
            // 1) 这类层改用图层空间 ⇒ 缓冲 = `size`（167: 89x2286），局部特效的条数/
            // 幅度按对象尺寸算（`g_Texture0Resolution` 也对上了）；
            // 2) 底图趟的 MVP 换成 **`obj_mvp`**（不是 `layer_copy_mvp`）：
            // `composelayer.vert` 的 `v_ScreenCoord = MVP·a_Position`，`obj_mvp` 把
            // 单位框映到**对象矩形在场景里的 NDC** ⇒ 片元采样到的正是「合成图在对象
            // 矩形那一块」的 1:1 拷贝（旧的 `full_q`/单位阵只在缓冲=画布时才等价）；
            // 3) 收尾趟已经用 `blit_place_m`（= `obj_mvp_m`）把缓冲摆回同一矩形
            // ⇒ 底图趟 ∘ 收尾趟 = 恒等 ⇒ **无特效/满画布的情形逐像素不变**。
            // `WE_COMPOSE=canvas` 回到旧行为做 A/B。
            // 只对「带 localeffects 的透传层」生效：`blur`/`tint`/`scroll` 那些
            // （group = blur/colorize/animate）本来就是整屏后处理，语义没变，不许动。
            let has_localeffect = obj.effects.iter().any(|e| {
                pkg.get(pkg_bytes, &e.file)
                    .and_then(|b| we_scene::json::from_slice::<shader::EffectFile>(b).ok())
                    .map(|ef| ef.group == "localeffects")
                    .unwrap_or(false)
            });
            let compose_rect = compose_layer
                && has_localeffect
                && std::env::var("WE_COMPOSE").ok().as_deref() != Some("canvas");
            let rect_chain_space = rect_chain_on
                // 不可见+被引用的层的链也走图层空间 ⇒ 链接 RT = 图层自己的画布
                && (!published || link_rt_chain_here(published, vis))
                && !fullscreen_layer
                && !compose_layer
                && (chain_on.is_empty() || chain_on.iter().any(|s| obj.name.contains(s.as_str())))
                && !chain_off.iter().any(|s| obj.name.contains(s.as_str()));
            // （`3571777169` `цепь перед`(2399)「横贯画面的粗黑硬边棒」）：
            // 判据见下方首趟 MVP 那段（「对象四边形在 NDC 里斜置」= 真包围盒 ≠ 对角两点跨度）。
            // 修法 = 首趟改用**局部单位阵**（WE 语义：图层缓冲 = 对象的**局部**矩形），
            // 并取消为旧构造准备的那条 V 反向。回退开关 **`WE_CHAIN_BASE_LOCAL=0`**。
            // 合成/透传层走图层空间的那条路（见上）：`published`/整屏层照旧排除。
            let compose_space = compose_rect
                && (!published || link_rt_chain_here(published, vis))
                && !fullscreen_layer
                && (chain_on.is_empty() || chain_on.iter().any(|s| obj.name.contains(s.as_str())))
                && !chain_off.iter().any(|s| obj.name.contains(s.as_str()));
            if compose_space && dbg {
                eprintln!(
                    "[图层]   合成层 + 局部特效 ⇒ 改用图层空间（缓冲 = 对象矩形 {}x{}）：{} id={}",
                    size[0], size[1], obj.name, obj.id
                );
            }
            let layer_space = layer_space || mesh_layer_space || rect_chain_space || compose_space
                // `WE_TEXT_CHAIN_LAYER`：文字层的链缓冲 = 文字画布（同一条"图层空间"机器，
                // 但尺寸取**光栅化画布**而不是 `obj.size`，见下面的 `lw/lh`）。
                || text_chain_space
                // uv-空间方案的 mesh 图层：底图/链趟已经按展开几何建好了 ⇒ 必须走图层空间那条路
                // （否则装配几何就没有"收尾合成趟"去画，展开图会被直接摆进场景）。见 `mesh_uvspace_on`。
                || self.cur_mesh_uvspace;
            if rect_chain_space && dbg {
                eprintln!("[图层]   WE_RECT_CHAIN：有特效链的非网格对象 ⇒ 改用图层空间（{}）", obj.name);
            }
            if mesh_layer_space && dbg {
                eprintln!("[图层]   模型层：有网格 ⇒ 自动改用图层空间（{}）", obj.name);
            }
            // 网格溢出取景框（修\"腿上的直线切边\"）：装配后的网格常落到归一化 ±1
            // 之外（实测 `底层1` y 低到 −1.234、`底层2` −1.139、`B沙发` −1.513），而图层缓冲
            // 恰等于对象矩形 ⇒ 超出部分被**硬裁成一条直线**（脚/小腿/下缘齐刷刷断掉 ——
            // 就是用户报的那道切边；`WE_MESH_DY=0` 时脚就回来了，一验即知）。
            // 这里取【对象矩形 ∪ 网格包围盒】当取景框，再加 `WE_LAYER_PAD`（默认 0.02）余量；
            // 层内投影与收尾趟同步换算 ⇒ 内容、位置、分辨率**全不变**，只是不再被裁。
            // `WE_LAYER_FIT=0` 关闭（回到旧行为，供 A/B）。
            // `WE_CHAIN_TEXGRID`：缓冲 = 贴图像素格子（官方口径）⇒ 不建取景框
            // 越界顶点改由 `mesh_norm_pos` 钳进 ±1（`WE_CHAIN_TEXGRID_CLAMP=0`：只换缓冲不钳）。
            let mesh_fit_bbox: Option<[f32; 4]> = if layer_space
                && !self.cur_mesh_uvspace
                && !chain_texgrid_on()
                && std::env::var("WE_LAYER_FIT").ok().as_deref() != Some("0")
            {
                puppet_mesh_h.as_ref().filter(|_| puppet_on_h).map(|m| {
                    let pad = std::env::var("WE_LAYER_PAD")
                        .ok()
                        .and_then(|s| s.parse::<f32>().ok())
                        .unwrap_or(0.02);
                    let b = mesh_bbox_norm(m, size, mesh_dy, mesh_flip_up);
                    [
                        (-1.0f32).min(b[0]) - pad,
                        (1.0f32).max(b[1]) + pad,
                        (-1.0f32).min(b[2]) - pad,
                        (1.0f32).max(b[3]) + pad,
                    ]
                })
            } else {
                None
            };
            let fit = mesh_fit_bbox;
            if let (Some(b), true) = (fit, dbg) {
                eprintln!(
                    "[图层]   取景框（对象矩形 ∪ 网格 x[{:.3},{:.3}] y[{:.3},{:.3}]）⇒ 缓冲 {:.0}x{:.0}",
                    b[0], b[1], b[2], b[3],
                    size[0] * (b[1] - b[0]) * 0.5,
                    size[1] * (b[3] - b[2]) * 0.5
                );
            }
            // 取景框的归一化半尺寸（默认 1 = 对象矩形本身）。
            let fit_hx = fit.map(|b| (b[1] - b[0]) * 0.5).unwrap_or(1.0);
            let fit_hy = fit.map(|b| (b[3] - b[2]) * 0.5).unwrap_or(1.0);
            // `WE_MASK_ATLAS_SPACE`：这一层的遮罩要不要从**取景框空间**仿射到
            // **图集空间**。只算一次（逐 pass 复用），返回仿射的 (u0,du,v0,dv)。
            // 作用域与 `SCOPE=narrow` 同一条数据依赖判据（任一 pass 纹理是 `_rt_*`）
            // 文字层与画布空间链的缓冲本就是画布 ⇒ 不碰。见 `mask_atlas_space_on`。
            let mask_atlas_affine: Option<(f32, f32, f32, f32)> = if mask_atlas_mode() == 2
                && !text_chain_space
                && !atlas_base_h
                && obj.effects.iter().any(|e| {
                    e.passes.iter().any(|p| {
                        p.textures
                            .iter()
                            .flatten()
                            .any(|t| t.starts_with("_rt_"))
                    })
                }) {
                // 没有取景框（缓冲 = 对象矩形）⇒ fit = [-1,1,-1,1]。
                let b = fit.unwrap_or([-1.0, 1.0, -1.0, 1.0]);
                let (u0, du) = (0.5 * (1.0 + b[0]), 0.5 * (b[1] - b[0]));
                let (v0, dv) = if mask_atlas_v_down() {
                    // 链内趟是 V 反向四边形：w=0 = 取景框下边 y0。
                    (0.5 * (1.0 - b[2]), -0.5 * (b[3] - b[2]))
                } else {
                    // 非反向四边形：w=0 = 取景框上边 y1。
                    (0.5 * (1.0 - b[3]), 0.5 * (b[3] - b[2]))
                };
                Some((u0, du, v0, dv))
            } else {
                None
            };
            if dbg {
                if let Some((u0, du, v0, dv)) = mask_atlas_affine {
                    eprintln!(
                        "[图层]   遮罩→图集仿射（WE_MASK_ATLAS_SPACE）：u={u0:.4}+{du:.4}·uf  v={v0:.4}+{dv:.4}·vf（fit {:?}）",
                        fit.unwrap_or([-1.0, 1.0, -1.0, 1.0])
                    );
                }
            }
            let (lw, lh) = if atlas_base_h {
                // 缓冲 = **作者画布**（贴图像素尺寸）⇒ 底图四边形以单位 MVP 铺满，
                // 链内遮罩（同样铺满缓冲）与内容**逐像素同空间**。见 `uvspace_atlas_on`。
                // `WE_UVSPACE_ATLAS_SIZE=obj` 退回 `obj.size`（A/B：两者在本场景都是 2140x3500）。
                let dims = match base_src {
                    Src::Tex(i) if std::env::var("WE_UVSPACE_ATLAS_SIZE").ok().as_deref()
                        != Some("obj") =>
                    {
                        self.texs.get(i).map(|t| (t.1.max(1), t.2.max(1)))
                    }
                    _ => None,
                };
                dims.unwrap_or_else(|| {
                    (
                        size[0].abs().round().max(1.0).min(8192.0) as u32,
                        size[1].abs().round().max(1.0).min(8192.0) as u32,
                    )
                })
            } else if text_chain_space {
                // `WE_TEXT_CHAIN_LAYER`：缓冲 = **文字光栅化画布**（1 RT px = 1 画布 px）。
                // 这正是 CaptSilver `WPSceneParser.cpp:4094-4099`（`effect_ppong_a = {texW, texH}`）
                // 与 OWE `SceneTextObjectParser.cpp:98-123`（运行时把 RT 和相机一起 resize 到画布）。
                // ⇒ 卡片铺满缓冲 = `texW×texH` ⇒ `quad_h ≡ T0_h` ⇒ `m` 回落到作者的 `blendscale`。
                // `WE_TEXT_CHAIN_CANVAS=0` ⇒ 改用 `obj.size`（= 作者写 `blendoffset` 时看到的
                // 那张缓冲的像素网格，若 `WE_TEXT_SCALE`≠1 则只有这一档才与作者像素一一对应）。
                // 两档都满足 `quad_h ≡ T0_h`，差别只在 `g_Texture0Resolution` 的**像素网格**
                // （`blendoffset`/`(T0−T1)/2` 是像素量 ⇒ 网格变了取景就跟着变）。
                match base_src {
                    Src::Tex(i) if env_flag("WE_TEXT_CHAIN_CANVAS", true) => self
                        .texs
                        .get(i)
                        .map(|t| (t.1.max(1), t.2.max(1)))
                        .unwrap_or((out_w, out_h)),
                    _ => (
                        size[0].abs().round().max(1.0).min(8192.0) as u32,
                        size[1].abs().round().max(1.0).min(8192.0) as u32,
                    ),
                }
            } else if layer_space {
                // `WE_CHAIN_LAYER_SIZE`：win 的口径 = 图层声明尺寸（不外扩）。
                if chain_layer_size_on() {
                    (
                        size[0].abs().round().max(1.0).min(8192.0) as u32,
                        size[1].abs().round().max(1.0).min(8192.0) as u32,
                    )
                } else {
                    (
                        (size[0] * fit_hx).round().max(1.0).min(8192.0) as u32,
                        (size[1] * fit_hy).round().max(1.0).min(8192.0) as u32,
                    )
                }
            } else {
                (out_w, out_h)
            };
            // `WE_PERF_RES_SCALE`：超大链 RT 按"屏上脚印"降采样（**上报尺寸不变**）。
            // 为什么可以：这些层的链缓冲本来就比它在屏上的脚印大（1016：2539 px 缓冲
            // vs 1693 px 屏上宽度），多出来的分辨率在最终合成时被丢掉 —— 纯超采样。
            // 为什么必须保持上报尺寸：shader 的像素级算子（blur 步长、light-ray 步长、
            // `g_BlendOffset`）都以 `g_Texture{N}Resolution` 为单位 ⇒ 报逻辑尺寸才能
            // 让"效果在画面里的相对尺度"一分不变。见 `perf_res_scale_on`。
            let mut size_rep: Option<(u32, u32)> = None;
            // `WE_PERF_SUPERSAMPLE` / `WE_PERF_SUPERSAMPLE_MASK` 用**逻辑**尺寸
            // （下面 `lw/lh` 会被降采样后的物理尺寸遮蔽，遮罩烘焙仍按逻辑尺寸算）。
            let (lw_log, lh_log) = (lw, lh);
            let ss_mesh_px = perf_supersample_mesh();
            let (lw, lh) = if perf_res_scale_on() || ss_mesh_px.is_some() {
                // 上限 = max(源贴图宽度, 这一层在屏上的宽度)：
                // · 超过源贴图 ⇒ 纯超采样（源里没有那些细节）；
                // · 超过屏上脚印 ⇒ 最终合成时反正会被丢掉。
                // 两者都取到 ⇒ 链缓冲**再也画不出更多可见细节**，只省带宽。
                let canvas_w = (self.cover.2 - self.cover.0).abs().max(1.0);
                let disp_w = lw as f32 * out_w as f32 / canvas_w;
                let src_w = match base_src {
                    Src::Tex(i) => self.texs.get(i).map(|t| t.1 as f32).unwrap_or(lw as f32),
                    // 目标来自"当前画面"的层（整屏后处理）⇒ 源就是输出分辨率
                    _ => out_w as f32,
                };
                // `WE_PERF_SCALE_TARGET`（默认 `max`）：`max` = max(源宽, 屏上宽)；
                // `display` = 只按屏上脚印（更省，等于"链缓冲 = 屏上 1:1"）；
                // `source` = 只按源贴图。三档都保持上报尺寸不变，只改物理采样率。
                let mode = std::env::var("WE_PERF_SCALE_TARGET").unwrap_or_default();
                let cap = match mode.trim() {
                    "display" => disp_w.min(lw as f32),
                    "source" => src_w.min(lw as f32),
                    _ => src_w.max(disp_w).min(lw as f32),
                };
                // 只开了 `WE_PERF_SUPERSAMPLE`（没开 `RES_SCALE`/`SCALE_POST`）时，非骨骼层
                // 必须**一点都不动**：s 从 1.0 起，只有下面的骨骼分支会改它。
                let mut s = if perf_res_scale_on() {
                    (cap / lw as f32).clamp(0.05, 1.0)
                } else {
                    1.0
                };
                // `WE_PERF_SCALE_POST`：只让**整屏后处理层**吃这一刀；其余层一律不降。
                // 判据用**数据**：链缓冲恰好等于画布尺寸（且画布 > 输出）。这类层的输入
                // 就是输出尺寸的场景 RT ⇒ 缓冲里全是上采样出来的像素，没有源细节可丢。
                if perf_scale_post_on() {
                    let canvas_w = (self.cover.2 - self.cover.0).abs().max(1.0);
                    let canvas_h = (self.cover.3 - self.cover.1).abs().max(1.0);
                    let is_canvas_rt = (lw as f32 - canvas_w).abs() <= 1.0
                        && (lh as f32 - canvas_h).abs() <= 1.0
                        && canvas_w > out_w as f32 + 1.0;
                    if !is_canvas_rt {
                        s = 1.0;
                    }
                }
                // 网格（puppet）层默认**不降采样**：发丝/衣边是高频细节，降一档就会在
                // 细边上产生 >100 级的单像素差（实测 `WE_PERF_RES_SCALE` 下头发区
                // max 块差 16.5）。这类层的钱由 scissor 去省。`=0` 可放开做 A/B。
                // `WE_PERF_SUPERSAMPLE=f` 是**显式指定**的中间档（口径：缓冲像素 /
                // 屏上脚印像素）⇒ 优先级高于上面两条（`SCALE_MESH` / 一律不降），
                // 因为它是"用户点名要的档位"，不是副作用。
                if puppet_mesh_h.is_some() {
                    if let Some(f) = ss_mesh_px {
                        s = supersample_chain_scale(f, disp_w, lw);
                    } else if env_flag("WE_PERF_SCALE_MESH", false) {
                        // 允许（A/B）
                    } else {
                        s = 1.0;
                    }
                }
                if lw as u64 * lh as u64 >= 6_000_000 && s < 0.995 {
                    size_rep = Some((lw, lh));
                    (
                        ((lw as f32 * s).round() as u32).max(1),
                        ((lh as f32 * s).round() as u32).max(1),
                    )
                } else {
                    (lw, lh)
                }
            } else {
                (lw, lh)
            };
            if layer_space && dbg {
                eprintln!(
                    "[图层]   链空间=图层 {}x{}（画布 {}x{}）",
                    lw, lh, out_w, out_h
                );
            }
            // ── `WE_FX_CARDSPACE`：特效链内趟换官方"卡片网格"顶点空间 ──────
            // 缺口 / 双出处 / 等价关系 / 边界见 `fx_cardspace_on` 的文档注释。
            // 作用域（**数据驱动**，不看 id / 名字）：只有
            // · 链跑在**图层空间**（缓冲 = 对象自己的矩形 ⇒ 才有"1 个 shader 单位 = 1 个对象像素"）；
            // · 不是整屏后处理层（`fullscreen`）、不是合成/透传层（官方给它们的是**画布尺寸**的
            // 正交相机，`SceneImageObjectParser.cpp:602-609`，映射关系不同）；
            // · 不是 shape / 文字（它们不走图层空间 / 几何另有构造）；
            // · 没有网格取景框（`fit`）也不是 mesh-uvspace（缓冲 ≠ 对象矩形）；
            // 的对象。其余一律保持现状（开关关时**一个字节都不动**）。
            let fx_cards_here = fx_cardspace_on()
                && layer_space
                && !fullscreen_layer
                && !compose_layer
                && (!published || link_rt_chain_cards(published, vis))
                && !is_shape
                && obj.kind() != we_scene::scene::ObjectKind::Text
                && fit.is_none()
                && !self.cur_mesh_uvspace
                && !puppet_on_h;
            let fx_card_px: [f32; 2] = if fx_cards_here {
                let tex_px = match base_src {
                    Src::Tex(i) => self
                        .texs
                        .get(i)
                        .map(|t| [t.1 as f32, t.2 as f32]),
                    _ => None,
                };
                fx_card_size(
                    size,
                    we_scene::scene::parse_vec2(&obj.size),
                    tex_px,
                )
            } else {
                size
            };
            if fx_cards_here && dbg {
                eprintln!(
                    "[图层]   WE_FX_CARDSPACE：特效趟顶点空间 = 官方卡片 ±({:.1},{:.1})（对象 size=({:.1},{:.1})，缓冲 {}x{}）",
                    fx_card_px[0] / 2.0, fx_card_px[1] / 2.0,
                    size[0], size[1], lw, lh
                );
            }
            // ── `WE_BASE_CARDSPACE`：底图趟（类①）换官方"卡片网格"顶点空间 ──
            // 缺口 / 双出处 / 等价关系 / 边界见 `base_cardspace_on` 的文档注释。
            // 作用域（**数据驱动**，不看 id / 名字）：只对"普通图像层的单位四边形"那条路
            // （`base_plain_vs_fs` 已填）生效 —— 网格层 / shape / 文字 / mesh-uvspace 的底图趟
            // 几何另有构造，一律不动。
            // **图层空间**（有特效链的对象）：与 `fx_cards_here` 同一套门。
            // **画布空间**（没有特效链的普通图像对象，例如 `2564205376` 的 `Raiden Mei202102`）：
            // 官方给它们的底图趟**同样是卡片网格**（`SceneImageObjectParser.cpp:365-368` + 主相机），
            // 所以也一起对齐。但那两条路上每帧**还会改写 `pass.mvp`**：
            // · `attach_passes`（draw_frame 里 `anim.m_place_at(t)` 命中时写 `passes[idx].mvp`）；
            // · `parallax_passes`（`mvp_eff` 覆盖）。
            // 被覆盖时顶点仍是 ±size/2、MVP 却不再除 `D` ⇒ 整层被放大 `size/2` 倍。
            // ⇒ 画布空间**只在既没挂骨骼、也没视差的对象**上生效（`attach_hit/par_info` 都为空）。
            // 其余对象保持现状（一个字节都不动）。
            let canvas_cards_ok = !layer_space
                && attach_hit.is_none()
                && par_info.is_none()
                && std::env::var("WE_BASE_MVP").ok().as_deref() != Some("layer")
                && !(compose_layer && std::env::var("WE_COMPOSE").ok().as_deref() != Some("obj"));
            let base_cards_here = base_cardspace_on()
                && base_plain_vs_fs.is_some()
                && !fullscreen_layer
                && !compose_layer
                && (!published || link_rt_chain_cards(published, vis))
                && !is_shape
                && obj.kind() != we_scene::scene::ObjectKind::Text
                && fit.is_none()
                && !self.cur_mesh_uvspace
                && !puppet_on_h
                && (layer_space || canvas_cards_ok);
            if dbg && base_cardspace_on() && base_plain_vs_fs.is_some() && !base_cards_here {
                eprintln!(
                    "[图层]   WE_BASE_CARDSPACE：id={} 「{}」不在作用域〔layer_space={} canvas_ok={} fullscreen={} compose={} published={} shape={} text={} fit={} meshuv={} puppet={}〕",
                    obj.id, obj.name, layer_space, canvas_cards_ok, fullscreen_layer, compose_layer,
                    published, is_shape,
                    obj.kind() == we_scene::scene::ObjectKind::Text,
                    fit.is_some(), self.cur_mesh_uvspace, puppet_on_h
                );
            }
            // 底图趟的卡片尺寸：与特效趟同一套解析（对象 size → 贴图像素尺寸 → 场景兜底）。
            let base_card_px: [f32; 2] = if base_cards_here {
                let tex_px = match base_src {
                    Src::Tex(i) => self.texs.get(i).map(|t| [t.1 as f32, t.2 as f32]),
                    _ => None,
                };
                fx_card_size(size, we_scene::scene::parse_vec2(&obj.size), tex_px)
            } else {
                size
            };
            // `card_ok=false`（顶点 shader 没在用 `g_ModelViewProjectionMatrix`、或引用了
            // view-projection）⇒ **不换**：顶点仍是单位四边形、MVP 保持原值（见 `base_cardspace_on`）。
            let mut base_card_active = false;
            let mut base_card_diag = String::from("未尝试");
            if base_cards_here {
                if let Some((bvs, bfs)) = base_plain_vs_fs.as_ref() {
                    let (l, card_ok) = self.gl.make_layer_card_vp(
                        bvs,
                        bfs,
                        [base_card_px[0] / 2.0, base_card_px[1] / 2.0],
                    )?;
                    base_card_diag = format!(
                        "card_ok={} has_g_ViewProjectionMatrix={} has_g_AltViewProjectionMatrix={}",
                        card_ok,
                        bvs.contains("g_ViewProjectionMatrix"),
                        bvs.contains("g_AltViewProjectionMatrix"),
                    );
                    if card_ok {
                        let mut l = l;
                        // 与底图趟原构造那条"透传层不翻"修正保持一致（作用域已排除
                        // `fullscreen_layer`；这里只是防将来放宽作用域时漏掉）。
                        if fullscreen_layer
                            && std::env::var("WE_FULLSCREEN_BASE").ok().as_deref() != Some("flip")
                        {
                            l.raw_position = false;
                        }
                        // ⚠ 旧层（单位四边形那份 VAO/VBO）在这里被替换 —— glow 不在 Drop 时释放
                        // GL 对象 ⇒ 开关**开**时每个对象多留一份 VAO + 2 个 VBO（默认关 ⇒ 不产生）。
                        base_layer = l;
                        base_card_active = true;
                    }
                }
            }
            if base_cards_here && dbg {
                eprintln!(
                    "[图层]   WE_BASE_CARDSPACE：id={} 「{}」底图趟顶点空间 = 官方卡片 ±({:.1},{:.1})（对象 size=({:.1},{:.1})，缓冲 {}x{}，{}｜{}）",
                    obj.id, obj.name,
                    base_card_px[0] / 2.0, base_card_px[1] / 2.0,
                    size[0], size[1], lw, lh,
                    if base_card_active { "已换" } else { "保持单位四边形" },
                    base_card_diag
                );
            }
            let comp_name = format!("_rt_imageLayerComposite_{}_b", obj.id);
            let comp = self.new_rt(lw, lh, &comp_name)?;
            // 降采样 RT：把**逻辑**尺寸记成上报尺寸（见 `perf_res_scale_on`）。
            if let Some(rep) = size_rep {
                self.rt_report[comp] = Some(rep);
            }
            if published {
                self.alias_rt(obj.id, comp);
            }
            // 不可见但被图层链接引用的层（见上面 `link_offscreen`）：把它**所有**"画到屏幕"的趟
            // 改道到它的图层链接 RT ⇒ 只喂 RT、完全不碰屏幕（= OWE `EnsureLinkRenderTarget`：
            // 链接 RT 喂的是这层的**最终输出**）。
            // 少了这一步，这类层的收尾/发光趟会漏到画面上：实测 `2482408967` 漏出 "02:13" 数字、
            // 主题1 也有轻微泄漏。`off_rt = None` 时 `off_rt.or(t) == t` ⇒ 逐字原样。
            let off_rt: Option<usize> = if link_offscreen && !vis { Some(comp) } else { None };
            // 链自己的输出 RT：**没有写目标的趟落到这里**，最后整体贴回屏幕。
            // （WE 的语义是 `previous` = 这条链上一趟的输出，不是"永远是图层合成结果" ——
            // 实测这个对象挂了 5 个特效，后面几趟都在读上一趟，指错就会洗白。）
            let chain_name = format!("_rt_chain_{}", obj.id);
            let chain_a = self.new_rt(lw, lh, &chain_name)?;
            let chain_b = self.new_rt(lw, lh, &format!("_rt_chain_{}_b", obj.id))?;
            if let Some(rep) = size_rep {
                self.rt_report[chain_a] = Some(rep);
                self.rt_report[chain_b] = Some(rep);
            }
            let chain_pair = [chain_a, chain_b];
            let lg = tex_extent_on() & !fullscreen_layer & !compose_layer & !published;
            if lg & fit.is_none() & !self.cur_mesh_uvspace { for i in [comp, chain_a, chain_b] { self.rt_logical[i] = Some((size[0] as u32, size[1] as u32)); } }
            // 图层空间下的首趟（Copy）：参照的几何 = **对象自己的场景矩形** + `ortho(0,size)`
            // ⇒ 等价于我们这里的 `scale(size/2)`（顶点本来就是 ±1 的单位框，
            // `obj_mvp = proj·world·scale(size/2)`；把 proj/world 拿掉就正好铺满图层缓冲）。
            // ⚠ 绝不能改成 `full_q`（[-1,1] 单位框 + uv 0..1）：那会**丢掉模型 sprite 的
            // texcoord 子矩形**，美术在缓冲里错位 + 轻微放大。实测（`девка` 单层、0 个特效、
            // 800x450）：`full_q` 版相对画布空间 **dy = -40px**（≈ 层高的 10%），
            // 而 `scale(size/2)` 版 dy = 0（逐像素一致）。
            // 摆位/旋转仍推迟到收尾趟（Screen 几何）。画布空间时保持老行为（`obj_mvp`）。
            // 首趟 MVP 的构造与「斜置四边形」判据见下（提前到这里：V 反向要按同一条判据决定）。
            let copy_world = if std::env::var("WE_LAYER_COPY_WORLD").ok().as_deref() == Some("origin") {
                Mat4::translate(origin)
            } else {
                mvp_world
            };
            let m0 = proj
                .mul(&copy_world)
                .mul(&Mat4::scale(size[0] / 2.0, size[1] / 2.0, 1.0));
            let tf = |x: f32, y: f32| -> (f32, f32) {
                let m = &m0.m;
                let w = m[3] * x + m[7] * y + m[15];
                let w = if w.abs() < 1e-9 { 1.0 } else { w };
                (
                    (m[0] * x + m[4] * y + m[12]) / w,
                    (m[1] * x + m[5] * y + m[13]) / w,
                )
            };
            // 取景框的对角（默认 = 对象矩形的 (−1,−1)/(1,1)）。
            let (qx0, qy0) = fit.map(|b| (b[0], b[2])).unwrap_or((-1.0, -1.0));
            let (qx1, qy1) = fit.map(|b| (b[1], b[3])).unwrap_or((1.0, 1.0));
            let (ax, ay) = tf(qx0, qy0);
            let (bx, by) = tf(qx1, qy1);
            let (cx, ay_dbg) = ((ax + bx) / 2.0, (ay + by) / 2.0);
            // （`3571777169` `цепь перед`(2399)「横贯画面的粗黑硬边棒」）：
            // 判据（数据驱动，不看 id/名字）= **对象四边形的真包围盒 ≠ 那条对角两点跨度**，
            // 也就是「四边形在 NDC 里是斜置的」。轴对齐对象（含 ~180° 的翻转，如
            // 2861113973 的 `Basic Clouds Movement`：转角 −178.4° 时 `bbox == 对角`，比值 ~0）
            // 两者逐位相等 ⇒ 这条门不命中 ⇒ 默认行为不动。
            // 为什么需要它：`layer_copy_mvp` 那条构造「按对象矩形的**对角两点**归一化到 ±1」
            // **只对轴对齐四边形**成立；斜置时另外两个角点逸出那条对角线，归一化用的宽度取错
            // 基准（实测 2399：对角 `bx−ax = −0.3632`、真包围盒宽 `2.0509` ⇒ `sx` 偏 **5.65 倍**），
            // 而且首趟里带着 `m0` 的旋转、收尾趟再乘一次 ⇒ **二次旋转**。两者叠加把 40px 宽的链
            // 糊成一根斜贯画面的粗黑硬边带（贴图 96.4% 的透明像素被插值抹平；与 α 无关）。
            // 修法 = 首趟改用**局部单位阵**（WE 语义：图层缓冲 = 对象的**局部**矩形，摆位/旋转
            // 一律推迟到收尾趟 ⇒ `首趟 ∘ 收尾 = obj_mvp`，与平面路径同一几何），并**取消**那条
            // 为旧构造准备的 V 反向（旧构造的 V 约定与单位阵不同）。
            // 回退开关 **`WE_CHAIN_BASE_LOCAL=0`**。
            let chain_base_local = {
                let (px2, py2) = tf(qx0, qy1);
                let (qx2, qy2) = tf(qx1, qy0);
                let xs = [ax, bx, px2, qx2];
                let ys = [ay, by, py2, qy2];
                let w = xs.iter().cloned().fold(f32::NEG_INFINITY, f32::max)
                    - xs.iter().cloned().fold(f32::INFINITY, f32::min);
                let h = ys.iter().cloned().fold(f32::NEG_INFINITY, f32::max)
                    - ys.iter().cloned().fold(f32::INFINITY, f32::min);
                // 相对比值：只有**强烈斜置**（≥50%）才动 —— 那时旧构造的归一化误差是同阶
                // （2399 = 82% ⇒ `sx` 偏 5.65 倍）。弱斜置（如 2861113973 的
                // `Basic Clouds Movement` 11%、`платье` ~42%）保持旧行为：实测那些对象上
                // 新口径与参照/平面路径的差**没有一致改善**（942 用 Windows `1.jpg` 反而更差），
                // 而它们不属于本次报的缺陷 ⇒ 不动（不搞坏已修好的）。
                let rw = if w.abs() > 1e-9 { (w - (bx - ax).abs()).abs() / w } else { 0.0 };
                let rh = if h.abs() > 1e-9 { (h - (by - ay).abs()).abs() / h } else { 0.0 };
                std::env::var("WE_CHAIN_BASE_LOCAL").ok().as_deref() != Some("0")
                    && rect_chain_space
                    && rw.max(rh) > 0.5
            };
            if std::env::var_os("WE_DEBUG_LAYERPROJ").is_some() {
                eprintln!(
                    "[层内投影] {} id={} size=({:.1},{:.1}) 场景={:?} proj=(m0={:.6},m5={:.6}) 世界={:?} 角点 NDC: (-1,-1)→({:.4},{:.4}) (1,1)→({:.4},{:.4}) 斜置={} 首趟={}",
                    obj.name, obj.id, size[0], size[1], self.scene_size, proj.m[0], proj.m[5], mvp_world.m,
                    ax, ay, bx, by, chain_base_local,
                    if chain_base_local { "局部单位阵" } else { "旧构造" },
                );
            }
            // 图层空间下的首趟（Copy）：参照的几何 = **对象自己的场景矩形** + `ortho(0,size)`
            // ⇒ 等价于我们这里的 `scale(size/2)`（顶点本来就是 ±1 的单位框，
            // `obj_mvp = proj·world·scale(size/2)`；把 proj/world 拿掉就正好铺满图层缓冲）。
            // ⚠ 绝不能改成 `full_q`（[-1,1] 单位框 + uv 0..1）：那会**丢掉模型 sprite 的
            // texcoord 子矩形**，美术在缓冲里错位 + 轻微放大。实测（`девка` 单层、0 个特效、
            // 800x450）：`full_q` 版相对画布空间 **dy = -40px**（≈ 层高的 10%），
            // 而 `scale(size/2)` 版 dy = 0（逐像素一致）。
            // 摆位/旋转仍推迟到收尾趟（Screen 几何）。画布空间时保持老行为（`obj_mvp`）。
            // V 翻转：默认**翻**（`WE_LAYER_VFLIP=0` 可关，用来 A/B）。
            // 实测证据（`/tmp/sway_ab.png` 面板 3/4）：不翻时图层空间画出的人物是**上下翻转**
            // （还带水平镜像）—— 因为 `raw_position` 反向的那套四边形翻的是 V，
            // 而画布空间的 comp 缓冲里美术**本来就是倒的**（`Mat4::ortho` 的 Y 翻转），
            // 图层空间用了同一个 ortho，所以还必须再翻一次 V 才对得上。
            // ⚠ 斜置对象（首趟已换成局部单位阵，V 约定跟着变）**不能再翻**：翻了会把链沿
            // **另一条对角线**摆（实测 2399 翻后跑到画面中部、与平面路径差 1.35%）。
            // `WE_LAYER_UPRIGHT`（默认关，A/B 用）：**不在 uv 上翻，而在几何上翻**
            // 让图层缓冲按**画布朝向**存（row 0 = 画布顶）。理由见 `layer_upright_on` 的注释：
            // uv 翻只镜像"取到的贴图"，缓冲里存的是**镜像的画**，于是链上每一趟里
            // `v_TexCoord`（缓冲 uv）与作者画在**画布 uv** 上的遮罩差一次 V 镜像 —— 这就是
            // 滑板尖端"自发水波"的根（遮罩在尖上被读到 240/255，而同一 uv 的遮罩原像素是 0）。
            // 几何翻 + 缓冲存正 ⇒ 遮罩与内容同空间，**不需要任何遮罩补偿**。
            let layer_upright_geom = layer_upright_on()
                && layer_space
                && !chain_base_local
                && !text_chain_space;
            if atlas_base_h {
                // 底图 = 平铺图集 ⇒ 朝向**显式**决定（不再套 `WE_LAYER_VFLIP` 那条
                // "翻 uv 存镜像"的旧约定，那正是 里遮罩镜像的来源）。
                // 默认**不翻** = 缓冲 row 0 = 作者画布 row 0 ⇒ 链内遮罩按画布 uv 采样即对齐。
                // `WE_UVSPACE_ATLAS_FLIP=1` 存镜像版做 A/B。
                base_layer.raw_position =
                    std::env::var("WE_UVSPACE_ATLAS_FLIP").ok().as_deref() == Some("1");
            } else if layer_upright_geom {
                // 缓冲朝正：底图趟的 uv 不翻（下面用 Y 镜像的 MVP 代替）。
            } else if layer_space
                && !chain_base_local
                && !text_chain_space
                && std::env::var("WE_LAYER_VFLIP").ok().as_deref() != Some("0")
            {
                base_layer.raw_position = !base_layer.raw_position;
            }
            // 诊断（默认关）：显式钉住斜置对象的 V 朝向做 A/B。实测「首趟 ∘ 收尾」的取向只取决于
            // `baseV XOR blitV`；2399 需要**奇数**次镜像（0/1 或 1/0 ⇒ 与平面路径差 0.30%，
            // 偶数次 1.36%）。
            if chain_base_local {
                if let Ok(v) = std::env::var("WE_CHAIN_BASE_VFLIP") {
                    base_layer.raw_position = v == "1";
                }
            }
            // ⚠ 现状（未解）：图层缓冲里**根本没有美术，只有一层平色**（`WE_DUMP_RT` 核对过）。
            // 真因（实验定案）：`scale(size/2)` 把四边形放大到 NDC 的 ±571 —— 远超 [-1,1] 裁剪体
            // ⇒ 屏幕被四边形"内部"盖住、插值 uv 全在 [0,1] 外 ⇒ CLAMP_TO_EDGE 取到贴图边缘色
            // = 一层平色。**在画布空间强行用它也变平**（对照实验），所以这与图层 RT 无关。
            // ⇒ 需要的是**层内投影**（WE 的 Copy 用 `ortho(0,size)`）：把对象的**场景矩形**
            // 映射到 [-1,1]。下面按 `origin ± size*objScale/2` 构造（对象原点=矩形中心）。
            // ⚠ 矩形中心**必须取世界矩阵的平移列**，不能取 `origin`：
            // 实测 `девка` 的 `origin=[2073.5, 942]` 但 `mvp_world` 的平移列是 **(2073.5, 1218)**
            // （`WE_DEBUG_MVP` 直接打出来 "与纯平移一致=false"，差 276 单位 —— 对齐/sprite 位移那一环）。
            // 围着 `origin` 建 ortho 会让内容在缓冲里偏 (1218-942)/942 ≈ 0.29 个半高 ⇒
            // 屏幕上看就是人物整体**偏上**（实测 bbox 画布 y[76,448] vs 图层 y[0,334]）。
            // 精确构造（替掉之前"围着 origin/世界平移建 ortho"的猜法）：
            // 既然画布空间那趟 `M0 = proj·mvp_world·scale(size/2)` 已经**正确**地把矩形放到屏幕上，
            // 层内投影只需把"M0 落在 NDC 的那块矩形"**归一化**到 [-1,1] 即可 —— 与 M0 做什么无关，
            // 旋转/对齐/对象缩放全都自动继承，且不需要任何魔数。
            // 实测校准：原先硬把 ortho 中心放在世界平移(1218)仍差 276 单位（= 世界平移 − origin），
            // 用本构造后偏差归零（MAD 1.56 → 0.23）。
            // 根因（A/B 定案）：**对齐偏移被应用了两次**。
            // 证据：`WE_MVP_OLD=1`（世界矩阵退化为 `translate(origin)`）时
            // · 图层空间渲染 == 画布空间(新世界)渲染（bbox 中心 262 完全一致）
            // · 且「图层(新世界) vs 画布(旧世界)」逐像素差 = 0.0006
            // ⇒ 画布空间只有**一趟**（底图趟）会应用世界矩阵里那个偏移（origin→世界平移，差 276）；
            // 图层空间有**两趟**（Copy + 收尾）都用了它 ⇒ 偏移翻倍。
            // 修法猜想（未成立）：Copy 趟只用对象自己的矩形（不带摆位偏移）。见下方"尝试过但已回退"。
            // ⚠ 尝试过但**已回退**：把 Copy 趟的世界矩阵换成 `translate(origin)`（"只去掉一次对齐偏移"）。
            // 结果：① `девка` 单层渲染**逐位没变**（说明这条路对它是死代码，待查）；
            // ② 全场景从 13.46 **恶化到 21.18** ⇒ 默认改回全量世界矩阵。
            // 开关留着做 A/B：`WE_LAYER_COPY_WORLD=origin` 启用那个变体。
            let dy_shift: f32 = std::env::var("WE_LAYER_DY")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0);
            // （`3571777169` `свеча перед`「火焰区的硬边横条」）：**跨度的钳位把
            // 「镜像对象」整块打死**。
            // 这条首趟 MVP 的构造是「把 m0 落在 NDC 的那块矩形归一化到 ±1」，于是
            // `sx = 2/(bx−ax)`、`sy = 2/(by−ay)`。旧代码写的是 `.max(1e-6)` —— 本意是
            // 「别除零」，但 `.max` 对**负数**也生效：
            // 对象 `scale.x` 为负（镜像；实测 389 = −1.16156）⇒ `bx−ax = −0.1264`
            // ⇒ `.max(1e-6)` 钳成 1e-6 ⇒ **sx = 2×10⁶**。
            // ⇒ 单位框在 x 方向被放大两百万倍，四边形跨出裁剪体、片元 uv 全在贴图外，
            // `CLAMP_TO_EDGE` 让每一列都取到**同一列**贴图 ⇒ 图层缓冲里只剩一列内容被
            // 拉满整块（`WE_DUMP_RT` 实测：209x663 缓冲的**每一列均值完全相同** 122/97/77），
            // 屏幕上就是用户报的「硬边渐变横条」；而平面路径 `WE_RECT_CHAIN=0` 不经过
            // 这条构造 ⇒ 同一对象是完整的柔光蜡烛（A/B 铁证与之一致）。
            // 修法：**跨度取绝对值做零保护，符号原样保留**（= 精确逆）。对非镜像、无旋转的
            // 对象两者逐位相同（`sx·m0` 本来就恒等）；对镜像对象它才第一次成立。
            // 回退开关 **`WE_LAYER_SPAN_SIGN=0`**（回到旧的 `.max(1e-6)` 钳位，逐字节复现缺陷）。
            let sign_span_on = std::env::var("WE_LAYER_SPAN_SIGN").ok().as_deref() != Some("0");
            let span = |e: f32| -> f32 {
                if !sign_span_on {
                    return e.max(1e-6);
                }
                if e.abs() >= 1e-6 {
                    e
                } else if e < 0.0 {
                    -1e-6
                } else {
                    1e-6
                }
            };
            let (sx, sy) = (2.0 / span(bx - ax), 2.0 / span(by - ay));
            let cy = ay_dbg + dy_shift / (by - ay).abs().max(1e-6);
            // （第二轮续：`3571777169` `цепь перед`(2399)「横贯画面的粗黑硬边棒」）：
            // 上面这条「按对象矩形**对角两点** (qx0,qy0)/(qx1,qy1) 归一化」的构造**只对轴对齐
            // 对象**成立。对**带旋转**的对象，另外两个角点 (qx0,qy1)/(qx1,qy0) 会落到那条对角
            // 线之外 ⇒ 归一化用的「宽度」取错了基准：
            // · 实测 2399（`angles.z=2.53357 rad`≈145.2°、`scale=2.32222`）：
            // 对角两点 NDC = (0.8056,1.7277)/(0.4420,−0.9208) ⇒ `bx−ax = −0.3632`；
            // 而四条边围出的**真包围盒**宽 = `1.6493−(−0.4017) = 2.0509`
            // ⇒ `sx = 2/(−0.3632) = −5.506`，正确值应是 `2/2.0509 = 0.975`
            // **水平被多拉伸 5.65 倍**（同时带一次多余的符号翻转）。
            // · 因为首趟 `S·T·m0` 里带着 `m0` 的旋转，收尾趟再乘一次 `m0` ⇒ **二次旋转**：
            // 对象在屏幕上被转 2θ（2399 = −290°）。两者叠加把 40px 宽的链糊成
            // 一根斜贯画面的粗黑硬边带，链节之间的透明缝被插值抹平。
            // 与 α 无关：贴图本身 96.4% 像素 α<8（真值见 `tools`），透明边距没问题；
            // 平面路径 `WE_RECT_CHAIN=0` 不经过这条构造 ⇒ 同一对象是**正常的一串链节**（A/B 铁证）。
            // 修法（= WE 的语义「图层缓冲 = 对象的**局部**矩形、摆位/旋转全推迟到收尾趟」）：
            // 首趟拿掉 `proj·world`（只留「单位框 ±1 本来就是对象矩形」⇒ 单位阵），
            // 旋转/镜像/对齐全部由收尾趟的 `obj_mvp` 一次性施加 ⇒ `首趟 ∘ 收尾 = obj_mvp`，
            // 与平面路径**同一几何**。
            // 判据（数据驱动，不看 id/名字）：**对角两点跨度 ≠ 真包围盒跨度**（即四边形斜置）。
            // 轴对齐对象两者逐位相等 ⇒ 这条门**不会**命中它们 ⇒ 默认行为逐字节不变。
            // 回退开关 **`WE_CHAIN_BASE_LOCAL=0`**（回到旧的 `S·T·m0` 构造）。
            let layer_copy_mvp: [f32; 16] = if atlas_base_h {
                // **单位 MVP** —— 顶点本来就是 ±1 单位四边形、uv 0..1，缓冲 = 作者画布
                // ⇒ 这一趟就是把图集 1:1 铺进缓冲（"平铺图集四边形 + 单位 MVP"）。
                // 绝不能再用下面那条「把对象矩形的 NDC 对角归一化到 ±1」的构造：它带
                // `m0` 的旋转/缩放（以及网格取景框的 2539x3570 跨度）⇒ 缓冲 ≠ 画布。
                full_q
            } else if text_chain_space {
                // `WE_TEXT_CHAIN_LAYER`：文字层的**首趟不用归一化**，照旧用 `obj_mvp`
                // （= 画布空间那条路的同一个摆放矩阵）。缓冲变大只是让"1 缓冲 px = 1 画布 px"，
                // 字形在缓冲里的**相对位置**与上屏时逐像素相同；收尾趟用 `full_q` 原样搬回屏幕
                // ⇒ **首趟∘收尾 = 画布空间那条路**，落点/朝向零变化（这是"只改 T0 网格、
                // 不改几何"的最小改动）。见 `text_chain_layer_on` 的文档注释。
                obj_mvp
            } else if chain_base_local {
                Mat4::identity().transposed().column_major()
            } else if rect_chain_space {
                // 非网格对象：用 **m0 的精确 2D 仿射逆** 作首趟 MVP（而不是上面那套
                // 「按对角两角点缩放」的近似）。理由：对角两角点只保证 (-1,-1)/(1,1) 两个
                // 角点落在 ±1，**带旋转/非等比缩放**的对象（实测 `платье` angles.z=0.102、
                // scale 0.849/0.753）另外两个角点会落到 ±1 之外 ⇒ 美术被**硬裁**（实测 bbox
                // 宽 152→100px、整体错位 (−33,−60)px）。精确逆把「对象自己的矩形」原样搬进
                // 缓冲，收尾趟再乘 m0 摆回去 ⇒ 首趟∘收尾 = 恒等：**落点与画布路径逐像素一致**，
                // 而链上各趟的 UV 空间 = 对象自己的矩形 ⇒ 特效幅度按对象尺寸（= WE 的语义）。
                // ⚠ **实测作废**：`WE_RECT_CHAIN=1` 下用精确逆反而更差 ——
                // `крестик 5 id216/222`、`крестик 3 id1309` 整块**消失**（footprint=None）、
                // `волосики`/`Basic Clouds` 错位 30~285px。原因未查清（疑与基底四边形的
                // 顶点单位/`WE_LAYER_VFLIP` 的朝向约定耦合）。⇒ **回退**到旧的近似构造，
                // 它对绝大多数对象落点 ≤4px（实测 8 个对象里 7 个），只有带旋转的对象
                // （`платье`）残留 (−33,−60)px 的偏差（已如实记录，留给下一轮）。
                let _ = (sx, sy, cx, cy);
                Mat4::scale(sx, sy, 1.0)
                    .mul(&Mat4::translate([-cx, -cy, 0.0]))
                    .mul(&m0)
                    .transposed()
                    .column_major()
            } else {
                Mat4::scale(sx, sy, 1.0)
                    .mul(&Mat4::translate([-cx, -cy, 0.0]))
                    .mul(&m0)
                    .transposed()
                    .column_major()
            };
            // uv-空间方案：收尾趟要按底图趟的 uv 朝向读回缓冲 ⇒ 记下它
            // `WE_LAYER_UPRIGHT`：缓冲存"正"的画 ⇒ 底图趟的 MVP 在 y 上取负
            // （翻**几何**而不是翻 uv；见 `layer_upright_on`）。column-major 的 y 输出行 = 1,5,9,13。
            let mut layer_copy_mvp = layer_copy_mvp;
            if layer_upright_geom {
                for i in [1usize, 5, 9, 13] {
                    layer_copy_mvp[i] = -layer_copy_mvp[i];
                }
            }
            self.cur_mesh_base_flip = base_layer.raw_position;
            // K1：这一趟写进的是**图层自己的缓冲**（`comp`），但 shader 里的 A 要的是
            // "到目前为止的**场景**合成图"（官方 `_rt_FullFrameBuffer`）⇒ 绑"画面拷贝"，
            // 与无特效那条路**同一机制**（拷贝在趟循环里做）。有特效时画面在链尾才落进场景，
            // 所以这里不会有 feedback。
            let mut base_units: Vec<(u32, Src)> = vec![(0, base_src)];
            if let Some((_, slot, copy, src)) = cbm_plan {
                base_units.push((slot, Src::Rt(copy)));
                self.cbm_passes.insert(self.passes.len(), (copy, src));
            }
            // 挂骨骼：这一趟（链的**底图趟**）是不是"把这一层摆进场景"的那一趟？
            // 判据 = 下面 `mvp:` 那一支**同一套表达式**（图层空间 + 合成层 ⇒ `obj_mvp`；
            // 画布空间且没被 `WE_BASE_MVP=layer` / 合成透传抢走 ⇒ `obj_mvp`）。命中才登记逐帧更新。
            let base_is_place = if layer_space {
                compose_space
            } else {
                std::env::var("WE_BASE_MVP").ok().as_deref() != Some("layer")
                    && !(compose_layer && std::env::var("WE_COMPOSE").ok().as_deref() != Some("obj"))
            };
            // `WE_PERF_SCISSOR`：这一层的底图趟下标 + 它拥有的链 RT ⇒ 开屏后探内容包围盒。
            if perf_scissor_on() {
                self.bbox_jobs
                    .push((self.passes.len(), vec![comp, chain_a, chain_b]));
            }
            self.passes.push(Pass {
                layer: base_layer,
                target: Some(comp),
                units: base_units,
                scalars: obj_scalars.clone(),
                mvp: {
                    let base_mvp_raw: [f32; 16] = if layer_space {
                        // 合成/透传层：`composelayer.vert` 用 `MVP·a_Position` 决定**采样**位置
                        // ⇒ 缓冲 = 对象矩形时必须给 `obj_mvp`（把单位框映到对象矩形的**场景 NDC**，
                        // 片元于是采到「合成图在对象矩形那一块」的 1:1 拷贝）。
                        // 绝不能沿用 `layer_copy_mvp` —— 那是「把对象矩形归一化到 ±1」，会让
                        // 整张合成图缩进对象矩形（正是当初把这类层排除出图层空间的原因）。
                        if compose_space {
                            obj_mvp
                        } else {
                            layer_copy_mvp
                        }
                    } else if std::env::var("WE_BASE_MVP").ok().as_deref() == Some("layer") {
                        layer_copy_mvp
                    } else if compose_layer
                        && std::env::var("WE_COMPOSE").ok().as_deref() != Some("obj")
                    {
                    // 同上面那条：合成/透传层的底图趟必须是**整屏 1:1 采样**（单位阵）。
                    full_q
                } else {
                    obj_mvp
                };
                    // `WE_BASE_CARDSPACE`：底图趟顶点已换成卡片网格 `±size/2` ⇒ MVP 乘
                    // `diag(2/size)` 使 `M(A·D⁻¹)·v_card = M(A)·v_unit`（等价关系）
                    // `base_cardspace_on`）。只有 `card_ok` 已换几何时才乘 —— 否则照旧用原值。
                    if base_card_active {
                        base_card_mvp(base_mvp_raw, base_card_px)
                    } else {
                        base_mvp_raw
                    }
                },
                // 网格（puppet）图层的底图趟**必须沿用材质的混合方式**（本场景 =
                // `translucent`），不能一律「覆盖」：MDLV 网格是**多个各自独立蒙皮的碎块**
                // 拼成的，装配后碎块之间**互相重叠**（关节处大腿片 / 小腿片交叠）。
                // 用「覆盖」画时，后画那片自己的**透明边缘**会把先画那片的皮肤**抹掉**
                // 实测膝关节处一条 1075px 的空白带、肘关节也有（用户报的"关节空白"）。
                // 逐像素复算证明：那些像素「先画的那片」采样到的是**不透明皮肤**（Python
                // 按同一装配/归一化/uv 重算 = 不透明），只是被后画的透明片元盖掉了。
                // alpha 混合下透明片元不改动目标 ⇒ 关节缝消失；不透明处结果不变。
                // 非网格图层保持原来的「覆盖」（其它壁纸零影响）。
                // `WE_MESH_BASE_BLEND=opaque` 可退回旧行为做 A/B。
                blend: if atlas_base_h {
                    // 底图趟是**整张图集的一次拷贝**（无重叠碎块）⇒ 必须 Opaque。
                    // 用材质的 translucent 混合会让半透明像素在透明底上被 alpha 预乘变暗。
                    gl_ctx::Blend::Opaque
                } else if (puppet_mesh_h.is_some() && puppet_on_h)
                    && std::env::var("WE_MESH_BASE_BLEND").ok().as_deref() != Some("opaque")
                {
                    blend
                } else {
                    gl_ctx::Blend::Opaque
                },
                // uv-空间方案：RT 里放的是**静态展开图**（不随时间变）⇒ 底图趟没有网格动画；
                // 逐帧装配改由**收尾合成趟**承担（见下方 composite）。
                mesh_anim: if self.cur_mesh_uvspace { None } else { mesh_anim_h.take() },
            });
            // 遮罩烘焙趟必须用**这一趟的 MVP**（同一几何变换 ⇒ 遮罩跟着网格走）。
            if self.cur_mask_warp_src.is_some() {
                self.cur_mask_warp_mvp = self.passes.last().map(|p| p.mvp);
                self.cur_mask_warp_blend = self
                    .passes
                    .last()
                    .map(|p| p.blend)
                    .unwrap_or(gl_ctx::Blend::Opaque);
                self.cur_mask_warp_base = Some(self.passes.len() - 1);
            }
            // 相机视差：画布空间时底图趟 = 放进场景的那一趟；图层空间时底图趟只是
            // "画布 → 图层缓冲"的搬运（真正的摆放是下面收尾趟），别动它。
            if let Some(pi) = par_info {
                if !layer_space
                    && !(compose_layer && std::env::var("WE_COMPOSE").ok().as_deref() != Some("obj"))
                {
                    self.parallax_passes.insert(self.passes.len() - 1, pi);
                }
            }
            // 挂骨骼：底图趟若就是"摆进场景"那一趟 ⇒ 登记逐帧（判据见上面的 `base_is_place`）
            if base_is_place {
                AttachHit::register(
                    &mut self.attach_passes,
                    &attach_hit,
                    attach_q_l,
                    obj_mvp_m,
                    self.passes.len() - 1,
                );
            }
            // ── `g_EffectTextureProjectionMatrixInverse` 的真实投影链 ──
            // 问题：跟 LWE 一样把它硬编码成单位阵（`CPass.cpp:881-882`）。单位阵只在
            // 「链的输入纹理空间 == 屏幕」时才对。**图层空间**那条路（`WE_RECT_CHAIN=1`，
            // **桌面内核正是这个环境**）的链缓冲 = 对象自己的矩形，而且链内趟采样 RT 时
            // 天然带一次 V 反向（见上面 2568 行的注释）⇒ 单位阵会把屏幕 NDC 当纹理 UV 用
            // ⇒ **鼠标 → 透视区落点整体上下镜像**（实测 640x360/1920x1080：拟合斜率
            // 0.9999，R²=0.9996）。画布空间那条路不镜像（斜率 +1.0001），所以只在
            // 图层空间这条路上修。
            // 真实链（跟 OWE，`SpecNames.cppm:73-79` + `UniformSource.cpp:641-660`：
            // `effect_model = layer_model · Scaling(size/2)`，再乘激活相机的 view-projection）
            // 它算出来的正好是「**屏幕 NDC → 本趟输入纹理 UV**」的映射，而这在数学上
            // 等于**本趟四边形摆放矩阵的逆**：`out = 0.5·(ETVPi·p).xy / w`，
            // 而 shader 里随后又把这个值当「纹理坐标 - 0.5」用（`xray.frag:32-42` 的
            // 精灵中心偏移），两处的 2/0.5 恰好抵消 ⇒ ETVPi = P⁻¹（P = 摆放矩阵）。
            // 验证：1:1 cover 下 P = diag(1,-1) ⇒ ETVPi = 同 ⇒ 与「单位阵 + V 反向」
            // 逐项一致；非 1:1 取景下 P 里带 cover/contain 的比例 ⇒ 落点跟着取景走。
            // `WE_ETVP=identity` 回旧行为做 A/B（默认 = 真实链）。
            let etvp_real = std::env::var("WE_ETVP").ok().as_deref() != Some("identity");
            // 把缓冲摆进场景的那个矩阵（= 下面收尾趟 `blit_mvp` 用的同一个；带网格取景框时
            // 再叠一次「取景框中心平移 · 半尺寸缩放」，与 WEB 的 Screen 几何同构）。
            let blit_place_m: Mat4 = match fit {
                Some(b) => obj_mvp_m
                    .mul(&Mat4::translate([
                        (b[0] + b[1]) * 0.5,
                        (b[2] + b[3]) * 0.5,
                        0.0,
                    ]))
                    .mul(&Mat4::scale((b[1] - b[0]) * 0.5, (b[3] - b[2]) * 0.5, 1.0)),
                None => obj_mvp_m,
            };
            // 只有图层空间的链需要它（画布空间的缓冲就是屏幕 ⇒ 单位阵本来就对）。
            // 上传口径与 `obj_mvp` 完全一致（`transposed.column_major`，见 gl_ctx::t4）。
            // y 取反（`diag(1,-1)`）：这不是随手补的符号，是**这套 shader 的坐标约定**推出来的。
            // 设 P = 摆放矩阵，则「某个屏幕 NDC q 处的片元」的纹理坐标
            // τ(q) = (P⁻¹q + 1)/2；xray.frag:32-42 把精灵中心放在
            // `σ(q*) = out + 0.5`、而 `σ = (τ.x, 1−τ.y)`（34-35 行自己翻了 y）
            // ⇒ 把 `out = 0.5·(ETVPi·p).xy` 代进去解出 ETVPi = diag(1,−1)·P⁻¹。
            // 实测（见 artifacts 的报告）：不给这个 y 反号时，1:1/cover/contain 三种取景下
            // 「鼠标px_y → 透视区质心px_y」的拟合斜率都是 **−1**（镜像）；加上之后三种取景
            // 都回到 **+1**。
            let mut etvp_flip_y = Mat4::identity();
            etvp_flip_y.m[5] = -1.0;
            let etvp_inv: Option<[f32; 16]> = if layer_space && etvp_real && !text_chain_space {
                blit_place_m
                    .invert()
                    .map(|inv| etvp_flip_y.mul(&inv).transposed().column_major())
            } else {
                None
            };
            if std::env::var_os("WE_DEBUG_ETVP").is_some() {
                eprintln!(
                    "[ETVP] {} id={} layer_space={layer_space} 真实链={} P=({:.6},{:.6},{:.6},{:.6}) ETVPi={:?}",
                    obj.name,
                    obj.id,
                    etvp_inv.is_some(),
                    blit_place_m.m[0],
                    blit_place_m.m[5],
                    blit_place_m.m[12],
                    blit_place_m.m[13],
                    etvp_inv.map(|v| format!(
                        "[{:.5},{:.5},{:.5},{:.5}]",
                        v[0], v[5], v[12], v[13]
                    ))
                );
            }
            let mut prev = Src::Rt(comp);
            // 这条链**开始**时的输入 = 本层的底图（合成缓冲）。下面每个特效开始时会刷新
            // `cur_effect_input`（特效内的 `"previous"` 指的是那个，不是链的当前输出）。
            let _chain_base = prev;
            // `WE_FUSE_SHIFT`：本层特效链的趟区间起点（链尾再把这段里的连续纯位移趟折成一趟）
            let fuse_start = self.passes.len();

            // 二分定位用：只跑每个对象特效链的前 N 个（`WE_MAX_EFFECTS=0` ⇒ 只剩底图）
            let max_eff = std::env::var("WE_MAX_EFFECTS")
                .ok()
                .and_then(|s| s.parse::<usize>().ok());
            // 网格对象现在**可以带特效链正常渲染**（去掉了 WE_MESH_SKIP_CHAIN）：
            // 原先之所以要跳过链，是因为链内趟用了网格几何 + 单位阵 MVP（把网格顶点当裁剪空间）
            // ⇒ 溢出的顶点在 ±1 被硬裁。查参照实现（linux-wallpaperengine `CImage.cpp:804-816`）：
            // **只有首趟用网格几何**（`m_puppetSpacePosition` + `m_modelViewProjectionCopy`），
            // 链内的每一趟都用 `getPassSpacePosition` 的**全屏 [-1,1] 四边形** +
            // `m_modelViewProjectionPass`（单位阵）。我们照此修正（链内趟一律 `make_layer`）
            // ⇒ 网格只决定首趟的几何，链内趟与平面图层**完全同一条路**。
            for (ei, eff) in obj.effects.iter().enumerate() {
                // 这个特效要从"当前画面"开始处理 ⇒ 记下输入，供本特效内 `"previous"` 用
                self.cur_effect_input = prev;
                if let Some(m) = max_eff {
                    if ei >= m {
                        break;
                    }
                }
                // 壁纸参数驱动的显隐（`visible`）：WE 里 `visible=false` 的特效**不参与合成**，
                // 且 `visible` 可以是用户属性绑定 `{"user":"<属性名>","value":<默认值>}` ——
                // 用户在 WE 界面改该属性 ⇒ 这个特效开/关（对齐 OWE `SceneNodeLayer.cpp:108`
                // 的 `if (eff->runtime_visible) resolve_effect(...)`；Almamu `CImage.cpp:645` 同）。
                // 我们复现"用户改属性"的开关见 `we_scene::scene::userprop`（`WE_PROP_*` / `--prop`）。
                // 档位选择见上面 `effect_visible_mode` 的注释（默认只处理 user 绑定）。
                let hide_eff = match effect_visible_mode {
                    0 => false,
                    1 => eff
                        .visible
                        .as_ref()
                        .map(|v| v.user.is_some() && !v.resolve())
                        .unwrap_or(false),
                    _ => !eff.is_visible(),
                };
                if hide_eff {
                    if dbg {
                        let who = match eff.visible.as_ref().and_then(|v| v.user_name()) {
                            Some(n) => format!("用户属性 {n}"),
                            None => "visible=false".to_string(),
                        };
                        eprintln!("[图层]   ⏭ {}：特效 {ei}（{}）不可见（{who}）", obj.name, eff.file);
                    }
                    continue;
                }
                let Some(ef_bytes) = pkg.get(pkg_bytes, &eff.file) else {
                    self.note_skipped(
                        &eff.file,
                        "特效文件在包里找不到（也无 assets 回退）⇒ 整个特效不参与合成",
                        None,
                    );
                    continue;
                };
                let ef: shader::EffectFile =
                    match we_scene::json::from_slice::<shader::EffectFile>(ef_bytes) {
                        Ok(ef) => ef,
                        Err(e) => {
                            // 必须带上 serde 的**原文**（含 `file:line:col`）：光说"字段形态
                            // 不认识"没法诊断到底是哪个字段/哪种写法。方言怀疑先试
                            // `WE_JSON_COMMENTS=1` / `WE_JSON_NULLMAP=1`（官方出处）
                            // `we_scene::json` 模块注释）。
                            self.note_skipped(
                                &eff.file,
                                &format!(
                                    "effect.json 反序列化失败（字段形态不认识）：{e}\
                                     （方言开关见 we_scene::json：WE_JSON_COMMENTS / WE_JSON_NULLMAP）"
                                ),
                                None,
                            );
                            continue;
                        }
                    };
                // 该特效用到的离屏目标（scale=4 ⇒ 四分之一分辨率）。**相对图层尺寸**（WE 的
                // FBOProvider 就是按图层 FBO 的比例建），不是画布。
                for f in &ef.fbos {
                    let sc = if f.scale > 0.0 { f.scale } else { 1.0 };
                    // 调试旋钮 `WE_QSSCALE`：把这些命名中间缓冲整体放大/缩小（判"降采样倍数对不对"）。
                    let qs: f32 = std::env::var("WE_QSSCALE")
                        .ok()
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(1.0);
                    // 已转默认：用**对象自己的尺寸**（参照实现 `CImage.cpp:653`
                    // `fboProvider->create(*fbo, …, this->getSize)` 就是这么来的；全屏后处理层的
                    // 对象尺寸 = 场景画布）。旧行为（渲染目标尺寸）会让模糊半径偏大约 2.4 倍，
                    // 人群/骑士那类细节被糊掉 —— 实测与 Windows 预览 + linux 参照帧两路互证。
                    // 要回退旧行为：`WE_FX_FBO=target`。
                    let (bw, bh) = if std::env::var("WE_FX_FBO").ok().as_deref() == Some("target") {
                        (lw as f32, lh as f32)
                    } else {
                        (size[0].abs().clamp(1.0, 8192.0), size[1].abs().clamp(1.0, 8192.0))
                    };
                    let rw = ((bw * qs / sc).round() as u32).max(1);
                    let rh = ((bh * qs / sc).round() as u32).max(1);
                    self.new_rt(rw, rh, &f.name)?;
                }

                for (i, ep) in ef.passes.iter().enumerate() {
                    let ov = eff.passes.get(i);
                    let Some(ebb) = pkg.get(pkg_bytes, &ep.material) else {
                        self.note_skipped(
                            &eff.file,
                            &format!("趟材质 {} 在包里找不到", ep.material),
                            Some(i),
                        );
                        continue;
                    };
                    let em: shader::MaterialFile =
                        match we_scene::json::from_slice::<shader::MaterialFile>(ebb) {
                            Ok(m) => m,
                            Err(e) => {
                                self.note_skipped(
                                    &eff.file,
                                    &format!("趟材质 {} 反序列化失败：{e}", ep.material),
                                    Some(i),
                                );
                                continue;
                            }
                        };
                    let Some(mp) = em.passes.first() else {
                        self.note_skipped(
                            &eff.file,
                            &format!("趟材质 {} 的 passes 为空", ep.material),
                            Some(i),
                        );
                        continue;
                    };
                    let v_raw = match shader::load_shader(pkg, pkg_bytes, assets, &mp.shader, "vert") {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("[场景] 跳过特效趟 {i}（{}）：{e}", mp.shader);
                            self.note_skipped(
                                &eff.file,
                                &format!("shader {} 读不到 vert：{e}", mp.shader),
                                Some(i),
                            );
                            continue;
                        }
                    };
                    let f_raw = match shader::load_shader(pkg, pkg_bytes, assets, &mp.shader, "frag") {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("[场景] 跳过特效趟 {i}（{}）：{e}", mp.shader);
                            self.note_skipped(
                                &eff.file,
                                &format!("shader {} 读不到 frag：{e}", mp.shader),
                                Some(i),
                            );
                            continue;
                        }
                    };
                    // ⚠ **两个阶段都要解析参数映射**：WE 常把关键参数只声明在**顶点着色器**里
                    // （`foliagesway` 的 `g_Ratio`/`g_Strength`/`g_Direction` 就是），
                    // 只读片元的话这些 uniform 永远是 0 —— 而 shader 里会拿它做 `1.0 / aspect`
                    // 这类运算 ⇒ Inf/NaN ⇒ 采样坐标烂掉 ⇒ 整层发黑（实测人物层 52 → 0）。
                    let mut meta = shader::parse_shader_meta(&f_raw);
                    let vmeta = shader::parse_shader_meta(&v_raw);
                    let vmeta_keys = vmeta.clone(); // 常量键→uniform 要连顶点 shader 一起查
                    meta.uniforms.extend(vmeta.uniforms);
                    meta.combos.extend(vmeta.combos);

                    // combo：shader 声明的默认值，被这一趟的覆盖改写
                    let mut combos: Vec<(String, i32)> = meta
                        .combos
                        .iter()
                        .map(|c| (c.combo.clone(), c.default))
                        .collect();
                    // -1：材质趟自己声明的 `passes[].combos`（官方 `VERTICAL` / `VERSION` /
                    // `REFRACT` / `LIGHTING` …）。优先级 = 高于 shader 的 `[COMBO]` 默认值、
                    // 低于场景级 `objects[].effects[].passes[].combos`（下面 `ov.combos` 的覆盖）。
                    // 开关 `WE_MAT_COMBOS=1`，**默认关** ⇒ 一个字节都不动。
                    if mat_combos_on() {
                        let n1 = merge_pass_combos(&mut combos, &mp.combos, &[&f_raw, &v_raw]);
                        let n2 = merge_pass_combos(&mut combos, &ep.combos, &[&f_raw, &v_raw]);
                        if n1 + n2 > 0 && std::env::var_os("WE_DEBUG_PASSES").is_some() {
                            eprintln!(
                                "[材质combo] {} 趟{i}：材质 {} 条 + effect {} 条 ⇒ {:?}",
                                mp.shader, n1, n2, combos
                            );
                        }
                    }

                    // 该趟实际会绑上贴图的槽位 ⇒ 把注解里的 combo 打开（`{"combo":"MASK"}` ⇒ MASK=1）。
                    // 不这么做：`#if MASK` 那段不编译，遮罩形同不存在（实测 tint 特效会把整层染黑）。
                    let mut bound: Vec<u32> = Vec::new();
                    for (k, t) in mp.textures.iter().enumerate() {
                        if !t.is_empty() {
                            bound.push(k as u32);
                        }
                    }
                    if let Some(o) = ov {
                        for (k, t) in o.textures.iter().enumerate() {
                            if t.as_deref().map(|s| !s.is_empty()).unwrap_or(false) {
                                bound.push(k as u32);
                            }
                        }
                    }
                    for b in &ep.bind {
                        if b.index >= 0 {
                            bound.push(b.index as u32);
                        }
                    }
                    for u in &meta.uniforms {
                        let Some(rest) = u.name.strip_prefix("g_Texture") else { continue };
                        let Ok(k) = rest.trim_end_matches("_b").parse::<u32>() else { continue };
                        let is_fb = matches!(
                            u.prop.as_str(),
                            "ui_editor_properties_framebuffer" | "Prev" | "ui_editor_properties_prev"
                        );
                        // 注解自带默认资源（util/white 之类）或标注成 framebuffer 的槽位，也算"绑了"
                        if is_fb || !u.default.is_empty() {
                            bound.push(k);
                        }
                    }
                    for (k, v) in shader::combos_for_units(&meta, &bound) {
                        if !combos.iter().any(|(n, _)| *n == k) {
                            combos.push((k, v));
                        }
                    }

                    if let Some(o) = ov {
                        for (k, v) in &o.combos {
                            let iv = *v as i32;
                            match combos.iter_mut().find(|(n, _)| n == k) {
                                Some(slot) => slot.1 = iv,
                                None => combos.push((k.clone(), iv)),
                            }
                        }
                    }
                    // 诊断开关（默认关闭 ⇒ 逐字节不变）：`WE_FX_COMBOS="<shader子串>:K=V;K=V"`
                    // 只给 shader 名匹配的趟覆盖**编译期 combo**。用途：`RAYCORNER/RAYMODE` 这种
                    // "扇区几何"是 combo（烘进源码），没有它就只能改场景数据；有它就能逐个 A/B
                    // 而完全不碰默认行为。实测（3704273480 的 `光束 - 角`）：RAYCORNER 0/1/2/3
                    // 只把扇区转个方向，棕榈冠区的贡献**都是 0.05 luma**（见 NUMBERS.txt 第 3 节）。
                    if let Ok(spec) = std::env::var("WE_FX_COMBOS") {
                        if let Some((sub, kv)) = spec.split_once(':') {
                            if mp.shader.contains(sub) {
                                for pair in kv.split(';') {
                                    if let Some((k, v)) = pair.split_once('=') {
                                        if let Ok(iv) = v.trim().parse::<i32>() {
                                            let k = k.trim();
                                            match combos.iter_mut().find(|(n, _)| n == k) {
                                                Some(slot) => slot.1 = iv,
                                                None => combos.push((k.to_string(), iv)),
                                            }
                                        }
                                    }
                                }
                                eprintln!("[诊断] WE_FX_COMBOS ⇒ {} combos={:?}", mp.shader, combos);
                            }
                        }
                    }
                    if dbg {
                        let unit_desc: Vec<String> = {
                            let mut v: Vec<String> = Vec::new();
                            for (k, t) in mp.textures.iter().enumerate() {
                                if !t.is_empty() {
                                    v.push(format!("m{k}={t}"));
                                }
                            }
                            if let Some(o) = ov {
                                for (k, t) in o.textures.iter().enumerate() {
                                    if let Some(s) = t.as_deref().filter(|s| !s.is_empty()) {
                                        v.push(format!("o{k}={s}"));
                                    }
                                }
                            }
                            v
                        };
                        eprintln!(
                            "[图层]     特效趟{i} {} combos={:?} 槽位={:?} 贴图={:?}",
                            mp.shader, combos, bound, unit_desc
                        );
                    }
                    let (vs, mut fs) = shader::compose_stages_with(&v_raw, &f_raw, &combos);
                    // ── `WE_LINK_RT_CONTENT`：链接 RT 当 blend 输入时的**内容缩放**（消费侧）──
                    // 推导见 [`link_rt_content_scale`]：屏上"屏幕像素/遮罩纹素" = `blendscale·obj.scale_y·k`。
                    // 要让遮罩里的**世界内容**与场景 1:1，就得把采样跨度放大 `1/f`（`f` = 推导值）。
                    // 只改 **Texture1（blend 的遮罩槽）** 的 uv，并绕 `uv=0.5` 缩放 ⇒ `blendoffset`
                    // 定出的取景中心**不动**（RT 侧缩放做不到这一点：实测会把内容搬出字窗）。
                    // `WE_LINK_RT_CONTENT=0/1/off` ⇒ 一字节不改（护栏基线）。
                    let mut vs = vs;
                    {
                        let slot1 = ov
                            .and_then(|o| o.textures.get(1))
                            .and_then(|t| t.as_deref())
                            .map(str::to_string)
                            .or_else(|| mp.textures.get(1).cloned());
                        let is_link = slot1
                            .as_deref()
                            .map(|s| s.starts_with("_rt_imageLayerComposite_"))
                            .unwrap_or(false);
                        if is_link {
                            let _bs = ov
                                .and_then(|o| o.constant_shader_values.get("blendscale"))
                                .and_then(|v| v.as_f64())
                                .map(|v| v as f32);
                            let sy = we_scene::scene::parse_vec3(&obj.scale)
                                .map(|v| v[1])
                                .unwrap_or(1.0);
                            let _ = sy;
                            // 默认 f = 1.0（官方：没有内容缩放）。`WE_LINK_RT_CONTENT=<f>` 只是 A/B。
                            if let Some(f) = link_rt_content_scale() {
                                // ── `WE_BLEND_SPACE`（默认 `layer`）：把**画布 UV** 换成**图层 UV** ──
                                // 官方 `blend.vert` 里 `v_TexCoord.xy` 是**图层的** uv：这一趟在
                                // WE 里跑在该图层的缓冲区上（缓冲 = `source_node.Size`，`uv` 0..1
                                // 正好铺满图层矩形）。我们这条路上这一趟跑在**画布**四边形上
                                // （实测 `WE_FX_PROBE=uv`：uv 0..1 铺满整屏）⇒ 同一个 `a_TexCoord`
                                // 被当成图层 uv 用，整体被拉伸 `画布/图层` 倍、且锚点从"图层中心"
                                // 变成了"画布中心"（= 屏中心）。
                                // 换算（只用场景数据，无壁纸专属常数）：
                                // `anchor` = 图层中心在画布 uv 里的位置 = `0.5 + origin/canvas`
                                // （y 轴向上 ⇒ 屏幕 y 用 `0.5 − origin.y/canvas_h`）；
                                // `ratio` = `canvas / (obj.size · obj.scale)`。
                                // 于是 `u_layer = 0.5 + (uv − anchor) · ratio`，再接原来的内容缩放
                                // `u' = 0.5 + (u_layer − 0.5)/f` ⇒
                                // `u' = 0.5 + (uv − anchor) · ratio / f`。
                                // `WE_BLEND_SPACE=canvas`（或 `0`）⇒ anchor=0.5、ratio=1 ⇒ 逐字节回旧。
                                let (anchor, ratio) = if blend_space_on() {
                                    let (caw, cah) = (self.scene_size.0.max(1.0), self.scene_size.1.max(1.0));
                                    let sz = we_scene::scene::parse_vec2(&obj.size)
                                        .unwrap_or([0.0, 0.0]);
                                    let sc = we_scene::scene::parse_vec3(&obj.scale)
                                        .unwrap_or([1.0, 1.0, 1.0]);
                                    let org = we_scene::scene::parse_vec3(&obj.origin)
                                        .unwrap_or([0.0, 0.0, 0.0]);
                                    let lw = (sz[0] * sc[0].abs()).abs();
                                    let lh = (sz[1] * sc[1].abs()).abs();
                                    if lw > 1.0 && lh > 1.0 {
                                        (
                                            [0.5 + org[0] / caw, 0.5 - org[1] / cah],
                                            [caw / lw, cah / lh],
                                        )
                                    } else {
                                        ([0.5, 0.5], [1.0, 1.0])
                                    }
                                } else {
                                    ([0.5, 0.5], [1.0, 1.0])
                                };
                                let kx = ratio[0] / f;
                                let ky = ratio[1] / f;
                                let cx0 = 0.5 - anchor[0] * kx;
                                let cy0 = 0.5 - anchor[1] * ky;
                                // `WE_BLEND_SPACE=0` ⇒ **逐字**用旧那两行（`0.5 + (v-0.5)/f`），
                                // 不是"代数等价的新写法" —— 浮点字面量的 ulp 差异会让旧基线
                                // 不再逐字节复现。
                                let space_on = blend_space_on();
                                let a = "v_TexCoord.zw = vec2(v_TexCoord.x * g_Texture1Resolution.z / g_Texture1Resolution.x,";
                                let b = "v_TexCoord.y * g_Texture1Resolution.w / g_Texture1Resolution.y);";
                                if vs.contains(a) && vs.contains(b) {
                                    let (sx, sy) = if space_on {
                                        (
                                            format!("v_TexCoord.zw = vec2(({cx0:.8} + v_TexCoord.x * {kx:.8}) * g_Texture1Resolution.z / g_Texture1Resolution.x,"),
                                            format!("({cy0:.8} + v_TexCoord.y * {ky:.8}) * g_Texture1Resolution.w / g_Texture1Resolution.y);"),
                                        )
                                    } else {
                                        (
                                            format!("v_TexCoord.zw = vec2((0.5 + (v_TexCoord.x - 0.5) / {f:.6}) * g_Texture1Resolution.z / g_Texture1Resolution.x,"),
                                            format!("(0.5 + (v_TexCoord.y - 0.5) / {f:.6}) * g_Texture1Resolution.w / g_Texture1Resolution.y);"),
                                        )
                                    };
                                    vs = vs.replace(a, &sx).replace(b, &sy);
                                    if dbg {
                                        eprintln!(
                                            "[链接RT] 消费层 {} 遮罩 UV：anchor=({:.5},{:.5}) ratio=({:.5},{:.5}) 内容缩放 f={:.5}（scale_y={:.5}）",
                                            obj.id,
                                            anchor[0], anchor[1], ratio[0], ratio[1], f, sy
                                        );
                                    }
                                }
                                // ── `WE_BLEND_YOFF`（默认 **`official` = 原文照抄**；`flip` = A/B 实验）──
                                // `blend.vert` 的 y 是 **WE 的 y-up 图层 uv**；我们这条路上的 uv 是
                                // 画布 **y-down**（`WE_FX_PROBE=uv` 实测 (0,0) 在左上），链接 RT 的内容
                                // 又是**翻转**存的（`WE_DUMP_RT`：RT alpha bbox y 26..1475 vs 源 0..1449）。
                                // 本次实测：把 `(T0.zw−T1.zw)*0.5` 在 y 上取反 ⇒ 字内人物**再上移 77 px**
                                // （眼部 y 161 → 84，win 是 247）⇒ 方向相反，**默认不用**。
                                // 真正的 86 px 纵向偏差
                                // 是**纯平移**，比例已由 `m = bs·quad_h/T0_h` 定死，疑似 WE 的文字 RT
                                // 比 `obj.size` 高（文字墨水锚定 ⇒ uv 平移而 m 不变）。
                                if blend_yoff_on() {
                                    let pat = "v_TexCoord.zw -= ((g_BlendOffset / g_TextureReductionScale) - (g_Texture0Resolution.zw - g_Texture1Resolution.zw) * 0.5) / g_Texture0Resolution.zw;";
                                    if vs.contains(pat) {
                                        vs = vs.replace(
                                            pat,
                                            "v_TexCoord.z -= ((g_BlendOffset / g_TextureReductionScale) - (g_Texture0Resolution.zw - g_Texture1Resolution.zw) * 0.5).x / g_Texture0Resolution.x;\n\tv_TexCoord.w -= ((g_BlendOffset / g_TextureReductionScale) + (g_Texture0Resolution.zw - g_Texture1Resolution.zw) * 0.5).y / g_Texture0Resolution.y;",
                                        );
                                    }
                                }
                                // ── `WE_BLEND_CENTER=<f>`（**默认 -1.0，仅 y**）：遮罩纹素口径的中心差平移 ──
                                // 见 [`blend_center_f`] 的推导。追加在 `#if TRANSFORMUV == 1`
                                // 分支的末尾（`v_TexCoord.zw += CAST2(0.5);` 之后）。
                                if let Some(f) = blend_center_f() {
                                    let tail = "v_TexCoord.zw += CAST2(0.5);";
                                    if vs.contains(tail) {
                                        // 只替换**第一处**（TRANSFORMUV 分支里的那一处）
                                        if let Some(i) = vs.find(tail) {
                                            let ins = format!(
                                                "{}\n\tv_TexCoord.w += (g_Texture1Resolution.w - g_Texture0Resolution.w) * 0.5 / g_Texture1Resolution.w * ({f:.6});",
                                                tail
                                            );
                                            vs.replace_range(i..i + tail.len(), &ins);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // ══════════════════════════════════════════════════════════════════
                    // 两处"语义方向"修正（各自带开关，默认开新行为、`=0` 逐字节回旧）
                    // ══════════════════════════════════════════════════════════════════
                    // ① `effects/blend` 的**字色染色强度**（文字层）。
                    // 官方 `blend.frag`（WRITEALPHA=0）是
                    // `albedo.rgb = ApplyBlending(BLENDMODE, albedo.rgb, blendColors.rgb, blendAlpha)`
                    // `BLENDMODE=2` = multiply ⇒ 结果 = 字色 × 人物色 —— **字色必然给人物染色**，
                    // 而且 `multiply` 只是把染色**减淡**（`mix(A, A·B, 0.66)`）。作者给的
                    // `multiply 0.66` / `blendscale` / `blendoffset` 一律不动。
                    // 第二轮（用户三条反驳之一）：
                    // 第一轮把它改成 `mix(albedo, blendColors, 遮罩)` = **完全换成**链接 RT 的原色
                    // ⇒ 字内的人物一个像素都不染，用户判「不染没有透玻璃的感觉了」✗。
                    // 现在改成**可调的部分染色**（`WE_TEXT_TINT`，默认 0.35 = 轻）：
                    // 结果 = mix( 人物原色 , PerformBlend(旧的全染) , TINT )
                    // · `WE_TEXT_TINT=0` ⇒ 逐字节等于第一轮的「完全不染」；
                    // · `WE_TEXT_TINT=1` ⇒ 逐字节等于旧的全染（原来那行 `PerformBlend` 不动）；
                    // · 0<t<1 ⇒ 线性插值，字内人物只是**轻微**偏色。
                    // 数字判据（text5 = 默认蓝 0.349/0.573/0.827，量字内人物中位色）：
                    // 与前景自然肤色的 **B/R 偏差** TINT=0 时 ≈0.04、TINT=1 时 ≈1.35，
                    // 目标区间 0.10~0.25 ⇒ 默认 0.35 落点见 artifacts 里的实测表。
                    // `WE_TEXT_NATURAL=0` ⇒ 整段不 patch，逐字节回 multiply（护栏基线）。
                    let text_natural = env_flag("WE_TEXT_NATURAL", true);
                    let text_tint = env_f32("WE_TEXT_TINT", 0.35).clamp(0.0, 1.0);
                    let blend_pat = "albedo = PerformBlend(albedo, blendColors, blendAlpha);";
                    if text_natural
                        && obj.kind() == we_scene::scene::ObjectKind::Text
                        && fs.contains(blend_pat)
                    {
                        // TINT=1 ⇒ 原样不动（= 旧全染，逐字节）；0 ⇒ 第一轮的完全不染；中间 ⇒ 插值。
                        let patched = if text_tint >= 1.0 {
                            fs.clone()
                        } else if text_tint <= 0.0 {
                            fs.replace(
                                blend_pat,
                                "albedo = mix(albedo, vec4(blendColors.rgb, albedo.a), \
                                 clamp(blend * blendColors.a, 0.0, 1.0));",
                            )
                        } else {
                            fs.replace(
                                blend_pat,
                                &format!(
                                    "albedo = mix(vec4(blendColors.rgb, albedo.a), \
                                     PerformBlend(albedo, blendColors, blendAlpha), {text_tint:.4});"
                                ),
                            )
                        };
                        fs = patched;
                        if dbg {
                            eprintln!(
                                "[图层]     ✓ 字色部分染色（WE_TEXT_TINT={text_tint:.3}；1=旧全染、0=完全不染）\
                                 遮罩/倍率仍用 g_Multiply（作者值不动）"
                            );
                        }
                    }
                    // ② 见下方（`WE_DOT_RAMP`）：那一条要用**本趟已解析的常量**（`u_movement`），
                    // 所以放在 scalars 建好之后再打补丁。

                    // 探针门控必须看**效果自己的源码**，不能看合成后的 `fs`：
                    // `shader.rs` 的 PRELUDE 注释里就写着 `v_TexCoordFx` 这个名字（讲 `mul`
                    // 顺序那段），于是 `fs.contains("v_TexCoordFx")` 对**每一趟**都为真 ⇒
                    // `WE_SHAPE_DEBUG` 的探针会被塞进 `effects/blend` 这类完全无关的趟，
                    // 而它里面没有 `mask`/`fxCoord` ⇒ 编译失败 ⇒ 那一趟被静默跳过。
                    // 实测（3704273480 主题1）：`WE_SHAPE_DEBUG=hit` 把**整屏涂成品红**
                    // （99.63% 像素），`=mask` 则让 `effects/blend`（对象 350）编译失败
                    // （`0:512(27): error: 'mask' undeclared`）⇒ 落盘的 RT 是坏层的产物，
                    // 这就是上次那条"探针自相矛盾"的真因（不是缓冲被复用）。
                    let is_shape_fx =
                        v_raw.contains("v_TexCoordFx") || f_raw.contains("v_TexCoordFx");
                    // ── 遮罩探针 / 调参（B 路线的唯一旋钮）────────────────────────────
                    // WE 的"不透明度遮罩"语义：**白 = 生效（这里是模糊），黑 = 不生效（保持清晰）**
                    // （依据 `${shader}` 注解 `"mode":"opacitymask","paintdefaultcolor":"0 0 0 1"`，
                    // 编辑器里默认画的是"不生效"的黑）。`effects/blur` 的 blur_combine 就是
                    // `float mask = texSample2D(g_Texture1, v_TexCoord.zw).r;` 再 `mix(原图, 模糊, mask)`。
                    // 实测 `2861113973` 的 `masks/blur_combine_mask_bb9fc457` 平均 R≈0.91（几乎全白），
                    // 只有**底部中间**一块是暗的（≈0.55）⇒ 作者的意图是"整块背景都糊，只有底部中间
                    // 那块前景保持清晰"。这跟"参照帧里整幅都不糊"是**两件事**：参照那条路根本没让
                    // 这个整屏后处理层生效。
                    // `WE_MASK_FORCE=0|1|invert` 把遮罩换成常量，用来**一眼判定**这块差异到底是不是
                    // 遮罩造成的（0 = 完全不糊，应当还原出参照那一档；1 = 全幅糊）。
                    if let Ok(mode) = std::env::var("WE_MASK_FORCE") {
                        let repl = match mode.as_str() {
                            "0" => Some("float mask = 0.0;"),
                            "1" => Some("float mask = 1.0;"),
                            "invert" => Some(
                                "float mask = 1.0 - texSample2D(g_Texture1, v_TexCoord.zw).r;",
                            ),
                            _ => None,
                        };
                        if let Some(r) = repl {
                            // 遮罩槽号随 shader 而变（blur_combine 用 g_Texture1，waterripple 用
                            // g_Texture2）⇒ 逐一试，命中哪个就换哪个。
                            let mut hit = None;
                            for pat in [
                                "float mask = texSample2D(g_Texture1, v_TexCoord.zw).r;",
                                "float mask = texSample2D(g_Texture2, v_TexCoord.zw).r;",
                            ] {
                                if fs.contains(pat) {
                                    fs = fs.replace(pat, r);
                                    hit = Some(pat);
                                    break;
                                }
                            }
                            if std::env::var_os("WE_DEBUG_LAYERS").is_some() {
                                match hit {
                                    Some(p) => eprintln!("[遮罩] {} ⇒ 强制 {r}（替换了 {p}）", mp.shader),
                                    None => eprintln!("[遮罩] {} ⇒ 没找到遮罩采样行（没生效 ✗）", mp.shader),
                                }
                            }
                        }
                    }
                    // `WE_FX_PROBE=mask|maskuv|uv` —— 把**特效趟的输出**换成中间量，用来分辨
                    // \"没人画\" / \"画了但输出 0\" / \"遮罩采样错位\"（三者在最终图上一模一样）。
                    // 只对含 `gl_FragColor = texSample2D(...)` 这类单行输出的趟生效；不改变默认行为。
                    if let Ok(mode) = std::env::var("WE_FX_PROBE") {
                        let probe = match mode.as_str() {
                            "mask" => "vec4(vec3(mask), 1.0)",
                            "maskflip" => "vec4(vec3(texSample2D(g_Texture2, vec2(v_TexCoord.z, 1.0 - v_TexCoord.w)).r), 1.0)",
                            "maskuv" => "vec4(v_TexCoord.zw, 0.0, 1.0)",
                            "uv" => "vec4(v_TexCoord.xy, 0.0, 1.0)",
                            // `blend`：把 `effects/blend` 的**混合源**原样铺满整块四边形
                            // （只对声明了 `blendColors` 的 shader 生效）。用途：玻璃字只让
                            // 遮罩在**字形窗口**里露出来，量不到角色整体高度；这个探针把同一套
                            // UV 变换铺满全四边形 ⇒ 能直接量\"透出来的角色在屏上有多高\"（A/B 用）。
                            "blend" => "vec4(blendColors.rgb, 1.0)",
                            _ => "vec4(1.0, 0.0, 1.0, 1.0)",
                        };
                        // `blend` 探针只对定义了 `blendColors` 的片元着色器生效，否则编译不过。
                        let applicable = mode != "blend" || fs.contains("blendColors");
                        let mut hits = 0;
                        for pat in [
                            "gl_FragColor = texSample2D(g_Texture0, texCoord);",
                            "gl_FragColor = texSample2D(g_Texture0, v_TexCoord.xy);",
                            "gl_FragColor = albedo;",
                            "gl_FragColor = color;",
                        ] {
                            if !applicable {
                                break;
                            }
                            let n = fs.matches(pat).count();
                            if n > 0 {
                                hits += n;
                                fs = fs.replace(pat, &format!("gl_FragColor = {probe}; // {pat}"));
                            }
                        }
                        eprintln!(
                            "[探针] {} ⇒ 输出换成 {probe}（命中 {hits} 处{}）",
                            mp.shader,
                            if applicable { "" } else { "，该 shader 无 blendColors ⇒ 跳过" }
                        );
                    }
                    // 诊断探针（只在 `WE_SHAPE_DEBUG` 打开时生效）：把**特效趟**的输出换成中间量。
                    // 用来分辨 shape 全黑是"四边形没画"还是"画了但 shader 输出 0"——两者在图像上
                    // 完全一样，只能这样问。`hit` = 命中测试（四边形内全品红）。
                    let fs = if let Ok(mode) = std::env::var("WE_SHAPE_DEBUG") {
                        if is_shape_fx {
                            // 探针只对含 `v_TexCoordFx` 的 shader（lightshafts 这类）生效，
                            // 否则把 `mask`/`fxCoord` 塞进别的 shader 会直接编译失败。
                            // ⚠ 这里只认**本处支持的四种**；不认识的模式**一律不替换** ——
                            // 以前把它们当 `hit`（整层品红），于是 `WE_SHAPE_DEBUG=rawv`
                            // 在这一处会把该层涂成品红，看起来像"探针生效了"，其实是假象。
                            // （细粒度探针见 `build_shape_layer` 那张表。）
                            let probe = match mode.as_str() {
                                "mask" => "vec4(vec3(mask), 1.0)",
                                "fxc" => "vec4(fxCoord, 0.0, 1.0)",
                                "uv" => "vec4(v_TexCoord.xy, 0.0, 1.0)",
                                "hit" => "vec4(1.0, 0.0, 1.0, 1.0)",
                                _ => "",
                            };
                            if probe.is_empty() {
                                eprintln!("[探针] 特效趟 shader={} 不认模式 {mode}（这一处只支持 mask/fxc/uv/hit）⇒ 未替换", mp.shader);
                            }
                            // 不同 shader 的输出变量名不一样，按常见几种各试一次；命中数打出来，避免"以为改了其实没改"。
                            let mut out = fs.clone();
                            let mut hits = 0;
                            if !probe.is_empty() {
                                for pat in ["gl_FragColor = albedo;", "gl_FragColor = color;", "gl_FragColor = sample;"]
                                {
                                    let n = out.matches(pat).count();
                                    if n > 0 {
                                        hits += n;
                                        out = out.replace(pat, &format!("gl_FragColor = {probe}; // {pat}"));
                                    }
                                }
                                // 命中数**永远打**（以前只在 WE_DEBUG_PASSES 下打）⇒ "以为改了其实没改"
                                // 不会再悄悄发生。
                                eprintln!("[探针] 特效趟 shader={} 输出替换成 {probe}（命中 {hits} 处）", mp.shader);
                            }
                            out
                        } else {
                            fs
                        }
                    } else {
                        fs
                    };
                    // 遮罩 V 补偿（`WE_MASK_VFLIP`，默认开）：链内趟的四边形是 V 反向的
                    // （见 `mask_vflip_rewrite` 的注释与实测数据），普通遮罩贴图会因此被
                    // 读成上下镜像。这里只改写**遮罩槽**的采样坐标，内容采样一律不动。
                    // 诊断（`WE_DBG_UNIFORMS`，默认关，**只打日志**）：把本趟每个
                    // sampler 槽的注解摊开（shader / 趟号 / name / prop / combo / mode /
                    // default）。用途：核对 `"mode":"flowmask"` 这类声明有没有被解析到
                    // （effects/shake 的位移场槽）；**不改变任何渲染数据**。
                    if std::env::var_os("WE_DBG_UNIFORMS").is_some() {
                        let mut line = format!("[uniform] {} 趟{i}", mp.shader);
                        for u in &meta.uniforms {
                            let Some(slot) = shader::sampler_slot(&u.name) else { continue };
                            line += &format!(
                                " | 槽{slot}{} name={} prop={:?} combo={:?} mode={:?} default={:?}",
                                if bound.contains(&slot) { "（已绑）" } else { "" },
                                u.name,
                                u.prop,
                                u.combo,
                                u.mode,
                                u.default
                            );
                        }
                        eprintln!("{line}");
                    }
                    let (mut fs, mask_warp_slots) = {
                        let mut mask_units: Vec<u32> = Vec::new();
                        // 只修**普通贴图**遮罩：RT 输入（`_rt_*` / 链接 RT / framebuffer）与
                        // 本趟 V 反向四边形**同一套约定**（反向就是为了它们），
                        // 翻它们等于把已经对齐的东西翻坏（实测 `effects/blend` 的
                        // `_rt_imageLayerComposite_350_a` 一翻，水珠就被镜像到滑板上）。
                        let tex_name = |slot: u32| -> Option<String> {
                            if let Some(o) = ov {
                                if let Some(Some(t)) = o.textures.get(slot as usize) {
                                    if !t.is_empty() {
                                        return Some(t.clone());
                                    }
                                }
                            }
                            mp.textures.get(slot as usize).cloned()
                        };
                        for u in &meta.uniforms {
                            let Some(slot) = shader::sampler_slot(&u.name) else { continue };
                            let is_mask = u.combo.eq_ignore_ascii_case("MASK")
                                || u.prop.to_ascii_lowercase().contains("opacity_mask")
                                || u.prop.to_ascii_lowercase().contains("opacitymask");
                            if !is_mask || !bound.contains(&slot) {
                                continue;
                            }
                            let is_rt = tex_name(slot).is_some_and(|t| {
                                t.starts_with("_rt_") || t.to_ascii_lowercase().contains("prev")
                            });
                            if !is_rt {
                                mask_units.push(slot);
                            }
                        }
                        mask_units.sort_unstable();
                        mask_units.dedup();
                        // `WE_FLOW_WARP`（默认关）：`"mode"` 含 flowmask / flow_mask 的槽
                        // 是作者画在**画布空间**的位移场 ⇒ 同样要按网格几何烘焙进帧空间
                        // （判据与遮罩槽一致：必须已绑、且不是 RT 输入）。
                        // ⚠ 只进 `warp_slots`，**不扩容 `mask_units`** —— 后者还喂遮罩
                        // V/U 补偿与图集仿射三条路径，扩了会连带改坏那三条。
                        let flow_units: Vec<u32> = if flow_warp_on() {
                            let mut v: Vec<u32> = Vec::new();
                            for u in &meta.uniforms {
                                let Some(slot) = shader::sampler_slot(&u.name) else { continue };
                                // `replace("_", "")` ⇒ `flow_mask` 与 `flowmask` 同一判据。
                                let m = u.mode.to_ascii_lowercase().replace("_", "");
                                if !m.contains("flowmask") {
                                    continue;
                                }
                                if !bound.contains(&slot) {
                                    continue;
                                }
                                let is_rt = match tex_name(slot) {
                                    Some(t) if t.starts_with("_rt_") => true,
                                    Some(t) if t.to_ascii_lowercase().contains("prev") => true,
                                    _ => false,
                                };
                                if !is_rt {
                                    v.push(slot);
                                }
                            }
                            v.sort_unstable();
                            v.dedup();
                            v
                        } else {
                            Vec::new()
                        };
                        // `WE_MASK_VFLIP` 除 `1/on/true` 外还接受**逗号分隔的遮罩名片段**
                        // （如 `480a6962,03fcd88f`）⇒ 只翻这些遮罩槽（其它遮罩保持原样）。
                        // 用途：分清"哪张遮罩被镜像"与"哪张本来就该镜像"，而不是笼统翻 v。
                        let sel_units: Vec<u32> = match mask_vflip_filter() {
                            None => mask_units.clone(),
                            Some(list) => mask_units
                                .iter()
                                .copied()
                                .filter(|s| {
                                    tex_name(*s)
                                        .map(|t| list.iter().any(|k| t.contains(k.as_str())))
                                        .unwrap_or(false)
                                })
                                .collect(),
                        };
                        if dbg && !sel_units.is_empty() {
                            let names: Vec<String> = sel_units
                                .iter()
                                .map(|s| tex_name(*s).unwrap_or_default())
                                .collect();
                            eprintln!("[图层]     遮罩V补偿选中：{:?}", names);
                        }
                        let mask_units = sel_units;
                        // `WE_MASK_UFLIP`（同样接受逗号分隔的遮罩名片段）⇒ 只把 **u** 取反。
                        // 存在的理由：mask `480a6962` 在**两种 v 取向**下都落在错的地方
                        // （不翻 ⇒ 滑板尖 10.94；翻 ⇒ 板面 5.18）——纯 V 翻转解释不了，
                        // 说明它差的是 u（或 uv）而不是 v。这个开关用来把三种组合一次定死。
                        let u_all: Vec<u32> = match mask_uflip_filter() {
                            None => Vec::new(),
                            Some(list) => mask_units
                                .iter()
                                .copied()
                                .filter(|s| {
                                    tex_name(*s)
                                        .map(|t| list.iter().any(|k| t.contains(k.as_str())))
                                        .unwrap_or(false)
                                })
                                .collect(),
                        };
                        let v_only: Vec<u32> = mask_units
                            .iter()
                            .copied()
                            .filter(|s| !u_all.contains(s))
                            .collect();
                        if dbg && !u_all.is_empty() {
                            eprintln!("[图层]     遮罩 U 翻转命中槽 {:?}", u_all);
                        }
                        let run_flips = mask_vflip_on() || mask_uflip_filter().is_some();
                        let out = if run_flips {
                            let (nf, h1) = mask_axis_rewrite(&fs, &v_only, false, true);
                            let (nf, h2) = mask_axis_rewrite(&nf, &u_all, true, false);
                            let (nf, hits) = (nf, h1 + h2);
                            if dbg && hits > 0 {
                                eprintln!(
                                    "[图层]     遮罩V补偿 ⇒ {} 槽 {:?}（改写 {hits} 处采样坐标）",
                                    mp.shader, mask_units
                                );
                            }
                            nf
                        } else {
                            fs
                        };
                        // 把遮罩采样从**取景框空间**仿射到**图集空间**（默认关）。
                        // 只改遮罩槽的 `v_TexCoord.zw`；内容采样（`.xy`）与网格顶点一个字节不动
                        // ⇒ 底图趟照旧画网格，动画不受影响。见 `mask_atlas_space_on`。
                        let (fs_out, _) = match mask_atlas_affine {
                            Some(aff) if !mask_units.is_empty() => {
                                let (nf, hits) = mask_uv_affine_rewrite(&out, &mask_units, aff);
                                if dbg {
                                    eprintln!(
                                        "[图层]     遮罩→图集仿射 ⇒ {} 槽 {:?}（改写 {hits} 处采样坐标）",
                                        mp.shader, mask_units
                                    );
                                }
                                (nf, ())
                            }
                            _ => (out, ()),
                        };
                        // `MODE=mesh`：不改 shader，只记下**本趟的遮罩贴图**
                        // 稍后在它前面插一趟「用同一份网格几何把这张遮罩画进帧空间」，
                        // 并把这一槽改成绑那张 RT ⇒ `v_TexCoord.zw` 原样采样即对齐。
                        // `warp_slots` = 本趟要按网格几何烘焙进帧空间的槽。默认就是
                        // 上面那批遮罩槽（**逐字节等同**于原来的 `mask_units.clone`）；
                        // `WE_FLOW_WARP` 打开时才追加 flowmask 槽。
                        let mut warp_slots: Vec<u32> = mask_units.clone();
                        for s in &flow_units {
                            if !warp_slots.contains(s) {
                                warp_slots.push(*s);
                            }
                        }
                        let slots = if mask_atlas_mode() == 1 {
                            warp_slots
                        } else {
                            Vec::new()
                        };
                        (fs_out, slots)
                    };
                    // `WE_WAVE_NPHASE`：水波族（`float distance = g_Time * g_Speed + …`）
                    // 的**时间项**口径。默认只在**骨骼网格层**冻住（`hold`）——
                    // `wave_nphase_mode` 的完整证据；平铺图像层保持官方 `live`。
                    // 只改那两行 `distance`，遮罩采样/空间项/内容采样一个字节不动。
                    {
                        let wmode = wave_nphase_mode(puppet_on_h);
                        if wmode != 0 {
                            let (nf, hits) = wave_nphase_rewrite(&fs, wmode);
                            if hits > 0 {
                                fs = nf;
                                if dbg {
                                    eprintln!(
                                        "[图层]     水波时间相位 ⇒ {} mode={wmode}（改写 {hits} 处）",
                                        mp.shader
                                    );
                                }
                            }
                        }
                    }
                    // ② 点阵「新生大 → 离场小」：把**同一条遮罩渐变**也接到点的半径上。
                    // 第二轮（用户反驳 2）：「点也没做到大变小」。
                    // 第一轮（`WE_DOT_RAMP=1`）只把遮罩 U **镜像**，数字上方向确实翻了
                    // （遮罩亮度剖面 134→18 变 27→117），但**屏上看不出**。本次逐层量出两个原因：
                    // ① 这个 shader 里 `opacity` 只走最后一行
                    // `col = ApplyBlending(BLENDMODE, background, col, opacity)`
                    // ⇒ 只让点**变淡**，**完全不碰半径**；
                    // ② 点的半径是 `thresh = u_baseSize + noise*u_mapInfluence + …`，
                    // 与遮罩无关 ⇒ 翻遮罩永远变不出「大→小」。
                    // 实测：整帧只有 2773 px 变化（0.13%），点本身 1~3px 只改了亮度
                    // ⇒ 这就是「数字说翻了、屏上没变化」的真因。
                    // 流向实测（这一步第一轮做错了，本次纠正）：
                    // 网格周期 ~28px，纯像素互相关在「一帧」尺度上会落到 **-28 = 一个整周期**
                    // 的**假匹配**（第一轮就是被它骗的）。用更小的时间步做互相关：
                    // t=4.0 → 4.2（0.2s）：最佳位移 **dx=+6, dy=0**（mad 0.27，次优 2.16）
                    // t=4.0 → 4.5（0.5s）：**±14 两侧等深**（15px ≈ 半周期，恰好折回）
                    // ⇒ 点实际**向右**流（≈30 px/s），**新生在左、离场在右**。
                    // 而 Windows 参照图（`~/Downloads/3-1.jpg` 左下角）正是
                    // **左大右小** ⇒ **作者原本的（未镜像）遮罩方向就是对的**；
                    // 第一轮的镜像把它翻**反**了（新生小、离场大）。
                    // 本次做法：
                    // · 默认**不再镜像**（`WE_DOT_MIRROR=0`），沿用作者的遮罩方向；
                    // · 把遮罩值接进半径：`thresh *= mix(1.0, dotRamp, WE_DOT_SHRINK)`
                    // ⇒ 左（新生、遮罩亮）= 大，右（离场、遮罩暗）= 小，与 win 一致；
                    // · 同一趟只采一次遮罩（`dotRamp`），`opacity` 复用同一采样；
                    // · 必须乘**整条 thresh**：只乘 `u_baseSize` 会被后面的
                    // `noise*u_mapInfluence`（负值）吃掉 ⇒ thresh<0 ⇒ 点整层消失
                    // （实测：最大亮度 255→30、连通块 294→0，这条是踩过的坑）。
                    // 开关：`WE_DOT_RAMP=0` ⇒ 整段不 patch（逐字节回旧，护栏用）；
                    // `WE_DOT_SHRINK=0` ⇒ 半径不接渐变（只留遮罩，= 第一轮那样）；
                    // `WE_DOT_MIRROR=1` ⇒ 遮罩 U 镜像（第一轮那条，留作 A/B；方向是反的）。
                    let dot_ramp = env_flag("WE_DOT_RAMP", true);
                    // 半径渐变挂在总闸下：`WE_DOT_RAMP=0` ⇒ 半径也回旧（否则「四开关全 0」的
                    // 护栏基线会被半径渐变改掉，护栏就不成其为护栏）。
                    // 默认值从 0.6 改为 0.0 —— **作者原 shader 里
                    // `thresh` 只有 baseSize + noise*mapInfluence + 鼠标项，没有任何
                    // "遮罩 → 半径" 的耦合**（`shaders/.../dot_matrix_mobile_fix.frag:116-118`）。
                    // 0.6 是上次为了"左大右小"自己加的项，实测把点整体缩小 6.8%：
                    // 2560x1440 同一区域的径向 FWHM 中位数 = 5.88 px（0.6）对 **6.31 px**
                    // （`WE_DOT_SHRINK=0`，= 作者口径）对 **6.31 px**（win 3-1 截图）。
                    // 遮罩本来就已经通过 `opacity` 让点变淡（`col = ApplyBlending(..., opacity)`），
                    // 再乘一次半径是双重计数。旧行为：`WE_DOT_SHRINK=0.6`。
                    let dot_shrink = if dot_ramp {
                        env_f32("WE_DOT_SHRINK", 0.0).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    let dot_mirror = env_flag("WE_DOT_MIRROR", false);
                    let mask_pat = "texSample2D(g_Texture2, v_TexCoord.xy / t2)";
                    if dot_ramp && fs.contains(mask_pat) {
                        // 作者给的 `Movement` 在**场景那一趟**（`ov`）里，特效文件那两层
                        // （`ep`/`ef`）通常没有 ⇒ 三处都查，`ov` 优先（与常量播种的优先级一致）。
                        let mut mov = ov.and_then(|o| o.constant_shader_values.get("Movement"));
                        if mov.is_none() {
                            mov = ep
                                .constantshadervalues
                                .get("Movement")
                                .or_else(|| ef.constantshadervalues.get("Movement"));
                        }
                        let flows_left = mov
                            .and_then(|v| v.as_str())
                            .and_then(we_scene::scene::parse_vec2)
                            .map(|p| p[0] < 0.0)
                            .unwrap_or(false);
                        if std::env::var_os("WE_DBG_DOT_RAMP").is_some() {
                            eprintln!(
                                "[点阵] shader={} Movement={:?} ⇒ flows_left={flows_left} \
                                 ramp={dot_ramp} mirror={dot_mirror} shrink={dot_shrink:.3}",
                                mp.shader,
                                mov.map(|v| v.to_string())
                            );
                        }
                        // 遮罩采样表达式：镜像只改 U（`1.0 - x/t2.x`），V 不动。
                        let mask_expr: &str = if dot_mirror && flows_left {
                            "texSample2D(g_Texture2, vec2(1.0 - v_TexCoord.x / t2.x, \
                             v_TexCoord.y / t2.y))"
                        } else {
                            mask_pat
                        };
                        // 三个锚点必须**同时**在，否则宁可不改：少一个就会把
                        // `dotRamp` 插到 `dotRamp` 自己也够不着的地方（= 编译失败整趟丢），
                        // 或者把 `t2` 声明删掉留下悬空引用。这一条是本次给自己加的保险。
                        let anchors_ok = fs.contains("float influence = ceil(")
                            && stmt_span(&fs, "float opacity = texSample2D(g_Texture2,").is_some()
                            && stmt_span(&fs, "vec2 t2 = g_Texture2Resolution").is_some()
                            && stmt_span(&fs, "fragLV += smoothstep(dist, dist + u_smoothing,")
                                .is_some();
                        if !anchors_ok {
                            if std::env::var_os("WE_DBG_DOT_RAMP").is_some() {
                                eprintln!(
                                    "[点阵] ⚠ shader={} 缺锚点，跳过（influence/opacity/t2/smoothstep）",
                                    mp.shader
                                );
                            }
                        } else {
                            // 顺序要紧：先落 `t2`/`dotRamp` 声明，**再**插 `sizeFactor`
                            // （两者都插在同一个锚点前；反序会变成"先用后声明"⇒ 编译失败）。
                            // ② opacity 改成复用 dotRamp（同一趟只采一次遮罩）
                            // ③ `t2` 声明搬到 `thresh` 之前（原来在文件末尾，作用域不够）
                            let (a, b) = stmt_span(&fs, "float opacity = texSample2D(g_Texture2,")
                                .unwrap();
                            fs.replace_range(a..b, "float opacity = dotRamp;");
                            let (a2, b2) = stmt_span(&fs, "vec2 t2 = g_Texture2Resolution").unwrap();
                            fs.replace_range(a2..b2, "");
                            let decl = format!(
                                "vec2 t2 = g_Texture2Resolution.xy / g_Texture2Resolution.zw;\n\
                                 \tfloat dotRamp = {mask_expr}.r;\n\t"
                            );
                            let i = fs.find("float influence = ceil(").unwrap();
                            fs.insert_str(i, &decl);
                            // ① 半径接渐变：**整条 thresh 一起乘** `sizeFactor`
                            // （只乘 `u_baseSize` 是错的：后面还有 `noise*u_mapInfluence`（负值）
                            // 会加回来 ⇒ 小 base + 负噪声 ⇒ thresh<0 ⇒ 点直接没有；
                            // 实测那样整层几乎全黑（最大亮度 255→30））。
                            // 方向（本次实测纠正后的结论）：流向**向右**（≈30px/s，见上面 2.3），
                            // 遮罩亮的一侧在左 = **新生端**，而这个 shader 里遮罩大 = 点更明显
                            // （`col = mix(background, col, opacity)`）⇒ 半径乘 `dotRamp` 本身
                            // 就是「左大右小」= win 的样子。乘 `1-dotRamp` / 只乘总闸下的镜像
                            // 都会把新生端或离场端整片缩没（两种都实测过，整层消失/仅变淡）。
                            if dot_shrink > 0.0 {
                                let radius = format!(
                                    "float sizeFactor = mix(1.0, dotRamp, {dot_shrink:.4});"
                                );
                                let i = fs.find("float influence = ceil(").unwrap();
                                fs.insert_str(i, &format!("{radius}\n\t"));
                                let (a3, b3) = stmt_span(
                                    &fs,
                                    "fragLV += smoothstep(dist, dist + u_smoothing,",
                                )
                                .unwrap();
                                let stmt = fs[a3..b3].replace(
                                    "u_smoothing, thresh)",
                                    "u_smoothing, thresh * sizeFactor)",
                                );
                                fs.replace_range(a3..b3, &stmt);
                            }
                            if dbg {
                                eprintln!(
                                    "[图层]     ✓ 点阵渐变：镜像={dot_mirror}（flows_left={flows_left}）\
                                     半径渐变={dot_shrink:.3}（WE_DOT_SHRINK）"
                                );
                            }
                        }
                    }
                    // ══════════════════════════════════════════════════════════════════
                    // ③ 点阵 **时间驱动项** 的 A/B 旋钮（默认全关 ⇒ 逐字节保持现状）
                    // 用户新线索：切壁纸的**那一瞬间**点是对的，只维持几帧，之后就不对。
                    // ⇒ 错的必须是 `g_Time` 项（t≈0 时它们全为 0 ⇒ 头几帧必然"对"），
                    // 不是静态渐变（静态项错了 t=0 也会错）。
                    // 本 shader 里**随时间变化**的项只有三处（逐条对应开关）：
                    // (a) `vec2 offset = mul(u_movement, rot) * g_Time;`
                    // ⇒ 点阵平移：方向（符号）/ 速率。它同时喂 `squareGrid(...)` 与
                    // `tileScreenPos`（后者再喂噪声采样位置与鼠标距离）。
                    // (b) `getNoise` 的 `u_noiseMovement * g_Time`（噪声场相位）
                    // ⇒ `thresh += noise * u_mapInfluence`（本场景 **−0.24**，
                    // 与 u_baseSize 0.29 同量级）⇒ 直接改**每个点的半径**，
                    // 且 0.5 uv/s ≈ 960 px/s（远快于点自身 28 px/s）。
                    // (c) 遮罩采样位置：作者写 `v_TexCoord`（= 层/屏固定）
                    // ⇒ 点流过渐变时**每个点自己变尺寸**（大→小）。
                    // 另一条语义是把它挂在**点阵格子坐标**上（跟着点流，
                    // 每个点一辈子一个尺寸，渐变带随点走）。两者在 t=0
                    // **完全重合**，t>0 才分道扬镳 ⇒ 也是"头几帧对"的候选。
                    // 开关（一个都不给 = 现状，逐字节不变）：
                    // WE_DOT_MOVSIGN=-1 (a) 方向反号（A/B）
                    // WE_DOT_MOVRATE=0.5|2 (a) 速率缩放（A/B；×2 里含 fps 口径的 2 倍）
                    // WE_DOT_NRATE=0|0.5|2 (b) 噪声漂移速率缩放（0 = 冻住相位）
                    // WE_DOT_NPHASE=zero|norm (b) 相位冻结在 0 / 按噪声自身周期 2s 归一化
                    // WE_DOT_RAMPSPACE=lattice (c) 遮罩挂在点阵格子上（不随屏固定）
                    if fs.contains(mask_pat) {
                        let mov_sign = env_f32("WE_DOT_MOVSIGN", 1.0);
                        let mov_rate = env_f32("WE_DOT_MOVRATE", 1.0);
                        let noise_rate = env_f32("WE_DOT_NRATE", 1.0);
                        // 默认**冻结噪声相位**（win 静帧里大点成片存在；实测相位漂移会在 2 s 内把尺寸场永久塌平）。
                        // `WE_DOT_NPHASE=live` 回旧（漂移），`=norm` 按 2s 周期归一化。
                        let nphase = std::env::var("WE_DOT_NPHASE").unwrap_or_else(|_| "zero".to_string());
                        let ramp_space = std::env::var("WE_DOT_RAMPSPACE").unwrap_or_default();
                        let mut applied: Vec<String> = Vec::new();
                        // (a) 平移：方向 × 速率，合成一个常数 k（=1 时**不动源码**）
                        let k = mov_sign * mov_rate;
                        if (k - 1.0).abs() > 1e-9 {
                            let pat = "vec2 offset = mul(u_movement, rot) * g_Time;";
                            if fs.contains(pat) {
                                fs = fs.replace(
                                    pat,
                                    &format!(
                                        "vec2 offset = mul(u_movement, rot) * (g_Time * {k:.6});"
                                    ),
                                );
                                applied.push(format!("mov×{k:.3}"));
                            }
                        }
                        // (b) 噪声相位
                        let npat = "u_noiseMovement * g_Time";
                        if fs.contains(npat) {
                            let rep = match nphase.as_str() {
                                "zero" => Some("vec2(0.0, 0.0)".to_string()),
                                // 噪声贴图 uv 自身以 1.0 为周期 ⇒ 0.5 uv/s 时 2s 一整圈；
                                // 归一化 = 相位只在一个周期内走（不累积、不漂出贴图）。
                                "norm" => Some("u_noiseMovement * mod(g_Time, 2.0)".to_string()),
                                _ if (noise_rate - 1.0).abs() > 1e-9 => Some(format!(
                                    "u_noiseMovement * (g_Time * {noise_rate:.6})"
                                )),
                                _ => None,
                            };
                            if let Some(r) = rep {
                                fs = fs.replace(npat, &r);
                                applied.push(format!(
                                    "noise:{}{}",
                                    if nphase.is_empty() { "" } else { &nphase },
                                    if (noise_rate - 1.0).abs() > 1e-9 {
                                        format!("×{noise_rate:.2}")
                                    } else {
                                        String::new()
                                    }
                                ));
                            }
                        }
                        // (c) 遮罩空间：lattice = 采样坐标跟着点阵平移（每点尺寸恒定），
                        // 与屏固定的 `v_TexCoord` 在 t=0 完全等价。
                        if ramp_space == "lattice" {
                            if let Some((a3, b3)) = stmt_span(&fs, "float dotRamp = ") {
                                // luv = 层 uv + _rot·offset/u_scale（= 点阵自己的坐标，
                                // 每个点不随时间变）；换回 v_TexCoord 口径再除 t2。
                                let luv = "mul(mul(uv * u_scale, rot) + offset, _rot) / u_scale";
                                let vtc = format!("(({luv}) / ratCorr + 0.5)");
                                let expr = if dot_mirror {
                                    format!(
                                        "vec2(1.0 - ({vtc}).x / t2.x, ({vtc}).y / t2.y)"
                                    )
                                } else {
                                    format!("({vtc}) / t2")
                                };
                                let stmt =
                                    format!("float dotRamp = texSample2D(g_Texture2, {expr}).r;");
                                fs.replace_range(a3..b3, &stmt);
                                applied.push("ramp:lattice".into());
                            }
                        }
                        if std::env::var_os("WE_DBG_DOT_TIME").is_some() {
                            eprintln!(
                                "[点阵时间] shader={} 生效={:?}（空 = 现状）",
                                mp.shader, applied
                            );
                        }
                    }

                    if let Ok(p) = std::env::var("WE_DUMP_SHADER") {
                        // 文件名**自报身份**（对象 id / 特效号 / 趟号 / shader 名）。
                        // 以前只写 `{p}.pass{i}` ⇒ 同场景别的链的同号趟互相覆盖 ⇒ **看错趟是常态**
                        // （实测踩坑：把音频条特效的 vert 当成 blend 的读，白跑一轮）。
                        // 现在带 id ⇒ `WE_ONLY_ID=691` 时能直接读到那一趟的两个阶段。
                        let safe: String = mp
                            .shader
                            .chars()
                            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                            .collect();
                        let idt = format!("{p}.{}.e{}p{}", obj.id, ei, i);
                        let _ = std::fs::write(format!("{idt}.{safe}.frag"), &fs);
                        // 顶点阶段也落盘：varying 类型不匹配 / 顶点编译失败时，只 dump 片元
                        // 根本没法定睛（顶点 declare vec4、片元 vec2 这种只能两阶段对照看）。
                        let _ = std::fs::write(format!("{idt}.{safe}.vert"), &vs);
                        // 顶点**来源**出声：`官方原样` = 直接跑 shader 自带的 `.vert`；
                        // `改写` = 过了 `unify_varyings`（收窄/补宽）等替换。
                        // "这趟到底跑没跑官方顶点数学"反复判断错，就是因为这条信息一直缺。
                        let prov = if vs == v_raw { "官方原样" } else { "改写(unify/补宽)" };
                        eprintln!(
                            "[图层]     趟 dump id={} 特效{}趟{} shader={} 顶点={}",
                            obj.id, ei, i, mp.shader, prov
                        );
                    }
                    // 链内趟的几何 = **全屏 [-1,1] 四边形**（与平面图层完全同一条路）。
                    // 参照实现 `linux-wallpaperengine CImage.cpp:804-816`：
                    // spacePosition = isFirstPass ? (hasPuppetMesh ? puppetSpacePosition : copySpacePosition)
                    // getPassSpacePosition; // 全屏 [-1,1]
                    // 即**网格几何只出现在首趟**；链内每一趟都是全屏四边形 + 单位阵 MVP。
                    // 以前我们把网格挂到每一趟（并把顶点当裁剪空间用）⇒ 溢出 ±1 的顶点被硬裁，
                    // 于是\"有网格的对象必须跳过链\"（`WE_MESH_SKIP_CHAIN`）。照参照修正后，
                    // 网格对象可以带链正常渲染。
                    // **例外：shape 的收尾趟**。OWE 的 `ParseShapeObj` 给
                    // ImageObject 传了 `requires_source_draw = false` + `final_mesh = 直接绘制
                    // 四边形` ⇒ `SceneNodeLayer::Resolve` 里 `isPassthrough || !requires_source_draw`
                    // 那条分支把**最后一个输出趟的几何换成 `m_final_mesh`**（并锚在图层节点上、
                    // 用主相机）⇒ WE 的扇区参数化 = **四边形自己的 uv**（作者那四个控制点）。
                    // 我们照此：object 的**最后一趟**用 shape 的四边形网格 + 对象 MVP。
                    let quad_last = shape_quads
                        .as_ref()
                        .filter(|_| ei + 1 == obj.effects.len() && i + 1 == ef.passes.len());
                    // WE_FX_CARDSPACE：链内趟的几何换成官方卡片网格 `±size/2`。
                    // `card_applied=false`（= shader 不用 MVP / 走 view-projection）时几何**没换**，
                    // `mvp` 必须照旧用 `full_q`（见 `make_layer_card` 的文档注释）。
                    let mut card_applied = false;
                    let layer = match quad_last {
                        Some(q) => self.gl.make_mesh_layer(&vs, &fs, q),
                        None if fx_cards_here => {
                            match self.gl.make_layer_card(
                                &vs,
                                &fs,
                                [fx_card_px[0] / 2.0, fx_card_px[1] / 2.0],
                            ) {
                                Ok((l, ok)) => {
                                    card_applied = ok;
                                    Ok(l)
                                }
                                Err(e) => Err(e),
                            }
                        }
                        // 官方铁律③：特效/后处理趟 = **2×2 常量卡片**
                        // （`wallpaper64.exe` `FUN_1401ea310` 传 `0x4000000040000000` =
                        // `(2.0, 2.0)`，配 `ortho(2,2)` 相机 ⇒ 顶点 = ±1、uv = 0..1）。
                        // 顶点与我们的 `[-1,1]` 单位四边形**逐位相同**，差别只在官方
                        // `FUN_1401ede30` 的 **0.15 texel uv 内缩**（`uvEnd - 0.15/texSize`；
                        // `flag & 4` 翻 v 由 `raw_position` 那条路给）。
                        // `WE_FX_CARD=0` ⇒ 回旧构造函数（与 `=1` + 内缩 0 逐位相同）。
                        // ⚠ **内缩量默认 0**：官方 `flag & 2` 才内缩，而我们这条路上每一趟
                        // 都是"RT → RT"（来源边缘已被 CLAMP_TO_EDGE 夹住），按 0.15 texel
                        // 逐趟内缩实测改动 70 万像素（mean 1.9，纯亚像素重采样）⇒ 会破坏
                        // "15 条稳定场景逐字节"的护栏。要复现官方内缩：`WE_FX_CARD_INSET=0.15`。
                        None if env_flag("WE_FX_CARD", true) => self
                            .gl
                            .make_fx_card(&vs, &fs, (lw, lh), env_f32("WE_FX_CARD_INSET", 0.0)),
                        None => self.gl.make_layer(&vs, &fs, None),
                    };
                    let layer = match layer {
                        Ok(l) => l,
                        Err(e) => {
                            // 编译/链接失败必须出声（含 shader 名与 GL 日志原文）——以前这里是
                            // `eprintln!` + continue，但只有 debug 才会看到，且没有汇总。
                            eprintln!("[场景] 特效趟 {i} 编译失败（{}）：{e}", mp.shader);
                            // 失败时按 **shader 名**落盘两个阶段：`{p}.pass{i}` 会被同场景里
                            // 别的链的同号趟覆盖，只看它经常看不到真正失败的那份源码。
                            if let Ok(p) = std::env::var("WE_DUMP_SHADER") {
                                let safe: String = mp
                                    .shader
                                    .chars()
                                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                                    .collect();
                                let _ = std::fs::write(
                                    format!("{p}.{}.e{}p{}.FAIL.{safe}.vert", obj.id, ei, i),
                                    &vs,
                                );
                                let _ = std::fs::write(
                                    format!("{p}.{}.e{}p{}.FAIL.{safe}.frag", obj.id, ei, i),
                                    &fs,
                                );
                            }
                            self.note_skipped(
                                &eff.file,
                                &format!("shader {} 编译/链接失败：{e}", mp.shader),
                                Some(i),
                            );
                            continue;
                        }
                    };

                    // ── 遍历式槽位加载（foreach item）────────────────────────────────
                    // 以 **shader 自己声明的 sampler 槽**为准，逐槽按“声明”解析数据源；
                    // **不做任何 item 名字 / 注解 mode / blending 名特判**（对齐用户准则：
                    // “foreach item 那样，不针对 itemname 进行读取并加载”）。
                    // 优先级（编辑器写的值最高；特效绑定次之；材质声明再次；引擎/注解兜底）：
                    // ① 场景/对象本趟 `passes[i].textures[k]`（作者在编辑器里改的值）
                    // ② 特效 `effect.json` 的 `bind`（slot → 名字，如 "previous"）
                    // ③ 材质本趟 `textures[k]`（材质**自己声明**的贴图 —— 以前完全没读）
                    // ④ 注解：framebuffer / Prev / hidden 且无默认资源 ⇒ 本趟输入
                    // ⑤ 注解 `default` 声明的资源（如 util/white）
                    // 打包贴图的 V 方向由 `FxTexVFlip` 从对象数据推导（不看槽的 mode），
                    // 整条链统一作用于**所有**打包槽（RT 槽不受影响，= 不入 `upload_*`）。
                    self.fx_slot_flip = self.fx_vflip.want_flip(obj.copy_background, layer_space);
                    let mut slots: Vec<u32> = shader::declared_sampler_slots(&meta);
                    if let Some(o) = ov {
                        for (k, t) in o.textures.iter().enumerate() {
                            if t.as_deref().map(|s| !s.is_empty()).unwrap_or(false) {
                                slots.push(k as u32);
                            }
                        }
                    }
                    for b in &ep.bind {
                        if b.index >= 0 {
                            slots.push(b.index as u32);
                        }
                    }
                    for (k, t) in mp.textures.iter().enumerate() {
                        if !t.is_empty() {
                            slots.push(k as u32);
                        }
                    }
                    slots.sort_unstable();
                    slots.dedup();
                    let mut units: Vec<(u32, Src)> = Vec::new();
                    for k in slots {
                        let decl = meta.uniforms.iter().find(|u| shader::sampler_slot(&u.name) == Some(k));
                        // ① 场景/对象本趟写了值
                        let ov_name = ov
                            .and_then(|o| o.textures.get(k as usize))
                            .and_then(|t| t.as_deref())
                            .filter(|s| !s.is_empty());
                        // ② 特效 bind
                        let bind_name = ep
                            .bind
                            .iter()
                            .find(|b| b.index == k as i32)
                            .map(|b| b.name.as_str())
                            .filter(|s| !s.is_empty());
                        // ③ 材质声明的贴图
                        let mat_name = mp
                            .textures
                            .get(k as usize)
                            .map(|s| s.as_str())
                            .filter(|s| !s.is_empty());
                        // ④ 注解说这是“引擎喂的输入槽”（framebuffer/Prev，或 hidden 且无默认资源）
                        let is_input = decl
                            .map(|u| {
                                matches!(
                                    u.prop.as_str(),
                                    "ui_editor_properties_framebuffer"
                                        | "Prev"
                                        | "ui_editor_properties_prev"
                                ) || (u.hidden && u.default.is_empty())
                            })
                            .unwrap_or(false);
                        let src = if let Some(n) = ov_name.or(bind_name).or(mat_name) {
                            self.resolve_src(n, prev, assets, pkg, pkg_bytes)
                        } else if is_input {
                            // slot 0 = 上一趟的输出（高斯/降采样这类“处理趟”的正解）；
                            // slot ≥1 = 本特效开始时的画面（合成趟的 `albedo`/背景）。
                            Some(if k == 0 { prev } else { self.cur_effect_input })
                        } else if let Some(d) = decl.map(|u| u.default.as_str()).filter(|s| !s.is_empty()) {
                            // ⑤ 注解默认资源（如 `util/white`）
                            self.resolve_src(d, prev, assets, pkg, pkg_bytes)
                        } else {
                            None
                        };
                        match src {
                            Some(s) => units.push((k, s)),
                            // 声明了槽却没有任何来源 ⇒ 出声（否则会读到 GL 里残留的绑定）。
                            None if dbg => eprintln!(
                                "[图层]     槽 {k}（{}）没有任何来源（声明了 sampler 但数据里没有）",
                                decl.map(|u| u.name.as_str()).unwrap_or("?")
                            ),
                            None => {}
                        }
                    }
                    self.fx_slot_flip = false;

                    // 逐趟常量：场景给的是"编辑器参数名"，用 shader 注释里的映射翻成 uniform 名
                    let mut scalars: Vec<(String, Value)> = Vec::new();
                    // 逐帧脚本常量：`constantshadervalues.<key>` 可以是 `{script, value}`
                    // （官方每帧跑 `update`，`value` 只是初值）。收集成本趟的 (uniform, 脚本)，
                    // 静态 `value` 照旧进 scalars ⇒ 脚本关掉时逐字节不变。
                    let mut pass_scripts: Vec<(String, propscript::NumScript)> = Vec::new();
                    // 先播种【特效自己声明的默认值】（effect.json / pass 的 `constantshadervalues`）。
                    // 以前完全没读 ⇒ 模糊半径、采样数这些 uniform 全空 ⇒ "特效在跑但量不对"。
                    // 对象级覆盖在下面，后写 ⇒ 天然优先（与 WE 的优先级一致）。
                    for (prop, v) in ef
                        .constantshadervalues
                        .iter()
                        .chain(ep.constantshadervalues.iter())
                    {
                        // 常量键 → uniform 名：**两种写法都认**（注解 material 标签 / uniform 名去 u_），
                        // 对齐 OWE `ResolveShaderMaterialKey`；见 `shader::find_uniform_for_key`。
                        // 以前只认标签 ⇒ 另一种写法的键被静默丢掉 ⇒ 那一趟"在跑但量不对"。
                        let Some(u) = shader::find_uniform_for_key(&meta, prop)
                              .or_else(|| shader::find_uniform_for_key(&vmeta_keys, prop)) else {
                            // 键在本趟 shader 里找不到对应 uniform ⇒ 这条值被丢弃。**出声**
                            // （对齐 OWE `SceneMaterialBuilder.cpp:922` 的
                            // `ignoring shader value '{}' without a uniform in '{}'`）：
                            // 静默丢弃正是"特效在跑但量不对"最难查的那一类。
                            if dbg {
                                eprintln!(
                                    "[图层]     常量 {prop:?} 在本趟 shader（{}）里没有对应 uniform ⇒ 丢弃",
                                    mp.shader
                                );
                            }
                            continue;
                        };
                        if let Some(val) = json_to_value(v) {
                            scalars.push((u.name.clone(), coerce_to_decl(val, &u.ty)));
                        }
                        if fx_const_script_on() {
                            if let Some(b) = fx_const_script(v) {
                                pass_scripts.retain(|(n, _)| n != &u.name);
                                pass_scripts.push((u.name.clone(), b));
                            }
                        }
                    }
                    if let Some(o) = ov {
                        for (prop, v) in &o.constant_shader_values {
                            let Some(u) = shader::find_uniform_for_key(&meta, prop)
                              .or_else(|| shader::find_uniform_for_key(&vmeta_keys, prop)) else {
                                continue;
                            };
                            if let Some(val) = json_to_value(v) {
                                scalars.push((u.name.clone(), coerce_to_decl(val, &u.ty)));
                            }
                            if fx_const_script_on() {
                                if let Some(b) = fx_const_script(v) {
                                    pass_scripts.retain(|(n, _)| n != &u.name);
                                    pass_scripts.push((u.name.clone(), b));
                                }
                            }
                        }
                    }

                    // `WE_FX_ALPHA` 诊断用的「显式给了」快照：此刻**只有** `ef`/`ep`/`ov` 三处
                    // 作者数据写过 scalars（我们的中性默认与 range 兜底都还没跑）⇒ 这里出现的
                    // alpha 族名字 = 作者显式给的。开关打开时正是**只有**这些名字会被采信。
                    let explicit_fam: Vec<String> = scalars
                        .iter()
                        .filter(|(k, _)| crate::gl_ctx::fx_dbg_family(k))
                        .map(|(k, v)| format!("{k}={v:?}"))
                        .collect();

                    // 效果趟的**中性默认值**：这些 uniform 场景一般不给，而 shader 会拿它们
                    // 直接做 `normalize(mul(dir, CAST3X3(g_EffectTextureProjectionMatrixInverse)))`
                    // 这类运算 —— 保持 0 就是 `normalize((0,0))` ⇒ NaN ⇒ 采样坐标 NaN ⇒ 整层发黑
                    // （实测 `depthparallax` 一步就把人物那层亮度从 133 打到 33）。
                    // 没有鼠标输入 ⇒ 视差位置取中心（0.5,0.5）= 中性。
                    // `g_EffectTextureProjectionMatrix(Inverse)`：**图层空间下给真实投影链**
                    // （`etvp_inv`，见上面 3244 行起的注释）；画布空间 / `WE_ETVP=identity`
                    // / 矩阵奇异时仍是单位阵（= 改动前的逐字节行为）。
                    #[rustfmt::skip]
                    let identity: [f32; 16] = [
                        1.0, 0.0, 0.0, 0.0,
                        0.0, 1.0, 0.0, 0.0,
                        0.0, 0.0, 1.0, 0.0,
                        0.0, 0.0, 0.0, 1.0,
                    ];
                    for (n, v) in [
                        ("g_ParallaxPosition", Value::V2(0.5, 0.5)),
                        ("g_EffectTextureProjectionMatrix", Value::M4(identity)),
                        (
                            "g_EffectTextureProjectionMatrixInverse",
                            Value::M4(etvp_inv.unwrap_or(identity)),
                        ),
                    ] {
                        if !scalars.iter().any(|(k, _)| k == n) {
                            scalars.push((n.to_string(), v));
                        }
                    }
                    // `WE_FX_DEFAULT=1`（默认关）：官方 scalar 缺省机制 —— **场景没给值**的
                    // 特效趟 uniform，落 **shader 注解里的 `"default"`**（我们现在停在 GLSL 的 0）。
                    // 这是上次 `NOTES §9` 分歧① 的修法（`g_AlphaMultiply` 没给 ⇒ 我们给 0，
                    // 而官方落注解 default=1）。双出处 / 判据见 `gl_ctx::fx_default_on` 的注释。
                    // ① 只认注解 label 属 alpha 族的 float（`gl_ctx::fx_default_label`）；
                    // ② `default` 必须能解析成 f32（空 / `"1 1 1"` / `"util/white"` ⇒ 跳过）；
                    // ③ 本趟 scalars 里已有本名（作者显式值）⇒ 不覆盖（显式优先，与 OWE 的
                    // `BuildMaterial` 先 seed default、`LoadConstvalue` 后覆盖的语序同向）。
                    // 放在**兜底 2 之前** ⇒ 注解 default **优先于**我们自己的 `range` 兜底 lo
                    // （官方的值优先于我们造的近似值）。开关关 ⇒ 这一段不进 ⇒ 逐字节不变。
                    // `WE_FX_DEFAULT_ALL=1`（默认关，可单独开、也可与 `WE_FX_DEFAULT` 同开）：
                    // 把①的**范围**从 alpha 族放宽成"**任何非空 material label**"（即把
                    // `fx_default_label` 那条判据换成"`u.prop` 非空"）⇒ 修上次 `NOTES §8`
                    // 交底的那一批（`waterwaves g_Scale` 注解 default 200 vs 我们 range 兜底
                    // 0.01、`shake g_Amp 0.1`、`foliagesway g_Strength 0.4`、`blend g_Multiply2..6`…）。
                    // 双出处与"为何不设白名单/为何不收底图趟"见 `gl_ctx::fx_default_all_on`。
                    let mut default_seeded: Vec<String> = Vec::new();
                    let seed_any = crate::gl_ctx::fx_default_all_on();
                    if crate::gl_ctx::fx_default_on() || seed_any {
                        for u in &meta.uniforms {
                            if u.ty != "float" {
                                continue;
                            }
                            // 判据只看数据：注解 label 属 alpha 族，或（ALL 开关下）label 非空
                            let in_scope = crate::gl_ctx::fx_default_label(&u.prop)
                                || (seed_any && !u.prop.trim().is_empty());
                            if !in_scope {
                                continue;
                            }
                            if scalars.iter().any(|(k, _)| k == &u.name) {
                                continue;
                            }
                            if let Ok(d) = u.default.trim().parse::<f32>() {
                                default_seeded.push(format!("{}={}", u.name, d));
                                scalars.push((u.name.clone(), Value::F(d)));
                            }
                        }
                    }
                    // `WE_FX_DEFAULT_VEC=1`（默认关，与上面两条标量开关**独立 A/B**）：
                    // 官方「**向量/矩阵族**注解 default」缺省机制 —— 上面两条只认 `u.ty ==
                    // "float"`，向量族**一律不兜** ⇒ 我们停在 GLSL 的 0。`foliagesway` 的
                    // `g_CornerWeights`（`vec4` 注解 default `"1 1 0 0"`）就死在这一条上：
                    // 场景没给 ⇒ 0 ⇒ 顶点形变的 `weight ≡ 0` ⇒ 该特效趟**建起来了但像素
                    // 0 变化**。
                    // 双出处（OWE `ShaderAnnot.cpp:190-204` 把向量 default 与标量 default 当
                    // 同一件事、`SceneMaterialBuilder.cpp:500` 全量进 `default_uniforms`、
                    // `UniformBuffer.cpp:160-174/289-294` 先 default 后显式）、字面量切分规则
                    // （`Core/Json.cpp:88-114` 的 `ArrayComponents`）与全部边界
                    // `gl_ctx::fx_default_vec_on`。
                    // 判据（纯数据，不看 id/名字/特效名）：注解 label 非空 + 声明类型属
                    // `ann_vec_ty` + `default` 能切出恰好 N 个数字 + 本趟还没给本名。
                    // 放在 scalar 注解 default **之后**、`range_lo` 兜底 **之前**。
                    let vec_decl: Vec<(String, String, String)> = if crate::gl_ctx::fx_default_vec_on()
                        || std::env::var("WE_DBG_FX_DEFAULT").ok().as_deref() == Some("1")
                    {
                        meta.uniforms
                            .iter()
                            .filter(|u| ann_vec_ty(&u.ty) && !u.prop.trim().is_empty())
                            .map(|u| (u.name.clone(), u.ty.clone(), u.default.clone()))
                            .collect()
                    } else {
                        Vec::new()
                    };
                    let mut default_seeded_vec: Vec<String> = Vec::new();
                    if crate::gl_ctx::fx_default_vec_on() {
                        for (n, ty, d) in &vec_decl {
                            if scalars.iter().any(|(k, _)| k == n) {
                                continue;
                            }
                            if let Some(v) = ann_vec_default(d, ty) {
                                default_seeded_vec.push(format!("{}={:?}", n, v));
                                scalars.push((n.clone(), v));
                            }
                        }
                    }
                    // 诊断（`WE_DBG_FX_DEFAULT=1`，默认关，无副作用）：逐趟打印**向量族注解
                    // 声明**、其中"场景已经给了"的、以及"我们兜的" —— 「哪些场景/对象/趟改变、
                    // 改多少」那份清单就是靠它 + PNG 逐像素统计做出来的。
                    if std::env::var("WE_DBG_FX_DEFAULT").ok().as_deref() == Some("1") {
                        let seeded: Vec<&str> = default_seeded_vec
                            .iter()
                            .map(|s| s.split('=').next().unwrap_or(""))
                            .collect();
                        let given: Vec<String> = vec_decl
                            .iter()
                            .filter(|(n, _, _)| {
                                scalars.iter().any(|(k, _)| k == n) && !seeded.contains(&n.as_str())
                            })
                            .map(|(n, _, d)| format!("{}={}", n, d))
                            .collect();
                        let out_of_scope: Vec<String> = vec_decl
                            .iter()
                            .filter(|(n, ty, d)| {
                                !scalars.iter().any(|(k, _)| k == n)
                                    && ann_vec_default(d, ty).is_none()
                            })
                            .map(|(n, _, d)| format!("{}={}", n, d))
                            .collect();
                        eprintln!(
                            "[特效vecdefault] id={} 「{}」特效{}趟{} shader={} 注解向量声明={:?} 场景已给={:?} 我们兜={:?} 判据外={:?}",
                            obj.id, obj.name, ei, i, mp.shader, vec_decl, given, default_seeded_vec, out_of_scope
                        );
                    }
                    // 兜底 2：**注解写了 `range:[lo,hi]` 且 lo>0 的 float**，场景不给时用 `lo`
                    // 而不是 0。理由：0 是作者合同外的值（编辑器滑杆给不出），而这类 uniform
                    // 在 shader 里常被直接拿去**做分母**（`1.0/x`、`Res.z/Res.w * g_Ratio`）⇒
                    // 0 ⇒ Inf ⇒ 再乘 0 得 NaN ⇒ 采样坐标 NaN ⇒ **整屏平色**
                    // （实测 `2482408967`：`foliagesway` 的 `g_Ratio` 场景没给 ⇒ 0 ⇒ 1/aspect=Inf
                    // ⇒ 片元里 `v_TexCoordNoise.z * dot(sines, amp)` 里 amp=0 ⇒ Inf*0=NaN ⇒ 武士画像
                    // 被压成一片色，标准差 0）。
                    for u in &meta.uniforms {
                        if u.ty != "float" {
                            continue;
                        }
                        let Some(lo) = u.range_lo else { continue };
                        if lo <= 0.0 || scalars.iter().any(|(k, _)| k == &u.name) {
                            continue;
                        }
                        // `WE_FX_ALPHA=1`：官方 `NeutralColorUniforms` 名单里的 alpha 族
                        // **不吃我们的 range 兜底** —— 那几支"场景没给"时的正确值是官方缺省
                        // 1.0（由 `draw_pass` 的 P_FIXED 段兜），而不是我们造的 `lo`
                        // （`opacity.frag` 的 `g_UserAlpha` range=[0.01,1] ⇒ lo=0.01）。
                        // 跳过之后，`draw_pass` 的 `WE_FX_ALPHA` 分支才会把"没给"判成 1.0。
                        // 开关关 ⇒ 逐字节不变（这一支根本不进）。
                        if crate::gl_ctx::fx_alpha_on() && crate::gl_ctx::fx_alpha_governs(&u.name) {
                            continue;
                        }
                        scalars.push((u.name.clone(), Value::F(lo)));
                    }

                    // 诊断（`WE_DBG_FX_ALPHA=1`，默认关，无副作用）：把这一趟（特效趟）的
                    // alpha 族 scalars 打出来，并**分开**列出"显式给的"（ef/ep/ov）与
                    // "我们兜的"，供「`WE_FX_ALPHA=1` 会改变哪些趟」的清单取证。
                    // `explicit_fam` 在兜底之前采集 ⇒ 只含真正来自作者数据的名字。
                    if std::env::var("WE_DBG_FX_ALPHA").ok().as_deref() == Some("1") {
                        let fam: Vec<String> = scalars
                            .iter()
                            .filter(|(k, _)| crate::gl_ctx::fx_dbg_family(k))
                            .map(|(k, v)| format!("{k}={v:?}"))
                            .collect();
                        eprintln!(
                            "[特效alpha] id={} 「{}」特效{}趟{} shader={} 显式={:?} 注解default兜底={:?} 最终={:?}",
                            obj.id, obj.name, ei, i, mp.shader, explicit_fam, default_seeded, fam
                        );
                    }

                    // 诊断（`WE_DEBUG_LAYERS=1`，默认关、无副作用）：把**这一趟最终下发的常量**
                    // 打出来。以前只打 combos/槽位，于是"作者给了 `multiply: 0.66` 但像素等价于
                    // 1.0"这种最难查的情形没法一眼定：常量在不在 `scalars` 里、值是多少、名字
                    // 解析成哪个 uniform，全都在这一行里。
                    if dbg {
                        let sv: Vec<String> =
                            scalars.iter().map(|(k, v)| format!("{k}={v:?}")).collect();
                        eprintln!("[图层]     常量 {:?}", sv);
                    }

                    // `WE_FX_CONST=<name>=<f>[;…]`（默认不设 = 不动）：诊断用——把**任意**特效趟
                    // 的常量按 `uniform 名` 强行覆盖。用途：分辨"作者值没进 scalars"与
                    // "进了但像素上等价于别的值"这两类完全不同的故障（后者只能靠强制两个不同的
                    // 值 + 比像素来判）。名字写完整 uniform 名（如 `g_Multiply`），不写标签名。
                    if let Ok(spec) = std::env::var("WE_FX_CONST") {
                        for pair in spec.split(';') {
                            if let Some((k, v)) = pair.split_once('=') {
                                if let Ok(f) = v.trim().parse::<f32>() {
                                    let k = k.trim();
                                    if let Some(slot) = scalars.iter_mut().find(|(n, _)| n == k) {
                                        slot.1 = Value::F(f);
                                    } else {
                                        scalars.push((k.to_string(), Value::F(f)));
                                    }
                                    eprintln!("[诊断] WE_FX_CONST ⇒ {k}={f}");
                                }
                            }
                        }
                    }

                    if std::env::var_os("WE_DOT_SWAP").is_some() && bound.len() >= 3 {
                        // 诊断实验（默认关）：把**噪声槽(g_Texture1)** 与**遮罩槽(g_Texture2)** 的贴图
                        // 对调，用来验证"作者那张 `渐变黑` 到底该喂哪个槽"——喂噪声槽 ⇒
                        // `thresh = baseSize + noise*mapInfluence` 变成一条**位置梯度**
                        // （干净的 大→小），喂遮罩槽 ⇒ 只能是一条不透明度渐变。两条都量出数字后再定。
                        let a = units.iter().position(|(k, _)| *k == 1);
                        let b = units.iter().position(|(k, _)| *k == 2);
                        if let (Some(ia), Some(ib)) = (a, b) {
                            let ta = units[ia].1;
                            let tb = units[ib].1;
                            if let (Src::Tex(x), Src::Tex(y)) = (ta, tb) {
                                units[ia].1 = Src::Tex(y);
                                units[ib].1 = Src::Tex(x);
                                eprintln!("[诊断] WE_DOT_SWAP ⇒ 槽1/槽2 贴图对调");
                            }
                        }
                    }

                    let tname = ov
                        .and_then(|o| o.target.clone())
                        .or_else(|| ep.target.clone());
                    // ⚠ **特效自己的命名中间缓冲**：`blur` 用 `_rt_QuarterCompoBuffer1/2`、
                    // `shine` 用 `_rt_HalfCompoBuffer1/2`。它们必须是**真实 RT，且尺寸 = 画布 /4 或 /2**
                    // WE 就是靠"目标更小 + 双线性过滤"完成降采样的（`effect.json` 的 pass0 就是
                    // `previous → _rt_QuarterCompoBuffer1`）。以前这里只在 `rt_index` 里查，查不到就
                    // 回落成链的乒乓缓冲 ⇒ 这些名字**永远不会存在** ⇒ "降采样→模糊→升采样"整段没发生
                    // ⇒ **该糊的地方一点不糊**（用户报的"虚化有问题"；Ice Princess 的 blur/shine 正是这两组）。
                    let target = match tname.as_deref() {
                        Some(t) if self.rt_index.contains_key(t) => self.rt_index[t],
                        Some(t) if t.starts_with("_rt_") => {
                            // 调试旋钮 `WE_QSSCALE=<f32>`：这些命名中间缓冲的尺寸倍数。
                            // 依据：参考实现 `CImage.cpp:653` 用 `fboProvider->create(*fbo, …,
                            // this->getSize)` ⇒ 尺寸 = **对象尺寸 / fbo.scale**，而全屏后处理层的
                            // 对象尺寸是**场景/摄像机尺寸**。若这里用输出尺寸当基准，降采样倍数与
                            // 模糊半径都会偏（实测人物/人群区锐度差 2.4 倍）。
                            let qs: f32 = std::env::var("WE_QSSCALE")
                                .ok()
                                .and_then(|s| s.trim().parse().ok())
                                .unwrap_or(1.0);
                            let (dw, dh) = if t.contains("Quarter") {
                                ((lw as f32 * qs / 4.0) as u32, (lh as f32 * qs / 4.0) as u32)
                            } else if t.contains("Half") {
                                ((lw as f32 * qs / 2.0) as u32, (lh as f32 * qs / 2.0) as u32)
                            } else {
                                (lw, lh)
                            };
                            self.new_rt(dw.max(1), dh.max(1), t)?
                        }
                        // 没有写目标 ⇒ 落到链缓冲，但**必须避开当前输入**（否则同一张纹理
                        // 又读又写就是反馈环）。WE 这里也是 a/b 乒乓。
                        _ => {
                            if matches!(prev, Src::Rt(i) if i == chain_pair[0]) {
                                chain_pair[1]
                            } else {
                                chain_pair[0]
                            }
                        }
                    };
                    if dbg {
                        let desc: Vec<String> = units
                            .iter()
                            .map(|(k, s)| match s {
                                Src::Tex(i) => format!("{k}=贴图#{:?}", self.texs[*i].0),
                                Src::Rt(i) => format!("{k}=RT#{i}"),
                                Src::White => format!("{k}=白"),
                            })
                            .collect();
                        eprintln!("[图层]     实际绑槽位 = {desc:?}");
                    }
                    if std::env::var("WE_DEBUG_PASSES").is_ok() {
                        eprintln!(
                            "[调试] 建第 {} 趟 = 特效「{}」第 {i} 趟 shader={}  输入={:?} → 目标=RT#{}",
                            self.passes.len(),
                            eff.file,
                            mp.shader,
                            prev,
                            target
                        );
                    }
                    // `WE_TEXT_BLENDSCALE=<f>`（默认不设 = 用作者值）：强制**文字层** `effects/blend` 的
                    // `g_BlendScale`。（注意与既有 `WE_BLEND_SCALE` 无关 —— 那个是 puppet 加法层的
                    // 姿态 blend 权重。）用途：作者写的 `blendscale = 2.2` **超出 shader 注解声明的
                    // 范围**（`blend.vert`: `g_BlendScale // {"range":[0.01,2],"default":1}`）⇒ 怀疑
                    // WE 对超范围值回落 **default 1.0**。这一支就是那根验证杠杆
                    // （放大率 = blendscale · quad_h / T0_h，见 `blend_t0_report` 的注释）。
                    if obj.kind() == we_scene::scene::ObjectKind::Text
                        && mp.shader.contains("blend")
                    {
                        if let Some(f) = std::env::var("WE_TEXT_BLENDSCALE")
                            .ok()
                            .and_then(|v| v.trim().parse::<f32>().ok())
                        {
                            scalars.push(("g_BlendScale".into(), Value::F(f)));
                            if std::env::var_os("WE_DBG_BLEND_T0").is_some() {
                                eprintln!(
                                    "[blendT0] id={} 强制 g_BlendScale={f}（作者值 ≈2.2）",
                                    obj.id
                                );
                            }
                        }
                    }
                    // `WE_BLEND_T0`（默认 `layer`）：文字层的 `effects/blend` 趟，
                    // `g_Texture0Resolution` 改报**场景画布**（像素口径与链接 RT 的
                    // "1 像素 = 1 场景单位" 对齐）。依据/算式见 `blend_t0_report` 的注释。
                    // scalars 在 `draw_pass` 里是**在纹理单元之后**下发的 ⇒ 同名覆盖生效。
                    if obj.kind() == we_scene::scene::ObjectKind::Text
                        && mp.shader.contains("blend")
                    {
                        let sz = we_scene::scene::parse_vec2(&obj.size).unwrap_or([0.0, 0.0]);
                        if let Some((tw_, th_)) =
                            blend_t0_report((sz[0].abs(), sz[1].abs()), self.scene_size)
                        {
                            if std::env::var_os("WE_DBG_BLEND_T0").is_some() {
                                eprintln!(
                                    "[blendT0] id={} 「{}」shader={} ⇒ g_Texture0Resolution 报 {}x{}（obj.size {}x{}，场景 {}x{}）",
                                    obj.id, obj.name, mp.shader, tw_, th_, sz[0], sz[1],
                                    self.scene_size.0, self.scene_size.1
                                );
                            }
                            scalars.push((
                                "g_Texture0Resolution".into(),
                                Value::V4(tw_, th_, tw_, th_),
                            ));
                        }
                    }
                        // 【实验开关 WE_FX_QUAD_FLIP】链上"读 RT 写 RT"的趟：GL 的 NDC y=+1 落在
                        // RT 的最后一行，而我们的 V 约定是 v=0 = 画面顶 ⇒ 这类趟读到的输入上下反了
                        // （WE 在 D3D 下这两件事天然一致）。判据：武士 Berserk 用 V 反向四边形时与参照帧
                        // 差 1.45/255，且"关掉特效后该翻转反而有害"⇒ 它补偿的就是链上的那一次翻转。
                        let mut layer = layer;
                        // 【默认开】链上"读 RT 写 RT"的全屏趟必须用 V 反向四边形。
                        // 实测 vs Almamu 参照帧：Citlali 18.04→2.93、武士 57.01→1.48、
                        // Sona 48.81→13.25、Gaze/Fantasy Woman 纹丝不动。设 WE_FX_QUAD_FLIP=0 关掉做 A/B。
                        // ⚠ shape 的**四边形收尾趟**不能套这条：它的 uv 是作者那四个点
                        // （不是全屏四角），V 反向会直接把扇区的参数化上下翻过来；
                        // 底图趟（同一份四边形、同一份 shader）用的就是**不反**那一套。
                        if quad_last.is_none()
                            && std::env::var("WE_FX_QUAD_FLIP").ok().as_deref() != Some("0")
                        {
                            layer.raw_position = true; // 用 vao_flip_v（V 反向）
                        }
                        // 本趟的 shader 同时声明 `g_PointerPosition` 与
                        // `g_EffectTextureProjectionMatrixInverse` ⇒ 它把指针**反投影进本层的
                        // 纹理空间**（官方 `effects/xray` 这一族）⇒ 没有指针输入时必须给它一个
                        // 真实位置（`pointer_default`），否则整条效果静默失效。见字段注释。
                        // 判据只看**本趟 shader 原文里的 `uniform` 声明**（数据驱动，不看
                        // id / item 名；也不能用 `meta` —— 它只收录带注解的 uniform，而这两个
                        // 引擎槽位没有注解）。
                        let ptr_decl = shader::declares_uniform(&v_raw, "g_PointerPosition")
                            || shader::declares_uniform(&f_raw, "g_PointerPosition")
                            || shader::declares_uniform(&v_raw, "g_PointerPositionLast")
                            || shader::declares_uniform(&f_raw, "g_PointerPositionLast");
                        let etvpi_decl =
                            shader::declares_uniform(&v_raw, "g_EffectTextureProjectionMatrixInverse")
                                || shader::declares_uniform(
                                    &f_raw,
                                    "g_EffectTextureProjectionMatrixInverse",
                                );
                        if ptr_decl && etvpi_decl {
                            self.pointer_proj.insert(self.passes.len());
                            if std::env::var_os("WE_DEBUG_POINTER").is_some() {
                                eprintln!(
                                    "[指针] 趟 {} = 特效「{}」第 {i} 趟（{}）同时声明 g_PointerPosition + ETVP 逆阵 ⇒ 无输入时拿兜底位",
                                    self.passes.len(),
                                    eff.file,
                                    mp.shader
                                );
                            }
                        }
                        // `WE_DBG_DOT_CONST=1`（诊断，默认关，**不改任何下发值**）：
                        // 把「按 thresh 画圆」的那一族趟（点阵/粒子：本趟 scalars 里有 `u_baseSize`）
                        // 的**最终生效常量**、目标 RT 尺寸、对象矩形、指针位置一次打出来。
                        // 为什么需要它：「屏上点比离线小 ~25%，位置/间距完全一致」有两条
                        // 完全不同的根因 —— ①常量取错源（场景 constantshadervalues vs 材质注解
                        // default）；②口径不同（RT 尺寸 / 层矩形 / 指针）。两者在图上都只是
                        // "点小一点"，只能靠把**同一趟的输入**并排打出来分辨。内核与离线共用
                        // 这一份代码 ⇒ 各跑一次、diff 两行即可。
                        // 判据只看数据（scalars 里有没有 u_baseSize），不看对象 id / 特效名。
                        if std::env::var_os("WE_DBG_DOT_CONST").is_some()
                            && scalars.iter().any(|(k, _)| k == "u_baseSize")
                        {
                            let f_of = |n: &str| -> Option<f32> {
                                scalars.iter().find(|(k, _)| k == n).map(|(_, v)| match v {
                                    Value::F(x) => *x,
                                    Value::I(x) => *x as f32,
                                    _ => f32::NAN,
                                })
                            };
                            let v_of = |n: &str| -> Option<(f32, f32)> {
                                scalars.iter().find(|(k, _)| k == n).map(|(_, v)| match v {
                                    Value::V2(a, b) => (*a, *b),
                                    Value::V4(a, b, _, _) => (*a, *b),
                                    _ => (f32::NAN, f32::NAN),
                                })
                            };
                            let fmt_f = |n: &str| -> String {
                                match f_of(n) {
                                    Some(x) => format!("{n}={x:.5}"),
                                    None => format!("{n}=<缺>"),
                                }
                            };
                            let fmt_v = |n: &str| -> String {
                                match v_of(n) {
                                    Some((a, b)) => format!("{n}=({a:.5},{b:.5})"),
                                    None => format!("{n}=<缺>"),
                                }
                            };
                            // 分辨率口径（ 第二步）：本趟 scalars 里有没有 `g_Texture{N}Resolution`
                            // 报的是什么。点阵 shader 的 `ratCorr` 与噪声采样位置都从它来 ⇒
                            // 「RT 尺寸不同」会从这里直接看出来。
                            let res_of = |n: &str| -> String {
                                match scalars.iter().find(|(k, _)| k == n) {
                                    Some((_, Value::V4(a, b, c, d))) => {
                                        format!("{n}=V4({a:.1},{b:.1},{c:.1},{d:.1})")
                                    }
                                    Some((_, v)) => format!("{n}={v:?}"),
                                    None => format!("{n}=<缺>"),
                                }
                            };
                            // 分辨率槽（`g_Texture{N}Resolution`）不在 pass.scalars 里：它们在
                            // `draw_pass` 里按**实际绑定的 GL 单元**下发（源尺寸/采样尺寸）⇒
                            // 「RT 尺寸口径」要看同一趟的 `WE_DBG_TEXEXT` 输出，这里只报缺席情况。
                            let resline = format!(
                                "{} {} {} | scalars={}",
                                res_of("g_Texture0Resolution"),
                                res_of("g_Texture1Resolution"),
                                res_of("g_Texture2Resolution"),
                                scalars.len()
                            );
                            eprintln!(
                                "[点阵常量·分辨率] id={} {resline}（分辨率槽由 draw_pass 按 GL 单元下发 ⇒ 对照 WE_DBG_TEXEXT）",
                                obj.id
                            );
                            let sz = we_scene::scene::parse_vec2(&obj.size).unwrap_or([0.0, 0.0]);
                            let ptr = match self.mouse {
                                Some((mx, my)) => (mx, my),
                                None => (self.pointer_default[0], self.pointer_default[1]),
                            };
                            let (tw, th) = (self.rts[target].w, self.rts[target].h);
                            eprintln!(
                                "[点阵常量] id={} 「{}」shader={} 目标RT={}x{} obj.size={:.3}x{:.3} \
                                 指针=({:.4},{:.4}){} | {} {} {} {} {} {} {} {} {} {}",
                                obj.id,
                                obj.name,
                                mp.shader,
                                tw,
                                th,
                                sz[0],
                                sz[1],
                                ptr.0,
                                ptr.1,
                                if self.mouse.is_some() { "(输入)" } else { "(兜底)" },
                                fmt_f("u_baseSize"),
                                fmt_f("u_mapInfluence"),
                                fmt_f("u_mouseInfluence"),
                                fmt_f("u_mouseRadius"),
                                fmt_f("u_scale"),
                                fmt_f("u_rotation"),
                                fmt_f("u_smoothing"),
                                fmt_f("u_alpha"),
                                fmt_v("u_movement"),
                                fmt_v("u_noiseMovement"),
                            );
                        }

                        // 在这一趟之前插「把本趟的**画布空间贴图**按网格蒙皮画进帧空间」
                        // 的烘焙趟，并把对应的槽改绑那张 RT。为什么不改 shader：烘焙后的缓冲与
                        // 内容缓冲**同一空间、同一套四边形约定** ⇒ `v_TexCoord.zw` 原样采样即对齐
                        // （连 `WE_FX_QUAD_FLIP` 的 V 反向一起继承，不需要任何 ±v 补偿）。
                        // 底图趟一个字节没动 ⇒ 网格动画照旧。
                        // 为什么不止遮罩：`effects/shake` 的位移场是从 **`g_Texture1`（flowmask）
                        // 那张作者画的图**里取的（MASK combo 没开，遮罩分支被编译掉）⇒ 只修遮罩槽
                        // 会漏掉它，髋部那块仍会被摇（实测 HIP 3.67）。凡是「作者按画布画的贴图」
                        // 都要一起换空间 —— 判据就是 **`Src::Tex` 且边长 > 4**（`util/black` 这类
                        // 1x1 常量不搬）；RT 输入（`Src::Rt`）本来就是帧/链空间，绝不动。
                        // `WE_FUSE_SHIFT`：记下烘焙改写**之前**的槽位表 + 本趟的烘焙趟
                        // （融合时每个 stage 的遮罩要各自搬一张 RT —— 那个池是被复用的）。
                        let units_pre_bake = units.clone();
                        let mut bake_marks: Vec<(usize, usize, u32)> = Vec::new();
                        let mut warp_srcs: Vec<usize> = Vec::new();
                        if self.cur_mask_warp_src.is_some() && !mask_warp_slots.is_empty() {
                            for (u, s) in units.iter() {
                                if !mask_warp_slots.contains(u) {
                                    continue;
                                }
                                if let Src::Tex(i) = *s {
                                    if !warp_srcs.contains(&i) {
                                        if let Some(t) = self.texs.get(i) {
                                            if t.1 > 4 && t.2 > 4 {
                                                warp_srcs.push(i);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if !warp_srcs.is_empty() {
                            if let Some((wpos, wuvs, widx)) = self.cur_mask_warp_src.clone() {
                                // `WE_PERF_MASK_HALF`：遮罩烘焙缓冲降到**一半边长**。
                                // 为什么几乎不损画质：烘焙源（本场景）只有 `1070x1750`，
                                // 而旧缓冲是 `2539x3570`（面积 5.6×）—— 一半边长后仍然
                                // ⊇ 源分辨率。遮罩是软边，下游按归一化 uv 采样。
                                // 用**逻辑**尺寸（`lw_log/lh_log`）：骨骼链自己的超采样
                                // （`WE_PERF_SUPERSAMPLE`）不能顺带把遮罩烘焙也缩掉 ——
                                // 两个旋钮必须正交，否则扫出来的曲线是两者之和。
                                let ms = perf_mask_scale();
                                let ss_mask = perf_supersample_mask();
                                let (mut sw, mut sh) = (0u32, 0u32);
                                if perf_mask_src_on() || ss_mask.is_some() {
                                    for i in warp_srcs.iter().copied() {
                                        if let Some(t) = self.texs.get(i) {
                                            sw = sw.max(t.1);
                                            sh = sh.max(t.2);
                                        }
                                    }
                                }
                                let mh = ms < 0.995 || perf_mask_src_on() || ss_mask.is_some();
                                // `WE_PERF_SUPERSAMPLE_MASK=f`：缓冲 = f × 源贴图像素（逐轴），
                                // 再夹到逻辑尺寸以内（不能比"不缩"更大）。
                                let (mut mw, mut mht) = if let Some(f) = ss_mask {
                                    if sw > 0 && sh > 0 {
                                        supersample_mask_dims(
                                            f,
                                            sw,
                                            sh,
                                            lw_log,
                                            lh_log,
                                        )
                                    } else {
                                        (lw_log.max(1), lh_log.max(1))
                                    }
                                } else if mh {
                                    (
                                        ((lw_log.max(1) as f32 * ms).round() as u32).max(1),
                                        ((lh_log.max(1) as f32 * ms).round() as u32).max(1),
                                    )
                                } else {
                                    (lw_log.max(1), lh_log.max(1))
                                };
                                // `WE_PERF_MASK_SRC`：再夹到**源贴图像素尺寸**（逐轴）。
                                // 烘焙趟按归一化 uv 铺满缓冲 ⇒ 两轴独立夹不影响几何，
                                // 只是不再把源纹素上采样成 4.7 个缓冲像素。
                                if perf_mask_src_on() && sw > 0 && sh > 0 {
                                    mw = mw.min(sw).max(1);
                                    mht = mht.min(sh).max(1);
                                }
                                // `WE_MASK_FULLSCREEN`：全屏一张、跨趟复用（win 的
                                // `_rt_FullAlphaMask`）。像素尺寸 = 输出（屏幕）尺寸；
                                // `WE_MASK_FULLSCREEN_SIZE=ss` 用「输出 × 超采样倍率」（保真档 A/B）。
                                // 内容在归一化 uv 里与旧口径**逐点相同**（只改采样密度）⇒
                                // 下游采样坐标不用动；但上报尺寸必须钉回旧值（见下面 `rep`）。
                                let fs_mask = mask_fullscreen_on();
                                // 尺寸档（`WE_MASK_FULLSCREEN_SIZE`）：
                                // · 默认 `screen` = 输出尺寸（win `_rt_FullAlphaMask` 的口径）；
                                // · `ss` = 输出 × 超采样倍率（保真档 A/B）；
                                // · `fit` = **保持链缓冲长宽比**、面积 = 输出面积（逐轴等比缩放）。
                                // 为什么需要 fit：本场景链 2539×3570（竖长），直接塞进 2560×1440
                                // 会把**纵向**采样密度砍到源贴图以下（1440 < 1750）⇒ 遮罩边缘会出现
                                // 各向异性的软/硬差（实测 max 8×8 块 4.07，刚过 4 的门）。
                                let fs_size_mode = std::env::var("WE_MASK_FULLSCREEN_SIZE")
                                    .unwrap_or_default()
                                    .trim()
                                    .to_ascii_lowercase();
                                let (fsw, fsh) = if fs_size_mode == "fit" {
                                    // 消费端是**物理链缓冲**（`lw/lh`，已含超采样/降采样）⇒ 用它的面积做基准；
                                    // 用逻辑尺寸（`lw_log*`）会在超采样档把预算算大（本场景 4.53 vs 3.69 MP）。
                                    let (aw, ah) = (lw.max(1) as f32, lh.max(1) as f32);
                                    let chain_area = aw * ah;
                                    let screen_area = (self.gl.w as f32) * (self.gl.h as f32);
                                    // 像素预算：**不低于链缓冲的一半**（面积），也不高于链缓冲本身。
                                    // 为什么有这条下限：消费端就是链缓冲，遮罩分辨率掉到它的一半
                                    // 以下时，遮罩的硬边会被重采样放大成可测的差
                                    // （实测 1920x1080 无超采样档：f=0.478→max 8×8 块 4.15 > 4；
                                    // 预算 0.5 链面积后 2.85 ✓）。2560+SS 档本来就低于一半 ⇒ 不变。
                                    // `WE_MASK_FULLSCREEN_SCALE=f`：再乘一个系数（扫档/A-B 用）。
                                    let f = env_f32("WE_MASK_FULLSCREEN_SCALE", 1.0).clamp(0.25, 4.0);
                                    let target = (screen_area.max(0.5 * chain_area))
                                        .min(chain_area);
                                    let s = (target / chain_area).sqrt().clamp(0.05, 4.0) * f;
                                    (
                                        ((aw * s).round() as u32).max(1),
                                        ((ah * s).round() as u32).max(1),
                                    )
                                } else {
                                    let fs_scale = if fs_mask && fs_size_mode == "ss" {
                                        perf_supersample_mesh().unwrap_or(1.0).max(1.0)
                                    } else {
                                        1.0
                                    };
                                    (
                                        ((self.gl.w as f32 * fs_scale).round() as u32).max(1),
                                        ((self.gl.h as f32 * fs_scale).round() as u32).max(1),
                                    )
                                };
                                while self.cur_mask_warp_pool.len() < warp_srcs.len().min(2) {
                                    let n = self.cur_mask_warp_pool.len();
                                    // 上报尺寸：旧口径 = 「这张 RT 如果是链缓冲尺寸会是多大」
                                    // （`mh` 时旧代码报逻辑尺寸 `lw_log/lh_log`，否则报真实尺寸）。
                                    let rep = if mh {
                                        (lw_log.max(1), lh_log.max(1))
                                    } else {
                                        (mw, mht)
                                    };
                                    let (pw, ph, pname) = if fs_mask {
                                        (fsw, fsh, format!("_rt_maskwarpfs{}_{}", n, obj.id))
                                    } else {
                                        (mw, mht, format!("_rt_maskwarp{}_{}", n, obj.id))
                                    };
                                    let r = self.new_rt(pw, ph, &pname)?;
                                    if fs_mask || mh {
                                        self.rt_report[r] = Some(rep);
                                    }
                                    self.cur_mask_warp_pool.push(r);
                                }
                                let uvd: Vec<(String, i32)> =
                                    if std::env::var_os("WE_WARP_UVDBG").is_some() {
                                        vec![("WE_WARP_UVDBG".to_string(), 1)]
                                    } else {
                                        Vec::new()
                                    };
                                // `WE_PERF_MASK_NOCLEAR`：换写成"预乘 + 关混合"的片元
                                // `mask_noclear_on` 的等价性推导），于是不用每趟清整张缓冲。
                                // 全屏复用模式下这套等价性**不成立**（一张 RT 被 9 趟
                                // 不同遮罩轮流写，网格盖不满 ⇒ 必须先清屏），所以强制走清屏那支。
                                let noclear = mask_noclear_on() && !fs_mask;
                                let (wvs, wfs) = if noclear {
                                    shader::compose_stages_with(
                                        MASK_WARP_VS,
                                        MASK_WARP_FS_PREMULT,
                                        &uvd,
                                    )
                                } else {
                                    shader::compose_stages_with(MASK_WARP_VS, MASK_WARP_FS, &uvd)
                                };
                                for (k, wtex) in warp_srcs.iter().copied().enumerate().take(2) {
                                    // 诊断（`WE_WARP_SELFTEST=1`）：第 0 趟改绑**本层自己的贴图**。
                                    // 若烘焙几何与底图趟完全一致，这张烘焙缓冲应与底图趟缓冲逐像素近似
                                    // ⇒ 一眼分辨"仿射/朝向错"还是"遮罩本身就不在那儿"。
                                    let wtex = if k == 0
                                        && std::env::var_os("WE_WARP_SELFTEST").is_some()
                                    {
                                        match base_src {
                                            Src::Tex(bi) => bi,
                                            _ => wtex,
                                        }
                                    } else {
                                        wtex
                                    };
                                    let wrt = self.cur_mask_warp_pool[k];
                                    let mut wlayer = self.gl.make_indexed_layer(
                                        &wvs, &wfs, &wpos, &wuvs, &widx, [0.0; 5],
                                    )?;
                                    // 必须和底图趟用**同一套 uv 朝向**：底图趟在图层空间里可能
                                    // 用 V 反向的网格 VAO（`WE_LAYER_VFLIP`，见 `cur_mesh_base_flip`），
                                    // 内容按 `1−v` 采样贴图。烘焙趟若用不反的那套，遮罩就会落在
                                    // **关于水平中线镜像**的位置（这正是 里"翻 u/翻 v 都只是搬家"
                                    // 的那个镜像）。两趟同源 ⇒ 对齐。
                                    wlayer.raw_position = if std::env::var_os("WE_WARP_FLIP").is_some()
                                    {
                                        !self.cur_mesh_base_flip
                                    } else {
                                        self.cur_mesh_base_flip
                                    };
                                    self.mask_warp_passes.push(self.passes.len());
                                    let wpi = self.passes.len();
                                    match self.cur_mask_warp_base {
                                        Some(b) => {
                                            match self
                                                .mask_warp_groups
                                                .iter_mut()
                                                .find(|(bb, _)| *bb == b)
                                            {
                                                Some((_, v)) => v.push(wpi),
                                                None => {
                                                    self.mask_warp_groups.push((b, vec![wpi]))
                                                }
                                            }
                                        }
                                        None => {
                                            self.mask_warp_groups.push((usize::MAX, vec![wpi]))
                                        }
                                    }
                                    self.passes.push(Pass {
                                        layer: wlayer,
                                        target: Some(wrt),
                                        units: vec![(0, Src::Tex(wtex))],
                                        scalars: Vec::new(),
                                        mvp: self.cur_mask_warp_mvp.unwrap_or(full_q),
                                        blend: if noclear {
                                            gl_ctx::Blend::Opaque
                                        } else {
                                            self.cur_mask_warp_blend
                                        },
                                        // 顶点由底图趟那份 `MeshAnim` 每帧算好、统一重传
                                        // （见 draw_frame 的 `mask_warp_pos`），这里不再挂第二份。
                                        mesh_anim: None,
                                    });
                                    // Record the SHADER slots this bake feeds (not the pool slot k):
                                    // several units can reference the same author texture.
                                    for (su, ss) in units.iter() {
                                        if let Src::Tex(ti) = *ss {
                                            if ti == wtex {
                                                bake_marks.push((wpi, wrt, *su));
                                            }
                                        }
                                    }
                                    let _ = k;
                                    for (u, s) in units.iter_mut() {
                                        if let Src::Tex(i) = *s {
                                            if i == wtex {
                                                *s = Src::Rt(wrt);
                                            }

                                        }
                                    }
                                    if dbg {
                                        eprintln!(
                                            "[图层]     遮罩烘焙趟 ⇒ {} 贴图#{wtex} → RT#{wrt}（{}x{}，与底图同一 MVP）",
                                            mp.shader, lw, lh
                                        );
                                    }
                                }
                            }
                        }
                        if !pass_scripts.is_empty() {
                            self.fx_scripts.push((self.passes.len(), pass_scripts));
                        }
                    if fuse_on() {
                        if std::env::var_os("WE_FUSE_DEBUG").is_some() {
                            eprintln!(
                                "[fuse-dbg] 记模板 趟{} shader={} 烘焙{}个",
                                self.passes.len(),
                                mp.shader,
                                bake_marks.len()
                            );
                        }
                        self.fuse_tpl.insert(
                            self.passes.len(),
                            FuseTpl {
                                vs: vs.clone(),
                                fs: fs.clone(),
                                combos: combos.clone(),
                                units: units_pre_bake.clone(),
                                scalars: scalars.clone(),
                                obj: obj.id,
                                bakes: bake_marks.clone(),
                            },
                        );
                    }
                        self.passes.push(Pass {
                            layer,
                            target: Some(target),
                        units,
                        scalars,
                        // shape 的四边形收尾趟：把四边形**摆进场景**（= 主相机下的对象几何）
                        mvp: if quad_last.is_some() {
                            obj_mvp
                        } else if card_applied {
                            // 几何已换成卡片网格 ⇒ MVP 乘 `diag(2/size)`（无位移时逐位等价；
                            // 有位移时位移量 = 官方语义，见 `fx_card_mvp`）。
                            fx_card_mvp(full_q, fx_card_px)
                        } else {
                            full_q
                        },
                        // 链内部的 RT→RT 趟：不混合
                        blend: gl_ctx::Blend::Opaque,
                        mesh_anim: None,
                    });
                    // 挂骨骼：shape 的四边形收尾趟 = 摆进场景的那一趟 ⇒ 登记逐帧
                    if quad_last.is_some() {
                        AttachHit::register(
                            &mut self.attach_passes,
                            &attach_hit,
                            attach_q_l,
                            obj_mvp_m,
                            self.passes.len() - 1,
                        );
                    }
                    prev = Src::Rt(target);
                }
                // 这个特效跑完，链的输出继续往后传
            }
            // `WE_FUSE_SHIFT`：把这一段链里连续的纯位移趟折成一趟（默认关；关时这里不进）
            if fuse_on() {
                if let Err(e) = self.try_fuse_chain(obj.id, fuse_start, self.passes.len()) {
                    eprintln!("[fuse] 层{} 融合放弃：{e}", obj.id);
                }
            }
            // 整条链跑完：把它贴回屏幕
            // （bug ②）：**mesh 图层**的收尾趟不再用四边形搬缓冲，而是**用网格几何**
            // 直接把"展开的图层缓冲"摆进场景：
            // · 顶点 = 本帧装配（逐帧蒙皮）后的顶点 —— 人物照旧是"活"的；
            // · 采样坐标 = **网格自己的 uv** —— 缓冲里的美术与混合贴图/遮罩同空间
            // ⇒ 丝袜/掩码跟着腿动，不再"留在原地"。
            // 这也正是参照实现的语义（`position == uv·size` ⇒ RT 是展开图集空间；
            // 装配发生在"Screen 几何"那一趟，即这里）。
            let mesh_comp = if layer_space { self.cur_mesh_comp.clone() } else { None };
            let mut blit = match mesh_comp.as_ref() {
                Some(mc) => {
                    let l = self.make_blit_layer_mesh(&mc.pos, &mc.uvs, &mc.indices, mc.sway)?;
                    if dbg {
                        eprintln!(
                            "[图层]   收尾趟用**网格几何**（装配 + 网格 uv；顶点 {} 索引 {}）⇒ 丝袜/掩码跟腿动",
                            mc.pos.len(),
                            mc.indices.len()
                        );
                    }
                    l
                }
                None => self.make_blit_layer()?,
            };
            if std::env::var("WE_FX_QUAD_FLIP").ok().as_deref() == Some("2") {
                blit.raw_position = true;
            }
            // 图层空间下收尾趟要做"把图层缓冲摆进场景"的映射：首趟（Copy）用**非翻转**四边形把美术
            // 铺满图层缓冲，收尾趟再按同样约定采样时方向正好相反 ⇒ 这里必须换 V 反向的那套四边形，
            // 否则整层上下翻转（实测：不翻时 Ice Princess 7.64 → 21.20，画面里背景整块倒过来）。
            // ⚠ mesh + uv 空间方案：几何换成网格后，采样坐标是**网格 uv**（不是四边形 uv）⇒
            // 方向关系与"四边形对四边形"不同，V 取与底图趟**相同**的朝向（底图趟写、收尾趟读，
            // 两次翻互相抵消）。`WE_MESH_UVSPACE_FLIP=1` 可反做 A/B。
            if layer_space && !text_chain_space {
                if mesh_comp.is_some() {
                    // 收尾趟的 uv 朝向 = **底图趟的 uv 朝向**（底图趟按 flip^F 写、收尾趟按 flip^C 读，
                    // 要还原原图需 flip^F∘flip^C = id ⇒ C = F）。`WE_MESH_UVSPACE_FLIP=1` 反做 A/B。
                    blit.raw_position = if std::env::var("WE_MESH_UVSPACE_FLIP").ok().as_deref()
                        == Some("1")
                    {
                        !self.cur_mesh_base_flip
                    } else {
                        self.cur_mesh_base_flip
                    };
                } else {
                    blit.raw_position = !blit.raw_position;
                }
            }
            // `WE_LAYER_UPRIGHT`：缓冲存正 ⇒ 收尾趟按**画布 uv** 直读（默认**不翻**）。
            // `WE_LAYER_UPRIGHT_BLIT=0|1` 可把这一位钉死做 A/B（错的那一档画面会上下颠倒）。
            if layer_upright_geom {
                blit.raw_position = match std::env::var("WE_LAYER_UPRIGHT_BLIT").ok().as_deref() {
                    Some("0") => false,
                    Some("1") => true,
                    _ => !blit.raw_position,
                };
            }
            // 收尾趟的 MVP：图层空间下就是\"把缓冲摆进场景\"。缓冲现在是【对象矩形 ∪ 网格】
            // 的取景框 ⇒ 在归一化坐标里再叠一次 `translate(取景框中心)·scale(取景框半尺寸)`，
            // 缓冲才落在正确的场景矩形上（默认取景框 = 对象矩形 ⇒ 这个变换是单位阵，
            // 与旧行为**逐字节一致**）。
            // 复用 `blit_place_m`（= 上面 `etvp_inv` 用的同一个矩阵）免得两处走偏。
            let blit_mvp = if layer_space && !text_chain_space {
                blit_place_m.transposed().column_major()
            } else {
                full_q
            };
            if std::env::var_os("WE_DEBUG_BLIT").is_some() {
                eprintln!(
                    "[BLIT] {} id={} layer_space={} base_local={} obj_mvp_m={:?} blit_place_m={:?} blit.raw_position={} upright={} upright_blit_env={:?}",
                    obj.name, obj.id, layer_space, chain_base_local, obj_mvp_m.m, blit_place_m.m,
                    blit.raw_position, layer_upright_geom,
                    std::env::var("WE_LAYER_UPRIGHT_BLIT").ok()
                );
            }
            if chain_base_local {
                if let Ok(v) = std::env::var("WE_CHAIN_BLIT_VFLIP") {
                    blit.raw_position = v == "1";
                }
            }
                // 链的收尾趟 ⇒ 关 alpha 写入（参照 CImage::render）
                if std::env::var_os("WE_FUSE_DEBUG").is_some() {
                    eprintln!("[fuse] 层{} blit 趟 #{} 读 {prev:?}", obj.id, self.passes.len());
                }
                self.alpha_off.insert(self.passes.len());
            self.passes.push(Pass {
                layer: blit,
                target: off_rt.or(self.scene_rt),
                units: vec![(0, prev)],
                scalars: vec![],
                // 图层空间下这一趟就是参照的**收尾趟 "Screen" 几何**：用对象的场景几何
                // （`proj × world × scale(size/2)`）把图层尺寸的结果**摆进场景**，UV 仍是 [0,1]。
                // 画布空间时保持老行为（全屏搬运）。
                mvp: blit_mvp,
                // 链的最终结果贴回屏幕 = 这一层真正和场景合成的地方 ⇒ 用材质的混合方式
                blend,
                // uv-空间方案：**逐帧装配在这里**（底图遈只画静态展开图）
                mesh_anim: if mesh_comp.is_some() { mesh_anim_h.take() } else { None },
            });
            // 相机视差：图层空间下**收尾趟**才是"把层放进场景"（链内趟都跑在缓冲空间里）。
            if let (Some(pi), true) = (par_info, layer_space) {
                self.parallax_passes.insert(self.passes.len() - 1, pi);
            }
            // 挂骨骼：图层空间下**收尾趟**就是"把这一层摆进场景"的那一趟（`mvp = blit_mvp`，
            // 与 `obj_mvp_m` 同形：`Q_L·world·(scale·取景框)`）⇒ 登记逐帧更新。
            // 画布空间下这趟是整屏搬运（`full_q`），不是摆放 ⇒ 不登记（那时底图趟已登记）。
            if layer_space {
                AttachHit::register(
                    &mut self.attach_passes,
                    &attach_hit,
                    attach_q_l,
                    blit_place_m,
                    self.passes.len() - 1,
                );
            }
            // **可见 + 被引用**的层也要有“真发布口”。
            // 官方（`SceneImageObjectParser.cpp:1058-1078`）给被引用层建两条独立趟：
            // `publish` = `effect_composite`（**链尾**）→ `_rt_imageLayerComposite_<id>`
            // `visible_resolve` = 链接 RT → 屏幕
            // ⇒ “发布”和“上屏”是两条路径，可以同时存在。
            // 我们过去把链接 RT 指向 `comp`（= 本层**底图**，特效之前），而收尾趟只画到屏幕
            // ⇒ 可见被引用层的消费方拿到的是“没跑特效的图”。这里在收尾趟**之后**再画一趟
            // 链尾 → `comp`（已 alias 成链接 RT），屏幕输出不受影响（收尾趟已入队）。
            // 不可见层不走这里：它们的收尾趟本来就以 `comp` 为目标（`off_rt`），
            // 再画一次就是 comp → comp 的自反馈。
            // ⚠ 链尾**已经**落在 `comp` 里时不要再画：有些特效的 `target` 直接写
            // `_rt_imageLayerComposite_<自己>_b`（= alias 到 `comp`）⇒ `prev == comp`，
            // 再画一趟就是 comp → comp 的自反馈（实测 Sona 整幅变暗：171/204/220 → 159/182/196）。
            let already_tail = matches!(prev, Src::Rt(t) if t == comp);
            if published && vis && link_rt_pub_tail() && !already_tail && !(link_offscreen && !vis) {
                let copy = self.make_copy_layer()?;
                self.passes.push(Pass {
                    layer: copy,
                    target: Some(comp),
                    units: vec![(0, prev)],
                    scalars: vec![],
                    mvp: full_q,
                    // 发布是“原样拷贝”，不与 RT 里的旧底图混合
                    blend: gl_ctx::Blend::Opaque,
                    mesh_anim: None,
                });
                if std::env::var_os("WE_DEBUG_LINKS").is_some() {
                    eprintln!(
                        "[链接RT] id={} 「{}」发布口 = 链尾（pass {}，RT {}x{}，screen 不变）",
                        obj.id, obj.name, self.passes.len() - 1, lw, lh
                    );
                }
            }
        }
        // ── 粒子对象 ──────────────────────────────────────────────────────
        // `WE_PARTICLE_LEGACY=1`：A/B 开关，把本次新实现的粒子语义整套关掉（详）
        // `particles.rs::legacy`）。默认 = 新语义。
        let particle_legacy =
            std::env::var("WE_PARTICLE_LEGACY").ok().as_deref() == Some("1");
        for (p_idx, obj) in scene.objects.iter().enumerate() {
            // ⚠ 以前这里**不认** `WE_SKIP_NAME`（只图层认）⇒ 想二分"哪个粒子对象在作怪"时
            // 逐名排除居然逐位不变（空转）。补上，让跳过对粒子同样生效。
            // 同理补上 `WE_ONLY_ID`：以前它**只对图层生效**，粒子对象全部照建
            // ⇒ 拿 `WE_ONLY_ID=<粒子对象id>` 做"单对象隔离"时，量到的 bbox/像素数里
            // 混着**场景里所有别的粒子对象**（本次一开始就是这么被误导的）。
            let only_ids_p: Vec<i64> = std::env::var("WE_ONLY_ID")
                .unwrap_or_default()
                .split(',')
                .filter_map(|s| s.trim().parse::<i64>().ok())
                .collect();
            if !only_ids_p.is_empty() && !only_ids_p.contains(&obj.id) {
                if std::env::var_os("WE_DEBUG_PARTICLES_BUILD").is_some() {
                    eprintln!("[粒子跳过] {}：不在 WE_ONLY_ID 里（id={}）", obj.name, obj.id);
                }
                continue;
            }
            // 诊断旋钮 `WE_PARTICLE_SKIP_ID=<id,id>`：**跳过这些粒子对象**（只影响粒子，
            // 不碰图层 —— 图层的跳过是 `WE_SKIP_ID`）。用途：拿**全帧** skip-diff 量某个
            // 粒子对象真正占的像素/落点（隔离渲染 WE_ONLY_ID 的落点与全帧不一致，不能用来量落点）。
            // 默认不设 ⇒ 一行不改，逐字节同旧行为。
            let skip_pids: Vec<i64> = std::env::var("WE_PARTICLE_SKIP_ID")
                .unwrap_or_default()
                .split(',')
                .filter_map(|s| s.trim().parse::<i64>().ok())
                .collect();
            if skip_pids.contains(&obj.id) {
                if std::env::var_os("WE_DEBUG_PARTICLES_BUILD").is_some() {
                    eprintln!("[粒子跳过] {}：被 WE_PARTICLE_SKIP_ID 跳过（id={}）", obj.name, obj.id);
                }
                continue;
            }
            if skip_names.iter().any(|s| obj.name.contains(s.as_str()))
                || (!only_names.is_empty()
                    && !only_names.iter().any(|s| obj.name.contains(s.as_str())))
            {
                continue;
            }
            // 以前这里四条路径全是**静默 continue** —— 实测 Ice Princess 28 个粒子对象只建起 4 个
            // （`Fog 1`/`Torch`/`Light shafts`/`Dust motes` 全部无声消失），而用户看到的"win 版那层
            // 白雾"极可能就是 `Fog 1`。没有日志就只能猜。
            let pdbg = std::env::var_os("WE_DEBUG_PARTICLES_BUILD").is_some();
            if !tree_visible(obj, &by_id) {
                if pdbg {
                    eprintln!("[粒子跳过] {}：树上不可见（自身或祖先 visible=false）", obj.name);
                }
                continue; // 隐藏的粒子对象也不该冒出来
            }
            let Some(pdef) = obj.particle.as_ref().and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(pb) = pkg.get(pkg_bytes, pdef) else {
                if pdbg {
                    eprintln!("[粒子跳过] {}：包里找不到预设 {pdef}", obj.name);
                }
                continue;
            };
            let Ok(preset) = we_scene::json::from_slice::<particles::ParticlePreset>(pb) else {
                if pdbg {
                    let why = we_scene::json::from_slice::<particles::ParticlePreset>(pb)
                        .err()
                        .map(|e| e.to_string())
                        .unwrap_or_default();
                    eprintln!("[粒子跳过] {}：预设 {pdef} 解析失败 —— {why}", obj.name);
                }
                continue;
            };
            // 材质 → shader + 贴图 + 混合
            let (shader_name, tex_name, additive, pmat_combos) = match pkg
                .get(pkg_bytes, &preset.material)
                .and_then(|b| we_scene::json::from_slice::<shader::MaterialFile>(b).ok())
                .and_then(|m| m.passes.into_iter().next())
            {
                Some(p) => (
                    p.shader,
                    p.textures.first().cloned().unwrap_or_default(),
                    p.blending.as_deref() == Some("additive"),
                    p.combos,
                ),
                None => {
                    if pdbg {
                        let mat = &preset.material;
                        let in_pkg = pkg.get(pkg_bytes, mat).is_some();
                        eprintln!(
                            "[粒子跳过] {}：材质 {mat} 取不到（在包里={in_pkg}）",
                            obj.name
                        );
                    }
                    continue;
                }
            };
            let v_raw = match shader::load_shader(pkg, pkg_bytes, assets, &shader_name, "vert") {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[场景] 粒子 shader 取不到（{shader_name}）：{e}");
                    continue;
                }
            };
            let f_raw = match shader::load_shader(pkg, pkg_bytes, assets, &shader_name, "frag") {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[场景] 粒子 shader 取不到（{shader_name}）：{e}");
                    continue;
                }
            };
            // combo：关掉光照 / 折射 / 抠图 / 雾；精灵表与"厚顶点格式"按需打开
            // （THICKFORMAT 必须开，`a_TexCoordVec4C1`（速度+寿命）才会存在 ——
            // 精灵表就是靠它选帧的）
            let frames_raw = Self::sprite_frames(assets, &tex_name);
            // 对照开关：强制关掉精灵表（`WE_NO_SPRITESHEET=1`）。用来判定
            // "64 帧选帧失效导致糊块"，还是"尺寸/贴图坐标"的问题：
            // 关掉后每个粒子会采样**整张** 1024² 精灵表 ⇒ 应该看到 8×8 的火苗网格。
            let no_sheet = std::env::var_os("WE_NO_SPRITESHEET").is_some();
            let frames = if no_sheet { 1 } else { frames_raw };
            // TEX0FORMAT：喂 WE 的贴图**通道格式码**，shader 里的 `ConvertTexture0Format`
            // 才会把 rg88/r8 这类"强度掩膜"贴图转成 (r,r,r,g) / (1,1,1,r)。不喂 = 硬边实心块。
            // 不变量（**本次根因修复**）：RG88/ 的通道语义必须**恰好应用一次**。
            // · `particle_tex0_format` 解析出 8/9 ⇒ **shader 负责**（`_sample.rrrg`），CPU 一个像素都不碰；
            // · 解析不出（0）⇒ shader 的转换是**恒等映射**，由 CPU 侧 `apply_channel_fmt*` 补上。
            // 两边同时作用 = 语义**复合**：`(r,r,r,g).rrrg = (r,r,r,1.0)`（改写后的 `.g` 已是 255）
            // ⇒ alpha 从 `a·g` 塌成 `a` ⇒ 加性项 `rgb·a` 直接放大 `1/ḡ` 倍
            // ⇒ **实测 4.402 / 4.173 / 5.103（R/G/B）≈ 4.3× 过曝**（`3704273480` 对象 250
            // `particle/light/light_shafts_6`；证据 说明）。
            // 取证：`WE_TEX_RGBA_FMT=1 + WE_PARTICLE_TEX0FORMAT=0`（CPU 改写 + shader 透传）与
            // 「默认」（shader 单次转换）**逐字节相同** ⇒ 两条路是同一语义的两次实现。
            // 开关（**默认开**，因为是正语义）：
            // · `WE_TEX_RGBA_FMT=0` ⇒ **整条语义关掉**（shader 与 CPU 都不动）⇒ 回到"硬边实心块"的
            // 旧对照行为，用来复现/排除。不设或 `=1` ⇒ 开。
            // · `WE_PARTICLE_TEX0FORMAT=<n>`（诊断）⇒ 强行覆盖本趟的 TEX0FORMAT。
            let rgba_fmt_on = std::env::var("WE_TEX_RGBA_FMT").ok().as_deref() != Some("0");
            let mut tex0_format = if rgba_fmt_on {
                Self::particle_tex0_format(pkg, pkg_bytes, assets, &tex_name)
            } else {
                0
            };
            if let Some(v) = std::env::var("WE_PARTICLE_TEX0FORMAT")
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
            {
                tex0_format = v;
            }
            // CPU 侧补位：**只有语义开着、而 shader 这趟不做转换时**才允许改像素
            // 不变量就在这里落地（开着时 shader 与 CPU **必有一个且只有一个**在做事）。
            // 诊断 `WE_PARTICLE_CPU_FMT=<0|1>`：强行指定。用它可以在**不重新编译**的情况下
            // 复现本次的历史故障态（`WE_PARTICLE_TEX0FORMAT=8 WE_PARTICLE_CPU_FMT=1`
            // = shader 转换 + CPU 改写**同时开** ⇒ 语义复合 ⇒ ×4.3 过曝）。
            let cpu_fmt = match std::env::var("WE_PARTICLE_CPU_FMT").ok().as_deref() {
                Some("1") => true,
                Some("0") => false,
                _ => rgba_fmt_on && tex0_format == 0,
            };
            let mut combos: Vec<(String, i32)> = [
                ("SPRITESHEET", i32::from(frames > 1)),
                ("THICKFORMAT", 1),
                ("TEX0FORMAT", tex0_format),
                ("LIGHTING", 0),
                ("REFRACT", 0),
                ("CUTOUT", 0),
                ("FOG", 0),
                ("GS_ENABLED", 0),
                ("TRAILRENDERER", i32::from(preset.trail().is_some() && !particle_legacy)),
            ]
            .iter()
            .map(|(n, v)| (n.to_string(), *v))
            .collect();
            // -1：粒子材质自己声明的 `passes[].combos`（官方 `REFRACT=1` 命中本机
            // `2080534540`/`2997301801`/`3571777169` 的粒子）。**并在这张硬编码表之后** ⇒
            // 材质里写了就赢，没写才用上面的默认值（= 审计给的口径"读材质的 combos，
            // 缺项再退回硬编码默认值"）。开关 `WE_MAT_COMBOS=1`，默认关。
            if mat_combos_on() {
                let n = merge_pass_combos(&mut combos, &pmat_combos, &[&f_raw, &v_raw]);
                if n > 0 && pdbg {
                    eprintln!("[材质combo] 粒子材质 {} 趟：{} 条 ⇒ {:?}", shader_name, n, combos);
                }
            }
            let (vs, fs) = shader::compose_stages_with(&v_raw, &f_raw, &combos);
            if let Ok(p) = std::env::var("WE_DUMP_SHADER") {
                let _ = std::fs::write(format!("{p}.particle.{}.frag", obj.id), &fs);
            }
            let tex = self.particle_texture(pkg, pkg_bytes, assets, &tex_name, cpu_fmt);
            let cols = (frames as f32).sqrt().ceil().max(1.0) as u32;
            let rows = (frames as f32 / cols as f32).ceil().max(1.0) as u32;
            let sheet = [
                1.0 / cols as f32,
                1.0 / rows as f32,
                frames as f32,
                tex.2 as f32 / tex.1.max(1) as f32,
            ];
            // 批的容量必须与**模拟用的上限**完全一致：模拟是 maxcount × count override，
            // 而这里以前只用了 maxcount ⇒ 萤火多长一颗就成了 66 > 60 顶点 ⇒ 画不下 ⇒ `Err`
            // ⇒ 内核把整个渲染器丢掉重建（每 ~22 秒一次，白花 200ms+ 还打断动画）。
            // 现在直接从模拟器取上限，杜绝两处公式漂移。
            let ov: particles::InstanceOverride = if std::env::var("WE_PARTICLE_LEGACY")
                .ok()
                .as_deref()
                == Some("1")
            {
                // A/B：复刻改前的严格解析（绑定对象 ⇒ 整条覆盖丢光）
                particles::parse_override_strict(obj.instance_override.as_ref())
            } else {
                obj.instance_override
                    .as_ref()
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default()
            };
            let mut origin = we_scene::scene::parse_vec3(&obj.origin).unwrap_or([0.0, 0.0, 0.0]);
            if flip_y {
                origin[1] = scene_h - origin[1];
            }
            // 发射点偏移要过的对象世界变换（含 angles；见下方 `WE_PARTICLE_EMITTER_XFORM`）
            let mut emit_m3: Option<[f32; 9]> = None;
            // 粒子的世界落点改用**图层那套父链语义**（`world_of`），并把
            // 那次 y 镜像**取消**。两个理由，都有实测数字：
            // ① 以前这里**完全忽略 `parent`** ⇒ Ice Princess 挂在 `хало`(id 83) 下的两个
            // `Falling debris particles small 1`（id 913/1918）被算到屏幕**左上角**
            // （2000x1125 上 bbox x41-117 y0-182），而 Windows 里那圈灰尘在月牙 x838-942 y92-176。
            // ② 粒子顶点在 GL 里的 y 约定**与图层相反**（实测两处独立标定）：
            // · 火把（9 个）的 `origin.y` 与吊灯图层的 `origin.y` 逐一相等（1831↔1833.5、
            // 1577.7↔1580.3、1595.8↔1575.4、1355.6↔1380.4），而图层落点 = `(2160-raw)/1.92`；
            // · 只渲火把测出的像素带 y309-1028 恰好等于 `raw/1.92` 那一档（208..1044），
            // 而不是图层的 171..754 ⇒ 喂**镜像过**的 y 会被再翻一次（= 全部粒子上下颠倒）。
            // 所以喂给粒子系统的必须是 **resolveTransform 之后的原始 y**（未镜像）。
            // ⇒ 步调与图层完全一致：`png_y = (2160 - raw_resolved)/1.92`。
            // `WE_PARTICLE_NO_PARENT=1` 回到旧行为（镜像 + 忽略 parent）做 A/B。
            if std::env::var("WE_PARTICLE_NO_PARENT").ok().as_deref() != Some("1") {
                let wm = world_of(obj, false);
                // 直接用 `world_of` 算出的**未镜像** resolve 原点。
                // 以前是"取 `m13` 再取消一次镜像"（`scene_h - y`）—— 两者在旧语义下逐位相同，
                // 但"镜像只做在链的根上"之后，容器子对象的 `m13` 已不再是那个被镜像的值，
                // 再取反就会翻回去（3704273480 的 `光束 2 (旧)`/`平整积雪` 都挂在容器 202 下）。
                // `WE_LANDING=legacy` 时仍走老的"取 m13 再取消镜像"（逐字节对齐旧二进制）。
                origin = if legacy_landing {
                    let mut o = [wm.m.m[12], wm.m.m[13], wm.m.m[14]];
                    if flip_y {
                        o[1] = scene_h - o[1];
                    }
                    o
                } else {
                    wm.raw_origin
                };
                // 对象自带 `angles`（如 913 的 z≈2.065rad≈118°）时，**发射点偏移**要不要
                // 跟着旋转 —— 参照实现分两派：`open-wallpaper-engine` 把粒子对象做成 `SceneNode(T·R·S)`，
                // 粒子局部坐标（含 emitter origin）整体过该节点变换（`ParticleRuntime.cpp:498` +
                // `SceneParticleObjectParser.cpp:358`）⇒ **旋转**；`linux-wallpaperengine` 只翻 y、不旋转
                // （`CParticle.cpp:373/527`）⇒ **不旋转**。
                // 本次**实测定案（两派都试过）**：Ice Princess 913 的 emitter origin=(0,200,0)、scale=1.86
                // ⇒ 偏移向量 (0,372)。不旋转时碎屑落 scene x[1787,1918]（2560 空间 x1191-1279，**在月牙窗
                // (1000,80,1300,260) 内**，与 Windows 1.jpg 碎屑区 scene x1680-1950 重叠）；按 world_of 的
                // 3×3 旋转后落 scene x[2059,2303]（2560 x1373-1535，**整块移出月牙窗**，比 Windows 右偏
                // ~180px）；反号旋转则左偏到 x~1257（同样出窗）。⇒ **Windows 这一帧选"不旋转"**，与 linux-WE
                // 一致（3 个 oracle 里 2 个支持）。所以默认**关**；要试 OWE 语义用 `WE_PARTICLE_EMITTER_XFORM=1`。
                // （球壳 emitter 的这个偏移恰好在球心，所以对 sphererandom 是"整体平移"，影响不小。）
                if std::env::var("WE_PARTICLE_EMITTER_XFORM").ok().as_deref() == Some("1") {
                    emit_m3 = Some([
                        wm.m.m[0], wm.m.m[4], wm.m.m[8], // 行 0：M[0][0],M[0][1],M[0][2]（列主序：M[r][c]=m[c*4+r]）
                        wm.m.m[1], wm.m.m[5], wm.m.m[9], // 行 1
                        wm.m.m[2], wm.m.m[6], wm.m.m[10], // 行 2
                    ]);
                }
            }
            let sc = we_scene::scene::parse_vec3(&obj.scale).unwrap_or([1.0, 1.0, 1.0]);
            // `spritetrail` 渲染器：把 `renderer.length/maxlength/minlength` 作为 WE 的
            // `g_RenderVar0` 下发（顶点 shader 用 `ComputeParticleTrailTangents` 把 quad 沿速度拉长）。
            // 普通 `sprite` → 全 0（那条分支不看它）。组合开关 TRAILRENDERER 已在上面按同一份
            // `preset.trail` 设好，两处不会漂移。
            // ⚠ 必须在 `preset` 被 move 进 `ParticleSystem::new` **之前**算。
            let rv0 = match preset.trail() {
                Some([len, maxlen, minlen]) if !particle_legacy => [len, maxlen, minlen, 0.0],
                _ => [0.0; 4],
            };
            // `spritetrail`：顶点 shader 用**速度 + g_RenderVar0** 算 up（不吃尺寸展开的那一套），
            // 这一趟保持旧口径（size 仍由 GPU 撑开）。
            let trail_on = preset.trail().is_some() && !particle_legacy;
            let mut sys = particles::ParticleSystem::new(preset, origin, [sc[0], sc[1]], &ov, frames);
            // 诊断 `WE_PARTICLE_GEOM=1`：把「配置推出的落点」需要的输入一次性打全
            // （对象 resolve 后的原点/缩放/角度、emitter 的轴向与距离区间、尺寸区间）。
            // 默认关 ⇒ 一行都不执行（逐字节不变）。
            if std::env::var("WE_PARTICLE_GEOM").is_ok()
                && std::env::var("WE_PARTICLE_GEOM_FILTER")
                    .map(|f| f.is_empty() || obj.name.contains(&f))
                    .unwrap_or(true)
            {
                let em0 = sys.emitter_debug();
                // 上传用的 MVP（`proj.transposed` 的列主序数组）也打出来，
                // 好在 Python 侧独立复算「配置推出的四角像素」。
                let pm = self.proj_mvp;
                eprintln!(
                    "[粒子配置] MVP上传={:?} 可见区={:?}",
                    pm.map(|v| (v * 1e6).round() / 1e6),
                    (self.gl.w, self.gl.h)
                );
                eprintln!(
                    "[粒子配置] 「{}」id={} origin_resolved=({:.2},{:.2},{:.2}) scale=({:.3},{:.3}) angles={:?} \
                     parent={:?} 位移乘子=({:.3},{:.3}) emitter={} directions={:?} 距离 {:?}..{:?} 尺寸 {}..{} 发射器初速 {}..{} \
                     emit_xform_on={} 半尺寸={}",
                    obj.name,
                    obj.id,
                    origin[0],
                    origin[1],
                    origin[2],
                    sc[0],
                    sc[1],
                    obj.angles,
                    obj.parent,
                    sc[0],
                    sc[1],
                    em0.0,
                    em0.1,
                    em0.2,
                    em0.3,
                    em0.4,
                    em0.5,
                    em0.6,
                    em0.7,
                    emit_m3.is_some(),
                    std::env::var("WE_PARTICLE_SIZE_LEGACY").ok().as_deref() != Some("1")
                );
                // 把「这层到底落在画布哪儿」直接算出来打一行 —— 0px 类
                // 缺陷（`2482408967` 的 Sakura 叶子层）第一判据就是发射中心：
                // `发射中心 = 对象 resolve 原点 + emitter.origin × 对象 scale`。
                // 口径与本函数上方的落点标定一致：喂给粒子系统的是**未镜像** raw y，
                // 最终 `png_y = (scene_h − raw_y)/k`、`png_x = raw_x/k`，`k = 画布宽 / 输出宽`。
                // `距离 × directions` 给的是**保守半跨**（`sphererandom` 的归一化方向分量
                // 上界 = `|dir[i]|`）：保守箱与画布不相交 ⇒ 该层**一定**一个像素都不落画布；
                // 相交则只说明"可能可见"，要实测。
                let eo = sys.emitter_origin_debug();
                let ow = self.gl.w as f32;
                let oh = self.gl.h as f32;
                let k = (scene_w / ow).max(1e-6);
                let ccx = origin[0] + eo[0] * sc[0];
                let ccy = origin[1] + eo[1] * sc[1];
                let hx = em0.3[0].abs() * em0.1[0].abs() * sc[0].abs();
                let hy = em0.3[1].abs() * em0.1[1].abs() * sc[1].abs();
                let (px, py) = (ccx / k, (scene_h - ccy) / k);
                let (hpx, hpy) = (hx / k, hy / k);
                let hits = (px + hpx >= 0.0) && (px - hpx <= ow) && (py + hpy >= 0.0) && (py - hpy <= oh);
                eprintln!(
                    "[粒子落点] 「{}」id={} 发射中心 canvas=({:.1},{:.1}) 输出px=({:.1},{:.1}) k={:.3} \
                     保守半跨=({:.1},{:.1})px ⇒ 生成箱 输出px x[{:.0},{:.0}] y[{:.0},{:.0}] {}",
                    obj.name,
                    obj.id,
                    ccx,
                    ccy,
                    px,
                    py,
                    k,
                    hpx,
                    hpy,
                    px - hpx,
                    px + hpx,
                    py - hpy,
                    py + hpy,
                    if hits {
                        "（与画布相交：可见与否需实测）"
                    } else {
                        "（保守箱整块在画布外 ⇒ 该层一定 0 像素）"
                    }
                );
            }
            // size 量纲：默认按参照实现把 sizerandom 结果 ÷2（`WE_PARTICLE_SIZE_LEGACY=1` 回旧行为）
            sys.set_half_size(std::env::var("WE_PARTICLE_SIZE_LEGACY").ok().as_deref() != Some("1"));
            // 精灵四边过**对象缩放**（见 `particles.rs::quad_scale` 的三方依据）。
            // `WE_PARTICLE_SIZE_SCALE=0` 回旧行为（size 原样，覆盖面积少 `sx·sy` 倍）；
            // `WE_PARTICLE_SIZE_SCALE=1x` 用缩放 (1,1) 做**自证**：应与旧行为逐像素一致。
            let size_scale_env = std::env::var("WE_PARTICLE_SIZE_SCALE").ok();
            if size_scale_env.as_deref() != Some("0") {
                let s2 = if size_scale_env.as_deref() == Some("1x") {
                    [1.0f32, 1.0f32]
                } else {
                    [sc[0], sc[1]]
                };
                if trail_on {
                    // `spritetrail`：四边的 right/up 来自**速度 + 眼位**
                    // （`particles.rs` 的 `trail_cfg` 分支），同样要过对象缩放。
                    sys.set_trail_scale(s2, tex.2 as f32 / tex.1.max(1) as f32);
                } else {
                    sys.set_quad_scale(s2, tex.2 as f32 / tex.1.max(1) as f32);
                }
            }
            if let Some(m3) = emit_m3 {
                sys.set_emit_xform(m3);
            }
            let maxc = sys.capacity().min(4096);
            let batch = match self.gl.make_batch(&vs, &fs, tex, maxc, sheet, rv0) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[场景] 粒子程序建不起来（{shader_name}）：{e}");
                    continue;
                }
            };
            println!(
                "  粒子「{}」shader={} 贴图={} 精灵帧={} 上限={} 加法混合={}",
                obj.name, shader_name, tex_name, frames, maxc, additive
            );
            // 场景属性脚本：`instanceoverride.rate` 写成 `{"script": "<JS>", "value": …}` 时
            // 把它接上（`WE_SCRIPT_AUDIO=1`；默认关 ⇒ `field_script` 恒 `None` ⇒ 与改动前一致）。
            // 本机命中：`2481886649` 的 Ember、`2997301801` 的 Ember（都是 WE 内置音频响应模板）。
            let rate_script =
                propscript::field_script(obj.instance_override.as_ref(), "rate", ov.rate);
            if rate_script.is_some() && std::env::var("WE_SCRIPT_DEBUG").is_ok() {
                eprintln!("[脚本] 「{}」挂了 instanceoverride.rate 脚本（init={:?}）", obj.name, ov.rate);
            }
            self.particle_systems.push(ParticleInstance {
                batch,
                sys,
                verts: Vec::new(),
                additive,
                name: obj.name.clone(),
                rate_script,
                // 按对象次序插入：`obj_boundary[p_idx]` = 排在它前面的对象产生的趟数。
                // 没有位置信息（不该发生）⇒ `usize::MAX` = 画在全部趟之后（旧行为）。
                insert_before: obj_boundary.get(p_idx).copied().unwrap_or(usize::MAX),
            });
        }
        // ── Glow：全屏"大半径暖色辉光"（`WE_GLOW=1`，**默认关**）──────────────
        // 见文件上方 `GlowCfg`：不是阈值泛光，而是"低阈值 + 暖色加权 + 大半径"的辉光，
        // 再（可选）在每个暖色粒子处补一朵硬光斑。**执行放在粒子之后**（读含粒子的整帧）。
        // `glow_pre` = **glow 在曝光之前**（默认）：叠回**场景 RT**（加法），
        // present（显示编码）推迟到"粒子 + glow 之后"再执行 ⇒ 与 WE 的 HDR→SDR 顺序一致。
        // 关掉它（`WE_GLOW_ORDER=post`，或没有场景 RT，或同时开了 bloom）⇒ 老行为：
        // glow 直接加法叠回**已编码的屏幕**。
        let bloom_on = BloomCfg::from_general(&scene.general).on;
        let glow_pre = self.glow.on && self.glow.order_pre && self.scene_rt.is_some() && !bloom_on;
        if self.glow.on {
            // 全屏通道（`Screen` = 旧行为 / `Local` = 默认点光源模型）；`Points` 只跑粒子光斑。
            if self.glow.source != GlowSource::Points {
            let div = self.glow.div.max(1.0);
            let (gw, gh) = (
                ((out_w as f32 / div) as u32).max(1),
                ((out_h as f32 / div) as u32).max(1),
            );
            let src = self.new_rt(out_w, out_h, "_rt_glow_src")?;
            let a = self.new_rt(gw, gh, "_rt_glow_ds")?;
            let b = self.new_rt(gw, gh, "_rt_glow_blurv")?;
            let c = self.new_rt(gw, gh, "_rt_glow_blurh")?;
            self.glow_src = Some(src);
            // `Local` 的额外缓冲：大尺度探测 v/h + 门输出（`g` = glow 的实际种子）。
            let local = if self.glow.source == GlowSource::Local {
                Some((
                    self.new_rt(gw, gh, "_rt_glow_probev")?,
                    self.new_rt(gw, gh, "_rt_glow_probeh")?,
                    self.new_rt(gw, gh, "_rt_glow_loc")?,
                ))
            } else {
                None
            };
            let mk = |gl: &gl_ctx::Gl, v: &str, f: &str| -> Result<gl_ctx::GlLayer, String> {
                let (cv, cf) = shader::compose_stages_with(v, f, &[]);
                gl.make_layer(&cv, &cf, None)
            };
            let l_ds = mk(&self.gl, BLOOM_DSQ_VERT, GLOW_DS_FRAG)?;
            let l_bv = mk(&self.gl, BLOOM_BLURV_VERT, BLOOM_BLUR_FRAG)?;
            let l_bh = mk(&self.gl, BLOOM_BLURH_VERT, BLOOM_BLUR_FRAG)?;
            let l_cb = mk(&self.gl, BLOOM_COMBINE_VERT, GLOW_COMBINE_FRAG)?;
            // `g_TexelSize` = 源纹素尺寸 × `WE_GLOW_SCALE`（blur 的步长写死 ×8 ⇒ 等效半径 ≈ 48×scale 像素）
            let texel = Value::V2(
                self.glow.scale.max(0.05) / out_w.max(1) as f32,
                self.glow.scale.max(0.05) / out_h.max(1) as f32,
            );
            let scal = |extra: Vec<(String, Value)>| {
                let mut s = vec![("g_TexelSize".to_string(), texel)];
                s.extend(extra);
                s
            };
            // ① 降采样 + 暖色加权 ⇒ 候选掩膜 `a`（ds 分辨率）
            self.glow_passes.push(Pass {
                layer: l_ds,
                target: Some(a),
                units: vec![(0, Src::Rt(src))],
                scalars: scal(vec![
                    ("g_GlowThreshold".into(), Value::F(self.glow.threshold)),
                    ("g_GlowWarm".into(), Value::F(self.glow.warmk)),
                ]),
                mvp: full_q,
                blend: gl_ctx::Blend::Opaque,
                mesh_anim: None,
            });
            // ② glow 的**种子**：`Screen` 直接是掩膜 `a`（旧行为）；`Local` 先过"局部性门"：
            // 大尺度探测 `ph` = 掩膜的软背景，门 = saturate((a − ph×k)/max(a,floor)×gain)
            // ⇒ 成片亮区（a ≈ ph）归零、小孤立光斑（a ≫ ph）保留。
            let seed = if let Some((pv, ph, g)) = local {
                // 大尺度半径 `loc_l` 像素 → `g_TexelSize`（blur 最大抽头 = 48 × g_TexelSize）
                let rr = self.glow.loc_l.max(8.0);
                let texel_l = Value::V2(
                    rr / (48.0 * out_w.max(1) as f32),
                    rr / (48.0 * out_h.max(1) as f32),
                );
                let scall = |extra: Vec<(String, Value)>| {
                    let mut s = vec![("g_TexelSize".to_string(), texel_l)];
                    s.extend(extra);
                    s
                };
                let l_pv = mk(&self.gl, BLOOM_BLURV_VERT, BLOOM_BLUR_FRAG)?;
                let l_ph = mk(&self.gl, BLOOM_BLURH_VERT, BLOOM_BLUR_FRAG)?;
                let l_loc = mk(&self.gl, BLOOM_COMBINE_VERT, GLOW_LOCALITY_FRAG)?;
                self.glow_passes.push(Pass {
                    layer: l_pv,
                    target: Some(pv),
                    units: vec![(0, Src::Rt(a))],
                    scalars: scall(vec![]),
                    mvp: full_q,
                    blend: gl_ctx::Blend::Opaque,
                    mesh_anim: None,
                });
                self.glow_passes.push(Pass {
                    layer: l_ph,
                    target: Some(ph),
                    units: vec![(0, Src::Rt(pv))],
                    scalars: scall(vec![]),
                    mvp: full_q,
                    blend: gl_ctx::Blend::Opaque,
                    mesh_anim: None,
                });
                self.glow_passes.push(Pass {
                    layer: l_loc,
                    target: Some(g),
                    units: vec![(0, Src::Rt(a)), (1, Src::Rt(ph))],
                    scalars: vec![
                        ("g_GlowLocK".into(), Value::F(self.glow.loc_k)),
                        ("g_GlowLocGain".into(), Value::F(self.glow.loc_gain)),
                        ("g_GlowLocFloor".into(), Value::F(self.glow.loc_floor)),
                        ("g_GlowLocBg".into(), Value::F(self.glow.loc_bg)),
                        ("g_GlowLocBg0".into(), Value::F(self.glow.loc_bg0)),
                    ],
                    mvp: full_q,
                    blend: gl_ctx::Blend::Opaque,
                    mesh_anim: None,
                });
                g
            } else {
                a
            };
            // ③ 大半径暖色模糊：seed → b → c（这就是光晕本体）
            self.glow_passes.push(Pass {
                layer: l_bv,
                target: Some(b),
                units: vec![(0, Src::Rt(seed))],
                scalars: scal(vec![]),
                mvp: full_q,
                blend: gl_ctx::Blend::Opaque,
                mesh_anim: None,
            });
            self.glow_passes.push(Pass {
                layer: l_bh,
                target: Some(c),
                units: vec![(0, Src::Rt(b))],
                scalars: scal(vec![]),
                mvp: full_q,
                blend: gl_ctx::Blend::Opaque,
                mesh_anim: None,
            });
            // 叠回去：`glow_pre` 时叠回**场景 RT**（加法），老路是叠回屏幕。
            // ⚠ 两种目标都用 `raw_position = true`（V 反向四边形）：新路的**源**是从
            // 场景 RT 拷的（场景 RT 相对"屏幕所见"是倒的）⇒ 必须同样翻一次才不回镜像。
            // 实测漏了这翻：光斑跑到画面**下部**（逐行 pre−post 在 y1000-1400 为正、
            // y200-400 为负）⇒ 吊灯窗占比没变化（亮 4.59% vs 无 glow 的 4.26%）。
            let mut cb = l_cb;
            cb.raw_position = true;
            self.glow_passes.push(Pass {
                layer: cb,
                // ⚠ 这趟**不是按图层构建的**（在别的块里，拿不到 `off_rt`）⇒ 不改道：
                // 实测改这里编译不过（E0425），且发光是场景级效果，按图层改道语义也不对。
                target: if glow_pre { self.scene_rt } else { None },
                units: vec![(0, Src::Rt(c))],
                scalars: vec![
                    ("g_GlowStrength".into(), Value::F(self.glow.strength)),
                    (
                        "g_GlowTint".into(),
                        Value::V3(self.glow.tint[0], self.glow.tint[1], self.glow.tint[2]),
                    ),
                ],
                mvp: full_q,
                blend: gl_ctx::Blend::Additive,
                mesh_anim: None,
            });
            println!(
                "  [glow] 光源={:?} {}x{}（1/{} 缓冲 {}x{}）阈值 {:.2} 暖色权重 {:.1} 强度 {:.2} 色调 {:.2}/{:.2}/{:.2} 局部性(L {:.0}px k {:.2} gain {:.2} floor {:.3})",
                self.glow.source,
                out_w,
                out_h,
                div as u32,
                gw,
                gh,
                self.glow.threshold,
                self.glow.warmk,
                self.glow.strength,
                self.glow.tint[0],
                self.glow.tint[1],
                self.glow.tint[2],
                self.glow.loc_l,
                self.glow.loc_k,
                self.glow.loc_gain,
                self.glow.loc_floor
            );
            }
            // 可选的"每个火把一个软光斑"
            if self.glow.particles {
                let n = 64u32;
                let rgba = glow_texture_rgba(n, self.glow.falloff);
                let tex = self.gl.make_texture(&rgba, n, n)?;
                let (vs, fs) = shader::compose_stages_with(GLOW_VERT, GLOW_FRAG, &[]);
                let bt = self.gl.make_batch(&vs, &fs, tex, 8192, [1.0, 1.0, 1.0, 1.0], [0.0; 4])?;
                self.glow_batch = Some(bt);
                println!(
                    "  [glow] 粒子光斑半径 {:.0} 场景单位 强度 {:.2} 衰减 {:.2}",
                    self.glow.radius, self.glow.pstrength, self.glow.falloff
                );
            }
        }
        // ── Bloom（`general.bloom`）───────────────────────────────────────────
        // **默认关**（`WE_BLOOM=1` 开）。为什么默认关：默认路径必须逐字节不变
        // （六张 scene_score 是护栏；泛光会把它们全部推偏），所以新行为一律走开关。
        // 打开时 present 那一趟不再是"原样贴回"，而是"画面 + 泛光"（官方 `combine`）。
        let mut bloom_rt: Option<usize> = None;
        if let Some(sr) = self.scene_rt {
            let cfg = BloomCfg::from_general(&scene.general);
            if cfg.on {
                bloom_rt = Some(self.push_bloom(&sr, out_w, out_h, &cfg, full_q)?);
            }
        }
        // ── 场景 RT → 屏幕（一次性 present）────────────────────────────────────
        // 位置在 passes 末尾：`draw_frame` 先跑完所有趟（含这一趟），**之后**才画粒子
        // ⇒ 粒子自然叠在壁纸上。**不要给这趟翻 V**（实测翻 V 全场景变差，见根因①的档2）。
        if let Some(sr) = self.scene_rt {
            match bloom_rt {
                Some(b) => {
                    let cb = self.make_combine_layer()?;
                    self.passes.push(Pass {
                        layer: cb,
                        target: None,
                        units: vec![(0, Src::Rt(sr)), (1, Src::Rt(b))],
                        scalars: vec![],
                        mvp: full_q,
                        blend: gl_ctx::Blend::Opaque,
                        mesh_anim: None,
                    });
                }
                None => {
                    let mut blit = self.make_present_layer()?;
                    // 采样 FBO 纹理贴到窗口 ⇒ 必须翻 V。默认帧缓冲与 FBO 的 Y 约定不同
                    // （参照实现自己的注释：`FBO content is not flipped like default framebuffer`）。
                    // 实测漏了这一翻，五条场景的分数**精确退回根因①修复之前**
                    // （武士 2.59→56.92、Sona 13.43→48.86、Fantasy 6.12→31.44）⇒ 就是这里。
                    blit.raw_position = true;
                    let pp = Pass {
                        layer: blit,
                        target: None,
                        units: vec![(0, Src::Rt(sr))],
                        scalars: vec![],
                        mvp: full_q,
                        blend: gl_ctx::Blend::Opaque,
                        mesh_anim: None,
                    };
                    // `glow_pre` ⇒ present 必须**推迟**到"粒子 + glow"之后（那时场景 RT 才是完整的、
                    // 且 glow 已按场景光叠进去），由 `draw_frame` 末尾执行。
                    if glow_pre {
                        self.pending_present = Some(pp);
                    } else {
                        self.passes.push(pp);
                    }
                }
            }
        }
        Ok(())
    }

    /// 精灵表的帧数（读 `.tex-json` 旁注；没有就 1）
    fn sprite_frames(assets: &Path, tex_name: &str) -> u32 {
        let p = assets
            .join("materials")
            .join(format!("{tex_name}.tex-json"));
        if let Ok(t) = std::fs::read_to_string(&p) {
            if let Ok(v) = we_scene::json::from_slice::<serde_json::Value>(t.as_bytes()) {
                if let Some(f) = v
                    .get("spritesheetsequences")
                    .and_then(|s| s.as_array())
                    .and_then(|a| a.first())
                    .and_then(|s| s.get("frames"))
                    .and_then(|f| f.as_u64())
                {
                    return f as u32;
                }
            }
        }
        1
    }

    /// `.tex-json` 旁注里的 `format` → WE 的 `FORMAT_*` 枚举（见 `assets/shaders/common_fragment.h`）。
    /// **这个值必须喂给 shader 的 `TEX0FORMAT` 组合开关**：WE 的粒子贴图常是"强度掩膜"
    /// （`rg88`：R=强度当 RGB、G=alpha），靠 `ConvertTexture0Format` 转换。
    /// 不喂 ⇒ 转换被跳过 ⇒ alpha 恒为 1 ⇒ **硬边实心色块**（用户问的"红块"就是这个：
    /// `Light shafts 0` 850~1000 单位的大四边形 × additive × 不透明 = 整屏红带）。
    fn tex_format(assets: &Path, tex_name: &str) -> i32 {
        let p = assets
            .join("materials")
            .join(format!("{tex_name}.tex-json"));
        if let Ok(t) = std::fs::read_to_string(&p) {
            if let Ok(v) = we_scene::json::from_slice::<serde_json::Value>(t.as_bytes()) {
                if let Some(f) = v.get("format").and_then(|f| f.as_str()) {
                    return match f.to_ascii_lowercase().as_str() {
                        "rgba8888" => 0,
                        "rgb888" => 1,
                        "rgb565" => 2,
                        "etc1_rgb8" => 3,
                        "dxt5" => 4,
                        "etc2_rgba8" => 5,
                        "dxt3" => 6,
                        "dxt1" => 7,
                        "rg88" => 8,
                        "r8" => 9,
                        "rg1616f" => 10,
                        "r16f" => 11,
                        "bc7" => 12,
                        _ => 0,
                    };
                }
            }
        }
        0
    }

    /// `.tex` 头部里的**通道格式码**（`TEXV0005\0TEXI0001\0` 之后第一个 i32）。
    /// 取值同 `we-scene/src/tex.rs` 的说明：`0=RGBA8888, 4=DXT5, 6=DXT3, 7=DXT1, 8=RG88, 9=`。
    fn we_tex_format_code(bytes: &[u8]) -> Option<i32> {
        if bytes.len() < 22 || &bytes[0..9] != b"TEXV0005\0" || &bytes[9..18] != b"TEXI0001\0" {
            return None;
        }
        Some(i32::from_le_bytes([bytes[18], bytes[19], bytes[20], bytes[21]]))
    }

    /// WE 的贴图**通道格式语义** —— 逐字复刻 `assets/shaders/common_fragment.h` 的
    /// `ConvertTexture0Format`（GL/非 HLSL 分支）：
///
    /// * `RG88`（format **8**）：`return _sample.rrrg` ⇒ **颜色 = R 通道（灰度），alpha = G 通道**；
///
    /// * `` （format **9**）：`return vec4(1,1,1,_sample.r)` ⇒ **颜色 = 白，alpha = R 通道**。
    /// 为什么必须在渲染器里做：`we-scene` 的解码把这两种都摊成 `(r,g,0,255)` / `(v,v,v,255)`
    /// **丢掉 alpha、颜色也错**。于是"带 alpha 的灰度精灵"被当成**不透明的红绿/灰度方块**。
    /// 实测（`3704273480` 的 `光束 2 (旧)`，贴图 `particle/light/light_shafts_6` = 128x512 **RG88**）：
    /// R 通道恒 **255**、G 通道 **0..194** 且剖面正是**光束的 alpha**
    /// （纵向中段峰值、横向双瓣 `…,93,94,58,59,109,173,186,…`）⇒ 正确渲染 = **白色柔光柱**
    /// （`WE_TEX_RGBA_FMT=0` 的对照 = 把整个四边形（放大后 **809x3065 px**）铺满的不透明平涂）。
    /// 默认**开**（`WE_TEX_RGBA_FMT=0` 是**回退开关**）。但**必须恰好应用一次**：
    /// 本函数是 shader 侧的**补位**（只有在 `TEX0FORMAT` 解析不出、shader 的
    /// `ConvertTexture0Format` 成为恒等映射时才走），判据在 [`SceneRenderer::particle_tex0_format`]。
    /// 历史上这条与 shader 侧（`TEX0FORMAT=8`）**同时生效** ⇒ 语义复合、alpha 塌成 1.0
    /// ⇒ 加性项 ×4.3 过曝。
    /// ⚠ **只用在「材质的 albedo（`g_Texture0`）」这条用法上**（本渲染器里 = 粒子贴图）。
    /// 理由（别照着推广到遮罩）：WE 的 `ConvertTexture0Format` 只出现在主 shader 对
    /// `g_Texture0` 的采样处；遮罩/法线走的是别的槽（`g_Texture1..`，由 `TEX1FORMAT`/`TEX4FORMAT` 管），
    /// 趟里读的是 **`.r`**，而两种解码的 `.r` **相同** ⇒ 对遮罩改它就是**引入回归**。
    /// （本机重新扫过：17 条壁纸包里头部为 RG88/ 的 .tex = 0 张；WE assets 里那 107 张
    /// 全在 `particle/**`、`gradient/**`、`util/**`，没有遮罩/法线 —— 收口依然必须做，
    /// 因为 `gradient/**` 那 4 张是当效果槽用的，套上就是回归。见 `particle_tex0_format`。）
    /// `enable` = 由调用方（[`SceneRenderer::particle_tex0_format`]）决定的**补位开关**：
    /// 只有 shader 这一趟的 `TEX0FORMAT` 是 0（`ConvertTexture0Format` = 恒等映射）时才为真。
    fn apply_channel_fmt_from_tex(
        bytes: &[u8],
        img: &mut image::RgbaImage,
        enable: bool,
    ) -> Option<i32> {
        if !enable {
            return None;
        }
        let f = Self::we_tex_format_code(bytes)?;
        match f {
            // RG88：颜色 = R 通道（灰度），alpha = G 通道
            8 => Self::apply_channel_fmt(img),
            // 颜色 = 白，alpha = R 通道
            9 => {
                for px in img.as_mut().chunks_exact_mut(4) {
                    let v = px[0];
                    px[0] = 255;
                    px[1] = 255;
                    px[2] = 255;
                    px[3] = v;
                }
            }
            _ => return None,
        }
        Some(f)
    }

    fn apply_channel_fmt(img: &mut image::RgbaImage) {
        let raw = img.as_mut();
        for px in raw.chunks_exact_mut(4) {
            let (r, g, _b, _a) = (px[0], px[1], px[2], px[3]);
            // 旧口径：RG88 解码 = (r, g, 0, 255)； 解码 = (v, v, v, 255)
            px[0] = r;
            px[1] = r;
            px[2] = r;
            px[3] = g;
        }
    }

    /// `.tex` → RGBA 图：裸像素/DXT **直接解**（按 TEXB 头部描述），图片容器交给图片解码器。
    /// 以前只走"扫内嵌图片"那条路 ⇒ DXT/ 的贴图整条壁纸都渲染不出来。
    fn tex_to_rgba(&self, name: &str, bytes: &[u8]) -> Option<image::RgbaImage> {
        let out = match we_scene::tex::decode(bytes) {
            Ok(we_scene::tex::Decoded::Rgba(p)) => {
                let mut img = image::RgbaImage::from_raw(p.width, p.height, p.rgba)?;
                // 与容器分支同一套翻转开关（两分支都要，否则"原始像素"的贴图测不到）
                if std::env::var_os("WE_TEX_FLIP").is_some() {
                    let skip = std::env::var("WE_TEX_FLIP_EXCEPT").unwrap_or_default();
                    let do_flip = skip.is_empty()
                        || !skip.split(',').any(|k| !k.trim().is_empty() && name.contains(k.trim()));
                    if do_flip {
                        image::imageops::flip_vertical_in_place(&mut img);
                    }
                }
                Some(img)
            }
            Ok(we_scene::tex::Decoded::Container(_payload, data)) => {
                let mut img = image::load_from_memory(&data).ok()?.to_rgba8();
                // `WE_TEX_FLIP=1` 全部翻转；`WE_TEX_FLIP_EXCEPT=甲,乙` 让名字含甲/乙的跳过翻转
                if std::env::var_os("WE_TEX_FLIP").is_some() {
                    let skip = std::env::var("WE_TEX_FLIP_EXCEPT").unwrap_or_default();
                    let skip = skip.is_empty()
                        || !skip.split(',').any(|k| !k.trim().is_empty() && name.contains(k.trim()));
                    if skip {
                        image::imageops::flip_vertical_in_place(&mut img);
                    }
                }
                Some(img)
            }
            Err(e) => {
                if std::env::var("WE_DEBUG_TEX").is_ok() {
                    eprintln!("[贴图] {name} 解不开：{e}");
                }
                None
            }
        };
        // ── 图集裁剪（两条解码分支共用）─────────────────────────────────────
        // `.tex` 头里有**两组**尺寸：第 1 组是纹理本身，第 2 组是"美术实际用到的区域"。
        // 有的作者把美术塞进更大的纹理里（本机 `1195626192`：纹理 8192x4096、美术 6000x3375）。
        // 不裁的话，四边形把 UV 0..1 铺满**整张纹理** ⇒ 美术只占一角、其余是空白 ⇒ 表现为
        // "只占一块、别处黑"（本机"画布有维度 > 4096 的 4 条壁纸全坏"就是这个）。
        // 裁成美术尺寸后，UV 0..1 正好覆盖美术 ⇒ 与作者预览一致。
        let out = out.map(|img| {
            let (w, h) = img.dimensions();
            if let Some((aw, ah)) = art_size(bytes) {
                if (aw < w || ah < h) && aw > 0 && ah > 0 {
                    let (x0, y0) = match std::env::var("WE_ART_ORIGIN").ok().as_deref() {
                        Some("bottom") => (0, h.saturating_sub(ah)), // 美术在图集左下角
                        _ => (0, 0),                                 // 默认左上角
                    };
                    if std::env::var_os("WE_DEBUG_UV").is_some() {
                        eprintln!(
                            "[贴图] 图集裁剪 {name}: 纹理 {w}x{h} → 美术 {aw}x{ah}（原点 {x0},{y0}）"
                        );
                    }
                    return cap_texture(
                        image::imageops::crop_imm(&img, x0, y0, aw, ah).to_image(),
                        self.tex_cap,
                        name,
                    );
                }
            }
            cap_texture(img, self.tex_cap, name)
        });
        // 调试旋钮：把**解码后的贴图**落盘（`WE_DUMP_TEX=<目录>`，前 256 张）。
        // "整张壁纸上下颠倒 / 带填充"这类问题，看一眼解码结果就能定性 —— 比在渲染管线里
        // 逐层猜快得多（`1931934283` 的倒置就是这么查的：头声明 8192x4096 而美术只有 7032x3840）。
        if let (Some(img), Some(dir)) = (out.as_ref(), std::env::var_os("WE_DUMP_TEX")) {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            if n < 256 {
                let safe: String = name
                    .chars()
                    .map(|c| if c == '/' || c == '\\' { '_' } else { c })
                    .collect();
                let _ = std::fs::create_dir_all(&dir);
                let p = std::path::Path::new(&dir).join(format!(
                    "tex{n:03}_{safe}_{}x{}.png",
                    img.width(),
                    img.height()
                ));
                let _ = img.save(&p);
                eprintln!("[贴图 dump] {name} → {}", p.display());
            }
        }
        out
    }

    /// **粒子 albedo 这一趟该喂给 shader 的 `TEX0FORMAT`**（`g_Texture0` 的通道格式码）。
    /// 只负责**解析**，不读开关（开关在调用点，见下）。
///
    /// 1. 贴图在包里（`materials/<name>.tex`）或 WE assets 里 ⇒ 读 **`.tex` 头部的 format 码**
    /// （`we_tex_format_code`，真值），只接受本实现覆盖的 `8=RG88 / 9=`。
    /// 头里的 0/4/7/12（RGBA/DXT/BC）在 `ConvertTexture0Format` 里都是**恒等分支**，
    /// 喂不喂结果一样 ⇒ 不靠它改变行为（别让旁注把 DXT 说成 rg88）。
    /// ⚠ 这条比"只看 assets 旁注"更宽：**包内**的 RG88/ 粒子 albedo 以前拿不到旁注
    /// ⇒ `TEX0FORMAT` 恒 0 ⇒ 静默画成硬边实心块。本机实测（每一条都重新扫过）：
    /// · 17 条壁纸的包里，`materials/**.tex` 头部为 RG88/ 的 = **0 张**
    /// · WE assets 里头部为 RG88/ 的 107 张 .tex 全在 `particle/**`(102)
    /// `gradient/**`(4)、`util/**`(1)，**没有一张是遮罩/法线**；
    /// · 17 条里被当"粒子 albedo"用到的那 96 条，头部 format 与 `.tex-json` 旁注
    /// **不一致 = 0 处**。
    /// ⇒ 新的"按头部取格式"对 17 条给出的 TEX0FORMAT 与旧路径**逐个相同** ⇒ **逐字节无变化**
    /// 。
    /// 唯一一处头/旁注分歧：`particle/smoke/smoke2normal.tex` 头=8、旁注=`rg88n`
    /// （`rg88n` = 法线专用的格式名，见 assets/shaders/declarations.json；头里仍是 8）。
    /// 它只作 `g_Texture1` 法线用、**不走本函数**，且 17 条里 0 处引用 ⇒ 无影响。
///
    /// 2. assets 旁边有同名 PNG ⇒ 上传的是 PNG（已是 RGBA8888），没有通道语义 ⇒ 0。
///
    /// 3. 头拿不到 ⇒ 退回旧路径：WE assets 旁的 `.tex-json` 旁注（`tex_format`）。
    /// ⚠ **只用在「粒子 albedo（= 材质 `g_Texture0`）」这条路上**（`particle_texture` 的调用点）。
    /// 遮罩/法线走别的槽（`g_Texture1..`，shader 里由 `TEX1FORMAT`/`TEX4FORMAT` 管，我们不喂），
    /// 趟里读的是 `.r`，两种解码的 `.r` 相同 ⇒ 对它们套这条语义就是纯回归。
    fn particle_tex0_format(
        pkg: &we_scene::pkg::Pkg,
        pkg_bytes: &[u8],
        assets: &Path,
        name: &str,
    ) -> i32 {
        // assets 旁边有同名 PNG ⇒ 上传的是 PNG（已是 RGBA8888），没有通道格式语义。
        if assets
            .join("materials")
            .join(format!("{name}.png"))
            .is_file()
        {
            return 0;
        }
        // 真值：`.tex` 头部 format 码（包内优先，顺序与 `particle_texture` 一致）
        let pkg_entry = pkg
            .find(&format!("materials/{name}.tex"))
            .or_else(|| pkg.find_suffix(&format!("{name}.tex")));
        let hdr = match pkg_entry {
            Some(e) => pkg.slice(pkg_bytes, e).and_then(Self::we_tex_format_code),
            None => std::fs::read(assets.join("materials").join(format!("{name}.tex")))
                .ok()
                .and_then(|b| Self::we_tex_format_code(&b)),
        };
        if let Some(f) = hdr {
            // 8/9 是本实现覆盖的语义；其余（0/4/7/12…）在 shader 里是恒等 ⇒ 一律 0
            return if f == 8 || f == 9 { f } else { 0 };
        }
        // 旧路径：assets 的 `.tex-json` 旁注
        Self::tex_format(assets, name)
    }

    /// 粒子贴图：assets PNG → 包内 .tex → assets .tex → 兜底生成一张柔光点
    /// `cpu_fmt` = 是否在 **CPU 侧**补 `ConvertTexture0Format` 的通道语义
    /// （由 [`SceneRenderer::particle_tex0_format`] 决定：只有 shader 的 `TEX0FORMAT` 是 0
    /// 即 `ConvertTexture0Format` 为恒等映射 —— 才补，从而保证**恰好一次**）。
    fn particle_texture(
        &mut self,
        pkg: &we_scene::pkg::Pkg,
        pkg_bytes: &[u8],
        assets: &Path,
        name: &str,
        cpu_fmt: bool,
    ) -> (glow::Texture, u32, u32) {
        if !name.is_empty() {
            if let Some(t) = self.load_resource(assets, name, cpu_fmt) {
                if let Src::Tex(i) = t {
                    return self.texs[i];
                }
            }
            if let Some(i) = self.upload_pkg_tex(pkg, pkg_bytes, assets, name) {
                return self.texs[i];
            }
            // assets 里的 .tex（内置粒子贴图大多是这种；解不开就往下走）
            let tex_path = assets.join("materials").join(format!("{name}.tex"));
            if let Ok(bytes) = std::fs::read(&tex_path) {
                if let Some(rgba) = self.tex_to_rgba(name, &bytes) {
                    let mut rgba = rgba;
                    // 粒子 albedo = 材质的 `g_Texture0`：由 CPU **补位**按 WE
                    // `ConvertTexture0Format` 改写通道语义（RG88/）。
                    // ⚠ 只有 shader 这趟不做转换（`cpu_fmt`）才补 —— 见 `particle_tex0_format`。
                    if let Some(f) = Self::apply_channel_fmt_from_tex(&bytes, &mut rgba, cpu_fmt) {
                        if std::env::var("WE_DEBUG_PARTICLES").is_ok()
                            || std::env::var("WE_DEBUG_TEX").is_ok()
                        {
                            eprintln!(
                                "[粒子贴图] {name} 通道格式 {f} ⇒ CPU 侧按 WE ConvertTexture0Format 改写"
                            );
                        }
                    }
                    if std::env::var("WE_DEBUG_PARTICLES").is_ok() {
                        let raw = rgba.as_raw();
                        let n = (raw.len() / 4) as f32;
                        let ma: f32 = raw.chunks_exact(4).map(|p| p[3] as f32).sum::<f32>() / n;
                        let mr: f32 = raw.chunks_exact(4).map(|p| p[0] as f32).sum::<f32>() / n;
                        eprintln!(
                            "[粒子贴图] {name} {}x{} 平均 alpha={ma:.1} 平均 r={mr:.1}",
                            rgba.width(),
                            rgba.height()
                        );
                    }
                    if let Ok(t) = self.gl.make_texture(rgba.as_raw(), rgba.width(), rgba.height()) {
                        self.texs.push(t);
                        return t;
                    }
                }
            }
        }
        // 兜底：64x64 的柔光点（加法混合下足够像样的雪花/萤火）
        let (px, w, h) = soft_dot();
        match self.gl.make_texture(&px, w, h) {
            Ok(t) => {
                self.texs.push(t);
                t
            }
            Err(_) => self.white,
        }
    }
    fn resolve_src(
        &mut self,
        name: &str,
        prev: Src,
        assets: &Path,
        pkg: &we_scene::pkg::Pkg,
        pkg_bytes: &[u8],
    ) -> Option<Src> {
        if name == "_rt_imageLayerComposite" || name == "previous" {
            // ⚠ **`previous` = 本特效开始时的画面**（effect.json 的 `bind` 常用它要"原图"），
            // 不是"上一趟输出"。`shine_combine` 的 slot1 就是 `previous`，shader 里叫 `albedo`
            // ⇒ 喂成链的当前输出 = "把光晕和光晕自己合成" ⇒ **整层变黑**（实测 `3504393434`）。
            return Some(self.cur_effect_input);
        }
        // ── 图层链接 vs **前向引用** ──────────────────────────────────────
        // `_rt_imageLayerComposite_<id>_<a|b>` 指向的层如果**排在当前对象后面**，这一刻它还没画，
        // 缓冲是空的 ⇒ 拿"空黑"去合成会把画面压成平均值（实测 `2482408967` 的 shine_combine
        // 引用后面那层 Clock ⇒ 武士画像被压成一片平色，标准差 0）。
        // WE 用 a/b 双缓冲读"上一帧"，我们退一步：**读不到就退回本链输入**（= 这一层自己的原图），
        // 语义上正是"把光晕叠回原图"这种用法的本意。
        if let Some(rest) = name.strip_prefix("_rt_imageLayerComposite_") {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(id) = digits.parse::<i64>() {
                // **自引用** = 本层**运行中**的合成结果（官方把该 slot 重写成
                // `effect_composite`，见 [`link_self_run_on`]）。
                // ⚠ 取的是 `cur_effect_input`（本特效开始时的那张），不是 `prev`：
                // `prev` 在本特效**内部**会逐趟前移（godrays 有 5 趟），第 4 趟的 `prev`
                // 是"模糊后的光柱"而不是本层合成结果 ⇒ 实测 Sona 整幅变暗
                // （mean 171/204/220 → 159/182/196）。官方的 `inRT = effect_composite`
                // 在 `for (effects)` 循环里只取一次（`SceneImageObjectParser.cpp:698`）
                // ⇒ 同一个特效的所有趟读到同一张。
                if link_self_run_on() && self.obj_order.get(&id) == Some(&self.cur_obj_idx) {
                    if std::env::var_os("WE_DEBUG_LINKS").is_some() {
                        eprintln!("[链接] 自引用 {name} ⇒ 本特效开始时的链结果（官方 effect_composite 口径）");
                    }
                    return Some(self.cur_effect_input);
                }
                if let Some(o) = self.obj_order.get(&id) {
                    if *o > self.cur_obj_idx {
                        if std::env::var_os("WE_DEBUG_LINKS").is_some() {
                            eprintln!(
                                "[链接] 前向引用 {name}（第 {o} 层 > 当前第 {} 层）⇒ 退回本链输入",
                                self.cur_obj_idx
                            );
                        }
                        return Some(prev);
                    }
                }
            }
        }
        if let Some(i) = self.rt_index.get(name) {
            return Some(Src::Rt(*i));
        }
        // `X_b` / `X_a` 当作 `X`（WE 的 a/b 双缓冲；这里只读，映射到同一个即可）
        for suf in ["_b", "_a"] {
            if let Some(base) = name.strip_suffix(suf) {
                if let Some(i) = self.rt_index.get(base) {
                    return Some(Src::Rt(*i));
                }
            }
        }
        if name.starts_with("_rt_") {
            // 引擎内部 RT：名字对不上就用"链上这一趟的输入"（最接近当前画面）
            return Some(prev);
        }
        // ⚠ **先搜壁纸自己的 pkg**（`materials/<名字>.tex`）：以前这里只搜 WE 自带 assets 的 PNG，
        // 于是壁纸包里的**遮罩 / 深度图 / 特效纹理全被悄悄换成"白"兜底** —— 症状是
        // `effects/tint` 的遮罩采样出 1.0 ⇒ 整层被染黑（实测人物层亮度 52 → 0）。
        if let Some(i) = self.upload_pkg_tex(pkg, pkg_bytes, assets, name) {
            return Some(Src::Tex(i));
        }
        Some(Src::White)
    }

    /// 内置资源：先找 PNG（WE 在旁边放了同名 PNG，省得解它的 DXT），再找 WE 自带的 `.tex`。
    /// **只找 PNG 是不够的** —— WE 的 assets 里绝大多数素材只有 `.tex`
    /// （例如 `materials/particle/halo_6.tex`，正是崩坏3 `2564205376` 的 X-Ray 用来做
    /// "透视区"的那张精灵）。找不到就退回 1x1 白 ⇒ 透视区铺满整层（不再"只在鼠标附近"）
    /// ⇒ 效果形状完全错（而且不报错）。所以这里给 `.tex` 也开一条路（走同一个解码器）。
    /// `cpu_fmt` = 这一趟是不是**由 CPU 补** `ConvertTexture0Format` 的通道语义
    /// （只有粒子 albedo 且 shader 侧不转换时才真；判据见 [`SceneRenderer::particle_tex0_format`]）。
    /// PNG 分支不受影响（PNG 已是 RGBA8888）。
    fn load_resource(&mut self, assets: &Path, name: &str, cpu_fmt: bool) -> Option<Src> {
        let png = assets.join("materials").join(format!("{name}.png"));
        if let Ok(bytes) = std::fs::read(&png) {
            if let Ok(img) = image::load_from_memory(&bytes) {
                let mut rgba = cap_texture(img.to_rgba8(), self.tex_cap, name);
                // 链趟里的打包贴图按数据规则定 V 方向（见 `FxTexVFlip`；RT 槽不走这里）。
                if self.fx_slot_flip {
                    image::imageops::flip_vertical_in_place(&mut rgba);
                }
                if let Ok(t) = self
                    .gl
                    .make_texture(rgba.as_raw(), rgba.width(), rgba.height())
                {
                    self.texs.push(t);
                    return Some(Src::Tex(self.texs.len() - 1));
                }
            }
        }
        // WE 自带 assets 里的 `.tex`（与"包内素材"同一套解码 + 同一套 V 规则）
        let tex = assets.join("materials").join(format!("{name}.tex"));
        if let Ok(bytes) = std::fs::read(&tex) {
            if let Some(mut rgba) = self.tex_to_rgba(name, &bytes) {
                // 只有"粒子 albedo 且 shader 侧不转换"才在 CPU 侧按 WE `ConvertTexture0Format`
                // 补通道语义（`cpu_fmt`；遮罩/其它槽传 false）—— 见 `apply_channel_fmt_from_tex`
                // 与 `particle_tex0_format`（**恰好一次**不变量）。
                if cpu_fmt {
                    let _ = Self::apply_channel_fmt_from_tex(&bytes, &mut rgba, true);
                }
                // 诊断：把**真正上传的**粒子 albedo 落盘（`WE_PARTICLE_TEXDUMP=<目录>`），
                // 另存一张 `_asshader` = **shader 采样后**（= 加性项真正乘的那个 `color`）——
                // 因为转换现在由 shader 做，上传的那张不再等于"肉眼验收看到的样子"。
                if let Some(d) = std::env::var_os("WE_PARTICLE_TEXDUMP") {
                    let _ = std::fs::create_dir_all(&d);
                    let safe: String = name
                        .chars()
                        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
                        .collect();
                    let base = std::path::Path::new(&d);
                    let p = base.join(format!("{safe}_{}x{}.png", rgba.width(), rgba.height()));
                    let _ = rgba.save(&p);
                    eprintln!("[粒子贴图 dump] {name} 上传版 → {}", p.display());
                    let mut seen = rgba.clone();
                    if !cpu_fmt {
                        let _ = Self::apply_channel_fmt_from_tex(&bytes, &mut seen, true);
                    }
                    let q = base.join(format!(
                        "{safe}_{}x{}_asshader.png",
                        seen.width(),
                        seen.height()
                    ));
                    let _ = seen.save(&q);
                    eprintln!("[粒子贴图 dump] {name} shader 采样后 → {}", q.display());
                }
                if self.fx_slot_flip {
                    image::imageops::flip_vertical_in_place(&mut rgba);
                }
                if let Ok(t) = self
                    .gl
                    .make_texture(rgba.as_raw(), rgba.width(), rgba.height())
                {
                    self.texs.push(t);
                    return Some(Src::Tex(self.texs.len() - 1));
                }
            } else if std::env::var_os("WE_DEBUG_TEX").is_some() {
                eprintln!("[贴图] assets 里的 {name}.tex 解不开");
            }
        }
        None
    }

    /// `workshop/<id>/<rest>` 形式的**跨工坊件引用**：去那个工坊作品的包里取素材。
    /// WE 的作者可以直接引用**另一个工坊作品**里的贴图（本机实例：`3704273480` 的
    /// `workshop/3661184966/footagecrate-birds-...`）。以前我们只在本壁纸包里找 ⇒
    /// 静默失败、那个对象整个消失。这里按 Steam 的标准工坊布局去那个作品里找：
    /// 先找它包内的 `materials/<rest>.tex`，再试散文件。
    fn cross_item_texture(&mut self, name: &str) -> Option<usize> {
        let rest = name.strip_prefix("workshop/")?;
        let (id, sub) = rest.split_once('/')?;
        let home = std::env::var("HOME").ok()?;
        let dir = std::path::PathBuf::from(home)
            .join(".local/share/Steam/steamapps/workshop/content/431960")
            .join(id);
        // 与包内贴图走**同一条** V 规则（`fx_slot_flip`）：跨件素材也是“打包贴图”，
        // 不能因为走了另一条解析路径就漏掉方向处理。
        let maybe_flip = |rgba: &mut image::RgbaImage| {
            if self.fx_slot_flip {
                image::imageops::flip_vertical_in_place(rgba);
            }
        };
        // ① 对方包内
        if let Ok(bytes) = std::fs::read(dir.join("scene.pkg")) {
            if let Ok(op) = we_scene::pkg::Pkg::parse(&bytes) {
                let e = op
                    .find(&format!("materials/{sub}.tex"))
                    .or_else(|| op.find_suffix(&format!("{sub}.tex")));
                if let Some(e) = e {
                    if let Some(b) = op.slice(&bytes, e) {
                        if let Some(mut rgba) = self.tex_to_rgba(sub, b) {
                            maybe_flip(&mut rgba);
                            if let Ok(t) = self
                                .gl
                                .make_texture(rgba.as_raw(), rgba.width(), rgba.height())
                            {
                                self.texs.push(t);
                                return Some(self.texs.len() - 1);
                            }
                        }
                    }
                }
            }
        }
        // ② 散文件（视频/图片类作品，或解包目录）
        for cand in [
            dir.join(format!("materials/{sub}.tex")),
            dir.join(sub),
            dir.join(format!("{sub}.tex")),
        ] {
            if let Ok(b) = std::fs::read(&cand) {
                if let Some(mut rgba) = self.tex_to_rgba(sub, &b) {
                    maybe_flip(&mut rgba);
                    if let Ok(t) = self
                        .gl
                        .make_texture(rgba.as_raw(), rgba.width(), rgba.height())
                    {
                        self.texs.push(t);
                        return Some(self.texs.len() - 1);
                    }
                }
            }
        }
        eprintln!("[跨件] ✗ {name}：工坊作品 {id} 里找不到（可能未订阅该依赖）");
        None
    }

    fn upload_pkg_tex(
        &mut self,
        pkg: &we_scene::pkg::Pkg,
        pkg_bytes: &[u8],
        assets: &Path,
        name: &str,
    ) -> Option<usize> {
        // 跨工坊件引用优先（本包里一定没有）
        if name.starts_with("workshop/") {
            if let Some(i) = self.cross_item_texture(name) {
                return Some(i);
            }
        }
        let entry = pkg
            .find(&format!("materials/{name}.tex"))
            .or_else(|| pkg.find_suffix(&format!("{name}.tex")));
        if let Some(e) = entry {
            if let Some(bytes) = pkg.slice(pkg_bytes, e) {
                if let Some(mut rgba) = self.tex_to_rgba(name, bytes) {
                    // 链趟里的打包贴图按数据规则定 V 方向（见 `FxTexVFlip`；RT 槽不走这里）。
                    if self.fx_slot_flip {
                        image::imageops::flip_vertical_in_place(&mut rgba);
                        if std::env::var_os("WE_DEBUG_FXFLIP").is_some() {
                            eprintln!("[翻V] {name} （链趟打包贴图，按 copybackground 规则）");
                        }
                    }
                    // 采样状态（wrap）是**数据声明的**：WE 的 `flags.bit1 = clampUVs`
                    // （`TexImageParser.cpp:161`：`clampUVs ? CLAMP_TO_EDGE : REPEAT`）。
                    // 我们以前恒定 CLAMP ⇒ 没置该位的贴图（本包的 `effects/waterflowphase`）
                    // 在 WE 里是 REPEAT，而 `waterflow.frag` 直接拿 `v_TexCoord.xy * g_FlowPhaseScale`
                    // 采样、**没有 `fract`** ⇒ wrap 改变云层流动相位。
                    // **默认按数据走**；`WE_TEX_FLAGS=0` 显式回退
                    // 旧行为（恒定 CLAMP_TO_EDGE）⇒ 能逐字节对照改动前后。
                    let repeat =
                        wrap_repeat(tex_flags_enabled(), we_scene::tex::repeat_wrap(bytes));
                    if repeat && std::env::var_os("WE_DEBUG_TEX").is_some() {
                        eprintln!("[贴图wrap] {name} ⇒ REPEAT（.tex flags 未置 clampUVs）");
                    }
                    if let Ok(t) = self.gl.make_texture_wrapped(
                        rgba.as_raw(),
                        rgba.width(),
                        rgba.height(),
                        repeat,
                    ) {
                        self.texs.push(t);
                        return Some(self.texs.len() - 1);
                    }
                }
            }
        }
        if let Some(Src::Tex(i)) = self.load_resource(assets, name, false) {
            return Some(i);
        }
        None
    }

    /// 把一张 RT 同时登记成 WE 的图层链接名字（`_a` / `_b` / 不带后缀三种写法都要查得到）
    fn alias_rt(&mut self, layer_id: i64, rt: usize) {
        self.rt_index
            .insert(format!("_rt_imageLayerComposite_{layer_id}_a"), rt);
        self.rt_index
            .insert(format!("_rt_imageLayerComposite_{layer_id}_b"), rt);
        self.rt_index
            .entry(format!("_rt_imageLayerComposite_{layer_id}"))
            .or_insert(rt);
    }

    /// shape 对象：把"直接绘制四边形"变成一张底图 layer。
    /// 参考实现 OWE `ParseShapeObj`：shape 会被改写成 ImageObject ——
///
    /// * 四边形四角来自**首个特效的首趟材质**里的 `point0..point3`（归一化坐标），
    /// 顶点 = `((px-0.5), (0.5-py)) * edge`，UV 直接用 `(px, py)`，edge = **场景高**；
///
    /// * 底图材质取**最后一个特效**的材质、**强制加法混合**；
///
    /// * 对象尺寸 = `(edge, edge)`。
    /// 我们这边顶点要换算成"单位四边形坐标"（因为 MVP 按 size/2 缩放）：
    /// x = 2px-1、y = 1-2py —— 与 `(px-0.5)*2 / (0.5-py)*2` 等价。
    fn build_shape_layer(
        &mut self,
        obj: &we_scene::scene::Object,
        pkg: &we_scene::pkg::Pkg,
        pkg_bytes: &[u8],
        assets: &std::path::Path,
        scene_h: f32,
    ) -> Result<(Src, gl_ctx::GlLayer, Vec<(String, Value)>), String> {
        if obj.shape.as_deref() != Some("quad") {
            return Err(format!(
                "只支持 quad（WE 自己也不支持 {:?}）",
                obj.shape.as_deref().unwrap_or("?")
            ));
        }
        let (Some(first), Some(last)) = (obj.effects.first(), obj.effects.last()) else {
            return Err("shape 没有特效（材质来自特效链）".into());
        };

        // ① 首个特效的首趟：从材质 + 趟覆盖里取四角点与常量
        let ef_bytes = pkg
            .get(pkg_bytes, &first.file)
            .ok_or_else(|| format!("包里没有 {}", first.file))?;
        let ef = we_scene::json::from_slice::<shader::EffectFile>(ef_bytes)
            .map_err(|e| format!("{} 解析失败：{e}", first.file))?;
        let ep = ef
            .passes
            .first()
            .ok_or_else(|| "首个特效没有趟".to_string())?;
        let ov = first.passes.first();
        let mat_bytes = pkg
            .get(pkg_bytes, &ep.material)
            .ok_or_else(|| format!("包里没有 {}", ep.material))?;
        let mat = we_scene::json::from_slice::<shader::MaterialFile>(mat_bytes)
            .map_err(|e| format!("{} 解析失败：{e}", ep.material))?;
        let _mp = mat
            .passes
            .first()
            .ok_or_else(|| "shape 材质没有趟".to_string())?;

        // 常量：先按**场景里的参数名**收下来（`point0..3` 要用它找），
        // 再用 shader 注释里的映射翻成 **uniform 名**（`rayradius → g_Radius`）——
        // 不翻的话常量等于没下发（实测：shape 图层建出来了但**画面差为 0**）。
        let mut raw_consts: Vec<(String, Value)> = Vec::new();
        if let Some(o) = ov {
            for (k, v) in &o.constant_shader_values {
                if let Some(val) = json_to_value(v) {
                    raw_consts.push((k.clone(), val));
                }
            }
        }
        let get_point = |name: &str| -> Result<[f32; 2], String> {
            raw_consts
                .iter()
                .find(|(k, _)| k == name)
                .and_then(|(_, v)| match v {
                    Value::V2(a, b) => Some([*a, *b]),
                    _ => None,
                })
                .ok_or_else(|| format!("缺少四角点 {name}（shape 靠它定位）"))
        };
        let (p0, p1, p2, p3) = (
            get_point("point0")?,
            get_point("point1")?,
            get_point("point2")?,
            get_point("point3")?,
        );
        // ② 单位四边形坐标 + UV 直接用归一化点（与 OWE `GenDirectDrawQuadMesh` 的 tex_coords 一致）
        let corner = |p: [f32; 2]| [p[0] * 2.0 - 1.0, 1.0 - p[1] * 2.0, p[0], p[1]];
        let quads = [corner(p0), corner(p1), corner(p2), corner(p3)];

        // ③ 底图材质 = 最后一个特效的（+ 它的趟覆盖），组合也取它的
        let le_bytes = pkg
            .get(pkg_bytes, &last.file)
            .ok_or_else(|| format!("包里没有 {}", last.file))?;
        let lef = we_scene::json::from_slice::<shader::EffectFile>(le_bytes)
            .map_err(|e| format!("{} 解析失败：{e}", last.file))?;
        let lep = lef
            .passes
            .last()
            .ok_or_else(|| "最后一个特效没有趟".to_string())?;
        let lov = last.passes.last();
        let lmat_bytes = pkg
            .get(pkg_bytes, &lep.material)
            .ok_or_else(|| format!("包里没有 {}", lep.material))?;
        let lmat = we_scene::json::from_slice::<shader::MaterialFile>(lmat_bytes)
            .map_err(|e| format!("{} 解析失败：{e}", lep.material))?;
        let lmp = lmat
            .passes
            .last()
            .ok_or_else(|| "shape 末材质没有趟".to_string())?;

        let mut combos: Vec<(String, i32)> = Vec::new();
        if let Some(o) = lov {
            for (k, v) in &o.combos {
                combos.push((k.clone(), *v as i32));
            }
        }
        let v_raw = shader::load_shader(pkg, pkg_bytes, assets, &lmp.shader, "vert")?;
        let f_raw = shader::load_shader(pkg, pkg_bytes, assets, &lmp.shader, "frag")?;
        // -1：shape 末材质**自己声明**的 `passes[].combos`（同一通道；开关默认关）。
        // 放在场景级 combo 之后 ⇒ 场景写了的仍然赢（与官方"编辑器值最高"一致）。
        if mat_combos_on() {
            merge_pass_combos(&mut combos, &lmp.combos, &[&f_raw, &v_raw]);
        }
        // 顶点探针（`WE_SHAPE_VPROBE=<模式>`）：把 `v_TexCoordFx = mul(vec3(uv,1), xform)`
        // 这一行换掉，用来判定"坏在 **上一步的 xform（squareToQuad∘inverse）** 还是坏在
        // 后面的片元数学"。配合片元探针 `WE_SHAPE_DEBUG=rawv`（直接画 v_TexCoordFx）读数。
        // id = 跳过 xform（v = (uv,1)） rows = 取 xform 对角线
        // col2= 取 xform 第 2 列 raw = 原样（对照）
        let v_raw = match std::env::var("WE_SHAPE_VPROBE").ok().as_deref() {
            Some("id") => v_raw.replace(
                "v_TexCoordFx = mul(vec3(a_TexCoord.xy, 1.0), xform);",
                "v_TexCoordFx = vec3(a_TexCoord.xy, 1.0);",
            ),
            Some("rows") => v_raw.replace(
                "v_TexCoordFx = mul(vec3(a_TexCoord.xy, 1.0), xform);",
                "v_TexCoordFx = vec3(xform[0][0], xform[1][1], xform[2][2]);",
            ),
            Some("col2") => v_raw.replace(
                "v_TexCoordFx = mul(vec3(a_TexCoord.xy, 1.0), xform);",
                "v_TexCoordFx = xform[2];",
            ),
            Some("row0") => v_raw.replace(
                "v_TexCoordFx = mul(vec3(a_TexCoord.xy, 1.0), xform);",
                "v_TexCoordFx = vec3(xform[0][0], xform[0][1], xform[0][2]);",
            ),
            Some("c0") => v_raw.replace(
                "v_TexCoordFx = mul(vec3(a_TexCoord.xy, 1.0), xform);",
                "v_TexCoordFx = xform[0];",
            ),
            Some("c1") => v_raw.replace(
                "v_TexCoordFx = mul(vec3(a_TexCoord.xy, 1.0), xform);",
                "v_TexCoordFx = xform[1];",
            ),
            // 语义校准：`vec3 * mat3`（= 我们 `mul` 宏展开的结果）到底是 **vᵀ·M** 还是 **M·v**？
            // 取 M = mat3(1,2,3, 4,5,6, 7,8,9)（列主序 ⇒ 数学上行 0=(1,2,3), 行 1=(4,5,6), 行 2=(7,8,9)），
            // v = (1,1,1)。 vᵀ·M = 列和 = (12,15,18)； M·v = 行和 = (6,15,24)。
            // 画成颜色 8bit：(12,15,18) vs (6,15,24) —— 一比即知，别靠记忆。
            Some("mtxtest") => v_raw.replace(
                "v_TexCoordFx = mul(vec3(a_TexCoord.xy, 1.0), xform);",
                "v_TexCoordFx = mul(vec3(1.0, 1.0, 1.0), mat3(0.01,0.02,0.03, 0.04,0.05,0.06, 0.07,0.08,0.09));",
            ),
            Some("hrow2") => v_raw.replace(
                "v_TexCoordFx = mul(vec3(a_TexCoord.xy, 1.0), xform);",
                "v_TexCoordFx = vec3(inverse(squareToQuad(g_Point0,g_Point1,g_Point2,g_Point3))[2][0], \
                 inverse(squareToQuad(g_Point0,g_Point1,g_Point2,g_Point3))[2][1], \
                 inverse(squareToQuad(g_Point0,g_Point1,g_Point2,g_Point3))[2][2]);",
            ),
            _ => v_raw,
        };
        let (vs, mut fs) = shader::compose_stages_with(&v_raw, &f_raw, &combos);
        // 探针：`WE_SHAPE_DEBUG=mask|fxc|uv` 时把片元的输出换成中间量，直接看死在哪一步。
        // （shape 全黑时，"四边形没光栅化"和"光栅化了但输出 0"在图像上完全一样，只能这样分辨。）
        if let Ok(mode) = std::env::var("WE_SHAPE_DEBUG") {
            // 细粒度探针（本次定位 lightshafts 全零用）：把片元里**每一步的中间量**直接画出来，
            // 这样"mask 为什么是 0"能拆成"z 分量符号 / 射线坐标 / 角向 smoothstep / 径向"四问。
            // 每个探针都是 `vec4(<三通道可视量>, 1.0)`，用 `WE_DUMP_RT` 读图层缓冲看数值。
            // ⚠ 门控同样只看**效果自己的源码**（`f_raw`）：看合成后的 `fs` 会因为 PRELUDE 注释里
            // 的 `v_TexCoordFx` 恒为真（见特效趟那处的长注释）。
            if v_raw.contains("v_TexCoordFx") || f_raw.contains("v_TexCoordFx") {
                let probe = match mode.as_str() {
                "mask" => "vec4(vec3(mask), 1.0)",
                "fxc" => "vec4(fxCoord, 0.0, 1.0)",
                "uv" => "vec4(v_TexCoord.xy, 0.0, 1.0)",
                // v_TexCoordFx 的三个分量（.xy 用 0..1 摆，.z 单独看符号）
                "vfxy" => "vec4(v_TexCoordFx.xy, 0.0, 1.0)",
                "vfz" => "vec4(vec3(step(0.0, v_TexCoordFx.z)), 1.0)",
                // 四个角点 uniform（若下发成功，整块应是同一个常量色）
                "pt0" => "vec4(g_Point0, 0.0, 1.0)",
                // 逐段：射线角向坐标 ×4、径向、角向平滑项、径向平滑项
                "mang" => "vec4(vec3(fxCoord.x), 1.0)",
                "mrad" => "vec4(vec3(fxCoord.y), 1.0)",
                "msx" => "vec4(vec3(smoothstep(0.50001, 0.5 - g_Feather.x, abs(fxCoord.x - 0.5))), 1.0)",
                "msy" => "vec4(vec3(smoothstep(0.50001, 0.5 - g_Feather.y, abs(fxCoord.y - 0.5))), 1.0)",
                "grad" => "vec4(vec3(1.0 - fxCoord.y), 1.0)",
                // 噪声纹理（g_Texture1）本身：黑 ⇒ fx=0 ⇒ 加法输出 0
                "noise" => "vec4(vec3(texSample2D(g_Texture1, v_TexCoord.xy).r), 1.0)",
                "fx" => "vec4(vec3(fx), 1.0)",
                // 直接画 v_TexCoordFx（配 WE_SHAPE_VPROBE 用）
                "rawv" => "vec4(v_TexCoordFx, 1.0)",
                // v_TexCoordFx.z 除以 8 再看（8bit 量化 ⇒ 0.031/级，够读 0..8）
                "vz8" => "vec4(vec3(v_TexCoordFx.z / 8.0), 1.0)",
                // 角向/径向分支里的中间量（在 smoothstep 之前）
                "rdmax" => "vec4(vec3(max(rayDelta.x, rayDelta.y)), 1.0)",
                // 关键 uniform 本身
                "gR" => "vec4(vec3(g_Radius), 1.0)",
                "gF" => "vec4(g_Feather, 0.0, 1.0)",
                _ => "vec4(1.0, 0.0, 1.0, 1.0)", // 命中测试：四边形范围内全品红
                };
                let n = fs.matches("gl_FragColor = albedo;").count();
                fs = fs.replace("gl_FragColor = albedo;", &format!("gl_FragColor = {probe};"));
                eprintln!("[探针] shape({}) 输出替换成 {probe}（命中 {n} 处）", lmp.shader);
            } else {
                eprintln!("[探针] WE_SHAPE_DEBUG={mode} 但该 shader 不含 v_TexCoordFx（{}）⇒ 未生效", lmp.shader);
            }
        }
        // 参数名 → uniform 名（注释里的 `{"material":"rayradius"}` 映射）
        // ⚠ **必须顶点 + 片元都解析**：`point0..3` 只声明在**顶点** shader 里，只扫片元就会
        // 找不到映射 ⇒ 四个角点**一次都没下发** ⇒ `squareToQuad(0,0,0,0)` 是退化矩阵 ⇒
        // `mask = step(0, v_TexCoordFx.z)` 恒 0 ⇒ **整个 shape 全黑**（实测这两条壁纸差分 0）。
        // 和坑 34（特效参数映射）是同一个错，只是换了个地方。
        let mut meta = shader::parse_shader_meta(&f_raw);
        let vmeta = shader::parse_shader_meta(&v_raw);
                    let vmeta_keys = vmeta.clone(); // 常量键→uniform 要连顶点 shader 一起查
        meta.uniforms.extend(vmeta.uniforms);
        let consts: Vec<(String, Value)> = raw_consts
            .iter()
            .filter_map(|(prop, v)| {
                // 常量键的两种写法（注解 material 标签 / uniform 名去 u_）都要认 ——
                // `shader::find_uniform_for_key`。shape 这一侧以前只认标签，和特效趟同一个坑。
                let u = shader::find_uniform_for_key(&meta, prop)
                              .or_else(|| shader::find_uniform_for_key(&vmeta_keys, prop))?;
                Some((u.name.clone(), v.clone()))
            })
            .collect();
        if let Ok(p) = std::env::var("WE_DUMP_SHADER") {
            let _ = std::fs::write(format!("{p}.shape.vert"), &vs);
            let _ = std::fs::write(format!("{p}.shape.frag"), &fs);
        }
        let layer = self.gl.make_mesh_layer(&vs, &fs, &quads)?;
        let _ = scene_h;

        // 底图贴图：shape 没有对象级贴图；材质趟给了就用，没有就绑白
        let src = match lmp.textures.first() {
            Some(t) => match self.upload_pkg_tex(pkg, pkg_bytes, assets, t) {
                Some(i) => Src::Tex(i),
                None => Src::White,
            },
            None => Src::White,
        };
        Ok((src, layer, consts))
    }

    /// 文字对象：文本内容（静态值 / WE 脚本）→ 光栅化成贴图 → 底图用"贴图直通"layer。
    /// 返回 `(贴图索引, 底图 layer)`；之后走**和图片层完全一样**的流程
    /// （特效链、混合、图层链接），所以这里只负责"把字变成一张贴图"。
    fn build_text_layer(
        &mut self,
        obj: &we_scene::scene::Object,
        pkg: &we_scene::pkg::Pkg,
        pkg_bytes: &[u8],
        assets: &std::path::Path,
    ) -> Result<(usize, gl_ctx::GlLayer), String> {
        let font_name = obj.font.clone().unwrap_or_default();
        // 先查**场景包**（WE 的包里会带 `fonts/*.ttf|otf`），再退 assets / 系统无衬线。
        let font_data = text::resolve_font_bytes(pkg, pkg_bytes, assets, &font_name)
            .ok_or_else(|| format!("字体找不到（{font_name}）"))?;

        // ── 文本内容 ────────────────────────────────────────────────────
        // 脚本（时钟/日期这类）用 boa 跑 `update(上一帧的文本)`；一秒内复用缓存结果
        // （脚本里的 `new Date` 是真实时钟，秒级刷新就够）。
        let script = obj.text.as_ref().and_then(text::script_body);
        let s = if let Some(body) = script {
            let fresh = match self.text_cache.get(&obj.id) {
                Some((_, t)) => self.time - *t < 1.0,
                None => false,
            };
            if fresh {
                self.text_cache.get(&obj.id).map(|(v, _)| v.clone()).unwrap_or_default()
            } else {
                let initial = self
                    .text_cache
                    .get(&obj.id)
                    .map(|(v, _)| v.clone())
                    .unwrap_or_default();
                match text::eval_script(&body, &initial) {
                    Ok(v) => {
                        self.text_cache.insert(obj.id, (v.clone(), self.time));
                        v
                    }
                    Err(e) => {
                        eprintln!("[文字] 「{}」脚本执行失败：{e}", obj.name);
                        initial
                    }
                }
            }
        } else {
            obj.text
                .as_ref()
                .and_then(text::static_value)
                .unwrap_or_default()
        };

        // ── 光栅化 ──────────────────────────────────────────────────────
        // 画布 = 对象的 `size`（场景单位）。光栅密度 `k`：
        // · 官方口径（`WE_TEXT_RT`，默认开）⇒ **k = 1**：1 画布像素 = 1 场景单位。
        // 这不是画质取舍，而是**量纲**问题：`blendoffset`、`(T0−T1)/2` 这些量都是
        // 以 RT 像素说话的（官方 `T0 ≡ RT`），k 一变 `g_Texture0Resolution` 就整体
        // 失真（2× ⇒ 同样的像素偏移只走一半）。官方文字 RT 就是"排版矩形 + 2×padding"
        // 的像素，1:1 对应场景单位；我们在 1920 宽的屏上仍有 ~3.4× 过采样。
        // · 旧口径（`WE_TEXT_RT=legacy`）⇒ `WE_TEXT_SCALE`（默认 2）。
        let official = text_rt_official();
        let size = we_scene::scene::parse_vec2(&obj.size).unwrap_or([256.0, 64.0]);
        let k = match std::env::var("WE_TEXT_SCALE")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
        {
            Some(v) => v.clamp(0.25, 8.0),
            None => {
                if official {
                    1.0
                } else {
                    2.0
                }
            }
        };
        // 光栅上限：以前硬编码 4096 ⇒ 场景单位下超过 4096 的文字层会被截掉（实测本机有超宽文字层）。
        // 提到 8192：既能让这类文字层完整落地，又不会出现 16384² 那种 1GB 级的风险分配。
        let w = ((size[0].abs() * k).round() as u32).clamp(1, 8192);
        let h = ((size[1].abs() * k).round() as u32).clamp(1, 8192);
        let pad = obj.padding.unwrap_or(0.0) * k;
        let halign = obj.horizontalalign.clone().unwrap_or_else(|| "left".into());
        // `verticalalign` 缺省按 OWE `align_or_default(verticalalign, alignment, top, bottom)`
        // ⇒ 就用对象自己的 `alignment`（图片层那套锚点），再没有才 center。
        let valign = obj
            .verticalalign
            .clone()
            .or_else(|| obj.alignment.clone())
            .unwrap_or_else(|| "center".into());
        let rgb = obj.color_rgb();
        // WE 的 `pointsize` 口径：OWE `TextPointSizeToPx` = `pointsize * 4`
        // （`SceneTextObjectParser.cpp:36-43`）。以前我们直接当像素用 ⇒ 字小 4 倍。
        let legacy = std::env::var("WE_TEXT_LAYOUT").ok().as_deref() == Some("legacy");
        // ── 场景排版框（`WE_TEXT_LAYOUT`，默认开）─────────────────────────────
        // 语义：可用区 = `obj.size − 2×padding`；`verticalalign`/`horizontalalign` 相对可用区；
        // 块高 = `ascent + (n−1)·line_h`（本字体 `descent` 悬挂在框外，见 `text.rs` 字段文档）。
        // 开关：
        // * `WE_TEXT_LAYOUT=0|legacy` ⇒ 逐字节回**旧**口径（块心/首行锚 + `ascent−descent`）；
        // * `WE_TEXRES=0`（旧口径总闸）⇒ 同样回旧口径（保证旧基线可逐字节复现）；
        // * `WE_TEXT_LAYOUT=1|scene` ⇒ 强制新口径（即使 `WE_TEXRES=0`）。
        let layout_scene = match std::env::var("WE_TEXT_LAYOUT")
            .map(|v| v.trim().to_ascii_lowercase())
            .ok()
            .as_deref()
        {
            Some("0") | Some("legacy") | Some("old") => false,
            Some("1") | Some("scene") | Some("new") => true,
            _ => per_tex_res_on(),
        };
        // em 标定：WE（Windows 版）/我们的光栅化在**同一** `pointsize` 下字号差一个
        // 无量纲比例（本场景实测：win 字形带 498.2 px vs 我们 483.1 px ⇒ 1.0297）。
        // 逐项排除见报告：字体确实加载了场景包里的 `PosterCompressed`（字形 bbox 恰为
        // 0.808 em，与 TTF 的 `glyf` 完全一致）、`scale` 只乘了一次（advance 宽度对得上
        // 1.649 em）、hinting/行框对墨迹高度贡献为 0 ⇒ 只能是 pointsize→px 的映射差。
        // 这是**全局标定**（所有文字层同比例），不是壁纸常数、更不是像素偏移；
        // `WE_TEXT_EM_FIT=<f>` 可覆盖（A/B 用），默认 1.0297。
        let em_fit = if layout_scene {
            std::env::var("WE_TEXT_EM_FIT")
                .ok()
                .and_then(|v| v.trim().parse::<f32>().ok())
                .filter(|v| v.is_finite() && *v > 0.0)
                .unwrap_or(1.0297)
        } else {
            1.0
        };
        let pts = (text::pointsize_to_px(obj.pointsize.unwrap_or(24.0))) * k * em_fit;
        // ── 白色填充底板（Windows 版行为，）──────────────────────────
        // 用户对 Windows 版的实测判据：「自定义文本是填充白色方块背景的 ⇒ 字体仅比白色边框
        // 小两个像素，上下左右」。⇒ 底板 = 字形墨迹 bbox **各边外扩 2 个最终屏幕像素**。
        // 单位换算是这里唯一要做对的事：本层贴图是 `size × k` 个**画布像素**，四边形的世界
        // 尺寸 = `size × obj.scale`，投影把 `cover` 宽映射到 `gl.w` 个**屏幕像素** ⇒
        // s = 屏幕像素 / 画布像素 = (gl.w / cover_w) · obj.scale.x / k
        // 本机 `3704273480` id=691 的实值：s = (1920/3840)·1.16768/2 = 0.29192
        // ⇒ pad = 2 / 0.29192 = 6.851 画布像素（= 2.000 屏幕像素）。
        // 关回旧行为：`WE_TEXT_PLATE=0`（一个字节都不动，含“不做底板对齐”）。
        // 微调边距：`WE_TEXT_PLATE_PX=<屏幕像素>`（默认 2）。
        // 默认**不**铺底板（用户 纠正：win 的白底在场景里本来就有，
        // 不该由渲染器凭空造一块）。要试底板实验才显式 WE_TEXT_PLATE=1。
        // 屏幕像素 / 画布像素（本层贴图）：所有「按屏幕像素说话」的旋钮都走这一个系数。
        // 本层贴图是 `size × k` 个**画布像素**，四边形的世界尺寸 = `size × obj.scale`，
        // 投影把 `cover` 宽映射到 `gl.w` 个**屏幕像素** ⇒
        // s = (gl.w / cover_w) · obj.scale.x / k
        // 本机 `3704273480` id=691 的实值：s = (1920/3840)·1.16768/2 = 0.29192。
        let objs = we_scene::scene::parse_vec3(&obj.scale).unwrap_or([1.0, 1.0, 1.0]);
        let cover_w = (self.cover.2 - self.cover.0).abs();
        let vis_w = if cover_w > 0.0 { cover_w } else { self.scene_size.0 };
        let zoom = self.gl.w as f32 / vis_w.max(1.0); // 屏幕像素 / 场景单位
        let s_px = zoom * objs[0].abs() / k; // 屏幕像素 / 画布像素
        let plate = if std::env::var("WE_TEXT_PLATE").ok().as_deref() != Some("1") {
            None
        } else {
            let want_px = std::env::var("WE_TEXT_PLATE_PX")
                .ok()
                .and_then(|v| v.trim().parse::<f32>().ok())
                .unwrap_or(2.0)
                .max(0.0);
            let s = s_px;
            if s > 1e-6 {
                let pad_canvas = want_px / s;
                if std::env::var_os("WE_TEXT_PLATE_DEBUG").is_some() {
                    eprintln!(
                        "[文字底板] id={} 「{}」画布 {}x{} 字号 {:.0} | zoom {:.4} scale {:.5} k {:.2} \
                         ⇒ 屏幕/画布 {:.5} ⇒ 边距 {:.1} 屏幕px = {:.2} 画布px",
                        obj.id, obj.name, w, h, pts, zoom, objs[0], k, s, want_px, pad_canvas
                    );
                }
                Some(pad_canvas)
            } else {
                None
            }
        };
        let (w0, _h0) = (w, h);
        let (mut rgba, mut tw, mut th) = if legacy {
            let pts_legacy = obj.pointsize.unwrap_or(24.0) * k;
            text::rasterize_legacy(&font_data, &s, pts_legacy, w, h, pad, &halign, rgb)?
        } else {
            let style = text::TextStyle {
                font_px: pts,
                padding: pad,
                halign: &halign,
                valign: &valign,
                color: rgb,
                plate,
                // 对照口径：`WE_TEXT_PLATE_ANCHOR=glyph` ⇒ 字形落点不动，底板只套在墨迹外
                plate_follow_glyph: std::env::var("WE_TEXT_PLATE_ANCHOR")
                    .ok()
                    .as_deref()
                    == Some("glyph"),
                // 首行锚定（`WE_TEXT_FIRSTLINE=0` 回旧的「块心居中」）：见 `text.rs` 字段文档。
                // 场景排版框打开时它不生效（新口径按可用区+行框，见 `ascent_box`）。
                firstline: env_flag("WE_TEXT_FIRSTLINE", true),
                // 默认**开**（官方 = max(排版矩形, 墨迹) + 2·pad，画布跟着墨迹长；否则长字符串被裁）
                canvas_grow: env_flag("WE_TEXT_CANVAS_GROW", true),
                // 场景排版框（`WE_TEXT_LAYOUT`，默认开）：可用区 = size−2·padding，
                // 块高 = ascent+(n−1)·line_h。`WE_TEXT_DY` 只是它的**手动微调**，
                // 默认 0（不再是任何硬编码像素）。
                ascent_box: layout_scene,
                // 屏幕像素级垂直位移（`WE_TEXT_DY`，**默认 0 = 逐字节不变**）：正数 = 向下。
                // 用户口径：「文本位置应该向下约 5 个像素」—— 拿本渲染器与
                // Windows 版的屏上截图比，整块字偏高。按**画布像素**整数平移（不重采样），
                // 换算用上面那个 `s_px`（屏幕px / 画布px，本层 0.29192）。
                // ⚠ 默认从 22 改回 **0** —— 22 是壁纸专属常数，换一张壁纸就会
                // 多移 22px。现在垂直位置由 `ascent_box` 从场景参数推导，这个旋钮只做手动 A/B。
                dy_canvas: {
                    let dy_px = std::env::var("WE_TEXT_DY")
                        .ok()
                        .and_then(|v| v.trim().parse::<f32>().ok())
                        .unwrap_or(0.0);
                    if dy_px != 0.0 && s_px > 1e-6 {
                        eprintln!(
                            "[文字] WE_TEXT_DY={dy_px:+.1} 屏幕px ⇒ {:.2} 画布px（屏幕/画布 {s_px:.5}）",
                            dy_px / s_px
                        );
                        dy_px / s_px
                    } else {
                        0.0
                    }
                },
            };
            text::rasterize(&font_data, &s, &style, w, h)?
        };

        // ── 官方铁律①：文字层 RT = 「墨迹 bbox + 2×padding」 ────────────────────
        // 一手依据：`wallpaper64.exe` `TextLayer::vtable+0x128` = `FUN_140258ab0`
        // ：
        // w = 排版矩形宽, h = 排版矩形高, pad = min(padding, 512), out = bbox + 2·pad
        // ⇒ 缓冲尺寸**只由这一层自己的排版结果决定**（没有输出分辨率/`renderScale` 参与）。
        // 几何配套：四边形同步换成这块矩形（`text_quads`），所以字形在屏上的落点
        // 与"整幅画布"口径**逐像素相同**，变的只是缓冲（= `T0`）的像素网格。
        self.text_quads.remove(&obj.id);
        let mut ink_crop: Option<text::InkCrop> = None;
        if official {
            let pad_src = obj.padding.unwrap_or(0.0).clamp(0.0, 512.0);
            let pad_px = pad_src * k;
            // 排版矩形 = **可用区**：`obj.size` 四边内缩 padding（`text.rs::rasterize` 就是按
            // 这个矩形摆字的）⇒ 再各边外扩 `padding` 正好回到 `obj.size`。文字溢出时
            // `crop_to_ink` 会取 `union(排版矩形, 墨迹)` ⇒ RT 随墨迹长大。
            // `WE_TEXT_RT=ink` ⇒ 只用墨迹紧包围盒（A/B 用，实测与 win 的 `T0_h` 不符）。
            // `WE_TEXT_CANVAS_GROW` may have grown the canvas symmetrically by `m` px
            // (`text.rs::rasterize`): the typeset rect is then inset by `pad + m` in the
            // *grown* canvas -- the same scene rect as before, because the glyphs were
            // translated by `+m` too (pure translation, not a resize).
            let grow_m = ((tw as f32 - w0 as f32) * 0.5).max(0.0);
            let pad_layout = pad_px + grow_m;
            let layout = if std::env::var("WE_TEXT_RT").ok().as_deref() == Some("ink") {
                None
            } else {
                let (fw, fh) = (tw as f32, th as f32);
                ((fw - 2.0 * pad_layout) > 4.0 && (fh - 2.0 * pad_layout) > 4.0)
                    .then(|| [pad_layout, pad_layout, fw - pad_layout, fh - pad_layout])
            };
            let (c, cw, ch, crop) =
                text::crop_to_ink(rgba, tw as usize, th as usize, pad_px, layout);
            if std::env::var_os("WE_DBG_TEXT_RT").is_some() {
                eprintln!(
                    "[文字RT] id={} 「{}」画布 {}x{} pad={:.2}(场景)·{:.2}(px) ⇒ RT {}x{} @({:.0},{:.0})",
                    obj.id, obj.name, tw, th, pad_src, pad_src * k,
                    crop.rect[2], crop.rect[3], crop.rect[0], crop.rect[1]
                );
            }
            rgba = c;
            tw = cw;
            th = ch;
            ink_crop = Some(crop);
        }
        // ⚠ 已证伪的一条（留档，别再试）：把 RT 内容按「墨迹中心 = RT 中心」摆正、
        // 四边形反向平移同一量 —— 对**纸上落点**是零效果（内容与四边形一起动），
        // 对 `blend` 的采样也**零效果**：那趟的 uv 只取决于 `T0`/`T1` 的**尺寸**，
        // 与内容在 RT 里的位置无关。实测（1920×1080，`--t 5`）：字窗内容
        // ncc/scale/dx/dy 全不变（1.01 / -6 / -85）。⇒ 残差只能出在尺寸或 shader 的
        // 像素偏移项，不在"RT 里怎么摆"。

        // 局部几何：裁切块的中心相对整幅画布中心的偏移（场景单位，y 向上）。
        if let Some(crop) = ink_crop {
            let (cw, ch) = (crop.rect[2], crop.rect[3]);
            if crop.canvas[0] > 0.0 && crop.canvas[1] > 0.0 && k > 0.0 {
                let dx = (crop.rect[0] + cw * 0.5 - crop.canvas[0] * 0.5) / k;
                let dy = (crop.rect[1] + ch * 0.5 - crop.canvas[1] * 0.5) / k;
                self.text_quads
                    .insert(obj.id, ([dx, -dy], [cw / (2.0 * k), ch / (2.0 * k)]));
            }
        }

        // ── 诊断：`WE_TEXT_OPAQUE=1` 把整块文字画布的 alpha 拉满 ─────────────────
        // 用途：玻璃字（effects/blend + OPACITYMASK）只让遮罩在**字形窗口**里露出来，
        // 量不到"透出来的角色"整体有多高。配合 `WE_FX_PROBE=blend`（把混合源铺满四边形）
        // 一起开，就能在**最终画面**上量到那个角色的真实像素高度 ⇒ A/B 比例判据有据可依。
        // 诊断专用：默认关，关时一个字节都不动。
        if std::env::var_os("WE_TEXT_OPAQUE").is_some() {
            for i in (3..rgba.len()).step_by(4) {
                rgba[i] = 255;
            }
            eprintln!("[文字] WE_TEXT_OPAQUE ⇒ 画布 alpha 全拉满（诊断用）");
        }

        // ── 诊断：`WE_TEXT_MARKER=1` 在画布的四条边各刷一条纯色带 ──────────────
        // 用途：**一次性定死"画布行 → 屏幕 y"的映射（含有没有 V 反）**——
        // 上=红、下=绿、左=蓝、右=黄。渲染后量四条带落在屏幕矩形哪条边即可。
        if std::env::var_os("WE_TEXT_MARKER").is_some() {
            let (w_, h_) = (tw as usize, th as usize);
            let band = (h_ / 40).max(2);
            let mut put = |x: usize, y: usize, c: [u8; 4]| {
                if x < w_ && y < h_ {
                    let i = (y * w_ + x) * 4;
                    rgba[i] = c[0];
                    rgba[i + 1] = c[1];
                    rgba[i + 2] = c[2];
                    rgba[i + 3] = 255;
                }
            };
            for y in 0..band {
                for x in 0..w_ { put(x, y, [255, 0, 0, 255]); }
            }
            for y in h_ - band..h_ {
                for x in 0..w_ { put(x, y, [0, 255, 0, 255]); }
            }
            for y in 0..h_ {
                for x in 0..band { put(x, y, [0, 0, 255, 255]); }
                for x in w_ - band..w_ { put(x, y, [255, 255, 0, 255]); }
            }
        }

        // `WE_SETPROP_FAST=1`：把这层文字留一份**覆盖率掩膜**，供"换色不重建"用
        // （见 `refresh_user_prop`）。默认关 ⇒ 一块内存都不多占。
        if setprop_fast_on() {
            let n = (tw as usize) * (th as usize);
            if rgba.len() >= n * 4 {
                let mut alpha = Vec::with_capacity(n);
                for i in 0..n {
                    alpha.push(rgba[i * 4 + 3]);
                }
                // ⚠ 存 **Vec**：同一个文字对象可能被建不止一次（不同趟/不同诊断路径各建一张），
                // 哪一张真正被采样取决于建场景的顺序 ⇒ 换色时**每一张都换**，不会赌错。
                self.text_recs.entry(obj.id).or_default().push(TextRec {
                    tex: self.texs.len(),
                    w: tw,
                    h: th,
                    alpha,
                    plate: plate.is_some(),
                    cur_rgb: [
                        (rgb[0].clamp(0.0, 1.0) * 255.0).round() as u8,
                        (rgb[1].clamp(0.0, 1.0) * 255.0).round() as u8,
                        (rgb[2].clamp(0.0, 1.0) * 255.0).round() as u8,
                    ],
                    pending: None,
                });
            }
        }
        let t = self
            .gl
            .make_texture(&rgba, tw, th)
            .map_err(|e| e.to_string())?;
        // 调试：把光栅化结果落盘（`WE_TEXT_DUMP=<目录>`）—— 判断"字没画出来"还是"没显示出来"
        if let Ok(dir) = std::env::var("WE_TEXT_DUMP") {
            let _ = std::fs::create_dir_all(&dir);
            let safe: String = obj
                .name
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { '_' })
                .collect();
            let path = std::path::Path::new(&dir).join(format!("text_{}_{}.png", obj.id, safe));
            let _ = image::save_buffer(&path, &rgba, tw, th, image::ColorType::Rgba8);
            eprintln!(
                "[文字] dump {} ← {:?}（{}x{}，字号 {:.0}）",
                path.display(),
                s,
                tw,
                th,
                pts
            );
        }
        self.texs.push(t);
        let idx = self.texs.len() - 1;
        self.text_count += 1;
        let layer = self.make_blit_layer()?;
        Ok((idx, layer))
    }

    /// 建 bloom 的 3 张 RT + 3 趟全屏后处理；返回泛光缓冲（present 那一趟拿它做 combine）。
    /// 尺寸照参考实现：`_rt_4FrameBuffer` = 输出/4、`_rt_8FrameBuffer` = `_rt_Bloom` = 输出/8
    /// （`CScene::setupFramebuffers`）。三趟都写 RT（`Blend::Opaque`，整张覆盖），
    /// 所以先建小的 RT 也不会读到脏内容。
    fn push_bloom(
        &mut self,
        scene_rt: &usize,
        out_w: u32,
        out_h: u32,
        cfg: &BloomCfg,
        full_q: [f32; 16],
    ) -> Result<usize, String> {
        let (q, qh) = ((out_w / 4).max(1), (out_h / 4).max(1));
        let (e, eh) = ((out_w / 8).max(1), (out_h / 8).max(1));
        let rt4 = self.new_rt(q, qh, "_rt_4FrameBuffer")?;
        let rt8 = self.new_rt(e, eh, "_rt_8FrameBuffer")?;
        let rtb = self.new_rt(e, eh, "_rt_Bloom")?;
        // `g_TexelSize` = 场景合成图的纹素尺寸（参考实现 `CPass.cpp:883`：`1/scene.getWidth`）。
        // 三趟都用同一个值（blur 的步长写死 `×8`，正好等于第八档缓冲的一纹素）。
        let texel = Value::V2(1.0 / out_w.max(1) as f32, 1.0 / out_h.max(1) as f32);

        let mk = |gl: &gl_ctx::Gl, v: &str, f: &str| -> Result<gl_ctx::GlLayer, String> {
            let (cv, cf) = shader::compose_stages_with(v, f, &[]);
            gl.make_layer(&cv, &cf, None)
        };
        let l0 = mk(&self.gl, BLOOM_DSQ_VERT, BLOOM_DSQ_FRAG)?;
        let l1 = mk(&self.gl, BLOOM_BLURV_VERT, BLOOM_BLUR_FRAG)?;
        let l2 = mk(&self.gl, BLOOM_BLURH_VERT, BLOOM_BLUR_FRAG)?;

        let scal = |extra: Vec<(String, Value)>| {
            let mut s = vec![("g_TexelSize".to_string(), texel)];
            s.extend(extra);
            s
        };
        // pass0：降采样到 1/4 + 阈值 + 强度 + 染色
        self.passes.push(Pass {
            layer: l0,
            target: Some(rt4),
            units: vec![(0, Src::Rt(*scene_rt))],
            scalars: scal(vec![
                ("g_BloomStrength".into(), Value::F(cfg.strength)),
                ("g_BloomThreshold".into(), Value::F(cfg.threshold)),
                (
                    "g_BloomTint".into(),
                    Value::V3(cfg.tint[0], cfg.tint[1], cfg.tint[2]),
                ),
            ]),
            mvp: full_q,
            blend: gl_ctx::Blend::Opaque,
            mesh_anim: None,
        });
        // pass1：降到 1/8 + 纵向 13 抽头高斯
        self.passes.push(Pass {
            layer: l1,
            target: Some(rt8),
            units: vec![(0, Src::Rt(rt4))],
            scalars: scal(vec![]),
            mvp: full_q,
            blend: gl_ctx::Blend::Opaque,
            mesh_anim: None,
        });
        // pass2：横向 13 抽头高斯 → 泛光缓冲
        self.passes.push(Pass {
            layer: l2,
            target: Some(rtb),
            units: vec![(0, Src::Rt(rt8))],
            scalars: scal(vec![]),
            mvp: full_q,
            blend: gl_ctx::Blend::Opaque,
            mesh_anim: None,
        });
        println!(
            "  [bloom] 场景 {out_w}x{out_h} ⇒ 1/4 {q}x{qh} + 1/8 {e}x{eh}；强度 {:.4} 阈值 {:.4} 染色 {:?}",
            cfg.strength, cfg.threshold, cfg.tint
        );
        Ok(rtb)
    }

    /// present 那一趟的 shader：**画面 + 泛光**（= 官方 `combine`），调过色时顺带烘进调色。
    fn make_combine_layer(&self) -> Result<gl_ctx::GlLayer, String> {
        let c = current_color();
        let (grading, out) = if color_is_identity(&c) {
            (String::new(), "clamp(albedo, 0.0, 1.0)".to_string())
        } else {
            (
                format!(
                    "  albedo = (albedo - 0.5) * {k1:.4} + 0.5;\n\
                       float luma = dot(albedo, vec3(0.2126, 0.7152, 0.0722));\n\
                       albedo = mix(vec3(luma), albedo, {k2:.4});\n\
                       albedo *= {k0:.4};\n\
                       float ca = cos({k3:.4}), sa = sin({k3:.4});\n\
                       mat3 hr = mat3(\n\
                         0.299 + 0.701*ca + 0.168*sa, 0.587 - 0.587*ca + 0.330*sa, 0.114 - 0.114*ca - 0.497*sa,\n\
                         0.299 - 0.299*ca - 0.328*sa, 0.587 + 0.413*ca + 0.035*sa, 0.114 - 0.114*ca + 0.292*sa,\n\
                         0.299 - 0.300*ca + 1.250*sa, 0.587 - 0.588*ca - 1.050*sa, 0.114 + 0.886*ca - 0.203*sa);\n\
                       albedo = hr * albedo;\n\
                       albedo.r *= (1.0 + {k4:.4} * 0.10);\n\
                       albedo.b *= (1.0 - {k4:.4} * 0.10);\n\
                       albedo = pow(max(albedo, vec3(0.0)), vec3(1.0 / max({k5:.4}, 0.01)));\n",
                    k0 = c[0], k1 = c[1], k2 = c[2], k3 = c[3], k4 = c[4], k5 = c[5]
                ),
                "clamp(albedo, 0.0, 1.0)".to_string(),
            )
        };
        let cf = format!(
            "varying vec2 v_TexCoord;\n\
             uniform sampler2D g_Texture0;\n\
             uniform sampler2D g_Texture1;\n\
             void main() {{\n\
               vec3 albedo = texSample2D(g_Texture0, v_TexCoord).rgb\n\
                           + texSample2D(g_Texture1, v_TexCoord).rgb;\n\
             {exposure}\
             {grading}\
               gl_FragColor = vec4({out}, 1.0);\n\
             }}\n",
            exposure = ExposureCfg::from_env().glsl_for("albedo"),
        );
        let (cv, cf) = shader::compose_stages_with(BLOOM_COMBINE_VERT, &cf, &[]);
        let mut l = self.gl.make_layer(&cv, &cf, None)?;
        // 采样 FBO 贴到窗口 ⇒ 必须翻 V（与原来的 present blit 同一理由）。
        l.raw_position = true;
        Ok(l)
    }

    /// **present 趟专用的 blit 层**：与 `make_blit_layer` 相同，但多一条 `WE_EXPOSURE`
    /// 的曝光/色调映射。**只有 present 这一条路带它** —— 链内 RT→RT / 文字那些 blit 走的
    /// 还是 `make_blit_layer`，否则同一帧里会被叠好几次。
    /// 单位值（默认）⇒ 直接返回 `make_blit_layer` 的结果，**逐字节不变**。
    fn make_present_layer(&self) -> Result<gl_ctx::GlLayer, String> {
        let ex = ExposureCfg::from_env();
        if ex.is_identity() {
            return self.make_blit_layer();
        }
        let c = current_color();
        let cv = "attribute vec3 a_Position;\nattribute vec2 a_TexCoord;\nvarying vec2 v_TexCoord;\nuniform mat4 g_ModelViewProjectionMatrix;\nvoid main() { gl_Position = mul(vec4(a_Position, 1.0), g_ModelViewProjectionMatrix); v_TexCoord = a_TexCoord; }\n";
        let (consts, grading) = Self::color_grading_src(&c);
        let cf = format!(
            "varying vec2 v_TexCoord;\nuniform sampler2D g_Texture0;\n{consts}\
             void main() {{\n\
               vec4 src = texSample2D(g_Texture0, v_TexCoord);\n\
               vec3 c = src.rgb;\n\
             {ex}{grading}\
               gl_FragColor = vec4(clamp(c, 0.0, 1.0), src.a);\n\
             }}\n",
            consts = consts,
            ex = ex.glsl(),
            grading = grading
        );
        let (vs, fs) = shader::compose_stages_with(cv, &cf, &[]);
        self.gl.make_layer(&vs, &fs, None)
    }

    /// `WE_COLOR` 调色那段的**常量声明**与**算式**（作用在已声明的 `vec3 c` 上）。
    /// 单位值 ⇒ 两个空串。
    fn color_grading_src(c: &[f32; 6]) -> (String, String) {
        if color_is_identity(c) {
            return (String::new(), String::new());
        }
        let consts = format!(
            "const float kBright = {:.4};\nconst float kContrast = {:.4};\n\
             const float kSatur = {:.4};\nconst float kHue = {:.4};\n\
             const float kTemp = {:.4};\nconst float kGamma = {:.4};\n",
            c[0], c[1], c[2], c[3], c[4], c[5]
        );
        let ops = "  c = (c - 0.5) * kContrast + 0.5;\n\
                   float luma = dot(c, vec3(0.2126, 0.7152, 0.0722));\n\
                   c = mix(vec3(luma), c, kSatur);\n\
                   c *= kBright;\n\
                   float ca = cos(kHue), sa = sin(kHue);\n\
                   mat3 hr = mat3(\n\
                     0.299 + 0.701*ca + 0.168*sa, 0.587 - 0.587*ca + 0.330*sa, 0.114 - 0.114*ca - 0.497*sa,\n\
                     0.299 - 0.299*ca - 0.328*sa, 0.587 + 0.413*ca + 0.035*sa, 0.114 - 0.114*ca + 0.292*sa,\n\
                     0.299 - 0.300*ca + 1.250*sa, 0.587 - 0.588*ca - 1.050*sa, 0.114 + 0.886*ca - 0.203*sa);\n\
                   c = hr * c;\n\
                   c.r *= (1.0 + kTemp * 0.10);\n\
                   c.b *= (1.0 - kTemp * 0.10);\n\
                   c = pow(max(c, vec3(0.0)), vec3(1.0 / max(kGamma, 0.01)));\n"
            .to_string();
        (consts, ops)
    }

    fn make_blit_layer(&self) -> Result<gl_ctx::GlLayer, String> {
        let cv = "attribute vec3 a_Position;\nattribute vec2 a_TexCoord;\nvarying vec2 v_TexCoord;\nuniform mat4 g_ModelViewProjectionMatrix;\nvoid main() { gl_Position = mul(vec4(a_Position, 1.0), g_ModelViewProjectionMatrix); v_TexCoord = a_TexCoord; }\n";
        // 色彩调整：**单位值时用原来那条 shader**（逐字节不变 ⇒ 零回归、零成本），
        // 调过色才换成带调色的版本，参数直接烘成常数。
        let cf = {
            let c = current_color();
            if color_is_identity(&c) {
                "varying vec2 v_TexCoord;\nuniform sampler2D g_Texture0;\nvoid main() { gl_FragColor = texSample2D(g_Texture0, v_TexCoord); }\n".to_string()
            } else {
                format!(
                    "varying vec2 v_TexCoord;\nuniform sampler2D g_Texture0;\n\
                     const float kBright = {:.4};\nconst float kContrast = {:.4};\n\
                     const float kSatur = {:.4};\nconst float kHue = {:.4};\n\
                     const float kTemp = {:.4};\nconst float kGamma = {:.4};\n\
                     void main() {{\n\
                       vec4 src = texSample2D(g_Texture0, v_TexCoord);\n\
                       vec3 c = src.rgb;\n\
                       c = (c - 0.5) * kContrast + 0.5;\n\
                       float luma = dot(c, vec3(0.2126, 0.7152, 0.0722));\n\
                       c = mix(vec3(luma), c, kSatur);\n\
                       c *= kBright;\n\
                       float ca = cos(kHue), sa = sin(kHue);\n\
                       mat3 hr = mat3(\n\
                         0.299 + 0.701*ca + 0.168*sa, 0.587 - 0.587*ca + 0.330*sa, 0.114 - 0.114*ca - 0.497*sa,\n\
                         0.299 - 0.299*ca - 0.328*sa, 0.587 + 0.413*ca + 0.035*sa, 0.114 - 0.114*ca + 0.292*sa,\n\
                         0.299 - 0.300*ca + 1.250*sa, 0.587 - 0.588*ca - 1.050*sa, 0.114 + 0.886*ca - 0.203*sa);\n\
                       c = hr * c;\n\
                       c.r *= (1.0 + kTemp * 0.10);\n\
                       c.b *= (1.0 - kTemp * 0.10);\n\
                       c = pow(max(c, vec3(0.0)), vec3(1.0 / max(kGamma, 0.01)));\n\
                       gl_FragColor = vec4(clamp(c, 0.0, 1.0), src.a);\n\
                     }}\n",
                    c[0], c[1], c[2], c[3], c[4], c[5]
                )
            }
        };
        let (vs, fs) = shader::compose_stages_with(cv, &cf, &[]);
        self.gl.make_layer(&vs, &fs, None)
    }

    /// **原样拷贝**一层缓冲到另一张同尺寸 RT（用于链接 RT 的“发布口”）。
    /// 与 [`Self::make_blit_layer`] 同一条顶点/片元，但**不带**全局色彩调整
    /// （`current_color` 是上屏阶段的调整；发布口要给消费方的是这一层的原始输出，
    /// 否则消费方再被调色一次 = 双重调色）。
    fn make_copy_layer(&self) -> Result<gl_ctx::GlLayer, String> {
        let cv = "attribute vec3 a_Position;\nattribute vec2 a_TexCoord;\nvarying vec2 v_TexCoord;\nuniform mat4 g_ModelViewProjectionMatrix;\nvoid main() { gl_Position = mul(vec4(a_Position, 1.0), g_ModelViewProjectionMatrix); v_TexCoord = a_TexCoord; }\n";
        let cf = "varying vec2 v_TexCoord;\nuniform sampler2D g_Texture0;\nvoid main() { gl_FragColor = texSample2D(g_Texture0, v_TexCoord); }\n";
        let (vs, fs) = shader::compose_stages_with(cv, cf, &[]);
        self.gl.make_layer(&vs, &fs, None)
    }

    /// 收尾合成趟的**网格版**：与 [`Self::make_blit_layer`] 同一条 shader
    /// （`a_Position`/`a_TexCoord` → `g_Texture0`），但几何是 puppet 网格 ⇒ 采样坐标 = 网格自己的 uv。
    /// 用于 uv-空间方案下把"展开的图层缓冲"按**本帧装配几何**摆进场景（丝袜/掩码跟着腿动）。
    fn make_blit_layer_mesh(
        &self,
        positions: &[[f32; 3]],
        uvs: &[[f32; 2]],
        indices: &[u16],
        sway: [f32; 5],
    ) -> Result<gl_ctx::GlLayer, String> {
        let cv = "attribute vec3 a_Position;\nattribute vec2 a_TexCoord;\nvarying vec2 v_TexCoord;\nuniform mat4 g_ModelViewProjectionMatrix;\nvoid main() { gl_Position = mul(vec4(a_Position, 1.0), g_ModelViewProjectionMatrix); v_TexCoord = a_TexCoord; }\n";
        let cf = {
            let c = current_color();
            if color_is_identity(&c) {
                "varying vec2 v_TexCoord;\nuniform sampler2D g_Texture0;\nvoid main() { gl_FragColor = texSample2D(g_Texture0, v_TexCoord); }\n".to_string()
            } else {
                format!(
                    "varying vec2 v_TexCoord;\nuniform sampler2D g_Texture0;\n\
                     const float kBright = {:.4};\nconst float kContrast = {:.4};\n\
                     const float kSatur = {:.4};\nconst float kHue = {:.4};\n\
                     const float kTemp = {:.4};\nconst float kGamma = {:.4};\n\
                     void main() {{\n\
                       vec4 src = texSample2D(g_Texture0, v_TexCoord);\n\
                       vec3 c = src.rgb;\n\
                       c = (c - 0.5) * kContrast + 0.5;\n\
                       float luma = dot(c, vec3(0.2126, 0.7152, 0.0722));\n\
                       c = mix(vec3(luma), c, kSatur);\n\
                       c *= kBright;\n\
                       float ca = cos(kHue), sa = sin(kHue);\n\
                       mat3 hr = mat3(\n\
                         0.299 + 0.701*ca + 0.168*sa, 0.587 - 0.587*ca + 0.330*sa, 0.114 - 0.114*ca - 0.497*sa,\n\
                         0.299 - 0.299*ca - 0.328*sa, 0.587 + 0.413*ca + 0.035*sa, 0.114 - 0.114*ca + 0.292*sa,\n\
                         0.299 - 0.300*ca + 1.250*sa, 0.587 - 0.588*ca - 1.050*sa, 0.114 + 0.886*ca - 0.203*sa);\n\
                       c = hr * c;\n\
                       c.r *= (1.0 + kTemp * 0.10);\n\
                       c.b *= (1.0 - kTemp * 0.10);\n\
                       c = pow(max(c, vec3(0.0)), vec3(1.0 / max(kGamma, 0.01)));\n\
                       gl_FragColor = vec4(clamp(c, 0.0, 1.0), src.a);\n\
                     }}\n",
                    c[0], c[1], c[2], c[3], c[4], c[5]
                )
            }
        };
        let (vs, fs) = shader::compose_stages_with(cv, &cf, &[]);
        self.gl.make_indexed_layer(&vs, &fs, positions, uvs, indices, sway)
    }

    /// 开屏后探一次「每层的内容在它自己的链 RT 里占哪一块」（`WE_PERF_SCISSOR` 用）。
///
    /// # 怎么探（不猜、不硬编码）
    /// 把这个对象的**底图趟**原样画进一张 `64x64` 的探针 RT —— 底图趟的 MVP 与真实
    /// 链 RT 用的是同一个（NDC 口径），所以 64x64 上量到的 alpha 包围盒按比例放大就是
    /// 真实 RT 上的包围盒。这一步把"贴图 alpha 在哪 / 网格装配到哪 / UV 子矩形"全部交给
    /// GPU 自己回答，不用我推导任何一层映射。
///
    /// # 为什么可以拿它当 scissor（输出为什么逐字节不变）
    /// 包围盒外那一圈像素在原路径里**每个通道都是 0**（内容覆盖率实测：1016 = 46%、
    /// 350 = 20%、691 = 24%）；跳过写 0 的像素 ⇒ 目标缓冲里的值不变。要让这条成立只需
    /// 两件事：① 包围盒**保守**（探针格 + padding）；② 那些像素从来没有被清成别的颜色
    /// （链 RT 只在开屏时由 `create_rt` 清零，之后每帧只被这些 scissored 趟写）。
    /// padding = `max(2 个探针格, 8% 短边) + 96px`：覆盖网格摆动（puppet sway）与
    /// 特效的采样半径（本场景最大 light-ray 步长 44 texel）。`WE_PERF_SCISSOR_PAD` 可调。
    fn probe_content_boxes(&mut self) -> Result<(), String> {
        if !perf_scissor_on() || self.bbox_jobs.is_empty() {
            return Ok(());
        }
        const P: u32 = 64;
        let probe = self.new_rt(P, P, "_rt_bbox_probe")?;
        let jobs = self.bbox_jobs.clone();
        let mut frac: Vec<Option<[f32; 4]>> = Vec::with_capacity(jobs.len());
        for (pi, _) in &jobs {
            let p = &self.passes[*pi];
            let mut units = Vec::new();
            for (u, s) in &p.units {
                let (t, _, _) = self.src_tex(*s);
                // 探针只关心**覆盖范围**：分辨率报 64,64 就够（只影响 shader 里的像素级算子）。
                units.push(gl_ctx::UnitTex::new(*u, t, P, P));
            }
            let ctx = gl_ctx::DrawCtx {
                target: Some(&self.rts[probe]),
                mvp: p.mvp,
                units: &units,
                scalars: &p.scalars,
                time: Some(self.time),
                blend: gl_ctx::Blend::Opaque,
            };
            self.gl.bind_target(Some(&self.rts[probe]));
            self.gl.scissor(None);
            self.gl.clear(0.0, 0.0, 0.0, 0.0);
            self.gl.color_mask(true, true, true, true);
            if let Err(e) = self.gl.draw_pass(&p.layer, &ctx) {
                eprintln!("[剪裁] ⚠ 探针（趟 {pi}）失败，该层不剪裁：{e}");
                frac.push(None);
                continue;
            }
            let px = self.gl.read_rt_pixels(&self.rts[probe]);
            let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
            for y in 0..P {
                for x in 0..P {
                    let a = px[((y * P + x) * 4 + 3) as usize];
                    if a > 8 {
                        x0 = x0.min(x);
                        x1 = x1.max(x);
                        y0 = y0.min(y);
                        y1 = y1.max(y);
                    }
                }
            }
            if x0 == u32::MAX {
                frac.push(None);
                continue;
            }
            let inv = 1.0 / P as f32;
            frac.push(Some([
                x0 as f32 * inv,
                y0 as f32 * inv,
                (x1 + 1) as f32 * inv,
                (y1 + 1) as f32 * inv,
            ]));
        }
        self.rt_bbox = vec![None; self.rts.len()];
        let pad_frac = env_f32("WE_PERF_SCISSOR_PAD", 0.08);
        let mut saved = 0usize;
        let mut total = 0usize;
        for ((_, rts), f) in jobs.iter().zip(frac.iter()) {
            let Some(f) = f else { continue };
            for rt in rts {
                let r = self.rts[*rt];
                let pad = ((pad_frac * r.w.min(r.h) as f32) + 96.0).round() as i32;
                let x0 = (f[0] * r.w as f32).floor() as i32 - pad;
                let y0 = (f[1] * r.h as f32).floor() as i32 - pad;
                let x1 = (f[2] * r.w as f32).ceil() as i32 + pad;
                let y1 = (f[3] * r.h as f32).ceil() as i32 + pad;
                let (x0, y0) = (x0.max(0), y0.max(0));
                let (x1, y1) = (x1.min(r.w as i32), y1.min(r.h as i32));
                let (w, h) = (x1 - x0, y1 - y0);
                if w <= 0 || h <= 0 {
                    continue;
                }
                total += (r.w * r.h) as usize;
                saved += ((r.w * r.h) as i32 - w * h).max(0) as usize;
                // 覆盖率 > 90% ⇒ 剪了也没意义（还多一次状态切换）⇒ 不剪
                if (w as f32 * h as f32) < 0.90 * (r.w * r.h) as f32 {
                    self.rt_bbox[*rt] = Some([x0, y0, w, h]);
                }
            }
        }
        if std::env::var_os("WE_PERF_DEBUG").is_some() {
            for (i, b) in self.rt_bbox.iter().enumerate() {
                if let Some(b) = b {
                    eprintln!(
                        "[剪裁] RT[{i}] {}x{} → 剪到 {:?}（{:.0}% 面积）",
                        self.rts[i].w,
                        self.rts[i].h,
                        b,
                        100.0 * (b[2] * b[3]) as f32 / (self.rts[i].w * self.rts[i].h) as f32
                    );
                }
            }
        }
        eprintln!(
            "[剪裁] WE_PERF_SCISSOR：{} 张链 RT 探到内容包围盒，平均省 {:.0}% 像素",
            self.rt_bbox.iter().filter(|b| b.is_some()).count(),
            if total > 0 {
                100.0 * saved as f32 / total as f32
            } else {
                0.0
            }
        );
        Ok(())
    }

    /// 把**一趟**原样画进探针 RT，返回它的内容包围盒（归一化 `[x0,y0,x1,y1]`，覆盖比例）。
    /// `thr` = alpha 阈值（链趟用 8；遮罩烘焙沿用 0 —— 它是预乘写入，"非零"就该算内容）。
    /// `g_Texture{N}Resolution` 报**真实**尺寸：按像素算偏移的特效在探针上才有同一个归一化范围。
    fn probe_one_pass(&self, pi: usize, probe: usize, thr: u8) -> Option<[f32; 4]> {
        const P: u32 = 256;
        let p = &self.passes[pi];
        let mut units = Vec::new();
        for (u, s) in &p.units {
            let (tx, tw, th) = self.src_tex(*s);
            units.push(gl_ctx::UnitTex::new(*u, tx, tw.max(1), th.max(1)));
        }
        let ctx = gl_ctx::DrawCtx {
            target: Some(&self.rts[probe]),
            mvp: p.mvp,
            units: &units,
            scalars: &p.scalars,
            time: Some(self.time),
            blend: gl_ctx::Blend::Opaque,
        };
        self.gl.bind_target(Some(&self.rts[probe]));
        self.gl.scissor(None);
        self.gl.clear(0.0, 0.0, 0.0, 0.0);
        self.gl.color_mask(true, true, true, true);
        if let Err(e) = self.gl.draw_pass(&p.layer, &ctx) {
            if std::env::var_os("WE_PERF_DEBUG").is_some() {
                eprintln!("[剪裁·逐趟] ⚠ 探针（趟 {pi}）失败，该趟不剪：{e}");
            }
            return None;
        }
        let px = self.gl.read_rt_pixels(&self.rts[probe]);
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
        for y in 0..P {
            for x in 0..P {
                if px[((y * P + x) * 4 + 3) as usize] > thr {
                    x0 = x0.min(x);
                    x1 = x1.max(x);
                    y0 = y0.min(y);
                    y1 = y1.max(y);
                }
            }
        }
        if x0 == u32::MAX {
            return None;
        }
        let inv = 1.0 / P as f32;
        Some([
            x0 as f32 * inv,
            y0 as f32 * inv,
            (x1 + 1) as f32 * inv,
            (y1 + 1) as f32 * inv,
        ])
    }

    /// **逐趟**内容包围盒（`WE_PERF_SCISSOR_PP`）：第 1 帧画完之后跑一次。
///
    /// # 为什么必须在「画完一帧之后」探
    /// 特效趟的输入是别的 RT（`0→RT[38]`、`1→RT[40]` …）。建树时那些 RT 还是全 0 ⇒ 探出来
    /// 只会是空白，或者把"输入为空"误读成"这趟不落笔"。第 1 帧之后它们的值是真实内容，
    /// 探针这一趟的覆盖范围才等于真实的一趟。
///
    /// # 两类趟，两套盒
    /// ① **链趟**（写 `comp / chain_a / chain_b`）：逐趟一个盒 —— 它们只往后写自己那一步，
    /// 剪紧了不影响别人。
    /// ② **遮罩烘焙趟**：同一张遮罩 RT 上的多趟是**互相覆盖**的关系（清屏已被
    /// `WE_PERF_MASK_NOCLEAR` 干掉，靠"整块覆盖"保证不留残影）⇒ 只能取**并集**，
    /// 且所有趟共用同一个矩形。并集外那圈在旧路径里每趟都写 0（RT 建时就是 0）⇒ 仍是逐字节不变。
///
    /// # 余量
    /// `WE_PERF_SCISSOR_PP_PAD`（默认 32 px）+ 1 个探针格：盖住量化误差与帧间漂移。
    /// 链趟的结果再与**整层**包围盒求交 ⇒ 逐趟盒永远 ⊆ 今天已经在用的那个盒。
    fn probe_pass_boxes(&mut self) -> Result<(), String> {
        if !perf_scissor_on() || !perf_scissor_pp_on() || self.bbox_jobs.is_empty() {
            return Ok(());
        }
        let mut chain: Vec<usize> = Vec::new();
        for (_, rts) in &self.bbox_jobs {
            chain.extend(rts.iter().copied());
        }
        chain.sort_unstable();
        chain.dedup();
        let probe = self.new_rt(256, 256, "_rt_bbox_probe_pp")?;
        let pad = perf_scissor_pp_pad();
        let dbg = std::env::var_os("WE_PERF_DEBUG").is_some();
        let mut out: Vec<Option<[i32; 4]>> = vec![None; self.passes.len()];
        let mut n_probed = 0usize;
        let mut n_tighter = 0usize;
        let mut saved_px = 0usize;

        // ── ① 链趟（逐趟） ───────────────────────────────────────────────
        for pi in 0..self.passes.len() {
            let Some(t) = self.passes[pi].target else { continue };
            if !chain.contains(&t) {
                continue;
            }
            let (rw, rh) = (self.rts[t].w, self.rts[t].h);
            let Some(f) = self.probe_one_pass(pi, probe, 8) else { continue };
            let cell = (rw.max(rh) as f32 / 256.0).ceil() as i32;
            let mut bx0 = (f[0] * rw as f32).floor() as i32 - pad - cell;
            let mut by0 = (f[1] * rh as f32).floor() as i32 - pad - cell;
            let mut bx1 = (f[2] * rw as f32).ceil() as i32 + pad + cell;
            let mut by1 = (f[3] * rh as f32).ceil() as i32 + pad + cell;
            // 与整层盒求交（逐趟盒不得比今天已经在用的那个更大）。
            if let Some([lx, ly, lw, lh]) = self.rt_bbox.get(t).copied().flatten() {
                bx0 = bx0.max(lx);
                by0 = by0.max(ly);
                bx1 = bx1.min(lx + lw);
                by1 = by1.min(ly + lh);
            }
            let (bx0, by0) = (bx0.max(0), by0.max(0));
            let (bx1, by1) = (bx1.min(rw as i32), by1.min(rh as i32));
            let (w, h) = (bx1 - bx0, by1 - by0);
            if w <= 0 || h <= 0 {
                continue;
            }
            n_probed += 1;
            // 覆盖率 > 92% ⇒ 剪了也没意义（还多一次状态切换）⇒ 不剪这一趟。
            if (w as f32 * h as f32) < 0.92 * (rw * rh) as f32 {
                out[pi] = Some([bx0, by0, w, h]);
                if let Some([_, _, lw, lh]) = self.rt_bbox.get(t).copied().flatten() {
                    if w * h < lw * lh {
                        n_tighter += 1;
                        saved_px += (lw * lh) as usize - (w * h) as usize;
                    }
                }
            }
            if dbg {
                eprintln!(
                    "[剪裁·逐趟] 趟{pi} → RT[{t}] {rw}x{rh}: 探针 {:.1}%×{:.1}% → 剪 {:?}（{:.0}% 面积）",
                    100.0 * (f[2] - f[0]),
                    100.0 * (f[3] - f[1]),
                    out[pi],
                    100.0 * (w * h) as f32 / (rw * rh) as f32
                );
            }
        }

        // ── ② 遮罩烘焙趟（同 RT 取并集） ─────────────────────────────────
        let bakes: Vec<usize> = self.mask_warp_passes.clone();
        let mut by_rt: HashMap<usize, [f32; 4]> = HashMap::new();
        for &pi in &bakes {
            let Some(t) = self.passes[pi].target else { continue };
            let Some(f) = self.probe_one_pass(pi, probe, 0) else { continue };
            let e = by_rt.entry(t).or_insert([1.0, 1.0, 0.0, 0.0]);
            e[0] = e[0].min(f[0]);
            e[1] = e[1].min(f[1]);
            e[2] = e[2].max(f[2]);
            e[3] = e[3].max(f[3]);
        }
        for (&t, f) in by_rt.iter() {
            let (rw, rh) = (self.rts[t].w, self.rts[t].h);
            let cell = (rw.max(rh) as f32 / 256.0).ceil() as i32;
            let bx0 = ((f[0] * rw as f32).floor() as i32 - pad - cell).max(0);
            let by0 = ((f[1] * rh as f32).floor() as i32 - pad - cell).max(0);
            let bx1 = ((f[2] * rw as f32).ceil() as i32 + pad + cell).min(rw as i32);
            let by1 = ((f[3] * rh as f32).ceil() as i32 + pad + cell).min(rh as i32);
            let (w, h) = (bx1 - bx0, by1 - by0);
            if w <= 0 || h <= 0 {
                continue;
            }
            if (w as f32 * h as f32) >= 0.92 * (rw * rh) as f32 {
                continue;
            }
            let mut n_on_rt = 0usize;
            for &pi in &bakes {
                if self.passes[pi].target == Some(t) {
                    out[pi] = Some([bx0, by0, w, h]);
                    n_on_rt += 1;
                }
            }
            saved_px += ((rw * rh) as i32 - w * h).max(0) as usize * n_on_rt;
            if dbg {
                eprintln!(
                    "[剪裁·逐趟] 遮罩烘焙 RT[{t}] {rw}x{rh}: 并集 {:.1}%×{:.1}% → 剪 [{}x{}]（{:.0}% 面积）",
                    100.0 * (f[2] - f[0]),
                    100.0 * (f[3] - f[1]),
                    w,
                    h,
                    100.0 * (w * h) as f32 / (rw * rh) as f32
                );
            }
        }

        self.gl.bind_target(None);
        self.gl.scissor(None);
        let n_bake = self
            .mask_warp_passes
            .iter()
            .filter(|pi| out[**pi].is_some())
            .count();
        self.pass_bbox = out;
        eprintln!(
            "[剪裁·逐趟] WE_PERF_SCISSOR_PP：链趟探了 {n_probed} 趟（{n_tighter} 趟比整层盒更紧）+ 遮罩烘焙 {n_bake} 趟，共省约 {} 万像素/帧",
            saved_px / 10_000
        );
        Ok(())
    }

    fn new_rt(&mut self, w: u32, h: u32, name: &str) -> Result<usize, String> {
        if let Some(i) = self.rt_index.get(name) {
            return Ok(*i);
        }
        let rt = self.gl.create_rt(w, h)?;
        self.rts.push(rt);
        self.rt_logical.push(None);
        self.rt_report.push(None);
        let i = self.rts.len() - 1;
        self.rt_index.insert(name.to_string(), i);
        Ok(i)
    }

    /// K1：取"屏幕拷贝"RT（画布尺寸，常驻一张）。`WE_CBM_SHADER_BLEND`（默认开）时才建/用。
    /// 名字故意用 `_rt_CBMScreenCopy`（不与 WE 的任何资源名冲突），只在开开关时才被创建。
    fn cbm_screen_rt(&mut self, w: u32, h: u32) -> Result<usize, String> {
        let name = "_rt_CBMScreenCopy";
        match self.cbm_copy_rt {
            Some(i) if self.rts.get(i).map(|r| (r.w, r.h)) == Some((w, h)) => Ok(i),
            _ => match self.rt_index.get(name).copied() {
                Some(i) if self.rts[i].w == w && self.rts[i].h == h => {
                    self.cbm_copy_rt = Some(i);
                    Ok(i)
                }
                _ => {
                    let rt = self.gl.create_rt(w, h)?;
                    self.rts.push(rt);
                    self.rt_logical.push(None);
                    let i = self.rts.len() - 1;
                    self.rt_index.insert(name.to_string(), i);
                    self.cbm_copy_rt = Some(i);
                    Ok(i)
                }
            },
        }
    }

    fn src_tex(&self, s: Src) -> (glow::Texture, u32, u32) {
        match s {
            Src::Rt(i) => {
                let r = &self.rts[i];
                (r.tex, r.w, r.h)
            }
            Src::Tex(i) => self.texs[i],
            Src::White => self.white,
        }
    }

    pub fn tile_gl_info(&self) -> (String, String) {
        (self.gl.renderer.clone(), self.gl.version.clone())
    }

    /// 渲染一帧到 CPU 缓冲（surfaceless + 回读那条老路：CLI 与静态图/视频源用）
    pub fn render_to(&mut self, canvas: &mut [u8], swizzle_bgra: bool) -> Result<(), String> {
        let (w, h) = (self.gl.w, self.gl.h);
        if canvas.len() < (w * h * 4) as usize {
            return Err(format!("目标缓冲太小：{} < {}", canvas.len(), w * h * 4));
        }
        self.draw_frame()?;
        // 0 拷贝窗口模式下**不能**走 `read_pixels`：它绑的 `self.fbo` 在窗口模式里是
        // 一个**没有附件的 1x1 占位**（`open_window` 只为"少写个 Option"而建），回读只会得到
        // 全 0 —— 这正是 `we-ctl dump` 在 0 拷贝下出空帧的根因（3/3 张 md5 相同、全通道 extrema
        // 都是 0）。`draw_frame` 刚把整帧画进窗口的**后缓冲**
        // （FBO 0；这一路**不 swap**）⇒ 立刻从 FBO 0 读回来的就是"上屏那一帧"的原始像素。
        let mut src = if self.gl.is_window() {
            self.gl.read_pixels_window()
        } else {
            self.gl.read_pixels()
        };
        readback_into(&mut src, canvas, w, h, swizzle_bgra);
        self.frames += 1;
        Ok(())
    }

    /// ── 切换壁纸的过渡 API（内核用；不调用则完全零开销）────────────────────

    /// 抓下"当前这一帧"的像素：**图像取向 RGBA**（第 0 行 = 画面顶）。
    /// 内核在丢旧渲染器**之前**调用它，把结果当过渡的"旧帧"。
    /// 窗口模式下会重画一帧再从 FBO 0 回读（**不 swap** ⇒ 屏幕上是上一帧，无可见变化）；
    /// 回读量 = 一张屏幕大小（1920×1080 ≈ 8MB / 一次性），只在切换那一刻发生。
    pub fn capture_frame(&mut self) -> Result<(u32, u32, Vec<u8>), String> {
        let (w, h) = (self.gl.w, self.gl.h);
        self.draw_frame()?;
        let mut src = if self.gl.is_window() {
            self.gl.read_pixels_window()
        } else {
            self.gl.read_pixels()
        };
        let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
        readback_into(&mut src, &mut out, w, h, false); // false ⇒ 保持 RGBA
        Ok((w, h, out))
    }

    /// 开始一次「`rgba`（旧帧）→ 新场景」的 crossfade。
    /// 语义：过渡期间每帧在**新画面上再叠一层旧帧**，透明度 α 从 1 线性降到 0
    /// （混合 = 常规半透明 ⇒ 结果 = 旧帧·α + 新画面·(1−α)）。α=1 时看起来就是旧帧
    /// ⇒ 与预热帧**无缝衔接**（中间不会有黑场）；α=0 时这一层不再画 ⇒ 与改动前逐字节相同。
    /// `duration_ms <= 0` ⇒ 不启用（老行为：预热清黑 + 硬切）。
    /// `rgba` 是**图像取向** RGBA（`capture_frame` 的输出，或 1×1 的兜底纯色）。
    pub fn start_fade(
        &mut self,
        w: u32,
        h: u32,
        rgba: Vec<u8>,
        duration_ms: f32,
    ) -> Result<(), String> {
        if duration_ms <= 0.0 {
            return Ok(());
        }
        if self.fade_layer.is_none() {
            let l = self.make_fade_layer()?;
            self.fade_layer = Some(l);
        }
        // 覆盖层自己乘 g_Alpha ⇒ 源图 alpha 不参与混合；统一置 255，
        // 免得半透明的旧帧把"新画面"透过来看起来像没淡出。
        let mut px = rgba;
        for a in px.iter_mut().skip(3).step_by(4) {
            *a = 255;
        }
        let (tex, _, _) = self.gl.make_texture_screen(&px, w, h)?;
        if let Some((old, _, _)) = self.fade_tex.take() {
            self.gl.free_texture(old);
        }
        self.fade_tex = Some((tex, w, h));
        self.fade = Some((std::time::Instant::now(), duration_ms));
        Ok(())
    }

    /// 正在过渡中吗。
    pub fn fade_active(&self) -> bool {
        self.fade.is_some()
    }

    /// 过渡覆盖层：全屏四边形 + 旧帧纹理，片元里乘 `g_Alpha`。
    /// 顶点/UV 用的是 `Gl::make_layer` 内置的那套四边形（与窗口预热的
    /// `draw_fullscreen_rgba_oneshot` **逐字节相同**）⇒ 两处朝向一致。
    fn make_fade_layer(&self) -> Result<gl_ctx::GlLayer, String> {
        let cv = "attribute vec3 a_Position;\nattribute vec2 a_TexCoord;\nvarying vec2 v_TexCoord;\nuniform mat4 g_ModelViewProjectionMatrix;\nvoid main() { gl_Position = mul(vec4(a_Position, 1.0), g_ModelViewProjectionMatrix); v_TexCoord = a_TexCoord; }\n";
        // 名字**不能**叫 `g_Alpha`：`draw_pass` 在应用完 `scalars` 之后会把
        // `g_Alpha/g_Opacity/g_UserAlpha/g_Brightness` 一律强制成 1.0（那是给 WE shader 的
        // "语义上应该是 1 的 uniform"兜底），我们的过渡透明度会被它覆盖掉（实测：α=0/0.5/1
        // 渲染出来一模一样，永远是 1）。
        let cf = "varying vec2 v_TexCoord;\nuniform sampler2D g_Texture0;\nuniform float g_FadeAlpha;\nvoid main() { vec4 c = texSample2D(g_Texture0, v_TexCoord); gl_FragColor = vec4(c.rgb, c.a * g_FadeAlpha); }\n";
        let (vs, fs) = shader::compose_stages_with(cv, cf, &[]);
        self.gl.make_layer(&vs, &fs, None)
    }

    /// 把 `tex` 按 `alpha` 叠到**屏幕**上（过渡与 `WE_FADE_TEST` 共用这一条）。
    fn draw_fade_overlay(
        &mut self,
        tex: glow::Texture,
        tw: u32,
        th: u32,
        alpha: f32,
    ) -> Result<(), String> {
        if self.fade_layer.is_none() {
            let l = self.make_fade_layer()?;
            self.fade_layer = Some(l);
        }
        let units = [gl_ctx::UnitTex::new(0, tex, tw, th)];
        let scalars = [("g_FadeAlpha".to_string(), gl_ctx::Value::F(alpha))];
        let ctx = gl_ctx::DrawCtx {
            target: None,
            mvp: Mat4::identity().column_major(),
            units: &units,
            scalars: &scalars,
            time: None,
            blend: gl_ctx::Blend::Translucent,
        };
        // 覆盖层是"最后一道"，写不写 alpha 都行；显式给全通道，别受链尾 color_mask 残留影响
        self.gl.color_mask(true, true, true, true);
        let l = self.fade_layer.as_ref().ok_or("过渡层未建")?;
        self.gl.draw_pass(l, &ctx)
    }

    /// **0 拷贝模式**：画一帧直接 `swap_buffers` 交给合成器（不读回，CPU 不碰像素）
    /// 0-copy window mode plus capture: draw once, read back the back buffer (RGBA,
    /// flipped to top-row-first), then swap. Used by `we-ctl dump` so a dumped frame
    /// costs one draw instead of two. The swap still happens after the read, so the
    /// on-screen pixels are unchanged; what we read is exactly the frame being shown.
    /// Returns the wall time spent on readback + flip (ms), i.e. the part the dump adds
    /// to the main loop. Swap time is excluded: it is paid by every frame anyway.
    pub fn render_window_with_capture(&mut self, out: &mut [u8]) -> Result<f64, String> {
        if !self.gl.is_window() {
            return Err("render_window_with_capture needs 0-copy window mode".into());
        }
        let (w, h) = (self.gl.w, self.gl.h);
        let n = (w as usize) * (h as usize) * 4;
        if out.len() < n {
            return Err(format!("buffer too small: {} < {n}", out.len()));
        }
        crate::trace::mark(crate::trace::Mark::DrawBegin);
        let drawn = self.draw_frame();
        crate::trace::mark(crate::trace::Mark::DrawEnd);
        drawn?;
        let t_cap = std::time::Instant::now();
        self.gl.read_pixels_window_into(&mut out[..n]);
        flip_rows_in_place(&mut out[..n], w, h);
        let cap_ms = t_cap.elapsed().as_secs_f64() * 1000.0;
        self.gl.swap()?;
        self.frames += 1;
        Ok(cap_ms)
    }

    /// Bounded variant of `render_window_with_capture` .
    /// Draws once, reads the back buffer into `out` (top row first), then hands the submit to
    /// the `we-commit` thread and waits at most `deadline_ms`. `Pending` means the pixels in
    /// `out` are this frame and only the present is late: the caller keeps the dump frame and
    /// must not draw again until `commit_pending` goes false.
    pub fn render_window_with_capture_bounded(
        &mut self,
        out: &mut [u8],
        deadline_ms: u64,
    ) -> Result<(f64, FrameCommit), String> {
        if !self.gl.is_window() {
            return Err("render_window_with_capture_bounded needs 0-copy window mode".into());
        }
        if self.gl.commit_pending() {
            return Err("commit still pending: draw skipped".into());
        }
        let (w, h) = (self.gl.w, self.gl.h);
        let n = (w as usize) * (h as usize) * 4;
        if out.len() < n {
            return Err(format!("buffer too small: {} < {n}", out.len()));
        }
        crate::trace::mark(crate::trace::Mark::DrawBegin);
        let drawn = self.draw_frame();
        crate::trace::mark(crate::trace::Mark::DrawEnd);
        drawn?;
        let t_cap = std::time::Instant::now();
        self.gl.read_pixels_window_into(&mut out[..n]);
        flip_rows_in_place(&mut out[..n], w, h);
        let cap_ms = t_cap.elapsed().as_secs_f64() * 1000.0;
        match self.gl.swap_bounded(deadline_ms) {
            gl_ctx::CommitOutcome::Done(Ok(())) => {
                self.frames += 1;
                Ok((cap_ms, FrameCommit::Presented))
            }
            gl_ctx::CommitOutcome::Done(Err(e)) => Err(e),
            _ => Ok((cap_ms, FrameCommit::Pending)),
        }
    }

    /// Is a submit from an earlier frame still in flight? (Callers must not draw while it is:
    /// the EGL context lives on the commit thread until that swap returns.)
    pub fn commit_pending(&self) -> bool {
        self.gl.commit_pending()
    }

    /// Collect a late submit (None = still running). Late failures surface here.
    pub fn poll_commit(&self) -> Option<Result<(), String>> {
        self.gl.poll_commit()
    }

    /// Wall time of the last collected swap in ms (0 = none collected yet).
    pub fn last_swap_ms(&self) -> u64 {
        self.gl.last_swap_ms()
    }

    /// **Bounded commit** : draw one frame, then hand the swap to the
    /// `we-commit` thread and wait at most `deadline_ms` for it on this thread.
    /// Why: measured, not guessed - freezing the compositor 2.5 s shows the whole stall inside
    /// `eglSwapBuffers` (swap 2551.5 ms, everything else normal). That call has no timeout, so
    /// a compositor that reconfigures its output and never sends the frame callback used to
    /// park the main loop until the watchdog restarted the process.
    /// `Pending` means "the frame was drawn, the submit is still out there": the caller must
    /// keep running its event loop and simply not draw again until this stops returning
    /// `Pending`. Pixels of a presented frame are the same as the synchronous path.
    pub fn render_window_bounded(&mut self, deadline_ms: u64) -> Result<FrameCommit, String> {
        if !self.gl.is_window() {
            return Err("render_window_bounded 需要 0 拷贝窗口模式（用 open_window 建）".into());
        }
        // A job from an earlier lap still running: nothing to draw this lap.
        if self.gl.commit_pending() {
            return Ok(FrameCommit::Pending);
        }
        // ... or it finished late: surface its failure here (success needs no action).
        if let Some(Err(e)) = self.gl.poll_commit() {
            return Err(e);
        }
        crate::trace::mark(crate::trace::Mark::DrawBegin);
        let drawn = self.draw_frame();
        crate::trace::mark(crate::trace::Mark::DrawEnd);
        drawn?;
        self.frames += 1;
        match self.gl.swap_bounded(deadline_ms) {
            gl_ctx::CommitOutcome::Done(Ok(())) => Ok(FrameCommit::Presented),
            gl_ctx::CommitOutcome::Done(Err(e)) => Err(e),
            _ => Ok(FrameCommit::Pending),
        }
    }

    pub fn render_window(&mut self) -> Result<(), String> {
        if !self.gl.is_window() {
            return Err("render_window 需要 0 拷贝窗口模式（用 open_window 建）".into());
        }
        // 慢圈归因：这两个点之间就是“真正画一帧”的 GPU 工作，与交帧分开量。关着时零开销。
        crate::trace::mark(crate::trace::Mark::DrawBegin);
        let drawn = self.draw_frame();
        crate::trace::mark(crate::trace::Mark::DrawEnd);
        drawn?;
        self.gl.swap()?;
        self.frames += 1;
        Ok(())
    }

    /// 本帧的灯 uniform（`g_LightsPosition[4]` 等的打包）。
    /// 打包顺序与语义**逐条照** OWE `Scene/Pkg/Parse/Uniform/UniformSource.cpp:848-925`
    /// （`LightUniformSource::Evaluate`）：
///
    /// * 固定 **4 个槽**；`position` = 4×`vec3`（每个灯 x,y,z 连续）、`color_radius` = 4×`vec4`
    /// （`rgb = color·intensity`、`a = radius`）、`direction_type` = 4×`vec4`
    /// （`xyz` 朝向、`w` 类型；**用不到的槽 `w = -1`**，`UniformSource.cpp:856-857`）、
    /// `cone_exponent` = 4×`vec4`（`(inner_cone_cos, outer_cone_cos, exponent, 0)`）、
    /// `cast_shadow` = 4×`float`、`color_premultiplied` = **前 3 盏** ×`vec4`
    /// （`rgb = color·intensity·radius²`，`UniformSource.cpp:914-922`）。
///
    /// * `color` 里的 `intensity`：OWE `Lighting.cppm:50` `color = m_desc.color * m_desc.intensity`。
///
    /// * 灯的**世界位置** = 「`origin` 经官方动画曲线求值」（OWE `Scene.cpp:969-995`）。
///
    /// * 非 Directional 的灯本地朝向取 `-X`、Spot 取 `+X`，再乘灯的旋转
    /// （`UniformSource.cpp:886-890`）。本机唯一一盏是 `lpoint` ⇒ 这一项对画面无影响
    /// （`PerformLighting_V1` 只在 Directional（`type > 1.5`）时才看 `.xyz`）。
    fn light_uniforms(&self, time: f32) -> gl_ctx::LightUniforms {
        let mut u = gl_ctx::LightUniforms::default();
        u.ambient = self.light_ambient;
        u.skylight = self.light_skylight;
        let mut used = 0usize;
        for (i, l) in self.light_srcs.iter().take(4).enumerate() {
            let pos = match &l.origin_anim {
                Some(c) => c.eval_vec3(l.origin, time),
                None => l.origin,
            };
            let radius = match &l.radius_anim {
                Some(c) => c.eval_scalar(l.radius, time),
                None => l.radius,
            };
            let rgb = [
                l.color[0] * l.intensity,
                l.color[1] * l.intensity,
                l.color[2] * l.intensity,
            ];
            u.position[i * 3..i * 3 + 3].copy_from_slice(&pos);
            u.color_radius[i * 4] = rgb[0];
            u.color_radius[i * 4 + 1] = rgb[1];
            u.color_radius[i * 4 + 2] = rgb[2];
            u.color_radius[i * 4 + 3] = radius;
            let d = rot3(l.angles, l.kind.local_direction());
            u.direction_type[i * 4] = d[0];
            u.direction_type[i * 4 + 1] = d[1];
            u.direction_type[i * 4 + 2] = d[2];
            u.direction_type[i * 4 + 3] = l.kind.value();
            u.cone_exponent[i * 4] = l.inner_cone_cos;
            u.cone_exponent[i * 4 + 1] = l.outer_cone_cos;
            u.cone_exponent[i * 4 + 2] = l.exponent;
            u.cast_shadow[i] = if l.cast_shadow { 1.0 } else { 0.0 };
            if i < 3 {
                let p = radius * radius;
                u.color_premultiplied[i * 4] = rgb[0] * p;
                u.color_premultiplied[i * 4 + 1] = rgb[1] * p;
                u.color_premultiplied[i * 4 + 2] = rgb[2] * p;
            }
            used += 1;
        }
        u.finish(used);
        u
    }

    /// 跑完整条趟链：清屏 → 逐趟 → 粒子。**不含**回读、**不含** swap。
    fn draw_frame(&mut self) -> Result<(), String> {
        // the commit thread takes the EGL context for the duration of
        // eglSwapBuffers, so re-bind it here before any GL call (no-op when it is ours).
        self.gl.ensure_current();
        // 控制面记账的"文字层换色"在这里落地 —— **必须在 `ensure_current` 之后**：
        // 这一行以前写在它前面，实测屏幕一点没变（没有 current context 的 glTexImage2D
        // 会被静默丢掉），这正是"离线 A/B 逐字节相同、屏上却看不出变化"的原因。
        self.apply_pending_text_colors();
        let prof_total_t0 = profcls::frame_begin(&self.gl);
        let ft_c0 = if ftime::on() { cpu_thread_us() } else { 0 };
        // 屏幕清成不透明黑；**中间 RT 清成透明**（alpha=0）。
        // ⚠ 这里以前 RT 也清成 (0,0,0,1) —— 后果很隐蔽但很严重：某个图层的合成缓冲是"空的"（比如
        // 它整块在屏幕外、或贴图没内容）时，链尾那趟"贴回屏幕"是**半透明混合**，src_alpha=1 的黑
        // 会把已有画面整个涂掉。实测 `2482408967`（Berserk：主画面正常画进了中间缓冲，但后面那个
        // 屏幕外的 Clock 文字层把屏幕刷成不透明黑 ⇒ 整条壁纸越来越黑到全黑）。
        // WE 自己的缓冲就是透明起步的，所以这里对齐 WE。
        let sp_clear = ftime::Span::new(&ftime::CLEAR);
        self.gl.bind_target(None);
        // 屏幕用**场景自己的 clearcolor**（WE/linux-we 同语义；见 `clear_rgb` 字段）。
        self.gl.clear(
            self.clear_rgb[0],
            self.clear_rgb[1],
            self.clear_rgb[2],
            1.0,
        );
        // WE_PERF_STATIC_CACHE: a frozen RT keeps exactly the content that the judgement
        // proved will be reproduced this frame, so skip the clear (sig != 0 = drawn once).
        for (k, rt) in self.rts.iter().enumerate() {
            let keep = self.static_cache.as_ref().map_or(false, |c| {
                c.live
                    && c.frozen.get(k).copied().unwrap_or(false)
                    && c.sig.get(k).copied().unwrap_or(0) != 0
            });
            if keep {
                continue;
            }
            self.gl.bind_target(Some(rt));
            if profcls::skip_rt_clear() { continue; }
            self.gl.clear(0.0, 0.0, 0.0, 0.0);
        }
        // GL 的错误标志是**粘住**的：只要没人读过，它就一直挂在那儿，害得后面
        // 第一趟 `bind_target` 之后一读就"报错"，让人以为是那一趟的错 ✗。
        // 清屏阶段就把残留读掉并打出来，才能分清"残留 vs 本趟真错"。
        // 限流：这种错冒出来往往是**每帧一条**（30fps = 一小时 10 万条），既刷满 journal，
        // 又把真正有用的日志埋掉。第一次必打，之后每 600 帧（≈20 秒 @30fps）报一次累计值；
        // 一旦干净就复位，下次再冒出来能立刻看到。
        // （0x506 = GL_INVALID_FRAMEBUFFER_OPERATION：先看 clear/bind 的目标绑对没有 ——
        // 2026-10 就是这里硬绑了一个没附件的占位 FBO，每帧 30 条刷了几小时。）
        {
            use std::sync::atomic::{AtomicU64, Ordering};
            static RESIDUAL_FRAMES: AtomicU64 = AtomicU64::new(0);
            let pending = self.gl.get_error_inner();
            if pending != glow::NO_ERROR {
                let n = RESIDUAL_FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
                if n == 1 || n % 600 == 0 {
                    eprintln!(
                        "[FBO] 清屏阶段的残留 GL 错误：0x{pending:x}（累计 {n} 帧 / 每帧一条；0x506=FBO 不完整 ⇒ 先查 clear 与 bind 的是不是同一个目标）"
                    );
                }
            } else {
                RESIDUAL_FRAMES.store(0, Ordering::Relaxed);
            }
        }
        drop(sp_clear);

        let mut units: Vec<gl_ctx::UnitTex> = Vec::new();
        let dbg_fbo = std::env::var("WE_DEBUG_FBO").is_ok();
        if dbg_fbo {
            let fb0 = self.gl.draw_fb_binding();
            let st0 = self.gl.check_fb_status();
            self.gl.force_window_fb();
            let fb1 = self.gl.draw_fb_binding();
            let st1 = self.gl.check_fb_status();
            eprintln!(
                "[FBO] is_window={} 绑前 fb={fb0} 状态=0x{st0:x}  ⇒  强制绑 0 后 fb={fb1} 状态=0x{st1:x}（0x8cd5 才完整）",
                self.gl.is_window()
            );
        }
        // 摆锤（A 路线：程序化骨骼动画）：每帧对启用了摆锤的网格图层做 CPU 形变并重传顶点。
        // 网格很小（本场景最大 2058 顶点）⇒ 每帧重传毫无压力。必须用 `sway_positions`
        // 从**原始**顶点算（内部以 `mesh_pos` 为基准，不会逐帧累积）。
        let sp_mesh = ftime::Span::new(&ftime::MESH);
        {
            let time = self.time;
            // 网格动画的时间可以用 `WE_ANIM_TIME` / `set_mesh_time` **单独覆盖**：
            // 场景时钟停在 0（特效/粒子都不动）而人物按 t 动 ⇒ 差分只剩"人物在动"这一件事。
            let t_mesh = self
                .mesh_time
                .or_else(|| {
                    std::env::var("WE_ANIM_TIME")
                        .ok()
                        .and_then(|s| s.trim().parse::<f32>().ok())
                })
                .unwrap_or(time);
            let sway_dbg = std::env::var_os("WE_SWAY_DEBUG").is_some();
            let mut tick: u64 = 0;
            for (mpi, p) in self.passes.iter_mut().enumerate() {
                if p.layer.sway_enabled() {
                    let deformed = gl_ctx::sway_positions(&p.layer, time);
                    if sway_dbg && !deformed.is_empty() {
                        tick += 1;
                        if tick % 20 == 1 {
                            let (mut lo, mut hi) = (f32::MAX, f32::MIN);
                            for q in &deformed {
                                lo = lo.min(q[0]);
                                hi = hi.max(q[0]);
                            }
                            eprintln!("[摆锤帧] t={time:.3}s 顶点={} x范围=[{lo:.4},{hi:.4}]", deformed.len());
                        }
                    }
                    if !deformed.is_empty() {
                        self.gl.update_layer_mesh(&p.layer, &deformed)?;
                    }
                }
                // 网格**时间动画**：按本帧时间重算装配顶点并重传 VBO。
                // 时间用 `self.time`（= 本帧开始前的时间，与下发 `g_Time` 的那只钟同源；
                // 内核走墙钟、CLI 走固定步长/`--t <秒>`）⇒ `--t 0.5` 单帧渲染精确落在 0.5 s。
                // `t = 0` 时 `positions_at(0)` 与建层时的几何逐位相同（同一函数 + 精确帧零算术）。
                if let Some(ma) = p.mesh_anim.as_mut() {
                    // 逐帧推进 `animationlayers[].blend` 的脚本。
                    // 静音（频谱全 0）⇒ 脚本返回 `value × minvalue`，与建层时
                    // `AnimationLayer::blend_weight` 的静默端**逐位相同** ⇒ 打开也不动画面；
                    // 有频谱时按 `value × (band × (max−min) + min)` 变（作者原脚本口径）。
                    // 默认（`WE_SCRIPT_AUDIO` / `WE_BLEND_LIVE` 未设）⇒ `blend_scripts` 空 ⇒
                    // 一次求值都不做，逐字节不变。
                    let mut forced = false;
                    if !ma.blend_scripts.is_empty() {
                        let spec = crate::audio::spectrum();
                        let now_m = match self.fixed_dt {
                            Some(d) => self.time + d,
                            None => self.time_base + self.t0.elapsed().as_secs_f32(),
                        };
                        let dt_m = (now_m - self.time).clamp(0.0, 0.25);
                        for (li, sc) in ma.blend_scripts.iter_mut() {
                            let v = sc.update(dt_m, now_m, &spec) * blend_scale();
                            ma.pose.set_layer_blend(*li, v);
                            if std::env::var("WE_SCRIPT_DEBUG").is_ok() {
                                eprintln!(
                                    "[脚本] blend {} 层 {li} ← {v:.4}（t={now_m:.2}s）",
                                    ma.name
                                );
                            }
                        }
                        // 权重变了 ⇒ 即便 `t` 没动也得重算装配（`--t` 单帧的口径）。
                        forced = true;
                    }
                    if ma.last_t != t_mesh || forced {
                        let positions = ma.positions_at(t_mesh);
                        self.gl.update_layer_mesh(&p.layer, &positions)?;
                        ma.last_t = t_mesh;
                        // 这是某一层遮罩烘焙组的**底图趟** ⇒ 把同一份装配顶点留着，
                        // 循环结束后喂给该组的烘焙趟（同一份蒙皮，不重复装配）。
                        if self.mask_warp_groups.iter().any(|(b, _)| *b == mpi) {
                            self.mask_warp_pos = Some(positions);
                        }
                    }
                }
            }
            // 把底图趟刚算出的装配顶点**同一份**传给遮罩烘焙趟（每帧一次）。
            if let Some(pos) = self.mask_warp_pos.take() {
                if std::env::var_os("WE_DEBUG_LAYERS").is_some() {
                    eprintln!(
                        "[遮罩烘焙] 本帧重传顶点 {} 个 → 烘焙趟 {:?}（组={:?}）",
                        pos.len(),
                        self.mask_warp_passes,
                        self.mask_warp_groups
                    );
                }
                for (_, warps) in self.mask_warp_groups.iter() {
                    for &wi in warps {
                        if let Some(p) = self.passes.get(wi) {
                            self.gl.update_layer_mesh(&p.layer, &pos)?;
                        }
                    }
                }
            }
            // ── 挂骨骼（`attachment`）：锚点**逐帧**求值─────────────────
            // OWE `Puppet.cpp:496-506` + `SceneFinalize.cpp:358-390`：父层会动时**逐帧**
            // `SetLocalFrame(anchor(t) · local_base)`，`anchor(t) = genFrame(t)[骨] · bind_xform`。
            // 我们建层时烘的是 `M0 = W_parent·anchor(0)·local_raw` ⇒ 这里每帧把登记过的
            // \"摆放趟\" 更新成 `mvp(t) = (Q_L·D(t)·Q_L⁻¹)·mvp(0)`（推导见 `AttachAnim`）。
            // `t` 与网格动画同一个时刻（`t_mesh`：`--t`/`WE_ANIM_TIME` 都能单独定它）。
            // 默认关（`WE_ATTACHMENT` 未设 / `WE_ATTACH_BAKE=1`）时 `attach_passes` 恒空
            // ⇒ 这段零开销、零行为变化。
            if self.frames == 0 && attach_dbg() {
                eprintln!(
                    "[挂接] 逐帧登记 {} 趟（WE_ATTACHMENT=1；{}）",
                    self.attach_passes.len(),
                    if attach_bake() {
                        "WE_ATTACH_BAKE=1 ⇒ 应为 0 趟（复现\"烘 t=0\"）"
                    } else {
                        "默认逐帧"
                    }
                );
            }
            for k in 0..self.attach_passes.len() {
                let idx = self.attach_passes[k].pass_idx;
                if idx >= self.passes.len() || self.attach_passes[k].anim.last_t == t_mesh {
                    continue;
                }
                let w0 = self.attach_passes[k].anim.world0;
                let truth = self.attach_passes[k]
                    .anim
                    .world_t_at(t_mesh)
                    .unwrap_or([f32::NAN; 3]);
                let probe = self.attach_passes[k].anim.delta_probe(t_mesh);
                let d = [
                    (truth[0] - w0[0]).abs(),
                    (truth[1] - w0[1]).abs(),
                    (truth[2] - w0[2]).abs(),
                ];
                // `t = 0` 时**建层值就是真值**（`D(0) = 单位阵`）⇒ 不写矩阵（省掉共轭那两步乘法的
                // 浮点残差，也让 `WE_ATTACHMENT=1 --t 0` 与上次的输出**逐字节相同**）。
                let mut applied = true;
                if t_mesh == 0.0 {
                    applied = false;
                } else if let Some(m) = self.attach_passes[k].anim.m_place_at(t_mesh) {
                    self.passes[idx].mvp = m.transposed().column_major();
                } else {
                    continue;
                }
                if attach_dbg() {
                    // 场景单位 → 输出像素：与 `WE_ANIM_PROBE` 同一换算（out_w / 画布宽）
                    let px = self.gl.w as f32 / self.scene_size.0.max(1.0);
                    let dd = (d[0] * d[0] + d[1] * d[1]).sqrt();
                    let (e_inv, e_conj) = match probe {
                        Some((_, a, b)) => (a, b),
                        None => (f32::NAN, f32::NAN),
                    };
                    eprintln!(
                        "[挂接t] 趟{idx} attachment={:?} 骨={} t={t_mesh:.3}s 对象世界 T: 烘t0=({:.2},{:.2}) \
                         逐帧真值=({:.2},{:.2}) 修复前误差=({:.2},{:.2}) |Δ|={dd:.2} 场景单位 = {:.2}px@{}；\
                         逆阵残差 max|Q_L·Q_L⁻¹−I|={e_inv:.2e} 共轭残差 max|Q_L⁻¹·(Q_L·D·Q_L⁻¹)·Q_L−D|={e_conj:.2e}；{}",
                        self.attach_passes[k].anim.att.name,
                        self.attach_passes[k].anim.att.bone_index,
                        w0[0],
                        w0[1],
                        truth[0],
                        truth[1],
                        d[0],
                        d[1],
                        dd * px,
                        self.gl.w,
                        if applied {
                            "已按逐帧写趟 mvp"
                        } else {
                            "t=0 ⇒ 沿用建层值（与烘 t=0 逐位相同）"
                        }
                    );
                }
                self.attach_passes[k].anim.last_t = t_mesh;
            }
        }
        drop(sp_mesh);
        // ── 网格动画探针（`WE_ANIM_PROBE=0,0.5,1.0`，只在第一帧打一次）────────────────
        // 逐部件给出"相对 t=0 的逐顶点装配位移"的 mean/max：网格单位（= 场景单位）与
        // 输出像素（按 out_w / 画布宽换算）。这是"腿在动"的可观测量，不靠肉眼看像素。
        if self.frames == 0 {
            if let Ok(spec) = std::env::var("WE_ANIM_PROBE") {
                let times: Vec<f32> = spec
                    .split(',')
                    .filter_map(|s| s.trim().parse::<f32>().ok())
                    .collect();
                let px = self.gl.w as f32 / self.scene_size.0.max(1.0);
                for p in self.passes.iter() {
                    let Some(ma) = p.mesh_anim.as_ref() else { continue };
                    let base = ma.pose.assemble(&ma.bind, &ma.bi, &ma.bw, 0.0);
                    eprintln!(
                        "[动画探针] {} 顶点={} 骨={} 动画层={:?}",
                        ma.name,
                        ma.bind.len(),
                        ma.pose.bones(),
                        ma.pose.layer_ids()
                    );
                    for &t in &times {
                        let cur = ma.pose.assemble(&ma.bind, &ma.bi, &ma.bw, t);
                        let (mut sum, mut mx, mut n) = (0.0f64, 0.0f32, 0usize);
                        for (a, b) in base.iter().zip(cur.iter()) {
                            let d = ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt();
                            sum += d as f64;
                            mx = mx.max(d);
                            n += 1;
                        }
                        let mean = if n > 0 { sum / n as f64 } else { 0.0 };
                        eprintln!(
                            "             t={t:.3}s Δ顶点 mean={mean:.3} max={mx:.3} 网格单位（≈{:.2}/{:.2} px @{} 宽）",
                            mean as f32 * px,
                            mx * px,
                            self.gl.w
                        );
                        // （`3571777169` 发丝被抽成长细黑线）：只给 mean/max 看不出
                        // 「是不是只有少数顶点飞了」。这里把**位移分布的尾部分位**与**最大的
                        // 若干个顶点**（连同绑定位置 / 绑骨索引 / 权重 / 装配后位置）逐条打出来 ⇒
                        // 一眼判「平滑形变」还是「少数顶点被抽飞」。
                        let mut ds: Vec<(f32, usize)> = (0..base.len().min(cur.len()))
                            .map(|i| {
                                let (a, b) = (base[i], cur[i]);
                                (
                                    ((a[0] - b[0]).powi(2)
                                        + (a[1] - b[1]).powi(2)
                                        + (a[2] - b[2]).powi(2))
                                    .sqrt(),
                                    i,
                                )
                            })
                            .collect();
                        ds.sort_by(|x, y| y.0.partial_cmp(&x.0).unwrap_or(std::cmp::Ordering::Equal));
                        // ds 降序 ⇒ ds[i] 是第 i 大的值；分位 = ds[floor((1−p)·(n−1))]。
                        let q = |p: f64| -> f32 {
                            if ds.is_empty() {
                                0.0
                            } else {
                                ds[(((ds.len() - 1) as f64) * (1.0 - p)).floor() as usize].0
                            }
                        };
                        let cnt = |t: f32| ds.iter().filter(|d| d.0 > t).count();
                        eprintln!(
                            "             位移分位 p50={:.3} p90={:.3} p99={:.3} p99.9={:.3} max={:.3}；尾部计数 >100={} >50={} >30={}（顶点 n={}）",
                            q(0.50),
                            q(0.90),
                            q(0.99),
                            q(0.999),
                            ds.first().map(|d| d.0).unwrap_or(0.0),
                            cnt(100.0),
                            cnt(50.0),
                            cnt(30.0),
                            ds.len()
                        );
                        let top_n: usize = std::env::var("WE_ANIM_PROBE_TOP")
                            .ok()
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0);
                        for &(d, i) in ds.iter().take(top_n) {
                            eprintln!(
                                "             #{i:<6} d={d:8.2} bind=({:8.2},{:8.2},{:8.2}) asm=({:8.2},{:8.2},{:8.2}) bi={:?} bw={:?}",
                                base[i][0],
                                base[i][1],
                                base[i][2],
                                cur[i][0],
                                cur[i][1],
                                cur[i][2],
                                ma.bi.get(i).copied().unwrap_or([0; 4]),
                                ma.bw.get(i).copied().unwrap_or([0.0; 4])
                            );
                        }
                    }
                }
            }
        }
        let sp_passes = ftime::Span::new(&ftime::PASSES);
        // 省电档：只跑底图那一刻（passes[0] 就是底图，目标=None ✓），特效与粒子都跳过
        let npasses = if self.effects_on {
            self.passes.len()
        } else {
            self.passes.len().min(1)
        };
        // 场景级 RT 每帧必须先清。以前逐层直接画屏幕（屏幕每帧被清），换成场景 RT 后
        // 忘了清它 ⇒ 第一层若是半透明（Ice Princess 的 `фон мин`）就会把残留混出来 ⇒ 整幅发软。
        if let Some(sr) = self.scene_rt {
            if let Some(rt) = self.rts.get(sr) {
                // 必须清成【不透明】黑：窗口帧缓冲是不透明的，清成 alpha=0 会让
                // 半透明的首层（Ice Princess 的 `фон мин`）混到"透明底"上被整个洗掉
                // 实测表现为背景丢失（顶部纯黑 + 一片白雾）。
                self.gl.clear_rt(rt, [0.0, 0.0, 0.0, 1.0]);
            }
        }
        // ── 粒子：**先推进模拟、逐系统建顶点**（必须在趟循环**之前** —— 它们要按对象次序插进趟里）──
        // 时间：这里只**算出** `now`/`dt`，**先不赋给 `self.time`** ⇒ 趟循环里的 `g_Time`
        // 仍是"这一帧更新之前"的值（与改动前逐字节一致）；`self.time` 在趟循环之后再赋
        // （后面的 glow 那几趟照旧看到新时间，与改动前相同）。
        // **按对象次序画粒子**（WE 的语义：粒子就是普通场景对象，跟着
        // `objects` 次序渲进场景 FBO ⇒ 排在它后面的图层会挡住它）。
        // `WE_PARTICLES_LAST=1` 回到旧行为（全部粒子画在最后、叠在所有图层之上）做 A/B。
        let sp_part_pre = ftime::Span::new(&ftime::PART);
        // 场景时间 = 墙钟（WE 的 `g_Time` 语义）。别再写死 1/30 —— 内核跑 20fps 会让动画快 1.5 倍。
        let now = match self.fixed_dt {
            Some(d) => self.time + d,
            None => self.time_base + self.t0.elapsed().as_secs_f32(),
        };
        let dt = (now - self.time).clamp(0.0, 0.25); // 粒子步长用同一只钟（卡帧时夹住，别一步跳太远）
        // 逐帧推进「特效趟的脚本标量」（见 `fx_scripts` 字段注释）。
        // 默认（`WE_SCRIPT_AUDIO` 未设 / 场景没脚本常量）⇒ `fx_scripts` 空 ⇒ 只清一个空 Vec，
        // 一趟都不求值、逐字节不变。
        self.fx_frame_scalars.clear();
        if !self.fx_scripts.is_empty() {
            let spec = crate::audio::spectrum();
            for (pi, scripts) in self.fx_scripts.iter_mut() {
                let mut vals: Vec<(String, Value)> = Vec::with_capacity(scripts.len());
                for (name, sc) in scripts.iter_mut() {
                    let v = sc.update(dt, now, &spec);
                    if std::env::var("WE_SCRIPT_DEBUG").is_ok() {
                        eprintln!("[脚本] 特效趟 {pi} 常量 {name} ← {v:.4}（t={now:.2}s）");
                    }
                    vals.push((name.clone(), Value::F(v)));
                }
                self.fx_frame_scalars.push((*pi, vals));
            }
        }
        let proj_mvp = self.proj_mvp;
        // ── 场景灯：按**本帧趟循环用的时间**（`self.time`，与 `g_Time` 同源，见上面 7480 的注释）
        // 算好后一次性装进 GL 上下文，`draw_pass` 里再按 shader 声明的名字上传。
        // 没有灯对象且两个环境色都是 0 ⇒ `None`（一个 uniform 都不碰，逐字节不变）。
        {
            let li = self.light_uniforms(self.time);
            // 逐值对照用（`WE_DEBUG_LIGHTS=1`）：把**真正要上传的那组数**打出来，
            // 与官方语义逐字段核对（见 `light_uniforms` 的出处清单）。只打前两帧。
            if std::env::var_os("WE_DEBUG_LIGHTS").is_some() && self.frames < 2 {
                eprintln!(
                    "[灯] t={:.3}s 上传值\n  position(4*3)={:?}\n  color_radius(4*4)={:?}\n  dir_type(4*4)={:?}\n  cone(4*4)={:?}\n  shadow(4)={:?}\n  premultiplied(3*4)={:?}\n  ambient={:?} skylight={:?}",
                    self.time,
                    li.position,
                    li.color_radius,
                    li.direction_type,
                    li.cone_exponent,
                    li.cast_shadow,
                    li.color_premultiplied,
                    li.ambient,
                    li.skylight
                );
            }
            let any = !self.light_srcs.is_empty()
                || li.ambient != [0.0, 0.0, 0.0]
                || li.skylight != [0.0, 0.0, 0.0];
            self.gl.set_lights(if any { Some(li) } else { None });
        }
        // 粒子画进**最终那张 RT**（None = 屏幕/窗口默认帧缓冲）
        // `glow_pre` 这条路：粒子必须画进**场景 RT**（glow 的源与 present 都读它），
        // 而 present 已推迟到后面 ⇒ 不能再用"passes 最后一趟的目标"推。
        let glow_pre = self.glow.on
            && self.glow.order_pre
            && self.scene_rt.is_some()
            && self.pending_present.is_some();
        let final_rt = if glow_pre {
            self.scene_rt
        } else {
            self.passes.last().and_then(|p| p.target)
        };
        let dbg = std::env::var("WE_DEBUG_PARTICLES").is_ok();
        // `WE_DEBUG_BOIDS=1`：逐帧打 boids 的可测判据（见下面 `boids_stats` 调用处的注释）。
        // 默认关 ⇒ 只有一个 `env::var` 的成本，物理与像素都不动。
        let boids_dbg = std::env::var("WE_DEBUG_BOIDS").is_ok();
        let boids_dump = std::env::var("WE_DEBUG_BOIDS_DUMP").is_ok();
        let bo_every: i64 = std::env::var("WE_DEBUG_BOIDS_EVERY")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(30);
        // 诊断 `WE_PARTICLE_GEOM=1`（默认关 ⇒ 逐字节不变）：把每颗粒子**四角**按
        // **本趟真实上传的 MVP**（= `proj_mvp`）投到 PNG 像素坐标打印，用来做
        // 「粒子实际落点 == 配置推出的落点」这条几何判据。判据只用**屏幕像素**，
        // 与 ±26 luma 的参照不确定度无关。
        // 顶点布局（`particles.rs::build` + WE 官方 `genericparticle.vert`）：
        // [0..3]=位置(场景), [3..5]=(u,v), [5]=rot.z, [6]=size, ...
        // `ComputeParticlePosition`：pos = a_Position + size·right·(u-0.5) − size·up·(v-0.5)·textureRatio
        // 本趟 `g_OrientationRight/Up` 由 `gl_ctx` 固定成屏幕 X/Y ⇒ right=(1,0,0)、up=(0,1,0)。
        let geom_dbg = std::env::var("WE_PARTICLE_GEOM").is_ok();
        let geom_filt = std::env::var("WE_PARTICLE_GEOM_FILTER").unwrap_or_default();
        // A/B 实验用：关掉粒子绘制，其余完全相同
        let particles_off = std::env::var("WE_NO_PARTICLES").is_ok() || !self.effects_on;
        // 诊断 `WE_PARTICLE_PROBE=<文件>`：逐帧逐系统把聚合量追加一行（默认不设 ⇒ 零行为）。
        // 慢帧丢时间的判据就是拿这些行做数值对比。
        let probe_file = std::env::var("WE_PARTICLE_PROBE").ok().filter(|s| !s.is_empty());
                let interleave = self.scene_rt.is_some()
            && std::env::var("WE_PARTICLES_LAST").ok().as_deref() != Some("1");
        // 每个系统的顶点（顺序与 `self.particle_systems` 一致）；插序点跟着一起取出来。
        let mut pvbuf: Vec<Vec<[f32; gl_ctx::PARTICLE_FLOATS]>> =
            Vec::with_capacity(self.particle_systems.len());
        for inst in self.particle_systems.iter_mut() {
            // 场景属性脚本驱动的发射率（`WE_SCRIPT_AUDIO=1`；默认关时 `rate_script` 为 None
            // ⇒ 一行都不执行）。脚本读的是**本帧**的 16 段频谱（`audio::spectrum`，与
            // `sys.update` 内部取的是同一只钟/同一份快照）。
            if let Some(sc) = inst.rate_script.as_mut() {
                let v = sc.update(dt, now, &crate::audio::spectrum());
                inst.sys.set_rate_mul(v);
                if std::env::var("WE_SCRIPT_DEBUG").is_ok() {
                    eprintln!("[脚本] 「{}」t={now:.2}s rate←{v:.4}", inst.name);
                }
            }
            inst.sys.update(dt);
            if let Some(pf) = probe_file.as_ref() {
                use std::io::Write;
                if let Ok(mut fh) = std::fs::OpenOptions::new().create(true).append(true).open(pf) {
                    let _ = writeln!(fh, "{}\tframe={}", inst.sys.probe_line(&inst.name), self.frames);
                }
            }
            // 诊断 `WE_DEBUG_BOIDS=1`（默认关 ⇒ 一行不执行、逐字节不变）：
            // 逐帧打这层 `boids` 的**可测判据** —— 存活数 / 有邻居的粒子数 / 两两距离分布 /
            // 速度统计 / 同向度 / 相对质心半径。用来回答"某条真实场景里 boids 为什么 0 变化"：
            // 邻居数恒 0 ⇒ 是**数据**（半径内根本没邻居），不是实现。
            // `WE_DEBUG_BOIDS_FILTER=<子串>` 只打名字含该子串的层；
            // `WE_DEBUG_BOIDS_EVERY=<n>` 每 n 帧打一行（0 = 每帧都打；有邻居的帧总是打）。
            if boids_dbg {
                let filt = std::env::var("WE_DEBUG_BOIDS_FILTER").unwrap_or_default();
                if filt.is_empty() || inst.name.contains(&filt) {
                    if let Some(s) = inst.sys.boids_stats() {
                        let tick = (now * 30.0).round() as i64;
                        if bo_every <= 0 || tick.rem_euclid(bo_every) == 0 || s.with_neighbour > 0 {
                            let (xo, xs) = inst.sys.xform();
                            eprintln!(
                                "[boids] 「{}」t={now:.2}s 存活 {}  有邻居 {}（平均 {:5.2} / 最多 {}）  有分离 {}  \
                                 两两距离 最近 {:9.2} 平均 {:9.2} 最远 {:9.2}  速度 均值 {:9.1} 最大 {:9.1}  \
                                 同向度 {:.3}  质心半径 {:9.2}  向心速度 {:9.1}  （邻居阈值 {:.1} / 分离阈值 {:.1}）\
                                 有效变换 原点 ({:.2},{:.2},{:.2}) scale ({:.5},{:.5})",
                                inst.name,
                                s.alive,
                                s.with_neighbour,
                                s.mean_neighbours,
                                s.max_neighbours,
                                s.with_separation,
                                s.min_pair_dist,
                                s.mean_pair_dist,
                                s.max_pair_dist,
                                s.mean_speed,
                                s.max_speed,
                                s.vel_coherence,
                                s.rms_radius,
                                s.mean_inward_speed,
                                s.neighbour_threshold,
                                s.separation_threshold,
                                xo[0],
                                xo[1],
                                xo[2],
                                xs[0],
                                xs[1]
                            );
                        }
                    }
                }
            }
            // `WE_DEBUG_BOIDS_DUMP=1`：把这一刻**每颗粒子**的 (age, pos, vel) 打出来
            // （只在需要定位"邻居长什么样"时开；用来量年龄差 / 是不是同帧生成）。
            if boids_dump {
                let filt = std::env::var("WE_DEBUG_BOIDS_FILTER").unwrap_or_default();
                if filt.is_empty() || inst.name.contains(&filt) {
                    let d = inst.sys.particle_dump();
                    if !d.is_empty() {
                        let mut s = format!("[boids转储] 「{}」t={now:.2}s 存活 {}", inst.name, d.len());
                        for (age, pos, vel) in d.iter() {
                            s.push_str(&format!(
                                " | age={age:.3} pos=({:.2},{:.2},{:.2}) vel=({:.1},{:.1},{:.2})",
                                pos[0], pos[1], pos[2], vel[0], vel[1], vel[2]
                            ));
                        }
                        eprintln!("{s}");
                    }
                }
            }
            if particles_off {
                inst.verts.clear();
                pvbuf.push(Vec::new());
                continue;
            }
            inst.verts.clear();
            inst.sys.build(&mut inst.verts);
            // `WE_PARTICLE_GEOM=1`：四角 → 像素（见上面 `geom_dbg` 的注释）
            if geom_dbg
                && !inst.verts.is_empty()
                && (geom_filt.is_empty() || inst.name.contains(&geom_filt))
            {
                // 本趟的 textureRatio：SPRITESHEET 时 = `g_RenderVar1.w`，否则 = 贴图 高/宽
                let (tex_w, tex_h) = (inst.batch.tex_w, inst.batch.tex_h);
                let (ow, oh) = (self.gl.w as f32, self.gl.h as f32);
                let ratio = if inst.batch.render_var1[2] > 1.0 {
                    inst.batch.render_var1[3]
                } else {
                    tex_h.max(1) as f32 / tex_w.max(1) as f32
                };
                let mut bx0 = f32::MAX;
                let mut bx1 = f32::MIN;
                let mut by0 = f32::MAX;
                let mut by1 = f32::MIN;
                let mut sx0 = f32::MAX;
                let mut sx1 = f32::MIN;
                let mut sy0 = f32::MAX;
                let mut sy1 = f32::MIN;
                let mut sizes: Vec<f32> = Vec::new();
                let mut centers: Vec<[f32; 3]> = Vec::new();
                // 拖尾/CPU 展开路径的四边：宽 = |v1−v0|（right 方向）、长 = |v5−v0|（up 方向）。
                // 用来给「轨迹长度」一个可复核的数（场景单位 + PNG 像素）。
                let mut spans: Vec<[f32; 2]> = Vec::new();
                for ch in inst.verts.chunks(6) {
                    let p = &ch[0];
                    let size = p[6];
                    sizes.push(if size > 0.0 {
                        size
                    } else {
                        let (dx, dy) = (ch[1][0] - p[0], ch[1][1] - p[1]);
                        (dx * dx + dy * dy).sqrt()
                    });
                    centers.push([p[0], p[1], p[2]]);
                    if size <= 0.0 {
                        let w = ((ch[1][0] - ch[0][0]).powi(2) + (ch[1][1] - ch[0][1]).powi(2)).sqrt();
                        let l = ((ch[5][0] - ch[0][0]).powi(2) + (ch[5][1] - ch[0][1]).powi(2)).sqrt();
                        spans.push([w, l]);
                    }
                    sx0 = sx0.min(p[0]);
                    sx1 = sx1.max(p[0]);
                    sy0 = sy0.min(p[1]);
                    sy1 = sy1.max(p[1]);
                    // 四角：size>0 ⇒ 按 shader 的 `ComputeParticlePosition` 复算（旧路径）；
                    // size==0 ⇒ 六个顶点**本身就是**场景空间四角（`quad_scale` 已在 CPU 展开）。
                    let qs: Vec<[f32; 2]> = if size > 0.0 {
                        vec![[0.0f32, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]
                    } else {
                        vec![
                            [ch[0][0], ch[0][1]],
                            [ch[1][0], ch[1][1]],
                            [ch[2][0], ch[2][1]],
                            [ch[4][0], ch[4][1]],
                        ]
                    };
                    for q in qs.iter() {
                        let (wx, wy) = if size > 0.0 {
                            (
                                p[0] + size * (q[0] - 0.5),
                                p[1] - size * (q[1] - 0.5) * ratio,
                            )
                        } else {
                            (q[0], q[1])
                        };
                        // 场景 → PNG 像素：用**可见区**（`self.cover`，= proj 的 ortho 区间）
                        // 直接推，避开"上传矩阵转了两次"的坑（`proj.transposed` 存字段、
                        // 上传前 `t4` 又转置一次 ⇒ 净效果就是 `proj` 本身）。口径：
                        // ndc_x = 2(sx−cx0)/(cx1−cx0) − 1 ；ndc_y 同理；png 行 0 = 可见区顶边。
                        let (vx0, vy0, vx1, vy1) = self.cover;
                        let px = (wx - vx0) / (vx1 - vx0).max(1e-6) * ow;
                        let py = (vy1 - wy) / (vy1 - vy0).max(1e-6) * oh;
                        bx0 = bx0.min(px);
                        bx1 = bx1.max(px);
                        by0 = by0.min(py);
                        by1 = by1.max(py);
                    }
                }
                sizes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let med = sizes[sizes.len() / 2];
                let c = centers[0];
                eprintln!(
                    "[粒子几何] 「{}」t={now:.2}s 粒子 {} 贴图 {tex_w}x{tex_h} ratio={ratio:.4} \
                     尺寸 min={:.1} p50={:.1} max={:.1}；中心场景 x[{sx0:.1},{sx1:.1}] y[{sy0:.1},{sy1:.1}]；\
                     首粒中心场景=({:.1},{:.1})；四角并集 png x[{bx0:.1},{bx1:.1}] y[{by0:.1},{by1:.1}]",
                    inst.name,
                    centers.len(),
                    sizes[0],
                    med,
                    sizes[sizes.len() - 1],
                    c[0],
                    c[1]
                );
                if !spans.is_empty() {
                    spans.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap_or(std::cmp::Ordering::Equal));
                    let w50 = spans[spans.len() / 2][0];
                    spans.sort_by(|a, b| a[1].partial_cmp(&b[1]).unwrap_or(std::cmp::Ordering::Equal));
                    let l50 = spans[spans.len() / 2][1];
                    let px_per_unit = ow / (self.cover.2 - self.cover.0).max(1e-6);
                    eprintln!(
                        "[粒子几何]   ↳ 四边（CPU 展开）：宽p50={w50:.2} 长p50={l50:.2}（场景单位）\
                         = 宽{w:.2}px 长{l:.2}px（{ow}x{oh} 口径，{px_per_unit:.4} px/单位）",
                        w = w50 * px_per_unit,
                        l = l50 * px_per_unit
                    );
                }
            }
            if dbg {
                // 只打**有顶点**的系统 + 可选名字过滤（`WE_DEBUG_PARTICLES_FILTER=<子串>`），
                // 否则 29 个系统 × 每帧一行会把日志淹掉（本项目踩过 `grep|head` 截断的坑）。
                let filt = std::env::var("WE_DEBUG_PARTICLES_FILTER").unwrap_or_default();
                if !inst.verts.is_empty() && (filt.is_empty() || inst.name.contains(&filt)) {
                    let (mut x0, mut y0, mut x1, mut y1) =
                        (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
                    for v in &inst.verts {
                        x0 = x0.min(v[0]);
                        y0 = y0.min(v[1]);
                        x1 = x1.max(v[0]);
                        y1 = y1.max(v[1]);
                    }
                    // 单粒子**屏幕全宽**（维度 = 场景单位；顶点 idx 6 就是 shader 用的 size）的
                    // p50/p90 —— 判"块大块小"用可测的数，而不是肉眼。每粒子 6 顶点，取每粒子首顶点。
                    // 新语义（`quad_scale`）下 size 字段恒 0（四角已在 CPU 侧展开）⇒ 用首两个顶点
                    // 的**间距**当全宽（= size × |scale.x|，旋转不改长度）。
                    let mut sizes: Vec<f32> = inst
                        .verts
                        .chunks(6)
                        .map(|c| {
                            if c[0][6] > 0.0 {
                                c[0][6]
                            } else {
                                let (dx, dy) = (c[1][0] - c[0][0], c[1][1] - c[0][1]);
                                (dx * dx + dy * dy).sqrt()
                            }
                        })
                        .collect();
                    sizes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    let pct = |p: f32| -> f32 {
                        if sizes.is_empty() {
                            0.0
                        } else {
                            sizes[((sizes.len() as f32 - 1.0) * p).round() as usize]
                        }
                    };
                    eprintln!(
                        "[粒子] 「{}」t={:.2}s 存活 {} 顶点 {} 场景bbox x[{:.0},{:.0}] y[{:.0},{:.0}] 尺寸p50={:.1} p90={:.1}（场景单位）",
                        inst.name,
                        now,
                        inst.sys.count(),
                        inst.verts.len(),
                        x0,
                        x1,
                        y0,
                        y1,
                        pct(0.5),
                        pct(0.9)
                    );
                }
            }
            pvbuf.push(std::mem::take(&mut inst.verts));
        }
        let pin: Vec<usize> = self
            .particle_systems
            .iter()
            .map(|i| i.insert_before)
            .collect();
        // `insert_before == 0` = 粒子对象排在**所有图层之前** ⇒ 就在趟循环之前画
        // （此时场景 RT 已经清好、还没画任何图层）。趟循环里的条件 `pin == pi+1` 覆盖不到它。
        if interleave {
            for (i, inst) in self.particle_systems.iter().enumerate() {
                if pin.get(i).copied() == Some(0) && !pvbuf[i].is_empty() {
                    let target = self.scene_rt.map(|k| &self.rts[k]);
                    self.gl
                        .draw_batch(&inst.batch, target, proj_mvp, &pvbuf[i], inst.additive)?;
                }
            }
        }
        drop(sp_part_pre);

        // ── 相机视差：每帧推一次平滑指针，并算好本帧的 **NDC 位移**（交给下面"放进场景"那趟）──
        // 公式 = `(node − cam + mouse) ⊙ depth · amount`，`mouse` 已含 `diag(1,-1)` 与 influence；
        // 正交投影下场景位移 → NDC 位移是线性的（`2·Δ/可见区`），所以能直接在 NDC 上左乘平移。
        // 关着（默认）时 `par_shift = [0,0]` ⇒ 下面一次矩阵乘法都不做 ⇒ 逐字节不变。
        let par_shift: [f32; 2] = if self.par_on {
            let dt = (now - self.time).max(0.0);
            let tgt = self.mouse.unwrap_or((0.5, 0.5));
            self.par.advance([tgt.0, tgt.1], dt);
            let (vis_w, vis_h) = (
                (self.cover.2 - self.cover.0).abs(),
                (self.cover.3 - self.cover.1).abs(),
            );
            let nd = self.par.ndc_shift(self.par.mouse_term(), [vis_w, vis_h]);
            self.parallax_disp = nd;
            if std::env::var_os("WE_DEBUG_PARALLAX").is_some() {
                let m = self.par.mouse_term();
                eprintln!(
                    "[视差] t={:.3} 指针=({:.4},{:.4}) 鼠标项(场景px)=({:.1},{:.1}) ortho={:?} mi={} amount={} 位移NDC=({:.4},{:.4})",
                    now, self.par.pointer[0], self.par.pointer[1], m[0], m[1],
                    self.par.ortho, self.par.cfg.mouse_influence, self.par.cfg.amount, nd[0], nd[1]
                );
            }
            nd
        } else {
            [0.0, 0.0]
        };

        // ── 逐 pass GPU 剖析（`WE_PROFILE_PASSES`，默认关）：标签表 + 本帧查询槽 ──
        let prof = passtime::prof_on() && !profcls::total_on();
        let prof_wall = if prof {
            // 「目标」标签：RT 名（`rt_index` 反查）+ 尺寸；屏幕趟直接写「屏幕」。
            let mut rt_name: Vec<Option<String>> = vec![None; self.rts.len()];
            for (k, v) in self.rt_index.iter() {
                if *v < rt_name.len() {
                    rt_name[*v] = Some(k.clone());
                }
            }
            let mut meta: Vec<String> = Vec::with_capacity(self.passes.len());
            let (sw, sh) = (self.gl.w, self.gl.h);
            for (i, p) in self.passes.iter().enumerate() {
                let (desc, w, h) = match p.target {
                    Some(t) => {
                        let rt = &self.rts[t];
                        (
                            format!("RT[{t}]:{}", rt_name.get(t).cloned().flatten().unwrap_or_default()),
                            rt.w,
                            rt.h,
                        )
                    }
                    None => ("屏幕".to_string(), sw, sh),
                };
                let bl = match p.blend {
                    gl_ctx::Blend::Opaque => "Opaque",
                    gl_ctx::Blend::Translucent => "Trans",
                    gl_ctx::Blend::Additive => "Add",
                };
                let mesh = match &p.mesh_anim {
                    Some(m) => format!(" 网格:{}", m.name),
                    None => String::new(),
                };
                meta.push(format!(
                    "{desc} {w}x{h} 单位={} {bl}{mesh}{}",
                    p.units.len(),
                    if i >= npasses { " [省电档未跑]" } else { "" }
                ));
            }
            passtime::prof_meta_init(meta);
            self.gl.pt_frame_begin();
            Some(std::time::Instant::now())
        } else {
            None
        };
        let mut prof_fullscreen = 0usize;

        // `WE_PERF_MASK_DEDUPE`：算出本帧可以跳过的重复烘焙趟（见 `perf_mask_dedupe_on`）。
        // 键 = (目标 RT, 源贴图)：同一对象内几何/mvp/源全同 ⇒ 重复趟输出逐字节相同。
        let bake_skip: std::collections::HashSet<usize> = if perf_mask_dedupe_on()
            && !self.mask_warp_passes.is_empty()
        {
            let mut seen: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
            let mut sk: std::collections::HashSet<usize> = std::collections::HashSet::new();
            for &i in self.mask_warp_passes.iter() {
                let p = &self.passes[i];
                if let (Some(t), Some((_, Src::Tex(tex)))) = (p.target, p.units.first()) {
                    if !seen.insert((t, *tex)) {
                        sk.insert(i);
                    }
                }
            }
            sk
        } else {
            std::collections::HashSet::new()
        };

        // WE_PERF_STATIC_CACHE runtime guard: every frozen RT must still carry the
        // signature it had when it was drawn. If one moved (a parent chain animating),
        // the judgement does not hold here - drop the whole cache and draw everything.
        if let Some(c) = self.static_cache.as_mut() {
            if c.live {
                let mut bad = 0usize;
                for t in 0..c.frozen.len() {
                    if !c.frozen[t] || c.sig[t] == 0 {
                        continue;
                    }
                    if c.sig[t] != c.rt_sig(&self.passes, t) {
                        bad += 1;
                    }
                }
                if bad > 0 {
                    c.live = false;
                    eprintln!(
                        "[static-cache] WARN {} frozen RTs moved their (mvp, units) signature => cache dropped, redrawing every pass from this frame on",
                        bad
                    );
                }
            }
        }

        let mut static_skipped: u64 = 0;


        for (pi, p) in self.passes.iter().take(npasses).enumerate() {
            if bake_skip.contains(&pi) || self.fuse_skip.contains(&pi) {
                continue;
            }
            if self.static_cache.as_ref().map_or(false, |c| {
                c.live
                    && c.skip.get(pi).copied().unwrap_or(false)
                    && p.target.map_or(false, |t| c.sig.get(t).copied().unwrap_or(0) != 0)
            }) {
                static_skipped += 1;
                continue;
            }

            // 全屏趟计数（profiling 用）：目标是屏幕，或目标 RT 尺寸 == 窗口尺寸。
            if prof {
                let fs = match p.target {
                    None => true,
                    Some(t) => {
                        let rt = &self.rts[t];
                        rt.w == self.gl.w && rt.h == self.gl.h
                    }
                };
                if fs {
                    prof_fullscreen += 1;
                }
            }
            // 参照实现 CImage::render：对象特效链的【最后一趟】不写 alpha
            // （glColorMask(1,1,1,0)）。每趟都显式设 ⇒ 天然恢复、不泄漏到下一个对象。
            self.gl.color_mask(true, true, true, !self.alpha_off.contains(&pi));
            // K1：这一趟要用 shader 内的 `ApplyBlending`（`BLENDMODE`）⇒ 先把"画它之前的
            // **画面**"在 GPU 侧拷进那张常驻拷贝 RT，再绑到 shader 声明的画面槽上。
            // 拷贝源 = 场景合成 RT（没有场景 RT 时 = 当前窗口）。默认路径 `cbm_passes` 恒空
            // ⇒ 这里一次都不进（逐字节不变）。
            if let Some(&(cr, src)) = self.cbm_passes.get(&pi) {
                match src {
                    Some(si) => self.gl.copy_rt_into_rt(&self.rts[si], &self.rts[cr]),
                    None => {
                        self.gl.bind_target(p.target.map(|i| &self.rts[i]));
                        self.gl.copy_screen_into_rt(&self.rts[cr]);
                    }
                }
            }
            units.clear();
            for (u, s) in &p.units {
                let (t, tw, th) = self.src_tex(*s);
                // 逐贴图尺寸（`WE_TEXRES`，默认开）：RT 报**图层自己的画布**（`rt_logical`），
                // 贴图报上传尺寸；`.xy`（源）与 `.zw`（采样）今天恒等 —— 差别只在
                // `mipmap_larger` 的图集上（我们按原图解码，源=采样）。见 `gl_ctx::UnitTex`。
                let rt_logical_here = if let Src::Rt(i) = *s {
                    self.rt_logical.get(i).copied().flatten()
                } else {
                    None
                };
                let rep_here = if let Src::Rt(i) = *s {
                    self.rt_report.get(i).copied().flatten()
                } else {
                    None
                };
                // 降采样过的 RT：按**逻辑**尺寸上报（视觉尺度不变，见 `perf_res_scale_on`）
                let (rw, rh) = if let Some(rep) = rep_here {
                    rep
                } else if texres_pass_on() {
                    // `WE_TEXRES_PASS`（默认开）：RT 槽的 `g_TextureNResolution` 报**该 RT 的真实
                    // 像素尺寸**（`src_tex` 的 `tw,th`）。这与两份重实现**逐行同构**：
                    // · CaptSilver `WPShaderValueUpdater.cpp:297-306`：`{rt_width, rt_height,
                    // rt_width, rt_height}`，`rt_width/height` 直接取自 `Scene::renderTargets`；
                    // · OWE `SceneMaterialBuilder.cpp:377-384`：`(rt.width, rt.height, rt.width, rt.height)`。
                    // `rt_logical`（图层自己的 `obj.size`）是旧口径：它让 `effects/blend` 的
                    // `m = g_BlendScale · quad_h_px / T0_h` 偏离作者值 —— 691 层 `T0_h=1344` 而真实
                    // 缓冲 1080 ⇒ `m` 偏小 1.244×（"字里人物太小"），且随输出分辨率漂移。
                    // `WE_TEXRES_PASS=0` ⇒ 逐字节回旧（照旧用 `rt_logical`）。
                    (tw, th)
                } else {
                    rt_logical_here.unwrap_or((tw, th))
                };
                // `WE_T1_REPORT`（诊断，默认关）：只覆盖 unit1（= `effects/blend` 的遮罩槽 `T1`）
                // 的**上报值**；缓冲 / 四边形 / 内容一律不动 ⇒ 只改 `blend.vert` 里那两个式子。
                let (rw, rh) = match t1_report_override() {
                    Some((ow, oh, pass)) if *u == 1
                        && matches!(*s, Src::Rt(_))
                        && pass.map_or(true, |p| p == pi) =>
                    {
                        if std::env::var_os("WE_DBG_TEXEXT").is_some() {
                            eprintln!(
                                "[T1REP] pass={} unit=1 actual={}x{} reported={}x{} (override)",
                                pi, tw, th, ow, oh
                            );
                        }
                        (ow, oh)
                    }
                    _ => (rw, rh),
                };
                if std::env::var_os("WE_DBG_TEXEXT").is_some() {
                    eprintln!(
                        "[TexExt] pass={} unit={} actual={}x{} reported={}x{} logical={:?}",
                        pi, u, tw, th, rw, rh, rt_logical_here
                    );
                }
                units.push(gl_ctx::UnitTex::new(*u, t, rw, rh));
            }
            // WE 的 shader 普遍拿 `g_Time`（秒）驱动动画（本机 17 条壁纸里 110 个 shader 用到它）。
            // 不给就是 0 ⇒ 所有动画冻在第一帧（水波/树叶摆动/脉动/抖动/水纹…）。
            // 逐帧下发走 `DrawCtx::time`（**不再克隆整条 scalars**：那是每趟每帧一次堆分配）。
            // ── 鼠标（`g_PointerPosition`）：WE 把它当**每帧**下发的**场景级** uniform，
            // 每一趟都注入（参照实现 `CPass.cpp:879`：
            // `addUniform("g_PointerPosition", scene.getMousePosition)`）。
            // 本机驱动它的是 `effects/xray`（鼠标处透视衣服）。
            // **没有输入时也要下发**（值 = `pointer_default`，见字段注释）。
            // 以前是 `self.mouse.is_some` 才注入 ⇒ 没输入时这个 uniform 从头到尾没人写
            // ⇒ GL 里就是默认的 0 ⇒ xray 的透视区被推到贴图角落 ⇒ **整层无变化**
            // （用户报的"透明层不显示"）。WE/OWE 的语义是"指针总是有一个位置"，
            // 没有输入时那个位置是**中心**（OWE `Scene.cppm:2872`）。
            // `WE_POINTER=0` 可整体关回旧口径（A/B）。
            // 有脚本标量的趟也必须走「逐帧标量表」这条路（否则脚本值没有入口）。
            let fx_vals = self
                .fx_frame_scalars
                .iter()
                .find(|(i, _)| *i == pi)
                .map(|(_, v)| v.as_slice());
            let mut scalars_ref: &[(String, Value)] = &p.scalars;
            if self.pointer_on || self.par_on || fx_vals.is_some() {
                self.frame_scalars.clear();
                self.frame_scalars.extend_from_slice(&p.scalars);
                // 有输入 ⇒ 每一趟都下发（WE 的语义：场景级 uniform 每帧都有值）。
                // 没有输入 ⇒ **只给"把指针反投影进图层纹理空间"的那一族趟**下发兜底位
                // （`pointer_proj`，判据 = 本趟 shader 自己声明了 ETVPi；见字段注释）。
                // 其余趟保持旧口径（不下发 ⇒ GL 里是 0），那是一条**行为护栏**：
                // 只用 g_PointerPosition 当屏幕坐标的特效（3704273480 主题2）保持逐字节不变。
                if self.pointer_on
                    && (self.mouse.is_some() || self.pointer_proj.contains(&pi))
                {
                    let (mx, my) = self.effective_pointer();
                    let (lx, ly) = self.mouse_last.unwrap_or((mx, my));
                    self.frame_scalars
                        .push(("g_PointerPosition".to_string(), Value::V2(mx, my)));
                    self.frame_scalars
                        .push(("g_PointerPositionLast".to_string(), Value::V2(lx, ly)));
                }
                // `g_ParallaxPosition`：场景级、每帧算（OWE `UniformSource.cpp:738-745`）。
                // 视差关着时不推 ⇒ 走静态中性值 (0.5,0.5)（= 改动前逐字节一致）。
                if self.par_on {
                    let pp = self.par.position_uniform();
                    self.frame_scalars.push((
                        "g_ParallaxPosition".to_string(),
                        Value::V2(pp[0], pp[1]),
                    ));
                }
                // 脚本值放**最后** ⇒ 同名覆盖静态 `value`（`draw_pass` 按序 set_uniform）。
                if let Some(vals) = fx_vals {
                    self.frame_scalars.extend_from_slice(vals);
                }
                scalars_ref = self.frame_scalars.as_slice();
            }
            let mvp_eff: [f32; 16] = match self.parallax_passes.get(&pi) {
                Some(pk) if self.par_on => {
                    let (vis_w, vis_h) = (
                        (self.cover.2 - self.cover.0).abs(),
                        (self.cover.3 - self.cover.1).abs(),
                    );
                    let off = parallax_layer_off(*pk, par_shift, [vis_w, vis_h]);
                    let nd = self.par.ndc_shift(off, [vis_w, vis_h]);
                    shift_mvp_ndc(&p.mvp, nd)
                }
                _ => p.mvp,
            };

            let ctx = gl_ctx::DrawCtx {
                target: p.target.map(|i| &self.rts[i]),
                mvp: mvp_eff,
                units: &units,
                scalars: scalars_ref,
                // `WE_FX_TIME_SCALE`（默认 1.0）：只乘 shader 看到的 `g_Time`，见 [`fx_time_scale`]
                time: Some(self.time * fx_time_scale()),
                blend: p.blend,
            };
            if self
                .mask_warp_groups
                .iter()
                .any(|(_, ws)| ws.contains(&pi))
            {
                // 烘焙缓冲每趟前清成透明黑 —— 网格没盖到的像素必须读 0，
                // 不能残留上一张遮罩（多张遮罩共用一张 RT）。
                // `WE_PERF_MASK_NOCLEAR`（默认开）把这一趟换成"预乘 + 关混合"的写法
                // ⇒ 目标被**整块覆盖**而不是叠加，于是这次清屏整个不需要（等价性推导）
                // `mask_noclear_on`；实测 2560 t=5 逐字节相同，省 2.14 ms/帧）。
                self.gl.bind_target(p.target.map(|i| &self.rts[i]));
                // 全屏共用 RT 必须每趟清干净（网格盖不满整张，上一趟的别的遮罩
                // 会残留在边缘）。清屏前先撤掉趟循环里上一趟留下的 scissor —— 否则
                // 清屏也会被剪，残留更隐蔽。
                let fs_bake = mask_fullscreen_on();
                if fs_bake && !profcls::skip_mask_clear() {
                    self.gl.scissor(None);
                }
                if !profcls::skip_mask_clear()
                    && !(mask_noclear_on() && !fs_bake)
                {
                    self.gl.clear(0.0, 0.0, 0.0, 0.0);
                }
            }
            // `WE_PERF_SCISSOR`：把这一趟剪到目标 RT 的内容包围盒。**必须在清屏之后**
            // （scissor 也剪 `glClear`）；关着时 `rt_bbox` 全 `None` ⇒ 一次 `disable`
            // （GL 默认就是关的，所以默认路径等价于不碰）。
            if perf_scissor_on() {
                let bb = self
                    .pass_bbox
                    .get(pi)
                    .copied()
                    .flatten()
                    .or_else(|| {
                        p.target
                            .and_then(|t| self.rt_bbox.get(t).copied().flatten())
                    });
                self.gl.scissor(bb);
            }
            let t_gpu = if passtime::on() {
                self.gl.gpu_sync();
                Some(std::time::Instant::now())
            } else {
                None
            };
            if prof {
                self.gl.pt_begin(pi);
            }
            if let Err(e) = self.gl.draw_pass(&p.layer, &ctx) {
                return Err(format!("第 {pi} 趟失败（目标={:?}）：{e}", p.target));
            }
            if prof {
                self.gl.pt_end();
            }
            // 逐趟表：这一趟画完之后目标的像素指纹（改前/改后两张表 diff ⇒ 谁真的吃到颜色）
            if let Some(t0) = t_gpu {
                self.gl.gpu_sync();
                passtime::note(pi, t0.elapsed().as_micros() as u64);
            }
            if dbg_fbo {
                eprintln!(
                    "[FBO] 第 {pi} 趟后 目标={:?} 状态 = 0x{:x}",
                    p.target,
                    self.gl.check_fb_status()
                );
            }
            // 这一趟之后若轮到某个**粒子对象**（`objects` 里排在它后面的第一个
            // 对象就是粒子）⇒ 就在这里把这路粒子画进**场景 RT**。WE 就是这么做的：
            // 粒子是普通场景对象，按 `objects` 次序渲进场景 FBO ⇒ 后面画的图层（前景房间层）
            // **会挡住它**。修的是用户报的「室内不应该有雨」。
            if interleave && pi + 1 < npasses {
                for (i, inst) in self.particle_systems.iter().enumerate() {
                    if pin.get(i).copied() != Some(pi + 1) || pvbuf[i].is_empty() {
                        continue;
                    }
                    // 目标 = 场景 RT（这批粒子在"场景合成"的中间位置，必须落在同一张缓冲里）
                    let target = self.scene_rt.map(|k| &self.rts[k]);
                    self.gl
                        .draw_batch(&inst.batch, target, proj_mvp, &pvbuf[i], inst.additive)?;
                }
            }
        }
        // WE_PERF_STATIC_CACHE: record the signatures of the frozen RTs that were just
        // drawn (frame 0, or after the cache was dropped). Re-checking happens above.
        if let Some(c) = self.static_cache.as_mut() {
            c.skipped += static_skipped;
            for t in 0..c.frozen.len() {
                if c.frozen[t] {
                    c.sig[t] = c.rt_sig(&self.passes, t);
                }
            }
        }


        drop(sp_passes);
        if passtime::on() {
            passtime::tick_frame(npasses);
        }
        if let Some(t0) = prof_wall {
            let wall_ns = t0.elapsed().as_nanos() as u64;
            let samples = self.gl.pt_collect();
            if !samples.is_empty() {
                passtime::prof_frame(&samples, prof_fullscreen, wall_ns);
                passtime::prof_tick();
            }
        }

        // ── 粒子收尾：**没能插进趟序列的**（`insert_before >= 趟数`、或关掉插序时的全部）
        // 按老路子画到 `final_rt`（= 屏幕 / 场景 RT）。插进趟里的那些已经在趟循环里画完了。
        let sp_part = ftime::Span::new(&ftime::PART);
        if interleave {
            for (i, inst) in self.particle_systems.iter().enumerate() {
                let pin_i = pin.get(i).copied().unwrap_or(usize::MAX);
                if pin_i < npasses {
                    continue; // 已在趟循环里按对象次序画过
                }
                if pvbuf[i].is_empty() {
                    continue;
                }
                let target = final_rt.map(|k| &self.rts[k]);
                self.gl
                    .draw_batch(&inst.batch, target, proj_mvp, &pvbuf[i], inst.additive)?;
            }
        } else {
            // 旧行为（`WE_PARTICLES_LAST=1` 或无场景 RT）：全部粒子叠在最后
            for (i, inst) in self.particle_systems.iter().enumerate() {
                if pvbuf[i].is_empty() {
                    continue;
                }
                let target = final_rt.map(|k| &self.rts[k]);
                self.gl
                    .draw_batch(&inst.batch, target, proj_mvp, &pvbuf[i], inst.additive)?;
            }
        }
        // 粒子推进用的是新时间 ⇒ 到这里再赋给 `self.time`（趟循环用的是旧值，见上）。
        // `time_last`：`cameraparallaxdelay` 的混合因子要 `g_Time - g_TimeLast`（WE 同名物）。
        self.time_last = self.time;
        self.time = now;
        // 帧末滚动鼠标："本帧用过的位置"变成"上一帧的位置"（WE `m_mousePositionLast` 的语义）
        self.mouse_last = self.mouse;
        drop(sp_part);

        // ── Glow（可选）：在**每个"暖色粒子"**处铺一朵软光斑（加法叠到同一目标）──────
        // 数据驱动（光源 = 粒子本身，不认对象名）；位置就是粒子位置，所以和火苗
        // 严格同心、跟着动画/摆动一起走。原理与依据见文件上方 `GlowCfg`。
        let sp_glow = ftime::Span::new(&ftime::GLOW);
        if self.glow.particles {
            let (radius, pstrength) = (self.glow.radius, self.glow.pstrength);
            let mut gv = std::mem::take(&mut self.glow_verts);
            gv.clear();
            const CORNERS: [[f32; 2]; 6] = [
                [0.0, 0.0],
                [1.0, 0.0],
                [1.0, 1.0],
                [0.0, 0.0],
                [1.0, 1.0],
                [0.0, 1.0],
            ];
            for inst in self.particle_systems.iter() {
                for p in inst.sys.particles.iter() {
                    let Some(tint) = glow_tint_for(p.color) else {
                        continue;
                    };
                    for c in CORNERS.iter() {
                        gv.push([
                            p.pos[0],
                            p.pos[1],
                            p.pos[2],
                            c[0],
                            c[1],
                            0.0,             // 旋转
                            radius * 2.0,    // 全宽（场景单位）
                            0.0,
                            0.0,
                            0.0,
                            0.0,
                            0.0,
                            1.0,             // 寿命进度（本 shader 不用）
                            tint[0],
                            tint[1],
                            tint[2],
                            pstrength,
                        ]);
                    }
                }
            }
            if let Some(batch) = self.glow_batch.as_ref() {
                let target = final_rt.map(|i| &self.rts[i]);
                self.gl.draw_batch(batch, target, proj_mvp, &gv, true)?;
            }
            self.glow_verts = gv;
        }

        // ── Glow（全屏辉光）：读"含粒子的整帧屏幕" ⇒ 大半径暖色模糊 ⇒ 加法叠回屏幕 ──
        // 必须排在粒子**之后**：否则 `copy_screen_into_rt` 抓到的画面里没有火苗。
        if let Some(src) = self.glow_src {
            if let Some(rt) = self.rts.get(src) {
                if glow_pre {
                    // 源 = **场景 RT**（含粒子），不是屏幕 —— glow 必须是场景光，
                    // 才能在 present 的显示编码里被一起映射（这就是"glow 在曝光之前"）。
                    if let Some(sr) = self.scene_rt.and_then(|i| self.rts.get(i)) {
                        self.gl.bind_target(Some(sr));
                    }
                }
                self.gl.copy_screen_into_rt(rt);
            }
            let gp: Vec<Pass> = std::mem::take(&mut self.glow_passes);
            let mut result: Result<(), String> = Ok(());
            {
                let mut gu: Vec<gl_ctx::UnitTex> = Vec::new();
                for p in gp.iter() {
                    gu.clear();
                    for (u, s) in &p.units {
                        let (t, tw, th) = self.src_tex(*s);
                        gu.push(gl_ctx::UnitTex::new(*u, t, tw, th));
                    }
                    let ctx = gl_ctx::DrawCtx {
                        target: p.target.map(|i| &self.rts[i]),
                        mvp: p.mvp,
                        units: &gu,
                        scalars: &p.scalars,
                        time: Some(self.time * fx_time_scale()),
                        blend: p.blend,
                    };
                    self.gl.color_mask(true, true, true, true);
                    if let Err(e) = self.gl.draw_pass(&p.layer, &ctx) {
                        result = Err(format!("glow 趟失败：{e}"));
                        break;
                    }
                }
            }
            self.glow_passes = gp;
            // 调试：把 glow 的中间缓冲落盘（`WE_GLOW_DUMP=<目录>`）—— 判"门有没有把成片亮区压掉"。
            if let Some(dir) = std::env::var_os("WE_GLOW_DUMP") {
                let _ = std::fs::create_dir_all(&dir);
                for name in ["_rt_glow_ds", "_rt_glow_probeh", "_rt_glow_loc", "_rt_glow_blurh"] {
                    if let Some(&i) = self.rt_index.get(name) {
                        if let Some(rt) = self.rts.get(i) {
                            let p = std::path::Path::new(&dir).join(format!("{name}.png"));
                            self.gl.read_rt_to_png(rt, &p);
                            eprintln!("[glow dump] {name} → {}", p.display());
                        }
                    }
                }
            }
            result?;
        }
        drop(sp_glow);

        // ── present（**只在 `glow_pre` 时跑到这里**）─────────────────────────────
        // 场景 RT（已含粒子 + 场景空间的 glow）→ 屏幕，并在这一趟里做**显示编码**
        // （`WE_EXPOSURE`）。这就是"glow 在曝光之前"的落地：glow 是场景光，编码是显示变换，
        // 顺序必须是"先加光、后编码"。老路（`glow_pre=false`）present 仍在 `passes` 末尾。
        let sp_present = ftime::Span::new(&ftime::PRESENT);
        if glow_pre {
            let pp = std::mem::take(&mut self.pending_present);
            if let Some(p) = pp {
                units.clear();
                for (u, s) in &p.units {
                    let (t, tw, th) = self.src_tex(*s);
                    units.push(gl_ctx::UnitTex::new(*u, t, tw, th));
                }
                self.gl.color_mask(true, true, true, true);
                let r = {
                    let ctx = gl_ctx::DrawCtx {
                        target: p.target.map(|i| &self.rts[i]),
                        mvp: p.mvp,
                        units: &units,
                        scalars: &p.scalars,
                        time: Some(self.time * fx_time_scale()),
                        blend: p.blend,
                    };
                    self.gl.draw_pass(&p.layer, &ctx)
                };
                self.pending_present = Some(p);
                r.map_err(|e| format!("present 趟失败（glow_pre）：{e}"))?;
            }
        }
        drop(sp_present);

        // ── `WE_PERF_SCISSOR_PP`：第 1 帧画完之后探一次「每一趟自己写到哪」 ──────────
        // 必须在这里（而不是建树时）：特效趟读的是别的 RT，建树时它们还是全 0。
        // 只在前 2 帧内做一次，之后 `pass_bbox` 非空 ⇒ 一个判断就跳过（零稳态开销）。
        if perf_scissor_pp_on() && self.pass_bbox.is_empty() && self.frames >= 1 {
            self.probe_pass_boxes()?;
        }

        // ── 切换壁纸的过渡：把旧帧按 α 叠在**已画好的新画面**上（crossfade）──────────
        // 只有真在过渡（或离线验证钩子）时才走这一段；过渡结束 ⇒ **什么都不画** ⇒
        // 输出与改动前逐字节相同、零每帧开销（`fade` 回到 None 后这里只是个判空）。
        // 位置必须在 present/粒子/glow **之后**：叠的是最终画面。
        if let Some((t0, dur)) = self.fade {
            let el = t0.elapsed().as_secs_f32() * 1000.0;
            let a = 1.0 - (el / dur).clamp(0.0, 1.0);
            if a <= 0.002 {
                // 过渡结束：把旧帧纹理还回去，状态清空（回到零开销）
                if let Some((t, _, _)) = self.fade_tex.take() {
                    self.gl.free_texture(t);
                }
                self.fade = None;
            } else if let Some((tex, tw, th)) = self.fade_tex {
                self.draw_fade_overlay(tex, tw, th, a)?;
            }
        } else if let Some((tex, tw, th, a)) = self.fade_test {
            self.draw_fade_overlay(tex, tw, th, a)?;
        }

        // ── 调试：把每张中间 RT 落盘一次（`WE_DUMP_RT=<路径前缀>`，只在**第一次** draw_frame）
        // 为什么需要它：链里"某趟没画 / 画了但输出 0 / 缓冲被建成 1 像素"三种情况在**最终图
        // 上完全相同**，只有把中间缓冲读出来才能分辨（`2861113973` 的整屏后处理层就是靠这条
        // 定位的：它的收尾缓冲里有内容，但合成回场景那一步没落地）。
        // 只在第一帧做一次 ⇒ 对稳态零开销；存成 `{prefix}{索引:03}_{RT名}.png`。
        if let Ok(prefix) = std::env::var("WE_DUMP_RT") {
            // `WE_DUMP_RT_FRAME=<n>`：改到**第 n 帧**（0 起）再落盘。默认不设 ⇒ 仍是第一帧，
            // 逐字节/逐行为不变。为什么需要它：本场景的粒子 `starttime=15s`（f900 口径下
            // 第 450 帧才出生）⇒ 只在第 0 帧 dump 永远看不到粒子写进 RT 的 alpha。
            let want: u64 = std::env::var("WE_DUMP_RT_FRAME")
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0);
            use std::sync::atomic::{AtomicBool, Ordering};
            static RT_DUMPED: AtomicBool = AtomicBool::new(false);
            if self.frames >= want && !RT_DUMPED.swap(true, Ordering::Relaxed) {
                let mut by_idx: Vec<(usize, String)> =
                    self.rt_index.iter().map(|(n, i)| (*i, n.clone())).collect();
                by_idx.sort();
                let mut seen = std::collections::HashSet::new();
                for (i, name) in by_idx {
                    if !seen.insert(i) {
                        continue; // 同一张 RT 的别名只存一次
                    }
                    let Some(rt) = self.rts.get(i) else { continue };
                    let mut src = self.gl.read_rt_pixels(rt);
                    let mut out = vec![0u8; (rt.w as usize) * (rt.h as usize) * 4];
                    readback_into(&mut src, &mut out, rt.w, rt.h, false);
                    let safe: String = name
                        .chars()
                        .map(|c| {
                            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                                c
                            } else {
                                '_'
                            }
                        })
                        .collect();
                    let p = format!("{prefix}{i:03}_{safe}.png");
                    let _ = image::save_buffer(&p, &out, rt.w, rt.h, image::ColorType::Rgba8);
                    eprintln!("[RT] 落盘 {p}（{}x{}）", rt.w, rt.h);
                }
            }
        }

        // 分相计时收尾（`WE_FRAME_TIMING=1` 才有输出；每 60 帧一行）
        if ftime::on() {
            use std::sync::atomic::Ordering::Relaxed;
            ftime::TOTAL.fetch_add(cpu_thread_us().saturating_sub(ft_c0), Relaxed);
            let n = ftime::FRAMES.fetch_add(1, Relaxed) + 1;
            if n % 60 == 0 {
                ftime::report();
            }
        }
        if let Some(t0) = prof_total_t0 {
            profcls::frame_end(&self.gl, t0);
        }
        Ok(())
    }

    /// 手动推进时钟（不调用就按 30fps 走）；返回当前时间
    /// **`set-prop` 的局部刷新**（`WE_SETPROP_FAST=1`）：
    /// 如果这条用户属性只绑到某个**文字层**的 `color`（例如本机 `3704273480` 的 `text5`），
    /// 就地把那张光栅化贴图的字色换掉，返回 `Ok(true)` —— 调用方**不要**重建场景。
    /// 其它一切情况（属性不存在 / 绑的是 `visible` / 绑到材质色 / 文字层带底板）返回
    /// `Ok(false)`，调用方照旧重建：宁可慢，不许错。
    /// 依据：`text::rasterize` 写的是**直通 alpha**（`rgb = 字色`，`a = 覆盖率`），
    /// 覆盖率与颜色无关 ⇒ 换色 = 同样的掩膜 + 新色，与重新光栅化逐字节相同。
    pub fn refresh_user_prop(&mut self, name: &str, value: &str) -> Result<bool, String> {
        if !setprop_fast_on() {
            return Ok(false);
        }
        let Some(&id) = self.prop_text.get(name) else {
            return Ok(false);
        };
        let Some(recs) = self.text_recs.get(&id) else {
            return Ok(false);
        };
        if recs.is_empty() || recs.iter().any(|r| r.plate) {
            return Ok(false);
        }
        let rgb = match parse_color_triplet(value) {
            Some(c) => c,
            None => return Ok(false),
        };
        // 只记账：真正的贴图上传在 `draw_frame` 开头做（那里 EGL context 才是 current 的）
        if let Some(recs) = self.text_recs.get_mut(&id) {
            for r in recs.iter_mut() {
                r.pending = Some(rgb);
            }
        }
        Ok(true)
    }

    /// 把 [`refresh_user_prop`] 记下的字色真正落到贴图上。
    /// **必须在有 current EGL context 的地方调用**（= `draw_frame` 开头）；控制面线程里
    /// 调用等于什么也不做（见 `TextRec::pending` 的注释）。
    fn apply_pending_text_colors(&mut self) {
        let ids: Vec<i64> = self
            .text_recs
            .iter()
            .filter(|(_, rs)| rs.iter().any(|r| r.pending.is_some()))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let Some(recs) = self.text_recs.get(&id) else { continue };
            // `(贴图下标, 宽, 高, 新色, 覆盖率掩膜, 量化色)`。**值没变的直接跳过**：
            // 拖色盘时 UI 每帧一条，量化到 8 位后大量重复（实测约占一半）⇒ 白传 9.7 MB。
            let mut jobs: Vec<(usize, u32, u32, [f32; 3], Vec<u8>, [u8; 3])> = Vec::new();
            let mut skipped = 0usize;
            for r in recs {
                if let Some(rgb) = r.pending {
                    let q = [
                        (rgb[0].clamp(0.0, 1.0) * 255.0).round() as u8,
                        (rgb[1].clamp(0.0, 1.0) * 255.0).round() as u8,
                        (rgb[2].clamp(0.0, 1.0) * 255.0).round() as u8,
                    ];
                    if q == r.cur_rgb {
                        skipped += 1;
                        continue;
                    }
                    jobs.push((r.tex, r.w, r.h, rgb, r.alpha.clone(), q));
                }
            }
            if skipped > 0 && std::env::var_os("WE_DBG_SETPROP_FAST").is_some() {
                eprintln!("[setprop-fast] obj {id}：{skipped} 张贴图字色没变 ⇒ 不重传");
            }
            for (tex_idx, w, h, rgb, alpha, q) in jobs {
                let t_up = std::time::Instant::now();
                if std::env::var_os("WE_DBG_SETPROP_FAST").is_some() {
                    let users: Vec<usize> = self
                        .passes
                        .iter()
                        .enumerate()
                        .filter(|(_, p)| {
                            p.units
                                .iter()
                                .any(|(_, s)| matches!(s, Src::Tex(i) if *i == tex_idx))
                        })
                        .map(|(i, _)| i)
                        .collect();
                    // 把"谁吃这张贴图"连同**它写哪张 RT** 一起打出来 —— 这就是局部刷新要
                    // 重烘的**全部**范围（本机 3704273480：趟 23 → RT[16] → 趟 24 → RT[17] →
                    // 趟 25 → RT[18] → 趟 26 场景 RT）。
                    let mut chain: Vec<String> = Vec::new();
                    for pi in &users {
                        let tgt = match self.passes[*pi].target {
                            Some(t) => format!(
                                "趟{pi}→RT[{t}]{}",
                                self.rt_index
                                    .iter()
                                    .find(|(_, v)| **v == t)
                                    .map(|(k, _)| format!(" {k}"))
                                    .unwrap_or_default()
                            ),
                            None => format!("趟{pi}→屏幕"),
                        };
                        chain.push(tgt);
                    }
                    eprintln!(
                        "[setprop-fast] obj {id} tex {tex_idx} {w}x{h} ⇒ 引用它的趟 {users:?}（[{}]；共 {} 趟、{} 张贴图）",
                        chain.join(" → "),
                        self.passes.len(),
                        self.texs.len()
                    );
                }
                let Some(&(tex, tw, th)) = self.texs.get(tex_idx) else { continue };
                if (tw, th) != (w, h) {
                    continue;
                }
                // 只重传"有字形的那一块"：bbox 外的像素 `a == 0`（换色前后都不可见），
                // 所以子块上传与整张 `glTexImage2D` **像素等价**（A/B 逐字节钉住，
                // 见 renderer 的单测 `rect_upload_matches_full_upload`）。
                let Some((x0, y0, sw, sh)) = alpha_bbox(&alpha, w, h) else {
                    continue; // 整张透明：没有可画的像素，连传都不用传
                };
                let t_cpu = std::time::Instant::now();
                let sub = recolor_rect(&alpha, w, h, (x0, y0, sw, sh), rgb);
                let cpu_ms = t_cpu.elapsed().as_secs_f64() * 1000.0;
                if std::env::var_os("WE_DBG_SETPROP_FAST").is_some() {
                    let covered = alpha.iter().filter(|a| **a > 0).count();
                    let e0 = self.gl.get_error_inner();
                    if let Ok(dir) = std::env::var("WE_SETPROP_DUMP") {
                        let _ = std::fs::create_dir_all(&dir);
                        let _ = image::save_buffer(
                            std::path::Path::new(&dir).join(format!("recolor_obj{id}_tex{tex_idx}.png")),
                            &sub,
                            sw,
                            sh,
                            image::ColorType::Rgba8,
                        );
                    }
                    eprintln!(
                        "[setprop-fast] 上传 tex {tex_idx} {w}x{h} 子块 ({x0},{y0}) {sw}x{sh} = {:.1}%（覆盖像素 {covered}/{w}x{h}），上传前 gl 错误 {e0:#x}",
                        100.0 * (sw as f64) * (sh as f64) / ((w as f64) * (h as f64))
                    );
                }
                // 建贴图时走的是 `flip = true`（`make_texture`），这里必须同一个朝向。
                // `WE_SETPROP_RECT=0` ⇒ 退回整张 `glTexImage2D`（A/B 用：子块在部分驱动上
                // 反而更慢 —— 平铺纹理的局部更新要先解平铺，实测本机 iGPU 两者接近）。
                if setprop_rect_on() {
                    let _ = self.gl
                        .replace_texture_rect(tex, w, h, x0, y0, sw, sh, &sub, true);
                } else {
                    let rgba = recolor_from_alpha(&alpha, rgb);
                    let _ = self.gl.replace_texture(tex, &rgba, w, h, true);
                }
                let after_upload_ms = t_up.elapsed().as_secs_f64() * 1000.0;
                if let Some(recs) = self.text_recs.get_mut(&id) {
                    for r in recs.iter_mut() {
                        if r.tex == tex_idx {
                            r.cur_rgb = q;
                        }
                    }
                }
                if std::env::var_os("WE_DBG_SETPROP_FAST").is_some() {
                    // 一次局部刷新的**全部**成本，分三段量：CPU 铺色 / glTexImage2D 排队 /
                    // `glFinish` 等 GPU 落地（别让异步当免费 —— 真正卡住主循环的是第三段）
                    self.gl.gpu_sync();
                    let total_ms = t_up.elapsed().as_secs_f64() * 1000.0;
                    eprintln!(
                        "[setprop-fast] 上传后 gl 错误 {:#x}，本次局部刷新 {total_ms:.2} ms（铺色 {cpu_ms:.2} + 上传 {:.2} + 等 GPU {:.2}）",
                        self.gl.get_error_inner(),
                        after_upload_ms - cpu_ms,
                        total_ms - after_upload_ms
                    );
                }
            }
            if let Some(recs) = self.text_recs.get_mut(&id) {
                for r in recs.iter_mut() {
                    r.pending = None;
                }
            }
        }
    }

    pub fn set_time(&mut self, t: f32) {
        // 设成"基准"，之后仍按墙钟往前走（保持单调）。
        // 播放速度（`set time_scale=…`）的唯一落点就在这里：内核每帧把**墙钟**喂进来，
        // 我们按倍率把它换算成**场景时刻** ⇒ `g_Time` / 粒子 `dt` / 网格动画 / 场景灯
        // 一起按倍率走。音频（独立 `pw-play`/`pw-cat` 子进程）与鼠标
        // （`g_PointerPosition`，即时输入）**不经过这里**，因此不受影响。
        // 默认 1.0 = 原样赋值（与改动前逐字节一致）。
        let next = advance_scene_time(self.time, self.wall_time, t, time_scale());
        self.wall_time = t;
        self.time_base = next;
        self.t0 = std::time::Instant::now();
        self.time = next;
    }

    /// 把场景时间**精确设到 `t` 并冻结**（固定步长 0）：本帧的 `g_Time` = `t`，
    /// 网格动画也用同一个 `t` ⇒ 渲染结果**逐位可复现**（采样/拼图用）。
    /// 代价：粒子在本帧不推进（`dt = 0`），所以它是"定格"，不是"跑了 t 秒"。
    pub fn set_time_frozen(&mut self, t: f32) {
        self.time_base = t;
        self.t0 = std::time::Instant::now();
        self.time = t;
        self.wall_time = t;
        self.fixed_dt = Some(0.0);
    }

    /// 只覆盖**网格动画**的时间（秒）；`None` = 跟随场景时钟。见字段注释。
    pub fn set_mesh_time(&mut self, t: Option<f32>) {
        self.mesh_time = t;
    }

    /// ── 鼠标输入──────────────────────────────────────────────
    /// 设置当前鼠标位置：**屏幕归一化 [0,1]，y 从屏幕顶往下**（左上角 = `0,0`）。
    /// 这是 WE 的 `g_PointerPosition` 的输入口：
    /// · `effects/xray`（崩坏3 `2564205376` 的"鼠标处透视衣服"）；
    /// · `camera.parallax`（`cameraparallax` + `cameraparallaxmouseinfluence`）的鼠标位移。
    /// 内核在每帧渲染前推一次（控制面 `{"cmd":"mouse","x":..,"y":..}` 只负责把值存下来）；
    /// 离线用 `WE_MOUSE=x,y`（同一口径、同一条路）。
    /// `x/y` 会被夹到 `[0,1]`：WE 那边也是这么做的（`CScene::updateMouse` 里
    /// `glm::clamp`），鼠标跑到别的输出/边缘外时不会把透视区甩到画面外。
    pub fn set_mouse(&mut self, x: f32, y: f32) {
        let nx = x.clamp(0.0, 1.0);
        let ny = y.clamp(0.0, 1.0);
        // ⚠ 这里**不**动 `mouse_last`：它是"**上一帧用过的**位置"（WE 在每帧开头做
        // `m_mousePositionLast = m_mousePosition`），由 `draw_frame` 末尾统一滚动。
        // 若在这里滚动，同一帧内 `g_PointerPositionLast` 会等于 `g_PointerPosition`
        // ⇒ 所有"跟手速度"类效果恒为 0（静默、看不出原因）。
        self.mouse = Some((nx, ny));
    }

    /// 关掉鼠标输入（回到"没有输入"的中性行为：值回落到 `pointer_default`；A/B 与单测用）。
    pub fn clear_mouse(&mut self) {
        self.mouse_last = self.mouse;
        self.mouse = None;
    }

    /// 本帧**真正下发**给 `g_PointerPosition` 的指针位（屏幕归一化，y 从顶往下）。
    /// 有输入 ⇒ 输入值；没有输入 ⇒ `pointer_default`（默认中心 `(0.5,0.5)`，
    /// 见字段注释里的 OWE 出处）。**"没有输入"不等于 0** —— 这一条就是
    /// "开了 X-Ray 却什么也看不到"的根因（`(0,0)` 会把透视区推到贴图角落）。
    pub fn effective_pointer(&self) -> (f32, f32) {
        self.mouse
            .unwrap_or((self.pointer_default[0], self.pointer_default[1]))
    }

    /// 指针在无输入时的默认位（`[0.5,0.5]` 或 `[0,0]`，见 `WE_POINTER_DEFAULT`）。
    pub fn pointer_default(&self) -> [f32; 2] {
        self.pointer_default
    }

    /// 当前鼠标位置（屏幕归一化，y 从顶往下）；`None` = 没有输入。
    pub fn mouse(&self) -> Option<(f32, f32)> {
        self.mouse
    }

    /// 固定步长（秒/帧）：让时钟**确定性**推进，与渲染快慢无关。
    /// CLI 的 `--frames N` 要靠它采样"第 N 帧"的动画/粒子相位（`None` 走墙钟）。
    pub fn set_fixed_dt(&mut self, dt: Option<f32>) {
        self.fixed_dt = dt;
        if dt.is_some() {
            self.t0 = std::time::Instant::now();
        }
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// **场景时刻**（秒，= 下发到 shader 的 `g_Time` 那只钟）。
    /// 给控制面 `status` 用：拿来量「播放速度」到底有没有真的乘上去
    /// （同一段墙钟里 `Δscene_time / Δwall` 就该等于 `time_scale`）。
    pub fn scene_time(&self) -> f32 {
        self.time
    }

    pub fn size(&self) -> (u32, u32) {
        (self.gl.w, self.gl.h)
    }

    /// 是 0 拷贝窗口模式吗（内核据此决定走 `render_window` 还是 `render_to`）
    pub fn is_window(&self) -> bool {
        self.gl.is_window()
    }

    /// 微基准：**单次 `glGetError` 的 CPU 代价**（µs）。
    /// 用来回答"每趟 4 次逐调用查错到底值多少"——分相计时给不出干净答案（会串味），
    /// 只有单独把它跑 N 次才可信。
    pub fn bench_get_error(&self, n: u32) -> (f64, u32) {
        let c0 = cpu_thread_us();
        let mut s = 0u32;
        for _ in 0..n.max(1) {
            s = s.wrapping_add(self.gl.get_error_inner());
        }
        let d = cpu_thread_us().saturating_sub(c0) as f64 / n.max(1) as f64;
        (d, s)
    }

    /// 微基准：把"一趟"拆成**单个 GL 调用**逐个量（µs/次）。回答"一趟十几微秒贵在哪一步"。
    pub fn bench_ops(&self, n: u32) -> Vec<(String, f64)> {
        use glow::HasContext;
        let gl = &self.gl.gl;
        let n = n.max(1);
        let rt = self.rts.first().map(|r| r.fbo);
        let per = |f: &mut dyn FnMut()| -> f64 {
            for _ in 0..2000 {
                f();
            }
            let c0 = cpu_thread_us();
            for _ in 0..n {
                f();
            }
            cpu_thread_us().saturating_sub(c0) as f64 / n as f64
        };
        let mut out: Vec<(String, f64)> = Vec::new();
        unsafe {
            out.push(("glBindFramebuffer(RT)".into(), per(&mut || {
                gl.bind_framebuffer(glow::FRAMEBUFFER, rt);
            })));
            out.push(("glBindFramebuffer(0)".into(), per(&mut || {
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            })));
            out.push(("glViewport".into(), per(&mut || {
                gl.viewport(0, 0, 1920, 1080);
            })));
            out.push(("glDisable+Enable(BLEND)".into(), per(&mut || {
                gl.disable(glow::BLEND);
                gl.enable(glow::BLEND);
            })));
            out.push(("glBlendFuncSeparate".into(), per(&mut || {
                gl.blend_func_separate(
                    glow::SRC_ALPHA,
                    glow::ONE_MINUS_SRC_ALPHA,
                    glow::SRC_ALPHA,
                    glow::ONE_MINUS_SRC_ALPHA,
                );
            })));
            out.push(("glActiveTexture".into(), per(&mut || {
                gl.active_texture(glow::TEXTURE0);
            })));
            out.push(("glBindTexture".into(), per(&mut || {
                gl.bind_texture(glow::TEXTURE_2D, None);
            })));
            out.push(("glGetError".into(), per(&mut || {
                let _ = gl.get_error();
            })));
            out.push(("glUniform1f(inactive loc)".into(), per(&mut || {
                let loc = glow::NativeUniformLocation(4095);
                gl.uniform_1_f32(Some(&loc), 1.0);
            })));
        }
        out
    }

    /// 微基准：一趟里那串"绑目标 + 设状态 + 画"的 GL 调用（不设 uniform），返回单次 µs。
    /// 用来把"GL 调用本身贵不贵"和"我们自己的 Rust 侧开销"分开。
    pub fn bench_state_calls(&self, n: u32) -> f64 {
        let gl = &self.gl.gl;
        let c0 = cpu_thread_us();
        for _ in 0..n.max(1) {
            unsafe {
                use glow::HasContext;
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                gl.viewport(0, 0, self.gl.w as i32, self.gl.h as i32);
                gl.disable(glow::BLEND);
                gl.enable(glow::BLEND);
                gl.blend_func_separate(
                    glow::SRC_ALPHA,
                    glow::ONE_MINUS_SRC_ALPHA,
                    glow::SRC_ALPHA,
                    glow::ONE_MINUS_SRC_ALPHA,
                );
            }
        }
        cpu_thread_us().saturating_sub(c0) as f64 / n.max(1) as f64
    }

    /// 特效链开关：关掉只画底图那一刻（省电档的最大杠杆；粒子也一起停）
    pub fn set_effects(&mut self, on: bool) {
        self.effects_on = on;
    }

    /// **基准**：只跑 `draw_frame`（= 内核每帧 CPU 侧的全部工作），不含回读、不含 `swap`。
    /// 返回 `(帧数, 总毫秒)`。
    /// 两个要点（都踩过）：
///
    /// 1. **量的是进程 CPU 时间**（`CLOCK_PROCESS_CPUTIME_ID`），不是墙钟。背靠背连跑时 GPU 队列
    /// 会堆起来，驱动在命令缓冲满时**阻塞 CPU** ⇒ 墙钟量到的是"被 GPU 拖住的 22ms"，而内核
    /// @30fps 真实只花 4.8ms。只有 CPU 时间与"内核 CPU%"同口径。
///
    /// 2. **按 30fps 节拍**（画完等下一帧）。内核就是那个形态；不节流还会让驱动行为变形。
    pub fn bench_frames(&mut self, n: u32) -> (u32, f64) {
        let n = n.max(1);
        let t0 = std::time::Instant::now();
        // 默认量**渲染线程**；`WE_BENCH_PROC=1` 量**整个进程**（为了和最初那版
        // "进程 CPU 时间"基准数字同口径可比 —— 当时 `--bench` 用的就是进程钟）。
        let proc_clock = std::env::var_os("WE_BENCH_PROC").is_some();
        let now = || if proc_clock { cpu_time_us() } else { cpu_thread_us() };
        let c0 = now();
        for i in 0..n {
            let _ = self.draw_frame();
            self.frames += 1;
            let target = t0 + std::time::Duration::from_micros((i as u64 + 1) * 33333);
            let now = std::time::Instant::now();
            if now < target {
                std::thread::sleep(target - now);
            }
        }
        (n, now().saturating_sub(c0) as f64 / 1000.0)
    }
}

/// 进程 CPU 时间（微秒，含内核态）。与 `/proc/self/stat` 的 utime+stime 同口径，
/// 但分辨率是纳秒（`/proc` 只有 10ms 的 tick，量单帧根本不够）。
pub fn cpu_time_us() -> u64 {
    cpu_clock_us(libc::CLOCK_PROCESS_CPUTIME_ID)
}

/// **本线程**的 CPU 时间（微秒）。`draw_frame` 只跑在渲染线程上，而 `CLOCK_PROCESS_CPUTIME_ID`
/// 会把驱动/Gallium 的后台线程（`sh0..sh11` 着色器编译、`disk$0` 磁盘缓存…）也算进来 ⇒
/// 用线程钟才能真正把"每帧花了多少"和"后台在干什么"分开。
pub fn cpu_thread_us() -> u64 {
    cpu_clock_us(libc::CLOCK_THREAD_CPUTIME_ID)
}

fn cpu_clock_us(which: libc::clockid_t) -> u64 {
    unsafe {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        libc::clock_gettime(which, &mut ts);
        ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
    }
}

/// 每帧分相计时（`WE_FRAME_TIMING=1` 打开；每 60 帧打一行均值）。
/// 为什么要有它：这台机器上没有任何采样工具（`perf` 未安装且 `perf_event_paranoid=3`、
/// 无 root、无 gdb/strace）⇒ "每帧 CPU 花在哪"只能由代码自己回答。全部是 `AtomicU64`
/// 累加微秒，只在渲染线程上写，开销本身可忽略（每帧 7 次 `fetch_add`）。
mod ftime {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    pub fn on() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("WE_FRAME_TIMING").is_ok())
    }

    pub static CLEAR: AtomicU64 = AtomicU64::new(0);
    pub static MESH: AtomicU64 = AtomicU64::new(0);
    pub static PASSES: AtomicU64 = AtomicU64::new(0);
    pub static PART: AtomicU64 = AtomicU64::new(0);
    pub static GLOW: AtomicU64 = AtomicU64::new(0);
    pub static PRESENT: AtomicU64 = AtomicU64::new(0);
    pub static FRAMES: AtomicU64 = AtomicU64::new(0);
    /// 本统计窗口内跑过的趟数（P_* 的除数）
    pub static NPASS: AtomicU64 = AtomicU64::new(0);
    /// 整个 `draw_frame` 的 CPU 时间（用来核对"分相之和 == 总时间"，否则就是漏测了一段）
    pub static TOTAL: AtomicU64 = AtomicU64::new(0);
    // ── 趟内的细分（`WE_PASS_TIMING=1`；回答"一趟 27µs 到底花在哪几步"）──
    pub static P_BIND: AtomicU64 = AtomicU64::new(0);
    pub static P_PROG: AtomicU64 = AtomicU64::new(0);
    pub static P_UNIT: AtomicU64 = AtomicU64::new(0);
    pub static P_SCAL: AtomicU64 = AtomicU64::new(0);
    pub static P_FIXED: AtomicU64 = AtomicU64::new(0);
    pub static P_DRAW: AtomicU64 = AtomicU64::new(0);
    pub static P_ERR: AtomicU64 = AtomicU64::new(0);

    pub fn pass_timing() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var("WE_PASS_TIMING").is_ok())
    }

    /// 记一个阶段的 CPU 耗时（微秒）。入参是 TSC 刻度（`tsc`）。
    pub fn add(x: &AtomicU64, t0: u64) {
        let d = tsc().saturating_sub(t0);
        x.fetch_add((d as f64 / ticks_per_us()) as u64, Relaxed);
    }

    /// 廉价时基：x86 的 `rdtsc`（约 10ns，无系统调用）。
    /// ⚠ 一开始这里用的是 `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)` —— 它**不在 vDSO 里**，
    /// 每次调用都是一次真系统调用（约 1µs）。用它给"每趟 7 段"计时 = 每趟 14 次系统调用，
    /// 113 趟/帧就是 1582 次 ⇒ 光仪表就吃掉 1.6ms，把所有真实数字都盖住了。
    #[inline]
    pub fn tsc() -> u64 {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            core::arch::x86_64::_rdtsc()
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            0
        }
    }

    /// TSC 刻度 → 微秒（启动时用 `CLOCK_MONOTONIC` 标定一次）
    pub fn ticks_per_us() -> f64 {
        static HZ: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
        *HZ.get_or_init(|| {
            let t0 = std::time::Instant::now();
            let c0 = tsc();
            std::thread::sleep(std::time::Duration::from_millis(30));
            let t1 = std::time::Instant::now();
            let c1 = tsc();
            let secs = t1.duration_since(t0).as_secs_f64();
            if secs > 0.0 {
                (c1 - c0) as f64 / secs / 1.0e6
            } else {
                1.0
            }
        })
    }

    pub fn report() {
        let n = FRAMES.load(Relaxed);
        if n == 0 {
            return;
        }
        let g = |x: &AtomicU64| x.load(Relaxed) as f64 / n as f64 / 1000.0; // ms/帧
        let (c, m, p, pa, gl, pr) = (
            g(&CLEAR),
            g(&MESH),
            g(&PASSES),
            g(&PART),
            g(&GLOW),
            g(&PRESENT),
        );
        eprintln!(
            "[每帧计时] {n} 帧均值(ms，CPU时间)：清屏 {c:.2} 网格 {m:.2} 趟链 {p:.2} 粒子 {pa:.2} glow {gl:.2} present {pr:.2} 合计 {:.2} 其它 {:.2}（总 {:.2}）",
            c + m + p + pa + gl + pr,
            g(&TOTAL) - (c + m + p + pa + gl + pr),
            g(&TOTAL)
        );
        if pass_timing() {
            let np = NPASS.load(Relaxed);
            let per = |x: &AtomicU64| x.load(Relaxed) as f64 / np.max(1) as f64 / 1000.0;
            eprintln!(
                "[趟内计时] 每趟(µs，{} 趟)：绑目标 {:.1} 程序/VAO {:.1} 贴图槽 {:.1} 标量 {:.1} 固定量 {:.1} 绘制 {:.1} 查错 {:.1}",
                np,
                per(&P_BIND) * 1000.0,
                per(&P_PROG) * 1000.0,
                per(&P_UNIT) * 1000.0,
                per(&P_SCAL) * 1000.0,
                per(&P_FIXED) * 1000.0,
                per(&P_DRAW) * 1000.0,
                per(&P_ERR) * 1000.0
            );
        }
        for x in [
            &CLEAR, &MESH, &PASSES, &PART, &GLOW, &PRESENT, &P_BIND, &P_PROG, &P_UNIT, &P_SCAL,
            &P_FIXED, &P_DRAW, &P_ERR, &TOTAL,
        ] {
            x.store(0, Relaxed);
        }
        NPASS.store(0, Relaxed);
        FRAMES.store(0, Relaxed);
    }

    /// 一个阶段的 RAII 计时器（`on` 为假时是零开销的空壳）。**记的是进程 CPU 时间**，
    /// 不是墙钟 —— 否则 GPU 阻塞会被算进"趟链"里，看着像 CPU 花了很多。
    pub struct Span(&'static AtomicU64, u64);
    impl Span {
        pub fn new(x: &'static AtomicU64) -> Self {
            Span(x, if on() { tsc() } else { 0 })
        }
        /// 趟内细分用（`WE_PASS_TIMING=1` 才真计时）
        pub fn pass(x: &'static AtomicU64) -> Self {
            Span(x, if pass_timing() { tsc() } else { 0 })
        }
    }
    impl Drop for Span {
        fn drop(&mut self) {
            if self.1 != 0 {
                add(self.0, self.1);
            }
        }
    }
}

/// 注解声明类型里属于**本开关作用范围**的那些（`WE_FX_DEFAULT_VEC`）。
/// 只有 `vec2/vec3/vec4/mat3/mat4` —— 判据是**数据**（官方库里出现过的向量族声明类型）
/// 外加**我们有没有能力下发**：
///
/// * `ivec*`/`bvec*`：官方库 **0 处**（只读扫描 `assets/**` 342 处向量型注解 default：
/// `vec2` 223 / `vec3` 106 / `vec4` 13），且 `Value` 只有浮点向量变体 ⇒ 不主张。
///
/// * `mat2`：官方库 **0 处**，且 `Value` 没有 2×2 变体（只有 `M3`/`M4`）、`set_uniform`
/// 也没有对应分支 ⇒ 不主张（真要支持得先扩 `Value` + `set_uniform`）。
fn ann_vec_ty(ty: &str) -> bool {
    matches!(ty, "vec2" | "vec3" | "vec4" | "mat3" | "mat4")
}

/// 官方注解 `default` 字面量 → 我们下发的 [`Value`]（`WE_FX_DEFAULT_VEC` 用）。
///
/// ## 出处（切分规则**逐字**照官方，见 `gl_ctx::fx_default_vec_on` 的出处③）
///
/// * `open-wallpaper-engine/src/Core/Json.cpp:88-114` 的 `ArrayComponents`：
/// 字符串**含 `,`** ⇒ 按 `,` 切且每段 `trim_ascii`；**否则按空格切**。
/// （官方是 `split_once(" ")`：连续空格会切出空段 ⇒ 数字解析失败 ⇒ 整条作废；
/// 我们过滤空段 = 更宽容，官方库 0 处可观测到该差异。）
///
/// * `:154-172` 数组分支：`default` 是 **JSON 数组**也合法。`shader::parse_shader_meta`
/// 对非字符串 default 走 `other.to_string` ⇒ 会变成 `"[1,1,1,1]"` ⇒ 这里剥方括号。
///
/// * `:143-151` 数字分支：裸数字 ⇒ 单分量（对向量声明 = 分量数不符 ⇒ 我们跳过，见下）。
///
/// ## 判据（严格，宁可不给也不猜）
///
/// * 必须切出**恰好 N 个**（N = 该类型的分量数）**且全部解析成 f32**；否则 `None`。
/// ⇒ `"1 1 1"` 给 `vec4`、`""`、纹理名 `"util/white"`、`"0.5"` 给 `vec2` **全部跳过**。
/// （官方库里唯一的"分量数不符"是 `shaders/editorpaintbrush.frag:7/9`：声明 `vec4` 但
/// default 是 `"0 0"` / `"0 0 0"` —— 它自己的官方值也就 ≈0，跳过 ⇒ 与现状同效；
/// 官方对"写不满的槽位怎么处理"落在未 vendor 的 `vrento::UniformValue` 里 ⇒ **无法判定**。）
///
/// * `mat` 族按**行主序**填：`Value::M4/M3` 的既有口径就是"行主序存放"——
/// `set_uniform` 之后还会再走一次 `t4`/`t3`（见 `gl_ctx::t4` 的长注释），两次下来
/// GL 里的矩阵正好等于字面量写的那个（`m[r*N+c]` = 第 r 行第 c 列）。
/// ⚠ 官方库 **0 处** `mat` 型注解 default ⇒ 这一支**没有真数据可验**，只有单测。
fn ann_vec_default(lit: &str, ty: &str) -> Option<Value> {
    let want = match ty {
        "vec2" => 2,
        "vec3" => 3,
        "vec4" => 4,
        "mat3" => 9,
        "mat4" => 16,
        _ => return None,
    };
    let s = lit.trim();
    // JSON 数组字面量（`parse_shader_meta` 把 `"default":[1,1]` 变成 `"[1,1]"`）
    let body = s
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .unwrap_or(s);
    let toks: Vec<&str> = if body.contains(',') {
        body.split(',').map(str::trim).collect()
    } else {
        body.split_whitespace().collect()
    };
    if toks.len() != want {
        return None;
    }
    let nums: Vec<f32> = toks.iter().map(|t| t.parse::<f32>().ok()).collect::<Option<_>>()?;
    Some(match ty {
        "vec2" => Value::V2(nums[0], nums[1]),
        "vec3" => Value::V3(nums[0], nums[1], nums[2]),
        "vec4" => Value::V4(nums[0], nums[1], nums[2], nums[3]),
        "mat3" => {
            let mut m = [0.0f32; 9];
            m.copy_from_slice(&nums[..9]);
            Value::M3(m)
        }
        _ => {
            let mut m = [0.0f32; 16];
            m.copy_from_slice(&nums[..16]);
            Value::M4(m)
        }
    })
}

/// 把解析出来的常量**按 uniform 的声明类型对齐分量数**。
/// 为什么：`set_uniform` 是按 `Value` 的变体选 GL 调用的（`Value::F` ⇒ `glUniform1f`）。
/// 值一旦被解析成标量而 uniform 声明是 `vec2`，GL 直接 `GL_INVALID_OPERATION`
/// （实测 `u_AASmoothness`：`has 2 components, not 1`）⇒ **整帧渲染失败**。
/// 少的分量补 0.0（GLSL 未写分量就是 0）、多的截断；矩阵/sampler 不动。
fn coerce_to_decl(val: Value, ty: &str) -> Value {
    let want: Option<usize> = match ty {
        "float" | "int" => Some(1),
        "vec2" => Some(2),
        "vec3" => Some(3),
        "vec4" => Some(4),
        _ => None,
    };
    let Some(w) = want else { return val };
    let comps: Vec<f32> = match val {
        Value::F(x) => vec![x],
        Value::I(x) => vec![x as f32],
        Value::V2(a, b) => vec![a, b],
        Value::V3(a, b, c) => vec![a, b, c],
        Value::V4(a, b, c, d) => vec![a, b, c, d],
        other => return other,
    };
    // `int` uniform 收到浮点值也必须转成 `glUniform1i`（否则同样是 INVALID_OPERATION）
    if ty == "int" && comps.len() == 1 {
        return match val {
            Value::I(x) => Value::I(x),
            _ => Value::I(comps[0] as i32),
        };
    }
    if comps.len() == w {
        return val;
    }
    if std::env::var_os("WE_DEBUG_PASSES").is_some() {
        eprintln!(
            "[常量] 分量数按声明类型对齐：值 {} 个分量 → {ty}（{} 个），补 0/截断",
            comps.len(),
            w
        );
    }
    let mut out = comps;
    out.resize(w, 0.0);
    match w {
        1 => {
            if ty == "int" {
                Value::I(out[0] as i32)
            } else {
                Value::F(out[0])
            }
        }
        2 => Value::V2(out[0], out[1]),
        3 => Value::V3(out[0], out[1], out[2]),
        _ => Value::V4(out[0], out[1], out[2], out[3]),
    }
}

/// `WE_MOUSE=x,y` 的解析（屏幕归一化，y 从屏幕顶往下）。
/// 抽成纯函数是为了能单测：**两个分量都在**才认（少一个就忽略，别拿 0 去补 —— 那会让
/// 透视区跑到边上）；不做夹取（夹取在 `set_mouse` 入口统一做，内核走控制面时也是那一条）。
fn parse_mouse_spec(spec: &str) -> Option<(f32, f32)> {
    let parts: Vec<f32> = spec
        .split(',')
        .filter_map(|t| t.trim().parse::<f32>().ok())
        .collect();
    if parts.len() == 2 {
        Some((parts[0], parts[1]))
    } else {
        None
    }
}

/// **没有指针输入时** `g_PointerPosition` 取什么（官方语义见 `pointer_default` 字段注释）。
///
/// * 默认 / 任何别的值 ⇒ **屏幕中心 `(0.5,0.5)`** —— 与 OWE 的 Scene 成员初始化一致
/// （`open-wallpaper-engine/src/Scene/Scene/Scene.cppm:2872`：`{ 0.5f, 0.5f }`）。
///
/// * `WE_POINTER_DEFAULT=zero`（也认 `0`/`off`）⇒ `(0,0)`：**加这一路之前的旧口径**
/// （GL 里没人写过这个 uniform 时的值），只给 A/B 用。
/// 为什么默认必须是中心：`(0,0)` 会让 `effects/xray` 的采样点落到贴图角落
/// ⇒ 精灵 alpha ≈ 0 ⇒ `blend ≈ 0` ⇒ **整层零变化**（"开了 X-Ray 什么也没发生"）。
/// `WE_POINTER=0` 则整条指针注入都关掉（等价于旧口径，连 uniform 都不下发）。
fn pointer_default_from_env() -> [f32; 2] {
    let raw = std::env::var("WE_POINTER_DEFAULT").unwrap_or_default();
    match raw.trim().to_ascii_lowercase().as_str() {
        "zero" | "0" | "off" | "false" => [0.0, 0.0],
        _ => [0.5, 0.5],
    }
}

/// **开关**：特效趟常量里的脚本绑定是否逐帧求值。
/// 只在 `propscript::enabled`（= `WE_SCRIPT_AUDIO=1`）时才有内容，所以默认档逐字节不变；
/// `WE_FX_CONST_SCRIPT=0` 单独关这一支（与 rate 脚本分开做 A/B）。
fn fx_const_script_on() -> bool {
    propscript::enabled()
        && std::env::var("WE_FX_CONST_SCRIPT").ok().as_deref() != Some("0")
}

/// 把一个常量值里可能的**脚本绑定**装成 [`propscript::NumScript`]。
/// `{"script": "...", "value": 0.205}` ⇒ 初值 0.205、每帧 `update` 的返回值。
fn fx_const_script(v: &serde_json::Value) -> Option<propscript::NumScript> {
    let o = v.as_object()?;
    let script = o.get("script")?.as_str()?;
    let init = o
        .get("value")
        .and_then(json_to_value)
        .and_then(|x| match x {
            Value::F(f) => Some(f),
            _ => None,
        })
        .unwrap_or(1.0);
    match propscript::NumScript::new(script, init, o.get("scriptproperties")) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("[脚本] 常量脚本装载失败（退回静态 value）：{e}");
            None
        }
    }
}

/// JSON → uniform 值（"1 1 1" 这种字符串当向量，数字当标量）
fn json_to_value(v: &serde_json::Value) -> Option<Value> {
    match v {
        serde_json::Value::Number(n) => n.as_f64().map(|f| Value::F(f as f32)),
        serde_json::Value::Bool(b) => Some(Value::I(*b as i32)),
        serde_json::Value::String(s) => {
            // 向量字符串有两种写法：`"1 1 1"`（空格）与 `"0.02, 0.02"`（**逗号 + 空格**，
            // WE 编辑器对 vec2 就写这种）。只按空格切会把 `"0.02,"` 解析失败 ⇒ 只剩一个分量
            // ⇒ 当成标量下发 ⇒ `glUniform1f` 打到 vec2 uniform ⇒
            // `GL_INVALID_OPERATION`（实测 3704273480 主题2 的 `u_AASmoothness`：
            // 报 `u_AASmoothness@6 has 2 components, not 1`，整帧渲染失败）。
            let parts: Vec<f32> = s
                .split(|c: char| c.is_whitespace() || c == ',')
                .filter(|p| !p.is_empty())
                .filter_map(|p| p.parse::<f32>().ok())
                .collect();
            match parts.len() {
                1 => Some(Value::F(parts[0])),
                2 => Some(Value::V2(parts[0], parts[1])),
                3 => Some(Value::V3(parts[0], parts[1], parts[2])),
                4 => Some(Value::V4(parts[0], parts[1], parts[2], parts[3])),
                _ => None,
            }
        }
        // **"用户可绑定"的常量**：WE 的场景常量有两种形态 ——
        // 直接值 `1`，或绑定对象 `{"user":"xraysize","value":0.5}`（还有 `{"script":…,"value":…}`）。
        // 以前这里只认标量 ⇒ 绑定形态**静默丢掉**该 uniform ⇒ shader 拿到 0。
        // 实测（崩坏3 `2564205376` 的 X-Ray）：`size` 被丢 ⇒ `g_PointerScale=0`
        // ⇒ `v_PointerScale = mix(999, …, step(0.001,0)) = 999` ⇒ 精灵被放大 999 倍
        // ⇒ 透视区**完全消失**（而且不报任何错）。
        // 语义与 `visible` 的 `bound::*` 一致：**先查用户属性覆盖，再退回它声明的 `value`**
        // （⇒ 界面上拖 `xraysize` 滑杆 / `--prop xraysize=0.8` 立刻生效）。
        serde_json::Value::Object(m) => {
            if let Some(u) = m.get("user").and_then(|u| u.as_str()) {
                if let Some(v) = we_scene::scene::userprop::get(u) {
                    if let Some(val) = json_to_value(&v) {
                        return Some(val);
                    }
                }
            }
            m.get("value").and_then(json_to_value)
        }
        _ => None,
    }
}

/// 兜底粒子贴图：64x64 的柔光点（中心白到边缘透明）
fn soft_dot() -> (Vec<u8>, u32, u32) {
    let n = 64u32;
    let mut px = vec![0u8; (n * n * 4) as usize];
    let c = (n as f32 - 1.0) / 2.0;
    for y in 0..n {
        for x in 0..n {
            let dx = (x as f32 - c) / c;
            let dy = (y as f32 - c) / c;
            let d = (dx * dx + dy * dy).sqrt();
            let a = (1.0 - d).clamp(0.0, 1.0);
            let a = a * a; // 柔一点
            let i = ((y * n + x) * 4) as usize;
            px[i] = 255;
            px[i + 1] = 255;
            px[i + 2] = 255;
            px[i + 3] = (a * 255.0) as u8;
        }
    }
    (px, n, n)
}

/// 取景模式（画布 → 屏幕的映射方式）。
///
/// * `Cover`（默认，等价于以前唯一的行为）：铺满屏幕，裁掉多出来的那条边。
///
/// * `Stretch`：整张画布拉伸到屏幕（会变形）。
///
/// * `Center`：画布按 **1:1 像素** 居中显示（不缩放），四周可能留黑。
///
/// * `Contain`：整张画布缩进屏幕（不变形），多出来的方向留黑边。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FitMode {
    Cover,
    Stretch,
    Center,
    Contain,
}

impl FitMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cover" | "覆盖" => Some(Self::Cover),
            "stretch" | "拉伸" => Some(Self::Stretch),
            "center" | "居中" | "centre" => Some(Self::Center),
            "contain" | "包含" | "适应" => Some(Self::Contain),
            _ => None,
        }
    }

    /// 环境变量旋钮（命令行/调试用）：`WE_FIT=cover|stretch|center|contain`
    pub fn from_env() -> Self {
        std::env::var("WE_FIT")
            .ok()
            .and_then(|v| Self::parse(&v))
            .unwrap_or(Self::Cover)
    }

    /// 进/出 `AtomicU8`（内核运行时通过 `set_global_fit` 设置）
    pub fn to_u8(self) -> u8 {
        match self {
            Self::Cover => 0,
            Self::Stretch => 1,
            Self::Center => 2,
            Self::Contain => 3,
        }
    }

    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Stretch,
            2 => Self::Center,
            3 => Self::Contain,
            _ => Self::Cover,
        }
    }
}

// ── 播放速度（场景时间倍率；`set time_scale=…`）────────────────────────────────
// 用户要的「播放速度」是**新能力**：逆向确认 win 的 `rate` 只作用于视频，场景本身没有变速
// 。这里的口径是**只乘场景自己的时间**：
// · 乘在 [`SceneRenderer::advance_clock`] 推进 `self.time` 的那一步 ⇒ `g_Time`、
// 粒子 `dt`、网格动画、场景灯**一起**按倍率走（这才是"播放速度"）；
// · **不碰**音频（音频是独立 `pw-play`/`pw-cat` 子进程，按自己的时钟播）、不碰鼠标
// （`g_PointerPosition` 是即时输入，与时间无关）。
// 默认 1.0 ⇒ 推进式与改动前**逐字节**一致（`prev + d * 1.0 == raw`，见单测）。
// 范围与界面滑杆一致：0.25× ~ 3.0×（越界在这里再夹一次，UI 传错也不会把场景冻住/飙飞）。
/// 播放速度倍率（`f32` 位模式存进原子量；默认 1.0 = 官方口径）。
static TIME_SCALE: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(1.0f32.to_bits());

/// 播放速度的合法范围（界面滑杆同口径）。
pub const TIME_SCALE_MIN: f32 = 0.25;
pub const TIME_SCALE_MAX: f32 = 3.0;

/// 设播放速度倍率（内核的 `set time_scale=…` 用它）；越界夹到 [`TIME_SCALE_MIN`]..=[`TIME_SCALE_MAX`]。
/// 返回**实际生效**的值（回执/日志用这个，不回声请求值）。
pub fn set_time_scale(scale: f32) -> f32 {
    let s = if scale.is_finite() {
        scale.clamp(TIME_SCALE_MIN, TIME_SCALE_MAX)
    } else {
        1.0
    };
    TIME_SCALE.store(s.to_bits(), std::sync::atomic::Ordering::Relaxed);
    s
}

/// 当前播放速度倍率。
pub fn time_scale() -> f32 {
    f32::from_bits(TIME_SCALE.load(std::sync::atomic::Ordering::Relaxed))
}

/// **纯函数**：把内核喂进来的**墙钟时刻**换算成**场景时刻**（播放速度的唯一落点）。
///
/// * `scale == 1.0` ⇒ 原样返回 `wall`（默认路径逐字节不变，也不引入任何浮点误差；
/// CLI 的 `--t` 绝对定位照旧是"给几秒就是几秒"）；
///
/// * 否则按**增量**累加：`prev_scene + clamp(wall - prev_wall, 0, 0.25) * scale`。
/// · 增量夹到 0.25 s 与粒子步长同口径 ⇒ 挡屏睡眠/暂停醒来那一下不会把场景时间
/// 一口吞掉一大段（乘 3.0 更明显）；
/// · 累加而不是乘绝对时刻 ⇒ 拖动速度滑杆的那一帧连续，画面不会往回跳。
pub fn advance_scene_time(prev_scene: f32, prev_wall: f32, wall: f32, scale: f32) -> f32 {
    if !(scale.is_finite() && scale > 0.0) || (scale - 1.0).abs() < f32::EPSILON {
        return wall;
    }
    let d = (wall - prev_wall).clamp(0.0, 0.25);
    prev_scene + d * scale
}

/// 运行时覆盖的取景模式（`255` = 没设过，回落到 `WE_FIT` 环境变量）。
/// 为什么用全局而不是构造参数：`SceneRenderer::open/open_window` 的签名被内核和命令行两边用着，
/// 加参数会牵动一堆调用点；而"取景模式"本来就是**进程级**的显示策略，全局正好。
/// 改完之后**必须重建渲染器**（投影矩阵是 `build` 时烘进每一趟的）。
static FIT_OVERRIDE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(255);

/// 设置全局取景模式（内核的 `set fit=…` 用它）。改完请重建渲染器：`self.scene = None`。
pub fn set_global_fit(mode: FitMode) {
    FIT_OVERRIDE.store(mode.to_u8(), std::sync::atomic::Ordering::Relaxed);
}

/// 当前生效的取景模式：显式设置过就用它，否则看 `WE_FIT`。
pub fn current_fit() -> FitMode {
    match FIT_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed) {
        255 => FitMode::from_env(),
        v => FitMode::from_u8(v),
    }
}

/// 色彩调整：`[亮度, 对比度, 饱和度, 色相(弧度), 色温, 伽马]`，单位值 = `[1,1,1,0,0,1]`。
/// 为什么不做成 uniform：这趟 shader 是**最后上屏**用的，改一次只重建一个全屏层（不做场景重建），
/// 所以直接烘成常数最省事；而且**单位值时用的是原来那条 shader**（逐字节不变）⇒ 没调色的用户
/// 一分钱成本都不付、也绝不会有回归。
/// ⚠ 色相矩阵的方向**别靠肉眼看大角度截图判断**。我在这上面栽过一次：蓝色调 +90° 看起来
/// 变成了橙（像是转反了），于是把 `sin` 取负 —— 用**逐像素代数模型**复核后才发现原来是对的，
/// 于是撤回。真正的判据：**用小角度（20°，此时 clamp 不介入、线性成立）**看色相往哪走 ——
/// `+20°` ⇒ 220°→282°（色轮正方向，蓝→紫）✓。大角度（≥90°）会因为 `clamp(0,1)` 把颜色压到
/// 边界，均值不再有意义（我最早的"通道均值"判断就是这么被骗的）。
/// 另：这条矩阵绕**亮度轴**旋转，和 HSV 色轮不是同一个参数化 ⇒ 色相滑杆的"度数"不是精确的
/// HSV 旋转（90° 会过冲到红）⇒ 它就是个**观感旋钮**，别当配色工具用。
static COLOR: std::sync::Mutex<[f32; 6]> = std::sync::Mutex::new([1.0, 1.0, 1.0, 0.0, 0.0, 1.0]);

/// 色彩调整的单位值（= 什么都不做）
pub const COLOR_IDENTITY: [f32; 6] = [1.0, 1.0, 1.0, 0.0, 0.0, 1.0];

/// 这组色彩参数是不是等于"什么都不做"（浮点比较，容差 1e-4）
pub fn color_is_identity(c: &[f32; 6]) -> bool {
    c.iter()
        .zip(COLOR_IDENTITY.iter())
        .all(|(a, b)| (a - b).abs() < 1e-4)
}

/// 设置全局色彩调整（内核 `set color=…` 用它）。改完请重建渲染器 —— 不过只是重做一个全屏层，
/// 比场景重建（~0.3s）快得多。
pub fn set_global_color(c: [f32; 6]) {
    if let Ok(mut g) = COLOR.lock() {
        *g = c;
    }
}

/// 当前生效的色彩调整。
pub fn current_color() -> [f32; 6] {
    COLOR.lock().map(|g| *g).unwrap_or(COLOR_IDENTITY)
}

// ── 用户属性覆盖表（壁纸自己的 `project.json general.properties` 是定义，这里是覆盖）──
// 壁纸作者在自己的 `project.json` 里声明"用户属性"（`general.properties.<名> = {type, value, text}`），
// 场景里用 `"visible": {"user":"<名>","value":<默认值>}` 把某个**对象/特效**的显隐绑上去。
// 用户在 WE 界面里改属性 ⇒ 绑定的对象/特效跟着显隐。
// 我们复现"用户改了属性"的那张覆盖表住在 `we_scene::scene::userprop`（惰性读 `WE_PROP_<名>` 环境变量）。
// 这里是**内核侧的入口**：环境变量只在进程启动时读一次，而内核常驻 ⇒ 必须能**运行时**写表。
// 内核的 `set-prop` 控制面命令最终走到下面三个函数。
// ⚠ 覆盖表读的是**建场景时**的 `Visibility::resolve`（`build` 里逐对象/逐特效）⇒ 改完
// **必须重建渲染器**（和 `set_global_fit` / `set_global_color` 同一条规矩）。否则你只是改了表、
// 画面纹丝不动 —— 那正是"改参数没反应"的老毛病。

/// 覆盖一个用户属性（名字与值都原样透传，**不认具体属性名**）。
/// 值按 `userprop::parse_value` 解释：`true/false/on/off/yes/no` ⇒ 布尔，整数/浮点 ⇒ 数字，
/// 其余 ⇒ 字符串。属性的**类型**由壁纸的 `project.json` 决定，渲染层不预设。
pub fn set_user_prop(name: &str, value: &str) {
    we_scene::scene::userprop::set(name, we_scene::scene::userprop::parse_value(value));
}

/// 清空整张用户属性覆盖表（切壁纸时用，防上一条的属性名撞上新壁纸）。
pub fn clear_user_props() {
    we_scene::scene::userprop::clear();
}

/// 当前生效的覆盖表（键值都转成字符串；诊断/控制面回执用）。
pub fn user_props() -> Vec<(String, String)> {
    we_scene::scene::userprop::all()
        .into_iter()
        .map(|(k, v)| {
            let s = match &v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            (k, s)
        })
        .collect()
}

/// **曝光 / 色调映射**（present 趟的最后一步，`WE_EXPOSURE` / `WE_TONEMAP`）。
/// 为什么有这一步：Ice Princess 上我们与 linux-wallpaperengine 的参照帧**逐区域一致**
/// （全图均值 66.5/72.8/76.3 vs 67.1/73.2/76.4），而 Windows 预览（`1.jpg`/`2.jpg`，
/// 同一台 2560x1440 桌面）整幅亮一档、也更暖 —— 两个独立实现一致、只有官方引擎更亮
/// ⇒ 差的是**引擎内置的 HDR→SDR 输出变换**，它的 shader 不在 `assets/` 里（`assets/`
/// 里只有 HDR 显示通路用的 `combine_hdr` / `passthroughlinear`，且本场景 `general.hdr=false`），
/// 无法从资源包复刻 ⇒ 这里按**标定**补一条等价曲线。
/// 标定：
/// 在「保持 gain=1，只做显示编码 `c^(1/gamma)`」这一族里，**gamma≈1.30** 同时最贴
/// `1.jpg` 与 `2.jpg` 的**全图均值 + 亮度分位**：
/// ```text
/// 均值 p10 p25 p50 p75 p90 p95
/// 我们（原样） 68.0 6 11 24 47 99 163 198
/// gamma 1.30 86.4 14 23 41 69 123 181 210
/// Windows 1.jpg 88.9 17 22 37 76 123 179 234
/// Windows 2.jpg 84.4 14 19 31 72 120 173 223
/// ```
/// 全族误差：gamma 1.30 = 0.0226 / 1.37 = 0.0238 / 1.20 = 0.0381 / 纯 gain 1.30 = 0.037
/// ⇒ **曲线不是直线**（纯增益拟合不出来），gamma 1.30 是最优点。
/// **默认关**（`gain=1, gamma=1` ⇒ `is_identity` ⇒ present 趟仍是**逐字节**的老 blit，
/// 六张 `scene_score` 与内核行为一律不变）。要 Windows 那种观感就 `WE_EXPOSURE=1.0,1.30`。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExposureCfg {
    /// 线性曝光增益
    pub gain: f32,
    /// 显示编码 gamma：输出 `= gain * c^(1/gamma)`；1.0 = 不编码
    pub gamma: f32,
    /// 色温（正=暖）：`R *= 1+0.10w`、`B *= 1-0.10w`（与 `WE_COLOR` 的色温同一约定）
    pub warmth: f32,
    /// `true` 时先做 `c/(1+c/k)`（k = gain）高光压缩再编码
    pub reinhard: bool,
}

impl ExposureCfg {
    pub const IDENTITY: ExposureCfg = ExposureCfg {
        gain: 1.0,
        gamma: 1.0,
        warmth: 0.0,
        reinhard: false,
    };

    pub fn is_identity(&self) -> bool {
        (self.gain - 1.0).abs() < 1e-4
            && (self.gamma - 1.0).abs() < 1e-4
            && self.warmth.abs() < 1e-4
            && !self.reinhard
    }

    /// `WE_EXPOSURE=<gain>[,<gamma>][,<warmth>][,reinhard]`（`WE_TONEMAP` 同名同义）。
    /// 例：`1.0,1.30,0.15`（Ice Princess 标定值）、`1.2`（只加曝光）、`1.0,1.3,0,reinhard`。
    pub fn from_env() -> ExposureCfg {
        let s = match std::env::var("WE_EXPOSURE").or_else(|_| std::env::var("WE_TONEMAP")) {
            Ok(s) if !s.is_empty() => s,
            _ => return ExposureCfg::IDENTITY,
        };
        let parts: Vec<&str> = s.split(',').map(|t| t.trim()).collect();
        let mut c = ExposureCfg::IDENTITY;
        if let Some(v) = parts.first().and_then(|p| p.parse::<f32>().ok()) {
            if v > 0.0 {
                c.gain = v;
            }
        }
        if let Some(v) = parts.get(1).and_then(|p| p.parse::<f32>().ok()) {
            if v > 0.0 {
                c.gamma = v;
            }
        }
        if let Some(v) = parts.get(2).and_then(|p| p.parse::<f32>().ok()) {
            c.warmth = v;
        }
        if parts.iter().any(|p| p.eq_ignore_ascii_case("reinhard")) {
            c.reinhard = true;
        }
        c
    }

    /// 作用在着色器里某个 `vec3` 上的 GLSL（单位值 ⇒ 空串）。
    pub fn glsl_for(&self, v: &str) -> String {
        if self.is_identity() {
            return String::new();
        }
        let mut s = String::new();
        if self.reinhard {
            let k = self.gain;
            s.push_str(&format!(
                "  {v} = {v} / (1.0 + {v} / {k:.5});\n  {v} *= {:.5};\n",
                1.0 + 1.0 / k
            ));
        } else if (self.gain - 1.0).abs() >= 1e-4 {
            s.push_str(&format!("  {v} *= {:.5};\n", self.gain));
        }
        if self.warmth.abs() >= 1e-4 {
            s.push_str(&format!(
                "  {v}.r *= {:.5};\n  {v}.b *= {:.5};\n",
                1.0 + self.warmth * 0.10,
                1.0 - self.warmth * 0.10
            ));
        }
        s.push_str(&format!(
            "  {v} = pow(max({v}, vec3(0.0)), vec3({:.5}));\n",
            1.0 / self.gamma
        ));
        s
    }

    /// `glsl_for("c")` 的简写（present blit 里那个 `vec3 c`）。
    pub fn glsl(&self) -> String {
        self.glsl_for("c")
    }
}


/// 画布里"可见的那块矩形"（场景单位）—— 由取景模式决定。
/// `Cover` 就是以前的 `cover_rect`：铺满屏幕、裁掉长边。其余模式让用户能选
/// 拉伸/居中/留黑边（以前只有 cover，硬编码）。
fn fit_rect(sw: f32, sh: f32, w: f32, h: f32, mode: FitMode) -> (f32, f32, f32, f32) {
    let (want, have) = (w / h, sw / sh);
    match mode {
        // 整张画布铺到屏幕上（不裁不填）
        FitMode::Stretch => (0.0, 0.0, sw, sh),
        // 1:1 像素：可见区就是屏幕那么大，围绕画布中心
        FitMode::Center => {
            let (cx, cy) = (sw / 2.0, sh / 2.0);
            (cx - w / 2.0, cy - h / 2.0, cx + w / 2.0, cy + h / 2.0)
        }
        // 整张画布缩进屏幕：可见区 = 画布按屏幕比例"外扩"（留黑边）
        FitMode::Contain => {
            if have >= want {
                // 画布更宽 ⇒ 竖直方向留黑边
                let vis = sw / want;
                let y0 = (sh - vis) / 2.0;
                (0.0, y0, sw, y0 + vis)
            } else {
                let vis = sh * want;
                let x0 = (sw - vis) / 2.0;
                (x0, 0.0, x0 + vis, sh)
            }
        }
        // 铺满：裁掉长边（= 老的 cover_rect）
        FitMode::Cover => {
            if have >= want {
                let vis = sh * want;
                let x0 = (sw - vis) / 2.0;
                (x0, 0.0, x0 + vis, sh)
            } else {
                let vis = sw / want;
                let y0 = (sh - vis) / 2.0;
                (0.0, y0, sw, y0 + vis)
            }
        }
    }
}

/// 兼容旧名字（单测与历史调用仍用它）：铺满 + 居中裁长边。
fn cover_rect(sw: f32, sh: f32, w: f32, h: f32) -> (f32, f32, f32, f32) {
    fit_rect(sw, sh, w, h, FitMode::Cover)
}

/// 回读缓冲 → 目标：翻转 Y + 可选 RGBA→BGRA 交换，一趟做完
/// GL readback is bottom-row-first; PNG wants top-row-first. Flip in place, no alloc.
fn flip_rows_in_place(buf: &mut [u8], w: u32, h: u32) {
    let row = (w * 4) as usize;
    let h = h as usize;
    for y in 0..h / 2 {
        let (a, b) = (y * row, (h - 1 - y) * row);
        for i in 0..row {
            buf.swap(a + i, b + i);
        }
    }
}

fn readback_into(src: &mut [u8], dst: &mut [u8], w: u32, h: u32, swizzle: bool) {
    let row = (w * 4) as usize;
    for y in 0..h as usize {
        let s = &src[(h as usize - 1 - y) * row..(h as usize - y) * row];
        let d = &mut dst[y * row..(y + 1) * row];
        if swizzle {
            for x in 0..(w as usize) {
                d[x * 4] = s[x * 4 + 2];
                d[x * 4 + 1] = s[x * 4 + 1];
                d[x * 4 + 2] = s[x * 4];
                d[x * 4 + 3] = s[x * 4 + 3];
            }
        } else {
            d.copy_from_slice(s);
        }
    }
}

/// 自动判定阈值：网格几何与"那张四边形"的偏差超过它，才切到网格路径（见 [`mesh_geometry_dev`]）。
/// 实测双峰（≈0 群 vs ≥0.277 群），0.15 落在空档里。
const MESH_MIN_DEV: f32 = 0.15;

/// 网格几何相对“那张四边形”的偏差（归一化 = 对象尺寸的比例）。**自动判定的依据**。
/// 平面（sprite）路径画的是**对象矩形的贴图四边形**；网格路径画的是模型几何。两条路只有在
/// 网格几何**确实不是**那张四边形时才会有差别（否则网格就是"三角形化的贴图矩形"，换过去
/// 只会改变混合/三角化缝隙/图层空间链的语义，不会多出任何信息）。本函数量化这个差别：
///
/// * `bind 顶点` 偏离 `((u−0.5)·sizeX, −(v−0.5)·sizeY)`（= 贴图布局）的最大残差；
///
/// * `装配位移`：bind → 装配后（MDLSv2 蒙皮 + animationlayers 姿态）的最大逐顶点位移。
/// 实测分布（本机三张壁纸，归一化）：
/// ```
/// 2887537617 底层1 .572 底层2 .277 底层4 .418 tou .624 | 头发1 .001 B沙发 .000
/// 3704273480 身体---拆分 .746（bind 本身已是装配姿态，残差 .746）
/// 2861113973 девка/плащ/рука*/хало .000 люстра .000/.112
/// ```
/// ⇒ 明显双峰：`≈0` 的一群（网格 = 四边形）与 `≥0.277` 的一群（网格携带真几何）。
/// 阈值取 **0.15**（落在 0.112 与 0.277 之间的空档），`≈0` 的那群留在平面路径上
/// （这正是 Ice Princess 既有评分 7.5470 的来源：它的 puppet 几何与四边形等价）。
fn mesh_geometry_dev(m: &mdl::PuppetMesh, size: [f32; 2]) -> f32 {
    let sx = if size[0].abs() > 1e-3 { size[0] } else { 1.0 };
    let sy = if size[1].abs() > 1e-3 { size[1] } else { 1.0 };
    let scale = sx.abs().max(sy.abs());
    let mut d = 0.0f32;
    for (i, p) in m.positions.iter().enumerate() {
        if let Some(uv) = m.uvs.get(i) {
            let ex = (uv[0] - 0.5) * sx;
            let ey = -(uv[1] - 0.5) * sy;
            d = d.max((p[0] - ex).abs()).max((p[1] - ey).abs());
        }
    }
    if let Some(a) = m.assembled.as_ref() {
        for (i, p) in m.positions.iter().enumerate() {
            if let Some(q) = a.get(i) {
                d = d.max(((q[0] - p[0]).powi(2) + (q[1] - p[1]).powi(2)).sqrt());
            }
        }
    }
    d / scale
}

/// 一个 puppet 网格在**图层归一化坐标**下的包围盒 `[x0, x1, y0, y1]`。
/// 归一化方式与建层时完全一致（`x / (size.x/2)`、`y / (size.y/2)` 再叠 `dy`；`flip_y` 时先翻 y），
/// 所以它的含义就是\"这些顶点会被画进缓冲的哪一块\"。装配后的网格常常超出 `±1`（对象矩形），
/// 超出的部分会被图层缓冲硬裁成一条直线 —— 取景框要用它把缓冲放大到装得下。
/// 经验值（本机 2887537617、`WE_MESH_DY=-197`）：`底层1` y ∈ [−1.234, 0.425]、
/// `底层2` y 低到 −1.139、`B沙发` 低到 −1.513 ⇒ 都被裁过。
fn mesh_bbox_norm(m: &mdl::PuppetMesh, size: [f32; 2], dy: f32, flip_y: bool) -> [f32; 4] {
    let hw = if size[0].abs() > 1e-3 { size[0] * 0.5 } else { 0.5 };
    let hh = if size[1].abs() > 1e-3 { size[1] * 0.5 } else { 0.5 };
    let (mut x0, mut x1, mut y0, mut y1) = (
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
    );
    for p in m.geometry() {
        let x = p[0] / hw;
        let mut y = p[1] / hh;
        if flip_y {
            y = -y;
        }
        let y = y + dy / hh;
        x0 = x0.min(x);
        x1 = x1.max(x);
        y0 = y0.min(y);
        y1 = y1.max(y);
    }
    if !x0.is_finite() {
        return [-1.0, 1.0, -1.0, 1.0];
    }
    [x0, x1, y0, y1]
}

#[cfg(test)]
mod tests {
    use super::*;

    // ════════════ ：负 scale 的镜像轴语义（`WE_NEG_SCALE`）════════════
    // 五个候选的**矩阵代数**必须逐条钉死 —— 它们之间的差别就是"哪个量被翻"：
    // signed/uv：世界矩阵原样（`S(sx<0)` ⇒ 绕自身中心镜像）
    // abs ：x 那一列符号抵消（= |scale.x|），平移不动
    // parent ：x 那一列抵消 **且** 平移绕父原点反射（`x → 2·Px − x`）
    // place ：x 那一列抵消 **且** 平移推到锚点另一侧（`x → x − w`）
    // auto ：只在 `full_cover`（四边形盖住画布）时按 abs
    // ⚠ 开关是**进程环境变量** ⇒ 所有模式检查必须写在**同一个 `#[test]`** 里顺序执行，
    // 否则并行跑测试会互相改 env（cargo 默认多线程）。
    fn neg_scale_mat(sx: f32, tx: f32) -> Mat4 {
        Mat4::translate([tx, 0.0, 0.0]).mul(&Mat4::scale(sx, 1.0, 1.0))
    }

    fn neg_scale_set(v: &str) {
        if v.is_empty() {
            std::env::remove_var("WE_NEG_SCALE");
        } else {
            std::env::set_var("WE_NEG_SCALE", v);
        }
    }

    #[test]
    fn neg_scale_modes_matrix_algebra() {
        let sc = [-2.06137f32, 2.06137, 2.06137];
        let m = neg_scale_mat(-2.06137, 1920.0);

        // 默认（不设）/ signed / uv / 拼错的词 ⇒ 逐元素不变（= 改动前行为）
        for mode in ["", "signed", "uv", "nonsense"] {
            neg_scale_set(mode);
            let out = neg_scale_fix(m, sc, 1920.0, 1979.0, true);
            assert_eq!(out.m, m.m, "mode={mode:?} 必须逐元素不变");
        }

        // abs：x 列 = +2.06137，平移不动（footprint 不变、只翻内容）
        neg_scale_set("abs");
        let out = neg_scale_fix(m, sc, 1920.0, 1979.0, true);
        assert!((out.m[0] - 2.06137).abs() < 1e-5, "abs: x 列应 = +2.06137");
        assert_eq!(out.m[12], m.m[12], "abs: 平移不能动");

        // parent：平移绕父原点 x=1920 反射（100 → 3740）；对象本就在父原点 ⇒ 不动点
        neg_scale_set("parent");
        let out = neg_scale_fix(neg_scale_mat(-2.06137, 100.0), sc, 1920.0, 1979.0, true);
        assert!((out.m[12] - 3740.0).abs() < 1e-3, "parent: 平移应反射到 3740");
        assert!((out.m[0] - 2.06137).abs() < 1e-5);
        let out2 = neg_scale_fix(m, sc, 1920.0, 1979.0, true);
        assert!((out2.m[12] - 1920.0).abs() < 1e-3, "parent: 父原点上的对象不动");

        // place：footprint 翻到锚点另一侧（x → x − 2·half_w）
        neg_scale_set("place");
        let out = neg_scale_fix(m, sc, 1920.0, 1979.0, true);
        assert!((out.m[12] - (1920.0 - 2.0 * 1979.0)).abs() < 1e-3, "place: 中心应在 −2038");
        assert!((out.m[0] - 2.06137).abs() < 1e-5);

        // auto：盖住画布 ⇒ abs；小道具（作者手翻）⇒ 逐元素不变
        neg_scale_set("auto");
        let covered = neg_scale_fix(m, sc, 1920.0, 1979.0, true);
        assert!((covered.m[0] - 2.06137).abs() < 1e-5, "auto: 盖住画布 ⇒ 按 abs");
        let small = neg_scale_fix(m, sc, 1920.0, 1979.0, false);
        assert_eq!(small.m, m.m, "auto: 不盖画布的层必须原样");

        // 正 scale：任何模式都不许动（归零风险面 = 0）
        let pos = neg_scale_mat(1.5, 700.0);
        for mode in ["abs", "parent", "place", "uv", "auto"] {
            neg_scale_set(mode);
            let out = neg_scale_fix(pos, [1.5, 1.5, 1.5], 1920.0, 100.0, false);
            assert_eq!(out.m, pos.m, "mode={mode:?} 不许动正 scale");
        }

        // 解析：大写 / 空格也要认；未知值回 signed
        for (raw, want) in [
            (" ABS ", NegScaleMode::Abs),
            ("Auto", NegScaleMode::Auto),
            ("parent", NegScaleMode::Parent),
            ("", NegScaleMode::Signed),
            ("bogus", NegScaleMode::Signed),
        ] {
            neg_scale_set(raw);
            assert_eq!(neg_scale_mode(), want, "WE_NEG_SCALE={raw:?}");
        }
        neg_scale_set("");
    }

    /// `WE_PERF_SUPERSAMPLE`：系数 → 骨骼层链缓冲缩放。
    /// 本场景 2560x1440 输出下 obj1016 的逻辑链空间 2539 宽、屏上脚印 1693 宽
    /// （2539 · 2560/3840）= 现值 1.5× 超采样。这条换算错了就会把"中间档"变成
    /// "另一个完全不同的档"，所以三个端点都钉死：1.5 ⇒ 不动、1.2 ⇒ 0.8、1.0 ⇒ 1.5 分之一。
    #[test]
    fn supersample_scale_maps_factor_to_buffer_width() {
        // 2539 · 2560/3840（= 屏上脚印宽，非整数）
        let (lw, disp) = (2539u32, 2539.0f32 * 2560.0 / 3840.0);
        assert!((supersample_chain_scale(1.5, disp, lw) - 1.0).abs() < 1e-6);
        assert!((supersample_chain_scale(1.2, disp, lw) - 0.8).abs() < 1e-5);
        assert!((supersample_chain_scale(1.0, disp, lw) - 1.0 / 1.5).abs() < 1e-5);
        // 超过 1.5 也不许把缓冲放大（那是白烧带宽）；下限 0.05 防 0 尺寸。
        assert!((supersample_chain_scale(4.0, disp, lw) - 1.0).abs() < 1e-6);
        assert_eq!(supersample_chain_scale(0.0, disp, lw), 0.05);
    }

    /// `WE_PERF_SUPERSAMPLE_MASK`：系数 → 遮罩烘焙缓冲尺寸（× 源贴图，夹到逻辑尺寸）。
    /// 源 1070x1750、逻辑 2539x3570（阴影/遮罩烘焙的老缓冲）。
    #[test]
    fn supersample_mask_dims_multiplies_source_and_clamps_to_logical() {
        assert_eq!(supersample_mask_dims(1.0, 1070, 1750, 2539, 3570), (1070, 1750));
        assert_eq!(supersample_mask_dims(1.45, 1070, 1750, 2539, 3570), (1552, 2538));
        // 夹到逻辑尺寸：f 够大时回到"完全不缩"（= 现行为）；2.37 会差 3 px，故用 2.5。
        assert_eq!(supersample_mask_dims(2.5, 1070, 1750, 2539, 3570), (2539, 3570));
    }

    /// **"用户可绑定"的常量**（`{"user":"xraysize","value":0.5}`）必须能解析成数值。
    /// 崩坏3 `2564205376` 的 X-Ray 就是靠它给 `g_PointerScale`（鼠标处透视区的大小）赋值：
    /// 解析不出来 ⇒ 该 uniform 恒 0 ⇒ shader 里 `mix(999, 1.0/g_PointerScale, step(0.001, 0))`
    /// 取 999 ⇒ 精灵被放大 999 倍 ⇒ **透视区完全消失**（而且不报任何错、画面看不出线索）。
    /// 语义与 `visible` 的 `bound::*` 一致：**用户属性覆盖优先，其次它自己声明的 `value`**。
    #[test]
    fn bound_constant_resolves_to_its_value() {
        // 直接值（本来就支持，防止改坏）
        let v = json_to_value(&serde_json::json!(1));
        assert!(matches!(v, Some(Value::F(x)) if (x - 1.0).abs() < 1e-6), "{v:?}");
        // 绑定形态（没有覆盖）⇒ 用它声明的 value
        let v = json_to_value(&serde_json::json!({"user": "xraysize_ut", "value": 0.5}));
        assert!(
            matches!(v, Some(Value::F(x)) if (x - 0.5).abs() < 1e-6),
            "绑定常量没解出 value：{v:?}"
        );
        // 用户属性覆盖优先（界面拖滑杆 / `--prop xraysize=…` / 控制面 set-prop）
        we_scene::scene::userprop::set("xraysize_ut", serde_json::json!(0.8));
        let v = json_to_value(&serde_json::json!({"user": "xraysize_ut", "value": 0.5}));
        assert!(
            matches!(v, Some(Value::F(x)) if (x - 0.8).abs() < 1e-6),
            "用户属性覆盖没生效：{v:?}"
        );
        // script 形态（只有 value 可取）也要能取到；向量串按空白切
        let v = json_to_value(&serde_json::json!({"script": "return 1;", "value": "0.25 0.75"}));
        assert!(
            matches!(v, Some(Value::V2(a, b)) if (a - 0.25).abs() < 1e-6 && (b - 0.75).abs() < 1e-6),
            "{v:?}"
        );
        // 解不出来就老老实实 None（别瞎猜一个值出来）
        assert!(json_to_value(&serde_json::json!({"user": "x"})).is_none());
    }

    /// 鼠标位置的口径与边界：夹到 `[0,1]`，`None` 表示"没有输入"（= 不下发 uniform）。
    #[test]
    fn mouse_is_clamped_and_optional() {
        // 没有 GL 也能测：`set_mouse`/`clear_mouse` 只改字段，不碰 GL
        // （`SceneRenderer` 需要 GL 才能构造 ⇒ 这里只测纯逻辑的部分：`WE_MOUSE` 的解析口径）
        assert_eq!(parse_mouse_spec("0.3,0.75"), Some((0.3, 0.75)));
        assert_eq!(parse_mouse_spec(" 0.5 , 0.5 "), Some((0.5, 0.5)));
        assert_eq!(parse_mouse_spec("0,1"), Some((0.0, 1.0)));
        // 夹取在渲染器入口做（`set_mouse`）：这里只保证解析不瞎猜
        assert_eq!(parse_mouse_spec("1.5,-2"), Some((1.5, -2.0)));
        assert_eq!(parse_mouse_spec("0.3"), None, "少一个分量 ⇒ 忽略（别猜另一半）");
        assert_eq!(parse_mouse_spec("a,b"), None);
        assert_eq!(parse_mouse_spec(""), None);
    }

    /// 指针兜底位：**没有输入时 `g_PointerPosition` 不是 0，而是屏幕中心**。
    /// 这条是"X-Ray 透明层不显示"的根因守卫：`(0,0)` ⇒ `effects/xray` 的
    /// `saturate(texSource - 指针)` 把采样点压到贴图角落 ⇒ 精灵 alpha≈0 ⇒ `blend≈0`
    /// ⇒ **整层零变化**（实测崩坏3 `2564205376`：开 X-Ray 与关整条特效链的输出逐字节相同）。
    /// 默认值取官方口径：OWE `Scene.cppm:2872` 的 `m_pointer_position { 0.5f, 0.5f }`。
    #[test]
    fn pointer_defaults_to_screen_center() {
        // 不设环境变量 ⇒ 中心
        std::env::remove_var("WE_POINTER_DEFAULT");
        assert_eq!(pointer_default_from_env(), [0.5, 0.5], "默认必须是中心（官方语义）");
        // 显式回旧口径（A/B 用）
        for v in ["zero", "0", "off", "FALSE", " Zero "] {
            std::env::set_var("WE_POINTER_DEFAULT", v);
            assert_eq!(pointer_default_from_env(), [0.0, 0.0], "{v:?} 应回旧口径");
        }
        // 别的值（拼错/空）一律按官方语义走中心 —— 宁可"有透镜"也不要"静默没有"
        for v in ["", "center", "1", "0.5"] {
            std::env::set_var("WE_POINTER_DEFAULT", v);
            assert_eq!(pointer_default_from_env(), [0.5, 0.5], "{v:?} 应回中心");
        }
        std::env::remove_var("WE_POINTER_DEFAULT");
    }

    /// 视差鼠标项的**代数恒等式**：`off == base + mouse_term ⊙ k`，**与可见区尺寸无关**。
    /// 这条断言的就是 修掉的那个单位错 —— 以前少乘一次 `vis/2`
    /// （`off = base + par_shift ⊙ k`），于是鼠标项被 `vis/2` 除了一次：
    /// 本机 `vis=3840x2160` 时小 **1920 倍**，在两个对角鼠标位上的差别小到不可
    /// （实测最大 23 级/三通道和，0 个像素超过阈值 24）。
    /// 恒等式对**任意** `vis` 都必须成立（换分辨率不能改鼠标项）。
    #[test]
    fn parallax_mouse_term_is_vis_independent() {
        let base = [120.0f32, -80.0];
        let k = [0.5f32, 0.5];
        let mouse_term = [-768.0f32, -432.0]; // 场景像素（含 diag(1,-1) 与 influence）
        let want = [
            base[0] + mouse_term[0] * k[0],
            base[1] + mouse_term[1] * k[1],
        ];
        for vis in [[3840.0f32, 2160.0], [1920.0, 1080.0], [1280.0, 720.0]] {
            // `par_shift` = 鼠标项的 NDC 位移（= 2·mouse/vis，见 `parallax::ndc_shift`）
            let par_shift = [2.0 * mouse_term[0] / vis[0], 2.0 * mouse_term[1] / vis[1]];
            let off = parallax_layer_off([base[0], base[1], k[0], k[1]], par_shift, vis);
            assert!(
                (off[0] - want[0]).abs() < 1e-3 && (off[1] - want[1]).abs() < 1e-3,
                "vis={vis:?} off={off:?} want={want:?}（鼠标项必须与可见区尺寸无关）"
            );
        }
        // 中性鼠标（pointer=0.5）⇒ 鼠标项为 0 ⇒ 只剩 base（"开/关"的静态差就是它）
        let off0 = parallax_layer_off([base[0], base[1], k[0], k[1]], [0.0, 0.0], [3840.0, 2160.0]);
        assert_eq!(off0, base);
    }

    /// y 落点那次镜像**只做在链的根上**（3704273480 夏日影的椰子树/饮料机/沙/人物
    /// 全在错误一侧的根因）。三条不变量：
    /// · 单层链（根就是对象自己）= 与旧口径逐位一致（Ice Princess / 其它 16 条不受影响）；
    /// · 根是纯容器时，子对象的 origin **不再**被翻（旧口径翻的是"折叠后的和"⇒ 每个子对象多翻一次
    /// = 2·子 origin.y 的错位）；
    /// · 根不是容器（叶子是粒子那条）保持旧口径。
    #[test]
    fn y_mirror_applies_at_the_chain_root_only() {
        let (h, cy) = (2160.0_f32, 1080.0_f32);
        // 单层链：根=叶子 ⇒ 与旧行为一致
        assert_eq!(chain_level_origin_y(1900.0, true, false, true, h), 260.0);
        assert_eq!(chain_level_origin_y(1080.0, true, false, true, h), 1080.0);
        // 容器根（202/943，origin=画布中心）的子对象：容器自己恒等，子对象原样
        assert_eq!(chain_level_origin_y(1080.0, true, true, true, h), 1080.0);
        assert_eq!(
            chain_level_origin_y(-1123.25, false, true, true, h),
            -1123.25,
            "「球」的 origin 不该被翻 ⇒ world_y = 1080 + (-1123.25) = -43.25（沙面下缘）"
        );
        // 根不是容器：保持旧口径（对和翻一次 ⇒ 叶子那层仍走 scene_h - y）
        assert_eq!(chain_level_origin_y(86.28, false, false, true, h), h - 86.28);
        // 镜像总开关关掉时一律原样
        assert_eq!(chain_level_origin_y(845.47, true, false, false, h), 845.47);
        assert_eq!(chain_level_origin_y(845.47, false, true, false, h), 845.47);
    }

    /// 对齐偏移的**幅度**按 WE `updateScenePosition` 的 `scaledSize = size × scale`
    /// （录制帧量出来的那条：3704273480「球」的 align=bottom 偏移 = 351×1.687/2 = 296.1，
    /// 旧口径 175.5 ⇒ 差 120.6px，配合镜像修正后预测下移 1775.5px vs 实测 1776.0px）。
    #[test]
    fn alignment_offset_uses_scaled_size() {
        let (dx, dy) = alignment_offset(Some("bottom"), [400.0, 351.0], [1.687, 1.687, 1.0]);
        assert!((dx).abs() < 1e-4);
        assert!((dy - 296.11).abs() < 0.05, "bottom ⇒ +scaledH/2，拿到 {dy}");
        let (_, dy_top) = alignment_offset(Some("top"), [400.0, 351.0], [1.687, 1.687, 1.0]);
        assert!((dy_top + 296.11).abs() < 0.05);
        let (dx_left, dy_left) = alignment_offset(Some("left"), [1000.0, 500.0], [2.0, 2.0, 1.0]);
        assert!((dx_left - 1000.0).abs() < 1e-3, "left ⇒ +scaledW/2");
        assert!(dy_left.abs() < 1e-3);
        // scale=1 的对象（2564205376 的 4 个 `topright`）逐位不变
        let (dx1, dy1) = alignment_offset(Some("topright"), [1826.0, 1103.0], [1.0, 1.0, 1.0]);
        assert!((dx1 + 913.0).abs() < 1e-3 && (dy1 + 551.5).abs() < 1e-3);
        assert_eq!(alignment_offset(None, [100.0, 100.0], [3.0, 3.0, 1.0]), (0.0, 0.0));
    }

    /// 图层空间 y 落点修正的**适用面**（用户报的「礼服斯卡蒂沙发靠背不见」）。
    /// 修正补的是「内容绕图层缓冲一圈」带出的那次 y 镜像 ⇒ 只对**声明了特效链**的层成立；
    /// 特效列表为空的层不绕圈，修正对它是凭空多出的一次纯平移。
    #[test]
    fn layer_space_y_fix_applies_only_to_layers_with_an_effect_chain() {
        assert!(yfix_applies_for_layer(true, false), "有链 ⇒ 修正是对的");
        assert!(!yfix_applies_for_layer(false, false), "无链且落点不由 origin 独占 ⇒ 凭空位移（B沙发）");
        // 模型未声明 `cropoffset`、且没有 alignment/locktransforms/solid 的
        // 「普通图片层」同样需要它（`3571777169` 的 353，行走无 cropoffset 补不上这次镜像）。
        assert!(yfix_applies_for_layer(false, true), "无链但无 cropoffset 补偿 ⇒ 需要修正");
    }

    /// `WE_FX_CARDSPACE`：**换几何 + 换 MVP 之后，没有顶点位移的趟逐位等价**。
    /// 这是整套改法的核心不变量（否则"对齐空间"就会顺手把底图/普通特效的画质改掉）：
    /// 卡片顶点 `v_card = D·v_unit`，配 `M(A') = M(A)·D⁻¹` ⇒ `M(A')·v_card = M(A)·v_unit`。
    #[test]
    fn fx_card_mvp_preserves_ndc_for_pure_mvp_shaders() {
        // 驱动实际算的是 `gl_Position = M(A)·v`（`M(A)` = 以数组 A 为 `m` 的数学矩阵）
        let apply = |a: [f32; 16], v: [f32; 4]| -> [f32; 4] {
            let m = Mat4 { m: a };
            let mut o = [0.0f32; 4];
            for r in 0..4 {
                let mut acc = 0.0;
                for c in 0..4 {
                    acc += v[c] * m.m[r + c * 4];
                }
                o[r] = acc;
            }
            o
        };
        // 两种 A：单位阵（= 链内趟的 `full_q`）与一个非平凡仿射（含平移/非等比缩放）
        let ident = Mat4::identity().column_major();
        let aff = Mat4::translate([3.5, -2.25, 0.0])
            .mul(&Mat4::scale(0.75, 1.5, 1.0))
            .mul(&Mat4::rotate_z(0.3))
            .column_major();
        for a in [ident, aff] {
            for card in [[512.0f32, 1024.0], [1024.0, 256.0], [256.0, 144.0], [1080.0, 1920.0]] {
                let a2 = fx_card_mvp(a, card);
                for uv in [[-1.0f32, -1.0], [1.0, 1.0], [0.37, -0.82]] {
                    let v_unit = [uv[0], uv[1], 0.0, 1.0];
                    let v_card = [uv[0] * card[0] / 2.0, uv[1] * card[1] / 2.0, 0.0, 1.0];
                    let p0 = apply(a, v_unit);
                    let p1 = apply(a2, v_card);
                    for k in 0..4 {
                        assert!(
                            (p0[k] - p1[k]).abs() < 1e-3,
                            "A={a:?} card={card:?} uv={uv:?} k={k}: {} vs {}",
                            p0[k],
                            p1[k]
                        );
                    }
                }
            }
        }
        // 卡片尺寸拿不到时**原样返回**（不瞎乘）
        assert_eq!(fx_card_mvp(ident, [0.0, 0.0]), ident);
    }

    /// 融合趟的"静默画空"护栏：顶点几何 × MVP 的两套正确组合都给 |ndc| ≈ 1，
    /// 而"单位四边形 × 卡片 MVP"（`3504393434` 的黑屏根因）给 5e-4。
    #[test]
    fn mvp_max_ndc_catches_unit_quad_with_card_mvp() {
        let ident = Mat4::identity().column_major();
        // ① 单位四边形 + 单位阵 = 1.0（链内趟的常规口径）
        assert!((mvp_max_ndc(&ident, [1.0, 1.0]) - 1.0).abs() < 1e-6);
        // ② 卡片 ±size/2 + 卡片 MVP = 1.0（`WE_FX_CARDSPACE` 口径）
        let card = [1920.0f32, 1080.0];
        let cmvp = fx_card_mvp(ident, [3840.0, 2160.0]);
        assert!((mvp_max_ndc(&cmvp, [card[0], card[1]]) - 1.0).abs() < 1e-5);
        // ③ 错配：单位四边形配卡片 MVP ⇒ 0.0005（会只画 RT 正中 2x2 像素）
        let bad = mvp_max_ndc(&cmvp, [1.0, 1.0]);
        assert!(bad < 0.01, "错配必须落进闸门（实测 {bad}）");
        // ④ 退化（w = 0）保守判成无穷大 ⇒ 被闸门拒绝
        let mut degen = ident;
        degen[15] = 0.0;
        assert!(mvp_max_ndc(&degen, [1.0, 1.0]).is_infinite());
    }

    /// `WE_FX_CARDSPACE` 的开关口径：不设 / 空 / `"0"` 一律**关**；其余（含 `"1"`）开。
    #[test]
    fn fx_cardspace_switch_defaults_to_on() {
        // 起：源码默认 = 屏上对齐行为 ⇒ 不设即**开**，`=0` 才关（保留 A/B 与回退能力）。
        std::env::remove_var("WE_FX_CARDSPACE");
        assert!(fx_cardspace_on(), "不设 ⇒ 默认开");
        std::env::set_var("WE_FX_CARDSPACE", "");
        assert!(fx_cardspace_on(), "空串 ⇒ 当不设 ⇒ 开");
        std::env::set_var("WE_FX_CARDSPACE", "1");
        assert!(fx_cardspace_on(), "\"1\" ⇒ 开");
        std::env::set_var("WE_FX_CARDSPACE", "0");
        assert!(!fx_cardspace_on(), "\"0\" ⇒ 关");
        std::env::set_var("WE_FX_CARDSPACE", "OFF");
        assert!(!fx_cardspace_on(), "\"OFF\" ⇒ 关（大小写不敏感）");
        std::env::remove_var("WE_FX_CARDSPACE");
    }

    /// `WE_BASE_CARDSPACE`：底图趟（类①）**换几何 + 换 MVP 之后逐位等价**的
    /// 定标测试 —— 用**底图趟真正的那条 MVP**（`layer_copy_mvp` 的两个分支：斜置对象的
    /// 单位阵 / 轴对齐对象的 `scale(sx,sy)·T(-c)·m0`）各算一遍。
    /// ⚠ 这里把**驱动约定**写死成"上传数组 `u` ⇒ 驱动算 `R(u)·v`，`R(u)` = 把 `u` 按
    /// **行主序**读出来的矩阵"（本渲染器实测定标：`gl_ctx.rs:493`
    /// `gl.uniform_matrix_4_f32_slice(Some(loc), false, &t4(&mvp))` —— 上传前先 `t4` 转置一次；
    /// 配合 GL 的"列主序数组"规则，净效果 = 数组的**行主序**读法）。在这条约定下
    /// **左乘** `D⁻¹·A` 才逐位等价；右乘会差一个 `D⁻¹·Aᵀ·D`。
    #[test]
    fn base_card_mvp_preserves_ndc_for_pure_mvp_shaders() {
        // 驱动实际算的：R(u)·v，R(u)(r,c) = u[r*4+c]
        let drive = |u: [f32; 16], v: [f32; 4]| -> [f32; 4] {
            let mut o = [0.0f32; 4];
            for r in 0..4 {
                let mut acc = 0.0;
                for c in 0..4 {
                    acc += v[c] * u[r * 4 + c];
                }
                o[r] = acc;
            }
            o
        };
        // 底图趟在图层空间用的两个 `base_mvp_raw` 分支：
        // ① `chain_base_local` ⇒ 单位阵（斜置对象）；
        // ② 轴对齐对象的近似构造 `scale(sx,sy)·translate(-cx,-cy)·m0`，
        // 其中 `m0 = proj·world·scale(size/2)`（见调用点）。
        let sx = 0.3125f32;
        let sy = -0.8642f32;
        let cx = 0.117f32;
        let cy = -0.413f32;
        let m0 = Mat4::translate([11.25, -3.5, 0.0])
            .mul(&Mat4::scale(64.0, 36.0, 1.0))
            .mul(&Mat4::rotate_z(0.21))
            .column_major();
        let a_case = Mat4::scale(sx, sy, 1.0)
            .mul(&Mat4::translate([-cx, -cy, 0.0]))
            .mul(&Mat4 { m: m0 })
            .transposed()
            .column_major();
        let ident = Mat4::identity().column_major();
        for (a, is_ident) in [(ident, true), (a_case, false)] {
            for card in [[512.0f32, 1024.0], [1024.0, 256.0], [256.0, 144.0], [1080.0, 1920.0]] {
                let u_left = fx_card_mvp_left(a, card);
                let u_right = fx_card_mvp(a, card);
                for uv in [[-1.0f32, -1.0], [1.0, 1.0], [0.37, -0.82]] {
                    let v_unit = [uv[0], uv[1], 0.0, 1.0];
                    // 卡片顶点就是 `card_quad_verts` 生成的那组（半尺寸 = card/2）
                    let v_card = [uv[0] * card[0] / 2.0, uv[1] * card[1] / 2.0, 0.0, 1.0];
                    let p0 = drive(a, v_unit);
                    let p1 = drive(u_left, v_card);
                    for k in 0..4 {
                        assert!(
                            (p0[k] - p1[k]).abs() < 1e-3,
                            "左乘必须逐位等价：a={a:?} card={card:?} uv={uv:?} k={k}: {} vs {}",
                            p0[k],
                            p1[k]
                        );
                    }
                }
                // 单位阵左右乘**完全相同**
                // ⇒ 那条已上屏的路不受本结论影响。
                if is_ident {
                    assert_eq!(u_left, u_right, "单位阵时左乘/右乘必须一模一样");
                }
            }
        }
        // 非单位阵时左右乘**必须不同**（否则说明"驱动约定"根本没被定到，本单会白做）
        assert_ne!(
            fx_card_mvp_left(a_case, [512.0, 1024.0]),
            fx_card_mvp(a_case, [512.0, 1024.0]),
            "非单位阵时左右乘必须不同（这正是 2861113973/3571777169 那两处亚像素偏移的来源）"
        );
        // 卡片尺寸拿不到时**原样返回**（不瞎乘）
        assert_eq!(fx_card_mvp_left(a_case, [0.0, 0.0]), a_case);
    }

    /// `WE_BASE_CARDSPACE` 的开关口径：不设 / 空 / `"0"` 一律**关**；其余（含 `"1"`）开。
    /// （`WE_BASE_CARDSPACE` 与 `WE_FX_CARDSPACE` 各读各的环境变量：本函数只读前者 ⇒ 两者
    /// 结构上互相独立，且这里**不动** `WE_FX_CARDSPACE`，避免与那条测试互相干扰。）
    #[test]
    fn base_cardspace_switch_defaults_to_on() {
        // 起：源码默认 = 屏上对齐行为 ⇒ 不设即**开**，`=0` 才关。
        std::env::remove_var("WE_BASE_CARDSPACE");
        assert!(base_cardspace_on(), "不设 ⇒ 默认开");
        std::env::set_var("WE_BASE_CARDSPACE", "");
        assert!(base_cardspace_on(), "空串 ⇒ 当不设 ⇒ 开");
        std::env::set_var("WE_BASE_CARDSPACE", "1");
        assert!(base_cardspace_on(), "\"1\" ⇒ 开");
        std::env::set_var("WE_BASE_CARDSPACE", "0");
        assert!(!base_cardspace_on(), "\"0\" ⇒ 关");
        std::env::remove_var("WE_BASE_CARDSPACE");
    }

    /// `base_card_mvp` 的默认乘序 = **左乘**（`WE_BASE_CARD_ORDER=right` 才切右乘）。
    #[test]
    fn base_card_mvp_defaults_to_the_left_order() {
        let a = Mat4::translate([2.0, -1.0, 0.0]).mul(&Mat4::scale(4.0, 9.0, 1.0)).column_major();
        let card = [512.0f32, 1024.0];
        std::env::remove_var("WE_BASE_CARD_ORDER");
        assert_eq!(base_card_mvp(a, card), fx_card_mvp_left(a, card), "默认必须左乘");
        std::env::set_var("WE_BASE_CARD_ORDER", "right");
        assert_eq!(base_card_mvp(a, card), fx_card_mvp(a, card), "=right ⇒ 右乘");
        std::env::remove_var("WE_BASE_CARD_ORDER");
        assert_eq!(base_card_mvp(a, card), fx_card_mvp_left(a, card), "去掉后回到左乘");
    }

    /// 官方卡片 `size` 的解析优先级：场景显式 `size` > 底图贴图像素尺寸 > 场景兜底。
    #[test]
    fn fx_card_size_prefers_scene_size_then_texture_pixels() {
        // 场景显式给了 size ⇒ 用它（语料 294 个对象里 117 个没给 size，给的那些一律走这条）
        assert_eq!(
            fx_card_size([256.0, 256.0], Some([512.0, 1024.0]), Some([8.0, 8.0])),
            [512.0, 1024.0]
        );
        // 没给 ⇒ 用贴图像素尺寸（= `autosize` 对象的官方语义；foliagesway preview 的 3 个对象）
        assert_eq!(
            fx_card_size([256.0, 256.0], None, Some([1024.0, 256.0])),
            [1024.0, 256.0]
        );
        // 都没有 ⇒ 场景兜底（与现状逐位一致）
        assert_eq!(fx_card_size([256.0, 256.0], None, None), [256.0, 256.0]);
        // 退化值（0 / 负 / 非有限）不参与，也不返回 0（0 会让 `diag(2/size)` 变 Inf）
        assert_eq!(
            fx_card_size([256.0, 256.0], Some([0.0, 100.0]), Some([32.0, 32.0])),
            [32.0, 32.0]
        );
        assert_eq!(fx_card_size([256.0, 256.0], Some([-512.0, -256.0]), None), [512.0, 256.0]);
        assert_eq!(
            fx_card_size([256.0, 256.0], None, Some([f32::NAN, 8.0])),
            [256.0, 256.0]
        );
    }

    /// 网格"是不是展开图集布局"的判据（uv-空间方案的数据门控）。
    /// 图集布局（`pos == (u−0.5)·size`）⇒ 残差 0 ⇒ 可以走 uv 空间；
    /// 装配姿态（残差 ≥ 0.6 个对象尺寸）⇒ 必须走旧口径，否则部件采样区整体错开。
    #[test]
    fn mesh_atlas_residual_separates_atlas_from_assembled_layouts() {
        let size = [2602.0f32, 2000.0];
        let uvs = vec![[0.25f32, 0.30f32], [0.75, 0.10], [0.5, 0.5]];
        let atlas: Vec<[f32; 3]> = uvs
            .iter()
            .map(|uv| [(uv[0] - 0.5) * size[0], -(uv[1] - 0.5) * size[1], 0.0])
            .collect();
        let r_atlas = mesh_atlas_residual(&atlas, &uvs, size);
        assert!(r_atlas < 1e-4, "图集布局残差应≈0：{r_atlas}");
        assert!(mesh_is_atlas(r_atlas));
        // 装配姿态：把 y 整体挪 0.7 个对象尺寸 ⇒ 残差 ≈ 0.7，必须判"不是图集"。
        let assembled: Vec<[f32; 3]> = atlas
            .iter()
            .map(|p| [p[0], p[1] + 0.7 * size[1], p[2]])
            .collect();
        let r_asm = mesh_atlas_residual(&assembled, &uvs, size);
        assert!(r_asm > 0.2, "装配姿态残差应≈0.54：{r_asm}");
        assert!(!mesh_is_atlas(r_asm));
        // 空网格 / uv 缺失：不 panic，残差 0（无数据 ⇒ 不改判据）
        assert_eq!(mesh_atlas_residual(&[], &[], size), 0.0);
        let two = vec![[0.0f32, 0.0, 0.0], [10.0, 0.0, 0.0]];
        assert_eq!(mesh_atlas_residual(&two, &[], size), 0.0);
    }

    /// 树上可见性：容器（`solid:true` + 无 `image` 的分组锚点）的 `visible` 管的是**整棵子树**。
    /// 钉死三件事：隐藏容器 ⇒ 子对象不画；祖父不可见 ⇒ 孙辈也不画；自身 `visible=true` 救不回来。
    /// （3704273480 的“主题”切换就是这个语义：改属性 = 整棵子树显隐。）
    #[test]
    fn tree_visibility_inherits_from_group_containers() {
        let doc = we_scene::scene::parse_scene(
            br#"{"objects":[
                 {"id":1,"visible":false},
                 {"id":2,"parent":1,"image":"a.png"},
                 {"id":3,"parent":2,"image":"b.png"},
                 {"id":4,"parent":1,"image":"c.png","visible":true},
                 {"id":5,"image":"d.png"},
                 {"id":6,"parent":999,"image":"e.png"}
               ]}"#,
        )
        .expect("synthetic scene must parse");
        let by_id: HashMap<i64, &we_scene::scene::Object> =
            doc.objects.iter().map(|o| (o.id, o)).collect();
        let obj = |id: i64| *by_id.get(&id).expect("id in map");
        assert!(tree_visible(obj(5), &by_id), "无父链 + 自身可见 ⇒ 可见");
        assert!(!tree_visible(obj(1), &by_id), "容器自身 visible=false");
        assert!(!tree_visible(obj(2), &by_id), "父容器不可见 ⇒ 子对象不可见");
        assert!(!tree_visible(obj(3), &by_id), "祖父不可见 ⇒ 孙辈也不可见");
        assert!(!tree_visible(obj(4), &by_id), "子对象自己写 visible=true 也救不回来");
        assert!(tree_visible(obj(6), &by_id), "父 id 不在场景里（悬空）⇒ 不因它变不可见");
    }

    /// 链趟打包贴图的 V 规则：**按对象数据推导**（`copybackground`），不看名字/mode。
    /// 钉死三件事：默认规则、图层空间下不再翻、A/B 开关能一键回到两个极端。
    #[test]
    fn chain_tex_vflip_follows_the_declared_field_not_names() {
        let d = FxTexVFlip::CopyBackground;
        // 默认规则：copybackground=true 且画布空间 ⇒ 翻（Gaze 的水波纹遮罩才落在水面上）
        assert!(d.want_flip(true, false));
        // 没有该字段的层（Ice Princess `рыцари` / 夏日影各层）⇒ 不翻
        assert!(!d.want_flip(false, false));
        // 图层空间那条路会给底图趟多翻一次 ⇒ 贴图不该再翻（实测 Gaze 翻反而落到云上）
        assert!(!d.want_flip(true, true));
        assert!(!d.want_flip(false, true));
        // A/B 开关
        assert!(!FxTexVFlip::Off.want_flip(true, false));
        assert!(FxTexVFlip::All.want_flip(false, true));
    }

    /// 色彩调整的代数性质：**色相 +小角度 = 色轮正方向**（蓝→紫）。
    /// 立这条测试是因为我在"肉眼看大角度截图觉得转反了"上栽过一次 —— 方向只认小角度
    /// （此时 clamp 不介入、线性成立，均值才可信）。
    #[test]
    fn hue_rotates_the_right_way() {
        // 复刻 shader 的矩阵。GLSL 的 mat3(a,b,c,…) 按**列**填充；我把 9 个常量按行摆 ⇒ 有效矩阵 = 转置
        let mk = |deg: f32| {
            let (ca, sa) = (deg.to_radians().cos(), deg.to_radians().sin());
            let l = [
                [
                    0.299 + 0.701 * ca + 0.168 * sa,
                    0.587 - 0.587 * ca + 0.330 * sa,
                    0.114 - 0.114 * ca - 0.497 * sa,
                ],
                [
                    0.299 - 0.299 * ca - 0.328 * sa,
                    0.587 + 0.413 * ca + 0.035 * sa,
                    0.114 - 0.114 * ca + 0.292 * sa,
                ],
                [
                    0.299 - 0.300 * ca + 1.250 * sa,
                    0.587 - 0.588 * ca - 1.050 * sa,
                    0.114 + 0.886 * ca - 0.203 * sa,
                ],
            ];
            let mut m = [[0.0f32; 3]; 3];
            for i in 0..3 {
                for j in 0..3 {
                    m[i][j] = l[j][i];
                }
            }
            m
        };
        let hsv_hue = |c: [f32; 3]| -> f32 {
            let mx = c[0].max(c[1]).max(c[2]);
            let mn = c[0].min(c[1]).min(c[2]);
            let d = mx - mn;
            let h = if d < 1e-6 {
                0.0
            } else if mx == c[0] {
                ((c[1] - c[2]) / d).rem_euclid(6.0)
            } else if mx == c[1] {
                (c[2] - c[0]) / d + 2.0
            } else {
                (c[0] - c[1]) / d + 4.0
            };
            h * 60.0
        };
        let apply = |m: [[f32; 3]; 3], v: [f32; 3]| -> [f32; 3] {
            let mut o = [0.0f32; 3];
            for i in 0..3 {
                o[i] = (0..3).map(|j| m[i][j] * v[j]).sum::<f32>().clamp(0.0, 1.0);
            }
            o
        };
        let blue = [0.25, 0.45, 0.85]; // 中等亮度、够饱和 ⇒ 小角度下不会被 clamp 截断
        let h0 = hsv_hue(blue);
        let h1 = hsv_hue(apply(mk(20.0), blue));
        assert!(h1 > h0, "色相 +20° 应当把色相往正方向推（{h0} → {h1}）");
        let h2 = hsv_hue(apply(mk(-20.0), blue));
        assert!(h2 < h0, "色相 -20° 应当反向（{h0} → {h2}）");
        // 0° 应当恒等。容差给 2e-3 而不是 1e-4：这条标准矩阵的常数本身带舍入
        // （第 3 行写的是 0.299-0.300 = -0.001、0.587-0.588 = -0.001）⇒ 它只是**近似**恒等。
        // 真实路径不受影响：单位值会被 color_is_identity 拦下、回落到原来那条 shader（逐字节精确）。
        let id = apply(mk(0.0), blue);
        for i in 0..3 {
            assert!(
                (id[i] - blue[i]).abs() < 2e-3,
                "色相 0° 应当近似恒等（差 {}）",
                (id[i] - blue[i]).abs()
            );
        }
    }

    #[test]
    fn fit_modes_do_what_they_say() {
        // 场景 4000x2736（比 1.462）到屏幕 2560x1440（比 1.778）
        let (sw, sh, w, h) = (4000.0, 2736.0, 2560.0, 1440.0);
        // stretch：整张画布
        assert_eq!(
            fit_rect(sw, sh, w, h, FitMode::Stretch),
            (0.0, 0.0, 4000.0, 2736.0)
        );
        // center：可见区就是屏幕那么大，围绕画布中心
        let (x0, y0, x1, y1) = fit_rect(sw, sh, w, h, FitMode::Center);
        assert!((x1 - x0 - w).abs() < 0.01 && (y1 - y0 - h).abs() < 0.01);
        assert!(((x0 + x1) / 2.0 - sw / 2.0).abs() < 0.01);
        assert!(((y0 + y1) / 2.0 - sh / 2.0).abs() < 0.01);
        // contain：整张画布都在可见区内（留黑边），且可见区比例 = 屏幕比例
        let (x0, y0, x1, y1) = fit_rect(sw, sh, w, h, FitMode::Contain);
        assert!(x0 <= 0.01 && y0 <= 0.01 && x1 >= sw - 0.01 && y1 >= sh - 0.01);
        assert!(((x1 - x0) / (y1 - y0) - w / h).abs() < 0.01);
        // cover：铺满（= 老行为）—— 这个例子里画布比屏幕"窄"，所以 cover 裁的是上下的 y
        let (cx0, cy0, cx1, cy1) = fit_rect(sw, sh, w, h, FitMode::Cover);
        assert_eq!((cx0, cx1), (0.0, 4000.0));
        assert!((cy1 - cy0) < (y1 - y0), "cover 的可见高度应当小于 contain");
        assert!(((cx1 - cx0) / (cy1 - cy0) - w / h).abs() < 0.01);
        // 解析（含中文）
        assert_eq!(FitMode::parse("拉伸"), Some(FitMode::Stretch));
        assert_eq!(FitMode::parse("COVER"), Some(FitMode::Cover));
        assert_eq!(FitMode::parse("居中"), Some(FitMode::Center));
        assert_eq!(FitMode::parse("瞎写"), None);
    }

    #[test]
    fn cover_rect_crops_the_long_side_and_keeps_center() {
        let (x0, y0, x1, y1) = cover_rect(4000.0, 2736.0, 2560.0, 1440.0);
        assert_eq!((x0, x1), (0.0, 4000.0));
        let shown = y1 - y0;
        assert!((shown - 4000.0 / (2560.0 / 1440.0)).abs() < 0.01, "{shown}");
        assert!((y0 - (2736.0 - shown) / 2.0).abs() < 0.01, "{y0}");
    }

    #[test]
    fn cover_rect_crops_sides_when_screen_is_taller() {
        let (x0, y0, x1, y1) = cover_rect(1920.0, 1080.0, 1024.0, 768.0);
        assert_eq!((y0, y1), (0.0, 1080.0));
        let shown = x1 - x0;
        assert!((shown - 1080.0 * (1024.0 / 768.0)).abs() < 0.01, "{shown}");
    }

    #[test]
    fn readback_flips_and_swizzles() {
        let mut src = vec![
            255, 0, 0, 255, 255, 0, 0, 255, 0, 0, 255, 255, 0, 0, 255, 255,
        ];
        let mut dst = vec![0u8; 16];
        readback_into(&mut src, &mut dst, 2, 2, true);
        assert_eq!(&dst[0..4], &[255, 0, 0, 255]);
        assert_eq!(&dst[8..12], &[0, 0, 255, 255]);
    }

    /// "1 1 1" 要变成 vec3（godrays 的射线颜色就是这么给的）；
    /// **`"0.02, 0.02"`（逗号）也要变成 vec2** —— WE 编辑器对 vec2 就写这种形态，
    /// 只按空格切会把它降级成标量 ⇒ `glUniform1f` 打到 vec2 ⇒ GL_INVALID_OPERATION
    /// ⇒ 整帧失败（实测 `3704273480` 主题2 的 `u_AASmoothness`）。
    #[test]
    fn json_values_map_to_uniform_types() {
        let v: serde_json::Value = serde_json::from_str("0.9").unwrap();
        assert_eq!(json_to_value(&v), Some(Value::F(0.9)));
        let v: serde_json::Value = serde_json::from_str("\"1 1 1\"").unwrap();
        assert_eq!(json_to_value(&v), Some(Value::V3(1.0, 1.0, 1.0)));
        let v: serde_json::Value = serde_json::from_str("\"0.5 0.5\"").unwrap();
        assert_eq!(json_to_value(&v), Some(Value::V2(0.5, 0.5)));
        let v: serde_json::Value = serde_json::from_str("\"0.02, 0.02\"").unwrap();
        assert_eq!(json_to_value(&v), Some(Value::V2(0.02, 0.02)));
        let v: serde_json::Value = serde_json::from_str("\"1, 1, 0.5, 1\"").unwrap();
        assert_eq!(json_to_value(&v), Some(Value::V4(1.0, 1.0, 0.5, 1.0)));
        let v: serde_json::Value = serde_json::from_str("true").unwrap();
        assert_eq!(json_to_value(&v), Some(Value::I(1)));
    }

    /// 声明类型是 vec2 的值绝不能以标量形态下发（GL 会直接报 INVALID_OPERATION）
    #[test]
    fn constants_are_coerced_to_the_declared_arity() {
        assert_eq!(coerce_to_decl(Value::F(0.02), "vec2"), Value::V2(0.02, 0.0));
        assert_eq!(coerce_to_decl(Value::V3(1.0, 2.0, 3.0), "vec2"), Value::V2(1.0, 2.0));
        assert_eq!(coerce_to_decl(Value::F(1.0), "float"), Value::F(1.0));
        assert_eq!(coerce_to_decl(Value::F(1.5), "int"), Value::I(1));
        // 矩阵 / sampler 不动
        assert_eq!(coerce_to_decl(Value::F(1.0), "sampler2D"), Value::F(1.0));
    }

    /// `WE_FX_DEFAULT_VEC` 的字面量解析：**官方写法**必须全部认。
    /// 出处是官方自己的切分器 `open-wallpaper-engine/src/Core/Json.cpp:88-114`
    /// （`ArrayComponents`：含 `,` ⇒ 按 `,` 切 + trim；否则按空格切）与
    /// `ShaderAnnot.cpp:190-204`（字符串 ⇒ 多分量；数字 ⇒ 单分量）。
    /// 语料/官方库里出现过的真实字面量形态逐个钉住 —— 覆盖 `foliagesway` 的
    /// `g_CornerWeights="1 1 0 0"` / `g_DirectionWeights="1 0.2"`、
    /// `zcompat` 的 `u_AASmoothness="0.02, 0.02"`（逗号 + 空格）、
    /// `u_BarBounds="0.0, 1.0"`、`g_CloudSpeeds="0.01 -0.02"`（负数）。
    #[test]
    fn ann_vec_default_accepts_official_literals() {
        assert_eq!(
            ann_vec_default("1 1 0 0", "vec4"),
            Some(Value::V4(1.0, 1.0, 0.0, 0.0))
        );
        assert_eq!(ann_vec_default("1 0.2", "vec2"), Some(Value::V2(1.0, 0.2)));
        assert_eq!(
            ann_vec_default("0.02, 0.02", "vec2"),
            Some(Value::V2(0.02, 0.02))
        );
        assert_eq!(
            ann_vec_default("0.0, 1.0", "vec2"),
            Some(Value::V2(0.0, 1.0))
        );
        assert_eq!(
            ann_vec_default("0.01 -0.02", "vec2"),
            Some(Value::V2(0.01, -0.02))
        );
        assert_eq!(
            ann_vec_default("1 0.75 0", "vec3"),
            Some(Value::V3(1.0, 0.75, 0.0))
        );
        // 前导/尾随空白也要吃得下（官方 `trim_ascii`）
        assert_eq!(ann_vec_default("  1 1 1 1 ", "vec4"), Some(Value::V4(1.0, 1.0, 1.0, 1.0)));
    }

    /// JSON **数组**字面量：`parse_shader_meta` 对 `"default":[1,1,1,1]` 会走
    /// `other.to_string` ⇒ 拿到的是 `"[1,1,1,1]"`；官方那边走 `Json.cpp:154-172`
    /// 的数组分支。两种写法必须同值。
    #[test]
    fn ann_vec_default_accepts_json_array_literal() {
        assert_eq!(
            ann_vec_default("[1,1,0,0]", "vec4"),
            Some(Value::V4(1.0, 1.0, 0.0, 0.0))
        );
        assert_eq!(ann_vec_default("[0.5, 0.5]", "vec2"), Some(Value::V2(0.5, 0.5)));
        assert_eq!(
            ann_vec_default("[1, 0.75, 0]", "vec3"),
            Some(Value::V3(1.0, 0.75, 0.0))
        );
    }

    /// 判据必须**严格**：切不出恰好 N 个数字就跳过（不猜、不补 0）。
    /// 反例来自官方库本身：`shaders/editorpaintbrush.frag:7/9` 声明 `vec4` 但 default
    /// 只有 2/3 个分量；另有空串、纹理名、单数字、非数字全部必须 `None`。
    #[test]
    fn ann_vec_default_rejects_wrong_arity_and_non_numbers() {
        assert_eq!(ann_vec_default("1 1 1", "vec4"), None);
        assert_eq!(ann_vec_default("0 0", "vec4"), None); // editorpaintbrush.frag:7
        assert_eq!(ann_vec_default("0 0 0", "vec4"), None); // editorpaintbrush.frag:9
        assert_eq!(ann_vec_default("0.5", "vec2"), None);
        assert_eq!(ann_vec_default("", "vec2"), None);
        assert_eq!(ann_vec_default("util/white", "vec2"), None);
        assert_eq!(ann_vec_default("1 x", "vec2"), None);
        assert_eq!(ann_vec_default("1 1 1 1 1", "vec4"), None);
        // 不在作用范围的类型一律 None（`Value` 没有对应变体 / 官方库 0 处）
        assert_eq!(ann_vec_default("1 1 1 1", "mat2"), None);
        assert_eq!(ann_vec_default("1 1", "ivec2"), None);
        assert_eq!(ann_vec_default("1 1", "float"), None);
        assert_eq!(ann_vec_default("1 1", "sampler2D"), None);
    }

    /// 作用范围只看**声明类型**（数据驱动）：官方库里出现过的向量族都要进，
    /// 能力不够的（`mat2`/`ivec*`/`bvec*`）与标量一律不进。
    #[test]
    fn ann_vec_scope_is_type_driven() {
        for ty in ["vec2", "vec3", "vec4", "mat3", "mat4"] {
            assert!(ann_vec_ty(ty), "{ty} 应在作用范围内");
        }
        for ty in ["float", "int", "mat2", "ivec2", "bvec4", "sampler2D", ""] {
            assert!(!ann_vec_ty(ty), "{ty} 不应在作用范围内");
        }
    }

    /// `mat` 族按**行主序**填：`Value::M4` 的既有口径 + `set_uniform` 会再 `t4` 一次
    /// ⇒ 两次下来 GL 里的矩阵 = 字面量写的那个（`m[r*4+c]` = 第 r 行第 c 列）。
    /// ⚠ 官方库 **0 处** `mat` 型注解 default ⇒ 没有真数据可验，只能靠这条单测钉住口径。
    #[test]
    fn ann_vec_default_matrices_are_row_major() {
        let id = ann_vec_default("1 0 0 0 0 1 0 0 0 0 1 0 0 0 0 1", "mat4");
        assert!(matches!(id, Some(Value::M4(_))));
        let Some(Value::M4(m)) = id else { panic!() };
        assert_eq!(m, [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0]);
        // `set_uniform` 的口径：GL 收的是 `t4(m)`（列主序展开）⇒ 第 0 列 = [0,4,8,12] 号元素
        let gl = crate::gl_ctx::t4(&m);
        assert_eq!(&gl[0..4], &[1.0, 0.0, 0.0, 0.0]);

        let seq = ann_vec_default(
            "1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16",
            "mat4",
        );
        let Some(Value::M4(m)) = seq else { panic!() };
        assert_eq!(m[0], 1.0);
        assert_eq!(m[1], 2.0); // 行主序：第 0 行第 1 列
        let gl = crate::gl_ctx::t4(&m);
        assert_eq!(&gl[0..4], &[1.0, 5.0, 9.0, 13.0]); // 第 0 列

        let Some(Value::M3(m3)) = ann_vec_default("1 2 3 4 5 6 7 8 9", "mat3") else { panic!() };
        assert_eq!(m3[1], 2.0);
        assert_eq!(&crate::gl_ctx::t3(&m3)[0..3], &[1.0, 4.0, 7.0]);
    }

    /// 造一个"只够 `we_scene::tex::flags` 解析"的最小 `.tex` 头：
    /// `TEXV0005\0` + `TEXI0001\0` + i32 format + u32 flags（后面的 TEXB/载荷用不到）。
    fn tex_header(flags: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"TEXV0005\0");
        v.extend_from_slice(b"TEXI0001\0");
        v.extend_from_slice(&0i32.to_le_bytes()); // format（RGBA8，内容无关）
        v.extend_from_slice(&flags.to_le_bytes());
        v
    }

    /// `.tex` 头部 `flags` 位域的解析（`bit0 = noInterpolation`、`bit1 = clampUVs`）。
    /// 采样状态是**数据里写死的** ⇒ 先把"每一种位组合读成什么"钉住。
    #[test]
    fn tex_flags_parse_covers_the_bit_field() {
        // 0x0 = 两个位都没置 ⇒ WE 是 REPEAT + LINEAR
        assert_eq!(we_scene::tex::flags(&tex_header(0x0)), Some(0));
        assert_eq!(we_scene::tex::repeat_wrap(&tex_header(0x0)), Some(true));
        assert_eq!(we_scene::tex::nearest_filter(&tex_header(0x0)), Some(false));

        // 0x1 = noInterpolation ⇒ NEAREST，wrap 仍是 REPEAT
        assert_eq!(we_scene::tex::repeat_wrap(&tex_header(0b1)), Some(true));
        assert_eq!(we_scene::tex::nearest_filter(&tex_header(0b1)), Some(true));

        // 0x2 = clampUVs ⇒ CLAMP_TO_EDGE（本机绝大多数贴图是这个）
        assert_eq!(we_scene::tex::repeat_wrap(&tex_header(0b10)), Some(false));
        assert_eq!(we_scene::tex::nearest_filter(&tex_header(0b10)), Some(false));

        // 0b11 = 两个位都置 ⇒ CLAMP + NEAREST
        assert_eq!(we_scene::tex::repeat_wrap(&tex_header(0b11)), Some(false));
        assert_eq!(we_scene::tex::nearest_filter(&tex_header(0b11)), Some(true));

        // 更高的位是别的语义（mip/流式等），不该影响 wrap/filter 的判读
        assert_eq!(we_scene::tex::repeat_wrap(&tex_header(0b100)), Some(true));
        assert_eq!(we_scene::tex::repeat_wrap(&tex_header(0x8000_0000)), Some(true));
        assert_eq!(we_scene::tex::repeat_wrap(&tex_header(u32::MAX)), Some(false));
        assert_eq!(we_scene::tex::nearest_filter(&tex_header(u32::MAX)), Some(true));

        // 头不合法 ⇒ 说不清（None），调用方必须按旧行为兜底
        assert_eq!(we_scene::tex::flags(b"not a tex at all"), None);
        assert_eq!(we_scene::tex::repeat_wrap(&[]), None);
        assert_eq!(we_scene::tex::repeat_wrap(b"TEXV0005\0"), None, "头截断也不能瞎猜");
    }

    /// 两种寻址**各自被选中**：`flags=0` ⇒ REPEAT、`flags=0b10` ⇒ CLAMP。
    /// 判据直接过"上传时用的那个决定"，而不是看常量表。
    #[test]
    fn tex_wrap_selection_follows_the_flag_and_the_switch() {
        // 数据说 REPEAT（bit1 未置）+ 开关默认开 ⇒ 选 REPEAT
        assert!(wrap_repeat(true, we_scene::tex::repeat_wrap(&tex_header(0x0))));
        // 数据说 CLAMP（bit1 置位）⇒ 无论开关如何都选 CLAMP
        assert!(!wrap_repeat(true, we_scene::tex::repeat_wrap(&tex_header(0b10))));
        assert!(!wrap_repeat(false, we_scene::tex::repeat_wrap(&tex_header(0b10))));
        // 解析不出来（不是 .tex）⇒ CLAMP 兜底（与改动前一致）
        assert!(!wrap_repeat(true, we_scene::tex::repeat_wrap(b"nope")));
        assert!(!wrap_repeat(true, None));
    }

    /// `WE_TEX_FLAGS=0` = **显式回退旧行为**（恒定 CLAMP_TO_EDGE）；
    /// 未设置 = 默认开（数据声明优先）。空串/写别的值都按开处理。
    #[test]
    fn tex_flags_switch_off_restores_the_old_clamp_behaviour() {
        // 默认（不设环境变量）⇒ 开
        assert!(tex_flags_enabled_from(None), "不设 WE_TEX_FLAGS 时必须默认开");
        // 显式回退
        assert!(!tex_flags_enabled_from(Some("0")), "WE_TEX_FLAGS=0 必须回旧行为");
        // 其它值仍然按开（历史上 "!=0 即开" 的口径不变）
        assert!(tex_flags_enabled_from(Some("1")));
        assert!(tex_flags_enabled_from(Some("")));
        assert!(tex_flags_enabled_from(Some("yes")));
        // 回退时：哪怕数据说 REPEAT，也必须按 CLAMP 采样（= 改动前逐字节相同）
        assert!(!wrap_repeat(
            tex_flags_enabled_from(Some("0")),
            we_scene::tex::repeat_wrap(&tex_header(0x0))
        ));
    }

    /// 数据回归：夏日影（工坊 `3704273480`）包里**只有 2 张**贴图未置 `clampUVs`
    /// （= REPEAT），且就是 `waterflowphase` 与 `waterripplenormal`。
    /// 这是"开这个默认值会影响谁"的名单钉死：45 张 `.tex`、43 CLAMP、2 REPEAT。
    /// 本机没有这条工坊内容（或换了机器）时**跳过**（打印一行说明），但只要有就断言。
    #[test]
    fn palm_package_has_exactly_two_repeat_textures() {
        let dir = std::env::var("WE_TEX_FLAGS_PKG_DIR")
            .ok()
            .map(std::path::PathBuf::from)
            .or_else(|| {
                let home = std::env::var("HOME").ok()?;
                Some(std::path::PathBuf::from(home).join(
                    ".local/share/Steam/steamapps/workshop/content/431960/3704273480",
                ))
            });
        let Some(dir) = dir else {
            eprintln!("[跳过] 没有 HOME，定位不到夏日影的场景目录");
            return;
        };
        let pkg_path = dir.join("scene.pkg");
        let Ok(bytes) = std::fs::read(&pkg_path) else {
            eprintln!("[跳过] 读不到 {}（本机没有这条工坊内容）", pkg_path.display());
            return;
        };
        let pkg = we_scene::pkg::Pkg::parse(&bytes).expect("scene.pkg 应能解析");

        let (mut total, mut clamp) = (0usize, 0usize);
        let mut repeat: Vec<String> = Vec::new();
        for e in &pkg.entries {
            if !e.name.to_ascii_lowercase().ends_with(".tex") {
                continue;
            }
            let Some(b) = pkg.slice(&bytes, e) else { continue };
            total += 1;
            match we_scene::tex::repeat_wrap(b) {
                Some(true) => repeat.push(e.name.clone()),
                Some(false) => clamp += 1,
                None => {} // 解析不出来 ⇒ 渲染器按 CLAMP 兜底，不算 REPEAT
            }
        }
        repeat.sort();
        assert_eq!(total, 45, "本包 `.tex` 条目数变了");
        assert_eq!(clamp, 43, "CLAMP 贴图数变了");
        assert_eq!(
            repeat,
            vec![
                "materials/effects/waterflowphase.tex".to_string(),
                "materials/effects/waterripplenormal.tex".to_string(),
            ],
            "REPEAT 名单变了：默认开会改变这些贴图的采样"
        );
    }

    /// 贴图**通道格式码**从头部读（`TEXV0005\0TEXI0001\0` 之后第一个 i32）。
    #[test]
    fn tex_format_code_is_read_from_the_header() {
        let mut b = b"TEXV0005\0TEXI0001\0".to_vec();
        b.extend_from_slice(&8i32.to_le_bytes());
        assert_eq!(SceneRenderer::we_tex_format_code(&b), Some(8), "RG88");
        b[18] = 0;
        assert_eq!(SceneRenderer::we_tex_format_code(&b), Some(0), "RGBA8888");
        b[18] = 9;
        assert_eq!(SceneRenderer::we_tex_format_code(&b), Some(9), "R8");
        assert_eq!(SceneRenderer::we_tex_format_code(b"nope"), None, "不是 .tex 头");
        assert_eq!(SceneRenderer::we_tex_format_code(&[]), None, "太短");
    }

    /// `ConvertTexture0Format` 语义：RG88 ⇒ **rgb=(r,r,r)、a=g**（旧解码产物是 `(r,g,0,255)`）。
    /// 正例 + 反侧反证（G=0 必须完全透明；只改颜色不改 alpha 的第一版会在这条上失败）。
    #[test]
    fn rg88_channel_format_becomes_grey_with_g_as_alpha() {
        let mut img = image::RgbaImage::from_raw(
            3,
            1,
            vec![200, 64, 0, 255, 255, 0, 0, 255, 32, 194, 0, 255],
        )
        .unwrap();
        SceneRenderer::apply_channel_fmt(&mut img);
        assert_eq!(img.get_pixel(0, 0).0, [200, 200, 200, 64], "rgb=R 灰度、a=G");
        assert_eq!(img.get_pixel(1, 0).0, [255, 255, 255, 0], "G=0 ⇒ 全透明（不是不透明白块）");
        assert_eq!(img.get_pixel(2, 0).0, [32, 32, 32, 194], "另一端同样成立");
    }

    /// `assets/shaders/common_fragment.h::ConvertTexture0Format` 的 **GL/非 HLSL 分支**，
    /// 逐字搬下来当"shader 侧"参照：`RG88 ⇒ _sample.rrrg`、` ⇒ vec4(1,1,1,_sample.r)`
    /// 其它格式（`0=RGBA8888`、DXT 等）在 shader 里就是**恒等映射**。
    fn gl_convert_texture0_format(fmt: i32, s: [f32; 4]) -> [f32; 4] {
        match fmt {
            8 | 10 => [s[0], s[0], s[0], s[1]],
            9 | 11 => [1.0, 1.0, 1.0, s[0]],
            _ => s,
        }
    }

    /// **本次根因的回归测试**：RG88 的通道语义必须**恰好应用一次**。
    /// `we-scene/src/tex.rs` 把 RG88 解成 `(r,g,0,1)`（R 恒 1.0、G = 强度/alpha）。两条**各自正确**
    /// 的单次实现：
    /// · shader 侧：贴图原样 + `TEX0FORMAT=8` ⇒ `_sample.rrrg` ⇒ `(1,1,1,g)` ← 默认路径
    /// · CPU 侧：先把像素改写成 `(1,1,1,g)`，`TEX0FORMAT=0`（shader 恒等）⇒ `(1,1,1,g)`
    /// 实测这两条**逐字节等价**（`WE_TEX_RGBA_FMT=1 + WE_PARTICLE_TEX0FORMAT=0` == 默认）。
    /// 两边同时作用 = 语义**复合**：`(1,1,1,g)` 再过一次 `.rrrg` ⇒ 第 4 分量取 `.g = 1.0`
    /// ⇒ `(1,1,1,1.0)`：**光束的柔光 alpha 被吃掉**，加性项从 `rgb·a·g` 变成 `rgb·a`
    /// ⇒ 整体 ×`1/ḡ`。
    #[test]
    fn rg88_semantics_must_be_applied_exactly_once() {
        // G 取光束剖面上的两个代表值（0 = 完全透明处、194 = 峰值）
        for g in [0.0f32, 194.0 / 255.0] {
            let sample = [255.0 / 255.0, g, 0.0, 1.0];
            let shader_only = gl_convert_texture0_format(8, sample);
            assert_eq!(shader_only, [1.0, 1.0, 1.0, g], "shader 单次 ⇒ alpha = G");

            let mut img = image::RgbaImage::from_raw(1, 1, vec![255, (g * 255.0).round() as u8, 0, 255])
                .unwrap();
            SceneRenderer::apply_channel_fmt(&mut img);
            let p = img.get_pixel(0, 0).0;
            let cpu_only = gl_convert_texture0_format(
                0,
                [
                    p[0] as f32 / 255.0,
                    p[1] as f32 / 255.0,
                    p[2] as f32 / 255.0,
                    p[3] as f32 / 255.0,
                ],
            );
            assert_eq!(cpu_only[0..3], shader_only[0..3], "CPU 单次：颜色相同（白）");
            assert!(
                (cpu_only[3] - shader_only[3]).abs() <= 1.0 / 255.0 + 1e-6,
                "CPU 单次：alpha 也 = G（8bit 量化内）"
            );

            let twice = gl_convert_texture0_format(8, cpu_only);
            assert_eq!(twice, [1.0, 1.0, 1.0, 1.0], "重复应用 ⇒ alpha 塌成 1.0（这就是过曝）");
            if g < 0.5 {
                assert_ne!(twice, shader_only, "重复应用必须与单次可区分，否则这条测试没意义");
            }
        }
    }

    /// 补位开关的另一半：**shader 已经在转换时，CPU 一个像素都不许碰**。
    #[test]
    fn cpu_channel_fmt_is_skipped_when_the_shader_already_converts() {
        let raw = vec![200u8, 64, 0, 255];
        let mut hdr = b"TEXV0005\0TEXI0001\0".to_vec();
        hdr.extend_from_slice(&8i32.to_le_bytes());

        let mut img = image::RgbaImage::from_raw(1, 1, raw.clone()).unwrap();
        assert_eq!(
            SceneRenderer::apply_channel_fmt_from_tex(&hdr, &mut img, false),
            None
        );
        assert_eq!(img.into_raw(), raw, "关着时像素必须逐字节不变");

        let mut img2 = image::RgbaImage::from_raw(1, 1, raw).unwrap();
        assert_eq!(
            SceneRenderer::apply_channel_fmt_from_tex(&hdr, &mut img2, true),
            Some(8)
        );
        assert_eq!(img2.into_raw(), vec![200, 200, 200, 64], "开着时才改写");
    }
}

#[cfg(test)]
mod texture_tests {
    use super::*;

    #[test]
    fn box_downsample_averages_the_footprint() {
        // 4x4 黑白棋盘 → 2x2：每格是 2x2 源像素的均值 ⇒ 黑+白各半 = 127（整数除）
        let mut img = image::RgbaImage::new(4, 4);
        for (x, y, p) in img.enumerate_pixels_mut() {
            let v = if (x + y) % 2 == 0 { 0 } else { 255 };
            *p = image::Rgba([v, v, v, 255]);
        }
        let out = box_downsample(&img, 2, 2);
        assert_eq!(out.dimensions(), (2, 2));
        for p in out.pixels() {
            assert_eq!(p[0], 127);
            assert_eq!(p[3], 255, "alpha 也要参与平均（这里全 255）");
        }
    }

    #[test]
    fn box_downsample_keeps_last_row_and_column() {
        // 只放右下角一个亮点：降采样后它必须还在（覆盖范围算错就会丢边）
        let mut img = image::RgbaImage::new(8, 8);
        for p in img.pixels_mut() {
            *p = image::Rgba([0, 0, 0, 255]);
        }
        img.put_pixel(7, 7, image::Rgba([255, 255, 255, 255]));
        let out = box_downsample(&img, 4, 4);
        assert!(out.get_pixel(3, 3)[0] > 0, "右下角的亮点不该被丢掉");
    }

    #[test]
    fn cap_texture_keeps_ratio_and_skips_small_textures() {
        let big = image::RgbaImage::new(400, 200);
        // cap 200 ⇒ 长边 200，等比 ⇒ 200x100
        let capped = cap_texture(big, 200, "t");
        assert_eq!(capped.dimensions(), (200, 100));
        let small = image::RgbaImage::new(64, 32);
        assert_eq!(cap_texture(small, 256, "t").dimensions(), (64, 32), "没超上限就别动");
        let small2 = image::RgbaImage::new(64, 32);
        assert_eq!(cap_texture(small2, 0, "t").dimensions(), (64, 32), "0 = 不限");
    }

    /// `bloomtint` 的解析：三通道正常、缺分量 ⇒ 白兜底（不要读出 0/NaN）。
    #[test]
    fn bloom_tint_parses_or_falls_back_to_white() {
        assert_eq!(parse_tint("1.00000 1.00000 1.00000"), [1.0, 1.0, 1.0]);
        assert_eq!(parse_tint("0.5 0.25 0.125"), [0.5, 0.25, 0.125]);
        assert_eq!(parse_tint(""), [1.0, 1.0, 1.0]);
        assert_eq!(parse_tint("1 1"), [1.0, 1.0, 1.0], "分量不够 ⇒ 白兜底");
    }

    /// Glow 机制：开关默认关（默认路径一字节不改）、暖色判据、光斑贴图中心的 alpha。
    /// 口径锁死：`glow_tint_for` 只把"R 明显高于 B 且够亮"的粒子当光源（白雾不算）。
    #[test]
    fn glow_tint_and_texture_are_sane() {
        // 橙火苗 ⇒ 光源；白雾/冷色 ⇒ 不是
        assert!(glow_tint_for([1.0, 0.5, 0.1]).is_some(), "橙火苗应算光源");
        assert!(glow_tint_for([1.0, 1.0, 1.0]).is_none(), "白雾不该算光源");
        assert!(glow_tint_for([0.05, 0.05, 0.2]).is_none(), "冷色不该算光源");
        // 偏暖化：B 被压低、R 保留
        let t = glow_tint_for([1.0, 0.5, 0.2]).unwrap();
        assert_eq!(t[0], 1.0);
        assert!(t[2] < 0.2, "光斑色应比粒子色更暖（B 更低）");
        // 光斑贴图：奇数边长 ⇒ 正中心 alpha=255，四角 0
        let px = glow_texture_rgba(17, 2.0);
        assert_eq!(px.len(), 17 * 17 * 4);
        assert_eq!(px[(8 * 17 + 8) * 4 + 3], 255, "中心应为满 alpha");
        assert_eq!(px[3], 0, "角落应为 0");
    }

    /// `WE_GLOW_TINT` 的解析：缺省/畸形 ⇒ 回落到传入的标定默认；负值夹到 0。
    #[test]
    fn glow_combine_tint_defaults_to_neutral() {
        let d = [1.0, 1.0, 1.0];
        assert_eq!(parse_glow_tint(None, d), d);
        assert_eq!(parse_glow_tint(Some(""), d), d);
        assert_eq!(parse_glow_tint(Some("1,1"), d), d, "分量数不对 ⇒ 回落");
        assert_eq!(parse_glow_tint(Some("x,y,z"), d), d);
        assert_eq!(parse_glow_tint(Some("1.0, 0.55, 0.30"), d), [1.0, 0.55, 0.30]);
        assert_eq!(parse_glow_tint(Some("-1,2,0.5"), d), [0.0, 2.0, 0.5], "负值夹到 0（不许减光）");
        // 标定默认值不参与解析，但必须是"有 R、压 G/B"的暖色
        assert!(GLOW_TINT_DEFAULT[0] >= GLOW_TINT_DEFAULT[1]);
        assert!(GLOW_TINT_DEFAULT[1] > GLOW_TINT_DEFAULT[2]);
    }

    /// `WE_GLOW_SOURCE` 的解析：缺省/写错 ⇒ `Local`（本次默认的点光源模型）；
    /// `screen` 回旧行为、`points` 只走粒子光斑。
    #[test]
    fn glow_source_defaults_to_local() {
        assert_eq!(parse_glow_source(None), GlowSource::Local);
        assert_eq!(parse_glow_source(Some("")), GlowSource::Local);
        assert_eq!(parse_glow_source(Some("junk")), GlowSource::Local, "写错 ⇒ 默认 local");
        assert_eq!(parse_glow_source(Some("local")), GlowSource::Local);
        assert_eq!(parse_glow_source(Some("screen")), GlowSource::Screen);
        assert_eq!(parse_glow_source(Some("points")), GlowSource::Points);
    }

    /// `WE_EXPOSURE` 的解析与**单位值 = 空字符串**（保证默认路径逐字节不变）。
    #[test]
    fn exposure_default_is_identity_and_emits_no_glsl() {
        let id = ExposureCfg::IDENTITY;
        assert!(id.is_identity());
        assert_eq!(id.glsl(), "", "单位值不许产生任何 GLSL（默认路径必须逐字节不变）");
        // gamma 1.0 + gain 1.0 但写了 reinhard ⇒ 也不算单位值
        assert!(!ExposureCfg { gain: 1.0, gamma: 1.0, warmth: 0.0, reinhard: true }.is_identity());
    }

    /// 标定值 `1.0,1.30`：只做显示编码 `c^(1/1.3)`，不加曝光。
    #[test]
    fn exposure_calibrated_spec_matches_gamma_only() {
        let c = ExposureCfg { gain: 1.0, gamma: 1.30, warmth: 0.0, reinhard: false };
        assert!(!c.is_identity());
        let g = c.glsl();
        assert!(g.contains("pow"), "要有编码：{g}");
        assert!(!g.contains("c *= "), "gain=1 不该产生曝光乘法：{g}");
        assert!(g.contains("0.76923"), "1/1.3 要烘成常数：{g}");
    }

    /// `glsl_for` 用的是传进来的变量名（bloom 的 combine 里那个 vec3 叫 albedo）。
    #[test]
    fn exposure_glsl_uses_requested_variable_name() {
        let c = ExposureCfg { gain: 1.2, gamma: 1.0, warmth: 0.0, reinhard: false };
        let g = c.glsl_for("albedo");
        assert!(g.contains("albedo *= 1.20000"), "{g}");
        assert!(!g.contains(" c "), "不该出现别的变量名：{g}");
    }

    /// reinhard 分支：先压缩高光再编码，且直流增益被归一化掉（0 → 0，1 → ≈1）。
    #[test]
    fn exposure_reinhard_normalises_round_trip() {
        let c = ExposureCfg { gain: 2.0, gamma: 1.0, warmth: 0.0, reinhard: true };
        let g = c.glsl_for("c");
        assert!(g.contains("(1.0 + c / 2.00000)"), "{g}");
        assert!(g.contains("1.50000"), "归一化系数 1+1/2：{g}");
    }

    /// 色温项：暖 = R 上 / B 下，且**排在编码之前**（先平衡再编码）。
    #[test]
    fn exposure_warmth_shifts_red_up_blue_down_before_encode() {
        let c = ExposureCfg { gain: 1.0, gamma: 1.3, warmth: 0.15, reinhard: false };
        let g = c.glsl_for("c");
        assert!(g.contains("c.r *= 1.01500"), "{g}");
        assert!(g.contains("c.b *= 0.98500"), "{g}");
        let (ir, ib) = (g.find("c.r *=").unwrap(), g.find("pow").unwrap());
        assert!(ir < ib, "色温要在显示编码之前：{g}");
    }

    /// -1：材质/特效 JSON 的 `passes[].combos` 合并 —— 大小写不敏感、已存在就地改写
    /// 认不出的值跳过（默认路径不经过这里，开关 `WE_MAT_COMBOS=1` 才调用）。
    #[test]
    fn merge_pass_combos_is_case_insensitive_and_skips_bad_values() {
        use serde_json::json;
        let src = "#ifndef VERSION\n#endif\n#if VERTICAL\n#endif\n";
        let mut combos: Vec<(String, i32)> = vec![("VERSION".to_string(), 0)];
        let mut kv: std::collections::BTreeMap<String, serde_json::Value> =
            std::collections::BTreeMap::new();
        kv.insert("version".to_string(), json!(2)); // 小写，shader 里是大写
        kv.insert("vertical".to_string(), json!(true));
        kv.insert("BAD".to_string(), json!("zzz")); // 认不出 ⇒ 不塞
        let n = merge_pass_combos(&mut combos, &kv, &[src]);
        assert_eq!(n, 2, "只应吃下两条可解析的");
        assert_eq!(combos[0], ("VERSION".to_string(), 2), "已存在的项要就地改写、键保持 shader 拼写");
        assert!(combos.iter().any(|(k, v)| k == "VERTICAL" && *v == 1));
        assert!(!combos.iter().any(|(k, _)| k == "BAD"));
        // shader 里没有对应拼写 ⇒ 保留原键（别的阶段可能引用）
        let mut c2: Vec<(String, i32)> = Vec::new();
        let mut kv2: std::collections::BTreeMap<String, serde_json::Value> =
            std::collections::BTreeMap::new();
        kv2.insert("Whatever".to_string(), json!(3));
        merge_pass_combos(&mut c2, &kv2, &[src]);
        assert_eq!(c2, vec![("Whatever".to_string(), 3)]);
    }

    /// clamp01 把越界顶点钳进 ±1，界内顶点一个 ulp 都不动。
    #[test]
    fn mesh_norm_pos_clamps_into_the_texture_grid() {
        let inside = mesh_norm_pos(&[10.0, 5.0, 0.0], 20.0, 10.0, 0.0, false, true);
        assert!((inside[0] - 0.5).abs() < 1e-6 && (inside[1] - 0.5).abs() < 1e-6);
        let out = mesh_norm_pos(&[60.0, -50.0, 0.0], 20.0, 10.0, 0.0, false, true);
        assert_eq!((out[0], out[1]), (1.0, -1.0));
        let raw = mesh_norm_pos(&[60.0, -50.0, 0.0], 20.0, 10.0, 0.0, false, false);
        assert!((raw[0] - 3.0).abs() < 1e-6 && (raw[1] + 5.0).abs() < 1e-6);
    }

    // ════════════ ：水波时间项（`WE_WAVE_NPHASE`）════════════
    // 判据是 shader 自己那两行 `distance`（不是特效名）：只改 `g_Time`，
    // 空间项 / 遮罩采样 / 内容采样一个字节不动。
    const WW: &str = "void main() {\n\
        \tfloat distance = g_Time * g_Speed + dot(texCoordMotion, v_Direction) * g_Scale;\n\
        \tfloat distance2 = (g_Time + g_Offset2) * g_Speed2 + dot(texCoordMotion, v_Direction2) * g_Scale2;\n\
        \tfloat t = g_Time;\n\
        \ttexCoord += val1 * s1 * offset * strength * mask;\n}\n";

    #[test]
    fn wave_nphase_hold_zeroes_only_the_time_term() {
        let (out, hits) = wave_nphase_rewrite(WW, 1);
        assert_eq!(hits, 2, "两条 distance 都该命中");
        assert!(out.contains("float distance = 0.0 * g_Speed + dot("));
        assert!(out.contains("float distance2 = (0.0 + g_Offset2) * g_Speed2 + dot("));
        // 空间项与遮罩采样保持原样（撕不撕不取决于它们，别顺手改）
        assert!(out.contains("* g_Scale;"));
        assert!(out.contains("texCoord += val1 * s1 * offset * strength * mask;"));
        // **其它**用 g_Time 的地方一律不动（水波族只有这两行，但判据要成立）
        assert!(out.contains("\tfloat t = g_Time;"));
    }

    #[test]
    fn wave_nphase_live_is_identity_and_other_shaders_untouched() {
        assert_eq!(wave_nphase_rewrite(WW, 0), (WW.to_string(), 0));
        // 非水波族（没有 `float distance`）⇒ 命中 0、源码逐字节不变
        let other = "void main() {\n\tgl_FragColor = vec4(g_Time, 0.0, 0.0, 1.0);\n}\n";
        let (o, h) = wave_nphase_rewrite(other, 1);
        assert_eq!((o.as_str(), h), (other, 0));
    }

    #[test]
    fn wave_nphase_norm_is_periodic_identity_on_paper() {
        // `mod(g_Time, 2π/g_Speed) * g_Speed` 与 `g_Time * g_Speed` 差 2πk ⇒ sin 相同。
        // 这里只钉"改写形态"：真正的等价性由 sin 的周期性保证（见函数注释）。
        let (out, hits) = wave_nphase_rewrite(WW, 2);
        assert_eq!(hits, 2);
        assert!(out.contains("mod(g_Time, 6.28318530718 / max(g_Speed, 0.0001)) * g_Speed"));
        assert!(out.contains("max(g_Speed2, 0.0001)"));
        let g = 6.28318530718f64;
        for speed in [2.0f64, 3.0, 4.0] {
            for t in [0.0f64, 0.37, 3.0, 7.9, 123.4] {
                let a = (t * speed).sin();
                let b = ((t % (g / speed)) * speed).sin();
                assert!((a - b).abs() < 1e-9, "speed={speed} t={t}");
            }
        }
    }

    /// `waterwaves.frag` 的真身（CRLF、双波分支关）：只留与本改写相关的行。
    const WW_FULL: &str = "void main() {\r\n\
        \tfloat pos = abs(dot((texCoordMotion - 0.5), v_Direction));\r\n\
        \tfloat distance = g_Time * g_Speed + dot(texCoordMotion, v_Direction) * g_Scale;\r\n\
        \tvec2 offset = vec2(v_Direction.y, -v_Direction.x);\r\n\
        \tfloat val1 = sin(distance);\r\n\
        \ttexCoord += val1 * s1 * offset * strength * mask;\r\n\
        \tgl_FragColor = texSample2D(g_Texture0, texCoord);\r\n\
        }\r\n";

    /// `amp0`：位移项 → 0（输出 uv = 原 uv），时间项/空间项**不动**
    /// （不动时间项是为了让这个档位只表达"位移归零"这一件事，A/B 时变量单一）。
    #[test]
    fn wave_nphase_amp0_zeroes_the_displacement_term_only() {
        let (out, hits) = wave_nphase_rewrite(WW_FULL, 3);
        assert_eq!(hits, 1, "只该命中位移累加那一行");
        assert!(out.contains("\ttexCoord += vec2(0.0);\r\n"), "位移项必须归零：{out}");
        // 时间项原样（`g_Time` 还在）：这一档不是"冻时间"，是"位移=0"
        assert!(out.contains("float distance = g_Time * g_Speed + dot("));
        // 输出行原样（uv 已是原 uv ⇒ 等价于没有这一趟）
        assert!(out.contains("gl_FragColor = texSample2D(g_Texture0, texCoord);"));
        // CRLF 一个不多一个不少
        assert_eq!(
            out.matches("\r\n").count(),
            WW_FULL.matches("\r\n").count()
        );
    }

    /// `shape`（默认）：**alpha 取自原 uv** ⇒ 轮廓静止；rgb 仍来自位移采样
    /// ⇒ 水波的颜色/折射流动保留。这是"复位"与"别把效果修没"两条判据同时成立的唯一形态。
    #[test]
    fn wave_nphase_shape_pins_alpha_to_rest_uv_and_keeps_displaced_rgb() {
        let (out, hits) = wave_nphase_rewrite(WW_FULL, 4);
        assert_eq!(hits, 1);
        // 原输出行（采样位移坐标）必须被替换掉，否则轮廓还是会跟着走
        assert!(!out.contains("gl_FragColor = texSample2D(g_Texture0, texCoord);"));
        // 覆盖来自原 uv
        assert!(out.contains("vec4 weRest = texSample2D(g_Texture0, v_TexCoord.xy);"));
        assert!(out.contains("gl_FragColor = vec4(min(weRgb, vec3(weRest.a)), weRest.a);"));
        // 颜色来自位移坐标（水波仍在颜色里）
        assert!(out.contains("vec4 weDisp = texSample2D(g_Texture0, texCoord);"));
        // 位移=0 时必须恒等（判据）：用 mix 而不是 over 复合 —— over 会把同一层的颜色
        // 在半透明像素上算两遍（躯干框实测 2.71% 像素变亮）
        assert!(out.contains("vec3 weRgb = mix(weRest.rgb, weDisp.rgb, weDisp.a);"));
        assert!(!out.contains("weRest.rgb * (1.0 - weDisp.a)"));
        // 位移累加行与时间项都不动（时间线照走 ⇒ 颜色在流动）
        assert!(out.contains("\ttexCoord += val1 * s1 * offset * strength * mask;\r\n"));
        assert!(out.contains("float distance = g_Time * g_Speed + dot("));
        // CRLF：替换 1 行 → 4 行 ⇒ 多 3 个行尾
        assert_eq!(
            out.matches("\r\n").count(),
            WW_FULL.matches("\r\n").count() + 3
        );
    }

    /// 3/4 两档的**安全闸**：锚点行缺一条就一个字节都不改（宁可不动，也不改错）。
    #[test]
    fn wave_nphase_amp_modes_need_both_anchor_lines() {
        // `WW` 有位移累加行，但没有 `gl_FragColor = texSample2D(g_Texture0, texCoord)`
        assert_eq!(wave_nphase_rewrite(WW, 3), (WW.to_string(), 0));
        assert_eq!(wave_nphase_rewrite(WW, 4), (WW.to_string(), 0));
        // 反过来：只有输出行、没有位移累加行
        let only_out = "void main() {\n\tgl_FragColor = texSample2D(g_Texture0, texCoord);\n}\n";
        assert_eq!(wave_nphase_rewrite(only_out, 3), (only_out.to_string(), 0));
        assert_eq!(wave_nphase_rewrite(only_out, 4), (only_out.to_string(), 0));
        // 无关 shader 两种档位都不碰
        let other = "void main() {\n\tgl_FragColor = vec4(g_Time, 0.0, 0.0, 1.0);\n}\n";
        assert_eq!(wave_nphase_rewrite(other, 3), (other.to_string(), 0));
        assert_eq!(wave_nphase_rewrite(other, 4), (other.to_string(), 0));
    }
}


/// 环境开关（**默认开**，`=0` 关）：不设 / 空 ⇒ **开**；`0` / `false` / `off` / `no`（忽略大小写与空白）⇒ 关。
/// 把"与官方 Win-WE 对齐"的那批特性从「`=1` 才开」改成「**默认开、`=0` 可关**」——
/// 目的：**源码默认行为 = 用户屏上所见的对齐行为**（重装 / 换机 / 不带 drop-in 也不会退回旧画面），
/// 同时保留 `=0` 做 A/B 与回退。
pub(crate) fn flag_default_on(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => true,
    }
}

// ────────────────────────── WE_FUSE_SHIFT ──────────────────────────
// 把同一层里**连续一段纯位移特效趟**折成一趟（默认关）。语义 = 路径展开
// （`fuse.rs` 顶部的推导）：n 趟是**嵌套** `u := c_i(u)`，不是位移相加；`mix` 趟
// （shake 的 MASK 分支）把路径一分为二，最后 `sum_k w_k * base(coord_k)`。
// 为什么值得：实测这条 12 趟链（层 1016）在 2200x3094 上是 0.102 ms/兆像素/趟的
// 纯填充/带宽代价，且与采样数无关 ⇒ 9 趟 e3..e11 折成 1 趟约省 5 ms/帧。
// 默认关：关着时 `fuse_tpl` 恒空、`try_fuse_chain` 一次都不调用 ⇒ 趟表、RT 分配、
// shader 编译与改动前**逐字节相同**（护栏基线）。
fn fuse_on() -> bool {
    match std::env::var("WE_FUSE_SHIFT") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "off" | "no"
        ),
        Err(_) => false,
    }
}

/// 一个特效趟的融合模板（只在 `WE_FUSE_SHIFT` 打开时记）。
#[derive(Clone)]
struct FuseTpl {
    vs: String,
    fs: String,
    combos: Vec<(String, i32)>,
    /// 烘焙改写**之前**的槽位表（只用来知道"这趟有哪些槽"，实际采样源取 Pass.units）
    units: Vec<(u32, Src)>,
    scalars: Vec<(String, Value)>,
    obj: i64,
    /// 本趟之前插入的烘焙趟：(烘焙趟下标, 原池 RT, 槽)
    bakes: Vec<(usize, usize, u32)>,
}

impl SceneRenderer {
    fn fuse_compose_run(&self, run: &[usize]) -> Result<fuse::Fused, String> {
        let mut stages = Vec::with_capacity(run.len());
        for &pi in run {
            let t = self.fuse_tpl.get(&pi).ok_or("没有融合模板")?;
            stages.push(fuse::Stage {
                vs: t.vs.clone(),
                fs: t.fs.clone(),
                combos: t.combos.clone(),
                slots: t.units.iter().map(|(k, _)| *k).collect(),
            });
        }
        fuse::compose(&stages)
    }

    /// 第 `j` 趟能不能接在 `run` 后面（Opaque / 同尺寸 / 输入 = 上一趟输出 / 无脚本）。
    fn fuse_reject(&self, run: &[usize], j: usize) -> Option<String> {
        let prev = *run.last()?;
        let cur = &self.passes[j];
        if cur.blend != gl_ctx::Blend::Opaque {
            return Some("这一趟不是 Opaque（内部 RT→RT）".into());
        }
        let (pt, ct) = match (self.passes[prev].target, cur.target) {
            (Some(a), Some(b)) => (a, b),
            _ => return Some("目标之一是屏幕".into()),
        };
        let (pw, ph) = (self.rts[pt].w, self.rts[pt].h);
        let (cw, ch) = (self.rts[ct].w, self.rts[ct].h);
        if (pw, ph) != (cw, ch) {
            return Some(format!("目标尺寸不同 {pw}x{ph} vs {cw}x{ch}"));
        }
        let t = self.fuse_tpl.get(&j)?;
        let input = t.units.iter().find(|(k, _)| *k == 0).map(|(_, s)| *s);
        match input {
            Some(Src::Rt(r)) if r == pt => {}
            other => return Some(format!("输入 {other:?} 不是上一趟的目标 RT#{pt}")),
        }
        if self.fx_scripts.iter().any(|(pi, _)| *pi == j) {
            return Some("这一趟带脚本标量".into());
        }
        if self.fx_frame_scalars.iter().any(|(pi, _)| *pi == j) {
            return Some("这一趟带逐帧脚本标量".into());
        }
        None
    }

    /// 在 `[start, end)` 这段本层趟里找最长的连续可融合段，逐段折成一趟。
    fn try_fuse_chain(&mut self, obj_id: i64, start: usize, end: usize) -> Result<(), String> {
        let debug = std::env::var_os("WE_FUSE_DEBUG").is_some();
        if debug {
            let mut keys: Vec<usize> = self
                .fuse_tpl
                .keys()
                .copied()
                .filter(|k| *k >= start && *k < end)
                .collect();
            keys.sort_unstable();
            eprintln!("[fuse] 层{obj_id} 区间 [{start},{end}) 的模板趟 = {keys:?}");
        }
        let mut i = start;
        let mut planned: Vec<(Vec<usize>, fuse::Fused)> = Vec::new();
        while i < end {
            if !self.fuse_tpl.contains_key(&i) {
                i += 1;
                continue;
            }
            let mut run: Vec<usize> = vec![i];
            let mut fused: Option<fuse::Fused> = None;
            loop {
                let prev = *run.last().unwrap_or(&i);
                // 下一个"有模板"的趟；中间那些趟**必须**是它自己的遮罩烘焙趟
                // （烘焙趟是紧挨在特效趟前面插进来的，所以融合段里它们是"夹心"）。
                let mut j = prev + 1;
                while j < end && !self.fuse_tpl.contains_key(&j) {
                    j += 1;
                }
                if j >= end {
                    break;
                }
                let between: Vec<usize> = (prev + 1..j).collect();
                let all_bakes = match self.fuse_tpl.get(&j) {
                    Some(t) => between
                        .iter()
                        .all(|p| t.bakes.iter().any(|(b, _, _)| b == p)),
                    None => false,
                };
                if !all_bakes {
                    if debug {
                        eprintln!(
                            "[fuse] 层{obj_id} 趟{j} 断开：中间有非烘焙趟 {between:?}"
                        );
                    }
                    break;
                }
                if let Some(why) = self.fuse_reject(&run, j) {
                    if debug {
                        eprintln!("[fuse] 层{obj_id} 趟{j} 断开：{why}");
                    }
                    break;
                }
                let mut cand = run.clone();
                cand.push(j);
                match self.fuse_compose_run(&cand) {
                    Ok(f) => {
                        run = cand;
                        fused = Some(f);
                    }
                    Err(e) => {
                        if debug {
                            eprintln!("[fuse] 层{obj_id} 趟{j} 融不进去：{e}");
                        }
                        break;
                    }
                }
            }
            match fused {
                Some(f) => {
                    let last = *run.last().unwrap_or(&i);
                    planned.push((run, f));
                    i = last + 1;
                }
                None => i += 1,
            }
        }
        for (run, f) in planned {
            if let Err(e) = self.apply_fuse(obj_id, &run, f) {
                if debug {
                    eprintln!("[fuse] 层{obj_id} 放弃（趟 {run:?}）：{e}");
                }
            }
        }
        Ok(())
    }

    /// 真正落地一段融合：搬烘焙 RT → 拼 units/scalars → 编译 → 追加一趟 → 标记跳过。
    fn apply_fuse(
        &mut self,
        obj_id: i64,
        run: &[usize],
        f: fuse::Fused,
    ) -> Result<(), String> {
        let debug = std::env::var_os("WE_FUSE_DEBUG").is_some();
        let last = *run.last().ok_or("空 run")?;
        let last_target = self.passes[last].target.ok_or("末趟目标不是 RT")?;
        let base = match self.fuse_tpl.get(&run[0]).and_then(|t| {
            t.units
                .iter()
                .find(|(k, _)| *k == 0)
                .map(|(_, s)| *s)
        }) {
            Some(s) => s,
            None => return Err("首趟没有 unit 0".into()),
        };
        // ⚠ 反馈环：首趟的输入 RT 就是末趟的输出 RT 时，融成一趟会"又读又写"同一张。
        // 奇偶乒乓下这必然发生（偶长度 run）⇒ 直接放弃这一段。
        if let Src::Rt(b) = base {
            if b == last_target {
                return Err(format!("输入 RT#{b} == 输出 RT#{last_target}（会成反馈环）"));
            }
        }
        // ⚠ 四边形**朝向**也要继承（放在分配任何 RT 之前，不成立就什么都不动）。
        // 链内"读 RT 写 RT"的趟都被 `WE_FX_QUAD_FLIP`（默认开，推导见 build 里链内趟
        // 几何那段）设成 `raw_position = true`（V 反向的 vao），而 `make_fx_card` 建出来
        // 的是**不反向**那一份。融合趟只画一次，朝向错了就整层上下镜像 —— 实测
        // （层 1016/主题2，1920x1080 t=5）：不继承时融合趟写出的目标 RT 覆盖率
        // 21% → 1.6%，整层几乎全空，融合输出与基线的 mean|Δ| 停在 16.3。
        let fx_flip = self.passes[last].layer.raw_position;
        if self.passes[run[0]].layer.raw_position != fx_flip {
            return Err("段内首/末趟四边形朝向不一致，放弃融合".into());
        }
        // ⚠ **顶点几何**也必须是同一套（3504393434 整幅黑屏的根因）。
        // WE_FX_CARDSPACE（默认开）下链内趟的顶点是官方卡片网格 ±size/2，MVP 里已乘 2/size；
        // 而 make_fx_card 建的是 ±1 单位四边形 ⇒ 同一个 MVP 把 ±1 变成 ±0.0005（3840 宽）
        // ⇒ 只有目标 RT 正中 ~2x2 像素被写，其余保持清屏黑 ⇒ 整层全黑。
        // 实测：亮度 122 → 0，WE_FUSE_IDENTITY=1 也全黑（不是 shader 语义，是没画到画布上）。
        // 判据只看几何字段（不看 layer id / 名字）：整段必须同为卡片（同一 half）或同为单位四边形；
        // 自定义四边形 / 索引网格（shape 收尾趟、puppet 网格）一律拒绝。
        let mut fused_half: Option<[f32; 2]> = self.passes[run[0]].layer.card_half;
        for &pi in run.iter() {
            let l = &self.passes[pi].layer;
            if l.custom_quad {
                return Err("段内有自定义四边形/索引网格趟（shape/网格），融合趟无法复现其几何"
                    .into());
            }
            match (fused_half, l.card_half) {
                (Some(a), Some(b))
                    if (a[0] - b[0]).abs() < 0.5 && (a[1] - b[1]).abs() < 0.5 => {}
                (None, None) => {}
                (a, b) => {
                    return Err(format!("段内顶点几何不一致：{a:?} vs {b:?}（会画空）"));
                }
            }
        }
        // 1) 每个 stage 的烘焙趟搬到自己那张 RT（`cur_mask_warp_pool` 是被复用的）
        let mut bake_moves: Vec<(usize, usize, usize)> = Vec::new();
        // (stage, slot) -> that stage own bake RT. Must NOT be looked up by source RT:
        // the pool is reused, all 7 stages bake from the same pooled RT#37.
        let mut slot_new: std::collections::HashMap<(usize, u32), usize> =
            std::collections::HashMap::new();
        let mut retargeted: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for (si, &pi) in run.iter().enumerate() {
            let t = match self.fuse_tpl.get(&pi) {
                Some(t) => t.clone(),
                None => continue,
            };
            for (bpi, wrt, slot) in t.bakes.iter().copied() {
                let used = f.unit_map.iter().any(|(s, k, _)| *s == si && *k == slot);
                if !used || bake_moves.iter().any(|(b, _, _)| *b == bpi) {
                    continue;
                }
                let (w, h) = (self.rts[wrt].w, self.rts[wrt].h);
                let name = format!("_rt_maskfuse{}_{}_{}", si, obj_id, slot);
                let nr = self.new_rt(w, h, &name)?;
                if let Some(rep) = self.rt_report.get(wrt).copied().flatten() {
                    if nr < self.rt_report.len() {
                        self.rt_report[nr] = Some(rep);
                    }
                }
                if let Some(lg) = self.rt_logical.get(wrt).copied().flatten() {
                    if nr < self.rt_logical.len() {
                        self.rt_logical[nr] = Some(lg);
                    }
                }
                if debug {
                    eprintln!(
                        "[fuse]   bake move: 趟{bpi} 池RT#{wrt} {}x{} -> RT#{nr} {}x{}",
                        self.rts[wrt].w, self.rts[wrt].h, self.rts[nr].w, self.rts[nr].h
                    );
                }
                bake_moves.push((bpi, wrt, nr));
                slot_new.insert((si, slot), nr);
                retargeted.insert(bpi);
            }
        }
        // 2) 安全闸：被搬走的池 RT 不能被**别的**趟读（否则那段内容的提供者就断了）
        let mut skip: std::collections::HashSet<usize> = run.iter().copied().collect();
        skip.extend(retargeted.iter().copied());
        let moved: Vec<usize> = bake_moves.iter().map(|(_, o, _)| *o).collect();
        if !moved.is_empty() {
            for (pi, p) in self.passes.iter().enumerate() {
                if skip.contains(&pi) {
                    continue;
                }
                for (k, s) in p.units.iter() {
                    if let Src::Rt(r) = *s {
                        if !moved.contains(&r) {
                            continue;
                        }
                        // 只在这一趟"最近一次写 RT#r 的趟"就是被搬走的烘焙趟时才有问题；
                        // 别的特效有它自己的烘焙趟在写同一张池 RT（池按层复用）。
                        let writer = (0..pi).rev().find(|q| self.passes[*q].target == Some(r));
                        match writer {
                            Some(w) if !retargeted.contains(&w) => {}
                            _ => {
                                return Err(format!(
                                    "趟 {pi}（槽 {k}）会读到被搬走的烘焙 RT#{r}"
                                ))
                            }
                        }
                    }
                }
            }
        }
        // 3) units：unit 0 = 整串输入，其余按 `unit_map` 逐个 stage 的槽
        let mut units: Vec<(u32, Src)> = Vec::with_capacity(1 + f.unit_map.len());
        units.push((0, base));
        for (si, slot, g) in f.unit_map.iter().copied() {
            if slot == 0 {
                continue;
            }
            let pi = run[si];
            let src = match self.passes[pi].units.iter().find(|(k, _)| *k == slot) {
                Some((_, s)) => *s,
                None => {
                    // 原趟**声明了但没绑**的槽（WE 的 combo 槽常见：`g_Texture3` 带
                    // `{"combo":"MASK"}` 而场景没给图）⇒ 原趟的 sampler 是未赋值的，
                    // GL 里等于 0 号 unit（= 该趟的输入）。融合后拿不到"该趟的输入"，
                    // 就用整串的输入图顶（位移掩码量级 ~1e-3 uv，实测差在亚像素级）。
                    if std::env::var_os("WE_FUSE_DEBUG").is_some() {
                        eprintln!(
                            "[fuse] 层{obj_id} 第 {si} 趟（趟 {pi}）槽 {slot} 原本没绑，用整串输入顶"
                        );
                    }
                    base
                }
            };
            let src = match slot_new.get(&(si, slot)) {
                Some(nr) => Src::Rt(*nr),
                None => src,
            };
            units.push((g, src));
        }
        // 4) scalars：逐 stage 加前缀（共享名保持原名，交给 draw_pass 的全局下发）
        let mut scalars: Vec<(String, Value)> = Vec::new();
        for (si, &pi) in run.iter().enumerate() {
            if let Some(t) = self.fuse_tpl.get(&pi) {
                for (k, v) in t.scalars.iter() {
                    if fuse::is_shared_name(k) {
                        scalars.push((k.clone(), *v));
                    } else {
                        scalars.push((format!("s{si}_{k}"), *v));
                    }
                }
            }
        }
        if debug {
            for (k, v) in scalars.iter() {
                eprintln!("[fuse]   scalar {k} = {v:?}");
            }
        }
        // 5) 编译 + 追加（几何/混合/MVP 与末趟一致 ⇒ 输出落在同一张 RT 的同一像素上）
        let (w, h) = (self.rts[last_target].w, self.rts[last_target].h);
        let mvp = self.passes[last].mvp;
        // 第二道几何闸（防"静默画空"）：极值顶点过一遍 MVP，必须落在裁剪空间的合理倍数内。
        // 单位四边形配卡片 MVP（= 本次 3504393434 的真实故障）时 |ndc| = 0.0005 ⇒ 一眼可见。
        let extent = fused_half.unwrap_or([1.0, 1.0]);
        let ndc_ext = mvp_max_ndc(&mvp, extent);
        if !(0.25..=4.0).contains(&ndc_ext) {
            return Err(format!(
                "融合趟几何落不进目标 RT（顶点 ±{extent:?} → 极值 |ndc| = {ndc_ext:.4}）"
            ));
        }
        let mut layer = match fused_half {
            // 卡片网格（官方口径）：用同一个 half 重建，MVP 原样沿用（它已经含 2/size）。
            Some(half) => {
                let (l, ok) = self.gl.make_layer_card_vp(&f.vs, &f.fs, half)?;
                if !ok {
                    return Err("融合趟的顶点不吃 g_ModelViewProjectionMatrix，卡片几何不成立".into());
                }
                l
            }
            // 我们的 ±1 单位四边形（WE_FX_CARDSPACE 关 / 非图层空间）
            None => self.gl.make_fx_card(&f.vs, &f.fs, (w, h), 0.0)?,
        };
        // 与它替换的那些链内趟同一个四边形朝向（见上面 `fx_flip` 那段推导）
        layer.raw_position = fx_flip;
        let n_units = units.len();
        if debug {
            eprintln!(
                "[fuse]   目标 RT#{last_target} {}x{}",
                self.rts[last_target].w, self.rts[last_target].h
            );
            for (u, s) in units.iter() {
                let (_, tw, th) = self.src_tex(*s);
                eprintln!("[fuse]   unit {u} {s:?} {tw}x{th}");
            }
        }
        // ⚠ **位置必须留在原处**（实测的第二处根因）：融合趟替换的是 run 的**末趟**，
        // 不能 `push` 到 pass 表末尾 —— 追加会把它排到本层**其余**特效趟之后（水波后面的
        // waterflow / waterripple / localcontrast / shine …），那些趟随后读到的是融合前的旧 RT，
        // 于是整条链只剩融合趟的效果（`3504393434`：mean|Δ| 1.23、块>4 7.05%，与分辨率无关）。
        // `3704273480` 一直没暴露是因为它那段 run 恰好是**该层的最后一段**（末趟之后没有趟）。
        self.passes[last] = Pass {
            layer,
            target: Some(last_target),
            units,
            scalars,
            mvp,
            blend: gl_ctx::Blend::Opaque,
            mesh_anim: None,
        };
        // 只跳过**被替换掉的**那些趟；末趟已被原地换成融合趟，不能再跳过。
        for &pi in run.iter() {
            if pi != last {
                self.fuse_skip.insert(pi);
            }
        }
        for (bpi, _, nr) in bake_moves.iter().copied() {
            self.passes[bpi].target = Some(nr);
        }
        if debug {
            for (si, &pi) in run.iter().enumerate() {
                let m = &self.passes[pi].mvp;
                eprintln!(
                    "[fuse]   第 {si} 趟({pi}) mvp=({:.4},{:.4},{:.4},{:.4})",
                    m[0], m[5], m[12], m[13]
                );
            }
        }
        if let Ok(p) = std::env::var("WE_FUSE_DUMP") {
            let _ = std::fs::write(format!("{p}.fuse{}.vert", run[0]), &f.vs);
            let _ = std::fs::write(format!("{p}.fuse{}.frag", run[0]), &f.fs);
        }
        eprintln!(
            "[fuse] 层{obj_id} 融合 {} 趟 {run:?} → 替换趟 #{}（target RT#{last_target}、路径 {}、槽 {}、烘焙搬走 {}）",
            run.len(),
            last,
            f.paths,
            n_units,
            bake_moves.len()
        );
        if debug {
            eprintln!("[fuse] 层{obj_id} unit_map={:?}", f.unit_map);
        }
        Ok(())
    }
}


#[cfg(test)]
mod setprop_fast_tests {
    use super::*;

    fn style<'a>(halign: &'a str, color: [f32; 3]) -> crate::text::TextStyle<'a> {
        crate::text::TextStyle {
            font_px: 64.0,
            padding: 4.0,
            halign,
            valign: "center",
            color,
            plate: None,
            plate_follow_glyph: false,
            firstline: true,
            canvas_grow: false,
            ascent_box: false,
            dy_canvas: 0.0,
        }
    }

    /// **这条测试是"换色不用重建"的全部依据**：同样的覆盖率掩膜 + 新色，
    /// 必须与"用新色重新光栅化"**逐字节相同**（同一个 `(c*255).round`）。
    /// 失败就意味着两条路会出不同的像素 ⇒ 局部刷新不许上屏。
    #[test]
    fn recolor_matches_rasterize() {
        let Ok(font) = std::fs::read("/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf") else {
            eprintln!("跳过：本机没有 NotoSansMono-Regular.ttf");
            return;
        };
        let old = [0.349f32, 0.573, 0.827];
        let new = [0.2f32, 0.8, 0.05];
        let (a, w, h) = crate::text::rasterize(&font, "Mouse pad 123", &style("center", old), 512, 128)
            .expect("旧色光栅化");
        let alpha: Vec<u8> = (0..(w as usize * h as usize)).map(|i| a[i * 4 + 3]).collect();
        let got = recolor_from_alpha(&alpha, new);
        let (want, w2, h2) =
            crate::text::rasterize(&font, "Mouse pad 123", &style("center", new), 512, 128)
                .expect("新色光栅化");
        assert_eq!((w2, h2), (w, h), "掩膜口径必须与画布尺寸一致");
        assert_eq!(got.len(), want.len());
        assert_eq!(got, want, "换色结果必须与重新光栅化逐字节相同");
        // 覆盖率一位都不能动
        let alpha2: Vec<u8> = (0..(w as usize * h as usize)).map(|i| got[i * 4 + 3]).collect();
        assert_eq!(alpha, alpha2);
    }

    /// 子块上传的**像素等价性**（与整张上传同一份数据）：
    /// `alpha_bbox` 给出的矩形内，`recolor_rect` 必须与 `recolor_from_alpha` 的对应区域
    /// **逐字节相同**；矩形外 `a == 0`（两条路都不可见）⇒ 换色不会漏。
    #[test]
    fn rect_recolor_matches_full_recolor() {
        let (w, h) = (37u32, 23u32);
        // 伪随机但确定的掩膜：中间一块有内容，四周全 0
        let mut alpha = vec![0u8; (w * h) as usize];
        for y in 5..18 {
            for x in 7..30 {
                alpha[(y * w + x) as usize] = ((x * 7 + y * 13) % 251) as u8;
            }
        }
        let rgb = [0.2f32, 0.8, 0.05];
        let (x0, y0, sw, sh) = alpha_bbox(&alpha, w, h).expect("有内容");
        assert_eq!((x0, y0, sw, sh), (7, 5, 23, 13));
        let sub = recolor_rect(&alpha, w, h, (x0, y0, sw, sh), rgb);
        let full = recolor_from_alpha(&alpha, rgb);
        assert_eq!(sub.len(), (sw * sh * 4) as usize);
        for y in 0..sh as usize {
            for x in 0..sw as usize {
                let fi = (((y0 as usize + y) * w as usize) + (x0 as usize + x)) * 4;
                let si = (y * sw as usize + x) * 4;
                assert_eq!(
                    &sub[si..si + 4],
                    &full[fi..fi + 4],
                    "子块 ({x},{y}) 与整张不一致"
                );
            }
        }
        // 全透明 ⇒ 没有可画的像素（连传都不用传）
        assert_eq!(alpha_bbox(&vec![0u8; (w * h) as usize], w, h), None);
    }

    #[test]
    fn parses_color_triplets_and_hex() {
        let c = parse_color_triplet("0.349 0.573 0.827").expect("三元组");
        assert!((c[0] - 0.349).abs() < 1e-6 && (c[2] - 0.827).abs() < 1e-6);
        let h = parse_color_triplet("#FF8000").expect("hex");
        assert!((h[0] - 1.0).abs() < 1e-6 && (h[1] - 128.0 / 255.0).abs() < 1e-6 && h[2] == 0.0);
        assert_eq!(parse_color_triplet("true"), None);
        assert_eq!(parse_color_triplet("1 1"), None);
    }

    /// `scene.json` 里 `"color": {"user": "text5"}` 必须被认成"文字层换色"这条路，
    /// 而 `"visible": {"user": ...}` / 非文字对象一律不认（退回重建）。
    #[test]
    fn scans_only_text_color_bindings() {
        let json = br#"{"objects":[
            {"id":691,"pointsize":256.0,"text":{"value":"x"},"color":{"user":"text5","value":"0.349 0.573 0.827"}},
            {"id":5,"image":"materials/a.tex","color":{"user":"matcolor","value":"1 1 1"}},
            {"id":7,"pointsize":30.0,"visible":{"user":"newproperty1","value":true}}
        ]}"#;
        let m = scan_text_color_bindings(json);
        assert_eq!(m.get("text5"), Some(&691));
        assert!(!m.contains_key("matcolor"), "材质色不是烘在贴图里的，必须走重建");
        assert!(!m.contains_key("newproperty1"), "visible 绑定必须走重建");
    }

    #[test]
    fn fast_path_is_off_by_default() {
        // 默认关：不设环境变量时这张表是空的 ⇒ 一条属性都不会走局部刷新
        assert!(!setprop_fast_on() || std::env::var("WE_SETPROP_FAST").is_ok());
    }

    /// 播放速度：默认 1.0 时**必须逐位**等于原墙钟（默认路径不许有任何浮点漂移）。
    #[test]
    fn clock_scale_default_is_bit_identical() {
        for wall in [0.0f32, 1.0, 12.345_679, 3600.5] {
            for prev_wall in [0.0f32, 0.5, 11.9] {
                assert_eq!(
                    advance_scene_time(3.0, prev_wall, wall, 1.0).to_bits(),
                    wall.to_bits(),
                    "scale=1 必须原样返回墙钟（prev_wall={prev_wall} wall={wall}）"
                );
            }
        }
    }

    /// 倍率只乘**增量**（增量夹在 0.25 s 内，与粒子步长同口径）。
    #[test]
    fn clock_scale_multiplies_the_delta_only() {
        // 一帧 0.2 s 的墙钟：2× ⇒ 场景走 0.4 s；0.25× ⇒ 0.05 s；3× ⇒ 0.6 s
        assert!((advance_scene_time(10.0, 10.0, 10.2, 2.0) - 10.4).abs() < 1e-6);
        assert!((advance_scene_time(10.0, 10.0, 10.2, 0.25) - 10.05).abs() < 1e-6);
        assert!((advance_scene_time(10.0, 10.0, 10.2, 3.0) - 10.6).abs() < 1e-6);
        // 卡了一大帧（10 s）：增量先夹到 0.25 s，再乘倍率 ⇒ 不会一口吞掉 20 s
        assert!((advance_scene_time(10.0, 10.0, 20.0, 2.0) - 10.5).abs() < 1e-6);
        // 墙钟倒退（不该发生）⇒ 场景时刻停住，绝不倒退
        assert_eq!(advance_scene_time(10.0, 10.0, 9.0, 2.0), 10.0);
        // 连续两帧累加：10.0 →(0.1s×2) 10.2 →(0.1s×2) 10.4
        let a = advance_scene_time(10.0, 10.0, 10.1, 2.0);
        let b = advance_scene_time(a, 10.1, 10.2, 2.0);
        assert!((b - 10.4).abs() < 1e-6);
    }

    /// 夹取：非法值 → 1.0；越界 → 边界；合法值原样。
    #[test]
    fn time_scale_clamps_to_the_ui_range() {
        assert_eq!(set_time_scale(0.0), TIME_SCALE_MIN);
        assert_eq!(set_time_scale(1e9), TIME_SCALE_MAX);
        assert_eq!(set_time_scale(f32::NAN), 1.0);
        assert_eq!(set_time_scale(-2.0), TIME_SCALE_MIN);
        assert!((set_time_scale(1.75) - 1.75).abs() < 1e-6);
        assert!((time_scale() - 1.75).abs() < 1e-6);
        // 收尾回默认，别影响同进程里别的用例
        set_time_scale(1.0);
        assert_eq!(time_scale(), 1.0);
    }
}
