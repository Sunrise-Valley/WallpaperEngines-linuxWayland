//! 文字对象：字体解析 + WE 文本脚本求值 + 光栅化。
//!
//! WE 的场景里，文字框和图片层放在同一个 `objects` 列表里（带 `font` + `text`）。
//! `text` 有两种形态：
//!
//! * `{"value": "静态文字"}`
//!
//! * `{"script": "<WE 编辑器生成的 JS>"}` —— 入口是 `export function update(value)`
//! 返回新的字符串（时钟/日期这类都是它，本机 20 个文字对象里 15 个）。
//! 脚本用**真 JS 引擎**（boa）跑，而不是自己写模板匹配：WE 的脚本是开发者可任意改的，
//! 模板匹配迟早会在别的壁纸上翻车。我们要提供的 WE API 只有 `createScriptProperties`
//! （一个链式构造器）——`new Date` / 字符串方法都是标准 JS，引擎自带。
//! 字体来源（按优先级）：
//!
//! 1. 壁纸目录下的同名字体（有的作者会把字体散放）
//!
//! 2. WE 自带的 `<assets>/fonts/<名字>`（本机 19 个，`Alcubierre.otf` 这种）
//!
//! 3. `systemfont_*` → 映射到系统字体（Linux 上 WE 的 Windows 字体名不可用）

use std::path::{Path, PathBuf};

/// 带**场景包**的字体解析：返回字体字节。
/// 为什么需要它（`3704273480` 主题2 的巨型排版字）：WE 的
/// `scene.pkg` **会带作者自带的字体**（`fonts/*.ttf|otf`）——
/// 实测 `3704273480` 带 `fonts/PosterCompressed-Regular.ttf`（upem 1000 / xHeight 800 /
/// ascender 933 的**极限压缩体**）、`3299815325` 带 `fonts/Your Everything otf.otf`。
/// 旧的 `resolve_font` 只查 assets 目录 ⇒ 这两条被误判成「作者字体两边都没有」而
/// **静默退回 Liberation Sans**：现象是「字能显示、但不是作者那款」——`mouse pad`
/// 画出来是普通无衬线，而作者字体应该是又高又窄的压缩体（判据：字形宽高比与
/// `xHeight/upem=0.8` 完全对不上）。旧注释里「实测 3299815325 / 3704273480 两边都没有」
/// 就是这条误判留下的记录。
///
/// 顺序：包内（场景写的路径 → `fonts/<名>` → 同基名，大小写不敏感）→ 文件系统/assets → 系统退路。
/// `WE_TEXT_FONT=legacy` 跳过包内那一步（做 A/B）。
pub fn resolve_font_bytes(
    pkg: &we_scene::pkg::Pkg,
    pkg_bytes: &[u8],
    assets: &Path,
    name: &str,
) -> Option<Vec<u8>> {
    let use_pkg = std::env::var("WE_TEXT_FONT").ok().as_deref() != Some("legacy");
    let base = Path::new(name)
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase());
    if use_pkg {
        let mut cands: Vec<String> = vec![name.to_string(), format!("fonts/{name}")];
        if let Some(b) = &base {
            cands.push(format!("fonts/{b}"));
            cands.push(b.clone());
        }
        for c in &cands {
            if let Some(d) = pkg.get(pkg_bytes, c) {
                if !d.is_empty() {
                    return Some(d.to_vec());
                }
            }
        }
        if let Some(b) = &base {
            for e in &pkg.entries {
                let lower = e.name.to_lowercase();
                let is_font = lower.ends_with(".ttf") || lower.ends_with(".otf");
                if is_font && (lower == *b || lower.ends_with(&format!("/{b}"))) {
                    if let Some(d) = pkg.get(pkg_bytes, &e.name) {
                        if !d.is_empty() {
                            return Some(d.to_vec());
                        }
                    }
                }
            }
        }
    }
    resolve_font(assets, name).and_then(|p| std::fs::read(p).ok())
}

/// 字体解析：返回字体文件路径。
///
/// 实测：WE 的场景包本身**不含字体**（`fonts/X` 指向 WE 安装目录的 `assets/fonts/X`），
/// 作者自带的字体（如 `Your Everything otf.otf`）常常两边都没有 ⇒ 这时退回系统无衬线，
/// 和 WE 在缺字体时的退路一致（行为上是"字能显示、但不是作者那款"）。
pub fn resolve_font(assets: &Path, name: &str) -> Option<PathBuf> {
    // WE 的内置字体名（`systemfont_arial` / `systemfont_consolas`）
    if let Some(sys) = name.strip_prefix("systemfont_") {
        let cands: &[&str] = match sys {
            "consolas" | "couriernew" | "lucidaconsole" => &[
                "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
                "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
            ],
            _ => &[
                // arial / segoeui / tahoma / verdana / calibri 都当无衬线
                "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
                "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
                "/usr/share/fonts/truetype/freefont/FreeSans.ttf",
            ],
        };
        for c in cands {
            if Path::new(c).exists() {
                return Some(PathBuf::from(c));
            }
        }
        return None;
    }
    // 壁纸目录 → WE assets
    let mut cands: Vec<PathBuf> = Vec::new();
    cands.push(assets.join(name));
    cands.push(assets.join("fonts").join(name));
    // 有些包把字体放在 `fonts/` 下但场景里写的是裸文件名
    if let Some(fname) = Path::new(name).file_name() {
        cands.push(assets.join("fonts").join(fname));
    }
    if let Some(p) = cands.into_iter().find(|p| p.exists()) {
        return Some(p);
    }
    // 作者自带字体两边都没有（实测 3299815325 / 3704273480）⇒ 退回系统无衬线
    for c in [
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ] {
        if Path::new(c).exists() {
            return Some(PathBuf::from(c));
        }
    }
    None
}

/// WE 的运行时垫片：`createScriptProperties` —— 一个链式构造器。
/// 真实 WE 里它返回一个"属性表"（编辑器读写）；脚本用链式 `.addCheckbox({...})`
/// 声明属性并取默认值。我们只用默认值，所以实现成"记录默认值 + 链式返回自己"。
///
/// ⚠ 抽成公开函数是**必需的**：`crate::propscript`（场景属性脚本，如
/// `instanceoverride.rate` 的 `{"script": …}`）里的脚本**同样**会先调
/// `createScriptProperties` 声明编辑器属性（实测 `3299815325` 的
/// `Частинки пилу` 就是 —— 没有这份垫片时它会 `ReferenceError` ⇒ 整段脚本装载失败）。
/// 两处共用同一份实现，避免漂移。
pub fn script_properties_shim() -> &'static str {
    r#"
var __we_props = {};
// 绑定级 `scriptproperties` 覆盖（WE 语义：编辑器写在绑定对象里的值**优先**于
// 脚本 `.addSlider({value:…})` 里声明的默认值）。**默认空对象 ⇒ 行为与改动前逐位相同**；
// 只有场景属性脚本那条路（`crate::propscript`，开关 `WE_SCRIPT_AUDIO=1`）会往里填。
// （实测本机 `3299815325`/`3571777169` 的 rate 绑定带的 scriptproperties 与脚本默认
// 并不一致，例如 `minvalue 0.25` vs 脚本里的 `0.1`、`frequency 15` vs `0`。）
var __we_overrides = {};
function createScriptProperties() {
    // 每种属性类型的默认值：**没写 `value` 时不能给 null** ——
    // 实测 `3299815325` 的日期脚本用 `.addCombo({options:[{value:'1'},…]})`（不带 value），
    // WE 的默认是**第一个选项**；给 null 会让后面的 `scriptProperties.x == 1` 全不成立、
    // 脚本里据此初始化的变量变 undefined ⇒ TypeError。
    function __we_default(kind, opt) {
        if (opt && opt.value !== undefined) return opt.value;
        switch (kind) {
            case 'checkbox': return false;
            case 'text':     return '';
            case 'slider':   return 0;
            case 'combo':
                if (opt && opt.options && opt.options.length > 0) return opt.options[0].value;
                return 0;
            case 'color':    return '1 1 1';
            case 'vec2':     return '0 0';
            case 'vec3':     return '0 0 0';
            case 'direction':return 0;
            case 'file':     return '';
            default:         return 0;
        }
    }
    var builder = {
        _add: function (kind, opt) {
            if (opt && opt.name !== undefined) {
                var d = __we_default(kind, opt);
                // 编辑器写进绑定的 `scriptproperties` 覆盖脚本里的默认值（WE 语义）。
                // 默认空 ⇒ `d` 原样，与改动前逐位相同。
                if (__we_overrides[opt.name] !== undefined) d = __we_overrides[opt.name];
                __we_props[opt.name] = d;
            }
            return builder;
        },
        addCheckbox: function (o) { return builder._add('checkbox', o); },
        addText:     function (o) { return builder._add('text', o); },
        addSlider:   function (o) { return builder._add('slider', o); },
        addCombo:    function (o) { return builder._add('combo', o); },
        addColor:    function (o) { return builder._add('color', o); },
        addVec2:     function (o) { return builder._add('vec2', o); },
        addVec3:     function (o) { return builder._add('vec3', o); },
        addFile:     function (o) { return builder._add('file', o); },
        addDirection: function (o) { return builder._add('direction', o); },
        finish:      function ()  { return __we_props; }
    };
    return builder;
}
"#
}

