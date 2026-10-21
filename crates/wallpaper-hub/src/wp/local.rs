// SPDX-License-Identifier: MIT
//
//! 本地壁纸库：**不经过 waywallen daemon**，直接扫 Steam 工坊目录建列表。
//!
//! 为什么需要它：daemon 是 waywallen 的一部分，重启后不会自动起来（我们是
//! `systemd-run` 拉的瞬态服务），而界面里那份列表**只有 daemon 一个来源**
//! ⇒ daemon 一停，界面就"未连接 + 列表空"。自研内核（we-native）本来就自己
//! 读 `scene.pkg`、自己渲染，**列表也没必要再问别人**。
//!
//! 数据来源全是磁盘上现成的东西：
//!
//! * `project.json` —— WE 工程文件，`title` / `type` / `file` / `preview` /
//! `contentrating`，以及 `general.properties`（就是界面"参数"页要的 schema）
//!
//! * `description` 里作者常写的 `tags:…` 一行（工坊标签，daemon 那边是从数据库取）
//!
//! * 目录名 —— 工坊 id（`external_id`，内核据此定位目录）
//!
//! 产出的结构与 daemon 回的 `WallpaperEntry` **完全一致** ⇒ 界面其余部分一行不用改。

use crate::wp::proto;
use std::path::{Path, PathBuf};

/// 工坊内容目录。`WE_WORKSHOP_DIR` 可覆盖（测试/多库共存时用）。
pub fn workshop_dir() -> PathBuf {
    if let Ok(p) = std::env::var("WE_WORKSHOP_DIR")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home).join(".local/share/Steam/steamapps/workshop/content/431960")
}

/// 扫默认工坊目录。
pub fn scan() -> Vec<proto::WallpaperEntry> {
    scan_dir(&workshop_dir())
}

/// 扫一个目录里的所有子目录。目录不存在 ⇒ 空表（**不报错**：全新机器/没装 Steam 是正常状态）。
/// 按显示名排序，保证每次打开的次序稳定。
pub fn scan_dir(dir: &Path) -> Vec<proto::WallpaperEntry> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<proto::WallpaperEntry> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter_map(|p| entry_from_dir(&p))
        .collect();
    out.sort_by_key(|e| e.name.to_lowercase());
    out
}

/// 一个工坊目录 → 一条记录。**读不到 `project.json` 也照样给一条**（用目录名当名字）——
/// 工坊里真有半残条目（作者上传坏了、下载中断），它们仍应出现在列表里而不是凭空消失。
pub fn entry_from_dir(dir: &Path) -> Option<proto::WallpaperEntry> {
    let id = dir.file_name()?.to_string_lossy().to_string();
    let project = std::fs::read_to_string(dir.join("project.json")).ok();
    let proj: Option<serde_json::Value> = project
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());

    let get_str = |key: &str| -> Option<String> {
        proj.as_ref()?
            .get(key)?
            .as_str()
            .map(|s| s.to_string())
    };

    let name = get_str("title")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| id.clone());

    // 类型：WE 用 scene / video / web / application；界面按小写比较。
    // 没有 project.json 时按磁盘内容猜（有 scene.pkg 就是 scene，有视频就是 video）。
    let wp_type = get_str("type")
        .map(|t| t.to_ascii_lowercase())
        .or_else(|| sniff_type(dir))
        .unwrap_or_else(|| "unknown".into());

    // 封面：project.json 里写的是相对文件名；也兜底找常见的 preview.*
    let preview = get_str("preview")
        .map(|p| dir.join(&p))
        .filter(|p| p.is_file())
        .or_else(|| {
            ["preview.gif", "preview.jpg", "preview.png", "preview.jpeg"]
                .iter()
                .map(|n| dir.join(n))
                .find(|p| p.is_file())
        })
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    // 参数 schema：project.json 的 `general.properties`（和 daemon 下发的是同一份数据）
    let schema = proj
        .as_ref()
        .and_then(|v| v.get("general")?.get("properties"))
        .filter(|v| v.is_object())
        .map(|v| v.to_string())
        .unwrap_or_default();

    let description = get_str("description").unwrap_or_default();
    let tags = tags_from_description(&description);

    Some(proto::WallpaperEntry {
        id: id.clone(),
        external_id: id,
        name,
        wp_type,
        // 目录路径：内核的 `entry_dir` 优先用 external_id，这里给全路径做兜底
        resource: dir.to_string_lossy().to_string(),
        preview,
        metadata: Default::default(),
        size: dir_size(dir),
        width: 0,
        height: 0,
        content_rating: get_str("contentrating").unwrap_or_default(),
        tags,
        user_properties_schema: schema,
        ..Default::default()
    })
}

