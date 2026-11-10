//! Headless GNOME harness: `gnome-shell --headless` with the GNOME portal stack, fully private.
//!
//! Private `dbus-daemon`s with no service activation as the session bus *and the system bus*
//! (gnome-shell would otherwise reach the host's system bus and D-Bus-activate services there,
//! e.g. geoclue, whose agent it registers at startup whatever the location setting), gnome-shell
//! on its own Wayland socket, a private `XDG_RUNTIME_DIR`, `HOME` and `XDG_*` dirs,
//! `GSETTINGS_BACKEND=memory`, no `DISPLAY` (and `--no-x11`), no audio. On that bus: `xdg-permission-store`,
//! `xdg-desktop-portal-gnome`, gnome-control-center's GlobalShortcuts provider (restarted by the
//! harness when it quits on idle, standing in for D-Bus activation) and `xdg-desktop-portal`.
//! Everything runs inside the toolbox, from the toolbox's GNOME packages.
//!
//! gnome-shell runs with its hidden `--unsafe-mode` flag, so the harness can use
//! `org.gnome.Shell.Eval` (windows, focus, the clipboard, the panel's screen-sharing indicator)
//! and `org.gnome.Shell.Screenshot`. The portal path doesn't depend on it: the portal backend is
//! on the shell's allowlists anyway. Dialogs are answered with keys from a "physical" keyboard:
//! a second EIS client through mutter's own `org.gnome.Mutter.RemoteDesktop` API. Each such
//! session shows in the panel's screen-sharing indicator, so the keyboard is connected only
//! while it's needed.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

use ashpd::zbus;
use serde::Deserialize;

use super::*;

/// gnome-shell's log line once startup (and its overview animation) is done.
const SHELL_STARTED: &str = "GNOME Shell started";
/// mutter's `device-types` bit for a keyboard.
const MUTTER_KEYBOARD: u32 = 1;

// Evdev keycodes (linux/input-event-codes.h).
pub const KEY_ESC: u32 = 1;
pub const KEY_TAB: u32 = 15;
pub const KEY_A: u32 = 30;
pub const KEY_S: u32 = 31;
pub const KEY_H: u32 = 35;
pub const KEY_LEFTALT: u32 = 56;
pub const KEY_SPACE: u32 = 57;
pub const KEY_CAPSLOCK: u32 = 58;
pub const KEY_LEFTMETA: u32 = 125;

/// The title of the GTK test pad's window.
pub const PAD_TITLE: &str = "murmur gtk testpad";

/// A portal dialog: window class and title (in the C locale the harness sets).
pub type Dialog = (&'static str, &'static str);
/// xdg-desktop-portal-gnome's RemoteDesktop dialog.
pub const REMOTE_DESKTOP_DIALOG: Dialog = ("xdg-desktop-portal-gnome", "Remote Desktop");
/// gnome-control-center's GlobalShortcuts dialog.
pub const SHORTCUTS_DIALOG: Dialog = (
    "org.gnome.Settings.GlobalShortcutsProvider",
    "Add Keyboard Shortcuts",
);

/// The portal stack: (log name, program, D-Bus name it owns), in start order. The shortcuts
/// provider is supervised separately (see [`Supervised`]).
const PORTALS: [(&str, &str, &str); 3] = [
    (
        "permission-store",
        "/usr/libexec/xdg-permission-store",
        "org.freedesktop.impl.portal.PermissionStore",
    ),
    (
        "portal-gnome",
        "/usr/libexec/xdg-desktop-portal-gnome",
        BACKEND,
    ),
    (
        "portal",
        "/usr/libexec/xdg-desktop-portal",
        "org.freedesktop.portal.Desktop",
    ),
];
const BACKEND: &str = "org.freedesktop.impl.portal.desktop.gnome";
const PROVIDER: &str = "/usr/libexec/gnome-control-center-global-shortcuts-provider";
const PROVIDER_NAME: &str = "org.gnome.Settings.GlobalShortcutsProvider";

