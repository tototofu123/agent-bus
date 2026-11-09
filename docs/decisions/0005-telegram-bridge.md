# 0005 — Telegram bridge as a `bus` subcommand

## Status

Accepted (2026-11-09). Implements agent-bus #8 and #12.

## Context

Issues #8 and #12 described a Telegram bridge (`telegram.rs`, a
responder loop, Markdown → HTML replies) that did not exist anywhere
in the repo. The Perihel bot is a separate personal agent on a VM; the
bus is a multi-agent collaboration tool on Windows. The bridge belongs
to the bus, not to Perihel.

## Decision

- The bridge is `bus telegram`, a subcommand of the existing CLI, in
  `crates/bus-cli/src/telegram.rs`. It long-polls the Bot API and
  speaks the bus protocol over the same TCP loopback connection as
  every other `bus` command.
- Human messages enter the room as `chat` envelopes from a dedicated
  `human@telegram` identity (registered like any agent), because the
  bus takes the sender from `hello` — one connection cannot post as
  two agents, so the bridge holds two connections.
- The bridge answers nothing itself. If no other agent is in the
  room it says so plainly; if agents are there but silent past the
  reply window it says that too. A generic `bus listen --exec`
  responder is a follow-up, not part of this change.
- Rendering is a pure function (`render_markdown` + `chunk_html`),
  unit-tested, with no network. Tables are aligned `<pre>` blocks;
  chunks split on block boundaries and never split a tag.
- The bridge needs its own bot token. One token has exactly one
  `getUpdates` poller; sharing the Perihel bot's token would break
  both bots. The token is passed by flag, env var, or file, and is
  never logged.
- HTTP uses `reqwest` with `rustls-tls` (pure Rust, no OpenSSL) so
  the Windows build needs nothing extra.

## Consequences

- The bridge must run on the same machine as `busd` (localhost-only
  bus). It cannot run on the Perihel VM.
- A restart skips the unconfirmed Telegram backlog, so messages sent
  while the bridge was down are not delivered. That is the safe
  default; a durable inbox would need the offset persisted.
- The bridge loop is integration-tested against a mock Telegram
  server and a fake busd, not against the live Telegram API.
