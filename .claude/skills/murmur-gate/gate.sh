#!/usr/bin/env bash
# murmur quality gate: mechanical part. Usage: gate.sh [crate-dir]   (default: repo root)
set -uo pipefail
ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
DIR="$ROOT/${1:-.}"
T=(toolbox run -c murmur-dev bash -lc)
fail=0

step() { echo "== $1"; }

step "cargo fmt --check"
"${T[@]}" "cd '$DIR' && cargo fmt --all -- --check" || fail=1

step "cargo clippy --all-targets -- -D warnings"
"${T[@]}" "cd '$DIR' && cargo clippy --all-targets --quiet -- -D warnings" || fail=1

step "cargo test"
"${T[@]}" "cd '$DIR' && cargo test --quiet 2>&1 | tail -n 25; exit \${PIPESTATUS[0]}" || fail=1

SRC="$DIR/src"
[ -d "$SRC" ] || SRC="$DIR"
step "convention hints (judge each REVIEW line)"
# unwrap/expect: list every occurrence; reviewer checks that it's in tests or commented as unreachable
grep -rn --include='*.rs' -E '\.(unwrap|expect)\(' "$SRC" | grep -v '/tests/' | sed 's/^/REVIEW unwrap: /'
# unsafe outside the ffi module
grep -rln --include='*.rs' -E '\bunsafe\b' "$SRC" | grep -v -E '/ffi(\.rs|/)' | sed 's/^/REVIEW unsafe outside ffi: /'
# info/warn/error logs mentioning text/transcript/words
grep -rn --include='*.rs' -E '\b(info|warn|error)!\(.*\b(text|transcri|word|phrase|sentence)' "$SRC" | sed 's/^/REVIEW log privacy: /'
# hard-coded home paths
grep -rn --include='*.rs' -E '"~/|/home/|\.config/|\.local/share' "$SRC" | sed 's/^/REVIEW hard-coded path: /'
# sleeps used as waits
grep -rn --include='*.rs' -E 'thread::sleep|tokio::time::sleep' "$SRC" | sed 's/^/REVIEW sleep (polling?): /'

if [ $fail -ne 0 ]; then echo "GATE: FAIL"; exit 1; fi
echo "GATE: mechanical checks PASS (now do steps 2-4 of the skill)"
