//! Nested KWin harness for the output tests.
//!
//! Starts a private `dbus-daemon` (no service activation) and an invisible
//! `kwin_wayland --virtual` on its own Wayland socket and bus, plus a test pad window that
//! mirrors its text to a file. Nothing typed here can reach the user's real desktop: the EIS
//! connection comes from the nested KWin's `connectToEIS` on the private bus, and
//! [`ChildEnv::assert_isolated`] checks that before anything is typed.
//!
//! Inside a toolbox (no `kwin_wayland` in the container), KWin runs on the host through
//! `flatpak-spawn --host --clear-env`, with the environment set explicitly.

#![allow(dead_code)] // Each test binary uses a different subset.

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use ashpd::zbus;

use reis::{PendingRequestResult, ei};
use tokio::io::Interest;

/// Env vars passed from the harness to the re-executed test process.
pub const ENV_DIR: &str = "MURMUR_NESTED_DIR";
pub const ENV_SOCKET: &str = "MURMUR_NESTED_SOCKET";
pub const ENV_BUS: &str = "MURMUR_NESTED_BUS";
pub const ENV_OUTER_BUS: &str = "MURMUR_OUTER_BUS";
pub const ENV_PAD: &str = "MURMUR_NESTED_PAD";

/// `connectToEIS` capability bit for a keyboard (same as the portal's `DeviceType::KEYBOARD`).
const KWIN_EIS_KEYBOARD: i32 = 1;

