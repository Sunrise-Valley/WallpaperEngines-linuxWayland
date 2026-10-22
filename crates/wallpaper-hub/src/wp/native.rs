// SPDX-License-Identifier: MIT
//
//! 自研内核（`we-native`）的控制面客户端。
//!
//! 内核以 `we-native --serve <socket>` 常驻，协议是**一行 JSON 请求 → 一行 JSON 响应**。
//!
//! {"cmd":"status"}
//! {"cmd":"apply","kind":"image|video|scene","path":"<绝对路径>"}
//! {"cmd":"pause"} / {"cmd":"resume"} / {"cmd":"quit"}
//! {"cmd":"set-prop","props":{"<属性名>":"<值>"}} # 壁纸参数（user property）
//! 这里只做三件事：
//!
//! 1. 把请求送出去（[`request_async`]，后台任务做真正的阻塞 socket I/O，不卡 UI 线程）
//!
//! 2. 周期性查状态，推给 UI（[`subscription`]）
//!
//! 3. 把 UI 的「某条壁纸」翻译成内核能播的**具体文件**（[`find_playable`]）

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use cosmic::iced::Subscription;
use cosmic::iced::futures::{self, SinkExt};
use tokio::sync::mpsc::UnboundedSender;

use crate::app::Message;
use crate::wp::proto;

/// 后台连接持有的出站通道。
static OUT: OnceLock<UnboundedSender<String>> = OnceLock::new();

/// 内核 socket 默认位置（与内核 `--serve` 的约定）。
pub fn sock_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".into());
    Path::new(&dir).join("we-native.sock")
}

// ── 请求行（纯函数，可单测）─────────────────────────────────────────────────

pub fn cmd_status() -> String {
    r#"{"cmd":"status"}"#.to_string()
}

pub fn cmd_pause() -> String {
    r#"{"cmd":"pause"}"#.to_string()
}

pub fn cmd_resume() -> String {
    r#"{"cmd":"resume"}"#.to_string()
}

/// 请内核退出。
///
/// 现在（waywallen 移除后）没有任何界面路径调用它 —— 内核由 systemd 服务托管、用
/// `systemctl --user restart` 重启。保留它是因为控制面协议里仍有 `quit`，将来要用不必重写。
#[allow(dead_code)]
pub fn cmd_quit() -> String {
    r#"{"cmd":"quit"}"#.to_string()
}

/// 性能档位的四项设置（内核侧纯机制、无状态 ⇒ 这里发什么它就执行什么）。
/// `fps` 走的是 **JSON 数字**（不是字符串）—— 内核解析器两种都收，但这里记一笔：
/// 当初界面发数字而内核只认字符串 ⇒ 回执一切正常、fps 却纹丝不动 ✗（成功假象）。
/// 取值都来自固定枚举，不做转义。
pub fn cmd_set(fps: u64, scale: &str, effects: &str, sleep: &str) -> String {
    format!(
        r#"{{"cmd":"set","fps":{fps},"scale":"{scale}","effects":"{effects}","sleep":"{sleep}"}}"#
    )
}

/// 取景模式（**几何**设置，跟 fps/scale/effects/sleep 那四项性能设置无关）。
/// 只对 scene 壁纸有意义：画布比例 ≠ 屏幕比例时怎么放。内核收到会**重建渲染器**
/// （投影是建的时候烘进去的），约 0.2 秒。
pub fn cmd_set_fit(mode: &str) -> String {
    format!(r#"{{"cmd":"set","fit":"{}"}}"#, json_escape(mode))
}

// ── 鼠标位（鼠标驱动特效用）──────────────────────────────────────────────────
//
// `{"cmd":"mouse","x":..,"y":..}`：**屏幕归一化 `[0,1]`，y 从屏幕顶往下**。
// 内核把它当场景级 uniform `g_PointerPosition` 每帧下发 ⇒ X-Ray 透视 / 视差这类特效跟着走。
//
// 为什么界面里要有这条：内核自己的 `wl_pointer` **只在指针位于桌面空白处**时才有 enter/motion
// （指针在任何窗口之上时归那个窗口 —— 这是「壁纸不抢输入」的代价；Wayland 也没有「全局指针位置」
// 这种客户端可查询的接口）。所以：
// · 桌面空白处 → 内核的 `wl_pointer` 自己会喂（`status.mouse_src == "wl_pointer"`）；
// · 其它情况 → 任何**拿得到指针位置**的进程都能推这条命令；界面里就是那个「设到画面中心」按钮
// （一眼验证「控制面 → 场景 → 屏幕」这条链路通不通）。
pub fn cmd_mouse(x: f32, y: f32) -> String {
    let (x, y) = (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0));
    // ⚠ 这里**不许出现反斜杠**：从"转义过的 JSON 文本"里抄一行过来，很容易写成
    // `r#"{{\"cmd\":…"#` —— 原始串里 `\"` 就是「反斜杠 + 引号」两个字符，于是送出去的
    // 行里真有反斜杠 ⇒ 内核 `find("\"cmd\"")` 找不到键 ⇒ 被当**未知命令静默忽略**
    // （回执照回、画面不动 —— 典型"成功假象"）。单测有一条专门钉「无反斜杠」。
    format!("{{\"cmd\":\"mouse\",\"x\":{x:.4},\"y\":{y:.4}}}")
}

