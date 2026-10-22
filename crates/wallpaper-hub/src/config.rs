// SPDX-License-Identifier: GPL-3.0-only

use cosmic::cosmic_config::{self, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry};
use std::collections::BTreeMap;

/// 画面色彩的**中性值**（= 不做任何处理，走原来那条 shader ⇒ 零开销）。
/// 空串 / 坏值 / 没有记录的壁纸一律按它算。
pub const COLOR_IDENTITY: &str = "1,1,1,0,0,1";

#[derive(Debug, Clone, CosmicConfigEntry, Eq, PartialEq)]
#[version = 1]
pub struct Config {
    demo: String,
    /// 性能档位：`powersave` / `balanced` / `quality`
    /// （空字符串 = 还没设过 ⇒ 按 `balanced` 处理；`custom` = 逐项被改过）
    pub perf_preset: String,
    /// 下面**三项**是"高级"里的逐项值；档位只是给它们一组预设
    /// （内核侧是纯机制、无状态 ⇒ 界面发什么它就执行什么）。
    ///
    /// ⚠ 2026-10-14：**帧率不在这里了**。它以前是这个结构体里的一个全局 `fps: u64`，
    /// 于是"每次应用壁纸都下发同一个数" —— 重场景被卡在 30、轻场景也上不去 60，
    /// 用户看到的就是"跟手慢 / 延迟太高"。现在 fps 是**逐场景**的
    /// （见 [`fps_for_scene`] 与 `~/.local/share/wallpaper-hub/scene_fps.json`），
    /// 默认 60、可按场景钉 30。旧的 `fps` 键（`~/.config/cosmic/…/v1/fps`）
    /// 从本轮起**不被读取也不再写入** —— 老机器上那个 30 不会覆盖任何东西。
    pub render_scale: String,
    /// `all` / `none`
    pub effects: String,
    /// `never` / `fullscreen` / `maximized`
    pub sleep_mode: String,
    /// 后端选择：`true` = 自研内核（we-native），`false` = waywallen。
    ///
    /// 必须持久化：默认值 `false`（waywallen）意味着"每次开界面都回 waywallen"——
    /// 而 waywallen 的 daemon 重启后不会自动起来（瞬态服务）⇒ 用户会看到"未连接 + 列表空"，
    /// 哪怕自研内核好好地在跑。用户选过一次就该记住。
    pub backend_native: bool,
    /// 取景模式（scene 壁纸的画布比例 ≠ 屏幕比例时怎么放）：
    /// `cover`（默认，铺满裁边）/ `stretch`（拉伸）/ `center`（1:1 居中）/ `contain`（整张留黑）。
    /// 跟 fps/scale/effects/sleep 那四项**无关** —— 那四项是性能，这个是几何。
    pub fit_mode: String,
    /// 画面色彩调整：`"亮度,对比度,饱和度,色相,色温,伽马"`（六个浮点的字符串）。
    /// 单位值 `1,1,1,0,0,1`（= 不做任何处理，走原来那条 shader ⇒ 零开销）。
    ///
    /// ⚠ 2026-10-12 起它只是**旧的全局值**：真正生效的是 [`Config::color_by_id`]
    /// （按壁纸 id 分槽）。这里保留是为了**兼容与迁移** —— 分槽表里没有某条壁纸时，
    /// 用这个值当它的默认（老用户之前调过的全局色彩不会丢）。
    pub color: String,
    /// **按壁纸 id 分槽的画面色彩**：id → `"亮度,对比度,饱和度,色相,色温,伽马"`。
    ///
    /// 切到哪条壁纸就下发它自己那一份；切回来时恢复（用户要求「色彩调整不要全局生效，
    /// 而是针对那一个壁纸」）。某条没记录时退回 [`Config::color`]，再没有就是
    /// [`COLOR_IDENTITY`]。存的是**中性值**时不写进表（等于没有覆盖）。
    pub color_by_id: BTreeMap<String, String>,
    /// 详情面板是否显示作者嵌在工程文件里的**宣传图**（`general.properties[*].text` /
    /// `description` 里嵌的图片引用，见 `promo` 模块）。
    ///
    /// **默认 `true` = 显示**（用户 2026-10-12 拍板「放回来吧，别去除了。或者默认展示」）。
    /// 那批图是作者写死的外链（实测全是 `i.ibb.co`），显示意味着本程序主动对外发起请求，
    /// 所以**保留开关**：关掉后既不显示、也不发任何请求（缓存仍在
    /// `~/.cache/wallpaper-hub/promo/`，按 URL 哈希）。
    pub show_promo: bool,
    /// 连上内核时是否允许**自动推进「上次应用」**那条壁纸。
    ///
    /// ⚠ 它**永远不换掉内核正在播的那条** —— 内核是它那一层壁纸的权威（内核自己也做
    /// 启动恢复）。只有内核**空闲**（没内容 / 没在播）时才会把"上次应用"补上。
    /// 默认 `true` = "内核空闲时补一条（不乱换）"；要**彻底关掉**这种自动同步：
    /// `--no-auto-apply` / `WPHUB_NO_AUTO_APPLY=1`（只影响本次会话），
    /// 或把这一项写成 `false`（持久）。判据是纯函数 `app::decide_auto_apply`。
    pub auto_apply_last: bool,
    /// **本地壁纸库根目录**（一行一个路径；`~` 会展开）。空表 = 用默认值 `~/Templates`。
    /// 这些目录下每个子目录各算一条壁纸（`<root>/<id>/` 或 `<root>/<任意名>/`）。
    /// 有效的根目录还会**连同可选的 Steam 工坊目录**一起写进
    /// `~/.local/share/wallpaper-hub/library_dirs`，常驻内核读那一份 ⇒ 这里改完，
    /// 内核不用重启也能按 id 找到新位置。环境变量 `WPHUB_LIBRARY_DIRS` 会整体覆盖本项。
    pub library_dirs: Vec<String>,
    /// **工坊页筛选里"与默认值不同"的那些项**（一行一个 `组键:标签=1|0`，1 = 勾选；
    /// 例：`type:Application=1`、`tags:Anime=0`、`only:approved=1`）。空表 = WE 默认筛选。
    ///
    /// 为什么只写差异：默认值本身就有"默认不勾"的项（Application / Questionable /
    /// Mature / Unspecified）—— 全量存"勾选表"表达不了"把默认不勾的勾上"，全量存"未勾表"
    /// 又表达不了反向那一半；差异表两边都能表达，而且将来改默认值时老配置自然跟着走。
    /// 标签**只能**来自 [`crate::wp::workshop::GROUPS`]（Steam 标签原文）；
    /// 读的时候会丢掉不认识的组/标签（`workshop::Filter::from_lines`），
    /// 所以标签表以后增删都不会让这里变成"发了却永远筛不到东西"的脏数据。
    /// 用户要求"重启/切页后筛选状态保留" ⇒ 每次防抖放行时写回。
    pub workshop_filter_state: Vec<String>,
    /// 工坊页那个「筛选」区是否展开（收起/展开也记住，重启后还原）。
    pub workshop_filter_open: bool,
}

/// 手写 `Default`（**不能**再 derive）：`show_promo` 的默认值要是 `true`
/// （宣传图默认展示），而 derive 出来的 bool 默认是 `false`。
/// `CosmicConfigEntry` 的 `get_entry` 内部就是 `Self::default()` 打底，
/// 所以磁盘上没有这个键时拿到的就是这里的值。
impl Default for Config {
    fn default() -> Self {
        Self {
            demo: String::new(),
            perf_preset: String::new(),
            render_scale: String::new(),
            effects: String::new(),
            sleep_mode: String::new(),
            backend_native: false,
            fit_mode: String::new(),
            color: String::new(),
            color_by_id: BTreeMap::new(),
            show_promo: true,
            // 默认"不乱换"：允许在内核**空闲**时补上"上次应用"，但绝不换掉内核正在播的那条。
            auto_apply_last: true,
            // 空表 = 默认根目录 `~/Templates`（见 `wp::library::default_roots`）。
            library_dirs: Vec::new(),
            // 工坊筛选默认不筛、默认收起（第一次进工坊页看到的是"趋势"而不是空结果）。
            workshop_filter_state: Vec::new(),
            workshop_filter_open: false,
        }
    }
}

/// 某条壁纸应该用的色彩串：分槽表里存过就用它；没存过退回**旧的全局** `color`
/// （迁移兼容 —— 老用户调过的全局色彩当成所有壁纸的默认）；全局也是空串才用中性值。
pub fn color_for_id<'a>(cfg: &'a Config, id: &str) -> &'a str {
    match cfg.color_by_id.get(id) {
        Some(s) if !s.is_empty() => s.as_str(),
        _ if !cfg.color.is_empty() => cfg.color.as_str(),
        _ => COLOR_IDENTITY,
    }
}

/// 没有具体壁纸时用的色彩（新增壁纸前的"全局"档，就是旧的 `color` 字段）。
pub fn color_default(cfg: &Config) -> &str {
    if cfg.color.is_empty() {
        COLOR_IDENTITY
    } else {
        cfg.color.as_str()
    }
}

