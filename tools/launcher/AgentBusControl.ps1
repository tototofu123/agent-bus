# Agent Bus control panel.
#
# Start and stop the broker. Closing this window leaves the broker running --
# that is the whole point, so the window is just a dashboard and a pair of
# buttons. Nothing here registers an autostart entry.

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$ErrorActionPreference = 'Stop'

# --- configuration -----------------------------------------------------------

$Port = if ($env:AGENT_BUS_PORT) { [int]$env:AGENT_BUS_PORT } else { 47113 }

# Two distinct locations, deliberately:
#   install dir -- the binaries and this script (LOCALAPPDATA, per-user apps)
#   bus home    -- the database, pid file and tokens (USERPROFILE, persistent state)
$BusHome = if ($env:AGENT_BUS_HOME) {
    $env:AGENT_BUS_HOME
} else {
    Join-Path $env:USERPROFILE '.agent-bus'
}
$InstallDir = if ($env:AGENT_BUS_INSTALL) {
    $env:AGENT_BUS_INSTALL
} else {
    Join-Path $env:LOCALAPPDATA 'agent-bus'
}
$PidFile = Join-Path $BusHome 'busd.pid'
$DbFile = Join-Path $BusHome 'history.sqlite'

# Prefer the installed copy; fall back to a source checkout so the panel is
# usable straight out of the repo.
$Busd = Join-Path $InstallDir 'bin\busd.exe'
if (-not (Test-Path $Busd)) {
    $candidate = Join-Path (Join-Path $PSScriptRoot '..\..') 'target\release\busd.exe'
    if (Test-Path $candidate) { $Busd = (Resolve-Path $candidate).Path }
}

# --- helpers -----------------------------------------------------------------

function Test-BrokerUp {
    param([int]$Port)
    $client = $null
    try {
        $client = New-Object System.Net.Sockets.TcpClient
        $async = $client.BeginConnect('127.0.0.1', $Port, $null, $null)
        if (-not $async.AsyncWaitHandle.WaitOne(600)) { return $false }
        $client.EndConnect($async)
        return $true
    } catch {
        return $false
    } finally {
        if ($client) { $client.Close() }
    }
}

function Get-BrokerPid {
    if (-not (Test-Path $PidFile)) { return $null }
    try {
        $raw = (Get-Content $PidFile -TotalCount 1 -ErrorAction Stop).Trim()
        if ($raw -match '^\d+$') { return [int]$raw }
    } catch { }
    return $null
}

function Get-BrokerProc {
    param([int]$ProcessId)
    if (-not $ProcessId) { return $null }
    try {
        $p = Get-Process -Id $ProcessId -ErrorAction Stop
        # Guard against pid reuse: only ever stop something named busd.
        if ($p.ProcessName -ne 'busd') { return $null }
        return $p
    } catch {
        return $null
    }
}

function Format-Uptime {
    param($Proc)
    if (-not $Proc) { return '-' }
    $span = (Get-Date) - $Proc.StartTime
    if ($span.TotalDays -ge 1) {
        return ('{0}d {1}h' -f [int]$span.TotalDays, $span.Hours)
    }
    if ($span.TotalHours -ge 1) {
        return ('{0}h {1}m' -f [int]$span.TotalHours, $span.Minutes)
    }
    return ('{0}m {1}s' -f [int]$span.TotalMinutes, $span.Seconds)
}

function Start-Broker {
    if (-not (Test-Path $Busd)) {
        [System.Windows.Forms.MessageBox]::Show(
            "busd.exe not found.`r`n`r`nLooked for:`r`n  $Busd`r`n`r`nRun install.ps1, or build the workspace first.",
            'Agent Bus', 'OK', 'Warning') | Out-Null
        return
    }
    New-Item -ItemType Directory -Force -Path $BusHome | Out-Null

    # Detached and windowless: no console to close, and nothing ties this
    # process's lifetime to the panel. That is what makes "close the window,
    # server keeps running" true.
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Busd
    $psi.Arguments = "--port $Port"
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true

    $proc = New-Object System.Diagnostics.Process
    $proc.StartInfo = $psi
    [void]$proc.Start()

    # Drain the pipes so a chatty broker can never block on a full buffer.
    $outLog = Join-Path $BusHome 'busd.out.log'
    $proc.BeginOutputReadLine()
    $proc.BeginErrorReadLine()

    for ($i = 0; $i -lt 40; $i++) {
        Start-Sleep -Milliseconds 150
        if (Test-BrokerUp -Port $Port) { return }
        if ($proc.HasExited) { break }
    }
}

function Stop-Broker {
    $proc = Get-BrokerProc -ProcessId (Get-BrokerPid)
    if (-not $proc) {
        # Nothing trustworthy to stop. Clear a stale pid file if the port is
        # genuinely free, so the next start is not confused by it.
        if (-not (Test-BrokerUp -Port $Port)) {
            Remove-Item $PidFile -Force -ErrorAction SilentlyContinue
            [System.Windows.Forms.MessageBox]::Show(
                'The broker is not running.', 'Agent Bus', 'OK', 'Information') | Out-Null
        } else {
            [System.Windows.Forms.MessageBox]::Show(
                "Port $Port is in use, but not by a busd process this panel started.`r`nLeaving it alone.",
                'Agent Bus', 'OK', 'Warning') | Out-Null
        }
        return
    }
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    for ($i = 0; $i -lt 40; $i++) {
        Start-Sleep -Milliseconds 150
        if (-not (Test-BrokerUp -Port $Port)) { return }
    }
}

