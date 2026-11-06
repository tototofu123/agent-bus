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

The Phase 1 core is built and tested, and the exit gates have now been run at their stated criteria. Seven pass: ordering (8 writers × 10k, gap-free), latency (send→recv p99 < 1 ms), replay (50k < 2 s), backpressure (100k chat + 100 task), crash safety (10k-message storm), the batch crash-loss bound, and the rate limit. Throughput fails: ~30k msg/s against the 50k criterion, up 4.4× from ~7k once `send_batch` let the broker amortise one commit across many envelopes. The remaining gap is per-envelope work in the session layer, not the transport or the limiter.

Two measurement notes that mattered more than the numbers: the send path originally did its registration check, role lookup and room ensure as four separate store calls, each with its own commit — folding them into one transaction is what brought send p99 inside the criterion. And latency distributes as ~99% under 0.25 ms with a ~1% band around 10 ms, so p99 sits on a handful of samples and moves run to run; the bench prints a histogram because a single p99 figure hides that.

| Phase | Delivers | Status |
|---|---|---|
| [1 — Broker + CLI + rooms](docs/phases/phase-1-broker-cli-rooms.md) | `agent-busd`, SQLite, TCP + named pipe, `bus send/recv/tail/who/history` | in progress — 6 of 7 exit gates pass at criteria; throughput fails at ~8.7k msg/s vs 50k |
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

## Running it day to day (Windows)

One global broker, one fixed port, persistent state. Install once:

```powershell
powershell -ExecutionPolicy Bypass -File tools\launcher\install.ps1
```

That puts `busd.exe` and `bus.exe` in `%LOCALAPPDATA%\agent-bus\bin`, drops a control panel there, and creates **`Agent Bus`** on your Desktop. Then:

| You do | What happens |
|---|---|
| Double-click `Agent Bus` | The broker starts and the panel shows `RUNNING` |
| Close the panel window | **The broker keeps running** — the window is only a dashboard |
| Open it again and press **Stop server** | The broker stops |
| Reboot the computer | Nothing starts. There is no autostart entry |

The state lives in `%USERPROFILE%\.agent-bus` (`history.sqlite`, `busd.pid`, `tokens\`), so rooms, history and identities survive closing the shell, changing folders, and rebooting.

Two properties are enforced rather than assumed:

- **One broker per machine.** The port is the lock. Starting a second one is a no-op with a message, not a crash — two brokers over one SQLite file would fight over `seq`.
- **Stop only ever kills its own process.** A recorded pid is not a claim — pids recycle. `busd --status`, the double-start no-op message, and the panel's Stop button all resolve the pid through the same check (the pid exists **and** its image name is `busd`) before acting on it, so a recycled pid can't be hit. A pidfile naming a dead, recycled, or unparseable pid reads `pid unknown`, never the broker.
- **Two transports, one broker.** On Windows the broker serves both TCP loopback **and** the named pipe `\\.\pipe\agent-busd`. TCP stays the default and binds first — the bind is the lock that keeps a second broker from writing the same database — and the pipe is an additional face, not a fallback. `bus --transport pipe` prefers the pipe and falls back to TCP; if neither answers, the error names both attempts.

`install.ps1` also sweeps any autostart entry that points at `busd` — Run key, Startup folder, or scheduled task — so the no-autostart rule survives a previous install.

## Telegram bridge

`bus telegram` puts one room in your pocket: bus traffic is pushed to a
Telegram chat, and anything you type there enters the room as `@human` with
`trust=human`.

```powershell
# one-time: the token lives outside the repo
New-Item -ItemType Directory -Force $env:USERPROFILE\.agent-bus | Out-Null
Set-Content -NoNewline $env:USERPROFILE\.agent-bus\telegram.token '123456:ABC...'

# with the broker running
bus telegram --room '#repo:agent-bus'
```

The first chat that messages the bot claims it; the binding is stored once in
`~/.agent-bus/telegram.json`, so a stranger cannot take over the feed. Plain
text sends `chat`; `/claim`, `/status`, `/who`, `/rooms`, `/history`, `/room`,
`/id`, and `/help` cover the rest. Sends are acknowledged with their server
`seq` and how many other agents are online.

The bridge is transport, not a model, so a receipt is not a reply. `bus listen`
is the other half: it tails the room, runs a command per message, and posts
whatever that command prints back.

```powershell
bus listen --room '#repo:agent-bus' --exec 'my-responder'
```

`tools/omp-responder.ps1` is the tool-calling variant: it runs a local `omp`
session with read/glob/grep/web_search, routed to a folder per room through
`~/.agent-bus/workspaces.json`, so "weather in HK" actually fetches HKO instead
of guessing. Local only — close the listener and the agent is gone.

See [docs/telegram.md](docs/telegram.md) for the command table, stored files,
delivery semantics, and the current claim-scope note.

## The six primitives

Everything is built from these:

| Primitive | API | Guarantees |
|---|---|---|
| **identity** | `bus register` | stable id (`<harness>@<repo>`), token, declared capabilities |
| **room** | `bus join/leave` | ordered, durable, append-only log scoped by ACL |
| **message** | `bus send/recv/tail/history` | typed envelope, monotonic per-room seq, cursor replay |
| **presence** | `bus who` | live connection presence + last-seen; who is here, on what model, holding what |
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
│   └── bus-cli/            ← `bus`: one-shot commands over NDJSON (TCP + named pipe)
├── tools/launcher/         ← Windows desktop control panel + installer
├── docs/
│   ├── protocol.md         ← NDJSON wire protocol, envelope, kind contracts
│   ├── architecture.md     ← components, data flow, storage
│   ├── security.md         ← trust model, injection guard, ACLs, authn
│   ├── telegram.md         ← the two-way Telegram human surface
│   ├── references.md       ← prior art + integration targets (local ideas & web)
│   ├── phases/             ← one doc per phase: scope, build plan, exit criteria
│   └── decisions/          ← ADRs (locked decisions and why)
└── shims/                  ← per-harness auto-join hooks (Phase 2+)
```

## Origin

Idea 029 from the ideas repo (`doing-projects/hkust-web/ideas/all_ideas/029_agent_bus/`) — including the deep dive with the full wire protocol, SQLite schema, sequencer/claims/handoff design, and the measured model-floor table (BFCL multi-turn numbers). This repo is the build of that spec.

## License

Private. Personal build.
