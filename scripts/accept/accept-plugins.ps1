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
# The shipped copy lives with the core crate; the repo-root plugins/ twin was deleted by name
# on 2026-10-30, so pointing here is the only path that exists on a fresh clone.
Copy-Item (Join-Path (Get-RepoRoot) 'crates\core\plugins\*.lua') $dst -Force
Copy-Item (Join-Path (Get-RepoRoot) 'desktop-native\probe\plugins\*.lua') $dst -Force
Remove-Item (Join-Path $WorkDir 'focusflow.log') -ErrorAction SilentlyContinue
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

# Modal dialogs are centred on the client; the native caption is gone, so the client is
# 1200x860 instead of 1184x821. Modal-internal clicks below were measured on the old
# client, so shift them by half the client delta (modal size itself is fixed).
$cl = Get-Client $h
$mx = [int](($cl.W - 1184) / 2)
$my = [int](($cl.H - 821) / 2)

[FF]::ClickWindow($h, 60, 275, 1)      # + add record: button carries modal="add_modal"
Check 'button.modal opens the modal form' (Wait-Log 'add_modal') ''

[FF]::ClickWindow($h, (520 + $mx), (270 + $my), 1)     # the type select inside the modal
Start-Sleep -Milliseconds 300
[FF]::ClickWindow($h, (520 + $mx), (325 + $my), 1)     # second option
Check 'select inside the modal writes the field' (Wait-Log 'd_type = ') ''

[FF]::ClickWindow($h, (816 + $mx), (583 + $my), 1)     # save
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
[FF]::ClickWindow($h, (520 + $mx), (301 + $my), 1)
Check 'modal text field opens the inline editor' (Wait-Log 'd_item') ''
[void][FF]::PostMessage($h, 0x0100, [IntPtr]0x1B, [IntPtr]::Zero)   # Esc = cancel
Start-Sleep -Milliseconds 400

# A content-only modal (monthly summary), opened by the plugin itself (open=true).
[FF]::ClickWindow($h, 60, 427, 1)
Check 'plugin can open a content modal' (Wait-Log 'monthly_detail') ''
[FF]::ClickWindow($h, (816 + $mx), (500 + $my), 1)     # close
Check 'content modal close runs its action' (Wait-Log 'clear_result') ''

$log = Read-AppLog $WorkDir
Check 'no error lines in the app log' (-not (@($log) -match "(?i)error|$(Get-FailWord)")) ''

[void][FF]::PostMessage($h, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw = [Diagnostics.Stopwatch]::StartNew()
while (-not $p.HasExited -and $sw.Elapsed.TotalMilliseconds -lt 8000) { Start-Sleep -Milliseconds 150 }
Check 'quits within 8 s' $p.HasExited "$([int]$sw.Elapsed.TotalMilliseconds) ms"
if (-not $p.HasExited) { Stop-App }

# ---- phase 2: the batched key/mouse dispatch must reach a loaded plugin in a real process.
#
# What this covers and what it does not: the panel's number is drawn text with no machine
# readable readout, so "a human sees a non-zero count there" is still an eye-check. What is
# asserted here are the two things that were missing before the wiring -- plugins are loaded at
# startup (nobody has to open the page first), and after real injected input plus one writer
# flush the host hands the batch over to that many loaded plugins. The Lua round trip itself
# (one call per key name, carrying the aggregated count) is pinned headlessly by core's
# a_batch_of_two_keys_reaches_the_hook_twice_with_its_counts.
#
# The two readouts are matched by their ASCII tails: PowerShell 5.1 reads a BOM-less UTF-8 .ps1
# as ANSI, so a Chinese regex in this file would break the matcher itself.
# Section name WITHOUT brackets: Set-ConfigLine writes "<section> = value" under a
# "[<section>]" header, so passing '[collector]' produced "[[collector]]" -- a header the parser
# reads as a section literally named "[collector", leaving the collector OFF and this phase
# measuring nothing. That is how it first presented: "no batch was ever dispatched".
Set-ConfigLine $app 'collector' 'enabled' 'true'
Remove-Item (Join-Path $WorkDir 'focusflow.log') -ErrorAction SilentlyContinue
$p2 = Start-App $Exe $app $WorkDir
$h2 = Wait-Window $p2.Id
Check 'phase 2: window appears' ($h2 -ne [IntPtr]::Zero)
# Poll: the file logger is not guaranteed to have written the startup line by the time the
# window exists, and a single read here raced (the line was there in the log afterwards).
$loaded = @()
$deadline = (Get-Date).AddSeconds(20)
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 1
    $loaded = @(Read-AppLog $WorkDir | Where-Object { $_ -match 'plugins_loaded=[1-9]' })
    if ($loaded.Count -ge 1) { break }
}
Check 'plugins load at startup (no page visit needed)' ($loaded.Count -ge 1) ($loaded -join ' | ')
$st = @(Read-AppLog $WorkDir | Where-Object { $_ -match 'collector=' } | Select-Object -Last 1)
# -match on an array is a *filter* (returns the array), and Check takes [bool] -- bind it out.
$on = @($st | Where-Object { $_ -match 'collector=on' }).Count -ge 1
Check 'phase 2: the collector really is on' $on ($st -join '')
if ($h2 -ne [IntPtr]::Zero) {
    [FF]::ClickScreen($h2, 1150, 700, 10)
}
$batch = @()
$deadline = (Get-Date).AddSeconds(80)
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 3
    $batch = @(Read-AppLog $WorkDir | Where-Object { $_ -match 'keybatch_lines=' })
    if ($batch.Count -ge 1) { break }
}
$first = ($batch | Select-Object -First 1)
Check 'one real flush handed its batch over and it was dispatched' ($batch.Count -ge 1) $first
$noRecipient = @($batch | Where-Object { $_ -match 'plugins=0' })
Check 'the dispatch had a recipient (plugins=0 would be a silent no-op)' (
    ($batch.Count -ge 1) -and ($noRecipient.Count -eq 0)) $first
Stop-App

Show-Summary