/// 三档预设：低配调低、高配调高（用户选的形状）。**只管三项**（渲染尺寸/特效/睡眠）：
/// 帧率从 2026-10-14 起是**逐场景**的，不再由档位统一钉死
/// （旧档位把 fps 一起改 ⇒ 每次应用都覆盖掉场景自己的选择，那正是"跟手慢"的病根）。
pub const PRESET_POWERSAVE: (&str, &str, &str, &str) = ("powersave", "logical", "none", "maximized");
pub const PRESET_BALANCED: (&str, &str, &str, &str) = ("balanced", "physical", "all", "fullscreen");
pub const PRESET_QUALITY: (&str, &str, &str, &str) = ("quality", "physical", "all", "never");

/// 三档全表，顺序就是界面上的顺序。
pub const PRESETS: [(&str, &str, &str, &str); 3] = [PRESET_POWERSAVE, PRESET_BALANCED, PRESET_QUALITY];

/// 档位名 → 界面上的标签。`custom` 不是真档位：表示四项被改成了"不属于任何一档"的组合。
pub fn preset_label(name: &str) -> String {
    match name {
        "powersave" => "省电".to_string(),
        "balanced" => "平衡".to_string(),
        "quality" => "画质".to_string(),
        "custom" => "自定义".to_string(),
        other => other.to_string(),
    }
}

/// 按名字取一档（界面点哪一档就走它）。
pub fn preset_by_name(name: &str) -> Option<(&'static str, &'static str, &'static str, &'static str)> {
    PRESETS.iter().copied().find(|(n, ..)| *n == name)
}

/// 这三项正好是哪一档？都不等 ⇒ None（界面显示"自定义"）。
/// 用它反推档位名，而不是记"最后点了哪个按钮" —— 逐项改回一档的值时标签自动跟上。
/// **不含 fps**：帧率逐场景决定，与档位正交（见 [`fps_for_scene`]）。
pub fn preset_for(scale: &str, effects: &str, sleep: &str) -> Option<&'static str> {
    PRESETS
        .iter()
        .find(|(_, s, e, sl)| *s == scale && *e == effects && *sl == sleep)
        .map(|(name, ..)| *name)
}

/// 逐项开关的"下一个值"（循环）。放在这里是为了能单测：
/// 界面按钮只调它，不再自己算一遍 —— 算错了会被测试挡住。
pub fn next_scale(value: &str) -> &'static str {
    if value == "logical" { "physical" } else { "logical" }
}

pub fn next_effects(value: &str) -> &'static str {
    if value == "none" { "all" } else { "none" }
}

pub fn next_sleep(value: &str) -> &'static str {
    match value {
        "never" => "fullscreen",
        "fullscreen" => "maximized",
        _ => "never",
    }
}

/// 帧率步进（界面上是 ±5）。内核自己 clamp 1..=240，这里收在 5~120：
/// 低于 5 已经不像动画、高于 120 对桌面壁纸没有意义。
pub fn step_fps(fps: u64, step: i64) -> u64 {
    (fps as i64 + step).clamp(5, 120) as u64
}

/// 把配置折算成"实际要下发给内核"的**三项**（渲染尺寸/特效/睡眠；空值 ⇒ 平衡档的默认）。
///
/// ⚠ **帧率不在这里** —— 它由 [`fps_for_scene`] 按**这条场景**算，不再有一把全局的尺。
/// 返回类型就是这条约束的编译期保证：这里根本没法返回一个 fps。
pub fn effective(cfg: &Config) -> (String, String, String) {
    let d = PRESET_BALANCED;
    let scale = if cfg.render_scale.is_empty() { d.1.to_string() } else { cfg.render_scale.clone() };
    let effects = if cfg.effects.is_empty() { d.2.to_string() } else { cfg.effects.clone() };
    let sleep = if cfg.sleep_mode.is_empty() { d.3.to_string() } else { cfg.sleep_mode.clone() };
    (scale, effects, sleep)
}

// ── 逐场景帧率（2026-10-14）────────────────────────────────────────────────
//
// 实测（本机 AMD Renoir 核显 —— APU，与 CPU 共享内存带宽）：
//   · 重场景 3571777169（原生 3840x2160 + 95 条特效趟）：目标 30 ⇒ 实际 30.4fps / GPU 94%；
//     目标 60 ⇒ 只 32.3fps / GPU 98%（上不去）⇒ **这类场景 60 反而更差**。
//   · 轻场景 2564205376（崩坏3 X-Ray）：目标 60 ⇒ 实际 62.8fps / GPU 84% ⇒ **完全跑得动**。
// ⇒ "一个固定帧率"对两边都不对：重场景要 30、轻场景要 60。
// 所以帧率**按场景记**：默认 60（跟手），用户可对某条场景钉 30（重场景 GPU 会满）；
// 逐场景表里没有记录 = **自适应档**（先 60 跑，实测持续明显跑不到就自动降到 30 并告知用户）。
//
// 为什么不再由"性能档位"统一钉：旧档位（省电/平衡/画质）把 fps 一起改（15/20/30），
// 于是每次应用壁纸都下发同一个数、把 drop-in 的 `WE_SCENE_FPS=60` 覆盖掉 ——
// 用户看到的"跟手慢 / 延迟太高"就是这么来的。档位现在只管三项（见 `PRESETS`）。

/// 逐场景帧率的**新默认**。不再是被写死的 30/20。
pub const SCENE_FPS_DEFAULT: u64 = 60;
/// 重场景 / 省电时的回退档：GPU 会打满，但跟手度明显好于 20。
pub const SCENE_FPS_HEAVY: u64 = 30;
/// 允许写进逐场景表的范围（内核侧另有一层 1..=240 的 clamp）。
pub const SCENE_FPS_MIN: u64 = 5;
pub const SCENE_FPS_MAX: u64 = 120;

/// 自适应判据（**只在"自动档"上生效**，用户钉过的场景绝不动）：
/// 目标 60 时实测持续低于 [`ADAPT_FAIL_FPS`] 就算"跑不到"，连着 [`ADAPT_FAIL_STREAK`]
/// 次采样（3 秒一条 ⇒ 约 18 秒）才降到 [`SCENE_FPS_HEAVY`] 并**告知用户**（日志 + 界面），绝不偷偷降。
/// 低于 [`ADAPT_FLOOR_FPS`] 的不算 —— 那是"几乎没在画"（视频播完了 / 没内容），不是"跑不到"，
/// 不然会误把静止画面判成"重场景"。
///
/// ## 2026-10-15：不许再把**外部负载**当成**场景能力上限**
///
/// 实测事故（journal 原文；当时用户在看的轻场景 X-Ray `2564205376`，**低载实测 62.8fps / GPU 84%**，
/// 也就是"完全跑得动 60"）：
/// ```text
/// [自适应] 实测 61.8 → 61.1 → 55.1 → 56.3 → 52.0 → 50.3 → 50.4 → 49.7 → 48.3 → 49.7
/// [自适应] 实测 49.7 fps 连续 3 次低于 50 ⇒ 本场景已降到 30 fps（重场景 GPU 会满）
/// ```
/// 那 30 秒里正好有**别的进程**在编译/渲染 —— 本机 GPU 是 AMD Renoir 核显（**APU，与 CPU 共享内存
/// 带宽**），别人的负载会**直接**把这条场景的帧率压下去。旧判据把这个瞬时下降读成"这条场景跑不到"，
/// 降档把延迟加倍，与用户诉求（"要跟手"）**相反**。三条修法（都有单测钉住）：
///
/// 1. **先看机器有没有别的负载**：[`external_load`]（`/proc/loadavg`）判为真 ⇒ 这条实测值反映的是
///    "别人在抢机器"⇒ **整个观测丢掉、不判定**（既不 +1 也不清零）；
/// 2. **显著放宽**：`FAIL 50 → 40`、`STREAK 3 → 6`（≈ 18 秒）—— 只有**持续**跑不到才动手；
/// 3. **降档可恢复**：[`adapt_should_upgrade`] 满足 ⇒ **升回 60**（同样出声）。
///
/// **核心原则不变**：降档必须说话（日志 + 界面），用户钉过/手选过的场景一个字都不动。
///
/// **如实记一处残余缺口**：外部负载这一门看的是 `/proc/loadavg`（CPU / 内存带宽）。
/// 若有**别的进程在占 GPU**、而 CPU 负载不高（实测本机跑别的渲染器时 GPU 99%、loadavg 才 6.24），
/// 这条场景的实测值仍可能被外部抢 GPU 压下来 ⇒ 仍会被判成"跑不到"。代价被"显著放宽 + 可恢复"
/// 限制成"最多几分钟的 30fps，而且每次都出声"，不再是旧判据那种"一降就永远 30"。
/// 想彻底分清"场景重"与"别人占 GPU"，需要按进程拆 GPU 占用（本机没有 `radeontop` 之类工具）。
pub const ADAPT_FAIL_FPS: f64 = 40.0;
pub const ADAPT_FLOOR_FPS: f64 = 10.0;
pub const ADAPT_FAIL_STREAK: u8 = 6;

