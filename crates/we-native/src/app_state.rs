// SPDX-License-Identifier: MIT
//
//! app 存档恢复：内核重启后把「运行时下发的属性覆盖表」与「逐场景帧率」找回来。
//!
//! ## 为什么需要它（本模块存在的唯一理由）
//!
//! 属性覆盖表（`set-prop` / `WE_PROP_*`）与帧率（`{"cmd":"set","fps":N}`）都是**内核运行时状态**：
//! 只在内存里，进程一重启就没了。而为了上屏渲染器修复，内核被**反复重装/重启**；每重启一次，
//! 桌面就退回**作者默认属性** + drop-in 的默认帧率 —— 用户看到的就是错的
//! （实测例子：X-Ray 场景的 `xrayversion` 回到作者默认 `false` ⇒ "X-Ray 消失了"）。
//!
//! 界面（shell）本来就有这两份**事实存档**，它连上内核时会把"正在播那张"的整套属性与帧率
//! 补发一次。但那只是"补救"：界面不一定在跑，也不一定在重启那一刻就连上。
//! 本模块让**内核自己**在启动 / 每次成功 apply 一个场景后，按存档把这两样写回运行时。
//!
//! ## 数据来源（app 写的，内核**只读**，一个字都不往里写）
//!
//! * `~/.local/share/wallpaper-hub/prop_overrides.json` —— `{ "<壁纸id>": { "<属性名>": 值 } }`
//!
//! * `~/.local/share/wallpaper-hub/scene_fps.json` —— `{ "<壁纸id>": fps }`
//! 路径与界面侧 `shell/src/app.rs` 的 `data_dir` **同一条规则**（`$HOME/.local/share/wallpaper-hub`），
//! 键与界面侧 `local.rs` 的 id 同一条规则（**壁纸内容目录名**，见 [`scene_id_from_path`]）。
//!
//! ## 三条硬规矩
//!
//! 1. **只针对当前场景**：存档里别的场景的记录一个都不许搬进来。
//!
//! 2. **不做判断，只搬运**：属性名内核本来就不认识（定义在壁纸自己的 `project.json`），
//! 原样搬进覆盖表；帧率表里没有这条场景 ⇒ 用 [`FPS_DEFAULT`]（60，**与界面侧
//! `config::SCENE_FPS_DEFAULT` 一致** —— 恢复出来的必须和"app 补发时会发什么"完全相同）。
//!
//! 3. **绝不 panic、绝不阻塞启动**：文件不存在 / 打不开 / 坏 JSON / 超大 ⇒ 跳过 + 一行日志。
//!
//! ## 开关
//! `WE_RESTORE_APP_STATE=0` ⇒ **关**（默认开）。关的时候本模块**不打开任何文件**、不产生任何动作、
//! 不打印任何东西 ⇒ 与"没有这段代码"的旧内核逐字节相同（见 `off_switch_is_byte_identical_to_old_behaviour`）。
//!
//! ## 与 app 不打架
//! 恢复只在**启动**与**每次成功 apply 一个场景**时各做一次；**没有任何定时器 / 轮询**。
//! app 随时可以重发（它优先）：它发晚了就覆盖我们恢复的值，值本身也来自同一份存档 ⇒ 不会来回抖。

use std::path::{Path, PathBuf};

/// 逐场景帧率表里没有这条场景（或表不可用）时的帧率。
///
/// **必须与界面侧 `shell/src/config.rs::SCENE_FPS_DEFAULT` 一致**：恢复的目标是
/// "app 连上会补发的那份状态"，两边数值不同就会出现"重启前后桌面帧率不一样"。
pub const FPS_DEFAULT: u64 = 60;
/// 帧率夹取范围（与界面侧 `SCENE_FPS_MIN/MAX` 一致）：存档里的怪值不该把内核带飞。
pub const FPS_MIN: u64 = 1;
pub const FPS_MAX: u64 = 240;
/// 单份存档的读取上限：坏文件/巨型文件不许拖住启动（超过就当"损坏"跳过）。
const MAX_ARCHIVE_BYTES: u64 = 4 * 1024 * 1024;

// ── 开关 ──────────────────────────────────────────────────────────────────────

/// `WE_RESTORE_APP_STATE=0` ⇒ 关。其它值（含未设置、空串）⇒ 开。
///
/// 单独抽一个 `_from` 版本是为了单测：测试不许去改进程环境变量（并发跑的单测会互相踩）。
pub fn restore_enabled_from(v: Option<&str>) -> bool {
    !matches!(v.map(|s| s.trim()), Some("0"))
}

/// 进程环境里的开关状态。
pub fn restore_enabled() -> bool {
    restore_enabled_from(std::env::var("WE_RESTORE_APP_STATE").ok().as_deref())
}

// ── 存档位置 ──────────────────────────────────────────────────────────────────

/// app 数据目录：`$HOME/.local/share/wallpaper-hub`（与界面侧 `data_dir` 同一条规则）。
pub fn data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".local/share/wallpaper-hub")
}

pub fn props_path_in(dir: &Path) -> PathBuf {
    dir.join("prop_overrides.json")
}

pub fn fps_path_in(dir: &Path) -> PathBuf {
    dir.join("scene_fps.json")
}

// ── 场景 id ───────────────────────────────────────────────────────────────────