/// A running nested KWin session. Everything is torn down on drop.
pub struct Nested {
    pub dir: PathBuf,
    pub socket: String,
    pub bus: String,
    children: Vec<(&'static str, Child)>,
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .expect("XDG_RUNTIME_DIR must be set")
}

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p).any(|d| {
            d.join(name)
                .metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

fn log_file(dir: &Path, name: &str) -> Stdio {
    std::fs::File::create(dir.join(format!("{name}.log")))
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null())
}

impl Nested {
    /// Starts dbus-daemon + KWin with the given `kxkbrc` layout list (e.g. `"us,lt"`).
    pub async fn start(layouts: &str) -> Self {
        let id = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        );
        let dir = runtime_dir().join(format!("murmur-test-{id}"));
        for sub in ["config", "data", "cache", "state"] {
            std::fs::create_dir_all(dir.join(sub)).expect("create nested dirs");
        }
        let mut nested = Self {
            socket: format!("murmur-wl-{id}"),
            bus: format!("unix:path={}", dir.join("bus").display()),
            dir,
            children: Vec::new(),
        };
        nested.write_kxkbrc(layouts);
        nested.start_bus().await;
        nested.start_kwin().await;
        nested
    }

    pub fn write_kxkbrc(&self, layouts: &str) {
        std::fs::write(
            self.dir.join("config/kxkbrc"),
            format!("[Layout]\nLayoutList={layouts}\nUse=true\n"),
        )
        .expect("write kxkbrc");
    }

    /// Environment for processes inside the nested session.
    pub fn session_env(&self) -> Vec<(String, String)> {
        let d = |sub: &str| self.dir.join(sub).display().to_string();
        vec![
            ("HOME".into(), std::env::var("HOME").unwrap_or_default()),
            ("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()),
            (
                "XDG_RUNTIME_DIR".into(),
                runtime_dir().display().to_string(),
            ),
            ("DBUS_SESSION_BUS_ADDRESS".into(), self.bus.clone()),
            ("XDG_CONFIG_HOME".into(), d("config")),
            ("XDG_DATA_HOME".into(), d("data")),
            ("XDG_CACHE_HOME".into(), d("cache")),
            ("XDG_STATE_HOME".into(), d("state")),
        ]
    }

    async fn start_bus(&mut self) {
        let conf = self.dir.join("bus.conf");
        let sock = self.dir.join("bus");
        std::fs::write(
            &conf,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}</listen>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                sock.display()
            ),
        )
        .expect("write bus.conf");
        let child = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", conf.display()))
            .arg("--nofork")
            .stdin(Stdio::null())
            .stdout(log_file(&self.dir, "dbus"))
            .stderr(log_file(&self.dir, "dbus-err"))
            .spawn()
            .expect("spawn dbus-daemon");
        self.children.push(("dbus-daemon", child));
        wait_for("private bus socket", Duration::from_secs(5), || async {
            sock.exists()
        })
        .await;
    }

    async fn start_kwin(&mut self) {
        let kwin_args = [
            "--virtual",
            "--no-lockscreen",
            "--no-global-shortcuts",
            "--no-kactivities",
            "--socket",
            &self.socket,
        ];
        let env = self.session_env();
        let mut cmd = if on_path("kwin_wayland") {
            let mut c = Command::new("kwin_wayland");
            c.env_clear().envs(env).args(kwin_args);
            c
        } else if on_path("flatpak-spawn") {
            // Toolbox: run the host's KWin. `--clear-env` keeps the outer session's
            // variables (bus, Wayland display) away from it; `--watch-bus` ends it if we die.
            let mut c = Command::new("flatpak-spawn");
            c.args(["--host", "--watch-bus", "--clear-env"]);
            for (k, v) in env {
                c.arg(format!("--env={k}={v}"));
            }
            c.arg("kwin_wayland").args(kwin_args);
            c
        } else {
            panic!("kwin_wayland not found (and no flatpak-spawn to reach the host)");
        };
        let child = cmd
            .stdin(Stdio::null())
            .stdout(log_file(&self.dir, "kwin"))
            .stderr(log_file(&self.dir, "kwin-err"))
            .spawn()
            .expect("spawn kwin_wayland");
        self.children.push(("kwin_wayland", child));
        let bus = self.bus.clone();
        wait_for(
            "org.kde.KWin on the private bus",
            Duration::from_secs(30),
            || {
                let bus = bus.clone();
                async move { kwin_pid(&bus).await.is_some() }
            },
        )
        .await;
        let socket = runtime_dir().join(&self.socket);
        wait_for("nested Wayland socket", Duration::from_secs(10), || async {
            socket.exists()
        })
        .await;
        // The layout service registers a little after org.kde.KWin.
        let bus = self.bus.clone();
        wait_for(
            "org.kde.keyboard on the private bus",
            Duration::from_secs(10),
            || {
                let bus = bus.clone();
                async move { has_owner(&bus, "org.kde.keyboard").await }
            },
        )
        .await;
    }

    /// Starts the test pad (a text field mirrored to `out`) on the nested display.
    pub fn spawn_pad(&mut self, out: &Path) {
        let pad = std::env::var_os("MURMUR_TESTPAD")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR")).join("spike/target/debug/spike")
            });
        assert!(
            pad.is_file(),
            "test pad not found at {} (build it with `cd spike && cargo build`, or set \
             MURMUR_TESTPAD to the spike binary)",
            pad.display()
        );
        let child = Command::new(pad)
            .arg("testpad")
            .arg(out)
            .env_clear()
            .envs(self.session_env())
            .env("WAYLAND_DISPLAY", &self.socket)
            .stdin(Stdio::null())
            .stdout(log_file(&self.dir, "pad"))
            .stderr(log_file(&self.dir, "pad-err"))
            .spawn()
            .expect("spawn test pad");
        self.children.push(("testpad", child));
    }

    /// Last lines of a child's log, for failure messages.
    pub fn log_tail(&self, name: &str) -> String {
        let text =
            std::fs::read_to_string(self.dir.join(format!("{name}.log"))).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(30)..].join("\n")
    }
}