/// 画面色彩调整（**观感**设置，跟性能四项无关）。六个逗号分隔的浮点：
/// `亮度,对比度,饱和度,色相(弧度),色温,伽马`，单位值 `1,1,1,0,0,1`。内核收到会重建那趟上屏层。
pub fn cmd_set_color(c: &str) -> String {
    format!(r#"{{"cmd":"set","color":"{}"}}"#, json_escape(c))
}

/// **播放速度**（场景时间倍率）：`{"cmd":"set","time_scale":1.25}`。
///
/// 内核把它乘在**场景时间**上（`g_Time`/粒子/网格动画一起走），音频与鼠标不受影响。
/// 范围 0.25..3.0（界面滑杆同口径，内核再夹一次）。值不是字符串 —— 与 `fps` 同一个教训
/// （发成字符串会被"宽容解析"吃掉、回执正常但画面不动）。
pub fn cmd_set_time_scale(v: f32) -> String {
    let v = v.clamp(0.25, 3.0);
    format!(r#"{{"cmd":"set","time_scale":{v:.3}}}"#)
}

/// **场景音频音量**：`{"cmd":"set","volume":55}`（0..100 百分比，内核只调**自己那条播放流**的音量，
/// 系统 sink 一动不动 —— per-stream，见 `we-native` 的 `audio_sinks`）。
pub fn cmd_set_volume(v: f32) -> String {
    let v = v.clamp(0.0, 100.0);
    format!(r#"{{"cmd":"set","volume":{v:.1}}}"#)
}

// ── 音频分析跟哪一路输出（sink）──────────────────────────────────────────────
//
// 内核对 `{"cmd":"audio","sinks":true}` 回一份**可用 sink 列表**（id / 名字 / 描述 / 是否默认），
// 对 `{"cmd":"audio","sink":"<id|名字|default|all>"}` 把分析切到那一路。枚举不到时内核**如实**
// 回 `{"ok":false,"error":"…"}` —— 界面必须把这句话显示出来，不能装作"没有 sink"。

/// 只列可用 sink（不改任何状态）。
pub fn cmd_audio_sinks() -> String {
    r#"{"cmd":"audio","sinks":true}"#.to_string()
}

/// 把音频分析切到某一路输出。
/// `spec`：`default`（跟随系统默认）/ sink 数字 id / sink 节点名 / `all`（内核如实报不支持）。
pub fn cmd_audio_sink(spec: &str) -> String {
    format!(r#"{{"cmd":"audio","sink":"{}"}}"#, json_escape(spec))
}

/// 一个可用输出设备（内核 `audio sinks` 回执里的一条）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SinkInfo {
    pub id: u32,
    pub name: String,
    pub desc: String,
    pub is_default: bool,
}

/// 解析 `{"ok":true,"sinks":[…]}` 回执。
///
/// **诚实降级**：`ok:false` ⇒ `Err(内核给的那句 error)`（例如 "无法枚举 sink：pactl 起不来"），
/// 界面拿它直接显示。`ok:true` 但列表为空 ⇒ `Ok(vec![])`（内核说"确实没有输出设备"）。
pub fn parse_sinks_reply(json: &str) -> Result<Vec<SinkInfo>, String> {
    if !get_bool(json, "ok") {
        let err = raw_value(json, "error").unwrap_or_else(|| "内核回了个看不懂的失败".into());
        return Err(err);
    }
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("回执不是合法 JSON（{e}）：{json}"))?;
    let arr = v
        .get("sinks")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::with_capacity(arr.len());
    for it in arr {
        let name = it.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
        if name.is_empty() {
            continue; // 没名字的条目当脏数据丢掉
        }
        out.push(SinkInfo {
            id: it.get("id").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            name,
            desc: it
                .get("desc")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            is_default: it.get("default").and_then(|x| x.as_bool()).unwrap_or(false),
        });
    }
    Ok(out)
}

/// 回执里报的"当前跟的是哪一路"（`{"cmd":"audio","sinks":true}` 的 `selected` 字段）。
pub fn reply_selected_sink(json: &str) -> Option<String> {
    raw_value(json, "selected").filter(|s| !s.is_empty())
}

/// 内核 `status.audio_sink` → 给人看的标签。`default`（或空）= 跟随系统默认。
pub fn sink_label(sel: &str) -> String {
    let s = sel.trim();
    if s.is_empty() || s == "default" {
        "默认（跟随系统）".to_string()
    } else {
        s.to_string()
    }
}

/// 从 `status.audio` 那一行里抠出 `analyzed=<n>`（内核每分析一个 FFT 窗 +1）。
/// 验收（"换 sink 后 analyzed 才涨"）与界面显示都用它。
pub fn analyzed_count(audio: &str) -> Option<u64> {
    let i = audio.find("analyzed=")? + "analyzed=".len();
    let rest = &audio[i..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

pub fn cmd_apply(kind: &str, path: &str) -> String {
    format!(
        r#"{{"cmd":"apply","kind":"{}","path":"{}"}}"#,
        json_escape(kind),
        json_escape(path)
    )
}

// ── 用户属性（壁纸参数页改的那一堆 bool/slider/color/combo/text）─────────────────
//
// 属性**定义**在壁纸自己的 `project.json → general.properties`（名字 + 类型 + 默认值），
// 场景里用 `"visible": {"user":"<名>","value":<默认值>}` 把对象/特效的显隐绑上去。
// 这里只做**通用搬运**：不认名字、不认类型，原样写进内核的覆盖表
// （内核再交给渲染器的 `WE_PROP_*` / `--prop` 那张表）。属性有哪些，界面从 schema 里读。

/// 把 JSON 属性值转成控制面里那个**字符串**（bool ⇒ `"true"`/`"false"`，字符串去引号，
/// 数字/其它 ⇒ 紧凑 JSON）。渲染器侧用 `parse_value` 再解释回 bool/数字/字符串。
pub fn value_str(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// 下发**一个**用户属性覆盖：
/// `{"cmd":"set-prop","props":{"<名>":"<值>"}}`。
///
/// **通用**：不针对任何属性名特判 —— 名字与值都原样透传，内核不认识它们也没关系。
pub fn cmd_set_prop(name: &str, value: &str) -> String {
    format!(
        r#"{{"cmd":"set-prop","props":{{"{}":"{}"}}}}"#,
        json_escape(name),
        json_escape(value)
    )
}

/// 批量下发一套属性覆盖（`clear=true` 时**先清空整张覆盖表**再写）。
///
/// 用途分工：**改单个属性**用 [`cmd_set_prop`]（合并，不动其它属性）；
/// **切壁纸**用本函数带 `clear=true`，把新壁纸的整套属性一次发过去 —— 属性名跨壁纸重名很常
/// （`newproperty` / `theme` 到处都是），不清就会把上一条的属性串到新壁纸身上。
pub fn cmd_set_props(props: &[(String, String)], clear: bool) -> String {
    let body: Vec<String> = props
        .iter()
        .map(|(k, v)| format!("\"{}\":\"{}\"", json_escape(k), json_escape(v)))
        .collect();
    if clear {
        format!(
            r#"{{"cmd":"set-prop","clear":true,"props":{{{}}}}}"#,
            body.join(",")
        )
    } else {
        format!(r#"{{"cmd":"set-prop","props":{{{}}}}}"#, body.join(","))
    }
}

/// 内核**当前播放**的那条（`status.path`）是不是来自这个壁纸目录。
///
/// 判据：scene 的 `path` 就是壁纸目录本身；视频/图片是目录里的某个文件（取父目录比）。
/// 界面据此决定"这次属性改动要不要**即时下发**"：只有正在播的那条才需要，
/// 其它条目落盘即可（下一条播到它时自然会带上）。
pub fn playing_matches_dir(status_path: &str, dir: &Path) -> bool {
    if status_path.is_empty() {
        return false;
    }
    let p = Path::new(status_path);
    p == dir || p.parent() == Some(dir)
}

/// JSON 字符串转义（`\` 与 `"`）
pub fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// 从响应里取一个键的值：引号串去引号并解转义，裸值（true/false/数字）原样返回。
pub fn raw_value(json: &str, key: &str) -> Option<String> {
    let k = format!("\"{key}\"");
    let i = json.find(&k)?;
    let rest = &json[i + k.len()..];
    let rest = &rest[rest.find(':')? + 1..];
    let rest = rest.trim_start();
    if let Some(stripped) = rest.strip_prefix('"') {
        let mut out = String::new();
        let mut esc = false;
        for ch in stripped.chars() {
            if esc {
                out.push(ch);
                esc = false;
                continue;
            }
            match ch {
                '\\' => esc = true,
                '"' => return Some(out),
                _ => out.push(ch),
            }
        }
        return None;
    }
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    Some(rest[..end].trim().to_string())
}

pub fn get_bool(json: &str, key: &str) -> bool {
    raw_value(json, key).as_deref() == Some("true")
}

pub fn get_num(json: &str, key: &str) -> Option<f64> {
    raw_value(json, key)?.parse().ok()
}

/// 从 `set` 回执里取内核**实际生效**的四项 `(fps, scale, effects, sleep)`。
///
/// 内核回的是它自己的字段值（可能被 clamp / 过滤过）⇒ 界面拿它显示"内核当前值"，
/// 一眼能看出界面和内核算的是不是同一组数（这就是 003 里"数字 vs 字符串"那个成功假象的解药：
/// 别只看回执 ok，要看回执里那四个数）。
pub fn perf_from_reply(reply: &str) -> Option<(u64, String, String, String)> {
    if !get_bool(reply, "ok") {
        return None;
    }
    Some((
        get_num(reply, "fps")? as u64,
        raw_value(reply, "scale")?,
        raw_value(reply, "effects")?,
        raw_value(reply, "sleep")?,
    ))
}

// ── 状态 ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    pub kind: String,
    pub path: String,
    pub playing: bool,
    pub suppressed: bool,
    pub user_paused: bool,
    pub frames: u64,
    pub decode_w: u32,
    pub decode_h: u32,
    pub outputs: u32,
    pub cpu_s: f64,
    pub rss_mb: f64,
    /// `status.fps`：内核**当前生效**的帧率上限（自己回报的数，界面拿它显示"内核当前值"）。
    /// 老内核没这个字段 ⇒ 0（界面据此不显示，不编一个假数）。
    pub fps: u64,
    /// `status.audio`：采集状态一行（`设备:状态(live) analyzed=N sink=… device=…`）
    pub audio: String,
    /// `status.audio_sink`：**当前分析跟的是哪一路**（`default` = 跟随系统默认）
    pub audio_sink: String,
    // ── 指针（鼠标驱动特效）诊断字段（内核 起回报；老内核缺字段 ⇒ 空/0）────
    /// `status.mouse`：内核当前的鼠标位（屏幕归一化 `x,y`，y 从顶往下）；空 = 还没有过任何输入
    pub mouse: String,
    /// `status.mouse_src`：**这个位置是谁写的**：`wl_pointer`（合成器给的）/`control`（控制面）/`none`
    pub mouse_src: String,
    /// `status.mouse_events`：成功换算成场景坐标的 enter/motion 数（= 真正驱动了画面的那些）
    pub mouse_events: u64,
    /// `status.pointer_events`：`wl_pointer` 收到的**原始**事件数（含 enter/leave/其它）——
    /// 这是"合成器到底有没有发指针事件"的唯一诚实指标（`mouse_events==0` 且它`==0`
    /// 才说明"一个都没来"）
    pub pointer_events: u64,
    pub mouse_enter: u64,
    pub mouse_leave: u64,
    pub mouse_motion: u64,
    /// `status.pointer_bound`：合成器是否给了 `wl_pointer`（`wl_seat.capabilities` 里有 pointer）
    pub pointer_bound: bool,
}

/// 指针状态一行（给人看的）：来源 + 位置 + 事件计数。
///
/// 这一行的用处是把"鼠标驱动特效为什么不动"这件事**说清**：
/// 指针在窗口上时背景层拿不到它（Wayland 不给客户端查询全局指针位置的接口），
/// 所以"一个事件都没有"和"有事件、只是最近没动"必须能一眼分开。
pub fn pointer_summary(s: &Status) -> String {
    let pos = if s.mouse.is_empty() { "—".to_string() } else { s.mouse.clone() };
    match s.mouse_src.as_str() {
        "wl_pointer" => format!(
            "合成器给的 wl_pointer：位置 {pos}｜enter {} / motion {} / leave {}（原始 {}）",
            s.mouse_enter, s.mouse_motion, s.mouse_leave, s.pointer_events
        ),
        "control" => format!(
            "控制面命令：位置 {pos}｜wl_pointer 累计 enter {} / motion {}（原始 {}）",
            s.mouse_enter, s.mouse_motion, s.pointer_events
        ),
        _ if s.pointer_bound => format!(
            "还没有任何指针输入（wl_pointer 已绑，原始事件 {}；指针在窗口上时背景层看不到它）",
            s.pointer_events
        ),
        _ => "内核没拿到 wl_pointer（合成器没给 pointer 能力）".to_string(),
    }
}

impl Status {
    pub fn parse(json: &str) -> Option<Status> {
        if !get_bool(json, "ok") {
            return None;
        }
        Some(Status {
            kind: raw_value(json, "kind").unwrap_or_else(|| "none".into()),
            path: raw_value(json, "path").unwrap_or_default(),
            playing: get_bool(json, "playing"),
            suppressed: get_bool(json, "suppressed"),
            user_paused: get_bool(json, "user_paused"),
            frames: get_num(json, "frames").unwrap_or(0.0) as u64,
            decode_w: get_num(json, "decode_w").unwrap_or(0.0) as u32,
            decode_h: get_num(json, "decode_h").unwrap_or(0.0) as u32,
            outputs: get_num(json, "outputs").unwrap_or(0.0) as u32,
            cpu_s: get_num(json, "cpu_s").unwrap_or(0.0),
            rss_mb: get_num(json, "rss_mb").unwrap_or(0.0),
            // 内核自己回报的当前帧率上限（老内核缺字段 ⇒ 0）
            fps: get_num(json, "fps").unwrap_or(0.0) as u64,
            audio: raw_value(json, "audio").unwrap_or_default(),
            audio_sink: raw_value(json, "audio_sink").unwrap_or_default(),
            // 指针诊断字段（老内核没这些字段 ⇒ 空串/0，不炸）
            mouse: raw_value(json, "mouse").unwrap_or_default(),
            mouse_src: raw_value(json, "mouse_src").unwrap_or_default(),
            mouse_events: get_num(json, "mouse_events").unwrap_or(0.0) as u64,
            pointer_events: get_num(json, "pointer_events").unwrap_or(0.0) as u64,
            mouse_enter: get_num(json, "mouse_enter").unwrap_or(0.0) as u64,
            mouse_leave: get_num(json, "mouse_leave").unwrap_or(0.0) as u64,
            mouse_motion: get_num(json, "mouse_motion").unwrap_or(0.0) as u64,
            pointer_bound: get_bool(json, "pointer_bound"),
        })
    }
}

// ── 把 UI 的壁纸条目翻译成内核能播的文件 ────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Video,
    Image,
    /// WE 场景包（`scene.pkg`）—— 自研内核自己解析渲染（17/17 条实测能画）
    Scene,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Video => "video",
            Kind::Image => "image",
            Kind::Scene => "scene",
        }
    }
}

/// 按扩展名判断是什么媒体（不含路径）
pub fn kind_for_name(name: &str) -> Option<Kind> {
    let ext = Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())?;
    match ext.as_str() {
        "mp4" | "webm" | "mkv" | "mov" | "avi" => Some(Kind::Video),
        "jpg" | "jpeg" | "png" | "webp" | "bmp" => Some(Kind::Image),
        _ => None,
    }
}

/// 内核能直接播的类型吗；不能就说明原因。
///
/// ⚠ `scene` **现在能播了**（自研渲染器已支持 PKGV/TEXB，本机 17/17 条 scene 实测能画）。
/// 这里以前写死"scene 不支持"，界面据此把 scene 全过滤掉 ⇒ 自研后端等于用不了
/// （用户库里绝大多数是 scene）。web/application 仍不支持。
pub fn unsupported_reason(wp_type: &str) -> Option<String> {
    match wp_type.to_ascii_lowercase().as_str() {
        "application" | "web" => Some(format!("内核暂不支持 {wp_type} 类型")),
        _ => None,
    }
}

/// 这条壁纸在磁盘上的内容目录。
/// 用 `external_id`（= 目录名）在**全部本地库根目录**里找（`~/Templates` 之类的用户
/// 目录优先，Steam 工坊目录只是可选的额外来源）；否则退回 daemon 给的 `resource`。
pub fn entry_dir(entry: &proto::WallpaperEntry) -> Option<PathBuf> {
    if !entry.external_id.is_empty()
        && let Some(d) = crate::wp::library::resolve_id(&entry.external_id)
    {
        return Some(d);
    }
    if entry.resource.is_empty() {
        return None;
    }
    let p = PathBuf::from(&entry.resource);
    if p.is_dir() {
        Some(p)
    } else {
        p.parent().map(|x| x.to_path_buf())
    }
}

/// 在目录里找内核能播的东西（有界递归）。
/// **scene 包优先**（`scene.pkg`/`scene.json` ⇒ 整条场景）；否则视频优先于图片；
/// 跳过 `preview*`（那是缩略图，不是壁纸本体）。
pub fn find_playable(dir: &Path) -> Option<(Kind, PathBuf)> {
    // 顶层有 scene.pkg 就直接当场景，不必递归
    for n in ["scene.pkg", "scene.json"] {
        let p = dir.join(n);
        if p.is_file() {
            return Some((Kind::Scene, dir.to_path_buf()));
        }
    }
    let mut video = None;
    let mut image = None;
    walk(dir, 0, &mut video, &mut image);
    video.or(image)
}

fn walk(dir: &Path, depth: usize, video: &mut Option<(Kind, PathBuf)>, image: &mut Option<(Kind, PathBuf)>) {
    if depth > 3 || (video.is_some() && image.is_some()) {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, depth + 1, video, image);
            continue;
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if name.starts_with("preview") {
            continue;
        }
        match kind_for_name(&name) {
            Some(Kind::Video) if video.is_none() => *video = Some((Kind::Video, path)),
            Some(Kind::Image) if image.is_none() => *image = Some((Kind::Image, path)),
            _ => {}
        }
    }
}

