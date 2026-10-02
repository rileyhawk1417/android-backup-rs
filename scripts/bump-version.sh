#!/usr/bin/env bash
# Set the project version, in the one place it lives.
#
#   scripts/bump-version.sh 0.2.0
#
# The version is declared once, in the root Cargo.toml's [workspace.package] table,
# and every crate inherits it (`version.workspace = true`), so a release cannot end
# up with a binary that disagrees with its tag. The script refuses to touch a
# version line outside that table, which is what keeps that promise true.
#
# It does not commit, tag, or push. A release is a deliberate act: read the
# CHANGELOG, run the scripts/verify-tag.sh checks, then tag.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
manifest="$root/Cargo.toml"

new="${1:?usage: bump-version.sh <new-version>}"
if ! printf '%s' "$new" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z][0-9A-Za-z.-]*)?$'; then
    echo "bump-version.sh: '$new' is not a semantic version (e.g. 0.2.0 or 0.2.0-rc.1)" >&2
    exit 1
fi

current="$("$root/scripts/version.sh")"
if [ "$new" = "$current" ]; then
    echo "bump-version.sh: already at $new; nothing to do"
    exit 0
fi

updated="$(mktemp)"
trap 'rm -f "$updated"' EXIT

awk -v version="$new" '
    /^\[/ { in_workspace_package = ($0 == "[workspace.package]") }
    in_workspace_package && /^version[[:space:]]*=/ && !done {
        sub(/"[^"]*"/, "\"" version "\"")
        done = 1
    }
    { print }
    END { if (!done) exit 3 }
' "$manifest" >"$updated" || {
    echo "bump-version.sh: no version line in [workspace.package] in $manifest" >&2
    exit 1
}

# A sanity check that the edit changed exactly one line, and that nothing else in
# the file moved: this is a release script, and quiet damage is the one failure
# mode worth being pedantic about.
if [ "$(diff <(cat "$manifest") <(cat "$updated") | grep -c '^<')" -ne 1 ]; then
    echo "bump-version.sh: refusing to write — the edit changed more than one line" >&2
    diff "$manifest" "$updated" >&2 || true
    exit 1
fi

cat "$updated" >"$manifest"

# Cargo records workspace members' versions in the lock file; refresh it without
# touching any dependency's version.
cargo metadata --manifest-path "$manifest" --format-version 1 --no-deps >/dev/null

if ! grep -q "version = \"$new\"" "$manifest"; then
    echo "bump-version.sh: the manifest does not read back as $new" >&2
    exit 1
fi

echo "bump-version.sh: $current -> $new"
echo
echo "next:"
echo "  1. add a '## [$new]' section to CHANGELOG.md"
echo "  2. git commit -am \"chore(release): $new\""
echo "  3. git tag -a v$new -m \"v$new\" && git push origin v$new"
