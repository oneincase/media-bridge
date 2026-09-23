# media-bridge

跨平台的**系统媒体中间件**：一条通道拿到「现在在放什么」，并反过来控制它。

- **第 1 期（元数据 / 封面 / 歌词 / 音频）**：曲名、歌手、专辑、专辑艺术家、流派、时长、进度、
  播放态、循环/随机、封面（落盘 + 可直接出图）、歌词（LRC 时间轴）、系统音频（64 段频谱 + 可选原始 PCM）
- **第 2 期（反向控制）**：播放 / 暂停 / 播放暂停切换 / 停止 / 上一曲 / 下一曲 / 定位 / 快进快退 /
  循环模式 / 随机 / 音量 / 静音 / 倍速 —— 每一条都先查**能力位**再发，返回**回执**说明是否真的生效

macOS / Windows / Linux 三平台同一套 API、同一份数据结构、同一种线协议。
Rust 实现，无运行时依赖（不需要 `playerctl`、`media-control`、虚拟声卡之类的额外安装）。

## 目录

- [能做什么](#能做什么)
- [快速开始](#快速开始)
- [三种接入方式](#三种接入方式)
- [协议速查](#协议速查)
- [三平台实现与限制](#三平台实现与限制)
- [构建与测试](#构建与测试)
- [设计取舍](#设计取舍)

## 能做什么

| 能力 | 说明 |
|---|---|
| 正在播放 | 曲名 / 歌手 / 专辑 / 专辑艺术家 / 流派 / 作曲 / 年份 / 音轨号 / 时长 |
| 播放状态 | 播放中 / 暂停 / 停止、当前位置（可外推）、速率、音量、静音、循环模式、随机 |
| 封面 | 播放器给的封面字节，按**魔数**判定类型后落盘，给出本地路径 + 可选 HTTP 地址 + 原始尺寸 |
| 歌词 | LRC 解析（含增强型 `<>` 逐字标签、非标准 `[mm:ss:xx]`、多时间戳一行）；四级取词：音频同目录 `.lrc` → 缓存目录 → 内嵌标签 → 在线（LRCLIB） |
| 系统音频 | 采集**系统输出**（回环，不是麦克风）→ 统一 16kHz 单声道 → 64 段频谱（0-255，对数刻度）+ 可选原始 PCM |
| 反向控制 | 13 种传输命令，**能力位 + 回执**双保险（见下） |
| 事件流 | 只在**真的变化**时推：换曲 / 播放态变化 / 定位跳变 / 封面变更 / 歌词到齐 / 状态变化 |
| 排障 | `diagnose` 逐项说明「拿到了什么、差什么」；`status` 给出每个数据源的可用性与修复建议 |

**为什么反向控制要「能力位 + 回执」**：不同播放器支持的操作差别很大（有的不能 seek、有的不能设循环），
所以每条命令执行前先看播放器自报的能力位（置灰按钮的依据），执行后回读校验并给出
`applied` / `reason` / `effective`——「发出去」和「生效了」是两件事，中间件不说含糊话。

## 快速开始

```bash
# 编译（默认含 HTTP 与音频采集）
cargo build --release
# 想要内嵌标签 + 在线歌词：cargo build --release --features embedded,online

BIN=./target/release/media-bridge

# 看一眼能不能用（逐项说明数据源状态）
$BIN diagnose

# 当前在放什么（人看的）
$BIN now

# 机器看的
$BIN now --json

# 反向控制
$BIN control next
$BIN control seek-by +30s
$BIN control loop playlist
$BIN control pause

# 频谱（字符条 / JSON）
$BIN spectrum --bars
$BIN spectrum --json

# 把系统音频录成 WAV 核对采集链路
$BIN capture 5 --pcm --out /tmp/sys.wav
# 离线分析一个 WAV（把「采集」和「分析」分开验证）
$BIN spectrum --wav /tmp/sys.wav
```

没有播放器也不影响试：加 `--provider mock` 会用一个内置假播放器（封面是现造的 PNG、歌词是现造的 LRC），
可以完整跑通协议、事件、控制往返。

## 三种接入方式

三种方式**共用同一份数据模型与同一套方法名**，换接入方式不用改代码。

### 1. 子进程 + JSON 行（Node / Python / Go / Electron 宿主首选）

```js
import { spawn } from 'node:child_process';
const child = spawn('media-bridge', ['serve'], { stdio: ['pipe', 'pipe', 'inherit'] });
const pending = new Map();
let buf = '';
child.stdout.on('data', (chunk) => {
  buf += chunk;
  let i;
  while ((i = buf.indexOf('\n')) >= 0) {
    const line = buf.slice(0, i); buf = buf.slice(i + 1);
    if (!line.trim()) continue;
    const msg = JSON.parse(line);
    if (msg.event) { onEvent(msg); continue; }     // 事件（换曲/播放态/封面/歌词…）
    pending.get(msg.id)?.(msg); pending.delete(msg.id);  // 响应
  }
});
let seq = 0;
const call = (method, params = {}) => new Promise((res) => {
  const id = ++seq;
  pending.set(id, res);
  child.stdin.write(JSON.stringify({ id, method, params }) + '\n');
});

await call('hello');
const now = await call('now');                    // → { hasMedia, track, playback, capabilities }
const r = await call('control', { action: 'next' });   // → { outcome, now }
```

### 2. 本地 HTTP（浏览器 / 沙箱页面 / 多消费者）

```bash
media-bridge serve --http 127.0.0.1:8765
```

```js
const now = await (await fetch('http://127.0.0.1:8765/v1/now')).json();
document.querySelector('#cover').src = 'http://127.0.0.1:8765/v1/artwork';

// 事件流（SSE）；要频谱就加 events=spectrum&spectrumMs=100
const es = new EventSource('http://127.0.0.1:8765/v1/events?events=track,playback');
es.addEventListener('track', (e) => renderTrack(JSON.parse(e.data)));

await fetch('http://127.0.0.1:8765/v1/control', {
  method: 'POST',
  headers: { 'content-type': 'application/json' },
  body: JSON.stringify({ action: 'play-pause' }),
});
```

### 3. Rust 进程内（Tauri / 原生宿主）

```toml
media-bridge = { path = "../media-bridge/crates/media-bridge", features = ["embedded", "online"] }
```

```rust
use media_bridge::{BridgeConfig, MediaBridge, TransportCommand};
use std::sync::Arc;

let bridge = MediaBridge::new(BridgeConfig::default());
bridge.start();                                  // 起轮询与事件
let mut events = bridge.subscribe();             // 事件流
let now = bridge.refresh().await?;               // 立刻刷新一次
println!("{} - {}", now.track.as_ref().unwrap().title, now.track.as_ref().unwrap().artist);

// 第 2 期：发命令 + 拿到执行后的状态
let report = bridge.control(TransportCommand::Next).await?;
assert!(report.outcome.applied);

// 频谱（第一次调用会触发权限申请，所以是按需的）
bridge.ensure_audio()?;
let frame = bridge.spectrum();
println!("{:?}", frame.bands);
```

接入细节（含替换现有实现的具体映射、Node/Tauri/浏览器的完整示例）见
[`docs/INTEGRATION.md`](docs/INTEGRATION.md)；**构建与分发（每个平台要不要单独构建、CI 矩阵、通用二进制）**
见 [`docs/BUILD.md`](docs/BUILD.md)。

## 协议速查

三种报文靠字段名区分：**有 `ok` 是响应、有 `event` 是事件**。

```jsonc
// 请求
{"id":1,"method":"now","params":{}}

// 响应
{"id":1,"ok":true,"result":{"hasMedia":true,"track":{...},"playback":{...},"capabilities":{...}}}
{"id":2,"ok":false,"error":{"code":"no-media","message":"当前没有正在播放的媒体"}}

// 事件（不请自来，插在响应之间）
{"event":"track","now":{...},"tsMs":1780000000000}
{"event":"spectrum","frame":{"bands":[...],"peak":96,"rms":0.019,"sampleRate":16000}}
```

| 方法 | 作用 |
|---|---|
| `hello` | 协议版本、平台、可用方法清单（接进来先问它） |
| `status` | 各数据源健康状况与修复建议 |
| `now` | 完整快照（曲目 / 进度 / 能力 / 封面 / 歌词） |
| `capabilities` | 只要能力位（置灰按钮用） |
| `spectrum` | 最新 64 段频谱 |
| `artwork` | 当前封面（路径 / MIME / 尺寸；可选 base64） |
| `lyrics` | 当前歌词（`{"online":true}` 强制联网查） |
| `control` | 反向控制（见下） |
| `refresh` | 立刻重新轮询并返回新快照 |
| `config` | 生效中的配置（排查用） |
| `subscribe` | 设置本连接订阅哪些事件（含频谱间隔） |

`control` 的动作：`play` `pause` `play-pause` `stop` `next` `previous`
`seek{positionMs}` `seek-by{deltaMs}` `set-loop{mode}` `cycle-loop` `set-shuffle{on}` `toggle-shuffle`
`set-volume{volume}` `set-mute{muted}` `set-rate{rate}`

完整字段表、错误码、订阅语义、版本策略见 [`docs/PROTOCOL.md`](docs/PROTOCOL.md)。

## 三平台实现与限制

| | macOS | Windows | Linux |
|---|---|---|---|
| 正在播放 | MediaRemote（私有框架，运行时 `dlopen`） | GSMTC（WinRT `Windows.Media.Control`） | MPRIS over D-Bus |
| 系统音频 | CoreAudio Process Tap（14.2+） | WASAPI loopback | PulseAudio / PipeWire monitor |
| 需要额外安装 | 无（系统自带） | 无（Win10 1809+ 自带） | 无（`parec` / `pw-record` / `ffmpeg` 三选一） |
| 实测状态 | ✅ 已实测 | ✅ 已实测（Win11 ARM64；GSMTC 元数据/控制、WASAPI 采集、stdio/HTTP 全通过） | ✅ 已实测（Ubuntu 24.04 aarch64；MPRIS 元数据/控制、歌词、采集、HTTP） |
| 首次需要授权 | 「音频录制」（只用音频采集时） | 无 | 无 |
| 音量 / 静音写入 | 不提供 | 不提供 | 音量可写，静音不提供 |
| 倍速写入 | 不提供 | 支持 | 支持 |
| 本地文件路径 | 拿不到 | 拿不到 | 能拿到（`xesam:url`）→ 内嵌标签可用 |

**刻意不提供的功能**（宁可明确报「不支持」，也不给一个猜的语义）：

- macOS 的音量/静音/倍速：MediaRemote 对应的私有接口是 per-origin 语义、且没有可靠的回读校验手段
- Windows 的音量/静音：GSMTC 没有音量接口；系统音量要 WASAPI 端点音量、per-app 音量要音频会话 API，
  两者语义不同且都不属于「媒体会话」，混在一起改会改错东西
- Linux 的静音：MPRIS 没有静音概念，把音量设 0 再恢复无法还原原音量

三平台的差异细节、依赖与权限路径、以及**每项能力的验证状态**见 [`docs/PLATFORM.md`](docs/PLATFORM.md)。

## 构建与测试

> 关于「不同系统 / x64 / arm 是不是都要单独构建」这个问题，
> 以及 CI 矩阵、macOS 通用二进制（x64+arm64 合一）、交叉编译可行性，
> 见 **[`docs/BUILD.md`](docs/BUILD.md)**。

```bash
cargo build --release                     # 默认：http + audio
cargo build --release --all-features      # 再加 embedded（内嵌标签）+ online（在线歌词 / 远端封面）
cargo test                                # 单元 + 集成（stdio / HTTP 真起子进程）
cargo test --features embedded,online

# 交叉编译检查（在 macOS 上也能查另外两个平台的类型/API 用法）
cargo check --target x86_64-pc-windows-msvc --all-targets
cargo check --target x86_64-unknown-linux-gnu --all-targets
cargo check --target x86_64-unknown-linux-gnu --all-targets --features embedded
```

> 交叉检查**不要**加 `--all-features`：`online` 依赖的 `ureq` 会拉进 `ring`，
> 而 `ring` 的构建脚本需要**目标平台**的 C 工具链（在 macOS 上交叉编译 ring 到 Windows/Linux 会失败）。
> 这是构建工具链的限制而不是代码问题 —— 在目标平台上原生构建 `--all-features` 是正常的。

可选 feature：

| feature | 默认 | 作用 |
|---|---|---|
| `http` | ✅ | 本地 HTTP / SSE / WebSocket 服务 |
| `audio` | ✅ | 系统音频采集（64 段频谱 + PCM） |
| `embedded` | ❌ | 从本地音频文件读内嵌标签（标题/封面/歌词）；仅当能拿到文件路径时有意义（Linux 常见） |
| `online` | ❌ | 联网：在线歌词（[LRCLIB](https://lrclib.net)，免费无鉴权）+ 远端封面下载（MPRIS `artUrl` 是 http 时） |

## 设计取舍

- **统一在「轮询 + 位置外推」上**，而不是三平台各写一套原生推送。做法上做三件事补偿实时性：
  1s 基线轮询；快照带 `updatedAtMs` 让消费端自己外推进度；任何控制命令后开 200ms 突发窗口让状态快速收敛。
  没有媒体时降到 2s（笔记本上这点很重要）。
- **事件只报变化**。位置每秒都在变，那是外推能算出来的信息，不该占用带宽与唤醒。
- **能力显式声明**，而不是「先发命令再看失败」。`Capabilities` 描述当前播放器**实际**能做什么。
- **封面按魔数定类型**。播放器自称的 MIME 经常是空的或错的，而存错后缀会让浏览器直接不显示图片。
- **封面文件名带内容指纹**，并且每次写入用唯一临时名 + 原子 `rename`：
  消费端读到的永远是完整文件，多进程同时取同一张封面也不会互相踩。
- **频谱与 PCM 统一到 16kHz 单声道**（降采样用分箱平均自带抗混叠），这样「同一段音乐在三平台得到同一张图」；
  分析窗口 2048 点、分箱二次分布、`-70dB..0dB → 0..255`，与既有实现逐位对齐。
- **依赖尽量少**：没有 rand / chrono / mime / uuid / clap / anyhow-in-lib 之类的东西 ——
  中间件会被塞进各种宿主进程，依赖树越短越好。命令行解析是手写的（为了把 `--help` 写成真正的中文说明）。

## License

MIT
