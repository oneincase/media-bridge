//! 本地 HTTP / SSE / WebSocket 服务。
//!
//! 适用场景：**多个消费者**（壁纸页面 + 桌宠 + 网页看板 + 另一个插件），或者消费者是
//! 浏览器/沙箱页面（它们没法起子进程、也拿不到宿主的 stdio）。
//!
//! 端点一览（与 stdio 的 method 一一对应）：
//!
//! | 方法 | 路径 | 说明 |
//! |---|---|---|
//! | GET | `/health` | 存活探针 |
//! | GET | `/v1/{method}` | 只读方法：hello/status/now/capabilities/spectrum/lyrics/config |
//! | GET | `/v1/artwork` | 当前封面（**二进制图片**，带正确 Content-Type） |
//! | POST | `/v1/control` | 传输控制（body 就是一条命令 JSON） |
//! | POST | `/v1/rpc` | 通用调用（body 是 `{method,params}`） |
//! | GET | `/v1/events` | SSE 事件流（`?events=track,playback` 过滤） |
//! | GET | `/v1/ws` | WebSocket（请求/事件双向，PCM 走二进制帧） |
//! | GET | `/v1/pcm` | 原始 PCM 的 NDJSON 流（16kHz 单声道 s16le 的 base64） |
//!
//! ## 安全边界
//!
//! 默认只绑 `127.0.0.1`，并且默认对任意来源开放 CORS（`--cors '*'`）—— 因为典型消费者
//! 是另一个源的沙箱页面，收紧到同源会让「接入」这件事变得很难。代价是：**你机器上
//! 任意网页都能读到你在听什么、并能控制播放**。要收紧就用 `--cors off` 或指定来源，
//! 并注意别把服务绑到非回环地址（绑了会打警告）。

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::{Router, extract::Request};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{Request as RpcRequest, Subscriptions, dispatch, event_to_value, pcm_to_value};
use crate::error::{BridgeError, Result};
use crate::service::MediaBridge;

/// CORS 策略。
#[derive(Debug, Clone)]
pub enum Cors {
    /// 允许任意来源（默认：沙箱页面接入最省事）
    Any,
    /// 只允许指定来源
    Origins(Vec<String>),
    /// 不带 CORS 头（只允许同源/非浏览器客户端）
    Off,
}

impl Cors {
    /// 命令行取值：`*` / `off` / 逗号分隔的来源列表。
    pub fn parse(spec: &str) -> Self {
        let spec = spec.trim();
        if spec.is_empty() || spec == "*" {
            return Cors::Any;
        }
        if spec.eq_ignore_ascii_case("off") || spec.eq_ignore_ascii_case("none") {
            return Cors::Off;
        }
        Cors::Origins(spec.split(',').map(|s| s.trim().to_string()).collect())
    }
}

/// HTTP 服务配置。
#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub addr: SocketAddr,
    pub cors: Cors,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            addr: SocketAddr::from(([127, 0, 0, 1], 8765)),
            cors: Cors::Any,
        }
    }
}

/// 启动 HTTP 服务（阻塞）。
pub async fn serve(bridge: Arc<MediaBridge>, config: HttpConfig) -> Result<()> {
    let cors = config.cors.clone();
    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/artwork", get(artwork))
        .route("/v1/control", post(control))
        .route("/v1/rpc", post(rpc))
        .route("/v1/events", get(sse_events))
        .route("/v1/ws", get(ws_upgrade))
        .route("/v1/pcm", get(pcm_stream))
        .route("/v1/{method}", get(readonly))
        .layer(middleware::from_fn(move |req, next| {
            let cors = cors.clone();
            async move { cors_layer(cors, req, next).await }
        }))
        .with_state(bridge.clone());

    let listener = tokio::net::TcpListener::bind(config.addr)
        .await
        .map_err(|e| BridgeError::other(format!("监听 {} 失败：{e}", config.addr)))?;
    let local = listener.local_addr().unwrap_or(config.addr);
    eprintln!("[media-bridge] HTTP 已就绪：http://{local}（SSE /v1/events，WS /v1/ws）");
    if !local.ip().is_loopback() {
        eprintln!(
            "[media-bridge] ⚠️ 绑定在非回环地址 {local}：局域网内任何人都能读取与控制系统播放"
        );
    }
    axum::serve(listener, app)
        .await
        .map_err(|e| BridgeError::other(format!("HTTP 服务退出：{e}")))?;
    Ok(())
}

