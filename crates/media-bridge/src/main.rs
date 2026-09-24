//! `media-bridge` 命令行。
//!
//! 一个二进制同时是「服务」（`serve`）和「诊断工具」（`now` / `control` / `spectrum` /
//! `diagnose`）—— 因为排查这类中间件时，最需要的就是**不写代码也能看一眼到底拿到了什么**。
//!
//! ```text
//! media-bridge serve                 # stdio 服务（宿主接子进程用这个）
//! media-bridge serve --http 127.0.0.1:8765
//! media-bridge now                   # 人看的当前播放
//! media-bridge now --json            # 机器看的
//! media-bridge control next
//! media-bridge control seek-by +30s
//! media-bridge control loop playlist
//! media-bridge spectrum --bars
//! media-bridge diagnose
//! ```
//!
//! 参数解析是手写的：命令行参数不到二十个，为此引一个解析框架不划算，
//! 而且手写能把 `--help` 写成真正的中文说明。

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use media_bridge::audio::AudioConfig;
use media_bridge::ipc::Request;
use media_bridge::platform::mock::MockConfig;
use media_bridge::platform::Provider;
use media_bridge::service::{BridgeConfig, MediaBridge};
use media_bridge::types::{LoopMode, PlaybackState, TransportCommand};
use media_bridge::{Result, VERSION};

const USAGE: &str = r#"media-bridge —— 跨平台系统媒体中间件（正在播放元数据 / 封面 / 歌词 / 系统音频 / 反向控制）

用法：media-bridge <命令> [选项]

命令
  serve                启动服务。默认 stdio（NDJSON）；加 --http 开本地 HTTP
  now                  打印当前播放（曲目/进度/封面/歌词/能力）
  status               打印各数据源健康状况
  control <动作>        反向控制（第 2 期）
  spectrum             打印一帧 64 段频谱
  lyrics               打印当前歌词
  artwork              导出当前封面到文件
  capture <秒数>        把系统音频采集落成 WAV（16kHz 单声道 s16le），用于核对采集链路
  diagnose             平台能力自检（一条条说明「拿到了什么、差什么」）
  version              版本与编译期能力
  help                 本说明

control 的动作
  play | pause | play-pause | stop | next | previous
  seek <时间>          跳到绝对位置（120000 / 2:00 / 1:02:03）
  seek-by <±时间>      快进/快退（+30s / -15s / 15000）
  loop <off|track|playlist|cycle>    循环模式（cycle = 按 关→列表→单曲 轮转）
  shuffle <on|off|toggle>
  volume <0..1> | mute <on|off> | rate <倍数>

通用选项
  --provider <auto|mock>   会话后端（默认 auto = 按平台自动选；mock = 内置假播放器）
  --mock-config <JSON>     假播放器字段覆盖，如 '{"title":"歌名","playing":false}'
  --cache-dir <目录>        封面/歌词缓存目录（默认平台缓存目录下的 media-bridge）
  --poll-ms <毫秒>          播放中的轮询间隔（默认 1000）
  --idle-poll-ms <毫秒>     无媒体时的轮询间隔（默认 2000）
  --no-audio               不采集系统音频（不申请音频权限）
  --pcm                    额外输出原始 PCM（16kHz 单声道；配合 --fps 使用）
  --fps <帧率>              频谱出帧频率（默认 20）
  --device <名称>           Linux：显式指定采集源（默认取默认输出的 monitor）
  --online / --no-online   在线歌词（LRCLIB）开关，默认开（需编译期 feature）
  --json                   以 JSON 输出（一次性的命令）
  --watch                  持续输出（now / status / spectrum；每 1s 或状态变化时刷新）
  --bars                   频谱用字符条绘制
  --out <文件>             artwork / capture：输出路径（capture 默认 media-bridge-capture.wav）
  --wav <文件>             spectrum：分析一个 16 位 PCM WAV，而不是实时采集
  --verbose                serve：把收到的请求也打到 stderr

HTTP 选项（serve --http）
  --http <地址>            监听地址，如 127.0.0.1:8765（默认 127.0.0.1:8765）
  --cors <'*'|off|来源列表> CORS 策略（默认 '*'：任意来源可读可控制；见文档的安全边界）
"#;

#[derive(Debug, Default)]
struct Args {
    command: String,
    sub: Vec<String>,
    provider: Option<String>,
    mock_config: Option<String>,
    cache_dir: Option<String>,
    poll_ms: Option<u64>,
    idle_poll_ms: Option<u64>,
    no_audio: bool,
    pcm: bool,
    fps: Option<u32>,
    device: Option<String>,
    online: Option<bool>,
    json: bool,
    watch: bool,
    bars: bool,
    out: Option<String>,
    wav: Option<String>,
    verbose: bool,
    http: Option<String>,
    cors: Option<String>,
}

