// SPDX-License-Identifier: GPL-3.0-only
//
//! Wallpaper Engine 创意工坊「发现页」的后端：搜索 + 元数据 + 缩略图。
//!
//! **全程零登录**：
//!   * 搜索/翻页 → 抓公开的 workshop browse 页面（拿 ID）
//!   * 元数据   → 公开接口 `ISteamRemoteStorage/GetPublishedFileDetails`（无需 key）
//!   * 缩略图   → 直接下 `preview_url`
//!
//! 订阅/下载**不在这里做**（2026-10-21 起程序里也**没有任何登录/订阅入口**了）：
//! 需要账号的动作请用详情面板的 [工坊页] 在浏览器/Steam 客户端里完成。
//! 本程序只负责把「未下载的内容」摆出来（用 external_id 对照本地库判断已下载）。
//!
//! # 加载性能（这是本模块的重点）
//!
//! 实测（2026-10-12，本机家宽，30 条/页）三个阶段：
//!
//! | 阶段 | 耗时 | 说明 |
//! |---|---|---|
//! | 搜索页 browse | 1.75s | 一次 GET，无法再快 |
//! | 详情 30 条 | 1.9s | **一次** POST 带 30 个 id（串行发 30 次是最常见的错误做法） |
//! | 缩略图 1 张 | 1~4s | 波动极大，时间几乎全花在 CDN 的 TCP+TLS 建连上 |
//!
//! ⇒ 修的**唯一**要义是「24 张图别再串行下」：串行 26~95s（网络差时更久，用户体感"一两分钟"），
//! 并发 12 张实测 24 张 **2.65s**（并发 8 → 5.28s；并发 16 → 7.29s，CDN 侧开始限速）。
//!
//! 因此本模块：
//!
//! 1. **有上限地并发**：缩略图 12 路、详情按 30 条/批最多 3 批并行（`crate::config::WS_*`），
//!    工作队列动态派发 —— 谁先下完谁接着下下一张，不被最慢的那张拖住。
//! 2. **每条请求都有超时**：建连 4s / 响应头 6s / 响应体 8s / 总 12s；
//!    另外每个阶段有 wall-clock 预算，超预算只跳过不再重试。
//! 3. **失败隔离 + 一次重试**：单条失败（超时/断连/5xx/429）跳过并计数，绝不让整批失败；
//!    4xx 等永久错误不重试。失败条数通过 [`Stats`] 回给界面。
//! 4. **落盘缓存**（缩略图 30 天 / 详情 24h / 搜索页 5min）：二次进入 0 网络。
//!    缓存写盘走「临时文件 + rename」，半截文件不会被当成命中。
//!
//! # 线程模型（不许阻塞 UI）
//!
//! 本模块全部是**阻塞式** HTTP，**必须**在后台线程调用：
//!
//! * 界面一次性读取：`tokio::task::spawn_blocking(|| workshop::search(..))`（app.rs 现用这条路）；
//! * 渐进式加载（先出条目、再补缩略图）：[`search_entries`] + [`load_thumbs`]，
//!   或直接 [`prefetch_thumbs`]（自己起线程，立刻返回）。
//!
//! 任何情况下 UI 线程只做 `Handle::from_bytes` 这种 O(1) 的包装，不做网络、不做解码。
//!
//! app.rs 已接上渐进式加载（`Message::DiscoverResult` → `Task::stream(discover_thumb_stream(..))`）：
//!
//! ```ignore
//! // Message::DiscoverRunSearch:
//! //   1) 阶段一：search_entries()          → 拿到条目就 DiscoverResult（缓存命中时几十毫秒出内容）
//! //   2) 阶段二：discover_thumb_stream()   → 后台并发下缩略图，**每下到一张就发一条消息**
//! //      （Message::DiscoverThumb）⇒ 界面逐张补图；结束后一条 Message::DiscoverThumbsDone
//! ```

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::config as cfg;

const APPID: u32 = 431960;
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36";
const BROWSE: &str = "https://steamcommunity.com/workshop/browse/";
const DETAILS: &str = "https://api.steampowered.com/ISteamRemoteStorage/GetPublishedFileDetails/v1/";

/// 两个接口基址的**测试钩子**：`WPHUB_WORKSHOP_BROWSE_BASE` / `WPHUB_WORKSHOP_DETAILS_URL`
/// 可把它们指向别处（默认就是上面两个真地址）。
///
/// 为什么要它：本机拿不出"连接接得上、但一直不回"的服务端，而"无网不卡死"这条要求
/// 恰恰要在**最坏情况**下量（DNS 秒失败不算，黑洞才算）。有了它就能把两个地址指到
/// 一个只 accept 不回包的本地端口上，端到端量出"超时 + 重试 + 落错误提示"的真实耗时。
/// **只影响地址，不改任何超时/重试参数。**
fn endpoint(default: &'static str, env_key: &str) -> String {
    std::env::var(env_key)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// 搜索页基址（可被 `WPHUB_WORKSHOP_BROWSE_BASE` 覆盖）
fn browse_base() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| endpoint(BROWSE, "WPHUB_WORKSHOP_BROWSE_BASE"))
        .as_str()
}

/// 详情接口地址（可被 `WPHUB_WORKSHOP_DETAILS_URL` 覆盖）
fn details_url() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| endpoint(DETAILS, "WPHUB_WORKSHOP_DETAILS_URL"))
        .as_str()
}

/// 详情响应体上限（30~60 条的 JSON，带描述也就几百 KB）
const DETAILS_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// 搜索页 HTML 上限
const BROWSE_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// 排序方式（对应 browse 页的 browsesort 参数）
pub const SORTS: [(&str, &str); 4] = [
    ("trend", "趋势"),
    ("mostrecent", "最新"),
    ("toprated", "评分最高"),
    ("mostsubscribed", "订阅最多"),
];

// ── 筛选：与 Win 版 WE 的筛选面板**逐项一致**（黑名单取反）─────────────────────
//
// 词表与语义**不是猜的**：来源 `artifacts/we-ui-filters-20261022/`（Win 版 WE 的明文
// AngularJS 面板 `ui/dist/scripts/scripts.js` + `bin/wallpaperui.exe` 的 `.rdata` 指针表：
// 6 组、真实 tag 值、默认值、查询语义全在里面，单测 `filter_matches_reversed_spec`
// 直接读那份 JSON 逐组对比）。要点：
//
// * **勾选 = 不排除**（不是"必须有"）；**未勾的**才进 `excludedtags[]`
// * 组内 **OR**、组间 **AND** 天然成立（同一个组只有未勾的进排除表 = OR；
//   多组各自的排除表叠在一条查询里 = AND）
// * 某组**全不勾**时追加字面量 `"Unspecified"`（WE 的保底）—— 否则"全不选"
//   在 Steam 那边等于"不筛"，会返回全部而不是空
// * `requiredtags[]` 只由两处写入：`Approved`（"仅显示"组）、三态 utility 标签设成"仅"
// * 因此**一页只发一次请求**：上一版用 `requiredtags[]` + "多组合并集"模拟组内 OR，
//   这次逆向推翻了 —— `excludedtags[]` 一次就能表达，不需要叉乘。

/// 某个组的选项里"空组保底"用的字面量（WE 写死的字符串，不是真标签）。
pub const UNSPECIFIED_FALLBACK: &str = "Unspecified";

/// 一个可勾选项。`tag` 是 Steam 标签原文（**一字不改**：多一个空格 Steam 就不认），
/// `label` 只管上屏。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opt {
    pub tag: &'static str,
    pub label: &'static str,
    /// WE **工坊源**的默认勾选态（`filters.json` → `defaults.workshop`）
    pub default: bool,
}

impl Opt {
    const fn on(tag: &'static str, label: &'static str) -> Self {
        Self {
            tag,
            label,
            default: true,
        }
    }
    const fn off(tag: &'static str, label: &'static str) -> Self {
        Self {
            tag,
            label,
            default: false,
        }
    }
}

/// 组内的一小节：分辨率那 5 个宽高比子组、类型组里"壁纸/预设"那道分割线、
/// 分辨率末尾两个不属于任何子组的尾巴。
#[derive(Debug, Clone, Copy)]
pub struct Section {
    pub key: &'static str,
    /// 小标题（"" = 本组的主小节，不印标题）
    pub title: &'static str,
    /// 带"所有 / 无"两个按钮？
    pub all_none: bool,
    /// "所有/无"要跳过的 tag（"" = 都管）。标签组跳过 `Unspecified`：
    /// WE 在工坊源里把它强制不勾（`filters.json` → `all_none_note`）。
    pub all_none_skip: &'static str,
    /// 这一节是否**开始一个新的 WE 模型对象**（angular 的 typetags / categorytags /
    /// ratingtags / resolutiontags / tags 各是一个对象）。空组保底按这个粒度算 ——
    /// 分辨率的 5 个子组**共用**同一个 `resolutiontags`，所以只有第一个是 `true`。
    pub new_unit: bool,
    pub opts: &'static [Opt],
}

/// 一个筛选组（面板里的一块）。顺序 = WE 面板顺序（`filters.json` → `ui.group_order`）。
#[derive(Debug, Clone, Copy)]
pub struct Group {
    /// 持久化用的键（`type:Application=0` 的前半段）。**改名会让老配置失效**，别动。
    pub key: &'static str,
    pub title: &'static str,
    /// 默认收起？（类型 / 年龄分级 / 分辨率收起；来源 / 标签展开 ——
    /// WE 里这两类组的 `ng-show` / `ng-hide` 约定相反，别"统一"掉。）
    pub collapsed: bool,
    /// 组级"所有 / 无"（只有"标签"组有；分辨率是每个子组各一对）
    pub all_none: bool,
    pub sections: &'static [Section],
    /// 界面上那行小字（"" = 不印）
    pub note: &'static str,
    /// 会不会进查询参数。**来源组是纯本地过滤**（WE 只对"已安装"项生效），浏览页不发。
    pub query_backed: bool,
}

pub const G_TYPE: &str = "type";
pub const G_RATING: &str = "rating";
pub const G_RESOLUTION: &str = "resolution";
pub const G_SOURCE: &str = "source";
pub const G_TAGS: &str = "tags";

/// 四个"黑名单"组 + 一个纯本地组。**没有**"仅显示"组：它不是黑名单
/// （勾上 = 进 `requiredtags[]` / 额外排除），单独用 [`SHOW_ONLY`] 表示。
pub const GROUPS: [Group; 5] = [
    Group {
        key: G_TYPE,
        title: "类型",
        collapsed: true,
        all_none: false,
        query_backed: true,
        note: "勾选 = 不排除。Application 在 WE 的工坊浏览页被隐藏、默认不勾",
        sections: &[
            Section {
                key: "type",
                title: "",
                all_none: false,
                all_none_skip: "",
                new_unit: true,
                opts: &[
                    Opt::on("Scene", "场景"),
                    Opt::on("Video", "视频"),
                    Opt::on("Web", "网页"),
                    Opt::off("Application", "应用程序"),
                ],
            },
            Section {
                key: "category",
                title: "分类",
                all_none: false,
                all_none_skip: "",
                new_unit: true,
                opts: &[Opt::on("Wallpaper", "壁纸"), Opt::on("Preset", "预设")],
            },
        ],
    },
    Group {
        key: G_RATING,
        title: "年龄分级",
        collapsed: true,
        all_none: false,
        query_backed: true,
        note: "默认只勾「大众级」⇒ PG-13 / R-18 默认被排除（= WE 默认）",
        sections: &[Section {
            key: "rating",
            title: "",
            all_none: false,
            all_none_skip: "",
            new_unit: true,
            opts: &[
                Opt::on("Everyone", "大众级 G"),
                Opt::off("Questionable", "家长指导 PG-13"),
                Opt::off("Mature", "成人 R-18"),
            ],
        }],
    },
    Group {
        key: G_RESOLUTION,
        title: "分辨率",
        collapsed: true,
        all_none: false,
        query_backed: true,
        note: "5 个宽高比子组各带「所有/无」；末尾两项不属于任何子组（默认全勾）",
        sections: &[
            Section {
                key: "widescreen",
                title: "宽屏",
                all_none: true,
                all_none_skip: "",
                new_unit: true,
                opts: &[
                    Opt::on("Standard Definition", "标清"),
                    Opt::on("1280 x 720", "1280 x 720"),
                    Opt::on("1366 x 768", "1366 x 768"),
                    Opt::on("1920 x 1080", "1920 x 1080 - 全高清"),
                    Opt::on("2560 x 1440", "2560 x 1440"),
                    Opt::on("3840 x 2160", "3840 x 2160 - 4K"),
                ],
            },
            Section {
                key: "ultrawide",
                title: "超宽屏",
                all_none: true,
                all_none_skip: "",
                new_unit: false,
                opts: &[
                    Opt::on("Ultrawide Standard Definition", "超宽（标准）"),
                    Opt::on("Ultrawide 2560 x 1080", "2560 x 1080"),
                    Opt::on("Ultrawide 3440 x 1440", "3440 x 1440"),
                ],
            },
            Section {
                key: "dual",
                title: "双显示器",
                all_none: true,
                all_none_skip: "",
                new_unit: false,
                opts: &[
                    Opt::on("Dual Standard Definition", "双显示器（标准）"),
                    Opt::on("Dual 3840 x 1080", "3840 x 1080"),
                    Opt::on("Dual 5120 x 1440", "5120 x 1440"),
                    Opt::on("Dual 7680 x 2160", "7680 x 2160"),
                ],
            },
            Section {
                key: "triple",
                title: "三显示器",
                all_none: true,
                all_none_skip: "",
                new_unit: false,
                opts: &[
                    Opt::on("Triple Standard Definition", "三显示器（标准）"),
                    Opt::on("Triple 4096 x 768", "4096 x 768"),
                    Opt::on("Triple 5760 x 1080", "5760 x 1080"),
                    Opt::on("Triple 7680 x 1440", "7680 x 1440"),
                    Opt::on("Triple 11520 x 2160", "11520 x 2160"),
                ],
            },
            Section {
                key: "portrait",
                title: "纵向监视器/手机",
                all_none: true,
                all_none_skip: "",
                new_unit: false,
                opts: &[
                    Opt::on("Portrait Standard Definition", "纵向（标准）"),
                    Opt::on("Portrait 720 x 1280", "720 x 1280"),
                    Opt::on("Portrait 1080 x 1920", "1080 x 1920"),
                    Opt::on("Portrait 1440 x 2560", "1440 x 2560"),
                    Opt::on("Portrait 2160 x 3840", "2160 x 3840"),
                ],
            },
            Section {
                key: "tail",
                title: "",
                all_none: false,
                all_none_skip: "",
                new_unit: false,
                opts: &[
                    Opt::on("Other resolution", "其他分辨率"),
                    Opt::on("Dynamic resolution", "动态分辨率"),
                ],
            },
        ],
    },
    Group {
        key: G_SOURCE,
        title: "来源",
        collapsed: false,
        all_none: false,
        // WE 里这一组用 `ng-hide="filterHideSource"`（变量为真 = 收起），初始 undefined
        // ⇒ **展开**；而且只在"已安装"源出现。
        query_backed: false,
        note: "只对「已安装」项有意义（WE：工坊浏览页整组隐藏）⇒ 本页不发查询参数",
        sections: &[Section {
            key: "source",
            title: "",
            all_none: false,
            all_none_skip: "",
            new_unit: true,
            opts: &[
                Opt::on("Official", "官方"),
                Opt::on("Workshop", "创意工坊"),
                Opt::on("Local", "我的壁纸"),
            ],
        }],
    },
    Group {
        key: G_TAGS,
        title: "标签",
        collapsed: false,
        all_none: true,
        query_backed: true,
        note: "25 个真实风格标签（`Unspecified` 排在末尾，WE 也是这么挪的）",
        sections: &[Section {
            key: "tags",
            title: "",
            all_none: true,
            all_none_skip: "Unspecified",
            new_unit: true,
            opts: &[
                Opt::on("Abstract", "抽象"),
                Opt::on("Animal", "动物"),
                Opt::on("Anime", "动漫"),
                Opt::on("Cartoon", "卡通"),
                Opt::on("CGI", "CGI"),
                Opt::on("Cyberpunk", "网络朋克"),
                Opt::on("Fantasy", "幻想"),
                Opt::on("Game", "游戏"),
                Opt::on("Girls", "女性"),
                Opt::on("Guys", "男性"),
                Opt::on("Landscape", "风景"),
                Opt::on("Medieval", "中世纪"),
                Opt::on("Memes", "网红事物"),
                Opt::on("MMD", "MMD"),
                Opt::on("Music", "音乐"),
                Opt::on("Nature", "自然"),
                Opt::on("Pixel art", "像素艺术"),
                Opt::on("Relaxing", "放松"),
                Opt::on("Retro", "复古"),
                Opt::on("Sci-Fi", "科幻"),
                Opt::on("Sports", "运动"),
                Opt::on("Technology", "科技"),
                Opt::on("Television", "电视节目"),
                Opt::on("Vehicle", "汽车"),
                // C++ 的数组以它开头，scripts.js 把它挪到末尾再渲染 ⇒ 界面顺序照抄
                Opt::off("Unspecified", "未指定样式"),
            ],
        }],
    },
];

