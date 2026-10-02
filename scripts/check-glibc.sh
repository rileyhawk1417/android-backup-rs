#!/usr/bin/env bash
# Report — and optionally gate on — the oldest glibc a Linux binary needs.
#
# An AppImage cannot bundle glibc: the loader and libc must match the host, so
# "portable Linux artifact" really means "built against an old enough glibc".
# Symbols are versioned, so the answer is readable straight out of the binary:
# the highest `GLIBC_x.y` in its dynamic symbol table is a hard floor on the
# systems it will start on. Building on a rolling-release distro silently
# produces a binary that refuses to start on an LTS one, which is why the
# release build happens in a pinned container and this check runs on the result.
#
# Usage:
#   scripts/check-glibc.sh target/release/android-backup            # report
#   scripts/check-glibc.sh target/release/android-backup 2.36       # gate
#
# With a maximum, exits non-zero when the binary needs something newer.

set -euo pipefail

binary="${1:?usage: check-glibc.sh <binary> [max-version]}"
max="${2:-}"

if [ ! -f "$binary" ]; then
    echo "check-glibc.sh: no such file: $binary" >&2
    exit 2
fi

if ! command -v objdump >/dev/null 2>&1; then
    echo "check-glibc.sh: objdump not found (binutils)" >&2
    exit 2
fi

required="$(
    objdump -T "$binary" 2>/dev/null |
        grep -oE 'GLIBC_[0-9]+(\.[0-9]+)*' |
        sed 's/^GLIBC_//' |
        sort -Vu |
        tail -1
)"

if [ -z "$required" ]; then
    echo "$binary: no versioned glibc symbols (statically linked?) — no floor"
    exit 0
fi

echo "$binary: needs glibc >= $required"

if [ -n "$max" ]; then
    newest="$(printf '%s\n%s\n' "$required" "$max" | sort -V | tail -1)"
    if [ "$newest" != "$max" ]; then
        echo "check-glibc.sh: FAIL — needs glibc $required, release floor is $max" >&2
        echo "check-glibc.sh: build in the pinned older container instead of this host" >&2
        exit 1
    fi
    echo "check-glibc.sh: ok — at or below the $max floor"
fi
