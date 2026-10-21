// SPDX-License-Identifier: MIT
//
//! `scene.json` 场景图。
//!
//! 顶层只有三个键：`camera` / `general` / `objects`（**对真实素材实测**）。
//! 对象只有四类：`image`（贴图）/ `model`（MDLV 骨骼）/ `particle`（**键名是单数**）/ `sound`（配乐）。
//! 特效是**声明式**的：`effects[].passes[]` 里写"用哪个 shader + 哪些常量 + 哪些 combo + 哪些纹理"，
//! shader 本体打包在壁纸里。`_rt_*` 形式的纹理名是**渲染目标引用**（上一趟的输出）。

use serde::Deserialize;
use std::collections::BTreeMap;

use crate::{Error, Result};

pub type Value = serde_json::Value;

/// WE 的**"用户属性"覆盖表**（本机实测的语义，对齐 OWE `SceneUserPropertyApplier`）。
///
/// 壁纸作者在 `project.json` 的 `general.properties` 里声明"用户属性"
/// （名字 + 类型 + **默认值**，例如 `newproperty: {type:"bool", value:false, text:"右腿丝袜"}`），
/// 场景里则用 `"visible": {"user": "<属性名>", "value": <默认值>}` 把某个**对象**或**特效**的
/// 显隐绑到该属性上。用户在 WE 界面里改属性 ⇒ 绑定的对象/特效跟着显隐。
///
/// 我们没法开 WE 界面，所以这里给一个**同名开关**来复现"用户改了属性"：
///
/// * 环境变量 `WE_PROP_<属性名>=<值>`（`true/false/1/0/<数字>/<字符串>`），
///
/// * 或渲染器 CLI 的 `--prop <属性名>=<值>`（见 `we-scene-render` 的 main.rs）。
///
/// 取不到覆盖 ⇒ 用场景里写的**默认值**（`Visibility::resolve`）。
pub mod userprop {
    use serde_json::Value;
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    /// `WE_PROP_` 之后就是属性名（**区分大小写**，和 scene.json 里的 `"user"` 一致）。
    const ENV_PREFIX: &str = "WE_PROP_";

    /// 把一个字符串解析成属性值：`true/false/on/off/yes/no` ⇒ 布尔；整数 ⇒ 整数；
    /// 其它数字串 ⇒ 浮点；都不是 ⇒ 字符串。CLI `--prop` 与环境变量共用它。
    pub fn parse_value(s: &str) -> Value {
        let t = s.trim();
        match t.to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" => Value::Bool(true),
            "false" | "off" | "no" => Value::Bool(false),
            _ => {
                if let Ok(i) = t.parse::<i64>() {
                    Value::from(i)
                } else if let Ok(f) = t.parse::<f64>() {
                    Value::from(f)
                } else {
                    Value::String(t.to_string())
                }
            }
        }
    }

    /// 惰性建表：第一次访问时把 `WE_PROP_*` 读进来。`set` 会先触发它再写 ⇒ 不会被环境变量盖掉。
    fn table() -> &'static Mutex<HashMap<String, Value>> {
        static T: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();
        T.get_or_init(|| {
            let mut m = HashMap::new();
            for (k, v) in std::env::vars() {
                if let Some(name) = k.strip_prefix(ENV_PREFIX) {
                    if !name.is_empty() {
                        m.insert(name.to_string(), parse_value(&v));
                    }
                }
            }
            Mutex::new(m)
        })
    }

    /// 覆盖一个用户属性的值（CLI `--prop`、控制面、单测都用它）。
    pub fn set(name: &str, value: Value) {
        table().lock().unwrap().insert(name.to_string(), value);
    }

    /// 覆盖成布尔（最常用：显隐）。
    pub fn set_bool(name: &str, value: bool) {
        set(name, Value::Bool(value));
    }

    /// 按名字取当前值（有覆盖 ⇒ Some(覆盖值)；没有 ⇒ None）。`table` 会惰性读环境变量。
    pub fn get(name: &str) -> Option<Value> {
        table().lock().unwrap().get(name).cloned()
    }

    /// 清空整张覆盖表。
///
    /// 用途：切壁纸时把上一条的属性覆盖清掉，避免"名字撞上"就串到新壁纸
    /// （属性名是作者随手起的，`newproperty` / `theme` 这类跨壁纸重名很常见）。
    /// 会先触发惰性装载再清，所以 `WE_PROP_*` 环境变量也不会"清完又冒出来"。
    pub fn clear() {
        table().lock().unwrap().clear();
    }

    /// 当前生效的覆盖条目数（诊断用）。
    pub fn len() -> usize {
        table().lock().unwrap().len()
    }

    /// 当前所有覆盖（诊断/日志用）。
    pub fn all() -> Vec<(String, Value)> {
        let mut v: Vec<(String, Value)> =
            table().lock().unwrap().iter().map(|(k, x)| (k.clone(), x.clone())).collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }
}

