//! 封面缓存：把播放器给的封面字节落到磁盘，并维护「当前这张」。
//!
//! 为什么一定要落盘而不是留在内存里：消费端常常是**另一个进程 / 沙箱页面**，
//! 它们要的是一个能直接读/能当 URL 用的东西，而不是一串 base64（每帧几 MB 的
//! JSON 会拖垮 IPC，旧实现踩过这个坑）。
//!
//! 三个约定：
//!   - **按魔数定类型**，不信播放器自称的 MIME（存错后缀 = 浏览器不显示）。
//!   - **内容定名**：文件名带内容指纹，所以「同一个文件永远同一份内容」，
//!     消费端读到的文件不会被就地改写（避免读到一半的图）。
//!   - **换曲才换图**：`key` 不变就不重写（播放器每秒都重发同一张封面的字节，
//!     不去重就是每秒写一次盘）。

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::error::{BridgeError, Result};
use crate::types::{Artwork, ArtworkOrigin};
use crate::util::{ext_for_mime, image_size, normalize_mime, short_hash, sniff_image_mime};

/// 默认保留的历史封面数量（`<dir>/covers/` 里）。
const DEFAULT_KEEP: usize = 8;

/// 封面缓存。
pub struct ArtworkCache {
    dir: PathBuf,
    keep: usize,
    current: RwLock<Option<Artwork>>,
}

