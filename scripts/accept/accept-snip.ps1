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

  Round 3 adds the third gesture: a single click (down and up without moving) picks the
  window under the cursor. The fixture is a top-level window of *this PowerShell process* --
  the app has to skip its own process, and a wrong pick shows up immediately as a PNG whose
  size is the app window (1200x860) or the whole screen instead of the fixture's 320x200.
  The same round then asks for a second overlay from the same process: the state flag that
  says "an overlay is open" must be free again once one finished.

  Round 4 covers the give-up-on-timeout branch: the overlay logs the three-minute timer it
  arms, and it must then close itself without writing anything -- which means this round
  really waits three minutes. A WM_TIMER posted from outside does not reach the app's message
  loop (measured), so there is no way to fast-forward it from the harness.

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

function Get-NewestPng {
    if (-not (Test-Path $sdir)) { return '' }
    $f = @(Get-ChildItem $sdir -Filter '*.png' -File | Sort-Object LastWriteTime -Descending)
    if ($f.Count -gt 0) { $f[0].FullName } else { '' }
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

# Poll the log for a line: the overlay window exists before the lines it logs reach the file,
# so reading once right after Wait-Window is a race the harness used to lose.
function Wait-Log([string]$Pattern, [int]$TimeoutMs = 4000) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalMilliseconds -lt $TimeoutMs) {
        if (@(Get-SnipLog | Where-Object { $_ -match $Pattern }).Count -gt 0) { return $true }
        Start-Sleep -Milliseconds 200
    }
    $false
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

# After a snip that really succeeded the overlay state must be free again: the flag that
# says "an overlay is open" is what start_snip checks, and if nothing clears it on the
# success path the very next hotkey press does nothing until the app is restarted.
[void][FF]::PostMessage($h, 0x8006, [IntPtr]::Zero, [IntPtr]::Zero)   # WM_SNIP_START (quick)
$ov1b = Wait-Window $p.Id 'FocusFlowSnipOverlay' 4000
Check 'a second snip opens the overlay again' ($ov1b -ne [IntPtr]::Zero)
if ($ov1b -ne [IntPtr]::Zero) {
    [void][FF]::PostMessage($ov1b, 0x0100, [IntPtr]0x1B, [IntPtr]::Zero)   # Esc = give up
    Start-Sleep -Milliseconds 600
}
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

# ---------- round 3: a single click picks the window under the cursor ----------
# A foreign top-level window (this PowerShell process, not the app) is the fixture: the app
# must skip its OWN process, so if the exclusion broke the PNG would come out 1200x860 (the
# app window) or full screen -- both are far off this rect.
Add-Type -AssemblyName System.Windows.Forms
$fx = New-Object System.Windows.Forms.Form
$fx.Text = 'FFixtureWindow'
$fx.FormBorderStyle = 'FixedDialog'
$fx.MaximizeBox = $false
$fx.MinimizeBox = $false
$fx.StartPosition = 'Manual'
$fx.Location = New-Object System.Drawing.Point(900, 300)
$fx.Size = New-Object System.Drawing.Size(320, 200)
$fx.TopMost = $true
$fx.Show()
[void]$fx.Activate()
[System.Windows.Forms.Application]::DoEvents()
$fxRect = Get-Rect $fx.Handle

$before3 = Count-Png
Remove-Item $logPath -ErrorAction SilentlyContinue
$p3 = Start-App $Exe $app $WorkDir @{ FOCUSFLOW_SNIP_OVERLAY = '1' }
$h3 = Wait-Window $p3.Id
if ($h3 -eq [IntPtr]::Zero) { Stop-App; Show-Summary }
$ov3 = Wait-Window $p3.Id 'FocusFlowSnipOverlay' 6000
Check 'the click round opens the overlay' ($ov3 -ne [IntPtr]::Zero)

