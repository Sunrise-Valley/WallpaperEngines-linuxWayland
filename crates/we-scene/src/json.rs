// SPDX-License-Identifier: MIT
//
//! WE 的 JSON **方言**宽容层（**默认关**，两个互相独立的开关）。
//!
//! ## 为什么需要
//!
//! 官方 assets 里**真实存在**两种 `serde_json` 默认不接受的写法：
//!
//! * **注释**：官方 `assets/effects/<eff>/preview/effects/<eff>/effect.json` 第 5 行普遍写成
//! `//"preview" : "preview/project.json",`。只读扫描本机官方库（`assets/`，1698 个 `.json`）：
//! **27 个文件带注释**，其中 **25 个是 `effect.json`**、2 个是 `presets/*/preset.json`。
//!
//! * **`null` 当空 map**：`assets/effects/{blur,opacity,blend}/preview/scene.json` 里有
//! `"constantshadervalues" : null`；`assets/effects/{reflection,blur,blend}/preview/scene.json`
//! 里有 `"combos" : null`。
//!
//! ## 官方解析器两边都能吃（双出处，逐字）
//!
//! * **注释** —— OWE 自己的 json 适配层单测
//! `open-wallpaper-engine/tests/tests/core/json_adapter_tests.cpp:30-42`
//! `Tests/JsonAdapter.CommentsRequireExplicitOption`：
//! `EXPECT_TRUE(owe::ParseJson("/* comment */ null").is_err);` 与
//! `ParseJson("{/* comment */ \"value\": 1 // line\n}", { .allow_comments = true })` 成功
//! ⇒ `rstd::json` 的默认是**不允许注释**，但它的方言里 `//` 与 `/* */` 都合法；
//! OWE 在 4 处显式打开：`src/Scene/Pkg/Parse/Puppet/MdlParser.cpp:1184`、
//! `src/Scene/SceneWallpaper.cpp:182`、`src/Web/Manifest.cpp:41`、
//! `src/Core/UserProperty.cpp:53`（都是 `{ .allow_comments = true }`）。
//! ⚠ 交底：OWE 加载 `effect.json` 的那条路（`src/Scene/Pkg/SceneObj/ImageObject.cpp:17-19`
//! 的 `LoadJsonFile` → `ReadJsonFile(vfs, path)`）用的是**默认 ParseOptions**，所以
//! **OWE 在这 25 个官方 effect.json 上也是解析失败的** —— 这条证据只能证明"WE 方言允许注释、
//! OWE 有开关但漏在 effect.json 路径上"，不能证明"OWE 读得了"。
//!
//! * **`null` 当空** —— OWE `MaterialPass::FromJson`
//! （`src/Scene/Pkg/SceneObj/Material.cpp:134-144`）：
//! `if (auto values = json.get("constantshadervalues"_str); values.is_some) { auto object = (*values)->as_object; if (object.is_some) … }`
//! ⇒ `null` 时 `as_object` 取不到，**整块跳过、不报错**；同一语义在
//! `src/Core/Json.cpp:250-258` 的 `GetJsonValue`：成员是 `null` 就 `return false`（= "没给"）。
//!
//! ## 开关（**默认关**：不设 或 ≠ `"1"`）
//!
//! * `WE_JSON_COMMENTS=1` —— 解析前剥掉**字符串外**的 `//` 与 `/* */` 注释
//! （块注释里的换行原样保留 ⇒ serde 报的行号和原文件一致）。
//!
//! * `WE_JSON_NULLMAP=0` —— **默认开**；本该给 map 的地方给 `null` ⇒ 当**空 map**
//! （`constantshadervalues` / `combos` 这两族字段，见 [`de_map_or_empty`]）。
//!
//! ## 安全性质（可证）
//!
//! 两个开关对**已经能解析**的输入是**恒等变换**：合法 JSON 在字符串外不可能出现 `//` / `/*`
//! （否则它本来就不是合法 JSON），而 [`strip_comments`] 在字符串内逐字节原样保留
//! ⇒ 剥完的串与原文相同 ⇒ 解析结果相同；`null` 分支只在 `null` 时生效，而 `null` 在旧路径上
//! 必然报错。所以：**开关只可能把"以前失败的"变成"成功"，不可能改变"以前成功的"**。
//! 单测 `strip_is_identity_on_clean_json` / `null_toggle_keeps_map_parse_identical` 钉住这条。

use std::collections::{BTreeMap, HashMap};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};

/// 开关读法：**只认 `"1"`**（与项目里 `WE_FX_DEFAULT_ALL` 等既有开关同口径；不设 / 别的值 ⇒ 关）。
fn flag_on(key: &str) -> bool {
    match std::env::var(key) {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => true,
    }
}

