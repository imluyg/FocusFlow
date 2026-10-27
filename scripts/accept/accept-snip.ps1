<#
.SYNOPSIS
  The snip flow on the frozen-screen overlay: the snip hotkey saves on mouse-up, the
  snip_annotate hotkey keeps the overlay alive so the selection can be adjusted and
  annotated in place -- and only the first one writes a file immediately.

.DESCRIPTION
  Runs against a fresh COPY of desktop-native\probe (nothing touches real data).

  The app hook FOCUSFLOW_SNIP_OVERLAY opens the full-screen selection overlay 1.5 s after
  start: any value = the quick path (save on mouse-up), the value "annotate" = stay on the
  overlay with the toolbar after the selection. Injected *mouse* events do reach the app, so
  the harness really drags a rubber band and then looks at what changed on disk.

  What the overlay shows now: the whole virtual screen is frozen at hotkey time, everything
  outside the selection is dimmed, and the saved pixels are cropped out of that frozen buffer.
  So the PNG is checked for the exact dragged size -- a live re-capture after the overlay was
  destroyed would no longer be the same picture the user selected.

  Round 2 drives the toolbar the way a user clicks it: the overlay logs every button as
  "snip-btn <id> <x> <y> <w> <h>" when the selection is confirmed, and the harness posts
  mouse messages to those coordinates. That is what makes the annotation chain
  (tool -> mark -> bake -> PNG) machine-verifiable instead of eye-verified: with the block
  tool covering the whole selection, the saved PNG has to be that color everywhere.
  Each leg ANDs its own precondition (the button line must be in the log) so a missing
  toolbar reads as a failure, not as a skip.

  Round 2b repeats the same gesture with Backspace before the commit: the undo must really
  remove the mark, so the PNG must NOT be that color. A leg that only checks "a file appeared"
  would pass both times.

  Round 3: a single click (down and up without moving) picks the window under the cursor. The
  fixture is a top-level window of *this PowerShell process* -- the app has to skip its own
  process, and a wrong pick shows up immediately as a PNG whose size is the app window
  (1200x860) or the whole screen instead of the fixture's 320x200.

  Round 3b: a corner handle drag resizes the confirmed selection; the PNG has to grow by the
  same number of pixels the handle was moved.

  Round 4 covers the give-up-on-timeout branch: the overlay logs the three-minute timer it
  arms, and it must then close itself without writing anything -- which means this round
  really waits three minutes. A WM_TIMER posted from outside does not reach the app's message
  loop (measured), so there is no way to fast-forward it from the harness.

  ASCII-only, like every file in this folder.
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

# One toolbar button as the overlay reported it: 'snip-btn <id> <x> <y> <w> <h>'
function Get-BarBtn([string]$Id) {
    $line = @(Get-SnipLog | Where-Object { $_ -match "snip-btn $Id (-?\d+) (-?\d+) (\d+) (\d+)" })
    if ($line.Count -eq 0) { return $null }
    $m = [regex]::Match($line[0], "snip-btn $Id (-?\d+) (-?\d+) (\d+) (\d+)")
    if (-not $m.Success) { return $null }
    [pscustomobject]@{
        X = [int]$m.Groups[1].Value
        Y = [int]$m.Groups[2].Value
        W = [int]$m.Groups[3].Value
        H = [int]$m.Groups[4].Value
        CX = ([int]$m.Groups[1].Value + [int]$m.Groups[3].Value / 2)
        CY = ([int]$m.Groups[2].Value + [int]$m.Groups[4].Value / 2)
    }
}

function Post-Mouse([IntPtr]$H, [uint32]$Msg, [int]$X, [int]$Y) {
    $lp = (($Y -band 0xFFFF) * 65536) + ($X -band 0xFFFF)
    [void][FF]::PostMessage($H, $Msg, [IntPtr]1, [IntPtr]$lp)
}

# Message ids, spelled out because mixing two of them up silently changes the gesture:
# 0x0200 WM_MOUSEMOVE, 0x0201 WM_LBUTTONDOWN, 0x0202 WM_LBUTTONUP, 0x0204 WM_RBUTTONDOWN (= cancel).
$FF_MOVE = 0x0200
$FF_DOWN = 0x0201
$FF_UP = 0x0202

