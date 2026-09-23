//! Windows 后端：**GSMTC**（`Windows.Media.Control`，WinRT）。
//!
//! GSMTC 就是「音量键弹出来的那个媒体面板」背后的接口：任何注册了系统媒体会话的播放器
//! （Spotify、foobar2000、各大浏览器、甚至 PowerPoint）都能被它读到和控制。所以这条路
//! 不需要任何第三方依赖，Windows 10 1809+ 开箱可用。
//!
//! ## 为什么是「专用线程 + 消息通道」而不是直接在 async 里调
//!
//! WinRT 的接口对象要求**同一个 COM 公寓**里使用，而 `tokio` 的阻塞线程池不保证
//! 每次给你同一条线程（`spawn_blocking` 可能落在不同线程上）。因此这里起一条专属工作线程：
//! 建一次会话管理器，之后所有调用都排进它自己的队列 —— 公寓亲和性问题从根上消失。
//! `RoInitialize(MULTITHREADED)` 也只在这条线程上调用一次。
//!
//! ## 诚实说明：音量/静音在 Windows 上不提供
//!
//! GSMTC **没有**音量接口（系统音量要 WASAPI 端点音量，per-app 音量要音频会话 API，
//! 两者语义都不一样、也都有各自的坑）。所以 `set-volume` / `set-mute` 在 Windows 上
//! 明确返回「不支持」，而不是随便改一个不是用户期望的音量。

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession, GlobalSystemMediaTransportControlsSessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus,
};
use windows::Media::MediaPlaybackAutoRepeatMode;
use windows::Storage::Streams::{DataReader, IInputStream};
use windows::core::Interface;
// windows-future 0.3 起，「阻塞等待 IAsyncOperation」的方法是 `join()`
// （0.2 时代叫 `get()`，且当时要引 trait）；现在它是 IAsyncOperation 上的固有方法。

use crate::error::{BridgeError, Result};
use crate::types::{
    ArtworkOrigin, Capabilities, ControlOutcome, LoopMode, Playback, PlaybackState, PositionSource,
    SourceState, SourceStatus, Track, TrackSource, TransportCommand,
};
use crate::util::{now_ms, truncate};

use super::{RawArtwork, RawNowPlaying};

/// 封面最大接受大小（超过就当播放器给了张离谱的图，跳过）。
const MAX_THUMBNAIL_BYTES: u64 = 32 * 1024 * 1024;

/// Windows 会话（对外接口与另两个平台一致）。
#[derive(Clone)]
pub struct WindowsSession {
    inner: Arc<Inner>,
}

struct Inner {
    tx: Sender<Job>,
    /// 工作线程最近一次失败的说明
    last_error: Mutex<Option<String>>,
    /// 工作线程是否已就绪（首次建管理器成功）
    ready: std::sync::atomic::AtomicBool,
}

enum Job {
    Snapshot(tokio::sync::oneshot::Sender<Result<RawNowPlaying>>),
    /// 同步取快照（`diagnose` 与 CLI 的一次性命令用）。
    ///
    /// 刻意用 `std::sync::mpsc` 而不是 tokio 的 oneshot：调用方（`Session::diagnose`）
    /// 常常正跑在 tokio 运行时线程上，那里 `Receiver::blocking_recv()` 会**直接 panic**
    /// （`Cannot block the current thread from within a runtime`）—— 真机 Windows 实测踩到。
    SnapshotBlocking(std::sync::mpsc::Sender<Result<RawNowPlaying>>),
    Control(TransportCommand, tokio::sync::oneshot::Sender<Result<ControlOutcome>>),
}