/// 「稳定地跑不到」的判据：连击窗口里实测值的 **max − min** 不许超过这个数（fps）。
///
/// 依据（本机实测，2026-10-15）：
/// * 真重场景 `3571777169`（目标 60）实测 `29.7 32.1 31.5 30.8 31.2 30.4` ⇒ 极差 **2.4**
///   （瓶颈就是它自己，帧率被自己的工作量钉住，很稳）；
/// * **别人在抢 GPU** 的那 36 秒（轻场景 `2564205376`，另一个渲染器把 GPU 顶到 99%）实测
///   `28.5 27.2 26.9 38.3 28.1 27.6 28.6` ⇒ 极差 **11.4**（外部负载忽强忽弱，帧率跟着跳）。
/// ⇒ 8.0 落在两者之间：**忽高忽低的"跑不到"不算能力上限**，只有稳定地跑不到才降档。
pub const ADAPT_PLATEAU_SPREAD: f64 = 8.0;
/// GPU 打满时升回冷却的倍数：GPU 打满**可能是**这条场景真重，也可能**是别人在占 GPU**（读不到按进程
/// 拆分的 GPU 占用，分不清）⇒ 不当场升回，但也**绝不永远卡在 30**：把 [`ADAPT_UPGRADE_MIN_SECS`]
/// 乘这个数再试一次（这就是旧判据"一降就永远 30"的封堵点）。
pub const ADAPT_GPU_HOLD_FACTOR: u64 = 3;

/// 一次采样的连胜计数推进：正常达标 ⇒ 归零；"跑不到"（在 `FLOOR..FAIL` 之间）⇒ +1；
/// 几乎没在画（< FLOOR）⇒ 也归零（不是一次"跑不到"的观测）。
///
/// **有外部负载时不要调它** —— 那种样本整个丢掉（见 [`external_load`] 与 `tick_adaptive`）。
pub fn adapt_step(streak: u8, measured: f64) -> u8 {
    if measured >= ADAPT_FLOOR_FPS && measured < ADAPT_FAIL_FPS {
        streak.saturating_add(1)
    } else {
        0
    }
}

/// 连击窗口里实测值的 `(min, max)`：**不算"跑不到"的样本 / 清零时**重置（与 [`adapt_step`] 同一判据）。
pub fn adapt_span(prev: Option<(f64, f64)>, measured: f64) -> Option<(f64, f64)> {
    if !(ADAPT_FLOOR_FPS..ADAPT_FAIL_FPS).contains(&measured) {
        return None;
    }
    Some(match prev {
        Some((lo, hi)) => (lo.min(measured), hi.max(measured)),
        None => (measured, measured),
    })
}

/// 这段"跑不到"是不是**稳定**的（`max − min ≤ ADAPT_PLATEAU_SPREAD`；没有窗口 ⇒ 不算）。
/// 忽高忽低说明是**外部环境在变**（别人在抢 GPU / 内存带宽），不是这条场景的能力上限。
pub fn adapt_shortfall_is_stable(span: Option<(f64, f64)>) -> bool {
    span.map(|(lo, hi)| hi - lo <= ADAPT_PLATEAU_SPREAD).unwrap_or(false)
}

/// 连胜够了**而且稳定**才该降档（"稳定"那一条的依据见 [`ADAPT_PLATEAU_SPREAD`]）。
pub fn adapt_should_downgrade(streak: u8, span: Option<(f64, f64)>) -> bool {
    streak >= ADAPT_FAIL_STREAK && adapt_shortfall_is_stable(span)
}

/// 两次采样至少要隔多久才算一条**可信**的样本（`status` tick 是 3 秒；太短噪声大）。
pub const ADAPT_SAMPLE_MIN_SECS: f64 = 2.0;

/// 「有别的负载」判据：loadavg(1min) **超过核数的一半**（本机 16 核 ⇒ 8.0），或
/// `/proc/loadavg` 第 4 段的**瞬时 runnable 线程数**超过核数的 3/4（16 核 ⇒ 12）。
///
/// 为什么用 loadavg 当「外部负载」的判据、而不是 GPU 占用：本机实测**这条轻场景在 60fps 下 GPU 就有
/// 84%**（那是它在正常出图），重场景 30fps 下 94% —— 只差 10 个点；拿 GPU 占用当「外部负载」会把
/// 「这条场景在正常干活」也判成负载。loadavg 直接回答「除了我，还有谁在抢 CPU / 内存带宽」，
/// 那正是事故里把帧率压下去的**原因**。GPU 占用另有用途：降档之后判断「是不是这条场景真重」。
pub const ADAPT_LOAD_FRACTION: f64 = 0.5;
pub const ADAPT_RUNNABLE_FRACTION: f64 = 0.75;

/// 降档后要连着多少条**负载干净**的样本才开始考虑升回（3 秒一条 ⇒ 20 条 ≈ 60 秒）。
pub const ADAPT_RECOVER_STREAK: u8 = 20;
/// 降档后**至少**要过这么久才考虑升回（「长时间」的硬下界，顺便把档位抖动的周期兜住）。
pub const ADAPT_UPGRADE_MIN_SECS: u64 = 180;
/// 降档期间读到 GPU 占比 ≥ 这个数 ⇒ 判为「这条场景**真的**把 GPU 打满了」⇒ 不升回。
/// 依据（本机实测三点）：轻场景 60fps **84%** / 重场景 30fps **94%** / 重场景 60fps **98%**
/// ⇒ 88 落在「正常出图」与「真打满」之间，**刻意取保守一侧**（宁可让重场景留在 30，也不来回抖）。
/// 读不到 `gpu_busy_percent`（非 amdgpu / 没权限）⇒ 这条**不参与**判定，只用负载 + 时间。
pub const ADAPT_GPU_BUSY_MAX: f64 = 88.0;

/// `/proc/loadavg` 的一行 → `(1 分钟平均, 瞬时 runnable 线程数)`。
/// 形如 `13.27 8.82 5.86 23/1818 911891`：第 4 段是 `runnable/total`。
/// 解析不了 ⇒ `None`（读不到就**当成没有负载**：宁可不动手，也不靠猜去降用户的档）。
pub fn parse_loadavg(s: &str) -> Option<(f64, usize)> {
    let mut it = s.split_whitespace();
    let load1: f64 = it.next()?.parse().ok()?;
    let runnable: usize = it.nth(2)?.split('/').next()?.parse().ok()?;
    Some((load1, runnable))
}

/// 现在有没有**别人**在跟这条场景抢机器（本机实测：外面编译时 loadavg 13.27 / 16 核）。
/// 判为真 ⇒ 这次实测值不可信，**不判定**（既不降档，也不把连击清零）。
pub fn external_load(ncpu: usize, load1: f64, runnable: usize) -> bool {
    let cores = ncpu.max(1) as f64;
    load1 > cores * ADAPT_LOAD_FRACTION || runnable as f64 > cores * ADAPT_RUNNABLE_FRACTION
}

/// 降档期间：这一条样本算不算"负载干净、可以计入升回连击"。
/// 只有一条判据：**没有外部负载**（[`external_load`]）。
///
/// 为什么 GPU 打满**不**在这里清零：GPU 打满既可能是这条场景真重，也可能是**别人在占 GPU**，
/// 只凭整卡占用分不清（这点如实说）。所以它不作为拦截条件，而是把升回的冷却乘
/// [`ADAPT_GPU_HOLD_FACTOR`]（见 [`adapt_should_upgrade`]）—— 既不冒进，也**不会永远卡在 30**。
pub fn adapt_recover_sample_ok(external_load: bool) -> bool {
    !external_load
}

/// 把这一条样本并进"升回"连击（不干净 ⇒ 清零）。
pub fn adapt_recover_step(streak: u8, ok: bool) -> u8 {
    if ok {
        streak.saturating_add(1)
    } else {
        0
    }
}

/// 该升回了：连着 [`ADAPT_RECOVER_STREAK`] 条干净样本，**且**距降档 ≥ 冷却时间。
/// 冷却时间平时是 [`ADAPT_UPGRADE_MIN_SECS`]；降档期间读到 GPU 打满（≥ [`ADAPT_GPU_BUSY_MAX`]）
/// 就乘 [`ADAPT_GPU_HOLD_FACTOR`]（"真重的可能性更大，晚点再试"），但**一定会再试**。
///
/// 为什么"实测回到目标附近"没出现在条件里：降档后内核的上限就是 30，实测值被这个上限夹住
/// （重场景实测 30.4fps = 上限），所以"实测 ≈ 60"在降档状态下**物理上不可能出现**。
/// 因此判据是"负载长期干净 + 时间够"，而不是"等实测回到 60"。
pub fn adapt_should_upgrade(recover_streak: u8, secs_since_downgrade: f64, gpu_busy: Option<f64>) -> bool {
    recover_streak >= ADAPT_RECOVER_STREAK && secs_since_downgrade >= upgrade_cooldown_secs(gpu_busy) as f64
}

