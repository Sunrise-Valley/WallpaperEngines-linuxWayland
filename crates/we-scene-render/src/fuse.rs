//! pass 融合（`WE_FUSE_SHIFT`）：把一串连续的「纯位移」特效趟折叠成**一趟**。
//! 语义模型（固定不变）：每趟算 `out(p) = in(c(p))`，`c(p) = p + d(p)`（waterwaves / iris /
//! waterripple）；`effects/shake` 算 `mix(in(p), in(p+d(p)), mask(p))`（两次采样）。
//! n 趟**不是**位移相加，而是**嵌套**：从 `u = p` 起依次 `u := c_i(u)`，最后 `pixel = base(u)`，
//! 其中 `base` = 整串的输入图像。
//! 精确展开 = 路径展开：状态 = [(coord, weight)]，初始 [(p, 1.0)]。确定性趟（单采样）改写
//! coord、权重不变；`mix` 趟（shake 的 MASK 分支）把一条路径**拆成两条**：
//! `(a, w*(1-mask))` 与 `(b, w*mask)`，其中 mask 在**片元自己的 uv** 上采样（不是路径 coord）。
//! 最终颜色 = `sum_k w_k * base(coord_k)`，权重和恒为 1。
//! 本模块只做**文本 → 文本**（不碰 GL），可以单测。

use std::fmt::Write as _;

/// 一个被折进链里的原始趟（= 该趟完整编译后的 GLSL 源码）。
#[derive(Clone, Debug)]
pub struct Stage {
    pub vs: String,
    pub fs: String,
    /// 该趟编译期用的 combo（`PERSPECTIVE=0`、`MASK=1` …）
    pub combos: Vec<(String, i32)>,
    /// 该趟绑定的贴图槽（0 = 本趟输入；其余 = 作者贴图/遮罩）
    pub slots: Vec<u32>,
}

/// 融合结果。
#[derive(Clone, Debug)]
pub struct Fused {
    pub vs: String,
    pub fs: String,
    /// (stage 下标, 该趟的槽, 全局 texture unit)。槽 0 恒映射到 unit 0（整串的输入图像）；
    /// **拼 units 时只取槽 != 0 的项**。
    pub unit_map: Vec<(usize, u32, u32)>,
    /// 最终路径条数（1 = 全程确定性 = 一次采样）
    pub paths: usize,
}

/// 路径条数上限：超过就放弃融合（着色器会又大又慢）。
const MAX_PATHS: usize = 8;

// 字符常量（写成数字，源码里不出现字符字面量）
const US: char = 95u8 as char;
const LP: char = 40u8 as char;
const RP: char = 41u8 as char;
const COM: char = 44u8 as char;
const SEMI: char = 59u8 as char;
const NL: char = 10u8 as char;
const SP: char = 32u8 as char;
const SLASH: u8 = 47;
const STAR: u8 = 42;

fn is_id0(c: char) -> bool {
    c.is_ascii_alphabetic() || c == US
}

fn is_idc(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == US
}

fn is_ident(n: &str) -> bool {
    let mut it = n.chars();
    match it.next() {
        Some(c) => is_id0(c) && it.all(is_idc),
        None => false,
    }
}

/// **共享**标识符（不按 stage 加前缀）。这些名字由绘制层**按原名**下发
/// （`gl_ctx::draw_pass` 里写死的那几支 + 场景级 uniform）；改名就再也收不到值。
pub fn is_shared_name(n: &str) -> bool {
    matches!(
        n,
        "g_Time"
            | "g_ModelViewProjectionMatrix"
            | "g_Brightness"
            | "g_UserAlpha"
            | "g_Alpha"
            | "g_Opacity"
            | "g_Color4"
            | "g_TextureReductionScale"
            | "g_ParallaxPosition"
            | "g_PointerPosition"
            | "g_PointerPositionLast"
            | "a_Position"
            | "a_TexCoord"
    ) || n.starts_with("g_Light")
        || n.starts_with("g_AudioSpectrum")
}

