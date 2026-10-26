<#
.SYNOPSIS
  The two snip paths: the snip hotkey saves on mouse-up, the snip_annotate hotkey opens the
  annotation window instead -- and only the first one writes a file immediately.

.DESCRIPTION
  Runs against a fresh COPY of desktop-native\probe (nothing touches real data).

  The app hook FOCUSFLOW_SNIP_OVERLAY opens the full-screen selection overlay 1.5 s after
  start: any value = the quick path (save on mouse-up), the value "annotate" = open the
  annotation editor after the selection. Injected *mouse* events do reach the app, so the
  harness really drags a rubber band and then looks at what changed on disk / which window
  exists. That makes this the end-to-end check for the selection -> save branch.

  Screenshots land in <app>\data\screenshots, so the PNG count is the observable.

  ASCII-only, like every file in this folder.

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-snip.ps1
#>
param(
    [string]$Exe = '',
    [string]$WorkDir = ''
)

. "$PSScriptRoot\accept-lib.ps1"

if (-not $Exe) { $Exe = Get-DefaultExe }
if (-not $WorkDir) { $WorkDir = Join-Path (Get-RepoRoot) 'target\accept\snip' }

$app = Join-Path $WorkDir 'app'
$logPath = Join-Path $WorkDir 'focusflow.log'
$sdir = Join-Path $app 'data\screenshots'

function Count-Png {
    if (-not (Test-Path $sdir)) { return 0 }
    @(Get-ChildItem $sdir -Filter '*.png' -File -ErrorAction SilentlyContinue).Count
}

function Wait-Png([int]$Before, [int]$TimeoutMs = 6000) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalMilliseconds -lt $TimeoutMs) {
        if ((Count-Png) -gt $Before) { return $true }
        Start-Sleep -Milliseconds 200
    }
    $false
}

function Get-SnipLog {
    if (Test-Path $logPath) { Get-Content $logPath -Encoding UTF8 } else { @() }
}

function Stop-Round([IntPtr]$Hwnd, $Proc) {
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
"app : $app"

# ---------- round 1: quick path (the snip hotkey) ----------
$before = Count-Png
Remove-Item $logPath -ErrorAction SilentlyContinue
$p = Start-App $Exe $app $WorkDir @{ FOCUSFLOW_SNIP_OVERLAY = '1' }
$h = Wait-Window $p.Id
Check 'main window appears' ($h -ne [IntPtr]::Zero)
if ($h -eq [IntPtr]::Zero) { Stop-App; Show-Summary }
$ov = Wait-Window $p.Id 'FocusFlowSnipOverlay' 6000
Check 'the selection overlay opens' ($ov -ne [IntPtr]::Zero)

Drag-Screen 240 200 640 470
Check 'the quick path saved a PNG' (Wait-Png $before) "png $before -> $(Count-Png)"
$quickHits = @(Get-SnipLog | Where-Object { $_ -match 'snip-quick' })
Check 'the log names the quick path' ($quickHits.Count -gt 0)
$ed = [FF]::ByClass([uint32]$p.Id, 'FocusFlowSnipEditor')
Check 'the quick path does not open the editor' ($ed -eq [IntPtr]::Zero)
Stop-Round $h $p

# ---------- round 2: annotate path (the snip_annotate hotkey) ----------
$before2 = Count-Png
Remove-Item $logPath -ErrorAction SilentlyContinue
$p2 = Start-App $Exe $app $WorkDir @{ FOCUSFLOW_SNIP_OVERLAY = 'annotate' }
$h2 = Wait-Window $p2.Id
if ($h2 -eq [IntPtr]::Zero) { Stop-App; Show-Summary }
$ov2 = Wait-Window $p2.Id 'FocusFlowSnipOverlay' 6000
Check 'the annotate round opens the overlay too' ($ov2 -ne [IntPtr]::Zero)

Drag-Screen 260 220 660 490
$ed2 = Wait-Window $p2.Id 'FocusFlowSnipEditor' 6000
Check 'the annotate path opens the editor window' ($ed2 -ne [IntPtr]::Zero)
Check 'the annotate path has not written a file yet' ((Count-Png) -eq $before2) "png=$(Count-Png)"

# Esc inside the editor = give up; nothing should be saved by that.
if ($ed2 -ne [IntPtr]::Zero) {
    [void][FF]::PostMessage($ed2, 0x0100, [IntPtr]0x1B, [IntPtr]::Zero)   # WM_KEYDOWN VK_ESCAPE
    Start-Sleep -Milliseconds 800
    $still = [FF]::ByClass([uint32]$p2.Id, 'FocusFlowSnipEditor')
    Check 'Esc closes the editor' ($still -eq [IntPtr]::Zero)
    Check 'giving up saved nothing' ((Count-Png) -eq $before2) "png=$(Count-Png)"
}
Stop-Round $h2 $p2

Show-Summary