/// 场景 id = **壁纸内容目录名**。
///
/// 内核从控制面只拿到一个路径（`apply` 的 `path`），而界面侧的 id 就是那个目录名：
/// 工坊条目是 `…/workshop/content/431960/<id>`（`wp/native.rs::entry_dir`），
/// 本地壁纸是 `wp/local.rs::entry_from_dir` 的 `dir.file_name`。两边同一条规则 ⇒
/// `scene_fps.json` / `prop_overrides.json` 的键能直接对上。
pub fn scene_id_from_path(path: &str) -> Option<String> {
    let p = path.trim();
    if p.is_empty() {
        return None;
    }
    let name = Path::new(p).file_name()?.to_string_lossy().trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

// ── 极简 JSON（只为这两份存档）────────────────────────────────────────────────
// 内核 crate 刻意**不加 serde_json 依赖**（依赖全在离线缓存里、且本次只许动 `src/**`），
// 而这两份文件的形状非常窄：顶层对象 + 一层（props 是两层）标量。
// 所以这里自带一个小而**严格**的解析器：解析失败 = 文件损坏 ⇒ 跳过（不猜、不半懂半搬）。

/// 解析出来的 JSON 值。数字/true/false/null 保留**原文**（`1.0` 不会被我格式化成 `1`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Jval {
    Str(String),
    /// 数字 / `true` / `false` / `null` 的原文
    Scalar(String),
    Obj(Vec<(String, Jval)>),
    Arr(Vec<Jval>),
}

/// 解析一整份 JSON（**尾部不许有垃圾**；失败 ⇒ `None`）。
pub fn parse_json(src: &str) -> Option<Jval> {
    let b = src.as_bytes();
    let mut i = 0usize;
    let v = parse_value(b, &mut i)?;
    skip_ws(b, &mut i);
    if i != b.len() {
        return None;
    }
    Some(v)
}

fn skip_ws(b: &[u8], i: &mut usize) {
    while *i < b.len() && b[*i].is_ascii_whitespace() {
        *i += 1;
    }
}

fn parse_value(b: &[u8], i: &mut usize) -> Option<Jval> {
    skip_ws(b, i);
    match *b.get(*i)? {
        b'{' => parse_obj(b, i),
        b'[' => parse_arr(b, i),
        b'"' => parse_string(b, i).map(Jval::Str),
        b't' => parse_lit(b, i, "true").map(|()| Jval::Scalar("true".into())),
        b'f' => parse_lit(b, i, "false").map(|()| Jval::Scalar("false".into())),
        b'n' => parse_lit(b, i, "null").map(|()| Jval::Scalar("null".into())),
        _ => parse_number(b, i),
    }
}

fn parse_lit(b: &[u8], i: &mut usize, lit: &str) -> Option<()> {
    if b.len() >= *i + lit.len() && &b[*i..*i + lit.len()] == lit.as_bytes() {
        *i += lit.len();
        Some(())
    } else {
        None
    }
}

fn parse_number(b: &[u8], i: &mut usize) -> Option<Jval> {
    let start = *i;
    let mut j = *i;
    if b.get(j) == Some(&b'-') || b.get(j) == Some(&b'+') {
        j += 1;
    }
    let d0 = j;
    while j < b.len() && b[j].is_ascii_digit() {
        j += 1;
    }
    if j == d0 {
        return None;
    }
    if b.get(j) == Some(&b'.') {
        j += 1;
        let d1 = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j == d1 {
            return None;
        }
    }
    if matches!(b.get(j), Some(b'e') | Some(b'E')) {
        j += 1;
        if matches!(b.get(j), Some(b'+') | Some(b'-')) {
            j += 1;
        }
        let d2 = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j == d2 {
            return None;
        }
    }
    *i = j;
    Some(Jval::Scalar(String::from_utf8_lossy(&b[start..j]).into_owned()))
}

