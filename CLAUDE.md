# CLAUDE.md

Instructions for working on **murmur**. Each fact lives in exactly one file:

| File | Owns |
|---|---|
| `README.md` | The spec: goals, UX, requirements, models, supported desktops, toolset, architecture, permissions, dev commands |
| `TODO.md` | Stages, task checklist, open questions |
| `CLAUDE.md` | How to work: process, code conventions, environment |
| `docs/decisions.md` | Decision log: maintainer decisions (UX, business, licensing) with date and reasoning. Binding like the spec |

Don't copy content between them. Link to the owning file instead. When something changes, update it in its owner only.

## The spec is binding

- Read `README.md` and `TODO.md` before starting work.
- Don't deviate from the README spec without asking. If code and README disagree, ask which one is right.
- When a decision changes the spec, update `README.md` in the same change.
- Questions only the maintainer can answer (UX, business, licensing) go to the maintainer. Record each
  answer in `docs/decisions.md` and follow the entries there as you would the spec.

## Working style

- **Follow the stages in `TODO.md` in order.** Don't build stage N+1 features while stage N is incomplete unless asked.
  Tick checkboxes as items are done, and add new items you discover.
- **Mark uncertainty.** Portal behaviour differs between KDE, GNOME and versions.
  Say whether something was tested, and where, or is assumed. Record test results in `docs/` (e.g. `docs/portal-findings.md`).
- **Prefer small, reversible experiments** (spikes in `spike/`) over large speculative implementations.
- **Keep scope to the spec.** Propose additions before building them.
- **Verify in the Flatpak.** `cargo run` in the toolbox is fine for iteration, but portal behaviour must be verified
  inside the Flatpak before a feature is considered done.
- **Never install packages on the host.** Build dependencies go into the toolbox (see README → Development).
  If something seems to need a host package, ask first.

## Rust conventions

- Stable Rust, edition 2024. `cargo fmt` and `cargo clippy --all-targets -- -D warnings` must pass.
- Async with `tokio`. Run blocking or CPU-heavy work (inference, audio callbacks) off the async runtime:
  use `spawn_blocking` or a dedicated thread with channels. No polling loops: wait on events.
- Errors: `thiserror` in library-style modules, `anyhow` at the binary edge. No `unwrap()`/`expect()` outside tests
  and truly unreachable cases (comment why).
- Logging with `tracing`. Never log transcribed text at `info` or above (privacy). Use `debug`/`trace` only.
- Keep FFI (`unsafe`) isolated in one module with safe wrappers and `// SAFETY:` comments.
- Paths come from XDG env vars (Flatpak redirects them into `~/.var/app/<app-id>/`). Never hard-code `~/.config` etc.
- Add dependencies sparingly. Each one must build offline inside flatpak-builder (via `flatpak-cargo-generator`).

## Environment

- Maintainer's machine: Fedora 44, KDE Plasma 6 (Wayland), RTX 3090, Ryzen 7 7700X. KDE is the primary test target.
- GNOME testing happens in a VM.

## Git

- Small, focused commits with imperative subject lines (`Add GlobalShortcuts spike`).
- Don't commit build output (`target/`, `build-dir/`, `.flatpak-builder/`) or model files.
