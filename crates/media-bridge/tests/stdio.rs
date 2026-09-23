//! stdio（NDJSON）协议集成测试 —— **真起子进程，真走管道**。
//!
//! 单元测试覆盖的是「分发逻辑」，这里覆盖的是「宿主实际会碰到的那条路」：
//! 进程启动、第一行协议、请求/响应配对、事件插在响应之间、控制命令往返、
//! 以及 stdin 关闭后进程自己退出。宿主接入时踩的坑基本都在这条路上。

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// 测试里的宿主。
struct Host {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    /// 响应之外收到的事件（按到达顺序）
    events: Vec<Value>,
}

impl Host {
    fn start() -> Self {
        // 每个 Host 一个独立缓存目录：测试是并发跑的，共用目录会互相 prune 掉封面
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let cache = std::env::temp_dir().join(format!("mb-stdio-it-{}-{seq}", std::process::id()));
        let mut child = Command::new(env!("CARGO_BIN_EXE_media-bridge"))
            .args([
                "serve",
                "--provider",
                "mock",
                "--no-audio", // 集成测试不碰系统音频（不申请权限、不出声音）
                "--no-online",
                "--cache-dir",
            ])
            .arg(&cache)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("无法启动 media-bridge");
        let stdin = child.stdin.take().expect("stdin");
        let stdout: ChildStdout = child.stdout.take().expect("stdout");
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
            events: Vec::new(),
        }
    }

    fn send(&mut self, value: Value) {
        let mut line = value.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).expect("写 stdin");
        self.stdin.flush().expect("flush");
    }

    /// 读一行；事件被攒进 `events`，返回的是响应行。超时返回 `None`。
    ///
    /// 注意：**收到的事件绝不能丢** —— 事件可能在响应之前或之后到达，
    /// 谁丢掉谁就会写出「明明发了事件却等不到」的假失败。
    fn pump_line(&mut self, timeout: Duration) -> Option<Value> {
        loop {
            match self.lines.recv_timeout(timeout) {
                Ok(line) => {
                    let v: Value = serde_json::from_str(&line)
                        .unwrap_or_else(|e| panic!("协议行不是合法 JSON：{e}\n{line}"));
                    if v.get("event").is_some() {
                        self.events.push(v);
                        continue;
                    }
                    return Some(v);
                }
                Err(RecvTimeoutError::Timeout) => return None,
                Err(RecvTimeoutError::Disconnected) => panic!("子进程提前退出（stdout 关闭）"),
            }
        }
    }

    fn next_line(&mut self, timeout: Duration) -> Value {
        let budget = timeout;
        self.pump_line(budget).unwrap_or_else(|| {
            panic!(
                "等待协议响应超时（{budget:?}）；已收到事件 {} 条",
                self.events.len()
            )
        })
    }

    /// 请求并等到同 id 的响应。
    fn call(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(json!({"id": id, "method": method, "params": params}));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let v = self.next_line(remaining.max(Duration::from_millis(200)));
            if v.get("ok").is_some() {
                assert_eq!(v.get("id"), Some(&json!(id)), "响应 id 与请求不匹配：{v}");
                return v;
            }
        }
    }

    fn call_ok(&mut self, id: i64, method: &str, params: Value) -> Value {
        let v = self.call(id, method, params);
        assert_eq!(v["ok"], true, "{method} 应成功：{v}");
        v["result"].clone()
    }

    /// 等到某一类事件出现（最多等多久）。
    fn wait_event(&mut self, name: &str, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(ev) = self.events.iter().find(|e| e["event"] == name) {
                return ev.clone();
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                panic!(
                    "等不到事件 {name}（已收到：{:?}）",
                    self.events
                        .iter()
                        .map(|e| e["event"].clone())
                        .collect::<Vec<_>>()
                );
            }
            // 继续读（事件会被 pump_line 收进 events，响应行这里不需要）
            let _ = self.pump_line(remaining.min(Duration::from_millis(200)));
        }
    }

    fn shutdown(mut self) {
        drop(self.stdin);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                Ok(None) => {
                    let _ = self.child.kill();
                    panic!("stdin 关闭后子进程没有自行退出（5s）");
                }
                Err(e) => panic!("try_wait 失败：{e}"),
            }
        }
    }
}

