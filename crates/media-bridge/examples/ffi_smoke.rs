//! FFI 冒烟测试：验证 macOS 上三件最容易出错的事
//!
//!   1. `dlopen` 私有框架 MediaRemote 并调用 **block 参数**的回调（`dlopen` + `block2`）；
//!   2. 用 ObjC 运行时凭空造 `CATapDescription` 对象（`objc2::msg_send!`，无 Swift、无 ObjC 源码）；
//!   3. 用 CoreFoundation 手搓 `CFDictionary` 建私有聚合设备，并读回 tap 的 UID 属性。
//!
//! 用法：`cargo run --example ffi_smoke`
//! 退出码 0 = 三项全通；非 0 = 对应项失败（每项独立报告，失败不影响后续项）。

#[cfg(target_os = "macos")]
use std::ffi::CString;

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("ffi_smoke 只在 macOS 上有意义（当前平台不含私有框架路径）");
}

#[cfg(target_os = "macos")]
fn main() {
    let mut bad = 0;
    if !probe_mediaremote() {
        bad += 1;
    }
    if !probe_tap() {
        bad += 1;
    }
    println!();
    if bad == 0 {
        println!("✅ 三项全部通过");
    } else {
        println!("❌ {bad} 项失败");
        std::process::exit(1);
    }
}

// ══════════════════════════════════════════════════════════════════════════
#[cfg(target_os = "macos")]
mod ffi {
    use std::ffi::{c_char, c_int, c_void};

    // ── libSystem / libdispatch ─────────────────────────────────────────
    #[link(name = "System", kind = "dylib")]
    unsafe extern "C" {
        pub fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
        pub fn dlsym(handle: *mut c_void, sym: *const c_char) -> *mut c_void;
        pub fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
        pub fn dispatch_semaphore_create(value: isize) -> *mut c_void;
        pub fn dispatch_semaphore_signal(sem: *mut c_void) -> isize;
        pub fn dispatch_semaphore_wait(sem: *mut c_void, timeout: u64) -> isize;
        pub fn dispatch_time(when: u64, delta: i64) -> u64;
    }
    pub const RTLD_NOW: c_int = 0x2;
    pub const DISPATCH_TIME_NOW: u64 = 0;

    // ── CoreFoundation ──────────────────────────────────────────────────
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub fn CFShow(obj: *const c_void);
        pub fn CFDictionaryCreateMutable(
            alloc: *const c_void,
            capacity: isize,
            key_cb: *const c_void,
            value_cb: *const c_void,
        ) -> *mut c_void;
        pub fn CFDictionarySetValue(dict: *mut c_void, key: *const c_void, value: *const c_void);
        pub fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
        pub fn CFArrayCreate(
            alloc: *const c_void,
            values: *const *const c_void,
            num: isize,
            callbacks: *const c_void,
        ) -> *mut c_void;
        pub fn CFStringCreateWithCString(
            alloc: *const c_void,
            s: *const c_char,
            encoding: u32,
        ) -> *mut c_void;
        pub fn CFStringGetCString(
            s: *const c_void,
            buf: *mut c_char,
            size: isize,
            encoding: u32,
        ) -> bool;
        pub fn CFBooleanGetTypeID() -> usize;
        pub fn CFGetTypeID(cf: *const c_void) -> usize;
        pub fn CFBooleanGetValue(b: *const c_void) -> u8;
        pub fn CFRelease(cf: *const c_void);
        pub static kCFTypeDictionaryKeyCallBacks: c_void;
        pub static kCFTypeDictionaryValueCallBacks: c_void;
        pub static kCFTypeArrayCallBacks: c_void;
        pub static kCFBooleanTrue: *const c_void;
    }
    pub const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
}

#[cfg(target_os = "macos")]
use ffi::*;

