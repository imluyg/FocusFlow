<#
.SYNOPSIS
  Clipboard auto-paste (v2): after Enter copies an item, the app restores the window that
  was in front when the palette opened and injects Ctrl+V into it.

.DESCRIPTION
  One launch with no explicit [clipboard] auto_paste line: the default must be ON. The paste
  target is a TextBox in this script's own WinForms form (a foreign window), so the app has to:
    - remember the foreground window when the palette opens,
    - hide the palette, give the foreground back, confirm it really changed,
    - inject exactly Ctrl+V, and log clip_autopaste ok=true.

  The second half proves the self-window guard: opening the palette while the app's own
  main window is in front must NOT paste anywhere; the log has ok=false with the reason.
  In between, the target is minimized while the palette is open: Enter must restore it
  before the injection (otherwise a minimized window would be "pasted into" invisibly).

  Injected keys do reach a focused TextBox, so the content check is the real evidence —
  the log line alone would also be written if SendInput had been called and dropped.

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-clipautopaste.ps1
#>
param(
    [string]$Exe = '',
    [string]$WorkDir = ''
)

. "$PSScriptRoot\accept-lib.ps1"

if (-not $Exe) { $Exe = Get-DefaultExe }
if (-not $WorkDir) { $WorkDir = Join-Path (Get-RepoRoot) 'target\accept\clipautopaste' }

$app = Join-Path $WorkDir 'app'
$logPath = Join-Path $WorkDir 'focusflow.log'
$marker = 'accept-autopaste-marker'
$hotCombo = 'ctrl+shift+f9'
$hotVk = 0x78

function Get-LogLines {
    @(Read-AppLog $WorkDir)
}

function Wait-LogMatch([string]$Pattern, [int]$TimeoutMs = 6000) {
    $deadline = (Get-Date).AddMilliseconds($TimeoutMs)
    while ((Get-Date) -lt $deadline) {
        if (@(Get-LogLines | Where-Object { $_ -like $Pattern }).Count -gt 0) {
            return $true
        }
        Start-Sleep -Milliseconds 150
    }
    $false
}

function Wait-LogCount([string]$Pattern, [int]$Want, [int]$TimeoutMs = 5000) {
    $deadline = (Get-Date).AddMilliseconds($TimeoutMs)
    while ((Get-Date) -lt $deadline) {
        if (@(Get-LogLines | Where-Object { $_ -like $Pattern }).Count -ge $Want) {
            return $true
        }
        Start-Sleep -Milliseconds 150
    }
    $false
}

function Pump([int]$Ms) {
    $deadline = (Get-Date).AddMilliseconds($Ms)
    while ((Get-Date) -lt $deadline) {
        [System.Windows.Forms.Application]::DoEvents()
        Start-Sleep -Milliseconds 30
    }
}

function Set-ClipboardRetry([string]$Text) {
    for ($i = 0; $i -lt 20; $i++) {
        try { Set-Clipboard -Value $Text; return } catch { Start-Sleep -Milliseconds 150 }
    }
    throw "Set-Clipboard failed after retries"
}

Stop-App $Exe
New-AppDir $app @('[collector]', 'enabled = false')
Set-ConfigLine $app 'hotkey' 'clipboard' $hotCombo
Set-ConfigLine $app 'clipboard' 'enabled' 'true'
# Deliberately no auto_paste line: this suite is what proves the new default is ON.