impl WindowsSession {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        let inner = Arc::new(Inner {
            tx,
            last_error: Mutex::new(None),
            ready: std::sync::atomic::AtomicBool::new(false),
        });
        let worker_inner = inner.clone();
        std::thread::Builder::new()
            .name("media-bridge-gsmtc".to_string())
            .spawn(move || worker(rx, worker_inner))
            .ok();
        Self { inner }
    }

    pub fn status(&self) -> SourceStatus {
        if let Some(e) = self.inner.last_error.lock().ok().and_then(|g| g.clone()) {
            return SourceStatus::new("metadata", SourceState::Unavailable, e);
        }
        if self.inner.ready.load(std::sync::atomic::Ordering::Relaxed) {
            SourceStatus::new("metadata", SourceState::Running, "GSMTC（系统媒体会话）")
        } else {
            SourceStatus::new("metadata", SourceState::Preparing, "正在建立系统媒体会话…")
        }
    }

    pub fn diagnose(&self) -> Vec<String> {
        let mut out = vec!["会话后端：GSMTC（Windows.Media.Control）".to_string()];
        match self.sync_snapshot() {
            Ok(snap) if snap.has_media() => {
                if let Some(t) = &snap.now.track {
                    out.push(format!("当前曲目：{} — {}（{}）", t.artist, t.title, t.album));
                }
                out.push(format!(
                    "播放态：{:?}，位置 {:.1}s / {:.1}s，循环 {:?}",
                    snap.now.playback.state,
                    snap.now.playback.position_ms as f64 / 1000.0,
                    snap.now.playback.duration_ms as f64 / 1000.0,
                    snap.now.playback.loop_mode
                ));
                out.push(format!(
                    "封面：{}",
                    match &snap.artwork_raw {
                        Some(a) => format!("{} 字节", a.bytes.len()),
                        None => "播放器未提供".to_string(),
                    }
                ));
            }
            Ok(_) => out.push("当前没有正在播放的媒体（属正常）".to_string()),
            Err(e) => out.push(format!("查询失败：{e}")),
        }
        out
    }

    /// 同步拿一次快照（`diagnose` 与 CLI 一次性命令用）。
    fn sync_snapshot(&self) -> Result<RawNowPlaying> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.inner
            .tx
            .send(Job::SnapshotBlocking(tx))
            .map_err(|_| BridgeError::other("GSMTC 工作线程已退出"))?;
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(result) => result,
            Err(_) => Err(BridgeError::other("GSMTC 工作线程 5 秒内未回执")),
        }
    }

    pub async fn snapshot(&self) -> Result<RawNowPlaying> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.inner
            .tx
            .send(Job::Snapshot(tx))
            .map_err(|_| BridgeError::other("GSMTC 工作线程已退出"))?;
        rx.await.map_err(|_| BridgeError::other("GSMTC 工作线程未回执"))?
    }

    pub async fn control(&self, cmd: TransportCommand) -> Result<ControlOutcome> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.inner
            .tx
            .send(Job::Control(cmd, tx))
            .map_err(|_| BridgeError::other("GSMTC 工作线程已退出"))?;
        rx.await.map_err(|_| BridgeError::other("GSMTC 工作线程未回执"))?
    }
}

impl Default for WindowsSession {
    fn default() -> Self {
        Self::new()
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// 工作线程
// ══════════════════════════════════════════════════════════════════════════════

fn worker(rx: Receiver<Job>, inner: Arc<Inner>) {
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};

    // 公寓初始化：这条线程上只做一次
    unsafe {
        // 已经初始化过会返回 S_FALSE / RPC_E_CHANGED_MODE，都不影响继续用
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }

    let mut manager: Option<GlobalSystemMediaTransportControlsSessionManager> = None;
    while let Ok(job) = rx.recv() {
        match job {
            Job::Snapshot(reply) => {
                let m = ensure_manager(&mut manager, &inner);
                let out = match m {
                    Ok(m) => snapshot_with(m),
                    Err(e) => Err(e),
                };
                let _ = reply.send(out);
            }
            Job::SnapshotBlocking(reply) => {
                let m = ensure_manager(&mut manager, &inner);
                let out = match m {
                    Ok(m) => snapshot_with(m),
                    Err(e) => Err(e),
                };
                let _ = reply.send(out);
            }
            Job::Control(cmd, reply) => {
                let m = ensure_manager(&mut manager, &inner);
                let out = match m {
                    Ok(m) => control_with(m, &cmd),
                    Err(e) => Err(e),
                };
                let _ = reply.send(out);
            }
        }
    }
}

fn ensure_manager<'a>(
    slot: &'a mut Option<GlobalSystemMediaTransportControlsSessionManager>,
    inner: &Arc<Inner>,
) -> Result<&'a GlobalSystemMediaTransportControlsSessionManager> {
    if slot.is_none() {
        match GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
            .and_then(|op| op.join())
        {
            Ok(m) => {
                inner.ready.store(true, std::sync::atomic::Ordering::Relaxed);
                *slot = Some(m);
            }
            Err(e) => {
                let msg = format!(
                    "建立 GSMTC 会话管理器失败：{}（需要 Windows 10 1809+，且不在受限会话里）",
                    truncate(&e.to_string(), 200)
                );
                if let Ok(mut g) = inner.last_error.lock() {
                    *g = Some(msg.clone());
                }
                return Err(BridgeError::unavailable(msg));
            }
        }
    }
    Ok(slot.as_ref().expect("刚刚已填充"))
}

