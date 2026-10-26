<#
.SYNOPSIS
  Plugin view interactions: modal forms open, selects write back, submit/cancel reach the plugin.

.DESCRIPTION
  The plugin protocol is declarative (Widget::Select / Widget::ModalForm); the host has to turn
  it into real clicks. This harness drives plugins/accounting_plugin.lua from a probe copy and
  asserts on the app log. The log lines are Chinese, so every pattern below is an ASCII
  fragment that only that one action writes (a field name or an action id) -- keep it that way
  when adding checks, and keep this file ASCII-only.

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-plugins.ps1
#>
param(
    [string]$Exe = '',
    [string]$WorkDir = ''
)

. "$PSScriptRoot\accept-lib.ps1"

if (-not $Exe) { $Exe = Get-DefaultExe }
if (-not $WorkDir) { $WorkDir = Join-Path (Get-RepoRoot) 'target\accept\plugins' }

Stop-App
$app = Join-Path $WorkDir 'app'
New-AppDir $app @('[collector]', 'enabled = false')
# The probe copy only carries its own plugins; add the shipped ones so accounting_plugin is there.
$dst = Join-Path $app 'plugins'
New-Item -ItemType Directory -Force -Path $dst | Out-Null
Copy-Item (Join-Path (Get-RepoRoot) 'plugins\*.lua') $dst -Force
Copy-Item (Join-Path (Get-RepoRoot) 'desktop-native\probe\plugins\*.lua') $dst -Force
Remove-Item (Join-Path $WorkDir 'focusflow-native.log') -ErrorAction SilentlyContinue
"exe : $Exe"
"app : $app"

function Wait-Log([string]$Pattern, [int]$TimeoutMs = 6000) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalMilliseconds -lt $TimeoutMs) {
        if (@(Read-AppLog $WorkDir) -match $Pattern) { return $true }
        Start-Sleep -Milliseconds 100
    }
    $false
}

$p = Start-App $Exe $app $WorkDir
$h = Wait-Window $p.Id
Check 'main window appears' ($h -ne [IntPtr]::Zero)
if ($h -eq [IntPtr]::Zero) { Stop-App; Show-Summary }

Click-ViewTab $h 4
Start-Sleep -Milliseconds 600
[FF]::ClickWindow($h, 200, 270, 1)     # first row = accounting_plugin
Start-Sleep -Milliseconds 600
$shot = Join-Path $WorkDir 'plugin-view.png'
Check 'plugin view renders' (Save-WindowShot $h $shot) $shot

[FF]::ClickWindow($h, 60, 275, 1)      # + add record: button carries modal="add_modal"
Check 'button.modal opens the modal form' (Wait-Log 'add_modal') ''

[FF]::ClickWindow($h, 520, 270, 1)     # the type select inside the modal
Start-Sleep -Milliseconds 300
[FF]::ClickWindow($h, 520, 325, 1)     # second option
Check 'select inside the modal writes the field' (Wait-Log 'd_type = ') ''

[FF]::ClickWindow($h, 816, 583, 1)     # save
Check 'modal submit runs the plugin action' (Wait-Log 'add_record') ''

# View-level filter select (category value box sits at x 185..445).
Start-Sleep -Milliseconds 400
[FF]::ClickWindow($h, 300, 588, 1)
Start-Sleep -Milliseconds 300
[FF]::ClickWindow($h, 300, 363, 1)
Check 'view-level select writes the field' (Wait-Log 'f_cat = ') ''

# TextInput inside a modal still uses the inline EDIT.
[FF]::ClickWindow($h, 60, 275, 1)
Start-Sleep -Milliseconds 500
[FF]::ClickWindow($h, 520, 301, 1)
Check 'modal text field opens the inline editor' (Wait-Log 'd_item') ''
[void][FF]::PostMessage($h, 0x0100, [IntPtr]0x1B, [IntPtr]::Zero)   # Esc = cancel
Start-Sleep -Milliseconds 400

# A content-only modal (monthly summary), opened by the plugin itself (open=true).
[FF]::ClickWindow($h, 60, 427, 1)
Check 'plugin can open a content modal' (Wait-Log 'monthly_detail') ''
[FF]::ClickWindow($h, 816, 500, 1)     # close
Check 'content modal close runs its action' (Wait-Log 'clear_result') ''

$log = Read-AppLog $WorkDir
Check 'no error lines in the app log' (-not (@($log) -match "(?i)error|$(Get-FailWord)")) ''

[void][FF]::PostMessage($h, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw = [Diagnostics.Stopwatch]::StartNew()
while (-not $p.HasExited -and $sw.Elapsed.TotalMilliseconds -lt 8000) { Start-Sleep -Milliseconds 150 }
Check 'quits within 8 s' $p.HasExited "$([int]$sw.Elapsed.TotalMilliseconds) ms"
if (-not $p.HasExited) { Stop-App }

Show-Summary