Add-Type -AssemblyName System.Windows.Forms
$fx = New-Object System.Windows.Forms.Form
$fx.Text = 'FFPasteTarget'
$fx.FormBorderStyle = 'FixedDialog'
$fx.MaximizeBox = $false
$fx.MinimizeBox = $false
$fx.StartPosition = 'Manual'
$fx.Location = New-Object System.Drawing.Point(480, 260)
$fx.Size = New-Object System.Drawing.Size(420, 160)
$tb = New-Object System.Windows.Forms.TextBox
$tb.Multiline = $true
$tb.Dock = 'Fill'
$script:pasteKeys = New-Object 'System.Collections.Generic.List[string]'
$tb.Add_KeyDown({
    param($sender, $event)
    if ($event.KeyCode -eq [System.Windows.Forms.Keys]::V) {
        $script:pasteKeys.Add("ctrl=$($event.Control) alt=$($event.Alt) shift=$($event.Shift)")
    }
})
$fx.Controls.Add($tb)
# The app's main window would otherwise cover this fixture, and a click meant for the
# TextBox would land on the app. TopMost keeps the fixture visible for the click.
# Set it AFTER Show: WinForms drops TopMost set before the handle exists (probe measured
# WS_EX_TOPMOST=0 and the click landing on the window above), which made this suite flaky.
$fx.Show()
$fx.TopMost = $true
[System.Windows.Forms.Application]::DoEvents()

$orig = $null
try { $orig = Get-Clipboard -Raw } catch { }

Remove-Item $logPath -ErrorAction SilentlyContinue
"exe : $Exe"
"app : $app  (auto_paste defaults on, hotkey=$hotCombo)"

$p = Start-App $Exe $app $WorkDir
$h = Wait-Window $p.Id
Check 'main window appears' ($h -ne [IntPtr]::Zero)
if ($h -eq [IntPtr]::Zero) { Stop-App $Exe; Show-Summary }

Start-Sleep -Milliseconds 1500

# The paste target must be the window in front when the hotkey is pressed.
# Activate() alone loses to the foreground lock, so click the form like a user would:
# a real click also puts keyboard focus in the TextBox that takes the paste.
$fxRect = New-Object FF+RECT
[void][FF]::GetWindowRect($fx.Handle, [ref]$fxRect)
[FF]::ClickScreen($fx.Handle, [int](($fxRect.Right - $fxRect.Left) / 2), 60, 1)
Pump 400
Check 'fixture owns the foreground before the hotkey' `
    ([FF]::GetForegroundWindow() -eq $fx.Handle) `
    "fg=$([FF]::GetForegroundWindow()) fixture=$($fx.Handle)"

Set-ClipboardRetry $marker
Check 'the marker copy is captured' (Wait-LogMatch '*clip_ingest*' 6000)
Start-Sleep -Milliseconds 400

[FF]::PressChord([byte[]]@(0x11, 0x10), [byte]$hotVk)
Start-Sleep -Milliseconds 900
$popup = [FF]::ByClassHandle($p.Id, 'FocusFlowClipboardPalette')
Check 'clipboard palette opens for the auto-paste round' ($popup -gt 0)
$edit = 0
if ($popup -gt 0) { $edit = [FF]::ChildByClassHandle($popup, 'Edit') }
if ($edit -gt 0) {
    [FF]::PostMessage([IntPtr]$edit, 0x0100, [IntPtr]0x0D, [IntPtr]::Zero) | Out-Null
}
Pump 2500
Check 'auto-paste lands in the target textbox' ($tb.Text.Contains($marker)) `
    "textbox='$($tb.Text)'"
Check 'log reports a successful injection' (Wait-LogMatch '*clip_autopaste ok=true*' 3000)
# The injection landing in the TextBox already implies the focus came back; assert it
# directly too, because a lost focus with a stray Ctrl+V elsewhere would still be a bug.
Check 'the target window keeps the foreground after the paste' `
    ([FF]::GetForegroundWindow() -eq $fx.Handle) `
    "fg=$([FF]::GetForegroundWindow()) fixture=$($fx.Handle)"
# Without this, the refusal check below could be satisfied by a failure from THIS round.
Check 'no failed injection before the self round' `
    (@(Get-LogLines | Where-Object { $_ -like '*clip_autopaste ok=false*' }).Count -eq 0)

# --- minimized target: open the palette while the fixture is in front, then minimize it
# without touching the foreground. Enter must restore it before injecting Ctrl+V; without
# the restore the injection would go into a hidden window and the textbox would stay empty.
$tb.Clear()
[void][FF]::ClickScreen($fx.Handle, [int](($fxRect.Right - $fxRect.Left) / 2), 60, 1)
Pump 400
Check 'fixture owns the foreground before the minimized round' `
    ([FF]::GetForegroundWindow() -eq $fx.Handle) `
    "fg=$([FF]::GetForegroundWindow()) fixture=$($fx.Handle)"
