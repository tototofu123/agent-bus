# Answer one agent-bus message with an NVIDIA-hosted model.
#
# Wired to the bus by `bus listen --exec`:
#
#   bus listen --room '#repo:agent-bus' --exec "powershell -NoProfile -ExecutionPolicy Bypass -File tools\nvidia-responder.ps1"
#
# Contract (see crates/bus-cli/src/listen.rs):
#   stdin   -> the message text
#   env     -> AGENT_BUS_ROOM, AGENT_BUS_SEQ, AGENT_BUS_FROM, AGENT_BUS_KIND
#   stdout  -> the reply, trimmed and sent back as a `chat` with reply_to
#
# The key is read from $env:NVIDIA_API_KEY, falling back to
# ~/.agent-bus/nvidia.token. It is never printed or committed.
#
# Optional knobs:
#   NVIDIA_MODEL   default z-ai/glm-5.3-flash (reasoning model, ~8 s for a
#                  short answer on the free tier)
#   NVIDIA_MAX_TOKENS   default 700
#   NVIDIA_HISTORY      default ~/.agent-bus/nvidia-chat.json

$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8

$model   = if ($env:NVIDIA_MODEL) { $env:NVIDIA_MODEL } else { 'z-ai/glm-5.3-flash' }
$maxOut  = if ($env:NVIDIA_MAX_TOKENS) { [int]$env:NVIDIA_MAX_TOKENS } else { 700 }
$history = if ($env:NVIDIA_HISTORY) { $env:NVIDIA_HISTORY } else { Join-Path $env:USERPROFILE '.agent-bus\nvidia-chat.json' }
$apiBase = if ($env:NVIDIA_API_BASE) { $env:NVIDIA_API_BASE } else { 'https://integrate.api.nvidia.com/v1' }

$key = $env:NVIDIA_API_KEY
if (-not $key) {
    $tokenFile = Join-Path $env:USERPROFILE '.agent-bus\nvidia.token'
    if (Test-Path $tokenFile) { $key = (Get-Content $tokenFile -Raw).Trim() }
}
if (-not $key) {
    [Console]::Error.WriteLine('nvidia-responder: no NVIDIA_API_KEY and no ~/.agent-bus/nvidia.token')
    exit 1
}

$text = (New-Object System.IO.StreamReader([Console]::OpenStandardInput(), [Text.Encoding]::UTF8)).ReadToEnd()
if ([string]::IsNullOrWhiteSpace($text)) { exit 0 }

$room = if ($env:AGENT_BUS_ROOM) { $env:AGENT_BUS_ROOM } else { '#unknown' }
$from = if ($env:AGENT_BUS_FROM) { $env:AGENT_BUS_FROM } else { 'human' }
$seq  = if ($env:AGENT_BUS_SEQ) { $env:AGENT_BUS_SEQ } else { '?' }

# One flat log of {room, role, content} entries, filtered per room. A dictionary
# keyed by room would need ConvertFrom-Json -AsHashtable, which Windows
# PowerShell 5.1 does not have.
$entries = @()
if (Test-Path $history) {
    try {
        $raw = Get-Content $history -Raw
        if (-not [string]::IsNullOrWhiteSpace($raw)) {
            $parsed = $raw | ConvertFrom-Json
            if ($null -ne $parsed) { $entries = @($parsed) }
        }
    } catch {
        [Console]::Error.WriteLine("nvidia-responder: ignoring unreadable history: $_")
    }
}
$turns = @(
    $entries |
        Where-Object { $_.room -eq $room } |
        Select-Object -Last 12 |
        ForEach-Object { @{ role = [string]$_.role; content = [string]$_.content } }
)
$turns += @{ role = 'user'; content = $text }

$system = @"
You are the assistant behind the agent-bus room $room. A person is typing from a
Telegram bridge; your reply is relayed to the room and to their chat.
Be direct. At most three short paragraphs. Plain text only: no markdown tables,
no code fences unless the person asks for code. If you do not know, say so.
The current message is from $from at seq $seq.
"@

$messages = @(@{ role = 'system'; content = $system }) + $turns

$body = @{
    model       = $model
    messages    = $messages
    max_tokens  = $maxOut
    temperature = 0.4
} | ConvertTo-Json -Depth 10

try {
    # Windows PowerShell 5.1 needs the assembly loaded before the types resolve.
    Add-Type -AssemblyName System.Net.Http
    # HttpClient rather than Invoke-RestMethod: Windows PowerShell 5.1 decodes
    # a JSON response as Latin-1 when the content type has no charset, which
    # double-encodes every non-ASCII character. Reading the bytes and decoding
    # UTF-8 explicitly is deterministic.
    $client = New-Object System.Net.Http.HttpClient
    $client.Timeout = [TimeSpan]::FromSeconds(180)
    $client.DefaultRequestHeaders.Authorization =
        New-Object System.Net.Http.Headers.AuthenticationHeaderValue('Bearer', $key)
    $content = New-Object System.Net.Http.StringContent($body, [Text.Encoding]::UTF8, 'application/json')
    $resp = $client.PostAsync("$apiBase/chat/completions", $content).GetAwaiter().GetResult()
    $bytes = $resp.Content.ReadAsByteArrayAsync().GetAwaiter().GetResult()
    $json = [Text.Encoding]::UTF8.GetString($bytes)
    if (-not $resp.IsSuccessStatusCode) {
        [Console]::Error.WriteLine("nvidia-responder: HTTP $([int]$resp.StatusCode): $json")
        exit 1
    }
    $respData = $json | ConvertFrom-Json
} catch {
    [Console]::Error.WriteLine("nvidia-responder: request failed: $($_.Exception.Message)")
    exit 1
} finally {
    if ($client) { $client.Dispose() }
}

$reply = ''
if ($respData.choices -and $respData.choices.Count -gt 0) {
    $reply = [string]$respData.choices[0].message.content
}
$reply = $reply.Trim()
if ([string]::IsNullOrWhiteSpace($reply)) {
    [Console]::Error.WriteLine('nvidia-responder: model returned no content')
    exit 1
}

# Persist the exchange only after a usable answer, so a failed call does not
# poison the history with a dangling user turn. The log is capped globally and
# filtered per room on read.
$entries = @($entries) + @(
    [pscustomobject]@{ room = $room; role = 'user'; content = $text },
    [pscustomobject]@{ room = $room; role = 'assistant'; content = $reply }
)
if ($entries.Count -gt 200) { $entries = $entries[($entries.Count - 200)..($entries.Count - 1)] }
try {
    $dir = Split-Path -Parent $history
    if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
    ConvertTo-Json -InputObject @($entries) -Depth 6 | Set-Content -Path $history -Encoding utf8
} catch {
    [Console]::Error.WriteLine("nvidia-responder: could not save history: $_")
}

# Write UTF-8 bytes directly: redirecting PowerShell's text stream uses the
# console code page, which mangles non-ASCII before the bus ever sees it.
$stdout = New-Object System.IO.StreamWriter([Console]::OpenStandardOutput(), (New-Object System.Text.UTF8Encoding($false)))
$stdout.WriteLine($reply)
$stdout.Flush()
