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
- [ ] Test on KDE Plasma 6 (primary ✅) and GNOME 48+ (headless mutter in the toolbox, then VM for the portal dialog)
- [x] Spike EIS typing (ConnectToEIS + reis + keymap-aware keycodes): works on KDE; KWin needs a `sync` poke
- [ ] Investigate cpal PulseAudio host failing inside Flatpak (ALSA fallback works)
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
- [ ] Output: type each committed word through the RemoteDesktop portal, with spacing
- [x] Measure: lag from spoken word to typed word, CPU while speaking, Parakeet run time per buffer length (`docs/benchmarks.md`)
- [ ] End-to-end check of the daemon (hotkey → mic → typed text) in a nested KWin, then in the Flatpak
- [x] State machine: `Idle → Listening(+Loading) → Idle`, toggled by the shortcut only, never by silence
- [x] Ring-buffer audio from key press so speech during model load isn't lost (worker channel buffers everything from Begin)
- [x] On stop: flush the phrase in progress (transcribe + insert), then close the mic (250 ms tail)
- [x] Hard-coded model path for now

## Stage 2: Tray

- [ ] `ksni` tray icon with states: idle, loading, listening, error
- [ ] Icons (symbolic SVG, light/dark friendly)
- [ ] Menu: Start/Stop, Model ▸, Settings, Quit
- [ ] Listening icon reacts to mic level (a few pre-rendered levels; update at most ~10×/s)
- [ ] Notification on errors (mic missing, portal denied, model missing)

## Stage 3: Models

- [ ] Model registry (name, mode, URL, size, sha256, language)
- [ ] Download on demand with progress, then verify checksum
- [ ] Store under `$XDG_DATA_HOME/murmur/models`
- [ ] Model picker in tray menu, with delete option
- [ ] Light engine: streaming Zipformer transducer, greedy decoding, ~1 s commit delay, punctuation/casing step
- [ ] Verify light engine output is append-only (log any case where earlier tokens change)
- [ ] Moonshine as a lighter re-check model
- [ ] Lazy load on first activation; unload after idle timeout
- [ ] Measure: load time, RAM, latency to text, CPU while listening (record in `docs/benchmarks.md`)

## Stage 4: Polish

- [ ] Config file `$XDG_CONFIG_HOME/murmur/config.toml` (model, silence timeout, idle unload, spacing, audio source)
- [ ] First-run setup window: shortcut → typing permission → model download (one time only)
- [ ] Left-click tray icon toggles listening; right-click opens menu
- [ ] Optional audio source picker
- [ ] Clipboard + paste fallback for characters that keysym typing can't produce
- [ ] Autostart via Background portal (opt-in)
- [ ] Single-instance guard
- [ ] Graceful handling of portal denial and session loss
- [ ] Basic punctuation/capitalization handling (model-dependent)

## Stage 5: Packaging

- [ ] Flatpak manifest `io.github.gabus.Murmur.yml` (Platform/Sdk 26.08, rust-stable)
- [ ] `flatpak-cargo-generator` for offline cargo sources
- [ ] Decide how onnxruntime / sherpa-onnx are built (from source likely required for Flathub)
- [ ] AppStream metainfo, `.desktop` file, icons (scalable + 128px)
- [ ] Minimal permissions (see README); no `--device=all`, no `--filesystem=host`
- [ ] CI: GitHub Actions building the Flatpak bundle on push
- [ ] License chosen and added

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
- [ ] Parakeet v3 auto language detection during re-check: do consecutive runs flip language on short/ambiguous starts? If so, add a "language: auto / fixed" setting.
- [ ] int8 vs. full-precision Parakeet v3: accuracy and speed difference on CPU.
- [ ] Best delay: compare accuracy at 0.5 / 1 / 1.5 s against whole-phrase Parakeet on the same recordings.
- [ ] Is there a streaming model sherpa-onnx can run that already outputs punctuation + casing (e.g. NVIDIA cache-aware streaming FastConformer)?
- [x] **Output mechanism (biggest UX decision):** → portal EIS key events for now (D-005); IME stays Stage 7. portal keystrokes (one-click Flatpak, works everywhere, not seamless) vs. input-method engine (Fcitx5/IBus: seamless and Mac-like, but not a one-click Flatpak and different per desktop). Decide after stage 0 shows how bad collisions and held modifiers are.
