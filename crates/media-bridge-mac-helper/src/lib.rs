//! media-bridge-mac-helper —— 在**被授权**的进程里读 MediaRemote 的 now-playing。
//!
//! 为什么需要这么一个动态库：
//!
//! macOS 15.4 起，MediaRemote 私有框架**只对「被授权」的进程返回数据** —— 第三方
//! 进程（Electron / Rust 二进制 / 我们自己的中间件）调用 `MRMediaRemoteGetNowPlayingInfo`
//! 会拿到空字典：进程里一切正常（符号全部就绪、`diagnose` 显示已加载），但一个键都读不
//! 到，表现就是「明明在放歌，却 hasMedia=false」。实测（macOS 27 + 汽水音乐）：同一组接口
//! 在自己的进程里是空字典，借 `/usr/bin/perl`（Apple 自带二进制，守护进程认它作
//! `com.apple.perl`）就能正常返回 13–14 个键。
//!
//! 所以访问路径是：**中间件 → `/usr/bin/perl` 加载本动态库 → 本库在 perl 进程里调
//! MediaRemote**。同类实现（media-control / MediaRemoteAdapter）用的是同一套办法。
//!
//! 对外暴露两个**无参**入口：`mb_now_playing_json`（例行轮询，不带封面）与
//! `mb_now_playing_json_with_artwork`（换曲那一拍），各写一行 JSON 到 stdout。
//! 由 `crates/media-bridge/build.rs` 找出来嵌进主二进制，运行时落盘再交给 perl。
#![cfg(target_os = "macos")]

use block2::RcBlock;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

// ── CoreFoundation / libSystem 的最小 FFI（只用到读字典这一路）──────────────
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDictionaryGetValue(the_dict: *const c_void, key: *const c_void) -> *const c_void;
    fn CFDictionaryGetCount(the_dict: *const c_void) -> isize;
    fn CFGetTypeID(cf: *const c_void) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFDataGetTypeID() -> usize;
    fn CFStringGetLength(s: *const c_void) -> isize;
    fn CFStringGetCString(s: *const c_void, buf: *mut c_char, size: isize, enc: u32) -> bool;
    fn CFNumberGetValue(num: *const c_void, the_type: isize, out: *mut c_void) -> bool;
    fn CFDataGetBytePtr(d: *const c_void) -> *const u8;
    fn CFDataGetLength(d: *const c_void) -> isize;
    fn CFRetain(cf: *const c_void) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

#[link(name = "System")]
unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
}

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const K_CF_NUMBER_DOUBLE: isize = 13; // kCFNumberDoubleType
const RTLD_NOW: c_int = 2;
/// 等待 MediaRemote 回执的上限（实测本机 <5ms 就回来；留足余量后放弃即可）
const READ_TIMEOUT: Duration = Duration::from_millis(600);

/// 带 block 的读取函数统一签名：`void f(dispatch_queue_t, void (^)(T))`
type QueryFn = unsafe extern "C" fn(*mut c_void, *mut c_void);
/// `void MRMediaRemoteRegisterForNowPlayingNotifications(dispatch_queue_t)`
type RegisterFn = unsafe extern "C" fn(*mut c_void);

struct Api {
    get_info: QueryFn,
    register: Option<RegisterFn>,
    get_is_playing: Option<QueryFn>,
    get_pid: Option<QueryFn>,
    queue: *mut c_void,
    keys: Keys,
}

// SAFETY: 装的都是进程生命周期内有效的函数指针与 CFStringRef 全局量地址，只读使用。
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

#[derive(Default)]
struct Keys {
    title: *const c_void,
    artist: *const c_void,
    album: *const c_void,
    genre: *const c_void,
    duration: *const c_void,
    elapsed: *const c_void,
    calculated_elapsed: *const c_void,
    rate: *const c_void,
    artwork_data: *const c_void,
    artwork_mime: *const c_void,
    content_id: *const c_void,
    track_number: *const c_void,
    disc_number: *const c_void,
    repeat: *const c_void,
    shuffle: *const c_void,
}

