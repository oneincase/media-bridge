//! 频谱分析：PCM（f32）→ 64 段（0-255）。
//!
//! **与既有实现逐位对齐**。dsh-wallpaper-engine 老路径（macOS 走 Swift + vDSP，
//! Linux/Windows 走 Node 里的手写 FFT）用的是同一套分箱与 dB 映射：Hann 窗、
//! 峰值取桶内最大值、`20log10`、`-70dB..0dB` 线性映射到 `0..255`。这里照抄那套
//! 数学（JS 版本是跨平台基准），所以换成 Rust 之后观感不会变。
//!
//! 分箱是**二次分布**（低频密、高频疏）——音乐的能量集中在中低频，线性分箱会让
//! 前几段挤成一团、后面全黑。
//!
//! ## 频率口径（2026-09-24 修正）
//!
//! 本分析器的输入**恒为 `ANALYSIS_RATE` = 16 kHz**：音频管线把设备采样率统一
//! 降采样后再算频谱与 PCM（见 `audio` 模块的 `OUTPUT_RATE`，两边必须一致）。
//! 于是 bin 宽 = `ANALYSIS_RATE / FFT_N`，频段边界也必须按这个口径算。
//!
//! 旧代码把输入当 48 kHz（`FFT_N=2048`「48kHz 下约 43ms」、`USABLE_RATIO`
//! 「截到约 16kHz@48k」），实际喂 16 kHz ⇒ **所有频段比标称低 3 倍**：
//! 第 0 段落在 2..2 bin = 15.6..23.4 Hz 的不可听区间，音乐里能量为零。
//! 后果是所有「按第 0 段取值」的音谱组件恒静止——2902406982 的三角漏斗填充
//! （`Simple_Audio_Bars` 的 `Bar Count=1` ⇒ `frequency = 0` ⇒ 只采第 0 段）底部
//! 永远不填充、偶发次声噪声才闪一下。现在按真实口径取：`FFT_N=1024`
//! （16 kHz → bin 15.625 Hz、窗长 64 ms，与 dsh 老实现同参数）、起点
//! `BIN_LO=2` = 31.25 Hz（跳过 DC 与 20-30 Hz 次声），最高约 5.7 kHz。

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;
use std::sync::Arc;

use crate::types::SPECTRUM_BANDS;

/// 分析输入采样率（= 音频管线降采样后的 `audio::OUTPUT_RATE`）。
/// 改这里必须同步改 `audio::OUTPUT_RATE`（`audio` 模块有 const 断言盯着）。
pub const ANALYSIS_RATE: u32 = 16_000;
/// FFT 窗口长度（1024 点 @16 kHz → 64 ms、bin 15.625 Hz）。
pub const FFT_N: usize = 1024;
/// 与旧实现一致的静音下限（dB）：低于它一律算 0。
const DB_FLOOR: f32 = -70.0;
/// 分箱起点（跳过 DC 与次声噪声）：@16k/1024 = 31.25 Hz，音乐最底的低音区。
const BIN_LO: usize = 2;
/// 用到的高频比例：截到约 5.7 kHz（16 kHz 输入的可用上限），更高频段能量极微。
const USABLE_RATIO: f64 = 0.72;

/// 复用的频谱分析器（内部持有 FFT plan 与临时缓冲，避免每帧分配）。
pub struct SpectrumAnalyzer {
    fft: Arc<dyn rustfft::Fft<f32>>,
    scratch: Vec<Complex<f32>>,
    fft_buf: Vec<Complex<f32>>,
    hann: Vec<f32>,
    /// 预计算的分箱边界（f0, f1），长度 = BANDS
    edges: Vec<(usize, usize)>,
}

