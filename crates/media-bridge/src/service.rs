//! 服务层：把平台会话、封面缓存、歌词、音频采集缝成**一个可长期运行的中间件**。
//!
//! ## 职责边界
//!
//! ```text
//!   平台会话（platform）   ── 只负责「问系统拿到原始事实」
//!        │
//!   服务层（service）      ── 轮询节拍、差异判定、封面落盘、歌词补全、事件广播、状态汇总
//!        │
//!   IPC（ipc）             ── 把同一份快照用 stdio / HTTP 两种线协议暴露出去
//! ```
//!
//! ## 轮询节拍
//!
//! 三平台统一用**轮询**（1s）作为基线，而不是各平台各写一套原生推送：
//! 路数少、行为可预测、跨平台表现一致。做法上做了三件事补偿轮询的实时性：
//!
//!   1. **位置外推**：快照带 `updatedAtMs`，消费端自己按速率算当前位置，不必调密轮询；
//!   2. **突发窗口**：任何控制命令发出后，短时间内按 200ms 高频轮询，让状态尽快收敛；
//!   3. **空闲降频**：没有媒体在放时降到 2s，省电（笔记本上这点很重要）。
//!
//! ## 事件只报「变化」
//!
//! 事件流里**不会**每秒刷一条 position：位置每秒都在变，那是外推能算出来的信息。
//! 只有「换曲」「播放态/循环/音量变了」「位置发生了外推解释不了的跳变（= seek）」
//! 才会发事件。这样订阅方可以放心把每条事件都当成「值得刷新 UI 的时机」。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::Serialize;

use crate::artwork::ArtworkCache;
use crate::audio::{AudioCapture, AudioConfig};
use crate::error::{BridgeError, Result};
use crate::lyrics::{LyricsQuery, LyricsResolver};
use crate::platform::{mock::MockConfig, Provider, RawNowPlaying, Session};
use crate::types::{
    Artwork, ControlOutcome, Lyrics, NowPlaying, SourceState, SourceStatus, Status,
    Track, TransportCommand,
};
use crate::util::now_ms;
use crate::VERSION;

/// 位置「跳变」判定阈值：外推预测值与实际值的差超过它，就算发生过 seek。
const POSITION_JUMP_MS: i64 = 2500;

/// 中间件配置。
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// 会话后端（Auto = 按平台自动选）
    pub provider: Provider,
    /// `provider = Mock` 时假播放器的配置
    pub mock: MockConfig,
    /// 缓存目录（封面 / 歌词）；`None` = 平台默认
    pub cache_dir: Option<PathBuf>,
    /// 播放中的轮询间隔
    pub poll_interval_ms: u64,
    /// 没有媒体时的轮询间隔
    pub idle_poll_interval_ms: u64,
    /// 控制命令后的突发轮询间隔
    pub burst_interval_ms: u64,
    /// 突发窗口长度
    pub burst_window_ms: u64,
    /// 音频采集配置
    pub audio: AudioConfig,
    /// 是否允许在线歌词（需要编译期 `online` feature，两者同时满足才生效）
    pub lyrics_online: bool,
    /// 开 HTTP 服务时，封面在快照里附带的相对地址（如 `/v1/artwork`）
    pub artwork_http_path: Option<String>,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            provider: Provider::default(),
            mock: MockConfig::default(),
            cache_dir: None,
            poll_interval_ms: crate::types::DEFAULT_POLL_MS,
            idle_poll_interval_ms: 2000,
            burst_interval_ms: 200,
            burst_window_ms: 3000,
            audio: AudioConfig::default(),
            lyrics_online: true,
            artwork_http_path: None,
        }
    }
}

impl BridgeConfig {
    /// 平台默认缓存目录（`~/Library/Caches/media-bridge` 等）。
    pub fn default_cache_dir() -> PathBuf {
        dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("media-bridge")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.cache_dir.clone().unwrap_or_else(Self::default_cache_dir)
    }
}

/// 事件流里的一条事件。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum Event {
    /// 曲目变化（含「从有到没有」）
    Track { now: NowPlaying },
    /// 播放态/循环/随机/音量等变化，或外推解释不了的位置跳变（seek）
    Playback { now: NowPlaying },
    /// 封面变化（已落盘，`artwork.path` 可直接用）
    Artwork { artwork: Artwork },
    /// 歌词补全（本地命中或在线命中；`lyrics = None` 表示查过但没有）
    Lyrics { track_id: String, lyrics: Option<Lyrics>, source: String },
    /// 状态变化（数据源可用性等）
    Status { status: Status },
    /// 频谱帧（只在订阅方明确要求时才推，默认不推）
    Spectrum { frame: crate::types::SpectrumFrame },
    /// 出错（按数据源分类，便于订阅方呈现）
    Error { source: String, code: String, message: String },
}