/// 「仅显示」组（WE 里唯一**不可折叠**的 `groupBox`）：勾上 ≠ 不排除，
/// 而是"必须带这个标签"（`requiredtags[]`）或"额外排除这几个"。
pub struct OnlyOpt {
    pub key: &'static str,
    pub label: &'static str,
}

pub const ONLY_APPROVED: &str = "approved";
pub const ONLY_MOBILE: &str = "mobile_compatible";

/// WE 的"仅显示"组里还有 **「我的收藏」**（纯本地、要登录态）和**两个 utility 标签**
/// （tag 由 C++ `setAvailableTags(arg2)` 运行时下发，逆向**没取到值**，
/// 见 `filters.json` → `confidence.utility_tags = unresolved`）——
/// 这两样这里都不渲染：前者我们没有登录态，后者连 tag 字符串都不知道，编不出来。
pub const SHOW_ONLY: [OnlyOpt; 2] = [
    // 勾上 ⇒ `requiredtags[]=Approved`
    OnlyOpt {
        key: ONLY_APPROVED,
        label: "广受好评",
    },
    // 勾上 ⇒ 额外排除 Application/Web（WE 还带 kvs，网页端没有这个参数）
    OnlyOpt {
        key: ONLY_MOBILE,
        label: "手机兼容",
    },
];

/// 某组的所有 `(tag, 显示名)`（校验配置/请求只允许真实标签）。
pub fn group_opts(key: &str) -> impl Iterator<Item = &'static Opt> {
    GROUPS
        .iter()
        .filter(move |g| g.key == key)
        .flat_map(|g| g.sections.iter())
        .flat_map(|s| s.opts.iter())
}

/// 某个 tag 在该组里是不是真实存在（配置里存了旧/错的 tag 时会被它挡掉）。
pub fn is_real_tag(key: &str, tag: &str) -> bool {
    group_opts(key).any(|o| o.tag == tag)
}

/// WE 的 `browserSettings.defaultfilterconfig`（默认
/// `{recommendedresolution:false, showotherresolution:true, Anime:null}`）。
///
/// 我们没抄 WE 那个 [⚙ 筛选器设置] 对话框，所以这里**恒为默认值**；结构留着是为了
/// "重置 = 重建默认 + 套这份配置"这条链路与 WE 一一对应（也能被单测覆盖）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultFilterConfig {
    /// `true` ⇒ 取消"推荐分辨率"整组。
    /// ⚠ WE 里"推荐"到底是哪一档**逆向没定下来**（`filters.json` 只写
    /// "unchecks the whole 'recommended' resolution set"，没给集合），所以这里**不猜**：
    /// 该位为 true 时一个字都不改，只打一行日志（默认 false ⇒ 不影响重置结果）。
    pub recommended_resolution: bool,
    /// `false` ⇒ 取消 `Other resolution`
    pub show_other_resolution: bool,
    /// `Some(false)` ⇒ 额外取消 Anime / MMD / Girls / Guys / Music（WE 的"不要动漫"）
    pub anime: Option<bool>,
}

impl Default for DefaultFilterConfig {
    fn default() -> Self {
        Self {
            recommended_resolution: false,
            show_other_resolution: true,
            anime: None,
        }
    }
}

impl DefaultFilterConfig {
    pub fn apply(&self, f: &mut Filter) {
        if self.anime == Some(false) {
            for tag in ["Anime", "MMD", "Girls", "Guys", "Music"] {
                f.set_checked(G_TAGS, tag, false);
            }
        }
        if !self.show_other_resolution {
            f.set_checked(G_RESOLUTION, "Other resolution", false);
        }
        if self.recommended_resolution {
            wtrace(
                "defaultfilterconfig.recommendedresolution=true：\"推荐分辨率\"集合逆向未定，本次不改分辨率",
            );
        }
    }
}

/// 一次查询要带的标签。两个表都**排好序、去重**（同一组只有一个结论，
/// 顺序只影响缓存键 ⇒ 排好序才能让同样的筛选命中同一份缓存）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub required: Vec<String>,
    pub excluded: Vec<String>,
}

/// 多选筛选状态。**与 WE 同构**：每个选项一个 bool，`true` = 勾选 = **不排除**。
///
/// 只存"未勾选项"这一半（其余按默认值补齐）：默认值是 `filters.json` 里的
/// `defaults.workshop`，所以 `Filter::default()` **就是 WE 的默认筛选**
/// （排除 Application、只留大众级、分辨率全勾、标签全勾除 Unspecified）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    /// 未勾选的项 `(组键, tag)`
    unchecked: BTreeSet<(&'static str, &'static str)>,
    /// 「仅显示」里勾上的项
    only: BTreeSet<&'static str>,
}

impl Default for Filter {
    fn default() -> Self {
        let mut unchecked = BTreeSet::new();
        for group in &GROUPS {
            for section in group.sections {
                for opt in section.opts {
                    if !opt.default {
                        unchecked.insert((group.key, opt.tag));
                    }
                }
            }
        }
        Self {
            unchecked,
            only: BTreeSet::new(),
        }
    }
}

impl Filter {
    /// 组键 + tag ⇒ `(组, 选项)`；不认识就是 None。
    fn find(group_key: &str, tag: &str) -> Option<(&'static Group, &'static Opt)> {
        GROUPS.iter().find(|g| g.key == group_key).and_then(|g| {
            g.sections
                .iter()
                .flat_map(|s| s.opts.iter())
                .find(|o| o.tag == tag)
                .map(|o| (g, o))
        })
    }

    /// 这一项**勾着**吗（= 不排除）。不认识的组/标签 ⇒ `true`：
    /// 宁可"什么都没排除"，也绝不因为一个手滑的字符串把结果排空。
    pub fn checked(&self, group_key: &str, tag: &str) -> bool {
        match Self::find(group_key, tag) {
            Some((g, opt)) => !self.unchecked.contains(&(g.key, opt.tag)),
            None => true,
        }
    }

    /// 勾/去勾一项。返回 **false = 组或标签不认识**（什么都没改）。
    pub fn set_checked(&mut self, group_key: &str, tag: &str, on: bool) -> bool {
        let Some((g, opt)) = Self::find(group_key, tag) else {
            return false;
        };
        if on {
            self.unchecked.remove(&(g.key, opt.tag));
        } else {
            self.unchecked.insert((g.key, opt.tag));
        }
        true
    }

    /// 点一下：返回**操作后**是否勾选。组/标签不认识 ⇒ `false` 且什么都不改
    /// （点了没反应，但绝不会发一个 Steam 不认的标签）。
    pub fn toggle(&mut self, group_key: &str, tag: &str) -> bool {
        let now = !self.checked(group_key, tag);
        if !self.set_checked(group_key, tag, now) {
            return false;
        }
        now
    }

    /// 某小节「所有 / 无」（`all` = 全勾）。`all_none_skip` 的项不动
    /// （标签组的 `Unspecified`：WE 在工坊源里强制不勾它）。
    pub fn set_section(&mut self, group_key: &str, section_key: &str, all: bool) {
        let Some(group) = GROUPS.iter().find(|g| g.key == group_key) else {
            return;
        };
        let Some(section) = group.sections.iter().find(|s| s.key == section_key) else {
            return;
        };
        for opt in section.opts {
            if !section.all_none_skip.is_empty() && opt.tag == section.all_none_skip {
                continue;
            }
            self.set_checked(group.key, opt.tag, all);
        }
    }

    pub fn only(&self, key: &str) -> bool {
        self.only.contains(key)
    }

    pub fn set_only(&mut self, key: &str, on: bool) -> bool {
        let Some(opt) = SHOW_ONLY.iter().find(|o| o.key == key) else {
            return false;
        };
        if on {
            self.only.insert(opt.key);
        } else {
            self.only.remove(opt.key);
        }
        true
    }

    /// 点一下「仅显示」里的一项，返回操作后的状态。
    pub fn toggle_only(&mut self, key: &str) -> bool {
        if !self.set_only(key, !self.only(key)) {
            return false;
        }
        self.only(key)
    }

    /// 重置为**默认值**（不是清空）＋套 `defaultfilterconfig`。
    /// WE 的"重置过滤器"就是这条路径（`callbackResetFilter()`）。
    pub fn reset(&mut self) {
        self.reset_with(&DefaultFilterConfig::default());
    }

    pub fn reset_with(&mut self, cfg: &DefaultFilterConfig) {
        *self = Self::default();
        cfg.apply(self);
    }

    /// 与默认值不同的**项数**（"（已筛选）"角标与日志用）。
    pub fn changed_count(&self) -> usize {
        let d = Self::default();
        let opts_changed = self
            .unchecked
            .symmetric_difference(&d.unchecked)
            .count();
        opts_changed + self.only.symmetric_difference(&d.only).count()
    }

    /// 某一组是否被改过（每组标题旁的"（已筛选）"角标；`show_only` 也认）。
    pub fn group_changed(&self, group_key: &str) -> bool {
        let d = Self::default();
        let mut changed = false;
        for group in GROUPS.iter().filter(|g| g.key == group_key) {
            for section in group.sections {
                for opt in section.opts {
                    let now = !self.unchecked.contains(&(group.key, opt.tag));
                    let def = !d.unchecked.contains(&(group.key, opt.tag));
                    changed |= now != def;
                }
            }
        }
        if group_key == "show_only" {
            changed |= !self.only.is_empty();
        }
        changed
    }

    /// 一行摘要（收起时/日志用）：只列**与默认不同**的组，形如
    /// `类型 4/5 · 标签 20/25 · 仅显示 广受好评`
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.only.is_empty() {
            let names: Vec<&str> = SHOW_ONLY
                .iter()
                .filter(|o| self.only(o.key))
                .map(|o| o.label)
                .collect();
            parts.push(format!("仅显示 {}", names.join("/")));
        }
        for group in &GROUPS {
            if !self.group_changed(group.key) {
                continue;
            }
            let total: usize = group.sections.iter().map(|s| s.opts.len()).sum();
            let on: usize = group
                .sections
                .iter()
                .flat_map(|s| s.opts.iter())
                .filter(|o| self.checked(group.key, o.tag))
                .count();
            parts.push(format!("{} {on}/{total}", group.title));
        }
        if parts.is_empty() {
            "WE 默认筛选".to_string()
        } else {
            parts.join(" · ")
        }
    }

    /// 构造查询的 `requiredtags[]` / `excludedtags[]`。
    pub fn query(&self) -> Query {
        let mut req: BTreeSet<String> = BTreeSet::new();
        let mut excl: BTreeSet<String> = BTreeSet::new();

        // 「仅显示」：Approved 走**白名单**侧；手机兼容走额外排除
        if self.only(ONLY_APPROVED) {
            req.insert("Approved".to_string());
        }
        if self.only(ONLY_MOBILE) {
            excl.insert("Application".to_string());
            excl.insert("Web".to_string());
        }

        for group in GROUPS.iter().filter(|g| g.query_backed) {
            // 空组保底的粒度 = WE 的"一个 angular 模型对象"（见 `Section::new_unit`）
            let mut unit_started = false;
            let mut unit_any_checked = false;
            for section in group.sections {
                if section.new_unit && unit_started {
                    if !unit_any_checked {
                        excl.insert(UNSPECIFIED_FALLBACK.to_string());
                    }
                    unit_any_checked = false;
                }
                unit_started = true;
                for opt in section.opts {
                    if self.unchecked.contains(&(group.key, opt.tag)) {
                        excl.insert(opt.tag.to_string());
                    } else {
                        unit_any_checked = true;
                    }
                }
            }
            if unit_started && !unit_any_checked {
                excl.insert(UNSPECIFIED_FALLBACK.to_string());
            }
        }

        Query {
            required: req.into_iter().collect(),
            excluded: excl.into_iter().collect(),
        }
    }

    /// 存进配置的形状：**只写与默认值不同的项**，一行一个 `组键:标签=1|0`（1 = 勾选）。
    /// 空表 = 就是默认（全新安装 / 点过"重置过滤器"都是这个状态）。
    pub fn to_lines(&self) -> Vec<String> {
        let d = Self::default();
        let mut out = Vec::new();
        for group in &GROUPS {
            for section in group.sections {
                for opt in section.opts {
                    let now = !self.unchecked.contains(&(group.key, opt.tag));
                    let def = !d.unchecked.contains(&(group.key, opt.tag));
                    if now != def {
                        out.push(format!("{}:{}={}", group.key, opt.tag, u8::from(now)));
                    }
                }
            }
        }
        for opt in &SHOW_ONLY {
            if self.only(opt.key) {
                out.push(format!("only:{}={}", opt.key, 1));
            }
        }
        out
    }

    /// 从配置读回来。**不认识的组键 / 不是真标签的行直接丢掉**（并打一行日志）：
    /// 词表变了（Steam 改标签、我们删了某组）时，老配置不该把筛选卡在
    /// "永远筛不到东西"的状态上。
    pub fn from_lines(lines: &[String]) -> Self {
        let mut filter = Self::default();
        for line in lines {
            let Some((head, flag)) = line.rsplit_once('=') else {
                // 上一版的形状是 `组键:标签`（无 `=`），而且语义是**反的**
                // （那时存的是"选中"，现在存的是"勾选/未勾"）⇒ 一律丢掉，从默认重建
                wtrace(&format!("筛选配置丢一行（没有 = 状态位）：{line:?}"));
                continue;
            };
            let on = match flag.trim() {
                "1" => true,
                "0" => false,
                other => {
                    wtrace(&format!("筛选配置丢一行（状态位不是 0/1）：{other:?}"));
                    continue;
                }
            };
            let Some((key, tag)) = head.split_once(':') else {
                wtrace(&format!("筛选配置丢一行（没有冒号）：{line:?}"));
                continue;
            };
            let (key, tag) = (key.trim(), tag.trim());
            let ok = if key == "only" {
                filter.set_only(tag, on)
            } else {
                filter.set_checked(key, tag, on)
            };
            if !ok {
                wtrace(&format!("筛选配置丢一行（组/标签不在词表里）：{line:?}"));
            }
        }
        filter
    }
}

#[derive(Debug, Clone)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub preview_url: String,
    pub size_bytes: u64,
    pub tags: Vec<String>,
    pub subscriptions: u64,
    /// 工坊物品说明（HTML 很重，界面暂不展示，先留着备查）
    #[allow(dead_code)]
    pub description: String,
}

/// 一次加载的**计数与耗时**：界面可以直接显示「30 条里跳过了 2 条」，
/// 排查"为什么慢"时也不用再猜（各阶段耗时都在里面）。
///
/// ⚠ 部分字段（`ids`/`items`/`thumbs`）目前只写不读：它们是为界面状态条和日志留的，
/// 渐进式加载接上后就会用到 —— 别因为"没人读"就删掉。
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub browse_ms: u64,
    pub details_ms: u64,
    pub thumbs_ms: u64,
    pub total_ms: u64,
    /// 搜索页拿到的**唯一** id 数
    pub ids: usize,
    /// 搜索页里 `filedetails/?id=` 链接的**出现次数**（含重复）。
    /// 实测：Steam 每个条目在页面里有**两处**链接（缩略图 + 标题）⇒ 30 条 = 60 次。
    /// 这个数比 `ids` 大是正常的，**不是**列表里出现了重复条目（`extract_ids` 已按 id 折叠）；
    /// 它只用来把"页面里的重复"与"上屏列表的重复"分清楚（日志/状态条都印它）。
    pub link_occurrences: usize,
    /// 条目层按 id 折叠掉的**重复条数**（`dedupe_by_id`；正常情况是 0）
    pub duplicates: usize,
    pub items: usize,
    pub thumbs: usize,
    /// 缓存命中的条数
    pub details_cached: usize,
    pub thumbs_cached: usize,
    /// 失败被跳过的条数（单条失败不影响整批）
    pub details_failed: usize,
    pub thumbs_failed: usize,
    /// 重试次数 / 其中因为超时
    pub retries: usize,
    pub timed_out: usize,
    /// 本次用的缩略图并发数
    pub concurrency: usize,
    /// 搜索页来自缓存（新／旧）
    pub browse_cached: bool,
    pub browse_stale: bool,
    /// 这一页**一共发了几个** browse 请求（筛选的多选组合各算一个；无筛选时 = 1）。
    /// 界面上要能看出"这次筛选打了 2 次网络"，也是防抖验收的计数来源。
    pub queries: usize,
    /// Steam 页面上报的**总条数**（`total_count`，工坊全站口径）。
    /// 筛选一变这个数就变 ⇒ 这就是"筛选真的生效了"最硬的证据；解析不到就是 None。
    /// 多组合时是各组合之和（含重叠），所以 status 里要说明。
    pub total_count: Option<u64>,
}

