//! Windows 系统音频采集：**WASAPI loopback**。
//!
//! WASAPI 的回环采集是「渲染端点反向读」：把默认播放设备以 `AUDCLNT_STREAMFLAGS_LOOPBACK`
//! 打开成 capture 流，就能拿到系统正在播放的声音 —— 不需要虚拟声卡、不需要「立体声混音」
//! （老实现要求用户自己去开立体声混音或装 VB-Cable，那是最劝退的一步）。
//!
//! 四条硬约束：
//!
//!   1. **必须有自己的 COM 公寓**：这里用一条专属线程，`CoInitializeEx(MTA)` 一次，
//!      采集循环全在这条线程上跑。
//!   2. **格式由系统决定**：回环流必须用 `GetMixFormat` 给的格式。混音格式绝大多数是
//!      32 位浮点（少数是 16 位 PCM），两种都支持；别的格式明确报不可用，不硬凑。
//!   3. **不能忙等**：按缓冲时长的一半左右轮询 `GetNextPacketSize`，让出 CPU。
//!   4. **设备切换要自己跟上**：loopback 流绑定的是「打开那一刻」的默认设备，
//!      用户切耳机/蓝牙后旧流**不会迁移**，只会继续在旧设备上出静音。采集线程每
//!      0.5s 对比一次默认端点 ID，发现变化就停旧流、在新设备上重开 —— 设备整个
//!      消失（蓝牙断连/拔线，流会报 `AUDCLNT_E_DEVICE_INVALIDATED`）走的也是同一条
//!      重开路径，设备回来或出现新默认设备时自动恢复。

use std::sync::Arc;
use std::time::Duration;

use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, IAudioCaptureClient,
    IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, eConsole, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
};

use super::CaptureShared;
use crate::error::{BridgeError, Result};
use crate::types::SourceState;

/// 回环缓冲时长（100ns 单位）：1 秒足够大，避免被音频引擎抖动打断。
const BUFFER_DURATION_HNS: i64 = 10_000_000;
/// 轮询间隔：约等于半个包的量级，够快也不烧 CPU。
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// 默认设备检查周期：每 `DEVICE_CHECK_POLLS` 次 10ms 轮询查一次默认端点（约 0.5s）。
/// 这是一次 `GetDefaultAudioEndpoint` + `GetId`，开销可忽略；切换后的频谱空窗 ≤ 0.5s。
const DEVICE_CHECK_POLLS: u32 = 50;
/// 运行中重开流失败的退避间隔（设备整个移除后等它回来 / 等新默认设备出现）。
const REOPEN_RETRY: Duration = Duration::from_secs(1);

/// 启动采集。初始化在专属线程里完成，失败会通过通道回传。
pub(crate) fn start(shared: Arc<CaptureShared>) -> Result<()> {
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<()>>();
    let shared_thread = shared.clone();
    let handle = std::thread::Builder::new()
        .name("media-bridge-wasapi".to_string())
        .spawn(move || {
            // 线程内已尽力上报失败；这里只保证不 panic 逃出线程
            let _ = run(shared_thread, &init_tx);
        })
        .map_err(|e| BridgeError::other(format!("无法创建 WASAPI 线程：{e}")))?;

    // 收尾：等采集线程自己看到停止标志退出（每 10ms~50ms 检查一次），
    // 这样进程退出时不会留下半死不活的音频线程。
    shared.on_stop(Box::new(move || {
        let _ = handle.join();
    }));

    match init_rx.recv_timeout(Duration::from_secs(8)) {
        Ok(r) => r,
        Err(_) => {
            let msg = "WASAPI 初始化超时（8s）".to_string();
            shared.set_state(SourceState::Unavailable, msg.clone());
            Err(BridgeError::other(msg))
        }
    }
}

/// 采集循环的结束原因。
#[derive(PartialEq)]
enum StreamOutcome {
    /// 收到停止标志，正常退出
    Stopped,
    /// 默认设备变了 / 旧设备失效，旧流已停，需要重开
    Reopen,
}

