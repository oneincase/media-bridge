# 三平台实现、依赖、权限与验证状态

这份文档的目标是**能查、可追溯**：每项能力背后是哪个系统接口、需要什么依赖/授权、
哪些限制是刻意的、以及**验证到哪一步为止**。

## 1. 一览

| | macOS | Windows | Linux |
|---|---|---|---|
| 元数据/控制 | MediaRemote（私有框架，运行时 `dlopen`） | GSMTC（WinRT `Windows.Media.Control`） | MPRIS over D-Bus |
| 系统音频 | CoreAudio Process Tap（macOS **14.2+**） | WASAPI loopback（Win10 1809+） | PulseAudio / PipeWire 的 monitor 源 |
| 外部依赖 | **无** | **无** | `parec`（pulseaudio-utils）或 `pw-record`（pipewire-bin）或 `ffmpeg` 之一 |
| 首次授权 | 仅音频采集需要「音频录制」 | 无 | 无 |
| 本地文件路径 | 拿不到 | 拿不到 | 能拿到（`xesam:url` 是 `file://`） |
| 内嵌标签（`embedded`） | 用不上（没路径） | 用不上（没路径） | **有用**（走本地文件） |

三种 Windows/Linux 的采集命令请求的都是「16kHz 单声道 s16le 写到 stdout」，
所以下游（降采样/FFT/PCM 输出）完全共用一份代码。

## 2. macOS

### 2.1 正在播放：MediaRemote

公开 API 里**没有**「系统正在播放什么」这种全局查询（`MPNowPlayingInfoCenter` 只能读自己进程写的内容）。
系统级的 now-playing 只存在于 `MediaRemote.framework`，控制中心的媒体面板、耳机按键走的都是它。

- 全部符号运行时 `dlopen` + `dlsym`；**拿不到就降级**（在 `status` 里说明缺什么），不做强依赖。
- 用到的符号（都在本机 `dyld_info -exports` 里核对过）：

  | 用途 | 符号 |
  |---|---|
  | 元数据字典 | `MRMediaRemoteGetNowPlayingInfo` |
  | 播放态 | `MRMediaRemoteGetNowPlayingApplicationIsPlaying` |
  | 应用名 / PID | `MRMediaRemoteGetNowPlayingApplicationDisplayName` / `…ApplicationPID` |
  | 能力位 | `MRMediaRemoteGetSupportedCommands` |
  | 传输控制 | `MRMediaRemoteSendCommand` |
  | 定位 | `MRMediaRemoteSetElapsedTime` |
  | 循环 / 随机 | `MRMediaRemoteSetRepeatMode` / `MRMediaRemoteSetShuffleMode` |

- 字典的键从框架 dlsym（不是手抄字符串）。**注意**：`dlsym` 得到的是「指向该 `CFStringRef` 全局量的
  指针」，要解一层引用才是键 —— 少解一层查表全落空，多解一层拿到的是字符串对象的前 8 字节，
  同样查不到（而且**不报错**，只是「永远读不到元数据」）。这行代码有真 CF 类型的测试钉着。
- 框架**没有** `kMRMediaRemoteNowPlayingInfoAlbumArtist` 这个符号（把导出符号列过一遍确认），
  所以 macOS 的 `albumArtist` 恒为空 —— 不是漏了，是拿不到。

**循环/随机的数值语义**依据 Apple 公开头文件的枚举顺序（`MPRemoteControlTypes.h`）：

```
MPRepeatType:  Off = 0, One = 1, All = 2     →  off / track / playlist
MPShuffleType: Off = 0, Items = 1, Collections = 2
```

设值时先直接 `Set` + **回读校验**；播放器不认就直接写，退回用 `AdvanceRepeatMode`
（循环按钮走的那条命令）轮转逼近，仍不成功就如实报 `applied:false`。

### 2.2 系统音频：CoreAudio Process Tap

`CATapDescription`（ObjC 运行时现造，不需要写 ObjC）→ `AudioHardwareCreateProcessTap` →
私有聚合设备承载 tap → `AudioDeviceCreateIOProcIDWithBlock` → `AudioDeviceStart`。
采集的是**系统输出**（loopback），不经过麦克风。

