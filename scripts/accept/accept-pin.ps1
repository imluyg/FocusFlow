<#
.SYNOPSIS
  Pin windows: they open on top of the desktop, the count is what the settings row shows,
  and closing one / closing all never writes another file.

.DESCRIPTION
  Runs against a fresh COPY of desktop-native\probe (nothing touches real data).

  Two acceptance messages drive it, because a real pin is created by dragging a snip and
  clicking a toolbar button -- the geometry of that is an eye-check:
    WM_APP+12 (LPARAM = N)  open N synthetic pins   (same open_pin the toolbar calls)
    WM_APP+13               close all pins          (same close_all_pins the row calls)

  The load-bearing assertion is the file count: a pin must never add a PNG, because the
  snip that fed it already saved one. That is the "pin is one exit of the snip, not a
  second save path" rule.

  Round 2 drops the synthetic shortcut and pins a real selection (drag -> annotate -> P),
  which is how two items from the eye-check list became measurable here: where the pin lands
  (it must sit exactly on the selection's rect) and what the twelve-pin cap costs this
  process (printed as an info line, not asserted -- the number belongs in the handoff doc).
  It also pushes one past the cap and requires the refusal to be said out loud.

  Round 1 drives every way out of a pin that a machine can drive, because the migration left
  the old list changed: the WebView build closed a pin with the title-bar X, Esc or Alt+F4,
  and a borderless popup has no X at all -- so the right click is the exit that replaced it,
  and both halves are checked (the style bits say there is no caption, the injected click says
  the replacement works). The wheel legs are the "which way does it grow" check: one notch up
  must be 132x88 for a 120x80 pin, and twenty notches down must stop at a quarter. An injected
  wheel travels to the foreground window's thread rather than to the window under the cursor,
  so those legs take the foreground first and refuse to score the gesture when the control
  says the pin did not get it -- see README pitfall 8.

  ASCII-only, like every file in this folder.

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-pin.ps1
#>
param(
    [string]$Exe = '',
    [string]$WorkDir = ''
)

. "$PSScriptRoot\accept-lib.ps1"

if (-not $Exe) { $Exe = Get-DefaultExe }
if (-not $WorkDir) { $WorkDir = Join-Path (Get-RepoRoot) 'target\accept\pin' }

# WM_APP is 0x8000 (0x400 is WM_USER) -- see main.rs const WM_PIN_TEST: u32 = WM_APP + 12.
$WM_PIN_TEST = 0x8000 + 12
$WM_PIN_CLOSE_ALL = 0x8000 + 13

$app = Join-Path $WorkDir 'app'
$logPath = Join-Path $WorkDir 'focusflow.log'
$sdir = Join-Path $app 'data\screenshots'

function Count-Pins([int]$ProcessId) {
    @([FF]::TopWindows([uint32]$ProcessId) | Where-Object { $_ -like 'FocusFlowPin *' })
}

function Count-Png {
    if (-not (Test-Path $sdir)) { return 0 }
    @(Get-ChildItem $sdir -Filter '*.png' -File -ErrorAction SilentlyContinue).Count
}

function Get-PinLog {
    if (Test-Path $logPath) { Get-Content $logPath -Encoding UTF8 } else { @() }
}

# Ask the app to quit the same way the tray menu does, then wait for it.
function Quit-Round([IntPtr]$Hwnd, $Proc) {
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

$p = Start-App $Exe $app $WorkDir
$h = Wait-Window $p.Id
Check 'main window appears' ($h -ne [IntPtr]::Zero)
if ($h -eq [IntPtr]::Zero) { Stop-App; Show-Summary }

$pngBefore = Count-Png
Check 'no pins at start' ((Count-Pins $p.Id).Count -eq 0)

[void][FF]::PostMessage($h, $WM_PIN_TEST, [IntPtr]::Zero, [IntPtr]3)
Start-Sleep -Milliseconds 1200
$pins = Count-Pins $p.Id
Check 'three pins are on the desktop' ($pins.Count -eq 3) "count=$($pins.Count)"
Check 'each pin has a real size' (@($pins | Where-Object { $_ -match '120x80' }).Count -eq 3) ($pins -join ' | ')
$log = @(Get-PinLog)
Check 'the log counts each pin open' (@($log | Where-Object { $_ -match 'pin-open' }).Count -ge 3)

# The trade the migration doc calls out: a pin is a tool window, so it stays out of Alt+Tab
# (the WebView build put them in it). And because the window is a borderless popup, the old
# title-bar X is not an exit any more -- the next two checks pin both halves of that choice.
$somePin = [FF]::ByClass([uint32]$p.Id, 'FocusFlowPin')
$ex = [FF]::ExStyle($somePin)
Check 'the pin is a tool window, so Alt+Tab never lists it' `
    ((($ex -band 0x80) -ne 0) -and (($ex -band 0x20000) -eq 0)) ("exstyle=0x{0:X}" -f $ex)
$st = [FF]::Style($somePin)
Check 'the pin has no title bar, so the old X exit is gone on purpose' `
    ((($st -band 0x00C00000) -eq 0) -and (($st -band 0x80) -eq 0)) ("style=0x{0:X}" -f $st)

# Dragging is the gesture a "the pin flickers while I move it" report is about. The window
# used to be moved from a CLIENT-space reference that travels with the window itself, so every
# other mouse message landed back near where the drag started -- the picture shakes instead of
# following. Screen space is the one reference that stays put, and the check is the plainest
# one available: a 120x60 drag has to end 120x60 away, not half of it.
$dragPin = [FF]::ByClass([uint32]$p.Id, 'FocusFlowPin')
$d0 = Get-Rect $dragPin
$grabX = $d0.X + [int]($d0.W / 2)
$grabY = $d0.Y + [int]($d0.H / 2)
$onPin = Test-PointIsOurs $dragPin $grabX $grabY
Check 'the drag starts on the pin itself' $onPin
if ($onPin) {
    Drag-Screen $grabX $grabY ($grabX + 120) ($grabY + 60)
    $d1 = Get-Rect $dragPin
    Check 'a 120x60 drag lands the pin 120x60 away' `
        ($d1.X -eq $d0.X + 120 -and $d1.Y -eq $d0.Y + 60) `
        ("was $($d0.X),$($d0.Y) now $($d1.X),$($d1.Y)")
} else {
    # A red, not a skip: the point was taken by somebody else's window, so the gesture went
    # there and this leg measured nothing about the pin.
    Check 'a 120x60 drag lands the pin 120x60 away' $false 'skipped: the grab point is not the pin'
}

# The wheel legs go first, on a pin nothing has disturbed yet. An injected wheel is delivered
# to the foreground window's thread -- NOT to the window under the cursor, the way a real
# wheel is (three-arm probe on this box: a fresh pin zooms; the same pin after another window
# takes the foreground does not; a survivor whose focused sibling closed does). So the pin is
# put in the foreground on purpose and that precondition is checked before the gesture is
# credited, otherwise a red would read as "the zoom is broken".
$wheelPin = [FF]::ByClass([uint32]$p.Id, 'FocusFlowPin')
[void][FF]::SetForegroundWindow($wheelPin)
Start-Sleep -Milliseconds 400
$wheelFg = ([FF]::GetForegroundWindow() -eq $wheelPin)
Check 'the wheel starts on a pin that holds the foreground' $wheelFg
if ($wheelFg) {
    $wr = Get-Rect $wheelPin
    [FF]::Wheel($wr.X + [int]($wr.W / 2), $wr.Y + [int]($wr.H / 2), 1)
    Start-Sleep -Milliseconds 600
    $w1 = Get-Rect $wheelPin
    # up = bigger is the easiest thing in this file to write backwards: 120x80 -> 132x88.
    Check 'one notch up grows the pin by ten percent' ($w1.W -eq 132 -and $w1.H -eq 88) "now $($w1.W)x$($w1.H)"
    # Shrinking keeps pulling the picture away from a fixed pointer (the window grows from
    # its top-left), and a wheel over the desktop is not a wheel over the pin, so each notch
    # re-aims at where the centre is now.
    for ($i = 0; $i -lt 20; $i++) {
        $wc = Get-Rect $wheelPin
        [FF]::Wheel($wc.X + [int]($wc.W / 2), $wc.Y + [int]($wc.H / 2), -1)
        Start-Sleep -Milliseconds 90
    }
    Start-Sleep -Milliseconds 600
    $w2 = Get-Rect $wheelPin
    Check 'twenty notches down stop at a quarter, never at zero' ($w2.W -eq 30 -and $w2.H -eq 20) "now $($w2.W)x$($w2.H)"
} else {
    # A leg that could not run is not a pass: the chord and the wheel would land elsewhere.
    Check 'one notch up grows the pin by ten percent' $false 'skipped: no pin held the foreground'
    Check 'twenty notches down stop at a quarter, never at zero' $false 'skipped: no pin held the foreground'
}

# The rule that matters: opening a pin must not save anything again.
Check 'pins wrote no extra file' ((Count-Png) -eq $pngBefore) "png $pngBefore -> $(Count-Png)"

# Exit 1: Esc closes this one pin.
$firstHwnd = [FF]::ByClass([uint32]$p.Id, 'FocusFlowPin')
[void][FF]::PostMessage($firstHwnd, 0x0100, [IntPtr]0x1B, [IntPtr]::Zero)   # WM_KEYDOWN ESC
Start-Sleep -Milliseconds 800
Check 'Esc closes a single pin' ((Count-Pins $p.Id).Count -eq 2) "count=$((Count-Pins $p.Id).Count)"

# Exit 2: a real right click on the picture -- this is what replaced the title-bar X, so the
# gesture itself has to be injected, not the message behind it.
$topHwnd = [FF]::ByClass([uint32]$p.Id, 'FocusFlowPin')
$tr = Get-Rect $topHwnd
[FF]::RightClick($tr.X + [int]($tr.W / 2), $tr.Y + [int]($tr.H / 2))
Start-Sleep -Milliseconds 900
Check 'a right click closes a single pin' ((Count-Pins $p.Id).Count -eq 1) "count=$((Count-Pins $p.Id).Count)"

# Exit 3: Alt+F4 as a chord. The foreground control first -- a red here means either the
# chord does nothing or the keystrokes went somewhere else, and only the control tells them apart.
$lastPin = [FF]::ByClass([uint32]$p.Id, 'FocusFlowPin')
[void][FF]::SetForegroundWindow($lastPin)
Start-Sleep -Milliseconds 400
$fgOk = ([FF]::GetForegroundWindow() -eq $lastPin)
Check 'the pin can hold the foreground' $fgOk
if ($fgOk) {
    [FF]::PressAltF4()
    Start-Sleep -Milliseconds 1200
    Check 'Alt+F4 closes a pin' ((Count-Pins $p.Id).Count -eq 0) "count=$((Count-Pins $p.Id).Count)"
} else {
    # Never send the chord blind: whatever really holds the foreground on this desktop is
    # somebody else's program, and Alt+F4 would close it. Record the leg as red, with the reason.
    Check 'Alt+F4 closes a pin' $false 'skipped: the pin did not take the foreground'
}

# Re-open two so the settings-row path closes a known number.
[void][FF]::PostMessage($h, $WM_PIN_TEST, [IntPtr]::Zero, [IntPtr]2)
Start-Sleep -Milliseconds 1200
Check 'the desktop is back to two pins' ((Count-Pins $p.Id).Count -eq 2) "count=$((Count-Pins $p.Id).Count)"

# Close the rest through the same path the settings row uses.
[void][FF]::PostMessage($h, $WM_PIN_CLOSE_ALL, [IntPtr]::Zero, [IntPtr]::Zero)
Start-Sleep -Milliseconds 800
Check 'close-all empties the desktop' ((Count-Pins $p.Id).Count -eq 0)
$log2 = @(Get-PinLog)
Check 'the log reports how many closed' (@($log2 | Where-Object { $_ -match 'pin-close-all 2' }).Count -gt 0)
Check 'still no extra file after closing' ((Count-Png) -eq $pngBefore) "png $pngBefore -> $(Count-Png)"

[void][FF]::PostMessage($h, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw = [Diagnostics.Stopwatch]::StartNew()
while (-not $p.HasExited -and $sw.Elapsed.TotalMilliseconds -lt 10000) { Start-Sleep -Milliseconds 150 }
Check 'quits within 10 s' $p.HasExited "$([int]$sw.Elapsed.TotalMilliseconds) ms"
if (-not $p.HasExited) { Stop-App }

# ---------- round 2: the real gesture, and what twelve of them cost ----------
# Everything above drives synthetic pins. This round pins a *selection* the way a person
# does (drag -> annotate -> P) and checks the two things the eye-check list asked for that
# a harness can actually measure: where the pin lands, and what the cap is worth.
$beforeB = Count-Png
Remove-Item $logPath -ErrorAction SilentlyContinue
$p2 = Start-App $Exe $app $WorkDir @{ FOCUSFLOW_SNIP_OVERLAY = 'annotate' }
$h2 = Wait-Window $p2.Id
Check 'the second round starts' ($h2 -ne [IntPtr]::Zero)
$ov2 = Wait-Window $p2.Id 'FocusFlowSnipOverlay' 6000
Check 'the second round opens the overlay' ($ov2 -ne [IntPtr]::Zero)

Drag-Screen 300 260 700 500
# The annotate stage is not a second window any more: the overlay stays alive and draws its
# own toolbar on the frozen screen, so the observable is "the same overlay is still there".
$alive = Wait-Window $p2.Id 'FocusFlowSnipOverlay' 2000
Check 'the drag left the overlay in the annotate stage' ($alive -ne [IntPtr]::Zero)
Start-Sleep -Milliseconds 600

# P = pin this selection: the same exit as the toolbar's own button.
[void][FF]::PostMessage($alive, 0x0100, [IntPtr]0x50, [IntPtr]::Zero)   # WM_KEYDOWN VK_P
Start-Sleep -Milliseconds 1500
$pinH = [FF]::ByClass([uint32]$p2.Id, 'FocusFlowPin')
Check 'P put the selection on the desktop' ($pinH -ne [IntPtr]::Zero)
$pr = Get-Rect $pinH
# The selection was 300,260 -> 700,500: the pin has to sit exactly on top of it, otherwise
# "the pin lands on its selection" is only a wish.
Check 'the pin lands where the selection was' `
    ($pr.X -eq 300 -and $pr.Y -eq 260 -and $pr.W -eq 400 -and $pr.H -eq 240) `
    ("pin=$($pr.W)x$($pr.H) at $($pr.X),$($pr.Y)")
Check 'the gesture wrote exactly one file' ((Count-Png) -eq ($beforeB + 1)) `
    "png $beforeB -> $(Count-Png)"

# Fill to the cap and push one past it: the 13th must be refused, out loud.
$memBefore = Get-Memory $p2
[void][FF]::PostMessage($h2, $WM_PIN_TEST, [IntPtr]::Zero, [IntPtr]11)
Start-Sleep -Milliseconds 2000
$atCap = Count-Pins $p2.Id
Check 'eleven more pins fill the cap to twelve' ($atCap.Count -eq 12) "count=$($atCap.Count)"
[void][FF]::PostMessage($h2, $WM_PIN_TEST, [IntPtr]::Zero, [IntPtr]2)
Start-Sleep -Milliseconds 1200
Check 'a thirteenth pin is refused' ((Count-Pins $p2.Id).Count -eq 12) "count=$((Count-Pins $p2.Id).Count)"
$skip = @(Get-PinLog | Where-Object { $_ -match 'pin-skip' })
Check 'the refusal says why' ($skip.Count -gt 0)
$memAfter = Get-Memory $p2
# Not a check -- a number for the handoff: what twelve pins cost this process.
("info: memory after 12 pins ws={0:N1} MB private={1:N1} MB (was ws={2:N1} MB private={3:N1} MB)" -f `
    $memAfter.WsMb, $memAfter.PrivateMb, $memBefore.WsMb, $memBefore.PrivateMb)

# ---------- the hover-revealed close box, and the percent the zoom reports ----------
# The product says where its own button is ('pin-chrome'), so the harness does not carry a
# second copy of the ruler. Twelve pins are on the desktop at this point: the click has to
# take exactly one away, which is also what separates a close-one path from close-all.
$chrome = @(Get-PinLog | Where-Object { $_ -match 'pin-chrome close_size=(\d+) close_inset=(\d+)' })
Check 'the pin reports its own close-button geometry' ($chrome.Count -gt 0)
if ($chrome.Count -gt 0) {
    $m = [regex]::Match($chrome[0], 'close_size=(\d+) close_inset=(\d+)')
    $cs = [int]$m.Groups[1].Value
    $ci = [int]$m.Groups[2].Value
    $one = [FF]::ByClass([uint32]$p2.Id, 'FocusFlowPin')
    $r = Get-Rect $one
    $hx = $r.X + $r.W - $ci - [int]($cs / 2)
    $hy = $r.Y + $ci + [int]($cs / 2)
    $beforeBtn = (Count-Pins $p2.Id).Count
    # The pointer has to actually be on that pin: the cross only appears -- and only answers --
    # while it is hovered, otherwise dragging a pin by its top-right corner would close it.
    [void][FF]::SetCursorPos($hx, $hy)
    Start-Sleep -Milliseconds 400
    Click-Screen $hx $hy
    Start-Sleep -Milliseconds 1200
    $afterBtn = (Count-Pins $p2.Id).Count
    Check 'clicking the close box shuts exactly that one pin' ($afterBtn -eq ($beforeBtn - 1)) `
        "pins $beforeBtn -> $afterBtn"
    Check 'the log names the button path' (@(Get-PinLog | Where-Object { $_ -match 'pin-close-btn' }).Count -gt 0)
}

# The badge itself has no machine-readable text; what is measurable is that every wheel step
# reports the same percentage the badge shows.
$wheelBefore = @(Get-PinLog | Where-Object { $_ -match 'pin-scale' }).Count
$zPin = [FF]::ByClass([uint32]$p2.Id, 'FocusFlowPin')
if ($zPin -ne [IntPtr]::Zero) {
    [void][FF]::PostMessage($zPin, 0x020A, [IntPtr](120 -shl 16), [IntPtr]0)   # WM_MOUSEWHEEL up one notch
    Start-Sleep -Milliseconds 800
    Check 'the wheel step reports its percent' `
        ((@(Get-PinLog | Where-Object { $_ -match 'pin-scale' }).Count) -gt $wheelBefore)
} else {
    Check 'the wheel step found a pin to zoom' $false 'no pin window left'
}

Quit-Round $h2 $p2

Show-Summary