/// OWE `SceneJsonScalarEquals`：属性值可能是 `true`/`"1"`/`1`，要能互相判等
/// （`{"user":{"name":"theme","condition":"1"}}` 这种"按主题切换"的绑定就靠它）。
fn scalar_equals(a: &Value, b: &Value) -> bool {
    if a == b {
        return true;
    }
    let as_str = |v: &Value| -> Option<String> {
        match v {
            Value::String(s) => Some(s.clone()),
            Value::Bool(b) => Some(if *b { "true".into() } else { "false".into() }),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    };
    let as_f64 = |v: &Value| -> Option<f64> {
        match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse::<f64>().ok(),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    };
    // **数值优先**：属性值可能是 `"2.0"`、条件写的是 `"2"`（`newproperty.value == 2`）——
    // 只按字符串比会 `"2.0" != "2"` ✗ ⇒ 主题条件永远不成立 ⇒ 屏上一直渲主题1
    // （用户报"改了参数没反应/文字压到白板外"的根子之一）。
    // 两边都能解析成数字 ⇒ 按数值比（`2.0 == 2` ✓）。
    if let (Some(x), Some(y)) = (as_f64(a), as_f64(b)) {
        if (x - y).abs() <= 1e-6 * x.abs().max(1.0) {
            return true;
        }
    }
    let (sa, sb) = match (as_str(a), as_str(b)) {
        (Some(x), Some(y)) => (x, y),
        _ => return false,
    };
    if sa == sb {
        return true;
    }
    // 一种可能是 JSON 数字、另一种是数字串（`"2.0"` / `2`）⇒ 已在上面的数值比较处理 ✓
    false
}

/// 一个 `visible` 字段的解析结果：**默认值** + 可选的**用户属性绑定**。
///
/// 三种写法都要吃（本机 17 条里都有）：
/// ```json
/// "visible": true
/// "visible": null
/// "visible": {"user": "xrayversion", "value": false}
/// "visible": {"user": {"name": "newproperty", "condition": "1"}, "value": true}
/// ```
#[derive(Debug, Clone, Default)]
pub struct Visibility {
    /// 场景里写的**默认值**（WE 语义：布尔直接取；对象取 `value`；取不到按 `true`）
    pub default: bool,
    /// 绑定的用户属性名（`"user": "xxx"` 或 `"user": {"name":"xxx"}`）
    pub user: Option<String>,
    /// `"user": {"condition": …}` 里的条件（`None` ⇒ 直接把属性值当布尔用）
    pub condition: Option<Value>,
}

impl Visibility {
    /// **当前**是否可见：有该用户属性的覆盖 ⇒ 用覆盖值（对齐 OWE
    /// `ResolveSceneUserVisibilityBinding`：有 condition 就判等、否则取布尔），
    /// 没有覆盖 ⇒ 用默认值。
    pub fn resolve(&self) -> bool {
        if let Some(name) = &self.user {
            if let Some(v) = userprop::get(name) {
                return match &self.condition {
                    Some(c) => scalar_equals(&v, c),
                    None => v.as_bool().unwrap_or(self.default),
                };
            }
        }
        self.default
    }

    /// 绑定到哪个用户属性（日志/诊断用）。
    pub fn user_name(&self) -> Option<&str> {
        self.user.as_deref()
    }
}


#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct SceneDoc {
    pub camera: Camera,
    pub general: General,
    pub objects: Vec<Object>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Camera {
    /// `"x y z"`
    #[serde(deserialize_with = "bound::string")]
    pub eye: String,
    #[serde(deserialize_with = "bound::string")]
    pub center: String,
    #[serde(deserialize_with = "bound::string")]
    pub up: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Ortho {
    #[serde(deserialize_with = "bound::f32")]
    pub width: f32,
    #[serde(deserialize_with = "bound::f32")]
    pub height: f32,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct General {
    #[serde(deserialize_with = "bound::bool")]
    pub bloom: bool,
    #[serde(rename = "bloomstrength", deserialize_with = "bound::f32")]
    pub bloom_strength: f32,
    #[serde(rename = "bloomthreshold", deserialize_with = "bound::f32")]
    pub bloom_threshold: f32,
    /// `bloomtint`（`"r g b"`，WE 的泛光染色）
    #[serde(rename = "bloomtint", deserialize_with = "bound::string")]
    pub bloom_tint: String,
    /// `hdr`（WE 的 HDR 渲染管开关；本机该场景是 false）
    #[serde(deserialize_with = "bound::bool")]
    pub hdr: bool,
    #[serde(rename = "cameraparallax", deserialize_with = "bound::bool")]
    pub camera_parallax: bool,
    #[serde(rename = "cameraparallaxamount", deserialize_with = "bound::f32")]
    pub camera_parallax_amount: f32,
    #[serde(rename = "cameraparallaxmouseinfluence", deserialize_with = "bound::f32")]
    pub camera_parallax_mouse_influence: f32,
    #[serde(rename = "camerashake", deserialize_with = "bound::bool")]
    pub camera_shake: bool,
    #[serde(rename = "camerashakeamplitude", deserialize_with = "bound::f32")]
    pub camera_shake_amplitude: f32,
    #[serde(rename = "camerashakeroughness", deserialize_with = "bound::f32")]
    pub camera_shake_roughness: f32,
    #[serde(rename = "camerashakespeed", deserialize_with = "bound::f32")]
    pub camera_shake_speed: f32,
    #[serde(rename = "clearcolor", deserialize_with = "bound::string")]
    pub clear_color: String,
    /// 可能是 `null`，所以是 Option
    #[serde(rename = "clearenabled", deserialize_with = "bound::opt_bool")]
    pub clear_enabled: Option<bool>,
    #[serde(rename = "skylightcolor", deserialize_with = "bound::string")]
    pub skylight_color: String,
    #[serde(rename = "ambientcolor", deserialize_with = "bound::string")]
    pub ambient_color: String,
    #[serde(rename = "orthogonalprojection")]
    pub orthographic: Ortho,
    /// `general.lightconfig`：场景**请求的灯数量**（每种灯最多几盏参与渲染）。
///
    /// 官方出处（逐字）：
///
    /// * `assets/scenes/modeleditor/scene.json:35-37` → `"lightconfig" : { "point" : 2 }`；
///
    /// * OWE `Scene/Pkg/SceneObj/SceneDocument.cpp:62-70`（`SceneLightConfig::FromJson`）
    /// 逐字段读 `directional` / `directionalshadow` / `point` / `pointshadow` / `spot` / `spotshadow`
    /// （都是整数，缺省 0）。
    /// 本机 17 条里只有 `3571777169` 有它：`{"point": 1, "pointshadow": 1}`。
    /// ⚠ 语义交底：OWE/WE 里它进的是 `SceneShaderEnvironment`（灯数量上限），
    /// **不改变** `g_LightsPosition[4]` 的打包口径（那永远是 4 槽、按 `objects[]` 次序填）。
    /// 这里解析出来只为"灯对象语义完整"，渲染侧当前**不**用它裁剪灯数。
    #[serde(rename = "lightconfig")]
    pub light_config: LightConfig,
}

/// `general.lightconfig` —— 每种灯的**数量上限**（官方字段名见 [`General::light_config`]）。
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct LightConfig {
    #[serde(deserialize_with = "bound::i32")]
    pub directional: i32,
    #[serde(deserialize_with = "bound::i32")]
    pub directionalshadow: i32,
    #[serde(deserialize_with = "bound::i32")]
    pub point: i32,
    #[serde(deserialize_with = "bound::i32")]
    pub pointshadow: i32,
    #[serde(deserialize_with = "bound::i32")]
    pub spot: i32,
    #[serde(deserialize_with = "bound::i32")]
    pub spotshadow: i32,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct EffectPass {
    /// shader 变体开关，映射成 `#define`
    /// `WE_JSON_NULLMAP=1` 时容忍 `"combos" : null`（官方
    /// `assets/effects/{reflection,blur,blend}/preview/scene.json` 就是这么写的；
    /// OWE 同义：`MaterialPass::FromJson` 对 `null` 整块跳过不报错）。
    #[serde(deserialize_with = "crate::json::de_map_or_empty")]
    pub combos: BTreeMap<String, i64>,
    /// 传给 shader 的常量（值可能是数字、字符串 `"r g b"`、数组）
    /// `WE_JSON_NULLMAP=1` 时容忍 `"constantshadervalues" : null`（官方
    /// `assets/effects/{blur,opacity,blend}/preview/scene.json`）。
    #[serde(
        rename = "constantshadervalues",
        deserialize_with = "crate::json::de_map_or_empty"
    )]
    pub constant_shader_values: BTreeMap<String, Value>,
    /// 绑定的纹理；`null` = 不绑，`_rt_*` = 渲染目标引用
    pub textures: Vec<Option<String>>,
    /// 输出到哪个渲染目标（特效文件里给）
    pub target: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Effect {
    /// 效果定义文件（相对包内路径）
    #[serde(deserialize_with = "bound::string")]
    pub file: String,
    pub passes: Vec<EffectPass>,
    #[serde(deserialize_with = "bound::string")]
    pub name: String,
    #[serde(deserialize_with = "bound::string")]
    pub username: String,
    /// "用户可绑定"的布尔：直接 `true/false`，或 `{"user": "<属性名>", "value": <默认值>}`
    /// （对齐 OWE `ReadVisibleProperty` + `VisibleUserBinding`，见 [`Visibility`]）。
    /// WE 里 `visible=false` 的特效**不参与合成**（OWE `SceneNodeLayer.cpp:108` 的
    /// `if (eff->runtime_visible) resolve_effect(...)`）。
    #[serde(deserialize_with = "bound::visibility")]
    pub visible: Option<Visibility>,
}

impl Effect {
    /// 当前是否可见（用户属性覆盖优先，否则用默认值）。
    pub fn is_visible(&self) -> bool {
        self.visible.as_ref().map(|v| v.resolve()).unwrap_or(true)
    }
}

/// 效果定义文件（`effects/xxx/effect.json`）
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct EffectFile {
    pub passes: Vec<EffectFilePass>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct EffectFilePass {
    /// shader 名（相对包内路径，`shaders/` 下）
    pub shader: String,
    pub target: Option<String>,
    #[serde(deserialize_with = "crate::json::de_map_or_empty")]
    pub combos: BTreeMap<String, i64>,
    #[serde(
        rename = "constantshadervalues",
        deserialize_with = "crate::json::de_map_or_empty"
    )]
    pub constant_shader_values: BTreeMap<String, Value>,
    pub textures: Vec<Option<String>>,
}

/// 对象的四类身份之一
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Image,
    Model,
    Particle,
    Sound,
    /// 文字对象（`font` + `text`；内容可能是静态值，也可能是 WE 的 JS 模板）
    Text,
    /// shape 对象（`"shape": "quad"`；`parent` 指定挂在哪个图层下）
    Shape,
    /// 灯对象（`"light": "lpoint"` 等）—— 见 [`Object::light`]。
///
    /// ⚠ 它不是"要画的东西"：渲染侧只拿它喂 `g_LightsPosition[4]` /
    /// `g_LightsColorRadius[4]`（OWE `UniformSource.cpp:848` `LightUniformSource::Evaluate`）。
    Light,
    Other,
}

/// 骨骼动画层（puppet 部件用）：WE 用它给 puppet **摆姿态**。
///
/// `additive:false` + `blend:1` 的层**替换**参考姿态（OWE `PuppetLayer::genFrame`
/// 的 `trans = trans*(1-blend) + frame*blend` 分支）⇒ 部件的几何落点由**动画帧**决定；
/// `additive:true` 的层只是在参考姿态上叠加小幅动作（`tou` 的「摇头」）。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct AnimationLayer {
    /// 动画 id（对应 MDLA 块的 `id` 字段，实测 246/453/465/368/239…）
    pub animation: i64,
    /// 加法层（默认 false = 替换式）
    #[serde(deserialize_with = "bound::opt_bool")]
    pub additive: Option<bool>,
    /// **混合权重**（`blend`）。WE 允许两种写法：
///
    /// * **数字**（实测 0.38 / 0.43 / 0.55 / 0.63 / 1.0 …）；
///
    /// * **字段绑定对象** `{"script":…, "scriptproperties":{…}, "value":N}` ——
    /// WE/OWE 每帧**跑脚本**取值。依据：OWE `SceneObj/AnimationLayer.cppm:26`
    /// `AbsorbAllFieldBindings(jLayer, entry.field_bindings)` 把该层的 `blend`（连同
    /// `script`/`scriptproperties`）收成 field binding，再由
    /// `Parse/Scene/SceneScriptBinding.cpp:295`（`ScriptBindingContext::ForAnimationLayer`）
    /// 逐帧求值回写 `layer.blend`；而 `GetJsonValue(jLayer,"blend",layer.blend)`
    /// 读到的只是对象里的 `value`，那正是脚本的**输入**（`init(value)`）。
    /// 原样保留 JSON（求值口径见 [`AnimationLayer::blend_weight`] / [`AnimationLayer::blend_raw`]）。
    pub blend: Option<Value>,
    /// **播放速率**（`rate`，WE 默认 1.0）：播放位置 = `t · rate`，周期 = 片长 / rate。
    /// 依据（OWE `Puppet.cpp::prepared` + `SceneAnimationPlayback::SetRate`）。
    #[serde(deserialize_with = "bound::opt_f32")]
    pub rate: Option<f32>,
    /// 是否可见（缺省按可见）
    #[serde(default = "default_true", deserialize_with = "bound::bool")]
    pub visible: bool,
}

fn default_true() -> bool {
    true
}

impl AnimationLayer {
    pub fn is_additive(&self) -> bool {
        self.additive.unwrap_or(false)
    }
    pub fn blend_weight(&self) -> f32 {
        self.blend_weight_for(true)
    }
    /// `blend` 的**原始数字** —— 脚本对象取它的 `value`（脚本的输入），数字就取数字。
    /// `WE_BLEND_SCRIPT=0` 时渲染侧用这个（= 之前的口径，逐字节回退）。
    pub fn blend_raw(&self) -> f32 {
        self.blend_weight_for(false)
    }

