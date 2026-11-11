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

# A running Murmur keeps the old build until it restarts: restart it on the new one (graceful
# SIGINT to the app inside each sandbox, then a fresh start), and check the running build.
running() { flatpak ps --columns=application | grep -qx "$APP"; }
if running; then
  for bwrap in $(flatpak ps --columns=child-pid,application | awk -v app="$APP" '$2 == app {print $1}'); do
    for pid in $(ps -o pid= --ppid "$bwrap"); do kill -INT "$pid"; done
  done
  for _ in $(seq 1 100); do running || break; sleep 0.1; done
  if running; then echo "Murmur didn't stop; restart it by hand" >&2; exit 1; fi
  setsid -f flatpak run "$APP" >/dev/null 2>&1
  for _ in $(seq 1 50); do running && break; sleep 0.2; done
  sleep 2
  want=$(flatpak info --user --show-commit "$APP")
  for inst in $(flatpak ps --columns=instance,application | awk -v app="$APP" '$2 == app {print $1}'); do
    got=$(sed -n 's/^app-commit=//p' "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/.flatpak/$inst/info")
    if [ "$got" != "$want" ]; then echo "running Murmur is $got, installed is $want" >&2; exit 1; fi
  done
  echo "Murmur restarted on ${want%"${want#????????????}"}"
fi
