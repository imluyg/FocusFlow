# Second launch while the app is hidden must not crash; it must summon the first window.
#
# The new process only posts WM_SECOND_INSTANCE (WM_APP + 18). Calling the first
# process's bring_to_front directly would dereference a pointer from another heap.
#
# ASCII-only, like every file in this folder.

param(
    [string]$Exe = '',
    [string]$WorkDir = ''
)

. "$PSScriptRoot\accept-lib.ps1"

if (-not $Exe) { $Exe = Get-DefaultExe }
if (-not $WorkDir) { $WorkDir = Join-Path (Get-RepoRoot) 'target\accept\singleinstance' }

Stop-App $Exe
$app = Join-Path $WorkDir 'app'
New-AppDir $app @('[collector]', 'enabled = false')

$p1 = Start-App $Exe $app $WorkDir
$h = Wait-Window $p1.Id
Check 'first instance shows its window' ($h -ne [IntPtr]::Zero)
if ($h -eq [IntPtr]::Zero) { Stop-App; Show-Summary }

# Same path as clicking X: hide to tray (the mutex stays held).
[void][FF]::PostMessage($h, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)
Start-Sleep -Milliseconds 700
Check 'closing hides the first window' (-not [FF]::IsWindowVisible($h))

$p2 = Start-App $Exe $app $WorkDir
$sw = [Diagnostics.Stopwatch]::StartNew()
while (-not $p2.HasExited -and $sw.Elapsed.TotalMilliseconds -lt 8000) {
    Start-Sleep -Milliseconds 100
}
Check 'second instance exits by itself' $p2.HasExited
if ($p2.HasExited) {
    Check 'second instance exit code is 0' ($p2.ExitCode -eq 0) "code=$($p2.ExitCode)"
}
Start-Sleep -Milliseconds 700
Check 'the first window comes back' ([FF]::IsWindowVisible($h))
Check 'the first instance is still alive' (-not $p1.HasExited)

# Quit through the acceptance hook (same as the tray Quit action).
[void][FF]::PostMessage($h, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw2 = [Diagnostics.Stopwatch]::StartNew()
while (-not $p1.HasExited -and $sw2.Elapsed.TotalMilliseconds -lt 8000) {
    Start-Sleep -Milliseconds 150
}
Check 'the first instance quits within 8 s' $p1.HasExited "$([int]$sw2.Elapsed.TotalMilliseconds) ms"
if (-not $p1.HasExited) { Stop-App $Exe }

Show-Summary
