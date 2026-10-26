<#
.SYNOPSIS
  Plugin table selection + button.sel: the ids/group/onselect a Lua table declares must
  reach the host, and a sel button must send "<button id><selected id>" to the plugin.

.DESCRIPTION
  Writes its own tiny ASCII plugin (SelFixture) into a fresh COPY of desktop-native\probe,
  so nothing depends on ledger data existing and nothing touches real data.

  The app hook FOCUSFLOW_PLUGIN_SEL="<plugin>|<row id|first>|<sel button id>" calls the very
  same two methods a real click calls (select_plugin_row / press_sel_button). Row pixel
  coordinates move with the plugin layout and the scroll offset, so the harness selects by
  id; the click geometry itself stays an eye-check (docs/handoff-20261027-parity.md).

  Asserts on ASCII fragments in the app log:
    "sel-row group="fgroup" id="r1""  -- the selection happened with the fixture's own
                                         group name and record id
    "f_pick = "                        -- onselect wrote the declared plugin field
    "/ touch_r1"                       -- the sel button composed <button id><record id>
  ASCII-only, like every file in this folder.

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-plugsel.ps1
#>
param(
    [string]$Exe = '',
    [string]$WorkDir = ''
)

. "$PSScriptRoot\accept-lib.ps1"

if (-not $Exe) { $Exe = Get-DefaultExe }
if (-not $WorkDir) { $WorkDir = Join-Path (Get-RepoRoot) 'target\accept\plugsel' }

$app = Join-Path $WorkDir 'app'
$logPath = Join-Path $WorkDir 'focusflow.log'

"exe : $Exe"
Stop-App
New-AppDir $app @('[collector]', 'enabled = false')
$dst = Join-Path $app 'plugins'
New-Item -ItemType Directory -Force -Path $dst | Out-Null

# The fixture: a grouped table (single select) with onselect, plus one sel button.
$lua = @'
PLUGIN_NAME = "SelFixture"
PLUGIN_DESC = "table selection acceptance fixture"
PLUGIN_VERSION = "1.0.0"

local picked = ""

function get_view()
    return {
        title = "sel fixture",
        widgets = {
            { type = "heading", text = "rows" },
            {
                type = "table",
                headers = { "name" },
                rows = { { "one" }, { "two" }, { "three" } },
                ids = { "r1", "r2", "r3" },
                group = "fgroup",
                onselect = "f_pick",
            },
            { type = "button", id = "touch_", text = "touch", sel = true, group = "fgroup" },
            { type = "label", text = "picked=" .. picked },
        },
    }
end

function set_field(field, value)
    if field == "f_pick" then picked = value end
end

function on_action(id)
end
'@
[System.IO.File]::WriteAllText((Join-Path $dst 'sel_fixture.lua'), $lua, (New-Object System.Text.UTF8Encoding $false))
"app : $app"

function Get-FixLog {
    if (Test-Path $logPath) { Get-Content $logPath -Encoding UTF8 } else { @() }
}

function Wait-FixLog([string]$Pattern, [int]$TimeoutMs = 8000) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalMilliseconds -lt $TimeoutMs) {
        if (@(Get-FixLog) -match $Pattern) { return $true }
        Start-Sleep -Milliseconds 150
    }
    $false
}

Remove-Item $logPath -ErrorAction SilentlyContinue
$p = Start-App $Exe $app $WorkDir @{ FOCUSFLOW_PLUGIN_SEL = 'SelFixture|first|touch_' }
$h = Wait-Window $p.Id
Check 'main window appears' ($h -ne [IntPtr]::Zero)
if ($h -eq [IntPtr]::Zero) { Stop-App; Show-Summary }

# The hook fires 1.5 s after start; give it a moment, then read what it did.
Start-Sleep -Milliseconds 4000
$log = @(Get-FixLog)

Check 'the fixture plugin loaded' (@($log | Where-Object { $_ -like '*SelFixture*' }).Count -gt 0)
Check 'a table row selection reached the host' (Wait-FixLog 'sel-row group=')
Check 'the record id came from the Lua table' (@($log | Where-Object { $_ -match 'sel-row group=.*id="r1"' }).Count -gt 0)
Check 'the group name came from the Lua table' (@($log | Where-Object { $_ -match 'sel-row group="fgroup"' }).Count -gt 0)
Check 'onselect wrote the declared plugin field' (@($log | Where-Object { $_ -match 'f_pick = ' }).Count -gt 0)
Check 'the sel button composed <button id><record id>' (@($log | Where-Object { $_ -like '*/ touch_r1*' }).Count -gt 0)
Check 'nothing was skipped for want of a selection' (-not (@($log | Where-Object { $_ -match 'sel-skip' }))) ''
Check 'no error lines in the app log' (-not (@($log) -match "(?i)error|$(Get-FailWord)")) ''

[void][FF]::PostMessage($h, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw = [Diagnostics.Stopwatch]::StartNew()
while (-not $p.HasExited -and $sw.Elapsed.TotalMilliseconds -lt 8000) { Start-Sleep -Milliseconds 150 }
Check 'quits within 8 s' $p.HasExited "$([int]$sw.Elapsed.TotalMilliseconds) ms"
if (-not $p.HasExited) { Stop-App }

Show-Summary