$cx = $fxRect.X + [int]($fxRect.W / 2)
$cy = $fxRect.Y + [int]($fxRect.H / 2)
Click-Screen $cx $cy
Check 'the single click saved a PNG' (Wait-Png $before3) "png $before3 -> $(Count-Png)"
$s3 = Get-PngSize (Get-NewestPng)
$okSize = $s3 -and $s3.Width -le $fxRect.W -and $s3.Width -ge ($fxRect.W - 24) `
    -and $s3.Height -le $fxRect.H -and $s3.Height -ge ($fxRect.H - 24)
Check 'the picked rect is that window, not the screen or our own' ([bool]$okSize) `
    ("png=$($s3.Width)x$($s3.Height) fixture=$($fxRect.W)x$($fxRect.H)")
Check 'the log names the window pick' (@(Get-SnipLog | Where-Object { $_ -match 'snip-pick' }).Count -gt 0)

# Same process, second gesture: the first snip succeeded, so the overlay state must be free
# again. Before this branch existed the run-once flag stayed set and the hotkey did nothing.
[void][FF]::PostMessage($h3, 0x8006, [IntPtr]::Zero, [IntPtr]::Zero)   # WM_SNIP_START (quick)
$ov3b = Wait-Window $p3.Id 'FocusFlowSnipOverlay' 4000
Check 'a second snip opens the overlay again' ($ov3b -ne [IntPtr]::Zero)
if ($ov3b -ne [IntPtr]::Zero) {
    [void][FF]::PostMessage($ov3b, 0x0100, [IntPtr]0x1B, [IntPtr]::Zero)   # Esc = give up
    Start-Sleep -Milliseconds 600
}
Stop-Round $h3 $p3
$fx.Close()
$fx.Dispose()

# ---------- round 4: the overlay gives up on its own ----------
$before4 = Count-Png
Remove-Item $logPath -ErrorAction SilentlyContinue
$p4 = Start-App $Exe $app $WorkDir @{ FOCUSFLOW_SNIP_OVERLAY = '1' }
$h4 = Wait-Window $p4.Id
if ($h4 -eq [IntPtr]::Zero) { Stop-App; Show-Summary }
$ov4 = Wait-Window $p4.Id 'FocusFlowSnipOverlay' 6000
Check 'the timeout round opens the overlay' ($ov4 -ne [IntPtr]::Zero)
Check 'the overlay arms a three-minute timeout' (Wait-Log 'snip-arm-timeout 180000')

# Waited out, not faked: a WM_TIMER posted from another process never reaches this message
# loop (measured -- posting WM_KEYDOWN/VK_ESCAPE to the same overlay closes it at once, while
# posting WM_TIMER with the product's own timer id does nothing). So the only honest check of
# "gives up after three minutes" is the real three minutes. This round is slow on purpose.
$gone = $false
$sw4 = [Diagnostics.Stopwatch]::StartNew()
while ($sw4.Elapsed.TotalSeconds -lt 210) {
    if ([FF]::ByClass([uint32]$p4.Id, 'FocusFlowSnipOverlay') -eq [IntPtr]::Zero) { $gone = $true; break }
    Start-Sleep -Milliseconds 1000
}
$waited = [int]$sw4.Elapsed.TotalSeconds
# "the window is gone" is not what this leg's name promises. The overlay is full-screen, so a
# click that lands on it destroys it through the product's own save path -- and the old shape
# read that as a pass (measured: overlay gone at 69 s, one extra PNG, no snip-timeout line,
# while this leg stayed green). The leg now has to match its own claim: it took the minutes.
Check 'the overlay gives up by itself after three minutes' ($gone -and ($waited -ge 170)) "gone=$gone waited $waited s"
Check 'the log names the timeout' (Wait-Log 'snip-timeout' 2000)
Check 'the timeout saved nothing' ((Count-Png) -eq $before4) "png=$(Count-Png)"
Stop-Round $h4 $p4

Show-Summary
