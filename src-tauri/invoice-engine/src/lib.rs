//! OFD (Open Fixed-layout Document) parser and SVG renderer.
//!
//! Supports Chinese electronic invoices (发票): extracts structured invoice data
//! from OFD XML metadata (CustomData + CustomTag) and renders pages as SVG.
//!
//! Also supports standalone XML 数电票 (fully digitalized e-invoice) parsing:
//! extracts structured invoice fields from `<EInvoice>` XML files.
//!
//! The OFD format is a ZIP archive containing XML page descriptions and image resources,
//! defined by Chinese national standard GB/T 33190-2016.

use image::GenericImageView;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// =====================================================
// Public Types
// =====================================================

/// Invoice data extracted from OFD XML
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OfdInvoiceInfo {
    pub invoice_no: Option<String>,
    pub invoice_date: Option<String>,
    pub buyer_name: Option<String>,
    pub buyer_tax_id: Option<String>,
    pub seller_name: Option<String>,
    pub seller_tax_id: Option<String>,
    pub amount_no_tax: Option<f64>,
    pub tax_amount: Option<f64>,
    pub amount_tax: Option<f64>,
    pub invoice_type: Option<String>,
    /// 通行费电子发票标记（销售方保留真实路桥公司名）
    pub is_toll: Option<bool>,
}

/// Result returned by `parse_ofd_file`: SVG rendering + structured invoice data.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OfdResult {
    pub svg: String,
    pub invoice_info: OfdInvoiceInfo,
    pub page_width: f64,
    pub page_height: f64,
}

/// An image extracted from an OFD file (for bitmap fallback path).
#[derive(Debug, Clone)]
pub struct OfdExtractedImage {
    pub data_url: String,
    pub ext: String,
    pub width: u32,
    pub height: u32,
}

/// Invoice data extracted from standalone XML 数电票 file.
/// XML 数电票 is a structured data format (no layout info), used for archiving and data exchange.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct XmlInvoiceInfo {
    pub invoice_no: Option<String>,
    pub invoice_date: Option<String>,
    pub seller_name: Option<String>,
    pub seller_tax_id: Option<String>,
    pub buyer_name: Option<String>,
    pub buyer_tax_id: Option<String>,
    pub amount_no_tax: Option<f64>,
    pub tax_amount: Option<f64>,
    pub amount_tax: Option<f64>,
    /// Invoice type label (e.g. "增值税专用发票", "电子发票(普通发票)")
    pub invoice_type: Option<String>,
    /// 通行费电子发票标记（ItemName 含「通行费」）
    pub is_toll: Option<bool>,
}

// =====================================================
// Internal OFD Structures
// =====================================================

#[derive(Debug, Default)]
struct OfdFont {
    id: u32,
    font_name: String,
    family_name: String,
}

/// DrawParam — inherited styling for paths/text (from PublicRes.xml)
#[derive(Debug, Default, Clone)]
struct OfdDrawParam {
    id: u32,
    relative: Option<u32>,
    line_width: f64,
    stroke_color: Option<(u8, u8, u8)>,
    fill_color: Option<(u8, u8, u8)>,
}

#[derive(Debug, Default)]
#[allow(dead_code)]
struct OfdImage {
    id: u32,
    file_name: String,
    base64: String,
}

/// 单个 TextCode 的定位与文本（GB/T 33190 §11.3：一个 TextObject 可含 1..N 个 TextCode，
/// 各自带 X/Y/DeltaX，标准示例就是两个，比如同一行里「Font」+「字体」）。
/// 渲染必须按段独立输出——曾把多段文本拼成一行、只用末段的 X/ΔX（ΔX 用尽后还重复末值），
/// 整行被均匀拉开（issue #47 截图里数字被隔开的形态之一）。
#[derive(Debug, Clone, Default)]
struct OfdTextSegment {
    text_x: f64,
    text_y: f64,
    delta_x: Vec<f64>,
    delta_y: Vec<f64>,
    text: String,
}

#[derive(Debug)]
struct OfdTextObject {
    id: u32,
    boundary: (f64, f64, f64, f64), // x, y, w, h
    font_id: u32,
    size: f64,
    ctm: Option<(f64, f64, f64, f64, f64, f64)>,
    /// 对象级文本 = 各段文本按序拼接（CustomTag 按 ID 取值、文本提取沿用该口径）
    text: String,
    /// 各 TextCode 的逐段定位参数；渲染路径用 segments，非空时不看下面这组镜像字段
    segments: Vec<OfdTextSegment>,
    delta_x: Vec<f64>,
    delta_y: Vec<f64>, // per-character Y offsets for multi-line text (DeltaY attribute)
    text_x: f64,
    text_y: f64,
    fill_color: Option<(u8, u8, u8)>,
    stroke_color: Option<(u8, u8, u8)>,
    alpha: Option<u8>,
    blend_mode: Option<String>,
    weight: u32, // OFD font weight: 400=normal, 700=bold
    layer_draw_param: Option<u32>, // DrawParam ID from the Layer this object belongs to
}

impl Default for OfdTextObject {
    fn default() -> Self {
        Self {
            id: 0,
            boundary: (0.0, 0.0, 0.0, 0.0),
            font_id: 0,
            size: 3.175,
            ctm: None,
            text: String::new(),
            segments: Vec::new(),
            delta_x: Vec::new(),
            delta_y: Vec::new(),
            text_x: 0.0,
            text_y: 0.0,
            fill_color: None,
            stroke_color: None,
            alpha: None,
            blend_mode: None,
            weight: 400, // Normal weight by default
            layer_draw_param: None,
        }
    }
}

#[derive(Debug, Default)]
struct OfdPathObject {
    id: u32,
    boundary: (f64, f64, f64, f64),
    line_width: f64,
    ctm: Option<(f64, f64, f64, f64, f64, f64)>,
    stroke_color: Option<(u8, u8, u8)>,
    fill_color: Option<(u8, u8, u8)>,
    fill: bool,
    abbreviated_data: String,
    alpha: Option<u8>,
    layer_draw_param: Option<u32>, // DrawParam ID from the Layer this object belongs to
}

#[derive(Debug, Default)]
struct OfdImageObject {
    id: u32,
    boundary: (f64, f64, f64, f64),
    resource_id: u32,
    ctm: Option<(f64, f64, f64, f64, f64, f64)>,
    blend_mode: Option<String>,
    alpha: Option<u8>,
    image_mask: Option<u32>, // ResourceID of mask image (OFD ImageMask attribute)
}

// =====================================================
// ZIP Helpers
// =====================================================

/// Read a file from ZIP archive as string
fn zip_read_str(archive: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Option<String> {
    use std::io::Read;
    let mut entry = archive.by_name(name).ok()?;
    let mut buf = String::new();
    entry.read_to_string(&mut buf).ok()?;
    Some(buf)
}

/// Read a file from ZIP archive as bytes
fn zip_read_bytes(archive: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut entry = archive.by_name(name).ok()?;
    let mut buf = Vec::new();
    entry.read_to_end(&mut buf).ok()?;
    Some(buf)
}

// =====================================================
// Parsing Helpers
// =====================================================

/// Parse 2 floats from "x y" string
#[allow(dead_code)]
fn parse_f2(s: &str) -> Option<(f64, f64)> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() >= 2 {
        Some((parts[0].parse().ok()?, parts[1].parse().ok()?))
    } else {
        None
    }
}

fn parse_f4(s: &str) -> Option<(f64, f64, f64, f64)> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() >= 4 {
        Some((
            parts[0].parse().ok()?,
            parts[1].parse().ok()?,
            parts[2].parse().ok()?,
            parts[3].parse().ok()?,
        ))
    } else {
        None
    }
}

fn parse_f6(s: &str) -> Option<(f64, f64, f64, f64, f64, f64)> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() >= 6 {
        Some((
            parts[0].parse().ok()?,
            parts[1].parse().ok()?,
            parts[2].parse().ok()?,
            parts[3].parse().ok()?,
            parts[4].parse().ok()?,
            parts[5].parse().ok()?,
        ))
    } else {
        None
    }
}

/// Parse OFD color value "R G B" → (r, g, b)
fn parse_color(s: &str) -> Option<(u8, u8, u8)> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() >= 3 {
        Some((
            parts[0].parse().ok()?,
            parts[1].parse().ok()?,
            parts[2].parse().ok()?,
        ))
    } else {
        None
    }
}

/// Get attribute value by local name (ignoring namespace prefix)
fn attr_val(e: &quick_xml::events::BytesStart, local_name: &str) -> Option<String> {
    for a in e.attributes().flatten() {
        let key = a.key;
        let local = if let Some(pos) = key.0.iter().position(|&b| b == b':') {
            &key.as_ref()[pos + 1..]
        } else {
            key.as_ref()
        };
        if local == local_name.as_bytes() {
            return std::str::from_utf8(&a.value).ok().map(|s| s.to_string());
        }
    }
    None
}

/// Get element text content from a quick-xml reader (reads until End tag)
fn read_element_text(reader: &mut quick_xml::Reader<&[u8]>) -> String {
    use quick_xml::events::Event;
    let mut text = String::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Text(t)) => {
                if let Ok(s) = t.unescape() {
                    text.push_str(&s);
                }
            }
            Ok(Event::End(_)) | Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }
    text
}

/// Parse DeltaX/DeltaY attribute string into individual character offsets.
/// Format:
///   - "3.175 3.175 3.175" — simple space-separated values
///   - "g 19 1.5875" — group: repeat next spacing 19 times at 1.5875
///   - "g 4 1.5875 3.175 g 2 1.5875 3.175" — mixed
fn parse_delta_values(s: &str) -> Vec<f64> {
    let mut result = Vec::new();
    let tokens: Vec<&str> = s.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i] == "g" && i + 2 < tokens.len() {
            // Group format: g count value [extra_value...]
            if let (Ok(count), Ok(val)) = (tokens[i + 1].parse::<usize>(), tokens[i + 2].parse::<f64>()) {
                for _ in 0..count {
                    result.push(val);
                }
                i += 3;
                // Check if there's an extra value after the group
                if i < tokens.len() && tokens[i] != "g" {
                    if let Ok(v) = tokens[i].parse::<f64>() {
                        result.push(v);
                        i += 1;
                    }
                }
            } else {
                i += 1;
            }
        } else if let Ok(v) = tokens[i].parse::<f64>() {
            result.push(v);
            i += 1;
        } else {
            i += 1;
        }
    }
    result
}

/// 解码 OFD TextCode 的 "\XXXX" 转义（GB/T 33190 规定：编码超出 XML 字符范围时
/// 用反斜杠加四位十六进制表示，空格亦需转义；TextCode 作为占位符时用 ¤ U+00A4）。
/// 不解码时 "\0020" 会被当成 5 个可见字符渲染，字距与正文全乱。
fn decode_ofd_escapes(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let cs: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < cs.len() {
        if cs[i] == '\\' && i + 5 <= cs.len() {
            let hex: String = cs[i + 1..i + 5].iter().collect();
            if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                if let Some(c) = char::from_u32(cp) {
                    out.push(c);
                    i += 5;
                    continue;
                }
            }
        }
        out.push(cs[i]);
        i += 1;
    }
    out
}

// =====================================================
// SVG Generation Helpers
// =====================================================

/// Normalize a font name that may contain a subset prefix.
/// Subset font names follow the pattern: `PREFIX+BaseFontName-PREFIX+BaseFontName-Suffix`
/// (e.g., `AEWMEC+KaiTi-AEWMEC+KaiTi-0` → base name `KaiTi`)
/// Also handles PostScript font names like `CourierNewPSMT` → `Courier New`.
fn normalize_font_name(raw: &str) -> String {
    // Step 1: Extract base font name from subset prefix pattern
    // Subset prefix format: UPPERCASE_LETTERS+BaseName
    let base = if let Some(plus_pos) = raw.find('+') {
        // Found subset prefix — extract text after '+' up to next '-' or end
        let after_plus = &raw[plus_pos + 1..];
        let end = after_plus.find('-').unwrap_or(after_plus.len());
        &after_plus[..end]
    } else {
        raw
    };

    // Step 2: Strip common encoding/localized suffixes (e.g., "仿宋_GB2312" → "仿宋")
    // 部分税务软件生成的 OFD 使用带后缀的字体名，不剥离会导致后续精确匹配失败。
    let base = base
        .trim_end_matches("_GB2312")
        .trim_end_matches("_GBK")
        .trim_end_matches("_GB18030")
        .trim_end_matches("-GB2312")
        .trim_end_matches("-ET")
        .trim_end_matches("-0")
        .trim_end_matches("-1")
        .trim_end_matches("-2");

    // Step 3: Map PostScript font names to standard CSS font-family names
    match base {
        "CourierNewPSMT" => "Courier New",
        "TimesNewRomanPSMT" => "Times New Roman",
        "ArialMT" => "Arial",
        "Arial-BoldMT" => "Arial",
        "SimSun" | "STSong" => "宋体",
        "KaiTi" | "STKaiti" => "楷体",
        "SimHei" | "STHeiti" => "黑体",
        "FangSong" | "STFangsong" => "仿宋",
        other => other,
    }.to_string()
}

/// GB/T 33190 表45：TextCode 作为占位符使用时，一律采用 ¤（U+00A4）占位
const OFD_PLACEHOLDER: char = '\u{A4}';

/// 占位符判定：占一个字符槽位参与 ΔX 定位，但**不渲染字形**——直接输出会与相邻字符叠字
/// （issue #44 的「月¤」叠字）。
/// - `¤`（U+00A4）：国标占位符
/// - PUA 码位（U+E000–U+F8FF 等）：字形索引类编码，用系统字体渲染必出乱码
/// 注意：**不要**把 Ø/∅/Φ 之类圆圈符号也拉进黑名单——issue #47 那个「月Ø」已查明是
/// 「数字 0 被 ΔX 错位挤到「月」的竖笔上」，与占位符无关；乱扩名单会误伤正文。
fn is_placeholder(ch: char) -> bool {
    ch == OFD_PLACEHOLDER
        || ('\u{E000}'..='\u{F8FF}').contains(&ch)
        || ('\u{F0000}'..='\u{FFFFD}').contains(&ch)
        || ('\u{100000}'..='\u{10FFFD}').contains(&ch)
}

/// 输出单个字符的 SVG 文本内容；占位符输出空串（不渲染字形）
fn glyph_out(ch: char) -> String {
    if is_placeholder(ch) {
        String::new()
    } else {
        esc_xml(&ch.to_string())
    }
}

/// Boundary 宽自校验误差：按间距口径消费前 min(序列长度-1, ΔX 个数) 个 ΔX，
/// 累加和与 Boundary 宽的差（残差通常为一个末字宽，越小说明口径越对）。
/// 序列不足 2 字符时返回 INFINITY（该口径无法用 Boundary 校验）。
fn delta_sum_err(seq_len: usize, dx: &[f64], boundary_w: f64) -> f64 {
    if seq_len < 2 {
        return f64::INFINITY;
    }
    let n = (seq_len - 1).min(dx.len());
    let sum: f64 = dx[..n].iter().sum();
    (sum - boundary_w).abs()
}

/// 逐字 ΔX 取值（表46）：DeltaX 不出现时字型在 X 方向**不做偏移**，不能拿字号顶替
/// ——否则纯 ΔY 定位的文本（竖排/垂直排列）会被斜着排（每字右移一个字号）。
/// ΔX 数组用尽（畸形文件）沿用末值兜底。
fn char_dx(delta_x: &[f64], i: usize) -> f64 {
    if delta_x.is_empty() {
        0.0
    } else if i < delta_x.len() {
        delta_x[i]
    } else {
        *delta_x.last().unwrap()
    }
}

/// Build SVG text element(s) from an OFD TextObject.
/// 一个 TextObject 可含多个 TextCode（GB/T 33190 §11.3），每段带独立的 X/Y/ΔX，必须逐段
/// 输出各自的 <text>；无段对象（测试构造 / 异常文件）退回对象级字段那条路径。
fn build_svg_text(
    text_obj: &OfdTextObject,
    font_map: &HashMap<u32, OfdFont>,
    color_spaces: &HashMap<u32, String>,
    scale_x: f64,
    scale_y: f64,
) -> String {
    if text_obj.segments.is_empty() {
        return build_svg_text_line(
            text_obj, &text_obj.text, text_obj.text_x, text_obj.text_y,
            &text_obj.delta_x, &text_obj.delta_y, font_map, color_spaces, scale_x, scale_y,
        );
    }
    let mut svg = String::new();
    for seg in &text_obj.segments {
        svg.push_str(&build_svg_text_line(
            text_obj, &seg.text, seg.text_x, seg.text_y,
            &seg.delta_x, &seg.delta_y, font_map, color_spaces, scale_x, scale_y,
        ));
    }
    svg
}

