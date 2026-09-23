//! 核心数据模型 —— 第 1 期（元数据/封面/歌词/音频）与第 2 期（反向控制）共用的契约。
//!
//! 设计取向：
//!   - **先统一，再降级**。三平台的原始数据差异（MediaRemote 的 CFDictionary、MPRIS 的
//!     `a{sv}`、GSMTC 的 WinRT 对象）在这一层被拍平；拍不平的部分（某平台拿不到的东西）
//!     一律是 `Option` / 空串，而不是「假装有值」。
//!   - **能力显式声明**。`Capabilities` 描述「当前这个播放器现在到底能做什么」——UI 据此
//!     决定按钮的禁用态，而不是先发命令再看失败。
//!   - **位置可外推**。轮询拿到的 `position_ms` 带 `updated_at_ms` 与 `rate`，消费者按
//!     `Playback::position_at()` 外推，就不必为进度条把轮询调密。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 频谱段数（与 dsh-wallpaper-engine 现有 64 段契约保持一致）。
pub const SPECTRUM_BANDS: usize = 64;
/// 默认轮询间隔（毫秒）。空闲时更慢、刚发过控制命令时突发加速。
pub const DEFAULT_POLL_MS: u64 = 1000;

// ══════════════════════════════════════════════════════════════════════════════
// 播放状态
// ══════════════════════════════════════════════════════════════════════════════

/// 播放状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlaybackState {
    Playing,
    Paused,
    Stopped,
    Unknown,
}

impl PlaybackState {
    pub fn is_playing(self) -> bool {
        matches!(self, Self::Playing)
    }

    /// 与旧 wire 兼容的数值：1 = 播放中，2 = 其它（DSH 现有 `setMedia` 用的是这一套）。
    pub fn wire_code(self) -> u8 {
        match self {
            Self::Playing => 1,
            _ => 2,
        }
    }

    /// 从 MPRIS 的 `PlaybackStatus` 字符串解析。
    pub fn from_mpris(s: &str) -> Self {
        match s {
            "Playing" => Self::Playing,
            "Paused" => Self::Paused,
            "Stopped" => Self::Stopped,
            _ => Self::Unknown,
        }
    }
}

/// 循环模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoopMode {
    Off,
    /// 单曲循环
    Track,
    /// 列表循环
    Playlist,
    Unknown,
}

impl LoopMode {
    /// 循环切换顺序：关 → 列表 → 单曲 → 关（与主流播放器按钮一致）。
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::Playlist,
            Self::Playlist => Self::Track,
            Self::Track | Self::Unknown => Self::Off,
        }
    }

    /// MPRIS `LoopStatus` 字符串 → 模式（MPRIS 没有「关」以外的第三个值）。
    pub fn from_mpris(s: &str) -> Self {
        match s {
            "None" => Self::Off,
            "Track" => Self::Track,
            "Playlist" => Self::Playlist,
            _ => Self::Unknown,
        }
    }

    pub fn to_mpris(self) -> &'static str {
        match self {
            Self::Off | Self::Unknown => "None",
            Self::Track => "Track",
            Self::Playlist => "Playlist",
        }
    }
}

/// 位置值的来源 —— 消费者可据此判断要不要外推。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PositionSource {
    /// 播放器直接报的
    Polled,
    /// 由轮询点 + 速率外推
    Interpolated,
    /// 拿不到（播放器不支持 Position）
    Unavailable,
}

// ══════════════════════════════════════════════════════════════════════════════
// 封面
// ══════════════════════════════════════════════════════════════════════════════

/// 封面来源渠道。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtworkOrigin {
    /// 播放器直接给的数据（macOS MediaRemote artworkData、GSMTC Thumbnail、MPRIS artUrl 已下载）
    Player,
    /// 本地文件的内嵌封面（embedded feature）
    Embedded,
    /// 播放器给的远端地址（已下载到本地缓存）
    Remote,
    /// 缓存命中（本轮没有重新取，直接用上次落盘的文件）
    Cache,
}

/// 封面（已落盘的本地文件 + 可选的服务地址）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Artwork {
    /// 去重键：同一张封面重复出现时不再重新下载/落盘。
    pub key: String,
    /// 真实 MIME（按魔数嗅探，不信播放器自称的类型 —— 存错后缀浏览器直接不显示）。
    pub mime: String,
    pub bytes: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub origin: ArtworkOrigin,
    /// 本地缓存文件的绝对路径（宿主可直接读盘/自己做路由）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// 开 HTTP 服务时的相对地址（形如 `/v1/artwork?v=<key 前缀>`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_path: Option<String>,
    /// 播放器给的原始远端地址（排查用，不要直接喂给沙箱页面）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
}