    /// `blend` 的求值口径。
///
    /// * `evaluate_script == false`：恒取原始数字（= 脚本对象的 `value`）。
///
    /// * `evaluate_script == true`：
    /// 数字 ⇒ 原值；
    /// 脚本对象且带 `scriptproperties.minvalue` ⇒ **`value × minvalue`**。
    /// **依据**（纯数据，不看场景 id / 对象名）：本机 17 条壁纸里**唯一一处**脚本 blend 是
    /// `3571777169` id=27 的层 609（`Анимация 8`），其 `script` 的 `update` 是
    /// ```js
    /// const valueDelta = scriptProperties.maxvalue - scriptProperties.minvalue; // 1.43-1.87
    /// smoothValue += audioDelta * Math.min(1.0, engine.frametime * scriptProperties.smoothing);
    /// return initialValue * (smoothValue * valueDelta + scriptProperties.minvalue);
    /// ```
///
    /// 我们这条渲染路径**没有音频输入** ⇒ `smoothValue` 收敛到 0 ⇒ 输出 = `value × minvalue`
    /// = 0.38 × 1.87 = **0.7106**（原来是 0.38，即脚本的输入）。带音频时它是
    /// `value × [minvalue, maxvalue]` 之间的值 —— 静态渲染取静音端（确定值）。
    /// 非该形态的对象不受影响。
    fn blend_weight_for(&self, evaluate_script: bool) -> f32 {
        match &self.blend {
            None => 1.0,
            Some(Value::Object(o)) => {
                let raw = o.get("value").and_then(|x| x.as_f64());
                let minv = if evaluate_script {
                    o.get("scriptproperties")
                        .and_then(|sp| sp.get("minvalue"))
                        .and_then(|x| x.as_f64())
                } else {
                    None
                };
                match (raw, minv) {
                    (Some(v), Some(m)) => (v * m) as f32,
                    (Some(v), None) => v as f32,
                    (None, _) => 1.0,
                }
            }
            Some(v) => v.as_f64().map(|x| x as f32).unwrap_or(1.0),
        }
    }
    /// 播放速率（WE 默认 1.0）。`rate <= 0` 视为 1.0（避免作者写坏的 0 把层冻住）。
    pub fn rate(&self) -> f32 {
        match self.rate {
            Some(r) if r > 0.0 => r,
            _ => 1.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Object {
    pub id: i64,
    pub name: String,
    /// 贴图路径（也可能是 MDLV 骨骼模型 json —— WE 把两者都放在 `image` 里）
    pub image: Option<String>,
    /// 骨骼模型（puppet）路径
    pub model: Option<String>,
    /// 粒子定义：形状复杂，先原样保留
    pub particle: Option<Value>,
    /// 配乐（可能是数组或字符串，用 [`Object::sound_files`] 取）
    pub sound: Option<Value>,
    #[serde(deserialize_with = "bound::string")]
    pub origin: String,
    #[serde(deserialize_with = "bound::string")]
    pub scale: String,
    #[serde(deserialize_with = "bound::string")]
    pub angles: String,
    #[serde(rename = "parallaxDepth", deserialize_with = "bound::string")]
    pub parallax_depth: String,
    #[serde(rename = "colorBlendMode", deserialize_with = "bound::i32")]
    pub color_blend_mode: i32,
    /// 图层整体不透明度（WE 的 `alpha`，默认 1）—— 走 `g_Alpha`/`g_UserAlpha`，并进 `g_Color4.a`
    #[serde(default = "default_alpha", deserialize_with = "bound::f32")]
    pub alpha: f32,
    /// 图层染色（WE 的 `color`，`"r g b"`，默认白色）—— 走 `g_Color4.rgb`
    #[serde(default = "default_color", deserialize_with = "bound::string")]
    pub color: String,
    #[serde(rename = "copybackground", deserialize_with = "bound::bool")]
    pub copy_background: bool,
    /// 「纯色层」标记：WE 里**没有 `image` + `solid:true`** 的对象 = 用对象自身的
    /// `color`/`alpha` 画一个纯色块（组合层容器就是这么表达的）。
    #[serde(default, deserialize_with = "bound::bool")]
    pub solid: bool,
    #[serde(deserialize_with = "bound::string")]
    pub size: String,
    /// 对象矩形的**锚点对齐**：`origin` 描述的是对象矩形上**哪个点**落在场景坐标里。
    /// `center`（WE 默认）/ `topleft` / `topright` / `bottomleft` / `bottomright` / `left` / `right` / `top` / `bottom`。
    /// 以前整个忽略 ⇒ 非中心的层会被整体偏半个尺寸（实测 `2564205376` 的 `topright` 层
    /// 被画到画布右下角，表现为"只剩一角、别处黑"）。
    pub alignment: Option<String>,
    #[serde(deserialize_with = "bound::visibility")]
    pub visible: Option<Visibility>,
    #[serde(rename = "locktransforms", deserialize_with = "bound::opt_bool")]
    pub lock_transforms: Option<bool>,
    /// 阻断父链 parallaxDepth 传播（OWE SceneImageObjectParser.cpp:221 的 !disablepropagation；默认 false）
    #[serde(default, rename = "disablepropagation", deserialize_with = "bound::opt_bool")]
    pub disable_propagation: Option<bool>,
    pub effects: Vec<Effect>,
    pub dependencies: Vec<i64>,
    /// 骨骼动画层（puppet 部件）：`[{"animation":246,"additive":false,"blend":1.0,"visible":true}]`。
    /// **非加法层替换参考姿态** ⇒ 部件几何要用动画帧摆（见 [`AnimationLayer`]）。
    #[serde(rename = "animationlayers")]
    pub animation_layers: Vec<AnimationLayer>,
    #[serde(rename = "instanceoverride")]
    pub instance_override: Option<Value>,
    // 音频相关
    #[serde(deserialize_with = "bound::opt_f32")]
    pub volume: Option<f32>,
    #[serde(deserialize_with = "bound::opt_f32")]
    pub mintime: Option<f32>,
    #[serde(deserialize_with = "bound::opt_f32")]
    pub maxtime: Option<f32>,
    pub playbackmode: Option<String>,

    // ── 文字对象（`font` + `text`）────────────────────────────────────────
    /// 字体：`fonts/Alcubierre.otf`（优先壁纸目录、再 WE 的 assets/fonts）或 `systemfont_arial`
    pub font: Option<String>,
    /// 文字内容：`{"value": "静态"}` 或 `{"script": "<WE 的 JS 模板>"}`，也可能直接是字符串
    pub text: Option<Value>,
    /// 字号（场景单位）
    #[serde(deserialize_with = "bound::opt_f32")]
    pub pointsize: Option<f32>,
    /// 水平对齐（`left`/`center`/`right`）
    pub horizontalalign: Option<String>,
    /// 垂直对齐（`top`/`center`/`bottom`）——才解析。
    /// WE 的文字层用它把**文字块**相对对象原点摆放（OWE
    /// `SceneTextObjectParser.cpp:928-941` 的 `apply_text_anchor`）：
    /// `center` ⇒ 文字块**居中于对象矩形**；`top` ⇒ 原点落在文字块的**上缘**；
    /// `bottom` ⇒ 原点落在下缘。以前完全不解析 ⇒ 我们的字永远贴在画布/矩形的**上缘**
    /// （实测 `3704273480` id=691：我们贴在矩形上缘，WE 侧写的是 `center`）。
    /// 缺省值：OWE `align_or_default(verticalalign, alignment, "top","bottom")` ⇒ 没有就用
    /// 对象自己的 `alignment`，再没有才 `center`（本函数只存字段，语义在渲染侧实现）。
    pub verticalalign: Option<String>,
    /// 文字四周留白（场景单位）。可能是 `"32.00000 32.00000"` 这种**字符串里的两个数**
    /// ⇒ 用 `opt_f32_first` 取第一个（见 `bound::opt_f32_first` 的说明）。
    #[serde(deserialize_with = "bound::opt_f32_first")]
    pub padding: Option<f32>,
    /// 换行宽度（场景单位）。⚠ 只有 `limitwidth == true` 时才生效（OWE
    /// `SceneTextObjectParser.cpp:431`：`if (obj.limitwidth && obj.maxwidth > 0)`），
    /// 所以**单看一眼 maxwidth 不能判定会换行**。这里只存，渲染侧目前不换行。
    #[serde(deserialize_with = "bound::opt_f32")]
    pub maxwidth: Option<f32>,
    /// 是否启用 `maxwidth` 换行（`true` 才换行）
    #[serde(deserialize_with = "bound::opt_bool")]
    pub limitwidth: Option<bool>,

    // ── shape 对象（`"shape": "quad"`）：没有 image/material ──────────────
    /// 形状类型（实测只有 `quad`；其他值 WE 自己也不支持）
    pub shape: Option<String>,
    /// 父对象 id（shape 常挂在某个图层下）
    pub parent: Option<i64>,
    /// **挂接骨骼名**（`attachment`）：子对象不按普通父容器摆位，而是**挂到父 puppet 的
    /// 某根骨头上**。取值就是 MDAT 块里的骨骼名（本场景：`цепь лево` / `цепь право` / `ж2`）。
    /// 以前该字段**被反序列化直接丢掉**（渲染器与 we-scene 里 grep 零命中）⇒ 2207/1128
    /// 的链按普通子对象算，整块落到画布外。依据：OWE `SceneFinalize.cpp:358-390` +
    /// `Puppet.cpp:119-136/496-506`（见 `mdl::rig_attachments` 的注释）。
    #[serde(default)]
    pub attachment: Option<String>,

    // ── 灯对象（`"light" : "lpoint"` 等）──────────────────────────────────
    /// 灯类型字符串（官方原文：`"lpoint"` / `"point"` / `"lspot"` / `"spot"` /
    /// `"ldirectional"` / `"directional"`）。
///
    /// 出处：`assets/scenes/modeleditor/scene.json:49` `"light" : "lpoint"`；
    /// OWE `Scene/Pkg/SceneObj/LightObject.cppm:29/71`（字段 + 样例 JSON）、
    /// `LightObject.cpp:17`（`GetJsonValue(json,"light",light)`）。
    /// **灯对象就是普通 `objects[]` 元素**，没有 `image`/`model`/`particle`/`sound`。
    #[serde(default, deserialize_with = "bound::opt_string")]
    pub light: Option<String>,
    /// 灯强度（官方 `"intensity"`，OWE 默认 **1.0**）。
///
    /// 出处：`assets/scenes/modeleditor/scene.json:48` `"intensity" : 6.0`；
    /// OWE `LightObject.cppm:32` `float intensity { 1.0f }`。
    #[serde(default, deserialize_with = "bound::opt_f32")]
    pub intensity: Option<f32>,
    /// 灯半径（官方 `"radius"`，OWE 默认 **1000.0**）。
///
    /// 出处：`assets/scenes/modeleditor/scene.json:54` `"radius" : 150`；
    /// OWE `LightObject.cppm:31` `float radius { 1000.0f }`。
    /// ⚠ 有可能是**动画对象** `{"animation":…, "value":N}` ⇒ `bound::opt_f32` 取 `value`
    /// （静态端）；带动画时渲染侧另有求值（见 `AnimCurve`）。
    #[serde(default, deserialize_with = "bound::opt_f32")]
    pub radius: Option<f32>,
    /// 灯是否投射阴影（官方 `"castshadow"`，OWE 默认 false，`LightObject.cppm:48`）。
    #[serde(rename = "castshadow", default, deserialize_with = "bound::opt_bool")]
    pub cast_shadow: Option<bool>,
    /// 聚光灯外锥角（**度**，官方 `"outercone"`；OWE `LightObject.cppm:50`）。
    #[serde(rename = "outercone", default, deserialize_with = "bound::opt_f32")]
    pub outer_cone: Option<f32>,
    /// 聚光灯内锥角（**度**，官方 `"innercone"`；OWE `LightObject.cppm:51`）。
    #[serde(rename = "innercone", default, deserialize_with = "bound::opt_f32")]
    pub inner_cone: Option<f32>,
    /// 灯的衰减指数（官方 `"exponent"`，OWE 默认 1.0，`LightObject.cppm:53`）。
    #[serde(default, deserialize_with = "bound::opt_f32")]
    pub exponent: Option<f32>,
}

impl Object {
    pub fn kind(&self) -> ObjectKind {
        if self.particle.is_some() {
            return ObjectKind::Particle;
        }
        if self.model.is_some() {
            return ObjectKind::Model;
        }
        if self.sound.is_some() {
            return ObjectKind::Sound;
        }
        // 文字对象：有 `font` + `text`（WE 把文字框也塞在图层列表里）
        if self.font.is_some() && self.text.is_some() {
            return ObjectKind::Text;
        }
        // shape 对象：有 `shape`（没有 image/material，材质来自它的特效链）
        if self.shape.is_some() {
            return ObjectKind::Shape;
        }
        // 灯对象：有 `light`（没有 image/model/particle/sound）。**必须在 image 之前判**，
        // 虽然灯对象本来也没有 `image`，但"按自己的字段判定"更贴合官方 `Object.cpp:89-90`
        // （`DecodeObject<LightObject>` 是独立的一支）。
        if self.light.is_some() {
            return ObjectKind::Light;
        }
        // 实测：puppet 骨骼模型也会写在 `image` 里（值是 `models/xxx.json`），
        // 所以要看扩展名区分"模型"和"贴图"
        if let Some(img) = &self.image {
            if img.to_ascii_lowercase().ends_with(".json") {
                return ObjectKind::Model;
            }
            return ObjectKind::Image;
        }
        ObjectKind::Other
    }

    pub fn sound_files(&self) -> Vec<String> {
        match &self.sound {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// 当前是否可见（用户属性覆盖优先，否则用场景里写的**默认值**）。
    /// 见 [`Visibility`] 与 [`userprop`]。
    pub fn is_visible(&self) -> bool {
        self.visible.as_ref().map(|v| v.resolve()).unwrap_or(true)
    }

    /// 图层不透明度（`alpha` 可能是 `{user, value}` 绑定，解析时已经取过 `value`）
    pub fn alpha_f32(&self) -> f32 {
        if self.alpha.is_finite() {
            self.alpha.clamp(0.0, 1.0)
        } else {
            1.0
        }
    }

    /// 图层染色（`"r g b"`；解析不出来 ⇒ 白色 = 不染色）
    pub fn color_rgb(&self) -> [f32; 3] {
        let v = parse_floats(&self.color);
        match v.len() {
            3 => [v[0], v[1], v[2]],
            _ => [1.0, 1.0, 1.0],
        }
    }

    /// 这一层要不要走**加法混合**：WE 的 `colorBlendMode=31` 在引擎侧就是"改成 additive"
    /// （见 OWE `SceneMaterialBuilder.cpp::ApplyLayerColorBlend`），不是 shader 里那 32 个混合分支。
    pub fn wants_additive(&self) -> bool {
        self.color_blend_mode == 31
    }

    /// 音量：0..1（没有 ⇒ 1.0）
    pub fn volume_f32(&self) -> f32 {
        self.volume.unwrap_or(1.0).clamp(0.0, 1.0)
    }
}

/// WE 的"用户可绑定"字段有**两种形态**：
///
/// ```json
/// "visible": true
/// "visible": {"user": "xrayversion", "value": false}
/// "size": "4000 2736"
/// "origin": {"script": "…", "scriptproperties": […], "value": "0 0 0"}
/// ```
///
/// 本机 17 条壁纸里 `visible` 有 9 条是后者、`copybackground`/`origin`/`angles`/`scale`/`mintime`/`maxtime`
/// 也都有这种形态（`text`/`color`/`alpha`/`radius`/`config` 同样，只是解析器本来就不读它们）。
/// 照纯 `bool`/`String`/`f32` 反序列化 ⇒ **整条壁纸打不开**（实测 `invalid type: map, expected …`，
/// 17 条里 13 条挂在这上面）。OWE 的 `ReadVisibleProperty` 同样兼容两种形态，这里对齐它：
/// 对象取 `value`，取不到就用默认值。
mod bound {
    use serde::{Deserialize, Deserializer};
    use serde_json::Value;

    /// 直接值 / `{..., "value": 真值}` ⇒ 真值；取不到 ⇒ `None`。
    /// **用户属性绑定**（`{"user": "<名>", "value": <默认>}`）：**有覆盖用覆盖**，否则用 `value`
    /// 与 [`super::Visibility::resolve`] / 渲染器 `json_to_value` 同一口径（OWE
    /// `ResolveSceneUserVisibilityBinding` 的推广）。以前这里**只取 `value`、从不查覆盖** ✗ ⇒
    /// 场景里靠绑定暴露的参数（如 `3704273480` 文字层的 `"color": {"user":"text5", …}`）改了不生效。
    /// 兼容两种 `user` 形态：`"user":"名"` 与 `"user":{"name":"名","condition":…}`
    /// （有 `condition` 时仍按"判等命中才用覆盖"处理，否则维持默认值）。
    /// ⚠ 没有覆盖时**逐字节**走旧路径（`o.remove("value")`）⇒ 默认行为不变。
    fn inner(v: Value) -> Option<Value> {
        match v {
            Value::Object(mut o) => {
                if let Some(u) = o.get("user") {
                    let (name, cond) = match u {
                        Value::String(s) => (Some(s.clone()), None),
                        Value::Object(m) => (
                            m.get("name").and_then(|x| x.as_str()).map(str::to_string),
                            m.get("condition").cloned(),
                        ),
                        _ => (None, None),
                    };
                    if let Some(name) = name {
                        if let Some(cur) = super::userprop::get(&name) {
                            match &cond {
                                None => return Some(cur),
                                Some(c) => {
                                    if super::scalar_equals(&cur, c) {
                                        if let Some(val) = o.remove("value") {
                                            return Some(val);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                o.remove("value")
            }
            other => Some(other),
        }
    }

    pub fn bool<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
        let v = Option::<Value>::deserialize(d)?;
        Ok(v.and_then(inner).and_then(|x| x.as_bool()).unwrap_or(true))
    }

    pub fn opt_bool<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
        let v = Option::<Value>::deserialize(d)?;
        Ok(match v {
            None | Some(Value::Null) => None,
            Some(v) => match inner(v) {
                Some(x) => Some(x.as_bool().unwrap_or(true)),
                // 有绑定对象但没有 value ⇒ 按"可见"处理（和 OWE 的默认一致）
                None => Some(true),
            },
        })
    }

    pub fn string<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
        let v = Option::<Value>::deserialize(d)?;
        Ok(v.and_then(inner)
            .and_then(|x| x.as_str().map(str::to_string))
            .unwrap_or_default())
    }

    /// 同 [`string`]，但**区分"字段不存在"与"值是空串"** ⇒ `Option<String>`。
    /// 用于灯对象的 `"light"`（`null`/缺失 ⇒ `None`，不能当成 `Some("")`）。
    pub fn opt_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
        let v = Option::<Value>::deserialize(d)?;
        Ok(match v {
            None | Some(Value::Null) => None,
            Some(v) => match inner(v) {
                Some(Value::String(s)) => Some(s),
                Some(other) => other.as_str().map(str::to_string),
                None => None,
            },
        })
    }

    pub fn opt_f32<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f32>, D::Error> {
        let v = Option::<Value>::deserialize(d)?;
        Ok(v.and_then(inner)
            .and_then(|x| x.as_f64())
            .map(|x| x as f32))
    }

    /// 同 [`opt_f32`]，但**字符串里带多个数**时取第一个。
    /// 为什么需要它（`3704273480` 主题2 的文字对象 id=691）：
    /// WE 把**二元**标量写成 `"32.00000 32.00000"`（一个 JSON **字符串**里的两个数），
    /// `as_f64` 对字符串返回 None ⇒ 整个字段被静默读成 `None`（= 0）。
    /// 实测该场景 `padding` 就是这个形态；OWE 侧 `obj.padding` 是个标量（取第一个数，
    /// `SceneTextObjectParser.cpp:430`），所以这里对齐它：从字符串里取第一个数。
    pub fn opt_f32_first<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f32>, D::Error> {
        let v = Option::<Value>::deserialize(d)?;
        Ok(v.and_then(inner).and_then(|x| match x {
            Value::Number(n) => n.as_f64().map(|f| f as f32),
            Value::String(s) => s
                .split_whitespace()
                .next()
                .and_then(|t| t.parse::<f32>().ok()),
            _ => None,
        }))
    }

    /// 非 Option 的 f32：取不到 ⇒ 0.0（调用方各自有默认值）
    pub fn f32<'de, D: Deserializer<'de>>(d: D) -> Result<f32, D::Error> {
        Ok(opt_f32(d)?.unwrap_or(0.0))
    }

    pub fn i32<'de, D: Deserializer<'de>>(d: D) -> Result<i32, D::Error> {
        let v = Option::<Value>::deserialize(d)?;
        Ok(v.and_then(inner).and_then(|x| x.as_i64()).unwrap_or(0) as i32)
    }

    /// `visible`：`bool` / `null` / `{"user":…, "value":…}` 三种形态 ⇒ [`Visibility`]。
    /// 对齐 OWE `ReadVisibleProperty`（`value` 是默认值；`user` 是用户属性名，可能是字符串，
    /// 也可能是 `{"name":…, "condition":…}` 对象）。
    pub fn visibility<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<super::Visibility>, D::Error> {
        let v = Option::<Value>::deserialize(d)?;
        Ok(v.as_ref().and_then(parse_visibility))
    }

    fn parse_visibility(v: &Value) -> Option<super::Visibility> {
        use super::Visibility;
        match v {
            Value::Null => None,
            Value::Bool(b) => Some(Visibility { default: *b, user: None, condition: None }),
            Value::Number(n) => Some(Visibility {
                default: n.as_f64().map(|x| x != 0.0).unwrap_or(true),
                user: None,
                condition: None,
            }),
            Value::Object(o) => {
                let default = match o.get("value") {
                    Some(Value::Bool(b)) => *b,
                    Some(Value::Number(n)) => n.as_f64().map(|x| x != 0.0).unwrap_or(true),
                    _ => true, // 没有 value（或类型不认识）⇒ 按"可见"，与 OWE 默认一致
                };
                let (user, condition) = match o.get("user") {
                    Some(Value::String(s)) => (Some(s.clone()), None),
                    Some(Value::Object(u)) => {
                        let name = u.get("name").and_then(|x| x.as_str()).map(str::to_string);
                        (name, u.get("condition").cloned())
                    }
                    _ => (None, None),
                };
                Some(Visibility { default, user, condition })
            }
            _ => None,
        }
    }
}

/// 图层 `alpha` 字段缺失时的默认值（WE 语义：不写就是完全不透明）
fn default_alpha() -> f32 {
    1.0
}

/// 图层 `color` 字段缺失时的默认值（WE 语义：不写就是不染色）
fn default_color() -> String {
    "1 1 1".to_string()
}

/// 解析场景图（宽容：未知字段忽略，缺失字段用默认值，`null` 用 Option 接）
/// JSON **方言**宽容层（默认关，见 [`crate::json`]）：`WE_JSON_COMMENTS=1` 容忍字符串外注释、
/// `WE_JSON_NULLMAP=1` 容忍该给 map 的地方给 `null`。两个开关任何组合下，
/// **已经能解析**的 `scene.json` 解析结果完全一致（见 `crate::json` 模块头部的可证性质）。
pub fn parse_scene(bytes: &[u8]) -> Result<SceneDoc> {
    // 实测文件尾部可能有 \0 填充
    let trimmed: &[u8] = {
        let mut end = bytes.len();
        while end > 0 && (bytes[end - 1] == 0 || bytes[end - 1].is_ascii_whitespace()) {
            end -= 1;
        }
        &bytes[..end]
    };
    crate::json::from_slice(trimmed).map_err(|e| Error::Json(e.to_string()))
}

/// 解析效果定义文件
/// 同样过方言宽容层：官方 25 个 `assets/effects/*/preview/**/effect.json` 第 5 行就是
/// `//"preview" : "preview/project.json",`（只读扫描见 [`crate::json`] 模块注释）。
pub fn parse_effect(bytes: &[u8]) -> Result<EffectFile> {
    crate::json::from_slice(bytes).map_err(|e| Error::Json(e.to_string()))
}

/// WE 的向量都是 `"x y z"` / `"x y"` 这样的空格分隔字符串
pub fn parse_floats(s: &str) -> Vec<f32> {
    s.split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse::<f32>().ok())
        .collect()
}

pub fn parse_vec3(s: &str) -> Option<[f32; 3]> {
    let v = parse_floats(s);
    match v.len() {
        3 => Some([v[0], v[1], v[2]]),
        _ => None,
    }
}

pub fn parse_vec2(s: &str) -> Option<[f32; 2]> {
    let v = parse_floats(s);
    match v.len() {
        2 => Some([v[0], v[1]]),
        _ => None,
    }
}

/// WE 场景字段的**动画曲线**（`"origin"` / `"radius"` 这类字段的 `{"animation": …}` 形态）
/// 以及官方求值口径。
///
/// 为什么要有它：灯对象的 `origin` / `radius` 在官方资产里**就是**这种形态
/// （`assets/scenes/modeleditor/scene.json:52` / `:54`），而本机唯一一盏灯
/// （`3571777169` id=412）两者都带 `animation` ⇒ 不实现这条曲线就只能读静态 `value`。
///
/// 出处（三方一致）：
///
/// * 官方资产：`assets/scenes/modeleditor/scene.json:46/48/52/54`（灯对象的字段形态）；
///
/// * OWE `Scene/Pkg/SceneObj/FieldBinding.cpp:19-119`（逐字段解析 + 默认值）
/// 与 `FieldBinding.cppm:12-66`（`AnimKeyframe`/`AnimOptions`/`AnimCurve` 的字段与默认）；
///
/// * OWE `Scene/Scene/Scene.cpp:236-308`（`cubic` / `eval_segment` / `eval_axis`）、
/// `:969-995`（`SceneAnimationCurve::EvaluateScalar/Vec3`，含 `relative` 基值相加）、
/// `:1140-1160`（`SceneAnimationPlayback::Sample` 的 `mirror` / `loop` / 夹取）。
pub mod anim {
    use super::Value;

    /// 关键帧的切线（`front` / `back`）。`enabled` 才参与三次 Bézier 段。
    #[derive(Debug, Clone, Copy, Default, PartialEq)]
    pub struct Tangent {
        pub enabled: bool,
        pub x: f32,
        pub y: f32,
    }

    /// 一个关键帧。`frame` 是**帧号**（时间 = 帧 / fps）。
    #[derive(Debug, Clone, Copy, Default, PartialEq)]
    pub struct Keyframe {
        pub frame: i32,
        pub value: f32,
        /// `step:true` ⇒ 该段保持前一个值（阶梯），不做插值。
        pub step: bool,
        pub front: Tangent,
        pub back: Tangent,
    }

    /// `animation.options`。
    #[derive(Debug, Clone, Default)]
    pub struct Options {
        /// 每秒帧数（缺失 / ≤0 ⇒ 30）
        pub fps: f32,
        /// 标称时长（帧）
        pub length: i32,
        /// `"single"` / `"loop"` / `"repeat"` / `"mirror"` / …
        pub mode: String,
        pub wraploop: bool,
    }

    /// 一条曲线：`c0`/`c1`/`c2` 对应 vec3 的三个轴（标量字段只有 `c0`）。
    #[derive(Debug, Clone, Default)]
    pub struct Curve {
        pub c0: Vec<Keyframe>,
        pub c1: Vec<Keyframe>,
        pub c2: Vec<Keyframe>,
        pub options: Options,
        /// **只有 `origin` 会带它**（OWE `FieldBinding.cppm:63` 的注释）：
        /// true ⇒ 通道值是相对静态 `value` 的**增量**（OWE `EvaluateVec3` 的 `base + value`）。
        pub relative: bool,
    }

    fn f32_of(v: &Value) -> Option<f32> {
        match v {
            Value::Number(n) => n.as_f64().map(|x| x as f32),
            Value::String(s) => s.trim().parse::<f32>().ok(),
            _ => None,
        }
    }

    fn i32_of(v: &Value) -> Option<i32> {
        match v {
            Value::Number(n) => n.as_i64().map(|x| x as i32),
            Value::String(s) => s.trim().parse::<i32>().ok(),
            _ => None,
        }
    }

    fn bool_of(v: &Value) -> Option<bool> {
        match v {
            Value::Bool(b) => Some(*b),
            Value::Number(n) => n.as_f64().map(|x| x != 0.0),
            _ => None,
        }
    }

    fn parse_tangent(v: Option<&Value>) -> Tangent {
        let Some(Value::Object(o)) = v else {
            return Tangent::default();
        };
        Tangent {
            enabled: o.get("enabled").and_then(bool_of).unwrap_or(false),
            x: o.get("x").and_then(f32_of).unwrap_or(0.0),
            y: o.get("y").and_then(f32_of).unwrap_or(0.0),
        }
    }

    fn parse_axis(v: Option<&Value>) -> Vec<Keyframe> {
        let Some(Value::Array(a)) = v else {
            return Vec::new();
        };
        a.iter()
            .filter_map(|k| {
                let Value::Object(o) = k else { return None };
                Some(Keyframe {
                    frame: o.get("frame").and_then(i32_of).unwrap_or(0),
                    value: o.get("value").and_then(f32_of).unwrap_or(0.0),
                    step: o.get("step").and_then(bool_of).unwrap_or(false),
                    front: parse_tangent(o.get("front")),
                    back: parse_tangent(o.get("back")),
                })
            })
            .collect()
    }

    impl Curve {
        /// 从**字段值**（`{"animation": {...}, "value": …}`）里取出曲线；没有 `animation` ⇒ `None`。
        pub fn from_field(field: &Value) -> Option<Curve> {
            let Value::Object(o) = field else { return None };
            let anim = o.get("animation")?;
            Self::from_animation_json(anim)
        }

        /// 从 `animation` 对象本身解析。
        pub fn from_animation_json(anim: &Value) -> Option<Curve> {
            let Value::Object(o) = anim else { return None };
            let mut options = Options {
                fps: 30.0,
                length: 0,
                mode: String::new(),
                wraploop: false,
            };
            if let Some(Value::Object(op)) = o.get("options") {
                if let Some(f) = op.get("fps").and_then(f32_of) {
                    options.fps = f;
                }
                options.length = op.get("length").and_then(i32_of).unwrap_or(0);
                options.mode = op
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                options.wraploop = op.get("wraploop").and_then(bool_of).unwrap_or(false);
            }
            let c = Curve {
                c0: parse_axis(o.get("c0")),
                c1: parse_axis(o.get("c1")),
                c2: parse_axis(o.get("c2")),
                options,
                relative: o.get("relative").and_then(bool_of).unwrap_or(false),
            };
            if c.c0.is_empty() && c.c1.is_empty() && c.c2.is_empty() {
                None
            } else {
                Some(c)
            }
        }

        pub fn is_empty(&self) -> bool {
            self.c0.is_empty() && self.c1.is_empty() && self.c2.is_empty()
        }

        fn fps(&self) -> f32 {
            if self.options.fps > 0.0 {
                self.options.fps
            } else {
                30.0
            }
        }

        /// `SceneAnimationCurve::EndFrame`（= 所有关键帧里最大的 frame）。
        pub fn end_frame(&self) -> i32 {
            let m = |k: &Vec<Keyframe>| k.last().map(|x| x.frame).unwrap_or(0);
            m(&self.c0).max(m(&self.c1)).max(m(&self.c2)).max(0)
        }

        /// 片段总长：`max(EndFrame, options.length)`（OWE `SceneFinalize.cpp:367` 的同一口径）。
        pub fn end(&self) -> i32 {
            self.end_frame().max(self.options.length).max(0)
        }

        fn loops(&self) -> bool {
            self.options.wraploop || self.options.mode == "loop" || self.options.mode == "repeat"
        }

        fn mirrors(&self) -> bool {
            self.options.mode == "mirror"
        }

        /// 把墙钟秒数映射成**曲线帧号**（OWE `SceneAnimationPlayback::Sample`，`Scene.cpp:1140-1160`）。
        fn sample_frame(&self, time: f32) -> f32 {
            let end = self.end() as f32;
            let frame = time * self.fps();
            if end <= 0.0 {
                return frame;
            }
            if self.mirrors() {
                let period = 2.0 * end;
                let mut f = frame % period;
                if f < 0.0 {
                    f += period;
                }
                if f <= end {
                    f
                } else {
                    period - f
                }
            } else if self.loops() {
                let mut f = frame % end;
                if f < 0.0 {
                    f += end;
                }
                f
            } else {
                frame.clamp(0.0, end)
            }
        }

        fn eval_scalar_at(&self, keys: &[Keyframe], cur: f32, wraps: bool, end: f32) -> f32 {
            if keys.is_empty() {
                return 0.0;
            }
            let first = &keys[0];
            let last = &keys[keys.len() - 1];
            if wraps && cur < first.frame as f32 {
                let previous = Keyframe {
                    frame: last.frame - end as i32,
                    ..*last
                };
                if (previous.frame as f32) < first.frame as f32 {
                    return seg(previous, *first, cur);
                }
            }
            if cur <= first.frame as f32 {
                return first.value;
            }
            for i in 1..keys.len() {
                if cur <= keys[i].frame as f32 {
                    return seg(keys[i - 1], keys[i], cur);
                }
            }
            if wraps {
                let next = Keyframe {
                    frame: first.frame + end as i32,
                    ..*first
                };
                if (next.frame as f32) > last.frame as f32 {
                    return seg(*last, next, cur);
                }
            }
            last.value
        }

        /// 标量字段求值（`SceneAnimationCurve::EvaluateScalar`）。
        pub fn eval_scalar(&self, base: f32, time: f32) -> f32 {
            if self.c0.is_empty() {
                return base;
            }
            let v = self.eval_axis(&self.c0, time);
            if self.relative {
                base + v
            } else {
                v
            }
        }

        /// vec3 字段求值（`SceneAnimationCurve::EvaluateVec3`）。
        pub fn eval_vec3(&self, base: [f32; 3], time: f32) -> [f32; 3] {
            if self.is_empty() {
                return base;
            }
            let mut out = base;
            for (i, axis) in [&self.c0, &self.c1, &self.c2].into_iter().enumerate() {
                if axis.is_empty() {
                    continue;
                }
                let v = self.eval_axis(axis, time);
                out[i] = if self.relative { base[i] + v } else { v };
            }
            out
        }

        fn eval_axis(&self, keys: &[Keyframe], time: f32) -> f32 {
            let cur = self.sample_frame(time);
            let wraps = self.loops() && self.options.wraploop;
            self.eval_scalar_at(keys, cur, wraps, self.end() as f32)
        }
    }

    fn cubic(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
        let omt = 1.0 - t;
        omt * omt * omt * p0 + 3.0 * omt * omt * t * p1 + 3.0 * omt * t * t * p2 + t * t * t * p3
    }

    /// 一段的插值（OWE `Scene.cpp:253-289`）：阶梯 / 线性 / 三次 Bézier。
    fn seg(a: Keyframe, b: Keyframe, frame: f32) -> f32 {
        let dt = (b.frame - a.frame) as f32;
        if dt <= 0.0 {
            return b.value;
        }
        if frame >= b.frame as f32 {
            return b.value;
        }
        if b.step {
            return a.value;
        }
        let (p0x, p3x) = (a.frame as f32, b.frame as f32);
        let p1x = p0x + if a.front.enabled { a.front.x * dt * 0.5 } else { 0.0 };
        let p2x = p3x + if b.back.enabled { b.back.x * dt * 0.5 } else { 0.0 };
        let (p0y, p3y) = (a.value, b.value);
        let p1y = p0y + if a.front.enabled { a.front.y } else { 0.0 };
        let p2y = p3y + if b.back.enabled { b.back.y } else { 0.0 };
        // x 单调性不满足 ⇒ 退回线性（OWE 同款兜底）
        if !(p0x <= p1x && p1x <= p2x && p2x <= p3x) {
            let t = ((frame - p0x) / dt).clamp(0.0, 1.0);
            return a.value + (b.value - a.value) * t;
        }
        let (mut lo, mut hi) = (0.0f32, 1.0f32);
        for _ in 0..16 {
            let mid = (lo + hi) * 0.5;
            let x = cubic(p0x, p1x, p2x, p3x, mid);
            if (x - frame).abs() <= 0.00001 {
                lo = mid;
                hi = mid;
                break;
            }
            if x < frame {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        cubic(p0y, p1y, p2y, p3y, (lo + hi) * 0.5)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 灯对象的 `origin` 动画曲线（真实素材逐字）：`3571777169` id=412 的 `scene.json` 形态。
///
    /// 手算依据（OWE `Scene.cpp:253-289` 的 Bézier 段 + `:1140-1160` 的 mirror 采样）：
    /// t=5s、fps=30 ⇒ 第 150 帧（`mirror`、length=300 ⇒ 150 ≤ 300 不动）；
    /// c0 的两个关键帧与切线**对称** ⇒ 三次 Bézier 在 t=0.5 处 = 线性中点
    /// `(-2169.7141 + 2002.4451)/2 = -83.6345`；c1 同理 = `(57.349239 - 43.012081)/2 = 7.16858`；
    /// c2 两端都是 0 ⇒ 0；`relative:true` ⇒ **base + Δ**（base = 静态 `value`）。
    #[test]
    fn light_origin_animation_evaluates_like_owe_at_t5() {
        let field = serde_json::json!({
            "value": "1959.72729 22.72888 250.00000",
            "animation": {
                "c0": [
                    {"frame": 0,   "value": -2169.7141,
                     "front": {"enabled": true, "x": 0.50166667, "y": 0.0},
                     "back":  {"enabled": true, "x": -1.0,       "y": -0.0}},
                    {"frame": 300, "value": 2002.4451,
                     "front": {"enabled": true, "x": 1.0,          "y": 0.0},
                     "back":  {"enabled": true, "x": -0.50166667, "y": -0.0}}
                ],
                "c1": [
                    {"frame": 0,   "value": 57.349239,
                     "front": {"enabled": true, "x": 0.50166667, "y": 0.0},
                     "back":  {"enabled": true, "x": -1.0,       "y": -0.0}},
                    {"frame": 300, "value": -43.012081,
                     "front": {"enabled": true, "x": 1.0,          "y": 0.0},
                     "back":  {"enabled": true, "x": -0.50166667, "y": -0.0}}
                ],
                "c2": [
                    {"frame": 0,   "value": 0.0},
                    {"frame": 300, "value": 0.0}
                ],
                "options": {"fps": 30, "length": 300, "mode": "mirror"},
                "relative": true
            }
        });
        let c = anim::Curve::from_field(&field).expect("有 animation ⇒ 应解析出曲线");
        assert!(c.relative, "`origin` 的曲线是相对基值的");
        assert_eq!(c.end(), 300);
        let p = c.eval_vec3(parse_vec3("1959.72729 22.72888 250.00000").unwrap(), 5.0);
        assert!((p[0] - 1876.093).abs() < 0.05, "{p:?}");
        assert!((p[1] - 29.897).abs() < 0.05, "{p:?}");
        assert!((p[2] - 250.0).abs() < 1e-4, "{p:?}");

        // `radius`：关键帧帧号正好落在采样帧上 ⇒ 取该关键帧原值（`Scene.cpp:271` 的早退分支）。
        let rf = serde_json::json!({
            "value": 1218.7,
            "animation": {
                "c0": [{"frame": 0, "value": 1218.7}, {"frame": 150, "value": 974.78607}],
                "options": {"fps": 30, "length": 150, "mode": "mirror"}
            }
        });
        let rc = anim::Curve::from_field(&rf).unwrap();
        assert!(!rc.relative);
        assert!((rc.eval_scalar(1218.7, 5.0) - 974.78607).abs() < 1e-3);
        // 没有 animation ⇒ 没有曲线（渲染侧退回静态 `value`）
        assert!(anim::Curve::from_field(&serde_json::json!({"value": 1.0})).is_none());
    }

    /// 用户属性覆盖表是**进程级全局**（`userprop::table`）⇒ 改它的测试必须串行，
    /// 否则 `cargo test` 的并行执行会让它们互相拆台（一个 `clear` 把另一个的覆盖抹掉，
    /// 偶发失败看起来像渲染 bug）。凡是动这张表的测试都先 `let _g = PROP_LOCK.lock;`。
    static PROP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// **真实素材**的 scene.json 片段（来自 1689515439 的实测输出，只截取我们建模的部分）
    const REAL: &str = r#"{
      "camera": {"center": "-86.856 28.141 0.000", "eye": "-86.856 28.141 1.000", "up": "0.000 1.000 0.000"},
      "general": {
        "ambientcolor": "0.3 0.3 0.3", "bloom": false, "bloomstrength": 2,
        "bloomthreshold": 0.6499999761581421, "camerafade": true,
        "cameraparallax": false, "cameraparallaxamount": 0.5, "cameraparallaxdelay": 0.1,
        "cameraparallaxmouseinfluence": 0, "camerapreview": true, "camerashake": false,
        "camerashakeamplitude": 0.5, "camerashakeroughness": 1, "camerashakespeed": 3,
        "clearcolor": "0.7 0.7 0.7", "clearenabled": null,
        "orthogonalprojection": {"height": 2736, "width": 4000}, "skylightcolor": "0.3 0.3 0.3"
      },
      "objects": [
        {"angles": "0.000 0.000 0.000", "colorBlendMode": 0, "copybackground": true, "id": 12,
         "image": "models/201803042116091058959866946.json", "locktransforms": true, "name": "Sona",
         "origin": "2000.000 1368.000 0.000", "parallaxDepth": "1.000 1.000",
         "scale": "1.000 1.000 1.000", "size": "4000.000 2736.000", "visible": true},
        {"id": 19, "name": "Ocean bubbles", "image": null, "visible": null,
         "particle": [{"name": "bubbles"}], "origin": "0 0 0", "parallaxDepth": "1 1"},
        {"id": 30, "name": "曲", "sound": ["sounds/a.mp3"], "volume": 0.5, "mintime": 1, "maxtime": 9,
         "playbackmode": "loop", "origin": "0 0 0", "parallaxDepth": "1 1", "visible": true},
        {"id": 40, "name": "特效层", "image": "models/x.json", "origin": "0 0 0", "parallaxDepth": "1 1",
         "visible": true, "effects": [{"file": "effects/godrays/effect.json", "username": "", "visible": true,
           "passes": [
             {"constantshadervalues": {"ui_editor_properties_noise_amount": 0.9},
              "textures": [null, "util/white", "util/clouds_256"]},
             {"combos": {"CASTER": 1, "SAMPLES": 1}, "constantshadervalues": {"ui_editor_properties_color": "1 1 1"}},
             {"combos": {"KERNEL": 1}},
             {"combos": {"BLENDMODE": 9}, "textures": [null, "_rt_imageLayerComposite_19_b"]}
           ]}]}
      ]
    }"#;

    #[test]
    fn parses_real_scene_shape() {
        let doc = parse_scene(REAL.as_bytes()).expect("真实结构应能解析");
        assert_eq!(doc.objects.len(), 4);

        // 相机与投影
        assert_eq!(parse_vec3(&doc.camera.eye), Some([-86.856, 28.141, 1.0]));
        assert_eq!(doc.general.orthographic.width, 4000.0);
        assert_eq!(doc.general.orthographic.height, 2736.0);
        assert!(!doc.general.bloom);
        assert!((doc.general.bloom_strength - 2.0).abs() < 1e-9, "整数 2 应能进 f32");
        assert_eq!(doc.general.clear_enabled, None, "null 要用 Option 接住");
    }

    #[test]
    fn classifies_objects() {
        let doc = parse_scene(REAL.as_bytes()).unwrap();
        assert_eq!(
            doc.objects[0].kind(),
            ObjectKind::Model,
            "image 指向 .json → 是 puppet 模型（实测 WE 就是这么放的）"
        );
        assert_eq!(doc.objects[1].kind(), ObjectKind::Particle, "有 particle → 粒子");
        assert_eq!(doc.objects[2].kind(), ObjectKind::Sound, "有 sound → 音频");
        assert_eq!(doc.objects[2].sound_files(), vec!["sounds/a.mp3"]);
        assert!(doc.objects[1].is_visible(), "visible 缺失/null 都当可见");
        assert_eq!(parse_vec2(&doc.objects[0].parallax_depth), Some([1.0, 1.0]));
    }

    #[test]
    fn parses_declarative_effect_chain() {
        let doc = parse_scene(REAL.as_bytes()).unwrap();
        let fx = &doc.objects[3].effects[0];
        assert_eq!(fx.file, "effects/godrays/effect.json");
        assert_eq!(fx.passes.len(), 4, "4 趟 pass");
        assert_eq!(fx.passes[1].combos.get("CASTER"), Some(&1));
        assert_eq!(fx.passes[0].textures.len(), 3);
        assert_eq!(fx.passes[0].textures[2].as_deref(), Some("util/clouds_256"));
        assert_eq!(fx.passes[0].textures[0], None, "null = 不绑纹理");
        let rt = fx.passes[3].textures[1].as_deref().unwrap();
        assert!(rt.starts_with("_rt_"), "渲染目标引用: {rt}");
        let col = fx.passes[1]
            .constant_shader_values
            .get("ui_editor_properties_color")
            .unwrap();
        assert_eq!(col.as_str(), Some("1 1 1"));
    }

    #[test]
    fn effect_file_parses_shader_refs() {
        let txt = r#"{"passes":[{"shader":"effects/godrays_downsample2","target":"_rt_a"},{"shader":"effects/x"}]}"#;
        let f = parse_effect(txt.as_bytes()).expect("效果文件应能解析");
        assert_eq!(f.passes.len(), 2);
        assert_eq!(f.passes[0].shader, "effects/godrays_downsample2");
        assert_eq!(f.passes[0].target.as_deref(), Some("_rt_a"));
    }

    #[test]
    fn tolerates_trailing_nul_padding() {
        let mut bytes = REAL.as_bytes().to_vec();
        bytes.extend_from_slice(&[0u8; 12]);
        assert!(parse_scene(&bytes).is_ok(), "尾部 \\0 填充不该导致失败");
    }

    #[test]
    fn parse_floats_is_tolerant() {
        assert_eq!(parse_floats("1 2 3"), vec![1.0, 2.0, 3.0]);
        assert_eq!(parse_floats("  1.5,2.5 "), vec![1.5, 2.5]);
        assert_eq!(parse_floats(""), Vec::<f32>::new());
        assert_eq!(parse_vec3("1 2"), None, "两个分量不是 vec3");
    }

    /// `visible` / `volume` 的两种形态都要吃（本机 9/17 条壁纸的 `visible` 是绑定对象，
    /// 以前照纯 bool 解析 ⇒ 整条壁纸 `invalid type: map, expected a boolean` 打不开）
    #[test]
    fn visible_accepts_bool_and_binding_object() {
        let doc = parse_scene(
            br#"{"objects":[
                 {"id":1,"name":"a","visible":false},
                 {"id":2,"name":"b","visible":true},
                 {"id":3,"name":"c","visible":{"user":"k","value":false}},
                 {"id":4,"name":"d","visible":{"user":"k","value":true}},
                 {"id":5,"name":"e"},
                 {"id":6,"name":"f","visible":null},
                 {"id":7,"name":"g","visible":{"user":"k"}}
               ]}"#,
        )
        .expect("绑定对象的 visible 不该让整条壁纸解析失败");
        let vis: Vec<bool> = doc.objects.iter().map(|o| o.is_visible()).collect();
        assert_eq!(
            vis,
            vec![false, true, false, true, true, true, true],
            "bool 照用；对象取 value；缺 value ⇒ 可见"
        );
    }

    #[test]
    fn volume_accepts_number_and_binding_object() {
        let doc = parse_scene(
            br#"{"objects":[
                 {"id":1,"name":"a","volume":0.25},
                 {"id":2,"name":"b","volume":{"user":"k","value":0.5}},
                 {"id":3,"name":"c","volume":null},
                 {"id":4,"name":"d","volume":{"user":"k"},"playbackmode":"random"}
               ]}"#,
        )
        .unwrap();
        let vols: Vec<f32> = doc.objects.iter().map(|o| o.volume_f32()).collect();
        assert_eq!(vols, vec![0.25, 0.5, 1.0, 1.0], "缺/取不到都当 1.0");
        assert_eq!(doc.objects[3].playbackmode.as_deref(), Some("random"));
    }

    /// 字符串/数值字段同样有两种形态 —— 本机 13/17 条壁纸就挂在这上面
    #[test]
    fn string_and_number_fields_accept_binding_objects() {
        let doc = parse_scene(
            br#"{"objects":[{
                 "id":1,"name":"a",
                 "origin":{"script":"s","scriptproperties":[],"value":"1 2 3"},
                 "size":{"user":"k","value":"4000 2736"},
                 "angles":{"user":"k"},
                 "scale":"1.5 1.5 1",
                 "copybackground":{"user":"k","value":false},
                 "mintime":{"user":"k","value":2.5},
                 "maxtime":{"user":"k"}
               }]}"#,
        )
        .expect("绑定对象不该让整条壁纸解析失败");
        let o = &doc.objects[0];
        assert_eq!(o.origin, "1 2 3");
        assert_eq!(o.size, "4000 2736");
        assert_eq!(o.angles, "", "有对象但没 value ⇒ 空串（调用方各自有默认）");
        assert_eq!(o.scale, "1.5 1.5 1");
        assert!(!o.copy_background);
        assert_eq!(o.mintime, Some(2.5));
        assert_eq!(o.maxtime, None);
    }

    /// 用户属性覆盖：`visible` 的**默认值**先生效；用 `userprop::set`（= 复现"用户在 WE 界面
    /// 里改了壁纸参数"）改属性值 ⇒ 绑定它的**对象/特效**跟着显隐
    /// （对齐 OWE `ResolveSceneUserVisibilityBinding`：无 condition 取布尔、有 condition 判等）。
    #[test]
    fn userprop_override_drives_object_and_effect_visibility() {
        let _g = PROP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let doc = parse_scene(
            br#"{"objects":[
                 {"id":1,"name":"stocking","visible":{"user":"smoke_prop","value":false},
                  "effects":[{"file":"effects/x/effect.json","visible":{"user":"smoke_prop","value":false}}]},
                 {"id":2,"name":"theme","visible":{"user":{"name":"smoke_theme","condition":"1"},"value":true}}
               ]}"#,
        )
        .expect("绑定对象不该让整条壁纸解析失败");
        // 默认值先生效（= 没给覆盖时的行为）
        assert!(!doc.objects[0].is_visible(), "绑定默认 false ⇒ 不可见");
        assert!(!doc.objects[0].effects[0].is_visible(), "特效绑定默认 false ⇒ 不可见");
        assert!(doc.objects[1].is_visible());
        // 模拟用户把参数打开
        userprop::set_bool("smoke_prop", true);
        assert!(doc.objects[0].is_visible(), "覆盖后对象应可见");
        assert!(doc.objects[0].effects[0].is_visible(), "覆盖后特效应可见");
        // 再关掉
        userprop::set_bool("smoke_prop", false);
        assert!(!doc.objects[0].is_visible());
        assert!(!doc.objects[0].effects[0].is_visible());
        // condition 形式：属性值 != condition ⇒ 不可见；`"1"` 与 `1` 要判等（OWE 的标量判等）
        userprop::set("smoke_theme", Value::from(2));
        assert!(!doc.objects[1].is_visible(), "主题=2 ≠ condition 1 ⇒ 不可见");
        userprop::set("smoke_theme", Value::from("1"));
        assert!(doc.objects[1].is_visible(), "字符串 \"1\" 与数字 1 应判等");
    }