/// 渲染单个 TextCode 段（一段 = 一个 <text>）：文本/坐标/ΔX 来自段，
/// 字体、字号、Boundary、CTM、颜色、粗细等来自所属 TextObject。
#[allow(clippy::too_many_arguments)]
fn build_svg_text_line(
    text_obj: &OfdTextObject,
    text: &str,
    text_x: f64,
    text_y: f64,
    delta_x: &[f64],
    delta_y: &[f64],
    font_map: &HashMap<u32, OfdFont>,
    _color_spaces: &HashMap<u32, String>,
    scale_x: f64,
    scale_y: f64,
) -> String {
    if text.is_empty() {
        return String::new();
    }

    let font = font_map.get(&text_obj.font_id);
    let font_family_raw = font.map(|f| {
        if !f.family_name.is_empty() { f.family_name.clone() } else { f.font_name.clone() }
    }).unwrap_or_else(|| "SimSun".to_string());

    // Normalize subset font names (e.g., "AEWMEC+KaiTi-AEWMEC+KaiTi-0" → "KaiTi")
    let font_base = normalize_font_name(&font_family_raw);

    // Font fallback: add generic CJK/serif/sans-serif fallbacks for cross-platform rendering.
    // SVG font-family is CSS: names with spaces need single quotes (attr value is in double quotes).
    // 用包含匹配替代精确匹配，兼容 仿宋_GB2312/楷体_GBK 等变体；未知字一律用 monospace 兜底，
    // 避免 SVG 输出裸字体名导致 Canvas 回退到不可控的比例字体（数字/汉字挤字）。
    let font_family = match font_base.as_str() {
        _ if font_base.contains("楷") || font_base.contains("Kai") => "楷体, KaiTi, STKaiti, serif".to_string(),
        _ if font_base.contains("黑") || font_base.contains("Hei") => "黑体, SimHei, STHeiti, sans-serif".to_string(),
        _ if font_base.contains("仿宋") || font_base.contains("Fang") => "仿宋, FangSong, STFangsong, serif".to_string(),
        _ if font_base.contains("宋") || font_base.contains("Sun") || font_base.contains("Song") => "宋体, SimSun, STSong, serif".to_string(),
        _ if font_base.contains("Courier") => "'Courier New', Courier, monospace".to_string(),
        _ if font_base.contains("Times") => "'Times New Roman', Times, serif".to_string(),
        other => format!("'{}', monospace", other),
    };

    let font_size = text_obj.size;
    // Use OFD Weight attribute for bold detection (>= 700 = bold)
    let bold = if text_obj.weight >= 700 {
        " font-weight=\"bold\""
    } else {
        ""
    };

    // Build text content using absolute x positions (tspan x).
    // OFD DeltaX = absolute advance from char origin to next char origin (includes char width).
    // SVG tspan dx = ADDITIONAL offset on top of natural char advance — would double the spacing.
    // Solution: use tspan x with absolute positions in the text element's coordinate system.
    // base_x = the x position of the first character (set on <text> element).
    // Subsequent chars: tspan x = base_x + accumulated DeltaX.
    let chars: Vec<char> = text.chars().collect();
    // DeltaX or DeltaY alone is enough to require per-char positioning; a DeltaY-only
    // object (multi-line without horizontal increments) must not fall back to plain text.
    let has_delta = (!delta_x.is_empty() || !delta_y.is_empty()) && chars.len() > 1;
    // 逐字定位的字符序列要与 DeltaX 对齐，各生成器口径不一：
    // ① 空格参与定位（规范口径：表45「文字内容中出现的空格也需要转义」，空格是内容的一部分；
    //    移动话费票样 '单··位' 4 字符配 3 个 ΔX 即此口径）；
    // ② 空格是列分隔符（数电票表头把多列标题拼进一条 TextCode，如
    //    "车牌号车辆类型 通行日期起…"，ΔX 按去空格后的序列对齐，属厂商偏差）。
    // 主判据：Boundary 宽自校验——ΔX 累加和应≈文字总宽（残差为一个末字宽），两口径误差
    // 差 2 倍以上才切换。表46：X/Y 是「对象坐标系」下的坐标，ΔX/ΔY 与 Boundary 同处
    // 该坐标系 → **CTM 含缩放不构成跳过本校验的理由**（issue #47 的不动产证就栽在这里：
    // 恰好一个空格时两种口径的 ΔX 条数都能对上，长度拟合无区分力，只能靠本校验）。
    // 两口径误差接近（数组与 Boundary 自身不自洽，如数电票表头样本）时判定不生效，
    // 退回长度拟合（保持旧行为）。
    let vis: Vec<char> = chars.iter().copied().filter(|c| !c.is_whitespace()).collect();
    let dx_len = delta_x.len();
    let fits_vis = dx_len + 1 == vis.len() || dx_len == vis.len();
    let fits_all = dx_len + 1 == chars.len() || dx_len == chars.len();
    let boundary_w = text_obj.boundary.2;
    // 多行逐字定位（issue #47 附记栏）：ΔX 中出现**负值＝换行回退**，说明 ΔX 是按含空格的
    // 完整字符流逐字生成的（回退量等于该行已推进的宽度，只有完整流才对得上）。此类文本
    // ΔX 累加和天然 ≠ Boundary 宽（每行宽 ≤ boundary、行间还有负回退），Boundary 自校验失效；
    // 而 ΔX 条数又常同时吻合两种口径（该文件恰好 1 个空格：131 字符配 130 ΔX 满足含空格口径，
    // 去空格后 130 字符也"凑巧"吻合）→ 长度拟合会误选去空格口径，换行点后移一个字符
    // （「2」被留在上一行行尾）。故含换行回退时直接按完整字符流渲染。
    let multiline = delta_x.iter().any(|&v| v < 0.0);
    let seq: Vec<char> = if multiline && fits_all {
        chars.clone()
    } else {
        let mut chosen: Option<bool> = None; // true=口径①含空格, false=口径②去空格
        if boundary_w > 0.0 && !delta_x.is_empty() && chars.len() != vis.len() {
            let err_all = delta_sum_err(chars.len(), delta_x, boundary_w);
            let err_vis = delta_sum_err(vis.len(), delta_x, boundary_w);
            // 需一方误差有限且小于对方 70% 才切换，避免 Boundary 不精确时误判
            if err_all.is_finite() && err_all * 10.0 < err_vis * 7.0 {
                chosen = Some(true);
            } else if err_vis.is_finite() && err_vis * 10.0 < err_all * 7.0 {
                chosen = Some(false);
            }
        }
        match chosen {
            Some(true) => chars.clone(),
            Some(false) => vis,
            None => if !fits_vis && fits_all { chars.clone() } else { vis },
        }
    };
    // We'll build the tspans later, after we know the base_x coordinate.
    // For now, just store the char data.

    // CTM transform: translate to boundary origin, apply matrix, then text at local coords
    if let Some(ctm) = text_obj.ctm {
        // CTM text: x is in local coords (text_x * scale)
        let base_x = text_x * scale_x;
        let base_y = text_y * scale_y;
        let content = if has_delta && seq.len() > 1 {
            let mut s = format!("<tspan x=\"{:.4}\" y=\"{:.4}\">{}</tspan>", base_x, base_y, glyph_out(seq[0]));
            let mut x_pos = base_x;
            let mut y_pos = base_y;
            for (i, ch) in seq.iter().enumerate().skip(1) {
                let dx = char_dx(delta_x, i - 1);
                x_pos += dx * scale_x;
                let dy = if i - 1 < delta_y.len() {
                    delta_y[i - 1]
                } else {
                    0.0
                };
                y_pos += dy * scale_y;
                s.push_str(&format!("<tspan x=\"{:.4}\" y=\"{:.4}\">{}</tspan>", x_pos, y_pos, glyph_out(*ch)));
            }
            s
        } else {
            // 无逐字定位：整段输出，占位符同样不渲染
            esc_xml(&text.chars().filter(|c| !is_placeholder(*c)).collect::<String>())
        };
        return format!(
            "<text transform=\"translate({bx},{by}) matrix({a},{b},{c},{d},{e},{f})\" x=\"{tx}\" y=\"{ty}\" font-family=\"{ff}\" font-size=\"{fs}\"{fc}{bw}>{ct}</text>",
            bx = text_obj.boundary.0 * scale_x,
            by = text_obj.boundary.1 * scale_y,
            a = ctm.0, b = ctm.1, c = ctm.2, d = ctm.3,
            e = ctm.4 * scale_x, f = ctm.5 * scale_y,
            tx = base_x,
            ty = base_y,
            ff = esc_xml_attr(&font_family),
            fs = font_size * scale_x,
            fc = fill_attr(text_obj.fill_color, text_obj.alpha),
            bw = bold,
            ct = content
        );
    }

    // Normal: position = Boundary + TextCode offset (absolute SVG coords)
    let base_x = (text_obj.boundary.0 + text_x) * scale_x;
    let base_y = (text_obj.boundary.1 + text_y) * scale_y;
    let content = if has_delta && seq.len() > 1 {
        let mut s = format!("<tspan x=\"{:.4}\" y=\"{:.4}\">{}</tspan>", base_x, base_y, glyph_out(seq[0]));
        let mut x_pos = base_x;
        let mut y_pos = base_y;
        for (i, ch) in seq.iter().enumerate().skip(1) {
            let dx = char_dx(delta_x, i - 1);
            x_pos += dx * scale_x;
            let dy = if i - 1 < delta_y.len() {
                delta_y[i - 1]
            } else {
                0.0
            };
            y_pos += dy * scale_y;
            s.push_str(&format!("<tspan x=\"{:.4}\" y=\"{:.4}\">{}</tspan>", x_pos, y_pos, glyph_out(*ch)));
        }
        s
    } else {
        // 无逐字定位：整段输出，占位符同样不渲染
        esc_xml(&text.chars().filter(|c| !is_placeholder(*c)).collect::<String>())
    };
    format!(
        "<text x=\"{x}\" y=\"{y}\" font-family=\"{ff}\" font-size=\"{fs}\"{fc}{bw}>{ct}</text>",
        x = base_x,
        y = base_y,
        ff = esc_xml_attr(&font_family),
        fs = font_size * scale_x,
        fc = fill_attr(text_obj.fill_color, text_obj.alpha),
        bw = bold,
        ct = content
    )
}

fn fill_attr(color: Option<(u8, u8, u8)>, alpha: Option<u8>) -> String {
    match (color, alpha) {
        (Some((r, g, b)), Some(a)) => format!(" fill=\"rgba({},{},{},{:.2})\"", r, g, b, a as f64 / 255.0),
        (Some((r, g, b)), None) => format!(" fill=\"rgb({},{},{})\"", r, g, b),
        (None, Some(a)) => format!(" fill=\"rgba(0,0,0,{:.2})\"", a as f64 / 255.0),
        (None, None) => String::new(),
    }
}

fn stroke_attr(color: Option<(u8, u8, u8)>, alpha: Option<u8>) -> String {
    match (color, alpha) {
        (Some((r, g, b)), Some(a)) => format!(" stroke=\"rgba({},{},{},{:.2})\"", r, g, b, a as f64 / 255.0),
        (Some((r, g, b)), None) => format!(" stroke=\"rgb({},{},{})\"", r, g, b),
        (None, Some(a)) => format!(" stroke=\"rgba(0,0,0,{:.2})\"", a as f64 / 255.0),
        (None, None) => String::new(),
    }
}

fn esc_xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn esc_xml_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// Convert OFD AbbreviatedData to SVG path data.
/// OFD commands: M(moveto), L(lineto), C(cubic bezier), Q(quadratic), A(arc), B(cubic bezier alias), Z(close)
fn ofd_path_to_svg(data: &str) -> String {
    let mut svg = String::new();
    let tokens: Vec<&str> = data.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i] {
            "M" => {
                if i + 2 < tokens.len() {
                    svg.push_str(&format!("M {} {} ", tokens[i+1], tokens[i+2]));
                    i += 3;
                } else { i += 1; }
            }
            "L" => {
                if i + 2 < tokens.len() {
                    svg.push_str(&format!("L {} {} ", tokens[i+1], tokens[i+2]));
                    i += 3;
                } else { i += 1; }
            }
            "C" => {
                if i + 6 < tokens.len() {
                    svg.push_str(&format!("C {} {} {} {} {} {} ",
                        tokens[i+1], tokens[i+2], tokens[i+3], tokens[i+4], tokens[i+5], tokens[i+6]));
                    i += 7;
                } else { i += 1; }
            }
            "B" => {
                // OFD B is also cubic bezier (same as C)
                if i + 6 < tokens.len() {
                    svg.push_str(&format!("C {} {} {} {} {} {} ",
                        tokens[i+1], tokens[i+2], tokens[i+3], tokens[i+4], tokens[i+5], tokens[i+6]));
                    i += 7;
                } else { i += 1; }
            }
            "Q" => {
                if i + 4 < tokens.len() {
                    svg.push_str(&format!("Q {} {} {} {} ",
                        tokens[i+1], tokens[i+2], tokens[i+3], tokens[i+4]));
                    i += 5;
                } else { i += 1; }
            }
            "A" => {
                if i + 7 < tokens.len() {
                    svg.push_str(&format!("A {} {} {} {} {} {} {} ",
                        tokens[i+1], tokens[i+2], tokens[i+3], tokens[i+4], tokens[i+5], tokens[i+6], tokens[i+7]));
                    i += 8;
                } else { i += 1; }
            }
            "S" => {
                // Smooth cubic bezier
                if i + 4 < tokens.len() {
                    svg.push_str(&format!("S {} {} {} {} ",
                        tokens[i+1], tokens[i+2], tokens[i+3], tokens[i+4]));
                    i += 5;
                } else { i += 1; }
            }
            "Z" | "z" => {
                svg.push('Z');
                i += 1;
            }
            _ => { i += 1; }
        }
    }
    svg
}

// =====================================================
// OFD Content Parsing
// =====================================================

/// Apply DrawParam defaults to paths and texts that have no explicit stroke/fill color.
/// Each object carries its own `layer_draw_param` from the Layer it belongs to,
/// so we apply per-object DrawParam inheritance rather than a single global default.
fn apply_draw_param_defaults(
    paths: &mut [OfdPathObject],
    texts: &mut [OfdTextObject],
    draw_params: &HashMap<u32, OfdDrawParam>,
) {
    // Cache resolved DrawParam results to avoid re-resolving the same ID
    let mut dp_cache: HashMap<u32, (f64, Option<(u8, u8, u8)>, Option<(u8, u8, u8)>)> = HashMap::new();

    for p in paths.iter_mut() {
        if let Some(dp_id) = p.layer_draw_param {
            let (lw, stroke, fill) = *dp_cache.entry(dp_id).or_insert_with(|| resolve_draw_param(draw_params, dp_id));
            if p.stroke_color.is_none() {
                p.stroke_color = stroke;
            }
            if p.fill_color.is_none() {
                p.fill_color = fill;
            }
            if p.line_width == 0.0 {
                p.line_width = lw;
            }
        }
        // Objects without layer_draw_param use OFD default (black stroke, no fill) — no inheritance
    }

    for t in texts.iter_mut() {
        if let Some(dp_id) = t.layer_draw_param {
            let (_lw, stroke, fill) = *dp_cache.entry(dp_id).or_insert_with(|| resolve_draw_param(draw_params, dp_id));
            if t.fill_color.is_none() {
                t.fill_color = fill;
            }
            if t.stroke_color.is_none() {
                t.stroke_color = stroke;
            }
        }
    }
}

