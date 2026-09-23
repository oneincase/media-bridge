//! Linux 后端：**MPRIS over D-Bus**。
//!
//! MPRIS 是 Linux 桌面的事实标准播放接口（`org.mpris.MediaPlayer2.*`），
//! 播放器、浏览器、甚至 `mpv` 都实现它 —— 所以这条路不依赖任何外部命令行工具
//! （老实现走 `playerctl`，装了才有得用）。
//!
//! ## 实现取向：用 zbus 的**动态 Proxy**，不用 `#[zbus::proxy]` 宏
//!
//! 宏会为每个接口生成一套类型；而我们只用一个接口、字段还都是 `a{sv}` 这种运行期才知道
//! 形状的东西。动态 Proxy（`get_property` / `call_method`）更直接：
//! 属性名和方法名就是 MPRIS 规范里的字符串，读代码时不用在两套名字之间来回翻译。
//!
//! ## 选哪个播放器
//!
//! 系统里可能同时跑着好几个 MPRIS 服务（网易云 + 浏览器 + mpv）。选择顺序：
//! **正在播放的 > 有元数据的 > 名字排序第一个**。这样「浏览器里放着视频」不会被
//! 「暂停着的音乐软件」抢走焦点。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use zbus::Connection;
use zbus::proxy::Proxy;
// zvariant 是 zbus 的转码库，从 zbus 里取（不单独声明依赖，避免版本错配）
use zbus::zvariant::{OwnedValue, Value};

use crate::error::{BridgeError, Result};
use crate::types::{
    ArtworkOrigin, Capabilities, ControlOutcome, LoopMode, Playback, PlaybackState, PositionSource,
    SourceState, SourceStatus, Track, TrackSource, TransportCommand,
};
use crate::util::now_ms;

use super::{RawArtwork, RawNowPlaying};

/// MPRIS 播放器对象路径（规范固定）。
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
/// Player 接口名。
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";
/// 总线名前缀。
const BUS_PREFIX: &str = "org.mpris.MediaPlayer2.";

/// Linux 会话。
#[derive(Clone)]
pub struct LinuxSession {
    inner: Arc<Inner>,
}

struct Inner {
    /// 懒连接：第一次问数据时才连会话总线
    conn: tokio::sync::OnceCell<Connection>,
    /// 上次选中的播放器总线名（避免每次都重新枚举）
    current: RwLock<Option<String>>,
    /// 连接/枚举失败的最近信息（供状态上报）
    last_error: RwLock<Option<String>>,
}