// ══════════════════════════════════════════════════════════════════════════════
// 歌词
// ══════════════════════════════════════════════════════════════════════════════

/// 一行歌词。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LrcLine {
    /// 行时间戳（毫秒，已含 offset 修正前的原始值）
    pub t_ms: i64,
    pub text: String,
}

/// 歌词（含时间轴时 `synced = true`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lyrics {
    /// 来源：`sidecar` / `cache` / `embedded` / `lrclib`
    pub source: String,
    /// 是否有时间轴（纯文本歌词为 false）
    pub synced: bool,
    /// LRC 头部 `[offset:]` 声明的偏移（毫秒）
    pub offset_ms: i64,
    /// 按时间升序的行（纯文本歌词只有一行一行的文本，t_ms 恒为 0）
    pub lines: Vec<LrcLine>,
    /// 参与匹配的曲目键（`artist|title|album|duration`），便于宿主做缓存
    pub track_key: String,
    /// 原始 LRC / 纯文本（宿主想自己排版时用）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

impl Lyrics {
    /// 当前应高亮/显示的行号（`pos_ms` 为播放位置）。
    ///
    /// 返回 `Some(i)` 表示第 i 行是「当前行」；在首行之前返回 `Some(0)`，
    /// 无时间轴或空歌词返回 `None`。
    ///
    /// `offset_ms` 语义（与 LRC 头部 `[offset:]` 的通行约定一致）：**正值表示歌词
    /// 整体晚出现**，等价于每行的生效时间 = `line.t_ms + offset_ms`。
    pub fn active_index(&self, pos_ms: i64) -> Option<usize> {
        if !self.synced || self.lines.is_empty() {
            return None;
        }
        let t = pos_ms - self.offset_ms;
        let mut idx = 0usize;
        for (i, line) in self.lines.iter().enumerate() {
            if line.t_ms <= t {
                idx = i;
            } else {
                break;
            }
        }
        Some(idx)
    }

    /// 第 `i` 行的生效时间（已含 offset；消费端做卡拉OK式高亮时用它）。
    pub fn effective_t_ms(&self, i: usize) -> Option<i64> {
        self.lines.get(i).map(|l| l.t_ms + self.offset_ms)
    }

    /// 纯文本拼接（无时间轴时的展示形态）。
    pub fn plain_text(&self) -> String {
        self.lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// 曲目 / 播放进度 / 能力
// ══════════════════════════════════════════════════════════════════════════════

/// 曲目来自哪个播放器 / 哪个文件。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackSource {
    /// 提供者标识：`macos-mediaremote` / `linux-mpris` / `windows-gsmtc` / `mock`
    pub provider: String,
    /// 面向用户的应用名（Music、Spotify、foobar2000…）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_name: Option<String>,
    /// 应用标识：bundle id / desktop app id / MPRIS bus name
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// 本地文件路径（能拿到时才填 —— Linux MPRIS 常见，macOS/Windows 通常拿不到）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<PathBuf>,
    /// 流地址（mpris:xesam:url 的 http(s) 形式）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// 曲目元数据。
///
/// 字段一律「有则填、无则空」，不区分平台 —— 消费端不需要知道数据从哪来。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    /// 稳定标识：优先用播放器给的内容标识，退化为 `artist|title|album`。
    /// 用于「换曲了没」的判定与封面/歌词缓存键。
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub album_artist: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub genre: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub composer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_number: Option<u32>,
    /// 时长（毫秒）。0 = 未知（直播流）。
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork: Option<Artwork>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lyrics: Option<Lyrics>,
    pub source: TrackSource,
}

impl Track {
    /// `artist|title|album` 形式的兜底键（播放器没给内容标识时用）。
    pub fn fallback_key(&self) -> String {
        format!("{}|{}|{}", self.artist, self.title, self.album)
    }