fn parse_string(b: &[u8], i: &mut usize) -> Option<String> {
    if b.get(*i) != Some(&b'"') {
        return None;
    }
    *i += 1;
    let mut out = String::new();
    while *i < b.len() {
        match b[*i] {
            b'"' => {
                *i += 1;
                return Some(out);
            }
            b'\\' => {
                *i += 1;
                let e = *b.get(*i)?;
                match e {
                    b'n' => out.push('\n'),
                    b't' => out.push('\t'),
                    b'r' => out.push('\r'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'u' => {
                        let hx = std::str::from_utf8(b.get(*i + 1..*i + 5)?).ok()?;
                        let cp = u32::from_str_radix(hx, 16).ok()?;
                        out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                        *i += 4;
                    }
                    // 非法转义 ⇒ 整份当损坏（不半懂半搬）
                    _ => return None,
                }
                *i += 1;
            }
            _ => {
                let ch = std::str::from_utf8(b.get(*i..)?).ok()?.chars().next()?;
                out.push(ch);
                *i += ch.len_utf8();
            }
        }
    }
    None
}

fn parse_obj(b: &[u8], i: &mut usize) -> Option<Jval> {
    if b.get(*i) != Some(&b'{') {
        return None;
    }
    *i += 1;
    let mut out: Vec<(String, Jval)> = Vec::new();
    skip_ws(b, i);
    if b.get(*i) == Some(&b'}') {
        *i += 1;
        return Some(Jval::Obj(out));
    }
    loop {
        skip_ws(b, i);
        let k = parse_string(b, i)?;
        skip_ws(b, i);
        if b.get(*i) != Some(&b':') {
            return None;
        }
        *i += 1;
        let v = parse_value(b, i)?;
        out.push((k, v));
        skip_ws(b, i);
        match b.get(*i) {
            Some(&b',') => *i += 1,
            Some(&b'}') => {
                *i += 1;
                return Some(Jval::Obj(out));
            }
            _ => return None,
        }
    }
}

fn parse_arr(b: &[u8], i: &mut usize) -> Option<Jval> {
    if b.get(*i) != Some(&b'[') {
        return None;
    }
    *i += 1;
    let mut out: Vec<Jval> = Vec::new();
    skip_ws(b, i);
    if b.get(*i) == Some(&b']') {
        *i += 1;
        return Some(Jval::Arr(out));
    }
    loop {
        let v = parse_value(b, i)?;
        out.push(v);
        skip_ws(b, i);
        match b.get(*i) {
            Some(&b',') => *i += 1,
            Some(&b']') => {
                *i += 1;
                return Some(Jval::Arr(out));
            }
            _ => return None,
        }
    }
}

/// JSON 值 → 字符串，口径与界面侧 `native::value_str`（`serde_json::Value::to_string`）一致：
/// 字符串原样、布尔 `true`/`false`、数字保留原文（`1.0` 就是 `1.0`）、`null` ⇒ 空串。
/// 对象/数组会序列化成紧凑 JSON 文本（WE 属性不可能是这种，但**不丢数据**）。
fn value_str(v: &Jval) -> String {
    match v {
        Jval::Str(s) => s.clone(),
        Jval::Scalar(s) if s == "null" => String::new(),
        Jval::Scalar(s) => s.clone(),
        other => to_json_text(other),
    }
}

fn to_json_text(v: &Jval) -> String {
    match v {
        Jval::Str(s) => {
            let mut out = String::from("\"");
            for ch in s.chars() {
                match ch {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\t' => out.push_str("\\t"),
                    '\r' => out.push_str("\\r"),
                    c => out.push(c),
                }
            }
            out.push('"');
            out
        }
        Jval::Scalar(s) => s.clone(),
        Jval::Arr(items) => {
            let body: Vec<String> = items.iter().map(to_json_text).collect();
            format!("[{}]", body.join(","))
        }
        Jval::Obj(items) => {
            let body: Vec<String> = items
                .iter()
                .map(|(k, x)| format!("{}:{}", to_json_text(&Jval::Str(k.clone())), to_json_text(x)))
                .collect();
            format!("{{{}}}", body.join(","))
        }
    }
}

// ── 两份存档的读取 ────────────────────────────────────────────────────────────

/// 从 `prop_overrides.json` 原文里取**这一个场景**的属性覆盖（顺序 = 文件里的顺序）。
///
/// * `Err(原因)` ⇒ 文件坏了（调用方只记日志、**一个字都不动覆盖表**）
///
/// * `Ok(vec![])` ⇒ 文件是好的、但这条场景没有记录
///
/// * 属性名与值都原样透传（**内核不认识任何一个属性名**）
pub fn props_for_scene(src: &str, scene: &str) -> Result<Vec<(String, String)>, String> {
    let root = parse_json(src).ok_or_else(|| "JSON 解析失败".to_string())?;
    let Jval::Obj(entries) = root else {
        return Err("顶层不是对象".to_string());
    };
    for (k, v) in entries {
        if k != scene {
            continue; // 别的场景一条都不搬
        }
        return match v {
            Jval::Obj(props) => Ok(props
                .iter()
                .map(|(k, v)| (k.clone(), value_str(v)))
                .collect()),
            // 键在、值不是对象 ⇒ 当"没记过"（不猜）
            _ => Ok(Vec::new()),
        };
    }
    Ok(Vec::new())
}

/// 从 `scene_fps.json` 原文里取这条场景的帧率。
///
/// * `Err(原因)` ⇒ 文件坏了
///
/// * `Ok(None)` ⇒ 文件是好的、但这条场景没有记录（或记录不是个正整数）
pub fn fps_for_scene(src: &str, scene: &str) -> Result<Option<u64>, String> {
    let root = parse_json(src).ok_or_else(|| "JSON 解析失败".to_string())?;
    let Jval::Obj(entries) = root else {
        return Err("顶层不是对象".to_string());
    };
    for (k, v) in entries {
        if k != scene {
            continue; // 只认这条场景
        }
        let raw = match v {
            Jval::Scalar(s) => s,
            Jval::Str(s) => s,
            _ => return Ok(None),
        };
        return Ok(raw.trim().parse::<u64>().ok());
    }
    Ok(None)
}

/// 一份存档的可读状态：原文 / 不存在 / 损坏。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Src {
    Text(String),
    Missing,
    Broken,
}

impl Src {
    pub fn label(&self) -> &'static str {
        match self {
            Src::Text(_) => "可用",
            Src::Missing => "不存在",
            Src::Broken => "损坏",
        }
    }
}

/// 读一份存档。**任何失败都只是"这一类"结果，绝不 panic、绝不阻塞**（有大小上限）。
pub fn read_src(p: &Path) -> Src {
    match std::fs::metadata(p) {
        // 只在非 Windows 上 `is_file` 才可靠；这里用 metadata 顺手把"目录"挡掉
        Ok(m) if m.is_file() && m.len() > MAX_ARCHIVE_BYTES => Src::Broken,
        Ok(m) if m.is_file() => match std::fs::read_to_string(p) {
            Ok(s) => Src::Text(s),
            Err(_) => Src::Broken,
        },
        Ok(_) => Src::Broken,      // 存在但不是文件（目录/设备）
        Err(_) => Src::Missing,    // 不存在 / 无权限 / 路径坏 —— 一律按"没有存档"跳过
    }
}

// ── 恢复计划 ──────────────────────────────────────────────────────────────────

