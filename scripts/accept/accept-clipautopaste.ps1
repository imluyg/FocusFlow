<#
.SYNOPSIS
  Clipboard auto-paste (v2): after Enter copies an item, the app restores the window that
  was in front when the palette opened and injects Ctrl+V into it.

.DESCRIPTION
  One launch with [clipboard] auto_paste = true. The paste target is a TextBox in this
  script's own WinForms form (a foreign window), so the app has to:
    - remember the foreground window when the palette opens,
    - hide the palette, give the foreground back, confirm it really changed,
    - inject exactly Ctrl+V, and log clip_autopaste ok=true.

  The second half proves the self-window guard: opening the palette while the app's own
  main window is in front must NOT paste anywhere; the log has ok=false with the reason.

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
Set-ConfigLine $app 'clipboard' 'auto_paste' 'true'

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
$fx.Controls.Add($tb)
# The app's main window would otherwise cover this fixture, and a click meant for the
# TextBox would land on the app. TopMost keeps the fixture visible for the click.
$fx.TopMost = $true
$fx.Show()

$orig = $null
try { $orig = Get-Clipboard -Raw } catch { }

Remove-Item $logPath -ErrorAction SilentlyContinue
"exe : $Exe"
"app : $app  (auto_paste=true, hotkey=$hotCombo)"

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
# Without this, the refusal check below could be satisfied by a failure from THIS round.
Check 'no failed injection before the self round' `
    (@(Get-LogLines | Where-Object { $_ -like '*clip_autopaste ok=false*' }).Count -eq 0)

# --- self-window guard: the app's own main window was in front, so nothing may be pasted.
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
Check 'the self-window target is refused' (Wait-LogMatch '*clip_autopaste ok=false*' 4000)
Check 'nothing is pasted into our own window' ($tb.Text.Length -eq 0) `
    "textbox='$($tb.Text)'"

Stop-App $Exe
$fx.Close()
if ($null -ne $orig) { try { Set-ClipboardRetry $orig } catch { } }
Show-Summary
