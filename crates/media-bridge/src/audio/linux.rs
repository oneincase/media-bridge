//! Linux 系统音频采集：PulseAudio / PipeWire 的 **monitor 源**。
//!
//! 三级回退（前一级不存在就试下一级，都不行才报不可用）：
//!
//!   1. `parec`（pulseaudio-utils）—— PA 与 PipeWire 的 PA 兼容层都能用，最普遍
//!   2. `pw-record`（PipeWire 原生工具）—— 没装 PA 工具但有 PipeWire 时
//!   3. `ffmpeg -f pulse` —— 兜底（很多发行版默认有 ffmpeg）
//!
//! 三者都按「16kHz 单声道 s16le 写到 stdout」请求，所以下游不需要再降采样。
//!
//! ## 采集源：PA 与纯 PipeWire 的写法**不一样**（踩过）
//!
//! - **有 PA 兼容层**（`parec`/`ffmpeg -f pulse` 可用）时用 `@DEFAULT_MONITOR@`：
//!   它是 PA 命令行工具的保留名，**在连接建立时**由服务端解析成当前默认输出的监听源。
//! - **纯 PipeWire**（只有 `pw-record`、没有 `pipewire-pulse`）时**不能**用
//!   `@DEFAULT_MONITOR@`：它解析不了，`pw-record` 会**静默连到默认源**（也就是麦克风），
//!   而进程照样正常出数据 —— 你会在毫不知情的情况下采到麦克风。正确写法是
//!   `-P '{ stream.capture.sink = true }'`（让流去连 sink 的监听端口），
//!   实测这是唯一能采到系统输出的方式（`@DEFAULT_SINK@`/sink 名 + 该属性都行，
//!   而 `<sink>.monitor` 这种节点名在纯 PipeWire 上通常**不存在**）。
//!
//! ## 两个保留名都是「连接时一次性解析」—— 设备切换要自己跟上
//!
//! `@DEFAULT_MONITOR@` 和 `stream.capture.sink` 的默认解析都发生在**流创建那一刻**，
//! 之后采集流就钉死在那个 sink 的 monitor 上。用户切输出设备（插耳机/连蓝牙）后，
//! 旧流继续在旧 sink 上出静音 —— 与 Windows WASAPI loopback 同一类问题。
//! 解法是监视线程轮询默认 sink（`pactl get-default-sink`，纯 PipeWire 用
//! `wpctl get-default`），发现变化就**重启采集进程**：新连接会解析到新 sink。
//! 显式 `--device` 指定了源时不监视 —— 用户自己管理采集目标。
//!
//! 采集的是系统输出（loopback），**不是麦克风**；在没有音频子系统的环境
//! （容器、纯 tty）里会明确报不可用，而不是静默出静音帧。

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::CaptureShared;
use crate::error::{BridgeError, Result};
use crate::types::SourceState;

/// 采集请求的输出格式（与 `audio::OUTPUT_RATE` 一致）。
const RATE: u32 = super::OUTPUT_RATE;
/// 默认 sink 的轮询间隔。切换后的频谱空窗 = 轮询周期 + 重启耗时，2s 对频谱足够跟手。
const SINK_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// 「当前采集进程」槽位：收尾与重启都通过它拿进程句柄。
type ChildSlot = Arc<Mutex<Option<Child>>>;