// ══════════════════════════════════════════════════════════════════════════════
// CORS
// ══════════════════════════════════════════════════════════════════════════════

fn apply_cors(cors: &Cors, headers: &mut axum::http::HeaderMap, origin: Option<&str>) {
    let allow = match cors {
        Cors::Any => Some("*".to_string()),
        Cors::Origins(list) => origin
            .filter(|o| list.iter().any(|x| x == o))
            .map(|o| o.to_string()),
        Cors::Off => None,
    };
    let Some(allow) = allow else { return };
    if let Ok(v) = HeaderValue::from_str(&allow) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
    }
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type"),
    );
    headers.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
}

async fn cors_layer(cors: Cors, req: Request, next: Next) -> Response {
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    if req.method() == Method::OPTIONS {
        let mut resp = StatusCode::NO_CONTENT.into_response();
        apply_cors(&cors, resp.headers_mut(), origin.as_deref());
        return resp;
    }
    let mut resp = next.run(req).await;
    apply_cors(&cors, resp.headers_mut(), origin.as_deref());
    resp
}

// ══════════════════════════════════════════════════════════════════════════════
// 基础端点
// ══════════════════════════════════════════════════════════════════════════════

/// JSON 应答，**显式带 `charset=utf-8`**。
///
/// 这不是洁癖：JSON 规范默认就是 UTF-8，但**PowerShell 5.1 的 `Invoke-RestMethod`
/// 在 `content-type` 不带 charset 时会按 Latin-1 解码** —— 于是中文元数据在 Windows 脚本里
/// 全变乱码（实测：`测` 被拆成三个字节）。Windows 侧是我们要支持的宿主平台之一，
/// 所以这里把 charset 写死，让这类客户端也能正确解码。
fn json_utf8(value: Value) -> Response {
    let mut resp = Json(value).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    resp
}

async fn health() -> impl IntoResponse {
    json_utf8(json!({ "ok": true, "service": "media-bridge", "version": crate::VERSION }))
}

/// 查询串 → 参数对象（值一律按字符串给，分发层认得 "true"/"1"）。
fn params_from_query(q: &HashMap<String, String>) -> Value {
    let mut map = serde_json::Map::new();
    for (k, v) in q {
        if k == "events" {
            continue;
        }
        map.insert(k.clone(), Value::String(v.clone()));
    }
    Value::Object(map)
}

/// `GET /v1/{method}` —— 只读方法统一入口。
async fn readonly(
    State(bridge): State<Arc<MediaBridge>>,
    axum::extract::Path(method): axum::extract::Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let mut subs = Subscriptions::none();
    let req = RpcRequest::new(&method, params_from_query(&q));
    let resp = dispatch(&bridge, &mut subs, req).await;
    match (resp.ok, resp.result, resp.error) {
        (true, Some(v), _) => json_utf8(v),
        (_, _, Some(e)) => {
            let code = match e.code.as_str() {
                "no-media" => StatusCode::NOT_FOUND,
                "protocol" => StatusCode::BAD_REQUEST,
                "unsupported" | "unavailable" | "denied" | "not-supported" => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (code, json_utf8(json!({ "error": e }))).into_response()
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `GET /v1/artwork` —— 直接出图（沙箱页面用 `<img src>` 或 `fetch` 都行）。
async fn artwork(State(bridge): State<Arc<MediaBridge>>) -> Response {
    let Some((bytes, mime)) = bridge.artwork_bytes() else {
        return (StatusCode::NOT_FOUND, "当前没有封面").into_response();
    };
    let ct = HeaderValue::from_str(&mime)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    let mut resp = Response::new(Body::from(bytes));
    resp.headers_mut().insert(header::CONTENT_TYPE, ct);
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, max-age=0, must-revalidate"),
    );
    resp
}

/// `POST /v1/control` —— body 就是一条命令。
async fn control(State(bridge): State<Arc<MediaBridge>>, body: String) -> Response {
    let params: Value = if body.trim().is_empty() {
        Value::Object(Default::default())
    } else {
        match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    json_utf8(json!({ "error": { "code": "protocol", "message": format!("body 不是合法 JSON：{e}") } })),
                )
                    .into_response();
            }
        }
    };
    let mut subs = Subscriptions::none();
    let resp = dispatch(&bridge, &mut subs, RpcRequest::new("control", params)).await;
    json_response(resp)
}