实现上的两个硬约束与坑：

- **API 尺寸必须对着真头文件量**（不是猜）：`sizeof(AudioStreamBasicDescription) = 40`、
  `sizeof(AudioBuffer) = 16`、`sizeof(AudioBufferList) = 24`、`mBuffers` 偏移 `8`。
  这些数字来自直接编译 C 程序问编译器，并且写进了单元测试。
- **进程排除列表要的是 CoreAudio 进程对象 ID，不是 Unix PID**。塞 PID 进去会让
  `AudioHardwareCreateProcessTap` 返回 `'!obj'`（bad AudioObjectID）——错误信息不会告诉你是这里错了。
  正确做法是先 `kAudioHardwarePropertyTranslatePIDToProcessObject`（`'id2p'`）翻译，
  翻译不出来就退化成空列表（不排除自己只是「可能采到自己的声音」，塞错值直接建不出来）。
- 实时回调里**只做算术与原子存储**：不分配内存、不加锁、不打日志（混音用的临时缓冲在启动时
  一次性分配；回调布局诊断走原子快照，由普通线程读取和打印）。
- 权限被拒时把状态置为 `denied` 并给出设置路径，**不重试、不反复弹窗**。
  错误码按类分开：`'!obj'`/`'!dev'`/`'!str'`/`'unop'` 归为「参数问题」，
  `'who?'` 归为「系统拒绝（多半是没授权）」——混成一句「请去授权」会让人白折腾一圈。

### 2.3 刻意不提供的功能

| 功能 | 为什么不做 |
|---|---|
| 音量 / 静音写入 | MediaRemote 的对应接口是 per-origin 语义、没有可靠回读校验手段。宁可明确报不支持，也不给一个猜的语义 |
| 倍速写入 | 同上（有 `MRMediaRemoteSetPlaybackSpeed` 符号，但参数语义随系统版本变化且无法验证） |

## 3. Windows

### 3.1 正在播放：GSMTC

任何注册了系统媒体会话的播放器（Spotify、foobar2000、浏览器、甚至 PowerPoint）都能被读到和控制，
所以不需要任何第三方依赖。

**实现取向：专用线程 + 消息通道**。WinRT 接口对象要求同一个 COM 公寓里使用，
而 `tokio` 的阻塞线程池不保证每次给你同一条线程；因此起一条专属工作线程，
`RoInitialize(MULTITHREADED)` 一次，之后所有调用都排进它自己的队列 —— 公寓亲和性问题从根上消失。

- 能力位来自 `GetPlaybackInfo().Controls()`：`IsPlayEnabled` / `IsPauseEnabled` /
  `IsPlayPauseToggleEnabled` / `IsStopEnabled` / `IsNextEnabled` / `IsPreviousEnabled` /
  `IsPlaybackPositionEnabled`（seek）/ `IsRepeatEnabled` / `IsShuffleEnabled` / `IsPlaybackRateEnabled`。
- 每个 `Try*Async` 都返回「请求是否被接受」的 bool，我们把它当 `applied` ——
  这比「发出去了」诚实得多（返回 `false` 时回执里会写清楚）。
- 时间轴单位是 100 纳秒（`TimeSpan`），换算时统一除 10⁴ 得到毫秒。
- 有些属性返回 WinRT 的「可空标量」`IReference<T>`（`AutoRepeatMode` / `IsShuffleActive` /
  `PlaybackRate`），要 `.Value()` 才拿到真值 —— 直接当值用会拿到一个包装对象。
- 阻塞等待异步操作用的是 `join()`（`windows-future` 0.3 起的方法名；0.2 时代叫 `get()` 且需要引 trait）。

### 3.2 系统音频：WASAPI loopback

把默认**渲染**端点以 `AUDCLNT_STREAMFLAGS_LOOPBACK` 打开成 capture 流，
就能拿到系统正在播放的声音 —— **不需要虚拟声卡、不需要「立体声混音」**
（老实现要求用户自己去开立体声混音或装 VB-Cable，那是最劝退的一步）。

