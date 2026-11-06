# Answer one agent-bus message with a local agent CLI (omp) that can call tools.
#
#   bus listen --room '#repo:agent-bus' --exec "powershell -NoProfile -ExecutionPolicy Bypass -File tools\omp-responder.ps1"
#
# Why a CLI and not a raw chat completion: a plain model cannot see the weather,
# the repo, or the notes. `omp` already has read/glob/grep/web_search wired to a
# model (and to the NVIDIA provider configured in ~/.omp), so the reply path is:
#
#   Telegram -> bridge -> bus -> listen -> omp (tools) -> bus -> bridge -> Telegram
#
# Local only, by design: the process and the ephemeral omp session die when you
# close the listener. Memory comes from the room's own history plus
# ~/.agent-bus/workspace/memory.md, both read fresh on every message.
#
# Contract (see crates/bus-cli/src/listen.rs):
#   stdin   -> the message text
#   env     -> AGENT_BUS_ROOM, AGENT_BUS_SEQ, AGENT_BUS_FROM, AGENT_BUS_KIND,
#              AGENT_BUS_BIN, AGENT_BUS_PORT
#   stdout  -> the reply, trimmed and sent back as `chat` with reply_to
#
# Knobs:
#   OMP_BIN        default `omp`
#   OMP_MODEL      optional --model (default: omp's default role)
#   OMP_HISTORY    how many earlier messages to include (default 8)
#   OMP_TIMEOUT    seconds before the child is killed (default 300)
#   OMP_TOOLS      default read,glob,grep,web_search (read-only + web)
#   OMP_WORKSPACES default ~/.agent-bus/workspaces.json

$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$utf8 = New-Object System.Text.UTF8Encoding($false)

$text = (New-Object System.IO.StreamReader([Console]::OpenStandardInput(), [Text.Encoding]::UTF8)).ReadToEnd()
if ([string]::IsNullOrWhiteSpace($text)) { exit 0 }

$room = if ($env:AGENT_BUS_ROOM) { $env:AGENT_BUS_ROOM } else { '#unknown' }
$from = if ($env:AGENT_BUS_FROM) { $env:AGENT_BUS_FROM } else { 'human' }
$seq = if ($env:AGENT_BUS_SEQ) { [int]$env:AGENT_BUS_SEQ } else { 0 }
$bus = if ($env:AGENT_BUS_BIN) { $env:AGENT_BUS_BIN } else { 'bus' }
$omp = if ($env:OMP_BIN) { $env:OMP_BIN } else { 'omp' }
$maxHistory = if ($env:OMP_HISTORY) { [int]$env:OMP_HISTORY } else { 8 }
$timeoutMs = if ($env:OMP_TIMEOUT) { [int]$env:OMP_TIMEOUT * 1000 } else { 300000 }
$tools = if ($env:OMP_TOOLS) { $env:OMP_TOOLS } else { 'read,glob,grep,web_search' }

$userHome = $env:USERPROFILE
$configPath = if ($env:OMP_WORKSPACES) { $env:OMP_WORKSPACES } else { Join-Path $userHome '.agent-bus\workspaces.json' }
$workspace = Join-Path $userHome '.agent-bus\workspace'

function Initialize-Workspace([string]$root) {
    foreach ($sub in @('notes', 'inbox', 'scratch')) {
        $p = Join-Path $root $sub
        if (-not (Test-Path $p)) { New-Item -ItemType Directory -Force -Path $p | Out-Null }
    }
    $mem = Join-Path $root 'memory.md'
    if (-not (Test-Path $mem)) {
        @"
# Agent memory

Anything in this file is included in every reply as standing context.
Keep it short; edit it yourself. The bot can read it but cannot write it.

## Facts
- (nothing yet)
"@ | Set-Content -Path $mem -Encoding utf8
    }
}

if (-not (Test-Path $workspace)) { New-Item -ItemType Directory -Force -Path $workspace | Out-Null }
Initialize-Workspace $workspace

if (-not (Test-Path $configPath)) {
    $seed = [ordered]@{
        default_folder = $workspace
        rooms = [ordered]@{}
        extra_dirs = @()
    }
    ConvertTo-Json -InputObject $seed -Depth 5 | Set-Content -Path $configPath -Encoding utf8
}

$cfg = $null
try { $cfg = Get-Content $configPath -Raw | ConvertFrom-Json } catch {
    [Console]::Error.WriteLine("omp-responder: ignoring unreadable workspaces config: $_")
}

# Room -> folder, then default, then the workspace itself. A stale mapping must
# not take the bot down, so a missing folder falls back rather than failing.
$folder = $null
if ($cfg -and $cfg.rooms) {
    $prop = $cfg.rooms.PSObject.Properties | Where-Object { $_.Name -eq $room } | Select-Object -First 1
    if ($prop) { $folder = [string]$prop.Value }
}
if (-not $folder -and $cfg -and $cfg.default_folder) { $folder = [string]$cfg.default_folder }
if (-not $folder -or -not (Test-Path $folder)) { $folder = $workspace }

