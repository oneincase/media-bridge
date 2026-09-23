# 线协议（协议版本 1）

同一份语义，两种传输：**NDJSON over stdio** 与 **HTTP + SSE + WebSocket**。
方法名、参数、返回结构完全一致 —— 先按 stdio 接好，之后想加一个网页看板不用改数据模型。

## 1. 报文格式

三种报文靠字段名区分，不需要嵌套包装：

| 报文 | 判别字段 | 形状 |
|---|---|---|
| 请求 | `method` | `{"id":1,"method":"now","params":{}}` |
| 响应 | `ok` | `{"id":1,"ok":true,"result":{…}}` |
| 事件 | `event` | `{"event":"track","now":{…},"tsMs":…}` |

- `id` 可以是任意 JSON 值（数字、字符串都行），响应原样带回；不传则响应里是 `null`。
- 一次请求必有一条响应；事件**不请自来**，可能插在任意两条响应之间（消费端要按字段分流，
  不要假设「下一行一定是我的响应」）。
- 时间戳一律是 **Unix 毫秒**（`tsMs` / `capturedAtMs` / `updatedAtMs`）。

### 1.1 stdio（一行一条 JSON）

```bash
media-bridge serve            # 或 media-bridge serve --stdio（等价）
```

- stdout **只走协议**；日志一律走 stderr（`--verbose` 会把收到的请求也打到 stderr）。
- 宿主关闭 stdin（或发 SIGINT/SIGTERM）→ 进程干净退出。
- 写出口是单写者：响应与事件不会交错成半个 JSON。

### 1.2 HTTP

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/health` | 存活探针 `{"ok":true,"service":"media-bridge","version":…}` |
| GET | `/v1/{method}` | 只读方法统一入口：`hello` `status` `now` `capabilities` `spectrum` `lyrics` `config` |
| GET | `/v1/artwork` | 当前封面（**二进制图片**，`content-type` 是真实类型） |
| POST | `/v1/control` | body 直接是一条命令：`{"action":"next"}` |
| POST | `/v1/rpc` | 通用调用：body 是 `{"method":…,"params":…}` |
| GET | `/v1/events` | SSE 事件流 |
| GET | `/v1/ws` | WebSocket（请求/事件双向；PCM 走二进制帧） |
| GET | `/v1/pcm` | 原始 PCM 的 NDJSON 流（需 `--pcm`） |

查询串里的键值会变成方法参数（值一律按字符串给，`true`/`1` 都能识别）：
`GET /v1/now?interpolate=false`、`GET /v1/lyrics?online=true`

- 错误映射：`no-media` → 404，`protocol` → 400，`unsupported`/`unavailable`/`denied`/`not-supported` → 503，其余 → 500。
- **CORS 默认放开**（`--cors '*'`）。默认只绑 `127.0.0.1`；绑到非回环地址会打警告。
  收紧用 `--cors off` 或 `--cors https://your.origin`。
- SSE 过滤：`?events=track,playback`；要频谱加 `&spectrumMs=100`（16–5000ms）。
  连接会发 `: keep-alive` 注释行防止中间层掐断。

## 2. 方法

### `hello`

接入后的第一件事。返回协议版本、平台、后端名与**方法清单**，宿主可据此做能力嗅探。

```json
{
  "protocol": 1,
  "version": "0.1.0",
  "platform": "macos",
  "arch": "aarch64",
  "provider": "macos-mediaremote",
  "pid": 4242,
  "features": ["http", "audio"],
  "methods": [{"name": "hello", "description": "协议版本/平台/可用方法"}]
}
```

### `status`

各数据源的健康状况与**给用户看的修复建议**（设置界面直接拿来显示）。

```json
{
  "platform": "macos", "arch": "aarch64", "version": "0.1.0",
  "provider": "macos-mediaremote",
  "sources": [
    {"name": "metadata", "state": "running",  "hint": "MediaRemote（系统级正在播放）"},
    {"name": "audio",    "state": "denied",   "hint": "创建音频 tap 被系统拒绝（OSStatus who? (2003332927)）：请在「系统设置 → 隐私与安全性 → 音频录制」…"},
    {"name": "lyrics",   "state": "idle",     "hint": "仅本地 sidecar/cache"},
    {"name": "artwork",  "state": "running",  "hint": "image/jpeg（12345 字节，Player）"}
  ],
  "features": ["http", "audio"],
  "cacheDir": "/Users/me/Library/Caches/media-bridge",
  "pollIntervalMs": 1000,
  "subscribers": 2,
  "polls": 1234,
  "uptimeMs": 1234567
}
```

`state` 取值：`idle`（还没启动/懒启动中）`preparing` `running` `unavailable`（环境缺东西）
`denied`（用户没授权）`error`。

