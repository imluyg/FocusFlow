<#
.SYNOPSIS
  Collector end-to-end: counts clicks, pause really stops, resume continues, quit flushes.

.DESCRIPTION
  Uses SCREEN-level clicks (the global hook sees those; injected KEYBOARD events do not
  reach low-level keyboard hooks on this machine -- see plan section 10p). Numbers are read
  from the app's acceptance status line, not from screenshots:
      <prefix> today=42 cpm=7 collector=on paused=false

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-collect.ps1
#>
param(
    [string]$Exe = '',
    [string]$WorkDir = '',
    [int]$Burst = 5
)

. "$PSScriptRoot\accept-lib.ps1"

if (-not $Exe) { $Exe = Get-DefaultExe }
if (-not $WorkDir) { $WorkDir = Join-Path (Get-RepoRoot) 'target\accept\collect' }

Stop-App
$app = Join-Path $WorkDir 'app'
New-AppDir $app @('[collector]', 'enabled = true')
Remove-Item (Join-Path $WorkDir 'focusflow.log') -ErrorAction SilentlyContinue
$backupDir = Join-Path $app 'backup'
$backupBefore = @(Get-ChildItem $backupDir -ErrorAction SilentlyContinue).Count
"exe : $Exe"
"app : $app  (collector on, burst=$Burst)"

$p = Start-App $Exe $app $WorkDir
$h = Wait-Window $p.Id
Check 'main window appears' ($h -ne [IntPtr]::Zero)
if ($h -eq [IntPtr]::Zero) { Stop-App; Show-Summary }

$st = Wait-Status $WorkDir 8000
Check 'collector reports on' ($null -ne $st -and $st.Collector -eq 'on') "today=$($st.Today) cpm=$($st.Cpm)"
$t0 = if ($st) { $st.Today } else { 0 }

# --- counting -------------------------------------------------------------------
[FF]::ClickScreen($h, 1150, 700, $Burst)
Start-Sleep -Milliseconds 2600
$s1 = Wait-Status $WorkDir
Check "+$Burst clicks are counted" ($s1.Today - $t0 -eq $Burst) "today $t0 -> $($s1.Today)"
Check 'speed (cpm) is non-zero right after typing' ($s1.Cpm -gt 0) "cpm=$($s1.Cpm)"
Save-WindowShot $h (Join-Path $WorkDir 'collect-01-counted.png') | Out-Null

# --- pause ----------------------------------------------------------------------
[void][FF]::PostMessage($h, 0x8008, [IntPtr]::Zero, [IntPtr]::Zero)   # WM_APP+8 (acceptance)
Start-Sleep -Milliseconds 900
$s2 = Wait-Status $WorkDir
Check 'pause is reported' $s2.Paused "paused=$($s2.Paused)"
[FF]::ClickScreen($h, 1150, 700, $Burst)
Start-Sleep -Milliseconds 2600
$s3 = Wait-Status $WorkDir
Check 'nothing is counted while paused' ($s3.Today -eq $s2.Today) "today $($s2.Today) -> $($s3.Today)"
Check 'speed drops to 0 while paused' ($s3.Cpm -eq 0) "cpm=$($s3.Cpm)"
Save-WindowShot $h (Join-Path $WorkDir 'collect-02-paused.png') | Out-Null

# --- resume ---------------------------------------------------------------------
[void][FF]::PostMessage($h, 0x8008, [IntPtr]::Zero, [IntPtr]::Zero)
Start-Sleep -Milliseconds 900
[FF]::ClickScreen($h, 1150, 700, $Burst)
Start-Sleep -Milliseconds 2600
$s4 = Wait-Status $WorkDir
Check "resume counts again (+$Burst)" ($s4.Today - $s3.Today -eq $Burst) "today $($s3.Today) -> $($s4.Today)"
Check 'resume is reported' (-not $s4.Paused) "paused=$($s4.Paused)"
Save-WindowShot $h (Join-Path $WorkDir 'collect-03-resumed.png') | Out-Null

# --- quit: flush + exit backup --------------------------------------------------
[void][FF]::PostMessage($h, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)   # WM_APP+9 (acceptance)
$sw = [Diagnostics.Stopwatch]::StartNew()
while (-not $p.HasExited -and $sw.Elapsed.TotalMilliseconds -lt 10000) { Start-Sleep -Milliseconds 150 }
Check 'quits within 10 s' $p.HasExited "$([int]$sw.Elapsed.TotalMilliseconds) ms"
if (-not $p.HasExited) { Stop-App }

$log = Read-AppLog $WorkDir
$coreLog = Get-ChildItem (Join-Path $app 'logs\*') -ErrorAction SilentlyContinue | Select-Object -First 1
Check 'core log file was created' ($null -ne $coreLog -and $coreLog.Length -gt 0) $(if ($coreLog) { "$($coreLog.Length) bytes" } else { 'missing' })
$coreClosed = $false
if ($coreLog) {
    $coreClosed = (@(Get-Content $coreLog.FullName -Encoding UTF8) -match (Get-CoreClosedWord)).Count -gt 0
}
Check 'core log says the database was closed' $coreClosed ''
$backupAfter = @(Get-ChildItem $backupDir -ErrorAction SilentlyContinue).Count
Check 'exit backup wrote files' ($backupAfter -gt $backupBefore) "files $backupBefore -> $backupAfter"
Check 'no error lines in the app log' (-not (@($log) -match "(?i)error|$(Get-FailWord)")) ''

Show-Summary
