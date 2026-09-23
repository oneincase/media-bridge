//! 假播放器：把自己注册成**一个真实的 macOS 系统媒体会话**，用来验证中间件的读写两端。
//!
//! 为什么需要它：真机验证反向控制时，不能拿用户的音乐软件做实验（会打断人家在听的歌），
//! 而「发出去的命令到底有没有被播放器收到」又必须有个确定的观察点。这个程序用公开 API
//! （`MPNowPlayingInfoCenter` + `MPRemoteCommandCenter`）注册成一个正常的媒体会话：
//!
//!   - 中间件读到的元数据 / 进度，就是它写进去的那些值；
//!   - 中间件发出的每条传输控制命令，都会打到它的命令处理器上，**原样打印出来**。
//!
//! 于是「中间件说生效了」和「播放器真的收到了」这两件事能对上。
//!
//! 用法：
//! ```text
//! cargo run --example fake_player -- --title "测试曲目" --artist "测试歌手" --duration 200
//! ```
//! 跑起来后会打印 `READY`，之后每个收到的命令打印一行 `CMD <名称> <参数>`。
//! Ctrl-C 退出（退出时清掉 now-playing，不给系统留残留会话）。

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("fake_player 只在 macOS 上有意义（它通过 MediaPlayer 框架注册系统媒体会话）");
}