### `now`

参数：`{"interpolate": true}`（默认 true = 位置外推到此刻；`false` = 原样返回上次轮询到的位置）

```json
{
  "hasMedia": true,
  "track": {
    "id": "com.spotify.track:0f3a…",
    "title": "歌名", "artist": "歌手", "album": "专辑",
    "albumArtist": "专辑歌手", "genre": "流派", "composer": "",
    "year": 2024, "trackNumber": 7,
    "durationMs": 215000,
    "artwork": {
      "key": "com.spotify.track:0f3a…",
      "mime": "image/jpeg", "bytes": 248123,
      "width": 640, "height": 640,
      "origin": "player",
      "path": "/Users/me/Library/Caches/media-bridge/covers/cover-1a2b3c4d5e6f7788.jpg",
      "httpPath": "/v1/artwork?v=99",
      "sourceUrl": "https://…"
    },
    "lyrics": {
      "source": "cache", "synced": true, "offsetMs": 0,
      "trackKey": "歌手|歌名|专辑|215",
      "lines": [{"tMs": 1000, "text": "第一句"}],
      "raw": "[00:01.00]第一句\n"
    },
    "source": {
      "provider": "macos-mediaremote",
      "appName": "Spotify", "appId": "com.spotify.client",
      "pid": 1234,
      "filePath": "/home/me/Music/x.flac",
      "url": "https://…"
    }
  },
  "playback": {
    "state": "playing",
    "positionMs": 42000,
    "positionSource": "polled",
    "durationMs": 215000,
    "rate": 1.0,
    "volume": 0.5, "muted": false,
    "loopMode": "playlist",
    "shuffle": true,
    "updatedAtMs": 1780000000000
  },
  "capabilities": { "play": true, "pause": true, "toggle": true, "stop": true,
                    "next": true, "previous": true, "seekAbsolute": true, "seekRelative": true,
                    "setLoop": true, "setShuffle": true,
                    "setVolume": false, "setMute": false, "setRate": false },
  "capturedAtMs": 1780000000000
}
```

字段语义要点：

- `hasMedia=false` 时 `track` 缺席，`capabilities` 全为 false。
- `positionSource`：`polled`（播放器报的）`interpolated`（服务端按速率外推的）`unavailable`（拿不到）。
  消费端要自己外推就用 `polled` 值 + `updatedAtMs` + `rate`：
  `pos_now = positionMs + (now - updatedAtMs) * rate`（暂停时不要外推）。
- `loopMode`：`off` / `track` / `playlist` / `unknown`（播放器没报时是 `unknown`，不要当成 `off`）。
- `track.id`：稳定标识（优先用播放器给的内容标识，退化为 `歌手|歌名|专辑`）—— **换曲判定与缓存键都用它**。
- 空字符串表示「这个平台/播放器不给这个字段」，不是错误。`albumArtist` 在 macOS 上恒为空（框架没导这个键）。

### `capabilities`

只返回能力位（与 `now.capabilities` 同构），用于初始化 UI 的按钮禁用态。

### `spectrum`

参数：`{"bandsOnly": false}`

```json
{"bands": [0,0,3,12,…], "peak": 96, "rms": 0.019, "sampleRate": 16000, "tsMs": 1780000000000}
```

- `bands` 恒为 **64 个 0–255** 的整数（对数刻度：`-70dB..0dB → 0..255`），与既有实现逐位对齐。
- 冷启动时服务端会等第一帧再返回（最多约 360ms），避免调用方拿到一屏 0。
- 未启用音频采集（`--no-audio`）时返回**形状正确但全 0** 的帧，而不是报错。
- 订阅推送用 `subscribe {"events":["spectrum"],"intervalMs":100}`，或 HTTP `?spectrumMs=100`。

### `artwork`

参数：`{"base64": false}`。返回 `now.track.artwork` 同构的对象；`base64:true` 时附带 `base64` 字段。

没有封面时返回错误 `no-media`。取二进制图片请用 HTTP `/v1/artwork`（带正确 `content-type`），
或直接读 `path` 指向的本地文件。

### `lyrics`

参数：`{"online": false}`。返回歌词对象或 `null`（查过但没有）。

- 默认先给已有的（本地命中过的），没有就做一次**本地**查询（同目录 `.lrc` → 缓存 → 内嵌标签）。
- `online:true` 会额外走一次在线查询（需要编译期 `online` feature，且启动未关掉在线）。
  在线命中会**写进缓存目录**，下次本地就能命中。

