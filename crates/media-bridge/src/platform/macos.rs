//! macOS 后端：MediaRemote（私有框架，运行时 `dlopen`）。
//!
//! ## 为什么走私有框架
//!
//! 公开 API 里没有「系统正在播放什么」这种全局查询（`MPNowPlayingInfoCenter` 只能读
//! **自己进程**写入的内容）。系统级的 now-playing 只存在于 `MediaRemote.framework`，
//! 控制中心的媒体面板、耳机按键走的都是它。它没有公开头文件、也不保证 ABI 稳定，
//! 但符号名十几年没变过（`media-control`、`nowplaying-cli` 等工具都靠它）。
//!
//! **设计取舍**：所有符号都 `dlopen` + `dlsym`，**拿不到就降级**（`status` 里给出原因），
//! 不做任何「假设存在」的强依赖 —— 这样即使未来 Apple 撤掉某个符号，中间件也只是少一项
//! 能力，而不是起不来。
//!
//! ## 关于 block 回调的参数类型
//!
//! MediaRemote 的查询接口都是「传队列 + 传 block，回调里给结果」。回调参数**有的是指针、
//! 有的是按值传的 `bool`/`int`**（例如 `...ApplicationIsPlaying` 的 `bool`）。block 的
//! ABI 由 block2 依 Rust 闭包的参数类型生成，所以每个接口的参数类型必须写对 ——
//! 把 `bool` 写成 `*const c_void` 会读到一个高位是垃圾的寄存器值，且**不报错**。
//!
//! ## 第 2 期（反向控制）的语义依据
//!
//! `repeat` / `shuffle` 的原始数值语义来自 Apple 公开头文件里的枚举顺序
//! （`MPRemoteControlTypes.h`）：
//!
//! ```text
//! MPRepeatType:  Off = 0, One = 1, All = 2
//! MPShuffleType: Off = 0, Items = 1, Collections = 2
//! ```
//!
//! 这是播放器写进 now-playing 字典时遵循的约定（控制中心的循环按钮就是按它渲染的），
//! 所以这里用它把原始值翻译成 [`LoopMode`]。写回时先直接 `Set`、再**回读校验**；
//! 播放器不认就直接写的话，退回用 `AdvanceRepeatMode`（循环按钮本身走的那条命令）
//! 轮转逼近，仍不成功就如实报 `applied: false`。

use std::ffi::{CString, c_char, c_int, c_void};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use crate::error::{BridgeError, Result};
use crate::types::{
    ArtworkOrigin, Capabilities, ControlOutcome, LoopMode, Playback, PlaybackState, PositionSource,
    SourceState, SourceStatus, Track, TrackSource, TransportCommand,
};
use crate::util::now_ms;

use super::{RawArtwork, RawNowPlaying};

/// MediaRemote 导出的符号（地址一律存 `usize`，这样 `Api` 自动 `Send + Sync`，
/// 不需要对装裸指针的结构体写 `unsafe impl`）。
#[derive(Debug, Clone, Copy, Default)]
struct Api {
    get_now_playing_info: usize,
    get_is_playing: usize,
    get_display_name: usize,
    get_pid: usize,
    get_supported_commands: usize,
    send_command: usize,
    set_elapsed_time: usize,
    set_repeat_mode: usize,
    set_shuffle_mode: usize,
    /// 注册成 now-playing 客户端。macOS 26 上**不注册就查不到数据**（见 ensure_registered）
    register: usize,
}

impl Api {
    fn all_present(&self) -> bool {
        self.get_now_playing_info != 0
    }
}

/// now-playing 字典里的键（`dlsym` 拿到的是指向 `CFStringRef` 全局量的指针）。
#[derive(Debug, Clone, Copy, Default)]
struct Keys {
    title: usize,
    artist: usize,
    album: usize,
    genre: usize,
    duration: usize,
    elapsed: usize,
    calculated_elapsed: usize,
    rate: usize,
    artwork_data: usize,
    artwork_mime: usize,
    artwork_url: usize,
    content_id: usize,
    track_number: usize,
    disc_number: usize,
    repeat: usize,
    shuffle: usize,
    timestamp: usize,
}

/// 把裸指针包成可跨线程传递的值。
///
/// 安全性依据：这里面装的都是**进程生命周期内长期有效**的地址 —— MediaRemote 导出的
/// `CFStringRef` 全局量、我们自己创建且从不释放的 dispatch queue。只在 block 回调里只读使用。
#[derive(Debug, Clone, Copy)]
struct SendPtr(usize);

impl SendPtr {
    fn new<T>(p: *const T) -> Self {
        Self(p as usize)
    }
    fn get<T>(&self) -> *const T {
        self.0 as *const T
    }
}

// SAFETY: 见 `SendPtr` 的文档 —— 指向的都是进程级长期有效的只读对象。
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

// ══════════════════════════════════════════════════════════════════════════════
// 底层 FFI
// ══════════════════════════════════════════════════════════════════════════════

#[link(name = "System", kind = "dylib")]
unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, sym: *const c_char) -> *mut c_void;
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
    fn CFArrayGetCount(arr: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(arr: *const c_void, idx: isize) -> *const c_void;
    fn CFGetTypeID(cf: *const c_void) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFDataGetTypeID() -> usize;
    fn CFDateGetTypeID() -> usize;
    fn CFStringGetCString(s: *const c_void, buf: *mut c_char, size: isize, encoding: u32) -> bool;
    fn CFStringGetLength(s: *const c_void) -> isize;
    fn CFDataGetLength(d: *const c_void) -> isize;
    fn CFDataGetBytePtr(d: *const c_void) -> *const u8;
    fn CFNumberGetValue(n: *const c_void, the_type: i64, out: *mut c_void) -> bool;
    fn CFDateGetAbsoluteTime(d: *const c_void) -> f64;
}

/// CoreFoundation 的 UTF-8 编码常量。
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
/// `kCFNumberSInt64Type`
const CF_NUMBER_SINT64: i64 = 4;
/// `kCFNumberFloat64Type`
const CF_NUMBER_FLOAT64: i64 = 13;
/// 2001-01-01 与 1970-01-01 之间的秒数（`CFAbsoluteTime` → Unix 时间）
const APPLE_EPOCH_OFFSET_S: f64 = 978_307_200.0;

const RTLD_NOW: c_int = 0x2;

/// 每个接口的回执超时。MediaRemote 是本地 XPC 往返，正常在毫秒级；
/// 超时说明对面没响应（没有 now-playing 客户端等），直接放弃这一轮。
const CALL_TIMEOUT: Duration = Duration::from_millis(1500);