/// 挑一个会话：优先「正在播放」，其次「有标题」，最后退回 `GetCurrentSession`。
fn pick_session(
    manager: &GlobalSystemMediaTransportControlsSessionManager,
) -> Result<GlobalSystemMediaTransportControlsSession> {
    let mut best: Option<(i32, GlobalSystemMediaTransportControlsSession)> = None;
    if let Ok(sessions) = manager.GetSessions() {
        for s in iter_vector(&sessions) {
            let playing = s
                .GetPlaybackInfo()
                .ok()
                .and_then(|i| i.PlaybackStatus().ok())
                .map(|st| st == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing)
                .unwrap_or(false);
            let has_title = s
                .TryGetMediaPropertiesAsync()
                .and_then(|op| op.join())
                .ok()
                .and_then(|p| p.Title().ok())
                .map(|t| !t.is_empty())
                .unwrap_or(false);
            let score = if playing {
                2
            } else if has_title {
                1
            } else {
                0
            };
            if best.as_ref().is_none_or(|(s, _)| score > *s) {
                best = Some((score, s));
            }
        }
    }
    if let Some((_, s)) = best {
        return Ok(s);
    }
    manager
        .GetCurrentSession()
        .map_err(|_| BridgeError::NoMedia)
}

fn snapshot_with(
    manager: &GlobalSystemMediaTransportControlsSessionManager,
) -> Result<RawNowPlaying> {
    let now = now_ms();
    let session = match pick_session(manager) {
        Ok(s) => s,
        Err(_) => return Ok(RawNowPlaying::empty(now)),
    };

    let info = session.GetPlaybackInfo().ok();
    let status = info
        .as_ref()
        .and_then(|i| i.PlaybackStatus().ok())
        .unwrap_or(GlobalSystemMediaTransportControlsSessionPlaybackStatus::Closed);
    let state = match status {
        s if s == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing => {
            PlaybackState::Playing
        }
        s if s == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Paused => {
            PlaybackState::Paused
        }
        s if s == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Stopped => {
            PlaybackState::Stopped
        }
        _ => PlaybackState::Unknown,
    };

    let props = session
        .TryGetMediaPropertiesAsync()
        .and_then(|op| op.join())
        .map_err(|e| BridgeError::other(format!("读取媒体属性失败：{}", truncate(&e.to_string(), 160))))?;

    let title = props.Title().map(|s| s.to_string()).unwrap_or_default();
    let artist = props.Artist().map(|s| s.to_string()).unwrap_or_default();
    let album = props.AlbumTitle().map(|s| s.to_string()).unwrap_or_default();
    let album_artist = props.AlbumArtist().map(|s| s.to_string()).unwrap_or_default();
    let track_number = props.TrackNumber().ok().filter(|n| *n > 0).map(|n| n as u32);
    let genre = props
        .Genres()
        .ok()
        .and_then(|g| iter_vector(&g).into_iter().next())
        .map(|s| s.to_string())
        .unwrap_or_default();

    // 时间轴：TimeSpan 的单位是 100 纳秒
    let (position_ms, duration_ms, position_source) = match session.GetTimelineProperties() {
        Ok(tl) => {
            let start = tl.StartTime().map(|t| t.Duration).unwrap_or(0);
            let end = tl.EndTime().map(|t| t.Duration).unwrap_or(0);
            let pos = tl.Position().map(|t| t.Duration).unwrap_or(0);
            let dur = if end > start { ((end - start) / 10_000) as u64 } else { 0 };
            (((pos.max(0) / 10_000) as u64), dur, PositionSource::Polled)
        }
        Err(_) => (0, 0, PositionSource::Unavailable),
    };

    // 注意：AutoRepeatMode / IsShuffleActive / PlaybackRate 返回的是 `IReference<T>`
    // （WinRT 的可空标量），要 `.Value()` 才拿到真正的值。
    let loop_mode = info
        .as_ref()
        .and_then(|i| i.AutoRepeatMode().ok())
        .and_then(|r| r.Value().ok())
        .map(auto_repeat_to_loop)
        .unwrap_or(LoopMode::Unknown);
    let shuffle = info
        .as_ref()
        .and_then(|i| i.IsShuffleActive().ok())
        .and_then(|r| r.Value().ok());
    let rate = info
        .as_ref()
        .and_then(|i| i.PlaybackRate().ok())
        .and_then(|r| r.Value().ok())
        .unwrap_or(1.0);

    let caps = match info.as_ref().and_then(|i| i.Controls().ok()) {
        Some(c) => Capabilities {
            play: c.IsPlayEnabled().unwrap_or(false),
            pause: c.IsPauseEnabled().unwrap_or(false),
            toggle: c.IsPlayPauseToggleEnabled().unwrap_or(false),
            stop: c.IsStopEnabled().unwrap_or(false),
            next: c.IsNextEnabled().unwrap_or(false),
            previous: c.IsPreviousEnabled().unwrap_or(false),
            seek_absolute: c.IsPlaybackPositionEnabled().unwrap_or(false),
            seek_relative: c.IsPlaybackPositionEnabled().unwrap_or(false),
            set_loop: c.IsRepeatEnabled().unwrap_or(false),
            set_shuffle: c.IsShuffleEnabled().unwrap_or(false),
            // GSMTC 没有音量/静音接口（见文件头的说明）
            set_volume: false,
            set_mute: false,
            set_rate: c.IsPlaybackRateEnabled().unwrap_or(false),
        },
        None => Capabilities::BASIC,
    };

    let app_id = session
        .SourceAppUserModelId()
        .map(|s| s.to_string())
        .ok()
        .filter(|s| !s.is_empty());

    let playback = Playback {
        state,
        position_ms,
        position_source,
        duration_ms,
        rate,
        volume: None,
        muted: None,
        loop_mode,
        shuffle,
        updated_at_ms: now,
    };

    let track = Track {
        id: {
            let base = format!("{artist}|{title}|{album}");
            if base.trim_matches('|').is_empty() {
                app_id.clone().unwrap_or_default()
            } else {
                base
            }
        },
        title,
        artist,
        album,
        album_artist,
        genre,
        composer: String::new(),
        year: None,
        track_number,
        duration_ms,
        artwork: None,
        lyrics: None,
        source: TrackSource {
            provider: "windows-gsmtc".to_string(),
            app_name: app_id.as_deref().map(friendly_app_name),
            app_id,
            pid: None,
            // GSMTC 不提供本地文件路径（同 macOS）
            file_path: None,
            url: None,
        },
    };

    let mut raw = RawNowPlaying::from_parts(Some(track), playback, caps, now);
    // 「播放器没给缩略图」和「给了但我们没读到」是两件完全不同的事，但对外都表现为「没封面」。
    // 后者是我们自己的 bug，所以这里至少提示一次，别让它静默消失。
    if let Some(t) = raw.now.track.as_ref() {
        match props.Thumbnail() {
            Ok(thumb) => match read_thumbnail(&thumb) {
                Some((bytes, mime)) => {
                    raw.artwork_raw = Some(RawArtwork {
                        key: t.id.clone(),
                        mime_hint: mime,
                        bytes,
                        origin: ArtworkOrigin::Player,
                        source_url: None,
                    });
                }
                None => {
                    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                    if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        eprintln!(
                            "[windows] 播放器提供了 GSMTC 缩略图，但读取失败（只提示一次；这是中间件侧的问题）"
                        );
                    }
                }
            },
            Err(_) => { /* 播放器根本没提供缩略图：正常情况，不提示 */ }
        }
    }
    Ok(raw)
}