/// 固定时钟垫片：`WE_NOW_EPOCH=<unix 秒>` ⇒ 脚本里的 `Date` 钉在该时刻。
///
/// 为什么需要：场景的时间文字每 1 秒重跑一次脚本，脚本里读真实墙钟 Date
/// 所以同一个二进制两次 --frames N 的结果会随墙钟秒变化
/// 2887537617 在 640x360/300 帧上跨进程差 1-4 px、最大 1/255，差异落在时间文字层 id=887
/// 关掉该层后 6/6 逐字节相同。钉住之后离线 A/B 才可复现。
fn clock_shim_at(secs: f64) -> String {
    let ms = secs.mul_add(1000.0, 0.5) as i64;
    let mut js = format!("(function(){{var R=Date;var F={ms};var D=function(){{var a=arguments;");
    js.push_str("if(a.length===0)return new R(F);");
    js.push_str("if(a.length===1)return new R(a[0]);");
    js.push_str("return new R(a[0],a[1],a.length>2?a[2]:1,a.length>3?a[3]:0,");
    js.push_str("a.length>4?a[4]:0,a.length>5?a[5]:0,a.length>6?a[6]:0);};");
    js.push_str("D.prototype=R.prototype;");
    js.push_str("D.now=function(){return F;};");
    js.push_str("D.parse=R.parse;D.UTC=R.UTC;Date=D;})();");
    js
}

/// Process-wide pinned wall clock (the offline CLI sets it by default).
/// `WE_NOW_EPOCH` still wins: an explicit value overrides the pin.
static PINNED_EPOCH: std::sync::OnceLock<f64> = std::sync::OnceLock::new();

/// Pin this process' wall clock at `secs` (takes effect once; afterwards
/// `clock_shim` uses it whenever `WE_NOW_EPOCH` is unset). `NaN`/`inf` is ignored.
pub fn pin_clock(secs: f64) {
    if secs.is_finite() {
        let _ = PINNED_EPOCH.set(secs);
    }
}

/// Read WE_NOW_EPOCH in unix seconds (fractional allowed); unset or unparsable => None.
pub fn clock_shim() -> Option<String> {
    let secs: f64 = match std::env::var("WE_NOW_EPOCH") {
        Ok(raw) => match raw.parse() {
            Ok(v) => v,
            Err(_) => return None,
        },
        // Unset => fall back to the in-process pin (offline renders pin it; see `pin_clock`).
        Err(_) => match PINNED_EPOCH.get() {
            Some(v) => *v,
            None => return None,
        },
    };
    if !secs.is_finite() {
        return None;
    }
    Some(clock_shim_at(secs))
}

/// 跑 WE 的文本脚本，返回要显示的字符串。
///
/// `initial` 是脚本收到并应返回的初始值（WE 里就是上一帧的文本）；
/// `time_secs` 只用于日志/调试（脚本里的 `new Date` 用真实时间，和 WE 一致）。
pub fn eval_script(script: &str, initial: &str) -> Result<String, String> {
    use boa_engine::{Context, Source};

    let mut ctx = Context::default();
    let prelude = script_properties_shim();
    ctx.eval(Source::from_bytes(prelude))
        .map_err(|e| format!("垫片执行失败：{e}"))?;
    if let Some(js) = clock_shim()
    {
        ctx.eval(Source::from_bytes(js.as_str()))
            .map_err(|e| format!("时钟垫片执行失败：{e}")) ?;
    }

    // WE 的导出语法 `export let/var/function` 不是标准脚本语法（是模块语法），
    // 直接把 `export ` 去掉再当普通脚本跑（WE 自己也这么干）。
    let body = script.replace("export let ", "var ").replace("export var ", "var ").replace("export function ", "function ");
    ctx.eval(Source::from_bytes(&body))
        .map_err(|e| format!("脚本执行失败：{e}"))?;

    // 调 `update(initial)`
    ctx.eval(Source::from_bytes(&format!(
        "var __we_out = (typeof update === 'function') ? update({initial_js}) : {initial_js};",
        initial_js = format!("{initial:?}")
    )))
    .map_err(|e| format!("update() 调用失败：{e}"))?;

    let out = ctx
        .eval(Source::from_bytes("String(__we_out)"))
        .map_err(|e| format!("取返回值失败：{e}"))?;
    Ok(out.to_string(&mut ctx).map_err(|e| e.to_string())?.to_std_string_escaped())
}

/// 从 `text` 字段取"初始值"（静态文本场景就是最终值）
pub fn static_value(text: &serde_json::Value) -> Option<String> {
    match text {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(m) => {
            // **用户属性绑定**（`{"user": "<名>", "value": <默认>}`）：有覆盖用覆盖，否则用 `value`
            // 与 `we_scene::scene` 的 `bound::inner` / `Visibility::resolve` 同一口径。
            // 以前只读 `"value"` ✗ ⇒ WE 界面里改文本框（如 `3704273480` 的 `text`）我们不跟着变。
            // ⚠ 无覆盖时**逐字节**走旧路径（`m.get("value")`）⇒ 默认行为不变。
            if let Some(u) = m.get("user") {
                let name = match u {
                    serde_json::Value::String(s) => Some(s.as_str()),
                    serde_json::Value::Object(o) => o.get("name").and_then(|x| x.as_str()),
                    _ => None,
                };
                if let Some(n) = name {
                    if let Some(v) = we_scene::scene::userprop::get(n) {
                        if let Some(s) = v.as_str() {
                            return Some(s.to_string());
                        }
                        // 数字/布尔形态也接受（WE 的 textinput 正常给字符串）
                        return Some(v.to_string().trim_matches('"').to_string());
                    }
                }
            }
            m.get("value")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        }
        _ => None,
    }
}

/// 取脚本正文（`{"script": "..."}`）
pub fn script_body(text: &serde_json::Value) -> Option<String> {
    match text {
        serde_json::Value::Object(m) => m.get("script").and_then(|v| v.as_str()).map(String::from),
        _ => None,
    }
}

/// 文字排版参数（对应 WE 的那几个字段）。
///
/// ⚠ 单位说明：`font_px` 是**画布像素**（调用方已把 `pointsize` 按 WE 的口径换算完，
/// 见 [`pointsize_to_px`]），其余尺寸都由它派生。
pub struct TextStyle<'a> {
    /// 字号（**画布像素**）
    pub font_px: f32,
    /// 四周留白（画布像素；OWE 只用它算文字 bbox/背景块，**不参与字形摆放**）
    pub padding: f32,
    /// 水平对齐 `left`/`center`/`right`
    pub halign: &'a str,
    /// 垂直对齐 `top`/`center`/`bottom`
    pub valign: &'a str,
    /// 文字颜色 `[r,g,b]`（0..1）
    pub color: [f32; 3],
    /// **白色填充底板**（WE 的"文本框背景"）：`Some(pad)` ⇒ 在字形墨迹后面铺一块纯白矩形，
    /// 四边各比墨迹外扩 `pad`（**画布像素**；调用方按"最终屏幕 2px"换算，见 `lib.rs`
    /// `build_text_layer`），并把**底板**（而不是字形墨迹）摆到该文字层的锚点上。
    /// `None` ⇒ 旧行为：只画字形、按字体度量块居中 —— **逐字节**不变（回归基线靠它）。
///
    /// 判据（用户对 Windows 版实测）："自定义文本是填充白色方块背景的：字体仅比白色边框小
    /// 两个像素，上下左右" ⇒ 底板 = 墨迹 bbox 各边 +2 屏幕像素，字必须落在底板内居中。
///
    /// `plate_follow_glyph`：`false`（默认）⇒ **底板**按 `halign`/`valign` 落到锚点、字形跟着
    /// 底板走（用户口径："底板中心落在该文字层应处的位置，而不是让字形自己去凑"）；
    /// `true` ⇒ 字形位置不动（= 旧的字形落点），底板只是套在墨迹外面 —— 给"只加底板、不改
    /// 落点"的对照用（`WE_TEXT_PLATE_ANCHOR=glyph`）。
    pub plate: Option<f32>,
    pub plate_follow_glyph: bool,
    /// **首行锚定**（`WE_TEXT_FIRSTLINE`，默认开）：块的原点按「**首行**」算，而不是「块心」。
    /// 用户对 Windows 版的判据（第二轮）：「换字符串就上移」= 位置随**行数**变 ⇒
    /// win 是**首行位置固定、字多就往下长**；我们以前按 `block_h` 居中 ⇒ 每多一行，
    /// 首行就上移 `line_h/2`（本机 `pointsize=256`、`WE_TEXT_SCALE=2` 时 ≈ **128 屏幕像素/行**，
    /// 两行字首行直接顶出画布）。
///
    /// `true` 时的锚点：
///
    /// * `top` ⇒ 块上缘 = 锚点（= 首行固定，行往下长）——与旧实现**逐像素相同**；
///
    /// * `bottom` ⇒ 块下缘 = 锚点（= 末行固定，行往上长）——与旧实现**逐像素相同**；
///
    /// * `center` ⇒ **首行**落在「单行时它的位置」（`ch - (ascender-descender)/2`），多出来的行往下长
    /// ⇒ 首行墨迹顶部/基线与行数（也即字符串长度）**无关**，单行时与旧实现逐像素相同。
    /// `false` ⇒ 旧口径（整块按 `block_h` 居中），给 A/B 用。
    pub firstline: bool,
    /// **场景排版框**（`WE_TEXT_LAYOUT`，默认开）：按官方语义排版 ——
    /// 可用区 = `obj.size − 2×padding`，`verticalalign`/`horizontalalign` **相对可用区**；
    /// 块高用 `ascent + (行数−1)·line_h`（**不含 descent**，见下）。
