# Installs agent-bus for the desktop workflow:
#
#   1. double-click "Agent Bus" on the Desktop  -> server runs
#   2. close the window                         -> server keeps running
#   3. open it again, press "Stop server"       -> server stops
#   4. nothing starts with Windows
#
# Rule 4 is enforced here: any autostart entry pointing at busd is removed.

$ErrorActionPreference = 'Stop'

$Repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$Install = Join-Path $env:LOCALAPPDATA 'agent-bus'
$Bin = Join-Path $Install 'bin'

Write-Host "agent-bus installer" -ForegroundColor Cyan
Write-Host "  repo    $Repo"
Write-Host "  install $Install"

# --- build -------------------------------------------------------------------

# Note: PowerShell variable names are case-insensitive, so this loop variable
# must not be $bin -- that would clobber $Bin.
$needBuild = $false
foreach ($exe in @('busd.exe', 'bus.exe')) {
    if (-not (Test-Path (Join-Path $Repo "target\release\$exe"))) { $needBuild = $true }
}
if ($needBuild) {
    Write-Host '  building release binaries...' -ForegroundColor Yellow
    Push-Location $Repo
    try { & cargo build --release } finally { Pop-Location }
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
} else {
    Write-Host '  release binaries already built'
}

# --- install -----------------------------------------------------------------

New-Item -ItemType Directory -Force -Path $Bin | Out-Null
foreach ($exe in @('busd.exe', 'bus.exe')) {
    Copy-Item (Join-Path $Repo "target\release\$exe") (Join-Path $Bin $exe) -Force
    Write-Host "  installed $exe" -ForegroundColor Green
}
Copy-Item (Join-Path $PSScriptRoot 'AgentBusControl.ps1') (Join-Path $Install 'AgentBusControl.ps1') -Force
Write-Host '  installed AgentBusControl.ps1' -ForegroundColor Green

# --- desktop entry -----------------------------------------------------------

$Desktop = [Environment]::GetFolderPath('Desktop')
$Bat = Join-Path $Desktop 'Agent Bus.bat'
@(
    '@echo off',
    'start "" powershell -NoProfile -WindowStyle Hidden -STA -ExecutionPolicy Bypass -File "%LOCALAPPDATA%\agent-bus\AgentBusControl.ps1"'
) | Set-Content -Path $Bat -Encoding ASCII
Write-Host "  created $Bat" -ForegroundColor Green

# --- no autostart ------------------------------------------------------------

$removed = @()

$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
if (Test-Path $runKey) {
    $props = Get-ItemProperty -Path $runKey -ErrorAction SilentlyContinue
    foreach ($p in $props.PSObject.Properties) {
        if ($p.Name -notlike 'PS*' -and "$($p.Value)" -match '(?i)busd|agent-bus') {
            Remove-ItemProperty -Path $runKey -Name $p.Name -ErrorAction SilentlyContinue
            $removed += "Run key: $($p.Name)"
        }
    }
}

$startup = [Environment]::GetFolderPath('Startup')
foreach ($f in Get-ChildItem -Path $startup -ErrorAction SilentlyContinue) {
    if ("$($f.FullName)" -match '(?i)busd|agent-bus') {
        Remove-Item $f.FullName -Force -ErrorAction SilentlyContinue
        $removed += "Startup: $($f.Name)"
    }
}

Get-ScheduledTask -ErrorAction SilentlyContinue |
    Where-Object { $_.TaskName -match '(?i)busd|agent-bus' } |
    ForEach-Object {
        Unregister-ScheduledTask -TaskName $_.TaskName -Confirm:$false -ErrorAction SilentlyContinue
        $removed += "Task: $($_.TaskName)"
    }

if ($removed.Count -eq 0) {
    Write-Host '  autostart: none (correct)' -ForegroundColor Green
} else {
    Write-Host '  removed autostart entries:' -ForegroundColor Yellow
    $removed | ForEach-Object { Write-Host "    $_" }
}

Write-Host ''
Write-Host 'Done. Double-click "Agent Bus" on your Desktop.' -ForegroundColor Cyan