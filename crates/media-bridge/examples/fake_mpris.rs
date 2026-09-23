//! 假 MPRIS 播放器（Linux 专用测试替身）。
//!
//! 把 `org.mpris.MediaPlayer2.mbtest` 注册到**会话总线**上，实现 `Player` 接口里中间件会用到的
//! 那一部分。用途与 macOS 的 `fake_player.rs` 相同，但 Linux 上这条路是通的（D-Bus 不像
//! MediaRemote 那样要求进程被系统「认领」），所以能真正验证到底：
//!
//!   - 中间件读到的元数据/进度，就是这里写进去的值；
//!   - 中间件发出的每条控制命令都会打到这里的对应方法上，**原样打印**（`CMD <动作> <参数>`）；
//!   - `xesam:url` 指向一个真实存在的本地文件，所以还能顺带验证「本地文件 → 同目录 .lrc」
//!     与 `--features embedded` 的内嵌标签读取。
//!
//! 用法：
//! ```text
//! # 需要有会话总线（桌面会话里天然有；无头的用 dbus-run-session 包一层）
//! cargo run --example fake_mpris -- --title "测试曲目" --artist "测试歌手" \
//!     --audio-file /tmp/mbtest/song.flac --duration 210
//! ```
//! 起好之后会打印 `READY bus=…`；Ctrl-C 退出（会自行断开总线名）。
//!
//! 说明：真实播放器在状态变化时会发 `PropertiesChanged` 信号；这里**刻意不发** ——
//! 中间件是按秒轮询的（`docs/PROTOCOL.md` 里写了为什么），不发信号正好也能验证轮询这条路。

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("fake_mpris 只在 Linux 上有意义（它通过 D-Bus 注册 MPRIS 服务）");
}

