<#
.SYNOPSIS
  Hotkey customization: the combos the app registers must be the combos the config (and
  therefore the settings page) shows, and an unusable value must not take the app down.

.DESCRIPTION
  Three launches against a fresh COPY of desktop-native\probe.

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
$annotate1 = 'ctrl+alt+9'
New-AppDir $app @('[collector]', 'enabled = false')
Set-ConfigLine $app 'hotkey' 'toggle_window' $toggle1
Set-ConfigLine $app 'hotkey' 'snip' $snip1
Set-ConfigLine $app 'hotkey' 'snip_annotate' $annotate1
Remove-Item $logPath -ErrorAction SilentlyContinue
"exe : $Exe"
"app : $app  (toggle=$toggle1 snip=$snip1 annotate=$annotate1)"

$p = Start-App $Exe $app $WorkDir
$h = Wait-Window $p.Id
Check 'main window appears' ($h -ne [IntPtr]::Zero)
if ($h -eq [IntPtr]::Zero) { Stop-App; Show-Summary }

# Registration happens at startup; give the log a moment to be flushed.
Start-Sleep -Milliseconds 2000
$log = @(Get-HotLog)
Check 'log registers the configured toggle combo' (@($log | Where-Object { $_ -like "*toggle_window = $toggle1*" }).Count -gt 0)
Check 'log registers the configured snip combo' (@($log | Where-Object { $_ -like "*snip = $snip1*" }).Count -gt 0)
Check 'log registers the configured annotate combo' (@($log | Where-Object { $_ -like "*snip_annotate = $annotate1*" }).Count -gt 0)
Check 'neither combo failed' ([bool](Test-NoComboFailure $log @($toggle1, $snip1, $annotate1)))

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

# ---------- round 3: the master switch survives a restart ----------
# The switch used to only take effect in the running process: it unregistered the combos and
# wrote [hotkey] enabled, but nothing read that key at boot, so the next start registered all
# three again -- while the settings row still showed whatever the last click had said.
#
# Absence here is backed by a presence control inside the same file: rounds 1-2 write exactly
# these 'key = combo' lines when the switch is on, so "not one of them in this log" can only
# come from the switch. A crashed start would also give an empty log, which is why the
# main-window leg below is a real check rather than a skip.
Set-ConfigLine $app 'hotkey' 'enabled' 'false'
Remove-Item $logPath -ErrorAction SilentlyContinue

$p3 = Start-App $Exe $app $WorkDir
$h3 = Wait-Window $p3.Id
Check 'the app starts with the switch off' ($h3 -ne [IntPtr]::Zero)
if ($h3 -eq [IntPtr]::Zero) { Stop-App; Show-Summary }

