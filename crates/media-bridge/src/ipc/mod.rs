//! 线协议：**同一份语义，两种传输**。
//!
//! | 传输 | 什么时候用 | 入口 |
//! |---|---|---|
//! | NDJSON（stdin/stdout） | 宿主是另一个进程（Node / Python / Go / Electron） | `media-bridge serve` |
//! | HTTP + SSE + WebSocket | 多个消费者、浏览器/沙箱页面 | `media-bridge serve --http 127.0.0.1:8765` |
//!
//! 两种传输共用同一个 `dispatch`：**同一个方法名、同一份返回结构**。所以先按 stdio 接好，
//! 之后想加一个网页看板，不用改任何数据模型。
//!
//! ## 三种报文，靠字段名区分（不用嵌套包装）
//!
//! ```jsonc
//! // 请求（stdin 一行一条）
//! {"id":1,"method":"now","params":{}}
//! // 响应（一定带 id 与 ok）
//! {"id":1,"ok":true,"result":{...}}
//! {"id":2,"ok":false,"error":{"code":"no-media","message":"当前没有正在播放的媒体"}}
//! // 事件（一定带 event）
//! {"event":"track","now":{...}}
//! ```
//!
//! ## 可用方法
//!
//! | 方法 | 说明 |
//! |---|---|
//! | `hello` | 协议版本、平台、可用方法清单（接进来的第一件事） |
//! | `status` | 各数据源健康状况与排障提示 |
//! | `now` | 完整快照（曲目/进度/能力/封面/歌词） |
//! | `capabilities` | 只要能力位（置灰按钮用） |
//! | `spectrum` | 最新一帧 64 段频谱 |
//! | `artwork` | 当前封面的路径/MIME/大小（可选 base64） |
//! | `lyrics` | 当前歌词（可强制在线查询） |
//! | `control` | 第 2 期：传输控制（play/pause/next/prev/seek/loop/shuffle/…） |
//! | `refresh` | 立刻重新轮询一次并返回新快照 |
//! | `config` | 生效中的配置（排查用） |
//! | `subscribe` | 订阅哪些事件（连接级状态，由传输层持有） |

pub mod stdio;

#[cfg(feature = "http")]
pub mod http;

use std::collections::BTreeSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::audio::PcmChunk;
use crate::error::BridgeError;
use crate::service::{ControlReport, Event, MediaBridge};
use crate::types::{NowPlaying, TransportCommand};
use crate::util::now_ms;
use crate::VERSION;

/// 协议版本（破坏性变更时 +1）。
pub const PROTOCOL_VERSION: u32 = 1;

/// 请求。
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    /// 回执里原样带回；缺省时响应里为 `null`
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    pub fn new(method: &str, params: Value) -> Self {
        Self {
            id: None,
            method: method.to_string(),
            params,
        }
    }

    pub fn with_id(mut self, id: impl Into<Value>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// 取一个布尔参数（`true`/`"true"`/`1` 都认 —— 命令行与 HTTP 的传法不一样）。
    fn bool_param(&self, key: &str) -> Option<bool> {
        match self.params.get(key) {
            Some(Value::Bool(b)) => Some(*b),
            Some(Value::String(s)) => match s.as_str() {
                "true" | "1" | "yes" => Some(true),
                "false" | "0" | "no" => Some(false),
                _ => None,
            },
            Some(Value::Number(n)) => Some(n.as_i64().unwrap_or(0) != 0),
            _ => None,
        }
    }

    fn u64_param(&self, key: &str) -> Option<u64> {
        match self.params.get(key) {
            Some(Value::Number(n)) => n.as_u64(),
            Some(Value::String(s)) => s.parse().ok(),
            _ => None,
        }
    }
}

/// 响应。
#[derive(Debug, Clone, Serialize)]
pub struct Response {
    pub id: Option<Value>,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

impl Response {
    pub fn ok(id: Option<Value>, result: Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Option<Value>, err: &BridgeError) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(ErrorBody {
                code: err.code().to_string(),
                message: err.to_string(),
            }),
        }
    }
}

/// 事件订阅（**连接级**状态：每个 stdio 连接 / 每条 WS 各自持有）。
///
/// `spectrum` 默认**不在**订阅里：它按帧率出数据，没人要就不该占带宽。
/// 订阅方显式 `{"event":"spectrum","intervalMs":100}` 之后才会推。
#[derive(Debug, Clone, Default)]
pub struct Subscriptions {
    events: BTreeSet<String>,
    spectrum_interval_ms: Option<u64>,
}

