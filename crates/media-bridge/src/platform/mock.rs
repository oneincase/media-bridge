//! 内存假播放器：不依赖任何系统媒体服务。
//!
//! 三个用途，都很实在：
//!
//!   1. **联调**：宿主还没接播放器时，`resolve` 出来的协议、事件、封面落盘、歌词解析
//!      全都能跑通（`media-bridge serve --provider mock`）。
//!   2. **测试**：第 2 期的反向控制要有「发了命令 → 状态真的变了」的断言，
//!      用真播放器做不稳定也不可断言，用假播放器就是确定性的。
//!   3. **跨平台验证**：Windows/Linux 上也能演示完整链路。
//!
//! 它会**真的执行**控制命令并改变自己的状态（包括循环/随机/音量/倍速），
//! 所以是一条可往返验证的链路，而不是只回一个「已收到」。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::{BridgeError, Result};
use crate::types::{
    ArtworkOrigin, Capabilities, ControlOutcome, LoopMode, Playback, PlaybackState, PositionSource,
    SourceState, SourceStatus, Track, TrackSource, TransportCommand,
};
use crate::util::{now_ms, short_hash};

use super::{RawArtwork, RawNowPlaying};

/// 假播放器的配置。
#[derive(Debug, Clone)]
pub struct MockConfig {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub genre: String,
    pub duration_ms: u64,
    /// 初始是否在播放
    pub playing: bool,
    /// 初始位置（毫秒）
    pub position_ms: u64,
    /// 播放速率
    pub rate: f64,
    /// 是否造封面（生成的是一张真的 PNG，会走完整的嗅探/落盘/尺寸解析链路）
    pub with_artwork: bool,
    /// 是否带 LRC 歌词
    pub with_lyrics: bool,
    /// 对外申报的 provider 名（默认 `mock`）
    pub provider_label: String,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            title: "示例曲目".to_string(),
            artist: "示例歌手".to_string(),
            album: "示例专辑".to_string(),
            album_artist: "示例歌手".to_string(),
            genre: "Test".to_string(),
            duration_ms: 215_000,
            playing: true,
            position_ms: 12_000,
            rate: 1.0,
            with_artwork: true,
            with_lyrics: true,
            provider_label: "mock".to_string(),
        }
    }
}

impl MockConfig {
    /// 从 JSON 覆盖字段（`--mock-config '{...}'` 用）。
    pub fn from_json(json: &str) -> Result<Self> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase", default)]
        struct Patch {
            title: Option<String>,
            artist: Option<String>,
            album: Option<String>,
            duration_ms: Option<u64>,
            playing: Option<bool>,
            position_ms: Option<u64>,
            rate: Option<f64>,
            with_artwork: Option<bool>,
            with_lyrics: Option<bool>,
            provider_label: Option<String>,
        }
        impl Default for Patch {
            fn default() -> Self {
                Self {
                    title: None,
                    artist: None,
                    album: None,
                    duration_ms: None,
                    playing: None,
                    position_ms: None,
                    rate: None,
                    with_artwork: None,
                    with_lyrics: None,
                    provider_label: None,
                }
            }
        }
        let patch: Patch = serde_json::from_str(json)
            .map_err(|e| BridgeError::Protocol(format!("mock 配置不是合法 JSON：{e}")))?;
        let mut cfg = Self::default();
        if let Some(v) = patch.title {
            cfg.title = v;
        }
        if let Some(v) = patch.artist {
            cfg.artist = v;
        }
        if let Some(v) = patch.album {
            cfg.album = v;
        }
        if let Some(v) = patch.duration_ms {
            cfg.duration_ms = v;
        }
        if let Some(v) = patch.playing {
            cfg.playing = v;
        }
        if let Some(v) = patch.position_ms {
            cfg.position_ms = v;
        }
        if let Some(v) = patch.rate {
            cfg.rate = v;
        }
        if let Some(v) = patch.with_artwork {
            cfg.with_artwork = v;
        }
        if let Some(v) = patch.with_lyrics {
            cfg.with_lyrics = v;
        }
        if let Some(v) = patch.provider_label {
            cfg.provider_label = v;
        }
        Ok(cfg)
    }
}

