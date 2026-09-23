//! 歌词：LRC 解析 + 四级回退取词。
//!
//! 取词顺序（**先本地、后网络**，每级都可能为空）：
//!
//!   1. `sidecar`  —— 音频文件旁边的同名 `.lrc`（用户自己放的，最可信）
//!   2. `cache`    —— `<缓存目录>/lyrics/` 里按「歌手 - 歌名」命中的文件
//!   3. `embedded` —— 音频文件内嵌的歌词标签（需要 `embedded` feature，且要能拿到本地路径）
//!   4. `lrclib`   —— LRCLIB 在线歌词（需要 `online` feature；免费、无鉴权、带时间轴）
//!
//! 「本地」与「在线」刻意拆成两个方法：本地读盘是**毫秒级**，可以在生成快照时同步做；
//! 在线查询要走网络（几百毫秒到几秒），必须异步补 —— 否则「现在在放什么」这条主链路
//! 会被网络拖住。服务层就是按这个分工调用的：快照先出去（无歌词），歌词到了再补事件。
//!
//! 已知限制：`sidecar` 的 `.lrc` 按 UTF-8 解码（BOM 容错），GBK 老资源会显示为乱码。
//! 这是刻意的取舍 —— 不为了十几年一遇的老文件把编码表塞进中间件。

use std::path::{Path, PathBuf};

use crate::types::{LrcLine, Lyrics};
use crate::util::{sanitize_filename, short_hash};
use crate::Result;

/// 取词请求。
#[derive(Debug, Clone)]
pub struct LyricsQuery {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u64,
    /// 本地音频文件路径（Linux MPRIS 常能拿到；macOS/Windows 通常拿不到）
    pub file_path: Option<PathBuf>,
}

impl LyricsQuery {
    pub fn new(title: impl Into<String>, artist: impl Into<String>, album: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            duration_ms: 0,
            file_path: None,
        }
    }

    pub fn with_duration(mut self, ms: u64) -> Self {
        self.duration_ms = ms;
        self
    }

    pub fn with_file(mut self, p: Option<PathBuf>) -> Self {
        self.file_path = p;
        self
    }

    /// 曲目键：跨来源稳定的缓存键（`歌手|歌名|专辑|秒`）。
    pub fn track_key(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.artist.trim(),
            self.title.trim(),
            self.album.trim(),
            self.duration_ms / 1000
        )
    }

    /// 人类可读的文件名基（`歌手 - 歌名`）。
    pub fn file_base(&self) -> String {
        match (self.artist.trim(), self.title.trim()) {
            ("", "") => "unknown".into(),
            ("", t) => t.to_string(),
            (a, "") => a.to_string(),
            (a, t) => format!("{a} - {t}"),
        }
    }

    /// 有没有足够信息去查歌词。
    pub fn is_usable(&self) -> bool {
        !self.title.trim().is_empty()
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// LRC 解析
// ══════════════════════════════════════════════════════════════════════════════

/// LRC 头部元数据。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LrcMeta {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub by: String,
    /// `[length:mm:ss]`
    pub length_ms: Option<i64>,
    /// `[offset:±ms]` —— 正值表示歌词整体晚出现
    pub offset_ms: i64,
}

/// 解析结果。
#[derive(Debug, Clone, PartialEq)]
pub struct LrcDoc {
    pub meta: LrcMeta,
    /// 是否带时间轴
    pub synced: bool,
    /// 按时间升序的歌词行（纯文本歌词时 `t_ms` 恒为 0）
    pub lines: Vec<LrcLine>,
    /// 原始文本
    pub raw: String,
}

impl LrcDoc {
    /// 转成对外的 `Lyrics`。
    pub fn into_lyrics(self, source: &str, track_key: &str) -> Lyrics {
        Lyrics {
            source: source.to_string(),
            synced: self.synced,
            offset_ms: self.meta.offset_ms,
            lines: self.lines,
            track_key: track_key.to_string(),
            raw: Some(self.raw),
        }
    }
}

