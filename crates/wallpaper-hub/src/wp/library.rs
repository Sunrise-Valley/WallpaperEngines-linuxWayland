// SPDX-License-Identifier: MIT
//
//! **本地壁纸库根目录**（library roots）。
//!
//! 这个程序**不需要 Steam 客户端**：壁纸就是磁盘上的一堆目录（`project.json` +
//! `scene.pkg`），放在哪儿都能扫、都能播。Steam 只是**一个（可选的）扫描源**。
//!
//! 扫描源分两类：
//!
//! 1. **用户自定义根目录**（正式概念，主来源）：每个根目录下的子目录各算一条壁纸
//!    —— `<root>/<工坊 id>/`，或 `<root>/<任意名字>/`（显示名取 `project.json` 的
//!    `title`）。根目录**本身**就直接是一个工程（根下就有 `project.json`）也支持。
//!    默认根目录是 `~/Templates`。
//! 2. **Steam 工坊目录**（**可选**）：`…/steamapps/workshop/content/431960`。
//!    存在就一并扫，不存在就是空表 —— **不报错、也不提示用户去装 Steam**。
//!
//! 用户自定义根目录的来源（优先级从高到低）：
//!
//! * 环境变量 [`ENV_DIRS`]（`WPHUB_LIBRARY_DIRS`，冒号分隔，支持 `~`）
//! * 界面配置的 `library_dirs` 列表（cosmic-config）
//! * 状态文件 [`state_file`]（一行一个；**界面写、内核读** ⇒ 界面里改了目录之后，
//!   常驻内核不用重启也能按 id 找到新位置）
//! * 都没有 ⇒ [`default_roots`] = `~/Templates`
//!
//! ⚠ 内核侧（`we-native` 的 `library.rs`）有一份**规则完全相同**的实现：内核不能依赖
//! cosmic-config，但它读同一个环境变量 + 同一个状态文件 ⇒ 两边永远看到同一组根目录。

use std::path::{Path, PathBuf};

/// 自定义根目录的环境变量（冒号分隔）。
pub const ENV_DIRS: &str = "WPHUB_LIBRARY_DIRS";
/// 默认根目录名（`$HOME/Templates`）。
pub const DEFAULT_DIR_NAME: &str = "Templates";
/// Wallpaper Engine 的 Steam appid —— 只用于拼工坊目录路径。
pub const WE_APPID: u32 = 431960;

fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// `~/x` → `$HOME/x`。其余原样返回（相对路径按当前目录理解，不做额外猜测）。
pub fn expand(raw: &str) -> PathBuf {
    let s = raw.trim();
    if s == "~" {
        home()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(s)
    }
}

/// 默认的用户根目录：`~/Templates`（用户选定的壁纸存放处）。
pub fn default_roots() -> Vec<PathBuf> {
    vec![home().join(DEFAULT_DIR_NAME)]
}