    /// combo 属性的**判等**：`project.json` 里 combo 的值是**字符串**（`"1"`/`"2"`），
    /// 而 `--prop` / `WE_PROP_` 走 `parse_value("2")` ⇒ 一个**数字** 2。
    /// 两边必须判等，否则“主题=2”永远切不过去（3704273480 这类 combo 绑定就靠这一条）。
    #[test]
    fn scalar_equals_covers_the_combo_string_vs_number_case() {
        assert!(scalar_equals(&Value::from(2), &Value::from("2")), "数字 2 vs 字符串 \"2\"");
        assert!(scalar_equals(&Value::from("2"), &Value::from(2)), "反向也必须成立");
        assert!(scalar_equals(&Value::from("2"), &Value::from("2")));
        // 协议按字符串传值，界面的 combo 可能发 "1.0"，场景条件写 "1" ⇒ 必须按数值判等
        assert!(scalar_equals(&Value::from("1.0"), &Value::from("1")), "\"1.0\" vs \"1\"");
        assert!(scalar_equals(&Value::from("2.0"), &Value::from("2")), "\"2.0\" vs \"2\"");
        assert!(!scalar_equals(&Value::from("1.0"), &Value::from("2")), "\"1.0\" 不等于 \"2\"");
        assert!(!scalar_equals(&Value::from(2), &Value::from("1")), "不同值不许相等");
        assert!(!scalar_equals(&Value::from(true), &Value::from("0")), "true 只等于 \"1\"");
        assert!(scalar_equals(&Value::from(true), &Value::from("1")), "true == \"1\"（OWE 的布尔/字符串规则）");
    }

