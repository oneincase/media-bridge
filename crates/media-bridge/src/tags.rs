//! 内嵌标签（需要 `embedded` feature）：直接从音频文件里读标题/歌手/专辑/内嵌歌词/内嵌封面。
//!
//! 什么时候用得上：**只要拿得到本地文件路径**，这条路就比问播放器更准更全 ——
//! 内嵌封面通常是原始大图（播放器给的常常是缩略图），内嵌歌词也常常比在线库更贴曲。
//! Linux 的 MPRIS 会把 `xesam:url` 给成 `file:///...`，所以 Linux 上收益最大；
//! macOS（MediaRemote）与 Windows（GSMTC）通常不给路径，这条路自然就用不上。

use std::path::Path;

use lofty::file::TaggedFileExt;
use lofty::prelude::Accessor;
use lofty::probe::Probe;
use lofty::tag::ItemKey;

use crate::error::{BridgeError, Result};

/// 从音频文件里读到的内嵌信息。
#[derive(Debug, Clone, Default)]
pub struct EmbeddedTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub year: Option<u32>,
    pub track_number: Option<u32>,
    /// 内嵌歌词（可能是 LRC，也可能是纯文本）
    pub lyrics: Option<String>,
    /// 内嵌封面（MIME, 字节）
    pub picture: Option<(String, Vec<u8>)>,
}

impl EmbeddedTags {
    /// 有没有任何可用内容（都没读到就别往下传）。
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.artist.is_none()
            && self.album.is_none()
            && self.lyrics.is_none()
            && self.picture.is_none()
    }
}

/// 读取内嵌标签。文件不存在 / 格式不支持时返回 `Ok(None)`（「没有」不等于「出错」）。
pub fn read_embedded(path: &Path) -> Result<Option<EmbeddedTags>> {
    if !path.exists() {
        return Ok(None);
    }
    let tagged = match Probe::open(path).and_then(|p| p.read()) {
        Ok(t) => t,
        Err(e) => {
            // 解析失败：可能是容器格式不认识、也可能是文件坏了 —— 不当致命错误
            return Err(BridgeError::other(format!("读取内嵌标签失败（{}）：{e}", path.display())));
        }
    };
    let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) else {
        return Ok(None);
    };

    let mut out = EmbeddedTags {
        title: tag.title().map(|c| c.into_owned()),
        artist: tag.artist().map(|c| c.into_owned()),
        album: tag.album().map(|c| c.into_owned()),
        album_artist: tag.get_string(ItemKey::AlbumArtist).map(str::to_string),
        genre: tag.genre().map(|c| c.into_owned()),
        // Accessor 只有 date（没有 year）；date 是可能只带年份的 Timestamp
        year: tag.date().map(|d| d.year.max(0) as u32),
        track_number: tag.track(),
        // Lyrics = 带时间轴的歌词（USLT / ©lyr / LYRICS）；UnsyncLyrics = 纯文本歌词
        lyrics: tag
            .get_string(ItemKey::Lyrics)
            .or_else(|| tag.get_string(ItemKey::UnsyncLyrics))
            .map(str::to_string),
        picture: None,
    };

    if let Some(pic) = tag.pictures().first() {
        let mime = pic
            .mime_type()
            .map(|m| m.to_string())
            .unwrap_or_else(|| "image/jpeg".to_string());
        let data = pic.data();
        if !data.is_empty() {
            out.picture = Some((mime, data.to_vec()));
        }
    }

    Ok(Some(out))
}

/// 只取内嵌封面（省掉其它字段的读取开销）。
pub fn read_embedded_picture(path: &Path) -> Option<(String, Vec<u8>)> {
    read_embedded(path).ok().flatten().and_then(|t| t.picture)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_not_an_error() {
        let p = std::env::temp_dir().join("definitely-not-here-12345.flac");
        assert!(read_embedded(&p).unwrap().is_none());
    }

    #[test]
    fn garbage_file_reports_error_not_panic() {
        let dir = std::env::temp_dir().join(format!(
            "mb-tags-{}",
            crate::util::short_hash(&crate::util::uuid_v4_ish())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("garbage.mp3");
        std::fs::write(&f, b"this is not audio").unwrap();
        // 要么认出「没有标签」，要么报错 —— 但不许 panic
        let _ = read_embedded(&f);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