///
    /// 这条路和 `firstline` 互斥：打开时 `firstline` 不生效（它是旧口径的补丁）。
///
    /// ## 为什么块高不含 `descent`
    /// 本场景（`3704273480` id=691，`pointsize 256`、`obj.scale 1.16768`）在 Windows 版上的
    /// 实测：文字层原点（= 画布中心）到字形带中心的偏移 ≈ +41 屏幕px；若块高取
    /// `ascent − descent`（OS/2 `sTypoAscender − sTypoDescender` = 1000/1000 em），
    /// 同一字体的字形带中心只比画布中心低 ≈ 20 屏幕px。差的这一截正好是 `descent/2`
    /// （本字体 `descent = −0.067 em`，256pt/`scale 1.16768` 下 ≈ 21 屏幕px）
    /// ⇒ WE 的单行行框是 **ascender 到 ascender**（末行 descent 悬挂在框外），
    /// 即 `block_h = ascent + (n−1)·line_h`。这条只用字体度量，不含任何像素常数。
    pub ascent_box: bool,
    /// **屏幕像素级垂直位移**（对应环境变量 `WE_TEXT_DY`，默认 `0.0` = 逐字节不变）。
    /// 单位是**画布像素**（调用方已按 `屏幕px / s` 换算好，`s` = 屏幕像素/画布像素，
    /// 见 `lib.rs::build_text_layer` 的 `s_px`）。正数 = **向下**（画布第 0 行 = 画面顶）。
///
    /// 落法：光栅化**全部结束之后**整块画布按 `round(dy_canvas)` 整数平移（`shift_canvas`，
    /// 不重采样 ⇒ 字形边缘逐像素原样搬走，不会因插值变糊），底板（若开）跟着一起走。
    /// 用户口径：「文本位置应该向下约 5 个像素」—— 与 Windows 版屏上截图比，
    /// 整块字偏高。`0.0` 时**连一次拷贝都不做** ⇒ 默认路径与旧版逐字节相同。
    pub dy_canvas: f32,
    /// **Grow the text canvas to the ink** (`WE_TEXT_CANVAS_GROW`, default off).
///
    /// Official rule (`wallpaper64.exe` `FUN_140258ab0`): `RT = max(typeset rect, ink) + 2*pad`.
    /// When the text overflows the typeset rect, the rect (and the RT) grow with it.
    /// We used to rasterize into a canvas fixed at `obj.size`, and `outline.draw` dropped
    /// every pixel with `px >= w` -- so long strings were literally cut (scene id=691 with
    /// "WE MOUSE PAD": ink touches x=0 and x=w-1, both edges).
///
    /// With this on: measure the ink from glyph `px_bounds` (outline bounds are *not*
    /// clipped by the canvas, so the cut-away part is still measured), then grow the canvas
    /// symmetrically by `m = ceil(over + pad)` and add `m` to `padding`. Canvas and padding
    /// grow by the same `m`, so the glyph placement inside the canvas is a pure translation:
    /// every glyph pixel keeps its scene position and on-screen size, and the quad (derived
    /// from the crop) shifts/grows to match. `m` comes from the font outline at runtime, so
    /// there are no wallpaper-specific constants here.
    pub canvas_grow: bool,
}

/// WE 的 `pointsize` → 字体像素的换算系数。
/// 这条是**从参照实现抠出来的硬事实**，不是猜的：OWE
/// `Scene/Pkg/Parse/Scene/SceneTextObjectParser.cpp:36-43`
/// ```text
/// rstd::uint32_t TextPointSizeToPx(float point_size) {
/// constexpr float kPointsizeToPx = 4.0f; // ← 就是 4
/// if (!finite || point_size <= 0) return 1;
/// auto px = round(point_size * kPointsizeToPx);
/// return clamp(px, 1, 1024); // ← 上限 1024
/// }
/// ```
/// 同一份 `pointsize` 我们以前**直接当像素**用 ⇒ 字比 WE 小 4 倍
/// （实测 `3704273480` id=691：`pointsize=256`，我们墨迹 328x162 @ (932,658)，
/// 参照约 4 倍大）。`WE_TEXT_LAYOUT=legacy` 时可回 `k=1`（做 A/B）。
///
/// ⚠ OWE 还有**上限 1024**：`pointsize=256` 正好撞上（256*4=1024）。
/// 我们这里**不加上限**：那是 OWE 的实现细节（它的字形图集按 1024 分配），
/// WE 原版没有这个可见证据；加了会让 `pointsize>256` 的场景继续偏小。
pub fn pointsize_to_px(pointsize: f32) -> f32 {
    let k = std::env::var("WE_TEXT_PX_SCALE")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .unwrap_or(4.0);
    (pointsize * k).max(1.0)
}

/// Top edge of the visible line block (canvas px, y down). The draw loop in `rasterize`
/// and the `WE_TEXT_CANVAS_GROW` measurement share this single formula -- two copies of the
/// placement math would drift apart.
///
/// The three branches are `TextStyle::ascent_box` (scene typeset box, default),
/// `TextStyle::firstline` (legacy patch) and plain block centering (oldest mode).
fn block_top_px(
    style: &TextStyle,
    ascent: f32,
    descent: f32,
    line_h: f32,
    nlines: f32,
    h: f32,
    pad: f32,
) -> f32 {
    let block_h_full = (ascent - descent) + (nlines - 1.0) * line_h;
    if style.ascent_box {
        let bh = ascent + (nlines - 1.0) * line_h;
        let (box_t, box_b) = (pad, h - pad);
        match style.valign {
            "top" => box_t,
            "bottom" => box_b - bh,
            _ => box_t + (box_b - box_t - bh) * 0.5,
        }
    } else if style.firstline {
        let block_h1 = ascent - descent;
        match style.valign {
            "top" => h * 0.5,
            "bottom" => h * 0.5 - block_h_full,
            _ => h * 0.5 - block_h1 * 0.5,
        }
    } else {
        let shift = match style.valign {
            "top" => block_h_full * 0.5,
            "bottom" => -block_h_full * 0.5,
            _ => 0.0,
        };
        h * 0.5 - block_h_full * 0.5 + shift
    }
}

