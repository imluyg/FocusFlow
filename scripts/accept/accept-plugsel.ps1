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
  Widget::Pager draws its declared prev/next ids as real buttons, so a pixel click on
  "next" must fire "/ pg_next", while "prev" is disabled on the first page and must
  fire nothing. Four controls stand in front of that readout (the window client size
  the hardcoded rows assume, the two plain buttons above and below the pager band, and
  the next click itself): with any of them red the inert-prev readout reports
  NOT MEASURED instead of passing for the wrong reason.
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
# A user-installed script plugin enters the list from the data dir (data\plugins\); this
# fixture takes that path. Keep this file ASCII-only.
$dst = Join-Path $app 'data\plugins'
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
            -- The shipped ledger puts its pager BELOW the table. Keep a second one here in
            -- that order: it is the leg that fails when a table swallows clicks outside its
            -- own rows (the pager above the table cannot see that defect).
            { type = "pager", page = 1, pages = 2, total = 7,
              prev = "pg2_prev", next = "pg2_next" },
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

# ---- pager ---------------------------------------------------------------
# Widget::Pager draws the declared prev/next ids as real buttons now (it used to be
# one inert label row). Coordinates are the client rows of the opened fixture view at
# 100% DPI (content starts at y=260; a plain button row is 38 tall, the pager band is
# 34 tall: label then prev/next, then the row below). Since the native caption was
# removed the client area is the whole 1200x860 window; top-anchored coordinates below
# are unchanged, only the bottom edge moved down.
Click-ViewTab $h $VIEW_PLUGINS
[FF]::ClickWindow($h, 200, 182, 1)          # the only plugin row = SelFixture: open its view
Start-Sleep -Milliseconds 600
$cl = Get-Client $h
$sizeOk = ($cl.W -eq 1200 -and $cl.H -eq 860)
Check 'the click coordinates address the rows they name' ([bool]$sizeOk) ("client=" + $cl.W + "x" + $cl.H)
[FF]::ClickWindow($h, 90, 187, 1)           # row 1: a plain button, must fire
$probeLive = Wait-FixLog '/ pg_probe'
Check 'the opened view takes a pixel click on a button' ([bool]$probeLive) ''
[FF]::ClickWindow($h, 90, 257, 1)           # row 3: below the 34 px pager band
$afterLive = Wait-FixLog '/ pg_after'
Check 'the pager band leaves the next row reachable' ([bool]$afterLive) ''
[FF]::ClickWindow($h, 350, 224, 1)          # pager: next (enabled on page 1 / 3)
$nextLive = Wait-FixLog '/ pg_next'
Check 'the pager next button fires the declared id' ([bool]$nextLive) ''
[FF]::ClickWindow($h, 270, 224, 1)          # pager: prev (disabled on page 1)
Start-Sleep -Milliseconds 800
$afterPager = @(Get-FixLog)
$prevFired = @($afterPager | Where-Object { $_ -match '/ pg_prev' }).Count -gt 0
# The inert-prev readout only means something while every control above held: a dead
# band and a window whose coordinates never landed both read as "no action".
if (-not ($sizeOk -and $probeLive -and $afterLive -and $nextLive)) {
    $pagerDetail = 'NOT MEASURED: a click control above was red, so nothing reached this view'
} elseif ($prevFired) {
    $pagerDetail = 'the disabled prev button fired an action on the first page'
} else {
    $pagerDetail = 'prev is inert on page 1 while next fired'
}
Check 'the pager prev button is inert on the first page' ([bool]($sizeOk -and $probeLive -and $afterLive -and $nextLive -and (-not $prevFired))) $pagerDetail

# ---- pager below the table (the order the shipped ledger uses) --------------------------
# Content rows after the table: table (3 rows) ends at y=484, then the second pager band
# 490..518. The table's hit arm used to return for every click that was not on a row, so
# this coordinate was dead while the pager ABOVE the table stayed green -- the exact shape
# of the reported "next page does nothing" in the ledger. A positive action readout here
# cannot be earned by a dead band.
[FF]::ClickWindow($h, 354, 416, 1)          # below-table pager: next
$belowLive = Wait-FixLog '/ pg2_next'
Check 'a pager below a table still fires its declared id' ([bool]$belowLive) ''
[FF]::ClickWindow($h, 270, 416, 1)          # below-table pager: prev (disabled on page 1)
Start-Sleep -Milliseconds 700
$belowPrev = @(Get-FixLog | Where-Object { $_ -match '/ pg2_prev' }).Count -gt 0
Check 'the below-table pager prev is inert on the first page' ([bool](-not $belowPrev)) ''

Check 'nothing was skipped for want of a selection' (-not (@($afterPager | Where-Object { $_ -match 'sel-skip' }))) ''
Check 'no error lines in the app log' (-not (@($afterPager) -match "(?i)error|$(Get-FailWord)")) ''

[void][FF]::PostMessage($h, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
$sw = [Diagnostics.Stopwatch]::StartNew()
while (-not $p.HasExited -and $sw.Elapsed.TotalMilliseconds -lt 8000) { Start-Sleep -Milliseconds 150 }
Check 'quits within 8 s' $p.HasExited "$([int]$sw.Elapsed.TotalMilliseconds) ms"
if (-not $p.HasExited) { Stop-App }

Show-Summary
