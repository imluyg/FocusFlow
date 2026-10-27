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

# A position that puts the WHOLE card inside the docked-bar band must be LEFT ALONE -- the ruler for
# "can the user see it" is z-order, not the work area.
# Live, twice, on 2026-10-27: first he reported the card hidden behind the bar (pos_y = 1033 with a
# 48 px bottom bar), which we "fixed" by lifting it above the work area (`30aa624`); he then
# reported THAT as the bug: he wants the card to sit ON the bar, left alone -- no auto-move, and no
# being hidden by the bar either.
# So: startup must not move a placement that is on screen (the bottom edge is the SCREEN's bottom),
# and being covered by the bar is answered by putting the pill above Shell_TrayWnd in z-order.
# Geometry comes from the system, never typed in (hard rule: measure what a leg depends on).
Add-Type -AssemblyName System.Windows.Forms
$work = [System.Windows.Forms.Screen]::PrimaryScreen.WorkingArea
$bnd = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
Stop-App
Set-ConfigLine $app 'floating' 'pos_x' '82'
Set-ConfigLine $app 'floating' 'pos_y' ($work.Bottom + 1)
$p = Start-App $Exe $app $WorkDir
$h = Wait-Window $p.Id
$pill = Wait-Window $p.Id 'FocusFlowFloatingPill' 8000
Start-Sleep -Milliseconds 1300   # give the tick a chance to push the card up, the way it used to
$rt = Get-Rect $pill
# Gate on the window actually being there: with no pill the rect is 0,0,0,0 and both rulers below
# would be TRUE for the wrong reason. Measured 2026-10-27: a run whose first instance never died
# (see the -Exe note in this file's header) produced (0,0) here and the old leg PASSED -- a false
# green on a red premise. Absence-shaped legs must AND their own premise in.
$have = ($pill -ne [IntPtr]::Zero) -and ($rt.H -gt 0) -and ($rt.W -gt 0)
$detail = if ($have) {
    'bottom=' + ($rt.Y + $rt.H) + ' work_bottom=' + $work.Bottom + ' screen_bottom=' + $bnd.Bottom
} else {
    'NOT MEASURED: no pill window to place (that restart did not come up -- premise, not the ruler)'
}
# Flushed to the screen bottom is the most this leg allows to move. The overlap itself is judged on
# the card's BOTTOM edge, not its top: a 48 px card landing at top=1032 sits exactly ON the work-area
# line, so "top > work bottom" would call a card that covers the whole bar band "not overlapping".
$stays = $have -and (($rt.Y + $rt.H) -eq $bnd.Bottom) -and (($rt.Y + $rt.H) -gt $work.Bottom)
Check 'a card placed on the taskbar band is not lifted out of it' $stays $detail
# The other half of his requirement: it must not be HIDDEN by the bar either. Smaller ZOrder = higher.
# The ruler gets its own control leg first: the bar must rank above the desktop window (Progman).
# Without that, "pill < bar" could be satisfied by a ruler that simply does not see either window.
$zPill = if ($have) { [FF]::ZOrder('FocusFlowFloatingPill', [uint32]$p.Id) } else { -1 }
$zBar = [FF]::ZOrder('Shell_TrayWnd', 0)
$zDesk = [FF]::ZOrder('Progman', 0)
$zCtl = ($zBar -gt 0) -and ($zDesk -gt 0) -and ($zBar -lt $zDesk)
$zCtlDetail = if ($zCtl) { 'bar_rank=' + $zBar + ' desktop_rank=' + $zDesk } else { 'NOT MEASURED: bar=' + $zBar + ' desktop=' + $zDesk + ' (the ruler could not rank both windows)' }
Check 'the z-order ruler ranks the taskbar above the desktop' $zCtl $zCtlDetail
$zHave = ($zPill -gt 0) -and ($zBar -gt 0)
$zDetail = if ($zHave) { 'pill_rank=' + $zPill + ' bar_rank=' + $zBar } else { 'NOT MEASURED: pill=' + $zPill + ' bar=' + $zBar + ' (a window the ruler needs was not enumerated)' }
Check 'a card on the band paints above the taskbar' ($zHave -and $zCtl -and ($zPill -lt $zBar)) $zDetail
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