/// 光栅化：把一行/多行文字画进 `w × h` 的 RGBA 画布（**第 0 行 = 画面顶**）。
///
/// 摆放口径照参照实现 OWE `Scene/Pkg/Parse/Scene/../Text/Text.cpp`
/// `TextLayouter::SetText`（1177-1213 行）：
///
/// * **文字块在画布里垂直居中**（`text_top = +text_h*0.5`，`text_h = ascender − descender
/// + (行数−1)·line_height`；`baseline_y = text_top − ascender − i·line_height`），
/// 然后按 `verticalalign` 整体平移（`apply_text_anchor`）：
/// · `center` ⇒ 块心 = 对象原点（= 画布中心）
/// · `top` ⇒ 原点落在块的**上缘**（块向下展开）
/// · `bottom` ⇒ 原点落在块的**下缘**
/// 这才是"对齐"的真实语义：**原点 = 锚点**，和图片层的 `alignment` 同一套。
/// 以前我们只把第一行的基线放在 `padding + line_h − descent`（= **永远贴上缘**）。
///
/// * 水平：`center` ⇒ 每行在自己的行宽里居中；`left` ⇒ 画布左缘；`right` ⇒ 画布右缘。
///
/// * `padding` 按 OWE 只用于 bbox/背景块，**不动字形**。
/// `style.plate = Some(pad)` 时改成“**底板对齐**”（Windows 版实测行为）：
///
/// 1. 先把字形按上面的度量摆好（一行都不改），扫描出**墨迹像素包围盒**；
///
/// 2. 底板 = 墨迹 bbox 各边外扩 `pad`（画布像素，允许小数 ⇒ 边缘按覆盖率抗锯齿）；
///
/// 3. **底板**（不是墨迹）按 `halign`/`valign` 落到锚点：center ⇒ 底板中心 = 画布中心
/// （= 对象原点）、top ⇒ 底板**上缘** = 画布中心、bottom ⇒ 下缘；水平 center ⇒ 底板中心 =
/// 画布中心、left ⇒ 底板左缘 = `padding`、right ⇒ 底板右缘 = `画布宽 − padding`；
///
/// 4. 整块画布按 `round(Δ)` 整数平移（**不重采样** ⇒ 字边缘逐像素不变），再在白底下合成字形。
/// 旧行为（`plate = None`）在这四步之前就返回 ⇒ **逐字节**不变。
pub fn rasterize(
    font_data: &[u8],
    text: &str,
    style: &TextStyle,
    w: u32,
    h: u32,
) -> Result<(Vec<u8>, u32, u32), String> {
    use ab_glyph::{Font, FontVec, PxScale, ScaleFont, point};

    let font = FontVec::try_from_vec(font_data.to_vec()).map_err(|e| format!("字体解析失败：{e}"))?;
    let scale = PxScale::from(style.font_px.max(1.0));
    let sfont = font.as_scaled(scale);

    let rgb = [
        (style.color[0].clamp(0.0, 1.0) * 255.0).round() as u8,
        (style.color[1].clamp(0.0, 1.0) * 255.0).round() as u8,
        (style.color[2].clamp(0.0, 1.0) * 255.0).round() as u8,
    ];

    // Font metrics (shared by the draw loop and the ink measurement below).
    let ascent = sfont.ascent();
    let descent = sfont.descent(); // ab_glyph returns a *negative* descent
    let line_h = sfont.height();
    let nlines = text.split('\n').count().max(1) as f32;
    let pad0 = style.padding.max(0.0);

    // Grow the canvas to the ink (`style.canvas_grow`, default off). Measure the outline
    // bounds first (not clipped by the canvas), then grow sym. by m = ceil(over + pad).
    let mut w = w.max(1);
    let mut h = h.max(1);
    let mut pad_grow = 0.0f32;
    if style.canvas_grow {
        let m_top = block_top_px(style, ascent, descent, line_h, nlines, h as f32, pad0);
        let (mut bx0, mut by0) = (f32::MAX, f32::MAX);
        let (mut bx1, mut by1) = (f32::MIN, f32::MIN);
        let mut below = 0.0f32;
        for (li, line) in text.split('\n').enumerate() {
            let baseline = m_top + ascent + li as f32 * line_h;
            let mut line_w = 0.0f32;
            for c in line.chars() {
                line_w += sfont.h_advance(font.glyph_id(c));
            }
            let mut x = match style.halign {
                "center" => pad0 + (w as f32 - 2.0 * pad0 - line_w) * 0.5,
                "right" => w as f32 - pad0 - line_w,
                _ => pad0,
            };
            for c in line.chars() {
                let gid = font.glyph_id(c);
                let g = gid.with_scale_and_position(scale, point(x, baseline));
                if let Some(o) = font.outline_glyph(g) {
                    let bb = o.px_bounds();
                    if style.ascent_box {
                        below = below.max(bb.max.y - baseline);
                    }
                    bx0 = bx0.min(bb.min.x);
                    by0 = by0.min(bb.min.y);
                    bx1 = bx1.max(bb.max.x);
                    by1 = by1.max(bb.max.y);
                }
                x += sfont.h_advance(gid);
            }
        }
        if bx1 > bx0 {
            // Same two whole-canvas shifts the draw path ends with: the optical-center
            // nudge (ascent_box + center) and `WE_TEXT_DY`.
            let nudge = if style.ascent_box && style.valign != "top" && style.valign != "bottom" {
                (below * 0.5).round()
            } else {
                0.0
            };
            let dy_i = style.dy_canvas.round();
            let (by0, by1) = (by0 + nudge + dy_i, by1 + nudge + dy_i);
            let over = (-bx0)
                .max(-by0)
                .max(bx1 - w as f32)
                .max(by1 - h as f32)
                .max(0.0);
            if over > 0.5 {
                let cap = 8192u32;
                let m = ((over + pad0.min(512.0)).ceil() as u32)
                    .min(cap.saturating_sub(w) / 2)
                    .min(cap.saturating_sub(h) / 2);
                if m > 0 {
                    w += 2 * m;
                    h += 2 * m;
                    pad_grow = m as f32;
                    if std::env::var_os("WE_DBG_TEXT_GROW").is_some() {
                        eprintln!(
                            "[text-grow] ink x[{:.1},{:.1}] y[{:.1},{:.1}] canvas {}x{} => +{} px/side => {}x{} pad {:.1} -> {:.1}",
                            bx0, bx1, by0, by1, w - 2 * m, h - 2 * m, m, w, h, pad0, pad0 + pad_grow
                        );
                    }
                }
            }
        }
    }
    let w = w as usize;
    let h = h as usize;
    let pad = pad0 + pad_grow;
    let mut canvas = vec![0u8; w * h * 4];
    // Actual ink descent below the baseline (glyph overshoot of `o`/`u`): used only to put
    // the optical center of the visible band back by half a pixel step. Measured from the
    // font outline, not a constant.
    let mut ink_below_baseline = 0.0f32;
    let block_top = block_top_px(style, ascent, descent, line_h, nlines, h as f32, pad);

    for (li, line) in text.split('\n').enumerate() {
        // OWE: baseline_y = text_top - ascender - li*line_height（y 向上）⇒ y 向下时相加
        let baseline = block_top + ascent + li as f32 * line_h;
        // 行宽（用于对齐）
        let mut line_w = 0.0f32;
        for ch in line.chars() {
            line_w += sfont.h_advance(font.glyph_id(ch));
        }
        let mut x = match style.halign {
            // 可用区水平居中（`padding` 对称时与整幅居中同值；显式写出来是让语义可读）
            "center" => pad + (w as f32 - 2.0 * pad - line_w) * 0.5,
            "right" => w as f32 - pad - line_w,
            _ => pad,
        };
        for ch in line.chars() {
            let gid = font.glyph_id(ch);
            let g = gid.with_scale_and_position(scale, point(x, baseline));
            if let Some(outline) = font.outline_glyph(g) {
                let bb = outline.px_bounds();
                if style.ascent_box {
                    ink_below_baseline = ink_below_baseline.max(bb.max.y - baseline);
                }
                outline.draw(|gx, gy, cov| {
                    let px = bb.min.x as i32 + gx as i32;
                    let py = bb.min.y as i32 + gy as i32;
                    if px < 0 || py < 0 || px as usize >= w || py as usize >= h {
                        return;
                    }
                    let i = (py as usize * w + px as usize) * 4;
                    // 源覆盖 * 现有 alpha 的"最大值"合成（同一格可能被多笔画覆盖）
                    let a = (cov.clamp(0.0, 1.0) * 255.0).round() as u8;
                    if a > canvas[i + 3] {
                        canvas[i] = rgb[0];
                        canvas[i + 1] = rgb[1];
                        canvas[i + 2] = rgb[2];
                        canvas[i + 3] = a;
                    }
                });
            }
            x += sfont.h_advance(gid);
        }
    }

    // ── 视觉字形带的光学中心（场景排版框）：墨迹下沿 overshoot 的一半 ─────────────
    // 只作用于 `ascent_box` + `center`（别的对齐由边锚定，不该再挪）。
    if style.ascent_box && style.valign != "top" && style.valign != "bottom" {
        let nudge = (ink_below_baseline * 0.5).round() as i32;
        if nudge != 0 {
            shift_canvas(&mut canvas, w, h, 0, nudge);
        }
    }

    // ── 白色底板（Windows 版行为）：底板对齐 + 白底合成 ────────────────────────
    // 只有 `plate = Some(_)` 才会走到这里；`None` 时上面就是最终结果（逐字节不变）。
    if let Some(plate_pad) = style.plate {
        if let Some(ink) = ink_pixel_bbox(&canvas, w, h) {
            let pad_px = plate_pad.max(0.0);
            // 底板矩形（像素边界，float；x1/y1 为**开**区间）
            let mut bx0 = ink.0 as f32 - pad_px;
            let mut by0 = ink.1 as f32 - pad_px;
            let mut bx1 = (ink.2 + 1) as f32 + pad_px;
            let mut by1 = (ink.3 + 1) as f32 + pad_px;
            // 锚点（画布 y 向下）：与上面 `valign` 同一套语义，只是把“块”换成“底板”。
            let (cw, ch) = (w as f32, h as f32);
            let tx = match style.halign {
                "center" => cw * 0.5,
                "right" => cw - pad, // 底板右缘贴“画布宽 − padding”
                _ => pad,            // 底板左缘贴 `padding`
            };
            let ty = ch * 0.5; // top ⇒ 上缘 / bottom ⇒ 下缘 / center ⇒ 中心，都落在这个原点
            // 目标：把底板的“对齐缘/中心”落到 (tx, ty)
            let dx = match style.halign {
                "center" => (tx - (bx0 + bx1) * 0.5).round(),
                "right" => (tx - bx1).round(),
                _ => (tx - bx0).round(),
            };
            let dy = match style.valign {
                "top" => (ty - by0).round(),
                "bottom" => (ty - by1).round(),
                _ => (ty - (by0 + by1) * 0.5).round(),
            };
            // 锚点口径（默认）⇒ 连字形一起搬；对照口径 ⇒ 字形原位不动，只把底板套上去
            if !style.plate_follow_glyph && (dx != 0.0 || dy != 0.0) {
                shift_canvas(&mut canvas, w, h, dx as i32, dy as i32);
                bx0 += dx;
                bx1 += dx;
                by0 += dy;
                by1 += dy;
            }
            paint_white_plate_under(&mut canvas, w, h, (bx0, by0, bx1, by1));
        }
    }

    // ── 屏幕像素级垂直位移（`WE_TEXT_DY`，默认 0）───────────────────────────
    // 放在**所有**摆放/底板之后：整块画布（字形 + 底板）按整数画布像素搬 ⇒ 屏上看到的
    // 就是这一层整体上下移 `dy_canvas · s` 个屏幕像素。`dy_canvas == 0` 时一分钱不花。
    let dy_i = style.dy_canvas.round() as i32;
    if dy_i != 0 {
        shift_canvas(&mut canvas, w, h, 0, dy_i);
    }
    Ok((canvas, w as u32, h as u32))
}

/// 墨迹像素包围盒（**含**端点、只认 alpha>0）：`(x0, y0, x1, y1)`；一个像素都没画 ⇒ `None`。
pub fn ink_pixel_bbox(buf: &[u8], w: usize, h: usize) -> Option<(i32, i32, i32, i32)> {
    let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for y in 0..h {
        let row = y * w * 4;
        for x in 0..w {
            if buf[row + x * 4 + 3] > 0 {
                if (x as i32) < x0 {
                    x0 = x as i32;
                }
                if (x as i32) > x1 {
                    x1 = x as i32;
                }
                if (y as i32) < y0 {
                    y0 = y as i32;
                }
                if (y as i32) > y1 {
                    y1 = y as i32;
                }
            }
        }
    }
    if x1 < x0 || y1 < y0 {
        None
    } else {
        Some((x0, y0, x1, y1))
    }
}