    /// 是否「有内容」——避免把只有空格的元数据当成正在播放。
    pub fn has_content(&self) -> bool {
        !self.title.trim().is_empty() || !self.artist.trim().is_empty()
    }
}

/// 播放进度与传输状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Playback {
    pub state: PlaybackState,
    /// 上报时刻的位置（毫秒）
    pub position_ms: u64,
    /// 位置值的来源
    pub position_source: PositionSource,
    pub duration_ms: u64,
    /// 播放速率（1.0 为正常；播客/有声书常用 0.8~2.0）
    pub rate: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
    pub loop_mode: LoopMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shuffle: Option<bool>,
    /// 本次上报的本地时钟（Unix 毫秒），供消费端外推
    pub updated_at_ms: u64,
}

impl Default for Playback {
    fn default() -> Self {
        Self {
            state: PlaybackState::Unknown,
            position_ms: 0,
            position_source: PositionSource::Unavailable,
            duration_ms: 0,
            rate: 1.0,
            volume: None,
            muted: None,
            loop_mode: LoopMode::Unknown,
            shuffle: None,
            updated_at_ms: 0,
        }
    }
}

impl Playback {
    /// 按本地时钟外推当前位置：`position_at(now)`。
    ///
    /// 只在「播放中 + 有速率」时外推；暂停/停止或位置不可用时原样返回。
    pub fn position_at(&self, now_ms: u64) -> u64 {
        if !self.state.is_playing()
            || self.position_source == PositionSource::Unavailable
            || self.updated_at_ms == 0
        {
            return self.position_ms;
        }
        let elapsed = now_ms.saturating_sub(self.updated_at_ms) as f64;
        let pos = self.position_ms as f64 + elapsed * self.rate.max(0.0);
        let pos = pos.max(0.0);
        if self.duration_ms > 0 {
            return (pos.min(self.duration_ms as f64)) as u64;
        }
        pos as u64
    }
}

/// 当前播放器**实际支持**的传输能力（不是「协议理论上支持」）。
///
/// 消费端据此置灰按钮，避免「点了没反应」。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub play: bool,
    pub pause: bool,
    /// 播放/暂停切换（多数播放器的耳机键行为）
    pub toggle: bool,
    pub stop: bool,
    pub next: bool,
    pub previous: bool,
    /// 跳到绝对位置
    pub seek_absolute: bool,
    /// 快进/快退（相对位移）
    pub seek_relative: bool,
    pub set_loop: bool,
    pub set_shuffle: bool,
    pub set_volume: bool,
    pub set_mute: bool,
    pub set_rate: bool,
}

impl Capabilities {
    /// 什么都不能做（没有播放器在跑时）。
    pub const NONE: Self = Self {
        play: false,
        pause: false,
        toggle: false,
        stop: false,
        next: false,
        previous: false,
        seek_absolute: false,
        seek_relative: false,
        set_loop: false,
        set_shuffle: false,
        set_volume: false,
        set_mute: false,
        set_rate: false,
    };

    /// 常见播放器都支持的一套（无法枚举能力时的乐观默认）。
    pub const BASIC: Self = Self {
        play: true,
        pause: true,
        toggle: true,
        stop: true,
        next: true,
        previous: true,
        seek_absolute: true,
        seek_relative: true,
        set_loop: false,
        set_shuffle: false,
        set_volume: false,
        set_mute: false,
        set_rate: false,
    };
}

/// 一次「现在在放什么」的完整快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NowPlaying {
    /// 有没有媒体在放（停止后播放器仍驻留时通常为 false）
    pub has_media: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<Track>,
    pub playback: Playback,
    pub capabilities: Capabilities,
    /// 快照生成时刻（Unix 毫秒）
    pub captured_at_ms: u64,
}

impl Default for NowPlaying {
    fn default() -> Self {
        Self {
            has_media: false,
            track: None,
            playback: Playback::default(),
            capabilities: Capabilities::NONE,
            captured_at_ms: 0,
        }
    }
}

