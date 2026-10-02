# Smoke-test the Windows build: does the .exe identify itself, and does it start
# as a real application?
#
#   pwsh -File scripts/smoke-windows.ps1
#
# The unit and integration suites run on every platform and prove the app's
# logic. This proves the *artifact*: that the packaged .exe runs on a machine
# with no build tools, using a throwaway APPDATA so the test cannot touch a real
# installation.
#
# The "does it start" part is the one thing cargo cannot tell us. A missing
# runtime DLL (a VC++ redistributable that this machine happens to have) means
# CreateProcess fails outright, which the --version check catches; a startup bug
# that only bites an empty configuration is caught by launching the app and
# checking that it got far enough to write its data directory.

[CmdletBinding()]
param(
    [string] $Exe = '',

    # How long to wait for the app to create its data directory.
    [int] $TimeoutSeconds = 30
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

# 4. It starts as an application, in a throwaway profile. The app keeps its
#    config, manifest and logs together in its data directory, which is under
#    %APPDATA% on Windows and under $HOME on Linux — hence the candidates.
$sandbox = Join-Path ([System.IO.Path]::GetTempPath()) ("android-backup-smoke-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $sandbox -Force | Out-Null

$saved = @{}
foreach ($variable in 'APPDATA', 'LOCALAPPDATA', 'USERPROFILE', 'HOME') {
    $saved[$variable] = [Environment]::GetEnvironmentVariable($variable)
}
try {
    [Environment]::SetEnvironmentVariable('APPDATA', $sandbox)
    [Environment]::SetEnvironmentVariable('LOCALAPPDATA', $sandbox)
    [Environment]::SetEnvironmentVariable('USERPROFILE', $sandbox)
    [Environment]::SetEnvironmentVariable('HOME', $sandbox)

    Write-Host "== launching the app (sandbox: $sandbox)"
    $process = Start-Process -FilePath $Exe -PassThru
    $dataCandidates = @(
        (Join-Path $sandbox 'android-backup'),
        (Join-Path (Join-Path (Join-Path $sandbox '.local') 'share') 'android-backup'),
        (Join-Path (Join-Path $sandbox '.config') 'android-backup')
    )

    $found = ''
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 250
        $process.Refresh()
        if ($process.HasExited) {
            throw "the app exited with code $($process.ExitCode) before creating its data directory"
        }
        $found = $dataCandidates | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
        if (-not [string]::IsNullOrEmpty($found)) {
            break
        }
    }

    if ([string]::IsNullOrEmpty($found)) {
        throw "the app did not create its data directory under $sandbox within $TimeoutSeconds seconds"
    }
    Write-Host "== data directory: $found"

    # Informational: a window title confirms the window itself came up, but a
    # build agent's session is allowed not to have a window station, so its
    # absence is reported rather than fatal.
    $process.Refresh()
    $title = $process.MainWindowTitle
    if ([string]::IsNullOrEmpty($title)) {
        Write-Host '== window: not reported by this session (the process is alive and past startup)'
    }
    else {
        if ($title -notlike '*Android Backup*') {
            throw "unexpected window title '$title'"
        }
        Write-Host "== window: $title"
    }

    if (-not $process.HasExited) {
        Stop-Process -Id $process.Id -Force
        $process.WaitForExit()
    }
}
finally {
    foreach ($variable in $saved.Keys) {
        [Environment]::SetEnvironmentVariable($variable, $saved[$variable])
    }
    Remove-Item -LiteralPath $sandbox -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host 'smoke-windows.ps1: PASS'
