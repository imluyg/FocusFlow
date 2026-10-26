<#
.SYNOPSIS
  Theme regression: walk every view in dark, flip to light from the settings row, walk
  them again, and check the inline EDIT's background follows the theme.

.DESCRIPTION
  Screenshots land in -WorkDir (default target\accept\theme). The EDIT check is numeric:
  we copy the control's own pixels and take the dominant colour (background wins over text).

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-theme.ps1
#>
param(
    [string]$Exe = '',
    [string]$WorkDir = ''
)

. "$PSScriptRoot\accept-lib.ps1"

if (-not $Exe) { $Exe = Get-DefaultExe }
if (-not $WorkDir) { $WorkDir = Join-Path (Get-RepoRoot) 'target\accept\theme' }

Stop-App
$app = Join-Path $WorkDir 'app'
New-AppDir $app @('[collector]', 'enabled = false')
Set-ConfigLine $app 'gui' 'theme' 'dark'
Remove-Item (Join-Path $WorkDir 'focusflow-native.log') -ErrorAction SilentlyContinue

$p = Start-App $Exe $app $WorkDir
$h = Wait-Window $p.Id
Check 'main window appears' ($h -ne [IntPtr]::Zero)
if ($h -eq [IntPtr]::Zero) { Stop-App; Show-Summary }
Start-Sleep -Milliseconds 1500
# Period "7 days": the probe data ends yesterday, so "today" is empty and tables would
# have no rows (the rename button lives on a device row).
Click-PeriodPill $h 1

# --- dark walk -------------------------------------------------------------------
for ($i = 0; $i -lt 6; $i++) { Click-ViewTab $h $i; Save-WindowShot $h (Join-Path $WorkDir "dark-view$i.png") | Out-Null }
Click-ViewTab $h 3
for ($j = 0; $j -lt 4; $j++) { Click-ATab $h $j; Save-WindowShot $h (Join-Path $WorkDir "dark-atab$j.png") | Out-Null }
Check 'dark screenshots written' ((Get-ChildItem $WorkDir -Filter 'dark-*.png').Count -ge 10) ''

# --- flip to light from the settings row (index 5 = theme) ------------------------
Click-ViewTab $h 5
$before = Get-ThemeName $app
Click-BasicRow $h 5
Start-Sleep -Milliseconds 700
$after = Get-ThemeName $app
Check 'theme flips through the settings row' (($before -match 'dark') -and ($after -match 'light')) "$before -> $after"

# --- light walk ------------------------------------------------------------------
for ($i = 0; $i -lt 6; $i++) { Click-ViewTab $h $i; Save-WindowShot $h (Join-Path $WorkDir "light-view$i.png") | Out-Null }
Click-ViewTab $h 3
for ($j = 0; $j -lt 4; $j++) { Click-ATab $h $j; Save-WindowShot $h (Join-Path $WorkDir "light-atab$j.png") | Out-Null }
Check 'light screenshots written' ((Get-ChildItem $WorkDir -Filter 'light-*.png').Count -ge 10) ''

# --- inline EDIT colours (WM_CTLCOLOREDIT must follow the theme) ------------------
Click-ViewTab $h 2
[FF]::ClickWindow($h, 639, 284, 1)   # "rename" button of device row 0
Start-Sleep -Milliseconds 500
$edit = [FF]::ChildByClass($h, 'Edit')
Check 'inline EDIT appears on rename' ($edit -ne [IntPtr]::Zero)
if ($edit -ne [IntPtr]::Zero) {
    $r = Get-Rect $edit
    $shot = Join-Path $WorkDir 'light-edit.png'
    Save-RectShot $r.X $r.Y $r.W $r.H $shot
    $c = Get-DominantColor $shot
    Check 'EDIT background is light in light theme' ($c.R -gt 180 -and $c.G -gt 180 -and $c.B -gt 180) "rgb=$($c.R),$($c.G),$($c.B)"
    [void][FF]::PostMessage($edit, 0x0100, [IntPtr]0x1B, [IntPtr]0)   # Esc = cancel
    Start-Sleep -Milliseconds 400
}

Click-ViewTab $h 5
Click-BasicRow $h 5
Start-Sleep -Milliseconds 600
$back = Get-ThemeName $app
Check 'theme flips back to dark' ($back -match 'dark') $back

Click-ViewTab $h 2
[FF]::ClickWindow($h, 639, 284, 1)
Start-Sleep -Milliseconds 500
$edit2 = [FF]::ChildByClass($h, 'Edit')
if ($edit2 -ne [IntPtr]::Zero) {
    $r2 = Get-Rect $edit2
    $shot2 = Join-Path $WorkDir 'dark-edit.png'
    Save-RectShot $r2.X $r2.Y $r2.W $r2.H $shot2
    $c2 = Get-DominantColor $shot2
    Check 'EDIT background is dark in dark theme' ($c2.R -lt 90 -and $c2.G -lt 90 -and $c2.B -lt 90) "rgb=$($c2.R),$($c2.G),$($c2.B)"
    [void][FF]::PostMessage($edit2, 0x0100, [IntPtr]0x1B, [IntPtr]0)
    Start-Sleep -Milliseconds 300
}

$log = Read-AppLog $WorkDir
Check 'no error lines in the app log' (-not (@($log) -match "(?i)error|$(Get-FailWord)")) ''

[void][FF]::PostMessage($h, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw = [Diagnostics.Stopwatch]::StartNew()
while (-not $p.HasExited -and $sw.Elapsed.TotalMilliseconds -lt 8000) { Start-Sleep -Milliseconds 150 }
Check 'quits cleanly' $p.HasExited "$([int]$sw.Elapsed.TotalMilliseconds) ms"
if (-not $p.HasExited) { Stop-App }

Show-Summary
