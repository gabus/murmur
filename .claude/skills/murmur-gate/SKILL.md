---
name: murmur-gate
description: Quality gate for murmur. Runs fmt, clippy -D warnings, tests and convention checks, then an independent code review, looping until clean. Run before every commit and at the end of every TODO item.
---

# murmur quality gate

A change is committable only after every step below passes. Loop: fix → re-run from step 1.

## 1. Mechanical checks

```sh
bash .claude/skills/murmur-gate/gate.sh          # whole workspace
bash .claude/skills/murmur-gate/gate.sh spike    # the spike crate
```

The script runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and
`cargo test` in the toolbox, then greps for convention violations (see the script). Its
`REVIEW:` lines are hints, not failures. Read each one and judge it.

## 2. Convention review (by hand, against CLAUDE.md → Rust conventions)

- No `unwrap()`/`expect()` outside tests, except truly unreachable cases with a comment
  explaining why.
- `unsafe` only in the single FFI module, each block with a `// SAFETY:` comment.
- Transcribed text is logged only at `debug`/`trace`, never `info`/`warn`/`error`. This
  includes `?`/`Display` of structs that carry text.
- Paths come only from XDG env vars or `dirs`-style helpers that read them. No hard-coded `~/.config`.
- Blocking or CPU-heavy work (inference, audio callbacks, file hashing) stays off the tokio
  runtime. No polling loops or `sleep` waits for events.
- Errors use `thiserror` in modules and `anyhow` only in `main.rs` / the binary edge.
- New dependencies: justified, and can be vendored offline for flatpak-builder.

## 3. Spec conformance

Diff against `README.md` and `docs/decisions.md`. Anything that deviates needs a maintainer
decision first (AskUserQuestion), recorded in `docs/decisions.md`, with README updated in the
same change.

## 4. Independent review

Run the built-in `code-review` skill (level `high`) on the uncommitted diff. If the change is
large, also spawn a fresh `general-purpose` agent with only the file list and the spec sections,
not your own conclusions. Its job is to find correctness bugs: races, lost audio, dropped
words, a mic left open, portal session leaks, and panics on the audio thread.

Fix every confirmed finding and re-run the gate. Record anything you decline to fix and the
reason in the commit message body.

## 5. Commit (D-004)

Small, imperative subject (`Add RemoteDesktop typing spike`), committed locally on `main`.
Never push. End the message with the Co-Authored-By line from the session's attribution
reminder.
