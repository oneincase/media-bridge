//! 小工具：时钟、MIME 嗅探、去重哈希、UUID、文件名清洗。
//!
//! 刻意不引第三方依赖（没有 rand / chrono / mime / uuid）——这些都是十几行的东西，
//! 而中间件会被塞进各种宿主进程里，依赖树越短越好。

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Unix 毫秒时间戳。
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 单调时钟毫秒（用于测速/超时，不受系统时间调整影响）。
pub fn monotonic_ms() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let start = START.get_or_init(std::time::Instant::now);
    start.elapsed().as_millis() as u64
}

// ══════════════════════════════════════════════════════════════════════════════
// MIME 嗅探
// ══════════════════════════════════════════════════════════════════════════════

/// 按魔数嗅探图片类型。
///
/// **不信播放器自称的 MIME**：MediaRemote/GSMTC/MPRIS 给的类型经常是空的或错的，
/// 而封面存错后缀 → 浏览器按错误类型解码 → 图直接不显示（旧实现踩过）。
pub fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() < 4 {
        return None;
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.starts_with(b"BM") {
        return Some("image/bmp");
    }
    if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        return Some("image/tiff");
    }
    // ISO-BMFF 家族：ftyp 盒子在偏移 4，brand 决定是 HEIC 还是 AVIF
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        let brand = &bytes[8..12];
        if brand == b"avif" || brand == b"avis" {
            return Some("image/avif");
        }
        if brand.starts_with(b"hei") || brand.starts_with(b"hev") || brand.starts_with(b"mif1") {
            return Some("image/heic");
        }
    }
    if bytes.starts_with(b"<?xml") || bytes.starts_with(b"<svg") {
        return Some("image/svg+xml");
    }
    None
}

/// MIME → 文件后缀。
pub fn ext_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        "image/avif" => "avif",
        "image/heic" => "heic",
        "image/svg+xml" => "svg",
        _ => "bin",
    }
}

/// 归一化播放器给的 MIME（去参数、统一小写），未知则 `None`。
pub fn normalize_mime(raw: &str) -> Option<String> {
    let m = raw.trim().split(';').next()?.trim().to_ascii_lowercase();
    if m.is_empty() || !m.contains('/') {
        return None;
    }
    // `image/jpg` 是非标准写法，统一成 `image/jpeg`
    Some(if m == "image/jpg" { "image/jpeg".to_string() } else { m })
}

// ══════════════════════════════════════════════════════════════════════════════
// 图片尺寸（PNG / JPEG，够用且不需要解码整张图）
// ══════════════════════════════════════════════════════════════════════════════

/// 从图片头部读出宽高（PNG / JPEG；其它格式返回 None）。
///
/// 消费端布局时用它预留封面位置，避免「图加载完 → 布局跳一下」。
pub fn image_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() >= 24 && bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return Some((w, h));
    }
    if bytes.len() >= 4 && bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return jpeg_size(bytes);
    }
    None
}