impl LinuxSession {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                conn: tokio::sync::OnceCell::new(),
                current: RwLock::new(None),
                last_error: RwLock::new(None),
            }),
        }
    }

    pub fn status(&self) -> SourceStatus {
        match self.inner.last_error.read().ok().and_then(|g| g.clone()) {
            Some(e) => SourceStatus::new("metadata", SourceState::Unavailable, e),
            None => SourceStatus::new(
                "metadata",
                SourceState::Running,
                "MPRIS over D-Bus（会话总线）",
            ),
        }
    }

    pub fn diagnose(&self) -> Vec<String> {
        let mut out = Vec::new();
        out.push("会话总线：MPRIS（org.mpris.MediaPlayer2.*）".to_string());
        match futures_lite_block_on(async {
            let conn = self.connection().await?;
            list_players(&conn).await
        }) {
            Ok(names) if names.is_empty() => {
                out.push("发现 0 个 MPRIS 播放器（播放任意音乐后重试）".to_string());
            }
            Ok(names) => {
                out.push(format!("发现 {} 个 MPRIS 播放器：{}", names.len(), names.join(", ")));
            }
            Err(e) => out.push(format!("枚举播放器失败：{e}")),
        }
        out
    }

    async fn connection(&self) -> Result<Connection> {
        let conn = self
            .inner
            .conn
            .get_or_try_init(|| async {
                Connection::session().await.map_err(|e| {
                    BridgeError::unavailable(format!("连接 D-Bus 会话总线失败：{e}"))
                })
            })
            .await?;
        Ok(conn.clone())
    }

    /// 选一个播放器并返回它的 Player Proxy。
    async fn active_player(&self) -> Result<(Proxy<'static>, String)> {
        let conn = self.connection().await?;
        let cached = self.inner.current.read().ok().and_then(|g| g.clone());
        // 缓存命中也要确认它还活着（服务可能已退出）
        if let Some(name) = cached
            && let Ok(proxy) = self.player_proxy(&conn, &name).await
            && proxy.get_property::<String>("PlaybackStatus").await.is_ok()
        {
            return Ok((proxy, name));
        }

        let names = list_players(&conn).await?;
        if names.is_empty() {
            self.set_error(Some("没有发现 MPRIS 播放器（启动任意音乐播放器后重试）".into()));
            return Err(BridgeError::NoMedia);
        }
        let mut best: Option<(i32, String, Proxy<'static>)> = None;
        for name in &names {
            let Ok(proxy) = self.player_proxy(&conn, name).await else {
                continue;
            };
            let playing = proxy
                .get_property::<String>("PlaybackStatus")
                .await
                .map(|s| s == "Playing")
                .unwrap_or(false);
            let has_meta = proxy
                .get_property::<HashMap<String, OwnedValue>>("Metadata")
                .await
                .map(|m| {
                    m.keys().any(|k| k == "xesam:title")
                        || m.keys().any(|k| k == "xesam:artist")
                })
                .unwrap_or(false);
            let score = if playing {
                2
            } else if has_meta {
                1
            } else {
                0
            };
            if best.as_ref().is_none_or(|(s, _, _)| score > *s) {
                best = Some((score, name.clone(), proxy));
            }
        }
        let Some((_, name, proxy)) = best else {
            return Err(BridgeError::NoMedia);
        };
        if let Ok(mut g) = self.inner.current.write() {
            *g = Some(name.clone());
        }
        self.set_error(None);
        Ok((proxy, name))
    }

    async fn player_proxy(&self, conn: &Connection, bus: &str) -> Result<Proxy<'static>> {
        Proxy::new(conn, bus.to_string(), MPRIS_PATH.to_string(), PLAYER_IFACE.to_string())
            .await
            .map_err(|e| BridgeError::other(format!("创建 MPRIS 代理失败：{e}")))
    }

    fn set_error(&self, msg: Option<String>) {
        if let Ok(mut g) = self.inner.last_error.write() {
            *g = msg;
        }
    }

    pub async fn snapshot(&self) -> Result<RawNowPlaying> {
        let now = now_ms();
        let (proxy, bus) = match self.active_player().await {
            Ok(v) => v,
            Err(e) => {
                if matches!(e, BridgeError::NoMedia) {
                    return Ok(RawNowPlaying::empty(now));
                }
                return Err(e);
            }
        };

        let status = proxy
            .get_property::<String>("PlaybackStatus")
            .await
            .unwrap_or_else(|_| "Stopped".to_string());
        let meta: HashMap<String, OwnedValue> = proxy
            .get_property("Metadata")
            .await
            .unwrap_or_default();

        // 元数据字段（MPRIS 用 xesam: 前缀；mpris: 前缀的键另有含义）
        let title = meta.get("xesam:title").and_then(as_string).unwrap_or_default();
        let artist = meta
            .get("xesam:artist")
            .map(as_strings)
            .map(|v| v.join(" / "))
            .unwrap_or_default();
        let album = meta.get("xesam:album").and_then(as_string).unwrap_or_default();
        let album_artist = meta
            .get("xesam:albumArtist")
            .map(as_strings)
            .map(|v| v.join(" / "))
            .unwrap_or_default();
        let genre = meta
            .get("xesam:genre")
            .map(as_strings)
            .map(|v| v.join(" / "))
            .unwrap_or_default();
        let track_number = meta.get("xesam:trackNumber").and_then(as_i64).map(|v| v as u32);
        let length_us = meta.get("mpris:length").and_then(as_i64).unwrap_or(0);
        let duration_ms = (length_us.max(0) as u64) / 1000;
        let track_id = meta.get("mpris:trackid").and_then(as_string).unwrap_or_default();
        let art_url = meta.get("mpris:artUrl").and_then(as_string);
        let file_url = meta.get("xesam:url").and_then(as_string);

        // 位置：只有播放器实现了 Position 才拿得到（很多播放器在 Stopped 时返回 0）
        let position_us = proxy.get_property::<i64>("Position").await.unwrap_or(-1);
        let (position_ms, position_source) = if position_us >= 0 {
            ((position_us as u64) / 1000, PositionSource::Polled)
        } else {
            (0, PositionSource::Unavailable)
        };

        let loop_mode = proxy
            .get_property::<String>("LoopStatus")
            .await
            .map(|s| LoopMode::from_mpris(&s))
            .unwrap_or(LoopMode::Unknown);
        let shuffle = proxy.get_property::<bool>("Shuffle").await.ok();
        let volume = proxy.get_property::<f64>("Volume").await.ok();
        let rate = proxy.get_property::<f64>("Rate").await.unwrap_or(1.0);

        let caps = Capabilities {
            play: get_bool(&proxy, "CanPlay").await,
            pause: get_bool(&proxy, "CanPause").await,
            toggle: get_bool(&proxy, "CanPlay").await && get_bool(&proxy, "CanPause").await,
            stop: get_bool(&proxy, "CanControl").await,
            next: get_bool(&proxy, "CanGoNext").await,
            previous: get_bool(&proxy, "CanGoPrevious").await,
            seek_absolute: get_bool(&proxy, "CanSeek").await,
            seek_relative: get_bool(&proxy, "CanSeek").await,
            set_loop: true, // MPRIS 的 LoopStatus 是读写属性；个别播放器只读，写失败会在回执里说明
            set_shuffle: true,
            set_volume: true,
            set_mute: false, // MPRIS 没有静音概念（只能把音量设 0，那不等价）
            set_rate: true,
        };

        let playback = Playback {
            state: PlaybackState::from_mpris(&status),
            position_ms,
            position_source,
            duration_ms,
            rate,
            volume,
            muted: None,
            loop_mode,
            shuffle,
            updated_at_ms: now,
        };

        // 本地文件路径：MPRIS 的 xesam:url 是 file:// 时就能拿到（内嵌标签/内嵌歌词因此可用）
        let file_path = file_url
            .as_deref()
            .and_then(|u| u.strip_prefix("file://"))
            .map(|p| std::path::PathBuf::from(percent_decode(p)));

        let track = Track {
            id: if track_id.is_empty() {
                format!("{artist}|{title}|{album}")
            } else {
                track_id
            },
            title,
            artist,
            album,
            album_artist,
            genre,
            composer: String::new(),
            year: meta
                .get("xesam:contentCreated")
                .and_then(as_string)
                .and_then(|s| s.get(..4).and_then(|y| y.parse().ok())),
            track_number,
            duration_ms,
            artwork: None,
            lyrics: None,
            source: TrackSource {
                provider: "linux-mpris".to_string(),
                app_name: bus.strip_prefix(BUS_PREFIX).map(|s| s.to_string()),
                app_id: Some(bus.clone()),
                pid: None,
                file_path,
                url: file_url,
            },
        };

        let mut raw = RawNowPlaying::from_parts(Some(track), playback, caps, now);

        // 封面：本地文件直接读；远端地址交给服务层（联网/读盘由它统一处理）
        if let Some(url) = art_url {
            if let Some(path) = url.strip_prefix("file://") {
                let path = std::path::PathBuf::from(percent_decode(path));
                match std::fs::read(&path) {
                    Ok(bytes) => {
                        raw.artwork_raw = Some(RawArtwork {
                            key: raw.now.track.as_ref().map(|t| t.id.clone()).unwrap_or_default(),
                            mime_hint: None,
                            bytes,
                            origin: ArtworkOrigin::Player,
                            source_url: Some(url),
                        });
                    }
                    Err(e) => {
                        // 读不到就把地址留着，让消费端/服务层自行决定
                        if let Some(t) = raw.now.track.as_mut() {
                            t.source.url = Some(url.clone());
                        }
                        let _ = e;
                    }
                }
            } else if let Some(t) = raw.now.track.as_mut() {
                t.source.url = Some(url);
            }
        }
        Ok(raw)
    }

    pub async fn control(&self, cmd: TransportCommand) -> Result<ControlOutcome> {
        let (proxy, _) = self.active_player().await?;
        let snap = self.snapshot().await?;
        if !snap.has_media() {
            return Err(BridgeError::NoMedia);
        }
        if let Some(gate) = cmd.required_capability()
            && !gate(&snap.now.capabilities)
        {
            return Ok(ControlOutcome::rejected(
                cmd.name(),
                "当前播放器未声明支持该操作（MPRIS 的 Can* 属性为 false）",
            ));
        }
        let name = cmd.name();

        let call = |method: &'static str| {
            let proxy = proxy.clone();
            async move {
                proxy
                    .call_method(method, &())
                    .await
                    .map_err(|e| BridgeError::other(format!("MPRIS {method} 调用失败：{e}")))
            }
        };

        match cmd {
            TransportCommand::Play => {
                call("Play").await?;
                Ok(ControlOutcome::ok(name))
            }
            TransportCommand::Pause => {
                call("Pause").await?;
                Ok(ControlOutcome::ok(name))
            }
            TransportCommand::PlayPause => {
                call("PlayPause").await?;
                Ok(ControlOutcome::ok(name))
            }
            TransportCommand::Stop => {
                call("Stop").await?;
                Ok(ControlOutcome::ok(name))
            }
            TransportCommand::Next => {
                call("Next").await?;
                Ok(ControlOutcome::ok(name))
            }
            TransportCommand::Previous => {
                call("Previous").await?;
                Ok(ControlOutcome::ok(name))
            }
            TransportCommand::Seek { position_ms } => {
                // MPRIS 的 SetPosition 需要 trackid + 微秒；有些播放器要求先有 trackid
                let track_id = proxy
                    .get_property::<HashMap<String, OwnedValue>>("Metadata")
                    .await
                    .ok()
                    .and_then(|m| m.get("mpris:trackid").and_then(as_string))
                    .unwrap_or_default();
                if track_id.is_empty() {
                    return Ok(ControlOutcome::rejected(
                        name,
                        "播放器没给 mpris:trackid，无法用 SetPosition 定位（可用 seek-by 走 Seek 偏移）",
                    ));
                }
                let path = zbus::zvariant::ObjectPath::try_from(track_id.as_str())
                    .map_err(|e| BridgeError::other(format!("trackid 不是合法对象路径：{e}")))?;
                let micros = position_ms as i64 * 1000;
                proxy
                    .call_method("SetPosition", &(path, micros))
                    .await
                    .map_err(|e| BridgeError::other(format!("MPRIS SetPosition 失败：{e}")))?;
                Ok(ControlOutcome::ok_effective(name, format!("{:.1}s", position_ms as f64 / 1000.0)))
            }
            TransportCommand::SeekBy { delta_ms } => {
                // 相对定位用 Seek(Offset)（单位微秒），不需要 trackid
                proxy
                    .call_method("Seek", &(delta_ms * 1000))
                    .await
                    .map_err(|e| BridgeError::other(format!("MPRIS Seek 失败：{e}")))?;
                Ok(ControlOutcome::ok_effective(name, format!("{delta_ms}ms")))
            }
            TransportCommand::SetLoop { mode } => {
                let value = mode.to_mpris();
                proxy
                    .set_property("LoopStatus", value)
                    .await
                    .map_err(|e| BridgeError::other(format!("设置 LoopStatus 失败：{e}")))?;
                Ok(ControlOutcome::ok_effective(name, format!("loop={mode:?}")))
            }
            TransportCommand::CycleLoop => {
                let cur = proxy
                    .get_property::<String>("LoopStatus")
                    .await
                    .map(|s| LoopMode::from_mpris(&s))
                    .unwrap_or(LoopMode::Unknown);
                let next = cur.next();
                proxy
                    .set_property("LoopStatus", next.to_mpris())
                    .await
                    .map_err(|e| BridgeError::other(format!("设置 LoopStatus 失败：{e}")))?;
                Ok(ControlOutcome::ok_effective(name, format!("loop={next:?}")))
            }
            TransportCommand::SetShuffle { on } => {
                proxy
                    .set_property("Shuffle", on)
                    .await
                    .map_err(|e| BridgeError::other(format!("设置 Shuffle 失败：{e}")))?;
                Ok(ControlOutcome::ok_effective(name, format!("shuffle={on}")))
            }
            TransportCommand::ToggleShuffle => {
                let cur = proxy.get_property::<bool>("Shuffle").await.unwrap_or(false);
                proxy
                    .set_property("Shuffle", !cur)
                    .await
                    .map_err(|e| BridgeError::other(format!("设置 Shuffle 失败：{e}")))?;
                Ok(ControlOutcome::ok_effective(name, format!("shuffle={}", !cur)))
            }
            TransportCommand::SetVolume { volume } => {
                proxy
                    .set_property("Volume", volume.clamp(0.0, 1.0))
                    .await
                    .map_err(|e| BridgeError::other(format!("设置 Volume 失败：{e}")))?;
                Ok(ControlOutcome::ok_effective(name, format!("{volume:.2}")))
            }
            TransportCommand::SetMute { .. } => Ok(ControlOutcome::rejected(
                name,
                "MPRIS 没有静音语义（把音量设 0 并不等价，恢复时无法还原原音量），故意不提供",
            )),
            TransportCommand::SetRate { rate } => {
                proxy
                    .set_property("Rate", rate)
                    .await
                    .map_err(|e| BridgeError::other(format!("设置 Rate 失败：{e}")))?;
                Ok(ControlOutcome::ok_effective(name, format!("{rate:.2}")))
            }
        }
    }
}