/// CFString → String。
fn cf_string(value: *const c_void) -> Option<String> {
    if value.is_null() {
        return None;
    }
    unsafe {
        if CFGetTypeID(value) != CFStringGetTypeID() {
            return None;
        }
        // UTF-8 最多 4 字节/UTF-16 单元，留出富余
        let len = CFStringGetLength(value).max(0);
        let cap = (len as usize).saturating_mul(4).clamp(64, 65_536) + 1;
        let mut buf = vec![0i8; cap];
        if CFStringGetCString(value, buf.as_mut_ptr(), cap as isize, CF_STRING_ENCODING_UTF8) {
            Some(
                std::ffi::CStr::from_ptr(buf.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
            )
        } else {
            None
        }
    }
}

/// CFNumber → f64。
fn cf_f64(value: *const c_void) -> Option<f64> {
    if value.is_null() {
        return None;
    }
    unsafe {
        if CFGetTypeID(value) != CFNumberGetTypeID() {
            return None;
        }
        let mut out = 0f64;
        if CFNumberGetValue(value, CF_NUMBER_FLOAT64, &mut out as *mut f64 as *mut c_void) {
            return Some(out);
        }
        let mut i = 0i64;
        if CFNumberGetValue(value, CF_NUMBER_SINT64, &mut i as *mut i64 as *mut c_void) {
            return Some(i as f64);
        }
        None
    }
}

/// CFNumber → i64。
fn cf_i64(value: *const c_void) -> Option<i64> {
    if value.is_null() {
        return None;
    }
    unsafe {
        if CFGetTypeID(value) != CFNumberGetTypeID() {
            return None;
        }
        let mut out = 0i64;
        if CFNumberGetValue(value, CF_NUMBER_SINT64, &mut out as *mut i64 as *mut c_void) {
            return Some(out);
        }
        cf_f64(value).map(|f| f as i64)
    }
}

/// CFData → Vec<u8>。
fn cf_bytes(value: *const c_void) -> Option<Vec<u8>> {
    if value.is_null() {
        return None;
    }
    unsafe {
        if CFGetTypeID(value) != CFDataGetTypeID() {
            return None;
        }
        let len = CFDataGetLength(value);
        if len <= 0 {
            return None;
        }
        let ptr = CFDataGetBytePtr(value);
        if ptr.is_null() {
            return None;
        }
        Some(std::slice::from_raw_parts(ptr, len as usize).to_vec())
    }
}

/// CFDate → Unix 毫秒。
fn cf_date_ms(value: *const c_void) -> Option<u64> {
    if value.is_null() {
        return None;
    }
    unsafe {
        if CFGetTypeID(value) != CFDateGetTypeID() {
            return None;
        }
        let abs = CFDateGetAbsoluteTime(value);
        if abs <= 0.0 {
            return None;
        }
        Some(((abs + APPLE_EPOCH_OFFSET_S) * 1000.0) as u64)
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// 会话
// ══════════════════════════════════════════════════════════════════════════════

/// macOS 会话。克隆成本 = 一次 `Arc` 自增（服务层会把它丢进阻塞线程池）。
#[derive(Clone)]
pub struct MacSession {
    inner: Arc<Inner>,
}

struct Inner {
    api: OnceLock<Option<Api>>,
    keys: OnceLock<Keys>,
    queue: OnceLock<SendPtr>,
    /// 是否已注册成 now-playing 客户端（只注册一次；见 `ensure_registered`）
    registered: OnceLock<()>,
    /// 上一轮看到的曲目标识（helper 路径用它判断「换曲了没」）
    last_track_id: Mutex<String>,
    /// 已经取过封面的曲目标识（换曲才重新取一次封面）
    artwork_for: RwLock<String>,
    /// 串行化 block 调用：一次只等一个回执（同时复用同一条队列）
    ctrl: Mutex<()>,
    /// 应用名/PID 缓存：只在换应用时刷新（每次轮询多问两次 MediaRemote 没必要）
    app: RwLock<AppCache>,
    /// 已成功发送过的命令数（诊断用）
    commands_sent: AtomicU64,
}

#[derive(Debug, Clone, Default)]
struct AppCache {
    id: String,
    name: Option<String>,
    pid: Option<u32>,
}

impl MacSession {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                api: OnceLock::new(),
                keys: OnceLock::new(),
                queue: OnceLock::new(),
                registered: OnceLock::new(),
                last_track_id: Mutex::new(String::new()),
                artwork_for: RwLock::new(String::new()),
                ctrl: Mutex::new(()),
                app: RwLock::new(AppCache::default()),
                commands_sent: AtomicU64::new(0),
            }),
        }
    }

    /// 后端状态（供 `status` 上报）。
    pub fn status(&self) -> SourceStatus {
        match self.inner.api() {
            Some(api) if api.all_present() => SourceStatus::new(
                "metadata",
                SourceState::Running,
                if super::macos_helper::available() {
                    // macOS 15.4 起只有被授权进程读得到，这里是借 /usr/bin/perl 的身份
                    "MediaRemote（系统级正在播放，经 /usr/bin/perl 权限通道）"
                } else {
                    "MediaRemote（系统级正在播放，进程内直连）"
                },
            ),
            Some(_) => SourceStatus::new(
                "metadata",
                SourceState::Unavailable,
                "MediaRemote 已加载但缺少关键符号，元数据不可用",
            ),
            None => SourceStatus::new(
                "metadata",
                SourceState::Unavailable,
                "无法加载 MediaRemote 私有框架（需要 macOS 12+ 且非沙箱进程）",
            ),
        }
    }

    /// 自检报告（`media-bridge diagnose`）。
    pub fn diagnose(&self) -> Vec<String> {
        let mut out = Vec::new();
        let api = self.inner.api();
        out.push(format!(
            "MediaRemote：{}",
            match api {
                Some(a) => format!("已加载（符号 {}）", symbol_report(&a)),
                None => "不可用（dlopen 失败）".to_string(),
            }
        ));
        if api.is_none() {
            return out;
        }
        out.push(if super::macos_helper::available() {
            "权限通道：经 /usr/bin/perl 读 MediaRemote（macOS 15.4+ 必需）".to_string()
        } else if super::macos_helper::embedded() {
            "权限通道：未启用（本机没有 /usr/bin/perl）—— macOS 15.4+ 会读不到正在播放"
                .to_string()
        } else {
            "权限通道：未编入本产物（构建时未嵌入 helper）—— macOS 15.4+ 会读不到正在播放"
                .to_string()
        });
        let keys = self.inner.keys();
        let found = [
            keys.title,
            keys.artist,
            keys.album,
            keys.duration,
            keys.elapsed,
            keys.artwork_data,
            keys.repeat,
            keys.shuffle,
        ]
        .iter()
        .filter(|k| **k != 0)
        .count();
        out.push(format!("NowPlayingInfo 键：{found}/8 可用"));
        match self.snapshot_blocking() {
            Ok(snap) if snap.has_media() => {
                if let Some(t) = &snap.now.track {
                    out.push(format!(
                        "当前曲目：{} — {}（{}）",
                        t.artist, t.title, t.album
                    ));
                }
                out.push(format!(
                    "播放态：{:?}，位置 {:.1}s / {:.1}s，循环 {:?}，随机 {:?}",
                    snap.now.playback.state,
                    snap.now.playback.position_ms as f64 / 1000.0,
                    snap.now.playback.duration_ms as f64 / 1000.0,
                    snap.now.playback.loop_mode,
                    snap.now.playback.shuffle
                ));
                out.push(format!(
                    "封面：{}",
                    match &snap.artwork_raw {
                        Some(a) => format!("{} 字节（自称 {}）", a.bytes.len(), a.mime_hint.as_deref().unwrap_or("未标注")),
                        None => "播放器未提供".to_string(),
                    }
                ));
                let c = snap.now.capabilities;
                out.push(format!(
                    "能力：play={} pause={} next={} prev={} seek={} loop={} shuffle={}",
                    c.play, c.pause, c.next, c.previous, c.seek_absolute, c.set_loop, c.set_shuffle
                ));
            }
            Ok(_) => out.push("当前没有正在播放的媒体（属正常，播放任意音频后重试）".to_string()),
            Err(e) => out.push(format!("查询失败：{e}")),
        }
        out
    }

    /// 同步取一次快照（内部会等 MediaRemote 的 block 回执）。
    ///
    /// 首选**借 `/usr/bin/perl` 身份的 helper**（macOS 15.4 起只有被授权进程读得到，
    /// 见 `platform::macos_helper`）；helper 不可用/读失败时退回本进程直连 ——
    /// 直连在 15.4 以下是正确路径，在 15.4 以上只会拿到空字典（那时 status 会说明原因）。
    pub fn snapshot_blocking(&self) -> Result<RawNowPlaying> {
        let now = now_ms();
        if super::macos_helper::available() {
            // 封面只在「换曲那一拍」取：封面动辄几百 KB，例行轮询不该每秒搬一次；
            // service 层的「永不降级」会保留同一首的上一张封面。
            let last = self.inner.last_track_id.lock().map(|t| t.clone()).unwrap_or_default();
            let fetched = self.inner.artwork_for.read().unwrap_or_else(|e| e.into_inner()).clone();
            let with_artwork = last.is_empty() || fetched != last;
            match super::macos_helper::snapshot_blocking(with_artwork, now) {
                Ok(raw) => {
                    let id = raw.track_id().to_string();
                    if !id.is_empty() {
                        if with_artwork && raw.artwork_raw.is_some() {
                            if let Ok(mut w) = self.inner.artwork_for.write() {
                                *w = id.clone();
                            }
                        }
                        if let Ok(mut t) = self.inner.last_track_id.lock() {
                            *t = id;
                        }
                    }
                    return Ok(raw);
                }
                Err(_) => {
                    // helper 这一轮失败（被系统临时拒了 / perl 不在 / 落盘失败…）：
                    // 不报错，交给下面的直连路径兜底 —— 它在 15.4 以下是对的，
                    // 在 15.4 以上会返回空快照（与「没有媒体」同形，status 里另有说明）。
                }
            }
        }
        let api = self
            .inner
            .api()
            .ok_or_else(|| BridgeError::unavailable("MediaRemote 不可用（需要 macOS 12+ 且非沙箱进程）"))?;
        if !api.all_present() {
            return Err(BridgeError::unavailable("MediaRemote 缺少 MRMediaRemoteGetNowPlayingInfo"));
        }
        // 必须先注册成客户端（对老系统上是充分条件；macOS 15.4+ 靠下面的 helper 路径）
        self.inner.ensure_registered();
        let now = now_ms();
        let keys = self.inner.keys();

        // 1) now-playing 字典（解析在 block 回调里做：CFDictionary 只在回调期间有效）
        let parsed = self
            .inner
            .call_dict(api.get_now_playing_info, move |dict| parse_info_dict(dict, &keys))
            .unwrap_or_default();

        if !parsed.has_any() {
            return Ok(RawNowPlaying::empty(now));
        }

        // 2) 播放态（单独的接口，字典里没有权威的 pause/play）
        let is_playing = self
            .inner
            .call_bool(api.get_is_playing, |v| v)
            .unwrap_or(false);

        // 3) 能力（播放器自报支持的媒体命令）
        let commands = if api.get_supported_commands != 0 {
            self.inner
                .call_array(api.get_supported_commands, |arr| array_of_i64(arr))
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        // 4) 应用名 / PID（换应用才刷新）
        self.inner.refresh_app(&parsed.content_id);

        let app = self.inner.app.read().map(|g| g.clone()).unwrap_or_default();
        let duration_ms = parsed.duration_s.map(|s| (s.max(0.0) * 1000.0) as u64).unwrap_or(0);
        // 位置优先级：CalculatedElapsedTime（系统算好的当前值）> ElapsedTime（配 Timestamp 外推）
        let (position_ms, position_source, updated_at_ms) = match parsed.calculated_elapsed_s {
            Some(s) => ((s.max(0.0) * 1000.0) as u64, PositionSource::Polled, now),
            None => match parsed.elapsed_s {
                Some(s) => (
                    (s.max(0.0) * 1000.0) as u64,
                    PositionSource::Polled,
                    parsed.timestamp_ms.unwrap_or(now),
                ),
                None => (0, PositionSource::Unavailable, now),
            },
        };

        let state = if is_playing {
            PlaybackState::Playing
        } else {
            PlaybackState::Paused
        };

        let playback = Playback {
            state,
            position_ms,
            position_source,
            duration_ms,
            rate: parsed.rate.unwrap_or(1.0),
            volume: None,
            muted: None,
            loop_mode: parsed.repeat_raw.map(repeat_raw_to_loop).unwrap_or(LoopMode::Unknown),
            shuffle: parsed.shuffle_raw.map(|v| v != 0),
            updated_at_ms,
        };

        let mut track = Track {
            id: if parsed.content_id.is_empty() {
                format!("{}|{}|{}", parsed.artist, parsed.title, parsed.album)
            } else {
                parsed.content_id.clone()
            },
            title: parsed.title.clone(),
            artist: parsed.artist.clone(),
            album: parsed.album.clone(),
            // 见 Keys 处的说明：框架没导出 albumArtist 键
            album_artist: String::new(),
            genre: parsed.genre.clone(),
            composer: String::new(),
            year: None,
            track_number: parsed.track_number.map(|n| n as u32),
            duration_ms,
            artwork: None,
            lyrics: None,
            source: TrackSource {
                provider: "macos-mediaremote".to_string(),
                app_name: app.name.clone(),
                app_id: app.id.clone().into(),
                pid: app.pid,
                // MediaRemote 不提供本地文件路径 —— 内嵌歌词/内嵌封面这条路在 macOS 上走不通
                file_path: None,
                url: parsed.artwork_url.clone(),
            },
        };
        if !track.has_content() {
            track.source.app_name = app.name.clone();
        }

        let capabilities = caps_from_commands(&commands, true);
        let mut raw = RawNowPlaying::from_parts(Some(track), playback, capabilities, now);

        // 5) 封面
        if let Some(data) = parsed.artwork_data {
            raw.artwork_raw = Some(RawArtwork {
                key: raw.now.track.as_ref().map(|t| t.id.clone()).unwrap_or_default(),
                mime_hint: parsed.artwork_mime,
                bytes: data,
                origin: ArtworkOrigin::Player,
                source_url: parsed.artwork_url,
            });
        } else if let Some(url) = parsed.artwork_url {
            // 少数播放器只给远端地址：交给服务层去下载（这里不做网络 IO）
            raw.now.track.as_mut().map(|t| {
                t.source.url = Some(url.clone());
            });
        }
        Ok(raw)
    }

    /// 同步发一条传输控制命令（第 2 期）。
    pub fn control_blocking(&self, cmd: &TransportCommand) -> Result<ControlOutcome> {
        let api = self
            .inner
            .api()
            .ok_or_else(|| BridgeError::unavailable("MediaRemote 不可用"))?;
        let snap = self.snapshot_blocking()?;
        if !snap.has_media() {
            return Err(BridgeError::NoMedia);
        }
        if let Some(gate) = cmd.required_capability()
            && !gate(&snap.now.capabilities)
        {
            return Ok(ControlOutcome::rejected(
                cmd.name(),
                "当前播放器未声明支持该操作（capabilities 里对应位为 false）",
            ));
        }

        let send = |code: i32| -> bool {
            if api.send_command == 0 {
                return false;
            }
            unsafe {
                let f: unsafe extern "C" fn(i32, *const c_void) -> i32 =
                    std::mem::transmute(api.send_command);
                f(code, std::ptr::null());
            }
            self.inner.commands_sent.fetch_add(1, Ordering::Relaxed);
            true
        };

        match cmd {
            TransportCommand::Play => Ok(control_result(cmd.name(), send(0))),
            TransportCommand::Pause => Ok(control_result(cmd.name(), send(1))),
            TransportCommand::PlayPause => Ok(control_result(cmd.name(), send(2))),
            TransportCommand::Stop => Ok(control_result(cmd.name(), send(3))),
            TransportCommand::Next => Ok(control_result(cmd.name(), send(4))),
            TransportCommand::Previous => Ok(control_result(cmd.name(), send(5))),

            TransportCommand::Seek { position_ms } => {
                let target = *position_ms as f64 / 1000.0;
                self.set_elapsed(target)?;
                // 回读校验：seek 是「发出去了但播放器可能不理」的典型，必须验
                std::thread::sleep(Duration::from_millis(120));
                let after = self.read_position_ms().unwrap_or(0) as f64;
                let delta = (after - *position_ms as f64).abs();
                Ok(if delta <= 2000.0 {
                    ControlOutcome::ok_effective(cmd.name(), format!("{:.1}s", after / 1000.0))
                } else {
                    ControlOutcome::rejected(
                        cmd.name(),
                        format!("播放器未响应 seek（目标 {:.1}s，回读 {:.1}s）", target, after / 1000.0),
                    )
                })
            }

            TransportCommand::SeekBy { delta_ms } => {
                let cur = snap.now.playback.position_ms as i64;
                let dur = snap.now.playback.duration_ms as i64;
                let mut target = (cur + *delta_ms).max(0);
                if dur > 0 {
                    target = target.min(dur);
                }
                self.set_elapsed(target as f64 / 1000.0)?;
                std::thread::sleep(Duration::from_millis(120));
                let after = self.read_position_ms().unwrap_or(0) as f64;
                Ok(if (after - target as f64).abs() <= 2000.0 {
                    ControlOutcome::ok_effective(
                        cmd.name(),
                        format!("{:.1}s → {:.1}s", cur as f64 / 1000.0, after / 1000.0),
                    )
                } else {
                    ControlOutcome::rejected(
                        cmd.name(),
                        format!("播放器未响应快进/快退（目标 {:.1}s，回读 {:.1}s）", target as f64 / 1000.0, after / 1000.0),
                    )
                })
            }

            TransportCommand::CycleLoop => {
                if !send(7) {
                    return Ok(ControlOutcome::rejected(cmd.name(), "播放器不支持切换循环模式"));
                }
                std::thread::sleep(Duration::from_millis(90));
                let after = self.read_repeat_raw().map(repeat_raw_to_loop);
                Ok(match after {
                    Some(m) => ControlOutcome::ok_effective(cmd.name(), format!("loop={m:?}")),
                    None => ControlOutcome::ok(cmd.name()),
                })
            }

            TransportCommand::ToggleShuffle => {
                if !send(6) {
                    return Ok(ControlOutcome::rejected(cmd.name(), "播放器不支持切换随机播放"));
                }
                std::thread::sleep(Duration::from_millis(90));
                let after = self.read_shuffle_raw();
                Ok(match after {
                    Some(v) => ControlOutcome::ok_effective(cmd.name(), format!("shuffle={}", v != 0)),
                    None => ControlOutcome::ok(cmd.name()),
                })
            }

            TransportCommand::SetLoop { mode } => self.set_loop(*mode),
            TransportCommand::SetShuffle { on } => self.set_shuffle(*on),

            TransportCommand::SetVolume { .. } => Ok(ControlOutcome::rejected(
                cmd.name(),
                "macOS 侧未暴露音量写入（MediaRemote 的音量接口是 per-origin 语义，未验证，故意不提供）",
            )),
            TransportCommand::SetMute { .. } => Ok(ControlOutcome::rejected(
                cmd.name(),
                "macOS 侧未暴露静音写入（同上：没有可回读校验的稳定接口）",
            )),
            TransportCommand::SetRate { .. } => Ok(ControlOutcome::rejected(
                cmd.name(),
                "macOS 侧的倍速写入语义未验证，故意不提供；请用播放器自身的倍速设置",
            )),
        }
    }

    fn set_elapsed(&self, seconds: f64) -> Result<()> {
        let api = self.inner.api().ok_or_else(|| BridgeError::unavailable("MediaRemote 不可用"))?;
        if api.set_elapsed_time == 0 {
            return Err(BridgeError::NotSupported("MediaRemote 没有 MRMediaRemoteSetElapsedTime".into()));
        }
        unsafe {
            let f: unsafe extern "C" fn(f64) = std::mem::transmute(api.set_elapsed_time);
            f(seconds.max(0.0));
        }
        self.inner.commands_sent.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// 设定循环模式：先直接写、回读校验，不成立就用「切换循环模式」轮转逼近。
    fn set_loop(&self, target: LoopMode) -> Result<ControlOutcome> {
        let api = self.inner.api().ok_or_else(|| BridgeError::unavailable("MediaRemote 不可用"))?;
        let want = match target {
            LoopMode::Off => 0i64,
            LoopMode::Track => 1,    // MPRepeatTypeOne
            LoopMode::Playlist => 2, // MPRepeatTypeAll
            LoopMode::Unknown => {
                return Ok(ControlOutcome::rejected("set-loop", "不支持把循环模式设为 unknown"))
            }
        };
        if api.set_repeat_mode != 0 {
            unsafe {
                let f: unsafe extern "C" fn(i64) = std::mem::transmute(api.set_repeat_mode);
                f(want);
            }
            self.inner.commands_sent.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(90));
            if self.read_repeat_raw() == Some(want) {
                return Ok(ControlOutcome::ok_effective("set-loop", format!("loop={target:?}")));
            }
        }
        // 兜底：用「切换循环模式」命令轮转（最多 3 步，回到原点即放弃）
        if api.send_command != 0 {
            for _ in 0..3 {
                unsafe {
                    let f: unsafe extern "C" fn(i32, *const c_void) -> i32 =
                        std::mem::transmute(api.send_command);
                    f(7, std::ptr::null());
                }
                self.inner.commands_sent.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(90));
                if self.read_repeat_raw() == Some(want) {
                    return Ok(ControlOutcome::ok_effective("set-loop", format!("loop={target:?}")));
                }
            }
        }
        Ok(ControlOutcome::rejected(
            "set-loop",
            "播放器未接受循环模式变更（该播放器可能不支持设定循环，只支持切换）",
        ))
    }

    /// 设定随机播放：同上，先写后验、退回轮转。
    fn set_shuffle(&self, on: bool) -> Result<ControlOutcome> {
        let api = self.inner.api().ok_or_else(|| BridgeError::unavailable("MediaRemote 不可用"))?;
        // MPShuffleTypeOff = 0，MPShuffleTypeItems = 1
        let want = if on { 1i64 } else { 0 };
        if api.set_shuffle_mode != 0 {
            unsafe {
                let f: unsafe extern "C" fn(i64) = std::mem::transmute(api.set_shuffle_mode);
                f(want);
            }
            self.inner.commands_sent.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(90));
            if self.read_shuffle_raw().map(|v| v != 0) == Some(on) {
                return Ok(ControlOutcome::ok_effective("set-shuffle", format!("shuffle={on}")));
            }
        }
        if api.send_command != 0 {
            for _ in 0..3 {
                unsafe {
                    let f: unsafe extern "C" fn(i32, *const c_void) -> i32 =
                        std::mem::transmute(api.send_command);
                    f(6, std::ptr::null());
                }
                self.inner.commands_sent.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(90));
                if self.read_shuffle_raw().map(|v| v != 0) == Some(on) {
                    return Ok(ControlOutcome::ok_effective("set-shuffle", format!("shuffle={on}")));
                }
            }
        }
        Ok(ControlOutcome::rejected(
            "set-shuffle",
            "播放器未接受随机播放变更（该播放器可能不支持设定，只支持切换）",
        ))
    }

    fn read_position_ms(&self) -> Option<u64> {
        let raw = self.read_dict(|p| p.calculated_elapsed_s.or(p.elapsed_s))?;
        Some((raw.max(0.0) * 1000.0) as u64)
    }

    fn read_repeat_raw(&self) -> Option<i64> {
        self.read_dict(|p| p.repeat_raw)
    }

    fn read_shuffle_raw(&self) -> Option<i64> {
        self.read_dict(|p| p.shuffle_raw)
    }

    /// 重新拉一次 now-playing 字典，取其中一个字段。
    fn read_dict<T: Send + 'static>(
        &self,
        pick: impl Fn(&ParsedDict) -> Option<T> + Send + 'static,
    ) -> Option<T> {
        let api = self.inner.api()?;
        // 与 snapshot_blocking 同一条前提：没注册成客户端就查不到（见 ensure_registered）
        self.inner.ensure_registered();
        let keys = self.inner.keys();
        self.inner
            .call_dict(api.get_now_playing_info, move |dict| {
                let parsed = parse_info_dict(dict, &keys);
                pick(&parsed)
            })
            .flatten()
    }
}