impl Drop for Nested {
    fn drop(&mut self) {
        // Reverse order: pad, KWin, bus. SIGTERM first (flatpak-spawn forwards it to the host
        // process), SIGKILL if it doesn't exit.
        while let Some((name, mut child)) = self.children.pop() {
            let _ = Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .status();
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = child.try_wait() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if let Ok(None) = child.try_wait() {
                eprintln!("harness: {name} ignored SIGTERM, killing");
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        if std::env::var_os("MURMUR_KEEP_NESTED").is_none() {
            let _ = std::fs::remove_dir_all(&self.dir);
        } else {
            eprintln!("harness: kept {}", self.dir.display());
        }
    }
}

/// Polls `check` every 50 ms until it returns true, panicking after `timeout`.
pub async fn wait_for<F, Fut>(what: &str, timeout: Duration, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = Instant::now();
    while !check().await {
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// PID owning `org.kde.KWin` on `bus`, if any.
pub async fn kwin_pid(bus: &str) -> Option<u32> {
    let conn = zbus::connection::Builder::address(bus).build().await.ok()?;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await.ok()?;
    let name = zbus::names::BusName::try_from("org.kde.KWin").ok()?;
    dbus.get_connection_unix_process_id(name).await.ok()
}

/// Whether `name` has an owner on `bus`.
pub async fn has_owner(bus: &str, name: &str) -> bool {
    let Ok(conn) = zbus::connection::Builder::address(bus).build().await else {
        return false;
    };
    let Ok(dbus) = zbus::fdo::DBusProxy::new(&conn).await else {
        return false;
    };
    let Ok(name) = zbus::names::BusName::try_from(name) else {
        return false;
    };
    dbus.name_has_owner(name).await.unwrap_or(false)
}

/// What the re-executed test process gets from the harness.
pub struct ChildEnv {
    pub dir: PathBuf,
    pub socket: String,
    pub bus: String,
    pub pad: PathBuf,
}

impl ChildEnv {
    /// `None` unless this process was started by the harness.
    pub fn from_env() -> Option<Self> {
        Some(Self {
            dir: std::env::var_os(ENV_DIR)?.into(),
            socket: std::env::var(ENV_SOCKET).ok()?,
            bus: std::env::var(ENV_BUS).ok()?,
            pad: std::env::var_os(ENV_PAD)?.into(),
        })
    }

    /// Refuses to continue unless the session bus and Wayland display are the nested ones and
    /// the `org.kde.KWin` on that bus is the nested KWin (its command line has our socket).
    pub async fn assert_isolated(&self) {
        let bus = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap_or_default();
        assert_eq!(bus, self.bus, "session bus is not the nested one");
        assert!(
            bus.starts_with(&format!("unix:path={}/", self.dir.display())),
            "nested bus must live in the harness directory"
        );
        let outer = std::env::var(ENV_OUTER_BUS).unwrap_or_default();
        assert_ne!(bus, outer, "nested bus equals the outer session bus");
        assert_eq!(
            std::env::var("WAYLAND_DISPLAY").unwrap_or_default(),
            self.socket,
            "WAYLAND_DISPLAY is not the nested socket"
        );
        let pid = kwin_pid(&bus)
            .await
            .expect("org.kde.KWin on the nested bus");
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let args: Vec<&[u8]> = cmdline.split(|&b| b == 0).collect();
        let ours = args
            .windows(2)
            .any(|w| w[0] == b"--socket" && w[1] == self.socket.as_bytes());
        assert!(
            ours,
            "org.kde.KWin on the nested bus (pid {pid}) is not the nested KWin"
        );
    }
}

/// The test pad's text, mirrored to a file.
pub struct Pad {
    path: PathBuf,
}

impl Pad {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn text(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap_or_default()
    }

    /// Waits until the pad shows exactly `want`.
    pub async fn expect(&self, want: &str, timeout: Duration) {
        let start = Instant::now();
        loop {
            let got = self.text();
            if got == want {
                return;
            }
            if start.elapsed() > timeout {
                assert_eq!(got, want, "test pad text");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Waits until the text stops changing for `quiet`, then returns it.
    pub async fn settle(&self, quiet: Duration) -> String {
        let mut last = self.text();
        let mut since = Instant::now();
        loop {
            tokio::time::sleep(Duration::from_millis(25)).await;
            let now = self.text();
            if now != last {
                last = now;
                since = Instant::now();
            } else if since.elapsed() >= quiet {
                return last;
            }
        }
    }
}

/// A connection to the nested bus, by explicit address.
pub async fn connect(bus: &str) -> zbus::Connection {
    zbus::connection::Builder::address(bus)
        .build()
        .await
        .expect("connect to nested bus")
}

/// Switches the nested KWin's active layout (`org.kde.keyboard /Layouts setLayout`).
pub async fn set_layout(bus: &str, index: u32) {
    let reply = connect(bus)
        .await
        .call_method(
            Some("org.kde.keyboard"),
            "/Layouts",
            Some("org.kde.KeyboardLayouts"),
            "setLayout",
            &(index,),
        )
        .await
        .expect("setLayout");
    let ok: bool = reply.body().deserialize().expect("setLayout reply");
    assert!(ok, "setLayout({index}) refused");
}

/// Layout short names and the active index (`getLayoutsList`, `getLayout`).
pub async fn layouts(bus: &str) -> (Vec<String>, u32) {
    let conn = connect(bus).await;
    let call = async |method: &str| {
        conn.call_method(
            Some("org.kde.keyboard"),
            "/Layouts",
            Some("org.kde.KeyboardLayouts"),
            method,
            &(),
        )
        .await
        .expect("org.kde.KeyboardLayouts call")
    };
    let list: Vec<(String, String, String)> = call("getLayoutsList")
        .await
        .body()
        .deserialize()
        .expect("getLayoutsList reply");
    let active: u32 = call("getLayout")
        .await
        .body()
        .deserialize()
        .expect("getLayout reply");
    (list.into_iter().map(|(short, ..)| short).collect(), active)
}

/// Tells the nested KWin that `kxkbrc` changed, as KConfig does when the keyboard settings are
/// saved. (KWin 6.7 listens for `org.kde.kconfig.notify.ConfigChanged` on `/kxkbrc`, not for
/// the older `/Layouts org.kde.keyboard.reloadConfig`.)
pub async fn reload_keyboard_config(bus: &str) {
    let changed: std::collections::HashMap<&str, Vec<&[u8]>> =
        [("Layout", vec![b"LayoutList".as_slice()])].into();
    connect(bus)
        .await
        .emit_signal(
            None::<&str>,
            "/kxkbrc",
            "org.kde.kconfig.notify",
            "ConfigChanged",
            &(changed,),
        )
        .await
        .expect("emit ConfigChanged");
}

/// Minimal EI socket for the harness: reis' sync I/O driven by tokio readiness (a test-only
/// copy of `src/output/reader.rs`; reis 0.7.1's own tokio stream can lose wakeups).
struct EiSocket {
    ctx: ei::Context,
    readiness: tokio::net::UnixStream,
}

impl EiSocket {
    fn new(fd: std::os::fd::OwnedFd) -> Self {
        let socket = std::os::unix::net::UnixStream::from(fd);
        let dup = socket.try_clone().expect("dup EI socket");
        let ctx = ei::Context::new(socket).expect("ei context");
        let readiness = tokio::net::UnixStream::from_std(dup).expect("register EI socket");
        Self { ctx, readiness }
    }

    async fn next(&self) -> ei::Event {
        loop {
            match self.ctx.pending_event() {
                Some(PendingRequestResult::Request(event)) => return event,
                Some(other) => panic!("raw keyboard: bad EI message {other:?}"),
                None => {}
            }
            self.readiness.readable().await.expect("EI socket readable");
            let read = self
                .readiness
                .try_io(Interest::READABLE, || match self.ctx.read() {
                    Ok(0) => Err(std::io::ErrorKind::WouldBlock.into()),
                    other => other,
                });
            if let Err(e) = read {
                assert_eq!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock,
                    "raw keyboard: {e}"
                );
            }
        }
    }

    fn flush(&self) {
        self.ctx.flush().expect("raw keyboard: flush");
    }
}

/// A second EIS client acting as the user's physical keyboard, so the tests can hold Ctrl.
pub struct RawKeyboard {
    _bus: zbus::Connection,
    socket: EiSocket,
    connection: ei::Connection,
    device: Option<ei::Device>,
    keyboard: Option<ei::Keyboard>,
    resumed: bool,
    serial: u32,
    sequence: u32,
    sync: Option<ei::Callback>,
    keyboard_mask: u64,
    /// Last `ei_keyboard.modifiers`: (depressed, group).
    pub mods: Option<(u32, u32)>,
}

fn now_us() -> u64 {
    let ts = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1_000
}

impl RawKeyboard {
    pub async fn connect(bus: &str) -> Self {
        let conn = connect(bus).await;
        let reply = conn
            .call_method(
                Some("org.kde.KWin"),
                "/org/kde/KWin/EIS/RemoteDesktop",
                Some("org.kde.KWin.EIS.RemoteDesktop"),
                "connectToEIS",
                &(KWIN_EIS_KEYBOARD,),
            )
            .await
            .expect("connectToEIS");
        let (fd, _cookie): (zbus::OwnedFd, i32) = reply.body().deserialize().expect("reply");
        let socket = EiSocket::new(fd.into());
        let mut handshaker = reis::handshake::EiHandshaker::new(
            "murmur-test-keyboard",
            ei::handshake::ContextType::Sender,
        );
        let resp = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(resp) = handshaker
                    .handle_event(socket.next().await)
                    .expect("raw keyboard handshake")
                {
                    return resp;
                }
            }
        })
        .await
        .expect("raw keyboard handshake timed out");
        let mut kb = Self {
            _bus: conn,
            socket,
            connection: resp.connection,
            device: None,
            keyboard: None,
            resumed: false,
            serial: resp.serial,
            sequence: 0,
            sync: None,
            keyboard_mask: 0,
            mods: None,
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while !(kb.resumed && kb.keyboard.is_some()) {
                kb.next().await;
            }
        })
        .await
        .expect("raw keyboard never resumed");
        kb
    }

    async fn next(&mut self) {
        match self.socket.next().await {
            ei::Event::Connection(_, ei::connection::Event::Ping { ping }) => {
                ping.done(0);
                self.socket.flush();
            }
            ei::Event::Callback(cb, _) => {
                if self.sync.as_ref() == Some(&cb) {
                    self.sync = None;
                }
            }
            ei::Event::Seat(_, ei::seat::Event::Capability { mask, interface }) => {
                if interface == "ei_keyboard" {
                    self.keyboard_mask = mask;
                }
            }
            ei::Event::Seat(seat, ei::seat::Event::Done) => {
                seat.bind(self.keyboard_mask);
                self.socket.flush();
            }
            ei::Event::Seat(_, ei::seat::Event::Device { device }) => {
                self.device = Some(device);
            }
            ei::Event::Device(_, ei::device::Event::Interface { object }) => {
                if let Some(k) = object.downcast::<ei::Keyboard>() {
                    self.keyboard = Some(k);
                }
            }
            ei::Event::Device(_, ei::device::Event::Resumed { serial }) => {
                self.serial = serial;
                self.resumed = true;
            }
            ei::Event::Device(_, ei::device::Event::Paused { serial }) => {
                self.serial = serial;
                self.resumed = false;
            }
            ei::Event::Keyboard(
                _,
                ei::keyboard::Event::Modifiers {
                    serial,
                    depressed,
                    group,
                    ..
                },
            ) => {
                self.serial = serial;
                self.mods = Some((depressed, group));
            }
            _ => {}
        }
    }

    /// Round trip: everything sent so far has been processed by KWin.
    pub async fn sync(&mut self) {
        self.sync = Some(self.connection.sync(1));
        self.socket.flush();
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.sync.is_some() {
                self.next().await;
            }
        })
        .await
        .expect("raw keyboard sync timed out");
    }

    /// Presses or releases an evdev key, then waits until KWin has processed it.
    pub async fn key(&mut self, key: u32, pressed: bool) {
        let (Some(device), Some(kb)) = (self.device.clone(), self.keyboard.clone()) else {
            panic!("raw keyboard has no device");
        };
        self.sequence += 1;
        device.start_emulating(self.serial, self.sequence);
        let state = if pressed {
            ei::keyboard::KeyState::Press
        } else {
            ei::keyboard::KeyState::Released
        };
        kb.key(key, state);
        device.frame(self.serial, now_us());
        device.stop_emulating(self.serial);
        self.socket.flush();
        self.sync().await;
    }

    /// Waits for a modifiers event matching `pred` on depressed mods (with a timeout).
    pub async fn wait_mods(&mut self, what: &str, pred: impl Fn(u32) -> bool) {
        let ok = tokio::time::timeout(Duration::from_secs(5), async {
            while !self.mods.is_some_and(|(d, _)| pred(d)) {
                self.next().await;
            }
        })
        .await;
        assert!(
            ok.is_ok(),
            "no modifiers event: {what} (last {:?})",
            self.mods
        );
    }
}
