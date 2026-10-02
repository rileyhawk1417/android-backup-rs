#!/usr/bin/env bash
# Print the workspace version — the single source of truth for it.
#
# Root `Cargo.toml` declares `[workspace.package] version` and every crate
# inherits it with `version.workspace = true`, so there is exactly one place to
# look and this script is that place. Packaging scripts and the release
# workflow use it so that artifact names, the tag, and the binary's own
# `--version` can never disagree by accident.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

version="$(
    awk '
        /^\[workspace\.package\]/ { inside = 1; next }
        /^\[/ { inside = 0 }
        inside && /^[[:space:]]*version[[:space:]]*=/ {
            gsub(/.*=[[:space:]]*"|".*/, "")
            print
            exit
        }
    ' "$root/Cargo.toml"
)"

if [ -z "$version" ]; then
    echo "version.sh: no version found in $root/Cargo.toml" >&2
    exit 1
fi

echo "$version"