impl Args {
    fn parse(argv: &[String]) -> std::result::Result<Self, String> {
        let mut a = Args::default();
        let mut it = argv.iter().peekable();
        while let Some(arg) = it.next() {
            let take_value = |it: &mut std::iter::Peekable<std::slice::Iter<'_, String>>, name: &str| -> std::result::Result<String, String> {
                match it.next() {
                    Some(v) if !v.starts_with("--") => Ok(v.clone()),
                    _ => Err(format!("{name} 需要一个取值")),
                }
            };
            match arg.as_str() {
                "--provider" => a.provider = Some(take_value(&mut it, "--provider")?),
                "--mock-config" => a.mock_config = Some(take_value(&mut it, "--mock-config")?),
                "--cache-dir" => a.cache_dir = Some(take_value(&mut it, "--cache-dir")?),
                "--poll-ms" => {
                    a.poll_ms = Some(
                        take_value(&mut it, "--poll-ms")?
                            .parse()
                            .map_err(|_| "--poll-ms 需要一个整数".to_string())?,
                    )
                }
                "--idle-poll-ms" => {
                    a.idle_poll_ms = Some(
                        take_value(&mut it, "--idle-poll-ms")?
                            .parse()
                            .map_err(|_| "--idle-poll-ms 需要一个整数".to_string())?,
                    )
                }
                "--no-audio" => a.no_audio = true,
                "--pcm" => a.pcm = true,
                "--fps" => {
                    a.fps = Some(
                        take_value(&mut it, "--fps")?
                            .parse()
                            .map_err(|_| "--fps 需要一个整数".to_string())?,
                    )
                }
                "--device" => a.device = Some(take_value(&mut it, "--device")?),
                "--online" => a.online = Some(true),
                "--no-online" => a.online = Some(false),
                "--json" => a.json = true,
                "--watch" => a.watch = true,
                "--bars" => a.bars = true,
                "--out" => a.out = Some(take_value(&mut it, "--out")?),
                "--wav" => a.wav = Some(take_value(&mut it, "--wav")?),
                "--verbose" | "-v" => a.verbose = true,
                "--http" => {
                    // `--http` 允许不带取值（用默认地址）
                    let v = it.peek().filter(|v| !v.starts_with("--")).map(|v| v.to_string());
                    if v.is_some() {
                        it.next();
                    }
                    a.http = Some(v.unwrap_or_else(|| "127.0.0.1:8765".to_string()));
                }
                "--cors" => a.cors = Some(take_value(&mut it, "--cors")?),
                "--help" | "-h" => a.command = "help".to_string(),
                "--version" | "-V" => a.command = "version".to_string(),
                other if other.starts_with("--") => {
                    return Err(format!("未知选项 {other}（用 media-bridge help 看用法）"));
                }
                other => {
                    if a.command.is_empty() {
                        a.command = other.to_string();
                    } else {
                        a.sub.push(other.to_string());
                    }
                }
            }
        }
        if a.command.is_empty() {
            a.command = "help".to_string();
        }
        Ok(a)
    }

    fn bridge_config(&self) -> Result<BridgeConfig> {
        let provider = match self.provider.as_deref() {
            None | Some("auto") => Provider::Auto,
            Some("mock") => Provider::Mock,
            Some(other) => {
                return Err(media_bridge::BridgeError::Protocol(format!(
                    "未知 provider {other}（可选 auto / mock）"
                )));
            }
        };
        let mock = match &self.mock_config {
            Some(json) => MockConfig::from_json(json)?,
            None => MockConfig::default(),
        };
        let mut cfg = BridgeConfig {
            provider,
            mock,
            cache_dir: self.cache_dir.as_ref().map(std::path::PathBuf::from),
            lyrics_online: self.online.unwrap_or(true),
            // 开了 HTTP 就告诉服务层：封面除了本地路径之外还能用这个地址取
            // （沙箱页面 / 别的进程直接用，不用自己去读别人的缓存目录）
            artwork_http_path: self.http.as_ref().map(|_| "/v1/artwork".to_string()),
            ..Default::default()
        };
        if let Some(ms) = self.poll_ms {
            cfg.poll_interval_ms = ms.clamp(50, 60_000);
        }
        if let Some(ms) = self.idle_poll_ms {
            cfg.idle_poll_interval_ms = ms.clamp(100, 120_000);
        }
        cfg.audio = AudioConfig {
            enabled: !self.no_audio,
            fps: self.fps.unwrap_or(20).clamp(1, 120),
            pcm: self.pcm,
            device: self.device.clone(),
            ..Default::default()
        };
        Ok(cfg)
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match Args::parse(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("参数错误：{e}");
            return ExitCode::from(2);
        }
    };
    if args.command == "help" {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.command == "version" {
        println!("media-bridge {VERSION}");
        println!("平台：{} {}", std::env::consts::OS, std::env::consts::ARCH);
        println!("协议版本：{}", media_bridge::ipc::PROTOCOL_VERSION);
        println!("可用方法：{}", media_bridge::ipc::METHODS.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", "));
        let features: Vec<&str> = [
            ("http", cfg!(feature = "http")),
            ("audio", cfg!(feature = "audio")),
            ("embedded", cfg!(feature = "embedded")),
            ("online", cfg!(feature = "online")),
        ]
        .iter()
        .filter(|(_, on)| *on)
        .map(|(n, _)| *n)
        .collect();
        println!("编译期能力：{}", if features.is_empty() { "无".to_string() } else { features.join(", ") });
        return ExitCode::SUCCESS;
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("无法创建异步运行时：{e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(args)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("错误：{e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<ExitCode> {
    let config = args.bridge_config()?;
    let bridge = MediaBridge::new(config);
    match args.command.as_str() {
        "serve" => cmd_serve(bridge, &args).await,
        "now" => cmd_now(bridge, &args).await,
        "status" => cmd_status(bridge, &args).await,
        "control" => cmd_control(bridge, &args).await,
        "spectrum" => cmd_spectrum(bridge, &args).await,
        "lyrics" => cmd_lyrics(bridge, &args).await,
        "artwork" => cmd_artwork(bridge, &args).await,
        "capture" => cmd_capture(bridge, &args).await,
        "diagnose" => cmd_diagnose(bridge, &args).await,
        other => {
            eprintln!("未知命令 {other:?}\n");
            println!("{USAGE}");
            Ok(ExitCode::from(2))
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// serve
// ══════════════════════════════════════════════════════════════════════════════

async fn cmd_serve(bridge: Arc<MediaBridge>, args: &Args) -> Result<ExitCode> {
    bridge.start();
    // 先把第一份快照拿到手：宿主连上来问 `now` 时就有东西可答
    let _ = bridge.refresh().await;

    if let Some(addr) = &args.http {
        #[cfg(feature = "http")]
        {
            let addr: std::net::SocketAddr = addr
                .parse()
                .map_err(|e| media_bridge::BridgeError::Protocol(format!("--http 地址不合法：{e}")))?;
            let cors = media_bridge::ipc::http::Cors::parse(args.cors.as_deref().unwrap_or("*"));
            if matches!(cors, media_bridge::ipc::http::Cors::Any) {
                eprintln!(
                    "[media-bridge] CORS 允许任意来源：本机任意网页都能读取并控制系统播放。\
                     需要收紧请用 --cors off 或 --cors http://your.origin"
                );
            }
            let io = media_bridge::ipc::http::serve(
                bridge.clone(),
                media_bridge::ipc::http::HttpConfig { addr, cors },
            );
            tokio::select! {
                r = io => r?,
                _ = shutdown_signal() => {}
            }
        }
        #[cfg(not(feature = "http"))]
        {
            let _ = addr;
            eprintln!("这个二进制没有编译 http 能力（重新构建：cargo build --features http）");
            return Ok(ExitCode::FAILURE);
        }
    } else {
        // stdio 模式：stdin 关闭即退出
        tokio::select! {
            r = media_bridge::ipc::stdio::serve(bridge.clone(), args.verbose) => r?,
            _ = shutdown_signal() => {}
        }
    }
    bridge.stop();
    Ok(ExitCode::SUCCESS)
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            let _ = term.recv().await;
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// 只读命令
// ══════════════════════════════════════════════════════════════════════════════

async fn cmd_now(bridge: Arc<MediaBridge>, args: &Args) -> Result<ExitCode> {
    bridge.start();
    bridge.refresh().await?;
    if args.watch {
        let mut last = String::new();
        let mut rx = bridge.subscribe();
        loop {
            let now = bridge.snapshot_at_now();
            let line = render_now(&now);
            if line != last {
                print!("{}", clear_screen());
                println!("{line}");
                last = line;
            }
            tokio::select! {
                ev = rx.recv() => { if ev.is_err() { break; } }
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
        }
        return Ok(ExitCode::SUCCESS);
    }
    let now = bridge.snapshot_at_now();
    if args.json {
        println!("{}", serde_json::to_string_pretty(&now).unwrap_or_default());
    } else {
        println!("{}", render_now(&now));
    }
    bridge.stop();
    Ok(ExitCode::SUCCESS)
}

async fn cmd_status(bridge: Arc<MediaBridge>, args: &Args) -> Result<ExitCode> {
    bridge.start();
    bridge.refresh().await.ok();
    let st = bridge.status();
    if args.json {
        println!("{}", serde_json::to_string_pretty(&st).unwrap_or_default());
    } else {
        println!("media-bridge {}（{} {}）", st.version, st.platform, st.arch);
        println!("会话后端：{}　缓存目录：{}", st.provider, st.cache_dir);
        println!("轮询间隔：{}ms　订阅者：{}　已轮询：{} 次　运行：{}", st.poll_interval_ms, st.subscribers, st.polls, fmt_duration(st.uptime_ms));
        for s in &st.sources {
            println!("  [{:?}] {} —— {}", s.state, s.name, s.hint);
        }
    }
    bridge.stop();
    Ok(ExitCode::SUCCESS)
}

async fn cmd_spectrum(bridge: Arc<MediaBridge>, args: &Args) -> Result<ExitCode> {
    // 离线分析：把一个 WAV 喂给同一个分析器。用于把「采集链路」和「分析数学」分开验证
    // —— 现场排查时能一句话回答「到底是没采到，还是算错了」。
    if let Some(path) = &args.wav {
        return analyze_wav(path, args);
    }
    bridge.ensure_audio()?;
    bridge.start();
    if args.watch {
        loop {
            let frame = bridge.spectrum();
            if args.json {
                println!("{}", serde_json::to_string(&frame).unwrap_or_default());
            } else {
                print!("\r{}", render_bars(&frame.bands, frame.peak));
            }
            use std::io::Write;
            let _ = std::io::stdout().flush();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    // 采集要一点时间才有数据：等半秒，避免打出一屏 0
    tokio::time::sleep(Duration::from_millis(600)).await;
    let frame = bridge.spectrum();
    if args.json {
        println!("{}", serde_json::to_string_pretty(&frame).unwrap_or_default());
    } else if args.bars {
        println!("{}", render_bars(&frame.bands, frame.peak));
    } else {
        println!("采样率 {}Hz　峰值 {}　RMS {:.3}", frame.sample_rate, frame.peak, frame.rms);
        println!("{:?}", frame.bands);
    }
    let st = bridge.audio_status();
    eprintln!("采集状态：[{:?}] {}", st.state, st.hint);
    bridge.stop();
    Ok(ExitCode::SUCCESS)
}

async fn cmd_lyrics(bridge: Arc<MediaBridge>, args: &Args) -> Result<ExitCode> {
    bridge.start();
    bridge.refresh().await?;
    let online = args.online.unwrap_or(false);
    let lyrics = bridge.refresh_lyrics(online).await;
    let Some(lyrics) = lyrics else {
        eprintln!("没有找到歌词（本地 {}，在线 {}）",
            "sidecar/缓存",
            if online { "已查" } else { "未查（加 --online）" });
        return Ok(ExitCode::from(1));
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&lyrics).unwrap_or_default());
    } else {
        println!("来源：{}　时间轴：{}　行数：{}　偏移：{}ms",
            lyrics.source,
            if lyrics.synced { "有" } else { "无" },
            lyrics.lines.len(),
            lyrics.offset_ms);
        let now = bridge.snapshot();
        let pos = now.playback.position_ms as i64;
        let active = lyrics.active_index(pos);
        for (i, l) in lyrics.lines.iter().enumerate() {
            let mark = if Some(i) == active { "▶" } else { " " };
            let t = if lyrics.synced { fmt_duration(l.t_ms.max(0) as u64) } else { "--:--".to_string() };
            println!("{mark} [{t}] {}", l.text);
        }
    }
    bridge.stop();
    Ok(ExitCode::SUCCESS)
}

async fn cmd_artwork(bridge: Arc<MediaBridge>, args: &Args) -> Result<ExitCode> {
    bridge.start();
    bridge.refresh().await?;
    let Some(art) = bridge.snapshot().track.and_then(|t| t.artwork) else {
        eprintln!("当前没有封面（播放器未提供，或还没轮询到）");
        return Ok(ExitCode::from(1));
    };
    match &args.out {
        Some(dest) => {
            let (bytes, _) = bridge
                .artwork_bytes()
                .ok_or_else(|| media_bridge::BridgeError::unavailable("封面文件读不到"))?;
            std::fs::write(dest, &bytes)?;
            println!("已保存 {dest}（{} 字节，{}）", bytes.len(), art.mime);
        }
        None => {
            if args.json {
                println!("{}", serde_json::to_string_pretty(&art).unwrap_or_default());
            } else {
                println!("MIME：{}　大小：{} 字节", art.mime, art.bytes);
                println!("尺寸：{}", match (art.width, art.height) {
                    (Some(w), Some(h)) => format!("{w}x{h}"),
                    _ => "未知".to_string(),
                });
                println!("来源：{:?}　去重键：{}", art.origin, art.key);
                if let Some(p) = &art.path {
                    println!("路径：{}", p.display());
                }
                if let Some(u) = &art.source_url {
                    println!("原始地址：{}", u);
                }
            }
        }
    }
    bridge.stop();
    Ok(ExitCode::SUCCESS)
}

/// 分析一个 WAV（16 位 PCM，单声道或多声道取第一声道）并打印 64 段频谱。
fn analyze_wav(path: &str, args: &Args) -> Result<ExitCode> {
    let bytes = std::fs::read(path)?;
    let (rate, channels, samples) = parse_wav_pcm16(&bytes)
        .ok_or_else(|| media_bridge::BridgeError::Protocol("不是可解析的 16 位 PCM WAV".into()))?;
    let mut analyzer = media_bridge::spectrum::SpectrumAnalyzer::new();
    println!(
        "文件 {path}：{}Hz / {}ch / {} 样本 / {:.2}s",
        rate,
        channels,
        samples.len(),
        samples.len() as f64 / (rate as f64 * channels as f64)
    );
    // 先把文件降到**分析口径**（16 kHz）再分帧：段号必须与实时链路一致，
    // 否则「一段纯音稳定落在同一段」的诊断结论对不上着色器实际看到的频段
    // （旧实现直接把文件样本按 2048 切，48 kHz 文件的段号会整体偏低）。
    let analysis_rate = media_bridge::spectrum::ANALYSIS_RATE;
    let mono: Vec<f32> = if rate == analysis_rate {
        samples.clone()
    } else {
        let mut down = media_bridge::audio::Downsampler::new(rate, analysis_rate);
        let mut out = Vec::with_capacity(samples.len() * analysis_rate as usize / rate as usize + 8);
        down.push(&samples, &mut out);
        out
    };
    // 按窗口滑动，打印若干帧的峰值落在哪一段（对齐「一段纯音应该稳定落在同一段」）
    let n = media_bridge::spectrum::FFT_N;
    let window_ms = n as f64 / analysis_rate as f64 * 1000.0;
    let mut frames = Vec::new();
    let mut offset = 0usize;
    while offset + n <= mono.len().min(n * 8) {
        let mut bands = [0u8; media_bridge::SPECTRUM_BANDS];
        let stats = analyzer.analyze(&mono[offset..offset + n], &mut bands);
        let peak_band = bands.iter().enumerate().max_by_key(|(_, v)| *v).map(|(i, _)| i).unwrap_or(0);
        frames.push((offset, peak_band, stats.rms, bands));
        offset += n;
    }
    if let Some((_, band, rms, _)) = frames.first() {
        println!("窗口 {window_ms:.0}ms　首帧峰值段：{band}　RMS {rms:.4}");
    }
    let at_ms = |off: usize| (off as f64 / analysis_rate as f64 * 1000.0) as u64;
    for (i, (off, band, rms, _)) in frames.iter().enumerate() {
        println!("  帧 {i}（偏移 {}ms）峰值段 {band}　RMS {rms:.4}", at_ms(*off));
    }
    let Some((_, band, _, bands)) = frames.last().cloned() else {
        eprintln!("样本不足一个窗口");
        return Ok(ExitCode::from(1));
    };
    if args.json {
        println!(
            "{}",
            serde_json::json!({"peakBand": band, "bands": bands.to_vec(), "sampleRate": analysis_rate})
        );
    } else {
        println!("{}", render_bars(&bands, bands.iter().copied().max().unwrap_or(0)));
        println!("峰值段：{band}（bands={bands:?}）");
    }
    Ok(ExitCode::SUCCESS)
}

/// 极简 WAV 解析：只认 16 位 PCM（我们自己的 capture 输出就是这个格式）。
fn parse_wav_pcm16(bytes: &[u8]) -> Option<(u32, u16, Vec<f32>)> {
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut pos = 12usize;
    let mut rate = 0u32;
    let mut channels = 1u16;
    let mut bits = 16u16;
    let mut data: Option<&[u8]> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().ok()?) as usize;
        let body_start = pos + 8;
        let body_end = (body_start + len).min(bytes.len());
        match id {
            b"fmt " => {
                let body = &bytes[body_start..body_end];
                if body.len() >= 16 {
                    channels = u16::from_le_bytes(body[2..4].try_into().ok()?);
                    rate = u32::from_le_bytes(body[4..8].try_into().ok()?);
                    bits = u16::from_le_bytes(body[14..16].try_into().ok()?);
                }
            }
            b"data" => data = Some(&bytes[body_start..body_end]),
            _ => {}
        }
        pos = body_start + len + (len & 1); // 块按偶数字节对齐
    }
    if bits != 16 {
        return None;
    }
    let data = data?;
    let ch = channels.max(1) as usize;
    let frames = data.len() / 2 / ch;
    let mut out = Vec::with_capacity(frames);
    for f in 0..frames {
        let mut sum = 0.0f32;
        for c in 0..ch {
            let i = (f * ch + c) * 2;
            sum += i16::from_le_bytes([data[i], data[i + 1]]) as f32 / 32768.0;
        }
        out.push(sum / ch as f32);
    }
    Some((rate, channels, out))
}

/// 采集一段系统音频写到 WAV（核对采集链路：能听到什么、频谱对不对）。
async fn cmd_capture(bridge: Arc<MediaBridge>, args: &Args) -> Result<ExitCode> {
    let seconds: f64 = args
        .sub
        .first()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(3.0)
        .clamp(0.5, 60.0);
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| "media-bridge-capture.wav".to_string());

    bridge.ensure_audio()?;
    let mut pcm = bridge
        .subscribe_pcm()
        .ok_or_else(|| media_bridge::BridgeError::Protocol("PCM 通道未启用：本命令需要 --pcm".into()))?;

    let rate = media_bridge::audio::OUTPUT_RATE;
    let mut samples: Vec<f32> = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs_f64(seconds);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), pcm.recv()).await {
            Ok(Ok(chunk)) => samples.extend_from_slice(&chunk.samples),
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(_)) => break,
            Err(_) => continue,
        }
    }
    let status = bridge.audio_status();
    let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    let rms = if samples.is_empty() {
        0.0
    } else {
        (samples.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>() / samples.len() as f64).sqrt()
    };

    let wav = pcm_to_wav(&samples, rate);
    std::fs::write(&out, &wav)?;
    println!(
        "已写入 {out}：{:.2}s / {} 样本 / {} 字节（{}Hz 单声道 s16le）",
        samples.len() as f64 / rate as f64,
        samples.len(),
        wav.len(),
        rate
    );
    println!("峰值 {peak:.4}　RMS {rms:.4}　采集状态：[{:?}] {}", status.state, status.hint);
    bridge.stop();
    Ok(ExitCode::SUCCESS)
}

/// f32 单声道 → WAV（16 位 PCM）。自己写 44 字节头，不引音频库。
fn pcm_to_wav(samples: &[f32], rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt 块长度
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // 单声道
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes()); // 字节率
    out.extend_from_slice(&2u16.to_le_bytes()); // 块对齐
    out.extend_from_slice(&16u16.to_le_bytes()); // 位深
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

async fn cmd_diagnose(bridge: Arc<MediaBridge>, args: &Args) -> Result<ExitCode> {
    bridge.start();
    bridge.refresh().await.ok();
    let lines = bridge.diagnose();
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "diagnose": lines })).unwrap_or_default()
        );
    } else {
        println!("== 平台自检 ==");
        for l in &lines {
            println!("· {l}");
        }
        println!("\n== 数据源状态 ==");
        for s in bridge.status().sources {
            println!("· [{:?}] {} —— {}", s.state, s.name, s.hint);
        }
    }
    bridge.stop();
    Ok(ExitCode::SUCCESS)
}

