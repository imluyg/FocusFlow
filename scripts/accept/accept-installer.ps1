<#
.SYNOPSIS
  Installer round-trip: silent install -> run the installed exe -> silent uninstall -> clean.

.DESCRIPTION
  Touches the real per-user footprint on purpose: %LOCALAPPDATA%\Programs\FocusFlow,
  the desktop/Start-Menu shortcuts and HKCU. Run it only on a machine where that is fine
  (it uninstalls whatever was installed there first, so the shorthand "already installed"
  state does not skew the checks).

  The setup exe comes from dist\ (build it with `cmd /c build_native.bat`).

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-installer.ps1
#>
param(
    [string]$Setup = '',
    [string]$WorkDir = ''
)

. "$PSScriptRoot\accept-lib.ps1"

if (-not $Setup) { $Setup = Get-NewestSetup }
if (-not $WorkDir) { $WorkDir = Join-Path (Get-RepoRoot) 'target\accept\installer' }

Check 'setup.exe exists in dist (run build_native.bat first)' ($null -ne $Setup -and (Test-Path $Setup)) $Setup
if (-not $Setup -or -not (Test-Path $Setup)) { Show-Summary }

Stop-App
$inst = Get-InstalledDir
$desktopLnk = Join-Path ([Environment]::GetFolderPath('Desktop')) 'FocusFlow.lnk'
$startMenu = Join-Path ([Environment]::GetFolderPath('Programs')) 'FocusFlow'
$uninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\FocusFlow'

# Start from "not installed" so every presence check below means something.
if (Test-Path (Join-Path $inst 'uninstall.exe')) {
    Start-Process -FilePath (Join-Path $inst 'uninstall.exe') -ArgumentList '/S' -Wait | Out-Null
    Start-Sleep -Milliseconds 900
}
$code = Invoke-Setup $Setup
Check 'silent install exits 0' ($code -eq 0) "exit=$code"
Check 'install dir created' (Test-Path $inst) $inst
Check 'FocusFlow.exe installed' (Test-Path (Join-Path $inst 'FocusFlow.exe')) ''
Check 'uninstall.exe installed' (Test-Path (Join-Path $inst 'uninstall.exe')) ''
Check 'desktop shortcut created' (Test-Path $desktopLnk) ''
Check 'start menu folder created' (Test-Path $startMenu) ''
Check 'HKCU uninstall key created' (Test-Path $uninstallKey) ''
$ver = if (Test-Path $uninstallKey) { (Get-ItemProperty $uninstallKey).DisplayVersion } else { '' }
Check 'uninstall key carries the version' ($ver -eq '0.5.0') "DisplayVersion=$ver"

Add-Type -AssemblyName System.Drawing
$ico = [System.Drawing.Icon]::ExtractAssociatedIcon((Join-Path $inst 'FocusFlow.exe'))
Check 'installed exe carries an icon resource' ($ico.Width -gt 0) "$($ico.Width)x$($ico.Height)"
$ico.Dispose()

# Run the INSTALLED exe, but point it at a probe copy so the install dir stays pristine.
$app = Join-Path $WorkDir 'app'
New-AppDir $app @('[collector]', 'enabled = false')
Remove-Item (Join-Path $WorkDir 'focusflow.log') -ErrorAction SilentlyContinue
$p = Start-App (Join-Path $inst 'FocusFlow.exe') $app $WorkDir
$h = Wait-Window $p.Id
Check 'installed exe starts' ($h -ne [IntPtr]::Zero)
Start-Sleep -Milliseconds 3000
$st = Wait-Status $WorkDir
Check 'collector stays off by default' ($null -ne $st -and $st.Collector -eq 'off') "collector=$($st.Collector)"
$mem = Get-Memory $p
"memory: $($mem.WsMb) MB WS / $($mem.PrivateMb) MB private / $($mem.Threads) threads"
Save-WindowShot $h (Join-Path $WorkDir 'installed-run.png') | Out-Null

[void][FF]::PostMessage($h, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw = [Diagnostics.Stopwatch]::StartNew()
while (-not $p.HasExited -and $sw.Elapsed.TotalMilliseconds -lt 8000) { Start-Sleep -Milliseconds 150 }
if (-not $p.HasExited) { Stop-App (Join-Path $inst 'FocusFlow.exe') }

$code2 = (Start-Process -FilePath (Join-Path $inst 'uninstall.exe') -ArgumentList '/S' -Wait -PassThru).ExitCode
Start-Sleep -Milliseconds 1000
Check 'silent uninstall exits 0' ($code2 -eq 0) "exit=$code2"
Check 'install dir removed' (-not (Test-Path $inst)) ''
Check 'desktop shortcut removed' (-not (Test-Path $desktopLnk)) ''
Check 'start menu folder removed' (-not (Test-Path $startMenu)) ''
Check 'HKCU uninstall key removed' (-not (Test-Path $uninstallKey)) ''

Show-Summary