/// 解析 LRC / 纯文本歌词。
///
/// 容忍真实世界里的脏数据：BOM、CRLF、一个时间戳带多行、增强型 LRC 的
/// `<00:12.34>` 逐字标签、`[mm:ss:xx]`（用冒号而非点号）这种非标准写法，
/// 以及完全没有时间戳的纯文本歌词（此时 `synced = false`）。
///
/// 规则：**带时间轴的只保留带时间轴的行**（无时间戳的行排不进时间轴，只能是章节标记
/// 之类的东西）；**纯文本歌词保留所有非空行**（包括 `[Verse]` 这样的分段标记）。
pub fn parse_lrc(text: &str) -> LrcDoc {
    let raw = text.to_string();
    let cleaned = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut meta = LrcMeta::default();
    let mut timed: Vec<(i64, String)> = Vec::new();
    let mut plain: Vec<String> = Vec::new();

    for line in cleaned.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        let (times, metas, rest) = split_tags(line);
        for (k, v) in metas {
            match k.as_str() {
                "ar" | "artist" => meta.artist = v,
                "ti" | "title" => meta.title = v,
                "al" | "album" => meta.album = v,
                "by" => meta.by = v,
                "length" => meta.length_ms = parse_timestamp(&v),
                "offset" => {
                    if let Ok(n) = v.trim().parse::<i64>() {
                        meta.offset_ms = n;
                    }
                }
                _ => {}
            }
        }
        let text_part = strip_word_tags(rest).trim().to_string();
        if times.is_empty() {
            if !text_part.is_empty() {
                plain.push(text_part);
            }
            continue;
        }
        for t in times {
            timed.push((t, text_part.clone()));
        }
    }

    if !timed.is_empty() {
        timed.sort_by_key(|(t, _)| *t);
        return LrcDoc {
            meta,
            synced: true,
            lines: timed
                .into_iter()
                .map(|(t_ms, text)| LrcLine { t_ms, text })
                .collect(),
            raw,
        };
    }

    LrcDoc {
        meta,
        synced: false,
        lines: plain.into_iter().map(|text| LrcLine { t_ms: 0, text }).collect(),
        raw,
    }
}

/// 拆出行首的所有 `[...]` 标签，返回（时间戳, 元数据键值, 剩余文本）。
fn split_tags(line: &str) -> (Vec<i64>, Vec<(String, String)>, &str) {
    let mut times = Vec::new();
    let mut metas = Vec::new();
    let mut rest = line;
    loop {
        let trimmed = rest.trim_start();
        if !trimmed.starts_with('[') {
            rest = trimmed;
            break;
        }
        let Some(close) = trimmed.find(']') else {
            rest = trimmed;
            break;
        };
        let inner = &trimmed[1..close];
        let after = &trimmed[close + 1..];
        if let Some(t) = parse_timestamp(inner) {
            times.push(t);
            rest = after;
            continue;
        }
        if let Some((k, v)) = split_meta(inner) {
            metas.push((k, v));
            rest = after;
            continue;
        }
        // 既不是时间戳也不是 `k:v` 元数据（例如 `[Chorus]`）：不当标签，按正文处理
        rest = trimmed;
        break;
    }
    (times, metas, rest)
}

/// `key:value` 形式的元数据标签（键只认字母，避免把 `[Chorus]` 误当元数据）。
fn split_meta(inner: &str) -> Option<(String, String)> {
    let (k, v) = inner.split_once(':')?;
    let key = k.trim().to_ascii_lowercase();
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some((key, v.trim().to_string()))
}

/// 解析时间戳，返回毫秒。
///
/// 支持：`mm:ss`、`mm:ss.xx`、`mm:ss.xxx`、`mm:ss:xx`（非标准，末段是小数秒）。
///
/// **三段的写法按 `mm:ss:frac` 解，不按 `hh:mm:ss`** —— LRC 的时间戳从不用小时，
/// 而中文歌词站大量使用 `[00:12:34]` 表示 12.34 秒。把三段当小时会得到「0 分 3 秒」
/// 这种差三个数量级的结果，那是更糟的错。
fn parse_timestamp(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s.split(':').collect();
    // 两段 = mm:ss（秒可带小数）；三段 = mm:ss:frac（末段是小数秒）
    let (minutes_part, secs_part, frac_part) = match parts.len() {
        2 => (parts[0], parts[1], None),
        3 => (parts[0], parts[1], Some(parts[2])),
        _ => return None,
    };
    let minutes = minutes_part.trim().parse::<i64>().ok()?;
    if minutes < 0 {
        return None;
    }
    let (secs, frac_ms) = if let Some((a, b)) = secs_part.split_once('.') {
        (a.trim().parse::<i64>().ok()?, frac_to_ms(b))
    } else if let Some(frac) = frac_part {
        (secs_part.trim().parse::<i64>().ok()?, frac_to_ms(frac))
    } else {
        (secs_part.trim().parse::<i64>().ok()?, 0)
    };
    if secs < 0 || secs >= 60 {
        return None;
    }
    Some(minutes * 60_000 + secs * 1000 + frac_ms)
}