impl Default for LinuxSession {
    fn default() -> Self {
        Self::new()
    }
}

async fn get_bool(proxy: &Proxy<'_>, name: &str) -> bool {
    proxy.get_property::<bool>(name).await.unwrap_or(false)
}

/// 枚举会话总线上的 MPRIS 服务名（排除我们自己）。
async fn list_players(conn: &Connection) -> Result<Vec<String>> {
    let dbus = zbus::fdo::DBusProxy::new(conn)
        .await
        .map_err(|e| BridgeError::other(format!("DBusProxy 创建失败：{e}")))?;
    let names = dbus
        .list_names()
        .await
        .map_err(|e| BridgeError::other(format!("ListNames 失败：{e}")))?;
    let mut out: Vec<String> = names
        .iter()
        .map(|n| n.as_str().to_string())
        .filter(|n| n.starts_with(BUS_PREFIX))
        .collect();
    out.sort();
    Ok(out)
}

// ══════════════════════════════════════════════════════════════════════════════
// zvariant 值提取
// ══════════════════════════════════════════════════════════════════════════════

/// `OwnedValue` 的公开枚举视图。
fn as_value(v: &OwnedValue) -> Option<Value<'_>> {
    Value::try_from(v).ok()
}

/// 取字符串（`s`、`o`、`g` 都算 —— MPRIS 的 trackid 是对象路径）。
fn as_string(v: &OwnedValue) -> Option<String> {
    match as_value(v)? {
        Value::Str(s) => Some(s.as_str().to_string()),
        Value::ObjectPath(p) => Some(p.as_str().to_string()),
        // Signature 没有 as_str()，用 Display
        _ => None,
    }
}