/// 升回冷却：GPU 打满时乘 [`ADAPT_GPU_HOLD_FACTOR`]（读不到 GPU 占用 ⇒ 按普通冷却，不假装知道）。
pub fn upgrade_cooldown_secs(gpu_busy: Option<f64>) -> u64 {
    match gpu_busy {
        Some(b) if b >= ADAPT_GPU_BUSY_MAX => ADAPT_UPGRADE_MIN_SECS * ADAPT_GPU_HOLD_FACTOR,
        _ => ADAPT_UPGRADE_MIN_SECS,
    }
}

// ── 自适应那几句话（日志与界面用**同一句**，每条都写清依据，好追因）──────────────
// 全部是纯函数 ⇒ 文案本身也有单测（`every_adaptive_verdict_says_why`）。

/// `[自适应]` 每条样本那行（含「连续 k/6 次」进度、loadavg、GPU）。
pub fn adapt_sample_line(
    measured: f64,
    frames: u64,
    dt: f64,
    scene: &str,
    streak: u8,
    ncpu: usize,
    load1: f64,
    gpu_busy: Option<f64>,
) -> String {
    format!(
        "[自适应] 实测 {measured:.1} fps（目标 {}；{frames} 帧 / {dt:.1}s；连续 {streak}/{ADAPT_FAIL_STREAK} 次低于 \
         {ADAPT_FAIL_FPS:.0}；loadavg {load1:.2}/{ncpu}；GPU {}；场景 {scene}）",
        SCENE_FPS_DEFAULT,
        gpu_text(gpu_busy)
    )
}

/// 判定为**外部负载** ⇒ 不降档（事故现场那句话）。
pub fn adapt_external_note(measured: f64, ncpu: usize, load1: f64, runnable: usize) -> String {
    format!(
        "自适应：实测 {measured:.1} fps 但 loadavg {load1:.2}/{ncpu}（runnable {runnable}）超过阈值 {:.1} \
         ⇒ 判定为外部负载，不降档（本场景仍是 {SCENE_FPS_DEFAULT} fps）",
        ncpu.max(1) as f64 * ADAPT_LOAD_FRACTION
    )
}

/// 数够了但**样本忽高忽低** ⇒ 也不算能力上限，不降档（也写清依据）。
pub fn adapt_unstable_note(streak: u8, span: (f64, f64), ncpu: usize, load1: f64) -> String {
    let (lo, hi) = span;
    format!(
        "自适应：实测连续 {streak} 次低于 {ADAPT_FAIL_FPS:.0}，但这几次在 {lo:.1}~{hi:.1} fps 之间跳\
         （极差 {:.1} > {ADAPT_PLATEAU_SPREAD:.1}）⇒ 判为外部负载忽强忽弱，不降档\
         （稳定地跑不到才降；loadavg {load1:.2}/{ncpu}）",
        hi - lo
    )
}

/// 降档那句话（说清依据：连续次数、阈值、稳定性、当时的负载）。
pub fn adapt_downgrade_note(measured: f64, streak: u8, span: Option<(f64, f64)>, ncpu: usize, load1: f64) -> String {
    let stable = match span {
        Some((lo, hi)) => format!("稳定在 {lo:.1}~{hi:.1} fps（极差 {:.1}）", hi - lo),
        None => "稳定".to_string(),
    };
    format!(
        "自适应：实测 {measured:.1} fps 连续 {streak} 次低于 {ADAPT_FAIL_FPS:.0}（{stable}），\
         且 loadavg {load1:.2}/{ncpu} 不高 ⇒ 本场景已降到 {SCENE_FPS_HEAVY} fps（重场景 GPU 会满；\
         负载干净 {ADAPT_UPGRADE_MIN_SECS} 秒起会重测，能跑得动就升回 {SCENE_FPS_DEFAULT}）"
    )
}

/// 中途达标 / 计数清零那句话（把「为什么不降」也说出来）。
pub fn adapt_clear_note(measured: f64, before: u8, ncpu: usize, load1: f64) -> String {
    if measured < ADAPT_FLOOR_FPS {
        // 低于 FLOOR 不是「跑不到」，是「几乎没在画」—— 分开说，别让日志撒谎
        return format!(
            "自适应：实测 {measured:.1} fps 低于 {ADAPT_FLOOR_FPS:.0} ⇒ 判为「几乎没在画」（视频播完 / 没内容），\
             不算跑不到 ⇒ 连击清零（此前 {before}/{ADAPT_FAIL_STREAK}；loadavg {load1:.2}/{ncpu}）"
        );
    }
    format!(
        "自适应：实测 {measured:.1} fps ⇒ 达标（≥ {ADAPT_FAIL_FPS:.0}），连续计数清零（此前 {before}/{ADAPT_FAIL_STREAK}；\
         loadavg {load1:.2}/{ncpu}）"
    )
}

/// 升回那句话（依据：干净样本条数、loadavg、GPU、距降档多久）。
pub fn adapt_upgrade_note(ncpu: usize, load1: f64, gpu_busy: Option<f64>, secs: f64) -> String {
    format!(
        "自适应：降档后连续 {ADAPT_RECOVER_STREAK} 次采样负载干净（loadavg {load1:.2}/{ncpu}、GPU {}），\
         且距降档 {secs:.0} 秒 ≥ {ADAPT_UPGRADE_MIN_SECS} ⇒ 本场景已升回 {SCENE_FPS_DEFAULT} fps（跟手优先）",
        gpu_text(gpu_busy)
    )
}

/// 升回被 GPU 打满压住那句话（不许默默卡在 30，也**不许把猜的说成确定的**）。
pub fn adapt_gpu_hold_note(gpu_busy: f64, ncpu: usize, load1: f64) -> String {
    format!(
        "自适应：降档后负载正常（loadavg {load1:.2}/{ncpu}）但 GPU {gpu_busy:.0}% 已打满（≥ {ADAPT_GPU_BUSY_MAX:.0}%）\
         ⇒ 分不清是这条场景真重、还是别的进程在占 GPU ⇒ 升回冷却乘 {ADAPT_GPU_HOLD_FACTOR}（{} 秒后再试），\
         本场景暂保持 {SCENE_FPS_HEAVY} fps —— **不会永远卡在 {SCENE_FPS_HEAVY}**",
        upgrade_cooldown_secs(Some(gpu_busy))
    )
}

/// GPU 占用的显示口径：读不到就写「未知」，**绝不编一个数**。
pub fn gpu_text(gpu_busy: Option<f64>) -> String {
    gpu_busy.map(|b| format!("{b:.0}%")).unwrap_or_else(|| "未知".into())
}

/// 一条场景的 fps 是**谁定的**（日志/界面要如实写"来源"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FpsSource {
    /// 逐场景表里有记录（用户按这条场景改过，或以前记住的）
    SceneTable,
    /// 表里没有记录 ⇒ 用新默认（[`SCENE_FPS_DEFAULT`]）
    Default,
    /// 用户刚在界面上按的（自适应 / 60 / 30）
    UserChoice,
    /// 自适应自动降档（实测持续跑不到）
    Adaptive,
}

impl FpsSource {
    /// 日志/界面里"来源="后面那个中文词。
    pub fn label(self) -> &'static str {
        match self {
            FpsSource::SceneTable => "逐场景表",
            FpsSource::Default => "默认",
            FpsSource::UserChoice => "用户选择",
            FpsSource::Adaptive => "自适应",
        }
    }
}

/// 这条场景该用多少 fps，以及这个值是哪来的。
///
/// ① 逐场景表里有（且 > 0）⇒ 用它（来源 = [`FpsSource::SceneTable`]）；
/// ② 没有 ⇒ 新默认 [`SCENE_FPS_DEFAULT`]（来源 = [`FpsSource::Default`]）。
///
/// **绝不**退回"全局固定档" —— 那正是"每次应用都被静默盖成 30"的病根。
/// `id` 为 `None`（还没选中 / 没在播）时按默认算：宁可给 60，也不给一个假的"用户档"。
pub fn fps_for_scene(table: &BTreeMap<String, u64>, id: Option<&str>) -> (u64, FpsSource) {
    if let Some(id) = id
        && let Some(&f) = table.get(id)
        && f > 0
    {
        return (f.clamp(SCENE_FPS_MIN, SCENE_FPS_MAX), FpsSource::SceneTable);
    }
    (SCENE_FPS_DEFAULT, FpsSource::Default)
}

/// 用户给这条场景钉一个值（写进逐场景表）。返回真正落进去的值（已 clamp）。
pub fn set_scene_fps(table: &mut BTreeMap<String, u64>, id: &str, fps: u64) -> u64 {
    let v = fps.clamp(SCENE_FPS_MIN, SCENE_FPS_MAX);
    table.insert(id.to_string(), v);
    v
}

