# Windows Terminal agent lanes: one window, three panes, different sizes.
#
# Panes cannot have their own font size. What they CAN do is each run a
# different profile, and font size is a per-profile setting -- so a pane gets
# its size from the profile it runs. That is the whole trick.
#
# Writes three profiles into Windows Terminal's settings.json, idempotently:
# an existing agent-* profile is updated in place rather than duplicated.
#
#   pwsh -File setup-terminal-profiles.ps1
#   pwsh -File setup-terminal-profiles.ps1 -Main 14 -Sub 9
#
# Then launch the lanes with agent-lanes.bat, or from the wt CLI:
#
#   wt -w new -p "agent-main" ";" split-pane -p "agent-sub" -s 34% ";" split-pane -p "agent-report" -s 50%
#
# Semicolons separate the nested split subcommands. They must be quoted, or
# the shell passes them as bare arguments and wt rejects the whole line.

[CmdletBinding()]
param(
    [int]$Main = 12,
    [int]$Sub = 8
)

$ErrorActionPreference = 'Stop'

$settings = Join-Path $env:LOCALAPPDATA 'Packages\Microsoft.WindowsTerminal_8wekyb3d8bbwe\LocalState\settings.json'
if (-not (Test-Path $settings)) {
    throw "Windows Terminal settings.json not found at $settings"
}

$raw = [System.IO.File]::ReadAllText($settings)
$json = $raw | ConvertFrom-Json

$wanted = @(
    @{ name = 'agent-main'; size = $Main; guid = '{7a1c0e11-9b2d-4f31-8c44-1e5a6b7c8d90}'; title = 'MAIN' }
    @{ name = 'agent-sub'; size = $Sub; guid = '{7a1c0e11-9b2d-4f31-8c44-1e5a6b7c8d91}'; title = 'SUB' }
    @{ name = 'agent-report'; size = $Sub; guid = '{7a1c0e11-9b2d-4f31-8c44-1e5a6b7c8d92}'; title = 'REPORT' }
)

$added = 0
$updated = 0
foreach ($w in $wanted) {
    $existing = $json.profiles.list | Where-Object { $_.name -eq $w.name } | Select-Object -First 1
    if ($existing) {
        $existing.font.size = $w.size
        $existing.tabTitle = $w.title
        $existing.suppressApplicationTitle = $true
        $updated++
    }
    else {
        $json.profiles.list = @($json.profiles.list) + @([pscustomobject]@{
                name = $w.name
                guid = $w.guid
                font = @{ face = 'Cascadia Mono'; size = $w.size }
                colorScheme = 'Campbell'
                suppressApplicationTitle = $true
                tabTitle = $w.title
            })
        $added++
    }
}

$backup = "$settings.bak-$(Get-Date -Format 'yyyyMMdd-HHmmss')"
Copy-Item $settings $backup -Force

# Round-trip through ConvertTo-Json. Windows Terminal rewrites this file on
# every settings change anyway, so its formatting is not load-bearing.
$out = $json | ConvertTo-Json -Depth 40
[System.IO.File]::WriteAllText($settings, $out)

# Prove the result parses before claiming success.
$check = [System.IO.File]::ReadAllText($settings) | ConvertFrom-Json
$made = $check.profiles.list | Where-Object { $_.name -like 'agent-*' }
Write-Host "profiles now present:" -ForegroundColor Cyan
$made | ForEach-Object { Write-Host ("  {0,-14} {1} pt" -f $_.name, $_.font.size) }
Write-Host "  added $added, updated $updated; backup at $backup" -ForegroundColor DarkGray
Write-Host ''
Write-Host 'Restart any open Windows Terminal windows so they pick up the new profiles.' -ForegroundColor Yellow