#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
    use std::sync::Mutex;
    use std::time::Instant;

    use zbus::interface;
    use zbus::zvariant::{ObjectPath, OwnedValue, Value};

    const TRACK_ID: &str = "/org/mpris/MediaPlayer2/mbtest/track/1";

    pub struct Player {
        title: String,
        artist: String,
        album: String,
        duration_us: i64,
        /// 可选：本地音频文件（填进 `xesam:url`，用来验证 sidecar 歌词与内嵌标签）
        audio_file: Option<String>,
        /// 是否在播
        playing: AtomicBool,
        /// 位置锚点（微秒）与锚点时刻
        anchor_pos_us: AtomicI64,
        anchor_at: Mutex<Instant>,
        loop_status: Mutex<String>,
        shuffle: AtomicBool,
        volume: Mutex<f64>,
        rate: Mutex<f64>,
        commands: AtomicI64,
        /// 用例里想验证「换曲」时会调 `next`，标题带上序号
        track_no: AtomicI64,
    }

    impl Player {
        fn position_us(&self) -> i64 {
            let base = self.anchor_pos_us.load(Ordering::Relaxed);
            if !self.playing.load(Ordering::Relaxed) {
                return base;
            }
            let anchor = self.anchor_at.lock().map(|g| *g).unwrap_or_else(|_| Instant::now());
            let elapsed = anchor.elapsed().as_micros() as i64;
            (base + elapsed).min(self.duration_us)
        }

        fn reanchor(&self, pos_us: i64) {
            self.anchor_pos_us.store(pos_us.max(0), Ordering::Relaxed);
            if let Ok(mut g) = self.anchor_at.lock() {
                *g = Instant::now();
            }
        }

        fn set_playing(&self, playing: bool) {
            let now = self.position_us();
            self.reanchor(now);
            self.playing.store(playing, Ordering::Relaxed);
        }

        fn log_cmd(&self, what: &str, detail: &str) {
            self.commands.fetch_add(1, Ordering::Relaxed);
            if detail.is_empty() {
                println!("CMD {what}");
            } else {
                println!("CMD {what} {detail}");
            }
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }

        fn current_title(&self) -> String {
            let n = self.track_no.load(Ordering::Relaxed);
            if n == 0 {
                self.title.clone()
            } else {
                format!("{} #{}", self.title, n)
            }
        }
    }

    fn owned_string(s: &str) -> OwnedValue {
        OwnedValue::try_from(Value::from(s.to_string())).expect("OwnedValue::from(String)")
    }

    #[interface(name = "org.mpris.MediaPlayer2.Player")]
    impl Player {
        // ── 方法：控制 ────────────────────────────────────────────────────
        fn play(&self) {
            self.set_playing(true);
            self.log_cmd("play", "");
        }

        fn pause(&self) {
            self.set_playing(false);
            self.log_cmd("pause", "");
        }

        fn play_pause(&self) {
            let now = !self.playing.load(Ordering::Relaxed);
            self.set_playing(now);
            self.log_cmd("play-pause", &format!("playing={now}"));
        }

        fn stop(&self) {
            self.set_playing(false);
            self.reanchor(0);
            self.log_cmd("stop", "");
        }

        fn next(&self) {
            self.track_no.fetch_add(1, Ordering::Relaxed);
            self.reanchor(0);
            self.log_cmd("next", &format!("track={}", self.track_no.load(Ordering::Relaxed)));
        }

        fn previous(&self) {
            let n = self.track_no.load(Ordering::Relaxed);
            self.track_no.store((n - 1).max(0), Ordering::Relaxed);
            self.reanchor(0);
            self.log_cmd("previous", "");
        }

        /// MPRIS 的相对定位：单位微秒（正数快进）。
        fn seek(&self, offset: i64) {
            let target = (self.position_us() + offset).clamp(0, self.duration_us);
            self.reanchor(target);
            self.log_cmd("seek", &format!("offset={}us → {:.1}s", offset, target as f64 / 1e6));
        }

        /// MPRIS 的绝对定位：需要 trackid + 微秒。
        fn set_position(&self, track_id: ObjectPath<'_>, position: i64) {
            if track_id.as_str() != TRACK_ID {
                self.log_cmd("set-position", &format!("拒绝未知 trackid {track_id}"));
                return;
            }
            self.reanchor(position.clamp(0, self.duration_us));
            self.log_cmd("set-position", &format!("to={:.1}s", position as f64 / 1e6));
        }

        // ── 属性：状态 ────────────────────────────────────────────────────
        #[zbus(property)]
        fn playback_status(&self) -> String {
            if self.playing.load(Ordering::Relaxed) {
                "Playing".to_string()
            } else {
                "Paused".to_string()
            }
        }

        #[zbus(property)]
        fn position(&self) -> i64 {
            self.position_us()
        }

        #[zbus(property)]
        fn loop_status(&self) -> String {
            self.loop_status.lock().map(|g| g.clone()).unwrap_or_else(|_| "None".into())
        }

        #[zbus(property)]
        fn set_loop_status(&self, value: String) {
            self.log_cmd("loop-status", &format!("{value}"));
            if let Ok(mut g) = self.loop_status.lock() {
                *g = value;
            }
        }

        #[zbus(property)]
        fn shuffle(&self) -> bool {
            self.shuffle.load(Ordering::Relaxed)
        }

        #[zbus(property)]
        fn set_shuffle(&self, value: bool) {
            self.log_cmd("shuffle", &format!("{value}"));
            self.shuffle.store(value, Ordering::Relaxed);
        }

        #[zbus(property)]
        fn volume(&self) -> f64 {
            self.volume.lock().map(|g| *g).unwrap_or(1.0)
        }

        #[zbus(property)]
        fn set_volume(&self, value: f64) {
            self.log_cmd("volume", &format!("{value:.3}"));
            if let Ok(mut g) = self.volume.lock() {
                *g = value;
            }
        }

        #[zbus(property)]
        fn rate(&self) -> f64 {
            self.rate.lock().map(|g| *g).unwrap_or(1.0)
        }

        #[zbus(property)]
        fn set_rate(&self, value: f64) {
            self.log_cmd("rate", &format!("{value:.3}"));
            let now = self.position_us();
            self.reanchor(now);
            if let Ok(mut g) = self.rate.lock() {
                *g = value;
            }
        }

        // ── 属性：元数据 ──────────────────────────────────────────────────
        #[zbus(property)]
        fn metadata(&self) -> HashMap<String, OwnedValue> {
            let mut m: HashMap<String, OwnedValue> = HashMap::new();
            m.insert(
                "mpris:trackid".into(),
                OwnedValue::try_from(Value::ObjectPath(
                    ObjectPath::try_from(TRACK_ID).expect("trackid"),
                ))
                .expect("trackid → OwnedValue"),
            );
            m.insert("xesam:title".into(), owned_string(&self.current_title()));
            m.insert("xesam:artist".into(), owned_string(&self.artist));
            m.insert("xesam:album".into(), owned_string(&self.album));
            m.insert("xesam:albumArtist".into(), owned_string(&self.artist));
            m.insert("xesam:genre".into(), owned_string("Test"));
            m.insert(
                "xesam:trackNumber".into(),
                OwnedValue::try_from(Value::from(1i32)).expect("trackNumber"),
            );
            m.insert("mpris:length".into(), self.duration_us.into());
            if let Some(f) = &self.audio_file {
                let url = if f.starts_with("file://") {
                    f.clone()
                } else {
                    format!("file://{f}")
                };
                m.insert("xesam:url".into(), owned_string(&url));
            }
            m
        }

        // ── 属性：能力（中间件据此决定按钮禁用态）─────────────────────────
        #[zbus(property)]
        fn can_play(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_pause(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_go_next(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_go_previous(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_seek(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_control(&self) -> bool {
            true
        }
    }

    // 注意：**不要**再给 Player 加第二个 `#[interface]` 块（例如根接口
    // `org.mpris.MediaPlayer2`）—— zbus 会为一个类型生成实现，两个块会冲突。
    // 这里也不需要根接口：中间件用总线名当应用名，不读 Identity。

    pub fn run() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        if let Err(e) = rt.block_on(async_main()) {
            eprintln!("fake_mpris 失败：{e}");
            std::process::exit(1);
        }
    }

    async fn async_main() -> Result<(), Box<dyn std::error::Error>> {
        let (player, bus_name) = parse_args();

        let connection = zbus::Connection::session().await?;
        // 接口对象归 ObjectServer 所有（zbus 只为类型本身实现 Interface，不为 Arc<T>），
        // 退出时再通过 `interface::<_, Player>()` 把统计读回来。
        connection
            .object_server()
            .at("/org/mpris/MediaPlayer2", player)
            .await?;
        let reply = connection
            .request_name(bus_name.clone())
            .await?;
        // RequestName 的返回：1=PrimaryOwner 2=InQueue 3=Exists 4=AlreadyOwner
        println!("READY bus={bus_name} reply={reply:?} trackid={TRACK_ID}");

        // 等 Ctrl-C / SIGTERM
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        if let Ok(iface) = connection
            .object_server()
            .interface::<_, Player>("/org/mpris/MediaPlayer2")
            .await
        {
            let guard = iface.get().await;
            println!("BYE commands={}", guard.commands.load(Ordering::Relaxed));
        }
        let _ = connection.release_name(bus_name).await;
        Ok(())
    }

    fn parse_args() -> (Player, String) {
        let argv: Vec<String> = std::env::args().skip(1).collect();
        let mut title = "media-bridge 测试曲目".to_string();
        let mut artist = "media-bridge 测试歌手".to_string();
        let mut album = "中间件验证".to_string();
        let mut duration_s = 210.0f64;
        let mut audio_file: Option<String> = None;
        let mut bus_name = "org.mpris.MediaPlayer2.mbtest".to_string();
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
                        duration_s = v;
                    }
                }
                "--audio-file" => audio_file = take(&mut i),
                "--bus-name" => {
                    if let Some(v) = take(&mut i) {
                        bus_name = v;
                    }
                }
                other => eprintln!("忽略未知参数 {other}"),
            }
            i += 1;
        }
        (
            Player {
                title,
                artist,
                album,
                duration_us: (duration_s * 1e6) as i64,
                audio_file,
                playing: AtomicBool::new(true),
                anchor_pos_us: AtomicI64::new(0),
                anchor_at: Mutex::new(Instant::now()),
                loop_status: Mutex::new("None".to_string()),
                shuffle: AtomicBool::new(false),
                volume: Mutex::new(1.0),
                rate: Mutex::new(1.0),
                commands: AtomicI64::new(0),
                track_no: AtomicI64::new(0),
            },
            bus_name,
        )
    }
}
