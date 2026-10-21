//! WE 的「模型」层 —— 对象引用 `models/*.json`，**材质 / 几何 / 标志位**都由它持有。
//!
//! 为什么要单独成层：我们此前把这些字段散落在各个特判里
//! （`solidlayer` 一个 if、`cropoffset` 一个特判、`puppet` 又一处），
//! 每遇到一张新壁纸就补一个分支 ⇒ 反复"打地鼠"。参照实现里模型是**一等公民**：
//! 对象 → 模型 → { 材质, 几何(quad|网格), flags }，特效链的每一趟都复用同一份几何。
//!
//! 本文件只做**数据结构 + 解析**，不改任何渲染路径（默认无人使用，零回归风险）。

/// 一个 WE 模型（`models/*.json`）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Model {
    /// 模型文件路径（`models/xxx.json`），便于日志/排查。
    pub path: String,
    /// 材质路径（`materials/xxx.json`）。
    pub material: Option<String>,
    /// puppet 网格文件（`*.mdl`，MDLV）。有它 ⇒ 该对象的几何是**网格**而不是四边形。
    pub puppet: Option<String>,
    /// 全屏后处理层：输入是"到目前为止的场景合成图"。
    pub fullscreen: bool,
    /// 透传层（合成层/工程层用）。
    pub passthrough: bool,
    /// 纯色层（无贴图材质 ⇒ 用对象自身 color/alpha 画）。
    pub solidlayer: bool,
    /// 尺寸随源图自动确定。
    pub autosize: bool,
    /// 关闭内边距。
    pub nopadding: bool,
    /// 工程层（组合层容器）。
    pub projectlayer: bool,
    /// 模型级绘制偏移（单位 = **贴图像素**）。
    pub cropoffset: Option<[f32; 2]>,
    /// 显式宽高（少见，`width`/`height` 字段）。
    pub width: Option<i32>,
    pub height: Option<i32>,
}

impl Model {
    /// 从 `models/*.json` 的原文解析。解析失败返回 `None`（调用方决定回退策略）。
    pub fn parse(path: &str, bytes: &[u8]) -> Option<Model> {
        let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
        let b = |k: &str| v.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
        let i = |k: &str| v.get(k).and_then(|x| x.as_i64()).map(|x| x as i32);
        let crop = v
            .get("cropoffset")
            .and_then(|x| x.as_str())
            .and_then(|t| {
                let n: Vec<f32> = t
                    .split_whitespace()
                    .filter_map(|p| p.parse::<f32>().ok())
                    .collect();
                (n.len() >= 2).then(|| [n[0], n[1]])
            });
        Some(Model {
            path: path.to_string(),
            material: s("material"),
            puppet: s("puppet"),
            fullscreen: b("fullscreen"),
            passthrough: b("passthrough"),
            solidlayer: b("solidlayer"),
            autosize: b("autosize"),
            nopadding: b("nopadding"),
            projectlayer: b("projectlayer"),
            cropoffset: crop,
            width: i("width"),
            height: i("height"),
        })
    }

    /// 该对象的几何是不是网格（有 puppet 就是）。
    pub fn has_mesh(&self) -> bool {
        self.puppet.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_puppet_model() {
        let src = r#"{"autosize":true,"cropoffset":"575.0 -196.0","material":"materials/B灯台.json"}"#;
        let m = Model::parse("models/B灯台.json", src.as_bytes()).unwrap();
        assert_eq!(m.material.as_deref(), Some("materials/B灯台.json"));
        assert_eq!(m.cropoffset, Some([575.0, -196.0]));
        assert!(m.autosize && !m.has_mesh());
    }

    #[test]
    fn parses_mesh_model() {
        let src = r#"{"material":"materials/B沙发.json","cropoffset":"0 -111.5","puppet":"models/B沙发_puppet.mdl"}"#;
        let m = Model::parse("models/B沙发.json", src.as_bytes()).unwrap();
        assert!(m.has_mesh());
        assert_eq!(m.puppet.as_deref(), Some("models/B沙发_puppet.mdl"));
        assert_eq!(m.cropoffset, Some([0.0, -111.5]));
    }

    #[test]
    fn flags_default_false() {
        let m = Model::parse("m.json", b"{}").unwrap();
        assert!(!m.fullscreen && !m.passthrough && !m.solidlayer && !m.projectlayer);
        assert!(m.material.is_none() && m.cropoffset.is_none());
    }
}
