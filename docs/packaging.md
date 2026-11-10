# Flatpak packaging notes

How the Flatpak (`io.github.gabus.Murmur.yml`) builds offline and from source, what was
verified and where, and what is still open before a Flathub submission. Build commands live in
[README → Build & run](../README.md#build--run).

## Layout

| File | What |
|---|---|
| `io.github.gabus.Murmur.yml` | Manifest: freedesktop 26.08 + rust-stable, permissions, the murmur module |
| `flatpak/onnxruntime.yml` | ONNX Runtime 1.28.3, CPU only, shared (`libonnxruntime.so`) |
| `flatpak/sherpa-onnx.yml` | sherpa-onnx 2.0.1 C API, shared (`libsherpa-onnx-c-api.so`), against the ORT module |
| `flatpak/cargo-sources.json` | Vendored crates for `cargo --offline` (generated, committed) |
| `flatpak/update-cargo-sources.sh` | Regenerates it with `flatpak-cargo-generator` at a pinned commit |
| `data/` | Desktop entry, AppStream metainfo, icons (SVG + 128 px PNG rendered from it) |
| `.github/workflows/flatpak.yml` | CI: builds the bundle on push and uploads `murmur.flatpak` |

## How the build stays offline

- **ONNX Runtime.** Its CMake reads dependencies from `cmake/deps.txt` and, for each `https://`
  URL, first looks for the same path under `onnxruntime_CMAKE_DEPS_MIRROR_DIR` (default
  `<source>/mirror`). The module places the 12 archives this configuration needs there
  (`dest: mirror/<url path>`), so FetchContent unpacks local files, still checks the SHA1 from
  `deps.txt`, and applies ORT's own patches. `FETCHCONTENT_TRY_FIND_PACKAGE_MODE=NEVER` stops it
  from picking up SDK copies of abseil, protobuf etc. in other versions. `protoc` is built from
  the protobuf sources (ORT downloads a prebuilt one only when cross-compiling).
  `--compile-no-warning-as-error` overrides ORT's `COMPILE_WARNING_AS_ERROR` (GCC 16 in the 26.08
  SDK adds warnings), so no patch file is needed.
- **sherpa-onnx.** Each `cmake/<dep>.cmake` checks `${CMAKE_SOURCE_DIR}/<archive>` before
  downloading, so the 9 archives sit in the source root (`dest-filename`). kissfft is pinned by
  kaldi-native-fbank and kaldifst by kaldi-decoder; both resolve through the same lookup.
  `SHERPA_ONNXRUNTIME_INCLUDE_DIR`/`SHERPA_ONNXRUNTIME_LIB_DIR` point it at the ORT module.
  TTS, diarization, websocket, portaudio, Python, tests and binaries are off. The C API keeps
  stubs for the TTS/diarization functions, so the Rust crate still links.
- **Rust.** `CARGO_HOME` holds the generated `config` that replaces crates.io with the vendored
  sources. murmur is built with `--no-default-features --features sherpa-shared`, so
  `sherpa-onnx-sys` links `/app/lib` (`SHERPA_ONNX_LIB_DIR`) and never downloads. It adds
  `-rpath /app/lib` (on the default library path in Flatpak anyway).
- `--sandbox` makes flatpak-builder refuse any module that asks for `--share=network`.

### Bumping versions