// ══════════════════════════════════════════════════════════════════════════════
// control（第 2 期）
// ══════════════════════════════════════════════════════════════════════════════

async fn cmd_control(bridge: Arc<MediaBridge>, args: &Args) -> Result<ExitCode> {
    let cmd = parse_control(&args.sub)?;
    bridge.start();
    bridge.refresh().await?;
    let report = bridge.control(cmd).await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
    } else if report.outcome.applied {
        println!(
            "✓ {} 已生效{}",
            report.outcome.action,
            report
                .outcome
                .effective
                .as_ref()
                .map(|e| format!("（{e}）"))
                .unwrap_or_default()
        );
        println!("{}", render_now(&report.now));
    } else {
        eprintln!(
            "✗ {} 未生效：{}",
            report.outcome.action,
            report.outcome.reason.as_deref().unwrap_or("未知原因")
        );
        return Ok(ExitCode::from(1));
    }
    bridge.stop();
    Ok(ExitCode::SUCCESS)
}

/// 解析 `control` 的子参数。
fn parse_control(sub: &[String]) -> Result<TransportCommand> {
    use media_bridge::BridgeError;
    let Some(action) = sub.first().map(|s| s.as_str()) else {
        return Err(BridgeError::Protocol(
            "control 需要一个动作（play/pause/play-pause/stop/next/previous/seek/seek-by/loop/shuffle/volume/mute/rate）".into(),
        ));
    };
    let arg1 = sub.get(1).map(|s| s.as_str());
    let need = |v: Option<&str>| -> Result<String> {
        v.map(|s| s.to_string())
            .ok_or_else(|| BridgeError::Protocol(format!("{action} 需要一个取值")))
    };
    Ok(match action {
        "play" => TransportCommand::Play,
        "pause" => TransportCommand::Pause,
        "play-pause" | "toggle" => TransportCommand::PlayPause,
        "stop" => TransportCommand::Stop,
        "next" => TransportCommand::Next,
        "previous" | "prev" => TransportCommand::Previous,
        "seek" => TransportCommand::Seek {
            position_ms: parse_time_ms(&need(arg1)?)
                .ok_or_else(|| BridgeError::Protocol("seek 需要时间，如 120000 或 2:00".into()))?
                .max(0) as u64,
        },
        "seek-by" => TransportCommand::SeekBy {
            delta_ms: parse_time_ms(&need(arg1)?)
                .ok_or_else(|| BridgeError::Protocol("seek-by 需要位移，如 +30s / -15s / 15000".into()))?,
        },
        "loop" => {
            let v = arg1.unwrap_or("cycle");
            match v {
                "cycle" | "toggle" => TransportCommand::CycleLoop,
                "off" | "none" => TransportCommand::SetLoop { mode: LoopMode::Off },
                "track" | "one" => TransportCommand::SetLoop { mode: LoopMode::Track },
                "playlist" | "all" => TransportCommand::SetLoop { mode: LoopMode::Playlist },
                other => {
                    return Err(BridgeError::Protocol(format!(
                        "loop 取值应为 off/track/playlist/cycle，收到 {other:?}"
                    )));
                }
            }
        }
        "shuffle" => match arg1.unwrap_or("toggle") {
            "on" | "true" => TransportCommand::SetShuffle { on: true },
            "off" | "false" => TransportCommand::SetShuffle { on: false },
            "toggle" => TransportCommand::ToggleShuffle,
            other => {
                return Err(BridgeError::Protocol(format!(
                    "shuffle 取值应为 on/off/toggle，收到 {other:?}"
                )));
            }
        },
        "volume" => {
            let v: f64 = need(arg1)?
                .trim_end_matches('%')
                .parse()
                .map_err(|_| BridgeError::Protocol("volume 需要 0..1（或 0..100%）".into()))?;
            let v = if v > 1.0 { v / 100.0 } else { v };
            TransportCommand::SetVolume { volume: v.clamp(0.0, 1.0) }
        }
        "mute" => TransportCommand::SetMute {
            muted: match arg1.unwrap_or("on") {
                "on" | "true" | "yes" => true,
                "off" | "false" | "no" => false,
                other => {
                    return Err(BridgeError::Protocol(format!(
                        "mute 取值应为 on/off，收到 {other:?}"
                    )));
                }
            },
        },
        "rate" => TransportCommand::SetRate {
            rate: need(arg1)?
                .parse()
                .map_err(|_| BridgeError::Protocol("rate 需要倍数，如 1.5".into()))?,
        },
        other => {
            return Err(BridgeError::Protocol(format!(
                "未知 control 动作 {other:?}（play/pause/play-pause/stop/next/previous/seek/seek-by/loop/shuffle/volume/mute/rate）"
            )));
        }
    })
}

