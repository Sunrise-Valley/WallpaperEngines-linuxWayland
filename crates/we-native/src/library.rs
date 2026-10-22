// SPDX-License-Identifier: MIT
//
//! **本地壁纸库根目录**（内核侧）。
//!
//! 内核从控制面拿到的只有一个 `apply` 的 `path`。界面那边知道壁纸在哪，但它可能给
//! **id**（工坊那条老习惯：`…/workshop/content/431960/<id>`）而不是绝对路径 ——
//! 壁纸搬到别处（比如 `~/Templates`）之后，旧的那条路径就不存在了。所以内核自己也
//! 有一套同样的根目录表：路径不存在时按 id 去各根目录里找。
//!
//! 规则与界面侧 `wallpaper-hub/src/wp/library.rs` **完全一致**（内核不依赖 cosmic-config，
//! 但它读同一个环境变量和同一个状态文件 ⇒ 两边看到的根目录永远一样）：
//!
//! 1. 环境变量 `WPHUB_LIBRARY_DIRS`（冒号分隔，支持 `~`）
//! 2. 状态文件 `~/.local/share/wallpaper-hub/library_dirs`（**界面写、内核读**，
//!    每次 apply 现读 ⇒ 界面里换目录之后内核不用重启）
//! 3. 都没有 ⇒ 默认 `~/Templates`
//!
//! 另外 Steam 工坊目录（`…/steamapps/workshop/content/431960`）也总会加入候选
//! —— **存在就有、不存在就没有**，绝不报错、也绝不提示装 Steam。

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

/// `~/x` → `$HOME/x`。其余原样返回。
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

/// 默认的用户根目录：`~/Templates`。
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

/// `libraryfolders.vdf` 里的 `"path"  "…"` 行（额外的 Steam 库磁盘）。认不出来就当没有。
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

/// Steam 工坊目录的**候选**，只返回真的存在的那些。
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

/// 状态文件：界面写、内核读。
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

/// 用户自定义根目录（不含可选 Steam 源）。
pub fn user_roots() -> Vec<PathBuf> {
    if let Ok(v) = std::env::var(ENV_DIRS) {
        let dirs = parse_list(&v);
        if !dirs.is_empty() {
            return dirs;
        }
    }
    // 兼容旧开关：`WE_WORKSHOP_DIR` 现在只当**一个根目录**用。
    if let Ok(p) = std::env::var("WE_WORKSHOP_DIR") {
        if !p.trim().is_empty() {
            return vec![expand(&p)];
        }
    }
    let state = read_state();
    if !state.is_empty() {
        return state;
    }
    default_roots()
}

/// 全部扫描根目录 = 用户目录（在前）+ 可选的 Steam 目录（在后）。
pub fn roots() -> Vec<PathBuf> {
    let mut v = user_roots();
    v.extend(steam_roots());
    dedupe(v)
}

/// 在给定根目录里按 id（= 目录名）找壁纸目录。
pub fn resolve_id_in(roots: &[PathBuf], id: &str) -> Option<PathBuf> {
    let id = id.trim().trim_matches('/');
    if id.is_empty() || id.contains('/') || id == ".." || id == "." {
        return None;
    }
    roots.iter().map(|r| r.join(id)).find(|d| d.is_dir())
}

/// 按 id 找壁纸目录（用当前有效的根目录）。
pub fn resolve_id(id: &str) -> Option<PathBuf> {
    resolve_id_in(&roots(), id)
}

/// 控制面给的 `path` → 真正存在的路径。
///
/// * 路径本身存在 ⇒ 原样返回（**不动**用户/界面给的绝对路径）；
/// * 否则把它当**壁纸 id**（`<root>/<id>`）在各根目录里找 —— 壁纸从 Steam 工坊目录
///   搬到了 `~/Templates`、界面还发着旧路径时，这条兜底就能救回来；
/// * 都没有 ⇒ `None`（调用方照旧报"路径不存在"）。
pub fn resolve_source(raw: &str) -> Option<PathBuf> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let p = PathBuf::from(s);
    if p.exists() {
        return Some(p);
    }
    let id = Path::new(s.trim_end_matches('/'))
        .file_name()?
        .to_string_lossy()
        .to_string();
    resolve_id(&id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("we-lib-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn default_root_is_templates() {
        let d = default_roots();
        assert_eq!(d.len(), 1);
        assert!(d[0].ends_with(DEFAULT_DIR_NAME));
    }

    #[test]
    fn parse_list_splits_colon_and_newline() {
        assert_eq!(
            parse_list("~/a:/opt/b\n# c\n/opt/d"),
            vec![home().join("a"), PathBuf::from("/opt/b"), PathBuf::from("/opt/d")]
        );
    }

    #[test]
    fn resolve_id_finds_dir_in_any_root() {
        let a = tmpdir("a");
        let b = tmpdir("b");
        std::fs::create_dir_all(a.join("111")).unwrap();
        std::fs::create_dir_all(b.join("3704273480")).unwrap();
        let roots = vec![a.clone(), b.clone()];
        assert_eq!(resolve_id_in(&roots, "111"), Some(a.join("111")));
        assert_eq!(
            resolve_id_in(&roots, "3704273480"),
            Some(b.join("3704273480")),
            "壁纸搬到第二个根目录（~/Templates）后也要找得到"
        );
        assert_eq!(resolve_id_in(&roots, "nope"), None);
        assert_eq!(resolve_id_in(&roots, "../x"), None);
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn resolve_source_keeps_existing_paths_and_rescues_stale_ones() {
        let root = tmpdir("src");
        let d = root.join("3704273480");
        std::fs::create_dir_all(&d).unwrap();
        // 1) 存在的路径原样返回（不做什么"聪明"的重写）
        assert_eq!(resolve_source(&*d.to_string_lossy()), Some(d.clone()));
        // 2) 旧路径（Steam 里已经搬走）⇒ 按 id 到根目录里捞回来
        unsafe { std::env::set_var(ENV_DIRS, root.to_string_lossy().to_string()) };
        let stale = "/home/user/.local/share/Steam/steamapps/workshop/content/431960/3704273480";
        assert_eq!(resolve_source(stale), Some(d.clone()), "旧路径不存在时按 id 兜底");
        assert_eq!(resolve_source("3704273480"), Some(d.clone()), "只给 id 也行");
        assert_eq!(resolve_source("9999999999"), None);
        assert_eq!(resolve_source(""), None);
        assert_eq!(resolve_source("   "), None);
        unsafe { std::env::remove_var(ENV_DIRS) };
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn steam_roots_only_existing_dirs() {
        for p in steam_roots() {
            assert!(p.is_dir());
        }
    }
}