unsafe impl Send for Keys {}
unsafe impl Sync for Keys {}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let path = CString::new(
            "/System/Library/PrivateFrameworks/MediaRemote.framework/MediaRemote",
        )
        .ok()?;
        let handle = dlopen(path.as_ptr(), RTLD_NOW);
        if handle.is_null() {
            return None;
        }
        let sym = |name: &str| -> *mut c_void {
            CString::new(name)
                .ok()
                .map(|c| dlsym(handle, c.as_ptr()))
                .unwrap_or(std::ptr::null_mut())
        };
        let query = |name: &str| -> Option<QueryFn> {
            let p = sym(name);
            (!p.is_null()).then(|| std::mem::transmute::<*mut c_void, QueryFn>(p))
        };
        let get_info = query("MRMediaRemoteGetNowPlayingInfo")?;
        // 键是 `CFStringRef` 全局量：dlsym 给的是「指向它的指针」，要解一层引用
        let key = |name: &str| -> *const c_void {
            let p = sym(name);
            if p.is_null() {
                std::ptr::null()
            } else {
                *(p as *const *const c_void)
            }
        };
        let queue = dispatch_queue_create(c"media-bridge.helper".as_ptr(), std::ptr::null());
        let register = {
            let p = sym("MRMediaRemoteRegisterForNowPlayingNotifications");
            (!p.is_null()).then(|| std::mem::transmute::<*mut c_void, RegisterFn>(p))
        };
        Some(Api {
            get_info,
            register,
            get_is_playing: query("MRMediaRemoteGetNowPlayingApplicationIsPlaying"),
            get_pid: query("MRMediaRemoteGetNowPlayingApplicationPID"),
            queue,
            keys: Keys {
                title: key("kMRMediaRemoteNowPlayingInfoTitle"),
                artist: key("kMRMediaRemoteNowPlayingInfoArtist"),
                album: key("kMRMediaRemoteNowPlayingInfoAlbum"),
                genre: key("kMRMediaRemoteNowPlayingInfoGenre"),
                duration: key("kMRMediaRemoteNowPlayingInfoDuration"),
                elapsed: key("kMRMediaRemoteNowPlayingInfoElapsedTime"),
                calculated_elapsed: key("kMRMediaRemoteNowPlayingInfoCalculatedElapsedTime"),
                rate: key("kMRMediaRemoteNowPlayingInfoPlaybackRate"),
                artwork_data: key("kMRMediaRemoteNowPlayingInfoArtworkData"),
                artwork_mime: key("kMRMediaRemoteNowPlayingInfoArtworkMIMEType"),
                content_id: key("kMRMediaRemoteNowPlayingInfoContentItemIdentifier"),
                track_number: key("kMRMediaRemoteNowPlayingInfoTrackNumber"),
                disc_number: key("kMRMediaRemoteNowPlayingInfoDiscNumber"),
                repeat: key("kMRMediaRemoteNowPlayingInfoRepeatMode"),
                shuffle: key("kMRMediaRemoteNowPlayingInfoShuffleMode"),
            },
        })
    })
    .as_ref()
}

