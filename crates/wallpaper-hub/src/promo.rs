// SPDX-License-Identifier: MIT
//
//! 作者嵌入的**宣传图**：从壁纸自己的工程文件里**遍历**找出来，不按壁纸名特判。
//!
//! ## 数据来源清单（实测，本机 17 条工坊内容）
//!
//! 1. **`project.json` → `general.properties[*].text` 里的 HTML / BBCode 图片** —— 真·宣传图。
//! 作者把 `<img src="…">` 写进属性的 `text`（WE 的属性面板把这段当富文本渲染，
//! 上次本程序用 `strip_markup` 把它整段删掉了 ⇒ 用户看不到）。
//! 实测：`2887537617`（麻匪 礼服斯卡蒂）**7 张**、`3704273480`（麻匪 夏日影）**7 张**，
//! 全是 `https://i.ibb.co/…` 外链。**用户反馈里那张图就是这个来源。**
//!
//! 2. **`project.json` → `description` 里的 `[img]URL[/img]`** —— 描述里的宣传图。
//! 实测：`3299815325`（Shinobu Kocho）1 张（kym-cdn 外链）。
//!
//! 3. **`project.json` → `preview` 字段** → 目录里的 `preview.jpg` / `preview.gif` —— 那是**封面**，
//! 17 条全都有；界面已经拿它当详情面板顶部的大图，**不算"宣传图"**，本模块不碰它。
//!
//! 4. **`scene.pkg` 里的 `"preview": "preview/project.json"`** —— 实测**不是**图片资源：
//! 它是 WE 场景预览相机的工程引用（`camerapreview` 那一组配置）；
//! `strings scene.pkg | grep preview/` 只匹配到这些 JSON 键值，pkg 的目录树里没有
//! `preview/` 图片（`2887537617` 解包出来也只有 effects / materials / models / particles /
//! scene.json / shaders）。⇒ **不作为宣传图来源**。
//! 结论：**宣传图 = 工程文件文本里嵌入的图片引用**。遍历 `general.properties`（**每条属性**都看）
//! + `description`，认三种写法：`<img src=…>` / `[img]…[/img]` / `[img=…]`。
//! 引用是本地相对路径就直接读盘；是 http(s) 外链就把图下回来缓存到
//! `~/.cache/wallpaper-hub/promo/`（以 URL 的 FNV-1a 哈希命名）。
//!
//! ## 并发与超时
//!
//! 宣传图数量少（实测最多 7 张）、体积小（73 KB ~ 680 KB），所以**按顺序**下、每张一个
//! 全局超时，一张失败不影响其余。下载全在后台线程，UI 线程一字节都不碰。

use std::path::{Path, PathBuf};
use std::time::Duration;

/// 一张宣传图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Promo {
    /// 原始引用（外链 URL，或相对壁纸目录的路径）
    pub src: String,
    /// 本地文件（能直接喂给 image 组件）
    pub local: Option<PathBuf>,
}

impl Promo {
    /// 是外链（需要下载）。
    pub fn is_remote(&self) -> bool {
        self.local.is_none()
    }
}

/// 单张图的大小上限（实测最大 680 KB，留 8 MB 余量足够）
const MAX_BYTES: u64 = 8 * 1024 * 1024;
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36";

// ── 抽取 ──────────────────────────────────────────────────────────────

/// 从一份 `project.json` 文本里抽出宣传图引用。
///
/// **遍历式**：`general.properties` 里**每一条**属性都看它的 `text`（不看属性名、不看类型），
/// 外加顶层 `description`。顺序 = 文件里的顺序。
pub fn extract(project_json: &str, dir: &Path) -> Vec<Promo> {
    let mut refs: Vec<String> = Vec::new();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(project_json) {
        if let Some(d) = v.get("description").and_then(|d| d.as_str()) {
            refs.extend(img_refs(d));
        }
        if let Some(props) = v
            .get("general")
            .and_then(|g| g.get("properties"))
            .and_then(|p| p.as_object())
        {
            for p in props.values() {
                if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                    refs.extend(img_refs(t));
                }
            }
        }
    }

    let mut out: Vec<Promo> = Vec::new();
    for raw in refs {
        let s = normalize_ref(&raw);
        if s.is_empty() {
            continue;
        }
        let remote = is_remote_ref(&s);
        let local = if remote {
            None
        } else {
            let p = if Path::new(&s).is_absolute() {
                PathBuf::from(&s)
            } else {
                dir.join(&s)
            };
            if p.is_file() { Some(p) } else { None }
        };
        // 既不是外链、本地又找不到 ⇒ 丢掉（别给界面一个必然加载失败的引用）
        if !remote && local.is_none() {
            continue;
        }
        if !out.iter().any(|p| p.src == s) {
            out.push(Promo { src: s, local });
        }
    }
    out
}

