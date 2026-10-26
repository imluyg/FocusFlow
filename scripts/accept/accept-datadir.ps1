<#
.SYNOPSIS
  "Change data folder" round trip: the pending-move marker written into [paths] is
  honoured by the next process, the data actually lands in the new home, and a target
  that cannot be used leaves the data exactly where it was.

.DESCRIPTION
  Two launches against fresh COPYIES of desktop-native\probe. Nothing touches real data.

  Round 1 writes what the settings button writes -- [paths] data_migrate_from = <old home>
  plus [paths] data_home = <new home> -- and then starts the app. The move must be done by
  startup, so afterwards: the new home has the yearly db, the aux db and the backups, the
  marker is back to empty, and the old data dir no longer holds the yearly db.

  Round 2 points data_home at a regular FILE, so the new home cannot be created. That is
  the "silently falls back and keeps recording somewhere else" shape, and it must not be
  irreversible: the app starts, the data stays put and the marker survives for a retry.

  ASCII-only, like every file in this folder.

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-datadir.ps1
#>
param(
    [string]$Exe = '',
    [string]$WorkDir = ''
)

. "$PSScriptRoot\accept-lib.ps1"

if (-not $Exe) { $Exe = Get-DefaultExe }
if (-not $WorkDir) { $WorkDir = Join-Path (Get-RepoRoot) 'target\accept\datadir' }

"exe : $Exe"
Stop-App

function Count-Db([string]$Dir) {
    if (-not (Test-Path $Dir)) { return 0 }
    @(Get-ChildItem $Dir -Filter '*.db*' -File -ErrorAction SilentlyContinue).Count
}

# ---------- round 1: a usable new home ----------
$app = Join-Path $WorkDir 'app'
$new = Join-Path $WorkDir 'newhome'
$logPath = Join-Path $WorkDir 'focusflow.log'
New-AppDir $app @('[collector]', 'enabled = false')
# probe's backup/ is not part of New-AppDir's copy, but a real data root has one
# (exit backup / "backup now" write it). Seed it so the "backup moved too" assertion
# has something behind it instead of passing on an empty premise.
Copy-Item (Join-Path (Get-RepoRoot) 'desktop-native\probe\backup') (Join-Path $app 'backup') -Recurse
if (Test-Path $new) { Remove-Item $new -Recurse -Force }
New-Item -ItemType Directory -Force -Path $new | Out-Null
Set-ConfigLine $app 'paths' 'data_migrate_from' $app
Set-ConfigLine $app 'paths' 'data_home' $new
Remove-Item $logPath -ErrorAction SilentlyContinue
"app : $app"
"new : $new"

$p = Start-App $Exe $app $WorkDir
$h = Wait-Window $p.Id
Check 'main window appears' ($h -ne [IntPtr]::Zero)
if ($h -eq [IntPtr]::Zero) { Stop-App; Show-Summary }

# The move happens in the first seconds of startup, before the window settles.
Start-Sleep -Milliseconds 2500
Check 'yearly db moved into the new home' (Test-Path (Join-Path $new 'data\focusflow_2026.db'))
Check 'aux db moved into the new home' (Test-Path (Join-Path $new 'data\device_aliases.json'))
Check 'backups moved into the new home' ((Count-Db (Join-Path $new 'backup')) -gt 0) "count=$(Count-Db (Join-Path $new 'backup'))"

$marker = Get-ConfigValue $app 'paths' 'data_migrate_from'
Check 'the pending-move marker is cleared' ([string]::IsNullOrEmpty($marker)) "marker=$marker"
Check 'data_home still points at the new home' ((Get-ConfigValue $app 'paths' 'data_home') -eq $new)

$srcDb = Join-Path $app 'data\focusflow_2026.db'
Check 'the old data dir no longer holds it' (-not (Test-Path $srcDb))
Check 'the old backup dir is gone' (-not (Test-Path (Join-Path $app 'backup')))

$log = @()
if (Test-Path $logPath) { $log = Get-Content $logPath -Encoding UTF8 }
Check 'app log names the new home' (@($log | Where-Object { $_ -like '*datadir\newhome*' }).Count -gt 0)

[void][FF]::PostMessage($h, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw = [Diagnostics.Stopwatch]::StartNew()
while (-not $p.HasExited -and $sw.Elapsed.TotalMilliseconds -lt 10000) { Start-Sleep -Milliseconds 150 }
Check 'quits within 10 s' $p.HasExited "$([int]$sw.Elapsed.TotalMilliseconds) ms"
if (-not $p.HasExited) { Stop-App }

# ---------- round 2: the new home cannot be created ----------
$app2 = Join-Path $WorkDir 'app2'
$blocked = Join-Path $WorkDir 'blocked'
New-AppDir $app2 @('[collector]', 'enabled = false')
if (Test-Path $blocked) { Remove-Item $blocked -Force }
# A regular file where the new data root should be: create_dir_all fails on it.
[void][IO.File]::WriteAllText($blocked, 'x')
Set-ConfigLine $app2 'paths' 'data_migrate_from' $app2
Set-ConfigLine $app2 'paths' 'data_home' $blocked
Remove-Item $logPath -ErrorAction SilentlyContinue

$p2 = Start-App $Exe $app2 $WorkDir
$h2 = Wait-Window $p2.Id
Check 'an unusable new home does not stop the app' ($h2 -ne [IntPtr]::Zero)
Start-Sleep -Milliseconds 2500

Check 'the data stays where it was' (Test-Path (Join-Path $app2 'data\focusflow_2026.db'))
$marker2 = Get-ConfigValue $app2 'paths' 'data_migrate_from'
Check 'the marker survives for a retry' ($marker2 -eq $app2) "marker=$marker2"

if ($h2 -ne [IntPtr]::Zero) {
    [void][FF]::PostMessage($h2, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
    $sw2 = [Diagnostics.Stopwatch]::StartNew()
    while (-not $p2.HasExited -and $sw2.Elapsed.TotalMilliseconds -lt 10000) { Start-Sleep -Milliseconds 150 }
}
if (-not $p2.HasExited) { Stop-App }

Show-Summary
