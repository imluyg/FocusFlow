<#
.SYNOPSIS
  Floating pill: default size, saved position, drag persistence, off-screen clamp,
  double-click brings the main window back, and the whole card answers HTCAPTION.

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-floating.ps1
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
Check 'pill uses the 132x48 content default' ($r.W -eq 132 -and $r.H -eq 48) "$($r.W)x$($r.H)"
Check 'WM_NCHITTEST answers HTCAPTION (2)' ([FF]::HitTest([uint32]$p.Id, 'FocusFlowFloatingPill') -eq 2)

# A drag ends with WM_EXITSIZEMOVE; the pill then writes pos_x/pos_y back to config.
Move-Window $pill 1300 300
Start-Sleep -Milliseconds 200
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
