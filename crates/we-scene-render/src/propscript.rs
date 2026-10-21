// SPDX-License-Identifier: MIT
//
//! **场景属性脚本 + WE 音频引擎 API**（`engine.registerAudioBuffers`）。
//!
//! ## 为什么需要这个文件
//! WE 的 `instanceoverride.rate` / `scale` / `visible` / 特效常量这些**绑定**可以写成
//! `{"script": "<WE 编辑器生成的 JS>", "value": <默认值>}`。本机 17 条里真正带脚本的
//! 有 5 条（见下），其中**只有 `instanceoverride.rate`** 是粒子发射率。
//!
//! 在改动前，渲染器对这类绑定**一律只取 `value`**（`json_to_value` / `opt_number`
//! 都是 `m.get("value")`）⇒ 脚本里那些 `engine.registerAudioBuffers(...)` +
//! `audioBuffer.average[i]` **根本没有执行**。本模块补上这条路：
//!
//! * 一个**持久**的 boa 上下文（脚本里的 `smoothValue` 这类状态要跨帧保持）；
//!
//! * `engine` 对象（`registerAudioBuffers` / `frametime` / `AUDIO_RESOLUTION_16|32|64`）；
//!
//! * `init(value)` 一次 + 每帧 `update`，返回值就是该属性的新数值。
//!
//! ## 本机真实用法（侦察结果，逐条来自 `scene.json`）
//!
//! * `2481886649` `/objects[1]` **Ember** → `/instanceoverride/rate/script`
//! （WE 内置"音频响应"模板，`__workshopId=2419741250`：`registerAudioBuffers(16)`、
//! `sourceFrequency=0`、`min/max=0.1/1.0`，返回 `initialValue*(smooth*0.9+0.1)`）
//!
//! * `2997301801` `/objects[2]` **Ember** → `/instanceoverride/rate/script`（同一模板，
//! 静态 `rate.value=0.58`）
//!
//! * `3299815325` `/objects[1]` `visible`（`NUM_BARS=64`）、`/objects[2]` 与 `/objects[22]`
//! 的 `rate`
//!
//! * `3571777169` 14 处：`animationlayers[7]/blend`、6 个特效 `constantshadervalues/strength`、
//! 5 个 `instanceoverride/rate`（Petals×3、Туман 2×2）
//!
//! * `3704273480` `/objects[22]` `scale`
//! ⇒ 全部只读 `audioBuffer.average[...]`（`grep` 无 `.left` / `.right` / `.buffer`）。
//! 本次（「场景 JS 通道」）在原来只接通 `instanceoverride.rate` 的基础上补齐：
//!
//! * **完整 `engine` getter 面**：`runtime` / `timeOfDay` / `canvasSize` / `screenResolution`
//! / `isRunningInEditor` / `isScreensaver` / `userProperties`（OWE `EngineGetter*`）。
//! `engine.runtime` 由渲染器每帧按**场景时间**下发（实测 `3704273480` obj[9] 的 angles
//! 脚本就读它）。
//!
//! * **`Vec2`/`Vec3`/`Vec4` 三个类**（逐字取 OWE bootstrap）：本机 `3299815325` obj[1]
//! 的音频条脚本 `new Vec3(...)` / `copy`、`3704273480` obj[9] 的 `value.z` 都要它。
//!
//! * **`console`**（no-op，OWE 同）：本机 2 条脚本会调 `console.error/log`。
//!
//! * **绑定级 `scriptproperties` 覆盖**（**数据驱动**：读 `instanceoverride.<字段>.scriptproperties`）：
//! WE 里编辑器写在绑定里的值**优先**于脚本 `.addSlider({value:…})` 的默认值。实测
//! `3299815325` obj[2]、`3571777169` obj[46]/[47] 的 rate 绑定都带 scriptproperties，
//! 与脚本默认**不一致**（例如 minvalue 0.25 vs 0.1、frequency 15 vs 0）。
//!
//! ## §边界：仍未接的属性绑定（都按老路只取 `value`，已定位）
//! 这些字段的**策略脚本本机真实存在**，但要把它们接到画面上需要各自的按帧/装配管线：
//!
//! * **`angles` / `scale` / `origin`（Vec3 字段）**：`3704273480` obj[9]（angles，`engine.runtime`）、
//! obj[22]（scale，音频）。渲染侧对象变换在 `build` 期一次性烘进 `Pass.mvp`
//! （`lib.rs` 的 `world_of` 父链折叠 + `Mat4`），**没有**按帧重算的口子；要接通须给
//! `Pass` 存一份"重建 MVP 的配方"再逐帧推 uniform（类比 `MeshAnim` 的做法）。工程量大、风险高（`world_of` 有
//! 多路 mirror/mesh_legacy/yfix 分支），本次**未做**。
//!
//! * **`visible`（bool）**：`3299815325` obj[1]、`3704273480` obj[15]×2 / obj[33]×2。
//! 其中 obj[15]/obj[33] 的脚本走 `thisObject.getAnimation.setFrame(...)`（改动画初帧、返回原值），
//! obj[1] 的返回 `undefined` ⇒ **接通 visible 本身几乎看不出差别**；真要有效果得实现
//! `thisObject` / 动画 API（见下）。
//!
//! * **特效常量 `effects[].passes[].constantshadervalues.strength`（标量）**：`3571777169` 6 处。
//! 要接到特效趟的 uniform（`Pass.scalars`）上。
//!
//! * **`animationlayers[].blend`（标量）**：`3571777169` obj[24]。要接到 puppet 混合权重。
//!
//! * **`thisObject` / `thisLayer` / `thisScene` / `Vec2/Vec3` 的"活"节点包装**：`3704273480`
//! obj[15]/obj[33] 的动画脚本用 `thisObject`；`3299815325` obj[1] 的音频条脚本用
//! `thisLayer` + `thisScene.createLayer/sortLayer/getLayerIndex`（**动态建 64 个子图层**）。
//! OWE 有整套实现（`Script.cpp` 的 `JsRuntime::LayerFactory` / `SceneNode` 包装），
//! 我们完全没有 ⇒ 属**独立一轮**，不在本次范围。
//!
//! * **`children`（子粒子/子对象）**：见仓库 说明 的调研。
//!
//! ## 语义依据（OWE 对照，linux-WE 无此实现）
//! 权威实现在 `open-wallpaper-engine/src/Scene/Script/Script.cpp`：
//!
//! * `EngineRegisterAudioBuffers`（:828）→ `NormalizeAudioResolution`（:581）：
//! `<=16 ⇒ 16`、`<=32 ⇒ 32`、否则 `64`；返回**同一个** slot 对象（:837-840），
//! 即"注册一次、以后每帧原地刷新"。
//!
//! * `MakeAudioBuffer`（:785）返回 `{left, right, average, buffer}`，其中
//! `average = buffer = scene_audio::Channel::Average`（**左右平均**，:794/:798）。
//!
//! * `RefreshAudioBuffer(s)`（:807）每帧把 slot 里的数组**原地**写新值
//! （`SetAudioArrayValue` 用 `JS_DefinePropertyValueUint32`）⇒ 脚本持有的
//! `audioBuffer` 引用是**活**的，和 WE 一致。
//!
//! * `engine.frametime`（:759 `EngineGetterFrametime` = 上一帧耗时秒数）。
//!
//! * `AUDIO_RESOLUTION_16|32|64`（:1432-1434）。
//!
//! * 生命周期：`RunFieldScriptInit` 调一次 `init(current_value)`（`current_value`
//! = 该字段解析出来的**静态值**，:4065），之后每帧 `update`（:3937 有参 / :3942 无参）。
//!
//! * `linux-wallpaperengine` **没有** JS 脚本引擎（只 vendored 了 quickjs 源码，
//! `src/WallpaperEngine` 里没有任何 `registerAudioBuffers`）⇒ **不存在两派分歧**，
//! 按 OWE 一家实现。
//!
//! ### 我们与 OWE 的**已知差异**（都写在这里，不藏）
//!
//! 1. **分辨率**：OWE 同时维护 16/32/64 三档（各自由同一份 64 段响应池化而来）。
//! 我们进程里只有 **16 段**（`crate::audio::BANDS`），所以 16 档是精确值；
//! 32/64 档用**最近邻上采样**（`i>>1` / `i>>2`）近似 —— 依据强度**弱**，但本机
//! 没有任何脚本请求 32/64（全是 `registerAudioBuffers(16)` / `AUDIO_RESOLUTION_16`），
//! 所以这条对本机 17 条**零影响**。
//!
//! 2. **left/right**：OWE 是分声道数组；我们的 16 段就是**左右平均后的**
//! 能量（`audio.rs` 的 DSP 第 3 步），没有单声道的分量。这里令
//! `left = right = average`（脚本只读 `average`，所以观察不到差异）。
//!
//! 3. **`size` 之类静态度**：OWE 每次 tick 会把脚本返回写回属性；我们只在
//! `rate` 上做（见 §边界）。
//!
//! ## 开关（**默认关，关时逐字节恒等**）
//! `WE_SCRIPT_AUDIO=1` 才启用。不设时：本模块的 `NumScript` 一个都不建
//! ⇒ `instanceoverride.rate` 仍取静态 `value`（= 改动前那条路）⇒ 17 条逐字节不变。
//! 打开时**不必**同时开 `WE_AUDIO_SPECTRUM`：没喂频谱（全 0）时脚本照跑，
//! `average[i]=0` ⇒ 返回 `initialValue*minimumValue`（WE 在静音下的行为）。
//! （⚠ 这是一个**有意的偏差**：真 WE 里这些脚本永远在跑；我们为了"默认逐字节不变"
//! 把它挂在开关后面。）

