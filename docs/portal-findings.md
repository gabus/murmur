# Portal findings (Stage 0)

Spike code: `spike/` (throwaway). Unless noted, everything was **tested on KDE Plasma 6.7.5,
KWin 6.7.5, xdg-desktop-portal 1.22.1, xdg-desktop-portal-kde 6.7.5, Fedora 44, Wayland**,
running from the `murmur-dev` toolbox as a host app. GNOME is **not tested yet**.

## Running outside Flatpak (toolbox/host)

- Host apps must register an app ID before any portal call. If they don't, `CreateSession`
  fails with `NotAllowed: An app id is required`.
- `org.freedesktop.host.portal.Registry.Register` (`ashpd::register_host_app`) works only if
  `io.github.gabus.Murmur.desktop` is installed, otherwise: `App info not found`. For
  development, a hidden (`NoDisplay=true`) entry lives in
  `~/.local/share/applications/io.github.gabus.Murmur.desktop`.

## GlobalShortcuts (portal v2)

- `BindShortcuts` with `preferred_trigger = "LOGO+h"` shows the KDE dialog once, and the
  user confirmed Meta+H. Later sessions list it as already bound.
- `Activated` and `Deactivated` both arrive. Tap: Activated → Deactivated after about 140 ms.
  KDE sends `timestamp = 0`, so we must time events ourselves. A held press was reported
  released after about 160 ms, which needs rechecking before any hold-to-talk work (Stage 7).
- A keyboard "mic" key reached through Fn: the KDE shortcut recorder does not capture Fn.
  If the key sends `XF86AudioMicMute`, Plasma already binds it to *Mute Microphone*, which
  would mute the mic murmur records from. Unverified which keysym the maintainer's key sends.

## RemoteDesktop: keysym path (`NotifyKeyboardKeysym`). **Rejected on KDE**

| Test | Result |
|---|---|
| `hello wörld ąčę 你好` | `helflo wrld   `: non-ASCII keysyms not in the active layout are dropped silently, and the ASCII letters came out scrambled |
| `The quick brown fox…` | `tHE QUICK BROWN FOX… HERE>`: Shift applied one key late and never released |
| Same at 3 ms/event pacing | Identical: deterministic, not a race |
| Shift state | Leaks across sessions and into the user's physical typing until Shift is pressed |
| Explicit Shift (keysym or keycode 42) + lowercase letter | Letters correct, but `?` (a shifted keysym through the portal) still broke it |