/// 扫 JPEG 的段，找 SOFn（0xC0-0xCF 去掉 C4/C8/CC）里的宽高。
fn jpeg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2usize;
    while i + 9 < bytes.len() {
        if bytes[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = bytes[i + 1];
        // 无长度字段的填充字节
        if marker == 0xFF {
            i += 1;
            continue;
        }
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        let is_sof = (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_sof {
            let h = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
            let w = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u32;
            return Some((w, h));
        }
        i += 2 + len.max(2);
    }
    None
}

// ══════════════════════════════════════════════════════════════════════════════
// 哈希 / ID
// ══════════════════════════════════════════════════════════════════════════════

/// FNV-1a 64 位。用途只是「缓存键去重」，不涉及安全，不需要加密哈希。
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 字符串的短指纹（16 位十六进制），用于文件名与缓存键。
pub fn short_hash(s: &str) -> String {
    format!("{:016x}", fnv1a64(s.as_bytes()))
}

/// 生成一个 UUID v4 形态的字符串。
///
/// 不引 rand：用「纳秒时间 + 进程 id + 计数器 + 栈地址」拼熵。这些值只用于
/// 生成一次性的设备/聚合设备 UID（要求「本机唯一且每次启动不同」），
/// 不做任何安全用途。
pub fn uuid_v4_ish() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let stack_marker = 0u8;
    let mix = nanos
        ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_mul(0xBF58_476D_1CE4_E5B9)
        ^ (&stack_marker as *const u8 as u64).wrapping_mul(0x94D0_49BB_1331_11EB);

    // 再混两轮，让低位也有分布（相邻调用只有计数器不同）
    let a = mix ^ (mix >> 33);
    let b = a.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    let c = (b ^ (b >> 33)).wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    let d = c ^ (c >> 33);

    // 取 32 位十六进制，再按 UUID 的 8-4-4-4-12 分组
    let hex = format!("{mix:016x}{a:016x}{d:016x}{b:016x}");
    let mut chars: Vec<char> = hex.chars().take(32).collect();
    // 版本位（4 = 随机 UUID）与变体位（10xx）
    chars[12] = '4';
    chars[16] = match chars[16] {
        '0'..='7' => '8',
        _ => '9',
    };
    let s: String = chars.into_iter().collect();
    format!("{}-{}-{}-{}-{}", &s[0..8], &s[8..12], &s[12..16], &s[16..20], &s[20..32])
}

/// 清洗成安全的文件名片段（歌词缓存用）。
pub fn sanitize_filename(s: &str, max: usize) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\n' | '\r' | '\t' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').to_string();
    if trimmed.is_empty() {
        return "unknown".to_string();
    }
    trimmed.chars().take(max).collect()
}

/// 取路径的 UTF-8 字符串（非 UTF-8 路径尽力而为）。
pub fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// 把秒数格式化成 `mm:ss`（日志/调试用）。
pub fn fmt_ms(ms: u64) -> String {
    let total = ms / 1000;
    format!("{:02}:{:02}", total / 60, total % 60)
}

/// 日志用截断（避免把整张封面的 base64 打进日志）。
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

// ══════════════════════════════════════════════════════════════════════════════
// PNG 生成（零依赖）
// ══════════════════════════════════════════════════════════════════════════════

/// 生成一张 RGB PNG（zlib 只用「存储块」，不压缩）。
///
/// 为什么要自己造图：假播放器与测试需要**真实可解码的图片字节**——这样封面链路上的
/// 魔数嗅探、尺寸解析、落盘、HTTP 出图跑的都是真路径，而不是拿几个随机字节糊过去
/// （那种测试一到真实播放器就会暴露问题）。`seed` 决定配色，不同曲目得到不同的图。
pub fn encode_png_rgb(width: u32, height: u32, seed: u32) -> Vec<u8> {
    let w = width.max(1);
    let h = height.max(1);
    let mut raw = Vec::with_capacity((h * (1 + w * 3)) as usize);
    for y in 0..h {
        raw.push(0u8); // 行滤波：None
        for x in 0..w {
            let r = ((x * 255 / w) ^ (seed * 37)) as u8;
            let g = ((y * 255 / h) ^ (seed * 91)) as u8;
            let b = ((x + y) * 255 / (w + h)) as u8;
            raw.extend_from_slice(&[r, g, b]);
        }
    }

    let mut out = Vec::with_capacity(raw.len() + 128);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8bit, truecolor, deflate, 无滤波, 非隔行
    write_png_chunk(&mut out, b"IHDR", &ihdr);
    write_png_chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    write_png_chunk(&mut out, b"IEND", &[]);
    out
}