impl NowPlaying {
    /// 没有媒体时的空快照。
    pub fn empty(now_ms: u64) -> Self {
        Self {
            captured_at_ms: now_ms,
            ..Default::default()
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// 第 2 期：反向控制
// ══════════════════════════════════════════════════════════════════════════════

/// 传输控制命令（第 2 期）。
///
/// wire 形态：`{"action":"seek-by","deltaMs":-15000}` —— 动作名 kebab-case，字段 camelCase。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum TransportCommand {
    Play,
    Pause,
    /// 播放 ↔ 暂停
    PlayPause,
    Stop,
    Next,
    Previous,
    /// 跳到绝对位置
    Seek { position_ms: u64 },
    /// 相对位移：正数快进、负数快退
    SeekBy { delta_ms: i64 },
    /// 设定循环模式
    SetLoop { mode: LoopMode },
    /// 循环模式按 关→列表→单曲→关 轮转
    CycleLoop,
    SetShuffle { on: bool },
    ToggleShuffle,
    SetVolume { volume: f64 },
    SetMute { muted: bool },
    SetRate { rate: f64 },
}

impl TransportCommand {
    /// 命令名（用于回执与日志）。
    pub fn name(&self) -> &'static str {
        match self {
            Self::Play => "play",
            Self::Pause => "pause",
            Self::PlayPause => "play-pause",
            Self::Stop => "stop",
            Self::Next => "next",
            Self::Previous => "previous",
            Self::Seek { .. } => "seek",
            Self::SeekBy { .. } => "seek-by",
            Self::SetLoop { .. } => "set-loop",
            Self::CycleLoop => "cycle-loop",
            Self::SetShuffle { .. } => "set-shuffle",
            Self::ToggleShuffle => "toggle-shuffle",
            Self::SetVolume { .. } => "set-volume",
            Self::SetMute { .. } => "set-mute",
            Self::SetRate { .. } => "set-rate",
        }
    }

    /// 该命令需要的能力位 —— 未声明能力即拒绝执行（不猜）。
    pub fn required_capability(&self) -> Option<fn(&Capabilities) -> bool> {
        Some(match self {
            Self::Play => |c: &Capabilities| c.play,
            Self::Pause => |c| c.pause,
            Self::PlayPause => |c| c.toggle || (c.play && c.pause),
            Self::Stop => |c| c.stop,
            Self::Next => |c| c.next,
            Self::Previous => |c| c.previous,
            Self::Seek { .. } => |c| c.seek_absolute || c.seek_relative,
            Self::SeekBy { .. } => |c| c.seek_relative || c.seek_absolute,
            Self::SetLoop { .. } | Self::CycleLoop => |c| c.set_loop,
            Self::SetShuffle { .. } | Self::ToggleShuffle => |c| c.set_shuffle,
            Self::SetVolume { .. } => |c| c.set_volume,
            Self::SetMute { .. } => |c| c.set_mute,
            Self::SetRate { .. } => |c| c.set_rate,
        })
    }
}

/// 控制命令的执行回执。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlOutcome {
    /// 命令名
    pub action: String,
    /// 是否真的发出去了
    pub applied: bool,
    /// 未生效的原因（能力不支持 / 播放器拒绝 / 没媒体）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// 生效后的实际取值（例如 set-loop 之后回读到的 loopMode）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective: Option<String>,
}

impl ControlOutcome {
    pub fn ok(action: &str) -> Self {
        Self { action: action.into(), applied: true, reason: None, effective: None }
    }

    pub fn ok_effective(action: &str, effective: impl Into<String>) -> Self {
        Self { action: action.into(), applied: true, reason: None, effective: Some(effective.into()) }
    }

    pub fn rejected(action: &str, reason: impl Into<String>) -> Self {
        Self { action: action.into(), applied: false, reason: Some(reason.into()), effective: None }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// 状态与诊断
// ══════════════════════════════════════════════════════════════════════════════

/// 单个数据源的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceState {
    /// 还没启动（懒启动：没人问就不启动）
    Idle,
    /// 正在准备（macOS 首次会编译/申请权限）
    Preparing,
    Running,
    /// 环境缺东西（没装依赖、没授权）
    Unavailable,
    /// 用户拒绝了权限
    Denied,
    Error,
}

/// 数据源状态 + 给用户看的排障提示。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceStatus {
    pub name: String,
    pub state: SourceState,
    /// 面向用户的提示（如何修复）
    pub hint: String,
}

impl SourceStatus {
    pub fn new(name: &str, state: SourceState, hint: impl Into<String>) -> Self {
        Self { name: name.into(), state, hint: hint.into() }
    }
}

