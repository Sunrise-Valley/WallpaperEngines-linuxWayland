// SPDX-License-Identifier: MIT
//
//! Wallpaper Engine `scene` 资产解析。
//!
//! 三块，全部按**真实素材反推并验证**（见同目录的 `../README.md` 与 `../probe_pkg.py`）：
//!
//! * [`pkg`] —— PKGV 容器（`scene.pkg`）：条目表 + 数据区
//!
//! * [`tex`] —— `.tex` 贴图容器：TEXV/TEXI/TEXB + 内嵌图片
//!
//! * [`scene`] —— `scene.json` 场景图（相机 / 全局 / 对象 / 特效）
//!
//! 两个反推出来的关键坑（都会让解析直接崩）：
//!
//! 1. PKGV 的 magic **前面有 4 字节长度前缀**，不是从偏移 0 开始
//!
//! 2. 条目的 `offset` **相对「头部末尾」**，不是文件绝对偏移

pub mod json;
pub mod model;
pub mod pkg;
pub mod scene;
pub mod tex;

use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum Error {
    /// 数据不够长 / 越界
    TooShort(&'static str),
    /// 容器 magic 不对（不是 PKGV / TEXV）
    BadMagic(String),
    /// 条目表不合法
    BadEntry(String),
    /// JSON 解析失败
    Json(String),
    /// 文件系统问题
    Io(String),
    /// 该格式暂不支持（附原因）
    Unsupported(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TooShort(w) => write!(f, "数据越界：{w}"),
            Error::BadMagic(m) => write!(f, "容器 magic 不对：{m}"),
            Error::BadEntry(m) => write!(f, "条目表不合法：{m}"),
            Error::Json(m) => write!(f, "JSON 解析失败：{m}"),
            Error::Io(m) => write!(f, "文件问题：{m}"),
            Error::Unsupported(m) => write!(f, "暂不支持：{m}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

// ── project.json ────────────────────────────────────────────────────────────

/// WE 壁纸的类型（`project.json` 的顶层 `type`）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Scene,
    Video,
    Image,
    Web,
    Application,
    Unknown(String),
}

impl Kind {
    pub fn from_str_lossy(s: &str) -> Kind {
        match s.to_ascii_lowercase().as_str() {
            "scene" => Kind::Scene,
            "video" => Kind::Video,
            "image" => Kind::Image,
            "web" => Kind::Web,
            "application" => Kind::Application,
            other => Kind::Unknown(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Kind::Scene => "scene",
            Kind::Video => "video",
            Kind::Image => "image",
            Kind::Web => "web",
            Kind::Application => "application",
            Kind::Unknown(s) => s,
        }
    }
}

/// 一个壁纸条目的元信息（来自 `project.json`；只有我们关心的几个字段）
#[derive(Debug, Clone)]
pub struct Project {
    pub dir: PathBuf,
    pub kind: Kind,
    pub title: String,
    /// `scene.json` / 视频文件之类的主文件（`project.json` 的 `file` 字段）
    pub file: String,
    pub preview: String,
}

impl Project {
    /// 读一个条目目录里的 `project.json`
    pub fn read_dir(dir: impl AsRef<Path>) -> Result<Project> {
        let dir = dir.as_ref().to_path_buf();
        let pj = dir.join("project.json");
        let text = std::fs::read_to_string(&pj)
            .map_err(|e| Error::Io(format!("读不了 {}: {e}", pj.display())))?;
        let doc: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| Error::Json(e.to_string()))?;
        let get = |k: &str| doc.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        Ok(Project {
            dir,
            kind: Kind::from_str_lossy(&get("type")),
            title: get("title"),
            file: get("file"),
            preview: get("preview"),
        })
    }

    /// 场景包路径（`file` 通常是 `scene.json`，包名固定 `scene.pkg`）
    pub fn scene_pkg(&self) -> PathBuf {
        self.dir.join("scene.pkg")
    }
}
