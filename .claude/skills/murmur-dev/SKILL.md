---
name: murmur-dev
description: How to build, run and test murmur during development. Covers the murmur-dev toolbox, cargo commands, dev model cache, and safe live tests of keystroke injection. Use before running any cargo/build/test command in this repo.
---

# murmur dev environment

All Rust/C++ tooling lives in the `murmur-dev` toolbox. **Never install packages on the host.**
Never run `cargo` on the host either, because it isn't installed there.

## Commands (run from the host via Bash)

```sh
T="toolbox run -c murmur-dev"
$T cargo build                         # in /home/gabe/Projects/murmur (home is shared)
$T cargo test
$T cargo run -- <args>
$T bash -lc 'cd spike && cargo run -- <args>'   # stage-0 spike
```

- The toolbox shares `$HOME`, the Wayland session, the session D-Bus and PipeWire, so portals and
  the mic work from `cargo run` (confirmed in stage 0, see `docs/portal-findings.md`).
- To add toolbox packages, use `$T sudo dnf install -y <pkg>`, then update README → Development
  in the same change. README owns that list.
- Long builds (sherpa-onnx, Slint, a cold `cargo build`) go in the background
  (`run_in_background: true`). Wait for the notification instead of polling.

## Host app ID (toolbox runs) and cleanup

- Portals need the app ID's desktop entry. Use the **installed Flatpak's exported entry**
  (`~/.local/share/flatpak/exports/share/applications/io.github.gabus.Murmur.desktop`). Never
  put an `io.github.gabus.Murmur.desktop` in `~/.local/share/applications`: it shadows the
  Flatpak's, and the maintainer's menu/Discover then runs that file's `Exec` (once `Exec=true`,
  so clicking murmur did nothing).
- After any Flatpak smoke test, run `flatpak kill io.github.gabus.Murmur`. Killing `flatpak run`
  leaves the sandboxed murmur alive (several leaked, about 1 GB of RAM each). Also kill the test's
  private dbus-daemon and nested KWin, and check with `pgrep -fa murmur`.

## Dev model cache

Real runs read models from `$XDG_DATA_HOME/murmur/models` (README → Models). For tests and
benchmarks, download the archives once into that same directory. Never commit model files.
`.gitignore` already excludes `*.onnx` and `models/`.

## Live tests that type into the desktop: safety rules

The RemoteDesktop portal types into **whatever window has focus**, which on this machine is the
maintainer's real desktop.

1. Only type strings that are harmless anywhere. Never type `\n`/Enter, Tab or modifier combos,
   and nothing that would be a valid shell command.
2. Prefer the test pad (`spike testpad`, later `cargo run --example testpad`). It is a window
   with one text field that mirrors its contents to a file, so results can be checked without
   screenshots. Launch it, wait for it to take focus, then inject.
3. Tests that need a human (portal permission dialogs, picking a shortcut, physically
   typing or holding Ctrl, speaking) are run *with* the maintainer. Before starting, use
   AskUserQuestion to say exactly what will appear and what to click.
4. Portal dialogs block until answered. Run the binary in the background with a timeout so a
   missed dialog never hangs the session.

## Screenshots (KDE)

`spectacle -b -n -f -o <scratchpad>/shot.png`, then Read the PNG. Use this only when the test pad
can't show the result.