impl Stats {
    fn absorb(&mut self, c: Counters, thumbs: bool) {
        self.retries += c.retries;
        self.timed_out += c.timed_out;
        if thumbs {
            self.thumbs_cached += c.cached;
            self.thumbs_failed += c.failed;
        } else {
            self.details_failed += c.failed;
        }
    }
}

/// 一条请求（或一批）的局部计数；并发 worker 各记各的，最后合并回 [`Stats`]。
#[derive(Debug, Clone, Copy, Default)]
struct Counters {
    cached: usize,
    failed: usize,
    retries: usize,
    timed_out: usize,
}

#[derive(Debug, Clone, Default)]
pub struct SearchResult {
    pub items: Vec<Item>,
    /// 已下载的缩略图（id → 原始字节），交给 UI 转成图片句柄
    #[allow(dead_code)] // 一次性 search() 会填它；界面现在走渐进式（load_thumbs_streaming），不读这个字段
    pub thumbs: Vec<(String, Vec<u8>)>,
    /// 各阶段耗时/失败计数（界面状态条与日志用；app.rs 接上就会读它）
    #[allow(dead_code)]
    pub stats: Stats,
}

/// 搜索一页（条目 + 缩略图）。**阻塞式**（HTTP），调用方要放在 `spawn_blocking` 里。
///
/// ⚠ 一次性路径（等条目和图都齐了才返回）。界面现在走的是渐进式：
/// [`search_entries`]（先出条目）+ [`load_thumbs_streaming`]（逐张补图），见模块头。
/// 这个函数保留给"就要一次拿全"的调用方（脚本/测试），别删。
#[allow(dead_code)]
pub fn search(
    query: &str,
    page: u32,
    sort_key: &str,
    filter: &Filter,
    want_thumbs: usize,
) -> Result<SearchResult, String> {
    let t0 = Instant::now();
    housekeeping(&cache_root());
    let mut stats = Stats {
        concurrency: cfg::WS_THUMB_CONCURRENCY,
        ..Stats::default()
    };

    let ids = ids_for(query, page, sort_key, filter, &mut stats)?;
    if ids.is_empty() {
        stats.total_ms = ms_since(t0);
        wtrace(&format!("total {}（搜索页没有解析到条目）", describe_stats(&stats)));
        return Ok(SearchResult::default());
    }
    let items = finish_items(details_cached(&ids, &mut stats), &mut stats);
    let thumbs = thumbs_cached(&items, want_thumbs, &mut stats);
    stats.thumbs = thumbs.len();
    stats.total_ms = ms_since(t0);
    wtrace(&format!("total {}ms {}", stats.total_ms, describe_stats(&stats)));
    Ok(SearchResult {
        items,
        thumbs,
        stats,
    })
}

/// 阶段一：**只要条目**（搜索页 + 详情），不碰缩略图 —— 冷启动约 4s、缓存命中时几十毫秒出内容。
///
/// 配 [`load_thumbs`] / [`load_thumbs_streaming`] 用，才能做到"先出条目、再补图"。
/// app.rs 的发现页/创意工坊页走的就是这条路（详见模块头）。
pub fn search_entries(
    query: &str,
    page: u32,
    sort_key: &str,
    filter: &Filter,
) -> Result<SearchResult, String> {
    let t0 = Instant::now();
    housekeeping(&cache_root());
    let mut stats = Stats {
        concurrency: cfg::WS_THUMB_CONCURRENCY,
        ..Stats::default()
    };
    let ids = ids_for(query, page, sort_key, filter, &mut stats)?;
    let items = if ids.is_empty() {
        Vec::new()
    } else {
        // 上屏前**按 id 折叠**：同一 id 只留一条（判据/留谁见 `dedupe_by_id`）
        finish_items(details_cached(&ids, &mut stats), &mut stats)
    };
    stats.total_ms = ms_since(t0);
    Ok(SearchResult {
        items,
        thumbs: Vec::new(),
        stats,
    })
}

/// 阶段二：给这批条目取缩略图（缓存优先，缺的按 [`cfg::WS_THUMB_CONCURRENCY`] 并发下载）。
/// 缺图/失败都只跳过并计数，返回的是"能拿到的那部分"。
///
/// ⚠ 这是**一次性**接口：要等这一批全部下完才返回。要做"边下边显示"用
/// [`load_thumbs_streaming`]（发现页用的就是它）。
#[allow(dead_code)] // 一次性接口：渐进式路径（app.rs）用 load_thumbs_streaming，本函数保留给"就要一批全拿"的调用方
pub fn load_thumbs(items: &[Item], want: usize, stats: &mut Stats) -> Vec<(String, Vec<u8>)> {
    thumbs_cached(items, want, stats)
}

/// 阶段三（渐进式）：并发下载这批条目的缩略图，**每拿到一张就回调一次** `on_thumb(id, bytes)`。
///
/// 与 [`load_thumbs`] 的区别：后者要等这一批**全部**下完才返回（界面那一下只能整批刷新），
/// 本函数边下边回调 ⇒ 界面可以一张一张地补图（发现页用的就是它）。
/// 回调从工作线程调用、必须轻量且非阻塞；`want` 一般传 `items.len()`（全部都要）。
/// **阻塞式**：调用方要放在后台线程里（app.rs 用 `Task::stream` + 一个后台线程包住它）。
pub fn load_thumbs_streaming(
    on_thumb: &(dyn Fn(String, Vec<u8>) + Sync),
    items: &[Item],
    want: usize,
) -> Stats {
    let mut stats = Stats {
        concurrency: cfg::WS_THUMB_CONCURRENCY,
        ..Stats::default()
    };
    let _ = thumbs_cached_each(items, want, &mut stats, Some(on_thumb));
    stats
}

/// 后台预热：把缩略图灌进磁盘缓存，**立刻返回**（下载在线程里跑）。
/// 供"先出条目、稍后补图"用；也可在别的页面空闲时提前给发现页铺路。
///
/// 与 [`load_thumbs_streaming`] 的分工：本函数只灌缓存、不回传结果（界面要等下一次读缓存
/// 才知道）；发现页要"逐张上屏"用的是 [`load_thumbs_streaming`]。本入口留给
/// "别处空闲时提前铺路"（目前没有调用者）。
#[allow(dead_code)]
pub fn prefetch_thumbs(items: Vec<Item>, want: usize) {
    let _ = std::thread::Builder::new()
        .name("workshop-prefetch".into())
        .spawn(move || {
            let mut stats = Stats::default();
            let got = thumbs_cached(&items, want, &mut stats);
            wtrace(&format!(
                "预热缩略图完成：{}/{} 张（后台线程，UI 未阻塞）",
                got.len(),
                want.min(items.len())
            ));
        });
}

/// 一行中文摘要（日志/界面状态条都能直接用）。
pub fn describe_stats(s: &Stats) -> String {
    let mut text = format!(
        "总共 {}ms（搜索页 {}ms{} / 详情 {}ms 命中 {} 失败 {} / 缩略图 {}ms 命中 {} 失败 {}，并发 {}，重试 {}，超时 {} · 链接 {} 次 → 唯一 id {} · 同 id 折叠 {}）",
        s.total_ms,
        s.browse_ms,
        if s.browse_cached {
            if s.browse_stale { "·旧缓存" } else { "·缓存" }
        } else {
            ""
        },
        s.details_ms,
        s.details_cached,
        s.details_failed,
        s.thumbs_ms,
        s.thumbs_cached,
        s.thumbs_failed,
        s.concurrency,
        s.retries,
        s.timed_out,
        s.link_occurrences,
        s.ids,
        s.duplicates,
    );
    // 筛选：发了几个组合、Steam 报的总条数 —— 这两项是"筛选真的生效了"的证据
    if s.queries > 1 {
        text.push_str(&format!(" · 筛选组合 {}", s.queries));
    }
    if let Some(total) = s.total_count {
        text.push_str(&format!(" · 工坊总条数 {total}"));
    }
    text
}

/// 缓存目录：`$WPHUB_WORKSHOP_CACHE_DIR`（可覆盖，测试/搬盘用）
/// 否则 `$XDG_CACHE_HOME/wallpaper-hub/workshop`（默认 `~/.cache/...`）。
///
/// 布局：`thumbs/<工坊id>.img`、`details/<工坊id>.json`、`browse/<url的fnv哈希>.html`
/// 过期判定用文件 mtime（写盘是 tmp+rename，所以不会读到半截文件）。
pub fn cache_root() -> PathBuf {
    if let Ok(dir) = std::env::var("WPHUB_WORKSHOP_CACHE_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    let base = match std::env::var("XDG_CACHE_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/home/dev".into())).join(".cache"),
    };
    base.join("wallpaper-hub").join("workshop")
}

// ── HTTP：三个带超时的 Agent（全模块共用，连接池复用） ────────────────────────

/// 图片 CDN 用的 agent：并发高，超时短，连接池按并发数留 idle 连接。
fn agent_image() -> &'static ureq::Agent {
    static A: OnceLock<ureq::Agent> = OnceLock::new();
    A.get_or_init(|| {
        ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .user_agent(UA)
                .timeout_connect(Some(Duration::from_millis(cfg::WS_CONNECT_TIMEOUT_MS)))
                .timeout_recv_response(Some(Duration::from_millis(cfg::WS_HEADER_TIMEOUT_MS)))
                .timeout_recv_body(Some(Duration::from_millis(cfg::WS_BODY_TIMEOUT_MS)))
                .timeout_global(Some(Duration::from_millis(cfg::WS_GLOBAL_TIMEOUT_MS)))
                .max_idle_connections(cfg::WS_THUMB_CONCURRENCY * 2)
                .max_idle_connections_per_host(cfg::WS_THUMB_CONCURRENCY)
                .build(),
        )
    })
}

/// 搜索页 / 详情接口用的 agent。
fn agent_api() -> &'static ureq::Agent {
    static A: OnceLock<ureq::Agent> = OnceLock::new();
    A.get_or_init(|| {
        ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .user_agent(UA)
                .timeout_connect(Some(Duration::from_millis(cfg::WS_CONNECT_TIMEOUT_MS)))
                .timeout_recv_response(Some(Duration::from_millis(cfg::WS_HEADER_TIMEOUT_MS)))
                .timeout_recv_body(Some(Duration::from_millis(cfg::WS_BODY_TIMEOUT_MS)))
                .timeout_global(Some(Duration::from_millis(cfg::WS_GLOBAL_TIMEOUT_MS)))
                .max_idle_connections(4)
                .max_idle_connections_per_host(4)
                .build(),
        )
    })
}

/// 一次尝试的结果：成功 / 可重试失败 / 永久失败。
enum Attempt {
    Bytes(Vec<u8>),
    Retry { why: String, timeout: bool },
    Fatal(String),
}

/// 4xx（除 408/429）是永久错误，重试没意义；429 和 5xx 值得再试一次。
fn is_retryable_status(code: u16) -> bool {
    code == 408 || code == 429 || (500..600).contains(&code)
}

fn retryable(err: &ureq::Error) -> bool {
    match err {
        ureq::Error::Timeout(_) => true,
        ureq::Error::ConnectionFailed => true,
        ureq::Error::Protocol(_) => true,
        ureq::Error::Io(_) => true,
        ureq::Error::StatusCode(code) => is_retryable_status(*code),
        ureq::Error::HostNotFound => false,
        _ => false,
    }
}

fn get_bytes(agent: &ureq::Agent, url: &str, max: u64) -> Attempt {
    match agent.get(url).call() {
        Ok(mut resp) => read_body(resp.body_mut(), max),
        Err(err) => classify(err),
    }
}

fn post_bytes(agent: &ureq::Agent, url: &str, form: &str, max: u64) -> Attempt {
    match agent
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(form)
    {
        Ok(mut resp) => read_body(resp.body_mut(), max),
        Err(err) => classify(err),
    }
}

fn read_body(body: &mut ureq::Body, max: u64) -> Attempt {
    match body.with_config().limit(max).read_to_vec() {
        Ok(bytes) if !bytes.is_empty() => Attempt::Bytes(bytes),
        Ok(_) => Attempt::Fatal("响应体是空的".into()),
        Err(err) => {
            let timeout = matches!(err, ureq::Error::Timeout(_));
            Attempt::Retry {
                why: format!("读响应体失败: {err}"),
                timeout,
            }
        }
    }
}

fn classify(err: ureq::Error) -> Attempt {
    let timeout = matches!(err, ureq::Error::Timeout(_));
    if retryable(&err) {
        Attempt::Retry {
            why: err.to_string(),
            timeout,
        }
    } else {
        Attempt::Fatal(err.to_string())
    }
}

/// 统一的"请求 + 超时 + 最多重试一次"包装。
///
/// * 每次尝试自身受 agent 的超时约束（建连/响应头/响应体/总时长）；
/// * `deadline` 是**阶段**预算：都快超预算了就不再重试，宁可少一条也别把整页拖住；
/// * 失败只记数并返回 Err，由调用方决定"跳过这一条"。
fn with_retry(
    label: &str,
    deadline: Instant,
    c: &mut Counters,
    mut attempt: impl FnMut() -> Attempt,
) -> Result<Vec<u8>, String> {
    let mut tries = 0usize;
    loop {
        match attempt() {
            Attempt::Bytes(bytes) => return Ok(bytes),
            Attempt::Retry { why, timeout } => {
                if timeout {
                    c.timed_out += 1;
                }
                let budget_left = Instant::now() + Duration::from_millis(cfg::WS_RETRY_BACKOFF_MS) < deadline;
                if tries < cfg::WS_MAX_RETRIES && budget_left {
                    tries += 1;
                    c.retries += 1;
                    wtrace(&format!("重试 {label}（第 {tries} 次）— {why}"));
                    std::thread::sleep(Duration::from_millis(cfg::WS_RETRY_BACKOFF_MS));
                    continue;
                }
                c.failed += 1;
                return Err(why);
            }
            Attempt::Fatal(why) => {
                c.failed += 1;
                return Err(why);
            }
        }
    }
}

// ── 有上限的并行工作队列 ────────────────────────────────────────────────────

/// 保序的并行 map：起 `min(limit, n)` 个线程，共用一个自增游标（动态派发 ——
/// 先下完的线程接着拿下一张，不会被最慢的那张卡住）。
fn parallel_map<T, R, F>(items: &[T], limit: usize, f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }
    let workers = limit.clamp(1, n);
    let mut slots: Vec<Option<R>> = (0..n).map(|_| None).collect();
    let next = AtomicUsize::new(0);
    {
        let slots = Mutex::new(&mut slots);
        let f = &f;
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let value = f(&items[i]);
                    slots.lock().expect("slots 不该被毒化")[i] = Some(value);
                });
            }
        });
    }
    slots
        .into_iter()
        .map(|slot| slot.expect("parallel_map 每个槽都会被填上"))
        .collect()
}

// ── 阶段一：搜索页（拿 id） ────────────────────────────────────────────────

/// browse 页 URL（`tags` = 每个筛选组各取一个 tag；空 = 不筛）。
///
/// 参数名/编码都按 Steam 的来：`requiredtags[]=`、方括号百分号编码成 `%5B%5D`
/// （不编码 Steam 也认，但 `%5B` 是它自己页面上的写法）。
pub fn browse_url(query: &str, page: u32, sort_key: &str, q: &Query) -> String {
    let mut url = format!(
        "{}?appid={APPID}&section=readytouseitems&browsesort={sort_key}&p={page}&numperpage=30",
        browse_base()
    );
    let text = query.trim();
    if !text.is_empty() {
        url.push_str("&searchtext=");
        url.push_str(&urlencode(text));
    }
    // 参数名/编码都按 Steam 的来：方括号百分号编码成 `%5B%5D`
    // （不编码 Steam 也认，但 `%5B` 是它自己页面上的写法）。
    // `excludedtags[]` 是 WE 真正用的那一侧：勾选 = **不排除** ⇒ 这里只放"未勾"的。
    for tag in &q.required {
        url.push_str("&requiredtags%5B%5D=");
        url.push_str(&urlencode(tag));
    }
    for tag in &q.excluded {
        url.push_str("&excludedtags%5B%5D=");
        url.push_str(&urlencode(tag));
    }
    url
}

