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

Stop-App $Exe
New-AppDir $app @('[collector]', 'enabled = false')
Set-ConfigLine $app 'hotkey' 'clipboard' $hotCombo
Set-ConfigLine $app 'clipboard' 'enabled' 'true'

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
# Auto-paste is opt-in; with the default config nothing may be injected at all.
Check 'auto-paste stays off by default' `
    (@(Get-LogLines | Where-Object { $_ -like '*clip_autopaste*' }).Count -eq 0)

$clip = ''
try { $clip = Get-ClipboardRetry } catch { }
Check 'Enter puts the selected (older) item on the clipboard' ($null -ne $clip -and $clip.Trim() -eq $older)

# Give the listener time to see the app's own write; it must be suppressed.
Start-Sleep -Milliseconds 1000
$after = @(Get-LogLines | Where-Object { $_ -like '*clip_ingest*' }).Count
Check 'the app does not capture its own clipboard write' ($after -eq $before)

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
Set-ClipboardRetry $marker
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

Stop-App $Exe
if ($null -ne $orig) { try { Set-ClipboardRetry $orig } catch { } }
Show-Summary
