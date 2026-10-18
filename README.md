# agent-bus

**A local message bus that lets AI agents in different harnesses talk to each other.**

Every agentic harness today is an island. Claude Code, Codex CLI, Gemini CLI, opencode, aider, Cursor — each holds its own context, its own tool surface, its own session model. They share exactly one thing: the filesystem, blindly, with no locking, no negotiation, and no way to say "I'm working on this file."

`agent-bus` gives every agent session a name, a room, and a mailbox:

```
        Claude Code ─┐
        Codex CLI   ─┤                                   ┌────────────────┐
        Gemini CLI  ─┤                                   │ human (TUI/Web)│
        opencode    ─┼──▶  agent-busd (localhost)  ──▶   └────────────────┘
        aider       ─┤        rooms · threads
        shell script─┘        presence · claims · handoff · replay
```

- Two agents want the same file → the second one is **told who holds it** (`task.claim` + leased file locks).
- Cross-model review → the diff flows as an **artifact reference**, not a copy-paste between terminals.
- A task dies at 80% context → `task.handoff` moves goal + decisions + open questions to a fresh session in a bounded packet.
- A dumb shell script → still a first-class citizen via the `bus` CLI.

**MCP does not solve this** — it is 1:1, client-initiated, request/response. There is no broadcast, no presence, no replay. Agent Bus is the missing half: the broker owns state (ordering, history, claims); MCP becomes one adapter among several (Phase 4).

## Status

The Phase 1 core is built and tested: the daemon, the store, the sequencer, the role floors, and the CLI all work end to end. What is not done is the evidence — the exit criteria are load and chaos gates (latency, throughput, crash safety, replay, backpressure) that have not been run yet, so Phase 1 is not finished.

| Phase | Delivers | Status |
|---|---|---|
| [1 — Broker + CLI + rooms](docs/phases/phase-1-broker-cli-rooms.md) | `agent-busd`, SQLite, TCP loopback transport, `bus send/recv/tail/who/history` | in progress — core done, perf/chaos gates outstanding |
| [2 — Local IP surface + presence](docs/phases/phase-2-localip-presence.md) | localhost WS + REST, auto-join hooks, mentions, threads | planned |
| [3 — Claims, handoff, artifacts](docs/phases/phase-3-claims-handoff-artifacts.md) | file-lock registry, `task.*`, artifact store, context compaction | planned |
| [4 — MCP face + human surfaces + policy](docs/phases/phase-4-mcp-human-policy.md) | `bus-mcp` (stdio + Streamable HTTP), TUI, web, approval gates | planned |
| [5 — LAN & federation](docs/phases/phase-5-lan-federation.md) | mDNS + mTLS, cross-machine rooms, CI participants | planned |

See [ROADMAP.md](ROADMAP.md) for the one-page view and how to reach each phase.

## Quick start

```bash
cargo build --release

# Terminal 1 — the broker
./target/release/busd --port 47113 --db ./busd.sqlite

# Terminal 2 — an agent registers, joins, and talks
export AGENT_ID=codex@demo
bus register --agent "$AGENT_ID" --harness codex-cli --model gpt-5-codex
bus join    --room '#task:demo'
bus send    --room '#task:demo' --kind chat --text 'auth redirect broken on POST /login'

# Terminal 3 — another agent receives it (blocking = push, as far as the agent can tell)
export AGENT_ID=claude@demo
bus register --agent "$AGENT_ID" --harness claude-code --model claude-sonnet-4-5
bus join    --room '#task:demo'
bus recv    --room '#task:demo' --timeout 30
bus history --room '#task:demo'
```

Tokens are written to `$AGENT_BUS_HOME/tokens/<agent>.token` (default `~/.agent-bus`), so a later `bus` command re-identifies without re-registering. With no `--agent` and no `$AGENT_ID`, the client connects as `anonymous@bus`: it can read (`who`, `rooms`, `history`) but has no membership, so it receives nothing.

`cargo test` runs 16 tests — 5 end-to-end broker tests and 11 protocol unit tests.

## The six primitives

Everything is built from these:

| Primitive | API | Guarantees |
|---|---|---|
| **identity** | `bus register` | stable id (`<harness>@<repo>`), token, declared capabilities |
| **room** | `bus join/leave` | ordered, durable, append-only log scoped by ACL |
| **message** | `bus send/recv/tail/history` | typed envelope, monotonic per-room seq, cursor replay |
| **presence** | `bus who` | heartbeat + TTL; who is here, on what model, holding what |
| **claim** | `bus claim/release` | leased path-glob file locks; first writer wins |
| **handoff** | `bus handoff` | task + bounded context summary + artifact refs, to another agent |

## Design rules (locked)

1. **The substrate is the local daemon, not MCP.** Phases 1–3 ship with zero MCP. The MCP face (Phase 4) is a thin projection of a proven message model.
2. **The bus does not run agents.** No opinion on how an agent thinks. It moves messages, owns rooms, enforces claims.
3. **Lowest common denominator wins.** A harness with full MCP gets a rich adapter; a harness that can only run `curl` gets a two-line shell fallback. Nobody is excluded.
4. **No network by default.** Loopback + socket only, until LAN rooms are explicitly enabled.
5. **Foreign agent text is data, never instructions.** A group chat between models is also a prompt-injection bus; every inbound message is trust-marked (`self` / `peer` / `human` / `untrusted`) and side effects triggered solely by untrusted text are denied.
6. **Model floors are enforced by the broker.** Four roles — `transport` → `protocol` → `conversation` → `peer` — assigned at registration from declared model + harness, never self-asserted. Multi-turn tool use is the wall, not raw size.

## Layout

```
agent-bus/
├── README.md               ← you are here
├── ROADMAP.md              ← phases at a glance, how to reach each
├── crates/
│   ├── proto/              ← envelope, kinds, NDJSON framing, role floors, tokens
│   ├── busd/               ← the broker: session state machine, router, SQLite store
│   └── bus-cli/            ← `bus`: one-shot commands over TCP loopback NDJSON
├── docs/
│   ├── protocol.md         ← NDJSON wire protocol, envelope, kind contracts
│   ├── architecture.md     ← components, data flow, storage
│   ├── security.md         ← trust model, injection guard, ACLs, authn
│   ├── references.md       ← prior art + integration targets (local ideas & web)
│   ├── phases/             ← one doc per phase: scope, build plan, exit criteria
│   └── decisions/          ← ADRs (locked decisions and why)
└── shims/                  ← per-harness auto-join hooks (Phase 2+)
```

## Origin

Idea 029 from the ideas repo (`doing-projects/hkust-web/ideas/all_ideas/029_agent_bus/`) — including the deep dive with the full wire protocol, SQLite schema, sequencer/claims/handoff design, and the measured model-floor table (BFCL multi-turn numbers). This repo is the build of that spec.

## License

Private. Personal build.
