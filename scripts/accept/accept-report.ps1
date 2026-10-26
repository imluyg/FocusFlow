<#
.SYNOPSIS
  Weekly report on startup: the app writes last week's report once, and never rewrites it.

.DESCRIPTION
  Two launches against a fresh COPY of desktop-native\probe (nothing touches real data).

  A few seconds after start the app looks for <data>\reports\weekly-<last Monday>.md and
  generates it only when it is missing (same generator the settings button uses). The log
  marker is ASCII on purpose: "weekly-report auto ..." the first time, "weekly-report
  exists ..." the second -- that second line is what proves the idempotent branch actually
  saw the file, rather than the check passing because nothing happened at all.

  ASCII-only, like every file in this folder.

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-report.ps1
#>
param(
    [string]$Exe = '',
    [string]$WorkDir = ''
)

. "$PSScriptRoot\accept-lib.ps1"

if (-not $Exe) { $Exe = Get-DefaultExe }
if (-not $WorkDir) { $WorkDir = Join-Path (Get-RepoRoot) 'target\accept\report' }

$app = Join-Path $WorkDir 'app'
$logPath = Join-Path $WorkDir 'focusflow.log'
$dir = Join-Path $app 'data\reports'

function Get-Reports {
    if (-not (Test-Path $dir)) { return @() }
    @(Get-ChildItem $dir -Filter 'weekly-*.md' -File -ErrorAction SilentlyContinue)
}

function Get-RepLog {
    if (Test-Path $logPath) { Get-Content $logPath -Encoding UTF8 } else { @() }
}

function Wait-RepLog([string]$Pattern, [int]$TimeoutMs = 12000) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalMilliseconds -lt $TimeoutMs) {
        if (@(Get-RepLog | Where-Object { $_ -match $Pattern }).Count -gt 0) { return $true }
        Start-Sleep -Milliseconds 250
    }
    $false
}

function Stop-Rep([IntPtr]$Hwnd, $Proc) {
    if ($Hwnd -ne [IntPtr]::Zero) {
        [void][FF]::PostMessage($Hwnd, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
    }
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while (-not $Proc.HasExited -and $sw.Elapsed.TotalMilliseconds -lt 10000) {
        Start-Sleep -Milliseconds 150
    }
    if (-not $Proc.HasExited) { Stop-App }
}

"exe : $Exe"
Stop-App
New-AppDir $app @('[collector]', 'enabled = false')
Remove-Item $logPath -ErrorAction SilentlyContinue
"app : $app"

Check 'no report file before the first launch' ((Get-Reports).Count -eq 0)

$p = Start-App $Exe $app $WorkDir
$h = Wait-Window $p.Id
Check 'main window appears' ($h -ne [IntPtr]::Zero)
if ($h -eq [IntPtr]::Zero) { Stop-App; Show-Summary }

Check 'the startup pass generated one' (Wait-RepLog 'weekly-report auto')
$files = Get-Reports
Check 'the report file is on disk' ($files.Count -eq 1) "count=$($files.Count)"
$one = $files | Select-Object -First 1
Check 'the file name is weekly-<date>.md' ($one.Name -match '^weekly-\d{4}-\d{2}-\d{2}\.md$') $one.Name
Check 'the report has content' ($one.Length -gt 200) "$($one.Length) bytes"
# Read the timestamp through the file object: on a missing file this must be a real
# "never written" value, so the last check cannot pass on two empty strings.
$written = if ($one) { $one.LastWriteTime } else { [datetime]::MinValue }
Stop-Rep $h $p

# ---------- second launch: the file is already there, nothing may be rewritten ----------
Remove-Item $logPath -ErrorAction SilentlyContinue
$p2 = Start-App $Exe $app $WorkDir
$h2 = Wait-Window $p2.Id
Check 'the second launch starts' ($h2 -ne [IntPtr]::Zero)
Check 'the startup pass sees the existing file' (Wait-RepLog 'weekly-report exists')
$again = Get-Reports
Check 'still exactly one report' ($again.Count -eq 1) "count=$($again.Count)"
$now = if ($again.Count -gt 0) { ($again | Select-Object -First 1).LastWriteTime } else { [datetime]::MaxValue }
Check 'the existing report was not rewritten' ($now -eq $written) "$written / $now"
Stop-Rep $h2 $p2

Show-Summary