impl Default for MacSession {
    fn default() -> Self {
        Self::new()
    }
}

/// 把「命令是否发出去」翻译成回执。
fn control_result(action: &str, sent: bool) -> ControlOutcome {
    if sent {
        ControlOutcome::ok(action)
    } else {
        ControlOutcome::rejected(action, "MediaRemote 没有 MRMediaRemoteSendCommand（无法发送传输控制）")
    }
}

/// `MPRepeatType` 原始值 → 语义（依据 `MPRemoteControlTypes.h` 的枚举顺序）。
pub(crate) fn repeat_raw_to_loop(raw: i64) -> LoopMode {
    match raw {
        0 => LoopMode::Off,
        1 => LoopMode::Track,
        2 => LoopMode::Playlist,
        _ => LoopMode::Unknown,
    }
}

/// 由播放器自报支持的媒体命令推导能力位。
///
/// 命令编号是 MediaRemote 的稳定约定（0=Play、1=Pause、2=TogglePlayPause、3=Stop、
/// 4=Next、5=Previous、6=AdvanceShuffle、7=AdvanceRepeat…）。播放器**没有**上报时
/// 退回 [`Capabilities::BASIC`]：宁可按「常见播放器都能做」来给按钮，也别把所有按钮
/// 都置灰（回执里会如实报告失败）。
fn caps_from_commands(commands: &[i64], has_media: bool) -> Capabilities {
    if !has_media {
        return Capabilities::NONE;
    }
    if commands.is_empty() {
        return Capabilities::BASIC;
    }
    let has = |id: i64| commands.contains(&id);
    Capabilities {
        play: has(0),
        pause: has(1),
        toggle: has(2) || (has(0) && has(1)),
        stop: has(3),
        next: has(4),
        previous: has(5),
        // seek 由 MRMediaRemoteSetElapsedTime 实现，是否真生效靠回读校验
        seek_absolute: true,
        seek_relative: true,
        set_loop: has(7),
        set_shuffle: has(6),
        set_volume: false,
        set_mute: false,
        set_rate: false,
    }
}