use crate::audio::Spectrum;
use boa_engine::{Context, Source};

/// 总开关：`WE_SCRIPT_AUDIO=1`。**默认关**（不设 = 属性脚本不执行 = 与改动前完全一致）。
pub fn enabled() -> bool {
    std::env::var("WE_SCRIPT_AUDIO").ok().as_deref() == Some("1")
}

/// WE `engine` 垫片 + 音频缓冲槽。这段是**固定**的（不随帧变），只 eval 一次。
///
/// 与 OWE 的对应：`registerAudioBuffers` 返回**同一个**对象（注册一次、之后原地刷新）；
/// `__we_fill` 就是 `RefreshAudioBuffer` 的等价物（把 16 段铺进 average/buffer/left/right）。
const PRELUDE: &str = r#"
var engine = {
    AUDIO_RESOLUTION_16: 16,
    AUDIO_RESOLUTION_32: 32,
    AUDIO_RESOLUTION_64: 64,
    frametime: 0,
    // 本次补齐的 engine 只读量（OWE Script.cpp 的 `EngineGetter*`，见 §语义依据）：
    // `runtime` = 场景开始以来的秒数；`timeOfDay`/`canvasSize`/`screenResolution` 同为
    // 每帧刷新的快照（本机脚本只用到 `runtime`，其余一起补上以免再出现"垫片缺符号"）。
    runtime: 0,
    timeOfDay: 0,
    canvasSize: null,
    screenResolution: null,
    userProperties: {},
    // WE 把它们当**零参查询函数**（部分脚本直接当布尔读）——都返回 false（OWE 同）。
    isRunningInEditor: function () { return false; },
    isScreensaver: function () { return false; }
};
var __we_audio = {};
// OWE NormalizeAudioResolution（Script.cpp:581）：<=16 ⇒ 16，<=32 ⇒ 32，否则 64
function __we_norm(res) {
    res = Number(res);
    if (!(res > 16)) return 16;
    if (res <= 32) return 32;
    return 64;
}
engine.registerAudioBuffers = function (res) {
    res = __we_norm(res);
    var slot = __we_audio[res];
    if (!slot) {
        var a = [];
        for (var i = 0; i < res; i++) a.push(0);
        slot = { average: a.slice(), buffer: a.slice(), left: a.slice(), right: a.slice() };
        __we_audio[res] = slot;
    }
    return slot;
};
// bands 永远是 16 段（我们进程里的口径）；32/64 档最近邻上采样（本机无脚本用）
function __we_fill(res, bands) {
    var slot = __we_audio[res];
    if (!slot) return;
    for (var i = 0; i < res; i++) {
        var j = (res === 16) ? i : ((res === 32) ? (i >> 1) : (i >> 2));
        var v = (j < bands.length) ? bands[j] : 0;
        slot.average[i] = v;
        slot.buffer[i] = v;
        slot.left[i] = v;
        slot.right[i] = v;
    }
}
// ── Vec2 / Vec3 / Vec4（逐字取 OWE bootstrap 的纯 JS 实现，Script.cpp:1449+）────────
// 本机用到：`3704273480` obj[33]/obj[9] 的脚本 `value.x`；`3299815325` obj[1] 的
// 音频条脚本 `new Vec3(...)` / `baseOrigin.copy` / `new Vec2(0,0)`。
class Vec2 {
  constructor(x, y) {
    if (typeof x === 'object' && x !== null) { this.x = x.x ?? 0; this.y = x.y ?? 0; return; }
    if (typeof x === 'number' && y === undefined) { this.x = x; this.y = x; return; }
    this.x = (typeof x === 'number') ? x : 0;
    this.y = (typeof y === 'number') ? y : 0;
  }
  add(o)      { return new Vec2(this.x + (o.x ?? o), this.y + (o.y ?? o)); }
  subtract(o) { return new Vec2(this.x - (o.x ?? o), this.y - (o.y ?? o)); }
  multiply(o) { return new Vec2(this.x * (o.x ?? o), this.y * (o.y ?? o)); }
  divide(o)   { return new Vec2(this.x / (o.x ?? o), this.y / (o.y ?? o)); }
  mix(o, t)   { return new Vec2(this.x + ((o.x ?? o) - this.x) * t, this.y + ((o.y ?? o) - this.y) * t); }
  copy()      { return new Vec2(this.x, this.y); }
  clone()     { return new Vec2(this.x, this.y); }
  toString()  { return this.x + ' ' + this.y; }
  length()    { return Math.sqrt(this.x*this.x + this.y*this.y); }
  lengthSqr() { return this.x*this.x + this.y*this.y; }
  normalize() { var l = this.length(); return l > 0 ? this.divide(l) : new Vec2(0, 0); }
}
class Vec3 {
  constructor(x, y, z) {
    if (typeof x === 'object' && x !== null) { this.x = x.x ?? 0; this.y = x.y ?? 0; this.z = x.z ?? 0; }
    else if (typeof x === 'number' && y === undefined && z === undefined) { this.x = x; this.y = x; this.z = x; }
    else {
      this.x = (typeof x === 'number') ? x : 0;
      this.y = (typeof y === 'number') ? y : 0;
      this.z = (typeof z === 'number') ? z : 0;
    }
  }
  add(o)      { return new Vec3(this.x + (o.x ?? o), this.y + (o.y ?? o), this.z + (o.z ?? o)); }
  subtract(o) { return new Vec3(this.x - (o.x ?? o), this.y - (o.y ?? o), this.z - (o.z ?? o)); }
  multiply(o) { return new Vec3(this.x * (o.x ?? o), this.y * (o.y ?? o), this.z * (o.z ?? o)); }
  divide(o)   { return new Vec3(this.x / (o.x ?? o), this.y / (o.y ?? o), this.z / (o.z ?? o)); }
  mix(o, t)   { return new Vec3(this.x + ((o.x ?? o) - this.x) * t, this.y + ((o.y ?? o) - this.y) * t, this.z + ((o.z ?? o) - this.z) * t); }
  copy()      { return new Vec3(this.x, this.y, this.z); }
  clone()     { return new Vec3(this.x, this.y, this.z); }
  toString()  { return this.x + ' ' + this.y + ' ' + this.z; }
  length()    { return Math.sqrt(this.x*this.x + this.y*this.y + this.z*this.z); }
  lengthSqr() { return this.x*this.x + this.y*this.y + this.z*this.z; }
  normalize() { var l = this.length(); return l > 0 ? this.divide(l) : new Vec3(0, 0, 0); }
}
class Vec4 {
  constructor(x, y, z, w) {
    if (typeof x === 'object' && x !== null) { this.x = x.x ?? 0; this.y = x.y ?? 0; this.z = x.z ?? 0; this.w = x.w ?? 0; }
    else if (typeof x === 'number' && y === undefined && z === undefined && w === undefined) { this.x = x; this.y = x; this.z = x; this.w = x; }
    else {
      this.x = (typeof x === 'number') ? x : 0;
      this.y = (typeof y === 'number') ? y : 0;
      this.z = (typeof z === 'number') ? z : 0;
      this.w = (typeof w === 'number') ? w : 0;
    }
  }
  add(o)      { return new Vec4(this.x + (o.x ?? o), this.y + (o.y ?? o), this.z + (o.z ?? o), this.w + (o.w ?? o)); }
  subtract(o) { return new Vec4(this.x - (o.x ?? o), this.y - (o.y ?? o), this.z - (o.z ?? o), this.w - (o.w ?? o)); }
  multiply(o) { return new Vec4(this.x * (o.x ?? o), this.y * (o.y ?? o), this.z * (o.z ?? o), this.w * (o.w ?? o)); }
  divide(o)   { return new Vec4(this.x / (o.x ?? o), this.y / (o.y ?? o), this.z / (o.z ?? o), this.w / (o.w ?? o)); }
  copy()      { return new Vec4(this.x, this.y, this.z, this.w); }
  clone()     { return new Vec4(this.x, this.y, this.z, this.w); }
  toString()  { return this.x + ' ' + this.y + ' ' + this.z + ' ' + this.w; }
}
// ── console（OWE 的 `__noop` 版；本机 `3299815325`/`3704273480` 里脚本会调 console.error/log）
var console = {
    log: function () {}, warn: function () {}, error: function () {}, info: function () {},
    debug: function () {}, trace: function () {}, table: function () {},
    group: function () {}, groupCollapsed: function () {}, groupEnd: function () {}
};
"#;

