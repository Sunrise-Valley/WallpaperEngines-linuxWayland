// SPDX-License-Identifier: MIT
//! `.tex` 贴图容器。
//! 两种载荷都要吃：
//!
//! * **内嵌图片容器**（PNG/JPEG/WebP/GIF/MP4）—— 按魔数扫描定位，尺寸从图片头读（老路，保留）
//!
//! * **裸像素 / DXT**（可选 LZ4 压缩）—— 按 TEXB 头部描述解（本文件新增，见 [`decode`]）
//! 头部布局（照 `open-wallpaper-engine/src/Scene/Pkg/Parse/Image/TexImageParser.cpp`
//! 的 `LoadHeader`；实测 TEXB0002/0003）：
//! ```text
//! TEXV0005 | TEXI0001 | i32 format | u32 flags | i32 width | i32 height
//! | i32 mapWidth | i32 mapHeight | i32 unknown | TEXB000x | i32 image_count
//! | [i32 image_type （texb>=3 才有）] | [条件补丁（texb>=4）]
//! 然后每个 image：i32 mip_count；每个 mip：i32 w | i32 h
//! | [i32 lz4_flag + i32 decompressed_size （texb>=2 才有）] | i32 src_size | bytes
//! ```
//! format 取值（同上）：`0=RGBA8888, 4=DXT5(BC3), 6=DXT3(BC2), 7=DXT1(BC1), 8=RG88, 9=`。
//! ⚠ 以前这里"不猜头部、只扫魔数"，于是 DXT/ 那类只能报"不支持" —— 而本机 17 条壁纸里
//! 有 2 条整条卡在这上面（症状是 `没有可渲染的图层（贴图可能是不支持的格式）`）。

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Payload {
    Png,
    Jpeg,
    Webp,
    Gif,
    Mp4,
}

impl Payload {
    pub fn ext(self) -> &'static str {
        match self {
            Payload::Png => "png",
            Payload::Jpeg => "jpg",
            Payload::Webp => "webp",
            Payload::Gif => "gif",
            Payload::Mp4 => "mp4",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Tex {
    /// 形如 `TEXV0005`
    pub version: String,
    /// 形如 `TEXB0003`（可能没有）
    pub body: Option<String>,
    pub payload: Payload,
    /// 内嵌数据（PNG 已按 IEND 精确截断）
    pub bytes: Vec<u8>,
    /// 尺寸（仅 PNG 能直接读出来）
    pub dims: Option<(u32, u32)>,
}

/// 解好的裸像素（RGBA8，行优先）
#[derive(Debug, Clone)]
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// `.tex` 的两种去向
#[derive(Debug, Clone)]
pub enum Decoded {
    /// 内嵌图片容器（交给图片解码器）
    Container(Payload, Vec<u8>),
    /// 裸像素（直接用）
    Rgba(Pixels),
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

const MAGICS: &[(&[u8], Payload)] = &[
    (b"\x89PNG\r\n\x1a\n", Payload::Png),
    (b"\xff\xd8\xff", Payload::Jpeg),
    (b"RIFF", Payload::Webp), // 还要看第 8..12 字节是不是 WEBP
    (b"GIF8", Payload::Gif),
];

/// 按魔数判断一段内存是不是图片/视频容器
fn sniff(data: &[u8]) -> Option<Payload> {
    for (magic, kind) in MAGICS {
        if data.starts_with(magic) {
            if *kind == Payload::Webp && data.get(8..12) != Some(b"WEBP") {
                continue;
            }
            return Some(*kind);
        }
    }
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        return Some(Payload::Mp4);
    }
    None
}

/// PNG 的 IHDR 尺寸（第 16..24 字节，大端）
fn png_dims(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 24 || !b.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    let w = u32::from_be_bytes([b[16], b[17], b[18], b[19]]);
    let h = u32::from_be_bytes([b[20], b[21], b[22], b[23]]);
    Some((w, h))
}

/// PNG 结尾（IEND 之后含 4 字节 CRC）
fn png_end(b: &[u8]) -> Option<usize> {
    let i = find(b, b"IEND")?;
    Some(i + 8)
}

// ── 小工具：按小端顺序读 ────────────────────────────────────────────────────

struct Reader<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn new(d: &'a [u8]) -> Self {
        Reader { d, p: 0 }
    }
    fn u32(&mut self) -> Option<u32> {
        let v = u32::from_le_bytes(self.d.get(self.p..self.p + 4)?.try_into().ok()?);
        self.p += 4;
        Some(v)
    }
    fn i32(&mut self) -> Option<i32> {
        self.u32().map(|v| v as i32)
    }
    /// `TEXV0005\0` / `TEXI0001\0` / `TEXB000x\0` 都是**定长 9 字节**（8 字符 + NUL），
    /// 不是 `u32 长度 + 字符串` —— 实测 `*.tex` 头部，按长度读会整段错位。
    fn tag(&mut self) -> Option<String> {
        let s = self.d.get(self.p..self.p + 8)?;
        if !s.iter().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        self.p += 8;
        if self.d.get(self.p) == Some(&0) {
            self.p += 1;
        }
        Some(String::from_utf8_lossy(s).to_string())
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        if self.p + n > self.d.len() {
            return None;
        }
        self.p += n;
        Some(())
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let v = self.d.get(self.p..self.p + n)?;
        self.p += n;
        Some(v)
    }
    fn rest(&self) -> &'a [u8] {
        &self.d[self.p.min(self.d.len())..]
    }
}

