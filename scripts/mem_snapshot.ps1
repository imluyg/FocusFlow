# FocusFlow memory snapshot: walk the whole process tree, not just direct children.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/mem_snapshot.ps1
#   powershell ... -File scripts/mem_snapshot.ps1 -Label S3-after-snip -Out docs/memory-snapshots.jsonl
#   powershell ... -File scripts/mem_snapshot.ps1 -SelfTest
#
# Why the tree walk is the load-bearing part: a Tauri app's WebView2 renderer/gpu/utility
# processes are NOT children of the app process - they are grandchildren under the browser
# process. Filtering by ParentProcessId == app pid sees 1 process out of 8 (measured on
# 2026-10-25: 93.9 MB of 362.4 MB). -SelfTest pins exactly that failure mode.
#
# Reading conventions (see docs/memory-baseline.md):
#   WS (WorkingSetSize) double-counts DLL/code pages shared between processes -> overstates
#   a sum; Private (PrivatePageCount) omits Chromium's shared memory -> understates.
#   The truth is between them, and a single sample carries +-10..20% noise.

param(
    [string]$Name = "FocusFlow",
    [string]$Label = "",
    [string]$Out = "",
    [switch]$SelfTest
)

$ErrorActionPreference = "Stop"

# Lines that must reach stdout even when this function's *return value* is consumed by
# `exit (...)`: pipeline output inside a function gets captured, so Write-Output there is
# invisible and a self-test that printed nothing still exited 0. Write the console directly.
function Emit([string]$text) { [Console]::WriteLine($text) }

function Get-DescendantIds {
    # Transitive closure over ParentProcessId. Self-parented rows are skipped so a
    # malformed snapshot cannot spin forever.
    param([object[]]$Rows, [int]$RootPid)
    $seen = New-Object "System.Collections.Generic.HashSet[int]"
    $queue = New-Object "System.Collections.Generic.Queue[int]"
    $queue.Enqueue($RootPid)
    while ($queue.Count -gt 0) {
        $cur = $queue.Dequeue()
        foreach ($r in $Rows) {
            $procId = [int]$r.ProcessId
            $parentId = [int]$r.ParentProcessId
            if ($procId -eq $parentId) { continue }
            if ($parentId -eq $cur -and -not $seen.Contains($procId)) {
                [void]$seen.Add($procId)
                $queue.Enqueue($procId)
            }
        }
    }
    return $seen
}

function Get-ProcRole {
    param([string]$ProcName, [string]$Cmd)
    if ($ProcName -notlike "*msedgewebview2*") { return "app" }
    if ($Cmd -like "*--type=renderer*") { return "renderer" }
    if ($Cmd -like "*--type=gpu*") { return "gpu" }
    if ($Cmd -like "*--type=utility*") { return "utility" }
    if ($Cmd -like "*crashpad*") { return "crashpad" }
    if ($Cmd -like "*--type=*") { return "webview-child" }
    return "browser"
}

function Get-TreeTotal {
    # Root plus every descendant. Returns MB rounded to 0.1.
    #
    # The `@(...)` is not decoration: PowerShell unrolls a one-element result into a bare
    # int, and an int has no `.Contains()` - it throws. That is exactly the shape of a tree
    # with a single child (a just-started app), so without it the snapshot dies on the one
    # scenario worth measuring.
    param([object[]]$Rows, [int]$RootPid)
    $ids = @(Get-DescendantIds -Rows $Rows -RootPid $RootPid)
    $ws = [double]$Rows.Where({ [int]$_.ProcessId -eq $RootPid }).WorkingSetSize
    $priv = [double]$Rows.Where({ [int]$_.ProcessId -eq $RootPid }).PrivatePageCount
    foreach ($r in $Rows) {
        if ($ids.Contains([int]$r.ProcessId)) {
            $ws += [double]$r.WorkingSetSize
            $priv += [double]$r.PrivatePageCount
        }
    }
    return [pscustomobject]@{
        RootPid       = $RootPid
        Descendants   = $ids.Count
        WsMb          = [math]::Round($ws / 1MB, 1)
        PrivateMb     = [math]::Round($priv / 1MB, 1)
    }
}

