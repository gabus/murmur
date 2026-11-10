# Setup and Settings windows: findings

Test results for the Slint UI process (`murmur --ui setup|settings`, D-001, D-009, D-010).
The pipe protocol lives in `src/ui/protocol.rs`.

## Tested (2026-11-10, Claude)

**Offscreen (no display).** `cargo run --example ui_shots -- <dir>` renders every sample state
in light and dark with Slint's software renderer (a `MinimalSoftwareWindow` platform) and writes
PNGs. `ui::offscreen::tests` does the same as a smoke test. It also drives the setup page's model
list with key events (Down, Up, Return, Escape) and checks which model is picked.

**Live, nested KWin** (`kwin_wayland --virtual` 6.x on a private D-Bus, as in
`tests/harness/mod.rs`; the UI's stdin and stdout driven through a FIFO):

| Check | Result |
|---|---|
| Window maps with KWin server-side decorations, title "murmur" | ✅ |
| `xdg_toplevel.set_app_id("io.github.gabus.Murmur")` (seen with `WAYLAND_DEBUG=1`) | ✅ |
| Fixed size (min = max, no maximize button), and the window shrinks when the progress bar goes away | ✅ |
| Quits on `{"type":"close"}` (~0.1 s), on stdin EOF, and when the compositor closes the window | ✅ |
| stdin EOF before any state: exits without mapping a window | ✅ |
| stdout stays empty without user actions (logs go to stderr) | ✅ |

**Not tested yet:**
- Clicks. The nested session has no pointer injection; the outgoing pipe is unit-tested.
- The real desktop, the Flatpak, and GNOME.
- Following the desktop's dark mode and accent colour. Slint reads them from the Settings
  portal, which the nested session lacks. Dark rendering is checked offscreen only.
- The KDE window icon. It needs an installed `io.github.gabus.Murmur.desktop` (Stage 5).

## Slint notes

- In the `slint!` macro, a hex colour that starts with a digit and contains `e` followed by a
  non-digit (e.g. `#0000009e`) fails to compile, because Rust lexes it as a float exponent.
- `slint::set_xdg_app_id` fails until the platform exists. Create the component first, and
  call it before `show()`.
- A fixed-size window that fits its content: bind `min-height`/`max-height` to the layout's
  `preferred-height`. The winit backend resizes the window when the constraints change.
- Popups (combo box lists, the model list) are drawn inside the window on Wayland, so they must
  fit in it. The model list opens above its button when there's no room below.
- The Fluent widgets' SVG icons (check box tick, combo box arrow) need `i-slint-core/svg`. The
  winit backend enables it, so the software renderer draws them.
- Slint's platform is per thread. The offscreen renderer installs its own on the calling
  thread, so tests can render in parallel.
- Text uses the system sans-serif through fontconfig: Noto Sans on KDE and in the toolbox.
