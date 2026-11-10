# Minimum-window layout smoke: the window must be able to reach 960x480 DIP and every
# page must paint without the process dying. This is deliberately a layout smoke, not a
# pixel oracle: the unit suite pins the geometry (chart card, board compact mode, detail
# error room), and this run exercises the real message loop at that size.
#
# ASCII-only, like every file in this folder.

param(
    [string]$Exe = '',
    [string]$WorkDir = ''
)

. "$PSScriptRoot\accept-lib.ps1"

if (-not $Exe) { $Exe = Get-DefaultExe }
if (-not $WorkDir) { $WorkDir = Join-Path (Get-RepoRoot) 'target\accept\minsize' }

Stop-App $Exe
$app = Join-Path $WorkDir 'app'
New-AppDir $app @('[collector]', 'enabled = false')

$p = Start-App $Exe $app $WorkDir
$h = Wait-Window $p.Id
Check 'main window appears' ($h -ne [IntPtr]::Zero)
if ($h -eq [IntPtr]::Zero) { Stop-App; Show-Summary }

$scale = [FF]::GetDpiForSystem() / 96.0
$wantW = [int][Math]::Round(960 * $scale)
$wantH = [int][Math]::Round(480 * $scale)
# SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE
[void][FF]::SetWindowPos($h, [IntPtr]::Zero, 0, 0, $wantW, $wantH, 0x0016)
Start-Sleep -Milliseconds 700
$cl = Get-Client $h
Check 'window reaches 960x480 DIP' ($cl.W -eq $wantW -and $cl.H -eq $wantH) "$($cl.W)x$($cl.H) want=${wantW}x${wantH}"

# Click every top tab. Coordinates come from the same helper the other suites use.
for ($i = 0; $i -lt 8; $i++) {
    Click-ViewTab $h $i
    Start-Sleep -Milliseconds 350
    $shot = Join-Path $WorkDir ("min-view-$i.png")
    $ok = Save-WindowShot $h $shot
    Check "view $i paints at 960x480" ([bool]$ok -and (Test-Path $shot)) $shot
    Check "process survives view $i" (-not $p.HasExited)
    if ($p.HasExited) { break }
}

$log = Read-AppLog $WorkDir
Check 'no error lines in the app log' (-not (@($log) -match "(?i)error|$(Get-FailWord)")) ''

[void][FF]::PostMessage($h, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw = [Diagnostics.Stopwatch]::StartNew()
while (-not $p.HasExited -and $sw.Elapsed.TotalMilliseconds -lt 8000) { Start-Sleep -Milliseconds 150 }
Check 'quits within 8 s' $p.HasExited "$([int]$sw.Elapsed.TotalMilliseconds) ms"
if (-not $p.HasExited) { Stop-App $Exe }

Show-Summary