These are known KDE bugs:
[477981](https://bugs.kde.org/show_bug.cgi?id=477981) (Shift stuck, confirmed),
[489021](https://bugs.kde.org/show_bug.cgi?id=489021) (keysym uses the wrong layout),
[502509](https://bugs.kde.org/show_bug.cgi?id=502509) (out-of-order events).
Speed is about 3000 chars/s, which doesn't matter given the corruption.

## RemoteDesktop + `ConnectToEIS` (libei via `reis`). **Chosen**

- RemoteDesktop v2. Available device types are keyboard, pointer and touchscreen.
- `ConnectToEIS` after `Start` returns an fd. The EI handshake takes about 5 ms.
- KWin's seat advertises **`ei_keyboard` only**. The `ei_text` interface version is negotiated,
  but the seat never offers the capability, so **there is no direct UTF-8 path** on Plasma 6.7.
- The keyboard device carries the **real compositor XKB keymap** (38 KB, layouts
  `["English (US)", "Lithuanian"]` on this machine), so text can be typed with keycodes
  resolved through the actual keymap, with murmur pressing Shift/AltGr itself.
- Result: `The Quick Brown Fox? Hello, World! (ok) "quoted" 123` was typed **exactly**,
  with no stuck modifiers.
- **"Flush stall": a `reis` 0.7.1 bug, not KWin's.** After the handshake, events often
  stopped partway through the seat/device announcements until something else woke the
  client. Idle runs: 0 of 6 got a device within 40 s. A periodic `sync` poke "fixed" it
  (5 of 5 in about 5 ms). Root cause, found while testing GNOME: a lost wakeup in
  `reis::tokio::EiEventStream::poll_next`. After `read()` + `clear_ready()`, when no
  complete event is buffered it returns `Pending` without re-arming readiness, so no
  waker is registered. The poke only helped because its timer re-polled the stream.
  mutter shows the same thing (device in 2 of 8 runs without the poke; 10 of 10 in about
  3 ms with the reader fixed). The output module therefore uses its own correctly
  re-arming `AsyncFd` reader and no poke.
- Characters not in the **active** layout (`ąčę` while on *us*) can't be produced with key
  events, and EI can't switch the layout group. With *lt* active they're on the keymap and
  typeable. `你好` is on no layout. These need the clipboard fallback (README → Output), or
  the user switching layout.
- **Interleaving with physical typing:** each word goes out as one burst in under 1 ms, so the
  user's keys landed only *between* murmur's words, never inside one
  (`words zwhile zyou type`). Acceptable.
- **Held Ctrl:** while the user holds Ctrl, the typed letters become shortcuts and nothing is
  inserted. Sending Ctrl key-ups through EIS first helped only partly. The fix that will ship:
  EIS forwards the seat's modifier state (`KeyboardModifiers`), so murmur **holds back words
  while Ctrl/Alt/Super are physically down** and types them once released.
- **Restore token:** `persist_mode = 2` returns a token. The next session with the token
  starts in about 3–18 ms **with no dialog**, including across process restarts (KDE). The
  first approval took one dialog.
- **"Remote control active" indicator on KDE:** not observed by the maintainer during the
  tests (one informal answer; not explicitly checked).

## GNOME (mutter / gnome-shell 50.6, headless in the toolbox)

Tested with `spike/harness/gnome-headless.sh`: private bus with no service activation,
private runtime/HOME/XDG dirs, `GSETTINGS_BACKEND=memory`. mutter's own
`org.gnome.Mutter.RemoteDesktop` API (`CreateSession` → `Start` → `ConnectToEIS`) provides
the EIS fd with no dialog. libei 1.6.0.

- Same as KDE: the seat offers `ei_keyboard` only, with no `ei_text`. Keymap-aware typing
  of `The Quick Brown Fox? Hello, World! (ok) "quoted" 123` is **exact** on mutter and
  gnome-shell.
- `KeyboardModifiers`: one event at connect, then one on every change, including echoes
  of murmur's own Shift (0x1). The hold-back logic must look only at Ctrl, Alt and Super.
- Held Ctrl from another EIS device shows up as `depressed=0x4` about 0.1 ms later. The
  typed letters become Ctrl+letter and nothing is inserted, so the hold-back plan applies
  to GNOME too.
- Layouts (gnome-shell with `sources=[us, lt]`): the keymap has both, and `group` follows
  the active source. With lt active, `ąčę` is typed exactly; with us, they're skipped.
- **Never call the session's `SetKeymap`**: it segfaults mutter and gnome-shell 50.6 (3 of
  3). `KeymapCapabilities` is empty.
- Harness focus quirks: the headless seat has no keyboard until an EIS keyboard connects.
  gnome-shell starts in the overview, which grabs focus. A window mapped before the shell
  has seen a key event isn't reliably activated, so the harness sends one warm-up EIS
  keypress first.
- **Not tested:** the GNOME portal path (xdg-desktop-portal-gnome dialog, restore token,
  Flatpak) and a real GNOME session.

## Output module on nested KWin 6.7.5 (`tests/output_nested_kwin.rs`, tested)

- Setup (handshake → resumed keyboard) takes about 5 ms with murmur's own EI reader
  (`src/output/reader.rs`), with no pokes.
- KWin sends **no `ei_keyboard.modifiers` until the state changes**, so on a new device the
  layout group, Caps Lock and held keys are unknown. With lt active and murmur assuming us,
  digits came out as Lithuanian letters. murmur taps `<LVL3>` (evdev 84, ISO_Level3_Shift
  in every group) once per device, which makes KWin report them.
- Modifiers from other devices, including other EIS clients, are forwarded. Held Ctrl and
  held Shift hold text back (D-006). Caps Lock is planned for.
- Layout-list change (`org.kde.kconfig.notify.ConfigChanged` on `/kxkbrc`;
  `/Layouts reloadConfig` is ignored): KWin destroys the EIS keyboard, adds a new device with
  the new keymap, and resets the layout to index 0. murmur recompiles and re-probes.
- End to end (`tests/e2e_nested_kwin.rs`, `--features dev-harness`): the daemon dictated
  `en.wav` into the test pad exactly, with the model loading (1.44 s) while audio was
  buffered. Avg lag 0.93 s.

## Output review (unit-tested with a fake EIS server, `src/output/test_eis.rs`)

- Replaying every typeable character through xkb across 13 layout/option combos (Caps on/off,
  Num Lock on) gives exact text, never activates Ctrl/Alt/Super, and leaves no modifier down.
- Waiting covers locked and latched modifiers, Neo's level 5 and level lock (D-007).
- A device replaced during a `sync` round-trip is re-probed. A held AltGr at the first word
  (the probe can't see it, because KWin reports changes only) makes text wait.
- The session `Closed` handling was later tested against the real KDE portal (next section).
  **Still untested:** reading the portal version when the property can't be read (the
  version-1 fallback path).

## Real KDE portal in a nested KWin (`tests/portal_nested_kde.rs`, tested)

The host's **xdg-desktop-portal 1.22.1 (with its xdg-permission-store), xdg-desktop-portal-kde
6.7.5, KWin 6.7.5** (Fedora 44; wl-clipboard 2.2.1, spectacle 6.7.5 for checks), started on the nested KWin's private bus and display, with murmur's
`EisSource::Portal` running from the toolbox as a host app. The dialogs appear only in the nested
KWin and are answered with keys from a second EIS client. **Not covered:** the Flatpak sandbox
(murmur runs as a host app here), a real Plasma session (no plasmashell, so no tray, no
Klipper), GNOME.

- Exposed: `RemoteDesktop` version 2 (`AvailableDeviceTypes` = 7) and `Clipboard` version 1.
- **Dialog** ("Remote Control: murmur is asking for special privileges: • Control input
  devices"): *Allow restoring on future sessions* is checked by default; buttons Approve and
  Deny. Return approves, Escape denies (`Start` response 1 → `PermissionDenied`; the portal
  then emits the session's `Closed`). It opened on top of the test pad and active in the
  screenshots; the test still activates it through KWin's window runner before pressing a key,
  so the key can't land in the pad.
- **The dialog doesn't mention the clipboard.** With `RequestClipboard`, Approve grants it
  (`clipboard_enabled = true`) without telling the user.
- **Restore token:** the next session with it is ready in about 15 ms with no dialog. KDE hands
  back the **same token value** each time.
- **A token granted without clipboard access re-prompts** (answers the open question): when
  murmur asks for the clipboard with such a token, KDE shows the dialog again, and after Approve
  the clipboard is granted and the token value stays the same. Users with a token from before
  D-011 see the dialog once more.
- **`SelectionOwnerChanged` is not sent after `Start`**, even when another app owns the
  clipboard. It arrives when another app copies (`session_is_owner = false` + its mime types)
  and after murmur's own `SetSelection` (`session_is_owner = true`).
- `SelectionRead` and `SelectionWrite` both hand out **pipes (FIFOs)**. Reads are answered in
  about 1 ms; an empty clipboard answers with an empty pipe.
- **`SelectionRead` for a mime type the owner doesn't offer is never answered** (no D-Bus reply
  at all; later calls still work). Bug found and fixed in `src/output/clipboard.rs`: murmur
  waited for the reply without a timeout, so with an image on the clipboard at session start
  (owner unknown, murmur tries the text types) **every untypeable word stalled 3.0 s** before
  being typed without the missing characters. The same would happen with an app that offers
  fewer text types than murmur tries (likely Qt apps: `text/plain` and
  `text/plain;charset=utf-8` only; **not tested**), where the paste then also fails. Now,
  with the owner unknown, murmur tries the text types at once and waits at most 300 ms for
  answers. It keeps the types that answered; if none did, the content can't be restored, so
  nothing is pasted (D-012) and murmur doesn't ask again until the owner changes (each
  unanswered call stays pending in the portal). Measured: 0.30 s for the first such word,
  about 0 s for the next, image kept. Announced types are still read one at a time (1 s each),
  and one that times out now blocks the paste instead of being dropped from the restore.
- **Paste works:** `mąstau ` (layout us) pasted exactly, about 110 to 160 ms per pasted word,
  both with the owner unknown (copied before `Start`) and announced (copied during the
  session). The previous text clipboard reads back afterwards (served by murmur). When
  murmur's session ends, KDE clears a selection murmur owned ("Nothing is copied").
- **Caps Lock + Ctrl+V:** the Slint test pad ignores it (Slint 1.18 matches the shortcut on the
  key's text, `"V"` with Caps Lock). murmur then falls back as designed: `ia ` typed,
  `Unmapped { count: 1 }`, clipboard restored. Qt and GTK match the key instead; **not tested**.
- **Session closed by the user:** the backend shows a "Remote Control" StatusNotifierItem with
  one menu entry, *End*. Clicking it (dbusmenu `Event clicked`) makes the portal emit `Closed`
  → murmur reports `Disconnected("the RemoteDesktop session was closed")`. The next word
  reconnects with the token, with no dialog.
- **Backend killed:** the frontend emits no `Closed`, but KWin drops the backend's EIS clients →
  `Disconnected("EIS disconnected …")`. Reconnecting without a backend gets response code 2 →
  `Unavailable` (in a real session, D-Bus activation would restart the backend instead). With
  the backend back, `retry` reconnects with the token and no dialog.
- **Still untested:** Ctrl+V on a non-Latin layout, a clipboard manager (Klipper) reading the
  offer, timing margins under load (`SETTLE` 50 ms, `PASTE_TIMEOUT` 1.5 s), the version-1
  fallback, the Flatpak sandbox, GNOME.

## Automated testing without the user's desktop

`kwin_wayland --virtual --socket murmur-test-0` inside `dbus-run-session` starts an invisible
KWin with its own bus. It exports `org.kde.KWin.EIS.RemoteDesktop.connectToEIS(i caps) → (h fd,
i cookie)` with no portal and no dialog. A test client on that compositor gets focus and
receives the typed text. This is the basis for the output integration tests (Stage 1).

The real portal stack runs in the same nested session (`tests/portal_nested_kde.rs`): the
private bus has no service activation, so the harness starts `xdg-permission-store`,
`xdg-desktop-portal-kde` and `xdg-desktop-portal` from the host itself (through
`flatpak-spawn --host --clear-env`, with the nested bus, `WAYLAND_DISPLAY`,
`XDG_CURRENT_DESKTOP=KDE`, `QT_QPA_PLATFORM=wayland` and private XDG dirs, so permissions and
tokens stay in the test directory) and waits for their names. It writes the app's desktop
file into the private `XDG_DATA_HOME` so `Registry.Register` works. Before typing it checks that
the portal processes' environment names the nested bus and display (the toolbox can't read a
host process's `/proc/<pid>/environ`, so the harness reads it on the host). Dialogs are found
and activated through KWin's window runner (`org.kde.KWin /WindowsRunner org.kde.krunner1
Match/Run`, filtered by the backend's resource class) and answered with Return or Escape from
a second EIS client. `spectacle -b -n -f -o` on the nested bus and display takes screenshots
(`MURMUR_SCREENSHOTS`), and `wl-copy`/`wl-paste` from the host work on the nested KWin
(`ext_data_control_v1`).

## Audio (cpal 0.18, `pulseaudio` feature: pure-Rust client, no libpulse)

- Default host: PulseAudio (on PipeWire 1.6.10). 16 kHz mono f32 is accepted directly.
- `BufferSize::Default` gives **1.02 s callbacks and about 2.06 s to open**, which is unusable.
  `BufferSize::Fixed(320)` (20 ms) gives **about 52 ms to open, then 20 ms callbacks**.
  `play()` blocks until the first buffer, so open the stream on the audio thread.
- The maintainer's default source (ArctisX headset) delivered digital silence at the time
  (also with `pw-record`). That's the hardware mute, not murmur.

## Slint (D-001)

- The Slint 1.18 test pad (`renderer-software` + `backend-winit-wayland`) runs on KDE Wayland
  from the toolbox. A new window does **not** take focus over the active app (KWin
  focus-stealing prevention), so tests must never assume the test pad has focus on a real desktop.

## Flatpak

- The spike builds and installs as `io.github.gabus.Murmur` (freedesktop 26.08 + rust-stable,
  network at build time for the spike only), with `--socket=wayland --socket=pulseaudio
  --talk-name=org.kde.StatusNotifierWatcher` only.
- **Tested in the sandbox (KDE 6.7.5):**
  - RemoteDesktop + ConnectToEIS: keyboard device ready in about 5 ms, keymap `["English (US)",
    "Lithuanian"]`. The restore token from the host run was accepted, so no dialog (permissions
    are keyed by app ID, which is the same for both).
  - GlobalShortcuts: `ListShortcuts` returns the existing Meta+H binding, and `BindShortcuts`
    returns in about 1 ms with no dialog.
  - Mic: cpal's PulseAudio host is *available* (`PULSE_SERVER=unix:/run/flatpak/pulse/native`)
    but fails to connect. Not root-caused yet; probably the pure-Rust client's auth handshake.
    cpal falls back to **ALSA (runtime pulse plugin)**, which works: open in about 38 ms,
    213-sample callbacks. The audio module must try PulseAudio, then ALSA, and log why.

## Go / no-go

**Go**, with **RemoteDesktop portal → ConnectToEIS → keymap-aware key events** as the typing
mechanism ([D-005](decisions.md)). The keysym path is not used. An input-method engine
(Stage 7) remains the only way to get preedit text and corrections.