/// `TEXB000x` → x
fn texb_version(tag: &str) -> Option<i32> {
    tag.strip_prefix("TEXB")?.parse::<i32>().ok()
}

// ── LZ4 块解压（自己写：离线 registry 里没有 lz4 crate）────────────────────────
// LZ4 block 格式：一串 sequence，每个 sequence = token(1B) + literals，之后可选
// 2 字节 offset + match。token 高 4 位 = literal 长度（15 表示后面跟扩展字节），
// 低 4 位 = match 长度 - 4（同样 15 表示扩展）。最后一个 sequence 只有 literals。
pub fn lz4_decompress(src: &[u8], out_len: usize) -> Option<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity(out_len);
    if out_len == 0 {
        return Some(out);
    }
    let mut i = 0usize;
    loop {
        let token = *src.get(i)?;
        i += 1;
        // literals
        let mut lit = (token >> 4) as usize;
        if lit == 15 {
            loop {
                let b = *src.get(i)?;
                i += 1;
                lit += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        out.extend_from_slice(src.get(i..i + lit)?);
        i += lit;
        if out.len() >= out_len {
            break;
        }
        // match
        let off = u16::from_le_bytes(src.get(i..i + 2)?.try_into().ok()?) as usize;
        i += 2;
        if off == 0 || off > out.len() {
            return None; // 非法偏移
        }
        let mut mlen = (token & 0x0f) as usize;
        if mlen == 15 {
            loop {
                let b = *src.get(i)?;
                i += 1;
                mlen += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        mlen += 4;
        let start = out.len() - off;
        for k in 0..mlen {
            let b = out[start + k]; // 允许重叠复制（LZ4 就是这么定义的）
            out.push(b);
            if out.len() >= out_len {
                break;
            }
        }
        if out.len() >= out_len {
            break;
        }
    }
    out.resize(out_len, 0);
    Some(out)
}

// ── DXT / 裸像素解码 ────────────────────────────────────────────────────────

fn rgb565(v: u16) -> [u8; 3] {
    let r = ((v >> 11) & 0x1f) as u32;
    let g = ((v >> 5) & 0x3f) as u32;
    let b = (v & 0x1f) as u32;
    [
        ((r * 255 + 15) / 31) as u8,
        ((g * 255 + 31) / 63) as u8,
        ((b * 255 + 15) / 31) as u8,
    ]
}

/// DXT1/BC1 一个 4x4 块（8 字节）→ 16 个 RGBA
fn bc1_block(src: &[u8], out: &mut [u8]) {
    let c0 = u16::from_le_bytes([src[0], src[1]]);
    let c1 = u16::from_le_bytes([src[2], src[3]]);
    let bits = u32::from_le_bytes([src[4], src[5], src[6], src[7]]);
    let (a, b) = (rgb565(c0), rgb565(c1));
    let mut pal = [[0u8; 3]; 4];
    pal[0] = a;
    pal[1] = b;
    if c0 > c1 {
        pal[2] = [
            ((2 * a[0] as u32 + b[0] as u32) / 3) as u8,
            ((2 * a[1] as u32 + b[1] as u32) / 3) as u8,
            ((2 * a[2] as u32 + b[2] as u32) / 3) as u8,
        ];
        pal[3] = [
            ((a[0] as u32 + 2 * b[0] as u32) / 3) as u8,
            ((a[1] as u32 + 2 * b[1] as u32) / 3) as u8,
            ((a[2] as u32 + 2 * b[2] as u32) / 3) as u8,
        ];
    } else {
        pal[2] = [
            ((a[0] as u32 + b[0] as u32) / 2) as u8,
            ((a[1] as u32 + b[1] as u32) / 2) as u8,
            ((a[2] as u32 + b[2] as u32) / 2) as u8,
        ];
        pal[3] = [0, 0, 0];
    }
    for i in 0..16 {
        let idx = ((bits >> (2 * i)) & 3) as usize;
        let alpha = if c0 <= c1 && idx == 3 { 0 } else { 255 };
        out[i * 4] = pal[idx][0];
        out[i * 4 + 1] = pal[idx][1];
        out[i * 4 + 2] = pal[idx][2];
        out[i * 4 + 3] = alpha;
    }
}

/// DXT3/BC2（16 字节：8 字节显式 4bit alpha + 8 字节颜色）
fn bc2_block(src: &[u8], out: &mut [u8]) {
    bc1_block(&src[8..16], out);
    let a = u64::from_le_bytes(src[0..8].try_into().unwrap());
    for i in 0..16 {
        let v = ((a >> (4 * i)) & 0x0f) as u8;
        out[i * 4 + 3] = (v << 4) | v; // 0..15 → 0..255
    }
}

/// DXT5/BC3（16 字节：2 个 alpha 端点 + 16×3bit 索引，再是 8 字节颜色）
fn bc3_block(src: &[u8], out: &mut [u8]) {
    bc1_block(&src[8..16], out);
    let a0 = src[0] as u32;
    let a1 = src[1] as u32;
    let mut pal = [0u8; 8];
    pal[0] = a0 as u8;
    pal[1] = a1 as u8;
    if a0 > a1 {
        for i in 1..7 {
            pal[1 + i] = (((7 - i) as u32 * a0 + i as u32 * a1) / 7) as u8;
        }
    } else {
        for i in 1..5 {
            pal[1 + i] = (((5 - i) as u32 * a0 + i as u32 * a1) / 5) as u8;
        }
        pal[6] = 0;
        pal[7] = 255;
    }
    let mut bits: u64 = 0;
    for (k, b) in src[2..8].iter().enumerate() {
        bits |= (*b as u64) << (8 * k);
    }
    for i in 0..16 {
        let idx = ((bits >> (3 * i)) & 7) as usize;
        out[i * 4 + 3] = pal[idx];
    }
}

/// 把一块一块解成 RGBA 图（贴图尺寸不一定是 4 的倍数 ⇒ 按块对齐、按行裁剪）
fn decode_blocks(
    data: &[u8],
    w: u32,
    h: u32,
    block_bytes: usize,
    block: fn(&[u8], &mut [u8]),
) -> Option<Pixels> {
    let (w, h) = (w as usize, h as usize);
    let bw = w.div_ceil(4);
    let bh = h.div_ceil(4);
    let need = bw * bh * block_bytes;
    if data.len() < need {
        return None;
    }
    let mut rgba = vec![0u8; w * h * 4];
    let mut tmp = [0u8; 64];
    for by in 0..bh {
        for bx in 0..bw {
            let off = (by * bw + bx) * block_bytes;
            block(&data[off..off + block_bytes], &mut tmp);
            for y in 0..4 {
                let py = by * 4 + y;
                if py >= h {
                    break;
                }
                for x in 0..4 {
                    let px = bx * 4 + x;
                    if px >= w {
                        break;
                    }
                    let s = (y * 4 + x) * 4;
                    let d = (py * w + px) * 4;
                    rgba[d..d + 4].copy_from_slice(&tmp[s..s + 4]);
                }
            }
        }
    }
    Some(Pixels {
        width: w as u32,
        height: h as u32,
        rgba,
    })
}

fn decode_raw(format: i32, data: &[u8], w: u32, h: u32) -> Option<Pixels> {
    let (w, h) = (w as usize, h as usize);
    match format {
        // RGBA8888 裸像素
        0 => {
            if data.len() < w * h * 4 {
                return None;
            }
            Some(Pixels {
                width: w as u32,
                height: h as u32,
                rgba: data[..w * h * 4].to_vec(),
            })
        }
        // RG88：两通道 → (r, g, 0, 255)
        8 => {
            if data.len() < w * h * 2 {
                return None;
            }
            let mut rgba = vec![0u8; w * h * 4];
            for i in 0..w * h {
                rgba[i * 4] = data[i * 2];
                rgba[i * 4 + 1] = data[i * 2 + 1];
                rgba[i * 4 + 3] = 255;
            }
            Some(Pixels {
                width: w as u32,
                height: h as u32,
                rgba,
            })
        }
        // 单通道 → (r, r, r, 255)
        9 => {
            if data.len() < w * h {
                return None;
            }
            let mut rgba = vec![0u8; w * h * 4];
            for i in 0..w * h {
                let v = data[i];
                rgba[i * 4] = v;
                rgba[i * 4 + 1] = v;
                rgba[i * 4 + 2] = v;
                rgba[i * 4 + 3] = 255;
            }
            Some(Pixels {
                width: w as u32,
                height: h as u32,
                rgba,
            })
        }
        4 => decode_blocks(data, w as u32, h as u32, 16, bc3_block),
        6 => decode_blocks(data, w as u32, h as u32, 16, bc2_block),
        7 => decode_blocks(data, w as u32, h as u32, 8, bc1_block),
        _ => None,
    }
}

/// 能把 `.tex` 解成什么：优先按头部描述解裸像素/DXT，解不动再退回"扫内嵌图片容器"
pub fn decode(data: &[u8]) -> Result<Decoded> {
    if let Some(d) = decode_standard(data) {
        return Ok(d);
    }
    // 退路：老路（有些 pkg 的头部版本特殊，但载荷是自包含的 PNG/JPEG）
    let t = parse(data)?;
    Ok(Decoded::Container(t.payload, t.bytes))
}

/// `.tex` 头部声明的 `flags` 位域（**采样状态**由它决定，不是纯装饰）。
/// WE 的位定义（`open-wallpaper-engine/src/Scene/Pkg/Parse/Image/TexImageParser.cpp`）
/// `bit0 = noInterpolation`、`bit1 = clampUVs`，落到 GL 是
/// `clampUVs ? CLAMP_TO_EDGE : REPEAT`、`noInterpolation ? NEAREST : LINEAR`。
/// 我们以前**恒定** CLAMP_TO_EDGE + LINEAR ⇒ 任何没置 `clampUVs` 位的贴图在 WE 里是
/// REPEAT、采样结果不同（本包里就有：`materials/effects/waterflowphase.tex`，32×32，
/// `waterflow.frag` 拿它当 `g_Texture2` 按 `v_TexCoord.xy * g_FlowPhaseScale` 采样，
/// **shader 里没有 `fract`** ⇒ wrap 直接改变云层的流动相位）。
pub fn flags(data: &[u8]) -> Option<u32> {
    let mut r = Reader::new(data);
    let texv = r.tag()?;
    if !texv.starts_with("TEXV") {
        return None;
    }
    let _texi = r.tag()?;
    let _format = r.i32()?;
    r.u32()
}

/// 这张贴图在 WE 里是不是 **REPEAT** 采样（= `clampUVs` 位没置）。
pub fn repeat_wrap(data: &[u8]) -> Option<bool> {
    flags(data).map(|f| f & 0b10 == 0)
}

/// 这张贴图在 WE 里是不是 **NEAREST** 采样（= `noInterpolation` 位置了）。
pub fn nearest_filter(data: &[u8]) -> Option<bool> {
    flags(data).map(|f| f & 0b1 != 0)
}


fn decode_standard(data: &[u8]) -> Option<Decoded> {
    let mut r = Reader::new(data);
    let texv = r.tag()?;
    if !texv.starts_with("TEXV") {
        return None;
    }
    let _texi = r.tag()?;
    let format = r.i32()?;
    let _flags = r.u32()?;
    let _w = r.i32()?;
    let _h = r.i32()?;
    let _mapw = r.i32()?;
    let _maph = r.i32()?;
    let _unknown = r.i32()?;
    let texb = r.tag()?;
    let texb_ver = texb_version(&texb)?;
    if !(1..=4).contains(&texb_ver) {
        return None;
    }
    let image_count = r.i32()?;
    if image_count <= 0 {
        return None;
    }
    // texb>=3：镜像类型槽（-1 = 裸像素；其它值是容器枚举，具体哪种按魔数认）
    if texb_ver >= 3 {
        let _declared_type = r.i32()?;
    }
    if texb_ver >= 4 {
        // 条件补丁：u32 组数，每组 u32 条数，每条 12 字节 + 以 \0 结尾的字符串
        let groups = r.u32()? as usize;
        if groups > r.rest().len() / 4 {
            return None;
        }
        for _ in 0..groups {
            let n = r.u32()? as usize;
            for _ in 0..n {
                r.skip(12)?;
                loop {
                    let b = *r.take(1)?.first()?;
                    if b == 0 {
                        break;
                    }
                }
            }
        }
    }

    // 只取第一张图的第 0 级 mip（渲染用不到其余 mip / 精灵帧）
    let mip_count = r.i32()?;
    if mip_count <= 0 {
        return None;
    }
    let w = r.i32()?;
    let h = r.i32()?;
    let mut lz4 = false;
    let mut decompressed = 0i32;
    if texb_ver >= 2 {
        lz4 = r.i32()? == 1;
        decompressed = r.i32()?;
    }
    let src_size = r.i32()?;
    if src_size <= 0 || w <= 0 || h <= 0 || decompressed < 0 {
        return None;
    }
    let raw = r.take(src_size as usize)?;
    let bytes = if lz4 {
        lz4_decompress(raw, decompressed as usize)?
    } else {
        raw.to_vec()
    };

    // 载荷本身像 PNG/JPEG（很常见：LZ4 压的就是一张图）⇒ 交给图片解码器
    if let Some(p) = sniff(&bytes) {
        return Some(Decoded::Container(p, bytes));
    }
    decode_raw(format, &bytes, w as u32, h as u32).map(Decoded::Rgba)
}

pub fn parse(data: &[u8]) -> Result<Tex> {
    let version = data
        .get(0..8)
        .filter(|v| v.starts_with(b"TEXV"))
        .map(|v| String::from_utf8_lossy(v).trim_end_matches('\0').to_string())
        .ok_or_else(|| Error::BadMagic(String::from_utf8_lossy(&data[..data.len().min(8)]).into()))?;

    let body = find(data, b"TEXB").and_then(|i| data.get(i..i + 8)).map(|v| {
        String::from_utf8_lossy(v)
            .trim_end_matches('\0')
            .to_string()
    });

    // 从 TEXB 之后开始找内嵌数据
    let start = find(data, b"TEXB").map(|i| i + 8).unwrap_or(0);
    let tail = &data[start.min(data.len())..];

    for (magic, kind) in MAGICS {
        let Some(pos) = find(tail, magic) else { continue };
        if *kind == Payload::Webp && tail.get(pos + 8..pos + 12) != Some(b"WEBP") {
            continue;
        }
        let blob = &tail[pos..];
        match kind {
            Payload::Png => {
                let end = png_end(blob).unwrap_or(blob.len());
                let bytes = blob[..end].to_vec();
                let dims = png_dims(&bytes);
                return Ok(Tex {
                    version,
                    body,
                    payload: Payload::Png,
                    bytes,
                    dims,
                });
            }
            _ => {
                return Ok(Tex {
                    version,
                    body,
                    payload: *kind,
                    bytes: blob.to_vec(),
                    dims: None,
                })
            }
        }
    }

    // MP4：box 里第 4..8 字节是 ftyp
    if let Some(pos) = tail.windows(12).position(|w| &w[4..8] == b"ftyp") {
        return Ok(Tex {
            version,
            body,
            payload: Payload::Mp4,
            bytes: tail[pos..].to_vec(),
            dims: None,
        });
    }

    Err(Error::Unsupported(format!(
        "{} 里没有找到内嵌图片（多半是 DXT/R8 裸像素或 LZ4 压缩，需要按 {} 的头部描述解）",
        version,
        body.as_deref().unwrap_or("TEXB")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个最小合法 PNG 头 + IEND（只为测尺寸与截断，不需要是完整可解码图片）
    fn fake_png(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 2, 0, 0, 0]);
        v.extend_from_slice(&[0, 0, 0, 0]); // 假 CRC
        v.extend_from_slice(&0u32.to_be_bytes());
        v.extend_from_slice(b"IEND");
        v.extend_from_slice(&[0, 0, 0, 0]);
        v.extend_from_slice(&[0u8; 7]); // 尾部填充，应当被截断掉
        v
    }

    #[test]
    fn finds_embedded_png_and_dims() {
        let png = fake_png(2000, 1368);
        let mut data = Vec::new();
        data.extend_from_slice(b"TEXV0005\0");
        data.extend_from_slice(b"TEXI0001\0");
        data.extend_from_slice(b"TEXB0003\0");
        data.extend_from_slice(&[0u8; 20]); // 头部元数据（parse 不解析，只扫魔数）
        data.extend_from_slice(&png);
        let t = parse(&data).expect("应能解析");
        assert_eq!(t.version, "TEXV0005");
        assert_eq!(t.body.as_deref(), Some("TEXB0003"));
        assert_eq!(t.payload, Payload::Png);
        assert_eq!(t.dims, Some((2000, 1368)));
        assert_eq!(
            t.bytes.len(),
            png.len() - 7,
            "PNG 应按 IEND 截断，去掉尾部填充"
        );
        assert_eq!(Payload::Png.ext(), "png");
    }

    #[test]
    fn rejects_non_tex() {
        let err = parse(b"not a tex at all").unwrap_err();
        assert!(matches!(err, Error::BadMagic(_)));
    }

    #[test]
    fn raw_pixels_report_unsupported_not_garbage() {
        let mut data = Vec::new();
        data.extend_from_slice(b"TEXV0005\0");
        data.extend_from_slice(b"TEXB0002\0");
        data.extend_from_slice(&[0xAB; 64]);
        let err = parse(&data).unwrap_err();
        match err {
            Error::Unsupported(m) => assert!(m.contains("TEXB0002"), "要说清是哪个版本: {m}"),
            other => panic!("应当是 Unsupported，实际 {other:?}"),
        }
    }

    /// 造一个标准头的 .tex（`fmt` + 可选 lz4 前导 + 载荷）
    fn build_tex(
        texb: i32,
        fmt: i32,
        w: i32,
        h: i32,
        payload: &[u8],
        lz4: bool,
        raw_len: i32,
    ) -> Vec<u8> {
        let mut v = Vec::new();
        fn tag(v: &mut Vec<u8>, s: &str) {
            // 头部标签是定长 9 字节（8 字符 + NUL），别用 u32 长度前缀
            assert_eq!(s.len(), 8);
            v.extend_from_slice(s.as_bytes());
            v.push(0);
        }
        tag(&mut v, "TEXV0005");
        tag(&mut v, "TEXI0001");
        v.extend_from_slice(&fmt.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // flags
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v.extend_from_slice(&w.to_le_bytes()); // mapW
        v.extend_from_slice(&h.to_le_bytes()); // mapH
        v.extend_from_slice(&0i32.to_le_bytes()); // unknown
        let texb_name = format!("TEXB{texb:04}");
        tag(&mut v, &texb_name);
        v.extend_from_slice(&1i32.to_le_bytes()); // image_count
        if texb >= 3 {
            v.extend_from_slice(&(-1i32).to_le_bytes()); // image_type = 裸像素
        }
        v.extend_from_slice(&1i32.to_le_bytes()); // mip_count
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        if texb >= 2 {
            v.extend_from_slice(&(if lz4 { 1i32 } else { 0 }).to_le_bytes());
            v.extend_from_slice(&raw_len.to_le_bytes());
        }
        v.extend_from_slice(&(payload.len() as i32).to_le_bytes());
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn decodes_raw_r8_rg88_and_rgba() {
        // 4x4 全 200 ⇒ 每像素 (200,200,200,255)
        let r8 = build_tex(2, 9, 4, 4, &[200u8; 16], false, 0);
        match decode(&r8).expect("R8 应当能解") {
            Decoded::Rgba(p) => {
                assert_eq!((p.width, p.height), (4, 4));
                assert_eq!(&p.rgba[0..4], &[200, 200, 200, 255]);
            }
            other => panic!("应当是裸像素，得到 {other:?}"),
        }
        // RGBA8888 裸像素
        let mut px = Vec::new();
        for _ in 0..4 {
            px.extend_from_slice(&[1, 2, 3, 4]);
        }
        let rgba_tex = build_tex(2, 0, 2, 2, &px, false, 0);
        match decode(&rgba_tex).unwrap() {
            Decoded::Rgba(p) => assert_eq!(&p.rgba[0..4], &[1, 2, 3, 4]),
            other => panic!("应当是裸像素，得到 {other:?}"),
        }
        // RG88：两通道 → (r, g, 0, 255)
        let rg = build_tex(2, 8, 2, 2, &[10, 20, 30, 40, 50, 60, 70, 80], false, 0);
        match decode(&rg).unwrap() {
            Decoded::Rgba(p) => assert_eq!(&p.rgba[0..4], &[10, 20, 0, 255]),
            other => panic!("应当是裸像素，得到 {other:?}"),
        }
    }

    #[test]
    fn decodes_dxt1_block() {
        // 一个 4x4 的 DXT1 块：color0=红(0xF800), color1=蓝(0x001F), 全用索引 0
        let block: [u8; 8] = [0x00, 0xf8, 0x1f, 0x00, 0x00, 0x00, 0x00, 0x00];
        let tex = build_tex(2, 7, 4, 4, &block, false, 0);
        match decode(&tex).expect("DXT1 应当能解") {
            Decoded::Rgba(p) => {
                assert_eq!((p.width, p.height), (4, 4));
                assert_eq!(p.rgba[0], 255, "红");
                assert_eq!(p.rgba[1], 0);
                assert_eq!(p.rgba[2], 0);
                assert_eq!(p.rgba[3], 255);
            }
            other => panic!("应当是裸像素，得到 {other:?}"),
        }
    }

    #[test]
    fn dxt_handles_non_multiple_of_four_sizes() {
        // 6x3 的 DXT1 ⇒ 2x1 个块，右边/下边要被裁掉
        let block: [u8; 8] = [0x00, 0xf8, 0x1f, 0x00, 0x00, 0x00, 0x00, 0x00];
        let two = [block, block].concat();
        let tex = build_tex(2, 7, 6, 3, &two, false, 0);
        match decode(&tex).expect("非 4 倍数也要能解") {
            Decoded::Rgba(p) => {
                assert_eq!((p.width, p.height), (6, 3));
                assert_eq!(p.rgba.len(), 6 * 3 * 4, "裁剪后尺寸要对");
            }
            other => panic!("应当是裸像素，得到 {other:?}"),
        }
    }

    #[test]
    fn lz4_roundtrip_against_handmade_block() {
        // 手工编一个 LZ4 块：token=0x40 ⇒ 4 个 literal、match 长度 0+4
        // literals "abcd"，offset=4 ⇒ 复制 "abcd"，总长 8
        let src = [0x40u8, b'a', b'b', b'c', b'd', 0x04, 0x00];
        assert_eq!(lz4_decompress(&src, 8).unwrap(), b"abcdabcd");
        assert!(lz4_decompress(&src, 0).is_some(), "长度 0 直接成功");
        assert!(lz4_decompress(&[0x10], 5).is_none(), "literal 不够要失败");
    }

    #[test]
    fn lz4_wrapped_container_is_sniffed_after_decompress() {
        // 载荷是"LZ4 压缩后的 PNG"：解出来才发现是图片容器（PKGV0022+ 的常见形态）
        let png = fake_png(8, 8);
        let mut lz4 = Vec::new();
        // 全 literal：token 高 4 位 = 15，扩展字节之和必须是 (总长 - 15)
        assert!(png.len() > 15);
        let mut rest = png.len() - 15;
        lz4.push(0xf0u8);
        while rest >= 255 {
            lz4.push(255);
            rest -= 255;
        }
        lz4.push(rest as u8);
        lz4.extend_from_slice(&png);
        let tex = build_tex(3, -1, 8, 8, &lz4, true, png.len() as i32);
        match decode(&tex).expect("应当解出容器") {
            Decoded::Container(p, bytes) => {
                assert_eq!(p, Payload::Png);
                assert!(bytes.starts_with(b"\x89PNG"));
            }
            other => panic!("应当是容器，得到 {other:?}"),
        }
    }

    /// `flags` 位域：bit0 = noInterpolation、bit1 = clampUVs。
    /// 采样状态是**数据里写死的**，不是渲染器的自由选择 —— 这条测试钉住解析。
    #[test]
    fn header_flags_drive_wrap_and_filter() {
        // build_tex 写的是 flags = 0 ⇒ WE 会 REPEAT + LINEAR
        let mut t = build_tex(2, 0, 4, 4, &[0u8; 64], false, 0);
        assert_eq!(flags(&t), Some(0));
        assert_eq!(repeat_wrap(&t), Some(true));
        assert_eq!(nearest_filter(&t), Some(false));

        // flags = bit1(clampUVs) ⇒ CLAMP
        let off = 9 + 9 + 4; // TEXV(9) + TEXI(9) + format(4)
        t[off] = 0b10;
        assert_eq!(repeat_wrap(&t), Some(false), "clampUVs 置位 ⇒ 不该判成 REPEAT");
        assert_eq!(nearest_filter(&t), Some(false));

        // flags = bit0|bit1 ⇒ CLAMP + NEAREST
        t[off] = 0b11;
        assert_eq!(repeat_wrap(&t), Some(false));
        assert_eq!(nearest_filter(&t), Some(true), "noInterpolation 置位 ⇒ NEAREST");

        // 不是 TEXV 头 ⇒ 说不清（调用方必须保持旧行为）
        assert_eq!(flags(b"not a tex file at all"), None);
        assert_eq!(repeat_wrap(&[]), None);
    }
}