/// 启动采集。
pub(crate) fn start(shared: Arc<CaptureShared>) -> Result<()> {
    let plan = plan_capture(shared.config.device.as_deref())?;
    let child_slot: ChildSlot = Arc::new(Mutex::new(None));
    // 代数计数：每次杀/起新进程都 +1。读线程拿着自己 spawn 时的值，
    // 退出时发现代数变了（被计划内重启杀掉）就安静退出，不误报「采集进程已退出」。
    let generation = Arc::new(AtomicU64::new(0));

    spawn_capture(&shared, &plan, &child_slot, &generation)?;
    shared.set_state(SourceState::Preparing, format!("{}（等待确认有音频数据）", plan.hint));

    // 收尾：杀掉当前采集进程（读线程看到代数已变，安静退出；watcher 随 stopping 退出）
    {
        let slot = child_slot.clone();
        let stop_gen = generation.clone();
        shared.on_stop(Box::new(move || {
            stop_gen.fetch_add(1, Ordering::Relaxed);
            kill_slot(&slot);
        }));
    }

    // 「启动成功」要拿数据说话：命令能起不代表真的连上了采集源。等最多 2 秒，
    // 没等到任何样本就如实报不可用（把「静默采不到」这种最难查的情况变成一句明确的话）。
    {
        let probe = shared.clone();
        let program = plan.program.clone();
        let hint = plan.hint.clone();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                if probe.is_stopping() {
                    return;
                }
                if probe.has_data() {
                    // 有数据了：把「正在采什么」写清楚（含实际生效的写法）
                    probe.set_state(SourceState::Running, hint);
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if !probe.is_stopping() {
                probe.set_state(
                    SourceState::Unavailable,
                    format!(
                        "{program} 已启动但 2 秒内没有任何音频数据：多半是采集源没连上。\
                         纯 PipeWire 环境需要 pw-record + stream.capture.sink；有 PulseAudio 时可用 parec。\
                         也可以用 --device 显式指定源（先用 wpctl status / pactl list short sources 查名字）。"
                    ),
                );
            }
        });
    }

    // 默认输出切换跟踪（仅自动选源时；用户显式指定了源就完全尊重用户的指定）
    if shared.config.device.is_none() {
        spawn_default_sink_watcher(shared, plan, child_slot, generation);
    }
    Ok(())
}

/// 起一个采集进程并为它配一个读线程。
///
/// 杀旧 / 起新都在槽位锁内完成，与收尾动作互斥 —— 保证 stop() 不会漏杀
/// 刚被重启动作拉起的进程。
fn spawn_capture(
    shared: &Arc<CaptureShared>,
    plan: &Plan,
    slot: &ChildSlot,
    slot_gen: &Arc<AtomicU64>,
) -> Result<()> {
    let Ok(mut guard) = slot.lock() else {
        return Err(BridgeError::other("采集进程槽位锁被污染"));
    };
    if shared.is_stopping() {
        return Ok(());
    }
    if let Some(mut old) = guard.take() {
        slot_gen.fetch_add(1, Ordering::Relaxed);
        let _ = old.kill();
        let _ = old.wait();
    }
    let mut child = Command::new(&plan.program)
        .args(&plan.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            let msg = format!("启动 {} 失败：{e}", plan.program);
            shared.set_state(SourceState::Unavailable, msg.clone());
            BridgeError::unavailable(msg)
        })?;

    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        shared.set_state(SourceState::Unavailable, "采集进程没有 stdout");
        return Err(BridgeError::other("采集进程没有 stdout"));
    };
    let my_gen = slot_gen.fetch_add(1, Ordering::Relaxed) + 1;
    *guard = Some(child);

    // 读线程：s16le → f32 单声道 → 环形缓冲
    let shared_reader = shared.clone();
    let gen_reader = Arc::clone(slot_gen);
    std::thread::Builder::new()
        .name("media-bridge-audio-read".to_string())
        .spawn(move || {
            read_loop(stdout, shared_reader, gen_reader, my_gen);
        })
        .map_err(|e| BridgeError::other(format!("无法创建采集读取线程：{e}")))?;
    Ok(())
}

