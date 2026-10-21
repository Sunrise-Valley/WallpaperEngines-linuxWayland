// SPDX-License-Identifier: MIT
//! WE 的粒子系统（格式源自 Source 引擎那套：`emitter` / `initializer` / `operator` / `renderer`）。
//! 覆盖的语义（全部**数据驱动**：只认字段名与 WE 的公式，不认场景/对象名）：
//!
//! * emitter：`sphererandom`（方向锥 + 距离区间 + rate）
//!
//! * initializer：`lifetimerandom` / `sizerandom` / `colorrandom` / `alpharandom` / `velocityrandom` /
//! `turbulentvelocityrandom` / `rotationrandom` / `angularvelocityrandom`
//!
//! * operator：`movement`（gravity + drag）/ `alphafade` / `sizechange` / `colorchange` /
//! `oscillateposition` / `oscillatealpha` / `turbulence`（Perlin curl noise）/ `angularmovement` /
//! **`oscillatesize`** / **`vortex`（含 `vortex_v2`）** / **`controlpointattract`** /
//! **`boids`** / **`collisionquad`**
//!
//! * initializer（本次新增）：**`mapsequencearoundcontrolpoint`**
//!
//! * 预设级 `controlpoint` 数组（8 个槽位）—— `vortex` / `controlpointattract` /
//! `mapsequencearoundcontrolpoint` 都按槽号取控制点位置（`origin + offset`）。
//!
//! * **音频响应**：emitter 的 `audioprocessing*`（OWE 的发射率倍率）、`turbulence` 的
//! `audioprocessing*`（速度）、`turbulentvelocityrandom` 的 `audioprocessing*`（初速）、
//! `vortex` 的 `audioprocessingmode`（无音频 ⇒ 整条算子跳过）。频谱是 **16 段**
//! （WE 的 `audio_average` 就是 `array<float,16>`），`audiofrequencystart/end` 是**段下标**
//! 详见 [`crate::audio`]。**没有频谱时全部恒等**（本机默认：不开音频功能 ⇒ 全 0 ⇒
//! 17 条场景逐字节不变）。
//! 开关：`WE_PARTICLE_LEGACY=1` 关掉**两轮**的全部新语义；
//! `WE_PARTICLE_OPS_OFF=1` 只关掉**本次**这批（`oscillatesize`/`vortex`/`controlpointattract`/
//! `boids`/`collisionquad`/`mapsequencearoundcontrolpoint`/音频响应），保留上次已确认的。
//! 两者都未设 = 全开（默认）。
//!
//! * renderer：`sprite` / `spritetrail`（= WE 的 `TRAILRENDERER` 组合开关 + `g_RenderVar0`；
//! quad 沿速度方向拉长、宽度方向取屏幕平面内的垂线）
//!
//! * 场景级 `instanceoverride`：alpha / rate / count / lifetime / size / speed / colorn
//! **每个字段都可能是 `{"user":"<属性>","value":…}` 绑定对象**（WE/OWE 就是这么存的）
//!
//! * 精灵表：`animationmode`（`randomframe` ⇒ 每颗粒子**固定**在一帧；其余 ⇒ 按寿命推进）与
//! `sequencemultiplier`（推进倍率）；帧号由 WE 官方顶点 shader 用
//! `g_RenderVar1` + `a_TexCoordVec4C1.w` 选（见 `gl_ctx::GlBatch`）
//!
//! * 预设级 `starttime`：延迟出场（`start_delay`）
//! 坐标系：**场景坐标**（4000x2736 那块屏），粒子按屏幕朝向的 billboard 画。

use serde::Deserialize;

/// 宽容的数字反序列化：WE 的粒子数据里**同一个字段**可能是
/// 数字 `3` / `null` / 数字字符串 `"3"` / 向量字符串 `"1024 512 0"`。
/// 以前这些字段直接声明成 `f32` ⇒ `invalid type: null, expected f32` ⇒ **整条预设解析失败**，
/// 后果是 Ice Princess 的 28 个粒子对象里 24 个被静默丢掉 —— 用户看到的"win 版那层白雾"
/// （`Fog 1`，6 个实例）就是这么没的。向量字符串取**绝对值最大的分量**（保守：
/// 宁可保留横向铺开，也别把 boxrandom 的 spread 缩成 0）。
pub fn de_f32_lenient<'de, D>(d: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(opt_number(&v).unwrap_or(0.0))
}

/// `Option<f32>` 的宽容反序列化：数字 / null / 数字串 / 向量串（取首个分量）/**绑定对象**。
pub fn de_f32_opt_lenient<'de, D>(d: D) -> Result<Option<f32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(opt_number(&v))
}

/// A/B 开关：`WE_PARTICLE_LEGACY=1` 把**本次新实现**的粒子语义整套关掉，
/// 重现改动前的行为（用于量化"影响面"—— 没有 VCS/旧二进制时这是唯一能拿到
/// 同二进制 before/after 的办法）。关掉的东西：
/// · `instanceoverride` 的宽松解析（改前：字段是绑定对象 ⇒ 整条覆盖丢光）
/// · `animationmode:"randomframe"` 的"钉帧"与 `sequencemultiplier`
/// · `rotationrandom` / `angularvelocityrandom` / `angularmovement`
/// · `turbulence` / `oscillatealpha`
/// · `spritetrail` 的 `TRAILRENDERER` 组合开关（见 lib.rs 的同一开关）
/// 默认（未设）= 新语义。
fn legacy() -> bool {
    std::env::var("WE_PARTICLE_LEGACY").ok().as_deref() == Some("1")
}

/// **本次（第二轮）新算子的独立开关**：`WE_PARTICLE_OPS_OFF=1` 只关掉**本次**加的语义
/// （`oscillatesize` / `vortex` / `controlpointattract` / `boids` / `collisionquad` /
/// `mapsequencearoundcontrolpoint` / 音频响应），**保留上次**已确认的那批
/// （`spritetrail`/`turbulence`/`oscillatealpha`/`angularmovement`/`sequencemultiplier`/…）。
/// 为什么需要它：`WE_PARTICLE_LEGACY=1` 会把**两轮**的东西一起关掉，拿它当"本次改前"的
/// 对照会把上次的差异混进来（实测 `2481886649` 的 `Ember` 就这么被 `turbulence` 带偏：
/// 那 69.8 单位/秒的速度是上次的湍流，不是本次任何算子）。默认**关**（= 本次新语义生效）。
fn ops_off() -> bool {
    std::env::var("WE_PARTICLE_OPS_OFF").ok().as_deref() == Some("1")
}

/// 本次新语义是否生效（= 两个开关都没拨）。
fn new_ops_on() -> bool {
    !legacy() && !ops_off()
}

// ══════════════════════════════════════════════════════════════════════════════
// 本次（遗留三件）新开关与缺省值
// ══════════════════════════════════════════════════════════════════════════════

/// **发射器缺省值**开关。`WE_EMITTER_DEFAULTS=0` 回旧行为（缺省 `rate`=0 ⇒ 不发射、
/// 缺省 `distancemax`=(0,0,0) ⇒ 盒退化成点）。默认**开**。
/// 拆开也可：`WE_EMITTER_RATE_DEFAULT=0` / `WE_EMITTER_DISTMAX_DEFAULT=0` 单独回退。
fn emitter_defaults_on() -> bool {
    std::env::var("WE_EMITTER_DEFAULTS").ok().as_deref() != Some("0")
}
fn emitter_rate_default_on() -> bool {
    emitter_defaults_on() && std::env::var("WE_EMITTER_RATE_DEFAULT").ok().as_deref() != Some("0")
}
fn emitter_distmax_default_on() -> bool {
    emitter_defaults_on() && std::env::var("WE_EMITTER_DISTMAX_DEFAULT").ok().as_deref() != Some("0")
}

/// WE 发射器缺省 `rate`（粒子/秒）。**两参照分歧**：
/// · linux-wallpaperengine `ObjectParser.cpp:601`：`it.optional("rate", 10.0f)` ⇒ **10**
/// · open-wallpaper-engine `ParticleObject.cppm:90`：`float rate { 5.0f }` ⇒ 5
/// 取 **10**：linux-WE 是本项目的裁判，且任务给的 WE 值也是 10。
/// 旧实现缺省 0 ⇒ 该字段缺省的发射器（本机 `1195626192` 的 `Ember small`、
/// 官方 `ember_small.json` / `dust_motes_0.json`）**一颗都不发射**。
const EMITTER_RATE_DEFAULT: f32 = 10.0;
/// WE 发射器缺省 `distancemax`（每轴）。**两参照分歧只在 z**：
/// · open-wallpaper-engine `ParticleObject.cppm:73`：`{256, 256, 256}`
/// · linux-wallpaperengine `ObjectParser.cpp:598`：`(256, 256, 0)`
/// 取 **三轴 256**（OWE + WE 编辑器里 Distance max 是个单值 ⇒ 三轴同值）；z 的分歧被
/// 缺省 `directions (1,1,0)` 的 z 掩膜吃掉（boxrandom 的盒是逐轴 `× directions`）。
/// 旧实现缺省 0 ⇒ 该字段缺省的盒退化成**一个点**（本机 `1931934283` 的
/// `new particle system*`、`2997301801` 的 `reactive Stars`）。
const EMITTER_DISTANCEMAX_DEFAULT: [f32; 3] = [256.0, 256.0, 256.0];

/// 对象级控制点覆盖的总开关。`WE_CP_OVERRIDE=0` 回旧行为（忽略 `instanceoverride.controlpointN`）。
/// 默认**开**：`instanceoverride.controlpointN` 是 WE 编辑器能拖的对象级控制点坐标，
/// 语义见 `ParticleSystem::new` 里那段（OWE `ParticleRuntime.cpp:605-631`）。
fn cp_override_on() -> bool {
    std::env::var("WE_CP_OVERRIDE").ok().as_deref() != Some("0")
}

/// `serde` 缺省函数：`emitter.rate` 键缺失时用的值（见 [`EMITTER_RATE_DEFAULT`]）。
fn default_emitter_rate() -> f32 {
    default_emitter_rate_with(emitter_rate_default_on())
}
/// 纯函数版（好写单测、不依赖进程环境）：`on` = 是否启用 WE 缺省。
fn default_emitter_rate_with(on: bool) -> f32 {
    if on {
        EMITTER_RATE_DEFAULT
    } else {
        0.0
    }
}

/// 取发射器 `distancemax`（vec3，逐轴）。键缺失 / `null` ⇒ WE 缺省
/// [`EMITTER_DISTANCEMAX_DEFAULT`]）；显式 `0` / `"1024 512 0"` 原样返回。
fn emitter_distancemax(em: &Emitter) -> [f32; 3] {
    emitter_distancemax_with(em, emitter_distmax_default_on())
}
/// 纯函数版（好写单测、不依赖进程环境）。
fn emitter_distancemax_with(em: &Emitter, on: bool) -> [f32; 3] {
    if em.distancemax.is_null() && on {
        EMITTER_DISTANCEMAX_DEFAULT
    } else {
        vec3_of(&em.distancemax, [0.0, 0.0, 0.0])
    }
}

/// 算子 blend-in/out 总开关。`WE_OP_BLEND=0` 回旧行为（忽略这四个字段）。默认**开**
/// （缺省权重恒 1 ⇒ 对没写这些字段的场景逐字节不变）。
fn op_blend_on() -> bool {
    std::env::var("WE_OP_BLEND").ok().as_deref() != Some("0")
}

/// 本次：算子的 **blend-in / blend-out** 窗口。
/// 语义（WE 官方文档 `docs.wallpaperengine.io/en/scene/particles/component/operator.html`
/// 的 'Operator Blending' 一节，原文照抄要点）：
/// 「Many operators have a section regarding operator blending. This optional functionality
/// allows you to start and stop the specific operator during specific points in a single
/// particle's lifetime. All values are provided in a range of 0.00 to 1.00 where 0.00
/// represents the spawn time of particle while 1.00 represents the last frame of a particle.」
/// 四个键 = `blendinstart`（开始淡入）/ `blendinend`（淡入结束、此后满强度）/
/// `blendoutstart`（开始淡出）/ `blendoutend`（完全停止）。OWE 的编辑器文案与之对应：
/// `docs/ui_en-us.json:1613-1617` = 'Blend-in end' / 'Blend-in start' / 'Blend-out end' /
/// 'Blend-out start'。
/// ⚠ **两份参照实现都没有实现它**：OWE 与 linux-wallpaperengine 的源码里搜 `blendin`
/// 只命中材质的 `blending`、着色器常量与 UI 文案，没有任何算子级实现
/// （本机有 `blendinstart` 的官方预设 = `assets/presets/lightning/*` 的 `turbulence`/
/// `oscillateposition`/`remapvalue`、`assets/presets/fireworks/*` 的 `oscillatealpha`、
/// `assets/scenes/particleelementpreviews/capvelocity/*` 的 `capvelocity`）。
/// ⇒ 语义只来自 WE 官方文档 + 官方预设写法。
/// 权重 = `ramp_in(t) × ramp_out(t)`；两端退化（`s == e`）时按"在 s 处瞬间开/关"处理。
/// 缺省 ⇒ `blendinstart = blendinend = 0`、`blendoutstart = blendoutend = 1`
/// ⇒ 权重恒 1（= 旧行为，`w >= 1.0` 时**原值直通**，保证 IEEE 逐位一致）。
#[derive(Debug, Clone, Copy)]
struct Blend {
    bin_s: f32,
    bin_e: f32,
    bout_s: f32,
    bout_e: f32,
}

impl Blend {
    /// 算子声明了任一 blend 字段才返回 `Some`（否则 `None` ⇒ 完全不碰）。
    fn from_op(op: &Operator) -> Option<Blend> {
        if !op_blend_on()
            || (op.blendinstart.is_none()
                && op.blendinend.is_none()
                && op.blendoutstart.is_none()
                && op.blendoutend.is_none())
        {
            return None;
        }
        Some(Blend {
            bin_s: op.blendinstart.unwrap_or(0.0),
            bin_e: op.blendinend.unwrap_or(0.0),
            bout_s: op.blendoutstart.unwrap_or(1.0),
            bout_e: op.blendoutend.unwrap_or(1.0),
        })
    }
    /// `t01` = 寿命比例（`age / life`）。返回 0..1 的作用权重。
    #[inline]
    fn weight(&self, t01: f32) -> f32 {
        ramp_in(t01, self.bin_s, self.bin_e) * ramp_out(t01, self.bout_s, self.bout_e)
    }
}

/// 淡入斜坡：`t <= s` ⇒ 0；`t >= e` ⇒ 1；中间线性。`s == e` ⇒ `t < s` 时 0、`t >= s` 时 1。
#[inline]
fn ramp_in(t: f32, s: f32, e: f32) -> f32 {
    if e > s {
        ((t - s) / (e - s)).clamp(0.0, 1.0)
    } else if t >= s {
        1.0
    } else {
        0.0
    }
}
/// 淡出斜坡：`t <= s` ⇒ 1；`t >= e` ⇒ 0；中间线性。`s == e` ⇒ `t < e` 时 1、`t >= e` 时 0。
#[inline]
fn ramp_out(t: f32, s: f32, e: f32) -> f32 {
    if e > s {
        (1.0 - (t - s) / (e - s)).clamp(0.0, 1.0)
    } else if t >= e {
        0.0
    } else {
        1.0
    }
}
/// 把一个"以 `1.0` 为恒等"的乘子按权重 `w` 从恒等插值：`w >= 1` 原值直通（IEEE 逐位一致）。
#[inline]
fn blend_mul(w: f32, x: f32) -> f32 {
    if w >= 1.0 {
        x
    } else if w <= 0.0 {
        1.0
    } else {
        1.0 + (x - 1.0) * w
    }
}
/// 把一个"以 `0.0` 为恒等"的量（力/位移/速度增量）按权重 `w` 缩放。
#[inline]
fn blend_add(w: f32, x: f32) -> f32 {
    if w >= 1.0 {
        x
    } else {
        x * w
    }
}

/// 改前那套**严格** `instanceoverride` 解析的复刻（只给 `WE_PARTICLE_LEGACY=1` 用）：
/// 当时字段都是 `Option<f32>` / `Option<String>`，任一字段的类型不是（数字 / 字符串）
/// 典型就是 `{"user":"_6","value":0.95}` 这种绑定对象 —— serde 直接报错 ⇒ 调用方
/// `.ok.unwrap_or_default` ⇒ **整条 override 丢光**。这就是 `2080534540` 的雨
/// 尺寸/速度/密度全部跑到作者意图之外的真正原因。
pub fn parse_override_strict(v: Option<&serde_json::Value>) -> InstanceOverride {
    let Some(obj) = v.and_then(|x| x.as_object()) else {
        return InstanceOverride::default();
    };
    for k in ["alpha", "rate", "count", "lifetime", "size", "speed"] {
        if let Some(x) = obj.get(k) {
            if !x.is_null() && !x.is_number() {
                return InstanceOverride::default();
            }
        }
    }
    if let Some(x) = obj.get("colorn") {
        if !x.is_null() && !x.is_string() {
            return InstanceOverride::default();
        }
    }
    let num = |k: &str| obj.get(k).and_then(|x| x.as_f64()).map(|f| f as f32);
    InstanceOverride {
        alpha: num("alpha"),
        rate: num("rate"),
        count: num("count"),
        lifetime: num("lifetime"),
        size: num("size"),
        speed: num("speed"),
        colorn: obj.get("colorn").and_then(|x| x.as_str()).map(String::from),
        // 旧（改前）路径不认对象级控制点覆盖 ⇒ 保持 None（逐字节对齐旧二进制）。
        ..Default::default()
    }
}

/// 从任何 WE 形态里取一个标量：数字 / 数字串 / 向量串（取首个分量）/ `{"user":…,"value":…}` 绑定对象。
/// **绑定对象这一条是关键**：WE 的 `instanceoverride.count` 之类常写成
/// `{"user":"_6","value":0.95}`，只认数字会让**整个** `instanceoverride` 反序列化失败
/// （serde 对"字段存在但类型不对"是硬错）⇒ 场景作者给的 size/speed/rate/lifetime 全部失效。
fn opt_number(v: &serde_json::Value) -> Option<f32> {
    match v {
        serde_json::Value::Number(n) => n.as_f64().map(|f| f as f32),
        serde_json::Value::String(s) => s
            .split_whitespace()
            .next()
            .and_then(|t| t.parse::<f32>().ok()),
        serde_json::Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        serde_json::Value::Array(a) => a.first().and_then(opt_number),
        serde_json::Value::Object(m) => m.get("value").and_then(opt_number),
        _ => None,
    }
}

/// `Option<String>` 的宽容反序列化：字符串 / `{"user":…,"value":"1 0 0"}` / `[r,g,b]` 数组。
pub fn de_string_opt_lenient<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(match &v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(m) => m.get("value").and_then(|x| match x {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Array(a) => Some(
                a.iter()
                    .filter_map(|n| n.as_f64())
                    .map(|f| f.to_string())
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        }),
        serde_json::Value::Array(a) => Some(
            a.iter()
                .filter_map(|n| n.as_f64())
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join(" "),
        ),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

/// **宽容的 vec3** 反序列化（`Option`）：`"x y z"` 串 / `[x,y,z]` 数组 / 单数字（三轴同值）/
/// `{"value":…}` 对象；`null` / 缺失 / 其它 → `None`。
/// 用途：`instanceoverride.controlpointN`（对象级控制点坐标，WE 写 `"196.797 176.936 0.000"`）。
pub fn de_vec3_opt_lenient<'de, D>(d: D) -> Result<Option<[f32; 3]>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    let inner = match &v {
        serde_json::Value::Object(m) => m.get("value").unwrap_or(&serde_json::Value::Null),
        other => other,
    };
    Ok(match inner {
        serde_json::Value::Number(n) => {
            let x = n.as_f64().unwrap_or(0.0) as f32;
            Some([x, x, x])
        }
        serde_json::Value::String(s) => {
            let p: Vec<f32> = s.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            match p.len() {
                0 => None,
                1 => Some([p[0], p[0], p[0]]),
                _ => Some([p[0], p[1], p[2]]),
            }
        }
        serde_json::Value::Array(a) => {
            let p: Vec<f32> = a.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect();
            match p.len() {
                0 => None,
                1 => Some([p[0], p[0], p[0]]),
                _ => Some([p[0], p[1], p[2]]),
            }
        }
        _ => None,
    })
}

/// **宽容的字符串**反序列化（非 `Option`）：字符串原样；数字 → 十进制串；数组 → 空格连接；
/// 对象取 `value`；`null`/其它 → 空串。
/// ⚠ 这条是**必需**的：WE 的字段类型很松，同名键在不同预设里可能是**数字**也可能是字符串。
/// 实测反例：`initializer[].offset` 在 `particles/presets/ember_small.json` 里是 **`-0.5`（浮点）**，
/// 把它声明成 `String` ⇒ serde 硬错 ⇒ **整份预设解析失败** ⇒ 那一族的粒子对象全部静默消失
/// （实测 17 条里 12 条的 `Ember small` 不见了）。宽容解析让"类型不对"退化成"取默认"，
/// 而不是丢掉整个对象家族（与 `de_f32_lenient` 同一套哲学）。
pub fn de_string_lenient<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(string_of(&v))
}

fn string_of(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => {
            if *b {
                "1".to_string()
            } else {
                "0".to_string()
            }
        }
        serde_json::Value::Array(a) => a
            .iter()
            .map(string_of)
            .collect::<Vec<_>>()
            .join(" "),
        serde_json::Value::Object(m) => m.get("value").map(string_of).unwrap_or_default(),
        _ => String::new(),
    }
}

/// 预设级 `controlpoint` 数组：数组 → 逐项解析（单项失败就跳过该项，不丢整份预设）；
/// `null` / 其它类型 → 空数组。
pub fn de_cp_list<'de, D>(d: D) -> Result<Vec<ControlPoint>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(match v {
        serde_json::Value::Array(a) => a
            .iter()
            .filter_map(|x| serde_json::from_value::<ControlPoint>(x.clone()).ok())
            .collect(),
        _ => Vec::new(),
    })
}

/// 从 `serde_json::Value` 里取一个标量（数字 / "0.5" / "0.5 0.5" 取首项）。
fn num_of(v: &serde_json::Value, d: f32) -> f32 {
    match v {
        serde_json::Value::Number(n) => n.as_f64().map(|f| f as f32).unwrap_or(d),
        serde_json::Value::String(s) => s
            .split_whitespace()
            .next()
            .and_then(|t| t.parse::<f32>().ok())
            .unwrap_or(d),
        serde_json::Value::Array(a) => a
            .first()
            .and_then(|x| x.as_f64())
            .map(|f| f as f32)
            .unwrap_or(d),
        _ => d,
    }
}

/// 从 `serde_json::Value` 里取 vec3（数字 → 三轴同值；"1 0.74 0" → 逐轴；数组同理）。
fn vec3_of(v: &serde_json::Value, d: [f32; 3]) -> [f32; 3] {
    match v {
        serde_json::Value::Number(n) => {
            let x = n.as_f64().unwrap_or(0.0) as f32;
            [x, x, x]
        }
        serde_json::Value::String(s) => {
            let t: Vec<f32> = s
                .split_whitespace()
                .filter_map(|x| x.parse::<f32>().ok())
                .collect();
            match t.len() {
                0 => d,
                1 => [t[0], t[0], t[0]],
                _ => [t[0], t[1], t[2]],
            }
        }
        serde_json::Value::Array(a) => {
            let t: Vec<f32> = a
                .iter()
                .filter_map(|x| x.as_f64().map(|f| f as f32))
                .collect();
            match t.len() {
                0 => d,
                1 => [t[0], t[0], t[0]],
                _ => [t[0], t[1], t[2]],
            }
        }
        _ => d,
    }
}

// ⚠ Rust 不允许宏展开成结构体字段（`macros cannot expand to struct fields`）⇒ 只能逐个写属性。

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Emitter {
    pub name: String,
    pub origin: String,
    pub directions: String,
    /// 发射器级**距离区间**。⚠ WE 里这是 **vec3**（本机实测 `"512 256 0"` / `"128 64 0"`），
    /// 不是标量：`boxrandom` 的盒子是**逐轴**的（OWE `ParticleEmitter.cpp:194-210`：
    /// `position[c] = lerp(rand(-1,1), min[c], max[c]) × directions[c]`；linux-WE
    /// `CParticle.cpp:470-484` 同型）。旧代码声明成 `f32` + `de_f32_lenient`（取第 0 个分量）
    /// ⇒ 逐轴信息丢光、只剩一个标量距离；`Value` 存原样，取标量走 `num_of`（= 旧值，逐位同）。
    /// 本次：**键缺失 ⇒ WE 缺省 256×3**（旧实现缺省 0 ⇒ 盒退化成点），见 [`emitter_distancemax`]。
    #[serde(default)]
    pub distancemin: serde_json::Value,
    #[serde(default)]
    pub distancemax: serde_json::Value,
    /// 发射器级**初速**（`velocity = normalize(局部偏移) × random(speedmin, speedmax)`，
    /// OWE `ParticleEmitter.cpp:203/259`、linux-WE `CParticle.cpp:632-640` 两份参照都这么做）。
    /// 旧代码**根本没有这两个字段** ⇒ 只有发射器给速的预设（官方 `spritetrail` 预览场景
    /// `speedmin 400 / speedmax 500`，`magic_vortex_*` 的 `128`）粒子一动不动。
    /// `WE_PARTICLE_EMITTER_SPEED=0` 回旧行为。
    #[serde(default, deserialize_with = "de_f32_lenient")]
    pub speedmin: f32,
    #[serde(default, deserialize_with = "de_f32_lenient")]
    pub speedmax: f32,
    #[serde(default = "default_emitter_rate", deserialize_with = "de_f32_lenient")]
    pub rate: f32,
    pub sign: String,
    /// 音频响应（OWE `ParticleAudioResponse`，`ParticleRuntime.cppm:40-46` + 用在
    /// `ParticleEmitter.cpp:181/240`）：`audioprocessingmode != 0` ⇒ 开；发射率 ×
    /// `max(1 + level^exponent × amount, 0)`，`level` = 频段均值按 `audiobounds` 归一化后夹到 0..1。
    /// **没有音频频谱时该函数提前返回 1.0（恒等）** ⇒ 本机（CLI 与内核都没喂频谱）默认一个像素都不改；
    /// `WE_PARTICLE_AUDIO=<0..1>` 是给它喂一个常数频谱的探针（默认 0 = 无音频）。
    #[serde(default, deserialize_with = "de_f32_lenient")]
    pub audioprocessingmode: f32,
    #[serde(default, deserialize_with = "de_f32_lenient")]
    pub audioamount: f32,
    #[serde(default, deserialize_with = "de_f32_lenient")]
    pub audioexponent: f32,
    #[serde(default, deserialize_with = "de_string_lenient")]
    pub audiobounds: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    pub audiofrequency: String,
}

