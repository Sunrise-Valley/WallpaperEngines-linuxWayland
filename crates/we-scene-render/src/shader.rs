// SPDX-License-Identifier: MIT
//! WE shader 的加载与适配。
//! 三件事：
//!
//! 1. **定位**：shader 可能在包里（`shaders/<名>.vert|frag`），也可能是 WE 内置的（在 assets 里）
//!
//! 2. **解析 `#include`**：WE 的 shader 会 `#include "common_vertex.h"` 等，头文件在 assets `/shaders/`
//!
//! 3. **补前导块**：WE 自己在 `.h` 里**只使用不定义** `mul`/`saturate`/`texSample2D`/`CAST3` ——
//! 引擎在编译前注入这段兼容代码（换后端就换它）。这里我们自己写。

use std::path::Path;

#[derive(serde::Deserialize, Debug, Default)]
#[serde(default)]
pub struct MaterialFile {
    pub passes: Vec<MaterialPass>,
}

/// `effect.json`（特效定义：它自己的离屏目标 + 若干趟）
#[derive(serde::Deserialize, Debug, Default)]
#[serde(default)]
pub struct EffectFile {
    pub name: String,
    /// 特效的**分组**（`effect.json` 的 `group`）——最要紧的是 `"localeffects"`：
    /// 局部特效画在**图层自己的矩形**里（例如「音频条」「Tech Circle」「圆角遮罩」），
    /// 而不是像 `blur`/`tint`/`scroll`（`group` = blur/colorize/animate）那样对整屏做后处理。
    /// 判「合成/透传层要不要跑图层空间」就靠它。
    pub group: String,
    pub fbos: Vec<EffectFbo>,
    pub passes: Vec<EffectPass>,
    pub dependencies: Vec<String>,
    /// 特效**自带**的数值常量（编辑器参数名 → 值）：模糊半径、采样数、边缘阈值这类。
    /// ⚠ 之前完全没解析 ⇒ 这些 uniform 全空 ⇒ 表现是"特效在跑但量不对"（实测：Ice Princess
    /// 的 blur/shine 雾与光晕偏强）。
    /// `WE_JSON_NULLMAP=1` 时容忍 `"constantshadervalues" : null`。
    #[serde(deserialize_with = "we_scene::json::de_map_or_empty")]
    pub constantshadervalues: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(serde::Deserialize, Debug, Default)]
#[serde(default)]
pub struct EffectFbo {
    pub name: String,
    pub scale: f32,
    pub format: String,
}

#[derive(serde::Deserialize, Debug, Default)]
#[serde(default)]
pub struct EffectPass {
    pub material: String,
    /// 输出到哪个离屏目标（空 = 屏幕）
    pub target: Option<String>,
    /// 输入绑定：名字 → 贴图槽
    pub bind: Vec<EffectBind>,
    /// 单趟自己的数值常量（覆盖 EffectFile 级；场景对象级再覆盖它）。
    /// `WE_JSON_NULLMAP=1` 时容忍 `"constantshadervalues" : null`。
    #[serde(deserialize_with = "we_scene::json::de_map_or_empty")]
    pub constantshadervalues: std::collections::HashMap<String, serde_json::Value>,
    /// -1：**这一趟自己声明的编译期 combo**（`effect.json` 的 `passes[].combos`）。
    /// 官方写法示例见 `effects/blur/materials/effects/blur_gaussian_y.json` 的 `{"VERTICAL":1}`。
    /// 值用 `serde_json::Value` 而不是 `i64`：**编成 i64 会让一个 bool/字符串值把整个
    /// effect.json 反序列化搞失败**（连累这一趟被整体跳过）。取用时统一 `as_i64`/bool 折算。
    /// `WE_JSON_NULLMAP=1` 时容忍 `"combos" : null`。
    #[serde(deserialize_with = "we_scene::json::de_map_or_empty")]
    pub combos: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(serde::Deserialize, Debug, Default)]
#[serde(default)]
pub struct EffectBind {
    pub name: String,
    pub index: i32,
}

#[derive(serde::Deserialize, Debug, Default)]
#[serde(default)]
pub struct MaterialPass {
    pub shader: String,
    /// ⚠ `"textures": [null, "effects/waterripplenormal"]` —— **`null` 是 WE 的合法写法**
    /// （= 该贴图槽没东西）。以前声明成 `Vec<String>` ⇒ serde 碰到 `null` 直接报
    /// `invalid type: null, expected a string` ⇒ **整个材质反序列化失败** ⇒ 调用方那条
    /// `else { continue }` 把**整趟特效静默跳过**。
    /// 实测影响面（本机 17 条场景 349 个材质）：**15 个材质**踩这个形状，全是
    /// `materials/effects/waterripple.json` / `waterflow.json` ⇒ 水波纹/水流这一族
    /// 在 12/17 条场景里**一次都没生效过**（Gaze 的"水面不波动"就是它）。
    /// 修法：用宽容反序列化把 `null`（以及任何非字符串标量）折成空串 —— 空串的语义与
    /// \"该槽没有贴图\"完全一致，下游 `if !t.is_empty` 的判断不用改。
    #[serde(deserialize_with = "de_vec_str_null_empty")]
    pub textures: Vec<String>,
    pub blending: Option<String>,
    pub cullmode: Option<String>,
    pub depthtest: Option<String>,
    pub depthwrite: Option<String>,
    /// -1：**材质自己声明的编译期 combo**（`materials/*.json` 的 `passes[].combos`）。
    /// 官方出处：`assets/effects/blur/materials/effects/blur_gaussian_y.json:8-10`
    /// `{"combos":{"VERTICAL":1}}`、`blur_precise_gaussian_y.json:8-11`
    /// `{"VERTICAL":1,"ENABLEMASK":1}`；层材质里 `VERSION=2` 命中本机 **10 条场景 / 66 个可见层**
    /// （`assets/shaders/genericimage2.frag:7/68` 的 `#ifndef VERSION` 决定走不走"现代"分支，
    /// 现代分支用 `g_Color4` 染色、旧分支用 `g_Brightness/g_UserAlpha`）——以前这一整条通道
    /// **没接** ⇒ 分支选错、丢掉对象色偏。
    /// ⚠ 官方同一批材质里 `"VERSION"` 与 `"version"` **两种拼写都有**
    /// （`materials/SteamLogoShattered.json` 小写、`materials/Ceremonial.json` 大写），
    /// 而 shader 里写的是大写 ⇒ 合并时**必须大小写不敏感**（见 `shader::canonical_combo_name`）。
    /// `WE_JSON_NULLMAP=1` 时容忍 `"combos" : null`。
    #[serde(deserialize_with = "we_scene::json::de_map_or_empty")]
    pub combos: std::collections::BTreeMap<String, serde_json::Value>,
}

/// 把 `[null, "x", 3]` 这类贴图槽数组折成 `["", "x", "3"]`（`null` ⇒ 空串）。
fn de_vec_str_null_empty<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let v = Vec::<serde_json::Value>::deserialize(d)?;
    Ok(v.into_iter()
        .map(|x| match x {
            serde_json::Value::String(s) => s,
            serde_json::Value::Null => String::new(),
            other => other.to_string(),
        })
        .collect())
}

/// 引擎注入的兼容前导块（GLSL ES 1.00）
pub const PRELUDE: &str = r#"
// ── WE 兼容前导块（我们自己提供；WE 的 .h 只使用不定义这些） ──
// `mul` 的**操作数顺序必须是反的**：WE 的 shader 写的是 HLSL 的 `mul(a, b)`，
// 而 HLSL 的矩阵是**行主序、`m[i][j]` = 第 i 行第 j 列**、`mul(v, M)` = **行向量 × 矩阵**。
// 同一段源码在 GLSL 里 `m[i][j]` 是**第 i 列第 j 行**（列主序），于是
// · 源码里 `m[i][j] = x` 写出来的 GLSL 矩阵 = HLSL 矩阵的**转置**；
// · GLSL 的 `a * b`（向量在左）= Mᵀ·v，和 HLSL 的 `mul(v, M)` 差一次转置。
// 两条**合起来**才等价：`mul(a, b)` 在 GLSL 里必须写成 `(b) * (a)`。
// 实测证据（3704273480 的 `光束 - 角`，lightshafts.vert 的
// `mat3 xform = inverse(squareToQuad(point0..3)); v_TexCoordFx = mul(vec3(uv,1), xform);`）：
// 用 `(a)*(b)` 时 GPU 算出的 `v_TexCoordFx = (0.00, 0, 2.29)`（探针 `WE_SHAPE_DEBUG=rawv`/`vz8` 读数），
// `v_TexCoordFx.xy/z = (-0.005, -0.42)` **不在** 单位方块里 ⇒ 角向 smoothstep 归零 ⇒
// `mask ≡ 0` ⇒ 该层一个像素都不画（`WE_DUMP_RT` 里 `_rt_imageLayerComposite_254_b`/`_rt_chain_254` 全零）。
// 换成 `(b)*(a)` 后 `v_TexCoordFx.xy/z` 回到 [0,1]² ⇒ 光柱出现。
// ⚠ 换这个宏的同时必须**把上传的矩阵一并转置**（见 `gl_ctx` 的 `up_transposed`/`t4`），
// 否则所有"引擎上传的矩阵"（MVP/模型矩阵）会整体转置 ⇒ 全库画面错乱。
// 两边一起换 = 上传矩阵那条路**逐位不变**（只有 shader 内部自己算出来的矩阵被修对）。
#define mul(a, b) ((b) * (a))
#define saturate(x) clamp(x, 0.0, 1.0)
#define CAST2(x) vec2(x)
#define CAST3(x) vec3(x)
#define CAST4(x) vec4(x)
#define CAST3X3(x) mat3(x)
#define CAST4X4(x) mat4(x)
#define lerp(a, b, t) mix(a, b, t)
#define frac(x) fract(x)
#define fmod(x, y) mod(x, y)
#define atan2(y, x) atan(y, x)
#define sincos(x, s, c) { s = sin(x); c = cos(x); }
vec4 texSample2D(sampler2D s, vec2 uv) { return texture2D(s, uv); }
vec4 texSample2DLod(sampler2D s, vec2 uv, float lod) { return texture2D(s, uv); } // 真正的 LOD 采样以后再加
// `inverse` 是 GLSL **1.40** 才有的内置函数，而我们固定在 `#version 120`。
// 实测 shape 对象的 `lightshafts` vert 直接调 `inverse(...)` ⇒ 不补就整层编译失败
// （报错 `0:186(15): error: no function with name 'inverse'`）。
// 下面是标准的 4x4 求逆（伴随矩阵/determinant 展开），det=0 时退化为单位阵（别让 NaN 扩散）。
mat4 inverse(mat4 m) {
    float a00 = m[0][0], a01 = m[0][1], a02 = m[0][2], a03 = m[0][3];
    float a10 = m[1][0], a11 = m[1][1], a12 = m[1][2], a13 = m[1][3];
    float a20 = m[2][0], a21 = m[2][1], a22 = m[2][2], a23 = m[2][3];
    float a30 = m[3][0], a31 = m[3][1], a32 = m[3][2], a33 = m[3][3];
    float b00 = a00 * a11 - a01 * a10, b01 = a00 * a12 - a02 * a10;
    float b02 = a00 * a13 - a03 * a10, b03 = a01 * a12 - a02 * a11;
    float b04 = a01 * a13 - a03 * a11, b05 = a02 * a13 - a03 * a12;
    float b06 = a20 * a31 - a21 * a30, b07 = a20 * a32 - a22 * a30;
    float b08 = a20 * a33 - a23 * a30, b09 = a21 * a32 - a22 * a31;
    float b10 = a21 * a33 - a23 * a31, b11 = a22 * a33 - a23 * a32;
    float det = b00 * b11 - b01 * b10 + b02 * b09 + b03 * b08 - b04 * b07 + b05 * b06;
    if (abs(det) < 1e-12) { return mat4(1.0); }
    det = 1.0 / det;
    return mat4(
        (a11 * b11 - a12 * b10 + a13 * b09) * det, (a02 * b10 - a01 * b11 - a03 * b09) * det,
        (a31 * b05 - a32 * b04 + a33 * b03) * det, (a22 * b04 - a21 * b05 - a23 * b03) * det,
        (a12 * b08 - a10 * b11 - a13 * b07) * det, (a00 * b11 - a02 * b08 + a03 * b07) * det,
        (a32 * b02 - a30 * b05 - a33 * b01) * det, (a20 * b05 - a22 * b02 + a23 * b01) * det,
        (a10 * b10 - a11 * b08 + a13 * b06) * det, (a01 * b08 - a00 * b10 - a03 * b06) * det,
        (a30 * b04 - a31 * b02 + a33 * b00) * det, (a21 * b02 - a20 * b04 - a23 * b00) * det,
        (a11 * b07 - a10 * b09 - a12 * b06) * det, (a00 * b09 - a01 * b07 + a02 * b06) * det,
        (a31 * b01 - a30 * b03 - a32 * b00) * det, (a20 * b03 - a21 * b01 + a22 * b00) * det);
}
mat3 inverse(mat3 m) {
    float a00 = m[0][0], a01 = m[0][1], a02 = m[0][2];
    float a10 = m[1][0], a11 = m[1][1], a12 = m[1][2];
    float a20 = m[2][0], a21 = m[2][1], a22 = m[2][2];
    float b01 =  a22 * a11 - a12 * a21;
    float b11 = -a22 * a10 + a12 * a20;
    float b21 =  a21 * a10 - a11 * a20;
    float det = a00 * b01 + a01 * b11 + a02 * b21;
    if (abs(det) < 1e-12) { return mat3(1.0); }
    det = 1.0 / det;
    return mat3(b01, (-a22 * a01 + a02 * a21), (a12 * a01 - a02 * a11),
                b11, ( a22 * a00 - a02 * a20), (-a12 * a00 + a02 * a10),
                b21, (-a21 * a00 + a01 * a20), ( a11 * a00 - a01 * a10)) * det;
}
"#;

/// `#require <Name>` —— **WE 专有标记，不是 GLSL 预处理指令**。
/// 原样交给 GL 会报 `preprocessor error: Illegal non-directive after #`（实测 4/17 条壁纸卡在这，
/// 报错行就是那一行 `#require LightingV1`）。参考实现（OWE `ShaderParser.cpp`）的做法：
/// `LightingV1` 换成真正的片段，**其它名字整行注释掉**。这里对齐。
const LIGHTING_V1: &str = r#"
// ── #require LightingV1（WE 专有 include 标记）──
uniform vec3 g_LightsPosition[4];
uniform vec4 g_LightsColorRadius[4];
uniform vec4 g_LightsDirectionType[4];
uniform vec4 g_LightsConeExponent[4];
uniform float g_LightsCastShadow[4];
// 简化实现：方向光/位置光都按"灯→点"方向算 PBR，不实现阴影贴图与锥角衰减。
// 没场景灯 ⇒ 返回 0，画面就只剩场景自己的 g_LightAmbientColor 项（和 WE 的结构一致）。
vec3 PerformLighting_V1(vec3 worldPos, vec3 albedo, vec3 normal, vec3 viewVector,
                        vec3 specularTint, vec3 f0, float roughness, float metallic) {
    vec3 light = vec3(0.0);
    for (int i = 0; i < 4; ++i) {
        float type = g_LightsDirectionType[i].w;
        vec3 lcolor = g_LightsColorRadius[i].rgb;
        if (type < -0.5 || dot(lcolor, lcolor) <= 0.0) continue;
        vec3 L = normalize((type > 1.5) ? -g_LightsDirectionType[i].xyz
                                        : (g_LightsPosition[i] - worldPos));
        light += ComputePBRLightShadowInfinite(normal, L, viewVector, albedo, lcolor,
                                               specularTint, f0, roughness, metallic, 1.0);
    }
    return light;
}
"#;

