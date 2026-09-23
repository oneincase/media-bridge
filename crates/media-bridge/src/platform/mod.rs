//! 平台会话层：把三套互不相干的系统 API 收敛成同一个接口。
//!
//! 用**枚举分派**而不是 `dyn Trait` + async-trait：平台后端是编译期固定的三选一，
//! 枚举没有虚调用开销、不需要 `Pin<Box<dyn Future>>`、也不用额外的 trait 适配 crate。
//!
//! 这一层的输出是 [`RawNowPlaying`]：与对外快照的唯一区别是**封面是字节而不是缓存
//! 路径** —— 因为「这张图存在哪、要不要落盘」是服务层的事，平台层只负责「拿到」。
//! 这样分层的直接好处：封面缓存/去重/裁剪逻辑只有一份，三平台共用。

use crate::error::Result;
use crate::types::{
    ArtworkOrigin, Capabilities, ControlOutcome, NowPlaying, SourceStatus, Track, TransportCommand,
};

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
/// macOS 15.4+ 的 MediaRemote 访问通道（借 /usr/bin/perl 身份读，见该文件头）
#[cfg(target_os = "macos")]
pub mod macos_helper;
pub mod mock;
#[cfg(target_os = "windows")]
pub mod windows;

/// 平台层拿到的封面原始字节。
#[derive(Debug, Clone)]
pub struct RawArtwork {
    /// 去重键（换曲判定用）：通常是内容标识或 `artist|title|album`
    pub key: String,
    /// 播放器自称的 MIME（**仅供嗅探失败时兜底**，不直接采信）
    pub mime_hint: Option<String>,
    pub bytes: Vec<u8>,
    pub origin: ArtworkOrigin,
    /// 播放器给的远端地址（若封面是从远端下载来的）
    pub source_url: Option<String>,
}

/// 平台层原始快照。
#[derive(Debug, Clone, Default)]
pub struct RawNowPlaying {
    /// 对外快照（`track.artwork` 恒为 `None`，由服务层填）
    pub now: NowPlaying,
    /// 封面字节（播放器没有封面时为 `None`）
    pub artwork_raw: Option<RawArtwork>,
}

impl RawNowPlaying {
    /// 什么都没有（没有播放器在放）。
    pub fn empty(now_ms: u64) -> Self {
        Self {
            now: NowPlaying::empty(now_ms),
            artwork_raw: None,
        }
    }

    pub fn has_media(&self) -> bool {
        self.now.has_media
    }

    /// 曲目标识（换曲判定）；没有曲目时为空串。
    pub fn track_id(&self) -> &str {
        self.now.track.as_ref().map(|t| t.id.as_str()).unwrap_or("")
    }

    fn from_parts(track: Option<Track>, playback: crate::types::Playback, capabilities: Capabilities, now_ms: u64) -> Self {
        let has_media = track.as_ref().is_some_and(|t| t.has_content());
        let track = if has_media { track } else { None };
        Self {
            now: NowPlaying {
                has_media,
                track,
                playback,
                capabilities: if has_media { capabilities } else { Capabilities::NONE },
                captured_at_ms: now_ms,
            },
            artwork_raw: None,
        }
    }
}

/// 使用哪个会话后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Provider {
    /// 按当前操作系统自动选（macOS→MediaRemote，Windows→GSMTC，Linux→MPRIS）
    #[default]
    Auto,
    /// 内存里的假播放器：不依赖任何系统媒体服务，用于联调、演示与测试
    Mock,
}

/// 平台会话：三平台 + Mock 的统一入口。
pub enum Session {
    #[cfg(target_os = "macos")]
    Mac(macos::MacSession),
    #[cfg(target_os = "linux")]
    Linux(linux::LinuxSession),
    #[cfg(target_os = "windows")]
    Windows(windows::WindowsSession),
    Mock(mock::MockSession),
}