/// 把 AUMID 变成给人看的应用名。
///
/// AUMID 形如 `Spotify.exe` 或 `Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic`，
/// 直接显示很难读；这里取 `!` 之后那段、去掉 `.exe` 后缀、再取最后一个点号段。
///
/// **顺序很关键**：必须先去掉 `.exe` 再取点号段。反过来的话 `Spotify.exe` 会变成 `exe`
/// （这个 bug 在本机 macOS 上编译不到这段代码，是真机跑 Windows 测试时才暴露的）。
fn friendly_app_name(aumid: &str) -> String {
    let last = aumid.rsplit('!').next().unwrap_or(aumid);
    let no_ext = last
        .strip_suffix(".exe")
        .or_else(|| last.strip_suffix(".EXE"))
        .unwrap_or(last);
    let name = no_ext.rsplit('.').next().unwrap_or(no_ext);
    if name.is_empty() {
        aumid.to_string()
    } else {
        name.to_string()
    }
}

fn read_thumbnail(thumb: &windows::Storage::Streams::IRandomAccessStreamReference) -> Option<(Vec<u8>, Option<String>)> {
    let stream = thumb.OpenReadAsync().ok()?.join().ok()?;
    let size = stream.Size().ok()?;
    if size == 0 || size > MAX_THUMBNAIL_BYTES {
        return None;
    }
    let mime = stream.ContentType().ok().map(|s| s.to_string());
    let reader = DataReader::CreateDataReader(&stream.cast::<IInputStream>().ok()?).ok()?;
    reader.LoadAsync(size as u32).ok()?.join().ok()?;
    let mut buf = vec![0u8; size as usize];
    reader.ReadBytes(&mut buf).ok()?;
    Some((buf, mime))
}