function Test-SelfTest {
    $script:pass = 0
    $script:fail = 0
    $row = {
        param($procId_, $parentId_, $name, $cmd, $wsBytes, $privBytes)
        [pscustomobject]@{
            ProcessId         = $procId_
            ParentProcessId   = $parentId_
            Name              = $name
            CommandLine       = $cmd
            WorkingSetSize    = $wsBytes
            PrivatePageCount  = $privBytes
        }
    }
    # A fake FocusFlow.exe (100) -> webview2 browser (200) -> 3 grandchildren (201/202/203),
    # plus a decoy tree (300/301) belonging to a different app with identical shape.
    $rows = @(
        (& $row 100 1   "FocusFlow.exe"      ""                        20MB 10MB),
        (& $row 200 100 "msedgewebview2.exe" "--browser-subprocess"    80MB 90MB),
        (& $row 201 200 "msedgewebview2.exe" "--type=renderer"         40MB 30MB),
        (& $row 202 200 "msedgewebview2.exe" "--type=gpu-process"      30MB 20MB),
        (& $row 203 200 "msedgewebview2.exe" "--type=crashpad-handler" 10MB 5MB),
        (& $row 300 999 "msedgewebview2.exe" "--browser-subprocess"    70MB 70MB),
        (& $row 301 300 "msedgewebview2.exe" "--type=renderer"         60MB 60MB)
    )

    $t = Get-TreeTotal -Rows $rows -RootPid 100
    # Root + children, i.e. what a depth-1 walk would report. The whole point of this script
    # is that the tree is bigger than that: the renderers sit under the browser process.
    $depthOne = [double]($rows | Where-Object { $_.ProcessId -eq 100 -or $_.ParentProcessId -eq 100 } |
        Measure-Object -Property WorkingSetSize -Sum).Sum

    function Check($what, $ok) {
        if ($ok) { $script:pass++; Emit "RESULT::PASS $what" }
        else { $script:fail++; Emit "RESULT::FAIL $what" }
    }

    $kids = @(Get-DescendantIds -Rows $rows -RootPid 100)
    # 4 = the browser process itself plus its three children. A previous revision asserted 3
    # ("grandchildren") and stayed green on the sums only because the sum walk was right
    # while the count expectation was wrong.
    Check "closure-sees-browser-and-its-children" ($kids.Count -eq 4)
    Check "closure-excludes-decoy-tree" (-not $kids.Contains(300) -and -not $kids.Contains(301))
    Check "closure-excludes-root" (-not $kids.Contains(100))

    # 20+80+40+30+10 = 180 MB WS.
    Check "tree-total-includes-root" ($t.WsMb -eq 180.0)
    Check "tree-walk-beats-depth-one" ($t.WsMb -gt [math]::Round($depthOne / 1MB, 1))
    Check "tree-private-sums" ($t.PrivateMb -eq 155.0)
    Check "tree-descendant-count" ($t.Descendants -eq 4)

    Check "role-browser" ((Get-ProcRole "msedgewebview2.exe" "--browser-subprocess") -eq "browser")
    Check "role-renderer" ((Get-ProcRole "msedgewebview2.exe" "--type=renderer") -eq "renderer")
    Check "role-gpu" ((Get-ProcRole "msedgewebview2.exe" "--type=gpu-process") -eq "gpu")
    Check "role-crashpad" ((Get-ProcRole "msedgewebview2.exe" "--type=crashpad-handler") -eq "crashpad")
    Check "role-app" ((Get-ProcRole "FocusFlow.exe" "") -eq "app")

    # The one-descendant tree (an app whose webview env has only just come up). Before the
    # `@(...)` fix this threw "[System.Int32] does not contain Contains" - the red here is
    # deliberately the same exception a real snapshot would hit, not a softer assertion.
    $solo = @((& $row 400 1 "FocusFlow.exe" "" 5MB 3MB),
              (& $row 401 400 "msedgewebview2.exe" "--browser-subprocess" 50MB 40MB))
    $st = Get-TreeTotal -Rows $solo -RootPid 400
    Check "single-descendant-still-sums" ($st.WsMb -eq 55.0 -and $st.Descendants -eq 1)

    Emit "RESULT::ASSERTS $script:pass $script:fail"
    if ($script:fail -ne 0 -or $script:pass -lt 13) { return 1 }
    Emit "RESULT::MEM_SNAPSHOT_OK"
    return 0
}

