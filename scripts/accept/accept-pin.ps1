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

# The rule that matters: opening a pin must not save anything again.
Check 'pins wrote no extra file' ((Count-Png) -eq $pngBefore) "png $pngBefore -> $(Count-Png)"

# Close one with Esc (the same message the pin window handles).
$firstHwnd = [FF]::ByClass([uint32]$p.Id, 'FocusFlowPin')
[void][FF]::PostMessage($firstHwnd, 0x0100, [IntPtr]0x1B, [IntPtr]::Zero)   # WM_KEYDOWN ESC
Start-Sleep -Milliseconds 800
Check 'Esc closes a single pin' ((Count-Pins $p.Id).Count -eq 2) "count=$((Count-Pins $p.Id).Count)"

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
$ed2 = Wait-Window $p2.Id 'FocusFlowSnipEditor' 6000
Check 'the drag opened the editor' ($ed2 -ne [IntPtr]::Zero)

# P = pin this selection: the same exit as the toolbar button.
[void][FF]::PostMessage($ed2, 0x0100, [IntPtr]0x50, [IntPtr]::Zero)   # WM_KEYDOWN VK_P
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

Quit-Round $h2 $p2

Show-Summary
