#!/usr/bin/env bash
# Does the tray icon really exist on a Linux desktop, and does it work?
#
#   scripts/smoke-tray.sh dist/…/android-backup        # the bare binary
#   scripts/smoke-tray.sh dist/…-x86_64.AppImage       # or the AppImage
#
# The unit suite drives the tray through a recording double, because a tray needs a
# session bus and a notification host and no test may own either. This asks the
# artifact instead, on a private Xvfb display and a private session bus, with
# scripts/tray-probe.py standing in for the panel: it holds
# org.kde.StatusNotifierWatcher, reads the menu the app publishes over
# com.canonical.dbusmenu, clicks items the way a panel does, and checks what changed.
#
# That is the whole feature from the outside — icon, menu, clicks, and the app's own
# handling of them:
#
#   * the app starts with `--hidden`, so there is no window, and the tray is the only
#     way in (which is close-to-tray, the point of the feature);
#   * "Open Android Backup" brings the window back;
#   * "Pause automatic backups" comes back as "Resume automatic backups" and is
#     written to the configuration file — a round trip through the app's intents;
#   * "Quit" ends the process, and it ends it by itself, cleanly.
#
# This script exists because of the bug it would have caught: the first 0.2.0 build
# built the icon on the window's thread, where gtk-rs asserts that GTK was never
# initialised. The window appeared, the process died behind a crash dialog, and
# `smoke-linux.sh` — which found a window by name, and a crash dialog carries that
# name — called it a pass. Nothing tested the tray; now something does.

set -euo pipefail

target="${1:?usage: smoke-tray.sh <binary-or-appimage>}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ ! -x "$target" ]; then
    echo "smoke-tray.sh: $target is not executable" >&2
    exit 1
fi

# A bus of its own, so that the probe's stand-in watcher cannot displace a real
# panel's on the machine running the test, and a real panel cannot answer in its
# place. `dbus-run-session` only sets the variable for the child it starts, hence
# the marker: a developer's own session bus must never be used, and this script
# cannot tell "already inside" from "running on a desktop".
if [ -z "${TRAY_SMOKE_BUS:-}" ]; then
    if ! command -v dbus-run-session >/dev/null 2>&1; then
        echo "smoke-tray.sh: dbus-run-session not found; install the dbus package" >&2
        exit 2
    fi
    self="$root/scripts/$(basename "${BASH_SOURCE[0]}")"
    # The private bus's daemon narrates every service it fails to activate
    # (a11y, systemd, a portal) on stderr, and the desktop libraries add their own
    # warnings about a session that has no panel in it. None of that is this
    # script's business — so stderr is kept aside and shown only if the run fails,
    # which leaves a passing log readable and a failing one complete.
    errlog="$(mktemp)"
    set +e
    timeout 600 env TRAY_SMOKE_BUS=1 dbus-run-session -- "$self" "$@" 2>"$errlog"
    status=$?
    set -e
    if [ "$status" -ne 0 ]; then
        sed 's/^/  /' "$errlog" >&2
    fi
    rm -f "$errlog"
    exit "$status"
fi

for tool in Xvfb xdotool python3 timeout; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "smoke-tray.sh: $tool not found; install xvfb, xdotool and python3" >&2
        exit 2
    fi
done
if ! python3 -c 'import gi; gi.require_version("Gio", "2.0")' >/dev/null 2>&1; then
    echo "smoke-tray.sh: python3 cannot import Gio; install python3-gi and gir1.2-glib-2.0" >&2
    exit 2
fi

probe="$root/scripts/tray-probe.py"
work="$(mktemp -d)"
log="$work/app.log"

cleanup() {
    for job in "${app:-}" "${watcher:-}" "${xvfb:-}"; do
        if [ -n "$job" ] && kill -0 "$job" 2>/dev/null; then
            kill "$job" 2>/dev/null || true
            wait "$job" 2>/dev/null || true
        fi
    done
    rm -rf "$work"
}
trap cleanup EXIT

if [[ "$target" == *.AppImage ]]; then
    payload="$work/payload"
    offset="$(timeout 20 "$target" --appimage-offset 2>/dev/null | tr -dc '0-9' || true)"
    if [ -z "$offset" ] || ! unsquashfs -q -o "$offset" -d "$payload" "$target" >/dev/null 2>&1; then
        echo "smoke-tray.sh: could not extract $target (is squashfs-tools installed?)" >&2
        exit 1
    fi
    run=("$payload/AppRun")
else
    run=("$target")
fi

# A display for this script alone. `smoke-linux.sh` starts in the nineties and calls
# this one, so the eighties are free.
display=""
for n in $(seq 80 89); do
    if [ ! -e "/tmp/.X11-unix/X$n" ]; then
        display=":$n"
        break
    fi
done
if [ -z "$display" ]; then
    echo "smoke-tray.sh: no free X display between :80 and :89" >&2
    exit 1
fi

echo "== starting Xvfb on $display"
Xvfb "$display" -screen 0 1280x900x24 -nolisten tcp >"$work/xvfb.log" 2>&1 &
xvfb=$!
for _ in $(seq 1 40); do
    if DISPLAY="$display" xdotool getdisplaygeometry >/dev/null 2>&1; then
        break
    fi
    if ! kill -0 "$xvfb" 2>/dev/null; then
        echo "smoke-tray.sh: Xvfb died" >&2
        sed 's/^/  /' "$work/xvfb.log" >&2
        exit 1
    fi
    sleep 0.25