- 格式由系统决定：回环流必须用 `GetMixFormat` 的格式。支持 float32 与 16 位 PCM 两种
  （EXTENSIBLE 的真实格式取 `SubFormat` GUID 的前 4 字节，等于 `WAVE_FORMAT_*` 标签），
  其它格式明确报不可用，不硬凑。
- `WAVEFORMATEX` 是 `pack(1)`：字段必须**先拷到局部变量**再使用，否则对 packed 字段取引用是 UB
  （编译器会直接拒绝）。
- 静音包（`AUDCLNT_BUFFERFLAGS_SILENT`）也要走一遍环形缓冲，否则写指针不前进，
  频谱泵会以为采集停了。

### 3.3 刻意不提供的功能

| 功能 | 为什么不做 |
|---|---|
| 音量 / 静音 | GSMTC 没有音量接口。系统音量要 WASAPI **端点**音量，per-app 音量要**音频会话** API；两者语义不同、也都不属于「媒体会话」。混在一起改会改错东西 |

## 4. Linux

### 4.1 正在播放：MPRIS

MPRIS（`org.mpris.MediaPlayer2.*`）是 Linux 桌面的事实标准，播放器/浏览器/mpv 都实现它，
所以**不依赖任何外部命令行工具**（老实现走 `playerctl`，装了才有得用）。

- 用 zbus 的**动态 Proxy**（`get_property` / `call_method`）而不是 `#[zbus::proxy]` 宏：
  属性名和方法名就是 MPRIS 规范里的字符串，读代码时不用在两套名字之间来回翻译。
- 播放器选择顺序：**正在播放的 > 有元数据的 > 名字排序第一个**。
  这样「浏览器里放着视频」不会被「暂停着的音乐软件」抢走焦点。选中的会缓存，但每次先探活。
- 元数据是 `a{sv}`，提取时的容错（都有单元测试）：
  - `xesam:artist` 规范是 `as`，但**很多播放器给单个字符串** —— 两种都认；
  - `mpris:trackid` 是对象路径（`o`），不是字符串；
  - 数值字段可能是 `i32`/`i64`/`u32`/`f64` 各种整型。
- 相对定位用 `Seek(Offset)`（微秒，不需要 trackid）；绝对定位用 `SetPosition(trackid, 微秒)`，
  播放器没给 `mpris:trackid` 时明确拒绝并提示改用 `seek-by`。
- 循环模式用 `LoopStatus` 读写属性（`None`/`Track`/`Playlist`）；`CycleLoop` 是「读当前 → 换成下一个」。

### 4.2 系统音频：monitor 源

三级回退，前一级不存在就试下一级，都不行才报不可用：

1. `parec`（pulseaudio-utils）——**有 PulseAudio 兼容层**时最普遍
2. `pw-record`（PipeWire 原生工具）——**纯 PipeWire**（没装 `pipewire-pulse`）时的正确路径
3. `ffmpeg -f pulse` —— 兜底

**采集源的写法在这两种环境下不一样，这是实测踩出来的（Ubuntu 24.04 + 纯 PipeWire 1.0.5）**：

| 环境 | 正确写法 | 用错会怎样 |
|---|---|---|
| 有 PA 兼容层（`parec`/`ffmpeg -f pulse`） | `@DEFAULT_MONITOR@`（PA 命令行工具的保留名，自动指向默认输出的监听源） | —— |
| 纯 PipeWire（只有 `pw-record`） | `-P '{ stream.capture.sink = true }'`，**不要**给 `--target` | `@DEFAULT_MONITOR@` 解析不了，`pw-record` 会**静默连到默认源（麦克风）**，而进程照样正常出数据 —— 你会在毫不知情的情况下采到麦克风 |

纯 PipeWire 上 `<sink>.monitor` 这种节点名通常**不存在**（WirePlumber 没建），所以也别指望它。
`sink 名 + stream.capture.sink` 与 `@DEFAULT_SINK@ + stream.capture.sink` 都可以；
用 `--device` 可以显式指定源（给 `xxx.monitor` 形式的名字时按普通源连）。