/// 真正发出去的 browse GET 次数（**只数网络请求，缓存命中不算**）。
///
/// 存在的理由是验收"防抖"：连点 5 次筛选，如果防抖没生效这里会跳 5 次以上；
/// 生效的话一个 300ms 窗口只放行一次。日志里每次放行都印一次它的增量。
pub fn browse_fetch_count() -> u64 {
    BROWSE_FETCHES.load(Ordering::Relaxed)
}

static BROWSE_FETCHES: AtomicU64 = AtomicU64::new(0);

/// 一页 browse 的解析结果（每个筛选组合一份）。
struct BrowsePage {
    ids: Vec<String>,
    occurrences: usize,
    /// 页面上报的工坊总条数（`total_count`）；解析不到 = None
    total: Option<u64>,
    cached: bool,
    stale: bool,
    c: Counters,
}

fn fetch_browse_page(url: &str) -> Result<BrowsePage, String> {
    let key = format!("{}.html", fnv1a_hex(url));
    let mut c = Counters::default();
    let (body, cached, stale) = match cache_read("browse", &key, cfg::WS_BROWSE_TTL_SECS) {
        Some(hit) => (hit, true, false),
        None => {
            let deadline = Instant::now() + Duration::from_millis(cfg::WS_BROWSE_BUDGET_MS);
            match with_retry("browse", deadline, &mut c, || {
                BROWSE_FETCHES.fetch_add(1, Ordering::Relaxed);
                get_bytes(agent_api(), url, BROWSE_MAX_BYTES)
            }) {
                Ok(bytes) => {
                    let _ = cache_write("browse", &key, &bytes);
                    (bytes, false, false)
                }
                Err(err) => {
                    // 单请求失败：退回**过期**缓存（宁可给旧结果也别给空页），完全没有才报错
                    match cache_read("browse", &key, u64::MAX) {
                        Some(stale) => {
                            wtrace(&format!("搜索页请求失败（{err}），退回旧缓存"));
                            (stale, false, true)
                        }
                        None => return Err(format!("创意工坊搜索页请求失败: {err}")),
                    }
                }
            }
        }
    };
    let html = String::from_utf8_lossy(&body);
    let (ids, occurrences) = extract_ids_counted(&html);
    Ok(BrowsePage {
        ids,
        occurrences,
        total: parse_total_count(&html),
        cached,
        stale,
        c,
    })
}

/// 从 browse 页里读 `total_count`（工坊总条数）。
///
/// 页面把它埋在 React Query 的初始数据里，形如 `\\\"total_count\\\":2923758`
/// （套了几层转义）。所以这里**不按整串匹配**：找到 `total_count` 之后往后扫，
/// 跳过引号/冒号/反斜杠，读第一段连续数字。读不到就 None（界面就不显示总数）。
///
/// 这个数是筛选生效最硬的证据：实测无筛选 2 923 758 → `Anime` 731 392 →
/// `Anime`+`Scene` 406 706。
pub fn parse_total_count(html: &str) -> Option<u64> {
    const NEEDLE: &str = "total_count";
    let at = html.find(NEEDLE)?;
    let after = &html[at + NEEDLE.len()..];
    let start = after.find(|c: char| c.is_ascii_digit())?;
    let digits: String = after[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    // 上限保护：`total_count` 后面紧跟的数字不可能有十几位（只取前 18 位）
    digits.get(..digits.len().min(18))?.parse().ok()
}

fn ids_for(
    query: &str,
    page: u32,
    sort_key: &str,
    filter: &Filter,
    stats: &mut Stats,
) -> Result<Vec<String>, String> {
    let t0 = Instant::now();
    let q = filter.query();
    let url = browse_url(query, page, sort_key, &q);
    // 一页**一条**请求：组内 OR / 组间 AND 全由 `excludedtags[]` 表达
    // （上一版按组合叉乘发 N 条再并集，已被这次逆向推翻 —— 见 `Filter::query`）
    stats.queries = 1;
    let p = fetch_browse_page(&url)?;
    stats.retries += p.c.retries;
    stats.timed_out += p.c.timed_out;
    stats.browse_cached = p.cached;
    stats.browse_stale = p.stale;
    stats.total_count = p.total;
    stats.link_occurrences = p.occurrences;
    stats.ids = p.ids.len();
    stats.browse_ms = ms_since(t0);
    wtrace(&format!(
        "browse {}ms · 1 条查询 · 排除 {} 项 / 要求 {} 项 · 链接 {} 次 → 唯一 id {} · 总条数 {:?} cached={} stale={}",
        stats.browse_ms,
        q.excluded.len(),
        q.required.len(),
        p.occurrences,
        p.ids.len(),
        p.total,
        p.cached,
        p.stale
    ));
    // 真实 URL 打一行进日志（排查"筛选到底发了什么"不用猜；验收脚本也读这一行）
    wtrace(&format!("browse URL {url}"));
    Ok(p.ids)
}

/// 从搜索页 HTML 里抽出工坊条目 ID（保持页面顺序、去重），回 `(唯一 id, 链接出现次数)`。
///
/// ⚠ 这里就是"同一 id 会不会在列表里出现两次"的**第一道闸**：Steam 的 browse 页对每个条目
/// 有**两处** `filedetails/?id=` 链接（缩略图一处、标题一处），实测 30 条的页面上共有
/// **60 次**出现。所以页面里"同一 id 出现两次"是**常态**，靠这里的 `out.contains` 折叠成一条；
/// 第二个返回值（出现次数）就是用来把这两件事分开报的（日志/界面状态条都印）。
pub fn extract_ids_counted(html: &str) -> (Vec<String>, usize) {
    const NEEDLE: &str = "filedetails/?id=";
    let mut out: Vec<String> = Vec::new();
    let mut occurrences: usize = 0;
    let mut rest = html;
    while let Some(pos) = rest.find(NEEDLE) {
        let after = &rest[pos + NEEDLE.len()..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            occurrences += 1;
            if !out.contains(&digits) {
                out.push(digits.clone());
                if out.len() >= cfg::WS_MAX_IDS {
                    break;
                }
            }
        }
        // digits 为空时也要前进，避免死循环
        let advance = digits.len().max(1).min(after.len());
        rest = &after[advance..];
    }
    (out, occurrences)
}

// ── 条目层：同一 id 只留一条 ───────────────────────────────────────────────

/// **同一工坊 id 不许在列表里出现两次**——这一条闸就落在这里。
///
/// # 判据（只有一条）
///
/// `id` 字符串**逐字节相等**即视为同一条。工坊 id 是十进制串，**不做** trim / 大小写折叠 /
/// 数字归一化 —— 那些都是"猜两条是不是同一个"，而 id 相等本来就够判；多做的归一化只会制造
/// **误合并**的机会（把两条真的不同的条目并成一条，比多列一格更糟）。
/// **绝不按标题**判断：工坊里标题极像、甚至完全同名的条目是**常态**（实测：
/// `SteamLogoShattered` 2481886649 与 `SteamLogoShattered CUSTOMUIZABLE V2` 2956412281 是同一个
/// 作者的两个不同条目；`Red Samurai` 3533946942 与 `red samurai` 2658466642 也是两条），
/// 按标题合并就会把它们吞掉。
///
/// # 留哪一条（数据驱动、可解释）
///
/// 同 id 有多条时留**信息更全**的那条（[`completeness`] 打分；只看条目自己的数据形状，
/// 不看标题文本、不认条目名字）。同分 ⇒ 留**列表里靠前**的那条（页面/接口给的第一条），
/// 于是"留谁"这件事完全由数据决定、可复现，不会因为遍历顺序变了就换一条。
/// 被替换掉的位置**不变**（list 顺序 = 各 id 首次出现的顺序）。
///
/// 返回 `(保留的条目, 被折叠掉的重复 id)`，后者只用于日志/计数与界面提示。
pub fn dedupe_by_id(items: Vec<Item>) -> (Vec<Item>, Vec<String>) {
    let mut kept: Vec<Item> = Vec::with_capacity(items.len());
    let mut at_of: HashMap<String, usize> = HashMap::new();
    let mut dropped: Vec<String> = Vec::new();
    for item in items {
        match at_of.get(&item.id).copied() {
            None => {
                at_of.insert(item.id.clone(), kept.len());
                kept.push(item);
            }
            Some(at) => {
                // 先记下"这个 id 出现过重复"（下面会把 item 移进 kept，之后就借不到 id 了）
                if !dropped.contains(&item.id) {
                    dropped.push(item.id.clone());
                }
                if completeness(&item) > completeness(&kept[at]) {
                    kept[at] = item;
                }
            }
        }
    }
    (kept, dropped)
}

/// 「信息更全」的评分：标题 / 预览图 / 标签 / 文件体积 / 简介，各占 1 分。
///
/// 只读**数据形状**（空不空、有没有），**不读文本内容、不比对名字** —— 所以它不会因为
/// 标题像谁而偏袒谁。这是 `dedupe_by_id` 里唯一的"留谁"依据，改动它要同时改单测。
fn completeness(item: &Item) -> u8 {
    u8::from(!item.title.trim().is_empty())
        + u8::from(!item.preview_url.trim().is_empty())
        + u8::from(!item.tags.is_empty())
        + u8::from(item.size_bytes > 0)
        + u8::from(!item.description.trim().is_empty())
}

/// 一次结果的收尾：按 id 折叠 + 记数 + 留一行日志。
/// `search()` / `search_entries()` 两条出条目的路都走它，所以"上屏的列表"必然是去重过的。
fn finish_items(items: Vec<Item>, stats: &mut Stats) -> Vec<Item> {
    let (kept, dropped) = dedupe_by_id(items);
    stats.duplicates = dropped.len();
    stats.items = kept.len();
    if !dropped.is_empty() {
        // 真有重复就得能查出**是哪些 id**（不是只报个数字）
        wtrace(&format!(
            "同 id 重复已折叠 {} 条：{}",
            dropped.len(),
            dropped.join(", ")
        ));
    }
    kept
}

// ── 阶段二：详情（缓存优先 + 分批并发） ─────────────────────────────────────

fn details_cached(ids: &[String], stats: &mut Stats) -> Vec<Item> {
    let t0 = Instant::now();
    let mut found: HashMap<String, Item> = HashMap::new();
    let mut miss: Vec<String> = Vec::new();
    for id in ids {
        match cache_read("details", &format!("{id}.json"), cfg::WS_DETAILS_TTL_SECS)
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|value| parse_entry(&value))
        {
            Some(item) => {
                stats.details_cached += 1;
                found.insert(item.id.clone(), item);
            }
            None => miss.push(id.clone()),
        }
    }

    let mut c = Counters::default();
    if !miss.is_empty() {
        let batches: Vec<Vec<String>> = miss.chunks(cfg::WS_DETAILS_BATCH).map(|c| c.to_vec()).collect();
        wtrace(&format!(
            "details 缺 {} 条，{} 批（并发上限 {}）",
            miss.len(),
            batches.len(),
            cfg::WS_DETAILS_CONCURRENCY
        ));
        let outcomes = parallel_map(&batches, cfg::WS_DETAILS_CONCURRENCY, |batch| {
            let mut local = Counters::default();
            let items = details_batch(batch, &mut local);
            (items, local)
        });
        for (items, local) in outcomes {
            c.failed += local.failed;
            c.retries += local.retries;
            c.timed_out += local.timed_out;
            for item in items {
                found.insert(item.id.clone(), item);
            }
        }
    }

    // 按页面顺序输出（详情接口返回的顺序不保证）
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(item) = found.remove(id) {
            out.push(item);
        }
    }
    stats.absorb(c, false);
    stats.details_ms = ms_since(t0);
    wtrace(&format!(
        "details {}ms 命中 {} 失败 {} 重试 {} 超时 {}（要 {} 条）",
        stats.details_ms,
        stats.details_cached,
        c.failed,
        c.retries,
        c.timed_out,
        ids.len()
    ));
    out
}

fn details_batch(ids: &[String], c: &mut Counters) -> Vec<Item> {
    let mut form = format!("itemcount={}", ids.len());
    for (i, id) in ids.iter().enumerate() {
        // publishedfileids[i]，方括号要百分号编码
        form.push_str(&format!("&publishedfileids%5B{i}%5D={id}"));
    }
    let deadline = Instant::now() + Duration::from_millis(cfg::WS_DETAILS_BUDGET_MS);
    let bytes = match with_retry("details", deadline, c, || {
        post_bytes(agent_api(), details_url(), &form, DETAILS_MAX_BYTES)
    }) {
        Ok(bytes) => bytes,
        // 这一批整体失败：跳过（其它批照常出结果）
        Err(err) => {
            wtrace(&format!("详情这一批 {} 条失败，跳过 — {err}", ids.len()));
            return Vec::new();
        }
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(err) => {
            c.failed += ids.len();
            wtrace(&format!("详情 JSON 解析失败，跳过这一批: {err}"));
            return Vec::new();
        }
    };
    let array = value
        .get("response")
        .and_then(|r| r.get("publishedfiledetails"))
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();

    let mut out = Vec::new();
    for entry in &array {
        // result != 1 表示该 ID 无效/已删除/不可见 —— 跳过（不算失败）
        let Some(item) = parse_entry(entry) else {
            continue;
        };
        if let Ok(text) = serde_json::to_vec(entry) {
            let _ = cache_write("details", &format!("{}.json", item.id), &text);
        }
        out.push(item);
    }
    out
}

/// 详情条目 JSON → [`Item`]；`result != 1` 或没有 id 的一律 None。
/// 缓存里存的就是原样的条目 JSON，所以缓存命中和网络解析走的是**同一条**代码。
fn parse_entry(entry: &Value) -> Option<Item> {
    if entry.get("result").and_then(|r| r.as_i64()).unwrap_or(0) != 1 {
        return None;
    }
    let s = |key: &str| {
        entry
            .get(key)
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let id = s("publishedfileid");
    if id.is_empty() {
        return None;
    }
    Some(Item {
        id,
        title: s("title"),
        preview_url: s("preview_url"),
        size_bytes: s("file_size").parse().unwrap_or(0),
        tags: entry
            .get("tags")
            .and_then(|t| t.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.get("tag").and_then(|x| x.as_str()))
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default(),
        subscriptions: entry
            .get("subscriptions")
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
        description: s("description"),
    })
}

// ── 阶段三：缩略图（缓存优先 + 并发下载） ───────────────────────────────────

fn thumbs_cached(items: &[Item], want: usize, stats: &mut Stats) -> Vec<(String, Vec<u8>)> {
    thumbs_cached_each(items, want, stats, None)
}

/// 和 [`thumbs_cached`] 同一套并发/缓存/超时逻辑，额外支持**每拿到一张就回调一次**
/// `on_thumb(id, bytes)`：发现页靠它做「先出条目、缩略图逐张补」的渐进式填充。
///
/// 回调是从 `parallel_map` 的**工作线程**里调的（谁下完谁调），所以必须轻量、非阻塞
/// ——典型的用法是往一个 channel 里 `send`，由 UI 侧把这条消息交给 update。
/// `None` = 只要最终结果、不要逐张回调（老的一次性路径）。
fn thumbs_cached_each(
    items: &[Item],
    want: usize,
    stats: &mut Stats,
    on_thumb: Option<&(dyn Fn(String, Vec<u8>) + Sync)>,
) -> Vec<(String, Vec<u8>)> {
    thumbs_cached_at(&cache_root(), items, want, stats, on_thumb)
}

/// 同上，但缓存根目录由调用方给（测试用临时目录注入，避免碰全局 `cache_root()`）。
fn thumbs_cached_at(
    root: &Path,
    items: &[Item],
    want: usize,
    stats: &mut Stats,
    on_thumb: Option<&(dyn Fn(String, Vec<u8>) + Sync)>,
) -> Vec<(String, Vec<u8>)> {
    let t0 = Instant::now();
    let targets: Vec<&Item> = items
        .iter()
        .filter(|item| !item.preview_url.is_empty())
        .take(want)
        .collect();

    let outcomes = parallel_map(&targets, cfg::WS_THUMB_CONCURRENCY, |item: &&Item| {
        let out = thumb_one(root, *item);
        if let Some(cb) = on_thumb
            && let Some(bytes) = &out.bytes
        {
            cb(out.id.clone(), bytes.clone());
        }
        out
    });

    let mut out = Vec::with_capacity(outcomes.len());
    let mut c = Counters::default();
    for outcome in outcomes {
        c.cached += outcome.c.cached;
        c.failed += outcome.c.failed;
        c.retries += outcome.c.retries;
        c.timed_out += outcome.c.timed_out;
        if let Some(bytes) = outcome.bytes {
            out.push((outcome.id, bytes));
        }
    }
    stats.absorb(c, true);
    stats.thumbs_ms = ms_since(t0);
    wtrace(&format!(
        "thumbs {}ms 拿到 {}/{} 命中 {} 失败 {} 重试 {} 超时 {}（并发 {}）",
        stats.thumbs_ms,
        out.len(),
        targets.len(),
        c.cached,
        c.failed,
        c.retries,
        c.timed_out,
        cfg::WS_THUMB_CONCURRENCY
    ));
    out
}

struct ThumbOut {
    id: String,
    bytes: Option<Vec<u8>>,
    c: Counters,
}

fn thumb_one(root: &Path, item: &Item) -> ThumbOut {
    let key = format!("{}.img", item.id);
    let mut c = Counters::default();
    if let Some(bytes) = cache_read_at(root, "thumbs", &key, cfg::WS_THUMB_TTL_SECS) {
        c.cached += 1;
        return ThumbOut {
            id: item.id.clone(),
            bytes: Some(bytes),
            c,
        };
    }
    let deadline = Instant::now() + Duration::from_millis(cfg::WS_THUMBS_BUDGET_MS);
    let label = format!("thumb {}", item.id);
    match with_retry(&label, deadline, &mut c, || {
        get_bytes(agent_image(), &item.preview_url, cfg::WS_THUMB_MAX_BYTES)
    }) {
        Ok(bytes) => {
            let _ = cache_write_at(root, "thumbs", &key, &bytes);
            ThumbOut {
                id: item.id.clone(),
                bytes: Some(bytes),
                c,
            }
        }
        Err(err) => {
            // 单张失败 ⇒ 跳过这一张，界面显示"无预览"占位，整页照常出来
            wtrace(&format!("缩略图跳过 {} — {err}", item.id));
            ThumbOut {
                id: item.id.clone(),
                bytes: None,
                c,
            }
        }
    }
}

// ── 磁盘缓存 ───────────────────────────────────────────────────────────────

fn cache_path(root: &Path, kind: &str, key: &str) -> PathBuf {
    root.join(kind).join(key)
}

/// mtime 过期判定（没有 mtime ⇒ 当作过期）。`ttl_secs = u64::MAX` 表示"再旧也要"。
fn fresh(path: &Path, ttl_secs: u64) -> bool {
    match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(modified) => match SystemTime::now().duration_since(modified) {
            Ok(age) => age.as_secs() <= ttl_secs,
            // mtime 在未来（时钟回拨）⇒ 当作新鲜
            Err(_) => true,
        },
        Err(_) => false,
    }
}