/// 小数秒字符串 → 毫秒：1 位=百毫秒、2 位=十毫秒、3 位及以上=毫秒。
fn frac_to_ms(frac: &str) -> i64 {
    let mut digits = frac.trim().to_string();
    digits.retain(|c| c.is_ascii_digit());
    if digits.is_empty() {
        return 0;
    }
    let Ok(val) = digits.parse::<i64>() else {
        return 0;
    };
    match digits.len() {
        1 => val * 100,
        2 => val * 10,
        3 => val,
        n => val / 10i64.pow((n - 3) as u32),
    }
}

/// 去掉增强型 LRC 的逐字标签 `<00:12.34>`（保留文本，行时间由行首标签决定）。
fn strip_word_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('<') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        match word_tag_len(tail) {
            Some(len) => rest = &tail[len..],
            None => {
                out.push('<');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// `s` 以 `<` 开头时，判断它是不是逐字标签，是则返回含尖括号的字节长度。
fn word_tag_len(s: &str) -> Option<usize> {
    let end = s.find('>')?;
    let inner = s.get(1..end)?;
    if inner.is_empty() || inner.len() > 16 {
        return None;
    }
    let mut parts = inner.split(':');
    let first = parts.next()?;
    if first.is_empty() || !first.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    parts.next()?;
    Some(end + 1)
}

// ══════════════════════════════════════════════════════════════════════════════
// 取词器
// ══════════════════════════════════════════════════════════════════════════════

/// 歌词来源的可用性（供 `status` 上报）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricsStatus {
    pub local_sources: Vec<String>,
    pub online_enabled: bool,
    pub last_error: Option<String>,
}

/// 多来源歌词取词器。
pub struct LyricsResolver {
    cache_dir: PathBuf,
    online_enabled: bool,
    last_error: std::sync::Mutex<Option<String>>,
    /// 最近解析过的曲目（曲目键 → 结果）的备忘。
    ///
    /// 为什么需要它：服务层**每次轮询**都会问一遍歌词。不缓存的话每秒都要读盘（虽然只有
    /// 几百微秒，但没必要）；而缓存了之后「每轮都解析」这个做法就变得既正确又便宜 ——
    /// 这正是修掉「歌词偶发丢失」那个竞态的关键（见 `service.rs` 里 `apply()` 的注释）。
    /// 只保留最近几个，避免长时间运行时无限增长。
    memo: std::sync::Mutex<Vec<(String, Option<Lyrics>)>>,
}

/// 备忘里最多保留几首（同时也就限制了一次查找的线性扫描长度）。
const MEMO_CAPACITY: usize = 8;

impl LyricsResolver {
    /// `cache_dir` 是缓存根目录（内部会再建 `lyrics/` 子目录）。
    pub fn new(cache_dir: impl Into<PathBuf>, online_enabled: bool) -> Self {
        let cache_dir = cache_dir.into();
        let _ = std::fs::create_dir_all(cache_dir.join("lyrics"));
        Self {
            cache_dir,
            online_enabled: cfg!(feature = "online") && online_enabled,
            last_error: std::sync::Mutex::new(None),
            memo: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// 带备忘的本地取词：**只缓存命中**。
    ///
    /// 刻意**不缓存「没有」**：用户完全可能在播放中往目录里补一个 `.lrc`，
    /// 缓存否定结果会让它一直看不见（实测就是这个问题）。而一次未命中的本地查找
    /// 只是几次 `stat`，每秒做一次毫无压力。
    pub fn resolve_local_cached(&self, q: &LyricsQuery) -> Option<Lyrics> {
        if !q.is_usable() {
            return None;
        }
        let key = q.track_key();
        if let Ok(memo) = self.memo.lock()
            && let Some((_, Some(hit))) = memo.iter().find(|(k, _)| *k == key)
        {
            return Some(hit.clone());
        }
        let found = self.resolve_local(q)?;
        if let Ok(mut memo) = self.memo.lock() {
            memo.retain(|(k, _)| *k != key);
            memo.push((key, Some(found.clone())));
            while memo.len() > MEMO_CAPACITY {
                memo.remove(0);
            }
        }
        Some(found)
    }

    /// 清掉某个曲目的备忘（在线歌词拿到之后要清，否则下次会拿旧的「没有」）。
    pub fn forget_memo(&self, track_key: &str) {
        if let Ok(mut memo) = self.memo.lock() {
            memo.retain(|(k, _)| k != track_key);
        }
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn online_enabled(&self) -> bool {
        self.online_enabled
    }

    pub fn status(&self) -> LyricsStatus {
        let mut local = vec!["sidecar".to_string(), "cache".to_string()];
        if cfg!(feature = "embedded") {
            local.push("embedded".to_string());
        }
        LyricsStatus {
            local_sources: local,
            online_enabled: self.online_enabled,
            last_error: self.last_error.lock().ok().and_then(|g| g.clone()),
        }
    }

    fn set_error(&self, msg: Option<String>) {
        if let Ok(mut g) = self.last_error.lock() {
            *g = msg;
        }
    }

    /// 本地取词（同步、毫秒级）：sidecar → 缓存 → 内嵌标签。
    pub fn resolve_local(&self, q: &LyricsQuery) -> Option<Lyrics> {
        if !q.is_usable() {
            return None;
        }
        self.from_sidecar(q)
            .or_else(|| self.from_cache(q))
            .or_else(|| self.from_embedded(q))
    }

    /// 在线取词（LRCLIB）。未启用 `online` 时直接返回 `None`。
    ///
    /// 命中后写入缓存目录，下次走本地路径即可命中。
    pub async fn resolve_online(&self, q: LyricsQuery) -> Option<Lyrics> {
        if !self.online_enabled || !q.is_usable() {
            return None;
        }
        #[cfg(feature = "online")]
        if let Some(doc) = online_lookup(&q).await {
            let key = q.track_key();
            self.set_error(None);
            let lyrics = doc.into_lyrics("lrclib", &key);
            if let Some(raw) = lyrics.raw.as_ref() {
                let _ = write_cached_lyrics(&self.cache_dir, &q, raw);
            }
            return Some(lyrics);
        }
        self.set_error(Some(format!("LRCLIB 未命中：{}", q.file_base())));
        None
    }

    /// 音频文件旁边的 `.lrc`。
    fn from_sidecar(&self, q: &LyricsQuery) -> Option<Lyrics> {
        let file = q.file_path.as_ref()?;
        let stem = file.file_stem()?.to_str()?;
        let dir = file.parent()?;
        // 去掉 `01 - ` / `07.` 这类序号前缀后再试一次
        let stripped = strip_track_prefix(stem);
        let mut candidates: Vec<PathBuf> = Vec::new();
        for base in [stem, stripped.as_str()] {
            for name in [format!("{base}.lrc"), format!("{base}.LRC")] {
                candidates.push(dir.join(&name));
                candidates.push(dir.join("lyrics").join(&name));
            }
        }
        if !q.title.trim().is_empty() {
            candidates.push(dir.join(format!("{}.lrc", sanitize_filename(q.title.trim(), 120))));
        }
        candidates
            .into_iter()
            .find_map(|p| read_lrc_file(&p).map(|doc| doc.into_lyrics("sidecar", &q.track_key())))
    }

    /// `<cache>/lyrics/` 里按「歌手 - 歌名」或曲目键命中的文件。
    fn from_cache(&self, q: &LyricsQuery) -> Option<Lyrics> {
        let dir = self.cache_dir.join("lyrics");
        let base = sanitize_filename(&q.file_base(), 120);
        let mut candidates = vec![
            dir.join(format!("{base}.lrc")),
            dir.join(format!("{}.lrc", sanitize_filename(&q.track_key(), 160))),
            dir.join(format!("{}.lrc", short_hash(&q.track_key()))),
        ];
        if !q.title.trim().is_empty() {
            candidates.push(dir.join(format!("{}.lrc", sanitize_filename(q.title.trim(), 120))));
        }
        candidates
            .into_iter()
            .find_map(|p| read_lrc_file(&p).map(|doc| doc.into_lyrics("cache", &q.track_key())))
    }

    /// 内嵌歌词标签。
    fn from_embedded(&self, q: &LyricsQuery) -> Option<Lyrics> {
        #[cfg(feature = "embedded")]
        {
            let file = q.file_path.as_ref()?;
            let tags = crate::tags::read_embedded(file).ok()??;
            let text = tags.lyrics?;
            if text.trim().is_empty() {
                return None;
            }
            return Some(parse_lrc(&text).into_lyrics("embedded", &q.track_key()));
        }
        #[cfg(not(feature = "embedded"))]
        {
            let _ = q;
            None
        }
    }
}

/// 从 `01 - Song` 里剥掉序号前缀。
fn strip_track_prefix(stem: &str) -> String {
    let s = stem.trim_start();
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 3 {
        return stem.to_string();
    }
    let rest = &s[digits.len()..];
    let trimmed = rest.trim_start_matches([' ', '-', '.', '_']).trim();
    if trimmed.is_empty() {
        return stem.to_string();
    }
    trimmed.to_string()
}

fn read_lrc_file(p: &Path) -> Option<LrcDoc> {
    let bytes = std::fs::read(p).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);
    let doc = parse_lrc(&text);
    if doc.lines.is_empty() {
        return None;
    }
    Some(doc)
}

/// 把歌词写进缓存目录（在线命中后落盘、`lyrics sync` 命令共用）。
pub fn write_cached_lyrics(cache_dir: &Path, q: &LyricsQuery, raw: &str) -> Result<PathBuf> {
    let dir = cache_dir.join("lyrics");
    std::fs::create_dir_all(&dir)?;
    let base = sanitize_filename(&q.file_base(), 120);
    let path = dir.join(format!("{base}.lrc"));
    std::fs::write(&path, raw)?;
    Ok(path)
}

/// 列出缓存目录里已有的歌词文件。
pub fn list_cached_lyrics(cache_dir: &Path) -> Vec<PathBuf> {
    let dir = cache_dir.join("lyrics");
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("lrc")) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

// ══════════════════════════════════════════════════════════════════════════════
// LRCLIB（在线）
// ══════════════════════════════════════════════════════════════════════════════

#[cfg(feature = "online")]
async fn online_lookup(q: &LyricsQuery) -> Option<LrcDoc> {
    let query = q.clone();
    tokio::task::spawn_blocking(move || lrclib_lookup(&query))
        .await
        .ok()
        .flatten()
}

/// LRCLIB 查询：先精确接口，未命中再用搜索接口挑时长最接近的一条。
#[cfg(feature = "online")]
fn lrclib_lookup(q: &LyricsQuery) -> Option<LrcDoc> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Hit {
        #[serde(default)]
        duration: f64,
        #[serde(default)]
        instrumental: bool,
        #[serde(default)]
        plain_lyrics: Option<String>,
        #[serde(default)]
        synced_lyrics: Option<String>,
    }

    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(8)))
        .user_agent(concat!(
            "media-bridge/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/oneincase/media-bridge)"
        ))
        .build()
        .new_agent();

    let pick = |hit: Hit| -> Option<LrcDoc> {
        if hit.instrumental {
            return None;
        }
        let text = hit.synced_lyrics.or(hit.plain_lyrics)?;
        if text.trim().is_empty() {
            return None;
        }
        Some(parse_lrc(&text))
    };

    // 1) 精确接口（需要 artist + track）
    if !q.artist.trim().is_empty() {
        let mut url = format!(
            "https://lrclib.net/api/get?artist_name={}&track_name={}",
            urlencode(&q.artist),
            urlencode(&q.title)
        );
        if !q.album.trim().is_empty() {
            url.push_str(&format!("&album_name={}", urlencode(&q.album)));
        }
        if q.duration_ms > 0 {
            url.push_str(&format!("&duration={}", q.duration_ms / 1000));
        }
        if let Ok(mut resp) = agent.get(&url).call()
            && let Ok(hit) = resp.body_mut().read_json::<Hit>()
            && let Some(doc) = pick(hit)
        {
            return Some(doc);
        }
    }

    // 2) 搜索接口：时长差是主序（带时间轴的优先）
    let query = format!("{} {}", q.artist.trim(), q.title.trim()).trim().to_string();
    if query.is_empty() {
        return None;
    }
    let url = format!("https://lrclib.net/api/search?q={}", urlencode(&query));
    let mut resp = agent.get(&url).call().ok()?;
    let hits: Vec<Hit> = resp.body_mut().read_json().ok()?;
    let target_s = q.duration_ms as f64 / 1000.0;
    let mut best: Option<(f64, LrcDoc)> = None;
    for hit in hits {
        let has_sync = hit.synced_lyrics.is_some();
        let delta = if target_s > 1.0 && hit.duration > 1.0 {
            (hit.duration - target_s).abs()
        } else {
            0.0
        };
        if target_s > 1.0 && delta > 5.0 {
            continue;
        }
        let score = delta + if has_sync { 0.0 } else { 30.0 };
        if let Some(doc) = pick(hit)
            && best.as_ref().is_none_or(|(s, _)| score < *s)
        {
            best = Some((score, doc));
        }
    }
    best.map(|(_, d)| d)
}

