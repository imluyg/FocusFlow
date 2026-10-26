<#
.SYNOPSIS
  Hotkey customization: the combos the app registers must be the combos the config (and
  therefore the settings page) shows, and an unusable value must not take the app down.

.DESCRIPTION
  Two launches against a fresh COPY of desktop-native\probe.

  Round 1 writes two custom combos into [hotkey] and expects the app log to say it
  registered exactly those two strings -- that log line and the settings page read the
  same config value, so this is the "shown == registered" assertion.

  Round 2 makes one combo unusable (a bare key with no modifier). The app must still
  start, must name the value it refused verbatim, and must still register the other one.

  Injected keystrokes never reach the low-level hook on this machine (see README), so
  the harness checks registration and log wording rather than pressing the combos.

  ASCII-only, like every file in this folder.

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-hotkey.ps1
#>
param(
    [string]$Exe = '',
    [string]$WorkDir = ''
)

. "$PSScriptRoot\accept-lib.ps1"

if (-not $Exe) { $Exe = Get-DefaultExe }
if (-not $WorkDir) { $WorkDir = Join-Path (Get-RepoRoot) 'target\accept\hotkey' }

$app = Join-Path $WorkDir 'app'
$logPath = Join-Path $WorkDir 'focusflow.log'
$fail = Get-FailWord

function Get-HotLog {
    if (Test-Path $logPath) { Get-Content $logPath -Encoding UTF8 } else { @() }
}

# Any line that names one of our combos and also says "failed" means shown != registered.
function Test-NoComboFailure([object[]]$Lines, [string[]]$Combos) {
    foreach ($c in $Combos) {
        $bad = @($Lines | Where-Object { $_ -like "*$c*" -and $_ -match $fail })
        if ($bad.Count -gt 0) { return $false }
    }
    $true
}

Stop-App

# ---------- round 1: both combos come from config ----------
$toggle1 = 'ctrl+alt+7'
$snip1 = 'ctrl+alt+8'
New-AppDir $app @('[collector]', 'enabled = false')
Set-ConfigLine $app 'hotkey' 'toggle_window' $toggle1
Set-ConfigLine $app 'hotkey' 'snip' $snip1
Remove-Item $logPath -ErrorAction SilentlyContinue
"exe : $Exe"
"app : $app  (toggle=$toggle1 snip=$snip1)"

$p = Start-App $Exe $app $WorkDir
$h = Wait-Window $p.Id
Check 'main window appears' ($h -ne [IntPtr]::Zero)
if ($h -eq [IntPtr]::Zero) { Stop-App; Show-Summary }

# Registration happens at startup; give the log a moment to be flushed.
Start-Sleep -Milliseconds 2000
$log = @(Get-HotLog)
Check 'log registers the configured toggle combo' (@($log | Where-Object { $_ -like "*toggle_window = $toggle1*" }).Count -gt 0)
Check 'log registers the configured snip combo' (@($log | Where-Object { $_ -like "*snip = $snip1*" }).Count -gt 0)
Check 'neither combo failed' ([bool](Test-NoComboFailure $log @($toggle1, $snip1)))

# Nothing rewrote what we asked for (the settings page shows these two strings).
$cfgToggle = Get-ConfigValue $app 'hotkey' 'toggle_window'
$cfgSnip = Get-ConfigValue $app 'hotkey' 'snip'
Check 'config still holds both combos' ($cfgToggle -eq $toggle1 -and $cfgSnip -eq $snip1) "$cfgToggle / $cfgSnip"

[void][FF]::PostMessage($h, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw = [Diagnostics.Stopwatch]::StartNew()
while (-not $p.HasExited -and $sw.Elapsed.TotalMilliseconds -lt 8000) { Start-Sleep -Milliseconds 150 }
Check 'quits within 8 s' $p.HasExited "$([int]$sw.Elapsed.TotalMilliseconds) ms"
if (-not $p.HasExited) { Stop-App }

# ---------- round 2: one combo is unusable ----------
Set-ConfigLine $app 'hotkey' 'toggle_window' 'f1'
Remove-Item $logPath -ErrorAction SilentlyContinue

$p2 = Start-App $Exe $app $WorkDir
$h2 = Wait-Window $p2.Id
Check 'an unusable combo does not stop the app' ($h2 -ne [IntPtr]::Zero)
if ($h2 -eq [IntPtr]::Zero) { Stop-App; Show-Summary }

Start-Sleep -Milliseconds 2000
$log2 = @(Get-HotLog)
Check 'log names the refused combo verbatim' (@($log2 | Where-Object { $_ -like '*toggle_window = "f1"*' }).Count -gt 0)
Check 'the other combo still registers' (@($log2 | Where-Object { $_ -like "*snip = $snip1*" -and $_ -notmatch $fail }).Count -gt 0)

[void][FF]::PostMessage($h2, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw2 = [Diagnostics.Stopwatch]::StartNew()
while (-not $p2.HasExited -and $sw2.Elapsed.TotalMilliseconds -lt 8000) { Start-Sleep -Milliseconds 150 }
if (-not $p2.HasExited) { Stop-App }

Show-Summary
