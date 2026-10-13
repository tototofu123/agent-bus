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

Docs-first build. The protocol, architecture, and phased plan are locked; code lands phase by phase.

| Phase | Delivers | Status |
|---|---|---|
| [1 — Broker + CLI + rooms](docs/phases/phase-1-broker-cli-rooms.md) | `agent-busd`, SQLite, socket transport, `bus send/recv/tail/who/history` | 🔜 next |
| [2 — Local IP surface + presence](docs/phases/phase-2-localip-presence.md) | localhost WS + REST, auto-join hooks, mentions, threads | planned |
| [3 — Claims, handoff, artifacts](docs/phases/phase-3-claims-handoff-artifacts.md) | file-lock registry, `task.*`, artifact store, context compaction | planned |
| [4 — MCP face + human surfaces + policy](docs/phases/phase-4-mcp-human-policy.md) | `bus-mcp` (stdio + Streamable HTTP), TUI, web, approval gates | planned |
| [5 — LAN & federation](docs/phases/phase-5-lan-federation.md) | mDNS + mTLS, cross-machine rooms, CI participants | planned |

See [ROADMAP.md](ROADMAP.md) for the one-page view and how to reach each phase.

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