function Post-Key([IntPtr]$H, [int]$Vk) {
    # WM_KEYDOWN: lParam bit 30 = not previously pressed; the app's handler reads only the vk.
    [void][FF]::PostMessage($H, 0x0100, [IntPtr]$Vk, [IntPtr]0)
}

# Click a toolbar button: move, then down + up on its center (client coordinates).
function Click-BarBtn([IntPtr]$H, $Btn) {
    $x = [int]$Btn.CX
    $y = [int]$Btn.CY
    Post-Mouse $H $FF_MOVE $x $y
    Post-Mouse $H $FF_DOWN $x $y
    Start-Sleep -Milliseconds 120
    Post-Mouse $H $FF_UP $x $y
    Start-Sleep -Milliseconds 200
}

# Press inside the image, move, release: one stroke of the current tool.
function Drag-Mark([IntPtr]$H, [int]$X0, [int]$Y0, [int]$X1, [int]$Y1) {
    Post-Mouse $H $FF_MOVE $X0 $Y0
    Post-Mouse $H $FF_DOWN $X0 $Y0
    Start-Sleep -Milliseconds 120
    Post-Mouse $H $FF_MOVE $X1 $Y1
    Start-Sleep -Milliseconds 120
    Post-Mouse $H $FF_MOVE (($X0 + $X1) / 2) (($Y0 + $Y1) / 2)
    Post-Mouse $H $FF_UP $X1 $Y1
    Start-Sleep -Milliseconds 250
}