/// 文字层 RT 的官方几何（`wallpaper64.exe` `FUN_140258ab0` + `FUN_1401ea500`）。
///
/// 铁律①：文字层的**渲染尺寸** = 排版矩形 + 2×padding（padding 先 clamp 到 ≤512，
/// 结果下限 4 px），**不是** `obj.size`。这一条同时决定 RT 分配、卡片尺寸与
/// （因 `T0 ≡ 本趟 RT`）`g_Texture0Resolution`。
///
/// 这里的两组数都是**光栅画布像素**：`rect` = 裁下来的那块在整幅画布里的位置/尺寸，
/// `canvas` = 整幅画布的尺寸。调用方用它把"图层四边形 = 整幅画布"换成
/// "图层四边形 = 裁下来那块"，于是字形在屏上的落点一分不变，只是缓冲变得和官方一样紧。
#[derive(Clone, Copy, Debug)]
pub struct InkCrop {
    /// 裁切矩形（画布像素，左上角原点）：`[x0, y0, w, h]`
    pub rect: [f32; 4],
    /// 整幅光栅画布尺寸（画布像素）
    pub canvas: [f32; 2],
}

/// 把画布裁到「**排版矩形** + 2×pad」（官方 `FUN_140258ab0` 的尺寸规则）。
///
/// 排版矩形（typesetter rect）与「墨迹 bbox」是两个不同的东西：
///
/// * WE 的排版器给的是它**排字用的那个矩形**（`[this+0x5a8]+0x90..0x9c`），
/// 文字在里面按 `horizontalalign`/`verticalalign` 摆；墨迹只是它的一部分。
///
/// * 文字**溢出**排版矩形时，排版矩形会跟着长大到墨迹的范围。
/// ⇒ 这里取 `union(排版矩形, 墨迹 bbox)`，再各边外扩 `pad`。
///
/// 本场景 `3704273480` id=691 就是一个可判定的例子：`padding=32`、
/// 排版矩形 = `obj.size` 内缩 32 ⇒ `+2×32` 正好回到 `obj.size`（1804×1344）。
/// win `3-1.jpg` 实测的「字内人物 1.284 px/源px」只在 `T0_h = 1344`
/// （= `obj.size`，1 px = 1 场景单位）时成立 ⇒ 反证"排版矩形 ≠ 墨迹紧包围盒"。
///
/// * `pad_px` = 已换算到画布像素的 padding（调用方负责先 clamp 到 ≤512 场景单位）；
///
/// * `layout` = 排版矩形（画布像素，`[x0,y0,x1,y1]`）；`None` ⇒ 只用墨迹 bbox；
///
/// * 结果宽高下限 **4 px**（官方 `FUN_1401ea500` 的 `max(4, …)`）；
///
/// * 墨迹为空且没有排版矩形 ⇒ **不裁**（返回整幅）。
///
/// 返回 `(rgba, w, h, crop)`：rgba 是裁下来的那块（行宽 = 新 w），`crop` 记着它在原画布里的位置。
pub fn crop_to_ink(
    canvas: Vec<u8>,
    w: usize,
    h: usize,
    pad_px: f32,
    layout: Option<[f32; 4]>,
) -> (Vec<u8>, u32, u32, InkCrop) {
    let full = InkCrop {
        rect: [0.0, 0.0, w as f32, h as f32],
        canvas: [w as f32, h as f32],
    };
    let ink = ink_pixel_bbox(&canvas, w, h).map(|(x0, y0, x1, y1)| {
        [x0 as f32, y0 as f32, (x1 + 1) as f32, (y1 + 1) as f32]
    });
    let rect = match (layout, ink) {
        (Some(l), Some(i)) => Some([l[0].min(i[0]), l[1].min(i[1]), l[2].max(i[2]), l[3].max(i[3])]),
        (Some(l), None) => Some(l),
        (None, Some(i)) => Some(i),
        (None, None) => None,
    };
    let Some(r) = rect else {
        return (canvas, w as u32, h as u32, full);
    };
    let pad = pad_px.max(0.0);
    // 与官方一致：先按排版矩形外扩 2×pad，再夹到画布内。
    let x0 = (r[0] - pad).floor().max(0.0);
    let y0 = (r[1] - pad).floor().max(0.0);
    let x1 = (r[2] + pad).ceil().min(w as f32);
    let y1 = (r[3] + pad).ceil().min(h as f32);
    let cw = ((x1 - x0) as u32).max(4).min(w as u32);
    let ch = ((y1 - y0) as u32).max(4).min(h as u32);
    // 下限 4 px 可能把窗口推出画布 ⇒ 再夹一次左上角。
    let cw = cw.min(w as u32);
    let ch = ch.min(h as u32);
    let cx0 = (x0 as u32).min(w as u32 - cw);
    let cy0 = (y0 as u32).min(h as u32 - ch);
    let (cw, ch) = (cw as usize, ch as usize);
    let mut out = vec![0u8; cw * ch * 4];
    for y in 0..ch {
        let src = ((cy0 as usize + y) * w + cx0 as usize) * 4;
        let dst = y * cw * 4;
        out[dst..dst + cw * 4].copy_from_slice(&canvas[src..src + cw * 4]);
    }
    (
        out,
        cw as u32,
        ch as u32,
        InkCrop {
            rect: [cx0 as f32, cy0 as f32, cw as f32, ch as f32],
            canvas: [w as f32, h as f32],
        },
    )
}

/// 把整块画布按整数行平移（公开包装：`crop_to_ink` 的调用方要按墨迹中心把内容摆正）。
pub fn shift_rows(buf: &mut [u8], w: usize, h: usize, dy: i32) {
    shift_canvas(buf, w, h, 0, dy);
}

/// 整块画布按整数像素平移（不重采样 ⇒ 字形边缘逐像素原样搬走）。
/// 单行临时缓冲 + 按方向遍历，避免 `dy` 覆盖还没读的行。
fn shift_canvas(buf: &mut [u8], w: usize, h: usize, dx: i32, dy: i32) {
    if dx == 0 && dy == 0 {
        return;
    }
    let mut row = vec![0u8; w * 4];
    let ys: Vec<usize> = if dy > 0 {
        (0..h).rev().collect()
    } else {
        (0..h).collect()
    };
    for y in ys {
        let src = y * w * 4;
        row.copy_from_slice(&buf[src..src + w * 4]);
        buf[src..src + w * 4].fill(0);
        let ty = y as i32 + dy;
        if ty < 0 || ty as usize >= h {
            continue;
        }
        let dst = ty as usize * w * 4;
        for x in 0..w {
            let tx = x as i32 + dx;
            if tx < 0 || tx as usize >= w {
                continue;
            }
            let s = x * 4;
            let d = dst + tx as usize * 4;
            buf[d..d + 4].copy_from_slice(&row[s..s + 4]);
        }
    }
}

