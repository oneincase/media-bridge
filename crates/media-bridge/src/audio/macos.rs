//! macOS 系统音频采集：CoreAudio Process Tap（macOS 14.2+）。
//!
//! 路径：`CATapDescription`（ObjC 运行时现造）→ `AudioHardwareCreateProcessTap` →
//! 私有聚合设备承载 tap → `AudioDeviceCreateIOProcIDWithBlock` 拿到 IOProc →
//! `AudioDeviceStart`。采集到的是**系统输出**（loopback），不经过麦克风。
//!
//! 权限：首次创建 tap 会走 TCC 的「音频录制」授权。被拒绝时
//! `AudioHardwareCreateProcessTap` 返回非 0（常见 -4），我们把状态置为 `Denied`
//! 并给出设置路径 —— 不重试、不反复弹窗。
//!
//! 实时线程约束：IOProc 回调里只做「混成单声道 → 写环形缓冲」，**不分配、不加锁、
//! 不 logging**。混音需要一块临时缓冲，这块缓冲在启动时一次性分配（见 `TapContext`），
//! 回调里只复用它。

use std::ffi::{CString, c_char, c_void};
use std::sync::Arc;

use objc2::msg_send;
use objc2::rc::autoreleasepool;
use objc2::runtime::{AnyClass, AnyObject};

use super::CaptureShared;
use crate::error::{BridgeError, Result};
use crate::types::SourceState;

/// IOProc 单次回调最多混音的样本数（远超一帧 IO：48k/512 帧 = 1024 样本立体声）。
const SCRATCH_BYTES: usize = 16_384 * std::mem::size_of::<f32>();

// ── CoreAudio / CoreFoundation FFI ──────────────────────────────────────────

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
    fn AudioDeviceCreateIOProcIDWithBlock(
        out: *mut *mut c_void,
        dev: u32,
        queue: *mut c_void,
        block: *mut c_void,
    ) -> i32;
    fn AudioDeviceDestroyIOProcID(dev: u32, proc_id: *mut c_void) -> i32;
    fn AudioDeviceStart(dev: u32, proc_id: *mut c_void) -> i32;
    fn AudioDeviceStop(dev: u32, proc_id: *mut c_void) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDictionaryCreateMutable(
        alloc: *const c_void,
        capacity: isize,
        key_cb: *const c_void,
        value_cb: *const c_void,
    ) -> *mut c_void;
    fn CFDictionarySetValue(dict: *mut c_void, key: *const c_void, value: *const c_void);
    fn CFArrayCreate(
        alloc: *const c_void,
        values: *const *const c_void,
        num: isize,
        callbacks: *const c_void,
    ) -> *mut c_void;
    fn CFStringCreateWithCString(alloc: *const c_void, s: *const c_char, enc: u32) -> *mut c_void;
    fn CFStringGetCString(s: *const c_void, buf: *mut c_char, size: isize, enc: u32) -> bool;
    fn CFRelease(cf: *const c_void);
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    static kCFTypeArrayCallBacks: c_void;
    static kCFBooleanTrue: *const c_void;
}

const CF_UTF8: u32 = 0x0800_0100;

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
/// `kAudioTapPropertyUID`（AudioHardware.h）
const TAP_PROP_UID: u32 = fourcc(b"tuid");
/// `kAudioTapPropertyFormat`（AudioHardware.h）
const TAP_PROP_FORMAT: u32 = fourcc(b"tfmt");
/// `kAudioHardwarePropertyTranslatePIDToProcessObject`（AudioHardware.h）—— 选 `'id2p'`
const PROP_TRANSLATE_PID: u32 = fourcc(b"id2p");
/// `kAudioObjectSystemObject`
const SYSTEM_OBJECT: u32 = 1;

/// CoreAudio 里常见的错误码（都是四字符码）。
const ERR_BAD_OBJECT: i32 = 0x216F_626A; // '!obj'
const ERR_BAD_DEVICE: i32 = 0x2164_6576; // '!dev'
const ERR_BAD_STREAM: i32 = 0x2173_7472; // '!str'
const ERR_ILLEGAL_OPERATION: i32 = 0x7768_6F3F; // 'who?'
const ERR_UNSUPPORTED: i32 = 0x756E_6F70; // 'unop'