/// 解析时间：`15000`（毫秒）、`+30s`、`-15s`、`2m`、`2:00`、`1:02:03`。
fn parse_time_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (sign, body) = match s.strip_prefix('+') {
        Some(rest) => (1i64, rest),
        None => match s.strip_prefix('-') {
            Some(rest) => (-1i64, rest),
            None => (1i64, s),
        },
    };
    // 冒号形式
    if body.contains(':') {
        let parts: Vec<&str> = body.split(':').collect();
        let (h, m, sec) = match parts.len() {
            2 => (0i64, parts[0].parse::<i64>().ok()?, parts[1].parse::<f64>().ok()?),
            3 => (
                parts[0].parse::<i64>().ok()?,
                parts[1].parse::<i64>().ok()?,
                parts[2].parse::<f64>().ok()?,
            ),
            _ => return None,
        };
        let ms = ((h * 3600 + m * 60) as f64 * 1000.0 + sec * 1000.0) as i64;
        return Some(sign * ms);
    }
    // 后缀形式
    let lower = body.to_ascii_lowercase();
    let (num, unit_ms) = if let Some(v) = lower.strip_suffix("ms") {
        (v, 1.0)
    } else if let Some(v) = lower.strip_suffix('s') {
        (v, 1000.0)
    } else if let Some(v) = lower.strip_suffix('m') {
        (v, 60_000.0)
    } else if let Some(v) = lower.strip_suffix('h') {
        (v, 3_600_000.0)
    } else {
        (lower.as_str(), 1.0)
    };
    let n: f64 = num.trim().parse().ok()?;
    Some(sign * (n * unit_ms) as i64)
}