/// 整体状态（`status` 方法返回的就是它）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub platform: String,
    pub arch: String,
    pub version: String,
    /// 当前会话后端：`macos-mediaremote` / `windows-gsmtc` / `linux-mpris` / `mock`
    pub provider: String,
    /// 各数据源的健康状况（metadata / audio / lyrics / artwork）
    pub sources: Vec<SourceStatus>,
    /// 已编译进来的可选能力：`http` / `audio` / `embedded` / `online`
    pub features: Vec<String>,
    /// 缓存目录（封面与歌词落在这里）
    pub cache_dir: String,
    /// 当前生效的轮询间隔（毫秒）——播放中/空闲/突发三种档位不一样
    pub poll_interval_ms: u64,
    /// 事件订阅者数量
    pub subscribers: usize,
    /// 已完成的轮询次数
    pub polls: u64,
    /// 运行时长（毫秒）
    pub uptime_ms: u64,
}

/// 频谱帧（第 1 期的「音频」部分）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpectrumFrame {
    /// 每段 0-255（对数刻度，与旧实现逐位一致）
    pub bands: Vec<u8>,
    /// 帧内整体能量 0-255
    pub peak: u8,
    /// 帧内 RMS（0.0-1.0）
    pub rms: f32,
    /// 采集采样率
    pub sample_rate: u32,
    /// 生成时刻（Unix 毫秒）
    pub ts_ms: u64,
}

impl SpectrumFrame {
    pub fn silent(now_ms: u64, sample_rate: u32) -> Self {
        Self {
            bands: vec![0; SPECTRUM_BANDS],
            peak: 0,
            rms: 0.0,
            sample_rate,
            ts_ms: now_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_position_extrapolates_only_while_playing() {
        let mut p = Playback {
            state: PlaybackState::Playing,
            position_ms: 10_000,
            position_source: PositionSource::Polled,
            duration_ms: 200_000,
            rate: 1.0,
            updated_at_ms: 1_000_000,
            ..Default::default()
        };
        assert_eq!(p.position_at(1_005_000), 15_000);
        // 外推不会超过时长
        assert_eq!(p.position_at(9_999_999), 200_000);
        p.state = PlaybackState::Paused;
        assert_eq!(p.position_at(1_005_000), 10_000);
        // 位置不可用时不外推
        p.state = PlaybackState::Playing;
        p.position_source = PositionSource::Unavailable;
        assert_eq!(p.position_at(1_005_000), 10_000);
        // 倍速
        p.position_source = PositionSource::Polled;
        p.rate = 2.0;
        assert_eq!(p.position_at(1_005_000), 20_000);
    }

    #[test]
    fn loop_mode_cycles_off_playlist_track_off() {
        assert_eq!(LoopMode::Off.next(), LoopMode::Playlist);
        assert_eq!(LoopMode::Playlist.next(), LoopMode::Track);
        assert_eq!(LoopMode::Track.next(), LoopMode::Off);
    }

    #[test]
    fn lyrics_active_index_picks_last_line_at_or_before_position() {
        let l = Lyrics {
            source: "test".into(),
            synced: true,
            offset_ms: 0,
            lines: vec![
                LrcLine { t_ms: 1_000, text: "a".into() },
                LrcLine { t_ms: 5_000, text: "b".into() },
                LrcLine { t_ms: 9_000, text: "c".into() },
            ],
            track_key: "k".into(),
            raw: None,
        };
        assert_eq!(l.active_index(0), Some(0));
        assert_eq!(l.active_index(1_000), Some(0));
        assert_eq!(l.active_index(4_999), Some(0));
        assert_eq!(l.active_index(5_000), Some(1));
        assert_eq!(l.active_index(60_000), Some(2));
    }

    #[test]
    fn transport_command_wire_shape_is_stable() {
        let j = serde_json::to_string(&TransportCommand::SeekBy { delta_ms: -15_000 }).unwrap();
        assert_eq!(j, r#"{"action":"seek-by","deltaMs":-15000}"#);
        let back: TransportCommand = serde_json::from_str(&j).unwrap();
        assert_eq!(back, TransportCommand::SeekBy { delta_ms: -15_000 });

        let j = serde_json::to_string(&TransportCommand::SetLoop { mode: LoopMode::Track }).unwrap();
        assert_eq!(j, r#"{"action":"set-loop","mode":"track"}"#);
    }

    #[test]
    fn capability_gate_uses_the_declared_bits() {
        let caps = Capabilities { next: true,..Capabilities::NONE };
        let gate = TransportCommand::Next.required_capability().unwrap();
        assert!(gate(&caps));
        let gate = TransportCommand::Previous.required_capability().unwrap();
        assert!(!gate(&caps));
    }
}