fn control_with(
    manager: &GlobalSystemMediaTransportControlsSessionManager,
    cmd: &TransportCommand,
) -> Result<ControlOutcome> {
    let session = match pick_session(manager) {
        Ok(s) => s,
        Err(_) => return Err(BridgeError::NoMedia),
    };
    let name = cmd.name();

    // GSMTC 的每个 Try* 都返回「请求是否被接受」的 bool —— 这比「发出去了」诚实得多
    let finish = |res: windows::core::Result<windows_future::IAsyncOperation<bool>>| -> ControlOutcome {
        match res {
            Ok(op) => match op.join() {
                Ok(true) => ControlOutcome::ok(name),
                Ok(false) => ControlOutcome::rejected(name, "播放器拒绝了该请求（Try*Async 返回 false）"),
                Err(e) => ControlOutcome::rejected(name, format!("请求失败：{}", truncate(&e.to_string(), 120))),
            },
            Err(e) => ControlOutcome::rejected(name, format!("调用失败：{}", truncate(&e.to_string(), 120))),
        }
    };

    match cmd {
        TransportCommand::Play => Ok(finish(session.TryPlayAsync())),
        TransportCommand::Pause => Ok(finish(session.TryPauseAsync())),
        TransportCommand::PlayPause => Ok(finish(session.TryTogglePlayPauseAsync())),
        TransportCommand::Stop => Ok(finish(session.TryStopAsync())),
        TransportCommand::Next => Ok(finish(session.TrySkipNextAsync())),
        TransportCommand::Previous => Ok(finish(session.TrySkipPreviousAsync())),

        TransportCommand::Seek { position_ms } => {
            let ticks = (*position_ms as i64).saturating_mul(10_000);
            Ok(finish(session.TryChangePlaybackPositionAsync(ticks)))
        }
        TransportCommand::SeekBy { delta_ms } => {
            // GSMTC 只有绝对定位：先读当前，再算目标
            let cur = session
                .GetTimelineProperties()
                .ok()
                .and_then(|t| t.Position().ok())
                .map(|p| p.Duration / 10_000)
                .unwrap_or(0);
            let target = (cur + *delta_ms).max(0);
            let ticks = target.saturating_mul(10_000);
            let out = finish(session.TryChangePlaybackPositionAsync(ticks));
            Ok(out)
        }
        TransportCommand::SetLoop { mode } => {
            let target = match mode {
                LoopMode::Off | LoopMode::Unknown => MediaPlaybackAutoRepeatMode::None,
                LoopMode::Track => MediaPlaybackAutoRepeatMode::Track,
                LoopMode::Playlist => MediaPlaybackAutoRepeatMode::List,
            };
            Ok(finish(session.TryChangeAutoRepeatModeAsync(target)))
        }
        TransportCommand::CycleLoop => {
            let cur = session
                .GetPlaybackInfo()
                .ok()
                .and_then(|i| i.AutoRepeatMode().ok())
                .and_then(|r| r.Value().ok())
                .map(auto_repeat_to_loop)
                .unwrap_or(LoopMode::Off);
            let next = cur.next();
            let target = match next {
                LoopMode::Off | LoopMode::Unknown => MediaPlaybackAutoRepeatMode::None,
                LoopMode::Track => MediaPlaybackAutoRepeatMode::Track,
                LoopMode::Playlist => MediaPlaybackAutoRepeatMode::List,
            };
            let out = finish(session.TryChangeAutoRepeatModeAsync(target));
            Ok(ControlOutcome {
                action: name.to_string(),
                applied: out.applied,
                reason: out.reason,
                effective: Some(format!("loop={next:?}")),
            })
        }
        TransportCommand::SetShuffle { on } => Ok(finish(session.TryChangeShuffleActiveAsync(*on))),
        TransportCommand::ToggleShuffle => {
            let cur = session
                .GetPlaybackInfo()
                .ok()
                .and_then(|i| i.IsShuffleActive().ok())
                .and_then(|r| r.Value().ok())
                .unwrap_or(false);
            let out = finish(session.TryChangeShuffleActiveAsync(!cur));
            Ok(ControlOutcome {
                action: name.to_string(),
                applied: out.applied,
                reason: out.reason,
                effective: Some(format!("shuffle={}", !cur)),
            })
        }
        TransportCommand::SetRate { rate } => Ok(finish(session.TryChangePlaybackRateAsync(*rate))),
        TransportCommand::SetVolume { .. } => Ok(ControlOutcome::rejected(
            name,
            "GSMTC 没有音量接口；系统音量请走 WASAPI 端点音量，per-app 音量请走音频会话 API（本中间件不代劳，避免改了不是用户期望的音量）",
        )),
        TransportCommand::SetMute { .. } => Ok(ControlOutcome::rejected(
            name,
            "GSMTC 没有静音接口（同上）",
        )),
    }
}