/// 线程主体：COM 初始化 → 「打开流 → 采集 → 按需重开」循环 → 收尾。
fn run(shared: Arc<CaptureShared>, init_tx: &std::sync::mpsc::Sender<Result<()>>) -> Result<()> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        // RPC_E_CHANGED_MODE 表示这条线程已经在别的公寓里，仍然可以继续用
        if hr.is_err() && hr != windows::Win32::Foundation::RPC_E_CHANGED_MODE {
            let msg = format!("CoInitializeEx 失败：{hr:?}");
            shared.set_state(SourceState::Unavailable, msg.clone());
            let _ = init_tx.send(Err(BridgeError::other(msg)));
            return Ok(());
        }
    }

    // 枚举器整个会话都要用：设备切换检测每 0.5s 要查一次默认端点
    let enumerator: IMMDeviceEnumerator = match unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) } {
        Ok(e) => e,
        Err(e) => {
            let msg = format!("创建 IMMDeviceEnumerator 失败：{e}");
            shared.set_state(SourceState::Unavailable, msg.clone());
            let _ = init_tx.send(Err(BridgeError::unavailable(msg)));
            return Ok(());
        }
    };

    // 重开循环。只有「第一次打开」的失败会让 start() 拿到 Err（与老行为一致）；
    // 运行中的失败一律退避重试 —— 设备移除是常态（蓝牙断连、拔耳机），
    // 等设备回来或出现新默认设备就自动恢复。
    let mut first_init = true;
    while !shared.is_stopping() {
        let opened = unsafe { open_loopback(&enumerator) };
        let (client, capture, format, device_id) = match opened {
            Ok(v) => v,
            Err(e) => {
                if first_init {
                    // 启动就失败（多半是没有可用的播放设备）：报不可用并结束线程
                    shared.set_state(SourceState::Unavailable, e.to_string());
                    let _ = init_tx.send(Err(e));
                    return Ok(());
                }
                shared.set_state(
                    SourceState::Unavailable,
                    format!("打开回环流失败（设备可能已移除），稍后重试：{e}"),
                );
                sleep_stoppable(&shared, REOPEN_RETRY);
                continue;
            }
        };
        shared.set_state(
            SourceState::Running,
            format!(
                "WASAPI loopback（{}Hz / {}ch / {}）",
                format.sample_rate,
                format.channels,
                format.kind_label()
            ),
        );
        if first_init {
            first_init = false;
            let _ = init_tx.send(Ok(()));
        }

        let outcome = capture_loop(&shared, &capture, &format, &enumerator, &device_id);
        unsafe { stop(&client) };
        if outcome == StreamOutcome::Stopped {
            break;
        }
        // Reopen：回到循环顶部在新默认设备上重开（切换是常态，不打状态）
    }

    unsafe { windows::Win32::System::Com::CoUninitialize() };
    shared.set_state(SourceState::Idle, "已停止");
    Ok(())
}

/// 采集循环：拉包 → 混单声道 → 写环；每 0.5s 对比一次默认设备 ID。
fn capture_loop(
    shared: &Arc<CaptureShared>,
    capture: &IAudioCaptureClient,
    format: &MixFormat,
    enumerator: &IMMDeviceEnumerator,
    device_id: &str,
) -> StreamOutcome {
    let mut scratch: Vec<f32> = Vec::with_capacity(8192);
    let mut polls: u32 = 0;
    loop {
        if shared.is_stopping() {
            return StreamOutcome::Stopped;
        }
        std::thread::sleep(POLL_INTERVAL);
        polls += 1;
        if polls >= DEVICE_CHECK_POLLS {
            polls = 0;
            // 默认设备变了（切耳机/蓝牙）：旧流绑在旧设备上只会出静音，退出去重开。
            // 查询失败当作「没变」处理 —— 短暂的 COM 失败不该把采集干掉，下轮再查。
            if let Some(id) = unsafe { default_device_id(enumerator) } {
                if id != device_id {
                    return StreamOutcome::Reopen;
                }
            }
        }
        loop {
            let frames = match unsafe { capture.GetNextPacketSize() } {
                Ok(n) => n,
                Err(e) => {
                    shared.set_state(
                        SourceState::Error,
                        format!("GetNextPacketSize 失败（多半是设备失效，将重开流）：{e}"),
                    );
                    return StreamOutcome::Reopen;
                }
            };
            if frames == 0 {
                break;
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut num_frames: u32 = 0;
            let mut flags: u32 = 0;
            let ok = unsafe { capture.GetBuffer(&mut data, &mut num_frames, &mut flags, None, None) };
            if let Err(e) = ok {
                shared.set_state(
                    SourceState::Error,
                    format!("GetBuffer 失败（多半是设备失效，将重开流）：{e}"),
                );
                return StreamOutcome::Reopen;
            }
            if num_frames > 0 && !data.is_null() {
                // 排查用：WASAPI loopback 给的是一个交错缓冲
                shared.record_layout(
                    1,
                    format.channels as u32,
                    num_frames * format.block_align as u32,
                    false,
                );
                if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 {
                    // 静音包：按 0 处理（必须走一遍，否则环形缓冲的写指针不前进，
                    // 频谱泵会以为采集停了）
                    let n = num_frames as usize;
                    scratch.clear();
                    scratch.resize(n, 0.0);
                    shared.push(&scratch, format.sample_rate);
                } else {
                    let bytes = unsafe {
                        std::slice::from_raw_parts(data, num_frames as usize * format.block_align as usize)
                    };
                    format.decode_mono(bytes, &mut scratch);
                    shared.push(&scratch, format.sample_rate);
                }
            }
            unsafe {
                let _ = capture.ReleaseBuffer(num_frames);
            }
        }
    }
}

/// 可中断的睡眠：50ms 一档检查停止标志，保证 stop() 不用等满整个退避间隔。
fn sleep_stoppable(shared: &CaptureShared, total: Duration) {
    let step = Duration::from_millis(50);
    let mut waited = Duration::ZERO;
    while waited < total && !shared.is_stopping() {
        std::thread::sleep(step.min(total - waited));
        waited += step;
    }
}

/// 当前默认播放端点的设备 ID（用于检测设备切换）。
unsafe fn default_device_id(enumerator: &IMMDeviceEnumerator) -> Option<String> {
    unsafe {
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        let id = device.GetId().ok()?;
        let s = id.to_string().ok();
        CoTaskMemFree(Some(id.0 as *const _));
        s
    }
}

unsafe fn stop(client: &IAudioClient) {
    unsafe {
        let _ = client.Stop();
    }
}

/// 采样格式（只支持回环流真实会出现的两种）。
#[derive(Debug, Clone, Copy, PartialEq)]
enum SampleKind {
    Float32,
    Pcm16,
}

impl SampleKind {
    fn kind_label(self) -> &'static str {
        match self {
            SampleKind::Float32 => "float32",
            SampleKind::Pcm16 => "pcm16",
        }
    }
}