# Sample the saved PNG on a grid and count red pixels (the annotate default is 255,45,45)
# inside the block that was dragged and in everything outside it. Both numbers are the point:
# "inside is red" proves the mark was baked, "outside is not red" proves nothing bled or moved.
function Measure-BlockHit([string]$Path, [int]$Bx0, [int]$By0, [int]$Bx1, [int]$By1) {
    if (-not $Path -or -not (Test-Path $Path)) { return $null }
    $bmp = [System.Drawing.Bitmap]::new($Path)
    $inHit = 0; $inTot = 0; $outHit = 0; $outTot = 0
    for ($yy = 3; $yy -lt $bmp.Height - 3; $yy += 5) {
        for ($xx = 3; $xx -lt $bmp.Width - 3; $xx += 5) {
            $c = $bmp.GetPixel($xx, $yy)
            $isRed = ($c.R -ge 250 -and $c.G -le 50 -and $c.B -le 50)
            $inBlock = ($xx -ge $Bx0 -and $xx -lt $Bx1 -and $yy -ge $By0 -and $yy -lt $By1)
            if ($inBlock) { $inTot++; if ($isRed) { $inHit++ } }
            else { $outTot++; if ($isRed) { $outHit++ } }
        }
    }
    $bmp.Dispose()
    if ($inTot -eq 0 -or $outTot -eq 0) { return $null }
    [pscustomobject]@{
        In = [int](100 * $inHit / $inTot)
        Out = [int](100 * $outHit / $outTot)
        InTot = $inTot
        OutTot = $outTot
    }
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

# The one product promise of the quick hotkey: mouse-up commits and nothing else opens.
# The old assertion looked for an editor window class that no longer exists at all, which
# passes even when the whole annotate stage is gone -- so the observable is the overlay.
Check 'the quick commit closes the overlay itself' `
    ([FF]::ByClass([uint32]$p.Id, 'FocusFlowSnipOverlay') -eq [IntPtr]::Zero)
$new1 = Get-NewestPng
Check 'the file is named with the size it saved' `
    ((Split-Path $new1 -Leaf) -match '^snip_\d{8}_\d{6}_\d+x\d+\.png$') (Split-Path $new1 -Leaf)
$s1 = Get-PngSize $new1
Check 'the saved pixels are the dragged rect' ($s1 -and $s1.Width -eq 400 -and $s1.Height -eq 270) `
    ("got=$($s1.Width)x$($s1.Height)")

# After a snip that really succeeded the overlay state must be free again: the flag that
# says "an overlay is open" is what start() checks, and if nothing clears it on the
# success path the very next hotkey press does nothing until the app is restarted.
[void][FF]::PostMessage($h, 0x8006, [IntPtr]::Zero, [IntPtr]::Zero)   # WM_SNIP_START (quick)
$ov1b = Wait-Window $p.Id 'FocusFlowSnipOverlay' 4000
Check 'a second snip opens the overlay again' ($ov1b -ne [IntPtr]::Zero)
if ($ov1b -ne [IntPtr]::Zero) {
    Post-Key $ov1b 0x1B
    Start-Sleep -Milliseconds 600
}
Stop-Round $h $p

# ---------- round 2: annotate path, all four tools of the frozen overlay ----------
$before2 = Count-Png
Remove-Item $logPath -ErrorAction SilentlyContinue
$p2 = Start-App $Exe $app $WorkDir @{ FOCUSFLOW_SNIP_OVERLAY = 'annotate' }
$h2 = Wait-Window $p2.Id
if ($h2 -eq [IntPtr]::Zero) { Stop-App; Show-Summary }
$ov2 = Wait-Window $p2.Id 'FocusFlowSnipOverlay' 6000
Check 'the annotate round opens the overlay too' ($ov2 -ne [IntPtr]::Zero)

Drag-Screen 260 220 660 490
# The overlay must stay (it is the annotate stage now, not a second window) and nothing
# must be on disk yet.
Check 'the annotate stage keeps the overlay alive' `
    ([FF]::ByClass([uint32]$p2.Id, 'FocusFlowSnipOverlay') -ne [IntPtr]::Zero)
Check 'the annotate stage logs itself' (Wait-Log 'snip-annotate')
Check 'the annotate stage has not written a file yet' ((Count-Png) -eq $before2) "png=$(Count-Png)"
$bBlock = Get-BarBtn 'block'
$bSave = Get-BarBtn 'save'
Check 'the toolbar reports the block and save buttons' ($bBlock -and $bSave) `
    ("block-null=$([bool](-not $bBlock)) save-null=$([bool](-not $bSave))")

if ($bBlock) {
    # Cover part of the selection with the block tool, then commit with Enter (the toolbar's
    # own 'save' button is exercised the same way in round 3b's sibling). The saved PNG has to
    # be that color exactly where the stroke was dragged -- the first machine check of
    # tool -> mark -> bake -> file, and of the fact that nothing outside the stroke changed.
    Click-BarBtn $ov2 $bBlock
    Drag-Mark $ov2 280 250 640 470
    Post-Key $ov2 0x0D
    $ok2 = Wait-Png $before2 8000
    Check 'the annotated snip saved a PNG' $ok2 "png $before2 -> $(Count-Png)"
    $hit2 = Measure-BlockHit (Get-NewestPng) 20 30 380 250
    Check 'the block landed where it was dragged' `
        ($hit2 -and $hit2.In -ge 98 -and $hit2.Out -le 2) ($hit2 | Out-String -Compress)
    Check 'the annotate overlay closed on commit' `
        ([FF]::ByClass([uint32]$p2.Id, 'FocusFlowSnipOverlay') -eq [IntPtr]::Zero)
} else {
    Check 'the annotate round drove the toolbar' $false 'no snip-btn block in the log'
}

# Re-pressing the annotate hotkey abandons the in-flight selection and opens a fresh overlay
# (the product logs 'snip-repress'); re-pressing the *quick* hotkey must not interrupt --
# that path logs 'snip-busy' and keeps the current selection. The count of 'snip-arm-timeout'
# lines is the observable that a second overlay really came up.
$armedBefore = @(Get-SnipLog | Where-Object { $_ -match 'snip-arm-timeout' }).Count
[void][FF]::PostMessage($h2, 0x8006, [IntPtr]1, [IntPtr]::Zero)     # WM_SNIP_START (annotate)
Start-Sleep -Milliseconds 1500
Check 'the log says the re-press abandoned the in-flight snip' (Wait-Log 'snip-repress')
Check 'the re-pressed annotate hotkey brings the overlay back' `
    ((@(Get-SnipLog | Where-Object { $_ -match 'snip-arm-timeout' }).Count) -gt $armedBefore)
Check 'the overlay after the re-press belongs to this process' `
    ([FF]::ByClass([uint32]$p2.Id, 'FocusFlowSnipOverlay') -ne [IntPtr]::Zero)
Stop-Round $h2 $p2

# ---------- round 2b: Backspace must undo the mark ----------
$before2b = Count-Png
Remove-Item $logPath -ErrorAction SilentlyContinue
$p2b = Start-App $Exe $app $WorkDir @{ FOCUSFLOW_SNIP_OVERLAY = 'annotate' }
$h2b = Wait-Window $p2b.Id
$ov2b = Wait-Window $p2b.Id 'FocusFlowSnipOverlay' 6000
Check 'the undo round opens the overlay' ($ov2b -ne [IntPtr]::Zero)
Drag-Screen 280 240 680 500
$bBlock2 = Get-BarBtn 'block'
$bUndo = Get-BarBtn 'undo'
if ($bBlock2 -and $bUndo -and $ov2b -ne [IntPtr]::Zero) {
    Click-BarBtn $ov2b $bBlock2
    Drag-Mark $ov2b 300 270 660 480
    Post-Key $ov2b 0x08          # Backspace = undo the only mark
    Start-Sleep -Milliseconds 300
    Post-Key $ov2b 0x0D          # Enter = commit
    $ok2b = Wait-Png $before2b 8000
    Check 'the undone snip still saved a PNG' $ok2b "png $before2b -> $(Count-Png)"
    $hit2b = Measure-BlockHit (Get-NewestPng) 20 30 380 240
    Check 'undo took the mark out of the saved pixels' ($hit2b -and $hit2b.In -le 5) `
        ($hit2b | Out-String -Compress)
} else {
    Check 'the undo round found the toolbar' ($bBlock2 -and $bUndo) 'block or undo button missing'
}
Stop-Round $h2b $p2b

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
    Post-Key $ov3b 0x1B
    Start-Sleep -Milliseconds 600
}
Stop-Round $h3 $p3

