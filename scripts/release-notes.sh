#!/usr/bin/env bash
# Compose the text of a GitHub release: the changelog entry, the artifacts with
# their checksums, and what a reader needs to know before running them.
#
#   scripts/release-notes.sh 0.1.0 dist > dist/release-notes.md
#
# Written by the release workflow (which then uses it as the release body) and
# usable by hand, which is the point: the notes can be read and corrected before
# they are published, instead of being assembled inside a CI step where a typo in
# an artifact name goes unnoticed.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

version="${1:?usage: release-notes.sh <version> [dist-dir]}"
dist="${2:-dist}"

if [ ! -f CHANGELOG.md ]; then
    echo "release-notes.sh: no CHANGELOG.md" >&2
    exit 1
fi

# The changelog section, verbatim: from "## [version]" to the next "## [".
section="$(awk -v want="## [$version]" '
    index($0, want) == 1 { printing = 1; next }
    printing && /^## \[/ { exit }
    printing { print }
' CHANGELOG.md)"

if [ -z "$section" ]; then
    echo "release-notes.sh: CHANGELOG.md has no '## [$version]' section" >&2
    exit 1
fi

# Drop link definitions and any blank lines at the edges, but keep the blank
# lines inside the section: they separate the sub-headings from the prose.
section="$(printf '%s\n' "$section" | sed '/^\[[^]]*\]:/d')"
section="$(printf '%s\n' "$section" | awk '
    NF { started = 1 }
    started { lines[++n] = $0 }
    END {
        while (n > 0 && lines[n] ~ /^[[:space:]]*$/) n--
        for (i = 1; i <= n; i++) print lines[i]
    }
')"

printf 'Android Backup Utility %s\n' "$version"

# Only the newest section is copied from the changelog, so say where the rest is.
printf '\n%s\n' "$section"
printf '\n[Full changelog](CHANGELOG.md) · [Setup](docs/setup.md) · [Troubleshooting](docs/troubleshooting.md)\n'

printf '\n## Downloads\n\n'
if [ -d "$dist" ]; then
    # Tarball first: on Linux it is the artifact we stand behind. The AppImage is a
    # convenience wrapper around the same binary, and is listed (and labelled) as
    # best-effort because mounting it depends on FUSE and on AppImageLauncher not
    # being in the way.
    for pattern in '*.tar.gz' '*.zip' '*.AppImage'; do
        for artifact in "$dist"/$pattern; do
            [ -f "$artifact" ] || continue
            name="$(basename "$artifact")"
            checksum=""
            if [ -f "$artifact.sha256" ]; then
                checksum="$(awk '{print $1}' "$artifact.sha256")"
            fi
            note=""
            case "$name" in
                *.AppImage) note=" — convenience wrapper; needs FUSE (see the tarball if it will not start)" ;;
                *.tar.gz)   note=" — recommended on Linux" ;;
                *.zip)      note=" — portable, no installer" ;;
            esac
            if [ -n "$checksum" ]; then
                printf -- '- `%s`%s\n  - SHA-256 `%s`\n' "$name" "$note" "$checksum"
            else
                printf -- '- `%s`%s\n' "$name" "$note"
            fi
        done
    done
else
    printf '(artifacts are attached to this release)\n'
fi

cat <<'NOTES'

Each artifact contains the documentation and the licence texts. `adb` is **not**
bundled: on first launch the app offers to download Google's official
platform-tools and verifies it against the checksum Google publishes.

Verifying a download before running it:

```sh
sha256sum -c android-backup-<version>-linux-x86_64.tar.gz.sha256   # Linux
shasum -a 256 -c android-backup-<version>-linux-x86_64.AppImage.sha256
```
```powershell
Get-FileHash .\android-backup-<version>-windows-x64-portable.zip -Algorithm SHA256
```

**These builds are not code-signed.** On Windows, SmartScreen will ask once
before the first launch ("More info" → "Run anyway"); the checksum above is how
you know what you are running. The reasoning is recorded in
[docs/release.md](docs/release.md), and so is what signing would change.

Requires Windows 10/11 x64, or a Linux desktop with glibc newer than the floor
recorded in the attached `.deps.txt` (the AppImage and the tarball are built
against an older glibc on purpose, to run on LTS distributions).
NOTES