[FF]::PressChord([byte[]]@(0x11, 0x10), [byte]$hotVk)
Start-Sleep -Milliseconds 900
$popup3 = [FF]::ByClassHandle($p.Id, 'FocusFlowClipboardPalette')
$edit3 = 0
if ($popup3 -gt 0) { $edit3 = [FF]::ChildByClassHandle($popup3, 'Edit') }
# SW_SHOWMINNOACTIVE = 7: iconic without changing the foreground, so the palette stays open.
[void][FF]::ShowWindow($fx.Handle, 7)
Pump 300
Check 'target is minimized before Enter' ([FF]::IsIconic($fx.Handle))
if ($edit3 -gt 0) {
    [FF]::PostMessage([IntPtr]$edit3, 0x0100, [IntPtr]0x0D, [IntPtr]::Zero) | Out-Null
}
Pump 3000
Check 'a minimized target is restored and pasted into' ($tb.Text.Contains($marker)) `
    "textbox='$($tb.Text)'"

# --- double-click round: a row double-click must go through the same finish path as
# Enter (copy + paste). A single click only selects now; that half lives in accept-clipboard.
$tb.Clear()
[void][FF]::ClickScreen($fx.Handle, [int](($fxRect.Right - $fxRect.Left) / 2), 60, 1)
Pump 400
Check 'fixture owns the foreground before the double-click round' `
    ([FF]::GetForegroundWindow() -eq $fx.Handle) `
    "fg=$([FF]::GetForegroundWindow()) fixture=$($fx.Handle)"
[FF]::PressChord([byte[]]@(0x11, 0x10), [byte]$hotVk)
Start-Sleep -Milliseconds 900
$popup4 = [FF]::ByClassHandle($p.Id, 'FocusFlowClipboardPalette')
$clickVisible = $false
if ($popup4 -gt 0) { $clickVisible = [FF]::IsWindowVisible([IntPtr]$popup4) }
Check 'palette opens for the double-click round' $clickVisible
if ($popup4 -gt 0) {
    $cr = New-Object FF+RECT
    [void][FF]::GetClientRect([IntPtr]$popup4, [ref]$cr)
    $scale = [FF]::GetDpiForSystem() / 96.0
    $cx = [int](($cr.Right - $cr.Left) / 2)
    # Row 1 center: list_top 92 + half a 36 px row, scaled exactly like `layout_for`.
    $cy = [int][Math]::Round((92 + 18) * $scale)
    $lp = [IntPtr](($cy * 65536) + ($cx -band 0xFFFF))
    [FF]::PostMessage([IntPtr]$popup4, 0x0203, [IntPtr]1, $lp) | Out-Null
}
Pump 2500
Check 'double-clicking a row auto-pastes into the target' ($tb.Text.Contains($marker)) `
    "textbox='$($tb.Text)'"
$hiddenAfterClick = $true
if ($popup4 -gt 0) { $hiddenAfterClick = -not [FF]::IsWindowVisible([IntPtr]$popup4) }
Check 'double-clicking a row closes the palette' $hiddenAfterClick

