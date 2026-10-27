# Shared helpers for the acceptance harnesses under scripts/accept.
#
# ASCII-ONLY on purpose: Windows PowerShell reads a BOM-less .ps1 as ANSI, and a UTF-8
# Chinese comment then swallows the ASCII characters after it (parse error, not a
# display problem). Keep every file in this folder ASCII.
#
# Usage from a harness:
#     . "$PSScriptRoot\accept-lib.ps1"
#
# Conventions:
#   * the exe defaults to <repo>\target\release\FocusFlow.exe (override with -Exe)
#   * every harness works on a COPY of desktop-native\probe (never the real app dir)
#   * results are printed as PASS/FAIL lines; the harness exits 1 if anything failed

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

Add-Type @"
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;

public class FF {
  public delegate bool EnumWindowsProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr p, EnumWindowsProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint flags, int dx, int dy, uint data, UIntPtr extra);
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
  [DllImport("user32.dll")] public static extern uint MapVirtualKeyW(uint code, uint mapType);
  [DllImport("user32.dll")] public static extern IntPtr GetWindowLongPtr(IntPtr h, int i);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);

  public struct RECT { public int Left, Top, Right, Bottom; }

  public static IntPtr ByClass(uint pid, string cls) {
    IntPtr found = IntPtr.Zero;
    EnumWindows((h, l) => {
      uint p; GetWindowThreadProcessId(h, out p);
      if (p == pid) {
        var sb = new StringBuilder(256); GetClassName(h, sb, 256);
        if (sb.ToString() == cls) { found = h; return false; }
      }
      return true;
    }, IntPtr.Zero);
    return found;
  }
  public static IntPtr ChildByClass(IntPtr parent, string cls) {
    IntPtr found = IntPtr.Zero;
    EnumChildWindows(parent, (h, l) => {
      var sb = new StringBuilder(256); GetClassName(h, sb, 256);
      if (sb.ToString() == cls && IsWindowVisible(h)) { found = h; return false; }
      return true;
    }, IntPtr.Zero);
    return found;
  }
  public static List<string> TopWindows(uint pid) {
    var list = new List<string>();
    EnumWindows((h, l) => {
      uint p; GetWindowThreadProcessId(h, out p);
      if (p == pid) {
        var sb = new StringBuilder(256); GetClassName(h, sb, 256);
        RECT r; GetWindowRect(h, out r);
        list.Add(sb.ToString() + " " + (r.Right - r.Left) + "x" + (r.Bottom - r.Top)
                 + " at " + r.Left + "," + r.Top + " visible=" + IsWindowVisible(h));
      }
      return true;
    }, IntPtr.Zero);
    return list;
  }
  // WM_NCHITTEST on the pill must answer HTCAPTION (2): the whole card is a drag handle.
  public static long HitTest(uint pid, string cls) {
    IntPtr h = ByClass(pid, cls);
    if (h == IntPtr.Zero) return -1;
    return SendMessage(h, 0x0084, IntPtr.Zero, (IntPtr)((10 << 16) | 10)).ToInt64();
  }
  // A click inside our own window: counted by the collector, and harmless there.
  public static void ClickWindow(IntPtr hwnd, int x, int y, int times) {
    for (int i = 0; i < times; i++) {
      PostMessage(hwnd, 0x0201, (IntPtr)1, (IntPtr)((y << 16) | (x & 0xFFFF)));
      System.Threading.Thread.Sleep(50);
      PostMessage(hwnd, 0x0202, IntPtr.Zero, (IntPtr)((y << 16) | (x & 0xFFFF)));
      System.Threading.Thread.Sleep(120);
    }
  }
  // Screen-level click: this one goes through the global hook (for collector tests).
  public static void ClickScreen(IntPtr hwnd, int x, int y, int times) {
    RECT r; GetWindowRect(hwnd, out r);
    for (int i = 0; i < times; i++) {
      SetCursorPos(r.Left + x, r.Top + y);
      System.Threading.Thread.Sleep(90);
      mouse_event(0x0002, 0, 0, 0, UIntPtr.Zero);
      System.Threading.Thread.Sleep(50);
      mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero);
      System.Threading.Thread.Sleep(120);
    }
  }
  // Letters with real scan codes: rdev maps by VK but a zero scan code still shows up
  // as Unknown on other paths, so set both.
  public static void Letters(string letters) {
    foreach (char c in letters) {
      byte vk = (byte)char.ToUpperInvariant(c);
      byte scan = (byte)MapVirtualKeyW(vk, 0);
      keybd_event(vk, scan, 0, UIntPtr.Zero);
      System.Threading.Thread.Sleep(25);
      keybd_event(vk, scan, 2, UIntPtr.Zero);
      System.Threading.Thread.Sleep(35);
    }
  }
}
"@