// ══════════════════════════════════════════════════════════════════════════════
// 渲染
// ══════════════════════════════════════════════════════════════════════════════

fn render_now(now: &media_bridge::types::NowPlaying) -> String {
    let mut out = String::new();
    if !now.has_media {
        return "（当前没有正在播放的媒体）".to_string();
    }
    let Some(t) = &now.track else {
        return "（有媒体但没有元数据）".to_string();
    };
    let icon = match now.playback.state {
        PlaybackState::Playing => "▶",
        PlaybackState::Paused => "❚❚",
        PlaybackState::Stopped => "■",
        PlaybackState::Unknown => "?",
    };
    let artist = if t.artist.is_empty() { "未知歌手" } else { &t.artist };
    out.push_str(&format!("{icon} {} — {}\n", t.title, artist));
    let mut meta = Vec::new();
    if !t.album.is_empty() {
        meta.push(format!("专辑：{}", t.album));
    }
    meta.push(format!(
        "{} / {}",
        fmt_duration(now.playback.position_ms),
        if now.playback.duration_ms > 0 { fmt_duration(now.playback.duration_ms) } else { "--:--".into() }
    ));
    meta.push(render_progress(now.playback.position_ms, now.playback.duration_ms));
    meta.push(format!("循环：{}", loop_label(now.playback.loop_mode)));
    if let Some(sh) = now.playback.shuffle {
        meta.push(format!("随机：{}", if sh { "开" } else { "关" }));
    }
    if now.playback.rate != 1.0 {
        meta.push(format!("速率：{:.2}x", now.playback.rate));
    }
    out.push_str(&format!("  {}\n", meta.join("　")));
    let app = t.source.app_name.clone().unwrap_or_else(|| t.source.provider.clone());
    out.push_str(&format!("  来源：{app}（{}）\n", t.source.provider));
    match &t.artwork {
        Some(a) => out.push_str(&format!(
            "  封面：{} {} {} {}\n",
            a.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "<未落盘>".into()),
            a.mime,
            match (a.width, a.height) {
                (Some(w), Some(h)) => format!("{w}x{h}"),
                _ => String::new(),
            },
            format_args!("{}字节", a.bytes)
        )),
        None => out.push_str("  封面：（无）\n"),
    }
    match &t.lyrics {
        Some(l) => {
            let active = l.active_index(now.playback.position_ms as i64);
            let current = active
                .and_then(|i| l.lines.get(i))
                .map(|x| x.text.clone())
                .unwrap_or_default();
            out.push_str(&format!(
                "  歌词：{}（{}，{} 行）当前行：{}\n",
                l.source,
                if l.synced { "带时间轴" } else { "无时间轴" },
                l.lines.len(),
                if current.is_empty() { "—" } else { current.as_str() }
            ));
        }
        None => out.push_str("  歌词：（无）\n"),
    }
    let caps = now.capabilities;
    let mut list = Vec::new();
    for (name, on) in [
        ("play", caps.play),
        ("pause", caps.pause),
        ("toggle", caps.toggle),
        ("stop", caps.stop),
        ("next", caps.next),
        ("previous", caps.previous),
        ("seek", caps.seek_absolute),
        ("seek-by", caps.seek_relative),
        ("loop", caps.set_loop),
        ("shuffle", caps.set_shuffle),
        ("volume", caps.set_volume),
        ("mute", caps.set_mute),
        ("rate", caps.set_rate),
    ] {
        if on {
            list.push(name);
        }
    }
    out.push_str(&format!("  能力：{}", if list.is_empty() { "（无）".to_string() } else { list.join(" ") }));
    out
}