/// 「自适应」= 删掉这条场景的记录（回到"默认 60；实测不够自动降 30"）。
/// 返回**以前是否记过**（日志用：说清"这条场景刚从 30 回到自适应"）。
pub fn clear_scene_fps(table: &mut BTreeMap<String, u64>, id: &str) -> bool {
    table.remove(id).is_some()
}

/// 界面上"本场景现在是什么档"的那句话（纯函数，可单测）。
///
/// 自适应那句必须把**新判据**说全：降档门槛（`< 40` 连续 6 次）+ **外部负载不判定**
/// + **降档后可升回** —— 否则用户会以为"低于 50 就掉到 30"（那正是被否掉的旧行为）。
pub fn scene_fps_label(fps: u64, src: FpsSource) -> String {
    match src {
        FpsSource::SceneTable => format!("{fps} fps（固定 · 已按这条场景记住）"),
        _ => format!(
            "{fps} fps（自适应 · 实测连续 {ADAPT_FAIL_STREAK} 次低于 {:.0} 才降到 {SCENE_FPS_HEAVY}；\
             机器上有别的负载（loadavg 高）时不判定，降档后负载长期干净会升回 {SCENE_FPS_DEFAULT}）",
            ADAPT_FAIL_FPS
        ),
    }
}

// ── 创意工坊「发现页」的加载性能参数 ────────────────────────────────────────
//
// 这些不是"用户偏好"，是**机制**参数：发现页唯一的耗时来源是 Steam 的公开接口和它的图片 CDN，
// 所以数值由实测决定（实测记录见 `wp/workshop.rs` 的模块注释）。集中放这里是为了：
// 一处可见、可单测、改一个数就能试。
//
// 实测（2026-10-12，本机家宽，30 条/页）：搜索页 1.75s、30 条详情一次请求 1.9s、
// 缩略图单张串行 1~4s（CDN 建连占大头，波动很大）⇒ 24 张串行 = 26~95s，这就是"首次加载要等一两分钟"的来源。

/// 缩略图并发下载数。实测 24 张：并发 12 → **2.65s**，并发 8 → 5.28s，并发 16 → 7.29s（CDN 侧开始限速）。
/// 上限也是资源上限：每个下载最多占 1 个 socket（≈1 个 fd）+ 一张图的内存（实测 0.6 MB/张）。
pub const WS_THUMB_CONCURRENCY: usize = 12;
/// 详情接口每次请求带多少个 id（Steam 单请求最多 100；30 个 id 一次请求 1.9s）。
pub const WS_DETAILS_BATCH: usize = 30;
/// 详情请求的并发批数（30 条/页通常只有 1 批；60 个 id 的页 = 2 批并行）。
pub const WS_DETAILS_CONCURRENCY: usize = 3;
/// 单页最多抓几个 id（搜索页 HTML 里的顺序，去重后截断）。
pub const WS_MAX_IDS: usize = 60;
/// 建连（含 TLS 握手）超时。
pub const WS_CONNECT_TIMEOUT_MS: u64 = 4_000;
/// 等响应头超时。
pub const WS_HEADER_TIMEOUT_MS: u64 = 6_000;
/// 读响应体超时。
pub const WS_BODY_TIMEOUT_MS: u64 = 8_000;
/// 单次请求的总超时（兜底，防止任何一条卡住整批）。
pub const WS_GLOBAL_TIMEOUT_MS: u64 = 12_000;
/// 每条请求失败后最多重试几次（1 = 最多两次尝试）；4xx 不重试。
pub const WS_MAX_RETRIES: usize = 1;
/// 重试前的退避。
pub const WS_RETRY_BACKOFF_MS: u64 = 400;
/// 三个阶段各自的 wall-clock 预算：超预算后**不再重试**（只把失败跳过并记数），
/// 保证"最坏情况"也有上界，而不是被 30 条各自的超时串起来。
pub const WS_BROWSE_BUDGET_MS: u64 = 15_000;
pub const WS_DETAILS_BUDGET_MS: u64 = 25_000;
pub const WS_THUMBS_BUDGET_MS: u64 = 30_000;
/// 缩略图缓存有效期：preview_url 里带内容哈希 ⇒ 内容不会变，放长（30 天）。
pub const WS_THUMB_TTL_SECS: u64 = 30 * 24 * 3600;
/// 详情缓存有效期：标题/订阅数/描述会变，一天一刷。
pub const WS_DETAILS_TTL_SECS: u64 = 24 * 3600;
/// 搜索页缓存有效期：5 分钟内的重复进入（切页/重开）直接复用，不再打 Steam。
pub const WS_BROWSE_TTL_SECS: u64 = 300;
/// 缓存容量上限（超出按 mtime 删最旧）：约 800 张缩略图（实测 0.6 MB/张 ⇒ 约 480 MB 上限由字节数兜住）。
pub const WS_CACHE_MAX_ENTRIES: usize = 800;
/// 缓存目录总字节上限：超了先删最旧的图。
pub const WS_CACHE_MAX_BYTES: u64 = 256 * 1024 * 1024;
/// 单张缩略图最大字节（异常/被投毒的响应不许吃内存）。
pub const WS_THUMB_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// 筛选变化的防抖窗口（毫秒）：用户连点 / 连续勾选时**只发最后一次**。
///
/// 300ms 是用户明确要求的；比逐次点击即打网络强得多（一次 browse 1.75s，5 次点击
/// 就是 9s + 5 倍 Steam 压力），又不至于让人等半天才看到结果。
pub const WS_FILTER_DEBOUNCE_MS: u64 = 300;

#[cfg(test)]
mod tests {
    use super::*;

    /// 这些数直接决定"发现页第一次进来要等多久"，改错了要么打爆 Steam CDN、要么还是慢。
    #[test]
    fn workshop_limits_stay_in_sane_ranges() {
        assert!(
            (2..=24).contains(&WS_THUMB_CONCURRENCY),
            "并发要够快（>1）又不能把公共 CDN 和本机 fd 打爆：{}",
            WS_THUMB_CONCURRENCY
        );
        assert!(WS_DETAILS_BATCH > 0 && WS_DETAILS_BATCH <= 100, "Steam 单请求 id 上限 100");
        assert!((1..=WS_DETAILS_CONCURRENCY * WS_DETAILS_BATCH).contains(&WS_MAX_IDS));
        // 三级超时必须都设了，而且 global 不小于任何一个单项
        let global = WS_GLOBAL_TIMEOUT_MS;
        assert!(WS_CONNECT_TIMEOUT_MS > 0 && WS_HEADER_TIMEOUT_MS > 0 && WS_BODY_TIMEOUT_MS > 0);
        assert!(
            global >= WS_CONNECT_TIMEOUT_MS && global >= WS_HEADER_TIMEOUT_MS && global >= WS_BODY_TIMEOUT_MS,
            "总超时是兜底，不能比单项小"
        );
        // 缓存过期：图（不变）> 详情（偶尔变）> 搜索页（随时变）
        assert!(WS_THUMB_TTL_SECS > WS_DETAILS_TTL_SECS);
        assert!(WS_DETAILS_TTL_SECS > WS_BROWSE_TTL_SECS);
        assert!(WS_BROWSE_TTL_SECS > 0, "至少得有过期时间，不能永不过期");
        assert!(WS_MAX_RETRIES >= 1, "要求至少能重试一次");
        assert!(WS_THUMB_MAX_BYTES > 0 && WS_CACHE_MAX_BYTES > WS_THUMB_MAX_BYTES);
        // 筛选：一页只发**一条**请求（`excludedtags[]` 一次就能表达组内 OR / 组间 AND，
        // 见 `wp::workshop::Filter::query`），所以只剩防抖窗口这一个可调项。
        // 300ms 是用户明确要的（太小 = 连点仍会各打一次网络）。
        assert_eq!(WS_FILTER_DEBOUNCE_MS, 300, "防抖窗口是用户拍板的 300ms");
    }

    #[test]
    fn empty_config_means_balanced() {
        // 全新安装：什么都没设过 ⇒ 平衡档的**三项**（物理尺寸 / 全部特效 / 全屏睡眠）。
        // ⚠ 帧率不在这里：它逐场景算、默认 60（见下面 scene_fps 的用例）——
        //    返回类型只有三项，这一点是编译期保证的。
        let (scale, effects, sleep) = effective(&Config::default());
        assert_eq!((scale.as_str(), effects.as_str(), sleep.as_str()), ("physical", "all", "fullscreen"));
    }

    #[test]
    fn each_preset_round_trips_by_name() {
        for (name, scale, effects, sleep) in PRESETS {
            let (n, s, e, sl) = preset_by_name(name).expect("表里应有这一档");
            assert_eq!((n, s, e, sl), (name, scale, effects, sleep));
            // 三项正好等于该档时，反推出的档位名就是它
            assert_eq!(preset_for(scale, effects, sleep), Some(name));
        }
    }

