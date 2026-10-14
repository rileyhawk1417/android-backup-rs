# Smoke-test the Windows build: does the .exe identify itself, and does it
# answer its command line?
#
#   pwsh -File scripts/smoke-windows.ps1
#
# The unit and integration suites run on every platform and prove the app's
# logic. This proves the *artifact*: that the packaged .exe runs on a machine
# that has no build tools and no configuration, and that its command line does
# what cli.rs (tested, and re-proved here on the real binary) says it does.
#
# The three checks are the ones cargo cannot substitute for:
# 1. It runs at all — this is the DLL check. A binary whose imports cannot be
#    resolved (a missing VC++ redistributable this machine happens to have)
#    never reaches its own first line of output; --version catches it.
# 2. The version string ties the artifact to the tree it claims to be built
#    from — what the tarball check depends on.
# 3. A typo is an error, not a window that opens with the default
#    configuration.
#
# Deliberately absent: launching the window. A build agent is exactly the
# machine docs/troubleshooting.md §22 rules out — a runner's graphics stack has
# no usable OpenGL, and the renderer refuses before the app creates its first
# directory. A GUI launch check there could never pass, and the first release
# cycle of this edit proved it by failing. The window is instead hosted where a
# display exists: the Linux smoke job (smoke-linux.sh) puts a real window on
# Xvfb — same binary sources, same winit/egui stack — and the Windows
# data-directory + tray behavior is checked by a human on a GL-capable machine
# (3D-accelerated VM counts; §22 has the two options).

[CmdletBinding()]
param(
    [string] $Exe = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
# Windows PowerShell 5.1 tolerates this; PowerShell 7.3+ turns native non-zero
# exits into terminating errors unless told not to, and this script checks exit
# codes itself.
$PSNativeCommandUseErrorActionPreference = $false

$root = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrEmpty($Exe)) {
    $Exe = Join-Path (Join-Path (Join-Path $root 'target') 'release') 'android-backup.exe'
}

if (-not (Test-Path -LiteralPath $Exe -PathType Leaf)) {
    throw "no binary at $Exe — run 'cargo build --release --locked -p android-backup-desktop' first"
}
$Exe = (Resolve-Path -LiteralPath $Exe).Path

function Get-WorkspaceVersion {
    $text = Get-Content -Raw -LiteralPath (Join-Path $root 'Cargo.toml')
    $table = [regex]::Match($text, '(?ms)^\[workspace\.package\]\s*$(?<body>.*?)(?=^\[|\z)')
    $version = [regex]::Match($table.Groups['body'].Value, '(?m)^\s*version\s*=\s*"(?<v>[^"]+)"')
    $version.Groups['v'].Value
}

$version = Get-WorkspaceVersion

Write-Host "== $Exe"

# 1. It runs at all. This is also the DLL check: a binary whose imports cannot be
#    resolved never reaches its own first line of output.
$reported = & $Exe --version
if ($LASTEXITCODE -ne 0) {
    throw "--version exited $LASTEXITCODE"
}
if ($reported -ne "android-backup $version") {
    throw "--version said '$reported', expected 'android-backup $version'"
}
Write-Host "== --version: $reported"

# 2. --help writes usage and succeeds.
& $Exe --help | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw "--help exited $LASTEXITCODE"
}
Write-Host '== --help: ok'

# 3. A typo is an error, not a window that opens with the default configuration.
& $Exe --definitely-not-a-flag 2>&1 | Out-Null
if ($LASTEXITCODE -eq 0) {
    throw 'an unknown argument exited 0'
}
Write-Host '== unknown argument: rejected'

Write-Host 'smoke-windows.ps1: PASS'