#[test]
fn stdio_session_covers_the_host_contract() {
    let mut host = Host::start();

    // 1) hello：协议版本 + 方法清单（宿主接进来的第一件事）
    let hello = host.call_ok(1, "hello", json!({}));
    assert_eq!(hello["protocol"], 1);
    assert!(hello["methods"].as_array().unwrap().len() >= 10, "方法清单应完整");
    assert_eq!(hello["provider"], "mock");

    // 2) now：拿到曲目 + 封面已落盘 + 能力位
    let now = host.call_ok(2, "now", json!({}));
    assert_eq!(now["hasMedia"], true);
    let art_path = now["track"]["artwork"]["path"]
        .as_str()
        .unwrap_or_else(|| panic!("封面应有本地路径，实际 now = {now}"));
    assert!(
        std::path::Path::new(art_path).exists(),
        "封面文件应真实存在：{art_path}"
    );
    assert_eq!(now["track"]["artwork"]["mime"], "image/png");
    assert_eq!(now["capabilities"]["play"], true);

    // 3) 第 2 期：控制命令往返（发出去 → 回执 → 状态真的变了）
    let paused = host.call_ok(3, "control", json!({"action": "pause"}));
    assert_eq!(paused["outcome"]["applied"], true);
    assert_eq!(paused["now"]["playback"]["state"], "paused");

    let seeked = host.call_ok(4, "control", json!({"action": "seek-by", "deltaMs": 30000}));
    assert_eq!(seeked["outcome"]["applied"], true);
    let pos = seeked["now"]["playback"]["positionMs"].as_u64().unwrap();
    assert!(pos >= 30_000, "快进 30s 后位置应 >= 30s，实际 {pos}");

    // 4) 订阅 + 事件：next 之后应该收到 track 事件（换曲）
    let sub = host.call_ok(5, "subscribe", json!({"events": ["track", "playback"]}));
    assert!(sub["events"].as_array().unwrap().iter().any(|e| e == "track"));
    host.call_ok(6, "control", json!({"action": "next"}));
    let ev = host.wait_event("track", Duration::from_secs(5));
    assert_eq!(ev["event"], "track");
    assert!(ev["now"]["hasMedia"].as_bool().unwrap());

    // 5) 状态与配置
    let status = host.call_ok(7, "status", json!({}));
    assert_eq!(status["provider"], "mock");
    assert!(status["sources"].as_array().unwrap().len() >= 3);
    let cfg = host.call_ok(8, "config", json!({}));
    assert_eq!(cfg["provider"], "mock");

    // 6) 错误路径：未知方法要报协议错，并且**不能把连接搞坏**
    let bad = host.call(9, "nope", json!({}));
    assert_eq!(bad["ok"], false);
    assert_eq!(bad["error"]["code"], "protocol");
    let after = host.call_ok(10, "now", json!({}));
    assert_eq!(after["hasMedia"], true, "报错之后连接应仍然可用");

    // 7) 歌词：假播放器不给歌词 → 返回 null 而不是报错
    let lyrics = host.call_ok(11, "lyrics", json!({}));
    assert!(lyrics.is_null(), "没有歌词时应返回 null，实际 {lyrics}");

    // 8) stdin 关闭 → 自行退出
    host.shutdown();
}

#[test]
fn stdio_rejects_garbage_lines_without_dying() {
    let mut host = Host::start();
    host.stdin.write_all(b"not-json-at-all\n".as_ref()).expect("写");
    host.stdin.flush().expect("flush");
    let err = host.next_line(Duration::from_secs(5));
    assert_eq!(err["ok"], false);
    assert_eq!(err["error"]["code"], "protocol");
    // 连接仍然可用
    let hello = host.call_ok(1, "hello", json!({}));
    assert_eq!(hello["protocol"], 1);
    host.shutdown();
}

#[test]
fn stdio_spectrum_keeps_the_64_band_shape_without_audio() {
    let mut host = Host::start();
    // --no-audio：频谱应该是「全 0 但有正确形状」，而不是报错
    let frame = host.call_ok(1, "spectrum", json!({}));
    let bands = frame["bands"].as_array().expect("bands 数组");
    assert_eq!(bands.len(), 64, "必须始终是 64 段（与旧实现一致）");
    assert!(bands.iter().all(|b| b.as_u64() == Some(0)));
    assert_eq!(frame["sampleRate"], 16000);
    host.shutdown();
}