impl Event {
    /// 事件名（wire 上的 `event` 字段值）。
    pub fn name(&self) -> &'static str {
        match self {
            Self::Track { .. } => "track",
            Self::Playback { .. } => "playback",
            Self::Artwork { .. } => "artwork",
            Self::Lyrics { .. } => "lyrics",
            Self::Status { .. } => "status",
            Self::Spectrum { .. } => "spectrum",
            Self::Error { .. } => "error",
        }
    }
}

/// 控制命令的回执 + 命令之后的最新快照。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlReport {
    pub outcome: ControlOutcome,
    /// 命令执行后立即刷新到的状态（消费端不用再问一次）
    pub now: NowPlaying,
}

struct State {
    now: NowPlaying,
    metadata: SourceStatus,
    last_track_id: String,
    /// 已为哪首曲目发起过**在线**歌词查询（本地查询每轮都做，不需要标记）
    online_requested_for: String,
    /// 已尝试过读取内嵌封面的曲目（避免每秒重新解析音频文件）。
    /// 只有 `embedded` feature 会用它，其它构建里保留字段但没人读。
    #[cfg_attr(not(feature = "embedded"), allow(dead_code))]
    embedded_artwork_tried: String,
    /// 上次上报的「外推位置预期值」基准
    last_position_ms: u64,
    last_position_at_ms: u64,
    started_at_ms: u64,
    polls: u64,
}

/// 中间件主体。
pub struct MediaBridge {
    config: BridgeConfig,
    session: Session,
    artwork: ArtworkCache,
    /// `Arc` 是为了让「在线歌词」的后台任务能持有同一个取词器（共享备忘与错误状态），
    /// 而不是另起一个实例导致两份缓存各自为政。
    lyrics: Arc<LyricsResolver>,
    audio: Arc<AudioCapture>,
    state: RwLock<State>,
    events: tokio::sync::broadcast::Sender<Event>,
    /// 突发窗口截止时刻（Unix 毫秒）
    burst_until: AtomicU64,
    started: AtomicBool,
    /// 轮询任务句柄
    poller: RwLock<Option<tokio::task::JoinHandle<()>>>,
    /// 串行化 `apply()`。
    ///
    /// 「轮询任务」和「显式 refresh（`now` 命令、控制命令后的刷新）」会并发进来；
    /// 不加锁时两次 apply 会互相踩：一个先把「已处理」标记写进去，另一个看到标记就跳过解析、
    /// 转而沿用状态里的旧值 —— 症状是**歌词/封面偶发丢失**（实测在 Linux 上 5 次里丢 1 次）。
    apply_lock: tokio::sync::Mutex<()>,
}

