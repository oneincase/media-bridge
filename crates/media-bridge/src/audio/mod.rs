//! 系统音频采集（第 1 期的「音频」部分）。
//!
//! 三平台各有一条采集路径，但**下游是同一套**：
//!
//! ```text
//!  采集回调（实时线程）           频谱泵（普通线程，20fps）
//!  ───────────────────          ─────────────────────────
//!  macOS  CoreAudio 进程 tap ─┐
//!  Windows WASAPI loopback   ─┼─→ 单声道 f32 → 环形缓冲 ─┬─→ 降采样 16k → 2048 点 FFT → 64 段
//!  Linux  parec/pw-record    ─┘                        └─→ （可选）原始 PCM 广播
//! ```
//!
//! ## 为什么把「降采样到 16kHz」做成统一约定
//!
//! 频谱本身对采样率不敏感，但 **PCM 输出**很敏感：48kHz 立体声 f32 是 16kHz 单声道的
//! 6 倍带宽，而中间件的主要消费者（壁纸/可视化/语音分析）都不需要那么高。统一到
//! 16kHz 单声道之后，「同一段音乐在三平台得到同一张图」这件事才成立。
//! 降采样用**分箱平均**（自带抗混叠低通），不是粗暴抽点 —— 抽点会把 8kHz 以上的
//! 能量折叠回可听频段，频谱上表现为「高频莫名亮起来」。
//!
//! ## 实时线程里的约束
//!
//! 采集回调在实时线程上跑，**不能分配内存、不能加锁、不能 logging**。所以：
//! 环形缓冲用 `Vec<AtomicU32>`（每个槽位一个原子，无锁无 UB），写入只是原子 `store`；
//! 读取端（频谱泵）容忍读到「半新半旧」的一帧 —— 可视化上完全看不出，换来的是
//! 采集回调永不卡顿。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;

use crate::error::Result;
use crate::spectrum::SpectrumAnalyzer;
use crate::types::{SourceState, SourceStatus, SpectrumFrame, SPECTRUM_BANDS};
use crate::util::now_ms;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

/// 对外统一的输出采样率（单声道 f32）。
pub const OUTPUT_RATE: u32 = 16_000;
/// 频谱分析窗口（点数）。16kHz 下 = 128ms，频率分辨率 7.8Hz。
pub const ANALYSIS_WINDOW: usize = 2048;
/// 环形缓冲容量（样本数）。按最高 48kHz 算约 0.68s，远大于分析窗口。
pub const RING_CAPACITY: usize = 1 << 15;

/// 频谱与 PCM 共用同一份降采样窗口（见音频泵里的 `ds_window` 注释），所以分析器的
/// 频率口径必须与这里的输出率一致 —— 曾经两边不一致（分析器按 48 kHz 标称、实际
/// 喂 16 kHz）：所有频段低 3 倍，第 0 段落进 15.6~23 Hz 的不可听区，按第 0 段取值的
/// 音谱组件（2902406982 三角漏斗填充）恒静止。改动任一常量时这条断言会拦下来。
const _: () = assert!(
    crate::spectrum::ANALYSIS_RATE == OUTPUT_RATE,
    "spectrum::ANALYSIS_RATE 必须等于 audio::OUTPUT_RATE（频谱分析的就是降采样后的窗口）"
);

/// 采集配置。
#[derive(Debug, Clone)]
pub struct AudioConfig {
    /// 是否启用采集（默认开；关掉则完全不碰系统音频，也不会触发权限申请）
    pub enabled: bool,
    /// 频谱出帧频率
    pub fps: u32,
    /// 是否对外提供原始 PCM（除了频谱之外，把音频本身也交出去）
    pub pcm: bool,
    /// 每块 PCM 的样本数（16kHz 下 8192 ≈ 0.5s）
    pub pcm_chunk: usize,
    /// macOS：tap 排除本进程自身（避免把中间件自己的输出也采进来形成反馈）
    pub exclude_self: bool,
    /// Linux：显式指定采集设备/源（默认自动探测默认输出的 monitor）
    pub device: Option<String>,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            fps: 20,
            pcm: false,
            pcm_chunk: 8192,
            exclude_self: true,
            device: None,
        }
    }
}

