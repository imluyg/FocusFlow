<#
.SYNOPSIS
  Clipboard history v1: an external copy is captured, the global hotkey opens the
  palette, Enter copies the selected item, and the app's own write is not captured
  a second time.

.DESCRIPTION
  One launch against a fresh COPY of desktop-native\probe.

  The suite drives the real path: it sets the system clipboard from PowerShell (an
  external writer), waits for the app log to say the item was ingested, presses the
  configured clipboard hotkey with a real chord, finds the palette window, posts
  Enter to its search EDIT, and then checks two independent things:
    - the clipboard now holds the selected item (Enter really copied it), and
    - the app's own write did not add another history row (self-write suppression).

  Every log assertion uses an ASCII tag (`clip_ingest`, `clip_enter`) so this file
  can stay ASCII-only like the rest of the folder.

  The last round publishes an oversize `CF_UNICODETEXT` together with a valid `CF_DIB`
  from this script (an external owner): the text must be refused as text and the DIB
  must not be recorded as an image in its place.

  The original clipboard text is restored at the end.

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-clipboard.ps1
#>
param(
    [string]$Exe = '',
    [string]$WorkDir = ''
)

. "$PSScriptRoot\accept-lib.ps1"

if (-not $Exe) { $Exe = Get-DefaultExe }
if (-not $WorkDir) { $WorkDir = Join-Path (Get-RepoRoot) 'target\accept\clipboard' }

$app = Join-Path $WorkDir 'app'
$logPath = Join-Path $WorkDir 'focusflow.log'
$older = 'accept-clipboard-older'
$marker = 'accept-clipboard-marker-1'
$hotCombo = 'ctrl+shift+f9'
$hotVk = 0x78

function Get-LogLines {
    @(Read-AppLog $WorkDir)
}

# The app's clipboard listener opens the clipboard briefly on every change; a test writer
# racing with it gets "cannot open clipboard". Retry instead of treating that as a product
# failure (a real user's clipboard manager behaves the same way).
function Set-ClipboardRetry([string]$Text) {
    for ($i = 0; $i -lt 20; $i++) {
        try { Set-Clipboard -Value $Text; return } catch { Start-Sleep -Milliseconds 150 }
    }
    throw "Set-Clipboard failed after retries"
}