    /// `clear`：整表清空（切壁纸时防属性串味）。与上面那条共用进程级表 ⇒ 同一把锁。
    #[test]
    fn userprop_clear_empties_the_table() {
        let _g = PROP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        userprop::clear();
        assert_eq!(userprop::len(), 0, "清空后表必须为空");
        userprop::set("smoke_prop", Value::Bool(true));
        userprop::set("smoke_theme", Value::from("1"));
        assert_eq!(userprop::len(), 2);
        userprop::clear();
        assert_eq!(userprop::len(), 0, "clear 要把整张表抹掉，而不仅是某个键");
        assert!(userprop::get("smoke_prop").is_none());
        userprop::clear(); // 空表再清一次不该出问题
    }

    /// `parse_value`（CLI `--prop` 与环境变量共用）的宽容解析。
    #[test]
    fn userprop_parse_value_is_tolerant() {
        assert_eq!(userprop::parse_value("true"), Value::Bool(true));
        assert_eq!(userprop::parse_value("OFF"), Value::Bool(false));
        assert_eq!(userprop::parse_value("2"), Value::from(2));
        assert_eq!(userprop::parse_value("0.5"), Value::from(0.5));
        assert_eq!(userprop::parse_value("dark"), Value::from("dark"));
    }

    /// 文字对象的**对齐/留白**字段（新增解析）。
    /// 判据（对着真实素材 `3704273480` 的文字对象 id=691）：
    /// · `verticalalign` 以前**根本不解析** ⇒ 我们的巨型排版字永远贴对象矩形上缘
    /// （WE 侧写的是 `center`）；这条测试把"解析得到"钉住。
    /// · `padding` 在真实数据里是 **JSON 字符串里的两个数**
    /// （`"32.00000 32.00000"`）—— `as_f64` 对字符串返回 None ⇒ 静默读成 0。
    /// 解析规则对齐 OWE `SceneTextObjectParser.cpp:430`（标量，取第一个数）。
    /// · `maxwidth` 单看一眼**不能**判定会换行：只有 `limitwidth == true` 才换行
    /// （OWE 的 `if (obj.limitwidth && obj.maxwidth > 0)`），所以两者都要解析出来。
    #[test]
    fn text_object_alignment_and_padding_fields() {
        let src = r#"{
          "objects": [
            {"id": 691, "font": "fonts/PosterCompressed-Regular.ttf", "text": {"user":"text","value":"mouse pad"},
             "pointsize": 256.0, "horizontalalign": "center", "verticalalign": "center",
             "maxwidth": 500.0, "limitwidth": false, "padding": "32.00000 32.00000",
             "origin": "-276.21411 614.00000 0.00000", "size": "1804.00000 1344.00000",
             "scale": "1.16768 1.16768 1.16768", "origin_parent_probe": 1}
          ]
        }"#;
        let doc = parse_scene(src.as_bytes()).expect("文字对象应能解析");
        let o = &doc.objects[0];
        assert_eq!(o.verticalalign.as_deref(), Some("center"), "verticalalign 必须解析出来");
        assert_eq!(o.horizontalalign.as_deref(), Some("center"));
        assert_eq!(o.pointsize, Some(256.0));
        assert_eq!(o.maxwidth, Some(500.0));
        assert_eq!(o.limitwidth, Some(false), "limitwidth=false ⇒ 即使 maxwidth>0 也不换行");
        assert_eq!(o.padding, Some(32.0), "双数串应取第一个数（OWE 口径），而不是丢成 0");
    }

    /// 缺字段时不该变：`verticalalign`/`maxwidth`/`limitwidth` 都是 Option。
    #[test]
    fn text_object_optional_fields_stay_none() {
        let src = r#"{"objects":[{"id":1,"origin":"0 0 0","size":"10 10"}]}"#;
        let doc = parse_scene(src.as_bytes()).unwrap();
        let o = &doc.objects[0];
        assert_eq!(o.verticalalign, None);
        assert_eq!(o.maxwidth, None);
        assert_eq!(o.limitwidth, None);
        assert_eq!(o.padding, None);
    }
}