/// The private environment of the session in `dir` (no `WAYLAND_DISPLAY`; see [`display_env`]).
pub fn env(dir: &Path, bus: &str) -> Vec<(String, String)> {
    let d = |sub: &str| dir.join(sub).display().to_string();
    [
        ("HOME", d("home")),
        ("PATH", "/usr/local/bin:/usr/bin:/bin".into()),
        ("XDG_RUNTIME_DIR", d("rt")),
        ("DBUS_SESSION_BUS_ADDRESS", bus.to_owned()),
        (
            "DBUS_SYSTEM_BUS_ADDRESS",
            format!("unix:path={}", d("system-bus")),
        ),
        ("XDG_CONFIG_HOME", d("config")),
        ("XDG_DATA_HOME", d("data")),
        ("XDG_CACHE_HOME", d("cache")),
        ("XDG_STATE_HOME", d("state")),
        ("XDG_CURRENT_DESKTOP", "GNOME".into()),
        ("XDG_SESSION_TYPE", "wayland".into()),
        ("GSETTINGS_BACKEND", "memory".into()),
        // English dialog texts and mnemonics.
        ("LANG", "C.UTF-8".into()),
        ("PULSE_SERVER", "unix:/nonexistent".into()),
        ("PIPEWIRE_REMOTE", "murmur-none".into()),
        ("GTK_A11Y", "none".into()),
        ("NO_AT_BRIDGE", "1".into()),
    ]
    .map(|(k, v)| (k.to_owned(), v))
    .into()
}

/// [`env`] for clients of the headless display.
pub fn display_env(dir: &Path, bus: &str, socket: &str) -> Vec<(String, String)> {
    let mut env = env(dir, bus);
    env.push(("WAYLAND_DISPLAY".into(), socket.to_owned()));
    env
}

/// A command with exactly `env`, logging stdout and stderr to `<dir>/<name>.log`.
fn command(program: &str, env: &[(String, String)], dir: &Path, name: &str) -> Command {
    let log = std::fs::File::create(dir.join(format!("{name}.log"))).expect("create log");
    let err = log.try_clone().expect("clone log");
    let mut cmd = Command::new(program);
    cmd.env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(err);
    cmd
}

/// A running headless GNOME session. Everything is torn down on drop.
pub struct Headless {
    pub dir: PathBuf,
    pub socket: String,
    pub bus: String,
    children: Vec<(&'static str, Child)>,
    provider: Option<Supervised>,
}

impl Headless {
    /// Starts the private bus and gnome-shell, leaves the overview and warms up the seat.
    pub async fn start() -> Self {
        let id = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        );
        let dir = runtime_dir().join(format!("murmur-gnome-{id}"));
        for sub in ["home", "config", "data", "cache", "state", "rt"] {
            std::fs::create_dir_all(dir.join(sub)).expect("create session dirs");
        }
        std::fs::set_permissions(dir.join("rt"), std::fs::Permissions::from_mode(0o700))
            .expect("chmod runtime dir");
        let mut session = Self {
            socket: format!("murmur-gnome-{id}"),
            bus: format!("unix:path={}", dir.join("bus").display()),
            dir,
            children: Vec::new(),
            provider: None,
        };
        let bus = start_private_bus(&session.dir, "bus").await;
        session.children.push(("dbus-daemon", bus));
        let system = start_private_bus(&session.dir, "system-bus").await;
        session.children.push(("system dbus-daemon", system));
        session.start_shell().await;
        session
    }

    pub fn env(&self) -> Vec<(String, String)> {
        display_env(&self.dir, &self.bus, &self.socket)
    }