// ── 与内核通信 ──────────────────────────────────────────────────────────────

/// 同步发一条请求并把一行响应读回来（本地 socket，微秒级；`apply` 可能要几百毫秒）。
pub fn query(line: &str) -> Result<String, String> {
    let path = sock_path();
    let mut s = UnixStream::connect(&path)
        .map_err(|e| format!("连不上内核 {}（没在 --serve？）: {e}", path.display()))?;
    s.set_read_timeout(Some(Duration::from_secs(15))).ok();
    s.set_write_timeout(Some(Duration::from_secs(15))).ok();
    s.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    s.write_all(b"\n").map_err(|e| e.to_string())?;
    s.flush().map_err(|e| e.to_string())?;
    let mut buf = String::new();
    BufReader::new(&s).read_line(&mut buf).map_err(|e| e.to_string())?;
    Ok(buf.trim().to_string())
}

async fn query_async(line: String) -> String {
    match tokio::task::spawn_blocking(move || query(&line)).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(e)) => format!(r#"{{"ok":false,"error":"{}"}}"#, json_escape(&e)),
        Err(e) => format!(r#"{{"ok":false,"error":"后台任务失败: {}"}}"#, json_escape(&e.to_string())),
    }
}

/// 这一行是不是 `{"cmd":"status"}` 轮询（界面拿它决定回执走 `NativeStatus` 还是 `NativeReply`）。
pub fn is_status_line(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("cmd").and_then(|c| c.as_str()).map(|s| s == "status"))
        .unwrap_or(false)
}

/// 投递一条请求（后台任务执行）。返回 false 表示后台还没起来。
pub fn request_async(line: String) -> bool {
    OUT.get().is_some_and(|tx| tx.send(line).is_ok())
}

// ── 出站合并队列（拖动节流）─────────────────────────────────────────────────
// 为什么必须做：色盘一次 3 秒拖动会让 UI 每帧发一条 `set-prop`，
// 而内核每收到一条就**重建整个场景**（同一台机器上实测 5 次 `set-prop`：中位 **620 ms**/条，
// 见 `we-ctl` 之外的裸 socket 计时）。旧出站通道是**无界 FIFO** ⇒ UI 发 90 条、内核只能吃
// 1.6 条/秒 ⇒ 回执要排队几十秒（当天 journal 里那次拖动留下的尾巴排了 **6 分钟**：
// 16:21:25 → 16:27:31 共 649 条），期间任何新操作都排在后面 ⇒ 用户看到的就是
// 「UI 又失效了」。
//
// 这里把"排队"换成"合并"：
// · 同一属性的 `set-prop`（以及 `set color`）在队列里**只留最新一条**（原地替换，
// 不改变相对顺序 ⇒ 其它命令的先后不会乱）；
// · 相邻两条合并类命令之间至少隔 [`prop_interval`]（默认 55 ms ≈ 18 Hz）
// ⇒ 3 秒拖动最多 3/0.055 ≈ 55 条；
// · 松手时 [`flush_now`] 立刻放行（终值不等下一个节流窗口）；
// · **终值永不丢**：最新值一直躺在队列里，直到真的被发出去为止。

/// 出站队列里的一条。
struct OutItem {
    /// `Some(key)` = 可合并类（同一 key 只留最新一条）；`None` = 不许合并（顺序命令）
    key: Option<String>,
    line: String,
}

