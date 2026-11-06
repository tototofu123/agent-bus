# Telegram bridge

`bus telegram` connects one bus room to one Telegram chat. It is a **human
surface**, not another agent: text typed in Telegram enters the bus as
`@human` with `trust=human`, and bus traffic for the room is pushed to the
chat.

```
Telegram chat  ──▶  bus telegram  ──▶  @human in #repo:agent-bus
               ◀──                ◀──  every envelope in that room
```

## Setup

The bot token is read from `$AGENT_BUS_HOME/telegram.token`
(`~/.agent-bus/telegram.token` by default), or from
`AGENT_BUS_TELEGRAM_TOKEN`. It is never a command-line flag, so it does not
land in shell history.

```powershell
New-Item -ItemType Directory -Force $env:USERPROFILE\.agent-bus | Out-Null
Set-Content -NoNewline $env:USERPROFILE\.agent-bus\telegram.token '123456:ABC...'
```

Then, with the broker running:

```powershell
bus telegram --room '#repo:agent-bus'
```

The first Telegram chat that messages the bot becomes the owner. The binding
is written once to `~/.agent-bus/telegram.json`; every other chat is ignored.
To re-pair, stop the bridge, delete `telegram.json`, and message the bot from
the new chat (the bot token itself stays put).

## Commands

| Message | Effect |
|---|---|
| plain text | `chat` into the room |
| `/send <text>` | `chat` into the room (escape hatch for text starting with `/`) |
| `/claim <glob>[,<glob>...] [ttl]` | `task.claim` with `files` and `claim_ttl_s` (default 300 s) |
| `/status <text>` | `status` into the room |
| `/who` | who is in the room: harness, model, role, and `online`/`offline` |
| `/rooms` | every room and its current seq |
| `/history [n]` | replay the last `n` messages (default 20, max 50) |
| `/room` | show the bridged room and identity |
| `/id` | show the Telegram chat id |
| `/help` | command list |

Every send is acknowledged with its server `seq`, so the Telegram side has the
same cursor as the room. The ack also says whether anyone is listening:
`→ chat seq 7 · 2 agents online`, or `· no agent online` when the room has no
other live connection.

## Who answers

The bridge is transport. It posts what you type and shows what the room says;
it never invents a reply. Something has to sit behind the bus door:

```powershell
bus listen --room '#repo:agent-bus' --exec 'my-responder'
```

`listen` tails the room and, for each new message, runs the command with the
text on stdin and these variables set: `AGENT_BUS_ROOM`, `AGENT_BUS_SEQ`,
`AGENT_BUS_FROM`, `AGENT_BUS_KIND`, `AGENT_BUS_ID`, `AGENT_BUS_BIN` (the path to
the running `bus` binary) and `AGENT_BUS_PORT`. Whatever the command prints
on stdout is sent back as `chat` with `reply_to` set to the message id. An empty
stdout, a non-zero exit, or a timeout sends nothing.

The handler can be any process — a shell script, a local model, or an HTTP call.
Nothing about it is Telegram-specific, so the same listener serves the TUI, a
web surface, or another bot.

A working NVIDIA-backed responder ships in
[`tools/nvidia-responder.ps1`](../tools/nvidia-responder.ps1): it reads
`NVIDIA_API_KEY` (or `~/.agent-bus/nvidia.token`), keeps six turns of per-room
history in `~/.agent-bus/nvidia-chat.json`, and returns the model's `content`.

```powershell
bus listen --room '#repo:agent-bus' `
  --exec 'powershell -NoProfile -ExecutionPolicy Bypass -File tools\nvidia-responder.ps1'
```

Set `NVIDIA_MODEL` to pick a model (`z-ai/glm-5.3-flash` by default; the free
tier is a reasoning model, so expect a few seconds per reply).

### Local agent with tools

A raw chat completion cannot see the weather or the repo. For tool calling, use
a local agent CLI instead — the reply path stays exactly the same:

```powershell
bus listen --room '#repo:agent-bus' --agent assistant@bus `
  --exec 'C:\Users\you\.agent-bus\omp-responder.cmd' --timeout 300
```

[`tools/omp-responder.ps1`](../tools/omp-responder.ps1) runs `omp -p` with
`read,glob,grep,web_search` (read-only; no write/bash), an ephemeral session
(`--no-session`), and the room folder as its cwd. It is local by design: close
the listener and the agent is gone.

Folder routing and standing memory live outside the repo, in
`~/.agent-bus/workspaces.json`:

```json
{
  "default_folder": "C:\\Users\\you\\.agent-bus\\workspace",
  "rooms": { "#repo:agent-bus": "C:\\...\\agent-bus" },
  "extra_dirs": ["C:\\...\\obsidian-data\\Projects\\Antigravity"]
}
```

On first run the responder creates `~/.agent-bus/workspace/{notes,inbox,scratch}`
and `memory.md`. `memory.md` is injected into every prompt; the folders are
read-only context. The last 8 `chat` messages are read from the broker each
time, so the conversation remembers itself without a persistent model session.

Measured locally: ~30–70 s for a web-backed answer on the free NVIDIA tier,
including the tool call.

The listener registers as `listener@bus` by default (`--agent` to change it)
with harness `bus-listen` and cap `respond`, joins the room, and therefore shows
up in `/who` as online while it runs.

`/who` marks an agent `online` while it holds a live broker connection and
`offline` once the last one closes. `bus who` shows the same, plus how long ago
the agent was last seen, its rooms and its caps. Presence is per broker
process: after a restart everyone is offline until they reconnect, which is the
truthful answer.

## What is stored where

| Path | Contents |
|---|---|
| `~/.agent-bus/telegram.token` | Telegram bot token (secret; never committed) |
| `~/.agent-bus/telegram.json` | `chat_id`, update `offset`, last room |
| `~/.agent-bus/tokens/_human.token` | the bridge's bus token |

The bus token is the same token file every other `bus` command uses. The
bridge registers `@human` with harness `telegram`; the broker assigns it
`role=peer` and stamps outbound messages `trust=human`. `policy` is still the
only kind gated on human trust, and it stays reserved for the TUI/web approval
surface.

## Delivery semantics

- **Bus → Telegram** uses a live `tail` subscription. If the broker restarts,
  the bridge reconnects every 2 seconds. Tail is not cursor-replayed, so
  messages produced during the gap are not pushed to Telegram; `/history`
  remains the durable record.
- **Telegram → bus** opens one short connection per message (`hello` →
  `send` → `bye`). At human rates the connection cost is noise, and a stuck
  Telegram poll cannot block the send path.
- **Telegram retries:** the update `offset` is persisted after each handled
  update. A crash can replay at most the update being handled, never the whole
  backlog.

## Scope note

`/claim` emits the protocol's `task.claim` with the declared files and lease
TTL. The leased lock registry and the broker-emitted `task.grant` /
`task.deny` replies are Phase 3; until then the claim is a durable, auditable
protocol message rather than an enforced lock. The bridge will pick up
`task.grant` / `task.deny` automatically when the broker starts emitting them.

## Security

- Only the claimed chat id is accepted. Messages from other chats are logged
  and dropped.
- Telegram is not an authenticated channel to the broker in the cryptographic
  sense; the bridge is a local process with a local bus token. Anyone who can
  read the bot token can impersonate the bot, and anyone who can write
  `telegram.json` can re-bind the chat. Treat both files as secrets.
- Bus messages are shown as data. The bridge never runs a command from a bus
  message and never turns bus text into a Telegram command.
