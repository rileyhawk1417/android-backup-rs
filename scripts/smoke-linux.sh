#!/usr/bin/env bash
# Smoke-test a Linux artifact: does it identify itself, and does it actually put
# a window on a display?
#
#   scripts/smoke-linux.sh dist/…/android-backup         # the bare binary
#   scripts/smoke-linux.sh dist/…-x86_64.AppImage        # or the AppImage
#
# The unit and integration suites prove the app's logic and run anywhere. This
# proves the *artifact*: that the packaged file works on a machine which has
# nothing but a desktop, using a throwaway HOME so the test cannot touch a real
# installation, and on a private Xvfb display so it also works on a headless CI
# runner.
#
# A GUI process that is still alive after it has been asked for a window has
# really loaded its windowing stack (winit, GL, fonts); one that cannot would
# exit immediately or never create the data directory the app writes on startup.
#
# AppImages are extracted rather than mounted, for two reasons that both keep
# this test deterministic: FUSE inside containers and CI runners is a coin flip,
# and on a desktop with AppImageLauncher installed the binfmt handler intercepts
# direct execution (and can block headlessly waiting for a dialog). Extraction
# exercises the same payload a mount would hand to the kernel, and it lets this
# script assert the AppDir structure too. Mounting is the AppImage runtime's job
# and is verified by hand once per release, not here.

set -euo pipefail

target="${1:?usage: smoke-linux.sh <binary-or-appimage>}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ ! -x "$target" ]; then
    echo "smoke-linux.sh: $target is not executable" >&2
    exit 1
fi

for tool in Xvfb xdotool; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "smoke-linux.sh: $tool not found; install the X11 test tools (xvfb, xdotool)" >&2
        exit 2
    fi
done

work="$(mktemp -d)"
log="$(mktemp)"

cleanup() {
    if [ -n "${pid:-}" ] && kill -0 "$pid" 2>/dev/null; then
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    fi
    if [ -n "${xvfb:-}" ]; then
        kill "$xvfb" 2>/dev/null || true
        wait "$xvfb" 2>/dev/null || true
    fi
    rm -rf "$work" "$log"
}
trap cleanup EXIT

# The squashfs offset is whatever byte the runtime's scanner finds, so the only
# general way to locate it without asking the runtime is to look for a squashfs
# superblock. Its magic also occurs inside the runtime's own scanner, hence
# "candidate offsets, first one that unsquashfs accepts".
extract_appimage() {
    local image="$1" dest="$2"
    local -a candidates=()
    local offset

    offset="$(timeout 20 "$image" --appimage-offset 2>/dev/null | tr -dc '0-9' || true)"
    if [ -n "$offset" ]; then
        candidates+=("$offset")
    fi
    while read -r line; do
        candidates+=("${line%%:*}")
    done < <(grep -abo 'hsqs' "$image" || true)

    if [ "${#candidates[@]}" -eq 0 ]; then
        echo "smoke-linux.sh: no squashfs superblock found in $image" >&2
        return 1
    fi

    for offset in "${candidates[@]}"; do
        rm -rf "$dest"
        if unsquashfs -q -o "$offset" -d "$dest" "$image" >/dev/null 2>&1; then
            echo "$offset"
            return 0
        fi
    done

    echo "smoke-linux.sh: could not extract $image (tried offsets: ${candidates[*]})" >&2
    echo "smoke-linux.sh: is unsquashfs built with the codec this AppImage uses?" >&2
    return 1
}

echo "== $target"
version="$("$root/scripts/version.sh")"

case "$target" in
    *.AppImage)
        if ! command -v unsquashfs >/dev/null 2>&1; then
            echo "smoke-linux.sh: unsquashfs not found (install squashfs-tools)" >&2
            exit 2
        fi
        payload="$work/payload"
        echo "== extracting the payload"
        offset="$(extract_appimage "$target" "$payload")"
        echo "== squashfs offset: $offset"

        for required in AppRun usr/bin/android-backup android-backup.desktop android-backup.png .DirIcon; do
            if [ ! -e "$payload/$required" ]; then
                echo "smoke-linux.sh: the payload is missing $required" >&2
                exit 1
            fi
        done
        for required in AppRun usr/bin/android-backup; do
            if [ ! -x "$payload/$required" ]; then
                echo "smoke-linux.sh: $required is not executable in the payload" >&2
                exit 1
            fi
        done
        echo "== payload structure: ok"
        run=("$payload/AppRun")
        ;;
    *)
        run=("$target")
        ;;