/// 一次 now-playing 字典的解析结果（纯 Rust 类型，可安全跨线程带回）。
#[derive(Debug, Clone, Default)]
struct ParsedDict {
    title: String,
    artist: String,
    album: String,
    genre: String,
    duration_s: Option<f64>,
    elapsed_s: Option<f64>,
    calculated_elapsed_s: Option<f64>,
    rate: Option<f64>,
    artwork_data: Option<Vec<u8>>,
    artwork_mime: Option<String>,
    artwork_url: Option<String>,
    content_id: String,
    track_number: Option<i64>,
    repeat_raw: Option<i64>,
    shuffle_raw: Option<i64>,
    timestamp_ms: Option<u64>,
}

impl ParsedDict {
    fn has_any(&self) -> bool {
        !self.title.trim().is_empty() || !self.artist.trim().is_empty() || !self.content_id.is_empty()
    }
}

/// 解析 now-playing 字典。**必须在 block 回调里调用**（字典出回调即失效）。
fn parse_info_dict(dict: *const c_void, keys: &Keys) -> ParsedDict {
    let mut out = ParsedDict::default();
    if dict.is_null() {
        return out;
    }
    // 注意：`Keys` 里存的就是 `CFStringRef` 本身（dlsym 得到的是「指向该全局变量的指针」，
    // 解析时已经解过一层）。这里**不能再解一层** —— 多解一层会拿到字符串对象的前 8 个字节
    // 当键用，查表全部落空（而且不报错，只是「永远读不到元数据」）。
    let get = |key_addr: usize| -> *const c_void {
        if key_addr == 0 {
            return std::ptr::null();
        }
        unsafe { CFDictionaryGetValue(dict, key_addr as *const c_void) }
    };

    out.title = cf_string(get(keys.title)).unwrap_or_default();
    out.artist = cf_string(get(keys.artist)).unwrap_or_default();
    out.album = cf_string(get(keys.album)).unwrap_or_default();
    out.genre = cf_string(get(keys.genre)).unwrap_or_default();
    out.content_id = cf_string(get(keys.content_id)).unwrap_or_default();
    out.duration_s = cf_f64(get(keys.duration));
    out.elapsed_s = cf_f64(get(keys.elapsed));
    out.calculated_elapsed_s = cf_f64(get(keys.calculated_elapsed));
    out.rate = cf_f64(get(keys.rate));
    out.track_number = cf_i64(get(keys.track_number));
    out.repeat_raw = cf_i64(get(keys.repeat));
    out.shuffle_raw = cf_i64(get(keys.shuffle));
    out.timestamp_ms = cf_date_ms(get(keys.timestamp));
    out.artwork_mime = cf_string(get(keys.artwork_mime));
    out.artwork_url = cf_string(get(keys.artwork_url));
    // 封面字节：解析期就拷出来（CFData 出回调即失效）
    out.artwork_data = cf_bytes(get(keys.artwork_data));
    let _ = keys.disc_number;
    out
}