/// 一个**数值型**场景属性脚本（本机只用于 `instanceoverride.rate`）。
///
/// 持有自己的 boa 上下文 ⇒ 脚本里的 `smoothValue` / `audioBuffer` 跨帧保持。
pub struct NumScript {
    ctx: Context,
    /// 脚本注册过的分辨率（16/32/64 的子集）——每帧只填这些，别白填。
    resolutions: Vec<u32>,
    /// `init(value)` 收到的静态值（= 属性的 `value`）——`update` 不存在时的兜底返回值。
    init_value: f32,
    /// `update(value)` 带参（WE 的 value-in/value-out 脚本）时为真；本机那两个是 `update`。
    takes_arg: bool,
    cur: f64,
    ok: bool,
}

impl NumScript {
    /// 装载：eval 垫片 + 脚本正文，然后调一次 `init(value)`（OWE `RunFieldScriptInit`）。
///
    /// `scriptproperties` = 绑定对象里的 `scriptproperties`（编辑器写进去的值）——
    /// **数据驱动地**覆盖脚本里 `.addSlider({value:…})` 的默认值（WE 语义）。给 `None`
    /// 或空对象时与原行为逐位相同。
    pub fn new(
        script: &str,
        init_value: f32,
        scriptproperties: Option<&serde_json::Value>,
    ) -> Result<Self, String> {
        let mut ctx = Context::default();
        // ① `createScriptProperties`（与文字脚本共用同一份实现）
        ctx.eval(Source::from_bytes(crate::text::script_properties_shim()))
            .map_err(|e| format!("属性垫片执行失败：{e}"))?;
        // ② `engine`（音频缓冲槽 / frametime / runtime / AUDIO_RESOLUTION_*）+ Vec2/3/4 + console
        ctx.eval(Source::from_bytes(PRELUDE))
            .map_err(|e| format!("引擎垫片执行失败：{e}"))?;
        if let Some(js) = crate::text::clock_shim()
        {
            ctx.eval(Source::from_bytes(js.as_str()))
                .map_err(|e| format!("时钟垫片执行失败：{e}")) ?;
        }
        // ③ 绑定级 `scriptproperties` 覆盖（必须在脚本正文 eval **之前**填好）
        if let Some(sp) = scriptproperties {
            if sp.is_object() {
                let lit = serde_json::to_string(sp).unwrap_or_else(|_| "{}".into());
                ctx.eval(Source::from_bytes(&format!("__we_overrides = {lit};")))
                    .map_err(|e| format!("scriptproperties 注入失败：{e}"))?;
            }
        }

        // WE 的模块导出语法（`export let/var/function`）不是脚本语法，去掉 `export ` 再当普通脚本跑
        // （与 `crate::text::eval_script` 同一套口径）。
        let body = script
            .replace("export let ", "var ")
            .replace("export var ", "var ")
            .replace("export function ", "function ");
        ctx.eval(Source::from_bytes(&body))
            .map_err(|e| format!("脚本执行失败：{e}"))?;

        // `init(value)`：WE 传的是该字段解析出来的静态值。
        let init_lit = js_num(init_value);
        ctx.eval(Source::from_bytes(&format!(
            "if (typeof init === 'function') {{ init({init_lit}); }}"
        )))
        .map_err(|e| format!("init() 调用失败：{e}"))?;

        // 记下脚本注册了哪些分辨率（`registerAudioBuffers` 通常写在模块顶层）
        let res_str = ctx
            .eval(Source::from_bytes("Object.keys(__we_audio).join(',')"))
            .map_err(|e| format!("读音频缓冲槽失败：{e}"))?
            .to_string(&mut ctx)
            .map_err(|e| e.to_string())?
            .to_std_string_escaped();
        let mut resolutions: Vec<u32> = res_str
            .split(',')
            .filter_map(|t| t.trim().parse::<u32>().ok())
            .collect();
        if resolutions.is_empty() {
            // 脚本没注册（或注册写在 update 里，本机没有）⇒ 兜底 16，保证任何 `average[i]` 可读
            resolutions.push(16);
        }

        let takes_arg = ctx
            .eval(Source::from_bytes(
                "(typeof update === 'function') && update.length > 0",
            ))
            .map(|v| v.as_boolean().unwrap_or(false))
            .unwrap_or(false);

        Ok(NumScript {
            ctx,
            resolutions,
            init_value,
            takes_arg,
            cur: init_value as f64,
            ok: true,
        })
    }