    #[test]
    fn presets_are_distinct_and_ordered_low_to_high() {
        // 三档必须互不相同（不然"切档"没意义），且省电档是最省的那组、画质档是最费的那组。
        // **帧率不参与档位**（2026-10-14 起逐场景）：档位只剩这三项。
        let names: Vec<&str> = PRESETS.iter().map(|p| p.0).collect();
        assert_eq!(names, vec!["powersave", "balanced", "quality"]);
        assert_eq!((PRESET_POWERSAVE.1, PRESET_POWERSAVE.2, PRESET_POWERSAVE.3), ("logical", "none", "maximized"));
        assert_eq!((PRESET_QUALITY.1, PRESET_QUALITY.2, PRESET_QUALITY.3), ("physical", "all", "never"));
        assert_ne!(PRESET_POWERSAVE.1, PRESET_BALANCED.1, "省电与平衡要能区分开");
        assert_eq!(preset_by_name("nope"), None);
    }

    #[test]
    fn mixed_values_are_not_any_preset() {
        // 平衡档但把尺寸改成逻辑 ⇒ 不再等于任何一档 ⇒ 界面显示"自定义"
        assert_eq!(preset_for("logical", "all", "fullscreen"), None);
        // 省电档但把睡眠改成"从不" ⇒ 也不再等于任何一档
        assert_eq!(preset_for("logical", "none", "never"), None);
    }

    #[test]
    fn effective_prefers_stored_fields_over_defaults() {
        let cfg = Config {
            render_scale: "logical".into(),
            effects: "none".into(),
            sleep_mode: "maximized".into(),
            ..Config::default()
        };
        let (scale, effects, sleep) = effective(&cfg);
        assert_eq!((scale.as_str(), effects.as_str(), sleep.as_str()), ("logical", "none", "maximized"));
        // 逐项改出来的组合也能被认出来（那就是省电档）
        assert_eq!(preset_for(&scale, &effects, &sleep), Some("powersave"));
    }

    #[test]
    fn labels_cover_all_names() {
        assert_eq!(preset_label("powersave"), "省电");
        assert_eq!(preset_label("balanced"), "平衡");
        assert_eq!(preset_label("quality"), "画质");
        assert_eq!(preset_label("custom"), "自定义");
        assert_eq!(preset_label(""), "");
    }

    #[test]
    fn per_item_toggles_cycle_through_valid_values() {
        // 渲染尺寸 / 特效链：两态来回
        assert_eq!(next_scale("physical"), "logical");
        assert_eq!(next_scale("logical"), "physical");
        assert_eq!(next_effects("all"), "none");
        assert_eq!(next_effects("none"), "all");
        // 睡眠策略：三态循环，一圈回到起点
        assert_eq!(next_sleep("never"), "fullscreen");
        assert_eq!(next_sleep("fullscreen"), "maximized");
        assert_eq!(next_sleep("maximized"), "never");
        // 空值（全新配置）按平衡档的默认值算，不能卡在空串上
        assert_eq!(next_scale(""), "logical");
        assert_eq!(next_effects(""), "none");
        assert_eq!(next_sleep(""), "never");
    }

    #[test]
    fn toggled_values_are_ones_the_kernel_accepts() {
        // 内核只认这几个字符串（见 003 的 Set 分支）—— 循环出来的值必须都在里面
        let allowed_scale = ["physical", "logical"];
        let allowed_effects = ["all", "none"];
        let allowed_sleep = ["never", "fullscreen", "maximized"];
        for start in ["", "physical", "logical"] {
            assert!(allowed_scale.contains(&next_scale(start)), "{}", next_scale(start));
        }
        for start in ["", "all", "none"] {
            assert!(allowed_effects.contains(&next_effects(start)), "{}", next_effects(start));
        }
        for start in ["", "never", "fullscreen", "maximized"] {
            assert!(allowed_sleep.contains(&next_sleep(start)), "{}", next_sleep(start));
        }
    }

    #[test]
    fn fps_step_moves_by_five_and_clamps() {
        assert_eq!(step_fps(60, 5), 65);
        assert_eq!(step_fps(60, -5), 55);
        // 边界：不能滑出 5~120（内核侧另有一层 1..=240 的 clamp）
        assert_eq!(step_fps(5, -5), 5);
        assert_eq!(step_fps(120, 5), 120);
    }

    /// 2026-10-12 两条用户拍板的行为，用断言钉住：
    /// ① 宣传图**默认展示**（`show_promo = true`，但仍可关）；
    /// ② 画面色彩**按壁纸 id 分槽**：只有记过的那条受影响，别的条目不受牵连；
    ///    旧的全局 `color` 字段当"没记录时的默认值"，老用户的设置不会丢。
    #[test]
    fn promo_defaults_on_and_color_is_per_wallpaper() {
        let d = Config::default();
        assert!(d.show_promo, "宣传图默认展示（改回默认开）");
        assert!(d.color_by_id.is_empty(), "分槽表默认是空的");
        assert_eq!(color_for_id(&d, "2887537617"), COLOR_IDENTITY, "没记录 ⇒ 中性");

        // 只给 A 记一套偏色：B 必须是中性的（这就是"不要全局生效"的判据）
        let mut cfg = Config::default();
        cfg.color_by_id.insert("A".into(), "1.2,1.3,1,0,0,1".into());
        assert_eq!(color_for_id(&cfg, "A"), "1.2,1.3,1,0,0,1");
        assert_eq!(color_for_id(&cfg, "B"), COLOR_IDENTITY, "别的壁纸不受影响");

        // 兼容：磁盘上旧的全局 color 读得到 ⇒ 当"没记录的那条"的默认值
        cfg.color = "0.8,0.9,1,0,0,1".into();
        assert_eq!(color_for_id(&cfg, "B"), "0.8,0.9,1,0,0,1", "旧的全局值当默认");
        assert_eq!(color_for_id(&cfg, "A"), "1.2,1.3,1,0,0,1", "分槽优先于全局");
        assert_eq!(color_default(&cfg), "0.8,0.9,1,0,0,1");

        // 空串不算一条记录（等于没记）
        cfg.color_by_id.insert("C".into(), String::new());
        assert_eq!(color_for_id(&cfg, "C"), "0.8,0.9,1,0,0,1");
    }

    // ── 逐场景帧率（2026-10-14）：默认 60、逐场景记住、可回退 30 ─────────────

    /// 新默认就是 60，且**没有任何全局档**能把它盖成 30（旧病灶）。
    #[test]
    fn scene_fps_defaults_to_60_with_no_global_dial() {
        let mut table = BTreeMap::new();
        // 表里没记录 ⇒ 任何场景都是 60、来源=默认（而不是某个全局 fps）
        assert_eq!(fps_for_scene(&table, Some("3571777169")), (SCENE_FPS_DEFAULT, FpsSource::Default));
        assert_eq!(fps_for_scene(&table, None), (SCENE_FPS_DEFAULT, FpsSource::Default));
        assert_eq!(SCENE_FPS_DEFAULT, 60, "旧默认是 30/20 ⇒ 必须换掉");
        // 用户给这条场景记了 30 ⇒ 只有**这条**变 30，别的仍是 60
        assert_eq!(set_scene_fps(&mut table, "2564205376", SCENE_FPS_HEAVY), 30);
        assert_eq!(fps_for_scene(&table, Some("2564205376")), (30, FpsSource::SceneTable));
        assert_eq!(
            fps_for_scene(&table, Some("3704273480")),
            (60, FpsSource::Default),
            "别的场景不许被牵连（这就是'逐场景'的意思）"
        );
        // 来源如实区分（日志/界面靠它）
        assert_eq!(FpsSource::SceneTable.label(), "逐场景表");
        assert_eq!(FpsSource::Default.label(), "默认");
        assert_eq!(FpsSource::UserChoice.label(), "用户选择");
        assert_eq!(FpsSource::Adaptive.label(), "自适应");
    }

    /// 逐场景表读写：clamp 到 5~120；0/坏值不算一条记录；「自适应」= 删记录。
    #[test]
    fn scene_fps_table_round_trips_and_clamps() {
        let mut table = BTreeMap::new();
        assert_eq!(set_scene_fps(&mut table, "A", 999), SCENE_FPS_MAX);
        assert_eq!(fps_for_scene(&table, Some("A")), (SCENE_FPS_MAX, FpsSource::SceneTable));
        assert_eq!(set_scene_fps(&mut table, "A", 1), SCENE_FPS_MIN);
        // 记 0 等于"没记"（坏数据不当成 0fps 下发）
        table.insert("B".into(), 0);
        assert_eq!(fps_for_scene(&table, Some("B")), (SCENE_FPS_DEFAULT, FpsSource::Default));
        // 「自适应」= 删掉这条
        assert!(clear_scene_fps(&mut table, "A"), "以前记过 ⇒ 返回 true");
        assert!(!clear_scene_fps(&mut table, "A"), "本来就没记 ⇒ false");
        assert_eq!(fps_for_scene(&table, Some("A")), (SCENE_FPS_DEFAULT, FpsSource::Default));
    }