$script:Checks = @()
$script:RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path

function Get-RepoRoot { $script:RepoRoot }

function Get-DefaultExe {
    Join-Path $script:RepoRoot 'target\release\FocusFlow.exe'
}

# The app writes Chinese log lines, but this folder must stay ASCII-only (see the header).
# Build the words from code points instead of typing them, so a BOM-less .ps1 read as ANSI
# cannot mangle them into a pattern that silently matches nothing.
#   "fail"    -> the word our own log lines use for failures
#   "closed"  -> core's "database closed" line
function Get-FailWord { [string]([char]0x5931) + [char]0x8D25 }
function Get-CoreClosedWord { [string]([char]0x6570) + [char]0x636E + [char]0x5E93 + [char]0x5DF2 + [char]0x5173 + [char]0x95ED }

# One PASS/FAIL line per check; the harness prints a summary and exits non-zero on any FAIL.
function Check([string]$Name, [bool]$Ok, [string]$Detail = '') {
    $script:Checks += [pscustomobject]@{ Name = $Name; Ok = $Ok; Detail = $Detail }
    if ($ok) {
        "PASS  $Name" + $(if ($Detail) { "  ($Detail)" } else { '' })
    } else {
        "FAIL  $Name" + $(if ($Detail) { "  ($Detail)" } else { '' })
    }
}

function Show-Summary {
    $bad = @($script:Checks | Where-Object { -not $_.Ok }).Count
    "---- $($script:Checks.Count) checks, $bad failed ----"
    if ($bad -gt 0) { exit 1 }
}

# Kill only the process started from the given exe path.
# The old Tauri app is also called FocusFlow.exe (it may sit in the tray on this machine),
# so killing by process name would take the user's legacy app down with us.
function Stop-App([string]$Exe = '') {
    if (-not $Exe) { $Exe = Get-DefaultExe }
    $full = [IO.Path]::GetFullPath($Exe)
    Get-Process -Name FocusFlow,focusflow-native -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and ([IO.Path]::GetFullPath($_.Path) -eq $full) } |
        Stop-Process -Force
    Start-Sleep -Milliseconds 350
}

# Fresh app dir: probe data + config, plus optional extra config lines (e.g. [collector]).
function New-AppDir([string]$Dest, [string[]]$ConfigAppend = @()) {
    $probe = Join-Path $script:RepoRoot 'desktop-native\probe'
    if (Test-Path $Dest) { Remove-Item $Dest -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $Dest | Out-Null
    Copy-Item (Join-Path $probe 'data') $Dest -Recurse
    Copy-Item (Join-Path $probe 'config.ini') (Join-Path $Dest 'config.ini')
    if ($ConfigAppend.Count -gt 0) {
        $cfg = Join-Path $Dest 'config.ini'
        $lines = [System.Collections.Generic.List[string]](Get-Content $cfg -Encoding UTF8)
        $lines.Add('')
        foreach ($l in $ConfigAppend) { $lines.Add($l) }
        # UTF-8 without BOM: the core config parser reads BOM-less UTF-8.
        [System.IO.File]::WriteAllLines($cfg, $lines, (New-Object System.Text.UTF8Encoding $false))
    }
}

function Start-App([string]$Exe, [string]$AppDir, [string]$WorkDir, [hashtable]$Env = @{}) {
    if (-not (Test-Path $Exe)) { throw "exe not found: $Exe" }
    $script:LastExe = $Exe
    New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null
    $psi = New-Object Diagnostics.ProcessStartInfo
    $psi.FileName = $Exe
    $psi.WorkingDirectory = $WorkDir
    $psi.UseShellExecute = $false
    $psi.EnvironmentVariables['FOCUSFLOW_APP_DIR'] = $AppDir
    $psi.EnvironmentVariables['FOCUSFLOW_ACCEPTANCE'] = '1'
    foreach ($k in $Env.Keys) { $psi.EnvironmentVariables[$k] = [string]$Env[$k] }
    [Diagnostics.Process]::Start($psi)
}

# When the main window never appears the usual cause is not a crash: another instance of the
# app (most often the user's own) holds the single-instance mutex, so our process raises the
# existing window and exits without writing a log line. Name that instance instead of leaving
# the suite with a silent one-check FAIL.
# Console output on purpose: Write-Output would land in the caller's $h = Wait-Window result.
function Get-Blocker([string]$Exe) {
    if (-not $Exe) { return '' }
    $full = [IO.Path]::GetFullPath($Exe)
    $others = @(Get-Process -Name FocusFlow,focusflow-native -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and ([IO.Path]::GetFullPath($_.Path) -ne $full) })
    if ($others.Count -eq 0) { return '' }
    ($others | ForEach-Object { 'pid=' + $_.Id + ' exe=' + $_.Path }) -join '; '
}