#[derive(Debug, Clone)]
struct State {
    config: MockConfig,
    state: PlaybackState,
    /// 位置锚点（毫秒）与锚点时刻 —— 播放中由二者外推当前位置
    position_ms: u64,
    anchor_ms: u64,
    rate: f64,
    loop_mode: LoopMode,
    shuffle: bool,
    volume: f64,
    muted: bool,
    /// 第几首（next/prev 会变）—— 用来让「换曲」在这条链路上真的发生
    track_index: u32,
}

impl State {
    fn current_position(&self, now: u64) -> u64 {
        if !self.state.is_playing() {
            return self.position_ms;
        }
        let elapsed = now.saturating_sub(self.anchor_ms) as f64;
        let pos = self.position_ms as f64 + elapsed * self.rate.max(0.0);
        let pos = pos.max(0.0);
        if self.config.duration_ms > 0 {
            (pos.min(self.config.duration_ms as f64)) as u64
        } else {
            pos as u64
        }
    }

    /// 把位置从「锚点 + 外推」折叠成新的锚点（任何改位置/改速的操作后都要调）。
    fn reanchor(&mut self, now: u64) {
        self.position_ms = self.current_position(now);
        self.anchor_ms = now;
    }
}

/// 假播放器会话。
pub struct MockSession {
    inner: Arc<Mutex<State>>,
}

impl MockSession {
    pub fn new() -> Self {
        Self::with_config(MockConfig::default())
    }

    pub fn with_config(config: MockConfig) -> Self {
        let state = State {
            rate: config.rate,
            state: if config.playing { PlaybackState::Playing } else { PlaybackState::Paused },
            position_ms: config.position_ms,
            anchor_ms: now_ms(),
            loop_mode: LoopMode::Off,
            shuffle: false,
            volume: 1.0,
            muted: false,
            track_index: 0,
            config,
        };
        Self {
            inner: Arc::new(Mutex::new(state)),
        }
    }

    pub fn status(&self) -> SourceStatus {
        SourceStatus::new(
            "metadata",
            SourceState::Running,
            "内置假播放器（--provider mock；用于联调，不反映真实系统状态）",
        )
    }

