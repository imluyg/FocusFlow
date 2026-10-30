<#
.SYNOPSIS
  Silent-start family: [gui] start_to_tray (persistent) and --hidden (the argument the autostart
  shortcut carries) must both keep the main window off screen while the process stays alive, the
  argument must win over the config, and WM_SHOW_MAIN must still be able to raise the window.

.DESCRIPTION
  Before this round neither entry was read anywhere: the config key only had a settings row and
  nobody parsed --hidden, so "start silently into the tray" had no working path at all.
  Leg 1 also proves the fixture guard in New-AppDir: desktop-native/probe/config.ini deliberately
  keeps the key ON, so a window that DOES appear means the appended override is what saved it.

  WM_APP is 0x8000, so WM_SHOW_MAIN is 0x8007 (not 0x407).

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-startup.ps1
#>
param(
    [string]$Exe = '',
    [string]$WorkDir = ''
)

. "$PSScriptRoot\accept-lib.ps1"

# The accept-lib lookup filters on IsWindowVisible, which is exactly what this family turns off.
# This one ignores visibility: a hidden-but-created window is the whole point of legs 2 and 3.
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class FFStartup {
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr l);
    public delegate bool EnumWindowsProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    static uint wantPid; static string wantCls; static IntPtr hit;
    public static IntPtr ByClassAnyState(uint pid, string cls) {
        wantPid = pid; wantCls = cls; hit = IntPtr.Zero;
        EnumWindows((h, l) => {
            uint p; GetWindowThreadProcessId(h, out p);
            if (p != wantPid) return true;
            StringBuilder sb = new StringBuilder(256);
            GetClassName(h, sb, 256);
            if (sb.ToString() == cls) { hit = h; return false; }
            return true;
        }, IntPtr.Zero);
        return hit;
    }
}
"@

$ErrorActionPreference = 'Continue'
if (-not $Exe) { $Exe = Get-DefaultExe }
if (-not $WorkDir) { $WorkDir = Join-Path (Get-RepoRoot) 'target\accept\startup' }
$WM_SHOW_MAIN = 0x8007

# Start-App does not take arguments; this family is about an argument, so it needs its own start.
function Start-AppArgs([string]$ExePath, [string]$AppDir, [string]$Work, [string]$Arguments) {
    if (-not (Test-Path $ExePath)) { throw "exe not found: $ExePath" }
    $script:LastExe = $ExePath
    New-Item -ItemType Directory -Force -Path $Work | Out-Null
    $psi = New-Object Diagnostics.ProcessStartInfo
    $psi.FileName = $ExePath
    $psi.WorkingDirectory = $Work
    $psi.UseShellExecute = $false
    $psi.EnvironmentVariables['FOCUSFLOW_APP_DIR'] = $AppDir
    $psi.EnvironmentVariables['FOCUSFLOW_ACCEPTANCE'] = '1'
    if ($Arguments) { $psi.Arguments = $Arguments }
    [Diagnostics.Process]::Start($psi)
}

Stop-App

# ---- leg 1: the fixture guard holds, so the twelve suites keep their wait-for-window premise.
$app1 = Join-Path $WorkDir 'app-guard'
New-AppDir $app1 @()
$probeCfg = Join-Path (Get-RepoRoot) 'desktop-native\probe\config.ini'
$probeSays = ''
if (Test-Path $probeCfg) {
    $lines = @(Get-Content $probeCfg -Encoding UTF8)
    $hits = @($lines | Where-Object { $_ -match '^\s*start_to_tray\s*=' })
    if ($hits.Count -gt 0) { $probeSays = ($hits[-1] -replace '\s', '') }
}
$p1 = Start-AppArgs $Exe $app1 (Join-Path $WorkDir 'w1') ''
$h1 = Wait-Window $p1.Id
Check '夹具兜底：默认那份 app 目录仍能等到主窗口' ($h1 -ne [IntPtr]::Zero) (
    'probe=' + $probeSays + ' ' + (Get-Blocker))
Stop-App
Start-Sleep -Seconds 1

# ---- leg 2: key ON, no argument -> no window, but the process is alive and holds the mutex.
$app2 = Join-Path $WorkDir 'app-key'
New-AppDir $app2 @('[gui]', 'start_to_tray = true')
$p2 = Start-AppArgs $Exe $app2 (Join-Path $WorkDir 'w2') ''
$h2 = Wait-Window $p2.Id 'FocusFlowNativeD2D' 6000
$p2.Refresh()
$alive = -not $p2.HasExited
Check '键为 true 且不带参数：等不到主窗口' ($h2 -eq [IntPtr]::Zero) (
    'hwnd=' + $h2 + ' alive=' + $alive + ' ' + (Get-Blocker))
Check '但进程活着（不是根本没建窗，也不是崩了）' $alive (
    'exit=' + $(if ($p2.HasExited) { $p2.ExitCode } else { 'running' }) + ' ' + (Get-Blocker))

# ---- leg 3: the control point -- WM_SHOW_MAIN raises it, so the window really exists hidden.
if ($alive) {
    $target = [FFStartup]::ByClassAnyState([uint32]$p2.Id, 'FocusFlowNativeD2D')
    $hiddenExists = $target -ne [IntPtr]::Zero
    Check '藏着的那一扇确实建了（按 pid 枚举得到，不看可见性）' $hiddenExists (
        'hwnd=' + $target + ' ' + (Get-Blocker))
    $posted = $false
    if ($hiddenExists) {
        $posted = [FF]::PostMessage($target, $WM_SHOW_MAIN, [IntPtr]::Zero, [IntPtr]::Zero)
    }
    $h3 = Wait-Window $p2.Id
    Check '发 WM_SHOW_MAIN 后窗口出现（证明它只是藏着，不是没建）' ($h3 -ne [IntPtr]::Zero) (
        'target=' + $target + ' posted=' + $posted + ' hwnd=' + $h3 + ' ' + (Get-Blocker))
} else {
    Check '藏着的那一扇确实建了（按 pid 枚举得到，不看可见性）' $false '进程已退出，无从枚举'
    Check '发 WM_SHOW_MAIN 后窗口出现（证明它只是藏着，不是没建）' $false '进程已退出，无法唤出'
}
Stop-App
Start-Sleep -Seconds 1

# ---- leg 4: key OFF but the argument is present -> still no window (the argument wins).
$app4 = Join-Path $WorkDir 'app-arg'
New-AppDir $app4 @('[gui]', 'start_to_tray = false')
$p4 = Start-AppArgs $Exe $app4 (Join-Path $WorkDir 'w4') '--hidden'
$h4 = Wait-Window $p4.Id 'FocusFlowNativeD2D' 6000
$p4.Refresh()
Check '键为 false 但带 --hidden：参数赢，仍不出窗' ($h4 -eq [IntPtr]::Zero) (
    'hwnd=' + $h4 + ' alive=' + (-not $p4.HasExited) + ' ' + (Get-Blocker))
Stop-App

Show-Summary
