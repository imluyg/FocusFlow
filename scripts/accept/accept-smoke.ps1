<#
.SYNOPSIS
  Fast sanity check after any change: window, floating pill, memory budget, log, clean quit.

.DESCRIPTION
  Runs the release exe against a fresh COPY of desktop-native\probe, so nothing touches
  real data. Prints PASS/FAIL lines; exits 1 if anything failed.

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-smoke.ps1
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-smoke.ps1 -Collector
#>
param(
    [string]$Exe = '',
    [string]$WorkDir = '',
    [switch]$Collector
)

. "$PSScriptRoot\accept-lib.ps1"

if (-not $Exe) { $Exe = Get-DefaultExe }
if (-not $WorkDir) { $WorkDir = Join-Path (Get-RepoRoot) 'target\accept\smoke' }
$collectorOn = if ($Collector) { 'true' } else { 'false' }

Stop-App
$app = Join-Path $WorkDir 'app'
New-AppDir $app @('[collector]', "enabled = $collectorOn")
Remove-Item (Join-Path $WorkDir 'focusflow-native.log') -ErrorAction SilentlyContinue
"exe : $Exe"
"app : $app"

$p = Start-App $Exe $app $WorkDir
$h = Wait-Window $p.Id
Check 'main window appears' ($h -ne [IntPtr]::Zero)
if ($h -eq [IntPtr]::Zero) { Stop-App; Show-Summary }

$pill = Wait-Window $p.Id 'FocusFlowFloatingPill' 6000
Check 'floating pill exists' ($pill -ne [IntPtr]::Zero)
Check 'pill answers HTCAPTION (whole card drags)' ([FF]::HitTest([uint32]$p.Id, 'FocusFlowFloatingPill') -eq 2)

Start-Sleep -Milliseconds 3000
$r = Get-Rect $h
Check 'window is 1200x860' ($r.W -eq 1200 -and $r.H -eq 860) "$($r.W)x$($r.H)"

$mem = Get-Memory $p
Check 'memory under the 120 MB budget' ($mem.WsMb -le 120) "$($mem.WsMb) MB WS / $($mem.PrivateMb) MB private / $($mem.Threads) threads"

$shot = Join-Path $WorkDir 'smoke.png'
$ok = Save-WindowShot $h $shot
Check 'PrintWindow captured the UI' ([bool]$ok -and (Test-Path $shot)) $shot

$st = Wait-Status $WorkDir
Check 'acceptance status line exists' ($null -ne $st)
if ($st) {
    $want = if ($Collector) { 'on' } else { 'off' }
    Check "collector reports '$want'" ($st.Collector -eq $want) "today=$($st.Today) cpm=$($st.Cpm)"
}

$log = Read-AppLog $WorkDir
Check 'log has no error lines' (-not (@($log) -match "(?i)error|$(Get-FailWord)")) ''

# Same code path as the tray "quit": flush + exit backup + destroy window.
[void][FF]::PostMessage($h, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw = [Diagnostics.Stopwatch]::StartNew()
while (-not $p.HasExited -and $sw.Elapsed.TotalMilliseconds -lt 8000) { Start-Sleep -Milliseconds 150 }
Check 'quits within 8 s' $p.HasExited "$([int]$sw.Elapsed.TotalMilliseconds) ms"
if (-not $p.HasExited) { Stop-App }

Show-Summary