    /// 自检报告。
    pub fn diagnose(&self) -> Vec<String> {
        let st = self.lock();
        vec![
            "会话后端：内置假播放器（不接触任何系统媒体服务）".to_string(),
            format!(
                "当前曲目：{} — {}（{}）",
                st.config.artist, st.config.title, st.config.album
            ),
            format!(
                "播放态：{:?}，位置 {:.1}s / {:.1}s，速率 {}",
                st.state,
                st.position_ms as f64 / 1000.0,
                st.config.duration_ms as f64 / 1000.0,
                st.rate
            ),
            format!(
                "封面：{}；歌词：{}",
                if st.config.with_artwork { "生成 PNG" } else { "不生成" },
                if st.config.with_lyrics { "生成 LRC" } else { "不生成" }
            ),
        ]
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub async fn snapshot(&self) -> Result<RawNowPlaying> {
        let now = now_ms();
        let mut st = self.lock();
        let position = st.current_position(now);
        // 播完自动从头（避免测试里等太久出现「位置 > 时长」的怪状态）
        if st.state.is_playing() && st.config.duration_ms > 0 && position >= st.config.duration_ms {
            st.position_ms = 0;
            st.anchor_ms = now;
            st.track_index = st.track_index.wrapping_add(1);
        }
        let position = st.current_position(now);

        let track_no = st.track_index;
        let title = if track_no == 0 {
            st.config.title.clone()
        } else {
            format!("{} #{track_no}", st.config.title)
        };
        let id = format!("mock:{}", short_hash(&title));
        let duration = st.config.duration_ms;

        let playback = Playback {
            state: st.state,
            position_ms: position,
            position_source: PositionSource::Polled,
            duration_ms: duration,
            rate: st.rate,
            volume: Some(st.volume),
            muted: Some(st.muted),
            loop_mode: st.loop_mode,
            shuffle: Some(st.shuffle),
            updated_at_ms: now,
        };

        let track = Track {
            id,
            title,
            artist: st.config.artist.clone(),
            album: st.config.album.clone(),
            album_artist: st.config.album_artist.clone(),
            genre: st.config.genre.clone(),
            composer: String::new(),
            year: Some(2026),
            track_number: Some(track_no + 1),
            duration_ms: duration,
            artwork: None,
            lyrics: None,
            source: TrackSource {
                provider: st.config.provider_label.clone(),
                app_name: Some("media-bridge mock".to_string()),
                app_id: Some("dev.media-bridge.mock".to_string()),
                pid: Some(std::process::id()),
                file_path: None,
                url: None,
            },
        };

        let mut raw = RawNowPlaying::from_parts(Some(track), playback, mock_capabilities(), now);
        if st.config.with_artwork
            && let Some(t) = raw.now.track.as_ref()
        {
            let bytes = crate::util::encode_png_rgb(160, 160, track_no);
            raw.artwork_raw = Some(RawArtwork {
                key: t.id.clone(),
                mime_hint: Some("image/png".to_string()),
                bytes,
                origin: ArtworkOrigin::Player,
                source_url: None,
            });
        }
        Ok(raw)
    }

    pub async fn control(&self, cmd: TransportCommand) -> Result<ControlOutcome> {
        let now = now_ms();
        let mut st = self.lock();
        // 绝大多数命令需要「有媒体」；假播放器永远有媒体，这里只演示语义
        let name = cmd.name();
        match cmd {
            TransportCommand::Play => {
                st.state = PlaybackState::Playing;
                st.reanchor(now);
                Ok(ControlOutcome::ok(name))
            }
            TransportCommand::Pause => {
                let pos = st.current_position(now);
                st.position_ms = pos;
                st.anchor_ms = now;
                st.state = PlaybackState::Paused;
                Ok(ControlOutcome::ok(name))
            }
            TransportCommand::PlayPause => {
                let pos = st.current_position(now);
                st.position_ms = pos;
                st.anchor_ms = now;
                st.state = if st.state.is_playing() {
                    PlaybackState::Paused
                } else {
                    PlaybackState::Playing
                };
                Ok(ControlOutcome::ok_effective(name, format!("{:?}", st.state)))
            }
            TransportCommand::Stop => {
                st.state = PlaybackState::Stopped;
                st.position_ms = 0;
                st.anchor_ms = now;
                Ok(ControlOutcome::ok(name))
            }
            TransportCommand::Next => {
                st.track_index = st.track_index.wrapping_add(1);
                st.position_ms = 0;
                st.anchor_ms = now;
                Ok(ControlOutcome::ok_effective(name, format!("曲目 #{}", st.track_index)))
            }
            TransportCommand::Previous => {
                // 通用语义：播过 3 秒先回到本曲开头，否则回上一首
                if st.current_position(now) > 3_000 {
                    st.position_ms = 0;
                    st.anchor_ms = now;
                } else {
                    st.track_index = st.track_index.saturating_sub(1);
                    st.position_ms = 0;
                    st.anchor_ms = now;
                }
                Ok(ControlOutcome::ok(name))
            }
            TransportCommand::Seek { position_ms } => {
                let mut target = position_ms as i64;
                if st.config.duration_ms > 0 {
                    target = target.min(st.config.duration_ms as i64);
                }
                st.position_ms = target.max(0) as u64;
                st.anchor_ms = now;
                Ok(ControlOutcome::ok_effective(name, format!("{:.1}s", st.position_ms as f64 / 1000.0)))
            }
            TransportCommand::SeekBy { delta_ms } => {
                let cur = st.current_position(now) as i64;
                let mut target = cur + delta_ms;
                if st.config.duration_ms > 0 {
                    target = target.min(st.config.duration_ms as i64);
                }
                st.position_ms = target.max(0) as u64;
                st.anchor_ms = now;
                Ok(ControlOutcome::ok_effective(name, format!("{:.1}s", st.position_ms as f64 / 1000.0)))
            }
            TransportCommand::SetLoop { mode } => {
                st.loop_mode = mode;
                Ok(ControlOutcome::ok_effective(name, format!("{mode:?}")))
            }
            TransportCommand::CycleLoop => {
                st.loop_mode = st.loop_mode.next();
                Ok(ControlOutcome::ok_effective(name, format!("{:?}", st.loop_mode)))
            }
            TransportCommand::SetShuffle { on } => {
                st.shuffle = on;
                Ok(ControlOutcome::ok_effective(name, format!("{on}")))
            }
            TransportCommand::ToggleShuffle => {
                st.shuffle = !st.shuffle;
                Ok(ControlOutcome::ok_effective(name, format!("{}", st.shuffle)))
            }
            TransportCommand::SetVolume { volume } => {
                st.volume = volume.clamp(0.0, 1.0);
                Ok(ControlOutcome::ok_effective(name, format!("{:.2}", st.volume)))
            }
            TransportCommand::SetMute { muted } => {
                st.muted = muted;
                Ok(ControlOutcome::ok_effective(name, format!("{muted}")))
            }
            TransportCommand::SetRate { rate } => {
                let pos = st.current_position(now);
                st.position_ms = pos;
                st.anchor_ms = now;
                st.rate = rate.clamp(0.1, 8.0);
                Ok(ControlOutcome::ok_effective(name, format!("{:.2}", st.rate)))
            }
        }
    }

    /// 直接改配置（测试里用来切「播放/暂停」等）。
    pub fn set_config(&self, config: MockConfig) {
        let mut st = self.lock();
        st.config = config;
        st.state = if st.config.playing { PlaybackState::Playing } else { PlaybackState::Paused };
        st.position_ms = st.config.position_ms;
        st.anchor_ms = now_ms();
    }

    /// 造一段 LRC 歌词（测试歌词渲染/高亮用）。
    pub fn mock_lrc(&self) -> String {
        let st = self.lock();
        format!(
            "[ti:{}]\n[ar:{}]\n[al:{}]\n[offset:0]\n\
             [00:00.00]♪\n[00:03.00]第一句歌词\n[00:08.50]第二句歌词\n[00:14.00]第三句歌词\n[00:20.00]（间奏）\n[00:26.00]最后一句\n",
            st.config.title, st.config.artist, st.config.album
        )
    }
}

impl Default for MockSession {
    fn default() -> Self {
        Self::new()
    }
}

fn mock_capabilities() -> Capabilities {
    Capabilities {
        play: true,
        pause: true,
        toggle: true,
        stop: true,
        next: true,
        previous: true,
        seek_absolute: true,
        seek_relative: true,
        set_loop: true,
        set_shuffle: true,
        set_volume: true,
        set_mute: true,
        set_rate: true,
    }
}

/// 假播放器里的「睡眠」——避免某些测试用真实时间等待时写魔数。
pub async fn settle() {
    tokio::time::sleep(Duration::from_millis(20)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn snapshot_reports_track_and_capabilities() {
        let s = MockSession::new();
        let snap = s.snapshot().await.unwrap();
        assert!(snap.has_media());
        let t = snap.now.track.unwrap();
        assert_eq!(t.title, "示例曲目");
        assert!(snap.now.capabilities.set_loop);
        assert!(snap.artwork_raw.is_some());
    }

    #[tokio::test]
    async fn pause_freezes_position_and_play_resumes() {
        let s = MockSession::new();
        let p1 = s.snapshot().await.unwrap().now.playback.position_ms;
        s.control(TransportCommand::Pause).await.unwrap();
        let p2 = s.snapshot().await.unwrap().now.playback.position_ms;
        assert!(p2 >= p1, "暂停发生在首次取快照之后，位置只会往前");
        tokio::time::sleep(Duration::from_millis(60)).await;
        let p3 = s.snapshot().await.unwrap().now.playback.position_ms;
        assert_eq!(p2, p3, "暂停后位置必须冻结");
        assert_eq!(s.snapshot().await.unwrap().now.playback.state, PlaybackState::Paused);
        s.control(TransportCommand::Play).await.unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        let p4 = s.snapshot().await.unwrap().now.playback.position_ms;
        assert!(p4 > p3, "恢复播放后位置应继续前进");
    }

    #[tokio::test]
    async fn seek_by_moves_and_clamps() {
        let s = MockSession::new();
        let out = s.control(TransportCommand::SeekBy { delta_ms: 30_000 }).await.unwrap();
        assert!(out.applied);
        let pos = s.snapshot().await.unwrap().now.playback.position_ms;
        assert!((42_000..44_000).contains(&pos), "12s + 30s 应约等于 42s，实际 {pos}");
        // 快退到负值应夹到 0
        s.control(TransportCommand::SeekBy { delta_ms: -999_999 }).await.unwrap();
        assert_eq!(s.snapshot().await.unwrap().now.playback.position_ms, 0);
    }

    #[tokio::test]
    async fn next_previous_change_track_identity() {
        let s = MockSession::new();
        let id0 = s.snapshot().await.unwrap().now.track.unwrap().id;
        s.control(TransportCommand::Next).await.unwrap();
        let id1 = s.snapshot().await.unwrap().now.track.unwrap().id;
        assert_ne!(id0, id1, "下一曲应换曲目标识（换曲判定依赖它）");
        s.control(TransportCommand::Previous).await.unwrap();
        let id2 = s.snapshot().await.unwrap().now.track.unwrap().id;
        assert_eq!(id0, id2);
    }

    #[tokio::test]
    async fn loop_cycles_and_shuffle_toggles() {
        let s = MockSession::new();
        assert_eq!(s.snapshot().await.unwrap().now.playback.loop_mode, LoopMode::Off);
        s.control(TransportCommand::CycleLoop).await.unwrap();
        assert_eq!(s.snapshot().await.unwrap().now.playback.loop_mode, LoopMode::Playlist);
        s.control(TransportCommand::CycleLoop).await.unwrap();
        assert_eq!(s.snapshot().await.unwrap().now.playback.loop_mode, LoopMode::Track);
        s.control(TransportCommand::CycleLoop).await.unwrap();
        assert_eq!(s.snapshot().await.unwrap().now.playback.loop_mode, LoopMode::Off);

        s.control(TransportCommand::ToggleShuffle).await.unwrap();
        assert_eq!(s.snapshot().await.unwrap().now.playback.shuffle, Some(true));
    }

    #[tokio::test]
    async fn volume_mute_and_rate_are_applied() {
        let s = MockSession::new();
        s.control(TransportCommand::SetVolume { volume: 0.42 }).await.unwrap();
        s.control(TransportCommand::SetMute { muted: true }).await.unwrap();
        s.control(TransportCommand::SetRate { rate: 1.5 }).await.unwrap();
        let pb = s.snapshot().await.unwrap().now.playback;
        assert_eq!(pb.volume, Some(0.42));
        assert_eq!(pb.muted, Some(true));
        assert_eq!(pb.rate, 1.5);
    }

    #[test]
    fn config_from_json_overrides_subset() {
        let cfg = MockConfig::from_json(r#"{"title":"换首歌","durationMs":1000}"#).unwrap();
        assert_eq!(cfg.title, "换首歌");
        assert_eq!(cfg.duration_ms, 1000);
        assert_eq!(cfg.artist, MockConfig::default().artist, "未指定的字段保持默认");
        assert!(MockConfig::from_json("{not json}").is_err());
    }
}
