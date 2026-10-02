# Build the portable Windows artifact: one .exe, the docs, the licences.
#
#   pwsh -File scripts/package-windows.ps1
#   pwsh -File scripts/package-windows.ps1 -Binary target\release\android-backup.exe
#
# Run it on Windows (the CI workflow does); the binary it packages comes from
# `cargo build --release --locked`. The artifact is deliberately a plain ZIP with
# no installer: nothing is written outside the user's profile, so "installing"
# is unzipping and "uninstalling" is deleting the folder.
#
# Windows PowerShell 5.1 is the floor — the version that ships with Windows 10 —
# so nothing here uses a module that has to be installed first.

[CmdletBinding()]
param(
    # The build to package. Defaults to the release output of the desktop crate.
    [string] $Binary = '',

    # Where the ZIP and its checksum are written.
    [string] $OutDir = '',

    # Skip the "does it even run" check before packaging.
    [switch] $SkipSmoke
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrEmpty($Binary)) {
    $Binary = Join-Path (Join-Path (Join-Path $root 'target') 'release') 'android-backup.exe'
}
if ([string]::IsNullOrEmpty($OutDir)) {
    $OutDir = Join-Path $root 'dist'
}

# Built from segments rather than one string with backslashes in it: the same code
# then runs on Windows (what it is for) and on Linux (where it can be tested).
function Join-Segments {
    param([string] $Base, [string[]] $Segments)

    $path = $Base
    foreach ($segment in $Segments) {
        $path = Join-Path $path $segment
    }
    $path
}

function Get-WorkspaceVersion {
    param([string] $CargoToml)

    $text = Get-Content -Raw -LiteralPath $CargoToml
    # Only the [workspace.package] table, never a dependency's version line.
    $table = [regex]::Match($text, '(?ms)^\[workspace\.package\]\s*$(?<body>.*?)(?=^\[|\z)')
    if (-not $table.Success) {
        throw "no [workspace.package] table in $CargoToml"
    }
    $version = [regex]::Match($table.Groups['body'].Value, '(?m)^\s*version\s*=\s*"(?<v>[^"]+)"')
    if (-not $version.Success) {
        throw "no version in the [workspace.package] table of $CargoToml"
    }
    $version.Groups['v'].Value
}

$version = Get-WorkspaceVersion (Join-Path $root 'Cargo.toml')
$name = "android-backup-$version-windows-x64-portable"

if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) {
    throw "no binary at $Binary — run 'cargo build --release --locked -p android-backup-desktop' first"
}
if ((Get-Item -LiteralPath $Binary).Length -eq 0) {
    throw "$Binary is empty"
}

if (-not $SkipSmoke) {
    Write-Host "== smoke-testing the binary"
    & (Join-Path $PSScriptRoot 'smoke-windows.ps1') -Exe $Binary
    if ($LASTEXITCODE -ne 0) {
        throw "smoke-windows.ps1 failed; refusing to package a binary that does not run"
    }
}

$stage = Join-Path $OutDir $name
if (Test-Path -LiteralPath $stage) {
    Remove-Item -LiteralPath $stage -Recurse -Force
}
New-Item -ItemType Directory -Path $stage -Force | Out-Null

Write-Host "== staging $stage"
Copy-Item -LiteralPath $Binary -Destination (Join-Path $stage 'android-backup.exe')

# Documentation and licences travel with the artifact: a user who unzips this on
# a machine with no browser access should still be able to read how to use it,
# and the licences have to accompany the binary either way.
$documents = @(
    @{ From = @('README.md');                To = 'README.md' },
    @{ From = @('docs', 'setup.md');         To = 'setup.md' },
    @{ From = @('docs', 'troubleshooting.md'); To = 'troubleshooting.md' },
    @{ From = @('LICENSE-MIT');              To = 'LICENSE-MIT' },
    @{ From = @('LICENSE-APACHE');           To = 'LICENSE-APACHE' }
)
foreach ($document in $documents) {
    $from = Join-Segments $root $document.From
    if (-not (Test-Path -LiteralPath $from -PathType Leaf)) {
        throw "missing $($document.From -join '/'): the artifact would ship incomplete"
    }
    Copy-Item -LiteralPath $from -Destination (Join-Path $stage $document.To)
}

# Windows users open a README in Notepad and see Markdown punctuation; one short
# plain-text file up front is worth more than a second copy of the manual.
$first = @"
Android Backup Utility $version — portable Windows build
=========================================================

1. Connect your phone with a USB *data* cable.
2. On the phone: Settings -> About phone -> tap Build number seven times,
   then Developer options -> enable USB debugging, and approve the
   "Allow USB debugging?" prompt.
3. Run android-backup.exe. It will offer to download adb (Google's official
   platform-tools) for you if you do not already have it.

Full instructions: setup.md
When something goes wrong: troubleshooting.md
Licence: MIT or Apache-2.0, at your option (see LICENSE-MIT, LICENSE-APACHE).

This build is not code-signed, so Windows SmartScreen will ask once before the
first launch: choose "More info" -> "Run anyway". The SHA-256 published next to
the download lets you check that the file arrived intact.
"@
Set-Content -LiteralPath (Join-Path $stage 'START-HERE.txt') -Value $first -Encoding UTF8

$zip = Join-Path $OutDir "$name.zip"
if (Test-Path -LiteralPath $zip) {
    Remove-Item -LiteralPath $zip -Force
}
Write-Host "== writing $zip"
# The archive's root is the payload itself (no wrapping directory): unzipping
# anywhere gives the user the .exe directly.
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip -CompressionLevel Optimal

$hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $zip).Hash.ToLower()
# The same "<hash>  <name>" format sha256sum and shasum write, so the published
# checksum can be checked with whichever tool the user has.
Set-Content -LiteralPath "$zip.sha256" -Value "$hash  $name.zip" -Encoding ASCII

$size = (Get-Item -LiteralPath $zip).Length
Write-Host ''
Write-Host "artifact: $zip ($([math]::Round($size / 1MB, 1)) MB)"
Write-Host "$hash  $name.zip"
