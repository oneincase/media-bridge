# 接入指南

三类宿主各有最合适的方式，**数据模型完全一样**，所以先按一种接好，换另一种不用改业务代码。

- [1. Node / Electron 宿主：子进程 + JSON 行](#1-node--electron-宿主子进程--json-行)
- [2. Tauri / Rust 宿主：直接依赖 crate](#2-tauri--rust-宿主直接依赖-crate)
- [3. 浏览器 / 沙箱页面：本地 HTTP + SSE](#3-浏览器--沙箱页面本地-http--sse)
- [4. 替换 dsh-wallpaper-engine 现有实现](#4-替换-dsh-wallpaper-engine-现有实现)
- [5. 常见坑](#5-常见坑)

## 1. Node / Electron 宿主：子进程 + JSON 行

宿主不需要任何依赖：起子进程、写 JSON 行、读 JSON 行。

```js
// media-bridge-client.mjs —— 可直接抄进你的插件
import { spawn } from 'node:child_process';
import { EventEmitter } from 'node:events';

export class MediaBridge extends EventEmitter {
  #child; #seq = 0; #pending = new Map(); #buf = '';

  /** @param {string} bin 可执行文件路径（随插件分发的那个） */
  constructor(bin, { args = [] } = {}) {
    super();
    this.#child = spawn(bin, ['serve', ...args], { stdio: ['pipe', 'pipe', 'inherit'] });
    this.#child.stdout.setEncoding('utf8');
    this.#child.stdout.on('data', (chunk) => this.#onData(chunk));
    this.#child.on('exit', (code, sig) => this.emit('exit', { code, sig }));
  }

  #onData(chunk) {
    this.#buf += chunk;
    let i;
    while ((i = this.#buf.indexOf('\n')) >= 0) {
      const line = this.#buf.slice(0, i);
      this.#buf = this.#buf.slice(i + 1);
      if (!line.trim()) continue;
      let msg;
      try { msg = JSON.parse(line); } catch { continue; }
      // 事件一定带 event；响应一定带 ok。两者可能交错到达。
      if (msg.event) { this.emit(msg.event, msg); this.emit('event', msg); continue; }
      const p = this.#pending.get(msg.id);
      if (p) { this.#pending.delete(msg.id); msg.ok ? p.resolve(msg.result) : p.reject(new Error(`${msg.error.code}: ${msg.error.message}`)); }
    }
  }

  call(method, params = {}) {
    const id = ++this.#seq;
    return new Promise((resolve, reject) => {
      this.#pending.set(id, { resolve, reject });
      this.#child.stdin.write(JSON.stringify({ id, method, params }) + '\n');
    });
  }

  // 便捷方法（与协议方法一一对应）
  hello()        { return this.call('hello'); }
  now(opts = {}) { return this.call('now', opts); }
  status()       { return this.call('status'); }
  caps()         { return this.call('capabilities'); }
  spectrum()     { return this.call('spectrum'); }
  lyrics(opts={}){ return this.call('lyrics', opts); }
  artifacts()    { return this.call('artwork'); }
  control(cmd)   { return this.call('control', typeof cmd === 'string' ? { action: cmd } : cmd); }
  subscribe(events, intervalMs) { return this.call('subscribe', { events, intervalMs }); }

  close() { try { this.#child.stdin.end(); } catch {} }
}
```

用法：

```js
const bridge = new MediaBridge(BIN_PATH);
bridge.on('track',    (ev) => renderTrack(ev.now));       // 换曲
bridge.on('playback', (ev) => renderState(ev.now));       // 播放态/seek
bridge.on('lyrics',   (ev) => renderLyrics(ev.lyrics));   // 歌词到齐
bridge.on('error',    (ev) => console.warn('[media-bridge]', ev.code, ev.message));

await bridge.hello();                       // 可选：协议版本/平台
const now = await bridge.now();             // 曲目 + 进度 + 能力 + 封面路径
await bridge.subscribe(['track', 'playback', 'lyrics', 'artwork']);
// 要频谱就再订阅（默认不推，省带宽）：
await bridge.subscribe(['track', 'playback', 'spectrum'], 50);
```

## 2. Tauri / Rust 宿主：直接依赖 crate

```toml
# src-tauri/Cargo.toml
[dependencies]
media-bridge = { path = "../../media-bridge/crates/media-bridge", features = ["embedded", "online"] }
```

```rust
use media_bridge::{BridgeConfig, MediaBridge, TransportCommand, Event};
use std::sync::Arc;

pub fn setup(app: tauri::AppHandle) -> Arc<MediaBridge> {
    let bridge = MediaBridge::new(BridgeConfig::default());
    bridge.start();

    // 一条事件流，转发给前端（Tauri 的 emit 或你自己的事件总线）
    let mut rx = bridge.subscribe();
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Ok(ev) = rx.recv().await {
            match &ev {
                Event::Track { now } => { let _ = handle.emit("media://track", now); }
                Event::Playback { now } => { let _ = handle.emit("media://playback", now); }
                Event::Lyrics { lyrics, .. } => { let _ = handle.emit("media://lyrics", lyrics); }
                _ => {}
            }
        }
    });
    bridge
}

// 命令桥：前端 invoke('media_control', { action: 'next' })
#[tauri::command]
async fn media_control(bridge: tauri::State<'_, Arc<MediaBridge>>, action: String) -> Result<serde_json::Value, String> {
    let cmd: TransportCommand = serde_json::from_value(serde_json::json!({ "action": action }))
        .map_err(|e| e.to_string())?;
    let report = bridge.control(cmd).await.map_err(|e| e.to_string())?;
    Ok(serde_json::to_value(report).unwrap())
}
```

要点：

- 事件流是 `tokio::sync::broadcast`，**慢消费者会丢帧**（收到 `Lagged` 错误）——
  这是刻意的：事件是「变化通知」，丢一条不该拖垮整个连接。要严格不丢就自己缓存快照并比对。
- 频谱按需启动：`bridge.ensure_audio()` 第一次调用会触发系统权限（macOS 的「音频录制」）。
  没人看频谱就别调 —— 这也解释了为什么它不是自动启动的。
- `bridge.snapshot()` 是同步的（读缓存），`bridge.snapshot_at_now()` 会把位置外推到此刻，
  适合每帧渲染时取用（不要每帧去 `refresh`，那是问系统的操作）。

## 3. 浏览器 / 沙箱页面：本地 HTTP + SSE

```bash
media-bridge serve --http 127.0.0.1:8765 --pcm      # --pcm 想拿原始音频时加
```

```js
// 一次性取
const now = await (await fetch('http://127.0.0.1:8765/v1/now')).json();
document.querySelector('img.cover').src = 'http://127.0.0.1:8765/v1/artwork';

// 持续订阅（SSE 最省事；要双向就用 /v1/ws）
const es = new EventSource('http://127.0.0.1:8765/v1/events?events=track,playback,lyrics&spectrumMs=80');
es.addEventListener('track',    (e) => { const d = JSON.parse(e.data); renderTrack(d.now); });
es.addEventListener('playback', (e) => { const d = JSON.parse(e.data); renderState(d.now.playback); });
es.addEventListener('spectrum', (e) => { const d = JSON.parse(e.data); drawBars(d.frame.bands); });
es.addEventListener('lyrics',   (e) => { const d = JSON.parse(e.data); renderLyrics(d.lyrics); });

// 控制
await fetch('http://127.0.0.1:8765/v1/control', {
  method: 'POST', headers: { 'content-type': 'application/json' },
  body: JSON.stringify({ action: 'seek-by', deltaMs: -10000 }),
});
```

关于沙箱页面（本项目的实际场景）：**中间件本身就是另一个 loopback 源**，
所以「不透明源 / 沙箱 iframe 拿不到宿主能力头」的问题在这里天然不存在 ——
页面直接对我们这台服务器发请求即可，不需要经过宿主的任何桥。
CORS 默认放开，正是为了这件事。

需要**原始音频**（自己做 FFT / 语音分析 / 录一段）时：

```js
// WebSocket：订阅 pcm 之后音频走二进制帧（16kHz 单声道 s16le）
const ws = new WebSocket('ws://127.0.0.1:8765/v1/ws');
ws.binaryType = 'arraybuffer';
ws.onopen = () => ws.send(JSON.stringify({ method: 'subscribe', params: { events: ['pcm', 'track'] } }));
ws.onmessage = (e) => {
  if (typeof e.data === 'string') { /* JSON：事件或响应 */ return; }
  const pcm = new Int16Array(e.data);   // 16kHz 单声道，直接可喂 AudioContext/自己的工作线程
};
```

## 4. 替换 dsh-wallpaper-engine 现有实现

现有实现的接口（`lib/media-bridge.js` 的 `createMediaBridge()`）与中间件的对应关系：

| 现有 | 中间件 | 说明 |
|---|---|---|
| `spectrum()` → `Uint8Array(64)` | `spectrum().bands` | 语义一致：64 段、0-255、对数刻度；突发窗口/事件模型不同 |
| `nowPlaying()` → `{hasMedia,title,artist,album,playing,state,position,duration,thumbnail}` | `now()` → `{hasMedia,track,playback,capabilities}` | 字段名换了、字段多了；`state` 的 1/2 约定可用 `playback.state` 映射（见下） |
| `status()` → `{audio:{status,hint},nowPlaying:{status,hint}}` | `status().sources[]` | 每个源都带 `state` 与 `hint`（hint 就是给用户看的修复建议） |
| `artworkFile()` / `artworkMime()` | `now().track.artwork.path` / `.mime` | 文件名不再是固定的 `now-playing.jpg`，**请用快照里给的路径**（带内容指纹，换曲即换名） |
| `start()` / `stop()` | 子进程的启动/退出 | 不需要显式调用；宿主关掉 stdin 即退出 |
| `media-control`（brew 装） | 不需要 | macOS 走 MediaRemote，零外部依赖 |
| `playerctl`（Linux） | 不需要 | 走 MPRIS over D-Bus |
| `ffmpeg` + monitor（Linux/Windows 音频） | macOS 用 CoreAudio tap，Windows 用 WASAPI loopback，Linux 用 `parec`/`pw-record`/`ffmpeg` 三选一 | 不再需要用户自己去开「立体声混音」或装虚拟声卡 |

字段映射（旧 → 新）：

```js
function toLegacyWire(now) {
  const t = now.track ?? {};
  return {
    hasMedia: now.hasMedia,
    title: t.title ?? '', artist: t.artist ?? '', album: t.album ?? '',
    playing: now.playback.state === 'playing',
    state: now.playback.state === 'playing' ? 1 : 2,   // 旧 wire 的数值约定
    position: now.playback.positionMs / 1000,          // 旧 wire 是秒
    duration: now.playback.durationMs / 1000,
    thumbnail: t.artwork?.path,                        // 宿主自己的 /now-playing/artwork 路由改成读这个路径
    // 中间件新增、值得用的东西：
    loopMode: now.playback.loopMode,
    capabilities: now.capabilities,
    lyrics: t.lyrics,
  };
}
```

同时可以顺手拿掉现有实现里的三处妥协（都在新实现里解决了）：

1. **封面每秒重写**：`takeArtworkMac` 靠 key 去重 + 一次 `media-control get`。
   新实现里封面按内容指纹命名，`key` 不变就不碰磁盘。
2. **Windows 的媒体集成留待二期**：GSMTC 后端已经就绪。
3. **音频权限与依赖**：改用 native 采集（macOS tap / WASAPI loopback），
   少一层「先 brew install / 先开立体声混音」的用户教育成本。

## 5. 常见坑

1. **不要把「下一行」当成「我的响应」**。事件会插在响应之间；按 `event`/`ok` 字段分流。
2. **第一次问 `spectrum` 可能稍慢**（要等第一帧），服务端最多等 360ms 再返回。
   之后每次都是内存里的最新帧，微秒级。
3. **不要每秒调 `refresh`**。轮询是中间件自己的事（1s 基线 + 控制后 200ms 突发），
   消费端只读快照 + 外推位置即可。`refresh` 是给「用户点了刷新」这种场景用的。
4. **位置要外推**：`positionMs` 是上报时刻的值，配合 `updatedAtMs` 与 `rate` 自己算，
   暂停/停止时不要外推。
5. **`loopMode: "unknown"` 不等于 `"off"`**：播放器没上报时是 unknown，UI 应显示「未知」而不是「关」。
6. **能力位是「播放器现在能做什么」**，不是「协议支持什么」。按钮禁用态用它，别硬编码。
7. **`control` 的 `applied:false` 不是异常**：看 `reason`。最常见的两个是
   「播放器未声明支持」与「播放器未响应 seek」。
8. **别把封面 base64 塞进事件流**：用 `artwork.path`（本地文件）或 HTTP `/v1/artwork`。
9. **macOS 上只有音频采集需要授权**（「音频录制」）。纯元数据不用授权；
   不想申请就 `--no-audio`，`spectrum` 会返回全 0 而不是报错。
10. **Windows 上音量/静音不提供**（GSMTC 没有这个接口）—— 见 `docs/PLATFORM.md` 的说明，
    这不是「没做完」，是刻意不给一个语义不对的实现。