# --- UI ----------------------------------------------------------------------

$form = New-Object System.Windows.Forms.Form
$form.Text = 'Agent Bus'
$form.ClientSize = New-Object System.Drawing.Size(430, 268)
$form.StartPosition = 'CenterScreen'
$form.FormBorderStyle = 'FixedSingle'
$form.MaximizeBox = $false

function New-Label {
    param([string]$Text, [int]$X, [int]$Y, [int]$W, [int]$H, [float]$Size = 9, [switch]$Bold)
    $l = New-Object System.Windows.Forms.Label
    $l.Text = $Text
    $l.Location = New-Object System.Drawing.Point($X, $Y)
    $l.Size = New-Object System.Drawing.Size($W, $H)
    $l.Font = New-Object System.Drawing.Font('Segoe UI', $Size, $(if ($Bold) { [System.Drawing.FontStyle]::Bold } else { [System.Drawing.FontStyle]::Regular }))
    $l.AutoSize = $false
    return $l
}

$lblTitle = New-Label 'agent-bus' 16 14 300 24 13 -Bold
$lblStatus = New-Label '' 16 42 398 34 19 -Bold
$lblPort = New-Label '' 16 90 200 20
$lblPid = New-Label '' 220 90 194 20
$lblUptime = New-Label '' 16 112 200 20
$lblDb = New-Label '' 220 112 194 20

$btnStart = New-Object System.Windows.Forms.Button
$btnStart.Text = 'Start server'
$btnStart.Location = New-Object System.Drawing.Point(16, 150)
$btnStart.Size = New-Object System.Drawing.Size(190, 40)
$btnStart.BackColor = [System.Drawing.Color]::FromArgb(37, 130, 73)
$btnStart.ForeColor = [System.Drawing.Color]::White
$btnStart.FlatStyle = 'Flat'
$btnStart.Font = New-Object System.Drawing.Font('Segoe UI', 10, [System.Drawing.FontStyle]::Bold)

$btnStop = New-Object System.Windows.Forms.Button
$btnStop.Text = 'Stop server'
$btnStop.Location = New-Object System.Drawing.Point(224, 150)
$btnStop.Size = New-Object System.Drawing.Size(190, 40)
$btnStop.BackColor = [System.Drawing.Color]::FromArgb(155, 58, 58)
$btnStop.ForeColor = [System.Drawing.Color]::White
$btnStop.FlatStyle = 'Flat'
$btnStop.Font = New-Object System.Drawing.Font('Segoe UI', 10, [System.Drawing.FontStyle]::Bold)

$lblNote = New-Label 'Closing this window leaves the server running.' 16 200 398 18
$lblNote.ForeColor = [System.Drawing.Color]::DimGray

$lblNote2 = New-Label 'Does not start with Windows.' 16 218 398 18
$lblNote2.ForeColor = [System.Drawing.Color]::DimGray

foreach ($c in @($lblTitle, $lblStatus, $lblPort, $lblPid, $lblUptime, $lblDb, $btnStart, $btnStop, $lblNote, $lblNote2)) {
    $form.Controls.Add($c)
}

function Update-Status {
    $up = Test-BrokerUp -Port $Port
    $proc = Get-BrokerProc -ProcessId (Get-BrokerPid)
    if ($up) {
        $lblStatus.Text = 'RUNNING'
        $lblStatus.ForeColor = [System.Drawing.Color]::FromArgb(37, 130, 73)
        $lblPid.Text = if ($proc) { 'pid ' + $proc.Id } else { 'pid unknown' }
        $lblUptime.Text = 'up ' + (Format-Uptime $proc)
        $btnStart.Enabled = $false
        $btnStop.Enabled = $true
    } else {
        $lblStatus.Text = 'STOPPED'
        $lblStatus.ForeColor = [System.Drawing.Color]::FromArgb(120, 120, 120)
        $lblPid.Text = 'pid -'
        $lblUptime.Text = 'up -'
        $btnStart.Enabled = $true
        $btnStop.Enabled = $false
    }
    $lblPort.Text = "port $Port"
    $lblDb.Text = 'db ' + (Split-Path $DbFile -Leaf)
}

$btnStart.Add_Click({ Start-Broker; Update-Status })
$btnStop.Add_Click({ Stop-Broker; Update-Status })

$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 2000
$timer.Add_Tick({ Update-Status })
$timer.Start()

$form.Add_FormClosing({
    # Deliberately do not touch the broker: the panel is a dashboard, and the
    # server is expected to outlive it.
    $timer.Stop()
    $timer.Dispose()
})

Update-Status

# Opening the panel is the "start" gesture: if the broker is not up, bring it
# up now. Idempotent, so re-opening while it already runs does nothing.
if (-not (Test-BrokerUp -Port $Port)) {
    Start-Broker
    Update-Status
}

[void]$form.ShowDialog()