    async fn start_shell(&mut self) {
        let shell = command(
            "gnome-shell",
            &env(&self.dir, &self.bus),
            &self.dir,
            "shell",
        )
        .args([
            "--headless",
            "--wayland",
            "--no-x11",
            "--unsafe-mode",
            "--virtual-monitor",
            "1280x800",
            "--wayland-display",
            &self.socket,
        ])
        .spawn()
        .expect("spawn gnome-shell");
        self.children.push(("gnome-shell", shell));
        let log = self.dir.join("shell.log");
        wait_for("gnome-shell to start", Duration::from_secs(60), || async {
            std::fs::read_to_string(&log).is_ok_and(|l| l.contains(SHELL_STARTED))
        })
        .await;
        // The startup animation ends in the overview, which takes keyboard focus. Close it like
        // a user would: a window mapped while it's up never gets focus.
        connect(&self.bus)
            .await
            .call_method(
                Some("org.gnome.Shell"),
                "/org/gnome/Shell",
                Some("org.freedesktop.DBus.Properties"),
                "Set",
                &(
                    "org.gnome.Shell",
                    "OverviewActive",
                    zbus::Variant::from(false),
                ),
            )
            .await
            .expect("close the overview");
        let bus = self.bus.clone();
        wait_for("the overview to close", Duration::from_secs(10), || {
            let bus = bus.clone();
            async move { eval(&bus, "Main.overview.visible").await == "false" }
        })
        .await;
        // The headless seat has no keyboard until an EIS keyboard connects, and clients bind
        // `wl_keyboard` only then: the first keys of a new EIS keyboard could reach a dialog
        // before it's ready. Plug in a keyboard for good, as a real machine has one.
        eval(
            &self.bus,
            "const C = imports.gi.Clutter;
             global.murmurTestKeyboard = C.get_default_backend().get_default_seat()
                 .create_virtual_device(C.InputDeviceType.KEYBOARD_DEVICE); true",
        )
        .await;
        // A window mapped before the shell has seen a key event isn't reliably activated
        // (docs/portal-findings.md), so press one key with nothing focused.
        let mut kb = RawKeyboard::connect_mutter(&self.bus).await;
        kb.tap(&[KEY_SPACE]).await;
    }

    /// Starts the portal stack on the private bus and display and waits for each D-Bus name
    /// (there is no service activation). Permissions and restore tokens live in the private
    /// `XDG_DATA_HOME`.
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
        let env = self.env();
        // The shortcuts provider quits when idle; D-Bus activation would start it again.
        let (dir, provider_env) = (self.dir.clone(), env.clone());
        self.provider = Some(Supervised::start("shortcuts-provider", move |n| {
            command(
                PROVIDER,
                &provider_env,
                &dir,
                &format!("shortcuts-provider-{n}"),
            )
        }));
        wait_name(&self.bus, PROVIDER_NAME).await;
        for (name, program, service) in PORTALS {
            let child = command(program, &env, &self.dir, name)
                .spawn()
                .unwrap_or_else(|e| panic!("spawn {program}: {e}"));
            self.children.push((name, child));
            wait_name(&self.bus, service).await;
        }
    }

    /// Starts the GTK test pad (`gtk-testpad.js`: a text view mirrored to `out`, titled
    /// [`PAD_TITLE`]) on the headless display. GTK, unlike the Slint pad, can paste on mutter.
    pub fn spawn_pad(&mut self, out: &Path) {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/harness/gtk-testpad.js");
        let child = command("gjs", &self.env(), &self.dir, "pad")
            .arg("-m")
            .arg(script)
            .arg(out)
            .spawn()
            .expect("spawn the GTK test pad (gjs)");
        self.children.push(("testpad", child));
    }

    /// Starts `program` on the headless display and bus with the session's environment plus
    /// `extra`, logging to `<dir>/<name>.log`. Stopped with the session.
    pub fn spawn(
        &mut self,
        name: &'static str,
        program: &str,
        args: &[&str],
        extra: &[(&str, &str)],
    ) {
        let mut env = self.env();
        env.extend(
            extra
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
        );
        let child = command(program, &env, &self.dir, name)
            .args(args)
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {program}: {e}"));
        self.children.push((name, child));
    }

    /// Stops the child started as `name`. Returns its exit status if it had already exited.
    pub fn stop(&mut self, name: &str) -> Option<std::process::ExitStatus> {
        let i = self.children.iter().position(|(n, _)| *n == name)?;
        let (name, mut child) = self.children.remove(i);
        let exited = child.try_wait().ok().flatten();
        stop_child(name, &mut child);
        exited
    }

    /// Whether the child started as `name` is still running.
    pub fn running(&mut self, name: &str) -> bool {
        self.children
            .iter_mut()
            .find(|(n, _)| *n == name)
            .is_some_and(|(_, c)| c.try_wait().is_ok_and(|s| s.is_none()))
    }

    /// The test binary re-executed as `test` inside the session: private bus, display, runtime
    /// dir and `XDG_*` dirs, no `DISPLAY`.
    pub fn scenario_command(&self, test: &str, pad: &Path) -> Command {
        let exe = std::env::current_exe().expect("test binary path");
        let mut cmd = Command::new(exe);
        cmd.args([
            test,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ]);
        for (k, v) in self.env() {
            if k != "PATH" {
                cmd.env(k, v);
            }
        }
        cmd.env_remove("DISPLAY")
            .env(ENV_DIR, &self.dir)
            .env(ENV_SOCKET, &self.socket)
            .env(ENV_BUS, &self.bus)
            .env(ENV_PAD, pad)
            .env(
                ENV_OUTER_BUS,
                std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap_or_default(),
            );
        cmd
    }

    /// Runs `cmd` (from [`Headless::scenario_command`]) to the end, killing it after `timeout`;
    /// prints the session's logs on failure. Returns whether it succeeded.
    pub async fn run_scenarios(&self, mut cmd: Command, timeout: Duration) -> bool {
        let mut child = cmd.spawn().expect("re-run test binary");
        let pid = child.id();
        let waited =
            tokio::time::timeout(timeout, tokio::task::spawn_blocking(move || child.wait())).await;
        let ok = match waited {
            Ok(Ok(Ok(status))) => status.success(),
            _ => {
                let _ = Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .status();
                false
            }
        };
        if !ok {
            for log in [
                "shell",
                "portal",
                "portal-gnome",
                "shortcuts-provider-0",
                "pad",
            ] {
                eprintln!("--- {log} ---\n{}", self.log_tail(log));
            }
        }
        ok
    }

    /// Last lines of a log, for failure messages.
    pub fn log_tail(&self, name: &str) -> String {
        let text =
            std::fs::read_to_string(self.dir.join(format!("{name}.log"))).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(30)..].join("\n")
    }

    /// Refuses to continue unless the `org.gnome.Shell` on the private bus is the headless one
    /// on our socket, on the private system bus (for checks run from the outer test process).
    pub async fn assert_isolated(&self) {
        let pid = owner_pid(&self.bus, "org.gnome.Shell")
            .await
            .expect("org.gnome.Shell on the private bus");
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let args: Vec<&[u8]> = cmdline.split(|&b| b == 0).collect();
        assert!(
            args.contains(&b"--headless".as_slice())
                && args
                    .windows(2)
                    .any(|w| w[0] == b"--wayland-display" && w[1] == self.socket.as_bytes()),
            "org.gnome.Shell on the private bus (pid {pid}) is not the headless shell"
        );
        let system = format!(
            "DBUS_SYSTEM_BUS_ADDRESS=unix:path={}",
            self.dir.join("system-bus").display()
        );
        assert!(
            process_env(pid).contains(&system),
            "gnome-shell (pid {pid}) is not on the private system bus"
        );
    }
}

