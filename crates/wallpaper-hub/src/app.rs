// SPDX-License-Identifier: MIT
// Wallpaper Hub —— COSMIC 原生壁纸中继客户端
// 渲染由**自研内核**（`we-native`，systemd 用户服务）负责，本程序只做「控制面 + 界面」：
// 直接扫工坊目录拿列表，通过内核的 Unix socket 控制面下发应用/暂停/设置指令。
// 历史上的 waywallen（独立 flatpak 守护进程 + WebSocket protobuf）已整体移除。

use crate::color_wheel;
use crate::config::{self, Config};
use crate::fl;
use crate::promo;
use crate::we_ui;
use crate::wp::{library, local, native, proto, workshop};
use cosmic::app::context_drawer;
use cosmic::cosmic_config::{self, CosmicConfigEntry};
use cosmic::iced::alignment::{Horizontal, Vertical};
use cosmic::iced::widget::container::Style as ContStyle;
use cosmic::iced::{Alignment, Border, Color, Length, Subscription};
use cosmic::prelude::*;
use cosmic::theme::Theme as CTheme;
use cosmic::widget::{self, about::About, icon, menu, nav_bar};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
const APP_ICON: &[u8] = include_bytes!("../resources/icons/hicolor/scalable/apps/icon.svg");

/// Steam 创意工坊（Wallpaper Engine）的 appid —— 发现页要用
const WE_APPID: u32 = 431960;

/// 设置页的标签数（行为 / 显示器 / 状态 / 性能）

pub struct AppModel {
    core: cosmic::Core,
    context_page: ContextPage,
    about: About,
    nav: nav_bar::Model,
    key_binds: HashMap<menu::KeyBind, MenuAction>,
    config: Config,
    /// cosmic-config 句柄：`config` 只是它的**值**，要持久化就得留着句柄 `write_entry`
    config_ctx: Option<cosmic_config::Config>,

    /// 最近一次错误（连不上/请求失败）
    error: Option<String>,
    wallpapers: Vec<proto::WallpaperEntry>,
    /// 自研内核（第二后端）状态；None = 连不上（没在 --serve）
    native: Option<native::Status>,
    /// 最近一次内核操作的说明（成功/失败都给一句人话）
    /// 最近一次动作的提示（"已生效 …" 这类）。
    /// ⚠ 目前**界面上没有常显位置**：角落那行只放资源占用，
    /// 而这句话不会自己消失 ⇒ 塞哪儿都会长期占着。留着等以后做 toast/自动消失再显示。
    #[allow(dead_code)]
    native_note: Option<String>,
    /// 桌面现在由谁在画：true = 自研内核。保留字段只为兼容旧配置（外部 daemon 后端已移除）。
    backend_native: bool,
    /// 最近一次下发给内核的（kind, path），切回来时重新下发（内核懒创建表面）
    native_last: Option<(String, String)>,
    /// 内核最近一次 `set` 回执里的四项（界面拿它显示"内核当前值"）
    kernel_perf: Option<(u64, String, String, String)>,
    /// **逐场景帧率表**（`~/.local/share/wallpaper-hub/scene_fps.json`）：壁纸 id → fps。
    /// 起帧率不再有全局档：表里有记录就用它，没有就用 [`config::SCENE_FPS_DEFAULT`]（60）。
    /// 这就是"重场景 30、轻场景 60"能同时成立的原因（见 `config::fps_for_scene` 的头注释）；
    scene_fps: BTreeMap<String, u64>,
    /// 自适应状态：本场景连续几次实测"明显跑不到"（见 `config::adapt_step`）。
    adapt_streak: u8,
    /// 已经为**当前这条场景**做过自适应降档（同一条不重复动作；切场景会清）。
    adapt_done: bool,
    /// 自适应「升回」连击：降档后连续多少条**负载干净**的样本（见 `config::adapt_recover_step`）。
    /// 满足 `config::adapt_should_upgrade` 就升回 60（同样出声）。
    adapt_recover_streak: u8,
    /// 自适应降档发生的时刻（升回要求"距降档 ≥ `config::ADAPT_UPGRADE_MIN_SECS`"）。
    adapt_downgraded_at: Option<std::time::Instant>,
    /// 当前"跑不到"连击窗口里实测值的 `(min, max)`：判"是不是**稳定**地跑不到"用
    /// （忽高忽低 = 外部负载在忽强忽弱 ⇒ 不降档，见 `config::ADAPT_PLATEAU_SPREAD`）。
    adapt_span: Option<(f64, f64)>,
    /// 自适应正在盯哪条场景（换了就清零重测，先按 60 跑）。
    adapt_scene: Option<String>,
    /// 内核累计帧数的上一次采样（帧数, 时刻）—— 两次之差 ÷ 间隔 = **实际**帧率。
    frame_sample: Option<(u64, std::time::Instant)>,
    /// 自适应最近一次动作/判定（含"因为外部负载所以不降档"）的那句话 ——
    /// 界面上给用户看：绝不偷偷降，也绝不偷偷不降。文案由 `config::adapt_*_note` 生成。
    adapt_note: Option<String>,
    /// **音频分析可用输出（sink）列表**：内核 `audio sinks` 回执。
    /// 空 + `audio_sinks_err = None` = 还没拉到（正在枚举）。
    audio_sinks: Vec<native::SinkInfo>,
    /// 拉 sink 列表失败的那句人话（内核「无法枚举」/ 连不上）。`Some` ⇒ 设置页**如实显示**，
    /// 绝不装作"没有可用输出"。
    audio_sinks_err: Option<String>,
    /// 本次进设置页是否已经要过一次 sink 列表（避免每 3 秒的 status tick 都发一次请求）；
    /// **枚举失败不重试** —— 只有重新进设置页 / 用户点「重新枚举」才会再要一次。
    audio_sinks_asked: bool,
    /// 列 sink 回执里内核报的"当前跟的是哪一路"（`selected`）。
    /// 只在 `status.audio_sink` 还没到（刚启动/老内核）时当兜底显示用。
    audio_sink_seen: Option<String>,
    /// 界面这四项是否已经在**当前这个**内核进程里生效过。
    /// 内核无状态（重启后不记得界面设过什么）⇒ 连上就先补发一次，靠它保证只补发一次。
    perf_synced: bool,
    /// 连上内核时是否允许**自动推进「上次应用」**（`native_last`）。
    /// 由 `Config::auto_apply_last`（默认 true）与 `--no-auto-apply` / `WPHUB_NO_AUTO_APPLY=1`
    /// 共同决定；**只有内核空闲（没在播 / 没内容）时才真的会推**，内核正在播的一律不动
    /// （内核是权威 —— 见纯函数 [`decide_auto_apply`]）。
    auto_apply_enabled: bool,
    /// 属性覆盖表**落盘**的节流状态：`(有未落的改动, 上次落盘时刻)`。
    /// 拖动色盘时 `set_prop` 每帧都被调一次，若每次都 `fs::write` 整个覆盖表就是白扔
    /// IO（每帧一次小文件写）。这里改成"最多 4 次/秒 + 松手必落"（见 [`AppModel::persist_props`]）。
    /// 3 秒一次的 status 轮询顺带补一次，保证"最后一下没松手"也不会丢。
    prop_dirty: bool,
    prop_saved_at: Option<std::time::Instant>,
    search: String,
    /// 发现页：手工输入创意工坊 ID 后跳转
    discover_query: String,

    /// 启动参数 --native-apply-first：拿到列表后，把第一条「内核能播」的下发给自研内核
    /// （没有输入模拟工具，靠它验证「GUI → 控制面 → 内核」这条链路）
    pending_native_apply_first: bool,
    /// 启动参数 `--apply-id <id>` 待补跑的那条：init 里 socket 还没就绪（`request_async`
    /// 一律 false），**第一次连上内核后再走一遍** `apply_native` —— 那一下才真的会下发
    /// 「内容 + 属性 + 这条壁纸自己的色彩」。和 `toggle_props` 的补跑是同一个道理。
    pending_apply_id: Option<String>,
    /// 参数页：正在查看哪条壁纸的参数
    /// 参数页：文本型参数的输入缓存（键 → 文本）
    /// 展开面板：滑块拖动中的本地预览值（键 → 值）。
    /// 拖动时只更新它（不发请求），松手才提交 —— 否则每移动一像素就会发一次请求。

    // 壁纸页：筛选
    /// 类型筛选（0 = 全部；n → known_types[n-1]）
    type_filter: usize,
    /// 标签筛选（0 = 全部）
    tag_filter: usize,
    /// 见过的全部类型/标签（筛选按钮的选项，不受当前筛选影响）
    known_types: Vec<String>,
    known_tags: Vec<String>,

    // ── 发现页（创意工坊，零登录）────────────────────────────────────────────
    discover_items: Vec<workshop::Item>,
    discover_thumbs: HashMap<String, widget::image::Handle>,
    /// 缩略图的**原始字节**（同一份，给按卡片尺寸做 CPU 预缩放用；见 [`thumb_from_bytes`]）。
    /// 30 条一页 ≈ 几 MB，切页时整个换掉。
    discover_thumb_raw: HashMap<String, Vec<u8>>,
    discover_loading: bool,
    /// 上一次搜索**页面里** `filedetails/?id=` 链接的出现次数（含重复）。
    /// 与 `discover_items.len` 一起显示，就是为了把"页面里同一 id 出现两次"（Steam 常态，
    /// 每条两处链接）和"上屏列表里同一 id 出现两次"（**不许发生**）分清楚。
    discover_links: usize,
    /// 上屏前按 id 折叠掉的重复条数（`workshop::dedupe_by_id`；正常 0）
    discover_dedup: usize,
    /// 缩略图是否还在后台逐张补（状态条显示 "缩略图 n/m"）
    discover_thumbs_loading: bool,
    /// 缩略图阶段的统计（日志/状态条用；None = 还没跑过）
    discover_thumb_stats: Option<workshop::Stats>,
    /// 本页搜索开始时刻（渐进式加载的耗时打点：条目上屏 / 缩略图就绪）
    discover_t0: Option<std::time::Instant>,
    discover_error: Option<String>,
    discover_page: u32,
    discover_sort: usize,
    /// 只给截图用：`--workshop-select <工坊id>` —— 发现页结果回来后选中这条
    /// （= 点一下缩略图，走**完全相同**的 `Message::SelectWorkshop`）。本机没有输入模拟工具。
    pending_workshop_select: Option<String>,
    /// 卡片尺寸档。
    /// 只影响**渲染尺寸**，不落盘 —— 换了档立刻按新宽度重排（列数/行数都跟着窗口算），
    /// 因此**重启后仍回到「中」**。
    card_idx: usize,
    /// 内核**播放速度**（场景时间倍率，0.25..3.0，默认 1.0）。滑杆拖动时每帧变一次，
    /// 出站队列按 key 合并 + 55 ms 节流（≤18 Hz），松手 `SliderRelease` 立刻放行终值。
    time_scale: f32,
    /// 内核**音乐音量**（默认 sink 的 volume，0..100）。启动读一次实际值（`read_sink_volume`），
    /// 之后由滑杆驱动；`None` = 读不到（界面显示"未知"，不假装 0）。
    music_vol: Option<f32>,

    /// 角落资源条：上次 CPU 采样（累计 CPU 秒, 采样时刻）＋算出来的占用率
    cpu_sample: Option<(f64, std::time::Instant)>,
    cpu_pct: Option<f64>,
    /// 主窗口的**逻辑宽度**（订阅 iced 的 resize 事件拿到）。
    /// 方块缩略图靠它自己算"每行放几个"，不写死列数。
    win_w: f32,

    // ── WE 皮肤：详情面板 / 逐壁纸属性编辑 ──────────────────────────────
    /// 详情面板里当前选中的壁纸 id（点缩略图 / 列表项）
    selected: Option<String>,
    /// 属性面板里当前展开取色的属性键
    open_color: Option<String>,
    /// 只给截图用（`--open-color <键>`）：**第一次**选中壁纸时把这条属性展开成色盘，
    /// 之后照常收起（`take` ⇒ 一次性）。
    pending_open_color: Option<String>,
    /// 逐壁纸属性覆盖：id → (键 → 值)。
    /// **数据驱动**：键与类型全部来自 `project.json` 的 `general.properties`
    /// （foreach 遍历，不按属性名特判），改一下就写进这里并落盘。
    prop_edits: HashMap<String, HashMap<String, serde_json::Value>>,
    /// 文本型属性的输入缓存："id\u{1f}键" → 文本
    prop_text: HashMap<String, String>,
    /// 「分享 JSON」待写进剪贴板的一段文本：update 里填上，**函数末尾**统一转成 Task
    /// （剪贴板写入是一条 iced Task，没法从 match 分支里直接返回）。
    clip_out: Option<String>,
    /// 右侧详情面板是否显示（顶栏「显示」按钮）
    show_panel: bool,
    /// 只给截图用：属性面板开在**底部**（`--panel-bottom`）—— 本机没有输入模拟工具，
    /// 滚不动面板，靠它把下半截（作者自定义项 + 您的预设）露出来。
    panel_bottom: bool,
    /// 只给截图用：设置页开在**底部**（`--settings-bottom`）—— 同理，设置列比一屏高时
    /// 把最后那几行（壁纸库 / 作者宣传图开关）露出来。
    settings_bottom: bool,
    /// 只给截图用：`--panel-scroll <像素>` —— 面板一开就滚到某个纵向偏移。
    /// 为什么要「重发几次」：滚动是一条 iced Task，布局还没完成时会被丢掉，
    /// 所以前几次状态回执里再发一遍（`panel_scroll_left` 是个小计数器）。
    panel_scroll_req: Option<f32>,
    panel_scroll_left: u8,
    /// 本帧要发出去的滚动请求（update 的 match 分支里没法返回 Task，统一在末尾发）
    scroll_out: Option<f32>,
    /// `--toggle-prop` 的键：init 里先跑一遍（那时内核还没连上，只会落盘），
    /// **首次收到内核状态后再补跑一遍** —— 那一下才真正走「改完即时下发内核」那条路。
    toggle_props: Vec<String>,
    toggle_redone: bool,
    /// 预览图尺寸缓存（选中时算一次，状态条显示用；不每帧读文件）
    dims: HashMap<String, (u32, u32)>,
    /// 面板顶部大图**实际用的文件**（id → 路径）。默认就是封面；包内若有
    /// **像素比封面更大**的本地图（`select` 时扫一遍目录）则记成那张 —— 见「优先用大图」。
    preview_src: HashMap<String, PathBuf>,
    /// 上面那张大图的**原始像素**（id → (w,h)）。显示尺寸按它算：**不放大**超过这个像素。
    /// 与 `dims`（封面 preview 的尺寸，缩略图 chips 显示用）分开存，别混。
    preview_dims: HashMap<String, (u32, u32)>,
    /// 已安装页的排序方式（下标指向 `SORTS`）
    sort: usize,
    /// 逐壁纸的**宣传图**清单：id → 图片引用（来自 `project.json` 里嵌入的图片，
    /// 见 `promo` 模块；遍历抽取，不按名字特判）
    promo: HashMap<String, Vec<promo::Promo>>,
    /// 已经能上屏的宣传图：id → (清单下标 → 句柄 + 原始尺寸)
    promo_img: HashMap<String, HashMap<usize, PromoImg>>,
    /// 本帧要起的宣传图下载流（id）—— 走 `clip_out` / `scroll_out` 同一个「末尾统一发 Task」套路
    promo_out: Option<String>,
    /// 正在后台下宣传图的那条 id（防重复起线程）
    promo_fetching: Option<String>,
}

/// 一张已经能上屏的宣传图：句柄 + 原始像素尺寸。
/// 尺寸在**插入时**量好（`image::image_dimensions` / `ImageReader::into_dimensions` 只读文件头），
/// 界面每帧只做一次比例计算 —— 不每帧解码整张图。
#[derive(Clone)]
struct PromoImg {
    handle: widget::image::Handle,
    w: u32,
    h: u32,
}

#[derive(Debug, Clone)]
pub enum Message {
    LaunchUrl(String),
    ToggleContextPage(ContextPage),
    UpdateConfig(Config),

    // 控制面
    /// 自研内核：状态响应（一行 JSON）
    NativeStatus(String),
    /// 自研内核：某条请求的响应（请求行, 响应行）
    NativeReply(String, String),
    /// 性能档位：点了某一档（`powersave` / `balanced` / `quality`）
    PerfPreset(String),
    /// 高级：帧率 ±step（**本场景**的微调；不再动任何全局档）
    PerfFpsStep(i64),
    /// 帧率档：把**本场景**改回自适应（删掉这条场景的记录 ⇒ 默认 60；实测不够自动降 30）
    SceneFpsAuto,
    /// 帧率档：把**本场景**钉成某个值（60 / 30），并按这条场景记住
    SceneFpsFixed(u64),
    /// 高级：渲染尺寸 物理(锐) ↔ 逻辑(省 GPU)
    PerfScaleToggle,
    /// 高级：特效链 全部 ↔ 无
    PerfEffectsToggle,
    /// 高级：睡眠策略循环 从不 → 全屏时 → 最大化时
    PerfSleepCycle,
    /// 把当前四项立刻再下发一次（内核重启后不用重开界面）
    PerfPush,
    /// 取景模式（几何，不属于性能四项）：`cover`/`stretch`/`center`/`contain`
    FitModeSet(String),
    /// 画面色彩：改第 i 项（0亮度/1对比度/2饱和度/3色相/4色温/5伽马）加 delta
    ColorStep(usize, f32),
    /// 画面色彩：把第 i 项**直接设成**某个值（属性面板里的滑块用）
    ColorSet(usize, f32),
    /// 画面色彩：还原（单位值）
    ColorReset,
    /// 重启自研内核服务（`systemctl --user restart wallpaper-native-kernel`）
    RestartKernel,
    /// 色盘/色块**松手**（拖动结束）：立刻放行最后那个值并落盘。
    /// 拖动过程中每帧都会发 `PropColor`，节流后会合并；松手这一条保证"终值精确"。
    ColorDragEnd,
    /// 指针：把内核的鼠标位设到**画面中心**（`{"cmd":"mouse","x":0.5,"y":0.5}`）。
    /// 鼠标驱动的特效（X-Ray 透视 / 视差）会立刻跟着跳 ⇒ 一眼验证「控制面 → 场景 → 屏幕」通不通。
    PointerToCenter,
    Refresh,
    Apply(String),
    /// 诊断：窗口事件（`WPHUB_WIN_EVENTS=1` 才订阅）——用来确认"事件循环为什么返回"
    WinEvent(String),
    /// 诊断：指针按下/抬起及其**控件本地坐标**（`WPHUB_TRACE_MSG=1` 才订阅）
    Pointer(String),
    PauseToggle,
    SearchChanged(String),
    /// 发现页：按关键词打开 Steam 创意工坊搜索
    DiscoverSearch,
    DiscoverQueryChanged(String),

    // 设置（行为页）

    // 发现页（创意工坊）
    DiscoverRunSearch,
    DiscoverResult(Box<Result<workshop::SearchResult, String>>),
    /// 发现页·渐进式填充：后台下到**一张**缩略图（id + 原始字节），立刻上屏
    DiscoverThumb(String, Vec<u8>),
    /// 发现页·渐进式填充：这一页的缩略图批结束（带统计；None = 起线程失败）
    DiscoverThumbsDone(Box<Option<workshop::Stats>>),
    DiscoverSortCycle,
    DiscoverNextPage,
    /// 选中一条**没有下载到本地**的工坊条目：右侧面板显示工坊信息 + 工坊页链接（**不跳网页**）
    SelectWorkshop(String),
    /// 宣传图：后台下好一张（壁纸 id + 清单下标 + 原始字节）
    PromoImage(String, usize, Vec<u8>),
    /// 宣传图：这条壁纸的远程图批结束
    PromoDone(String),

    // 排除名单（0 应用ID / 1 标题 / 2 应用ID通配符 / 3 标题通配符）

    // 参数页（WE 壁纸自带的可调参数）

    // 页面切换（顶部横向标签）
    GoPage(Page),
    /// 主窗口尺寸变了（逻辑像素）：缩略图网格据此重排列数
    WindowResized(f32),
    /// 壁纸页·类型筛选（0 = 全部）—— 下拉选择，不再循环点击
    SetTypeFilter(usize),
    /// 壁纸页·标签筛选（0 = 全部）
    SetTagFilter(usize),

    // 层壳客户端（真正画桌面的那个进程）

    // ── WE 皮肤：详情面板 / 逐壁纸属性编辑 ──────────────────────────────
    /// 选中一条壁纸（点缩略图）。只选中，不应用 —— 和 WE 一致。
    Select(String),
    /// bool 属性（勾选框）
    PropBool(String, String, bool),
    /// slider 属性（滑块）
    PropNum(String, String, f64),
    /// color 属性（取色），值是 "r g b" 浮点串
    PropColor(String, String, String),
    /// combo 属性，值是选项的 value 串
    PropCombo(String, String, String),
    /// text 属性
    PropText(String, String, String),
    /// 展开/收起某个属性行的取色面板
    ToggleColorPicker(String),
    /// 「您的预设」：把当前壁纸这套属性改动存成一个预设
    PresetSave,
    /// 「您的预设」：读回这条壁纸存过的预设并**逐条应用**（走 set_prop：改完即时下发）
    PresetLoad,
    /// 「您的预设·应用到所有壁纸」：把当前这套改动写进**凡是也有同名同类属性**的每条壁纸
    PropsApplyAll,
    /// 「您的预设·分享 JSON」：把当前这套改动导出成 JSON（复制到剪贴板 + 日志留一份）
    PropsShareJson,
    /// 「您的预设·重置」：丢掉这条壁纸的所有改动，回到 project.json 的默认值（并下发）
    PropsReset,
    /// 顶栏「显示」：开关右侧详情面板
    TogglePanel,
    /// 设置页：开关「显示作者宣传图」（默认开；关掉 = 不显示、不联网。见 `Config::show_promo`）
    SetPromo(bool),
    /// **音频分析跟哪一路输出**：`"default"` = 跟随系统默认，其余 = sink 名（内核按名字解析）。
    AudioSinkSet(String),
    /// 音频分析：重新枚举可用输出（拿不到列表时那个「重新枚举」按钮）
    AudioSinksRefresh,
    /// 详情面板的「打开目录」：用文件管理器打开选中壁纸的目录
    OpenSelectedDir,
    /// 详情面板的「取消」：放弃选中
    Deselect,
    /// 已安装页的排序方式（下标指向 `SORTS`）
    SetSort(usize),
    /// 卡片尺寸档（0 小 / 1 中 / 2 大，见 [`CARD_SIZES`]）—— 网格据此重排列数
    SetCardSize(usize),
    /// **播放速度**滑杆（0.25..3.0，步进 0.05）—— 只乘内核的场景时间
    TimeScaleSet(f32),
    /// **音乐音量**滑杆（0..100）—— 调内核默认 sink 的 volume
    MusicVolumeSet(f32),
    /// 任一滑杆**松手**：立刻放行出站队列里的终值（不等节流窗口）
    SliderRelease,

    // 设置页：切换标签

}

// ── 调试：每秒统计各类 Message（`WPHUB_MSG_STATS=1` 打开；默认关、零开销）──────
// 用来定位「空闲时主线程为什么还在烧 CPU」：把每秒进来的消息按类型打一行。
// 空闲 100% 的根因（NativeReply 回执无条件再发一次 status ⇒ 自我循环）就是
// 靠它一眼看出来的：那一行里 NativeReply 有 ~8000/秒。
fn msg_stats_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("WPHUB_MSG_STATS")
            .map(|v| v != "0")
            .unwrap_or(false)
    })
}

const MSG_BUCKETS: [&str; 9] = [
    "NativeStatus",
    "NativeReply",
    "WindowResized",
    "UpdateConfig",
    "DiscoverThumb",
    "DiscoverResult",
    "DiscoverThumbsDone",
    "SubWatch",
    "其它",
];

fn msg_bucket(m: &Message) -> usize {
    match m {
        Message::NativeStatus(_) => 0,
        Message::NativeReply(..) => 1,
        Message::WindowResized(_) => 2,
        Message::UpdateConfig(_) => 3,
        Message::DiscoverThumb(..) => 4,
        Message::DiscoverResult(_) => 5,
        Message::DiscoverThumbsDone(_) => 6,
                _ => 8,
    }
}

static MSG_COUNTS: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];

/// 指针最后一次的窗口本地坐标（f32 位模式存进原子量）——只给 `WPHUB_TRACE_MSG` 诊断用。
static POS_X: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static POS_Y: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// 诊断（`WPHUB_TRACE_MSG=1`）：把每条进 update 的消息打一行名字 ——
/// 用来回答"我点了那个按钮，到底有没有一条 Message 进来"。
fn trace_msg_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("WPHUB_TRACE_MSG")
            .map(|v| v != "0")
            .unwrap_or(false)
    })
}

fn msg_name(m: &Message) -> &'static str {
    match m {
        Message::Select(_) => "Select",
        Message::Apply(_) => "Apply",
        Message::Deselect => "Deselect",
        Message::PropNum(..) => "PropNum",
        Message::PropBool(..) => "PropBool",
        Message::PropText(..) => "PropText",
        Message::PropCombo(..) => "PropCombo",
        Message::PropColor(..) => "PropColor",
        Message::ColorSet(..) => "ColorSet",
        Message::ColorStep(..) => "ColorStep",
        Message::ColorReset => "ColorReset",
        Message::ColorDragEnd => "ColorDragEnd",
        Message::TogglePanel => "TogglePanel",
        Message::GoPage(_) => "GoPage",
        Message::SetPromo(_) => "SetPromo",
        Message::Refresh => "Refresh",
        Message::PauseToggle => "PauseToggle",
        Message::NativeStatus(_) => "NativeStatus",
        Message::NativeReply(..) => "NativeReply",
        Message::WindowResized(_) => "WindowResized",
        Message::WinEvent(_) => "WinEvent",
        Message::UpdateConfig(_) => "UpdateConfig",
        Message::PresetSave => "PresetSave",
        Message::PresetLoad => "PresetLoad",
        Message::PropsApplyAll => "PropsApplyAll",
        Message::PropsReset => "PropsReset",
        Message::ToggleColorPicker(_) => "ToggleColorPicker",
        Message::AudioSinkSet(_) => "AudioSinkSet",
        Message::RestartKernel => "RestartKernel",
        _ => "其它",
    }
}

fn msg_stats_bump(m: &Message) {
    MSG_COUNTS[msg_bucket(m)].fetch_add(1, Ordering::Relaxed);
}

/// 单次 `update` 的**最坏耗时**（微秒）—— 用来回答"UI 到底卡没卡"：
/// 只要它远小于一帧（60fps = 16.7ms），用户就感觉不到卡顿。
static UPD_MAX_US: AtomicU64 = AtomicU64::new(0);

/// 放在 `update` 门口；靠 Drop 记录耗时（早退分支也能记上）。
struct UpdateTimer(std::time::Instant);

impl Drop for UpdateTimer {
    fn drop(&mut self) {
        let us = self.0.elapsed().as_micros() as u64;
        UPD_MAX_US.fetch_max(us, Ordering::Relaxed);
    }
}

fn upd_max_us_take() -> u64 {
    UPD_MAX_US.swap(0, Ordering::Relaxed)
}

/// 每秒打印一次（只在 `WPHUB_MSG_STATS=1` 时启动）。
fn spawn_msg_stats_printer() {
    std::thread::Builder::new()
        .name("msg-stats".into())
        .spawn(|| loop {
            std::thread::sleep(Duration::from_secs(1));
            let parts: Vec<String> = MSG_BUCKETS
                .iter()
                .zip(MSG_COUNTS.iter())
                .map(|(name, c)| format!("{name}={}", c.swap(0, Ordering::Relaxed)))
                .collect();
            eprintln!("[msg/s] {}", parts.join(" "));
        })
        .ok();
}

impl cosmic::Application for AppModel {
    type Executor = cosmic::executor::Default;
    type Flags = String;
    type Message = Message;
    const APP_ID: &'static str = "io.github.wallpaperhub.WallpaperHub";

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    /// 窗口"被请求关闭"必须留下**证据**。
    /// 背景：应用曾经"每 ~10 秒静默退出一次"（journal 里只有 systemd 的
    /// `Consumed ... over 10.186s wall clock time`，没有 panic、没有 error、退出码 0）。
    /// 代码里没有任何 `std::process::exit`，说明当时唯一的出口是 **iced 事件循环正常返回**
    /// 也就是"最后一个窗口被关掉了"。到底是谁要关它（合成器发 `xdg_toplevel.close`、
    /// 还是用户点 X）以前完全看不见；这里把它打成一行日志，
    /// 配合 main.rs 里 "[exit] cosmic::app::run 返回 ..." 一起看，静默退出就再也藏不住。
    /// 返回 `None` = 照旧允许关闭（不改变用户点 X 的行为）。
    fn on_close_requested(&self, id: cosmic::iced::window::Id) -> Option<Self::Message> {
        eprintln!("[close] 窗口 {id:?} 收到关闭请求（若这条日志出现在'自己退出'之前，就是它导致的）");
        None
    }

    fn init(
        core: cosmic::Core,
        flags: Self::Flags,
    ) -> (Self, Task<cosmic::Action<Self::Message>>) {
        let mut nav = nav_bar::Model::default();

        nav.insert()
            .text("壁纸")
            .data::<Page>(Page::Wallpapers)
            .icon(icon::from_name("applications-graphics-symbolic"));

        nav.insert()
            .text("发现")
            .data::<Page>(Page::Discover)
            .icon(icon::from_name("system-search-symbolic"));

        nav.insert()
            .text("设置")
            .data::<Page>(Page::Settings)
            .icon(icon::from_name("preferences-system-symbolic"));

        nav.insert()
            .text("创意工坊")
            .data::<Page>(Page::Workshop)
            .icon(icon::from_name("folder-download-symbolic"));

        // page <名字> 直接切到指定页。位置：0 已安装 / 1 发现 / 2 设置 / 3 创意工坊
        let args: Vec<&str> = flags.split_whitespace().collect();
        let page = args
            .iter()
            .position(|a| *a == "--page")
            .and_then(|i| args.get(i + 1))
            .copied()
            .unwrap_or("");
        let position = match page {
            "discover" => 1,
            "workshop" => 3,
            // 兼容旧名字：行为/显示器/状态/参数 都落到设置页
            "settings" | "behavior" | "displays" | "status" | "props" => 2,
            _ => 0,
        };
        nav.activate_position(position);
        let pending_native_apply_first = args.contains(&"--native-apply-first");
        // select-first / --expand-first：启动就选中第一条（截图验证右侧详情面板用；
        // 本机没有输入模拟工具，走的是和点缩略图完全相同的代码路径）
        let select_first = args.contains(&"--select-first") || args.contains(&"--expand-first");
        // select-id <工坊id>：启动就选中指定的那一条（截图验证某个具体壁纸的属性面板用）
        let select_id = args
            .iter()
            .position(|a| *a == "--select-id")
            .and_then(|i| args.get(i + 1))
            .map(|s| s.to_string());

        // 验收专用（本机 Wayland 下没有输入模拟工具）：
        // card-size 小|中|大|0..2 卡片档（走和「尺寸」滑杆**完全相同**的 Message）
        // time-scale <0.25..3> 单次设播放速度（走 TimeScaleSet）
        // volume <0..100> 单次设音乐音量（走 MusicVolumeSet）
        let arg_of = |name: &str| -> Option<String> {
            args.iter()
                .position(|a| *a == name)
                .and_then(|i| args.get(i + 1))
                .map(|s| s.to_string())
        };
        let card_size_arg = arg_of("--card-size").and_then(|v| match v.trim() {
            "小" | "small" | "0" => Some(0usize),
            "中" | "mid" | "medium" | "1" => Some(1),
            "大" | "large" | "2" => Some(2),
            _ => None,
        });
        let time_scale_arg = arg_of("--time-scale").and_then(|v| v.trim().parse::<f32>().ok());
        let volume_arg = arg_of("--volume").and_then(|v| v.trim().parse::<f32>().ok());
        // open-color <属性键>：启动就把这条属性**展开成圆形色盘**（只给截图用；
        // 走的是和点一下色点完全相同的 `open_color` 状态，没有第二套逻辑）
        let open_color = args
            .iter()
            .position(|a| *a == "--open-color")
            .and_then(|i| args.get(i + 1))
            .map(|s| s.to_string());
        // perf-preset <名字>：脚本化验证用 —— 走**和设置页按钮同一条** update 路径
        // （另写一份下发逻辑就证明不了界面那条路通）
        let perf_preset = args
            .iter()
            .position(|a| *a == "--perf-preset")
            .and_then(|i| args.get(i + 1))
            .map(|s| s.to_string());
        // perf-msg <名字>：脚本化验证「高级四项」用的 —— 每个名字 = 按一下某个按钮的那条 Message
        // （同一个 handler；可用 fps-5 / fps+5 / scale / effects / sleep / push / fit-cover|fit-stretch|fit-center|fit-contain，可重复，按顺序应用）
        let perf_msgs: Vec<String> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| **a == "--perf-msg")
            .filter_map(|(i, _)| args.get(i + 1))
            .map(|s| s.to_string())
            .collect();

        // toggle-prop <key>：脚本化验证「UI 改属性 → 内核 → 屏幕」闭环用（本机没有 wtype/ydotool，
        // 无法真的点勾选框）。对**第一条**壁纸把该 bool 属性置为 true，走 `Message::PropBool` ——
        // 与手点勾选框**完全相同**的那条分支。
        let toggle_props: Vec<String> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| **a == "--toggle-prop")
            .filter_map(|(i, _)| args.get(i + 1))
            .map(|s| s.to_string())
            .collect();

        // apply-id <工坊id>：启动就把**指定这一条**下发给内核（走与详情面板 [应用] 按钮
        // **完全相同**的 `Message::Apply`）。用来验证「切壁纸时该条的分槽色彩跟着下来」。
        let apply_id = args
            .iter()
            .position(|a| *a == "--apply-id")
            .and_then(|i| args.get(i + 1))
            .map(|s| s.to_string());
        // color-set <项>:<值>：把**当前选中**那条壁纸的画面色彩第 <项> 项直接设成 <值>
        // （走与属性面板滑块完全相同的 `Message::ColorSet`）。可重复、按顺序应用。
        // 项：0 亮度 / 1 对比 / 2 饱和 / 3 色相(弧度) / 4 色温 / 5 伽马。
        let color_sets: Vec<(usize, f32)> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| **a == "--color-set")
            .filter_map(|(i, _)| args.get(i + 1))
            .filter_map(|s| {
                let (idx, val) = s.split_once(':')?;
                Some((idx.parse::<usize>().ok()?, val.parse::<f32>().ok()?))
            })
            .collect();

        // preset-save / --preset-load / --props-apply-all / --props-share / --props-reset：
        // 「您的预设」那一排按钮的脚本化入口（本机没有 wtype/ydotool ⇒ 点不了按钮）。
        // 每条都走**和点按钮完全相同**的那条 Message 分支。
        let preset_flags: Vec<Message> = [
            ("--preset-save", Message::PresetSave),
            ("--preset-load", Message::PresetLoad),
            ("--props-apply-all", Message::PropsApplyAll),
            ("--props-share", Message::PropsShareJson),
            ("--props-reset", Message::PropsReset),
        ]
        .into_iter()
        .filter(|(flag, _)| args.contains(flag))
        .map(|(_, msg)| msg)
        .collect();
        // props-json：只把「分享 JSON」那份文本打到日志（不碰剪贴板、不改任何状态）
        let want_props_json = args.contains(&"--props-json");

        let about = About::default()
            .name(fl!("app-title"))
            .icon(widget::icon::from_svg_bytes(APP_ICON))
            .version(env!("CARGO_PKG_VERSION"))
            .links([(fl!("repository"), REPOSITORY)])
            .license(env!("CARGO_PKG_LICENSE"));

        // 之前这里会"顺手拉起 waywallen 的层壳客户端"。waywallen 已彻底移除：显示由自研内核负责，
        // 而内核是 systemd 用户服务（登录即起、懒创建表面），所以启动阶段不需要再拉任何东西。

        // 保留句柄：`self.config` 只是它的值，写回（持久化）必须用句柄
        let (config, config_ctx) = match cosmic_config::Config::new(Self::APP_ID, Config::VERSION) {
            Ok(context) => {
                let cfg = match Config::get_entry(&context) {
                    Ok(config) => config,
                    Err((_errors, config)) => config,
                };
                (cfg, Some(context))
            }
            Err(err) => {
                eprintln!("读不到 cosmic-config（{err}）：这次的改动不会被持久化");
                (Config::default(), None)
            }
        };

        // 帧率**不再是全局的**。旧配置里那个全局 `fps` 键（曾经被拿来每次应用
        // 都下发、把场景自己的帧率盖成 30）现在既不被读取也不再写入 ⇒ 这里如实说一句。
        match legacy_global_fps(Self::APP_ID) {
            Some(old) => eprintln!(
                "[fps] 旧配置里的全局 fps={old} **不再生效**：帧率已改为逐场景（默认 {}；可按场景钉 {}）。\
                 这条老值不会被下发，也不会覆盖任何场景",
                config::SCENE_FPS_DEFAULT,
                config::SCENE_FPS_HEAVY
            ),
            None => eprintln!(
                "[fps] 帧率是**逐场景**的：默认 {}；可按场景钉 {}（重场景 GPU 会满）；\
                 没记过的场景自适应（实测连续 {} 次低于 {:.0}、且机器上没有别的负载时才降 {}）",
                config::SCENE_FPS_DEFAULT,
                config::SCENE_FPS_HEAVY,
                config::ADAPT_FAIL_STREAK,
                config::ADAPT_FAIL_FPS,
                config::SCENE_FPS_HEAVY
            ),
        }

        let start_native = config.backend_native;
        // 连上内核时"自动推进上次应用"的总开关。
        // 默认取持久配置（`Config::auto_apply_last`，默认 true = "内核空闲时补一条"）；
        // `--no-auto-apply` / `WPHUB_NO_AUTO_APPLY=1` 本次会话强制关（一个字也不推）。
        // 注意：**无论开关如何，内核正在播的那条永远不会被换掉**（见 `decide_auto_apply`）。
        let no_auto_apply = args.contains(&"--no-auto-apply")
            || matches!(
                std::env::var("WPHUB_NO_AUTO_APPLY").ok().as_deref(),
                Some("1" | "on" | "true" | "yes")
            );
        let auto_apply_enabled = config.auto_apply_last && !no_auto_apply;
        eprintln!(
            "[auto-apply] 连上内核时自动推进「上次应用」= {}（配置={} · 本次会话{}）\
             —— 内核正在播的**永不换掉**，只在它空闲时补一条",
            if auto_apply_enabled {
                "允许（内核空闲才推）"
            } else {
                "关闭（一个字也不推）"
            },
            config.auto_apply_last,
            if no_auto_apply {
                "被 --no-auto-apply 关掉"
            } else {
                "未干预"
            }
        );
        let mut app = AppModel {
            core,
            context_page: ContextPage::default(),
            about,
            nav,
            key_binds: HashMap::new(),
            config,
            config_ctx,
            error: None,
            wallpapers: Vec::new(),
            native: None,
            native_note: None,
            backend_native: start_native,
            native_last: None,
            kernel_perf: None,
            // 逐场景帧率表（读不到就是空的 ⇒ 全部走"默认 60 + 自适应"）
            scene_fps: load_scene_fps(),
            adapt_streak: 0,
            adapt_done: false,
            adapt_recover_streak: 0,
            adapt_downgraded_at: None,
            adapt_span: None,
            adapt_scene: None,
            frame_sample: None,
            adapt_note: None,
            audio_sinks: Vec::new(),
            audio_sinks_err: None,
            audio_sinks_asked: false,
            audio_sink_seen: None,
            // 内核无状态：启动时先当"没同步过"，第一次连上内核就补发一次界面里的四项
            perf_synced: false,
            auto_apply_enabled,
            prop_dirty: false,
            prop_saved_at: None,
            search: String::new(),
            discover_query: String::new(),
            pending_native_apply_first,
            pending_apply_id: apply_id,
            type_filter: 0,
            tag_filter: 0,
            known_types: Vec::new(),
            known_tags: Vec::new(),
            discover_items: Vec::new(),
            discover_links: 0,
            discover_dedup: 0,
            discover_thumbs: HashMap::new(),
            discover_thumb_raw: HashMap::new(),
            discover_loading: false,
            discover_thumbs_loading: false,
            discover_thumb_stats: None,
            discover_t0: None,
            discover_error: None,
            discover_page: 1,
            discover_sort: 0,
            // 卡片尺寸 / 播放速度 / 音乐音量（后两项的初值：速度 1.0 = 官方口径；
            // 音量**读一次**当前 sink 的实际值，见 `read_sink_volume`）
            card_idx: card_size_arg.unwrap_or(CARD_DEFAULT),
            time_scale: 1.0,
            music_vol: read_sink_volume(),
            // workshop-select <工坊id>：发现页结果一到就选中这条（截图用，等价于点缩略图）
            pending_workshop_select: args
                .iter()
                .position(|a| *a == "--workshop-select")
                .and_then(|i| args.get(i + 1))
                .map(|s| s.to_string()),
            win_w: 1280.0,
            cpu_sample: None,
            cpu_pct: None,
            selected: None,
            open_color: open_color.clone(),
            pending_open_color: open_color,
            prop_edits: load_overrides(),
            prop_text: HashMap::new(),
            clip_out: None,
            show_panel: true,
            panel_bottom: args.contains(&"--panel-bottom"),
            settings_bottom: args.contains(&"--settings-bottom"),
            // panel-scroll <像素>：截图时把面板滚到想看的那一段
            panel_scroll_req: args
                .iter()
                .position(|a| *a == "--panel-scroll")
                .and_then(|i| args.get(i + 1))
                .and_then(|s| s.parse::<f32>().ok()),
            panel_scroll_left: 6,
            scroll_out: None,
            toggle_props: toggle_props.clone(),
            toggle_redone: false,
            dims: HashMap::new(),
            preview_src: HashMap::new(),
            preview_dims: HashMap::new(),
            sort: 0,
            promo: HashMap::new(),
            promo_img: HashMap::new(),
            promo_out: None,
            promo_fetching: None,
        };

        // 「宣传图」**默认开**（见 `Config::show_promo`：用户要"放回来/默认展示"）。
        // 本次会话想临时改：
        // promo / --no-promo（启动参数） 或 WPHUB_PROMO=1 / =0（环境变量，供脚本用）
        // 只改本次会话的内存值；要长期生效用设置页那个开关（它会持久化）。
        if args.contains(&"--promo") {
            app.config.show_promo = true;
        }
        if args.contains(&"--no-promo") {
            app.config.show_promo = false;
        }
        match std::env::var("WPHUB_PROMO").ok().and_then(|v| match v.as_str() {
            "1" | "on" | "true" | "yes" => Some(true),
            "0" | "off" | "false" | "no" => Some(false),
            _ => None,
        }) {
            Some(true) => app.config.show_promo = true,
            Some(false) => app.config.show_promo = false,
            None => {}
        }
        eprintln!(
            "[promo] 本次会话宣传图显示 = {}（默认开；--promo/--no-promo、WPHUB_PROMO 或设置页可关）",
            if app.config.show_promo { "开" } else { "关" }
        );
        // workshop-query <关键词>：**只给截图用** —— 启动那一次工坊搜索用这个关键词
        // （`Message::DiscoverRunSearch` 走的是和点 [搜索] 完全相同的分支）。
        // 为什么需要它：本机没有输入模拟工具，点不了/打不了搜索框（同 `--workshop-select` 的理由）。
        match args
            .iter()
            .position(|a| *a == "--workshop-query")
            .and_then(|i| args.get(i + 1))
        {
            Some(q) => {
                app.discover_query = q.to_string();
                eprintln!("[--workshop-query] 启动时那次工坊搜索的关键词 = {q}");
            }
            None => {}
        }
        // 应用内主题：**跟随系统**（COSMIC 上原生；其它桌面退化为中性，见 `we_ui::theme`）。
        // 只作用于本程序，不写系统配置、不改系统主题。
        let mut command = Task::batch(vec![
            app.update_title(),
            Task::done(cosmic::app::Action::AppThemeChange(we_ui::theme()))
                .map(cosmic::Action::Cosmic),
        ]);
        if position == 1 || position == 3 {
            let search = app.update(Message::DiscoverRunSearch);
            command = Task::batch(vec![command, search]);
        }
        // perf-preset <名字>：走和设置页按钮完全相同的那条 update 分支
        // （真正的下发要等内核连上：request_async 会失败并留下 perf_synced=false，
        // 第一次 NativeStatus 时自动补发 —— 这就是"内核无状态、界面负责同步"那条路）
        if let Some(name) = perf_preset {
            let preset = app.update(Message::PerfPreset(name));
            command = Task::batch(vec![command, preset]);
        }
        // 验收用的单次下发（走与滑杆**完全相同**的 Message 分支）
        if let Some(v) = time_scale_arg {
            let task = app.update(Message::TimeScaleSet(v));
            command = Task::batch(vec![command, task]);
        }
        if let Some(v) = volume_arg {
            let task = app.update(Message::MusicVolumeSet(v));
            command = Task::batch(vec![command, task]);
        }
        if let Some(i) = card_size_arg {
            let task = app.update(Message::SetCardSize(i));
            command = Task::batch(vec![command, task]);
        }
        // perf-msg 的每一条都走和对应按钮完全相同的 update 分支
        for name in perf_msgs {
            match perf_msg_from_name(&name) {
                Some(msg) => {
                    let task = app.update(msg);
                    command = Task::batch(vec![command, task]);
                }
                None => eprintln!(
                    "[--perf-msg] 不认识的名字：{name}（可用 fps-5 / fps+5 / scale / effects / sleep / push）"
                ),
            }
        }
        // 启动就先用**本地扫描**把列表填上：扫的是**本地库根目录**（默认 `~/Templates`）
        // 加可选的 Steam 工坊目录 —— 这个程序不需要 Steam 客户端，壁纸在哪就扫哪。
        app.sync_library_roots();
        app.adopt_wallpapers(local::scan_with(&app.config.library_dirs));
        // 启动参数：选中第一条 / 指定的那一条（截图/验证右侧详情面板）
        // 放在 --toggle-prop / --preset-* 之前：那两组参数都作用于「当前选中的那条」。
        // 走 `Message::Select` 这条**和点缩略图完全相同**的路（而不是直接调 select）：
        // 选中会顺手起「宣传图」的后台下载流，那条流是在 update 的末尾统一发出去的，
        // 绕过 update 就没人把它交出去（*首次实测踩过*）。
        if let Some(id) = select_id {
            if app.wallpapers.iter().any(|w| w.id == id) {
                let sel = app.update(Message::Select(id));
                command = Task::batch(vec![command, sel]);
            } else {
                eprintln!("[--select-id] 列表里没有 {id}");
            }
        } else if select_first
            && let Some(first) = app.wallpapers.first().map(|w| w.id.clone())
        {
            let sel = app.update(Message::Select(first));
            command = Task::batch(vec![command, sel]);
        }

        // apply-id <id>：**不在这里下发**（此刻 socket 还没就绪）—— 记在 `pending_apply_id`，
        // 第一次连上内核时补跑一遍 `apply_native`（和 `--toggle-prop` 的补跑同一个套路）。
        // color-set <项>:<值>：对**当前选中**那条壁纸设色彩（= 拖一下属性面板里的滑块）。
        // 此刻内核还没连上 ⇒ `set_color` 只落盘（这正是分槽的意义：先记住，切过去时下发）；
        // 连上之后由 `NativeStatus` 的同步那一段把这条壁纸的色彩补发出去。
        for (i, v) in color_sets {
            let t = app.update(Message::ColorSet(i, v));
            command = Task::batch(vec![command, t]);
        }

        // toggle-prop：对**当前选中**（没有就第一条）壁纸把该 bool 属性置为 true（与点勾选框同一条分支）
        for key in toggle_props {
            let id = app
                .selected
                .clone()
                .or_else(|| app.wallpapers.first().map(|w| w.id.clone()));
            if let Some(id) = id {
                let task = app.update(Message::PropBool(id, key.clone(), true));
                command = Task::batch(vec![command, task]);
            }
        }

        // preset-save / --preset-load / --props-apply-all / --props-share / --props-reset：
        // 脚本化验证「您的预设」那一排按钮 —— 每个都走**和点按钮完全相同**的那条 Message 分支。
        for msg in preset_flags {
            let task = app.update(msg);
            command = Task::batch(vec![command, task]);
        }

        // props-json：把「分享 JSON」那**同一份**文本打到日志（同一个生成函数，不是另写一份）
        if want_props_json
            && let Some(id) = app.selected.clone()
        {
            println!("[--props-json] {}", app.props_json(&id));
        }

        // 调试用：`WPHUB_MSG_STATS=1` 时每秒打一行各类 Message 计数
        if msg_stats_on() {
            spawn_msg_stats_printer();
        }

        (app, command)
    }

    fn header_start(&self) -> Vec<Element<'_, Self::Message>> {
        let menu_bar = menu::bar(vec![menu::Tree::with_children(
            menu::root(fl!("app-title")).apply(Element::from),
            menu::items(
                &self.key_binds,
                vec![menu::Item::Button(fl!("about"), None, MenuAction::About)],
            ),
        )]);

        vec![menu_bar.into()]
    }

    /// 不用 libcosmic 的侧边导航栏：三个页面改成顶部**横向标签**（见 `view_tabs`）。
    /// `nav` 只留作"当前是哪一页"的状态（`activate_position` / `active_data`）。
    fn nav_model(&self) -> Option<&nav_bar::Model> {
        None
    }

    fn context_drawer(&self) -> Option<context_drawer::ContextDrawer<'_, Self::Message>> {
        if !self.core.window.show_context {
            return None;
        }

        Some(match self.context_page {
            ContextPage::About => context_drawer::about(
                &self.about,
                |url| Message::LaunchUrl(url.to_string()),
                Message::ToggleContextPage(ContextPage::About),
            ),
        })
    }

    fn view(&self) -> Element<'_, Self::Message> {
        let spacing = cosmic::theme::spacing();
        let page = self
            .nav
            .active_data::<Page>()
            .copied()
            .unwrap_or(Page::Wallpapers);

        // 顶栏：左 = 三个标签（已安装 / 发现 / 创意工坊），右 = 灰底小按钮组
        let top = widget::row::with_capacity(2)
            .push(self.view_tabs(page))
            .push(
                widget::container(self.view_top_actions(page))
                    .width(Length::Fill)
                    .align_x(Horizontal::Right),
            )
            .spacing(spacing.space_s)
            .align_y(Alignment::Center);

        let content: Element<_> = match page {
            Page::Wallpapers => self.view_wallpapers(),
            Page::Discover => self.view_discover(),
            Page::Workshop => self.view_workshop(),
            Page::Settings => self.view_settings(),
        };
        let left = widget::container(content)
            .width(Length::Fill)
            .height(Length::Fill);

        // 左 = 当前页，右 = 详情面板（常驻，可用顶栏「显示」关掉）
        let body: Element<_> = if self.show_panel {
            widget::row::with_capacity(2)
                .push(left)
                .push(
                    widget::container(self.view_detail())
                        .width(Length::Fixed(panel_width(self.win_w, self.card_w())))
                        .height(Length::Fill),
                )
                .spacing(spacing.space_s)
                .height(Length::Fill)
                .into()
        } else {
            left.into()
        };

        let mut col = widget::column::with_capacity(4)
            .push(top)
            .push(we_ui::divider())
            .push(body);
        // 底部只有状态条了：原来那行「播放列表」（读取/保存/配置/+添加壁纸 + 条数摘要）已去掉
        // （本程序不做壁纸、也没有播放列表这个功能；详情面板的 [应用]/[确认] 仍在下发链路里）。
        col = col.push(self.view_status_bar());
        // 窗口底：**不刷不透明色**，交给 libcosmic 那层「主题半透明背景」
        // （`background(theme.transparent).base` + 窗口圆角 + 描边，见 we_ui::window_style）。
        // 只有框架不画那层时才自己补一层半透明底，避免整窗全透。
        let frame_paints_bg = self.core.window.content_container;
        widget::container(
            col.spacing(spacing.space_s)
                .padding(spacing.space_s)
                .height(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .style(if frame_paints_bg {
            we_ui::window_style
        } else {
            we_ui::window_style_fallback
        })
        .into()
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        let mut subs = vec![
            self.core().watch_config::<Config>(Self::APP_ID).map(|update| {
                Message::UpdateConfig(update.config)
            }),
            native::subscription(),
            // 窗口尺寸：方块缩略图按它自动决定每行放几个
            cosmic::iced::window::resize_events().map(|(_id, size)| Message::WindowResized(size.width)),
        ];
        // 诊断（`WPHUB_WIN_EVENTS=1`）：把每一个窗口事件打出来 —— 事件循环"正常返回"的
        // 唯一常见原因就是**窗口被关掉**，这里能看见是 CloseRequested 还是别的。
        if std::env::var("WPHUB_WIN_EVENTS").ok().as_deref() == Some("1") {
            subs.push(
                cosmic::iced::window::events()
                    .map(|(id, ev)| Message::WinEvent(format!("window {id:?}: {ev:?}"))),
            );
        }
        // 诊断（`WPHUB_TRACE_MSG=1`）：指针按下落在**窗口本地坐标**的哪里、被谁吃掉。
        // 按钮画在那儿却点不动时，这一行能直接回答"事件到没到、到的是哪个坐标"。
        if trace_msg_on() {
            subs.push(cosmic::iced::event::listen_with(|event, status, _id| {
                match event {
                    cosmic::iced::Event::Mouse(cosmic::iced::mouse::Event::CursorMoved {
                        position,
                    }) => {
                        POS_X.store(position.x.to_bits(), Ordering::Relaxed);
                        POS_Y.store(position.y.to_bits(), Ordering::Relaxed);
                        None
                    }
                    cosmic::iced::Event::Mouse(cosmic::iced::mouse::Event::CursorLeft) => {
                        Some(Message::Pointer("CursorLeft".into()))
                    }
                    cosmic::iced::Event::Mouse(cosmic::iced::mouse::Event::CursorEntered) => {
                        Some(Message::Pointer("CursorEntered".into()))
                    }
                    cosmic::iced::Event::Mouse(
                        cosmic::iced::mouse::Event::ButtonPressed(b),
                    ) => Some(Message::Pointer(format!(
                        "按下 {b:?} @ ({:.0},{:.0}) status={status:?}",
                        f32::from_bits(POS_X.load(Ordering::Relaxed)),
                        f32::from_bits(POS_Y.load(Ordering::Relaxed)),
                    ))),
                    cosmic::iced::Event::Mouse(
                        cosmic::iced::mouse::Event::ButtonReleased(b),
                    ) => Some(Message::Pointer(format!(
                        "抬起 {b:?} @ ({:.0},{:.0}) status={status:?}",
                        f32::from_bits(POS_X.load(Ordering::Relaxed)),
                        f32::from_bits(POS_Y.load(Ordering::Relaxed)),
                    ))),
                    _ => None,
                }
            }));
        }
        Subscription::batch(subs)
    }

    fn update(&mut self, message: Self::Message) -> Task<cosmic::Action<Self::Message>> {
        let _upd_timer = msg_stats_on().then(|| UpdateTimer(std::time::Instant::now()));
        if msg_stats_on() {
            msg_stats_bump(&message);
        }
        if trace_msg_on() {
            let name = msg_name(&message);
            // 状态回执/尺寸这类每 3 秒来的别刷屏
            if !matches!(name, "NativeStatus" | "WindowResized" | "WinEvent") {
                eprintln!("[msg] {name}");
            }
        }
        match message {
            Message::WinEvent(s) => eprintln!("[win-event] {s}"),
            Message::Pointer(s) => eprintln!("[pointer] {s}"),
            Message::LaunchUrl(url) => {
                if let Err(err) = open::that_detached(&url) {
                    eprintln!("failed to open {url:?}: {err}");
                }
            }

            Message::ToggleContextPage(context_page) => {
                if self.context_page == context_page {
                    self.core.window.show_context = !self.core.window.show_context;
                } else {
                    self.context_page = context_page;
                    self.core.window.show_context = true;
                }
            }

            Message::UpdateConfig(config) => {
                self.config = config;
            }

            // ── 自研内核（第二后端）──────────────────────────────────────────
            Message::NativeStatus(raw) => {
                self.native = native::Status::parse(&raw);
                // 拖动期间被节流的落盘在这里补一次（3 秒一次的网，保证"最后一下没松手"也不丢）
                if self.prop_dirty {
                    self.persist_props(true);
                }
                // 内核连上了、当前正停在设置页、这次还没要到 sink 列表 ⇒ 在后台补要一次。
                // （只补一次：拿到回执后 `audio_sinks_asked` 就是 true；内核报"无法枚举"也**不重试**，
                // 用户点「重新枚举」或重新进设置页才会再来。）
                if self.native.is_some()
                    && !self.audio_sinks_asked
                    && self.nav.active_data::<Page>().copied() == Some(Page::Settings)
                {
                    self.audio_sinks_asked = native::request_async(native::cmd_audio_sinks());
                }
                // `--toggle-prop` 补跑一次：连上内核之后这一下才真的会下发（见字段注释）
                if !self.toggle_redone && !self.toggle_props.is_empty() && self.native.is_some() {
                    self.toggle_redone = true;
                    let id = self
                        .selected
                        .clone()
                        .or_else(|| self.wallpapers.first().map(|w| w.id.clone()));
                    if let Some(id) = id {
                        for key in self.toggle_props.clone() {
                            self.set_prop(&id, &key, serde_json::Value::Bool(true));
                        }
                    }
                }
                // `--apply-id` 的**补跑**：init 里 socket 还没就绪（那一下只会落盘/记地址），
                // 连上内核之后这一遍才真的走「切壁纸 → 下发内容 + 整套属性 + 这条壁纸的色彩」。
                if self.native.is_some()
                    && let Some(id) = self.pending_apply_id.take()
                {
                    if self.wallpapers.iter().any(|w| w.id == id) {
                        self.apply_native(&id);
                    } else {
                        eprintln!("[--apply-id] 列表里没有 {id}");
                    }
                }
                // 截图用：`--panel-scroll` 的滚动请求在这里重发几次（布局完成前发的会被丢掉）
                if let Some(px) = self.panel_scroll_req
                    && self.panel_scroll_left > 0
                {
                    self.panel_scroll_left -= 1;
                    self.scroll_out = Some(px);
                }
                // 角落资源条：两次采样之间的 CPU 时间差 ÷ 真实间隔 = 占用率
                if let Some(n) = self.native.as_ref() {
                    let now = std::time::Instant::now();
                    if let Some((prev_cpu, prev_at)) = self.cpu_sample {
                        let dt = now.duration_since(prev_at).as_secs_f64();
                        if dt > 0.2 {
                            self.cpu_pct = Some(((n.cpu_s - prev_cpu) / dt * 100.0).clamp(0.0, 999.0));
                        }
                    }
                    self.cpu_sample = Some((n.cpu_s, now));
                }
                // 自适应：用内核累计帧数的两次采样算实际帧率；持续明显跑不到就把本场景降到 30
                // （只在"自动档"上动手，且降的时候会说话 —— 见 `tick_adaptive`）。
                self.tick_adaptive();
                // 内核无状态：重启后不记得界面设过什么 ⇒ 每次「从连不上变成连上」都补发一次四项。
                // 界面侧的改动是即时下发的，所以只在还没同步过时补，不每 3 秒重发一遍。
                match self.native.is_some() {
                    true if !self.perf_synced => {
                        // 帧率按**这条场景**算（逐场景表 → 默认 60），再不是一把全局的尺；
                        // 日志里会写明来源。三项（尺寸/特效/睡眠）仍是全局偏好。
                        let sid = self.current_scene_id();
                        self.push_scene_perf(sid.as_deref(), None, "启动同步");
                        // 取景模式也补一次：它是几何设置（跟四项分开存），内核重启后同样得知道
                        let fit = if self.config.fit_mode.is_empty() {
                            "cover".to_string()
                        } else {
                            self.config.fit_mode.clone()
                        };
                        native::request_async(native::cmd_set_fit(&fit));
                        // 播放速度 / 音乐音量也补一次（内核重启后不记得；音量按界面记的实际值）
                        native::request_async(native::cmd_set_time_scale(self.time_scale));
                        if let Some(v) = self.music_vol {
                            native::request_async(native::cmd_set_volume(v));
                        }
                        // 内核里**已经在播**的壁纸是权威 —— 启动同步**绝不**把它换掉。
                        // 这是真实困扰的修法：为装/装修复而重启内核时，桌面壁纸不该被 app
                        // "上次应用"那条换走（实测 X-Ray 2564205376 被换成 1931934283）。
                        // 只有内核**空闲**（没内容 / 没在播）时才允许推进"上次应用"；
                        // `--no-auto-apply` / 配置关掉则**一个字也不推**。判据是纯函数 `decide_auto_apply`。
                        let kernel_busy = kernel_has_content(self.native.as_ref());
                        let has_last = self.native_last.is_some();
                        match decide_auto_apply(self.auto_apply_enabled, kernel_busy, has_last) {
                            AutoApply::PushLast => {
                                if let Some((kind, path)) = self.native_last.clone() {
                                    let id = self
                                        .wallpaper_id_for_path(&path)
                                        .unwrap_or_else(|| "(不在列表里)".into());
                                    // 要求：每次自动应用都要留一行（场景 id + 触发原因）
                                    eprintln!(
                                        "[控制面] 自动应用「上次应用」{id} · 原因=连上内核且内核空闲（没在播）· kind={kind} path={path}"
                                    );
                                    native::request_async(native::cmd_apply(&kind, &path));
                                }
                            }
                            AutoApply::KernelBusy => {
                                let playing = self
                                    .playing_id()
                                    .unwrap_or_else(|| "(内核在播，但不在本地列表里)".into());
                                // 不换：留一行说明"看到了、但故意不动"
                                eprintln!(
                                    "[控制面] 启动同步：内核正在播 {playing} ⇒ **不推进\"上次应用\"**（内核是权威，绝不换掉你正在看的）"
                                );
                            }
                            AutoApply::Disabled => {
                                eprintln!(
                                    "[控制面] 启动同步：自动应用已关闭（--no-auto-apply / 配置）⇒ 不推进\"上次应用\""
                                );
                            }
                            // 内核空闲、也没有"上次应用" ⇒ 无事可做（不打日志，免得每次连上都刷屏）
                            AutoApply::NothingRemembered => {}
                        }
                        // **分槽画面色彩**也走这条"连上就重发"的路。内核无状态
                        // （重启后色彩是中性），而界面按壁纸 id 存着各自的色彩（`color_by_id`）
                        // ⇒ 把**这次要播的那张**的色彩补发一次：**优先用内核当前在播的那张**
                        // （内核是权威 —— 与上面"不换掉在播的那条"同一条事实），
                        // 内核没在播时才退回 `native_last`（启动参数刚下发的那条）；
                        // 都没有才用旧的全局值。此后切壁纸/拖滑块仍走即时下发（见 `set_color`）。
                        let color_id = self
                            .playing_id()
                            .or_else(|| {
                                self.native_last
                                    .as_ref()
                                    .and_then(|(_, path)| self.wallpaper_id_for_path(path))
                            });
                        let col = match &color_id {
                            Some(id) => config::color_for_id(&self.config, id).to_string(),
                            None => config::color_default(&self.config).to_string(),
                        };
                        let who = color_id.clone().unwrap_or_else(|| "(全局)".into());
                        self.push_color(&col, &format!("启动同步（这条壁纸 {who} 的色彩）"));
                        // **属性覆盖表**同理 —— 内核重启后覆盖表是空的，而界面存了一份
                        // （`prop_overrides.json`）⇒ 把"正在播放那张"的整套属性重发一次
                        // （`clear=true`：先清空再写，避免上一张的属性残留）。这样"界面显示的状态"
                        // 与"屏幕上实际的状态"始终一致；此后每次改动仍走即时下发（见 `set_prop`）。
                        let playing_path = self
                            .native
                            .as_ref()
                            .map(|s| s.path.clone())
                            .unwrap_or_default();
                        if !playing_path.is_empty() {
                            let mut sent: Vec<(String, String)> = Vec::new();
                            for (id, m) in &self.prop_edits {
                                let dir = self
                                    .wallpapers
                                    .iter()
                                    .find(|w| w.id == *id)
                                    .and_then(native::entry_dir);
                                if let Some(dir) = dir
                                    && native::playing_matches_dir(&playing_path, &dir)
                                {
                                    sent = m
                                        .iter()
                                        .map(|(k, v)| (k.clone(), native::value_str(v)))
                                        .collect();
                                }
                            }
                            let _ = native::request_async(native::cmd_set_props(&sent, true));
                        }
                    }
                    true => {}
                    // 内核不在（没 --serve / 刚重启）⇒ 下次连上重新补发
                    false => self.perf_synced = false,
                }
            }

            Message::NativeReply(request, reply) => {
                let cmd = native::raw_value(&request, "cmd").unwrap_or_default();
                let ok = native::get_bool(&reply, "ok");
                // set 的回执带着内核**实际生效**的四项 ⇒ 存下来显示"内核当前值"
                if cmd == "set" {
                    self.kernel_perf = native::perf_from_reply(&reply);
                }
                // 音频那两条：① 列可用输出（存进模型，供设置页画选择器）
                // ② 切输出源（提示跟上了哪一路；失败把内核的理由原样带出来）
                if cmd == "audio" {
                    let asked_sink = native::raw_value(&request, "sink").unwrap_or_default();
                    if native::get_bool(&request, "sinks") {
                        match native::parse_sinks_reply(&reply) {
                            Ok(list) => {
                                self.audio_sinks = list;
                                self.audio_sinks_err = None;
                            }
                            Err(e) => {
                                // 枚举失败 ⇒ **如实**记下那句人话，清空列表；界面上显示它，不装作没有输出
                                self.audio_sinks.clear();
                                self.audio_sinks_err = Some(e);
                            }
                        }
                        // 回执里内核报的"当前跟哪一路"（status 还没到时当兜底）
                        if let Some(sel) = native::reply_selected_sink(&reply) {
                            self.audio_sink_seen = Some(sel);
                        }
                    } else if !asked_sink.is_empty() {
                        self.native_note = Some(if ok {
                            let sel = native::raw_value(&reply, "sink").unwrap_or(asked_sink.clone());
                            format!("音频分析 → {}", native::sink_label(&sel))
                        } else {
                            let err = native::raw_value(&reply, "error").unwrap_or_default();
                            format!("音频分析切换失败 — {err}")
                        });
                    }
                }
                // **只给"用户动作"的回执留提示**：`status` 是每 3 秒一次的轮询，
                // 给它写提示会让角落那行永远挂着"status 成功" ✗。文案也不带「内核:」前缀。
                // 音频那两条上面已经写过提示了，这里跳过（否则会被覆成干巴巴的"audio 成功"）。
                if cmd != "status" && cmd != "audio" {
                    self.native_note = Some(match (ok, cmd.as_str()) {
                        (true, "set") => match &self.kernel_perf {
                            Some((fps, scale, effects, sleep)) => {
                                format!("已生效 {fps} fps · {scale} · {effects} · {sleep}")
                            }
                            None => "设置已下发（回执里没带四项）".into(),
                        },
                        (true, other) => format!("{other} 成功"),
                        (false, other) => {
                            let err = native::raw_value(&reply, "error").unwrap_or_default();
                            format!("{other} 失败 — {err}")
                        }
                    });
                }
                // 用户动作的回执 ⇒ 立刻刷一次状态，不等下一个 3 秒 tick。
                // ⚠ **只有非 status 的回执才允许再发 status**：status 的请求也会走
                // `rx.recv → Message::NativeReply` 这条路回来；若不判断，回执又会再发一次
                // status ⇒ `status → reply → status → …` **自我循环**，以 socket 往返速度空转
                // （实测空闲 ~8k 请求/秒、主线程约 100% CPU，纯浪费、状态还一份没多刷）。
                // 周期性刷新由 subscription 里那条 3 秒 ticker（`Message::NativeStatus`）负责。
                if cmd != "status" {
                    native::request_async(native::cmd_status());
                }
            }

            // ── 性能档位（设置页 → 自研内核）────────────────────────────────
            // 每条都：算新的四项 → 写进 Config + 持久化 → 下发给内核
            Message::PerfPreset(name) => {
                if let Some((_, scale, effects, sleep)) = config::preset_by_name(&name) {
                    let note = format!(
                        "内核: 切到「{}」= {scale} · {effects} · {sleep}（帧率按本场景）",
                        config::preset_label(&name)
                    );
                    self.commit_perf(scale.to_string(), effects.to_string(), sleep.to_string(), note);
                } else {
                    self.error = Some(format!("没有这个档位：{name}"));
                }
            }

            Message::FitModeSet(mode) => {
                // 几何设置：单独存、单独发（不碰性能四项，也不重建整套 set）
                self.config.fit_mode = mode.clone();
                if let Some(ctx) = &self.config_ctx {
                    if let Err(err) = self.config.write_entry(ctx) {
                        self.error = Some(format!("写配置失败（取景模式不会持久化）：{err}"));
                    }
                }
                native::request_async(native::cmd_set_fit(&mode));
                self.native_note = Some(format!("取景模式 → {}", mode));
            }

            // **音频分析跟哪一路输出**：走内核控制面 `{"cmd":"audio","sink":…}`（后台 socket）。
            // 真正下发在后台任务里（不卡 UI 线程）；跟没跟上由随后的 status 回执显示
            // （`status.audio` / `status.audio_sink`）。
            Message::AudioSinkSet(spec) => {
                let spec = if spec.trim().is_empty() {
                    "default".to_string()
                } else {
                    spec
                };
                if native::request_async(native::cmd_audio_sink(&spec)) {
                    self.native_note = Some(format!(
                        "音频分析 → 请求切到 {}",
                        native::sink_label(&spec)
                    ));
                } else {
                    self.error = Some("内核没连上，切不了音频分析输出源".into());
                }
            }

            Message::AudioSinksRefresh => {
                // 只重发一次枚举请求；枚举失败不在这里空转重试
                self.audio_sinks_asked = native::request_async(native::cmd_audio_sinks());
                if !self.audio_sinks_asked {
                    self.error = Some("内核没连上，暂时枚举不了可用输出".into());
                }
            }

            // 指针（鼠标驱动特效）：把鼠标位设到画面中心。
            // 口径与内核自己的 `wl_pointer` 完全一致（屏幕归一化 [0,1]，y 从顶往下）——
            // 这条命令就是"任何拿得到指针位置的进程"都能用的那个口子（脚本化/离线也用它）。
            Message::PointerToCenter => {
                if native::request_async(native::cmd_mouse(0.5, 0.5)) {
                    self.native_note = Some("鼠标位 → 画面中心（透视/视差会跟着跳）".into());
                } else {
                    self.error = Some("内核没连上，推不了鼠标位".into());
                }
            }

            Message::ColorStep(i, delta) => {
                let mut c = self.current_color();
                let (lo, hi) = color_bounds(i);
                c[i] = (c[i] + delta).clamp(lo, hi);
                self.set_color(c);
            }

            // 属性面板里的滑块：**直接设成**某个值（和上面 ± 按钮共用同一条写配置+下发路径）
            Message::ColorSet(i, v) => {
                let mut c = self.current_color();
                let (lo, hi) = color_bounds(i);
                c[i] = v.clamp(lo, hi);
                self.set_color(c);
            }

            Message::ColorReset => {
                self.set_color([1.0f32, 1.0, 1.0, 0.0, 0.0, 1.0]);
            }

            // 拖动结束（松手）：① 立刻放行出站队列里最后那个值（不等 55 ms 节流窗口）
            // ② 覆盖表强落盘（拖动期间最多 4 次/秒）。这就是"松手必发终值"的落点。
            Message::ColorDragEnd => {
                native::flush_now();
                self.persist_props(true);
            }

            Message::PerfFpsStep(step) => {
                // 微调**本场景**的帧率（写进逐场景表；不再动任何全局档）
                let sid = self.current_scene_id();
                let (cur, _src) = config::fps_for_scene(&self.scene_fps, sid.as_deref());
                let new = config::step_fps(cur, step);
                if new != cur {
                    self.set_scene_fps_now(new, &format!("内核: 本场景帧率 {cur} → {new} fps"));
                }
            }

            // 帧率档（**本场景**三选一）：自适应（默认 60）/ 固定 60 / 固定 30。
            // 两者都写进逐场景表 ⇒ "按这条场景记住"，别的场景不受牵连。
            Message::SceneFpsAuto => {
                self.clear_scene_fps_now("内核: 本场景帧率 → 自适应");
            }

            Message::SceneFpsFixed(fps) => {
                self.set_scene_fps_now(fps, &format!("内核: 本场景帧率 → {fps} fps"));
            }

            Message::PerfScaleToggle => {
                let (scale, effects, sleep) = config::effective(&self.config);
                let new = config::next_scale(&scale);
                self.commit_perf(new.to_string(), effects, sleep, format!("内核: 渲染尺寸 → {new}"));
            }

            Message::PerfEffectsToggle => {
                let (scale, effects, sleep) = config::effective(&self.config);
                let new = config::next_effects(&effects);
                self.commit_perf(scale, new.to_string(), sleep, format!("内核: 特效链 → {new}"));
            }

            Message::PerfSleepCycle => {
                let (scale, effects, sleep) = config::effective(&self.config);
                let new = config::next_sleep(&sleep);
                self.commit_perf(scale, effects, new.to_string(), format!("内核: 睡眠策略 → {new}"));
            }

            Message::PerfPush => {
                let sid = self.current_scene_id();
                self.push_scene_perf(sid.as_deref(), None, "内核: 手动下发");
            }


            Message::RestartKernel => {
                // 内核是 systemd 用户服务；界面只发一条 systemctl restart（阻塞约 1 秒）
                match restart_native_kernel() {
                    Ok(msg) => self.native_note = Some(msg),
                    Err(e) => self.error = Some(format!("重启内核失败: {e}")),
                }
            }

            Message::WindowResized(w) => {
                // 布局（每行几列 / 面板宽度）全靠这个数；`WPHUB_MSG_STATS=1` 时打出来，
                // 免得截图量尺寸时靠猜。
                if msg_stats_on() && (self.win_w - w).abs() > 0.5 {
                    eprintln!("[win] 窗口逻辑宽度 {w:.0}（面板 {} / 每行 {} 列）", panel_width(w, self.card_w()), tiles_per_row(w, self.card_w()));
                }
                self.win_w = w;
            }

            Message::Refresh => self.refresh_all(),

            Message::Apply(id) => {
                if self.backend_native {
                    eprintln!("[apply] 收到点击 确认/应用 id={id}（backend_native=true）");
                    self.apply_native(&id);
                } else {
                    // 这条以前是**完全静默**的：backend_native=false 时点"确认/应用"什么都不做、
                    // 一个字也不打 ⇒ 现象正是"点了没反应"。现在留证据并给界面一句人话。
                    eprintln!("[apply] 点了确认/应用 id={id} 但 backend_native=false ⇒ 不下发（内核后台未启用）");
                    self.error = Some(
                        "当前没有启用自研内核后台（设置里的「自研内核」是关的）⇒ 点了不会切壁纸"
                            .into(),
                    );
                }
            }

            Message::PauseToggle => {
                if self.backend_native {
                    // 自研内核：走控制面的 pause/resume（内核里是真 SIGSTOP 解码器，不是只停提交）
                    let paused = self.native.as_ref().is_some_and(|n| n.user_paused);
                    let line = if paused {
                        native::cmd_resume()
                    } else {
                        native::cmd_pause()
                    };
                    if !native::request_async(line) {
                        self.error = Some("下发内核暂停指令失败：后台未就绪".into());
                    }
                }
            }

            Message::SearchChanged(text) => {
                self.search = text;
                self.refresh_wallpapers();
            }

            Message::DiscoverQueryChanged(text) => {
                self.discover_query = text;
            }

            Message::DiscoverSearch => {
                // 在系统浏览器里打开对应的工坊搜索页；订阅/下载只能在工坊页面上完成
                let q = self.discover_query.trim().to_string();
                let url = if q.is_empty() {
                    format!("https://steamcommunity.com/workshop/browse/?appid={WE_APPID}&browsesort=trend")
                } else {
                    format!(
                        "https://steamcommunity.com/workshop/browse/?appid={WE_APPID}&searchtext={}",
                        workshop::urlencode(&q)
                    )
                };
                if let Err(err) = open::that_detached(&url) {
                    eprintln!("failed to open {url:?}: {err}");
                }
            }

            // ── 设置（行为页）──────────────────────────────────────────────
            // 注：`ReloadSettings` / `SaveSettings`（旧 daemon 的全局设置读写）已随 daemon 移除。

            // ── 发现页（创意工坊，零登录）──────────────────────────────────
            Message::DiscoverRunSearch => {
                self.discover_loading = true;
                self.discover_error = None;
                self.discover_t0 = Some(std::time::Instant::now());
                // 重置"UI 最坏一帧"统计（下面 ThumbsDone 时打印）
                let _ = upd_max_us_take();
                let query = self.discover_query.clone();
                let page = self.discover_page;
                let sort = workshop::SORTS[self.discover_sort % workshop::SORTS.len()].0;
                return Task::perform(
                    async move {
                        // HTTP 是阻塞式，放到线程池，别卡住 UI
                        tokio::task::spawn_blocking(move || {
                            // 阶段一：**只要条目**（缓存命中时几十毫秒就回来），
                            // 缩略图随后由 DiscoverResult 里的 Task::stream 后台逐张补
                            workshop::search_entries(&query, page, sort)
                        })
                        .await
                        .unwrap_or_else(|e| Err(format!("搜索任务失败: {e}")))
                    },
                    |result| Message::DiscoverResult(Box::new(result)),
                )
                .map(cosmic::Action::App);
            }

            Message::DiscoverResult(result) => {
                self.discover_loading = false;
                match *result {
                    Ok(found) => {
                        // 阶段一的结果：条目**先上屏**（还没有图 ⇒ 界面给"无预览"占位）。
                        // 这是一次性 search 的关键改动：以前要等 items+thumbs 全好才显示。
                        // ⚠ **上屏前的最后一道按 id 去重**（`workshop::dedupe_by_id`）：
                        // 数据层（`search_entries`）已经折过一次，这里再折一次是因为**列表的
                        // 组装点**才是不许重复的那一处 —— 以后如果本地已下载条目、缓存条目、
                        // 第二页结果并进这个列表，重复 id 只可能出现在这里。
                        // 判据只有 id 相等（不按标题：标题极像的两条是**不同**条目）
                        // `dedupe_never_merges_different_ids_even_with_identical_titles`）。
                        // 去重是"整条丢/整条留"，留下那条的 id 不变 ⇒ 右上角"已订阅"角标
                        // （按 id 查本机订阅集合）不会因为去重而丢。
                        let (items, dup_ids) = workshop::dedupe_by_id(found.items);
                        self.discover_links = found.stats.link_occurrences;
                        self.discover_dedup = dup_ids.len();
                        if !dup_ids.is_empty() {
                            eprintln!(
                                "[discover] 上屏前按 id 折叠重复 {} 条：{}",
                                dup_ids.len(),
                                dup_ids.join(", ")
                            );
                        }
                        self.discover_items = items;
                        self.discover_error = None;
                        if msg_stats_on() {
                            let ms = self
                                .discover_t0
                                .map(|t| t.elapsed().as_millis())
                                .unwrap_or(0);
                            eprintln!(
                                "[discover] 条目上屏 {ms}ms（{} 条，缩略图后台补；页面链接 {} 次 → 唯一 id {}；同 id 折叠 {}）",
                                self.discover_items.len(),
                                self.discover_links,
                                self.discover_items.len(),
                                self.discover_dedup
                            );
                        }
                        // 新一页：id 会变，旧的缩略图（句柄 + 原始字节）不能串到新条目上
                        self.discover_thumbs.clear();
                        self.discover_thumb_raw.clear();
                        self.discover_thumb_stats = None;
                        // workshop-select：结果一到就选中那条（= 点缩略图，同一条 Message）。
                        // ⚠ **不能在这里直接 return** —— 下面那段还要起缩略图流，两个 Task 要一起发。
                        let select_task = match self.pending_workshop_select.clone() {
                            Some(id) if self.discover_items.iter().any(|i| i.id == id) => {
                                self.pending_workshop_select = None;
                                eprintln!("[--workshop-select] 选中工坊条目 {id}");
                                Some(self.update(Message::SelectWorkshop(id)))
                            }
                            _ => None,
                        };
                        let items = self.discover_items.clone();
                        if items.is_empty() {
                            self.discover_thumbs_loading = false;
                            if let Some(t) = select_task {
                                return t;
                            }
                        } else {
                            // 阶段二：后台并发下缩略图，**每下到一张就发一条消息**逐张补图
                            self.discover_thumbs_loading = true;
                            let stream = Task::stream(discover_thumb_stream(items)).map(cosmic::Action::App);
                            return match select_task {
                                Some(t) => Task::batch(vec![stream, t]),
                                None => stream,
                            };
                        }
                    }
                    Err(err) => {
                        // 失败**要留一行日志**（界面里只有那行提示；日志要能拿来算"到底等了多久"）
                        let ms = self
                            .discover_t0
                            .map(|t| t.elapsed().as_millis())
                            .unwrap_or(0);
                        eprintln!("[发现] 搜索失败（{ms}ms 后放弃）：{err}");
                        self.discover_items = Vec::new();
                        self.discover_links = 0;
                        self.discover_dedup = 0;
                        self.discover_thumbs.clear();
                        self.discover_thumb_raw.clear();
                        self.discover_thumbs_loading = false;
                        self.discover_error = Some(err);
                    }
                }
            }

            Message::DiscoverThumb(id, bytes) => {
                // 只收当前这一页的：翻页/重搜后迟到的旧图丢掉（不串页、不涨内存）
                if self.discover_items.iter().any(|i| i.id == id) {
                    // 原始字节留一份：按卡片尺寸做 CPU 预缩放要用（Handle::Bytes 拿不回字节）
                    self.discover_thumb_raw.insert(id.clone(), bytes.clone());
                    self.discover_thumbs
                        .insert(id, widget::image::Handle::from_bytes(bytes));
                }
            }

            Message::DiscoverThumbsDone(stats) => {
                self.discover_thumbs_loading = false;
                self.discover_thumb_stats = *stats;
                if msg_stats_on() {
                    let ms = self
                        .discover_t0
                        .map(|t| t.elapsed().as_millis())
                        .unwrap_or(0);
                    // 关键判据：**每一条**（不是只数了多少张图）都拿到了缩略图
                    let with_img = self
                        .discover_items
                        .iter()
                        .filter(|i| self.discover_thumbs.contains_key(&i.id))
                        .count();
                    eprintln!(
                        "[discover] 缩略图就绪 {ms}ms（自搜索开始，{}/{} 张，条目有图 {}/{}）· UI 单次 update 最长 {}µs",
                        self.discover_thumbs.len(),
                        self.discover_items.len(),
                        with_img,
                        self.discover_items.len(),
                        upd_max_us_take()
                    );
                }
            }

            Message::DiscoverSortCycle => {
                self.discover_sort = (self.discover_sort + 1) % workshop::SORTS.len();
                self.discover_page = 1;
                return self
                    .update(Message::DiscoverRunSearch);
            }

            Message::DiscoverNextPage => {
                self.discover_page += 1;
                return self.update(Message::DiscoverRunSearch);
            }

            // ── 创意工坊：浏览 / 搜索 / 详情（**不跳网页**、零登录）──
            // 浏览与搜索走公开页面（`workshop` 模块，不需要任何身份）；登录 / 订阅 / 下载
            // 这一整套入口已删除 —— 要动手请用详情面板的 [工坊页]（明写是浏览器）。
            Message::SelectWorkshop(id) => {
                if self.wallpapers.iter().any(|w| w.id == id) {
                    // 已经下载到本地 ⇒ 走本地详情（有完整属性面板）
                    self.select(id);
                } else if self.discover_items.iter().any(|i| i.id == id) {
                    // 还没下载 ⇒ 仍然在**本程序内**看详情（右侧面板显示工坊信息 + 工坊页链接）
                    self.selected = Some(id);
                    self.open_color = None;
                } else {
                    eprintln!("[工坊] 列表里没有 {id}");
                }
            }

            // ── 详情面板·作者宣传图（后台逐张下，和发现页缩略图同一种套路）──
            Message::PromoImage(id, i, bytes) => {
                // 只收**当前选中**那条的：换条后迟到的旧图丢掉（不串图、不涨内存；
                // 磁盘缓存还在，回头再看是瞬时的）
                if self.selected.as_deref() == Some(id.as_str())
                    && let Some((w, h)) = dims_of_bytes(&bytes)
                {
                    self.promo_img.entry(id).or_default().insert(
                        i,
                        PromoImg {
                            handle: widget::image::Handle::from_bytes(bytes),
                            w,
                            h,
                        },
                    );
                }
            }

            Message::PromoDone(id) => {
                if self.promo_fetching.as_deref() == Some(id.as_str()) {
                    self.promo_fetching = None;
                }
            }

            // ── 排除名单 ────────────────────────────────────────────────────
            // ── 显示器页的操作 ──────────────────────────────────────────────
            // ── 参数页 ──────────────────────────────────────────────────────
            // ── 壁纸页：视图与筛选 ──────────────────────────────────────────
            Message::GoPage(page) => {
                self.nav.activate_position(match page {
                    Page::Wallpapers => 0,
                    Page::Discover => 1,
                    Page::Settings => 2,
                    Page::Workshop => 3,
                });
                if page == Page::Wallpapers {
                    self.refresh_wallpapers();
                }
                // 进设置页就**在后台**枚举一次可用输出（socket I/O 在后台任务里，不卡 UI 线程）。
                // 内核此刻没起来 ⇒ request_async 回 false ⇒ `asked` 保持 false，
                // 等第一个 NativeStatus（内核起来后）再补要一次，不自己空转重试。
                if page == Page::Settings {
                    self.audio_sinks_asked = native::request_async(native::cmd_audio_sinks());
                }
                // 进「发现」/「创意工坊」都自动拉一页（用户第一次点进去就有东西看）
                if matches!(page, Page::Discover | Page::Workshop)
                    && self.discover_items.is_empty()
                    && !self.discover_loading
                {
                    return self.update(Message::DiscoverRunSearch);
                }
            }

            // 类型/标签筛选：**下拉选中**（不再是循环点击）—— 选中即改下标，
            // 列表每帧由 `filtered_wallpapers` 现算，所以不用重扫磁盘。
            Message::SetTypeFilter(i) => {
                self.type_filter = i.min(self.known_types.len());
            }

            Message::SetTagFilter(i) => {
                self.tag_filter = i.min(self.known_tags.len());
            }

            // ── WE 皮肤：详情面板 / 逐壁纸属性编辑 ─────────────────────────
            Message::Select(id) => self.select(id),

            Message::Deselect => {
                self.selected = None;
                self.open_color = None;
            }

            Message::TogglePanel => {
                self.show_panel = !self.show_panel;
            }

            Message::ToggleColorPicker(key) => {
                self.open_color = if self.open_color.as_deref() == Some(key.as_str()) {
                    None
                } else {
                    Some(key)
                };
            }

            // 四种属性控件，全部走同一个「按 key 写覆盖表」的路 —— 不认属性名。
            Message::PropBool(id, key, v) => self.set_prop(&id, &key, serde_json::Value::Bool(v)),
            Message::PropNum(id, key, v) => {
                self.set_prop(&id, &key, serde_json::json!(v));
            }
            Message::PropColor(id, key, v) => self.set_prop(&id, &key, serde_json::Value::String(v)),
            Message::PropCombo(id, key, v) => {
                self.set_prop(&id, &key, combo_value_json(v));
            }
            Message::PropText(id, key, text) => {
                self.prop_text.insert(text_key(&id, &key), text.clone());
                self.set_prop(&id, &key, serde_json::Value::String(text));
            }

            // ── 「您的预设」那一排（WE 7-4 底部）───────────────────────────
            Message::PresetSave => {
                if let Some(id) = self.selected.clone() {
                    let slot = self.prop_edits.get(&id).cloned().unwrap_or_default();
                    let n = slot.len();
                    let mut all = load_presets();
                    all.insert(id.clone(), slot);
                    save_presets(&all);
                    self.native_note = Some(format!("预设 → 已保存这条壁纸的 {n} 项改动"));
                }
            }
            Message::PresetLoad => {
                if let Some(id) = self.selected.clone() {
                    let all = load_presets();
                    match all.get(&id).filter(|m| !m.is_empty()) {
                        Some(slot) => {
                            let pairs: Vec<(String, serde_json::Value)> =
                                slot.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                            let n = pairs.len();
                            for (k, v) in pairs {
                                self.set_prop(&id, &k, v);
                            }
                            self.native_note = Some(format!("预设 → 已应用 {n} 项"));
                        }
                        None => {
                            self.native_note = Some("预设 → 这条壁纸还没保存过".to_string());
                        }
                    }
                }
            }
            Message::PropsApplyAll => {
                if let Some(src) = self.selected.clone() {
                    let Some(edits) = self.prop_edits.get(&src).cloned() else {
                        self.native_note = Some("应用到所有壁纸 → 当前没有可复制的改动".to_string());
                        return Task::none();
                    };
                    // 目标要先算完再改（`set_prop` 要 `&mut self`，不能边借边改）
                    let targets: Vec<(String, Vec<Prop>)> = self
                        .wallpapers
                        .iter()
                        .filter(|w| w.id != src)
                        .map(|w| (w.id.clone(), parse_props(&w.user_properties_schema)))
                        .collect();
                    let (mut pairs, mut walls) = (0usize, 0usize);
                    for (id, props) in targets {
                        let mut hit = false;
                        for p in &props {
                            // 只写「目标壁纸也有这个名字、且类型对得上」的键 —— 不硬塞
                            if let Some(v) = edits.get(&p.key)
                                && json_fits_kind(v, p.kind)
                            {
                                self.set_prop(&id, &p.key, v.clone());
                                pairs += 1;
                                hit = true;
                            }
                        }
                        if hit {
                            walls += 1;
                        }
                    }
                    self.native_note = Some(format!("应用到所有壁纸 → {walls} 条壁纸共 {pairs} 项"));
                }
            }
            Message::PropsShareJson => {
                if let Some(id) = self.selected.clone() {
                    let json = self.props_json(&id);
                    // 日志里留一份同样的文本（本机没有 wl-paste，验证靠它 + 下面的剪贴板 Task）
                    println!("[分享 JSON] {json}");
                    self.clip_out = Some(json);
                    self.native_note = Some("分享 JSON → 已导出（复制到剪贴板）".to_string());
                }
            }
            Message::PropsReset => {
                if let Some(id) = self.selected.clone() {
                    let props = self
                        .selected_entry()
                        .map(|e| parse_props(&e.user_properties_schema))
                        .unwrap_or_default();
                    self.prop_edits.remove(&id);
                    save_overrides(&self.prop_edits);
                    // 回到 `project.json` 的默认值 ⇒ **把默认值重新下发**（屏幕跟着变回去）
                    if let Some(dir) = self
                        .wallpapers
                        .iter()
                        .find(|w| w.id == id)
                        .and_then(native::entry_dir)
                    {
                        let playing = self.native.as_ref().map(|s| s.path.clone()).unwrap_or_default();
                        if native::playing_matches_dir(&playing, &dir) {
                            for p in &props {
                                let _ = native::request_async(native::cmd_set_prop(
                                    &p.key,
                                    &native::value_str(&p.value),
                                ));
                            }
                        }
                    }
                    self.native_note = Some(format!("重置 → 已回到默认（{} 项）", props.len()));
                }
            }

            Message::OpenSelectedDir => {
                let dir = self
                    .selected_entry()
                    .map(|e| Path::new(&e.resource).to_path_buf());
                if let Some(dir) = dir {
                    if let Err(err) = open::that_detached(&dir) {
                        self.error = Some(format!("打不开目录：{err}"));
                    }
                }
            }

            Message::SetSort(i) => {
                self.sort = i.min(SORTS.len() - 1);
            }

            // 卡片尺寸档（小/中/大）：只改渲染宽度 ⇒ 列数/行数按新宽度重算（同一帧就重排）。
            Message::SetCardSize(i) => {
                self.card_idx = i.min(CARD_SIZES.len() - 1);
                eprintln!(
                    "[布局] 卡片档 = {}（{}×{}，每行 {} 列 @ 窗口 {:.0}）",
                    ["小", "中", "大"][self.card_idx],
                    self.card_w(),
                    self.card_h(),
                    tiles_per_row(self.win_w, self.card_w()),
                    self.win_w
                );
            }

            // ── 已安装壁纸属性里的两个调节项──────────────────────
            // 两个滑杆都**节流**：出站队列按 key 合并 + 55 ms 间隔（`native::OutQueue`，
            // ≈18 Hz ≤ 20 Hz 验收线），松手 `SliderRelease` 立刻放行终值。
            // 值也先写进本地字段 ⇒ 拖动中界面显示的就是最新值，不等内核回执。
            Message::TimeScaleSet(v) => {
                let v = v.clamp(0.25, 3.0);
                self.time_scale = v;
                let line = native::cmd_set_time_scale(v);
                eprintln!("[控制面] set time_scale={v:.2}");
                if !native::request_async(line) {
                    self.error = Some("内核没连上：播放速度这次只记在界面上".into());
                }
                self.native_note = Some(format!("播放速度 → {v:.2}×"));
            }
            Message::MusicVolumeSet(v) => {
                let v = v.clamp(0.0, 100.0);
                self.music_vol = Some(v);
                let line = native::cmd_set_volume(v);
                eprintln!("[控制面] set volume={v:.0}");
                if !native::request_async(line) {
                    self.error = Some("内核没连上：音乐音量这次只记在界面上".into());
                }
                self.native_note = Some(format!("音乐音量 → {v:.0}%"));
            }
            // 松手：立刻放行队列里的终值（不等下一个 55 ms 窗口）。
            Message::SliderRelease => {
                native::flush_now();
            }
            // 脚本化拖动（验收）：推进一拍，然后**发和滑杆完全相同的那条 Message**。
            // 「作者宣传图」开关（`Config::show_promo`，默认**开** = 面板里直接出作者的图）。
            // 关掉 = 不显示、也不联网；重新打开时若正选中一条、且它还有没缓存的宣传图，
            // 就顺手起后台下载流（不阻塞 UI）。
            Message::SetPromo(on) => {
                self.config.show_promo = on;
                if let Some(ctx) = &self.config_ctx
                    && let Err(err) = self.config.write_entry(ctx)
                {
                    self.error = Some(format!("写配置失败（这次改动不会持久化）：{err}"));
                }
                if on {
                    if let Some(id) = self.selected.clone() {
                        let has_remote = self
                            .promo
                            .get(&id)
                            .is_some_and(|l| l.iter().any(promo::Promo::is_remote));
                        let fully_cached = self
                            .promo_img
                            .get(&id)
                            .zip(self.promo.get(&id))
                            .is_some_and(|(m, l)| m.len() >= l.len());
                        if has_remote && !fully_cached {
                            self.promo_out = Some(id);
                        }
                    }
                } else {
                    // 关掉后不再起新的下载流（已发出的那一批会自己跑完，结果被忽略）
                    self.promo_out = None;
                }
            }

            // ── 内核服务 ────────────────────────────────────────────────────
        }
        // 「分享 JSON」：上面只把文本塞进 `clip_out`（match 的每个分支都是语句，
        // 没地方返回 Task），这里统一换成一条剪贴板写入 Task。
        let mut tail = match self.clip_out.take() {
            Some(text) => cosmic::iced::clipboard::write::<cosmic::Action<Message>>(text),
            None => Task::none(),
        };
        // 同理：`--panel-scroll` 的滚动请求也在这里统一发出去
        if let Some(px) = self.scroll_out.take() {
            let scroll = cosmic::iced::widget::scrollable::scroll_to::<cosmic::Action<Message>>(
                panel_scroll_id(),
                cosmic::iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: px }.into(),
            );
            tail = Task::batch(vec![tail, scroll]);
        }
        // 宣传图：选中一条、且还有没缓存的远程图 ⇒ 起一条后台流逐张补
        if let Some(id) = self.promo_out.take() {
            let list = self.promo.get(&id).cloned().unwrap_or_default();
            let pending: Vec<(usize, String)> = list
                .iter()
                .enumerate()
                .filter(|(_, p)| p.is_remote())
                .map(|(i, p)| (i, p.src.clone()))
                .collect();
            if !pending.is_empty() {
                self.promo_fetching = Some(id.clone());
                let stream = promo_stream(id, pending);
                tail = Task::batch(vec![tail, Task::stream(stream).map(cosmic::Action::App)]);
            }
        }
        tail
    }

    fn on_nav_select(&mut self, id: nav_bar::Id) -> Task<cosmic::Action<Self::Message>> {
        self.nav.activate(id);
        let title = self.update_title();
        // 首次进入发现页时自动拉一页
        if matches!(self.nav.active_data::<Page>(), Some(Page::Discover))
            && self.discover_items.is_empty()
            && !self.discover_loading
        {
            let search = self.update(Message::DiscoverRunSearch);
            return Task::batch(vec![title, search]);
        }
        title
    }
}

impl AppModel {
    pub fn update_title(&mut self) -> Task<cosmic::Action<Message>> {
        let mut window_title = fl!("app-title");
        if let Some(page) = self.nav.text(self.nav.active()) {
            window_title.push_str(" — ");
            window_title.push_str(page);
        }
        if let Some(id) = self.core.main_window_id() {
            self.set_window_title(window_title, id)
        } else {
            Task::none()
        }
    }

    // ── 性能档位：Config ⇄ 内核 ─────────────────────────────────────────────

    /// 改档位/逐项都走这里：写进 `Config` → 持久化 → 下发内核。
    /// 档位名（`perf_preset`）是**反推**出来的：三项与某一档完全一致就是那一档，
    /// 否则是 `custom`（"自定义"）。这样逐项改回去时标签会自己跟上，不用记"最后点了哪个按钮"。
    /// ⚠ **帧率不在档位里**：档位只管渲染尺寸/特效/睡眠。下发时 fps 按
    /// "**当前这条场景**"算（逐场景表 → 默认 60），由 [`Self::push_scene_perf`] 完成。
    fn commit_perf(&mut self, scale: String, effects: String, sleep: String, note: String) {
        self.config.render_scale = scale.clone();
        self.config.effects = effects.clone();
        self.config.sleep_mode = sleep.clone();
        self.config.perf_preset = config::preset_for(&scale, &effects, &sleep)
            .unwrap_or("custom")
            .to_string();

        // 持久化（重启 GUI 后还是这一档）
        if let Some(ctx) = &self.config_ctx {
            if let Err(err) = self.config.write_entry(ctx) {
                self.error = Some(format!("写配置失败（这次改动不会持久化）：{err}"));
            }
        }

        let sid = self.current_scene_id();
        self.push_scene_perf(sid.as_deref(), None, &note);
    }

    /// 当前该按**哪条场景**算帧率：内核正在播的那条 → 上次下发的那条 → 界面选中的那条。
    /// 这个顺序的理由：内核那条才是"用户正在看的"；还没连上内核时用 `native_last`
    /// （启动参数刚下发的那条）；都没有才退到面板里选中的（此时只是"将要播"）。
    fn current_scene_id(&self) -> Option<String> {
        self.playing_id()
            .or_else(|| {
                self.native_last
                    .as_ref()
                    .and_then(|(_, p)| self.wallpaper_id_for_path(p))
            })
            .or_else(|| self.selected.clone())
    }

    /// 把「这条场景的 fps + 全局三项（尺寸/特效/睡眠）」下发内核，并**留一行日志说明 fps 的来源**。
    /// 这是**唯一**下发性能四项的地方 —— `apply` / 档位 / 逐项 / 微调 / 自适应 / 启动同步都走它。
    /// 之前的问题是"每次应用都下发一个固定数、还把 drop-in 的 `WE_SCENE_FPS=60` 盖掉"；
    /// 现在 fps 一律来自 [`config::fps_for_scene`]（表 → 默认 60），
    /// `force` 只给"刚由用户 / 自适应定下的值"用，好让日志如实写"来源=用户选择 / 自适应"。
    fn push_scene_perf(&mut self, id: Option<&str>, force: Option<(u64, config::FpsSource)>, why: &str) {
        let (scale, effects, sleep) = config::effective(&self.config);
        let (fps, src) = resolve_fps(&self.scene_fps, id, force);
        let scene = id.unwrap_or("(无场景)");
        // 来源必须写清楚，绝不静默覆盖（来源=逐场景表/用户选择/默认/自适应）。
        eprintln!(
            "[控制面] 下发 fps={fps}（来源={}）· 场景={scene} · scale={scale} effects={effects} sleep={sleep} · {why}",
            src.label()
        );
        let note = format!("{why} → {fps} fps（来源：{}）", src.label());
        if native::request_async(native::cmd_set(fps, &scale, &effects, &sleep)) {
            self.perf_synced = true;
            self.native_note = Some(note);
        } else {
            self.perf_synced = false;
            self.native_note = Some(format!("{note}（内核没连上，等它就绪后自动补发）"));
        }
    }

    /// 用户给**本场景**钉一个 fps（写进逐场景表 + 落盘 + 下发）。
    fn set_scene_fps_now(&mut self, fps: u64, why: &str) {
        let Some(id) = self.current_scene_id() else {
            self.native_note = Some("还没有选中/在播的壁纸 ⇒ 先选一条再设帧率".into());
            eprintln!("[控制面] 改 fps 被忽略：{why}（没有场景）");
            return;
        };
        let v = config::set_scene_fps(&mut self.scene_fps, &id, fps);
        save_scene_fps(&self.scene_fps);
        // 用户刚给这条钉过 ⇒ 它不再自适应（免得自适应回头把用户的选择盖掉）
        if self.adapt_scene.as_deref() == Some(id.as_str()) {
            self.adapt_done = true;
        }
        // 用户的选择优先：升回连击 / 降档时刻 / 连击窗口一起清掉（这条场景已经不由自适应管了）
        self.adapt_recover_streak = 0;
        self.adapt_downgraded_at = None;
        self.adapt_span = None;
        self.adapt_note = None;
        self.push_scene_perf(Some(&id), Some((v, config::FpsSource::UserChoice)), why);
    }

    /// 用户给**本场景**改回自适应（删掉记录 ⇒ 默认 60 + 实测不够自动降 30）。
    fn clear_scene_fps_now(&mut self, why: &str) {
        let Some(id) = self.current_scene_id() else {
            self.native_note = Some("还没有选中/在播的壁纸 ⇒ 先选一条再设帧率".into());
            return;
        };
        let had = config::clear_scene_fps(&mut self.scene_fps, &id);
        save_scene_fps(&self.scene_fps);
        // 重开自适应：盯上这条、计数清零（升回连击与降档时刻也一起清）
        self.adapt_scene = Some(id.clone());
        self.adapt_done = false;
        self.adapt_streak = 0;
        self.adapt_recover_streak = 0;
        self.adapt_downgraded_at = None;
        self.adapt_span = None;
        self.frame_sample = None;
        self.adapt_note = None;
        eprintln!(
            "[控制面] 场景 {id} 帧率 → 自适应（{}）；默认 {} fps，实测连续 {} 次低于 {:.0}（且负载干净）才降 {}；\
             机器上有别的负载（loadavg 高）时不判定，降档后负载长期干净会升回（来源=用户选择）",
            if had { "以前记的固定档已删" } else { "本来就没记过" },
            config::SCENE_FPS_DEFAULT,
            config::ADAPT_FAIL_STREAK,
            config::ADAPT_FAIL_FPS,
            config::SCENE_FPS_HEAVY
        );
        self.push_scene_perf(Some(&id), None, why);
    }

    /// 自适应：用内核**累计帧数**的两次采样算**实际**帧率；持续明显跑不到就把本场景降到 30，
    /// 负载长期干净后**升回** 60。
    /// 只在"自动档"上动手（逐场景表里没有记录 —— 用户钉过的场景一个字都不动），
    /// 而且每次**判定**都必须说话：日志一行 + `adapt_note` 显示在设置页，降 / 升 / 不降都出声。
    /// 采样源是 `status.frames`（内核每渲染一帧 +1，`status` 里就有），所以这是**实测**而不是猜。
    /// **先看机器有没有别的负载**（修）：`/proc/loadavg` 明显高 ⇒ 这次实测值反映的是
    /// "别人在抢 CPU / 内存带宽"（本机是 APU，GPU 与 CPU 共享内存带宽），不是这条场景的能力
    /// ⇒ 整个观测丢掉、**不判定**（既不 +1 也不清零）。降档后连续 `ADAPT_RECOVER_STREAK` 条
    /// 干净样本 + 距降档 `ADAPT_UPGRADE_MIN_SECS` 秒 ⇒ 升回（GPU 打满则判为真重场景，不升）。
    fn tick_adaptive(&mut self) {
        let Some(n) = self.native.clone() else {
            self.frame_sample = None;
            return;
        };
        let id = self.playing_id();
        // 换场景 ⇒ 自适应重新起测（先按 60 跑）
        if self.adapt_scene != id {
            self.adapt_scene = id.clone();
            self.adapt_streak = 0;
            self.adapt_done = false;
            self.adapt_recover_streak = 0;
            self.adapt_downgraded_at = None;
            self.adapt_span = None;
            self.frame_sample = None;
        }
        // 没在画（暂停 / 被遮挡 / 没内容）⇒ 不采样、不判定
        if !n.playing || n.suppressed || n.user_paused {
            self.frame_sample = None;
            return;
        }
        let now = std::time::Instant::now();
        // 只有"自动档"才自适应：用户钉过（表里有记录）的场景一个字都不动
        if !adaptive_armed(&self.scene_fps, id.as_deref()) {
            self.frame_sample = Some((n.frames, now));
            return;
        }
        let prev = self.frame_sample.replace((n.frames, now));
        let Some((pf, pat)) = prev else { return };
        // 计数器被重置（换了内容）⇒ 这一次不算
        if n.frames < pf {
            return;
        }
        let dt = now.duration_since(pat).as_secs_f64();
        if dt < config::ADAPT_SAMPLE_MIN_SECS {
            return; // 采样间隔太短，噪声大
        }
        let measured = (n.frames - pf) as f64 / dt;
        let scene = id.as_deref().unwrap_or("(未知)");
        // 机器上有没有**别人**在抢 CPU / 内存带宽（判据是纯函数；这里只负责读 /proc/loadavg）
        let (ncpu, load1, runnable) = machine_load();
        let gpu = gpu_busy_percent();
        let ext = config::external_load(ncpu, load1, runnable);
        eprintln!(
            "{}",
            config::adapt_sample_line(measured, n.frames - pf, dt, scene, self.adapt_streak, ncpu, load1, gpu)
        );

        if self.adapt_done {
            // ── 已降档：只看"能不能升回" ────────────────────────────────────────
            let ok = config::adapt_recover_sample_ok(ext);
            self.adapt_recover_streak = config::adapt_recover_step(self.adapt_recover_streak, ok);
            // 拿不到降档时刻（理论上只在旧状态里）⇒ 不假装"已经很久了"：按"刚降"算，先攒干净样本
            let secs = self
                .adapt_downgraded_at
                .map(|t| now.duration_since(t).as_secs_f64())
                .unwrap_or(0.0);
            if config::adapt_should_upgrade(self.adapt_recover_streak, secs, gpu) {
                let note = config::adapt_upgrade_note(ncpu, load1, gpu, secs);
                eprintln!("[自适应] {note}");
                self.adapt_note = Some(note.clone());
                self.adapt_done = false;
                self.adapt_recover_streak = 0;
                self.adapt_downgraded_at = None;
                self.adapt_streak = 0;
                self.adapt_span = None;
                self.push_scene_perf(
                    id.as_deref(),
                    Some((config::SCENE_FPS_DEFAULT, config::FpsSource::Adaptive)),
                    &note,
                );
            } else if !ok {
                // 负载不干净 ⇒ 连击清零：也留一行写明依据
                let note = config::adapt_external_note(measured, ncpu, load1, runnable);
                eprintln!("[自适应] {note}");
                self.adapt_note = Some(note);
            } else if let Some(b) = gpu.filter(|b| *b >= config::ADAPT_GPU_BUSY_MAX)
                && self.adapt_recover_streak >= config::ADAPT_RECOVER_STREAK
            {
                // 负载干净但 GPU 打满 ⇒ 冷却乘 3：必须说清"**晚点还会试**"，不是永远卡在 30
                let note = config::adapt_gpu_hold_note(b, ncpu, load1);
                eprintln!("[自适应] {note}");
                self.adapt_note = Some(note);
            }
            return;
        }

        // ── 未降档：有别的负载 ⇒ **不判定**（这次观测整个丢掉）──────────────────
        if ext {
            let note = config::adapt_external_note(measured, ncpu, load1, runnable);
            eprintln!("[自适应] {note}");
            // 只在"这次本来会被算作跑不到"或"正在数连击"时才占用界面那行提示（免得刷屏；
            // 日志是每次都写，界面提示挑要紧的）
            if measured < config::ADAPT_FAIL_FPS || self.adapt_streak > 0 {
                self.adapt_note = Some(note);
            }
            return;
        }
        let before = self.adapt_streak;
        self.adapt_streak = config::adapt_step(self.adapt_streak, measured);
        self.adapt_span = config::adapt_span(self.adapt_span, measured);
        if self.adapt_streak == 0 && before > 0 {
            // 中途达标 / 几乎没在画 ⇒ 连击清零：把"为什么这次不降"也说清楚
            let note = config::adapt_clear_note(measured, before, ncpu, load1);
            eprintln!("[自适应] {note}");
            self.adapt_note = Some(note);
        }
        let span = self.adapt_span;
        if self.adapt_streak >= config::ADAPT_FAIL_STREAK && !config::adapt_shortfall_is_stable(span) {
            // 次数够了但实测值**忽高忽低** ⇒ 那是外部负载在忽强忽弱，不是这条场景的能力上限
            let note = match span {
                Some(s) => config::adapt_unstable_note(self.adapt_streak, s, ncpu, load1),
                // 理论上到不了（数够了就一定有窗口）；真到不了也不许 panic、更不许降档
                None => config::adapt_external_note(measured, ncpu, load1, runnable),
            };
            eprintln!("[自适应] {note}");
            self.adapt_note = Some(note);
        } else if config::adapt_should_downgrade(self.adapt_streak, span) {
            let note = config::adapt_downgrade_note(measured, self.adapt_streak, span, ncpu, load1);
            eprintln!("[自适应] {note}");
            self.adapt_note = Some(note.clone());
            self.adapt_done = true;
            self.adapt_recover_streak = 0;
            self.adapt_downgraded_at = Some(now);
            self.push_scene_perf(
                id.as_deref(),
                Some((config::SCENE_FPS_HEAVY, config::FpsSource::Adaptive)),
                &note,
            );
        }
    }

    // ── 控制面：拉取 ────────────────────────────────────────────────────────

    fn refresh_all(&mut self) {
        self.refresh_wallpapers();
    }

    fn refresh_wallpapers(&mut self) {
        // 列表**永远**来自本地扫描：磁盘上 project.json + scene.pkg 就够（自研内核走这条路），
        // 不用问任何人要。扫描根目录 = 用户自定义目录（默认 `~/Templates`）+ 可选的 Steam
        // 工坊目录（`wp::library`）；Steam 没装就是少一处来源，不是错误。
        self.sync_library_roots();
        self.adopt_wallpapers(local::scan_with(&self.config.library_dirs));
    }

    /// 把**有效的**库根目录写进状态文件 `~/.local/share/wallpaper-hub/library_dirs`
    /// （`wp::library`）：常驻内核按 id 找目录时读同一份 ⇒ 界面里换了目录之后
    /// 内核**不用重启**也能找到新位置。写失败只是回到"内核只认自己的那几处"，不影响界面。
    fn sync_library_roots(&self) {
        let roots = library::roots(&self.config.library_dirs);
        if let Err(e) = library::write_state(&roots) {
            eprintln!("[库目录] 写不进 {}（{e}），内核将只用自己的默认根目录", library::state_file().display());
        }
    }

    /// 用一份列表填界面；顺带累积"见过的类型/标签"，供筛选按钮用（不受当前筛选影响）。
    fn adopt_wallpapers(&mut self, list: Vec<proto::WallpaperEntry>) {
        self.wallpapers = list;
        for w in &self.wallpapers {
            if !self.known_types.contains(&w.wp_type) {
                self.known_types.push(w.wp_type.clone());
            }
            for tag in &w.tags {
                if !self.known_tags.contains(tag) {
                    self.known_tags.push(tag.clone());
                }
            }
        }
        self.maybe_native_apply_first();
    }

    /// `--native-apply-first`：把第一条「内核能播」的下发给自研内核。
    /// ⚠ 这段逻辑以前**只写在"daemon 回了列表"那个分支里** ⇒ 没有 daemon 时旗标永远不触发
    /// （验证脚本/headless 用法直接失效）。现在本地扫描这条路也会调它。
    fn maybe_native_apply_first(&mut self) {
        if !self.pending_native_apply_first {
            return;
        }
        self.pending_native_apply_first = false;
        let pick = self
            .wallpapers
            .iter()
            .filter(|w| native::unsupported_reason(&w.wp_type).is_none())
            .find_map(|w| {
                native::entry_dir(w)
                    .and_then(|d| native::find_playable(&d))
                    .map(|pl| (w.id.clone(), pl))
            });
        match pick {
            Some((id, (kind, path))) => {
                let p = path.to_string_lossy().into_owned();
                eprintln!("[--native-apply-first] 下发内核播放（{kind:?}）: {p}");
                self.native_last = Some((kind.as_str().to_string(), p.clone()));
                let ok = native::request_async(native::cmd_apply(kind.as_str(), &p));
                self.native_note = Some(if ok {
                    format!("内核: 正在下发 {p}")
                } else {
                    "内核: 后台未就绪（没在 --serve？）".into()
                });
                if ok {
                    // 同 `apply_native`：这条场景的帧率一起下发（没记过就是默认 60）
                    self.push_scene_perf(Some(&id), None, &format!("--native-apply-first {id}"));
                }
            }
            None => {
                eprintln!("[--native-apply-first] 列表里没有内核能播的条目");
                self.native_note = Some("内核: 列表里没有能播的场景/视频/图片".into());
            }
        }
    }

    // ── 逐壁纸属性（数据驱动：全部来自 project.json 的 general.properties）──

    /// 属性覆盖表落盘（**节流**）。
    /// 拖动色盘/滑块时 `set_prop` 每帧都被调一次（30~60 次/秒），逐条 `fs::write`
    /// 整个 JSON 纯属白扔 IO，而且全在 **UI 线程**上。这里：
///
    /// * `force=false` ⇒ 距上次落盘不到 250 ms 就只 Mark dirty（内存里的 `prop_edits`
    /// 已经是最新值，界面读的是它，不会看到旧值）；
///
    /// * `force=true`（松手 / 3 秒 status tick）⇒ 一定落盘。
    /// 最坏情况：拖动中崩进程 ⇒ 丢掉最后 ≤250 ms 的预览值（松手那一下会强落，所以
    /// 正常路径不会丢）。
    fn persist_props(&mut self, force: bool) {
        self.prop_dirty = true;
        const MIN_GAP: std::time::Duration = std::time::Duration::from_millis(250);
        if !force
            && self
                .prop_saved_at
                .is_some_and(|t| t.elapsed() < MIN_GAP)
        {
            return;
        }
        save_overrides(&self.prop_edits);
        self.prop_dirty = false;
        self.prop_saved_at = Some(std::time::Instant::now());
    }

    /// 把一次改动写进覆盖表并落盘。
    /// **不认属性名、不认类型**：谁调它都一样 —— 键和值原样存。
    fn set_prop(&mut self, id: &str, key: &str, value: serde_json::Value) {
        self.prop_edits
            .entry(id.to_string())
            .or_default()
            .insert(key.to_string(), value.clone());
        // 落盘节流（拖动时这个函数**每帧**被调一次）：内存里改完仍然立刻可见，
        // 只是不每帧都写盘 —— 见 `persist_props`。
        self.persist_props(false);
        // 除了落盘，**还要下发给正在播放的那张壁纸**（README 承诺"改完立即生效"）。
        // 走内核的通用控制面命令 `set-prop`（名值对，由渲染器按 project.json 的 general.properties
        // 解释；内核与渲染器都不针对属性名/类型做特判）。若改的不是当前在播的那张，只落盘。
        if !self.native.is_none() {
            let dir = self
                .wallpapers
                .iter()
                .find(|w| w.id == id)
                .and_then(native::entry_dir);
            let playing_path = self.native.as_ref().map(|s| s.path.clone()).unwrap_or_default();
            if let Some(dir) = dir
                && native::playing_matches_dir(&playing_path, &dir)
            {
                let _ = native::request_async(native::cmd_set_prop(key, &native::value_str(&value)));
            }
        }
    }

    /// 画面色彩六项的**唯一**落点：写进 `Config` → 持久化 → （该下发时）下发内核。
    /// `ColorStep`（± 按钮）/ `ColorSet`（属性面板滑块）/ `ColorReset` 三条路都走它 ——
    /// 三处一份逻辑，不会漂。
    /// **按壁纸 id 分槽**：
    /// 有选中就写进这条壁纸自己的槽（值为中性 ⇒ 删掉这条覆盖，等于回到默认）。
    /// **没有选中的壁纸 ⇒ 一个字也不写**（既不落盘也不下发）。
    /// 以前无选中时会把值写进**全局** `color`，让所有还没记过槽的壁纸一起被改掉 ——
    /// 那条真实路径是 `--perf-msg color-reset`（性能自检在"选中"之前就按了按钮），
    /// 它把分槽设计打穿了。落点判定拆成纯函数 [`write_color`]，单测直接钉它。
    fn set_color(&mut self, c: [f32; 6]) {
        let is_default = c == [1.0f32, 1.0, 1.0, 0.0, 0.0, 1.0];
        let (who, s) = match write_color(&mut self.config, self.selected.as_deref(), c) {
            ColorWrite::Slot { id, value } => (id, value),
            ColorWrite::Nothing => {
                self.native_note =
                    Some("画面色彩：没有选中的壁纸 ⇒ 不改（色彩按壁纸 id 分槽）".into());
                eprintln!(
                    "[控制面] set-color 忽略：没有选中的壁纸（值 {} 未落盘、未下发）",
                    color_to_config(&c)
                );
                return;
            }
        };
        if let Some(ctx) = &self.config_ctx
            && let Err(err) = self.config.write_entry(ctx)
        {
            self.error = Some(format!("写配置失败（调色不会持久化）：{err}"));
        }
        // 即时下发：**只有正在播的那条**才需要 —— 与 `set_prop` 同一条规矩。
        // 改的不是在播的那条时只落盘，等切到它时由 `Message::Apply` 带上。
        if self.playing_id().as_deref() == Some(who.as_str()) {
            self.push_color(&s, &format!("调色 · {who}"));
        } else {
            eprintln!("[控制面] set-color 只落盘（{who} 不是正在播的那条；值 {s}，切到它时下发）");
        }
        self.native_note = Some(if is_default {
            format!("画面色彩（{who}）→ 已还原")
        } else {
            format!("画面色彩（{who}）→ {}", color_summary(&c))
        });
    }

    /// 当前卡片宽度（逻辑像素，三档之一）
    fn card_w(&self) -> f32 {
        CARD_SIZES[self.card_idx.min(CARD_SIZES.len() - 1)].0
    }

    /// 当前卡片高度
    fn card_h(&self) -> f32 {
        CARD_SIZES[self.card_idx.min(CARD_SIZES.len() - 1)].1
    }

    /// 当前**在编辑的那条**壁纸的画面色彩：有选中就取这条的槽（分槽优先 ⇒ 退回旧的全局值），
    /// 没选中才取全局。属性面板那六个滑块与 `ColorStep/ColorSet/ColorReset` 都读它 ——
    /// 只有一处口径，界面显示 / 落盘 / 下发不会漂。
    /// ⚠ 读与写不对称：**没有选中时**这里仍会读出旧的全局值（当"默认档"显示用），
    /// 但 [`AppModel::set_color`] / [`write_color`] 在那种情况下**什么都不写** —— 见那里的注释。
    fn current_color(&self) -> [f32; 6] {
        match self.selected.as_deref() {
            Some(id) => color_from_config(config::color_for_id(&self.config, id)),
            None => color_from_config(config::color_default(&self.config)),
        }
    }

    /// 下发一条画面色彩到内核，并留一行**journal 可见**的证据。
    /// ⚠ 内核对 `{"cmd":"set","color":…}` **不打印任何日志**（它只给 `set-prop` 打；
    /// 实测两条命令的回执里也没有 color 字段）⇒ "确实下发了"这条证据只能留在**界面这一侧**
    /// 的控制面日志里（标识 `[控制面]`，和内核自己的日志同一批格式）。配合截图（画面真的变了）
    /// 一起看才算完整证据链。
    fn push_color(&mut self, col: &str, why: &str) {
        eprintln!("[控制面] set-color 值={col} · {why}");
        native::request_async(native::cmd_set_color(col));
    }

    /// 内核**现在正在播**的那条壁纸的 id（拿 `status.path` 去比对每条壁纸的目录）。
    fn playing_id(&self) -> Option<String> {
        let path = self.native.as_ref()?.path.clone();
        self.wallpaper_id_for_path(&path)
    }

    /// 这个内核路径属于哪条壁纸（scene = 目录本身；视频/图片 = 目录里的文件）。
    fn wallpaper_id_for_path(&self, path: &str) -> Option<String> {
        if path.is_empty() {
            return None;
        }
        self.wallpapers
            .iter()
            .find(|w| {
                native::entry_dir(w).is_some_and(|d| native::playing_matches_dir(path, &d))
            })
            .map(|w| w.id.clone())
    }

    /// 把某条壁纸下发给自研内核。详情面板 [应用] 按钮与 `--apply-id` 的**补跑**都走它
    /// （一处口径，不会漂）。
    /// 一次做三件事：
    /// ① **下发内容** —— scene 包 ⇒ `kind=scene` + 目录；视频/图片 ⇒ 对应文件（直接问磁盘，不问 daemon）；
    /// ② **整套作者属性**（`clear=true`）—— 内核先清空覆盖表再写，避免上一张的属性残留
    /// （属性名跨壁纸重名很常见，例如两张壁纸都有 `newproperty`）；
    /// ③ **这条壁纸自己的画面色彩**（分槽）—— 内核那层色彩是一趟**全局** shader，
    /// 不跟着换就会把上一条的偏色带到这一条身上（这正是用户要修的问题）。
    fn apply_native(&mut self, id: &str) {
        let picked = self
            .wallpapers
            .iter()
            .find(|w| w.id == id)
            .and_then(|e| native::entry_dir(e).and_then(|d| native::find_playable(&d)));
        match picked {
            Some((kind, path)) => {
                let p = path.to_string_lossy().into_owned();
                self.native_last = Some((kind.as_str().to_string(), p.clone()));
                let ok = native::request_async(native::cmd_apply(kind.as_str(), &p));
                self.native_note = Some(if ok {
                    format!("内核: 正在下发 {p}")
                } else {
                    "内核: 后台未就绪（没在 --serve？）".into()
                });
                if ok {
                    let props: Vec<(String, String)> = self
                        .prop_edits
                        .get(id)
                        .map(|m| {
                            m.iter()
                                .map(|(k, v)| (k.clone(), native::value_str(v)))
                                .collect()
                        })
                        .unwrap_or_default();
                    let _ = native::request_async(native::cmd_set_props(&props, true));
                    let col = config::color_for_id(&self.config, id).to_string();
                    self.push_color(&col, &format!("切换壁纸 {id}"));
                    // **这条场景自己的帧率**也要跟着切。内核的 fps 是**全局运行态**
                    // （`set` 只改字段、不重建），不重发就会把上一条的帧率带到这一条身上 ——
                    // 重场景 30 / 轻场景 60 就是靠这一下发成立的。来源写进那行日志。
                    self.push_scene_perf(Some(id), None, &format!("应用壁纸 {id}"));
                } else {
                    self.error = Some("下发内核指令失败：后台没在跑".into());
                }
            }
            None => {
                self.error = Some("这条壁纸内核播不了（目录里没有 scene.pkg / 视频 / 图片）".into());
            }
        }
    }

    /// 当前壁纸那套属性改动 → 一段 JSON（「分享 JSON」按钮与 `--props-json` 共用，
    /// **同一份生成函数**：验证时看到的文本就是复制进剪贴板的那一份）。
    fn props_json(&self, id: &str) -> String {
        let name = self
            .wallpapers
            .iter()
            .find(|w| w.id == id)
            .map(|e| e.name.as_str())
            .unwrap_or("");
        let edits = self.prop_edits.get(id).cloned().unwrap_or_default();
        props_json_text(id, name, &edits)
    }

    fn select(&mut self, id: String) {
        if !self.dims.contains_key(&id) {
            if let Some(e) = self.wallpapers.iter().find(|w| w.id == id)
                && !e.preview.is_empty()
                && let Ok((w, h)) = image::image_dimensions(&e.preview)
            {
                self.dims.insert(id.clone(), (w, h));
            }
        }

        let dir = self
            .wallpapers
            .iter()
            .find(|w| w.id == id)
            .map(|e| PathBuf::from(&e.resource));

        // 面板顶部大图的**源文件**：默认封面；若目录里还有**像素比封面更多**的本地图
        // （作者的高分辨率帧/作者图），优先用它 —— 但显示尺寸不会把图拉大（见 `preview_display_size`）。
        let cover = self
            .wallpapers
            .iter()
            .find(|w| w.id == id)
            .map(|e| e.preview.clone())
            .filter(|p| !p.is_empty() && Path::new(p).exists());
        let cover_dims = cover.as_deref().and_then(|p| image::image_dimensions(p).ok());
        let bigger = dir.as_deref().and_then(|d| {
            find_larger_local_image(d, cover_dims.map(|(w, h)| w.saturating_mul(h)))
        });
        match bigger {
            Some((p, dims)) => {
                self.preview_src.insert(id.clone(), p);
                self.preview_dims.insert(id.clone(), dims);
            }
            None => match (cover, cover_dims) {
                (Some(p), dims) => {
                    self.preview_src.insert(id.clone(), PathBuf::from(p));
                    match dims {
                        Some(d) => {
                            self.preview_dims.insert(id.clone(), d);
                        }
                        None => {
                            self.preview_dims.remove(&id);
                        }
                    }
                }
                (None, _) => {
                    self.preview_src.remove(&id);
                    self.preview_dims.remove(&id);
                }
            },
        }

        let list = dir
            .as_deref()
            .and_then(|d| std::fs::read_to_string(d.join("project.json")).ok().map(|js| (d, js)))
            .map(|(d, js)| promo::extract(&js, d))
            .unwrap_or_default();

        let mut ready: HashMap<usize, PromoImg> = HashMap::new();
        let mut need_fetch = false;
        for (i, p) in list.iter().enumerate() {
            let bytes_and_dims = match &p.local {
                Some(path) => image::image_dimensions(path)
                    .ok()
                    .and_then(|(w, h)| std::fs::read(path).ok().map(|b| (b, w, h))),
                None => match promo::cached(&p.src) {
                    Some(b) => dims_of_bytes(&b).map(|(w, h)| (b, w, h)),
                    None => {
                        need_fetch = true;
                        None
                    }
                },
            };
            if let Some((bytes, w, h)) = bytes_and_dims {
                ready.insert(
                    i,
                    PromoImg {
                        handle: widget::image::Handle::from_bytes(bytes),
                        w,
                        h,
                    },
                );
            }
        }

        let has_promo = !list.is_empty();
        if msg_stats_on() {
            let remote = list.iter().filter(|p| p.is_remote()).count();
            eprintln!(
                "[promo] {id}: 抽到 {} 条宣传图引用（本地 {} / 外链 {}，已上屏 {}，待下载 {}）",
                list.len(),
                list.len() - remote,
                remote,
                ready.len(),
                list.len() - ready.len()
            );
        }
        self.promo.insert(id.clone(), list);
        self.promo_img.insert(id.clone(), ready);
        // **默认关**时绝不起下载流（= 不联网拉作者外链）；开着才补没缓存的那些。
        if has_promo && need_fetch && self.config.show_promo {
            self.promo_out = Some(id.clone());
        }
        self.selected = Some(id);
        // 截图参数 `--open-color` 只对**第一次**选中生效；平时照旧收起（None）
        self.open_color = self.pending_open_color.take();
    }

    /// 当前选中的条目
    fn selected_entry(&self) -> Option<&proto::WallpaperEntry> {
        let id = self.selected.as_deref()?;
        self.wallpapers.iter().find(|w| w.id == id)
    }

    /// 当前选中的是一条**还没下载到本地**的工坊条目时，拿它在发现页结果里的那一份
    /// （右侧面板据此显示工坊信息 + 工坊页链接，**不跳网页**）。
    fn selected_workshop(&self) -> Option<&workshop::Item> {
        let id = self.selected.as_deref()?;
        self.discover_items.iter().find(|i| i.id == id)
    }

    /// 搜索 + 类型/标签筛选 + 排序之后的列表。
    fn filtered_wallpapers(&self) -> Vec<&proto::WallpaperEntry> {
        let q = self.search.trim().to_lowercase();
        let ty = if self.type_filter == 0 {
            None
        } else {
            self.known_types.get(self.type_filter - 1)
        };
        let tag = if self.tag_filter == 0 {
            None
        } else {
            self.known_tags.get(self.tag_filter - 1)
        };
        let mut out: Vec<&proto::WallpaperEntry> = self
            .wallpapers
            .iter()
            .filter(|w| {
                let hit_q = q.is_empty()
                    || w.name.to_lowercase().contains(&q)
                    || w.tags.iter().any(|t| t.to_lowercase().contains(&q));
                let hit_ty = ty.is_none_or(|t| &w.wp_type == t);
                let hit_tag = tag.is_none_or(|t| w.tags.iter().any(|x| x == t));
                hit_q && hit_ty && hit_tag
            })
            .collect();
        // 排序：0 = 名称（默认，扫描时就这序）/ 1 = 类型 / 2 = 大小（大在前）
        match self.sort {
            1 => out.sort_by(|a, b| a.wp_type.cmp(&b.wp_type).then_with(|| a.name.cmp(&b.name))),
            2 => out.sort_by(|a, b| b.size.cmp(&a.size)),
            _ => out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
        }
        out
    }

    /// 一条属性的**当前值**：覆盖表优先，没有就用 schema 里的默认值。
    /// 返回 owned：返回值不该和 `p`（一个临时）绑在一起，面板每帧克隆这点数据不心疼。
    fn prop_value(&self, id: &str, p: &Prop) -> serde_json::Value {
        self.prop_edits
            .get(id)
            .and_then(|m| m.get(&p.key))
            .cloned()
            .unwrap_or_else(|| p.value.clone())
    }

    /// 按**属性键**取一条属性的当前值（覆盖表优先 → schema 默认值；键不在工程里 ⇒ `None`）。
    /// 「依赖显示」条件求值用它 —— 条件里引用的是**兄弟属性的键**（如 `newproperty.value`），
    /// 手上只有键、没有那条 `Prop`。口径与 [`Self::prop_value`] 完全一致（同一张覆盖表）。
    fn prop_value_of(&self, id: &str, key: &str, props: &[Prop]) -> Option<serde_json::Value> {
        if let Some(v) = self.prop_edits.get(id).and_then(|m| m.get(key)) {
            return Some(v.clone());
        }
        props.iter().find(|p| p.key == key).map(|p| p.value.clone())
    }

    // ── 视图 ───────────────────────────────────────────────────────────────

    /// 已安装页：WE 式搜索行（搜索框 + 筛选/排序）+ 16:9 缩略图网格。
    fn view_wallpapers(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let list = self.filtered_wallpapers();

        let type_label = if self.type_filter == 0 {
            "全部".to_string()
        } else {
            self.known_types
                .get(self.type_filter - 1)
                .cloned()
                .unwrap_or_else(|| "全部".into())
        };
        let tag_label = if self.tag_filter == 0 {
            "全部".to_string()
        } else {
            self.known_tags
                .get(self.tag_filter - 1)
                .cloned()
                .unwrap_or_else(|| "全部".into())
        };

        let paused = self.native.as_ref().is_some_and(|n| n.user_paused);

        // 类型 / 标签筛选：**下拉选择**（点一下展开、选一个），不再是"点一下跳下一个"的循环按钮。
        // 第 0 项固定是「全部」；后面是见过的类型 / 标签（去重在 `adopt_wallpapers` 里做）。
        // 选中的名字 → 下标的映射在闭包里做（闭包借不到 self，所以先把清单搬出来）。
        let type_opts: Vec<String> = std::iter::once("全部".to_string())
            .chain(self.known_types.iter().cloned())
            .collect();
        let tag_opts: Vec<String> = std::iter::once("全部".to_string())
            .chain(self.known_tags.iter().cloned())
            .collect();
        let types_for_map = self.known_types.clone();
        let tags_for_map = self.known_tags.clone();
        let filter_combo = |opts: Vec<String>, sel: String, map: Vec<String>, tag: bool| {
            cosmic::iced::widget::pick_list(opts, Some(sel), move |label: String| {
                let idx = if label == "全部" {
                    0
                } else {
                    map.iter().position(|t| *t == label).map_or(0, |i| i + 1)
                };
                if tag {
                    Message::SetTagFilter(idx)
                } else {
                    Message::SetTypeFilter(idx)
                }
            })
            .text_size(11.0)
            .padding([2.0, 4.0])
            .width(Length::Fixed(132.0))
        };
        let type_combo = filter_combo(type_opts, type_label.clone(), types_for_map, false);
        let tag_combo = filter_combo(tag_opts, tag_label.clone(), tags_for_map, true);

        // 右侧那个排序下拉：真的排序（名字 / 类型 / 大小）
        let sort_opts: Vec<String> = SORTS.iter().map(|s| (*s).to_string()).collect();
        let sort_combo = cosmic::iced::widget::pick_list(
            sort_opts,
            Some(SORTS[self.sort.min(SORTS.len() - 1)].to_string()),
            |label: String| Message::SetSort(sort_index_of(&label)),
        )
        .text_size(11.0)
        .padding([2.0, 4.0])
        .width(Length::Fixed(104.0));

        // 左：搜索框 + 「类型」下拉 + 「标签」下拉；右：排序下拉 + 刷新 + 暂停/继续
        let left = widget::row::with_capacity(5)
            .push(
                widget::container(
                    widget::text_input("搜索…", &self.search)
                        .on_input(Message::SearchChanged)
                        .width(Length::Fixed(220.0)),
                )
                .style(we_ui::card_style),
            )
            .push(we_ui::label("类型", 11.0, we_ui::ink_dim()))
            .push(
                widget::container(type_combo)
                    .style(we_ui::card_style)
                    .padding([1.0, 2.0]),
            )
            .push(we_ui::label("标签", 11.0, we_ui::ink_dim()))
            .push(
                widget::container(tag_combo)
                    .style(we_ui::card_style)
                    .padding([1.0, 2.0]),
            )
            .spacing(spacing.space_xs)
            .align_y(Alignment::Center);

        // 「尺寸」滑杆（小/中/大 三档，离散 step=1）：改卡片宽高 ⇒ `tiles_per_row` 立刻重算列数。
        let size_slider = widget::slider(0.0..=2.0, self.card_idx as f64, |v: f64| {
            Message::SetCardSize(v.round() as usize)
        })
        .step(1.0)
        .width(Length::Fixed(84.0))
        .class(we_ui::slider_class());
        let size_label = ["小", "中", "大"][self.card_idx.min(2)];
        let right = widget::row::with_capacity(6)
            .push(we_ui::label("尺寸", 11.0, we_ui::ink_dim()))
            .push(size_slider)
            .push(we_ui::label(size_label, 11.0, we_ui::ink_dim()))
            .push(
                widget::container(sort_combo)
                    .style(we_ui::card_style)
                    .padding([1.0, 2.0]),
            )
            .push(we_ui::icon_button(
                "刷新",
                "view-refresh-symbolic",
                we_ui::gray_class(),
                Message::Refresh,
            ))
            .push(we_ui::icon_button(
                if paused { "继续" } else { "暂停" },
                if paused {
                    "media-playback-start-symbolic"
                } else {
                    "media-playback-pause-symbolic"
                },
                we_ui::gray_class(),
                Message::PauseToggle,
            ))
            .spacing(spacing.space_xs)
            .align_y(Alignment::Center);

        let header = widget::row::with_capacity(2)
            .push(left)
            .push(
                widget::container(right)
                    .width(Length::Fill)
                    .align_x(Horizontal::Right),
            )
            .spacing(spacing.space_s)
            .align_y(Alignment::Center);

        let body: Element<_> = if list.is_empty() {
            widget::container(we_ui::label(
                "没有壁纸（或当前筛选下没有）。把 scene 壁纸目录放进本地库根目录（默认 ~/Templates），再点「刷新」。不需要 Steam。",
                12.0,
                we_ui::ink_dim(),
            ))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Horizontal::Center)
            .align_y(Vertical::Center)
            .into()
        } else {
            // **自动换行**：按窗口**实际宽度**算每行放几个，再用 iced 的 row 分块。
            // ⚠ 别用 `widget::flex_row` / `widget::grid` ✗ —— 它们底下的布局会把格子沿纵向
            // 拉到行高（实测格子 580px 而内容只有 230px ⇒ 每行下面一大块空白）。
            // iced 自己的 `row` 不拉伸子项 ⇒ 高度正常；列数则由 `win_w`（订阅窗口 resize 得到）算。
            let per_row = tiles_per_row(self.win_w, self.card_w());
            let mut rows = widget::column::with_capacity(list.len() / per_row + 1).spacing(TILE_GAP);
            let mut chunk: Vec<Element<'_, Message>> = Vec::with_capacity(per_row);
            for &entry in &list {
                chunk.push(self.wallpaper_tile(entry));
                if chunk.len() == per_row {
                    rows = rows.push(
                        widget::row::with_children(std::mem::take(&mut chunk))
                            .spacing(TILE_GAP)
                            .align_y(Alignment::Start),
                    );
                }
            }
            if !chunk.is_empty() {
                rows = rows.push(
                    widget::row::with_children(chunk)
                        .spacing(TILE_GAP)
                        .align_y(Alignment::Start),
                );
            }
            widget::scrollable(rows).height(Length::Fill).into()
        };

        widget::column::with_capacity(2)
            .push(header)
            .push(body)
            .spacing(spacing.space_s)
            .height(Length::Fill)
            .into()
    }

    /// 一张 16:9 缩略图：底部半透明黑条叠标题、左上角类型角标、选中蓝描边。
    fn wallpaper_tile<'a>(&'a self, entry: &'a proto::WallpaperEntry) -> Element<'a, Message> {
        let selected = self.selected.as_deref() == Some(entry.id.as_str());

        let thumb: Element<_> = if !entry.preview.is_empty() && Path::new(&entry.preview).exists() {
            // CPU 预缩放（Lanczos3）到卡片**正好那么大** ⇒ 上屏 1:1，不糊也不锯齿
            // （iced/wgpu 那条路没有 mipmap，直接缩原图会走样，见 `thumb_from_path`）。
            let handle = thumb_from_path(&entry.preview, self.card_w() as u32, self.card_h() as u32);
            widget::image::Image::new(handle)
                .content_fit(cosmic::iced::ContentFit::Cover)
                .width(Length::Fixed(self.card_w()))
                .height(Length::Fixed(self.card_h()))
                .into()
        } else {
            // 没封面（半残条目）⇒ 拿类型占位，不留空洞
            widget::container(we_ui::label(
                if entry.wp_type.is_empty() {
                    "无预览".to_string()
                } else {
                    entry.wp_type.clone()
                },
                11.0,
                we_ui::ink_dim(),
            ))
            .width(Length::Fixed(self.card_w()))
            .height(Length::Fixed(self.card_h()))
            .align_x(Horizontal::Center)
            .align_y(Vertical::Center)
            .style(we_ui::chip_style)
            .into()
        };

        // 底部半透明黑条 + 居中白字标题
        let bar = widget::container(we_ui::label(short_name(&entry.name, 22), 11.0, we_ui::WHITE))
            .width(Length::Fill)
            .height(Length::Fixed(22.0))
            .align_x(Horizontal::Center)
            .align_y(Vertical::Center)
            .style(we_ui::bar_style);
        let bar_layer = widget::container(bar)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_y(Vertical::Bottom);

        // 左上角类型角标
        let badge = widget::container(we_ui::label(type_badge(&entry.wp_type), 9.0, we_ui::WHITE))
            .padding([1.0, 4.0])
            .style(we_ui::badge_style(badge_color(&entry.wp_type)));
        let badge_layer = widget::container(badge)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Horizontal::Left)
            .align_y(Vertical::Top)
            .padding(3.0);

        let stack = cosmic::iced::widget::Stack::new()
            .push(thumb)
            .push(bar_layer)
            .push(badge_layer)
            .width(Length::Fixed(self.card_w()))
            .height(Length::Fixed(self.card_h()));

        // 选中 = 强调色高亮描边（颜色取自当前主题）
        let framed = widget::container(stack).style(move |t: &CTheme| ContStyle {
            border: Border {
                color: if selected {
                    t.cosmic().accent_color().into()
                } else {
                    t.cosmic().background(false).component.border.into()
                },
                width: if selected { 2.0 } else { 1.0 },
                radius: t.cosmic().radius_s()[0].into(),
            },
            ..ContStyle::default()
        });

        widget::button::custom(framed)
            .class(we_ui::bare_class())
            .padding([0.0, 0.0])
            // 按下即选中（on_press_down）：见 we_ui::icon_button 上面那段说明 ——
            // 这批按钮的"松开"事件在这个壳里判定不到按钮矩形，用 on_press 会点半天没反应。
            .on_press_down(Message::Select(entry.id.clone()))
            .into()
    }


    /// 发现页：创意工坊热门内容，用**和已安装页一样的 16:9 格子**铺开。
    fn view_discover(&self) -> Element<'_, Message> {
        self.view_workshop_body(true)
    }

    /// 创意工坊页：关键词搜索 + 排序 + 翻页（缩略图网格，和发现页一致；用户 要求）。
    fn view_workshop(&self) -> Element<'_, Message> {
        self.view_workshop_body(true)
    }

    /// 两页共用的工坊视图：`as_grid` = 缩略图网格（发现页）还是列表（创意工坊页）。
    fn view_workshop_body(&self, as_grid: bool) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let sort_label = workshop::SORTS[self.discover_sort % workshop::SORTS.len()].1;
        let sort_txt = format!("排序: {sort_label}");
        let page_txt = format!("第 {} 页 →", self.discover_page);

        let header = widget::row::with_capacity(4)
            .push(
                widget::container(
                    widget::text_input("搜索创意工坊（留空看趋势）…", &self.discover_query)
                        .on_input(Message::DiscoverQueryChanged)
                        .on_submit(|_| Message::DiscoverRunSearch)
                        .width(Length::Fixed(240.0)),
                )
                .style(we_ui::card_style),
            )
            .push(we_ui::icon_button(
                "搜索",
                "system-search-symbolic",
                we_ui::filled_class(),
                Message::DiscoverRunSearch,
            ))
            .push(we_ui::icon_button(
                "在浏览器打开",
                "web-browser-symbolic",
                we_ui::gray_class(),
                Message::DiscoverSearch,
            ))
            .push(
                widget::container(
                    widget::row::with_capacity(2)
                        .push(we_ui::icon_button(
                            sort_txt,
                            "view-sort-ascending-symbolic",
                            we_ui::gray_class(),
                            Message::DiscoverSortCycle,
                        ))
                        .push(we_ui::icon_button(
                            page_txt,
                            "go-next-symbolic",
                            we_ui::gray_class(),
                            Message::DiscoverNextPage,
                        ))
                        .spacing(spacing.space_xs)
                        .align_y(Alignment::Center),
                )
                .width(Length::Fill)
                .align_x(Horizontal::Right),
            )
            .spacing(spacing.space_xs)
            .align_y(Alignment::Center);

        // 本地已下载的工坊 ID（用 external_id 对照）
        let local_ids: std::collections::HashSet<&str> = self
            .wallpapers
            .iter()
            .map(|w| w.external_id.as_str())
            .filter(|s| !s.is_empty())
            .collect();
        let missing = self
            .discover_items
            .iter()
            .filter(|i| !local_ids.contains(i.id.as_str()))
            .count();

        let mut status = vec![format!("工坊结果 {} 条", self.discover_items.len())];
        // 页面里的链接次数 vs 上屏的唯一 id：两个数**分开印**，因为 Steam 的 browse 页对
        // 每个条目有**两处**链接（缩略图 + 标题）⇒ 30 条的页面上是 60 次出现，
        // 那是"页面内部重复"（解析时已按 id 折叠），**不是**列表里列了两格同一 id。
        if self.discover_links > self.discover_items.len() {
            status.push(format!(
                "页面链接 {} 次 → 唯一 id {}（页面内同 id 已折叠）",
                self.discover_links,
                self.discover_items.len()
            ));
        }
        if self.discover_dedup > 0 {
            status.push(format!("同 id 重复已折叠 {} 条", self.discover_dedup));
        } else {
            status.push("同一 id 只列一次（无重复）".into());
        }
        if !self.discover_items.is_empty() {
            status.push(format!("其中 {missing} 条未下载"));
        }
        if self.discover_loading {
            status.push("正在请求创意工坊…".into());
        }
        // 渐进式填充的进度：条目已经出来了、图还在后台逐张补
        if self.discover_thumbs_loading && !self.discover_items.is_empty() {
            status.push(format!(
                "缩略图 {}/{}",
                self.discover_thumbs.len(),
                self.discover_items.len()
            ));
        }
        let mut status_col = widget::column::with_capacity(4)
            .push(we_ui::label(status.join("   ·   "), 11.0, we_ui::ink_dim()))
            .spacing(spacing.space_xxs);

        // 失败要给**明确的出路**：一条人话 + 一个 [重试] 按钮（与顶栏 [搜索] **同一条** Message）
        if let Some(err) = &self.discover_error {
            status_col = status_col.push(
                widget::row::with_capacity(3)
                    .push(we_ui::label(format!("⚠ {err}"), 11.0, we_ui::ink_danger()))
                    .push(we_ui::icon_button(
                        "重试",
                        "view-refresh-symbolic",
                        we_ui::filled_class(),
                        Message::DiscoverRunSearch,
                    ))
                    .push(we_ui::label(
                        "（工坊要联网；失败不会再自己重试，点这里或顶栏的「搜索」再拉一次）",
                        11.0,
                        we_ui::ink_dim(),
                    ))
                    .spacing(spacing.space_xs)
                    .align_y(Alignment::Center),
            );
        }

        // 状态条装进一张卡片里：这一行是小字、又要贴着壁纸看（窗口是半透明的），
        // 直接浮在窗口底色上在浅色壁纸前几乎读不出来。给它一层主题底色，字才看得见。
        let status_card = widget::container(status_col)
            .width(Length::Fill)
            .padding([4.0, 8.0])
            .style(we_ui::card_style);

        if self.discover_items.is_empty() && !self.discover_loading {
            return widget::column::with_capacity(3)
                .push(header)
                .push(status_card)
                .push(we_ui::label(
                    "点「搜索」拉取创意工坊的热门内容（可先不填关键词）。下载后的条目会自动出现在「已安装」页。",
                    12.0,
                    we_ui::ink_dim(),
                ))
                .spacing(spacing.space_s)
                .height(Length::Fill)
                .into();
        }

        let body: Element<_> = if as_grid {
            let per_row = tiles_per_row(self.win_w, self.card_w());
            let mut rows =
                widget::column::with_capacity(self.discover_items.len() / per_row + 1).spacing(TILE_GAP);
            let mut chunk: Vec<Element<'_, Message>> = Vec::with_capacity(per_row);
            for item in &self.discover_items {
                chunk.push(self.workshop_tile(item, local_ids.contains(item.id.as_str())));
                if chunk.len() == per_row {
                    rows = rows.push(
                        widget::row::with_children(std::mem::take(&mut chunk))
                            .spacing(TILE_GAP)
                            .align_y(Alignment::Start),
                    );
                }
            }
            if !chunk.is_empty() {
                rows = rows.push(
                    widget::row::with_children(chunk)
                        .spacing(TILE_GAP)
                        .align_y(Alignment::Start),
                );
            }
            widget::scrollable(rows).height(Length::Fill).into()
        } else {
            let mut list =
                widget::column::with_capacity(self.discover_items.len().max(1)).spacing(spacing.space_xs);
            for item in &self.discover_items {
                list = list.push(self.workshop_row(item, local_ids.contains(item.id.as_str())));
            }
            widget::scrollable(list).height(Length::Fill).into()
        };

        widget::column::with_capacity(3)
            .push(header)
            .push(status_card)
            .push(body)
            .spacing(spacing.space_s)
            .height(Length::Fill)
            .into()
    }

    /// 工坊条目：16:9 格子（发现页用，和已安装页同一种格子）
    /// `downloaded` = 本地已有这条（标题带 ✓）。没有订阅角标 —— 登录/订阅整套 UI 已删除。
    fn workshop_tile<'a>(
        &'a self,
        item: &'a workshop::Item,
        downloaded: bool,
    ) -> Element<'a, Message> {
        let thumb: Element<_> = match self.discover_thumbs.get(&item.id) {
            // 同样按卡片尺寸预缩放（缓存按 id + 尺寸），上屏 1:1
            Some(handle) => widget::image::Image::new(match self.discover_thumb_raw.get(&item.id) {
                Some(raw) => thumb_from_bytes(
                    &format!("d:{}", item.id),
                    raw,
                    self.card_w() as u32,
                    self.card_h() as u32,
                ),
                None => handle.clone(),
            })
            .content_fit(cosmic::iced::ContentFit::Cover)
            .width(Length::Fixed(self.card_w()))
            .height(Length::Fixed(self.card_h()))
            .into(),
            None => widget::container(we_ui::label("无预览", 11.0, we_ui::ink_dim()))
                .width(Length::Fixed(self.card_w()))
                .height(Length::Fixed(self.card_h()))
                .align_x(Horizontal::Center)
                .align_y(Vertical::Center)
                .style(we_ui::chip_style)
                .into(),
        };

        let title = strip_markup(&item.title);
        let caption = if downloaded {
            format!("✓ {title}")
        } else {
            title
        };
        let bar = widget::container(we_ui::label(short_name(&caption, 22), 11.0, we_ui::WHITE))
            .width(Length::Fill)
            .height(Length::Fixed(22.0))
            .align_x(Horizontal::Center)
            .align_y(Vertical::Center)
            .style(we_ui::bar_style);
        let bar_layer = widget::container(bar)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_y(Vertical::Bottom);

        let stack = cosmic::iced::widget::Stack::new()
            .push(thumb)
            .push(bar_layer);
        let stack = stack
            .width(Length::Fixed(self.card_w()))
            .height(Length::Fixed(self.card_h()));

        let framed = widget::container(stack).style(move |t: &CTheme| ContStyle {
            border: Border {
                color: t.cosmic().background(false).component.border.into(),
                width: 1.0,
                radius: t.cosmic().radius_s()[0].into(),
            },
            ..ContStyle::default()
        });

        // 点缩略图 = 在**程序内**看这条的工坊详情（右侧面板，只读 + [工坊页] 链接）：
        // "浏览创意工坊"因此不再需要跳浏览器。"网页版地址"由详情面板的 [工坊页] 提供。
        widget::button::custom(framed)
            .class(we_ui::bare_class())
            .padding([0.0, 0.0])
            .on_press_down(Message::SelectWorkshop(item.id.clone()))
            .into()
    }

    /// 工坊条目：列表行（创意工坊页用，标签/体积/工坊自己的订阅数都摆出来）
    fn workshop_row<'a>(
        &'a self,
        item: &'a workshop::Item,
        downloaded: bool,
    ) -> Element<'a, Message> {
        let spacing = cosmic::theme::spacing();
        let thumb: Element<_> = match self.discover_thumbs.get(&item.id) {
            Some(handle) => widget::image::Image::new(handle.clone())
                .content_fit(cosmic::iced::ContentFit::Cover)
                .width(Length::Fixed(160.0))
                .height(Length::Fixed(90.0))
                .into(),
            None => widget::container(we_ui::label("无预览", 11.0, we_ui::ink_dim()))
                .width(Length::Fixed(160.0))
                .height(Length::Fixed(90.0))
                .align_x(Horizontal::Center)
                .align_y(Vertical::Center)
                .style(we_ui::chip_style)
                .into(),
        };

        let mut chips = widget::row::with_capacity(4).spacing(spacing.space_xxs);
        // 工坊 id 摆**第一位**：列表里唯一能一眼核对"同一 id 有没有列两次"的字段
        // （标题可以极像甚至完全同名 —— 那是两条不同的条目，见 workshop.rs 的假阳性单测）。
        // 放最前面是因为这一排本来就会超出本行宽度被裁掉尾部（标签多的时候），id 必须在**看得见**的那一段里。
        chips = chips.push(we_ui::chip(format!("id {}", item.id)));
        for t in item.tags.iter().take(6) {
            chips = chips.push(we_ui::chip(t.clone()));
        }
        chips = chips.push(we_ui::chip(format!(
            "{:.1} MB",
            item.size_bytes as f64 / 1024.0 / 1024.0
        )));
        chips = chips.push(we_ui::chip(format!("订阅 {}", item.subscriptions)));
        if downloaded {
            chips = chips.push(we_ui::chip("✓ 已下载"));
        }

        let info = widget::column::with_capacity(2)
            .push(we_ui::label(strip_markup(&item.title), 14.0, we_ui::ink_text()))
            .push(chips);

        let url = format!(
            "https://steamcommunity.com/sharedfiles/filedetails/?id={}",
            item.id
        );

        // 只留 [工坊页]（明说是浏览器）—— 登录/订阅入口已整体删除。
        let actions = widget::row::with_capacity(2)
            .spacing(spacing.space_xs)
            .align_y(Alignment::Center)
            .push(we_ui::icon_button(
                "工坊页",
                "web-browser-symbolic",
                we_ui::gray_class(),
                Message::LaunchUrl(url),
            ));

        // 左边一整块**可点**（= 在程序内看这条的工坊详情，不跳网页）
        let head = widget::button::custom(
            widget::row::with_capacity(2)
                .push(thumb)
                .push(widget::container(info).width(Length::Fill))
                .spacing(spacing.space_s)
                .align_y(Alignment::Center),
        )
        .class(we_ui::bare_class())
        .padding([0.0, 0.0])
        .width(Length::Fill)
        .on_press_down(Message::SelectWorkshop(item.id.clone()));

        widget::container(
            widget::row::with_capacity(2)
                .push(head)
                .push(actions)
                .spacing(spacing.space_s)
                .align_y(Alignment::Center),
        )
        .padding(spacing.space_s)
        .style(we_ui::card_style)
        .width(Length::Fill)
        .into()
    }

}

/// 内核二进制的位置。
///
/// 查找顺序：`WPHUB_KERNEL` 指定 → `~/.local/bin/we-native`（装好的）
/// → 当前可执行文件同目录（`cargo build` 出来的两个二进制挨着）
/// → `PATH` 里的 `we-native`。
fn native_kernel_binary() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("WPHUB_KERNEL") {
        return std::path::PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let installed = std::path::PathBuf::from(&home).join(".local/bin/we-native");
    if installed.exists() {
        return installed;
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join("we-native");
        if sibling.exists() {
            return sibling;
        }
    }
    if let Ok(out) = std::process::Command::new("which").arg("we-native").output()
        && out.status.success()
    {
        let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !p.is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    std::path::PathBuf::from("we-native")
}

fn native_running() -> bool {
    std::process::Command::new("pgrep")
        .args(["-x", "we-native"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// 起自研内核（常驻；**懒创建表面**，没被 apply 之前不占背景层）
fn spawn_native_kernel() -> Result<String, String> {
    if native_running() {
        return Ok("自研内核已在运行".into());
    }
    let bin = native_kernel_binary();
    if !bin.exists() {
        return Err(format!(
            "找不到自研内核：{}（先 `cargo build --release -p we-native`，或设 WPHUB_KERNEL 指向它）",
            bin.display()
        ));
    }
    let child = std::process::Command::new(&bin)
        .arg("--serve")
        .arg(native::sock_path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("起自研内核失败: {e}"))?;
    std::thread::sleep(std::time::Duration::from_millis(600));
    Ok(format!("已切到自研内核（pid {}）", child.id()))
}

/// 确认自研内核在跑（waywallen 已彻底移除，不再需要"先停对方"这一步）。
/// 界面里现在没有直接调用它 —— 内核由 systemd 服务托管。保留是因为它仍是"确保后端可用"的
/// 正确动作，将来做命令行/自检要用。
#[allow(dead_code)]
fn switch_to_native() -> Result<String, String> {
    spawn_native_kernel()
}

/// 重启自研内核服务。
/// 为什么需要它：会话锁屏时 layer surface 会被销毁，而内核**不会重建自己的 surface**，于是变成
/// 「进程活着、socket 活着、帧也照收，但屏幕上什么都没有」的僵尸状态 —— 表现为：渲染器在跑、
/// `apply` 也成功，可桌面上看不出任何变化。重启内核服务即可恢复。
/// 内核现在由 systemd 用户服务托管（`wallpaper-native-kernel`），直接 restart 服务即可。
fn restart_native_kernel() -> Result<String, String> {
    let out = std::process::Command::new("systemctl")
        .args(["--user", "restart", "wallpaper-native-kernel"])
        .output()
        .map_err(|e| format!("调用 systemctl 失败: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "重启内核服务失败：{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    std::thread::sleep(std::time::Duration::from_millis(800));
    Ok("已重启自研内核服务".into())
}

// （原来这里还有 switch_to_waywallen —— waywallen 后端已整体移除，函数一并删掉。）

/// 去掉壁纸/工坊文本里的 HTML 与 BBCode 标记。
/// WE 的 `project.json` 和创意工坊文本里常带 `<style>…</style>`、`<b>`、`[b]` 这类标记，
/// 原样显示就是乱码（用户明确提过这点）。
fn strip_markup(text: &str) -> String {
    let mut s = text.to_string();

    // 1) 整块去掉 <style>/<script>（连同里面的内容）
    for tag in ["style", "script"] {
        loop {
            let lower = s.to_lowercase();
            let open = format!("<{tag}");
            let close = format!("</{tag}>");
            let Some(start) = lower.find(&open) else { break };
            match lower[start..].find(&close) {
                Some(rel_end) => {
                    s.replace_range(start..start + rel_end + close.len(), " ")
                }
                None => {
                    // 没有闭合标签就把剩下全丢掉
                    s.replace_range(start.., " ");
                    break;
                }
            }
        }
    }

    // 2) 去 HTML 标签 + 3) 去 BBCode 标签（都只去标签，保留其间的内容）
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '<' => {
                // 跳到 '>'
                while i < chars.len() && chars[i] != '>' {
                    i += 1;
                }
                i += 1;
                out.push(' ');
            }
            '[' => {
                // 只在"看起来像标签"时才吞掉：[、可选 /、字母，然后到 ]
                let mut j = i + 1;
                if j < chars.len() && chars[j] == '/' {
                    j += 1;
                }
                let looks_like_tag = j < chars.len() && chars[j].is_ascii_alphabetic();
                if looks_like_tag {
                    while j < chars.len() && chars[j] != ']' {
                        j += 1;
                    }
                    if j < chars.len() {
                        i = j + 1;
                        out.push(' ');
                        continue;
                    }
                }
                out.push(chars[i]);
                i += 1;
            }
            ch => {
                out.push(ch);
                i += 1;
            }
        }
    }

    // 合并空白
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 值 → 协议要求的字符串。
/// 实测：bool → "true"/"false"；slider → "200"；color → "0.2 0.4 0.9"（空格分隔）
/// 值 → 协议字符串（协议约定：滑块写 "200"、开关写 "true"/"false"、颜色写 "r g b"）。
/// 运行时走的是 fmt_f64 / 裸字符串那条路，这里目前只被单元测试当权威实现用，所以标 allow。
#[allow(dead_code)]
fn props_value_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => fmt_f64(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::Array(items) => items
            .iter()
            .map(props_value_string)
            .collect::<Vec<_>>()
            .join(" "),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// 数字转字符串：整数不要带 .0
fn fmt_f64(value: f64) -> String {
    if (value - value.round()).abs() < 1e-9 {
        format!("{}", value.round() as i64)
    } else {
        format!("{value}")
    }
}

/// 四项设置的界面标签（性能页与档位说明共用同一套词）
fn scale_label(value: &str) -> &'static str {
    if value == "logical" {
        "逻辑尺寸"
    } else {
        "物理尺寸（锐）"
    }
}

fn effects_label(value: &str) -> &'static str {
    if value == "none" {
        "无特效"
    } else {
        "全部特效"
    }
}

fn sleep_label(value: &str) -> &'static str {
    match value {
        "never" => "从不睡眠",
        "maximized" => "最大化时睡眠",
        _ => "全屏时睡眠",
    }
}

/// 配置里的色彩串 ⇒ 六个浮点；空串或坏值一律当"单位值"（= 不做处理）。
fn color_from_config(s: &str) -> [f32; 6] {
    let v: Vec<f32> = s.split(',').filter_map(|t| t.trim().parse().ok()).collect();
    if v.len() == 6 {
        [v[0], v[1], v[2], v[3], v[4], v[5]]
    } else {
        [1.0, 1.0, 1.0, 0.0, 0.0, 1.0]
    }
}

/// 六个浮点 ⇒ 配置串（跟内核收到的格式一致：逗号分隔）
fn color_to_config(c: &[f32; 6]) -> String {
    format!("{},{},{},{},{},{}", c[0], c[1], c[2], c[3], c[4], c[5])
}

/// 一次调色**写到哪** —— 从 `set_color` 里拆出来的纯函数（落点判定只在这一处，改错了单测先响）。
///
/// * 有选中的壁纸 ⇒ 只动**这条壁纸自己的槽**（[`config::Config::color_by_id`]）；中性值 = 删掉这条覆盖；
///
/// * **没有选中的壁纸 ⇒ `Nothing`，一个字也不写**。
/// ⚠ （实测的 bug）：以前这条 `None` 分支会把值写进**全局** `color`（`Config::color`）。
/// 那个字段只是**老配置的迁移默认值**，写它等于把所有"还没记过槽"的壁纸一起改掉 ——
/// 真实触发路径是 `--perf-msg color-reset`：性能自检在界面**选中任何壁纸之前**就按了那个按钮
/// （见启动参数那一段的顺序），于是通用色彩落到了全局，把"按壁纸 id 分槽"的设计打穿。
/// 现在没有具体壁纸就没有"这条壁纸的色彩"可改 ⇒ 如实忽略，别去动全局。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ColorWrite {
    /// 写进了这条壁纸的槽（`value` = 落盘用的字符串；中性值表示删掉了这条覆盖）
    Slot { id: String, value: String },
    /// 什么都没写（没有选中的壁纸）
    Nothing,
}

fn write_color(cfg: &mut config::Config, selected: Option<&str>, c: [f32; 6]) -> ColorWrite {
    let Some(id) = selected else {
        return ColorWrite::Nothing;
    };
    let s = color_to_config(&c);
    if c == [1.0f32, 1.0, 1.0, 0.0, 0.0, 1.0] {
        cfg.color_by_id.remove(id);
    } else {
        cfg.color_by_id.insert(id.to_string(), s.clone());
    }
    ColorWrite::Slot {
        id: id.to_string(),
        value: s,
    }
}

/// 第 i 项的合理值域（界面按钮按它夹取；内核不做值域校验，那是策略层的事）
fn color_bounds(i: usize) -> (f32, f32) {
    match i {
        0 => (0.2, 3.0),    // 亮度
        1 => (0.2, 3.0),    // 对比度
        2 => (0.0, 3.0),    // 饱和度（0 = 黑白）
        3 => (-3.14, 3.14), // 色相（弧度）
        4 => (-1.0, 1.0),   // 色温
        _ => (0.2, 3.0),    // 伽马
    }
}

impl AppModel {
    /// 顶栏左侧三个标签：选中 = 蓝实心，未选中 = 白底细描边（都带小图标）。
    fn view_tabs(&self, active: Page) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let mut row = widget::row::with_capacity(3).spacing(spacing.space_xxs);
        for (page, label, icon_name) in [
            (Page::Wallpapers, "已安装", "applications-graphics-symbolic"),
            (Page::Discover, "发现", "system-search-symbolic"),
            (Page::Workshop, "创意工坊", "folder-download-symbolic"),
        ] {
            row = row.push(we_ui::icon_button(
                label,
                icon_name,
                we_ui::tab_class(page == active),
                Message::GoPage(page),
            ));
        }
        row.align_y(Alignment::Center).into()
    }

    /// 顶栏右侧那组灰底小按钮（显示 / 设置 / 关于）。
    fn view_top_actions(&self, page: Page) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        widget::row::with_capacity(3)
            .push(we_ui::icon_button(
                "显示",
                "view-reveal-symbolic",
                if self.show_panel {
                    we_ui::filled_class()
                } else {
                    we_ui::gray_class()
                },
                Message::TogglePanel,
            ))
            .push(we_ui::icon_button(
                "设置",
                "preferences-system-symbolic",
                if page == Page::Settings {
                    we_ui::filled_class()
                } else {
                    we_ui::gray_class()
                },
                Message::GoPage(Page::Settings),
            ))
            .push(we_ui::icon_button(
                "关于",
                "help-about-symbolic",
                we_ui::gray_class(),
                Message::ToggleContextPage(ContextPage::About),
            ))
            .spacing(spacing.space_xs)
            .align_y(Alignment::Center)
            .into()
    }

    /// 右侧面板的「工坊条目」一节：**还没下载到本地**的条目也能在程序里看，
    /// 只读：浏览 / 详情 / 工坊页链接复制。**没有任何登录、订阅、下载入口**。
    fn view_workshop_detail(&self, item: &workshop::Item) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let id = item.id.clone();
        let downloaded = self.wallpapers.iter().any(|w| w.id == id);
        let (fw, fh) = detail_preview_size(self.win_w, self.card_w());

        let preview: Element<_> = match self.discover_thumbs.get(&id) {
            Some(handle) => {
                // 大图也走同一套 CPU 预缩放（Lanczos3 + 16:9 居中裁剪，缓存键按 id+尺寸），
                // 上屏 1:1 ⇒ 放到 520 宽也不糊（源图 514~1079 见方，永远是缩小）。
                let hd = match self.discover_thumb_raw.get(&id) {
                    Some(raw) => thumb_from_bytes(
                        &format!("d:{id}"),
                        raw,
                        fw as u32,
                        fh as u32,
                    ),
                    None => handle.clone(),
                };
                widget::image::Image::new(hd)
                    .content_fit(cosmic::iced::ContentFit::Cover)
                    .width(Length::Fixed(fw))
                    .height(Length::Fixed(fh))
                    .into()
            }
            None => widget::container(we_ui::label(
                "缩略图还没下到（发现页在后台逐张补；也可以点 [重试]）",
                11.0,
                we_ui::ink_dim(),
            ))
            .width(Length::Fixed(fw))
            .height(Length::Fixed(fh))
            .align_x(Horizontal::Center)
            .align_y(Vertical::Center)
            .style(we_ui::chip_style)
            .into(),
        };

        let mut chips = widget::row::with_capacity(7).spacing(spacing.space_xxs);
        chips = chips.push(we_ui::chip(if downloaded { "✓ 本地已有" } else { "未下载" }));
        for t in item.tags.iter().take(4) {
            chips = chips.push(we_ui::chip(t.clone()));
        }
        chips = chips.push(we_ui::chip(format!(
            "{:.1} MB",
            item.size_bytes as f64 / 1024.0 / 1024.0
        )));
        chips = chips.push(we_ui::chip(format!("订阅 {}", item.subscriptions)));

        let mut col = widget::column::with_capacity(9)
            .spacing(spacing.space_s)
            .width(Length::Fill);
        col = col.push(we_ui::label(
            "创意工坊（还没下载到本地）",
            11.0,
            we_ui::ink_dim(),
        ));
        col = col.push(
            widget::container(preview)
                .width(Length::Fill)
                .align_x(Horizontal::Center),
        );
        col = col.push(
            widget::container(we_ui::label(strip_markup(&item.title), 16.0, we_ui::ink_text()))
                .width(Length::Fill)
                .align_x(Horizontal::Center),
        );
        // 工坊 id：**唯一**能核对"是不是同一条、有没有列两次"的字段（标题可以极像甚至同名）。
        // 公开数据，与账号身份无关；放在标题正下方一行，正常字号、不被 chips 挤掉。
        col = col.push(
            widget::container(we_ui::label(format!("工坊 id {id}"), 11.0, we_ui::ink_dim()))
                .width(Length::Fill)
                .align_x(Horizontal::Center),
        );
        col = col.push(chips);
        // 只留「工坊页」这一个出口：明写是浏览器（不在应用内假装订阅/下载）。
        col = col.push(
            widget::row::with_capacity(1)
                .spacing(spacing.space_xs)
                .align_y(Alignment::Center)
                .push(we_ui::icon_button(
                    "工坊页",
                    "web-browser-symbolic",
                    we_ui::gray_class(),
                    Message::LaunchUrl(workshop_url(&id)),
                )),
        );
        col = col.push(we_ui::label(
            "想看更多/订阅/下载 ⇒ 点 [工坊页] 在浏览器或 Steam 客户端里操作；本程序只做浏览与属性调节。",
            11.0,
            we_ui::ink_dim(),
        ));

        let bottom = widget::row::with_capacity(1).push(
            widget::button::custom(
                widget::container(we_ui::label("取消", 12.0, we_ui::ink_text()))
                    .width(Length::Fill)
                    .align_x(Horizontal::Center),
            )
            .class(we_ui::ghost_class())
            .padding([5.0, 8.0])
            .width(Length::Fill)
            .on_press_down(Message::Deselect),
        );

        let mut sc = widget::scrollable(col)
            .id(panel_scroll_id())
            .height(Length::Fill);
        if self.panel_bottom {
            sc = sc.anchor_y(cosmic::iced::widget::scrollable::Anchor::End);
        }

        widget::container(
            widget::column::with_capacity(3)
                .push(sc)
                .push(we_ui::divider())
                .push(bottom)
                .spacing(spacing.space_xs)
                .height(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(spacing.space_s)
        .style(we_ui::panel_style)
        .into()
    }

    /// 右侧详情/属性面板（WE 观感）：大预览图 → 居中标题 → 评分行 → chips →
    /// 红色通栏按钮 → 小按钮行 → 「属性」分组（**引擎项 → 【宣传图】 → 细分割线 → 作者项**）
    /// →「您的预设」→ 确认/取消。
    /// 「宣传图」段的位置：**壁纸参数区的顶部** —— 紧跟在「通用色彩调整」
    /// 那一组引擎项（对齐方式 + 六项色彩 + 重新下发按钮）之后、作者自定义属性之前；
    /// 原来是画在面板最上面（缩览图 / chips 一带），位置不对。
    /// 属性区**完全数据驱动**：逐条遍历 `project.json` 的 `general.properties`，
    /// 按每条自己的 `type` 出控件（bool/slider/color/combo/text），改完立即写回覆盖表
    /// 并落盘 —— 不认属性名、不特判。
    fn view_detail(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();

        let Some(entry) = self.selected_entry() else {
            // 选中的可能是**还没下载到本地**的工坊条目（发现页点缩略图）⇒ 在本程序内给一节
            // 「工坊条目」面板（用户的原始要求：浏览不用跳网页）。本地有这条时才走下面的本地面板。
            if let Some(item) = self.selected_workshop() {
                return self.view_workshop_detail(item);
            }
            return widget::container(we_ui::label(
                "从左侧点一张壁纸，这里显示详情与属性",
                12.0,
                we_ui::ink_dim(),
            ))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Horizontal::Center)
            .align_y(Vertical::Center)
            .padding(spacing.space_s)
            .style(we_ui::card_style)
            .into();
        };

        let id = entry.id.clone();
        let preview_w = panel_width(self.win_w, self.card_w()) - 20.0;

        // 顶部大图的**源文件**：默认封面；若目录里找到了更大的本地图（`preview_src`）就用那张。
        let src_path: Option<PathBuf> = self
            .preview_src
            .get(&id)
            .filter(|p| p.exists())
            .cloned()
            .or_else(|| {
                (!entry.preview.is_empty() && Path::new(&entry.preview).exists())
                    .then(|| PathBuf::from(&entry.preview))
            });
        // 显示尺寸：≤ 面板宽、≤ 上限、**绝不放大超过源图原始像素**（按源图自身比例，居中）。
        let (disp_w, disp_h) = match self.preview_dims.get(&id) {
            Some(&(sw, sh)) => preview_display_size(sw, sh, preview_w),
            None => preview_display_size(0, 0, preview_w),
        };

        let mut col = widget::column::with_capacity(10)
            .spacing(spacing.space_s)
            .width(Length::Fill);

        // 1) 大预览图：按源图像素上限显示（不再拉满面板宽、也不再按 16:9 覆盖裁剪放大）
        let preview: Element<_> = match &src_path {
            Some(p) => widget::image::Image::new(widget::image::Handle::from_path(p))
                .content_fit(cosmic::iced::ContentFit::Contain)
                .width(Length::Fixed(disp_w))
                .height(Length::Fixed(disp_h))
                .into(),
            None => widget::container(we_ui::label("无预览图", 12.0, we_ui::ink_dim()))
                .width(Length::Fixed(disp_w))
                .height(Length::Fixed(disp_h))
                .align_x(Horizontal::Center)
                .align_y(Vertical::Center)
                .style(we_ui::chip_style)
                .into(),
        };
        col = col.push(
            widget::container(preview)
                .width(Length::Fill)
                .align_x(Horizontal::Center),
        );

        // 2) 居中标题
        col = col.push(
            widget::container(we_ui::label(entry.name.clone(), 16.0, we_ui::ink_text()))
                .width(Length::Fill)
                .align_x(Horizontal::Center),
        );

        // 3) 元信息行：评分星 + 收藏 + 体积
        let size_mb = entry.size as f64 / 1024.0 / 1024.0;
        col = col.push(
            widget::row::with_capacity(3)
                .push(we_ui::label("★★★★☆", 13.0, we_ui::ink_accent()))
                .push(we_ui::label("♥", 13.0, we_ui::ink_danger()))
                .push(we_ui::label(format!("{size_mb:.1} MB"), 11.0, we_ui::ink_dim()))
                .spacing(spacing.space_s)
                .align_y(Alignment::Center),
        );

        // 4) 一排圆角浅灰 chips：类型 / 预览尺寸 / 分级 / 标签
        let mut chips = widget::row::with_capacity(4).spacing(spacing.space_xxs);
        chips = chips.push(we_ui::chip(type_badge(&entry.wp_type)));
        if let Some((w, h)) = self.dims.get(&id) {
            chips = chips.push(we_ui::chip(format!("{w} x {h}")));
        }
        if !entry.content_rating.is_empty() {
            chips = chips.push(we_ui::chip(entry.content_rating.clone()));
        }
        for t in entry.tags.iter().take(2) {
            chips = chips.push(we_ui::chip(t.clone()));
        }
        col = col.push(chips);

        // 5) 红色通栏按钮
        let url = format!(
            "https://steamcommunity.com/sharedfiles/filedetails/?id={}",
            entry.external_id
        );
        col = col.push(
            widget::button::custom(
                widget::container(we_ui::label("✕ 在创意工坊打开", 12.0, we_ui::ink_on_danger()))
                    .width(Length::Fill)
                    .align_x(Horizontal::Center),
            )
            .class(we_ui::red_class())
            .padding([6.0, 8.0])
            .width(Length::Fill)
            .on_press_down(Message::LaunchUrl(url)),
        );

        // 6) 小按钮行
        col = col.push(
            widget::row::with_capacity(3)
                .push(we_ui::icon_button(
                    "应用",
                    "media-playback-start-symbolic",
                    we_ui::filled_class(),
                    Message::Apply(id.clone()),
                ))
                .push(we_ui::icon_button(
                    "更多设置",
                    "preferences-system-symbolic",
                    we_ui::gray_class(),
                    Message::GoPage(Page::Settings),
                ))
                .push(we_ui::icon_button(
                    "打开目录",
                    "folder-symbolic",
                    we_ui::gray_class(),
                    Message::OpenSelectedDir,
                ))
                .spacing(spacing.space_xs)
                .align_y(Alignment::Center),
        );

        // 7) 「属性」分组：**上半 = 引擎内置项（带小图标）** → 一条细分割线 →
        // **下半 = 作者自定义项（只有标签 + 控件、不带图标）**，逐条遍历
        // general.properties（数据驱动、foreach，不认属性名）
        col = col.push(we_ui::section("属性"));
        let props = parse_props(&entry.user_properties_schema);

        let mut rows = widget::column::with_capacity(props.len() + 9)
            .spacing(we_ui::ROW_GAP)
            .width(Length::Fill);

        // ── 引擎内置（本程序自己的运行时项：取景 + 画面色彩）──
        rows = rows.push(self.engine_row_fit());
        // 用户 「已安装壁纸的属性里加两个调节项」—— 播放速度 / 音乐音量。
        // 都是**内核运行时**旋钮（跟壁纸作者属性无关，也不落盘到 project.json）。
        rows = rows.push(self.engine_row_speed());
        rows = rows.push(self.engine_row_volume());
        for i in 0..6 {
            rows = rows.push(self.engine_row_color(i));
        }
        rows = rows.push(self.engine_row_actions());

        // ── **作者嵌入的宣传图**：放在**壁纸参数区的顶部**（用户 「宣传图一般是
        // 壁纸参数的顶部，而不是边栏的顶部，应该在通用色彩设置的下方」）——
        // 即紧跟上面「通用色彩调整」这一组（引擎项，含对齐方式/六项色彩/重新下发按钮）之后、
        // 作者自定义项之前。原来是画在面板最上面（缩览图 / chips 一带），位置不对。
        // 数据仍来自 `promo` 模块（遍历 project.json 每条属性的 `text` + `description`；
        // 不新建第二套抓取），**默认展示**（`Config::show_promo` 默认 true，开关保留）：
        // 开着就出这一段；关掉而这条又有图时，给一行开关提示。
        if self.config.show_promo {
            if let Some(section) = self.view_promo(&id, preview_w) {
                rows = rows.push(section);
            }
        } else if let Some(n) = self.promo_count(&id) {
            rows = rows.push(self.promo_off_hint(n));
        }

        // 细分割线：自己占一拍（上下间距和属性行一样），就长在参考图那个位置
        rows = rows.push(we_ui::row_divider());

        // ── 作者自定义（project.json 的 general.properties）──
        // **按每条自己的呈现形态出东西**（`PropForm`，判据只看 type / text / value）：
        // 可编辑项才给控件；作者的标题只读加粗；图片型整行不出（已在「宣传图」段）；
        // 空 text / 本地化键跳过。这样面板里不会再有「没意义的键 + 输入框」。
        // **「依赖显示」门控**（`condition` 字段，作者写的 `<兄弟属性key>.value == <字面量>`）：
        // 条件不成立这一行就不画。判据全在 `parse_condition` / `cond_holds` 里（数据驱动，
        // 不按属性名/壁纸名特判）；求值读的是**当前值**（覆盖表优先，与控件上显示的一致），
        // 所以下拉一改、受影响的行立刻跟着进出。**只影响画不画**，值照旧落盘/下发。
        let shows = |p: &Prop| cond_holds(&p.cond, &|k: &str| self.prop_value_of(&id, k, &props));
        let author_rows: Vec<&Prop> = props
            .iter()
            .filter(|p| {
                matches!(
                    p.form,
                    PropForm::Editable(_) | PropForm::Heading(_) | PropForm::ReadOnlyNote(_)
                )
            })
            .filter(|p| shows(p))
            .collect();
        if author_rows.is_empty() {
            // 一条不剩有两种原因，别混成一句话：可能全被条件挡住了（那是真被挡住了，
            // 不是「作者没提供」），也可能是作者真的只有排版/图片。
            let gated = props
                .iter()
                .filter(|p| {
                    matches!(
                        p.form,
                        PropForm::Editable(_) | PropForm::Heading(_) | PropForm::ReadOnlyNote(_)
                    ) && !shows(p)
                })
                .count();
            rows = rows.push(we_ui::label(
                if gated > 0 {
                    format!(
                        "这里本来有 {gated} 项作者参数，但它们的「依赖显示」条件现在不成立\
                         （作者写在工程文件的 condition 里）"
                    )
                } else if props.is_empty() {
                    "这条壁纸没有可调参数".to_string()
                } else {
                    "作者没提供可调参数（作者项都是排版/图片，已归入上面的「宣传图」）".to_string()
                },
                11.0,
                we_ui::ink_dim(),
            ));
        } else {
            for p in author_rows {
                match &p.form {
                    PropForm::Editable(_) => {
                        rows = rows.push(self.prop_row(&id, p));
                        if self.open_color.as_deref() == Some(p.key.as_str()) {
                            rows = rows.push(self.color_wheel_panel(&id, p));
                        }
                    }
                    PropForm::Heading(title) => {
                        rows = rows.push(we_ui::author_heading(title.clone()));
                    }
                    PropForm::ReadOnlyNote(text) => {
                        rows = rows.push(we_ui::readonly_note(text.clone()));
                    }
                    // 不出行的三种（宣传图型 / 空 text / 本地化键）
                    PropForm::MediaOnly | PropForm::Blank | PropForm::LocalizedKey => {}
                }
            }
        }
        // 面板的滚动条是**叠在内容上**画的 ⇒ 行内容右侧留出 `SCROLL_PAD`，
        // 不然滑块最右边那个数字会被滚动条压掉一半。
        col = col.push(
            widget::container(rows)
                .width(Length::Fill)
                .padding([0.0, we_ui::SCROLL_PAD, 0.0, 0.0]),
        );

        // 8) 「您的预设」：保存 / 加载 / 应用到所有壁纸 / 分享 JSON / 重置
        // （参考图 7-4 底部那五连：两个半宽 + 三个通栏，最后一个是红底）
        col = col.push(we_ui::section("您的预设"));
        col = col.push(
            widget::container(self.view_presets(&id, &props))
                .width(Length::Fill)
                .padding([0.0, we_ui::SCROLL_PAD, 0.0, 0.0]),
        );

        // 9) 底部：确认（蓝）/ 取消（灰）—— **钉在面板底部**，不跟着内容滚走（和 WE 一样）
        let bottom = widget::row::with_capacity(2)
            .push(
                widget::button::custom(
                    widget::container(we_ui::label("确认", 12.0, we_ui::ink_on_accent()))
                        .width(Length::Fill)
                        .align_x(Horizontal::Center),
                )
                .class(we_ui::filled_class())
                .padding([5.0, 8.0])
                .width(Length::FillPortion(1))
                // 按下即触发（on_press_down），不等 release：
                // 实测这个底栏按钮**按下能进、松开不触发** —— 按下的
                // `cursor.is_over(bounds)` 为真（rect 852,828.8,258x26.8），松开时同一位置
                // 却判定为"不在按钮上"（事件仍被 Captured ⇒ 走的是 is_pressed 分支但 is_over 为假），
                // 也就是 release 那一帧按钮算出来的矩形漂了出去。按下即触发绕开这一帧的不确定性，
                // 而且与用户预期一致（点"确认"就是要立刻生效）。
                .on_press_down(Message::Apply(id.clone())),
            )
            .push(
                widget::button::custom(
                    widget::container(we_ui::label("取消", 12.0, we_ui::ink_text()))
                        .width(Length::Fill)
                        .align_x(Horizontal::Center),
                )
                .class(we_ui::ghost_class())
                .padding([5.0, 8.0])
                .width(Length::FillPortion(1))
                .on_press_down(Message::Deselect),
            )
            .spacing(spacing.space_xs);

        let mut sc = widget::scrollable(col)
            .id(panel_scroll_id())
            .height(Length::Fill);
        if self.panel_bottom {
            // 截图用：从底部开（见 panel_bottom 字段的注释）
            sc = sc.anchor_y(cosmic::iced::widget::scrollable::Anchor::End);
        }
        let body_inner: Element<'_, Message> = sc.into();

        widget::container(
            widget::column::with_capacity(3)
                .push(body_inner)
                .push(we_ui::divider())
                .push(bottom)
                .spacing(spacing.space_xs)
                .height(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(spacing.space_s)
        .style(we_ui::panel_style)
        .into()
    }

    /// 这条壁纸抽到几张宣传图引用（0 张 = `None`）。切换开关是否要出提示行就看它。
    fn promo_count(&self, id: &str) -> Option<usize> {
        self.promo.get(id).map(Vec::len).filter(|n| *n > 0)
    }

    /// 「宣传图」段当前**关**、但这条有图时的提示行：一句说明 + 一个「显示」开关。
    /// （默认是开的，所以这一行只在用户**主动关掉**后出现。）
    /// 这样开关在**面板上**也够得着（设置页里也有一个，两处同一条 `Message::SetPromo`）。
    fn promo_off_hint(&self, n: usize) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        widget::column::with_capacity(2)
            .push(we_ui::section("宣传图"))
            .push(
                widget::row::with_capacity(2)
                    .push(we_ui::label(
                        format!("已关闭（此条有 {n} 张作者宣传图，不联网）"),
                        11.0,
                        we_ui::ink_dim(),
                    ))
                    .push(we_ui::icon_button(
                        "显示",
                        "image-x-generic-symbolic",
                        we_ui::gray_class(),
                        Message::SetPromo(true),
                    ))
                    .spacing(6.0)
                    .align_y(Alignment::Center),
            )
            .spacing(spacing.space_s)
            .into()
    }

    /// **作者嵌入的宣传图**那一段（0 张 = `None`，整段不出）。
    /// 遍历「该壁纸的图片引用清单」（`promo` 模块给的），**逐张**显示：
    /// 已经上屏的直接画；还在后台下的画占位（下载完成会来一条 `Message::PromoImage` 换成真图）。
    /// 一行的宽度是面板内容宽，图按自身比例缩进去（不超过 300 高）。
    fn view_promo<'a>(&'a self, id: &str, content_w: f32) -> Option<Element<'a, Message>> {
        let list = self.promo.get(id)?;
        if list.is_empty() {
            return None;
        }
        let spacing = cosmic::theme::spacing();
        let loading = self.promo_fetching.as_deref() == Some(id);

        let mut items = widget::column::with_capacity(list.len() * 2 + 2)
            .spacing(spacing.space_xs)
            .width(Length::Fill);
        for (i, p) in list.iter().enumerate() {
            let el: Element<_> = match self.promo_img.get(id).and_then(|m| m.get(&i)) {
                Some(img) => {
                    let (w, h) = fit_into(img.w as f32, img.h as f32, content_w, 300.0);
                    widget::container(
                        widget::image::Image::new(img.handle.clone())
                            .content_fit(cosmic::iced::ContentFit::Contain)
                            .width(Length::Fixed(w))
                            .height(Length::Fixed(h)),
                    )
                    .width(Length::Fill)
                    .align_x(Horizontal::Center)
                    .into()
                }
                None => widget::container(we_ui::label(
                    if loading {
                        "宣传图加载中…"
                    } else {
                        "宣传图（未缓存，联网后自动补）"
                    },
                    11.0,
                    we_ui::ink_dim(),
                ))
                .width(Length::Fill)
                .height(Length::Fixed(48.0))
                .align_x(Horizontal::Center)
                .align_y(Vertical::Center)
                .style(we_ui::chip_style)
                .into(),
            };
            items = items.push(el);
            // 外链标注来源，方便核对（本地图就是目录里的文件）
            if p.is_remote() {
                items = items.push(we_ui::label(
                    short_name(&p.src, 64),
                    9.5,
                    we_ui::ink_dim(),
                ));
            }
        }

        // 这一段现在挂在「属性」那个 rows 列里（见 `view_detail`），父容器已经留出
        // `SCROLL_PAD`（滚动条叠画在内容上）⇒ 这里**不再重复留右内边距**，免得图偏窄。
        let el: Element<'a, Message> = widget::column::with_capacity(2)
            .push(we_ui::section("宣传图"))
            .push(widget::container(items).width(Length::Fill))
            .spacing(spacing.space_s)
            .into();
        Some(el)
    }


    /// **作者自定义**属性的一行：左「标签」，右**按 type** 出的控件 —— **不带图标**
    /// （WE 的引擎项才带；作者项只有标签 + 控件，标签落在图标列左缘）。
    fn prop_row<'a>(&'a self, id: &str, p: &Prop) -> Element<'a, Message> {
        let value = self.prop_value(id, p);

        let control: Element<'a, Message> = match p.kind {
            PropKind::Bool => {
                let on = value.as_bool().unwrap_or(false);
                we_ui::checkbox(
                    on,
                    Message::PropBool(id.to_string(), p.key.clone(), !on),
                )
            }
            PropKind::Slider => {
                let v = value.as_f64().unwrap_or(p.min);
                let id2 = id.to_string();
                let key2 = p.key.clone();
                let track = widget::slider(p.min..=p.max, v, move |nv: f64| {
                    Message::PropNum(id2.clone(), key2.clone(), nv)
                })
                .step(if p.step > 0.0 { p.step } else { 1.0 })
                .class(we_ui::slider_class());
                we_ui::slider_row(track.into(), fmt_num(v))
            }
            PropKind::Color => {
                let (r, g, b) = rgb_of(&value);
                let key2 = p.key.clone();
                we_ui::color_field(r, g, b, Message::ToggleColorPicker(key2))
            }
            PropKind::Combo => {
                // 灰底下拉：当前值 + ▾（pick_list 自带箭头），宽度铺满控件列
                let labels: Vec<String> = p.options.iter().map(|(l, _)| l.clone()).collect();
                let cur = value_to_string(&value);
                let cur_label = p
                    .options
                    .iter()
                    .find(|(_, v)| v == &cur)
                    .map(|(l, _)| l.clone())
                    .unwrap_or(cur);
                let opts = p.options.clone();
                let id2 = id.to_string();
                let key2 = p.key.clone();
                let pl = cosmic::iced::widget::pick_list(labels, Some(cur_label), move |lbl: String| {
                    let v = opts
                        .iter()
                        .find(|(l, _)| l == &lbl)
                        .map(|(_, v)| v.clone())
                        .unwrap_or(lbl);
                    Message::PropCombo(id2.clone(), key2.clone(), v)
                })
                .text_size(12.0)
                .padding([4.0, 8.0])
                .width(Length::Fill);
                we_ui::dropdown(pl.into())
            }
            PropKind::Text => {
                let cur = self
                    .prop_text
                    .get(&text_key(id, &p.key))
                    .cloned()
                    .unwrap_or_else(|| value_to_string(&value));
                let id2 = id.to_string();
                let key2 = p.key.clone();
                widget::text_input("…", cur)
                    .on_input(move |s: String| Message::PropText(id2.clone(), key2.clone(), s))
                    .width(Length::Fill)
                    .into()
            }
        };

        // 无图标：作者项
        we_ui::prop_row(None, p.label.clone(), control)
    }

    /// **引擎内置项**·对齐方式：灰底下拉（走 `FitModeSet`，和设置页「取景」同一批值、同一条路）。
    fn engine_row_fit<'a>(&self) -> Element<'a, Message> {
        let cur = if self.config.fit_mode.is_empty() {
            "cover".to_string()
        } else {
            self.config.fit_mode.clone()
        };
        let labels: Vec<String> = FIT_MODES.iter().map(|(_, l)| (*l).to_string()).collect();
        let cur_label = FIT_MODES
            .iter()
            .find(|(id, _)| *id == cur)
            .map(|(_, l)| (*l).to_string())
            .unwrap_or_else(|| cur.clone());
        let pl = cosmic::iced::widget::pick_list(labels, Some(cur_label), |lbl: String| {
            let id = FIT_MODES
                .iter()
                .find(|(_, l)| *l == lbl)
                .map(|(id, _)| *id)
                .unwrap_or("cover");
            Message::FitModeSet(id.to_string())
        })
        .text_size(12.0)
        .padding([4.0, 8.0])
        .width(Length::Fill);
        we_ui::prop_row(
            Some("preferences-desktop-wallpaper-symbolic"),
            "对齐方式",
            we_ui::dropdown(pl.into()),
        )
    }

    /// **引擎内置项**·播放速度：滑杆 0.25×~3.0×（默认 1.0×，步进 0.05）。
    /// 内核侧是**新能力**：倍率只乘场景时间（视频壁纸的 `rate` 是另一条路，不在这里）。
    /// 拖动时每帧发一次 `TimeScaleSet`，出站队列按 key 合并 + 55 ms 节流（≈18 Hz）；
    /// `.on_release(SliderRelease)` 保证**松手必发终值**。
    fn engine_row_speed<'a>(&self) -> Element<'a, Message> {
        let v = self.time_scale.clamp(0.25, 3.0) as f64;
        let track = widget::slider(0.25..=3.0, v, |nv: f64| Message::TimeScaleSet(nv as f32))
            .step(0.05)
            .on_release(Message::SliderRelease)
            .class(we_ui::slider_class());
        we_ui::prop_row(
            Some("speedometer-symbolic"),
            "播放速度",
            we_ui::slider_row(track.into(), format!("{v:.2}×")),
        )
    }

    /// **引擎内置项**·音乐音量：滑杆 0~100%（默认 = 启动时读到的 sink 实际值）。
    /// 内核调的是**默认 sink 的 volume**（壁纸自己的 sound 就播在它上面），回读实际值；
    /// 界面这里只保留最后一次的值，拖动中的显示不等回执。
    fn engine_row_volume<'a>(&self) -> Element<'a, Message> {
        let v = self.music_vol.unwrap_or(0.0).clamp(0.0, 100.0) as f64;
        let track = widget::slider(0.0..=100.0, v, |nv: f64| Message::MusicVolumeSet(nv as f32))
            .step(1.0)
            .on_release(Message::SliderRelease)
            .class(we_ui::slider_class());
        let text = match self.music_vol {
            Some(v) => format!("{v:.0}%"),
            None => "未知".to_string(),
        };
        we_ui::prop_row(
            Some("audio-volume-high-symbolic"),
            "音乐音量",
            we_ui::slider_row(track.into(), text),
        )
    }

    /// **引擎内置项**·画面色彩第 `i` 项：滑块 + 右侧数值列（值域/步长和原来的 ± 按钮完全一致）。
    /// 例外：**色相**（i=3）—— 用户 要求"所有选色的内容改成圆形的色盘"，
    /// 色相是这六项里唯一"选色"的那一项 ⇒ 换成圆形色相盘（[`color_wheel::hue_wheel`]）。
    /// 输出仍是**弧度**、仍走 `Message::ColorSet(3, v)`（与滑条同一条落点，
    /// `color_bounds(3) = (-3.14, 3.14)` 不变）；其余五项是数值调整（亮度/对比/饱和/色温/伽马），
    /// 不属于"选色"，继续用滑条 —— 换成圆盘会让它们失去可调性（那是功能回归）。
    fn engine_row_color<'a>(&'a self, i: usize) -> Element<'a, Message> {
        if i == 3 {
            return self.engine_row_hue();
        }
        // 读**这条壁纸自己的**色彩（分槽；没选中才退回全局）—— 见 `current_color`。
        let color = self.current_color();
        let (lo, hi) = color_bounds(i);
        let v = color[i].clamp(lo, hi) as f64;
        let step = match i {
            2 | 4 => 0.01,
            _ => 0.05,
        };
        // 滑块按「弧度/倍率」的原始值滑动，但**数值列**仍按原来的可读写法显示
        let track = widget::slider((lo as f64)..=(hi as f64), v, move |nv: f64| {
            Message::ColorSet(i, nv as f32)
        })
        .step(step)
        .class(we_ui::slider_class());
        we_ui::prop_row(
            Some(engine_color_icon(i)),
            color_label(i),
            we_ui::slider_row(track.into(), color_value_text(i, color[i])),
        )
    }

    /// **引擎内置项**·色相：圆形色相盘（外圈 = 色相环，中间 = 当前色相圆点预览）
    /// + 右侧原来的度数读数。拖动发 `Message::ColorSet(3, 弧度)` —— 与旧滑条**同一个**落点，
    /// 所以配置串（`color_to_config`，6 个逗号分隔浮点）与下发链路一个字节都没变。
    fn engine_row_hue<'a>(&self) -> Element<'a, Message> {
        let color = self.current_color();
        let rad = color[3].clamp(-color_wheel::HUE_MAX, color_wheel::HUE_MAX);
        let disc = color_wheel::hue_wheel(64.0, rad, |a: f32| Message::ColorSet(3, a));
        we_ui::prop_row_h(
            Some(engine_color_icon(3)),
            color_label(3),
            widget::row::with_capacity(2)
                .push(disc)
                .push(we_ui::label(color_value_text(3, rad), 12.0, we_ui::ink_dim()))
                .spacing(6)
                .align_y(Alignment::Center)
                .into(),
            78.0,
        )
    }

    /// **引擎内置项**最后一行：色彩还原 / 重新下发 —— 原来的功能一个不落，只换了摆放。
    fn engine_row_actions<'a>(&self) -> Element<'a, Message> {
        we_ui::prop_row(
            Some("emblem-system-symbolic"),
            "画面色彩",
            widget::row::with_capacity(2)
                .push(we_ui::icon_button(
                    "色彩还原",
                    "edit-clear-symbolic",
                    we_ui::gray_class(),
                    Message::ColorReset,
                ))
                .push(we_ui::icon_button(
                    "重新下发",
                    "view-refresh-symbolic",
                    we_ui::gray_class(),
                    Message::PerfPush,
                ))
                .spacing(4)
                .align_y(Alignment::Center)
                .into(),
        )
    }

    /// 「您的预设」那一排：保存 / 加载 / 应用到所有壁纸 / 分享 JSON / 重置
    /// （参考图 7-4 底部：两个半宽 + 三个通栏，最后一个是红底通栏）。
///
    /// * **保存 / 加载**：本程序一条壁纸一个槽位（`presets.json`）—— 不做命名槽位、
    /// 也没有文件对话框，这一点和 WE 不同（如实标出来）。
///
    /// * **应用到所有壁纸**：凡是「也有这个属性名、且类型对得上」的壁纸都会写上。
///
    /// * **分享 JSON**：导出当前改动（复制到剪贴板）；**重置**：丢掉改动回到默认值并下发。
    fn view_presets<'a>(&'a self, id: &str, props: &[Prop]) -> Element<'a, Message> {
        let n = self.prop_edits.get(id).map(|m| m.len()).unwrap_or(0);
        let mut col = widget::column::with_capacity(6)
            .spacing(5)
            .width(Length::Fill);

        col = col.push(
            widget::row::with_capacity(2)
                .push(
                    widget::container(we_ui::wide_button(
                        "加载",
                        "document-open-symbolic",
                        we_ui::gray_class(),
                        Message::PresetLoad,
                    ))
                    .width(Length::FillPortion(1)),
                )
                .push(
                    widget::container(we_ui::wide_button(
                        "保存",
                        "document-save-symbolic",
                        we_ui::gray_class(),
                        Message::PresetSave,
                    ))
                    .width(Length::FillPortion(1)),
                )
                .spacing(5)
                .width(Length::Fill),
        );
        col = col.push(we_ui::wide_button(
            "应用到所有壁纸",
            "edit-copy-symbolic",
            we_ui::gray_class(),
            Message::PropsApplyAll,
        ));
        col = col.push(we_ui::wide_button(
            "分享 JSON",
            "document-share-symbolic",
            we_ui::gray_class(),
            Message::PropsShareJson,
        ));
        col = col.push(we_ui::wide_button(
            if n > 0 {
                format!("重置（当前 {n} 项改动）")
            } else {
                "重置".to_string()
            },
            "edit-undo-symbolic",
            we_ui::red_class(),
            Message::PropsReset,
        ));
        let _ = props;
        col.into()
    }

    /// 颜色属性点开后展开的**圆形色盘**（hue 环 + 内部 S/V + 当前色预览 + 数值 + 圆形预设点）。
    /// 统一走圆形色盘：原来那排 12 个**矩形**预设色块换成圆盘 + 圆形预设点。
    /// 落点、消息、值格式一个都没变：
    /// 拖动和点预设走的都是 `Message::PropColor(id, key, "r g b")`（→ `set_prop` → `set-prop`）。
    fn color_wheel_panel<'a>(&'a self, id: &str, p: &Prop) -> Element<'a, Message> {
        let value = self.prop_value(id, p);
        let (r, g, b) = rgb_of(&value);
        let rgb_text = color_wheel::format_prop_color(r, g, b);
        let hex = color_wheel::hex_string(r, g, b);

        // 圆盘：回调直接给"已经格式化好的协议串"（格式只在 `color_wheel` 里定义一次）
        let id2 = id.to_string();
        let key2 = p.key.clone();
        let wheel = color_wheel::rgb_wheel(148.0, r, g, b, move |s: String| {
            Message::PropColor(id2.clone(), key2.clone(), s)
        });

        // 12 个预设：两行、每行 6 个**圆形**色点（点击 = 与拖动完全相同的一条路）
        let mut dots = widget::column::with_capacity(2).spacing(4);
        for chunk in COLOR_PRESETS.chunks(6) {
            let mut row = widget::row::with_capacity(6).spacing(5);
            for preset in chunk {
                let (pr, pg, pb) = parse_rgb_str(preset);
                row = row.push(we_ui::dot_button(
                    18.0,
                    pr,
                    pg,
                    pb,
                    Message::PropColor(id.to_string(), p.key.clone(), (*preset).to_string()),
                ));
            }
            dots = dots.push(row);
        }

        // 读数：左 = 当前色圆点；右 = **属性值的原样协议串**（"r g b"）+ hex + 操作提示
        let readout = widget::column::with_capacity(2)
            .push(we_ui::label(rgb_text, 12.0, we_ui::ink_text()))
            .push(we_ui::label(
                format!("{hex} · 拖圆环改色相 / 拖方块改饱和·明度"),
                10.0,
                we_ui::ink_dim(),
            ))
            .spacing(2);
        let head = widget::row::with_capacity(2)
            .push(we_ui::color_dot(30.0, r, g, b))
            .push(readout)
            .spacing(8)
            .align_y(Alignment::Center);

        // 「还原默认」：回到 project.json 里这条属性的默认色（同一条 PropColor 路）
        let (dr, dg, db) = rgb_of(&p.value);
        let restore = we_ui::icon_button(
            "还原默认",
            "edit-undo-symbolic",
            we_ui::gray_class(),
            Message::PropColor(
                id.to_string(),
                p.key.clone(),
                color_wheel::format_prop_color(dr, dg, db),
            ),
        );

        let right = widget::column::with_capacity(4)
            .push(head)
            .push(dots)
            .push(restore)
            .spacing(8)
            .width(Length::Fill);

        widget::container(
            widget::row::with_capacity(2)
                .push(wheel)
                .push(right)
                .spacing(12)
                .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .padding(8.0)
        .style(we_ui::field_style)
        .into()
    }

    /// 底部状态条：选中壁纸的分辨率/大小 + 内核资源占用（+ 真出错了才多一句）。
    fn view_status_bar(&self) -> Element<'_, Message> {
        let mut bits: Vec<String> = Vec::new();
        match self.selected_entry() {
            Some(e) => {
                let mut s = format!("已选中：{} · {}", e.name, type_badge(&e.wp_type));
                if let Some((w, h)) = self.dims.get(&e.id) {
                    s.push_str(&format!(" · 预览 {w} x {h}"));
                }
                s.push_str(&format!(" · {:.1} KB", e.size as f64 / 1024.0));
                bits.push(s);
            }
            None => bits.push(format!(
                "{} 张壁纸 · 点缩略图选中，右侧看属性与画面",
                self.wallpapers.len()
            )),
        }
        // 角落只放**资源占用** + 真出了错才多一句。动作提示（"已生效 …"）不往这儿塞：
        // 它不会自己消失，会把角落长期占满。
        match self.native.as_ref() {
            Some(n) => {
                match self.cpu_pct {
                    Some(p) => bits.push(format!("CPU {p:.1}%")),
                    None => bits.push(format!("CPU {:.1}s", n.cpu_s)),
                }
                bits.push(format!("内存 {:.0} MB", n.rss_mb));
                bits.push(format!("{} 帧", n.frames));
                // 把"两边看到的当前场景 id"都摆出来，便于一眼核对
                // 内核在播的（权威） vs 界面记着的"上次应用"。不一致时一眼能看出。
                let kernel_id = self.playing_id().unwrap_or_else(|| {
                    if kernel_has_content(Some(n)) {
                        "（内核在播，不在本地列表）".to_string()
                    } else {
                        "—".to_string()
                    }
                });
                let app_id = self
                    .native_last
                    .as_ref()
                    .and_then(|(_, p)| self.wallpaper_id_for_path(p))
                    .unwrap_or_else(|| "—".to_string());
                bits.push(format!("场景 内核={kernel_id} · 界面上次应用={app_id}"));
            }
            None => bits.push("内核未连接".into()),
        }
        if let Some(err) = &self.error {
            bits.push(format!("⚠ {err}"));
        }
        widget::container(we_ui::label(bits.join("   ·   "), 11.0, we_ui::ink_dim()))
            .width(Length::Fill)
            .padding([2.0, 4.0])
            .into()
    }
}

/// 连上内核时要不要把「上次应用」那条推给内核 —— **纯函数**，单测直接钉（见 `auto_apply_tests`）。
/// 设计要点：
/// · 内核里**已经在播**的那条是**权威** ⇒ 启动同步绝不把它换掉；
/// · 只有内核**空闲**（没内容 / 没在播）时，才允许推进"上次应用"（否则会出现"重启内核后
/// 桌面被换成 app 记得的那条"这种不可接受的副作用）；
/// · 开关关掉 ⇒ **一个字也不推**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoApply {
    /// 开关关了（`--no-auto-apply` / 配置 `auto_apply_last=false`）⇒ 不推
    Disabled,
    /// 内核正在播某条 ⇒ 内核是权威，**不换掉它**
    KernelBusy,
    /// 内核空闲，但手上没有"上次应用" ⇒ 没东西可推
    NothingRemembered,
    /// 内核空闲 + 手上有"上次应用" ⇒ 允许推进（唯一会真的下发 `apply` 的情形）
    PushLast,
}

/// 连上内核时"上次应用"推不推：`enabled` = 总开关，`kernel_busy` = 内核有没有在播，
/// `has_last` = 界面上有没有记着"上次应用"。判定顺序：开关 → 内核权威 → 有无可推。
pub fn decide_auto_apply(enabled: bool, kernel_busy: bool, has_last: bool) -> AutoApply {
    if !enabled {
        return AutoApply::Disabled;
    }
    if kernel_busy {
        return AutoApply::KernelBusy;
    }
    if !has_last {
        return AutoApply::NothingRemembered;
    }
    AutoApply::PushLast
}

/// 内核**有没有内容**（正在播某条壁纸）。
/// 判据只看 `kind` + `path`：内核**没内容**时自己报 `kind:"none"` 且 `path` 为空。
/// **刻意不看 `playing`** —— 它会被"全屏窗口遮挡（`suppressed`）"和"用户暂停（`user_paused`）"
/// 置成 false，而那两种情况下**内容还在**（用户暂停的正是那条壁纸）⇒ 那时也**不算**空闲、
/// 不允许被"上次应用"换掉。
pub fn kernel_has_content(n: Option<&native::Status>) -> bool {
    n.is_some_and(|s| s.kind != "none" && !s.path.is_empty())
}

/// 画面色彩五项的值怎么显示：色相是**弧度**，显示成度数才看得懂。
fn color_value_text(i: usize, v: f32) -> String {
    if i == 3 {
        format!("{:+.0}°", v.to_degrees())
    } else {
        format!("{v:.2}")
    }
}

/// 发现页阶段二：后台并发下这一页的缩略图，**每下到一张就发一条 `Message::DiscoverThumb`**。
/// 为什么用流（`Task::stream`）而不是一次性 `Task::perform`：一次性的 `load_thumbs`
/// 要等这一页全部下完才返回，界面那一下只能整批刷新（30 张全好之前一张都看不到）；
/// 这里把**每张图一进后台就立刻**送回 UI ⇒ 界面是"一张一张补出来"的。
/// 下载全在后台线程里跑（`load_thumbs_streaming`），UI 只收消息 ⇒ 全程可交互、滚动不卡。
/// 全部结束后再发一条 `Message::DiscoverThumbsDone`（带统计）。
/// `want = items.len` ⇒ 这一页**全部**都要（以前写死 24，第 25 张起永远没图）。
fn discover_thumb_stream(
    items: Vec<workshop::Item>,
) -> impl cosmic::iced::futures::Stream<Item = Message> {
    use cosmic::iced::futures::SinkExt;
    cosmic::iced::stream::channel(
        64,
        move |mut out: cosmic::iced::futures::channel::mpsc::Sender<Message>| async move {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, Vec<u8>)>();
            let shared = std::sync::Arc::new(std::sync::Mutex::new(None::<workshop::Stats>));
            let want = items.len();
            let writer = shared.clone();
            let spawned = std::thread::Builder::new()
                .name("workshop-thumbs".into())
                .spawn(move || {
                    // 回调是在下载工作线程里调的 ⇒ 只做一次无界 send（非阻塞、不碰 UI）
                    let cb = move |id: String, bytes: Vec<u8>| {
                        let _ = tx.send((id, bytes));
                    };
                    let stats = workshop::load_thumbs_streaming(&cb, &items, want);
                    if let Ok(mut slot) = writer.lock() {
                        *slot = Some(stats);
                    }
                    // tx 随 cb 一起 drop ⇒ rx 关闭 ⇒ 上面的 while 自然结束
                });
            if spawned.is_err() {
                let _ = out.send(Message::DiscoverThumbsDone(Box::new(None))).await;
                return;
            }
            while let Some((id, bytes)) = rx.recv().await {
                if out.send(Message::DiscoverThumb(id, bytes)).await.is_err() {
                    return;
                }
            }
            let stats = shared.lock().ok().and_then(|mut slot| slot.take());
            let _ = out.send(Message::DiscoverThumbsDone(Box::new(stats))).await;
        },
    )
}

/// 宣传图的后台下载流：按顺序把还没缓存的远程图下回来，**每下到一张就发一条**
/// `Message::PromoImage`，最后一条 `Message::PromoDone`。
/// 与发现页缩略图流同一套路：下载在线程里跑，UI 线程一字节都不碰。
/// 顺序下（实测最多 7 张、单张 ≤ 680 KB），一张失败只跳过这一张。
fn promo_stream(
    id: String,
    pending: Vec<(usize, String)>,
) -> impl cosmic::iced::futures::Stream<Item = Message> {
    use cosmic::iced::futures::SinkExt;
    cosmic::iced::stream::channel(
        16,
        move |mut out: cosmic::iced::futures::channel::mpsc::Sender<Message>| async move {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(usize, Vec<u8>)>();
            let spawned = std::thread::Builder::new()
                .name("wp-promo".into())
                .spawn(move || {
                    for (i, url) in pending {
                        if let Some(bytes) = promo::fetch_remote(&url) {
                            if tx.send((i, bytes)).is_err() {
                                break;
                            }
                        }
                    }
                    // tx 随本线程 drop ⇒ rx 关闭 ⇒ 下面的 while 自然结束
                });
            if spawned.is_err() {
                let _ = out.send(Message::PromoDone(id)).await;
                return;
            }
            while let Some((i, bytes)) = rx.recv().await {
                if out.send(Message::PromoImage(id.clone(), i, bytes)).await.is_err() {
                    return;
                }
            }
            let _ = out.send(Message::PromoDone(id)).await;
        },
    )
}

/// 16:9 缩略图的宽 / 高 —— 网格排布与 tile 共用一份（WE 的格子就是 16:9）。
/// 尺寸随右侧面板一起调过：面板从 380 加宽到 540 之后，格子必须相应收一点，
/// 否则默认窗口（实测 1280 逻辑宽）下**放不下 4 列**。
/// 168x95 = 1.77（≈16:9）；1280 宽窗口扣掉面板与留白后还能排 4 列（见 `tiles_per_row`）。
/// 卡片尺寸三档 `(宽, 高)`（16:9 附近；逻辑像素）。
/// 用户 第二轮：「三档**整体上移一级** —— 当前的大（280×158）当标准
/// 再出一个更大的」。于是 小 220×124 / **中 280×158（默认）** / 大 360×203。
/// 做成**三档而不是连续像素**是刻意的：缩略图是 CPU 预先降采样好的（见 [`thumb_from_bytes`]），
/// 三档 ⇒ 每条壁纸最多 3 份缓存，拖动滑杆不会把解码/缩放风暴打到 UI 线程上。
const CARD_SIZES: [(f32, f32); 3] = [(220.0, 124.0), (280.0, 158.0), (360.0, 203.0)];
/// 默认档 = 中（280×158）= 上次的「大」，用户认定的标准尺寸。
/// **硬编码而不是落盘**：重启后必须仍停在这一档（见 `AppModel::card_idx` 的初始化）。
const CARD_DEFAULT: usize = 1;
/// 格子之间的横向/纵向间距 —— 用户 「缩览图间距稍微大一点，现在太紧凑了」。
/// 这是**真间距**（行/列的 `spacing`），不是靠给卡片加内边距充数；24 ≈ 上次 8 的 3 倍，
/// 量出来相邻两张卡片的缝隙就是 24 px（见 `layout_tests::tile_gap_is_measured_gap`）。
const TILE_GAP: f32 = 24.0;
/// 右侧详情/属性面板的宽度（加宽到 540 —— 落在要求的 520~560 区间内）
const PANEL_W: f32 = 540.0;

/// 详情面板顶部大预览图的**显示上限**（宽 / 高，逻辑像素）。
/// 用户反馈：「把右侧的缩览图缩小一些吧，不要搞这么大。图像都缩放模糊了，观感不好」——
/// 数据源是工坊的 `preview.jpg/gif`，实测本机 17 条分辨率只有 180~1080 且多为**正方形**；
/// 以前不管源图多大都拉满面板宽（≈520）、还按 16:9 覆盖裁剪，小图被放大 2~3 倍 ⇒ 糊。
/// 现在：显示尺寸既不超过这个上限，也**绝不超过源图的原始像素**（见 `preview_display_size`）。
const PREVIEW_MAX_W: f32 = 320.0;
/// 高度上限：只对极端高图生效（正方/横图用不到），免得一张 100×1000 的图把面板撑爆
const PREVIEW_MAX_H: f32 = 400.0;

/// **工坊条目详情图**（发现/工坊两页的右侧详情）的宽度上限。
/// 用户 第二轮：「发现和工坊的详细页，缩览图给到最大 —— 这个页面空间比较空余，
/// 不要给这么小」。那一档原来写死 320 宽 × 200 高，在 540 宽的面板里左右各空 100 px、
/// 上面还压着一大片空白。现在：**宽度 = 面板内容区全宽**（540 - 2×内边距 = 520，上限 720
/// 只在面板真被放宽时才起作用），高按 16:9 算 ⇒ 1400 宽窗口下量出来 520×292。
/// 不违反"绝不超过源图像素"那条：工坊缩略图实测 514~1079 px 见方（`~/.cache/wallpaper-hub/
/// workshop/thumbs`），520 宽仍是**缩小**，且走的是同一套 Lanczos3 预缩放（上屏 1:1）。
const DETAIL_PREVIEW_MAX_W: f32 = 720.0;

/// 详情面板左右内边距合计（`view_workshop_detail` 里 `.padding(spacing.space_s)` 两侧）
const PANEL_PAD_X: f32 = 20.0;

/// 详情面板里那张大预览的显示尺寸 `(宽, 高)` —— 纯函数，好单测。
/// 宽度 = 面板内容区宽度（夹在 160..[`DETAIL_PREVIEW_MAX_W`] 之间），高 = 宽 × 9/16
/// （工坊缩略图都是方图，显示时按 16:9 居中裁剪；见 `thumb_from_bytes` 的 Cover 语义）。
fn detail_preview_size(win_w: f32, card_w: f32) -> (f32, f32) {
    let w = (panel_width(win_w, card_w) - PANEL_PAD_X).clamp(160.0, DETAIL_PREVIEW_MAX_W);
    let w = w.floor();
    (w, (w * 9.0 / 16.0).round())
}

/// 面板之外还要扣掉的横向留白：窗口左右内边距 8×2 + 「左内容 ↔ 面板」之间的 8。
/// 与 `view` 里的 `padding(spacing.space_s)` + `spacing(space_s)` 对应。
const CHROME_X: f32 = 24.0;

/// 属性面板那个滚动区的 id（`--panel-scroll` 要按它把面板滚到指定位置）
fn panel_scroll_id() -> cosmic::widget::Id {
    cosmic::widget::Id::new("we-prop-panel")
}

/// 已安装页的排序方式（顺序就是排序下拉里的顺序）
const SORTS: [&str; 3] = ["名称", "类型", "大小"];

/// 排序下拉的标签 → 下标（认不出来就回到「名称」）
fn sort_index_of(label: &str) -> usize {
    SORTS.iter().position(|s| *s == label).unwrap_or(0)
}

/// 档位名 → 标签（`&'static str` 版本：按钮要把它借到函数外，String 借不出去）
fn preset_label_static(name: &str) -> &'static str {
    match name {
        "powersave" => "省电",
        "balanced" => "平衡",
        "quality" => "画质",
        _ => "自定义",
    }
}

/// 取色板的 12 个预设色（`"r g b"` 浮点串，和 WE 的 color 属性同一种写法）。
const COLOR_PRESETS: [&str; 12] = [
    "1 1 1",
    "0 0 0",
    "1 0 0",
    "1 0.5 0",
    "1 1 0",
    "0.4 0.8 0.2",
    "0 1 0",
    "0 1 1",
    "0 0.4 1",
    "0.2 0.2 0.9",
    "0.6 0.2 0.9",
    "1 0 1",
];

/// 右侧面板宽度：目标 `PANEL_W`（540，落在要求的 520~560 区间）。
/// 窗口很窄时**退让**（否则中间网格会被压成一列甚至压没）：保证左边至少还能放两列格子，
/// 但不会小于 320（再小属性行的控件列就摆不下了）。900 宽（本程序的最小宽度）时 =
/// 900 - 24 - (2×168+8) = 532，仍在 520~560 内。
fn panel_width(win_w: f32, card_w: f32) -> f32 {
    let left_min = card_w * 2.0 + TILE_GAP;
    (win_w - CHROME_X - left_min).clamp(320.0, PANEL_W)
}

/// 缩略图**每行放几个**：按窗口逻辑宽度算（右侧面板 + 左右留白先扣掉）。
/// 抽成纯函数是为了能单测 —— "窗口变窄 ⇒ 每行少放几个"必须可验证，别靠截图猜。
fn tiles_per_row(win_w: f32, card_w: f32) -> usize {
    let avail = (win_w - panel_width(win_w, card_w) - CHROME_X).max(card_w);
    (((avail + TILE_GAP) / (card_w + TILE_GAP)).floor() as usize).clamp(1, 12)
}

/// 缩略图预缩放的缓存：`(来源键, 宽, 高) → 已缩好的 RGBA 句柄`。
/// 为什么要自己缩：iced 的 wgpu 图像 atlas 只有 `mip_level_count: 1`（无 mipmap），
/// 把 1920×1080 直接画进 220×124 会掉采样 ⇒ 糊 + 锯齿。这里用 Lanczos3 在 CPU 上缩到
/// **卡片正好那么大**，上屏就是 1:1 像素。三档卡片尺寸 ⇒ 最多 3 份缓存。
/// 上限 192 条，超了整体清空（本仓库十几~三十条壁纸/一页工坊，够用且不会无限涨）。
fn thumb_cache() -> &'static std::sync::Mutex<HashMap<(String, u32, u32), widget::image::Handle>> {
    static C: std::sync::OnceLock<
        std::sync::Mutex<HashMap<(String, u32, u32), widget::image::Handle>>,
    > = std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn thumb_cache_get(key: &str, w: u32, h: u32) -> Option<widget::image::Handle> {
    thumb_cache()
        .lock()
        .ok()?
        .get(&(key.to_string(), w, h))
        .cloned()
}

fn thumb_cache_put(key: &str, w: u32, h: u32, handle: widget::image::Handle) {
    if let Ok(mut m) = thumb_cache().lock() {
        if m.len() > 192 {
            m.clear();
        }
        m.insert((key.to_string(), w, h), handle);
    }
}

/// 从**文件路径**取一张按卡片尺寸缩好的缩略图（读不出文件 ⇒ 退回 `Handle::from_path`，
/// 由 iced 自己解码；至少不会空一块）。
fn thumb_from_path(path: &str, w: u32, h: u32) -> widget::image::Handle {
    let key = format!("p:{path}");
    if let Some(hd) = thumb_cache_get(&key, w, h) {
        return hd;
    }
    match std::fs::read(path) {
        Ok(bytes) => thumb_from_bytes(&key, &bytes, w, h),
        Err(_) => widget::image::Handle::from_path(path),
    }
}

/// 从**编码字节**取一张按 `w×h` 缩好的缩略图。
/// 语义 = `ContentFit::Cover`：先按目标比例**居中裁剪**，再 Lanczos3 缩到 `w×h`。
/// 原图比卡片还小 ⇒ 不放大（保持原像素，交给 GPU 做一次线性放大 —— 放大糊是源图不够，
/// 不是缩放算法的问题）。
fn thumb_from_bytes(key: &str, bytes: &[u8], w: u32, h: u32) -> widget::image::Handle {
    if w == 0 || h == 0 {
        return widget::image::Handle::from_bytes(bytes.to_vec());
    }
    if let Some(hd) = thumb_cache_get(key, w, h) {
        return hd;
    }
    let Some(img) = image::load_from_memory(bytes).ok() else {
        // 解不了（webp/坏文件…）⇒ 退回原字节，让 iced 自己试
        return widget::image::Handle::from_bytes(bytes.to_vec());
    };
    let (sw, sh) = (img.width().max(1), img.height().max(1));
    // 居中裁剪到目标比例（与 Cover 一致）
    let target = w as f32 / h as f32;
    let src = sw as f32 / sh as f32;
    let (cw, ch) = if src > target {
        (((sh as f32) * target).round().max(1.0) as u32, sh)
    } else {
        (sw, ((sw as f32) / target).round().max(1.0) as u32)
    };
    let (cw, ch) = (cw.min(sw), ch.min(sh));
    let x = (sw - cw) / 2;
    let y = (sh - ch) / 2;
    let cropped = image::imageops::crop_imm(&img, x, y, cw, ch).to_image();
    let out = if cw <= w && ch <= h {
        cropped
    } else {
        image::imageops::resize(&cropped, w, h, image::imageops::FilterType::Lanczos3)
    };
    let handle = widget::image::Handle::from_rgba(out.width(), out.height(), out.into_raw());
    thumb_cache_put(key, w, h, handle.clone());
    handle
}

/// 创意工坊条目的网页地址（**只读浏览**用；没有任何账号/id 参数）。
fn workshop_url(id: &str) -> String {
    format!("https://steamcommunity.com/sharedfiles/filedetails/?id={id}")
}

/// **纯函数**：从 `pactl get-sink-volume` 的输出里取第一个百分比。
/// 形态（实测本机 PipeWire 1.6.9）：
/// `Volume: front-left: 26241 / 40% / -23.85 dB, front-right: …`
fn parse_pactl_volume(text: &str) -> Option<f32> {
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix("Volume:") else {
            continue;
        };
        let chars: Vec<char> = rest.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i].is_ascii_digit() {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                if i < chars.len() && chars[i] == '%' {
                    return chars[start..i].iter().collect::<String>().parse().ok();
                }
            } else {
                i += 1;
            }
        }
    }
    None
}

/// 读一次**系统默认 sink 的实际音量**（0..100）。读不到 ⇒ `None`（界面显示"未知"）。
/// 只读一次（启动时），默认值取当前 sink 的实际音量。之后的改动由内核回读，
/// 界面不自己轮询 `pactl`（不在渲染路径上起子进程）。
fn read_sink_volume() -> Option<f32> {
    let out = std::process::Command::new("pactl")
        .args(["get-sink-volume", "@DEFAULT_SINK@"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_pactl_volume(&String::from_utf8_lossy(&out.stdout))
}

/// 字节流里的图片尺寸 —— **只读文件头**（`into_dimensions`），不整张解码。
/// 界面每帧不用它（尺寸在插入时量好），只在宣传图上屏那一刻调一次。
fn dims_of_bytes(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// 把一张 `w × h` 的图按比例塞进 `max_w × max_h`。
fn fit_into(w: f32, h: f32, max_w: f32, max_h: f32) -> (f32, f32) {
    if w <= 0.0 || h <= 0.0 {
        return (max_w, max_w * 9.0 / 16.0);
    }
    let s = (max_w / w).min(max_h / h);
    (w * s, h * s)
}

/// 详情面板顶部大预览图的**显示尺寸**（纯函数，可单测）。
/// 规则（按用户反馈「不要搞这么大，图像都缩放模糊了」定）：
///
/// 1. 不超过面板内容宽 `avail_w`；
///
/// 2. 不超过 `PREVIEW_MAX_W`（水平上限）/ `PREVIEW_MAX_H`（只对极端高图生效）；
///
/// 3. **绝不放大超过源图实际像素**：显示宽 ≤ 源图宽、显示高 ≤ 源图高（按源图自身比例）。
/// 于是：源图是 180×180 的小 GIF ⇒ 就显示 180×180（放大倍率 1.0，不糊）；
/// 源图是 1080×1080 ⇒ 收到 320×320（缩小，清晰）；源图尺寸未知 ⇒ 上限内的 16:9 占位。
fn preview_display_size(src_w: u32, src_h: u32, avail_w: f32) -> (f32, f32) {
    let cap_w = avail_w.max(1.0).min(PREVIEW_MAX_W);
    if src_w == 0 || src_h == 0 {
        // 量不到源图尺寸：只保证不超上限、不超面板宽，给个 16:9
        return (cap_w, cap_w * 9.0 / 16.0);
    }
    let (sw, sh) = (src_w as f32, src_h as f32);
    // 不放大：宽最多 = 源图宽（等比 ⇒ 高也随之 ≤ 源图高）
    let w = cap_w.min(sw);
    // 极端高图兜底：再按高度上限收一次（仍然不会超过源图宽）
    let by_h = PREVIEW_MAX_H * sw / sh;
    let w = w.min(by_h);
    (w, w * sh / sw)
}

/// 在壁纸目录里找一张**像素比封面更多**的本地图（作者的高分辨率帧/作者图），
/// 返回 `(路径, (宽,高))`。找不到就 `None`（界面就退回用封面）。
/// 只扫**一层**（非递归）、只看常见光栅扩展名、最多看 64 个文件；判据是纯像素面积，
/// 不看文件名。本机实测 17 条工坊内容目录里**只有 `preview.*`**（其余都在 `scene.pkg` 里），
/// 所以这条基本走 None —— 有更大的图时才会真的换（以及相应的单测覆盖）。
fn find_larger_local_image(dir: &Path, baseline_area: Option<u32>) -> Option<(PathBuf, (u32, u32))> {
    const EXTS: [&str; 6] = ["png", "jpg", "jpeg", "webp", "gif", "bmp"];
    let baseline = baseline_area.unwrap_or(0);
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(u64, PathBuf, (u32, u32))> = None;
    for (n, e) in entries.flatten().enumerate() {
        if n >= 64 {
            break;
        }
        let p = e.path();
        let is_img = p
            .extension()
            .and_then(|x| x.to_str())
            .map(|x| EXTS.contains(&x.to_ascii_lowercase().as_str()))
            .unwrap_or(false);
        if !is_img {
            continue;
        }
        let Ok((w, h)) = image::image_dimensions(&p) else {
            continue;
        };
        let area = (w as u64) * (h as u64);
        // 必须**严格大于**封面，且比已经选中的更大，才换
        if area <= baseline as u64 || area == 0 {
            continue;
        }
        if best.as_ref().is_none_or(|(a, _, _)| area > *a) {
            best = Some((area, p, (w, h)));
        }
    }
    best.map(|(_, p, d)| (p, d))
}

// ── 逐壁纸属性（数据驱动）────────────────────────────────────────────────

/// 一条可调属性（完全来自 `project.json` 的 `general.properties`）。
/// `kind` 决定右边出什么控件 —— 按**每条自己的 `type`** 走，不认属性名。
#[derive(Debug, Clone)]
struct Prop {
    key: String,
    label: String,
    kind: PropKind,
    value: serde_json::Value,
    min: f64,
    max: f64,
    step: f64,
    /// combo 用：(显示名, 值串)
    options: Vec<(String, String)>,
    /// 原始 `text`（判定时用 `classify_prop` 看它；单测里拿它复核「图片型」那条真的能抽到图）
    #[allow(dead_code)]
    raw_text: String,
    /// 面板上的呈现形态（**由数据算出**，不按属性名/壁纸名特判）
    form: PropForm,
    /// 作者写的「依赖显示」条件（`condition` 字段；缺席 = 恒显示，见 `parse_condition`）
    cond: PropCond,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PropKind {
    Bool,
    Slider,
    Color,
    Combo,
    Text,
}

/// 作者属性在面板上的呈现形态 —— **只看「类型 + text 的内容 + value」**，不看属性名：
/// WE 的作者把 `general.properties` 当**排版区**用：`type:"text"` 里塞 `<img>`、`<h3>`、
/// 空串。以前这里一律渲染成「标签 + 可编辑输入框」，用户看到一堆没意义的键与输入框。
#[derive(Debug, Clone, PartialEq, Eq)]
enum PropForm {
    /// 真·可编辑控件（作者给了用户编辑的类型 + 干净的标签）
    Editable(PropKind),
    /// 作者自己的分组/横幅标题（`text` 是文字型排版块，如 `<h3>🎚️全局</h3>`）
    Heading(String),
    /// 有可读文字、但没有可编辑语义 ⇒ 只读一行
    ReadOnlyNote(String),
    /// `text` 里只有标记/图片 ⇒ 图片已由 `promo` 模块归入「宣传图」段，本行不出
    MediaOnly,
    /// `text` 空 / 只剩标记没文字 ⇒ 整行跳过
    Blank,
    /// `text` 是 `ui_*` 这类**本地化键** ⇒ 拿不到人话标签，跳过（别把键名当标签）。
    /// ⚠ 唯一的例外是**真·颜色控件**（`type:"color"` + 可解析的 `value`）：那种情况下
    /// [`PropForm::Editable(PropKind::Color)`] 会赢，本变体只剩"没有控件可用"的那种本地化键
    /// （例如键名挂在 `bool`/`text` 上）。
    LocalizedKey,
}

/// 解析一份 `general.properties`。
/// 规则只有一条：**foreach 每一条**，按它自己的 `type` 决定控件；名字只当显示文本
/// （`text` 字段），绝不参与判断；`text` 是不是「控件标签」由 `classify_prop` 看数据定
/// （排版块/图片/空串/本地化键都不出控件）。顺序按 `index` / `order`，
/// 两个字段都没有就保持文件里的顺序（serde_json 开了 `preserve_order`）。
fn parse_props(schema: &str) -> Vec<Prop> {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(schema) else {
        return Vec::new();
    };
    let mut out: Vec<(i64, Prop)> = Vec::with_capacity(map.len());
    for (idx, (key, v)) in map.iter().enumerate() {
        let Some(obj) = v.as_object() else { continue };
        let type_str = obj.get("type").and_then(|t| t.as_str()).unwrap_or("text");
        let kind = kind_of(type_str);
        let raw_text = obj
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let value = obj.get("value").cloned().unwrap_or(serde_json::Value::Null);
        let label = if raw_text.trim().is_empty() {
            key.clone()
        } else {
            pretty_label(&raw_text)
        };
        let order = obj
            .get("index")
            .or_else(|| obj.get("order"))
            .and_then(|n| n.as_i64())
            .unwrap_or(idx as i64);

        // 「依赖显示」条件（`condition` 字段）：缺席 / 空串 / 解析不了 ⇒ 恒显示（见 `parse_condition`）
        let cond = parse_condition(obj.get("condition").and_then(|c| c.as_str()).unwrap_or(""));

        let mut options = Vec::new();
        if let Some(list) = obj.get("options").and_then(|o| o.as_array()) {
            for o in list {
                match o {
                    serde_json::Value::Object(m) => {
                        let l = m.get("label").map(value_to_string).unwrap_or_default();
                        let val = m.get("value").map(value_to_string).unwrap_or_default();
                        options.push((if l.is_empty() { val.clone() } else { l }, val));
                    }
                    other => {
                        let s = value_to_string(other);
                        options.push((s.clone(), s));
                    }
                }
            }
        }

        out.push((
            order,
            Prop {
                key: key.clone(),
                label,
                kind,
                value: value.clone(),
                min: obj.get("min").and_then(|n| n.as_f64()).unwrap_or(0.0),
                max: obj.get("max").and_then(|n| n.as_f64()).unwrap_or(100.0),
                step: obj.get("step").and_then(|n| n.as_f64()).unwrap_or(1.0),
                options,
                form: classify_prop(type_str, &raw_text, &value),
                cond,
                raw_text,
            },
        ));
    }
    out.sort_by_key(|(order, _)| *order);
    out.into_iter().map(|(_, p)| p).collect()
}

/// 属性到底该不该出可编辑控件 —— **只看数据**（`type` + `text` 的形状 + `value`），
/// 不看属性名、不看壁纸名、不按 `key` 白名单。规则（每一条都有本机真实数据兜底）：
///
/// 1. `text` 为空（trim 后）⇒ `Blank` 跳过。
/// *实测* `2887537617` 的 `xian` / `xinzeng2,3,4,5,9`、`3704273480` 的 `kong1/2/4`：
/// `type:"text"` 而 `text:""` —— 作者拿它占位。
///
/// 2. `text` 是 `ui_*` 本地化键（如 `ui_browse_properties_scheme_color`）⇒ `LocalizedKey` 跳过
/// （没有翻译表就只剩键名，**别把键名当标签**）。
/// ⚠ **例外**：`type:"color"` 且 `value` 是**可解析颜色** ⇒ 留作可编辑**色盘**
/// （键名经 `pretty_label` 美化后当标签）。判据仍然只看数据：`type` + `value` 的形状
/// + `text` 的形态，**不看 `key`/属性名/壁纸名**。
/// 以前整行跳过 ⇒ 作者那排"配色/主题色"色盘在面板里**根本看不到**（本机
/// `2887537617` / `3704273480` / `2564205376` / `2823687015` 的 `schemecolor` 都是这种）。
/// 图片型/排版型 `text` **不享受**这个例外（它们走下面 3 / 4 两条，照旧只读/归宣传图）。
///
/// 3. `text` 里有图片引用、去掉标记后没有文字 ⇒ `MediaOnly`。
/// 这类图 `promo` 模块已经在「宣传图」段画出来了（遍历每条属性的 text），这里不再出行。
///
/// 4. `text` 是**排版块**（块级标记：`<h1..h6>` / `<div>` / `<p>` / `<center>` / `<br>` /
/// `[center]` / `[hr]` …，或带换行）⇒ 不是控件标签：
/// 去掉标记还有文字 ⇒ `Heading`（作者的**分组标题/横幅**，只读、加粗带线）；
/// 没有文字 ⇒ `MediaOnly` / `Blank`（按有没有图）。
/// 只有**行内**标记（`<b>` / `[b]` / `[i]` 这种包着标签文字的）才当标签看。
///
/// 5. 剩下的就是「干净标签 + 作者真给用户编辑的类型」：
/// `bool` / `slider` / `color` / `combo` / `textinput` ⇒ `Editable`（照旧出控件）；
/// `text` 类型要看 `value`：只有**用户真填过字符串**（非空字符串，不是 `false`/`null`）
/// 才当输入框（实测 `3704273480` 的 `key="text"` 就是 `type:"textinput"`、
/// `value:"mouse pad"`）；否则没有可编辑语义 ⇒ `ReadOnlyNote`（只读一行，不出输入框）。
fn classify_prop(type_str: &str, text: &str, value: &serde_json::Value) -> PropForm {
    let raw = text.trim();
    let plain = strip_markup(raw);
    let has_img = !promo::img_refs(raw).is_empty();
    let loc = is_loc_key(raw);
    let layout = is_layout_markup(raw);

    // 1) 空 text ⇒ 整行跳过
    if raw.is_empty() {
        return PropForm::Blank;
    }
    // 2) 本地化键 ⇒ 没有可读标签，跳过；**例外**：真·颜色控件（见上面第 2 条）
    if loc {
        if type_str == "color" && is_parseable_color(value) && !has_img && !layout {
            return PropForm::Editable(PropKind::Color);
        }
        return PropForm::LocalizedKey;
    }
    // 3) 只有图片 ⇒ 归「宣传图」段（promo 模块在画），本行不出
    if plain.is_empty() {
        return if has_img {
            PropForm::MediaOnly
        } else {
            PropForm::Blank
        };
    }
    // 4) 排版块：有文字就是作者的分组标题 / 横幅
    if layout {
        return PropForm::Heading(plain);
    }
    // 5) 干净标签 + 作者真给用户编辑的类型
    let editable = matches!(
        type_str,
        "bool" | "slider" | "color" | "combo" | "textinput"
    );
    if editable {
        return PropForm::Editable(kind_of(type_str));
    }
    // `text` 类型：只有 value 是用户真填的字符串才算输入框
    if type_str == "text" && user_text_value(value) {
        return PropForm::Editable(PropKind::Text);
    }
    PropForm::ReadOnlyNote(plain)
}

/// `type` 字符串 → 控件种类（真编辑类型才走这里）
fn kind_of(type_str: &str) -> PropKind {
    match type_str {
        "bool" => PropKind::Bool,
        "slider" => PropKind::Slider,
        "color" => PropKind::Color,
        "combo" => PropKind::Combo,
        _ => PropKind::Text,
    }
}

// ── 「依赖显示」条件（`project.json` 的 `condition` 字段）──────────────────
// 数据事实（本机 17 条工坊工程；属性字段全集只有
// `{order,text,type,value,fraction,max,min,precision,step,condition,index,options}`）：
// `condition` 只有三种形态 ——
// ① **缺席**（多数，如 `2887537617` 的 23 条全没有）⇒ 恒显示；
// ② **指向兄弟属性的表达式**，实测两例：`2823687015` 的 `_12hmode` = `clock.value == true`、
// `3704273480` 的 `text` / `text5` / `newproperty1` = `newproperty.value == 2`；
// 语义是「**依赖显示**」（条件成立这一行才该出现），不是互斥/单选；
// ③ 作者手填的任意串：`2564205376` 的 `"true"`、`2481886649` 的 `"Water Flow"`、
// `2482408967` 的空串。
// **[策略] 解析不了的一律「恒显示」**（fail-open）。依据三条：
// 1. 形态①（缺席）与空串在数据里就等于「没有条件」⇒ 解析不了不该比没写显示得更少；
// 2. 本程序**不实现任意表达式求值器**（那要一个 JS 解释器，超出范围）。能**确证**的只有
// `<属性key>.value <op> <单个字面量>` 这种**单操作数**形状；右操作数必须是一个不带空白的
// 单 token（引号包着的串除外），像 `a.value == 1 && b.value == 2` 这种复合式一律不猜；
// 3. 代价不对称：**错隐**会让用户**再也拿不到**作者给的控件（面板里无处可点），**错显**只是
// 多一行。⇒ 只有「能解析 + 引用得到那个兄弟属性 + 比较为假」才隐藏。
// **[策略] 条件不成立时仍然照常下发该属性**（只是这一行不画）。依据：`condition` 管的是
// **编辑器里这一行显不显示**，不是这个值生不生效；`prop_edits` / `prop_overrides.json` 与
// 内核 `set-prop` 都不看条件。若「不满足就不下发」，用户把下拉改回去时那一行会带着旧值重现、
// 而屏幕上是内核的默认值 —— 界面与屏幕就不一致了（本程序最忌讳这个）。
// ⇒ 门控**只发生在视图层**（`view_detail`），落盘与下发链路一行没动。

/// 一条属性的「依赖显示」条件（来自 `condition` 字段）
#[derive(Debug, Clone, PartialEq)]
enum PropCond {
    /// 恒显示：字段缺席 / 空串 / 解析不了（fail-open，见上面策略）
    Always,
    /// 字面量条件：`"true"` ⇒ 恒显示、`"false"` ⇒ 恒隐藏
    Const(bool),
    /// `<兄弟属性 key>.value <op> <字面量>`
    Cmp {
        key: String,
        op: CmpOp,
        rhs: serde_json::Value,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CmpOp {
    Eq,
    Ne,
}

/// 解析 `condition`。只认「单操作数」形状，其余一律 `Always`（见上面策略）。
fn parse_condition(raw: &str) -> PropCond {
    let s = raw.trim();
    if s.is_empty() {
        return PropCond::Always;
    }
    if s.eq_ignore_ascii_case("true") {
        return PropCond::Const(true);
    }
    if s.eq_ignore_ascii_case("false") {
        return PropCond::Const(false);
    }
    let Some((pos, len, neg)) = find_cmp_op(s) else {
        return PropCond::Always;
    };
    // 左边必须是 `<属性key>.value` —— 这就是「指向兄弟属性」的形状；别的写法不猜
    let Some(key) = s[..pos].trim().strip_suffix(".value").map(str::trim) else {
        return PropCond::Always;
    };
    if !is_prop_ident(key) {
        return PropCond::Always;
    }
    let Some(rhs) = parse_cond_literal(s[pos + len..].trim()) else {
        return PropCond::Always;
    };
    PropCond::Cmp {
        key: key.to_string(),
        op: if neg { CmpOp::Ne } else { CmpOp::Eq },
        rhs,
    }
}

/// 找比较运算符：返回 `(字节位置, 运算符长度, 是否 !=)`。
/// `===` / `!==` 与 `==` / `!=` 当同一件事（多出来那个 `=` 只是作者的写法差异）。
fn find_cmp_op(s: &str) -> Option<(usize, usize, bool)> {
    let b = s.as_bytes();
    for i in 0..b.len() {
        let eq = b[i] == b'=' && i + 1 < b.len() && b[i + 1] == b'=';
        let ne = b[i] == b'!' && i + 1 < b.len() && b[i + 1] == b'=';
        if !eq && !ne {
            continue;
        }
        let len = if i + 2 < b.len() && b[i + 2] == b'=' { 3 } else { 2 };
        return Some((i, len, ne));
    }
    None
}

/// 属性键的形状：非空、只有字母数字 / 下划线 / `$`
/// （`props.a.value`、`a.b.value` 这种带点的写法不认 —— 不猜）
fn is_prop_ident(k: &str) -> bool {
    !k.is_empty() && k.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

/// 条件右边的**单个字面量**：引号字符串 / `true` / `false` / 数字 / 裸词。
/// 裸 token 里带空白（= 复合表达式的一部分）⇒ `None` ⇒ 调用方按「解析不了 ⇒ 恒显示」处理。
fn parse_cond_literal(raw: &str) -> Option<serde_json::Value> {
    if raw.is_empty() {
        return None;
    }
    let quoted = raw.len() >= 2
        && ((raw.starts_with('"') && raw.ends_with('"'))
            || (raw.starts_with('\'') && raw.ends_with('\'')));
    if quoted {
        let inner = &raw[1..raw.len() - 1];
        return if inner.is_empty() {
            None
        } else {
            Some(serde_json::Value::String(inner.to_string()))
        };
    }
    if raw.chars().any(|c| c.is_whitespace()) {
        return None;
    }
    if raw.eq_ignore_ascii_case("true") {
        return Some(serde_json::Value::Bool(true));
    }
    if raw.eq_ignore_ascii_case("false") {
        return Some(serde_json::Value::Bool(false));
    }
    if let Ok(n) = raw.parse::<i64>() {
        return Some(serde_json::json!(n));
    }
    if let Ok(f) = raw.parse::<f64>() {
        return Some(serde_json::json!(f));
    }
    Some(serde_json::Value::String(raw.to_string()))
}

/// 条件是否成立。`value_of` 按**属性键**取那条兄弟属性的当前值（覆盖表优先）。
/// 引用不到那个键（工程里没这条属性 / 拿不到值）⇒ **不隐藏**（同 fail-open 理由）。
fn cond_holds(cond: &PropCond, value_of: &dyn Fn(&str) -> Option<serde_json::Value>) -> bool {
    match cond {
        PropCond::Always => true,
        PropCond::Const(b) => *b,
        PropCond::Cmp { key, op, rhs } => {
            let Some(lhs) = value_of(key) else { return true };
            let eq = cond_eq(&lhs, rhs);
            match op {
                CmpOp::Eq => eq,
                CmpOp::Ne => !eq,
            }
        }
    }
}

/// 条件里的等值比较：`<兄弟属性的值>` vs `<字面量>`。
/// **类型不严格**是必须的、不是放松：同一件事在数据里两种写法都有 ——
/// `3704273480` 的 combo `newproperty` **默认值**是字符串 `"1"`（`options` 的 value 也是字符串），
/// 而用户在下拉里选过之后写回覆盖表的是**数字**（`Message::PropCombo` → `combo_value_json`）
/// ⇒ `"2"` 与 `2` 必须算相等，否则同一条条件的显示状态会随「用户有没有动过这个控件」跳变。
fn cond_eq(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    match (a, b) {
        (serde_json::Value::Null, serde_json::Value::Null) => true,
        (serde_json::Value::Bool(x), serde_json::Value::Bool(y)) => x == y,
        (serde_json::Value::String(x), serde_json::Value::String(y)) => x.trim() == y.trim(),
        (serde_json::Value::Number(x), serde_json::Value::Number(y)) => {
            match (x.as_f64(), y.as_f64()) {
                (Some(x), Some(y)) => (x - y).abs() < 1e-9,
                _ => false,
            }
        }
        // 类型不同（数字 vs 字符串、bool vs 字符串）⇒ 按**标量文本**比（`2` ↔ `"2"`、`true` ↔ `"true"`）
        _ => match (cond_scalar(a), cond_scalar(b)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        },
    }
}

/// 标量 → 规范化文本（数字走 `fmt_num`：`2.0` → `"2"`，这样才能和字符串 `"2"` 对上）
fn cond_scalar(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.trim().to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Number(n) => Some(fmt_num(n.as_f64().unwrap_or(0.0))),
        _ => None,
    }
}

/// `value` 是不是「用户真填过的文本」—— 空的 / `false` / `null` 都不算。
fn user_text_value(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(s) => !s.trim().is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => !o.is_empty(),
        _ => false,
    }
}

/// `value` 是不是「可解析的颜色」—— WE 的 color 属性两种写法都认（`"1 0.5 0"` 与 `[1, 0.5, 0]`）。
/// 纯**形状**判据：字符串按空白切出 ≥3 个能解析的浮点，或数组前三个元素都是数字。
/// 不比对任何具体值、不看属性名 —— 这就是 `classify_prop` 用来判断"这个是能用的色盘"的依据。
fn is_parseable_color(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Array(a) => {
            a.len() >= 3 && a.iter().take(3).all(|x| x.as_f64().is_some())
        }
        serde_json::Value::String(s) => {
            s.split_whitespace().filter(|t| t.parse::<f32>().is_ok()).count() >= 3
        }
        _ => false,
    }
}

/// `text` 是不是本地化键（`ui_browse_properties_*` / `ui_properties_*` / `ui.*`）。
/// 纯字符串判据，不针对具体键名。
fn is_loc_key(text: &str) -> bool {
    let t = text.trim().trim_matches(['"', '\'']);
    let l = t.to_ascii_lowercase();
    l.starts_with("ui_") || l.starts_with("ui.")
}

/// `text` 里有没有**排版级**标记（块级标签 / 换行 / 内联样式块）。
/// 判据：出现块级标签或 `<br>` / 换行 ⇒ 这段 text 是「排版块」而不是控件标签。
/// 只列标签名，不做内容匹配 —— 换任何属性名、任何作者都成立。
fn is_layout_markup(text: &str) -> bool {
    const BLOCK: &[&str] = &[
        "<h1", "<h2", "<h3", "<h4", "<h5", "<h6", "<div", "<p>", "<p ", "<center", "<br", "<hr",
        "<table", "<tr", "<td", "<ul", "<ol", "<li", "<style", "<blockquote", "[center]", "[hr]",
        "[list", "[table", "[quote",
    ];
    let l = text.to_ascii_lowercase();
    if BLOCK.iter().any(|t| l.contains(t)) {
        return true;
    }
    // 真换行也算排版（`\n` 在 HTML 里不换行，但作者写 `text` 时会用它分行）
    text.contains('\n')
}

/// 属性显示名美化：WE 常用 `ui_browse_properties_xxx` 这种本地化键当 `text`，
/// 直接显示是乱码感 —— 去掉前缀、下划线换空格（**通用处理，不针对具体属性名**）。
fn pretty_label(raw: &str) -> String {
    let s = strip_markup(raw);
    let s = s
        .strip_prefix("ui_browse_properties_")
        .or_else(|| s.strip_prefix("ui_properties_"))
        .unwrap_or(&s);
    if s.contains('_') {
        s.replace('_', " ")
    } else {
        s.to_string()
    }
}

/// JSON 值 → 界面上的字符串（滑块/颜色/combo 的当前值都用它）
fn value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => fmt_num(n.as_f64().unwrap_or(0.0)),
        other => other.to_string(),
    }
}

/// combo 选中的值串 → JSON：能当数字/布尔就还原成对应类型，否则当字符串。
/// 这样写回 project.json 语义时类型不会漂。
fn combo_value_json(s: String) -> serde_json::Value {
    match s.as_str() {
        "true" => serde_json::Value::Bool(true),
        "false" => serde_json::Value::Bool(false),
        _ => match s.parse::<f64>() {
            Ok(n) => serde_json::json!(n),
            Err(_) => serde_json::Value::String(s),
        },
    }
}

/// 数字 → 短字符串（整数不带 .0，小数最多三位且去掉尾零）
fn fmt_num(v: f64) -> String {
    if (v - v.round()).abs() < 1e-6 {
        return format!("{}", v.round() as i64);
    }
    let s = format!("{v:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// color 属性值 → (r, g, b)：WE 既可能是 `"1 0.5 0"` 也可能是 `[1, 0.5, 0]`，两种都认。
fn rgb_of(v: &serde_json::Value) -> (f32, f32, f32) {
    match v {
        serde_json::Value::Array(a) => {
            let f = |i: usize| a.get(i).and_then(|x| x.as_f64()).unwrap_or(0.0) as f32;
            clamp_rgb(f(0), f(1), f(2))
        }
        serde_json::Value::String(s) => parse_rgb_str(s),
        _ => (0.6, 0.6, 0.6),
    }
}

fn parse_rgb_str(s: &str) -> (f32, f32, f32) {
    let p: Vec<f32> = s.split_whitespace().filter_map(|t| t.parse().ok()).collect();
    if p.len() >= 3 {
        clamp_rgb(p[0], p[1], p[2])
    } else {
        (0.6, 0.6, 0.6)
    }
}

fn clamp_rgb(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    (r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0))
}

/// 类型 → 角标文字（左上角那小方块）
fn type_badge(t: &str) -> &'static str {
    match t {
        "scene" => "场景",
        "video" => "视频",
        "web" => "网页",
        "image" => "图片",
        "application" => "应用",
        _ => "其他",
    }
}

/// 类型 → 角标底色
fn badge_color(t: &str) -> Color {
    match t {
        "scene" => we_ui::ink_accent(),
        "video" => Color::from_rgb(0.51, 0.30, 0.72),
        "web" => Color::from_rgb(0.20, 0.62, 0.45),
        "image" => Color::from_rgb(0.82, 0.55, 0.16),
        "application" => Color::from_rgb(0.55, 0.55, 0.58),
        _ => Color::from_rgb(0.45, 0.45, 0.48),
    }
}

/// 文本属性输入缓存的键（id 与属性名用不可见分隔符拼，避免撞键）
fn text_key(id: &str, key: &str) -> String {
    format!("{id}\u{1f}{key}")
}

// ── 本程序自己的数据目录（属性覆盖表 / 播放列表）────────────────────────

fn data_dir() -> std::path::PathBuf {
    // `WPHUB_DATA_DIR` 只给**脚本化验收/测试**用（把覆盖表与预设落到临时目录，
    // 不动用户真正的 `~/.local/share/wallpaper-hub`）；不设时逐字节等于原来那条路。
    if let Ok(d) = std::env::var("WPHUB_DATA_DIR") {
        if !d.trim().is_empty() {
            return std::path::PathBuf::from(d);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home).join(".local/share/wallpaper-hub")
}

fn overrides_path() -> std::path::PathBuf {
    data_dir().join("prop_overrides.json")
}

/// 读回逐壁纸属性覆盖表（读不到就是空的，不报错）。
fn load_overrides() -> HashMap<String, HashMap<String, serde_json::Value>> {
    std::fs::read_to_string(overrides_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

// ── 逐场景性能表：帧率────────────────────────────────────────
// 就放在 `prop_overrides.json` 旁边（同一个数据目录）：壁纸 id → fps。
// 表里有记录 = "用户给这条场景钉的值"；没有 = **自适应档**（默认 60，实测不够自动降 30）。
// 刻意**不塞进 cosmic-config**：它是"按内容（场景）记的事实"，与 prop_overrides / presets 同类，
// 一起放数据目录便于随数据迁移，也让"哪条场景钉过 30"一眼可见（老的那个全局 `fps` 键
// 从本次起不再读写，见 `Config` 的注释）。

fn scene_fps_path() -> std::path::PathBuf {
    data_dir().join("scene_fps.json")
}

/// 读回逐场景帧率表（读不到/坏 JSON 就是空的，不报错 —— 下次写回时重建）。
fn load_scene_fps() -> BTreeMap<String, u64> {
    std::fs::read_to_string(scene_fps_path())
        .ok()
        .and_then(|s| serde_json::from_str::<BTreeMap<String, u64>>(&s).ok())
        .unwrap_or_default()
}

/// 落盘。失败只打一行 stderr —— 帧率记录不该因为写盘失败就崩界面。
fn save_scene_fps(map: &BTreeMap<String, u64>) {
    let _ = std::fs::create_dir_all(data_dir());
    match serde_json::to_string_pretty(map) {
        Ok(s) => {
            if let Err(e) = std::fs::write(scene_fps_path(), s) {
                eprintln!("写逐场景帧率表失败: {e}");
            }
        }
        Err(e) => eprintln!("序列化逐场景帧率表失败: {e}"),
    }
}

/// 旧配置里那个全局 `fps` 键的值（cosmic-config 每个字段一个文件：`…/cosmic/<app_id>/v1/fps`）。
/// **只用来在启动时说一句**"老值不再生效"，绝不拿它做任何决定 —— 帧率已经逐场景了。
/// 读不到 / 解析不出就是 `None`（新装机器、或用户从没设过）。
fn legacy_global_fps(app_id: &str) -> Option<u64> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            std::path::PathBuf::from(home).join(".config")
        });
    let p = base.join("cosmic").join(app_id).join("v1").join("fps");
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}

/// 落盘。失败只打一行 stderr —— 属性编辑不该因为写盘失败就崩界面。
fn save_overrides(map: &HashMap<String, HashMap<String, serde_json::Value>>) {
    let _ = std::fs::create_dir_all(data_dir());
    match serde_json::to_string_pretty(map) {
        Ok(s) => {
            if let Err(e) = std::fs::write(overrides_path(), s) {
                eprintln!("写属性覆盖表失败: {e}");
            }
        }
        Err(e) => eprintln!("序列化属性覆盖表失败: {e}"),
    }
}

// ── 「您的预设」：一条壁纸一个槽位（当前改动整套存下 / 整套读回）────────────
// WE 的 7-4 里是「加载 / 保存 / 应用到所有壁纸 / 分享 JSON / 重置」五连。
// 本程序的「加载 / 保存」不做命名槽位、也没有文件对话框（那要 rfd + 一套列表 UI），
// **一条壁纸一个槽位**：保存 = 把这套改动整套记下，加载 = 整套读回来逐条下发。

fn presets_path() -> std::path::PathBuf {
    data_dir().join("presets.json")
}

fn load_presets() -> HashMap<String, HashMap<String, serde_json::Value>> {
    std::fs::read_to_string(presets_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_presets(map: &HashMap<String, HashMap<String, serde_json::Value>>) {
    let _ = std::fs::create_dir_all(data_dir());
    match serde_json::to_string_pretty(map) {
        Ok(s) => {
            if let Err(e) = std::fs::write(presets_path(), s) {
                eprintln!("写预设失败: {e}");
            }
        }
        Err(e) => eprintln!("序列化预设失败: {e}"),
    }
}

/// 属性的 JSON 值是否**配得上** schema 里那个 `type`。
/// 「应用到所有壁纸」只按「键名相同 + 类型对得上」写过去：同名不同类的键
/// （比如这条是 bool 那条是 slider）不硬塞，免得把别人的属性写成垃圾。
fn json_fits_kind(v: &serde_json::Value, kind: PropKind) -> bool {
    match kind {
        PropKind::Bool => v.is_boolean(),
        PropKind::Slider => v.is_number(),
        PropKind::Color | PropKind::Combo | PropKind::Text => v.is_string(),
    }
}

/// 「分享 JSON」的文本本体：id / 名字 / 这套改动（键值原样）。
fn props_json_text(
    id: &str,
    name: &str,
    edits: &HashMap<String, serde_json::Value>,
) -> String {
    let mut map = serde_json::Map::new();
    map.insert(
        "id".to_string(),
        serde_json::Value::String(id.to_string()),
    );
    map.insert(
        "name".to_string(),
        serde_json::Value::String(name.to_string()),
    );
    let props = serde_json::to_value(edits).unwrap_or(serde_json::Value::Null);
    map.insert("properties".to_string(), props);
    serde_json::to_string_pretty(&serde_json::Value::Object(map)).unwrap_or_default()
}

/// 画面色彩六项的名字（下标 → 标签）
fn color_label(i: usize) -> &'static str {
    match i {
        0 => "亮度",
        1 => "对比",
        2 => "饱和",
        3 => "色相",
        4 => "色温",
        _ => "伽马",
    }
}

/// 取景（对齐）方式：值 → 下拉里显示的名字。**和参考图的「对齐方式」是同一个东西**
/// （WE 那里也是四个选项的下拉）。设置页那排按钮、`--perf-msg fit-*`、属性面板的下拉
/// 三处共用这一份，不会漂。
const FIT_MODES: [(&str, &str); 4] = [
    ("cover", "铺满"),
    ("stretch", "拉伸"),
    ("center", "居中"),
    ("contain", "留黑"),
];

/// 画面色彩六项各自的小图标（引擎内置行**每行都要带一个**）。
fn engine_color_icon(i: usize) -> &'static str {
    match i {
        0 => "display-brightness-symbolic",
        1 => "contrast-symbolic",
        2 => "color-select-symbolic",
        3 => "gtk-select-color-symbolic",
        4 => "preferences-desktop-display-symbolic",
        _ => "speedometer-symbolic",
    }
}

/// 参数面板里的一行：上面小字标签，下面控件。
fn param_row<'a>(label: &'a str, control: Element<'a, Message>) -> Element<'a, Message> {
    widget::column::with_capacity(2)
        .push(widget::text::caption(label))
        .push(control)
        .spacing(2)
        .into()
}

/// 音频「分析输出源」列表里"这一项是不是当前生效的那一路"（纯函数，可单测）。
/// `default` 那一项在 `cur` 为空 **或** 就是 `default` 时选中（内核没报时按"跟随系统"算）；
/// 其余项按 sink 节点名**精确**比（和内核 `plan_sink` 的名字匹配口径一致，不做前缀/子串匹配）。
fn sink_option_selected(cur: &str, value: &str) -> bool {
    let cur = cur.trim();
    if value == "default" {
        cur.is_empty() || cur.eq_ignore_ascii_case("default")
    } else {
        cur == value
    }
}

/// 音频「分析输出源」列表里的一个可点项：当前生效那项用实心按钮，其余灰底。
/// `value` = 发给内核的 `sink` 值（`default` 或 sink 节点名 —— 名字比 id 稳，插拔不会换 id 语义）。
fn audio_sink_option(cur: &str, value: &str, label: String) -> Element<'static, Message> {
    let class = if sink_option_selected(cur, value) {
        we_ui::filled_class()
    } else {
        we_ui::gray_class()
    };
    we_ui::icon_button(
        label,
        "audio-volume-high-symbolic",
        class,
        Message::AudioSinkSet(value.to_string()),
    )
}

/// 名字太长就按**字符**截断（中文一个字算一个 —— 别按字节切）。
fn short_name(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// 给提示条用的一句话摘要
fn color_summary(c: &[f32; 6]) -> String {
    format!(
        "亮度 {:.2} · 对比度 {:.2} · 饱和度 {:.2} · 色相 {:+.0}° · 色温 {:+.2} · 伽马 {:.2}",
        c[0],
        c[1],
        c[2],
        c[3].to_degrees(),
        c[4],
        c[5]
    )
}

/// 只有"自动档"才自适应：**逐场景表里有记录**（用户按这条场景钉过 30/60、或手选过）⇒ 一个字都不动。
/// 纯函数 ⇒ 单测直接钉"用户的选择不被自适应打扰"（含本次的新判据）。
fn adaptive_armed(table: &BTreeMap<String, u64>, id: Option<&str>) -> bool {
    let (fps, src) = config::fps_for_scene(table, id);
    src == config::FpsSource::Default && fps == config::SCENE_FPS_DEFAULT
}

/// 读一次机器负载：`(核数, loadavg 1 分钟, 瞬时 runnable 线程数)`，给 `config::external_load` 用。
/// 读不到 `/proc/loadavg` ⇒ 按 `(核数, 0.0, 0)`（= 没有别的负载）走：Linux 上它恒在，
/// 真读不到时这是唯一能做的选择，而且后面还压着"连续 6 次"这一层。
fn machine_load() -> (usize, f64, usize) {
    let ncpu = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let (load1, runnable) = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| config::parse_loadavg(&s))
        .unwrap_or((0.0, 0));
    (ncpu, load1, runnable)
}

/// 整卡 GPU 占用百分比（amdgpu：`/sys/class/drm/card*/device/gpu_busy_percent`）。
/// 读不到 ⇒ `None`（非 amdgpu / 没权限）—— **绝不编一个数**，判据里就当"这条不参与"。
/// 用途只有一个：降档之后判断"这条场景是不是真的把 GPU 打满了"（真重就不升回）。
fn gpu_busy_percent() -> Option<f64> {
    let mut cards: Vec<std::path::PathBuf> = std::fs::read_dir("/sys/class/drm")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix("card"))
                .map(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit()))
                .unwrap_or(false)
        })
        .collect();
    cards.sort();
    for c in cards {
        if let Some(v) = std::fs::read_to_string(c.join("device/gpu_busy_percent"))
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
        {
            return Some(v);
        }
    }
    None
}

/// 下发时 fps 的最终判定（纯函数 → 单测直接钉"不静默覆盖"）。
///
/// * `force` 是"用户刚按的 / 自适应刚定的" ⇒ 就用它，日志写`用户选择` / `自适应`；
///
/// * 否则按**逐场景表**算：有记录 = `逐场景表`，没记录 = `默认`（[`config::SCENE_FPS_DEFAULT`]）。
/// **无论哪条路，都不可能凭空变成"固定 30"** —— 只有"表里给这条记过 30"或"用户刚选 30"
fn resolve_fps(
    table: &BTreeMap<String, u64>,
    id: Option<&str>,
    force: Option<(u64, config::FpsSource)>,
) -> (u64, config::FpsSource) {
    force.unwrap_or_else(|| config::fps_for_scene(table, id))
}

/// `--perf-msg <名字>` 的名字表：只做「名字 → 和设置页按钮**完全相同**的那条 Message」的翻译。
/// 新值由 handler 自己算（`config::next_*` / `step_fps`）—— 这里绝不重算一遍，
/// 否则验证的就是另一份逻辑，而不是界面上那条路（参见 `--perf-preset` 的同一原则）。
fn perf_msg_from_name(name: &str) -> Option<Message> {
    match name {
        "fps-5" | "fps-down" => Some(Message::PerfFpsStep(-5)),
        "fps+5" | "fps-up" => Some(Message::PerfFpsStep(5)),
        // 帧率档（= 设置页「帧率（本场景）」那三个按钮）：自适应 / 60 / 30
        "fps-auto" => Some(Message::SceneFpsAuto),
        "fps-60" => Some(Message::SceneFpsFixed(60)),
        "fps-30" => Some(Message::SceneFpsFixed(30)),
        // 取景模式：= 按设置页「取景模式」那一排按钮里的某一个（几何，不属于性能四项）
        "fit-cover" | "fit" => Some(Message::FitModeSet("cover".into())),
        "fit-stretch" => Some(Message::FitModeSet("stretch".into())),
        "fit-center" => Some(Message::FitModeSet("center".into())),
        "fit-contain" => Some(Message::FitModeSet("contain".into())),
        // 画面色彩：= 按设置页「画面色彩」那几对 −/+ 按钮里的某一个
        "color-reset" => Some(Message::ColorReset),
        "bright+" => Some(Message::ColorStep(0, 0.05)),
        "bright-" => Some(Message::ColorStep(0, -0.05)),
        "contrast+" => Some(Message::ColorStep(1, 0.05)),
        "contrast-" => Some(Message::ColorStep(1, -0.05)),
        "satur+" => Some(Message::ColorStep(2, 0.10)),
        "satur-" => Some(Message::ColorStep(2, -0.10)),
        "hue+" => Some(Message::ColorStep(3, 0.1745)),
        "hue-" => Some(Message::ColorStep(3, -0.1745)),
        "temp+" => Some(Message::ColorStep(4, 0.10)),
        "temp-" => Some(Message::ColorStep(4, -0.10)),
        "scale" => Some(Message::PerfScaleToggle),
        "effects" => Some(Message::PerfEffectsToggle),
        "sleep" => Some(Message::PerfSleepCycle),
        "push" => Some(Message::PerfPush),
        _ => None,
    }
}

impl AppModel {
    /// 设置页：**能效**（发给内核的运行时参数）+ 应用级的事。
    /// 「画面」（缩放/色彩）在右侧详情面板里，属性在「属性」分组里。
    fn view_settings(&self) -> Element<'_, Message> {
        let spacing = cosmic::theme::spacing();
        let (scale, effects, sleep) = config::effective(&self.config);
        let current = config::preset_for(&scale, &effects, &sleep).unwrap_or("custom");
        // 设置页底部显示**配置的**库根目录。只读配置、不探磁盘：这一行每帧都会重建，
        // 而 `library::roots()` 要 stat 几个 Steam 候选目录 + 读 libraryfolders.vdf。
        let dirs_text = if self.config.library_dirs.is_empty() {
            "~/Templates（默认）".to_string()
        } else {
            self.config.library_dirs.join(" · ")
        };

        let mut col = widget::column::with_capacity(14).spacing(spacing.space_s);

        // ── 能效 ──
        col = col.push(we_ui::section("能效"));

        // 三档预设
        let mut preset_row = widget::row::with_capacity(3).spacing(spacing.space_xs);
        for (name, _s, _e, _sl) in config::PRESETS {
            preset_row = preset_row.push(we_ui::icon_button(
                preset_label_static(name),
                "media-playback-start-symbolic",
                if current == name {
                    we_ui::filled_class()
                } else {
                    we_ui::gray_class()
                },
                Message::PerfPreset(name.to_string()),
            ));
        }
        col = col.push(param_row("性能档位", preset_row.into()));

        // 本场景的帧率档：逐场景表里有记录 = 固定档；没有 = 自适应（默认 60）。
        let (cur_fps, cur_src) =
            config::fps_for_scene(&self.scene_fps, self.current_scene_id().as_deref());
        let auto_on = cur_src == config::FpsSource::Default;

        let fps_row: Element<'_, Message> = widget::row::with_capacity(3)
            .push(widget::button::custom(we_ui::label("−", 12.0, we_ui::ink_text()))
                .class(we_ui::gray_class())
                .padding([1.0, 8.0])
                .on_press_down(Message::PerfFpsStep(-5)))
            .push(widget::container(we_ui::label(format!("{cur_fps} fps"), 12.0, we_ui::ink_text()))
                .width(Length::Fixed(64.0))
                .align_x(Horizontal::Center))
            .push(widget::button::custom(we_ui::label("+", 12.0, we_ui::ink_text()))
                .class(we_ui::gray_class())
                .padding([1.0, 8.0])
                .on_press_down(Message::PerfFpsStep(5)))
            .spacing(spacing.space_xxs)
            .align_y(Alignment::Center)
            .into();
        col = col.push(param_row("帧率（本场景）", fps_row));
        // 三选一：自适应（默认 60）/ 固定 60 / 固定 30 —— 都按**这条场景**记住。
        let mode_row: Element<'_, Message> = widget::row::with_capacity(3)
            .push(we_ui::icon_button(
                "自适应",
                "media-playback-start-symbolic",
                if auto_on { we_ui::filled_class() } else { we_ui::gray_class() },
                Message::SceneFpsAuto,
            ))
            .push(we_ui::icon_button(
                "固定 60",
                "media-playback-start-symbolic",
                if !auto_on && cur_fps == 60 { we_ui::filled_class() } else { we_ui::gray_class() },
                Message::SceneFpsFixed(60),
            ))
            .push(we_ui::icon_button(
                "固定 30",
                "media-playback-start-symbolic",
                if !auto_on && cur_fps == 30 { we_ui::filled_class() } else { we_ui::gray_class() },
                Message::SceneFpsFixed(30),
            ))
            .spacing(spacing.space_xs)
            .align_y(Alignment::Center)
            .into();
        col = col.push(param_row("帧率档（本场景）", mode_row));
        // 本场景现在是什么档（一句话说清"按这条场景记住"）
        let scene_name = self
            .current_scene_id()
            .map(|id| {
                self.wallpapers
                    .iter()
                    .find(|w| w.id == id)
                    .map(|w| format!("{}（{id}）", w.name))
                    .unwrap_or(id)
            })
            .unwrap_or_else(|| "（还没有选中/在播的壁纸 ⇒ 先选一条）".to_string());
        col = col.push(we_ui::label(
            format!("本场景 {scene_name}：{}", config::scene_fps_label(cur_fps, cur_src)),
            11.0,
            we_ui::ink_dim(),
        ));
        // 内核**自己报**的生效值（两个数都给，界面不编数据；老内核不回就是 0）
        let kern = self.native.as_ref().map(|n| n.fps).unwrap_or(0);
        col = col.push(we_ui::label(
            if kern > 0 {
                format!("内核当前生效：{kern} fps（内核 status.fps）")
            } else {
                "内核当前生效：未知（内核没连上 / 老内核不回 status.fps）".to_string()
            },
            11.0,
            we_ui::ink_dim(),
        ));
        if let Some(note) = &self.adapt_note {
            col = col.push(we_ui::label(note.clone(), 11.0, we_ui::ink_text()));
        }
        col = col.push(param_row(
            "渲染尺寸",
            we_ui::icon_button(
                scale_label(&scale),
                "applications-graphics-symbolic",
                we_ui::gray_class(),
                Message::PerfScaleToggle,
            ),
        ));
        col = col.push(param_row(
            "特效链",
            we_ui::icon_button(
                effects_label(&effects),
                "applications-graphics-symbolic",
                we_ui::gray_class(),
                Message::PerfEffectsToggle,
            ),
        ));
        col = col.push(param_row(
            "睡眠策略",
            we_ui::icon_button(
                sleep_label(&sleep),
                "media-playback-pause-symbolic",
                we_ui::gray_class(),
                Message::PerfSleepCycle,
            ),
        ));
        col = col.push(we_ui::label(
            "改哪一项只动哪一项；档位是**三项**（渲染尺寸/特效/睡眠）的一组预设，改过就显示「自定义」。",
            11.0,
            we_ui::ink_dim(),
        ));
        col = col.push(we_ui::label(
            "帧率是**逐场景**的（按壁纸 id 记住，不再有全局一把尺）：默认 60 更跟手；\
             重场景（4K + 大量特效）60 也上不去、GPU 会打满 ⇒ 选「固定 30」更稳；\
             轻场景 60 完全跑得动。自适应档先按 60 跑：实测**连续 6 次低于 40** 才降到 30 —— \
             而且降档前会先看机器负载（loadavg），**有别的程序在抢 CPU/内存带宽时不判定**（那不是这条场景的能力，\
             实测：别的进程一编译，轻场景也会被压到 50 以下）；降完还会盯着，负载长期干净 + GPU 没打满就升回 60。\
             每次「不降 / 降 / 升回」都写日志并在这里说明。",
            11.0,
            we_ui::ink_dim(),
        ));

        // ── 应用级 ──
        col = col.push(we_ui::section("应用"));
        col = col.push(param_row(
            "内核服务",
            we_ui::icon_button(
                "重启内核服务",
                "view-refresh-symbolic",
                we_ui::gray_class(),
                Message::RestartKernel,
            ),
        ));
        let lib_row: Element<'_, Message> = widget::row::with_capacity(2)
            .push(we_ui::icon_button(
                "重新扫描",
                "view-refresh-symbolic",
                we_ui::gray_class(),
                Message::Refresh,
            ))
            .push(we_ui::label(
                format!("{} 张", self.wallpapers.len()),
                12.0,
                we_ui::ink_text(),
            ))
            .spacing(spacing.space_xs)
            .align_y(Alignment::Center)
            .into();
        col = col.push(param_row("壁纸库", lib_row));
        col = col.push(we_ui::label(
            format!("库目录：{dirs_text}（Steam 工坊目录若存在则一并扫描，不需要安装 Steam）"),
            11.0,
            we_ui::ink_dim(),
        ));

        // ── 作者宣传图（**默认开**，可关）──
        let promo_on = self.config.show_promo;
        let promo_row: Element<'_, Message> = widget::row::with_capacity(2)
            .push(we_ui::checkbox(promo_on, Message::SetPromo(!promo_on)))
            .push(we_ui::label(
                if promo_on {
                    "显示（默认；会联网拉作者外链）"
                } else {
                    "不显示（不联网）"
                },
                12.0,
                we_ui::ink_text(),
            ))
            .spacing(spacing.space_xs)
            .align_y(Alignment::Center)
            .into();
        col = col.push(param_row("作者宣传图", promo_row));
        col = col.push(we_ui::label(
            "作者嵌在工程文件里的图（实测都是 i.ibb.co 一类外链）。默认开 = 详情面板里显示；\
             关掉后既不出这段、也不发任何请求（下过的走本地缓存）。详情面板里也有同一个开关。",
            11.0,
            we_ui::ink_dim(),
        ));

        // ── 音频（频谱分析跟哪一路输出）────────────────────────────────────
        // 采集线程一次只钉**一个** monitor：默认档 = 跟随系统默认输出（插拔 HDMI/USB 后自动跟着变）；
        // 声音只放到非默认那一路时，在这里指定它（否则分析听不到声音、粒子音频响应不动）。
        col = col.push(we_ui::section("音频"));
        let cur_sink = self
            .native
            .as_ref()
            .map(|s| s.audio_sink.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| self.audio_sink_seen.clone())
            .unwrap_or_default();
        let mut opts = widget::column::with_capacity(self.audio_sinks.len() + 2)
            .spacing(spacing.space_xxs);
        opts = opts.push(audio_sink_option(
            &cur_sink,
            "default",
            "默认（跟随系统）".to_string(),
        ));
        for s in &self.audio_sinks {
            let label = if s.desc.is_empty() || s.desc == s.name {
                format!("{}（id {}）", s.name, s.id)
            } else {
                format!("{} — {}（id {}）", short_name(&s.desc, 30), s.name, s.id)
            };
            opts = opts.push(audio_sink_option(&cur_sink, &s.name, label));
        }
        col = col.push(param_row("分析输出源", opts.into()));
        // **当前实际生效**的那一路（内核 `status.audio_sink`）+ 内核那条采集状态原文
        col = col.push(we_ui::label(
            format!("当前生效：{}", native::sink_label(&cur_sink)),
            11.0,
            we_ui::ink_dim(),
        ));
        match self.native.as_ref().map(|n| n.audio.clone()) {
            Some(a) if !a.is_empty() => {
                col = col.push(we_ui::label(format!("内核采集：{a}"), 11.0, we_ui::ink_dim()));
                // `analyzed=` 单独再写一行（"到底有没有在分析"一眼可见；切 sink 后它才涨）
                if let Some(c) = native::analyzed_count(&a) {
                    col = col.push(we_ui::label(
                        format!("已分析 {c} 个频谱窗"),
                        11.0,
                        we_ui::ink_dim(),
                    ));
                }
            }
            _ => {}
        }
        match &self.audio_sinks_err {
            // 拿不到列表 ⇒ **如实**说（绝不装作"本机没有输出设备"），并给一个「重新枚举」
            Some(e) => col = col.push(we_ui::label(
                format!("拿不到可用输出列表：{e}"),
                11.0,
                we_ui::ink_danger(),
            )),
            None if self.audio_sinks.is_empty() => col = col.push(we_ui::label(
                "正在枚举可用输出…（拿不到会如实说明）",
                11.0,
                we_ui::ink_dim(),
            )),
            None => {}
        }
        col = col.push(we_ui::icon_button(
            "重新枚举可用输出",
            "view-refresh-symbolic",
            we_ui::gray_class(),
            Message::AudioSinksRefresh,
        ));
        col = col.push(we_ui::label(
            "只影响音频分析跟哪一路（粒子音频响应）；不改系统默认输出。",
            11.0,
            we_ui::ink_dim(),
        ));

        // ── 指针（鼠标驱动的特效：X-Ray 透视 / 视差）──────────────────────────
        // 这里显示的**全是内核如实回报的数**：`mouse_src` = 当前这个位置是谁写的
        // （合成器给的 `wl_pointer` / 控制面命令），`pointer_events` = 合成器**到底发没发**
        // 指针事件。为什么要分开显示：`mouse_events:0` 常常只是"指针在窗口上 / 没动"，
        // 并不等于"没收到" —— 这两个数分开才能一眼看出到底是哪种。
        col = col.push(we_ui::section("指针"));
        match self.native.as_ref() {
            Some(n) => {
                col = col.push(we_ui::label(
                    native::pointer_summary(n),
                    11.0,
                    we_ui::ink_dim(),
                ));
                col = col.push(we_ui::label(
                    format!(
                        "驱动画面的换算事件 {} 个 · wl_pointer 输入区 {}",
                        n.mouse_events,
                        if n.pointer_bound { "已绑（整块屏幕）" } else { "没绑上" }
                    ),
                    11.0,
                    we_ui::ink_dim(),
                ));
            }
            None => col = col.push(we_ui::label("内核没连上，指针状态未知", 11.0, we_ui::ink_dim())),
        }
        col = col.push(we_ui::icon_button(
            "鼠标位 → 画面中心",
            "input-mouse-symbolic",
            we_ui::gray_class(),
            Message::PointerToCenter,
        ));
        col = col.push(we_ui::label(
            "透视/视差只在指针位于**桌面空白处**时跟随：指针在任何窗口之上时归那个窗口，\
             背景层看不到它（Wayland 没有「全局指针位置」这种客户端可查询的接口）。\
             想让别的进程驱动它，推控制面 {\"cmd\":\"mouse\",\"x\":..,\"y\":..} 即可（命令行：we-ctl mouse x,y）。",
            11.0,
            we_ui::ink_dim(),
        ));

        let mut sc = widget::scrollable(col);
        if self.settings_bottom {
            // 截图用：设置在底部开（见 `settings_bottom` 字段注释）
            sc = sc.anchor_y(cosmic::iced::widget::scrollable::Anchor::End);
        }
        widget::container(sc.height(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(spacing.space_s)
            .style(we_ui::card_style)
            .into()
    }



}

#[cfg(test)]
mod markup_tests {
    use super::*;

    #[test]
    fn strips_style_blocks_with_their_content() {
        // 这正是实测在壁纸 schema 里看到的形态
        let raw = "<style>img[max-width:100%]</style>ui_browse";
        assert_eq!(strip_markup(raw), "ui_browse");
    }

    #[test]
    fn strips_tags_but_keeps_text() {
        assert_eq!(strip_markup("Playback speed"), "Playback speed");
        assert_eq!(strip_markup("<b>粗体</b> 文本"), "粗体 文本");
        assert_eq!(strip_markup("[b]粗体[/b] 文本"), "粗体 文本");
        assert_eq!(strip_markup("<br>换行<br/>了"), "换行 了");
        assert_eq!(strip_markup("颜色 <color=#fff>白</color>"), "颜色 白");
    }

    #[test]
    fn keeps_brackets_that_are_not_tags() {
        assert_eq!(strip_markup("范围 [0, 1]"), "范围 [0, 1]");
        assert_eq!(strip_markup("数组 a[0]"), "数组 a[0]");
    }

    #[test]
    fn collapses_whitespace_and_survives_empty() {
        assert_eq!(strip_markup("  a  \n b "), "a b");
        assert_eq!(strip_markup(""), "");
        assert_eq!(strip_markup("<style>only style</style>"), "");
    }
}

#[cfg(test)]
mod audio_sink_tests {
    use super::*;

    /// 选择器高亮判据：`default` 那一项在"跟默认/内核没报"时亮，其它按名字精确比。
    #[test]
    fn default_row_is_selected_unless_a_named_sink_is_active() {
        assert!(sink_option_selected("", "default"));
        assert!(sink_option_selected("default", "default"));
        assert!(sink_option_selected(" DEFAULT ", "default"), "大小写/空白都要认");
        assert!(!sink_option_selected("whtest", "default"));
        assert!(sink_option_selected("whtest", "whtest"));
        assert!(!sink_option_selected("whtest", "whtest2"), "不许前缀匹配");
        assert!(!sink_option_selected("whtest.monitor", "whtest"), "节点名要精确");
        assert!(!sink_option_selected("default", "whtest"));
    }

    /// 选择器发出去的值就是内核 `sink` 字段的值（`default` / sink 名），逐字节钉住。
    #[test]
    fn sink_option_sends_the_kernel_spec() {
        assert_eq!(
            native::cmd_audio_sink("default"),
            r#"{"cmd":"audio","sink":"default"}"#
        );
        assert_eq!(
            native::cmd_audio_sink("alsa_output.pci-0000_09_00.1.hdmi-stereo"),
            r#"{"cmd":"audio","sink":"alsa_output.pci-0000_09_00.1.hdmi-stereo"}"#
        );
        assert_eq!(native::cmd_audio_sinks(), r#"{"cmd":"audio","sinks":true}"#);
    }

    /// 界面显示的"当前生效"文案来自内核 `status.audio_sink`；老内核没这个字段时按默认算。
    #[test]
    fn current_effective_sink_label() {
        assert_eq!(native::sink_label(""), "默认（跟随系统）");
        assert_eq!(native::sink_label("default"), "默认（跟随系统）");
        assert_eq!(native::sink_label("whtest"), "whtest");
        // analyzed 计数从 status.audio 里抠（验收"换 sink 才涨"看的就是它）
        assert_eq!(
            native::analyzed_count("monitor:x.monitor:running(live) analyzed=123 sink=default device=<系统默认>"),
            Some(123)
        );
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    /// 面板宽度：**目标 540**（落在要求的 520~560 区间），窗口窄了才退让。
    /// 左边界随**卡片档**走（`card_w × 2 + TILE_GAP`）—— 卡片放大后窄窗口要早点退让。
    #[test]
    fn panel_width_targets_540_and_yields_only_when_narrow() {
        for (w, _) in CARD_SIZES {
            // 1400 起一律 540（1280 下大档要退让到 512 才能保住 2 列，见下面的 `tiles_per_row`）
            assert_eq!(panel_width(1400.0, w), 540.0);
            assert_eq!(panel_width(1920.0, w), 540.0);
            assert_eq!(panel_width(2400.0, w), 540.0);
            assert!(panel_width(99_999.0, w) <= 560.0, "不得超出要求的 520~560 上界");
            // 极窄也不小于 320（再小控件列摆不下）
            assert_eq!(panel_width(600.0, w), 320.0);
        }
        // 1280（默认窗口）：小/中仍是 540，大档退让到 512（= 1280 - 24 - (2×360+24)）
        assert_eq!(panel_width(1280.0, CARD_SIZES[0].0), 540.0);
        assert_eq!(panel_width(1280.0, CARD_SIZES[1].0), 540.0);
        assert_eq!(panel_width(1280.0, CARD_SIZES[2].0), 512.0);
        // 默认档（中 = 280×158）：最小窗口 900 时退让到 320（左边只够一列 280）
        let mid = CARD_SIZES[CARD_DEFAULT].0;
        assert_eq!(panel_width(900.0, mid), 320.0);
        // 小档（220）在 900 宽窗口里退让到 412 —— 900 - 24 - (2×220+24)
        assert_eq!(panel_width(900.0, CARD_SIZES[0].0), 412.0);
    }

    /// 列数 = floor((可用宽 + 间距) / (卡片宽 + 间距))：**随窗口宽与卡片档一起变**。
    #[test]
    fn tiles_per_row_follows_window_and_card_width() {
        let (small, mid, large) = (CARD_SIZES[0].0, CARD_SIZES[1].0, CARD_SIZES[2].0);

        // 1280 宽（默认窗口）：小 3 列 / 中 2 列 / 大 2 列（大档会把面板压到 512 才保住 2 列）
        assert_eq!(tiles_per_row(1280.0, small), 3);
        assert_eq!(tiles_per_row(1280.0, mid), 2);
        assert_eq!(tiles_per_row(1280.0, large), 2);

        // 1400 宽（验收基准）：默认档中 = 2 列；1900 宽 ⇒ 4 列（列数确实跟着窗口变）
        assert_eq!(tiles_per_row(1400.0, mid), 2);
        assert_eq!(tiles_per_row(1900.0, mid), 4);
        assert_eq!(tiles_per_row(1920.0, mid), 4);
        assert_eq!(tiles_per_row(1000.0, mid), 2);
        // 最小窗口 900：默认档只剩 1 列（面板也不能再压，见 `panel_width` 的 320 下界）
        assert_eq!(tiles_per_row(900.0, mid), 1);

        // 极窄：至少 1 列（绝不 0，也绝不崩）
        assert_eq!(tiles_per_row(560.0, mid), 1);
        assert_eq!(tiles_per_row(100.0, mid), 1);
        // 有上限（别把格子摊成牙签）
        assert_eq!(tiles_per_row(2400.0, small), 7);
        assert!(tiles_per_row(99_999.0, small) <= 12, "有上限");

        // 单调性：同一窗口下，卡片越大列越少（这张表就是"响应式"的定义）
        for w in [900.0f32, 1280.0, 1600.0, 2400.0] {
            assert!(tiles_per_row(w, small) >= tiles_per_row(w, mid));
            assert!(tiles_per_row(w, mid) >= tiles_per_row(w, large));
        }
        // 同一档下，窗口越宽列不减
        let mut prev = 0;
        for w in [600.0f32, 900.0, 1200.0, 1600.0, 2000.0, 2560.0] {
            let n = tiles_per_row(w, mid);
            assert!(n >= prev, "窗口变宽列数不该减少（{w}）");
            prev = n;
        }
    }

    /// 卡片默认档 = 中（**280×158**）：用户 第二轮「当前的大（280×158）当标准」。
    #[test]
    fn default_card_is_280x158() {
        assert_eq!(CARD_SIZES[CARD_DEFAULT], (280.0, 158.0));
        // 三档整体上移一级：小 = 上次的中（220×124），大 = 新出的 360×203
        assert_eq!(CARD_SIZES[0], (220.0, 124.0));
        assert_eq!(CARD_SIZES[2], (360.0, 203.0));
        // 三档递增（滑杆从左到右 = 小→大）
        assert!(CARD_SIZES[0].0 < CARD_SIZES[1].0 && CARD_SIZES[1].0 < CARD_SIZES[2].0);
        assert!(CARD_SIZES[0].1 < CARD_SIZES[1].1 && CARD_SIZES[1].1 < CARD_SIZES[2].1);
        // 都是 ≈16:9（0.02 容差），不要某档被压成别的比例
        for (w, h) in CARD_SIZES {
            assert!((w / h - 16.0 / 9.0).abs() < 0.02, "{w}×{h} 不是 16:9");
        }
    }

    /// 卡片间距 = **真 spacing**（不是给卡片加内边距），至少 20 px：
    /// 再小相邻卡片就糊成一片，看不出是两条独立壁纸。
    #[test]
    fn tile_gap_is_a_real_spacing_of_at_least_20px() {
        assert!(
            (20.0..=32.0).contains(&TILE_GAP),
            "间距 {TILE_GAP} 不在 20~32 之间"
        );
    }

    /// 发现/工坊详情页的大图 = **面板内容区全宽**，1400 宽窗口下 ≥ 500 px，且高按 16:9。
    #[test]
    fn workshop_detail_preview_is_the_panel_width_and_at_least_500() {
        let mid = CARD_SIZES[CARD_DEFAULT].0;
        let (w, h) = detail_preview_size(1400.0, mid);
        assert_eq!((w, h), (520.0, 293.0));
        assert!(w >= 500.0, "1400 宽窗口下详情大图必须 ≥500 px，实测 {w}");
        assert!((w / h - 16.0 / 9.0).abs() < 0.02, "{w}×{h} 不是 16:9");
        // 上限 720 起作用（面板本身 540，所以现在拿不到，但函数语义要成立）
        assert_eq!(detail_preview_size(2400.0, mid).0, 520.0);
        assert!(detail_preview_size(2400.0, mid).0 <= DETAIL_PREVIEW_MAX_W);
        // 窄窗口退让，不会把面板撑破
        assert_eq!(detail_preview_size(900.0, mid), (300.0, 169.0));
        // 每一档 1400 宽都 ≥ 500
        for (cw, _) in CARD_SIZES {
            assert!(detail_preview_size(1400.0, cw).0 >= 500.0);
        }
    }

    /// 缩略图预缩放：**输出就是卡片像素**（1:1 上屏 ⇒ 不糊），且比卡片小的源图**不放大**。
    #[test]
    fn thumbnails_are_prescaled_to_card_pixels_and_never_upscaled() {
        // 640×480（4:3）⇒ 220×124：先按 16:9 居中裁剪再缩，输出必须是 220×124
        let big = image::RgbaImage::from_fn(640, 480, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
        });
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(big)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let handle = thumb_from_bytes("test:big", png.get_ref(), 220, 124);
        match handle {
            widget::image::Handle::Rgba { width, height, .. } => {
                assert_eq!((width, height), (220, 124), "必须缩到卡片像素");
            }
            other => panic!("期望 RGBA 句柄，得到 {other:?}"),
        }

        // 100×60 的源图比卡片小 ⇒ 保持原像素（放大只会更糊，交给 GPU 一次线性放大）
        let small = image::RgbaImage::from_fn(100, 60, |_, _| image::Rgba([10, 20, 30, 255]));
        let mut png2 = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(small)
            .write_to(&mut png2, image::ImageFormat::Png)
            .unwrap();
        match thumb_from_bytes("test:small", png2.get_ref(), 220, 124) {
            widget::image::Handle::Rgba { width, height, .. } => {
                // 100×60 按 16:9 居中裁剪 ⇒ 100×56（宽没变 = 没放大）
                assert_eq!((width, height), (100, 56), "小图不许被放大");
            }
            other => panic!("期望 RGBA 句柄，得到 {other:?}"),
        }

        // 同一 (键, 尺寸) 命中缓存 ⇒ 同一个 id（不重复解码/上传）
        let a = thumb_from_bytes("test:big", png.get_ref(), 220, 124);
        let b = thumb_from_bytes("test:big", png.get_ref(), 220, 124);
        assert_eq!(a, b, "同参数必须命中缓存");
    }

    /// 启动时读 sink 音量：解析 `pactl get-sink-volume` 的真实输出（含多声道/小数/垃圾行）。
    #[test]
    fn parses_sink_volume_from_pactl_output() {
        let real = "Volume: front-left: 26241 /  40% / -23.85 dB,   front-right: 26241 /  40% / -23.85 dB\n        balance 0.00\n";
        assert_eq!(parse_pactl_volume(real), Some(40.0));
        assert_eq!(parse_pactl_volume("Volume: mono: 65536 / 100% / 0.00 dB\n"), Some(100.0));
        assert_eq!(parse_pactl_volume("Volume: front-left: 1 /   0% / -inf dB\n"), Some(0.0));
        assert_eq!(parse_pactl_volume("Failure: No such entity\n"), None);
        assert_eq!(parse_pactl_volume(""), None);
    }

    /// 宣传图的等比缩放：塞进给定框内、不超框、不变形。
    #[test]
    fn promo_images_scale_into_the_panel() {
        // 宽横幅：以宽为准
        let (w, h) = fit_into(1600.0, 200.0, 500.0, 300.0);
        assert_eq!((w, h), (500.0, 62.5));
        // 高图：以高为准
        let (w, h) = fit_into(200.0, 1600.0, 500.0, 300.0);
        assert!((w - 37.5).abs() < 0.01 && (h - 300.0).abs() < 0.01, "{w} {h}");
        // 尺寸坏掉时的兜底：给个 16:9
        let (w, h) = fit_into(0.0, 0.0, 500.0, 300.0);
        assert!(w > 0.0 && h > 0.0);
    }

    /// 顶部大图的显示尺寸：不超面板宽 / 不超上限 / **不放大超过源图像素**（用户反馈的那条）。
    #[test]
    fn preview_never_upscales_past_the_source_pixels() {
        // 1080×1080（本机常见）：缩到上限 320，等比仍是正方形
        assert_eq!(preview_display_size(1080, 1080, 520.0), (320.0, 320.0));
        // 小 GIF 180×180（本机 2887537617 就是）：**按原尺寸**显示，不放大
        assert_eq!(preview_display_size(180, 180, 520.0), (180.0, 180.0));
        // 192×192（3299815325）：同上
        assert_eq!(preview_display_size(192, 192, 520.0), (192.0, 192.0));
        // 面板很窄：≤ 面板宽，且仍不超过源图像素
        assert_eq!(preview_display_size(1080, 1080, 200.0), (200.0, 200.0));
        // 宽图 1024×512：上限宽 320 ⇒ 高 160
        assert_eq!(preview_display_size(1024, 512, 520.0), (320.0, 160.0));
        // 极端高图 100×1000：被高度上限收住，宽 40 ≤ 源图宽 100（没放大），比例保持
        let (w, h) = preview_display_size(100, 1000, 520.0);
        assert!(w <= 100.0 && h <= 400.0 + 0.01, "{w} {h}");
        assert!((w / h - 0.1).abs() < 1e-4, "比例要保持：{w} {h}");
        // 尺寸未知：上限内的 16:9
        assert_eq!(preview_display_size(0, 0, 520.0), (320.0, 180.0));
        // 无论哪种输入，都不超过面板宽与上限
        for (sw, sh) in [(4000u32, 10u32), (10, 4000), (320, 320), (1, 1)] {
            let (w, h) = preview_display_size(sw, sh, 520.0);
            assert!(w <= 520.0 && w <= 320.0 + 0.01 && h <= 400.0 + 0.01, "{sw}x{sh} -> {w}x{h}");
            assert!(w <= sw as f32 + 0.01 && h <= sh as f32 + 0.01, "放大超过了源图：{sw}x{sh} -> {w}x{h}");
        }
    }

    /// 只有**像素更多**的本地图才会被选作顶部大图；没有就 None（本机 17 条真实条目就是没有）。
    #[test]
    fn picks_a_larger_local_image_only_when_one_exists() {
        let dir = std::env::temp_dir().join(format!("wh-preview-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 封面 200×200
        image::RgbaImage::new(200, 200).save(dir.join("preview.png")).unwrap();
        // 一张更大的作者图 640×360（应当被选中）
        image::RgbaImage::new(640, 360).save(dir.join("author.png")).unwrap();
        // 一张更小的图（不该被选中）
        image::RgbaImage::new(64, 64).save(dir.join("small.png")).unwrap();
        let got = find_larger_local_image(&dir, Some(200 * 200)).expect("应选到更大的图");
        assert_eq!(got.0.file_name().unwrap(), "author.png");
        assert_eq!(got.1, (640, 360));

        // 删掉更大的那张 ⇒ 只剩封面本身 ⇒ None（不比封面大就不换）
        std::fs::remove_file(dir.join("author.png")).unwrap();
        assert!(find_larger_local_image(&dir, Some(200 * 200)).is_none());
        // 非图片文件不算
        std::fs::write(dir.join("note.png.txt"), b"x").unwrap();
        assert!(find_larger_local_image(&dir, Some(200 * 200)).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod perf_msg_tests {
    use super::*;

    #[test]
    fn flag_names_map_to_the_same_messages_the_buttons_send() {
        assert!(matches!(perf_msg_from_name("scale"), Some(Message::PerfScaleToggle)));
        assert!(matches!(perf_msg_from_name("effects"), Some(Message::PerfEffectsToggle)));
        assert!(matches!(perf_msg_from_name("sleep"), Some(Message::PerfSleepCycle)));
        assert!(matches!(perf_msg_from_name("push"), Some(Message::PerfPush)));
        match perf_msg_from_name("fps+5") {
            Some(Message::PerfFpsStep(5)) => {}
            other => panic!("fps+5 应映射到 PerfFpsStep(5)，实际 {other:?}"),
        }
        match perf_msg_from_name("fps-5") {
            Some(Message::PerfFpsStep(-5)) => {}
            other => panic!("fps-5 应映射到 PerfFpsStep(-5)，实际 {other:?}"),
        }
        // 帧率档三条（= 设置页「帧率档（本场景）」那三个按钮）
        assert!(matches!(perf_msg_from_name("fps-auto"), Some(Message::SceneFpsAuto)));
        match perf_msg_from_name("fps-60") {
            Some(Message::SceneFpsFixed(60)) => {}
            other => panic!("fps-60 应映射到 SceneFpsFixed(60)，实际 {other:?}"),
        }
        match perf_msg_from_name("fps-30") {
            Some(Message::SceneFpsFixed(30)) => {}
            other => panic!("fps-30 应映射到 SceneFpsFixed(30)，实际 {other:?}"),
        }
        assert!(perf_msg_from_name("nonsense").is_none());
    }

    /// **不静默覆盖**：下发时那一个 fps 只可能来自
    /// ①逐场景表 ②用户刚按的 ③自适应刚定的 —— 空表 + 没有 force 时**必须是新默认 60**，
    /// 而且日志里的"来源"要如实写出来。这条测试是那个bug的回归网。
    #[test]
    fn fps_is_never_silently_pinned_to_a_fixed_30() {
        let mut t = BTreeMap::new();
        // 空表、没有 force ⇒ 60（默认）。旧行为会在这里给 30 ✗
        assert_eq!(resolve_fps(&t, Some("3571777169"), None), (60, config::FpsSource::Default));
        assert_eq!(resolve_fps(&t, None, None), (60, config::FpsSource::Default));
        // 别的场景记过 30 ⇒ **不牵连**这一条（"逐场景"的意思）
        t.insert("X".into(), 30);
        assert_eq!(resolve_fps(&t, Some("3571777169"), None), (60, config::FpsSource::Default));
        // 这条记过 30 ⇒ 才是 30，且来源=逐场景表（不是"默认"、更不是无声无息）
        t.insert("3571777169".into(), 30);
        assert_eq!(resolve_fps(&t, Some("3571777169"), None), (30, config::FpsSource::SceneTable));
        // 用户刚按 30（force）⇒ 来源=用户选择；自适应降档 ⇒ 来源=自适应
        assert_eq!(
            resolve_fps(&BTreeMap::new(), Some("A"), Some((30, config::FpsSource::UserChoice))),
            (30, config::FpsSource::UserChoice)
        );
        assert_eq!(
            resolve_fps(&BTreeMap::new(), Some("A"), Some((30, config::FpsSource::Adaptive))),
            (30, config::FpsSource::Adaptive)
        );
        // 来源标签就是日志里那几个中文词
        assert_eq!(config::FpsSource::Default.label(), "默认");
        assert_eq!(config::FpsSource::SceneTable.label(), "逐场景表");
    }

    /// 用户钉过 / 手选过的场景，自适应**一个字都不动**（本次改了判据之后也必须保持）：
    /// 事故当时就是靠 `scene_fps.json = {"2564205376":60,"3571777169":30}` 临时按住的。
    #[test]
    fn pinned_scene_is_never_touched_by_the_new_judgement() {
        let mut t = BTreeMap::new();
        config::set_scene_fps(&mut t, "2564205376", 60);
        config::set_scene_fps(&mut t, "3571777169", 30);
        assert!(!adaptive_armed(&t, Some("2564205376")), "钉过 60 的场景不吃自适应");
        assert!(!adaptive_armed(&t, Some("3571777169")), "钉过 30 的场景更不该被自适应碰");
        // 钉了 60（恰好等于默认值）也算「用户钉过」—— 表里有记录就是用户的话，不许当成自动档
        assert_eq!(config::fps_for_scene(&t, Some("2564205376")), (60, config::FpsSource::SceneTable));
        // 没记过的场景才是自动档
        assert!(adaptive_armed(&t, Some("1931934283")));
        assert!(adaptive_armed(&t, None));
        // 记 0 不算「钉过」（坏数据不当成用户的选择）⇒ 仍然走自适应
        t.insert("0fps".into(), 0);
        assert!(adaptive_armed(&t, Some("0fps")));
    }

    /// 负载 / GPU 的**读**这一层也要能跑通：读到就如实给，读不到就 `None` —— **绝不编数**。
    #[test]
    fn machine_load_and_gpu_probes_read_the_real_machine() {
        let (ncpu, load1, runnable) = machine_load();
        assert!(ncpu >= 1, "核数至少 1");
        assert!(load1 >= 0.0, "loadavg 不可能是负数");
        assert!(runnable <= 1_000_000);
        // 判据本身是纯函数：这里只确认「能用真读数跑一遍不炸」
        let _ = config::external_load(ncpu, load1, runnable);
        match gpu_busy_percent() {
            // 本机是 amdgpu ⇒ 读得到；读到了就必须是个合理的百分比
            Some(g) => assert!((0.0..=100.0).contains(&g), "gpu_busy_percent 是百分比：{g}"),
            // 别的机器可能没有这个节点 ⇒ 只能是 None（不许编一个假数）
            None => {}
        }
    }
}

#[cfg(test)]
mod auto_apply_tests {
    use super::*;

    /// 造一个内核 status：`kind` + `path` 决定"有没有内容"，`playing/suppressed/user_paused`
    /// 只影响"有没有在画"。
    fn st(kind: &str, path: &str, playing: bool, user_paused: bool, suppressed: bool) -> native::Status {
        native::Status {
            kind: kind.into(),
            path: path.into(),
            playing,
            user_paused,
            suppressed,
            ..Default::default()
        }
    }

    /// ① **内核有在播 ⇒ 绝不换掉它**（这就是本次要修的真实困扰：重启内核后桌面被换成
    /// app"上次应用"那条）。即使手上确实记着一条"上次应用"，判定也必须是 `KernelBusy`。
    #[test]
    fn kernel_busy_is_authoritative_so_last_applied_is_never_pushed() {
        // 内核在播 X-Ray 2564205376；app 记得"上次应用"= 1931934283（两者不同）
        let playing = st(
            "scene",
            "/home/user/.local/share/Steam/steamapps/workshop/content/431960/2564205376",
            true,
            false,
            false,
        );
        assert!(kernel_has_content(Some(&playing)), "内核有内容 ⇒ 算在播");
        assert_eq!(
            decide_auto_apply(true, kernel_has_content(Some(&playing)), true),
            AutoApply::KernelBusy,
            "内核在播 ⇒ 不推进'上次应用'（绝不换掉你正在看的）"
        );
        // 开关是开的也一样：内核权威优先于开关
        assert_ne!(
            decide_auto_apply(true, true, true),
            AutoApply::PushLast,
            "有在播时**任何**开关组合都不许推"
        );
    }

    /// ② **内核无在播 ⇒ 才允许推进"上次应用"**（且只有这时才真的下发 `apply`）。
    #[test]
    fn only_when_kernel_is_idle_do_we_push_the_last_applied() {
        // 内核空闲（kind=none / 空 path）+ 手上有"上次应用" ⇒ 推
        let idle = st("none", "", false, false, false);
        assert!(!kernel_has_content(Some(&idle)), "kind=none + 空 path ⇒ 没内容");
        assert_eq!(
            decide_auto_apply(true, kernel_has_content(Some(&idle)), true),
            AutoApply::PushLast,
            "内核空闲 ⇒ 才允许推进'上次应用'"
        );
        // 内核空闲但没记过 ⇒ 没东西可推
        assert_eq!(decide_auto_apply(true, false, false), AutoApply::NothingRemembered);
        // 连不上内核（None）也按"没在播"处理 —— 不能因为拿不到状态就去换桌面
        assert!(!kernel_has_content(None));
        assert_eq!(decide_auto_apply(true, kernel_has_content(None), true), AutoApply::PushLast);
    }

    /// ③ **开关关闭 ⇒ 完全不动作**（四种组合里，`Disabled` 永远压过 `KernelBusy`/`PushLast`）。
    #[test]
    fn switch_off_means_no_action_at_all() {
        for busy in [true, false] {
            for has_last in [true, false] {
                assert_eq!(
                    decide_auto_apply(false, busy, has_last),
                    AutoApply::Disabled,
                    "开关关了 ⇒ 一个字也不推（busy={busy} has_last={has_last}）"
                );
            }
        }
    }

    /// ④ **"用户暂停"与"全屏遮挡"都还算是内容**：内核没在画（`playing=false`）但那条壁纸
    /// 还在 ⇒ 也**不算空闲**、不许被"上次应用"换掉。（判据刻意不看 `playing`。）
    #[test]
    fn paused_or_suppressed_wallpaper_still_counts_as_content() {
        let paused = st("scene", "/x/scene.pkg", false, true, false);
        assert!(kernel_has_content(Some(&paused)), "用户暂停的那条仍是内容 ⇒ 不算空闲");
        assert_eq!(decide_auto_apply(true, kernel_has_content(Some(&paused)), true), AutoApply::KernelBusy);

        let covered = st("scene", "/x/scene.pkg", false, false, true);
        assert!(kernel_has_content(Some(&covered)), "全屏窗口遮挡 ⇒ 内容还在");
        assert_eq!(decide_auto_apply(true, kernel_has_content(Some(&covered)), true), AutoApply::KernelBusy);
    }

    /// ⑤ 从**真·内核 status 行**（内核自己那套格式）解析出来也要判对 —— 避免"只在手造
    /// 结构体上成立、换个字段名就漂"。
    #[test]
    fn parses_real_kernel_status_lines() {
        let playing = r#"{"ok":true,"kind":"scene","path":"/home/user/.local/share/Steam/steamapps/workshop/content/431960/2564205376","playing":true,"suppressed":false,"user_paused":false,"frames":1234,"fps":60}"#;
        let s = native::Status::parse(playing).expect("ok:true 要能解析");
        assert!(kernel_has_content(Some(&s)), "内核报了 scene + path ⇒ 有内容");

        let empty = r#"{"ok":true,"kind":"none","path":"","playing":false,"suppressed":false,"user_paused":false,"frames":0}"#;
        let s = native::Status::parse(empty).expect("ok:true 要能解析");
        assert!(!kernel_has_content(Some(&s)), "kind=none + path 空 ⇒ 空闲");
    }

    /// ⑥ 默认是"不乱换"：持久配置里 `auto_apply_last` 默认 `true`（允许在内核空闲时补一条），
    /// 而**内核在播时**仍然判成 `KernelBusy`（不动作）。
    #[test]
    fn config_default_is_do_not_swap() {
        let cfg = config::Config::default();
        assert!(cfg.auto_apply_last, "默认允许'内核空闲时补一条'");
        assert_eq!(
            decide_auto_apply(cfg.auto_apply_last, true, true),
            AutoApply::KernelBusy,
            "默认配置下，内核在播 ⇒ 依然一个字也不推"
        );
    }
}

#[cfg(test)]
mod color_slot_tests {
    use super::*;

    /// **的 bug**：调色的落点 —— **没有选中的壁纸 ⇒ 一个字也不写**。
    /// 以前无选中时会把值写进**全局** `color`（`Config::color`，只是老配置的迁移默认值）
    /// ⇒ 所有"还没记过槽"的壁纸一起被改掉，把"色彩按壁纸 id 分槽"的设计打穿。
    /// 真实触发路径是 `--perf-msg color-reset`（性能自检在界面选中任何壁纸之前就按了那个按钮）。
    #[test]
    fn color_never_writes_the_global_field_without_a_selection() {
        let mut cfg = config::Config::default();

        // ① 没有选中的壁纸 ⇒ 全局 `color` 与分槽表**都不许动**
        assert_eq!(
            write_color(&mut cfg, None, [1.2, 1.3, 1.0, 0.0, 0.0, 1.0]),
            ColorWrite::Nothing
        );
        assert!(cfg.color.is_empty(), "无选中时绝不许写全局 color");
        assert!(cfg.color_by_id.is_empty());

        // 老配置迁移来的全局值也不能被"无选中"的调色改掉
        cfg.color = "0.8,0.9,1,0,0,1".into();
        assert_eq!(
            write_color(&mut cfg, None, [1.0, 1.0, 1.0, 0.0, 0.0, 1.0]),
            ColorWrite::Nothing
        );
        assert_eq!(cfg.color, "0.8,0.9,1,0,0,1", "无选中时全局值保持原样");

        // ② 选中 A ⇒ 只写 A 的槽；全局照旧；别的壁纸不受影响
        let w = write_color(&mut cfg, Some("A"), [1.2, 1.0, 1.0, 0.0, 0.0, 1.0]);
        assert_eq!(
            w,
            ColorWrite::Slot {
                id: "A".into(),
                value: "1.2,1,1,0,0,1".into()
            }
        );
        assert_eq!(cfg.color_by_id.get("A").map(String::as_str), Some("1.2,1,1,0,0,1"));
        assert_eq!(cfg.color, "0.8,0.9,1,0,0,1", "分槽写入不许碰全局");
        assert_eq!(
            config::color_for_id(&cfg, "B"),
            "0.8,0.9,1,0,0,1",
            "别的壁纸仍走（迁移来的）全局默认"
        );

        // ③ 中性值 ⇒ 删掉这条覆盖（等于回到"没记过"）
        let w = write_color(&mut cfg, Some("A"), [1.0, 1.0, 1.0, 0.0, 0.0, 1.0]);
        assert_eq!(
            w,
            ColorWrite::Slot {
                id: "A".into(),
                value: "1,1,1,0,0,1".into()
            }
        );
        assert!(cfg.color_by_id.get("A").is_none(), "中性值等于没有覆盖");

        // ④ 连按几次"没选中时的颜色键"（`--perf-msg color-reset` / `bright+` 那条路）：
        // 全局值、分槽表都原封不动
        let before = cfg.color.clone();
        for c in [
            [1.0f32, 1.0, 1.0, 0.0, 0.0, 1.0],
            [1.05, 1.0, 1.0, 0.0, 0.0, 1.0],
        ] {
            assert_eq!(write_color(&mut cfg, None, c), ColorWrite::Nothing);
        }
        assert_eq!(cfg.color, before);
        assert!(cfg.color_by_id.is_empty());
    }

    /// `--perf-msg` 里那批色彩名字确实映射到走 `set_color` 的 Message
    /// （`color-reset` 就是 报上来的那条路径）。
    #[test]
    fn perf_color_names_go_through_set_color() {
        assert!(matches!(
            perf_msg_from_name("color-reset"),
            Some(Message::ColorReset)
        ));
        assert!(matches!(
            perf_msg_from_name("bright+"),
            Some(Message::ColorStep(0, _))
        ));
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Page {
    Wallpapers,
    Discover,
    Workshop,
    Settings,
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum ContextPage {
    #[default]
    About,
}

#[cfg(test)]
mod prop_tests {
    use super::*;

    /// 数据驱动：**foreach** 遍历 `general.properties`，按每条自己的 `type` 决定控件。
    /// 顺序按 `index`；认不出来的类型退回文本，而不是丢掉。
    #[test]
    fn parses_every_property_by_its_own_type() {
        let schema = r#"{
            "alpha_bool": {"index": 2, "type": "bool", "value": true, "text": "开关 A"},
            "beta_slider": {"index": 1, "type": "slider", "value": 0.5, "min": 0, "max": 1, "text": "滑条"},
            "gamma_color": {"index": 3, "type": "color", "value": "1 0.5 0"},
            "delta_combo": {"index": 4, "type": "combo", "value": "1",
                            "options": [{"label": "开", "value": "1"}, {"label": "关", "value": "0"}]},
            "epsilon_text": {"index": 5, "type": "text", "value": "hi"},
            "zeta_unknown": {"index": 6, "type": "wat"}
        }"#;
        let props = parse_props(schema);
        assert_eq!(props.len(), 6, "每一条都要出来（不按名字过滤）");
        let keys: Vec<&str> = props.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "beta_slider",
                "alpha_bool",
                "gamma_color",
                "delta_combo",
                "epsilon_text",
                "zeta_unknown"
            ],
            "顺序 = index，而不是文件顺序/名字"
        );
        assert_eq!(props[0].kind, PropKind::Slider);
        assert_eq!((props[0].min, props[0].max), (0.0, 1.0));
        assert_eq!(props[1].kind, PropKind::Bool);
        assert_eq!(props[1].label, "开关 A");
        assert_eq!(props[2].kind, PropKind::Color);
        assert_eq!(props[3].kind, PropKind::Combo);
        assert_eq!(props[3].options.len(), 2);
        assert_eq!(props[3].options[0], ("开".to_string(), "1".to_string()));
        assert_eq!(props[4].kind, PropKind::Text);
        // 认不出来的类型退回文本控件，而不是丢掉
        assert_eq!(props[5].kind, PropKind::Text);
    }

    #[test]
    fn missing_or_broken_schema_is_empty_not_error() {
        assert!(parse_props("").is_empty());
        assert!(parse_props("not json").is_empty());
        assert!(parse_props("[]").is_empty());
        // 没有 type 字段 ⇒ 当文本，不 panic
        let props = parse_props(r#"{"a": {"value": "x"}}"#);
        assert_eq!(props.len(), 1);
        assert_eq!(props[0].kind, PropKind::Text);
    }

    #[test]
    fn plain_array_options_and_we_localisation_keys() {
        let schema = r#"{"a": {"type": "combo", "value": "x", "options": ["x", "y"]},
                          "b": {"type": "color", "value": "0 0 0",
                                "text": "ui_browse_properties_scheme_color"},
                          "c": {"type": "bool", "value": true,
                                "text": "ui_browse_properties_some_toggle"}}"#;
        let props = parse_props(schema);
        assert_eq!(props[0].options[0], ("x".to_string(), "x".to_string()));
        assert_eq!(props[1].label, "scheme color", "本地化键要美化成人话");
        // 本地化键 + color ⇒ 仍然是**可编辑色盘**；本地化键 + bool ⇒ 跳过
        assert_eq!(props[1].form, PropForm::Editable(PropKind::Color));
        assert_eq!(props[2].form, PropForm::LocalizedKey);
    }

    #[test]
    fn color_values_accept_both_string_and_array() {
        assert_eq!(parse_rgb_str("1 0.5 0"), (1.0, 0.5, 0.0));
        assert_eq!(rgb_of(&serde_json::json!([0.2, 0.4, 0.6])), (0.2, 0.4, 0.6));
        assert_eq!(rgb_of(&serde_json::json!("0.1 0.2 0.3")), (0.1, 0.2, 0.3));
        // 越界要夹回 0..1
        assert_eq!(parse_rgb_str("2 -1 0.5"), (1.0, 0.0, 0.5));
    }

    #[test]
    fn combo_value_round_trips_json_types() {
        // 数字串要还原成 JSON 数字（这里只比数值，1 与 1.0 在 serde_json 里不是同一个 Number）
        assert_eq!(combo_value_json("1".into()).as_f64(), Some(1.0));
        assert_eq!(combo_value_json("true".into()), serde_json::Value::Bool(true));
        assert_eq!(
            combo_value_json("On".into()),
            serde_json::Value::String("On".into())
        );
    }

    #[test]
    fn numbers_format_without_trailing_noise() {
        assert_eq!(fmt_num(50.0), "50");
        assert_eq!(fmt_num(0.8), "0.8");
        assert_eq!(fmt_num(0.123456), "0.123");
    }

    /// 「分享 JSON」导出的就是这份文本 —— id / 名字 / 键值原样，能被解析回来。
    #[test]
    fn share_json_carries_id_name_and_every_edit() {
        let mut edits: HashMap<String, serde_json::Value> = HashMap::new();
        edits.insert("petals".into(), serde_json::Value::Bool(true));
        edits.insert("soundcolor".into(), serde_json::Value::String("1 0.9 0.47".into()));
        edits.insert("skirttrialoption".into(), serde_json::Value::Bool(false));
        let text = props_json_text("3571777169", "just a girl v.2", &edits);
        let v: serde_json::Value = serde_json::from_str(&text).expect("必须是合法 JSON");
        assert_eq!(v["id"], "3571777169");
        assert_eq!(v["name"], "just a girl v.2");
        assert_eq!(v["properties"]["petals"], serde_json::Value::Bool(true));
        assert_eq!(v["properties"]["skirttrialoption"], serde_json::Value::Bool(false));
        assert_eq!(v["properties"]["soundcolor"], "1 0.9 0.47");
        assert_eq!(v["properties"].as_object().unwrap().len(), 3, "一项都不能漏");
    }

    /// 「应用到所有壁纸」的准入：**键名相同 + 类型对得上**才写过去。
    #[test]
    fn apply_all_only_takes_type_matching_values() {
        assert!(json_fits_kind(&serde_json::Value::Bool(true), PropKind::Bool));
        assert!(!json_fits_kind(&serde_json::Value::Bool(true), PropKind::Slider));
        assert!(json_fits_kind(&serde_json::json!(1.0), PropKind::Slider));
        assert!(!json_fits_kind(&serde_json::json!("1"), PropKind::Slider));
        assert!(json_fits_kind(&serde_json::json!("1 0 0"), PropKind::Color));
        assert!(!json_fits_kind(&serde_json::json!(7), PropKind::Color));
        assert!(json_fits_kind(&serde_json::json!("2"), PropKind::Combo));
        assert!(json_fits_kind(&serde_json::json!("hi"), PropKind::Text));
    }

    /// 预设槽位：写下去、读回来，键值一字不差（含 bool / 数字 / 字符串三种类型）。
    #[test]
    fn preset_slot_round_trips_through_json() {
        let mut slot: HashMap<String, serde_json::Value> = HashMap::new();
        slot.insert("chains".into(), serde_json::Value::Bool(true));
        slot.insert("volume".into(), serde_json::json!(50.0));
        slot.insert("soundcolor".into(), serde_json::Value::String("1 0 0".into()));
        let mut all: HashMap<String, HashMap<String, serde_json::Value>> = HashMap::new();
        all.insert("3571777169".into(), slot.clone());
        let text = serde_json::to_string_pretty(&all).unwrap();
        let back: HashMap<String, HashMap<String, serde_json::Value>> =
            serde_json::from_str(&text).unwrap();
        assert_eq!(back.get("3571777169"), Some(&slot));
    }

    // ── ① 作者属性「该不该出可编辑控件」的判定（只按数据，不按属性名）─────────

    /// 逐条规则：空 text / 图片型 / 排版块标题 / 本地化键 / 真编辑类型 / textinput 的 value。
    #[test]
    fn classify_prop_only_looks_at_data() {
        let j = |s: &str| serde_json::Value::String(s.to_string());
        let f = |t: &str, text: &str, v: serde_json::Value| classify_prop(t, text, &v);

        // 1) 空 text ⇒ 跳过（本机 2887537617 的 xian / xinzeng2…9 就是这种）
        assert_eq!(f("text", "", j("false")), PropForm::Blank);
        assert_eq!(f("text", "   ", j("false")), PropForm::Blank);
        // 2) 本地化键 ⇒ 跳过（键名不当标签）
        // ⚠ 例外：**真·颜色控件**（type=color + 可解析颜色）留作可编辑色盘
        assert_eq!(
            f("color", "ui_browse_properties_scheme_color", j("0 0 0")),
            PropForm::Editable(PropKind::Color),
            "本地化键挂在真颜色控件上 ⇒ 保留成色盘（整行跳过的话用户看不到它）"
        );
        // 判据只看数据：换个键名、换个可解析颜色写法，结论一样
        assert_eq!(
            f("color", "ui_properties_whatever_scheme", j("0.1 0.2 0.3")),
            PropForm::Editable(PropKind::Color)
        );
        assert_eq!(
            f("color", "ui_browse_properties_scheme_color", serde_json::json!([0.2, 0.4, 0.6])),
            PropForm::Editable(PropKind::Color),
            "数组写法的颜色同样认"
        );
        // 不是 color 类型 / 值不成形状 ⇒ 照旧跳过（不许把任意本地化键都变成控件）
        assert_eq!(
            f("bool", "ui_browse_properties_scheme_color", j("0 0 0")),
            PropForm::LocalizedKey
        );
        assert_eq!(
            f("color", "ui_browse_properties_scheme_color", j("not a colour")),
            PropForm::LocalizedKey,
            "值不是可解析颜色 ⇒ 不给色盘"
        );
        assert_eq!(
            f("color", "ui_browse_properties_scheme_color", serde_json::json!(true)),
            PropForm::LocalizedKey
        );
        // HTML/图片型 text 不享受这个例外：图片型仍归「宣传图」段（只读展示）
        assert_eq!(
            f(
                "color",
                "ui_browse_properties_scheme_color <img src=\"https://x/1.gif\">",
                j("0 0 0")
            ),
            PropForm::LocalizedKey
        );
        assert_eq!(
            f("color", "<b><img src=\"https://x/1.gif\"></b>", j("0 0 0")),
            PropForm::MediaOnly,
            "图片型 text（去掉标记没文字）⇒ 只读展示，不出色盘"
        );
        // 3) 图片型（去掉标记没文字）⇒ 归「宣传图」段，本行不出
        assert_eq!(
            f("text", "<img src=\"https://x/1.gif\" width=\"100%\">", j("false")),
            PropForm::MediaOnly
        );
        assert_eq!(
            f("text", "<br/><br><a href=''><img src=\"https://x/1.gif\"></a><br>", j("false")),
            PropForm::MediaOnly
        );
        // 4) 排版块 + 有文字 ⇒ 作者的分组标题 / 横幅（`type` 写 bool 也一样，看的是数据）
        assert_eq!(
            f("text", "<h3>🎚️全局 Global</h3>", j("false")),
            PropForm::Heading("🎚️全局 Global".into())
        );
        assert_eq!(
            f("bool", "<h4>💷赞助</h4>", j("false")),
            PropForm::Heading("💷赞助".into())
        );
        // 排版块 + 没文字 + 没图 ⇒ 跳过
        assert_eq!(f("text", "<br><br>", j("false")), PropForm::Blank);
        // 5a) 真编辑类型 + 干净标签 ⇒ 可编辑
        assert_eq!(f("bool", "右腿丝袜", j("false")), PropForm::Editable(PropKind::Bool));
        assert_eq!(f("slider", "速度", serde_json::json!(1.0)), PropForm::Editable(PropKind::Slider));
        assert_eq!(f("color", "时间颜色", j("1 0 0")), PropForm::Editable(PropKind::Color));
        assert_eq!(f("combo", "主题", j("1")), PropForm::Editable(PropKind::Combo));
        assert_eq!(
            f("textinput", "Text", j("mouse pad")),
            PropForm::Editable(PropKind::Text)
        );
        // 5b) 行内标记包着的标签照样是可编辑项的标签（只去标记，不改成只读）
        assert_eq!(f("bool", "<b>音频</b>", serde_json::json!(true)), PropForm::Editable(PropKind::Bool));
        // 5c) `text` 类型但用户没填过值 ⇒ 只读一行（不出输入框）
        assert_eq!(
            f("text", "右腿丝袜", serde_json::json!(false)),
            PropForm::ReadOnlyNote("右腿丝袜".into())
        );
        // 5d) `text` 类型且 value 是用户真填的字符串 ⇒ 保留可编辑
        assert_eq!(f("text", "Text", j("mouse pad")), PropForm::Editable(PropKind::Text));
        // 认不出来的类型 + 没文字 ⇒ 跳过（不给出没标签的控件）
        assert_eq!(f("wat", "", serde_json::Value::Null), PropForm::Blank);
    }

    /// 真数据回归：本机装的 `2887537617`（麻匪 礼服斯卡蒂）与 `3704273480`（麻匪 夏日影）。
    /// 打印「可编辑控件数 / 各跳过项 / 只读块数」，并钉住数字 —— 面板里不该再有没意义的输入框。
    #[test]
    fn real_projects_have_no_meaningless_editable_controls() {
        // 本地库根目录（默认 `~/Templates`；"搬过家"的工坊内容照样找得到）
        let base = crate::wp::library::roots(&[])
            .into_iter()
            .next()
            .unwrap_or_default();
        let mut checked = 0;
        for id in ["2887537617", "3704273480"] {
            let dir = base.join(id);
            let Ok(js) = std::fs::read_to_string(dir.join("project.json")) else {
                eprintln!("跳过：本机没有 {id} 的 project.json");
                continue;
            };
            let v: serde_json::Value = serde_json::from_str(&js).unwrap();
            let schema = v
                .get("general")
                .and_then(|g| g.get("properties"))
                .map(|p| serde_json::to_string(p).unwrap())
                .unwrap_or_default();
            let props = parse_props(&schema);

            let mut editable = 0;
            let mut heading = 0;
            let mut readonly = 0;
            let mut media = 0;
            let mut blank = 0;
            let mut loc = 0;
            for p in &props {
                match &p.form {
                    PropForm::Editable(_) => editable += 1,
                    PropForm::Heading(_) => heading += 1,
                    PropForm::ReadOnlyNote(_) => readonly += 1,
                    PropForm::MediaOnly => media += 1,
                    PropForm::Blank => blank += 1,
                    PropForm::LocalizedKey => loc += 1,
                }
            }
            eprintln!(
                "[属性面板] {id}: 共 {} 条 ⇒ 可编辑 {editable} / 作者标题 {heading} / 只读 {readonly} \
                 / 图片型(归宣传图) {media} / 空 text 跳过 {blank} / 本地化键跳过 {loc}",
                props.len()
            );

            if id == "2887537617" {
                // 23 条里 9 条是真控件（4 bool + 2 slider + 2 color + **1 个本地化键色盘**）
                // `schemecolor`（`type=color` + `text=ui_browse_properties_scheme_color`）
                // 以前整行跳过 ⇒ 用户看不到那个色盘；现在保留成可编辑色盘 ⇒ 可编辑 8→9、本地化键跳过 1→0
                assert_eq!(
                    (editable, heading, media, blank, loc),
                    (9, 1, 7, 6, 0),
                    "2887537617 的分类变了：{:?}",
                    props.iter().map(|p| (&p.key, &p.form)).collect::<Vec<_>>()
                );
                // 真控件那几条的键，一条不能少（顺序无关，比集合）
                let mut eds: Vec<&str> = props
                    .iter()
                    .filter(|p| matches!(p.form, PropForm::Editable(_)))
                    .map(|p| p.key.as_str())
                    .collect();
                eds.sort_unstable();
                let mut want = vec![
                    "newproperty",
                    "newproperty1",
                    "newproperty2",
                    "newproperty3",
                    "newproperty4",
                    "newproperty5",
                    "newproperty6",
                    "newproperty7",
                    "schemecolor",
                ];
                want.sort_unstable();
                assert_eq!(eds, want);
                // 那个键名挂在真颜色控件上 ⇒ 必须是**色盘**（标签用美化后的键名）
                let scheme = props.iter().find(|p| p.key == "schemecolor").unwrap();
                assert_eq!(scheme.form, PropForm::Editable(PropKind::Color));
                assert_eq!(scheme.kind, PropKind::Color);
                assert_eq!(scheme.label, "scheme color");
                // 图片型那条（gexing）必须落进「宣传图」抽取里，不能两头都丢
                let gexing = props.iter().find(|p| p.key == "gexing").unwrap();
                assert_eq!(gexing.form, PropForm::MediaOnly);
                assert!(!promo::img_refs(&gexing.raw_text).is_empty());
            } else {
                // 20 条里 6 条真控件（2 bool + 1 combo + 2 color + 1 textinput）+ 4 个作者标题
                // 多出来的那条是 `schemecolor`（本地化键 + color ⇒ 保留成色盘）
                assert_eq!(
                    (editable, heading, media, blank, loc),
                    (6, 4, 7, 3, 0),
                    "3704273480 的分类变了：{:?}",
                    props.iter().map(|p| (&p.key, &p.form)).collect::<Vec<_>>()
                );
                let mut eds: Vec<&str> = props
                    .iter()
                    .filter(|p| matches!(p.form, PropForm::Editable(_)))
                    .map(|p| p.key.as_str())
                    .collect();
                eds.sort_unstable();
                let mut want =
                    vec!["newproperty", "newproperty1", "newproperty2", "schemecolor", "text", "text5"];
                want.sort_unstable();
                assert_eq!(eds, want);
                // `key="text"` 是 type=textinput + value="mouse pad" ⇒ 必须**保留可编辑**
                let text_prop = props.iter().find(|p| p.key == "text").unwrap();
                assert_eq!(text_prop.form, PropForm::Editable(PropKind::Text));
            }
            checked += 1;
        }
        if checked == 0 {
            eprintln!("本机没装这两条工坊内容，跳过");
        }
    }

    /// 真数据回归：**为什么 `nsfwversion`/`sfwversion`/`xrayversion`/`xrayglasses` 没做成单选组**。
    /// 用户指出崩坏3 `2564205376` 的这四个 bool 「表现上像一组互斥选项」
    /// （渲染器侧实测：`xrayversion=true` + `sfwversion=true` 与纯 SFW **逐像素 0 差异** ——
    /// 是 scene.json 里 SFW 层画在 X-Ray 层**之上**把它盖住了），要求**按数据**找分组信号、
    /// 找到才渲染 radio。这个测试把「找信号」这件事本身钉住，判据**只用真 `project.json` 的字段**：
///
    /// 1. 这 5 条属性（4 bool + 1 slider）的 `condition` **逐字节相同** = 字面量 `"true"`（恒显示）
    /// ⇒ `condition` 既隔离不出那 4 个（把 slider 一起圈进来）、也没有互斥语义；
///
    /// 2. `index` / `order` 是 0,1,2,3,4 / 100..104 的**逐条递增序号**（slider 夹在 0 与 2 之间）
    /// ⇒ 不是「序号族」，切不出这四个；
///
    /// 3. 本机全部工坊工程的属性字段**全集**只有
    /// {order,text,type,value,fraction,max,min,precision,step,condition,index,options}
    /// **没有** group / radio / mutex / exclusive 之类组标记。
    /// ⇒ 找不到可靠信号 ⇒ **不做单选组、也不按属性名硬编码**（面板里仍是 4 个独立勾选框）。
    /// 反面也量了：把「condition 相同 ⇒ 一组」当规则用，会在 7 条工程上把 49 个互不相干的 bool
    /// 并成一组（`3299815325` 13 个、`2823687015` 9 个、`3571777169` 9 个、`2080534540` 6 个…）
    /// ⇒ 这条近似规则在真数据上就是错的，所以不能用。
    #[test]
    fn no_group_signal_so_author_bools_stay_independent() {
        // 本地库根目录（默认 `~/Templates`；"搬过家"的工坊内容照样找得到）
        let base = crate::wp::library::roots(&[])
            .into_iter()
            .next()
            .unwrap_or_default();

        let mut dirs: Vec<std::path::PathBuf> = std::fs::read_dir(&base)
            .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
            .unwrap_or_default();
        dirs.sort();

        let mut projects = 0usize;
        let mut fields: std::collections::BTreeSet<String> = Default::default();
        let mut groupish: Vec<String> = Vec::new();
        // 反面量：「condition 相同 ⇒ 一组」会造出的假分组
        let mut false_groups = 0usize;
        let mut false_group_props = 0usize;
        for dir in &dirs {
            let Ok(js) = std::fs::read_to_string(dir.join("project.json")) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&js) else {
                continue;
            };
            let Some(props) = v
                .get("general")
                .and_then(|g| g.get("properties"))
                .and_then(|p| p.as_object())
            else {
                continue;
            };
            projects += 1;
            let mut by_cond: std::collections::BTreeMap<String, Vec<String>> = Default::default();
            for (k, p) in props {
                let Some(o) = p.as_object() else { continue };
                for f in o.keys() {
                    let lf = f.to_lowercase();
                    if ["group", "radio", "mutex", "exclus", "parent", "section", "variant"]
                        .iter()
                        .any(|t| lf.contains(t))
                    {
                        groupish.push(format!(
                            "{}: {k}.{f}",
                            dir.file_name().unwrap_or_default().to_string_lossy()
                        ));
                    }
                    fields.insert(lf);
                }
                if o.get("type").and_then(|t| t.as_str()) == Some("bool") {
                    let cond = o
                        .get("condition")
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "<缺席>".into());
                    by_cond.entry(cond).or_default().push(k.clone());
                }
            }
            for ks in by_cond.values() {
                if ks.len() >= 2 {
                    false_groups += 1;
                    false_group_props += ks.len();
                }
            }
        }
        eprintln!(
            "[属性分组信号] {projects} 条工程；属性字段全集 = {:?}；组标记类字段命中 {} 个",
            fields.iter().cloned().collect::<Vec<_>>(),
            groupish.len()
        );
        eprintln!(
            "[属性分组信号] 反面量：「condition 相同 ⇒ 一组」⇒ {false_groups} 个假分组 / \
             {false_group_props} 个互不相干的 bool 被错并（真数据上会错，所以不做单选组）"
        );
        if projects == 0 {
            eprintln!("本机没有工坊工程数据，跳过（这条回归测试靠本机真数据）");
            return;
        }
        assert!(
            groupish.is_empty(),
            "项目文件里出现了组标记类字段 ⇒ 得重新评估「按数据渲染单选组」：{groupish:?}"
        );
        assert!(
            false_groups >= 5 && false_group_props >= 30,
            "假分组数 {false_groups} 组 / {false_group_props} 条 与记录的不符（数据变了？）"
        );

        // `2564205376`：那四个 bool 今天就是四个独立勾选框（没有任何合并），且五条 `condition` 全同
        let pj = base.join("2564205376/project.json");
        let Ok(js) = std::fs::read_to_string(&pj) else {
            eprintln!("跳过 2564205376：本机没有它的 project.json");
            return;
        };
        let v: serde_json::Value = serde_json::from_str(&js).unwrap();
        let raw = v
            .get("general")
            .and_then(|g| g.get("properties"))
            .expect("2564205376 一定有 general.properties");
        let props = parse_props(&serde_json::to_string(raw).unwrap());
        let editable: Vec<(&str, PropKind)> = props
            .iter()
            .filter(|p| matches!(p.form, PropForm::Editable(_)))
            .map(|p| (p.key.as_str(), p.kind))
            .collect();
        assert_eq!(
            editable,
            vec![
                // `schemecolor`（本地化键 + color）与 `xrayversion` 都是 `index 0`，
                // 稳定排序下按文件里的顺序 ⇒ 它在最前。以前它被整行跳过，所以那时从 xrayversion 起。
                ("schemecolor", PropKind::Color),
                ("xrayversion", PropKind::Bool),
                ("xraysize", PropKind::Slider),
                ("nsfwversion", PropKind::Bool),
                ("sfwversion", PropKind::Bool),
                ("xrayglasses", PropKind::Bool),
            ],
            "顺序按 index；四个 bool 各自出行、没有被合并成一组，slider 夹在中间"
        );
        for k in [
            "xrayversion",
            "xraysize",
            "nsfwversion",
            "sfwversion",
            "xrayglasses",
        ] {
            assert_eq!(
                raw.get(k).and_then(|p| p.get("condition")).and_then(|c| c.as_str()),
                Some("true"),
                "{k} 的 condition 变了 ⇒ 这条壁纸的「没有分组信号」结论要重新评估"
            );
        }
    }

    // ── ③ 作者属性的「依赖显示」条件（`condition`）────────────────────────────
    // 判据全部来自数据：`<兄弟属性key>.value <op> <单个字面量>`；
    // 缺席 / 空串 / 解析不了 ⇒ **恒显示**（fail-open，理由见 `parse_condition` 上面那段注释）。

    #[test]
    fn parses_only_single_operand_conditions() {
        // 形态①：缺席 / 空串 ⇒ 恒显示
        assert_eq!(parse_condition(""), PropCond::Always);
        assert_eq!(parse_condition("   "), PropCond::Always);
        // 字面量（本机 `2564205376` 五条都是 "true"）
        assert_eq!(parse_condition("true"), PropCond::Const(true));
        assert_eq!(parse_condition(" FALSE "), PropCond::Const(false));
        // 形态②：指向兄弟属性 —— 本机实测那两例，逐字节照抄
        assert_eq!(
            parse_condition("clock.value == true"),
            PropCond::Cmp {
                key: "clock".into(),
                op: CmpOp::Eq,
                rhs: serde_json::json!(true)
            }
        );
        assert_eq!(
            parse_condition("newproperty.value == 2"),
            PropCond::Cmp {
                key: "newproperty".into(),
                op: CmpOp::Eq,
                rhs: serde_json::json!(2)
            }
        );
        // != / === / !== / 字符串字面量 / 小数
        assert_eq!(
            parse_condition("mode.value != 3"),
            PropCond::Cmp {
                key: "mode".into(),
                op: CmpOp::Ne,
                rhs: serde_json::json!(3)
            }
        );
        assert_eq!(
            parse_condition("mode.value !== 'off'"),
            PropCond::Cmp {
                key: "mode".into(),
                op: CmpOp::Ne,
                rhs: serde_json::json!("off")
            }
        );
        assert_eq!(
            parse_condition("size.value === 0.5"),
            PropCond::Cmp {
                key: "size".into(),
                op: CmpOp::Eq,
                rhs: serde_json::json!(0.5)
            }
        );
        // 形态③ + 各种「不猜」的形状 ⇒ 恒显示
        for s in [
            "Water Flow",                    // 2481886649 的 combo 手填串
            "!!!",
            "a.value",                       // 没有运算符
            "wat == 1",                      // 左边不是 <key>.value
            "props.a.value == 1",            // 键里带点 ⇒ 不认
            "a.b.value == 1",
            "a.value ==",                    // 右边空
            "a.value = 1",                   // 单等号（不是比较）
            "a.value == 1 && b.value == 2",  // 复合表达式：右操作数带空白 ⇒ 不猜
            "a.value == ",                   
            "()",                            
        ] {
            assert_eq!(parse_condition(s), PropCond::Always, "{s:?} 不该被当成条件");
        }
    }

    /// 条件求值：拿**兄弟属性的当前值**（覆盖表优先）比字面量；类型不严格（`"2"` ↔ `2`）。
    #[test]
    fn condition_decides_row_visibility() {
        let p = parse_props(
            r#"{
            "newproperty": {"index": 0, "type": "combo", "value": "1", "text": "主题",
                            "options": [{"label": "1", "value": "1"}, {"label": "2", "value": "2"}]},
            "text":   {"index": 1, "type": "textinput", "value": "mouse pad", "text": "Text",
                       "condition": "newproperty.value == 2"},
            "clock":  {"index": 2, "type": "bool", "value": true, "text": "Clock"},
            "_12hmode": {"index": 3, "type": "bool", "value": false, "text": "12h mode",
                         "condition": "clock.value == true"},
            "ghost_dep": {"index": 4, "type": "bool", "value": true, "text": "X",
                          "condition": "ghost.value == true"},
            "weird_dep": {"index": 5, "type": "bool", "value": true, "text": "Y",
                          "condition": "Water Flow"},
            "plain":  {"index": 6, "type": "bool", "value": true, "text": "Z"}
        }"#,
        );
        let lookup = |k: &str, edits: &HashMap<String, serde_json::Value>| {
            edits
                .get(k)
                .cloned()
                .or_else(|| p.iter().find(|x| x.key == k).map(|x| x.value.clone()))
        };
        let holds = |key: &str, edits: &HashMap<String, serde_json::Value>| {
            let prop = p.iter().find(|x| x.key == key).unwrap();
            cond_holds(&prop.cond, &|k: &str| lookup(k, edits))
        };
        let edits = |k: &str, v: serde_json::Value| {
            let mut m: HashMap<String, serde_json::Value> = Default::default();
            m.insert(k.to_string(), v);
            m
        };
        let none: HashMap<String, serde_json::Value> = Default::default();

        // 默认（newproperty 的 schema 值是**字符串** "1"）⇒ 条件不成立 ⇒ 不显示
        assert!(!holds("text", &none));
        // 下拉选「2」：写回覆盖表的可能是字符串，也可能是数字（combo_value_json）⇒ 两种都算成立
        assert!(holds("text", &edits("newproperty", serde_json::json!("2"))));
        assert!(holds("text", &edits("newproperty", serde_json::json!(2.0))));
        assert!(holds("text", &edits("newproperty", serde_json::json!(2))));
        // 用户之前存下的数字 1.0（本机磁盘上就是它）⇒ 仍然不成立
        assert!(!holds("text", &edits("newproperty", serde_json::json!(1.0))));
        assert!(!holds("text", &edits("newproperty", serde_json::json!("1"))));

        // `clock.value == true`：默认 true ⇒ 显示；关掉 ⇒ 不显示
        assert!(holds("_12hmode", &none));
        assert!(!holds("_12hmode", &edits("clock", serde_json::json!(false))));

        // 引用不到的键 / 解析不了的串 ⇒ 恒显示（fail-open）
        assert!(holds("ghost_dep", &none));
        assert!(holds("weird_dep", &none));
        // 没有条件的行永远显示
        assert!(holds("plain", &none));
    }

    /// 真数据回归：本机装的 `3704273480` / `2823687015`，逐条钉住「哪一行被条件挡住」。
    /// 面板行数因此改变（`3704273480` 的 9 行 → 6 行；起多一条 `schemecolor`
    /// 色盘 ⇒ 10 → 7，**两边都 +1，差仍为 3**），所以这条测试也是那两次改动的回归网。
    #[test]
    fn real_projects_gate_rows_by_condition() {
        // 本地库根目录（默认 `~/Templates`；"搬过家"的工坊内容照样找得到）
        let base = crate::wp::library::roots(&[])
            .into_iter()
            .next()
            .unwrap_or_default();
        fn props_of(base: &std::path::Path, id: &str) -> Option<Vec<Prop>> {
            let js = std::fs::read_to_string(base.join(id).join("project.json")).ok()?;
            let v: serde_json::Value = serde_json::from_str(&js).ok()?;
            let raw = v.get("general")?.get("properties")?;
            Some(parse_props(&serde_json::to_string(raw).ok()?))
        }
        // 面板会为这些形态出行（与 `view_detail` 的过滤一模一样）
        let row_worthy = |p: &Prop| {
            matches!(
                p.form,
                PropForm::Editable(_) | PropForm::Heading(_) | PropForm::ReadOnlyNote(_)
            )
        };
        let visible_rows = |props: &[Prop], edits: &HashMap<String, serde_json::Value>| {
            props
                .iter()
                .filter(|p| row_worthy(p))
                .filter(|p| {
                    cond_holds(&p.cond, &|k: &str| {
                        edits
                            .get(k)
                            .cloned()
                            .or_else(|| props.iter().find(|x| x.key == k).map(|x| x.value.clone()))
                    })
                })
                .count()
        };
        let mut checked = 0;

        // ① 3704273480（麻匪 夏日影）：combo `newproperty` 默认值**字符串** "1"
        // ⇒ `text` / `text5` / `newproperty1` 三行（`condition = newproperty.value == 2`）不显示
        if let Some(props) = props_of(&base, "3704273480") {
            let np = props.iter().find(|p| p.key == "newproperty").unwrap();
            assert_eq!(
                np.value,
                serde_json::json!("1"),
                "newproperty 的默认值类型变了 ⇒ 「字符串/数字都要能对上」这条要重看"
            );
            let gated = ["text", "text5", "newproperty1"];
            let none: HashMap<String, serde_json::Value> = Default::default();
            let n_off = visible_rows(&props, &none);
            for k in gated {
                let p = props.iter().find(|x| x.key == k).unwrap();
                assert_eq!(
                    p.cond,
                    PropCond::Cmp {
                        key: "newproperty".into(),
                        op: CmpOp::Eq,
                        rhs: serde_json::json!(2)
                    },
                    "{k} 的 condition 变了"
                );
                let v = |kk: &str| -> Option<serde_json::Value> {
                    props.iter().find(|x| x.key == kk).map(|x| x.value.clone())
                };
                assert!(!cond_holds(&p.cond, &v), "{k} 在 newproperty=1 时不该显示");
            }
            // 下拉切到「2」（用户点过之后写回的是数字）⇒ 三行都回来
            let mut two: HashMap<String, serde_json::Value> = Default::default();
            two.insert("newproperty".to_string(), serde_json::json!(2.0));
            let n_on = visible_rows(&props, &two);
            assert_eq!(
                n_on - n_off,
                3,
                "切到 2 应该正好多出三条（实际 {n_off} → {n_on}）"
            );
            eprintln!(
                "[依赖显示] 3704273480: newproperty=1 ⇒ 作者行 {n_off} 条；newproperty=2 ⇒ {n_on} 条"
            );
            checked += 1;
        } else {
            eprintln!("跳过 3704273480：本机没有它的 project.json");
        }

        // ② 2823687015：`_12hmode` 依赖 `clock.value == true`（clock 默认 true ⇒ 显示；关掉 ⇒ 不显示）
        if let Some(props) = props_of(&base, "2823687015") {
            let p = props.iter().find(|x| x.key == "_12hmode").unwrap();
            assert_eq!(
                p.cond,
                PropCond::Cmp {
                    key: "clock".into(),
                    op: CmpOp::Eq,
                    rhs: serde_json::json!(true)
                }
            );
            let clock_on = |v: bool| {
                let props = &props;
                move |k: &str| -> Option<serde_json::Value> {
                    if k == "clock" {
                        return Some(serde_json::json!(v));
                    }
                    props.iter().find(|x| x.key == k).map(|x| x.value.clone())
                }
            };
            assert!(
                cond_holds(&p.cond, &clock_on(true)),
                "clock=true 时 12h mode 该显示"
            );
            assert!(
                !cond_holds(&p.cond, &clock_on(false)),
                "clock=false 时 12h mode 不该显示"
            );
            let none: HashMap<String, serde_json::Value> = Default::default();
            eprintln!(
                "[依赖显示] 2823687015: clock=true ⇒ 作者行 {} 条（含 12h mode）",
                visible_rows(&props, &none)
            );
            checked += 1;
        } else {
            eprintln!("跳过 2823687015：本机没有它的 project.json");
        }

        if checked == 0 {
            eprintln!("本机没装这两条工坊内容，跳过（这条回归测试靠本机真数据）");
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MenuAction {
    About,
}

impl menu::action::MenuAction for MenuAction {
    type Message = Message;

    fn message(&self) -> Self::Message {
        match self {
            MenuAction::About => Message::ToggleContextPage(ContextPage::About),
        }
    }
}
