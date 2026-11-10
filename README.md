# murmur

Live voice dictation for Linux on Wayland. Press a key, talk, and your words are
typed into whatever app has focus. Press the key again to stop. You can keep
typing normally the whole time.

> **Status:** alpha. Dictation works end to end on KDE Plasma 6 (verified in automated nested-KWin
> tests); Flatpak packaging and release are in progress. See [TODO.md](TODO.md).

## Goals

- **Mac-like dictation.** Press a shortcut, then speak. You see the transcription
  as you speak, and text appears in the focused app.
- **One-click install.** Ships as a Flatpak (a `.flatpakref` from the project's own repo,
  [D-015](docs/decisions.md)). No compilers, no `input` group, no udev rules, no `sudo`.
- **Wayland only.** Uses xdg-desktop-portals and no X11 fallbacks.
- **Lightweight.** Nothing is loaded while idle. The mic is opened only while
  listening. The model is loaded on demand and unloaded after a period of
  inactivity.
- **Local and private.** All speech recognition runs on your machine. The only
  network access is the one-time model download. No telemetry.


## Install

```sh
flatpak install --user https://gabus.github.io/murmur/murmur.flatpakref
```

Or open `murmur.flatpakref` from the [project page](https://gabus.github.io/murmur/) or the
[latest release](https://github.com/gabus/murmur/releases) in Discover or GNOME Software. The
first start opens a one-page setup: shortcut, typing permission, speech model. After that,
murmur lives in the tray. Requires KDE Plasma 6.1+ or GNOME 48+ on Wayland (see Supported
desktops).

> **About this project:** murmur's code, tests and packaging were written with AI assistance
> (Anthropic's Claude), directed and reviewed by the maintainer. It is distributed from its own
> Flatpak repo, not Flathub ([D-015](docs/decisions.md)).

## Specification

### UX specification

murmur behaves like macOS dictation: one key starts it, the same key stops it,
and in between it stays out of the way.

#### Principles

- **The user is in control.** Listening starts and stops only when the user
  presses the shortcut or clicks the tray icon. murmur never stops on its own
  (no silence timeout) and never starts on its own.
- **Nothing is lost.** Audio is recorded from the moment the key is pressed, even
  while the model is still loading, and is transcribed once the model is ready.
- **No interruptions.** No popups, preview windows, sounds or notifications in
  normal use. The tray icon is the only feedback. Notifications appear only for
  errors that need action (no mic, permission revoked, model missing).
- **Typing and talking mix freely.** The keyboard and mouse work normally while
  listening. Each word is inserted at the cursor, wherever focus is at that
  moment.
- **Readable output by default.** The default model must produce punctuation and
  capitalization.
- **Setup happens once.** The shortcut, typing permission and model download are
  handled in first-run setup, never during the first dictation.

#### First run

A one-time setup window, all on one page ([D-009](docs/decisions.md)):

1. **Shortcut.** Portal dialog where the user picks a key (e.g. `Meta+H`), with a Change button.
2. **Typing permission.** An "Allow…" button opens the RemoteDesktop portal dialog. The
   permission is remembered, so it is never asked again.
3. **Model.** The default model starts downloading as soon as the window opens, with a
   progress bar and an option to pick another.
4. **Start at login** checkbox (off by default).

After setup the window closes, and murmur lives only in the tray. **Settings…** in the tray
menu opens a small window with the same look: shortcut, microphone, model, the pause that ends
a sentence, idle unload time and start at login ([D-010](docs/decisions.md)).

#### Dictating

| User does | murmur does |
|---|---|
| Presses shortcut | Icon switches to *listening* immediately and the mic opens. If the model isn't loaded, it loads in the background while audio is buffered. |
| Speaks | Words appear at the cursor as you speak, about 1 second behind your voice. Icon reacts to voice level. |
| Pauses | The last words and the sentence's final punctuation are typed. |
| Types, clicks, switches apps | Nothing changes. Listening continues, and the next phrase goes wherever the cursor is. |
| Presses shortcut again | The phrase in progress is finished and inserted, then the mic closes and the icon goes back to *idle*. |
| Clicks tray icon | Same as the shortcut. Right-click opens the menu. |

#### Tray icon states

| State | Look |
|---|---|
| Idle | Monochrome mic, matching the panel |
| Loading | Subtle animation (only seen if loading takes longer than ~300 ms) |
| Listening | Accent/red mic that reacts to voice level |
| Error | Mic with warning badge; tooltip says what's wrong |

#### Real-time typing

Words are typed as you speak, **about 1 second behind your voice**, and once typed
they are **never changed**: no backspacing, so nothing breaks if you move the
cursor or type yourself. The 1 s delay is deliberate. It gives the model time to
settle, so most corrections (e.g. "I scream" → "ice cream") happen before a word
is typed.

Two engines:

- **Re-check (default, tentative).** Every ~0.5 s an accurate offline model
  (Parakeet) re-transcribes the sentence so far. A word is typed once two
  consecutive runs agree on it (the "local agreement" policy from whisper_streaming).
  Punctuation and casing come from the model itself. The audio buffer is trimmed at
  each finished sentence. Costs CPU while speaking.
- **Light.** A streaming transducer with greedy decoding (append-only output). A word
  is typed ~1 s after it's recognized, with punctuation/casing added by a separate
  step. For weaker machines.

On a pause, or when the user stops, the remaining words and final punctuation are
typed. The delay is configurable (default ~1 s).

**Difference from macOS:** macOS shows provisional words and can still correct
them afterwards. murmur catches most corrections during the 1 s wait; anything the
model changes its mind about later stays as typed.

#### Known limitations of keystroke output

murmur inserts text by sending keystrokes through the RemoteDesktop portal. That
works in every app, but it can't be fully seamless:

- **Collision.** Each word is sent as one burst (under 1 ms), so the user's own keystrokes can
  land between murmur's words but not inside them.
- **Layout.** Characters that aren't on the active keyboard layout (e.g. `ą` while on *US*)
  are pasted through the clipboard, which is restored afterwards. A clipboard manager may
  still record them.
- **Held modifiers.** If Ctrl/Alt/Super were physically held while murmur typed,
  the letters would act as shortcuts (and Shift would change their case). murmur sees held,
  latched and locked modifiers through EIS and waits until they are released before typing.
- **No context.** murmur can't read the text around the cursor, so spacing and
  capitalization after the user's own edits are a guess.
- **Password fields.** murmur can't detect them.
- **No corrections.** Words are typed once. Earlier words can't be fixed when
  later context arrives, as macOS does.

An input-method engine (Fcitx5 on KDE, IBus on GNOME) solves all five: text is
committed in one piece through the text-input protocol, the field type and
surrounding text are known, and live text can be shown underlined in the field
(as on macOS). See the open questions in TODO.md.

### Functional requirements

| Area | Requirement |
|---|---|
| Hotkey | Global toggle shortcut via the `org.freedesktop.portal.GlobalShortcuts` portal. User-rebindable through the desktop's settings. |
| Audio | Capture the default PipeWire/Pulse source at 16 kHz mono. The stream is open **only while listening**. Optional source picker. |
| VAD | Silero VAD for end-of-phrase detection only. Never stops listening. |
| ASR | sherpa-onnx. Default: offline model re-run every ~0.5 s on the current sentence, committing words two runs agree on. Light: streaming transducer, greedy decoding, plus a punctuation/casing step. |
| Output | Type text through the `org.freedesktop.portal.RemoteDesktop` portal's EIS channel (`ConnectToEIS`, libei): key events resolved through the compositor's XKB keymap and active layout, with murmur pressing Shift/AltGr itself ([D-005](docs/decisions.md)). Words wait while held, latched or locked modifiers could change what's typed ([D-007](docs/decisions.md)); after ~2 s the tray shows *Waiting*. If typing is denied, murmur asks again only on the next dictation. A word containing characters missing from the active layout is pasted whole through the clipboard (Ctrl+V; `paste_keys = "ctrl+shift+v"` in the config for terminals), then the previous clipboard content is restored and served by murmur until something else is copied ([D-011](docs/decisions.md)). Every typed word is followed by one space ([D-002](docs/decisions.md)). |
| Indicator | StatusNotifierItem tray icon with idle / loading / listening / error states. While listening, the icon reacts to mic level. The menu has start/stop, model picker, settings and quit. |
| Models | Download on demand into `$XDG_DATA_HOME/murmur/models` with checksum verification. Show size before download. Delete from menu. |
| Resources | Idle: no audio stream, no model in memory, ~0% CPU. Unload the model after configurable idle time (default 10 min). |
| Autostart | Optional start at login via the `org.freedesktop.portal.Background` portal. |
| Config | Single TOML file in `$XDG_CONFIG_HOME/murmur/config.toml`: `model`, `pause_secs`, `idle_unload_minutes`, `microphone`, `autostart`, `paste_keys`, `setup_done`. Most are also in the Settings window. |

### Supported models

| Model | Mode | Download | Notes |
|---|---|---|---|
| Parakeet TDT 0.6B v3 (int8) | re-check | 490 MB | **Default.** Most accurate; built-in punctuation + casing. 25 European languages, auto-detected. |
| Moonshine base v2 (en) | re-check | 110 MB | Lighter, English. Punctuation + casing built in. No timestamps, so words are aligned by order. |
| Moonshine tiny v2 (en) | re-check | 30 MB | Lightest download, English. Less accurate. |
| Light: streaming Zipformer en (2023-06-26) | light | 310 MB + 31 MB | Lowest CPU (~5 % of real time). Punctuation/casing from a separate model ([D-008](docs/decisions.md)). |

All models are downloaded on demand from the sherpa-onnx releases, with the sizes and
checksums pinned in `src/models.rs`. Measurements are in [docs/benchmarks.md](docs/benchmarks.md).

### Supported desktops

| Desktop | Hotkey | Typing | Tray | Notes |
|---|---|---|---|---|
| KDE Plasma 6.1+ | ✅ | ✅ | ✅ | Primary target. Typing needs EIS in RemoteDesktop (6.1+); tested on 6.7 |
| GNOME 48+ | ✅ | ✅ | ⚠️ needs AppIndicator extension | GNOME may show a "remote control" indicator while typing |
| Hyprland | ✅ | ⚠️ to verify | ✅ (with a tray bar) | |
| Sway / wlroots | ❌ no GlobalShortcuts portal | ⚠️ | ✅ | Unsupported |

## Toolset

| Concern | Choice |
|---|---|
| Language | Rust (stable) |
| Async runtime | `tokio` |
| Portals | [`ashpd`](https://crates.io/crates/ashpd): GlobalShortcuts, RemoteDesktop, Background |
| Tray | [`ksni`](https://crates.io/crates/ksni) (StatusNotifierItem, no Qt/GTK) |
| Setup & Settings windows | [Slint](https://slint.dev), software renderer: no GPU permission ([D-001](docs/decisions.md)) |
| Audio | `cpal`, or `libpulse-binding` if cpal misbehaves inside the sandbox |
| ASR + VAD | [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) via `sherpa-rs` (to evaluate) or own FFI to its C API |
| Inference | onnxruntime, CPU. Dev builds: sherpa-onnx's prebuilt static libs (Cargo feature `sherpa-static`, default). Flatpak: ONNX Runtime and the sherpa-onnx C API built from source as shared libs (`sherpa-shared`) |
| Config | `serde` + `toml` |
| Model download | `reqwest` + `sha2` |
| Logging | `tracing` |
| Packaging | Flatpak: `org.freedesktop.Platform` / `Sdk` 26.08 + `rust-stable` extension, `flatpak-cargo-generator`. Builds offline, everything from source ([docs/packaging.md](docs/packaging.md)) |
| App ID | `io.github.gabus.Murmur` |

### Flatpak permissions

```
--socket=wayland
--socket=pulseaudio          # microphone
--share=network              # model download only
--talk-name=org.kde.StatusNotifierWatcher
```

Hotkey, typing and autostart all go through portals, so they need no extra
permissions. Explicitly **not** requested: `--device=all`, `--filesystem=host`,
X11. Never used: `/dev/uinput`, evdev, `dotool`/`ydotool`, the `input` group or
udev rules. They would break the one-click install.

## Architecture

The daemon is a tokio app. Blocking work runs on dedicated threads: audio capture, and the
ASR worker that owns the models. The setup and settings windows run in a separate short-lived
process (`murmur --ui …`), so the daemon never loads a GUI toolkit while idle.

```
src/
  main.rs          # startup, wiring; `--ui` runs the window process
  app.rs           # state machine Idle ⇄ Listening; window state and requests
  instance.rs      # single instance (D-Bus name), Activate/Toggle methods
  hotkey.rs        # GlobalShortcuts portal (bind, change, activations)
  output/          # typing: RemoteDesktop portal → EIS (libei), keymap planning,
                   #   modifier hold-back, clipboard paste fallback
  audio.rs         # mic capture (cpal), 16 kHz mono, open only while listening
  agc.rs           # gain control for quiet microphones
  vad.rs           # Silero VAD (end of phrase only)
  asr/             # engines: re-check (Parakeet, Moonshine) with local-agreement
                   #   committer; light streaming engine; worker thread
  models.rs        # registry (URLs, sizes, sha256), engine loading
  model_manager.rs # active model, downloads, config persistence
  download.rs      # streaming download + sha256 + atomic extract
  tray.rs, icons.rs# StatusNotifierItem tray, drawn state icons
  notify.rs        # error notifications (Notification portal)
  autostart.rs     # Background portal
  config.rs        # $XDG_CONFIG_HOME/murmur/config.toml
  ui/              # Slint windows (setup, settings) + daemon↔UI protocol
  ui_host.rs       # spawns and drives the window process
```

## Development

### Prerequisites (Fedora)

Nothing is installed on the host with `dnf`. Build tools live in a
[toolbox](https://containertoolbx.org/) container and in Flatpak.

```sh
# dev container (one time): Rust + C++ toolchain, isolated from the host
toolbox create murmur-dev
toolbox run -c murmur-dev sudo dnf install -y rust cargo rustfmt clippy gcc-c++ cmake clang pkg-config \
  git alsa-lib-devel pulseaudio-libs-devel libxkbcommon-devel wayland-devel fontconfig-devel \
  python3-aiohttp python3-tomlkit   # last two: flatpak-cargo-generator
# headless compositor tests (nested KWin runs from the host; GNOME runs in the toolbox)
toolbox run -c murmur-dev sudo dnf install -y mutter gnome-shell dbus-daemon \
  gsettings-desktop-schemas dconf systemd libei-utils python3-gobject-base wev

# flatpak builds (one time)
flatpak install flathub org.flatpak.Builder \
  org.freedesktop.Sdk//26.08 org.freedesktop.Sdk.Extension.rust-stable//26.08
```

The toolbox shares the home directory, Wayland session, D-Bus and PipeWire, so
murmur run from it can reach the portals and the mic (to verify in stage 0).
Delete it at any time with `toolbox rm -f murmur-dev`.

### Build & run

```sh
T="toolbox run -c murmur-dev"
$T cargo run                    # the daemon (first run opens the setup window)
$T cargo test                   # unit tests
bash .claude/skills/murmur-gate/gate.sh   # fmt + clippy -D warnings + tests

# integration tests in an invisible nested KWin (never touch the real desktop);
# need the spike test pad: (cd spike && $T cargo build)
$T cargo test --test output_nested_kwin -- --ignored
$T cargo test --test portal_nested_kde -- --ignored   # real KDE portal: dialog, token, paste
$T cargo test --features dev-harness --test e2e_nested_kwin -- --ignored   # needs models

# dev tools
$T cargo run --release --example simulate -- --model parakeet-v3 file.wav   # engine on WAVs
$T cargo run --example ui_shots -- /tmp/shots                                # window screenshots

# after every Cargo.lock change: refresh the vendored crate list for the Flatpak
$T flatpak/update-cargo-sources.sh

# Flatpak (from the host): offline, everything from source
flatpak run org.flatpak.Builder --user --install --force-clean --sandbox \
  --state-dir=.flatpak-builder build-dir io.github.gabus.Murmur.yml
flatpak run io.github.gabus.Murmur
```

The `dev-harness` cargo feature (never shipped) adds `MURMUR_DEV_AUDIO=<wav>` (a WAV instead of
the mic), `MURMUR_DEV_EIS=kwin-direct`, `MURMUR_DEV_NO_HOTKEY` and `MURMUR_DEV_AUTORUN`.

The first Flatpak build compiles ONNX Runtime and sherpa-onnx (about 10 minutes in total on
the maintainer's 16-thread machine). flatpak-builder caches each module in `.flatpak-builder/`,
so later builds only rebuild murmur unless those modules change. CI builds the same bundle on
every push (`.github/workflows/flatpak.yml`). Details and Flathub notes:
[docs/packaging.md](docs/packaging.md).

## Implementation stages

See [TODO.md](TODO.md).

## License

GPL-3.0-or-later ([LICENSE](LICENSE), [D-013](docs/decisions.md)). The speech models are
downloaded separately and carry their own licenses (Parakeet: CC-BY-4.0; Moonshine English:
MIT; Zipformer/punctuation: Apache-2.0).