impl Drop for Headless {
    fn drop(&mut self) {
        // Reverse start order: clients (pad, portals, daemon), the shortcuts provider, then
        // gnome-shell and the buses.
        let mut base = std::mem::take(&mut self.children);
        let clients_at = base
            .iter()
            .position(|(n, _)| *n == "gnome-shell")
            .map_or(base.len(), |i| i + 1);
        let clients = base.split_off(clients_at);
        for (name, mut child) in clients.into_iter().rev() {
            stop_child(name, &mut child);
        }
        self.provider = None;
        for (name, mut child) in base.into_iter().rev() {
            stop_child(name, &mut child);
        }
        if std::env::var_os("MURMUR_KEEP_NESTED").is_none() {
            let _ = std::fs::remove_dir_all(&self.dir);
        } else {
            eprintln!("harness: kept {}", self.dir.display());
        }
    }
}

async fn wait_name(bus: &str, name: &str) {
    wait_for(
        &format!("{name} on the private bus"),
        Duration::from_secs(20),
        || async move { has_owner(bus, name).await },
    )
    .await;
}

/// A process restarted whenever it exits, until dropped.
struct Supervised {
    stop: Arc<AtomicBool>,
    pid: Arc<AtomicU32>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Supervised {
    /// `make(n)` builds the command for the n-th start.
    fn start(name: &'static str, make: impl Fn(u32) -> Command + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let pid = Arc::new(AtomicU32::new(0));
        let (s, p) = (Arc::clone(&stop), Arc::clone(&pid));
        let thread = std::thread::spawn(move || {
            let mut n = 0;
            while !s.load(Ordering::SeqCst) {
                let mut child = match make(n).spawn() {
                    Ok(child) => child,
                    Err(e) => panic!("spawn {name}: {e}"),
                };
                p.store(child.id(), Ordering::SeqCst);
                let _ = child.wait();
                n += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        Self {
            stop,
            pid,
            thread: Some(thread),
        }
    }
}

impl Drop for Supervised {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let Some(thread) = self.thread.take() else {
            return;
        };
        // The thread may start one more child before it sees `stop`: keep killing the newest.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !thread.is_finished() && Instant::now() < deadline {
            let pid = self.pid.load(Ordering::SeqCst);
            if pid != 0 {
                let _ = Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .stderr(Stdio::null())
                    .status();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = thread.join();
    }
}

impl RawKeyboard {
    /// A keyboard from mutter's own RemoteDesktop API on `bus` (no portal, no dialog). The
    /// session lasts as long as the keyboard; it shows in the panel's screen-sharing indicator.
    pub async fn connect_mutter(bus: &str) -> Self {
        const RD: &str = "org.gnome.Mutter.RemoteDesktop";
        let conn = connect(bus).await;
        let path: zbus::OwnedObjectPath = conn
            .call_method(
                Some(RD),
                "/org/gnome/Mutter/RemoteDesktop",
                Some(RD),
                "CreateSession",
                &(),
            )
            .await
            .expect("mutter CreateSession")
            .body()
            .deserialize()
            .expect("CreateSession reply");
        let session = Some("org.gnome.Mutter.RemoteDesktop.Session");
        conn.call_method(Some(RD), &path, session, "Start", &())
            .await
            .expect("mutter session Start");
        let options: HashMap<&str, zbus::Variant> =
            [("device-types", MUTTER_KEYBOARD.into())].into();
        let fd: zbus::OwnedFd = conn
            .call_method(Some(RD), &path, session, "ConnectToEIS", &(options,))
            .await
            .expect("mutter ConnectToEIS")
            .body()
            .deserialize()
            .expect("ConnectToEIS reply");
        Self::from_eis(conn, fd.into()).await
    }
}

// --- gnome-shell over D-Bus (unsafe mode) ---

/// Runs `js` in gnome-shell (`org.gnome.Shell.Eval`) and returns the result as JSON. Panics
/// if it throws.
pub async fn eval(bus: &str, js: &str) -> String {
    let reply = connect(bus)
        .await
        .call_method(
            Some("org.gnome.Shell"),
            "/org/gnome/Shell",
            Some("org.gnome.Shell"),
            "Eval",
            &(js,),
        )
        .await
        .expect("org.gnome.Shell.Eval");
    let (ok, result): (bool, String) = reply.body().deserialize().expect("Eval reply");
    assert!(
        ok,
        "gnome-shell Eval failed (unsafe mode off?): {result}\n{js}"
    );
    result
}

/// [`eval`], deserialized.
pub async fn eval_as<T: serde::de::DeserializeOwned>(bus: &str, js: &str) -> T {
    let json = eval(bus, js).await;
    serde_json::from_str(&json).unwrap_or_else(|e| panic!("Eval result {json:?}: {e}"))
}

/// A window on the headless shell.
#[derive(Debug, Clone, Deserialize)]
pub struct Window {
    pub id: u64,
    pub title: Option<String>,
    pub class: Option<String>,
    pub focused: bool,
    pub minimized: bool,
    /// Mapped and drawn on screen (a new window is listed before it's shown).
    pub shown: bool,
}

const WINDOWS_JS: &str = "global.get_window_actors().map(a => a.meta_window).map(w => ({
    id: w.get_id(), title: w.get_title(), class: w.get_wm_class(),
    focused: w === global.display.focus_window, minimized: w.minimized,
    shown: !w.is_hidden() && w.get_compositor_private().is_mapped()
        && w.get_compositor_private().get_paint_opacity() === 255 }))";

pub async fn windows(bus: &str) -> Vec<Window> {
    eval_as(bus, WINDOWS_JS).await
}

/// The window titled `title`, if any.
pub async fn window(bus: &str, title: &str) -> Option<Window> {
    windows(bus)
        .await
        .into_iter()
        .find(|w| w.title.as_deref() == Some(title))
}

/// The open window of `dialog`, if any.
pub async fn dialog(bus: &str, (class, title): Dialog) -> Option<Window> {
    windows(bus)
        .await
        .into_iter()
        .find(|w| w.class.as_deref() == Some(class) && w.title.as_deref() == Some(title))
}

/// Waits up to `timeout` for `dialog` to open and be shown.
pub async fn wait_dialog(bus: &str, which: Dialog, timeout: Duration) -> Option<Window> {
    let start = Instant::now();
    loop {
        if let Some(w) = dialog(bus, which).await.filter(|w| w.shown) {
            return Some(w);
        }
        if start.elapsed() > timeout {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Activates (raises and focuses) the window with `id`, and waits until it has focus.
pub async fn activate(bus: &str, id: u64) {
    eval(
        bus,
        &format!(
            "const w = global.get_window_actors().map(a => a.meta_window)
                 .find(w => w.get_id() === {id});
             if (!w) throw new Error('no window {id}');
             w.activate(global.get_current_time()); true"
        ),
    )
    .await;
    wait_for(
        "the window to get focus",
        Duration::from_secs(5),
        || async { windows(bus).await.iter().any(|w| w.id == id && w.focused) },
    )
    .await;
}

/// How to answer a portal dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Approve,
    Deny,
}

/// Answers the RemoteDesktop dialog from the "physical" keyboard. Approving turns on "Allow
/// Remote Interaction" (off by default; *Share* stays disabled until it's on) with Tab and
/// Space, then presses Alt+S (*Share*). "Remember This Selection" and "Allow Clipboard Access"
/// keep their defaults (on). Escape denies.
pub async fn answer_remote_desktop(bus: &str, answer: Answer) {
    let keys: &[&[u32]] = match answer {
        Answer::Approve => &[&[KEY_TAB], &[KEY_SPACE], &[KEY_LEFTALT, KEY_S]],
        Answer::Deny => &[&[KEY_ESC]],
    };
    answer_dialog(bus, REMOTE_DESKTOP_DIALOG, keys).await;
}

/// Answers the "Add Keyboard Shortcuts" dialog: Alt+A (*Add*) or Escape (*Cancel*). (Return
/// would open the shortcut row's editor.)
pub async fn answer_shortcuts(bus: &str, answer: Answer) {
    let keys: &[&[u32]] = match answer {
        Answer::Approve => &[&[KEY_LEFTALT, KEY_A]],
        Answer::Deny => &[&[KEY_ESC]],
    };
    answer_dialog(bus, SHORTCUTS_DIALOG, keys).await;
}

async fn answer_dialog(bus: &str, which: Dialog, combos: &[&[u32]]) {
    let window = wait_dialog(bus, which, Duration::from_secs(20))
        .await
        .unwrap_or_else(|| panic!("no {:?} dialog appeared", which.1));
    if !window.focused {
        activate(bus, window.id).await;
    }
    // Let the dialog settle (initial focus) before the keys arrive.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut kb = RawKeyboard::connect_mutter(bus).await;
    for combo in combos {
        kb.tap(combo).await;
        // GTK updates the dialog (e.g. enables Share) between keys.
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let start = Instant::now();
    while dialog(bus, which).await.is_some() {
        if start.elapsed() > Duration::from_secs(10) {
            let dir = PathBuf::from(std::env::var("XDG_STATE_HOME").unwrap_or_default());
            screenshot(bus, &dir, "dialog-stuck").await;
            panic!(
                "the {:?} dialog didn't close: {:?}",
                which.1,
                windows(bus).await
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Screenshot of the headless screen (`org.gnome.Shell.Screenshot`) as `<name>.png` in
/// `$MURMUR_SCREENSHOTS`, or in `dir`.
pub async fn screenshot(bus: &str, dir: &Path, name: &str) -> PathBuf {
    let dir = std::env::var_os("MURMUR_SCREENSHOTS")
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.to_owned());
    std::fs::create_dir_all(&dir).expect("create screenshot dir");
    let path = dir.join(format!("{name}.png"));
    let out = path.display().to_string();
    let taken = connect(bus)
        .await
        .call_method(
            Some("org.gnome.Shell.Screenshot"),
            "/org/gnome/Shell/Screenshot",
            Some("org.gnome.Shell.Screenshot"),
            "Screenshot",
            &(false, false, out.as_str()),
        )
        .await;
    eprintln!(
        "harness: screenshot {out} ({})",
        if taken.is_ok() { "ok" } else { "failed" }
    );
    path
}

/// RemoteDesktop/ScreenCast sessions shown by the panel's screen-sharing indicator (the
/// orange "Stop Screen Sharing" button).
pub async fn sharing_sessions(bus: &str) -> u32 {
    eval_as(bus, "Main.panel.statusArea.screenSharing._handles.size").await
}

/// Clicks the panel's "Stop Screen Sharing" button, as the user would to end remote control.
/// It stops every session it shows.
pub async fn stop_sharing(bus: &str) {
    eval(
        bus,
        "Main.panel.statusArea.screenSharing._stopSharing(); true",
    )
    .await;
}

/// Whether gnome-shell and mutter would let a portal grab `accelerator` (GTK syntax): `false`
/// when one of their own shortcuts (e.g. `<Super>h`, "Hide window") already has it.
pub async fn accelerator_free(bus: &str, accelerator: &str) -> bool {
    let accelerator = serde_json::to_string(accelerator).expect("json");
    eval_as(
        bus,
        &format!(
            "const id = global.display.grab_accelerator({accelerator}, 0);
             if (id) global.display.ungrab_accelerator(id); id !== 0"
        ),
    )
    .await
}

/// Puts `data` on the clipboard as `mime_type` (default: text), owned by gnome-shell.
pub async fn copy(bus: &str, mime_type: Option<&str>, data: &[u8]) {
    let js = match mime_type {
        None => format!(
            "imports.gi.St.Clipboard.get_default().set_text(imports.gi.St.ClipboardType.CLIPBOARD, {}); true",
            serde_json::to_string(&String::from_utf8_lossy(data)).expect("json")
        ),
        Some(mime_type) => format!(
            "imports.gi.St.Clipboard.get_default().set_content(imports.gi.St.ClipboardType.CLIPBOARD, {},
                 new (imports.gi.GLib.Bytes)(new Uint8Array({}))); true",
            serde_json::to_string(mime_type).expect("json"),
            serde_json::to_string(data).expect("json")
        ),
    };
    eval(bus, &js).await;
}

/// Mime types offered on the clipboard.
pub async fn clipboard_types(bus: &str) -> Vec<String> {
    eval_as(
        bus,
        "imports.gi.St.Clipboard.get_default().get_mimetypes(imports.gi.St.ClipboardType.CLIPBOARD)",
    )
    .await
}

/// The clipboard's content as `mime_type`, if any (read by gnome-shell, like a paste).
pub async fn paste(bus: &str, mime_type: &str) -> Option<Vec<u8>> {
    let mime_type = serde_json::to_string(mime_type).expect("json");
    eval_as(
        bus,
        &format!(
            "new Promise(done => imports.gi.St.Clipboard.get_default().get_content(
                 imports.gi.St.ClipboardType.CLIPBOARD, {mime_type},
                 (_, bytes) => done(bytes ? Array.from(bytes.toArray()) : null)))"
        ),
    )
    .await
}

/// Sets a string-list GSettings key in gnome-shell (its in-memory settings).
pub async fn set_shell_strv(bus: &str, schema: &str, key: &str, values: &[&str]) {
    eval(
        bus,
        &format!(
            "new (imports.gi.Gio.Settings)({{schema_id: {}}}).set_strv({}, {}); true",
            serde_json::to_string(schema).expect("json"),
            serde_json::to_string(key).expect("json"),
            serde_json::to_string(values).expect("json"),
        ),
    )
    .await;
}

impl ChildEnv {
    /// The headless session's environment, for processes started by the re-executed test.
    pub fn gnome_env(&self) -> Vec<(String, String)> {
        display_env(&self.dir, &self.bus, &self.socket)
    }

    /// Starts xdg-desktop-portal-gnome again (after [`kill_gnome_backend`]), as D-Bus
    /// activation would in a real session. Logs to `portal-gnome-2.log`.
    pub async fn restart_gnome_backend(&self) -> HostProcess {
        let name = "portal-gnome-2";
        let child = command(PORTALS[1].1, &self.gnome_env(), &self.dir, name)
            .spawn()
            .expect("spawn xdg-desktop-portal-gnome");
        wait_name(&self.bus, BACKEND).await;
        HostProcess(name, child)
    }
}

/// Kills xdg-desktop-portal-gnome (after [`ChildEnv::assert_portals_isolated`]).
pub async fn kill_gnome_backend(bus: &str) {
    let pid = owner_pid(bus, BACKEND)
        .await
        .expect("GNOME portal backend on the private bus");
    let killed = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .is_ok_and(|s| s.success());
    assert!(killed, "kill the GNOME portal backend (pid {pid})");
}