impl ArtworkCache {
    /// `dir` 是缓存根目录（内部会建 `covers/` 子目录）。
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self::with_keep(dir, DEFAULT_KEEP)
    }

    pub fn with_keep(dir: impl Into<PathBuf>, keep: usize) -> Self {
        let dir = dir.into();
        let _ = std::fs::create_dir_all(dir.join("covers"));
        Self {
            dir,
            keep: keep.max(1),
            current: RwLock::new(None),
        }
    }

    /// 缓存目录。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn covers_dir(&self) -> PathBuf {
        self.dir.join("covers")
    }

    /// 当前封面（`None` = 还没拿到或已清空）。
    pub fn current(&self) -> Option<Artwork> {
        self.current.read().ok().and_then(|g| g.clone())
    }

    /// 当前封面的 `key`（用于判断要不要重新取）。
    pub fn current_key(&self) -> Option<String> {
        self.current.read().ok().and_then(|g| g.as_ref().map(|a| a.key.clone()))
    }

    /// 存一张封面。
    ///
    /// - `key` 与当前封面相同的直接返回现有结果（不写盘）；
    /// - 类型按魔数嗅探，嗅不出且 `mime_hint` 也不是图片则返回 `Unsupported`
    ///   （**宁可不显示，也不要给消费端一个解码不了的破图**）；
    /// - 落盘用「临时文件 + rename」，保证消费端读到的永远是完整文件。
    pub fn store(
        &self,
        key: &str,
        mime_hint: Option<&str>,
        bytes: &[u8],
        origin: ArtworkOrigin,
        source_url: Option<String>,
    ) -> Result<Artwork> {
        if bytes.is_empty() {
            return Err(BridgeError::unavailable("封面数据为空"));
        }
        // 已经存过同一张：直接复用（播放器每次轮询都会重发同样的字节）
        if let Some(cur) = self.current()
            && cur.key == key
        {
            return Ok(cur);
        }

        let hint = mime_hint.and_then(normalize_mime);
        let mime = match sniff_image_mime(bytes) {
            Some(m) => m.to_string(),
            None => match hint {
                Some(h) if h.starts_with("image/") => h,
                _ => {
                    return Err(BridgeError::Unsupported(format!(
                        "无法识别封面格式（自称 {mime_hint:?}，前 4 字节 {:02X?}）",
                        &bytes[..bytes.len().min(4)]
                    )));
                }
            },
        };

        let ext = ext_for_mime(&mime);
        let name = format!("cover-{}.{}", short_hash(&format!("{key}|{mime}|{}", bytes.len())), ext);
        let dir = self.covers_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(&name);

        if !path.exists() {
            // 临时文件名必须**每次调用都不同**（进程 id + 计数器）：两个进程/两个线程同时
            // 存同一张封面时，若共用同一个临时名，先 rename 的那个会把临时文件「拿走」，
            // 后 rename 的直接 ENOENT 失败 —— 现象是「有时没封面」，极难复现。
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp = dir.join(format!(".{name}.{}.{seq}.tmp", std::process::id()));
            std::fs::write(&tmp, bytes)?;
            // rename 是原子的：消费端要么看到旧文件、要么看到完整的新文件，不会读到半张图。
            // 若另一个进程抢先建好了同名文件，rename 仍然成功（覆盖同内容），不是错误。
            match std::fs::rename(&tmp, &path) {
                Ok(()) => {}
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    if !path.exists() {
                        return Err(BridgeError::Io(e));
                    }
                }
            }
            let _ = self.prune();
        }

        let (width, height) = match image_size(bytes) {
            Some((w, h)) => (Some(w), Some(h)),
            None => (None, None),
        };
        let art = Artwork {
            key: key.to_string(),
            mime,
            bytes: bytes.len() as u64,
            width,
            height,
            origin,
            path: Some(path),
            http_path: None,
            source_url,
        };
        if let Ok(mut g) = self.current.write() {
            *g = Some(art.clone());
        }
        Ok(art)
    }

    /// 清掉当前封面（媒体停止时调用）。
    pub fn clear(&self) {
        if let Ok(mut g) = self.current.write() {
            *g = None;
        }
    }

    /// 按修改时间保留最近的 `keep` 个文件，其余删除。
    pub fn prune(&self) -> usize {
        let dir = self.covers_dir();
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return 0;
        };
        let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let md = e.metadata().ok()?;
                if !md.is_file() {
                    return None;
                }
                if p.extension().is_some_and(|x| x == "tmp") {
                    // 顺手清掉上次崩溃留下的临时文件
                    let _ = std::fs::remove_file(&p);
                    return None;
                }
                Some((md.modified().unwrap_or(std::time::UNIX_EPOCH), p))
            })
            .collect();
        if files.len() <= self.keep {
            return 0;
        }
        files.sort_by_key(|(t, _)| *t);
        let drop_count = files.len() - self.keep;
        let current_path = self.current().and_then(|a| a.path);
        let mut removed = 0;
        for (_, p) in files.into_iter().take(drop_count) {
            // 正在用的那张别删
            if Some(&p) == current_path.as_ref() {
                continue;
            }
            if std::fs::remove_file(&p).is_ok() {
                removed += 1;
            }
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::uuid_v4_ish;

    fn tmp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mb-art-{}", short_hash(&uuid_v4_ish())));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const PNG: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R', //
        0, 0, 0x01, 0x40, 0, 0, 0x00, 0xF0, 8, 6, 0, 0, 0,
    ];

    #[test]
    fn stores_file_with_sniffed_type_and_size() {
        let dir = tmp_dir();
        let cache = ArtworkCache::new(&dir);
        // 播放器自称 jpeg，实际是 PNG —— 必须按 PNG 存
        let art = cache.store("k1", Some("image/jpeg"), PNG, ArtworkOrigin::Player, None).unwrap();
        assert_eq!(art.mime, "image/png");
        assert_eq!((art.width, art.height), (Some(320), Some(240)));
        let path = art.path.clone().unwrap();
        assert!(path.exists());
        assert_eq!(path.extension().unwrap(), "png", "后缀必须跟着嗅探结果");
        assert_eq!(std::fs::read(&path).unwrap(), PNG);
        assert_eq!(cache.current().unwrap().key, "k1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_key_does_not_rewrite_the_file() {
        let dir = tmp_dir();
        let cache = ArtworkCache::new(&dir);
        let a = cache.store("k1", None, PNG, ArtworkOrigin::Player, None).unwrap();
        let mtime = std::fs::metadata(a.path.as_ref().unwrap()).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = cache.store("k1", None, PNG, ArtworkOrigin::Player, None).unwrap();
        assert_eq!(a.path, b.path);
        let mtime2 = std::fs::metadata(b.path.as_ref().unwrap()).unwrap().modified().unwrap();
        assert_eq!(mtime, mtime2, "同一个 key 不应重新写盘");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_key_switches_current_and_keeps_old_file_readable() {
        let dir = tmp_dir();
        let cache = ArtworkCache::new(&dir);
        let a = cache.store("k1", None, PNG, ArtworkOrigin::Player, None).unwrap();
        let b = cache.store("k2", None, PNG, ArtworkOrigin::Player, None).unwrap();
        assert_ne!(a.path, b.path, "不同 key 用不同文件（避免就地改写）");
        assert!(a.path.as_ref().unwrap().exists(), "旧文件仍在（消费端可能还在读）");
        assert_eq!(cache.current().unwrap().key, "k2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_unidentifiable_bytes_instead_of_writing_a_broken_image() {
        let dir = tmp_dir();
        let cache = ArtworkCache::new(&dir);
        let err = cache
            .store("k", Some("application/octet-stream"), b"not an image at all", ArtworkOrigin::Player, None)
            .unwrap_err();
        assert_eq!(err.code(), "unsupported");
        assert!(cache.current().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_bytes_are_rejected() {
        let dir = tmp_dir();
        let cache = ArtworkCache::new(&dir);
        assert!(cache.store("k", None, b"", ArtworkOrigin::Player, None).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_keeps_recent_and_never_drops_current() {
        let dir = tmp_dir();
        let cache = ArtworkCache::with_keep(&dir, 3);
        for i in 0..6 {
            let mut png = PNG.to_vec();
            png.extend_from_slice(&[i as u8; 8]); // 内容不同 → 不同文件名
            cache
                .store(&format!("k{i}"), None, &png, ArtworkOrigin::Player, None)
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let files: Vec<_> = std::fs::read_dir(cache.covers_dir()).unwrap().flatten().collect();
        assert!(files.len() <= 3, "应只保留 3 个，实际 {}", files.len());
        let cur = cache.current().unwrap().path.unwrap();
        assert!(cur.exists(), "当前封面必须还在");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_resets_current() {
        let dir = tmp_dir();
        let cache = ArtworkCache::new(&dir);
        cache.store("k", None, PNG, ArtworkOrigin::Player, None).unwrap();
        cache.clear();
        assert!(cache.current().is_none());
        assert!(cache.current_key().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