impl Subscriptions {
    /// 默认订阅：除频谱外的全部事件（换曲/播放态/封面/歌词/状态/错误）。
    pub fn defaults() -> Self {
        let mut s = Self::default();
        s.events.insert("track".into());
        s.events.insert("playback".into());
        s.events.insert("artwork".into());
        s.events.insert("lyrics".into());
        s.events.insert("status".into());
        s.events.insert("error".into());
        s
    }

    /// 一条事件都不推（纯请求-响应模式）。
    pub fn none() -> Self {
        Self::default()
    }

    pub fn set(&mut self, events: &[String], spectrum_interval_ms: Option<u64>) {
        self.events = events.iter().cloned().collect();
        self.spectrum_interval_ms = if self.events.contains("spectrum") {
            Some(spectrum_interval_ms.unwrap_or(100).clamp(16, 5000))
        } else {
            None
        };
    }

    pub fn wants(&self, event: &str) -> bool {
        self.events.contains(event)
    }

    pub fn spectrum_interval(&self) -> Option<u64> {
        self.spectrum_interval_ms
    }

    pub fn list(&self) -> Vec<&str> {
        self.events.iter().map(|s| s.as_str()).collect()
    }
}

/// 方法清单（`hello` 里返回，也用于文档与自检）。
pub const METHODS: &[(&str, &str)] = &[
    ("hello", "协议版本/平台/可用方法"),
    ("status", "各数据源健康状况"),
    ("now", "完整快照（曲目/进度/能力/封面/歌词）"),
    ("capabilities", "只要传输控制能力位"),
    ("spectrum", "最新 64 段频谱"),
    ("artwork", "当前封面（路径/MIME/可选 base64）"),
    ("lyrics", "当前歌词（可强制在线）"),
    ("control", "传输控制：play/pause/play-pause/stop/next/previous/seek/seek-by/set-loop/cycle-loop/set-shuffle/toggle-shuffle"),
    ("refresh", "立刻重新轮询并返回新快照"),
    ("config", "生效中的配置"),
    ("subscribe", "设置本连接订阅的事件（含频谱）"),
];

/// 分发一条请求。
///
/// `subs` 由传输层持有（stdio / WS 各自一份）；HTTP 的一次性请求传 `Subscriptions::none()`。
pub async fn dispatch(
    bridge: &Arc<MediaBridge>,
    subs: &mut Subscriptions,
    req: Request,
) -> Response {
    let id = req.id.clone();
    match dispatch_inner(bridge, subs, &req).await {
        Ok(v) => Response::ok(id, v),
        Err(e) => Response::err(id, &e),
    }
}