- **ORT:** change the tarball, then list what the configuration fetches: configure once with
  network in the SDK (`flatpak run --command=bash org.freedesktop.Sdk//26.08`, `cmake -S cmake
  ... -DFETCHCONTENT_TRY_FIND_PACKAGE_MODE=NEVER` with the module's options) and read the
  `Fetch <name> from <url>` lines / `_deps/*-src`. Add one `file` source per entry with its
  sha256, under `mirror/` + the URL path. A missing archive fails the offline build loudly.
- **sherpa-onnx:** the archive names and sha256 are in its `cmake/*.cmake` (and in
  kaldi-native-fbank's / kaldi-decoder's for kissfft and kaldifst). Keep the crate version and
  the C library version equal.
- **Crates:** run `flatpak/update-cargo-sources.sh` after every `Cargo.lock` change.

## Results (2026-11-10, Fedora 44 host, Ryzen 7 7700X, 16 threads)

Tested with `org.flatpak.Builder` 1.4.12, freedesktop SDK 26.08 (GCC 16), rust-stable.

**Build times**, cold (fresh state dir, no ccache, `--sandbox`), measured before the Slint
windows landed:

| Step | Time |
|---|---|
| Download sources (ORT 279 MB and sherpa-onnx tarballs, 21 dependency archives, 311 crates) | 47 s |
| `onnxruntime` (1193 ninja steps) | 7 min 55 s |
| `sherpa-onnx` (255 steps) | 55 s |
| `murmur` (`cargo build --release`) | 42 s |
| Total | 10 min 26 s |

With Slint (652 crates) the `murmur` module takes 75 s. With the module cache, a change to murmur
only rebuilds that module. After a change to the ORT module, `--ccache` brings ORT down to
about 1.5 min.

**Output:** `libonnxruntime.so.1.28.3` (27 MB), `libsherpa-onnx-c-api.so` (3.6 MB),
`/app/bin/murmur` (33 MB with Slint). `murmur` needs `libsherpa-onnx-c-api.so` (no rpath; `/app/lib` is
on the sandbox's library path), which needs `libonnxruntime.so.1` (rpath `$ORIGIN`). `ldd` in
the sandbox resolves both to `/app/lib`. License files for murmur, ORT (+ ThirdPartyNotices) and
sherpa-onnx with each bundled library are in `/app/share/licenses/io.github.gabus.Murmur/`.

**Validation:** `appstreamcli validate` (1.1.3) passes (one pedantic hint: uppercase letters
in the app ID). `desktop-file-validate` (SDK) passes. `flatpak-validate-icon` accepts both
icons. The SVG must start with `<svg`: gdk-pixbuf only sniffs the first bytes, and a long
leading comment made `build-export` reject the icon.

**Smoke test in the sandbox** (installed `--user`, real Flatpak permissions, KDE Plasma 6 session
but run on a private `dbus-daemon` with no service activation and `WAYLAND_DISPLAY`/`DISPLAY`
unset, so no portals, no tray host and nothing can be typed):

- Starts (`murmur ready`). Expected warnings: no tray watcher, no GlobalShortcuts portal, no
  RemoteDesktop portal, and without a display the setup window process exits ("neither
  WAYLAND_DISPLAY nor ... is set"). cpal's PulseAudio host still fails in the sandbox and falls
  back to ALSA (known, TODO Stage 4).
- `Toggle` over D-Bus: mic opens (ALSA), Parakeet v3 loads through the source-built
  sherpa-onnx + ORT 1.28.3 in 1.6 s, status Listening. Second `Toggle`: Idle.
- SIGINT to the sandboxed `murmur` process: `shutting down`, exit code 0. (A signal to the
  `flatpak run` process kills bwrap instead, which isn't a test of murmur.)
- Same run with an invisible nested KWin (`kwin_wayland --virtual`, on the private bus) as the
  sandbox's `WAYLAND_DISPLAY`: the Slint setup window process starts, stays up without errors
  and closes on shutdown. Only warning: no a11y bus on the private bus. Rendering wasn't
  checked visually.

Not tested: aarch64, the CI workflow (written, not run), GNOME.

## Store data: why the remote must be a normal one

Discover and GNOME Software take an app's description, license and branding from the
**remote's AppStream branch**, not from the installed files. A refresh (`flatpak update
--appstream`, which the stores also run) only covers normal remotes. Remotes marked
`no-enumerate` are skipped.

Tested 2026-11-10 with Flatpak 1.18.4, in throwaway `FLATPAK_USER_DIR`s against a local repo
with an AppStream branch (`flatpak build-update-repo`):

| Install from | Remote created | AppStream after install | After `flatpak update --appstream` |
|---|---|---|---|
| `flatpak-builder --install` | `murmur-origin`, no-enumerate | none | none (Discover: no description, "License: unknown") |
| `.flatpakref` without `SuggestRemoteName` | `murmur-origin`, no-enumerate | none | none |
| `.flatpakref` with `SuggestRemoteName=murmur` | `murmur`, normal | none | ✅ description and license |

So the release `.flatpakref` sets `SuggestRemoteName=murmur`, and local builds install from a
normal `murmur-local` remote through `flatpak/install-local.sh`. The composed AppStream data
was fine all along (`build-dir/files/share/app-info/xmls/`).

## Flathub: open points and risks

- **Generative-AI policy (blocking).** Flathub's requirements say manifests must not contain
  AI-generated or AI-assisted content, AI agents must not open or automate submission PRs, and
  AI-generated material must be disclosed. This manifest, the module files and the metainfo were
  written by Claude, so they can't go to Flathub as they are. The maintainer has to author the
  Flathub manifest and PR, using this one only as a tested reference.
- **Source type.** The manifest builds the working tree (`type: dir`). Flathub needs a `git`
  source with `tag` + `commit` (or a release archive), so this needs a tagged release.
- **Repository must be public.** `flatpak-builder-lint` fails with `appid-url-not-reachable`
  because `https://github.com/gabus/murmur` returns 404 (the `io.github.gabus` ID is verified
  through it). The metainfo URLs fail the same way.
- **Screenshots.** `metainfo-missing-screenshots` is a linter error. Stage 6.
- **Wayland only.** `finish-args-only-wayland` is a linter warning. murmur is Wayland-only by
  design (README); reviewers may ask why there is no `fallback-x11`.
- **aarch64.** Flathub builds x86_64 and aarch64; only x86_64 was built here. KleidiAI (ORT's ARM
  kernels) is off, ORT's default for plain CMake builds.
- **Release entry.** The metainfo has a `development` release stub for 0.1.0; it needs a real
  release entry and date when tagging.
- **Build time on Flathub builders** is dominated by ORT; acceptable for Flathub, but every
  ORT bump means re-checking the mirror list (see *Bumping versions*).
- **Dependency drift.** `cargo-sources.json` must be regenerated after every `Cargo.lock`
  change (Slint alone added 341 crates), or the offline build fails.