fn dedupe(v: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(v.len());
    for p in v {
        if !p.as_os_str().is_empty() && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// `libraryfolders.vdf` 里的 `"path"  "…"` 行（额外的 Steam 库磁盘）。
/// 只认这一种形状，解析不出来就当没有 —— 绝不猜。
fn vdf_library_paths(vdf: &Path) -> Vec<PathBuf> {
    let Ok(s) = std::fs::read_to_string(vdf) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in s.lines() {
        let l = line.trim();
        let Some(rest) = l.strip_prefix("\"path\"") else {
            continue;
        };
        let Some(rest) = rest.trim().strip_prefix('"') else {
            continue;
        };
        let Some(end) = rest.find('"') else {
            continue;
        };
        let p = rest[..end].trim();
        if !p.is_empty() {
            out.push(PathBuf::from(p));
        }
    }
    out
}

fn workshop_subdir() -> PathBuf {
    Path::new("steamapps/workshop/content").join(WE_APPID.to_string())
}

/// Steam 工坊目录的**候选**，只返回真的存在的那些（这就是"存在就扫"的全部含义）。
pub fn steam_roots() -> Vec<PathBuf> {
    let h = home();
    let mut cands = vec![
        h.join(".local/share/Steam").join(workshop_subdir()),
        h.join(".steam/steam").join(workshop_subdir()),
        h.join(".var/app/com.valvesoftware.Steam/.local/share/Steam")
            .join(workshop_subdir()),
    ];
    for lib in vdf_library_paths(&h.join(".local/share/Steam/steamapps/libraryfolders.vdf")) {
        cands.push(lib.join(workshop_subdir()));
    }
    cands.retain(|p| p.is_dir());
    dedupe(cands)
}

/// 把一段文本切成目录表：冒号 / 换行 / 逗号都算分隔符，`#` 开头的行当注释。
/// （冒号是主分隔符；换行与逗号只是宽容，方便手写状态文件。）
pub fn parse_list(s: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for part in s.split([':', '\n', ',']) {
        let p = part.trim();
        if p.is_empty() || p.starts_with('#') {
            continue;
        }
        out.push(expand(p));
    }
    dedupe(out)
}

/// 状态文件：**界面写、内核读**，用来把有效根目录告诉常驻内核。
pub fn state_file() -> PathBuf {
    home().join(".local/share/wallpaper-hub/library_dirs")
}

pub fn read_state_from(path: &Path) -> Vec<PathBuf> {
    match std::fs::read_to_string(path) {
        Ok(s) => parse_list(&s),
        Err(_) => Vec::new(),
    }
}

pub fn read_state() -> Vec<PathBuf> {
    read_state_from(&state_file())
}

pub fn write_state_to(path: &Path, dirs: &[PathBuf]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut s = String::from(
        "# wallpaper-hub 本地壁纸根目录（一行一个）。由界面自动生成，改这里内核立刻生效。\n\
         # 环境变量 WPHUB_LIBRARY_DIRS 会整体覆盖本文件。\n",
    );
    for d in dirs {
        s.push_str(&d.to_string_lossy());
        s.push('\n');
    }
    std::fs::write(path, s)
}

/// 有效根目录（**写回**的时候用它：连 Steam 源一起写过去，内核看到的就是界面看到的）。
pub fn write_state(dirs: &[PathBuf]) -> std::io::Result<()> {
    write_state_to(&state_file(), dirs)
}

/// 用户自定义根目录（不含可选 Steam 源）。
pub fn user_roots(configured: &[String]) -> Vec<PathBuf> {
    if let Ok(v) = std::env::var(ENV_DIRS) {
        let dirs = parse_list(&v);
        if !dirs.is_empty() {
            return dirs;
        }
    }
    // 兼容旧开关：`WE_WORKSHOP_DIR` 过去是"唯一的工坊内容目录"，现在只当**一个根目录**用
    // （它是工坊内容目录形状 ⇒ 子目录就是各条壁纸，扫法完全一样）。
    if let Ok(p) = std::env::var("WE_WORKSHOP_DIR")
        && !p.trim().is_empty()
    {
        return vec![expand(&p)];
    }
    let cfg: Vec<PathBuf> = configured
        .iter()
        .map(|s| expand(s))
        .filter(|p| !p.as_os_str().is_empty())
        .collect();
    if !cfg.is_empty() {
        return dedupe(cfg);
    }
    // 配置空着时才看状态文件：这样"手工改 library_dirs"也能生效（界面刷新时会再写回去）
    let state = read_state();
    if !state.is_empty() {
        return state;
    }
    default_roots()
}

/// 全部扫描根目录 = 用户目录（在前）+ 可选的 Steam 目录（在后）。
/// 前面优先：同一个 id 在两处都出现时，用户目录那份胜出。
pub fn roots(configured: &[String]) -> Vec<PathBuf> {
    let mut v = user_roots(configured);
    v.extend(steam_roots());
    dedupe(v)
}

/// 在给定根目录里按 id（= 目录名）找壁纸目录。
pub fn resolve_id_in(roots: &[PathBuf], id: &str) -> Option<PathBuf> {
    let id = id.trim().trim_matches('/');
    // id 必须是**单层目录名**：带分隔符或 `..` 的一律不当 id（避免越界到别的目录）
    if id.is_empty() || id.contains('/') || id == ".." || id == "." {
        return None;
    }
    roots.iter().map(|r| r.join(id)).find(|d| d.is_dir())
}

/// 按 id 找壁纸目录（扫默认的那组根目录）。
pub fn resolve_id(id: &str) -> Option<PathBuf> {
    resolve_id_in(&roots(&[]), id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wh-lib-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn default_root_is_templates() {
        let d = default_roots();
        assert_eq!(d.len(), 1);
        assert!(d[0].ends_with(DEFAULT_DIR_NAME), "{}", d[0].display());
    }

    #[test]
    fn expand_handles_tilde_and_absolute() {
        let h = home();
        assert_eq!(expand("~/Templates"), h.join("Templates"));
        assert_eq!(expand("~"), h);
        assert_eq!(expand("/opt/walls"), PathBuf::from("/opt/walls"));
        assert_eq!(expand("  /opt/walls  "), PathBuf::from("/opt/walls"), "两端空白要切掉");
    }

    #[test]
    fn parse_list_splits_and_skips_noise() {
        let v = parse_list("# 注释\n/opt/a:/opt/b\n/opt/c,,\n");
        assert_eq!(v, vec![PathBuf::from("/opt/a"), PathBuf::from("/opt/b"), PathBuf::from("/opt/c")]);
        assert_eq!(parse_list("   "), Vec::<PathBuf>::new());
        // 重复项只留一份（否则同一批壁纸会扫两遍）
        assert_eq!(parse_list("/opt/a:/opt/a"), vec![PathBuf::from("/opt/a")]);
    }

    #[test]
    fn state_file_roundtrip() {
        let root = tmpdir("state");
        let f = root.join("library_dirs");
        let dirs = vec![root.join("Templates"), PathBuf::from("/opt/walls")];
        write_state_to(&f, &dirs).unwrap();
        assert_eq!(read_state_from(&f), dirs, "写进去什么就读回什么");
        // 不存在的文件 = 空表，不是错误
        assert_eq!(read_state_from(&root.join("nope")), Vec::<PathBuf>::new());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_id_finds_any_root_and_rejects_escapes() {
        let a = tmpdir("resolve-a");
        let b = tmpdir("resolve-b");
        std::fs::create_dir_all(a.join("111")).unwrap();
        std::fs::create_dir_all(b.join("222")).unwrap();
        let roots = vec![a.clone(), b.clone()];
        assert_eq!(resolve_id_in(&roots, "111"), Some(a.join("111")));
        assert_eq!(resolve_id_in(&roots, "222"), Some(b.join("222")), "第二个根目录也要找");
        assert_eq!(resolve_id_in(&roots, "333"), None);
        assert_eq!(resolve_id_in(&roots, "/111"), Some(a.join("111")), "两端斜杠要容忍");
        assert_eq!(resolve_id_in(&roots, "../etc"), None, "多段路径不当 id");
        assert_eq!(resolve_id_in(&roots, ""), None);
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn steam_roots_only_existing_dirs() {
        // 机器上装没装 Steam 都成立：返回的每一项都必须真的存在
        for p in steam_roots() {
            assert!(p.is_dir(), "只返回存在的目录：{}", p.display());
        }
    }

    #[test]
    fn roots_put_user_dirs_first() {
        let cfg = vec!["/opt/walls".to_string()];
        let r = roots(&cfg);
        assert_eq!(r[0], PathBuf::from("/opt/walls"), "用户目录优先");
        assert!(r.len() == 1 || r[1..].iter().all(|p| p.is_dir()));
    }
}