$extras = @()
if ($cfg -and $cfg.extra_dirs) { $extras += @($cfg.extra_dirs | ForEach-Object { [string]$_ }) }
$extras += $workspace
$extras = @($extras | Where-Object { $_ -and (Test-Path $_) } | Select-Object -Unique)

# Memory: the room's own history, read from the broker. This is the "something
# remembers" part that survives the stateless child process: the bus log is the
# transcript, and memory.md is the long-lived note the human controls.
$history = @()
if ($seq -gt 0) {
    $since = [Math]::Max(0, $seq - 30)
    try {
        $raw = & $bus history --room $room --since $since --limit 30 --full 2>$null
        foreach ($line in @($raw)) {
            if ([string]::IsNullOrWhiteSpace($line)) { continue }
            try { $e = $line | ConvertFrom-Json } catch { continue }
            if ($e.kind -ne 'chat') { continue }
            if (-not $e.text) { continue }
            if ([int]$e.seq -eq $seq) { continue }
            $body = ([string]$e.text).Trim()
            if ($body.Length -gt 600) { $body = $body.Substring(0, 600) + '...' }
            $history += "$($e.from.id) ($($e.seq)): $body"
        }
    } catch {
        [Console]::Error.WriteLine("omp-responder: history unavailable: $_")
    }
}
$history = @($history | Select-Object -Last $maxHistory)
$historyText = if ($history.Count) { $history -join "`n" } else { '(no earlier messages)' }

$memFile = Join-Path $workspace 'memory.md'
$memory = ''
if (Test-Path $memFile) { $memory = (Get-Content $memFile -Raw).Trim() }
if ($memory.Length -gt 4000) { $memory = $memory.Substring(0, 4000) }

$system = @"
You are the local assistant for the agent-bus room $room, replying through a Telegram bridge.
You have read-only file tools and web search. Use web_search for current facts (weather, prices, news, releases) and read/glob/grep for the workspace folders. Try a tool before saying you cannot access something.
Workspace: $folder
Other readable folders: $($extras -join '; ')
Standing memory:
$memory
Rules: plain text, at most three short paragraphs unless asked for more; no markdown tables; no code fences unless code is requested; if you are unsure, say what you checked.
"@

$prompt = @"
Recent room messages (oldest first):
$historyText

New message from $from at seq ${seq}:
$text
"@

# Windows command-line quoting. Prompts carry spaces, quotes and newlines, and
# Start-Process would re-split them; ProcessStartInfo with an explicitly quoted
# argument string is the portable .NET Framework 4.8 way.
function ConvertTo-QuotedArg([string]$s) {
    if ($s -eq '') { return '""' }
    if ($s -notmatch '[\s"]') { return $s }
    $escaped = [regex]::Replace($s, '(\\*)"', '$1$1\"')
    $escaped = [regex]::Replace($escaped, '(\\+)$', '$1$1')
    return '"' + $escaped + '"'
}

$ompArgs = @(
    '-p', $prompt,
    '--auto-approve',
    '--no-session',
    '--mode', 'text',
    '--cwd', $folder,
    '--tools', $tools,
    '--append-system-prompt', $system
)
if ($env:OMP_MODEL) { $ompArgs += @('--model', $env:OMP_MODEL) }
foreach ($d in $extras) { $ompArgs += @('--add-dir', $d) }

$argLine = ($ompArgs | ForEach-Object { ConvertTo-QuotedArg $_ }) -join ' '

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $omp
$psi.Arguments = $argLine
$psi.WorkingDirectory = $folder
$psi.UseShellExecute = $false
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.StandardOutputEncoding = [Text.Encoding]::UTF8
$psi.StandardErrorEncoding = [Text.Encoding]::UTF8

try {
    $p = [System.Diagnostics.Process]::Start($psi)
} catch {
    [Console]::Error.WriteLine("omp-responder: could not start ${omp}: $_")
    exit 1
}

$outTask = $p.StandardOutput.ReadToEndAsync()
$errTask = $p.StandardError.ReadToEndAsync()
if (-not $p.WaitForExit($timeoutMs)) {
    try { $p.Kill() } catch { }
    [Console]::Error.WriteLine("omp-responder: omp exceeded $($timeoutMs / 1000)s")
    exit 1
}
$stdout = $outTask.GetAwaiter().GetResult()
$stderr = $errTask.GetAwaiter().GetResult()

if ($p.ExitCode -ne 0) {
    [Console]::Error.WriteLine("omp-responder: omp exited $($p.ExitCode): $($stderr.Trim())")
    exit 1
}

# Strip ANSI colour codes; Telegram would show them as literal escapes.
$esc = [char]27
$reply = ($stdout -replace "$esc\[[0-9;?]*[ -/]*[@-~]", '').Trim()
if ([string]::IsNullOrWhiteSpace($reply)) {
    [Console]::Error.WriteLine("omp-responder: omp produced no reply: $($stderr.Trim())")
    exit 1
}

# UTF-8 bytes directly; PowerShell's text stream uses the console code page.
$out = New-Object System.IO.StreamWriter([Console]::OpenStandardOutput(), $utf8)
$out.WriteLine($reply)
$out.Flush()