另外：**「命令起来了」不等于「真的连上了采集源」**。所以启动后会等最多 2 秒确认有样本进来，
没等到就如实报 `unavailable` 并给出排查建议 —— 把「静默采不到」变成一句明确的话。

### 4.3 刻意不提供的功能

| 功能 | 为什么不做 |
|---|---|
| 静音 | MPRIS 没有静音概念；把音量设 0 再恢复无法还原原音量 |

## 5. 验证状态（诚实记录）

### 已在 macOS 26.x / darwin 27.0.0 / arm64 实测通过

| 项目 | 证据 |
|---|---|
| 依赖与全量测试 | 92 个单元测试 + 4 个 CLI 测试 + 3 个 stdio 集成测试 + 3 个 HTTP 集成测试 全绿 |
| MediaRemote `dlopen`/`dlsym` | 9/9 符号与键解析成功（`examples/ffi_smoke.rs`） |
| 元数据字典解析 | 用**真 CoreFoundation 类型**构造字典走真解析函数（这条测试当场抓出一个「键多解一层引用」的真 bug） |
| 能力位推导 | 用真 `NSNumber` 数组（媒体命令的真实形态）验证 0/1/2/4/6/7 → 能力位映射 |
| 进程 tap + 聚合设备 | 创建/销毁成功（`ffi_smoke`） |
| **系统音频采集端到端** | 静音 → 数字零（rms 0.0000、全 0 段）；放 1kHz 测试音 → **每一帧峰值都精确落在第 26 段**（= 1kHz @ 16kHz 分析）且频谱只有那一段亮；停掉后回到零。实时接口与录成 WAV 离线分析两条路都验过 |
| HTTP 端到端 | `curl` 验过 `/health` `/v1/now` `/v1/control` `/v1/spectrum` `/v1/artwork`（返回真 PNG，`content-type: image/png`）与 SSE |
| 反向控制 | 在假播放器（mock）上完整往返：13 种命令都验过「发出去 → 回执 applied=true → 状态真的变了」 |

### 已在 Linux（Ubuntu 24.04.3 LTS / aarch64 / Parallels 虚拟机 / 原生 Rust 1.98.1）实测通过

把项目同步进虚拟机后**原生编译并运行**，不是交叉编译：

| 项目 | 证据 |
|---|---|
| 测试 | **99 个测试原生通过**（89 单元 + 4 CLI + 3 stdio 集成 + 3 HTTP 集成），0 失败、0 警告 |
| MPRIS 元数据 | 用 `examples/fake_mpris.rs`（注册在真会话总线上的 MPRIS 服务）验证：标题/歌手/专辑/时长/进度/循环/随机全部读到，能力位 13 项全 true |
| **MPRIS 反向控制** | 13 条命令逐个走通，且**播放器自己的日志证明收到了**：`CMD next track=1`、`CMD seek offset=30000000us → 30.4s`、`CMD set-position to=60.0s`、`CMD loop-status Track`、`CMD shuffle true`、`CMD volume 0.420`、`CMD rate 1.500` |
| 歌词（sidecar） | 播放器给 `xesam:url=file:///tmp/mbtest/song.flac`，同目录 `song.lrc` 被读到 |
| 歌词（内嵌） | `--features embedded` 下从 FLAC 的 `LYRICS` 标签读到带时间轴的歌词 |
| 内嵌封面 | 播放器不给封面时，从文件的内嵌 PNG 读到（120×120，`origin=embedded`） |
| 系统音频 | 纯 PipeWire 下采到系统输出：放 1kHz 测试音 → **每帧峰值都落在第 26 段**；静音 → 数字零（峰值 0.0000、RMS 0.0000） |
| HTTP / SSE | `/health` `/v1/now` `/v1/status` `/v1/spectrum` `/v1/artwork`（真 PNG）`POST /v1/control` 全部验过 |

**这一轮真机测试还抓出并修掉了两个真问题**（都是三平台共有的服务层 bug，以及一个 Linux 专属坑）：

