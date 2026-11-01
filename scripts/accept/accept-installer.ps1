<#
.SYNOPSIS
  Installer round-trip: silent install -> run the installed exe -> uninstall three ways.

.DESCRIPTION
  Touches the real per-user footprint on purpose: %LOCALAPPDATA%\Programs\FocusFlow,
  the desktop/Start-Menu shortcuts and HKCU. Run it only on a machine where that is fine
  (it uninstalls whatever was installed there first, so the shorthand "already installed"
  state does not skew the checks).

  The uninstall contract it measures: the program files always go, the statistics in
  <inst>\data go ONLY when a human answered "yes" to the prompt. Silent (/S) means keep,
  because nobody is there to answer. Every leg writes a sentinel file into data\ first --
  an empty data folder is created by the installer itself, so its presence alone would
  not prove which branch ran.

  The setup exe comes from dist\ (build it with `cmd /c build_native.bat`).

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-installer.ps1
#>
param(
    [string]$Setup = '',
    [string]$WorkDir = ''
)

. "$PSScriptRoot\accept-lib.ps1"

Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class Dlg {
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextLength(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr GetDlgItem(IntPtr h, int id);
  [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr h, uint msg, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern IntPtr PostMessage(IntPtr h, uint msg, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWndProc cb, IntPtr l);
  public delegate bool EnumWndProc(IntPtr h, IntPtr l);

  static bool TitleMatches(IntPtr h, string needle) {
    if (!IsWindowVisible(h)) return false;
    int n = GetWindowTextLength(h);
    if (n == 0) return false;
    StringBuilder sb = new StringBuilder(n + 2);
    GetWindowText(h, sb, sb.Capacity);
    return sb.ToString().IndexOf(needle, StringComparison.OrdinalIgnoreCase) >= 0;
  }

  // Visible top-level window whose title contains needle AND that owns the given button id.
  public static IntPtr Prompt(string needle, int buttonId) {
    IntPtr hit = IntPtr.Zero;
    EnumWindows((h, l) => {
      if (hit != IntPtr.Zero) return false;
      if (!TitleMatches(h, needle)) return true;
      if (GetDlgItem(h, buttonId) == IntPtr.Zero) return true;
      hit = h;
      return false;
    }, IntPtr.Zero);
    return hit;
  }

  public static bool AnyWindowTitle(string needle) {
    return FindAny(needle) != IntPtr.Zero;
  }

  public static IntPtr FindAny(string needle) {
    IntPtr hit = IntPtr.Zero;
    EnumWindows((h, l) => {
      if (hit != IntPtr.Zero) return false;
      if (!TitleMatches(h, needle)) return true;
      hit = h;
      return false;
    }, IntPtr.Zero);
    return hit;
  }
}
"@

# Run the uninstaller the way a person does: no /S, so the data prompt appears, answer it
# (IDYES = 6 deletes the statistics too, IDNO = 7 keeps them) and wait for the work to land.
#
# Three things measured here, none of them guessed (first run of this harness got a green
# "exit=0" from a process that had deleted nothing):
#   * an NSIS uninstaller that has to remove its own exe copies itself to %TEMP% and restarts
#     -- measured as %TEMP%\~nsuN.tmp\Un.exe, process name `Un` -- so the pid Start-Process
#     hands back is a stub that exits 0 BEFORE any file is touched, and `uninstall*` does not
#     match the worker. The dialog is found by its caption instead (NSIS titles it
#     "<Name> Uninstall"), and the button by control id.
#   * the end signal is NOT "our window disappeared": the non-silent uninstaller leaves its
#     progress window open after the sections finish, waiting for a click on Close. So the
#     caller passes the state that proves work happened, and this function then closes
#     whatever window is left so the worker actually exits.
function Invoke-InteractiveUninstall([int]$ButtonId, [ScriptBlock]$Settled) {
    $u = Join-Path (Get-InstalledDir) 'uninstall.exe'
    if (-not (Test-Path $u)) { return [pscustomobject]@{ Answered = $false; Settled = $false } }
    [void](Start-Process -FilePath $u)
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $dlg = [IntPtr]::Zero
    while ($sw.Elapsed.TotalMilliseconds -lt 30000) {
        $dlg = [Dlg]::Prompt('FocusFlow', $ButtonId)
        if ($dlg -ne [IntPtr]::Zero) { break }
        Start-Sleep -Milliseconds 120
    }
    if ($dlg -eq [IntPtr]::Zero) { return [pscustomobject]@{ Answered = $false; Settled = $false } }
    $btn = [Dlg]::GetDlgItem($dlg, $ButtonId)
    [void][Dlg]::SendMessage($btn, 0xF5, [IntPtr]::Zero, [IntPtr]::Zero)
    $sw2 = [Diagnostics.Stopwatch]::StartNew()
    # NOTE: PowerShell names are case-insensitive -- `$settled` here would be the ScriptBlock
    # parameter itself, and assigning $false to it throws a cast error mid-run. Measured.
    $reached = $false
    while ($sw2.Elapsed.TotalMilliseconds -lt 60000) {
        if (Invoke-Command -ScriptBlock $Settled) { $reached = $true; break }
        Start-Sleep -Milliseconds 150
    }
    # dismiss the "done" window(s). Measured: WM_CLOSE alone does not end it -- NSIS's progress
    # window waits for its own Close button (control id 2 = IDCANCEL), and a run that leaves it
    # open leaves the %TEMP%\~nsuN.tmp\Un.exe worker alive on the machine.
    $sw3 = [Diagnostics.Stopwatch]::StartNew()
    while ($sw3.Elapsed.TotalMilliseconds -lt 8000) {
        $w = [Dlg]::FindAny('FocusFlow')
        if ($w -eq [IntPtr]::Zero) { break }
        $close = [Dlg]::GetDlgItem($w, 2)
        if ($close -ne [IntPtr]::Zero) { [void][Dlg]::SendMessage($close, 0xF5, [IntPtr]::Zero, [IntPtr]::Zero) }
        Start-Sleep -Milliseconds 250
    }
    for ($round = 0; $round -lt 4; $round++) {
        $w = [Dlg]::FindAny('FocusFlow')
        if ($w -eq [IntPtr]::Zero) { break }
        $wid = 0
        [void][Dlg]::GetWindowThreadProcessId($w, [ref]$wid)
        try { [Diagnostics.Process]::GetProcessById([int]$wid).Kill() } catch { }
        Start-Sleep -Milliseconds 300
    }
    Start-Sleep -Milliseconds 400
    [pscustomobject]@{ Answered = $true; Settled = $reached }
}

# A sentinel only this harness writes, with a per-run token: the installer creates an EMPTY
# data\ itself, so "data\ is still there" would pass on its own and prove nothing.
$script:SentinelName = 'accept-keep-sentinel.txt'
function New-Sentinel([string]$Inst) {
    $token = 'keep-' + [Guid]::NewGuid().ToString('N').Substring(0, 8)
    $dir = Join-Path $Inst 'data'
    if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
    $path = Join-Path $dir $script:SentinelName
    Set-Content -Path $path -Value $token -Encoding ASCII -NoNewline
    $token
}
function Test-SentinelKept([string]$Inst, [string]$Token) {
    $path = Join-Path (Join-Path $Inst 'data') $script:SentinelName
    (Test-Path $path) -and ((Get-Content $path -Raw) -eq $Token)
}

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
# The new silent uninstall keeps data\ on purpose, so a previous run of THIS harness can leave
# its sentinel behind. Drop only that file -- anything else in there belongs to the user.
Remove-Item (Join-Path (Join-Path $inst 'data') $script:SentinelName) -Force -ErrorAction SilentlyContinue
$code = Invoke-Setup $Setup
Check 'silent install exits 0' ($code -eq 0) "exit=$code"
Check 'install dir created' (Test-Path $inst) $inst
Check 'FocusFlow.exe installed' (Test-Path (Join-Path $inst 'FocusFlow.exe')) ''
Check 'uninstall.exe installed' (Test-Path (Join-Path $inst 'uninstall.exe')) ''
Check 'desktop shortcut created' (Test-Path $desktopLnk) ''
Check 'start menu folder created' (Test-Path $startMenu) ''
Check 'HKCU uninstall key created' (Test-Path $uninstallKey) ''
$ver = if (Test-Path $uninstallKey) { (Get-ItemProperty $uninstallKey).DisplayVersion } else { '' }
# Expected number comes from the setup we just built, not a second hardcoded copy: pinning it
# here made the first release after the pin go red with a perfectly correct installer.
$setupVer = "$(([Diagnostics.FileVersionInfo]::GetVersionInfo($Setup)).FileVersion)"
Check 'uninstall key carries the version' ($ver -eq $setupVer -and $ver.Length -gt 0) "DisplayVersion=$ver setup=[$setupVer]"

Add-Type -AssemblyName System.Drawing
$ico = [System.Drawing.Icon]::ExtractAssociatedIcon((Join-Path $inst 'FocusFlow.exe'))
Check 'installed exe carries an icon resource' ($ico.Width -gt 0) "$($ico.Width)x$($ico.Height)"
$ico.Dispose()

# The exe's own VERSIONINFO resource. NSIS stamps a version onto setup.exe (VIAddVersionKey) and
# writes DisplayVersion into the uninstall key, but the exe ITSELF had no version resource at all
# once the frontend went native: right-click -> Details is blank and anything auditing by file
# version gets nothing (measured 2026-10-27, both via .VersionInfo and
# [Diagnostics.FileVersionInfo]::GetVersionInfo, so it is not a reader quirk).
# The expected number comes from setup.exe, not a second hardcoded copy of the version.
$viSetup = [Diagnostics.FileVersionInfo]::GetVersionInfo($Setup)
$viExe = [Diagnostics.FileVersionInfo]::GetVersionInfo((Join-Path $inst 'FocusFlow.exe'))
$exeVer = "$($viExe.FileVersion)"
Check 'installed exe carries a version resource' ($exeVer.Length -gt 0) "exe=[$exeVer] setup=[$setupVer]"
Check 'installed exe version matches the installer' ($exeVer -eq $setupVer -and $exeVer.Length -gt 0) "exe=[$exeVer] setup=[$setupVer]"

# Run the INSTALLED exe, but point it at a probe copy so the install dir stays pristine.
# No [collector] override here: this checks the real default (on, 2026-10-27).
$app = Join-Path $WorkDir 'app'
New-AppDir $app
Remove-Item (Join-Path $WorkDir 'focusflow.log') -ErrorAction SilentlyContinue
$p = Start-App (Join-Path $inst 'FocusFlow.exe') $app $WorkDir
$h = Wait-Window $p.Id
Check 'installed exe starts' ($h -ne [IntPtr]::Zero)
Start-Sleep -Milliseconds 3000
$st = Wait-Status $WorkDir
Check 'collector is on by default' ($null -ne $st -and $st.Collector -eq 'on') "collector=$($st.Collector)"
$mem = Get-Memory $p
"memory: $($mem.WsMb) MB WS / $($mem.PrivateMb) MB private / $($mem.Threads) threads"
Save-WindowShot $h (Join-Path $WorkDir 'installed-run.png') | Out-Null

[void][FF]::PostMessage($h, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw = [Diagnostics.Stopwatch]::StartNew()
while (-not $p.HasExited -and $sw.Elapsed.TotalMilliseconds -lt 8000) { Start-Sleep -Milliseconds 150 }
if (-not $p.HasExited) { Stop-App (Join-Path $inst 'FocusFlow.exe') }

# ---------------------------------------------------------------- the data contract
# Leg 1: silent (/S) uninstall. Nobody is there to answer the question, so it keeps the data.
$token1 = New-Sentinel $inst
$code2 = (Start-Process -FilePath (Join-Path $inst 'uninstall.exe') -ArgumentList '/S' -Wait -PassThru).ExitCode
Start-Sleep -Milliseconds 1000
Check 'silent uninstall exits 0' ($code2 -eq 0) "exit=$code2"
Check 'silent uninstall removes the program binary' (-not (Test-Path (Join-Path $inst 'FocusFlow.exe'))) ''
Check 'silent uninstall removes the plugins it installed' (-not (Test-Path (Join-Path $inst 'plugins'))) ''
Check 'silent uninstall removes its own exe' (-not (Test-Path (Join-Path $inst 'uninstall.exe'))) ''
Check 'silent uninstall keeps the data sentinel' (Test-SentinelKept $inst $token1) "token=$token1"
Check 'desktop shortcut removed' (-not (Test-Path $desktopLnk)) ''
Check 'start menu folder removed' (-not (Test-Path $startMenu)) ''
Check 'HKCU uninstall key removed' (-not (Test-Path $uninstallKey)) ''
Check 'HKCU app key removed' (-not (Test-Path 'HKCU:\Software\FocusFlow')) ''

# Leg 2: a person is watching and answers "no" -- the default button, i.e. exactly what
# somebody who taps Enter instead of reading gets.
$code3 = Invoke-Setup $Setup
Check 'reinstalled for the interactive prompt' (Test-Path (Join-Path $inst 'FocusFlow.exe')) "exit=$code3"
$token2 = New-Sentinel $inst
# Proof the uninstaller ran at all. Deliberately NOT the branch-specific assertions below it,
# so "answered the prompt but nothing deleted" stays distinguishable from "deleted the wrong
# half" -- the first version of this harness could not tell those apart and called it a pass.
$didWork = { -not (Test-Path (Join-Path $inst 'FocusFlow.exe')) }
$r2 = Invoke-InteractiveUninstall 7 $didWork
Check 'uninstall answered "no" ran the deletion' ($r2.Answered -and $r2.Settled) "answered=$($r2.Answered) settled=$($r2.Settled)"
Check '"no" removes the program binary' (-not (Test-Path (Join-Path $inst 'FocusFlow.exe'))) ''
Check '"no" keeps the data sentinel' (Test-SentinelKept $inst $token2) "token=$token2"

# Leg 3: the same prompt answered "yes" -- the ONLY path that takes the statistics with it.
# Last, so the machine ends as it was found.
$code4 = Invoke-Setup $Setup
Check 'reinstalled for the destructive answer' (Test-Path (Join-Path $inst 'FocusFlow.exe')) "exit=$code4"
$token3 = New-Sentinel $inst
$token3 = New-Sentinel $inst
$r3 = Invoke-InteractiveUninstall 6 $didWork
Check 'uninstall answered "yes" ran the deletion' ($r3.Answered -and $r3.Settled) "answered=$($r3.Answered) settled=$($r3.Settled)"
Check '"yes" takes the whole folder with it' (-not (Test-Path $inst)) ''
Check '"yes" leaves no desktop shortcut' (-not (Test-Path $desktopLnk)) ''
Check '"yes" leaves no HKCU uninstall key' (-not (Test-Path $uninstallKey)) ''

Show-Summary
