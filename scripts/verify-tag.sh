#!/usr/bin/env bash
# Check that a tag is fit to publish.
#
#   scripts/verify-tag.sh v0.1.0     # explicit
#   scripts/verify-tag.sh            # the tag CI is building (GITHUB_REF_NAME)
#
# Run by the release workflow before anything is built, so a tag that disagrees
# with the source tree fails in seconds instead of publishing an artifact named
# after a version it does not contain. Everything here is a check on the tree;
# the build and the artifact smoke tests come after.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

tag="${1:-${GITHUB_REF_NAME:-}}"
if [ -z "$tag" ]; then
    tag="$(git describe --tags --exact-match 2>/dev/null || true)"
fi
if [ -z "$tag" ]; then
    echo "verify-tag.sh: no tag given, not set in GITHUB_REF_NAME, and HEAD is not tagged" >&2
    exit 1
fi

failures=0
check() {
    local what="$1" ok="$2"
    if [ "$ok" = "0" ]; then
        printf '  ok    %s\n' "$what"
    else
        printf '  FAIL  %s\n' "$what" >&2
        failures=$((failures + 1))
    fi
}

echo "verify-tag.sh: checking $tag"

case "$tag" in
    v[0-9]*) version="${tag#v}" ;;
    *) version="" ;;
esac
if printf '%s' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z][0-9A-Za-z.-]*)?$'; then
    check "tag '$tag' is v<semver>" 0
else
    check "tag '$tag' is v<semver>" 1
    version=""
fi

if [ -n "$version" ]; then
    declared="$(scripts/version.sh)"
    if [ "$declared" = "$version" ]; then
        check "Cargo.toml declares $declared" 0
    else
        check "Cargo.toml declares $version (found $declared)" 1
    fi

    # Cargo writes the member versions into the lock file, so a lock left behind
    # by the bump would be published as-is by --locked builds.
    if grep -q "name = \"android-backup-core\"" Cargo.lock \
        && grep -A1 "name = \"android-backup-core\"" Cargo.lock | grep -q "version = \"$version\""; then
        check "Cargo.lock is up to date" 0
    else
        check "Cargo.lock records $version (run scripts/bump-version.sh)" 1
    fi

    if grep -q "^## \[$version\]" CHANGELOG.md; then
        check "CHANGELOG.md has a [$version] section" 0
    else
        check "CHANGELOG.md has a [$version] section" 1
    fi

    # A section that is only a heading produces an empty release page.
    if awk -v want="## [$version]" '
            index($0, want) == 1 { printing = 1; next }
            printing && /^## \[/ { exit }
            printing && /^- / { found = 1 }
            END { exit !found }
        ' CHANGELOG.md; then
        check "the [$version] changelog entry is not empty" 0
    else
        check "the [$version] changelog entry is not empty" 1
    fi
fi

for required in \
    LICENSE-MIT LICENSE-APACHE README.md CHANGELOG.md \
    docs/setup.md docs/troubleshooting.md docs/release.md docs/testing.md \
    packaging/AppRun packaging/android-backup.desktop packaging/install.sh \
    scripts/version.sh scripts/package-linux.sh scripts/build-appimage.sh \
    scripts/package-windows.ps1 scripts/smoke-linux.sh scripts/smoke-windows.ps1 \
    .github/workflows/release.yml
do
    if [ -e "$required" ]; then
        check "$required exists" 0
    else
        check "$required exists" 1
    fi
done

# The artifacts are built from a clean checkout of the tag; anything the build
# needs that is not committed does not exist as far as a release is concerned.
for script in scripts/package-linux.sh scripts/build-appimage.sh scripts/smoke-linux.sh; do
    if [ -x "$script" ]; then
        check "$script is executable" 0
    else
        check "$script is executable (chmod +x)" 1
    fi
done

if [ "$failures" -ne 0 ]; then
    echo "verify-tag.sh: $failures check(s) failed; not publishing $tag" >&2
    exit 1
fi

echo "verify-tag.sh: $tag is fit to build"
