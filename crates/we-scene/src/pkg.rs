// SPDX-License-Identifier: MIT
//
//! PKGV 容器解析（`scene.pkg` 等）。
//!
//! 布局（**对真实素材实测验证**，版本号随壁纸而变，本例 `PKGV0002`）：
//!
//! ```text
//! u32 magic_len(=8) | magic(8 字节) | u32 entry_count
//! 条目 × entry_count: u32 name_len | name(UTF-8) | u32 offset | u32 size
//! 数据区（从 header_end 开始；条目 offset 相对 header_end）
//! ```
//!
//! 两个必须记住的点：
//!
//! * magic 有 **4 字节长度前缀** ⇒ 别假设文件从 "PKGV" 开头
//!
//! * `offset` **相对 header_end**，不是绝对偏移

use crate::{Error, Result};

/// 名字长度上限（防脏数据把解析带飞）
const MAX_NAME_LEN: u32 = 4096;
/// 条目数上限
const MAX_ENTRIES: u32 = 100_000;

fn u32_at(d: &[u8], o: usize) -> Result<u32> {
    d.get(o..o + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or(Error::TooShort("读 u32 越界"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// 相对 `Pkg::header_end`
    pub offset: u32,
    pub size: u32,
}

#[derive(Debug, Clone)]
pub struct Pkg {
    /// 形如 `PKGV0002`
    pub magic: String,
    /// 头部末尾 = 数据区起点
    pub header_end: usize,
    pub entries: Vec<Entry>,
}

impl Pkg {
    pub fn parse(data: &[u8]) -> Result<Pkg> {
        let magic_len = u32_at(data, 0)? as usize;
        if !(4..=64).contains(&magic_len) {
            return Err(Error::BadMagic(format!(
                "magic 长度 {magic_len} 不合理（期望 4..=64）"
            )));
        }
        let raw_magic = data
            .get(4..4 + magic_len)
            .ok_or(Error::TooShort("magic 越界"))?;
        let magic = String::from_utf8_lossy(raw_magic).to_string();
        if !magic.starts_with("PKGV") {
            return Err(Error::BadMagic(magic));
        }

        let count = u32_at(data, 4 + magic_len)?;
        if count == 0 || count > MAX_ENTRIES {
            return Err(Error::BadEntry(format!("条目数 {count} 不合理")));
        }

        let mut o = 8 + magic_len;
        let mut entries = Vec::with_capacity(count as usize);
        for i in 0..count {
            let nlen = u32_at(data, o)?;
            if nlen == 0 || nlen > MAX_NAME_LEN {
                return Err(Error::BadEntry(format!("第 {i} 条名字长度 {nlen} 不合理")));
            }
            let name_bytes = data
                .get(o + 4..o + 4 + nlen as usize)
                .ok_or(Error::TooShort("条目名越界"))?;
            let offset = u32_at(data, o + 4 + nlen as usize)?;
            let size = u32_at(data, o + 8 + nlen as usize)?;
            entries.push(Entry {
                name: String::from_utf8_lossy(name_bytes).to_string(),
                offset,
                size,
            });
            o += 4 + nlen as usize + 8;
        }
        Ok(Pkg {
            magic,
            header_end: o,
            entries,
        })
    }

    /// 取某个条目的数据（名字大小写不敏感）
    pub fn get<'a>(&self, data: &'a [u8], name: &str) -> Option<&'a [u8]> {
        let e = self
            .entries
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case(name))?;
        self.slice(data, e)
    }

    /// 按条目切数据
    pub fn slice<'a>(&self, data: &'a [u8], e: &Entry) -> Option<&'a [u8]> {
        let base = self.header_end.checked_add(e.offset as usize)?;
        data.get(base..base.checked_add(e.size as usize)?)
    }

    pub fn find(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.name.eq_ignore_ascii_case(name))
    }

    pub fn find_suffix(&self, suffix: &str) -> Option<&Entry> {
        let s = suffix.to_ascii_lowercase();
        self.entries
            .iter()
            .find(|e| e.name.to_ascii_lowercase().ends_with(&s))
    }

    /// 场景图条目（`scene.json`）
    pub fn scene_json_entry(&self) -> Option<&Entry> {
        self.find_suffix("scene.json")
    }

    /// 自检：条目链应该**无空洞**（下一条的 offset == 上一条 offset+size）。
    /// 真实素材满足这一点；不满足通常意味着布局假设错了。
    pub fn is_contiguous(&self) -> bool {
        let mut expect: u64 = 0;
        for e in &self.entries {
            if e.offset as u64 != expect {
                return false;
            }
            expect += e.size as u64;
        }
        true
    }

    /// 数据区总长（最后一条的末尾）
    pub fn data_len(&self) -> u64 {
        self.entries
            .iter()
            .map(|e| e.offset as u64 + e.size as u64)
            .max()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个最小的 PKGV（用来测布局，不依赖任何外部素材）
    fn build_pkg(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let magic = b"PKGV0002";
        let mut out = Vec::new();
        out.extend_from_slice(&(magic.len() as u32).to_le_bytes());
        out.extend_from_slice(magic);
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        let mut off = 0u32;
        for (name, data) in entries {
            out.extend_from_slice(&(name.len() as u32).to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&off.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            off += data.len() as u32;
        }
        for (_, data) in entries {
            out.extend_from_slice(data);
        }
        out
    }

    #[test]
    fn parses_entry_table_and_data() {
        let bytes = build_pkg(&[("a.json", b"{\"x\":1}"), ("b/c.tex", b"TEXV")]);
        let pkg = Pkg::parse(&bytes).expect("应能解析");
        assert_eq!(pkg.magic, "PKGV0002");
        assert_eq!(pkg.entries.len(), 2);
        assert_eq!(pkg.entries[0].name, "a.json");
        assert_eq!(pkg.entries[0].offset, 0);
        assert_eq!(pkg.entries[0].size, 7);
        assert_eq!(pkg.entries[1].offset, 7, "偏移必须相对 header_end 连续");
        assert!(pkg.is_contiguous());
        assert_eq!(pkg.get(&bytes, "a.json"), Some(&b"{\"x\":1}"[..]));
        assert_eq!(pkg.get(&bytes, "B/C.TEX"), Some(&b"TEXV"[..]), "名字不区分大小写");
        assert_eq!(pkg.get(&bytes, "没有这个"), None);
        assert_eq!(pkg.data_len(), 11u64);
    }

    #[test]
    fn rejects_wrong_magic() {
        let bytes = build_pkg(&[("a", b"x")]);
        // 把 magic 改成 PKGV 以外的东西
        let mut bad = bytes.clone();
        bad[4..12].copy_from_slice(b"XXXX0002");
        assert!(matches!(Pkg::parse(&bad), Err(Error::BadMagic(_))));
    }

    #[test]
    fn rejects_truncated_header() {
        let bytes = build_pkg(&[("a.json", b"{}")]);
        let cut = &bytes[..12];
        assert!(Pkg::parse(cut).is_err());
    }

    #[test]
    fn contiguity_check_catches_gaps() {
        let mut bytes = build_pkg(&[("a.json", b"{}"), ("b.json", b"[]")]);
        // 手工把第二条的 offset 改掉，制造空洞
        // 布局: 4+8(magic) +4(count) + [4+6(name)+4+4] + ...
        let second_off_pos = 4 + 8 + 4 + (4 + 6 + 4 + 4) + 4 + 6;
        bytes[second_off_pos..second_off_pos + 4].copy_from_slice(&99u32.to_le_bytes());
        let pkg = Pkg::parse(&bytes).expect("仍应能解析");
        assert!(!pkg.is_contiguous(), "有空洞必须被发现");
    }

    /// 对真实素材的端到端验证（素材不存在就跳过）
    #[test]
    fn real_scene_pkg_matches_measured_layout() {
        let dir = std::env::var("WE_SCENE_DIR").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            format!("{home}/.local/share/Steam/steamapps/workshop/content/431960/1689515439")
        });
        let path = std::path::Path::new(&dir).join("scene.pkg");
        let Ok(data) = std::fs::read(&path) else {
            eprintln!("跳过：没有素材 {}", path.display());
            return;
        };
        let pkg = Pkg::parse(&data).expect("真实素材应能解析");
        assert_eq!(pkg.magic, "PKGV0002");
        assert_eq!(pkg.entries.len(), 48);
        assert!(pkg.is_contiguous(), "真实素材的条目链应当无空洞");
        assert_eq!(pkg.entries[0].name, "materials/effects/godrays_downsample2.json");
        assert_eq!((pkg.entries[0].offset, pkg.entries[0].size), (0, 176));
        // 第一段数据必须是 JSON —— 这是"偏移基准算对了"的证据
        let first = pkg.slice(&data, &pkg.entries[0]).unwrap();
        assert!(
            first.starts_with(b"{") || first.starts_with(b"["),
            "entry[0] 应当是 JSON，实际开头是 {:?}",
            &first[..first.len().min(16)]
        );
        // scene.json 在且能解析
        let sj = pkg.scene_json_entry().expect("应当有 scene.json");
        let raw = pkg.slice(&data, sj).unwrap();
        let doc: serde_json::Value = serde_json::from_slice(raw).expect("scene.json 应能解析");
        assert!(doc.get("objects").is_some());
    }
}
