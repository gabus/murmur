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