    /// 推进一帧：写 `engine.frametime`/`engine.runtime` + 刷新音频缓冲，调 `update`，返回数值。
    /// `runtime` = 场景开始以来的秒数（OWE `EngineGetterRuntime` ⇒ `inputs.runtime`）。
    /// 出错时返回上一次的值（不 panic、不中断渲染；脚本挂了只影响这一个属性）。
    pub fn update(&mut self, dt: f32, runtime: f32, spec: &Spectrum) -> f32 {
        if !self.ok {
            return self.cur as f32;
        }
        let mut src = String::with_capacity(256);
        src.push_str("engine.frametime = ");
        src.push_str(&js_num(dt));
        src.push_str(";engine.runtime = ");
        src.push_str(&js_num(runtime));
        src.push_str(";");
        // 只填脚本注册过的分辨率（本机全是 16）
        let bands = &spec.bands;
        for res in self.resolutions.clone() {
            src.push_str("__we_fill(");
            src.push_str(&res.to_string());
            src.push_str(",[");
            for (i, b) in bands.iter().enumerate() {
                if i > 0 {
                    src.push(',');
                }
                src.push_str(&js_num(*b));
            }
            src.push_str("]);");
        }
        // 返回值 = 最后那个表达式的值（boa 的 eval 返回脚本的完成值）
        if self.takes_arg {
            src.push_str(&format!("update({})", js_num(self.cur as f32)));
        } else {
            src.push_str("(typeof update === 'function') ? update() : ");
            src.push_str(&js_num(self.init_value));
            src.push(';');
        }
        match self.ctx.eval(Source::from_bytes(&src)) {
            Ok(v) => match v.to_number(&mut self.ctx) {
                Ok(x) if x.is_finite() => {
                    self.cur = x;
                    let out = x as f32;
                    // 夹到非负：发射率乘子在 [0, …)；负值会让 spawn_acc 倒扣
                    out.max(0.0)
                }
                Ok(_) | Err(_) => {
                    self.ok = false;
                    self.cur as f32
                }
            },
            Err(e) => {
                eprintln!("[脚本] rate 脚本 update() 失败（保持上次值 {:.4}）：{e}", self.cur);
                self.ok = false;
                self.cur as f32
            }
        }
    }

