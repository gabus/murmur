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
//!
//! [`gnome`] is the GNOME equivalent: a headless gnome-shell with the GNOME portal stack.

#![allow(dead_code)] // Each test binary uses a different subset.

pub mod gnome;

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
    /// Screen size in pixels and scale; KWin's default (1024×768 at 1) when None.
    screen: Option<(u32, u32, f32)>,
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

/// Environment for processes inside the nested session in `dir` with private bus `bus`.
fn session_env(dir: &Path, bus: &str) -> Vec<(String, String)> {
    let d = |sub: &str| dir.join(sub).display().to_string();
    vec![
        ("HOME".into(), std::env::var("HOME").unwrap_or_default()),
        ("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()),
        (
            "XDG_RUNTIME_DIR".into(),
            runtime_dir().display().to_string(),
        ),
        ("DBUS_SESSION_BUS_ADDRESS".into(), bus.to_owned()),
        ("XDG_CONFIG_HOME".into(), d("config")),
        ("XDG_DATA_HOME".into(), d("data")),
        ("XDG_CACHE_HOME".into(), d("cache")),
        ("XDG_STATE_HOME".into(), d("state")),
    ]
}

/// A command running the desktop's `program` (KWin, the portals, wl-clipboard, spectacle) with
/// exactly `env`. Inside a toolbox it runs on the host through `flatpak-spawn --host
/// --clear-env`, so the versions under test are the host's, and `--clear-env` keeps the outer
/// session's variables (bus, Wayland display) away from it; `watch_bus` ends it if we die.
/// flatpak-spawn itself talks to the outer session bus: [`ENV_OUTER_BUS`] when set (the
/// re-executed test process has the nested bus as its session bus), else the inherited one.
fn host_command(program: &str, env: &[(String, String)], watch_bus: bool) -> Command {
    let path = Path::new(program);
    let here = if path.is_absolute() {
        path.is_file()
    } else {
        on_path(program)
    };
    if here && !Path::new("/run/.toolboxenv").exists() {
        let mut c = Command::new(program);
        c.env_clear().envs(env.iter().map(|(k, v)| (k, v)));
        return c;
    }
    assert!(
        on_path("flatpak-spawn"),
        "{program} not found (and no flatpak-spawn to run the host's)"
    );
    let mut c = Command::new("flatpak-spawn");
    if let Ok(outer) = std::env::var(ENV_OUTER_BUS)
        && !outer.is_empty()
    {
        c.env("DBUS_SESSION_BUS_ADDRESS", outer);
    }
    c.arg("--host");
    if watch_bus {
        c.arg("--watch-bus");
    }
    c.arg("--clear-env");
    for (k, v) in env {
        c.arg(format!("--env={k}={v}"));
    }
    c.arg(program);
    c
}

/// The portal stack: (log name, host program, D-Bus name it owns), in start order.
const PORTALS: [(&str, &str, &str); 3] = [
    (
        "permission-store",
        "/usr/libexec/xdg-permission-store",
        "org.freedesktop.impl.portal.PermissionStore",
    ),
    (
        "portal-kde",
        "/usr/libexec/xdg-desktop-portal-kde",
        "org.freedesktop.impl.portal.desktop.kde",
    ),
    (
        "portal",
        "/usr/libexec/xdg-desktop-portal",
        "org.freedesktop.portal.Desktop",
    ),
];

/// Environment for clients of the nested display (and bus).
fn display_env(dir: &Path, bus: &str, socket: &str) -> Vec<(String, String)> {
    let mut env = session_env(dir, bus);
    env.push(("WAYLAND_DISPLAY".into(), socket.to_owned()));
    env
}

/// Environment for the portal processes of the nested session.
fn portal_env(dir: &Path, bus: &str, socket: &str) -> Vec<(String, String)> {
    let mut env = display_env(dir, bus, socket);
    env.extend(
        [
            ("XDG_CURRENT_DESKTOP", "KDE"),
            ("XDG_SESSION_TYPE", "wayland"),
            ("QT_QPA_PLATFORM", "wayland"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned())),
    );
    env
}

/// Starts one portal process (logging to `<dir>/<name>{,-err}.log`) and waits until it owns
/// `service` on `bus` (there is no service activation).
async fn spawn_portal(
    dir: &Path,
    bus: &str,
    env: &[(String, String)],
    name: &str,
    program: &str,
    service: &'static str,
) -> Child {
    let child = host_command(program, env, true)
        .stdin(Stdio::null())
        .stdout(log_file(dir, name))
        .stderr(log_file(dir, &format!("{name}-err")))
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {program}: {e}"));
    wait_for(
        &format!("{service} on the private bus"),
        Duration::from_secs(20),
        || async move { has_owner(bus, service).await },
    )
    .await;
    child
}

impl Nested {
    /// Starts dbus-daemon + KWin with the given `kxkbrc` layout list (e.g. `"us,lt"`).
    pub async fn start(layouts: &str) -> Self {
        Self::start_on(layouts, None).await
    }

    /// [`Nested::start`] on a `width`×`height` pixel screen at `scale` (e.g. 1.5).
    pub async fn start_scaled(layouts: &str, width: u32, height: u32, scale: f32) -> Self {
        Self::start_on(layouts, Some((width, height, scale))).await
    }

    async fn start_on(layouts: &str, screen: Option<(u32, u32, f32)>) -> Self {
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
            screen,
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
        session_env(&self.dir, &self.bus)
    }

    /// Starts the real portal stack on the nested bus and display: `xdg-permission-store`,
    /// `xdg-desktop-portal-kde` and `xdg-desktop-portal` (the host's, from `/usr/libexec`). The
    /// private bus has no service activation, so each is started explicitly and waited for.
    /// Permissions and restore tokens live in the nested `XDG_DATA_HOME`, and the portal's
    /// dialogs show up on the nested display only.
    pub async fn start_portals(&mut self) {
        // Host apps must register their app ID, which the portal accepts only with a desktop
        // file (docs/portal-findings.md → Running outside Flatpak).
        let apps = self.dir.join("data/applications");
        std::fs::create_dir_all(&apps).expect("create applications dir");
        std::fs::write(
            apps.join(format!("{}.desktop", murmur::paths::APP_ID)),
            "[Desktop Entry]\nType=Application\nName=murmur (test)\nExec=true\nNoDisplay=true\n",
        )
        .expect("write desktop file");
        let env = portal_env(&self.dir, &self.bus, &self.socket);
        for (name, program, service) in PORTALS {
            let child = spawn_portal(&self.dir, &self.bus, &env, name, program, service).await;
            self.children.push((name, child));
        }
    }

    async fn start_bus(&mut self) {
        let child = start_private_bus(&self.dir, "bus").await;
        self.children.push(("dbus-daemon", child));
    }

    async fn start_kwin(&mut self) {
        let mut kwin_args: Vec<String> = [
            "--virtual",
            "--no-lockscreen",
            "--no-global-shortcuts",
            "--no-kactivities",
            "--socket",
            &self.socket,
        ]
        .map(String::from)
        .into();
        if let Some((width, height, scale)) = self.screen {
            kwin_args.extend([
                "--width".into(),
                width.to_string(),
                "--height".into(),
                height.to_string(),
                "--scale".into(),
                scale.to_string(),
            ]);
        }
        let mut cmd = host_command("kwin_wayland", &self.session_env(), true);
        let child = cmd
            .args(&kwin_args)
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
        let child = Command::new(pad_binary())
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
        // Reverse order: pad, portals, KWin, bus.
        while let Some((name, mut child)) = self.children.pop() {
            stop_child(name, &mut child);
        }
        if std::env::var_os("MURMUR_KEEP_NESTED").is_none() {
            let _ = std::fs::remove_dir_all(&self.dir);
        } else {
            eprintln!("harness: kept {}", self.dir.display());
        }
    }
}

/// The spike's test pad binary (`spike testpad <file>`), or `$MURMUR_TESTPAD`.
fn pad_binary() -> PathBuf {
    let pad = std::env::var_os("MURMUR_TESTPAD")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("spike/target/debug/spike"));
    assert!(
        pad.is_file(),
        "test pad not found at {} (build it with `cd spike && cargo build`, or set \
         MURMUR_TESTPAD to the spike binary)",
        pad.display()
    );
    pad
}