/// `POST /v1/rpc` —— 通用调用。
async fn rpc(State(bridge): State<Arc<MediaBridge>>, body: String) -> Response {
    let req: std::result::Result<RpcRequest, _> = serde_json::from_str(&body);
    match req {
        Ok(req) => {
            let mut subs = Subscriptions::none();
            let resp = dispatch(&bridge, &mut subs, req).await;
            json_response(resp)
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            json_utf8(json!({ "error": { "code": "protocol", "message": format!("body 不是合法请求：{e}") } })),
        )
            .into_response(),
    }
}

fn json_response(resp: super::Response) -> Response {
    if resp.ok {
        json_utf8(resp.result.unwrap_or(Value::Null))
    } else {
        let e = resp.error.unwrap_or(super::ErrorBody {
            code: "other".into(),
            message: "未知错误".into(),
        });
        let code = match e.code.as_str() {
            "no-media" => StatusCode::NOT_FOUND,
            "protocol" => StatusCode::BAD_REQUEST,
            "unsupported" | "unavailable" | "denied" | "not-supported" => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (code, json_utf8(json!({ "error": e }))).into_response()
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// 流式端点
// ══════════════════════════════════════════════════════════════════════════════

/// `GET /v1/events?events=track,playback&spectrumMs=100`
async fn sse_events(
    State(bridge): State<Arc<MediaBridge>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let wants: Vec<String> = q
        .get("events")
        .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
        .unwrap_or_default();
    let spectrum_ms = q
        .get("spectrumMs")
        .and_then(|s| s.parse::<u64>().ok())
        .map(|v| v.clamp(16, 5000));
    let want_spectrum = wants.iter().any(|w| w == "spectrum") || spectrum_ms.is_some();
    if want_spectrum || wants.iter().any(|w| w == "pcm") {
        bridge.ensure_audio().ok();
    }

    let mut events = bridge.subscribe();
    let bridge_for_stream = bridge.clone();
    let stream = async_stream_lite(move |tx| async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(
            spectrum_ms.unwrap_or(100),
        ));
        loop {
            tokio::select! {
                ev = events.recv() => match ev {
                    Ok(ev) => {
                        if !wants.is_empty() && !wants.iter().any(|w| w == ev.name()) {
                            continue;
                        }
                        let data = event_to_value(&ev).to_string();
                        if tx.send(SseEvent::default().event(ev.name()).data(data)).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                },
                _ = ticker.tick(), if want_spectrum => {
                    let frame = bridge_for_stream.spectrum();
                    let data = json!({"event":"spectrum","frame":frame}).to_string();
                    if tx.send(SseEvent::default().event("spectrum").data(data)).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut resp = Sse::new(stream).keep_alive(KeepAlive::default()).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    resp
}

/// 极简的「把闭包变成 Stream」工具：用一个 mpsc 通道桥接。
///
/// 不引 async-stream crate —— 这点胶水不值得多一个依赖。
fn async_stream_lite<F, Fut>(f: F) -> impl futures_util::Stream<Item = std::result::Result<SseEvent, Infallible>>
where
    F: FnOnce(mpsc::Sender<SseEvent>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<SseEvent>(256);
    tokio::spawn(f(tx));
    futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (Ok(item), rx))
    })
}

/// `GET /v1/pcm` —— 原始 PCM 的 NDJSON 流。
async fn pcm_stream(State(bridge): State<Arc<MediaBridge>>) -> Response {
    let _ = bridge.ensure_audio();
    let Some(pcm) = bridge.subscribe_pcm() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            json_utf8(json!({ "error": { "code": "unavailable", "message": "PCM 未启用：用 --pcm 启动（AudioConfig.pcm = true）" } })),
        )
            .into_response();
    };
    let stream = futures_util::stream::unfold(pcm, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(chunk) => {
                    let mut line = pcm_to_value(&chunk).to_string();
                    line.push('\n');
                    return Some((Ok::<_, Infallible>(line), rx));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return None,
            }
        }
    })
    .map(|chunk| match chunk {
        Ok(s) => Ok::<_, std::io::Error>(s),
        Err(_) => Err(std::io::Error::other("pcm")),
    });
    let mut resp = Response::new(Body::from_stream(stream));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson"),
    );
    resp
}