/// `MediaPlaybackAutoRepeatMode` → 统一语义。
///
/// 枚举值来自头文件：`None = 0, Track = 1, List = 2`。
fn auto_repeat_to_loop(mode: MediaPlaybackAutoRepeatMode) -> LoopMode {
    if mode == MediaPlaybackAutoRepeatMode::None {
        LoopMode::Off
    } else if mode == MediaPlaybackAutoRepeatMode::Track {
        LoopMode::Track
    } else if mode == MediaPlaybackAutoRepeatMode::List {
        LoopMode::Playlist
    } else {
        LoopMode::Unknown
    }
}

/// 遍历 WinRT 的 `IVectorView`（`windows` crate 不直接给迭代器）。
fn iter_vector<T: windows::core::RuntimeType + Clone>(v: &windows_collections::IVectorView<T>) -> Vec<T> {
    let mut out = Vec::new();
    let size = v.Size().unwrap_or(0);
    for i in 0..size {
        if let Ok(item) = v.GetAt(i) {
            out.push(item);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aumid_becomes_readable_app_name() {
        assert_eq!(friendly_app_name("Spotify.exe"), "Spotify");
        assert_eq!(
            friendly_app_name("Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic"),
            "ZuneMusic"
        );
        assert_eq!(friendly_app_name("chrome.exe"), "chrome");
        assert_eq!(friendly_app_name("Music.EXE"), "Music", "大写后缀也要认");
        assert_eq!(friendly_app_name("foo.bar.exe"), "bar", "先剥 .exe 再取最后一段");
        assert_eq!(friendly_app_name(""), "");
        assert_eq!(
            friendly_app_name("Microsoft.WindowsMediaPlayer"),
            "WindowsMediaPlayer"
        );
    }

    #[test]
    fn repeat_mode_maps_to_unified_loop_mode() {
        assert_eq!(auto_repeat_to_loop(MediaPlaybackAutoRepeatMode::None), LoopMode::Off);
        assert_eq!(auto_repeat_to_loop(MediaPlaybackAutoRepeatMode::Track), LoopMode::Track);
        assert_eq!(auto_repeat_to_loop(MediaPlaybackAutoRepeatMode::List), LoopMode::Playlist);
        assert_eq!(auto_repeat_to_loop(MediaPlaybackAutoRepeatMode(9)), LoopMode::Unknown);
    }
}