impl MediaBridge {
    /// 构造（不启动任何采集 —— 懒启动是刻意的：没人用就不该申请麦克风/音频权限）。
    pub fn new(config: BridgeConfig) -> Arc<Self> {
        let cache_dir = config.cache_dir();
        let session = match config.provider {
            Provider::Mock => Session::mock(config.mock.clone()),
            Provider::Auto => Session::new(Provider::Auto),
        };
        let artwork = ArtworkCache::new(&cache_dir);
        let lyrics = Arc::new(LyricsResolver::new(&cache_dir, config.lyrics_online));
        let audio = AudioCapture::new(config.audio.clone());
        let (events, _) = tokio::sync::broadcast::channel(256);
        let metadata = session.status();
        Arc::new(Self {
            config,
            session,
            artwork,
            lyrics,
            audio,
            state: RwLock::new(State {
                now: NowPlaying::empty(now_ms()),
                metadata,
                last_track_id: String::new(),
                online_requested_for: String::new(),
                embedded_artwork_tried: String::new(),
                last_position_ms: 0,
                last_position_at_ms: 0,
                started_at_ms: now_ms(),
                polls: 0,
            }),
            events,
            burst_until: AtomicU64::new(0),
            started: AtomicBool::new(false),
            poller: RwLock::new(None),
            apply_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// 后端名（`macos-mediaremote` 等）。
    pub fn provider_name(&self) -> &'static str {
        self.session.provider_name()
    }

    pub fn config(&self) -> &BridgeConfig {
        &self.config
    }

    /// 订阅事件流。
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// 当前订阅者数量（诊断用）。
    pub fn subscriber_count(&self) -> usize {
        self.events.receiver_count()
    }

    /// 最新快照（克隆，调用方随便持有）。
    pub fn snapshot(&self) -> NowPlaying {
        self.state
            .read()
            .map(|g| g.now.clone())
            .unwrap_or_else(|_| NowPlaying::empty(now_ms()))
    }

    /// 最新快照，但位置按当前时钟**外推到此刻**（进度条用这个）。
    pub fn snapshot_at_now(&self) -> NowPlaying {
        let mut now = self.snapshot();
        let pos = now.playback.position_at(now_ms());
        now.playback.position_ms = pos;
        now.playback.position_source = crate::types::PositionSource::Interpolated;
        now
    }

    /// 启动轮询（幂等）。音频采集**不在这里**自动启动（见 [`MediaBridge::ensure_audio`]）。
    pub fn start(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = Arc::clone(self);
        let handle = tokio::spawn(async move { me.run_poller().await });
        if let Ok(mut g) = self.poller.write() {
            *g = Some(handle);
        }
    }

    /// 停止轮询与采集。
    pub fn stop(&self) {
        self.started.store(false, Ordering::SeqCst);
        if let Some(h) = self.poller.write().ok().and_then(|mut g| g.take()) {
            h.abort();
        }
        self.audio.stop();
    }

    /// 音频采集按需启动（第一次被问频谱/订阅 PCM 时才启动）。
    ///
    /// macOS 首次会触发「音频录制」权限弹窗 —— 这就是为什么它必须是按需的。
    pub fn ensure_audio(&self) -> Result<()> {
        self.audio.start()
    }

    pub fn audio_status(&self) -> SourceStatus {
        self.audio.status()
    }

    /// 最新频谱帧。
    pub fn spectrum(&self) -> crate::types::SpectrumFrame {
        self.audio.frame()
    }

    pub fn subscribe_pcm(&self) -> Option<tokio::sync::broadcast::Receiver<crate::audio::PcmChunk>> {
        self.audio.subscribe_pcm()
    }

    /// 当前曲目的歌词：先用本地（毫秒级），必要时按需查在线。
    pub fn lyrics(&self) -> Option<Lyrics> {
        self.state.read().ok().and_then(|g| g.now.track.as_ref()?.lyrics.clone())
    }

    /// 强制重新解析当前曲目的歌词（`lyrics sync`）。
    pub async fn refresh_lyrics(&self, online: bool) -> Option<Lyrics> {
        let query = self.current_lyrics_query()?;
        let local = self.lyrics.resolve_local(&query);
        if let Some(l) = local {
            self.attach_lyrics(&l);
            return Some(l);
        }
        if online {
            let l = self.lyrics.resolve_online(query).await;
            if let Some(l) = &l {
                self.attach_lyrics(l);
            }
            return l;
        }
        None
    }

    /// 取当前快照对应的歌词查询条件。
    fn current_lyrics_query(&self) -> Option<LyricsQuery> {
        let now = self.snapshot();
        let t = now.track.as_ref()?;
        Some(
            LyricsQuery::new(&t.title, &t.artist, &t.album)
                .with_duration(t.duration_ms)
                .with_file(t.source.file_path.clone()),
        )
    }

    /// 最新状态汇总。
    pub fn status(&self) -> Status {
        let state = self.state.read().ok();
        let (metadata, ticks, started) = match state.as_ref() {
            Some(g) => (g.metadata.clone(), g.polls, g.started_at_ms),
            None => (
                SourceStatus::new("metadata", SourceState::Error, "状态锁被污染"),
                0,
                0,
            ),
        };
        let mut sources = vec![metadata];
        sources.push(self.audio.status());
        let ls = self.lyrics.status();
        sources.push(SourceStatus::new(
            "lyrics",
            if ls.online_enabled { SourceState::Running } else { SourceState::Idle },
            match (&ls.last_error, ls.online_enabled) {
                (Some(e), _) => e.clone(),
                (None, true) => format!("本地 {} + 在线 LRCLIB", ls.local_sources.join("/")),
                (None, false) => format!("仅本地 {}", ls.local_sources.join("/")),
            },
        ));
        let art = self.artwork.current();
        sources.push(SourceStatus::new(
            "artwork",
            SourceState::Running,
            match &art {
                Some(a) => format!("{}（{} 字节，{:?}）", a.mime, a.bytes, a.origin),
                None => format!("缓存目录 {}", self.artwork.dir().display()),
            },
        ));

        let mut features = vec![];
        if cfg!(feature = "http") {
            features.push("http".to_string());
        }
        if cfg!(feature = "audio") {
            features.push("audio".to_string());
        }
        if cfg!(feature = "embedded") {
            features.push("embedded".to_string());
        }
        if cfg!(feature = "online") {
            features.push("online".to_string());
        }

        Status {
            platform: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            version: VERSION.to_string(),
            provider: self.provider_name().to_string(),
            sources,
            features,
            cache_dir: self.config.cache_dir().display().to_string(),
            poll_interval_ms: self.effective_poll_ms(),
            subscribers: self.subscriber_count(),
            polls: ticks,
            uptime_ms: now_ms().saturating_sub(started),
        }
    }

    /// 发一条传输控制命令（第 2 期），并立刻回读到新状态。
    pub async fn control(&self, cmd: TransportCommand) -> Result<ControlReport> {
        let outcome = self.session.control(cmd).await?;
        // 开突发窗口：命令发出后状态可能在几十毫秒内才真正生效
        let until = now_ms() + self.config.burst_window_ms;
        self.burst_until.store(until, Ordering::Relaxed);
        // 立即刷新一次（让调用方拿到「命令之后」的状态，不用等下一个 tick）
        tokio::time::sleep(Duration::from_millis(self.config.burst_interval_ms.min(250))).await;
        if let Err(e) = self.poll_once().await {
            self.emit_error("metadata", &e);
        }
        Ok(ControlReport {
            outcome,
            now: self.snapshot_at_now(),
        })
    }

    /// 主动刷新一次（供 `refresh` 方法用）。
    pub async fn refresh(&self) -> Result<NowPlaying> {
        self.poll_once().await?;
        Ok(self.snapshot_at_now())
    }

    fn effective_poll_ms(&self) -> u64 {
        if now_ms() < self.burst_until.load(Ordering::Relaxed) {
            return self.config.burst_interval_ms;
        }
        let has_media = self
            .state
            .read()
            .map(|g| g.now.has_media)
            .unwrap_or(false);
        if has_media {
            self.config.poll_interval_ms
        } else {
            self.config.idle_poll_interval_ms
        }
    }

    /// 轮询主循环。
    async fn run_poller(self: Arc<Self>) {
        loop {
            if !self.started.load(Ordering::SeqCst) {
                break;
            }
            if let Err(e) = self.poll_once().await {
                self.emit_error("metadata", &e);
                // 数据源不可用：把状态同步出去，但**不退出**（可能只是播放器还没起来）
                if let Ok(mut g) = self.state.write() {
                    g.metadata = self.session.status();
                }
                let status = self.status();
                let _ = self.events.send(Event::Status { status });
                // 数据源出问题时也开一个短突发窗口，尽快恢复
                self.burst_until.store(now_ms() + 2000, Ordering::Relaxed);
            }
            let wait = self.effective_poll_ms().clamp(50, 10_000);
            tokio::time::sleep(Duration::from_millis(wait)).await;
        }
    }

    /// 取一次快照并把差异同步到状态 + 事件流。
    pub async fn poll_once(&self) -> Result<()> {
        let raw = self.session.snapshot().await?;
        self.apply(raw).await;
        Ok(())
    }

    /// 应用一次平台快照（串行化：见 `apply_lock` 的说明）。
    async fn apply(&self, mut raw: RawNowPlaying) {
        let _guard = self.apply_lock.lock().await;
        self.apply_inner(&mut raw).await;
    }

    async fn apply_inner(&self, raw: &mut RawNowPlaying) {
        let now_ms_value = now_ms();

        // 0) 播放器没给封面、但拿得到本地文件路径 → 试一次内嵌封面（`embedded` feature）。
        //    每首只试一次：解析音频文件不便宜，不能每秒做。
        #[cfg(feature = "embedded")]
        if raw.artwork_raw.is_none() {
            let track_id = raw.now.track.as_ref().map(|t| t.id.clone()).unwrap_or_default();
            let path = raw.now.track.as_ref().and_then(|t| t.source.file_path.clone());
            if let Some(path) = path
                && !track_id.is_empty()
                && self
                    .state
                    .read()
                    .map(|g| g.embedded_artwork_tried != track_id)
                    .unwrap_or(true)
            {
                if let Ok(mut g) = self.state.write() {
                    g.embedded_artwork_tried = track_id.clone();
                }
                if let Some((mime, bytes)) = crate::tags::read_embedded_picture(&path) {
                    // 全路径引用：只在 embedded feature 下编译，顶部导入会在别的构建里变成未使用
                    raw.artwork_raw = Some(crate::platform::RawArtwork {
                        key: track_id,
                        mime_hint: Some(mime),
                        bytes,
                        origin: crate::types::ArtworkOrigin::Embedded,
                        source_url: None,
                    });
                }
            }
        }

        // 1) 封面：先落盘（这样快照出去时 path 已经可用）。
        //    事件本身压后发 —— 消费端先知道「换曲了」，再收到「封面到了」，顺序更自然。
        let mut artwork_event: Option<Artwork> = None;
        if let Some(art) = raw.artwork_raw.take() {
            match self
                .artwork
                .store(&art.key, art.mime_hint.as_deref(), &art.bytes, art.origin, art.source_url)
            {
                Ok(cached) => {
                    let changed = self
                        .state
                        .read()
                        .ok()
                        .and_then(|g| g.now.track.as_ref()?.artwork.as_ref().map(|a| a.key.clone()))
                        != Some(cached.key.clone());
                    if let Some(t) = raw.now.track.as_mut() {
                        t.artwork = Some(self.decorate_artwork(cached.clone()));
                    }
                    if changed {
                        artwork_event = Some(cached);
                    }
                }
                Err(e) => {
                    // 嗅探不出类型的封面：不写盘、不显示，但要说清楚发生了什么
                    self.emit_error("artwork", &e);
                }
            }
        }

        // 1b) 「永不降级」：这一轮没有封面，但状态里同一首**已经有**封面 → 沿用。
        //
        // 不加这条会丢封面：播放器只在换曲时给一次封面字节，之后每轮的 raw 都是空的；
        // 而并发 apply（轮询 + refresh）里后到的那个会把 snapshot 的封面抹成空。
        // 歌词那边是同一个模式（见步骤 3 的注释）。
        if raw.now.track.as_ref().is_some_and(|t| t.artwork.is_none())
            && let Some(prev) = self.state.read().ok().and_then(|g| {
                let same = g.now.track.as_ref().map(|t| t.id.clone())
                    == raw.now.track.as_ref().map(|t| t.id.clone());
                if same {
                    g.now.track.as_ref().and_then(|t| t.artwork.clone())
                } else {
                    None
                }
            })
            && let Some(t) = raw.now.track.as_mut()
        {
            t.artwork = Some(prev);
        }

        // 2) 远端封面（只在没有字节、但有 URL 时尝试）
        #[cfg(feature = "online")]
        if raw.artwork_raw.is_none()
            && let Some(url) = raw
                .now
                .track
                .as_ref()
                .and_then(|t| t.source.url.clone())
        {
            self.fetch_remote_artwork(&url, raw.now.track.as_ref().map(|t| t.id.clone()).unwrap_or_default())
                .await;
        }

        // 3) 歌词：本地部分**每一轮都解析**（带备忘，命中就是一次内存查找，不读盘）。
        //
        // 之前是「只在曲目变化时解析一次，之后沿用状态里的值」—— 那样在并发 apply 下会丢：
        // 一个任务标记了「已处理」但还没把结果写进状态，另一个任务看到标记就跳过，于是
        // 快照里永久没有歌词。改成「每轮都解析 + 幂等」（备忘让它依然便宜）。
        let lyrics_query = raw.now.track.as_ref().map(|t| {
            LyricsQuery::new(&t.title, &t.artist, &t.album)
                .with_duration(t.duration_ms)
                .with_file(t.source.file_path.clone())
        });
        let mut has_lyrics = false;
        if let Some(q) = &lyrics_query
            && let Some(l) = self.lyrics.resolve_local_cached(q)
        {
            has_lyrics = true;
            if let Some(tr) = raw.now.track.as_mut() {
                tr.lyrics = Some(l);
            }
        } else if !has_lyrics
            && let Some(prev) = self.state.read().ok().and_then(|g| {
                let now = &g.now;
                // 只有同一首才沿用，避免把上一首的歌词挂到新曲目上
                let same = now.track.as_ref().map(|t| t.id.clone())
                    == raw.now.track.as_ref().map(|t| t.id.clone());
                if same { now.track.as_ref().and_then(|t| t.lyrics.clone()) } else { None }
            })
            && let Some(tr) = raw.now.track.as_mut()
        {
            // 「解析不到」不等于「没有歌词」：可能是别的路径刚附上的（在线命中、
            // 显式重查），也可能是备忘还没更新。**永不把已有的歌词降级成空**。
            tr.lyrics = Some(prev);
        }

        // 4) 本地没有才考虑在线：同一首只发起一次（标记在 apply 锁内更新，不会重复发起）
        let track_id = raw.now.track.as_ref().map(|t| t.id.clone()).unwrap_or_default();
        if !has_lyrics
            && !track_id.is_empty()
            && self.lyrics.online_enabled()
            && let Some(q) = lyrics_query
            && q.is_usable()
        {
            let should_spawn = self
                .state
                .read()
                .map(|g| g.online_requested_for != track_id)
                .unwrap_or(true);
            if should_spawn {
                if let Ok(mut g) = self.state.write() {
                    g.online_requested_for = track_id.clone();
                }
                self.spawn_online_lyrics(q, track_id.clone());
            }
        }

        // 4) 差异判定 + 落状态
        let mut emit_track = false;
        let mut emit_playback = false;
        {
            let Ok(mut g) = self.state.write() else { return };
            g.polls += 1;
            let prev_has_media = g.now.has_media;
            let prev_id = g.last_track_id.clone();
            let prev_pb = g.now.playback.clone();

            if track_id != prev_id {
                emit_track = true;
                g.last_track_id = track_id.clone();
                g.last_position_ms = raw.now.playback.position_ms;
                g.last_position_at_ms = now_ms_value;
            } else if prev_has_media != raw.has_media() {
                emit_track = true;
            }

            // 播放态变化：只比「有意义的字段」，位置仅在跳变超阈值时才算
            let expected = if prev_pb.state.is_playing() && g.last_position_at_ms > 0 {
                let elapsed = now_ms_value.saturating_sub(g.last_position_at_ms) as i64;
                prev_pb.position_ms as i64 + elapsed
            } else {
                prev_pb.position_ms as i64
            };
            let actual = raw.now.playback.position_ms as i64;
            let jumped = (actual - expected).abs() > POSITION_JUMP_MS;
            if raw.now.playback.state != prev_pb.state
                || raw.now.playback.loop_mode != prev_pb.loop_mode
                || raw.now.playback.shuffle != prev_pb.shuffle
                || raw.now.playback.volume != prev_pb.volume
                || raw.now.playback.muted != prev_pb.muted
                || (raw.now.playback.rate - prev_pb.rate).abs() > f64::EPSILON
                || raw.now.playback.duration_ms != prev_pb.duration_ms
                || jumped
            {
                emit_playback = true;
                g.last_position_ms = raw.now.playback.position_ms;
                g.last_position_at_ms = now_ms_value;
            }

            g.metadata = self.session.status();
            g.now = raw.now.clone();
        }

        if emit_track {
            let _ = self.events.send(Event::Track { now: self.snapshot_at_now() });
        }
        if emit_playback {
            let _ = self.events.send(Event::Playback { now: self.snapshot_at_now() });
        }
        if let Some(artwork) = artwork_event {
            let _ = self.events.send(Event::Artwork { artwork });
        }
    }

    /// 给封面补上「HTTP 相对地址」（开了 HTTP 服务时）。
    fn decorate_artwork(&self, mut art: Artwork) -> Artwork {
        if let Some(base) = &self.config.artwork_http_path {
            let v = art.key.as_bytes().first().map(|b| *b as u32).unwrap_or(0);
            art.http_path = Some(format!("{base}?v={v}"));
        }
        art
    }

    /// 在线歌词：后台补，补到了再发一条事件（不阻塞主链路）。
    fn spawn_online_lyrics(&self, q: LyricsQuery, track_id: String) {
        // 用同一个取词器实例（共享备忘/错误状态）；命中的结果已落盘，
        // 清掉备忘就能让下一轮 `resolve_local_cached` 从缓存目录读到它。
        let resolver = Arc::clone(&self.lyrics);
        let events = self.events.clone();
        let key = q.track_key();
        tokio::spawn(async move {
            let lyrics = resolver.resolve_online(q).await;
            resolver.forget_memo(&key);
            let _ = events.send(Event::Lyrics {
                track_id,
                lyrics,
                source: "lrclib".to_string(),
            });
        });
    }

    /// 把在线拿到的歌词写回当前快照（若曲目没变）。
    pub fn attach_lyrics(&self, lyrics: &Lyrics) {
        if let Ok(mut g) = self.state.write() {
            let key = lyrics.track_key.clone();
            if let Some(t) = g.now.track.as_mut() {
                let matches = LyricsQuery::new(&t.title, &t.artist, &t.album)
                    .with_duration(t.duration_ms)
                    .track_key()
                    == key;
                if matches {
                    t.lyrics = Some(lyrics.clone());
                    // 清掉备忘：否则下一轮轮询还可能用旧的（未命中）结果把歌词盖掉
                    self.lyrics.forget_memo(&key);
                }
            }
        }
    }

    /// 远端封面下载（需要 `online` feature）。
    #[cfg(feature = "online")]
    async fn fetch_remote_artwork(&self, url: &str, track_id: String) {
        let url = url.to_string();
        let url_for_fetch = url.clone();
        let fetched = tokio::task::spawn_blocking(move || fetch_url_blocking(&url_for_fetch))
            .await
            .ok()
            .flatten();
        let Some((bytes, mime)) = fetched else { return };
        match self.artwork.store(&track_id, mime.as_deref(), &bytes, crate::types::ArtworkOrigin::Remote, Some(url)) {
            Ok(cached) => {
                if let Ok(mut g) = self.state.write()
                    && let Some(t) = g.now.track.as_mut()
                    && t.id == track_id
                {
                    t.artwork = Some(self.decorate_artwork(cached.clone()));
                }
                let _ = self.events.send(Event::Artwork { artwork: cached });
            }
            Err(e) => self.emit_error("artwork", &e),
        }
    }

    fn emit_error(&self, source: &str, err: &BridgeError) {
        let _ = self.events.send(Event::Error {
            source: source.to_string(),
            code: err.code().to_string(),
            message: err.to_string(),
        });
    }

    /// 读一张封面的字节（HTTP 路由用）。
    pub fn artwork_bytes(&self) -> Option<(Vec<u8>, String)> {
        let art = self.artwork.current()?;
        let path = art.path?;
        let bytes = std::fs::read(path).ok()?;
        Some((bytes, art.mime))
    }

    /// 列出缓存目录里的歌词（`lyrics list`）。
    pub fn cached_lyrics(&self) -> Vec<PathBuf> {
        crate::lyrics::list_cached_lyrics(&self.config.cache_dir())
    }

    /// 清掉封面缓存。
    pub fn clear_artwork(&self) {
        self.artwork.clear();
    }

    /// 平台自检报告（`media-bridge diagnose`）。
    pub fn diagnose(&self) -> Vec<String> {
        let mut out = self.session.diagnose();
        out.push(format!(
            "会话后端：{}　缓存目录：{}",
            self.provider_name(),
            self.config.cache_dir().display()
        ));
        let audio = self.audio.status();
        out.push(format!("系统音频：{:?}　{}", audio.state, audio.hint));
        let layout = self.audio.layout();
        if layout.callbacks > 0 {
            out.push(format!(
                "采集回调布局：{} 次回调，{} buffer × {} 通道 × {} 字节{}",
                layout.callbacks,
                layout.buffers,
                layout.channels,
                layout.bytes,
                if layout.non_interleaved { "（非交错）" } else { "（交错）" }
            ));
        }
        let ls = self.lyrics.status();
        out.push(format!(
            "歌词：本地 {}　在线 {}",
            ls.local_sources.join("/"),
            if ls.online_enabled { "已启用（LRCLIB）" } else { "未启用" }
        ));
        out
    }
}

/// 阻塞式 GET（只在 `online` feature 下编译；调用方负责丢进阻塞线程池）。
#[cfg(feature = "online")]
fn fetch_url_blocking(url: &str) -> Option<(Vec<u8>, Option<String>)> {
    // 本地文件地址直接读盘（Linux MPRIS 常见）
    if let Some(path) = url.strip_prefix("file://") {
        let bytes = std::fs::read(path).ok()?;
        return Some((bytes, None));
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return None;
    }
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(8)))
        .user_agent(concat!("media-bridge/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent();
    let mut resp = agent.get(url).call().ok()?;
    let mime = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let bytes = resp.body_mut().read_to_vec().ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some((bytes, mime))
}

/// 判断一份快照里是否有可播内容（对外便捷函数）。
pub fn has_playable(track: Option<&Track>) -> bool {
    track.is_some_and(|t| t.has_content())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::mock::MockConfig;

    fn mock_bridge() -> Arc<MediaBridge> {
        let dir = std::env::temp_dir().join(format!(
            "mb-service-{}",
            crate::util::short_hash(&crate::util::uuid_v4_ish())
        ));
        let cfg = BridgeConfig {
            provider: Provider::Mock,
            mock: MockConfig::default(),
            cache_dir: Some(dir),
            audio: AudioConfig { enabled: false, ..Default::default() },
            lyrics_online: false,
            ..Default::default()
        };
        MediaBridge::new(cfg)
    }

    #[tokio::test]
    async fn poll_sets_snapshot_and_persists_artwork() {
        let b = mock_bridge();
        b.poll_once().await.unwrap();
        let now = b.snapshot();
        assert!(now.has_media);
        let t = now.track.expect("应有曲目");
        let art = t.artwork.expect("假播放器应提供封面");
        assert_eq!(art.mime, "image/png");
        let path = art.path.expect("封面应已落盘");
        assert!(path.exists(), "封面文件应真实存在：{}", path.display());
        assert_eq!(art.width, Some(160));
    }

    #[tokio::test]
    async fn track_change_emits_track_event_only_once() {
        let b = mock_bridge();
        let mut rx = b.subscribe();
        b.poll_once().await.unwrap();
        // 第一轮：从「无」到「有」→ 一次 track 事件
        let ev = rx.try_recv().expect("应发出一条事件");
        assert_eq!(ev.name(), "track");
        // 第二轮：同一首 → 不该再发 track
        b.poll_once().await.unwrap();
        let mut names = vec![];
        while let Ok(e) = rx.try_recv() {
            names.push(e.name());
        }
        assert!(
            !names.contains(&"track"),
            "同一首曲目不该重复发 track 事件，实际：{names:?}"
        );
    }

    #[tokio::test]
    async fn control_reports_and_refreshes_state() {
        let b = mock_bridge();
        b.poll_once().await.unwrap();
        let report = b.control(TransportCommand::Pause).await.unwrap();
        assert!(report.outcome.applied);
        assert_eq!(report.now.playback.state, crate::types::PlaybackState::Paused);
        let report = b.control(TransportCommand::SeekBy { delta_ms: 60_000 }).await.unwrap();
        assert!(report.outcome.applied, "seek-by 应生效：{:?}", report.outcome);
        assert!(report.now.playback.position_ms > 50_000);
    }

    #[tokio::test]
    async fn seek_is_reported_as_playback_event() {
        let b = mock_bridge();
        b.poll_once().await.unwrap();
        let mut rx = b.subscribe();
        b.control(TransportCommand::Seek { position_ms: 120_000 }).await.unwrap();
        let mut saw_playback = false;
        while let Ok(e) = rx.try_recv() {
            if e.name() == "playback" {
                saw_playback = true;
            }
        }
        assert!(saw_playback, "seek 是外推解释不了的跳变，应发 playback 事件");
    }

    /// 回归：并发 apply 不能把已经拿到的歌词弄丢。
    ///
    /// 真实场景：轮询任务与「显式 refresh / 控制命令后的刷新」会同时进来。曾经的实现是
    /// 「只在曲目变化时解析一次，之后沿用状态里的值」，两次 apply 交叠时会出现
    /// 「一个标记了已处理、另一个跳过解析」，症状是**歌词偶发消失**（在 Linux 虚拟机实测 5 次里丢 1 次）。
    #[tokio::test]
    async fn concurrent_polls_never_drop_lyrics() {
        let b = mock_bridge();
        b.poll_once().await.unwrap();
        let t = b.snapshot().track.expect("应有曲目");
        let q = LyricsQuery::new(&t.title, &t.artist, &t.album).with_duration(t.duration_ms);
        crate::lyrics::write_cached_lyrics(&b.config.cache_dir(), &q, "[00:01.00]并发下的歌词\n")
            .unwrap();

        // 并发地反复轮询 / 刷新 / 重查，任何一次都不该让歌词消失
        let mut handles = Vec::new();
        for _ in 0..8 {
            let b = b.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..5 {
                    b.poll_once().await.unwrap();
                    tokio::task::yield_now().await;
                    assert!(
                        b.snapshot().track.and_then(|t| t.lyrics).is_some(),
                        "并发轮询中歌词消失了"
                    );
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert!(b.lyrics().is_some(), "收尾时歌词应仍在");
    }

    /// 回归：并发 apply 不能把已经拿到的封面弄丢（与歌词同一个模式的 bug）。
    ///
    /// 真实场景：播放器只在换曲时给一次封面字节，之后每轮的 raw 都是空的；
    /// 没有「永不降级」规则时，后到的那次 apply 会把 snapshot 的封面抹成空。
    #[tokio::test]
    async fn concurrent_polls_never_drop_artwork() {
        let b = mock_bridge();
        b.poll_once().await.unwrap();
        assert!(b.snapshot().track.and_then(|t| t.artwork).is_some(), "首轮就该有封面");

        let mut handles = Vec::new();
        for _ in 0..8 {
            let b = b.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..5 {
                    b.poll_once().await.unwrap();
                    tokio::task::yield_now().await;
                    assert!(
                        b.snapshot().track.and_then(|t| t.artwork).is_some(),
                        "并发轮询中封面消失了"
                    );
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    }

    #[tokio::test]
    async fn status_reports_all_sources() {
        let b = mock_bridge();
        b.poll_once().await.unwrap();
        let st = b.status();
        let names: Vec<&str> = st.sources.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"metadata"));
        assert!(names.contains(&"audio"));
        assert!(names.contains(&"lyrics"));
        assert_eq!(st.provider, "mock");
        assert!(st.version.starts_with('0'));
    }

    #[tokio::test]
    async fn stop_is_idempotent_and_poller_exits() {
        let b = mock_bridge();
        b.start();
        b.poll_once().await.unwrap();
        b.stop();
        b.stop();
        assert!(b.snapshot().has_media || !b.snapshot().has_media);
    }

    #[tokio::test]
    async fn lyrics_are_resolved_from_mock_lrc_via_sidecar_path() {
        // 假播放器不给 file_path，但我们可以直接在缓存目录里放一份歌词来验证 cache 路径
        let b = mock_bridge();
        b.poll_once().await.unwrap();
        let now = b.snapshot();
        let t = now.track.unwrap();
        let q = LyricsQuery::new(&t.title, &t.artist, &t.album).with_duration(t.duration_ms);
        crate::lyrics::write_cached_lyrics(&b.config.cache_dir(), &q, "[00:01.00]缓存里的歌词\n")
            .unwrap();
        // 显式重查（不带备忘的那条路）—— 用户新放了 .lrc 时的真实用法
        b.refresh_lyrics(false).await.expect("应能重查到歌词");
        b.poll_once().await.unwrap();
        let l = b.lyrics().expect("应命中缓存歌词");
        assert_eq!(l.source, "cache");
        assert_eq!(l.lines[0].text, "缓存里的歌词");
    }
}
