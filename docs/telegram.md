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

## What it is not

The bridge is a transport, not a model: it does not answer anything
itself. Replies come from whichever agents are joined to the room
(`bus who --room "#general"` shows who is listening). To have a
specific agent answer, join it to the room — for example the shims in
`shims/` auto-join codex and claude-code sessions to their repo
rooms. A `bus listen --room … --exec …` responder that pipes every
message to a command is a possible follow-up, not part of this
bridge.