/// 没有 `project.json` 时按磁盘上的东西猜类型。
fn sniff_type(dir: &Path) -> Option<String> {
    let rd = std::fs::read_dir(dir).ok()?;
    let mut has_video = false;
    let mut has_html = false;
    let mut has_image = false;
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().to_ascii_lowercase();
        if n.starts_with("preview") {
            continue;
        }
        if n == "scene.pkg" || n == "scene.json" {
            return Some("scene".into());
        }
        if n.ends_with(".mp4") || n.ends_with(".webm") || n.ends_with(".mkv") {
            has_video = true;
        }
        if n.ends_with(".html") {
            has_html = true;
        }
        if n.ends_with(".jpg") || n.ends_with(".png") || n.ends_with(".gif") {
            has_image = true;
        }
    }
    if has_html {
        Some("web".into())
    } else if has_video {
        Some("video".into())
    } else if has_image {
        Some("image".into())
    } else {
        None
    }
}

/// 作者常把标签写在简介里的 `tags:…` 一行（WE 工坊习惯）。
/// 只取那一行、按空白切开；没有就返回空表 —— **不猜**、不去解析正文。
fn tags_from_description(desc: &str) -> Vec<String> {
    for line in desc.lines() {
        let l = line.trim();
        let lower = l.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("tags:") {
            // 用原串切，避免大小写被小写化
            let orig = &l[l.len() - rest.len()..];
            return orig
                .split_whitespace()
                .map(|t| t.trim_matches(',').to_string())
                .filter(|t| !t.is_empty())
                .take(32)
                .collect();
        }
    }
    Vec::new()
}

/// 目录里顶层文件的总大小（不递归 —— 递归会为了一个"显示用"的数字去 stat 上万个文件）。
fn dir_size(dir: &Path) -> i64 {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    rd.flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len() as i64)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wh-local-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn missing_dir_is_empty_not_error() {
        // 全新机器没装 Steam ⇒ 空表，不能 panic、也不能报错
        let out = scan_dir(Path::new("/nonexistent/workshop/431960"));
        assert!(out.is_empty());
    }

    #[test]
    fn reads_project_json_fields() {
        let root = tmpdir("proj");
        let d = root.join("1234567");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("project.json"),
            r#"{"title":"just a girl v.2","type":"scene","preview":"preview.gif",
                "contentrating":"Everyone",
                "description":"tags:night moon girl\n\nsome long text",
                "general":{"properties":{"bloom":{"index":1,"type":"bool","value":true}}}}"#,
        )
        .unwrap();
        std::fs::write(d.join("preview.gif"), b"GIF89a").unwrap();
        std::fs::write(d.join("scene.pkg"), vec![0u8; 2048]).unwrap();

        let list = scan_dir(&root);
        assert_eq!(list.len(), 1, "一个工坊目录应产出一条");
        let e = &list[0];
        assert_eq!(e.id, "1234567");
        assert_eq!(e.external_id, "1234567", "内核靠 external_id 定位目录");
        assert_eq!(e.name, "just a girl v.2");
        assert_eq!(e.wp_type, "scene", "界面按小写比较类型");
        assert!(e.preview.ends_with("preview.gif"), "preview 要绝对路径：{}", e.preview);
        assert_eq!(e.content_rating, "Everyone");
        assert_eq!(e.tags, vec!["night", "moon", "girl"], "从 description 的 tags: 行取");
        assert!(e.user_properties_schema.contains("bloom"), "schema 用 general.properties");
        assert!(e.size >= 2048, "size 至少含 scene.pkg");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn broken_entry_still_listed() {
        // 没有 project.json（作者上传坏了 / 下载中断）⇒ 仍要出现在列表里
        let root = tmpdir("broken");
        let d = root.join("7654321");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("scene.pkg"), b"PKGV").unwrap();

        let list = scan_dir(&root);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "7654321", "没名字就用目录名兜底");
        assert_eq!(list[0].wp_type, "scene", "按磁盘内容猜出 scene");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn type_is_lowercased_and_preview_found_without_hint() {
        let root = tmpdir("lower");
        let d = root.join("999");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("project.json"), r#"{"title":"V","type":"Video"}"#).unwrap();
        std::fs::write(d.join("preview.jpg"), b"x").unwrap();
        let list = scan_dir(&root);
        assert_eq!(list[0].wp_type, "video", "类型统一小写");
        assert!(list[0].preview.ends_with("preview.jpg"), "project.json 没写 preview 也要能找到");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sorted_by_name_for_stable_order() {
        let root = tmpdir("sort");
        for (id, title) in [("1", "zeta"), ("2", "Alpha"), ("3", "mid")] {
            let d = root.join(id);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("project.json"), format!(r#"{{"title":"{title}","type":"scene"}}"#)).unwrap();
        }
        let names: Vec<String> = scan_dir(&root).into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["Alpha", "mid", "zeta"], "按名字排序（忽略大小写）");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tags_line_only_first_match_and_no_guessing() {
        assert_eq!(tags_from_description("no tags here"), Vec::<String>::new());
        assert_eq!(
            tags_from_description("hello\ntags:a b, c\nmore"),
            vec!["a", "b", "c"],
            "逗号要切掉"
        );
        // 只有一行 tags: 生效（后面的不再收）
        assert_eq!(tags_from_description("tags:x\ntags:y"), vec!["x"]);
    }
}