/// 帧率值的来源（日志里"来源="后面那个词，必须写清楚）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FpsSrc {
    /// 逐场景表里有这条场景的记录
    Table,
    /// 表里没有 ⇒ 用缺省 60（与界面侧 `SCENE_FPS_DEFAULT` 一致）
    Default,
}

impl FpsSrc {
    pub fn label(self) -> &'static str {
        match self {
            FpsSrc::Table => "app 存档",
            FpsSrc::Default => "app 存档缺省",
        }
    }
}

/// 一次恢复要做的动作（**纯数据**，便于"逐字节"比对与追踪打印）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// 先清空整张覆盖表（换场景时防止上一条的属性名串过来 —— 界面侧补发也是 `clear=true`）
    ClearProps,
    SetProp(String, String),
    SetFps(u64),
}

/// 恢复计划：**只针对一个场景**。
///
/// 两份存档各自独立决定：
///
/// * `props = Some(v)`（含 `v` 为空）⇒ 存档可用 ⇒ **覆盖表 = 这条场景的存档内容**（先清表）；
/// `props = None` ⇒ 存档不可用（不存在/损坏）⇒ **覆盖表一个字都不动**。
///
/// * `fps = Some(v)` ⇒ 存档可用 ⇒ 帧率设成 `v`（表里没记过就是 [`FPS_DEFAULT`]）；
/// `fps = None` ⇒ 存档不可用 ⇒ **帧率一个字都不动**（drop-in 的 `WE_SCENE_FPS` 照旧生效）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub scene: String,
    pub props: Option<Vec<(String, String)>>,
    pub fps: Option<u64>,
    /// 只有 `fps.is_some` 时有意义
    pub fps_src: FpsSrc,
    /// 日志备注（哪份存档不可用）。空 = 两份都正常。
    pub notes: Vec<String>,
}

impl Plan {
    pub fn props_count(&self) -> usize {
        self.props.as_ref().map(|p| p.len()).unwrap_or(0)
    }

    pub fn actions(&self) -> Vec<Action> {
        let mut out = Vec::new();
        if let Some(props) = &self.props {
            // 只要有**可用**的 props 存档就先清表：这条场景没记过 ⇒ 覆盖表就该是空的。
            // 与界面侧补发（`clear=true` + 整套属性）逐条对应。
            out.push(Action::ClearProps);
            for (k, v) in props {
                out.push(Action::SetProp(k.clone(), v.clone()));
            }
        }
        if let Some(fps) = self.fps {
            out.push(Action::SetFps(fps));
        }
        out
    }

    /// 启动/apply 时那行证据。
    pub fn log_line(&self, why: &str) -> String {
        let props = if self.props.is_some() {
            format!("属性={} 条", self.props_count())
        } else {
            "属性=不动".to_string()
        };
        let fps = match self.fps {
            Some(f) => format!("fps={f}（来源={}）", self.fps_src.label()),
            None => "fps=不动".to_string(),
        };
        let mut s = format!("[启动恢复] 场景={} {props} {fps}", self.scene);
        if !self.notes.is_empty() {
            s.push_str(&format!(" · 备注={}", self.notes.join("；")));
        }
        s.push_str(&format!(" · 触发={why}"));
        s
    }
}