/// 把 `//` 与 `/* */` 注释换成空格，**保持字节长度与换行**（这样下标在原文里也有效）。
fn strip_comments(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len() + 8);
    let mut i = 0usize;
    let mut line = false;
    let mut block = false;
    while i < b.len() {
        if line {
            if b[i] == NL as u8 {
                line = false;
                out.push(NL);
            } else {
                out.push(SP);
            }
            i += 1;
            continue;
        }
        if block {
            if b[i] == STAR && i + 1 < b.len() && b[i + 1] == SLASH {
                out.push_str("  ");
                i += 2;
                block = false;
                continue;
            }
            if b[i] == NL as u8 {
                out.push(NL);
            } else {
                out.push(SP);
            }
            i += 1;
            continue;
        }
        if b[i] == SLASH && i + 1 < b.len() && b[i + 1] == SLASH {
            out.push_str("  ");
            i += 2;
            line = true;
            continue;
        }
        if b[i] == SLASH && i + 1 < b.len() && b[i + 1] == STAR {
            out.push_str("  ");
            i += 2;
            block = true;
            continue;
        }
        let ch = match src[i..].chars().next() {
            Some(c) => c,
            None => break,
        };
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn count_braces(s: &str) -> i32 {
    s.matches("{").count() as i32 - s.matches("}").count() as i32
}

/// 开头那个标识符（函数名 / 宏名）
fn first_ident(s: &str) -> String {
    s.trim_start()
        .chars()
        .take_while(|c| is_idc(*c))
        .collect::<String>()
}

/// `name(...)` 里的 name（不是函数就是 None）
fn func_name(text: &str) -> Option<String> {
    let p = text.find("(")?;
    let head = &text[..p];
    let mut name = String::new();
    for c in head.chars().rev() {
        if is_idc(c) {
            name.insert(0, c);
        } else {
            break;
        }
    }
    if is_ident(&name) {
        Some(name)
    } else {
        None
    }
}

/// `uniform sampler2D g_Texture0; // {...}` → `g_Texture0`
fn decl_name(t: &str) -> Option<String> {
    let t = t.trim();
    if !(t.starts_with("uniform ") || t.starts_with("varying ") || t.starts_with("attribute ")) {
        return None;
    }
    let mut toks = t.split_whitespace();
    toks.next()?;
    toks.next()?;
    let rest: String = toks.collect::<Vec<&str>>().join(" ");
    let nm: String = rest.chars().take_while(|c| is_idc(*c)).collect();
    if nm.is_empty() {
        None
    } else {
        Some(nm)
    }
}

/// `g_Texture3` / `g_Texture3Resolution` / `g_Texture3Texel` → (3, 后缀)
fn tex_name(n: &str) -> Option<(u32, &'static str)> {
    let r = n.strip_prefix("g_Texture")?;
    let digits: String = r.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let k: u32 = digits.parse().ok()?;
    let suf = &r[digits.len()..];
    match suf {
        "" => Some((k, "")),
        "Resolution" => Some((k, "Resolution")),
        "Texel" => Some((k, "Texel")),
        _ => None,
    }
}

/// `src` → (void main 之前的全部文本, main 的花括号体内文本)。注释会被抹掉（等长）。
fn split_main(src: &str) -> Result<(String, String), String> {
    let st = strip_comments(src);
    let p = st.find("void main").ok_or("找不到 void main")?;
    let head = st[..p].to_string();
    let open = p + st[p..].find("{").ok_or("main 缺左花括号")?;
    let b = st.as_bytes();
    let mut depth = 0i32;
    let mut i = open;
    let mut close = None;
    while i < b.len() {
        if b[i] == 123 {
            depth += 1;
        } else if b[i] == 125 {
            depth -= 1;
            if depth == 0 {
                close = Some(i);
                break;
            }
        }
        i += 1;
    }
    let close = close.ok_or("main 的花括号不配平")?;
    Ok((head, st[open + 1..close].to_string()))
}

fn macro_val(m: &[(String, i32)], name: &str) -> Option<i32> {
    m.iter().find(|(k, _)| k == name).map(|(_, v)| *v)
}

fn num_or_macro(t: &str, m: &[(String, i32)]) -> Result<i32, String> {
    let t = t.trim();
    if let Ok(v) = t.parse::<i32>() {
        return Ok(v);
    }
    if is_ident(t) {
        // GLSL：未定义的宏在 #if 里就是 0
        return Ok(macro_val(m, t).unwrap_or(0));
    }
    Err(format!("#if 里无法求值: {t}"))
}

/// `#if` 的表达式求值（只支持这些 shader 里实际出现的形状：NAME / !NAME / A op B）
fn eval_cond(e: &str, m: &[(String, i32)]) -> Result<bool, String> {
    let mut e = e.trim();
    // 去掉包裹的括号
    while e.starts_with("(") && e.ends_with(")") && e.len() > 1 {
        e = e[1..e.len() - 1].trim();
    }
    // `A && B` / `A || B`（`&&` 结合更紧）
    if let Some(p) = e.find("||") {
        return Ok(eval_cond(&e[..p], m)? || eval_cond(&e[p + 2..], m)?);
    }
    if let Some(p) = e.find("&&") {
        return Ok(eval_cond(&e[..p], m)? && eval_cond(&e[p + 2..], m)?);
    }
    let mut neg = false;
    if let Some(r) = e.strip_prefix("!") {
        neg = true;
        e = r.trim();
    }
    let toks: Vec<&str> = e.split_whitespace().collect();
    let v: i32 = match toks.len() {
        1 => num_or_macro(toks[0], m)?,
        3 => {
            let a = num_or_macro(toks[0], m)?;
            let b = num_or_macro(toks[2], m)?;
            match toks[1] {
                "==" => (a == b) as i32,
                "!=" => (a != b) as i32,
                ">" => (a > b) as i32,
                "<" => (a < b) as i32,
                ">=" => (a >= b) as i32,
                "<=" => (a <= b) as i32,
                other => return Err(format!("#if 运算符不支持: {other}")),
            }
        }
        _ => return Err(format!("#if 表达式不支持: {e}")),
    };
    Ok(if neg { v == 0 } else { v != 0 })
}

/// 语句列表里的条件编译：按 combo **就地求值**，丢掉死分支（返回纯 GLSL）。
fn eval_body(body: &str, m: &[(String, i32)]) -> Result<String, String> {
    let mut out = String::new();
    let mut stack: Vec<bool> = Vec::new();
    let act = |s: &Vec<bool>| s.iter().all(|b| *b);
    for line in body.lines() {
        let t = line.trim();
        if let Some(d) = t.strip_prefix("#") {
            let d = d.trim_start();
            let word = first_ident(d);
            let rest = d[word.len()..].trim();
            match word.as_str() {
                "if" => {
                    let v = if act(&stack) { eval_cond(rest, m)? } else { false };
                    stack.push(v);
                }
                "ifdef" | "ifndef" => {
                    let nm = first_ident(rest);
                    let def = m.iter().any(|(k, _)| *k == nm);
                    let v = if word == "ifdef" { def } else { !def };
                    let v = v && act(&stack);
                    stack.push(v);
                }
                "else" => {
                    let top = stack.pop().ok_or("#else 没有 #if")?;
                    let par = act(&stack);
                    stack.push(!top && par);
                }
                "elif" => {
                    let top = stack.pop().ok_or("#elif 没有 #if")?;
                    let par = act(&stack);
                    let v = if par && !top { eval_cond(rest, m)? } else { false };
                    stack.push(v);
                }
                "endif" => {
                    stack.pop().ok_or("#endif 没有 #if")?;
                }
                _ => {}
            }
            continue;
        }
        if act(&stack) {
            out.push_str(line);
            out.push(NL);
        }
    }
    if !stack.is_empty() {
        return Err("#if 未闭合".into());
    }
    Ok(out)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Decl,
    Func,
    Macro,
    Other,
}

#[derive(Clone, Debug)]
struct Unit {
    kind: Kind,
    /// 去重键：`decl:名字` / `func:签名` / `macro:名字` / `other:全文`
    key: String,
    name: Option<String>,
    text: String,
}

/// 函数签名（第一个 `{` 之前，空白压平）—— 重载（`inverse(mat3)` / `inverse(mat4)`）靠它区分
fn norm_sig(text: &str) -> String {
    let head = match text.find("{") {
        Some(p) => &text[..p],
        None => text,
    };
    head.split_whitespace().collect::<Vec<&str>>().join(" ")
}

/// 把 "void main 之前" 的文本切成顶层单元：声明 / 函数 / 宏 / 其它。
/// 条件编译按 `m` 求值 ⇒ 返回的单元全是**活的**（于是按名字去重是安全的）。
fn collect_units(head: &str, m: &[(String, i32)]) -> Result<Vec<Unit>, String> {
    let lines: Vec<&str> = head.lines().collect();
    let mut out: Vec<Unit> = Vec::new();
    let mut conds: Vec<bool> = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim();
        i += 1;
        if t.is_empty() {
            continue;
        }
        if let Some(d) = t.strip_prefix("#") {
            let d = d.trim_start();
            let word = first_ident(d);
            let rest = d[word.len()..].trim();
            match word.as_str() {
                "if" => {
                    let v = if conds.iter().all(|b| *b) {
                        eval_cond(rest, m)?
                    } else {
                        false
                    };
                    conds.push(v);
                }
                "ifdef" | "ifndef" => {
                    let nm = first_ident(rest);
                    let def = m.iter().any(|(k, _)| *k == nm);
                    let v = if word == "ifdef" { def } else { !def };
                    let v = v && conds.iter().all(|b| *b);
                    conds.push(v);
                }
                "else" => {
                    let top = conds.pop().ok_or("#else 没有 #if")?;
                    let par = conds.iter().all(|b| *b);
                    conds.push(!top && par);
                }
                "elif" => {
                    let top = conds.pop().ok_or("#elif 没有 #if")?;
                    let par = conds.iter().all(|b| *b);
                    let v = if par && !top { eval_cond(rest, m)? } else { false };
                    conds.push(v);
                }
                "endif" => {
                    conds.pop().ok_or("#endif 没有 #if")?;
                }
                "define" => {
                    if conds.iter().all(|b| *b) {
                        let mut text = String::from(line);
                        while text.trim_end().ends_with("\\") {
                            if i >= lines.len() {
                                break;
                            }
                            text.push(NL);
                            text.push_str(lines[i]);
                            i += 1;
                        }
                        let nm = first_ident(rest);
                        let key = format!("macro:{nm}");
                        out.push(Unit {
                            kind: Kind::Macro,
                            key,
                            name: if nm.is_empty() { None } else { Some(nm) },
                            text,
                        });
                    }
                }
                _ => {}
            }
            continue;
        }
        if !conds.iter().all(|b| *b) {
            continue;
        }
        if let Some(nm) = decl_name(t) {
            let key = format!("decl:{nm}");
            out.push(Unit {
                kind: Kind::Decl,
                key,
                name: Some(nm),
                text: line.trim_end().to_string(),
            });
            continue;
        }
        // 函数定义：`{` 可能在同一行，也可能在**下一行**（WE 的 common.h 全是这种写法）
        let brace_follows = t.contains("{")
            || (!t.contains(";")
                && i < lines.len()
                && lines[i].trim_start().starts_with("{"));
        if brace_follows {
            let mut text = String::from(line);
            let mut bal = count_braces(line);
            let mut seen = text.contains("{");
            // `{` 可能在下一行：要先吃到它，再等配平
            while i < lines.len() && (!seen || bal > 0) {
                let l = lines[i];
                text.push(NL);
                text.push_str(l);
                bal += count_braces(l);
                if l.contains("{") {
                    seen = true;
                }
                i += 1;
            }
            if !seen || bal != 0 {
                return Err(format!("花括号不配平: {t}"));
            }
            let nm = func_name(&text);
            let key = format!("func:{}", norm_sig(&text));
            out.push(Unit {
                kind: Kind::Func,
                key,
                name: nm,
                text,
            });
            continue;
        }
        if t.contains(";") {
            let text = line.trim_end().to_string();
            let key = format!("other:{}", text.split_whitespace().collect::<Vec<&str>>().join(" "));
            out.push(Unit {
                kind: Kind::Other,
                key,
                name: None,
                text,
            });
        }
    }
    if !conds.is_empty() {
        return Err("#if 未闭合".into());
    }
    Ok(out)
}

/// `#ifndef X` + 紧跟的 `#define X <整数>` ⇒ 该 shader 自己的默认值
fn header_defaults(head: &str) -> Vec<(String, i32)> {
    let lines: Vec<&str> = head.lines().collect();
    let mut out: Vec<(String, i32)> = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let t = lines[i].trim();
        i += 1;
        let Some(r) = t.strip_prefix("#ifndef ") else { continue };
        let nm = first_ident(r);
        if nm.is_empty() {
            continue;
        }
        let mut j = i;
        while j < lines.len() && lines[j].trim().is_empty() {
            j += 1;
        }
        if j >= lines.len() {
            continue;
        }
        let Some(r2) = lines[j].trim().strip_prefix("#define ") else { continue };
        let mut it = r2.split_whitespace();
        if it.next().unwrap_or("") != nm {
            continue;
        }
        if let Some(v) = it.next().and_then(|v| v.parse::<i32>().ok()) {
            if !out.iter().any(|(k, _)| *k == nm) {
                out.push((nm.to_string(), v));
            }
        }
    }
    out
}

/// Sampler slots the shader really declares AFTER conditional compilation.
/// Must go through `collect_units`: a `g_Texture3` inside `#if MASK` is not
/// declared when MASK=0, so it must not eat a global unit either.
fn sampler_slots(head: &str, m: &[(String, i32)]) -> Result<Vec<u32>, String> {
    let mut v: Vec<u32> = Vec::new();
    for u in collect_units(head, m)? {
        if u.kind != Kind::Decl {
            continue;
        }
        if let Some(nm) = u.name {
            if let Some((k, suf)) = tex_name(&nm) {
                if suf.is_empty() && !v.contains(&k) {
                    v.push(k);
                }
            }
        }
    }
    Ok(v)
}

/// 改名上下文：stage 下标 + 该趟的 (槽 → 全局 unit) 表 + 是否顶点阶段
struct RCtx<'a> {
    stage: usize,
    units: &'a [(u32, u32)],
    in_vs: bool,
}

fn map_name(c: &RCtx, n: &str) -> Result<Option<String>, String> {
    if n == "v_TexCoord" {
        // 顶点阶段: 每趟写自己的局部变量, 绝不重写共享 varying
        // 以前每趟都 `vc_uv = a_TexCoord.xyxy` 再按本趟分辨率缩放 .zw
        // 于是只有最后一趟的缩放活下来, 前 n-1 趟的遮罩坐标口径全错
        // (采到别的像素, 位移落到整层)
        return Ok(Some(if c.in_vs {
            format!("s{}_tcv", c.stage)
        } else {
            format!("s{}_tc", c.stage)
        }));
    }
    if is_shared_name(n) {
        return Ok(None);
    }
    if n.starts_with("g_Texture") {
        let (k, suf) = match tex_name(n) {
            Some(x) => x,
            None => return Err(format!("无法识别的贴图名 {n}")),
        };
        return match c.units.iter().find(|(s, _)| *s == k) {
            Some((_, g)) => Ok(Some(format!("g_Texture{}{}", g, suf))),
            None => Err(format!("第 {} 趟引用了未绑定的槽 {n}", c.stage)),
        };
    }
    if n.starts_with("g_") || n.starts_with("v_") {
        return Ok(Some(format!("s{}_{}", c.stage, n)));
    }
    Ok(None)
}