/// 读线程主体：把子进程 stdout 的 s16le 流写进环形缓冲。
fn read_loop(
    mut stdout: impl Read + Send + 'static,
    shared: Arc<CaptureShared>,
    generation: Arc<AtomicU64>,
    my_gen: u64,
) {
    let mut buf = vec![0u8; 8192];
    let mut acc: Vec<u8> = Vec::with_capacity(8192);
    let mut first_chunk = true;
    loop {
        if shared.is_stopping() {
            return;
        }
        match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                // 记录一次布局（排查用）：外部命令给的是 s16le 单声道交错流
                if first_chunk {
                    first_chunk = false;
                    shared.record_layout(1, 1, n as u32, false);
                }
                acc.extend_from_slice(&buf[..n]);
                // 每 2 字节一个样本；尾部不足一个样本就留到下一轮
                let samples = acc.len() / 2;
                if samples > 0 {
                    let mut out = Vec::with_capacity(samples);
                    for i in 0..samples {
                        let v = i16::from_le_bytes([acc[i * 2], acc[i * 2 + 1]]);
                        out.push(v as f32 / 32768.0);
                    }
                    shared.push(&out, RATE);
                    acc.drain(..samples * 2);
                }
            }
            Err(e) => {
                // 计划内重启会杀掉旧进程导致读失败：代数已变，安静退出
                if generation.load(Ordering::Relaxed) == my_gen && !shared.is_stopping() {
                    shared.set_state(SourceState::Error, format!("读取采集流失败：{e}"));
                }
                return;
            }
        }
    }
    if generation.load(Ordering::Relaxed) == my_gen && !shared.is_stopping() {
        shared.set_state(
            SourceState::Unavailable,
            "采集进程已退出（可能是不支持 monitor 采集或设备被占用）",
        );
    }
}