function Wait-Window([int]$ProcessId, [string]$Class = 'FocusFlowNativeD2D', [int]$TimeoutMs = 15000) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalMilliseconds -lt $TimeoutMs) {
        $h = [FF]::ByClass([uint32]$ProcessId, $Class)
        if ($h -ne [IntPtr]::Zero) { return $h }
        Start-Sleep -Milliseconds 100
    }
    $b = Get-Blocker $script:LastExe
    if ($b) { [Console]::Out.WriteLine('BLOCKER another instance holds the single-instance mutex: ' + $b) }
    [IntPtr]::Zero
}

function Get-Rect([IntPtr]$Hwnd) {
    $r = New-Object FF+RECT
    [void][FF]::GetWindowRect($Hwnd, [ref]$r)
    [pscustomobject]@{ X = $r.Left; Y = $r.Top; W = $r.Right - $r.Left; H = $r.Bottom - $r.Top }
}

function Get-Client([IntPtr]$Hwnd) {
    $r = New-Object FF+RECT
    [void][FF]::GetClientRect($Hwnd, [ref]$r)
    [pscustomobject]@{ W = $r.Right; H = $r.Bottom }
}

function Save-WindowShot([IntPtr]$Hwnd, [string]$Path) {
    $c = Get-Client $Hwnd
    $w = [Math]::Max($c.W, 1)
    $h = [Math]::Max($c.H, 1)
    $bmp = [System.Drawing.Bitmap]::new($w, $h)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc()
    $ok = [FF]::PrintWindow($Hwnd, $hdc, 2)   # PW_RENDERFULLCONTENT: DComp window
    $g.ReleaseHdc($hdc)
    $g.Dispose()
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Path) | Out-Null
    $bmp.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    $ok
}

function Save-RectShot([int]$X, [int]$Y, [int]$W, [int]$H, [string]$Path) {
    $bmp = [System.Drawing.Bitmap]::new([Math]::Max($W, 1), [Math]::Max($H, 1))
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($X, $Y, 0, 0, (New-Object Drawing.Size ([Math]::Max($W, 1)), ([Math]::Max($H, 1))))
    $g.Dispose()
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Path) | Out-Null
    $bmp.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
}

function Get-Memory([Diagnostics.Process]$Proc) {
    $Proc.Refresh()
    [pscustomobject]@{
        WsMb      = [math]::Round($Proc.WorkingSet64 / 1MB, 1)
        PrivateMb = [math]::Round($Proc.PrivateMemorySize64 / 1MB, 1)
        Threads   = $Proc.Threads.Count
    }
}

function Read-AppLog([string]$WorkDir) {
    $p = Join-Path $WorkDir 'focusflow.log'
    if (Test-Path $p) { Get-Content $p -Encoding UTF8 } else { @() }
}

