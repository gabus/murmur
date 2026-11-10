#!/bin/bash
# Headless GNOME (mutter / gnome-shell) harness for EIS keyboard injection tests.
#
# Runs entirely on a private D-Bus session bus, a private XDG_RUNTIME_DIR, private HOME/XDG dirs
# and GSETTINGS_BACKEND=memory, so nothing reaches the user's real desktop, bus, dconf or audio.
# Typing goes through mutter's own org.gnome.Mutter.RemoteDesktop API (no portal, no dialog).
#
# Run inside the murmur-dev toolbox, from the repo root, after `cd spike && cargo build`:
#
#   toolbox run -c murmur-dev spike/harness/gnome-headless.sh test           # mutter only
#   toolbox run -c murmur-dev env COMPOSITOR=shell spike/harness/gnome-headless.sh test
#   toolbox run -c murmur-dev env COMPOSITOR=shell LAYOUTS=us,lt ACTIVE=lt \
#       spike/harness/gnome-headless.sh layouts                             # gnome-shell only
#   toolbox run -c murmur-dev spike/harness/gnome-headless.sh poke [runs]   # sync-poke experiment
#   toolbox run -c murmur-dev spike/harness/gnome-headless.sh type type ctrl  # several, one session
#   toolbox run -c murmur-dev spike/harness/gnome-headless.sh shell -- <cmd...>  # ad-hoc command
#
# Tests: probe (seat caps), type (exact sentence), ctrl (other client holds Control_L), text
# (ei_text, INFO if not offered), layouts (needs LAYOUTS/ACTIVE, gnome-shell), test = probe type
# ctrl text. poke = device readiness with sync poke / re-poll only / neither (the no-poke stalls
# are a reis 0.7.1 lost wakeup, not the compositor; each stalled run costs 10 s).
#
# Env: COMPOSITOR=mutter|shell (default mutter), SPIKE=<path to spike binary>,
#      LAYOUTS=us,lt ACTIVE=lt  input sources via a private compiled gsettings schema override,
#      WARMUP=key|connect|window|none  (gnome-shell only, default key: see start_compositor),
#      SPIKE_EXTRA="<args>" extra `spike gnome` args, e.g. --no-poke,
#      KEEP=1 keeps the work dir (logs: compositor.log, pad-*.log with WAYLAND_DEBUG, spike-*.log).
# Exit status: 0 if every check passed.
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
SPIKE=${SPIKE:-$HERE/../target/debug/spike}
COMPOSITOR=${COMPOSITOR:-mutter}
SENTENCE='The Quick Brown Fox? Hello, World! (ok) "quoted" 123 '

die() { echo "harness: $*" >&2; exit 2; }

# ---------------------------------------------------------------- outer stage
if [ "${1:-}" != "__inner" ]; then
    [ -x "$SPIKE" ] || die "spike binary not found at $SPIKE (cd spike && cargo build)"
    command -v dbus-run-session >/dev/null || die "dbus-run-session missing (dnf install dbus-daemon)"
    WORK=$(mktemp -d "${TMPDIR:-/tmp}/murmur-gnome-work.XXXXXX")
    # Unix socket paths are limited to 108 bytes, so the runtime dir gets its own short path.
    RT=$(mktemp -d /tmp/murmur-rt.XXXXXX)
    chmod 700 "$RT"
    mkdir -p "$WORK"/{home,config,data,cache,state,dbus-services}
    # Private session bus with NO service activation (no portals, dconf, a11y bus, tracker...).
    cat >"$WORK/bus.conf" <<EOF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <keep_umask/>
  <listen>unix:tmpdir=/tmp</listen>
  <auth>EXTERNAL</auth>
  <servicedir>$WORK/dbus-services</servicedir>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
EOF
    export MURMUR_OUTER_BUS=${DBUS_SESSION_BUS_ADDRESS:-none}
    env -u DISPLAY -u WAYLAND_DISPLAY -u DBUS_SESSION_BUS_ADDRESS \
        HOME="$WORK/home" XDG_CONFIG_HOME="$WORK/config" XDG_DATA_HOME="$WORK/data" \
        XDG_CACHE_HOME="$WORK/cache" XDG_STATE_HOME="$WORK/state" XDG_RUNTIME_DIR="$RT" \
        GSETTINGS_BACKEND=memory PULSE_SERVER=unix:/nonexistent PIPEWIRE_REMOTE=murmur-none \
        GTK_USE_PORTAL=0 MURMUR_WORK="$WORK" SPIKE="$SPIKE" COMPOSITOR="$COMPOSITOR" \
        dbus-run-session --config-file="$WORK/bus.conf" -- "$0" __inner "$@"
    rc=$?
    rm -rf "$RT"
    if [ "${KEEP:-0}" = 1 ]; then echo "harness: work dir kept: $WORK"; else rm -rf "$WORK"; fi
    exit $rc
