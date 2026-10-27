<#
.SYNOPSIS
  Floating pill: default size, saved position, drag persistence, off-screen clamp,
  double-click brings the main window back, and the whole card answers HTCAPTION.

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-floating.ps1

  Note on -Exe: the suite's Stop-App calls match the DEFAULT exe path (target\release\...), so
  pointing -Exe at a copy elsewhere leaves the first instance alive; it then holds the
  single-instance mutex and every later restart in this file reads a missing pill (measured
  2026-10-27: two such runs, both red on the restart legs for that reason alone).
  Use the default path, or a mutation probe on the product, when you need a before/after pair.
#>
param(
    [string]$Exe = '',
    [string]$WorkDir = ''
)

. "$PSScriptRoot\accept-lib.ps1"

if (-not $Exe) { $Exe = Get-DefaultExe }
if (-not $WorkDir) { $WorkDir = Join-Path (Get-RepoRoot) 'target\accept\floating' }

$WM_EXITSIZEMOVE = 0x0232
$WM_NCLBUTTONDBLCLK = 0x00A3

Stop-App
$app = Join-Path $WorkDir 'app'
New-AppDir $app @('[collector]', 'enabled = false')
Set-ConfigLine $app 'floating' 'enabled' 'true'
Set-ConfigLine $app 'floating' 'pos_x' '1500'
Set-ConfigLine $app 'floating' 'pos_y' '140'
Remove-Item (Join-Path $WorkDir 'focusflow.log') -ErrorAction SilentlyContinue

$p = Start-App $Exe $app $WorkDir
$h = Wait-Window $p.Id
$pill = Wait-Window $p.Id 'FocusFlowFloatingPill' 8000
Check 'main window appears' ($h -ne [IntPtr]::Zero)
Check 'floating pill appears' ($pill -ne [IntPtr]::Zero)
if ($pill -eq [IntPtr]::Zero) { Stop-App; Show-Summary }
Start-Sleep -Milliseconds 900

$r = Get-Rect $pill
Check 'pill lands where the config says (1500,140)' ($r.X -eq 1500 -and $r.Y -eq 140) "$($r.X),$($r.Y)"
Check 'pill uses the 104x48 content default' ($r.W -eq 104 -and $r.H -eq 48) "$($r.W)x$($r.H)"
Check 'WM_NCHITTEST answers HTCAPTION (2)' ([FF]::HitTest([uint32]$p.Id, 'FocusFlowFloatingPill') -eq 2)

# A drag ends with WM_EXITSIZEMOVE; the pill then writes pos_x/pos_y back to config.
#
# The old shape waited 200 ms before posting the drag-end message. The pill repaints on a
# 1000 ms timer (floating.rs TICK_MS) and that tick positions the card from ANCHOR_TOP, which
# only persist_position() refreshes -- so whether a tick happened to land inside those 200 ms
# decided the reading (measured: one session 11/0, the next session 2 reds on the same bytes).
# These two legs wait THROUGH a tick on purpose instead of leaving it to luck: a move that has
# not reached drag-end yet must still survive the tick, because a card that jumps back at the
# user mid-drag is exactly what this suite is supposed to catch.
Move-Window $pill 1300 300
Start-Sleep -Milliseconds 1300
$rm = Get-Rect $pill
Check 'an unpersisted vertical move survives a repaint tick' ($rm.X -eq 1300 -and $rm.Y -eq 300) "$($rm.X),$($rm.Y)"
[void][FF]::PostMessage($pill, $WM_EXITSIZEMOVE, [IntPtr]::Zero, [IntPtr]::Zero)
Start-Sleep -Milliseconds 1000
$px = Get-ConfigValue $app 'floating' 'pos_x'
$py = Get-ConfigValue $app 'floating' 'pos_y'
Check 'drag writes the position back to config' ("$px,$py" -eq '1300,300') "$px,$py"

Stop-App
$p = Start-App $Exe $app $WorkDir
$h = Wait-Window $p.Id
$pill = Wait-Window $p.Id 'FocusFlowFloatingPill' 8000
Start-Sleep -Milliseconds 900
$r = Get-Rect $pill
Check 'position survives a restart' ($r.X -eq 1300 -and $r.Y -eq 300) "$($r.X),$($r.Y)"

