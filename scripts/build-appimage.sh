#!/usr/bin/env bash
# Build the single-file Linux artifact (AppImage).
#
#   scripts/build-appimage.sh
#   ANDROID_BACKUP_BIN=path/to/bin scripts/build-appimage.sh
#   APPIMAGETOOL=/usr/local/bin/appimagetool scripts/build-appimage.sh
#
# The AppDir is written by hand rather than assembled by `linuxdeploy`. That is
# a deliberate choice, not a shortcut: `ldd` on this binary shows four
# dependencies (libc, libm, libgcc_s, the loader) and everything else is loaded
# at runtime by the desktop, so there is nothing for a dependency-bundling tool
# to find. A hand-written AppDir is the whole recipe in one readable file:
#
#   AppRun                                    the entry point
#   android-backup.desktop, android-backup.png   required at the root by appimagetool
#   usr/bin/android-backup                    the payload
#   usr/share/{applications,icons}/...        the same files, where a desktop looks
#
# `appimagetool` itself is a prebuilt appimage downloaded from the internet and
# executed, so it is pinned to a tag **and** a SHA-256 that is checked on every
# build — the same rule the managed adb installer follows (E07.1). Bump both
# together, deliberately.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

version="$("$root/scripts/version.sh")"
name="android-backup-${version}-linux-x86_64"
dist="$root/dist"
appdir="$dist/$name.AppDir"
out="$dist/$name.AppImage"

appimagetool_version="1.9.1"
appimagetool_sha256="ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0"
appimagetool_url="https://github.com/AppImage/appimagetool/releases/download/${appimagetool_version}/appimagetool-x86_64.AppImage"

echo "== building $name.AppImage"

if [ -n "${ANDROID_BACKUP_BIN:-}" ]; then
    bin="$ANDROID_BACKUP_BIN"
    echo "== using prebuilt binary: $bin"
else
    echo "== cargo build --release --locked"
    cargo build --release --locked -p android-backup-desktop
    bin="$root/target/release/android-backup"
fi

if [ ! -f "$bin" ]; then
    echo "build-appimage.sh: no binary at $bin" >&2
    exit 1
fi

reported="$("$bin" --version 2>/dev/null | awk '{print $NF}' || echo "")"
if [ "$reported" != "$version" ]; then
    echo "build-appimage.sh: binary reports '$reported', workspace version is '$version'" >&2
    exit 1
fi

# --- appimagetool ------------------------------------------------------------

if [ -n "${APPIMAGETOOL:-}" ]; then
    tool="$APPIMAGETOOL"
    echo "== using appimagetool from APPIMAGETOOL: $tool"
else
    cache="$root/.cache/appimage"
    tool="$cache/appimagetool-${appimagetool_version}-x86_64.AppImage"
    mkdir -p "$cache"
    if [ ! -f "$tool" ]; then
        echo "== downloading appimagetool $appimagetool_version"
        tmp="$tool.part"
        rm -f "$tmp"
        curl -fsSL -o "$tmp" "$appimagetool_url"
        mv "$tmp" "$tool"
    fi
    echo "== verifying appimagetool"
    actual="$(sha256sum "$tool" | cut -d' ' -f1)"
    if [ "$actual" != "$appimagetool_sha256" ]; then
        echo "build-appimage.sh: appimagetool checksum mismatch" >&2
        echo "  expected $appimagetool_sha256" >&2
        echo "  actual   $actual" >&2
        exit 1
    fi
    chmod +x "$tool"
fi

# --- AppDir -----------------------------------------------------------------

echo "== staging $appdir"
rm -rf "$appdir"
mkdir -p "$appdir/usr/bin" \
    "$appdir/usr/share/applications" \
    "$appdir/usr/share/icons/hicolor/256x256/apps"

install -m755 "$bin" "$appdir/usr/bin/android-backup"
install -m755 "$root/packaging/AppRun" "$appdir/AppRun"
install -m644 "$root/packaging/android-backup.desktop" "$appdir/android-backup.desktop"
install -m644 "$root/assets/icon.png" "$appdir/android-backup.png"
install -m644 "$root/packaging/android-backup.desktop" \
    "$appdir/usr/share/applications/android-backup.desktop"
install -m644 "$root/assets/icon.png" \
    "$appdir/usr/share/icons/hicolor/256x256/apps/android-backup.png"

if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "$appdir/android-backup.desktop"
    echo "== desktop entry valid"
fi

# --- AppImage ---------------------------------------------------------------

echo "== running appimagetool"
rm -f "$out"
# APPIMAGE_EXTRACT_AND_RUN makes the tool work with or without FUSE, which is
# what a build container without /dev/fuse needs.
APPIMAGE_EXTRACT_AND_RUN=1 ARCH=x86_64 "$tool" -n --comp zstd "$appdir" "$out"

chmod +x "$out"
( cd "$dist" && sha256sum "$name.AppImage" > "$name.AppImage.sha256" )

echo
echo "artifact: $out ($(du -h "$out" | cut -f1))"
cat "$dist/$name.AppImage.sha256"