#[cfg(target_os = "macos")]
fn main() {
    mac::run();
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{CString, c_char, c_int, c_void};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use block2::RcBlock;
    use objc2::msg_send;
    use objc2::rc::autoreleasepool;
    use objc2::runtime::{AnyClass, AnyObject};

    #[link(name = "System", kind = "dylib")]
    unsafe extern "C" {
        fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, sym: *const c_char) -> *mut c_void;
        fn signal(sig: c_int, handler: extern "C" fn(c_int)) -> usize;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after: u8) -> i32;
        static kCFRunLoopDefaultMode: *const c_void;
        // 私有 setter 需要手工构造 CFDictionary（MediaRemote 的键是 CFString 全局量）
        fn CFDictionaryCreateMutable(
            alloc: *const c_void,
            capacity: isize,
            key_cb: *const c_void,
            value_cb: *const c_void,
        ) -> *mut c_void;
        fn CFDictionarySetValue(dict: *mut c_void, key: *const c_void, value: *const c_void);
        fn CFStringCreateWithCString(alloc: *const c_void, s: *const c_char, enc: u32) -> *mut c_void;
        fn CFNumberCreate(alloc: *const c_void, t: isize, value: *const c_void) -> *mut c_void;
        fn CFRelease(cf: *const c_void);
        static kCFTypeDictionaryKeyCallBacks: c_void;
        static kCFTypeDictionaryValueCallBacks: c_void;
    }
    const CF_UTF8: u32 = 0x0800_0100;
    /// `kCFNumberFloat64Type`
    const CF_NUMBER_FLOAT64: isize = 13;

    const RTLD_NOW: c_int = 0x2;
    const SIGINT: c_int = 2;
    const SIGTERM: c_int = 15;

    /// `MPNowPlayingPlaybackState`
    const STATE_PLAYING: usize = 1;
    const STATE_PAUSED: usize = 2;
    const STATE_STOPPED: usize = 3;

    /// 播放器状态：命令处理器改它，主循环把它写回 now-playing。
    pub struct Player {
        title: String,
        artist: String,
        album: String,
        duration_s: f64,
        playing: AtomicBool,
        /// 位置锚点（毫秒，整数存）
        anchor_elapsed_ms: AtomicI64,
        anchor_at: Mutex<Instant>,
        /// 第几首（用来验证「换曲」：下一曲会换标题）
        track_no: AtomicUsize,
        commands: AtomicUsize,
        /// 可选：把输出也写一份到文件（`open -a` 启动时终端拿不到 stdout）
        log_path: Option<String>,
    }

    impl Player {
        fn elapsed_s(&self) -> f64 {
            let base = self.anchor_elapsed_ms.load(Ordering::Relaxed) as f64 / 1000.0;
            if !self.playing.load(Ordering::Relaxed) {
                return base;
            }
            let anchor = self.anchor_at.lock().map(|g| *g).unwrap_or_else(|_| Instant::now());
            (base + anchor.elapsed().as_secs_f64()).min(self.duration_s)
        }

        /// 改位置/改播放态后重新打锚点（真实播放器也是这个模型）。
        fn reanchor(&self, elapsed_s: f64) {
            self.anchor_elapsed_ms
                .store((elapsed_s * 1000.0) as i64, Ordering::Relaxed);
            if let Ok(mut g) = self.anchor_at.lock() {
                *g = Instant::now();
            }
        }

        fn set_playing(&self, playing: bool) {
            let now = self.elapsed_s();
            self.reanchor(now);
            self.playing.store(playing, Ordering::Relaxed);
        }

        /// 当前曲目标题（下一曲会带序号，用于验证换曲判定）。
        fn current_title(&self) -> String {
            let n = self.track_no.load(Ordering::Relaxed);
            if n == 0 {
                self.title.clone()
            } else {
                format!("{} #{}", self.title, n)
            }
        }

        fn log_cmd(&self, what: &str, detail: &str) {
            self.commands.fetch_add(1, Ordering::Relaxed);
            let line = if detail.is_empty() {
                format!("CMD {what}")
            } else {
                format!("CMD {what} {detail}")
            };
            self.emit(&line);
        }

        /// 同时打到 stdout 与（若配置了）日志文件。
        fn emit(&self, line: &str) {
            println!("{line}");
            use std::io::Write;
            let _ = std::io::stdout().flush();
            if let Some(path) = &self.log_path {
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                    let _ = writeln!(f, "{line}");
                }
            }
        }
    }

    static PLAYER: std::sync::OnceLock<&'static Player> = std::sync::OnceLock::new();

    fn current_player() -> Option<&'static Player> {
        PLAYER.get().copied()
    }

    pub fn run() {
        let player: &'static Player = Box::leak(Box::new(parse_args()));
        let _ = PLAYER.set(player);

        extern "C" fn on_signal(_: c_int) {
            let n = current_player().map(|p| p.commands.load(Ordering::Relaxed)).unwrap_or(0);
            cleanup();
            println!("BYE commands={n}");
            std::process::exit(0);
        }
        unsafe {
            signal(SIGINT, on_signal);
            signal(SIGTERM, on_signal);
        }

        // **先 dlopen 框架**：objc 的类查找只在「已经加载的镜像」里搜，不先加载框架的话
        // MPRemoteCommandCenter / MPNowPlayingInfoCenter 都会找不到（而且不报错，只是静默失效）。
        unsafe {
            match dlopen_media_player() {
                Some(_) => {}
                None => eprintln!("⚠️ 加载 MediaPlayer.framework 失败"),
            }
            // MediaRemote 只认「应用」的媒体会话。命令行进程先走一遍 AppKit 的启动流程，
            // 把自己变成一个没有 Dock 图标的附件应用（LSUIElement / Accessory）。
            if let Some(framework) = dlopen_framework("/System/Library/Frameworks/AppKit.framework/AppKit") {
                let _ = framework;
                if let Some(app_cls) = AnyClass::get(c"NSApplication") {
                    let app: *mut AnyObject = msg_send![app_cls, sharedApplication];
                    // NSApplicationActivationPolicyAccessory = 1
                    let _: bool = msg_send![app, setActivationPolicy: 1isize];
                    let _: () = msg_send![app, finishLaunching];
                    player.emit("APKIT sharedApplication ok");
                } else {
                    eprintln!("⚠️ 找不到 NSApplication");
                }
            }
            player.emit(&format!(
                "CLASSES infoCenter={} commandCenter={}",
                AnyClass::get(c"MPNowPlayingInfoCenter").is_some(),
                AnyClass::get(c"MPRemoteCommandCenter").is_some()
            ));
            player.emit(&format!(
                "PRIVATE canBeNowPlaying={}",
                load_private_bool("MRMediaRemoteSetCanBeNowPlayingApplication").is_some()
            ));
            use std::io::Write;
            let _ = std::io::stdout().flush();

            register_commands();
            publish(player);
            // 让系统把我们当成「可以成为 now-playing 的进程」：命令行程序有时需要这一步
            // （拿不到这个私有符号也无妨，公开路径通常也够）
            if let Some(f) = load_private_bool("MRMediaRemoteSetCanBeNowPlayingApplication") {
                f(true);
            }
        }

        player.emit(&format!(
            "READY title={} artist={} album={} duration={}s",
            player.title, player.artist, player.album, player.duration_s
        ));
        use std::io::Write;
        let _ = std::io::stdout().flush();

        // 主循环：跑 runloop 让命令能递达；每 500ms 把最新位置写回 now-playing
        let mut last_publish = Instant::now();
        loop {
            unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.1, 0) };
            if last_publish.elapsed() >= Duration::from_millis(500) {
                last_publish = Instant::now();
                unsafe { publish(player) };
            }
        }
    }

    /// 写 now-playing：**优先走私有 setter**，同时仍然设一遍公开 API。
    ///
    /// 为什么：macOS 26 上实测「只用 MPNowPlayingInfoCenter」不会被 MediaRemote 认领
    /// （系统媒体面板里看不到），而 `MRMediaRemoteSetNowPlayingInfo` 才是播放器内部
    /// 真正调用的那个 —— 用框架自己导出的键 + 这个 setter，系统就会认。
    unsafe fn publish(player: &Player) {
        unsafe {
            publish_private(player);
            publish_public(player);
        }
    }

    /// 私有 setter：用 MediaRemote 自己导出的 `kMRMediaRemoteNowPlayingInfo*` 键。
    unsafe fn publish_private(player: &Player) {
        unsafe {
            let path =
                CString::new("/System/Library/PrivateFrameworks/MediaRemote.framework/MediaRemote")
                    .unwrap_or_default();
            let handle = dlopen(path.as_ptr(), RTLD_NOW);
            if handle.is_null() {
                return;
            }
            let sym = |name: &str| -> *mut c_void {
                let cs = CString::new(name).unwrap_or_default();
                dlsym(handle, cs.as_ptr())
            };
            let set_info = sym("MRMediaRemoteSetNowPlayingInfo");
            if set_info.is_null() {
                return;
            }
            let key = |name: &str| -> *const c_void {
                let p = sym(name);
                if p.is_null() {
                    return std::ptr::null();
                }
                *(p as *const *const c_void)
            };
            let cfstr = |s: &str| -> *mut c_void {
                let cs = CString::new(s).unwrap_or_default();
                CFStringCreateWithCString(std::ptr::null(), cs.as_ptr(), CF_UTF8)
            };
            let cfnum = |v: f64| -> *mut c_void {
                CFNumberCreate(
                    std::ptr::null(),
                    CF_NUMBER_FLOAT64,
                    &v as *const f64 as *const c_void,
                )
            };

            let dict = CFDictionaryCreateMutable(
                std::ptr::null(),
                8,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            );
            let put = |k: *const c_void, v: *mut c_void| {
                if !k.is_null() && !v.is_null() {
                    CFDictionarySetValue(dict, k, v);
                }
            };
            put(key("kMRMediaRemoteNowPlayingInfoTitle"), cfstr(&player.current_title()));
            put(key("kMRMediaRemoteNowPlayingInfoArtist"), cfstr(&player.artist));
            put(key("kMRMediaRemoteNowPlayingInfoAlbum"), cfstr(&player.album));
            put(
                key("kMRMediaRemoteNowPlayingInfoDuration"),
                cfnum(player.duration_s),
            );
            put(
                key("kMRMediaRemoteNowPlayingInfoElapsedTime"),
                cfnum(player.elapsed_s()),
            );
            let rate = if player.playing.load(Ordering::Relaxed) { 1.0 } else { 0.0 };
            put(key("kMRMediaRemoteNowPlayingInfoPlaybackRate"), cfnum(rate));
            put(
                key("kMRMediaRemoteNowPlayingInfoContentItemIdentifier"),
                cfstr(&format!("mb-test:{}", player.track_no.load(Ordering::Relaxed))),
            );

            let f: unsafe extern "C" fn(*const c_void) = std::mem::transmute(set_info);
            f(dict);
            CFRelease(dict);
        }
    }

    /// 公开 API 路径（对照用；系统不一定会认，但设了无害）。
    unsafe fn publish_public(player: &Player) {
        unsafe {
            let Some(center_cls) = AnyClass::get(c"MPNowPlayingInfoCenter") else {
                return;
            };
            let center: *mut AnyObject = msg_send![center_cls, defaultCenter];
            if center.is_null() {
                return;
            }
            if let Some(dict) = build_info_dict(player) {
                let _: () = msg_send![center, setNowPlayingInfo: dict];
                let _: *mut AnyObject = msg_send![dict, release];
            }
            let state = if player.playing.load(Ordering::Relaxed) {
                STATE_PLAYING
            } else {
                STATE_PAUSED
            };
            let _: () = msg_send![center, setPlaybackState: state];
        }
    }

    /// 造 now-playing 字典。键来自 MediaPlayer 框架导出的 NSString 常量（dlsym 取，
    /// 不手抄字符串值）。
    unsafe fn build_info_dict(player: &Player) -> Option<*mut AnyObject> {
        unsafe {
            let nsstring_cls = AnyClass::get(c"NSString")?;
            let nsnumber_cls = AnyClass::get(c"NSNumber")?;
            let dict_cls = AnyClass::get(c"NSMutableDictionary")?;
            let handle = dlopen_media_player()?;

            let key = |name: &str| -> *mut AnyObject {
                let cs = CString::new(name).unwrap_or_default();
                let p = dlsym(handle, cs.as_ptr());
                if p.is_null() {
                    return std::ptr::null_mut();
                }
                *(p as *const *mut AnyObject)
            };
            let nsstr = |s: &str| -> *mut AnyObject {
                let cs = CString::new(s).unwrap_or_default();
                msg_send![nsstring_cls, stringWithUTF8String: cs.as_ptr()]
            };
            let nsnum = |v: f64| -> *mut AnyObject { msg_send![nsnumber_cls, numberWithDouble: v] };

            let dict: *mut AnyObject = msg_send![dict_cls, new];
            if dict.is_null() {
                return None;
            }
            unsafe fn put(dict: *mut AnyObject, k: *mut AnyObject, v: *mut AnyObject) {
                if !k.is_null() && !v.is_null() {
                    let _: () = msg_send![dict, setObject: v, forKey: k];
                }
            }
            put(dict, key("MPMediaItemPropertyTitle"), nsstr(&player.current_title()));
            put(dict, key("MPMediaItemPropertyArtist"), nsstr(&player.artist));
            put(dict, key("MPMediaItemPropertyAlbumTitle"), nsstr(&player.album));
            put(dict, key("MPMediaItemPropertyPlaybackDuration"), nsnum(player.duration_s));
            put(dict, key("MPNowPlayingInfoPropertyElapsedPlaybackTime"), nsnum(player.elapsed_s()));
            let rate = if player.playing.load(Ordering::Relaxed) { 1.0 } else { 0.0 };
            put(dict, key("MPNowPlayingInfoPropertyPlaybackRate"), nsnum(rate));
            put(dict, key("MPNowPlayingInfoPropertyDefaultPlaybackRate"), nsnum(1.0));
            Some(dict)
        }
    }

    /// `MPRemoteCommandCenter` 里我们要接的命令。
    #[derive(Clone, Copy)]
    enum Cmd {
        Play,
        Pause,
        Toggle,
        Stop,
        Next,
        Previous,
        SeekPosition,
        Repeat,
        Shuffle,
        Rate,
    }

    unsafe fn command_ptr(cc: *mut AnyObject, which: Cmd) -> *mut AnyObject {
        unsafe {
            match which {
                Cmd::Play => msg_send![cc, playCommand],
                Cmd::Pause => msg_send![cc, pauseCommand],
                Cmd::Toggle => msg_send![cc, togglePlayPauseCommand],
                Cmd::Stop => msg_send![cc, stopCommand],
                Cmd::Next => msg_send![cc, nextTrackCommand],
                Cmd::Previous => msg_send![cc, previousTrackCommand],
                Cmd::SeekPosition => msg_send![cc, changePlaybackPositionCommand],
                Cmd::Repeat => msg_send![cc, changeRepeatModeCommand],
                Cmd::Shuffle => msg_send![cc, changeShuffleModeCommand],
                Cmd::Rate => msg_send![cc, changePlaybackRateCommand],
            }
        }
    }

    unsafe fn register_commands() {
        unsafe {
            let Some(cc_cls) = AnyClass::get(c"MPRemoteCommandCenter") else {
                eprintln!("⚠️ 找不到 MPRemoteCommandCenter");
                return;
            };
            let cc: *mut AnyObject = msg_send![cc_cls, sharedCommandCenter];
            if cc.is_null() {
                eprintln!("⚠️ sharedCommandCenter 为 nil");
                return;
            }

            let mut count = 0;
            for which in [
                Cmd::Play,
                Cmd::Pause,
                Cmd::Toggle,
                Cmd::Stop,
                Cmd::Next,
                Cmd::Previous,
                Cmd::SeekPosition,
                Cmd::Repeat,
                Cmd::Shuffle,
                Cmd::Rate,
            ] {
                let cmd = command_ptr(cc, which);
                if cmd.is_null() {
                    continue;
                }
                let _: () = msg_send![cmd, setEnabled: true];
                let handler: RcBlock<dyn Fn(*mut AnyObject) -> isize> =
                    RcBlock::new(move |ev: *mut AnyObject| -> isize {
                        autoreleasepool(|_| handle_command(which, ev))
                    });
                let _: *mut AnyObject = msg_send![cmd, addTargetWithHandler: RcBlock::as_ptr(&handler)];
                // 命令中心会 retain 这个 block；这里再故意漏一份引用，确保即使某个系统版本
                // 不 retain 也不会用到已释放的 block（进程生命周期内几十字节，无所谓）
                std::mem::forget(handler);
                count += 1;
            }
            let _ = count;
        }
    }

    /// 命令处理器：改状态 + 打印（打印出来的就是「播放器真的收到了」的证据）。
    fn handle_command(which: Cmd, ev: *mut AnyObject) -> isize {
        let Some(p) = current_player() else {
            return 0;
        };
        unsafe {
            match which {
                Cmd::Play => {
                    p.set_playing(true);
                    p.log_cmd("play", "");
                }
                Cmd::Pause => {
                    p.set_playing(false);
                    p.log_cmd("pause", "");
                }
                Cmd::Toggle => {
                    let now = !p.playing.load(Ordering::Relaxed);
                    p.set_playing(now);
                    p.log_cmd("toggle", &format!("playing={now}"));
                }
                Cmd::Stop => {
                    p.set_playing(false);
                    p.reanchor(0.0);
                    p.log_cmd("stop", "");
                }
                Cmd::Next => {
                    p.reanchor(0.0);
                    let n = p.track_no.fetch_add(1, Ordering::Relaxed) + 1;
                    p.log_cmd("next", &format!("track={n}"));
                }
                Cmd::Previous => {
                    p.reanchor(0.0);
                    p.log_cmd("previous", "");
                }
                Cmd::SeekPosition => {
                    let t: f64 = if ev.is_null() { 0.0 } else { msg_send![ev, positionTime] };
                    p.reanchor(t);
                    p.log_cmd("seek", &format!("to={t:.3}s"));
                }
                Cmd::Repeat => {
                    let m: isize = if ev.is_null() { -1 } else { msg_send![ev, mode] };
                    p.log_cmd("repeat", &format!("mode={m}"));
                }
                Cmd::Shuffle => {
                    let t: isize = if ev.is_null() { -1 } else { msg_send![ev, shuffleType] };
                    p.log_cmd("shuffle", &format!("type={t}"));
                }
                Cmd::Rate => {
                    let r: f32 = if ev.is_null() { 0.0 } else { msg_send![ev, playbackRate] };
                    p.log_cmd("rate", &format!("rate={r}"));
                }
            }
        }
        0
    }

    fn parse_args() -> Player {
        let argv: Vec<String> = std::env::args().skip(1).collect();
        let mut title = "media-bridge 测试曲目".to_string();
        let mut artist = "media-bridge 测试歌手".to_string();
        let mut album = "中间件验证".to_string();
        let mut duration = 200.0f64;
        let mut log_path: Option<String> = std::env::var("MB_FAKE_PLAYER_LOG").ok();
        let mut i = 0;
        while i < argv.len() {
            let take = |i: &mut usize| -> Option<String> {
                *i += 1;
                argv.get(*i).cloned()
            };
            match argv[i].as_str() {
                "--title" => {
                    if let Some(v) = take(&mut i) {
                        title = v;
                    }
                }
                "--artist" => {
                    if let Some(v) = take(&mut i) {
                        artist = v;
                    }
                }
                "--album" => {
                    if let Some(v) = take(&mut i) {
                        album = v;
                    }
                }
                "--duration" => {
                    if let Some(v) = take(&mut i).and_then(|s| s.parse().ok()) {
                        duration = v;
                    }
                }
                "--log" => {
                    log_path = take(&mut i);
                }
                other => eprintln!("忽略未知参数 {other}"),
            }
            i += 1;
        }
        Player {
            title,
            artist,
            album,
            duration_s: duration,
            playing: AtomicBool::new(true),
            anchor_elapsed_ms: AtomicI64::new(0),
            anchor_at: Mutex::new(Instant::now()),
            track_no: AtomicUsize::new(0),
            commands: AtomicUsize::new(0),
            log_path,
        }
    }

    fn cleanup() {
        unsafe {
            let Some(cls) = AnyClass::get(c"MPNowPlayingInfoCenter") else {
                return;
            };
            let center: *mut AnyObject = msg_send![cls, defaultCenter];
            if !center.is_null() {
                let _: () = msg_send![center, setNowPlayingInfo: std::ptr::null_mut::<AnyObject>()];
                let _: () = msg_send![center, setPlaybackState: STATE_STOPPED];
            }
            if let Some(f) = load_private_bool("MRMediaRemoteSetCanBeNowPlayingApplication") {
                f(false);
            }
        }
    }

    fn dlopen_media_player() -> Option<*mut c_void> {
        dlopen_framework("/System/Library/Frameworks/MediaPlayer.framework/MediaPlayer")
    }

    fn dlopen_framework(path: &str) -> Option<*mut c_void> {
        let path = CString::new(path).ok()?;
        let h = unsafe { dlopen(path.as_ptr(), RTLD_NOW) };
        if h.is_null() { None } else { Some(h) }
    }

    /// 从私有框架取一个 `void(BOOL)` 形状的函数。
    fn load_private_bool(name: &str) -> Option<unsafe extern "C" fn(bool)> {
        let path =
            CString::new("/System/Library/PrivateFrameworks/MediaRemote.framework/MediaRemote").ok()?;
        let h = unsafe { dlopen(path.as_ptr(), RTLD_NOW) };
        if h.is_null() {
            return None;
        }
        let cs = CString::new(name).ok()?;
        let p = unsafe { dlsym(h, cs.as_ptr()) };
        if p.is_null() {
            return None;
        }
        // 签名不确定（void 或 Boolean）时按 void 调：返回值忽略，ABI 上安全
        Some(unsafe { std::mem::transmute(p) })
    }
}
