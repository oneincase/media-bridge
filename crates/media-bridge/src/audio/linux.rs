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
//!   它是 PA 命令行工具的保留名，自动解析到当前默认输出的监听源，比先查 `pactl info`
//!   再拼 `<sink>.monitor` 更省事，用户切换输出设备也不会失效。
//! - **纯 PipeWire**（只有 `pw-record`、没有 `pipewire-pulse`）时**不能**用
//!   `@DEFAULT_MONITOR@`：它解析不了，`pw-record` 会**静默连到默认源**（也就是麦克风），
//!   而进程照样正常出数据 —— 你会在毫不知情的情况下采到麦克风。正确写法是
//!   `-P '{ stream.capture.sink = true }'`（让流去连 sink 的监听端口），
//!   实测这是唯一能采到系统输出的方式（`@DEFAULT_SINK@`/sink 名 + 该属性都行，
//!   而 `<sink>.monitor` 这种节点名在纯 PipeWire 上通常**不存在**）。
//!
//! 采集的是系统输出（loopback），**不是麦克风**；在没有音频子系统的环境
//! （容器、纯 tty）里会明确报不可用，而不是静默出静音帧。

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;

use super::CaptureShared;
use crate::error::{BridgeError, Result};
use crate::types::SourceState;

/// 采集请求的输出格式（与 `audio::OUTPUT_RATE` 一致）。
const RATE: u32 = super::OUTPUT_RATE;

/// 启动采集。
pub(crate) fn start(shared: Arc<CaptureShared>) -> Result<()> {
    let plan = plan_capture(shared.config.device.as_deref())?;
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
        shared.set_state(SourceState::Unavailable, "采集进程没有 stdout");
        let _ = child.kill();
        return Err(BridgeError::other("采集进程没有 stdout"));
    };

    shared.set_state(SourceState::Preparing, format!("{}（等待确认有音频数据）", plan.hint));

    // 收尾：杀掉采集进程（子进程随中间件退出而回收）
    shared.on_stop(Box::new(move || {
        let _ = child.kill();
        let _ = child.wait();
    }));

    // 「启动成功」要拿数据说话：命令能起不代表真的连上了采集源。等最多 2 秒，
    // 没等到任何样本就如实报不可用（把「静默采不到」这种最难查的情况变成一句明确的话）。
    {
        let probe = shared.clone();
        let program = plan.program.clone();
        let hint = plan.hint.clone();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                if probe.is_stopping() {
                    return;
                }
                if probe.has_data() {
                    // 有数据了：把「正在采什么」写清楚（含实际生效的写法）
                    probe.set_state(SourceState::Running, hint);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
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

    // 读线程：s16le → f32 单声道 → 环形缓冲
    let shared_reader = shared.clone();
    std::thread::Builder::new()
        .name("media-bridge-audio-read".to_string())
        .spawn(move || {
            let mut stdout = stdout;
            let mut buf = vec![0u8; 8192];
            let mut acc: Vec<u8> = Vec::with_capacity(8192);
            let mut first_chunk = true;
            loop {
                if shared_reader.is_stopping() {
                    break;
                }
                match stdout.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        // 记录一次布局（排查用）：外部命令给的是 s16le 单声道交错流
                        if first_chunk {
                            first_chunk = false;
                            shared_reader.record_layout(1, 1, n as u32, false);
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
                            shared_reader.push(&out, RATE);
                            acc.drain(..samples * 2);
                        }
                    }
                    Err(e) => {
                        shared_reader
                            .set_state(SourceState::Error, format!("读取采集流失败：{e}"));
                        break;
                    }
                }
            }
            if !shared_reader.is_stopping() {
                shared_reader.set_state(
                    SourceState::Unavailable,
                    "采集进程已退出（可能是不支持 monitor 采集或设备被占用）",
                );
            }
        })
        .map_err(|e| BridgeError::other(format!("无法创建采集读取线程：{e}")))?;

    Ok(())
}

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
        "@DEFAULT_MONITOR@（默认输出的监听源）".to_string()
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
}