struct MixFormat {
    sample_rate: u32,
    channels: u16,
    block_align: u16,
    kind: SampleKind,
}

impl MixFormat {
    fn kind_label(&self) -> &'static str {
        self.kind.kind_label()
    }

    /// 一帧（多通道交错）→ 单声道 f32。
    fn decode_mono(&self, bytes: &[u8], out: &mut Vec<f32>) {
        let ch = self.channels.max(1) as usize;
        out.clear();
        match self.kind {
            SampleKind::Float32 => {
                let samples = bytes.len() / 4;
                let frames = samples / ch;
                out.reserve(frames);
                for f in 0..frames {
                    let mut sum = 0.0f32;
                    for c in 0..ch {
                        let i = (f * ch + c) * 4;
                        let v = f32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
                        sum += v;
                    }
                    out.push(sum / ch as f32);
                }
            }
            SampleKind::Pcm16 => {
                let samples = bytes.len() / 2;
                let frames = samples / ch;
                out.reserve(frames);
                for f in 0..frames {
                    let mut sum = 0.0f32;
                    for c in 0..ch {
                        let i = (f * ch + c) * 2;
                        let v = i16::from_le_bytes([bytes[i], bytes[i + 1]]);
                        sum += v as f32 / 32768.0;
                    }
                    out.push(sum / ch as f32);
                }
            }
        }
    }
}

/// 打开默认渲染端点的回环捕获流，返回流三件套 + 设备 ID（设备切换检测的基线）。
unsafe fn open_loopback(
    enumerator: &IMMDeviceEnumerator,
) -> Result<(IAudioClient, IAudioCaptureClient, MixFormat, String)> {
    unsafe {
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| BridgeError::unavailable(format!("取默认播放设备失败：{e}")))?;
        let id_pw = device
            .GetId()
            .map_err(|e| BridgeError::unavailable(format!("取设备 ID 失败：{e}")))?;
        let id_res = id_pw.to_string();
        CoTaskMemFree(Some(id_pw.0 as *const _));
        let device_id = id_res.map_err(|_| BridgeError::other("设备 ID 不是合法 UTF-16"))?;

        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| BridgeError::unavailable(format!("激活 IAudioClient 失败：{e}")))?;

        // 回环流必须用混音格式，否则 Initialize 会失败
        let mix_ptr = client
            .GetMixFormat()
            .map_err(|e| BridgeError::unavailable(format!("GetMixFormat 失败：{e}")))?;
        let format = parse_format(mix_ptr)?;

        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                BUFFER_DURATION_HNS,
                0,
                mix_ptr,
                None,
            )
            .map_err(|e| {
                CoTaskMemFree(Some(mix_ptr as *const _));
                BridgeError::unavailable(format!("IAudioClient::Initialize(loopback) 失败：{e}"))
            })?;
        CoTaskMemFree(Some(mix_ptr as *const _));

        let capture: IAudioCaptureClient = client
            .GetService()
            .map_err(|e| BridgeError::other(format!("取 IAudioCaptureClient 失败：{e}")))?;
        client
            .Start()
            .map_err(|e| BridgeError::other(format!("启动回环流失败：{e}")))?;
        Ok((client, capture, format, device_id))
    }
}

