# Phase 2 — Local IP Surface + Presence

**Goal:** anything that can open a localhost WebSocket or send `curl` becomes a participant, agents become addressable by name, and the first real harness auto-joins with zero configuration.

**Why now:** Phase 1 proved the semantics with scripts; this phase makes real agents first-class. Presence + auto-join is where adoption stops being a chore.

## Scope

**In**
- localhost WebSocket (`ws://127.0.0.1:47114/ws`) + REST (history, rooms, who) in `crates/busd/src/ws.rs` (axum)
- Same NDJSON envelopes over WS; REST for replay/inspection (curl-able)
- Presence: heartbeat every 5 s, TTL 15 s, `bus who` shows agent/model/current file/ctx usage; broker-emitted `presence` join/leave/timeout events
- Mentions with routing: `@codex@repo`, `@human`, `@room:reviewers`
- Threads: `thread`/`reply_to` fan-out and rendering (room shows thread summary, not every reply)
- Token auth on the HTTP face (loopback-only bind, per-agent token)
- **Shims** for the harnesses used daily (Codex / Claude Code first): shell hook that exports `AGENT_ID`, registers, joins `#repo:$(basename $PWD)`, and runs the background `bus recv` inbox loop

**Out**
- Claims/locks/artifacts (Phase 3), TUI/web human UI (Phase 4), MCP (Phase 4), LAN (Phase 5)

## Prerequisites

- Phase 1 exit criteria green
- The WS/REST face reuses the same sequencer and store — no second write path

## Build plan

1. `ws.rs`: axum router with `/ws` (upgrade, handshake reuse), `/rooms`, `/rooms/:name/history?since=`, `/who`
2. `presence.rs`: heartbeat task, TTL sweeper, `presence` envelopes into the room log, last-seen in `agents`
3. Mention parser in `proto` (routing targets on envelope: room / agent / human)
4. Thread registry queries (`messages_thread` index already exists)
5. `shims/codex/join.sh`, `shims/claude-code/join.sh` + docs for wiring as a session-start hook
6. `bus recv` gains `--as` identity handling from `AGENT_ID` env

## The proof (demo)

Open a Codex CLI session and a Claude Code session in the same repo. Both auto-join `#repo:<name>` with no manual steps. From either one:

```bash
bus send --room '#repo:demo' --kind chat --text '@codex@demo can you review my change?'
```

The Codex session's inbox loop prints `[bus] …` into its notice stream. `bus who` shows both agents with models and ctx usage.

## How to reach it (exit criteria)

| Test | Pass criterion |
|---|---|
| Auto-join | Both harnesses joined the repo room with zero manual config, verified across 10 session starts |
| Cursor resume | Kill the consumer mid-stream, reconnect with `welcome` cursor: no message lost or duplicated |
| Presence TTL | Kill an agent without leave: `presence.timeout` emitted within 15 s + TTL slack; `bus who` drops it |
| WS parity | The Phase 1 ordering/throughput tests re-run over WS: same numbers within 2× on latency |
| REST replay | `curl /rooms/x/history?since=N` returns the exact NDJSON envelopes, cursor-stable |
| Mention routing | `@agent` message delivered only to that agent's inbox + room log; `@human` reaches the human queue |

## Risks

- **Harness notice streams differ** — the shim's stdout-print trick is the fallback; where a structured channel exists (plugin event), prefer it. 029d's PTY supervisor is the escape hatch for harnesses with neither.
- **Identity collision** when two sessions of the same harness run in one repo — resolve with ordinal suffix (`codex@proj#2`), decided here, formalized in an ADR.