done

echo "== taking org.kde.StatusNotifierWatcher on the private bus"
python3 "$probe" watch >"$work/watcher.log" 2>&1 &
watcher=$!
for _ in $(seq 1 40); do
    grep -q '^READY$' "$work/watcher.log" 2>/dev/null && break
    sleep 0.25
done
if ! grep -q '^READY$' "$work/watcher.log" 2>/dev/null; then
    echo "smoke-tray.sh: the probe could not take the watcher name" >&2
    sed 's/^/  /' "$work/watcher.log" >&2
    exit 1
fi

home="$work/home"
mkdir -p "$home/run" "$home/config" "$home/cache"
echo "== launching the app hidden (isolated HOME: $home)"
# `env -i` as in smoke-linux.sh: nothing of the developer's environment reaches the
# app except the private bus and the throwaway display created above.
env -i HOME="$home" DISPLAY="$display" PATH="/usr/bin:/bin" \
    XDG_RUNTIME_DIR="$home/run" XDG_CONFIG_HOME="$home/config" \
    XDG_CACHE_HOME="$home/cache" DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-}" \
    "${run[@]}" --hidden >"$log" 2>&1 &
app=$!

applog="$home/.local/share/android-backup/logs/android-backup.log"
item=""
for _ in $(seq 1 80); do
    if ! kill -0 "$app" 2>/dev/null; then
        echo "smoke-tray.sh: the app exited instead of installing a tray icon" >&2
        sed 's/^/  /' "$applog" 2>/dev/null >&2
        sed 's/^/  /' "$log" >&2
        exit 1
    fi
    if [ -s "$applog" ] && grep -q 'tray icon installed' "$applog"; then
        item="$(sed -n 's/^REGISTERED \([^ ]*\) \(.*\)$/\1 \2/p' "$work/watcher.log" | head -1)"
        [ -n "$item" ] && break
    fi
    sleep 0.5
done

if [ ! -s "$applog" ] || ! grep -q 'tray icon installed' "$applog"; then
    echo "smoke-tray.sh: no tray icon was installed within 40s" >&2
    sed 's/^/  /' "$applog" 2>/dev/null >&2
    sed 's/^/  /' "$log" >&2
    exit 1
fi
echo "== tray icon installed"

if [ -z "$item" ]; then
    echo "smoke-tray.sh: the icon never registered with the watcher" >&2
    sed 's/^/  /' "$work/watcher.log" >&2
    exit 1
fi
service="${item%% *}"
path="${item#* }"
echo "== registered as $service at $path"

if [ "$(DISPLAY="$display" xdotool search --onlyvisible --name 'Android Backup' 2>/dev/null | wc -l)" != "0" ]; then
    echo "smoke-tray.sh: --hidden left a window on screen" >&2
    exit 1
fi
echo "== hidden start: no window on screen"

echo "== reading the menu the app publishes for the panel"
python3 "$probe" menu "$service" "$path/Menu" --brief

# One click at a time, because each one is a claim about the world that the next
# could undo: the window that "Open" shows is gone again after "Quit".
echo "== clicking 'Open Android Backup'"
python3 "$probe" menu "$service" "$path/Menu" --brief --click "Open Android Backup"

shown=0
for _ in $(seq 1 40); do
    shown="$(DISPLAY="$display" xdotool search --onlyvisible --name 'Android Backup' 2>/dev/null | wc -l)"
    [ "$shown" != "0" ] && break
    sleep 0.25
done
if [ "$shown" = "0" ]; then
    echo "smoke-tray.sh: the tray's 'Open Android Backup' did not bring the window back" >&2
    sed 's/^/  /' "$applog" >&2
    exit 1
fi
echo "== the tray brought the window back"

echo "== clicking 'Pause automatic backups'"
python3 "$probe" menu "$service" "$path/Menu" --brief \
    --click "Pause automatic backups" --expect "Resume automatic backups"

config="$home/config/android-backup/config.toml"
if ! grep -q '^paused = true' "$config" 2>/dev/null; then
    echo "smoke-tray.sh: the tray's pause toggle was not written to $config" >&2
    sed 's/^/  /' "$config" 2>/dev/null >&2
    exit 1
fi
echo "== the toggle is in the configuration"

echo "== clicking 'Quit'"
python3 "$probe" menu "$service" "$path/Menu" --brief --click "Quit"

for _ in $(seq 1 40); do
    if ! kill -0 "$app" 2>/dev/null; then
        break
    fi
    sleep 0.25
done
if kill -0 "$app" 2>/dev/null; then
    echo "smoke-tray.sh: the tray's Quit did not end the process" >&2
    sed 's/^/  /' "$applog" >&2
    exit 1
fi
status=0
wait "$app" || status=$?
app=""
if [ "$status" -ne 0 ]; then
    echo "smoke-tray.sh: the app exited with status $status after Quit" >&2
    sed 's/^/  /' "$applog" >&2
    exit 1
fi
echo "== Quit ended the process, cleanly (status 0)"

if grep -qi 'panic' "$applog"; then
    echo "smoke-tray.sh: the log mentions a panic" >&2
    grep -i 'panic' "$applog" | sed 's/^/  /' >&2
    exit 1
fi

if [ -s "$log" ]; then
    echo "== stderr/stdout during launch (informational):"
    sed 's/^/  /' "$log"
fi

echo "smoke-tray.sh: PASS"