/// Starts a private `dbus-daemon` listening on `<dir>/<name>`, with no service directories (so
/// no service activation), and waits for its socket. Its config and logs are `<dir>/<name>.conf`
/// and `<dir>/<name>{,-err}.log`.
async fn start_private_bus(dir: &Path, name: &str) -> Child {
    let conf = dir.join(format!("{name}.conf"));
    let sock = dir.join(name);
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
        .stdout(log_file(dir, name))
        .stderr(log_file(dir, &format!("{name}-err")))
        .spawn()
        .expect("spawn dbus-daemon");
    wait_for("private bus socket", Duration::from_secs(5), || async {
        sock.exists()
    })
    .await;
    child
}

/// Stops a child: SIGTERM first (flatpak-spawn forwards it to the host process), SIGKILL if it
/// doesn't exit.
fn stop_child(name: &str, child: &mut Child) {
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
    owner_pid(bus, "org.kde.KWin").await
}

/// PID of the connection owning `name` (well-known or unique) on `bus`, if any.
pub async fn owner_pid(bus: &str, name: &str) -> Option<u32> {
    let conn = zbus::connection::Builder::address(bus).build().await.ok()?;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await.ok()?;
    let name = zbus::names::BusName::try_from(name).ok()?;
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

    /// Refuses to continue unless the session bus and Wayland display are the nested ones, X11 is
    /// out of reach, and the compositor on that bus is ours: the `org.kde.KWin` of a nested KWin
    /// (its command line has our socket) or the `org.gnome.Shell` of a headless gnome-shell (its
    /// command line has `--headless` and our socket, and the runtime dir is the private one).
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
        assert!(
            std::env::var_os("DISPLAY").is_none(),
            "DISPLAY is set: X11 clients could reach the real desktop"
        );
        let (pid, flag, compositor) = if let Some(pid) = kwin_pid(&bus).await {
            (pid, "--socket", "KWin")
        } else {
            let pid = owner_pid(&bus, "org.gnome.Shell")
                .await
                .expect("org.kde.KWin or org.gnome.Shell on the nested bus");
            let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
            assert!(
                runtime.is_some_and(|r| r.starts_with(&self.dir)),
                "XDG_RUNTIME_DIR is not the private one in the harness directory"
            );
            let system = self.private_system_bus();
            assert!(
                process_env(pid).contains(&format!("DBUS_SYSTEM_BUS_ADDRESS={system}")),
                "gnome-shell (pid {pid}) is on the host's system bus"
            );
            (pid, "--wayland-display", "headless gnome-shell")
        };
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let args: Vec<&[u8]> = cmdline.split(|&b| b == 0).collect();
        let ours = args
            .windows(2)
            .any(|w| w[0] == flag.as_bytes() && w[1] == self.socket.as_bytes());
        let headless = flag == "--socket" || args.contains(&b"--headless".as_slice());
        assert!(
            ours && headless,
            "the compositor on the nested bus (pid {pid}) is not the {compositor}"
        );
    }
}

