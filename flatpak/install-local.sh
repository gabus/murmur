#!/bin/sh
# Build the Flatpak offline into ./repo and install it from a local remote, "murmur-local".
#
# Why not `flatpak-builder --install`: that installs from a hidden one-app remote
# ("murmur-origin", no-enumerate) which never gets AppStream data, so Discover and GNOME
# Software show no description and "License: unknown". A normal remote gets its AppStream
# data refreshed like any other source, like the release .flatpakref (docs/packaging.md).
set -eu
cd "$(dirname "$0")/.."

APP=io.github.gabus.Murmur
flatpak run org.flatpak.Builder --user --force-clean --sandbox \
  --state-dir=.flatpak-builder --repo=repo build-dir "$APP.yml"
flatpak build-update-repo repo
flatpak remote-add --user --if-not-exists --no-gpg-verify \
  --title="Murmur (local build)" murmur-local "file://$PWD/repo"

if [ "$(flatpak info --user --show-origin "$APP" 2>/dev/null)" = murmur-local ]; then
  flatpak update --user -y --noninteractive "$APP//master"
else
  # First run, or switching from a `flatpak-builder --install` build: --reinstall changes
  # the origin and keeps the app's data (~/.var/app).
  flatpak install --user -y --noninteractive --reinstall murmur-local "$APP//master"
fi
flatpak update --user -y --noninteractive --appstream murmur-local
