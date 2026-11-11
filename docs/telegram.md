# Telegram bridge

`bus telegram` connects one Telegram chat to one bus room, both ways.
It runs next to `busd` — the bus is localhost-only, so the bridge
must run on the same machine (usually Windows).

## What it does

- Long-polls the bot for messages in the configured chat and posts
  each human message into the room as a `chat` envelope from
  `human@telegram` (issue #8).
- Tails the room for agent replies and sends each one back to the
  chat, rendered as Telegram HTML with the replying agent as a bold
  heading (issue #12).
- If nobody else is in the room, it answers honestly instead of
  looking like a reply: "Delivered to #room (seq N) — no agent is
  listening in that room right now." If agents are there but none
  replies within the window (default 60 s), it says that too.
- It never echoes its own traffic, and it skips messages from other
  bots (Telegram bots cannot see each other's messages anyway).

## Setup

1. Make a **dedicated bot** for the bridge in BotFather (`/newbot`).
   Do NOT reuse the Perihel bot's token: Telegram delivers each
   update to exactly one `getUpdates` poller, so a second poller
   with the same token would steal the Perihel bot's messages (and
   vice versa). The Perihel bot is a personal agent; this bridge is
   the multi-agent room's mouth — two bots, two jobs.
2. Add the bot to the Telegram group and copy the chat id (group ids
   are negative, e.g. `-5312549577`).
3. Build and run, from the repo on the Windows machine:

```powershell
cargo build -p bus-cli --release
$env:TELEGRAM_BOT_TOKEN = "<bridge bot token>"
.\target\release\bus.exe telegram --room "#general" --chat -5312549577
```

Or pass the token explicitly (`--token`) or from a file
(`--token-file`, one line, no trailing spaces). The token is never
printed or logged. The bridge registers two agent ids on first run
(`telegram@bridge` and `human@telegram`) and stores their tokens
under `$AGENT_BUS_HOME\tokens\` like any other `bus register`.

## Replies and rendering

Outbound agent replies are Markdown rendered to Telegram HTML:

- `## heading` → bold, `**bold**`, `*italic*`, `` `code` ``,
  `[label](url)` → link, fenced code → `<pre>`, pipe tables →
  aligned monospace `<pre>` (Telegram has no real tables).
- Messages are chunked at 4000 characters on block boundaries, so
  tags are never split; a long `<pre>` block is re-wrapped per chunk.
- Link previews are disabled: the bridge forwards agent text, and a
  preview card for every URL in it is noise.

## Options

```powershell
bus telegram --room "#general" --chat -5312549577 `
  --poll-secs 25 --reply-timeout-secs 60 `
  --bridge-agent "telegram@bridge" --human-agent "human@telegram"
```

`--api-base` only exists for tests (it points the client at a mock
Telegram server). On a restart the bridge skips the unconfirmed
backlog, so it never re-posts old messages.

## Answering with `bus listen`

The bridge is a transport, not a model: it does not answer anything
itself. Replies come from whichever agents are joined to the room
(`bus who --room "#general"` shows who is listening). To have a
specific agent answer, join it to the room — for example the shims in
`shims/` auto-join codex and claude-code sessions to their repo
rooms. Or run a responder:

```powershell
bus listen --room "#general" --exec "my-responder"
```

`listen` tails the room and, for each new message, runs the command
with the message text on stdin and the envelope as environment
variables (`AGENT_BUS_ROOM`, `AGENT_BUS_SEQ`, `AGENT_BUS_FROM`,
`AGENT_BUS_KIND`, `AGENT_BUS_ID`, plus `AGENT_BUS_BIN` and
`AGENT_BUS_PORT` so the handler can reach the same broker). Whatever
the handler prints on stdout becomes the reply (trimmed, capped at
16,000 chars, sent back as a `chat` with `reply_to`); a non-zero
exit or empty stdout sends nothing. The handler is a normal process,
so any model, script or CLI can answer — nothing about `listen` is
Telegram-specific, so the same listener serves the TUI, a web page,
or any other human surface.

In a group, several listeners can share a room. Each one answers
only when mentioned by name:

```powershell
bus listen --room "#general" --agent assistant@bus --exec "my-responder" `
  --mention-only --default
```

`--mention-only` answers only messages that mention `@assistant`
(the handle defaults to the agent id before `@`, `--name` overrides
it). `--default` also answers messages that mention nobody, so a
bare question reaches the default agent instead of nobody. Mentions
are `@name` tokens at a word boundary (`user@example.com` is not a
mention); the leading `@name` is stripped before the text reaches
the handler. `--only-human` answers only messages from human
surfaces (the Telegram bridge posts as one), so agents in a shared
room never answer each other in a loop. The listener never answers
its own replies.

Two working responders ship in [`tools/`](../tools/):

- [`nvidia-responder.ps1`](../tools/nvidia-responder.ps1) — answers
  with an NVIDIA-hosted model (key from `$env:NVIDIA_API_KEY` or
  `~/.agent-bus/nvidia.token`; default model `z-ai/glm-5.3-flash`).
  Run it with
  `bus listen --room '#repo:agent-bus' --exec 'powershell -NoProfile -ExecutionPolicy Bypass -File tools\nvidia-responder.ps1'`.
- [`omp-responder.ps1`](../tools/omp-responder.ps1) — runs `omp -p`
  with the room as context, so an OpenHands-style agent answers.

The listener registers as `listener@bus` by default (`--agent` to
change it), with harness `bus-listen` and cap `respond`. `--timeout`
(seconds, default 120) kills a handler that runs too long;
`--once` handles one message, then exits (handy for smoke tests).

The bridge strips its own `@username` from group text before
posting to the room, so a message that wakes the bot
("@perihelcloudbot what is the weather?") reaches the listeners as
just the question.

## What it is not

The bridge is a transport, not a model: it does not answer anything
itself. Replies come from whichever agents are joined to the room
(`bus who --room "#general"` shows who is listening).
