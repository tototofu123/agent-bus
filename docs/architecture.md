# Architecture

## Components

```
┌─────────────────────────── harnesses (unchanged) ───────────────────────────┐
│ Claude Code │ Codex CLI │ Gemini CLI │ opencode │ aider │ Cursor │ omp │ CI │
└──────┬──────────┬────────────┬───────────┬─────────┬─────────┴────────┬─────┘
       │MCP(stdio)│MCP/HTTP    │CLI        │CLI      │CLI      │MCP     │HTTP
       ▼          ▼            ▼           ▼         ▼         ▼        ▼
┌──────────────────────────── agent-busd (localhost) ─────────────────────────┐
│  ┌───────────────┐  ┌───────────────┐  ┌───────────────┐  ┌──────────────┐  │
│  │ transport     │  │ room registry │  │ presence      │  │ auth         │  │
│  │ socket/ws/mcp │  │ topics, ACL   │  │ heartbeat, TTL│  │ token, scopes│  │
│  └───────┬───────┘  └───────┬───────┘  └───────┬───────┘  └──────┬───────┘  │
│          └──────────┬───────┴──────────┬───────┴─────────────────┘          │
│                     ▼                  ▼                                     │
│            ┌────────────────┐  ┌────────────────┐  ┌──────────────────────┐  │
│            │ router / pub-sub│ │ thread store   │  │ capability registry  │  │
│            │ fan-out, filters│ │ SQLite WAL     │  │ model, tools, limits │  │
│            └────────┬───────┘  └───────┬────────┘  └──────────┬───────────┘  │
│                     └──────────┬───────┴──────────────────────┘              │
│                                ▼                                             │
│            ┌──────────────────────────────────────────────┐                 │
│            │ hooks: file-lock registry · artifact store   │                 │
│            │        context compactor · policy engine     │                 │
│            └──────────────────────────────────────────────┘                 │
└───────────┬───────────────────────────────┬─────────────────────────────────┘
            │                               │
            ▼                               ▼
   ┌─────────────────┐            ┌──────────────────────────┐
   │ human: TUI/web  │            │ 023 worktree orchestrator│
   │ (join any room) │            │ file locks ↔ worktrees   │
   └─────────────────┘            └──────────────────────────┘
```

## Transports (build order)

| # | Transport | Phase | Purpose |
|---|---|---|---|
| 1 | Unix socket / Windows named pipe (+ TCP loopback fallback) | 1 | primary local transport, NDJSON lines |
| 2 | `bus` CLI over that transport | 1 | universal adapter — any harness that can run a shell |
| 3 | localhost WebSocket + REST (`ws://127.0.0.1:47114/ws`) | 2 | browsers, WASM, curl-able history |
| 4 | MCP server (`bus-mcp`, stdio + Streamable HTTP via `rmcp`) | 4 | native integration for MCP harnesses |
| 5 | mDNS + mTLS over LAN | 5 | cross-machine rooms |

## Storage

SQLite WAL, one file the user can delete. Full schema in the deep dive (ideas repo 029); summary:

- `rooms` — name, ACL, opened/closed, `seq` counter
- `messages` — append-only log; ULID id, server-assigned `(room, seq)`, thread/reply_to, sender, ts, lamport, kind, priority, trust, text, payload JSON, sig, expires
- `agents` — identity, harness, model, session, cwd, caps, joined, last_seen (TTL 15 s), token_hash
- `locks` — path_glob, holder, room, leased_at, expires, task
- `artifacts` — content-addressed (`sha256:…`), small inline / large on disk under `artifacts/`

## Sequencer (the only writer of `seq`)

Per-room mutex (std `Mutex`, never held across await) → `UPDATE rooms SET seq = seq + 1 RETURNING seq` + insert, one transaction. Then fan-out. This single choice makes ordering trivial, gap-free, and crash-safe.

## Fan-out with backpressure

Per-subscriber queues with drop policy by kind:

- `chat` / `status` → **DropOldest** — a slow tailing consumer is evicted, never stalls the room
- `task.*` / `policy` → **Block** — a grant/deny must never be silently missed; blocks briefly, evicts only a *gone* consumer

## Claims (leased, glob-matched, first-writer-wins)

- Expire stale leases first — a crashed agent is harmless
- Conflict = any existing lock whose glob intersects; glob intersection is computed by expanding both globs against the room's repo file list (cheap and exact for real repos)
- Renewal: re-send `task.claim` with same files before expiry → lease extends
- On expiry, the lock is reclaimed and the room gets a `status` note

Why leases and not OS file locks: OS locks are per-machine, per-process, invisible across harnesses, and cannot be inspected. A leased registry in the broker is visible (`bus who`), auditable (it is in the log), and works when one agent is a shell script.

## Handoff (bounded context transfer)

`compact(thread, budget_tokens)` builds a packet:

1. **Invariants first** (never summarized away): goal (first `task.offer`), decisions (`policy` + `answer`), artifact refs, held locks
2. **Narrative**, compressed from the middle outward — keep first 20% and last 60% verbatim (divergence + current state), middle as bullets
3. **Open questions** — unanswered `question`s — the thing a summary always drops and a successor always needs
4. **Next step** from last `status`

Target budget: ≤ 4k tokens. The successor also gets a `bus history --full` pointer to go deeper on demand (ship both the packet and the pointer).

## Policy engine (injection guard)

Every adapter marks inbound messages before they enter a model's context:

```
[self]   own session text        → Directive::Free
[human]  trust=human             → trusted
[untrusted]  everything else     → Directive::DataOnly
```

Tool calls whose **only proximate cause is untrusted text** are denied. Being a large model does not make a message an instruction.

## Crate layout (planned)

```
crates/
├── proto/          # envelope types, kind schemas, NDJSON framing, role/caps
├── busd/           # the daemon: socket.rs ws.rs router.rs claims.rs
│                   #   presence.rs handoff.rs policy.rs store.rs mcp.rs
├── bus-cli/        # `bus` — the universal adapter
└── mcp-face/       # Phase 4: rmcp server (stdio + Streamable HTTP)
shims/              # per-harness auto-join hooks (shell), pty-supervisor (029d)
```

## Integration points

- **023 worktree orchestrator** — claims map to worktrees, not branches; 029 owns the claims, 023 owns the isolation
- **014 local swarm / 022 Perihel Copilot** — join as first-class agents; capability registry enables cost-aware routing ("ask local first, escalate after two failures")
- **017 token optimization / 021 instruction automation** — the compaction inside handoff
- **028 Perihel Embedded** — the device node as a `transport`-role participant over loopback HTTP

See [references.md](references.md) for the full map.
