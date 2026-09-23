//! macOS 15.4+ 的 MediaRemote 访问通道：**借 `/usr/bin/perl` 的身份**读 now-playing。
//!
//! 背景（实测结论，不是猜测）：macOS 15.4 起，MediaRemote 私有框架只对**被授权**的
//! 进程返回数据。本进程直接 `dlopen`（macos.rs 里的直连路径）拿到的是**空字典** ——
//! 符号全部就绪、`diagnose` 也显示「已加载」，但一个键都读不到，症状就是「明明在放歌，
//! 却 hasMedia=false」。同机上 `media-control` 能读到，差别只在它借了 Apple 自带二进制
//! （`/usr/bin/perl`，守护进程认作 `com.apple.perl`）。
//!
//! 所以这里：把 build.rs 嵌进主二进制的 helper 动态库（`crates/media-bridge-mac-helper`）
//! 落到缓存目录，然后 `/usr/bin/perl -e '<脚本>' <dylib> <入口>` 调它 —— MediaRemote 的
//! 调用就发生在 perl 进程里。实测单次往返约 30ms（含 perl 启动），对 1s 轮询完全够用，
//! 所以不做常驻进程（media-control 的 `stream` 模式是为更密集的推送场景准备的）。
//!
//! 直连路径保留着：macOS 15.4 以下它才是对的那条路，这里失败时会回退过去。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::error::{BridgeError, Result};
use crate::platform::{RawArtwork, RawNowPlaying};
use crate::types::{
    ArtworkOrigin, Capabilities, LoopMode, NowPlaying, Playback, PlaybackState, PositionSource,
    Track, TrackSource,
};

/// 借权用的 Apple 自带二进制（守护进程认它的 bundle id 是 com.apple.*）。
const PERL: &str = "/usr/bin/perl";
/// helper 动态库（由 build.rs 找到并嵌进来；没嵌上时本模块整体不可用）。
#[cfg(mac_helper_embedded)]
static HELPER_DYLIB: &[u8] = include_bytes!(env!("MB_MAC_HELPER_DYLIB"));
#[cfg(not(mac_helper_embedded))]
static HELPER_DYLIB: &[u8] = &[];

/// 调 helper 的 perl 脚本（内联，不额外落盘一个文件）。
///
/// `dl_install_xsub` 生成的是 XSUB 包装：**不要给它传参**（第一个参数位置会是 CV 指针
/// 而不是我们给的值），所以入口按「带/不带封面」拆成两个无参函数。
const PERL_SCRIPT: &str = r#"use DynaLoader;
my $dylib = $ARGV[0];
my $entry = $ARGV[1] eq 'art' ? 'mb_now_playing_json_with_artwork' : 'mb_now_playing_json';
my $h = DynaLoader::dl_load_file($dylib, 0) or do { print STDERR "helper load failed\n"; exit 3 };
my $sym = DynaLoader::dl_find_symbol($h, $entry) or do { print STDERR "helper symbol missing\n"; exit 4 };
DynaLoader::dl_install_xsub('main::mb_read', $sym, $dylib);
main::mb_read();
"#;

/// 单次调用的超时（实测 ~30ms；留足余量，超时即杀进程并回退直连）。
const CALL_TIMEOUT: Duration = Duration::from_secs(3);

/// 本模块是否可用：helper 已嵌入 + `/usr/bin/perl` 在。
pub fn available() -> bool {
    !HELPER_DYLIB.is_empty() && Path::new(PERL).exists()
}

/// helper 得没嵌进二进制（`status`/`diagnose` 要如实说明）。
pub fn embedded() -> bool {
    !HELPER_DYLIB.is_empty()
}

/// 把内嵌的 dylib 落盘（只做一次），返回路径。
fn dylib_path() -> Option<&'static Path> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        if HELPER_DYLIB.is_empty() {
            return None;
        }
        // 缓存目录：与封面缓存同一层（默认 `~/Library/Caches/media-bridge/`；宿主用
        // --cache-dir 改了封面缓存位置时，这里仍用默认目录 —— 只放这一个文件，不影响别处），
        // 不做清理：它必须长期可读（每次读取都要用）。
        let dir = crate::service::BridgeConfig::default_cache_dir().join("mac-helper");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("libmedia_bridge_mac_helper.dylib");
        // 大小一致就认为已有的是同一份（版本更新时大小几乎必然变；变了就重写）
        let need_write = std::fs::metadata(&path)
            .map(|m| m.len() != HELPER_DYLIB.len() as u64)
            .unwrap_or(true);
        if need_write {
            // 原子落盘：多进程（轮询线程 + refresh）可能同时第一次走到这里
            let tmp = dir.join(format!(".helper.{}.tmp", std::process::id()));
            {
                let mut f = std::fs::File::create(&tmp).ok()?;
                f.write_all(HELPER_DYLIB).ok()?;
                f.sync_all().ok()?;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755));
            }
            if std::fs::rename(&tmp, &path).is_err() {
                let _ = std::fs::remove_file(&tmp);
                // 并发时可能是别人先 rename 成功了：文件在就行
                if !path.exists() {
                    return None;
                }
            }
        }
        Some(path)
    })
    .as_deref()
}