/// OSStatus → 可读文本（四字符码能打出来就打出来，否则给十进制/十六进制）。
fn osstatus_label(st: i32) -> String {
    let bytes = [
        ((st >> 24) & 0xFF) as u8,
        ((st >> 16) & 0xFF) as u8,
        ((st >> 8) & 0xFF) as u8,
        (st & 0xFF) as u8,
    ];
    let printable = bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ');
    if printable {
        format!("{} ({st})", String::from_utf8_lossy(&bytes))
    } else {
        format!("{st} (0x{:08X})", st as u32)
    }
}

/// Unix PID → CoreAudio 的**音频进程对象** ID。
///
/// tap 描述里的 `processes` 数组要的是进程对象 ID（CoreAudio 自己的编号），
/// **不是 Unix PID**。直接把 PID 塞进去，`AudioHardwareCreateProcessTap` 会返回
/// `'!obj'`（bad AudioObjectID）—— 而且错误信息不会告诉你是这里错了。
fn translate_pid_to_process_object(pid: u32) -> Option<u32> {
    let addr = AudioObjectPropertyAddress {
        selector: PROP_TRANSLATE_PID,
        scope: SCOPE_GLOBAL,
        element: 0,
    };
    let mut obj: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let st = unsafe {
        AudioObjectGetPropertyData(
            SYSTEM_OBJECT,
            &addr,
            std::mem::size_of::<u32>() as u32,
            &pid as *const u32 as *const c_void,
            &mut size,
            &mut obj as *mut u32 as *mut c_void,
        )
    };
    if st == 0 && obj != 0 { Some(obj) } else { None }
}

/// AudioBufferList / AudioBuffer（CoreAudioTypes.h）
#[repr(C)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

/// `AudioBufferList` 的头部：`{ UInt32 mNumberBuffers; AudioBuffer mBuffers[]; }`。
/// `mBuffers` 的偏移是 8（AudioBuffer 含指针，要求 8 字节对齐，前面补了 4 字节）。
const ABL_BUFFERS_OFFSET: usize = 8;

