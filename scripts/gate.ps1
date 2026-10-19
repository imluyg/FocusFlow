# FocusFlow gate launcher for PowerShell.
# Keep this file ASCII-only: on this machine a BOM-less UTF-8 .ps1 with non-ASCII
# comments has silently swallowed code before. The real implementation is
# scripts/gate.sh, kept as the single source of truth so the two entry points
# cannot drift apart.

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot

$bash = $null
$found = Get-Command bash -ErrorAction SilentlyContinue
if ($found) { $bash = $found.Source }
if (-not $bash) {
    $git = Get-Command git -ErrorAction SilentlyContinue
    if ($git) {
        $dir = Split-Path -Parent $git.Source
        foreach ($candidate in @((Join-Path $dir 'bash.exe'), (Join-Path $dir '..\bin\bash.exe'), (Join-Path $dir '..\usr\bin\bash.exe'))) {
            if (Test-Path $candidate) { $bash = (Resolve-Path $candidate).Path; break }
        }
    }
}
if (-not $bash) { Write-Output 'GATE_FAILED=bash-not-found'; exit 1 }

Push-Location $repo
try {
    & $bash scripts/gate.sh
    exit $LASTEXITCODE
} finally {
    Pop-Location
}
