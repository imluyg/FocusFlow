<#
.SYNOPSIS
  Plugin package install / details / uninstall drive a real window and the real filesystem.

.DESCRIPTION
  Covers P2 steps 17 and 19 end to end: the install button writes the package into the data
  dir and records source + checksum; the detail page opens from the row; its uninstall button
  asks first, and "delete settings" removes both the package and plugins/state/<stem>/.

  The fixture package is generated here with focusflow-plugin (pack writes the deterministic
  STORED zip the host reads; a Compress-Archive zip is refused by the reader). Build the tool
  first: cargo build --release -p focusflow-plugin-tool

  Keep this file ASCII-only: PowerShell 5.1 reads a BOM-less UTF-8 .ps1 as ANSI, and a CJK
  comment whose last byte is a GBK lead byte swallows the following line.

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-install.ps1
#>
param(
    [string]$Exe = '',
    [string]$WorkDir = ''
)

. "$PSScriptRoot\accept-lib.ps1"

if (-not $Exe) { $Exe = Get-DefaultExe }
if (-not $WorkDir) { $WorkDir = Join-Path (Get-RepoRoot) 'target\accept\install' }
$tool = Join-Path (Get-RepoRoot) 'target\release\focusflow-plugin.exe'

Stop-App
$app = Join-Path $WorkDir 'app'
$src = Join-Path $WorkDir 'pkg-src\e2e_demo'
New-Item -ItemType Directory -Force -Path $src | Out-Null
$pluginText = @(
    'PLUGIN_NAME = "E2E demo"',
    'PLUGIN_VERSION = "1.0.0"',
    'PLUGIN_AUTHOR = "probe"',
    'PLUGIN_DESC = "Installed and uninstalled by the acceptance run."',
    '',
    'function get_view()',
    '    return {',
    '        title = "E2E demo",',
    '        widgets = {',
    '            { type = "heading", text = "Installed from a package" },',
    '        },',
    '    }',
    'end'
)
[System.IO.File]::WriteAllLines((Join-Path $src 'plugin.lua'), $pluginText, (New-Object System.Text.UTF8Encoding $false))
$zip = Join-Path $WorkDir 'pkg-src\e2e_demo.zip'
Remove-Item $zip -ErrorAction SilentlyContinue
Check 'the packaging tool is built (cargo build --release -p focusflow-plugin-tool)' (Test-Path $tool) $tool
if (-not (Test-Path $tool)) { Show-Summary }
& $tool pack $src | Out-Null
Check 'pack produced a package' (Test-Path $zip) $zip
if (-not (Test-Path $zip)) { Show-Summary }

New-AppDir $app @('[collector]', 'enabled = false')
Remove-Item (Join-Path $WorkDir 'focusflow.log') -ErrorAction SilentlyContinue
$pkg = Join-Path $app 'data\plugins\e2e_demo.zip'
$state = Join-Path $app 'data\plugins\state\e2e_demo'
"exe : $Exe"
"zip : $zip"
"app : $app"

function Wait-Log([string]$Pattern, [int]$TimeoutMs = 12000) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalMilliseconds -lt $TimeoutMs) {
        if (@(Read-AppLog $WorkDir) -match $Pattern) { return $true }
        Start-Sleep -Milliseconds 150
    }
    $false
}

function Wait-File([string]$Path, [int]$TimeoutMs = 12000) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalMilliseconds -lt $TimeoutMs) {
        if (Test-Path $Path) { return $true }
        Start-Sleep -Milliseconds 150
    }
    $false
}

$zipHash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant()

$p = Start-App $Exe $app $WorkDir @{ FOCUSFLOW_INSTALL_PLUGIN = (Resolve-Path $zip).Path }
$h = Wait-Window $p.Id
Check 'main window appears' ($h -ne [IntPtr]::Zero)
if ($h -eq [IntPtr]::Zero) { Stop-App; Show-Summary }

Click-ViewTab $h 4
Start-Sleep -Milliseconds 700

# Install button: plugin_install_rect(card) with a 1200x860 client.
[FF]::ClickWindow($h, 1102, 229, 1)
Check 'the install operation reported back' (Wait-Log 'OP_DONE' 15000) ''
Check 'the package landed in the data dir' (Wait-File $pkg) $pkg
Check 'the install record exists' (Wait-File (Join-Path $state 'meta.json')) ''

$meta = $null
$metaPath = Join-Path $state 'meta.json'
if (Test-Path $metaPath) { $meta = Get-Content $metaPath -Raw -Encoding UTF8 | ConvertFrom-Json }
Check 'the record carries the package hash' (
    $null -ne $meta -and $meta.content_hash -match [regex]::Escape($zipHash)) "got=$($meta.content_hash) want=sha256:$zipHash"
Check 'the record names the stem' ($null -ne $meta -and $meta.stem -eq 'e2e_demo') "stem=$($meta.stem)"

$shot1 = Join-Path $WorkDir 'after-install.png'
Check 'the list redraws after install' (Save-WindowShot $h $shot1) $shot1

# Row is the first extension row: list_top 252 + header 46 => y 298..340.
# Its "detail" button sits at row_right-208, y+9 (58x22).
[FF]::ClickWindow($h, 989, 318, 1)
Check 'the detail page opens' (Wait-Log 'detail_open=1') ''
$shot2 = Join-Path $WorkDir 'detail.png'
Check 'the detail page renders' (Save-WindowShot $h $shot2) $shot2

# Uninstall button top-right of the detail card, then "delete settings" (middle button).
[FF]::ClickWindow($h, 1124, 231, 1)
Start-Sleep -Milliseconds 500
$shot3 = Join-Path $WorkDir 'uninstall-prompt.png'
Check 'the confirm box renders' (Save-WindowShot $h $shot3) $shot3
[FF]::ClickWindow($h, 600, 495, 1)
Check 'uninstall reports back' (Wait-Log 'uninstall_done=1' 10000) ''

Check 'the package is gone' (-not (Test-Path $pkg)) $pkg
Check 'the state dir is gone (settings deleted)' (-not (Test-Path $state)) $state
$shot4 = Join-Path $WorkDir 'after-uninstall.png'
Check 'the list redraws after uninstall' (Save-WindowShot $h $shot4) $shot4

$log = Read-AppLog $WorkDir
Check 'no error lines in the app log' (-not (@($log) -match "(?i)error|$(Get-FailWord)")) ''

[void][FF]::PostMessage($h, 0x8009, [IntPtr]::Zero, [IntPtr]::Zero)
Start-Sleep -Milliseconds 1200
Stop-App
Show-Summary
