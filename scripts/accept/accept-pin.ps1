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

$WM_PIN_TEST = 0x400 + 12
$WM_PIN_CLOSE_ALL = 0x400 + 13

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

Show-Summary
