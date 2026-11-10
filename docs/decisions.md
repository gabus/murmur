# Decision log

Decisions taken with the maintainer, with date and reasoning. They are binding like the spec.
The resulting behaviour lives in [README.md](../README.md) (the spec). This file keeps only the
*why* and *when*. To revisit a decision, ask the maintainer and add a new entry that supersedes
the old one. Don't edit old entries.

Format: `D-NNN · date · who decided` → decision → why → where the spec reflects it.

---

### D-001 · 2026-11-09 · maintainer
**GUI toolkit for the first-run setup window and Settings: Slint, software renderer.**
- Why: Rust-native and declarative. The software renderer means no GPU or `--device=dri`
  permission. Runs on the freedesktop 26.08 runtime with no extra runtime. Looks polished
  (Fluent style).
- Licensing: Slint is GPL-3.0-only / Royalty-free-2.0 (attribution) / commercial. Which terms
  apply depends on murmur's license (D-003).
- Spec: README → Toolset.

### D-002 · 2026-11-09 · maintainer
**Spacing: every typed word is followed by one space** (`"Hello "`, `"world. "`).
- Why: murmur can't read the text around the cursor. A trailing space means the cursor always
  sits after a space, so the user can keep talking or start typing. The only cost is one
  trailing space at the end of a session.
- Spec: README → Functional requirements → Output.

### D-003 · 2026-11-09 · maintainer
**License: decided later (Stage 5).** Until then, avoid dependencies whose license would
constrain the choice. Slint's royalty-free terms (attribution) work with any license.

### D-004 · 2026-11-09 · maintainer
**Git: Claude commits locally on `main` and never pushes.** Small, reviewed commits, made once a
step passes the quality gate. The maintainer pushes.

### D-005 · 2026-11-10 · Claude (from Stage 0 evidence; maintainer informed)
**Typing goes through RemoteDesktop → `ConnectToEIS` (libei) with key events resolved through
the compositor's real XKB keymap. murmur presses Shift/AltGr itself and does not use
`NotifyKeyboardKeysym`.**
- Why: on KDE the keysym path corrupts text (inverted case, dropped non-ASCII) and leaves
  Shift stuck for the user (`docs/portal-findings.md`). EIS typed mixed case and punctuation
  exactly. It also reports the active layout and the physically held modifiers. The
  maintainer asked for input that works properly, in the style of dictee, rather than
  workarounds. dictee uses dotool/uinput, which the README rules out for the Flatpak, and EIS
  is the sandboxed equivalent.
- Consequences: words are held back while the user physically holds Ctrl/Alt/Super. Only
  characters missing from the active layout use the clipboard fallback. Minimum desktops are
  Plasma 6.1+ and GNOME 46+ (EIS in RemoteDesktop).
- Spec: README → Functional requirements → Output; Supported desktops; Known limitations.

### D-006 · 2026-11-10 · Claude (from nested-KWin tests; maintainer informed)
**Typing waits while the user physically holds *any* modifier: Shift, Ctrl, Alt, Super or
AltGr.**
- Why: tests showed that a user holding Shift (e.g. typing a capital) while murmur types turns
  "hello" into "HELLO". The spec's principle is that typing and talking mix freely and nothing
  breaks if you type yourself. Words are held back and typed once the keys are released, so
  nothing is lost.
- Also: murmur taps the `<LVL3>` key (ISO_Level3_Shift, never Alt) once per EIS device, because
  KWin reports the active layout and held modifiers only after a change.
- Spec: README → Functional requirements → Output; Known limitations → Held modifiers.

### D-007 · 2026-11-10 · Claude (from output review; maintainer informed). Supersedes D-006
**Typing waits whenever the keyboard state could change what murmur types:** any held modifier
(except Caps Lock), any latched modifier, and any locked modifier except Caps Lock and locks
proven harmless for the current layout (e.g. Num Lock on common layouts, but not Neo's level
lock). Caps Lock is planned for, not waited on.
- Why: a review replayed every typeable character through xkb. Sticky-keys locks, latched
  Shift and Neo's level-5/level-lock turned dictated letters into shortcuts or navigation keys
  (Return, BackSpace). Waiting is the only safe choice, and nothing is lost.
- While waiting for more than about 2 s, the tray shows *Waiting* with a hint to release the
  keys.
- If typing is denied or unavailable, murmur stops retrying until the next dictation starts
  (no dialog per word).
- Spec: README → Functional requirements → Output; Known limitations.

### D-008 · 2026-11-10 · Claude (model evaluation; maintainer informed)
**Model catalogue:** Parakeet v3 (default), Moonshine base and tiny v2 (English, lighter), and a
"Light" streaming engine (Zipformer en 2023-06-26 plus the English punctuation/casing model).
- Why: measured in `docs/benchmarks.md`. The 20M Zipformer drops utterance starts. Kroko
  (2025) streams with native punctuation and is better, but its license is unclear
  (CC-BY-SA community tier, placeholder metadata). Kept as an open question until licensing
  (D-003).
- Spec: README → Supported models.

### D-009 · 2026-11-10 · maintainer
**First-run setup is a single page**: Shortcut (with Change), Typing permission (Allow…),
Speech model (download progress, Choose another model), and a "Start murmur when I log in"
checkbox, then Done.
- Why: the model download runs while the user handles the two system dialogs, so setup
  finishes sooner.
- Spec: README → First run.