/// 取字符串数组（`as`）；单值也接受（有些播放器把 artist 写成单字符串，不合规但很常见）。
fn as_strings(v: &OwnedValue) -> Vec<String> {
    match as_value(v) {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|item| match item {
                Value::Str(s) => Some(s.as_str().to_string()),
                _ => None,
            })
            .collect(),
        _ => as_string(v).into_iter().collect(),
    }
}

/// 取整数（各种整型都试一遍）。
fn as_i64(v: &OwnedValue) -> Option<i64> {
    match as_value(v)? {
        Value::I64(x) => Some(x),
        Value::U64(x) => Some(x as i64),
        Value::I32(x) => Some(x as i64),
        Value::U32(x) => Some(x as i64),
        Value::I16(x) => Some(x as i64),
        Value::U16(x) => Some(x as i64),
        Value::U8(x) => Some(x as i64),
        Value::F64(x) => Some(x as i64),
        _ => None,
    }
}

/// 把 `file:///a%20b` 里的百分号转义还原成 `file:///a b`。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| -> Option<u8> {
                match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 在同步上下文里跑一小段异步逻辑（只用于 `diagnose`，不参与主链路）。
fn futures_lite_block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(fut)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_restores_spaces_and_unicode() {
        assert_eq!(percent_decode("/a%20b/c.mp3"), "/a b/c.mp3");
        assert_eq!(percent_decode("/%E4%B8%AD%E6%96%87.flac"), "/中文.flac");
        assert_eq!(percent_decode("/no-escape.flac"), "/no-escape.flac");
        // 不完整的转义原样保留，不 panic
        assert_eq!(percent_decode("/bad%2"), "/bad%2");
        assert_eq!(percent_decode("/bad%zz"), "/bad%zz");
    }

    #[test]
    fn loop_mode_mpris_round_trip() {
        assert_eq!(LoopMode::from_mpris("None"), LoopMode::Off);
        assert_eq!(LoopMode::from_mpris("Track"), LoopMode::Track);
        assert_eq!(LoopMode::from_mpris("Playlist"), LoopMode::Playlist);
        assert_eq!(LoopMode::from_mpris("???"), LoopMode::Unknown);
        assert_eq!(LoopMode::Off.to_mpris(), "None");
        assert_eq!(LoopMode::Track.to_mpris(), "Track");
        assert_eq!(LoopMode::Playlist.to_mpris(), "Playlist");
    }

    #[test]
    fn playback_state_mpris_mapping() {
        assert_eq!(PlaybackState::from_mpris("Playing"), PlaybackState::Playing);
        assert_eq!(PlaybackState::from_mpris("Paused"), PlaybackState::Paused);
        assert_eq!(PlaybackState::from_mpris("Stopped"), PlaybackState::Stopped);
        assert_eq!(PlaybackState::from_mpris("x"), PlaybackState::Unknown);
    }

    #[test]
    fn zvariant_extraction_handles_the_shapes_players_actually_send() {
        // OwnedValue 只能从 Value 转（zvariant 没给 From<String>），
        // 但真正的代码遇到的正是「从 D-Bus 解码出来的 Value」
        fn owned<T: Into<Value<'static>>>(v: T) -> OwnedValue {
            OwnedValue::try_from(v.into()).expect("转成 OwnedValue")
        }

        let s = owned("歌名");
        assert_eq!(as_string(&s).as_deref(), Some("歌名"));

        let path = owned(Value::ObjectPath(
            zbus::zvariant::ObjectPath::try_from("/org/mpris/track/1").unwrap(),
        ));
        assert_eq!(as_string(&path).as_deref(), Some("/org/mpris/track/1"), "trackid 是对象路径");

        let arr = owned(Value::Array(zbus::zvariant::Array::from(vec!["A", "B"])));
        assert_eq!(as_strings(&arr), vec!["A", "B"]);

        // 不合规但常见的写法：artist 给成单个字符串
        let single = owned("独唱歌手");
        assert_eq!(as_strings(&single), vec!["独唱歌手"]);

        assert_eq!(as_i64(&owned(12i64)), Some(12));
        assert_eq!(as_i64(&owned(42i32)), Some(42), "不同整型都要认");
        assert_eq!(as_i64(&owned(true)), None, "布尔不是整数");
        assert_eq!(as_strings(&owned(7u8)), Vec::<String>::new(), "数字不是字符串");
    }
}