    /// `/proc/loadavg` 的解析与「有别的负载」判据（事故那天本机就是 13.27 / 16 核）。
    #[test]
    fn external_load_is_recognised_from_loadavg() {
        let (load1, runnable) = parse_loadavg("13.27 8.82 5.86 23/1818 911891").expect("真实读数");
        assert!((load1 - 13.27).abs() < 1e-9);
        assert_eq!(runnable, 23);
        assert!(external_load(16, load1, runnable), "13.27 > 8.0（16 核的一半）⇒ 有别人的负载");
        // 空闲机器：0.62 / 16 核（+ 2 个 runnable）⇒ 没有别的负载，可以放心判定
        let (l1, r) = parse_loadavg("0.62 0.40 0.31 2/1818 911891").expect("真实读数");
        assert!(!external_load(16, l1, r));
        // runnable 这一栏自己也能判（瞬时抢 CPU 的人很多，1 分钟平均还没涨上来）
        assert!(external_load(16, 0.10, 15), "瞬时 15 个 runnable > 12 ⇒ 也是在抢机器");
        // 阈值上不算（判据是**超过**）：8.0 / 12 都还是干净的
        assert!(!external_load(16, 8.0, 12));
        // 坏输入不 panic，且**当成没有负载**（宁可不动手，也不靠猜去降用户的档）
        assert_eq!(parse_loadavg(""), None);
        assert_eq!(parse_loadavg("nonsense"), None);
        assert_eq!(parse_loadavg("1.0 2.0"), None);
        assert!(!external_load(16, 0.0, 0));
        // 核数 0（读不到）不许除零 / 不许全判成负载
        assert!(!external_load(0, 0.0, 0));
    }

    /// **事故回放**（journal 原文的十个实测值）：那 30 秒机器上有别的负载 ⇒ 一次都不许降。
    /// 这正是本轮要修的缺陷 —— 把**外部负载**当成了**场景能力上限**。
    #[test]
    fn real_busy_machine_incident_never_downgrades_the_light_scene() {
        // 用户在看的 X-Ray 2564205376（低载实测 62.8fps ⇒ 完全跑得动 60）被压下来的那串值：
        let measured = [61.8, 61.1, 55.1, 56.3, 52.0, 50.3, 50.4, 49.7, 48.3, 49.7];
        let (load1, runnable) = parse_loadavg("13.27 8.82 5.86 23/1818 911891").expect("真实读数");
        assert!(external_load(16, load1, runnable), "别的子代理在编译 ⇒ loadavg 13.27");
        // ① 负载门：有别的负载 ⇒ 观测整个丢掉 ⇒ 连击不动 ⇒ 不降档
        let mut streak = 0u8;
        let mut span = None;
        for m in measured {
            if external_load(16, load1, runnable) {
                continue;
            }
            streak = adapt_step(streak, m);
            span = adapt_span(span, m);
        }
        assert_eq!(streak, 0, "外部负载窗口里的实测值一条都不算");
        assert!(span.is_none(), "窗口里没有任何一条算「跑不到」");
        assert!(!adapt_should_downgrade(streak, span), "外部负载不是这条场景的能力 ⇒ 不许降档");
        // ② 放宽后的门槛自己就够挡住这次事故：这串值最低 48.3，够不上「< 40」
        let mut clean = 0u8;
        let mut clean_span = None;
        for m in measured {
            clean = adapt_step(clean, m);
            clean_span = adapt_span(clean_span, m);
        }
        assert!(!adapt_should_downgrade(clean, clean_span), "放宽到 40 之后这串值也不该降");
        // 对照：旧判据（`< 50` 连续 3 次）在同样的值上**会**降 —— 缺陷复现得出来
        let mut old = 0u8;
        for m in measured {
            old = if (10.0..50.0).contains(&m) { old + 1 } else { 0 };
        }
        assert!(old >= 3, "旧判据（<50 连续 3 次）确实会降 ⇒ 这就是 journal 里那次误降");
    }

    /// **第二轮实测**（2026-10-15 现场：另一个子代理在跑 `we-scene-render`，GPU 97~99%、
    /// 而 CPU 很闲 ⇒ loadavg 只有 2.5~3.3，**CPU 负载门看不见它**）：轻场景 X-Ray 实测被压成
    /// 26~47fps，而且**忽高忽低** ⇒ 只看"连续 6 次 < 40"会误降，加上"稳定"这一条才挡得住。
    /// 真重场景是**稳定**的 30 出头（极差 2.4），不会被这条判据误放。
    #[test]
    fn jittery_shortfall_from_a_gpu_hog_does_not_downgrade() {
        // 现场实测（每 3.4 秒一条；loadavg 2.5~3.3 / 16 核，GPU 97~99%）：
        let live = [28.5, 27.2, 26.9, 38.3, 28.1, 27.6, 28.6];
        assert!(!external_load(16, 3.05, 3), "CPU 侧的负载门确实看不见「别人在占 GPU」");
        let mut s = 0u8;
        let mut span = None;
        for m in live {
            s = adapt_step(s, m);
            span = adapt_span(span, m);
        }
        assert!(s >= ADAPT_FAIL_STREAK, "7 条连续 < 40，数量这一关是过的");
        let (lo, hi) = span.expect("窗口里有 7 条样本");
        assert!(hi - lo > ADAPT_PLATEAU_SPREAD, "极差 {:.1} 超过 {ADAPT_PLATEAU_SPREAD}", hi - lo);
        assert!(!adapt_shortfall_is_stable(span));
        assert!(!adapt_should_downgrade(s, span), "忽高忽低的跑不到 ⇒ 判为外部负载，不降档");
        // 真重场景：稳定在 30 出头 ⇒ 照降不误（"稳定"这一条不许挡住真重场景）
        let mut hs = 0u8;
        let mut hspan = None;
        for m in [29.7, 32.1, 31.5, 30.8, 31.2, 30.4] {
            hs = adapt_step(hs, m);
            hspan = adapt_span(hspan, m);
        }
        assert!(adapt_shortfall_is_stable(hspan));
        assert!(adapt_should_downgrade(hs, hspan), "稳定地跑不到 ⇒ 该降");
        // 窗口怎么维护：达标（≥40）/「几乎没在画」（<10）都不算进窗口
        assert_eq!(adapt_span(span, 47.2), None);
        assert_eq!(adapt_span(span, 4.0), None);
        assert_eq!(adapt_span(None, 30.0), Some((30.0, 30.0)));
        assert_eq!(adapt_span(Some((28.0, 31.0)), 26.5), Some((26.5, 31.0)));
        assert_eq!(adapt_span(Some((28.0, 31.0)), 33.0), Some((28.0, 33.0)));
        assert!(!adapt_shortfall_is_stable(None), "没有窗口 ⇒ 不算「稳定跑不到」");
    }

    /// 「真低才降」：干净机器上**持续**跑不到（连续 6 次 < 40）才降；5 次不降。
    #[test]
    fn downgrade_needs_a_long_unbroken_shortfall() {
        // 重场景 3571777169 的实测（目标 60 只跑到 30.4 / GPU 94%）⇒ 这是**真的**跑不到
        let heavy = [29.7, 32.1, 31.5, 30.8, 31.2, 30.4, 30.1];
        let n = ADAPT_FAIL_STREAK as usize;
        let mut s = 0u8;
        let mut span = None;
        for m in &heavy[..n - 1] {
            s = adapt_step(s, *m);
            span = adapt_span(span, *m);
        }
        assert!(!adapt_should_downgrade(s, span), "{} 次还不够（要连续 {n} 次）", n - 1);
        s = adapt_step(s, heavy[n - 1]);
        span = adapt_span(span, heavy[n - 1]);
        assert!(adapt_should_downgrade(s, span), "连续 {n} 次稳定跑不到 ⇒ 该降");
        // 中途有一次达标 ⇒ 前面的全白数（不累积偶发抖动）
        let mut s = 0u8;
        for m in [30.0, 31.0, 30.0, 62.0, 30.0] {
            s = adapt_step(s, m);
        }
        assert_eq!(s, 1);
        // 边界：刚好 40 算达标（判据是 `< 40`）；45 / 50 更算 —— 这就是「显著放宽」那一步
        assert_eq!(adapt_step(2, ADAPT_FAIL_FPS), 0);
        assert_eq!(adapt_step(2, 45.0), 0);
        assert_eq!(adapt_step(2, 50.0), 0);
        assert_eq!(adapt_step(2, 39.9), 3);
        // < 10 = 几乎没在画（视频播完 / 没内容）⇒ 不算「跑不到」，不然会误降
        assert_eq!(adapt_step(2, 0.0), 0);
        assert_eq!(adapt_step(2, 4.0), 0);
        // 判据常量全部钉住：改任何一个数都要在这张网里显形
        assert_eq!(ADAPT_FAIL_FPS, 40.0);
        assert_eq!(ADAPT_FAIL_STREAK, 6);
        assert_eq!(ADAPT_FLOOR_FPS, 10.0);
        assert_eq!(SCENE_FPS_HEAVY, 30);
        assert_eq!(ADAPT_SAMPLE_MIN_SECS, 2.0);
        assert_eq!(ADAPT_LOAD_FRACTION, 0.5);
        assert_eq!(ADAPT_RUNNABLE_FRACTION, 0.75);
        assert_eq!(ADAPT_GPU_BUSY_MAX, 88.0);
        assert_eq!(ADAPT_UPGRADE_MIN_SECS, 180);
        assert_eq!(ADAPT_RECOVER_STREAK, 20);
        assert_eq!(ADAPT_GPU_HOLD_FACTOR, 3);
        assert_eq!(ADAPT_PLATEAU_SPREAD, 8.0);
        assert!(
            ADAPT_PLATEAU_SPREAD > 2.4 && ADAPT_PLATEAU_SPREAD < 11.4,
            "「稳定」阈值必须落在两点实测之间：真重 2.4 / 别人抢 GPU 11.4"
        );
        assert!(u64::from(ADAPT_RECOVER_STREAK) * 3 >= 60, "3 秒一条 ⇒ 干净样本要凑够 60 秒以上");
    }

