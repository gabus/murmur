# TODO

Stages are in order. Each stage ends with something runnable.

## Stage 0: Portal spike (de-risk)

Goal: prove the two uncertain pieces before writing the real app.

- [x] Create `murmur-dev` toolbox with the Rust + C++ toolchain; install `org.flatpak.Builder` + SDK 26.08 + rust-stable from Flathub
- [x] Confirm portals (GlobalShortcuts, RemoteDesktop) and mic work from `cargo run` inside the toolbox
- [x] `cargo new` a throwaway `spike/` binary
- [x] GlobalShortcuts portal via `ashpd`: register a "toggle" shortcut and print on activation
- [x] RemoteDesktop portal via `ashpd`: request a keyboard-only session and type `hello wörld ąčę 你好` (keysym path broken on KDE → EIS, see findings)
- [ ] Check whether `persist_mode` / `restore_token` avoids re-prompting after restart (KDE ✅ + GNOME)
- [x] Note any "remote control active" indicator shown by the desktop
- [x] Inject a phrase while physically typing: how bad is interleaving?
- [x] Inject a phrase while physically holding Ctrl: does it trigger shortcuts? Can sending explicit modifier-up events first prevent it safely?
- [x] Measure injection speed (chars/s) for a 100-char phrase
- [x] Wrap the spike in a minimal Flatpak manifest and confirm both portals work sandboxed
- [x] Test on KDE Plasma 6 (primary ✅) and GNOME 48+ (headless mutter/gnome-shell 50.6 ✅; portal dialog + restore token on GNOME still untested → VM)
- [x] Spike EIS typing (ConnectToEIS + reis + keymap-aware keycodes): works on KDE and GNOME; the apparent stall was a reis lost-wakeup bug
- [x] Write findings in `docs/portal-findings.md`, then go / no-go on the typing approach (go: EIS, D-005)

## Stage 1: Core dictation (CLI)

Goal: `murmur` daemon, hotkey toggles listening, phrases get typed.

- [x] Project skeleton: `tokio`, `tracing`, module layout (`audio`, `vad`, `asr`, `output`, `hotkey`, `app`)
- [x] Evaluate `sherpa-rs` vs. own FFI to the sherpa-onnx C API → neither: the official `sherpa-onnx` crate (2.0.1) covers offline/online ASR, VAD, punctuation; `sherpa-rs` is archived. Flatpak: `shared` feature + `SHERPA_ONNX_LIB_DIR` from a source-built module (Stage 5)
- [x] Audio capture at 16 kHz mono with `cpal`; stream opened only on start, dropped on stop
- [x] Silero VAD: segment speech, emit end-of-phrase events
- [x] Re-check engine: re-run Parakeet TDT on the current sentence buffer every ~0.5 s
- [x] Local-agreement committer: type words that two consecutive runs agree on; flush on pause / stop
- [x] Trim the buffer at committed sentence ends so each run stays short
- [x] Output: type each committed word through the RemoteDesktop portal, with spacing (EIS, D-005/D-006; nested-KWin tests)
- [x] Measure: lag from spoken word to typed word, CPU while speaking, Parakeet run time per buffer length (`docs/benchmarks.md`)
- [x] End-to-end check of the daemon (WAV → engine → typed text) in a nested KWin (`tests/e2e_nested_kwin.rs`)
- [x] Verify the daemon inside the Flatpak: real session → shortcut bound + EIS typing ready via restore token (no dialogs); private bus → tray toggle, mic (ALSA), model load and transcription in the sandbox
- [x] State machine: `Idle → Listening(+Loading) → Idle`, toggled by the shortcut only, never by silence
- [x] Ring-buffer audio from key press so speech during model load isn't lost (worker channel buffers everything from Begin)
- [x] On stop: flush the phrase in progress (transcribe + insert), then close the mic (250 ms tail)
- [x] Hard-coded model path for now

## Stage 2: Tray

- [x] `ksni` tray icon with states: idle, loading, listening, error
- [x] Icons (light/dark friendly): idle = the theme's `audio-input-microphone-symbolic`; other states are pixmaps drawn in code (`src/icons.rs`), because the host panel can't read icon files inside the Flatpak
- [x] Menu: Start/Stop, Quit (Model ▸ arrives with stage 3, Settings with stage 4)
- [x] Listening icon reacts to mic level (a few pre-rendered levels; update at most ~10×/s)
- [x] Notification on errors (mic missing, portal denied, model missing) via the Notification portal

## Stage 3: Models