/// Parse OFD content XML (Page or Template) and extract render objects.
/// Returns (text_objects, path_objects, image_objects)
/// Each object records its Layer's DrawParam ID in `layer_draw_param` for per-Layer inheritance.
fn parse_ofd_content(xml: &str) -> (Vec<OfdTextObject>, Vec<OfdPathObject>, Vec<OfdImageObject>) {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut text_objs = Vec::new();
    let mut path_objs = Vec::new();
    let mut img_objs = Vec::new();

    // We need to track context: which element we're in
    // TextObject, PathObject, ImageObject are direct children of Layer
    // TextCode is a child of TextObject
    // AbbreviatedData is a child of PathObject

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut current_text: Option<OfdTextObject> = None;
    let mut current_path: Option<OfdPathObject> = None;
    let mut current_img: Option<OfdImageObject> = None;
    let mut in_text_code = false;
    let mut current_seg: Option<OfdTextSegment> = None; // 当前 TextCode（一个 TextObject 可含多个）
    let mut current_layer_dp: Option<u32> = None; // DrawParam of the current Layer

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let tag_local = local_tag_name(&e.name());
                match tag_local.as_str() {
                    "Layer" => {
                        // Track this Layer's DrawParam
                        if let Some(v) = attr_val(&e, "DrawParam") {
                            current_layer_dp = v.parse().ok();
                        } else {
                            current_layer_dp = None;
                        }
                    }
                    "TextObject" => {
                        let mut t = OfdTextObject::default();
                        if let Some(v) = attr_val(&e, "ID") { t.id = v.parse().unwrap_or(0); }
                        if let Some(v) = attr_val(&e, "Boundary") {
                            if let Some(f4) = parse_f4(&v) { t.boundary = f4; }
                        }
                        if let Some(v) = attr_val(&e, "Font") { t.font_id = v.parse().unwrap_or(0); }
                        if let Some(v) = attr_val(&e, "Size") { t.size = v.parse().unwrap_or(3.175); }
                        if let Some(v) = attr_val(&e, "CTM") { t.ctm = parse_f6(&v); }
                        if let Some(v) = attr_val(&e, "Alpha") { t.alpha = v.parse().ok(); }
                        if let Some(v) = attr_val(&e, "BlendMode") { t.blend_mode = Some(v); }
                        if let Some(v) = attr_val(&e, "Weight") { t.weight = v.parse().unwrap_or(400); }
                        t.layer_draw_param = current_layer_dp;
                        current_text = Some(t);
                    }
                    "PathObject" => {
                        let mut p = OfdPathObject::default();
                        if let Some(v) = attr_val(&e, "ID") { p.id = v.parse().unwrap_or(0); }
                        if let Some(v) = attr_val(&e, "Boundary") {
                            if let Some(f4) = parse_f4(&v) { p.boundary = f4; }
                        }
                        if let Some(v) = attr_val(&e, "LineWidth") { p.line_width = v.parse().unwrap_or(0.25); }
                        if let Some(v) = attr_val(&e, "CTM") { p.ctm = parse_f6(&v); }
                        if let Some(v) = attr_val(&e, "Fill") { p.fill = v == "true"; }
                        if let Some(v) = attr_val(&e, "Alpha") { p.alpha = v.parse().ok(); }
                        p.layer_draw_param = current_layer_dp;
                        current_path = Some(p);
                    }
                    "ImageObject" => {
                        let mut img = OfdImageObject::default();
                        if let Some(v) = attr_val(&e, "ID") { img.id = v.parse().unwrap_or(0); }
                        if let Some(v) = attr_val(&e, "Boundary") {
                            if let Some(f4) = parse_f4(&v) { img.boundary = f4; }
                        }
                        if let Some(v) = attr_val(&e, "ResourceID") { img.resource_id = v.parse().unwrap_or(0); }
                        if let Some(v) = attr_val(&e, "CTM") { img.ctm = parse_f6(&v); }
                        if let Some(v) = attr_val(&e, "BlendMode") { img.blend_mode = Some(v); }
                        if let Some(v) = attr_val(&e, "Alpha") { img.alpha = v.parse().ok(); }
                        if let Some(v) = attr_val(&e, "ImageMask") { img.image_mask = v.parse().ok(); }
                        current_img = Some(img);
                    }
                    "TextCode" => {
                        in_text_code = true;
                        if let Some(ref mut t) = current_text {
                            let mut seg = OfdTextSegment::default();
                            // 表46：X/Y 不出现时沿用上一个 TextCode 的坐标（对象内首个 TextCode 必需；
                            // 镜像字段 t.text_x/t.text_y 即上一个已解析 TextCode 的坐标）
                            seg.text_x = attr_val(&e, "X").and_then(|v| v.parse().ok()).unwrap_or(t.text_x);
                            seg.text_y = attr_val(&e, "Y").and_then(|v| v.parse().ok()).unwrap_or(t.text_y);
                            if let Some(v) = attr_val(&e, "DeltaX") {
                                seg.delta_x = parse_delta_values(&v);
                            }
                            if let Some(v) = attr_val(&e, "DeltaY") {
                                seg.delta_y = parse_delta_values(&v); // 与 DeltaX 相同格式
                            }
                            // 镜像到对象级字段：单 TextCode 时即本段参数；多 TextCode 时保留末段
                            // （`extract_invoice_from_body_coords` 等提取路径按对象级 X/Y 定位）
                            t.text_x = seg.text_x;
                            t.text_y = seg.text_y;
                            t.delta_x = seg.delta_x.clone();
                            t.delta_y = seg.delta_y.clone();
                            current_seg = Some(seg);
                        }
                    }
                    "AbbreviatedData" => {
                        let text = read_element_text(&mut reader);
                        if let Some(ref mut p) = current_path {
                            p.abbreviated_data = text;
                        }
                        continue;
                    }
                    "StrokeColor" => {
                        if let Some(v) = attr_val(&e, "Value") {
                            if let Some(c) = parse_color(&v) {
                                if let Some(ref mut p) = current_path { p.stroke_color = Some(c); }
                                if let Some(ref mut t) = current_text { t.stroke_color = Some(c); }
                            }
                        }
                    }
                    "FillColor" => {
                        if let Some(v) = attr_val(&e, "Value") {
                            if let Some(c) = parse_color(&v) {
                                if let Some(ref mut p) = current_path { p.fill_color = Some(c); }
                                if let Some(ref mut t) = current_text { t.fill_color = Some(c); }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) => {
                // Self-closing elements like <ImageObject ... /> or <TextObject ... />
                let tag_local = local_tag_name(&e.name());
                match tag_local.as_str() {
                    "Layer" => {
                        // Self-closing Layer: just update current_layer_dp
                        if let Some(v) = attr_val(&e, "DrawParam") {
                            current_layer_dp = v.parse().ok();
                        } else {
                            current_layer_dp = None;
                        }
                    }
                    "TextObject" => {
                        let mut t = OfdTextObject::default();
                        if let Some(v) = attr_val(&e, "ID") { t.id = v.parse().unwrap_or(0); }
                        if let Some(v) = attr_val(&e, "Boundary") {
                            if let Some(f4) = parse_f4(&v) { t.boundary = f4; }
                        }
                        if let Some(v) = attr_val(&e, "Font") { t.font_id = v.parse().unwrap_or(0); }
                        if let Some(v) = attr_val(&e, "Size") { t.size = v.parse().unwrap_or(3.175); }
                        if let Some(v) = attr_val(&e, "CTM") { t.ctm = parse_f6(&v); }
                        if let Some(v) = attr_val(&e, "Alpha") { t.alpha = v.parse().ok(); }
                        if let Some(v) = attr_val(&e, "Weight") { t.weight = v.parse().unwrap_or(400); }
                        t.layer_draw_param = current_layer_dp;
                        text_objs.push(t);
                    }
                    "PathObject" => {
                        let mut p = OfdPathObject::default();
                        if let Some(v) = attr_val(&e, "ID") { p.id = v.parse().unwrap_or(0); }
                        if let Some(v) = attr_val(&e, "Boundary") {
                            if let Some(f4) = parse_f4(&v) { p.boundary = f4; }
                        }
                        if let Some(v) = attr_val(&e, "LineWidth") { p.line_width = v.parse().unwrap_or(0.25); }
                        if let Some(v) = attr_val(&e, "CTM") { p.ctm = parse_f6(&v); }
                        if let Some(v) = attr_val(&e, "Fill") { p.fill = v == "true"; }
                        if let Some(v) = attr_val(&e, "Alpha") { p.alpha = v.parse().ok(); }
                        p.layer_draw_param = current_layer_dp;
                        path_objs.push(p);
                    }
                    "ImageObject" => {
                        let mut img = OfdImageObject::default();
                        if let Some(v) = attr_val(&e, "ID") { img.id = v.parse().unwrap_or(0); }
                        if let Some(v) = attr_val(&e, "Boundary") {
                            if let Some(f4) = parse_f4(&v) { img.boundary = f4; }
                        }
                        if let Some(v) = attr_val(&e, "ResourceID") { img.resource_id = v.parse().unwrap_or(0); }
                        if let Some(v) = attr_val(&e, "CTM") { img.ctm = parse_f6(&v); }
                        if let Some(v) = attr_val(&e, "Alpha") { img.alpha = v.parse().ok(); }
                        if let Some(v) = attr_val(&e, "ImageMask") { img.image_mask = v.parse().ok(); }
                        img_objs.push(img);
                    }
                    "StrokeColor" => {
                        if let Some(v) = attr_val(&e, "Value") {
                            if let Some(c) = parse_color(&v) {
                                if let Some(ref mut p) = current_path { p.stroke_color = Some(c); }
                                if let Some(ref mut t) = current_text { t.stroke_color = Some(c); }
                            }
                        }
                    }
                    "FillColor" => {
                        if let Some(v) = attr_val(&e, "Value") {
                            if let Some(c) = parse_color(&v) {
                                if let Some(ref mut p) = current_path { p.fill_color = Some(c); }
                                if let Some(ref mut t) = current_text { t.fill_color = Some(c); }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if in_text_code {
                    if let Ok(s) = t.unescape() {
                        let decoded = decode_ofd_escapes(&s);
                        // 段内文本供渲染；对象级文本保持拼接（提取路径沿用旧口径）
                        if let Some(ref mut seg) = current_seg {
                            seg.text.push_str(&decoded);
                        }
                        if let Some(ref mut text_obj) = current_text {
                            text_obj.text.push_str(&decoded);
                        }
                    }
                }
            }
            Ok(Event::End(e)) => {
                let tag_local = local_tag_name(&e.name());
                match tag_local.as_str() {
                    "Layer" => {
                        // Exiting Layer: reset to no DrawParam
                        current_layer_dp = None;
                    }
                    "TextObject" => {
                        if let Some(t) = current_text.take() {
                            text_objs.push(t);
                        }
                    }
                    "PathObject" => {
                        if let Some(p) = current_path.take() {
                            path_objs.push(p);
                        }
                    }
                    "ImageObject" => {
                        if let Some(img) = current_img.take() {
                            img_objs.push(img);
                        }
                    }
                    "TextCode" => {
                        in_text_code = false;
                        if let (Some(seg), Some(ref mut t)) = (current_seg.take(), current_text.as_mut()) {
                            if !seg.text.is_empty() {
                                t.segments.push(seg);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }

    (text_objs, path_objs, img_objs)
}

/// Get local tag name (strip namespace prefix)
fn local_tag_name(name: &quick_xml::name::QName) -> String {
    let bytes = name.as_ref();
    if let Some(pos) = bytes.iter().position(|&b| b == b':') {
        String::from_utf8_lossy(&bytes[pos + 1..]).to_string()
    } else {
        String::from_utf8_lossy(bytes).to_string()
    }
}

// =====================================================
// OFD Metadata Parsing
// =====================================================

/// Parse OFD.xml CustomData entries for quick invoice data extraction
fn parse_ofd_custom_data(xml: &str) -> HashMap<String, String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut map = HashMap::new();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(e)) => {
                // Self-closing tag: <CustomData Name="xxx"/> — value is empty, do NOT call read_element_text
                let tag = local_tag_name(&e.name());
                if tag == "CustomData" {
                    if let Some(name) = attr_val(&e, "Name") {
                        map.insert(name, String::new());
                    }
                }
            }
            Ok(Event::Start(e)) => {
                let tag = local_tag_name(&e.name());
                if tag == "CustomData" {
                    if let Some(name) = attr_val(&e, "Name") {
                        let value = read_element_text(&mut reader);
                        map.insert(name, value);
                        continue;
                    }
                }
            }
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }
    map
}

/// Parse Tags/CustomTag.xml — maps semantic field names to TextObject IDs
fn parse_custom_tag(xml: &str) -> HashMap<String, Vec<u32>> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut map: HashMap<String, Vec<u32>> = HashMap::new();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut current_field = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let tag = local_tag_name(&e.name());
                match tag.as_str() {
                    "InvoiceNo" | "IssueDate" | "BuyerName" | "BuyerTaxID" |
                    "SellerName" | "SellerTaxID" | "TaxExclusiveTotalAmount" |
                    "TaxTotalAmount" | "TaxInclusiveTotalAmount" | "Amount" |
                    "TaxAmount" | "InvoiceClerk" | "Item" | "Price" | "Quantity" |
                    "Note" | "TaxScheme" | "MeasurementDimension" => {
                        current_field = tag;
                    }
                    "ObjectRef" => {
                        if !current_field.is_empty() {
                            // Read text content (the object ID)
                            let text = read_element_text(&mut reader);
                            if let Ok(id) = text.trim().parse::<u32>() {
                                map.entry(current_field.clone()).or_default().push(id);
                            }
                            continue;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let tag = local_tag_name(&e.name());
                match tag.as_str() {
                    "InvoiceNo" | "IssueDate" | "BuyerName" | "BuyerTaxID" |
                    "SellerName" | "SellerTaxID" | "TaxExclusiveTotalAmount" |
                    "TaxTotalAmount" | "TaxInclusiveTotalAmount" | "Amount" |
                    "TaxAmount" | "InvoiceClerk" | "Item" | "Price" | "Quantity" |
                    "Note" | "TaxScheme" | "MeasurementDimension" | "Buyer" | "Seller" => {
                        current_field.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }
    map
}

/// Parse PublicRes.xml for font definitions
fn parse_fonts(xml: &str) -> (HashMap<u32, OfdFont>, HashMap<u32, String>, HashMap<u32, OfdDrawParam>) {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut fonts = HashMap::new();
    let mut color_spaces = HashMap::new();
    let mut draw_params = HashMap::new();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut current_dp_id: Option<u32> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let tag = local_tag_name(&e.name());
                if tag == "Font" {
                    let mut font = OfdFont::default();
                    if let Some(v) = attr_val(&e, "ID") { font.id = v.parse().unwrap_or(0); }
                    if let Some(v) = attr_val(&e, "FontName") { font.font_name = v; }
                    if let Some(v) = attr_val(&e, "FamilyName") { font.family_name = v; }
                    fonts.insert(font.id, font);
                } else if tag == "ColorSpace" {
                    if let (Some(id_v), Some(type_v)) = (attr_val(&e, "ID"), attr_val(&e, "Type")) {
                        if let Ok(id) = id_v.parse::<u32>() {
                            color_spaces.insert(id, type_v);
                        }
                    }
                } else if tag == "DrawParam" {
                    let mut dp = OfdDrawParam::default();
                    if let Some(v) = attr_val(&e, "ID") { dp.id = v.parse().unwrap_or(0); }
                    if let Some(v) = attr_val(&e, "Relative") { dp.relative = v.parse().ok(); }
                    if let Some(v) = attr_val(&e, "LineWidth") { dp.line_width = v.parse().unwrap_or(0.25); }
                    current_dp_id = Some(dp.id);
                    draw_params.insert(dp.id, dp);
                } else if tag == "StrokeColor" {
                    if let Some(v) = attr_val(&e, "Value") {
                        if let Some(c) = parse_color(&v) {
                            if let Some(id) = current_dp_id {
                                if let Some(dp) = draw_params.get_mut(&id) {
                                    dp.stroke_color = Some(c);
                                }
                            }
                        }
                    }
                } else if tag == "FillColor" {
                    if let Some(v) = attr_val(&e, "Value") {
                        if let Some(c) = parse_color(&v) {
                            if let Some(id) = current_dp_id {
                                if let Some(dp) = draw_params.get_mut(&id) {
                                    dp.fill_color = Some(c);
                                }
                            }
                        }
                    }
                }
            }
            Ok(Event::End(e)) => {
                let tag = local_tag_name(&e.name());
                if tag == "DrawParam" { current_dp_id = None; }
            }
            Ok(Event::Empty(e)) => {
                let tag = local_tag_name(&e.name());
                if tag == "Font" {
                    let mut font = OfdFont::default();
                    if let Some(v) = attr_val(&e, "ID") { font.id = v.parse().unwrap_or(0); }
                    if let Some(v) = attr_val(&e, "FontName") { font.font_name = v; }
                    if let Some(v) = attr_val(&e, "FamilyName") { font.family_name = v; }
                    fonts.insert(font.id, font);
                } else if tag == "ColorSpace" {
                    if let (Some(id_v), Some(type_v)) = (attr_val(&e, "ID"), attr_val(&e, "Type")) {
                        if let Ok(id) = id_v.parse::<u32>() {
                            color_spaces.insert(id, type_v);
                        }
                    }
                } else if tag == "StrokeColor" {
                    // Self-closing: <ofd:StrokeColor Value="128 0 0" ColorSpace="2"/>
                    if let Some(v) = attr_val(&e, "Value") {
                        if let Some(c) = parse_color(&v) {
                            if let Some(id) = current_dp_id {
                                if let Some(dp) = draw_params.get_mut(&id) {
                                    dp.stroke_color = Some(c);
                                }
                            }
                        }
                    }
                } else if tag == "FillColor" {
                    if let Some(v) = attr_val(&e, "Value") {
                        if let Some(c) = parse_color(&v) {
                            if let Some(id) = current_dp_id {
                                if let Some(dp) = draw_params.get_mut(&id) {
                                    dp.fill_color = Some(c);
                                }
                            }
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }
    (fonts, color_spaces, draw_params)
}

/// Resolve DrawParam inheritance chain: returns fully resolved (line_width, stroke_color, fill_color)
fn resolve_draw_param(draw_params: &HashMap<u32, OfdDrawParam>, param_id: u32) -> (f64, Option<(u8, u8, u8)>, Option<(u8, u8, u8)>) {
    let mut lw = 0.25f64;
    let mut stroke: Option<(u8, u8, u8)> = None;
    let mut fill: Option<(u8, u8, u8)> = None;
    let mut visited = std::collections::HashSet::new();
    let mut current_id = param_id;
    // Walk the Relative chain: 4 → 3 → None
    loop {
        if !visited.insert(current_id) { break; } // prevent cycles
        if let Some(dp) = draw_params.get(&current_id) {
            if dp.line_width > 0.0 { lw = dp.line_width; }
            if stroke.is_none() && dp.stroke_color.is_some() { stroke = dp.stroke_color; }
            if fill.is_none() && dp.fill_color.is_some() { fill = dp.fill_color; }
            if let Some(rel) = dp.relative {
                current_id = rel;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    (lw, stroke, fill)
}

/// Parse DocumentRes.xml for image resources
fn parse_image_resources(xml: &str) -> HashMap<u32, String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut images = HashMap::new();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut current_id: Option<u32> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let tag = local_tag_name(&e.name());
                if tag == "MultiMedia" {
                    if let Some(v) = attr_val(&e, "ID") {
                        current_id = v.parse().ok();
                    }
                } else if tag == "MediaFile" {
                    let text = read_element_text(&mut reader);
                    if let Some(id) = current_id.take() {
                        images.insert(id, text.trim().to_string());
                    }
                    continue;
                }
            }
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }
    images
}

/// Parse Annotations XML for watermark layer.
/// Each Annot contains an Appearance with a global Boundary.
/// Inner TextObject/ImageObject boundaries are relative to the Appearance.
/// This function adds the Appearance offset to convert to page-global coordinates.
fn parse_annotations(xml: &str) -> (Vec<OfdTextObject>, Vec<OfdImageObject>) {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut all_texts = Vec::new();
    let mut all_imgs = Vec::new();

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    // Track current Appearance offset (x, y) to apply to inner objects
    let mut appearance_offset: Option<(f64, f64)> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let tag = local_tag_name(&e.name());
                match tag.as_str() {
                    "Appearance" => {
                        if let Some(v) = attr_val(&e, "Boundary") {
                            if let Some((x, y, _w, _h)) = parse_f4(&v) {
                                appearance_offset = Some((x, y));
                            }
                        }
                    }
                    "TextObject" | "ImageObject" => {
                        // We're inside an Appearance — parse the inner XML fragment
                        // by collecting until the matching End tag, then feed to parse_ofd_content
                        // Simpler approach: reconstruct a minimal Content XML with the object
                        let mut depth = 1u32;
                        let mut frag = format!("<ofd:Content><ofd:Layer>");
                        frag.push_str(&format!("<{} ", tag));
                        // Re-add attributes from the start element
                        for attr in e.attributes().flatten() {
                            let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
                            let val = std::str::from_utf8(&attr.value).unwrap_or("");
                            frag.push_str(&format!("{}=\"{}\" ", key, esc_xml_attr(val)));
                        }
                        frag.push('>');
                        // Read until matching End tag
                        loop {
                            let mut inner_buf = Vec::new();
                            match reader.read_event_into(&mut inner_buf) {
                                Ok(Event::Start(inner_e)) => {
                                    depth += 1;
                                    let inner_tag = local_tag_name(&inner_e.name());
                                    frag.push_str(&format!("<{} ", inner_tag));
                                    for attr in inner_e.attributes().flatten() {
                                        let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
                                        let val = std::str::from_utf8(&attr.value).unwrap_or("");
                                        frag.push_str(&format!("{}=\"{}\" ", key, esc_xml_attr(val)));
                                    }
                                    frag.push('>');
                                }
                                Ok(Event::Empty(inner_e)) => {
                                    let inner_tag = local_tag_name(&inner_e.name());
                                    frag.push_str(&format!("<{} ", inner_tag));
                                    for attr in inner_e.attributes().flatten() {
                                        let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
                                        let val = std::str::from_utf8(&attr.value).unwrap_or("");
                                        frag.push_str(&format!("{}=\"{}\" ", key, esc_xml_attr(val)));
                                    }
                                    frag.push_str("/>");
                                }
                                Ok(Event::Text(t)) => {
                                    if let Ok(s) = t.unescape() {
                                        frag.push_str(&esc_xml(&s));
                                    }
                                }
                                Ok(Event::End(_inner_e)) => {
                                    depth -= 1;
                                    let inner_tag = local_tag_name(&_inner_e.name());
                                    frag.push_str(&format!("</{}>", inner_tag));
                                    if depth == 0 { break; }
                                }
                                Ok(Event::Eof) => break,
                                _ => {}
                            }
                        }
                        frag.push_str("</ofd:Layer></ofd:Content>");

                        let (mut texts, _, mut imgs) = parse_ofd_content(&frag);
                        // Apply Appearance offset to convert local → global coordinates
                        if let Some((ox, oy)) = appearance_offset {
                            for t in &mut texts {
                                t.boundary.0 += ox;
                                t.boundary.1 += oy;
                            }
                            for i in &mut imgs {
                                i.boundary.0 += ox;
                                i.boundary.1 += oy;
                            }
                        }
                        all_texts.extend(texts);
                        all_imgs.extend(imgs);
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let tag = local_tag_name(&e.name());
                if tag == "Appearance" {
                    appearance_offset = None;
                }
            }
            Ok(Event::Eof) => break,
            _ => {}
        }
        buf.clear();
    }

    (all_texts, all_imgs)
}

// =====================================================
// SVG Assembly
// =====================================================

/// Build complete SVG from parsed OFD layers
fn build_ofd_svg(
    page_w: f64,
    page_h: f64,
    tpl_texts: &[OfdTextObject],
    tpl_paths: &[OfdPathObject],
    tpl_imgs: &[OfdImageObject],
    page_texts: &[OfdTextObject],
    page_paths: &[OfdPathObject],
    page_imgs: &[OfdImageObject],
    annot_texts: &[OfdTextObject],
    annot_imgs: &[OfdImageObject],
    font_map: &HashMap<u32, OfdFont>,
    color_spaces: &HashMap<u32, String>,
    image_data: &HashMap<u32, String>,
    image_sizes: &HashMap<u32, (u32, u32)>,
) -> String {
    let scale = 3.5; // Scale factor: 1mm → 3.5 SVG units for good resolution
    let vw = page_w * scale;
    let vh = page_h * scale;

    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" viewBox=\"0 0 {:.1} {:.1}\" width=\"{:.1}\" height=\"{:.1}\" style=\"background:white\">",
        vw, vh, vw, vh
    );

    // Layer 1: Template (background) — grid lines and static labels
    svg.push_str("<g id=\"template\">");
    for p in tpl_paths {
        svg.push_str(&build_svg_path(p, scale));
    }
    for t in tpl_texts {
        svg.push_str(&build_svg_text(t, font_map, color_spaces, scale, scale));
    }
    for img in tpl_imgs {
        svg.push_str(&build_svg_image(img, image_data, image_sizes, page_w, page_h, scale));
    }
    svg.push_str("</g>");

    // Layer 2: Content (data)
    svg.push_str("<g id=\"content\">");
    for p in page_paths {
        svg.push_str(&build_svg_path(p, scale));
    }
    for t in page_texts {
        svg.push_str(&build_svg_text(t, font_map, color_spaces, scale, scale));
    }
    for img in page_imgs {
        svg.push_str(&build_svg_image(img, image_data, image_sizes, page_w, page_h, scale));
    }
    svg.push_str("</g>");

    // Layer 3: Annotations (watermarks)
    svg.push_str("<g id=\"annotations\">");
    for t in annot_texts {
        svg.push_str(&build_svg_text(t, font_map, color_spaces, scale, scale));
    }
    for img in annot_imgs {
        svg.push_str(&build_svg_image(img, image_data, image_sizes, page_w, page_h, scale));
    }
    svg.push_str("</g>");

    svg.push_str("</svg>");
    svg
}

/// Compute the bounding box (u_min, v_min, u_max, v_max) of an OFD AbbreviatedData path.
/// Only M/L/C/B/Q/S endpoint coordinates are considered (good enough for frame lines;
/// arcs are rare on invoice frames and their control-point approximation is acceptable).
fn ofd_path_bbox(data: &str) -> Option<(f64, f64, f64, f64)> {
    let tokens: Vec<&str> = data.split_whitespace().collect();
    let mut min_u = f64::MAX; let mut min_v = f64::MAX;
    let mut max_u = f64::MIN; let mut max_v = f64::MIN;
    let mut i = 0;
    let mut extend = |us: &[f64], vs: &[f64]| {
        for &u in us { if u < min_u { min_u = u; } if u > max_u { max_u = u; } }
        for &v in vs { if v < min_v { min_v = v; } if v > max_v { max_v = v; } }
    };
    let pairs = |t: &[&str], base: usize, n: usize| -> Option<(Vec<f64>, Vec<f64>)> {
        let mut us = Vec::new(); let mut vs = Vec::new();
        for k in 0..n {
            let u: f64 = t.get(base + k * 2)?.parse().ok()?;
            let v: f64 = t.get(base + k * 2 + 1)?.parse().ok()?;
            us.push(u); vs.push(v);
        }
        Some((us, vs))
    };
    while i < tokens.len() {
        let n = match tokens[i] {
            "M" | "L" => 1,
            "C" | "B" | "S" => 2, // S: end + implied control — endpoints only
            "Q" => 2,
            _ => { i += 1; continue; }
        };
        if let Some((us, vs)) = pairs(&tokens, i + 1, n) {
            extend(&us, &vs);
            i += 1 + n * 2;
        } else {
            i += 1;
        }
    }
    if min_u == f64::MAX { None } else { Some((min_u, min_v, max_u, max_v)) }
}

/// Build SVG path from OFD PathObject
fn build_svg_path(p: &OfdPathObject, scale: f64) -> String {
    if p.abbreviated_data.is_empty() {
        return String::new();
    }

    let svg_d = ofd_path_to_svg(&p.abbreviated_data);
    if svg_d.is_empty() {
        return String::new();
    }

    // Normal case: Boundary = (x, y, w, h) in mm, path data in local mm coords.
    // Apply translate to Boundary position, then scale mm → SVG units.
    let mut transform = format!(
        "translate({:.4},{:.4}) scale({:.4})",
        p.boundary.0 * scale, p.boundary.1 * scale, scale
    );

    // CTM case (数电票 producers): path data lives in a DESIGN coordinate space
    // (hundreds of units) and CTM carries the design→mm scale; Boundary carries
    // the true page placement (CTM's e/f translation is unreliable there — it maps
    // the shared design origin, not this object's local origin). Two observed
    // variants unify into one model:
    //   a) design-space coords (e.g. u∈[9.8, 804.8]) — offset from own bbox origin
    //   b) already-local coords (u,v from 0) — bbox offset is a no-op
    // Model: page pos = Boundary.xy + CTM-linear(path - bbox anchor), where the
    // v anchor is v_max when CTM.d < 0 (design v axis points up) else v_min.
    // Guard: only when path bbox × CTM scale ≈ Boundary size (±15%), b/c are 0.
    if let Some((a, b, c, d, _e, _f)) = p.ctm {
        if b == 0.0 && c == 0.0 && a != 0.0 && d != 0.0 && p.boundary.2 > 0.0 && p.boundary.3 > 0.0 {
            if let Some((u0, v0, u1, v1)) = ofd_path_bbox(&p.abbreviated_data) {
                let ew = (u1 - u0) * a.abs();
                let eh = (v1 - v0) * d.abs();
                let size_match = (ew - p.boundary.2).abs() <= p.boundary.2 * 0.15
                    && (eh - p.boundary.3).abs() <= p.boundary.3 * 0.15;
                if size_match {
                    let v_anchor = if d < 0.0 { v1 } else { v0 };
                    // +0.0 normalizes -0.0 to 0.0 for clean output
                    let (ou, oa) = (-u0 + 0.0, -v_anchor + 0.0);
                    transform = format!(
                        "translate({:.4},{:.4}) scale({:.4},{:.4}) translate({:.4},{:.4})",
                        p.boundary.0 * scale, p.boundary.1 * scale,
                        a * scale, d * scale,
                        ou, oa
                    );
                }
            }
        }
    }

    let mut attrs = String::new();
    attrs.push_str(&format!(" transform=\"{}\"", transform));
    attrs.push_str(&format!(" stroke-width=\"{:.4}\"", p.line_width));
    if p.fill {
        attrs.push_str(" fill-rule=\"nonzero\"");
    }
    // Per OFD spec, default stroke color is black (0,0,0) when not specified.
    // This ensures PathObjects without explicit StrokeColor (and no DrawParam inheritance)
    // are still visible — e.g. the ⊗ symbol (circled-X) in the uppercase amount area.
    attrs.push_str(&stroke_attr(p.stroke_color.or(Some((0, 0, 0))), p.alpha));
    if p.fill {
        if let Some(fc) = p.fill_color {
            attrs.push_str(&fill_attr(Some(fc), p.alpha));
        } else {
            // fill=true but no explicit fill_color: per OFD spec default is black,
            // but filling solid would hide internal strokes (e.g. the ⊗ cross).
            // Use fill="none" so the circle outline + X cross are both visible via stroke.
            attrs.push_str(" fill=\"none\"");
        }
    } else {
        attrs.push_str(" fill=\"none\"");
    }

    format!("<g{}><path d=\"{}\"/></g>", attrs, svg_d)
}

/// Build SVG image from OFD ImageObject
fn build_svg_image(
    img: &OfdImageObject,
    image_data: &HashMap<u32, String>,
    image_sizes: &HashMap<u32, (u32, u32)>,
    page_w: f64,
    page_h: f64,
    scale: f64,
) -> String {
    let data_url = match image_data.get(&img.resource_id) {
        Some(url) => url,
        None => return String::new(),
    };

    // Boundary = (x, y, w, h) in mm — normally defines where and how big the image is.
    // Exception (数电票 producers): some ImageObjects carry a full-page placeholder
    // Boundary (e.g. "0 0 210 297") while the real placement lives in the CTM.
    // Verified against the PDF twin of the same invoice: this producer writes CTM as
    // [displayW 0 0 displayH posX posY] in mm (e.g. QR: 18.2×18.2 @ (5.3,3) matches
    // the PDF's 18.59×18.59 @ (5.31,2.9)). Fallback to the spec semantics
    // (a/d = pixel density → size = px/a, px/d) only when a/d is absurdly large.
    let mut x = img.boundary.0;
    let mut y = img.boundary.1;
    let mut w = img.boundary.2;
    let mut h = img.boundary.3;
    let is_placeholder = page_w > 0.0 && page_h > 0.0
        && w >= page_w * 0.95 && h >= page_h * 0.95;
    if is_placeholder {
        if let Some((a, _b, _c, d, e, f)) = img.ctm {
            if a > 0.0 && d > 0.0 {
                let limit = 2.0 * page_w.max(page_h);
                let (cw, ch) = if a <= limit && d <= limit {
                    (a, d)
                } else if let Some(&(pw, ph)) = image_sizes.get(&img.resource_id) {
                    (pw as f64 / a, ph as f64 / d)
                } else {
                    (a.min(limit), d.min(limit))
                };
                x = e;
                y = f;
                w = cw;
                h = ch;
            }
        }
    }

    let opacity = img.alpha.map(|a| format!(" opacity=\"{:.2}\"", a as f64 / 255.0)).unwrap_or_default();

    format!(
        "<image href=\"{}\" x=\"{:.4}\" y=\"{:.4}\" width=\"{:.4}\" height=\"{:.4}\"{}/>",
        data_url, x * scale, y * scale, w * scale, h * scale, opacity
    )
}

// =====================================================
// Bitmap Fallback: Extract Images from OFD ZIP
// =====================================================

/// Extract embedded images from an OFD file (Chinese electronic invoice format)
/// OFD is a ZIP archive containing XML page descriptions and image resources.
/// For electronic invoices, the content is typically a full-page image.
///
/// Filtering strategy:
/// 1. Path-based: exclude Seals/, Signs/ directories (stamp/signature images)
/// 2. Dimension-based: prefer images where the longest side >= 500px
///    (QR codes ~100-200px, seal stamps ~300-400px; full invoice pages > 800px)
///    If large images exist, small ones are filtered out.
///    If NO large images exist (vector-based OFD), fall back to including all path-filtered images.
/// 3. Per-page dedup: keep only the largest image per page index
fn extract_ofd_images(ofd_path: &str) -> Result<Vec<(String, String, u32, u32)>, String> {
    use base64::Engine;
    use std::io::Read;

    let file = std::fs::File::open(ofd_path)
        .map_err(|e| format!("打开OFD文件失败: {}", e))?;

    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| format!("解析OFD ZIP失败: {}", e))?;

    // Collect candidate image entries with path-based filtering
    // OFD structure:
    //   Doc_0/Pages/Page_0/Res/xxx.jpg   — per-page resources (invoice image, QR code)
    //   Doc_0/Res/xxx.jpg                 — document-level resources
    //   Doc_0/Seals/xxx.jpg               — seal/stamp images (EXCLUDE)
    //   Doc_0/Signs/xxx.jpg               — signature images (EXCLUDE)
    let mut image_entries: Vec<String> = Vec::new();

    for i in 0..archive.len() {
        let entry = archive.by_index(i).map_err(|e| format!("读取ZIP条目失败: {}", e))?;
        let name = entry.name().to_string();
        let lower = name.to_lowercase();

        // Path-based exclusion: skip Seals/, Signs/ directories and sign_/seal_ filenames
        let path_has_seal_or_sign = lower.contains("/seals/")
            || lower.contains("/signs/")
            || lower.contains("\\seals\\")
            || lower.contains("\\signs\\")
            || lower.contains("sign_")
            || lower.contains("seal_");

        if (lower.ends_with(".jpg") || lower.ends_with(".jpeg") || lower.ends_with(".png"))
            && !path_has_seal_or_sign
        {
            image_entries.push(name);
        }
    }

    if image_entries.is_empty() {
        return Err("OFD文件中未找到图片资源".to_string());
    }

    // Extract page index from path for grouping
    fn extract_page_index(path: &str) -> u32 {
        let lower = path.to_lowercase();
        if let Some(pos) = lower.find("page_") {
            let rest = &path[pos + 5..];
            let num_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(idx) = num_str.parse::<u32>() {
                return idx;
            }
        }
        u32::MAX // no page index found, sort last
    }

    // Read and decode all candidate images, collect (data_url, ext, w, h, page_idx)
    const MIN_LONGEST_SIDE: u32 = 500; // Full invoice pages are always > 500px; QR codes/seals are smaller
    let mut all_decoded: Vec<(String, String, u32, u32, u32)> = Vec::new(); // (data_url, ext, w, h, page_idx)

    for entry_name in &image_entries {
        let mut entry = archive.by_name(entry_name)
            .map_err(|e| format!("读取OFD图片失败: {}", e))?;
        let mut data = Vec::new();
        entry.read_to_end(&mut data)
            .map_err(|e| format!("读取OFD图片数据失败: {}", e))?;

        // Decode image to get dimensions
        let (w, h) = match image::load_from_memory(&data) {
            Ok(img) => img.dimensions(),
            Err(_) => {
                log::warn!("OFD: 无法解码图片 {}, 跳过", entry_name);
                continue;
            }
        };

        // Determine MIME type and extension
        let lower = entry_name.to_lowercase();
        let (mime, img_ext) = if lower.ends_with(".png") {
            ("image/png", "png")
        } else {
            ("image/jpeg", "jpg")
        };

        let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
        let data_url = format!("data:{};base64,{}", mime, b64);

        let page_idx = extract_page_index(entry_name);
        let longest_side = w.max(h);

        log::info!("OFD: 图片 {} ({}x{}, longest={}, page_idx={})",
            entry_name, w, h, longest_side, page_idx);
        all_decoded.push((data_url, img_ext.to_string(), w, h, page_idx));
    }

    if all_decoded.is_empty() {
        return Err("OFD文件中未找到可解码的图片资源".to_string());
    }

    // Two-pass strategy:
    // Pass 1: Try to find large images (>= MIN_LONGEST_SIDE) — these are likely full invoice pages
    // Pass 2: If no large images found (vector-based OFD), fall back to all decoded images
    let large_images: Vec<_> = all_decoded.iter()
        .filter(|c| c.2.max(c.3) >= MIN_LONGEST_SIDE)
        .cloned()
        .collect();

    let candidates = if !large_images.is_empty() {
        log::info!("OFD: 找到{}张大图(>={}px)，过滤小图片", large_images.len(), MIN_LONGEST_SIDE);
        large_images
    } else {
        log::warn!("OFD: 未找到大图(>={}px)，可能是矢量版式OFD，回退到包含所有图片", MIN_LONGEST_SIDE);
        all_decoded
    };

    // Per-page dedup: keep only the largest image (by pixel count) per page index
    let mut sorted = candidates;
    sorted.sort_by(|a, b| {
        a.4.cmp(&b.4) // sort by page_idx first
            .then((b.2 * b.3).cmp(&(a.2 * a.3))) // then by pixel count descending
    });

    let mut seen_pages = std::collections::HashSet::new();
    let mut results = Vec::new();
    for (data_url, img_ext, w, h, page_idx) in sorted {
        if seen_pages.insert(page_idx) {
            results.push((data_url, img_ext, w, h));
        } else {
            log::info!("OFD: 页面{}已保留最大图片，跳过重复", page_idx);
        }
    }

    if results.is_empty() {
        return Err("OFD文件中未找到有效的发票页面图片（可能为矢量版式OFD，建议转换为PDF后使用）".to_string());
    }

    log::info!("OFD extracted {} page images from {}", results.len(), ofd_path);
    Ok(results)
}

// =====================================================
// Text-based Invoice Extraction (Fallback)
// =====================================================

/// Semi-structured extraction for 数电票 OFD (digital e-invoices, e.g. toll invoices).
///
/// These OFDs have no Template layer and no CustomData/CustomTag: template labels
/// and data values live in the same Content.xml as two Layers, so label-value
/// sequence proximity is useless (values are 10+ texts away from labels).
/// However, data-layer TextObjects carry page-absolute TextCode X/Y coordinates
/// (their Boundary spans the whole page), and 数电票 layout is standardized:
///   - invoice no / issue date: top-right area (ny < 0.2)
///   - buyer/seller names + credit codes: middle band, left/right halves
///   - ¥ amounts: bottom band (ny > 0.5) — 合计 row + 价税合计 row
///
/// Returns default (all-None) info when the input doesn't match the 数电票
/// data-layer signature, so callers can safely fall back to other extractors.
fn extract_invoice_from_body_coords(texts: &[OfdTextObject], page_w: f64, page_h: f64) -> OfdInvoiceInfo {
    let mut info = OfdInvoiceInfo::default();
    if page_w <= 0.0 || page_h <= 0.0 {
        return info;
    }

    // 数电票数据层签名：Boundary 覆盖 ≥70% 页面 → TextCode X/Y 为页面绝对 mm 坐标。
    // 普通 OFD（含 dzcp）的数据层 Boundary 是局部小矩形，自动排除。
    let body: Vec<&OfdTextObject> = texts.iter()
        .filter(|t| t.boundary.2 >= page_w * 0.7 && t.boundary.3 >= page_h * 0.7)
        .collect();
    if body.is_empty() {
        return info;
    }

    let half_w = page_w * 0.5;
    let mut no_candidates: Vec<String> = Vec::new();
    let mut name_candidates: Vec<(f64, f64, String)> = Vec::new(); // (y, x, text)
    let mut taxid_candidates: Vec<(f64, f64, String)> = Vec::new(); // (y, x, text)
    let mut yen_amounts: Vec<(f64, f64, f64)> = Vec::new(); // (y, x, value)

    for t in &body {
        let text = t.text.trim();
        if text.is_empty() {
            continue;
        }
        let (x, y) = (t.text_x, t.text_y);
        let ny = y / page_h;

        // 开票日期：头部区「YYYY年MM月DD日」
        if info.invoice_date.is_none() && ny < 0.2 {
            if let Some(d) = parse_cn_date(text) {
                info.invoice_date = Some(d);
            }
        }

        // 发票号候选：头部区纯数字（10~20 位，数电票为 20 位）
        if ny < 0.2 && text.len() >= 10 && text.chars().all(|c| c.is_ascii_digit()) {
            no_candidates.push(text.to_string());
        }

        // 税号候选：18 位大写字母数字（统一社会信用代码）
        if text.chars().count() == 18
            && text.chars().all(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
        {
            taxid_candidates.push((y, x, text.to_string()));
        }

        // 名称候选：纯 CJK 文本（≥2 字），购销信息区
        let is_pure_cjk = text.chars().count() >= 2
            && text.chars().all(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
        if is_pure_cjk && ny > 0.15 && ny < 0.5 {
            name_candidates.push((y, x, text.to_string()));
        }

        // ¥ 金额：合计区
        if text.starts_with('¥') || text.starts_with('￥') {
            let amt_str = text.trim_start_matches('¥').trim_start_matches('￥').trim();
            if let Ok(v) = amt_str.parse::<f64>() {
                if ny > 0.5 {
                    yen_amounts.push((y, x, v));
                }
            }
        }
    }

    // 发票号：取最长候选（数电票 20 位 > 其他短数字串）
    if !no_candidates.is_empty() {
        no_candidates.sort_by_key(|s| std::cmp::Reverse(s.len()));
        info.invoice_no = Some(no_candidates[0].clone());
    }

    // 名称：有税号时以税号行为锚（±10% 页高），否则取首聚类行；左半=购买方，右半=销售方
    if !name_candidates.is_empty() {
        let anchor_y: Option<f64> = taxid_candidates.first().map(|(y, _, _)| *y);
        let in_band = |y: f64| -> bool {
            match anchor_y {
                Some(a) => (y - a).abs() <= page_h * 0.1,
                None => true, // 无锚时 name_candidates 已按 ny∈(0.15,0.5) 预过滤
            }
        };
        let mut rows: Vec<Vec<(f64, f64, String)>> = Vec::new(); // 聚类行（y 差 < 5% 页高）
        let mut sorted_names = name_candidates;
        sorted_names.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        for (y, x, name) in sorted_names {
            if !in_band(y) {
                continue;
            }
            match rows.last_mut() {
                Some(row) if (y - row[0].0).abs() < page_h * 0.05 => row.push((y, x, name)),
                _ => rows.push(vec![(y, x, name)]),
            }
        }
        if let Some(first_row) = rows.first() {
            for (_, x, name) in first_row {
                if *x < half_w {
                    if info.buyer_name.is_none() {
                        info.buyer_name = Some(name.clone());
                    }
                } else if info.seller_name.is_none() {
                    info.seller_name = Some(name.clone());
                }
            }
        }
    }

    // 税号：左半=购买方，右半=销售方
    for (_, x, code) in &taxid_candidates {
        if *x < half_w {
            if info.buyer_tax_id.is_none() {
                info.buyer_tax_id = Some(code.clone());
            }
        } else if info.seller_tax_id.is_none() {
            info.seller_tax_id = Some(code.clone());
        }
    }

    // 金额：按 y 聚类成行（差 < 5% 页高）。
    // 末行（价税合计行）最大 ¥ = 含税价；前行（合计行）大值=不含税、小值=税额。
    // 交叉校验失败时退回全局 ¥ 配对（a + b ≈ c）。
    if !yen_amounts.is_empty() {
        yen_amounts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut amt_rows: Vec<Vec<(f64, f64, f64)>> = Vec::new();
        for &(y, x, v) in &yen_amounts {
            match amt_rows.last_mut() {
                Some(row) if (y - row[0].0).abs() < page_h * 0.05 => row.push((y, x, v)),
                _ => amt_rows.push(vec![(y, x, v)]),
            }
        }

        let mut resolved = false;
        if amt_rows.len() >= 2 {
            let last_row = &amt_rows[amt_rows.len() - 1];
            let prev_row = &amt_rows[amt_rows.len() - 2];
            let tax_total = last_row.iter().map(|(_, _, v)| *v).fold(0.0_f64, f64::max);
            let mut prev_vals: Vec<f64> = prev_row.iter().map(|(_, _, v)| *v).collect();
            prev_vals.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
            if tax_total > 0.0 {
                let (no_tax, tax) = match prev_vals.len() {
                    0 => (None, None),
                    1 => (Some(prev_vals[0]), None),
                    _ => (Some(prev_vals[0]), Some(prev_vals[1])),
                };
                if let Some(nt) = no_tax {
                    let t = tax.unwrap_or(0.0);
                    let sum = ((nt + t) * 100.0).round() / 100.0;
                    if (sum - tax_total).abs() < 0.02 {
                        info.amount_tax = Some(tax_total);
                        info.amount_no_tax = Some(nt);
                        info.tax_amount = Some(t);
                        resolved = true;
                    }
                }
            }
        }

        if !resolved {
            // 全局配对：找 (a, b, c) 使 a + b ≈ c
            let vals: Vec<f64> = yen_amounts.iter().map(|(_, _, v)| *v).collect();
            let n = vals.len();
            'outer: for i in 0..n {
                for j in (i + 1)..n {
                    for k in 0..n {
                        if k == i || k == j {
                            continue;
                        }
                        let sum = ((vals[i] + vals[j]) * 100.0).round() / 100.0;
                        if (sum - vals[k]).abs() < 0.02 && vals[i] > 0.0 && vals[j] > 0.0 {
                            let (no_tax, tax) = if vals[i] >= vals[j] {
                                (vals[i], vals[j])
                            } else {
                                (vals[j], vals[i])
                            };
                            info.amount_tax = Some(vals[k]);
                            info.amount_no_tax = Some(no_tax);
                            info.tax_amount = Some(tax);
                            break 'outer;
                        }
                    }
                }
            }
            // 兜底：只有一行 ¥ 时，最大值视为含税价
            if info.amount_tax.is_none() && !vals.is_empty() {
                let max_v = vals.iter().cloned().fold(0.0_f64, f64::max);
                if max_v > 0.0 {
                    info.amount_tax = Some(max_v);
                }
            }
        }
    }

    if info.invoice_no.is_some() || info.amount_tax.is_some() || info.seller_name.is_some() {
        log::info!(
            "OFD 数电票坐标提取: no={:?} date={:?} buyer={:?} seller={:?} tax={:?} noTax={:?} taxAmt={:?}",
            info.invoice_no, info.invoice_date, info.buyer_name, info.seller_name,
            info.amount_tax, info.amount_no_tax, info.tax_amount
        );
    }

    info
}

/// Parse "YYYY年MM月DD日" into "YYYY-MM-DD". Returns None on mismatch.
fn parse_cn_date(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let find = |c: char| chars.iter().position(|&x| x == c);
    let (yi, mi, di) = (find('年')?, find('月')?, find('日')?);
    if yi == 0 || mi <= yi + 1 || di <= mi + 1 || di != chars.len() - 1 {
        return None;
    }
    let year: String = chars[..yi].iter().collect();
    let month: String = chars[yi + 1..mi].iter().collect();
    let day: String = chars[mi + 1..di].iter().collect();
    if !year.chars().all(|c| c.is_ascii_digit())
        || !month.chars().all(|c| c.is_ascii_digit())
        || !day.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    if year.len() != 4 || month.is_empty() || month.len() > 2 || day.is_empty() || day.len() > 2 {
        return None;
    }
    Some(format!("{}-{:0>2}-{:0>2}", year, month, day))
}

/// 发票类型判定：逐条文本匹配，避免跨文本拼接出来的假关键词。
/// 「普通」优先于「专用」—— 票面其它位置出现「专用」字样、或 OCR/文字层误识时，不会被误判成专票。
fn detect_invoice_type<'a, I>(texts: I) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut general = false;
    let mut special = false;
    let mut electronic = false;
    for text in texts {
        let s: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        if s.contains("普通发票") || s.contains("增值税普通") || s.contains("电子普通") {
            general = true;
        }
        if s.contains("专用发票") || s.contains("增值税专用") {
            special = true;
        }
        if s.contains("电子发票") {
            electronic = true;
        }
    }
    if general {
        Some("增值税普通发票".to_string())
    } else if special {
        Some("增值税专用发票".to_string())
    } else if electronic {
        Some("电子发票".to_string())
    } else {
        None
    }
}

/// Extract invoice data from text content when no CustomData or CustomTag is available.
/// This handles OFD files from non-standard producers that embed subset fonts
/// but don't include structured XML metadata.
///
/// Strategy: scan text objects in order, detect label patterns, and extract
/// values from the same text (after "：") or the next text object.
fn extract_invoice_from_text(texts: &[&OfdTextObject]) -> OfdInvoiceInfo {
    let mut info = OfdInvoiceInfo::default();
    let mut section = ""; // "buyer" or "seller"
    let mut name_count = 0; // 1st "名称" = buyer, 2nd = seller
    let mut taxid_count = 0; // 1st "纳税人识别号" = buyer, 2nd = seller
    let mut found_jiashui_label = false; // "价税合计" marker (may be separate from "小写")
    let mut found_xiaoxie_label = false; // "（小写）" after "价税合计"
    let mut found_heji_label = false; // "合计" (non-价税合计) marker

    // Pre-concatenate adjacent single-character texts (some OFDs split CJK labels into
    // individual characters, e.g., "购""买""方""信""息" instead of "购买方信息").
    // We accumulate into a buffer and flush when we see a multi-char text or a label.
    let mut char_buf = String::new();
    let flush_buf = |buf: &mut String| -> Option<String> {
        if buf.len() >= 2 {
            let s = buf.trim().to_string();
            buf.clear();
            Some(s)
        } else {
            buf.clear();
            None
        }
    };

    // We process texts in a two-pass approach:
    // Pass 1: Concatenate single-char sequences into composite labels
    // Pass 2: Apply pattern matching on the composite sequence

    let mut composite_texts: Vec<String> = Vec::new();
    for t in texts {
        let text = t.text.trim();
        if text.is_empty() { continue; }
        let chars: Vec<char> = text.chars().collect();
        if chars.len() == 1 {
            char_buf.push(chars[0]);
        } else {
            // Flush accumulated single chars first
            if let Some(composite) = flush_buf(&mut char_buf) {
                composite_texts.push(composite);
            }
            composite_texts.push(text.to_string());
        }
    }
    // Flush remaining
    if let Some(composite) = flush_buf(&mut char_buf) {
        composite_texts.push(composite);
    }

    // Pass 2: Pattern matching on composite text sequence
    for (i, text) in composite_texts.iter().enumerate() {
        let t = text.as_str();
        if t.is_empty() { continue; }

        // Remove spaces for flexible matching (e.g., "合        计" → "合计")
        let t_nospace: String = t.chars().filter(|c| !c.is_whitespace()).collect();
        let t_nospace_ref = t_nospace.as_str();

        // Detect buyer/seller section boundaries
        if t_nospace_ref.contains("购买方") || t_nospace_ref.contains("买方") {
            section = "buyer";
        }
        if t_nospace_ref.contains("销售方") || t_nospace_ref.contains("卖方") {
            section = "seller";
        }

        // Invoice number
        if (t.contains("发票号码") || t_nospace_ref.contains("发票号码")) && info.invoice_no.is_none() {
            info.invoice_no = extract_composite_value(t, &composite_texts, i, "taxid");
        }

        // Invoice date
        if (t.contains("开票日期") || t_nospace_ref.contains("开票日期")) && info.invoice_date.is_none() {
            info.invoice_date = extract_composite_value(t, &composite_texts, i, "any");
        }

        // Name label — 1st occurrence = buyer, 2nd = seller
        // (Some OFDs have "名称：" as a standalone label before each section's value)
        if (t.contains("名称") || t_nospace_ref.contains("名称"))
            && !t.contains("货物") && !t.contains("劳务") && !t.contains("项目") {
            name_count += 1;
            let value = extract_composite_value(t, &composite_texts, i, "name");
            // If section is still unknown, use occurrence count
            let effective_section = if section.is_empty() {
                if name_count == 1 { "buyer" } else { "seller" }
            } else { section };
            match effective_section {
                "buyer" if info.buyer_name.is_none() => info.buyer_name = value,
                "seller" if info.seller_name.is_none() => info.seller_name = value,
                _ => {}
            }
        }

        // Tax ID — 1st occurrence = buyer, 2nd = seller
        if (t.contains("纳税人识别号") || t.contains("统一社会信用代码"))
            && !t.contains("货物") {
            taxid_count += 1;
            let value = extract_composite_value(t, &composite_texts, i, "taxid");
            let effective_section = if section.is_empty() {
                if taxid_count == 1 { "buyer" } else { "seller" }
            } else { section };
            match effective_section {
                "buyer" if info.buyer_tax_id.is_none() => info.buyer_tax_id = value,
                "seller" if info.seller_tax_id.is_none() => info.seller_tax_id = value,
                _ => {}
            }
        }

        // Amount detection
        // "价税合计" label — may be followed by separate "（小写）" label
        if t_nospace_ref.contains("价税合计") {
            if t_nospace_ref.contains("小写") {
                found_xiaoxie_label = true;
            } else {
                found_jiashui_label = true;
            }
        }
        // "（小写）" or "小写" after "价税合计"
        if (t.contains("小写") || t_nospace_ref.contains("小写")) && found_jiashui_label {
            found_xiaoxie_label = true;
        }
        // "合计" label (not "价税合计") — handle spaced variants like "合        计"
        if (t_nospace_ref.contains("合计") || t_nospace_ref == "合计")
            && !t_nospace_ref.contains("价税") {
            found_heji_label = true;
        }

        // ¥ amount values
        if t.starts_with("¥") || t.starts_with("￥") {
            let amt_str = t.trim_start_matches('¥').trim_start_matches('￥').trim();
            if let Ok(amt) = amt_str.parse::<f64>() {
                if found_xiaoxie_label {
                    // This ¥ is after "价税合计（小写）" → total amount with tax
                    if info.amount_tax.is_none() {
                        info.amount_tax = Some(amt);
                    }
                    found_xiaoxie_label = false;
                    found_jiashui_label = false;
                } else if found_heji_label {
                    // This ¥ is after "合计" → subtotal (no tax or with tax)
                    if info.amount_no_tax.is_none() {
                        info.amount_no_tax = Some(amt);
                    }
                    found_heji_label = false;
                }
            }
        }

    }

    // 发票类型：在合并后的文本序列上判定（连续单字已拼接，拆字票同样可识别）
    info.invoice_type = detect_invoice_type(composite_texts.iter().map(|s| s.as_str()));

    // Compute missing amount fields
    // If we have amount_tax but no breakdown, assume no_tax = amount_tax and tax = 0
    if info.amount_tax.is_some() && info.amount_no_tax.is_none() {
        info.amount_no_tax = info.amount_tax;
        info.tax_amount = Some(0.0);
    }
    // If we have amount_no_tax but no amount_tax, try to compute
    if info.amount_no_tax.is_some() && info.amount_tax.is_none() {
        if let Some(tax) = info.tax_amount {
            info.amount_tax = Some(((info.amount_no_tax.unwrap() + tax) * 100.0).round() / 100.0);
        } else {
            // No tax info → assume amount_no_tax IS the total (tax exempt)
            info.amount_tax = info.amount_no_tax;
            info.tax_amount = Some(0.0);
        }
    }

    info
}

/// Extract a value from a label text or the next text in the composite sequence.
/// First tries to get value after "：" or ":" in the same text.
/// If not found, looks at the next 1-3 texts for a non-label value.
/// `value_kind` hints at the expected format: "name" (CJK chars), "taxid" (alphanumeric), or "any".
fn extract_composite_value(label_text: &str, texts: &[String], label_idx: usize, value_kind: &str) -> Option<String> {
    // Try extracting from same text after colon
    for sep in &["：", ":"] {
        if let Some(pos) = label_text.find(sep) {
            let after = label_text[pos + sep.len()..].trim();
            if !after.is_empty() && value_matches(after, value_kind) {
                return Some(after.to_string());
            }
        }
    }

    // Look at next texts for the value
    for j in (label_idx + 1)..std::cmp::min(label_idx + 5, texts.len()) {
        let next_text = texts[j].trim();
        if !next_text.is_empty() && !is_common_label(next_text) && value_matches(next_text, value_kind) {
            return Some(next_text.to_string());
        }
    }

    None
}

/// Check if a candidate value matches the expected format kind.
fn value_matches(text: &str, kind: &str) -> bool {
    match kind {
        "taxid" => {
            // Tax IDs are alphanumeric (digits + possible X/x suffix), no CJK characters
            text.chars().all(|c| c.is_ascii_alphanumeric())
        }
        "name" => {
            // Names should contain at least one CJK character or be a known format
            text.chars().any(|c| c > '\u{2E80}') // CJK and other East Asian chars
        }
        _ => true,
    }
}

/// Check if a text looks like a common invoice label (not a value)
fn is_common_label(text: &str) -> bool {
    let labels = [
        "发票号码", "开票日期", "名称", "纳税人识别号", "统一社会信用代码",
        "地址", "电话", "开户行", "账号", "购买方", "销售方",
        "价税合计", "合计", "备注", "开票人", "收款人", "复核人",
        "货物", "劳务", "规格型号", "单位", "数量", "单价", "金额",
        "税率", "税额", "项目名称", "小写", "大写",
    ];
    labels.iter().any(|l| text.contains(l))
        || text.ends_with("：") || text.ends_with(":")
}

// =====================================================
// Public API
// =====================================================

// =====================================================
// Diagnostics（诊断导出）— 渲染/识别问题可一键导出结构数据（脱敏）
// =====================================================

/// 诊断导出用文本脱敏：汉字→汉、数字→9、字母→A、空白→·，标点符号原样保留。
/// 报告会经 GitHub issue 公开传递，原文（金额/名称/号码等）绝不能出现在其中；
/// 保留长度与「字符类型序列」是定位排版问题所需的最小信息。
pub fn sanitize_text(s: &str) -> String {
    s.chars()
        .map(|ch| {
            let cp = ch as u32;
            if matches!(cp, 0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF) {
                '汉' // CJK 汉字（基本区 / 扩展A / 兼容区）
            } else if ch.is_numeric() {
                '9' // 含全角数字等各语言数字
            } else if ch.is_alphabetic() {
                'A' // 非 CJK 字母（全角字母、希腊/西里尔等）
            } else if ch.is_whitespace() {
                '·'
            } else {
                ch // 标点、符号原样
            }
        })
        .collect()
}

fn fmt_arr(a: &[f64]) -> String {
    a.iter().map(|v| format!("{:.4}", v)).collect::<Vec<_>>().join(" ")
}

fn push_textcode_line(out: &mut String, x: f64, y: f64, dx: &[f64], dy: &[f64], text: &str) {
    let mut line = format!("    TextCode X={:.4} Y={:.4}", x, y);
    if !dx.is_empty() {
        line.push_str(&format!(" DeltaX=\"[{}个] {}\"", dx.len(), fmt_arr(dx)));
    }
    if !dy.is_empty() {
        line.push_str(&format!(" DeltaY=\"[{}个] {}\"", dy.len(), fmt_arr(dy)));
    }
    line.push_str(&format!(" 文本({}字)=\"{}\"\n", text.chars().count(), sanitize_text(text)));
    out.push_str(&line);
}

/// 诊断导出：把 OFD 各页 Content.xml 的排版结构 dump 为脱敏文本
/// （TextObject 的 Boundary/Font/Size/CTM + 每个 TextCode 的 X/Y/ΔX/ΔY + 脱敏文本）。
/// ΔX/ΔY 数组完整保留——逐字定位错乱正是靠这份数据本地复现定位的。
pub fn dump_ofd_structure(ofd_path: &str) -> Result<String, String> {
    let file = std::fs::File::open(ofd_path).map_err(|e| format!("打开OFD文件失败: {}", e))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("解析OFD ZIP失败: {}", e))?;
    let mut names: Vec<String> = (0..archive.len())
        .filter_map(|i| archive.by_index(i).ok().map(|f| f.name().to_string()))
        .filter(|n| n.ends_with("Content.xml"))
        .collect();
    names.sort();
    let mut out = String::new();
    for name in &names {
        let xml = match zip_read_str(&mut archive, name) {
            Some(x) => x,
            None => continue,
        };
        let (texts, paths, imgs) = parse_ofd_content(&xml);
        out.push_str(&format!("[页面结构] {}\n", name));
        out.push_str(&format!(
            "  对象统计: 文本 {} · 路径 {} · 图像 {}\n",
            texts.len(),
            paths.len(),
            imgs.len()
        ));
        for t in &texts {
            let ctm = t.ctm.map(|m| {
                format!(" CTM=\"{:.4} {:.4} {:.4} {:.4} {:.4} {:.4}\"", m.0, m.1, m.2, m.3, m.4, m.5)
            }).unwrap_or_default();
            out.push_str(&format!(
                "  TextObject#{} Font={} Size={:.4} Boundary=\"{:.4} {:.4} {:.4} {:.4}\" Weight={}{}\n",
                t.id, t.font_id, t.size, t.boundary.0, t.boundary.1, t.boundary.2, t.boundary.3, t.weight, ctm
            ));
            if t.segments.is_empty() {
                push_textcode_line(&mut out, t.text_x, t.text_y, &t.delta_x, &t.delta_y, &t.text);
            } else {
                for seg in &t.segments {
                    push_textcode_line(&mut out, seg.text_x, seg.text_y, &seg.delta_x, &seg.delta_y, &seg.text);
                }
            }
        }
        // 路径/图像对文字错乱诊断价值有限：给前 10 个（ID/Boundary）即可
        for p in paths.iter().take(10) {
            out.push_str(&format!(
                "  PathObject#{} Boundary=\"{:.4} {:.4} {:.4} {:.4}\" LineWidth={:.4}\n",
                p.id, p.boundary.0, p.boundary.1, p.boundary.2, p.boundary.3, p.line_width
            ));
        }
        if paths.len() > 10 {
            out.push_str(&format!("  …其余 {} 个 PathObject 略\n", paths.len() - 10));
        }
        for im in imgs.iter().take(10) {
            out.push_str(&format!(
                "  ImageObject#{} Boundary=\"{:.4} {:.4} {:.4} {:.4}\" Res={}\n",
                im.id, im.boundary.0, im.boundary.1, im.boundary.2, im.boundary.3, im.resource_id
            ));
        }
        if imgs.len() > 10 {
            out.push_str(&format!("  …其余 {} 个 ImageObject 略\n", imgs.len() - 10));
        }
        out.push('\n');
    }
    if out.is_empty() {
        out.push_str("（未找到 Content.xml）\n");
    }
    Ok(out)
}

// =====================================================

/// Parse OFD file: returns SVG vector rendering + structured invoice data from XML.
/// Skips OCR — invoice fields are extracted directly from OFD metadata.
///
/// This is the primary entry point for OFD processing. It:
/// 1. Opens the OFD as a ZIP archive
/// 2. Parses OFD.xml for CustomData (quick invoice fields)
/// 3. Parses Document.xml for page/template structure
/// 4. Parses PublicRes.xml for fonts and DrawParam inheritance
/// 5. Parses DocumentRes.xml for image resources
/// 6. Parses page content (TextObject/PathObject/ImageObject)
/// 7. Parses annotations (watermark layer with Appearance offset handling)
/// 8. Maps CustomTag.xml fields to TextObject IDs for buyer/seller names
/// 9. Generates SVG with 3 layers: template + content + annotations
pub fn parse_ofd_file(ofd_path: &str) -> Result<OfdResult, String> {
    use base64::Engine;

    let file = std::fs::File::open(ofd_path)
        .map_err(|e| format!("打开OFD文件失败: {}", e))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| format!("解析OFD ZIP失败: {}", e))?;

    // 1. Read OFD.xml — root metadata + CustomData
    let ofd_xml = zip_read_str(&mut archive, "OFD.xml")
        .ok_or("OFD.xml 不存在")?;

    // Find DocRoot path (usually Doc_0/Document.xml)
    let doc_root = {
        use quick_xml::events::Event;
        use quick_xml::Reader;
        let mut rdr = Reader::from_str(&ofd_xml);
        rdr.config_mut().trim_text(true);
        let mut b = Vec::new();
        let mut root = String::from("Doc_0/Document.xml");
        loop {
            match rdr.read_event_into(&mut b) {
                Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                    if local_tag_name(&e.name()) == "DocRoot" {
                        let t = read_element_text(&mut rdr);
                        root = t.trim().trim_start_matches('/').to_string();
                        break;
                    }
                }
                Ok(Event::Eof) => break,
                _ => {}
            }
            b.clear();
        }
        root
    };

    // Determine base directory from doc_root (e.g., "Doc_0/Document.xml" → "Doc_0")
    let base_dir = if let Some(pos) = doc_root.rfind('/') {
        doc_root[..pos].to_string()
    } else {
        String::from("Doc_0")
    };

    // 2. Parse CustomData from OFD.xml
    let custom_data = parse_ofd_custom_data(&ofd_xml);

    // 3. Read Document.xml to find template and page content paths
    let doc_xml = zip_read_str(&mut archive, &doc_root)
        .ok_or_else(|| format!("{} 不存在", doc_root))?;

    // Parse Document.xml to get template and page content paths
    let (template_path, page_paths) = {
        use quick_xml::events::Event;
        use quick_xml::Reader;
        let mut rdr = Reader::from_str(&doc_xml);
        rdr.config_mut().trim_text(true);
        let mut b = Vec::new();
        let mut tpl = String::new();
        let mut pages = Vec::new();
        loop {
            match rdr.read_event_into(&mut b) {
                Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                    let tag = local_tag_name(&e.name());
                    if tag == "TemplatePage" {
                        if let Some(v) = attr_val(&e, "BaseLoc") {
                            tpl = format!("{}/{}", base_dir, v);
                        }
                    } else if tag == "Page" {
                        if let Some(v) = attr_val(&e, "BaseLoc") {
                            pages.push(format!("{}/{}", base_dir, v));
                        }
                    }
                }
                Ok(Event::End(_)) => {}
                Ok(Event::Eof) => break,
                _ => {}
            }
            b.clear();
        }
        (tpl, pages)
    };

    // 4. Parse PublicRes.xml for fonts + DrawParam
    let public_res_path = format!("{}/PublicRes.xml", base_dir);
    let (font_map, color_spaces, draw_params) = if let Some(xml) = zip_read_str(&mut archive, &public_res_path) {
        parse_fonts(&xml)
    } else {
        (HashMap::new(), HashMap::new(), HashMap::new())
    };

    // 5. Parse DocumentRes.xml for image resources
    let doc_res_path = format!("{}/DocumentRes.xml", base_dir);
    let image_map = if let Some(xml) = zip_read_str(&mut archive, &doc_res_path) {
        parse_image_resources(&xml)
    } else {
        HashMap::new()
    };

    // Load actual image raw bytes from ZIP (data URL generation deferred until after content parsing
    // so we can apply ImageMask from parsed ImageObjects)
    let mut image_raw_bytes: HashMap<u32, Vec<u8>> = HashMap::new();
    let mut image_file_names: HashMap<u32, String> = HashMap::new();
    for (res_id, file_name) in &image_map {
        let img_path = format!("{}/Res/{}", base_dir, file_name);
        if let Some(bytes) = zip_read_bytes(&mut archive, &img_path) {
            image_raw_bytes.insert(*res_id, bytes);
            image_file_names.insert(*res_id, file_name.clone());
        }
    }

    // 6. Parse template content (background layer)
    let (tpl_texts, tpl_paths, tpl_imgs) = if !template_path.is_empty() {
        if let Some(xml) = zip_read_str(&mut archive, &template_path) {
            let (mut t, mut p, i) = parse_ofd_content(&xml);
            apply_draw_param_defaults(&mut p, &mut t, &draw_params);
            (t, p, i)
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        }
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };

    // 7. Parse page content (data layer)
    // Note: avoid shadowing `page_paths` (Vec<String> from Document.xml parsing)
    // Page content Layer has no DrawParam → OFD default: black (0,0,0). Do NOT apply
    // any DrawParam inheritance — invoice data text and ¥ symbol are naturally black.
    let (page_texts, page_obj_paths, page_imgs) = if let Some(page_path) = page_paths.first() {
        if let Some(xml) = zip_read_str(&mut archive, page_path) {
            parse_ofd_content(&xml)
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        }
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };

    // 8. Parse annotations (watermark layer) — uses parse_annotations to handle Appearance offsets
    let annots_path = format!("{}/Annots/Page_0/Annotation.xml", base_dir);
    let (annot_texts, annot_imgs) = if let Some(xml) = zip_read_str(&mut archive, &annots_path) {
        parse_annotations(&xml)
    } else {
        (Vec::new(), Vec::new())
    };

    // 5b. Generate image data URLs, applying ImageMask compositing where needed
    // Collect all ImageObjects with ImageMask from template + page + annotations
    let mut mask_map: HashMap<u32, u32> = HashMap::new(); // resource_id → mask_resource_id
    {
        let collect_masks = |imgs: &[OfdImageObject], map: &mut HashMap<u32, u32>| {
            for img in imgs {
                if let Some(mask_id) = img.image_mask {
                    map.insert(img.resource_id, mask_id);
                }
            }
        };
        collect_masks(&tpl_imgs, &mut mask_map);
        collect_masks(&page_imgs, &mut mask_map);
        collect_masks(&annot_imgs, &mut mask_map);
    }

    let mut image_data: HashMap<u32, String> = HashMap::new();
    // Pixel dimensions per resource — used to compute display size from CTM density
    // when an ImageObject's Boundary is a full-page placeholder (数电票 qrcode etc.)
    let mut image_sizes: HashMap<u32, (u32, u32)> = HashMap::new();
    for (res_id, bytes) in &image_raw_bytes {
        if let Ok(decoded) = image::load_from_memory(bytes) {
            image_sizes.insert(*res_id, decoded.dimensions());
        }
        if let Some(&mask_res_id) = mask_map.get(res_id) {
            // Composite: decode main image + mask, merge alpha channel, encode as RGBA PNG
            if let Some(mask_bytes) = image_raw_bytes.get(&mask_res_id) {
                if let Ok(main_img) = image::load_from_memory(bytes) {
                    if let Ok(mask_img) = image::load_from_memory(mask_bytes) {
                        let main_rgba = main_img.to_rgba8();
                        let mask_rgba = mask_img.to_rgba8();
                        // Both images must match dimensions
                        if main_rgba.width() == mask_rgba.width() && main_rgba.height() == mask_rgba.height() {
                            let mut composited = main_rgba.clone();
                            for (pixel, mask_pixel) in composited.pixels_mut().zip(mask_rgba.pixels()) {
                                // Mask: white (255) = opaque, black (0) = transparent
                                // Use the red channel of the mask as alpha
                                pixel[3] = mask_pixel[0];
                            }
                            let mut png_buf = Vec::new();
                            use std::io::Cursor;
                            if composited.write_to(&mut Cursor::new(&mut png_buf), image::ImageFormat::Png).is_ok() {
                                let b64 = base64::engine::general_purpose::STANDARD.encode(&png_buf);
                                image_data.insert(*res_id, format!("data:image/png;base64,{}", b64));
                                log::info!("ImageMask applied: resource {} masked by {}", res_id, mask_res_id);
                                continue;
                            }
                        } else {
                            log::warn!("ImageMask dimension mismatch: main={}x{}, mask={}x{}, skipping mask",
                                main_rgba.width(), main_rgba.height(), mask_rgba.width(), mask_rgba.height());
                        }
                    }
                }
            }
            // Fallback: if mask compositing failed, use the main image as-is
            log::warn!("ImageMask compositing failed for resource {}, using unmasked image", res_id);
        }
        // Default: encode as-is
        let file_name = image_file_names.get(res_id).map(|s| s.as_str()).unwrap_or("");
        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        let mime = if file_name.to_lowercase().ends_with(".png") || file_name.to_lowercase().ends_with(".bmp") {
            "image/png" // BMP decoded → re-encode as PNG for browser compatibility
        } else {
            "image/jpeg"
        };
        image_data.insert(*res_id, format!("data:{};base64,{}", mime, b64));
    }

    // 9. Get page dimensions
    let (page_w, page_h) = if let Some(page_path) = page_paths.first() {
        if let Some(xml) = zip_read_str(&mut archive, page_path) {
            // Parse PhysicalBox from the page XML
            use quick_xml::events::Event;
            use quick_xml::Reader;
            let mut rdr = Reader::from_str(&xml);
            rdr.config_mut().trim_text(true);
            let mut b = Vec::new();
            let mut dims = (210.0f64, 140.0f64);
            loop {
                match rdr.read_event_into(&mut b) {
                    Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                        if local_tag_name(&e.name()) == "PhysicalBox" {
                            let text = read_element_text(&mut rdr);
                            if let Some((_, _, w, h)) = parse_f4(text.trim()) {
                                dims = (w, h);
                            }
                            break;
                        }
                    }
                    Ok(Event::Eof) => break,
                    _ => {}
                }
                b.clear();
            }
            dims
        } else {
            (210.0, 140.0)
        }
    } else {
        (210.0, 140.0)
    };

    // 10. Parse CustomTag.xml for semantic field mapping
    let custom_tag_path = format!("{}/Tags/CustomTag.xml", base_dir);
    let tag_map = if let Some(xml) = zip_read_str(&mut archive, &custom_tag_path) {
        parse_custom_tag(&xml)
    } else {
        HashMap::new()
    };

    // 11. Extract invoice info from structured data
    let mut invoice_info = OfdInvoiceInfo::default();

    // From OFD.xml CustomData — skip empty strings (empty self-closing tags)
    let get_custom = |key: &str| -> Option<String> {
        custom_data.get(key).and_then(|s| if s.trim().is_empty() { None } else { Some(s.clone()) })
    };
    invoice_info.invoice_no = get_custom("发票号码");
    invoice_info.invoice_date = get_custom("开票日期");
    invoice_info.buyer_tax_id = get_custom("购买方纳税人识别号");
    invoice_info.seller_tax_id = get_custom("销售方纳税人识别号");
    invoice_info.amount_no_tax = custom_data.get("合计金额").and_then(|s| s.parse().ok());
    invoice_info.tax_amount = custom_data.get("合计税额").and_then(|s| s.parse().ok());

    // Compute total = no_tax + tax (both already in yuan, e.g. 17699.12 + 2300.88 = 20000.00)
    if let (Some(no_tax), Some(tax)) = (invoice_info.amount_no_tax, invoice_info.tax_amount) {
        invoice_info.amount_tax = Some(((no_tax + tax) * 100.0).round() / 100.0);
    }

    // From CustomTag.xml + Content.xml — get buyer/seller names
    // Build a text lookup: TextObject ID → text content
    let mut text_lookup: HashMap<u32, &str> = HashMap::new();
    for t in &page_texts {
        text_lookup.insert(t.id, &t.text);
    }

    // Map tag fields to text content
    let get_tag_text = |field: &str| -> Option<String> {
        tag_map.get(field).and_then(|ids| {
            ids.iter().filter_map(|id| text_lookup.get(id)).map(|s| s.to_string()).collect::<Vec<_>>().into_iter().next()
        })
    };

    if invoice_info.invoice_no.is_none() {
        invoice_info.invoice_no = get_tag_text("InvoiceNo");
    }
    if invoice_info.invoice_date.is_none() {
        invoice_info.invoice_date = get_tag_text("IssueDate");
    }
    if invoice_info.buyer_name.is_none() {
        invoice_info.buyer_name = get_tag_text("BuyerName");
    }
    if invoice_info.seller_name.is_none() {
        invoice_info.seller_name = get_tag_text("SellerName");
    }
    if invoice_info.buyer_tax_id.is_none() {
        invoice_info.buyer_tax_id = get_tag_text("BuyerTaxID");
    }
    if invoice_info.seller_tax_id.is_none() {
        invoice_info.seller_tax_id = get_tag_text("SellerTaxID");
    }

    // 11a. 数电票半结构化提取（无 Template/CustomData/CustomTag 的双层 Content.xml，
    // 数据层 TextCode 为页面绝对坐标，如通行费电子发票）。仅填充仍为 None 的字段。
    let body_info = extract_invoice_from_body_coords(&page_texts, page_w, page_h);
    if invoice_info.invoice_no.is_none() { invoice_info.invoice_no = body_info.invoice_no; }
    if invoice_info.invoice_date.is_none() { invoice_info.invoice_date = body_info.invoice_date; }
    if invoice_info.buyer_name.is_none() { invoice_info.buyer_name = body_info.buyer_name; }
    if invoice_info.buyer_tax_id.is_none() { invoice_info.buyer_tax_id = body_info.buyer_tax_id; }
    if invoice_info.seller_name.is_none() { invoice_info.seller_name = body_info.seller_name; }
    if invoice_info.seller_tax_id.is_none() { invoice_info.seller_tax_id = body_info.seller_tax_id; }
    if invoice_info.amount_no_tax.is_none() { invoice_info.amount_no_tax = body_info.amount_no_tax; }
    if invoice_info.tax_amount.is_none() { invoice_info.tax_amount = body_info.tax_amount; }
    if invoice_info.amount_tax.is_none() { invoice_info.amount_tax = body_info.amount_tax; }

    // 通行费标记：模板层或数据层文本含「通行费」
    if invoice_info.is_toll.is_none() {
        let has_toll_text = tpl_texts.iter().chain(page_texts.iter())
            .any(|t| t.text.contains("通行费"));
        invoice_info.is_toll = Some(has_toll_text);
    }

    // 发票类型：模板层 + 数据层文本合并判定（「普通」优先；逐条未命中时用拼接串兜底拆字票）
    let mut type_texts: Vec<&str> = tpl_texts.iter().chain(page_texts.iter())
        .map(|t| t.text.as_str()).collect();
    let type_text_merged: String = type_texts.concat();
    type_texts.push(type_text_merged.as_str());
    invoice_info.invoice_type = detect_invoice_type(type_texts);

    // 11b. Text-based fallback extraction when no CustomData or CustomTag
    // This handles OFD files from non-tax producers (e.g., dzcp) that embed fonts
    // but don't include structured metadata.
    // Run when any key field is still missing (11a may only fill part of them).
    if invoice_info.invoice_no.is_none() || invoice_info.invoice_date.is_none()
        || invoice_info.buyer_name.is_none() || invoice_info.seller_name.is_none() {
        // Combine template + page texts (preserving order by ID)
        let mut all_texts: Vec<&OfdTextObject> = Vec::new();
        all_texts.extend(&tpl_texts);
        all_texts.extend(&page_texts);
        all_texts.sort_by_key(|t| t.id);

        let extracted = extract_invoice_from_text(&all_texts);

        // Only fill fields that are still None
        if invoice_info.invoice_no.is_none() { invoice_info.invoice_no = extracted.invoice_no; }
        if invoice_info.invoice_date.is_none() { invoice_info.invoice_date = extracted.invoice_date; }
        if invoice_info.buyer_name.is_none() { invoice_info.buyer_name = extracted.buyer_name; }
        if invoice_info.buyer_tax_id.is_none() { invoice_info.buyer_tax_id = extracted.buyer_tax_id; }
        if invoice_info.seller_name.is_none() { invoice_info.seller_name = extracted.seller_name; }
        if invoice_info.seller_tax_id.is_none() { invoice_info.seller_tax_id = extracted.seller_tax_id; }
        if invoice_info.amount_no_tax.is_none() { invoice_info.amount_no_tax = extracted.amount_no_tax; }
        if invoice_info.tax_amount.is_none() { invoice_info.tax_amount = extracted.tax_amount; }
        if invoice_info.amount_tax.is_none() { invoice_info.amount_tax = extracted.amount_tax; }
        if invoice_info.invoice_type.is_none() { invoice_info.invoice_type = extracted.invoice_type; }
    }

    // 12. Build SVG
    let svg = build_ofd_svg(
        page_w, page_h,
        &tpl_texts, &tpl_paths, &tpl_imgs,
        &page_texts, &page_obj_paths, &page_imgs,
        &annot_texts, &annot_imgs,
        &font_map, &color_spaces, &image_data, &image_sizes,
    );

    log::info!("OFD parsed: {}x{}mm, {} template texts, {} page texts, {} paths",
        page_w, page_h, tpl_texts.len(), page_texts.len(), tpl_paths.len() + page_obj_paths.len());

    Ok(OfdResult {
        svg,
        invoice_info,
        page_width: page_w,
        page_height: page_h,
    })
}

/// Extract OFD page images as structured data (for bitmap fallback).
/// Returns `OfdExtractedImage` with base64 data URLs, dimensions, and file extension.
/// The caller can convert these to whatever type they need (e.g., FileData).
pub fn extract_ofd_images_raw(ofd_path: &str) -> Result<Vec<OfdExtractedImage>, String> {
    let images = extract_ofd_images(ofd_path)?;
    Ok(images.into_iter().map(|(data_url, ext, w, h)| OfdExtractedImage {
        data_url,
        ext,
        width: w,
        height: h,
    }).collect())
}

// =====================================================
// XML 数电票 Parsing (standalone .xml files)
// =====================================================

/// Parse a standalone XML 数电票 file and extract structured invoice data.
///
/// The XML format follows the 国家税务总局《电子凭证会计数据标准》specification,
/// with root element `<EInvoice>`. This is a pure data format with no layout info —
/// it cannot be rendered as a visual invoice page.
///
/// Returns `XmlInvoiceInfo` with key fields for file list display, summary export, etc.
pub fn parse_xml_invoice(xml_path: &str) -> Result<XmlInvoiceInfo, String> {
    let content = std::fs::read_to_string(xml_path)
        .map_err(|e| format!("读取 XML 文件失败: {}", e))?;

    // Quick check: must contain <EInvoice> root element
    if !content.contains("<EInvoice") {
        return Err("不是有效的数电票 XML 文件（缺少 EInvoice 根元素）".to_string());
    }

    let info = parse_xml_invoice_content(&content)?;
    Ok(info)
}

/// Parse XML 数电票 content string and extract structured invoice data.
fn parse_xml_invoice_content(content: &str) -> Result<XmlInvoiceInfo, String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);

    let mut info = XmlInvoiceInfo::default();
    let mut buf = Vec::new();

    // Track element path for context-aware parsing
    let mut path: Vec<String> = Vec::new();
    // Track LabelName values from EInvoiceType and GeneralOrSpecialVAT
    let mut einvoice_type_label: Option<String> = None;
    let mut general_or_special_label: Option<String> = None;
    // LabelCode 兜底（部分结构变体只有码值没有名称）
    let mut general_or_special_code: Option<String> = None;
    // Item names (IssuItemInformation) — used for toll detection
    let mut item_names: Vec<String> = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let local = e.local_name();
                let name_str = String::from_utf8_lossy(local.as_ref()).to_string();
                path.push(name_str);
            }
            Ok(Event::End(ref _e)) => {
                path.pop();
            }
            Ok(Event::Empty(ref _e)) => {
                // Self-closing tags like <SpecificInformation/> — nothing to extract
            }
            Ok(Event::Text(ref e)) => {
                if let Ok(text) = e.unescape() {
                    let text = text.trim();
                    if text.is_empty() { continue; }

                    let current_tag = path.last().map(|s| s.as_str()).unwrap_or("");
                    // Check parent context for LabelName disambiguation
                    let parent_tag = if path.len() >= 2 {
                        path.get(path.len() - 2).map(|s| s.as_str()).unwrap_or("")
                    } else {
                        ""
                    };

                    match current_tag {
                        // TaxSupervisionInfo
                        "InvoiceNumber" => info.invoice_no = Some(text.to_string()),
                        "IssueTime" => {
                            info.invoice_date = Some(text.split('T').next().unwrap_or(text).to_string());
                        }
                        // Seller — skip empty values (e.g. personal invoices)
                        "SellerName" => info.seller_name = Some(text.to_string()),
                        "SellerIdNum" if !text.is_empty() => info.seller_tax_id = Some(text.to_string()),
                        // Buyer — skip empty values (e.g. personal invoices where BuyerIdNum is empty)
                        "BuyerName" => info.buyer_name = Some(text.to_string()),
                        "BuyerIdNum" if !text.is_empty() => info.buyer_tax_id = Some(text.to_string()),
                        // BasicInformation amounts
                        "TotalAmWithoutTax" => info.amount_no_tax = text.parse().ok(),
                        "TotalTaxAm" => info.tax_amount = text.parse().ok(),
                        // TotalTax-includedAmount: tag name contains hyphen, quick-xml preserves it
                        "TotalTax-includedAmount" => info.amount_tax = text.parse().ok(),
                        // Invoice type: collect LabelName from different parent contexts
                        "LabelName" => match parent_tag {
                            "EInvoiceType" if einvoice_type_label.is_none() => {
                                einvoice_type_label = Some(text.to_string());
                            }
                            "GeneralOrSpecialVAT" if general_or_special_label.is_none() => {
                                general_or_special_label = Some(text.to_string());
                            }
                            _ => {}
                        },
                        // LabelCode 兜底：标准码表 01=增值税专用发票、02=普通发票
                        "LabelCode" => match parent_tag {
                            "GeneralOrSpecialVAT" if general_or_special_code.is_none() => {
                                general_or_special_code = Some(text.to_string());
                            }
                            _ => {}
                        },
                        // Item names — used for toll detection (通行费)
                        "ItemName" => item_names.push(text.to_string()),
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML 解析错误: {}", e)),
            _ => {}
        }
        buf.clear();
    }

    // Compose invoice_type from EInvoiceType + GeneralOrSpecialVAT labels
    // e.g. "电子发票" + "普通发票" → "电子发票(普通发票)"
    // e.g. "电子发票" + "增值税专用发票" → "电子发票(增值税专用发票)"
    // LabelName 缺失时按 LabelCode 兜底（01=增值税专用发票，02=普通发票）
    let special_label = general_or_special_label.clone().or_else(|| {
        match general_or_special_code.as_deref() {
            Some("01") => Some("增值税专用发票".to_string()),
            Some("02") => Some("普通发票".to_string()),
            _ => None,
        }
    });
    if let Some(special_label) = special_label {
        let prefix = einvoice_type_label.as_deref().unwrap_or("电子发票");
        info.invoice_type = Some(format!("{}({})", prefix, special_label));
    } else if let Some(type_label) = &einvoice_type_label {
        info.invoice_type = Some(type_label.clone());
    }

    // Toll detection: item name contains 通行费 (e.g. "*生产生活服务*通行费")
    info.is_toll = Some(item_names.iter().any(|s| s.contains("通行费")));

    // Fallback: if amount_tax still empty, try alternate tag name
    if info.amount_tax.is_none() {
        // Some XML variants may use TotalTaxIncludedAmount instead of TotalTax-includedAmount
        let alt = content.find("<TotalTaxIncludedAmount>")
            .and_then(|start| {
                let text_start = start + "<TotalTaxIncludedAmount>".len();
                content[text_start..].find("</TotalTaxIncludedAmount>")
                    .map(|end| content[text_start..text_start + end].trim())
            });
        if let Some(v) = alt {
            info.amount_tax = v.parse().ok();
        }
    }

    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_xml_general_invoice_personal() {
        // 普通发票 - 个人购买方 (BuyerIdNum 为空)
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<EInvoice>
  <Header>
    <InherentLabel>
      <EInvoiceType><LabelCode>01</LabelCode><LabelName>电子发票</LabelName></EInvoiceType>
      <GeneralOrSpecialVAT><LabelCode>02</LabelCode><LabelName>普通发票</LabelName></GeneralOrSpecialVAT>
    </InherentLabel>
  </Header>
  <EInvoiceData>
    <SellerInformation>
      <SellerIdNum>913416007050059877</SellerIdNum>
      <SellerName>中国联合网络通信有限公司亳州市分公司</SellerName>
    </SellerInformation>
    <BuyerInformation>
      <BuyerIdNum></BuyerIdNum>
      <BuyerName>高宗林（个人）</BuyerName>
    </BuyerInformation>
    <BasicInformation>
      <TotalAmWithoutTax>19.00</TotalAmWithoutTax>
      <TotalTaxAm>0.00</TotalTaxAm>
      <TotalTax-includedAmount>19.00</TotalTax-includedAmount>
    </BasicInformation>
  </EInvoiceData>
  <TaxSupervisionInfo>
    <InvoiceNumber>26347000000117553300</InvoiceNumber>
    <IssueTime>2026-05-05</IssueTime>
  </TaxSupervisionInfo>
</EInvoice>"#;

        let info = parse_xml_invoice_content(xml).unwrap();
        assert_eq!(info.invoice_no.as_deref(), Some("26347000000117553300"));
        assert_eq!(info.invoice_date.as_deref(), Some("2026-05-05"));
        assert_eq!(info.seller_name.as_deref(), Some("中国联合网络通信有限公司亳州市分公司"));
        assert_eq!(info.seller_tax_id.as_deref(), Some("913416007050059877"));
        assert_eq!(info.buyer_name.as_deref(), Some("高宗林（个人）"));
        assert_eq!(info.buyer_tax_id, None, "个人发票 BuyerIdNum 为空应为 None");
        assert_eq!(info.amount_no_tax, Some(19.00));
        assert_eq!(info.tax_amount, Some(0.00));
        assert_eq!(info.amount_tax, Some(19.00));
        assert_eq!(info.invoice_type.as_deref(), Some("电子发票(普通发票)"));
    }

    #[test]
    fn test_parse_xml_special_vat_invoice() {
        // 增值税专用发票
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<EInvoice>
  <Header>
    <InherentLabel>
      <EInvoiceType><LabelCode>01</LabelCode><LabelName>电子发票</LabelName></EInvoiceType>
      <GeneralOrSpecialVAT><LabelCode>01</LabelCode><LabelName>增值税专用发票</LabelName></GeneralOrSpecialVAT>
    </InherentLabel>
  </Header>
  <EInvoiceData>
    <SellerInformation>
      <SellerIdNum>91320106751253359F</SellerIdNum>
      <SellerName>安元科技股份有限公司</SellerName>
    </SellerInformation>
    <BuyerInformation>
      <BuyerIdNum>9132020013590404XW</BuyerIdNum>
      <BuyerName>江苏苏豪天鹏农产品集团有限公司</BuyerName>
    </BuyerInformation>
    <BasicInformation>
      <TotalAmWithoutTax>18876.44</TotalAmWithoutTax>
      <TotalTaxAm>1793.56</TotalTaxAm>
      <TotalTax-includedAmount>20670.00</TotalTax-includedAmount>
    </BasicInformation>
  </EInvoiceData>
  <TaxSupervisionInfo>
    <InvoiceNumber>26322000004478296111</InvoiceNumber>
    <IssueTime>2026-06-04</IssueTime>
  </TaxSupervisionInfo>
</EInvoice>"#;

        let info = parse_xml_invoice_content(xml).unwrap();
        assert_eq!(info.invoice_no.as_deref(), Some("26322000004478296111"));
        assert_eq!(info.seller_name.as_deref(), Some("安元科技股份有限公司"));
        assert_eq!(info.buyer_tax_id.as_deref(), Some("9132020013590404XW"));
        assert_eq!(info.amount_no_tax, Some(18876.44));
        assert_eq!(info.tax_amount, Some(1793.56));
        assert_eq!(info.amount_tax, Some(20670.00));
        assert_eq!(info.invoice_type.as_deref(), Some("电子发票(增值税专用发票)"));
    }

    #[test]
    fn test_parse_xml_general_invoice_company() {
        // 普通发票 - 企业购买方
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<EInvoice>
  <Header>
    <InherentLabel>
      <EInvoiceType><LabelCode>01</LabelCode><LabelName>电子发票</LabelName></EInvoiceType>
      <GeneralOrSpecialVAT><LabelCode>02</LabelCode><LabelName>普通发票</LabelName></GeneralOrSpecialVAT>
    </InherentLabel>
  </Header>
  <EInvoiceData>
    <SellerInformation>
      <SellerIdNum>52320200509244470T</SellerIdNum>
      <SellerName>无锡市安协安全培训中心</SellerName>
    </SellerInformation>
    <BuyerInformation>
      <BuyerIdNum>9132020013590404XW</BuyerIdNum>
      <BuyerName>江苏苏豪天鹏农产品集团有限公司</BuyerName>
    </BuyerInformation>
    <BasicInformation>
      <TotalAmWithoutTax>235.85</TotalAmWithoutTax>
      <TotalTaxAm>14.15</TotalTaxAm>
      <TotalTax-includedAmount>250.00</TotalTax-includedAmount>
    </BasicInformation>
  </EInvoiceData>
  <TaxSupervisionInfo>
    <InvoiceNumber>25322000000365404822</InvoiceNumber>
    <IssueTime>2025-08-08</IssueTime>
  </TaxSupervisionInfo>
</EInvoice>"#;

        let info = parse_xml_invoice_content(xml).unwrap();
        assert_eq!(info.invoice_no.as_deref(), Some("25322000000365404822"));
        assert_eq!(info.buyer_name.as_deref(), Some("江苏苏豪天鹏农产品集团有限公司"));
        assert_eq!(info.buyer_tax_id.as_deref(), Some("9132020013590404XW"));
        assert_eq!(info.amount_tax, Some(250.00));
        assert_eq!(info.invoice_type.as_deref(), Some("电子发票(普通发票)"));
    }

    #[test]
    fn test_parse_xml_not_einvoice() {
        let result = parse_xml_invoice_content("<root>not an invoice</root>");
        // parse_xml_invoice_content doesn't validate root element; that's done by parse_xml_invoice
        assert!(result.is_ok());
        let info = result.unwrap();
        assert_eq!(info.invoice_no, None);
    }

    #[test]
    fn test_parse_xml_issue_time_with_t() {
        // IssueTime may include time portion with T separator
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<EInvoice>
  <EInvoiceData>
    <BasicInformation>
      <TotalAmWithoutTax>100</TotalAmWithoutTax>
      <TotalTaxAm>6</TotalTaxAm>
      <TotalTax-includedAmount>106</TotalTax-includedAmount>
    </BasicInformation>
  </EInvoiceData>
  <TaxSupervisionInfo>
    <InvoiceNumber>12345</InvoiceNumber>
    <IssueTime>2026-01-15T10:30:00</IssueTime>
  </TaxSupervisionInfo>
</EInvoice>"#;

        let info = parse_xml_invoice_content(xml).unwrap();
        assert_eq!(info.invoice_date.as_deref(), Some("2026-01-15"));
    }

    #[test]
    fn test_parse_xml_label_code_fallback() {
        // GeneralOrSpecialVAT 只有码值没有名称时按码表兜底：01=专票、02=普票
        let special = r#"<EInvoice><Header><InherentLabel>
            <EInvoiceType><LabelCode>01</LabelCode><LabelName>电子发票</LabelName></EInvoiceType>
            <GeneralOrSpecialVAT><LabelCode>01</LabelCode></GeneralOrSpecialVAT>
        </InherentLabel></Header><EInvoiceData/></EInvoice>"#;
        assert_eq!(
            parse_xml_invoice_content(special).unwrap().invoice_type.as_deref(),
            Some("电子发票(增值税专用发票)")
        );

        let general = r#"<EInvoice><Header><InherentLabel>
            <EInvoiceType><LabelCode>01</LabelCode><LabelName>电子发票</LabelName></EInvoiceType>
            <GeneralOrSpecialVAT><LabelCode>02</LabelCode></GeneralOrSpecialVAT>
        </InherentLabel></Header><EInvoiceData/></EInvoice>"#;
        assert_eq!(
            parse_xml_invoice_content(general).unwrap().invoice_type.as_deref(),
            Some("电子发票(普通发票)")
        );
    }

    #[test]
    fn test_parse_xml_label_name_wins_over_code() {
        // 名称与码值同时存在时以名称为准
        let xml = r#"<EInvoice><Header><InherentLabel>
            <EInvoiceType><LabelCode>01</LabelCode><LabelName>电子发票</LabelName></EInvoiceType>
            <GeneralOrSpecialVAT><LabelCode>01</LabelCode><LabelName>增值税普通发票</LabelName></GeneralOrSpecialVAT>
        </InherentLabel></Header><EInvoiceData/></EInvoice>"#;
        assert_eq!(
            parse_xml_invoice_content(xml).unwrap().invoice_type.as_deref(),
            Some("电子发票(增值税普通发票)")
        );
    }

    #[test]
    fn test_detect_invoice_type_variants() {
        let detect = |texts: Vec<&str>| detect_invoice_type(texts).unwrap_or_default();
        // 数电票标题
        assert_eq!(detect(vec!["电子发票（普通发票）"]), "增值税普通发票");
        assert_eq!(detect(vec!["电子发票（增值税专用发票）"]), "增值税专用发票");
        // 老式纸票标题
        assert_eq!(detect(vec!["江苏增值税普通发票"]), "增值税普通发票");
        assert_eq!(detect(vec!["江苏增值税专用发票"]), "增值税专用发票");
        // 无专普信息时退化为「电子发票」
        assert_eq!(detect(vec!["电子发票"]), "电子发票");
        // 无任何类型信息
        assert_eq!(detect(vec!["某某公司", "价税合计"]), "");
    }

    #[test]
    fn test_detect_invoice_type_general_wins() {
        // 票面其它位置出现「专用」字样时，仍按「普通」判定（普票优先）
        let got = detect_invoice_type(vec!["电子发票（普通发票）", "备注：本票非增值税专用发票"]);
        assert_eq!(got.as_deref(), Some("增值税普通发票"));
    }

    #[test]
    fn test_detect_invoice_type_no_cross_item_match() {
        // 逐条匹配：跨文本拼接出的「增值税专用」不算命中
        assert_eq!(detect_invoice_type(vec!["增值税", "专用"]), None);
        // 连续单字拼接后的整串可识别（OFD/PDF 拆字场景由调用方先合并）
        assert_eq!(
            detect_invoice_type(vec!["增值税专用发票"]).as_deref(),
            Some("增值税专用发票")
        );
    }

    #[test]
    fn test_ofd_text_escape_decode() {
        // "\XXXX" 转义解码（GB/T 33190：空格等需转义）
        assert_eq!(decode_ofd_escapes("A\\0020B"), "A B");
        assert_eq!(decode_ofd_escapes("\\4E2D\\6587"), "中文");
        assert_eq!(decode_ofd_escapes("no escape"), "no escape");
        // 非十六进制内容原样保留
        assert_eq!(decode_ofd_escapes("bad\\00ZZ"), "bad\\00ZZ");
    }

    /// 构造 OFD 文本对象（其余字段取默认），供逐字定位判定测试用
    fn ofd_text(
        text: &str,
        boundary_w: f64,
        size: f64,
        dx: Vec<f64>,
        ctm: Option<(f64, f64, f64, f64, f64, f64)>,
    ) -> OfdTextObject {
        OfdTextObject {
            boundary: (0.0, 0.0, boundary_w, size),
            size,
            text: text.to_string(),
            delta_x: dx,
            ctm,
            ..Default::default()
        }
    }

    #[test]
    fn test_ofd_deltax_space_included_by_boundary() {
        // 「单··位」（移动话费票样）：4 字符（含 2 空格）配 3 个 ΔX。
        // 含空格口径：6.35 + 末字宽 3.175 = 9.525 = Boundary ✓；
        // 去空格口径残差 6.35（2 倍）→ Boundary 校验选含空格序列 → 4 个 tspan
        let t = ofd_text(
            "单  位", 9.525, 3.175, vec![3.175, 1.5875, 1.5875],
            Some((1.0, 0.0, 0.0, 1.0, 0.0, 0.0)),
        );
        let svg = build_svg_text(&t, &HashMap::new(), &HashMap::new(), 1.0, 1.0);
        assert_eq!(svg.matches("<tspan").count(), 4);
    }

    #[test]
    fn test_ofd_deltax_single_space_ambiguous_by_boundary() {
        // 唯一空格歧义案例「缴费时间：2026-03-26 14:33:36」：ΔX 个数（23）同时匹配
        // 两种口径，长度拟合会误选去空格 → 半字宽错位；Boundary 校验
        // （44.45 vs 46.0375，差 1.5875 一个数字宽）应选含空格口径 → 24 个 tspan
        let mut dx = vec![3.175; 5];
        dx.extend(std::iter::repeat(1.5875).take(18));
        let t = ofd_text(
            "缴费时间：2026-03-26 14:33:36", 46.0375, 3.175, dx,
            Some((1.0, 0.0, 0.0, 1.0, 0.0, 0.0)),
        );
        let svg = build_svg_text(&t, &HashMap::new(), &HashMap::new(), 1.0, 1.0);
        assert_eq!(svg.matches("<tspan").count(), 24);
    }

    #[test]
    fn test_ofd_deltax_included_with_scaled_ctm() {
        // issue #47 场景：单空格 + 含空格口径 ΔX + CTM 含缩放（不动产证）。
        // 表46 规定 ΔX/Boundary 同处对象坐标系 → CTM 缩放不影响 Boundary 自校验；
        // 恰好一个空格时长度拟合无区分力（两种口径条数都对得上），只有本校验能救回来。
        let mut dx = vec![3.175; 5];
        dx.extend(std::iter::repeat(1.5875).take(18));
        let t = ofd_text(
            "缴费时间：2026-03-26 14:33:36", 46.0375, 3.175, dx,
            Some((0.2367, 0.0, 0.0, 0.2367, 0.0, 0.0)),
        );
        let svg = build_svg_text(&t, &HashMap::new(), &HashMap::new(), 1.0, 1.0);
        assert_eq!(svg.matches("<tspan").count(), 24, "含空格口径：空格占一个 ΔX 槽位");
    }

    #[test]
    fn test_ofd_deltax_inconsistent_array_falls_back_to_length() {
        // 「车牌号…」式表头样本：ΔX（27 条 ×12）与 Boundary（139.403）自身不自洽，
        // 两种口径误差都很大（184.6 vs 172.6）→ 判定不生效、退回长度拟合
        // （ΔX 条数同时匹配两种口径 → 保持去空格口径，27 个 tspan）
        let t = ofd_text(
            "车牌号车辆类型 通行日期起通行日期止金额税率/征收率税额",
            139.403, 12.0, vec![12.0; 27],
            Some((0.2367, 0.0, 0.0, 0.2367, 0.0, 0.0)),
        );
        let svg = build_svg_text(&t, &HashMap::new(), &HashMap::new(), 1.0, 1.0);
        assert_eq!(svg.matches("<tspan").count(), 27);
    }

    #[test]
    fn test_ofd_placeholder_not_rendered() {
        // ¤ 占位符（U+00A4）占 ΔX 槽位但不渲染字形（issue #44 叠字来源）
        let t = ofd_text("1月\u{A4}", 6.35, 3.175, vec![3.175, 3.175], None);
        let svg = build_svg_text(&t, &HashMap::new(), &HashMap::new(), 1.0, 1.0);
        assert!(!svg.contains('\u{A4}'));
        assert!(svg.contains(">月</tspan>"));
        assert_eq!(svg.matches("<tspan").count(), 3);
    }

    #[test]
    fn test_ofd_placeholder_and_pua_not_rendered() {
        // 占位符只认国标 ¤（表45）与 PUA 字形码位（系统字体渲染必乱码）：只占 ΔX 槽位、不渲染字形。
        // Ø/∅/Φ 必须照常渲染——issue #47 的「月Ø」已查明是 ΔX 错位挤过去的数字 0，不是占位符
        for ch in ['\u{A4}', '\u{E123}', '\u{F0001}'] {
            let t = ofd_text(&format!("1月{ch}"), 6.35, 3.175, vec![3.175, 3.175], None);
            let svg = build_svg_text(&t, &HashMap::new(), &HashMap::new(), 1.0, 1.0);
            assert!(!svg.contains(ch), "占位符 {ch:?} 不应渲染字形: {svg}");
            assert_eq!(svg.matches("<tspan").count(), 3, "占位符仍占 ΔX 槽位: {svg}");
        }
        for ch in ['\u{D8}', '\u{2205}', '\u{3A6}'] {
            let t = ofd_text(&format!("1月{ch}"), 6.35, 3.175, vec![3.175, 3.175], None);
            let svg = build_svg_text(&t, &HashMap::new(), &HashMap::new(), 1.0, 1.0);
            assert!(svg.contains(ch), "圆圈类符号是正文，不能当占位符滤掉: {ch:?} {svg}");
        }
    }

    #[test]
    fn test_ofd_multi_textcode_renders_per_segment() {
        // 国标 11.3：一个 TextObject 可含 1..N 个 TextCode（标准示例即两个，各带 X/ΔX）。
        // 修复前把多段文本拼成一行、只用末段的 X/ΔX（ΔX 用尽后重复末值）→ 整行从末段位置铺开
        let xml = r#"<ofd:Content xmlns:ofd="http://www.ofdspec.org/2016"><ofd:Layer>
            <ofd:TextObject ID="9" Font="1" Size="3.175" Boundary="10 20 100 10">
                <ofd:TextCode X="0" Y="5" DeltaX="3.175 3.175 3.175">2025</ofd:TextCode>
                <ofd:TextCode X="40" Y="5" DeltaX="3.175 3.175">年1月</ofd:TextCode>
            </ofd:TextObject>
        </ofd:Layer></ofd:Content>"#;
        let (texts, _, _) = parse_ofd_content(xml);
        assert_eq!(texts.len(), 1, "一个 TextObject 仍对应一个对象（提取路径语义不变）");
        assert_eq!(texts[0].segments.len(), 2);
        assert_eq!(texts[0].text, "2025年1月", "对象级文本按旧语义拼接");
        assert_eq!(texts[0].text_x, 40.0, "对象级 X/Y 保留末段（body 坐标提取依赖）");
        let svg = build_svg_text(&texts[0], &HashMap::new(), &HashMap::new(), 1.0, 1.0);
        assert_eq!(svg.matches("<text ").count(), 2, "每段各一个 <text>: {svg}");
        assert!(svg.contains("<text x=\"10\""), "段 1 用自身 X=0（+Boundary 10）: {svg}");
        assert!(svg.contains("<text x=\"50\""), "段 2 用自身 X=40（+Boundary 10）: {svg}");
        assert_eq!(svg.matches("<tspan").count(), 7, "两段各自逐字定位（4 + 3）: {svg}");
    }

    #[test]
    fn test_ofd_deltax_space_excluded_by_boundary() {
        // Boundary 校验选「去空格」口径的正例：ΔX 数须多于 vis-1，含空格口径才会
        // 多消费 ΔX 拉开误差——dx=[10,50]、bw=11 时 err_vis=1 vs err_all=49，
        // 应剔除空格 → 2 个 tspan（A、B），且不渲染空格字形
        let t = ofd_text("A B", 11.0, 3.0, vec![10.0, 50.0], None);
        let svg = build_svg_text(&t, &HashMap::new(), &HashMap::new(), 1.0, 1.0);
        assert_eq!(svg.matches("<tspan").count(), 2);
        assert!(svg.contains(">A</tspan>") && svg.contains(">B</tspan>"));
        assert!(!svg.contains("> </tspan>"), "空格不应渲染为字形");
    }

    #[test]
    fn test_ofd_multiline_negative_deltax_uses_full_charset() {
        // issue #47 附记栏：多行逐字定位（ΔX 含**换行回退负值**），且 ΔX 条数同时吻合两种口径
        // （真实文件 131 字符 + 1 空格配 130 ΔX：含空格口径 131-1=130 ✓，去空格后 130 字符
        // 又"凑巧"= 130 ✓）→ Boundary 自校验因多行累加和 ≠ boundary 宽而失效，长度拟合
        // 便误选去空格口径，换行点后移一个字符（「2」被留在上一行行尾、「、」跑到下一行行首）。
        // 含换行回退时按完整字符流渲染，换行落在正确字符上。
        let mut t = ofd_text("甲乙 丙丁", 5.0, 2.0, vec![2.0, 1.0, -6.0, 2.0], None);
        t.delta_y = vec![0.0, 0.0, 5.0, 0.0];
        let svg = build_svg_text(&t, &HashMap::new(), &HashMap::new(), 1.0, 1.0);
        assert_eq!(svg.matches("<tspan").count(), 5, "含空格口径：5 个字符各占一个 tspan: {svg}");
        let tspans: Vec<&str> = svg.split("<tspan").skip(1).collect();
        assert!(tspans[3].contains("y=\"5.0000\""), "换行应落在第 4 个字符上: {svg}");
        assert!(tspans[2].contains("y=\"0.0000\""), "第 3 个字符仍在首行: {svg}");
    }

    #[test]
    fn test_ofd_deltax_missing_keeps_x_fixed() {
        // 表46：DeltaX 不出现时字型在 X 方向不做偏移。纯 ΔY 定位（竖排/垂直）文本此前
        // 因 ΔX 为空被 font_size 顶替 → 每字右移一个字号、斜着排；现应 x 不动、y 递增
        let mut t = ofd_text("ABC", 3.175, 3.175, vec![], None);
        t.delta_y = vec![3.175, 3.175];
        let svg = build_svg_text(&t, &HashMap::new(), &HashMap::new(), 1.0, 1.0);
        assert_eq!(svg.matches("<tspan").count(), 3);
        assert_eq!(svg.matches("x=\"0.0000\"").count(), 3, "ΔX 缺省时 x 不应推进: {svg}");
        assert!(svg.contains("y=\"3.1750\"") && svg.contains("y=\"6.3500\""), "ΔY 应逐字递增: {svg}");
    }

    #[test]
    fn test_ofd_textcode_inherits_prev_xy() {
        // 表46：X/Y 不出现时沿用上一个 TextCode 的坐标（对象内首个 TextCode 必需）
        let xml = r#"<ofd:Content xmlns:ofd="http://www.ofdspec.org/2016"><ofd:Layer>
            <ofd:TextObject ID="9" Font="1" Size="3.175" Boundary="10 20 100 10">
                <ofd:TextCode X="7" Y="5" DeltaX="3.175 3.175 3.175">2025</ofd:TextCode>
                <ofd:TextCode DeltaX="3.175 3.175">年1月</ofd:TextCode>
            </ofd:TextObject>
        </ofd:Layer></ofd:Content>"#;
        let (texts, _, _) = parse_ofd_content(xml);
        assert_eq!(texts[0].segments.len(), 2);
        assert_eq!(texts[0].segments[1].text_x, 7.0, "缺省 X 沿用上一个 TextCode 的 X");
        assert_eq!(texts[0].segments[1].text_y, 5.0, "缺省 Y 沿用上一个 TextCode 的 Y");
        let svg = build_svg_text(&texts[0], &HashMap::new(), &HashMap::new(), 1.0, 1.0);
        assert!(svg.contains("<text x=\"17\""), "段 2 用继承坐标（7 + Boundary 10）: {svg}");
    }

    #[test]
    fn test_sanitize_text_keeps_structure() {
        // 诊断导出脱敏：汉字→汉、数字→9、字母→A、空白→·，标点原样保留（长度与结构不变）
        assert_eq!(sanitize_text("2021年11月05日"), "9999汉99汉99汉");
        assert_eq!(sanitize_text("城镇住宅用地"), "汉汉汉汉汉汉");
        assert_eq!(sanitize_text("A1 中文"), "A9·汉汉");
        // 金额 + 全角标点：小数点/括号保留，㎡（符号类）原样
        assert_eq!(sanitize_text("（87.76㎡）"), "（99.99㎡）");
        assert_eq!(sanitize_text("1月\u{A4}"), "9汉\u{A4}");
    }

    #[test]
    fn test_dump_ofd_structure_sample() {
        // 诊断 dump：结构 + ΔX 数组完整输出，但原文绝不出现（sample 标题含「通行费」）
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../sample/高速通行费ofd.ofd");
        if !std::path::Path::new(path).exists() {
            return; // sample 缺失时跳过（不作为 CI 硬依赖）
        }
        let dump = dump_ofd_structure(path).expect("dump 应成功");
        // 人工核查 dump 格式：cargo test test_dump_ofd_structure_sample -- --nocapture
        for line in dump.lines().take(10) {
            println!("{line}");
        }
        assert!(dump.contains("TextObject#"), "应含 TextObject 结构");
        assert!(dump.contains("DeltaX="), "应含 DeltaX 数组");
        assert!(dump.contains("汉"), "文本应经脱敏输出");
        assert!(!dump.contains("通行费"), "原始文本不得出现在诊断输出中");
    }
}

#[cfg(test)]
mod font_normalize_tests {
    use super::normalize_font_name;

    #[test]
    fn strips_gb_suffix() {
        assert_eq!(normalize_font_name("仿宋_GB2312"), "仿宋");
        assert_eq!(normalize_font_name("楷体_GBK"), "楷体");
        assert_eq!(normalize_font_name("黑体_GB18030"), "黑体");
    }

    #[test]
    fn strips_subset_prefix_and_suffix() {
        assert_eq!(normalize_font_name("EBOEFC+KaiTi-EBOEFC+KaiTi-0"), "楷体");
    }

    #[test]
    fn maps_postscript_names() {
        assert_eq!(normalize_font_name("CourierNewPSMT"), "Courier New");
        assert_eq!(normalize_font_name("SimSun"), "宋体");
        assert_eq!(normalize_font_name("FangSong"), "仿宋");
    }

    #[test]
    fn keeps_unknown_unchanged() {
        assert_eq!(normalize_font_name("SomeUnknownFont"), "SomeUnknownFont");
    }
}