1. **并发 apply 导致歌词/封面偶发丢失**：轮询任务与「显式 refresh / 控制命令后的刷新」会交叠，
   一个任务标记了「已处理」但还没把结果写进状态，另一个看到标记就跳过 —— 症状是
   **歌词 5 次里丢 1 次**（修好后 30/30），封面同理。修法：`apply()` 串行化 +
   本地歌词每轮都解析（带只缓存命中的备忘）+ 「已有内容永不降级」。
2. **纯 PipeWire 上采到了麦克风而不是系统输出**（见 §4.2）：命令正常出数据、状态显示 Running，
   但内容是错的。修法：`stream.capture.sink = true` + 启动后确认有数据流入。

### 已在 Windows（Windows 11 ARM64 / build 26200 / Parallels 虚拟机 / 原生 Rust 1.98.1）实测通过

工具链说明：虚拟机里没有 MSVC，所以用 rustup 的 **`x86_64-pc-windows-gnu`**（自带 MinGW 链接器，
x64 进程在 ARM 上通过模拟运行）。`windows-sys` 的构建脚本还需要 `dlltool`，而 rustup 的 `rust-mingw`
组件已不再附带 binutils —— 补一个 MinGW-w64（winlibs，解压即用）即可。**在真实 Windows 上构建只需要
「rustup 装 gnu 工具链 + 一个 MinGW-w64」**，不需要 Visual Studio。

| 项目 | 证据 |
|---|---|
| 测试 | **97 个测试原生通过**（87 单元 + 4 CLI + 3 stdio 集成 + 3 HTTP 集成），0 失败 —— 含真起子进程的 stdio/HTTP 集成测试 |
| GSMTC 元数据 | 用系统自带的 Media Player（ZuneMusic）播放一个带 ID3 标签的 MP3：标题/歌手/专辑**逐字断言相等**，时长 30066ms，应用标识 `Microsoft.ZuneMusic_8wekyb3d8bbwe!…`，进度在走 |
| 能力位 | 精准反映该播放器的真实状态：本次是 `play toggle seek seek-by loop rate`（Media Player 不报 next/prev/shuffle，UI 就该置灰它们） |
| **GSMTC 反向控制** | `set-loop track` → 播放器回读 `loopMode=track`；`seek-by +8s` → 位置跳到 8.0s；`pause` → 状态变 `paused`；`play` → 回到 `playing`；`set-rate` / `next` 均被接受 |
| WASAPI loopback 采集 | 播 1kHz（就是那个 MP3）→ **每帧峰值都落在第 26 段**（peak=144、rms=0.084）；暂停播放 → **peak=0、rms=0.0** |
| HTTP / stdio | `/health` `/v1/now` `/v1/control` `/v1/spectrum` 全部验过；stdio 与 HTTP 的**集成测试在这个平台上原生跑通** |

**这一轮在 Windows 真机上抓出并修掉了三个 bug**（都是只有真机才会暴露的）：

1. **中文元数据乱码（影响所有 Windows 脚本宿主）**：HTTP 应答的 `content-type` 没带 `charset`，
   而 **PowerShell 5.1 的 `Invoke-RestMethod` 在缺 charset 时按 Latin-1 解码 JSON** ——
   中文标题会整片变成乱码（实测把 `测` 拆成三个字符）。修法：所有 JSON/SSE 应答显式带
   `charset=utf-8`（并加了回归断言）。Node/浏览器/Python 不受影响，但 Windows 侧的脚本宿主会。
2. **`diagnose` 在 Windows 上 panic**：`sync_snapshot()` 用了 tokio 的 `blocking_recv()`，
   而 `diagnose` 是在异步运行时线程上被调用的 → `Cannot block the current thread from within a runtime`。
   改成 `std::sync::mpsc` 通道（不依赖运行时的同步等待）。`now`/`control` 走异步路径，所以只有诊断命令会崩。
3. **应用名解析顺序写反**：`friendly_app_name("Spotify.exe")` 会先按 `.` 切最后一段得到 `exe`。
   这个函数在 macOS 上根本不会被编译（`cfg(target_os = "windows")`），所以**只有真机跑测试才会发现** ——
   这也说明「交叉编译检查通过」远不等于「代码是对的」。