impl Session {
    /// 构造会话。`Auto` 在本平台后端初始化失败时**不报错**，而是返回一个「永远空快照」
    /// 的会话 —— 中间件必须能在数据源不可用时活着（消费端看 `status()` 拿原因）。
    pub fn new(provider: Provider) -> Self {
        match provider {
            Provider::Mock => Session::Mock(mock::MockSession::new()),
            Provider::Auto => Self::new_auto(),
        }
    }

    #[cfg(target_os = "macos")]
    fn new_auto() -> Self {
        Session::Mac(macos::MacSession::new())
    }

    #[cfg(target_os = "linux")]
    fn new_auto() -> Self {
        Session::Linux(linux::LinuxSession::new())
    }

    #[cfg(target_os = "windows")]
    fn new_auto() -> Self {
        Session::Windows(windows::WindowsSession::new())
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    fn new_auto() -> Self {
        Session::Mock(mock::MockSession::new())
    }

    /// 构造一个假播放器会话（测试/演示用）。
    pub fn mock(config: mock::MockConfig) -> Self {
        Session::Mock(mock::MockSession::with_config(config))
    }

    /// 后端名（`macos-mediaremote` 等），会出现在 `Track.source.provider` 与状态里。
    pub fn provider_name(&self) -> &'static str {
        match self {
            #[cfg(target_os = "macos")]
            Session::Mac(_) => "macos-mediaremote",
            #[cfg(target_os = "linux")]
            Session::Linux(_) => "linux-mpris",
            #[cfg(target_os = "windows")]
            Session::Windows(_) => "windows-gsmtc",
            Session::Mock(_) => "mock",
        }
    }

    /// 取一次「现在在放什么」。
    pub async fn snapshot(&self) -> Result<RawNowPlaying> {
        match self {
            #[cfg(target_os = "macos")]
            Session::Mac(s) => {
                let s = s.clone();
                // MediaRemote 的查询是「发 block 到私有队列 + 等回执」，天然阻塞；
                // 丢到阻塞线程池，别占住 async 执行器。
                tokio::task::spawn_blocking(move || s.snapshot_blocking())
                    .await
                    .map_err(|e| crate::error::BridgeError::other(format!("会话线程 panic：{e}")))?
            }
            #[cfg(target_os = "linux")]
            Session::Linux(s) => s.snapshot().await,
            #[cfg(target_os = "windows")]
            Session::Windows(s) => s.snapshot().await,
            Session::Mock(s) => s.snapshot().await,
        }
    }

    /// 发一条传输控制命令（第 2 期）。
    pub async fn control(&self, cmd: TransportCommand) -> Result<ControlOutcome> {
        match self {
            #[cfg(target_os = "macos")]
            Session::Mac(s) => {
                let s = s.clone();
                tokio::task::spawn_blocking(move || s.control_blocking(&cmd))
                    .await
                    .map_err(|e| crate::error::BridgeError::other(format!("会话线程 panic：{e}")))?
            }
            #[cfg(target_os = "linux")]
            Session::Linux(s) => s.control(cmd).await,
            #[cfg(target_os = "windows")]
            Session::Windows(s) => s.control(cmd).await,
            Session::Mock(s) => s.control(cmd).await,
        }
    }

    /// 后端健康状态（供 `status` 上报与设置界面给引导）。
    pub fn status(&self) -> SourceStatus {
        match self {
            #[cfg(target_os = "macos")]
            Session::Mac(s) => s.status(),
            #[cfg(target_os = "linux")]
            Session::Linux(s) => s.status(),
            #[cfg(target_os = "windows")]
            Session::Windows(s) => s.status(),
            Session::Mock(s) => s.status(),
        }
    }

    /// 平台自检报告（`media-bridge diagnose`）：逐项说明「能不能拿到、差什么」。
    pub fn diagnose(&self) -> Vec<String> {
        match self {
            #[cfg(target_os = "macos")]
            Session::Mac(s) => s.diagnose(),
            #[cfg(target_os = "linux")]
            Session::Linux(s) => s.diagnose(),
            #[cfg(target_os = "windows")]
            Session::Windows(s) => s.diagnose(),
            Session::Mock(s) => s.diagnose(),
        }
    }
}