/// 引用串的清洗：去空白、去包裹引号、去 JSON 转义残留的反斜杠。
fn normalize_ref(raw: &str) -> String {
    raw.trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim_end_matches('\\')
        .trim()
        .to_string()
}

/// 是不是外链。
pub fn is_remote_ref(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://") || l.starts_with("//")
}

/// 从一段文本里抽出所有图片引用（纯函数，好单测）。
///
/// 认三种写法：
///
/// * HTML：`<img src="…">` / `<img src='…'>`（属性顺序不限、大小写不限）
///
/// * BBCode：`[img]…[/img]`
///
/// * BBCode 带参：`[img=…]`
pub fn img_refs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let lower = text.to_ascii_lowercase();

    // ── HTML `<img …>` ──
    let mut i = 0usize;
    while let Some(rel) = lower[i..].find("<img") {
        let start = i + rel;
        let end = lower[start..]
            .find('>')
            .map(|e| start + e)
            .unwrap_or(text.len());
        if let Some(v) = attr(&text[start..end], "src") {
            out.push(v);
        }
        i = end.saturating_add(1).max(start + 4);
        if i >= text.len() {
            break;
        }
    }

    // ── BBCode `[img]…[/img]` / `[img=…]` ──
    let mut j = 0usize;
    while let Some(rel) = lower[j..].find("[img") {
        let start = j + rel;
        let after = start + 4;
        if after >= text.len() {
            break;
        }
        if lower.as_bytes()[after] == b'=' {
            let close = lower[after..]
                .find(']')
                .map(|c| after + c)
                .unwrap_or(text.len());
            out.push(text[after + 1..close].to_string());
            j = close.saturating_add(1);
        } else if lower.as_bytes()[after] == b']' {
            let body = after + 1;
            let close = lower[body..]
                .find("[/img]")
                .map(|c| body + c)
                .unwrap_or(text.len());
            out.push(text[body..close].to_string());
            j = close.saturating_add(1);
        } else {
            // `[image]` 之类：跳过这个 `[img` 继续找
            j = after;
        }
        if j >= text.len() {
            break;
        }
    }

    out
}

/// 在一个 HTML 标签里取属性值（`name="v"` / `name='v'` / `name=v`，`=` 两边可有空格）。
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let bytes = tag.as_bytes();
    let mut i = 0usize;
    while let Some(rel) = lower[i..].find(name) {
        let pos = i + rel;
        let after = pos + name.len();
        let boundary_before = pos == 0 || bytes[pos - 1].is_ascii_whitespace();
        let rest = match tag.get(after..) {
            Some(r) => r.trim_start(),
            None => return None,
        };
        if boundary_before {
            if let Some(v) = rest.strip_prefix('=') {
                let v = v.trim_start();
                let value = if let Some(s) = v.strip_prefix('"') {
                    s.split('"').next().unwrap_or("")
                } else if let Some(s) = v.strip_prefix('\'') {
                    s.split('\'').next().unwrap_or("")
                } else {
                    v.split(|c: char| c.is_whitespace() || c == '>' || c == '/')
                        .next()
                        .unwrap_or("")
                };
                return Some(value.to_string());
            }
        }
        i = after;
        if i >= tag.len() {
            break;
        }
    }
    None
}

// ── 外链下载 + 磁盘缓存 ────────────────────────────────────────────────

fn cache_dir() -> PathBuf {
    let base = match std::env::var("XDG_CACHE_HOME") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".cache"),
    };
    base.join("wallpaper-hub").join("promo")
}

/// URL → 缓存文件名（FNV-1a 64 位十六进制；纯函数，可单测）。
pub fn cache_file(url: &str) -> PathBuf {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in url.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    cache_dir().join(format!("{h:016x}"))
}