/// `GET /v1/ws`
async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(bridge): State<Arc<MediaBridge>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_session(socket, bridge))
}

async fn ws_session(socket: WebSocket, bridge: Arc<MediaBridge>) {
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(512);

    // 单写者：所有出站消息都排进队列，避免多任务并发写同一个 sink
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    let subs = Arc::new(tokio::sync::Mutex::new(Subscriptions::defaults()));

    // 事件 → 客户端
    {
        let mut events = bridge.subscribe();
        let tx = out_tx.clone();
        let subs = subs.clone();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(ev) => {
                        if !subs.lock().await.wants(ev.name()) {
                            continue;
                        }
                        let line = event_to_value(&ev).to_string();
                        if tx.send(Message::Text(line.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
    }

    // 频谱（被订阅时按间隔推）
    {
        let tx = out_tx.clone();
        let subs = subs.clone();
        let bridge = bridge.clone();
        tokio::spawn(async move {
            loop {
                let interval = subs.lock().await.spectrum_interval();
                match interval {
                    Some(ms) => {
                        bridge.ensure_audio().ok();
                        let frame = bridge.spectrum();
                        let line = json!({"event":"spectrum","frame":frame}).to_string();
                        if tx.send(Message::Text(line.into())).await.is_err() {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                    }
                    None => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
                }
            }
        });
    }

    // PCM → 二进制帧
    if let Some(mut pcm) = bridge.subscribe_pcm() {
        let tx = out_tx.clone();
        let subs = subs.clone();
        tokio::spawn(async move {
            while let Ok(chunk) = pcm.recv().await {
                if !subs.lock().await.wants("pcm") {
                    continue;
                }
                let mut bytes = Vec::with_capacity(chunk.samples.len() * 2);
                for s in chunk.samples.iter() {
                    bytes.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
                }
                if tx.send(Message::Binary(bytes.into())).await.is_err() {
                    break;
                }
            }
        });
    }

    // 入站请求
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(text) => {
                let resp = match serde_json::from_str::<RpcRequest>(&text) {
                    Ok(req) => {
                        let mut guard = subs.lock().await;
                        dispatch(&bridge, &mut guard, req).await
                    }
                    Err(e) => super::Response::err(
                        None,
                        &BridgeError::Protocol(format!("不是合法请求：{e}")),
                    ),
                };
                let line = serde_json::to_string(&resp).unwrap_or_else(|_| "{}".into());
                if out_tx.send(Message::Text(line.into())).await.is_err() {
                    break;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    drop(out_tx);
    let _ = writer.await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cors_spec_parsing() {
        assert!(matches!(Cors::parse("*"), Cors::Any));
        assert!(matches!(Cors::parse(""), Cors::Any));
        assert!(matches!(Cors::parse("off"), Cors::Off));
        match Cors::parse("http://a.test, http://b.test") {
            Cors::Origins(list) => assert_eq!(list, vec!["http://a.test", "http://b.test"]),
            other => panic!("应为来源列表，实际 {other:?}"),
        }
    }

    #[test]
    fn cors_headers_reflect_origin_only_for_allowed_origins() {
        let mut h = axum::http::HeaderMap::new();
        apply_cors(&Cors::Origins(vec!["http://ok.test".into()]), &mut h, Some("http://ok.test"));
        assert_eq!(h.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(), "http://ok.test");

        let mut h2 = axum::http::HeaderMap::new();
        apply_cors(&Cors::Origins(vec!["http://ok.test".into()]), &mut h2, Some("http://evil.test"));
        assert!(h2.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none(), "非白名单来源不该拿到 CORS 头");

        let mut h3 = axum::http::HeaderMap::new();
        apply_cors(&Cors::Off, &mut h3, Some("http://ok.test"));
        assert!(h3.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
    }

    #[test]
    fn query_params_become_json_object_without_events_key() {
        let mut q = HashMap::new();
        q.insert("interpolate".to_string(), "false".to_string());
        q.insert("events".to_string(), "track".to_string());
        let v = params_from_query(&q);
        assert_eq!(v["interpolate"], "false");
        assert!(v.get("events").is_none(), "events 是订阅专用参数，不该混进方法参数");
    }
}