/// 动作序列的**稳定文本**：「逐字节旧行为」的比对就比它。
///
/// 空串 = 一个动作都没有 = **旧内核**（那段代码根本不存在）对运行时的影响。
/// 开关关 / 跳过 / 没有当前场景 ⇒ 必须恒为空串 —— 这条不变量由单测钉住。
pub fn actions_bytes(actions: &[Action]) -> String {
    actions
        .iter()
        .map(|a| match a {
            Action::ClearProps => "clear".to_string(),
            Action::SetProp(k, v) => format!("set:{k}={v}"),
            Action::SetFps(f) => format!("fps={f}"),
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// 恢复决策。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 开关关（或没有当前场景）⇒ **一个字都不动、一个字都不打**（逐字节旧行为）
    Off,
    /// 存档不可用 ⇒ 跳过，但留一行日志（要求：静默跳过 + 一行日志）
    Skip { scene: String, reason: String },
    Apply(Plan),
}

impl Decision {
    /// 这次决策要落地的动作（`Off` / `Skip` ⇒ 空 = 零影响）。
    pub fn actions(&self) -> Vec<Action> {
        match self {
            Decision::Off | Decision::Skip { .. } => Vec::new(),
            Decision::Apply(p) => p.actions(),
        }
    }

    /// 要打印的那一行。`Off` ⇒ `None`（关的时候连日志都不打，才对得起"逐字节旧行为"）。
    pub fn log_line(&self, why: &str) -> Option<String> {
        match self {
            Decision::Off => None,
            Decision::Skip { scene, reason } => {
                Some(format!("[启动恢复] 场景={scene} ⇒ 跳过：{reason} · 触发={why}"))
            }
            Decision::Apply(p) => Some(p.log_line(why)),
        }
    }
}

/// 决策（**纯函数**：不碰磁盘、不读环境）。
///
/// * `enabled=false` ⇒ [`Decision::Off`]（调用方连文件都不该读 —— 见 [`decide_in`]）
///
/// * `scene` 空 ⇒ [`Decision::Off`]（没有当前场景，没什么可恢复的）
///
/// * 两份存档都不可用（不存在/读不了/坏 JSON） ⇒ [`Decision::Skip`]：**一个字都不改**
/// （drop-in 的 `WE_SCENE_FPS` 与环境变量照旧生效），只留一行日志
///
/// * 单份不可用 ⇒ 那一份**一个字都不动**，另一份正常恢复（互不牵连）
pub fn decide(enabled: bool, scene: &str, props: Src, fps: Src) -> Decision {
    if !enabled || scene.trim().is_empty() {
        return Decision::Off;
    }
    // 每份存档**独立**判"可用"：读不到 / 坏 JSON ⇒ 这一份不可用（另一份照常用）
    let props_res: Result<Vec<(String, String)>, &'static str> = match &props {
        Src::Text(s) => props_for_scene(s, scene).map_err(|_| "损坏"),
        other => Err(other.label()),
    };
    let fps_res: Result<Option<u64>, &'static str> = match &fps {
        Src::Text(s) => fps_for_scene(s, scene).map_err(|_| "损坏"),
        other => Err(other.label()),
    };
    let props_err: Option<&'static str> = props_res.as_ref().err().copied();
    let fps_err: Option<&'static str> = fps_res.as_ref().err().copied();
    let entries = match props_res {
        Ok(v) => Some(v),
        Err(_) => None,
    };
    let (rate, rate_src) = match fps_res {
        // 与界面侧 `config::fps_for_scene` 同一条判据：`f > 0` 才算"记过"
        Ok(Some(v)) if v > 0 => (Some(v.clamp(FPS_MIN, FPS_MAX)), FpsSrc::Table),
        Ok(_) => (Some(FPS_DEFAULT), FpsSrc::Default),
        Err(_) => (None, FpsSrc::Default),
    };
    let mut notes: Vec<String> = Vec::new();
    if let Some(why) = props_err {
        notes.push(format!(
            "prop_overrides.json {why} ⇒ 属性覆盖表一个字都不动"
        ));
    }
    if let Some(why) = fps_err {
        notes.push(format!("scene_fps.json {why} ⇒ 帧率一个字都不动"));
    }
    if entries.is_none() && rate.is_none() {
        // 两份都读不了 ⇒ 整体跳过（不改任何运行时状态，只留一行日志）
        return Decision::Skip {
            scene: scene.trim().to_string(),
            reason: format!(
                "app 存档不可用（prop_overrides.json={}，scene_fps.json={}）",
                props_err.unwrap_or("损坏"),
                fps_err.unwrap_or("损坏")
            ),
        };
    }
    Decision::Apply(Plan {
        scene: scene.trim().to_string(),
        props: entries,
        fps: rate,
        fps_src: rate_src,
        notes,
    })
}

/// 读盘 + 决策（`decide_in` 的便利包装）。`enabled=false` 时**立刻返回**，一个文件都不打开。
pub fn decide_in(dir: &Path, enabled: bool, scene: &str) -> Decision {
    if !enabled || scene.trim().is_empty() {
        return Decision::Off;
    }
    let props = read_src(&props_path_in(dir));
    let fps = read_src(&fps_path_in(dir));
    decide(enabled, scene, props, fps)
}