    /// **降档可恢复，而且永远不会卡在 30**：负载长期干净 ⇒ 升回 60；
    /// GPU 打满时只是把冷却乘 3（分不清"真重"还是"别人在占 GPU"），到点照样再试。
    #[test]
    fn downgrade_is_recoverable_and_never_stuck_at_30() {
        let mut s = 0u8;
        for _ in 0..ADAPT_RECOVER_STREAK - 1 {
            s = adapt_recover_step(s, true);
        }
        assert!(!adapt_should_upgrade(s, ADAPT_UPGRADE_MIN_SECS as f64, None), "还差一条干净样本");
        s = adapt_recover_step(s, true);
        assert!(adapt_should_upgrade(s, ADAPT_UPGRADE_MIN_SECS as f64, None));
        // 时间不够也不行（「长时间」的硬下界）
        assert!(!adapt_should_upgrade(s, (ADAPT_UPGRADE_MIN_SECS - 1) as f64, None));
        // 外部负载的样本**不算干净**（读数不可信），一条脏的就把连击清零
        assert!(!adapt_recover_sample_ok(true));
        assert_eq!(adapt_recover_step(s, false), 0);
        assert!(adapt_recover_sample_ok(false), "负载干净就算数（GPU 打满不在这里拦）");
        // GPU 打满 ⇒ 冷却乘 3：晚点再试，但**一定会再试**
        let busy = Some(ADAPT_GPU_BUSY_MAX);
        assert_eq!(upgrade_cooldown_secs(busy), ADAPT_UPGRADE_MIN_SECS * ADAPT_GPU_HOLD_FACTOR);
        assert_eq!(upgrade_cooldown_secs(Some(94.0)), ADAPT_UPGRADE_MIN_SECS * ADAPT_GPU_HOLD_FACTOR);
        assert_eq!(upgrade_cooldown_secs(Some(84.0)), ADAPT_UPGRADE_MIN_SECS, "84% < 88% ⇒ 普通冷却");
        assert_eq!(upgrade_cooldown_secs(None), ADAPT_UPGRADE_MIN_SECS, "读不到 GPU ⇒ 普通冷却");
        assert!(!adapt_should_upgrade(s, ADAPT_UPGRADE_MIN_SECS as f64, busy), "GPU 打满 ⇒ 这次还不升");
        assert!(
            adapt_should_upgrade(s, (ADAPT_UPGRADE_MIN_SECS * ADAPT_GPU_HOLD_FACTOR) as f64, busy),
            "等够了照样升回 —— 不允许永远卡在 30"
        );
        // 读不到 GPU 占用（非 amdgpu）⇒ 按普通冷却走，不假装知道
        assert!(adapt_should_upgrade(s, ADAPT_UPGRADE_MIN_SECS as f64, None));
    }

    /// 每条判定都要**能追因**：日志 / 界面那句必须写清依据（外部负载 / 连续次数 / 阈值 / GPU）。
    #[test]
    fn every_adaptive_verdict_says_why() {
        // ① 外部负载 ⇒ 不降档（事故现场那句）
        let ext = adapt_external_note(48.3, 16, 13.27, 23);
        assert!(ext.contains("48.3") && ext.contains("loadavg 13.27/16") && ext.contains("runnable 23"), "{ext}");
        assert!(ext.contains("阈值 8.0"), "{ext}");
        assert!(ext.contains("判定为外部负载") && ext.contains("不降档"), "{ext}");
        // ② 降档 ⇒ 说清连续次数、阈值、稳定性、当时的负载
        let down = adapt_downgrade_note(32.2, ADAPT_FAIL_STREAK, Some((29.7, 32.1)), 16, 0.62);
        assert!(down.contains("32.2") && down.contains("连续 6 次低于 40"), "{down}");
        assert!(down.contains("稳定在 29.7~32.1 fps（极差 2.4）"), "{down}");
        assert!(down.contains("loadavg 0.62/16") && down.contains("降到 30"), "{down}");
        // ②b 数够了但忽高忽低 ⇒ 不降档，也要说清为什么
        let jumpy = adapt_unstable_note(7, (26.9, 38.3), 16, 3.05);
        assert!(jumpy.contains("连续 7 次低于 40"), "{jumpy}");
        assert!(jumpy.contains("26.9~38.3") && jumpy.contains("极差 11.4"), "{jumpy}");
        assert!(jumpy.contains("不降档") && jumpy.contains("忽强忽弱"), "{jumpy}");
        // ③ 升回 ⇒ 说清干净样本条数 / loadavg / GPU / 距降档多久
        let up = adapt_upgrade_note(16, 0.62, Some(42.0), 200.0);
        assert!(up.contains("连续 20 次") && up.contains("loadavg 0.62/16") && up.contains("GPU 42%"), "{up}");
        assert!(up.contains("180") && up.contains("升回 60"), "{up}");
        // ④ 升回被 GPU 打满压住 ⇒ 也要出声，并说清"晚点会再试"，**不许永远卡在 30**
        let hold = adapt_gpu_hold_note(94.0, 16, 0.62);
        assert!(hold.contains("GPU 94%") && hold.contains("真重"), "{hold}");
        assert!(hold.contains("乘 3") && hold.contains("540 秒后再试"), "{hold}");
        assert!(hold.contains("不会永远卡在 30"), "{hold}");
        // ⑤ 每条样本那行：连续 k/6 的进度 + loadavg + GPU 都在
        let sample = adapt_sample_line(48.3, 145, 3.0, "2564205376", 2, 16, 13.27, Some(84.0));
        assert!(sample.contains("48.3") && sample.contains("连续 2/6 次低于 40"), "{sample}");
        assert!(sample.contains("loadavg 13.27/16") && sample.contains("GPU 84%"), "{sample}");
        assert!(sample.contains("2564205376") && sample.starts_with("[自适应]"), "{sample}");
        // GPU 读不到 ⇒ 写「未知」，**绝不编一个数**
        assert!(adapt_sample_line(48.3, 145, 3.0, "x", 0, 16, 1.0, None).contains("GPU 未知"));
        assert_eq!(gpu_text(None), "未知");
        // ⑥ 中途达标清零 ⇒ 也说清「此前 k/6」（为什么不降）
        let clear = adapt_clear_note(62.8, 2, 16, 0.62);
        assert!(clear.contains("62.8") && clear.contains("达标") && clear.contains("此前 2/6"), "{clear}");
        // 「几乎没在画」（< 10）不许说成「达标」，那是两回事
        let idle = adapt_clear_note(3.0, 2, 16, 0.62);
        assert!(idle.contains("几乎没在画") && !idle.contains("达标"), "{idle}");
    }

    /// 界面那句话要能一眼看出"这条是本场景固定档还是自适应档"，而且**判据写全**
    /// （降档门槛 + 外部负载不判定 + 能升回）—— 界面文案必须和代码里的判据一致。
    #[test]
    fn scene_fps_label_distinguishes_fixed_from_adaptive() {
        let fixed = scene_fps_label(30, FpsSource::SceneTable);
        assert!(fixed.contains("30") && fixed.contains("固定") && fixed.contains("记住"), "{fixed}");
        let auto = scene_fps_label(60, FpsSource::Default);
        assert!(auto.contains("60") && auto.contains("自适应") && auto.contains(&SCENE_FPS_HEAVY.to_string()), "{auto}");
        // 与新判据一致：门槛 40 / 连续 6 次 / 有外部负载不判定 / 能升回
        assert!(auto.contains("连续 6 次") && auto.contains("40"), "{auto}");
        assert!(auto.contains("loadavg") && auto.contains("不判定"), "{auto}");
        assert!(auto.contains("升回 60"), "{auto}");
        assert!(!auto.contains("低于 50"), "旧文案不许残留（那是被否掉的判据）：{auto}");
    }
}