$tb.Clear()
$script:pasteKeys.Clear()
[void][FF]::ClickScreen($fx.Handle, [int](($fxRect.Right - $fxRect.Left) / 2), 60, 1)
Pump 300
Check 'target owns the foreground before held Alt selection' ([FF]::GetForegroundWindow() -eq $fx.Handle)
[FF]::PressChord([byte[]]@(0x11, 0x10), [byte]$hotVk)
Pump 900
$altPopup = [FF]::ByClassHandle($p.Id, 'FocusFlowClipboardPalette')
Check 'palette is visible before held Alt selection' ($altPopup -gt 0 -and [FF]::IsWindowVisible([IntPtr]$altPopup))
try {
    [FF]::keybd_event(0x12, 0, 0, [UIntPtr]::Zero)
    [FF]::keybd_event(0x31, 0, 0, [UIntPtr]::Zero)
    [FF]::keybd_event(0x31, 0, 2, [UIntPtr]::Zero)
    Pump 150
    Check 'no paste key is injected while Alt remains held' ($script:pasteKeys.Count -eq 0) ($script:pasteKeys -join '; ')
    Check 'held Alt does not insert content early' ($tb.Text.Length -eq 0)
} finally {
    [FF]::keybd_event(0x31, 0, 2, [UIntPtr]::Zero)
    [FF]::keybd_event(0x12, 0, 2, [UIntPtr]::Zero)
}
Pump 900
Check 'Alt selection pastes exactly once after release' ($tb.Text -eq $marker) "textbox='$($tb.Text)'"
Check 'Alt selection sends only unmodified Ctrl+V' `
    ($script:pasteKeys.Count -eq 1 -and $script:pasteKeys[0] -eq 'ctrl=True alt=False shift=False') `
    ($script:pasteKeys -join '; ')

$tb.Clear()
$script:pasteKeys.Clear()
[void][FF]::ClickScreen($fx.Handle, [int](($fxRect.Right - $fxRect.Left) / 2), 60, 1)
Pump 300
[FF]::PressChord([byte[]]@(0x11, 0x10), [byte]$hotVk)
Pump 900
$failBeforeHeld = @(Get-LogLines | Where-Object { $_ -like '*clip_autopaste ok=false*' }).Count
try {
    [FF]::keybd_event(0x12, 0, 0, [UIntPtr]::Zero)
    [FF]::keybd_event(0x31, 0, 0, [UIntPtr]::Zero)
    [FF]::keybd_event(0x31, 0, 2, [UIntPtr]::Zero)
    Pump 650
    Check 'held modifier timeout never injects a paste key' ($script:pasteKeys.Count -eq 0)
} finally {
    [FF]::keybd_event(0x31, 0, 2, [UIntPtr]::Zero)
    [FF]::keybd_event(0x12, 0, 2, [UIntPtr]::Zero)
}
Pump 500
Check 'releasing Alt after timeout cannot paste late' ($tb.Text.Length -eq 0 -and $script:pasteKeys.Count -eq 0)
Check 'modifier timeout reports a failed auto-paste' (Wait-LogCount '*clip_autopaste ok=false*' ($failBeforeHeld + 1) 2000)

$tb.Clear()
$script:pasteKeys.Clear()
[void][FF]::ClickScreen($fx.Handle, [int](($fxRect.Right - $fxRect.Left) / 2), 60, 1)
Pump 300
[FF]::PressChord([byte[]]@(0x11, 0x10), [byte]$hotVk)
Pump 900
$reopenPopup = [FF]::ByClassHandle($p.Id, 'FocusFlowClipboardPalette')
try {
    [FF]::keybd_event(0x10, 0, 0, [UIntPtr]::Zero)
    [FF]::PostMessage([IntPtr]$reopenPopup, 0x0100, [IntPtr]0x0D, [IntPtr]::Zero) | Out-Null
    Pump 70
    [FF]::keybd_event(0x11, 0, 0, [UIntPtr]::Zero)
    [FF]::keybd_event([byte]$hotVk, 0, 0, [UIntPtr]::Zero)
    [FF]::keybd_event([byte]$hotVk, 0, 2, [UIntPtr]::Zero)
    Pump 170
} finally {
    [FF]::keybd_event([byte]$hotVk, 0, 2, [UIntPtr]::Zero)
    [FF]::keybd_event(0x11, 0, 2, [UIntPtr]::Zero)
    [FF]::keybd_event(0x10, 0, 2, [UIntPtr]::Zero)
}
Pump 500
Check 'hotkey reopens the palette while paste waits on Shift' ([FF]::IsWindowVisible([IntPtr]$reopenPopup))
Check 'reopening cancels the pending paste' ($tb.Text.Length -eq 0 -and $script:pasteKeys.Count -eq 0)
$reopenEdit = [FF]::ChildByClassHandle($reopenPopup, 'Edit')
if ($reopenEdit -gt 0) {
    [FF]::PostMessage([IntPtr]$reopenEdit, 0x0100, [IntPtr]0x1B, [IntPtr]::Zero) | Out-Null
}
Pump 300