fn write_png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    let mut crc_input = Vec::with_capacity(4 + payload.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(payload);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// zlib 流：2 字节头 + deflate「存储块」+ adler32 尾。
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + (data.len() / 65535 + 1) * 5 + 6);
    out.push(0x78);
    out.push(0x01); // (0x78*256 + 0x01) % 31 == 0
    let mut chunks = data.chunks(65_535).peekable();
    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);
    } else {
        while let Some(c) = chunks.next() {
            let last = chunks.peek().is_none();
            out.push(if last { 1 } else { 0 });
            let len = c.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(c);
        }
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let mut a = 1u32;
    let mut b = 0u32;
    for &byte in data {
        a = (a + byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_picks_real_type_over_claim() {
        assert_eq!(sniff_image_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(
            sniff_image_mime(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
            Some("image/png")
        );
        assert_eq!(sniff_image_mime(b"GIF89a..."), Some("image/gif"));
        assert_eq!(
            sniff_image_mime(b"RIFF____WEBPVP8 "),
            Some("image/webp")
        );
        assert_eq!(sniff_image_mime(b"not an image"), None);
    }

    #[test]
    fn normalize_mime_cleans_up_player_values() {
        assert_eq!(normalize_mime("image/jpg"), Some("image/jpeg".into()));
        assert_eq!(normalize_mime("  IMAGE/PNG ; charset=x"), Some("image/png".into()));
        assert_eq!(normalize_mime(""), None);
        assert_eq!(normalize_mime("jpeg"), None);
    }

    #[test]
    fn png_size_reads_ihdr() {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&[0, 0, 0, 13]); // IHDR 长度
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        assert_eq!(image_size(&png), Some((640, 480)));
    }

    #[test]
    fn jpeg_size_walks_to_sof() {
        let mut j = vec![0xFF, 0xD8, 0xFF];
        // APP0：FF E0 + 长度 4（含长度自身）+ 2 字节内容
        j.extend_from_slice(&[0xE0, 0x00, 0x04, 0xAA, 0xBB]);
        // SOF0：FF C0 + 长度 17 + 精度 8 + 高 300 + 宽 400 + 3 个分量描述
        j.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        j.extend_from_slice(&300u16.to_be_bytes());
        j.extend_from_slice(&400u16.to_be_bytes());
        j.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        assert_eq!(image_size(&j), Some((400, 300)));
    }

    #[test]
    fn uuid_is_unique_across_calls_and_well_formed() {
        let a = uuid_v4_ish();
        let b = uuid_v4_ish();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[4].len(), 12);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }

    #[test]
    fn sanitize_strips_path_separators() {
        assert_eq!(sanitize_filename("a/b:c*d", 32), "a_b_c_d");
        assert_eq!(sanitize_filename("   ", 32), "unknown");
        assert_eq!(sanitize_filename("...", 32), "unknown");
        assert_eq!(sanitize_filename("正常名字", 32), "正常名字");
    }

    #[test]
    fn generated_png_is_recognizable_and_self_describing() {
        let png = encode_png_rgb(64, 32, 7);
        assert_eq!(sniff_image_mime(&png), Some("image/png"));
        assert_eq!(image_size(&png), Some((64, 32)));
        assert_eq!(png[12..16], *b"IHDR");
        // IEND 块 = 长度(4) + "IEND" + CRC(4)
        assert_eq!(&png[png.len() - 12..], b"\0\0\0\0IEND\xae\x42\x60\x82");
        // 同样的输入必须给出同样的字节（测试里当固定 fixture 用）
        assert_eq!(png, encode_png_rgb(64, 32, 7));
        assert_ne!(png, encode_png_rgb(64, 32, 8));
    }

    #[test]
    fn generated_png_survives_multi_block_deflate() {
        // 每像素 3 字节 → 需要 > 65535 才有多个存储块
        let png = encode_png_rgb(200, 200, 1);
        assert_eq!(image_size(&png), Some((200, 200)));
    }

    #[test]
    fn crc32_matches_known_vector() {
        // IEEE CRC-32("123456789") = 0xCBF43926
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn adler32_matches_known_vector() {
        // zlib 文档里的样例：adler32("Wikipedia") = 0x11E60398
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }
}