esac

reported="$("${run[@]}" --version)"
if [ "$reported" != "android-backup $version" ]; then
    echo "smoke-linux.sh: --version said '$reported', expected 'android-backup $version'" >&2
    exit 1
fi
echo "== --version: $reported"

"${run[@]}" --help >/dev/null
echo "== --help: ok"

if "${run[@]}" --definitely-not-a-flag >/dev/null 2>&1; then
    echo "smoke-linux.sh: an unknown argument exited 0" >&2
    exit 1
fi
echo "== unknown argument: rejected"

# A private display, so that `xdotool` below can look at the same one the app is
# on. (`xvfb-run` puts the display in the app's environment only, leaving the
# window search blind to it.)
display=""
for n in $(seq 90 99); do
    if [ ! -e "/tmp/.X11-unix/X$n" ]; then
        display=":$n"
        break
    fi
done
if [ -z "$display" ]; then
    echo "smoke-linux.sh: no free X display between :90 and :99" >&2
    exit 1
fi

echo "== starting Xvfb on $display"
Xvfb "$display" -screen 0 1280x900x24 -nolisten tcp >"$log" 2>&1 &
xvfb=$!
home="$work/home"
mkdir -p "$home/run"
for _ in $(seq 1 40); do
    if DISPLAY="$display" xdotool getdisplaygeometry >/dev/null 2>&1; then
        break
    fi
    if ! kill -0 "$xvfb" 2>/dev/null; then
        echo "smoke-linux.sh: Xvfb died" >&2
        sed 's/^/  /' "$log" >&2
        exit 1
    fi
    sleep 0.25
done

echo "== launching the app (isolated HOME: $home)"
# `env -i` keeps the app from reading the developer's own environment: the only
# DISPLAY is the one just created, the only XDG_* point into the throwaway HOME.
env -i HOME="$home" DISPLAY="$display" PATH="/usr/bin:/bin" \
    XDG_RUNTIME_DIR="$home/run" XDG_CACHE_HOME="$home/cache" \
    "${run[@]}" >"$log" 2>&1 &
pid=$!

window=""
for _ in $(seq 1 40); do
    if ! kill -0 "$pid" 2>/dev/null; then
        echo "smoke-linux.sh: the app exited before showing a window" >&2
        sed 's/^/  /' "$log" >&2
        exit 1
    fi
    window="$(DISPLAY="$display" xdotool search --name 'Android Backup' 2>/dev/null | head -1 || true)"
    if [ -n "$window" ]; then
        break
    fi
    sleep 0.5
done

if [ -z "$window" ]; then
    echo "smoke-linux.sh: no 'Android Backup' window appeared within 20s" >&2
    sed 's/^/  /' "$log" >&2
    exit 1
fi

echo "== window: $(DISPLAY="$display" xdotool getwindowname "$window")"
echo "== geometry: $(DISPLAY="$display" xdotool getwindowgeometry "$window" | tr '\n' ' ')"

# The app must have written its data directory under the throwaway HOME, which
# proves it ran its startup path rather than merely creating a window.
if [ ! -d "$home/.local/share/android-backup" ]; then
    echo "smoke-linux.sh: the app created no data directory under $home" >&2
    sed 's/^/  /' "$log" >&2
    exit 1
fi
echo "== data directory: ok"

if [ -s "$home/.local/share/android-backup/logs/android-backup.log" ]; then
    echo "== startup log:"
    sed 's/^/  /' "$home/.local/share/android-backup/logs/android-backup.log"
fi

kill "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true
pid=""

if [ -s "$log" ]; then
    echo "== stderr/stdout during launch (informational):"
    sed 's/^/  /' "$log"
fi

echo "smoke-linux.sh: PASS"