/// 解析 `WAVEFORMATEX`（含 EXTENSIBLE 的 SubFormat）。
unsafe fn parse_format(ptr: *const WAVEFORMATEX) -> Result<MixFormat> {
    if ptr.is_null() {
        return Err(BridgeError::other("GetMixFormat 返回空指针"));
    }
    // WAVEFORMATEX 是 `pack(1)`：字段必须**先拷到局部变量**再使用，
    // 否则对 packed 字段取引用是未定义行为（编译器会直接拒绝）。
    let fmt = unsafe { *ptr };
    let tag = fmt.wFormatTag;
    let channels = fmt.nChannels;
    let sample_rate = fmt.nSamplesPerSec;
    let block_align = fmt.nBlockAlign;
    let bits_per_sample = fmt.wBitsPerSample;
    let cb_size = fmt.cbSize;
    // WAVE_FORMAT_EXTENSIBLE(0xFFFE)：真正的格式在 SubFormat 的前 4 字节
    // （等于 GUID 的 Data1，取值就是 WAVE_FORMAT_* 标签）—— 这样不用引内核流媒体那边
    // 的 GUID 常量表。
    let effective_tag = if tag == 0xFFFE && cb_size >= 22 {
        let ext = unsafe { &*(ptr as *const WAVEFORMATEXTENSIBLE_VIEW) };
        (ext.sub_format_first_u32 & 0xFFFF) as u16
    } else {
        tag
    };
    let kind = match effective_tag {
        3 => SampleKind::Float32, // WAVE_FORMAT_IEEE_FLOAT
        1 => SampleKind::Pcm16,   // WAVE_FORMAT_PCM
        other => {
            return Err(BridgeError::unsupported(format!(
                "混音格式标签 {other} 不在支持范围（只支持 float32 / pcm16）"
            )));
        }
    };
    if kind == SampleKind::Pcm16 && bits_per_sample != 16 {
        return Err(BridgeError::unsupported(format!(
            "PCM 位深 {bits_per_sample} 不在支持范围（只支持 16 位）"
        )));
    }
    if kind == SampleKind::Float32 && bits_per_sample != 32 {
        return Err(BridgeError::unsupported(format!(
            "浮点位深 {bits_per_sample} 不在支持范围（只支持 32 位）"
        )));
    }
    Ok(MixFormat {
        sample_rate,
        channels,
        block_align,
        kind,
    })
}

/// `WAVEFORMATEXTENSIBLE` 的最小视图（只关心 SubFormat 的前 4 字节）。
///
/// `WAVEFORMATEX`(18 字节，packed 1) 后面紧跟 union `Samples`（2 字节）与 `dwChannelMask`(4)，
/// 因此 SubFormat 的偏移是 18 + 2 + 4 = 24。
#[repr(C, packed(1))]
struct WAVEFORMATEXTENSIBLE_VIEW {
    format: WAVEFORMATEX,
    samples: u16,
    channel_mask: u32,
    sub_format_first_u32: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float32_stereo_decodes_to_mono() {
        let f = MixFormat {
            sample_rate: 48_000,
            channels: 2,
            block_align: 8,
            kind: SampleKind::Float32,
        };
        let mut bytes = Vec::new();
        for (l, r) in [(1.0f32, 0.0f32), (-1.0, 1.0)] {
            bytes.extend_from_slice(&l.to_le_bytes());
            bytes.extend_from_slice(&r.to_le_bytes());
        }
        let mut out = Vec::new();
        f.decode_mono(&bytes, &mut out);
        assert_eq!(out, vec![0.5, 0.0]);
    }

    #[test]
    fn pcm16_decodes_and_scales() {
        let f = MixFormat {
            sample_rate: 44_100,
            channels: 1,
            block_align: 2,
            kind: SampleKind::Pcm16,
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&16384i16.to_le_bytes());
        bytes.extend_from_slice(&(-16384i16).to_le_bytes());
        let mut out = Vec::new();
        f.decode_mono(&bytes, &mut out);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 0.5).abs() < 1e-4);
        assert!((out[1] + 0.5).abs() < 1e-4);
    }

    #[test]
    fn extensible_subformat_offset_matches_win32_layout() {
        // WAVEFORMATEX = 18 字节（packed），+ Samples(2) + dwChannelMask(4) → SubFormat 在 24
        assert_eq!(std::mem::size_of::<WAVEFORMATEX>(), 18);
        assert_eq!(
            std::mem::offset_of!(WAVEFORMATEXTENSIBLE_VIEW, sub_format_first_u32),
            24
        );
    }
}