交叉检查用的是**默认 feature + `embedded`**（都是纯 Rust）。
`--all-features` 加不上：`online` 依赖的 `ureq` 会拉进 `ring`，而 `ring` 的构建脚本需要
**目标平台**的 C 工具链，在 macOS 上交叉编译到 Windows/Linux 会失败 —— 这是构建工具链的限制，
不是代码问题（在目标平台上原生构建 `--all-features` 正常）。

**Windows 的现状**：代码路径语法与类型正确、API 用法与系统绑定一致，
但**没有在真实系统上跑过**。要到「实测」需要在那台机器上做三件事：

```bash
cargo test                                     # 单元 + 集成
media-bridge diagnose                          # 看数据源是否就绪
# 播一段测试音，验证采集：
ffplay -f lavfi -i "sine=frequency=1000:duration=10"
media-bridge capture 3 --pcm --out /tmp/t.wav
media-bridge spectrum --wav /tmp/t.wav         # 峰值应落在第 26 段
media-bridge now && media-bridge control next  # 元数据 + 反向控制
```

### 三平台通用的自检 / 复现方法

```bash
# 一条命令跑完：本机测试 + 交叉检查（+ 传 real 参数则再做采集复核）
scripts/verify.sh [real]

# 手动看数据源状态（会逐条说明「拿到了什么、差什么」）
media-bridge diagnose

# Linux：起一个真的 MPRIS 服务当被测对象（它的日志就是「命令真的收到」的证据）
cargo run --example fake_mpris -- --title 测试曲 --artist 测试歌手 --audio-file /path/song.flac
media-bridge now && media-bridge control next
```

### macOS 上无法验证的一项：合成一个假的「系统媒体会话」

为了在不打扰用户音乐软件的前提下验证**反向控制的真实路由**，尝试过让一个辅助进程
注册成 now-playing 会话（`examples/fake_player.rs`），三种办法都失败：

1. 公开 API（`MPNowPlayingInfoCenter` + `MPRemoteCommandCenter`）—— 系统媒体面板里看不到；
2. 私有 setter（`MRMediaRemoteSetNowPlayingInfo` + `MRMediaRemoteSetCanBeNowPlayingApplication`）—— 同样不认；
3. 打包成 `.app` 并经 LaunchServices（`open -a`）启动 —— 仍然不认。

结论：**macOS 26 起，非签名的辅助进程无法被 MediaRemote 认领为媒体会话**。
所以「`MRMediaRemoteSendCommand` 发出去之后系统是否路由到播放器」这一环，
只能拿真实播放器验证（本机当时没有正在播放的媒体，也没有为此去启动用户的音乐软件）。
中间件侧的准备是完整的：符号已解析、命令按能力位门控、回执带 `applied`/`reason`，
`examples/fake_player.rs` 也保留着 —— 一旦在真机上放一首歌，它会立刻打印出收到的每条命令，
可以作为「播放器真的收到了」的观察点。

## 6. 已知限制汇总

| 限制 | 影响 | 有没有绕法 |
|---|---|---|
| macOS 无 `albumArtist` | 该字段恒空 | 无（框架没这个键） |
| macOS/Windows 拿不到本地文件路径 | 内嵌标签、同目录 `.lrc` 用不上 | 用缓存目录里的 `.lrc`，或开 `online` 走在线歌词 |
| `.lrc` 按 UTF-8 解码 | GBK 老资源会乱码 | 转码即可：`iconv -f GBK -t UTF-8 x.lrc > y.lrc` |
| Linux 没装任何采集工具 | 采集不可用（仍返回全 0 频谱） | 装 `pulseaudio-utils` / `pipewire-bin` / `ffmpeg` 任一 |
| macOS 首次采集需授权 | 未授权时 `state=denied` | 系统设置 → 隐私与安全性 → 音频录制 |
| Windows/Linux 未实测 | 见上 | 按上面的清单跑一遍 |