/// 音频格式描述（AudioStreamBasicDescription）
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Asbd {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

const FORMAT_FLAG_IS_FLOAT: u32 = 1 << 0;
const FORMAT_FLAG_IS_NON_INTERLEAVED: u32 = 1 << 5;

/// IOProc 回调期间用到的上下文。
///
/// `scratch` 是**启动时一次性分配**的混音缓冲（实时线程不能分配内存），
/// 生命周期由 `cleanup` 负责释放 —— 必须在设备停掉、IOProc 销毁之后才 free。
struct TapContext {
    shared: Arc<CaptureShared>,
    scratch: *mut f32,
    /// tap 的采样率（推样本时一并上报，频谱泵据此建立降采样器）
    rate: u32,
}

/// 启动采集。失败时返回错误（调用方会把状态置为不可用），**不 panic**。
pub(crate) fn start(shared: Arc<CaptureShared>) -> Result<()> {
    // 1) 造 CATapDescription（ObjC 运行时 + objc2，不需要 ObjC 源码）
    let tap_obj = create_tap_description(shared.config.exclude_self)?;

    // 2) 创建 tap
    let mut tap_id: u32 = 0;
    let st = unsafe { AudioHardwareCreateProcessTap(tap_obj, &mut tap_id) };
    unsafe {
        let _: *mut AnyObject = msg_send![tap_obj, release];
    }
    if st != 0 {
        let label = osstatus_label(st);
        // 错误分类要准：`!obj` 是「描述里的某个对象不合法」（参数问题），
        // `who?` 才是「系统不让你建 tap」（多半是没给音频录制权限）。把它们混成一句
        // 「请去授权」会让人白折腾一圈。
        return match st {
            ERR_BAD_OBJECT | ERR_BAD_DEVICE | ERR_BAD_STREAM | ERR_UNSUPPORTED => {
                let msg = format!(
                    "CoreAudio 拒绝了 tap 描述（OSStatus {label}）：通常是进程排除列表里的对象 ID 无效"
                );
                shared.set_state(SourceState::Error, msg.clone());
                Err(BridgeError::other(msg))
            }
            ERR_ILLEGAL_OPERATION => {
                let msg = format!(
                    "创建音频 tap 被系统拒绝（OSStatus {label}）：请在「系统设置 → 隐私与安全性 \
                     → 音频录制」里勾选本程序（或运行它的终端/宿主），然后重启程序。\
                     当前回落静音频谱。"
                );
                shared.set_state(SourceState::Denied, msg.clone());
                Err(BridgeError::Denied(msg))
            }
            _ => {
                let msg = format!("创建音频 tap 失败（OSStatus {label}）");
                shared.set_state(SourceState::Unavailable, msg.clone());
                Err(BridgeError::other(msg))
            }
        };
    }
    shared.set_state(SourceState::Preparing, "tap 已创建，正在准备聚合设备…");

    // 3) 读 tap 的格式（校验是不是我们假设的 Float32 —— 假设错了就明确报错，别猜）
    let asbd = match read_tap_format(tap_id) {
        Ok(a) => a,
        Err(e) => {
            unsafe { AudioHardwareDestroyProcessTap(tap_id) };
            shared.set_state(SourceState::Unavailable, format!("读取 tap 格式失败：{e}"));
            return Err(e);
        }
    };
    if asbd.bits_per_channel != 32 || asbd.format_flags & FORMAT_FLAG_IS_FLOAT == 0 {
        unsafe { AudioHardwareDestroyProcessTap(tap_id) };
        let msg = format!(
            "tap 格式不是 Float32（bits={} flags=0x{:x}）—— 本实现按 Float32 解析，暂不支持该格式",
            asbd.bits_per_channel, asbd.format_flags
        );
        shared.set_state(SourceState::Unavailable, msg.clone());
        return Err(BridgeError::unsupported(msg));
    }
    let channels = asbd.channels_per_frame.max(1);
    let _ = channels;
    let non_interleaved = asbd.format_flags & FORMAT_FLAG_IS_NON_INTERLEAVED != 0;

    // 4) 私有聚合设备承载 tap
    let tap_uid = match read_tap_uid(tap_id) {
        Ok(u) => u,
        Err(e) => {
            unsafe { AudioHardwareDestroyProcessTap(tap_id) };
            shared.set_state(SourceState::Unavailable, format!("读取 tap UID 失败：{e}"));
            return Err(e);
        }
    };
    let agg_desc = build_aggregate_description(&tap_uid, tap_id)?;
    let mut agg_id: u32 = 0;
    let st = unsafe { AudioHardwareCreateAggregateDevice(agg_desc.0, &mut agg_id) };
    unsafe { CFRelease(agg_desc.0) };
    if st != 0 {
        unsafe { AudioHardwareDestroyProcessTap(tap_id) };
        shared.set_state(SourceState::Unavailable, format!("创建聚合设备失败（OSStatus {st}）"));
        return Err(BridgeError::other(format!("AudioHardwareCreateAggregateDevice = {st}")));
    }

    // 5) IOProc：只做「混单声道 → 写环」
    let scratch = {
        let mut v = vec![0f32; SCRATCH_BYTES / std::mem::size_of::<f32>()];
        let ptr = v.as_mut_ptr();
        std::mem::forget(v);
        ptr
    };
    let ctx = Box::into_raw(Box::new(TapContext {
        shared: shared.clone(),
        scratch,
        rate: asbd.sample_rate.max(1.0).round() as u32,
    }));
    let ctx_addr = ctx as usize;

    let block = block2::RcBlock::new(
        move |_now: *const c_void,
              in_data: *const c_void,
              _in_time: *const c_void,
              _out_data: *mut c_void,
              _out_time: *const c_void| {
            if in_data.is_null() {
                return;
            }
            // SAFETY: ctx 活着（cleanup 里在设备停掉之后才释放），回调由 CoreAudio 串行调用
            let ctx = unsafe { &*(ctx_addr as *const TapContext) };
            unsafe { mix_and_push(ctx, in_data, non_interleaved) };
        },
    );

    let mut proc_id: *mut c_void = std::ptr::null_mut();
    let st = unsafe {
        AudioDeviceCreateIOProcIDWithBlock(&mut proc_id, agg_id, std::ptr::null_mut(), block2::RcBlock::as_ptr(&block).cast())
    };
    if st != 0 {
        unsafe {
            AudioHardwareDestroyAggregateDevice(agg_id);
            AudioHardwareDestroyProcessTap(tap_id);
            drop(Box::from_raw(ctx));
            drop(Vec::from_raw_parts(scratch, 0, SCRATCH_BYTES / std::mem::size_of::<f32>()));
        }
        shared.set_state(SourceState::Unavailable, format!("创建 IOProc 失败（OSStatus {st}）"));
        return Err(BridgeError::other(format!("AudioDeviceCreateIOProcIDWithBlock = {st}")));
    }

    let st = unsafe { AudioDeviceStart(agg_id, proc_id) };
    if st != 0 {
        unsafe {
            AudioDeviceDestroyIOProcID(agg_id, proc_id);
            AudioHardwareDestroyAggregateDevice(agg_id);
            AudioHardwareDestroyProcessTap(tap_id);
            drop(Box::from_raw(ctx));
            drop(Vec::from_raw_parts(scratch, 0, SCRATCH_BYTES / std::mem::size_of::<f32>()));
        }
        shared.set_state(SourceState::Unavailable, format!("启动采集设备失败（OSStatus {st}）"));
        return Err(BridgeError::other(format!("AudioDeviceStart = {st}")));
    }

    // 6) 收尾：先停设备再销毁 IOProc，最后才释放上下文/scratch（顺序不能反）
    // 收尾闭包必须是 Send（存放在 CaptureShared 里），所以这里把裸指针转成地址再带进去
    // —— 与 `SendPtr` 同一套理由：这些地址指向的都是本线程创建、本线程释放的对象。
    let shared_for_cleanup = shared.clone();
    let proc_addr = proc_id as usize;
    let ctx_addr_cleanup = ctx_addr;
    let scratch_addr = scratch as usize;
    let scratch_cap = SCRATCH_BYTES / std::mem::size_of::<f32>();
    shared.on_stop(Box::new(move || {
        let proc_id = proc_addr as *mut c_void;
        let scratch = scratch_addr as *mut f32;
        unsafe {
            // 顺序不能反：先停设备，再销毁 IOProc，最后才释放回调用的上下文与 scratch
            AudioDeviceStop(agg_id, proc_id);
            AudioDeviceDestroyIOProcID(agg_id, proc_id);
            AudioHardwareDestroyAggregateDevice(agg_id);
            AudioHardwareDestroyProcessTap(tap_id);
            drop(Box::from_raw(ctx_addr_cleanup as *mut TapContext));
            drop(Vec::from_raw_parts(scratch, 0, scratch_cap));
        }
        shared_for_cleanup.set_state(SourceState::Idle, "已停止");
    }));

    shared.set_state(
        SourceState::Running,
        format!(
            "CoreAudio 进程 tap（{:.0}Hz / {channels}ch / {}）",
            asbd.sample_rate,
            if non_interleaved { "非交错" } else { "交错" }
        ),
    );
    Ok(())
}

/// 用 ObjC 运行时造一个 CATapDescription。
///
/// 这一步没有任何 ObjC 源码：类与选择器都在运行时解析（CoreAudio 框架自带这个类）。
fn create_tap_description(exclude_self: bool) -> Result<*mut AnyObject> {
    autoreleasepool(|_| unsafe {
        let Some(tap_cls) = AnyClass::get(c"CATapDescription") else {
            return Err(BridgeError::unsupported(
                "找不到 CATapDescription 类：需要 macOS 14.2+（系统音频 tap 从这里开始才有）",
            ));
        };
        let Some(nsarray_cls) = AnyClass::get(c"NSArray") else {
            return Err(BridgeError::other("找不到 NSArray 类"));
        };
        let Some(nsstring_cls) = AnyClass::get(c"NSString") else {
            return Err(BridgeError::other("找不到 NSString 类"));
        };

        // 排除自身进程（避免中间件自己产生的声音被采进来形成反馈）。
        //
        // 注意：这里要的是 **CoreAudio 进程对象 ID**，不是 Unix PID。翻译不出来
        // （例如进程还没被 CoreAudio 认识）就退化成空列表 —— 不排除自己只是「可能
        // 采到自己的声音」，而塞错值会直接让 tap 创建失败。
        let exclude: *mut AnyObject = if exclude_self {
            let self_obj = translate_pid_to_process_object(std::process::id());
            match (self_obj, AnyClass::get(c"NSNumber")) {
                (Some(obj), Some(nsnumber_cls)) => {
                    let num: *mut AnyObject = msg_send![nsnumber_cls, numberWithUnsignedInt: obj];
                    msg_send![nsarray_cls, arrayWithObject: num]
                }
                _ => msg_send![nsarray_cls, array],
            }
        } else {
            msg_send![nsarray_cls, array]
        };

        let alloc: *mut AnyObject = msg_send![tap_cls, alloc];
        let tap: *mut AnyObject = msg_send![alloc, initStereoGlobalTapButExcludeProcesses: exclude];
        if tap.is_null() {
            return Err(BridgeError::other("CATapDescription 初始化返回 nil"));
        }
        let name: *mut AnyObject =
            msg_send![nsstring_cls, stringWithUTF8String: c"media-bridge-system-tap".as_ptr()];
        let _: () = msg_send![tap, setName: name];
        let _: () = msg_send![tap, setPrivate: true];
        // CATapUnmuted：只监听，不影响系统输出（mute 掉就等于把用户的音乐静音了）
        let _: () = msg_send![tap, setMuteBehavior: 0isize];
        let _: *mut AnyObject = msg_send![tap, retain];
        Ok(tap)
    })
}

/// 读 tap 的流格式。
fn read_tap_format(tap_id: u32) -> Result<Asbd> {
    let addr = AudioObjectPropertyAddress {
        selector: TAP_PROP_FORMAT,
        scope: SCOPE_GLOBAL,
        element: 0,
    };
    let mut asbd = Asbd::default();
    let mut size = std::mem::size_of::<Asbd>() as u32;
    let st = unsafe {
        AudioObjectGetPropertyData(
            tap_id,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            &mut asbd as *mut Asbd as *mut c_void,
        )
    };
    if st != 0 {
        return Err(BridgeError::other(format!("AudioObjectGetPropertyData(tfmt) = {st}")));
    }
    Ok(asbd)
}

/// 读 tap 的 UID（聚合设备的 `kAudioSubTapUIDKey` 要用它）。
fn read_tap_uid(tap_id: u32) -> Result<String> {
    let addr = AudioObjectPropertyAddress {
        selector: TAP_PROP_UID,
        scope: SCOPE_GLOBAL,
        element: 0,
    };
    let mut uid_ref: *const c_void = std::ptr::null();
    let mut size = std::mem::size_of::<*const c_void>() as u32;
    let st = unsafe {
        AudioObjectGetPropertyData(
            tap_id,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            &mut uid_ref as *mut *const c_void as *mut c_void,
        )
    };
    if st != 0 || uid_ref.is_null() {
        return Err(BridgeError::other(format!("读 kAudioTapPropertyUID 失败（OSStatus {st}）")));
    }
    let mut buf = [0i8; 256];
    let ok = unsafe { CFStringGetCString(uid_ref, buf.as_mut_ptr(), buf.len() as isize, CF_UTF8) };
    unsafe { CFRelease(uid_ref) };
    if !ok {
        return Err(BridgeError::other("tap UID 不是可解码的 CFString"));
    }
    Ok(unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned())
}

/// 造聚合设备描述字典。
///
/// 键名都是 `#define` 出来的 C 字符串（`AudioHardware.h`），所以直接用字面量，
/// 不需要 dlsym：`name` / `uid` / `private` / `taps` / `drift`。
fn build_aggregate_description(tap_uid: &str, tap_id: u32) -> Result<CfOwned> {
    unsafe {
        let mk = |s: &str| -> *mut c_void {
            let cs = CString::new(s).unwrap_or_default();
            CFStringCreateWithCString(std::ptr::null(), cs.as_ptr(), CF_UTF8)
        };
        let dict_new = || {
            CFDictionaryCreateMutable(
                std::ptr::null(),
                4,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        };

        let k_name = mk("name");
        let k_uid = mk("uid");
        let k_private = mk("private");
        let k_taps = mk("taps");
        let k_drift = mk("drift");

        // 子 tap 项：{ uid = <tap UID>, drift = true }
        let sub = dict_new();
        CFDictionarySetValue(sub, k_uid, mk(tap_uid));
        CFDictionarySetValue(sub, k_drift, kCFBooleanTrue);
        let taps = CFArrayCreate(std::ptr::null(), &(sub as *const c_void), 1, &kCFTypeArrayCallBacks);

        let agg = dict_new();
        CFDictionarySetValue(agg, k_name, mk("media-bridge-tap-agg"));
        CFDictionarySetValue(
            agg,
            k_uid,
            mk(&format!("media-bridge-agg-{tap_id}-{}", crate::util::uuid_v4_ish())),
        );
        CFDictionarySetValue(agg, k_private, kCFBooleanTrue);
        CFDictionarySetValue(agg, k_taps, taps);
        Ok(CfOwned(agg))
    }
}

/// 持有 CF 对象并负责释放。
struct CfOwned(*mut c_void);

// ══════════════════════════════════════════════════════════════════════════════
// 实时回调：混单声道 + 写环
// ══════════════════════════════════════════════════════════════════════════════

/// 把一帧 AudioBufferList 混成单声道并写入环形缓冲。
///
/// **实时线程**：只做算术与原子存储 —— 不分配、不加锁、不 logging。
/// 同时处理交错（一个 buffer 多通道）与非交错（多个 buffer 各一通道）两种布局。
unsafe fn mix_and_push(ctx: &TapContext, in_data: *const c_void, non_interleaved: bool) {
    let number_buffers = unsafe { std::ptr::read_unaligned(in_data as *const u32) } as usize;
    if number_buffers == 0 {
        return;
    }
    let buffers = unsafe {
        std::slice::from_raw_parts(
            (in_data as *const u8).add(ABL_BUFFERS_OFFSET) as *const AudioBuffer,
            number_buffers,
        )
    };
    let cap = SCRATCH_BYTES / std::mem::size_of::<f32>();
    let scratch = unsafe { std::slice::from_raw_parts_mut(ctx.scratch, cap) };
    let rate = ctx.rate;

    // 记下回调看到的布局（只做原子存储；**实时线程里不能格式化/打印/分配**）
    ctx.shared.record_layout(
        number_buffers as u32,
        buffers.first().map(|b| b.number_channels).unwrap_or(0),
        buffers.first().map(|b| b.data_byte_size).unwrap_or(0),
        non_interleaved,
    );

    if non_interleaved || number_buffers > 1 {
        // 非交错：每个 buffer 一条通道，逐样本取平均
        if buffers.iter().any(|b| b.data.is_null()) {
            return;
        }
        let frames = buffers
            .iter()
            .map(|b| b.data_byte_size as usize / std::mem::size_of::<f32>())
            .min()
            .unwrap_or(0)
            .min(cap);
        if frames == 0 {
            return;
        }
        for (i, slot) in scratch.iter_mut().enumerate().take(frames) {
            let mut sum = 0.0f32;
            for b in buffers {
                sum += unsafe { *(b.data as *const f32).add(i) };
            }
            *slot = sum / buffers.len() as f32;
        }
        ctx.shared.push(&scratch[..frames], rate);
        return;
    }

    // 交错：一个 buffer，多通道交织
    let b = &buffers[0];
    if b.data.is_null() {
        return;
    }
    let ch = b.number_channels.max(1) as usize;
    let total = b.data_byte_size as usize / std::mem::size_of::<f32>();
    let ptr = b.data as *const f32;
    if ch == 1 {
        // 单声道：直接推给环形缓冲，连 scratch 都不用
        let n = total.min(cap);
        if n == 0 {
            return;
        }
        let slice = unsafe { std::slice::from_raw_parts(ptr, n) };
        ctx.shared.push(slice, rate);
        return;
    }
    let frames = (total / ch).min(cap);
    for (i, slot) in scratch.iter_mut().enumerate().take(frames) {
        let mut sum = 0.0f32;
        for c in 0..ch {
            sum += unsafe { *ptr.add(i * ch + c) };
        }
        *slot = sum / ch as f32;
    }
    if frames > 0 {
        ctx.shared.push(&scratch[..frames], rate);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asbd_layout_matches_coreaudio() {
        // 这些数字是用真头文件量出来的（`sizeof` 直接问编译器），不是猜的：
        //   sizeof(AudioStreamBasicDescription) = 40（8 + 8×u32），align = 8
        //   sizeof(AudioBuffer) = 16，sizeof(AudioBufferList) = 24，mBuffers 偏移 = 8
        assert_eq!(std::mem::size_of::<Asbd>(), 40);
        assert_eq!(std::mem::align_of::<Asbd>(), 8);
    }

    #[test]
    fn audio_buffer_list_offsets_are_abi_correct() {
        assert_eq!(std::mem::size_of::<AudioBuffer>(), 16);
        assert_eq!(std::mem::align_of::<AudioBuffer>(), 8);
        assert_eq!(ABL_BUFFERS_OFFSET, 8);
    }

    #[test]
    fn fourcc_values_match_headers() {
        assert_eq!(SCOPE_GLOBAL, 0x676c_6f62); // 'glob'
        assert_eq!(TAP_PROP_UID, 0x7475_6964); // 'tuid'
        assert_eq!(TAP_PROP_FORMAT, 0x7466_6d74); // 'tfmt'
    }

    #[test]
    fn non_apple_flag_matches_header() {
        assert_eq!(FORMAT_FLAG_IS_FLOAT, 1);
        assert_eq!(FORMAT_FLAG_IS_NON_INTERLEAVED, 32);
    }
}