# Off-screen saved position (unplugged second monitor) must be pulled back to 20 px visible.
Stop-App
Set-ConfigLine $app 'floating' 'pos_x' '4000'
$p = Start-App $Exe $app $WorkDir
$h = Wait-Window $p.Id
$pill = Wait-Window $p.Id 'FocusFlowFloatingPill' 8000
Start-Sleep -Milliseconds 900
$r = Get-Rect $pill
Check 'off-screen position is clamped to 20 px visible' ($r.X -eq 1900) "$($r.X),$($r.Y)"

# A position that puts the WHOLE card inside the docked-bar band must be lifted out at startup.
# Reported live 2026-10-27: 1920x1080 with a 48 px bottom taskbar (work bottom 1032) and
# pos_y = 1033, so the card sat entirely behind the taskbar -- the bar is topmost too and higher,
# and "at least 20 px visible" measured against the FULL screen cannot save that case.
# The ruler here is therefore the WORK AREA, and its bottom edge is read from the system rather
# than typed in (hard rule: measure the geometry a leg depends on, don't hardcode it).
Add-Type -AssemblyName System.Windows.Forms
$work = [System.Windows.Forms.Screen]::PrimaryScreen.WorkingArea
$bnd = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
Stop-App
Set-ConfigLine $app 'floating' 'pos_x' '82'
Set-ConfigLine $app 'floating' 'pos_y' ($work.Bottom + 1)
$p = Start-App $Exe $app $WorkDir
$h = Wait-Window $p.Id
$pill = Wait-Window $p.Id 'FocusFlowFloatingPill' 8000
Start-Sleep -Milliseconds 900
$rt = Get-Rect $pill
# Gate on the window actually being there: with no pill, the rect is 0,0,0,0 and "bottom <= work
# bottom" would be TRUE for the wrong reason. Measured 2026-10-27: a run whose first instance
# never died (see the -Exe note in this file's header) produced (0,0) here and this leg PASSED --
# a false green on a premise that was red. Absence-shaped legs must AND their own premise in.
$have = ($pill -ne [IntPtr]::Zero) -and ($rt.H -gt 0) -and ($rt.W -gt 0)
$detail = if ($have) {
    'bottom=' + ($rt.Y + $rt.H) + ' work_bottom=' + $work.Bottom + ' screen_bottom=' + $bnd.Bottom
} else {
    'NOT MEASURED: no pill window to place (that restart did not come up -- premise, not the ruler)'
}
Check 'a card saved inside the taskbar band is lifted above it' ($have -and (($rt.Y + $rt.H) -le $work.Bottom)) $detail
# Put the pill back where the rest of this suite expects it.
Stop-App
Set-ConfigLine $app 'floating' 'pos_x' '1300'
Set-ConfigLine $app 'floating' 'pos_y' '300'
$p = Start-App $Exe $app $WorkDir
$h = Wait-Window $p.Id
$pill = Wait-Window $p.Id 'FocusFlowFloatingPill' 8000
Start-Sleep -Milliseconds 900

# Double-click on the card must bring the main window back after it was hidden to tray.
[void][FF]::PostMessage($h, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)   # WM_CLOSE -> hide
Start-Sleep -Milliseconds 600
$hidden = -not [FF]::IsWindowVisible($h)
[void][FF]::PostMessage($pill, $WM_NCLBUTTONDBLCLK, [IntPtr]::Zero, [IntPtr]::Zero)
Start-Sleep -Milliseconds 800
Check 'double-click brings the hidden main window back' ($hidden -and [FF]::IsWindowVisible($h)) "hidden_before=$hidden"

$log = Read-AppLog $WorkDir
Check 'no error lines in the app log' (-not (@($log) -match "(?i)error|$(Get-FailWord)")) ''

[void][FF]::PostMessage($h, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw = [Diagnostics.Stopwatch]::StartNew()
while (-not $p.HasExited -and $sw.Elapsed.TotalMilliseconds -lt 8000) { Start-Sleep -Milliseconds 150 }
Check 'quits cleanly' $p.HasExited "$([int]$sw.Elapsed.TotalMilliseconds) ms"
if (-not $p.HasExited) { Stop-App }

Show-Summary