/// 1) MediaRemote：dlopen + dlsym + block 回调
#[cfg(target_os = "macos")]
fn probe_mediaremote() -> bool {
    use block2::RcBlock;
    use std::ffi::c_void;

    println!("== 1. MediaRemote（dlopen + block 回调）==");
    let path = CString::new(
        "/System/Library/PrivateFrameworks/MediaRemote.framework/MediaRemote",
    )
    .unwrap();
    let handle = unsafe { dlopen(path.as_ptr(), RTLD_NOW) };
    if handle.is_null() {
        println!("  ❌ dlopen 失败（该框架在本机不可用）");
        return false;
    }
    println!("  ✅ dlopen 成功");

    let name = CString::new("MRMediaRemoteGetNowPlayingInfo").unwrap();
    let sym = unsafe { dlsym(handle, name.as_ptr()) };
    if sym.is_null() {
        println!("  ❌ dlsym(MRMediaRemoteGetNowPlayingInfo) 为空");
        return false;
    }
    println!("  ✅ dlsym 成功 @ {sym:p}");

    // 私有串行队列：不能用主队列 —— 主线程正阻塞在信号量上，block 永远不会被调度
    let label = CString::new("media-bridge.ffi_smoke").unwrap();
    let queue = unsafe { dispatch_queue_create(label.as_ptr(), std::ptr::null()) };
    let sem = unsafe { dispatch_semaphore_create(0) };
    let sem_addr = sem as usize;

    // dlsym 拿「字典的键」——这些是框架导出的 CFStringRef 全局量，
    // 不是 C 字符串字面量：dlsym 得到的是 `*const CFStringRef`，要解一层引用。
    let resolve_key = |sym: &str| -> *const c_void {
        let cs = CString::new(sym).unwrap();
        let p = unsafe { dlsym(handle, cs.as_ptr()) };
        if p.is_null() {
            return std::ptr::null();
        }
        unsafe { *(p as *const *const c_void) }
    };
    let keys: Vec<(*const c_void, &'static str)> = [
        "kMRMediaRemoteNowPlayingInfoTitle",
        "kMRMediaRemoteNowPlayingInfoArtist",
        "kMRMediaRemoteNowPlayingInfoAlbum",
        "kMRMediaRemoteNowPlayingInfoDuration",
        "kMRMediaRemoteNowPlayingInfoElapsedTime",
        "kMRMediaRemoteNowPlayingInfoArtworkData",
        "kMRMediaRemoteNowPlayingInfoArtworkMIMEType",
        "kMRMediaRemoteNowPlayingInfoRepeatMode",
        "kMRMediaRemoteNowPlayingInfoShuffleMode",
    ]
    .into_iter()
    .map(|n| (resolve_key(n), n))
    .collect();
    let found = keys.iter().filter(|(p, _)| !p.is_null()).count();
    println!("  ✅ dlsym 到 {found}/{} 个 NowPlayingInfo 键", keys.len());
    let keys_addr = keys.as_ptr() as usize;
    let keys_len = keys.len();

    let block = RcBlock::new(move |info: *const c_void| {
        println!("  ✅ block 被回调（info = {info:p}）");
        if info.is_null() {
            println!("  ⚠️ info 为 NULL（当前没有正在播放的媒体，属正常）");
            unsafe { dispatch_semaphore_signal(sem_addr as *mut c_void) };
            return;
        }
        let keys = unsafe {
            std::slice::from_raw_parts(keys_addr as *const (*const c_void, &'static str), keys_len)
        };
        for (key, name) in keys {
            if key.is_null() {
                continue;
            }
            let v = unsafe { CFDictionaryGetValue(info, *key) };
            if v.is_null() {
                continue;
            }
            let is_bool = unsafe { CFGetTypeID(v) == CFBooleanGetTypeID() };
            if is_bool {
                println!("    {name} = <bool {}>", unsafe { CFBooleanGetValue(v) } != 0);
                continue;
            }
            let mut buf = [0i8; 256];
            let ok = unsafe { CFStringGetCString(v, buf.as_mut_ptr(), buf.len() as isize, K_CF_STRING_ENCODING_UTF8) };
            if ok {
                let s = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
                let shown = if s.len() > 60 { format!("{}…", &s[..60.min(s.len())]) } else { s.into_owned() };
                println!("    {name} = {shown}");
            } else {
                println!("    {name} = <{} 字节的非字符串值>", unsafe { CFGetTypeID(v) });
            }
        }
        unsafe { dispatch_semaphore_signal(sem_addr as *mut c_void) };
    });

    let get: unsafe extern "C" fn(*mut c_void, *mut c_void) = unsafe { std::mem::transmute(sym) };
    unsafe { get(queue, RcBlock::as_ptr(&block).cast()) };

    let timeout = unsafe { dispatch_time(DISPATCH_TIME_NOW, 3_000_000_000) };
    let rc = unsafe { dispatch_semaphore_wait(sem, timeout) };
    if rc != 0 {
        println!("  ❌ 3s 内 block 未被回调（超时）");
        return false;
    }
    true
}

/// 2)+3) CoreAudio：ObjC 造 CATapDescription → 建 tap → 建私有聚合设备 → 读回 UID
#[cfg(target_os = "macos")]
fn probe_tap() -> bool {
    use objc2::msg_send;
    use objc2::rc::autoreleasepool;
    use objc2::runtime::{AnyClass, AnyObject};
    use std::ffi::c_void;

    println!("\n== 2. CoreAudio Process Tap（ObjC 运行时 + CFDictionary）==");

    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C" {
        fn AudioHardwareCreateProcessTap(desc: *mut AnyObject, out: *mut u32) -> i32;
        fn AudioHardwareDestroyProcessTap(tap: u32) -> i32;
        fn AudioHardwareCreateAggregateDevice(desc: *const c_void, out: *mut u32) -> i32;
        fn AudioHardwareDestroyAggregateDevice(dev: u32) -> i32;
        fn AudioObjectGetPropertyData(
            obj: u32,
            addr: *const AudioObjectPropertyAddress,
            qual_size: u32,
            qual: *const c_void,
            size: *mut u32,
            out: *mut c_void,
        ) -> i32;
    }

    #[repr(C)]
    struct AudioObjectPropertyAddress {
        selector: u32,
        scope: u32,
        element: u32,
    }
    const fn fourcc(b: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*b)
    }
    const SCOPE_GLOBAL: u32 = fourcc(b"glob");
    const TAP_PROP_UID: u32 = fourcc(b"tuid");

    unsafe {
        let Some(tap_cls) = AnyClass::get(c"CATapDescription") else {
            println!("  ❌ 找不到 CATapDescription 类（需要 macOS 14.2+）");
            return false;
        };
        println!("  ✅ 找到 CATapDescription 类");

        let tap_obj = autoreleasepool(|_| {
            let nsarray_cls = AnyClass::get(c"NSArray").expect("NSArray");
            let nsstring_cls = AnyClass::get(c"NSString").expect("NSString");
            let empty: *mut AnyObject = msg_send![nsarray_cls, array];
            let alloc: *mut AnyObject = msg_send![tap_cls, alloc];
            let tap: *mut AnyObject = msg_send![alloc, initStereoGlobalTapButExcludeProcesses: empty];
            if tap.is_null() {
                println!("  ❌ initStereoGlobalTapButExcludeProcesses: 返回 nil");
                return std::ptr::null_mut();
            }
            let name: *mut AnyObject =
                msg_send![nsstring_cls, stringWithUTF8String: c"media-bridge-smoke".as_ptr()];
            let _: () = msg_send![tap, setName: name];
            let _: () = msg_send![tap, setPrivate: true];
            let _: () = msg_send![tap, setMuteBehavior: 0isize]; // CATapUnmuted
            // autoreleasepool 之后还要用：retain 一次（测试结束不释放，进程随即退出）
            let _: *mut AnyObject = msg_send![tap, retain];
            tap
        });
        if tap_obj.is_null() {
            return false;
        }
        println!("  ✅ CATapDescription 构造成功");

        let mut tap_id: u32 = 0;
        let st = AudioHardwareCreateProcessTap(tap_obj, &mut tap_id);
        if st != 0 {
            println!(
                "  ❌ AudioHardwareCreateProcessTap 失败：OSStatus {st}（-4 通常是没给「音频录制」权限）"
            );
            println!("     → 授权路径：系统设置 → 隐私与安全性 → 音频录制");
            return false;
        }
        println!("  ✅ tap 创建成功，AudioObjectID = {tap_id}");

        // 读 tap 的 UID（作为聚合设备 kAudioSubTapUIDKey）
        let addr = AudioObjectPropertyAddress {
            selector: TAP_PROP_UID,
            scope: SCOPE_GLOBAL,
            element: 0,
        };
        let mut uid_ptr: *const c_void = std::ptr::null();
        let mut size = std::mem::size_of::<*const c_void>() as u32;
        let st = AudioObjectGetPropertyData(
            tap_id,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            &mut uid_ptr as *mut _ as *mut c_void,
        );
        if st != 0 || uid_ptr.is_null() {
            println!("  ❌ 读 kAudioTapPropertyUID 失败：OSStatus {st}");
            AudioHardwareDestroyProcessTap(tap_id);
            return false;
        }
        let mut buf = [0i8; 256];
        let ok = CFStringGetCString(uid_ptr, buf.as_mut_ptr(), 256, K_CF_STRING_ENCODING_UTF8);
        let uid = if ok {
            std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
        } else {
            String::from("<解码失败>")
        };
        println!("  ✅ tap UID = {uid}");
        CFRelease(uid_ptr);

        // 3) 私有聚合设备：CFDictionary{name,uid,private,taps:[{uid,drift}]}
        let mk_str = |s: &str| {
            let cs = CString::new(s).unwrap();
            CFStringCreateWithCString(std::ptr::null(), cs.as_ptr(), K_CF_STRING_ENCODING_UTF8)
        };
        let k_name = mk_str("name");
        let k_uid = mk_str("uid");
        let k_private = mk_str("private");
        let k_taps = mk_str("taps");
        let k_drift = mk_str("drift");

        let v_name = mk_str("media-bridge-tap-agg");
        let v_uid = mk_str(&format!("media-bridge-agg-{tap_id}"));
        let sub = CFDictionaryCreateMutable(
            std::ptr::null(),
            2,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );
        CFDictionarySetValue(sub, k_uid, mk_str(&uid));
        CFDictionarySetValue(sub, k_drift, kCFBooleanTrue);
        let taps = CFArrayCreate(
            std::ptr::null(),
            &(sub as *const c_void),
            1,
            &kCFTypeArrayCallBacks,
        );
        let agg = CFDictionaryCreateMutable(
            std::ptr::null(),
            4,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );
        CFDictionarySetValue(agg, k_name, v_name);
        CFDictionarySetValue(agg, k_uid, v_uid);
        CFDictionarySetValue(agg, k_private, kCFBooleanTrue);
        CFDictionarySetValue(agg, k_taps, taps);
        CFShow(agg);

        let mut agg_id: u32 = 0;
        let st = AudioHardwareCreateAggregateDevice(agg, &mut agg_id);
        if st != 0 {
            println!("  ❌ AudioHardwareCreateAggregateDevice 失败：OSStatus {st}");
            AudioHardwareDestroyProcessTap(tap_id);
            return false;
        }
        println!("  ✅ 聚合设备创建成功，AudioDeviceID = {agg_id}");

        // 收尾：立刻拆掉（冒烟测试不采集）
        let _ = AudioHardwareDestroyAggregateDevice(agg_id);
        let _ = AudioHardwareDestroyProcessTap(tap_id);
        println!("  ✅ 资源已回收");
        true
    }
}