/// 按标识符改名（其它字符原样拷贝）
fn rename(text: &str, c: &RCtx) -> Result<String, String> {
    let mut out = String::with_capacity(text.len() + 64);
    let mut i = 0usize;
    while i < text.len() {
        let ch = match text[i..].chars().next() {
            Some(x) => x,
            None => break,
        };
        if is_id0(ch) {
            let mut j = i + ch.len_utf8();
            for (k, c2) in text[i + ch.len_utf8()..].char_indices() {
                if is_idc(c2) {
                    j = i + ch.len_utf8() + k + c2.len_utf8();
                } else {
                    break;
                }
            }
            let name = &text[i..j];
            match map_name(c, name)? {
                Some(n2) => out.push_str(&n2),
                None => out.push_str(name),
            }
            i = j;
            continue;
        }
        out.push(ch);
        i += ch.len_utf8();
    }
    Ok(out)
}

fn deprefix(t: &str, stage: usize) -> String {
    t.replace(&format!("s{stage}_"), "")
}

fn combo_block(names: &[String], m: &[(String, i32)]) -> String {
    let mut s = String::new();
    for n in names {
        let v = macro_val(m, n).unwrap_or(0);
        let _ = writeln!(s, "#undef {n}");
        let _ = writeln!(s, "#define {n} {v}");
    }
    s
}

fn find_all(t: &str, pat: &str) -> Vec<usize> {
    let mut v = Vec::new();
    let mut from = 0usize;
    while from < t.len() {
        let Some(p) = t[from..].find(pat) else { break };
        let at = from + p;
        let ok_before = at == 0
            || !t[..at]
                .chars()
                .next_back()
                .map(is_idc)
                .unwrap_or(false);
        let end = at + pat.len();
        let ok_after = end >= t.len()
            || !t[end..]
                .chars()
                .next()
                .map(is_idc)
                .unwrap_or(false);
        if ok_before && ok_after {
            v.push(at);
        }
        from = at + pat.len().max(1);
    }
    v
}

/// 包含 `pos` 的那条语句的 [起点, 终点)（';' 分段，也在花括号处断开）
fn stmt_bounds(t: &str, pos: usize) -> (usize, usize) {
    let mut start = 0usize;
    for p in [
        t[..pos].rfind(";"),
        t[..pos].rfind("{"),
        t[..pos].rfind("}"),
    ]
    .into_iter()
    .flatten()
    {
        if p + 1 > start {
            start = p + 1;
        }
    }
    let end = match t[pos..].find(";") {
        Some(p) => pos + p + 1,
        None => t.len(),
    };
    (start, end)
}

/// `gl_FragColor = texSample2D(g_Texture0, EXPR);` → Some(EXPR)
fn parse_tex0_assign(stmt: &str) -> Option<String> {
    let s = stmt.trim();
    let s = s.strip_suffix(";")?.trim();
    let r = s.strip_prefix("gl_FragColor")?.trim_start();
    let r = r.strip_prefix("=")?.trim_start();
    let r = r.strip_prefix("texSample2D")?.trim_start();
    let r = r.strip_prefix("(")?.trim_start();
    let r = r.strip_prefix("g_Texture0")?.trim_start();
    let r = r.strip_prefix(",")?.trim_start();
    let r = r.strip_suffix(")")?.trim_end();
    if r.is_empty() {
        None
    } else {
        Some(r.to_string())
    }
}

fn split_args(s: &str) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in s.chars() {
        if ch == LP {
            depth += 1;
        } else if ch == RP {
            depth -= 1;
        } else if ch == COM && depth == 0 {
            out.push(cur.clone());
            cur.clear();
            continue;
        }
        cur.push(ch);
    }
    out.push(cur);
    Some(out)
}