fn render_progress(pos_ms: u64, dur_ms: u64) -> String {
    const WIDTH: usize = 20;
    if dur_ms == 0 {
        return "[".to_string() + &"·".repeat(WIDTH) + "]";
    }
    let ratio = (pos_ms as f64 / dur_ms as f64).clamp(0.0, 1.0);
    let filled = (ratio * WIDTH as f64).round() as usize;
    format!(
        "[{}{}] {:>3.0}%",
        "█".repeat(filled),
        "░".repeat(WIDTH - filled.min(WIDTH)),
        ratio * 100.0
    )
}

fn render_bars(bands: &[u8], peak: u8) -> String {
    const LEVELS: [char; 8] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '█'];
    let mut s = String::with_capacity(bands.len() * 4);
    for &b in bands {
        let idx = ((b as usize) * (LEVELS.len() - 1)) / 255;
        s.push(LEVELS[idx.min(LEVELS.len() - 1)]);
    }
    format!("{s}  peak={peak:>3}")
}

fn loop_label(m: LoopMode) -> &'static str {
    match m {
        LoopMode::Off => "关",
        LoopMode::Track => "单曲",
        LoopMode::Playlist => "列表",
        LoopMode::Unknown => "未知",
    }
}

fn fmt_duration(ms: u64) -> String {
    let total = ms / 1000;
    format!("{:02}:{:02}", total / 60, total % 60)
}