/// 把 `#require X` 处理掉（返回替换后的整段源码）
pub fn resolve_requires(src: &str) -> String {
    if !src.contains("#require") {
        return src.to_string();
    }
    let mut out = String::with_capacity(src.len());
    for line in src.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("#require") {
            let name = rest.trim();
            if name == "LightingV1" {
                out.push_str(LIGHTING_V1);
            } else {
                out.push_str(&format!("// ⚠ 未实现的 #require {name}（注释掉，免得 GL 预处理报错）"));
            }
            out.push('\n');
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// 把 `#include "x.h"` 递归展开（先查包里，再查 WE assets 的 shaders/）
pub fn resolve_includes(
    src: &str,
    pkg: &we_scene::pkg::Pkg,
    pkg_bytes: &[u8],
    assets: &Path,
    depth: usize,
) -> String {
    if depth > 8 {
        return src.to_string(); // 防相互包含
    }
    // WE 专有标记先换掉（include 进来的文本在递归里同样会走到这一步）
    let src = resolve_requires(src);
    let src = src.as_str();
    let mut out = String::with_capacity(src.len());
    for line in src.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("#include") {
            let name = rest.trim().trim_matches('"').trim_matches('<').trim_matches('>');
            let text = lookup(name, pkg, pkg_bytes, assets);
            match text {
                Some(txt) => {
                    out.push_str(&format!("\n// ── include {name} ──\n"));
                    out.push_str(&resolve_includes(&txt, pkg, pkg_bytes, assets, depth + 1));
                    out.push('\n');
                }
                None => out.push_str(&format!("\n// ⚠ 找不到 include {name}\n")),
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn lookup(name: &str, pkg: &we_scene::pkg::Pkg, pkg_bytes: &[u8], assets: &Path) -> Option<String> {
    for cand in [format!("shaders/{name}"), name.to_string()] {
        if let Some(b) = pkg.get(pkg_bytes, &cand) {
            return Some(String::from_utf8_lossy(b).to_string());
        }
    }
    for cand in [
        assets.join("shaders").join(name),
        assets.join(name),
    ] {
        if let Ok(t) = std::fs::read_to_string(&cand) {
            return Some(t);
        }
    }
    None
}

/// 读一个 shader 源（包内优先，否则 WE 内置），并展开 include
pub fn load_shader(
    pkg: &we_scene::pkg::Pkg,
    pkg_bytes: &[u8],
    assets: &Path,
    name: &str,
    ext: &str,
) -> Result<String, String> {
    let raw = lookup(&format!("{name}.{ext}"), pkg, pkg_bytes, assets)
        .ok_or_else(|| format!("找不到 shader {name}.{ext}（包内和 WE assets 都没有）"))?;
    let expanded = resolve_includes(&raw, pkg, pkg_bytes, assets, 0);
    // 作者源码里会出现**多一个 `#endif`**（HLSL 编译器忽略，GL 预处理器报
    // `#endif without #if` ⇒ 整个阶段编译失败）。展开 include 之后统一容错。
    // `WE_HLSL_COMPAT=0` 退回改动前（逐字节）。
    let expanded = if glsl_compat_on() {
        drop_stray_directives(&expanded)
    } else {
        expanded
    };
    Ok(hoist_decls(&expanded))
}

/// 把**全局作用域**上的 `uniform` / `varying` / `attribute` 声明提到文件最前。
/// 为什么必须做：WE 的 shader 常写成
/// ```glsl
/// #include "common_blur.h" // 里面的函数体就读 g_Texture0
/// uniform sampler2D g_Texture0; // 声明却在 include 之后
/// ```
/// 而 GLSL 是**先声明后使用** ⇒ 直接编译报 `` `g_Texture0' undeclared ``，
/// 整条特效链作废（症状：图层整个不见，比如 `3299815325` 的人物）。
/// WE 自己的 shader 编译器就是把全局声明收集起来放在最前面 —— 这里照做。
/// 只搬**单行完整声明**（`...;` 结尾）且不在 `{}` 里的行；`#if` 守卫**不跟着搬**
/// （多个未使用的 sampler 声明是无害的，而留在原地反而会让守卫里的代码继续引用不到）。
pub fn hoist_decls(src: &str) -> String {
    // `#version` / `#extension` 必须留在最前（注释与空行无所谓，这里一并跳过）
    let lines: Vec<&str> = src.lines().collect();
    let is_head = |l: &str| {
        let t = l.trim_start();
        t.is_empty() || t.starts_with("//") || t.starts_with("/*") || t.starts_with("*")
            || t.starts_with("#version") || t.starts_with("#extension")
    };
    let mut ins = 0usize;
    while ins < lines.len() && is_head(lines[ins]) {
        ins += 1;
    }

    let mut decls: Vec<&str> = Vec::new();
    let mut rest: Vec<&str> = Vec::new();
    let mut depth: i32 = 0;
    // `#if` 嵌套层数：**被条件编译包住的声明不能搬** —— 搬出去就等于让它无条件编译，
    // 而守卫里的声明本身可能只在某个 combo 下才是合法 GLSL（实测：`#if SKINNING` 里的
    // `attribute uvec4 a_BlendIndices;` 在 `#version 120` 下直接语法错）。
    let mut guards: i32 = 0;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        if t.starts_with("#if") {
            guards += 1;
        } else if t.starts_with("#endif") {
            guards -= 1;
            if guards < 0 {
                guards = 0;
            }
        }
        // 行尾的元数据注释（`// {...}`）不参与"是不是完整声明"的判断，但要留在行里
        let code = t.split("//").next().unwrap_or("").trim_end();
        let looks_decl = ["uniform ", "varying ", "attribute "]
            .iter()
            .any(|k| t.starts_with(k))
            && code.ends_with(';')
            // 带下标尺寸的（可能是 `[NUM_LIGHTS]` 这种宏）不搬：搬走会跑到达 `#define` 前面
            && !code.contains('[');
        if depth == 0 && guards == 0 && looks_decl && i >= ins {
            decls.push(l);
        } else {
            rest.push(l);
        }
        depth += l.matches('{').count() as i32 - l.matches('}').count() as i32;
        if depth < 0 {
            depth = 0;
        }
    }
    if decls.is_empty() {
        return src.to_string();
    }
    let mut out = String::with_capacity(src.len() + 64);
    for l in rest.iter().take(ins) {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str(&format!("// ── 全局声明提到这里（{} 条）──\n", decls.len()));
    for d in &decls {
        out.push_str(d);
        out.push('\n');
    }
    out.push_str("// ── 以下是原文件内容（全局声明已上移）──\n");
    for l in rest.iter().skip(ins) {
        out.push_str(l);
        out.push('\n');
    }
    out
}

/// GLSL ES 要求 `#if` 里用到的宏**必须先定义**（桌面 GL 才把未定义当 0）。
/// 规则：**凡出现在 `#if` / `#elif` 表达式里的标识符，一律补 0**；
/// 只出现在 `#ifdef` / `#ifndef` 里的（例如 `VERSION`）**保持未定义**，语义才不会被改坏
/// （一旦给它补上 0，`#ifdef` 就变真了）。
/// 注意要**顶点与片元一起扫**：同一个 combo 可能在一个阶段写成 `#if X`、
/// 在另一个阶段写成 `#ifdef X`，两阶段定义不一致会导致 varying 声明不匹配、链接失败。
pub fn combo_defaults(bodies: &[&str]) -> Vec<String> {
    let mut must_stay_undefined: Vec<String> = Vec::new();
    let mut in_if: Vec<String> = Vec::new();
    for body in bodies {
        for line in body.lines() {
            let t = line.trim_start();
            if let Some(rest) = t.strip_prefix("#ifdef") {
                collect_idents(rest, &mut must_stay_undefined);
            } else if let Some(rest) = t.strip_prefix("#ifndef") {
                collect_idents(rest, &mut must_stay_undefined);
            } else if let Some(rest) = t.strip_prefix("#if").or_else(|| t.strip_prefix("#elif")) {
                collect_idents(rest, &mut in_if);
            }
        }
    }
    let mut out: Vec<String> = Vec::new();
    for id in in_if {
        // 已经在前导块里定义过的（mul/saturate/…）别再补 0
        if PRELUDE.contains(&format!("#define {id}(")) || PRELUDE.contains(&format!("#define {id} ")) {
            continue;
        }
        let _ = &must_stay_undefined; // 只出现在 #ifdef 的宏根本不会进 in_if，无需额外过滤
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// 抓 C 标识符（跳过 `defined` 与数字）
fn collect_idents(expr: &str, out: &mut Vec<String>) {
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if !cur.is_empty() {
            if cur != "defined" && !cur.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                out.push(cur.clone());
            }
            cur.clear();
        }
    };
    for ch in expr.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else {
            flush(&mut cur, out);
        }
    }
    flush(&mut cur, out);
}

/// shader 自带元数据（WE 把"编辑器参数 ↔ uniform/combo"的映射写成 JSON 注释贴在声明后面）：
/// ```glsl
/// uniform float g_Threshold; // {"material":"ui_editor_properties_ray_threshold","default":0.5}
/// uniform sampler2D g_Texture1; // {"material":"ui_editor_properties_opacity_mask","default":"util/white"}
/// // [COMBO] {"material":"ui_editor_properties_noise","combo":"NOISE","type":"options","default":1}
/// ```
/// 有了它就不用猜：场景里给的 `ui_editor_properties_*` 常量/开关，直接对到 uniform 名与 combo 名；
/// 纹理没给时也知道该退回哪个内置资源。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ShaderMeta {
    /// combo 声明：编辑器参数 → combo 名 + 默认值
    pub combos: Vec<ComboDecl>,
    /// uniform 声明：uniform 变量名 → 编辑器参数 + 默认值（默认值是字符串，纹理时是资源名）
    pub uniforms: Vec<UniformDecl>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ComboDecl {
    pub prop: String,
    pub combo: String,
    pub default: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UniformDecl {
    pub name: String,
    pub prop: String,
    pub default: String,
    /// 声明里的类型（`float`/`vec2`/`vec4`/`mat4`/`sampler2D`/…）——
    /// 给"按类型决定兜底值"用（拿 `glUniform1f` 去喂一个 `vec2` 会报 GL 错）
    pub ty: String,
    /// 注解里的 `"range":[lo,hi]` 的**下界**。`lo>0` 表示 **0 是作者合同外的值**
    /// （编辑器滑杆给不出 0）⇒ 场景不给时**不能用 0 兜底**：shader 里常写成
    /// `1.0/x`、`Res.z/Res.w * g_Ratio` 这类，0 ⇒ `Inf` ⇒ 再乘个 0 就是 `NaN`
    /// ⇒ 采样坐标 NaN ⇒ **整屏平色**（实测 `2482408969` 的 `foliagesway`）。
    pub range_lo: Option<f32>,
    /// 注解里的 `"combo":"MASK"` —— **该槽位有贴图时，这个 combo 就该是 1**（WE 的规则）
    pub combo: String,
    /// 注解里的 `"mode":"flowmask"` —— WE 用它声明「这个槽是画在**画布空间**的位移场/
    /// 方向图」。这类槽不能当帧空间贴图直接采样：作者画的是画布空间的场，特效趟却拿
    /// 帧空间 uv 去采 ⇒ 位移落到错的位置（ 实测：髋部被推）。缺省空串。
    pub mode: String,
    /// 注解里的 `"require":{"ENABLEMASK":1}` —— 该槽位有贴图时要顺带设上的 combo
    pub require: Vec<(String, i32)>,
    /// 注解里的 `"hidden":true` —— 引擎自己提供的槽位（**没有默认资源时 = 本趟的输入**）
    pub hidden: bool,
}

/// 从 `g_Texture{N}` / `g_Texture{N}_b` 里取出槽号 N（遍历式加载与 combo 判定共用）。
pub fn sampler_slot(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("g_Texture")?;
    rest.trim_end_matches("_b").parse::<u32>().ok()
}

/// K1：对象级 `colorBlendMode` 1..30 要落到哪个 sampler 槽 = **shader 自己声明的那张"画面"**。
/// 判据（**数据驱动**，全部来自官方 shader 的注解，不看 id/名字）：
/// ① shader 文本里真的出现 `BLENDMODE`（否则我们塞的 `#define` 是死代码，绝不动 GL 混合）；
/// ② 声明里有一个 `sampler` 槽被标成**引擎提供的"画面"**。官方两种写法都实测存在：
/// · `// {"hidden":true,"default":"_rt_FullFrameBuffer"}`
/// `genericimage2.frag:57`、`genericimage4.frag:79`（对象趟采样"到目前为止的场景"）；
/// · `// {"material":"framebuffer","hidden":true}`（其它官方 shader 的同类槽）。
/// 返回该槽号（如 `g_Texture4` ⇒ 4）；找不到就返回 `None`（调用方**必须**保持旧行为）。
pub fn blend_screen_slot(frag_src: &str) -> Option<u32> {
    if !frag_src.contains("BLENDMODE") {
        return None;
    }
    let meta = parse_shader_meta(frag_src);
    meta.uniforms.iter().find_map(|u| {
        if !u.ty.starts_with("sampler") {
            return None;
        }
        let p = u.prop.to_ascii_lowercase();
        let is_screen = p.contains("framebuffer")
            || p == "prev"
            || p == "ui_editor_properties_prev"
            || u.default == "_rt_FullFrameBuffer";
        if is_screen {
            sampler_slot(&u.name)
        } else {
            None
        }
    })
}

/// -1：把「材质/特效 JSON 里的 combo 键」对齐到 **shader 自己的拼写**。
/// 为什么必须做：官方同一批材质里 `"VERSION"` 与 `"version"` 两种拼写都有
/// （`materials/Ceremonial.json` 大写、`materials/SteamLogoShattered.json` 小写），
/// 而 `genericimage2.frag:7/68` 里写的是大写 `#ifndef VERSION`。GLSL 的宏名**大小写敏感**
/// ⇒ 直接把 `version` 塞进去会造出**另一个宏**，`#ifndef VERSION` 仍然成立、分支照旧选错。
/// 判据（数据驱动，不猜）：扫 shader 文本里 `#if` / `#ifdef` / `#ifndef` / `#elif` 的**标识符**，
/// 找出与键**大小写不敏感**相等的那一个，返回 **shader 的拼写**；找不到就返回 `None`
/// （调用方保留原键 —— 也可能这个 combo 在别的阶段才被引用）。
pub fn canonical_combo_name(src: &str, key: &str) -> Option<String> {
    if key.is_empty() {
        return None;
    }
    for line in src.lines() {
        let t = line.trim_start();
        let Some(rest) = t.strip_prefix("#if") else { continue };
        for tok in rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            if tok.len() >= 2 && tok.eq_ignore_ascii_case(key) {
                return Some(tok.to_string());
            }
        }
    }
    None
}

/// -1：取一条 combo JSON 值的**整数语义**（`2` ⇒ 2、`true` ⇒ 1、`"2"` ⇒ 2）。
/// 认不出来 ⇒ `None`（调用方**跳过**这一条，绝不把 0 当默认硬塞）。
pub fn combo_json_i32(v: &serde_json::Value) -> Option<i32> {
    if let Some(i) = v.as_i64() {
        return Some(i as i32);
    }
    if let Some(b) = v.as_bool() {
        return Some(i32::from(b));
    }
    if let Some(s) = v.as_str() {
        return s.trim().parse::<i32>().ok();
    }
    v.as_f64().map(|f| f as i32)
}

/// 该 shader **声明的全部 sampler 槽**（升序、去重）。
/// 这是「遍历式加载」的基准集合：**以 shader 自己声明的槽为准**逐个解析数据源，
/// 而不是按数据里出现的名字去猜（也不看 item 名字 / `mode` 这类特判）。
pub fn declared_sampler_slots(meta: &ShaderMeta) -> Vec<u32> {
    let mut v: Vec<u32> = meta
        .uniforms
        .iter()
        .filter(|u| u.ty.starts_with("sampler"))
        .filter_map(|u| sampler_slot(&u.name))
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}


/// 该 shader **文本**里声明的全部 uniform 名里有没有 `name`（含**没有注解**的那些）。
/// ⚠ [`parse_shader_meta`] 只收录**同一行带 `// {...}` 注解**的 uniform（= 编辑器里可见、
/// 作者可调的那些）。引擎自己喂的（`g_PointerPosition` / `g_PointerPositionLast` /
/// `g_EffectTextureProjectionMatrixInverse` / `g_Time` …）**没有注解** ⇒ 想知道"这一趟到底
/// 用不用指针"必须看**原文**。实测（崩坏3 `2564205376` 的 `effects/xray`）：它的
/// `xray.vert:6` 声明了 `uniform vec2 g_PointerPosition;`，而 meta 里查不到 ⇒ 用 meta 判会
/// **永远判成"不用指针"** ⇒ 兜底位不会下发 ⇒ 透明层继续不显示。
/// 判据只看 `uniform` 声明本身（**数据驱动**：不看对象 id、不看 item/effect 名字）。
pub fn declares_uniform(src: &str, name: &str) -> bool {
    src.lines().any(|line| {
        // 去掉行尾注释再判（`uniform ... ; // {"material":…}` 这种）
        let code = match line.find("//") {
            Some(p) => &line[..p],
            None => line,
        };
        let t = code.trim_start();
        if !t.starts_with("uniform") {
            return false;
        }
        uniform_name(t).map(|n| n == name).unwrap_or(false)
    })
}

/// 采样器槽位绑了贴图 ⇒ 把该槽注解里声明的 combo 打开。
/// WE 的语义（以 `effects/tint` 为例）：`uniform sampler2D g_Texture1; // {"combo":"MASK"}`
/// 只有当**材质/场景真的给了这张遮罩**时才是 `MASK=1`；否则 `MASK=0`、shader 里
/// `#if MASK` 那段不编译，`mask` 就剩 `g_BlendAlpha`(=1) ⇒ **整层被染黑**（实测：
/// `3299815325` 的人物被 tint 特效"染成黑的"、中心区亮度 91→11）。
/// `"require":{...}` 同理（如 `blur_precise_gaussian` 的 `g_Texture2`）。
pub fn combos_for_units(meta: &ShaderMeta, bound_units: &[u32]) -> Vec<(String, i32)> {
    let mut out: Vec<(String, i32)> = Vec::new();
    for u in &meta.uniforms {
        let Some(unit) = sampler_slot(&u.name) else { continue };
        if !bound_units.contains(&unit) {
            continue;
        }
        if !u.combo.is_empty() {
            out.push((u.combo.clone(), 1));
        }
        for (k, v) in &u.require {
            out.push((k.clone(), *v));
        }
    }
    out
}

pub fn parse_shader_meta(src: &str) -> ShaderMeta {
    let mut meta = ShaderMeta::default();
    for line in src.lines() {
        let Some(pos) = line.find("//") else { continue };
        let (code, comment) = line.split_at(pos);
        let c = comment[2..].trim();
        if let Some(rest) = c.strip_prefix("[COMBO]") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(rest.trim()) {
                let (Some(prop), Some(combo)) = (
                    v.get("material").and_then(|m| m.as_str()),
                    v.get("combo").and_then(|m| m.as_str()),
                ) else {
                    continue;
                };
                let default = v.get("default").and_then(|d| d.as_i64()).unwrap_or(0) as i32;
                meta.combos.push(ComboDecl {
                    prop: prop.to_string(),
                    combo: combo.to_string(),
                    default,
                });
            }
            continue;
        }
        if !c.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(c) else { continue };
        // ⚠ **别要求一定带 `material`**：真实注解里有的只有 `label`/`combo`
        // （`effects/tint` 的遮罩槽就是 `{"label":"…","combo":"MASK"}`）——
        // 以前这里 `continue` 掉，导致那条槽位的 combo 永远读不到，遮罩形同不存在。
        let prop = v
            .get("material")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_string();
        let Some(name) = uniform_name(code) else { continue };
        let default = match v.get("default") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        meta.uniforms.push(UniformDecl {
            name,
            prop,
            default,
            ty: decl_type(code),
            range_lo: v
                .get("range")
                .and_then(|r| r.as_array())
                .and_then(|a| a.first())
                .and_then(|x| x.as_f64())
                .map(|x| x as f32),
            combo: v
                .get("combo")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string(),
            mode: v
                .get("mode")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            require: v
                .get("require")
                .and_then(|m| m.as_object())
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, val)| {
                            let iv = val.as_i64()? as i32;
                            Some((k.clone(), iv))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            hidden: v.get("hidden").and_then(|h| h.as_bool()).unwrap_or(false),
        });
    }
    meta
}

/// 场景给的「常量键」→ shader 里的 uniform 声明。
/// WE 的 `constantshadervalues` 键**不是** GLSL uniform 名，而是「编辑器参数键」。
/// 参照实现 OWE 用一张**别名表**翻译它（`Scene/Pkg/Parse/Shader/ShaderAnnot.cpp:154`：
/// 每个带 `"material"` 注解的 uniform 都往 `info.alias[material] = uniform名` 里塞一条），
/// 翻译函数是 `ResolveShaderMaterialKey`（`Scene/Pkg/Parse/Scene/SceneMaterialBuilder.cpp:556-577`）：
///
/// 1. **键 == 注解里的 `material` 标签**（`Brightness` / `Channel influence` / `ui_editor_properties_*` …）
/// ⇒ 命中；
///
/// 2. 否则：uniform 名**去掉前两个字符**（`u_` / `g_`）后 == 键 ⇒ 命中
/// （OWE 就是 `value->as_str.get(2, len)`；例：`u_brightness` ⇒ 键 `brightness`、
/// `g_Length` ⇒ 键 `Length`）。**只考虑带注解的 uniform**（OWE 的别名表里只有它们）；
/// 多个候选互相冲突 ⇒ **放弃**（OWE：`if (resolved.is_some && *resolved != value) return {}`）。
/// ⚠ 我们以前**只认第 1 条** ⇒ 用第 2 种写法写的趟常量被**静默丢掉**，那一趟"在跑但量不对"
/// （uniform 停在注解默认值或 0）—— 是最难查的一类"特效没生效"。
/// 第 1 条优先：两条都能命中时以标签为准（与 OWE 的别名表先查标签一致）。
pub fn find_uniform_for_key<'a>(meta: &'a ShaderMeta, key: &str) -> Option<&'a UniformDecl> {
    if let Some(u) = meta.uniforms.iter().find(|u| u.prop == key) {
        return Some(u);
    }
    let mut hit: Option<&UniformDecl> = None;
    for u in &meta.uniforms {
        // 只考虑带注解的 uniform（= OWE 别名表里的那些）
        if u.prop.is_empty() || u.name.len() <= 2 {
            continue;
        }
        let Some(stripped) = u.name.get(2..) else { continue };
        if stripped != key {
            continue;
        }
        match hit {
            // 歧义：两个不同的 uniform 都能命中同一个键 ⇒ 与 OWE 一致地放弃
            Some(prev) if prev.name != u.name => return None,
            Some(_) => {}
            None => hit = Some(u),
        }
    }
    hit
}

/// 从 `uniform float g_Threshold;` 里取出 `float`
fn decl_type(code: &str) -> String {
    let decl = code.split(';').next().unwrap_or("").trim();
    let toks: Vec<&str> = decl.split_whitespace().collect();
    if toks.len() >= 2 {
        toks[toks.len() - 2].to_string()
    } else {
        String::new()
    }
}

/// 从 `uniform float g_Threshold;` 里取出 `g_Threshold`
fn uniform_name(code: &str) -> Option<String> {
    let decl = code.split(';').next()?.trim();
    let last = decl.split_whitespace().last()?;
    let name = last.trim_matches(|c: char| !(c.is_alphanumeric() || c == '_'));
    if name.is_empty() || matches!(name, "uniform" | "lowp" | "mediump" | "highp") {
        None
    } else {
        Some(name.to_string())
    }
}

/// 正文（含内联进来的头文件）里自己 `#define` 过的名字 —— 这些**绝不能**补 0，
/// 否则后面头文件里的 `#define FORMAT_DXT1 1` 会撞上我们的 `#define FORMAT_DXT1 0` ⇒
/// 预处理报 "Redefinition of macro"（粒子 shader 就是这么炸的）。
fn defined_in_bodies(bodies: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for b in bodies {
        for line in b.lines() {
            let t = line.trim_start();
            if let Some(rest) = t.strip_prefix("#define") {
                if let Some(name) = rest.trim().split(|c: char| !(c.is_alphanumeric() || c == '_')).next()
                {
                    if !name.is_empty() && !out.iter().any(|x| x == name) {
                        out.push(name.to_string());
                    }
                }
            }
        }
    }
    out
}

/// ── HLSL 宽容语义 → GLSL 严格语义 的适配层 ──────────────────────────────────
/// WE 的 shader 是 **HLSL** 写的（引擎自己翻成 GLSL）。HLSL 的几条语义在 GLSL 里
/// 要么是**链接错误**、要么是**编译错误**，而源码本身在 HLSL 下完全合法
/// （作者的壁纸在 Windows 上跑得起来）。我们这里照 HLSL 的语义补回来。
/// 实测的三条：
/// ① **两阶段 `varying` 类型不一致**：HLSL 按语义/寄存器（TEXCOORD0）链接，
/// 顶点输出 `vec4 v_TexCoord` 与片元输入 `vec2 v_TexCoord` 可以并存；
/// GLSL 要求两阶段声明**逐字相同**，否则
/// `vertex shader output 'v_TexCoord' declared as type 'vec4', but fragment shader input
/// declared as type 'vec2'` ⇒ 整趟链接失败被跳过。
/// 例：`3704273480` 的 `workshop/2795521260/effects/color_grading`
/// （vert 写 `varying vec4 v_TexCoord;`、frag 写 `varying vec2 v_TexCoord;`，
/// 片元只用 `.xy`）、`2961776805` 的 `workshop/2844906964/effects/test_shader`。
/// **规则：取宽者统一**（片元声明窄 ⇒ 把它加宽；顶点窄 ⇒ 加宽顶点并把它写 varing 的
/// 赋值补成宽构造，参照实现 `linux-wallpaperengine ShaderUnit.cpp:382-412`
/// 的 `applyLinkedVaryingCompatibility` 就是这么做的）。
/// ② **`vec2 x = <vec4 表达式>;`（隐式截断）**：HLSL 允许窄 = 宽（取低分量），
/// GLSL 报 `value of type vec4 cannot be assigned to variable of type vec2`
/// （例：`3504002340` 的 `effects/cloudmotion.vert` 第 114 行 `v_NoiseCoord = v_TexCoord;`；
/// 官方 `_assets/generic.vert` 的 `v_TexCoord = a_TexCoordVec4;` 同形）。
/// ③ **`int i = <float 表达式>;`（隐式取整）** 与 **整数字面量进“向量参数”的调用**
/// （`max(0, albedo.rgb)` —— HLSL 把 `0` 提升成 `0.0` 再广播）。
/// 例：`2961776805` 的 `test_shader.frag`（`int i = u_MinFreqRange;` +
/// `vec4(max(0, albedo.rgb), albedo.a)`）。
/// ⚠ 只在**没有 pair 冲突**时才动文本：规则都是"类型已知且确实更窄"才改写，
/// 且改写只碰整行/整实参，不做模糊替换。
pub fn hlsl_stage_compat(vert: &str, frag: &str) -> (String, String) {
    if !glsl_compat_on() {
        return (vert.to_string(), frag.to_string());
    }
    let (v, f) = unify_varyings(vert, frag);
    let v = fix_narrow_assignments(&v);
    let f = fix_narrow_assignments(&f);
    let v = fix_int_inits(&v);
    let f = fix_int_inits(&f);
    let v = promote_int_literals(&v);
    let f = promote_int_literals(&f);
    // ③c：标量实参广播成向量（`pow(vec4, float)` 这类）。**自带独立默认关开关**：
    // 别的规则要么在官方 GLSL120→HLSL→SM3.0 管线里也成立、要么不成立，这条只在
    // 我们"直编 GLSL"的替代管线上才需要 ⇒ 默认关，`WE_GLSL_SCALAR_PROMOTE=1` 才动。
    let (v, f) = if scalar_promote_on() {
        (promote_scalar_args(&v), promote_scalar_args(&f))
    } else {
        (v, f)
    };
    // ④ 最后做：片元给 varying 赋值（HLSL 片元输入是可写寄存器）⇒ 物化成局部副本。
    // 必须在 varying 类型统一**之后**：注入的局部副本类型按最终声明类型取。
    let frag = materialize_fragment_written_varyings(&f);
    (v, frag)
}

/// 规则④：**片元给自己的 `varying` 赋值** ⇒ 物化成可写局部变量。
/// HLSL 里片元的输入语义（`TEXCOORD0`）在编译后是**普通寄存器**，`v_TexCoord.y = …` 合法；
/// GLSL 里片元的 `varying` 是**只读**（`error: assignment to read-only variable 'v_TexCoord'`）。
/// 实测素材：`3704273480` 包内 `shaders/workshop/3082978660/effects/Simple_Audio_Bars.frag`
/// 第 26 行声明 `varying vec2 v_TexCoord;`，然后在 `main` 里按 `SHAPE` 把它反复改写
/// （第 218~253 行一大段）。这一层在 Windows 上是正常的音频条。
/// 做法（只改片元，顶点不动）：
/// · 把片元里该 varying 的**所有使用**改名成 `<名>__local`，并在**同一个函数体**开头
/// 注入 `<类型> <名>__local = <名>;`；
/// · 原来的 `varying` 声明**保留**（链接仍要求两阶段同名同类型声明）。
/// · 只有当该 varying 的全部使用都落在**同一个函数体**里时才动（否则不做，宁可报错也不乱改）。
fn materialize_fragment_written_varyings(frag: &str) -> String {
    let decls = guarded_scan_decls(frag, "varying");
    if decls.is_empty() {
        return frag.to_string();
    }
    let lines: Vec<&str> = frag.lines().collect();
    // 顶层函数体的 (开行, 闭行)：行首非空、含 `(` 且以 `{` 结尾（跨行签名也按"遇到 { 时记开"）
    let mut bodies: Vec<(usize, usize)> = Vec::new();
    let mut depth = 0i32;
    let mut open: Option<usize> = None;
    let mut fn_like = true;
    for (i, l) in lines.iter().enumerate() {
        if depth == 0 {
            // 只看顶层：出现 `{` 前的行必须像函数头（不含 `=`，排除初始化列表/结构）
            let code = l.split("//").next().unwrap_or("");
            if code.contains('{') && !code.contains('=') && !code.starts_with('}') {
                fn_like = !code.trim_start().starts_with('#');
            } else {
                fn_like = false;
            }
        }
        let opens = l.matches('{').count() as i32;
        let closes = l.matches('}').count() as i32;
        if depth == 0 && opens > 0 && fn_like {
            open = Some(i);
        }
        depth += opens - closes;
        if depth <= 0 {
            if let Some(s) = open {
                bodies.push((s, i));
            }
            open = None;
            depth = 0;
        }
    }
    let enclosing = |line: usize| -> Option<(usize, usize)> {
        bodies
            .iter()
            .find(|(s, e)| line >= *s && line <= *e)
            .copied()
    };
    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    let mut injections: Vec<(usize, String)> = Vec::new();
    for (decl_line, ty, name) in &decls {
        // 该名字在本文件里的**全部**出现行（跳过声明行）
        let hits: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(i, l)| *i != *decl_line && has_ident(l, name))
            .map(|(i, _)| i)
            .collect();
        if hits.is_empty() {
            continue;
        }
        // 赋值（含 swizzle / 复合赋值）出现的行
        let writes: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(i, l)| *i != *decl_line && is_write_to(l, name))
            .map(|(i, _)| i)
            .collect();
        if writes.is_empty() {
            continue;
        }
        let Some(body) = enclosing(writes[0]) else {
            continue;
        };
        // 所有出现（读+写）都必须在同一个函数体里，否则注入的局部变量覆盖不到
        if !hits.iter().all(|h| enclosing(*h) == Some(body)) {
            continue;
        }
        let local = format!("{name}__local");
        for l in out.iter_mut() {
            *l = replace_ident(l, name, &local);
        }
        // 声明行本身保持原样（链接仍要 `varying`）
        out[*decl_line] = lines[*decl_line].to_string();
        injections.push((body.0, format!("\t{ty} {name}__local = {name};")));
    }
    if injections.is_empty() {
        return frag.to_string();
    }
    injections.sort_by_key(|(i, _)| *i);
    let mut res = String::with_capacity(frag.len() + 64);
    for (i, l) in out.iter().enumerate() {
        if !l.trim_start().starts_with("varying ") && !l.trim().is_empty() {
            if let Some((_, inj)) = injections.iter().find(|(li, _)| *li == i) {
                // 函数头那一行：在它后面补一行局部副本
                res.push_str(l);
                res.push('\n');
                res.push_str(inj);
                res.push('\n');
                continue;
            }
        }
        res.push_str(l);
        res.push('\n');
    }
    res
}