/// vs body 按顶层 `;` 切语句 (括号/花括号内的分号不切)
fn split_stmts(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for ch in body.chars() {
        match ch {
            LP | '{' | '[' => depth += 1,
            RP | '}' | ']' => depth -= 1,
            ';' if depth <= 0 => {
                out.push(cur.clone());
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// 顶层按 `*` 拆乘性因子 (括号内的不拆)
fn mul_factors(e: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for ch in e.chars() {
        match ch {
            LP => depth += 1,
            RP => depth -= 1,
            '*' if depth <= 0 => {
                out.push(cur.clone());
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    out.push(cur);
    out
}

/// `v_TexCoord.x * E` / `E * v_TexCoord.x` -> Some(E), 其它形状 -> None
fn strip_mul_var(e: &str, var: &str) -> Option<String> {
    let f = mul_factors(e);
    if f.len() < 2 {
        return None;
    }
    let mut rest: Vec<String> = Vec::new();
    let mut hit = false;
    for x in f {
        if !hit && x.trim() == var {
            hit = true;
            continue;
        }
        rest.push(x.trim().to_string());
    }
    if !hit {
        return None;
    }
    let s = rest.join(" * ");
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn mul_into(acc: &str, e: &str) -> String {
    let e = e.trim();
    if acc == "1.0" {
        e.to_string()
    } else {
        format!("({acc}) * ({e})")
    }
}

/// 从一趟的 vs body (已 eval_body) 抽出 `v_TexCoord.zw` 的纯乘性缩放 (sx, sy),
/// 使该趟的 `v_TexCoord.zw == vec2(u.x*sx, u.y*sy)` (u = 该趟片元自己的 uv).
/// 只认这几种写法 (本场景 9 趟全在内), 其余一律 Reject:
/// a) `v_TexCoord = a_TexCoord.xyxy` / `v_TexCoord.xy = a_TexCoord` => sx=sy=1
/// b) `v_TexCoord.z *= E` / `v_TexCoord.w *= E` => 累乘
/// c) `v_TexCoord.zw = vec2(v_TexCoord.x * E1, v_TexCoord.y * E2)` => 累乘 E1/E2
fn vs_uv_scale(body: &str, st: usize) -> Result<(String, String), String> {
    let mut sx = String::from("1.0");
    let mut sy = String::from("1.0");
    for raw in split_stmts(body) {
        let t = raw.trim();
        if t.is_empty() || find_all(t, "v_TexCoord").is_empty() {
            continue;
        }
        let after = match t.strip_prefix("v_TexCoord") {
            Some(r) if !r.chars().next().map(is_idc).unwrap_or(false) => r.trim_start(),
            _ => {
                return Err(format!(
                    "第 {st} 趟 vs: 这处 v_TexCoord 不是对它的赋值, 不支持融合: {t}"
                ))
            }
        };
        let (comps, rest) = match after.strip_prefix('.') {
            Some(r) => {
                let c: String = r.chars().take_while(|c| "xyzw".contains(*c)).collect();
                let n = c.len();
                (c, r[n..].trim_start())
            }
            None => (String::new(), after),
        };
        let (star, rhs) = match rest.strip_prefix("*=") {
            Some(r) => (true, r.trim()),
            None => match rest.strip_prefix('=') {
                Some(r) if !r.starts_with('=') => (false, r.trim()),
                _ => {
                    return Err(format!(
                        "第 {st} 趟 vs: v_TexCoord 这条语句不认识 (只认 `=` / `*=`): {t}"
                    ))
                }
            },
        };
        match (comps.as_str(), star) {
            ("", false) | ("xy", false) => {
                if rhs == "a_TexCoord.xyxy" || rhs == "a_TexCoord" {
                    sx = "1.0".to_string();
                    sy = "1.0".to_string();
                } else {
                    return Err(format!(
                        "第 {st} 趟 vs: `v_TexCoord.{comps} = {rhs}` 不是 a_TexCoord.xyxy / a_TexCoord, 不支持"
                    ));
                }
            }
            ("z", true) => {
                if !find_all(rhs, "v_TexCoord").is_empty() {
                    return Err(format!(
                        "第 {st} 趟 vs: `v_TexCoord.z *= {rhs}` 右边也读 v_TexCoord, 不支持"
                    ));
                }
                sx = mul_into(&sx, rhs);
            }
            ("w", true) => {
                if !find_all(rhs, "v_TexCoord").is_empty() {
                    return Err(format!(
                        "第 {st} 趟 vs: `v_TexCoord.w *= {rhs}` 右边也读 v_TexCoord, 不支持"
                    ));
                }
                sy = mul_into(&sy, rhs);
            }
            ("zw", false) => {
                let inner = rhs
                    .strip_prefix("vec2")
                    .map(|r| r.trim_start())
                    .and_then(|r| r.strip_prefix('('))
                    .and_then(|r| r.strip_suffix(')'))
                    .ok_or_else(|| {
                        format!("第 {st} 趟 vs: `v_TexCoord.zw = {rhs}` 不是 vec2(...), 不支持")
                    })?;
                let args = split_args(inner)
                    .ok_or_else(|| format!("第 {st} 趟 vs: vec2(...) 拆不开: {rhs}"))?;
                if args.len() != 2 {
                    return Err(format!(
                        "第 {st} 趟 vs: `v_TexCoord.zw = vec2(...)` 不是恰好两个分量: {rhs}"
                    ));
                }
                let ex = strip_mul_var(&args[0], "v_TexCoord.x").ok_or_else(|| {
                    format!(
                        "第 {st} 趟 vs: v_TexCoord.zw 的 x 分量不是 `v_TexCoord.x * E` / `E * v_TexCoord.x`: {}",
                        args[0].trim()
                    )
                })?;
                let ey = strip_mul_var(&args[1], "v_TexCoord.y").ok_or_else(|| {
                    format!(
                        "第 {st} 趟 vs: v_TexCoord.zw 的 y 分量不是 `v_TexCoord.y * E` / `E * v_TexCoord.y`: {}",
                        args[1].trim()
                    )
                })?;
                sx = mul_into(&sx, &ex);
                sy = mul_into(&sy, &ey);
            }
            _ => {
                return Err(format!(
                    "第 {st} 趟 vs: 写法不支持 `v_TexCoord.{comps} {} ...`: {t}",
                    if star { "*=" } else { "=" }
                ))
            }
        }
    }
    Ok((sx, sy))
}

/// 文本里出现的 `g_TextureKResolution` / `g_TextureKT...` (带后缀的贴图 uniform 名)
fn referenced_tex_uniforms(src: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < src.len() {
        let ch = match src[i..].chars().next() {
            Some(c) => c,
            None => break,
        };
        if is_id0(ch) {
            let mut j = i + ch.len_utf8();
            for (k, c2) in src[i + ch.len_utf8()..].char_indices() {
                if is_idc(c2) {
                    j = i + ch.len_utf8() + k + c2.len_utf8();
                } else {
                    break;
                }
            }
            let name = &src[i..j];
            if let Some((_, suf)) = tex_name(name) {
                if !suf.is_empty() && !out.iter().any(|x| x == name) {
                    out.push(name.to_string());
                }
            }
            i = j;
            continue;
        }
        i += ch.len_utf8();
    }
    out
}

/// `gl_FragColor = mix(texSample2D(g_Texture0, A), gl_FragColor, M);` → Some((A, M))
fn parse_mix(stmt: &str) -> Option<(String, String)> {
    let s = stmt.trim();
    let s = s.strip_suffix(";")?.trim();
    let r = s.strip_prefix("gl_FragColor")?.trim_start();
    let r = r.strip_prefix("=")?.trim_start();
    let r = r.strip_prefix("mix")?.trim_start();
    let inner = r.strip_prefix("(")?.strip_suffix(")")?.trim();
    let args = split_args(inner)?;
    if args.len() != 3 {
        return None;
    }
    if args[1].trim() != "gl_FragColor" {
        return None;
    }
    let e = args[0].trim();
    let e = e.strip_prefix("texSample2D")?.trim_start();
    let e = e.strip_prefix("(")?.trim_start();
    let e = e.strip_prefix("g_Texture0")?.trim_start();
    let e = e.strip_prefix(",")?.trim();
    if e.is_empty() {
        return None;
    }
    Some((e.to_string(), args[2].trim().to_string()))
}

fn check_pre(pre: &str, st: usize) -> Result<(), String> {
    if !find_all(pre, "gl_FragColor").is_empty() {
        return Err(format!("第 {st} 趟：body 里除了末尾还用了 gl_FragColor，不可融合"));
    }
    if !find_all(pre, "g_Texture0").is_empty() {
        return Err(format!("第 {st} 趟：body 里除了末尾还采了 g_Texture0，不可融合"));
    }
    if !find_all(pre, "return").is_empty() {
        return Err(format!("第 {st} 趟：body 里有 return，不可融合"));
    }
    Ok(())
}

fn make_fn(st: usize, pre: &str, a: &str, b: &str, w: &str, sx: &str, sy: &str) -> String {
    let mut f = String::new();
    let _ = writeln!(
        f,
        "void s{}_op(vec2 u, out vec2 oA, out vec2 oB, out float oW) {{",
        st
    );
    // 本趟的遮罩坐标 = 当前路径坐标 u 乘以本趟自己的缩放 (从本趟 vs body 抽出来的)
    // 以前是 `vec4(u, uv0.zw)`: uv0 只有末趟的缩放, 前 n-1 趟采到的是别的像素
    let _ = writeln!(
        f,
        "    vec4 s{}_tc = vec4(u, vec2(u.x * ({}), u.y * ({})));",
        st, sx, sy
    );
    f.push_str(pre);
    if !f.ends_with(NL) {
        f.push(NL);
    }
    let _ = writeln!(f, "    oA = {};", a);
    let _ = writeln!(f, "    oB = {};", b);
    let _ = writeln!(f, "    oW = {};", w);
    f.push_str("}\n");
    f
}

/// `vec4 NAME = texSample2D(g_Texture0, EXPR);` -> Some(EXPR)
fn parse_sample_assign(stmt: &str, name: &str) -> Option<String> {
    let s = stmt.trim();
    let s = s.strip_suffix(";")?.trim();
    let r = s.strip_prefix("vec4")?.trim_start();
    let r = r.strip_prefix(name)?.trim_start();
    let r = r.strip_prefix("=")?.trim_start();
    let r = r.strip_prefix("texSample2D")?.trim_start();
    let r = r.strip_prefix("(")?.trim_start();
    let r = r.strip_prefix("g_Texture0")?.trim_start();
    let r = r.strip_prefix(",")?.trim_start();
    let r = r.strip_suffix(")")?.trim_end();
    if r.is_empty() {
        None
    } else {
        Some(r.to_string())
    }
}

/// 去掉所有空白, 只用来比对固定形态的语句 (缩进/空格随作者 shader 变)。
fn norm_stmt(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// `shape` 尾巴 (`WE_WAVE_NPHASE=shape`, 默认档) 的识别。形态固定四行
/// vec4 weDisp = texSample2D(g_Texture0, texCoord);
/// vec4 weRest = texSample2D(g_Texture0, v_TexCoord.xy);
/// vec3 weRgb = mix(weRest.rgb, weDisp.rgb, weDisp.a);
/// gl_FragColor = vec4(min(weRgb, vec3(weRest.a)), weRest.a);
/// 该趟 = 一次确定性位移采样 (路径继续往前走) + 收尾时把 alpha 钉回原 uv。
/// 返回 None = 不是这个形态 (交给后面的 mix / 单采样分支)。
fn shape_op(
    st: usize,
    t: &str,
    stmt1: &str,
    s1: usize,
    e1: usize,
    sx: &str,
    sy: &str,
) -> Option<Result<(String, bool, bool), String>> {
    let want_out = norm_stmt("gl_FragColor = vec4(min(weRgb, vec3(weRest.a)), weRest.a);");
    if norm_stmt(stmt1) != want_out {
        return None;
    }
    let prev1 = s1.checked_sub(1)?;
    let (s_rgb, _e_rgb) = stmt_bounds(t, prev1);
    let want_rgb = norm_stmt("vec3 weRgb = mix(weRest.rgb, weDisp.rgb, weDisp.a);");
    if norm_stmt(&t[s_rgb..s1]) != want_rgb {
        return None;
    }
    let prev2 = s_rgb.checked_sub(1)?;
    let (s_rest, e_rest) = stmt_bounds(t, prev2);
    let rest_expr = parse_sample_assign(&t[s_rest..e_rest], "weRest")?;
    let prev3 = s_rest.checked_sub(1)?;
    let (s_disp, e_disp) = stmt_bounds(t, prev3);
    let disp_expr = parse_sample_assign(&t[s_disp..e_disp], "weDisp")?;
    // rest 必须采在本趟的原 uv 上 (`v_TexCoord.xy` 改名成 s{st}_tc.xy),
    // 否则不是 的 shape 形态, 不能按这个语义融合。
    if norm_stmt(&rest_expr) != format!("s{}_tc.xy", st) {
        return None;
    }
    let mut pre = String::new();
    pre.push_str(&t[..s_disp]);
    pre.push_str(&t[e1..]);
    if let Err(e) = check_pre(&pre, st) {
        return Some(Err(e));
    }
    Some(Ok((
        make_fn(st, &pre, &disp_expr, &disp_expr, "0.0", sx, sy),
        false,
        true,
    )))
}

/// 一条链要么全是 shape 尾巴, 要么全不是。混着融等于把两种语义当成一种 (画面必错)。
fn shape_group_ok(shapes: &[bool], splits: &[bool]) -> Result<(), String> {
    if !shapes.iter().any(|x| *x) {
        return Ok(());
    }
    if !shapes.iter().all(|x| *x) {
        return Err("一段链里混了 shape 尾巴的趟与非 shape 趟, 放弃融合".into());
    }
    if splits.iter().any(|x| *x) {
        return Err("shape 尾巴的趟按定义不分裂路径".into());
    }
    Ok(())
}

/// shape 段的收尾: alpha 取原 uv 的 base alpha, rgb 取路径展开后的位移采样,
/// 再套一次 的 mix(rest.rgb, disp.rgb, disp.a) + min(., rest.a) 复合。
fn shape_tail_main(n: usize) -> String {
    let mut s = String::new();
    s.push_str("    vec4 weRest = texSample2D(g_Texture0, vc_uv.xy);\n");
    s.push_str("    vec4 weDisp = ");
    for p in 0..n {
        if p > 0 {
            s.push_str(" + ");
        }
        let _ = write!(s, "w{} * texSample2D(g_Texture0, c{})", p, p);
    }
    s.push_str(";\n");
    s.push_str("    gl_FragColor = vec4(min(mix(weRest.rgb, weDisp.rgb, weDisp.a), vec3(weRest.a)), weRest.a);\n");
    s.push_str("}\n");
    s
}

/// 一个 stage 的 body → `s{i}_op(...)` 函数（返回 (函数文本, 是否分裂路径)）
fn build_op(
    st: usize,
    body: &str,
    macros: &[(String, i32)],
    c: &RCtx,
    sx: &str,
    sy: &str,
) -> Result<(String, bool, bool), String> {
    let evaluated = eval_body(body, macros)?;
    let renamed = rename(&evaluated, c)?;
    let t = renamed.trim().to_string();
    let occ = find_all(&t, "gl_FragColor");
    if occ.is_empty() {
        return Err(format!("第 {st} 趟：没有 gl_FragColor 赋值"));
    }
    let last = *occ.last().unwrap();
    let (s1, e1) = stmt_bounds(&t, last);
    let stmt1 = t[s1..e1].trim().to_string();
    if let Some(r) = shape_op(st, &t, &stmt1, s1, e1, sx, sy) {
        return r;
    }
    if let Some((ea, m)) = parse_mix(&stmt1) {
        // 分裂趟：`mix(texSample2D(g_Texture0, A), gl_FragColor, M)`
        let before: Vec<usize> = occ.iter().copied().filter(|p| *p < s1).collect();
        if before.is_empty() {
            return Err(format!("第 {st} 趟：mix 之前没有 base 采样"));
        }
        let (s0, e0) = stmt_bounds(&t, *before.last().unwrap());
        let eb = match parse_tex0_assign(t[s0..e0].trim()) {
            Some(x) => x,
            None => return Err(format!("第 {st} 趟：mix 之前那条不是 texSample2D(g_Texture0, B)")),
        };
        if e0 > s1 {
            return Err(format!("第 {st} 趟：两处 gl_FragColor 的语句顺序不认识"));
        }
        let mut pre = String::new();
        pre.push_str(&t[..s0]);
        pre.push_str(&t[e0..s1]);
        pre.push_str(&t[e1..]);
        check_pre(&pre, st)?;
        return Ok((make_fn(st, &pre, &ea, &eb, &m, sx, sy), true, false));
    }
    if let Some(e) = parse_tex0_assign(&stmt1) {
        // 确定性趟：`gl_FragColor = texSample2D(g_Texture0, EXPR)`
        let mut pre = String::new();
        pre.push_str(&t[..s1]);
        pre.push_str(&t[e1..]);
        check_pre(&pre, st)?;
        return Ok((make_fn(st, &pre, &e, &e, "0.0", sx, sy), false, false));
    }
    Err(format!(
        "第 {st} 趟：末尾不是 texSample2D(g_Texture0, EXPR) 也不是 mix(base, gl_FragColor, M)"
    ))
}

/// 路径展开的 main（返回 (main 文本, 路径条数)）
fn build_main(splits: &[bool], shape: bool) -> Result<(String, usize), String> {
    let mut m = String::new();
    m.push_str("void main() {\n");
    m.push_str("    vec2 c0 = vc_uv.xy;\n");
    m.push_str("    float w0 = 1.0;\n");
    let mut n = 1usize;
    // 嵌套复合: 最后画的趟最先作用在 p 上, 所以必须倒序 (i 从 n-1 到 0)
    // 正序会把复合次序弄反 (每趟都在别人的输出空间里算位移)
    for (i, sp) in splits.iter().enumerate().rev() {
        let before = n;
        for p in 0..before {
            if *sp {
                let np = n;
                n += 1;
                if n > MAX_PATHS {
                    return Err(format!("路径数会超过 {MAX_PATHS}，放弃融合"));
                }
                let _ = writeln!(m, "    {{ vec2 a{np}; vec2 b{np}; float ww{np};");
                let _ = writeln!(m, "        s{i}_op(c{p}, a{np}, b{np}, ww{np});");
                let _ = writeln!(m, "        c{np} = b{np}; w{np} = w{p} * ww{np};");
                let _ = writeln!(m, "        c{p} = a{np}; w{p} = w{p} * (1.0 - ww{np}); }}");
            } else {
                let _ = writeln!(
                    m,
                    "    {{ vec2 a; vec2 b; float ww; s{i}_op(c{p}, a, b, ww); c{p} = a; }}"
                );
            }
        }
    }
    if std::env::var_os("WE_FUSE_UVDBG").is_some() {
        // debug: paint the fragment uv itself (vg: x -> red, y -> green)
        m.push_str("    gl_FragColor = vec4(vc_uv.x, vc_uv.y, 0.0, 1.0);\n");
        m.push_str("}\n");
        return Ok((m, 1));
    }
    if std::env::var_os("WE_FUSE_IDENTITY").is_some() {
        // debug: prove the plumbing (unit 0 = run input, geometry, uv) by copying it out
        m.push_str("    gl_FragColor = texSample2D(g_Texture0, vc_uv.xy);\n");
        m.push_str("}\n");
        return Ok((m, 1));
    }
    if shape {
        m.push_str(&shape_tail_main(n));
        return Ok((m, n));
    }
    if n == 1 {
        m.push_str("    gl_FragColor = texSample2D(g_Texture0, c0);\n");
    } else {
        let mut acc = String::new();
        for p in 0..n {
            if p > 0 {
                acc.push_str(" + ");
            }
            let _ = write!(acc, "w{p} * texSample2D(g_Texture0, c{p})");
        }
        let _ = writeln!(m, "    gl_FragColor = {acc};");
    }
    m.push_str("}\n");
    Ok((m, n))
}

fn version_line(head: &str) -> Option<String> {
    head.lines()
        .find(|l| l.trim_start().starts_with("#version"))
        .map(|l| l.trim().to_string())
}

fn push_extensions(out: &mut String, heads: &[String]) {
    for h in heads {
        for l in h.lines() {
            if l.trim_start().starts_with("#extension") {
                let t = l.trim();
                if !out.contains(t) {
                    out.push_str(t);
                    out.push(NL);
                }
            }
        }
    }
}

/// 把一串 stage 融合成一趟。
pub fn compose(stages: &[Stage]) -> Result<Fused, String> {
    if stages.len() < 2 {
        return Err("至少需要 2 趟才能融合".into());
    }
    let n = stages.len();
    let mut heads: Vec<String> = Vec::with_capacity(n);
    let mut bodies: Vec<String> = Vec::with_capacity(n);
    let mut vheads: Vec<String> = Vec::with_capacity(n);
    let mut vbodies: Vec<String> = Vec::with_capacity(n);
    let mut macros: Vec<Vec<(String, i32)>> = Vec::with_capacity(n);
    let mut units_of: Vec<Vec<(u32, u32)>> = Vec::with_capacity(n);
    let mut macros_all: Vec<String> = Vec::new();
    let mut unit_map: Vec<(usize, u32, u32)> = Vec::new();
    // 全局 texture unit 计数器：跨 stage 连续递增（unit 0 = 整串的输入图）
    let mut g: u32 = 0;

    for (i, st) in stages.iter().enumerate() {
        if st.combos.iter().any(|(k, v)| k == "PERSPECTIVE" && *v != 0) {
            return Err(format!("第 {i} 趟 PERSPECTIVE != 0（透视参数化不支持）"));
        }
        if st.fs.contains("#include") || st.vs.contains("#include") {
            return Err(format!("第 {i} 趟含 #include"));
        }
        let (h, b) = split_main(&st.fs).map_err(|e| format!("第 {i} 趟 fs: {e}"))?;
        let (vh, vb) = split_main(&st.vs).map_err(|e| format!("第 {i} 趟 vs: {e}"))?;
        let mut mac = header_defaults(&h);
        for (k, v) in header_defaults(&vh) {
            if !mac.iter().any(|(x, _)| *x == k) {
                mac.push((k, v));
            }
        }
        // 头里的 `#ifndef X / #define X v` 是已经编译进源码的真值
        // (`compose_stages_with` 把 combo 烘成了这对宏), 而 `st.combos` 只是当时的输入,
        // 可能正是被头覆盖掉的那个, 所以头优先, combos 只补头里没有的名字
        for (k, v) in &st.combos {
            if !mac.iter().any(|(x, _)| x == k) {
                mac.push((k.clone(), *v));
            }
        }
        for (k, _) in mac.iter() {
            if !macros_all.iter().any(|x| x == k) {
                macros_all.push(k.clone());
            }
        }
        let mut slots: Vec<u32> = st.slots.clone();
        for k in sampler_slots(&h, &mac)? {
            if !slots.contains(&k) {
                slots.push(k);
            }
        }
        for k in sampler_slots(&vh, &mac)? {
            if !slots.contains(&k) {
                slots.push(k);
            }
        }
        slots.push(0);
        slots.sort_unstable();
        slots.dedup();
        unit_map.push((i, 0, 0));
        let mut ut: Vec<(u32, u32)> = vec![(0, 0)];
        for k in slots.iter().copied() {
            if k == 0 {
                continue;
            }
            g += 1;
            ut.push((k, g));
            unit_map.push((i, k, g));
        }
        heads.push(h);
        bodies.push(b);
        vheads.push(vh);
        vbodies.push(vb);
        macros.push(mac);
        units_of.push(ut);
    }

    // fragment ----------
    let mut fs = String::new();
    match version_line(&heads[0]) {
        Some(v) => {
            fs.push_str(&v);
            fs.push(NL);
        }
        None => fs.push_str("#version 120\n"),
    }
    push_extensions(&mut fs, &heads);
    let _ = writeln!(fs, "// ---- fused: {n} stages folded into one pass ----");
    fs.push_str("varying vec4 vc_uv;\n");

    let mut decls = String::new();
    let mut shared = String::new();
    let mut seen_decl: Vec<String> = Vec::new();
    let mut seen_unit: Vec<(String, String)> = Vec::new();
    for i in 0..n {
        let c = RCtx {
            stage: i,
            units: &units_of[i],
            in_vs: false,
        };
        for u in collect_units(&heads[i], &macros[i])? {
            if u.kind == Kind::Decl {
                let nm = match u.name.clone() {
                    Some(x) => x,
                    None => continue,
                };
                if nm == "v_TexCoord" || nm == "vc_uv" {
                    continue;
                }
                let rn = rename(&nm, &c)?;
                if seen_decl.iter().any(|x| *x == rn) {
                    continue;
                }
                seen_decl.push(rn);
                let mut txt = rename(&u.text, &c)?;
                txt.push(NL);
                decls.push_str(&txt);
                continue;
            }
            let key = u.key.clone();
            let norm = deprefix(&u.text, i);
            if let Some(prev) = seen_unit.iter().find(|(k, _)| *k == key) {
                if prev.1 != norm {
                    let a = norm.lines().next().unwrap_or("");
                    let b = prev.1.lines().next().unwrap_or("");
                    return Err(format!(
                        "第 {i} 趟的 {key} 与之前的内容不同（helper 冲突）：new [{a}] old [{b}]"
                    ));
                }
                continue;
            }
            seen_unit.push((key, norm));
            let mut txt = rename(&u.text, &c)?;
            txt.push(NL);
            shared.push_str(&txt);
        }
    }

    // 每趟 `v_TexCoord.zw` 的纯乘性缩放 (从该趟 vs body 抽出来, 片元里按当前路径坐标 u 复算,
    // 于是第 i 趟的遮罩仍采在自己的 uv 上, 而不是继承末趟的 p)
    let mut scales: Vec<(String, String)> = Vec::with_capacity(n);
    for i in 0..n {
        let vb = eval_body(&vbodies[i], &macros[i])
            .map_err(|e| format!("第 {i} 趟 vs body: {e}"))?;
        let (rx, ry) = vs_uv_scale(&vb, i)?;
        let c = RCtx {
            stage: i,
            units: &units_of[i],
            in_vs: false,
        };
        scales.push((rename(&rx, &c)?, rename(&ry, &c)?));
    }

    let mut ops = String::new();
    let mut splits: Vec<bool> = Vec::new();
    let mut shapes: Vec<bool> = Vec::new();
    for i in 0..n {
        let c = RCtx {
            stage: i,
            units: &units_of[i],
            in_vs: false,
        };
        ops.push_str(&combo_block(&macros_all, &macros[i]));
        let (f, sp, sh) = build_op(i, &bodies[i], &macros[i], &c, &scales[i].0, &scales[i].1)?;
        ops.push_str(&f);
        splits.push(sp);
        shapes.push(sh);
    }
    shape_group_ok(&shapes, &splits)?;
    let shape_all = *shapes.first().unwrap_or(&false);
    let (main_txt, paths) = build_main(&splits, shape_all)?;

    // `g_TextureKResolution` 通常只在顶点头里声明 (片元不用它), 但融合后片元要靠它算遮罩缩放的数值,
    // 所以按 ops 里的实际引用把缺的 vec4 uniform 补进片元声明.
    // 上传端 (`gl_ctx::draw_pass`) 对每个 unit 都下发 `g_TextureNResolution`, 值与非融合趟一致.
    for nm in referenced_tex_uniforms(&ops) {
        if find_all(&decls, &nm).is_empty() {
            let _ = writeln!(decls, "uniform vec4 {nm};");
        }
    }

    fs.push_str(&decls);
    fs.push_str(&shared);
    fs.push_str(&ops);
    fs.push_str(&main_txt);

    // vertex ----------
    let mut vs = String::new();
    match version_line(&vheads[0]) {
        Some(v) => {
            vs.push_str(&v);
            vs.push(NL);
        }
        None => vs.push_str("#version 120\n"),
    }
    push_extensions(&mut vs, &vheads);
    vs.push_str("varying vec4 vc_uv;\n");
    let mut vdecls = String::new();
    let mut vshared = String::new();
    let mut vseen_decl: Vec<String> = Vec::new();
    let mut vseen_unit: Vec<(String, String)> = Vec::new();
    for i in 0..n {
        let c = RCtx {
            stage: i,
            units: &units_of[i],
            in_vs: true,
        };
        for u in collect_units(&vheads[i], &macros[i])? {
            if u.kind == Kind::Decl {
                let nm = match u.name.clone() {
                    Some(x) => x,
                    None => continue,
                };
                if nm == "v_TexCoord" || nm == "vc_uv" {
                    continue;
                }
                let rn = rename(&nm, &c)?;
                if vseen_decl.iter().any(|x| *x == rn) {
                    continue;
                }
                vseen_decl.push(rn);
                let mut txt = rename(&u.text, &c)?;
                txt.push(NL);
                vdecls.push_str(&txt);
                continue;
            }
            let key = u.key.clone();
            let norm = deprefix(&u.text, i);
            if let Some(prev) = vseen_unit.iter().find(|(k, _)| *k == key) {
                if prev.1 != norm {
                    return Err(format!("第 {i} 趟 vs 的 {key} 内容不同（helper 冲突）"));
                }
                continue;
            }
            vseen_unit.push((key, norm));
            let mut txt = rename(&u.text, &c)?;
            txt.push(NL);
            vshared.push_str(&txt);
        }
    }
    vs.push_str(&vdecls);
    vs.push_str(&vshared);
    vs.push_str("void main() {\n");
    // 共享 varying 只在这里写一次. 以前每趟都重写 `vc_uv` 并按本趟分辨率缩放 .zw,
    // 结果只有最后一趟活下来, 前面各趟的遮罩坐标口径全错.
    vs.push_str("    vc_uv = a_TexCoord.xyxy;\n");
    for i in 0..n {
        let c = RCtx {
            stage: i,
            units: &units_of[i],
            in_vs: true,
        };
        let b = eval_body(&vbodies[i], &macros[i])
            .map_err(|e| format!("第 {i} 趟 vs body: {e}"))?;
        let r = rename(&b, &c)?;
        // 每趟自己的局部 uv. 包在花括号块里, 声明仍然落在块首 (GLSL 1.20 稳妥)
        let _ = writeln!(
            vs,
            "    {{ vec4 s{i}_tcv = vec4(a_TexCoord.xy, a_TexCoord.xy);"
        );
        vs.push_str(&r);
        if !vs.ends_with(NL) {
            vs.push(NL);
        }
        vs.push_str("    }\n");
    }
    vs.push_str("}\n");

    // Keep only slots that are really referenced. WE declares combo-gated samplers
    // (`uniform sampler2D g_Texture3; // {"combo":"MASK"}`) that stay unbound when the
    // combo is off; those must not eat a global unit (and the caller has no source for them).
    // NOTE: strip declaration lines first -- a `uniform sampler2D g_TextureK;` line is not a use.
    let strip_decls = |src: &str| -> String {
        src.lines()
            .filter(|l| {
                let t = l.trim_start();
                !(t.starts_with("uniform ")
                    || t.starts_with("varying ")
                    || t.starts_with("attribute "))
            })
            .collect::<Vec<&str>>()
            .join("\n")
    };
    let fs_ref = strip_decls(&fs);
    let vs_ref = strip_decls(&vs);
    let referenced = |g: u32| -> bool {
        [format!("g_Texture{g}"), format!("g_Texture{g}Resolution"), format!("g_Texture{g}Texel")]
            .iter()
            .any(|n| !find_all(&fs_ref, n).is_empty() || !find_all(&vs_ref, n).is_empty())
    };
    unit_map.retain(|(_, slot, g)| *slot == 0 || referenced(*g));

    Ok(Fused {
        vs,
        fs,
        unit_map,
        paths,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 头里的 `#ifndef MASK / #define MASK <v>` 是**已经编译进源码**的真值
    /// (`compose_stages_with` 把 combo 烘成了这对宏), compose 现在以它为准
    /// ⇒ 要测 MASK=0 必须造 MASK=0 的源码, 在 combos 里塞 0 已经不起作用.
    fn prelude_mask(_fs: bool, mask: i32) -> String {
        let mut s = String::new();
        s.push_str("#version 120\n");
        s.push_str("#define mul(a, b) ((b) * (a))\n");
        s.push_str("#ifndef PERSPECTIVE\n#define PERSPECTIVE 0\n#endif\n");
        s.push_str(&format!("#ifndef MASK\n#define MASK {mask}\n#endif\n"));
        s
    }

    fn prelude(fs: bool) -> String {
        prelude_mask(fs, 1)
    }

    fn vs_wave_m(mask: i32) -> String {
        let mut s = prelude_mask(false, mask);
        s.push_str("uniform mat4 g_ModelViewProjectionMatrix;\n");
        s.push_str("uniform vec4 g_Texture1Resolution;\n");
        s.push_str("uniform float g_Direction;\n");
        s.push_str("attribute vec3 a_Position;\n");
        s.push_str("attribute vec2 a_TexCoord;\n");
        s.push_str("varying vec4 v_TexCoord;\n");
        s.push_str("varying vec2 v_Direction;\n");
        s.push_str("void main() {\n");
        s.push_str("\tgl_Position = mul(vec4(a_Position, 1.0), g_ModelViewProjectionMatrix);\n");
        s.push_str("\tv_TexCoord = a_TexCoord.xyxy;\n");
        s.push_str("#if MASK\n");
        s.push_str("\tv_TexCoord.z *= g_Texture1Resolution.z / g_Texture1Resolution.x;\n");
        s.push_str("#endif\n");
        s.push_str("\tv_Direction = vec2(sin(g_Direction), cos(g_Direction));\n");
        s.push_str("}\n");
        s
    }

    fn vs_wave() -> String {
        vs_wave_m(1)
    }

    fn fs_wave() -> String {
        let mut s = prelude(true);
        s.push_str("uniform sampler2D g_Texture0; // {\"hidden\":true}\n");
        s.push_str("uniform sampler2D g_Texture1;\n");
        s.push_str("uniform float g_Strength;\n");
        s.push_str("#define M_PI 3.14159265359\n");
        s.push_str("float greyscale(vec3 c)\n{\n\treturn dot(c, vec3(0.11, 0.59, 0.3));\n}\n");
        s.push_str("varying vec4 v_TexCoord;\n");
        s.push_str("varying vec2 v_Direction;\n");
        s.push_str("void main() {\n");
        s.push_str("#if MASK\n");
        s.push_str("\tfloat mask = texSample2D(g_Texture1, v_TexCoord.zw).r;\n");
        s.push_str("#else\n");
        s.push_str("\tfloat mask = 1.0;\n");
        s.push_str("#endif\n");
        s.push_str("\tvec2 tc = v_TexCoord.xy + v_Direction * g_Strength * mask;\n");
        s.push_str("\tgl_FragColor = texSample2D(g_Texture0, tc);\n");
        s.push_str("}\n");
        s
    }

    fn fs_shake_m(mask: i32) -> String {
        let mut s = prelude_mask(true, mask);
        s.push_str("uniform sampler2D g_Texture0;\n");
        s.push_str("uniform sampler2D g_Texture1;\n");
        s.push_str("uniform sampler2D g_Texture3;\n");
        s.push_str("uniform float g_Amp;\n");
        s.push_str("float greyscale(vec3 c)\n{\n\treturn dot(c, vec3(0.11, 0.59, 0.3));\n}\n");
        s.push_str("varying vec4 v_TexCoord;\n");
        s.push_str("void main() {\n");
        s.push_str("\tvec2 flow = texSample2D(g_Texture1, v_TexCoord.zw).rg;\n");
        s.push_str("\tvec2 off = (flow - vec2(0.5)) * g_Amp;\n");
        s.push_str("\tgl_FragColor = texSample2D(g_Texture0, off + v_TexCoord.xy);\n");
        s.push_str("#if MASK\n");
        s.push_str("\tfloat m = texSample2D(g_Texture3, v_TexCoord.zw).r;\n");
        s.push_str("\tgl_FragColor = mix(texSample2D(g_Texture0, v_TexCoord.xy), gl_FragColor, m);\n");
        s.push_str("#endif\n");
        s.push_str("}\n");
        s
    }

    fn fs_shake() -> String {
        fs_shake_m(1)
    }

    fn mk(fs: String, vs: String, combos: &[(&str, i32)], slots: &[u32]) -> Stage {
        Stage {
            vs,
            fs,
            combos: combos.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            slots: slots.to_vec(),
        }
    }

    #[test]
    fn two_waves_fuse_to_one_sample() {
        let st = [
            mk(fs_wave(), vs_wave(), &[("MASK", 1), ("PERSPECTIVE", 0)], &[0, 1]),
            mk(fs_wave(), vs_wave(), &[("MASK", 1), ("PERSPECTIVE", 0)], &[0, 1]),
        ];
        let f = compose(&st).expect("should fuse");
        assert_eq!(f.paths, 1);
        assert_eq!(
            f.unit_map,
            vec![(0, 0, 0), (0, 1, 1), (1, 0, 0), (1, 1, 2)]
        );
        // 单条路径 ⇒ 只有一次 base 采样
        assert_eq!(f.fs.matches("texSample2D(g_Texture0").count(), 1);
        // 嵌套复合: s1 (后画的趟) 先作用在 p 上, 再 s0 ⇒ main 必须倒序
        let p1 = f.fs.find("s1_op(c0").expect("s1 先");
        let p0 = f.fs.find("s0_op(c0").expect("s0 后");
        assert!(p1 < p0, "main 必须按 n-1..0 展开");
        // 每趟的遮罩坐标 = 当前路径坐标 u x 本趟自己的分辨率缩放
        assert!(f
            .fs
            .contains("vec4 s0_tc = vec4(u, vec2(u.x * (g_Texture1Resolution.z / g_Texture1Resolution.x)"));
        assert!(f
            .fs
            .contains("vec4 s1_tc = vec4(u, vec2(u.x * (g_Texture2Resolution.z / g_Texture2Resolution.x)"));
        // 这些 uniform 只在顶点头里声明, 片元里必须补 (否则编译不过)
        assert!(f.fs.contains("uniform vec4 g_Texture1Resolution;"));
        assert!(f.fs.contains("uniform vec4 g_Texture2Resolution;"));
        // op 签名不再有 uv0
        assert!(f.fs.contains("void s0_op(vec2 u, out vec2 oA"));
        // 非零槽重映射到全局 unit
        assert!(f.fs.contains("g_Texture1"));
        assert!(f.fs.contains("g_Texture2"));
        // 标量前缀
        assert!(f.fs.contains("s1_g_Strength"));
        // helper 只出现一次
        assert_eq!(f.fs.matches("float greyscale").count(), 1);
        // vs：varying 前缀 + 共享 vc_uv
        assert!(f.vs.contains("varying vec4 vc_uv;"));
        assert!(f.vs.contains("s1_v_Direction"));
        // 共享 varying 只写一次; 每趟写自己的局部 uv
        // (以前每趟重写 vc_uv ⇒ 只有最后一趟的分辨率缩放活下来)
        assert_eq!(f.vs.matches("vc_uv = a_TexCoord.xyxy").count(), 1);
        for i in 0..2 {
            assert_eq!(
                f.vs
                    .matches(&format!("vec4 s{i}_tcv = vec4(a_TexCoord.xy, a_TexCoord.xy);"))
                    .count(),
                1,
                "第 {i} 趟缺局部 uv"
            );
        }
        assert!(!f.vs.contains("vc_uv.z *="), "缩放不能落到共享 varying 上");
        assert!(!f.vs.contains("#if"));
    }

    #[test]
    fn shake_splits_paths() {
        let st = [
            mk(fs_wave(), vs_wave(), &[("MASK", 1)], &[0, 1]),
            mk(fs_shake(), vs_wave(), &[("MASK", 1)], &[0, 1, 3]),
        ];
        let f = compose(&st).expect("should fuse");
        assert_eq!(f.paths, 2);
        assert_eq!(f.fs.matches("texSample2D(g_Texture0").count(), 2);
        assert!(f.fs.contains("w1 = w0 * ww1"));
        assert!(f.fs.contains("gl_FragColor = w0 * texSample2D(g_Texture0, c0)"));
    }

    #[test]
    fn mask_zero_no_split() {
        let st = [
            mk(fs_wave(), vs_wave(), &[("MASK", 1)], &[0, 1]),
            mk(fs_shake_m(0), vs_wave_m(0), &[("MASK", 0)], &[0, 1, 3]),
        ];
        let f = compose(&st).expect("should fuse");
        assert_eq!(f.paths, 1);
    }

    /// 头里的 `#define` 才是已编译源码的真值: combos 里的 1 不该把 MASK 打开.
    #[test]
    fn header_defaults_beat_combos() {
        let st = [
            mk(fs_wave(), vs_wave(), &[("MASK", 1)], &[0, 1]),
            // 源码头里 MASK=0, combos 却写 1 ⇒ 必须按头走 (不分裂路径)
            mk(fs_shake_m(0), vs_wave_m(0), &[("MASK", 1)], &[0, 1, 3]),
        ];
        let f = compose(&st).expect("should fuse");
        assert_eq!(f.paths, 1);
        assert_eq!(f.fs.matches("texSample2D(g_Texture0").count(), 1);
        assert!(!f.fs.contains("s1_op(c0, a1"), "不该有分裂路径");
    }

    /// vs 里的 uv 写法不认识时必须整体 Reject (并说清第几趟/哪种写法).
    #[test]
    fn unsupported_vs_uv_pattern_rejected() {
        let mut vs = prelude(false);
        vs.push_str("uniform mat4 g_ModelViewProjectionMatrix;\n");
        vs.push_str("attribute vec3 a_Position;\n");
        vs.push_str("attribute vec2 a_TexCoord;\n");
        vs.push_str("varying vec4 v_TexCoord;\n");
        vs.push_str("void main() {\n");
        vs.push_str("\tgl_Position = mul(vec4(a_Position, 1.0), g_ModelViewProjectionMatrix);\n");
        vs.push_str("\tv_TexCoord = a_TexCoord.xyxy;\n");
        vs.push_str("\tv_TexCoord.z = 0.5;\n");
        vs.push_str("}\n");
        let st = [
            mk(fs_wave(), vs.clone(), &[("MASK", 1)], &[0, 1]),
            mk(fs_wave(), vs, &[("MASK", 1)], &[0, 1]),
        ];
        let e = compose(&st).err().expect("must be rejected");
        assert!(e.contains("第 0 趟"), "{e}");
        assert!(e.contains("不支持"), "{e}");
    }

    #[test]
    fn non_fusible_colour_op() {
        let mut s = prelude(true);
        s.push_str("uniform sampler2D g_Texture0;\n");
        s.push_str("uniform vec4 g_Color4;\n");
        s.push_str("varying vec4 v_TexCoord;\n");
        s.push_str("void main() {\n");
        s.push_str("\tgl_FragColor = g_Color4 * texSample2D(g_Texture0, v_TexCoord.xy);\n");
        s.push_str("}\n");
        // vs_wave 读 g_Texture1Resolution ⇒ 槽必须含 1 (否则那是"趟自己引用了未绑定的槽")
        let st = [mk(fs_wave(), vs_wave(), &[("MASK", 1)], &[0, 1]), mk(s, vs_wave(), &[], &[0, 1])];
        let e = compose(&st).err().expect("must be rejected");
        assert!(e.contains("texSample2D(g_Texture0"), "{e}");
    }

    #[test]
    fn texture0_used_early_rejected() {
        let mut s = prelude(true);
        s.push_str("uniform sampler2D g_Texture0;\n");
        s.push_str("varying vec4 v_TexCoord;\n");
        s.push_str("void main() {\n");
        s.push_str("\tvec2 tc = v_TexCoord.xy + texSample2D(g_Texture0, v_TexCoord.xy).rg * 0.1;\n");
        s.push_str("\tgl_FragColor = texSample2D(g_Texture0, tc);\n");
        s.push_str("}\n");
        // vs_wave 读 g_Texture1Resolution ⇒ 槽必须含 1 (否则那是"趟自己引用了未绑定的槽")
        let st = [mk(fs_wave(), vs_wave(), &[("MASK", 1)], &[0, 1]), mk(s, vs_wave(), &[], &[0, 1])];
        let e = compose(&st).err().expect("must be rejected");
        assert!(e.contains("g_Texture0"), "{e}");
    }

    #[test]
    fn perspective_rejected() {
        let st = [
            mk(fs_wave(), vs_wave(), &[("MASK", 1)], &[0, 1]),
            mk(fs_wave(), vs_wave(), &[("PERSPECTIVE", 1)], &[0, 1]),
        ];
        let e = compose(&st).err().expect("must be rejected");
        assert!(e.contains("PERSPECTIVE"), "{e}");
    }

    #[test]
    fn path_cap() {
        let st = [
            mk(fs_shake(), vs_wave(), &[("MASK", 1)], &[0, 1, 3]),
            mk(fs_shake(), vs_wave(), &[("MASK", 1)], &[0, 1, 3]),
            mk(fs_shake(), vs_wave(), &[("MASK", 1)], &[0, 1, 3]),
            mk(fs_shake(), vs_wave(), &[("MASK", 1)], &[0, 1, 3]),
        ];
        let e = compose(&st).err().expect("must be rejected");
        assert!(e.contains("路径数"), "{e}");
    }

    /// Regression on a real 11-effect chain (waterwaves x4, shake, waterwaves x3, shake).
    /// Needs a directory of dumped shader sources in `WE_FUSE_DUMP_DIR`; skipped when unset.
    #[test]
    fn real_1016_chain() {
        let Ok(dir) = std::env::var("WE_FUSE_DUMP_DIR") else {
            return;
        };
        let dir = dir.as_str();
        if !std::path::Path::new(dir).is_dir() {
            return;
        }
        let names = [
            (3, "effects_waterwaves"),
            (4, "effects_waterwaves"),
            (5, "effects_waterwaves"),
            (6, "effects_waterwaves"),
            (7, "effects_shake"),
            (8, "effects_waterwaves"),
            (9, "effects_waterwaves"),
            (10, "effects_waterwaves"),
            (11, "effects_shake"),
        ];
        let mut st: Vec<Stage> = Vec::new();
        for (i, nm) in names {
            let fs = std::fs::read_to_string(format!("{dir}/fs.1016.e{i}p0.{nm}.frag"))
                .expect("dump frag");
            let vs = std::fs::read_to_string(format!("{dir}/fs.1016.e{i}p0.{nm}.vert"))
                .expect("dump vert");
            st.push(Stage {
                vs,
                fs,
                combos: vec![
                    ("MASK".into(), 1),
                    ("TIMEOFFSET".into(), 1),
                    ("DUALWAVES".into(), 0),
                    ("PERSPECTIVE".into(), 0),
                    ("NOISE".into(), 0),
                    ("DIRECTION".into(), 0),
                    ("AUDIOPROCESSING".into(), 0),
                    ("HLSL".into(), 0),
                ],
                slots: vec![0, 1, 2, 3],
            });
        }
        let f = compose(&st).expect("real chain should fuse");
        // 真实 dump 里 shake 趟的头是 `#define MASK 0` (没有遮罩分支) ⇒ 不分裂, 单路径.
        // 以前 combos 里的 MASK=1 会把它打开 ⇒ 4 条路径, 那本身就是错的.
        assert_eq!(f.paths, 1, "shake 的 MASK 在源码里是 0 ⇒ 全程确定性");
        assert_eq!(f.fs.matches("texSample2D(g_Texture0").count(), 1);
        // 9 趟: 各自局部 uv + 各自的 op, 倒序展开
        for i in 0..9 {
            assert!(
                f.vs.contains(&format!(
                    "vec4 s{i}_tcv = vec4(a_TexCoord.xy, a_TexCoord.xy);"
                )),
                "第 {i} 趟缺局部 uv"
            );
            assert!(f.fs.contains(&format!("void s{i}_op(vec2 u, out vec2 oA")));
        }
        let p8 = f.fs.find("s8_op(c0").expect("s8 先");
        let p0 = f.fs.find("s0_op(c0").expect("s0 后");
        assert!(p8 < p0, "main 必须倒序");
        // 每趟的遮罩缩放取自**该趟自己的**槽 1 分辨率 uniform
        // (slots=[0,1,2,3] ⇒ 第 i 趟的槽 1 -> 全局 unit 1+3i, 逐趟不同, 正是缺陷 1 的要害)
        for i in 0..9 {
            let u1 = 1 + 3 * i;
            assert!(
                f.fs.contains(&format!(
                    "s{i}_tc = vec4(u, vec2(u.x * (g_Texture{u1}Resolution.z"
                )),
                "第 {i} 趟的遮罩缩放没用 unit {u1} 的分辨率"
            );
            assert!(f.fs.contains(&format!("uniform vec4 g_Texture{u1}Resolution;")));
        }
    }

    #[test]
    fn three_splitters_is_allowed() {
        let st = [
            mk(fs_shake(), vs_wave(), &[("MASK", 1)], &[0, 1, 3]),
            mk(fs_shake(), vs_wave(), &[("MASK", 1)], &[0, 1, 3]),
            mk(fs_shake(), vs_wave(), &[("MASK", 1)], &[0, 1, 3]),
        ];
        let f = compose(&st).expect("8 paths is the cap");
        assert_eq!(f.paths, 8);
    }
}

#[cfg(test)]
mod shape_tail_tests {
    use super::*;

    fn prelude() -> String {
        let mut s = String::new();
        s.push_str("#version 120\n");
        s.push_str("#define mul(a, b) ((b) * (a))\n");
        s.push_str("#ifndef PERSPECTIVE\n#define PERSPECTIVE 0\n#endif\n");
        s.push_str("#ifndef MASK\n#define MASK 1\n#endif\n");
        s.push_str("vec4 texSample2D(sampler2D t, vec2 uv)\n{\n\treturn texture2D(t, uv);\n}\n");
        s
    }

    fn vs_wave() -> String {
        let mut s = prelude();
        s.push_str("uniform mat4 g_ModelViewProjectionMatrix;\n");
        s.push_str("uniform vec4 g_Texture1Resolution;\n");
        s.push_str("attribute vec3 a_Position;\n");
        s.push_str("attribute vec2 a_TexCoord;\n");
        s.push_str("varying vec4 v_TexCoord;\n");
        s.push_str("void main() {\n");
        s.push_str("\tgl_Position = mul(vec4(a_Position, 1.0), g_ModelViewProjectionMatrix);\n");
        s.push_str("\tv_TexCoord = a_TexCoord.xyxy;\n");
        s.push_str("#if MASK\n");
        s.push_str("\tv_TexCoord.z *= g_Texture1Resolution.z / g_Texture1Resolution.x;\n");
        s.push_str("#endif\n");
        s.push_str("}\n");
        s
    }

    fn fs_wave(shape: bool) -> String {
        let mut s = prelude();
        s.push_str("uniform sampler2D g_Texture0; // {\"hidden\":true}\n");
        s.push_str("uniform sampler2D g_Texture1;\n");
        s.push_str("uniform float g_Speed;\n");
        s.push_str("varying vec4 v_TexCoord;\n");
        s.push_str("void main() {\n");
        s.push_str("\tfloat mask = texSample2D(g_Texture1, v_TexCoord.zw).r;\n");
        s.push_str("\tvec2 texCoord = v_TexCoord.xy;\n");
        s.push_str("\tfloat distance = g_Speed;\n");
        s.push_str("\ttexCoord += vec2(sin(distance), 0.0) * 0.01 * mask;\n");
        if shape {
            s.push_str("\tvec4 weDisp = texSample2D(g_Texture0, texCoord);\n");
            s.push_str("\tvec4 weRest = texSample2D(g_Texture0, v_TexCoord.xy);\n");
            s.push_str("\tvec3 weRgb = mix(weRest.rgb, weDisp.rgb, weDisp.a);\n");
            s.push_str("\tgl_FragColor = vec4(min(weRgb, vec3(weRest.a)), weRest.a);\n");
        } else {
            s.push_str("\tgl_FragColor = texSample2D(g_Texture0, texCoord);\n");
        }
        s.push_str("}\n");
        s
    }

    fn mk(fs: String) -> Stage {
        Stage {
            vs: vs_wave(),
            fs,
            combos: vec![("MASK".to_string(), 1), ("PERSPECTIVE".to_string(), 0)],
            slots: vec![0, 1],
        }
    }

    #[test]
    fn shape_chain_fuses_into_one_pass_with_pinned_alpha() {
        let st = [mk(fs_wave(true)), mk(fs_wave(true))];
        let f = compose(&st).expect("shape chain should fuse");
        assert_eq!(f.paths, 1);
        assert!(f
            .fs
            .contains("vec4 weRest = texSample2D(g_Texture0, vc_uv.xy);"));
        assert!(f.fs.contains(
            "gl_FragColor = vec4(min(mix(weRest.rgb, weDisp.rgb, weDisp.a), vec3(weRest.a)), weRest.a);"
        ));
        assert_eq!(
            f.fs.matches("texSample2D(g_Texture0").count(),
            2,
            "rest + disp only"
        );
    }

    #[test]
    fn mixed_shape_and_plain_is_rejected() {
        let st = [mk(fs_wave(true)), mk(fs_wave(false))];
        let e = compose(&st).err().expect("must reject");
        assert!(e.contains("shape"), "{e}");
    }
}