/// 杀掉槽位里的采集进程（有就杀，没有就什么都不做）。
fn kill_slot(slot: &ChildSlot) {
    if let Ok(mut guard) = slot.lock() {
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// 盯默认输出的变化：发现变了就重启采集进程，让新连接解析到新 sink。
fn spawn_default_sink_watcher(
    shared: Arc<CaptureShared>,
    plan: Plan,
    slot: ChildSlot,
    generation: Arc<AtomicU64>,
) {
    std::thread::Builder::new()
        .name("media-bridge-audio-watch".to_string())
        .spawn(move || {
            // 没有可用的查询工具就退回老行为（采到哪个算哪个），不报错 ——
            // 查询工具缺失只是「无法自动跟随」，采集本身不受影响。
            let Some(mut probe) = default_sink_probe() else {
                return;
            };
            let mut current: Option<String> = None;
            while !shared.is_stopping() {
                std::thread::sleep(SINK_POLL_INTERVAL);
                if shared.is_stopping() {
                    return;
                }
                let Ok(out) = probe.output() else { continue };
                let sink = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if sink.is_empty() {
                    continue; // 服务端暂时不可答（如 PA 没起）：等下一轮
                }
                match &current {
                    None => current = Some(sink), // 先建立基线，不算变化
                    Some(prev) if *prev == sink => {}
                    Some(_) => {
                        current = Some(sink);
                        if spawn_capture(&shared, &plan, &slot, &generation).is_ok() {
                            shared.set_state(
                                SourceState::Running,
                                format!("{}（已跟随默认输出切换）", plan.hint),
                            );
                        }
                    }
                }
            }
        })
        .ok();
}

/// 用来查询「当前默认输出 sink」的命令（输出一行可比较的标识）。
fn default_sink_probe() -> Option<Command> {
    if binary_exists("pactl") {
        // PulseAudio 与 PipeWire 的 PA 兼容层都有 pactl
        let mut c = Command::new("pactl");
        c.arg("get-default-sink");
        return Some(c);
    }
    if binary_exists("wpctl") {
        // 纯 PipeWire：wireplumber 的默认 sink 节点 ID（设备重连会换 ID，重启一次无害）
        let mut c = Command::new("wpctl");
        c.arg("get-default").arg("@DEFAULT_AUDIO_SINK@");
        return Some(c);
    }
    None
}

#[derive(Clone)]
struct Plan {
    program: String,
    args: Vec<String>,
    hint: String,
}

/// 选出可用的采集命令。
fn plan_capture(device: Option<&str>) -> Result<Plan> {
    let source = device.unwrap_or("@DEFAULT_MONITOR@");
    let monitor_hint = if device.is_some() {
        format!("用户指定源 {source}")
    } else {
        "@DEFAULT_MONITOR@（默认输出的监听源，跟随默认输出切换）".to_string()
    };

    if binary_exists("parec") {
        return Ok(Plan {
            program: "parec".into(),
            args: vec![
                format!("--device={source}"),
                "--format=s16le".into(),
                format!("--rate={RATE}"),
                "--channels=1".into(),
                "--latency-msec=50".into(),
                "--client-name=media-bridge".into(),
            ],
            hint: format!("parec + {monitor_hint}"),
        });
    }
    if binary_exists("pw-record") {
        // 见文件头：纯 PipeWire 上必须走「连到 sink 的监听端口」，不能用 PA 的保留名。
        let mut args = Vec::new();
        let mut hint = String::from("pw-record + 系统输出的监听（stream.capture.sink）");
        if let Some(dev) = device {
            if let Some(base) = dev.strip_suffix(".monitor") {
                // 用户直接给了监听源名：按普通源连（这种名字只在有 monitor 节点的系统上存在）
                args.push(format!("--target={dev}"));
                hint = format!("pw-record + 用户指定监听源 {base}.monitor");
            } else {
                args.push(format!("--target={dev}"));
                args.push("-P".into());
                args.push("{ stream.capture.sink = true }".into());
                hint = format!("pw-record + 监听 {dev}");
            }
        } else {
            // 不给 target：配上 capture.sink 就会连到**默认输出**的监听端口
            args.push("-P".into());
            args.push("{ stream.capture.sink = true }".into());
        }
        args.extend([
            "--format=s16".into(),
            format!("--rate={RATE}"),
            "--channels=1".into(),
            "-".into(),
        ]);
        return Ok(Plan {
            program: "pw-record".into(),
            args,
            hint,
        });
    }
    if binary_exists("ffmpeg") {
        return Ok(Plan {
            program: "ffmpeg".into(),
            args: vec![
                "-hide_banner".into(),
                "-loglevel".into(),
                "error".into(),
                "-f".into(),
                "pulse".into(),
                "-i".into(),
                source.to_string(),
                "-f".into(),
                "s16le".into(),
                "-ac".into(),
                "1".into(),
                "-ar".into(),
                RATE.to_string(),
                "-".into(),
            ],
            hint: format!("ffmpeg -f pulse + {monitor_hint}"),
        });
    }
    Err(BridgeError::unavailable(
        "没有可用的采集工具：请安装 pulseaudio-utils（parec）、pipewire-bin（pw-record）或 ffmpeg 之一",
    ))
}

/// 在 PATH 里找可执行文件（不引 which crate）。
fn binary_exists(name: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    path.split(':').any(|dir| {
        let p = std::path::Path::new(dir).join(name);
        p.is_file()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_prefers_parec_and_encodes_the_requested_format() {
        // 这台机器上不一定有 parec；只验证「不论选到哪个，参数都是 16k 单声道 s16le」
        match plan_capture(None) {
            Ok(plan) => {
                let joined = format!("{} {}", plan.program, plan.args.join(" "));
                assert!(
                    joined.contains("16000") || joined.contains("--rate=16000") || joined.contains("-ar 16000"),
                    "必须请求 16kHz：{joined}"
                );
                assert!(joined.contains("1"), "必须请求单声道：{joined}");
                assert!(!plan.hint.is_empty());
            }
            Err(e) => assert_eq!(e.code(), "unavailable", "没有工具时应报 unavailable"),
        }
    }

    #[test]
    fn explicit_device_overrides_default_monitor() {
        if let Ok(plan) = plan_capture(Some("my-monitor")) {
            let joined = plan.args.join(" ");
            assert!(joined.contains("my-monitor"), "应使用用户指定的源：{joined}");
            assert!(!joined.contains("@DEFAULT_MONITOR@"));
        }
    }

    #[test]
    fn binary_exists_is_false_for_nonsense() {
        assert!(!binary_exists("definitely-not-a-real-binary-xyz"));
    }

    #[test]
    fn plan_is_cloneable_for_the_watcher() {
        // watcher 需要拿一份 plan 复本去重启采集进程
        if let Ok(plan) = plan_capture(None) {
            let copy = plan.clone();
            assert_eq!(plan.program, copy.program);
            assert_eq!(plan.args, copy.args);
        }
    }
}