/// 该行是否出现完整标识符 `name`
fn has_ident(line: &str, name: &str) -> bool {
    ident_positions(line, name).next().is_some()
}

/// 逐个定位 `name` 作为**完整标识符**出现的位置
fn ident_positions<'a>(line: &'a str, name: &str) -> impl Iterator<Item = usize> + 'a {
    let b = line.as_bytes();
    let n = name.as_bytes();
    let mut i = 0usize;
    let mut out: Vec<usize> = Vec::new();
    while i + n.len() <= b.len() {
        if &b[i..i + n.len()] == n {
            let before_ok = i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
            let after = i + n.len();
            let after_ok = after >= b.len() || !(b[after].is_ascii_alphanumeric() || b[after] == b'_');
            if before_ok && after_ok {
                out.push(i);
            }
            i += n.len();
        } else {
            i += 1;
        }
    }
    out.into_iter()
}

/// 该行是否**给** `name` 赋值（`name = `、`name.xy = `、`name.x += `…，但不是 `==`）
fn is_write_to(line: &str, name: &str) -> bool {
    let code = line.split("//").next().unwrap_or("");
    for p in ident_positions(code, name) {
        let rest = &code[p + name.len()..];
        let rest = rest.trim_start();
        let rest = if let Some(r) = rest.strip_prefix('.') {
            // 吃 swizzle
            let sw: String = r
                .chars()
                .take_while(|c| matches!(c, 'x' | 'y' | 'z' | 'w' | 'r' | 'g' | 'b' | 'a'))
                .collect();
            if sw.is_empty() {
                continue;
            }
            r[sw.len()..].trim_start()
        } else {
            rest
        };
        for op in ["+=", "-=", "*=", "/=", "%="] {
            if rest.starts_with(op) {
                return true;
            }
        }
        if rest.starts_with('=') && !rest.starts_with("==") {
            return true;
        }
    }
    false
}