if ($SelfTest) {
    exit (Test-SelfTest)
}

# ---- real snapshot ---------------------------------------------------------------
try {
    $rows = @(Get-CimInstance -ClassName Win32_Process)
} catch {
    Write-Output "ERROR::CIM-DENIED $($_.Exception.Message)"
    Write-Output "ABORT: cannot enumerate process parentage without Win32_Process; totals would be wrong, not small."
    exit 2
}

$roots = @($rows | Where-Object { $_.Name -like "$Name.exe" })
if ($roots.Count -eq 0) {
    Write-Output "ERROR::NO-ROOT $Name.exe is not running"
    Write-Output "ABORT: nothing measured. Start the app and pick the scenario first."
    exit 3
}
if ($roots.Count -gt 1) {
    Write-Output "NOTE::MULTIPLE-ROOTS $($roots.Count) processes named $Name.exe; using the newest."
    $roots = @($roots | Sort-Object CreationDate -Descending | Select-Object -First 1)
}
$rootPid = [int]$roots[0].ProcessId

$ids = @(Get-DescendantIds -Rows $rows -RootPid $rootPid)
$tree = @($rows | Where-Object { [int]$_.ProcessId -eq $rootPid -or $ids.Contains([int]$_.ProcessId) })

Write-Output "ROOT $rootPid $($roots[0].Name)"
foreach ($p in ($tree | Sort-Object WorkingSetSize -Descending)) {
    $role = Get-ProcRole ([string]$p.Name) ([string]$p.CommandLine)
    Write-Output ("PROC pid={0} ppid={1} role={2} wsMB={3} privMB={4}" -f `
        $p.ProcessId, $p.ParentProcessId, $role, `
        [math]::Round($p.WorkingSetSize / 1MB, 1), [math]::Round($p.PrivatePageCount / 1MB, 1))
}
$total = Get-TreeTotal -Rows $rows -RootPid $rootPid
$direct = @($rows | Where-Object { [int]$_.ParentProcessId -eq $rootPid })
Write-Output ("TOTAL label='{0}' root={1} procs={2} directChildren={3} wsMB={4} privMB={5}" -f `
    $Label, $rootPid, $tree.Count, $direct.Count, $total.WsMb, $total.PrivateMb)

if ($Out -ne "") {
    $rec = [ordered]@{
        ts             = (Get-Date).ToString("o")
        label          = $Label
        rootPid        = $rootPid
        app            = $Name
        procs          = $tree.Count
        directChildren = $direct.Count
        wsMb           = $total.WsMb
        privMb         = $total.PrivateMb
        perProcess     = @($tree | ForEach-Object {
            [ordered]@{
                pid    = [int]$_.ProcessId
                ppid   = [int]$_.ParentProcessId
                role   = (Get-ProcRole ([string]$_.Name) ([string]$_.CommandLine))
                wsMb   = [math]::Round($_.WorkingSetSize / 1MB, 1)
                privMb = [math]::Round($_.PrivatePageCount / 1MB, 1)
            }
        })
    }
    (ConvertTo-Json -InputObject $rec -Depth 6 -Compress) | Add-Content -Path $Out -Encoding ASCII
    Write-Output "APPENDED $Out"
}
exit 0