/// 一块 PCM（16kHz 单声道 f32）。
#[derive(Debug, Clone)]
pub struct PcmChunk {
    pub samples: Arc<[f32]>,
    pub sample_rate: u32,
    pub ts_ms: u64,
}

// ══════════════════════════════════════════════════════════════════════════════
// 环形缓冲（实时线程安全）
// ══════════════════════════════════════════════════════════════════════════════

/// 单写多读的样本环。
///
/// 每个槽位是一个 `AtomicU32`（存 f32 的位模式），所以：
///   - 写入 = 一次原子 `store`，无锁、无分配、不会阻塞实时线程；
///   - 读取 = 若干次原子 `load`，绝不产生 UB（哪怕写者同时在写）；
///   - 代价 = 读到的可能是「半新半旧」的一帧，对频谱可视化无影响。
pub struct SampleRing {
    slots: Box<[AtomicU32]>,
    /// 单调递增的写入位置（取模后才是槽位下标，可用来算「总共写了多少」）
    write: AtomicUsize,
}

impl SampleRing {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(64).next_power_of_two();
        let mut slots = Vec::with_capacity(cap);
        for _ in 0..cap {
            slots.push(AtomicU32::new(0));
        }
        Self {
            slots: slots.into_boxed_slice(),
            write: AtomicUsize::new(0),
        }
    }

    fn mask(&self) -> usize {
        self.slots.len() - 1
    }

    /// 写入一段单声道样本（**实时线程调用**：只做原子存储）。
    pub fn push(&self, samples: &[f32]) {
        let mask = self.mask();
        let mut w = self.write.load(Ordering::Relaxed);
        for &s in samples {
            self.slots[w & mask].store(s.to_bits(), Ordering::Relaxed);
            w = w.wrapping_add(1);
        }
        self.write.store(w, Ordering::Release);
    }

    /// 已写入的总样本数（用于判断有没有数据）。
    pub fn total_written(&self) -> usize {
        self.write.load(Ordering::Acquire)
    }

    /// 取最近 `n` 个样本（不足时用 0 补齐，按时间顺序）。
    pub fn tail(&self, n: usize, out: &mut Vec<f32>) {
        out.clear();
        let total = self.total_written();
        if total == 0 || n == 0 {
            out.resize(n, 0.0);
            return;
        }
        let mask = self.mask();
        let start = total.saturating_sub(n);
        // 还没写满 n 个：前面补零（保持「末尾是最新」的语义）
        let pad = n.saturating_sub(total);
        out.resize(pad, 0.0);
        for i in start..total {
            let bits = self.slots[i & mask].load(Ordering::Relaxed);
            out.push(f32::from_bits(bits));
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// 降采样（任意采样率 → 16kHz 单声道，分箱平均自带抗混叠）
// ══════════════════════════════════════════════════════════════════════════════

/// 分箱平均降采样器。
///
/// 输入任意采样率，输出 16kHz；每个输出样本 = 对应输入区间内样本的平均值。
/// 平均本身就是一个矩形窗低通（对 48k→16k 而言，在 8kHz 附近衰减有限但足以压住
/// 最刺耳的折叠分量），比抽点干净得多。
pub struct Downsampler {
    in_rate: u32,
    out_rate: u32,
    /// 当前输出样本已累积的和与计数
    acc: f64,
    acc_n: u32,
    /// 按输入样本计的「下一个输出边界的相位」（浮点，支持 44.1k 这类非整数比）
    phase: f64,
}

impl Downsampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            in_rate: in_rate.max(1),
            out_rate: out_rate.max(1),
            acc: 0.0,
            acc_n: 0,
            phase: 0.0,
        }
    }

    /// 需要多少输入样本才产生一个输出样本（浮点）。
    fn step(&self) -> f64 {
        self.in_rate as f64 / self.out_rate as f64
    }

    /// 输入一段，把产生的输出样本追加到 `out`。
    ///
    /// 若输入采样率 <= 输出采样率（例如设备本来就是 16k），则原样直通。
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.in_rate <= self.out_rate {
            out.extend_from_slice(input);
            return;
        }
        let step = self.step();
        for &s in input {
            self.acc += s as f64;
            self.acc_n += 1;
            self.phase += 1.0;
            if self.phase >= step {
                self.phase -= step;
                let v = if self.acc_n > 0 {
                    (self.acc / self.acc_n as f64) as f32
                } else {
                    0.0
                };
                out.push(v);
                self.acc = 0.0;
                self.acc_n = 0;
            }
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// 采集共享状态
// ══════════════════════════════════════════════════════════════════════════════

/// 采集回调看到的缓冲区布局（**实时线程写入、普通线程读取**，所以全是原子量）。
///
/// 排查「采到的数据不对」时，第一件要知道的事就是「回调到底给了几个 buffer、几个通道、
/// 多少字节」。这些数字必须在回调里记，但**绝不能在回调里格式化/打印** ——
/// 实时线程不允许分配内存、不允许加锁。
#[derive(Debug, Clone, Copy, Default)]
pub struct LayoutSnapshot {
    pub callbacks: u64,
    pub buffers: u32,
    pub channels: u32,
    pub bytes: u32,
    pub non_interleaved: bool,
}

#[derive(Debug, Default)]
pub(crate) struct LayoutInfo {
    callbacks: std::sync::atomic::AtomicU64,
    buffers: AtomicU32,
    channels: AtomicU32,
    bytes: AtomicU32,
    non_interleaved: AtomicBool,
}

/// 采集后端与频谱泵之间的共享状态。
pub(crate) struct CaptureShared {
    ring: SampleRing,
    /// 设备实际采样率（首帧确定）
    input_rate: AtomicU32,
    status: RwLock<SourceStatus>,
    stop: AtomicBool,
    /// 后端注册的收尾动作（销毁 tap / 杀子进程）—— `stop()` 时执行一次
    cleanup: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// 回调布局快照（仅排查用）
    layout: LayoutInfo,
    pub(crate) config: AudioConfig,
}

impl CaptureShared {
    fn new(config: AudioConfig) -> Self {
        Self {
            ring: SampleRing::new(RING_CAPACITY),
            input_rate: AtomicU32::new(0),
            status: RwLock::new(SourceStatus::new(
                "audio",
                SourceState::Idle,
                "尚未启动（首次被订阅时才启动，避免平白申请音频权限）",
            )),
            stop: AtomicBool::new(false),
            cleanup: Mutex::new(None),
            layout: LayoutInfo::default(),
            config,
        }
    }

    /// 记录一次回调看到的缓冲区布局（**实时线程调用**：只做原子存储）。
    pub(crate) fn record_layout(&self, buffers: u32, channels: u32, bytes: u32, non_interleaved: bool) {
        self.layout.callbacks.fetch_add(1, Ordering::Relaxed);
        self.layout.buffers.store(buffers, Ordering::Relaxed);
        self.layout.channels.store(channels, Ordering::Relaxed);
        self.layout.bytes.store(bytes, Ordering::Relaxed);
        self.layout.non_interleaved.store(non_interleaved, Ordering::Relaxed);
    }

    /// 读回调布局快照。
    pub(crate) fn layout(&self) -> LayoutSnapshot {
        LayoutSnapshot {
            callbacks: self.layout.callbacks.load(Ordering::Relaxed),
            buffers: self.layout.buffers.load(Ordering::Relaxed),
            channels: self.layout.channels.load(Ordering::Relaxed),
            bytes: self.layout.bytes.load(Ordering::Relaxed),
            non_interleaved: self.layout.non_interleaved.load(Ordering::Relaxed),
        }
    }

    /// 后端注册收尾动作（只能注册一次）。
    pub(crate) fn on_stop(&self, cb: Box<dyn FnOnce() + Send>) {
        if let Ok(mut g) = self.cleanup.lock() {
            *g = Some(cb);
        }
    }

    pub(crate) fn run_cleanup(&self) {
        let cb = self.cleanup.lock().ok().and_then(|mut g| g.take());
        if let Some(cb) = cb {
            cb();
        }
    }

    /// 采集回调入口（**实时线程**）：写入环形缓冲。
    pub(crate) fn push(&self, mono: &[f32], rate: u32) {
        if rate > 0 {
            self.input_rate.store(rate, Ordering::Relaxed);
        }
        self.ring.push(mono);
    }

    pub(crate) fn set_state(&self, state: SourceState, hint: impl Into<String>) {
        if let Ok(mut g) = self.status.write() {
            *g = SourceStatus::new("audio", state, hint);
        }
    }

    pub(crate) fn is_stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// 采集源是否真的在出数据。
    ///
    /// Linux 用它识别「命令起来了但没连上采集源」（那边最容易静默采错源）；
    /// macOS/Windows 的采集后端失败时会明确报错，所以只有 Linux 会读它。
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn has_data(&self) -> bool {
        self.ring.total_written() > 0
    }

    fn input_rate(&self) -> u32 {
        self.input_rate.load(Ordering::Relaxed)
    }

    fn status(&self) -> SourceStatus {
        self.status
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| SourceStatus::new("audio", SourceState::Error, "状态锁被污染"))
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// 采集器
// ══════════════════════════════════════════════════════════════════════════════

/// 系统音频采集器。
///
/// 懒启动：构造时什么都不做，第一次 `start()` 才真正去碰系统音频（macOS 首次会触发
/// 「音频录制」权限弹窗 —— 没人用就不该弹）。
pub struct AudioCapture {
    shared: Arc<CaptureShared>,
    frame: RwLock<SpectrumFrame>,
    pcm_tx: Option<tokio::sync::broadcast::Sender<PcmChunk>>,
    pump: RwLock<Option<JoinHandle<()>>>,
    started: AtomicBool,
}

impl AudioCapture {
    pub fn new(config: AudioConfig) -> Arc<Self> {
        let pcm_tx = config
            .pcm
            .then(|| tokio::sync::broadcast::channel::<PcmChunk>(8).0);
        Arc::new(Self {
            frame: RwLock::new(SpectrumFrame::silent(now_ms(), OUTPUT_RATE)),
            shared: Arc::new(CaptureShared::new(config)),
            pcm_tx,
            pump: RwLock::new(None),
            started: AtomicBool::new(false),
        })
    }

    /// 最新一帧频谱（未启动/无数据时是全 0）。
    pub fn frame(&self) -> SpectrumFrame {
        self.frame.read().map(|g| g.clone()).unwrap_or_else(|_| SpectrumFrame::silent(now_ms(), OUTPUT_RATE))
    }

    pub fn status(&self) -> SourceStatus {
        self.shared.status()
    }

    /// 采集回调看到的缓冲区布局（排查用）。
    pub fn layout(&self) -> LayoutSnapshot {
        self.shared.layout()
    }

    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::Relaxed)
    }

    /// 订阅原始 PCM（需要 `AudioConfig::pcm = true`）。
    pub fn subscribe_pcm(&self) -> Option<tokio::sync::broadcast::Receiver<PcmChunk>> {
        self.pcm_tx.as_ref().map(|tx| tx.subscribe())
    }

    /// 启动采集（幂等）。失败不 panic：只把状态改成不可用，频谱继续出静音帧。
    pub fn start(self: &Arc<Self>) -> Result<()> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        if !self.shared.config.enabled {
            self.shared.set_state(SourceState::Unavailable, "采集已被配置关闭（--no-audio）");
            return Ok(());
        }
        // 支持 stop() 之后再次 start()（宿主的设置开关 / 看门狗重启）：
        // stop 会置位 stopping 并 join 掉旧泵，这里复位让新后端/新泵能跑
        self.shared.stop.store(false, Ordering::Relaxed);
        // 频谱泵先跑起来：即使后端启动失败，也有稳定的静音帧输出
        self.spawn_pump();
        #[cfg(target_os = "macos")]
        let started = macos::start(self.shared.clone());
        #[cfg(target_os = "windows")]
        let started = windows::start(self.shared.clone());
        #[cfg(target_os = "linux")]
        let started = linux::start(self.shared.clone());
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        let started: Result<()> = {
            self.shared
                .set_state(SourceState::Unavailable, "当前平台不支持系统音频采集");
            Ok(())
        };
        if let Err(e) = &started {
            self.shared
                .set_state(SourceState::Unavailable, format!("采集启动失败：{e}"));
        }
        started
    }

    /// 停止采集并回收系统资源（macOS 会销毁 tap 与私有聚合设备）。
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.run_cleanup();
        if let Some(h) = self.pump.write().ok().and_then(|mut g| g.take()) {
            let _ = h.join();
        }
        self.started.store(false, Ordering::SeqCst);
    }

    /// 起 20fps 的频谱泵：读环 → 降采样 → FFT → 出帧 →（可选）广播 PCM。
    fn spawn_pump(self: &Arc<Self>) {
        let me = Arc::clone(self);
        let fps = self.shared.config.fps.clamp(1, 120);
        let interval = std::time::Duration::from_millis((1000 / fps) as u64);
        let handle = std::thread::Builder::new()
            .name("media-bridge-spectrum".to_string())
            .spawn(move || {
                let mut analyzer = SpectrumAnalyzer::new();
                // 降采样后的滚动窗口（**频谱与 PCM 都走这一份**，保证三平台同一段音乐
                // 得到同一张图 —— 设备采样率是 44.1k 还是 48k 不该影响分箱位置）
                let mut ds_window: Vec<f32> = Vec::with_capacity(ANALYSIS_WINDOW * 2);
                let mut fresh: Vec<f32> = Vec::with_capacity(RING_CAPACITY / 2);
                let mut down: Option<Downsampler> = None;
                let mut down_out: Vec<f32> = Vec::with_capacity(RING_CAPACITY);
                let mut last_input_rate = 0u32;
                let mut last_total = 0usize;
                let mut idle_since = now_ms();
                while !me.shared.is_stopping() {
                    let total = me.shared.ring.total_written();
                    let input_rate = me.shared.input_rate();
                    if input_rate == 0 {
                        // 后端还没起来（或已失败）：维持静音帧
                        std::thread::sleep(interval);
                        continue;
                    }
                    if down.is_none() || input_rate != last_input_rate {
                        down = Some(Downsampler::new(input_rate, OUTPUT_RATE));
                        last_input_rate = input_rate;
                    }
                    let new_samples = total.saturating_sub(last_total);
                    last_total = total;
                    if new_samples == 0 {
                        // 没有新数据：连续静音超过 1s 就把频谱清零（别让最后一帧亮着不动）
                        if now_ms().saturating_sub(idle_since) > 1000 {
                            ds_window.clear();
                            let mut bands = [0u8; SPECTRUM_BANDS];
                            analyzer.analyze(&[], &mut bands);
                            if let Ok(mut g) = me.frame.write() {
                                *g = SpectrumFrame {
                                    bands: bands.to_vec(),
                                    peak: 0,
                                    rms: 0.0,
                                    sample_rate: OUTPUT_RATE,
                                    ts_ms: now_ms(),
                                };
                            }
                        }
                        std::thread::sleep(interval);
                        continue;
                    }
                    idle_since = now_ms();

                    // 只降采样「这一轮新增」的样本，追加进滚动窗口
                    down_out.clear();
                    {
                        let fresh_n = new_samples.min(RING_CAPACITY / 2);
                        me.shared.ring.tail(fresh_n, &mut fresh);
                        let downs = down.as_mut().expect("downsampler 已初始化");
                        downs.push(&fresh, &mut down_out);
                    }
                    ds_window.extend_from_slice(&down_out);
                    if ds_window.len() > ANALYSIS_WINDOW * 2 {
                        // 只留最近的两个窗口，别让滚动缓冲无限长
                        let drop = ds_window.len() - ANALYSIS_WINDOW;
                        ds_window.drain(..drop);
                    }

                    // 频谱 = 降采样后窗口的末尾（滑动窗口，总是「最新的一屏」）
                    let mut bands = [0u8; SPECTRUM_BANDS];
                    let stats = analyzer.analyze(&ds_window, &mut bands);
                    if std::env::var_os("MB_DEBUG_PUMP").is_some() {
                        let peak_band = bands.iter().enumerate().max_by_key(|(_, v)| **v).map(|(i, _)| i).unwrap_or(0);
                        let head = |v: &[f32]| -> String {
                            v.iter().take(8).map(|x| format!("{x:+.3}")).collect::<Vec<_>>().join(",")
                        };
                        eprintln!(
                            "[pump] rate={input_rate} new={new_samples} fresh={} ds_out={} ds_window={} rms={:.4} peak_band={peak_band}",
                            fresh.len(),
                            down_out.len(),
                            ds_window.len(),
                            stats.rms,
                        );
                        let l = me.shared.layout();
                        eprintln!(
                            "[pump]   layout: callbacks={} buffers={} ch={} bytes={} noninterleaved={}",
                            l.callbacks, l.buffers, l.channels, l.bytes, l.non_interleaved
                        );
                        eprintln!("[pump]   fresh=[{}] ds_head=[{}] ds_tail=[{}]",
                            head(&fresh),
                            head(&ds_window),
                            head(&ds_window[ds_window.len().saturating_sub(8)..]),
                        );
                    }
                    if let Ok(mut g) = me.frame.write() {
                        *g = SpectrumFrame {
                            bands: bands.to_vec(),
                            peak: stats.peak,
                            rms: stats.rms,
                            sample_rate: OUTPUT_RATE,
                            ts_ms: now_ms(),
                        };
                    }

                    // 原始 PCM（16kHz 单声道）：用的就是上面那份降采样结果
                    if let Some(tx) = &me.pcm_tx {
                        let chunk = me.shared.config.pcm_chunk.max(1);
                        for part in down_out.chunks(chunk) {
                            // 不足一块的尾巴直接发（直播场景宁愿小块也不要延迟）
                            let _ = tx.send(PcmChunk {
                                samples: Arc::from(part.to_vec()),
                                sample_rate: OUTPUT_RATE,
                                ts_ms: now_ms(),
                            });
                        }
                    }
                    std::thread::sleep(interval);
                }
            })
            .ok();
        if let Ok(mut g) = self.pump.write() {
            *g = handle;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_latest_samples_in_order() {
        let ring = SampleRing::new(8);
        ring.push(&[1.0, 2.0, 3.0]);
        let mut out = Vec::new();
        ring.tail(3, &mut out);
        assert_eq!(out, vec![1.0, 2.0, 3.0]);
        ring.push(&[4.0, 5.0]);
        ring.tail(4, &mut out);
        assert_eq!(out, vec![2.0, 3.0, 4.0, 5.0]);
    }

    #[test]
    fn ring_pads_with_zeros_before_any_write() {
        let ring = SampleRing::new(8);
        let mut out = Vec::new();
        ring.tail(4, &mut out);
        assert_eq!(out, vec![0.0, 0.0, 0.0, 0.0]);
        ring.push(&[9.0]);
        ring.tail(4, &mut out);
        assert_eq!(out, vec![0.0, 0.0, 0.0, 9.0], "末尾必须是最新样本");
    }

    #[test]
    fn ring_wraps_around_capacity() {
        let ring = SampleRing::new(4);
        for i in 0..20 {
            ring.push(&[i as f32]);
        }
        let mut out = Vec::new();
        ring.tail(4, &mut out);
        assert_eq!(out, vec![16.0, 17.0, 18.0, 19.0]);
    }

    #[test]
    fn downsampler_conserves_length_approximately() {
        let mut d = Downsampler::new(48_000, 16_000);
        let input = vec![0.5f32; 48_000];
        let mut out = Vec::new();
        d.push(&input, &mut out);
        assert!(
            (out.len() as i64 - 16_000).abs() < 100,
            "48k→16k 的 1 秒应约等于 16000 个样本，实际 {}",
            out.len()
        );
        assert!(out.iter().all(|v| (*v - 0.5).abs() < 1e-6), "直流分量应被保留");
    }

    #[test]
    fn downsampler_handles_non_integer_ratio_and_splits() {
        let mut d = Downsampler::new(44_100, 16_000);
        let mut total = 0usize;
        for _ in 0..44 {
            let mut out = Vec::new();
            d.push(&vec![0.25f32; 1000], &mut out);
            total += out.len();
        }
        let expected = 44_000.0 * 16_000.0 / 44_100.0;
        assert!(
            (total as f64 - expected).abs() < 50.0,
            "44.1k→16k 的 44000 个样本应约等于 {expected:.0}，实际 {total}"
        );
    }

    #[test]
    fn downsampler_is_passthrough_at_or_below_output_rate() {
        let mut d = Downsampler::new(16_000, 16_000);
        let mut out = Vec::new();
        d.push(&[1.0, 2.0], &mut out);
        assert_eq!(out, vec![1.0, 2.0]);
        let mut d = Downsampler::new(8_000, 16_000);
        let mut out2 = Vec::new();
        d.push(&[1.0, 2.0], &mut out2);
        assert_eq!(out2, vec![1.0, 2.0]);
    }

    #[test]
    fn tone_survives_downsampling_in_spectrum() {
        // 1kHz 正弦经 48k→16k 降采样后仍应落在同一段（降采样不该把能量搬走）
        let mut d = Downsampler::new(48_000, 16_000);
        let sig: Vec<f32> = (0..48_000)
            .map(|i| (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 48_000.0).sin() as f32)
            .collect();
        let mut out = Vec::new();
        d.push(&sig, &mut out);
        let (bands, stats) = crate::spectrum::analyze_once(&out);
        assert!(stats.rms > 0.5, "正弦 RMS 应约 0.7，实际 {}", stats.rms);
        let max_band = bands.iter().enumerate().max_by_key(|(_, v)| **v).map(|(i, _)| i).unwrap();
        // 1kHz@16k → bin 128 → 二次分箱约第 26 段
        assert!(
            (24..=29).contains(&max_band),
            "1kHz 降采样后应落在中低频段，实际第 {max_band} 段"
        );
    }

    #[test]
    fn capture_is_lazy_and_disabled_config_reports_unavailable() {
        let cap = AudioCapture::new(AudioConfig { enabled: false, ..Default::default() });
        assert!(!cap.is_started());
        cap.start().unwrap();
        assert_eq!(cap.status().state, SourceState::Unavailable);
        // 未启动时拿帧也不该 panic，且是全 0
        let f = cap.frame();
        assert!(f.bands.iter().all(|&b| b == 0));
        assert_eq!(f.bands.len(), SPECTRUM_BANDS);
        cap.stop();
    }
}