/// 把行里所有完整标识符 `name` 换成 `to`
fn replace_ident(line: &str, name: &str, to: &str) -> String {
    let mut out = String::with_capacity(line.len() + 16);
    let mut last = 0usize;
    for p in ident_positions(line, name) {
        out.push_str(&line[last..p]);
        out.push_str(to);
        last = p + name.len();
    }
    out.push_str(&line[last..]);
    out
}

/// 兼容层总开关 —— `WE_HLSL_COMPAT=0` 退回**改动前**的行为（逐字节、用于 A/B 与回滚）。
/// 默认开：不修的话这几条在 GL 上就是链接/编译失败（整趟被跳过）。
pub fn glsl_compat_on() -> bool {
    !matches!(
        std::env::var("WE_HLSL_COMPAT").as_deref(),
        Ok("0") | Ok("off") | Ok("false")
    )
}

/// 类型宽度排名（只处理标量/向量；矩阵等一律返回 `None` ⇒ 不参与合并）
fn width_of(ty: &str) -> Option<u8> {
    match ty {
        "float" => Some(1),
        "vec2" => Some(2),
        "vec3" => Some(3),
        "vec4" => Some(4),
        _ => None,
    }
}

/// 扫出**没被 `#if` 守卫包住**的 `varying` 声明（`(行号, 类型, 名字)`）。
/// 守卫里的声明两边可能取不同 combo ⇒ 不能合并（会编译进本不该存在的分支）。
fn guarded_scan_decls(src: &str, kw: &str) -> Vec<(usize, String, String)> {
    let mut out = Vec::new();
    let mut guards = 0i32;
    let mut depth = 0i32;
    for (i, l) in src.lines().enumerate() {
        let t = l.trim_start();
        if t.starts_with("#if") {
            guards += 1;
        } else if t.starts_with("#endif") {
            guards -= 1;
            if guards < 0 {
                guards = 0;
            }
        }
        if guards == 0 && depth == 0 {
            if let Some(rest) = t.strip_prefix(kw) {
                let code = rest.split("//").next().unwrap_or("").trim();
                if let Some(ty) = code.split_whitespace().next() {
                    if let Some(name) = code
                        .split_whitespace()
                        .nth(1)
                        .map(|s| s.trim_matches(|c: char| !(c.is_alphanumeric() || c == '_')))
                    {
                        if !name.is_empty() {
                            out.push((i, ty.trim().to_string(), name.to_string()));
                        }
                    }
                }
            }
        }
        depth += l.matches('{').count() as i32 - l.matches('}').count() as i32;
        if depth < 0 {
            depth = 0;
        }
    }
    out
}

/// 把 `varying vec4 X;` 这一行换成 `varying vec3 X;` 之类的**声明类型改写**（逐行，只改命中行）
fn rewrite_decl_line(src: &str, line_no: usize, ty: &str, name: &str) -> String {
    let mut out = String::with_capacity(src.len() + 8);
    for (i, l) in src.lines().enumerate() {
        if i == line_no {
            let comment = match l.find("//") {
                Some(p) => &l[p..],
                None => "",
            };
            out.push_str(&format!("varying {ty} {name}; "));
            if !comment.is_empty() {
                out.push_str(comment);
            }
        } else {
            out.push_str(l);
        }
        out.push('\n');
    }
    out
}

/// 规则①：两阶段 `varying` 类型取宽者统一 —— 但**先看顶点能不能收窄**。
/// 两个方向都实测过：
/// **顶点更宽、且顶点对这个 varying 的写全落在片元宽度内** ⇒ **收窄顶点**（片元一个字不动）。
/// 实测 `color_grading` / `test_shader`：vert 声明 `vec4 v_TexCoord` 但只写
/// `v_TexCoord.xy = a_TexCoord;`，片元声明 `vec2` 且把它当 vec2 用（`vec2 fragPos = v_TexCoord;`）
/// ⇒ 收窄顶点是**唯一**两边都不破的做法（加宽片元会把片元里的 vec2 算式全打烂）。
/// **片元更宽** ⇒ **加宽顶点**，并把它写这个 varying 的赋值补成宽构造
/// （与参照实现 `ShaderUnit.cpp:382-412` 的 `applyLinkedVaryingCompatibility` 同形）。
/// 顶点写了超出片元宽度的分量（例如 `v_TexCoord.zw`）⇒ **两边都不动**（宁可链接失败出声，
/// 也不把片元的算式改坏）。
fn unify_varyings(vert: &str, frag: &str) -> (String, String) {
    let vd = guarded_scan_decls(vert, "varying");
    let fd = guarded_scan_decls(frag, "varying");
    let mut v = vert.to_string();
    let f = frag.to_string();
    for (vi, vt, name) in &vd {
        let Some((_, ft, _)) = fd.iter().find(|(_, _, n)| n == name) else {
            continue;
        };
        let (Some(wv), Some(wf)) = (width_of(vt), width_of(ft)) else {
            continue;
        };
        if wv == wf || wv < 2 || wf < 2 {
            continue; // 相同 / 只涉及标量 ⇒ 不动（宁可有声地失败，也别静默改语义）
        }
        if wv > wf {
            if vertex_writes_fit(&v, name, wf) {
                v = rewrite_decl_line(&v, *vi, ft, name);
            }
        } else {
            v = rewrite_decl_line(&v, *vi, ft, name);
            v = widen_assignments(&v, name, ft);
        }
    }
    (v, f)
}

/// 顶点里对 `name` 的所有写是否都在 `w` 个分量以内（`w=2` ⇒ 不许出现 `.z/.w/.zw/...`），
/// 且没有整行裸赋值（裸赋值的右值可能是更宽的表达式，收窄后必然非法 ⇒ 交给"加宽片元"那条路）。
fn vertex_writes_fit(vert: &str, name: &str, w: u8) -> bool {
    let allowed: &[char] = if w >= 3 { &['x', 'y', 'z', 'r', 'g', 'b'] } else { &['x', 'y', 'r', 'g'] };
    for l in vert.lines() {
        let code = l.split("//").next().unwrap_or("");
        if !has_ident(code, name) {
            continue;
        }
        // 裸赋值（`name = …`，不带 swizzle）⇒ 不算"写得下"
        let t = code.trim_start();
        if t.starts_with(name) {
            let rest = t[name.len()..].trim_start();
            if rest.starts_with('=') && !rest.starts_with("==") {
                return false;
            }
        }
        // swizzle 里出现超宽分量 ⇒ 不算
        for p in ident_positions(code, name) {
            let rest = &code[p + name.len()..];
            if let Some(r) = rest.strip_prefix('.') {
                let sw: String = r.chars().take_while(|c| c.is_alphabetic()).collect();
                if sw.chars().any(|c| !allowed.contains(&c)) {
                    return false;
                }
            }
        }
    }
    true
}

/// 把 `X = expr;`（整行、顶部无 swizzle）补成宽类型构造：
/// `vec4(expr, 0.0, 1.0)`（源 vec2）/ `vec4(expr, 1.0)`（源 vec3）/ `vec3(expr, 0.0)`。
/// 与参照实现同形（`ShaderUnit.cpp:404-410`）。
fn widen_assignments(src: &str, name: &str, target: &str) -> String {
    let mut out = String::with_capacity(src.len() + 32);
    for l in src.lines() {
        let indent: String = l.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        let t = &l[indent.len()..];
        let comment = match t.find("//") {
            Some(p) => &t[p..],
            None => "",
        };
        let code = t.split("//").next().unwrap_or("").trim_end();
        let hit = code
            .strip_prefix(name)
            .map(|r| r.trim_start().starts_with('=') && !r.trim_start().starts_with("=="))
            .unwrap_or(false)
            && code.ends_with(';')
            && code
                .strip_prefix(name)
                .map(|r| !r.trim_start().starts_with("=."))
                .unwrap_or(true);
        if hit {
            let expr = code[name.len()..].trim_start();
            let expr = expr[1..].trim_start().trim_end_matches(';').trim_end();
            let wrapped = match target {
                "vec4" => format!("vec4({expr}, 0.0, 1.0)"),
                "vec3" => format!("vec3({expr}, 0.0)"),
                _ => format!("{target}({expr})"),
            };
            out.push_str(&format!("{indent}{name} = {wrapped}; {comment}"));
        } else {
            out.push_str(l);
        }
        out.push('\n');
    }
    out
}

/// 规则②：`窄 = 宽`（两者都是本文件里声明过的全局标识符）⇒ 给右边补 swizzle。
fn fix_narrow_assignments(src: &str) -> String {
    let mut types: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for kw in ["varying ", "attribute ", "uniform "] {
        for (_, ty, name) in guarded_scan_decls(src, kw) {
            types.insert(name, ty);
        }
    }
    if types.is_empty() {
        return src.to_string();
    }
    let mut out = String::with_capacity(src.len());
    let mut changed = 0usize;
    for l in src.lines() {
        let indent: String = l.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        let t = &l[indent.len()..];
        let comment = match t.find("//") {
            Some(p) => t[p..].to_string(),
            None => String::new(),
        };
        let code = t.split("//").next().unwrap_or("").trim_end();
        let mut replaced = None;
        if let Some(eq) = code.find('=') {
            if !code[eq..].starts_with("==") && code.ends_with(';') {
                let lhs = code[..eq].trim();
                let rhs = code[eq + 1..].trim().trim_end_matches(';').trim();
                if let (Some(lt), Some(rt)) = (types.get(lhs), types.get(rhs)) {
                    if let (Some(lw), Some(rw)) = (width_of(lt), width_of(rt)) {
                        if lw >= 2 && rw > lw {
                            let sw = &"xyz"[..lw as usize];
                            replaced = Some(format!("{lhs} = {rhs}.{sw};"));
                        }
                    }
                }
            }
        }
        match replaced {
            Some(r) => {
                changed += 1;
                out.push_str(&format!("{indent}{r} {comment}"));
            }
            None => out.push_str(l),
        }
        out.push('\n');
    }
    if changed > 0 {
        // 只改类型已知的整行赋值，改了几处自己说出来（免得"以为改了其实没改"）
        out.push_str(&format!("// [hlsl-compat] 隐式截断补 swizzle：{changed} 处\n"));
    }
    out
}

/// 规则③a：`int x = <非 int 字面量/非 int(...)>;` ⇒ 补 `int(...)`（HLSL 允许隐式取整）
fn fix_int_inits(src: &str) -> String {
    let mut out = String::with_capacity(src.len() + 16);
    for l in src.lines() {
        let code = l.split("//").next().unwrap_or("");
        let pat = "int ";
        let mut done = false;
        if let Some(p) = code.find(pat) {
            let (head, tail) = code.split_at(p + pat.len());
            let rest = tail.trim_start();
            let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            let after = rest[name.len()..].trim_start();
            if !name.is_empty() {
                if let Some(r) = after.strip_prefix('=') {
                    if !r.trim_start().starts_with("==") {
                        let r = r.trim_start();
                        let expr = r.split(';').next().unwrap_or("").trim();
                        let intish = expr.is_empty()
                            || expr.starts_with("int(")
                            || expr.chars().all(|c| c.is_ascii_digit());
                        if !intish && !expr.is_empty() {
                            done = true;
                            let nl = l.replacen(
                                &format!("int {name} = {expr}"),
                                &format!("int {name} = int({expr})"),
                                1,
                            );
                            out.push_str(&nl);
                        }
                    }
                }
            }
            let _ = head;
        }
        if !done {
            out.push_str(l);
        }
        out.push('\n');
    }
    out
}

