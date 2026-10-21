//! **PipeWire/PulseAudio sink 枚举与选择**（音频频谱的"跟哪一路输出"）。
//!
//! ## 为什么需要它
//! 频谱采集原来是**只跟默认 sink**：内核 `parec --device <默认 sink>.monitor`。
//! 而默认 sink 会随插拔变化（HDMI ⇄ USB 声卡），更麻烦的是用户可以把声音**只**放到
//! 另一个非默认输出（或多路同时）—— 那时分析听不到任何声音，粒子的音频响应就"死"了。
//!
//! ## 三层分工（本文件只管第 ①③ 层，切换动作在 main.rs 的 `Cmd::Audio` 处理里）
//! ① **枚举**：`pactl list sinks`（长格式，带人类可读的 `Description`）→ `Vec<Sink>`；
//! 拿默认 sink 用 `pactl info` 的 `Default Sink:` 行。没有 `pactl`（或它报错）⇒
//! `enumerate` 回 `Err(人话原因)` —— **优雅降级**：调用方如实报"无法枚举"，不崩、不假装。
//! ② **解析**：`parse_sinks` / `parse_sinks_short` / `parse_default_sink` 是纯函数，
//! 空输出、缺字段、残块、非 UTF-8 垃圾都当"没有/跳过"，绝不 panic。
//! ③ **决策**：`plan_sink(spec, sinks, enum_err)` —— 把 `id` / `名字` / `default` / `all`
//! 四种写法翻译成 [`SinkSel`]。纯函数 ⇒ 状态机可单测（含"枚举失败"那条诚实降级分支）。
//!
//! ## `all` 为什么不支持
//! `parec` 一次只能钉**一个** monitor（渲染器的采集线程也是"一个子进程 + 一条 stdout"）。
//! 想同时听多路要么起 N 个 `parec` 再各自 FFT 合并（要动渲染器采集线程），要么让用户用
//! `pactl load-module module-combine-sink` 自己合成一路 —— 两者都不该由内核偷偷做。
//! 所以 `sink=all` **如实报不支持**，不假装成功。

use std::process::Command;

/// 一个可用 sink（人类可读描述来自 `pactl list sinks` 的 `Description:` 行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sink {
    pub id: u32,
    /// PulseAudio/PipeWire 的 sink 节点名（如 `alsa_output.pci-0000_09_00.1.hdmi-stereo`）
    pub name: String,
    /// 人类可读描述（如 `Renoir/Cezanne HDMI/DP Audio Controller 数字立体声 (HDMI)`）
    pub desc: String,
    /// 对应的采集源（monitor）：`<name>.monitor` —— 就是喂给 `parec --device` 的那个值
    pub monitor: String,
    /// 是不是系统当前默认输出
    pub is_default: bool,
}

/// `sink=all` 的固定回复（渲染器一次只钉一个 monitor，见文件头注释）。
pub const ALL_UNSUPPORTED: &str = "sink=all 暂不支持：本实现一次只跟一个 sink 的 monitor（PulseAudio/PipeWire 的 parec 只能钉一个源），\
想同时听多路请先用 `pactl load-module module-combine-sink` 合成一路，再在它上面新建的 monitor 上选";

/// 想跟哪一路输出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkSel {
    /// **跟随系统默认**：不设 `WE_AUDIO_DEVICE`，内核每次（重）起采集都用 `pactl info` 现查的默认 sink
    /// 这正是"插拔后跟着变"的行为，所以默认档才是那个语义。
    Default,
    /// 指定 sink 的**节点名**（不含 `.monitor`；采集设备 = `<name>.monitor`）。
    Named(String),
}

impl SinkSel {
    /// 状态里给人看/给界面比对的标签
    pub fn label(&self) -> String {
        match self {
            SinkSel::Default => "default".to_string(),
            SinkSel::Named(n) => n.clone(),
        }
    }