/// 合并键：只有"拖动时每帧都会变"的那两类命令有键，其余一律 `None`。
///
/// * `{"cmd":"set-prop","props":{"<名>":"<值>"}}`（**单条**属性）⇒ `"sp:<名>"`
/// （批量 `set-prop` 带多条属性时不合并：一次请求里本来就没有重复项）
///
/// * `{"cmd":"set","color":…}`（画面色彩）⇒ `"color"`
pub fn coalesce_key(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let cmd = v.get("cmd")?.as_str()?;
    match cmd {
        "set-prop" => {
            let props = v.get("props")?.as_object()?;
            if props.len() != 1 {
                return None;
            }
            let (k, _) = props.iter().next()?;
            Some(format!("sp:{k}"))
        }
        // 同一类拖动产物各自一个 key ⇒ 队列里只留最新一条（不同的旋钮互不覆盖）
        "set" => {
            if v.get("color").is_some() {
                Some("color".to_string())
            } else if v.get("time_scale").is_some() {
                Some("time_scale".to_string())
            } else if v.get("volume").is_some() {
                Some("volume".to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// 两条合并类命令之间的最小间隔（`WPHUB_PROP_MS`，默认 55 ms ⇒ 约 18 Hz）。
///
/// 上限为什么是 18 而不是 20：验收线是"3 秒 ≤ 60 条"，55 ms 留了 5 条的余量，
/// 免得卡在 60 条的边界上（定时器精度、首条立即发送都会吃掉一点）。
pub fn prop_interval() -> Duration {
    static D: OnceLock<Duration> = OnceLock::new();
    *D.get_or_init(|| {
        let ms = std::env::var("WPHUB_PROP_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|v| *v <= 5000)
            .unwrap_or(55);
        Duration::from_millis(ms)
    })
}

/// 松手时置位：下一条合并类命令**不再等**节流窗口。
static FORCE_NOW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 立刻放行下一条合并类命令（拖拽松手时调用：终值必须马上到内核）。
pub fn flush_now() {
    FORCE_NOW.store(true, std::sync::atomic::Ordering::SeqCst);
    notify().notify_one();
}

/// 驱动循环的唤醒器（入队 / `flush_now` 各叫一次）。
static NOTIFY: OnceLock<Arc<tokio::sync::Notify>> = OnceLock::new();

fn notify() -> Arc<tokio::sync::Notify> {
    NOTIFY.get_or_init(|| Arc::new(tokio::sync::Notify::new())).clone()
}

/// 出站合并队列（纯数据 + 两个时刻，单测直接钉它）。
#[derive(Default)]
pub struct OutQueue {
    items: std::collections::VecDeque<OutItem>,
    /// 上一条**合并类**命令被取走的时刻（节流基准）
    last_sent: Option<Instant>,
}

impl OutQueue {
    pub fn new() -> Self {
        Self::default()
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 入队。可合并类且队列里已有同 key ⇒ **原地替换**（位置不变，值取最新）。
    pub fn push(&mut self, line: String) {
        let key = coalesce_key(&line);
        if let Some(k) = &key {
            if let Some(it) = self.items.iter_mut().find(|it| it.key.as_deref() == Some(k)) {
                it.line = line;
                return;
            }
        }
        self.items.push_back(OutItem { key, line });
    }

    fn ready(&self, now: Instant) -> bool {
        match self.items.front() {
            None => false,
            Some(it) if it.key.is_some() => {
                if FORCE_NOW.load(std::sync::atomic::Ordering::SeqCst) {
                    return true;
                }
                match self.last_sent {
                    None => true,
                    Some(t) => now.saturating_duration_since(t) >= prop_interval(),
                }
            }
            // 顺序命令（apply / pause / status …）从不等待：它们不是拖动产生的
            Some(_) => true,
        }
    }

    /// 下一条**最早**可以发出的时刻；`None` = 队列空（驱动方可以睡到天荒地老）。
    pub fn next_due(&self, now: Instant) -> Option<Instant> {
        let it = self.items.front()?;
        if it.key.is_none() || self.ready(now) {
            return Some(now);
        }
        let t = self.last_sent?;
        Some(t + prop_interval())
    }

    /// 该发就取走一条（取不走返回 `None`）。
    pub fn pop_due(&mut self, now: Instant) -> Option<String> {
        if !self.ready(now) {
            return None;
        }
        let it = self.items.pop_front()?;
        if it.key.is_some() {
            self.last_sent = Some(now);
            FORCE_NOW.store(false, std::sync::atomic::Ordering::SeqCst);
        }
        Some(it.line)
    }

    /// 队头是不是合并类（诊断/单测用）。
    #[allow(dead_code)]
    pub fn front_is_coalescable(&self) -> bool {
        self.items.front().is_some_and(|i| i.key.is_some())
    }
}

/// 进程内唯一的出站队列（`request_async` 往里塞，订阅里的驱动循环往外取）。
static QUEUE: OnceLock<Arc<Mutex<OutQueue>>> = OnceLock::new();

/// 共享句柄（驱动循环 / 单测都用它）。
pub type SharedQueue = Arc<Mutex<OutQueue>>;

fn queue() -> SharedQueue {
    QUEUE
        .get_or_init(|| Arc::new(Mutex::new(OutQueue::new())))
        .clone()
}

/// 出站驱动：把队列里的行一条条送出去，并按 [`prop_interval`] 合并/限速。
///
/// `send` 负责"发一行、等回执"（真实实现 = [`query_async`]）；`emit` 把
/// `(请求行, 回执行)` 推给界面。抽成两个闭包是为了让**验收测试**能用假内核
/// （含人为的 600 ms 延迟 = 实测重建耗时）驱动**同一段代码**，从而量出真数字。
///
/// 返回的 future 永不完成（直到 `emit` 让上层退出）；调用方用 `select!` 与状态轮询并跑。
pub async fn drive<F, Fut, G, Fut2>(
    q: SharedQueue,
    notify: Arc<tokio::sync::Notify>,
    status_line: Option<String>,
    status_every: Option<Duration>,
    mut send: F,
    mut emit: G,
) where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = String>,
    G: FnMut(String, String) -> Fut2,
    Fut2: std::future::Future<Output = ()>,
{
    let mut ticker = status_every.map(tokio::time::interval);
    loop {
        // ① 该发就发一条（发的同时**不阻塞入队**：其它任务照旧 push，同 key 直接合并）
        let head = {
            let mut g = q.lock().unwrap_or_else(|e| e.into_inner());
            g.pop_due(Instant::now())
        };
        if let Some(line) = head {
            let reply = send(line.clone()).await;
            emit(line, reply).await;
            continue;
        }
        // ② 队列空 → 睡到天荒地老（等 notify）；队头在等节流窗口 → 睡到那一刻
        let sleep_until = {
            let g = q.lock().unwrap_or_else(|e| e.into_inner());
            match g.next_due(Instant::now()) {
                Some(t) => tokio::time::Instant::from_std(t),
                None => tokio::time::Instant::now() + Duration::from_secs(3600),
            }
        };
        let tick = async {
            match ticker.as_mut() {
                Some(t) => {
                    t.tick().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = notify.notified() => {}
            _ = tokio::time::sleep_until(sleep_until) => {}
            _ = tick => {
                if let Some(l) = &status_line {
                    let mut g = q.lock().unwrap_or_else(|e| e.into_inner());
                    g.push(l.clone());
                }
            }
        }
    }
}

/// 常驻订阅：消费出站请求 + 每 3 秒查一次状态。
/// 状态查询很便宜（本地 socket + 内核只是回一行 JSON），用来驱动状态条。
pub fn subscription() -> Subscription<Message> {
    Subscription::run(|| {
        cosmic::iced::stream::channel(
            64,
            |out: futures::channel::mpsc::Sender<Message>| async move {
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                let _ = OUT.set(tx);
                let q = queue();
                let notify = notify();
                // 入队侧：来的每一条都进合并队列（同 key 原地替换 ⇒ 拖动不堆消息）
                {
                    let q = q.clone();
                    let n = notify.clone();
                    tokio::spawn(async move {
                        while let Some(line) = rx.recv().await {
                            {
                                let mut g = q.lock().unwrap_or_else(|e| e.into_inner());
                                g.push(line);
                            }
                            n.notify_one();
                        }
                    });
                }
                drive(
                    q,
                    notify,
                    Some(cmd_status()),
                    Some(Duration::from_secs(3)),
                    |line| async move { query_async(line).await },
                    |line, reply| {
                        let mut out = out.clone();
                        async move {
                            // 每 3 秒那条轮询走 `NativeStatus`（界面在它里面解析状态），
                            // 其它回执仍按 `NativeReply`（界面在它里面读 ok/error/四项）。
                            let m = if is_status_line(&line) {
                                Message::NativeStatus(reply)
                            } else {
                                Message::NativeReply(line, reply)
                            };
                            let _ = out.send(m).await;
                        }
                    },
                )
                .await;
            },
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实内核回的 status（照抄实测输出）
    const REAL: &str = r#"{"ok":true,"kind":"video","path":"/home/user/Wallpapers/test-clip.mp4","playing":true,"suppressed":false,"user_paused":false,"frames":195,"decode_w":1920,"decode_h":1080,"outputs":1,"cpu_s":1.17,"rss_mb":39.0}"#;

    #[test]
    fn parses_real_status_reply() {
        let s = Status::parse(REAL).expect("应能解析");
        assert_eq!(s.kind, "video");
        assert_eq!(s.path, "/home/user/Wallpapers/test-clip.mp4");
        assert!(s.playing);
        assert!(!s.suppressed && !s.user_paused);
        assert_eq!(s.frames, 195);
        assert_eq!((s.decode_w, s.decode_h), (1920, 1080));
        assert_eq!(s.outputs, 1);
        assert!((s.cpu_s - 1.17).abs() < 1e-9);
        assert!((s.rss_mb - 39.0).abs() < 1e-9);
    }

    /// ⭐ 内核 `status.fps`：内核**当前生效**的帧率上限 —— 界面显示"内核当前值"要用它。
    /// 老内核没这个字段 ⇒ 0（界面据此不显示，不编假数）。
    #[test]
    fn parses_kernel_fps_from_status() {
        // 照抄内核 status 的形状（s3 起 `"fps":{}` 就在里面）
        let j = r#"{"ok":true,"kind":"scene","path":"/w/2564205376","playing":true,"frames":61000,"fps":60,"scale":"physical","effects":"all","sleep":"maximized"}"#;
        let s = Status::parse(j).expect("应能解析");
        assert_eq!(s.fps, 60, "内核报的生效帧率要读出来");
        assert_eq!(s.frames, 61000);
        // 老回执（没有 fps 字段）⇒ 0，不炸
        assert_eq!(Status::parse(REAL).expect("应能解析").fps, 0);
    }

    /// ⭐ 内核新加的 `audio` / `audio_sink` 两个字段（照抄实测输出）：
    /// 界面要能一眼看出"分析跟的是哪一路"以及"analyzed 涨到多少了"。
    #[test]
    fn parses_audio_fields_from_real_status() {
        let j = r#"{"ok":true,"kind":"scene","path":"/w/2997301801","playing":true,"audio":"monitor:alsa_output.pci-0000_09_00.1.hdmi-stereo.monitor:running(live) analyzed=4321 sink=default device=<系统默认>","audio_sink":"default"}"#;
        let s = Status::parse(j).expect("应能解析");
        assert_eq!(s.audio_sink, "default");
        assert_eq!(analyzed_count(&s.audio), Some(4321));
        assert_eq!(sink_label(&s.audio_sink), "默认（跟随系统）");
        // 指定了某一路
        let j2 = r#"{"ok":true,"audio_sink":"whtest"}"#;
        assert_eq!(Status::parse(j2).unwrap().audio_sink, "whtest");
        assert_eq!(sink_label("whtest"), "whtest");
        assert_eq!(sink_label(""), "默认（跟随系统）");
        assert_eq!(sink_label("  "), "默认（跟随系统）");
        // 老回执（没有这两个字段）⇒ 空串，不炸
        let old = Status::parse(REAL).unwrap();
        assert!(old.audio.is_empty() && old.audio_sink.is_empty());
        assert_eq!(analyzed_count(""), None);
        assert_eq!(analyzed_count("analyzed="), None);
        assert_eq!(analyzed_count("(live) analyzed=7 sink=x"), Some(7));
    }

    /// sink 请求行：**逐字节**钉住（内核解析照这个写的）。
    #[test]
    fn audio_sink_request_lines_are_exact() {
        assert_eq!(cmd_audio_sinks(), r#"{"cmd":"audio","sinks":true}"#);
        assert_eq!(cmd_audio_sink("default"), r#"{"cmd":"audio","sink":"default"}"#);
        assert_eq!(cmd_audio_sink("593"), r#"{"cmd":"audio","sink":"593"}"#);
        assert_eq!(
            cmd_audio_sink("alsa_output.pci-0000_09_00.1.hdmi-stereo"),
            r#"{"cmd":"audio","sink":"alsa_output.pci-0000_09_00.1.hdmi-stereo"}"#
        );
        // 名字里有引号/反斜杠要转义（否则整行 JSON 废掉 ⇒ 内核当未知命令）
        assert_eq!(
            cmd_audio_sink(r#"a"b\c"#),
            r#"{"cmd":"audio","sink":"a\"b\\c"}"#
        );
    }

    /// 列 sink 的回执解析：正常 / 空 / **枚举失败（如实报错）** / 脏数据。
    #[test]
    fn parses_sinks_reply_including_honest_failure() {
        let ok = r#"{"ok":true,"sinks":[{"id":593,"name":"alsa_output.pci-0000_09_00.1.hdmi-stereo","desc":"HDMI 数字立体声","monitor":"alsa_output.pci-0000_09_00.1.hdmi-stereo.monitor","default":true},{"id":700,"name":"whtest","desc":"WHTEST 虚拟输出","monitor":"whtest.monitor","default":false}],"default":"alsa_output.pci-0000_09_00.1.hdmi-stereo","selected":"default","note":""}"#;
        let v = parse_sinks_reply(ok).expect("应能解析");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].id, 593);
        assert_eq!(v[0].desc, "HDMI 数字立体声");
        assert!(v[0].is_default);
        assert_eq!(v[1].name, "whtest");
        assert!(!v[1].is_default);
        assert_eq!(reply_selected_sink(ok).as_deref(), Some("default"));
        // 枚举失败：把内核那句 error 原样带出来（界面据此显示"无法枚举"）
        let err = r#"{"ok":false,"error":"无法枚举 sink：pactl 起不来（No such file or directory）"}"#;
        let e = parse_sinks_reply(err).unwrap_err();
        assert!(e.contains("无法枚举"), "{e}");
        // ok 但没有 sinks 字段 / 空数组 ⇒ 空列表（不是错误）
        assert_eq!(parse_sinks_reply(r#"{"ok":true}"#).unwrap(), vec![]);
        assert_eq!(parse_sinks_reply(r#"{"ok":true,"sinks":[]}"#).unwrap(), vec![]);
        assert_eq!(reply_selected_sink(r#"{"ok":true,"selected":""}"#), None);
        // 脏条目（没名字）丢掉，不 panic
        let dirty = r#"{"ok":true,"sinks":[{"id":1,"desc":"x"},{"id":2,"name":"good"}]}"#;
        let v = parse_sinks_reply(dirty).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "good");
        assert_eq!(v[0].id, 2);
        // 回执坏到不是 JSON ⇒ Err（不 panic）
        assert!(parse_sinks_reply("内核回了 pcatl 报错").is_err());
    }

    #[test]
    fn parse_rejects_error_reply() {
        assert!(Status::parse(r#"{"ok":false,"error":"路径不存在：/nope"}"#).is_none());
    }

    #[test]
    fn raw_value_handles_quoted_bare_and_escapes() {
        assert_eq!(raw_value(REAL, "kind").as_deref(), Some("video"));
        assert_eq!(raw_value(REAL, "frames").as_deref(), Some("195"));
        assert_eq!(raw_value(REAL, "rss_mb").as_deref(), Some("39.0"));
        // 转义：路径里有引号和反斜杠
        let j = r#"{"path":"/tmp/a\"b\\c.mp4"}"#;
        assert_eq!(raw_value(j, "path").as_deref(), Some(r#"/tmp/a"b\c.mp4"#));
        assert_eq!(raw_value(REAL, "no_such_key"), None);
    }

    #[test]
    fn apply_line_escapes_path() {
        let line = cmd_apply("video", r#"/tmp/a"b\c.mp4"#);
        assert_eq!(line, r#"{"cmd":"apply","kind":"video","path":"/tmp/a\"b\\c.mp4"}"#);
        assert_eq!(cmd_status(), r#"{"cmd":"status"}"#);
        assert_eq!(
            cmd_set(15, "logical", "none", "maximized"),
            r#"{"cmd":"set","fps":15,"scale":"logical","effects":"none","sleep":"maximized"}"#
        );
        assert_eq!(cmd_pause(), r#"{"cmd":"pause"}"#);
    }

    /// 属性下发的请求行：**逐字节**钉住格式（内核的解析就是照这个写的；
    /// 一旦这里飘了，屏幕上就会"回执成功但画面没变"——003 里踩过的成功假象）。
    #[test]
    fn set_prop_line_is_exact_and_generic() {
        assert_eq!(
            cmd_set_prop("newproperty", "true"),
            r#"{"cmd":"set-prop","props":{"newproperty":"true"}}"#
        );
        // 不针对属性名特判：随便一个名字/值都照发
        assert_eq!(
            cmd_set_prop("把把", "0.5"),
            r#"{"cmd":"set-prop","props":{"把把":"0.5"}}"#
        );
        // 名字与值里的引号/反斜杠要转义（否则整行 JSON 就废了 ⇒ 内核解析成未知命令）
        assert_eq!(
            cmd_set_prop(r#"a"b"#, r#"c\d"#),
            r#"{"cmd":"set-prop","props":{"a\"b":"c\\d"}}"#
        );
    }

    #[test]
    fn set_props_batch_and_clear() {
        let props = vec![
            ("newproperty".to_string(), "true".to_string()),
            ("newproperty1".to_string(), "false".to_string()),
        ];
        assert_eq!(
            cmd_set_props(&props, false),
            r#"{"cmd":"set-prop","props":{"newproperty":"true","newproperty1":"false"}}"#
        );
        assert_eq!(
            cmd_set_props(&props, true),
            r#"{"cmd":"set-prop","clear":true,"props":{"newproperty":"true","newproperty1":"false"}}"#
        );
        // 空集：清表（切壁纸到一张没有任何覆盖的壁纸）
        assert_eq!(cmd_set_props(&[], true), r#"{"cmd":"set-prop","clear":true,"props":{}}"#);
        assert_eq!(cmd_set_props(&[], false), r#"{"cmd":"set-prop","props":{}}"#);
    }

    /// JSON 值 → 控制面字符串：bool 要变成内核认得的那两个词，字符串要去引号。
    #[test]
    fn value_str_maps_json_values() {
        use serde_json::json;
        assert_eq!(value_str(&json!(true)), "true");
        assert_eq!(value_str(&json!(false)), "false");
        assert_eq!(value_str(&json!("0.1 0.2 0.3")), "0.1 0.2 0.3");
        assert_eq!(value_str(&json!(2)), "2");
        assert_eq!(value_str(&json!(0.5)), "0.5");
        assert_eq!(value_str(&json!(null)), "");
    }

    /// 「这条改动要不要即时下发」的判据：只对**正在播的那条**为真。
    #[test]
    fn playing_matches_dir_covers_scene_and_file() {
        let dir = Path::new("/w/2887537617");
        // scene：内核的 path 就是壁纸目录
        assert!(playing_matches_dir("/w/2887537617", dir));
        // 视频/图片：内核的 path 是目录里的文件
        assert!(playing_matches_dir("/w/2887537617/movie.mp4", dir));
        // 别的壁纸 / 空状态 ⇒ 不下发（只落盘）
        assert!(!playing_matches_dir("/w/3704273480", dir));
        assert!(!playing_matches_dir("/w/2887537617x/a.mp4", dir));
        assert!(!playing_matches_dir("", dir));
        assert!(!playing_matches_dir("/w/2887537617", Path::new("/w/other")));
    }

    #[test]
    fn kind_by_extension() {
        assert_eq!(kind_for_name("a.MP4"), Some(Kind::Video));
        assert_eq!(kind_for_name("b.webm"), Some(Kind::Video));
        assert_eq!(kind_for_name("c.PNG"), Some(Kind::Image));
        assert_eq!(kind_for_name("scene.pkg"), None);
        assert_eq!(kind_for_name("noext"), None);
        assert_eq!(Kind::Video.as_str(), "video");
        assert_eq!(Kind::Image.as_str(), "image");
    }

    #[test]
    fn scene_is_supported_now() {
        // 自研渲染器已能画 scene（本机 17/17 条实测）⇒ 不能再把 scene 判成"不支持"，
        // 否则界面会把用户库里绝大多数壁纸过滤掉（这正是"自研后端用不了"的老毛病）。
        assert!(unsupported_reason("scene").is_none());
        assert!(unsupported_reason("Scene").is_none());
        assert!(unsupported_reason("video").is_none());
        assert!(unsupported_reason("image").is_none());
        // 仍然不支持的两种，要给出原因（不是静默丢弃）
        assert!(unsupported_reason("web").is_some());
        assert!(unsupported_reason("application").is_some());
    }

    #[test]
    fn find_playable_prefers_video_and_skips_preview() {
        let base = std::env::temp_dir().join(format!("wp-native-test-{}", std::process::id()));
        let sub = base.join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(base.join("preview.jpg"), b"x").unwrap();
        std::fs::write(base.join("still.png"), b"x").unwrap();
        std::fs::write(sub.join("movie.mp4"), b"x").unwrap();

        let got = find_playable(&base).expect("应找到一个可播文件");
        assert_eq!(got.0, Kind::Video, "视频应优先于图片");
        assert!(got.1.ends_with("movie.mp4"));

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn find_playable_returns_scene_for_scene_dir() {
        let base = std::env::temp_dir().join(format!("wp-native-test-scene-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("scene.pkg"), b"x").unwrap();
        std::fs::write(base.join("preview.jpg"), b"x").unwrap(); // 缩略图不算，但 scene.pkg 命中

        let got = find_playable(&base).expect("有 scene.pkg 就该当场景播");
        assert_eq!(got.0, Kind::Scene);
        assert_eq!(got.0.as_str(), "scene", "下发给内核的 kind 必须是 scene");
        assert_eq!(got.1, base, "场景要交目录（内核自己去读 scene.pkg）");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn parses_video_status_fields() {
        // 状态条那句文案已随 UI 改版删除 ⇒ 这里改成直接断言**字段**
        // （比断言文案结实：文案会变、字段不会）
        let s = Status::parse(REAL).expect("应能解析");
        assert_eq!(s.kind, "video");
        assert!(s.path.contains("test-clip.mp4"), "{:?}", s.path);
        assert_eq!((s.decode_w, s.decode_h), (1920, 1080), "解码尺寸要读出来");
    }

    #[test]
    fn scene_status_is_not_none() {
        // 自研内核的 scene 路径（path 是壁纸目录、不是文件）。
        // 这条守的是一个真实踩过的坑：scene 在播时被报成 `none`（"内核空闲"），
        // 于是界面上一边在播场景壁纸、一边写着内核没干活。
        let json = r#"{"ok":true,"kind":"scene","path":"/home/user/.local/share/Steam/steamapps/workshop/content/431960/1689515439","playing":true}"#;
        let s = Status::parse(json).expect("应能解析");
        assert_eq!(s.kind, "scene");
        assert_ne!(s.kind, "none", "scene 在播时不能报成 none");
        assert!(s.playing, "playing 要读出来");
    }

    #[test]
    fn parses_set_reply_into_effective_settings() {
        // 真实内核回执（照抄 003 的 Set 分支）
        let reply = r#"{"ok":true,"fps":15,"sleep":"maximized","scale":"logical","effects":"none"}"#;
        let (fps, scale, effects, sleep) = perf_from_reply(reply).expect("应能解析");
        assert_eq!((fps, scale.as_str(), effects.as_str(), sleep.as_str()), (15, "logical", "none", "maximized"));
        // 失败回执不能当成"生效了"（成功假象的解药）
        assert!(perf_from_reply(r#"{"ok":false,"error":"未知命令 set"}"#).is_none());
        assert!(perf_from_reply(r#"{"ok":true}"#).is_none(), "缺字段 ⇒ 拿不到四项");
    }

    #[test]
    fn sock_path_uses_xdg_runtime_dir() {
        // 只验证拼装规则（不依赖当前环境真的存在）
        let p = sock_path();
        assert!(p.to_string_lossy().ends_with("we-native.sock"), "{p:?}");
    }

    /// 请求行的**逐字节**格式（含最容易踩的那个坑）：
    /// 拼控制面请求行时若把"转义过的 JSON"（`{\"cmd\":…`）原样抄进 format 串，送出去的行里
    /// 就有真反斜杠 ⇒ 内核 `find("\"cmd\"")` 找不到键 ⇒ 被当**未知命令静默忽略**
    /// （回执照回、画面不动 —— 典型"成功假象"）。所以这里不仅钉内容，还钉"不许有反斜杠"。
    /// 顺手把 `set(fit)` / `set(color)` 两条一起钉住（它们同属"键找不到就静默忽略"这一类）。
    #[test]
    fn mouse_request_line_is_exact_and_backslash_free() {
        assert_eq!(
            cmd_mouse(0.5, 0.25),
            r#"{"cmd":"mouse","x":0.5000,"y":0.2500}"#
        );
        // 口径与内核一致：屏幕归一化 [0,1]，越界在这里就夹住（内核也会夹，两道都留着）
        assert_eq!(cmd_mouse(2.0, -1.0), r#"{"cmd":"mouse","x":1.0000,"y":0.0000}"#);
        assert_eq!(cmd_mouse(0.0, 1.0), r#"{"cmd":"mouse","x":0.0000,"y":1.0000}"#);
        for line in [
            cmd_mouse(0.5, 0.5),
            cmd_set_fit("cover"),
            cmd_set_color("1,1,1,0,0,1"),
        ] {
            assert!(line.contains("\"cmd\""), "行里找不到 \"cmd\" 键：{line}");
            assert!(!line.contains('\\'), "行里不该有反斜杠：{line}");
        }
    }

    /// ⭐ 内核新加的指针诊断字段（照抄实测 `we-ctl status`）：
    /// 界面要能把"来源/计数"如实显示出来 —— `mouse_events==0` 并不等于"合成器不发"，
    /// 只有 `pointer_events==0` 才是"一个原始事件都没来"。
    #[test]
    fn parses_pointer_diagnostics_from_real_status() {
        let j = r#"{"ok":true,"kind":"scene","path":"/w/2564205376","playing":true,"mouse":"0.9500,0.5500","mouse_src":"control","mouse_events":0,"pointer_events":0,"mouse_enter":0,"mouse_leave":0,"mouse_motion":0,"mouse_input":true,"pointer_bound":true}"#;
        let s = Status::parse(j).expect("应能解析");
        assert_eq!(s.mouse, "0.9500,0.5500");
        assert_eq!(s.mouse_src, "control");
        assert_eq!((s.mouse_events, s.pointer_events), (0, 0));
        assert!(s.pointer_bound);
        assert!(pointer_summary(&s).contains("控制面命令"));
        // 合成器真的派发过（10/13 22:15:11 实测：1 个 enter + 位置 0.2107,0.9659）
        let real = r#"{"ok":true,"mouse":"0.2107,0.9659","mouse_src":"wl_pointer","mouse_events":1,"pointer_events":1,"mouse_enter":1,"mouse_leave":0,"mouse_motion":0,"pointer_bound":true}"#;
        let s2 = Status::parse(real).expect("应能解析");
        let line = pointer_summary(&s2);
        assert!(line.contains("wl_pointer") && line.contains("0.2107,0.9659"), "{line}");
        // 老内核（没有这些字段）⇒ 空/0，且不 panic
        let old = Status::parse(REAL).expect("应能解析");
        assert!(old.mouse.is_empty() && old.mouse_src.is_empty());
        assert_eq!(old.pointer_events, 0);
        assert!(pointer_summary(&old).contains("没拿到 wl_pointer"));
        // 拿到了 wl_pointer 但还没动过 ⇒ 必须与"没拿到"分开说
        let idle = r#"{"ok":true,"mouse_src":"none","pointer_events":0,"pointer_bound":true}"#;
        let s3 = Status::parse(idle).expect("应能解析");
        assert!(pointer_summary(&s3).contains("还没有任何指针输入"));
    }
}


// ── 拖动节流 / 合并队列（色盘拖动把内核打满的那一条）───────────────

#[cfg(test)]
mod throttle_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::sync::Mutex as StdMutex;

    fn sp(v: &str) -> String {
        cmd_set_prop("text5", v)
    }

    /// 出站队列的**时间线**测试共用一个全局 `FORCE_NOW`（`flush_now` 是进程级），
    /// 并行跑会互相把"强制放行"打开 ⇒ 用一把锁把这几条串起来（只影响测试）。
    static Q_LOCK: StdMutex<()> = StdMutex::new(());

    #[test]
    fn coalesce_key_splits_drag_streams_from_ordinary_commands() {
        // 单条属性 ⇒ 有键（可合并）
        assert_eq!(coalesce_key(&sp("0.1 0.2 0.3")).as_deref(), Some("sp:text5"));
        // 画面色彩 ⇒ 有键
        assert_eq!(
            coalesce_key(&cmd_set_color("1 1 1 0 0 1")).as_deref(),
            Some("color")
        );
        // 其它命令一律不合并（顺序必须保留）
        for line in [
            cmd_status(),
            cmd_pause(),
            cmd_set_props(&[("a".into(), "1".into()), ("b".into(), "2".into())], false),
            r#"{"cmd":"apply","kind":"scene","path":"/w/x"}"#.to_string(),
            "not json".to_string(),
        ] {
            assert_eq!(coalesce_key(&line), None, "不该合并：{line}");
        }
        assert!(is_status_line(&cmd_status()));
        assert!(!is_status_line(&sp("1 1 1")));
    }

    /// 两个新旋钮（播放速度 / 场景音频音量）的请求行**精确**（无反斜杠、值是数字），
    /// 且各用**自己的合并键** —— 拖速度不能把音量那条也吃掉。
    #[test]
    fn time_scale_and_volume_lines_are_exact_and_coalesce_per_knob() {
        assert_eq!(cmd_set_time_scale(1.25), r#"{"cmd":"set","time_scale":1.250}"#);
        assert_eq!(cmd_set_volume(45.0), r#"{"cmd":"set","volume":45.0}"#);
        // 值是 JSON 数字（不是字符串）：与 fps 同一个教训
        assert!(!cmd_set_time_scale(1.25).contains("\"1.250\""));
        assert!(!cmd_set_volume(45.0).contains('\\'), "不能带反斜杠");

        assert_eq!(
            coalesce_key(&cmd_set_time_scale(1.25)).as_deref(),
            Some("time_scale")
        );
        assert_eq!(coalesce_key(&cmd_set_volume(45.0)).as_deref(), Some("volume"));

        // 越界夹取（内核还会再夹一次；这里先夹住，省得发一条必被改写的请求）
        assert_eq!(cmd_set_time_scale(9.0), r#"{"cmd":"set","time_scale":3.000}"#);
        assert_eq!(cmd_set_time_scale(0.0), r#"{"cmd":"set","time_scale":0.250}"#);
        assert_eq!(cmd_set_volume(120.0), r#"{"cmd":"set","volume":100.0}"#);
        assert_eq!(cmd_set_volume(-5.0), r#"{"cmd":"set","volume":0.0}"#);

        // 两个旋钮互不覆盖；性能四项那条 set 仍不可合并（顺序必须保留）
        let mut q = OutQueue::new();
        q.push(cmd_set_time_scale(1.5));
        q.push(cmd_set_volume(50.0));
        assert_eq!(q.len(), 2, "两个不同旋钮各留一条");
        assert_eq!(
            coalesce_key(&cmd_set(60, "physical", "all", "fullscreen")),
            None
        );
    }

    /// **节流实测**：模拟一次 3 秒拖动（每 10 ms 一个值 = 3 秒 300 条），
    /// 用 `pop_due` 的时间线数**真发出去**多少条 ⇒ 必须 ≤ 20 次/秒。
    /// 这条钉的是用户 的验收线「两个滑杆节流 ≤20Hz」：内核每条 `set` 都会走
    /// 控制面工人线程 + （音量）起一次 pactl，打满就是"拖动时 UI/内核一起卡"。
    #[test]
    fn drag_throttle_stays_at_or_below_20hz() {
        let _q = Q_LOCK.lock().unwrap();
        for name in ["time_scale", "volume"] {
            // 先清掉可能被别的用例（`release_flushes_*`）置上的 FORCE_NOW，否则这一趟会全放行
            let mut warm = OutQueue::new();
            warm.push(cmd_set_volume(1.0));
            let _ = warm.pop_due(Instant::now());
            let t0 = Instant::now();
            let mut q = OutQueue::new();
            let mut sent = 0usize;
            for i in 0..300u64 {
                let t = t0 + Duration::from_millis(i * 10);
                // 每条都是新值（拖动就是这样），不会被"同值"优化掉
                let v = 0.25 + (i % 50) as f32 * 0.05;
                q.push(match name {
                    "time_scale" => cmd_set_time_scale(v),
                    _ => cmd_set_volume(v * 20.0),
                });
                while q.pop_due(t).is_some() {
                    sent += 1;
                }
            }
            let secs = 299.0 / 100.0; // 最后一次推入到第一次的时间跨度
            let hz = sent as f32 / secs;
            assert!(hz <= 20.0, "{name}: 实测 {hz:.1} Hz（{sent} 条 / {secs:.2}s）超过 20Hz");
            assert!(sent >= 3, "{name}: 100 ms~55 ms 窗口下 3 秒至少要发几条，实测 {sent}");
            // 终值不丢：队列里还躺着最后那个值，松手立刻能发出去
            flush_now();
            let last_sent = q.pop_due(t0 + Duration::from_secs(3)).expect("终值必须还在");
            assert_eq!(last_sent, last_of(&name), "{name}: 终值必须是最后一次拖到的值");
        }
    }

    /// 拖动模拟里"最后推入的值"（与上面的循环同口径，单列出来免得抄错）
    fn last_of(name: &str) -> String {
        let i = 299u64;
        let v = 0.25 + (i % 50) as f32 * 0.05;
        match name {
            "time_scale" => cmd_set_time_scale(v),
            _ => cmd_set_volume(v * 20.0),
        }
    }

    /// 同一属性的 60 帧只留最新一条；不同属性互不影响；顺序命令位置不变。
    #[test]
    fn queue_keeps_only_the_newest_value_per_property_and_keeps_order() {
        let _q = Q_LOCK.lock().unwrap();
        let mut q = OutQueue::new();
        for i in 0..60 {
            q.push(sp(&format!("0.{} 0 0", i)));
        }
        assert_eq!(q.len(), 1, "同一属性 60 条必须合并成 1 条");
        // 另一条属性 + 一条顺序命令
        q.push(cmd_set_prop("text", "hello"));
        q.push(cmd_status());
        assert_eq!(q.len(), 3);
        let t0 = Instant::now();
        // 顺序命令排最后 ⇒ 前面两条（合并类）先出，且第一条立刻发（last_sent=None）
        assert_eq!(q.pop_due(t0).unwrap(), sp("0.59 0 0"));
        // 第二条合并类要等节流窗口
        assert!(q.pop_due(t0).is_none(), "55 ms 窗口内不许发第二条");
        assert!(q.pop_due(t0 + prop_interval()).is_some());
        // 顺序命令从不等待
        assert_eq!(q.pop_due(t0 + prop_interval()).unwrap(), cmd_status());
        assert!(q.is_empty());
    }

    /// 松手：`flush_now` 必须把终值立刻放出去（不等节流窗口）。
    #[test]
    fn release_flushes_the_final_value_immediately() {
        let _q = Q_LOCK.lock().unwrap();
        let mut q = OutQueue::new();
        q.push(sp("0 0 0"));
        let t0 = Instant::now();
        assert!(q.pop_due(t0).is_some());
        q.push(sp("0.5 0.5 0.5"));
        assert!(q.pop_due(t0).is_none(), "节流窗口内本该等");
        flush_now();
        let got = q.pop_due(t0).expect("松手必须立刻放行");
        assert!(got.contains("0.5 0.5 0.5"), "{got}");
    }

    /// 假内核：真 unix socket，每条请求先睡 `latency` 再回，记录收到的行与时刻。
    fn fake_kernel(
        sock: std::path::PathBuf,
        latency: Duration,
    ) -> Arc<StdMutex<Vec<(String, Instant)>>> {
        let _ = std::fs::remove_file(&sock);
        let listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind");
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let seen2 = seen.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(c) = conn else { break };
                let mut w = c.try_clone().expect("clone");
                let mut rdr = BufReader::new(c);
                let mut line = String::new();
                loop {
                    line.clear();
                    match rdr.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    seen2
                        .lock()
                        .unwrap()
                        .push((line.trim().to_string(), Instant::now()));
                    std::thread::sleep(latency);
                    let _ = writeln!(w, "{{\"ok\":true,\"props\":{{}}}}");
                    let _ = w.flush();
                }
            }
        });
        seen
    }

    fn sock_in_tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("wphub-a2-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d.join("k.sock")
    }

    fn blocking_call(sock: &std::path::Path, line: &str) -> String {
        use std::os::unix::net::UnixStream;
        let Ok(mut s) = UnixStream::connect(sock) else {
            return "{\"ok\":false}".into();
        };
        let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
        if s.write_all(line.as_bytes()).is_err() || s.write_all(b"\n").is_err() {
            return "{\"ok\":false}".into();
        }
        let mut buf = String::new();
        let _ = BufReader::new(&s).read_line(&mut buf);
        buf.trim().to_string()
    }

    /// **验收（硬指标）**：拖 3 秒 × 60 Hz（180 个值）⇒ 内核收到的 `set-prop` ≤ 60 条，
    /// 且**最后一条就是松手那个终值**；拖拽中途插一条无关命令（`status`）必须
    /// 在**一次内核往返**内得到服务（旧的无界 FIFO 下它会排到几十秒之后）。
///
    /// 假内核的延迟取实测值 **600 ms/条**（本机 set-prop 中位数 620 ms，因为内核每条都
    /// 重建整个场景）—— 这正是"拖动把内核打满"的物理原因。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn three_second_drag_sends_at_most_60_set_props_and_keeps_the_final_value() {
        let latency = Duration::from_millis(600);
        let sock = sock_in_tmp("slow");
        let seen = fake_kernel(sock.clone(), latency);

        let q: SharedQueue = Arc::new(Mutex::new(OutQueue::new()));
        let notify = Arc::new(tokio::sync::Notify::new());
        {
            let (q, n, s) = (q.clone(), notify.clone(), sock.clone());
            tokio::spawn(async move {
                drive(
                    q,
                    n,
                    None,
                    None,
                    |line| {
                        let s = s.clone();
                        async move {
                            tokio::task::spawn_blocking(move || blocking_call(&s, &line))
                                .await
                                .unwrap_or_default()
                        }
                    },
                    |_l, _r| async {},
                )
                .await;
            });
        }

        let mut pushed: Vec<String> = Vec::new();
        let t0 = Instant::now();
        let mut probe_pushed: Option<Instant> = None;
        let mut probe_seen: Option<Instant> = None;
        for i in 0..180u32 {
            // 一次"拖动"里的值：RGB 各分量都随帧变（和色盘/滑块一样）
            let v = format!("{:.3} {:.3} {:.3}", i as f32 / 180.0, 0.5, 1.0 - i as f32 / 180.0);
            let line = sp(&v);
            pushed.push(v);
            {
                let mut g = q.lock().unwrap();
                g.push(line);
            }
            notify.notify_one();
            // 拖到一半插一条无关命令：量它从入队到被内核收到花了多久
            if i == 90 {
                let mut g = q.lock().unwrap();
                g.push(cmd_status());
                drop(g);
                probe_pushed = Some(Instant::now());
            }
            tokio::time::sleep(Duration::from_micros(16_667)).await;
        }
        let drag_ms = t0.elapsed().as_millis();
        // 松手：终值 + 立刻放行（= 界面那条 Message::ColorDragEnd）
        let final_v = "0.111 0.222 0.333".to_string();
        {
            let mut g = q.lock().unwrap();
            g.push(sp(&final_v));
        }
        flush_now();
        let t_release = Instant::now();

        // 等到终值真的被内核收到（最多 3 s）
        loop {
            let hit = {
                let g = seen.lock().unwrap();
                g.iter().any(|(l, _)| l.contains(&final_v))
            };
            if hit || t_release.elapsed() > Duration::from_secs(3) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        let g = seen.lock().unwrap();
        // ① 拖拽窗口内（t0 .. t0+3s+1 次往返）内核收到的 set-prop 条数
        let in_window = g
            .iter()
            .filter(|(l, at)| {
                l.contains("\"set-prop\"") && *at >= t0 && *at <= t0 + Duration::from_millis(3_000)
            })
            .count();
        let total_sp = g.iter().filter(|(l, _)| l.contains("\"set-prop\"")).count();
        // ② 终值误差
        let last_sp = g
            .iter()
            .rev()
            .find(|(l, _)| l.contains("\"set-prop\""))
            .map(|(l, _)| l.clone())
            .unwrap_or_default();
        // ③ 无关命令的等待（中途插入的那条 status）
        probe_seen = g
            .iter()
            .find(|(l, _)| is_status_line(l))
            .map(|(_, at)| *at);
        drop(g);

        // 终值解析：从请求行里取出三个分量，跟界面最后发的那条比
        let val_of = |line: &str| -> Vec<i32> {
            let v: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
            let s = v
                .get("props")
                .and_then(|p| p.get("text5"))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            s.split_whitespace()
                .filter_map(|t| t.parse::<f32>().ok())
                .map(|x| (x * 255.0).round() as i32)
                .collect()
        };
        let want = val_of(&sp(&final_v));
        let got = val_of(&last_sp);
        let err_255 = want
            .iter()
            .zip(got.iter())
            .map(|(a, b)| (a - b).abs())
            .max()
            .unwrap_or(255);

        let probe_wait = match (probe_pushed, probe_seen) {
            (Some(a), Some(b)) => b.saturating_duration_since(a),
            _ => Duration::from_secs(99),
        };
        println!(
            "[A2 验收·慢内核 600ms/条] 拖动 {drag_ms} ms / 180 帧 ⇒ 窗口内 set-prop {in_window} 条（另加终值 1 条，共 {total_sp}）；终值误差 {err_255}/255；中途插入的 status 等了 {:.1} ms",
            probe_wait.as_secs_f64() * 1000.0
        );

        assert!(
            in_window <= 60,
            "拖拽 3 秒内核收到 {in_window} 条 set-prop，超过 60 条"
        );
        assert_eq!(last_sp, sp(&final_v), "最后一条必须是松手终值：{last_sp}");
        assert!(err_255 <= 1, "终值误差 {err_255}/255 > 1");
        assert!(
            probe_wait < latency * 2,
            "无关命令等了 {probe_wait:?}（> 2 次内核往返）⇒ 队列还在饿死别的操作"
        );
    }

    /// 同一段代码在**快内核**（1 ms）下也不能超过 60 条：证明上限是**节流**给的，
    /// 不是"内核慢所以碰巧少发"。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fast_kernel_still_caps_the_drag_at_60() {
        let sock = sock_in_tmp("fast");
        let seen = fake_kernel(sock.clone(), Duration::from_millis(1));
        let q: SharedQueue = Arc::new(Mutex::new(OutQueue::new()));
        let notify = Arc::new(tokio::sync::Notify::new());
        {
            let (q, n, s) = (q.clone(), notify.clone(), sock.clone());
            tokio::spawn(async move {
                drive(
                    q,
                    n,
                    None,
                    None,
                    |line| {
                        let s = s.clone();
                        async move {
                            tokio::task::spawn_blocking(move || blocking_call(&s, &line))
                                .await
                                .unwrap_or_default()
                        }
                    },
                    |_l, _r| async {},
                )
                .await;
            });
        }
        let t0 = Instant::now();
        for i in 0..180u32 {
            let line = sp(&format!("{i} 0 0"));
            {
                let mut g = q.lock().unwrap();
                g.push(line);
            }
            notify.notify_one();
            tokio::time::sleep(Duration::from_micros(16_667)).await;
        }
        {
            let mut g = q.lock().unwrap();
            g.push(sp("9 9 9"));
        }
        flush_now();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let n = {
            let g = seen.lock().unwrap();
            g.iter()
                .filter(|(l, at)| l.contains("\"set-prop\"") && *at <= t0 + Duration::from_millis(3_100))
                .count()
        };
        let hz = n as f64 / 3.0;
        println!("[A2 验收·快内核 1ms/条] 3 秒 180 帧 ⇒ set-prop {n} 条（{hz:.1}/s）");
        assert!(n <= 60, "快内核下 3 秒发了 {n} 条（> 60）");
        assert!(hz >= 10.0, "预览率太低：{hz:.1}/s（应 ≥ 10，目标 15~20）");
    }
}
