#![cfg(feature = "http")]
//! HTTP 传输的集成测试。
//!
//! 这里**故意不用 HTTP 客户端库**：一条 `GET` 请求手写出来也就十几行，而少一个依赖
//! 意味着这个中间件在任何宿主里都更容易被构建。测的是宿主/浏览器会看到的东西：
//! 状态码、`content-type`、CORS 头、以及 JSON 结构。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn start() -> Self {
        // 端口交给系统分配（`--http 127.0.0.1:0`），再从服务日志里读回实际端口：
        // 三个测试并发跑，硬编码端口一定会撞车，而这次也顺带验证了「0 = 让系统挑」。
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let cache = std::env::temp_dir()
            .join(format!("mb-http-it-{}-{seq}", std::process::id()));
        let mut child = Command::new(env!("CARGO_BIN_EXE_media-bridge"))
            .args(["serve", "--provider", "mock", "--no-audio", "--no-online", "--cache-dir"])
            .arg(&cache)
            .args(["--http", "127.0.0.1:0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("无法启动 media-bridge serve --http");
        let stderr = child.stderr.take().expect("stderr");
        let (tx, lines) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        // 等「HTTP 已就绪：http://127.0.0.1:PORT」这一行
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut port = None;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match lines.recv_timeout(remaining.min(Duration::from_millis(500))) {
                Ok(line) => {
                    if let Some(idx) = line.find("http://127.0.0.1:") {
                        let tail = &line[idx + "http://127.0.0.1:".len()..];
                        let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
                        if let Ok(p) = digits.parse::<u16>() {
                            port = Some(p);
                            break;
                        }
                    }
                }
                Err(_) => continue,
            }
        }
        let port = port.expect("10s 内没等到 HTTP 就绪日志");
        let server = Self { child, port };
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if server.try_get("/health").is_some() {
                return server;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("HTTP 服务已打印就绪但连不上 127.0.0.1:{port}");
    }

    /// 发一条 GET，返回 (状态行, 头, body)。
    ///
    /// **按字节切分**：`/v1/artwork` 回的是 PNG，把它当字符串处理会把二进制内容毁掉
    /// （UTF-8 lossy 之后连 PNG 魔数都对不上）。
    fn try_get(&self, path: &str) -> Option<(String, String, Vec<u8>)> {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nOrigin: http://example.test\r\n\r\n");
        stream.write_all(req.as_bytes()).ok()?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).ok()?;
        let boundary = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
        let head = String::from_utf8_lossy(&raw[..boundary]).to_string();
        let body = raw[boundary + 4..].to_vec();
        let mut lines = head.lines();
        let status = lines.next()?.to_string();
        let headers = lines.collect::<Vec<_>>().join("\n");
        Some((status, headers, body))
    }

    fn get_json(&self, path: &str) -> Value {
        let (status, headers, body) = self.try_get(path).unwrap_or_else(|| panic!("{path} 无响应"));
        assert!(status.contains("200"), "{path} 应 200，实际 {status}");
        assert!(
            headers.to_lowercase().contains("content-type: application/json"),
            "{path} 应回 JSON，实际头：{headers}"
        );
        // 必须显式带 charset：PowerShell 5.1 的 Invoke-RestMethod 在缺 charset 时按 Latin-1
        // 解码 JSON，中文元数据会整片乱码（Windows 宿主实测踩到过）
        assert!(
            headers.to_lowercase().contains("charset=utf-8"),
            "{path} 的 content-type 必须带 charset=utf-8（Windows 客户端依赖它），实际：{headers}"
        );
        assert!(
            headers.to_lowercase().contains("access-control-allow-origin"),
            "{path} 应带 CORS 头（沙箱页面要能 fetch）"
        );
        serde_json::from_slice(&body).unwrap_or_else(|e| panic!("{path} body 不是 JSON：{e}"))
    }

    fn get_raw(&self, path: &str) -> (String, String, Vec<u8>) {
        self.try_get(path).unwrap_or_else(|| panic!("{path} 无响应"))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn http_endpoints_serve_the_same_shapes_as_stdio() {
    let server = Server::start();

    let health = server.get_json("/health");
    assert_eq!(health["ok"], true);

    let hello = server.get_json("/v1/hello");
    assert_eq!(hello["protocol"], 1);

    let now = server.get_json("/v1/now");
    assert_eq!(now["hasMedia"], true);
    assert!(now["track"]["title"].as_str().unwrap().len() > 0);
    assert_eq!(now["track"]["artwork"]["mime"], "image/png");
    // 开了 HTTP 时，快照里会带上封面地址（宿主/页面直接用）。
    // 带 `?v=` 是刻意的：换封面时同一个路径要能被浏览器重新拉取，而不是吃缓存。
    let http_path = now["track"]["artwork"]["httpPath"]
        .as_str()
        .unwrap_or_else(|| panic!("应带封面 HTTP 地址，实际 now = {now}"));
    assert!(
        http_path.starts_with("/v1/artwork"),
        "封面地址应指向 /v1/artwork，实际 {http_path}"
    );

    let caps = server.get_json("/v1/capabilities");
    assert_eq!(caps["play"], true);

    let status = server.get_json("/v1/status");
    assert_eq!(status["provider"], "mock");

    // 封面：二进制出图，类型必须是图片（不是 octet-stream）
    let (st, headers, body) = server.get_raw("/v1/artwork");
    assert!(st.contains("200"), "artwork 应 200：{st}");
    assert!(
        headers.to_lowercase().contains("content-type: image/png"),
        "artwork 应回 image/png，实际：{headers}"
    );
    assert!(body.starts_with(&[0x89, b'P', b'N', b'G']), "body 应是真 PNG 字节");
    assert!(body.len() > 1000, "PNG 应有一定大小，实际 {}", body.len());
}

#[test]
fn http_control_accepts_post_and_reports_outcome() {
    let server = Server::start();
    // 先保证有一帧快照
    let _ = server.get_json("/v1/now");

    let post = |path: &str, body: &str| -> (String, Vec<u8>) {
        let mut stream = TcpStream::connect(("127.0.0.1", server.port)).expect("连接");
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(req.as_bytes()).expect("写请求");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("读响应");
        let text = String::from_utf8_lossy(&raw);
        let (head, rest) = text.split_once("\r\n\r\n").expect("响应格式");
        (head.lines().next().unwrap_or_default().to_string(), rest.as_bytes().to_vec())
    };

    let (status, body) = post("/v1/control", r#"{"action":"pause"}"#);
    assert!(status.contains("200"), "control 应 200：{status}");
    let v: Value = serde_json::from_slice(&body).expect("control 回 JSON");
    assert_eq!(v["outcome"]["applied"], true);
    assert_eq!(v["now"]["playback"]["state"], "paused");

    // 未知动作 → 400（协议错），不是 500
    let (status, _) = post("/v1/control", r#"{"action":"fly"}"#);
    assert!(status.contains("400"), "未知动作应 400，实际 {status}");

    // 非法 JSON → 400
    let (status, _) = post("/v1/control", "not json");
    assert!(status.contains("400"), "非法 body 应 400，实际 {status}");
}

#[test]
fn http_unknown_route_is_404_and_method_errors_are_mapped() {
    let server = Server::start();
    let (status, _, _) = server.get_raw("/v1/nope");
    assert!(status.contains("400"), "未知方法应 400（协议错），实际 {status}");
    let (status, _, _) = server.get_raw("/completely/unknown");
    assert!(status.contains("404"), "未知路径应 404，实际 {status}");
}