/// 规则③b：浮点内置函数的**实参里**出现裸整数字面量，而同一调用里还有"像浮点/向量"的实参
/// （浮点字面量 / swizzle / `vecN(...)` 构造 / 已知的浮点或向量变量）⇒ 把这个整数字面量补 `.0`。
/// （`max(0, albedo.rgb)` 在 HLSL 里 = `max(0.0, albedo.rgb)`；GLSL 里没有这个提升。）
/// 同一调用里若有**已知的 int 变量**（`max(0, i)` 这类真·整数上下文）则整条不动。
fn promote_int_literals(src: &str) -> String {
    const BUILTINS: &[&str] = &[
        "max", "min", "clamp", "mix", "pow", "step", "smoothstep", "abs", "sign", "floor",
        "ceil", "fract", "mod", "sqrt", "exp", "log", "dot", "length", "distance", "normalize",
        "reflect", "refract", "atan", "asin", "acos", "tan", "sin", "cos", "modf", "degrees",
        "radians", "faceforward", "ldexp", "fwidth",
    ];
    // 本文件里声明过的变量类型（uniform/varying/attribute/局部都算）—— 用来判断实参是不是向量/浮点
    let mut types: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let re_types = [
        "float", "vec2", "vec3", "vec4", "mat2", "mat3", "mat4", "int", "ivec2", "ivec3",
        "ivec4", "sampler2D",
    ];
    for l in src.lines() {
        let code = l.split("//").next().unwrap_or("");
        for ty in re_types {
            let mut rest = code;
            while let Some(p) = rest.find(ty) {
                let before_ok = p == 0
                    || !(rest.as_bytes()[p - 1].is_ascii_alphanumeric()
                        || rest.as_bytes()[p - 1] == b'_');
                let after = &rest[p + ty.len()..];
                let after_ok = after.starts_with(|c: char| c.is_whitespace());
                if before_ok && after_ok {
                    let name: String = after
                        .trim_start()
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if !name.is_empty() {
                        types.entry(name).or_insert_with(|| ty.to_string());
                    }
                }
                rest = &rest[p + ty.len()..];
            }
        }
    }
    let is_float_like = |ty: &str| {
        matches!(ty, "float" | "vec2" | "vec3" | "vec4" | "mat2" | "mat3" | "mat4")
    };
    let is_int_like = |ty: &str| matches!(ty, "int" | "ivec2" | "ivec3" | "ivec4");
    let mut out = String::with_capacity(src.len() + 16);
    let mut total = 0usize;
    for l in src.lines() {
        let (code, comment) = match l.find("//") {
            Some(p) => (&l[..p], &l[p..]),
            None => (l, ""),
        };
        let mut s: Vec<char> = code.chars().collect();
        let mut i = 0usize;
        let mut changed = 0usize;
        while i < s.len() {
            if s[i].is_ascii_alphabetic() || s[i] == '_' {
                let st = i;
                while i < s.len() && (s[i].is_ascii_alphanumeric() || s[i] == '_') {
                    i += 1;
                }
                let name: String = s[st..i].iter().collect();
                if BUILTINS.contains(&name.as_str())
                    && i < s.len()
                    && s[i] == '('
                    && (st == 0 || !(s[st - 1].is_ascii_alphanumeric() || s[st - 1] == '_'))
                {
                    // 找到匹配的右括号
                    let mut depth = 0i32;
                    let mut j = i;
                    while j < s.len() {
                        if s[j] == '(' {
                            depth += 1;
                        } else if s[j] == ')' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        j += 1;
                    }
                    if j < s.len() {
                        let inner: String = s[i + 1..j].iter().collect();
                        // 顶层逗号切实参
                        let mut args: Vec<String> = Vec::new();
                        let mut depth2 = 0i32;
                        let mut cur = String::new();
                        for c in inner.chars() {
                            match c {
                                '(' => {
                                    depth2 += 1;
                                    cur.push(c);
                                }
                                ')' => {
                                    depth2 -= 1;
                                    cur.push(c);
                                }
                                ',' if depth2 == 0 => {
                                    args.push(cur.clone());
                                    cur.clear();
                                }
                                _ => cur.push(c),
                            }
                        }
                        args.push(cur);
                        let bare_type = |a: &str| -> Option<String> {
                            let t = a.trim();
                            if !t.is_empty()
                                && t.chars().all(|c| c.is_alphanumeric() || c == '_')
                                && !t.chars().next().unwrap().is_ascii_digit()
                            {
                                types.get(t).cloned()
                            } else {
                                None
                            }
                        };
                        // `max(0, i)`（i 是 int）⇒ 真·整数上下文，整条不动
                        let has_int_arg = args
                            .iter()
                            .any(|a| bare_type(a).map(|t| is_int_like(&t)).unwrap_or(false));
                        let floatish = |a: &str| {
                            let a = a.trim();
                            let a = a.trim_end_matches(',');
                            a.contains('.')
                                || a.contains("vec")
                                || a.contains("mat")
                                || a.contains("g_")
                                || a.contains("u_")
                                || bare_type(a)
                                    .map(|t| is_float_like(&t))
                                    .unwrap_or(false)
                        };
                        let any_floatish = !has_int_arg && args.iter().any(|a| floatish(a));
                        // `max(0, vecN)` / `min(0, vecN)`：GLSL 的 `max` 只有
                        // `max(GenType, GenType)` 与 `max(GenType, float)` 两种形态，
                        // **没有** `max(float, GenType)`；HLSL 的两个操作数是对称提升的。
                        // `max`/`min` 满足交换律 ⇒ **第一个实参是数字字面量、第二个是向量**时对调
                        // （`max(0, albedo.rgb)` ⇒ `max(albedo.rgb, 0.0)`）。
                        let is_number_literal = |a: &str| {
                            let t = a.trim();
                            !t.is_empty()
                                && t.chars().all(|c| c.is_ascii_digit() || c == '.')
                                && t.chars().any(|c| c.is_ascii_digit())
                        };
                        let is_vectorish = |a: &str| {
                            let t = a.trim();
                            if t.contains("vec") {
                                return true;
                            }
                            if let Some((base, sw)) = t.split_once('.') {
                                return !base.is_empty()
                                    && sw.chars().all(|c| "xyzwrgba".contains(c))
                                    && !sw.is_empty();
                            }
                            bare_type(t)
                                .map(|ty| ty.starts_with("vec"))
                                .unwrap_or(false)
                        };
                        let mut args = args;
                        if (name == "max" || name == "min")
                            && args.len() == 2
                            && is_number_literal(&args[0])
                            && is_vectorish(&args[1])
                        {
                            args.swap(0, 1);
                        }
                        let promoted: Vec<String> = args
                            .iter()
                            .map(|a| {
                                let t = a.trim();
                                if any_floatish
                                    && !t.is_empty()
                                    && t.chars().all(|c| c.is_ascii_digit())
                                {
                                    changed += 1;
                                    let lead = &a[..a.len() - a.trim_start().len()];
                                    let trail = &a[a.trim_end().len()..];
                                    format!("{lead}{t}.0{trail}")
                                } else {
                                    a.clone()
                                }
                            })
                            .collect();
                        if changed > 0 {
                            let new_inner = promoted.join(",");
                            let mut ns: Vec<char> = s[..i + 1].to_vec();
                            ns.extend(new_inner.chars());
                            ns.extend(s[j..].iter());
                            s = ns;
                            i = i + 1 + new_inner.len() + 1;
                            total += changed;
                            changed = 0;
                            continue;
                        }
                        i = j + 1;
                        continue;
                    }
                }
                continue;
            }
            i += 1;
        }
        let l2: String = s.into_iter().collect();
        out.push_str(&l2);
        out.push_str(comment);
        out.push('\n');
    }
    if total > 0 {
        out.push_str(&format!("// [hlsl-compat] 整数字面量补 .0：{total} 处\n"));
    }
    out
}

/// 扫出本文件里**声明过**的变量名 → 类型（`uniform`/`varying`/`attribute`/局部都算）。
/// 口径与 [`promote_int_literals`] 里的同一段一致：只认「类型关键字 + 空白 + 名字」，
/// 同名以**第一次出现**为准（`HashMap::entry.or_insert`）。
fn declared_types(src: &str) -> std::collections::HashMap<String, String> {
    const TYPES: &[&str] = &[
        "float", "vec2", "vec3", "vec4", "mat2", "mat3", "mat4", "int", "ivec2", "ivec3", "ivec4",
        "sampler2D",
    ];
    let mut types: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for l in src.lines() {
        let code = l.split("//").next().unwrap_or("");
        for ty in TYPES {
            let mut rest = code;
            while let Some(p) = rest.find(ty) {
                let before_ok = p == 0
                    || !(rest.as_bytes()[p - 1].is_ascii_alphanumeric()
                        || rest.as_bytes()[p - 1] == b'_');
                let after = &rest[p + ty.len()..];
                let after_ok = after.starts_with(|c: char| c.is_whitespace());
                if before_ok && after_ok {
                    let name: String = after
                        .trim_start()
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if !name.is_empty() {
                        types.entry(name).or_insert_with(|| ty.to_string());
                    }
                }
                rest = &rest[p + ty.len()..];
            }
        }
    }
    types
}

/// 表达式 → 宽度（1=标量，2/3/4=向量）；认不出 ⇒ `None`（一律不动）。
/// 认得出的是：显式 `vecN(...)` 构造、已知类型的变量、swizzle（宽度取基类型）、
/// 浮点字面量、以及**一元浮点内置调用**（`abs(sines)` ⇒ 看 `sines`）。
fn expr_width(types: &std::collections::HashMap<String, String>, a: &str) -> Option<u8> {
    let t = a.trim();
    if t.is_empty() {
        return None;
    }
    // `vec3(x)` / `vec3 (x)` 这类显式构造
    for w in [4u8, 3, 2] {
        if t.contains(&format!("vec{w}(")) || t.contains(&format!("vec{w} (")) {
            return Some(w);
        }
    }
    // 一元浮点内置：`abs(sines)` / `normalize(n)` / `(-x)` ⇒ 取里面那个实参的宽度
    const UNARY: &[&str] = &[
        "abs", "sign", "sqrt", "exp", "log", "floor", "ceil", "fract", "sin", "cos", "tan",
        "asin", "acos", "atan", "normalize", "degrees", "radians",
    ];
    let head: String = t.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
    if UNARY.contains(&head.as_str()) {
        if let (Some(op), true) = (t.find('('), t.ends_with(')')) {
            return expr_width(types, &t[op + 1..t.len() - 1]);
        }
    }
    // swizzle：**宽度 = 分量个数**（`.y` ⇒ 1 个标量分量、`.xyz` ⇒ 3），且不超过基类型宽度。
    // ⚠ 以前这里错取"基类型宽度"（`.y` 当成 vec2 的 2）⇒ 把 `pow(float, vec.y)` 当成
    // "一向量一标量"误改（实测 `1931934283` 的 `pow(offset, g_Friction.y)` 就踩了这条）。
    if let Some((base, sw)) = t.split_once('.') {
        let b = base.trim();
        let s: String = sw.chars().take_while(|c| "xyzwrgba".contains(*c)).collect();
        if !b.is_empty() && !s.is_empty() {
            if let Some(bw) = types.get(b).and_then(|ty| width_of(ty)) {
                let n = s.chars().count() as u8;
                return Some(n.min(bw).max(1));
            }
        }
    }
    if let Some(ty) = types.get(t) {
        return width_of(ty);
    }
    // 纯浮点字面量
    if t.chars().any(|c| c.is_ascii_digit())
        && t.chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E')
    {
        return Some(1);
    }
    None
}

/// 规则③c：`pow` / `mod` / `atan` 的实参**一个向量、一个标量** ⇒ 把标量广播成同宽向量。
/// **默认关**（`WE_GLSL_SCALAR_PROMOTE=1` 才生效）。为什么需要：
///
/// * 官方管线的编译目标是 **HLSL**，不是 GLSL —— 逐字出处
/// `open-wallpaper-engine/docs/shader.md:3`：
/// "Shaders are written in GLSL version 120 with a few minor exceptions. They are then
/// translated into HLSL dynamically and compiled as shader model 3.0."
/// ⇒ 官方的 shader 源码**从不进 GLSL 编译器**。
///
/// * HLSL 的 `pow` 类型表（微软官方文档逐字，
/// <https://learn.microsoft.com/en-us/windows/win32/direct3dhlsl/dx-graphics-hlsl-pow>
/// "Type Description"）：
/// `x` = "scalar, vector, or matrix"、`y` = "float"、`ret` = "same dimension(s) as input x"
/// ⇒ `pow(vec4 x, float y) → vec4` 在 HLSL 下**合法**（标量被广播到 x 的维度）。
///
/// * GLSL ES 1.00 里 `pow`/`mod`/`atan` **没有** `(genType, float)` 这种重载
/// ⇒ 我们**直编 GLSL** 的替代管线在这类写法上比官方更严。
/// 实测素材：官方 `assets/effects/foliagesway/shaders/effects/foliagesway.vert:25-26`
/// `sines = pow(abs(sines), g_Power) * sign(sines);`（`sines` 是 `vec4`、`g_Power` 是
/// `uniform float`）⇒ 参考 GL 报
/// `no matching function for call to 'pow(vec4, float)'`、该趟被跳过。
/// 只碰 `pow`/`mod`/`atan` 这三个（GLSL 里**没有任何**标量混合重载的），
/// 且只在**两个实参宽度都认得出**、恰好「一个 ≥2、一个 =1」时才改写 ⇒ 对别的一律不动。
/// `min`/`max`/`step`/`clamp`/`mix`/`smoothstep` **不碰**（GLSL 有合法标量重载，改了反而多此一举）。
fn promote_scalar_args(src: &str) -> String {
    const BUILTINS: &[&str] = &["pow", "mod", "atan"];
    let types = declared_types(src);
    let mut out = String::with_capacity(src.len() + 32);
    let mut total = 0usize;
    for l in src.lines() {
        let (code, comment) = match l.find("//") {
            Some(p) => (&l[..p], &l[p..]),
            None => (l, ""),
        };
        let mut cur: String = code.to_string();
        // 每行最多重扫 8 次（嵌套调用一层层剥）
        for _ in 0..8 {
            let bytes: Vec<char> = cur.chars().collect();
            let mut fixed: Option<(usize, usize, String)> = None; // (实参起点, 实参终点, 新文本)
            let mut i = 0usize;
            'scan: while i < bytes.len() {
                if bytes[i].is_ascii_alphabetic() || bytes[i] == '_' {
                    let st = i;
                    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == '_') {
                        i += 1;
                    }
                    let name: String = bytes[st..i].iter().collect();
                    let ok_head =
                        st == 0 || !(bytes[st - 1].is_ascii_alphanumeric() || bytes[st - 1] == '_');
                    if ok_head && BUILTINS.contains(&name.as_str()) && i < bytes.len() && bytes[i] == '('
                    {
                        let open = i;
                        let mut depth = 0i32;
                        let mut j = i;
                        while j < bytes.len() {
                            if bytes[j] == '(' {
                                depth += 1;
                            } else if bytes[j] == ')' {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            j += 1;
                        }
                        if j < bytes.len() {
                            // 顶层逗号切实参（同时记住每个实参的字符区间）
                            let mut spans: Vec<(usize, usize)> = Vec::new();
                            let mut d2 = 0i32;
                            let mut start = open + 1;
                            let mut k = open + 1;
                            while k < j {
                                match bytes[k] {
                                    '(' => d2 += 1,
                                    ')' => d2 -= 1,
                                    ',' if d2 == 0 => {
                                        spans.push((start, k));
                                        start = k + 1;
                                    }
                                    _ => {}
                                }
                                k += 1;
                            }
                            spans.push((start, j));
                            if spans.len() == 2 {
                                let s0: String = bytes[spans[0].0..spans[0].1].iter().collect();
                                let s1: String = bytes[spans[1].0..spans[1].1].iter().collect();
                                match (expr_width(&types, &s0), expr_width(&types, &s1)) {
                                    (Some(w0), Some(w1)) if w0 >= 2 && w1 == 1 => {
                                        fixed = Some((
                                            spans[1].0,
                                            spans[1].1,
                                            format!("vec{w0}({})", s1.trim()),
                                        ));
                                    }
                                    (Some(w0), Some(w1)) if w0 == 1 && w1 >= 2 => {
                                        fixed = Some((
                                            spans[0].0,
                                            spans[0].1,
                                            format!("vec{w1}({})", s0.trim()),
                                        ));
                                    }
                                    _ => {}
                                }
                                if fixed.is_some() {
                                    break 'scan;
                                }
                            }
                        }
                        i = j + 1;
                        continue;
                    }
                    continue;
                }
                i += 1;
            }
            match fixed {
                Some((a, b, newtxt)) => {
                    let mut nb: Vec<char> = Vec::with_capacity(bytes.len() + newtxt.len());
                    nb.extend(&bytes[..a]);
                    nb.extend(newtxt.chars());
                    nb.extend(&bytes[b..]);
                    cur = nb.into_iter().collect();
                    total += 1;
                }
                None => break,
            }
        }
        out.push_str(&cur);
        out.push_str(comment);
        out.push('\n');
    }
    if total > 0 {
        out.push_str(&format!("// [hlsl-compat] 标量实参广播成向量：{total} 处\n"));
    }
    out
}

