---
name: murmur-stage
description: Protocol for executing a TODO.md stage of murmur end to end. Plan, implement in small steps, gate and review each, verify on the real desktop (and in the Flatpak where portals are involved), record findings, tick TODO, commit. Use when starting or resuming any stage.
---

# Executing a murmur stage

## 0. Load context
- Read `README.md`, `TODO.md`, `docs/decisions.md` and any `docs/*-findings.md` / `docs/benchmarks.md`.
- Stages run in order (CLAUDE.md). Finish the current stage before starting the next unless the
  maintainer says otherwise.

## 1. Plan the stage
- List the open checkboxes for the stage. Group them into steps of roughly one commit each.
- Find **maintainer decisions** hiding in the steps (UX, wording, defaults, anything that changes
  the spec). Ask them in one batched AskUserQuestion, with a recommended option first, then
  record the answers in `docs/decisions.md` (+ README if the spec changes).
- Find **risky unknowns** and spike them in `spike/` before writing the real code.

## 2. Per step: implement → gate → commit
1. Implement the smallest thing that is runnable end to end.
2. Run the `murmur-gate` skill and loop until it is clean.
3. Tick the TODO checkbox. Add any newly discovered work as new checkboxes.
4. Commit (D-004: local only, never push).

Parallelism: independent research or review can go to background agents. Agents may write
code only in isolated, non-overlapping files. The orchestrator integrates and owns the gate.

## 3. Verify for real
- Unit tests for pure logic: committer, state machine, config, registry, checksum.
- Pipeline tests with recorded WAV files. No mic and no keystrokes needed.
- Live desktop tests follow the safety rules in the `murmur-dev` skill. If a human is needed
  (dialogs, speech, physical keys), batch those tests into one session announced through
  AskUserQuestion.
- Portal features are not done until verified inside the Flatpak (CLAUDE.md).
- KDE is tested on this machine. GNOME needs the maintainer's VM: list exactly what to run and
  what to report, then record it.

## 4. Record
- Results go in `docs/` (`portal-findings.md`, `benchmarks.md`, …), each marked **tested
  (where/version)** or **assumed**.
- Resolve answered "Open questions" in TODO.md, linking the evidence.

## 5. Stage exit
- Every checkbox is ticked, or explicitly deferred with a reason and the maintainer told.
- `README.md` status line matches reality.
- Report to the maintainer: what works, how it was verified, known gaps, and decisions needed
  for the next stage.