fn clear_screen() -> &'static str {
    "\x1b[2J\x1b[H"
}

/// 未使用的请求构造入口（保留给未来的 `rpc` 子命令）。
#[allow(dead_code)]
fn _unused_request() -> Request {
    Request::new("hello", serde_json::json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_time_forms() {
        assert_eq!(parse_time_ms("15000"), Some(15_000));
        assert_eq!(parse_time_ms("+30s"), Some(30_000));
        assert_eq!(parse_time_ms("-15s"), Some(-15_000));
        assert_eq!(parse_time_ms("2m"), Some(120_000));
        assert_eq!(parse_time_ms("2:00"), Some(120_000));
        assert_eq!(parse_time_ms("1:02:03"), Some(3_723_000));
        assert_eq!(parse_time_ms("1.5s"), Some(1_500));
        assert_eq!(parse_time_ms("abc"), None);
        assert_eq!(parse_time_ms(""), None);
    }

    #[test]
    fn parses_control_verbs() {
        let c = |v: &[&str]| parse_control(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(c(&["next"]).unwrap(), TransportCommand::Next);
        assert_eq!(
            c(&["seek", "1:30"]).unwrap(),
            TransportCommand::Seek { position_ms: 90_000 }
        );
        assert_eq!(
            c(&["seek-by", "-20s"]).unwrap(),
            TransportCommand::SeekBy { delta_ms: -20_000 }
        );
        assert_eq!(
            c(&["loop", "track"]).unwrap(),
            TransportCommand::SetLoop { mode: LoopMode::Track }
        );
        assert_eq!(c(&["loop"]).unwrap(), TransportCommand::CycleLoop);
        assert_eq!(
            c(&["shuffle", "on"]).unwrap(),
            TransportCommand::SetShuffle { on: true }
        );
        assert_eq!(
            c(&["volume", "50%"]).unwrap(),
            TransportCommand::SetVolume { volume: 0.5 }
        );
        assert_eq!(c(&["mute", "off"]).unwrap(), TransportCommand::SetMute { muted: false });
        assert_eq!(c(&["rate", "1.25"]).unwrap(), TransportCommand::SetRate { rate: 1.25 });
        assert!(c(&["fly"]).is_err());
        assert!(c(&[]).is_err());
        assert!(c(&["seek"]).is_err(), "seek 缺参数应报错");
    }

    #[test]
    fn parses_cli_args_with_and_without_values() {
        let argv: Vec<String> = ["serve", "--http", "--provider", "mock", "--json"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let a = Args::parse(&argv).unwrap();
        assert_eq!(a.command, "serve");
        assert_eq!(a.http.as_deref(), Some("127.0.0.1:8765"), "--http 不带取值应用默认地址");
        assert_eq!(a.provider.as_deref(), Some("mock"));
        assert!(a.json);

        let argv2: Vec<String> = ["control", "seek", "+10s", "--http", "0.0.0.0:9000"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let a2 = Args::parse(&argv2).unwrap();
        assert_eq!(a2.command, "control");
        assert_eq!(a2.sub, vec!["seek", "+10s"]);
        assert_eq!(a2.http.as_deref(), Some("0.0.0.0:9000"));

        assert!(Args::parse(&["--nope".to_string()]).is_err());
        assert!(Args::parse(&["--poll-ms".to_string()]).is_err(), "缺取值应报错");
    }

    #[test]
    fn progress_and_bars_render_within_bounds() {
        let p = render_progress(30_000, 120_000);
        assert!(p.contains("25%"), "实际：{p}");
        assert!(render_progress(0, 0).contains('·'));
        let b = render_bars(&[0, 128, 255], 255);
        assert!(b.starts_with(' '));
        assert!(b.contains('█'));
    }
}