async fn dispatch_inner(
    bridge: &Arc<MediaBridge>,
    subs: &mut Subscriptions,
    req: &Request,
) -> Result<Value, BridgeError> {
    match req.method.as_str() {
        "hello" => Ok(json!({
            "protocol": PROTOCOL_VERSION,
            "version": VERSION,
            "platform": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "provider": bridge.provider_name(),
            "pid": std::process::id(),
            "features": bridge.status().features,
            "methods": METHODS.iter().map(|(n, d)| json!({"name": n, "description": d})).collect::<Vec<_>>(),
        })),

        "status" => to_value(bridge.status()),

        "now" => {
            let now = if req.bool_param("interpolate").unwrap_or(true) {
                bridge.snapshot_at_now()
            } else {
                bridge.snapshot()
            };
            to_value(now)
        }

        "capabilities" => to_value(bridge.snapshot().capabilities),

        "spectrum" => {
            bridge.ensure_audio()?;
            // 冷启动（刚拉起采集那一下）时先等出第一帧再回，免得调用方拿到一屏 0 ——
            // 「第一次问频谱全是 0，第二次才对」是最容易被当成 bug 的行为。
            let mut frame = bridge.spectrum();
            if frame.ts_ms == 0 || frame.peak == 0 {
                for _ in 0..6 {
                    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                    frame = bridge.spectrum();
                    if frame.ts_ms > 0 && frame.peak > 0 {
                        break;
                    }
                }
            }
            if req.bool_param("bandsOnly").unwrap_or(false) {
                to_value(frame.bands)
            } else {
                to_value(frame)
            }
        }

        "artwork" => {
            let Some(art) = bridge.snapshot().track.and_then(|t| t.artwork) else {
                return Err(BridgeError::NoMedia);
            };
            let mut v = serde_json::to_value(&art).map_err(json_err)?;
            if req.bool_param("base64").unwrap_or(false)
                && let Some((bytes, _)) = bridge.artwork_bytes()
                && let Some(obj) = v.as_object_mut()
            {
                use base64::Engine;
                obj.insert(
                    "base64".into(),
                    Value::String(base64::engine::general_purpose::STANDARD.encode(bytes)),
                );
            }
            Ok(v)
        }

        "lyrics" => {
            let online = req.bool_param("online").unwrap_or(false);
            let lyrics = if online {
                bridge.refresh_lyrics(true).await
            } else {
                match bridge.lyrics() {
                    Some(l) => Some(l),
                    None => bridge.refresh_lyrics(false).await,
                }
            };
            to_value(lyrics)
        }

        "control" => {
            // 命令既支持 params 里带 action（推荐），也支持 {"action":"..."} 直接平铺
            let cmd: TransportCommand = serde_json::from_value(req.params.clone())
                .map_err(|e| BridgeError::Protocol(format!("control 参数不是合法的命令：{e}")))?;
            let report: ControlReport = bridge.control(cmd).await?;
            to_value(report)
        }

        "refresh" => {
            let now: NowPlaying = bridge.refresh().await?;
            to_value(now)
        }

        "config" => {
            let cfg = bridge.config();
            Ok(json!({
                "provider": bridge.provider_name(),
                "cacheDir": cfg.cache_dir().display().to_string(),
                "pollIntervalMs": cfg.poll_interval_ms,
                "idlePollIntervalMs": cfg.idle_poll_interval_ms,
                "burstIntervalMs": cfg.burst_interval_ms,
                "burstWindowMs": cfg.burst_window_ms,
                "lyricsOnline": cfg.lyrics_online,
                "audio": {
                    "enabled": cfg.audio.enabled,
                    "fps": cfg.audio.fps,
                    "pcm": cfg.audio.pcm,
                    "pcmChunk": cfg.audio.pcm_chunk,
                    "device": cfg.audio.device,
                },
            }))
        }

        "subscribe" => {
            let events: Vec<String> = req
                .params
                .get("events")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_else(|| {
                    vec![
                        "track".into(),
                        "playback".into(),
                        "artwork".into(),
                        "lyrics".into(),
                        "status".into(),
                        "error".into(),
                    ]
                });
            let interval = req.u64_param("intervalMs");
            subs.set(&events, interval);
            if subs.wants("spectrum") {
                bridge.ensure_audio()?;
            }
            Ok(json!({
                "events": subs.list(),
                "spectrumIntervalMs": subs.spectrum_interval(),
            }))
        }

        other => Err(BridgeError::Protocol(format!(
            "未知方法 {other:?}（可用：{}）",
            METHODS.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
        ))),
    }
}

fn to_value<T: Serialize>(v: T) -> Result<Value, BridgeError> {
    serde_json::to_value(v).map_err(json_err)
}

fn json_err(e: serde_json::Error) -> BridgeError {
    BridgeError::other(format!("序列化失败：{e}"))
}

/// 事件 → wire（与 `Response` 靠字段名区分：事件一定有 `event`）。
pub fn event_to_value(ev: &Event) -> Value {
    let mut v = serde_json::to_value(ev).unwrap_or_else(|_| json!({"event": "error"}));
    if let Some(obj) = v.as_object_mut() {
        obj.insert("tsMs".into(), json!(now_ms()));
    }
    v
}