impl ChildEnv {
    /// The private system bus (headless GNOME): this process's `DBUS_SYSTEM_BUS_ADDRESS`, which
    /// must be in the harness directory.
    fn private_system_bus(&self) -> String {
        let system = std::env::var("DBUS_SYSTEM_BUS_ADDRESS").unwrap_or_default();
        assert!(
            system.starts_with(&format!("unix:path={}/", self.dir.display())),
            "DBUS_SYSTEM_BUS_ADDRESS {system:?} is not the private one in the harness directory"
        );
        system
    }
}

/// The environment of a local process (`NAME=value` entries), empty if unreadable.
fn process_env(pid: u32) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/environ"))
        .unwrap_or_default()
        .split(|&b| b == 0)
        .map(|v| String::from_utf8_lossy(v).into_owned())
        .collect()
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
    /// A keyboard from the nested KWin's `connectToEIS`.
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
        Self::from_eis(conn, fd.into()).await
    }

    /// Handshakes on an EIS socket and waits for a resumed keyboard. `conn` is kept open: the
    /// compositor may end the EIS session when it closes.
    async fn from_eis(conn: zbus::Connection, fd: std::os::fd::OwnedFd) -> Self {
        let socket = EiSocket::new(fd);
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

    /// Presses `keys` in order (e.g. Alt, S), then releases them in reverse order.
    pub async fn tap(&mut self, keys: &[u32]) {
        for &key in keys {
            self.key(key, true).await;
        }
        for &key in keys.iter().rev() {
            self.key(key, false).await;
        }
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

// --- The real portal stack in the nested session (tests/portal_nested_kde.rs) ---

/// Resource class of the KDE portal backend's windows (its permission dialogs).
const PORTAL_KDE_CLASS: &str = "org.freedesktop.impl.portal.desktop.kde";
/// How long a host command (wl-copy, wl-paste, spectacle, cat) may take.
const HOST_TIMEOUT: Duration = Duration::from_secs(10);

impl ChildEnv {
    /// Environment for host programs on the nested display and bus.
    pub fn session_env(&self) -> Vec<(String, String)> {
        display_env(&self.dir, &self.bus, &self.socket)
    }

    /// Starts the KDE portal backend again (after [`kill_portal_backend`]), like D-Bus
    /// activation would in a real session. Logs to `portal-kde-2{,-err}.log`.
    pub async fn restart_portal_backend(&self) -> HostProcess {
        let (_, program, service) = PORTALS[1];
        let env = portal_env(&self.dir, &self.bus, &self.socket);
        let name = "portal-kde-2";
        let child = spawn_portal(&self.dir, &self.bus, &env, name, program, service).await;
        HostProcess(name, child)
    }

    /// Refuses to continue unless the portal frontend and the desktop's backend (KDE, or GNOME
    /// with its shortcuts provider) on the nested bus run with the nested bus and Wayland
    /// display, so their dialogs can only appear in the nested compositor.
    pub async fn assert_portals_isolated(&self) {
        let kde = has_owner(&self.bus, "org.freedesktop.impl.portal.desktop.kde").await;
        let backends = if kde {
            &["org.freedesktop.impl.portal.desktop.kde"][..]
        } else {
            &[
                "org.freedesktop.impl.portal.desktop.gnome",
                "org.gnome.Settings.GlobalShortcutsProvider",
            ][..]
        };
        let mut wants = vec![
            format!("DBUS_SESSION_BUS_ADDRESS={}", self.bus),
            format!("WAYLAND_DISPLAY={}", self.socket),
        ];
        if !kde {
            // The GNOME stack runs in the toolbox, with a private system bus too.
            wants.push(format!(
                "DBUS_SYSTEM_BUS_ADDRESS={}",
                self.private_system_bus()
            ));
        }
        for &name in ["org.freedesktop.portal.Desktop"].iter().chain(backends) {
            // GNOME's shortcuts provider quits when idle and the harness restarts it.
            let bus = &self.bus;
            wait_for(
                &format!("{name} on the nested bus"),
                Duration::from_secs(10),
                || async move { has_owner(bus, name).await },
            )
            .await;
            let pid = owner_pid(&self.bus, name)
                .await
                .unwrap_or_else(|| panic!("{name} on the nested bus"));
            let path = format!("/proc/{pid}/environ");
            // A toolbox (another user namespace) may not read a host process's environment.
            let environ = match std::fs::read(&path) {
                Ok(environ) => environ,
                Err(_) => self
                    .run_host("cat", &[&path], None)
                    .await
                    .unwrap_or_default(),
            };
            let vars: Vec<&[u8]> = environ.split(|&b| b == 0).collect();
            for want in &wants {
                assert!(
                    vars.contains(&want.as_bytes()),
                    "{name} (pid {pid}) is not the nested one: no {want}"
                );
            }
        }
    }

    /// Runs a host program on the nested display and bus; returns its stdout (`None` on failure).
    /// With `stdin`, its output isn't captured (empty), because it may leave a background
    /// process behind that would hold the pipes open (wl-copy does).
    async fn run_host(
        &self,
        program: &str,
        args: &[&str],
        stdin: Option<&[u8]>,
    ) -> Option<Vec<u8>> {
        use tokio::io::AsyncWriteExt;
        let output = || {
            if stdin.is_some() {
                Stdio::null()
            } else {
                Stdio::piped()
            }
        };
        let mut cmd =
            tokio::process::Command::from(host_command(program, &self.session_env(), false));
        cmd.args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(output())
            .stderr(output())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {program}: {e}"));
        if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(data).await.expect("write to host program");
            // Dropping the pipe closes it: the end of the input.
        }
        let out = tokio::time::timeout(HOST_TIMEOUT, child.wait_with_output())
            .await
            .unwrap_or_else(|_| panic!("{program} {args:?} timed out"))
            .expect("host program output");
        if !out.status.success() {
            eprintln!(
                "harness: {program} {args:?}: {} {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
            return None;
        }
        Some(out.stdout)
    }

    /// Screenshot of the nested screen (spectacle through KWin's ScreenShot2 on the nested bus)
    /// as `<name>.png` in `$MURMUR_SCREENSHOTS`, or in the session directory.
    pub async fn screenshot(&self, name: &str) -> PathBuf {
        let dir = std::env::var_os("MURMUR_SCREENSHOTS")
            .map(PathBuf::from)
            .unwrap_or_else(|| self.dir.clone());
        std::fs::create_dir_all(&dir).expect("create screenshot dir");
        let path = dir.join(format!("{name}.png"));
        let out = path.display().to_string();
        let taken = self
            .run_host("spectacle", &["-b", "-n", "-f", "-o", &out], None)
            .await
            .is_some();
        eprintln!(
            "harness: screenshot {out} ({})",
            if taken { "ok" } else { "failed" }
        );
        path
    }

    /// Puts `data` on the nested clipboard as `mime_type` (default: text), with wl-copy, which
    /// keeps serving it in the background until another client takes the selection.
    pub async fn copy(&self, mime_type: Option<&str>, data: &[u8]) {
        let mut args = Vec::new();
        if let Some(mime_type) = mime_type {
            args.extend(["--type", mime_type]);
        }
        self.run_host("wl-copy", &args, Some(data))
            .await
            .expect("wl-copy on the nested display");
    }

    /// Mime types offered on the nested clipboard; empty when nothing is copied.
    pub async fn clipboard_types(&self) -> Vec<String> {
        self.run_host("wl-paste", &["--list-types"], None)
            .await
            .map(|out| {
                String::from_utf8_lossy(&out)
                    .lines()
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The nested clipboard's content as `mime_type`, if any.
    pub async fn paste(&self, mime_type: &str) -> Option<Vec<u8>> {
        self.run_host("wl-paste", &["--no-newline", "--type", mime_type], None)
            .await
    }
}

/// The `version` property of portal `interface` on `bus`, if the frontend exposes it.
pub async fn portal_version(bus: &str, interface: &str) -> Option<u32> {
    portal_property(bus, interface, "version").await
}

/// A `u32` property of portal `interface` on `bus` (e.g. RemoteDesktop's
/// `AvailableDeviceTypes`), if the frontend exposes it.
pub async fn portal_property(bus: &str, interface: &str, property: &str) -> Option<u32> {
    let reply = connect(bus)
        .await
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(interface, property),
        )
        .await
        .ok()?;
    let value: zbus::OwnedVariant = reply.body().deserialize().ok()?;
    u32::try_from(value).ok()
}

/// The KDE portal backend's windows (its dialogs) on the nested KWin, as KRunner match IDs of
/// KWin's window runner (`org.kde.KWin /WindowsRunner`), which can also activate them.
pub async fn portal_dialogs(bus: &str) -> Vec<String> {
    type Match = (
        String,
        String,
        String,
        i32,
        f64,
        std::collections::HashMap<String, zbus::OwnedVariant>,
    );
    let conn = connect(bus).await;
    let Ok(reply) = conn
        .call_method(
            Some("org.kde.KWin"),
            "/WindowsRunner",
            Some("org.kde.krunner1"),
            "Match",
            &(PORTAL_KDE_CLASS,),
        )
        .await
    else {
        return Vec::new();
    };
    let matches: Vec<Match> = reply.body().deserialize().expect("window runner reply");
    let mut dialogs = Vec::new();
    for (id, ..) in matches {
        // Match IDs are `<desktop>_<window uuid>`; check the window really is the backend's.
        let uuid = id.split_once('_').map_or(id.as_str(), |(_, uuid)| uuid);
        let info = conn
            .call_method(
                Some("org.kde.KWin"),
                "/KWin",
                Some("org.kde.KWin"),
                "getWindowInfo",
                &(uuid,),
            )
            .await;
        let class = info.ok().and_then(|reply| {
            let info: std::collections::HashMap<String, zbus::OwnedVariant> =
                reply.body().deserialize().ok()?;
            String::try_from(info.get("resourceClass")?.try_clone().ok()?).ok()
        });
        if class.as_deref() == Some(PORTAL_KDE_CLASS) {
            dialogs.push(id);
        }
    }
    dialogs
}

/// Waits up to `timeout` for a dialog of the KDE portal backend.
pub async fn wait_portal_dialog(bus: &str, timeout: Duration) -> Option<String> {
    let start = Instant::now();
    loop {
        if let Some(dialog) = portal_dialogs(bus).await.into_iter().next() {
            return Some(dialog);
        }
        if start.elapsed() > timeout {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Answers the KDE portal's dialog with `key` from the "physical" keyboard (Return approves,
/// Escape denies): waits for it, activates it through KWin (a new window may not get focus over
/// the active one), presses the key and waits until it is gone.
pub async fn answer_portal_dialog(bus: &str, kb: &mut RawKeyboard, key: u32) {
    let dialog = wait_portal_dialog(bus, Duration::from_secs(20))
        .await
        .expect("no portal dialog appeared");
    connect(bus)
        .await
        .call_method(
            Some("org.kde.KWin"),
            "/WindowsRunner",
            Some("org.kde.krunner1"),
            "Run",
            &(dialog.as_str(), ""),
        )
        .await
        .expect("activate the portal dialog");
    // Let the activation settle before the key arrives.
    tokio::time::sleep(Duration::from_millis(300)).await;
    kb.key(key, true).await;
    kb.key(key, false).await;
    wait_for(
        "the portal dialog to close",
        Duration::from_secs(10),
        || async { !portal_dialogs(bus).await.contains(&dialog) },
    )
    .await;
}

/// Ends the RemoteDesktop session as the user would: the "End" entry in the menu of the KDE
/// backend's "Remote Control" tray icon (a StatusNotifierItem with a dbusmenu at `/MenuBar`, on
/// another connection of the backend's process).
pub async fn end_remote_control(bus: &str) {
    type Item = (
        i32,
        std::collections::HashMap<String, zbus::OwnedVariant>,
        Vec<zbus::OwnedVariant>,
    );
    let conn = connect(bus).await;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await.expect("bus proxy");
    let backend = owner_pid(bus, "org.freedesktop.impl.portal.desktop.kde")
        .await
        .expect("KDE portal backend on the nested bus");
    for name in dbus.list_names().await.expect("ListNames") {
        if !name.starts_with(':') || owner_pid(bus, name.as_str()).await != Some(backend) {
            continue;
        }
        let Ok(reply) = conn
            .call_method(
                Some(name.as_str()),
                "/MenuBar",
                Some("com.canonical.dbusmenu"),
                "GetLayout",
                &(0i32, -1i32, vec!["label"]),
            )
            .await
        else {
            continue;
        };
        let (_, (_, _, children)): (u32, Item) = reply.body().deserialize().expect("menu layout");
        for child in children {
            let Ok((id, props, _)) = Item::try_from(child) else {
                continue;
            };
            let label = props
                .get("label")
                .and_then(|l| String::try_from(l.try_clone().ok()?).ok())
                .unwrap_or_default();
            if label.replace('&', "") == "End" {
                conn.call_method(
                    Some(name.as_str()),
                    "/MenuBar",
                    Some("com.canonical.dbusmenu"),
                    "Event",
                    &(id, "clicked", zbus::Variant::from(""), 0u32),
                )
                .await
                .expect("click End");
                return;
            }
        }
    }
    panic!("no \"End\" entry in the KDE portal's tray menu");
}

/// A host process started by the re-executed test process; stopped on drop.
pub struct HostProcess(&'static str, Child);

impl Drop for HostProcess {
    fn drop(&mut self) {
        stop_child(self.0, &mut self.1);
    }
}

/// Kills the KDE portal backend (after [`ChildEnv::assert_portals_isolated`]).
pub async fn kill_portal_backend(bus: &str) {
    let pid = owner_pid(bus, "org.freedesktop.impl.portal.desktop.kde")
        .await
        .expect("KDE portal backend on the nested bus");
    let killed = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .is_ok_and(|s| s.success());
    assert!(killed, "kill the KDE portal backend (pid {pid})");
}

// --- Driving `murmur::output::Output` (tests/portal_nested_kde.rs, tests/portal_headless_gnome.rs) ---

pub type Events = tokio::sync::mpsc::UnboundedReceiver<murmur::output::OutputEvent>;

/// Queues `text` word by word, as the daemon does, and waits until it's typed.
pub async fn type_words(out: &murmur::output::Output, text: &str) {
    for word in text.split_inclusive(' ') {
        out.type_text(word.to_owned());
    }
    tokio::time::timeout(Duration::from_secs(5), out.flush())
        .await
        .expect("flush timed out");
}

/// Types a probe word until it lands in the pad (which may get focus a little after a dialog
/// closes), then returns the settled text, which later checks extend.
pub async fn focus(out: &murmur::output::Output, pad: &Pad) -> String {
    for _ in 0..20 {
        out.type_text("probe ".into());
        out.flush().await;
        let landed = tokio::time::timeout(Duration::from_secs(1), async {
            while !pad.text().ends_with("probe ") {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await;
        if landed.is_ok() {
            return pad.settle(Duration::from_millis(300)).await;
        }
    }
    panic!("the test pad never got keyboard focus");
}

/// Waits for the first event matching `pred`; other events are logged and skipped.
pub async fn wait_event(
    events: &mut Events,
    what: &str,
    pred: impl Fn(&murmur::output::OutputEvent) -> bool,
) -> murmur::output::OutputEvent {
    let got = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match events.recv().await {
                Some(e) if pred(&e) => return e,
                Some(e) => eprintln!("(event while waiting for {what}: {e:?})"),
                None => panic!("output event channel closed"),
            }
        }
    })
    .await;
    got.unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// Waits for `want`; a failure event first fails the test.
pub async fn expect_event(events: &mut Events, want: murmur::output::OutputEvent) {
    use murmur::output::OutputEvent;
    let failure = |e: &OutputEvent| {
        matches!(
            e,
            OutputEvent::Disconnected(_)
                | OutputEvent::Unavailable(_)
                | OutputEvent::PermissionDenied
        )
    };
    let got = wait_event(events, &format!("{want:?}"), |e| *e == want || failure(e)).await;
    assert_eq!(got, want, "unexpected output event");
}

/// Fails on any queued `Unmapped`, `Disconnected` or `Unavailable` event.
pub fn assert_no_unmapped(events: &mut Events) {
    use murmur::output::OutputEvent;
    while let Ok(e) = events.try_recv() {
        assert!(
            !matches!(
                e,
                OutputEvent::Unmapped { .. }
                    | OutputEvent::Disconnected(_)
                    | OutputEvent::Unavailable(_)
            ),
            "unexpected output event {e:?}"
        );
    }
}
