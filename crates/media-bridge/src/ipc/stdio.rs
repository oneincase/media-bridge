//! NDJSON over stdin/stdout —— **宿主是另一个进程时首选这条**。
//!
//! 为什么不是「启动一个 HTTP 服务让宿主连」：宿主（Node 插件、Electron 主进程、Python 脚本）
//! 起子进程是最自然的做法，stdin/stdout 天然随进程生命周期收放，不需要端口协商、不需要
//! 处理「端口被占」、不需要鉴权 —— 而且子进程死了宿主立刻知道。
//!
//! 约定：
//!   - **stdout 只走协议**（一行一条 JSON），日志一律走 stderr；
//!   - 收到 `EOF`（宿主关掉 stdin）就干净退出；
//!   - 请求与事件**可以交错**，靠字段名区分（`ok`/`id` 是响应，`event` 是事件）。

use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc};

use super::{Request, Subscriptions, dispatch, event_to_value, pcm_to_value};
use crate::error::Result;
use crate::service::{Event, MediaBridge};
use crate::util::now_ms;

/// 启动 stdio 服务（阻塞直到 stdin 关闭或收到信号）。
///
/// `verbose` 时把收到的请求也回一行到 stderr，方便排查宿主那边发了什么。
pub async fn serve(bridge: Arc<MediaBridge>, verbose: bool) -> Result<()> {
    // 写出口：所有响应与事件都排进这条队列，由单个任务串行写出 ——
    // 避免多任务并发写 stdout 交错成半个 JSON
    let (tx, mut rx) = mpsc::channel::<Value>(4096);
    let writer = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(v) = rx.recv().await {
            let mut line = match serde_json::to_string(&v) {
                Ok(s) => s,
                Err(_) => continue,
            };
            line.push('\n');
            if out.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if out.flush().await.is_err() {
                break;
            }
        }
    });

    let subs = Arc::new(Mutex::new(Subscriptions::defaults()));

    // 事件转发：订阅表是连接级的，每次发之前现查一遍（订阅可以随时改）
    {
        let mut events = bridge.subscribe();
        let tx = tx.clone();
        let subs = subs.clone();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(ev) => {
                        let want = subs.lock().await.wants(ev.name());
                        if want && tx.send(event_to_value(&ev)).await.is_err() {
                            break;
                        }
                    }
                    // 慢消费者丢帧：事件是「变化通知」，丢一条不该拖垮整个连接
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
    }

    // 频谱转发：只在被订阅时按订阅间隔推（默认不推）
    {
        let tx = tx.clone();
        let subs = subs.clone();
        let bridge = bridge.clone();
        tokio::spawn(async move {
            loop {
                let interval = subs.lock().await.spectrum_interval();
                match interval {
                    Some(ms) => {
                        bridge.ensure_audio().ok();
                        let frame = bridge.spectrum();
                        let v = serde_json::json!({
                            "event": "spectrum",
                            "frame": frame,
                            "tsMs": now_ms(),
                        });
                        if tx.send(v).await.is_err() {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                    }
                    None => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
                }
            }
        });
    }

    // 原始 PCM 转发
    if let Some(mut pcm) = bridge.subscribe_pcm() {
        let tx = tx.clone();
        let subs = subs.clone();
        tokio::spawn(async move {
            loop {
                match pcm.recv().await {
                    Ok(chunk) => {
                        if subs.lock().await.wants("pcm") && tx.send(pcm_to_value(&chunk)).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
    }

    // 请求读取循环
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if verbose {
            eprintln!("[media-bridge] ← {}", crate::util::truncate(line, 400));
        }
        let req: Request = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                let resp = super::Response::err(
                    None,
                    &crate::error::BridgeError::Protocol(format!("请求不是合法 JSON 行：{e}")),
                );
                let _ = tx.send(serde_json::to_value(resp).unwrap_or(Value::Null)).await;
                continue;
            }
        };
        let mut guard = subs.lock().await;
        let resp = dispatch(&bridge, &mut guard, req).await;
        drop(guard);
        if tx.send(serde_json::to_value(resp).unwrap_or(Value::Null)).await.is_err() {
            break;
        }
    }

    // stdin 关闭：把队列里剩下的响应写完再退出
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), writer).await;
    bridge.stop();
    Ok(())
}

/// 把一条事件也写进 stderr（`--verbose` 排查用）。
pub fn log_event_stderr(ev: &Event) {
    eprintln!("[media-bridge] {} {}", ev.name(), crate::util::truncate(&event_to_value(ev).to_string(), 300));
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
            "mb-stdio-{}",
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

    /// 直接把「宿主会发的那几行」跑一遍分发，验证协议层（不依赖真实 stdin）。
    #[tokio::test]
    async fn typical_host_session_flows() {
        let b = bridge();
        b.poll_once().await.unwrap();
        let mut subs = Subscriptions::defaults();
        for (method, params) in [
            ("hello", serde_json::json!({})),
            ("status", serde_json::json!({})),
            ("now", serde_json::json!({})),
            ("capabilities", serde_json::json!({})),
            ("spectrum", serde_json::json!({})),
            ("config", serde_json::json!({})),
        ] {
            let resp = dispatch(&b, &mut subs, Request::new(method, params)).await;
            assert!(resp.ok, "{method} 应成功：{:?}", resp.error);
            assert!(resp.result.is_some());
        }
    }

    #[test]
    fn event_value_has_event_and_timestamp() {
        let ev = Event::Error {
            source: "test".into(),
            code: "io".into(),
            message: "boom".into(),
        };
        let v = event_to_value(&ev);
        assert_eq!(v["event"], "error");
        assert_eq!(v["source"], "test");
        assert!(v["tsMs"].as_u64().unwrap() > 0);
    }
}