fi

# ---------------------------------------------------------------- inner stage (private bus)
shift
WORK=$MURMUR_WORK
SOCK=murmur-gnome-$$
PIDS=()
FOCUS=1
cleanup() { for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done; wait 2>/dev/null; }
trap cleanup EXIT

assert_isolated() {
    [ -z "${DISPLAY:-}" ] || die "SAFETY: DISPLAY is set"
    [ "${WAYLAND_DISPLAY:-}" = "$SOCK" ] || die "SAFETY: WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-} is not $SOCK"
    [ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ] || die "SAFETY: no private bus"
    [ "$DBUS_SESSION_BUS_ADDRESS" != "$MURMUR_OUTER_BUS" ] || die "SAFETY: bus is the outer bus"
    case "$XDG_RUNTIME_DIR" in /run/user/*) die "SAFETY: XDG_RUNTIME_DIR is the real one";; esac
}

# Layout config for gnome-shell: compile a private schema dir with an input-sources override.
# LAYOUTS=us,lt ACTIVE=lt  ->  sources [('xkb','us'),('xkb','lt')], mru-sources starts with lt.
setup_layouts() {
    [ -n "${LAYOUTS:-}" ] || return 0
    local dir=$WORK/schemas src="" mru="" l
    mkdir -p "$dir"
    cp /usr/share/glib-2.0/schemas/*.xml /usr/share/glib-2.0/schemas/*.override "$dir"/ 2>/dev/null
    IFS=, read -ra ls <<<"$LAYOUTS"
    for l in "${ls[@]}"; do src+="('xkb', '$l'), "; done
    mru="('xkb', '${ACTIVE:-${ls[0]}}'), "
    for l in "${ls[@]}"; do [ "$l" = "${ACTIVE:-${ls[0]}}" ] || mru+="('xkb', '$l'), "; done
    cat >"$dir/zz-murmur-test.gschema.override" <<EOF
[org.gnome.desktop.input-sources]
sources=[${src%, }]
mru-sources=[${mru%, }]
EOF
    glib-compile-schemas "$dir" || die "glib-compile-schemas failed"
    export GSETTINGS_SCHEMA_DIR=$dir
    echo "harness: input-sources sources=[${src%, }] mru-sources=[${mru%, }]"
}

start_compositor() {
    local args=(--headless --wayland --no-x11 --virtual-monitor 1280x800 --wayland-display "$SOCK")
    case "$COMPOSITOR" in
        mutter) mutter "${args[@]}" >"$WORK/compositor.log" 2>&1 & ;;
        shell) gnome-shell "${args[@]}" >"$WORK/compositor.log" 2>&1 & ;;
        *) die "COMPOSITOR must be mutter or shell" ;;
    esac
    PIDS+=($!)
    local i
    for i in $(seq 100); do
        [ -S "$XDG_RUNTIME_DIR/$SOCK" ] && break
        sleep 0.1
    done
    [ -S "$XDG_RUNTIME_DIR/$SOCK" ] || { tail -20 "$WORK/compositor.log"; die "compositor socket never appeared"; }
    # The RemoteDesktop API appears with the compositor; wait for the bus name.
    for i in $(seq 100); do
        busctl --user status org.gnome.Mutter.RemoteDesktop >/dev/null 2>&1 && break
        sleep 0.1
    done
    if [ "$COMPOSITOR" = shell ]; then
        # The startup animation ends in the overview, which takes keyboard focus (typed text
        # lands in its search entry). Wait for startup to finish, then close the overview.
        for i in $(seq 150); do
            grep -q 'GNOME Shell started' "$WORK/compositor.log" && break
            sleep 0.1
        done
        grep -q 'GNOME Shell started' "$WORK/compositor.log" || die "gnome-shell never finished starting"
        sleep 1
        # Close it like a user would; a window mapped while the overview is up never gets focus.
        busctl --user set-property org.gnome.Shell /org/gnome/Shell org.gnome.Shell OverviewActive b false ||
            die "could not close the overview"
        sleep 1
        echo "harness: overview active: $(busctl --user get-property org.gnome.Shell /org/gnome/Shell org.gnome.Shell OverviewActive)"
    fi
    export WAYLAND_DISPLAY=$SOCK
    if [ "$COMPOSITOR" = shell ]; then
        # A window mapped before the shell has seen any key event is not reliably activated
        # (wl_keyboard.enter comes seconds later or mid-typing). One EIS key press fixes it
        # (WARMUP=key: a space typed with no window focused). connect / window don't help.
        case "${WARMUP:-key}" in
            window) start_pad warmup; sleep 1; stop_pad; PAD_PID= ;;
            connect) assert_isolated; "$SPIKE" gnome --settle-ms 200 >"$WORK/spike-warmup.log" 2>&1 ;;
            key) assert_isolated; "$SPIKE" gnome --settle-ms 200 ' ' >"$WORK/spike-warmup.log" 2>&1 ;;
            none) ;;
            *) die "WARMUP must be window|connect|key|none" ;;
        esac
        echo "harness: warm-up ${WARMUP:-key} done"
    fi
    local ver; ver=$(mutter --version | head -1)
    [ "$COMPOSITOR" = shell ] && ver="$(gnome-shell --version), $ver"
    echo "harness: $COMPOSITOR ($ver) on $SOCK, bus $DBUS_SESSION_BUS_ADDRESS"
}

PAD_PID=
start_pad() { # $1 = name
    local log=$WORK/pad-$1.log i
    PAD_FILE=$WORK/pad-$1.txt
    PAD_LOG=$log
    WAYLAND_DEBUG=1 "$SPIKE" testpad "$PAD_FILE" >"$log" 2>&1 &
    PAD_PID=$!
    PIDS+=($PAD_PID)
    # Ready = the toplevel exists and a frame callback fired (the window is mapped and drawn).
    # Keyboard focus can't be checked yet: the headless seat has no keyboard until an EIS
    # client connects (wl_seat.capabilities(0)), so spike_gnome syncs on wl_keyboard.enter.
    for i in $(seq 100); do
        grep -qE 'xdg_toplevel#[0-9]+\.configure\(' "$log" 2>/dev/null &&
            grep -qE 'wl_callback#[0-9]+\.done\([1-9]' "$log" 2>/dev/null && break
        sleep 0.1
    done
    grep -qE 'wl_callback#[0-9]+\.done\([1-9]' "$log" || die "testpad window never mapped (see $log)"
}
stop_pad() { kill "$PAD_PID" 2>/dev/null; wait "$PAD_PID" 2>/dev/null; }

FAILS=0
check() { # $1 = name, $2 = condition result (0 ok), $3 = detail
    if [ "$2" = 0 ]; then echo "PASS $1 $3"; else echo "FAIL $1 $3"; FAILS=$((FAILS + 1)); fi
}
pad_focused() { grep -qE 'wl_keyboard#[0-9]+\.enter\([0-9]+, wl_surface' "$PAD_LOG"; }

# Run `spike gnome`. With a pad running, the spike connects, then waits on a go-file that is
# created once the pad has keyboard focus (wl_keyboard.enter), so no keys are sent unfocused.
spike_gnome() { # $1 = log name, rest = args
    local name=$1 go spid i rc
    shift
    assert_isolated
    go=$WORK/go-$name
    if [ -n "$PAD_PID" ] && kill -0 "$PAD_PID" 2>/dev/null; then
        # shellcheck disable=SC2086  # SPIKE_EXTRA is a word list on purpose
        "$SPIKE" gnome ${SPIKE_EXTRA:-} --go-file "$go" "$@" >"$WORK/spike-$name.log" 2>&1 &
        spid=$!
        for i in $(seq 100); do pad_focused && break; sleep 0.05; done
        if pad_focused; then FOCUS=0; else FOCUS=1; fi
        touch "$go"
        wait "$spid"; rc=$?
    else
        # shellcheck disable=SC2086
        "$SPIKE" gnome ${SPIKE_EXTRA:-} "$@" >"$WORK/spike-$name.log" 2>&1; rc=$?
    fi
    sed 's/^/    /' "$WORK/spike-$name.log" | grep -vE 'prop (CapsLock|NumLock|SessionId)|MODIFIERS depressed=0x[01] '
    return $rc
}

t_probe() {
    spike_gnome probe --probe
    local caps
    caps=$(grep -o 'SEAT CAPS: .*' "$WORK/spike-probe.log")
    check probe "$([ -n "$caps" ]; echo $?)" "$caps"
}

t_type() {
    start_pad type
    spike_gnome type "$SENTENCE"
    local got; got=$(cat "$PAD_FILE")
    check type-focus "$FOCUS" "(testpad got wl_keyboard.enter before typing)"
    check type-exact "$([ "$got" = "$SENTENCE" ]; echo $?)" "got=[$got]"
    stop_pad
}

t_text() {
    start_pad text
    spike_gnome text --mode text 'ąčę 你好 '
    if grep -q 'sent ei_text.utf8' "$WORK/spike-text.log"; then
        check ei-text-utf8 "$([ "$(cat "$PAD_FILE")" = 'ąčę 你好 ' ]; echo $?)" "got=[$(cat "$PAD_FILE")]"
    else
        echo "INFO ei_text not offered by $COMPOSITOR (keyboard-only seat)"
    fi
    stop_pad
}

t_hold_ctrl() {
    start_pad ctrl
    spike_gnome ctrl --hold-ctrl --settle-ms 500 'abc Hello '
    local got; got=$(cat "$PAD_FILE")
    check ctrl-focus "$FOCUS" ""
    check ctrl-modifiers-forwarded "$(grep -q '\[typer\] MODIFIERS depressed=0x4 ' "$WORK/spike-ctrl.log"; echo $?)" \
        "(typer saw Control depressed from the other client)"
    check ctrl-nothing-inserted "$([ -z "$got" ]; echo $?)" "got=[$got]"
    check ctrl-released "$(tail -3 "$WORK/spike-ctrl.log" | grep -q 'MODIFIERS depressed=0x0 '; echo $?)" ""
    stop_pad
}

t_layouts() {
    start_pad layouts
    spike_gnome layouts 'ąčę '
    check layouts-focus "$FOCUS" ""
    grep -E 'keymap layouts|MODIFIERS' "$WORK/spike-layouts.log" | head -3
    check layouts-both "$(grep -q 'keymap layouts: \[.*Lithuanian' "$WORK/spike-layouts.log"; echo $?)" ""
    # ąčę exist only in the lt layout: typeable when lt is the active group, skipped otherwise.
    local want='ąčę '
    [ "${ACTIVE:-}" = lt ] || want=' '
    check "layouts-active-${ACTIVE:-first}" "$([ "$(cat "$PAD_FILE")" = "$want" ]; echo $?)" \
        "want=[$want] got=[$(cat "$PAD_FILE")]"
    stop_pad
}

t_poke() { # $1 = runs; device readiness with sync poke / plain re-poll / neither
    local n=${1:-5} mode i
    for mode in "" "--no-poke --repoll" "--no-poke"; do
        local ok=0
        for i in $(seq "$n"); do
            assert_isolated
            if "$SPIKE" gnome $mode --settle-ms 0 >"$WORK/spike-poke.log" 2>&1; then ok=$((ok + 1)); fi
            grep -o 'ready=.*' "$WORK/spike-poke.log" | sed "s/^/    [${mode:-poke}] /"
        done
        echo "poke-experiment mode=[${mode:-sync poke}] ready $ok/$n"
    done
}

setup_layouts
start_compositor
assert_isolated
# Commands run in order inside one compositor session, e.g. `type type ctrl`.
[ $# -gt 0 ] || set -- test
while [ $# -gt 0 ]; do
    cmd=$1; shift
    case "$cmd" in
        test) t_probe; t_type; t_hold_ctrl; t_text ;;
        probe) t_probe ;;
        type) t_type ;;
        ctrl) t_hold_ctrl ;;
        text) t_text ;;
        layouts) t_layouts ;;
        poke)
            if [[ "${1:-}" =~ ^[0-9]+$ ]]; then t_poke "$1"; shift; else t_poke 5; fi ;;
        shell) [ "${1:-}" = "--" ] && shift; "$@"; set -- ;;
        *) die "unknown command $cmd" ;;
    esac
done
echo "harness: $FAILS failure(s)"
[ "$FAILS" = 0 ]
