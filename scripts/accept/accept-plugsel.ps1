<#
.SYNOPSIS
  Plugin table selection + button.sel: the ids/group/onselect a Lua table declares must
  reach the host, and a sel button must send "<button id><selected id>" to the plugin.

.DESCRIPTION
  Writes its own tiny ASCII plugin (SelFixture) into a fresh COPY of desktop-native\probe,
  so nothing depends on ledger data existing and nothing touches real data.

  The app hook FOCUSFLOW_PLUGIN_SEL="<plugin>|<row id|first>|<sel button id>" calls the very
  same two methods a real click calls (select_plugin_row / press_sel_button). Table-row pixel
  coordinates move with the plugin layout and the scroll offset, so the harness selects a row
  by id and that one gesture stays an eye-check; plain buttons and the pager row, which stack
  from a known top, ARE clicked at real pixel coordinates below.

  Asserts on ASCII fragments in the app log:
    "sel-row group="fgroup" id="r1""  -- the selection happened with the fixture's own
                                         group name and record id
    "f_pick = "                        -- onselect wrote the declared plugin field
    "/ touch_r1"                       -- the sel button composed <button id><record id>
    "/ pg_probe" and "/ pg_after"      -- a real pixel click reached the opened view

  It also opens the fixture view with real pixel clicks and reads the pager bar back:
  Widget::Pager currently renders as one inert label row (the renderer drops the
  declared prev/next ids), so clicking across that row must fire nothing. That readout
  is a BASELINE for the day the Pager gets its buttons -- it is supposed to fail then,
  and to be rewritten to assert the new shape. Three controls stand in front of it
  (the window client size the hardcoded rows assume, and the two plain buttons directly
  above and below the pager row): with any of them red the readout reports NOT MEASURED
  instead of passing for the wrong reason.
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
            -- Rows 1-3 are the pager readout below; keep their order (the click rows
            -- are laid out top-down: plain button, pager, plain button).
            { type = "button", id = "pg_probe", text = "probe" },
            { type = "pager", page = 1, pages = 3, total = 12,
              prev = "pg_prev", next = "pg_next" },
            { type = "button", id = "pg_after", text = "after" },
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

# ---- pager readout -------------------------------------------------------
# Today Widget::Pager becomes ONE inert POp::Label: the renderer never reads the
# pager's prev/next ids (desktop-native/src/main.rs:5937-5947), so the bar shows
# "page x / y" and nothing in that row can be clicked. The old WebView bar had two
# buttons. These three checks pin that shape on purpose: wiring the Pager must turn
# the row click into a real action and this readout red, then be rewritten to assert
# the new behaviour. Coordinates are the client rows of the opened fixture view at
# 100% DPI (content starts at y=260; a plain button row is 38 tall, the label row 22).
# Since the native caption was removed the client area is the whole 1200x860 window;
# top-anchored coordinates below are unchanged, only the bottom edge moved down.
Click-ViewTab $h 4
[FF]::ClickWindow($h, 200, 270, 1)          # the only plugin row = SelFixture: open its view
Start-Sleep -Milliseconds 600
$cl = Get-Client $h
$sizeOk = ($cl.W -eq 1200 -and $cl.H -eq 860)
Check 'the click coordinates address the rows they name' ([bool]$sizeOk) ("client=" + $cl.W + "x" + $cl.H)
[FF]::ClickWindow($h, 90, 275, 1)           # row 1: a plain button, must fire
$probeLive = Wait-FixLog '/ pg_probe'
Check 'the opened view takes a pixel click on a button' ([bool]$probeLive) ''
[FF]::ClickWindow($h, 90, 335, 1)           # row 3: lands right below the pager row
$afterLive = Wait-FixLog '/ pg_after'
Check 'the pager row takes one label row of height' ([bool]$afterLive) ''
foreach ($px in 45, 600, 1150) {            # across the whole pager row band
    [FF]::ClickWindow($h, $px, 307, 1)
}
Start-Sleep -Milliseconds 1200
$afterPager = @(Get-FixLog)
$inert = -not (@($afterPager | Where-Object { $_ -match '/ pg_prev|/ pg_next' }))
# This readout only means something while the three controls above hold: a row where
# nothing is clickable and a window whose coordinates never landed both read as "no action".
if (-not ($sizeOk -and $probeLive -and $afterLive)) {
    $pagerDetail = 'NOT MEASURED: a click control above was red, so nothing reached this view'
} elseif ($inert) {
    $pagerDetail = '3 clicks across the row: no action reached the plugin'
} else {
    $pagerDetail = 'the pager row fired an action: the Pager has buttons now, rewrite this baseline'
}
Check 'the pager row fires no prev/next action yet' ([bool]($sizeOk -and $probeLive -and $afterLive -and $inert)) $pagerDetail

Check 'nothing was skipped for want of a selection' (-not (@($afterPager | Where-Object { $_ -match 'sel-skip' }))) ''
Check 'no error lines in the app log' (-not (@($afterPager) -match "(?i)error|$(Get-FailWord)")) ''

[void][FF]::PostMessage($h, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw = [Diagnostics.Stopwatch]::StartNew()
while (-not $p.HasExited -and $sw.Elapsed.TotalMilliseconds -lt 8000) { Start-Sleep -Milliseconds 150 }
Check 'quits within 8 s' $p.HasExited "$([int]$sw.Elapsed.TotalMilliseconds) ms"
if (-not $p.HasExited) { Stop-App }

Show-Summary