$tb.Clear()
$script:pasteKeys.Clear()
[void][FF]::ClickScreen($fx.Handle, [int](($fxRect.Right - $fxRect.Left) / 2), 60, 1)
Pump 300
[FF]::PressChord([byte[]]@(0x11, 0x10), [byte]$hotVk)
Pump 900
$other = New-Object System.Windows.Forms.Form
$other.Text = 'FFOtherPasteTarget'
$other.StartPosition = 'Manual'
$other.Location = New-Object System.Drawing.Point(480, 260)
$other.Size = New-Object System.Drawing.Size(420, 160)
$otherText = New-Object System.Windows.Forms.TextBox
$otherText.Multiline = $true
$otherText.Dock = 'Fill'
$other.Controls.Add($otherText)
$otherText.Add_KeyDown({
    param($sender, $event)
    if ($event.KeyCode -eq [System.Windows.Forms.Keys]::V) { $script:pasteKeys.Add('other-target-V') }
})
try {
    [FF]::keybd_event(0x12, 0, 0, [UIntPtr]::Zero)
    [FF]::keybd_event(0x31, 0, 0, [UIntPtr]::Zero)
    [FF]::keybd_event(0x31, 0, 2, [UIntPtr]::Zero)
    Pump 80
    $other.Show()
    $other.TopMost = $true
    [FF]::ClickScreen($other.Handle, 200, 60, 1)
    Pump 200
    Check 'another application target owns the foreground during the wait' ([FF]::GetForegroundWindow() -eq $other.Handle)
} finally {
    [FF]::keybd_event(0x31, 0, 2, [UIntPtr]::Zero)
    [FF]::keybd_event(0x12, 0, 2, [UIntPtr]::Zero)
}
Pump 500
Check 'changing windows during the wait never pastes into either target' `
    ($tb.Text.Length -eq 0 -and $otherText.Text.Length -eq 0 -and $script:pasteKeys.Count -eq 0)
$other.Close()
Pump 200

# --- self-window guard: the app's own main window was in front, so nothing may be pasted.
$failBefore = @(Get-LogLines | Where-Object { $_ -like '*clip_autopaste ok=false*' }).Count
$tb.Clear()
[void][FF]::SetForegroundWindow($h)
Pump 300
Check 'main window owns the foreground before the second round' `
    ([FF]::GetForegroundWindow() -eq $h) `
    "fg=$([FF]::GetForegroundWindow()) main=$h"
[FF]::PressChord([byte[]]@(0x11, 0x10), [byte]$hotVk)
Start-Sleep -Milliseconds 900
$popup2 = [FF]::ByClassHandle($p.Id, 'FocusFlowClipboardPalette')
$edit2 = 0
if ($popup2 -gt 0) { $edit2 = [FF]::ChildByClassHandle($popup2, 'Edit') }
if ($edit2 -gt 0) {
    [FF]::PostMessage([IntPtr]$edit2, 0x0100, [IntPtr]0x0D, [IntPtr]::Zero) | Out-Null
}
Pump 1500
Check 'the self-window target is refused' `
    (Wait-LogCount '*clip_autopaste ok=false*' ($failBefore + 1) 4000)
Check 'nothing is pasted into our own window' ($tb.Text.Length -eq 0) `
    "textbox='$($tb.Text)'"
# A refusal must stay visible: the user has to see why nothing was pasted, and the
# content is already on the clipboard for a manual Ctrl+V.
$stillOpen = $false
if ($popup2 -gt 0) { $stillOpen = [FF]::IsWindowVisible([IntPtr]$popup2) }
Check 'the palette stays open when the paste is refused' $stillOpen
Check 'the refusal log says the palette was kept' `
    (Wait-LogMatch '*clip_autopaste ok=false*kept=1*' 2000)

Stop-App $Exe
$fx.Close()
if ($null -ne $orig) { try { Set-ClipboardRetry $orig } catch { } }
Show-Summary