/// 发一次带 block 的查询并等回执（block 可能晚到，所以超时也只把它漏在堆上、不提前释放）。
///
/// 两个具体类型各写一份：block2 的 `IntoBlock` 不为**泛型**闭包实现，
/// 而裸指针又不是 `Send`（跨线程回填需要可 Send 的值）—— 指针因此按 `usize` 收。
fn query_i32(f: Option<QueryFn>, queue: *mut c_void) -> Option<i32> {
    let f = f?;
    let slot: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
    let writer = slot.clone();
    let block = RcBlock::new(move |v: c_int| {
        if let Ok(mut g) = writer.lock() {
            *g = Some(v);
        }
    });
    unsafe { f(queue, RcBlock::as_ptr(&block).cast()) };
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut out = None;
    while Instant::now() < deadline {
        if let Ok(g) = slot.lock()
            && let Some(v) = *g
        {
            out = Some(v);
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    std::mem::forget(block);
    out
}

/// 从 CFString 取值（UTF-8）。
unsafe fn cf_string(v: *const c_void) -> Option<String> {
    if v.is_null() || unsafe { CFGetTypeID(v) } != unsafe { CFStringGetTypeID() } {
        return None;
    }
    let len = unsafe { CFStringGetLength(v) };
    let cap = (len * 4 + 8).max(16);
    let mut buf = vec![0i8; cap as usize];
    if unsafe { CFStringGetCString(v, buf.as_mut_ptr(), cap, K_CF_STRING_ENCODING_UTF8) } {
        return unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_str()
            .ok()
            .map(str::to_string);
    }
    None
}

/// 从 CFNumber 取 f64。
unsafe fn cf_number(v: *const c_void) -> Option<f64> {
    if v.is_null() || unsafe { CFGetTypeID(v) } != unsafe { CFNumberGetTypeID() } {
        return None;
    }
    let mut out: f64 = 0.0;
    if unsafe {
        CFNumberGetValue(
            v,
            K_CF_NUMBER_DOUBLE,
            (&mut out as *mut f64).cast::<c_void>(),
        )
    } {
        Some(out)
    } else {
        None
    }
}

/// 读一次 now-playing，返回 JSON。
///
/// `with_artwork = false` 时**不**附带封面字节：封面动辄几百 KB，例行轮询不该每秒搬一次
///（消费端只需在换曲那一拍取一次，之后沿用缓存 —— service 层的「永不降级」会保留
/// 同一首的上一张封面）。
fn read_json(with_artwork: bool) -> serde_json::Value {
    let Some(api) = api() else {
        return serde_json::json!({ "ok": false, "error": "MediaRemote 不可用（dlopen/dlsym 失败）" });
    };
    // 注册成 now-playing 客户端：不注册时守护进程不一定把数据给我们
    //（本库跑在 perl 进程里，注册的也是 perl —— 这正是借它身份的意义）。
    if let Some(reg) = api.register {
        unsafe { reg(api.queue) };
    }

    // 字典只在回调期间有效 → 回调里 CFRetain 带出来，读完再 CFRelease
    let slot: Arc<Mutex<Option<*const c_void>>> = Arc::new(Mutex::new(None));
    let writer = slot.clone();
    let block = RcBlock::new(move |dict: *const c_void| {
        if !dict.is_null() {
            unsafe { CFRetain(dict) };
        }
        if let Ok(mut g) = writer.lock() {
            *g = Some(dict);
        }
    });
    unsafe { (api.get_info)(api.queue, RcBlock::as_ptr(&block).cast()) };
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut dict: *const c_void = std::ptr::null();
    while Instant::now() < deadline {
        if let Ok(g) = slot.lock()
            && let Some(d) = *g
        {
            dict = d;
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    std::mem::forget(block);

    if dict.is_null() {
        return serde_json::json!({ "ok": true, "hasMedia": false, "detail": "MediaRemote 未回执" });
    }

    let keys = &api.keys;
    let get = |k: *const c_void| -> *const c_void {
        if k.is_null() {
            std::ptr::null()
        } else {
            unsafe { CFDictionaryGetValue(dict, k) }
        }
    };
    let title = unsafe { cf_string(get(keys.title)) }.unwrap_or_default();
    let artist = unsafe { cf_string(get(keys.artist)) }.unwrap_or_default();
    let album = unsafe { cf_string(get(keys.album)) }.unwrap_or_default();
    let genre = unsafe { cf_string(get(keys.genre)) }.unwrap_or_default();
    let content_id = unsafe { cf_string(get(keys.content_id)) }.unwrap_or_default();
    let duration_s = unsafe { cf_number(get(keys.duration)) };
    // 位置优先 CalculatedElapsedTime（系统算好的当前值），退回 ElapsedTime
    let position_s = unsafe { cf_number(get(keys.calculated_elapsed)) }
        .or_else(|| unsafe { cf_number(get(keys.elapsed)) });
    let rate = unsafe { cf_number(get(keys.rate)) };
    let repeat = unsafe { cf_number(get(keys.repeat)) };
    let shuffle = unsafe { cf_number(get(keys.shuffle)) };
    let track_number = unsafe { cf_number(get(keys.track_number)) };
    let disc_number = unsafe { cf_number(get(keys.disc_number)) };

    // 没有曲名也没有歌手 = 守护进程没给有效数据（本轮按「没有媒体」处理）
    if title.is_empty() && artist.is_empty() {
        let keys_count = unsafe { CFDictionaryGetCount(dict) };
        unsafe { CFRelease(dict) };
        return serde_json::json!({ "ok": true, "hasMedia": false, "keys": keys_count });
    }

    let playing: Option<bool> = query_i32(api.get_is_playing, api.queue).map(|v| v != 0);
    // 应用名（`MRMediaRemoteGetNowPlayingApplicationDisplayName`）与能力位
    //（`MRMediaRemoteGetSupportedCommands`）在 macOS 27 上**取不到**：前者调了会崩
    //（回调返回 CFStringRef 的 block，实测在 perl 进程里必崩），后者回空数组。
    // 因此这里只取 PID；能力位由服务层回落到「常见播放器都能做」的 BASIC 档，
    // 名称字段留空（直连路径在 15.4 以下仍然提供它们）。
    let app_pid: Option<i64> = query_i32(api.get_pid, api.queue).map(i64::from);
    let commands: Vec<i64> = Vec::new();

    let mut out = serde_json::Map::new();
    out.insert("ok".into(), true.into());
    out.insert("hasMedia".into(), true.into());
    out.insert("title".into(), title.into());
    out.insert("artist".into(), artist.into());
    out.insert("album".into(), album.into());
    out.insert("genre".into(), genre.into());
    out.insert("contentId".into(), content_id.into());
    // 毫秒按**整数**发：JSON 里写成 211167.0 会让只认整数的消费端（as_u64）读成 0
    if let Some(v) = duration_s {
        out.insert("durationMs".into(), ((v.max(0.0) * 1000.0).round() as i64).into());
    }
    if let Some(v) = position_s {
        out.insert("positionMs".into(), ((v.max(0.0) * 1000.0).round() as i64).into());
    }
    if let Some(v) = rate {
        out.insert("rate".into(), v.into());
    }
    if let Some(v) = repeat {
        out.insert("repeatMode".into(), (v as i64).into());
    }
    if let Some(v) = shuffle {
        out.insert("shuffleMode".into(), (v as i64).into());
    }
    if let Some(v) = track_number {
        out.insert("trackNumber".into(), (v as i64).into());
    }
    if let Some(v) = disc_number {
        out.insert("discNumber".into(), (v as i64).into());
    }
    if let Some(p) = playing {
        out.insert("playing".into(), p.into());
    }
    if let Some(p) = app_pid {
        out.insert("appPid".into(), p.into());
    }
    if !commands.is_empty() {
        out.insert("commands".into(), commands.into());
    }

    if with_artwork {
        let data = get(keys.artwork_data);
        if !data.is_null() && unsafe { CFGetTypeID(data) } == unsafe { CFDataGetTypeID() } {
            let len = unsafe { CFDataGetLength(data) };
            let ptr = unsafe { CFDataGetBytePtr(data) };
            if !ptr.is_null() && len > 0 {
                let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
                out.insert("artworkBase64".into(), base64_encode(bytes).into());
                out.insert(
                    "artworkMime".into(),
                    unsafe { cf_string(get(keys.artwork_mime)) }
                        .unwrap_or_default()
                        .into(),
                );
            }
        }
    }

    let value = serde_json::Value::Object(out);
    unsafe { CFRelease(dict) };
    value
}

/// 极其朴素的 base64（只需 encode，避免为它引一个依赖）。
fn base64_encode(bytes: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        s.push(T[(n >> 18 & 63) as usize] as char);
        s.push(T[(n >> 12 & 63) as usize] as char);
        s.push(if chunk.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        s.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    s
}

fn emit(with_artwork: bool) {
    let value = read_json(with_artwork);
    let line = serde_json::to_string(&value).unwrap_or_else(|_| "{\"ok\":false}".to_string());
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    // 必须立刻刷出：调用方（中间件）按行读 stdout，缓冲会让它一直等
    let _ = out.flush();
}

/// 由 perl 调用的入口（**无参**）：例行轮询用，不带封面。
///
/// 为什么拆成两个无参函数而不是 `fn(with_artwork: c_int)`：perl 的
/// `DynaLoader::dl_install_xsub` 生成的是 XSUB 包装，调用时第一个参数位置拿到的是
/// CV 指针而不是我们传的值（实测：`main::mb(0)` 进到 C 侧是非 0，于是「不带封面」
/// 那一档照样把封面搬了出来）。无参就没有这层歧义。
#[unsafe(no_mangle)]
pub extern "C" fn mb_now_playing_json() {
    emit(false);
}

/// 同 [`mb_now_playing_json`]，但附带 `artworkBase64` / `artworkMime`（换曲那一拍用）。
#[unsafe(no_mangle)]
pub extern "C" fn mb_now_playing_json_with_artwork() {
    emit(true);
}