/// 跑一次 helper，拿回 JSON（`with_artwork` 决定是否附带封面字节）。
fn read_json(with_artwork: bool) -> Option<serde_json::Value> {
    let dylib = dylib_path()?;
    let mut child = Command::new(PERL)
        .arg("-e")
        .arg(PERL_SCRIPT)
        .arg(dylib)
        .arg(if with_artwork { "art" } else { "plain" })
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + CALL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
    let out = child.wait_with_output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().rev().find(|l| l.trim_start().starts_with('{'))?;
    serde_json::from_str(line).ok()
}

/// 读一次 now-playing，映射成平台层快照。
///
/// `with_artwork` 由上层按「换曲了没」决定：例行轮询不带封面（省掉几百 KB 的搬运），
/// 换曲那一拍才带 —— service 层的「永不降级」会保留同一首的上一张封面。
pub fn snapshot_blocking(with_artwork: bool, now_ms: u64) -> Result<RawNowPlaying> {
    let value = read_json(with_artwork)
        .ok_or_else(|| BridgeError::unavailable("macOS helper 读取失败（perl 或 helper 不可用）"))?;
    if value.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let err = value
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("helper 返回 ok=false");
        return Err(BridgeError::unavailable(err.to_string()));
    }
    if value.get("hasMedia").and_then(|v| v.as_bool()) != Some(true) {
        return Ok(RawNowPlaying::empty(now_ms));
    }

    let s = |k: &str| value.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    // 毫秒：整数或浮点都收 —— 早先 helper 发的是 211167.0，只认 as_u64 会静默变 0
    let u = |k: &str| {
        value
            .get(k)
            .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64)))
            .unwrap_or(0)
    };
    let title = s("title");
    let artist = s("artist");
    let album = s("album");
    let content_id = s("contentId");
    let duration_ms = u("durationMs");
    let position_ms = u("positionMs");
    let track_id = if content_id.is_empty() {
        format!("{artist}|{title}|{album}")
    } else {
        content_id.clone()
    };

    let playing = value.get("playing").and_then(|v| v.as_bool()).unwrap_or(false);
    // 位置来自 CalculatedElapsedTime（系统算好的当前值），所以按「刚轮询到」记时
    let playback = Playback {
        state: if playing {
            PlaybackState::Playing
        } else {
            PlaybackState::Paused
        },
        position_ms,
        position_source: if duration_ms > 0 || position_ms > 0 {
            PositionSource::Polled
        } else {
            PositionSource::Unavailable
        },
        duration_ms,
        rate: value.get("rate").and_then(|v| v.as_f64()).unwrap_or(1.0),
        volume: None,
        muted: None,
        loop_mode: value
            .get("repeatMode")
            .and_then(|v| v.as_i64())
            .map(super::macos::repeat_raw_to_loop)
            .unwrap_or(LoopMode::Unknown),
        shuffle: value.get("shuffleMode").and_then(|v| v.as_i64()).map(|v| v != 0),
        updated_at_ms: now_ms,
    };

    let app_pid = value
        .get("appPid")
        .and_then(|v| v.as_i64())
        .map(|v| v as u32);
    let track = Track {
        id: track_id.clone(),
        title,
        artist,
        album,
        // 框架没导出 albumArtist 键（详见 macos.rs 的说明）
        album_artist: String::new(),
        genre: s("genre"),
        composer: String::new(),
        year: None,
        track_number: value.get("trackNumber").and_then(|v| v.as_i64()).map(|v| v as u32),
        duration_ms,
        artwork: None,
        lyrics: None,
        source: TrackSource {
            // 与直连路径区分开：排查时一眼能看出走的是哪条路
            provider: "macos-mediaremote-helper".to_string(),
            app_name: None,
            app_id: None,
            pid: app_pid,
            file_path: None,
            url: None,
        },
    };

    let mut raw = RawNowPlaying {
        now: NowPlaying {
            has_media: true,
            track: Some(track),
            playback,
            // 能力位：helper 拿不到播放器自报的命令表（见 media-bridge-mac-helper 的说明），
            // 走「常见播放器都能做」的 BASIC 档；回执会如实报告哪条没生效。
            capabilities: Capabilities::BASIC,
            captured_at_ms: now_ms,
        },
        artwork_raw: None,
    };

    if with_artwork
        && let Some(b64) = value.get("artworkBase64").and_then(|v| v.as_str())
        && !b64.is_empty()
    {
        use base64::Engine;
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
            raw.artwork_raw = Some(RawArtwork {
                key: track_id,
                mime_hint: value
                    .get("artworkMime")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                bytes,
                origin: ArtworkOrigin::Player,
                source_url: None,
            });
        }
    }

    Ok(raw)
}