impl SpectrumAnalyzer {
    pub fn new() -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_N);
        let scratch = vec![Complex::new(0.0f32, 0.0f32); fft.get_inplace_scratch_len()];
        // Hann 窗（周期外的分母用 n-1，与 JS 基准一致）
        let hann = (0..FFT_N)
            .map(|i| {
                let x = 2.0 * std::f64::consts::PI * i as f64 / (FFT_N as f64 - 1.0);
                (0.5 - 0.5 * x.cos()) as f32
            })
            .collect();
        let half = FFT_N / 2;
        let usable = half as f64 * USABLE_RATIO;
        let edges = (0..SPECTRUM_BANDS)
            .map(|b| {
                let x0 = BIN_LO as f64 + (usable * (b as f64 / SPECTRUM_BANDS as f64).powi(2)).floor();
                let x1 = BIN_LO as f64 + (usable * ((b + 1) as f64 / SPECTRUM_BANDS as f64).powi(2)).floor();
                let f0 = x0 as usize;
                let f1 = (x1 as usize).max(f0 + 1).min(half);
                (f0.min(half.saturating_sub(1)), f1)
            })
            .collect();
        Self {
            fft,
            scratch,
            fft_buf: vec![Complex::new(0.0f32, 0.0f32); FFT_N],
            hann,
            edges,
        }
    }

    /// 分析一帧 PCM（单声道 f32，取值范围 -1.0..1.0），返回 64 段 0-255。
    ///
    /// 只取**末尾** `FFT_N` 个样本（调用方给整段环形缓冲的快照，末尾即最新）。
    /// 样本不足 `FFT_N` 时补零，静音输入返回全 0。
    pub fn analyze(&mut self, samples: &[f32], out: &mut [u8; SPECTRUM_BANDS]) -> SpectrumStats {
        let take = samples.len().min(FFT_N);
        let offset = samples.len() - take;
        for (i, slot) in self.fft_buf.iter_mut().enumerate() {
            let v = if i < take { samples[offset + i] } else { 0.0 };
            slot.re = v * self.hann[i];
            slot.im = 0.0;
        }
        self.fft.process_with_scratch(&mut self.fft_buf, &mut self.scratch);

        let norm = 1.0 / FFT_N as f64;
        let half = FFT_N / 2;
        for (b, &(f0, f1)) in self.edges.iter().enumerate() {
            let mut peak = 0.0f64;
            for f in f0..f1.min(half) {
                let c = self.fft_buf[f];
                let mag = ((c.re as f64) * (c.re as f64) + (c.im as f64) * (c.im as f64)).sqrt() * norm;
                if mag > peak {
                    peak = mag;
                }
            }
            let db = 20.0 * peak.max(1e-7).log10();
            let n = ((db as f32 - DB_FLOOR) / -DB_FLOOR).clamp(0.0, 1.0);
            out[b] = (n * 255.0).round() as u8;
        }

        // 整体能量（消费者做「有没有声音」的判定用，不必自己扫 64 段）
        let mut sum_sq = 0.0f64;
        let mut max_abs = 0.0f32;
        for &s in &samples[offset..] {
            let a = s.abs();
            if a > max_abs {
                max_abs = a;
            }
            sum_sq += (s as f64) * (s as f64);
        }
        let rms = if take > 0 {
            (sum_sq / take as f64).sqrt() as f32
        } else {
            0.0
        };
        let peak = out.iter().copied().max().unwrap_or(0);
        SpectrumStats { rms, peak, input_peak: max_abs }
    }
}

impl Default for SpectrumAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

/// 一帧的统计量。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpectrumStats {
    /// 帧 RMS（0.0-1.0）
    pub rms: f32,
    /// 64 段里的最大值（0-255）
    pub peak: u8,
    /// 输入样本的绝对峰值（用于判断是否真的静音 —— 频谱段可能因 dB 下限全为 0）
    pub input_peak: f32,
}

