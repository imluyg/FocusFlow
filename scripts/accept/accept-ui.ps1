<#
.SYNOPSIS
  Visual-parameter regression: card elevation, zebra/hover separation, hand cursor on
  controls, and the tab underline following the label width.

.DESCRIPTION
  Runs with FOCUSFLOW_NO_GLASS=1 so the opaque (non-Mica) branch is exercised: shadows are
  deliberately not painted behind translucent cards, so a glass run would measure nothing.
  Card coordinates are found by scanning the captured frame for the card fill, never by
  hard-coding the non-client offset that PrintWindow adds.

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-ui.ps1
#>
param(
    [string]$Exe = '',
    [string]$WorkDir = ''
)

. "$PSScriptRoot\accept-lib.ps1"
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class FFUI {
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X; public int Y; }
  [StructLayout(LayoutKind.Sequential)] public struct CURSORINFO { public int cbSize; public int flags; public IntPtr hCursor; public POINT pt; }
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern bool GetCursorInfo(ref CURSORINFO ci);
  [DllImport("user32.dll")] public static extern IntPtr LoadCursor(IntPtr h, int name);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
  [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr h);
}
"@

if (-not $Exe) { $Exe = Get-DefaultExe }
if (-not $WorkDir) { $WorkDir = Join-Path (Get-RepoRoot) 'target\accept\ui' }
$app = Join-Path $WorkDir 'app'

function Shot-Pixel($Bmp, [int]$X, [int]$Y) { return $Bmp.GetPixel($X, $Y) }

# Image offset that PrintWindow adds (it prints the whole window into a client-sized bitmap).
# The card corner is rounded, so measure the top edge at x=200 and the left edge 36 px below
# the top -- scanning from the corner would land inside the arc and report a shifted origin.
function Find-CardOrigin($Bmp, [int]$WantR, [int]$WantG, [int]$WantB, [int]$Tol) {
    $oy = -1
    # Start above the card top: with the native caption gone the captured bitmap can begin
    # at client y=0, so starting at 100 would miss the top edge and report a false offset.
    for ($y = 60; $y -lt 320; $y++) {
        # x=60 is inside card 1 but left of the period pills (they start at x=82): the idle
        # pills are card-fill coloured and would win the scan for the card top.
        $c = $Bmp.GetPixel(60, $y)
        if ([Math]::Abs($c.R - $WantR) -le $Tol -and [Math]::Abs($c.G - $WantG) -le $Tol -and [Math]::Abs($c.B - $WantB) -le $Tol) {
            $oy = $y - 84
            break
        }
    }
    if ($oy -lt 0) { return @(-1, -1) }
    $ox = -1
    for ($x = 0; $x -lt 260; $x++) {
        $c = $Bmp.GetPixel($x, ($oy + 120))
        if ([Math]::Abs($c.R - $WantR) -le $Tol -and [Math]::Abs($c.G - $WantG) -le $Tol -and [Math]::Abs($c.B - $WantB) -le $Tol) {
            $ox = $x - 16
            break
        }
    }
    return @($ox, $oy)
}

function Cursor-Kind([IntPtr]$Hwnd, [int]$X, [int]$Y) {
    $pt = New-Object FFUI+POINT
    $pt.X = $X; $pt.Y = $Y
    [void][FFUI]::ClientToScreen($Hwnd, [ref]$pt)
    [void][FFUI]::SetCursorPos($pt.X, $pt.Y)
    Start-Sleep -Milliseconds 320
    $ci = New-Object FFUI+CURSORINFO
    $ci.cbSize = [System.Runtime.InteropServices.Marshal]::SizeOf($ci)
    [void][FFUI]::GetCursorInfo([ref]$ci)
    $hand = [FFUI]::LoadCursor([IntPtr]::Zero, 32649)
    $arrow = [FFUI]::LoadCursor([IntPtr]::Zero, 32512)
    if ($ci.hCursor -eq $hand) { return 'hand' }
    if ($ci.hCursor -eq $arrow) { return 'arrow' }
    return 'other'
}

function Move-CursorAway([IntPtr]$Hwnd) {
    $pt = New-Object FFUI+POINT
    $pt.X = 600; $pt.Y = 700
    [void][FFUI]::ClientToScreen($Hwnd, [ref]$pt)
    [void][FFUI]::SetCursorPos($pt.X, $pt.Y)
}

function Count-Accent($Bmp, [int]$Y, [int]$X0, [int]$X1) {
    $n = 0
    for ($x = $X0; $x -le $X1; $x++) {
        $c = $Bmp.GetPixel($x, $Y)
        if ($c.B -gt 180 -and $c.B -gt ($c.R + 40) -and $c.G -gt ($c.R + 20)) { $n++ }
    }
    return $n
}

Stop-App
New-AppDir $app
Set-ConfigLine $app 'gui' 'theme' 'light'
Remove-Item (Join-Path $WorkDir 'focusflow.log') -ErrorAction SilentlyContinue

