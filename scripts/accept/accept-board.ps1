# Board visibility: the board tab is off by default, and the settings row is the only entry.
#
# This suite deliberately starts with [gui] show_board = false (the shipped default), then
# proves the tab strip really has seven slots and that toggling the row rewrites the config
# and restores the eighth slot. The other suites keep the board visible (see New-AppDir in
# accept-lib.ps1), because they click tabs by view index.
#
# Slot vs view index (client coords, 100% DPI, 1184x821 client):
#   hidden: slot 6 = settings (view 7); shown: slot 6 = plugins (view 6), slot 7 = settings.
# The app logs every tab click as "<n> at (x,y)", so the digit before "at" is the view that
# was actually opened -- no OCR needed.
#
# ASCII-only, like every file in this folder.

param(
    [string]$Exe = '',
    [string]$WorkDir = ''
)

. "$PSScriptRoot\accept-lib.ps1"

if (-not $Exe) { $Exe = Get-DefaultExe }
if (-not $WorkDir) { $WorkDir = Join-Path (Get-RepoRoot) 'target\accept\board' }

# The last tab click the app logged: returns the view index, or $null.
function Get-LastViewClick([string]$WorkDir) {
    $hit = @(Read-AppLog $WorkDir | Select-String -Pattern '(\d+)\s+at\s+\(\d+,\d+\)\s*$' | Select-Object -Last 1)
    if (-not $hit) { return $null }
    $m = [regex]::Match($hit.Line, '(\d+)\s+at\s+\(\d+,\d+\)\s*$')
    if ($m.Success) { return [int]$m.Groups[1].Value }
    $null
}

function Get-ConfigValueNormalized([string]$AppDir, [string]$Section, [string]$Key) {
    $v = Get-ConfigValue $AppDir $Section $Key
    if ($null -eq $v) { return $null }
    return $v.Trim().ToLowerInvariant()
}

Stop-App $Exe
$app = Join-Path $WorkDir 'app'
# The shipped default is "off"; pass it explicitly so this suite keeps testing that path
# even if the default is flipped again. The last [gui] block wins.
New-AppDir $app @('[gui]', 'show_board = false', '[collector]', 'enabled = false')

$p = Start-App $Exe $app $WorkDir
$h = Wait-Window $p.Id
Check 'main window appears' ($h -ne [IntPtr]::Zero)
if ($h -eq [IntPtr]::Zero) { Stop-App; Show-Summary }
Start-Sleep -Milliseconds 1200

$startsHidden = Get-ConfigValueNormalized $app 'gui' 'show_board'
Check 'fixture starts with the board hidden' ($startsHidden -eq 'false') "show_board=$startsHidden"

# Hidden layout: seven slots, settings sits in slot 6 (x = 56 + 6*84 = 560).
Click-ViewTab $h 6
Start-Sleep -Milliseconds 400
Check 'hidden: slot 6 opens settings (view 7)' ((Get-LastViewClick $WorkDir) -eq 7) "view=$(Get-LastViewClick $WorkDir)"

# The board row is the 5th row of the regular category (autostart, collector, pause,
# theme, board): centre y = 194 + 4*44 = 370.
Click-BasicRow $h 4
Start-Sleep -Milliseconds 700
$shown = Get-ConfigValueNormalized $app 'gui' 'show_board'
Check 'the board row writes show_board = true' ($shown -eq 'true') "show_board=$shown"

# Shown layout: eight slots, so slot 6 is plugins (view 6) and slot 7 is settings (view 7).
Click-ViewTab $h 6
Start-Sleep -Milliseconds 400
Check 'shown: slot 6 opens plugins (view 6)' ((Get-LastViewClick $WorkDir) -eq 6) "view=$(Get-LastViewClick $WorkDir)"
Click-ViewTab $h 7
Start-Sleep -Milliseconds 400
Check 'shown: slot 7 opens settings (view 7)' ((Get-LastViewClick $WorkDir) -eq 7) "view=$(Get-LastViewClick $WorkDir)"

# Toggle it back off and prove the strip shrinks again.
Click-BasicRow $h 4
Start-Sleep -Milliseconds 700
$hidden = Get-ConfigValueNormalized $app 'gui' 'show_board'
Check 'the board row writes show_board = false' ($hidden -eq 'false') "show_board=$hidden"
Click-ViewTab $h 6
Start-Sleep -Milliseconds 400
Check 'hidden again: slot 6 opens settings (view 7)' ((Get-LastViewClick $WorkDir) -eq 7) "view=$(Get-LastViewClick $WorkDir)"

[void][FF]::PostMessage($h, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw = [Diagnostics.Stopwatch]::StartNew()
while (-not $p.HasExited -and $sw.Elapsed.TotalMilliseconds -lt 8000) { Start-Sleep -Milliseconds 150 }
Check 'quits within 8 s' $p.HasExited "$([int]$sw.Elapsed.TotalMilliseconds) ms"
if (-not $p.HasExited) { Stop-App $Exe }

Show-Summary