Start-Sleep -Milliseconds 2000
$log3 = @(Get-HotLog)
# The off sentence carries the ASCII '[hotkey] enabled = false'. Match it with -match:
# -like would read those brackets as a character set and silently never hit.
Check 'the log says the switch is what stopped registration' `
    (@($log3 | Where-Object { $_ -match '\[hotkey\] enabled = false' }).Count -gt 0)
$regLines = @($log3 | Where-Object {
        $_ -like "*toggle_window = $toggle1*" -or `
        $_ -like "*snip = $snip1*" -or `
        $_ -like "*snip_annotate = $annotate1*"
    })
Check 'no combo is registered while the switch is off' ($regLines.Count -eq 0) ("lines=$($regLines.Count)")
# Turning the switch off must not eat the bindings: it is a switch, not a delete.
$cfgAfter = Get-ConfigValue $app 'hotkey' 'snip'
Check 'the configured combo is still in the file' ($cfgAfter -eq $snip1) "$cfgAfter"

[void][FF]::PostMessage($h3, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw3 = [Diagnostics.Stopwatch]::StartNew()
while (-not $p3.HasExited -and $sw3.Elapsed.TotalMilliseconds -lt 8000) { Start-Sleep -Milliseconds 150 }
if (-not $p3.HasExited) { Stop-App }

# ---------- round 4: the clear button next to a binding ----------
# The cross inside the value box is the "keep the line, do not arm it" gesture. Two things
# have to hold: the file gets an EMPTY value (not the default string -- restoring the default
# would silently re-arm the combo on the next start), and the next start really skips it.
#
# Coordinates are CLIENT pixels (the click helper posts WM_LBUTTONDOWN in client space):
# settings list_top=260, row_h=44, the first binding row is the 5th item of this category,
# and the button is 18 tall starting 13 px into the row -> y = 260 + 4*44 + 22 = 458.
# x = 847 is the centre of the 34 px button that ends 6 px inside a 150 px box at x=720.
Set-ConfigLine $app 'hotkey' 'enabled' 'true'
Set-ConfigLine $app 'hotkey' 'toggle_window' 'ctrl+alt+3'
Remove-Item $logPath -ErrorAction SilentlyContinue

$p4 = Start-App $Exe $app $WorkDir
$h4 = Wait-Window $p4.Id
Check 'the app starts for the clear-button round' ($h4 -ne [IntPtr]::Zero)
if ($h4 -eq [IntPtr]::Zero) { Stop-App; Show-Summary }

Click-ViewTab $h4 5
Start-Sleep -Milliseconds 400
# The page opens on category 0; the three bindings live in category 1 (client y = 260 + 44*i + 20).
[FF]::ClickWindow($h4, 123, 324, 1)
Start-Sleep -Milliseconds 500
[FF]::ClickWindow($h4, 847, 458, 1)
Start-Sleep -Milliseconds 1500

$cfg4 = Get-ConfigValue $app 'hotkey' 'toggle_window'
Check 'the cross left an empty value, not the default combo' ($cfg4 -eq '') "got [$cfg4]"
$log4 = @(Get-HotLog)
Check 'the button path logged the empty value' `
    (@($log4 | Where-Object { $_ -like '*toggle_window = ""*' }).Count -gt 0)
# The boot line for ctrl+alt+3 is already in this log (the app started with that combo), so
# "not registered" cannot be read as "no line mentions toggle_window". What must never appear
# is the DEFAULT combo: that is what a reader collapsing "empty" into "absent" would arm.
$rearmed = @($log4 | Where-Object { $_ -like '*toggle_window = ctrl+shift+f*' })
Check 'clearing did not re-arm the default combo' ($rearmed.Count -eq 0) "lines=$($rearmed.Count)"
# A clear button that eats its neighbours is the bug to fear: the other two must stay intact.
Check 'the other binding still registers' `
    (@($log4 | Where-Object { $_ -like '*snip = ctrl+alt+8*' }).Count -gt 0)
$cfgSnip = Get-ConfigValue $app 'hotkey' 'snip'
Check 'the other binding is untouched in the file' ($cfgSnip -eq 'ctrl+alt+8') "got [$cfgSnip]"

[void][FF]::PostMessage($h4, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw4 = [Diagnostics.Stopwatch]::StartNew()
while (-not $p4.HasExited -and $sw4.Elapsed.TotalMilliseconds -lt 8000) { Start-Sleep -Milliseconds 150 }
if (-not $p4.HasExited) { Stop-App }

# ---------- round 5: an emptied binding stays emptied across a restart ----------
# This is the whole point of the gesture: if any reader collapses "line present but empty"
# into "no line", the default combo comes back on the next start -- while the screen has
# already told him that binding is off.
Remove-Item $logPath -ErrorAction SilentlyContinue
$p5 = Start-App $Exe $app $WorkDir
$h5 = Wait-Window $p5.Id
Check 'the app starts with one binding cleared' ($h5 -ne [IntPtr]::Zero)
if ($h5 -eq [IntPtr]::Zero) { Stop-App; Show-Summary }
Start-Sleep -Milliseconds 2000
$log5 = @(Get-HotLog)
$back = @($log5 | Where-Object { $_ -like '*toggle_window*' })
Check 'the cleared binding did not come back on restart' ($back.Count -eq 0) "lines=$($back.Count)"
Check 'the untouched binding still registers after restart' `
    (@($log5 | Where-Object { $_ -like '*snip = ctrl+alt+8*' }).Count -gt 0)

[void][FF]::PostMessage($h5, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw5 = [Diagnostics.Stopwatch]::StartNew()
while (-not $p5.HasExited -and $sw5.Elapsed.TotalMilliseconds -lt 8000) { Start-Sleep -Milliseconds 150 }
if (-not $p5.HasExited) { Stop-App }

Show-Summary