# Viewer-tab / settings-row helpers (client coordinates, 100% DPI, 1184x821 client).
# Period pills: 16 + 66 + i*60, 48, 54x26 -> centre (109 + i*60, 61)
#   0 today, 1 7d, 2 15d, 3 30d, 4 1y, 5 all
function Click-PeriodPill([IntPtr]$Hwnd, [int]$Index) { [FF]::ClickWindow($Hwnd, 109 + $Index * 60, 61, 1); Start-Sleep -Milliseconds 500 }
function Click-ViewTab([IntPtr]$Hwnd, [int]$Index) { [FF]::ClickWindow($Hwnd, 56 + $Index * 84, 183, 1); Start-Sleep -Milliseconds 400 }
function Click-ATab([IntPtr]$Hwnd, [int]$Index) { [FF]::ClickWindow($Hwnd, 48 + $Index * 68, 270, 1); Start-Sleep -Milliseconds 400 }
# Basic rows in the first ("regular") category (current order): 0 autostart, 1 collector,
# 2 pause, 3 theme -- 44 px apart starting at y=260 (row centre 282).
function Click-BasicRow([IntPtr]$Hwnd, [int]$Index) { [FF]::ClickWindow($Hwnd, 320, 282 + $Index * 44, 1); Start-Sleep -Milliseconds 450 }

# Real mouse drag at absolute screen coordinates (LEFTDOWN / moves / LEFTUP).
# Used by the snip overlay, which is a full-virtual-screen window: at 100% DPI with the
# primary screen at the origin, screen coords are also its client coords. Injected *mouse*
# events do reach the app (injected keyboard events do not -- see README pitfall 1).
function Drag-Screen([int]$X1, [int]$Y1, [int]$X2, [int]$Y2) {
    [void][FF]::SetCursorPos($X1, $Y1)
    Start-Sleep -Milliseconds 150
    [FF]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)     # MOUSEEVENTF_LEFTDOWN
    Start-Sleep -Milliseconds 150
    # Two intermediate stops: WM_MOUSEMOVE has to see a drag, not a jump.
    foreach ($t in 35, 70) {
        $x = $X1 + [int](($X2 - $X1) * $t / 100)
        $y = $Y1 + [int](($Y2 - $Y1) * $t / 100)
        [void][FF]::SetCursorPos($x, $y)
        Start-Sleep -Milliseconds 90
    }
    [void][FF]::SetCursorPos($X2, $Y2)
    Start-Sleep -Milliseconds 150
    [FF]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)     # MOUSEEVENTF_LEFTUP
    Start-Sleep -Milliseconds 500
}
function Get-ThemeName([string]$AppDir) {
    $line = (Select-String -Path (Join-Path $AppDir 'config.ini') -Pattern '^theme' -Encoding UTF8 | ForEach-Object { $_.Line })
    ($line -join '')
}
function Get-CollectorEnabled([string]$AppDir) {
    $lines = Get-Content (Join-Path $AppDir 'config.ini') -Encoding UTF8
    $at = [Array]::IndexOf($lines, '[collector]')
    if ($at -lt 0) { return $false }
    for ($i = $at + 1; $i -lt $lines.Count -and $lines[$i] -notmatch '^\['; $i++) {
        if ($lines[$i] -match '^\s*enabled\s*=\s*(true|false)') { return $matches[1] -eq 'true' }
    }
    $false
}

# Rewrite one `key = value` line of config.ini (creates the line if the key is absent in
# the first occurrence of its section, else appends a section). Used by harnesses to set
# theme / floating position / collector switch before starting the app.
function Set-ConfigLine([string]$AppDir, [string]$Section, [string]$Key, [string]$Value) {
    $cfg = Join-Path $AppDir 'config.ini'
    $lines = [System.Collections.Generic.List[string]](Get-Content $cfg -Encoding UTF8)
    $secAt = $lines.IndexOf("[$Section]")
    $done = $false
    if ($secAt -ge 0) {
        for ($i = $secAt + 1; $i -lt $lines.Count -and -not $lines[$i].StartsWith('['); $i++) {
            if ($lines[$i] -match "^\s*$([regex]::Escape($Key))\s*=") {
                $lines[$i] = "$Key = $Value"
                $done = $true
                break
            }
        }
        if (-not $done) { $lines.Insert($secAt + 1, "$Key = $Value"); $done = $true }
    }
    if (-not $done) {
        $lines.Add('')
        $lines.Add("[$Section]")
        $lines.Add("$Key = $Value")
    }
    [System.IO.File]::WriteAllLines($cfg, $lines, (New-Object System.Text.UTF8Encoding $false))
}

