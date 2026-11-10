#!/bin/bash
# Regenerate flatpak/cargo-sources.json from Cargo.lock (run after every Cargo.lock change).
# Uses flatpak-cargo-generator from flatpak-builder-tools at a pinned commit. Needs python3 with
# aiohttp and tomlkit (both in the murmur-dev toolbox, see README → Development):
#   toolbox run -c murmur-dev bash flatpak/update-cargo-sources.sh
set -euo pipefail

COMMIT=41c20aa10819cdb2a4f3ca171758a96d1955c018
SHA256=0a2db6be87d75910facef28ab46d4d6460802e8419ab850d0caa6a364d26b380
URL="https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/$COMMIT/cargo/flatpak-cargo-generator.py"

root="$(cd "$(dirname "$0")/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

curl -sSfL -o "$tmp/flatpak-cargo-generator.py" "$URL"
echo "$SHA256  $tmp/flatpak-cargo-generator.py" | sha256sum -c --quiet
python3 -I "$tmp/flatpak-cargo-generator.py" "$root/Cargo.lock" -o "$root/flatpak/cargo-sources.json"
