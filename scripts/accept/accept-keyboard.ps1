<#
.SYNOPSIS
  Keyboard navigation, long-text tooltips and right-click copy.

.DESCRIPTION
  Drives the real window with posted keys (Down/Enter/Esc), a real Ctrl+F chord, a real
  cursor hover over a clipped plugin name, and a right click that must put that name
  into the clipboard. The long-name fixture is written into a fresh probe copy, so
  nothing depends on real data and nothing outside the copy changes.
  ASCII-only, like every file in this folder.

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-keyboard.ps1
#>
param(
    [string]$Exe = '',
    [string]$WorkDir = ''
)

. "$PSScriptRoot\accept-lib.ps1"

$root = Get-RepoRoot
if (-not $WorkDir) { $WorkDir = Join-Path $root 'target\accept\keyboard' }
$Work = $WorkDir
if (-not $Exe) { $Exe = Get-DefaultExe }

Stop-App
$app = Join-Path $Work 'app'
New-AppDir $app @('[collector]', 'enabled = false')
$alias = 'VeryLongDeviceAliasNameForTooltipVerification'
$json = '{' + "`n" + '  "HID\\VID_046D&PID_C52B&MI_00": "' + $alias + '"' + "`n" + '}'
# BOM-less UTF-8: the alias file is parsed by serde_json, which rejects a BOM.
$utf8 = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllText((Join-Path $app 'data\device_aliases.json'), $json, $utf8)
# A plugin list entry whose name cannot fit the 240 px column -> the tooltip/copy target.
$builtin = Join-Path $app 'plugins\builtin'
New-Item -ItemType Directory -Force -Path $builtin | Out-Null
$longName = 'VeryLongPluginNameThatWillNeverFitInsideTheListColumn'
$lua = 'PLUGIN_NAME = "' + $longName + '"' + "`n" +
    'PLUGIN_DESC = "tooltip fixture"' + "`n" +
    'PLUGIN_VERSION = "1.0.0"' + "`n" +
    'function get_view() return { title = "t", widgets = { { type = "label", text = "x" } } } end' + "`n"
[System.IO.File]::WriteAllText((Join-Path $builtin 'aaa_tooltip.lua'), $lua, $utf8)
Remove-Item (Join-Path $Work 'focusflow.log') -ErrorAction SilentlyContinue

function Count-Diff([string]$A, [string]$B, [int]$X0, [int]$Y0, [int]$X1, [int]$Y1) {
    $ba = [System.Drawing.Bitmap]::FromFile($A)
    $bb = [System.Drawing.Bitmap]::FromFile($B)
    $n = 0
    for ($y = $Y0; $y -lt $Y1; $y += 2) {
        for ($x = $X0; $x -lt $X1; $x += 2) {
            $ca = $ba.GetPixel($x, $y)
            $cb = $bb.GetPixel($x, $y)
            $d = [Math]::Abs($ca.R - $cb.R) + [Math]::Abs($ca.G - $cb.G) + [Math]::Abs($ca.B - $cb.B)
            if ($d -gt 12) { $n++ }
        }
    }
    $ba.Dispose()
    $bb.Dispose()
    return $n
}

function Pixel([string]$Png, [int]$X, [int]$Y) {
    $b = [System.Drawing.Bitmap]::FromFile($Png)
    $c = $b.GetPixel($X, $Y)
    $b.Dispose()
    return $c
}