/// 极简 URL 编码（只覆盖查询参数需要的字符，不引额外依赖）。
#[cfg(feature = "online")]
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_lrc_with_metadata() {
        let text = "[ti:测试歌曲]\n[ar:某歌手]\n[al:某专辑]\n[offset:+500]\n\
                    [00:01.00]第一行\n[00:05.50]第二行\n[01:02.25]第三行\n";
        let doc = parse_lrc(text);
        assert!(doc.synced);
        assert_eq!(doc.meta.title, "测试歌曲");
        assert_eq!(doc.meta.artist, "某歌手");
        assert_eq!(doc.meta.album, "某专辑");
        assert_eq!(doc.meta.offset_ms, 500);
        assert_eq!(doc.lines.len(), 3);
        assert_eq!(doc.lines[0], LrcLine { t_ms: 1_000, text: "第一行".into() });
        assert_eq!(doc.lines[1], LrcLine { t_ms: 5_500, text: "第二行".into() });
        assert_eq!(doc.lines[2], LrcLine { t_ms: 62_250, text: "第三行".into() });
    }

    #[test]
    fn one_timestamp_can_cover_multiple_lines_and_sorts() {
        let text = "[00:10.00]后出现的\n[00:02.00]先出现的\n[00:02.00][00:20.00]重复行\n";
        let doc = parse_lrc(text);
        assert_eq!(
            doc.lines.iter().map(|l| (l.t_ms, l.text.as_str())).collect::<Vec<_>>(),
            vec![
                (2_000, "先出现的"),
                (2_000, "重复行"),
                (10_000, "后出现的"),
                (20_000, "重复行"),
            ]
        );
    }

    #[test]
    fn plain_text_lyrics_mark_unsynced() {
        let doc = parse_lrc("第一行\n\n第二行\n第三行\n");
        assert!(!doc.synced);
        assert_eq!(doc.lines.len(), 3);
        assert!(doc.lines.iter().all(|l| l.t_ms == 0));
    }

    #[test]
    fn plain_text_keeps_section_markers_but_synced_drops_them() {
        let plain = parse_lrc("[Verse 1]\n第一行\n");
        assert!(!plain.synced);
        assert!(plain.lines.iter().any(|l| l.text == "[Verse 1]"));

        let synced = parse_lrc("[Chorus]\n[00:01.00]歌词\n");
        assert!(synced.synced);
        assert_eq!(synced.lines, vec![LrcLine { t_ms: 1_000, text: "歌词".into() }]);
    }

    #[test]
    fn enhanced_lrc_word_tags_are_stripped() {
        let doc = parse_lrc("[00:01.00]<00:01.00>Hel<00:01.30>lo <00:01.60>world\n");
        assert_eq!(doc.lines.len(), 1);
        assert_eq!(doc.lines[0].text, "Hello world");
    }

    #[test]
    fn angle_brackets_that_are_not_word_tags_are_kept() {
        let doc = parse_lrc("[00:01.00]a < b > c\n");
        assert_eq!(doc.lines[0].text, "a < b > c");
    }

    #[test]
    fn tolerates_bom_crlf_and_nonstandard_colon_fraction() {
        // 中文歌词站常见的 `[mm:ss:xx]`：末段是小数秒（50 → 0.50s）
        let text = "\u{feff}[00:03:50]非标准厘秒写法\r\n[00:04.123]毫秒写法\r\n[01:02.25]常规写法\r\n";
        let doc = parse_lrc(text);
        assert_eq!(
            doc.lines.iter().map(|l| (l.t_ms, l.text.as_str())).collect::<Vec<_>>(),
            vec![
                (3_500, "非标准厘秒写法"),
                (4_123, "毫秒写法"),
                (62_250, "常规写法"),
            ]
        );
    }

    #[test]
    fn fraction_scales_by_digit_count() {
        // 同一个小数秒写成不同位数，毫秒值必须一致（0.5s）
        assert_eq!(frac_to_ms("5"), 500);
        assert_eq!(frac_to_ms("50"), 500);
        assert_eq!(frac_to_ms("500"), 500);
        assert_eq!(frac_to_ms("5000"), 500);
        // 0.25s 的两种常见写法
        assert_eq!(frac_to_ms("25"), 250);
        assert_eq!(frac_to_ms("250"), 250);
        assert_eq!(frac_to_ms(""), 0);
        assert_eq!(frac_to_ms("abc"), 0);
    }

    #[test]
    fn offset_shifts_active_line_later() {
        let doc = parse_lrc("[offset:+1000]\n[00:10.00]a\n[00:20.00]b\n");
        assert_eq!(doc.meta.offset_ms, 1_000);
        let l = doc.into_lyrics("test", "k");
        // 正向 offset = 歌词整体晚出现 1 秒
        assert_eq!(l.effective_t_ms(0), Some(11_000));
        assert_eq!(l.active_index(20_500), Some(0), "第 2 行要等到 21s 才生效");
        assert_eq!(l.active_index(21_000), Some(1));
    }

    #[test]
    fn negative_offset_shifts_lyrics_earlier() {
        let l = parse_lrc("[offset:-2000]\n[00:10.00]a\n[00:20.00]b\n").into_lyrics("test", "k");
        assert_eq!(l.effective_t_ms(0), Some(8_000));
        assert_eq!(l.active_index(18_500), Some(1), "offset -2000 时第 2 行 18s 就生效");
    }

    #[test]
    fn track_prefix_is_stripped_for_sidecar_lookup() {
        assert_eq!(strip_track_prefix("01 - Song"), "Song");
        assert_eq!(strip_track_prefix("07.Song"), "Song");
        assert_eq!(strip_track_prefix("Song"), "Song");
        assert_eq!(strip_track_prefix("2024 Song"), "2024 Song", "4 位数字不当序号");
        assert_eq!(strip_track_prefix("01 - "), "01 - ", "剥完是空则原样返回");
    }

    #[test]
    fn query_key_is_stable() {
        let q = LyricsQuery::new("歌", "手", "专辑").with_duration(123_456);
        assert_eq!(q.track_key(), "手|歌|专辑|123");
        assert_eq!(q.file_base(), "手 - 歌");
    }

    #[test]
    fn urlencode_escapes_query_chars() {
        #[cfg(feature = "online")]
        {
            assert_eq!(urlencode("周杰伦"), "%E5%91%A8%E6%9D%B0%E4%BC%A6");
            assert_eq!(urlencode("a b&c"), "a+b%26c");
        }
    }

    #[test]
    fn sidecar_and_cache_lookup_work_off_disk() {
        let dir = std::env::temp_dir().join(format!("mb-lyrics-{}", crate::util::short_hash(&crate::util::uuid_v4_ish())));
        let audio_dir = dir.join("music");
        std::fs::create_dir_all(&audio_dir).unwrap();
        let audio = audio_dir.join("03 - My Song.mp3");
        std::fs::write(&audio, b"fake").unwrap();
        std::fs::write(audio_dir.join("My Song.lrc"), "[00:01.00]侧车歌词\n").unwrap();

        let resolver = LyricsResolver::new(dir.join("cache"), false);
        let q = LyricsQuery::new("My Song", "Someone", "Album")
            .with_duration(60_000)
            .with_file(Some(audio.clone()));
        let hit = resolver.resolve_local(&q).expect("应命中同名 sidecar（去掉序号前缀后）");
        assert_eq!(hit.source, "sidecar");
        assert_eq!(hit.lines[0].text, "侧车歌词");
        assert!(hit.synced);

        // 缓存命中路径
        let q2 = LyricsQuery::new("另一首", "别人", "别的专辑");
        write_cached_lyrics(resolver.cache_dir(), &q2, "[00:02.00]缓存歌词\n").unwrap();
        let hit2 = resolver.resolve_local(&q2).expect("应命中 cache");
        assert_eq!(hit2.source, "cache");
        assert_eq!(hit2.lines[0].t_ms, 2_000);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_everything_returns_none_without_panicking() {
        let resolver = LyricsResolver::new(std::env::temp_dir().join("mb-lyrics-missing"), false);
        let q = LyricsQuery::new("不存在的歌", "不存在的人", "");
        assert!(resolver.resolve_local(&q).is_none());
    }
}