/// 在**已有字形**下面铺一块纯白矩形（`rect` = 像素边界 float，x1/y1 开区间）。
///
/// 边缘按“像素方块 ∩ 矩形”的面积覆盖率抗锯齿 ⇒ 底板的**几何**位置是连续量：`pad` 可以取
/// 分数个画布像素（`2 屏幕像素 / (屏幕像素每画布像素)`），降采样到最终分辨率后正好是 2px。
fn paint_white_plate_under(buf: &mut [u8], w: usize, h: usize, rect: (f32, f32, f32, f32)) {
    let (bx0, by0, bx1, by1) = rect;
    if bx1 <= bx0 || by1 <= by0 {
        return;
    }
    let x_lo = bx0.floor().max(0.0) as usize;
    let x_hi = (bx1.ceil() as i64).clamp(0, w as i64) as usize;
    let y_lo = by0.floor().max(0.0) as usize;
    let y_hi = (by1.ceil() as i64).clamp(0, h as i64) as usize;
    let span = |a: f32, b: f32, lo: f32, hi: f32| (b.min(hi) - a.max(lo)).max(0.0);
    for y in y_lo..y_hi {
        let cy = span(y as f32, y as f32 + 1.0, by0, by1);
        if cy <= 0.0 {
            continue;
        }
        for x in x_lo..x_hi {
            let cx = span(x as f32, x as f32 + 1.0, bx0, bx1);
            if cx <= 0.0 {
                continue;
            }
            let cov = (cx * cy).clamp(0.0, 1.0);
            let i = (y * w + x) * 4;
            let ga = buf[i + 3] as f32 / 255.0;
            let a_out = cov + ga * (1.0 - cov);
            if a_out <= 0.0 {
                continue;
            }
            // 白底板在**下**、字形在**上**，非预乘合成
            let wg = ga / a_out; // 字形权重
            let wp = cov * (1.0 - ga) / a_out; // 白底权重（只在字形没盖住的地方露出来）
            for c in 0..3 {
                let v = 255.0 * wp + buf[i + c] as f32 * wg;
                buf[i + c] = v.round().clamp(0.0, 255.0) as u8;
            }
            buf[i + 3] = (a_out * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// 旧版光栅化（`WE_TEXT_LAYOUT=legacy` 时用）：第一行基线 = `padding + line_h − descent`
/// ⇒ **永远贴画布上缘**、`pointsize` 直接当像素。留在这里只为逐字节 A/B。
pub fn rasterize_legacy(
    font_data: &[u8],
    text: &str,
    pointsize: f32,
    w: u32,
    h: u32,
    padding: f32,
    align: &str,
    color: [f32; 3],
) -> Result<(Vec<u8>, u32, u32), String> {
    use ab_glyph::{Font, FontVec, PxScale, ScaleFont, point};

    let font = FontVec::try_from_vec(font_data.to_vec()).map_err(|e| format!("字体解析失败：{e}"))?;
    let scale = PxScale::from(pointsize.max(1.0));
    let sfont = font.as_scaled(scale);

    let w = w.max(1) as usize;
    let h = h.max(1) as usize;
    let mut canvas = vec![0u8; w * h * 4];
    let rgb = [
        (color[0].clamp(0.0, 1.0) * 255.0).round() as u8,
        (color[1].clamp(0.0, 1.0) * 255.0).round() as u8,
        (color[2].clamp(0.0, 1.0) * 255.0).round() as u8,
    ];

    let line_h = sfont.height().max(1.0);
    for (li, line) in text.split('\n').enumerate() {
        let baseline = padding + line_h * (li as f32 + 1.0) - sfont.descent();
        let mut line_w = 0.0f32;
        for ch in line.chars() {
            line_w += sfont.h_advance(font.glyph_id(ch));
        }
        let mut x = match align {
            "center" => (w as f32 - line_w) * 0.5,
            "right" => w as f32 - padding - line_w,
            _ => padding,
        };
        for ch in line.chars() {
            let gid = font.glyph_id(ch);
            let g = gid.with_scale_and_position(scale, point(x, baseline));
            if let Some(outline) = font.outline_glyph(g) {
                let bb = outline.px_bounds();
                outline.draw(|gx, gy, cov| {
                    let px = bb.min.x as i32 + gx as i32;
                    let py = bb.min.y as i32 + gy as i32;
                    if px < 0 || py < 0 || px as usize >= w || py as usize >= h {
                        return;
                    }
                    let i = (py as usize * w + px as usize) * 4;
                    let a = (cov.clamp(0.0, 1.0) * 255.0).round() as u8;
                    if a > canvas[i + 3] {
                        canvas[i] = rgb[0];
                        canvas[i + 1] = rgb[1];
                        canvas[i + 2] = rgb[2];
                        canvas[i + 3] = a;
                    }
                });
            }
            x += sfont.h_advance(gid);
        }
    }
    Ok((canvas, w as u32, h as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_value_handles_both_shapes() {
        assert_eq!(
            static_value(&serde_json::json!({"value": "鼠标垫"})).as_deref(),
            Some("鼠标垫")
        );
        assert_eq!(
            static_value(&serde_json::json!("-----------")).as_deref(),
            Some("-----------")
        );
        assert_eq!(static_value(&serde_json::json!({"script": "x"})), None);
    }

    #[test]
    fn eval_script_runs_we_clock_template() {
        // WE 编辑器生成的时钟模板（节选，语法结构与真实一致）
        let script = r#"
'use strict';
export var scriptProperties = createScriptProperties()
	.addCheckbox({ name: 'use12hFormat', label: '使用12时', value: false })
	.addText({ name: 'delimiter', label: '定界符', value: ':' })
	.finish();
export function update(value) {
	let time = new Date();
	var hours = time.getHours();
	if (scriptProperties.use12hFormat) { hours %= 12; if (hours == 0) { hours = 12; } }
	hours = ("00" + hours).slice(-2);
	let minutes = ("00" + time.getMinutes()).slice(-2);
	value = hours + scriptProperties.delimiter + minutes;
	return value;
}
"#;
        let out = eval_script(script, "").expect("脚本应当能跑");
        assert_eq!(out.len(), 5, "HH:MM 应当 5 个字符，实得 {out:?}");
        assert_eq!(&out[2..3], ":", "分隔符应当来自 scriptProperties");
        assert!(out[..2].chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn eval_script_handles_no_update_export() {
        assert_eq!(eval_script("var x = 1;", "原样").unwrap(), "原样");
    }

    #[test]
    fn combo_without_value_defaults_to_first_option() {
        // 实测 `3299815325` 的日期脚本就是这个形状：`addCombo` 不写 `value`
        let script = r#"
export var scriptProperties = createScriptProperties()
	.addCombo({ name: 'monthFormat', label: 'Month Format',
		options: [{label:'Numeric', value:'1'}, {label:'Full', value:'3'}] })
	.finish();
export function update(value) {
	var months = ['x'];
	if (scriptProperties.monthFormat == 1) { months = ['JAN','FEB']; }
	return months[1];
}
"#;
        let out = eval_script(script, "").expect("脚本应当能跑（combo 默认值 = 第一个选项）");
        assert_eq!(out, "FEB", "combo 默认值应当是第一个选项 '1'，实得 {out:?}");
    }

    #[test]
    fn rasterize_draws_something_with_alpha() {
        // 用系统字体（一定存在）画两个大字母
        let path = resolve_font(Path::new("/nonexistent"), "systemfont_arial")
            .expect("系统字体应当存在");
        let data = std::fs::read(path).unwrap();
        let style = TextStyle {
            font_px: 40.0,
            padding: 4.0,
            halign: "left",
            valign: "center",
            color: [1.0, 1.0, 1.0],
            plate: None,
            plate_follow_glyph: false,
            firstline: false,
            ascent_box: false,
            dy_canvas: 0.0,
            canvas_grow: false,
        };
        let (buf, w, h) = rasterize(&data, "AB", &style, 120, 60).unwrap();
        assert_eq!(buf.len(), (w * h * 4) as usize);
        let painted = buf.chunks(4).filter(|p| p[3] > 0).count();
        assert!(painted > 50, "应当画出像素，实得 {painted}");
    }

    #[test]
    fn rasterize_respects_right_alignment() {
        let path = resolve_font(Path::new("/nonexistent"), "systemfont_arial").unwrap();
        let data = std::fs::read(path).unwrap();
        let mk = |align: &'static str| TextStyle {
            font_px: 40.0,
            padding: 0.0,
            halign: align,
            valign: "center",
            color: [1.0, 1.0, 1.0],
            plate: None,
            plate_follow_glyph: false,
            firstline: false,
            ascent_box: false,
            dy_canvas: 0.0,
            canvas_grow: false,
        };
        let (l, w, h) = rasterize(&data, "I", &mk("left"), 200, 60).unwrap();
        let (r, _, _) = rasterize(&data, "I", &mk("right"), 200, 60).unwrap();
        let last = |buf: &Vec<u8>| {
            (0..w as usize)
                .rev()
                .find(|x| (0..h as usize).any(|y| buf[(y * w as usize + x) * 4 + 3] > 0))
                .unwrap_or(0)
        };
        assert!(last(&r) > last(&l) + 100, "右对齐应当明显更靠右");
    }

    /// 判据（`WE_TEXT_LAYOUT` 场景排版框）：块高 = `ascent + (行数−1)·line_h`
    /// （**不含 descent**），且 `verticalalign` 相对**可用区**。
    /// 用**新旧两条路互相比**来量（不依赖字体轮廓的具体数值，期望值由 `ascent/descent` 现场算）：
///
    /// * `center`：新口径的墨迹中心比旧口径（`ascent−descent` 块心）低 **`−descent/2`**
    /// 本场景 Windows 版比旧口径低的那 ≈21 屏幕px 就是这一项；
///
    /// * `top` + `padding`：新口径墨迹上沿比旧口径低 `padding − h/2`；
///
    /// * `center` 落点与 `padding` 无关（可用区对称 ⇒ 中心不动）。
    #[test]
    fn scene_box_uses_ascent_only_block_and_available_area() {
        use ab_glyph::{Font as _, FontVec, PxScale, ScaleFont};
        let path = resolve_font(Path::new("/nonexistent"), "systemfont_arial").unwrap();
        let data = std::fs::read(&path).unwrap();
        let font = FontVec::try_from_vec(data.clone()).unwrap();
        let (font_px, w, h) = (80.0f32, 600u32, 400u32);
        let desc = font.as_scaled(PxScale::from(font_px)).descent();
        let mk = |v: &'static str, pad: f32, scene: bool| TextStyle {
            font_px,
            padding: pad,
            halign: "center",
            valign: v,
            color: [1.0, 1.0, 1.0],
            plate: None,
            plate_follow_glyph: false,
            firstline: false,
            ascent_box: scene,
            dy_canvas: 0.0,
            canvas_grow: false,
        };
        let ink = |v: &'static str, pad: f32, scene: bool| {
            let (b, ww, hh) = rasterize(&data, "HXO", &mk(v, pad, scene), w, h).unwrap();
            ink_bbox(&b, ww, hh)
        };
        // center：新 − 旧 = −descent/2（desc 为负 ⇒ 新口径更低）
        let new_c = ink("center", 0.0, true);
        let old_c = ink("center", 0.0, false);
        let dc = ((new_c.1 + new_c.3) as f32 - (old_c.1 + old_c.3) as f32) * 0.5;
        // 容差 ±2：新口径还含「墨迹下沿 overshoot 的一半」这一格（≤1.5px，见 rasterize 注释）
        assert!(
            (dc - (-desc * 0.5)).abs() <= 2.0,
            "center：新口径应比旧口径低 −descent/2 = {:.1}px（+ 墨下沿半格），实得 {dc:.1}",
            -desc * 0.5
        );
        assert!(dc > 2.0, "新口径必须明显更低（实得 {dc:.1}px）");
        // top + padding：墨迹上沿 = padding + ascent − 墨上沿；旧口径块顶在画布中心
        let new_t = ink("top", 20.0, true);
        let old_t = ink("top", 0.0, false);
        let dt = new_t.1 as f32 - old_t.1 as f32;
        assert!(
            (dt - (20.0 - h as f32 * 0.5)).abs() <= 1.0,
            "top：新口径(pad=20) 应比旧口径低 padding−h/2 = {:.1}px，实得 {dt:.1}",
            20.0 - h as f32 * 0.5
        );
        // center 不随 padding 移动（可用区对称）
        let pad_c = ink("center", 40.0, true);
        assert!(
            (pad_c.1 as f32 - new_c.1 as f32).abs() <= 1.0,
            "center 的墨迹上沿不应随 padding 变（{} vs {}）",
            pad_c.1,
            new_c.1
        );
    }

    /// 判据（Windows 版观测）：`WE_TEXT_LAYOUT` 的**几何**必须由场景参数决定 ——
    /// 同一层 `size`/`padding`/`pointsize`/`scale` 不同，落点/字号按比例变；
    /// 这里把「画布尺寸翻倍」这一条钉住：字号与墨迹都正好翻倍、相对位置不变。
    #[test]
    fn scene_box_scales_with_frame_and_font_size() {
        let path = resolve_font(Path::new("/nonexistent"), "systemfont_arial").unwrap();
        let data = std::fs::read(&path).unwrap();
        let mk = |px: f32| TextStyle {
            font_px: px,
            padding: 16.0,
            halign: "center",
            valign: "center",
            color: [1.0, 1.0, 1.0],
            plate: None,
            plate_follow_glyph: false,
            firstline: false,
            ascent_box: true,
            dy_canvas: 0.0,
            canvas_grow: false,
        };
        let (a, w1, h1) = rasterize(&data, "HXO", &mk(80.0), 600, 400).unwrap();
        let (b, w2, h2) = rasterize(&data, "HXO", &mk(160.0), 1200, 800).unwrap();
        let ia = ink_bbox(&a, w1, h1);
        let ib = ink_bbox(&b, w2, h2);
        let (ha, hb) = ((ia.3 - ia.1 + 1) as f32, (ib.3 - ib.1 + 1) as f32);
        assert!(
            (hb / ha - 2.0).abs() < 0.06,
            "字号 ×2 ⇒ 墨迹高应 ×2（实得 {ha} → {hb}）"
        );
        let ca = (ia.1 + ia.3) as f32 * 0.5;
        let cb = (ib.1 + ib.3) as f32 * 0.5;
        assert!(
            (cb - 2.0 * ca).abs() < 3.0,
            "画布/字号同比例翻倍 ⇒ 墨迹中心也应翻倍（{ca} → {cb}）"
        );
    }

    /// 墨迹 bbox（行/列范围），用来量"文字块摆在画布的哪里"。
    fn ink_bbox(buf: &[u8], w: u32, h: u32) -> (usize, usize, usize, usize) {
        let (mut x0, mut y0, mut x1, mut y1) = (w as usize, h as usize, 0usize, 0usize);
        for y in 0..h as usize {
            for x in 0..w as usize {
                if buf[(y * w as usize + x) * 4 + 3] > 0 {
                    x0 = x0.min(x);
                    x1 = x1.max(x);
                    y0 = y0.min(y);
                    y1 = y1.max(y);
                }
            }
        }
        (x0, y0, x1, y1)
    }

    /// 判据（巨型排版字）：`verticalalign` 决定**文字块相对对象原点**的位置。
    /// `center` ⇒ 块心落在画布中心；`top` ⇒ 块的上缘落在画布中心；`bottom` ⇒ 下缘。
    /// 旧实现**只认 top**（第一行基线写在 padding 起）⇒ 这条测试以前会挂。
    #[test]
    fn rasterize_vertical_align_places_the_block_around_the_origin() {
        let path = resolve_font(Path::new("/nonexistent"), "systemfont_arial").unwrap();
        let data = std::fs::read(path).unwrap();
        let (w, h) = (400u32, 600u32);
        let mk = |v: &'static str| TextStyle {
            font_px: 80.0,
            padding: 0.0,
            halign: "center",
            valign: v,
            color: [1.0, 1.0, 1.0],
            plate: None,
            plate_follow_glyph: false,
            firstline: false,
            ascent_box: false,
            dy_canvas: 0.0,
            canvas_grow: false,
        };
        let centre = h as f32 * 0.5;
        let (cb, _, _) = rasterize(&data, "HXO", &mk("center"), w, h).unwrap();
        let (_, cy0, _, cy1) = ink_bbox(&cb, w, h);
        let ink_c = (cy0 + cy1) as f32 * 0.5;
        assert!(
            (ink_c - centre).abs() < h as f32 * 0.12,
            "center：墨迹中心 {ink_c:.1} 应靠近画布中心 {centre:.1}"
        );

        // top：块的**上缘**落在画布中心 ⇒ 墨迹（大写字形）整体在画布中心以下，
        // 且墨迹上缘 ≈ 画布中心（前导 ascent 空白不算墨迹）
        let (tb, _, _) = rasterize(&data, "HXO", &mk("top"), w, h).unwrap();
        let (_, ty0, _, ty1) = ink_bbox(&tb, w, h);
        let ink_t = (ty0 + ty1) as f32 * 0.5;
        assert!(
            (ty0 as f32 - centre).abs() < h as f32 * 0.06 && ink_t > centre + h as f32 * 0.02,
            "top：墨迹上缘 {ty0} 应贴着画布中心 {centre}（实得 y[{ty0},{ty1}]，中心 {ink_t:.1}）"
        );

        // bottom：块的**下缘**落在画布中心 ⇒ 墨迹整体在上半部
        let (bb, _, _) = rasterize(&data, "HXO", &mk("bottom"), w, h).unwrap();
        let (_, by0, _, by1) = ink_bbox(&bb, w, h);
        assert!(
            (by1 as f32) <= centre + h as f32 * 0.05,
            "bottom：墨迹下缘 {by1} 应不越过画布中心 {centre}"
        );
        assert!(by0 < ty0, "bottom 的墨迹应当整体高于 top 的（{by0} < {ty0}）");
    }

    /// 判据：`pointsize` → 字体像素的系数 = OWE 的 `kPointsizeToPx = 4`。
    /// 同一段文字，字号翻 4 倍 ⇒ 墨迹宽高各约 4 倍（字形等比）。
    #[test]
    fn pointsize_is_quadrupled_like_owe() {
        // 默认系数（没设 WE_TEXT_PX_SCALE 时）
        assert_eq!(pointsize_to_px(100.0), 400.0);
        assert_eq!(pointsize_to_px(0.0), 1.0, "非正数应夹到 1");
        let path = resolve_font(Path::new("/nonexistent"), "systemfont_arial").unwrap();
        let data = std::fs::read(path).unwrap();
        let mk = |px: f32| TextStyle {
            font_px: px,
            padding: 0.0,
            halign: "center",
            valign: "center",
            color: [1.0, 1.0, 1.0],
            plate: None,
            plate_follow_glyph: false,
            firstline: false,
            ascent_box: false,
            dy_canvas: 0.0,
            canvas_grow: false,
        };
        let (a, w, h) = rasterize(&data, "M", &mk(50.0), 800, 400).unwrap();
        let (b, _, _) = rasterize(&data, "M", &mk(200.0), 800, 400).unwrap();
        let (ax0, ay0, ax1, ay1) = ink_bbox(&a, w, h);
        let (bx0, by0, bx1, by1) = ink_bbox(&b, w, h);
        let (ra, rb) = ((ax1 - ax0 + 1) as f32, (bx1 - bx0 + 1) as f32);
        let (ha, hb) = ((ay1 - ay0 + 1) as f32, (by1 - by0 + 1) as f32);
        assert!(
            (rb / ra - 4.0).abs() < 0.35 && (hb / ha - 4.0).abs() < 0.35,
            "字号 ×4 ⇒ 墨迹应当约 ×4（实得 宽 {ra}→{rb} = {:.2}，高 {ha}→{hb} = {:.2}）",
            rb / ra,
            hb / ha
        );
    }

    /// 白底/字形的像素 bbox（判据用：字形 = 近纯红，底板 = 近纯白）。
    fn colour_bbox(buf: &[u8], w: u32, h: u32, want: &str) -> Option<(i32, i32, i32, i32)> {
        let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for y in 0..h as usize {
            for x in 0..w as usize {
                let i = (y * w as usize + x) * 4;
                let (r, g, b, a) = (buf[i], buf[i + 1], buf[i + 2], buf[i + 3]);
                let hit = match want {
                    "white" => a > 200 && r > 240 && g > 240 && b > 240,
                    _ => a > 128 && r > 200 && g < 60 && b < 60,
                };
                if hit {
                    x0 = x0.min(x as i32);
                    x1 = x1.max(x as i32);
                    y0 = y0.min(y as i32);
                    y1 = y1.max(y as i32);
                }
            }
        }
        if x1 < x0 {
            None
        } else {
            Some((x0, y0, x1, y1))
        }
    }

    /// 判据（Windows 版实测，）：白色底板 = 字形墨迹 bbox 各边 +`pad`，
    /// 且**底板**（不是字形）居中于画布 ⇒ 字落在底板里居中、四周各 `pad` 像素。
    #[test]
    fn plate_pads_the_ink_by_exactly_pad_and_centers_it_on_the_canvas() {
        let path = resolve_font(Path::new("/nonexistent"), "systemfont_arial").unwrap();
        let data = std::fs::read(path).unwrap();
        let (w, h) = (600u32, 400u32);
        let pad = 8.0f32;
        let style = TextStyle {
            font_px: 100.0,
            padding: 0.0,
            halign: "center",
            valign: "center",
            color: [1.0, 0.0, 0.0], // 红字 ⇒ 与白底板可分离统计
            plate: Some(pad),
            plate_follow_glyph: false,
            firstline: false,
            ascent_box: false,
            dy_canvas: 0.0,
            canvas_grow: false,
        };
        let (buf, _, _) = rasterize(&data, "HXO", &style, w, h).unwrap();
        let ink = colour_bbox(&buf, w, h, "red").expect("应当有字形");
        let plate = colour_bbox(&buf, w, h, "white").expect("应当有白色底板");
        // 四周各 pad（边缘是覆盖率抗锯齿 ⇒ 允许 1px 量化）
        let (l, r_, t, b) = (
            ink.0 - plate.0,
            plate.2 - ink.2,
            ink.1 - plate.1,
            plate.3 - ink.3,
        );
        assert!(
            (l as f32 - pad).abs() <= 1.0
                && (r_ as f32 - pad).abs() <= 1.0
                && (t as f32 - pad).abs() <= 1.0
                && (b as f32 - pad).abs() <= 1.0,
            "四周边距应当都 ≈ {pad}（实得 左{l} 右{r_} 上{t} 下{b}）"
        );
        // 底板中心 = 画布中心（±1px 取整）
        let (pcx, pcy) = (
            (plate.0 + plate.2) as f32 * 0.5,
            (plate.1 + plate.3) as f32 * 0.5,
        );
        assert!(
            (pcx - w as f32 * 0.5).abs() <= 1.0 && (pcy - h as f32 * 0.5).abs() <= 1.0,
            "底板中心应当落在画布中心（实得 ({pcx},{pcy}) vs ({},{})）",
            w as f32 * 0.5,
            h as f32 * 0.5
        );
        // 字形必须完全在底板内
        assert!(
            ink.0 >= plate.0 && ink.1 >= plate.1 && ink.2 <= plate.2 && ink.3 <= plate.3,
            "字形 {ink:?} 应当完全落在底板 {plate:?} 内"
        );
    }

    /// 关掉底板（`plate: None`）= 旧行为：一个白像素都不该多出来，字形位置也不动。
    #[test]
    fn plate_none_is_the_old_layout_with_no_white() {
        let path = resolve_font(Path::new("/nonexistent"), "systemfont_arial").unwrap();
        let data = std::fs::read(path).unwrap();
        let (w, h) = (600u32, 400u32);
        let mk = |plate: Option<f32>| TextStyle {
            font_px: 100.0,
            padding: 0.0,
            halign: "center",
            valign: "center",
            color: [1.0, 0.0, 0.0],
            plate,
            plate_follow_glyph: false,
            firstline: false,
            ascent_box: false,
            dy_canvas: 0.0,
            canvas_grow: false,
        };
        let (off, _, _) = rasterize(&data, "HXO", &mk(None), w, h).unwrap();
        assert!(
            colour_bbox(&off, w, h, "white").is_none(),
            "plate=None 时不应出现白色底板"
        );
        let (on, _, _) = rasterize(&data, "HXO", &mk(Some(8.0)), w, h).unwrap();
        // 底板开着时墨迹**尺寸**基本不变（只是整体平移；边缘与白底混色会让"近纯红"判据
        // 各收缩 ≤2px，这是抗锯齿混色而不是字形缩放）
        let i_off = colour_bbox(&off, w, h, "red").unwrap();
        let i_on = colour_bbox(&on, w, h, "red").unwrap();
        let (wo, ho) = (i_off.2 - i_off.0, i_off.3 - i_off.1);
        let (wn, hn) = (i_on.2 - i_on.0, i_on.3 - i_on.1);
        assert!(
            (wo - wn).abs() <= 2 && (ho - hn).abs() <= 2,
            "平移不能改变墨迹尺寸（{i_off:?} vs {i_on:?}）"
        );
    }

    /// 判据（第二轮，用户反驳 3）：「换字符串就上移」。
    /// `firstline: true` ⇒ **首行墨迹顶部与行数无关**（1 行 / 3 行必须相等，±1px）；
    /// `firstline: false`（旧口径）⇒ 每多一行首行上移 `line_h/2`，本测试同时把**改前的差值**打印出来。
    #[test]
    fn firstline_anchor_keeps_ink_top_independent_of_line_count() {
        let path = resolve_font(Path::new("/nonexistent"), "systemfont_arial").unwrap();
        let data = std::fs::read(path).unwrap();
        let (w, h) = (400u32, 900u32); // 画布够高：3 行也整块在画布里，量得到真值
        let mk = |fl: bool| TextStyle {
            font_px: 80.0,
            padding: 0.0,
            halign: "center",
            valign: "center",
            color: [1.0, 1.0, 1.0],
            plate: None,
            plate_follow_glyph: false,
            firstline: fl,
            ascent_box: false,
            dy_canvas: 0.0,
            canvas_grow: false,
        };
        let top = |txt: &str, fl: bool| {
            let (b, ww, hh) = rasterize(&data, txt, &mk(fl), w, h).unwrap();
            let (_, y0, _, y1) = ink_bbox(&b, ww, hh);
            (y0 as i32, y1 as i32)
        };
        // 三行都用同一组大写字母 ⇒ 首行墨迹顶部只可能因**块高**而变，与字形无关。
        let (o1, o3) = (top("HXO", false), top("HXO\nHXO\nHXO", false));
        let (n1, n3) = (top("HXO", true), top("HXO\nHXO\nHXO", true));
        eprintln!(
            "改前（块心锚）：1 行 top={} → 3 行 top={} ⇒ 上移 {}px；改后（首行锚）：{} → {} ⇒ 差 {}px",
            o1.0,
            o3.0,
            o1.0 - o3.0,
            n1.0,
            n3.0,
            n1.0 - n3.0
        );
        assert!(
            o1.0 - o3.0 > 20,
            "旧口径应当能复现「行数多 ⇒ 首行上移」（实得 1 行 {} / 3 行 {}）",
            o1.0,
            o3.0
        );
        assert_eq!(
            n1.0, n3.0,
            "首行锚定后 1 行与 3 行的墨迹顶部必须相等（实得 {} vs {}）",
            n1.0, n3.0
        );
        // 单行时两种口径必须**逐像素相同**（不改变已上屏的单行画面）
        assert_eq!(n1.0, o1.0, "单行时首行锚定不应改变落点");
        assert_eq!(n1.1, o1.1, "单行时首行锚定不应改变墨迹下缘");
    }
//
    #[test]
    fn canvas_grow_stops_clipping_and_is_a_pure_translation() {
        // A string wider than the canvas: grow-on must return a canvas that holds the whole
        // ink (margins > 0 on all four sides) and the ink width/height must match what the
        // same text produces on a canvas big enough that nothing is clipped.
        let path = resolve_font(Path::new("/nonexistent"), "systemfont_arial")
            .expect("系统字体应当存在");
        let data = std::fs::read(path).unwrap();
        let glyphs = "WWWWWWWWWWWW";
        let mk = |grow: bool| TextStyle {
            font_px: 60.0,
            padding: 8.0,
            halign: "center",
            valign: "center",
            color: [1.0, 1.0, 1.0],
            plate: None,
            plate_follow_glyph: false,
            firstline: false,
            ascent_box: true,
            dy_canvas: 0.0,
            canvas_grow: grow,
        };
        let (small, sw, sh) = rasterize(&data, glyphs, &mk(true), 200, 90).unwrap();
        let (big, bw, bh) = rasterize(&data, glyphs, &mk(false), 1600, 400).unwrap();
        let (sx0, sy0, sx1, sy1) = {
            let (x0, y0, x1, y1) = ink_bbox(&small, sw, sh);
            (x0 as i32, y0 as i32, x1 as i32, y1 as i32)
        };
        let (bx0, by0, bx1, by1) = {
            let (x0, y0, x1, y1) = ink_bbox(&big, bw, bh);
            (x0 as i32, y0 as i32, x1 as i32, y1 as i32)
        };
        let (siw, sih) = (sx1 - sx0 + 1, sy1 - sy0 + 1);
        let (biw, bih) = (bx1 - bx0 + 1, by1 - by0 + 1);
        eprintln!(
            "grow: {}x{} ink {}x{} margins {}/{}/{}/{} | big: {}x{} ink {}x{}",
            sw, sh, siw, sih, sx0, sw as i32 - 1 - sx1, sy0, sh as i32 - 1 - sy1, bw, bh, biw, bih
        );
        assert!(sw > 200, "画布应当长大（实得 {}x{}）", sw, sh);
        assert!(siw == biw && sih == bih, "墨迹尺寸必须与不裁剪口径一致（{siw}x{sih} vs {biw}x{bih}）");
        assert!(sx0 > 0 && sy0 > 0, "墨迹不能贴左/上边（{sx0},{sy0}）");
        assert!(
            (sw as i32 - 1 - sx1) > 0 && (sh as i32 - 1 - sy1) > 0,
            "墨迹不能贴右/下边（{},{})",
            sw as i32 - 1 - sx1,
            sh as i32 - 1 - sy1
        );
    }
}