fn cache_read_at(root: &Path, kind: &str, key: &str, ttl_secs: u64) -> Option<Vec<u8>> {
    let path = cache_path(root, kind, key);
    if !fresh(&path, ttl_secs) {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    if bytes.is_empty() { None } else { Some(bytes) }
}

fn cache_write_at(root: &Path, kind: &str, key: &str, bytes: &[u8]) -> std::io::Result<()> {
    let path = cache_path(root, kind, key);
    let dir = path.parent().ok_or_else(|| std::io::Error::other("缓存路径没有父目录"))?;
    std::fs::create_dir_all(dir)?;
    // 先写临时文件再 rename：并发/中断都不会留下半截文件被当成命中
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)
}

fn cache_read(kind: &str, key: &str, ttl_secs: u64) -> Option<Vec<u8>> {
    cache_read_at(&cache_root(), kind, key, ttl_secs)
}

fn cache_write(kind: &str, key: &str, bytes: &[u8]) -> std::io::Result<()> {
    cache_write_at(&cache_root(), kind, key, bytes)
}

/// 缓存清理：每 60s 最多跑一次（翻页不该每次扫目录），
/// 超出上限时按 mtime **最旧优先**删，直到条数与字节数都回到上限内。
fn housekeeping(root: &Path) {
    static LOCK: Mutex<()> = Mutex::new(());
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if now.saturating_sub(LAST.load(Ordering::Relaxed)) < 60 {
        return;
    }
    let Ok(_guard) = LOCK.try_lock() else {
        return;
    };
    if now.saturating_sub(LAST.load(Ordering::Relaxed)) < 60 {
        return;
    }
    LAST.store(now, Ordering::Relaxed);
    housekeeping_at(root, cfg::WS_CACHE_MAX_ENTRIES, cfg::WS_CACHE_MAX_BYTES);
}

fn housekeeping_at(root: &Path, max_entries: usize, max_bytes: u64) {
    let dir = root.join("thumbs");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let modified = meta.modified().unwrap_or(UNIX_EPOCH);
        total += meta.len();
        files.push((modified, meta.len(), entry.path()));
    }
    if files.len() <= max_entries && total <= max_bytes {
        return;
    }
    files.sort_by_key(|(modified, _, _)| *modified);
    let mut count = files.len();
    let mut bytes = total;
    let mut removed = 0usize;
    for (_, size, path) in files {
        if count <= max_entries && bytes <= max_bytes {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            count -= 1;
            bytes = bytes.saturating_sub(size);
            removed += 1;
        }
    }
    if removed > 0 {
        wtrace(&format!(
            "缓存清理：删掉 {removed} 个最旧的缩略图（现在 {count} 个 / {} MB）",
            bytes / 1024 / 1024
        ));
    }
}

/// FNV-1a 64（只用来给长 URL 做个稳定的短文件名，不是加密）
fn fnv1a_hex(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

// ── 日志/工具 ──────────────────────────────────────────────────────────────

/// 阶段耗时日志默认开着（`WPHUB_WORKSHOP_TIMING=0` 关掉）：
/// 加载慢的时候第一手证据就是这几行，比截图猜强。
fn timing_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("WPHUB_WORKSHOP_TIMING")
            .map(|v| v != "0")
            .unwrap_or(true)
    })
}

fn wtrace(msg: &str) {
    if timing_on() {
        eprintln!("[workshop] {msg}");
    }
}

fn ms_since(t0: Instant) -> u64 {
    t0.elapsed().as_millis() as u64
}