/// 用内核实际的数据目录（`$HOME/.local/share/wallpaper-hub`）决策。
pub fn decide_here(enabled: bool, scene: &str) -> Decision {
    decide_in(&data_dir(), enabled, scene)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实的 app 存档片段（**逐字**取自 `~/.local/share/wallpaper-hub/*.json` 的形状：
    /// serde_json 的 `to_string_pretty` 输出，含缩进、换行、字符串/数字/布尔混排）。
    const PROPS_REAL: &str = r#"{
  "2564205376": {
    "nsfwversion": false,
    "sfwversion": false,
    "xraysize": 0.5,
    "xrayversion": true
  },
  "3704273480": {
    "newproperty1": false,
    "newproperty": 1.0,
    "newproperty2": false,
    "text5": "1 0 1"
  },
  "2887537617": {
    "newproperty": true
  }
}"#;
    const FPS_REAL: &str = r#"{
  "2564205376": 60,
  "3571777169": 30
}"#;

    fn tmpdir(tag: &str) -> PathBuf {
        // Scratch space under target/ so the tree stays clean and `cargo clean` wipes it.
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp/app-state");
        let d = base.join(tag);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("建临时目录");
        d
    }

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).expect("写测试存档");
    }

    // ── 极简 JSON 解析器（它坏了整条恢复链就瞎了）────────────────────────────

    #[test]
    fn json_parser_handles_the_two_archive_shapes() {
        let p = parse_json(PROPS_REAL).expect("props 存档要能解析");
        let Jval::Obj(top) = &p else { panic!("顶层必须是对象") };
        assert_eq!(top.len(), 3);
        assert_eq!(top[0].0, "2564205376");
        let Jval::Obj(inner) = &top[0].1 else { panic!("场景值必须是对象") };
        assert_eq!(
            inner.iter().map(|(k, v)| (k.as_str(), value_str(v))).collect::<Vec<_>>(),
            vec![
                ("nsfwversion", "false".to_string()),
                ("sfwversion", "false".to_string()),
                ("xraysize", "0.5".to_string()),
                ("xrayversion", "true".to_string()),
            ]
        );
        // 数字保留原文：`1.0` 不许被格式化成 `1`（与 serde_json 的输出口径一致）
        let Jval::Obj(inner) = &top[1].1 else { panic!() };
        assert_eq!(value_str(&inner[1].1), "1.0");
        assert_eq!(value_str(&inner[3].1), "1 0 1");
        assert!(parse_json(FPS_REAL).is_some());
    }

    #[test]
    fn json_parser_rejects_garbage() {
        for bad in [
            "",
            "{",
            "{not json",
            "{\"a\":}",
            "{\"a\":1,}",
            "{\"a\" 1}",
            "[1,2",
            "{\"a\":\"\\q\"}",
            "{} trailing",
            "{\"a\":+}",
        ] {
            assert!(parse_json(bad).is_none(), "这种必须当损坏：{bad:?}");
        }
        assert!(parse_json("  \n {\"a\": null} \n ").is_some(), "首尾空白合法");
        assert!(parse_json("{}").is_some());
        assert!(parse_json("[]").is_some());
    }

    // ── 场景 id ──────────────────────────────────────────────────────────────

    #[test]
    fn scene_id_is_the_content_dir_name() {
        assert_eq!(
            scene_id_from_path("/home/user/.local/share/Steam/steamapps/workshop/content/431960/2564205376"),
            Some("2564205376".to_string())
        );
        assert_eq!(scene_id_from_path("/x/y/2564205376/"), Some("2564205376".to_string()));
        assert_eq!(scene_id_from_path("/x/y/summer-beach-3704273480"), Some("summer-beach-3704273480".to_string()));
        assert_eq!(scene_id_from_path(""), None);
        assert_eq!(scene_id_from_path("   "), None);
    }

    // ── 单份存档的取数 ────────────────────────────────────────────────────────

    #[test]
    fn props_only_for_the_current_scene() {
        let a = props_for_scene(PROPS_REAL, "2564205376").unwrap();
        assert_eq!(a.len(), 4);
        assert!(a.contains(&("xrayversion".to_string(), "true".to_string())));
        assert!(a.contains(&("xraysize".to_string(), "0.5".to_string())));
        // 别的场景一条都不许进来（3704273480 的属性名就在文件里）
        assert!(!a.iter().any(|(k, _)| k == "text5" || k == "newproperty"));
        // 文件里没有的场景 ⇒ 空表（不是错误）
        assert!(props_for_scene(PROPS_REAL, "999999").unwrap().is_empty());
    }

    #[test]
    fn fps_only_for_the_current_scene() {
        assert_eq!(fps_for_scene(FPS_REAL, "2564205376").unwrap(), Some(60));
        assert_eq!(fps_for_scene(FPS_REAL, "3571777169").unwrap(), Some(30));
        assert_eq!(fps_for_scene(FPS_REAL, "999999").unwrap(), None);
        assert!(fps_for_scene("{oops", "1").is_err());
    }

    // ── 决策：四种坏情况 ─────────────────────────────────────────────────────

    /// ① 文件不存在 ⇒ 静默跳过 + 一行日志，**一个字都不改**，不 panic。
    #[test]
    fn missing_files_skip_with_one_log_line() {
        let d = tmpdir("missing"); // 空目录（两份文件都不存在）
        let dec = decide_in(&d, true, "2564205376");
        assert!(matches!(dec, Decision::Skip { .. }), "两份都没有 ⇒ 跳过：{dec:?}");
        assert!(
            dec.actions().is_empty() && actions_bytes(&dec.actions()).is_empty(),
            "跳过 ⇒ 零动作、逐字节等于旧内核"
        );
        let line = dec.log_line("内核启动").expect("要留一行日志");
        assert!(line.contains("[启动恢复]"), "{line}");
        assert!(line.contains("跳过"), "{line}");
        assert!(line.contains("不存在"), "{line}");
        // 连目录都不存在也一样（不 panic、不阻塞）
        let dec2 = decide_in(Path::new("/nonexistent-wallpaper-hub-dir"), true, "2564205376");
        assert!(matches!(dec2, Decision::Skip { .. }));
    }

    /// ② 文件损坏 ⇒ 静默跳过 + 一行日志，不 panic。
    #[test]
    fn broken_files_skip_with_one_log_line() {
        let d = tmpdir("broken");
        write(&d, "prop_overrides.json", "{\"2564205376\": { oh no");
        write(&d, "scene_fps.json", "}}}}");
        let dec = decide_in(&d, true, "2564205376");
        assert!(matches!(dec, Decision::Skip { .. }), "两份都坏 ⇒ 跳过：{dec:?}");
        assert!(actions_bytes(&dec.actions()).is_empty(), "跳过 ⇒ 零动作");
        let line = dec.log_line("内核启动").unwrap();
        assert!(line.contains("损坏"), "{line}: {line}");
        // 单份坏：好的那份照样用，坏的那份一个字都不动
        let d2 = tmpdir("broken_props_only");
        write(&d2, "prop_overrides.json", "{ broken");
        write(&d2, "scene_fps.json", FPS_REAL);
        match decide_in(&d2, true, "2564205376") {
            Decision::Apply(p) => {
                assert!(p.props.is_none(), "props 坏了 ⇒ 覆盖表一个字都不动");
                assert_eq!(p.fps, Some(60));
                assert_eq!(p.fps_src, FpsSrc::Table);
                assert!(
                    !p.actions().iter().any(|a| matches!(a, Action::ClearProps)),
                    "存档不可用 ⇒ 不许清表"
                );
                let line = p.log_line("x");
                assert!(line.contains("prop_overrides.json 损坏"), "{line}");
                assert!(
                    !p.actions().iter().any(|a| matches!(a, Action::SetProp(..))),
                    "坏了就不许写任何属性"
                );
            }
            other => panic!("fps 那份是好的 ⇒ 应当恢复 fps：{other:?}"),
        }
        // 反向：fps 坏、props 好
        let d3 = tmpdir("broken_fps_only");
        write(&d3, "prop_overrides.json", PROPS_REAL);
        write(&d3, "scene_fps.json", "not json at all");
        match decide_in(&d3, true, "2564205376") {
            Decision::Apply(p) => {
                assert_eq!(p.props.as_ref().map(|v| v.len()), Some(4));
                assert_eq!(p.fps, None, "fps 存档坏了 ⇒ 帧率一个字都不动（不许瞎猜 60）");
                assert!(!p.actions().iter().any(|a| matches!(a, Action::SetFps(_))));
                assert!(p.log_line("x").contains("scene_fps.json 损坏"));
            }
            other => panic!("props 那份是好的 ⇒ 应当恢复属性：{other:?}"),
        }
    }

    /// ③ 空表 ⇒ 只清空 + 用缺省 60（不猜、不搬别的场景）。
    #[test]
    fn empty_tables_restore_nothing_but_still_clear() {
        let d = tmpdir("empty");
        write(&d, "prop_overrides.json", "{}");
        write(&d, "scene_fps.json", "{}");
        match decide_in(&d, true, "2564205376") {
            Decision::Apply(p) => {
                assert_eq!(p.props.as_deref(), Some(&[][..]), "空表=可用但没记录");
                assert_eq!(p.fps, Some(FPS_DEFAULT));
                assert_eq!(p.fps_src, FpsSrc::Default);
                assert_eq!(p.actions(), vec![Action::ClearProps, Action::SetFps(60)]);
                let line = p.log_line("内核启动");
                assert!(line.contains("属性=0 条"), "{line}");
                assert!(line.contains("fps=60（来源=app 存档缺省）"), "{line}");
            }
            other => panic!("空表也是「可用」的存档 ⇒ Apply：{other:?}"),
        }
        // 表里有别的场景、就是没有当前这条 ⇒ 同样"空"（但**绝不**搬别人的）
        let d2 = tmpdir("other_scene_only");
        write(&d2, "prop_overrides.json", "{\"3704273480\": {\"newproperty\": true}}");
        write(&d2, "scene_fps.json", "{\"3571777169\": 30}");
        match decide_in(&d2, true, "2564205376") {
            Decision::Apply(p) => {
                assert_eq!(p.props_count(), 0, "别的场景的属性一条都不许搬");
                assert_eq!(p.fps, Some(FPS_DEFAULT), "别的场景的帧率不许借用");
            }
            other => panic!("{other:?}"),
        }
    }

    /// ④ 只恢复当前场景（真实存档，两次决策互不影响）。
    #[test]
    fn only_the_current_scene_is_restored() {
        let d = tmpdir("two_scenes");
        write(&d, "prop_overrides.json", PROPS_REAL);
        write(&d, "scene_fps.json", FPS_REAL);
        let a = match decide_in(&d, true, "2564205376") {
            Decision::Apply(p) => p,
            other => panic!("{other:?}"),
        };
        assert_eq!(a.props_count(), 4);
        assert_eq!(a.fps, Some(60));
        assert_eq!(a.fps_src, FpsSrc::Table);
        // 需求里给的那行证据格式，逐字对一遍
        assert!(
            a.log_line("内核启动").starts_with("[启动恢复] 场景=2564205376 属性=4 条 fps=60（来源=app 存档）"),
            "{}",
            a.log_line("内核启动")
        );
        let b = match decide_in(&d, true, "3704273480") {
            Decision::Apply(p) => p,
            other => panic!("{other:?}"),
        };
        assert_eq!(b.props_count(), 4);
        assert!(b.props.as_ref().unwrap().iter().any(|(k, v)| k == "text5" && v == "1 0 1"));
        assert_eq!(
            b.actions().iter().filter(|x| matches!(x, Action::SetProp(..))).count(),
            4
        );
        // 场景 3704273480 的帧率表里没记 ⇒ 60（缺省），不许借用 2564205376 的 60 或 3571777169 的 30 当"表里值"
        assert_eq!(b.fps, Some(60));
        assert_eq!(b.fps_src, FpsSrc::Default, "没记过的场景来源必须写'缺省'");
    }

    /// 帧率夹取：存档里的怪值不许把内核带飞；0/负数 ⇒ 当没记。
    #[test]
    fn fps_values_are_clamped_and_zero_means_unset() {
        let d = tmpdir("fps_clamp");
        write(&d, "prop_overrides.json", "{}");
        for (raw, want, src) in [
            ("999", 240, FpsSrc::Table),
            ("0", 60, FpsSrc::Default),
            ("1", 1, FpsSrc::Table),
            ("30", 30, FpsSrc::Table),
        ] {
            write(&d, "scene_fps.json", &format!("{{\"S\": {raw}}}"));
            match decide_in(&d, true, "S") {
                Decision::Apply(p) => {
                    assert_eq!(p.fps, Some(want), "raw={raw}");
                    assert_eq!(p.fps_src, src, "raw={raw}");
                }
                other => panic!("{other:?}"),
            }
        }
    }

    /// 开关：关的时候**逐字节等于旧行为**（旧行为 = 不存在这段代码 ⇒ 零动作、零影响、零日志）。
    #[test]
    fn off_switch_is_byte_identical_to_old_behaviour() {
        let d = tmpdir("off_switch");
        write(&d, "prop_overrides.json", PROPS_REAL);
        write(&d, "scene_fps.json", FPS_REAL);
        // 先证明这份档案是**真的可用**（否则下面的比较是空对空）
        let on = decide_in(&d, true, "2564205376");
        assert!(matches!(on, Decision::Apply(_)), "开的时候必须真的恢复：{on:?}");
        assert!(
            !actions_bytes(&on.actions()).is_empty(),
            "开的时候动作序列必须非空，比较才有意义"
        );
        // 逐个开关写法：只有 "0" 是关
        let mut on_count = 0;
        for (raw, want_on) in [
            (None, true),
            (Some(""), true),
            (Some("1"), true),
            (Some("0"), false),
            (Some(" 0 "), false),
            (Some("0\n"), false),
        ] {
            let en = restore_enabled_from(raw);
            assert_eq!(en, want_on, "WE_RESTORE_APP_STATE={raw:?}");
            if en {
                on_count += 1;
            }
        }
        assert_eq!(on_count, 3, "只有 \"0\"（含首尾空白）是关");
        // 关 ⇒ Off：零动作 / 零影响 / 零日志
        let off = decide_in(&d, false, "2564205376");
        assert_eq!(off, Decision::Off);
        assert_eq!(
            actions_bytes(&off.actions()),
            "",
            "关的时候对运行时的影响必须与旧内核逐字节相同（零动作）"
        );
        assert_eq!(
            off.actions(),
            Vec::<Action>::new(),
            "旧行为的动作序列就是空的（注册下来，别被以后改格式的人悄悄改掉）"
        );
        assert_eq!(off.log_line("内核启动"), None, "关的时候连日志都不打");
        // 关的时候连数据目录都不需要存在（更没有"打开文件"这一步）
        let off_elsewhere = decide_in(Path::new("/nonexistent-x"), false, "2564205376");
        assert_eq!(off_elsewhere, off);
        // 没有任何当前场景 ⇒ 同样是 Off（旧行为）
        assert_eq!(decide_in(&d, true, ""), Decision::Off);
        assert_eq!(decide(true, "  ", Src::Missing, Src::Missing), Decision::Off);
    }

    /// 决策层不碰渲染器、也不碰磁盘：`decide` 只看传进来的三样东西（纯函数，可随便测）。
    #[test]
    fn decide_is_pure_over_its_inputs() {
        let missing = decide(true, "S", Src::Missing, Src::Text(FPS_REAL.to_string()));
        match missing {
            Decision::Apply(p) => {
                assert!(p.props.is_none(), "props 存档不存在 ⇒ 覆盖表一个字都不动");
                assert_eq!(p.fps, Some(60), "fps 存档可用、S 没记过 ⇒ 缺省 60");
                assert_eq!(p.fps_src, FpsSrc::Default);
                assert!(p.notes.iter().any(|n| n.contains("不存在")), "{:?}", p.notes);
            }
            other => panic!("{other:?}"),
        }
        // 损坏的那份也不许"半懂半搬"
        let broken = decide(
            true,
            "2564205376",
            Src::Text("{oops".to_string()),
            Src::Text(FPS_REAL.to_string()),
        );
        match broken {
            Decision::Apply(p) => {
                assert!(p.props.is_none());
                assert_eq!(p.fps, Some(60));
            }
            other => panic!("{other:?}"),
        }
    }

    /// 对**真实**存档的只读抽查（本机没有 app 存档 ⇒ 直接跳过，绝不 fail）。
    /// 目的：证明这份手写解析器吃得下 app 真正写出来的文件（而不是在测试夹具上自娱自乐），
    /// 并把"内核下次启动会恢复成什么"那行证据提前打出来（`cargo test -- --nocapture` 可见）。
    /// **只读**：这份用例一个字都不往用户的数据目录里写。
    #[test]
    fn live_archive_probe_is_read_only() {
        let dir = data_dir();
        let p_src = read_src(&props_path_in(&dir));
        let f_src = read_src(&fps_path_in(&dir));
        if matches!(p_src, Src::Missing) && matches!(f_src, Src::Missing) {
            eprintln!("[live] 本机没有 app 存档（{}）⇒ 跳过", dir.display());
            return;
        }
        // 真文件必须能被解析器吃下（形状契约；坏了就是契约变了，得知道）
        if let Src::Text(s) = &p_src {
            assert!(parse_json(s).is_some(), "prop_overrides.json 必须是合法 JSON");
        }
        if let Src::Text(s) = &f_src {
            assert!(parse_json(s).is_some(), "scene_fps.json 必须是合法 JSON");
        }
        // 内核现在会拿"上次应用的壁纸"当启动场景（`~/.cache/wallpaper-hub/last-applied`）
        let last = std::fs::read_to_string(crate::last_applied_file()).unwrap_or_default();
        let mut lines = last.lines();
        let kind = lines.next().unwrap_or("").trim().to_string();
        let path = lines.next().unwrap_or("").trim().to_string();
        let scene = scene_id_from_path(&path).unwrap_or_default();
        let dec = decide(true, &scene, p_src.clone(), f_src.clone());
        eprintln!(
            "[live] 存档={} · 上次应用 kind={kind} path={path} ⇒ {}",
            dir.display(),
            dec.log_line("内核启动")
                .unwrap_or_else(|| "（没有当前场景/开关关 ⇒ 什么都不做）".to_string())
        );
        // 有场景 + 两份存档都可读 ⇒ 决策必须落到 Apply（Skip 只该发生在两份都读不了时）
        if !scene.is_empty()
            && matches!(p_src, Src::Text(_))
            && matches!(f_src, Src::Text(_))
        {
            assert!(
                matches!(dec, Decision::Apply(_)),
                "两份存档都可读、又有场景 ⇒ 必须恢复：{dec:?}"
            );
        }
    }
}