# ---- light, opaque: shadows must be visible ------------------------------------------
$p = Start-App $Exe $app $WorkDir @{ 'FOCUSFLOW_NO_GLASS' = '1' }
$h = Wait-Window $p.Id
Check 'main window appears' ($h -ne [IntPtr]::Zero)
if ($h -eq [IntPtr]::Zero) { Stop-App; Show-Summary }
Start-Sleep -Milliseconds 1400
[void][FFUI]::SetForegroundWindow($h)
Move-CursorAway $h
Start-Sleep -Milliseconds 300
$lightShot = Join-Path $WorkDir 'ui-light.png'
Save-WindowShot $h $lightShot | Out-Null
$bl = [System.Drawing.Bitmap]::FromFile($lightShot)
$o = Find-CardOrigin $bl 255 255 255 2
Check 'the first stat card is found in the frame' ($o[0] -ge 0) "offset=$($o[0]),$($o[1])"
$ox = $o[0]; $oy = $o[1]
$page = Shot-Pixel $bl (4 + $ox) (150 + $oy)
$near = Shot-Pixel $bl (12 + $ox) (150 + $oy)
$card = Shot-Pixel $bl (220 + $ox) (150 + $oy)
$delta = ($page.R - $near.R) + ($page.G - $near.G) + ($page.B - $near.B)
Check 'opaque cards cast a visible shadow' ($delta -ge 6) "page=$($page.R),$($page.G),$($page.B) shadow=$($near.R),$($near.G),$($near.B) delta=$delta"
Check 'the shadow stays outside the card' ($card.R -ge 250 -and $card.G -ge 250 -and $card.B -ge 250) "card=$($card.R),$($card.G),$($card.B)"

# ---- hover tint + cursor shape -------------------------------------------------------
$before = Shot-Pixel $bl (150 + $ox) (61 + $oy)
$kindPill = Cursor-Kind $h 169 61
$hoverShot = Join-Path $WorkDir 'ui-hover.png'
Start-Sleep -Milliseconds 400
Save-WindowShot $h $hoverShot | Out-Null
$bh = [System.Drawing.Bitmap]::FromFile($hoverShot)
$after = Shot-Pixel $bh (150 + $ox) (61 + $oy)
$hd = [Math]::Abs($after.R - $before.R) + [Math]::Abs($after.G - $before.G) + [Math]::Abs($after.B - $before.B)
Check 'an idle pill changes colour on hover' ($hd -ge 6) "before=$($before.R),$($before.G),$($before.B) after=$($after.R),$($after.G),$($after.B) delta=$hd"
$bh.Dispose()
Check 'the period pill shows a hand cursor' ($kindPill -eq 'hand') "cursor=$kindPill"
$kindTab = Cursor-Kind $h 56 183
Check 'a view tab shows a hand cursor' ($kindTab -eq 'hand') "cursor=$kindTab"
$kindEmpty = Cursor-Kind $h 600 400
Check 'empty space keeps the arrow' ($kindEmpty -eq 'arrow') "cursor=$kindEmpty"

# ---- underline follows the label ------------------------------------------------------
$yUnd = 196 + $oy
$w0 = Count-Accent $bl $yUnd (16 + $ox) (96 + $ox)
Check 'the active tab draws an underline' ($w0 -ge 20) "accent_pixels=$w0"
Click-ViewTab $h 5
Start-Sleep -Milliseconds 600
$setShot = Join-Path $WorkDir 'ui-settings.png'
Save-WindowShot $h $setShot | Out-Null
$bs = [System.Drawing.Bitmap]::FromFile($setShot)
$w5 = Count-Accent $bs $yUnd (436 + $ox) (516 + $ox)
Check 'the underline width follows the label (2 chars vs 4)' (($w0 - $w5) -ge 8) "four_chars=$w0 two_chars=$w5"
$bs.Dispose()
$bl.Dispose()

# ---- custom title bar: drag band, buttons, resize edges -------------------------------
function HitTest([IntPtr]$Hwnd, [int]$X, [int]$Y) {
    $pt = New-Object FFUI+POINT
    $pt.X = $X; $pt.Y = $Y
    [void][FFUI]::ClientToScreen($Hwnd, [ref]$pt)
    $lp = [IntPtr](($pt.Y -shl 16) -bor ($pt.X -band 0xFFFF))
    return [int][FF]::SendMessage($Hwnd, 0x0084, [IntPtr]::Zero, $lp)
}
$cl = Get-Client $h
$htTitle = HitTest $h 300 20
$htClose = HitTest $h ($cl.W - 20) 20
$htLeft = HitTest $h 2 300
$htBody = HitTest $h 300 300
Check 'the title band answers HTCAPTION' ($htTitle -eq 2) "ht=$htTitle"
Check 'the close button answers HTCLIENT' ($htClose -eq 1) "ht=$htClose"
Check 'the left edge answers HTLEFT' ($htLeft -eq 10) "ht=$htLeft"
Check 'the body answers HTCLIENT' ($htBody -eq 1) "ht=$htBody"