    /// 要写进 `WE_AUDIO_DEVICE` 的值。
    /// `Default` ⇒ 空串（渲染器 `.filter(!trim.is_empty)` 会把它当"没设"⇒ 走 `pactl info` 现查默认）。
    pub fn device_env(&self) -> String {
        match self {
            SinkSel::Default => String::new(),
            SinkSel::Named(n) => format!("{n}.monitor"),
        }
    }
}

impl Default for SinkSel {
    fn default() -> Self {
        SinkSel::Default
    }
}

/// **纯函数**：解析 `pactl list sinks` 的长格式输出。
///
/// 形态（实测本机 PipeWire 1.6.9）：
/// ```text
/// Sink #593
/// State: IDLE
/// Name: alsa_output.pci-0000_09_00.1.hdmi-stereo
/// Description: Renoir/Cezanne HDMI/DP Audio Controller 数字立体声 (HDMI) [SG27QC]
/// Driver: PipeWire
/// Monitor Source: alsa_output.pci-0000_09_00.1.hdmi-stereo.monitor
/// ...
/// device.description = "..." ← 属性行**不算** Description
/// ```
/// 规则：`Sink #<id>` 起一块；块内取 `Name:` / `Description:` / `Monitor Source:`。
/// 没有名字的残块直接丢；缺 `Description` 用名字兜底；缺 `Monitor Source` 用 `<name>.monitor` 兜底。
/// 空输入 ⇒ 空 vec（不 panic）。
pub fn parse_sinks(text: &str) -> Vec<Sink> {
    let mut out: Vec<Sink> = Vec::new();
    let mut cur: Option<Sink> = None;
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("Sink #") {
            if let Some(s) = cur.take() {
                out.push(s);
            }
            let id = rest
                .trim()
                .split_whitespace()
                .next()
                .and_then(|x| x.parse::<u32>().ok())
                .unwrap_or(0);
            cur = Some(Sink {
                id,
                name: String::new(),
                desc: String::new(),
                monitor: String::new(),
                is_default: false,
            });
            continue;
        }
        let Some(s) = cur.as_mut() else { continue };
        // 属性行是更深缩进且形如 `device.description = "..."`，前缀不匹配这两个词。
        if let Some(v) = t.strip_prefix("Name:") {
            let v = v.trim();
            if !v.is_empty() {
                s.name = v.to_string();
            }
        } else if let Some(v) = t.strip_prefix("Description:") {
            s.desc = v.trim().to_string();
        } else if let Some(v) = t.strip_prefix("Monitor Source:") {
            s.monitor = v.trim().to_string();
        }
    }
    if let Some(s) = cur.take() {
        out.push(s);
    }
    out.retain(|s| !s.name.is_empty());
    for s in out.iter_mut() {
        if s.monitor.is_empty() {
            s.monitor = format!("{}.monitor", s.name);
        }
        if s.desc.is_empty() {
            s.desc = s.name.clone();
        }
    }
    out
}

/// **纯函数**：解析 `pactl list short sinks`（制表符分隔：`id\t名字\t驱动\tspec\t状态`）。
/// 有些精简 build 只给短列表（没有 `Description`）⇒ 描述用名字兜底。
/// 少于两列 / id 不成数字 / 名字为空 ⇒ 跳过那一行。
pub fn parse_sinks_short(text: &str) -> Vec<Sink> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.trim().is_empty() {
            continue;
        }
        let mut fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 2 {
            // 兼容用空格分隔的版本
            fields = line.split_whitespace().collect();
        }
        if fields.len() < 2 {
            continue;
        }
        let Ok(id) = fields[0].trim().parse::<u32>() else {
            continue;
        };
        let name = fields[1].trim();
        if name.is_empty() {
            continue;
        }
        out.push(Sink {
            id,
            name: name.to_string(),
            desc: name.to_string(),
            monitor: format!("{name}.monitor"),
            is_default: false,
        });
    }
    out
}