/// CFArray（元素是 NSNumber）→ Vec<i64>。同样只能在回调里用。
fn array_of_i64(arr: *const c_void) -> Vec<i64> {
    if arr.is_null() {
        return Vec::new();
    }
    unsafe {
        let n = CFArrayGetCount(arr);
        let mut out = Vec::with_capacity(n.max(0) as usize);
        for i in 0..n {
            let item = CFArrayGetValueAtIndex(arr, i);
            if let Some(v) = cf_i64(item) {
                out.push(v);
            }
        }
        out
    }
}

fn symbol_report(api: &Api) -> String {
    let mut missing = Vec::new();
    let entries: [(&str, usize); 9] = [
        ("GetNowPlayingInfo", api.get_now_playing_info),
        ("GetNowPlayingApplicationIsPlaying", api.get_is_playing),
        ("GetNowPlayingApplicationDisplayName", api.get_display_name),
        ("GetNowPlayingApplicationPID", api.get_pid),
        ("GetSupportedCommands", api.get_supported_commands),
        ("SendCommand", api.send_command),
        ("SetElapsedTime", api.set_elapsed_time),
        ("SetRepeatMode", api.set_repeat_mode),
        ("SetShuffleMode", api.set_shuffle_mode),
    ];
    for (name, addr) in entries {
        if addr == 0 {
            missing.push(name);
        }
    }
    if missing.is_empty() {
        "全部就绪".to_string()
    } else {
        format!("缺少 {}", missing.join(", "))
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// 调用助手（宏：把「队列 + block + 等回执」压成一行，参数类型保持显式）
// ══════════════════════════════════════════════════════════════════════════════

/// 调用一个 `(dispatch_queue_t, block)` 形状的 MediaRemote 接口。
///
/// `$arg` 必须与真实 block 的参数类型一致（指针就写指针、按值就写按值）——
/// block 的 ABI 由 block2 依这个类型生成，写错不会报错、只会读到垃圾。
macro_rules! mr_call {
    ($inner:expr, $addr:expr, $arg:ty, |$v:ident| $body:expr) => {{
        let addr = $addr;
        if addr == 0 {
            None
        } else {
            // 串行化：同一条队列上一次只等一个回执
            let _guard = $inner.ctrl.lock().unwrap_or_else(|e| e.into_inner());
            let (tx, rx) = std::sync::mpsc::channel();
            let block = block2::RcBlock::new(move |$v: $arg| {
                let _ = tx.send($body);
            });
            let f: unsafe extern "C" fn(*mut c_void, *mut c_void) =
                unsafe { std::mem::transmute(addr) };
            unsafe { f($inner.queue(), block2::RcBlock::as_ptr(&block).cast()) };
            match rx.recv_timeout(CALL_TIMEOUT) {
                Ok(v) => Some(v),
                Err(_) => {
                    // 超时：MediaRemote 那边可能还握着这个 block。宁可漏一个（几十字节，
                    // 且只在异常路径发生），也不能让它回调已经释放的内存。
                    std::mem::forget(block);
                    None
                }
            }
        }
    }};
}

impl Inner {
    /// 解析 `dlopen` 出来的符号表（只做一次）。
    fn api(&self) -> Option<Api> {
        *self.api.get_or_init(|| load_api())
    }

    /// 解析 now-playing 字典里的键（只做一次）。
    fn keys(&self) -> Keys {
        *self.keys.get_or_init(|| {
            let Some(handle) = dlopen_media_remote() else {
                return Keys::default();
            };
            let resolve = |name: &str| -> usize {
                let Ok(cs) = CString::new(name) else {
                    return 0;
                };
                let p = unsafe { dlsym(handle, cs.as_ptr()) };
                if p.is_null() {
                    return 0;
                }
                // 这些是 `CFStringRef` 全局量：dlsym 给的是指向它的指针，要解一层引用
                unsafe { *(p as *const *const c_void) as usize }
            };
            Keys {
                title: resolve("kMRMediaRemoteNowPlayingInfoTitle"),
                artist: resolve("kMRMediaRemoteNowPlayingInfoArtist"),
                album: resolve("kMRMediaRemoteNowPlayingInfoAlbum"),
                // 注意：框架**没有** kMRMediaRemoteNowPlayingInfoAlbumArtist 这个符号
                // （用 nm/dyld_info 把 MediaRemote 的导出符号列过一遍确认过）。
                // 所以 macOS 侧 albumArtist 永远为空 —— 不是漏了，是拿不到。
                genre: resolve("kMRMediaRemoteNowPlayingInfoGenre"),
                duration: resolve("kMRMediaRemoteNowPlayingInfoDuration"),
                elapsed: resolve("kMRMediaRemoteNowPlayingInfoElapsedTime"),
                calculated_elapsed: resolve("kMRMediaRemoteNowPlayingInfoCalculatedElapsedTime"),
                rate: resolve("kMRMediaRemoteNowPlayingInfoPlaybackRate"),
                artwork_data: resolve("kMRMediaRemoteNowPlayingInfoArtworkData"),
                artwork_mime: resolve("kMRMediaRemoteNowPlayingInfoArtworkMIMEType"),
                artwork_url: resolve("kMRMediaRemoteNowPlayingInfoArtworkURL"),
                content_id: resolve("kMRMediaRemoteNowPlayingInfoContentItemIdentifier"),
                track_number: resolve("kMRMediaRemoteNowPlayingInfoTrackNumber"),
                disc_number: resolve("kMRMediaRemoteNowPlayingInfoDiscNumber"),
                repeat: resolve("kMRMediaRemoteNowPlayingInfoRepeatMode"),
                shuffle: resolve("kMRMediaRemoteNowPlayingInfoShuffleMode"),
                timestamp: resolve("kMRMediaRemoteNowPlayingInfoTimestamp"),
            }
        })
    }

    /// 私有串行队列（只建一次，进程内复用）。
    ///
    /// 不能用主队列：主线程（或调用线程）正阻塞等回执，block 调度到主队列就永远跑不到。
    fn queue(&self) -> *mut c_void {
        let p = self.queue.get_or_init(|| {
            let label = CString::new("media-bridge.macos.mediaremote").unwrap_or_default();
            let q = unsafe { dispatch_queue_create(label.as_ptr(), std::ptr::null()) };
            SendPtr::new(q)
        });
        p.get::<c_void>() as *mut c_void
    }

    /// 注册成 MediaRemote 的 now-playing 客户端（+ 一次预热读取），进程内只做一次。
    ///
    /// **为什么必须注册**：macOS 26 上 `MRMediaRemoteGetNowPlayingInfo` 只对注册过的
    /// 客户端返回数据。不注册时它会回一个空字典 —— 表现就是「明明在放歌，中间件却报
    /// hasMedia=false」，而 App 侧看不出任何异常（`diagnose` 里「MediaRemote 已加载、
    /// NowPlayingInfo 键 8/8 可用」全部正常）。实测对照：同机上 `media-control`
    /// （MediaRemoteAdapter）用的是**同一组**读取接口，差别仅仅是它先调了这个注册；
    /// 汽水音乐（com.soda.music）在播时，未注册查询一个键都拿不到。
    ///
    /// 预热那一次读取是给 one-shot 场景用的：注册后守护进程要一拍才把本进程认成客户端，
    /// 紧接着的第一次读取往往还是空的。丢掉这一次的结果，让调用方真正的读取拿到数据，
    /// 否则 `media-bridge now` 这种一次性命令永远看到「没有媒体」。
    fn ensure_registered(&self) {
        self.registered.get_or_init(|| {
            let Some(api) = self.api() else { return };
            if api.register == 0 {
                return;
            }
            let f: unsafe extern "C" fn(*mut c_void) = unsafe { std::mem::transmute(api.register) };
            unsafe { f(self.queue()) };
            if api.get_now_playing_info != 0 {
                let _ = mr_call!(self, api.get_now_playing_info, *const c_void, |_v| ());
            }
        });
    }

    fn call_dict<T: Send + 'static>(
        &self,
        addr: usize,
        f: impl Fn(*const c_void) -> T + Send + 'static,
    ) -> Option<T> {
        mr_call!(self, addr, *const c_void, |v| f(v))
    }

    /// 调一个「回调参数是按值传的 bool」的接口。
    ///
    /// 这里用 `i8` 收而不是 `bool`：block2 的 `IntoBlock` 不为 `bool` 参数实现，
    /// 而两者在 ABI 上都只占一个字节（读低字节、非 0 即真），语义完全一致。
    fn call_bool(&self, addr: usize, f: impl Fn(bool) -> bool + Send + 'static) -> Option<bool> {
        mr_call!(self, addr, i8, |v| f(v != 0))
    }

    fn call_array<T: Send + 'static>(
        &self,
        addr: usize,
        f: impl Fn(*const c_void) -> T + Send + 'static,
    ) -> Option<T> {
        mr_call!(self, addr, *const c_void, |v| f(v))
    }

    fn call_string(&self, addr: usize) -> Option<String> {
        // 回调本身可能给 nil（没有播放器时）→ 外层 Option 表示「调用成不成」，
        // 内层表示「值有没有」，这里拍平：拿不到就是 None
        mr_call!(self, addr, *const c_void, |v| cf_string(v)).flatten()
    }

    fn call_int(&self, addr: usize) -> Option<i32> {
        mr_call!(self, addr, i32, |v| v)
    }

    /// 应用名/PID：只在换曲目/换应用时刷新（缓存键是内容标识）。
    fn refresh_app(&self, content_id: &str) {
        {
            let cache = self.app.read().unwrap_or_else(|e| e.into_inner());
            if !content_id.is_empty() && cache.id == content_id && cache.name.is_some() {
                return;
            }
        }
        let Some(api) = self.api() else { return };
        let name = self.call_string(api.get_display_name);
        let pid = self.call_int(api.get_pid).map(|p| p as u32);
        let mut cache = self.app.write().unwrap_or_else(|e| e.into_inner());
        cache.id = content_id.to_string();
        if name.is_some() {
            cache.name = name;
        }
        if pid.is_some() {
            cache.pid = pid;
        }
    }
}