# Window buttons keep the arrow cursor (only page controls get the hand).
$kindClose = Cursor-Kind $h ($cl.W - 20) 20
Check 'window buttons keep the arrow' ($kindClose -eq 'arrow') "cursor=$kindClose"
Move-CursorAway $h
Start-Sleep -Milliseconds 350
$titleBase = Join-Path $WorkDir 'ui-title-base.png'
Save-WindowShot $h $titleBase | Out-Null
$bt = [System.Drawing.Bitmap]::FromFile($titleBase)
$basePx = Shot-Pixel $bt ($cl.W - 40) 8
$bt.Dispose()
[void](Cursor-Kind $h ($cl.W - 20) 20)
Start-Sleep -Milliseconds 400
$titleHover = Join-Path $WorkDir 'ui-title-hover.png'
Save-WindowShot $h $titleHover | Out-Null
$th = [System.Drawing.Bitmap]::FromFile($titleHover)
$hotPx = Shot-Pixel $th ($cl.W - 40) 8
$th.Dispose()
$cd = [Math]::Abs($hotPx.R - $basePx.R) + [Math]::Abs($hotPx.G - $basePx.G) + [Math]::Abs($hotPx.B - $basePx.B)
Check 'the close button turns red on hover' ($cd -ge 40) "base=$($basePx.R),$($basePx.G),$($basePx.B) hot=$($hotPx.R),$($hotPx.G),$($hotPx.B)"

# Maximize / restore through the middle button.
[FF]::ClickWindow($h, $cl.W - 69, 20, 1)
Start-Sleep -Milliseconds 700
$max = Get-Client $h
Check 'maximize grows the client area' ($max.W -gt $cl.W -and $max.H -gt $cl.H) "before=$($cl.W)x$($cl.H) after=$($max.W)x$($max.H)"
[FF]::ClickWindow($h, $max.W - 69, 20, 1)
Start-Sleep -Milliseconds 700
$rest = Get-Client $h
Check 'restore returns the 1200x860 client' ($rest.W -eq 1200 -and $rest.H -eq 860) "client=$($rest.W)x$($rest.H)"

# Minimize / restore through the left button.
[FF]::ClickWindow($h, $rest.W - 115, 20, 1)
Start-Sleep -Milliseconds 700
$mini = [FFUI]::IsIconic($h)
Check 'minimize sends the window down' ([bool]$mini) ''
[void][FFUI]::ShowWindow($h, 9)   # SW_RESTORE
Start-Sleep -Milliseconds 700
Check 'restore brings it back' (-not [FFUI]::IsIconic($h)) ''

# ---- dark, opaque: no shadow, top highlight instead -----------------------------------
[void][FF]::PostMessage($h, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw = [Diagnostics.Stopwatch]::StartNew()
while (-not $p.HasExited -and $sw.Elapsed.TotalMilliseconds -lt 8000) { Start-Sleep -Milliseconds 120 }
if (-not $p.HasExited) { Stop-App }
Set-ConfigLine $app 'gui' 'theme' 'dark'
$p2 = Start-App $Exe $app $WorkDir @{ 'FOCUSFLOW_NO_GLASS' = '1' }
$h2 = Wait-Window $p2.Id
Start-Sleep -Milliseconds 1400
$darkShot = Join-Path $WorkDir 'ui-dark.png'
Save-WindowShot $h2 $darkShot | Out-Null
$bd = [System.Drawing.Bitmap]::FromFile($darkShot)
$od = Find-CardOrigin $bd 33 39 54 2
Check 'the dark card fill is found in the frame' ($od[0] -ge 0) "offset=$($od[0]),$($od[1])"
if ($od[0] -ge 0) {
    $dx = $od[0]; $dy = $od[1]
    $dPage = Shot-Pixel $bd (4 + $dx) (150 + $dy)
    $dNear = Shot-Pixel $bd (12 + $dx) (150 + $dy)
    $dSame = [Math]::Abs($dPage.R - $dNear.R) + [Math]::Abs($dPage.G - $dNear.G) + [Math]::Abs($dPage.B - $dNear.B)
    Check 'the dark theme paints no shadow' ($dSame -le 2) "page=$($dPage.R),$($dPage.G),$($dPage.B) near=$($dNear.R),$($dNear.G),$($dNear.B)"
    # $dy is found from the first *card-fill* row, so the highlight line sits one row above it.
    $top = Shot-Pixel $bd (200 + $dx) ($dy + 83)
    $body = Shot-Pixel $bd (200 + $dx) ($dy + 97)
    $hl = ($top.R - $body.R) + ($top.G - $body.G) + ($top.B - $body.B)
    Check 'the dark theme has a top highlight' ($hl -ge 8) "top=$($top.R),$($top.G),$($top.B) body=$($body.R),$($body.G),$($body.B)"
}
$bd.Dispose()

[void][FF]::PostMessage($h2, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw2 = [Diagnostics.Stopwatch]::StartNew()
while (-not $p2.HasExited -and $sw2.Elapsed.TotalMilliseconds -lt 8000) { Start-Sleep -Milliseconds 120 }
Check 'quits cleanly' $p2.HasExited "$([int]$sw2.Elapsed.TotalMilliseconds) ms"
if (-not $p2.HasExited) { Stop-App }
$log = Read-AppLog $WorkDir
Check 'no error lines in the app log' (-not (@($log) -match "(?i)error|$(Get-FailWord)")) ''

Show-Summary
