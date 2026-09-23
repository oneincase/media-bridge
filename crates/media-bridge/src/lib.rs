//! # media-bridge
//!
//! 跨平台的**系统媒体中间件**：一条通道拿到「现在在放什么」，并反过来控制它。
//!
//! 第 1 期（元数据 / 封面 / 歌词 / 音频）与第 2 期（反向控制）共用同一套平台会话层，
//! 所以能力是统一的：`now` 拿快照，`control` 发命令，`spectrum` 拿频谱。
//!
//! ## 三平台的数据源
//!
//! | 能力 | macOS | Windows | Linux |
//! |---|---|---|---|
//! | 正在播放 | MediaRemote（私有框架，dlopen） | GSMTC（WinRT） | MPRIS（D-Bus） |
//! | 封面 | MediaRemote `artworkData` | GSMTC `Thumbnail` | MPRIS `mpris:artUrl` |
//! | 系统音频 | CoreAudio Process Tap（14.2+） | WASAPI loopback | PulseAudio/PipeWire monitor |
//! | 歌词 | sidecar / 缓存 / 内嵌 / LRCLIB | 同左 | 同左（+ MPRIS 文件路径带内嵌标签） |
//!
//! ## 接入方式
//!
//! - **进程内**（Tauri / Rust 宿主）：`MediaBridge::new(...)` 直接用，见 [`service`]。
//! - **子进程 JSON 行**（Node / Python / Go 宿主）：`media-bridge serve`，见 [`ipc::stdio`]。
//! - **本地 HTTP**（浏览器 / 多消费者）：`media-bridge serve --http 127.0.0.1:8765`，见 [`ipc::http`]。
//!
//! 三种接入方式共用同一份 wire 类型（[`ipc`]），换接入方式不需要改数据模型。

pub mod artwork;
pub mod error;
pub mod lyrics;
pub mod service;
pub mod spectrum;
pub mod types;
pub mod util;

#[cfg(feature = "embedded")]
pub mod tags;

pub mod audio;
pub mod ipc;
pub mod platform;

pub use error::{BridgeError, Result};
pub use service::{BridgeConfig, MediaBridge};
pub use types::{
    Artwork, ArtworkOrigin, Capabilities, ControlOutcome, LoopMode, LrcLine, Lyrics, NowPlaying,
    Playback, PlaybackState, PositionSource, SourceState, SourceStatus, SpectrumFrame, Status,
    Track, TrackSource, TransportCommand, SPECTRUM_BANDS,
};

/// crate 版本。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