function Get-ClipboardRetry {
    for ($i = 0; $i -lt 20; $i++) {
        try { return (Get-Clipboard -Raw) } catch { Start-Sleep -Milliseconds 150 }
    }
    return $null
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

# The clipboard writer may publish the same text more than once (dedupe updates the row
# instead of adding one). Wait until the ingest line count stops moving before using it
# as the "before" reading for the self-suppression check.
function Wait-IngestCountStable([int]$StableMs = 1200, [int]$TimeoutMs = 8000) {
    $deadline = (Get-Date).AddMilliseconds($TimeoutMs)
    $last = -1
    $since = Get-Date
    while ((Get-Date) -lt $deadline) {
        $n = @(Get-LogLines | Where-Object { $_ -like '*clip_ingest*' }).Count
        if ($n -ne $last) {
            $last = $n
            $since = Get-Date
        } elseif (((Get-Date) - $since).TotalMilliseconds -ge $StableMs) {
            return $n
        }
        Start-Sleep -Milliseconds 150
    }
    return $last
}

# Open the palette and return its EDIT handle. One helper for the paging rounds, so each
# starts from a fresh palette (selection at row 1, scroll 0).
function Open-ClipPalette {
    [FF]::PressChord([byte[]]@(0x11, 0x10), [byte]$hotVk)
    Start-Sleep -Milliseconds 900
    $hp = [FF]::ByClassHandle($p.Id, 'FocusFlowClipboardPalette')
    $he = 0
    if ($hp -gt 0) { $he = [FF]::ChildByClassHandle($hp, 'Edit') }
    [PSCustomObject]@{ Popup = $hp; Edit = $he }
}

Stop-App $Exe
New-AppDir $app @('[collector]', 'enabled = false')
Set-ConfigLine $app 'hotkey' 'clipboard' $hotCombo
Set-ConfigLine $app 'clipboard' 'enabled' 'true'
# Auto-paste is on by default as of the UI round; this suite is the copy-only arm, so it
# must opt out explicitly. Otherwise Enter would inject Ctrl+V into the test console.
Set-ConfigLine $app 'clipboard' 'auto_paste' 'false'

$orig = $null
try { $orig = Get-ClipboardRetry } catch { }

Remove-Item $logPath -ErrorAction SilentlyContinue
"exe : $Exe"
"app : $app  (hotkey=$hotCombo)"

$p = Start-App $Exe $app $WorkDir
$h = Wait-Window $p.Id
Check 'main window appears' ($h -ne [IntPtr]::Zero)
if ($h -eq [IntPtr]::Zero) { Stop-App $Exe; Show-Summary }

Start-Sleep -Milliseconds 2000
$log = @(Get-LogLines)
Check 'clipboard hotkey is registered' (@($log | Where-Object { $_ -like "*clipboard = $hotCombo*" }).Count -gt 0)

# Two external copies through the real clipboard: the older one first, the marker last,
# so the palette's selection starts on the marker and Down can move to the older item.
Set-ClipboardRetry $older
Check 'first external copy is captured' (Wait-LogMatch '*clip_ingest*' 6000)
Start-Sleep -Milliseconds 400
Set-ClipboardRetry $marker
Check 'second external copy is captured' (Wait-LogMatch '*clip_ingest*' 6000)
$before = Wait-IngestCountStable

# The hotkey must toggle ONCE. A press handled twice would leave the palette hidden.
# Remember who owns the foreground: with auto_paste=false, Enter must hand it back.
$fgBefore = [FF]::GetForegroundWindow()
[FF]::PressChord([byte[]]@(0x11, 0x10), [byte]$hotVk)
Start-Sleep -Milliseconds 900
$popup = [FF]::ByClassHandle($p.Id, 'FocusFlowClipboardPalette')
Check 'clipboard palette is created' ($popup -gt 0)
$visible = $false
if ($popup -gt 0) { $visible = [FF]::IsWindowVisible([IntPtr]$popup) }
Check 'clipboard palette is visible after the hotkey' $visible

$edit = 0
if ($popup -gt 0) { $edit = [FF]::ChildByClassHandle($popup, 'Edit') }
Check 'palette has a search edit' ($edit -gt 0)

if ($edit -gt 0) {
    # Down selects the second row (the older text); Enter copies that one. If the
    # palette were still on the marker, the clipboard check below would be a no-op
    # because the clipboard already held the marker.
    [FF]::PostMessage([IntPtr]$edit, 0x0100, [IntPtr]0x28, [IntPtr]::Zero) | Out-Null
    Start-Sleep -Milliseconds 150
    [FF]::PostMessage([IntPtr]$edit, 0x0100, [IntPtr]0x0D, [IntPtr]::Zero) | Out-Null
    Start-Sleep -Milliseconds 900
}
$hidden = $true
if ($popup -gt 0) { $hidden = -not [FF]::IsWindowVisible([IntPtr]$popup) }
Check 'Enter closes the palette' $hidden
Check 'Enter reports a successful copy' (Wait-LogMatch '*clip_enter copied=true*' 3000)
Check 'focus returns to the window that was in front' `
    ([FF]::GetForegroundWindow() -eq $fgBefore) `
    "fg=$([FF]::GetForegroundWindow()) before=$fgBefore"
# This suite explicitly sets auto_paste=false, so nothing may be injected at all.
Check 'auto-paste stays off when configured off' `
    (@(Get-LogLines | Where-Object { $_ -like '*clip_autopaste*' }).Count -eq 0)

$clip = ''
try { $clip = Get-ClipboardRetry } catch { }
Check 'Enter puts the selected (older) item on the clipboard' ($null -ne $clip -and $clip.Trim() -eq $older)

# Give the listener time to see the app's own write; it must be suppressed.
Start-Sleep -Milliseconds 1000
$after = @(Get-LogLines | Where-Object { $_ -like '*clip_ingest*' }).Count
Check 'the app does not capture its own clipboard write' ($after -eq $before)

# ---- Search: words are ANDed across the content. The old implementation required them to
# appear contiguously; "accept-clipboard older" only exists here as "accept-clipboard-older",
# so a hit is a real difference between the new and old search (qlen=22, one match).
[FF]::PressChord([byte[]]@(0x11, 0x10), [byte]$hotVk)
Start-Sleep -Milliseconds 900
$popup2 = [FF]::ByClassHandle($p.Id, 'FocusFlowClipboardPalette')
$edit2 = 0
if ($popup2 -gt 0) { $edit2 = [FF]::ChildByClassHandle($popup2, 'Edit') }
Check 'palette reopens for the search round' ($edit2 -gt 0)
if ($edit2 -gt 0) {
    foreach ($ch in [System.Text.Encoding]::ASCII.GetBytes('accept-clipboard older')) {
        [FF]::PostMessage([IntPtr]$edit2, 0x0102, [IntPtr][int]$ch, [IntPtr]::Zero) | Out-Null
        Start-Sleep -Milliseconds 25
    }
}
Check 'multi-word search matches non-contiguous content' (Wait-LogMatch '*clip_search qlen=22 n=1*' 4000)
if ($edit2 -gt 0) {
    [FF]::PostMessage([IntPtr]$edit2, 0x0100, [IntPtr]0x1B, [IntPtr]::Zero) | Out-Null
    Start-Sleep -Milliseconds 400
}

# ---- Shortcut round: Ctrl+Enter copies without closing; Alt+1 picks row 1 and closes.
[FF]::PressChord([byte[]]@(0x11, 0x10), [byte]$hotVk)
Start-Sleep -Milliseconds 900
$popup4 = [FF]::ByClassHandle($p.Id, 'FocusFlowClipboardPalette')
$shortcutVisible = $false
if ($popup4 -gt 0) { $shortcutVisible = [FF]::IsWindowVisible([IntPtr]$popup4) }
Check 'palette opens for the shortcut round' $shortcutVisible
# PostMessage does not move GetKeyState, so these must be real injected chords.
[FF]::PressChord([byte[]]@(0x11), [byte]0x0D)
Start-Sleep -Milliseconds 500
Check 'Ctrl+Enter copies without closing' (Wait-LogMatch '*Ctrl+Enter copied=true*' 3000)
$stillVisible = $false
if ($popup4 -gt 0) { $stillVisible = [FF]::IsWindowVisible([IntPtr]$popup4) }
Check 'Ctrl+Enter leaves the palette open' $stillVisible
if ($popup4 -gt 0) { [void][FF]::SetForegroundWindow([IntPtr]$popup4) }
Start-Sleep -Milliseconds 200
[FF]::PressChord([byte[]]@(0x12), [byte]0x31)
Start-Sleep -Milliseconds 800
Check 'Alt+1 copies the first visible row' (Wait-LogMatch '*Alt+1 copied=true*' 3000)
$hiddenAfterAlt = $true
if ($popup4 -gt 0) { $hiddenAfterAlt = -not [FF]::IsWindowVisible([IntPtr]$popup4) }
Check 'Alt+1 closes the palette' $hiddenAfterAlt

# ---- Mouse round: a single click only selects. Row 1 (the marker) was copied by Alt+1,
# so an unchanged clipboard proves the click did not copy; Enter then copies the clicked row.
[FF]::PressChord([byte[]]@(0x11, 0x10), [byte]$hotVk)
Start-Sleep -Milliseconds 900
$popup5 = [FF]::ByClassHandle($p.Id, 'FocusFlowClipboardPalette')
$edit5 = 0
if ($popup5 -gt 0) { $edit5 = [FF]::ChildByClassHandle($popup5, 'Edit') }
if ($popup5 -gt 0) {
    $pr = New-Object FF+RECT
    [void][FF]::GetWindowRect([IntPtr]$popup5, [ref]$pr)
    $scale = [FF]::GetDpiForSystem() / 96.0
    $cx = [int](($pr.Right - $pr.Left) / 2)
    # Row 2 center: list_top 92 + 1.5 * row_h 36, scaled exactly like `layout_for`.
    $cy = [int][Math]::Round((92 + 36 * 1.5) * $scale)
    [FF]::ClickScreen([IntPtr]$popup5, $cx, $cy, 1)
    Start-Sleep -Milliseconds 400
}
$visibleAfterClick = $false
if ($popup5 -gt 0) { $visibleAfterClick = [FF]::IsWindowVisible([IntPtr]$popup5) }
Check 'a single click keeps the palette open' $visibleAfterClick
$clickClip = ''
try { $clickClip = Get-ClipboardRetry } catch { }
Check 'a single click does not copy' `
    ($null -ne $clickClip -and $clickClip.Trim() -eq $marker) `
    "clipboard='$clickClip' still='$marker'"
if ($edit5 -gt 0) {
    [FF]::PostMessage([IntPtr]$edit5, 0x0100, [IntPtr]0x0D, [IntPtr]::Zero) | Out-Null
}
Start-Sleep -Milliseconds 900
$afterClickEnter = ''
try { $afterClickEnter = Get-ClipboardRetry } catch { }
Check 'Enter after a single click copies the clicked row' `
    ($null -ne $afterClickEnter -and $afterClickEnter.Trim() -eq $older) `
    "clipboard='$afterClickEnter' want='$older'"
$hiddenAfterClick = $true
if ($popup5 -gt 0) { $hiddenAfterClick = -not [FF]::IsWindowVisible([IntPtr]$popup5) }
Check 'Enter after a single click closes the palette' $hiddenAfterClick

# ---- Double-click row 1 copies that row and closes.
[FF]::PressChord([byte[]]@(0x11, 0x10), [byte]$hotVk)
Start-Sleep -Milliseconds 900
$popup6 = [FF]::ByClassHandle($p.Id, 'FocusFlowClipboardPalette')
if ($popup6 -gt 0) {
    $cr = New-Object FF+RECT
    [void][FF]::GetClientRect([IntPtr]$popup6, [ref]$cr)
    $scale = [FF]::GetDpiForSystem() / 96.0
    $cx1 = [int](($cr.Right - $cr.Left) / 2)
    # Row 1 center: list_top 92 + half a 36 px row.
    $cy1 = [int][Math]::Round((92 + 18) * $scale)
    $lp = [IntPtr](($cy1 * 65536) + ($cx1 -band 0xFFFF))
    [FF]::PostMessage([IntPtr]$popup6, 0x0203, [IntPtr]1, $lp) | Out-Null
    Start-Sleep -Milliseconds 700
}
$dblClip = ''
try { $dblClip = Get-ClipboardRetry } catch { }
Check 'double-clicking the first row copies it' `
    ($null -ne $dblClip -and $dblClip.Trim() -eq $marker) `
    "clipboard='$dblClip' want='$marker'"
$hiddenAfterDbl = $true
if ($popup6 -gt 0) { $hiddenAfterDbl = -not [FF]::IsWindowVisible([IntPtr]$popup6) }
Check 'double-clicking a row closes the palette' $hiddenAfterDbl

# ---- PageDown: from the top row it lands on the last row. Put a neutral text on the
# clipboard first so "older" is a change, not the state left by the mouse round.
$neutral = 'accept-clipboard-neutral'
Set-ClipboardRetry $neutral
Start-Sleep -Milliseconds 600
[FF]::PressChord([byte[]]@(0x11, 0x10), [byte]$hotVk)
Start-Sleep -Milliseconds 900
$popup6 = [FF]::ByClassHandle($p.Id, 'FocusFlowClipboardPalette')
$edit6 = 0
if ($popup6 -gt 0) { $edit6 = [FF]::ChildByClassHandle($popup6, 'Edit') }
if ($edit6 -gt 0) {
    # VK_NEXT = 0x22: PageDown moves by (visible rows - 1), clamped to the last row.
    [FF]::PostMessage([IntPtr]$edit6, 0x0100, [IntPtr]0x22, [IntPtr]::Zero) | Out-Null
    Start-Sleep -Milliseconds 250
    [FF]::PostMessage([IntPtr]$edit6, 0x0100, [IntPtr]0x0D, [IntPtr]::Zero) | Out-Null
}
Start-Sleep -Milliseconds 900
$pageClip = ''
try { $pageClip = Get-ClipboardRetry } catch { }
Check 'PageDown then Enter copies the last row' `
    ($null -ne $pageClip -and $pageClip.Trim() -eq $older) `
    "clipboard='$pageClip' want='$older'"

# ---- End / Home / PageUp, each with a different "before" so a no-op key cannot pass.
# A fresh top row makes "copy the first row" differ from whatever the clipboard held before.
$topText = 'accept-clipboard-top'
$ingestBeforeTop = @(Get-LogLines | Where-Object { $_ -like '*clip_ingest*' }).Count
Set-ClipboardRetry $topText
$ingestAfterTop = Wait-IngestCountStable 800 8000
Check 'the top-row text is captured before the paging round' ($ingestAfterTop -gt $ingestBeforeTop)

# End + Enter: last row (older), while the clipboard still holds the fresh top text.
$pal = Open-ClipPalette
if ($pal.Edit -gt 0) {
    [FF]::PostMessage([IntPtr]$pal.Edit, 0x0100, [IntPtr]0x23, [IntPtr]::Zero) | Out-Null  # VK_END
    Start-Sleep -Milliseconds 200
    [FF]::PostMessage([IntPtr]$pal.Edit, 0x0100, [IntPtr]0x0D, [IntPtr]::Zero) | Out-Null
}
Start-Sleep -Milliseconds 900
$endClip = ''
try { $endClip = Get-ClipboardRetry } catch { }
Check 'End then Enter copies the last row' `
    ($null -ne $endClip -and $endClip.Trim() -eq $older) `
    "clipboard='$endClip' want='$older'"

# Home from a middle row: first row (the fresh top text), while the clipboard holds older.
$pal = Open-ClipPalette
if ($pal.Edit -gt 0) {
    foreach ($vk in @(0x28, 0x28)) {   # VK_DOWN twice -> row 3
        [FF]::PostMessage([IntPtr]$pal.Edit, 0x0100, [IntPtr]$vk, [IntPtr]::Zero) | Out-Null
        Start-Sleep -Milliseconds 120
    }
    [FF]::PostMessage([IntPtr]$pal.Edit, 0x0100, [IntPtr]0x24, [IntPtr]::Zero) | Out-Null  # VK_HOME
    Start-Sleep -Milliseconds 200
    [FF]::PostMessage([IntPtr]$pal.Edit, 0x0100, [IntPtr]0x0D, [IntPtr]::Zero) | Out-Null
}
Start-Sleep -Milliseconds 900
$homeClip = ''
try { $homeClip = Get-ClipboardRetry } catch { }
Check 'Home from a middle row copies the first row' `
    ($null -ne $homeClip -and $homeClip.Trim() -eq $topText) `
    "clipboard='$homeClip' want='$topText'"

# Re-arm the "before" with the app's own copy of the last row (self-write keeps the order),
# then PageUp from a middle row must land on the first row.
$pal = Open-ClipPalette
if ($pal.Edit -gt 0) {
    [FF]::PostMessage([IntPtr]$pal.Edit, 0x0100, [IntPtr]0x23, [IntPtr]::Zero) | Out-Null
    Start-Sleep -Milliseconds 200
    [FF]::PostMessage([IntPtr]$pal.Edit, 0x0100, [IntPtr]0x0D, [IntPtr]::Zero) | Out-Null
}
Start-Sleep -Milliseconds 900
$pal = Open-ClipPalette
if ($pal.Edit -gt 0) {
    foreach ($vk in @(0x28, 0x28)) {
        [FF]::PostMessage([IntPtr]$pal.Edit, 0x0100, [IntPtr]$vk, [IntPtr]::Zero) | Out-Null
        Start-Sleep -Milliseconds 120
    }
    [FF]::PostMessage([IntPtr]$pal.Edit, 0x0100, [IntPtr]0x21, [IntPtr]::Zero) | Out-Null  # VK_PRIOR
    Start-Sleep -Milliseconds 200
    [FF]::PostMessage([IntPtr]$pal.Edit, 0x0100, [IntPtr]0x0D, [IntPtr]::Zero) | Out-Null
}
Start-Sleep -Milliseconds 900
$pageUpClip = ''
try { $pageUpClip = Get-ClipboardRetry } catch { }
Check 'PageUp from a middle row copies the first row' `
    ($null -ne $pageUpClip -and $pageUpClip.Trim() -eq $topText) `
    "clipboard='$pageUpClip' want='$topText'"

# Page: open the management page, enable multi-select, pick the first row, pin it.
[FF]::ClickWindow($h, 644, 183, 1)
Check 'clipboard management page opens' (Wait-LogMatch '*clip_page_open*' 3000)
[FF]::ClickWindow($h, 474, 256, 1)
Start-Sleep -Milliseconds 250
[FF]::ClickWindow($h, 300, 300, 1)
Start-Sleep -Milliseconds 250
[FF]::ClickWindow($h, 138, 256, 1)
Check 'page bulk pin runs' (Wait-LogMatch '*clip_page_pin*' 3000)

# ---- v1.2: images and files through the real clipboard ----
# .NET's SetImage publishes CF_DIB (BI_BITFIELDS) here; the app has to parse it (and the
# synthesized CF_DIB from CF_BITMAP sources) to count as "copied an image".
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
$imgFile = Join-Path $WorkDir 'accept-clip-image.png'
$bmp = New-Object System.Drawing.Bitmap 64, 48
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.Clear([System.Drawing.Color]::FromArgb(210, 60, 40))
$g.Dispose()
$bmp.Save($imgFile, [System.Drawing.Imaging.ImageFormat]::Png)

$imgSet = $false
for ($i = 0; $i -lt 20; $i++) {
    try {
        $img = [System.Drawing.Image]::FromFile($imgFile)
        [System.Windows.Forms.Clipboard]::SetImage($img)
        $img.Dispose()
        $imgSet = $true
        break
    } catch { Start-Sleep -Milliseconds 150 }
}
Check 'external image copy is published' $imgSet
Check 'external image is captured' (Wait-LogMatch '*clip_ingest_image*' 8000)
$imgDir = Join-Path $app 'data\clipboard\images'
$thumbDir = Join-Path $app 'data\clipboard\thumbs'
$pngs = @()
if (Test-Path $imgDir) { $pngs = @(Get-ChildItem $imgDir -Filter *.png) }
$thumbs = @()
if (Test-Path $thumbDir) { $thumbs = @(Get-ChildItem $thumbDir -Filter *.png) }
Check 'image is stored on disk with a thumbnail' (($pngs.Count -ge 1) -and ($thumbs.Count -ge 1))

# File entry: copy a real file, then delete it so the page has to say the path is gone.
$fileFixture = Join-Path $WorkDir 'accept-clip-file.txt'
Set-Content -Path $fileFixture -Value 'accept-clip-file' -Encoding UTF8
$fileSet = $false
for ($i = 0; $i -lt 20; $i++) {
    try { Set-Clipboard -Path $fileFixture; $fileSet = $true; break } catch { Start-Sleep -Milliseconds 150 }
}
Check 'external file copy is published' $fileSet
Check 'external file is captured' (Wait-LogMatch '*clip_ingest_files*' 8000)

Remove-Item $fileFixture -Force
# A fresh string, not the marker: the shortcut round just put the marker on the clipboard,
# and re-setting the same text inside the 3 s self-write window is (correctly) suppressed.
# Without a real ingest here nothing reloads the page, so the missing file would go unseen.
$afterFile = 'accept-clipboard-after-file'
Set-ClipboardRetry $afterFile
Check 'missing file rows are marked on the page' (Wait-LogMatch '*clip_page_missing_files*' 8000)

# Copy the image back from the page: re-copy it (dedupe only moves it to the top), leave
# the pinned row alone, click the first unpinned row and press the Copy tool button.
$imgSet2 = $false
for ($i = 0; $i -lt 20; $i++) {
    try {
        $img = [System.Drawing.Image]::FromFile($imgFile)
        [System.Windows.Forms.Clipboard]::SetImage($img)
        $img.Dispose()
        $imgSet2 = $true
        break
    } catch { Start-Sleep -Milliseconds 150 }
}
Check 'image can be copied a second time' $imgSet2
Start-Sleep -Milliseconds 1200
# Leave multi-select first: while it is on, a row click toggles a checkbox and Copy acts on
# the checked row (still the pinned text), not on the row under the cursor.
[FF]::ClickWindow($h, 474, 256, 1)
Start-Sleep -Milliseconds 250
[FF]::ClickWindow($h, 300, 340, 1)
Start-Sleep -Milliseconds 250
[FF]::ClickWindow($h, 54, 256, 1)
Check 'page copy sends the image back to the clipboard' (Wait-LogMatch '*clip_page_copy kind=Image*' 4000)
$hasImage = $false
try { $hasImage = [System.Windows.Forms.Clipboard]::ContainsImage() } catch { }
Check 'clipboard holds an image after copying the image row' $hasImage

# ---- D batch: live filtering, Ctrl+A, and delete must ask before it destroys anything ----
function Wait-LogCount([string]$Pattern, [int]$Want, [int]$TimeoutMs = 5000) {
    $deadline = (Get-Date).AddMilliseconds($TimeoutMs)
    while ((Get-Date) -lt $deadline) {
        if (@(Get-LogLines | Where-Object { $_ -like $Pattern }).Count -ge $Want) { return $true }
        Start-Sleep -Milliseconds 150
    }
    $false
}

# Click the search box (the drawn rect; the real EDIT only appears once it has focus),
# then type into that EDIT. Each WM_CHAR changes the text, so EN_CHANGE must reach the
# window and the list must be filtered without pressing Enter.
[FF]::ClickWindow($h, 1050, 217, 1)
Start-Sleep -Milliseconds 400
$editHost = [FF]::ByClassHandle($p.Id, 'FocusFlowEditHost')
$se = 0
if ($editHost -gt 0) { $se = [FF]::ChildByClassHandle($editHost, 'Edit') }
Check 'clipboard search edit exists after clicking the box' ($se -gt 0)
if ($se -gt 0) {
    foreach ($ch in @(0x7A, 0x7A, 0x7A, 0x71)) {   # "zzzq": matches nothing
        [FF]::PostMessage([IntPtr]$se, 0x0102, [IntPtr]$ch, [IntPtr]::Zero) | Out-Null
        Start-Sleep -Milliseconds 60
    }
}
Check 'typing filters the list without Enter' (Wait-LogMatch '*clip_page_live_filter len=4 hits=0*' 4000)
[FF]::PostMessage([IntPtr]$se, 0x0100, [IntPtr]0x1B, [IntPtr]::Zero) | Out-Null
Check 'Esc clears the live filter' (Wait-LogMatch '*commit=false*' 3000)

# Tab out of the search EDIT: History -> Snippets -> Stats. Hiding the hosted EDIT
# must hand the foreground back to the main window, or every later key goes elsewhere.
[FF]::PostMessage([IntPtr]$se, 0x0100, [IntPtr]0x09, [IntPtr]::Zero) | Out-Null
Start-Sleep -Milliseconds 300
[FF]::PostMessage([IntPtr]$se, 0x0100, [IntPtr]0x09, [IntPtr]::Zero) | Out-Null
Start-Sleep -Milliseconds 400
Check 'Tab to stats leaves the main window in the foreground' ([FF]::GetForegroundWindow() -eq $h)
[FF]::PostMessage([IntPtr]$h, 0x0100, [IntPtr]0x09, [IntPtr]::Zero) | Out-Null
Start-Sleep -Milliseconds 300

[void][FF]::SetForegroundWindow($h)
Start-Sleep -Milliseconds 300
[FF]::PressChord([byte[]]@(0x11), [byte]0x41)
Check 'Ctrl+A checks every row on the page' (Wait-LogMatch '*clip_page_check_all*' 3000)

# Ctrl+A checked every row. Now change the filter: the checked ids that fall out of the
# list must not stay armed, or Del would remove rows the user cannot see.
[FF]::ClickWindow($h, 1050, 217, 1)
Start-Sleep -Milliseconds 300
foreach ($ch in @(0x6D, 0x61, 0x72, 0x6B, 0x65, 0x72)) {  # "marker": exactly one row
    [FF]::PostMessage([IntPtr]$se, 0x0102, [IntPtr]$ch, [IntPtr]::Zero) | Out-Null
    Start-Sleep -Milliseconds 60
}
Check 'the filter change narrows the list to one row' (Wait-LogMatch '*clip_page_live_filter len=6 hits=1*' 4000)
# The keyboard still belongs to the search EDIT here, so drive the delete through the
# toolbar button: the point of this leg is which ids are armed, not the key routing.
[FF]::ClickWindow($h, 220, 256, 1)
Start-Sleep -Milliseconds 900
[FF]::PressChord([byte[]]@(), [byte]0x0D)
Check 'delete after a filter change only removes the visible row' (Wait-LogMatch '*clip_page_delete n=1*' 5000)

# Clear the filter again so the rounds below still have a full page to work with.
[FF]::ClickWindow($h, 1050, 217, 1)
Start-Sleep -Milliseconds 300
$clearsBefore = @(Get-LogLines | Where-Object { $_ -like '*commit=false*' }).Count
[FF]::PostMessage([IntPtr]$se, 0x0100, [IntPtr]0x1B, [IntPtr]::Zero) | Out-Null
Check 'Esc clears the filter again after the delete round' (Wait-LogCount '*commit=false*' ($clearsBefore + 1) 3000)
[void][FF]::SetForegroundWindow($h)
Start-Sleep -Milliseconds 250
[FF]::PressChord([byte[]]@(0x11), [byte]0x41)
Check 'Ctrl+A checks every row again after clearing the filter' (Wait-LogCount '*clip_page_check_all*' 2 3000)

# Click Delete: the row set must still be there after a beat (the confirm dialog is up),
# and only Enter (the default Yes) may actually remove it.
$deletesBefore = @(Get-LogLines | Where-Object { $_ -like '*clip_page_delete*' }).Count
[FF]::ClickWindow($h, 220, 256, 1)
Start-Sleep -Milliseconds 900
$deletesPending = @(Get-LogLines | Where-Object { $_ -like '*clip_page_delete*' }).Count
Check 'delete waits for the confirmation' ($deletesPending -eq $deletesBefore)
[FF]::PressChord([byte[]]@(), [byte]0x0D)
Check 'confirming deletes the checked rows' (Wait-LogCount '*clip_page_delete*' ($deletesBefore + 1) 5000)

# ---- capture guard: an oversize text that also carries a DIB must not become an image ----
# Excel-style copies publish both CF_UNICODETEXT and CF_DIB. When the text was over the
# limit the reader used to fall through to the image path, so a text the user could never
# get back was stored as a picture. The fixture publishes both formats from THIS script's
# process: a same-process writer inside the app would be rejected as a self write, so the
# end-to-end case has to come from outside.
if (-not ('FFClipFixture' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class FFClipFixture {
    [DllImport("user32.dll", SetLastError = true)] public static extern bool OpenClipboard(IntPtr owner);
    [DllImport("user32.dll", SetLastError = true)] public static extern bool CloseClipboard();
    [DllImport("user32.dll", SetLastError = true)] public static extern bool EmptyClipboard();
    [DllImport("user32.dll", SetLastError = true)] public static extern IntPtr SetClipboardData(uint format, IntPtr mem);
    [DllImport("kernel32.dll", SetLastError = true)] public static extern IntPtr GlobalAlloc(uint flags, UIntPtr bytes);
    [DllImport("kernel32.dll", SetLastError = true)] public static extern IntPtr GlobalLock(IntPtr mem);
    [DllImport("kernel32.dll", SetLastError = true)] public static extern bool GlobalUnlock(IntPtr mem);
    static IntPtr CopyToGlobal(byte[] bytes) {
        IntPtr h = GlobalAlloc(0x0002, (UIntPtr)bytes.Length);
        if (h == IntPtr.Zero) return IntPtr.Zero;
        IntPtr p = GlobalLock(h);
        if (p == IntPtr.Zero) return IntPtr.Zero;
        Marshal.Copy(bytes, 0, p, bytes.Length);
        GlobalUnlock(h);
        return h;
    }
    public static bool Publish(string text, byte[] dib) {
        if (!OpenClipboard(IntPtr.Zero)) return false;
        try {
            if (!EmptyClipboard()) return false;
            byte[] utf16 = System.Text.Encoding.Unicode.GetBytes(text + "\0");
            IntPtr h1 = CopyToGlobal(utf16);
            if (h1 == IntPtr.Zero || SetClipboardData(13, h1) == IntPtr.Zero) return false;
            IntPtr h2 = CopyToGlobal(dib);
            if (h2 == IntPtr.Zero || SetClipboardData(8, h2) == IntPtr.Zero) return false;
            return true;
        } finally { CloseClipboard(); }
    }
}
'@
}
# 2x2, 24 bpp, BI_RGB: 40-byte header + 16 bytes of bottom-up rows (2 pixels padded to 4).
$dib = New-Object byte[] 56
[BitConverter]::GetBytes([int]40).CopyTo($dib, 0)
[BitConverter]::GetBytes([int]2).CopyTo($dib, 4)
[BitConverter]::GetBytes([int]2).CopyTo($dib, 8)
[BitConverter]::GetBytes([int16]1).CopyTo($dib, 12)
[BitConverter]::GetBytes([int16]24).CopyTo($dib, 14)
[BitConverter]::GetBytes([int]16).CopyTo($dib, 20)
for ($i = 40; $i -lt 56; $i++) { $dib[$i] = 0x80 }
$imageBefore = @(Get-LogLines | Where-Object { $_ -like '*clip_ingest_image*' }).Count
$ingestBefore = @(Get-LogLines | Where-Object { $_ -like '*clip_ingest *' }).Count
$oversize = 'x' * 17000
# Liveness control: a normal copy right before the fixture proves the watcher is
# seeing clipboard updates. Without it, "nothing was ingested" below could also mean
# "the watcher never looked" -- the assertion would pass for the wrong reason.
$ingestBeforeControl = @(Get-LogLines | Where-Object { $_ -like '*clip_ingest *' }).Count
Set-ClipboardRetry 'accept-clip-overlimit-control'
Check 'the watcher is alive before the over-limit round' `
    (Wait-LogCount '*clip_ingest *' ($ingestBeforeControl + 1) 5000)
$imageBefore = @(Get-LogLines | Where-Object { $_ -like '*clip_ingest_image*' }).Count
$ingestBefore = @(Get-LogLines | Where-Object { $_ -like '*clip_ingest *' }).Count
Check 'the oversize text + DIB fixture is published' ([FFClipFixture]::Publish($oversize, $dib))
Start-Sleep -Milliseconds 1500
$imageAfter = @(Get-LogLines | Where-Object { $_ -like '*clip_ingest_image*' }).Count
$ingestAfter = @(Get-LogLines | Where-Object { $_ -like '*clip_ingest *' }).Count
Check 'the DIB is not captured in place of the oversize text' `
    (($imageAfter -eq $imageBefore) -and ($ingestAfter -eq $ingestBefore)) `
    "image=$imageBefore->$imageAfter ingest=$ingestBefore->$ingestAfter"

$dbHelper = Join-Path $WorkDir 'clipboard-db-fixture.cjs'
[IO.File]::WriteAllText($dbHelper, @'
const { DatabaseSync } = require('node:sqlite');
const db = new DatabaseSync(process.argv[2]);
db.exec('PRAGMA busy_timeout=3000');
const mode = process.argv[3];
if (mode === 'reject') db.exec("CREATE TRIGGER ux_reject BEFORE INSERT ON clipboard_items BEGIN SELECT RAISE(ABORT, 'ux-ingest-denied'); END");
if (mode === 'allow') db.exec('DROP TRIGGER ux_reject');
console.log(JSON.stringify({
  snippets: db.prepare('SELECT name,content FROM clipboard_snippets ORDER BY id DESC').all(),
  failed: db.prepare("SELECT count(*) AS n FROM clipboard_items WHERE content='accept-ingest-failure'").get().n
}));
db.close();
'@, [Text.Encoding]::ASCII)
function Read-FixtureDb([string]$Mode = 'read') {
    $out = & node --no-warnings $dbHelper (Join-Path $app 'data\focusflow_clipboard.db') $Mode
    if ($LASTEXITCODE -ne 0) { throw "database fixture failed: $Mode" }
    $out | ConvertFrom-Json
}

[FF]::ClickWindow($h, 474, 256, 1)
$pageBody = "accept-snippet-page`r`n`t  "
$beforeSnippet = @(Get-LogLines | Where-Object { $_ -like '*clip_ingest *' }).Count
Set-ClipboardRetry $pageBody
Check 'page snippet fixture preserves its trailing whitespace' ((Get-ClipboardRetry) -ceq $pageBody)
Check 'page snippet source is captured' (Wait-LogCount '*clip_ingest *' ($beforeSnippet + 1) 5000)
[FF]::ClickWindow($h, 300, 300, 1)
[FF]::ClickWindow($h, 306, 256, 1)
Start-Sleep -Milliseconds 400
$dbState = Read-FixtureDb
Check 'page saves snippet body without trimming' ($dbState.snippets.Count -eq 1 -and $dbState.snippets[0].content -ceq $pageBody)
[FF]::ClickWindow($h, 138, 217, 1)
Set-ClipboardRetry 'accept-before-snippet-copy'
[FF]::ClickWindow($h, 54, 256, 1)
Start-Sleep -Milliseconds 500
Check 'page copies snippet body byte-for-byte' ((Get-ClipboardRetry) -ceq $pageBody)
[FF]::ClickWindow($h, 54, 217, 1)

$popupBody = "accept-snippet-popup`n`t  "
$beforePopupSnippet = @(Get-LogLines | Where-Object { $_ -like '*clip_ingest *' }).Count
Set-ClipboardRetry $popupBody
Check 'popup snippet fixture preserves its trailing whitespace' ((Get-ClipboardRetry) -ceq $popupBody)
Check 'popup snippet source is captured' (Wait-LogCount '*clip_ingest *' ($beforePopupSnippet + 1) 5000)
$pal = Open-ClipPalette
$scale = [FF]::GetDpiForSystem() / 96.0
$menuX = [int][Math]::Round(140 * $scale)
$menuY = [int][Math]::Round(110 * $scale)
$pr = Get-Rect ([IntPtr]$pal.Popup)
# Real right click: the menu must open at a known point and own the foreground.
if ($pal.Popup -gt 0) {
    [void][FF]::SetForegroundWindow([IntPtr]$pal.Popup)
    Start-Sleep -Milliseconds 250
    [FF]::RightClick(($pr.X + $menuX), ($pr.Y + $menuY))
    Start-Sleep -Milliseconds 350
}
# Select by item rectangle, not by a fixed number of Downs: the item under the
# physical cursor is already hot when a menu opens, which made the old key count
# land on Stats/Transform depending on where the previous round left the cursor.
$menu = [FF]::ByClass([uint32]$p.Id, '#32768')
Check 'popup context menu opens' ($menu -ne [IntPtr]::Zero)
if ($menu -ne [IntPtr]::Zero) {
    Check 'popup menu item save-as-snippet is clickable' ([FF]::ClickMenuItem($menu, 3))
}
Start-Sleep -Milliseconds 400
$dbState = Read-FixtureDb
Check 'popup menu saves snippet body without trimming' ($dbState.snippets.Count -eq 2 -and $dbState.snippets[0].content -ceq $popupBody)
[FF]::PostMessage([IntPtr]$pal.Edit, 0x0100, [IntPtr]0x09, [IntPtr]::Zero) | Out-Null
Start-Sleep -Milliseconds 200
Set-ClipboardRetry 'accept-before-popup-snippet-copy'
[FF]::PostMessage([IntPtr]$pal.Popup, 0x0100, [IntPtr]0x0D, [IntPtr]::Zero) | Out-Null
Start-Sleep -Milliseconds 600
Check 'popup copies snippet body byte-for-byte' ((Get-ClipboardRetry) -ceq $popupBody)

Set-ClipboardRetry 'accept-before-ingest-failure'
Start-Sleep -Milliseconds 500
Read-FixtureDb 'reject' | Out-Null
$pal = Open-ClipPalette
try {
    Set-ClipboardRetry 'accept-ingest-failure'
    Check 'database really rejects the captured write' (Wait-LogMatch '*ux-ingest-denied*' 5000)
    Check 'failed capture is absent from the database' ((Read-FixtureDb).failed -eq 0)
    Save-WindowShot ([IntPtr]$pal.Popup) (Join-Path $WorkDir 'ingest-failed-popup.png')
    [FF]::PostMessage([IntPtr]$pal.Edit, 0x0100, [IntPtr]0x1B, [IntPtr]::Zero) | Out-Null
    Start-Sleep -Milliseconds 400
    Save-WindowShot $h (Join-Path $WorkDir 'ingest-failed-page.png')
} finally {
    Read-FixtureDb 'allow' | Out-Null
}
$beforeRecovery = @(Get-LogLines | Where-Object { $_ -like '*clip_ingest *' }).Count
Set-ClipboardRetry 'accept-ingest-recovery'
Check 'capture resumes after the write failure' (Wait-LogCount '*clip_ingest *' ($beforeRecovery + 1) 5000)
Save-WindowShot $h (Join-Path $WorkDir 'ingest-recovered-page.png')

Stop-App $Exe
if ($null -ne $orig) { try { Set-ClipboardRetry $orig } catch { } }
Show-Summary