    /// 当前值（调试/报告用）
    pub fn value(&self) -> f32 {
        self.cur as f32
    }
}

/// f32 → JS 数值字面量（`{}` 是 Rust 的最短可回环表示 ⇒ 确定、不丢精度）。
#[inline]
fn js_num(v: f32) -> String {
    if v.is_finite() {
        format!("{v}")
    } else {
        "0".to_string()
    }
}

/// 从 `instanceoverride` 的原始 JSON 里取某个字段的策略脚本。
///
/// 只在**该字段确实是** `{"script": "...", ...}` 且开关打开时构造 `NumScript`；
/// 否则返回 `None`（调用方继续用静态 `value`）。
/// `static_value` 是同一字段解析出来的数（`{"value": …}` / 直接数字）——它就是 `init(value)` 的入参。
pub fn field_script(
    override_json: Option<&serde_json::Value>,
    field: &str,
    static_value: Option<f32>,
) -> Option<NumScript> {
    if !enabled() {
        return None;
    }
    let binding = override_json?.get(field)?.as_object()?;
    let script = binding.get("script")?.as_str()?;
    // 绑定对象里的 `scriptproperties`（编辑器值）——数据驱动，不看壁纸 id / 属性名
    let scriptproperties = binding.get("scriptproperties");
    match NumScript::new(script, static_value.unwrap_or(1.0), scriptproperties) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("[脚本] {field} 脚本装载失败，退回静态值：{e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::BANDS;

    /// WE 内置"音频响应"模板（与 `2481886649` / `2997301801` 里那两段逐字相同）
    const WE_AUDIO_TEMPLATE: &str = r#"'use strict';
export let __workshopId = '2419741250';
let frequencyResolution = 16;
let sourceFrequency = 0;
let smoothingRate = 16;
let minimumValue = 0.1;
let maximumValue = 1.0;
let audioBuffer = engine.registerAudioBuffers(frequencyResolution);
var smoothValue = 0;
var initialValue;
const valueDelta = maximumValue - minimumValue;
export function update() {
	let audioDelta = audioBuffer.average[sourceFrequency] - smoothValue;
	smoothValue += audioDelta * engine.frametime * smoothingRate;
	smoothValue = Math.min(1.0, smoothValue);
	return initialValue * (smoothValue * valueDelta + minimumValue);
}
export function init(value) {
	initialValue = value;
}
"#;

    fn spec(band0: f32) -> Spectrum {
        let mut s = Spectrum::default();
        s.bands[0] = band0;
        s.level = band0 / BANDS as f32;
        s
    }

    #[test]
    fn template_runs_and_returns_min_at_silence() {
        let mut s = NumScript::new(WE_AUDIO_TEMPLATE, 0.58, None).expect("脚本应当能跑");
        assert_eq!(s.resolutions, vec![16], "模板注册的是 16 档");
        // 静音：average[0]=0 ⇒ smoothValue 停在 0 ⇒ 返回 initial*minimumValue = 0.58*0.1
        for _ in 0..30 {
            let v = s.update(1.0 / 30.0, 0.0, &Spectrum::default());
            assert!((v - 0.058).abs() < 1e-5, "静音应当返回 initial*0.1，实得 {v}");
        }
    }

    #[test]
    fn template_ramps_up_with_audio_band_and_caps_at_max() {
        let mut s = NumScript::new(WE_AUDIO_TEMPLATE, 1.0, None).expect("脚本应当能跑");
        // 满幅 bass（average[0]=1）：smoothValue 按 16/s 上升，约 0.2s 到 1
        let mut last = 0.0;
        for _ in 0..60 {
            last = s.update(1.0 / 30.0, 0.0, &spec(1.0));
        }
        assert!(last > 0.99, "2 秒满幅后应当逼近 maximumValue，实得 {last}");
        // 边界：夹到 minimumValue..maximumValue ⇒ [0.1,1.0]
        assert!(last <= 1.0 + 1e-6);
    }

    #[test]
    fn audio_band_change_is_visible_in_returned_value() {
        // 同一份脚本：低段 vs 静音必须给出**不同**的返回值（这就是"接通音频"的判据）
        let mut s_off = NumScript::new(WE_AUDIO_TEMPLATE, 0.58, None).unwrap();
        let mut s_on = NumScript::new(WE_AUDIO_TEMPLATE, 0.58, None).unwrap();
        let mut off = 0.0;
        let mut on = 0.0;
        for _ in 0..45 {
            off = s_off.update(1.0 / 30.0, 0.0, &Spectrum::default());
            on = s_on.update(1.0 / 30.0, 0.0, &spec(0.9));
        }
        assert!(on > off + 0.1, "有音频时应当明显更大：off={off} on={on}");
    }

    #[test]
    fn register_audio_buffers_normalizes_resolution_like_owe() {
        // OWE NormalizeAudioResolution：<=16 ⇒ 16；<=32 ⇒ 32；否则 64
        let script = r#"
var a = engine.registerAudioBuffers(8);
var b = engine.registerAudioBuffers(16);
var c = engine.registerAudioBuffers(20);
var d = engine.registerAudioBuffers(64);
var e = engine.registerAudioBuffers(engine.AUDIO_RESOLUTION_32);
export function update() { return a.average.length * 1000 + b.average.length * 100 + c.average.length * 10 + d.average.length; }
"#;
        let mut s = NumScript::new(script, 0.0, None).unwrap();
        let v = s.update(1.0 / 30.0, 0.0, &spec(0.5)) as i64;
        // a=16, b=16, c=32, d=64 ⇒ 16000 + 1600 + 320 + 64 = 17984
        assert_eq!(v, 17984, "分辨率归一化应当照 OWE：8→16, 16→16, 20→32, 64→64");
    }

    #[test]
    fn audio_array_is_refreshed_in_place_each_frame() {
        // 活引用：脚本持有 audioBuffer 引用，每帧读到的是**当帧**的段值
        let script = r#"
var buf = engine.registerAudioBuffers(16);
export function update() { return buf.average[3]; }
"#;
        let mut s = NumScript::new(script, 0.0, None).unwrap();
        let mut sp = Spectrum::default();
        sp.bands[3] = 0.25;
        assert!((s.update(1.0 / 30.0, 0.0, &sp) - 0.25).abs() < 1e-6);
        sp.bands[3] = 0.75;
        assert!((s.update(1.0 / 30.0, 0.0, &sp) - 0.75).abs() < 1e-6);
    }

    #[test]
    fn script_with_create_script_properties_also_loads() {
        // 实测 `3299815325` 的 `Частинки пилу`：rate 脚本先调 createScriptProperties
        // 声明编辑器属性，再 registerAudioBuffers。缺 `createScriptProperties` 垫片时
        // 会 ReferenceError ⇒ 整段脚本装载失败（实测过）。
        let script = r#"
export var scriptProperties = createScriptProperties()
    .addSlider({ name: 'freq', label: 'f', value: 0, min: 0, max: 15, integer: true })
    .addSlider({ name: 'gain', label: 'g', value: 2.0, min: 0, max: 10, integer: false })
    .finish();
let buf = engine.registerAudioBuffers(engine.AUDIO_RESOLUTION_16);
export function init(value) { globalThis.__init0 = value; }
export function update() { return globalThis.__init0 + buf.average[scriptProperties.freq] * scriptProperties.gain; }
"#;
        let mut s = NumScript::new(script, 1.0, None).expect("带 createScriptProperties 的脚本也应当能跑");
        let mut sp = Spectrum::default();
        sp.bands[0] = 0.5;
        let v = s.update(1.0 / 30.0, 0.0, &sp);
        assert!((v - (1.0 + 0.5 * 2.0)).abs() < 1e-5, "实得 {v}");
    }

    #[test]
    fn missing_update_falls_back_to_static_value() {
        let mut s = NumScript::new("var x = 1;", 0.42, None).unwrap();
        assert!((s.update(1.0 / 30.0, 0.0, &Spectrum::default()) - 0.42).abs() < 1e-6);
    }

    #[test]
    fn field_script_only_picks_real_script_objects() {
        // 静态数字 / 绑定对象 ⇒ None（调用方继续用静态 value）
        assert!(field_script(Some(&serde_json::json!({"rate": 0.58})), "rate", Some(0.58)).is_none());
        assert!(field_script(
            Some(&serde_json::json!({"rate": {"user": "_6", "value": 0.95}})),
            "rate",
            Some(0.95)
        )
        .is_none());
        // 开关没开 ⇒ 即使有 script 也不建
        let std::result::Result::Ok(_) = std::env::var("WE_SCRIPT_AUDIO") else {
            assert!(field_script(
                Some(&serde_json::json!({"rate": {"script": "x", "value": 1.0}})),
                "rate",
                Some(1.0)
            )
            .is_none());
            return;
        };
    }

    // ── 本次新增：engine.runtime + 绑定级 scriptproperties 覆盖 ──────────────────

    /// 实测 `3704273480` `/objects[9]` 的 `angles` 脚本用的就是 `engine.runtime`
    /// （`value.z = sin(runtime*0.4)*3`）。`angles` 是 Vec3 字段、返回值是对象，
    /// 本次的按帧消费口只有 `rate`（标量），所以单测用同一只 `runtime` 的**标量**形式
    /// 直接证明"runtime 每帧是活的"—— 这正是那条 angles 脚本唯一依赖的输入。
    #[test]
    fn engine_runtime_is_wired_and_visible_in_returned_value() {
        let script = "export function update() { return engine.runtime; }";
        let mut s = NumScript::new(script, 0.0, None).unwrap();
        assert!((s.update(1.0 / 30.0, 0.0, &Spectrum::default()) - 0.0).abs() < 1e-6);
        assert!((s.update(1.0 / 30.0, 2.0, &Spectrum::default()) - 2.0).abs() < 1e-6);
        assert!((s.update(1.0 / 30.0, 5.0, &Spectrum::default()) - 5.0).abs() < 1e-6);
        // 与 angles 脚本同形（`sin(runtime*0.4)*3`）：t=5 ⇒ sin(2)*3，t=0 ⇒ 0
        let script2 = "export function update() { return Math.sin(engine.runtime * 0.4) * 3; }";
        let mut s2 = NumScript::new(script2, 0.0, None).unwrap();
        assert!((s2.update(1.0 / 30.0, 0.0, &Spectrum::default()) - 0.0).abs() < 1e-6);
        let want = (5.0f32 * 0.4).sin() * 3.0;
        assert!(
            (s2.update(1.0 / 30.0, 5.0, &Spectrum::default()) - want).abs() < 1e-5,
            "runtime=5 应给出 sin(2)*3={want}"
        );
    }

    /// 实测 `3299815325` `/objects[2]` 的 rate 绑定：脚本里 `minvalue` 默认 0.8，
    /// 但绑定自带的 `scriptproperties.minvalue` 是 0.25 —— WE 用**编辑器值**。
    const P3299_RATE: &str = r#"
export var scriptProperties = createScriptProperties()
	.addSlider({ name: 'frequency', value: 0, min: 0, max: 15, integer: true })
	.addSlider({ name: 'smoothing', value: 15, min: 0, max: 25, integer: false })
	.addSlider({ name: 'minvalue', value: 0.8, min: 0, max: 3, integer: false })
	.addSlider({ name: 'maxvalue', value: 1.2, min: 0, max: 3, integer: false })
	.finish();
const audioBuffer = engine.registerAudioBuffers(engine.AUDIO_RESOLUTION_16);
let smoothValue = 0;
let initialValue;
export function update() {
	const valueDelta = scriptProperties.maxvalue - scriptProperties.minvalue;
	const audioDelta = audioBuffer.average[scriptProperties.frequency] - smoothValue;
	smoothValue += audioDelta * Math.min(1.0, engine.frametime * scriptProperties.smoothing);
	smoothValue = Math.min(1.0, smoothValue);
	return initialValue * (smoothValue * valueDelta + scriptProperties.minvalue);
}
export function init(value) { initialValue = (typeof value === 'number') ? value : value.x; }
"#;

    #[test]
    fn scriptproperties_from_binding_override_script_defaults() {
        // 静音下返回值 = initialValue * minvalue ⇒ 直接读出 minvalue
        let mut no_sp = NumScript::new(P3299_RATE, 1.0, None).unwrap();
        let v0 = no_sp.update(1.0 / 30.0, 0.0, &Spectrum::default());
        assert!((v0 - 0.8).abs() < 1e-5, "无覆盖时用脚本默认 minvalue=0.8，实得 {v0}");

        let sp = serde_json::json!({"frequency":15,"maxvalue":2.5,"minvalue":0.25,"smoothing":2.5});
        let mut with_sp = NumScript::new(P3299_RATE, 1.0, Some(&sp)).unwrap();
        let v1 = with_sp.update(1.0 / 30.0, 0.0, &Spectrum::default());
        assert!((v1 - 0.25).abs() < 1e-5, "覆盖后应取 scriptproperties.minvalue=0.25，实得 {v1}");
    }

    #[test]
    fn scriptproperties_frequency_selects_the_band() {
        // frequency=15 ⇒ 读第 15 段（脚本默认是第 0 段）；给第 15 段能量看它是否被选中
        let sp = serde_json::json!({"frequency":15,"smoothing":2.5,"minvalue":0.25,"maxvalue":2.5});
        let mut s = NumScript::new(P3299_RATE, 1.0, Some(&sp)).unwrap();
        let mut spec15 = Spectrum::default();
        spec15.bands[15] = 1.0;
        let mut last = 0.0;
        for _ in 0..80 {
            last = s.update(1.0 / 30.0, 0.0, &spec15);
        }
        // 满幅第 15 段 ⇒ smoothValue→1 ⇒ maxvalue=2.5
        assert!(last > 2.0, "覆盖后的 frequency=15 应当吃到第 15 段，实得 {last}");
    }

    /// Vec2/Vec3（OWE bootstrap 的实现，本次照搬）——`3299815325` obj[1] 音频条脚本
    /// 用到 `new Vec3(标量)` 的**三轴铺开**与 `copy`；`3704273480` obj[9] 用 `value.x/.z`。
    #[test]
    fn vec_classes_match_owe_semantics() {
        let script = r#"
export function update() {
    var a = new Vec3(2.5);           // 单数字 ⇒ 三轴铺开
    var b = a.copy(); b.z = 7;
    var c = new Vec2(1, 2);
    return a.x * 100 + a.y * 10 + a.z + b.z + c.x;   // 2.5,2.5,2.5 → 25+2.5+2.5 + 7 + 1
}
"#;
        let mut s = NumScript::new(script, 0.0, None).unwrap();
        let v = s.update(1.0 / 30.0, 0.0, &Spectrum::default());
        // a=(2.5,2.5,2.5) ⇒ 2.5*100 + 2.5*10 + 2.5 = 277.5；+ b.z(7) + c.x(1) = 285.5
        assert!((v - 285.5).abs() < 1e-4, "实得 {v}");
    }

    /// `console.*` 必须可用且是 no-op（本机脚本会调 console.error/log；OWE 也是 no-op）。
    #[test]
    fn console_is_available_and_noop() {
        let script = r#"
export function update() {
    console.error('x', 1);
    console.log('y');
    console.warn('z');
    return 42;
}
"#;
        let mut s = NumScript::new(script, 0.0, None).unwrap();
        assert!((s.update(1.0 / 30.0, 0.0, &Spectrum::default()) - 42.0).abs() < 1e-6);
    }
}