/// `dlopen` MediaRemote（只做一次）。
fn dlopen_media_remote() -> Option<*mut c_void> {
    static HANDLE: OnceLock<SendPtr> = OnceLock::new();
    let ptr = HANDLE.get_or_init(|| {
        let path = CString::new("/System/Library/PrivateFrameworks/MediaRemote.framework/MediaRemote")
            .unwrap_or_default();
        SendPtr::new(unsafe { dlopen(path.as_ptr(), RTLD_NOW) })
    });
    let handle = ptr.get::<c_void>() as *mut c_void;
    if handle.is_null() {
        None
    } else {
        Some(handle)
    }
}

/// 解析全部需要的符号。
fn load_api() -> Option<Api> {
    let handle = dlopen_media_remote()?;
    let sym = |name: &str| -> usize {
        let Ok(cs) = CString::new(name) else {
            return 0;
        };
        unsafe { dlsym(handle, cs.as_ptr()) as usize }
    };
    let api = Api {
        get_now_playing_info: sym("MRMediaRemoteGetNowPlayingInfo"),
        get_is_playing: sym("MRMediaRemoteGetNowPlayingApplicationIsPlaying"),
        get_display_name: sym("MRMediaRemoteGetNowPlayingApplicationDisplayName"),
        get_pid: sym("MRMediaRemoteGetNowPlayingApplicationPID"),
        get_supported_commands: sym("MRMediaRemoteGetSupportedCommands"),
        send_command: sym("MRMediaRemoteSendCommand"),
        set_elapsed_time: sym("MRMediaRemoteSetElapsedTime"),
        set_repeat_mode: sym("MRMediaRemoteSetRepeatMode"),
        set_shuffle_mode: sym("MRMediaRemoteSetShuffleMode"),
        register: sym("MRMediaRemoteRegisterForNowPlayingNotifications"),
    };
    if api.all_present() {
        Some(api)
    } else {
        Some(api) // 关键符号缺失时也要保留（status 里会说明缺什么）
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 创建 CF 对象用的 FFI：只在测试里用，所以放在测试模块内 ──────────────
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFDictionaryCreateMutable(
            alloc: *const c_void,
            capacity: isize,
            key_cb: *const c_void,
            value_cb: *const c_void,
        ) -> *mut c_void;
        fn CFDictionarySetValue(dict: *mut c_void, key: *const c_void, value: *const c_void);
        fn CFStringCreateWithCString(alloc: *const c_void, s: *const c_char, enc: u32) -> *mut c_void;
        fn CFNumberCreate(alloc: *const c_void, t: isize, value: *const c_void) -> *mut c_void;
        fn CFDataCreate(alloc: *const c_void, bytes: *const u8, len: isize) -> *mut c_void;
        fn CFArrayCreate(
            alloc: *const c_void,
            values: *const *const c_void,
            num: isize,
            callbacks: *const c_void,
        ) -> *mut c_void;
        fn CFRelease(cf: *const c_void);
        static kCFTypeDictionaryKeyCallBacks: c_void;
        static kCFTypeDictionaryValueCallBacks: c_void;
        static kCFTypeArrayCallBacks: c_void;
    }
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};

    const CF_UTF8: u32 = 0x0800_0100;
    const CF_NUMBER_FLOAT64: isize = 13;
    const CF_NUMBER_SINT64: isize = 4;

    fn cfs(s: &str) -> *mut c_void {
        let cs = CString::new(s).unwrap_or_default();
        unsafe { CFStringCreateWithCString(std::ptr::null(), cs.as_ptr(), CF_UTF8) }
    }

    fn cfn_f64(v: f64) -> *mut c_void {
        unsafe {
            CFNumberCreate(
                std::ptr::null(),
                CF_NUMBER_FLOAT64,
                &v as *const f64 as *const c_void,
            )
        }
    }

    fn cfn_i64(v: i64) -> *mut c_void {
        unsafe { CFNumberCreate(std::ptr::null(), CF_NUMBER_SINT64, &v as *const i64 as *const c_void) }
    }

    /// 用真实的 CF 类型造一个 now-playing 字典。
    fn build_dict(pairs: &[(usize, *mut c_void)]) -> *mut c_void {
        unsafe {
            let dict = CFDictionaryCreateMutable(
                std::ptr::null(),
                8,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            );
            let handle = dlopen_media_remote().expect("MediaRemote 应可用");
            for (key_addr, value) in pairs {
                if *key_addr == 0 || value.is_null() {
                    continue;
                }
                // Keys 里存的就是 CFStringRef 本身，直接用
                let key = *key_addr as *const c_void;
                if key.is_null() {
                    continue;
                }
                let _ = handle;
                CFDictionarySetValue(dict, key, *value);
            }
            dict
        }
    }

    /// 核心验证：**用真 CF 类型 + 真 dlsym 键**走一遍真正的解析函数。
    ///
    /// 这条测试的价值在于它测的是真代码路径 —— 所有 CF 类型判定、键查找、单位换算
    /// （秒→毫秒、TimeSpan 无关的 f64 处理）都真的跑了一遍，而不是靠脑补的数据结构。
    #[test]
    fn parses_a_real_cf_dictionary() {
        let session = MacSession::new();
        let keys = session.inner.keys();
        if keys.title == 0 {
            eprintln!("跳过：本机拿不到 MediaRemote 的键（非 macOS 或不支持）");
            return;
        }
        let artwork = [0xFFu8, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4];
        let dict = build_dict(&[
            (keys.title, cfs("测试标题")),
            (keys.artist, cfs("测试歌手")),
            (keys.album, cfs("测试专辑")),
            (keys.genre, cfs("电子")),
            (keys.duration, cfn_f64(210.5)),
            (keys.elapsed, cfn_f64(12.25)),
            (keys.rate, cfn_f64(1.0)),
            (keys.track_number, cfn_i64(7)),
            (keys.repeat, cfn_i64(1)),
            (keys.shuffle, cfn_i64(0)),
            (keys.content_id, cfs("test-content-id")),
            (keys.artwork_mime, cfs("image/jpeg")),
            (
                keys.artwork_data,
                unsafe {
                    CFDataCreate(std::ptr::null(), artwork.as_ptr(), artwork.len() as isize)
                },
            ),
        ]);
        // 解析必须在「字典有效」的范围内进行（真实代码在 block 回调里做的就是这件事）
        let parsed = parse_info_dict(dict, &keys);
        unsafe { CFRelease(dict) };

        assert_eq!(parsed.title, "测试标题");
        assert_eq!(parsed.artist, "测试歌手");
        assert_eq!(parsed.album, "测试专辑");
        assert_eq!(parsed.genre, "电子");
        assert_eq!(parsed.duration_s, Some(210.5));
        assert_eq!(parsed.elapsed_s, Some(12.25));
        assert_eq!(parsed.rate, Some(1.0));
        assert_eq!(parsed.track_number, Some(7));
        assert_eq!(parsed.repeat_raw, Some(1));
        assert_eq!(parsed.shuffle_raw, Some(0));
        assert_eq!(parsed.content_id, "test-content-id");
        assert_eq!(parsed.artwork_mime.as_deref(), Some("image/jpeg"));
        assert_eq!(parsed.artwork_data.as_deref(), Some(&artwork[..]));
        assert!(parsed.has_any());
        // 原始值 1 = MPRepeatTypeOne（依据 MPRemoteControlTypes.h）→ 单曲循环
        assert_eq!(repeat_raw_to_loop(parsed.repeat_raw.unwrap()), LoopMode::Track);
    }

    #[test]
    fn real_cf_types_are_distinguished_by_type_id() {
        // 类型判定错了不会报错、只会静默给错值 —— 单独钉一下
        let session = MacSession::new();
        let keys = session.inner.keys();
        if keys.title == 0 {
            return;
        }
        let dict = build_dict(&[
            (keys.title, cfs("标题")),
            // duration 放字符串：不该被当成数字
            (keys.duration, cfs("不是数字")),
        ]);
        let parsed = parse_info_dict(dict, &keys);
        unsafe { CFRelease(dict) };
        assert_eq!(parsed.title, "标题");
        assert_eq!(parsed.duration_s, None, "字符串不能被当成 CFNumber");
    }

    #[test]
    fn null_dict_is_treated_as_no_media() {
        let session = MacSession::new();
        let keys = session.inner.keys();
        let parsed = parse_info_dict(std::ptr::null(), &keys);
        assert!(!parsed.has_any());
    }

    /// 命令数组解析：用真的 NSNumber 放进 CFArray（媒体命令正是 NSNumber 数组）。
    #[test]
    fn command_ids_are_read_from_a_real_cf_array() {
        let nsnumber = AnyClass::get(c"NSNumber").expect("NSNumber 类");
        let nums: Vec<*const c_void> = [0i64, 1, 2, 4, 6, 7]
            .iter()
            .map(|v| unsafe {
                let n: *mut AnyObject = msg_send![nsnumber, numberWithLongLong: *v];
                n as *const c_void
            })
            .collect();
        let arr = unsafe {
            CFArrayCreate(
                std::ptr::null(),
                nums.as_ptr(),
                nums.len() as isize,
                &kCFTypeArrayCallBacks,
            )
        };
        let ids = array_of_i64(arr);
        unsafe { CFRelease(arr) };
        assert_eq!(ids, vec![0, 1, 2, 4, 6, 7]);
        let caps = caps_from_commands(&ids, true);
        assert!(caps.play && caps.pause && caps.toggle && caps.next);
        assert!(caps.set_loop, "7 = AdvanceRepeatMode → 支持设置循环");
        assert!(caps.set_shuffle, "6 = AdvanceShuffleMode → 支持设置随机");
        assert!(!caps.previous, "没报 5 → 不支持上一曲");
    }

    #[test]
    fn repeat_raw_maps_per_apple_enum_order() {
        // MPRepeatType: Off = 0, One = 1, All = 2
        assert_eq!(repeat_raw_to_loop(0), LoopMode::Off);
        assert_eq!(repeat_raw_to_loop(1), LoopMode::Track);
        assert_eq!(repeat_raw_to_loop(2), LoopMode::Playlist);
        assert_eq!(repeat_raw_to_loop(9), LoopMode::Unknown);
    }

    #[test]
    fn capabilities_follow_reported_commands() {
        let caps = caps_from_commands(&[0, 1, 3, 4, 5], true);
        assert!(caps.play && caps.pause && caps.next && caps.previous);
        assert!(caps.toggle, "同时有 play/pause 应推出 toggle");
        assert!(!caps.set_loop, "没报 AdvanceRepeatMode 就不该声称支持循环设置");
        assert!(!caps.set_shuffle);
        assert!(caps.seek_absolute, "seek 由 SetElapsedTime 实现，能力位乐观给 true");
    }

    #[test]
    fn capabilities_fall_back_to_basic_when_player_reports_nothing() {
        let caps = caps_from_commands(&[], true);
        assert!(caps.play && caps.next);
        assert!(!caps.set_volume);
    }

    #[test]
    fn no_media_means_no_capabilities() {
        assert_eq!(caps_from_commands(&[0, 1, 2, 3, 4, 5, 6, 7], false).play, false);
    }

    #[test]
    fn send_ptr_round_trips() {
        let x = 42u8;
        let p = SendPtr::new(&x as *const u8);
        assert_eq!(unsafe { *p.get::<u8>() }, 42);
    }

    #[test]
    fn symbol_report_lists_missing() {
        let api = Api { get_now_playing_info: 1, ..Default::default() };
        let r = symbol_report(&api);
        assert!(r.contains("GetNowPlayingInfo") == false, "已具备的符号不该出现在缺失列表");
        assert!(r.contains("SendCommand"));
    }
}