/// `WE_GLSL_SCALAR_PROMOTE=1` ⇒ 开 [`promote_scalar_args`]（**默认关**，见该函数注释）。
pub fn scalar_promote_on() -> bool {
    std::env::var("WE_GLSL_SCALAR_PROMOTE")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// ── 预处理指令容错：丢掉**没有配对 `#if` 的 `#endif` / `#else` / `#elif`** ──
/// 作者源码里确实会出现多一个 `#endif`（实测：`3704273480` 包内
/// `shaders/workshop/3082978660/effects/Simple_Audio_Bars.vert` 第 70 行 —— 前面
/// `DEFORMITY==3/1/2` 三段各自配平后还剩一个 `#endif`，全程深度变成 −1）。
/// HLSL/WE 的编译器把它当噪声跳过；GL 的预处理器直接
/// `preprocessor error: #endif without #if` ⇒ 整个顶点阶段编译失败。
/// 参照实现也是这个口径（`linux-wallpaperengine ShaderUnit.cpp:284-286`：
/// "most likely a syntax error, but we'll ignore it for now..."）。
/// **只丢确实没有开括号的那些**（按行跟踪嵌套），并把该行替换成注释 ⇒ 行号不变，
/// 后面 GL 报的行号仍然对得上原文。
pub fn drop_stray_directives(src: &str) -> String {
    let mut out = String::with_capacity(src.len() + 64);
    let mut depth = 0i32;
    let mut dropped = 0usize;
    for l in src.lines() {
        let t = l.trim_start();
        if t.starts_with("#if") {
            depth += 1;
            out.push_str(l);
        } else if t.starts_with("#endif") {
            if depth == 0 {
                dropped += 1;
                out.push_str("// [preproc-compat] 丢弃无配对 #endif（作者源码多一个；HLSL 忽略它）");
            } else {
                depth -= 1;
                out.push_str(l);
            }
        } else if (t.starts_with("#else") || t.starts_with("#elif")) && depth == 0 {
            dropped += 1;
            out.push_str("// [preproc-compat] 丢弃无配对 #else/#elif");
        } else {
            out.push_str(l);
        }
        out.push('\n');
    }
    if dropped > 0 {
        out.push_str(&format!("// [preproc-compat] 共丢弃 {dropped} 条无配对条件指令\n"));
    }
    out
}

/// 把 combo 覆盖（来自场景 / 效果趟）折算成 `#define`。
/// `names` 里所有出现的宏都会被 `normalize_conditionals` 照顾到（把 `#ifdef` 改写成 `#if`）。
fn build_defs(names: &[String], overrides: &[(String, i32)], already_defined: &[String]) -> String {
    let mut defs = String::new();
    for (n, v) in overrides {
        if already_defined.iter().any(|d| d == n) {
            continue; // 正文自己定义了，别插手
        }
        defs.push_str(&format!("#ifndef {n}\n#define {n} {v}\n#endif\n"));
    }
    for n in names {
        if overrides.iter().any(|(o, _)| o == n) || already_defined.iter().any(|d| d == n) {
            continue;
        }
        if PRELUDE.contains(&format!("#define {n}(")) || PRELUDE.contains(&format!("#define {n} ")) {
            continue;
        }
        defs.push_str(&format!("#ifndef {n}\n#define {n} 0\n#endif\n"));
    }
    defs
}

/// 顶点 + 片元一起定 combo，并叠加**逐趟覆盖**（effect.json 里每趟自己的 combos）
pub fn compose_stages_with(
    vert_body: &str,
    frag_body: &str,
    overrides: &[(String, i32)],
) -> (String, String) {
    // HLSL 宽容语义 → GLSL 严格语义（varying 类型统一 / 隐式截断 / 隐式取整 / 整数字面量提升）
    // 必须在**两个阶段一起**做（varying 类型要看两边），且在 combo 展开之前
    // （改的是声明与语句的文本，与 `#if` 无关）。实测修复：
    // color_grading 链接失败、cloudmotion 顶点编译失败、test_shader 片元编译失败。
    let (vert_body, frag_body) = hlsl_stage_compat(vert_body, frag_body);
    let (vert_body, frag_body) = (vert_body.as_str(), frag_body.as_str());
    let mut names = combo_defaults(&[vert_body, frag_body]);
    for (n, _) in overrides {
        if !names.iter().any(|x| x == n) {
            names.push(n.clone());
        }
    }
    let already = defined_in_bodies(&[vert_body, frag_body]);
    let defs = build_defs(&names, overrides, &already);
    let v = normalize_conditionals(vert_body, &names);
    let f = normalize_conditionals(frag_body, &names);
    (
        compose_with_defs(false, &defs, &v),
        compose_with_defs(true, &defs, &f),
    )
}
/// 为什么必须做：GLSL ES 里 `#if 未定义宏` 非法 ⇒ 凡是出现在 `#if` 里的 combo 都得补 0；
/// 但一补 0，原本靠"没定义"来判断的 `#ifdef X` 就变成真了 ⇒ 会把
/// `#ifdef SKINNING_ALPHA` 里那段（引用 `BONECOUNT` 的骨骼声明）错误地编译进来。
/// 只对**已补定义的那批 combo** 改写（`VERSION` 这类仍保持未定义的不能动）。
pub fn normalize_conditionals(body: &str, defs: &[String]) -> String {
    let mut out = String::with_capacity(body.len());
    for line in body.lines() {
        let t = line.trim_start();
        let indent = &line[..line.len() - t.len()];
        let mut done = false;
        for (kw, neg) in [("#ifdef", ""), ("#ifndef", "!")] {
            if let Some(rest) = t.strip_prefix(kw) {
                let name = rest.trim();
                if defs.iter().any(|d| d == name) {
                    out.push_str(&format!("{indent}#if {neg}{name}\n"));
                    done = true;
                    break;
                }
            }
        }
        if !done {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// 组装成可编译的 GLSL（`#version` 必须是最前面一行）。
/// 用 **1.20**（桌面 GL）：WE 的 shader 里有 `const float x = intExpr - 1;` 这种隐式 int→float，
/// GLSL ES 1.00 不允许，桌面 1.20 才行 —— 这也正是 WE 自己 PC 端的方言。
pub fn compose_with_defs(_stage_is_fragment: bool, defs: &str, body: &str) -> String {
    // 注意：**不发 precision 语句** —— 那是 ES 的语法，桌面 GLSL 要到 1.30 才认；
    // 1.20 的默认 float 精度就是 highp，够用。
    // ⚠ WE 的 shader 是 **GLSL ES** 写的：正文里会写 `lowp/mediump/highp` 精度限定符。
    // 桌面 GLSL 1.20 里 `mediump` 是**保留字**（不是合法限定符）⇒ 直接语法错
    // （实例：内置 `flat` shader 的 `mediump vec4`）。用三个空宏把它们吃掉即可。
    format!(
        "#version 120\n{PRELUDE}\n#define lowp\n#define mediump\n#define highp\n{defs}\n{body}"
    )
}

/// 顶点 + 片元一起定 combo（必须共用以保证 varying 声明一致）
pub fn compose_stages(vert_body: &str, frag_body: &str) -> (String, String) {
    let defs_list = combo_defaults(&[vert_body, frag_body]);
    let mut defs = String::new();
    for name in &defs_list {
        defs.push_str(&format!("#ifndef {name}\n#define {name} 0\n#endif\n"));
    }
    let v = normalize_conditionals(vert_body, &defs_list);
    let f = normalize_conditionals(frag_body, &defs_list);
    (
        compose_with_defs(false, &defs, &v),
        compose_with_defs(true, &defs, &f),
    )
}

/// 单阶段版本（测试用）
pub fn compose(stage_is_fragment: bool, body: &str) -> String {
    let defs_list = combo_defaults(&[body]);
    let already = defined_in_bodies(&[body]);
    let defs = build_defs(&defs_list, &[], &already);
    let b = normalize_conditionals(body, &defs_list);
    compose_with_defs(stage_is_fragment, &defs, &b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prelude_defines_the_helpers_we_h_only_uses() {
        for sym in ["mul(", "saturate(", "texSample2D(", "CAST3(", "lerp(", "frac("] {
            assert!(PRELUDE.contains(sym), "前导块应提供 {sym}");
        }
        // `inverse` 是 GLSL 1.40 的内置函数，我们固定用 1.20 ⇒ 必须自带
        // （实测 shape 的 lightshafts vert 直接调它，不补就整层编译失败）
        assert!(PRELUDE.contains("mat4 inverse(mat4"), "前导块应提供 inverse()");
    }

    #[test]
    fn compose_puts_version_first() {
        let s = compose(true, "void main() { gl_FragColor = vec4(1.0); }");
        assert!(s.starts_with("#version 120\n"), "{}", &s[..40.min(s.len())]);
        assert!(s.contains("gl_FragColor"));
        // 桌面 GLSL 1.20 不认 precision 语句（ES 的语法），不能发
        assert!(!s.contains("precision "), "{}", &s[..200.min(s.len())]);
    }

    /// 这段就是现场栽的那个跟头：e#if 用到的宏必须补 0，
    /// 但 #ifdef 提到过的绝不能补（否则 #ifdef 会变真）
    #[test]
    fn combo_defaults_only_covers_if_expression_macros() {
        let body = "
#if SKINNING
uniform mat4x3 g_Bones[BONECOUNT];
#endif
#ifdef SKINNING_ALPHA
varying float v_BoneAlpha;
#endif
#ifndef VERSION
uniform float g_Brightness;
#endif
#if LIGHTING || REFLECTION
uniform vec3 g_EyePosition;
#endif
varying vec2 v_TexCoord;
";
        let got = combo_defaults(&[body]);
        assert!(got.contains(&"SKINNING".to_string()), "{got:?}");
        assert!(got.contains(&"LIGHTING".to_string()), "{got:?}");
        assert!(got.contains(&"REFLECTION".to_string()), "{got:?}");
        assert!(
            !got.contains(&"SKINNING_ALPHA".to_string()),
            "#ifdef 提到的必须保持未定义：{got:?}"
        );
        assert!(
            !got.contains(&"VERSION".to_string()),
            "#ifndef 提到的必须保持未定义：{got:?}"
        );
    }

    #[test]
    fn compose_injects_defaults_in_guarded_form() {
        let body = "#if LIGHTING\nx\n#endif\nvoid main() {}";
        let s = compose(false, body);
        assert!(s.contains("#ifndef LIGHTING\n#define LIGHTING 0\n#endif"), "{s}");
    }

    /// 现场第二个跟头：补了 0 之后 `#ifdef SKINNING_ALPHA` 会变真，
    /// 把引用 BONECOUNT 的骨骼声明错误地编译进来 ⇒ 必须规范化成 `#if`
    #[test]
    fn ifdef_is_normalized_for_defined_combos_only() {
        let defs = vec!["SKINNING_ALPHA".to_string()];
        let body = "#ifdef SKINNING_ALPHA\nuniform float g_BonesAlpha[BONECOUNT];\n#endif\n#ifndef VERSION\nuniform float g_Brightness;\n#endif\n";
        let out = normalize_conditionals(body, &defs);
        assert!(out.contains("#if SKINNING_ALPHA"), "{out}");
        assert!(!out.contains("#ifdef SKINNING_ALPHA"), "{out}");
        // VERSION 不在补定义之列 ⇒ 它的 #ifndef 必须原样保留
        assert!(out.contains("#ifndef VERSION"), "{out}");
    }

    #[test]
    fn compose_stages_shares_one_combo_set() {
        // 一个阶段写 #if X、另一个写 #ifdef X ⇒ 两边都要得到同一份定义
        let (v, f) = compose_stages(
            "#if SKINNING\nvarying float v_BoneAlpha;\n#endif\nvoid main() {}",
            "#ifdef SKINNING\nvarying float v_BoneAlpha;\n#endif\nvoid main() {}",
        );
        assert!(v.contains("#define SKINNING 0"), "{v}");
        assert!(f.contains("#define SKINNING 0"), "{f}");
        assert!(f.contains("#if SKINNING"), "片元也应被规范化：{f}");
    }

    #[test]
    fn parse_shader_meta_reads_annotations() {
        let src = r#"
// [COMBO] {"material":"ui_editor_properties_cast_type","combo":"CASTER","type":"options","default":0}
// [COMBO] {"material":"ui_editor_properties_quality","combo":"SAMPLES","type":"options","default":0}
varying vec2 v_TexCoord;
uniform sampler2D g_Texture0; // {"material":"ui_editor_properties_framebuffer","hidden":true}
uniform sampler2D g_Texture1; // {"material":"ui_editor_properties_opacity_mask","default":"util/white"}
uniform float g_Length; // {"material":"ui_editor_properties_ray_length","default":0.5,"range":[0.01,1]}
uniform vec3 g_ColorRays; // {"material":"ui_editor_properties_color","default":"1 1 1","type":"color"}
void main() { }
"#;
        let m = parse_shader_meta(src);
        assert_eq!(m.combos.len(), 2, "{:?}", m.combos);
        assert_eq!(m.combos[0].combo, "CASTER");
        assert_eq!(m.combos[0].prop, "ui_editor_properties_cast_type");
        assert_eq!(m.combos[1].combo, "SAMPLES");
        let by = |n: &str| m.uniforms.iter().find(|u| u.name == n).cloned();
        assert_eq!(by("g_Length").unwrap().prop, "ui_editor_properties_ray_length");
        assert_eq!(by("g_Length").unwrap().default, "0.5");
        assert_eq!(by("g_Texture1").unwrap().default, "util/white");
        assert_eq!(by("g_ColorRays").unwrap().default, "1 1 1");
        assert_eq!(
            by("g_Texture0").unwrap().prop,
            "ui_editor_properties_framebuffer"
        );
    }

    /// 常量键 → uniform 的两种写法都要认（对齐 OWE `ResolveShaderMaterialKey`）。
    #[test]
    fn find_uniform_for_key_accepts_label_and_u_stripped_spelling() {
        let src = r#"
uniform float u_brightness; // {"material":"Brightness","default":0,"range":[-1,1]}
uniform vec3 u_channelMultiplier; // {"material":"Channel influence","default":"1 1 1"}
uniform float u_alpha; // {"material":"Opacity","default":1}
uniform mat4 g_ModelViewProjectionMatrix;
void main() { }
"#;
        let m = parse_shader_meta(src);
        // 写法 1：注解里的 material 标签
        assert_eq!(
            find_uniform_for_key(&m, "Brightness").unwrap().name,
            "u_brightness"
        );
        assert_eq!(
            find_uniform_for_key(&m, "Channel influence").unwrap().name,
            "u_channelMultiplier"
        );
        // 写法 2：uniform 名去掉前 2 字符（u_/g_）—— 以前这种键被**静默丢掉**
        assert_eq!(
            find_uniform_for_key(&m, "brightness").unwrap().name,
            "u_brightness"
        );
        assert_eq!(find_uniform_for_key(&m, "alpha").unwrap().name, "u_alpha");
        assert_eq!(
            find_uniform_for_key(&m, "channelMultiplier").unwrap().name,
            "u_channelMultiplier"
        );
        // 没有注解的 uniform 不参与写法 2（OWE 的别名表里只有带 material 的那些）
        assert!(find_uniform_for_key(&m, "ModelViewProjectionMatrix").is_none());
        // 两边都认不出来 ⇒ None（场景里写错/引擎级键，如 blur_combine 的 compositecolor）
        assert!(find_uniform_for_key(&m, "compositecolor").is_none());
        // 标签优先：标签命中时不再看写法 2
        assert_eq!(
            find_uniform_for_key(&m, "Opacity").unwrap().name,
            "u_alpha"
        );
    }

    /// 写法 2 出现**歧义**（两个不同 uniform 都能命中）时，与 OWE 一致地**放弃**。
    #[test]
    fn find_uniform_for_key_gives_up_on_ambiguity() {
        let src = r#"
uniform float u_size; // {"material":"One"}
uniform float g_size; // {"material":"Two"}
void main() { }
"#;
        let m = parse_shader_meta(src);
        assert!(find_uniform_for_key(&m, "size").is_none());
        // 但标签写法仍然各自命中
        assert_eq!(find_uniform_for_key(&m, "One").unwrap().name, "u_size");
        assert_eq!(find_uniform_for_key(&m, "Two").unwrap().name, "g_size");
    }

    /// 逐趟覆盖：CASTER=1 必须真的变成 1（否则 godrays 整链失效）
    #[test]
    fn compose_stages_with_applies_per_pass_overrides() {
        let vert = "#if CASTER\nvarying vec3 v_Dir;\n#endif\nvoid main() {}";
        let frag = "#ifdef CASTER\nvec3 d = vec3(1.0);\n#endif\nvoid main() {}";
        let (v, f) = compose_stages_with(
            vert,
            frag,
            &[("CASTER".into(), 1), ("SAMPLES".into(), 1)],
        );
        assert!(v.contains("#define CASTER 1"), "{v}");
        assert!(v.contains("#define SAMPLES 1"), "{v}");
        // 片元那边是 #ifdef，必须被规范成 #if（否则补了定义就恒真）
        assert!(f.contains("#if CASTER"), "{f}");
        assert!(!f.contains("#ifdef CASTER"), "{f}");
    }

    /// 没被覆盖的 combo 仍要补 0（GLSL ES 的硬要求）
    #[test]
    fn compose_stages_with_still_zeroes_others() {
        let frag = "#if KERNEL\nx\n#endif\n#if VERTICAL\ny\n#endif\nvoid main() {}";
        let (_, f) = compose_stages_with("void main() {}", frag, &[("KERNEL".into(), 1)]);
        assert!(f.contains("#define KERNEL 1"), "{f}");
        assert!(f.contains("#define VERTICAL 0"), "{f}");
    }

    #[test]
    fn includes_are_replaced_and_missing_ones_are_marked() {
        let assets = std::path::Path::new("/nonexistent-assets");
        let empty = std::path::PathBuf::new();
        let _ = &empty;
        // 用一个空包：所有 include 都会"找不到"，但不应 panic
        let pkg = we_scene::pkg::Pkg {
            magic: "PKGV0002".into(),
            header_end: 0,
            entries: vec![],
        };
        let src = "#include \"common.h\"\nvoid main() {}";
        let out = resolve_includes(src, &pkg, &[], assets, 0);
        assert!(out.contains("找不到 include"), "{out}");
        assert!(out.contains("void main()"), "{out}");
    }

    /// `#require` 是 WE 专有标记：GL 原样吃会报 `Illegal non-directive after #`
    /// （实测 4/17 条壁纸卡在这一行），必须换掉
    #[test]
    fn require_lighting_v1_is_inlined_and_others_commented() {
        let src = "#version 120\n#require LightingV1\nvoid main() {}\n#require SomethingElse\n";
        let out = resolve_requires(src);
        // 按行判断：注入的片段注释里也会出现 "#require LightingV1" 字样，但**不能有以它开头的行**
        assert!(
            !out.lines().any(|l| l.trim_start().starts_with("#require")),
            "不能把 #require 留给 GL"
        );
        assert!(out.contains("PerformLighting_V1"), "LightingV1 要注入实现");
        assert!(out.contains("g_LightsPosition[4]"), "灯光的 uniform 也要给");
        assert!(out.contains("未实现的 #require SomethingElse"), "未知的注释掉");
        assert!(out.contains("void main() {}"), "正文不能丢");
    }

    #[test]
    fn require_pass_is_a_noop_when_absent() {
        let src = "void main() {}\n";
        assert_eq!(resolve_requires(src), src, "没有 #require 时原样返回");
    }

    #[test]
    fn hoists_uniform_above_include_body() {
        // 真实形态：include 里的函数体先用 g_Texture0，声明却在 include 之后
        let src = "\n#include \"common_blur.h\"\n\nvarying vec4 v_TexCoord;\n\nuniform sampler2D g_Texture0; // {\"hidden\":true}\nuniform sampler2D g_Texture1;\n\nvoid main() { gl_FragColor = blur13a(v_TexCoord.xy, v_TexCoord.zw); }\n";
        let out = hoist_decls(src);
        let pos_uniform = out.find("uniform sampler2D g_Texture0").expect("声明还在");
        let pos_include = out.find("include ").expect("include 标记还在");
        assert!(pos_uniform < pos_include, "uniform 必须排到 include 之前：\n{out}");
        let pos_include_body = out.find("blur13a").unwrap();
        assert!(pos_uniform < pos_include_body, "也要排到用到它的函数体之前");
        assert!(out.contains("// {\"hidden\":true}"), "行尾的元数据注释要保住");
        assert!(out.contains("void main()"), "正文不能丢");
    }

    #[test]
    fn hoist_keeps_version_and_extension_first() {
        let src = "#version 120\n#extension GL_ARB_x : enable\nuniform sampler2D g_Texture0;\nvarying vec2 v_TexCoord;\nvoid main() {}\n";
        let out = hoist_decls(src);
        let v = out.find("#version").unwrap();
        let e = out.find("#extension").unwrap();
        let u = out.find("uniform sampler2D").unwrap();
        assert!(v < u && e < u, "#version/#extension 必须留在最前：\n{out}");
    }

    #[test]
    fn hoist_leaves_function_locals_alone() {
        // 花括号里的同名行不该被当成全局声明搬走
        let src = "uniform sampler2D g_Texture0;\nvoid main() {\n    vec4 a = texSample2D(g_Texture0, vec2(0.0));\n    if (a.a > 0.0) { varying float x = a.r; }\n}\n";
        let out = hoist_decls(src);
        assert_eq!(out.matches("uniform sampler2D g_Texture0;").count(), 1, "只应有一份声明");
        assert!(out.contains("if (a.a > 0.0)"), "函数体不能被动过");
    }

    #[test]
    fn hoist_is_noop_without_declarations() {
        let src = "void main() { gl_FragColor = vec4(1.0); }\n";
        assert_eq!(hoist_decls(src), src);
    }

    #[test]
    fn hoist_skips_declarations_inside_conditional_blocks() {
        // `#if SKINNING` 里的声明搬出去会变成无条件编译 —— 而它可能只在那个 combo 下合法
        // （真实案例：`attribute uvec4 a_BlendIndices;` 在 GLSL 1.20 下语法错）
        let src = "#if SKINNING\nattribute uvec4 a_BlendIndices;\n#endif\nuniform sampler2D g_Texture0;\nvoid main() {}\n";
        let out = hoist_decls(src);
        let guarded = out.find("attribute uvec4 a_BlendIndices;").unwrap();
        let if_pos = out.find("#if SKINNING").unwrap();
        let endif_pos = out.find("#endif").unwrap();
        assert!(
            if_pos < guarded && guarded < endif_pos,
            "守卫里的声明必须留在守卫里：\n{out}"
        );
        assert!(
            out.find("// ── 全局声明提到这里").unwrap() < out.find("uniform sampler2D g_Texture0;").unwrap(),
            "没被守卫包住的要照常上移"
        );
    }

    #[test]
    fn unit_annotation_combo_turns_on_only_when_slot_is_bound() {
        // 真实形态（effects/tint）：遮罩槽的注解里写着 combo MASK
        let src = "uniform sampler2D g_Texture0; // {\"hidden\":true}\nuniform sampler2D g_Texture1; // {\"label\":\"ui_editor_properties_opacity_mask\",\"combo\":\"MASK\"}\nvoid main() {}\n";
        let meta = parse_shader_meta(src);
        assert_eq!(meta.uniforms.len(), 2);
        assert_eq!(meta.uniforms[1].combo, "MASK");
        // 槽位 1 有贴图 ⇒ MASK=1
        assert_eq!(combos_for_units(&meta, &[0, 1]), vec![("MASK".to_string(), 1)]);
        // 只有槽位 0 ⇒ 不许开 MASK（这一条就是"整层被染黑"的开关）
        assert!(combos_for_units(&meta, &[0]).is_empty());
    }

    #[test]
    fn unit_annotation_require_map_is_honored() {
        // 真实形态（blur_precise_gaussian 的 g_Texture2）
        let src = "uniform sampler2D g_Texture2; // {\"combo\":\"MASK\",\"require\":{\"ENABLEMASK\":1}}\nvoid main() {}\n";
        let meta = parse_shader_meta(src);
        let c = combos_for_units(&meta, &[2]);
        assert!(c.contains(&("MASK".to_string(), 1)));
        assert!(c.contains(&("ENABLEMASK".to_string(), 1)));
    }

    /// 材质 JSON 的贴图槽数组里 `null` 是 WE 的**合法**写法（= 该槽没贴图）。
    /// 以前 `textures: Vec<String>` 碰到它就整条报错 ⇒ 整个材质反序列化失败 ⇒ 调用方
    /// 静默 `continue` ⇒ **整趟特效消失**（Gaze 的水波纹就是这么没的）。
    #[test]
    fn material_textures_accept_null_slots() {
        let json = br#"{"passes":[{"shader":"effects/waterripple","blending":"normal",
            "depthtest":"disabled","depthwrite":"disabled","cullmode":"nocull",
            "textures":[null,"effects/waterripplenormal"]}]}"#;
        let m: MaterialFile = serde_json::from_slice(json).expect("带 null 的贴图槽必须能解析");
        assert_eq!(m.passes.len(), 1);
        assert_eq!(m.passes[0].shader, "effects/waterripple");
        assert_eq!(m.passes[0].textures, vec!["".to_string(), "effects/waterripplenormal".to_string()]);
        // 槽 0 = null ⇒ 空串 ⇒ 下游 `!t.is_empty` 判断照旧成立
        assert!(m.passes[0].textures[0].is_empty());
    }

    /// `declares_uniform`：**没有注解**的引擎 uniform 也必须能查到（`parse_shader_meta`
    /// 查不到它们，因为 meta 只收 `// {...}` 注解行）。
    /// 这是"X-Ray 透明层不显示"的另一半根因守卫：`effects/xray` 的
    /// `xray.vert` 里 `uniform vec2 g_PointerPosition;` 与
    /// `uniform mat4 g_EffectTextureProjectionMatrixInverse;` 都**没有注解**
    /// ⇒ 用 meta 判会永远判成"这趟不用指针" ⇒ 兜底位不下发 ⇒ 透镜不出现。
    #[test]
    fn declares_uniform_sees_unannotated_engine_uniforms() {
        let src = concat!(
            "\r\n",
            "uniform mat4 g_ModelViewProjectionMatrix;\n",
            "uniform mat4 g_EffectTextureProjectionMatrixInverse;\n",
            "uniform vec4 g_Texture0Resolution;\n",
            "uniform vec2 g_PointerPosition;\n",
            "uniform float g_PointerScale; // {\"material\":\"size\",\"default\":0.2}\n",
            "// uniform vec2 g_NotDeclared;  ← 注释掉的不算\n",
            "void main() {}\n"
        );
        assert!(declares_uniform(src, "g_PointerPosition"), "无注解的引擎槽位要查到");
        assert!(declares_uniform(src, "g_EffectTextureProjectionMatrixInverse"));
        assert!(declares_uniform(src, "g_PointerScale"), "有注解的一样要查到");
        assert!(declares_uniform(src, "g_Texture0Resolution"));
        assert!(!declares_uniform(src, "g_NotDeclared"), "被注释掉的不算");
        assert!(!declares_uniform(src, "g_Time"));
        // 对照：meta 路径**查不到**无注解的那两个（正是要绕开它的原因）
        let meta = parse_shader_meta(src);
        assert!(!meta.uniforms.iter().any(|u| u.name == "g_PointerPosition"));
        assert!(meta.uniforms.iter().any(|u| u.name == "g_PointerScale"));
        // 带行尾注解的声明也要认出名字（别被注释切断）
        assert!(declares_uniform(
            "uniform vec2 g_PointerPosition; // {\"hidden\":true}\n",
            "g_PointerPosition"
        ));
    }

    /// 非字符串标量也要吃得下（同族：WE 的槽位写法很松）
    #[test]
    fn material_textures_accept_scalar_slots() {
        let json = br#"{"passes":[{"shader":"s","textures":[3,"x"]}]}"#;
        let m: MaterialFile = serde_json::from_slice(json).unwrap();
        assert_eq!(m.passes[0].textures, vec!["3".to_string(), "x".to_string()]);
    }

    /// 遍历式加载的基准：shader 声明的 sampler 槽（含 `_b` 写法）都要被枚举出来。
    #[test]
    fn declared_sampler_slots_enumerates_declared_textures_only() {
        let src = concat!(
            "uniform sampler2D g_Texture0; // {\"material\":\"ui_editor_properties_framebuffer\",\"hidden\":true}\n",
            "uniform sampler2D g_Texture1; // {\"material\":\"ui_editor_properties_water_normal\"}\n",
            "uniform sampler2D g_Texture2; // {\"material\":\"ui_editor_properties_opacity_mask\",\"mode\":\"opacitymask\",\"default\":\"util/white\"}\n",
            "uniform float g_Strength;    // {\"material\":\"ripplestrength\"}\n",
            "void main() {}\n"
        );
        let meta = parse_shader_meta(src);
        // 三个 sampler 槽都在；不带 sampler 的 float 不算槽
        assert_eq!(declared_sampler_slots(&meta), vec![0, 1, 2]);
        // `_b` 写法归到同一个槽；combo / 输入槽判定也走同一个 sampler_slot
        let src2 = "uniform sampler2D g_Texture1_b; // {\"material\":\"ui_editor_properties_opacity_mask\"}\nvoid main() {}\n";
        let m2 = parse_shader_meta(src2);
        assert_eq!(declared_sampler_slots(&m2), vec![1]);
        assert_eq!(sampler_slot("g_Texture1_b"), Some(1));
        assert_eq!(sampler_slot("g_Ratio"), None);
    }

    /// varying 类型两阶段不一致（vert vec4 / frag vec2，且顶点只写 `.xy`）⇒ **收窄顶点**，
    /// 片元一个字不动（片元里 `vec2 fragPos = v_TexCoord;` 这类算式靠片元自己的宽度）。
    /// 真实素材：`3704273480` 的 `workshop/2795521260/effects/color_grading`、
    /// `2961776805` 的 `workshop/2844906964/effects/test_shader`。
    #[test]
    fn varying_type_is_unified_by_narrowing_the_vertex() {
        let v = "attribute vec2 a_TexCoord;\nvarying vec4 v_TexCoord;\nvoid main() {\n\tv_TexCoord.xy = a_TexCoord;\n}\n";
        let f = "varying vec2 v_TexCoord;\nvoid main() {\n\tvec2 fragPos = v_TexCoord;\n\tgl_FragColor = vec4(fragPos, 0.0, 1.0);\n}\n";
        let (vv, ff) = hlsl_stage_compat(v, f);
        assert!(vv.contains("varying vec2 v_TexCoord;"), "顶点应被收窄: {vv}");
        assert!(ff.contains("varying vec2 v_TexCoord;"), "片元应保持 vec2: {ff}");
        assert!(ff.contains("vec2 fragPos = v_TexCoord;"), "{ff}");
    }

    /// 顶点写了超出片元宽度的分量（`.zw`）⇒ 两边都不动（宁可链接失败出声）
    #[test]
    fn vertex_writing_wide_components_is_left_alone() {
        let v = "varying vec4 v_TexCoord;\nvoid main() {\n\tv_TexCoord.zw = vec2(0.0);\n}\n";
        let f = "varying vec2 v_TexCoord;\nvoid main() {}\n";
        let (vv, ff) = hlsl_stage_compat(v, f);
        assert!(vv.contains("varying vec4 v_TexCoord;"), "{vv}");
        assert!(ff.contains("varying vec2 v_TexCoord;"), "{ff}");
    }

    /// 片元给 varying 赋值（HLSL 的片元输入可写 / GLSL 只读）⇒ 物化成局部副本，
    /// 原 `varying` 声明保留（链接仍要同名声明），顶点不动。
    /// 真实素材：`3704273480` 的 `Simple_Audio_Bars.frag`（`v_TexCoord.y = 1.0 - v_TexCoord.y;`）
    #[test]
    fn fragment_written_varying_becomes_a_local_copy() {
        let v = "varying vec2 v_TexCoord;\nvoid main() { v_TexCoord = vec2(1.0); }\n";
        let f = "varying vec2 v_TexCoord;\nvoid main() {\n\tv_TexCoord.y = 1.0 - v_TexCoord.y;\n}\n";
        let (_, ff) = hlsl_stage_compat(v, f);
        assert!(ff.contains("varying vec2 v_TexCoord;"), "声明要留着（链接）: {ff}");
        assert!(ff.contains("vec2 v_TexCoord__local = v_TexCoord;"), "{ff}");
        assert!(ff.contains("v_TexCoord__local.y = 1.0 - v_TexCoord__local.y;"), "{ff}");
        assert!(!ff.contains("\tv_TexCoord.y = "), "原只读赋值必须消失: {ff}");
    }

    /// 反方向（frag vec4 / vert vec2）：加宽顶点，并把顶点的赋值补成宽构造。
    /// 与参照实现 `ShaderUnit.cpp:applyLinkedVaryingCompatibility` 同形。
    #[test]
    fn narrowing_vertex_is_widened_and_its_assignment_wrapped() {
        let v = "attribute vec2 a_TexCoord;\nvarying vec2 v_TexCoord;\nvoid main() {\n\tv_TexCoord = a_TexCoord;\n}\n";
        let f = "varying vec4 v_TexCoord;\nvoid main() { gl_FragColor = vec4(v_TexCoord); }\n";
        let (vv, ff) = hlsl_stage_compat(v, f);
        assert!(vv.contains("varying vec4 v_TexCoord;"), "{vv}");
        assert!(vv.contains("v_TexCoord = vec4(a_TexCoord, 0.0, 1.0);"), "赋值要补宽构造: {vv}");
        assert!(ff.contains("varying vec4 v_TexCoord;"));
    }

    /// `#if` 守卫里的 varying 声明**不能合并**（两边守卫不同 ⇒ 会编译进不该存在的分支）
    #[test]
    fn guarded_varyings_are_left_alone() {
        let v = "#if MASK\nvarying vec2 v_Mask;\n#endif\nvoid main() {}\n";
        let f = "varying vec4 v_Mask;\nvoid main() {}\n";
        let (vv, _) = hlsl_stage_compat(v, f);
        assert!(vv.contains("varying vec2 v_Mask;"), "守卫里的声明不该被动: {vv}");
    }

    /// HLSL 隐式截断 `vec2 = vec4` ⇒ 补 swizzle。
    /// 真实素材：`3504002340` 的 `effects/cloudmotion.vert`（`v_NoiseCoord = v_TexCoord;`）
    #[test]
    fn implicit_truncating_assignment_gets_swizzle() {
        let v = "attribute vec2 a_TexCoord;\nvarying vec4 v_TexCoord;\nvarying vec2 v_NoiseCoord;\nvoid main() {\n\tv_NoiseCoord = v_TexCoord;\n\tv_TexCoord.xyzw = a_TexCoord.xyxy;\n}\n";
        let f = "varying vec4 v_TexCoord;\nvarying vec2 v_NoiseCoord;\nvoid main() {}\n";
        let (vv, _) = hlsl_stage_compat(v, f);
        assert!(vv.contains("v_NoiseCoord = v_TexCoord.xy;"), "{vv}");
        // 已经写了 swizzle 的赋值不能被再动一次
        assert!(vv.contains("v_TexCoord.xyzw = a_TexCoord.xyxy;"), "{vv}");
    }

    /// HLSL 隐式取整 `int i = <float 表达式>` ⇒ `int(...)`；
    /// 已经是 `int(...)` 或纯字面量的不动。
    /// 真实素材：`2961776805` 的 `test_shader.frag`（`for (int i = u_MinFreqRange; ...)`）
    #[test]
    fn implicit_int_conversion_is_wrapped() {
        let f = "uniform float u_MinFreqRange;\nvoid main() {\n\tfor (int i = u_MinFreqRange; i < 8; i++) {}\n\tint a = int(3.0);\n\tint b = 4;\n}\n";
        let (_, ff) = hlsl_stage_compat("void main() {}\n", f);
        assert!(ff.contains("int i = int(u_MinFreqRange)"), "{ff}");
        assert!(ff.contains("int a = int(3.0);"), "已 cast 的不该二次包装: {ff}");
        assert!(ff.contains("int b = 4;"), "纯字面量不该变: {ff}");
    }

    /// 整数字面量 + 向量实参的调用 ⇒ 字面量补 `.0`（HLSL 的隐式提升）。
    /// 真实素材：`2961776805` 的 `test_shader.frag`（`vec4(max(0, albedo.rgb), albedo.a)`）
    #[test]
    fn int_literals_are_promoted_next_to_vector_args() {
        let f = "void main() {\n\tvec3 c = max(0, vec3(0.1));\n\tgl_FragColor = vec4(max(0, c), 1.0);\n\tfloat d = max(0, 0.5);\n}\n";
        let (_, ff) = hlsl_stage_compat("void main() {}\n", f);
        let flat: String = ff.chars().filter(|c| !c.is_whitespace()).collect();
        // 数字字面量在前、向量在后 ⇒ 对调（GLSL 没有 max(float, vecN)）再补 .0
        assert!(flat.contains("max(vec3(0.1),0.0)"), "{ff}");
        assert!(flat.contains("max(c,0.0)"), "{ff}");
        assert!(flat.contains("max(0.0,0.5)"), "{ff}");
        // 已经是对的顺序（无向量实参）不动
        assert!(flat.contains("max(0.0,0.5)"));
    }

    /// 无配对的 `#endif` 要丢掉（作者源码多一个），而且**行号不变**。
    /// 真实素材：`3704273480` 的 `Simple_Audio_Bars.vert` 第 70 行。
    #[test]
    fn stray_endif_is_dropped_but_line_numbers_survive() {
        let src = "#if A\n1\n#endif\n#if B\n2\n#endif\n3\n#endif\n";
        let out = drop_stray_directives(src);
        assert_eq!(
            out.lines().count(),
            src.lines().count() + 1,
            "只多出末尾那条汇总注释"
        );
        assert_eq!(
            out.lines().nth(7).unwrap().trim_start().starts_with("// [preproc-compat]"),
            true
        );
        let mut d = 0i32;
        for l in out.lines() {
            let t = l.trim_start();
            if t.starts_with("#if") {
                d += 1;
            } else if t.starts_with("#endif") {
                d -= 1;
            }
        }
        assert_eq!(d, 0, "丢完之后必须配平");
    }

    /// 正常的 `#if/#else/#endif` 一个字符都不许动（默认路径逐字不变）
    #[test]
    fn balanced_directives_are_untouched() {
        let src = "#if A\n1\n#else\n2\n#endif\n#ifdef B\n3\n#endif\n";
        assert_eq!(drop_stray_directives(src), src);
    }

    /// -1：材质 JSON 的 combo 键要对齐到 **shader 自己的拼写**
    /// （官方 `materials/SteamLogoShattered.json` 写小写 `version`，而
    /// `genericimage2.frag` 里是大写 `#ifndef VERSION` ⇒ 不映射就造出另一个宏、分支照旧选错）。
    #[test]
    fn canonical_combo_name_maps_material_case_onto_shader_spelling() {
        let src = "#ifndef VERSION\nuniform float g_Brightness;\n#endif\n\
                   #if BLENDMODE == 1\n#endif\n#if defined(FOO) || VERTICAL\n#endif\n";
        assert_eq!(canonical_combo_name(src, "version").as_deref(), Some("VERSION"));
        assert_eq!(canonical_combo_name(src, "blendmode").as_deref(), Some("BLENDMODE"));
        assert_eq!(canonical_combo_name(src, "vertical").as_deref(), Some("VERTICAL"));
        assert_eq!(canonical_combo_name(src, "VERSION").as_deref(), Some("VERSION"));
        assert_eq!(canonical_combo_name(src, "nope"), None);
        assert_eq!(canonical_combo_name(src, ""), None);
    }

    /// -1：combo 值折算（`2`/`true`/`"2"` 都认；认不出 ⇒ `None`，**不许**当 0 塞进去）。
    #[test]
    fn combo_json_i32_accepts_int_bool_and_numeric_string() {
        use serde_json::json;
        assert_eq!(combo_json_i32(&json!(2)), Some(2));
        assert_eq!(combo_json_i32(&json!(0)), Some(0));
        assert_eq!(combo_json_i32(&json!(true)), Some(1));
        assert_eq!(combo_json_i32(&json!(false)), Some(0));
        assert_eq!(combo_json_i32(&json!("3")), Some(3));
        assert_eq!(combo_json_i32(&json!(" 4 ")), Some(4));
        assert_eq!(combo_json_i32(&json!("x")), None);
        assert_eq!(combo_json_i32(&json!(null)), None);
    }

    /// -1：材质趟的 `combos` 字段真的被反序列化出来（默认空 ⇒ 老行为不变）。
    #[test]
    fn material_pass_deserializes_combos_and_defaults_empty() {
        let j = br#"{"passes":[{"shader":"genericimage2","blending":"translucent",
                      "combos":{"version":2,"VERTICAL":true},"textures":["a"]}]}"#;
        let m: MaterialFile = serde_json::from_slice(j).expect("材质应能解析");
        assert_eq!(m.passes[0].combos.len(), 2);
        assert_eq!(m.passes[0].combos.get("version"), Some(&serde_json::json!(2)));
        // 没写 combos ⇒ 空表（**不是报错**）
        let j2 = br#"{"passes":[{"shader":"genericimage2","textures":["a"]}]}"#;
        let m2: MaterialFile = serde_json::from_slice(j2).expect("材质应能解析");
        assert!(m2.passes[0].combos.is_empty());
    }

    /// 规则③c：`pow(向量, 标量)` ⇒ 把标量广播成同宽向量。
    /// 真实素材：官方 `assets/effects/foliagesway/shaders/effects/foliagesway.vert:25`
    /// （`pow(abs(sines), g_Power)`，`sines` 是 `vec4`、`g_Power` 是 `uniform float`）。
    /// 出处：HLSL `pow` 的类型表（x 可 vector、y 是 float、ret 同 x 维度）+
    /// WE 官方管线是 GLSL120→HLSL→SM3.0（`open-wallpaper-engine/docs/shader.md:3`）。
    #[test]
    fn scalar_arg_is_broadcast_to_the_vector_width() {
        let src = "uniform float g_Power;\nvoid main() {\n\tvec4 sines = abs(vec4(1.0));\n\tsines = pow(abs(sines), g_Power) * sign(sines);\n}\n";
        let out = promote_scalar_args(src);
        let flat: String = out.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(flat.contains("pow(abs(sines),vec4(g_Power))"), "{out}");
        // 行数不变（只是把实参包起来）
        assert_eq!(out.lines().count(), src.lines().count() + 1, "只多末尾汇总注释");
    }

    /// 规则③c **必须**对下面这些"不动"：
    /// · `min/max/step/clamp/mix`（GLSL 本来就有标量重载，改了是多此一举）
    /// · 两实参同宽（`pow(vec2, a.xy)`）
    /// · 宽度认不出（未知标识符）
    /// · 字符串/没有任何 `pow|mod|atan` 的代码
    #[test]
    fn scalar_promotion_touches_nothing_else() {
        let src = "uniform float f;\nuniform vec3 v;\nvoid main() {\n\tfloat a = max(f, v.x);\n\tfloat b = max(v.x, f);\n\tvec2 c = pow(v.xy, v.xy);\n\tfloat d = pow(f, unknownthing);\n\tfloat e = max(unknownthing, f);\n}\n";
        let out = promote_scalar_args(src);
        let flat: String = out.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(flat.contains("max(f,v.x)"), "{out}");
        assert!(flat.contains("max(v.x,f)"), "{out}");
        assert!(flat.contains("pow(v.xy,v.xy)"), "{out}");
        assert!(flat.contains("pow(f,unknownthing)"), "{out}");
        assert!(flat.contains("max(unknownthing,f)"), "{out}");
        assert!(!out.contains("vec3(f)") && !out.contains("vec2(unknownthing)"), "{out}");
    }

    /// 规则③c 的**默认关**：不设 `WE_GLSL_SCALAR_PROMOTE` 时 `hlsl_stage_compat` 的产物
    /// 与"没有这条规则"逐字节相同。这里用「开关关 ⇒ 产物里不含本规则的标记注释」钉住。
    #[test]
    fn scalar_promotion_is_off_unless_switched_on() {
        assert!(!scalar_promote_on(), "默认必须是关（不设 / ≠1）");
        let v = "uniform float g_Power;\nvoid main() {\n\tvec4 sines = abs(vec4(1.0));\n\tsines = pow(abs(sines), g_Power);\n}\n";
        let (vv, _) = hlsl_stage_compat(v, "void main() {}\n");
        assert!(
            !vv.contains("标量实参广播"),
            "开关关时这条规则一个字符都不该动：{vv}"
        );
    }

    /// 官方 `effect.json` 的**真实那一行**（第 5 行 `//"preview" : "preview/project.json",`）：
    /// 过 `we_scene::json::strip_comments` 后能反序列化成 [`EffectFile`]。
    /// 方言宽容层本身在 `we_scene::json` 的单测里（含开关关/开两条路）。
    #[test]
    fn official_effect_json_comment_line_parses_after_stripping() {
        let src = "{\r\n\t\"name\" : \"Water Ripple\",\r\n\t\"description\" : \"x\",\r\n\t\"group\" : \"image\",\r\n\t//\"preview\" : \"preview/project.json\",\r\n\t\"passes\" :\r\n\t[\r\n\t\t{\r\n\t\t\t\"material\" : \"materials/effects/waterripple.json\"\r\n\t\t}\r\n\t],\r\n\t\"dependencies\" : [\"materials/effects/waterripple.json\"]\r\n}";
        assert!(
            serde_json::from_str::<EffectFile>(src).is_err(),
            "原样（带注释）必须失败 —— 这就是缺口本身"
        );
        let stripped = we_scene::json::strip_comments(src);
        assert_eq!(
            stripped.lines().count(),
            src.lines().count(),
            "剥注释不能改行号"
        );
        let ef: EffectFile =
            serde_json::from_str(&stripped).expect("剥掉注释后应能解析成 EffectFile");
        assert_eq!(ef.group, "image");
        assert_eq!(ef.passes.len(), 1);
        assert_eq!(ef.passes[0].material, "materials/effects/waterripple.json");
        assert_eq!(ef.dependencies.len(), 1);
    }

    /// swizzle 的宽度 = **分量个数**，不是"基类型的宽度"：
    /// `g_Friction.y` 是**标量**（1 个分量）⇒ `pow(float, vecN.y)` 两边都是标量、一个字符都不该动。
    /// 真因：这条取错宽度会让规则在**本来能编译**的 shader 上乱改（实测 `1931934283` 的
    /// `offset = mix(1.0 - pow(1.0 - offset, g_Friction.x), pow(offset, g_Friction.y), base);`
    /// 被误改成 `pow(vec2(offset), g_Friction.y)`）。
    #[test]
    fn swizzle_width_is_component_count_not_base_width() {
        let src = "uniform vec2 g_Friction;\nvoid main() {\n\tfloat offset = 0.5;\n\toffset = 1.0 - pow(1.0 - offset, g_Friction.x);\n\toffset = pow(offset, g_Friction.y);\n\tvec3 v = vec3(1.0);\n\tv = pow(v, g_Friction.y);\n}\n";
        let out = promote_scalar_args(src);
        let flat: String = out.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(flat.contains("pow(1.0-offset,g_Friction.x)"), "两个标量不许动: {out}");
        assert!(flat.contains("pow(offset,g_Friction.y)"), "两个标量不许动: {out}");
        assert!(
            flat.contains("pow(v,vec3(g_Friction.y))"),
            "向量 × 标量分量才该广播: {out}"
        );
    }
}