function Send-Key([IntPtr]$Hwnd, [int]$Vk) {
    [void][FF]::PostMessage($Hwnd, 0x0100, [IntPtr]$Vk, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 250
}

function Has-Visible-Edit([int]$ProcId) {
    $script:foundEdit = $false
    $cb = [FF+EnumWindowsProc]{ param($h,$l)
        $wp = [uint32]0
        [void][FF]::GetWindowThreadProcessId($h, [ref]$wp)
        if ($wp -eq [uint32]$ProcId) {
            $sb = New-Object System.Text.StringBuilder 256
            [void][FF]::GetClassName($h, $sb, 256)
            if ($sb.ToString() -eq 'FocusFlowEditHost') {
                $ch = [FF]::ChildByClassHandle($h.ToInt64(), 'Edit')
                if ($ch -ne 0) { $script:foundEdit = $true; return $false }
            }
        }
        return $true
    }
    [void][FF]::EnumWindows($cb, [IntPtr]::Zero)
    return $script:foundEdit
}

$p = Start-App $Exe $app $Work
$h = Wait-Window $p.Id
Check 'window appears' ($h -ne [IntPtr]::Zero)
if ($h -eq [IntPtr]::Zero) { Stop-App; Show-Summary }
Click-ViewTab $h $VIEW_DEVICES
Click-PeriodPill $h 5
Start-Sleep -Milliseconds 500

$plain = Join-Path $Work 'kbd-plain.png'
Save-WindowShot $h $plain | Out-Null
Start-Sleep -Milliseconds 200

# --- Down: focus ring on the first device row; Enter opens its detail -------------
Send-Key $h 0x28
$focus = Join-Path $Work 'kbd-focus.png'
Save-WindowShot $h $focus | Out-Null
$ring = Count-Diff $plain $focus 14 270 716 302
Check 'Down draws a keyboard focus ring on the row' ($ring -ge 20) "diff=$ring"

Send-Key $h 0x0D
$afterEnter = Join-Path $Work 'kbd-enter.png'
Save-WindowShot $h $afterEnter | Out-Null
$dim = Count-Diff $focus $afterEnter 900 700 1100 820
Check 'Enter opens the focused device detail' ($dim -ge 20) "diff=$dim"

# --- Esc closes the dialog, the next Esc only clears the focus ring ----------------
Send-Key $h 0x1B
Start-Sleep -Milliseconds 300
$vis1 = [bool][FF]::IsWindowVisible($h)
Check 'Esc closes the detail and keeps the window' $vis1 ''

Send-Key $h 0x1B
Start-Sleep -Milliseconds 400
$vis2 = [bool][FF]::IsWindowVisible($h)
$afterEsc = Join-Path $Work 'kbd-esc.png'
Save-WindowShot $h $afterEsc | Out-Null
# The ring paints 2 px outside the row band; row selection (from Enter) does not.
$edgeFocus = Pixel $focus 14 286
$edgeEsc = Pixel $afterEsc 14 286
$edgeD = [Math]::Abs($edgeFocus.R - $edgeEsc.R) + [Math]::Abs($edgeFocus.G - $edgeEsc.G) + [Math]::Abs($edgeFocus.B - $edgeEsc.B)
Check 'the second Esc clears the ring without hiding to tray' ($vis2 -and $edgeD -gt 12) "visible=$vis2 edge_delta=$edgeD"

# --- Ctrl+F: the settings search box gets a real focused EDIT ---------------------
[FF]::ClickScreen($h, 600, 15, 1)
Start-Sleep -Milliseconds 300
[FF]::PressChord([byte[]](0x11), 0x46)
Start-Sleep -Milliseconds 500
$search = Has-Visible-Edit $p.Id
Check 'Ctrl+F puts a visible search EDIT on screen' $search ''

# --- hover a clipped plugin name -> tooltip; right click -> copy the full text ---
Click-ViewTab $h $VIEW_PLUGINS
Start-Sleep -Milliseconds 800
$preTip = Join-Path $Work 'kbd-pretip.png'
Save-WindowShot $h $preTip | Out-Null
# The real cursor must sit on the cell: a posted WM_MOUSEMOVE is followed by a
# WM_MOUSELEAVE (the pointer really is elsewhere) and the hover state is cleared.
$wr = New-Object FF+RECT
[void][FF]::GetWindowRect($h, [ref]$wr)
[void][FF]::SetCursorPos(($wr.Left + 120), ($wr.Top + 224))
Start-Sleep -Milliseconds 1000
$tipShot = Join-Path $Work 'kbd-tooltip.png'
Save-WindowShot $h $tipShot | Out-Null
$tip = Count-Diff $preTip $tipShot 40 232 480 262
Check 'hovering a clipped plugin name shows a tooltip' ($tip -ge 20) "diff=$tip"

Set-Clipboard -Value 'sentinel'
[void][FF]::PostMessage($h, 0x0204, [IntPtr]::Zero, [IntPtr]((224 -shl 16) -bor 120))
Start-Sleep -Milliseconds 500
$clip = (Get-Clipboard -Raw)
Check 'right click copies the full name' ($clip -eq $longName) "clip=[$clip]"
[void][FF]::SetCursorPos(($wr.Left + 600), ($wr.Top + 700))

[void][FF]::PostMessage($h, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw = [Diagnostics.Stopwatch]::StartNew()
while (-not $p.HasExited -and $sw.Elapsed.TotalMilliseconds -lt 8000) { Start-Sleep -Milliseconds 150 }
Check 'quits cleanly' $p.HasExited ''
if (-not $p.HasExited) { Stop-App }

Show-Summary