- [x] Model registry (name, mode, URL, size, sha256, language)
- [x] Download on demand with progress, then verify checksum
- [x] Store under `$XDG_DATA_HOME/murmur/models`
- [x] Model picker in tray menu, with delete option
- [x] Light engine: streaming Zipformer transducer, greedy decoding, ~1 s commit delay, punctuation/casing step
- [x] Verify light engine output is append-only (log any case where earlier tokens change)
- [x] Moonshine as a lighter re-check model
- [x] Lazy load on first activation; unload after idle timeout
- [x] Measure: load time, RAM, latency to text, CPU while listening (record in `docs/benchmarks.md`)

## Stage 4: Polish

- [x] Config file `$XDG_CONFIG_HOME/murmur/config.toml` (model, pause, idle unload, microphone, autostart, paste keys; spacing is fixed by D-002)
- [x] First-run setup window: shortcut → typing permission → model download (one time only); D-009, verified in a nested KWin with the real daemon
- [x] Settings window (D-010)
- [ ] Verify the paste fallback against the real Clipboard portal (KDE, then GNOME)
- [ ] Investigate cpal PulseAudio host failing inside the Flatpak (now logged)
- [x] Left-click tray icon toggles listening; right-click opens menu
- [x] Optional audio source picker (config + Settings window)
- [x] Clipboard + paste fallback for characters the active layout can't produce (D-011/D-012; real-portal check pending)
- [x] Autostart via Background portal (opt-in)
- [x] Single-instance guard (D-Bus name; a second start opens Settings; `Toggle` exposed on D-Bus)
- [x] Graceful handling of portal denial and session loss
- [x] Basic punctuation/capitalization handling (model-dependent; light engine adds final punctuation at pauses)

## Stage 5: Packaging

- [ ] Flatpak manifest `io.github.gabus.Murmur.yml` (Platform/Sdk 26.08, rust-stable)
- [ ] `flatpak-cargo-generator` for offline cargo sources
- [ ] Decide how onnxruntime / sherpa-onnx are built (from source likely required for Flathub)
- [ ] AppStream metainfo, `.desktop` file, icons (scalable + 128px)
- [ ] Minimal permissions (see README); no `--device=all`, no `--filesystem=host`
- [ ] CI: GitHub Actions building the Flatpak bundle on push
- [x] License chosen and added (GPL-3.0-or-later, D-013)

## Stage 6: Release

- [ ] Tag `v0.1.0`; GitHub release with `.flatpak` bundle + `.flatpakref`
- [ ] Install instructions in README
- [ ] Screenshots / short GIF
- [ ] Submit to Flathub; iterate on review

## Stage 7: Later

- [ ] Fcitx5 engine for in-field live (preedit) text, as a separate non-Flatpak component
- [ ] GPU (CUDA) inference option
- [ ] Languages beyond Parakeet v3's 25 European ones (e.g. Whisper or language-specific sherpa-onnx models)
- [ ] Hold-to-talk mode in addition to toggle (if the portal exposes key release)

## Open questions

- [ ] Does the RemoteDesktop portal remember permission across restarts on KDE / GNOME? (KDE: yes, see findings)
- [ ] Does the GlobalShortcuts portal report key release (needed for hold-to-talk)?
- [x] Is `sherpa-rs` complete enough for streaming + VAD, or do we need our own bindings? → use the official `sherpa-onnx` crate
- [ ] Will Flathub accept prebuilt onnxruntime, or must it be built from source?
- [x] Re-check CPU cost: can Parakeet re-run every ~0.5 s on CPU without noticeable load? → yes on a 7700X: 16 % of real time (`docs/benchmarks.md`); weaker CPUs TBD
- [ ] Parakeet v3 auto language detection during re-check: do consecutive runs flip language on short/ambiguous starts? If so, add a "language: auto / fixed" setting. (en→de→en test: no flips; real short Lithuanian utterances untested)
- [ ] int8 vs. full-precision Parakeet v3: accuracy and speed difference on CPU.
- [ ] Best delay: compare accuracy at 0.5 / 1 / 1.5 s against whole-phrase Parakeet on the same recordings.
- [ ] Is there a streaming model sherpa-onnx can run that already outputs punctuation + casing? → yes: Kroko en 2025 (57 MB, license unclear, D-008) and Nemotron streaming 0.6B (464 MB). Revisit with the license decision.
- [x] **Output mechanism (biggest UX decision):** → portal EIS key events for now (D-005); IME stays Stage 7. portal keystrokes (one-click Flatpak, works everywhere, not seamless) vs. input-method engine (Fcitx5/IBus: seamless and Mac-like, but not a one-click Flatpak and different per desktop). Decide after stage 0 shows how bad collisions and held modifiers are.