/// 缓存里有没有（有就直接用，不打网络）。
pub fn cached(url: &str) -> Option<Vec<u8>> {
    std::fs::read(cache_file(url)).ok().filter(|b| !b.is_empty())
}

fn agent() -> &'static ureq::Agent {
    static A: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    A.get_or_init(|| {
        ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .user_agent(UA)
                .timeout_connect(Some(Duration::from_secs(8)))
                .timeout_recv_response(Some(Duration::from_secs(10)))
                .timeout_recv_body(Some(Duration::from_secs(20)))
                .timeout_global(Some(Duration::from_secs(30)))
                .max_idle_connections(2)
                .max_idle_connections_per_host(2)
                .build(),
        )
    })
}

/// 拿一张外链图：**先看缓存**，没有才下；下到就写缓存（临时文件 + rename，中途被杀不会留半截）。
/// 失败返回 `None`，调用方跳过这一张即可。
pub fn fetch_remote(url: &str) -> Option<Vec<u8>> {
    if let Some(b) = cached(url) {
        return Some(b);
    }
    let bytes = match agent().get(url).call() {
        Ok(mut resp) => match resp.body_mut().with_config().limit(MAX_BYTES).read_to_vec() {
            Ok(b) if !b.is_empty() => b,
            Ok(_) => return None,
            Err(e) => {
                eprintln!("[promo] 读响应体失败 {url}: {e}");
                return None;
            }
        },
        Err(e) => {
            eprintln!("[promo] 下载失败 {url}: {e}");
            return None;
        }
    };
    let path = cache_file(url);
    if let Some(dir) = path.parent()
        && std::fs::create_dir_all(dir).is_ok()
    {
        let tmp = path.with_extension("part");
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 麻匪 礼服斯卡蒂（2887537617）真实 project.json 的片段。
    const MAFEI: &str = r#"{
        "title": "麻匪 礼服斯卡蒂",
        "type": "scene",
        "preview": "preview.gif",
        "description": "动态壁纸制作\n哔哩哔哩：Hope麻匪",
        "general": { "properties": {
            "gexing": { "index": 6, "text": "<br/><br><a href=''><img src=\"https://i.ibb.co/nNdQ1qyJ/image.gif\" width=\"100%\"></a><br><br><br>", "type": "text", "value": false },
            "logo": { "index": 2, "text": "<center> <a href='https://space.bilibili.com/111174060/'>  <img src=\"https://i.ibb.co/6hsBG5h/image.gif\" width=\"100%\" /> </a> </center>", "type": "text" },
            "newproperty": { "index": 7, "text": "右腿丝袜", "type": "bool", "value": false },
            "speed": { "index": 9, "text": "速度", "type": "slider", "value": 1 }
        } }
    }"#;

    #[test]
    fn finds_img_tags_and_bbcode() {
        assert_eq!(
            img_refs("<img src=\"https://a/b.png\" width=\"100%\">"),
            vec!["https://a/b.png".to_string()]
        );
        assert_eq!(
            img_refs("<img width='4' SRC='https://a/c.gif' />"),
            vec!["https://a/c.gif".to_string()]
        );
        assert_eq!(
            img_refs("看这个 [img]https://a/d.png[/img] 还有 [img=https://a/e.png]"),
            vec!["https://a/d.png".to_string(), "https://a/e.png".to_string()]
        );
    }

    #[test]
    fn img_refs_survives_junk_and_empty() {
        assert!(img_refs("").is_empty());
        assert!(img_refs("没有任何图片").is_empty());
        assert!(img_refs("[image]not-a-tag[/image]").is_empty());
        assert!(img_refs("<img class=\"x\">").is_empty(), "没有 src 就不算");
    }

    /// 真实条目：7 张里有 2 张在这段片段里，且**不按名字特判** —— 换个属性名一样能抓到。
    #[test]
    fn extracts_remote_promo_from_real_mafei_project() {
        let dir = Path::new("/nonexistent");
        let got = extract(MAFEI, dir);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].src, "https://i.ibb.co/nNdQ1qyJ/image.gif");
        assert_eq!(got[1].src, "https://i.ibb.co/6hsBG5h/image.gif");
        assert!(got.iter().all(|p| p.is_remote()));
        // 「preview」字段是封面，不该混进来
        assert!(!got.iter().any(|p| p.src.contains("preview.gif")));
    }

    /// 本地相对路径直接落到盘上；找不到的引用丢掉。
    #[test]
    fn resolves_local_refs_against_the_wallpaper_dir() {
        let dir = std::env::temp_dir().join(format!("wh-promo-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("banner.png"), b"\x89PNG").unwrap();
        let json = r#"{"general":{"properties":{
            "a":{"type":"text","text":"<img src=\"banner.png\">"},
            "b":{"type":"text","text":"<img src=\"missing.png\">"}
        }}}"#;
        let got = extract(json, &dir);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].local.as_deref(), Some(dir.join("banner.png").as_path()));
        assert!(!got[0].is_remote());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 同一张图在多个属性里出现 ⇒ 只留一份（顺序按首次出现）。
    #[test]
    fn dedupes_repeated_refs() {
        let json = r#"{"general":{"properties":{
            "a":{"type":"text","text":"<img src=\"https://x/1.png\">"},
            "b":{"type":"text","text":"[img]https://x/1.png[/img]"},
            "c":{"type":"text","text":"<img src=\"https://x/2.png\">"}
        }}}"#;
        let got = extract(json, Path::new("/nonexistent"));
        assert_eq!(
            got.iter().map(|p| p.src.as_str()).collect::<Vec<_>>(),
            vec!["https://x/1.png", "https://x/2.png"]
        );
    }

    /// 描述里的宣传图也算（实测 3299815325 就是这种）。
    #[test]
    fn picks_up_description_images() {
        let json = r#"{"description":"[img] https://i.kym-cdn.com/photos/images/x.gif [/img] 标题", "general":{"properties":{}}}"#;
        let got = extract(json, Path::new("/nonexistent"));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].src, "https://i.kym-cdn.com/photos/images/x.gif");
    }

    #[test]
    fn cache_file_is_stable_and_url_specific() {
        let a = cache_file("https://i.ibb.co/6hsBG5h/image.gif");
        let b = cache_file("https://i.ibb.co/6hsBG5h/image.gif");
        let c = cache_file("https://i.ibb.co/nNdQ1qyJ/image.gif");
        assert_eq!(a, b, "同一个 URL 永远同一个缓存文件");
        assert_ne!(a, c, "不同 URL 不能撞同一个缓存文件");
        assert_eq!(a.file_name().unwrap().to_string_lossy().len(), 16, "哈希部分 16 位十六进制");
    }

    #[test]
    fn recognizes_remote_forms() {
        assert!(is_remote_ref("https://a/b.png"));
        assert!(is_remote_ref("HTTP://A/B.PNG"));
        assert!(is_remote_ref("//cdn.example/x.gif"));
        assert!(!is_remote_ref("preview/banner.png"));
        assert!(!is_remote_ref("/home/user/x.jpg"));
    }
}

#[cfg(test)]
mod real_data_tests {
    use super::*;

    /// 拿本机真实工坊内容跑一遍（**没装那条就跳过**）：确认「麻匪 礼服斯卡蒂」的
    /// 宣传图能被抽出来、且都是外链。
    #[test]
    fn real_workshop_project_yields_remote_promo_when_present() {
        let dir = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
            .join(".local/share/Steam/steamapps/workshop/content/431960/2887537617");
        let Ok(js) = std::fs::read_to_string(dir.join("project.json")) else {
            eprintln!("跳过：本机没有 2887537617 的 project.json");
            return;
        };
        let got = extract(&js, &dir);
        assert!(!got.is_empty(), "真实条目里应该能抽到宣传图");
        assert!(
            got.iter().all(|p| p.is_remote()),
            "这条的宣传图都是外链：{got:?}"
        );
        // 同一张图在多个属性里出现 ⇒ 只留一份（这条实测 7 处引用、去重后 6 张）
        let mut srcs: Vec<&str> = got.iter().map(|p| p.src.as_str()).collect();
        let n = srcs.len();
        srcs.sort_unstable();
        srcs.dedup();
        assert_eq!(srcs.len(), n, "不该有重复引用");
    }
}