/// **默认开**（`WE_JSON_COMMENTS=0` 关）⇒ 容忍字符串外的 `//` 与 `/* */`。
pub fn comments_enabled() -> bool {
    flag_on("WE_JSON_COMMENTS")
}

/// `WE_JSON_NULLMAP=1` ⇒ 该给 map 的地方给 `null` 时当空 map。
pub fn null_map_enabled() -> bool {
    flag_on("WE_JSON_NULLMAP")
}

/// 剥掉**字符串外**的 `//` 与 `/* */` 注释。
///
/// * 字符串内的 `//`、`/*` **原样保留**（否则 `"http://…"` 这种值会被啃掉）；
///
/// * 被剥掉的字符用**空格**顶替、块注释里的换行**原样保留** ⇒ **字节数不变、行号不变**，
/// serde 报的 `line`/`column` 与原始文件一致（诊断要靠这个）；
///
/// * 对不含注释的合法 JSON 是**恒等**（逐字节相同）。
pub fn strip_comments(src: &str) -> String {
    if !src.as_bytes().contains(&b'/') {
        return src.to_string(); // 快路径：没有 '/' 就绝不可能是注释
    }
    let b = src.as_bytes();
    let n = b.len();
    let mut out: Vec<u8> = Vec::with_capacity(n);
    let mut i = 0usize;
    let mut in_str = false;
    while i < n {
        let c = b[i];
        if in_str {
            out.push(c);
            if c == b'\\' && i + 1 < n {
                // 转义序列整体搬走（`\"` 不结束字符串）
                out.push(b[i + 1]);
                i += 2;
                continue;
            }
            if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_str = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == b'/' && i + 1 < n && b[i + 1] == b'/' {
            i += 2;
            while i < n && b[i] != b'\n' {
                out.push(b' ');
                i += 1;
            }
            continue; // 换行留给下一轮原样搬（行号不变）
        }
        if c == b'/' && i + 1 < n && b[i + 1] == b'*' {
            i += 2;
            out.push(b' ');
            out.push(b' ');
            while i < n && !(b[i] == b'*' && i + 1 < n && b[i + 1] == b'/') {
                out.push(if b[i] == b'\n' { b'\n' } else { b' ' });
                i += 1;
            }
            if i < n {
                out.push(b' ');
                out.push(b' ');
                i += 2;
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    // 只搬过原字节 + 空格替换 ⇒ 仍是合法 UTF-8；真出意外也不 panic。
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// **所有** WE JSON 的解析入口（`scene.json` / `effect.json` / 材质 / `preset.json` / `project.json`）。
///
/// 开关**关**时逐字调用 `serde_json::from_slice`（与旧路径完全等价）；
/// 开时先 [`strip_comments`]（若 `WE_JSON_COMMENTS=1`）再解析。
pub fn from_slice<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, serde_json::Error> {
    if !comments_enabled() {
        return serde_json::from_slice::<T>(bytes);
    }
    // 只读诊断/解析路径：非 UTF-8 的 WE JSON 不存在，`from_utf8_lossy` 只为不 panic。
    let text = String::from_utf8_lossy(bytes);
    let stripped = strip_comments(&text);
    serde_json::from_str::<T>(&stripped)
}

/// `serde(deserialize_with = "we_scene::json::de_map_or_empty")` 用的 map 类型标记。
pub trait NullEmpty: Sized {
    fn null_empty() -> Self;
}
impl<V> NullEmpty for BTreeMap<String, V> {
    fn null_empty() -> Self {
        BTreeMap::new()
    }
}
impl<V> NullEmpty for HashMap<String, V> {
    fn null_empty() -> Self {
        HashMap::new()
    }
}

/// 该给 map 的地方给了 `null` ⇒ 当空 map（`WE_JSON_NULLMAP=1` 时才生效）。
///
/// 开关**关**时**逐字**调用 `M::deserialize(d)` ⇒ 与旧路径（`null` 直接报错）等价。
/// 开关**开**且不是 `null` 时，走 `serde_json::from_value` ⇒ 与直接反序列化同语义。
pub fn de_map_or_empty<'de, M, D>(d: D) -> Result<M, D::Error>
where
    M: DeserializeOwned + NullEmpty,
    D: Deserializer<'de>,
{
    if !null_map_enabled() {
        return M::deserialize(d);
    }
    let v = serde_json::Value::deserialize(d)?;
    if v.is_null() {
        return Ok(M::null_empty());
    }
    serde_json::from_value::<M>(v).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 合法（无注释）JSON ⇒ 剥注释是**逐字节恒等**（这是"开关不可能改变已能解析的输入"的依据）。
    #[test]
    fn strip_is_identity_on_clean_json() {
        let cases = [
            r#"{"a":1,"b":[null,"x"],"c":{"d":true}}"#,
            "{\n  \"a\": \"http://example.com/*not a comment*/\",\n  \"b\": \"// also not\"\n}",
            r#"{"esc": "a\"//b", "t": 1}"#,
            "{}",
        ];
        for c in cases {
            assert_eq!(strip_comments(c), c, "必须逐字节恒等：{c}");
        }
    }

    /// 字符串里的 `//` / `/* */` 绝不能被啃；真注释要被剥掉且**行号不变**。
    #[test]
    fn strip_keeps_strings_and_preserves_line_numbers() {
        let src = "{\n\t\"name\": \"a//b\",\n\t//\"preview\": \"x\",\n\t\"v\": 1\n}";
        let out = strip_comments(src);
        assert_eq!(out.lines().count(), src.lines().count(), "行数不变");
        assert!(out.contains("\"a//b\""), "字符串内的 // 必须留着");
        assert!(!out.contains("preview"), "真注释必须剥掉");
        let v: serde_json::Value = serde_json::from_str(&out).expect("剥完应是合法 JSON");
        assert_eq!(v["v"], 1);
        assert_eq!(v["name"], "a//b");
    }

    /// 块注释：跨行也保持行号，且字符串内的 `/*` 不动。
    #[test]
    fn strip_block_comments_keeps_lines() {
        let src = "{\n/* a\n b */\n\"k\": \"/*keep*/\"\n}";
        let out = strip_comments(src);
        assert_eq!(out.lines().count(), src.lines().count());
        assert!(out.contains("\"/*keep*/\""), "字符串内的 /* 必须留着");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["k"], "/*keep*/");
    }

    /// 官方 `effect.json` 的真实那一行（第 5 行的 `//"preview" …`）：剥完能解析。
    #[test]
    fn strips_the_official_effect_json_comment_shape() {
        let src = "{\r\n\t\"name\" : \"Water Ripple\",\r\n\t\"group\" : \"image\",\r\n\t//\"preview\" : \"preview/project.json\",\r\n\t\"passes\" : []\r\n}";
        let out = strip_comments(src);
        assert_eq!(out.lines().count(), src.lines().count());
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["group"], "image");
        assert_eq!(v["passes"].as_array().map(|a| a.len()), Some(0));
    }

    /// 不设 ⇒ **默认开**（容忍注释）；显式 `=0` 关时与 `serde_json::from_slice` 完全等价（含错误文本）。
    #[test]
    fn from_slice_off_equals_serde_json() {
        let bad = br#"{"a": 1 // hi
}"#;
        std::env::remove_var("WE_JSON_COMMENTS");
        assert!(
            from_slice::<serde_json::Value>(bad).is_ok(),
            "不设 ⇒ 默认开 ⇒ 注释被容忍"
        );
        std::env::set_var("WE_JSON_COMMENTS", "0");
        let a = from_slice::<serde_json::Value>(bad).unwrap_err().to_string();
        let b = serde_json::from_slice::<serde_json::Value>(bad)
            .unwrap_err()
            .to_string();
        assert_eq!(a, b, "显式关时错误文本必须一致");
        let ok = br#"{"a":1}"#;
        assert_eq!(
            from_slice::<serde_json::Value>(ok).unwrap()["a"],
            1
        );
    }

    /// `null` 当 map：不设 ⇒ **默认开** ⇒ 空 map；显式 `=0` ⇒ 与旧的直接反序列化等价（报错）。
    #[test]
    fn null_map_toggle() {
        #[derive(serde::Deserialize, Debug, Default)]
        #[serde(default)]
        struct S {
            #[serde(deserialize_with = "de_map_or_empty")]
            m: BTreeMap<String, i64>,
        }
        std::env::remove_var("WE_JSON_NULLMAP");
        let s_on: S = serde_json::from_str(r#"{"m": null}"#).unwrap();
        assert!(s_on.m.is_empty(), "不设 ⇒ 默认开 ⇒ null ⇒ 空 map");
        std::env::set_var("WE_JSON_NULLMAP", "0");
        assert!(
            serde_json::from_str::<S>(r#"{"m": null}"#).is_err(),
            "显式关时必须报错"
        );
        std::env::remove_var("WE_JSON_NULLMAP");
        assert_eq!(serde_json::from_str::<S>(r#"{"m": {"a": 2}}"#).unwrap().m["a"], 2);

        std::env::set_var("WE_JSON_NULLMAP", "1");
        let s: S = serde_json::from_str(r#"{"m": null}"#).unwrap();
        assert!(s.m.is_empty(), "开时 null ⇒ 空 map");
        let s2: S = serde_json::from_str(r#"{"m": {"a": 2}}"#).unwrap();
        assert_eq!(s2.m["a"], 2, "开时正常 map 语义不变");
        let s3: S = serde_json::from_str(r#"{}"#).unwrap();
        assert!(s3.m.is_empty(), "缺字段仍走 default");
        std::env::remove_var("WE_JSON_NULLMAP");
    }
}
