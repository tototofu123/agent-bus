# Roadmap

Five phases, each ending in a working, demonstrable system. **No phase starts until the previous phase's exit criteria pass.** Each phase doc has the full scope, build plan, and acceptance tests:

| Phase | Title | Delivers | Proof (the demo) | Exit criteria |
|---|---|---|---|---|
| [1](docs/phases/phase-1-broker-cli-rooms.md) | Broker + CLI + rooms | `agent-busd` daemon, Unix socket / named pipe / TCP loopback transport, SQLite WAL store, sequencer, `bus send/recv/tail/who/history` | Two shell scripts converse in one room: one emits `task.offer`, the other replies `question` → `answer` | 8 writers × 10k msgs → seq strictly monotonic, zero gaps; p99 latency < 1 ms; `kill -9` × 1000 → no torn messages |
| [2](docs/phases/phase-2-localip-presence.md) | Local IP + presence | localhost WS + REST, presence heartbeat/TTL, mentions, threads, per-harness auto-join shims | Claude Code and Codex CLI sessions auto-join `#repo:<name>` and can `@mention` each other | A real harness joins with zero manual config; reconnect with `welcome` cursor loses no messages |
| [3](docs/phases/phase-3-claims-handoff-artifacts.md) | Claims, handoff, artifacts | file-lock registry (leased globs), `task.offer/claim/result`, artifact store by hash, context compaction | Two agents race for the same file; loser is told who holds it; 80%-context agent hands off mid-task | Chaos test: never two holders on intersecting paths; handoff packet ≤ 4k tokens with goal + open questions always present |
| [4](docs/phases/phase-4-mcp-human-policy.md) | MCP face + human surfaces + policy | `bus-mcp` (stdio + Streamable HTTP, via `rmcp`), ratatui TUI, localhost web, `policy` kind, approval gates, `untrusted` marking | One approval surface for every harness; human ships a task from the TUI | Injection red-team: zero side-effecting tool calls with only-untrusted cause; role-floor deny is typed and public |
| [5](docs/phases/phase-5-lan-federation.md) | LAN & federation | mDNS discovery, mTLS, cross-machine rooms, CI one-shot participant, path translation | Phone/desktop/CI agents share one room over LAN | Two machines, one room, same ordering guarantees; A2A-style interop sketch documented |

## How to reach each phase (the short version)

1. **Reach Phase 1** by building the boring core: `crates/proto` (envelope + NDJSON framing) → `crates/busd` (socket listener, room registry, single-writer sequencer, SQLite) → `crates/bus-cli`. Prove it with shell scripts, not harnesses. *If the semantics survive shell scripts, every richer harness will be easy.*
2. **Reach Phase 2** by adding the localhost WS/REST face and the auto-join shims, then measuring whether auto-join actually happens without thinking about it.
3. **Reach Phase 3** by adding claims + handoff and running the chaos test. *This is the phase that pays for itself: no more clobbered edits, no more tasks dying at 80% context.*
4. **Reach Phase 4** only then — by that point the MCP face is a ~200-line projection, and MCP-native harnesses get a nicer integration without anything else changing.
5. **Reach Phase 5** when a real need spans machines (028's device node, CI, a second desk).

## Current status

- [x] Idea + deep dive (ideas repo 029)
- [x] This repo: protocol spec, architecture, phased plan, ADRs
- [~] **Phase 1 — gates run at criteria; throughput still fails**
  - [x] `proto` (envelope, kinds, NDJSON framing with line cap, role floors, tokens) + 11 unit tests
  - [x] `busd` store: WAL migrations, single-writer sequencer, cursor replay
  - [x] `busd` session state machine + router: fan-out, drop policy, 60 msg/s per agent
  - [x] `bus` CLI: register/join/leave/send/recv/tail/who/history, TCP + named pipe
  - [x] `send_batch` protocol op: N envelopes in one frame, one commit (7k → ~31k msg/s)
  - [x] Exit gates run at their stated criteria: ordering (8×10k gap-free), latency p99 0.49 ms, replay 50k in 0.35 s, backpressure 100k+100, crash safety 10k storm, rate limit typed, framing regression fixed
  - [ ] **Throughput fails: ~31k msg/s vs the 50k criterion.** Remaining cost is per-envelope work in the session layer (frame parse, envelope construction, fan-out clone), not the transport or the limiter. Gate left failing, not relaxed.
- [ ] Phase 2
- [ ] Phase 3
- [ ] Phase 4
- [ ] Phase 5

Phase 1 is not closed until the throughput gate passes.