### D-010 · 2026-11-10 · maintainer
**Tray → Settings… opens a small window**: Shortcut (Change…), Microphone, Model, the pause
that ends a sentence, free memory after N idle minutes, and start at login. Same look as the
setup window.
- Spec: README → First run / Functional requirements.

### D-011 · 2026-11-10 · maintainer
**Characters missing from the active layout are pasted, then the previous clipboard is
restored.**
- Why: it works for any character. The trade-off the maintainer accepted is that a clipboard
  manager may record the word.
- Spec: README → Output.

### D-012 · 2026-11-10 · Claude (implementation of D-011; maintainer informed)
**Pasting covers a whole word, and the paste shortcut is the one place murmur presses Ctrl.**
- Why: a paste is read asynchronously by the app, so keys typed right after it could overtake
  it. One paste per word also means one clipboard round trip.
- The clipboard is saved first, and if it can't be saved and restored safely (too large,
  unreadable), murmur doesn't paste: the word is typed without the missing characters, plus
  an `Unmapped` notice.
- Shortcut: Ctrl+V, or Ctrl+Shift+V via `paste_keys`. Ctrl is pressed only after the
  held-modifier check passes, and released in the same burst.
- Verified against KDE's real Clipboard portal in a nested KWin; GNOME not yet. See
  `docs/portal-findings.md`.

### D-013 · 2026-11-10 · maintainer. Resolves D-003
**License: GPL-3.0-or-later.**
- Why: the maintainer's choice. Copyleft keeps forks open. Slint is then used under its
  GPLv3 terms, so no attribution badge is needed.
- Spec: README → License; `LICENSE`; Cargo `license`; AppStream `project_license`.

### D-014 · 2026-11-10 · Claude (Stage 5; maintainer informed)
**The Flatpak builds ONNX Runtime (1.28.3, CPU) and the sherpa-onnx 2.0.1 C API from source as
shared libraries, fully offline. Dev builds keep sherpa-onnx's prebuilt static libs.**
- Why: Flathub requires everything to be built from source, and its builders have no network.
  The `sherpa-onnx` crate's `shared` feature with `SHERPA_ONNX_LIB_DIR` links an installed
  library and never downloads, so the Flatpak needs no patches to the crate. ORT 1.28.3 rather
  than 1.28.2 (what sherpa-onnx 2.0.1 ships with): a patch release with memory-safety fixes and
  the same ABI, which matters because models are downloaded files.
- Cost: the first build compiles ORT (8 min of a 10.5 min cold build on 16 threads);
  flatpak-builder caches it.
- Spec: README → Toolset, Build & run; `docs/packaging.md`.

### D-015 · 2026-11-10 · maintainer
**v0.1 is self-hosted: a GitHub Release with the `.flatpak` bundle, plus a signed Flatpak repo
on GitHub Pages with a `.flatpakref` for one-click install. The README discloses that the code
was written with AI assistance.**
- Why: Flathub's Generative AI policy (docs.flathub.org → Requirements → Generative AI policy,
  checked 2026-11-10). Manifests must contain no AI-generated content, AI-generated code must
  be disclosed and may be rejected, and AI agents must not open submission PRs. Flathub
  remains possible later if the maintainer writes that manifest and submits it personally.
- Spec: README → Goals (one-click install), Install.

### D-016 · 2026-11-10 · maintainer
**Suggested shortcut: Super+H, except Super+Alt+H on GNOME.**
- Why: Super+H matches Windows' dictation key (Win+H) and is free on KDE. On stock GNOME 50,
  Super+H is "Hide window": GNOME's shortcut dialog accepts it, but it never fires (verified
  headless, gnome-shell 50.6 + xdg-desktop-portal-gnome 50.0). Super+Alt+H is free on both.
- GNOME's GlobalShortcuts portal is v1 (no ConfigureShortcuts), so there the shortcut can
  only be changed in GNOME Settings.
- Spec: README → First run; Supported desktops.

### D-017 · 2026-11-10 · maintainer
**The product name is "Murmur" (capital M)** in everything users see: window titles, the setup
and settings text, the tray, notifications, the desktop entry, the metainfo, the landing page
and the README title.
- The command (`murmur`), app ID (`io.github.gabus.Murmur`), paths and logs are unchanged.
- The tray and app icons are being replaced by a distinct brand mark (the current tray icon
  looks like a microphone-settings applet). Options are in `docs/icon-options/`; the
  maintainer chose **option 2, "Wave M"** (the letter M drawn as a sound wave, teal → blue).
  The app icon is `data/icons/io.github.gabus.Murmur.svg`. The tray uses the exported
  symbolic icon `io.github.gabus.Murmur-symbolic` when idle (recoloured by the panel); the
  pixmaps drawn in `src/icons.rs` cover listening (red, and the wave grows with the voice
  level), loading and error.

### D-018 · 2026-11-10 · maintainer
**The store description (AppStream metainfo) lists "What a distro must have" and what is "Not
supported yet"**, so people can see before installing whether Murmur works on their system.
- Requirements: Wayland, Plasma 6.1+ or GNOME 48+, xdg-desktop-portal with the KDE/GNOME
  backend, PipeWire or PulseAudio, x86_64, and on GNOME the AppIndicator extension for the
  tray. Not supported yet: X11, Sway/wlroots, Hyprland (untested), ARM.
- Only real constraints are listed. RAM and network access are left out, because every machine
  has them.
- When a desktop or architecture changes status in README → Supported desktops, update
  `data/io.github.gabus.Murmur.metainfo.xml` in the same change.
