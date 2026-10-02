#!/usr/bin/env bash
# Build the portable Linux tarball.
#
#   scripts/package-linux.sh                 # build, then package
#   ANDROID_BACKUP_BIN=path/to/bin scripts/package-linux.sh   # package an existing binary
#   MAX_GLIBC=2.36 scripts/package-linux.sh  # also fail if the binary needs newer
#
# What "portable" means here, precisely: the binary's only dynamic dependencies
# are libc, libm, libgcc_s and the loader (everything else — X11, Wayland, GL,
# GTK for the file picker — is dlopen'd at runtime and must come from the
# desktop). So the artifact is a tarball, not a bundle, and the one thing that
# can make it refuse to start on a given machine is the glibc version it was
# compiled against. `.github/workflows/release.yml` builds it inside a pinned
# Debian container for that reason, and this script records the floor it
# actually got next to the artifact.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

version="$("$root/scripts/version.sh")"
name="android-backup-${version}-linux-x86_64"
dist="$root/dist"
stage="$dist/$name"

echo "== packaging $name"

if [ -n "${ANDROID_BACKUP_BIN:-}" ]; then
    bin="$ANDROID_BACKUP_BIN"
    echo "== using prebuilt binary: $bin"
else
    echo "== cargo build --release --locked"
    cargo build --release --locked -p android-backup-desktop
    bin="$root/target/release/android-backup"
fi

if [ ! -f "$bin" ]; then
    echo "package-linux.sh: no binary at $bin" >&2
    exit 1
fi

# The version the binary reports must be the version in the filename, or a
# support conversation starts with "which build are you running?" and nobody
# knows. This is cheap insurance against a stale target/ directory.
reported="$("$bin" --version 2>/dev/null | awk '{print $NF}' || echo "")"
if [ "$reported" != "$version" ]; then
    echo "package-linux.sh: binary reports '$reported' but the workspace version is '$version'" >&2
    echo "package-linux.sh: rebuild (cargo clean -p android-backup-desktop) and retry" >&2
    exit 1
fi

echo "== staging $stage"
rm -rf "$stage"
mkdir -p "$stage"

install -m755 "$bin" "$stage/android-backup"
install -m755 "$root/packaging/install.sh" "$stage/install.sh"
install -m644 "$root/packaging/android-backup.desktop" "$stage/android-backup.desktop"
install -m644 "$root/assets/icon.png" "$stage/icon.png"
for doc in README.md LICENSE-MIT LICENSE-APACHE docs/setup.md docs/troubleshooting.md; do
    install -m644 "$root/$doc" "$stage/$(basename "$doc")"
done

echo "== dependency report"
deps="$dist/$name.deps.txt"
{
    echo "$name"
    echo "built: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "rustc: $(rustc --version 2>/dev/null || echo unknown)"
    echo "host:  $(uname -srm)"
    echo
    echo "glibc floor:"
    "$root/scripts/check-glibc.sh" "$stage/android-backup" | sed 's/^/  /'
    echo
    echo "dynamic dependencies (ldd):"
    ldd "$stage/android-backup" | sed 's/^/  /'
    echo
    echo "X11, Wayland, GL and GTK are dlopen'd at runtime by winit/glutin/rfd,"
    echo "not linked, so they must come from the host desktop environment."
} > "$deps"
sed 's/^/  /' "$deps"

if [ -n "${MAX_GLIBC:-}" ]; then
    "$root/scripts/check-glibc.sh" "$stage/android-backup" "$MAX_GLIBC"
fi

echo "== writing $name.tar.gz"
archive="$dist/$name.tar.gz"
rm -f "$archive"
tar -czf "$archive" -C "$dist" "$name"
( cd "$dist" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256" )

echo
echo "artifact: $archive ($(du -h "$archive" | cut -f1))"
cat "$dist/$name.tar.gz.sha256"