# Latest machine-readable state line the app writes every second in acceptance mode:
#   "<CJK prefix> today=42 cpm=7 collector=on paused=false"
# (keys are ASCII, so parsing does not care about the file being UTF-8).
function Get-Status([string]$WorkDir) {
    $line = (Read-AppLog $WorkDir | Select-String -Pattern 'today=(\d+)\s+cpm=(-?\d+)\s+collector=(\w+)\s+paused=(\w+)' | Select-Object -Last 1)
    if (-not $line) { return $null }
    $m = [regex]::Match($line.Line, 'today=(\d+)\s+cpm=(-?\d+)\s+collector=(\w+)\s+paused=(\w+)')
    [pscustomobject]@{
        Today     = [int]$m.Groups[1].Value
        Cpm       = [int]$m.Groups[2].Value
        Collector = $m.Groups[3].Value
        Paused    = $m.Groups[4].Value -eq 'true'
    }
}

# Wait until a status line NEWER than the call appears (or the timeout passes).
#
# The app writes one status line per second, so a caller that sleeps less than a second
# after an action (pause / resume is 900 ms) would otherwise read the line written BEFORE
# that action and conclude it did not happen. Count the lines already there, then wait for
# one more -- the previous version returned any line, stale or not.
function Wait-Status([string]$WorkDir, [int]$TimeoutMs = 6000) {
    $seen = @(Read-AppLog $WorkDir | Select-String -Pattern 'today=').Count
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalMilliseconds -lt $TimeoutMs) {
        $lines = @(Read-AppLog $WorkDir | Select-String -Pattern 'today=')
        if ($lines.Count -gt $seen) { return Get-Status $WorkDir }
        Start-Sleep -Milliseconds 200
    }
    $null
}

# Most frequent colour in a PNG: background wins over text, so this is a robust way to
# answer "is this EDIT light or dark" without OCR.
function Get-DominantColor([string]$Path) {
    $bmp = [System.Drawing.Bitmap]::new($Path)
    $counts = @{}
    for ($y = 0; $y -lt $bmp.Height; $y += 2) {
        for ($x = 0; $x -lt $bmp.Width; $x += 2) {
            $c = $bmp.GetPixel($x, $y)
            $k = "$($c.R),$($c.G),$($c.B)"
            if ($counts.ContainsKey($k)) { $counts[$k]++ } else { $counts[$k] = 1 }
        }
    }
    $bmp.Dispose()
    $best = ($counts.GetEnumerator() | Sort-Object Value -Descending | Select-Object -First 1)
    $rgb = $best.Key -split ','
    [pscustomobject]@{ R = [int]$rgb[0]; G = [int]$rgb[1]; B = [int]$rgb[2]; Count = $best.Value }
}

function Get-ConfigValue([string]$AppDir, [string]$Section, [string]$Key) {
    $lines = Get-Content (Join-Path $AppDir 'config.ini') -Encoding UTF8
    $at = [Array]::IndexOf($lines, "[$Section]")
    if ($at -lt 0) { return $null }
    for ($i = $at + 1; $i -lt $lines.Count -and -not $lines[$i].StartsWith('['); $i++) {
        if ($lines[$i] -match "^\s*$([regex]::Escape($Key))\s*=\s*(.*)$") { return $matches[1].Trim() }
    }
    $null
}

# Move a window without resizing/activating (the pill persists its position on WM_EXITSIZEMOVE).
function Move-Window([IntPtr]$Hwnd, [int]$X, [int]$Y) {
    # SWP_NOSIZE(1) | SWP_NOZORDER(4) | SWP_NOACTIVATE(0x10)
    [void][FF]::SetWindowPos($Hwnd, [IntPtr]::Zero, $X, $Y, 0, 0, 0x15)
}

function Get-InstalledDir { Join-Path $env:LOCALAPPDATA 'Programs\FocusFlow' }

function Get-NewestSetup {
    $dist = Join-Path (Get-RepoRoot) 'dist'
    (Get-ChildItem $dist -Filter '*-setup.exe' -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1).FullName
}

# Silent install. -InstallDir empty = the shipped default (%LOCALAPPDATA%\Programs\FocusFlow).
function Invoke-Setup([string]$SetupPath, [string]$InstallDir = '') {
    $argList = @('/S')
    if ($InstallDir) { $argList += "/D=$InstallDir" }
    (Start-Process -FilePath $SetupPath -ArgumentList $argList -Wait -PassThru).ExitCode
}