/// 预设/对象级的**控制点**（`controlpoint` 数组）。WE 的控制点有 8 个槽位，`id` 是槽号；
/// `offset` 是**相对粒子对象原点**的局部偏移（linux-WE `CParticle.cpp:147-163`：非
/// `linkMouse`/非 `worldSpace` 时 `position = cp.offset`；场景坐标 = 对象原点 + 该偏移）。
/// 本渲染器不做骨骼/鼠标绑定（`flags` 位 1/2 解析但不用），因为本机 17 条里
/// 用到控制点的都是 offset 全 0 的静态点。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ControlPoint {
    #[serde(default, deserialize_with = "de_f32_lenient")]
    pub id: f32,
    /// ⚠ 本机实测这个键出现 **`null`** ⇒ 必须宽容解析（`null` → 空串 = 偏移 0）。
    #[serde(default, deserialize_with = "de_string_lenient")]
    pub offset: String,
    #[serde(default, deserialize_with = "de_f32_lenient")]
    pub flags: f32,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Init {
    pub name: String,
    pub min: serde_json::Value,
    pub max: serde_json::Value,
    /// `turbulentvelocityrandom` 的速度区间 / `mapsequencearoundcontrolpoint` 的**向量**速度区间。
    /// ⚠ 存成 `Value` 而不是 `f32`：后者的 `speedmin`/`speedmax` 是 **vec3**（`"0 10 0"`），
    /// 用标量反序列化会把向量串截成第一个分量（0）⇒ 速度全丢。标量取值走 `num_of(..)`，
    /// 与旧的 `de_f32_lenient`（取首个分量）逐位同值 ⇒ 对已有场景无行为变化。
    pub speedmin: serde_json::Value,
    pub speedmax: serde_json::Value,
    #[serde(default, deserialize_with = "de_f32_lenient")]
    pub scale: f32,
    /// `rotationrandom` / `angularvelocityrandom` / `velocityrandom` 用：
    /// `random = rand^exponent` 再 lerp(min,max)（OWE `GenRandom`，`ParticleParser.cpp:34-38`）。
    #[serde(default, deserialize_with = "de_f32_lenient")]
    pub exponent: f32,
    // ── `mapsequencearoundcontrolpoint`（WE 编辑器文案：'Place each particle around a certain
    // control point in sequence.'；字段名/默认值见 OWE `ParticleParser.cpp:76-135` +
    // linux-WE `ObjectParser.cpp:668-672`）──
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub controlpoint: Option<f32>,
    /// 环上均匀分布的槽位数（WE 里可以是小数，如 3.02 / 2.2；OWE 用 `count` 当除数）。
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub count: Option<f32>,
    /// 环绕角度的区间（比例，默认 `0 1` ⇒ 一整圈）。
    #[serde(default, deserialize_with = "de_string_lenient")]
    pub bounds: String,
    /// 环绕轴（默认 `0 0 1`）。
    #[serde(default, deserialize_with = "de_string_lenient")]
    pub axis: String,
    // ── 音频响应（与 emitter 同名字段；本机用在 `turbulentvelocityrandom` 上）──
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub audioprocessingmode: Option<f32>,
    #[serde(default, deserialize_with = "de_string_lenient")]
    pub audioprocessingbounds: String,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub audioprocessingexponent: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub audioprocessingfrequencystart: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub audioprocessingfrequencyend: Option<f32>,
    /// `turbulentvelocityrandom` 的 `offset`（噪声场相位偏移，OWE `TurbulentRandom.offset`
    /// 默认 0）。⚠ 本机实测这个键是**浮点**（`ember_small.json` 的 `-0.5`、
    /// `leaves5.json` 的 `3`）⇒ 必须宽容解析成数字，声明成字符串会让整份预设解析失败。
    #[serde(default, deserialize_with = "de_f32_lenient")]
    pub offset: f32,
    // ── `turbulentvelocityrandom` 的其余字段（此前**整组都没有** ⇒ 初速恒 0）
    // `turbulentvelocityrandom` 那一臂的长注释）──
    /// 方向基：`forward` = "噪声锥"的中心方向，默认 **`0 1 0`（向上）**
    /// （OWE `TurbulentRandom.forward {0,1,0}`；linux-WE `ObjectParser.cpp:664` 同）。
    #[serde(default, deserialize_with = "de_string_lenient")]
    pub forward: String,
    /// 旋转轴。WE 的键名**两个**：`right`（OWE `ReadFromJson` 把它写进 `normal`；
    /// linux-WE 用 `it.user("right", …)`）与 `normal`（OWE 也认）。默认 **`0 0 1`**。
    #[serde(default, deserialize_with = "de_string_lenient")]
    pub right: String,
    #[serde(default, deserialize_with = "de_string_lenient")]
    pub normal: String,
    /// 噪声场的推进速度（OWE 默认 1）：每 spawn 把采样点沿方向推 `0.005 × timescale`。
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub timescale: Option<f32>,
    /// 每 spawn 抽的相位区间（OWE 默认 `0 .. 0.1`）。
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub phasemin: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub phasemax: Option<f32>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Operator {
    pub name: String,
    pub gravity: String,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub drag: Option<f32>,
    /// `angularmovement` 的角加速度（"x y z"，WE 默认 0）。
    pub force: String,
    #[serde(default, deserialize_with = "de_f32_lenient")]
    pub fadeintime: f32,
    #[serde(default, deserialize_with = "de_f32_lenient")]
    pub fadeouttime: f32,
    // ⚠ 这一组字段被 `oscillateposition` / `oscillatealpha` 共用（WE 的 JSON 键名一样），
    // 但两个算子的**默认值不同**（WE 的 `oscillateposition.frequencymax=5`、
    // `oscillatealpha.frequencymax=10`）⇒ 这里存 `Option`，由各算子自己决定兜底
    // （`null`/缺省 与 显式的 0 在 WE 里语义不同，不能一律当 0）。
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub frequencymin: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub frequencymax: Option<f32>,
    pub mask: String,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub scalemin: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub scalemax: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub phasemin: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub phasemax: Option<f32>,
    /// `turbulence` 的噪声尺度（OWE 默认 0.01）。
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub scale: Option<f32>,
    /// `turbulence` 的噪声场变化速度（OWE 默认 20）。
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub timescale: Option<f32>,
    /// `turbulence` 的湍流速度区间（OWE 默认 500/1000）。
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub speedmin: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub speedmax: Option<f32>,
    // ── `colorchange` / `sizechange` 用（WE 里同一字段既可能是数字也可能是 "r g b" 串 / 数字串）──
    #[serde(default)]
    pub startvalue: Option<serde_json::Value>,
    #[serde(default)]
    pub endvalue: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub starttime: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub endtime: Option<f32>,
    // ══ 本次新增：`controlpointattract` / `vortex` / `boids` / `collisionquad` /
    // `oscillatesize`（后者的字段与 `oscillatealpha` 完全同名，已在上面）══════════
    /// 这些算子共用的控制点选择器（`controlpoint`，缺省 0）。
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub controlpoint: Option<f32>,
    /// `flags` 位域，逐算子含义不同（见各实现注释）。**注意 `boids`/`vortex` 都用它**。
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub flags: Option<f32>,
    /// `controlpointattract` 的作用半径（WE/OWE 默认 512，linux-WE 默认 1000）。
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub threshold: Option<f32>,
    /// `controlpointattract` 的偏移（相对控制点中心，默认 (0,0,0)）。
    #[serde(default, deserialize_with = "de_string_lenient")]
    pub origin: String,
    /// `vortex` 的旋转轴（默认 (0,0,1)）。
    #[serde(default, deserialize_with = "de_string_lenient")]
    pub axis: String,
    /// `vortex` 的偏移（相对控制点中心）。⚠ 与 `emitter.origin` 不是一回事。
    #[serde(default, deserialize_with = "de_string_lenient")]
    pub offset: String,
    /// `vortex` 的速度环带（`distanceinner`/`distanceouter` 之间线性插值 `speedinner→speedouter`）。
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub distanceinner: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub distanceouter: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub speedinner: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub speedouter: Option<f32>,
    /// `vortex` 的环带模式（`flags & 4`）参数。
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub ringradius: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub ringwidth: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub ringpulldistance: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub ringpullforce: Option<f32>,
    /// `vortex` 的向心力（`flags & 2` = maintain distance）。
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub centerforce: Option<f32>,
    // ── `boids`（WE 编辑器文案：'Groups close particles and matches their velocity while still
    // keeping a minimum distance between particles.'）──
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub alignmentfactor: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub cohesionfactor: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub separationfactor: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub neighborthreshold: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub separationthreshold: Option<f32>,
    // ── `collisionquad`（WE 编辑器文案 'Collision rectangle'：'Collides particles against a
    // rectangle of limited size and only from one side.'）──
    /// 矩形尺寸 `"w h"`（场景单位）。
    #[serde(default, deserialize_with = "de_string_lenient")]
    pub size: String,
    /// `"slide"` / `"bounce"` / `"stop"` / `"kill"`（WE 编辑器的 Collision behavior）。
    #[serde(default, deserialize_with = "de_string_opt_lenient")]
    pub collisionbehavior: Option<String>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub stoprotationoncollision: Option<f32>,
    // ── 音频响应（`turbulence` / `vortex` 的 `audioprocessing*`；linux-WE
    // `ObjectParser.cpp:713-730` 逐个给出默认值）──
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub audioprocessingmode: Option<f32>,
    #[serde(default, deserialize_with = "de_string_lenient")]
    pub audioprocessingbounds: String,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub audioprocessingexponent: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub audioprocessingfrequencystart: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub audioprocessingfrequencyend: Option<f32>,
    /// `controlpointattract` 的 `blendinstart`（本机 0 处出现；语义见 [`Blend`]）。
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub blendinstart: Option<f32>,
    /// 本次补齐：算子 blend-in/out 的**另外三个**字段（WE 编辑器 Blend-in end /
    /// Blend-out start / Blend-out end，见 [`Blend`]）。以前只解析了 `blendinstart`。
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub blendinend: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub blendoutstart: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub blendoutend: Option<f32>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Renderer {
    pub name: String,
    #[serde(default, deserialize_with = "de_f32_lenient")]
    pub length: f32,
    #[serde(default, deserialize_with = "de_f32_lenient")]
    pub maxlength: f32,
    /// `spritetrail` 的拖尾最短长度（WE/linux-WE 默认 0）。
    #[serde(default, deserialize_with = "de_f32_lenient")]
    pub minlength: f32,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ParticlePreset {
    pub emitter: Vec<Emitter>,
    pub initializer: Vec<Init>,
    pub operator: Vec<Operator>,
    pub renderer: Vec<Renderer>,
    /// 预设级控制点（最多 8 个槽位，按 `id` 索引）。
    #[serde(default, deserialize_with = "de_cp_list")]
    pub controlpoint: Vec<ControlPoint>,
    pub material: String,
    #[serde(default, deserialize_with = "de_f32_lenient")]
    pub maxcount: f32,
    #[serde(default, deserialize_with = "de_f32_lenient")]
    pub sequencemultiplier: f32,
    #[serde(default, deserialize_with = "de_f32_lenient")]
    pub starttime: f32,
    /// `"randomframe"` / `"sequence"` / null（WE 的精灵表播放模式）。
    #[serde(default, deserialize_with = "de_string_opt_lenient")]
    pub animationmode: Option<String>,
    /// 预设级 `flags`。**位 2（值 4）= 3D 粒子**：`flags & 4 == 0` 时粒子是 2D 的，
    /// 参照实现会把方向投影到 XY 平面再归一化（linux-WE `CParticle.cpp:903-910`）。
    /// 本机 17 条全部 `flags: 0`（= 2D），但这条语义影响初速方向，实现里按数据判断。
    #[serde(default, deserialize_with = "de_f32_lenient")]
    pub flags: f32,
}

impl ParticlePreset {
    /// WE 的 `spritetrail` 渲染器参数：`(length, maxlength, minlength)`；
    /// `None` = 不是拖尾渲染器（= 普通 `sprite` billboard）。
    /// 默认值照 linux-WE `ObjectParser::parseParticleRenderer`（length 0.05 / maxlength 10 / minlength 0）。
    pub fn trail(&self) -> Option<[f32; 3]> {
        let r = self.renderer.first()?;
        if r.name != "spritetrail" {
            return None;
        }
        Some([
            if r.length != 0.0 { r.length } else { 0.05 },
            if r.maxlength != 0.0 { r.maxlength } else { 10.0 },
            r.minlength,
        ])
    }

    /// `animationmode == "randomframe"` ⇒ 每颗粒子固定在一帧（OWE `ParticleAnimationMode::RANDOMONE`）。
    pub fn random_frame(&self) -> bool {
        self.animationmode.as_deref() == Some("randomframe")
    }
}

/// 场景对这个粒子对象做的覆盖（`instanceoverride`）
/// ⚠ **每个字段都可能是绑定对象** `{"user":"<属性>","value":…}`（OWE `ParticleInstanceoverride`
/// 对 alpha/count/lifetime/rate/speed/size/brightness/color/colorn **逐个**支持绑定，
/// 见 `ParticleObject.cppm:162-196`）。所以这里一律走宽容反序列化：类型不匹配不再是硬错
/// 否则「一个字段是对象」= 整个 override 丢光（`2080534540` 的雨就是这样：
/// 它的 `count` 是 `{"user":"_6","value":0.95}`，于是作者给的 `size 0.68 / speed 0.78`
/// 一条都没生效）。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct InstanceOverride {
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub alpha: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub rate: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub count: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub lifetime: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub size: Option<f32>,
    #[serde(default, deserialize_with = "de_f32_opt_lenient")]
    pub speed: Option<f32>,
    /// 颜色乘子，形如 `"0.62 0.89 0.95"`（也可能是绑定对象 / 三元数组）。
    #[serde(default, deserialize_with = "de_string_opt_lenient")]
    pub colorn: Option<String>,
    // ── 本次：对象级**控制点覆盖** `controlpoint0`..`controlpoint7` ─────────────────
    /// WE 编辑器里每个粒子对象实例都能单独拖 8 个控制点的坐标，写进
    /// `instanceoverride.controlpointN`（本机 `1931934283` 6 处、`2861113973` 2 处、
    /// `3571777169` 4 处）。语义：见 [`ParticleSystem::new`]（OWE
    /// `ParticleRuntime.cpp:605-631` 的 `UpdateControlpoints`）。
    #[serde(default, rename = "controlpoint0", deserialize_with = "de_vec3_opt_lenient")]
    pub controlpoint0: Option<[f32; 3]>,
    #[serde(default, rename = "controlpoint1", deserialize_with = "de_vec3_opt_lenient")]
    pub controlpoint1: Option<[f32; 3]>,
    #[serde(default, rename = "controlpoint2", deserialize_with = "de_vec3_opt_lenient")]
    pub controlpoint2: Option<[f32; 3]>,
    #[serde(default, rename = "controlpoint3", deserialize_with = "de_vec3_opt_lenient")]
    pub controlpoint3: Option<[f32; 3]>,
    #[serde(default, rename = "controlpoint4", deserialize_with = "de_vec3_opt_lenient")]
    pub controlpoint4: Option<[f32; 3]>,
    #[serde(default, rename = "controlpoint5", deserialize_with = "de_vec3_opt_lenient")]
    pub controlpoint5: Option<[f32; 3]>,
    #[serde(default, rename = "controlpoint6", deserialize_with = "de_vec3_opt_lenient")]
    pub controlpoint6: Option<[f32; 3]>,
    #[serde(default, rename = "controlpoint7", deserialize_with = "de_vec3_opt_lenient")]
    pub controlpoint7: Option<[f32; 3]>,
}

impl InstanceOverride {
    /// 第 `i`（0..8）号槽位的对象级控制点覆盖。
    pub fn cp_override(&self, i: usize) -> Option<[f32; 3]> {
        match i {
            0 => self.controlpoint0,
            1 => self.controlpoint1,
            2 => self.controlpoint2,
            3 => self.controlpoint3,
            4 => self.controlpoint4,
            5 => self.controlpoint5,
            6 => self.controlpoint6,
            7 => self.controlpoint7,
            _ => None,
        }
    }
    /// 任意一个槽位有覆盖？
    pub fn has_cp_override(&self) -> bool {
        (0..8).any(|i| self.cp_override(i).is_some())
    }
}


fn v3(s: &str, default: [f32; 3]) -> [f32; 3] {
    let p: Vec<f32> = s.split_whitespace().filter_map(|x| x.parse().ok()).collect();
    if p.len() >= 3 {
        [p[0], p[1], p[2]]
    } else {
        default
    }
}

fn v2f(v: &serde_json::Value, default: [f32; 2]) -> [f32; 2] {
    match v {
        serde_json::Value::Number(n) => {
            let x = n.as_f64().unwrap_or(0.0) as f32;
            [x, x]
        }
        serde_json::Value::String(s) => {
            let p: Vec<f32> = s.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            if p.len() >= 2 {
                [p[0], p[1]]
            } else if p.len() == 1 {
                [p[0], p[0]]
            } else {
                default
            }
        }
        _ => default,
    }
}

fn v3j(v: &serde_json::Value, default: [f32; 3]) -> [f32; 3] {
    match v {
        serde_json::Value::Number(n) => {
            let x = n.as_f64().unwrap_or(0.0) as f32;
            [x, x, x]
        }
        serde_json::Value::String(s) => v3(s, default),
        _ => default,
    }
}

/// 极简随机（xorshift）—— 确定性，单测才好写
#[derive(Clone)]
struct Rng(u32);

// ── Perlin 噪声 + curl noise（`turbulence` 算子的地基）──────────────────────────
// 出处：`linux-wallpaperengine/src/WallpaperEngine/Render/Utils/NoiseUtils.h`
// （标准 Ken Perlin 的 perm/grad + 用三个不同 offset 采样成 vec3 + 中心差分求 curl）。
// WE 自己的噪声是引擎内部的 ⇒ 照参照实现**同一套**公式与同一张 perm 表照抄，
// 保证噪声的量级与形态一致（这台机器上没法拿 WE 逐位对比噪声）。
const PERLIN_PERM: [u8; 256] = [
    151, 160, 137, 91, 90, 15, 131, 13, 201, 95, 96, 53, 194, 233, 7, 225,
    140, 36, 103, 30, 69, 142, 8, 99, 37, 240, 21, 10, 23, 190, 6, 148,
    247, 120, 234, 75, 0, 26, 197, 62, 94, 252, 219, 203, 117, 35, 11, 32,
    57, 177, 33, 88, 237, 149, 56, 87, 174, 20, 125, 136, 171, 168, 68, 175,
    74, 165, 71, 134, 139, 48, 27, 166, 77, 146, 158, 231, 83, 111, 229, 122,
    60, 211, 133, 230, 220, 105, 92, 41, 55, 46, 245, 40, 244, 102, 143, 54,
    65, 25, 63, 161, 1, 216, 80, 73, 209, 76, 132, 187, 208, 89, 18, 169,
    200, 196, 135, 130, 116, 188, 159, 86, 164, 100, 109, 198, 173, 186, 3, 64,
    52, 217, 226, 250, 124, 123, 5, 202, 38, 147, 118, 126, 255, 82, 85, 212,
    207, 206, 59, 227, 47, 16, 58, 17, 182, 189, 28, 42, 223, 183, 170, 213,
    119, 248, 152, 2, 44, 154, 163, 70, 221, 153, 101, 155, 167, 43, 172, 9,
    129, 22, 39, 253, 19, 98, 108, 110, 79, 113, 224, 232, 178, 185, 112, 104,
    218, 246, 97, 228, 251, 34, 242, 193, 238, 210, 144, 12, 191, 179, 162, 241,
    81, 51, 145, 235, 249, 14, 239, 107, 49, 192, 214, 31, 181, 199, 106, 157,
    184, 84, 204, 176, 115, 121, 50, 45, 127, 4, 150, 254, 138, 236, 205, 93,
    222, 114, 67, 29, 24, 72, 243, 141, 128, 195, 78, 66, 215, 61, 156, 180,
];

/// C++ 那张表是 512 项（256 重复一遍）⇒ 这里统一用 `& 255` 取，等价。
#[inline]
fn perm_at(i: usize) -> usize {
    PERLIN_PERM[i & 255] as usize
}

#[inline]
fn perlin_grad(hash: u8, x: f32, y: f32, z: f32) -> f32 {
    match hash & 0xF {
        0x0 => x + y,
        0x1 => -x + y,
        0x2 => x - y,
        0x3 => -x - y,
        0x4 => x + z,
        0x5 => -x + z,
        0x6 => x - z,
        0x7 => -x - z,
        0x8 => y + z,
        0x9 => -y + z,
        0xA => y - z,
        0xB => -y - z,
        0xC => y + x,
        0xD => -y + z,
        0xE => y - x,
        _ => -y - z, // 0xF（C++ 里 0xD/0xF 与 0x9/0xB 重复，照抄）
    }
}

#[inline]
fn perlin_ease(t: f32) -> f32 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

#[inline]
fn lerpf(t: f32, a: f32, b: f32) -> f32 {
    a + t * (b - a)
}

#[inline]
fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

#[inline]
fn cross3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

#[inline]
fn len3(a: [f32; 3]) -> f32 {
    (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
}

/// 单位化；零向量（或近零）退回 `fallback`（参照实现都是这么处理的）。
#[inline]
fn unit3(v: [f32; 3], fallback: [f32; 3]) -> [f32; 3] {
    let l = len3(v);
    if l > 1e-6 {
        [v[0] / l, v[1] / l, v[2] / l]
    } else {
        fallback
    }
}

/// 绕过 `axis`（**单位**向量）旋转 `v` 角 `a` 弧度（Rodrigues 公式）。
/// `turbulentvelocityrandom` 的初速方向就是 `AngleAxis(angle*scale*0.5 + offset, normal) × forward`
/// （OWE `ParticleParser.cpp:343`；linux-WE `CParticle.cpp:891-900` 同型）。
#[inline]
fn rot_about(v: [f32; 3], axis: [f32; 3], a: f32) -> [f32; 3] {
    let (s, c) = a.sin_cos();
    let k = cross3(axis, v);
    let d = dot3(axis, v);
    [
        v[0] * c + k[0] * s + axis[0] * d * (1.0 - c),
        v[1] * c + k[1] * s + axis[1] * d * (1.0 - c),
        v[2] * c + k[2] * s + axis[2] * d * (1.0 - c),
    ]
}

/// `turbulentvelocityrandom` 的速度区间**缺省**（两份参照**逐值一致**）：
/// linux-WE `ObjectParser.cpp:660-661` = `it.user("speedmin", …, 100.0f)` / `250.0f`；
/// OWE `ParticleParser.cpp:212-220` = `TurbulentRandom{speedmin 100.0f, speedmax 250.0f}`。
/// ⚠ 缺省的 100..250 与**显式写 `0`** 语义完全不同（后者是"没有初速"）——
/// 这正是 `ember.json` / `ember_small.json` / `Stars_copy1.json` 等预设里那个键
/// **整组不写**的原因，也是"Ember 一层一个像素都看不见"的真根因（见 `spawn` 那一臂）。
const TVR_SPEEDMIN_DEFAULT: f32 = 100.0;
const TVR_SPEEDMAX_DEFAULT: f32 = 250.0;

/// `vortex` 的切向速度随半径的插值（linux-WE `CParticle.cpp:1392-1431`；
/// OWE `VortexSpeedAtDistance`（`ParticleParser.cpp:775-781`）逐项等价，只有 `+0.1` 的位置一样）。
#[inline]
fn vortex_speed_cfg(v: &VortexCfg, distance: f32) -> f32 {
    let mid = v.distance_outer - v.distance_inner + 0.1;
    if mid < 0.0 || distance < v.distance_inner {
        v.speed_inner
    } else if distance > v.distance_outer {
        v.speed_outer
    } else {
        lerpf((distance - v.distance_inner) / mid, v.speed_inner, v.speed_outer)
    }
}

/// 标准 Perlin 噪声（`NoiseUtils.h:81`）。
fn perlin_noise(x: f32, y: f32, z: f32) -> f32 {
    let fx = x.floor();
    let fy = y.floor();
    let fz = z.floor();
    let xi = (fx as i32 & 255) as usize;
    let yi = (fy as i32 & 255) as usize;
    let zi = (fz as i32 & 255) as usize;

    let x = x - fx;
    let y = y - fy;
    let z = z - fz;

    let u = perlin_ease(x);
    let v = perlin_ease(y);
    let w = perlin_ease(z);

    let a = perm_at(xi) + yi;
    let aa = perm_at(a) + zi;
    let ab = perm_at(a + 1) + zi;
    let b = perm_at(xi + 1) + yi;
    let ba = perm_at(b) + zi;
    let bb = perm_at(b + 1) + zi;

    lerpf(
        w,
        lerpf(
            v,
            lerpf(
                u,
                perlin_grad(PERLIN_PERM[aa & 255], x, y, z),
                perlin_grad(PERLIN_PERM[ba & 255], x - 1.0, y, z),
            ),
            lerpf(
                u,
                perlin_grad(PERLIN_PERM[ab & 255], x, y - 1.0, z),
                perlin_grad(PERLIN_PERM[bb & 255], x - 1.0, y - 1.0, z),
            ),
        ),
        lerpf(
            v,
            lerpf(
                u,
                perlin_grad(PERLIN_PERM[(aa + 1) & 255], x, y, z - 1.0),
                perlin_grad(PERLIN_PERM[(ba + 1) & 255], x - 1.0, y, z - 1.0),
            ),
            lerpf(
                u,
                perlin_grad(PERLIN_PERM[(ab + 1) & 255], x, y - 1.0, z - 1.0),
                perlin_grad(PERLIN_PERM[(bb + 1) & 255], x - 1.0, y - 1.0, z - 1.0),
            ),
        ),
    )
}

/// 三个不同 offset 的各向异性采样（`NoiseUtils.h:121`）。
fn perlin_noise_vec3(p: [f32; 3]) -> [f32; 3] {
    [
        perlin_noise(p[0], p[1], p[2]),
        perlin_noise(p[0] + 89.2, p[1] + 33.1, p[2] + 57.3),
        perlin_noise(p[0] + 100.3, p[1] + 120.1, p[2] + 142.2),
    ]
}

/// curl noise（`NoiseUtils.h:130`）：中心差分，返回**未归一化**的旋度（调用方按 WE 归一化再乘速度）。
fn curl_noise(p: [f32; 3]) -> [f32; 3] {
    let e = 1e-4f32;
    let off = |d: [f32; 3], s: f32| {
        [p[0] + d[0] * s, p[1] + d[1] * s, p[2] + d[2] * s]
    };
    let (dx, dy, dz) = ([e, 0.0, 0.0], [0.0, e, 0.0], [0.0, 0.0, e]);
    let x0 = perlin_noise_vec3(off(dx, -1.0));
    let x1 = perlin_noise_vec3(off(dx, 1.0));
    let y0 = perlin_noise_vec3(off(dy, -1.0));
    let y1 = perlin_noise_vec3(off(dy, 1.0));
    let z0 = perlin_noise_vec3(off(dz, -1.0));
    let z1 = perlin_noise_vec3(off(dz, 1.0));
    let cx = (y1[2] - y0[2]) - (z1[1] - z0[1]);
    let cy = (z1[0] - z0[0]) - (x1[2] - x0[2]);
    let cz = (x1[1] - x0[1]) - (y1[0] - y0[0]);
    [cx / (2.0 * e), cy / (2.0 * e), cz / (2.0 * e)]
}

impl Rng {
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }
    fn f(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 / (1u32 << 24) as f32
    }
    fn range(&mut self, a: f32, b: f32) -> f32 {
        a + (b - a) * self.f()
    }
}

#[derive(Debug, Clone, Copy)]
struct VortexCfg {
    cp: usize,
    flags: u32,
    axis: [f32; 3],
    offset: [f32; 3],
    distance_inner: f32,
    distance_outer: f32,
    speed_inner: f32,
    speed_outer: f32,
    center_force: f32,
    ring_radius: f32,
    ring_width: f32,
    ring_pull_distance: f32,
    ring_pull_force: f32,
    audio_mode: u32,
}

/// `controlpointattract`：按到控制点的距离给一个恒定的向心/离心冲量。
/// 依据：OWE `ParticleParser.cpp:1352-1384`（`ControlPointAttractOperator`）——
/// `difference = center - pos`；`distance >= threshold` 直接跳过；
/// `impulse = scale × dt × (1 - distance/threshold)`；`flags & 2` ⇒ `impulse = min(distance, impulse)`
/// （OWE 原文注释：'WE limits the velocity increment near the control point by default'）；
/// `vel += difference × (impulse/distance)`。`scale` 为负 ⇒ 排斥。
/// 默认值：OWE `ControlPointForce`（`ParticleParser.cpp:783-807`）= controlpoint 0、flags 2、
/// scale 512、threshold 512、origin (0,0,0)；linux-WE `ObjectParser.cpp:732-736` 给的是
/// scale 100、threshold 1000（**两派不一致，仅影响没写字段的预设**；本机 6 处用到它的预设
/// 都显式写了 scale/threshold）。我们取 OWE 的一套（它有 `flags & 2` 的钳制语义）。
/// 第二种实现（linux-WE `CParticle.cpp:1449-1494`）用 `threshold/2` 且没有距离衰减 ——
/// 两者在 `threshold=64` 时行为差别明显，这里选 OWE 并记录分歧。
#[derive(Debug, Clone, Copy)]
struct AttractCfg {
    cp: usize,
    origin: [f32; 3],
    scale: f32,
    threshold: f32,
    flags: u32,
}

/// `boids`：Reynolds 三条规则（对齐/聚集/分离）。
/// 依据强度：**弱**。两份参照实现都**没有**实现它（OWE `ParticleParser.cpp:1391-1509` 的
/// operator 分派里没有 boids；linux-WE `CParticle.cpp:971-1011` 同样没有），所以语义只能取
/// ① WE 编辑器官方文案（`locale/ui_en-us.json`，**逐字**）：
/// `ui_editor_particle_element_operator_boids` = "Boids"、
/// `ui_editor_particle_element_operator_boids_description` =
/// "Groups close particles and matches their velocity while still keeping a minimum distance
/// between particles."；
/// ② 字段名（编辑器标签逐字：`alignment_factor`="Alignment factor"、
/// `cohesion_factor`="Cohesion factor"、`separation_factor`="Separation factor"、
/// `neighbor_threshold`="Neighbor threshold"、`separation_threshold`="Separation threshold"；
/// 数据里出现于本机 `dripping_water_single` 与官方预览场景）；
/// ③ WE 主程序里的 JSON 键表（`wallpaper64.exe` @0x48FBF0 一带的 `separationthreshold` /
/// `neighborthreshold` / `separationfactor` / `alignmentfactor` / `cohesionfactor`）——
/// 注意同一张表里紧邻的 `maxspeed` 属 **`capvelocity`**（官方
/// `assets/scenes/particleelementpreviews/capvelocity` 实测），**不是** boids 的字段。
/// 采用标准写法：对半径内的邻居分别做
/// `v += (mean(neighbor.v) - v) × alignment × dt`（对齐）、
/// `v += (mean(neighbor.pos) - pos) × cohesion × dt`（聚集）、
/// `v -= (neighbor.pos - pos)/d × separation × dt`（分离，只算比 `separationthreshold` 近的）。
/// 三处都是**加在速度上**（不直接改位置），位置由同一帧末尾的 `pos += v·dt` 积分 ⇒
/// "matches their velocity" 字面成立、"keeping a minimum distance" 由分离项实现。
/// 邻居用**上一帧快照**（同帧内互不影响，顺序无关）。
/// 缺省：三个 factor 0、`neighborthreshold` 0 ⇒ **不生效**（保守：没有依据就不猜一个"好看的"力）。
#[derive(Debug, Clone, Copy)]
struct BoidsCfg {
    alignment: f32,
    cohesion: f32,
    separation: f32,
    neighbour_threshold: f32,
    separation_threshold: f32,
}

/// `WE_DEBUG_BOIDS=1` 的诊断快照（**只读**，不参与物理）。用来离线/在线回答
/// "这组参数下邻居到底有没有出现过" —— 即"0 变化"是**数据**的事还是**实现**的事。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoidsStats {
    /// 本帧存活粒子数。
    pub alive: usize,
    /// 本帧 `neighborthreshold` 内**至少有一个邻居**的粒子数（= 对齐/聚集两项真正作用的粒子数）。
    pub with_neighbour: usize,
    /// 本帧 `separationthreshold` 内至少有一个邻居的粒子数（= 分离项真正作用的粒子数）。
    pub with_separation: usize,
    /// 本帧每个粒子看到的邻居数：最大 / 平均。
    pub max_neighbours: usize,
    pub mean_neighbours: f32,
    /// 两两距离（场景单位）的最小 / 平均 / 最大；粒子 < 2 时全是 0。
    pub min_pair_dist: f32,
    pub mean_pair_dist: f32,
    pub max_pair_dist: f32,
    /// 速度模的均值 / 最大（场景单位每秒）。
    pub mean_speed: f32,
    pub max_speed: f32,
    /// "同向度" = |Σv| / Σ|v| ∈ [0,1]（对齐项该把它推向 1）。
    pub vel_coherence: f32,
    /// 相对质心的均方根半径（聚集项该把它压小）。
    pub rms_radius: f32,
    /// 平均**向心**速度分量（正 = 在往质心靠；聚集项该给正值）。
    pub mean_inward_speed: f32,
    /// 本帧的实际阈值（打印出来才能确认打印的"邻居数"是按哪个半径算的）。
    pub neighbour_threshold: f32,
    pub separation_threshold: f32,
}

/// `collisionquad`：一块**有限大小的矩形**、**只从一面**碰撞粒子。
/// 依据强度：**弱**（同上，两份参照实现都没有 collisionquad）。
/// · WE 编辑器文案："Collision rectangle" —— "Collides particles against a rectangle of
/// limited size and only from one side."；
/// · 字段名（本机唯一一处，官方预览场景 `particleelementpreviews/collisionquad`）：
/// `size "100 100"`、`origin "0 -100 0"`、`collisionbehavior "slide"`；
/// WE 编辑器另有 `stoprotationoncollision` / `collisionmask`。
/// 实现取"正面（+y 侧）是一块平面矩形"这一读法：矩形中心 = 对象原点 + `origin`，
/// 尺寸 `size`；粒子在矩形 x 跨度内、从 **+y 侧**越过来时贴在矩形上表面（y = 顶边），
/// 行为 `slide`（切向速度归零）/ `bounce`（法向速度反号）/ `stop`（速度全归零）/
/// `kill`（标记消亡）。背面（-y 侧）穿过不碰。
#[derive(Debug, Clone, Copy)]
struct CollideCfg {
    center: [f32; 3],
    half: [f32; 2],
    behavior: u8, // 0 slide / 1 bounce / 2 stop / 3 kill
    stop_rot: bool,
}

/// `mapsequencearoundcontrolpoint`（**初始化器**）：把每颗粒子沿控制点周围的环按序号摆位。
/// 依据：OWE `ParticleParser.cpp:76-135`（`MapSequenceAroundControlPoint` / `...Program.Initialize`）——
/// `angle = TAU × (seq % count)/count`，再 `angle = angle×(bounds1-bounds0) + TAU×bounds0`；
/// `pos = center + parallel + radius×(cos(angle)×basis + sin(angle)×tangent)`（`basis` 由轴推导、
/// `tangent = basis × axis`）；速度 `+= AngleAxis(-angle, axis) × randVec(speedmin, speedmax)`。
/// 默认值：OWE 结构体 `controlpoint 0 / count 1 / bounds (0,1) / axis (0,0,1) / speed (0,0,0)`；
/// linux-WE `ObjectParser.cpp:668-672` 的 `speedmax` 默认是 `(0,0,100)`（不同）。
/// 第二种实现（linux-WE `CParticle.cpp:919-967`）把位置直接设成控制点中心（丢掉半径）⇒ 明显
/// 不如 OWE 忠实，这里取 OWE。
#[derive(Debug, Clone, Copy)]
struct MapSeqCfg {
    cp: usize,
    count: f32,
    bounds: [f32; 2],
    axis: [f32; 3],
    speed_min: [f32; 3],
    speed_max: [f32; 3],
}

/// 音频响应（OWE `ParticleAudioResponse`，见 `ParticleEmitter.cpp:60-82`）。
/// `scale` 在**没有频谱时返回 1.0（恒等）**—— 这是 OWE 的原行为，也是本机默认。
/// 频谱来源见 [`crate::audio`]：`WE_AUDIO_SPECTRUM=1`（真频谱）或 `WE_AUDIO_BANDS=...`
/// （静态探针）；都不设 ⇒ 全 0 ⇒ 与不开音频逐位相同。
#[derive(Debug, Clone, Copy, Default)]
struct AudioCfg {
    enable: bool,
    amount: f32,
    exponent: f32,
    /// 段区间（`audiofrequencystart/end`）——**是 16 段数组的下标**，不是 Hz！
    /// 依据：OWE `ParticleAudioResponse` 默认 `frequency {0.0f, 15.0f}`
    /// （`ParticleRuntime.cppm:44-46`，数组长 16）＋ `AudioResponseScale` 用
    /// `round(value)` 当数组下标（`ParticleEmitter.cpp:62-69`）。
    /// 本机用到它的 3 条预设（`2481886649` 的 ember/torch）只写 `audiofrequencyend: 15`。
    freq: [f32; 2],
    bounds: [f32; 2],
}

impl AudioCfg {
    /// OWE `AudioResponseScale`（`ParticleEmitter.cpp:60-82`）的移植；`spec` = 16 段频谱。
    /// 没有音频（全 0）⇒ 1.0（恒等，绝不改变没有音频的场景）。
    fn scale(&self, spec: &crate::audio::Spectrum) -> f32 {
        if !self.enable || !spec.any() {
            return 1.0;
        }
        let band = spec.range_mean(self.freq);
        let low = self.bounds[0].min(self.bounds[1]);
        let high = self.bounds[0].max(self.bounds[1]);
        let mut level = band;
        if high > low {
            level = (band - low) / (high - low);
        }
        level = level.clamp(0.0, 1.0).powf(self.exponent.max(0.001));
        (1.0 + level * self.amount).max(0.0)
    }
}

/// `0` 当"字段没写"处理 —— 音频响应的 `amount`/`exponent` 在 WE 里缺省是 **1.0**
/// （OWE `ParticleObject.cppm:82-83`），而 serde 的默认是 0.0；`0 × level` 会让整条
/// 响应恒等于 1（实测 `2997301801` 的 emitter 正因此**曾经完全不动**）。
/// 数值 0 在这里没有别的语义（真要关就用 `audioprocessingmode:0`）。
fn none_if_zero(v: f32) -> Option<f32> {
    if v == 0.0 {
        None
    } else {
        Some(v)
    }
}

/// 从字段名解析音频响应（emitter 与 `turbulence` 共用同一套键名）。
/// `mode` 缺省 0 ⇒ 关；`freq` 缺省 (0,0)；`bounds` 缺省 (0,1)；`amount` 缺省 1；`exponent` 缺省 1。
/// （OWE `ParticleAudioResponse` 的默认：enable false / amount 1 / exponent 1 / freq (0,15) /
/// bounds (0,1)。linux-WE `ObjectParser.cpp:713-730` 的 `turbulence` 默认是
/// mode 0 / bounds (0,1) / exponent 1 / freq 0..15，与之一致。）
fn audio_cfg(mode: Option<f32>, amount: Option<f32>, exponent: Option<f32>, bounds: &str, freq: (Option<f32>, Option<f32>)) -> AudioCfg {
    AudioCfg {
        enable: mode.unwrap_or(0.0) != 0.0,
        amount: amount.unwrap_or(1.0),
        exponent: exponent.unwrap_or(1.0),
        freq: [freq.0.unwrap_or(0.0), freq.1.unwrap_or(15.0)],
        bounds: v2s(bounds, [0.0, 1.0]),
    }
}

/// 从 "a b" 里取 2 个标量（`_lenient`：数字串 / 单值复制 / 缺省）。
fn v2s(s: &str, default: [f32; 2]) -> [f32; 2] {
    let p: Vec<f32> = s.split_whitespace().filter_map(|x| x.parse().ok()).collect();
    match p.len() {
        0 => default,
        1 => [p[0], p[0]],
        _ => [p[0], p[1]],
    }
}

/// 控制点槽号（WE 只有 8 个槽；OWE `ParticleParser.cpp:736` 就是 `controlpoint %= 8`）。
fn cp_index(v: Option<f32>) -> usize {
    (v.unwrap_or(0.0).max(0.0) as i64 % 8) as usize
}

/// 频谱快照（真频谱或静态探针；见 [`crate::audio`]）。
/// 没开功能时恒为全 0 ⇒ 所有音频响应恒等（OWE/linux-WE 的"没频谱就什么都不做"语义）。
fn audio_now() -> crate::audio::Spectrum {
    crate::audio::spectrum()
}

#[derive(Debug, Clone, Copy)]
pub struct Particle {
    pub pos: [f32; 3],
    pub vel: [f32; 3],
    pub age: f32,
    pub life: f32,
    pub size: f32,
    pub color: [f32; 3],
    /// 出生 alpha（`alpharandom` 初始化器）。**以前没有这个字段 ⇒ 默认 1.0（全不透明）**，
    /// 而 `Fog 1` 这类预设声明的就是 `alpharandom 0.15~0.2` ⇒ 12 片不透明大白雾糊满屏。
    pub alpha0: f32,
    pub phase: f32,
    pub freq: f32,
    /// 精灵绕三轴的旋转（`rotationrandom` 初始化器 + `angularmovement` 算子改它）。
    /// 布局与 WE 一致：顶点里 `rotation = (a_TexCoordC2.x, a_TexCoordC2.y, a_TexCoordVec4.z)`。
    pub rot: [f32; 3],
    /// 角速度（`angularvelocityrandom` 初始化器 + `angularmovement` 算子积分它）。
    pub ang_vel: [f32; 3],
    /// `oscillatealpha` 的每粒子频率/相位（照 OWE `GenFrequency` 在**出生时**抽一次）。
    pub alpha_freq: f32,
    pub alpha_phase: f32,
    /// 每粒子的固定随机数 [0,1)——`animationmode:"randomframe"` 用它把每颗粒子**钉在一帧**上。
    pub rand: f32,
    /// `oscillatesize` 的每粒子频率/相位（OWE `FrequencyValue::GenFrequency`，出生时抽一次；
    /// 与 `oscillatealpha` 同一套公式，只是乘在 size 上）。
    pub size_freq: f32,
    pub size_phase: f32,
}

/// 单个积分子步的上限（秒）。粒子的 `dt` 由主循环夹在 0.25 s 内，这里再把一步切成
/// ≤1/30 s 的子步 ⇒ 慢帧不丢时间、大步长也不至于过冲。取 1/30 而不是 1/60：
/// 离线的确定性口径本来就是 1/30，这样**那一支仍是一次子步**、像素逐字节不变。
const MAX_SUBSTEP: f32 = 1.0 / 30.0;

/// 子步判定的相对容差（见 `update` 里的说明：1/30 的墙钟差值会多出 1 ulp）。
const SUBSTEP_EPS: f32 = 1e-4;

pub struct ParticleSystem {
    pub preset: ParticlePreset,
    pub particles: Vec<Particle>,
    origin: [f32; 3],
    scale: [f32; 2],
    alpha: f32,
    rate_mul: f32,
    count_mul: f32,
    size_mul: f32,
    life_mul: f32,
    speed_mul: f32,
    color_mul: [f32; 3],
    spawn_acc: f32,
    time: f32,
    /// `preset.starttime`：延迟出场（秒）。
    start_delay: f32,
    rng: Rng,
    gravity: [f32; 3],
    drag: f32,
    fadein: f32,
    fadeout: f32,
    /// `sizechange` / `colorchange` 算子：`(starttime, endtime, startvalue, endvalue)`
    /// （时间都是**寿命比例** 0..1，与 WE 一致；同区间后一个算子覆盖前一个）。
    size_ops: Vec<(f32, f32, f32, f32)>,
    color_ops: Vec<(f32, f32, [f32; 3], [f32; 3])>,
    osc: Option<(f32, f32, [f32; 3], f32, f32)>,
    /// `oscillatealpha`：`(freqmin, freqmax, scalemin, scalemax, phasemin, phasemax)`。
    /// 作用在**alphafade 之后的 alpha** 上（参照实现把当帧 alpha 记成 base 再乘振荡系数）。
    osc_alpha: Option<[f32; 6]>,
    /// `turbulence`：`(scale, timescale, speed, phase, mask)`；phase/speed 是**每个算子抽一次**
    /// （不是每颗粒子），出处 OWE `TurbulenceOperator` 的 `.phase = Random::get(...)`。
    turb: Option<(f32, f32, f32, f32, [f32; 3])>,
    /// `angularmovement`：`(drag, force)`；速度倍率走 `speed_mul`（与 WE 一致）。
    angmov: Option<(f32, [f32; 3])>,
    /// `animationmode == "randomframe"` ⇒ 每颗粒子固定一帧。
    random_frame: bool,
    /// `sequencemultiplier`（<=0 时按 1 处理，对齐 linux-WE `animSpeed`）。
    seq_mult: f32,
    /// `sizerandom` 取值喂 shader 前是否 **÷2**（= 参照实现语义，默认 **是**）。
    /// 依据：
    /// · `linux-wallpaperengine` `CParticle::createSizeRandomInitializer`：
    /// `p.size = (min + t^e*(max-min)) * sizeOverride / 2.0f`；
    /// · `open-wallpaper-engine` `SizeRandomProgram`：`value.size`（= rand × sizeOverride，
    /// 见 `ParticleParser.cpp:364 columns.sizes *= modifiers.Size`）在写顶点时再 `* 0.5f`
    /// （`ParticleRawGenerator.cpp:398`）。
    /// 而 WE 官方顶点 shader `ComputeParticlePosition`（`common_particles.h:54-58`）把顶点里的
    /// size 当作**全宽**（`position + size*right*(uv-0.5) - ...` ⇒ 半宽 = size/2）。
    /// ⇒ 屏幕上的全宽 = `sizerandom × sizeOverride ÷ 2`。旧实现漏了那个 ÷2（块大一倍）。
    half_size: bool,
    /// emitter 局部偏移要过的**对象世界变换的上 3×3**（含 angles 旋转 + scale，列主序 `M[r][c]`）。
    /// `None` = 旧行为（偏移只乘 scale，不旋转）。见 lib.rs 的 `WE_PARTICLE_EMITTER_XFORM`。
    emit_xform: Option<[f32; 9]>,
    /// 精灵四边要过的**对象缩放**（`scale.x/scale.y`）+ 贴图高宽比。
    /// `Some([sx, sy])` ⇒ `build` 在 CPU 侧把「对象缩放 × 粒子自转」展开成**场景空间的真实四角**，
    /// 顶点里的 size 写 0（顶点 shader 的 `ComputeParticlePosition` 不再撑开）。
    /// 依据（三方一致）：WE 官方 `genericparticle.vert` 把 size 与中心点放进**同一个**
    /// `g_ModelViewProjectionMatrix`（`position = ComputeParticlePosition(uv, ratio, vec4(a_Position, size), …)`
    /// 之后才 `gl_Position = mul(vec4(position,1), MVP)`）；`linux-wallpaperengine`
    /// `CParticle::updateMatrices` 的模型矩阵 = `T(origin)·Rz(−z)·Ry(y)·Rx(−x)·S(scale)` 且
    /// `mvp = VP·model`；`open-wallpaper-engine` `SceneParticleObjectParser` 把粒子对象做成
    /// `SceneNode(origin, scale, angles)`（`RenderPosition` 只返回 `instance.position + local`）。
    /// ⇒ 精灵四边**与中心点过同一个变换**。旧实现只把 scale 乘在**发射点偏移**上、size 原样喂
    /// shader ⇒ 少乘了对象缩放：一条 `scale=4.062` 的光束覆盖面积只有它的 `1/(sx·sy)`（线性 `1/4.06`）。
    /// `None` = 旧行为（`WE_PARTICLE_SIZE_SCALE=0` 可回退）。
    quad_scale: Option<[f32; 2]>,
    /// `spritetrail` 的四边展开参数（`length, maxlength, minlength` = WE 的 `g_RenderVar0`）。
    /// `Some(...)` ⇒ `build` 在 CPU 侧按逐字复刻的 `ComputeParticleTrailTangents` +
    /// `ComputeParticlePosition` 算出四角，并把**对象缩放**作用在整个偏移上
    /// （与 `quad_scale` 对 sprite 做的是同一件事、同一份依据：WE 把粒子顶点与中心点放进
    /// **同一个** `g_ModelViewProjectionMatrix`）。旧实现把拖尾整条留在 GPU 侧 ⇒ 对象缩放
    /// 一点没作用到拖尾的宽/长上（`0.23653` 的官方预览场景会大 4 倍）。
    /// `None` = 旧行为（`WE_PARTICLE_SIZE_SCALE=0` 回退）。
    trail_cfg: Option<[f32; 3]>,
    /// 贴图高宽比（= 顶点 shader 的 `textureRatio`：`g_RenderVar1.w` / `g_Texture0Resolution.y÷x`）。
    tex_ratio: f32,
    /// 精灵表帧数：现在**由 shader 自己按寿命选帧**（`g_RenderVar1` 布局 +
    /// `a_TexCoordVec4C1.w` 的寿命进度），这里保留参数以便将来做 CPU 侧选帧。
    #[allow(dead_code)]
    seq_frames: u32,

    // ══ 本次新增的算子/初始化器状态（全部只在 `!legacy` 时被填）═════════════════
    /// 控制点在**场景坐标**里的位置（`origin + cp.offset × scale`；`id` 索引，最多 8 个）。
    cp: Vec<[f32; 3]>,
    /// `vortex`（绕控制点的涡旋力）。
    vortex: Option<VortexCfg>,
    /// `controlpointattract`（控制点向心/离心力）。
    attract: Option<AttractCfg>,
    /// `boids`（对齐/聚集/分离）。
    boids: Option<BoidsCfg>,
    /// `collisionquad`（有限矩形、单面碰撞）。
    collide: Option<CollideCfg>,
    /// `oscillatesize`：`(freqmin, freqmax, scalemin, scalemax, phasemin, phasemax)`。
    osc_size: Option<[f32; 6]>,
    /// `mapsequencearoundcontrolpoint`（初始化器）。
    mapseq: Option<MapSeqCfg>,
    /// `mapsequencearoundcontrolpoint` 的**跨粒子序号**（OWE/linux-WE 都是"每 spawn 一次 +1"）。
    seq_index: i32,
    /// `turbulentvelocityrandom` 的**噪声场采样点**（OWE `TurbulentVelocityRandomProgram::position`，
    /// 是程序级成员、跨 spawn 累积）：每抽一次初速就沿噪声方向推 `0.005 × timescale`，
    /// 于是相邻粒子拿到**逐渐变化**的方向（形成连贯的上升气流），而不是逐粒子独立随机。
    tvr_pos: [f32; 3],
    /// 发射器级音频响应（`emitter.audioprocessing*`）——按 OWE 用在**发射率**上。
    emit_audio: AudioCfg,
    /// `turbulentvelocityrandom` 的音频响应（本机 `2481886649` 的 ember/torch 用它）。
    init_audio: AudioCfg,
    /// `turbulence` 的音频响应（linux-WE 的 `audioprocessing*` 字段）。
    turb_audio: AudioCfg,
    /// 当前频谱快照（每帧刷新；没开音频功能时恒全 0 ⇒ 全部音频响应恒等）。
    /// 来源：`WE_AUDIO_SPECTRUM=1` 的真频谱（内核采集/喂入）或 `WE_AUDIO_BANDS` 静态值。
    audio: crate::audio::Spectrum,
    // ══ 本次：各算子的 blend-in/out 权重（见 [`Blend`]）；`None` = 该算子没写那四个字段 ══
    /// `oscillateposition` 的 blend。
    blend_osc: Option<Blend>,
    /// `oscillatealpha` 的 blend（本机唯一命中：`2887537617` 的 `fireflies`）。
    blend_osc_alpha: Option<Blend>,
    /// `oscillatesize` 的 blend。
    blend_osc_size: Option<Blend>,
    /// `turbulence` 的 blend。
    blend_turb: Option<Blend>,
    /// `controlpointattract` 的 blend。
    blend_attract: Option<Blend>,
    /// `vortex` 的 blend。
    blend_vortex: Option<Blend>,
}

impl ParticleSystem {
    pub fn new(
        preset: ParticlePreset,
        origin: [f32; 3],
        scale: [f32; 2],
        ov: &InstanceOverride,
        seq_frames: u32,
    ) -> Self {
        let mut gravity = [0.0, 0.0, 0.0];
        let mut drag = 0.0;
        let mut fadein = 0.0;
        let mut fadeout = 0.0;
        let mut osc = None;
        let mut osc_alpha: Option<[f32; 6]> = None;
        let mut turb: Option<(f32, f32, f32, f32, [f32; 3])> = None;
        let mut angmov: Option<(f32, [f32; 3])> = None;
        let mut size_ops: Vec<(f32, f32, f32, f32)> = Vec::new();
        let mut color_ops: Vec<(f32, f32, [f32; 3], [f32; 3])> = Vec::new();
        // ── 本次新增（见各 `*Cfg` 的注释拿依据/默认值）──
        let mut osc_size: Option<[f32; 6]> = None;
        let mut attract: Option<AttractCfg> = None;
        let mut vortex: Option<VortexCfg> = None;
        let mut boids: Option<BoidsCfg> = None;
        let mut collide: Option<CollideCfg> = None;
        let mut mapseq: Option<MapSeqCfg> = None;
        let mut init_audio = AudioCfg::default();
        let mut turb_audio = AudioCfg::default();
        // 本次：各算子的 blend-in/out（见 [`Blend`]）
        let mut blend_osc: Option<Blend> = None;
        let mut blend_osc_alpha: Option<Blend> = None;
        let mut blend_osc_size: Option<Blend> = None;
        let mut blend_turb: Option<Blend> = None;
        let mut blend_attract: Option<Blend> = None;
        let mut blend_vortex: Option<Blend> = None;
        // 湍流算子的 phase/speed 是\"每个算子抽一次\"，所以这里就要能抽随机数
        let mut rng = Rng(0x9E3779B9);
        for op in &preset.operator {
            match op.name.as_str() {
                "movement" => {
                    gravity = v3(&op.gravity, [0.0, 0.0, 0.0]);
                    drag = op.drag.unwrap_or(0.0);
                }
                "alphafade" => {
                    fadein = op.fadeintime;
                    fadeout = op.fadeouttime;
                }
                // WE 的 sizechange：`p.size = p.initial.size × multiplier`（参照 CParticle.cpp:1160）
                "sizechange" => {
                    let st = op.starttime.unwrap_or(0.0);
                    let en = op.endtime.unwrap_or(1.0);
                    let sv = op.startvalue.as_ref().map(|v| num_of(v, 1.0)).unwrap_or(1.0);
                    let ev = op.endvalue.as_ref().map(|v| num_of(v, 1.0)).unwrap_or(1.0);
                    size_ops.push((st, en, sv, ev));
                }
                // colorchange：同为"乘在初值上"，`startvalue`/`endvalue` 是 "r g b"
                "colorchange" => {
                    let st = op.starttime.unwrap_or(0.0);
                    let en = op.endtime.unwrap_or(1.0);
                    let sv = op
                        .startvalue
                        .as_ref()
                        .map(|v| vec3_of(v, [1.0, 1.0, 1.0]))
                        .unwrap_or([1.0, 1.0, 1.0]);
                    let ev = op
                        .endvalue
                        .as_ref()
                        .map(|v| vec3_of(v, [1.0, 1.0, 1.0]))
                        .unwrap_or([1.0, 1.0, 1.0]);
                    color_ops.push((st, en, sv, ev));
                }
                // 位移振荡：我们的简化实现（幅度取 scalemin/scalemax 的中点、sin 驱动）。
                // WE 默认 frequencymax=5、scale 0..1（OWE `FrequencyValue::ReadFromJson`）。
                "oscillateposition" => {
                    let fmax = op.frequencymax.unwrap_or(5.0);
                    let fmax = if fmax == 0.0 {
                        op.frequencymin.unwrap_or(0.0)
                    } else {
                        fmax
                    };
                    osc = Some((
                        op.frequencymin.unwrap_or(0.0),
                        fmax,
                        v3(&op.mask, [1.0, 0.0, 0.0]),
                        op.scalemin.unwrap_or(0.0),
                        op.scalemax.unwrap_or(1.0),
                    ));
                    blend_osc = Blend::from_op(op);
                }
                // `oscillatealpha`：alpha 按余弦在 [scalemin, scalemax] 之间摆动
                // （OWE `OscillateScalarOperator` + linux-WE `createOscillateAlphaOperator`）：
                // `multiplier = mix(scalemin, scalemax, (cos(w*age + phase)+1)/2)`，
                // 乘在 **alphafade 之后的 alpha** 上（参照把它叫 base，每帧刷新）。
                // 默认值：frequencymin 0 / frequencymax 10（=0 时退化成 frequencymin）、
                // scale 0..1、phase = rand(phasemin, phasemax + 2π)（linux-WE:1522）。
                "oscillatealpha" if !legacy() => {
                    let fmax = op.frequencymax.unwrap_or(10.0);
                    let fmax = if fmax == 0.0 {
                        op.frequencymin.unwrap_or(0.0)
                    } else {
                        fmax
                    };
                    osc_alpha = Some([
                        op.frequencymin.unwrap_or(0.0),
                        fmax,
                        op.scalemin.unwrap_or(0.0),
                        op.scalemax.unwrap_or(1.0),
                        op.phasemin.unwrap_or(0.0),
                        op.phasemax.unwrap_or(std::f32::consts::TAU),
                    ]);
                    blend_osc_alpha = Blend::from_op(op);
                }
                // `turbulence`：Perlin curl noise 扰动速度（OWE `TurbulenceOperator`：
                // 每粒子 `velocity += speed * speedOverride * normalize(curlNoise((pos + (phase + timescale*t)*x̂) * scale * 2)) * mask * dt`）。
                // phase/speed **每个算子抽一次**（不是每颗粒子）。默认 scale 0.01 / timescale 20 / speed 500..1000 / mask (1,1,0)。
                "turbulence" if !legacy() => {
                    let s = op.scale.unwrap_or(0.01);
                    let s = if s != 0.0 { s } else { 0.01 };
                    let ts = op.timescale.unwrap_or(20.0);
                    let smin = op.speedmin.unwrap_or(500.0);
                    let smax = op.speedmax.unwrap_or(1000.0);
                    let speed = rng.range(smin, smax.max(smin));
                    let phase = rng.range(
                        op.phasemin.unwrap_or(0.0),
                        op.phasemax.unwrap_or(0.0).max(op.phasemin.unwrap_or(0.0)),
                    );
                    turb = Some((s, ts, speed, phase, v3(&op.mask, [1.0, 1.0, 0.0])));
                    blend_turb = Blend::from_op(op);
                }
                // `angularmovement`：角速度积分（linux-WE `createAngularMovementOperator`）：
                // `rotation += angularVelocity*dt*speed`；`angularVelocity += force*dt*speed`；
                // `angularVelocity *= clamp(1 - drag*dt)`。初始 rotation/angularVelocity
                // 由 `rotationrandom` / `angularvelocityrandom` 初始化器给。
                "angularmovement" if !legacy() => {
                    angmov = Some((op.drag.unwrap_or(0.0), v3(&op.force, [0.0, 0.0, 0.0])));
                }
                // `oscillatesize`：与 `oscillatealpha` 完全同构（同一套 `FrequencyValue` 字段），
                // 只是乘在 **size** 上。依据：OWE `ParticleParser.cpp:1456-1463`（同一个
                // `OscillateScalarOperator`，`Target::Size`）+ `FrequencyValue::ReadFromJson`
                // （`oscillatesize` 的默认 `scalemin 0.8 / scalemax 1.2`，`frequencymax` 0 ⇒
                // 退化成 `frequencymin`）；linux-WE `CParticle.cpp:1540-1582`
                // （`createOscillateSizeOperator`）+ `ObjectParser.cpp:743-748`（同样的默认值）。
                "oscillatesize" if new_ops_on() => {
                    let fmax = op.frequencymax.unwrap_or(10.0);
                    let fmax = if fmax == 0.0 { op.frequencymin.unwrap_or(0.0) } else { fmax };
                    osc_size = Some([
                        op.frequencymin.unwrap_or(0.0),
                        fmax,
                        op.scalemin.unwrap_or(0.8),
                        op.scalemax.unwrap_or(1.2),
                        op.phasemin.unwrap_or(0.0),
                        op.phasemax.unwrap_or(std::f32::consts::TAU),
                    ]);
                    blend_osc_size = Blend::from_op(op);
                }
                // `controlpointattract`：见 `AttractCfg`（取 OWE 的公式与默认值）。
                "controlpointattract" if new_ops_on() => {
                    attract = Some(AttractCfg {
                        cp: cp_index(op.controlpoint),
                        origin: v3(&op.origin, [0.0, 0.0, 0.0]),
                        scale: op.scale.unwrap_or(512.0),
                        threshold: op.threshold.unwrap_or(512.0),
                        flags: op.flags.unwrap_or(2.0) as u32,
                    });
                    blend_attract = Blend::from_op(op);
                }
                // `vortex`（= WE 的 `vortex` 与 `vortex_v2`；两份参照实现的公式见 `VortexCfg`）。
                "vortex" | "vortex_v2" if new_ops_on() => {
                    vortex = Some(VortexCfg {
                        cp: cp_index(op.controlpoint),
                        flags: op.flags.unwrap_or(0.0) as u32,
                        axis: v3(&op.axis, [0.0, 0.0, 1.0]),
                        offset: v3(&op.offset, [0.0, 0.0, 0.0]),
                        distance_inner: op.distanceinner.unwrap_or(500.0),
                        distance_outer: op.distanceouter.unwrap_or(650.0),
                        speed_inner: op.speedinner.unwrap_or(2500.0),
                        speed_outer: op.speedouter.unwrap_or(0.0),
                        center_force: op.centerforce.unwrap_or(1.0),
                        ring_radius: op.ringradius.unwrap_or(300.0),
                        ring_width: op.ringwidth.unwrap_or(50.0),
                        ring_pull_distance: op.ringpulldistance.unwrap_or(50.0),
                        ring_pull_force: op.ringpullforce.unwrap_or(10.0),
                        audio_mode: op.audioprocessingmode.unwrap_or(0.0).max(0.0) as u32,
                    });
                    blend_vortex = Blend::from_op(op);
                }
                // `boids`：见 `BoidsCfg`（依据强度弱 —— 无参照实现，只有官方文案 + 字段名）。
                "boids" if new_ops_on() => {
                    boids = Some(BoidsCfg {
                        alignment: op.alignmentfactor.unwrap_or(0.0),
                        cohesion: op.cohesionfactor.unwrap_or(0.0),
                        separation: op.separationfactor.unwrap_or(0.0),
                        neighbour_threshold: op.neighborthreshold.unwrap_or(0.0),
                        // 没给 `separationthreshold` 时退回 `neighborthreshold`
                        // （编辑器把两者并列，语义上"分离"用的就是更近的那一圈）。
                        separation_threshold: op
                            .separationthreshold
                            .unwrap_or_else(|| op.neighborthreshold.unwrap_or(0.0)),
                    });
                }
                // `collisionquad`：见 `CollideCfg`（依据强度弱）。
                "collisionquad" if new_ops_on() => {
                    let size = v2s(&op.size, [0.0, 0.0]);
                    let behavior = match op.collisionbehavior.as_deref() {
                        Some("bounce") => 1u8,
                        Some("stop") => 2,
                        Some("kill") | Some("die") | Some("destroy") | Some("remove") => 3,
                        // WE/OWE 没写的默认取 "slide"（本机唯一一处的显式值也是它）
                        _ => 0,
                    };
                    collide = Some(CollideCfg {
                        center: v3(&op.origin, [0.0, 0.0, 0.0]),
                        half: [size[0].abs() / 2.0, size[1].abs() / 2.0],
                        behavior,
                        stop_rot: op.stoprotationoncollision.unwrap_or(0.0) != 0.0,
                    });
                }
                _ => {}
            }
        }
        // `turbulence` 的音频响应（linux-WE 把 `audioprocessing*` 挂在湍流算子上；
        // `ObjectParser.cpp:713-718` 给默认 mode 0 / bounds (0,1) / exponent 1 / freq 0..15）。
        for op in &preset.operator {
            if op.name == "turbulence" && new_ops_on() {
                turb_audio = audio_cfg(
                    op.audioprocessingmode,
                    None, // linux-WE 的湍流音频没有 `amount` 字段 ⇒ 用 OWE 默认 1.0
                    op.audioprocessingexponent,
                    &op.audioprocessingbounds,
                    (
                        op.audioprocessingfrequencystart,
                        op.audioprocessingfrequencyend,
                    ),
                );
            }
        }
        // `mapsequencearoundcontrolpoint`（初始化器）—— 取第一个出现的。
        for ini in &preset.initializer {
            if ini.name == "mapsequencearoundcontrolpoint" && new_ops_on() {
                mapseq = Some(MapSeqCfg {
                    cp: cp_index(ini.controlpoint),
                    count: ini.count.unwrap_or(1.0),
                    bounds: v2s(&ini.bounds, [0.0, 1.0]),
                    axis: v3(&ini.axis, [0.0, 0.0, 1.0]),
                    speed_min: vec3_of(&ini.speedmin, [0.0, 0.0, 0.0]),
                    speed_max: vec3_of(&ini.speedmax, [0.0, 0.0, 0.0]),
                });
            }
            // `turbulentvelocityrandom` 的音频响应（本机 `2481886649` 的 ember/torch）：
            // 依据强度**中**——字段名与 emitter 的 `audioprocessing*` 同族，公式借用
            // OWE `AudioResponseScale`（同样的 amount/exponent/bounds 语义），但**没有参照实现**
            // 直接把它用在初始化器上，所以只调**初速**（最局部的一处），不动发射率。
            if ini.name == "turbulentvelocityrandom" && new_ops_on() {
                init_audio = audio_cfg(
                    ini.audioprocessingmode,
                    None,
                    ini.audioprocessingexponent,
                    &ini.audioprocessingbounds,
                    (
                        ini.audioprocessingfrequencystart,
                        ini.audioprocessingfrequencyend,
                    ),
                );
            }
        }
        // 发射器音频响应（OWE 把它用在发射率上；没音频时 `scale` 恒为 1.0）。
        // ⚠ `audioamount` / `audioexponent` 在 JSON 里**常常不写**，而 WE 的默认是
        // **1.0 / 1.0**（OWE `ParticleObject.cppm:82-83`：`audioamount { 1.0f }`、
        // `audioexponent { 1.0f }`；`ParticleParser.cpp:1515-1521` 原样透传）。
        // 我们的 serde 默认是 0.0 ⇒ 会把 `scale` 算成 `1 + level×0 = 1`，
        // 等于"这条音频响应永远不生效"（实测 `2997301801` 的 emitter 就是这样：
        // 只有 `audioprocessingmode:3` + `audioprocessingbounds`，没写 amount）。
        // 所以这里把 0 当"未写"处理 ⇒ 回到 WE 的 1.0。
        let emit_audio = if new_ops_on() {
            preset
            .emitter
            .first()
            .map(|em| {
                audio_cfg(
                    Some(em.audioprocessingmode),
                    none_if_zero(em.audioamount),
                    none_if_zero(em.audioexponent),
                    &em.audiobounds,
                    (None, None),
                )
            })
            .unwrap_or_default()
        } else {
            AudioCfg::default()
        };
        // 控制点：`origin + offset × scale`（linux-WE 的非 worldSpace 分支；`id` 当槽号）。
        // 本次补上**对象级覆盖** `instanceoverride.controlpointN`：WE 编辑器里每个粒子
        // 对象实例都能单独拖 8 个控制点的坐标（本机 3 条场景用到：`1931934283` 6 处、
        // `2861113973` 2 处、`3571777169` 4 处）。语义取 **OWE**（唯一实现了它的参照；
        // `linux-wallpaperengine` 的 `ParticleInstanceOverride` 根本没有控制点字段 ⇒ 两家分歧）：
        // `ParticleRuntime.cpp:605-631` 的 `UpdateControlpoints`——
        // · `offset = base_offset`（预设里的 `controlpoint[].offset`，**相对粒子系统原点**）
        // · 有覆盖时：`worldspace`（预设 flags 位 1 = 2）⇒ `offset = local_from_world × 覆盖值`
        // （覆盖值是**绝对世界/场景坐标**，与对象位置无关；OWE `ParticleObject.cppm:29-31`
        // 注释：'the control point will always be at the same position in the world'）
        // 否则 ⇒ `offset += 覆盖值`（覆盖值是**相对对象原点**的局部偏移）
        // ⇒ 我们没有 OWE 那套 4×4 世界矩阵，改用与别的路径一致的近似：局部量乘对象 scale
        // 再加 `origin`；worldspace 时直接落在绝对坐标上（scale/origin 都不再作用）。
        // `WE_CP_OVERRIDE=0` 回旧行为（忽略覆盖）。
        let mut cp: Vec<[f32; 3]> = vec![[0.0, 0.0, 0.0]; 8];
        let mut cp_base: [[f32; 3]; 8] = [[0.0; 3]; 8];
        let mut cp_ws = [false; 8];
        for c in &preset.controlpoint {
            let slot = (c.id.max(0.0) as i32 as usize).min(7);
            let off = v3(&c.offset, [0.0, 0.0, 0.0]);
            cp_base[slot] = off;
            // OWE `ParticleControlpoint::FlagEnum::worldspace = 1`（位掩码 2）
            cp_ws[slot] = (c.flags as i64) & 2 != 0;
            cp[slot] = [
                origin[0] + off[0] * scale[0],
                origin[1] + off[1] * scale[1],
                origin[2] + off[2],
            ];
        }
        // 对象级覆盖只改**被覆盖的**槽位 ⇒ 没有覆盖的对象逐位不变（`cp_override_on` 默认开）。
        if cp_override_on() && ov.has_cp_override() {
            for slot in 0..8 {
                if let Some(o) = ov.cp_override(slot) {
                    cp[slot] = if cp_ws[slot] {
                        o // 绝对世界/场景坐标（worldspace）
                    } else {
                        let b = cp_base[slot];
                        [
                            origin[0] + (b[0] + o[0]) * scale[0],
                            origin[1] + (b[1] + o[1]) * scale[1],
                            origin[2] + (b[2] + o[2]),
                        ]
                    };
                }
            }
        }
        let color_mul = ov
            .colorn
            .as_deref()
            .map(|s| v3(s, [1.0, 1.0, 1.0]))
            .unwrap_or([1.0, 1.0, 1.0]);
        let seq_mult = if preset.sequencemultiplier > 0.0 {
            preset.sequencemultiplier
        } else {
            1.0
        };
        ParticleSystem {
            origin,
            scale: [scale[0].abs().max(0.001), scale[1].abs().max(0.001)],
            alpha: ov.alpha.unwrap_or(1.0),
            rate_mul: ov.rate.unwrap_or(1.0),
            count_mul: ov.count.unwrap_or(1.0),
            size_mul: ov.size.unwrap_or(1.0),
            life_mul: ov.lifetime.unwrap_or(1.0),
            speed_mul: ov.speed.unwrap_or(1.0),
            color_mul,
            spawn_acc: 0.0,
            // 模拟时钟从 0 起；延迟出场交给 `start_delay`（以前是把 time 直接初始化成 starttime，
            // 结果延迟**没生效**——粒子照样从第 0 秒开始铺）。
            time: 0.0,
            start_delay: preset.starttime.max(0.0),
            rng,
            gravity,
            drag,
            fadein,
            fadeout,
            osc,
            osc_alpha,
            turb,
            angmov,
            random_frame: preset.random_frame() && !legacy(),
            seq_mult,
            size_ops,
            color_ops,
            seq_frames: seq_frames.max(1),
            half_size: true,
            emit_xform: None,
            cp,
            vortex,
            attract,
            boids,
            collide,
            osc_size,
            mapseq,
            seq_index: 0,
            tvr_pos: [0.0, 0.0, 0.0],
            emit_audio,
            init_audio,
            turb_audio,
            audio: audio_now(),
            blend_osc,
            blend_osc_alpha,
            blend_osc_size,
            blend_turb,
            blend_attract,
            blend_vortex,
            particles: Vec::new(),
            preset,
            quad_scale: None,
            trail_cfg: None,
            tex_ratio: 1.0,
        }
    }

    /// 本帧写进 `a_TexCoordVec4C1.w` 的**精灵表寿命进度**（WE 官方顶点 shader 用它选帧：
    /// `ComputeSpriteFrame(frac(lifetime))` → `frame = floor(frac(lifetime) * numFrames)`，见 `common_particles.h:61-85`）。
///
    /// * `animationmode:"randomframe"` ⇒ **每颗粒子固定在一帧**：写它出生时抽的那个
    /// `rand ∈ [0,1)`（OWE `AnimationLifetime` 的 `RANDOMONE` 分支：
    /// `min(1-ε, max(0, value.random))`）⇒ 帧号 = `floor(rand × frames)`，一辈子不变。
///
    /// * 其余（`sequence` / 未声明）⇒ 按寿命推进：`frac(寿命比例 × sequencemultiplier)`。
    /// 同一式子在两份参照实现里都有：OWE `SEQUENCE` 分支 `(1 - lifetime/initial_lifetime) * sequence_multiplier`
    /// （`lifetime` 是**剩余**寿命 ⇒ 这就是 `age/life × mult`）；linux-WE `CParticle.cpp:299-301`
    /// `fmod(lifetimePos * frames * animSpeed, frames)` 再被 shader 除回帧数 ⇒ 等价。
    /// `sequencemultiplier` 为 null/0 时按 1 处理（linux-WE:279）。
    #[inline]
    fn frame_value(&self, p: &Particle) -> f32 {
        if self.random_frame {
            p.rand.min(1.0 - f32::EPSILON).max(0.0)
        } else {
            let t = p.age / p.life.max(1e-6);
            (t * self.seq_mult).fract().max(0.0)
        }
    }

    /// `sizerandom` 结果喂 shader 前是否 ÷2（参照实现语义，默认 true）。
    /// `false` 回旧行为（块大一倍）做 A/B —— 对应 `WE_PARTICLE_SIZE_LEGACY=1`。
    pub fn set_half_size(&mut self, on: bool) {
        self.half_size = on;
    }

    /// 精灵四边要过的**对象缩放**（见 `quad_scale` 字段上的注释）。
    /// `scale = [1,1]` 是**自证模式**：CPU 展开的几何必须与旧路径（GPU 侧展开）逐像素一致
    /// ⇒ 用来验证旋转/尺寸代数没写反。
    pub fn set_quad_scale(&mut self, scale: [f32; 2], tex_ratio: f32) {
        self.quad_scale = Some(scale);
        self.tex_ratio = tex_ratio;
    }

    /// `spritetrail` 的四边：同样在 CPU 侧展开，但 `right/up` 来自**速度 + 眼位**
    /// 而不是粒子自转（见 `trail_cfg` 字段）。参数自带（`preset.renderer[0]`），
    /// 所以调用点只需给缩放/贴图比。`scale = [1,1]` 是**自证模式**（应与旧的 GPU 路径逐像素一致）。
    pub fn set_trail_scale(&mut self, scale: [f32; 2], tex_ratio: f32) {
        if let Some(t) = self.preset.trail() {
            self.trail_cfg = Some(t);
            self.quad_scale = Some(scale);
            self.tex_ratio = tex_ratio;
        }
    }

    /// 场景属性脚本驱动的**发射率乘子**（`instanceoverride.rate` 是 `{"script": …}` 时，
    /// 由 [`crate::propscript::NumScript`] 每帧算出）。语义与静态 `ov.rate` 完全相同：
    /// `发射率 = emitter.rate × rate_mul × 音频响应`。默认（不开 `WE_SCRIPT_AUDIO`）
    /// 永远走构造时的静态值 ⇒ 不碰这里。
    pub fn set_rate_mul(&mut self, v: f32) {
        self.rate_mul = v;
    }

    /// 让 emitter 的局部偏移过一遍**对象的世界变换上 3×3**（含 angles 旋转 + scale）。
    /// ⚠ 与 `world_of` 同语义：`v = M3 · (base_origin + dir·dist)`，然后**只把 v 的 y 反号**
    /// 再叠到（未镜像的）`origin` 上 —— 与 lib.rs 里"origin 取消那次 y 镜像"是同一套约定。
    /// 依据：`open-wallpaper-engine` 把粒子对象做成 `SceneNode(T(origin)·R(angles)·S(scale))`，
    /// 粒子局部坐标（含 emitter origin）整体过该节点 MVP（`RenderPosition` 返回局部坐标后由节点变换）；
    /// `linux-wallpaperengine` 只翻 y、不旋转（旧实现的来源）。本开关默认走 OWE 语义。
    pub fn set_emit_xform(&mut self, m3: [f32; 9]) {
        self.emit_xform = Some(m3);
    }

    /// 诊断用（`WE_PARTICLE_GEOM=1`）：把「配置推出的落点」需要的参数一次性交出 ——
    /// `(emitter 名, directions, distancemin, distancemax, sizerandom min, sizerandom max, speedmin, speedmax)`。
    /// 距离/速度都是 **vec3/标量按 WE 语义**（`boxrandom` 的盒子逐轴，见 `Emitter` 注释）。
    /// 只读，不改任何状态（默认路径不调用）。
    pub fn emitter_debug(&self) -> (String, [f32; 3], [f32; 3], [f32; 3], f32, f32, f32, f32) {
        let em = self.preset.emitter.first();
        let name = em.map(|e| e.name.clone()).unwrap_or_default();
        let dir = em
            .map(|e| v3(&e.directions, [1.0, 1.0, 0.0]))
            .unwrap_or([1.0, 1.0, 0.0]);
        let dmn = em
            .map(|e| vec3_of(&e.distancemin, [0.0; 3]))
            .unwrap_or([0.0; 3]);
        let dmx = em
            .map(emitter_distancemax)
            .unwrap_or([0.0; 3]);
        let smin0 = em.map(|e| e.speedmin).unwrap_or(0.0);
        let smax0 = em.map(|e| e.speedmax).unwrap_or(0.0);
        let mut smn = 0.0f32;
        let mut smx = 0.0f32;
        for ini in &self.preset.initializer {
            if ini.name == "sizerandom" {
                smn = v2f(&ini.min, [10.0, 10.0])[0];
                smx = v2f(&ini.max, [50.0, 50.0])[0];
            }
        }
        (name, dir, dmn, dmx, smn, smx, smin0, smax0)
    }

    /// 诊断用：第一个 emitter 的**局部原点**（对象局部空间，**未**乘对象 `scale`）。
    /// `lib.rs` 的 `WE_PARTICLE_GEOM=1` 用它算「配置推出的发射中心」（见那里的 `[粒子落点]` 行）。
    /// 只读，不改任何状态（默认路径不调用）。
    /// 为什么单列一个：`emitter_debug` 返回的是方向/距离/尺寸区间，**没有原点**；
    /// 而「这层到底落在画布哪儿」的第一判据就是 `对象 origin + emitter.origin × 对象 scale`
    /// （`2482408967` 的 Sakura 层 0px 就是发射中心被 `scale.y=4.31616` 放大到画布外）。
    pub fn emitter_origin_debug(&self) -> [f32; 3] {
        self.preset
            .emitter
            .first()
            .map(|e| v3(&e.origin, [0.0, 0.0, 0.0]))
            .unwrap_or([0.0, 0.0, 0.0])
    }

    pub fn count(&self) -> usize {
        self.particles.len()
    }

    /// 逐颗粒子快照 `(age, pos, vel)` —— `WE_DEBUG_BOIDS_DUMP=1` 用。
    /// 定位"邻居到底长什么样"（两颗是不是同一帧生成、年龄差多少）时必需的原始数据。
    pub fn particle_dump(&self) -> Vec<(f32, [f32; 3], [f32; 3])> {
        self.particles
            .iter()
            .map(|p| (p.age, p.pos, p.vel))
            .collect()
    }

    /// 这个实例的**有效**变换（`(origin, scale)`）—— 诊断/复现用。
    /// ⚠ 离线探针（`boids_probe` / `particle_probe`）必须在 `--origin/--scale` 里给**同样的值**，
    /// 否则粒子布局与渲染器不同（本机 `3571777169` 的对象 426 就带 `parent: 385`，
    /// 渲染器走的是**父链合成后**的变换：物体自己写的 `1028.97` 并不是粒子实际落点）。
    pub fn xform(&self) -> ([f32; 3], [f32; 2]) {
        (self.origin, self.scale)
    }

    /// `boids` 诊断（`WE_DEBUG_BOIDS`；**只读，不改物理**）。
    /// 有 `boids` 算子时返回本帧快照，否则 `None`（没这个算子的层不留噪声）。
    pub fn boids_stats(&self) -> Option<BoidsStats> {
        let b = self.boids?;
        let n = self.particles.len();
        let (nt, st) = (b.neighbour_threshold, b.separation_threshold);
        // 质心（位置）单独算：速度均值不是位置均值，同向度用 `vsum` 直接比。
        let mut csum = [0.0f32; 3];
        let mut vsum = [0.0f32; 3];
        for p in &self.particles {
            for k in 0..3 {
                csum[k] += p.pos[k];
                vsum[k] += p.vel[k];
            }
        }
        let centre = if n == 0 {
            [0.0; 3]
        } else {
            [csum[0] / n as f32, csum[1] / n as f32, csum[2] / n as f32]
        };
        let mut s = BoidsStats {
            alive: n,
            with_neighbour: 0,
            with_separation: 0,
            max_neighbours: 0,
            mean_neighbours: 0.0,
            min_pair_dist: 0.0,
            mean_pair_dist: 0.0,
            max_pair_dist: 0.0,
            mean_speed: 0.0,
            max_speed: 0.0,
            vel_coherence: 0.0,
            rms_radius: 0.0,
            mean_inward_speed: 0.0,
            neighbour_threshold: nt,
            separation_threshold: st,
        };
        if n == 0 {
            return Some(s);
        }
        let (mut sp_sum, mut sp_max, mut r2, mut inward) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        let (mut dmin, mut dsum, mut dmax, mut pairs) = (f32::MAX, 0.0f32, 0.0f32, 0usize);
        let (mut nb_sum, mut nb_cnt) = (0usize, 0usize);
        for (i, p) in self.particles.iter().enumerate() {
            let sp = len3(p.vel);
            sp_sum += sp;
            sp_max = sp_max.max(sp);
            let to_c = [centre[0] - p.pos[0], centre[1] - p.pos[1], centre[2] - p.pos[2]];
            let d = len3(to_c);
            r2 += d * d;
            if d > 1e-6 {
                inward += dot3(p.vel, [to_c[0] / d, to_c[1] / d, to_c[2] / d]);
            }
            let mut nb = 0usize;
            for (j, q) in self.particles.iter().enumerate() {
                if i == j {
                    continue;
                }
                let dd = [
                    q.pos[0] - p.pos[0],
                    q.pos[1] - p.pos[1],
                    q.pos[2] - p.pos[2],
                ];
                let dist = len3(dd);
                if dist <= 1e-4 {
                    continue;
                }
                if j > i {
                    dmin = dmin.min(dist);
                    dmax = dmax.max(dist);
                    dsum += dist;
                    pairs += 1;
                }
                if nt > 0.0 && dist < nt {
                    nb += 1;
                }
                if st > 0.0 && dist < st {
                    s.with_separation += 1;
                }
            }
            if nb > 0 {
                s.with_neighbour += 1;
            }
            s.max_neighbours = s.max_neighbours.max(nb);
            nb_sum += nb;
            nb_cnt += 1;
        }
        s.with_separation /= 2; // 上面按有序对累的，除 2 才是粒子数
        s.mean_speed = sp_sum / n as f32;
        s.max_speed = sp_max;
        s.vel_coherence = if sp_sum > 1e-9 { len3(vsum) / sp_sum } else { 0.0 };
        s.rms_radius = (r2 / n as f32).sqrt();
        s.mean_inward_speed = inward / n as f32;
        s.mean_neighbours = if nb_cnt > 0 {
            nb_sum as f32 / nb_cnt as f32
        } else {
            0.0
        };
        s.min_pair_dist = if pairs > 0 { dmin } else { 0.0 };
        s.mean_pair_dist = if pairs > 0 { dsum / pairs as f32 } else { 0.0 };
        s.max_pair_dist = if pairs > 0 { dmax } else { 0.0 };
        Some(s)
    }

    /// 这个系统**实际**允许的粒子上限（= maxcount × count override，与 `update` 同一条公式）。
    /// 建 GL 批的容量必须用它 —— 之前建批只看了 `maxcount`，而模拟乘了 override，
    /// 萤火多长一颗就 66 顶点 > 60 容量 ⇒ 报错 ⇒ 内核丢掉整个渲染器重建。
    pub fn capacity(&self) -> usize {
        (self.preset.maxcount * self.count_mul).clamp(1.0, 4096.0) as usize
    }

    /// 诊断聚合量（`WE_PARTICLE_PROBE`，只读、不改物理）：一行 = 一个系统**本帧后**的状态。
    /// 用来回答 那种「慢帧下粒子是不是丢了物理时间」——判据必须是位置/寿命/速度的
    /// 数值对比，不是"看起来慢"。默认不设环境变量 ⇒ 调用方一行都不写。
    pub fn probe_line(&self, name: &str) -> String {
        let n = self.particles.len();
        let (mut px, mut py, mut pz) = (0.0f64, 0.0f64, 0.0f64);
        let (mut sp2, mut age, mut life) = (0.0f64, 0.0f64, 0.0f64);
        for p in &self.particles {
            px += p.pos[0] as f64;
            py += p.pos[1] as f64;
            pz += p.pos[2] as f64;
            sp2 += (p.vel[0] * p.vel[0] + p.vel[1] * p.vel[1] + p.vel[2] * p.vel[2]) as f64;
            age += p.age as f64;
            life += p.life as f64;
        }
        let inv = if n > 0 { 1.0 / n as f64 } else { 0.0 };
        format!(
            "{name}\tt={:.4}\tn={n}\tpos=({:.3},{:.3},{:.3})\tsp2={:.3}\tage={:.4}\tlife={:.4}",
            self.time,
            px * inv,
            py * inv,
            pz * inv,
            sp2 * inv,
            age * inv,
            life * inv
        )
    }

    /// 推进 dt 秒：**照单全收**（`dt` 上限与主循环一致 0.25 s），内部按 ≤1/30 s 的子步积分。
    /// 以前这里把 `dt` 再夹到 0.1 s ⇒ 单圈 >100 ms 时粒子**系统性丢物理时间**
    /// （ 实测转储期间每圈 222.5 ms ⇒ 只进 45% 的时间，看起来"慢一半"）
    /// 而场景其余部分（`g_Time`、网格动画、脚本标量）走的是主循环夹过的 0.25 s
    /// ⇒ 慢帧时粒子与画面其余部分**脱同期**。
    /// 现在：总时长一分不丢，但一步也不跨太远（欧拉积分在大步长下会过冲/穿模）。
    pub fn update(&mut self, dt: f32) {
        let dt = dt.clamp(0.0, 0.25);
        // 正常路径（60 fps 的 16.7 ms、离线 1/30 的 33.3 ms）**只有一次子步、步长就是 dt**
        // ⇒ 与改动前逐字节相同。
        // ⚠ 必须带容差：`dt` 是**墙钟差值**（`now - self.time`），1/30 的累加结果实测比常量
        // 大 1 个 ulp（`0.1+1/30-0.1 = 0.033333343 > 0.033333335`）。直接比 `<= MAX_SUBSTEP`
        // 会把离线 1/30 口径误判成"两步"⇒ 像素不再逐字节相同（本次就是这么撞出来的）。
        let cap = MAX_SUBSTEP * (1.0 + SUBSTEP_EPS);
        if dt <= cap {
            self.step(dt);
            return;
        }
        // 子步**踩在绝对时间的 1/30 网格上**：先走到下一个网格点，然后整步 1/30，末尾吃余量。
        // 这样慢帧与"1/30 口径"的发射量化边界**落在同一组时刻**上（发射按步边重量化），
        // 位置/岁数才真正可比；`dt/n` 均分（0.25/8 = 31.25 ms）会统统偏离网格。
        let mut left = dt;
        while left > 0.0 {
            let phase = self.time.rem_euclid(MAX_SUBSTEP);
            let mut d = MAX_SUBSTEP - phase;
            if !(d > SUBSTEP_EPS) {
                d = MAX_SUBSTEP;
            }
            if d > left {
                d = left;
            }
            self.step(d);
            left -= d;
        }
    }

    /// 一个积分子步：先发射，再逐粒子积分 + 老化。`dt` 必须已在 `[0, MAX_SUBSTEP]` 内。
    fn step(&mut self, dt: f32) {
        self.time += dt;
        // `starttime`：WE 用它做"延迟出场"。**以前完全忽略** ⇒ `Light shafts 0`（starttime 10、
        // 尺寸 850~1000 单位、寿命 8~20 秒的暖橙大光柱）从第 0 秒就开始铺 ⇒ t=6s 时整屏被它糊成红橙色，
        // 就是用户问的那条"红色斑块/竖带"（实测排除它红点 167686 → 7229）。
        if self.time < self.start_delay {
            return;
        }
        let maxcount = self.capacity();

        // 频谱每帧刷新一次（内核采集线程/控制面随时在写；没开功能时是全 0 ⇒ 恒等）。
        // 放在这里而不是构造时：真频谱是**随时间变化**的，这是"音频响应真正生效"的前提。
        self.audio = audio_now();

        // 发射：rate 是"每秒几个"，乘 override 的 rate
        // 音频响应（OWE 把它乘在发射率上；`AudioResponseScale` 在**没有音频**时返回 1.0
        // ⇒ 本机默认一个粒子都不会多也不会少）。
        if let Some(em) = self.preset.emitter.first().cloned() {
            let rate = (em.rate * self.rate_mul * self.emit_audio.scale(&self.audio)).max(0.0);
            self.spawn_acc += rate * dt;
            while self.spawn_acc >= 1.0 && self.particles.len() < maxcount {
                self.spawn_acc -= 1.0;
                let p = self.spawn(&em);
                self.particles.push(p);
            }
        }

        let life_mul = self.life_mul;
        let speed = self.speed_mul;
        // 湍流噪声场：相位随**绝对时间**推进（OWE `position.x += phase + timescale * frame->time`）。
        // 湍流速度区间再乘它的音频响应（同为恒等，除非喂了 `WE_PARTICLE_AUDIO`）。
        let turb = self.turb.map(|(sc, ts, sp, ph, mask)| {
            (
                sc,
                sp * self.turb_audio.scale(&self.audio),
                ph + ts * self.time,
                mask,
            )
        });
        let angmov = self.angmov;
        // 本次：blend-in/out 权重所需的算子拷贝（见 [`Blend`]）。
        let blend_turb = self.blend_turb;
        let blend_vortex = self.blend_vortex;
        let blend_attract = self.blend_attract;
        // ── 本次新增算子（Copy 出来再进 `iter_mut`，避免借用冲突）──
        // `vortex` 的音频模式 >0 且**没有音频** ⇒ 整条算子跳过（linux-WE `CParticle.cpp:1324-1327`）。
        let vortex = self
            .vortex
            .filter(|v| v.audio_mode == 0 || self.audio.any());
        let attract = self.attract;
        let boids = self.boids;
        let collide = self.collide;
        // boids 需要邻居的**上一帧**位置/速度（不能边算边改）；上限保护见 BoidsCfg 注释。
        let snap: Vec<([f32; 3], [f32; 3])> = if boids.is_some() && self.particles.len() <= 512 {
            self.particles.iter().map(|p| (p.pos, p.vel)).collect()
        } else {
            Vec::new()
        };
        let collide_center = collide.map(|c| {
            [
                self.origin[0] + c.center[0] * self.scale[0],
                self.origin[1] + c.center[1] * self.scale[1],
                self.origin[2] + c.center[2],
            ]
        });
        for (pi, p) in self.particles.iter_mut().enumerate() {
            p.age += dt;
            // movement：重力 + 阻力（简化：vel *= 1 - drag*dt）
            p.vel[0] += self.gravity[0] * dt;
            p.vel[1] += self.gravity[1] * dt;
            p.vel[2] += self.gravity[2] * dt;
            let damp = (1.0 - self.drag * dt).clamp(0.0, 1.0);
            p.vel[0] *= damp;
            p.vel[1] *= damp;
            p.vel[2] *= damp;
            // turbulence：Perlin curl noise 推速度（在 movement 之后、位置积分之前 —— 与预设里
            // operator 的排列顺序一致：movement → alphafade/sizechange → oscillate* → turbulence → angularmovement）
            if let Some((scale, amp, phase_t, mask)) = turb {
                // blend-in/out：湍流的强度按权重从 0 插进来（见 [`Blend`]）。
                let amp = match blend_turb {
                    Some(b) => blend_add(b.weight(p.age / p.life), amp),
                    None => amp,
                };
                let k = scale * 2.0;
                let np = [
                    (p.pos[0] + phase_t) * k,
                    p.pos[1] * k,
                    p.pos[2] * k,
                ];
                let mut c = curl_noise(np);
                let l = (c[0] * c[0] + c[1] * c[1] + c[2] * c[2]).sqrt();
                if l > 1e-4 {
                    for i in 0..3 {
                        c[i] = c[i] / l * amp * speed;
                    }
                }
                p.vel[0] += c[0] * mask[0] * dt;
                p.vel[1] += c[1] * mask[1] * dt;
                p.vel[2] += c[2] * mask[2] * dt;
            }
            // `vortex`：绕控制点的涡旋力。公式/默认值/分歧见 `VortexCfg`。
            if let Some(v) = vortex {
                let center = [
                    self.cp[v.cp][0] + v.offset[0],
                    self.cp[v.cp][1] + v.offset[1],
                    self.cp[v.cp][2] + v.offset[2],
                ];
                let mut axis = v.axis;
                let al = (axis[0] * axis[0] + axis[1] * axis[1] + axis[2] * axis[2]).sqrt();
                axis = if al > 1e-6 {
                    [axis[0] / al, axis[1] / al, axis[2] / al]
                } else {
                    [0.0, 0.0, 1.0]
                };
                let to_p = [p.pos[0] - center[0], p.pos[1] - center[1], p.pos[2] - center[2]];
                // flags 位 1 = infinite axis（把轴向分量投影掉 ⇒ 柱面而不是球面）
                let (radial, distance) = if v.flags & 1 != 0 {
                    let ax = dot3(to_p, axis);
                    let r = [to_p[0] - axis[0] * ax, to_p[1] - axis[1] * ax, to_p[2] - axis[2] * ax];
                    (r, len3(r))
                } else {
                    (to_p, len3(to_p))
                };
                let mut tangent = cross3(axis, radial);
                let tl = len3(tangent);
                if tl > 1e-3 {
                    tangent = [tangent[0] / tl, tangent[1] / tl, tangent[2] / tl];
                } else {
                    // 粒子正好在轴上 ⇒ 没有切向可言（linux-WE 也是 `continue`）
                    tangent = [0.0, 0.0, 0.0];
                }
                let sp = vortex_speed_cfg(&v, distance);
                // blend-in/out：整条 vortex 的作用按权重 `vt` 缩放（`dtv = dt × vt`）。
                let dtv = dt
                    * match blend_vortex {
                        Some(b) => b.weight(p.age / p.life),
                        None => 1.0,
                    };
                let ring = v.flags & 4 != 0 && v.ring_radius > 0.0 && v.ring_width > 0.0 && v.ring_pull_distance > 0.0;
                if len3(tangent) > 0.0 && ring {
                    // 环带模式（linux-WE `CParticle.cpp:1395-1419`）：空心环 + 环外回拉
                    let ring_inner = v.ring_radius - v.ring_width * 0.5;
                    let ring_outer = v.ring_radius + v.ring_width * 0.5;
                    let mut ring_speed = 0.0;
                    let mut radial_force = [0.0, 0.0, 0.0];
                    if distance < ring_inner {
                        ring_speed = 0.0;
                    } else if distance <= ring_outer {
                        let t = (distance - ring_inner) / v.ring_width;
                        ring_speed = lerpf(t, v.speed_inner, v.speed_outer);
                    } else if distance <= ring_outer + v.ring_pull_distance {
                        let pull_t = (distance - ring_outer) / v.ring_pull_distance;
                        ring_speed = v.speed_outer * (1.0 - pull_t);
                        if distance > 0.001 {
                            let inv = 1.0 / distance;
                            radial_force = [
                                -radial[0] * inv * v.ring_pull_force * pull_t,
                                -radial[1] * inv * v.ring_pull_force * pull_t,
                                -radial[2] * inv * v.ring_pull_force * pull_t,
                            ];
                        }
                    }
                    p.vel[0] += tangent[0] * ring_speed * dtv * speed;
                    p.vel[1] += tangent[1] * ring_speed * dtv * speed;
                    p.vel[2] += tangent[2] * ring_speed * dtv * speed;
                    p.vel[0] += radial_force[0] * dtv * speed;
                    p.vel[1] += radial_force[1] * dtv * speed;
                    p.vel[2] += radial_force[2] * dtv * speed;
                } else if len3(tangent) > 0.0 {
                    if v.flags & 2 != 0 {
                        // maintain distance：只改切向分量，保持径/轴向速度（linux-WE:1441-1444）
                        let t_speed = dot3(p.vel, tangent) + sp * dtv * speed;
                        let ax_v = dot3(p.vel, axis);
                        p.vel = [
                            axis[0] * ax_v + tangent[0] * t_speed,
                            axis[1] * ax_v + tangent[1] * t_speed,
                            axis[2] * ax_v + tangent[2] * t_speed,
                        ];
                        if distance > 0.001 {
                            let inv = 1.0 / distance;
                            p.vel[0] += -radial[0] * inv * v.center_force * dtv * speed;
                            p.vel[1] += -radial[1] * inv * v.center_force * dtv * speed;
                            p.vel[2] += -radial[2] * inv * v.center_force * dtv * speed;
                        }
                    } else {
                        p.vel[0] += tangent[0] * sp * dtv * speed;
                        p.vel[1] += tangent[1] * sp * dtv * speed;
                        p.vel[2] += tangent[2] * sp * dtv * speed;
                    }
                }
            }
            // `controlpointattract`：控制点向心/离心冲量（取 OWE 的公式与默认值，见 `AttractCfg`）。
            if let Some(a) = attract {
                let center = [
                    self.cp[a.cp][0] + a.origin[0],
                    self.cp[a.cp][1] + a.origin[1],
                    self.cp[a.cp][2] + a.origin[2],
                ];
                let diff = [center[0] - p.pos[0], center[1] - p.pos[1], center[2] - p.pos[2]];
                let d = len3(diff);
                if d > 0.0 && d < a.threshold {
                    let mut impulse = a.scale * dt * (1.0 - d / a.threshold);
                    // blend-in/out：向心冲量按权重从 0 插进来（见 [`Blend`]）。
                    if let Some(b) = blend_attract {
                        impulse = blend_add(b.weight(p.age / p.life), impulse);
                    }
                    if a.flags & 2 != 0 {
                        impulse = impulse.min(d); // `min(distance, impulse)`（OWE 原文注释）
                    }
                    let k = impulse / d;
                    p.vel[0] += diff[0] * k;
                    p.vel[1] += diff[1] * k;
                    p.vel[2] += diff[2] * k;
                }
            }
            // `boids`：对齐 / 聚集 / 分离（依据强度**弱**，见 `BoidsCfg`；邻居用上一帧快照）
            if let Some(b) = boids {
                if b.neighbour_threshold > 0.0 && !snap.is_empty() && pi < snap.len() {
                    let (mut av, mut cv, mut sv) = ([0.0f32; 3], [0.0f32; 3], [0.0f32; 3]);
                    let (mut an, mut cn, mut sn) = (0.0f32, 0.0f32, 0.0f32);
                    for (j, (pj, vj)) in snap.iter().enumerate() {
                        if j == pi {
                            continue;
                        }
                        let dd = [pj[0] - p.pos[0], pj[1] - p.pos[1], pj[2] - p.pos[2]];
                        let dist = len3(dd);
                        if dist <= 1e-4 {
                            continue;
                        }
                        if dist < b.neighbour_threshold {
                            for k in 0..3 {
                                av[k] += vj[k];
                                cv[k] += pj[k];
                            }
                            an += 1.0;
                            cn += 1.0;
                        }
                        if b.separation_threshold > 0.0 && dist < b.separation_threshold {
                            let inv = -1.0 / dist;
                            for k in 0..3 {
                                sv[k] += dd[k] * inv;
                            }
                            sn += 1.0;
                        }
                    }
                    if an > 0.0 {
                        let inv = 1.0 / an;
                        for k in 0..3 {
                            p.vel[k] += (av[k] * inv - p.vel[k]) * b.alignment * dt;
                        }
                    }
                    if cn > 0.0 {
                        let inv = 1.0 / cn;
                        for k in 0..3 {
                            p.vel[k] += (cv[k] * inv - p.pos[k]) * b.cohesion * dt;
                        }
                    }
                    if sn > 0.0 {
                        let inv = b.separation * dt / sn;
                        for k in 0..3 {
                            p.vel[k] += sv[k] * inv;
                        }
                    }
                }
            }
            // angularmovement：rotation/角速度积分（linux-WE `createAngularMovementOperator`）
            if let Some((drag_a, force)) = angmov {
                let dfac = (1.0 - drag_a * dt).clamp(0.0, 1.0);
                for i in 0..3 {
                    p.rot[i] += p.ang_vel[i] * dt * speed;
                    p.ang_vel[i] = (p.ang_vel[i] + force[i] * dt * speed) * dfac;
                    // 绕到 [-π, π]，避免长时间累积把浮点精度烧掉（同一份参照实现也这么做）
                    let two_pi = std::f32::consts::TAU;
                    while p.rot[i] > std::f32::consts::PI {
                        p.rot[i] -= two_pi;
                    }
                    while p.rot[i] < -std::f32::consts::PI {
                        p.rot[i] += two_pi;
                    }
                }
            }
            let y_before = p.pos[1];
            p.pos[0] += p.vel[0] * dt;
            p.pos[1] += p.vel[1] * dt;
            p.pos[2] += p.vel[2] * dt;
            // `collisionquad`：有限矩形、**只从 +y 一面**碰（见 `CollideCfg` 的依据说明与
            // 坐标读法）。放在位置积分之后 —— 它是位置约束，不是力。
            // 判据用"**积分前**在前脸侧（`dy_before >= 0`）且本帧跨进了矩形"，
            // 这样贴面停住的粒子每帧会被重新接住（重力每帧都会重新给一点法向速度），
            // 而从背面来的粒子（`dy_before < 0`）永远碰不上 ⇒ 就是"只从一面"。
            if let (Some(c), Some(cc)) = (collide, collide_center) {
                if c.half[0] > 0.0 && c.half[1] > 0.0 {
                    let dy_before = y_before - cc[1];
                    let dy_after = p.pos[1] - cc[1];
                    if (p.pos[0] - cc[0]).abs() <= c.half[0] && dy_before >= 0.0 && dy_after <= c.half[1] {
                        p.pos[1] = cc[1];
                        match c.behavior {
                            1 => p.vel[1] = -p.vel[1],      // bounce：法向反号
                            2 => p.vel = [0.0, 0.0, 0.0],   // stop：全停
                            3 => p.age = p.life + 1.0,      // kill：交给下面的 retain 收走
                            _ => p.vel[1] = 0.0,            // slide：只去法向，切向保留
                        }
                        if c.stop_rot {
                            p.ang_vel = [0.0, 0.0, 0.0];
                        }
                    }
                }
            }
            let _ = life_mul;
        }
        self.particles.retain(|p| p.age < p.life);
    }

    fn spawn(&mut self, em: &Emitter) -> Particle {
        // 方向锥：directions 是各轴的**范围/掩膜**（`"1 0.1 0"` ⇒ y 轴只有 1/10 幅度）
        let dir = v3(&em.directions, [1.0, 1.0, 0.0]);
        let sign = v3(&em.sign, [0.0, 0.0, 0.0]);
        // `boxrandom` 的盒子是**逐轴**的；`sphererandom` 走"单位方向 × 半径"。
        // `WE_PARTICLE_BOX=legacy` 回旧行为（见 `offset` 的说明）。
        let box_mode = em.name == "boxrandom"
            && std::env::var("WE_PARTICLE_BOX").ok().as_deref() != Some("legacy");
        let (dmin, dmax) = (
            vec3_of(&em.distancemin, [0.0, 0.0, 0.0]),
            emitter_distancemax(em),
        );
        // 每轴一个随机量（含 sign 处理）——`turbulentvelocityrandom` 的初始方向也用它
        let mut d = [0.0f32; 3];
        if box_mode {
            // `boxrandom`（逐轴盒）：`d[c] = lerp(u∈[-1,1], min[c], max[c])`
            // 照 OWE `ParticleEmitter.cpp:194-210` 逐行；linux-WE
            // `CParticle.cpp:470-484` 的"逐轴 random(min,max) + 随机翻符号"同型。
            // ⚠ 旧实现是「随机向量**归一化** × 一个标量距离」⇒ 某一轴幅度小时（Ember 的
            // `directions "1 0.1 0"`）归一化会把**小轴放大到 ~1** ⇒ 盒子形状/半宽全错
            // （实测 `2961776805` 的 Ember y 展开 ±489，而配置推出只有 ±24）。
            for i in 0..3 {
                let t = self.rng.range(-1.0, 1.0);
                d[i] = dmin[i] + t * (dmax[i] - dmin[i]);
            }
        } else {
            d = [
                self.rng.range(-dir[0], dir[0]),
                self.rng.range(-dir[1], dir[1]),
                self.rng.range(-dir[2], dir[2]),
            ];
        }
        for i in 0..3 {
            if sign[i] != 0.0 {
                d[i] = d[i].abs() * sign[i].signum();
            }
        }
        // ⚠ 这次"距离"随机数**两条路都抽**（盒模式下不用它）：保持与旧路径**同样的随机数
        // 消耗序列**，否则后面所有初始化器的随机相位都会漂移，A/B 就分不清究竟是
        // "几何改了" 还是 "随机相位换了"。
        let dist = self.rng.range(num_of(&em.distancemin, 0.0), emitter_distancemax(em)[0]);
        let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt().max(0.0001);
        // 局部偏移（相对 `emitter.origin`）：`sphererandom` = 单位方向 × 半径
        // （`distancemin/distancemax` 的第 0 个分量，与旧行为逐位相同）；`boxrandom` = 逐轴。
        let offset = if box_mode {
            [d[0] * dir[0], d[1] * dir[1], d[2] * dir[2]]
        } else {
            [d[0] / len * dist, d[1] / len * dist, d[2] / len * dist]
        };
        let base_origin = v3(&em.origin, [0.0, 0.0, 0.0]);
        // 发射器初速：`normalize(局部偏移) × random(speedmin, speedmax)`（两份参照一致）。
        // `WE_PARTICLE_EMITTER_SPEED=0` 回旧行为（= 发射器不给初速）。
        // ⚠ 只在**声明了**初速时才抽随机数：绝大多数预设没有这两个字段，
        // 不抽 ⇒ 随机数序列与旧路径完全一致（改动面只剩"有初速的那几条"）。
        let mut vel = [0.0f32, 0.0, 0.0];
        if (em.speedmin != 0.0 || em.speedmax != 0.0)
            && std::env::var("WE_PARTICLE_EMITTER_SPEED").ok().as_deref() != Some("0")
        {
            let sp = self.rng.range(em.speedmin, em.speedmax);
            let n = len3(offset);
            if sp != 0.0 && n > 1e-6 {
                vel = [offset[0] / n * sp, offset[1] / n * sp, offset[2] / n * sp];
            }
        }

        // initializer
        let mut life = 1.0;
        let mut size = 20.0;
        let mut color = [1.0, 1.0, 1.0];
        let mut alpha0 = 1.0;
        // `vel` 已由发射器初速初始化（见上）；带速度的初始化器**叠加**在它上面
        // OWE 的 spawn pipeline 就是 `columns.velocities[slot] += value`
        // （`ParticleParser.cpp:311` 的 `Target::Velocity` 分支）。
        let mut rot = [0.0, 0.0, 0.0];
        let mut ang_vel = [0.0, 0.0, 0.0];
        for ini in &self.preset.initializer {
            match ini.name.as_str() {
                // 以前漏了这个 ⇒ alpha 落到默认 1.0。`Fog 1` 声明 0.15~0.2，
                // 12 片全不透明的大白雾就是这么糊满屏的（用户报的第 4 条）。
                "alpharandom" => {
                    alpha0 = self.rng.range(
                        v2f(&ini.min, [1.0, 1.0])[0],
                        v2f(&ini.max, [1.0, 1.0])[0],
                    )
                }
                "lifetimerandom" => life = self.rng.range(v2f(&ini.min, [1.0, 1.0])[0], v2f(&ini.max, [10.0, 10.0])[0]),
                // 量纲（见 `half_size` 字段说明）：参照实现把 sizerandom 的取值 **÷2** 再喂 shader。
                // 两份独立实现（linux-wallpaperengine / open-wallpaper-engine）+ WE 官方顶点 shader
                // 三方一致 ⇒ 屏幕全宽 = `sizerandom × sizeOverride ÷ 2`。
                "sizerandom" => {
                    let s = self.rng.range(v2f(&ini.min, [10.0, 10.0])[0], v2f(&ini.max, [50.0, 50.0])[0]);
                    size = if self.half_size { s / 2.0 } else { s };
                }
                "colorrandom" => {
                    // WE 的 colorrandom 是 0..255，得归一化
                    let a = v3j(&ini.min, [255.0, 255.0, 255.0]);
                    let b = v3j(&ini.max, [255.0, 255.0, 255.0]);
                    color = [
                        self.rng.range(a[0], b[0]) / 255.0,
                        self.rng.range(a[1], b[1]) / 255.0,
                        self.rng.range(a[2], b[2]) / 255.0,
                    ];
                }
                "velocityrandom" => {
                    let a = v3j(&ini.min, [0.0, 0.0, 0.0]);
                    let b = v3j(&ini.max, [0.0, 0.0, 0.0]);
                    vel = [
                        vel[0] + self.rng.range(a[0], b[0]),
                        vel[1] + self.rng.range(a[1], b[1]),
                        vel[2] + self.rng.range(a[2], b[2]),
                    ];
                }
                "turbulentvelocityrandom" => {
                    // ⚠ `speedmin`/`speedmax` 现在是 `Value`（`mapsequencearoundcontrolpoint` 用的是
                    // vec3 同名字段）⇒ 取标量走 `num_of`（数字 / 数字串 / 向量串取首项），
                    // 与旧的 `de_f32_lenient` **同值**。
                    // 音频响应：本机 `2481886649` 的 ember/torch 在这个初始化器上挂了
                    // `audioprocessingmode 3` / `bounds "0.8 1"` / `exponent 5`；没有频谱时
                    // `scale` 恒为 1.0（见 `AudioCfg`）。
                    // ══ 本次真根因 ：初速的**速度区间缺省** + `scale` 的语义 ══
                    // 旧实现取 `random(声明的 speedmin, 声明的 speedmax) × scale`，
                    // **键缺失 ⇒ 0** ⇒ `ember.json` / `ember_small.json` / `Stars_copy1.json`
                    // 这些只写 `offset`+`scale`、不写速度区间的预设拿到 `random(0,0)=0`
                    // ⇒ **初速恒 0**。后果（实测 `2961776805`）：粒子永远停在发射盒里
                    // （场景 y∈[-148.6,-99.7]，画布 y∈[0,1620]）⇒ 整层在画布外、0 像素。
                    // 两份参照**逐值一致**：缺省 `speedmin=100 / speedmax=250`；`scale`
                    // **不是速度倍率**，而是"方向偏离 `forward` 的锥角系数"
                    // （最终角 = `angle*scale*0.5 + offset`，见 [`rot_about`]）：
                    // linux-WE `ObjectParser.cpp:660-667` + `CParticle.cpp:817-915`
                    // OWE `ParticleParser.cpp:212-230` + `:320-348`
                    // 第三方旁证：`linux-wallpaperengine` 渲同一条壁纸时粒子铺满整帧。
                    // `WE_PARTICLE_TVR=legacy` 回旧行为做 A/B。
                    if std::env::var("WE_PARTICLE_TVR").ok().as_deref() == Some("legacy") {
                        let sp = self.rng.range(num_of(&ini.speedmin, 0.0), num_of(&ini.speedmax, 0.0))
                            * ini.scale.max(0.01)
                            * self.init_audio.scale(&self.audio);
                        vel = [
                            vel[0] + d[0] / len * sp,
                            vel[1] + d[1] / len * sp,
                            vel[2] + d[2] / len * sp,
                        ];
                    } else {
                        // 速度区间：**只有键缺失/null** 才用缺省 —— 显式 `0` 仍是 0
                        // （`torch.json` / `emberglow_small.json` 就是这么写的）。
                        let smin = opt_number(&ini.speedmin).unwrap_or(TVR_SPEEDMIN_DEFAULT);
                        let smax = opt_number(&ini.speedmax).unwrap_or(TVR_SPEEDMAX_DEFAULT);
                        let speed =
                            self.rng.range(smin, smax.max(smin)) * self.init_audio.scale(&self.audio);
                        // 方向基：`forward`（缺省"向上"）与旋转轴 `normal`（`right` 优先；OWE 两个键名都认）。
                        let f = unit3(v3(&ini.forward, [0.0, 1.0, 0.0]), [0.0, 1.0, 0.0]);
                        let n_raw = if !ini.right.trim().is_empty() {
                            v3(&ini.right, [0.0, 0.0, 1.0])
                        } else {
                            v3(&ini.normal, [0.0, 0.0, 1.0])
                        };
                        let n = unit3(n_raw, [0.0, 0.0, 1.0]);
                        let ts = ini.timescale.unwrap_or(1.0);
                        let phase =
                            self.rng.range(ini.phasemin.unwrap_or(0.0), ini.phasemax.unwrap_or(0.1));
                        // 采样点 = 累积场位置 + normal×phase（OWE `ParticleParser.cpp:334-336`）。
                        let sp3 = [
                            self.tvr_pos[0] + n[0] * phase,
                            self.tvr_pos[1] + n[1] * phase,
                            self.tvr_pos[2] + n[2] * phase,
                        ];
                        // 去掉法向分量再归一化（OWE `:337-341`）。
                        let mut c = curl_noise(sp3);
                        let dn = dot3(c, n);
                        for i in 0..3 {
                            c[i] -= n[i] * dn;
                        }
                        let c = if dot3(c, c) <= 1e-8 { f } else { unit3(c, f) };
                        // 从 `forward` 到噪声方向的**带符号**夹角 → 压成 `scale` 锥角 → 再加 `offset`。
                        let cosine = dot3(c, f).clamp(-1.0, 1.0);
                        let angle = dot3(n, cross3(f, c)).atan2(cosine);
                        let s = (ini.scale * 0.5).max(0.0);
                        let mut dir = rot_about(f, n, angle * s + ini.offset);
                        // 2D 粒子（`flags & 4 == 0`）把方向投到 XY 平面（linux-WE `CParticle.cpp:903-910`）。
                        if ((self.preset.flags as i32) & 4) == 0 {
                            dir[2] = 0.0;
                        }
                        let dir = unit3(dir, f);
                        vel = [
                            vel[0] + dir[0] * speed,
                            vel[1] + dir[1] * speed,
                            vel[2] + dir[2] * speed,
                        ];
                        // 噪声场自身推进（OWE `position += result * 0.005 * timescale`）。
                        for i in 0..3 {
                            self.tvr_pos[i] += c[i] * 0.005 * ts;
                        }
                    }
                }
                // `rotationrandom`：每颗粒子一个随机朝向。WE 的默认是 min (0,0,0)、
                // **max (0,0,2π)**（OWE `ParticleParser.cpp:519-522`）⇒ 不写字段时只有绕 z 的随机。
                // 这就是"树叶/雾片/火星为什么在 WE 里是乱七八糟的角度、在我们这儿全正着"的原因。
                "rotationrandom" if !legacy() => {
                    let a = v3j(&ini.min, [0.0, 0.0, 0.0]);
                    let b = v3j(&ini.max, [0.0, 0.0, std::f32::consts::TAU]);
                    rot = [
                        self.rng.range(a[0], b[0]),
                        self.rng.range(a[1], b[1]),
                        self.rng.range(a[2], b[2]),
                    ];
                }
                // `angularvelocityrandom`：默认 min (0,0,-5) / max (0,0,5)（OWE `ParticleParser.cpp:527-531`）。
                "angularvelocityrandom" if !legacy() => {
                    let a = v3j(&ini.min, [0.0, 0.0, -5.0]);
                    let b = v3j(&ini.max, [0.0, 0.0, 5.0]);
                    ang_vel = [
                        self.rng.range(a[0], b[0]),
                        self.rng.range(a[1], b[1]),
                        self.rng.range(a[2], b[2]),
                    ];
                }
                _ => {}
            }
        }
        // 场景覆盖：life/size/speed 是倍率
        life *= self.life_mul.max(0.0);
        size *= self.size_mul;
        vel = [
            vel[0] * self.speed_mul,
            vel[1] * self.speed_mul,
            vel[2] * self.speed_mul,
        ];
        color = [
            color[0] * self.color_mul[0],
            color[1] * self.color_mul[1],
            color[2] * self.color_mul[2],
        ];

        // 位置：场景坐标（origin + 局部偏移，再按对象变换放大/旋转）
        // 局部偏移 = emitter.origin + 上面算出的 `offset`（都在对象局部空间）。
        let local = [
            base_origin[0] + offset[0],
            base_origin[1] + offset[1],
            base_origin[2] + offset[2],
        ];
        // 对象自带 `angles` 时，发射点偏移必须跟着旋转（`emit_xform`，列主序 M[r][c]）。
        // 没给 `emit_xform` ⇒ 旧行为（只乘 scale、不旋转）。
        let mut pos = match self.emit_xform {
            Some(m3) => {
                let vx = m3[0] * local[0] + m3[1] * local[1] + m3[2] * local[2];
                let vy = m3[3] * local[0] + m3[4] * local[1] + m3[5] * local[2];
                let vz = m3[6] * local[0] + m3[7] * local[1] + m3[8] * local[2];
                // origin 已经是"取消那次 y 镜像"后的值 ⇒ 偏移的 y 也反号（同一套约定）
                [
                    self.origin[0] + vx,
                    self.origin[1] - vy,
                    self.origin[2] + vz,
                ]
            }
            None => [
                self.origin[0] + local[0] * self.scale[0],
                self.origin[1] + local[1] * self.scale[1],
                self.origin[2] + local[2],
            ],
        };
        // `mapsequencearoundcontrolpoint`（初始化器）：把粒子沿控制点周围的环按序号摆位，
        // 位置/速度都按 OWE 的公式（见 `MapSeqCfg`）。`seq_index` 是**跨粒子**计数器
        // （OWE 的 `SpawnSequence` / linux-WE 的 `sequenceIndex` 都是"每 spawn 一次 +1"）。
        if let Some(ms) = self.mapseq {
            let count = ms.count.max(1.0);
            let seq = self.seq_index;
            self.seq_index = ((self.seq_index + 1) as f32 % count) as i32;
            let two_pi = std::f32::consts::TAU;
            let mut angle = two_pi * (seq as f32) / count;
            angle = angle * (ms.bounds[1] - ms.bounds[0]) + two_pi * ms.bounds[0];
            let center = self.cp[ms.cp];
            let mut axis = ms.axis;
            let al = len3(axis);
            axis = if al > 1e-6 {
                [axis[0] / al, axis[1] / al, axis[2] / al]
            } else {
                [0.0, 0.0, 1.0]
            };
            // 与轴正交的基底（OWE `SequenceBasis`：轴偏 z 时取 y、否则取 x）
            let mut basis = if axis[2].abs() > 0.5 {
                [0.0, 1.0, 0.0]
            } else {
                [1.0, 0.0, 0.0]
            };
            let d = dot3(basis, axis);
            basis = [basis[0] - axis[0] * d, basis[1] - axis[1] * d, basis[2] - axis[2] * d];
            let bl = len3(basis);
            if bl > 1e-6 {
                basis = [basis[0] / bl, basis[1] / bl, basis[2] / bl];
            }
            let mut tangent = cross3(basis, axis);
            let tl = len3(tangent);
            if tl > 1e-6 {
                tangent = [tangent[0] / tl, tangent[1] / tl, tangent[2] / tl];
            }
            let rel = [pos[0] - center[0], pos[1] - center[1], pos[2] - center[2]];
            let par = dot3(rel, axis);
            let parallel = [axis[0] * par, axis[1] * par, axis[2] * par];
            let radial = [
                rel[0] - parallel[0],
                rel[1] - parallel[1],
                rel[2] - parallel[2],
            ];
            let radius = len3(radial);
            let (sa, ca) = (angle.sin(), angle.cos());
            pos = [
                center[0] + parallel[0] + radius * (ca * basis[0] + sa * tangent[0]),
                center[1] + parallel[1] + radius * (ca * basis[1] + sa * tangent[1]),
                center[2] + parallel[2] + radius * (ca * basis[2] + sa * tangent[2]),
            ];
            // 速度：把随机的速度向量绕轴转 -angle（OWE 的最后一步）
            if ms.speed_max != [0.0, 0.0, 0.0] || ms.speed_min != [0.0, 0.0, 0.0] {
                let v0 = [
                    self.rng.range(ms.speed_min[0], ms.speed_max[0]),
                    self.rng.range(ms.speed_min[1], ms.speed_max[1]),
                    self.rng.range(ms.speed_min[2], ms.speed_max[2]),
                ];
                let (s2, c2) = (-angle).sin_cos();
                let k = 1.0 - c2;
                let (x, y, z) = (axis[0], axis[1], axis[2]);
                // Rodrigues 旋转（绕任意轴转 -angle）
                let rot = [
                    v0[0] * (c2 + k * x * x) + v0[1] * (k * x * y - s2 * z) + v0[2] * (k * x * z + s2 * y),
                    v0[0] * (k * y * x + s2 * z) + v0[1] * (c2 + k * y * y) + v0[2] * (k * y * z - s2 * x),
                    v0[0] * (k * z * x - s2 * y) + v0[1] * (k * z * y + s2 * x) + v0[2] * (c2 + k * z * z),
                ];
                vel[0] += rot[0];
                vel[1] += rot[1];
                vel[2] += rot[2];
            }
        }
        let (freq, phase) = match self.osc {
            Some((fmin, fmax, _, _, _)) => (
                self.rng.range(fmin.max(0.01), fmax.max(0.01)),
                self.rng.range(0.0, 1.0),
            ),
            None => (0.0, 0.0),
        };
        // `oscillatealpha`：每粒子频率/相位（OWE `GenFrequency`：`phase = rand(phasemin, phasemax + 2π)`）
        let (alpha_freq, alpha_phase) = match self.osc_alpha {
            Some([fmin, fmax, _, _, pmin, pmax]) => (
                self.rng.range(fmin, fmax.max(fmin)),
                self.rng.range(pmin, pmax + std::f32::consts::TAU),
            ),
            None => (0.0, 0.0),
        };
        // 每颗粒子一个固定随机数：`animationmode:"randomframe"` 靠它把帧钉住
        let rand01 = self.rng.f();
        // `oscillatesize`：每粒子频率/相位（OWE `FrequencyValue::GenFrequency`：
        // `frequency = rand(fmin, fmax)`、`phase = rand(phasemin, phasemax + 2π)`）。
        let (size_freq, size_phase) = match self.osc_size {
            Some([fmin, fmax, _, _, pmin, pmax]) => (
                self.rng.range(fmin, fmax.max(fmin)),
                self.rng.range(pmin, pmax + std::f32::consts::TAU),
            ),
            None => (0.0, 0.0),
        };
        Particle {
            pos,
            vel,
            age: 0.0,
            life: life.max(0.01),
            size,
            color,
            alpha0: alpha0.clamp(0.0, 1.0),
            phase,
            freq,
            rot,
            ang_vel,
            alpha_freq,
            alpha_phase,
            rand: rand01,
            size_freq,
            size_phase,
        }
    }

    /// 生成粒子顶点（每个 17 个 f32，见 `gl_ctx::PARTICLE_FLOATS` 的布局说明）
    /// 诊断开关 `WE_PARTICLE_ALPHA_FORCE=<0..1>`（第三轮加的探针，**默认不开 ⇒ 逐字节不变**）：
    /// 把所有粒子的最终 alpha 钉成给定值。用途：区分「这层粒子没被发射/没画」与
    /// 「画了但 alpha 算成 0」——`WE_DEBUG_PARTICLES_BIG=1`（染红 + 20 倍尺寸）能让
    /// **alpha≠0** 的粒子显形，但强不了 alpha；两者一起用才能把 夏日影 「平整积雪」
    /// （`snowflat`，instanceoverride alpha=3.0/rate=0.6）那条「整层 0 像素」定位到 alpha。
    pub fn build(&self, out: &mut Vec<[f32; 17]>) {
        let aforce: Option<f32> = std::env::var("WE_PARTICLE_ALPHA_FORCE")
            .ok()
            .and_then(|v| v.parse::<f32>().ok());
        for p in &self.particles {
            let t = p.age / p.life;
            // alphafade：淡入 + 淡出（起点用出生 alpha，不再是写死的 1.0）
            let mut a = p.alpha0;
            if self.fadein > 0.0 {
                a *= (p.age / self.fadein).clamp(0.0, 1.0);
            }
            if self.fadeout > 0.0 {
                a *= ((p.life - p.age) / self.fadeout).clamp(0.0, 1.0);
            }
            a *= self.alpha;
            // oscillatealpha：乘在**上面那一串（alphafade 之后的 alpha）**上
            // 参照实现把它当作 base（每帧由 alphafade 刷新）再乘振荡系数。
            // `multiplier = mix(scalemin, scalemax, (cos(freq*age + phase) + 1) / 2)`。
            if let Some([_, _, smin, smax, _, _]) = self.osc_alpha {
                let w = p.alpha_freq;
                let cos01 = ((w * p.age + p.alpha_phase).cos() + 1.0) * 0.5;
                let m = smin + (smax - smin) * cos01;
                // blend-in/out：乘子按权重从恒等 1.0 插进来（`w>=1` 时原值直通）。
                a *= match self.blend_osc_alpha {
                    Some(b) => blend_mul(b.weight(t), m),
                    None => m,
                };
            }
            if let Some(x) = aforce {
                a = x;
            }
            if a <= 0.001 {
                continue;
            }
            // sizechange / colorchange：**与参照 `fadeValue` 完全一致** —— 端点钳制、无条件应用、
            // 后一个算子覆盖前一个（`CParticle.cpp:1159/1160`：`p.size = p.initial.size × fadeValue(life,…)`）。
            // ⚠ 之前我写成"窗口外回落到初值" ⇒ 火把尺寸在 20% 寿命后弹回 1.0、颜色从红弹回白。
            let mut size = p.size;
            for (st, en, sv, ev) in &self.size_ops {
                let f = if en > st {
                    ((t - st) / (en - st)).clamp(0.0, 1.0)
                } else if t >= *en {
                    1.0
                } else {
                    0.0
                };
                size = p.size * (sv + (ev - sv) * f);
            }
            // `oscillatesize`：乘在**上面算出的 size**（= sizechange 之后）上，
            // `multiplier = mix(scalemin, scalemax, (cos(freq×age + phase)+1)/2)` —— 与
            // `oscillatealpha` 同一套公式（OWE `OscillateScalarOperator` / linux-WE
            // `createOscillateSizeOperator`：`p.size = base × multiplier`，base 是"每帧刷新的 size"）。
            if let Some([_, _, smin, smax, _, _]) = self.osc_size {
                let cos01 = ((p.size_freq * p.age + p.size_phase).cos() + 1.0) * 0.5;
                let m = smin + (smax - smin) * cos01;
                // blend-in/out：同 `oscillatealpha`。
                size *= match self.blend_osc_size {
                    Some(b) => blend_mul(b.weight(t), m),
                    None => m,
                };
            }
            let mut col = p.color;
            for (st, en, sv, ev) in &self.color_ops {
                let f = if en > st {
                    ((t - st) / (en - st)).clamp(0.0, 1.0)
                } else if t >= *en {
                    1.0
                } else {
                    0.0
                };
                col = [
                    p.color[0] * (sv[0] + (ev[0] - sv[0]) * f),
                    p.color[1] * (sv[1] + (ev[1] - sv[1]) * f),
                    p.color[2] * (sv[2] + (ev[2] - sv[2]) * f),
                ];
            }
            // oscillateposition：按 mask 方向做正弦位移（简化）
            let mut pos = p.pos;
            if let Some((_, _, mask, smin, smax)) = self.osc {
                let amp0 = smin + (smax - smin) * 0.5;
                // blend-in/out：位移幅度按权重从 0 插进来。
                let amp = match self.blend_osc {
                    Some(b) => blend_add(b.weight(t), amp0),
                    None => amp0,
                };
                let s = (self.time * p.freq * std::f32::consts::TAU + p.phase * std::f32::consts::TAU).sin();
                pos[0] += mask[0] * s * amp;
                pos[1] += mask[1] * s * amp;
                pos[2] += mask[2] * s * amp;
            }
            let c0 = [col[0], col[1], col[2], a];
            // 隔离实验：WE_DEBUG_PARTICLES_BIG=1 时染红 + 放大 20 倍
            let (c0, size) = if std::env::var("WE_DEBUG_PARTICLES_BIG").is_ok() {
                ([1.0, 0.0, 0.0, 1.0], size * 20.0)
            } else {
                (c0, size)
            };
            // 四边形角（0/1）—— shader 用它既当纹理 uv 又当撑开方向
            let corners = [
                [0.0f32, 0.0f32],
                [1.0, 0.0],
                [1.0, 1.0],
                [0.0, 0.0],
                [1.0, 1.0],
                [0.0, 1.0],
            ];
            // 精灵表选帧用的"寿命进度"：`randomframe` ⇒ 每颗粒子固定的随机值（帧钉住），
            // 其余 ⇒ `frac(寿命比例 × sequencemultiplier)`。见 `frame_value`。
            let frame_life = self.frame_value(p);
            // `spritetrail`：`right/up` 由「眼位 + 速度」算（WE 官方
            // `common_particles.h:42-52` 的 `ComputeParticleTrailTangents` 逐行复刻），
            // 再把**对象缩放**作用到整个偏移上 —— 与下面 sprite 的 `quad_scale` 同一件事。
            if let Some([tlen, tmax, tmin]) = self.trail_cfg {
                let eye = std::env::var("WE_TRAIL_EYE")
                    .ok()
                    .and_then(|v| v.parse::<f32>().ok())
                    .unwrap_or(1.0e6);
                let sx = self.quad_scale.map(|s| s[0]).unwrap_or(1.0);
                let sy = self.quad_scale.map(|s| s[1]).unwrap_or(1.0);
                let vlen = len3(p.vel);
                // 速度为 0 ⇒ GPU 侧 `normalize(cross(...,0)) = normalize(0)`（未定义/NaN），
                // 本来就是一个像素都画不出来 ⇒ CPU 侧同样整颗跳过（不是"丢掉一颗星"）。
                if vlen > 1e-6 {
                    // eyeDirection = localPosition − mul(vec4(g_EyePosition,1), g_ModelMatrixInverse)
                    // （单位阵）—— 与 `gl_ctx::draw_batch` 里设的 `g_EyePosition` **同一个值**。
                    let ed = [pos[0], pos[1], pos[2] - eye];
                    let r = cross3(ed, p.vel);
                    let rl = len3(r);
                    if rl > 1e-9 {
                        let right = [r[0] / rl, r[1] / rl, r[2] / rl];
                        // up = 速度单位向量 × max(minlength, min(|v|×length, maxlength))
                        // （照 shader 逐字：`max(g_RenderVar0.z, min(trailLength*g_RenderVar0.x, g_RenderVar0.y))`
                        // 用 `max/min` 而不是 `clamp`：`maxlength < minlength` 时 clamp 会 panic，
                        // 而 shader 的写法只是取 minlength）
                        let l = tmin.max((vlen * tlen).min(tmax));
                        let up = [
                            p.vel[0] / vlen * l,
                            p.vel[1] / vlen * l,
                            p.vel[2] / vlen * l,
                        ];
                        for q in corners.iter() {
                            let (du, dv) = (q[0] - 0.5, q[1] - 0.5);
                            // position = P + size·right·(u−.5) − size·up·(v−.5)·ratio
                            let ox = size * (du * right[0] - self.tex_ratio * dv * up[0]);
                            let oy = size * (du * right[1] - self.tex_ratio * dv * up[1]);
                            let oz = size * (du * right[2] - self.tex_ratio * dv * up[2]);
                            out.push([
                                pos[0] + sx * ox,
                                pos[1] + sy * oy,
                                pos[2] + oz,
                                q[0],
                                q[1],
                                p.rot[2],
                                0.0, // size：已由 CPU 展开
                                p.rot[0],
                                p.rot[1],
                                p.vel[0],
                                p.vel[1],
                                p.vel[2],
                                frame_life,
                                c0[0],
                                c0[1],
                                c0[2],
                                c0[3],
                            ]);
                        }
                        continue;
                    }
                }
                continue;
            }
            // 新语义：CPU 侧把「对象缩放 × 粒子自转」展开成**场景空间的真实四角**，
            // 顶点里的 size 写 0 ⇒ 顶点 shader 的 `ComputeParticlePosition` 不再撑开
            // （`positionAndSize.w = 0` ⇒ 只返回 `a_Position`）。依据见 `quad_scale` 字段。
            if let Some([sx, sy]) = self.quad_scale {
                // 顶点 shader 的 `ComputeParticleTangents`：把它的 `mRotation` **逐字**在 CPU 侧
                // 重算（`g_OrientationRight/Up/Forward` = 单位阵；`mul(a,b)` 在我们的前导块里
                // 定义成 `(b)*(a)`；GLSL 的 `mat3(...)` 是**列主序填充** ⇒ 下面每个矩阵按行写出）：
                // mRotation = mul(mul(A_z, B_x), C_y) = C_y · B_x · A_z
                // right = mRotation·(1,0,0) = 第 0 列 ；up = mRotation·(0,1,0) = 第 1 列
                let (sxr, cxr) = p.rot[0].sin_cos();
                let (syr, cyr) = p.rot[1].sin_cos();
                let (szr, czr) = p.rot[2].sin_cos();
                let a = [[czr, szr, 0.0], [-szr, czr, 0.0], [0.0, 0.0, 1.0]];
                let b = [[1.0, 0.0, 0.0], [0.0, cxr, sxr], [0.0, -sxr, cxr]];
                let c = [[cyr, 0.0, -syr], [0.0, 1.0, 0.0], [syr, 0.0, cyr]];
                let mul3 = |m: [[f32; 3]; 3], n: [[f32; 3]; 3]| -> [[f32; 3]; 3] {
                    let mut o = [[0.0f32; 3]; 3];
                    for (i, row) in o.iter_mut().enumerate() {
                        for (j, cell) in row.iter_mut().enumerate() {
                            *cell = (0..3).map(|k| m[i][k] * n[k][j]).sum();
                        }
                    }
                    o
                };
                let m3 = mul3(c, mul3(b, a));
                // 右向量 / 上向量（列 0 / 列 1）
                let right = [m3[0][0], m3[1][0], m3[2][0]];
                let up = [m3[0][1], m3[1][1], m3[2][1]];
                for q in corners.iter() {
                    let (du, dv) = (q[0] - 0.5, q[1] - 0.5);
                    // position = p + size·right·(u−.5) − size·up·(v−.5)·ratio
                    let ox = size * (du * right[0] - self.tex_ratio * dv * up[0]);
                    let oy = size * (du * right[1] - self.tex_ratio * dv * up[1]);
                    out.push([
                        pos[0] + sx * ox,
                        pos[1] + sy * oy,
                        pos[2],
                        q[0],
                        q[1],
                        p.rot[2],
                        0.0, // size：已由 CPU 展开，shader 不得再乘一次
                        p.rot[0],
                        p.rot[1],
                        p.vel[0],
                        p.vel[1],
                        p.vel[2],
                        frame_life,
                        c0[0],
                        c0[1],
                        c0[2],
                        c0[3],
                    ]);
                }
                continue;
            }
            for q in corners.iter() {
                out.push([
                    pos[0],
                    pos[1],
                    pos[2],
                    q[0],
                    q[1],
                    p.rot[2],   // 旋转 z（a_TexCoordVec4.z）
                    size,       // 尺寸
                    p.rot[0],   // 旋转 x（a_TexCoordC2.x）
                    p.rot[1],   // 旋转 y（a_TexCoordC2.y）
                    p.vel[0],
                    p.vel[1],
                    p.vel[2],
                    frame_life, // a_TexCoordVec4C1.w：精灵表按它选帧
                    c0[0],
                    c0[1],
                    c0[2],
                    c0[3],
                ]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bubbles() -> ParticlePreset {
        serde_json::from_str(
            r#"{
            "emitter":[{"name":"sphererandom","origin":"0 0 0","directions":"1 1 0","distancemin":0,"distancemax":16,"rate":5}],
            "initializer":[
                {"name":"lifetimerandom","min":2,"max":5},
                {"name":"sizerandom","min":90,"max":250},
                {"name":"colorrandom","min":"255 255 255","max":"255 255 255"}
            ],
            "operator":[{"name":"movement","gravity":"0 100 0","drag":0.2},{"name":"alphafade","fadeintime":0.5}],
            "renderer":[{"name":"sprite"}],
            "material":"materials/presets/bubbles2.json",
            "maxcount":32
        }"#,
        )
        .unwrap()
    }

    #[test]
    fn parses_preset_and_spawns_at_rate() {
        let mut ps = ParticleSystem::new(bubbles(), [2000.0, 1368.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        // rate=5/s ⇒ 1 秒大约 5 个
        for _ in 0..30 {
            ps.update(1.0 / 30.0);
        }
        let n = ps.count();
        assert!((3..=7).contains(&n), "1 秒内应发射约 5 个，实际 {n}");
    }

    #[test]
    fn maxcount_caps_particles() {
        let mut ps = ParticleSystem::new(bubbles(), [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        for _ in 0..600 {
            ps.update(1.0 / 30.0); // 20 秒
        }
        assert!(ps.count() <= 32, "maxcount=32 必须封顶，实际 {}", ps.count());
    }

    #[test]
    fn instance_override_scales_rate_and_size() {
        let ov: InstanceOverride =
            serde_json::from_str(r#"{"alpha":0.8,"rate":0.21,"size":0.5,"colorn":"0.5 0.5 0.5"}"#)
                .unwrap();
        let mut ps = ParticleSystem::new(
            bubbles(),
            [0.0, 0.0, 0.0],
            [1.0, 1.0],
            &ov,
            1,
        );
        for _ in 0..60 {
            ps.update(1.0 / 30.0);
        }
        // rate 5 * 0.21 ≈ 1.05/s ⇒ 2 秒约 2 个
        let n = ps.count();
        assert!((1..=4).contains(&n), "override 后速率应明显变慢，实际 {n}");
        let mut verts = Vec::new();
        ps.build(&mut verts);
        if !verts.is_empty() {
            // colorrandom 是白的（0..255 归一化后 = 1.0），乘 0.5 之后应该是 0.5
            // （颜色在新的 17 浮点布局里位于下标 13..16）
            assert!((verts[0][13] - 0.5).abs() < 0.02, "颜色乘子没生效：{:?}", verts[0]);
        }
    }

    #[test]
    fn build_emits_six_vertices_per_particle_with_uv_in_unit_range() {
        let mut ps = ParticleSystem::new(bubbles(), [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 8);
        for _ in 0..90 {
            ps.update(1.0 / 30.0);
        }
        let mut verts = Vec::new();
        ps.build(&mut verts);
        assert_eq!(verts.len() % 6, 0);
        assert!(verts.len() > 0);
        for v in &verts {
            assert_eq!(v.len(), 17, "顶点必须是 17 个 f32");
            assert!(v[3] >= 0.0 && v[3] <= 1.0, "角 x 越界 {:?}", v);
            assert!(v[4] >= 0.0 && v[4] <= 1.0, "角 y 越界 {:?}", v);
            assert!(v[6] > 0.0, "尺寸必须为正（shader 用它撑开四边形）{:?}", v);
            assert!(v[12] >= 0.0 && v[12] <= 1.0, "寿命进度要在 0..1 {:?}", v);
        }
    }

    #[test]
    fn gravity_pulls_particles_down() {
        let mut ps = ParticleSystem::new(bubbles(), [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        // rate=5/s ⇒ 得先跑够时间才有粒子
        for _ in 0..15 {
            ps.update(1.0 / 30.0);
        }
        assert!(!ps.particles.is_empty(), "半秒后应该有粒子了");
        let y0 = ps.particles[0].pos[1];
        for _ in 0..30 {
            ps.update(1.0 / 30.0);
        }
        let p = &ps.particles[0];
        // 初速为 0（无 velocityrandom）⇒ 只受重力，y 应该往下（场景 y 向下为正）
        let dy = p.pos[1] - y0;
        assert!(dy > 0.0, "重力 100 应该把粒子往下拉，实际 dy={dy}");
    }

    /// 用 **min=max** 去掉随机性来钉住量纲：`sizerandom` 的值喂 shader 前要 ÷2（参照实现语义），
    /// override 是**乘数**。预设 sizerandom=100、override size=2 ⇒ 屏幕全宽 = 100×2÷2 = 100。
    #[test]
    fn sizerandom_is_halved_and_override_is_multiplier() {
        let preset: ParticlePreset = serde_json::from_str(
            r#"{
            "emitter":[{"name":"sphererandom","origin":"0 0 0","directions":"1 1 0","distancemin":0,"distancemax":1,"rate":100}],
            "initializer":[{"name":"lifetimerandom","min":5,"max":5},{"name":"sizerandom","min":100,"max":100}],
            "renderer":[{"name":"sprite"}],
            "maxcount":4
        }"#,
        )
        .unwrap();
        let ov: InstanceOverride = serde_json::from_str(r#"{"size":2.0}"#).unwrap();
        let mut ps = ParticleSystem::new(preset.clone(), [0.0, 0.0, 0.0], [1.0, 1.0], &ov, 1);
        for _ in 0..10 {
            ps.update(1.0 / 30.0);
        }
        let mut verts = Vec::new();
        ps.build(&mut verts);
        assert!(!verts.is_empty());
        // 顶点 idx 6 = shader 用的 size（屏幕全宽，场景单位）
        assert!((verts[0][6] - 100.0).abs() < 0.01, "全宽应=100（100×2÷2），实际 {}", verts[0][6]);
        // 旧语义（不 ÷2）回得到 200，A/B 开关必须真的起作用（要在出生前设，size 是出生时定的）
        let mut ps2 = ParticleSystem::new(preset, [0.0, 0.0, 0.0], [1.0, 1.0], &ov, 1);
        ps2.set_half_size(false);
        for _ in 0..10 {
            ps2.update(1.0 / 30.0);
        }
        let mut verts2 = Vec::new();
        ps2.build(&mut verts2);
        assert!((verts2[0][6] - 200.0).abs() < 0.01, "LEGACY 全宽应=200，实际 {}", verts2[0][6]);
    }

    /// 对象自带 `angles` 时，**发射点偏移**必须跟着旋转：给 M3 = Rz(90°)·I，emitter 局部偏移
    /// (100,0,0) 应旋到 y 方向 ⇒ 世界 pos.y = origin.y − 100（粒子空间 y 与图层相反，取负）。
    #[test]
    fn emitter_offset_follows_object_rotation() {
        let preset: ParticlePreset = serde_json::from_str(
            r#"{
            "emitter":[{"name":"sphererandom","origin":"100 0 0","directions":"1 1 1","distancemin":0,"distancemax":0,"rate":100}],
            "initializer":[{"name":"lifetimerandom","min":5,"max":5},{"name":"sizerandom","min":10,"max":10}],
            "renderer":[{"name":"sprite"}],
            "maxcount":2
        }"#,
        )
        .unwrap();
        let origin = [1000.0f32, 500.0, 0.0];
        let mut ps = ParticleSystem::new(preset, origin, [1.0, 1.0], &InstanceOverride::default(), 1);
        // 列主序 M[r][c]：(c*4+r) → 行主序 [r*3+c]
        // Rz(90°) = [[0,-1,0],[1,0,0],[0,0,1]]
        ps.set_emit_xform([0.0, -1.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0]);
        for _ in 0..5 {
            ps.update(1.0 / 30.0);
        }
        assert!(!ps.particles.is_empty());
        let p = ps.particles[0];
        assert!((p.pos[0] - origin[0]).abs() < 0.01, "x 不该有偏移：{}", p.pos[0]);
        assert!((p.pos[1] - (origin[1] - 100.0)).abs() < 0.01, "y 应=origin.y−100（旋转生效），实际 {}", p.pos[1]);
        // 关掉 xform（旧行为）：偏移只乘 scale ⇒ y 偏移 = +100
        let preset2: ParticlePreset = serde_json::from_str(
            r#"{
            "emitter":[{"name":"sphererandom","origin":"100 0 0","directions":"1 1 1","distancemin":0,"distancemax":0,"rate":100}],
            "initializer":[{"name":"lifetimerandom","min":5,"max":5},{"name":"sizerandom","min":10,"max":10}],
            "renderer":[{"name":"sprite"}],
            "maxcount":2
        }"#,
        )
        .unwrap();
        let mut ps2 = ParticleSystem::new(preset2, origin, [1.0, 1.0], &InstanceOverride::default(), 1);
        for _ in 0..5 {
            ps2.update(1.0 / 30.0);
        }
        // 旧行为：偏移只乘 scale（不旋转）⇒ (100,0,0)*scale=(100,0,0) ⇒ x 偏 +100、y 不变
        assert!((ps2.particles[0].pos[0] - (origin[0] + 100.0)).abs() < 0.01, "旧行为 x 应 +100");
        assert!((ps2.particles[0].pos[1] - origin[1]).abs() < 0.01, "旧行为 y 不该动");
    }

    /// `instanceoverride` 里的**绑定对象**（`{"user":…,"value":…}`）不能把整条覆盖打掉。
    /// 出处：OWE `ParticleInstanceoverride` 对 alpha/count/lifetime/rate/speed/size/colorn
    /// **逐个**支持 `{"user":…}` 绑定（`ParticleObject.cppm:162-196`）。
    /// 实测背景：`2080534540` 的雨 `count` 写成 `{"user":"_6","value":0.95}`，旧实现因为这个
    /// 字段类型不对 ⇒ **整条 override 丢光** ⇒ 作者给的 `size 0.68 / speed 0.78` 全部失效
    /// （屏幕上量到尺寸正好是"没乘 0.68"的 p50 600 / p90 760，速度正好是"没乘 0.78"的 ~4200 单位/秒）。
    #[test]
    fn instance_override_survives_bound_object_fields() {
        let ov: InstanceOverride = serde_json::from_str(
            r#"{"count":{"user":"_6","value":0.95},"id":200,"lifetime":0.86,"rate":0.58,
                "size":0.68,"speed":0.78,
                "colorn":{"user":"schemecolor","value":"0.5 0.25 1"}}"#,
        )
        .unwrap();
        assert_eq!(ov.count, Some(0.95), "绑定对象里的 value 要取出来");
        assert_eq!(ov.lifetime, Some(0.86));
        assert_eq!(ov.rate, Some(0.58));
        assert_eq!(ov.size, Some(0.68));
        assert_eq!(ov.speed, Some(0.78));
        assert_eq!(ov.colorn.as_deref(), Some("0.5 0.25 1"), "colorn 也可能是绑定对象");
    }

    /// 绑定对象修好之后，`size`/`speed`/`count` 的**乘数**语义必须逐项成立
    /// （min=max 去掉随机性）：`sizerandom 1000 × 0.68 ÷ 2 = 340`、`velocity -1000 × 0.78`、
    /// `maxcount 4 × 0.95 = 3.8 → 3`。
    #[test]
    fn bound_override_still_multiplies_size_speed_and_count() {
        let preset: ParticlePreset = serde_json::from_str(
            r#"{
            "emitter":[{"name":"sphererandom","origin":"0 0 0","directions":"1 1 0","distancemin":0,"distancemax":1,"rate":600}],
            "initializer":[{"name":"lifetimerandom","min":5,"max":5},{"name":"sizerandom","min":1000,"max":1000},
                           {"name":"velocityrandom","min":"0 -1000 0","max":"0 -1000 0"}],
            "renderer":[{"name":"sprite"}],
            "maxcount":4
        }"#,
        )
        .unwrap();
        let ov: InstanceOverride = serde_json::from_str(
            r#"{"count":{"user":"_6","value":0.95},"lifetime":0.86,"rate":0.58,"size":0.68,"speed":0.78}"#,
        )
        .unwrap();
        let mut ps = ParticleSystem::new(preset, [0.0, 0.0, 0.0], [1.0, 1.0], &ov, 1);
        for _ in 0..10 {
            ps.update(1.0 / 30.0);
        }
        assert!(!ps.particles.is_empty());
        let p = ps.particles[0];
        assert!((p.size - 340.0).abs() < 0.01, "尺寸应 = 1000×0.68÷2 = 340，实际 {}", p.size);
        assert!((p.vel[1] + 780.0).abs() < 0.01, "速度应 = −1000×0.78，实际 {}", p.vel[1]);
        assert_eq!(ps.capacity(), 3, "count override 0.95 应生效（4×0.95=3.8→3）");
    }

    /// 精灵表选帧的两条语义（WE 官方顶点 shader `ComputeSpriteFrame` 用 `a_TexCoordVec4C1.w`）：
///
    /// * `animationmode:"randomframe"` ⇒ 每颗粒子**钉在一帧**（OWE `RANDOMONE` 分支写 `value.random`）；
///
    /// * 其余 ⇒ `frac(寿命比例 × sequencemultiplier)`（OWE `SEQUENCE` 分支 / linux-WE `animSpeed`）。
    #[test]
    fn sprite_frame_is_frozen_for_randomframe_and_scales_for_sequence() {
        let mk = |anim: &str, seq: &str| {
            serde_json::from_str::<ParticlePreset>(&format!(
                r#"{{
                "emitter":[{{"name":"sphererandom","origin":"0 0 0","directions":"1 1 0","distancemin":0,"distancemax":1,"rate":600}}],
                "initializer":[{{"name":"lifetimerandom","min":10,"max":10}},{{"name":"sizerandom","min":10,"max":10}}],
                "renderer":[{{"name":"sprite"}}],"maxcount":12,
                "animationmode":{anim},"sequencemultiplier":{seq}}}"#
            ))
            .unwrap()
        };
        // randomframe：帧值 == 该粒子出生时抽的固定随机数（与年龄无关）
        let mut ps = ParticleSystem::new(mk("\"randomframe\"", "null"), [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 4);
        for _ in 0..90 {
            ps.update(1.0 / 30.0);
        }
        assert!(ps.particles.len() >= 4, "要有足够粒子做统计");
        let mut bins = std::collections::BTreeSet::new();
        for p in ps.particles.clone() {
            assert!((ps.frame_value(&p) - p.rand).abs() < 1e-6, "randomframe 必须写每粒子的固定随机数");
            assert!((0.0..1.0).contains(&p.rand));
            bins.insert((p.rand * 4.0) as i32);
        }
        assert!(bins.len() >= 2, "4 帧精灵表上应当落在不同的帧上：{bins:?}");
        // sequence（未声明 animationmode）+ sequencemultiplier=2 ⇒ 帧值 = frac(age/life × 2)，随时间推进
        let mut qs = ParticleSystem::new(mk("null", "2"), [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 4);
        for _ in 0..3 {
            qs.update(1.0 / 30.0);
        }
        let p0 = qs.particles[0];
        let want = (p0.age / p0.life * 2.0).fract();
        assert!((qs.frame_value(&p0) - want).abs() < 1e-6, "sequence 的帧值 = frac(age/life×mult)");
        for _ in 0..30 {
            qs.update(1.0 / 30.0);
        }
        let p1 = qs.particles[0];
        assert!(qs.frame_value(&p1) > qs.frame_value(&p0), "寿命内帧值必须前进");
    }

    /// `turbulence`：Perlin curl noise 只沿 `mask` 推速度，且不能出 NaN/Inf。
    /// 公式照 OWE `TurbulenceOperator`（`speed * speedOverride * normalize(curlNoise((pos+(phase+timescale*t))·scale·2)) · mask · dt`）。
    #[test]
    fn turbulence_pushes_velocity_only_along_the_mask() {
        let preset: ParticlePreset = serde_json::from_str(
            r#"{
            "emitter":[{"name":"sphererandom","origin":"0 0 0","directions":"1 1 0","distancemin":0,"distancemax":50,"rate":600}],
            "initializer":[{"name":"lifetimerandom","min":10,"max":10},{"name":"sizerandom","min":10,"max":10}],
            "operator":[{"name":"movement","gravity":"0 0 0"},
                        {"name":"turbulence","mask":"1 0 0","scale":0.002,"speedmin":100,"speedmax":150,"phasemax":5}],
            "renderer":[{"name":"sprite"}],"maxcount":8
        }"#,
        )
        .unwrap();
        let mut ps = ParticleSystem::new(preset, [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        for _ in 0..60 {
            ps.update(1.0 / 30.0);
        }
        assert!(ps.particles.iter().all(|p| p.vel[1] == 0.0 && p.vel[2] == 0.0), "mask=1 0 0 ⇒ y/z 速度必须原样保持 0");
        assert!(ps.particles.iter().any(|p| p.vel[0].abs() > 1.0), "湍流应当真的推动 x 速度");
        assert!(
            ps.particles.iter().all(|p| p.vel.iter().all(|v| v.is_finite())),
            "噪声必须有限（不能 NaN/Inf）"
        );
        // 噪声本身：Perlin 取值范围约 [-1,1]，curl 不得为 NaN
        for i in 0..50 {
            let v = perlin_noise(i as f32 * 0.37, i as f32 * 0.11, 0.5);
            assert!(v.is_finite() && v.abs() <= 1.5, "perlin_noise 越界：{v}");
        }
        assert!(curl_noise([1.5, -2.25, 0.75]).iter().all(|v| v.is_finite()));
    }

    /// `oscillatealpha`：乘在 alphafade 之后的 alpha 上（`multiplier = mix(scalemin, scalemax, (cos(freq·age+phase)+1)/2)`）。
    /// 用 `scalemin == scalemax == 0.5` 把振荡钉成常数 ⇒ 顶点 alpha 必须正好是出生 alpha 的一半。
    #[test]
    fn oscillatealpha_multiplies_alpha_and_stays_in_scale_range() {
        let mk = |smin: &str, smax: &str| {
            serde_json::from_str::<ParticlePreset>(&format!(
                r#"{{
                "emitter":[{{"name":"sphererandom","origin":"0 0 0","directions":"1 1 0","distancemin":0,"distancemax":1,"rate":600}}],
                "initializer":[{{"name":"lifetimerandom","min":10,"max":10}},{{"name":"sizerandom","min":10,"max":10}},
                               {{"name":"alpharandom","min":0.8,"max":0.8}}],
                "operator":[{{"name":"oscillatealpha","frequencymin":5,"frequencymax":5,"scalemin":{smin},"scalemax":{smax}}}],
                "renderer":[{{"name":"sprite"}}],"maxcount":6}}"#
            ))
            .unwrap()
        };
        let mut ps = ParticleSystem::new(mk("0.5", "0.5"), [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        for _ in 0..30 {
            ps.update(1.0 / 30.0);
        }
        let mut v = Vec::new();
        ps.build(&mut v);
        assert!(!v.is_empty());
        assert!((v[0][16] - 0.4).abs() < 0.01, "alpha 应 = 0.8×0.5 = 0.4，实际 {}", v[0][16]);
        // scalemin != scalemax ⇒ 随时间摆动（同一颗粒子在两个时刻的 alpha 不同）
        let mut qs = ParticleSystem::new(mk("0.2", "1.0"), [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        for _ in 0..3 {
            qs.update(1.0 / 30.0);
        }
        let mut a1 = Vec::new();
        qs.build(&mut a1);
        for _ in 0..3 {
            qs.update(1.0 / 30.0);
        }
        let mut a2 = Vec::new();
        qs.build(&mut a2);
        assert!(!a1.is_empty() && !a2.is_empty());
        assert!((a1[0][16] - a2[0][16]).abs() > 1e-3, "alpha 应当随时间摆动：{} vs {}", a1[0][16], a2[0][16]);
        for a in [a1[0][16], a2[0][16]] {
            assert!((0.16..=0.8 + 1e-3).contains(&a), "alpha 必须落在 [0.8×0.2, 0.8×1.0] 内：{a}");
        }
    }

    /// `rotationrandom` + `angularmovement`：角速度积分成旋转，写进顶点；**没被驱动的轴必须原样是 0**
    /// 。
    #[test]
    fn angular_movement_integrates_rotation_and_leaves_other_axes_alone() {
        let preset: ParticlePreset = serde_json::from_str(
            r#"{
            "emitter":[{"name":"sphererandom","origin":"0 0 0","directions":"1 1 0","distancemin":0,"distancemax":1,"rate":600}],
            "initializer":[{"name":"lifetimerandom","min":10,"max":10},{"name":"sizerandom","min":10,"max":10},
                           {"name":"angularvelocityrandom","min":"0 0 2","max":"0 0 2"},
                           {"name":"rotationrandom","min":"0 0 0","max":"0 0 0"}],
            "operator":[{"name":"angularmovement","force":"0 0 0"}],
            "renderer":[{"name":"sprite"}],"maxcount":4
        }"#,
        )
        .unwrap();
        let mut ps = ParticleSystem::new(preset, [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        // 每 3 帧更新一次、记下"第一颗粒子的 rot.z ≈ 2×age"
        let mut checked = 0;
        for _ in 0..30 {
            ps.update(1.0 / 30.0);
            if let Some(p) = ps.particles.first() {
                assert!((p.rot[2] - 2.0 * p.age).abs() < 1e-3, "rot.z 应 = ω·age = 2×{}，实际 {}", p.age, p.rot[2]);
                assert_eq!((p.rot[0], p.rot[1]), (0.0, 0.0), "没被驱动的轴必须保持 0");
                assert_eq!((p.ang_vel[0], p.ang_vel[1]), (0.0, 0.0));
                // 顶点里：idx5 = 旋转 z、idx7/idx8 = 旋转 x/y
                let mut v = Vec::new();
                ps.build(&mut v);
                assert!((v[0][5] - p.rot[2]).abs() < 1e-6);
                assert_eq!((v[0][7], v[0][8]), (0.0, 0.0));
                checked += 1;
            }
        }
        assert!(checked > 5, "应当检查到足够多帧");
    }

    /// `spritetrail` 的默认值照 linux-WE `ObjectParser::parseParticleRenderer`
    /// （length 0.05 / maxlength 10 / minlength 0）；`sprite` 不是拖尾 ⇒ None。
    #[test]
    fn spritetrail_defaults_match_the_reference() {
        let t = |json: &str| {
            serde_json::from_str::<ParticlePreset>(json)
                .unwrap()
                .trail()
        };
        assert_eq!(
            t(r#"{"renderer":[{"name":"spritetrail","length":0.007}],"maxcount":1}"#),
            Some([0.007, 10.0, 0.0]),
            "缺 maxlength 时用 WE 默认 10"
        );
        assert_eq!(
            t(r#"{"renderer":[{"name":"spritetrail"}],"maxcount":1}"#),
            Some([0.05, 10.0, 0.0]),
            "缺 length 时用 WE 默认 0.05"
        );
        assert_eq!(t(r#"{"renderer":[{"name":"sprite"}],"maxcount":1}"#), None);
        assert_eq!(
            t(r#"{"renderer":[{"name":"spritetrail","length":0.008,"maxlength":20,"minlength":0.5}],"maxcount":1}"#),
            Some([0.008, 20.0, 0.5])
        );
    }

    /// 预设级 `starttime` = 延迟出场：到点之前**一颗都不发**，到点之后才开始（WE 语义）
    /// OWE `SceneParticleObjectParser` 把 `particle_obj.starttime` 交给子系统）。
    #[test]
    fn starttime_delays_spawn_and_animation() {
        let preset: ParticlePreset = serde_json::from_str(
            r#"{
            "emitter":[{"name":"sphererandom","origin":"0 0 0","directions":"1 1 0","distancemin":0,"distancemax":1,"rate":600}],
            "initializer":[{"name":"lifetimerandom","min":10,"max":10},{"name":"sizerandom","min":10,"max":10}],
            "renderer":[{"name":"sprite"}],"maxcount":16,"starttime":1
        }"#,
        )
        .unwrap();
        let mut ps = ParticleSystem::new(preset, [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        for _ in 0..29 {
            ps.update(1.0 / 30.0); // 0.967 秒 < starttime 1
        }
        assert_eq!(ps.count(), 0, "starttime 之前不能有粒子");
        for _ in 0..30 {
            ps.update(1.0 / 30.0);
        }
        assert!(ps.count() > 0, "starttime 之后必须开始发射");
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // 本次新增算子/初始化器的单测（依据/默认值写在每条上面；`*Cfg` 里有完整出处）
    // ══════════════════════════════════════════════════════════════════════════════

    /// `oscillatesize`：`multiplier = mix(scalemin, scalemax, (cos(freq×age+phase)+1)/2)`
    /// （OWE `OscillateScalarOperator` Target::Size / linux-WE `createOscillateSizeOperator`）。
    /// `scalemin == scalemax == 0.5` ⇒ 顶点 size 必须正好是出生 size 的一半（出生 size = 100÷2 = 50）。
    #[test]
    fn oscillatesize_multiplies_size_and_stays_in_scale_range() {
        let mk = |smin: &str, smax: &str| {
            serde_json::from_str::<ParticlePreset>(&format!(
                r#"{{
                "emitter":[{{"name":"sphererandom","origin":"0 0 0","directions":"1 1 0","distancemin":0,"distancemax":1,"rate":600}}],
                "initializer":[{{"name":"lifetimerandom","min":10,"max":10}},{{"name":"sizerandom","min":100,"max":100}}],
                "operator":[{{"name":"oscillatesize","frequencymin":5,"frequencymax":5,"scalemin":{smin},"scalemax":{smax}}}],
                "renderer":[{{"name":"sprite"}}],"maxcount":6}}"#
            ))
            .unwrap()
        };
        let mut ps = ParticleSystem::new(mk("0.5", "0.5"), [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        for _ in 0..30 {
            ps.update(1.0 / 30.0);
        }
        let mut v = Vec::new();
        ps.build(&mut v);
        assert!(!v.is_empty());
        assert!((v[0][6] - 25.0).abs() < 0.01, "size 应 = 50×0.5 = 25，实际 {}", v[0][6]);
        // scalemin != scalemax ⇒ 同一颗粒子在不同时刻的 size 不同，且始终落在 [50×0.2, 50×1.0]
        let mut qs = ParticleSystem::new(mk("0.2", "1.0"), [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        for _ in 0..3 {
            qs.update(1.0 / 30.0);
        }
        let mut a1 = Vec::new();
        qs.build(&mut a1);
        let first = qs.particles[0].size_freq;
        for _ in 0..3 {
            qs.update(1.0 / 30.0);
        }
        let mut a2 = Vec::new();
        qs.build(&mut a2);
        assert!((a1[0][6] - a2[0][6]).abs() > 1e-3, "size 应当随时间摆动：{} vs {}", a1[0][6], a2[0][6]);
        for s in [a1[0][6], a2[0][6]] {
            assert!((10.0 - 1e-3..=50.0 + 1e-3).contains(&s), "size 必须落在 [50×0.2, 50×1.0] 内：{s}");
        }
        // 每粒子频率来自 [frequencymin, frequencymax]（这里两者都是 5）
        assert!((first - 5.0).abs() < 1e-6);
    }

    /// `vortex`：绕轴 `axis` 的**切向**力（linux-WE `CParticleViewer` 的 `tangent = cross(axis, radial)`）。
    /// 轴为 +z、粒子在 xy 平面半径 100 ⇒ 角动量 `cross(radial, vel)·z` 必须全为正且速度真的涨起来。
    #[test]
    fn vortex_pushes_tangentially_around_the_axis() {
        let preset: ParticlePreset = serde_json::from_str(
            r#"{
            "emitter":[{"name":"sphererandom","origin":"0 0 0","directions":"1 1 0","distancemin":100,"distancemax":100,"rate":600}],
            "initializer":[{"name":"lifetimerandom","min":100,"max":100},{"name":"sizerandom","min":10,"max":10}],
            "operator":[{"name":"vortex","controlpoint":0,"axis":"0 0 1","distanceinner":0,"distanceouter":200,"speedinner":100,"speedouter":100}],
            "renderer":[{"name":"sprite"}],"maxcount":8
        }"#,
        )
        .unwrap();
        let mut ps = ParticleSystem::new(preset, [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        for _ in 0..30 {
            ps.update(1.0 / 30.0);
        }
        assert!(ps.particles.len() >= 4, "要有足够粒子做统计");
        for p in &ps.particles {
            let ang = p.pos[0] * p.vel[1] - p.pos[1] * p.vel[0];
            assert!(ang > 0.0, "角动量必须与 cross(axis, radial) 同号（正）: {ang} pos={:?} vel={:?}", p.pos, p.vel);
            assert!(p.vel.iter().all(|v| v.is_finite()), "涡旋不能产生 NaN/Inf");
        }
        assert!(ps.particles.iter().any(|p| (p.vel[0] * p.vel[0] + p.vel[1] * p.vel[1]).sqrt() > 20.0), "切向速度应当真的涨起来");
    }

    /// `controlpointattract`（OWE 公式）：`scale > 0` 吸引、`scale < 0` 排斥；超出 `threshold` 不动。
    #[test]
    fn controlpointattract_pulls_inside_threshold_and_pushes_when_negative() {
        let mk = |scale: &str, threshold: &str| {
            serde_json::from_str::<ParticlePreset>(&format!(
                r#"{{
                "controlpoint":[{{"id":0,"offset":"0 0 0","flags":0}}],
                "emitter":[{{"name":"sphererandom","origin":"0 0 0","directions":"1 1 0","distancemin":200,"distancemax":200,"rate":600}}],
                "initializer":[{{"name":"lifetimerandom","min":100,"max":100}},{{"name":"sizerandom","min":10,"max":10}}],
                "operator":[{{"name":"controlpointattract","controlpoint":0,"origin":"0 0 0","scale":{scale},"threshold":{threshold}}}],
                "renderer":[{{"name":"sprite"}}],"maxcount":6}}"#
            ))
            .unwrap()
        };
        let radial_dot = |ps: &ParticleSystem| -> f32 {
            let p = &ps.particles[0];
            p.pos[0] * p.vel[0] + p.pos[1] * p.vel[1] + p.pos[2] * p.vel[2]
        };
        // scale = +500（吸引）：径向速度分量必须为负（朝控制点）
        let mut pull = ParticleSystem::new(mk("500", "512"), [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        for _ in 0..30 {
            pull.update(1.0 / 30.0);
        }
        assert!(radial_dot(&pull) < 0.0, "吸引时径向速度应为负，实际 {}", radial_dot(&pull));
        // scale = −500（排斥）：径向速度分量为正（远离）
        let mut push = ParticleSystem::new(mk("-500", "512"), [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        for _ in 0..30 {
            push.update(1.0 / 30.0);
        }
        assert!(radial_dot(&push) > 0.0, "排斥时径向速度应为正，实际 {}", radial_dot(&push));
        // 粒子在 threshold 之外（半径 200 > 64）⇒ 一颗都不受影响
        let mut far = ParticleSystem::new(mk("-500", "64"), [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        for _ in 0..30 {
            far.update(1.0 / 30.0);
        }
        assert!(far.particles.iter().all(|p| p.vel == [0.0, 0.0, 0.0]), "threshold 之外不该给速度");
    }

    /// `boids` 的对齐项：因子>0 时速度必须**收敛到一致**（这是编辑器文案 "matches their velocity" 的核心）。
    #[test]
    fn boids_alignment_makes_velocities_converge() {
        let mk = |align: &str| {
            serde_json::from_str::<ParticlePreset>(&format!(
                r#"{{
                "emitter":[{{"name":"sphererandom","origin":"0 0 0","directions":"1 1 0","distancemin":0,"distancemax":20,"rate":600}}],
                "initializer":[{{"name":"lifetimerandom","min":100,"max":100}},{{"name":"sizerandom","min":10,"max":10}},
                               {{"name":"velocityrandom","min":"-100 -100 0","max":"100 100 0"}}],
                "operator":[{{"name":"boids","alignmentfactor":{align},"cohesionfactor":0,"separationfactor":0,"neighborthreshold":1000}}],
                "renderer":[{{"name":"sprite"}}],"maxcount":12}}"#
            ))
            .unwrap()
        };
        let spread = |ps: &ParticleSystem| -> f32 {
            let n = ps.particles.len() as f32;
            let (mx, my) = ps.particles.iter().fold((0.0f32, 0.0f32), |a, p| (a.0 + p.vel[0], a.1 + p.vel[1]));
            let (mx, my) = (mx / n, my / n);
            (ps.particles.iter().map(|p| (p.vel[0] - mx).powi(2) + (p.vel[1] - my).powi(2)).sum::<f32>() / n).sqrt()
        };
        let mut on = ParticleSystem::new(mk("10"), [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        for _ in 0..5 {
            on.update(1.0 / 30.0);
        }
        for _ in 0..60 {
            on.update(1.0 / 30.0);
        }
        let s1 = spread(&on);
        // 参照：同一份预设、`alignmentfactor = 0`（= 空转）在同一时刻的分散度。
        // boids 不抽随机数 ⇒ 两套系统的粒子序列逐颗粒子同值 ⇒ 可以直接比。
        let mut off = ParticleSystem::new(mk("0"), [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        for _ in 0..5 {
            off.update(1.0 / 30.0);
        }
        let o0 = spread(&off);
        for _ in 0..60 {
            off.update(1.0 / 30.0);
        }
        let o1 = spread(&off);
        assert!(o0 > 10.0, "初始速度得有分散度（否则这个测试没意义）：{o0}");
        assert!(s1 < o0 * 0.5, "对齐项必须让速度收敛：空转 {o0} → 对齐 {s1}");
        assert!((o1 - o0).abs() < 1e-3, "factor=0 时 boids 必须是空转（速度不该动）：{o0} → {o1}");
    }

    /// `boids` 的**三条力各自的判据**（N=24 颗粒子、初始聚在一起、邻域半径大到互相能看见）。
    /// 这是本次"合成最小场景"在单测里的等价物：
    /// ① 三个系数全 0 ⇒ **逐位空转**（位置/速度一个字节都不动）；
    /// ② 只有**聚集**（初速 0）⇒ 相对质心的均方根半径**单调收缩**；
    /// ③ 只有**分离**（初速 0、初始间距远小于分离阈值）⇒ 最近邻距离被推大、分离窗口内配对数归零；
    /// 对齐项由 `boids_alignment_makes_velocities_converge` 覆盖（同向度 → 1、速度散布 → 0）。
    /// 三条力都是**加在速度上**（`v += …×dt`），位置由同一帧末尾 `pos += v·dt` 积分。
    mod boids_forces {
        use super::*;

        /// 24 颗粒子：rate 极大 ⇒ 第 1 帧全放出来；初速 0（除非显式给）；
        /// `distancemax` 决定初始有多聚；`neighborthreshold` 200、`separationthreshold` 30。
        fn mk(distmax: f32, vspread: f32, a: f32, c: f32, s: f32) -> ParticlePreset {
            serde_json::from_str::<ParticlePreset>(&format!(
                r#"{{
                "emitter":[{{"name":"sphererandom","origin":"0 0 0","directions":"1 1 0","distancemin":0,"distancemax":{distmax},"rate":100000}}],
                "initializer":[{{"name":"lifetimerandom","min":1000,"max":1000}},{{"name":"sizerandom","min":10,"max":10}},
                               {{"name":"velocityrandom","min":"-{vspread} -{vspread} 0","max":"{vspread} {vspread} 0"}}],
                "operator":[{{"name":"movement"}},
                            {{"name":"boids","alignmentfactor":{a},"cohesionfactor":{c},"separationfactor":{s},
                              "neighborthreshold":200,"separationthreshold":30}}],
                "renderer":[{{"name":"sprite"}}],"maxcount":24}}"#
            ))
            .unwrap()
        }
        fn sys(p: ParticlePreset) -> ParticleSystem {
            ParticleSystem::new(p, [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1)
        }
        /// 相对质心的均方根半径（"集群有多大"的单一数字）。
        fn rms(ps: &ParticleSystem) -> f32 {
            let n = ps.particles.len() as f32;
            let (mut cx, mut cy) = (0.0f32, 0.0f32);
            for p in &ps.particles {
                cx += p.pos[0];
                cy += p.pos[1];
            }
            (cx, cy) = (cx / n, cy / n);
            (ps.particles
                .iter()
                .map(|p| (p.pos[0] - cx).powi(2) + (p.pos[1] - cy).powi(2))
                .sum::<f32>()
                / n)
                .sqrt()
        }
        /// 分离窗口（30 单位）内的**无序配对数** —— 分离项作用的对数。
        fn close_pairs(ps: &ParticleSystem) -> usize {
            let mut n = 0;
            for i in 0..ps.particles.len() {
                for j in (i + 1)..ps.particles.len() {
                    let (a, b) = (&ps.particles[i], &ps.particles[j]);
                    let dx = b.pos[0] - a.pos[0];
                    let dy = b.pos[1] - a.pos[1];
                    if (dx * dx + dy * dy).sqrt() < 30.0 {
                        n += 1;
                    }
                }
            }
            n
        }

        /// ① 三个系数全 0 ⇒ **空转**：位置与速度逐位不变（"没有依据就不猜力"这条约束的可测形式）。
        #[test]
        fn boids_all_zero_is_bit_exact_no_op() {
            let mut ps = sys(mk(20.0, 0.0, 0.0, 0.0, 0.0));
            ps.update(1.0 / 30.0);
            let before: Vec<_> = ps.particles.iter().map(|p| (p.pos, p.vel)).collect();
            assert_eq!(before.len(), 24, "第 1 帧应把 24 颗全放出来");
            for _ in 0..120 {
                ps.update(1.0 / 30.0);
            }
            let after: Vec<_> = ps.particles.iter().map(|p| (p.pos, p.vel)).collect();
            assert_eq!(before, after, "三个系数全 0 时 boids 必须逐位空转");
        }

        /// ② 只有**聚集**：初速 0 ⇒ 相对质心的半径**收缩到近 0**（过冲是弹簧的正常行为，
        /// 所以判据取"全程最小半径"+"绝不超过初始半径"，而不是逐帧单调）。
        #[test]
        fn boids_cohesion_contracts_the_cluster() {
            let mut on = sys(mk(20.0, 0.0, 0.0, 4.0, 0.0));
            let mut off = sys(mk(20.0, 0.0, 0.0, 0.0, 0.0));
            on.update(1.0 / 30.0);
            off.update(1.0 / 30.0);
            let (r0, r_off) = (rms(&on), rms(&off));
            assert!(r0 > 5.0, "初始得有半径（不然测不出收缩）：{r0}");
            let mut r_min = r0;
            for i in 0..60 {
                on.update(1.0 / 30.0);
                off.update(1.0 / 30.0);
                let r = rms(&on);
                assert!(
                    r <= r0 * 1.05,
                    "聚集项下半径不该明显超过初始值（第 {i} 帧 {r0} → {r}）"
                );
                r_min = r_min.min(r);
            }
            assert!(
                r_min < r0 * 0.1,
                "聚集项必须把集群收到质心附近：初始 {r0} → 全程最小 {r_min}"
            );
            assert!(
                rms(&off) > r_off * 0.999,
                "系数 0 时半径必须一动不动：{r_off} → {}",
                rms(&off)
            );
        }

        /// ③ 只有**分离**：初始大量配对比 `separationthreshold`(30) 近 ⇒ 全部被推到窗口外，
        /// 且最近邻距离**变大**。
        #[test]
        fn boids_separation_clears_the_separation_window() {
            let mut on = sys(mk(8.0, 0.0, 0.0, 0.0, 30.0));
            let mut off = sys(mk(8.0, 0.0, 0.0, 0.0, 0.0));
            on.update(1.0 / 30.0);
            off.update(1.0 / 30.0);
            let p0 = close_pairs(&on);
            assert!(p0 > 20, "初态得有一堆近配对（不然分离项没事做）：{p0}");
            let nn = |ps: &ParticleSystem| -> f32 {
                let mut best = f32::MAX;
                for i in 0..ps.particles.len() {
                    for j in (i + 1)..ps.particles.len() {
                        let (a, b) = (&ps.particles[i], &ps.particles[j]);
                        let d = ((b.pos[0] - a.pos[0]).powi(2) + (b.pos[1] - a.pos[1]).powi(2)).sqrt();
                        best = best.min(d);
                    }
                }
                best
            };
            let nn0 = nn(&on);
            for _ in 0..300 {
                on.update(1.0 / 30.0);
                off.update(1.0 / 30.0);
            }
            assert_eq!(close_pairs(&on), 0, "分离项必须把分离窗口内的配对数清 0");
            assert!(nn(&on) > nn0 * 3.0, "最近邻距离必须被推大：{nn0} → {}", nn(&on));
            assert_eq!(close_pairs(&off), p0, "系数 0 时近配对数必须不变（空转）");
        }
    }

    /// `collisionquad`：有限矩形、**单面**。往下落的粒子必须停在矩形上表面（`slide` ⇒ 法向速度归零），
    /// 而从背面来的粒子必须**穿过**（单面语义）。
    #[test]
    fn collisionquad_lands_particles_on_one_side_only() {
        let mk = |origin: &str| {
            serde_json::from_str::<ParticlePreset>(&format!(
                r#"{{
                "emitter":[{{"name":"sphererandom","origin":"0 0 0","directions":"1 1 1","distancemin":0,"distancemax":0,"rate":600}}],
                "initializer":[{{"name":"lifetimerandom","min":100,"max":100}},{{"name":"sizerandom","min":10,"max":10}}],
                "operator":[{{"name":"movement","gravity":"0 -100 0","drag":0}},
                            {{"name":"collisionquad","origin":{origin},"size":"100 100","collisionbehavior":"slide"}}],
                "renderer":[{{"name":"sprite"}}],"maxcount":4}}"#
            ))
            .unwrap()
        };
        // 矩形在 y = −100，尺寸 100×100 ⇒ 上表面 y = −100（粒子从 y=0 往下掉）
        let mut ps = ParticleSystem::new(mk("\"0 -100 0\""), [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        for _ in 0..90 {
            ps.update(1.0 / 30.0);
        }
        assert!(!ps.particles.is_empty(), "要有粒子");
        for p in &ps.particles {
            assert!(p.vel[1] >= -1e-3, "slide 之后法向速度必须归零/向上：{}", p.vel[1]);
            assert!(p.pos[1] >= -100.0 - 1e-2, "不能被压到矩形里面去：y={}", p.pos[1]);
            assert!(p.pos[1] <= -100.0 + 1.0, "应当贴在矩形上表面：y={}", p.pos[1]);
        }
        // 背面：矩形放到粒子**上方**（y=+100）⇒ 往下掉的粒子从 −y 侧接近 ⇒ 单面语义下必须穿过
        let mut back = ParticleSystem::new(mk("\"0 100 0\""), [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        for _ in 0..90 {
            back.update(1.0 / 30.0);
        }
        assert!(back.particles.iter().any(|p| p.pos[1] < -50.0), "背面必须穿过（单面）");
    }

    /// `mapsequencearoundcontrolpoint`（OWE `MapSequenceAroundControlPointProgram`）：
    /// 4 颗粒子按序号均匀分布在控制点周围**同一半径**的环上；半径保持不变（OWE 保留 Δ 的径向分量）。
    #[test]
    fn mapsequence_spreads_particles_evenly_around_the_control_point() {
        let preset: ParticlePreset = serde_json::from_str(
            r#"{
            "controlpoint":[{"id":0,"offset":"0 0 0","flags":0}],
            "emitter":[{"name":"sphererandom","origin":"0 0 0","directions":"1 1 0","distancemin":50,"distancemax":50,"rate":600}],
            "initializer":[{"name":"lifetimerandom","min":100,"max":100},{"name":"sizerandom","min":10,"max":10},
                           {"name":"mapsequencearoundcontrolpoint","controlpoint":0,"count":4,"bounds":"0 1","axis":"0 0 1"}],
            "renderer":[{"name":"sprite"}],"maxcount":4
        }"#,
        )
        .unwrap();
        let mut ps = ParticleSystem::new(preset, [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        ps.update(1.0 / 30.0); // rate 600/30 = 20 次请求，被 maxcount=4 截断 ⇒ 一次拿满 4 颗
        assert_eq!(ps.particles.len(), 4, "该拿满 4 颗");
        for p in &ps.particles {
            let r = (p.pos[0] * p.pos[0] + p.pos[1] * p.pos[1] + p.pos[2] * p.pos[2]).sqrt();
            assert!((r - 50.0).abs() < 1e-2, "半径必须保持 50（OWE 保留径向分量），实际 {r}");
        }
        let mut min_d = f32::MAX;
        for i in 0..4 {
            for j in (i + 1)..4 {
                let (a, b) = (&ps.particles[i], &ps.particles[j]);
                let d = ((a.pos[0] - b.pos[0]).powi(2) + (a.pos[1] - b.pos[1]).powi(2) + (a.pos[2] - b.pos[2]).powi(2)).sqrt();
                min_d = min_d.min(d);
            }
        }
        assert!(min_d > 50.0, "4 个槽位应当分散在环上（最小间距 > 50），实际 {min_d}");
    }

    /// 音频响应：**没有频谱时恒等**（OWE `AudioResponseScale` 在 `audio.is_empty` 时返回 1.0）。
    /// `WE_AUDIO_BANDS` / `WE_PARTICLE_AUDIO` 只是离线探针；默认全 0 ⇒ 本机所有场景逐字节不变。
    #[test]
    fn audio_response_is_identity_without_a_spectrum() {
        let _g = crate::audio::test_lock();
        let cfg = AudioCfg {
            enable: true,
            amount: 1.0,
            exponent: 5.0,
            freq: [0.0, 15.0],
            bounds: [0.8, 1.0],
        };
        let none = crate::audio::Spectrum::default();
        assert_eq!(cfg.scale(&none), 1.0, "没有频谱 ⇒ 必须正好是 1.0（不是接近 1）");
        // 有频谱时按 bounds 归一化（段均值 0.9 → level 0.5）+ exponent（5）⇒ 1 + 0.5^5 × 1
        let uniform = crate::audio::Spectrum {
            bands: [0.9; crate::audio::BANDS],
            level: 0.9,
            generation: 1,
        };
        assert!(
            (cfg.scale(&uniform) - (1.0 + 0.5f32.powi(5))).abs() < 1e-6,
            "实际 {}",
            cfg.scale(&uniform)
        );
        // 关掉的响应即使有频谱也是恒等
        let off = AudioCfg { enable: false, ..cfg };
        assert_eq!(off.scale(&uniform), 1.0);
        // 系统级：emitter 声明了 audioprocessingmode 但没喂频谱 ⇒ 发射个数与完全不声明时相同
        let mk = |extra: &str| {
            serde_json::from_str::<ParticlePreset>(&format!(
                r#"{{
                "emitter":[{{"name":"sphererandom","origin":"0 0 0","directions":"1 1 0","distancemin":0,"distancemax":1,"rate":6{extra}}}],
                "initializer":[{{"name":"lifetimerandom","min":100,"max":100}},{{"name":"sizerandom","min":10,"max":10}}],
                "renderer":[{{"name":"sprite"}}],"maxcount":32}}"#
            ))
            .unwrap()
        };
        let run = |p: ParticlePreset| {
            let mut ps = ParticleSystem::new(p, [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
            for _ in 0..60 {
                ps.update(1.0 / 30.0);
            }
            ps.count()
        };
        let plain = run(mk(""));
        let with_audio = run(mk(r#","audioprocessingmode":3,"audioamount":1.0,"audioexponent":1.0,"audiobounds":"0.0 1.0""#));
        assert_eq!(plain, with_audio, "没有频谱时音频响应必须恒等");
    }

    /// 发射率音频响应：**真频谱喂进来时发射个数必须真的上去**，并且
    /// `audioamount`/`audioexponent` **没写在 JSON 里时按 WE 的默认 1.0** 处理
    /// （OWE `ParticleObject.cppm:82-83`）。这条是"音频响应真正生效"的回归测试：
    /// 修复前 `audioamount` 走 serde 的 0.0 ⇒ `1 + level×0 = 1` ⇒ 一个粒子都不多
    /// （实测就是 `2997301801` 的 emitter 曾经完全不动的根因）。
    #[test]
    fn emitter_audio_response_boosts_the_rate_when_a_spectrum_arrives() {
        let _g = crate::audio::test_lock();
        let preset: ParticlePreset = serde_json::from_str(
            r#"{
            "emitter":[{"name":"sphererandom","origin":"0 0 0","directions":"1 1 0","distancemin":0,"distancemax":1,"rate":30,
                        "audioprocessingmode":3,"audioprocessingbounds":"0.5 1"}],
            "initializer":[{"name":"lifetimerandom","min":100,"max":100},{"name":"sizerandom","min":10,"max":10}],
            "renderer":[{"name":"sprite"}],"maxcount":4096}"#,
        )
        .unwrap();
        let run = |preset: ParticlePreset| {
            let mut ps = ParticleSystem::new(preset, [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
            let mut peak = 0usize;
            for _ in 0..60 {
                ps.update(1.0 / 30.0);
                peak = peak.max(ps.count());
            }
            peak
        };
        // ① 没有频谱（默认）⇒ 与不声明音频响应一致
        crate::audio::set_static_bands([0.0; crate::audio::BANDS]);
        let quiet = run(preset.clone());
        // ② 真频谱：段均值 0.9 ∈ bounds(0.5,1) ⇒ level 0.8 ⇒ 倍率 1.8
        let scale_expected: f32 = 1.0 + ((0.9f32 - 0.5) / (1.0 - 0.5)) * 1.0;
        assert!((scale_expected - 1.8).abs() < 1e-6);
        crate::audio::set_static_bands([0.9; crate::audio::BANDS]);
        let loud = run(preset);
        // 复位（别影响别的测试）
        crate::audio::set_static_bands([0.0; crate::audio::BANDS]);
        assert!(
            loud as f32 > quiet as f32 * 1.2,
            "有频谱时必须明显多发射（quiet={quiet} loud={loud}，期望倍数 {scale_expected}）"
        );
    }

    /// **回归护栏**：WE 的粒子字段类型很松，本次新增的键不能因为"类型不对"就把**整份预设**打掉。
    /// 实测反例（本次真踩到过）：`initializer[].offset` 是**浮点**（`ember_small.json` 的
    /// `-0.5`、`leaves5.json` 的 `3`）、控制点 `offset` 出现 **`null`**。声明成 `String`
    /// 会让 serde 硬错 ⇒ 整个 `Ember small` 家族在 17 条里 12 条静默消失。
    #[test]
    fn presets_with_loose_types_still_parse() {
        // ① turbulentvelocityrandom 的 offset 是浮点；② controlpoint 的 offset 是 null；
        // ③ boids/vortex/collisionquad/mapsequence 的真实字段形状（来自本机场景与官方预览）
        let j = r#"{
            "controlpoint":[{"id":0,"offset":"0 0 0","flags":0},{"id":1,"offset":null,"flags":16}],
            "emitter":[{"name":"sphererandom","origin":"0 0 0","directions":"1 1 1","distancemin":0,"distancemax":5,"rate":6,
                        "audioprocessingmode":3,"audioamount":1,"audioexponent":1,"audiobounds":"0.8 1","audiofrequency":15}],
            "initializer":[{"name":"lifetimerandom","min":1,"max":2},
                           {"name":"turbulentvelocityrandom","offset":-0.5,"scale":0.1,
                            "audioprocessingmode":3,"audioprocessingbounds":"0.8 1","audioprocessingexponent":5,"audioprocessingfrequencyend":15},
                           {"name":"mapsequencearoundcontrolpoint","controlpoint":0,"count":3.02,"bounds":"0 1","axis":"0 0 1",
                            "speedmin":"0 10 0","speedmax":"0 100 0"}],
            "operator":[{"name":"movement","gravity":"0 -500 0","drag":0.5},
                        {"name":"turbulence","mask":"1 1 0","scale":0.01,"speedmin":30,"speedmax":50,
                         "audioprocessingmode":2,"audioprocessingbounds":"0.9 1","audioprocessingexponent":3,"audioprocessingfrequencyend":15},
                        {"name":"vortex","id":9,"speedinner":100,"audioprocessingmode":0},
                        {"name":"controlpointattract","controlpoint":1,"scale":-500,"threshold":64},
                        {"name":"boids","alignmentfactor":5,"cohesionfactor":5,"separationfactor":0,"neighborthreshold":150,"flags":0},
                        {"name":"collisionquad","origin":"0 -100 0","size":"100 100","collisionbehavior":"slide"},
                        {"name":"oscillatesize","frequencymin":5,"scalemax":1.1,"scalemin":0.9}],
            "material":"materials/particle/halo_1.json","maxcount":250
        }"#;
        let preset: ParticlePreset = serde_json::from_str(j).expect("宽松类型必须能解析");
        assert_eq!(preset.controlpoint.len(), 2);
        assert_eq!(preset.initializer[1].offset, -0.5, "浮点 offset 要原样取到");
        assert_eq!(preset.initializer[2].count, Some(3.02));
        // 建起来之后：新算子都该被认出来（不是静默跳过）
        let ps = ParticleSystem::new(preset, [10.0, 20.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        assert!(ps.vortex.is_some() && ps.attract.is_some() && ps.boids.is_some());
        assert!(ps.collide.is_some() && ps.osc_size.is_some() && ps.mapseq.is_some());
        assert!(ps.emit_audio.enable && ps.init_audio.enable && ps.turb_audio.enable);
        // 控制点位置 = 对象原点 + offset（id 1 的 offset 是 null ⇒ 空串 ⇒ 偏移 0）
        assert_eq!(ps.cp[0], [10.0, 20.0, 0.0]);
        assert_eq!(ps.cp[1], [10.0, 20.0, 0.0]);
    }

    /// `boxrandom` 的盒子是**逐轴**的：`directions "1 0.1 0"` + `distancemax "512 256 0"`
    /// ⇒ 场景区间 `x ∈ origin ± 512`、`y ∈ origin ± 25.6`。
    /// 旧实现（"归一化随机向量 × 一个标量距离"）在某一轴幅度小时会把**小轴放大**，
    /// y 展开能到 ±490 —— 这就是 `2961776805` 的 Ember「整层在画布外」的第二个真因。
    #[test]
    fn box_random_stays_within_each_axis_range() {
        let p: ParticlePreset = serde_json::from_str(
            r#"{
            "emitter":[{"name":"boxrandom","directions":"1 0.1 0","distancemax":"512 256 0","rate":600}],
            "initializer":[{"name":"lifetimerandom","min":9,"max":10}],
            "renderer":[{"name":"sprite"}],
            "maxcount":600
        }"#,
        )
        .unwrap();
        let mut ps =
            ParticleSystem::new(p, [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        for _ in 0..90 {
            ps.update(1.0 / 30.0);
        }
        assert!(ps.count() > 100, "应有大量粒子，实际 {}", ps.count());
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for q in &ps.particles {
            x0 = x0.min(q.pos[0]);
            x1 = x1.max(q.pos[0]);
            y0 = y0.min(q.pos[1]);
            y1 = y1.max(q.pos[1]);
        }
        assert!(
            y0 >= -25.7 && y1 <= 25.7,
            "y 半宽必须是 distancemax.y × directions.y = 25.6，实际 [{y0:.1},{y1:.1}]"
        );
        assert!(
            x0 >= -512.1 && x1 <= 512.1,
            "x 半宽必须是 512，实际 [{x0:.1},{x1:.1}]"
        );
        // 两条轴都要真的用满（别只是"没超界"）
        assert!(y1 - y0 > 30.0, "y 展开太小，实际 {:.1}", y1 - y0);
        assert!(x1 - x0 > 600.0, "x 展开太小，实际 {:.1}", x1 - x0);
    }

    /// `emitter_origin_debug`：诊断用的发射器**局部原点**必须与预设原文逐值一致
    /// （`WE_PARTICLE_GEOM=1` 的 `[粒子落点]` 行靠它；**键缺失** ⇒ (0,0,0)）。
    #[test]
    fn emitter_origin_debug_reports_the_preset_value() {
        let mk = |origin_field: &str| {
            format!(
                r#"{{"emitter":[{{"name":"sphererandom"{origin_field},"directions":"1 0.1 1",
                                "distancemax":750,"rate":20}}],
                    "initializer":[],"renderer":[{{"name":"sprite"}}],"maxcount":10}}"#
            )
        };
        let p: ParticlePreset = serde_json::from_str(&mk(r#","origin":"350 750 0""#)).unwrap();
        let ps =
            ParticleSystem::new(p, [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        assert_eq!(ps.emitter_origin_debug(), [350.0, 750.0, 0.0]);
        // 键缺失（WE 编辑器不写 origin 时）⇒ 全 0，不是 NaN/崩
        let p2: ParticlePreset = serde_json::from_str(&mk("")).unwrap();
        let ps2 =
            ParticleSystem::new(p2, [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        assert_eq!(ps2.emitter_origin_debug(), [0.0, 0.0, 0.0]);
    }

    /// 发射器自带初速：`velocity = normalize(局部偏移) × random(speedmin, speedmax)`
    /// （OWE `ParticleEmitter.cpp:203/259` 与 linux-WE `CParticle.cpp:632-640`）。
    /// 官方 `spritetrail` 预览场景只有这一个速度来源 —— 少了它，粒子一动不动、
    /// 拖尾的 `up` 退化 ⇒ 一个像素都不画。
    #[test]
    fn emitter_speed_sets_the_radial_initial_velocity() {
        let p: ParticlePreset = serde_json::from_str(
            r#"{
            "emitter":[{"name":"sphererandom","directions":"1 1 0","distancemin":100,"distancemax":100,
                        "speedmin":200,"speedmax":200,"rate":600}],
            "initializer":[{"name":"lifetimerandom","min":9,"max":10}],
            "renderer":[{"name":"sprite"}],
            "maxcount":600
        }"#,
        )
        .unwrap();
        let mut ps =
            ParticleSystem::new(p, [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        for _ in 0..90 {
            ps.update(1.0 / 30.0);
        }
        assert!(ps.count() > 100);
        for q in &ps.particles {
            // 出生位置已沿速度走了几帧，用"位置 - 速度×年龄"把出生点还原出来
            let spawn = [
                q.pos[0] - q.vel[0] * q.age,
                q.pos[1] - q.vel[1] * q.age,
                q.pos[2] - q.vel[2] * q.age,
            ];
            let n = len3(spawn);
            assert!((n - 100.0).abs() < 1.0, "半径应 ≈100，实际 {n:.1}");
            let v = len3(q.vel);
            assert!((v - 200.0).abs() < 1.0, "初速应 ≈200，实际 {v:.1}");
            // 方向 = 局部偏移方向（径向向外）
            let d = (q.vel[0] * spawn[0] + q.vel[1] * spawn[1]) / (v * n);
            assert!(d > 0.999, "速度必须沿局部偏移方向，cos={d:.4}");
        }
    }

    /// `spritetrail` 的四边同样要过**对象缩放**（与 sprite 的 `quad_scale` 同一份依据）：
    /// 同一份预设，缩放 (2,2) 的四边长/宽必须是 (1,1) 的 2 倍。
    #[test]
    fn spritetrail_quad_is_scaled_by_the_object() {
        fn preset() -> ParticlePreset {
            serde_json::from_str(
                r#"{
                "emitter":[{"name":"sphererandom","directions":"1 1 0","distancemin":50,"distancemax":50,"rate":600}],
                "initializer":[{"name":"lifetimerandom","min":9,"max":10},
                               {"name":"velocityrandom","min":"100 0 0","max":"100 0 0"}],
                "renderer":[{"name":"spritetrail","length":0.05,"maxlength":10}],
                "maxcount":600
            }"#,
            )
            .unwrap()
        }
        let spans = |s: f32| -> (f32, f32) {
            let mut ps =
                ParticleSystem::new(preset(), [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
            for _ in 0..30 {
                ps.update(1.0 / 30.0);
            }
            ps.set_trail_scale([s, s], 1.0);
            let mut v: Vec<[f32; 17]> = Vec::new();
            ps.build(&mut v);
            assert!(v.len() >= 6);
            let w = ((v[1][0] - v[0][0]).powi(2) + (v[1][1] - v[0][1]).powi(2)).sqrt();
            let l = ((v[5][0] - v[0][0]).powi(2) + (v[5][1] - v[0][1]).powi(2)).sqrt();
            (w, l)
        };
        let (w1, l1) = spans(1.0);
        let (w2, l2) = spans(2.0);
        assert!(w1 > 1.0 && l1 > 1.0, "四边不能退化：宽{w1:.2} 长{l1:.2}");
        assert!((w2 / w1 - 2.0).abs() < 1e-3, "宽度应随缩放 ×2，实际 {:.4}", w2 / w1);
        assert!((l2 / l1 - 2.0).abs() < 1e-3, "长度应随缩放 ×2，实际 {:.4}", l2 / l1);
    }

    // ══════════════════════════════════════════════════════════════════════════
    // 本次（遗留三件）新增单测 —— 判据统一为「对象实际位置/形状 == 配置推出的位置/形状」
    // ══════════════════════════════════════════════════════════════════════════

    // ── 遗留 3-a：发射器缺省 `rate` = 10（WE；linux-WE `ObjectParser.cpp:601`）────────
    #[test]
    fn emitter_rate_absent_defaults_to_we_ten_but_explicit_zero_stays_zero() {
        let p: ParticlePreset = serde_json::from_str(
            r#"{"emitter":[{"name":"sphererandom","distancemax":32},{"name":"boxrandom","rate":0}],"maxcount":100}"#,
        )
        .unwrap();
        // serde 缺省函数读进程环境（未设 ⇒ 开）⇒ 缺 rate 的那条 = 10
        assert_eq!(
            p.emitter[0].rate, 10.0,
            "rate 缺失应取 WE 缺省 10，实际 {}",
            p.emitter[0].rate
        );
        assert_eq!(p.emitter[1].rate, 0.0, "显式 rate=0 必须保持 0（= 不按速率发射）");
        // 纯函数：开关两态
        assert_eq!(default_emitter_rate_with(true), 10.0);
        assert_eq!(default_emitter_rate_with(false), 0.0);
    }

    /// 缺省 `rate` 的**行为**：1 秒内应真的发射出 ~10 颗（旧缺省 0 ⇒ 一颗都没有）。
    #[test]
    fn emitter_without_rate_emits_particles_at_the_we_default() {
        let p: ParticlePreset = serde_json::from_str(
            r#"{"emitter":[{"name":"sphererandom","directions":"1 1 0","distancemin":0,"distancemax":32}],
                "initializer":[{"name":"lifetimerandom","min":5,"max":5},{"name":"sizerandom","min":10,"max":10}],
                "renderer":[{"name":"sprite"}],"maxcount":64}"#,
        )
        .unwrap();
        let mut ps = ParticleSystem::new(p, [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        for _ in 0..30 {
            ps.update(1.0 / 30.0);
        }
        assert!(
            (7..=13).contains(&ps.count()),
            "缺省 10/s ⇒ 1 秒约 10 颗，实际 {}",
            ps.count()
        );
    }

    // ── 遗留 3-b：发射器缺省 `distancemax` = 256（OWE `ParticleObject.cppm:73`）────────
    #[test]
    fn emitter_distancemax_defaults_to_we_256() {
        let absent: Emitter = serde_json::from_str(r#"{"name":"boxrandom"}"#).unwrap();
        let explicit_zero: Emitter =
            serde_json::from_str(r#"{"name":"boxrandom","distancemax":0}"#).unwrap();
        let explicit_vec: Emitter =
            serde_json::from_str(r#"{"name":"boxrandom","distancemax":"1024 512 0"}"#).unwrap();
        assert_eq!(emitter_distancemax_with(&absent, true), [256.0, 256.0, 256.0]);
        assert_eq!(
            emitter_distancemax_with(&absent, false),
            [0.0, 0.0, 0.0],
            "开关关 ⇒ 旧行为（点）"
        );
        assert_eq!(
            emitter_distancemax_with(&explicit_zero, true),
            [0.0, 0.0, 0.0],
            "显式 0 保持 0"
        );
        assert_eq!(emitter_distancemax_with(&explicit_vec, true), [1024.0, 512.0, 0.0]);
        // 进程默认（未设环境变量）也应是 WE 缺省
        assert_eq!(emitter_distancemax(&absent), [256.0, 256.0, 256.0]);
    }

    /// 缺省 `distancemax` 的**形状**：盒的 x 展开 ≈ 配置推出的 ±256（旧实现退化成一点）。
    #[test]
    fn box_emitter_without_distancemax_spans_the_we_default_box_not_a_point() {
        let mk = |extra: &str| {
            serde_json::from_str::<ParticlePreset>(&format!(
                r#"{{"emitter":[{{"name":"boxrandom","directions":"1 1 0","rate":4000{extra}}}],
                    "initializer":[{{"name":"lifetimerandom","min":100,"max":100}},{{"name":"sizerandom","min":4,"max":4}}],
                    "renderer":[{{"name":"sprite"}}],"maxcount":200}}"#
            ))
            .unwrap()
        };
        let spread = |mut ps: ParticleSystem| -> (f32, f32) {
            for _ in 0..60 {
                ps.update(1.0 / 30.0);
            }
            let (mut lo, mut hi) = (f32::MAX, f32::MIN);
            for p in &ps.particles {
                lo = lo.min(p.pos[0]);
                hi = hi.max(p.pos[0]);
            }
            (lo, hi)
        };
        let (lo, hi) = spread(ParticleSystem::new(
            mk(""),
            [0.0, 0.0, 0.0],
            [1.0, 1.0],
            &InstanceOverride::default(),
            1,
        ));
        assert!(hi - lo > 200.0, "缺省盒应铺开 ~±256，不是一点：{lo}..{hi}");
        assert!(lo >= -260.0 && hi <= 260.0, "缺省盒不应超过 256：{lo}..{hi}");
    }

    // ── 遗留 1：对象级 `controlpointN` 覆盖 ────────────────────────────────────────
    #[test]
    fn de_vec3_opt_lenient_accepts_string_array_number_and_null() {
        let ov: InstanceOverride =
            serde_json::from_str(r#"{"controlpoint1":"300 200 7","controlpoint2":[1,2,3],"controlpoint3":5,"controlpoint4":null}"#)
                .unwrap();
        assert_eq!(ov.controlpoint1, Some([300.0, 200.0, 7.0]));
        assert_eq!(ov.controlpoint2, Some([1.0, 2.0, 3.0]));
        assert_eq!(ov.controlpoint3, Some([5.0, 5.0, 5.0]));
        assert_eq!(ov.controlpoint4, None, "null ⇒ 无覆盖");
        assert_eq!(ov.controlpoint0, None, "缺失 ⇒ 无覆盖");
        assert!(ov.has_cp_override());
    }

    /// 语义（OWE `ParticleRuntime.cpp:605-631` 的 `UpdateControlpoints`，唯一实现它的参照）：
    /// · 非 `worldspace`（预设 flags 位 1 = 0）：`offset = base_offset + 覆盖值`（局部，乘 scale 加 origin）
    /// · `worldspace`（flags 位 1 = 2）：覆盖值 = **绝对世界/场景坐标**（与对象位置无关）
    #[test]
    fn object_controlpoint_override_replaces_the_preset_position() {
        let preset = || {
            serde_json::from_str::<ParticlePreset>(
                r#"{"controlpoint":[{"id":1,"offset":"0 0 0","flags":0},{"id":2,"offset":"10 -5 0","flags":2}],
                    "emitter":[{"name":"sphererandom","distancemin":0,"distancemax":1,"rate":1}],
                    "initializer":[{"name":"lifetimerandom","min":1,"max":1}],
                    "operator":[{"name":"controlpointattract","controlpoint":1,"scale":1,"threshold":10000}],
                    "renderer":[{"name":"sprite"}],"maxcount":4}"#,
            )
            .unwrap()
        };
        let origin = [1000.0f32, 500.0, 0.0];
        let scale = [2.0f32, 3.0];
        // 无覆盖：cp[1] = origin（预设 offset 0）、cp[2] = origin + (10,−5,0)×scale
        let base = ParticleSystem::new(preset(), origin, scale, &InstanceOverride::default(), 1);
        assert_eq!(base.cp[1], [1000.0, 500.0, 0.0]);
        assert_eq!(base.cp[2], [1000.0 + 10.0 * 2.0, 500.0 - 5.0 * 3.0, 0.0]);
        // 覆盖槽 1（非 worldspace）⇒ origin + (0 + 覆盖) × scale
        let ov: InstanceOverride =
            serde_json::from_str(r#"{"controlpoint1":"300 200 7"}"#).unwrap();
        let got = ParticleSystem::new(preset(), origin, scale, &ov, 1);
        assert_eq!(
            got.cp[1],
            [1000.0 + 300.0 * 2.0, 500.0 + 200.0 * 3.0, 7.0],
            "非 worldspace 覆盖 = origin + (base+覆盖)×scale"
        );
        // 覆盖槽 2（worldspace）⇒ 覆盖值就是绝对场景坐标
        let ov2: InstanceOverride =
            serde_json::from_str(r#"{"controlpoint2":"-42 88 0"}"#).unwrap();
        let got2 = ParticleSystem::new(preset(), origin, scale, &ov2, 1);
        assert_eq!(got2.cp[2], [-42.0, 88.0, 0.0], "worldspace 覆盖 = 绝对坐标");
        // 未覆盖的槽位不受影响
        assert_eq!(got2.cp[1], [1000.0, 500.0, 0.0]);
        // 未覆盖的槽位不受别的槽的覆盖影响（槽 2 被覆盖，故与基线不同）
        for i in [0usize, 1, 3, 4, 5, 6, 7] {
            assert_eq!(base.cp[i], got2.cp[i], "槽 {i} 不该被别的槽的覆盖影响");
        }
        assert_ne!(base.cp[2], got2.cp[2], "槽 2 被覆盖 ⇒ 必须与基线不同");
    }

    /// 覆盖**真的**把 `controlpointattract` 的吸引中心挪到配置推出的位置：粒子群被拉向覆盖点。
    /// （精确定位已由 `object_controlpoint_override_replaces_the_preset_position` 逐值断言；
    /// 这里验的是**行为**真的接通了 —— 无阻尼吸引子会来回摆，所以判「明显朝配置推出的方向移动
    /// 且更靠近配置推出的控制点」。）
    #[test]
    fn controlpointattract_pulls_toward_the_object_override_position() {
        let preset = || {
            serde_json::from_str::<ParticlePreset>(
                r#"{"controlpoint":[{"id":1,"offset":"0 0 0","flags":0}],
                    "emitter":[{"name":"sphererandom","origin":"0 0 0","directions":"1 1 0","distancemin":800,"distancemax":800,"rate":2000}],
                    "initializer":[{"name":"lifetimerandom","min":60,"max":60},{"name":"sizerandom","min":4,"max":4}],
                    "operator":[{"name":"controlpointattract","controlpoint":1,"origin":"0 0 0","scale":2000,"threshold":10000}],
                    "renderer":[{"name":"sprite"}],"maxcount":40}"#,
            )
            .unwrap()
        };
        let centroid = |ov: InstanceOverride| -> [f32; 3] {
            let mut ps = ParticleSystem::new(preset(), [0.0, 0.0, 0.0], [1.0, 1.0], &ov, 1);
            for _ in 0..90 {
                ps.update(1.0 / 30.0);
            }
            let n = ps.particles.len().max(1) as f32;
            let mut c = [0.0f32; 3];
            for p in &ps.particles {
                c[0] += p.pos[0] / n;
                c[1] += p.pos[1] / n;
                c[2] += p.pos[2] / n;
            }
            c
        };
        // 无覆盖 ⇒ 控制点在原点 (0,0)
        let c0 = centroid(InstanceOverride::default());
        // 覆盖 controlpoint1 = (1000, 600, 0)（非 worldspace；origin=0、scale=1 ⇒ 场景坐标 (1000,600)）
        let ov: InstanceOverride =
            serde_json::from_str(r#"{"controlpoint1":"1000 600 0"}"#).unwrap();
        let c1 = centroid(ov);
        let dist = |c: [f32; 3], cp: [f32; 2]| -> f32 {
            ((c[0] - cp[0]).powi(2) + (c[1] - cp[1]).powi(2)).sqrt()
        };
        assert!(
            dist(c1, [1000.0, 600.0]) < dist(c0, [1000.0, 600.0]),
            "覆盖后粒子群应更靠近配置推出的 (1000,600)：无覆盖 {c0:?} vs 覆盖 {c1:?}"
        );
        assert!(
            c1[0] - c0[0] > 500.0 && c1[1] - c0[1] > 300.0,
            "覆盖必须把粒子群往 +x/+y 明显拉开：{c0:?} → {c1:?}"
        );
        // 反证：把覆盖设到反侧 ⇒ 粒子群必须往反方向走
        let ov2: InstanceOverride =
            serde_json::from_str(r#"{"controlpoint1":"-1000 -600 0"}"#).unwrap();
        let c2 = centroid(ov2);
        assert!(
            c2[0] < -500.0 && c2[1] < -300.0,
            "反侧覆盖应把粒子群拉向 (−1000,−600)：实际 {c2:?}"
        );
    }

    // ── 遗留 2：算子 blend-in/out ─────────────────────────────────────────────────
    /// WE 官方文档的例子：Blend-in 0.2/0.4、Blend-out 0.6/0.8。
    #[test]
    fn blend_weight_ramps_in_then_out_over_the_lifetime() {
        let b = Blend {
            bin_s: 0.2,
            bin_e: 0.4,
            bout_s: 0.6,
            bout_e: 0.8,
        };
        assert_eq!(b.weight(0.0), 0.0);
        assert_eq!(b.weight(0.19), 0.0);
        assert!((b.weight(0.3) - 0.5).abs() < 1e-6, "淡入中点应 0.5，实际 {}", b.weight(0.3));
        assert_eq!(b.weight(0.5), 1.0, "窗口内满强度");
        assert!((b.weight(0.7) - 0.5).abs() < 1e-6, "淡出中点应 0.5，实际 {}", b.weight(0.7));
        assert_eq!(b.weight(0.9), 0.0);
    }

    /// 缺省 / 退化窗口 ⇒ 权重恒 1（= 旧行为，且 `blend_mul/add` 在 w≥1 时**逐位直通**）。
    #[test]
    fn blend_defaults_and_degenerate_windows_are_the_identity() {
        // 本机唯一命中：`2887537617` 的 `fireflies` oscillatealpha `{blendoutstart:1, blendoutend:1}`
        let op: Operator =
            serde_json::from_str(r#"{"name":"oscillatealpha","blendoutstart":1,"blendoutend":1}"#)
                .unwrap();
        let b = Blend::from_op(&op).expect("声明了 blend 字段 ⇒ 应有 Blend");
        for t in [0.0f32, 0.25, 0.5, 0.999] {
            assert_eq!(b.weight(t), 1.0, "s==e==1 在整个寿命内满强度（t={t}）");
        }
        let x = 0.1234567f32;
        assert_eq!(blend_mul(1.0, x).to_bits(), x.to_bits(), "w=1 必须逐位直通");
        assert_eq!(blend_add(1.0, x).to_bits(), x.to_bits(), "w=1 必须逐位直通");
        // 没有任何 blend 字段 ⇒ None（完全不碰）
        let none: Operator = serde_json::from_str(r#"{"name":"turbulence"}"#).unwrap();
        assert!(Blend::from_op(&none).is_none());
    }

    /// blend 真的改行为：带 blend-in 的 `oscillatealpha` 在寿命前段**不改变 alpha**。
    #[test]
    fn oscillatealpha_blend_in_keeps_alpha_at_base_early_in_life() {
        let preset = |extra: &str| {
            serde_json::from_str::<ParticlePreset>(&format!(
                r#"{{"emitter":[{{"name":"sphererandom","distancemin":0,"distancemax":1,"rate":600}}],
                    "initializer":[{{"name":"lifetimerandom","min":10,"max":10}},{{"name":"sizerandom","min":4,"max":4}}],
                    "operator":[{{"name":"oscillatealpha","frequencymax":0,"scalemin":0.1,"scalemax":0.1{extra}}}],
                    "renderer":[{{"name":"sprite"}}],"maxcount":8}}"#
            ))
            .unwrap()
        };
        // 顶点布局（见 `build` 的 push）：下标 16 = alpha
        let alpha_after_1s = |p: ParticlePreset| -> f32 {
            let mut ps = ParticleSystem::new(p, [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
            for _ in 0..30 {
                ps.update(1.0 / 30.0);
            }
            let mut v: Vec<[f32; 17]> = Vec::new();
            ps.build(&mut v);
            v.first().map(|r| r[16]).unwrap_or(-1.0)
        };
        let without = alpha_after_1s(preset(""));
        let with_blend = alpha_after_1s(preset(r#","blendinstart":0.1,"blendinend":0.9"#));
        assert!((without - 0.1).abs() < 0.02, "无 blend 时 alpha 应≈0.1，实际 {without}");
        assert!(with_blend > 0.9, "blend-in 尚未开始 ⇒ alpha 应≈1.0，实际 {with_blend}");
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // `turbulentvelocityrandom`（本次真根因）：速度区间缺省 100/250 + `scale` 是**锥角**不是速度
    // ══════════════════════════════════════════════════════════════════════════════
    /// ⚠ `WE_PARTICLE_TVR` 是**进程级**环境变量，而 cargo test 是多线程的 ⇒ 凡是要动它
    /// 或读它的测试都要拿这把锁，否则会互相串味（legacy 测试会把别的测试也变成 legacy）。
    /// 用 `into_inner` 恢复中毒锁：某个 tvr 测试断言失败时，不该把其余测试一起带崩。
    static TVR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn tvr_lock() -> std::sync::MutexGuard<'static, ()> {
        TVR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 只有 `turbulentvelocityrandom` 一个初始化器的预设；`extra` 是它的附加字段。
    fn tvr_preset(extra: &str) -> ParticlePreset {
        serde_json::from_str(&format!(
            r#"{{
            "flags":0,
            "emitter":[{{"name":"boxrandom","directions":"1 1 0","distancemin":"-512 -256 0","distancemax":"512 256 0","rate":3000}}],
            "initializer":[{{"name":"turbulentvelocityrandom"{extra}}}],
            "operator":[{{"name":"movement"}}],
            "renderer":[{{"name":"sprite"}}],
            "material":"m.json","maxcount":256
        }}"#
        ))
        .unwrap()
    }

    /// 跑一帧、取第一颗粒子的速度（`rate` 很高 ⇒ 一定有线粒子）。
    fn tvr_first_velocity(p: ParticlePreset) -> [f32; 3] {
        let mut ps = ParticleSystem::new(p, [0.0, 0.0, 0.0], [1.0, 1.0], &InstanceOverride::default(), 1);
        ps.update(1.0 / 30.0);
        assert!(!ps.particles.is_empty(), "发射器没发射");
        ps.particles[0].vel
    }

    /// 缺省 `speedmin/speedmax` = **100 / 250**（两份参照逐值一致）。旧实现缺省 0 ⇒ 初速恒 0。
    #[test]
    fn turbulentvelocityrandom_defaults_to_100_250_speed() {
        let _g = tvr_lock();
        let vel = tvr_first_velocity(tvr_preset(r#","offset":-0.5,"scale":0.1"#));
        let sp = len3(vel);
        assert!((100.0..=250.0).contains(&sp), "缺省速度区间应是 100..250，实际 {sp}");
    }

    /// 显式 `speedmin/speedmax = 0` 与"键缺失"**语义不同**：显式 0 就是没有初速。
    #[test]
    fn turbulentvelocityrandom_honours_explicit_zero_speed() {
        let _g = tvr_lock();
        let vel = tvr_first_velocity(tvr_preset(r#","speedmin":0,"speedmax":0,"scale":0.1"#));
        assert!(len3(vel) < 1e-6, "显式 speedmin/max=0 必须保持 0，实际 {vel:?}");
    }

    /// `scale` **不是速度倍率**，而是方向锥角系数 ⇒ 改 `scale` 不改变速度大小。
    #[test]
    fn turbulentvelocityrandom_scale_is_not_a_speed_multiplier() {
        let _g = tvr_lock();
        for sc in ["0.1", "1.0"] {
            let sp = len3(tvr_first_velocity(tvr_preset(&format!(r#","scale":{sc}"#))));
            assert!(
                (100.0..=250.0).contains(&sp),
                "scale={sc} 时速度应仍落在缺省区间 100..250，实际 {sp}"
            );
        }
    }

    /// 方向：`angle*scale*0.5 + offset` 绕 `right` 轴偏转；`scale=0.1` ⇒ 只在 `forward` 附近 ±0.05π。
    #[test]
    fn turbulentvelocityrandom_direction_stays_inside_the_scale_cone() {
        let _g = tvr_lock();
        let u = unit3(tvr_first_velocity(tvr_preset(r#","scale":0.1"#)), [0.0, 0.0, 1.0]);
        let cos = dot3(u, [0.0, 1.0, 0.0]);
        assert!(cos > 0.9, "缺省 forward=(0,1,0) ⇒ 方向应基本朝上，实际 {u:?}");
        assert!(
            cos >= (0.05 * std::f32::consts::PI).cos() - 1e-3,
            "方向跑出 scale 锥角：cos={cos}"
        );
    }

    /// `offset` 是绕 `right` 轴的偏转角（OWE `AngleAxis(angle*scale*0.5 + offset, normal)`）。
    #[test]
    fn turbulentvelocityrandom_offset_rotates_about_the_right_axis() {
        let _g = tvr_lock();
        // offset=-π/2、forward=(0,1,0)、normal=(0,0,1) ⇒ 方向转到 +x
        // （Rodrigues `v' = v cosθ + (n×v) sinθ + n(n·v)(1-cosθ)`，θ=-π/2、n×v=(-1,0,0) ⇒ (+1,0,0)；
        // 与 Eigen 的 `AngleAxisf(θ, n) * v` 同号）。
        let u = unit3(
            tvr_first_velocity(tvr_preset(&format!(
                r#","offset":{}"#,
                -std::f32::consts::FRAC_PI_2
            ))),
            [0.0, 0.0, 1.0],
        );
        assert!(u[0] > 0.9, "应转到 +x 方向，实际 {u:?}");
    }

    /// `right` 与 `forward` **平行**时旋转轴退化 ⇒ 方向仍是 `forward`（`2481886649` 的 ember.json
    /// 就写了 `right "0 1 0"`，与缺省 forward 平行）。
    #[test]
    fn turbulentvelocityrandom_parallel_right_keeps_the_forward_direction() {
        let _g = tvr_lock();
        let u = unit3(
            tvr_first_velocity(tvr_preset(r#","offset":-0.5,"scale":0.1,"right":"0 1 0""#)),
            [0.0, 0.0, 1.0],
        );
        assert!(u[1] > 0.99, "right∥forward ⇒ 方向应保持 forward=(0,1,0)，实际 {u:?}");
    }

    /// `WE_PARTICLE_TVR=legacy` 逐项回旧语义（缺省 0 ⇒ 无初速）——A/B 自证。
    #[test]
    fn turbulentvelocityrandom_legacy_switch_restores_the_zero_speed_behaviour() {
        let _g = tvr_lock();
        std::env::set_var("WE_PARTICLE_TVR", "legacy");
        let vel = tvr_first_velocity(tvr_preset(r#","offset":-0.5,"scale":0.1"#));
        std::env::remove_var("WE_PARTICLE_TVR");
        assert!(len3(vel) < 1e-6, "legacy 口径下应无初速，实际 {vel:?}");
    }

    /// 端到端：`ember.json` 的形状（发射盒在画布下方）+ 初速 ⇒ 10 秒内粒子必须升进画布。
    /// 这一条同时是"整层在画布外"这个现象的单测化：`legacy` 口径下仍然出不来。
    #[test]
    fn ember_like_preset_drifts_upward_into_the_canvas() {
        let _g = tvr_lock();
        let ember_like = || -> ParticlePreset {
            serde_json::from_str(
                r#"{
                "flags":0,
                "emitter":[{"name":"boxrandom","directions":"1 0.1 0","distancemax":"512 256 0","rate":10}],
                "initializer":[
                    {"name":"lifetimerandom","min":3,"max":5},
                    {"name":"sizerandom","min":50,"max":200},
                    {"name":"colorrandom","min":"255 143 102","max":"255 218 108"},
                    {"name":"turbulentvelocityrandom","offset":-0.5,"scale":0.1}
                ],
                "operator":[{"name":"movement","gravity":"0 0 0"},
                            {"name":"alphafade","fadeintime":0.1,"fadeouttime":1}],
                "renderer":[{"name":"spritetrail","length":0.007}],
                "material":"m.json","maxcount":40,"starttime":0
            }"#,
            )
            .unwrap()
        };
        // 发射原点 y=-124（`2961776805` 的 Ember 实测原点是 -124.16 / -52.55），画布 y∈[0,1620]
        let top = |legacy: bool| -> f32 {
            if legacy {
                std::env::set_var("WE_PARTICLE_TVR", "legacy");
            }
            let mut ps = ParticleSystem::new(
                ember_like(),
                [1025.0, -124.0, 0.0],
                [0.955, 0.955],
                &InstanceOverride::default(),
                1,
            );
            for _ in 0..300 {
                ps.update(1.0 / 30.0);
            }
            if legacy {
                std::env::remove_var("WE_PARTICLE_TVR");
            }
            ps.particles.iter().map(|p| p.pos[1]).fold(f32::MIN, f32::max)
        };
        let hi = top(false);
        let hi_legacy = top(true);
        assert!(hi > 0.0, "修复后 10 秒内粒子最高点应进入画布 (y>0)，实际 {hi}");
        assert!(hi_legacy < 0.0, "legacy 口径应仍停在画布下方，实际 {hi_legacy}");
    }

    /// `rot_about` 的代数性质：0 角是恒等、绕 z 转 90° 把 +y 送到 -x。
    #[test]
    fn rot_about_matches_rodrigues_for_identity_and_right_angle() {
        let v = [0.0f32, 1.0, 0.0];
        let k = [0.0f32, 0.0, 1.0];
        let same = rot_about(v, k, 0.0);
        assert!((same[1] - 1.0).abs() < 1e-6 && same[0].abs() < 1e-6, "{same:?}");
        let q = rot_about(v, k, std::f32::consts::FRAC_PI_2);
        assert!((q[0].abs() - 1.0).abs() < 1e-6 && q[1].abs() < 1e-6, "{q:?}");
    }

    /// `unit3` 对零向量退回 fallback（参照实现都这么兜底）。
    #[test]
    fn unit3_falls_back_for_a_zero_vector() {
        assert_eq!(unit3([0.0, 0.0, 0.0], [0.0, 1.0, 0.0]), [0.0, 1.0, 0.0]);
        assert!((unit3([3.0, 4.0, 0.0], [0.0; 3])[1] - 0.8).abs() < 1e-6);
    }

    /// 慢帧（内核转储时实测每圈 222.5 ms）**不许丢物理时间**。
    /// 以前 `update` 把 `dt` 再夹到 0.1 s，24 × 0.25 s 的慢帧序列只把
    /// 粒子推到 2.4 s（40%）；现在照单全收（与主循环同一只钟），
    /// 结果要与等时长的 1/60 序列在位置/岁数上一致（判据 ≤5%）。
    #[test]
    fn slow_frame_keeps_particle_time_and_matches_60fps() {
        let preset: ParticlePreset = serde_json::from_str(
            r#"{
            "emitter":[{"name":"boxrandom","origin":"0 0 0","directions":"1 1 0","distancemin":0,"distancemax":1,"rate":60}],
            "initializer":[{"name":"lifetimerandom","min":5,"max":5},{"name":"sizerandom","min":100,"max":100},
                           {"name":"velocityrandom","min":"300 0 0","max":"300 0 0"}],
            "operator":[{"name":"movement","gravity":"0 -200 0","drag":0}],
            "renderer":[{"name":"sprite"}],
            "maxcount":200
        }"#,
        )
        .unwrap();
        let mk = || {
            ParticleSystem::new(
                preset.clone(),
                [0.0, 0.0, 0.0],
                [1.0, 1.0],
                &InstanceOverride::default(),
                7,
            )
        };
        let (mut slow, mut fast) = (mk(), mk());
        // 4 圈 × 0.25 s = 1.0 s（= 实测的慢帧量级）
        for _ in 0..4 {
            slow.update(0.25);
        }
        for _ in 0..60 {
            fast.update(1.0 / 60.0);
        }
        assert!((slow.time - 1.0).abs() < 1e-4, "慢帧必须走满 1.0 s，实际 {}", slow.time);
        assert!((fast.time - 1.0).abs() < 1e-4);
        assert!(slow.particles.len() > 20, "要够多粒子做统计，实际 {}", slow.particles.len());
        let mean = |ps: &ParticleSystem| {
            let n = ps.particles.len() as f32;
            let (mut px, mut py, mut age) = (0.0f32, 0.0f32, 0.0f32);
            for p in &ps.particles {
                px += p.pos[0];
                py += p.pos[1];
                age += p.age;
            }
            (px / n, py / n, age / n)
        };
        let (sx, sy, sa) = mean(&slow);
        let (fx, fy, fa) = mean(&fast);
        let scale = fx.abs().max(1.0);
        let dx = (sx - fx).abs() / scale;
        let dy = (sy - fy).abs() / fy.abs().max(1.0);
        let da = (sa - fa).abs() / fa.max(1e-3);
        assert!(dx < 0.05, "x 位置偏差 {:.3}% 超 5%", dx * 100.0);
        assert!(dy < 0.05, "y 位置偏差 {:.3}% 超 5%", dy * 100.0);
        assert!(da < 0.05, "平均岁数偏差 {:.3}% 超 5%", da * 100.0);
    }

    /// 参考口径（1/30 s 一步）**必须还是"一步"**：慢帧子步改造不许改变默认像素。
    /// 判据是**逐位**相等：`update(1/30)` 与 `step(1/30)` 的状态（时间/粒子数/首粒子/末粒子）
    /// 完全相同。
    #[test]
    fn single_step_path_is_bit_identical_to_step() {
        let preset: ParticlePreset = serde_json::from_str(
            r#"{
            "emitter":[{"name":"sphererandom","origin":"0 0 0","directions":"1 1 0","distancemin":0,"distancemax":5,"rate":90}],
            "initializer":[{"name":"lifetimerandom","min":3,"max":3},{"name":"sizerandom","min":50,"max":50},
                           {"name":"velocityrandom","min":"100 20 0","max":"100 20 0"}],
            "operator":[{"name":"movement","gravity":"0 -300 0","drag":0.1}],
            "renderer":[{"name":"sprite"}],
            "maxcount":64
        }"#,
        )
        .unwrap();
        let mk = || {
            ParticleSystem::new(
                preset.clone(),
                [10.0, 20.0, 0.0],
                [1.0, 1.0],
                &InstanceOverride::default(),
                11,
            )
        };
        let (mut a, mut b) = (mk(), mk());
        for _ in 0..20 {
            a.update(1.0 / 30.0);
            b.step(1.0 / 30.0);
        }
        assert_eq!(a.time.to_bits(), b.time.to_bits(), "时间必须逐位相同");
        assert_eq!(a.particles.len(), b.particles.len());
        for (pa, pb) in a.particles.iter().zip(b.particles.iter()) {
            assert_eq!(pa.pos[0].to_bits(), pb.pos[0].to_bits());
            assert_eq!(pa.pos[1].to_bits(), pb.pos[1].to_bits());
            assert_eq!(pa.vel[1].to_bits(), pb.vel[1].to_bits());
            assert_eq!(pa.age.to_bits(), pb.age.to_bits());
        }
    }

    /// 上限仍与主循环一致：单圈再长也只吃 0.25 s（不能让"卡 10 秒"变成一步跳完）。
    #[test]
    fn update_still_clamps_a_huge_frame() {
        let preset: ParticlePreset = serde_json::from_str(
            r#"{
            "emitter":[{"name":"sphererandom","origin":"0 0 0","directions":"1 1 0","distancemin":0,"distancemax":1,"rate":30}],
            "initializer":[{"name":"lifetimerandom","min":9,"max":9},{"name":"sizerandom","min":10,"max":10}],
            "renderer":[{"name":"sprite"}],
            "maxcount":32
        }"#,
        )
        .unwrap();
        let mut ps = ParticleSystem::new(
            preset,
            [0.0, 0.0, 0.0],
            [1.0, 1.0],
            &InstanceOverride::default(),
            1,
        );
        ps.update(10.0);
        assert!((ps.time - 0.25).abs() < 1e-6, "10 s 的一圈必须夹到 0.25 s，实际 {}", ps.time);
        ps.update(-1.0);
        assert!((ps.time - 0.25).abs() < 1e-6, "负 dt 不许倒退");
    }

}