/// **纯函数**：从 `pactl info` 的输出里取 `Default Sink:` 那一行。
pub fn parse_default_sink(text: &str) -> Option<String> {
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("Default Sink:") {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// 查系统当前默认 sink（`pactl info`）。拿不到 ⇒ `None`（不是错误：调用方会退回 `@DEFAULT_MONITOR@`）。
pub fn default_sink() -> Option<String> {
    let out = Command::new("pactl").args(["info"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_default_sink(&String::from_utf8_lossy(&out.stdout))
}

// ── 音乐音量（内核自己的输出 / 系统默认输出）───────────────────────────────────
// 口径（用户 的要求）：内核**有音频输出**（壁纸自己的 sound 对象经
// `pw-play`/`pw-cat` 播到默认输出）⇒ 调那一路的 sink volume；**没有**也照样调
// 系统默认 sink —— 壁纸的音乐就在那条 sink 上。两种情形在这里是**同一条**命令：
// `pactl set-sink-volume <默认 sink> <百分比>`（`@DEFAULT_SINK@` 也行，但先解析成
// 节点名再调，回执里就能给出**具体是哪一路**，不糊成一句"成功了"）。
//
// 只动音量，不动静音位（`set-sink-mute` 不碰）—— "把音量拖到 0" 与"用户自己静音"是两件事。

/// **纯函数**：从 `pactl get-sink-volume` 的输出里取第一个百分比。
///
/// 形态（实测本机 PipeWire 1.6.9）：
/// `Volume: front-left: 26241 / 40% / -23.85 dB, front-right: 26241 / 40% / -23.85 dB`
/// ⇒ 取 `front-left` 那一项的 `40`。多行/残行一律跳过，取不到 ⇒ `None`。
pub fn parse_volume_pct(text: &str) -> Option<f32> {
    for line in text.lines() {
        let l = line.trim_start();
        let Some(rest) = l.strip_prefix("Volume:") else {
            continue;
        };
        // 找形如 `NN%` 的第一处
        let bytes: Vec<char> = rest.chars().collect();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i].is_ascii_digit() {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == '.') {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == '%' {
                    let s: String = bytes[start..i].iter().collect();
                    if let Ok(v) = s.parse::<f32>() {
                        return Some(v);
                    }
                    return None;
                }
            } else {
                i += 1;
            }
        }
    }
    None
}

/// 读系统默认 sink 的音量（0..100）。拿不到 ⇒ `Err(人话原因)`。
pub fn default_volume() -> Result<(String, f32), String> {
    let sink = default_sink().unwrap_or_else(|| "@DEFAULT_SINK@".to_string());
    let out = Command::new("pactl")
        .args(["get-sink-volume", &sink])
        .output()
        .map_err(|e| format!("pactl 起不来：{e}"))?;
    if !out.status.success() {
        return Err(format!(
            "pactl get-sink-volume {sink} 失败：{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let v = parse_volume_pct(&text).ok_or_else(|| format!("读不懂 pactl 的音量输出：{}", text.trim()))?;
    Ok((sink, v))
}

/// 设系统默认 sink 的音量（0..100，夹取）。成功 ⇒ `Ok((sink 名, 回读的实际百分比))`。
///
/// **回读**是刻意的：`pactl` 会按步长量化（本机 65536 级的 `40%` ⇒ 26214），回执里给
/// 实际值，界面/验收看到的就是内核真正做出来的那个数。
pub fn set_default_volume(percent: f32) -> Result<(String, f32), String> {
    let pct = if percent.is_finite() {
        percent.clamp(0.0, 100.0)
    } else {
        return Err("音量必须是 0..100 的数字".into());
    };
    let sink = default_sink().unwrap_or_else(|| "@DEFAULT_SINK@".to_string());
    let out = Command::new("pactl")
        .args(["set-sink-volume", &sink, &format!("{pct:.0}%")])
        .output()
        .map_err(|e| format!("pactl 起不来：{e}"))?;
    if !out.status.success() {
        return Err(format!(
            "pactl set-sink-volume {sink} {pct:.0}% 失败：{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let (sink, actual) = default_volume()?;
    Ok((sink, actual))
}

/// `pactl` 的百分比是**每声道**的；这里给单测用的样本文本。
#[cfg(test)]
pub(crate) const VOLUME_SAMPLE: &str =
    "Volume: front-left: 26241 /  40% / -23.85 dB,   front-right: 26241 /  40% / -23.85 dB\n        balance 0.00\n";

/// **枚举可用 sink**。
///
/// 先 `pactl list sinks`（长格式，带描述）；它没给出任何块时退回 `list short sinks`。
/// 两次都空 / 没有 pactl / pactl 报错 ⇒ `Err(<人话原因>)`（**优雅降级**，不 panic）。
/// 成功时会把默认 sink 标出来。
pub fn enumerate() -> Result<Vec<Sink>, String> {
    let long = Command::new("pactl").args(["list", "sinks"]).output();
    let mut sinks = match long {
        Ok(out) if out.status.success() => {
            parse_sinks(&String::from_utf8_lossy(&out.stdout))
        }
        Ok(out) => {
            // pactl 在、但这一条失败了：仍然试一次短列表（可能是权限/版本差异）
            eprintln!(
                "[音频 sink] `pactl list sinks` 退出码 {:?}，改用短列表试试",
                out.status.code()
            );
            Vec::new()
        }
        Err(e) => {
            return Err(format!(
                "无法枚举 sink：pactl 起不来（{e}）—— 本机没装 PulseAudio/PipeWire 客户端工具？"
            ))
        }
    };
    if sinks.is_empty() {
        if let Ok(out) = Command::new("pactl").args(["list", "short", "sinks"]).output() {
            if out.status.success() {
                sinks = parse_sinks_short(&String::from_utf8_lossy(&out.stdout));
            }
        }
    }
    if sinks.is_empty() {
        return Err("无法枚举 sink：pactl 报不出任何输出设备（list sinks 与 list short sinks 都是空）"
            .to_string());
    }
    if let Some(def) = default_sink() {
        for s in sinks.iter_mut() {
            s.is_default = s.name == def;
        }
    }
    Ok(sinks)
}

/// **纯函数：切换 sink 的状态机**。把用户的 `<id|名字|default|all>` 翻译成 [`SinkSel`]。
///
/// `sinks` 是已枚举的列表；`enum_err` = `Some(原因)` 表示**枚举失败**（此时 `sinks` 必为空）。
///
/// 各写法的语义与失败口径：
///
/// * `""` / `default` ⇒ [`SinkSel::Default`]（跟随系统默认，永远可行）
///
/// * `all` ⇒ `Err(ALL_UNSUPPORTED)`（**如实报不支持**）
///
/// * 纯数字 ⇒ 按 id 在列表里找；枚举失败 ⇒ 报"无法按 id 解析，请给名字"，**不瞎猜**
///
/// * 其它 ⇒ 当 sink 名（可带 `.monitor` 后缀，会剥掉）：
/// 枚举成功：必须在列表里（精确匹配），否则报错
/// 枚举失败：**诚实降级** —— 直接按这个名字用（内核空转了一个错名字的 monitor 也不会装作成功：
/// `status.audio` 会把实际设备名原样打出来，一眼看得出）
pub fn plan_sink(spec: &str, sinks: &[Sink], enum_err: Option<&str>) -> Result<SinkSel, String> {
    let s = spec.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("default") {
        return Ok(SinkSel::Default);
    }
    if s.eq_ignore_ascii_case("all") {
        return Err(ALL_UNSUPPORTED.to_string());
    }
    // 允许直接写 monitor 源名（`xxx.monitor`）—— 用户从 `pactl` 里抄的多半就是它
    let name_spec = s.strip_suffix(".monitor").unwrap_or(s);
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
        if let Some(reason) = enum_err {
            return Err(format!("{reason}；无法按 id 解析，请给 sink 名或 default"));
        }
        let Ok(id) = s.parse::<u32>() else {
            return Err(format!("id 超出范围：{s}"));
        };
        return match sinks.iter().find(|x| x.id == id) {
            Some(x) => Ok(SinkSel::Named(x.name.clone())),
            None => Err(format!("没有 id={id} 的 sink（`we-ctl audio sinks` 看列表）")),
        };
    }
    if enum_err.is_some() {
        // 枚举失败：按名字直接用（不假装成功 —— status 里会打出真实设备名）
        return Ok(SinkSel::Named(name_spec.to_string()));
    }
    match sinks.iter().find(|x| x.name == name_spec) {
        Some(x) => Ok(SinkSel::Named(x.name.clone())),
        None => Err(format!(
            "没有名为 {name_spec} 的 sink（`we-ctl audio sinks` 看列表）"
        )),
    }
}

/// 一行状态：`sink=<标签> monitor=<实际设备>`（给 `status.audio` 用）。
pub fn status_line(sel: &SinkSel, note: Option<&str>) -> String {
    let mut out = format!("sink={} device={}", sel.label(), sel.device_env());
    if sel.device_env().is_empty() {
        out = format!("sink={} device=<系统默认>", sel.label());
    }
    if let Some(n) = note {
        out.push_str(&format!(" note={n}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 实测本机 `pactl list sinks` 的形态（含属性行里的 `device.description`，不能误当 Description）
    const LONG: &str = "Sink #593\n\
\tState: IDLE\n\
\tName: alsa_output.pci-0000_09_00.1.hdmi-stereo\n\
\tDescription: Renoir/Cezanne HDMI/DP Audio Controller 数字立体声 (HDMI) [SG27QC]\n\
\tDriver: PipeWire\n\
\tSample Specification: s32le 2ch 48000Hz\n\
\tMonitor Source: alsa_output.pci-0000_09_00.1.hdmi-stereo.monitor\n\
\tProperties:\n\
\t\tdevice.description = \"Renoir/Cezanne HDMI/DP Audio Controller\"\n\
\t\tnode.name = \"alsa_output.pci-0000_09_00.1.hdmi-stereo\"\n\
Sink #700\n\
\tState: RUNNING\n\
\tName: whtest\n\
\tDescription: WHTEST 虚拟输出\n\
\tDriver: PipeWire\n\
\tProperties:\n\
\t\tdevice.description = \"逗号, 冒号: 花括号{} 都不能带偏\"\n";

    #[test]
    fn parses_two_sinks_with_description_and_monitor() {
        let v = parse_sinks(LONG);
        assert_eq!(v.len(), 2, "两块都要在：{v:?}");
        assert_eq!(v[0].id, 593);
        assert_eq!(v[0].name, "alsa_output.pci-0000_09_00.1.hdmi-stereo");
        assert_eq!(
            v[0].desc,
            "Renoir/Cezanne HDMI/DP Audio Controller 数字立体声 (HDMI) [SG27QC]"
        );
        assert_eq!(
            v[0].monitor,
            "alsa_output.pci-0000_09_00.1.hdmi-stereo.monitor"
        );
        assert_eq!(v[1].id, 700);
        assert_eq!(v[1].name, "whtest");
        assert_eq!(v[1].desc, "WHTEST 虚拟输出");
        // 短格式没给 Monitor Source ⇒ 用名字兜底
        assert_eq!(v[1].monitor, "whtest.monitor");
        assert!(v.iter().all(|s| !s.is_default), "默认标记由 enumerate 填");
    }

    #[test]
    fn empty_and_garbage_inputs_yield_nothing() {
        assert!(parse_sinks("").is_empty());
        assert!(parse_sinks("\n\n   \n").is_empty());
        assert!(parse_sinks("Server String: /run/user/1000/pulse/native").is_empty());
        // 残块：有 `Sink #` 但没 Name ⇒ 丢掉（不许出一个空名字的 sink）
        assert!(parse_sinks("Sink #1\n\tState: IDLE\n").is_empty());
        // 名字行是空的 ⇒ 也丢
        assert!(parse_sinks("Sink #1\n\tName:\n").is_empty());
    }

    #[test]
    fn description_falls_back_to_name_and_survives_weird_id() {
        let v = parse_sinks("Sink #abc\n\tName: foo\n");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id, 0, "怪异 id 当 0，不 panic");
        assert_eq!(v[0].desc, "foo", "没描述就用名字");
    }

    #[test]
    fn parses_short_format_tabs_and_spaces() {
        let tabbed = "593\talsa_output.pci-0000_09_00.1.hdmi-stereo\tPipeWire\ts32le 2ch 48000Hz\tIDLE\n\
                      701\twhtest\tPipeWire\tfloat32le 2ch 48000Hz\tSUSPENDED\n";
        let v = parse_sinks_short(tabbed);
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].id, v[0].name.as_str()), (593, "alsa_output.pci-0000_09_00.1.hdmi-stereo"));
        assert_eq!(v[1].monitor, "whtest.monitor");
        // 空格分隔的老版式
        let spaced = "12\tdev\nbad-line-no-id\n42\tfoo\n";
        let v2 = parse_sinks_short(spaced);
        assert_eq!(v2.len(), 2, "没有 id 的行要跳过：{v2:?}");
        // 空/垃圾
        assert!(parse_sinks_short("").is_empty());
        assert!(parse_sinks_short("not a sink line").is_empty());
    }

    #[test]
    fn parses_default_sink_line() {
        let info = "Server String: /run/user/1000/pulse/native\n\
                    Server Name: PulseAudio (on PipeWire 1.6.9)\n\
                    Default Sink: alsa_output.pci-0000_09_00.1.hdmi-stereo\n\
                    Default Source: alsa_output.pci-0000_09_00.1.hdmi-stereo.monitor\n";
        assert_eq!(
            parse_default_sink(info).as_deref(),
            Some("alsa_output.pci-0000_09_00.1.hdmi-stereo")
        );
        // 没有这一行 / 空值 ⇒ None
        assert_eq!(parse_default_sink("Default Source: x.monitor\n"), None);
        assert_eq!(parse_default_sink("Default Sink:   \n"), None);
        assert_eq!(parse_default_sink(""), None);
    }

    // ── 切换状态机 ──────────────────────────────────────────────────────────
    fn two_sinks() -> Vec<Sink> {
        parse_sinks(LONG)
    }

    #[test]
    fn plan_default_and_empty_mean_follow_system() {
        let s = two_sinks();
        assert_eq!(plan_sink("", &s, None).unwrap(), SinkSel::Default);
        assert_eq!(plan_sink("default", &s, None).unwrap(), SinkSel::Default);
        assert_eq!(plan_sink("  DEFAULT ", &s, None).unwrap(), SinkSel::Default);
        assert_eq!(SinkSel::Default.device_env(), "", "默认档必须让内核现查");
        assert_eq!(SinkSel::Default.label(), "default");
    }

    #[test]
    fn plan_by_id_and_by_name() {
        let s = two_sinks();
        assert_eq!(
            plan_sink("700", &s, None).unwrap(),
            SinkSel::Named("whtest".into())
        );
        assert_eq!(
            plan_sink("whtest", &s, None).unwrap(),
            SinkSel::Named("whtest".into())
        );
        // 直接写 monitor 源名也认（剥后缀）
        assert_eq!(
            plan_sink("whtest.monitor", &s, None).unwrap(),
            SinkSel::Named("whtest".into())
        );
        assert_eq!(
            SinkSel::Named("whtest".into()).device_env(),
            "whtest.monitor"
        );
        assert_eq!(SinkSel::Named("whtest".into()).label(), "whtest");
    }

    #[test]
    fn plan_rejects_unknown_id_and_name() {
        let s = two_sinks();
        let e = plan_sink("999", &s, None).unwrap_err();
        assert!(e.contains("id=999"), "{e}");
        let e = plan_sink("nope", &s, None).unwrap_err();
        assert!(e.contains("nope"), "{e}");
        // 名字匹配是**精确**的：前缀/子串不算（否则会挑错设备）
        assert!(plan_sink("whtes", &s, None).is_err());
    }

    #[test]
    fn plan_all_is_honestly_unsupported() {
        let s = two_sinks();
        for spec in ["all", "ALL", " All "] {
            let e = plan_sink(spec, &s, None).unwrap_err();
            assert!(e.contains("不支持"), "必须如实报不支持：{e}");
            assert_eq!(e, ALL_UNSUPPORTED);
        }
    }

    #[test]
    fn plan_degrades_honestly_when_enumeration_fails() {
        let reason = "无法枚举 sink：pactl 起不来";
        // ① default 永远可用（枚举无关）
        assert_eq!(plan_sink("default", &[], Some(reason)).unwrap(), SinkSel::Default);
        // ② all 仍然如实报不支持
        assert_eq!(
            plan_sink("all", &[], Some(reason)).unwrap_err(),
            ALL_UNSUPPORTED
        );
        // ③ 按 id：没法解析 ⇒ 明确让用户改用名字，**不瞎猜 id 对应的名字**
        let e = plan_sink("700", &[], Some(reason)).unwrap_err();
        assert!(e.contains(reason) && e.contains("id"), "{e}");
        // ④ 按名字：诚实降级 —— 直接按名字用（status 会打出真实设备名）
        assert_eq!(
            plan_sink("whtest", &[], Some(reason)).unwrap(),
            SinkSel::Named("whtest".into())
        );
        assert_eq!(
            plan_sink("whtest.monitor", &[], Some(reason)).unwrap(),
            SinkSel::Named("whtest".into())
        );
    }

    /// 音乐音量：从 `pactl get-sink-volume` 的真实输出里取第一个百分比。
    #[test]
    fn parse_volume_picks_the_first_percentage() {
        assert_eq!(parse_volume_pct(VOLUME_SAMPLE), Some(40.0));
        assert_eq!(parse_volume_pct("Volume: mono: 65536 / 100% / 0.00 dB\n"), Some(100.0));
        assert_eq!(parse_volume_pct("Volume: front-left: 1 / 0% / -inf dB\n"), Some(0.0));
        // 小数（`pactl` 在 65536 级量化下会打印 33.33% 之类）
        assert_eq!(parse_volume_pct("Volume: mono: 21845 / 33.33% / -9.54 dB\n"), Some(33.33));
        // 不是音量输出 / 空 ⇒ None（不瞎猜 0 —— 0 是个合法音量，混了就分不清"静音"和"没读到"）
        assert_eq!(parse_volume_pct("Failure: No such entity\n"), None);
        assert_eq!(parse_volume_pct(""), None);
    }

    /// 音量夹取：0..100，非法值如实报错（不悄悄变成 0）。
    #[test]
    fn volume_range_is_clamped_and_nan_is_rejected() {
        // 夹取只影响发给 pactl 的字符串；这里测纯逻辑（不真跑 pactl：会改本机音量）
        let clamp = |v: f32| v.clamp(0.0, 100.0);
        assert_eq!(clamp(-5.0), 0.0);
        assert_eq!(clamp(120.0), 100.0);
        assert_eq!(clamp(40.0), 40.0);
        assert!(set_default_volume(f32::NAN).is_err(), "NaN 必须报错");
    }

    #[test]
    fn status_line_shows_the_selected_sink() {
        let a = status_line(&SinkSel::Named("whtest".into()), None);
        assert!(a.contains("sink=whtest"), "{a}");
        assert!(a.contains("device=whtest.monitor"), "{a}");
        let b = status_line(&SinkSel::Default, None);
        assert!(b.contains("sink=default"), "{b}");
        assert!(b.contains("系统默认"), "{b}");
        let c = status_line(&SinkSel::Named("whtest".into()), Some("枚举失败，按名字直接用"));
        assert!(c.contains("note=枚举失败"), "{c}");
    }
}