行时间与偏移：`lines[].tMs` 是原始时间轴，`offsetMs` 来自 LRC 头部的 `[offset:]`，
**正值表示歌词整体晚出现**（生效时间 = `tMs + offsetMs`）。服务端不替你做高亮计算，
拿 `{positionMs}` 自己比对即可。

### `control`（第 2 期）

`params` 就是一条命令，动作名 kebab-case、字段 camelCase：

```jsonc
{"action":"play"}                      // pause / play-pause / stop / next / previous
{"action":"seek","positionMs":120000}  // 绝对定位
{"action":"seek-by","deltaMs":-15000}  // 快退 15 秒（正数快进）
{"action":"set-loop","mode":"track"}   // off / track / playlist
{"action":"cycle-loop"}                // 关 → 列表 → 单曲 → 关
{"action":"set-shuffle","on":true}     // toggle-shuffle
{"action":"set-volume","volume":0.5}
{"action":"set-mute","muted":true}
{"action":"set-rate","rate":1.5}
```

返回：

```json
{
  "outcome": {
    "action": "seek",
    "applied": true,
    "reason": null,
    "effective": "120.1s"
  },
  "now": { …控制之后的完整快照… }
}
```

- `applied=false` 时**一定要看 `reason`**（能力位不支持 / 播放器拒绝 / 没媒体）。
- `effective` 是回读到的实际值（例如 `set-loop` 之后播放器真正处于的模式），
  这是「真的生效了」的证据，不是回显。
- 没有媒体时返回错误 `no-media`；命令参数不合法返回 `protocol`。

### `refresh` / `config`

- `refresh`：立刻重新问一次系统并返回新快照（不等下一个轮询周期）。
- `config`：生效中的配置（provider、缓存目录、各种轮询间隔、音频设置），排查用。

### `subscribe`

参数：`{"events":["track","playback"],"intervalMs":100}`

- `events` 缺省 = 除频谱外的全部事件。
- `intervalMs` 只对 `spectrum` 有意义（16–5000ms，默认 100）。**订阅了才会推频谱**，
  没订阅时一帧都不推（省带宽）。订阅 `pcm` 同理（需要启动时带 `--pcm`）。
- 这是**连接级**状态：stdio 连接与每条 WS 各自独立。HTTP 的一次性请求不受影响。

## 3. 事件

| `event` | 触发时机 | 载荷 |
|---|---|---|
| `track` | 换曲；或从「有媒体」变「无媒体」 | `now`（完整快照） |
| `playback` | 播放态/循环/随机/音量/速率变化，或**外推解释不了的位置跳变**（= seek） | `now` |
| `artwork` | 封面变了（已落盘，`artwork.path` 可直接用） | `artwork` |
| `lyrics` | 歌词到齐（本地命中或在线命中；`lyrics:null` 表示查过没有） | `trackId` `lyrics` `source` |
| `status` | 数据源可用性变化 | `status` |
| `spectrum` | 被订阅后按间隔推 | `frame` |
| `pcm` | 被订阅后推原始 PCM（base64 s16le） | `tsMs` `sampleRate` `channels` `format` `samples` `base64` |
| `error` | 数据源出错（按来源分类） | `source` `code` `message` |

**事件只报变化**：位置每秒都在变，那是外推能算出来的信息，不会每秒推一条。
事件顺序对换曲做了保证：`track` → `playback` → `artwork`（先知道换歌了，再收到封面）。
`lyrics` 可能晚几秒（在线查询），是独立事件。

## 4. 错误

```json
{"ok": false, "error": {"code": "unavailable", "message": "需要 ffmpeg 与 PulseAudio/PipeWire（monitor 源）"}}
```

| code | 含义 | 宿主该怎么做 |
|---|---|---|
| `unsupported` | 当前平台/编译期没有这条路径 | 隐藏对应 UI |
| `unavailable` | 环境缺东西（依赖/设备/服务） | 把 `message` 当作修复指引显示给用户 |
| `denied` | 用户没授权（macOS 音频录制等） | 显示授权路径，别反复重试 |
| `not-supported` | 播放器不支持这个操作 | 置灰按钮 |
| `no-media` | 当前没有媒体在放 | 正常状态，不是错误 |
| `protocol` | 请求不合法（未知方法、参数错） | 开发期错误，消息里会列出可用方法 |
| `io` / `other` | 其它 | 记日志，稍后重试 |

## 5. 版本与兼容

- `protocol` 是**协议版本**，破坏性变更时 +1；`hello` 里返回。
- 新增字段是兼容变更（消费端请忽略不认识的字段）；新增方法/新增事件名同理。
- `version` 是中间件版本（`cargo` 版本号），与协议版本独立。
- 字段命名约定：**方法名与动作名 kebab-case，JSON 字段 camelCase**。这条约定从 1 开始不变。