/// 极简 URL 编码（够用于搜索词）
pub fn urlencode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("wphub-workshop-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 只要"唯一 id 列表"时的薄封装（生产代码走 [`extract_ids_counted`]，它同时给出现次数）。
    fn extract_ids(html: &str) -> Vec<String> {
        extract_ids_counted(html).0
    }

    #[test]
    fn extracts_ids_in_page_order_without_duplicates() {
        let html = r#"
            <a href="https://steamcommunity.com/sharedfiles/filedetails/?id=1195626192">a</a>
            <a href="/sharedfiles/filedetails/?id=1689515439&searchtext=">b</a>
            <a href="/sharedfiles/filedetails/?id=1195626192">重复</a>
            <div>filedetails/?id=notanumber</div>
            <a href="/sharedfiles/filedetails/?id=2080534540">c</a>
        "#;
        assert_eq!(
            extract_ids(html),
            vec!["1195626192", "1689515439", "2080534540"],
            "要保持页面顺序并去重，非数字的跳过"
        );
    }

    #[test]
    fn survives_degenerate_pages() {
        assert!(extract_ids("<html>没有链接</html>").is_empty());
        assert!(
            extract_ids("filedetails/?id=").is_empty(),
            "空 id 不能让解析死循环"
        );
        assert!(extract_ids("").is_empty());
    }

    /// **取证**：真实的 browse 页对每个条目有**两处** `filedetails/?id=` 链接
    /// （缩略图一处、标题一处 ⇒ 本机实测 30 条 = 60 次出现）。
    /// 所以"页面里同一 id 出现两次"是常态，靠 extract_ids 按 id 折叠成一条。
    /// 钉住这一点，是因为"列表里同一 id 两条"的图像**只能**从这条误解里来。
    #[test]
    fn real_page_shape_links_every_item_twice_and_collapses_to_unique() {
        // 形如 Steam 页面：每条一个缩略图链接 + 一个标题链接
        let html = r#"
          <div class="cell"><a href="https://steamcommunity.com/sharedfiles/filedetails/?id=2481886649">
            <img src="..."/></a></div>
          <div class="title"><a href="https://steamcommunity.com/sharedfiles/filedetails/?id=2481886649">SteamLogoShattered</a></div>
          <div class="cell"><a href="/sharedfiles/filedetails/?id=2956412281"><img/></a></div>
          <div class="title"><a href="/sharedfiles/filedetails/?id=2956412281">SteamLogoShattered CUSTOMUIZABLE V2</a></div>
        "#;
        let (ids, occurrences) = extract_ids_counted(html);
        assert_eq!(occurrences, 4, "页面里 4 处链接（2 条 × 2 处）");
        assert_eq!(
            ids,
            vec!["2481886649", "2956412281"],
            "折叠后是 2 个唯一 id，顺序保持页面顺序"
        );
        assert_eq!(extract_ids(html), ids, "老入口同义");
        assert_eq!(
            occurrences - ids.len(),
            2,
            "差出来那 2 次就是页面内部的同 id 重复（不是列表重复）"
        );
    }

    // ── 同 id 去重：判据 / 留谁 / 假阳性护栏 ────────────────────────────────

    /// 拿**本机真实的 search 页缓存**（`WPHUB_WORKSHOP_CACHE_DIR` /
    /// `~/.cache/wallpaper-hub/workshop/browse/*.html`）再跑一遍解析：
    /// 每个真实页面上 `filedetails/?id=` 链接出现次数都会**多于**唯一 id 数（Steam 每条两处链接），
    /// 而唯一 id 列表里**一个重复都不许有**。
    /// 没有缓存就**跳过**（不是失败）：这是台机器上的附加取证，不该让别的环境红。
    #[test]
    fn real_cached_pages_have_no_duplicate_ids() {
        let dir = cache_root().join("browse");
        let Ok(rd) = std::fs::read_dir(&dir) else {
            eprintln!("没有 {} ⇒ 跳过（真实页面取证只在有缓存的机器上跑）", dir.display());
            return;
        };
        let mut checked = 0usize;
        for entry in rd.flatten() {
            let Ok(html) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let (ids, occurrences) = extract_ids_counted(&html);
            let unique: std::collections::HashSet<&String> = ids.iter().collect();
            assert_eq!(unique.len(), ids.len(), "唯一 id 列表里不许有重复：{:?}", entry.path());
            assert!(
                occurrences >= ids.len(),
                "出现次数不可能少于唯一 id 数：{} < {}",
                occurrences,
                ids.len()
            );
            checked += 1;
        }
        eprintln!("真实缓存页取证：{checked} 个页面，全部无重复 id");
    }

    fn item_of(id: &str, title: &str) -> Item {
        Item {
            id: id.to_string(),
            title: title.to_string(),
            preview_url: format!("https://cdn.example/{id}.jpg"),
            size_bytes: 1024,
            tags: vec!["Scene".into()],
            subscriptions: 1,
            description: "x".into(),
        }
    }

    /// ① 同 id 两条 ⇒ 合成一条（这是要解决的问题本身）
    #[test]
    fn dedupe_collapses_two_entries_with_the_same_id() {
        let items = vec![
            item_of("2481886649", "SteamLogoShattered"),
            item_of("2956412281", "SteamLogoShattered CUSTOMUIZABLE V2"),
            item_of("2481886649", "SteamLogoShattered"),
        ];
        let (kept, dropped) = dedupe_by_id(items);
        assert_eq!(kept.len(), 2, "3 条里同 id 的那两条要合成一条");
        assert_eq!(
            kept.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["2481886649", "2956412281"],
            "顺序 = 各 id 首次出现的顺序"
        );
        assert_eq!(dropped, vec!["2481886649"], "被折叠掉的 id 要能报出来");
        // 同 id 出现三次也只留一条（不是只折叠一次）
        let thrice = vec![
            item_of("7", "a"),
            item_of("7", "a"),
            item_of("7", "a"),
            item_of("8", "b"),
        ];
        let (kept, dropped) = dedupe_by_id(thrice);
        assert_eq!(kept.len(), 2);
        assert_eq!(dropped, vec!["7"], "同一个 id 只记一次");
    }

    /// ② 关键的**假阳性护栏**：不同 id、标题极像（甚至一模一样）⇒ 一条都不许并。
    /// 数据取自本机真实缓存（`~/.cache/wallpaper-hub/workshop/details/*.json`）：
    /// 2481886649 / 2956412281 同一作者的两个条目；3533946942 / 2658466642 只是大小写不同。
    #[test]
    fn dedupe_never_merges_different_ids_even_with_identical_titles() {
        // 验收截图里怀疑"重复"的那一格，其实是这两条不同 id
        let real_pair = vec![
            item_of("2481886649", "SteamLogoShattered"),
            item_of("2956412281", "SteamLogoShattered CUSTOMUIZABLE V2"),
        ];
        let (kept, dropped) = dedupe_by_id(real_pair);
        assert_eq!(kept.len(), 2, "标题像 ≠ 同一条");
        assert!(dropped.is_empty());

        // 大小写不同的同名条目
        let case_pair = vec![
            item_of("3533946942", "Red Samurai"),
            item_of("2658466642", "red samurai"),
        ];
        assert_eq!(dedupe_by_id(case_pair).0.len(), 2, "大小写不同也是两条");

        // 最极端：标题**完全相同**、其它字段也相同，只有 id 不同 ⇒ 仍不许并
        let twins = vec![item_of("100", "Samurai"), item_of("101", "Samurai")];
        let (kept, dropped) = dedupe_by_id(twins);
        assert_eq!(kept.len(), 2, "判据只有 id，标题一模一样也不能并");
        assert!(dropped.is_empty());
    }

    /// ③ 同 id 留谁：**信息更全**的胜；同分留靠前的。位置取首次出现处（顺序稳定）。
    #[test]
    fn dedupe_keeps_the_most_complete_entry_then_page_order() {
        let thin = Item {
            id: "42".into(),
            title: "".into(),
            preview_url: String::new(),
            size_bytes: 0,
            tags: Vec::new(),
            subscriptions: 0,
            description: String::new(),
        };
        let rich = item_of("42", "完整的一条");
        // 先薄后全 ⇒ 位置仍是第 1 格，但内容是"全"的那条
        let (kept, dropped) = dedupe_by_id(vec![thin.clone(), rich.clone(), item_of("43", "别的")]);
        assert_eq!(dropped, vec!["42"]);
        assert_eq!(kept[0].id, "42");
        assert_eq!(kept[0].title, "完整的一条", "留信息更全的那条");
        assert_eq!(kept[1].id, "43");
        assert_eq!(completeness(&rich), 5);
        assert_eq!(completeness(&thin), 0, "空条目 0 分（只看数据形状）");
        // 同分 ⇒ 先出现的胜（可复现：不会因为遍历顺序变了就换一条）
        let a = item_of("42", "先");
        let b = item_of("42", "后");
        assert_eq!(dedupe_by_id(vec![a, b]).0[0].title, "先");
    }

    /// ④ 没有重复、或空列表 ⇒ 原样返回（去重不许成为"改写列表"的理由）。
    #[test]
    fn dedupe_is_a_noop_when_ids_are_already_unique() {
        let items = vec![
            item_of("1", "a"),
            item_of("2", "b"),
            item_of("3", "c"),
        ];
        let (kept, dropped) = dedupe_by_id(items.clone());
        assert!(dropped.is_empty());
        assert_eq!(
            kept.iter().map(|i| i.id.clone()).collect::<Vec<_>>(),
            items.iter().map(|i| i.id.clone()).collect::<Vec<_>>(),
            "顺序与内容都不许变"
        );
        let (kept, dropped) = dedupe_by_id(Vec::new());
        assert!(kept.is_empty() && dropped.is_empty());
    }

    /// ⑤ **去重护栏**：同一 id 只留**一条**，且留下的 id 集合 == 原来的 id 集合。
    ///
    /// （登录/订阅整套 UI 已在 2026-10-21 删除，角标那条断言随之退场；去重本身仍是
    /// 「上屏列表里同一 id 只出现一次」的保证。）
    #[test]
    fn dedupe_keeps_exactly_one_entry_per_id() {
        const SUB_ID: u64 = 2481886649; // 本机内容目录里真实存在的一条
        let thin = Item {
            id: SUB_ID.to_string(),
            title: String::new(),
            preview_url: String::new(),
            size_bytes: 0,
            tags: Vec::new(),
            subscriptions: 0,
            description: String::new(),
        };
        let before: Vec<Item> = vec![
            thin,
            item_of(&SUB_ID.to_string(), "SteamLogoShattered"),
            item_of("2956412281", "另一条"),
        ];
        let (kept, dropped) = dedupe_by_id(before.clone());

        assert_eq!(kept.len(), 2);
        assert_eq!(dropped, vec![SUB_ID.to_string()]);
        let mut have: Vec<String> = kept.iter().map(|i| i.id.clone()).collect();
        have.sort();
        assert_eq!(have, vec![SUB_ID.to_string(), "2956412281".to_string()]);
        assert!(
            kept.iter().filter(|i| i.id == SUB_ID.to_string()).count() == 1,
            "上屏的同一 id 只能有一条"
        );
    }

    #[test]
    fn urlencodes_query_terms() {
        assert_eq!(urlencode("夜空"), "%E5%A4%9C%E7%A9%BA");
        assert_eq!(urlencode("night sky"), "night%20sky");
        assert_eq!(urlencode("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn sort_keys_are_non_empty() {
        for (key, label) in SORTS {
            assert!(!key.is_empty() && !label.is_empty());
        }
    }

    // ── 筛选：与 Win 版 WE 面板逐项一致（黑名单取反） ───────────────────────
    //
    // 这些用例的"标准答案"不在这里手写：`filter_matches_reversed_spec` 直接读
    // `artifacts/we-ui-filters-20261022/filters.json`（从 Win 版 WE 的明文面板 + exe
    // 指针表拓下来的规格），**用另一套独立实现**从 JSON 推出应有的标签集合，再和
    // 我们生成的 URL 逐组比对。下面那些用例是手感/回归，不承担"和规格一致"的证明。

    /// 六组结构：顺序、小节、选项数、**默认值**都照抄规格（`filters.json`）。
    #[test]
    fn filter_groups_match_we_panel_layout() {
        let keys: Vec<&str> = GROUPS.iter().map(|g| g.key).collect();
        assert_eq!(
            keys,
            vec![G_TYPE, G_RATING, G_RESOLUTION, G_SOURCE, G_TAGS],
            "组顺序 = WE 面板顺序（仅显示组单独表示）"
        );
        // 折叠默认值：类型/年龄分级/分辨率**收起**，来源/标签**展开**
        let collapsed: Vec<bool> = GROUPS.iter().map(|g| g.collapsed).collect();
        assert_eq!(collapsed, vec![true, true, true, false, false]);

        // 类型组：两个小节（typetags / categorytags），Application 在工坊默认**不勾**
        let ty = GROUPS.iter().find(|g| g.key == G_TYPE).unwrap();
        assert_eq!(ty.sections.len(), 2, "类型 + 分类（分割线）");
        assert_eq!(ty.sections[0].opts.len(), 4);
        assert_eq!(ty.sections[1].opts.len(), 2);
        assert!(!is_real_tag(G_TYPE, "Application") == false, "Application 是真标签");
        assert!(ty.sections[0].opts.iter().any(|o| o.tag == "Application" && !o.default));

        // 分辨率：5 个宽高比子组（各带所有/无）+ 2 个尾巴，共 23 + 2
        let res = GROUPS.iter().find(|g| g.key == G_RESOLUTION).unwrap();
        assert_eq!(res.sections.len(), 6, "5 个子组 + 尾巴");
        let (all_none, sizes): (Vec<bool>, Vec<usize>) =
            res.sections.iter().map(|s| (s.all_none, s.opts.len())).unzip();
        assert_eq!(all_none, vec![true, true, true, true, true, false]);
        assert_eq!(sizes, vec![6, 3, 4, 5, 5, 2]);
        assert_eq!(group_opts(G_RESOLUTION).count(), 25);
        // 3840 x 2160 属于**宽屏**子组，不是独立组；8K 只在语言包里，不是筛选标签
        assert!(res.sections[0].opts.iter().any(|o| o.tag == "3840 x 2160"));
        assert!(!is_real_tag(G_RESOLUTION, "7680 x 4320"), "8K 不在筛选数组里");
        for shorthand in ["4K", "1080p", "1440p", "8K"] {
            assert!(!is_real_tag(G_RESOLUTION, shorthand), "{shorthand} 不是工坊标签");
        }

        // 标签组：25 条，可见顺序 Abstract..Vehicle 然后 Unspecified（WE 把 C++ 数组第 0 条挪到末尾）
        let tg = GROUPS.iter().find(|g| g.key == G_TAGS).unwrap();
        let tags: Vec<&str> = group_opts(G_TAGS).map(|o| o.tag).collect();
        assert_eq!(tags.len(), 25);
        assert_eq!(tags.first().copied(), Some("Abstract"));
        assert_eq!(tags.last().copied(), Some("Unspecified"));
        assert!(tg.all_none, "标签组在组头上有「所有/无」");
        assert!(tg.sections[0].all_none_skip == "Unspecified", "所有/无 跳过 Unspecified");

        // 来源组：纯本地（不发查询参数）；WE 里它只在"已安装"源出现
        let src = GROUPS.iter().find(|g| g.key == G_SOURCE).unwrap();
        assert!(!src.query_backed);
        assert_eq!(group_opts(G_SOURCE).count(), 3);
    }

    /// **默认状态 = WE 默认筛选**：排除 Application / Questionable / Mature / Unspecified，
    /// 不往 `requiredtags[]` 里写任何东西（类型/年龄/分辨率/风格从不进白名单）。
    #[test]
    fn default_filter_is_the_we_default_query() {
        let f = Filter::default();
        assert_eq!(f.changed_count(), 0, "默认状态不该被算成「已筛选」");
        let q = f.query();
        assert_eq!(
            q.excluded,
            vec![
                "Application".to_string(),
                "Mature".to_string(),
                "Questionable".to_string(),
                "Unspecified".to_string(),
            ],
            "默认只排这四项（排好序）"
        );
        assert!(q.required.is_empty(), "默认没有 requiredtags");
        // 默认勾着的那些确实在"不排除"侧
        for (g, t) in [
            (G_TYPE, "Scene"),
            (G_TYPE, "Wallpaper"),
            (G_RATING, "Everyone"),
            (G_RESOLUTION, "3840 x 2160"),
            (G_TAGS, "Anime"),
        ] {
            assert!(f.checked(g, t), "默认应勾着 {g}:{t}");
        }
        assert!(!f.checked(G_RATING, "Mature"));
        assert!(!f.checked(G_TAGS, "Unspecified"));
    }

    /// 黑名单取反：**勾选 = 不排除**。去勾才进 `excludedtags[]`，勾回来就消失；
    /// 组内多个 = OR（都在排除表里），组间 = AND（同一条查询里叠加）。
    #[test]
    fn query_is_blacklist_inverted_group_or_and_cross_group_and() {
        let mut f = Filter::default();
        // 去勾 Anime+Game（同一组两项）⇒ 两个都进排除表（组内 OR：任一即可留下）
        assert!(!f.toggle(G_TAGS, "Anime"), "点一下 = 去勾");
        assert!(!f.toggle(G_TAGS, "Game"));
        let q = f.query();
        assert!(q.excluded.contains(&"Anime".to_string()));
        assert!(q.excluded.contains(&"Game".to_string()));
        assert!(q.required.is_empty(), "风格标签从不进 requiredtags");
        assert_eq!(q.excluded.len(), 6, "默认 4 + Anime + Game：{q:?}");

        // 再跨一组：去勾 Video、Mature 之外的 Everyone（年龄组换一档）
        f.toggle(G_TYPE, "Video");
        f.toggle(G_RATING, "Everyone"); // 去勾 → Everyone 进排除
        f.toggle(G_RATING, "Mature"); // 勾上 → Mature 从排除表里消失
        let q = f.query();
        for t in ["Anime", "Game", "Video", "Everyone"] {
            assert!(q.excluded.contains(&t.to_string()), "{t} 应被排除: {q:?}");
        }
        assert!(!q.excluded.contains(&"Mature".to_string()), "勾上 Mature 后不再排除它");
        assert!(!q.excluded.contains(&"Scene".to_string()), "Scene 一直勾着");

        // 勾上 Application（WE 默认不勾）⇒ 它从排除表里消失
        f.toggle(G_TYPE, "Application");
        assert!(!f.query().excluded.contains(&"Application".to_string()));
    }

    /// 某组**全不勾** ⇒ 追加字面量 `Unspecified`（否则 Steam 那边等于"不筛"）。
    /// 粒度 = WE 的"一个 angular 模型对象"：类型的 typetags / categorytags 各算一个，
    /// 分辨率的 5 个子组 + 尾巴合起来**只算一个**。
    #[test]
    fn empty_group_appends_unspecified_fallback() {
        // 年龄分级全不勾 ⇒ 三个真标签 + 一个 Unspecified
        let mut f = Filter::default();
        for tag in ["Everyone", "Questionable", "Mature"] {
            f.set_checked(G_RATING, tag, false);
        }
        let q = f.query();
        assert_eq!(
            q.excluded.iter().filter(|t| *t == "Unspecified").count(),
            1,
            "空组保底只加一次：{q:?}"
        );
        for tag in ["Everyone", "Questionable", "Mature"] {
            assert!(q.excluded.contains(&tag.to_string()));
        }

        // 分辨率：只清掉尾巴那两节不够（整组还是一个 map）⇒ **不**额外加 Unspecified
        let mut g = Filter::default();
        g.set_section(G_RESOLUTION, "tail", false);
        let q = g.query();
        assert!(q.excluded.contains(&"Other resolution".to_string()));
        assert!(q.excluded.contains(&"Dynamic resolution".to_string()));
        assert_eq!(
            q.excluded.iter().filter(|t| *t == "Unspecified").count(),
            1,
            "只有标签组那一份默认的 Unspecified，不该多出来：{q:?}"
        );

        // 分辨率整组全清 ⇒ 25 个标签 + 保底 Unspecified。
        // 先把标签组的 Unspecified 勾上（否则它本来就在排除表里，看不出保底有没有发生）
        let mut g = Filter::default();
        g.set_checked(G_TAGS, "Unspecified", true);
        assert!(!g.query().excluded.contains(&"Unspecified".to_string()));
        for section in ["widescreen", "ultrawide", "dual", "triple", "portrait", "tail"] {
            g.set_section(G_RESOLUTION, section, false);
        }
        let q = g.query();
        assert_eq!(
            q.excluded.iter().filter(|t| *t == "Unspecified").count(),
            1,
            "分辨率整组清空 ⇒ 保底那一个 Unspecified：{q:?}"
        );
        assert_eq!(
            q.excluded.iter().filter(|t| *t == "Other resolution").count(),
            1,
            "标签不许重复"
        );
    }

    /// 「重置过滤器」= **回到默认**（不是清空）+ 套 `defaultfilterconfig`。
    #[test]
    fn reset_restores_default_and_applies_defaultfilterconfig() {
        let mut f = Filter::default();
        f.toggle(G_TAGS, "Anime");
        f.toggle(G_RATING, "Mature");
        f.toggle(G_TYPE, "Application");
        f.toggle(G_RESOLUTION, "3840 x 2160");
        f.toggle_only(ONLY_APPROVED);
        assert_ne!(f, Filter::default());

        f.reset();
        assert_eq!(f, Filter::default(), "重置 = 默认筛选，不是「什么都不排」");
        assert!(f.checked(G_RATING, "Everyone"));
        assert!(!f.checked(G_RATING, "Mature"));
        assert!(!f.checked(G_TYPE, "Application"), "重置后 Application 仍不勾");
        assert_eq!(f.query().excluded.len(), 4);

        // defaultfilterconfig.Anime === false ⇒ 额外排除 Anime/MMD/Girls/Guys/Music
        let cfg = DefaultFilterConfig {
            anime: Some(false),
            ..DefaultFilterConfig::default()
        };
        f.reset_with(&cfg);
        let q = f.query();
        for tag in ["Anime", "MMD", "Girls", "Guys", "Music"] {
            assert!(q.excluded.contains(&tag.to_string()), "{tag} 应被排除：{q:?}");
        }
        // showotherresolution === false ⇒ 取消 Other resolution
        let cfg = DefaultFilterConfig {
            show_other_resolution: false,
            ..DefaultFilterConfig::default()
        };
        f.reset_with(&cfg);
        assert!(f.query().excluded.contains(&"Other resolution".to_string()));
        // recommendedresolution 的"推荐集合"逆向没定下来 ⇒ 绝不瞎猜（这一位不许改分辨率）
        let cfg = DefaultFilterConfig {
            recommended_resolution: true,
            ..DefaultFilterConfig::default()
        };
        f.reset_with(&cfg);
        assert_eq!(f, Filter::default(), "未定的规则不许改任何东西");
    }

    /// 标签组的「所有/无」跳过 `Unspecified`（WE 在工坊源里强制不勾它）。
    #[test]
    fn all_none_on_tags_skips_unspecified() {
        let mut f = Filter::default();
        f.set_section(G_TAGS, "tags", false);
        assert!(f.checked(G_TAGS, "Unspecified") == false, "它一直是未勾");
        // 24 个真标签全排 + 默认那 3 个 + 空组保底 Unspecified
        let q = f.query();
        for tag in ["Abstract", "Anime", "Vehicle"] {
            assert!(q.excluded.contains(&tag.to_string()));
        }
        // "无"之后"所有"：24 个回来，Unspecified 仍不勾
        f.set_section(G_TAGS, "tags", true);
        let q = f.query();
        assert!(!q.excluded.contains(&"Abstract".to_string()));
        assert!(q.excluded.contains(&"Unspecified".to_string()), "它永远不勾");
    }

    /// 「仅显示」组：Approved 走 `requiredtags[]`；手机兼容额外排除 Application/Web。
    #[test]
    fn show_only_writes_the_required_side() {
        let mut f = Filter::default();
        assert_eq!(f.toggle_only(ONLY_APPROVED), true);
        assert_eq!(f.query().required, vec!["Approved".to_string()]);
        assert_eq!(f.toggle_only(ONLY_APPROVED), false, "再点一下取消");
        assert!(f.query().required.is_empty());

        f.toggle_only(ONLY_MOBILE);
        let q = f.query();
        assert!(q.excluded.contains(&"Application".to_string()));
        assert!(q.excluded.contains(&"Web".to_string()));
        assert!(q.excluded.contains(&"Anime".to_string()) == false);
        // 不认识的 key 不许改状态
        assert!(!f.toggle_only("nope"));
        // 假标签 / 假组键：什么都不改（绝不会发 Steam 不认的标签）
        let before = f.clone();
        assert!(!f.set_checked("bogus", "Anime", false));
        assert!(!f.set_checked(G_TAGS, "NotARealGenre", false));
        assert_eq!(f, before);
    }

    /// URL 形状：`requiredtags[]` / **`excludedtags[]`** 都用 `%5B%5D` 编码，
    /// 值用 `%20` 编码；搜索词/排序/页码**一起带上**（不能因为加筛选就弄丢）。
    #[test]
    fn browse_url_carries_search_sort_page_and_encoded_tags() {
        let mut f = Filter::default();
        f.toggle(G_RESOLUTION, "3840 x 2160"); // 去勾 → 排除（值里的空格要编码）
        f.toggle_only(ONLY_APPROVED); // required 侧
        let url = browse_url("夜空", 3, "toprated", &f.query());
        assert!(url.contains("appid=431960"), "{url}");
        assert!(url.contains("section=readytouseitems"));
        assert!(url.contains("browsesort=toprated"), "排序不能被筛选顶掉：{url}");
        assert!(url.contains("p=3"), "页码要带上：{url}");
        assert!(url.contains("numperpage=30"));
        assert!(url.contains("searchtext=%E5%A4%9C%E7%A9%BA"), "搜索词要带上：{url}");
        assert!(
            url.contains("excludedtags%5B%5D=3840%20x%202160"),
            "分辨率标签里的空格要编码：{url}"
        );
        assert!(url.contains("requiredtags%5B%5D=Approved"), "{url}");
        // 默认筛选 ⇒ 只有 excludedtags，没有 requiredtags，也没有 searchtext
        let plain = browse_url("", 1, "trend", &Filter::default().query());
        assert!(!plain.contains("requiredtags"), "{plain}");
        assert!(!plain.contains("searchtext"), "空关键词不该出现 searchtext：{plain}");
        assert!(plain.contains("excludedtags%5B%5D=Application"));
        assert!(!plain.contains("excludedtags%5B%5D=Anime"), "{plain}");
    }

    /// 持久化：只写**与默认不同**的项，`组键:标签=1|0`；空表 = 默认；脏行丢掉。
    #[test]
    fn filter_round_trips_through_config_lines() {
        // 默认状态 ⇒ 零行（空表 = 默认，不是"不筛"）
        assert!(Filter::default().to_lines().is_empty());

        let mut f = Filter::default();
        f.toggle(G_TAGS, "Anime"); // 去勾
        f.toggle(G_TYPE, "Application"); // 勾上（默认是不勾的）
        f.toggle_only(ONLY_APPROVED);
        let lines = f.to_lines();
        assert_eq!(lines.len(), 3, "只写差异：{lines:?}");
        assert!(lines.contains(&format!("{G_TAGS}:Anime=0")), "{lines:?}");
        assert!(lines.contains(&format!("{G_TYPE}:Application=1")), "{lines:?}");
        assert!(lines.contains(&format!("only:{ONLY_APPROVED}=1")), "{lines:?}");
        assert_eq!(Filter::from_lines(&lines), f, "写出去再读回来必须一模一样");

        // 脏数据：不认识的组、不是真标签、没有状态位（旧格式）、怪状态位 —— 全丢，
        // 但**不许把整个筛选搞坏**（剩下的照常生效）
        let dirty = vec![
            format!("{G_TAGS}:Anime=0"),
            "bogus:Anime=0".to_string(),
            format!("{G_TAGS}:NotARealGenre=0"),
            "没有等号".to_string(),
            format!("{G_RESOLUTION}:3840 x 2160=2"),
            format!("only:nope=1"),
        ];
        let cleaned = Filter::from_lines(&dirty);
        assert!(!cleaned.checked(G_TAGS, "Anime"), "唯一一行合法配置要生效");
        assert_eq!(cleaned.changed_count(), 1, "{cleaned:?}");
        // 空配置 = WE 默认（而不是"什么都不排除"）
        assert_eq!(Filter::from_lines(&[]), Filter::default());
    }

    /// **验收主用例**：直接读逆向规格 `filters.json`，用**另一套独立实现**推出应有的
    /// 标签集合，再和我们生成的 URL 逐组比对（排序后必须完全相同）。
    ///
    /// 没有规格文件就跳过（OSS 仓/别的机器不该因此变红）。
    #[test]
    fn filter_matches_reversed_spec() {
        let Some(spec) = load_we_filter_spec() else {
            eprintln!("没有 filters.json ⇒ 跳过规格对比（只在带 artifacts 的开发树里跑）");
            return;
        };
        // ① 词表与默认值：对我们每组每个选项逐条比
        let ours = spec_ours();
        assert_eq!(ours.len(), 4, "会进查询的是四组（类型/年龄/分辨率/标签）");
        for (key, mine) in &ours {
            let s = spec_group(&spec, key).expect("规格里应有这一组");
            assert_eq!(
                mine.tags, s.tags,
                "{key} 组的 tag 集合/顺序必须与逆向规格一致"
            );
            assert_eq!(
                mine.defaults, s.defaults,
                "{key} 组的默认勾选必须与逆向规格的 workshop 默认值一致"
            );
            // 逐组对比表（`--nocapture` 时打印，可直接贴进报告）
            let on = mine.defaults.iter().filter(|d| **d).count();
            let mut a = mine.tags.clone();
            let mut b = s.tags.clone();
            a.sort();
            b.sort();
            eprintln!(
                "  {key:10} 规格 {:>2} 项 / 我们 {:>2} 项 · 默认勾选 {}/{} · tag 集合相等 {}",
                s.tags.len(),
                mine.tags.len(),
                on,
                mine.tags.len(),
                a == b
            );
        }

        // ② 查询：几种典型状态，与我们**独立**从 JSON 推出来的集合比
        let cases: Vec<(&str, Vec<(&str, &str)>, bool)> = vec![
            // 默认状态
            ("默认", vec![], false),
            // 组内多选（同一组去勾两项 = OR）
            ("组内 Anime+Game", vec![(G_TAGS, "Anime"), (G_TAGS, "Game")], false),
            // 组间组合（类型 + 标签 各去勾一项 = AND）
            (
                "组间 Video + Anime",
                vec![(G_TYPE, "Video"), (G_TAGS, "Anime")],
                false,
            ),
            // 空组保底：整组年龄分级都不勾
            (
                "空组（年龄全不勾）",
                vec![
                    (G_RATING, "Everyone"),
                    (G_RATING, "Questionable"),
                    (G_RATING, "Mature"),
                ],
                false,
            ),
            // 分辨率子组"无" + 标签组"无"
            ("分辨率尾巴全不勾", vec![(G_RESOLUTION, "Other resolution"), (G_RESOLUTION, "Dynamic resolution")], false),
            // 仅显示侧（requiredtags）
            ("仅显示 Approved", vec![], true),
        ];

        for (name, unchecked, approved) in cases {
            let mut f = Filter::default();
            for (g, t) in &unchecked {
                assert!(f.set_checked(g, t, false), "{g}:{t} 应该是真标签");
            }
            if approved {
                f.set_only(ONLY_APPROVED, true);
            }
            let q = f.query();

            // 独立实现：规格侧从 JSON 重建同样的状态，再推导 excluded 集合
            let want = spec_expected_excluded(&spec, &unchecked);
            let mut got = q.excluded.clone();
            let mut want = want;
            got.sort();
            want.sort();
            eprintln!(
                "  {name:24} 规格推导 {} 项 / 我们 {} 项 · 集合相等 {}",
                want.len(),
                got.len(),
                got == want
            );
            assert_eq!(got, want, "【{name}】excludedtags 集合必须与规格推导完全一致");

            // 再核一遍 URL 上的真实参数（不只是内存里的 Vec）
            let url = browse_url("", 1, "trend", &q);
            let mut url_tags = url_param_values(&url, "excludedtags%5B%5D=");
            url_tags.sort();
            assert_eq!(url_tags, want, "【{name}】URL 里的 excludedtags 必须与规格推导一致");

            // requiredtags 侧：规格说只有 Approved / utility 会进
            let want_req: Vec<String> = if approved {
                vec!["Approved".to_string()]
            } else {
                vec![]
            };
            assert_eq!(q.required, want_req, "【{name}】requiredtags 侧");
            let url_req = url_param_values(&url, "requiredtags%5B%5D=");
            assert_eq!(url_req, want_req, "【{name}】URL 里的 requiredtags");
        }

        // ③ "重置"之后必须回到 ①:默认状态（与规格默认值逐项一致）
        let mut f = Filter::default();
        f.set_section(G_TAGS, "tags", false);
        f.set_section(G_RESOLUTION, "widescreen", false);
        f.set_only(ONLY_MOBILE, true);
        f.reset();
        assert_eq!(f, Filter::default(), "重置 = 回到规格里的默认值");
    }

    /// 从规格 JSON 读出来的"我们这一侧"的形状（测试内部用）。
    struct SpecTags {
        tags: Vec<String>,
        defaults: Vec<bool>,
    }

    /// `filters.json` 的路径（开发树里在 `artifacts/`；可用 `WPHUB_WE_FILTERS_SPEC` 覆盖）。
    fn load_we_filter_spec() -> Option<Value> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(p) = std::env::var("WPHUB_WE_FILTERS_SPEC") {
            candidates.push(PathBuf::from(p));
        }
        candidates.push(PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../artifacts/we-ui-filters-20261022/filters.json"
        )));
        candidates.push(PathBuf::from(
            "/home/dev/artifacts/we-ui-filters-20261022/filters.json",
        ));
        for path in candidates {
            if let Ok(text) = std::fs::read_to_string(&path) {
                match serde_json::from_str(&text) {
                    Ok(v) => return Some(v),
                    Err(e) => eprintln!("{} 解析失败：{e}", path.display()),
                }
            }
        }
        None
    }

    /// 规格里某一组的 `(tag 列表, workshop 默认勾选)`。
    ///
    /// 刻意**不复用**我们的 `GROUPS`：从 JSON 里按规格的结构（type.options +
    /// type.second_section.options / resolution.subgroups + ungrouped_options / …）自己拼。
    fn spec_group(spec: &Value, key: &str) -> Option<SpecTags> {
        let groups = spec.get("groups")?.as_array()?;
        let g = groups
            .iter()
            .find(|g| g.get("id").and_then(|v| v.as_str()) == Some(key))?;
        let mut tags = Vec::new();
        let mut defaults = Vec::new();
        let mut push = |o: &Value| {
            tags.push(o.get("tag")?.as_str()?.to_string());
            // workshop_default 优先（工坊源），否则 default
            let d = o
                .get("workshop_default")
                .or_else(|| o.get("default"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            defaults.push(d);
            Some(())
        };
        if key == G_RESOLUTION {
            // 23 个按 5 个子组的顺序，然后两个尾巴
            for sg in g.get("subgroups")?.as_array()? {
                for o in sg.get("options")?.as_array()? {
                    push(o)?;
                }
            }
            for o in g.get("ungrouped_options")?.as_array()? {
                push(o)?;
            }
        } else if key == G_TYPE {
            for o in g.get("options")?.as_array()? {
                push(o)?;
            }
            for o in g.get("second_section")?.get("options")?.as_array()? {
                push(o)?;
            }
        } else {
            for o in g.get("options")?.as_array()? {
                push(o)?;
            }
        }
        // 规格里标签组的顺序是 C++ 数组顺序（Unspecified 在开头），WE 渲染时把它挪到末尾
        if key == G_TAGS {
            if let Some(pos) = tags.iter().position(|t| t == "Unspecified") {
                let (t, d) = (tags.remove(pos), defaults.remove(pos));
                tags.push(t);
                defaults.push(d);
            }
        }
        Some(SpecTags { tags, defaults })
    }

    /// 我们这一侧的同一份形状（只比会进查询的四组；来源组是纯本地、规格里也没默认值）。
    fn spec_ours() -> Vec<(&'static str, SpecTags)> {
        GROUPS
            .iter()
            .filter(|g| g.query_backed)
            .map(|g| {
                (
                    g.key,
                    SpecTags {
                        tags: group_opts(g.key).map(|o| o.tag.to_string()).collect(),
                        defaults: group_opts(g.key).map(|o| o.default).collect(),
                    },
                )
            })
            .collect()
    }

    /// **独立实现**：从规格 JSON 推导"这些项没勾"时该有的 `excludedtags` 集合。
    ///
    /// 保底粒度按规格的 angular 模型对象算：typetags / categorytags / ratingtags /
    /// resolutiontags(23+2) / tags 各一个。
    fn spec_expected_excluded(spec: &Value, unchecked: &[(&str, &str)]) -> Vec<String> {
        let is_unchecked = |g: &str, t: &str| unchecked.iter().any(|(gg, tt)| *gg == g && *tt == t);
        let mut out: BTreeSet<String> = BTreeSet::new();
        // 每个单元：组的持久化键 + 规格里属于这个单元的 tag（带默认值）
        let mut units: Vec<(&str, Vec<(String, bool)>)> = Vec::new();
        for g in spec.get("groups").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
            let id = g.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let key = if id == "type(+category)" || id == "type" {
                G_TYPE
            } else {
                match id {
                    "rating" => G_RATING,
                    "resolution" => G_RESOLUTION,
                    "tags" => G_TAGS,
                    _ => continue, // show_only / source 不进 excludedtags 的可勾选单元
                }
            };
            let read = |arr: Option<&Vec<Value>>| -> Vec<(String, bool)> {
                arr.map(|a| {
                    a.iter()
                        .filter_map(|o| {
                            let t = o.get("tag")?.as_str()?.to_string();
                            let d = o
                                .get("workshop_default")
                                .or_else(|| o.get("default"))
                                .and_then(|v| v.as_bool())
                                .unwrap_or(true);
                            Some((t, d))
                        })
                        .collect()
                })
                .unwrap_or_default()
            };
            match id {
                "resolution" => {
                    // 5 个子组 + 尾巴 = **一个**单元
                    let mut all = Vec::new();
                    if let Some(sgs) = g.get("subgroups").and_then(|v| v.as_array()) {
                        for sg in sgs {
                            all.extend(read(sg.get("options").and_then(|v| v.as_array())));
                        }
                    }
                    all.extend(read(g.get("ungrouped_options").and_then(|v| v.as_array())));
                    units.push((key, all));
                }
                "type" | "type(+category)" => {
                    // typetags 与 categorytags 是**两个**单元
                    units.push((key, read(g.get("options").and_then(|v| v.as_array()))));
                    units.push((
                        key,
                        read(
                            g.get("second_section")
                                .and_then(|s| s.get("options"))
                                .and_then(|v| v.as_array()),
                        ),
                    ));
                }
                _ => units.push((key, read(g.get("options").and_then(|v| v.as_array())))),
            }
        }

        for (key, opts) in units {
            let mut any_checked = false;
            for (t, default_on) in opts {
                let on = default_on && !is_unchecked(key, &t);
                if on {
                    any_checked = true;
                } else {
                    out.insert(t);
                }
            }
            if !any_checked {
                out.insert(UNSPECIFIED_FALLBACK.to_string());
            }
        }
        out.into_iter().collect()
    }

    /// 从 URL 里取出某个参数的所有值（百分号解码 `%20`/`%5B%5D` 之外的常规转义）。
    fn url_param_values(url: &str, prefix: &str) -> Vec<String> {
        url.split('&')
            .filter_map(|kv| kv.strip_prefix(prefix))
            .map(|v| {
                v.replace("%20", " ")
                    .replace("%5B%5D", "[]")
                    .replace("%2F", "/")
            })
            .collect()
    }

    /// **真网络验收**（默认跳过）：把每种筛选状态的 URL 真打一次 Steam，印出
    /// `excludedtags` 集合 + `total_count`（= 筛选真的生效的硬证据）。
    ///
    /// 开法：`WPHUB_LIVE_FILTER=1 WPHUB_WORKSHOP_CACHE_DIR=… cargo test --release \
    ///        live_filter_urls -- --nocapture`（会走 `fetch_browse_page`，即产品代码那条路）
    #[test]
    fn live_filter_urls_against_steam() {
        if std::env::var("WPHUB_LIVE_FILTER").ok().as_deref() != Some("1") {
            eprintln!("跳过真网络验收（WPHUB_LIVE_FILTER=1 才跑）");
            return;
        }
        // (名字, 先去勾哪些项, 勾不勾 Approved, 最后是否点「重置过滤器」)
        let scenarios: Vec<(&str, Vec<(&str, &str)>, bool, bool)> = vec![
            ("① 默认（= WE 默认）", vec![], false, false),
            (
                "② 乱改一通后点「重置过滤器」",
                vec![(G_TAGS, "Anime"), (G_TYPE, "Application"), (G_RATING, "Mature")],
                true,
                true,
            ),
            ("③ 单组取消一项 tags:Anime", vec![(G_TAGS, "Anime")], false, false),
            (
                "④ 组内多选 Anime+Game",
                vec![(G_TAGS, "Anime"), (G_TAGS, "Game")],
                false,
                false,
            ),
            (
                "⑤ 组间 Video+Anime",
                vec![(G_TYPE, "Video"), (G_TAGS, "Anime")],
                false,
                false,
            ),
            (
                "⑥ 分辨率·宽屏=无",
                vec![
                    (G_RESOLUTION, "Standard Definition"),
                    (G_RESOLUTION, "1280 x 720"),
                    (G_RESOLUTION, "1366 x 768"),
                    (G_RESOLUTION, "1920 x 1080"),
                    (G_RESOLUTION, "2560 x 1440"),
                    (G_RESOLUTION, "3840 x 2160"),
                ],
                false,
                false,
            ),
            (
                "⑦ 空组（年龄全不勾）",
                vec![
                    (G_RATING, "Everyone"),
                    (G_RATING, "Questionable"),
                    (G_RATING, "Mature"),
                ],
                false,
                false,
            ),
            ("⑧ 仅显示 Approved", vec![], true, false),
        ];
        eprintln!("── 真网络：每条 URL 打一次 Steam（缓存目录 {}）", cache_root().display());
        for (name, unchecked, approved, reset_last) in scenarios {
            let mut f = Filter::default();
            for (g, t) in &unchecked {
                f.set_checked(g, t, false);
            }
            if approved {
                f.set_only(ONLY_APPROVED, true);
            }
            if reset_last {
                // WE 的「重置过滤器」：回到默认 + 套 defaultfilterconfig
                f.reset();
                assert_eq!(f, Filter::default(), "重置后必须等于 WE 默认筛选");
            }
            let q = f.query();
            let url = browse_url("", 1, "trend", &q);
            let shown = url.replace("%5B%5D", "[]").replace("%20", " ");
            let outcome = fetch_browse_page(&url);
            match outcome {
                Ok(p) => eprintln!(
                    "{name}\n  URL {shown}\n  excluded {} 项 → total_count={:?} · 本页 id {}",
                    q.excluded.len(),
                    p.total,
                    p.ids.len()
                ),
                Err(e) => {
                    eprintln!("{name}\n  URL {shown}\n  请求失败：{e}");
                    panic!("真网络验收失败：{e}");
                }
            }
        }
    }

    /// 并行的核心约定：**保序**（缩略图/条目的显示顺序不能乱）+ 并发数真的有上限。
    #[test]
    fn parallel_map_keeps_order_and_bounds_workers() {
        let items: Vec<usize> = (0..40).collect();
        let alive = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let out = parallel_map(&items, 4, |i| {
            let now = alive.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(2));
            alive.fetch_sub(1, Ordering::SeqCst);
            i * 2
        });
        assert_eq!(out, (0..40).map(|i| i * 2).collect::<Vec<_>>(), "结果必须保序");
        assert!(peak.load(Ordering::SeqCst) <= 4, "并发不能被超过：{}", peak.load(Ordering::SeqCst));
        assert!(peak.load(Ordering::SeqCst) >= 2, "这么慢的任务应该真的并起来了");
    }

    #[test]
    fn parallel_map_handles_empty_and_fewer_items_than_workers() {
        let none: Vec<u8> = Vec::new();
        assert!(parallel_map(&none, 8, |_| 1u8).is_empty());
        let two = vec![1u8, 2];
        assert_eq!(parallel_map(&two, 12, |v| *v as u32 + 1), vec![2, 3]);
    }

    /// 缓存：写过、能读回、过期就读不到（"过期"用 ttl=0 模拟）。
    #[test]
    fn cache_round_trips_and_honours_ttl() {
        let root = tmp_root("roundtrip");
        assert!(cache_read_at(&root, "thumbs", "123.img", 60).is_none(), "没写过就是 miss");
        cache_write_at(&root, "thumbs", "123.img", b"IMGDATA").unwrap();
        assert_eq!(
            cache_read_at(&root, "thumbs", "123.img", 60).as_deref(),
            Some(&b"IMGDATA"[..])
        );
        assert!(
            cache_read_at(&root, "thumbs", "123.img", 0).is_some(),
            "文件刚写出来，age=0 仍算新鲜"
        );
        // 空文件不许当成命中（半截/被截断的响应）
        cache_write_at(&root, "thumbs", "124.img", b"").unwrap();
        assert!(cache_read_at(&root, "thumbs", "124.img", 60).is_none());
        // 两个 kind 互不干扰
        assert!(cache_read_at(&root, "details", "123.img", 60).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cache_ttl_expires_old_entries() {
        let root = tmp_root("ttl");
        cache_write_at(&root, "browse", "abc.html", b"<html/>").unwrap();
        let path = cache_path(&root, "browse", "abc.html");
        assert!(fresh(&path, 300), "刚写的是新鲜的");
        // 把 mtime 拨回 10 分钟前：TTL 5 分钟的读不到（会重新请求），TTL 1 小时的能读到（旧的也能兜底）
        let old = SystemTime::now() - Duration::from_secs(600);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(!fresh(&path, 300), "超过 TTL 必须当作过期");
        assert!(
            cache_read_at(&root, "browse", "abc.html", 300).is_none(),
            "过期条目不许命中"
        );
        assert!(
            cache_read_at(&root, "browse", "abc.html", 3600).is_some(),
            "「写请求失败就退回旧缓存」那条路要靠它"
        );
        assert!(!fresh(&root.join("browse").join("nope.html"), 300), "不存在的文件 = 过期");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn housekeeping_drops_oldest_first() {
        let root = tmp_root("evict");
        for i in 0..5u8 {
            cache_write_at(&root, "thumbs", &format!("{i}.img"), &[b'x'; 10]).unwrap();
            // 拉开 mtime：靠 sleep 太慢，这里至少保证顺序稳定可断言条数
            std::thread::sleep(Duration::from_millis(2));
        }
        housekeeping_at(&root, 2, u64::MAX);
        let left = std::fs::read_dir(root.join("thumbs")).unwrap().count();
        assert_eq!(left, 2, "超出条数上限要删到上限内");
        // 字节上限：每张 10 字节 ⇒ 只留 2 张
        for i in 10..15u8 {
            cache_write_at(&root, "thumbs", &format!("{i}.img"), &[b'x'; 10]).unwrap();
        }
        housekeeping_at(&root, usize::MAX, 20);
        let left = std::fs::read_dir(root.join("thumbs")).unwrap().count();
        assert!(left <= 2, "超出字节上限也要删：剩 {left}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn retry_only_for_transient_statuses() {
        // 429/408/5xx 值得重试；其它 4xx（404/403/400）重试纯浪费
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(408));
        assert!(is_retryable_status(500) && is_retryable_status(503) && is_retryable_status(599));
        assert!(!is_retryable_status(400) && !is_retryable_status(403) && !is_retryable_status(404));
        assert!(!is_retryable_status(200));
    }

    /// 失败**只重试一次**（有上限），而且失败的这条只记账、不抛给调用方。
    #[test]
    fn with_retry_retries_exactly_once_then_counts_the_failure() {
        let calls = AtomicUsize::new(0);
        let mut c = Counters::default();
        let err = with_retry(
            "test-fail",
            Instant::now() + Duration::from_secs(30),
            &mut c,
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                Attempt::Retry {
                    why: "模拟超时".into(),
                    timeout: true,
                }
            },
        )
        .unwrap_err();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1 + cfg::WS_MAX_RETRIES,
            "总共试 1+重试次数 次"
        );
        assert_eq!(c.retries, cfg::WS_MAX_RETRIES);
        assert_eq!(c.failed, 1, "失败计数是 1，不是 2（重试不算第二次失败）");
        assert!(err.contains("模拟超时"));
    }

    #[test]
    fn with_retry_succeeds_on_the_second_attempt() {
        let calls = AtomicUsize::new(0);
        let mut c = Counters::default();
        let bytes = with_retry(
            "test-ok",
            Instant::now() + Duration::from_secs(30),
            &mut c,
            || {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Attempt::Retry {
                        why: "第一次假失败".into(),
                        timeout: true,
                    }
                } else {
                    Attempt::Bytes(b"OK".to_vec())
                }
            },
        )
        .unwrap();
        assert_eq!(bytes, b"OK");
        assert_eq!(c.retries, 1);
        assert_eq!(c.failed, 0);
        assert_eq!(c.timed_out, 1, "超时要单独计数（界面/日志要能看出是超时不是 404）");
    }

    /// 阶段预算用完（deadline 已过）⇒ **不再重试**，宁可少一条也不把整页拖住。
    #[test]
    fn with_retry_stops_retrying_once_the_phase_budget_is_gone() {
        let calls = AtomicUsize::new(0);
        let mut c = Counters::default();
        let _ = with_retry(
            "test-no-budget",
            Instant::now() - Duration::from_millis(1),
            &mut c,
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                Attempt::Retry {
                    why: "x".into(),
                    timeout: false,
                }
            },
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "预算没了就不该再试");
        assert_eq!(c.retries, 0);
        assert_eq!(c.failed, 1);
    }

    /// 永久错误（4xx 之类）不重试，直接跳过。
    #[test]
    fn fatal_errors_are_not_retried() {
        let calls = AtomicUsize::new(0);
        let mut c = Counters::default();
        let _ = with_retry(
            "test-fatal",
            Instant::now() + Duration::from_secs(30),
            &mut c,
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                Attempt::Fatal("404".into())
            },
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(c.retries, 0);
        assert_eq!(c.failed, 1);
    }

    /// 失败隔离的**端到端**验证：几条连不上的缩略图不会让整批失败、不会 panic，
    /// 只把自己记成 failed —— 界面照样能出条目、给"无预览"占位。
    /// （用 127.0.0.1:1 = 立刻 connection refused，不走外网。）
    #[test]
    fn broken_thumbnails_are_skipped_not_fatal() {
        let items: Vec<Item> = (0..3)
            .map(|i| Item {
                id: format!("test-broken-{}-{}", std::process::id(), i),
                title: "坏图".into(),
                preview_url: "http://127.0.0.1:1/definitely-not-here.jpg".into(),
                size_bytes: 0,
                tags: Vec::new(),
                subscriptions: 0,
                description: String::new(),
            })
            .collect();
        let mut stats = Stats::default();
        let thumbs = thumbs_cached(&items, 3, &mut stats);
        assert!(thumbs.is_empty(), "连不上就是拿不到图，不该有假数据");
        assert_eq!(stats.thumbs_failed, 3, "三条都要记成失败（跳过）");
        assert_eq!(stats.retries, 3, "每条都重试过一次（说明重试真的发生了）");
    }

    /// 渐进式填充的核心约定：**每拿到一张就回调一次**（缓存命中也要回调），
    /// 而且回调和最终返回的是同一批 —— 界面就是靠这个回调逐张补图的。
    #[test]
    fn streaming_thumbs_fire_one_callback_per_item_even_on_cache_hit() {
        let root = tmp_root("stream");
        // 预置两张到缓存 ⇒ 走 cache hit 分支，不需要联网也能确定性地触发回调
        cache_write_at(&root, "thumbs", "s-1.img", b"IMGA").unwrap();
        cache_write_at(&root, "thumbs", "s-2.img", b"IMGB").unwrap();
        let mk = |id: &str| Item {
            id: id.to_string(),
            title: "t".into(),
            preview_url: "http://127.0.0.1:1/x.jpg".into(),
            size_bytes: 0,
            tags: Vec::new(),
            subscriptions: 0,
            description: String::new(),
        };
        let items = vec![mk("s-1"), mk("s-2")];
        let seen = Mutex::new(Vec::<(String, Vec<u8>)>::new());
        let cb = |id: String, bytes: Vec<u8>| {
            seen.lock().expect("seen 不该被毒化").push((id, bytes));
        };
        let mut stats = Stats::default();
        let thumbs = thumbs_cached_at(&root, &items, items.len(), &mut stats, Some(&cb));
        assert_eq!(thumbs.len(), 2, "两张缓存命中都要拿到");
        let mut got = seen.into_inner().unwrap();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("s-1".to_string(), b"IMGA".to_vec()),
                ("s-2".to_string(), b"IMGB".to_vec())
            ],
            "每张都要单独回调一次（并行时回调顺序不保证，比较前排序）"
        );
        assert_eq!(stats.thumbs_cached, 2, "缓存命中也要算进统计");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `want` 决定最多下几张 —— 界面要"30 条全都要"就传 `items.len()`。
    #[test]
    fn streaming_respects_want_ceiling() {
        let root = tmp_root("stream-want");
        for i in 0..5u8 {
            cache_write_at(&root, "thumbs", &format!("w-{i}.img"), b"X").unwrap();
        }
        for i in 0..3u8 {
            cache_write_at(&root, "thumbs", &format!("x-{i}.img"), b"X").unwrap();
        }
        let items: Vec<Item> = (0..5u8)
            .map(|i| Item {
                id: format!("w-{i}"),
                title: "t".into(),
                preview_url: "http://127.0.0.1:1/x.jpg".into(),
                size_bytes: 0,
                tags: Vec::new(),
                subscriptions: 0,
                description: String::new(),
            })
            .chain((0..3u8).map(|i| Item {
                id: format!("x-{i}"),
                title: "t".into(),
                preview_url: "http://127.0.0.1:1/x.jpg".into(),
                size_bytes: 0,
                tags: Vec::new(),
                subscriptions: 0,
                description: String::new(),
            }))
            .collect();
        let mut stats = Stats::default();
        // 只取前 2 条 ⇒ 后 3 条哪怕有缓存也不该被下
        let thumbs = thumbs_cached_at(&root, &items, 2, &mut stats, None);
        assert_eq!(thumbs.len(), 2);
        assert_eq!(thumbs[0].0, "w-0");
        assert_eq!(thumbs[1].0, "w-1");
        assert_eq!(stats.thumbs_cached, 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 详情缓存里存的就是**原样的条目 JSON** ⇒ 命中与网络走同一条解析路径。
    #[test]
    fn cached_entry_json_parses_back_to_the_same_item() {
        let entry = serde_json::json!({
            "result": 1,
            "publishedfileid": "1195626192",
            "title": "夜空",
            "preview_url": "https://cdn.example/thumb.jpg",
            "file_size": "1234567",
            "subscriptions": 42,
            "tags": [{"tag": "风景"}, {"tag": "动画"}],
            "description": "<b>x</b>"
        });
        let item = parse_entry(&entry).expect("result=1 应能解析");
        assert_eq!(item.id, "1195626192");
        assert_eq!(item.title, "夜空");
        assert_eq!(item.size_bytes, 1_234_567);
        assert_eq!(item.subscriptions, 42);
        assert_eq!(item.tags, vec!["风景", "动画"]);

        let bytes = serde_json::to_vec(&entry).unwrap();
        let again = parse_entry(&serde_json::from_slice::<Value>(&bytes).unwrap()).unwrap();
        assert_eq!(again.id, item.id);
        assert_eq!(again.preview_url, item.preview_url);
        assert_eq!(again.tags, item.tags);
    }

    #[test]
    fn parse_entry_rejects_deleted_or_idless_items() {
        assert!(parse_entry(&serde_json::json!({"result": 9, "publishedfileid": "1"})).is_none());
        assert!(parse_entry(&serde_json::json!({"result": 1})).is_none());
        assert!(parse_entry(&serde_json::json!({})).is_none());
    }

    #[test]
    fn fnv_hash_is_stable_short_and_distinct() {
        let a = fnv1a_hex("https://steamcommunity.com/workshop/browse/?appid=431960&p=1");
        let b = fnv1a_hex("https://steamcommunity.com/workshop/browse/?appid=431960&p=2");
        assert_eq!(a.len(), 16);
        assert_ne!(a, b, "不同页签不能撞成同一个缓存文件");
        assert_eq!(a, fnv1a_hex("https://steamcommunity.com/workshop/browse/?appid=431960&p=1"));
    }

    #[test]
    fn stats_summary_mentions_every_phase() {
        let stats = Stats {
            browse_ms: 1750,
            details_ms: 1900,
            thumbs_ms: 2650,
            total_ms: 6300,
            ids: 30,
            link_occurrences: 60,
            duplicates: 0,
            items: 30,
            thumbs: 24,
            thumbs_cached: 0,
            details_cached: 0,
            details_failed: 1,
            thumbs_failed: 2,
            retries: 1,
            timed_out: 1,
            concurrency: cfg::WS_THUMB_CONCURRENCY,
            browse_cached: false,
            browse_stale: false,
            queries: 1,
            total_count: None,
        };
        let text = describe_stats(&stats);
        for needle in ["6300ms", "1750ms", "1900ms", "2650ms", "失败 1", "失败 2", "重试 1"] {
            assert!(text.contains(needle), "{text} 里应包含 {needle}");
        }
        // 状态条/日志要能把"页面里的重复"与"上屏列表的重复"分开说
        assert!(text.contains("链接 60 次"), "{text}");
        assert!(text.contains("唯一 id 30"), "{text}");
        assert!(text.contains("同 id 折叠 0"), "{text}");
        // 单组合 + 没有 total_count ⇒ 不印这两项（不制造噪声）
        assert!(!text.contains("筛选组合"), "{text}");
        assert!(!text.contains("工坊总条数"), "{text}");
    }

    /// 摘要里必须**看得见**"Steam 报的总条数" —— 这就是验收里"筛选真的生效了"的证据
    /// （默认筛选 1 960 215 条 vs 无筛选 2 923 776 条，见 REPORT 与本轮实测）。
    #[test]
    fn stats_summary_shows_total_count() {
        let stats = Stats {
            queries: 1,
            total_count: Some(1_960_215),
            ..Stats::default()
        };
        let text = describe_stats(&stats);
        assert!(text.contains("工坊总条数 1960215"), "{text}");
        assert!(!text.contains("筛选组合"), "一页只发一条查询，不再印组合数：{text}");
    }

    #[test]
    fn cache_root_follows_xdg_or_defaults_to_home_cache() {
        // 不设 WPHUB_WORKSHOP_CACHE_DIR 时给的是 ~/.cache/wallpaper-hub/workshop 形状的路径
        let root = cache_root();
        assert!(
            root.to_string_lossy().contains("wallpaper-hub"),
            "缓存路径要在自己的目录下：{}",
            root.display()
        );
        assert!(root.to_string_lossy().contains("workshop"));
    }

    #[test]
    fn count_and_byte_limits_do_not_delete_when_under_budget() {
        let root = tmp_root("nokeep");
        cache_write_at(&root, "thumbs", "1.img", b"abc").unwrap();
        housekeeping_at(&root, 100, 1000);
        assert!(
            root.join("thumbs").join("1.img").exists(),
            "没超上限就不该删"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
