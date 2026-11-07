<#
.SYNOPSIS
  Clipboard plugin API end to end: a real copy reaches a real plugin.

.DESCRIPTION
  Writes its own tiny ASCII plugin into a fresh COPY of desktop-native\probe:
  PLUGIN_API = 3, granted clipboard.read, subscribed with on("clipboard").
  The fixture runs as a lifecycle "always" plugin, so it subscribes at startup.

  Then the script sets the system clipboard from PowerShell (an external writer), waits
  for the app's own ingest line, and reads the plugin's state.json: the copied text must
  be there and the counter must be 1. That is the proof both halves are wired in the real
  program - the core tests only call the pump by hand.

  No grants are clicked in the UI: the script writes plugins\state\<stem>\grants.json
  itself, with the same two hashes the app computes (sha256 of the canonical sorted
  permission list / of the file bytes).

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\accept\accept-plugclip.ps1
#>
param(
    [string]$Exe = '',
    [string]$WorkDir = ''
)

. "$PSScriptRoot\accept-lib.ps1"

if (-not $Exe) { $Exe = Get-DefaultExe }
if (-not $WorkDir) { $WorkDir = Join-Path (Get-RepoRoot) 'target\accept\plugclip' }

$app = Join-Path $WorkDir 'app'
$logPath = Join-Path $WorkDir 'focusflow.log'
$marker = 'accept-clip-plugin-marker'
$stem = 'clip_fixture'

function Get-LogLines {
    @(Read-AppLog $WorkDir)
}

function Wait-LogMatch([string]$Pattern, [int]$TimeoutMs = 8000) {
    $deadline = (Get-Date).AddMilliseconds($TimeoutMs)
    while ((Get-Date) -lt $deadline) {
        if (@(Get-LogLines | Where-Object { $_ -like $Pattern }).Count -gt 0) { return $true }
        Start-Sleep -Milliseconds 150
    }
    $false
}

function Set-ClipboardRetry([string]$Text) {
    for ($i = 0; $i -lt 20; $i++) {
        try { Set-Clipboard -Value $Text; return } catch { Start-Sleep -Milliseconds 150 }
    }
    throw "Set-Clipboard failed after retries"
}

function Sha256Hex([byte[]]$Bytes) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try { return ([BitConverter]::ToString($sha.ComputeHash($Bytes))).Replace('-', '').ToLower() }
    finally { $sha.Dispose() }
}

Stop-App
New-AppDir $app @('[collector]', 'enabled = false')
Set-ConfigLine $app 'clipboard' 'enabled' 'true'

$pluginDir = Join-Path $app 'data\plugins'
New-Item -ItemType Directory -Force -Path $pluginDir | Out-Null
$luaPath = Join-Path $pluginDir "$stem.lua"
$lua = @'
PLUGIN_NAME = "ClipFixture"
PLUGIN_DESC = "clipboard acceptance fixture"
PLUGIN_API = 3
PLUGIN_LIFECYCLE = "always"
PLUGIN_PERMISSIONS = { "clipboard.read", "storage.read", "storage.write" }
function init()
  focusflow.on("clipboard", function(text)
    focusflow.store.set("event", text)
    local n = tonumber(focusflow.store.get("n") or "0") + 1
    focusflow.store.set("n", tostring(n))
    focusflow.store.flush()
  end)
  focusflow.store.flush()
end
'@
# UTF-8 without BOM: the Lua reader does not strip a BOM from PLUGIN_NAME's line.
[System.IO.File]::WriteAllText($luaPath, $lua, (New-Object System.Text.UTF8Encoding $false))

$declared = @('clipboard.read', 'storage.read', 'storage.write')
$canonical = ($declared | Sort-Object) -join "`n"
$stateDir = Join-Path $app "data\plugins\state\$stem"
New-Item -ItemType Directory -Force -Path $stateDir | Out-Null
$grants = [ordered]@{
    permission_hash = 'sha256:' + (Sha256Hex ([Text.Encoding]::UTF8.GetBytes($canonical)))
    content_hash    = 'sha256:' + (Sha256Hex ([IO.File]::ReadAllBytes($luaPath)))
    granted         = $declared
} | ConvertTo-Json -Compress
[System.IO.File]::WriteAllText(
    (Join-Path $stateDir 'grants.json'), $grants, (New-Object System.Text.UTF8Encoding $false))

$orig = $null
try { $orig = Get-Clipboard -Raw } catch { }

Remove-Item $logPath -ErrorAction SilentlyContinue
"exe : $Exe"
"app : $app  (plugin=$stem, API 3, clipboard.read granted)"

$p = Start-App $Exe $app $WorkDir
$h = Wait-Window $p.Id
Check 'main window appears' ($h -ne [IntPtr]::Zero)
if ($h -eq [IntPtr]::Zero) { Stop-App; Show-Summary }

Start-Sleep -Milliseconds 1500
Check 'the fixture plugin loaded at startup' (Wait-LogMatch '*plugins_loaded=*' 4000)

Set-ClipboardRetry $marker
Check 'the marker copy is captured' (Wait-LogMatch '*clip_ingest*' 6000)

# One tick (100 ms) delivers the event; give it a couple of ticks before reading the file.
$statePath = Join-Path $stateDir 'state.json'
$deadline = (Get-Date).AddSeconds(8)
$state = ''
while ((Get-Date) -lt $deadline) {
    if (Test-Path $statePath) {
        $state = [IO.File]::ReadAllText($statePath)
        if ($state -like "*$marker*") { break }
    }
    Start-Sleep -Milliseconds 200
}
Check 'the copied text reached the plugin callback' ($state -like "*$marker*") `
    "state='$state'"
Check 'the callback ran exactly once' ($state -match '"n"\s*:\s*"1"') `
    "state='$state'"

Stop-App $Exe
if ($null -ne $orig) { try { Set-ClipboardRetry $orig } catch { } }
Show-Summary