/// PCM 块 → wire（样本用 base64 的 s16le，比 JSON 数字数组小一个数量级）。
pub fn pcm_to_value(chunk: &PcmChunk) -> Value {
    use base64::Engine;
    let mut bytes = Vec::with_capacity(chunk.samples.len() * 2);
    for s in chunk.samples.iter() {
        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    json!({
        "event": "pcm",
        "tsMs": chunk.ts_ms,
        "sampleRate": chunk.sample_rate,
        "channels": 1,
        "format": "s16le",
        "samples": chunk.samples.len(),
        "base64": base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::AudioConfig;
    use crate::platform::mock::MockConfig;
    use crate::platform::Provider;
    use crate::service::BridgeConfig;

    fn bridge() -> Arc<MediaBridge> {
        let dir = std::env::temp_dir().join(format!(
            "mb-ipc-{}",
            crate::util::short_hash(&crate::util::uuid_v4_ish())
        ));
        MediaBridge::new(BridgeConfig {
            provider: Provider::Mock,
            mock: MockConfig::default(),
            cache_dir: Some(dir),
            audio: AudioConfig { enabled: false, ..Default::default() },
            lyrics_online: false,
            ..Default::default()
        })
    }

    async fn call(b: &Arc<MediaBridge>, subs: &mut Subscriptions, method: &str, params: Value) -> Response {
        dispatch(b, subs, Request::new(method, params)).await
    }

    #[tokio::test]
    async fn hello_lists_protocol_and_methods() {
        let b = bridge();
        let mut subs = Subscriptions::defaults();
        let r = call(&b, &mut subs, "hello", json!({})).await;
        assert!(r.ok);
        let v = r.result.unwrap();
        assert_eq!(v["protocol"], PROTOCOL_VERSION);
        assert!(v["methods"].as_array().unwrap().len() >= 10);
        assert!(v["pid"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn unknown_method_returns_protocol_error_with_method_list() {
        let b = bridge();
        let mut subs = Subscriptions::defaults();
        let r = dispatch(&b, &mut subs, Request::new("nope", json!({})).with_id(7)).await;
        assert!(!r.ok);
        assert_eq!(r.id, Some(json!(7)));
        let err = r.error.unwrap();
        assert_eq!(err.code, "protocol");
        assert!(err.message.contains("now"), "错误里应列出可用方法");
    }

    #[tokio::test]
    async fn now_returns_track_after_poll() {
        let b = bridge();
        b.poll_once().await.unwrap();
        let mut subs = Subscriptions::defaults();
        let r = call(&b, &mut subs, "now", json!({})).await;
        let v = r.result.unwrap();
        assert_eq!(v["hasMedia"], true);
        assert!(v["track"]["title"].as_str().unwrap().len() > 0);
        assert!(v["capabilities"]["play"].as_bool().unwrap());
        assert!(v["track"]["artwork"]["path"].as_str().unwrap().ends_with(".png"));
    }

    #[tokio::test]
    async fn control_round_trips_through_the_wire() {
        let b = bridge();
        b.poll_once().await.unwrap();
        let mut subs = Subscriptions::defaults();
        let r = call(&b, &mut subs, "control", json!({"action": "pause"})).await;
        assert!(r.ok, "{:?}", r.error);
        let v = r.result.unwrap();
        assert_eq!(v["outcome"]["applied"], true);
        assert_eq!(v["now"]["playback"]["state"], "paused");

        // 第 2 期的六种操作逐个走一遍
        for cmd in [
            json!({"action":"play"}),
            json!({"action":"next"}),
            json!({"action":"previous"}),
            json!({"action":"seek-by","deltaMs":-5000}),
            json!({"action":"cycle-loop"}),
            json!({"action":"set-shuffle","on":true}),
        ] {
            let r = call(&b, &mut subs, "control", cmd.clone()).await;
            assert!(r.ok, "{cmd} 失败：{:?}", r.error);
            assert_eq!(r.result.unwrap()["outcome"]["applied"], true, "{cmd} 未生效");
        }
    }

    #[tokio::test]
    async fn control_with_bad_params_is_protocol_error() {
        let b = bridge();
        let mut subs = Subscriptions::defaults();
        let r = call(&b, &mut subs, "control", json!({"action": "fly"})).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "protocol");
    }

    #[tokio::test]
    async fn subscribe_controls_event_selection() {
        let b = bridge();
        let mut subs = Subscriptions::defaults();
        assert!(subs.wants("track") && !subs.wants("spectrum"));
        let r = call(&b, &mut subs, "subscribe", json!({"events": ["spectrum", "track"], "intervalMs": 50})).await;
        assert!(r.ok);
        assert!(subs.wants("spectrum"));
        assert!(!subs.wants("playback"));
        assert_eq!(subs.spectrum_interval(), Some(50));
        // 只订阅 track：spectrum 的间隔应被清掉
        call(&b, &mut subs, "subscribe", json!({"events": ["track"]})).await;
        assert_eq!(subs.spectrum_interval(), None);
    }

    #[tokio::test]
    async fn artmoork_or_lyrics_without_media_reports_no_media() {
        let b = bridge();
        let mut subs = Subscriptions::defaults();
        let r = call(&b, &mut subs, "artwork", json!({})).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "no-media");
    }

    #[tokio::test]
    async fn spectrum_and_event_serialization_shapes_are_stable() {
        let b = bridge();
        b.poll_once().await.unwrap();
        let mut subs = Subscriptions::defaults();
        let r = call(&b, &mut subs, "spectrum", json!({})).await;
        let v = r.result.unwrap();
        assert_eq!(v["bands"].as_array().unwrap().len(), crate::types::SPECTRUM_BANDS);
        assert!(v["tsMs"].as_u64().unwrap() > 0);

        let ev = event_to_value(&Event::Playback { now: b.snapshot() });
        assert_eq!(ev["event"], "playback");
        assert!(ev["tsMs"].as_u64().unwrap() > 0);

        let chunk = PcmChunk {
            samples: std::sync::Arc::from(vec![0.5f32, -0.5, 1.0]),
            sample_rate: 16_000,
            ts_ms: 1,
        };
        let pv = pcm_to_value(&chunk);
        assert_eq!(pv["event"], "pcm");
        assert_eq!(pv["format"], "s16le");
        assert_eq!(pv["samples"], 3);
        assert!(pv["base64"].as_str().unwrap().len() > 4);
    }
}