/// 单次调用便捷入口（内部新建分析器；连续分析请自己持有 `SpectrumAnalyzer`）。
pub fn analyze_once(samples: &[f32]) -> ([u8; SPECTRUM_BANDS], SpectrumStats) {
    let mut analyzer = SpectrumAnalyzer::new();
    let mut bands = [0u8; SPECTRUM_BANDS];
    let stats = analyzer.analyze(samples, &mut bands);
    (bands, stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(freq: f64, sample_rate: f64, n: usize, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f64 / sample_rate;
                (amp as f64 * (2.0 * std::f64::consts::PI * freq * t).sin()) as f32
            })
            .collect()
    }

    #[test]
    fn silence_gives_all_zero() {
        let mut a = SpectrumAnalyzer::new();
        let mut bands = [0u8; SPECTRUM_BANDS];
        let stats = a.analyze(&vec![0.0f32; FFT_N], &mut bands);
        assert!(bands.iter().all(|&b| b == 0), "静音应为全 0，实际 {bands:?}");
        assert_eq!(stats.peak, 0);
        assert_eq!(stats.rms, 0.0);
    }

    #[test]
    fn empty_input_is_safe_and_silent() {
        let mut a = SpectrumAnalyzer::new();
        let mut bands = [1u8; SPECTRUM_BANDS];
        let stats = a.analyze(&[], &mut bands);
        assert!(bands.iter().all(|&b| b == 0));
        assert_eq!(stats.rms, 0.0);
    }

    #[test]
    fn one_khz_tone_lands_in_expected_band() {
        let mut a = SpectrumAnalyzer::new();
        let mut bands = [0u8; SPECTRUM_BANDS];
        let sig = tone(1000.0, ANALYSIS_RATE as f64, FFT_N, 1.0);
        let stats = a.analyze(&sig, &mut bands);
        let max_band = bands
            .iter()
            .enumerate()
            .max_by_key(|(_, v)| *v)
            .map(|(i, _)| i)
            .unwrap();
        // 1000Hz @16k → bin 64 → 二次分箱落在第 26 段
        assert_eq!(max_band, 26, "1kHz 应落在第 26 段，实际 {max_band}（bands={bands:?}）");
        assert!(
            bands[26] > 180,
            "满幅正弦的第 26 段应接近满格，实际 {}",
            bands[26]
        );
        assert!(stats.input_peak > 0.9 && stats.input_peak <= 1.0);
        assert!(stats.rms > 0.5, "满幅正弦 RMS 应明显大于 0，实际 {}", stats.rms);
    }

    /// 第 0 段必须落在**可听的低音区**：按第 0 段取值的音谱组件（`Bar Count=1` 的
    /// 漏斗填充等）只有在这里才有能量可跟。曾按 48 kHz 标称算边界，实际输入 16 kHz，
    /// 第 0 段落到 15.6..23.4 Hz，音乐里恒 0（2902406982 底部不填充）。
    #[test]
    fn bass_tone_reaches_band_zero() {
        let mut a = SpectrumAnalyzer::new();
        let mut bands = [0u8; SPECTRUM_BANDS];
        // 40Hz @16k → bin 2.56 → 第 0 段（31.25..46.9 Hz）
        a.analyze(&tone(40.0, ANALYSIS_RATE as f64, FFT_N, 0.5), &mut bands);
        assert!(
            bands[0] > 60,
            "40Hz 必须点亮第 0 段，实际 {}（bands={bands:?}）",
            bands[0]
        );
    }

    /// 负控：高频不得点亮第 0 段（否则上位「40Hz 点亮第 0 段」可能只是全段一起亮）。
    /// 注意不能用次声（如 18 Hz）做负控：64 ms 窗在低频的主瓣约 4 bin（±31 Hz），
    /// 次声泄漏进第 0 段是窗函数的物理结果，不是缺陷。
    #[test]
    fn high_tone_does_not_light_band_zero() {
        let mut a = SpectrumAnalyzer::new();
        let mut bands = [0u8; SPECTRUM_BANDS];
        a.analyze(&tone(1000.0, ANALYSIS_RATE as f64, FFT_N, 0.5), &mut bands);
        assert_eq!(bands[0], 0, "1kHz 不该点亮第 0 段（bands={bands:?}）");
    }

    #[test]
    fn low_tone_lands_in_low_band_and_high_tone_higher() {
        let mut a = SpectrumAnalyzer::new();
        let mut low = [0u8; SPECTRUM_BANDS];
        let mut high = [0u8; SPECTRUM_BANDS];
        a.analyze(&tone(100.0, ANALYSIS_RATE as f64, FFT_N, 1.0), &mut low);
        a.analyze(&tone(4000.0, ANALYSIS_RATE as f64, FFT_N, 1.0), &mut high);
        let argmax = |b: &[u8; SPECTRUM_BANDS]| {
            b.iter().enumerate().max_by_key(|(_, v)| *v).map(|(i, _)| i).unwrap()
        };
        let lo_band = argmax(&low);
        let hi_band = argmax(&high);
        assert!(lo_band < 8, "100Hz 应在低频段，实际第 {lo_band} 段");
        assert!(hi_band > 40, "4kHz 应在高频段，实际第 {hi_band} 段");
        assert!(lo_band < hi_band);
    }

    #[test]
    fn band_edges_are_monotonic_and_within_spectrum() {
        let a = SpectrumAnalyzer::new();
        let half = FFT_N / 2;
        for (b, &(f0, f1)) in a.edges.iter().enumerate() {
            assert!(f0 < f1, "第 {b} 段边界非法：{f0}..{f1}");
            assert!(f1 <= half, "第 {b} 段越界：{f1} > {half}");
            if b > 0 {
                assert!(f0 >= a.edges[b - 1].0, "第 {b} 段起点应单调不减");
            }
        }
        assert_eq!(a.edges.len(), SPECTRUM_BANDS);
    }

    #[test]
    fn output_range_is_always_zero_to_255() {
        let mut a = SpectrumAnalyzer::new();
        let mut bands = [0u8; SPECTRUM_BANDS];
        // 超范围输入（削波）不应 panic，也不该溢出
        let hot = tone(440.0, ANALYSIS_RATE as f64, FFT_N, 4.0);
        a.analyze(&hot, &mut bands);
        // u8 天然 <= 255：真正要验的是「没有整段饱和到 255」（削波输入也不该全亮）
        assert!(bands.iter().filter(|&&b| b == 255).count() < SPECTRUM_BANDS);
        assert!(bands.iter().any(|&b| b > 0));
    }
}