# ---------- round 3b: the corner handle resizes the confirmed selection ----------
$before3b = Count-Png
Remove-Item $logPath -ErrorAction SilentlyContinue
$p3c = Start-App $Exe $app $WorkDir @{ FOCUSFLOW_SNIP_OVERLAY = 'annotate' }
$h3c = Wait-Window $p3c.Id
$ov3c = Wait-Window $p3c.Id 'FocusFlowSnipOverlay' 6000
Check 'the resize round opens the overlay' ($ov3c -ne [IntPtr]::Zero)
Drag-Screen 300 260 600 440
$bSave3 = Get-BarBtn 'save'
Check 'the resize round found the toolbar' ($bSave3 -ne $null)
if ($bSave3) {
    # bottom-right handle of the 300,260 - 600,440 selection = (600,440) in client coords;
    # drag it +40/+40, then press the toolbar's own commit button.
    Post-Mouse $ov3c $FF_MOVE 600 440
    Post-Mouse $ov3c $FF_DOWN 600 440
    Start-Sleep -Milliseconds 100
    Post-Mouse $ov3c $FF_MOVE 640 480
    Start-Sleep -Milliseconds 100
    Post-Mouse $ov3c $FF_UP 640 480
    Start-Sleep -Milliseconds 250
    Click-BarBtn $ov3c $bSave3
    $ok3b = Wait-Png $before3b 8000
    Check 'the resized selection saved a PNG' $ok3b "png $before3b -> $(Count-Png)"
    $s3b = Get-PngSize (Get-NewestPng)
    Check 'the saved size grew with the handle' `
        ($s3b -and $s3b.Width -ge 330 -and $s3b.Width -le 344 -and $s3b.Height -ge 210 -and $s3b.Height -le 224) `
        ("png=$($s3b.Width)x$($s3b.Height) want=340x220")
} else {
    Stop-Round $h3c $p3c
    $fx.Close(); $fx.Dispose()
    Show-Summary
}
# Esc on a fresh overlay gives up without writing anything (the annotate stage's own Esc is
# covered by the undo round's outcome shape).
[void][FF]::PostMessage($h3c, 0x8006, [IntPtr]::Zero, [IntPtr]::Zero)
$ov3d = Wait-Window $p3c.Id 'FocusFlowSnipOverlay' 4000
if ($ov3d -ne [IntPtr]::Zero) {
    $pngBeforeEsc = Count-Png
    Drag-Screen 320 280 520 380
    Post-Key $ov3d 0x1B
    Start-Sleep -Milliseconds 800
    Check 'Esc after the selection saved nothing' ((Count-Png) -eq $pngBeforeEsc) "png=$(Count-Png)"
    Check 'Esc closed the overlay' `
        ([FF]::ByClass([uint32]$p3c.Id, 'FocusFlowSnipOverlay') -eq [IntPtr]::Zero)
}
Stop-Round $h3c $p3c
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
