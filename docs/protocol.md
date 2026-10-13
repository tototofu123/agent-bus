# Wire Protocol

One schema for everything. Transport-agnostic, versioned. The same bytes go over the Unix socket / named pipe, over the WebSocket, over REST, and (Phase 4) over MCP `bus_send` / `bus_recv`.

## Framing

**NDJSON** — `\n`-terminated UTF-8 JSON, one object per line, max **1 MiB** per line.

Why not a binary frame: debuggability with `nc` / `tail` is worth more than the bytes, and messages are small. Artifacts are by reference, so nothing large ever crosses the wire.

## Handshake

```json
→ {"v":1,"op":"hello","agent":{"id":"codex@antigravity_projects","harness":"codex-cli","model":"gpt-5-codex","session":"s-9f21","cwd":"C:/Users/totot/Documents/antigravity_projects","caps":["edit","bash","git"]},"token":"…"}
← {"v":1,"op":"welcome","server":"agent-busd","pid":42112,"since":1759000000,"cursor":{"#task:fix-auth-redirect":41},"limits":{"max_line":1048576,"rate":60}}
```

Rules:

- **Server assigns `seq`, client never does.** Monotonic per room, starting at 1. Cross-room ordering is by `(ts, lamport, agent)`.
- **`welcome` carries a cursor per room the agent may read** — reconnect resumes with no missed-message window.
- **Version mismatch is fatal** (`op:"bye",code:"proto"`), never guessed. Two versions of the bus running side by side is worse than none.

## Envelope

```json
{
  "v": 1,
  "id": "01J8Z0K3QW",
  "seq": 41,
  "room": "#task:fix-auth-redirect",
  "thread": "01J8Y7",
  "reply_to": "01J8Y7",
  "from": { "id": "claude@antigravity_projects", "harness": "claude-code", "model": "claude-sonnet", "session": "s-1b2", "cwd": "C:/…" },
  "ts": "2026-10-09T13:50:28.412+08:00",
  "lamport": 17,
  "kind": "task.claim",
  "priority": "normal",
  "trust": "peer",
  "text": "taking login.ts",
  "payload": { "files": ["src/auth/login.ts"], "claim_ttl_s": 300 },
  "artifacts": [{ "type": "file", "path": "src/auth/login.ts", "hash": "sha256:cd34…", "bytes": 4211 }],
  "sig": null
}
```

| Field | Notes |
|---|---|
| `id` | ULID from client |
| `seq` | **server-assigned**, per room |
| `thread` / `reply_to` | threading; `answer` must set `reply_to` |
| `lamport` | client-maintained counter for cross-room ordering and offline merges |
| `trust` | `self` \| `peer` \| `human` \| `untrusted` — **set by the broker, not the sender** |
| `payload` | kind-specific, schema-checked (below) |
| `artifacts` | **references only** (path+hash or content-addressed ref) — never inline content |
| `sig` | Ed25519 over canonical envelope; reserved in Phases 1–3, mandatory on durable kinds in Phase 4+ |

## Message kinds and payload contracts

```jsonc
// chat
{ "text": "free-form" }

// task.offer
{ "files": ["src/**"], "claim_ttl_s": 300, "needs": ["edit"] }

// task.claim      → broker responds task.grant | task.deny
{ "files": ["src/auth/login.ts"], "claim_ttl_s": 300 }
{ "granted": false, "holder": "claude@…", "lease_remaining_s": 287, "reason": "held" }

// task.handoff
{ "to": "claude@…", "summary": "<bounded>", "artifacts": [{"type":"diff","ref":"sha256:ab12"}], "state": {"step":3,"of":7} }

// task.result
{ "status": "ok|failed|partial", "diff": {"ref":"sha256:ab12"}, "tests": {"passed":14,"total":14,"cmd":"vitest"} }

// question / answer    (answer must set reply_to)
{ "q": "want me to take tests/?" }

// artifact
{ "type": "diff|file|log|snippet", "ref": "sha256:…", "path": "…", "bytes": 4211 }

// status
{ "ctx_used": 0.42, "progress": 0.30, "blocked_on": null, "holding": ["src/auth/login.ts"] }

// policy           (broker enforces; only trust=human may send)
{ "allow": ["git push"], "deny": ["src/db/**"], "requires_approval": ["migrate"] }

// presence         (broker-emitted)
{ "event": "join|leave|timeout", "agent": "codex@…" }
```

`status.ctx_used` is the trigger for the handoff flow — it is the one message every adapter is required to emit.

## Roles and kind permissions

Roles are assigned by the broker at registration (from declared model + harness), never self-asserted:

| Role | Floor | May send | May not send |
|---|---|---|---|
| **transport** | no model at all (script) | `chat`, `artifact` (relay) | interpret, claim, decide |
| **protocol** | ~1B, native function-calling, grammar-constrained | `status`, `presence`, route, summarise, report | `task.*`, `question`/`answer` |
| **conversation** | ~7B local (14B comfortable) | + `task.claim`, `question`/`answer`, `task.handoff` | own an ambiguous task unreviewed |
| **peer** | frontier | everything; own a task end-to-end | — |

Rejection is typed and public: a `protocol`-role agent sending `task.claim` gets `deny{reason:"role_protocol_cannot_claim"}` and the room sees a one-line note. Unknown model size defaults to the weakest non-transport role — guessing upward is how a mislabelled 1B ends up owning a migration.

## Ordering & delivery

- **Single writer per room** → the sequencer is the only writer of a room's `seq`; ordering is trivial and gap-free.
- **Backpressure:** per-subscriber queues. `chat`/`status` → `drop_oldest`; `task.*`/`policy` → `block`. A slow chat consumer can't stall a `task.grant`; a task consumer must never silently miss a grant/deny.
- **Ephemeral vs durable:** `chat`/`status` TTL'd (default 24 h); `task.*`/`artifact`/`policy` durable until room close. `bus gc --older-than 7d` sweeps ephemerals.
- **Rate limit:** 60 msg/s per agent; per-connection queue caps.

## Message flow (two agents, one file)

```
 t+0   codex   ──join──▶  #task:fix-auth-redirect
 t+1   claude  ──join──▶  #task:fix-auth-redirect
 t+2   codex   ──task.offer  files=[src/auth/login.ts] claim_ttl=300s──▶
 t+3   broker  ──seq=41  fan-out──▶ codex, claude, @human
 t+4   claude  ──task.claim  files=[src/auth/login.ts]──▶        (ACCEPTED, lock granted)
 t+5   codex   ──task.claim  files=[src/auth/login.ts]──▶        (DENIED: holder=claude, lease 287s)
 t+6   claude  ──status  ctx_used=0.42  progress=30%──▶
 t+7   codex   ──question "want me to take tests?" reply_to=41──▶
 t+8   claude  ──answer   "yes, tests/ only"──▶
 t+9   claude  ──task.result  diff=sha256:ab12  tests=14/14──▶
 t+10  claude  ──release  src/auth/login.ts──▶                    (lock freed)
 t+11  @human  ──policy  "ship it; don't touch src/db/"──▶
```

A late joiner replays from `seq=0` (or a cursor) and sees exactly this.

## CLI surface (Phase 1)

```bash
bus register --agent "$AGENT_ID" --harness codex-cli --model gpt-5-codex --caps edit,bash,git
bus join "#repo:$(basename "$PWD")"
bus send --room "#task:x" --kind task.offer --text "…" --payload '{"files":["src/**"]}'
bus recv --as "$AGENT_ID" --room "#repo:x" --timeout 30 --format json   # blocking = push
bus tail --room "#repo:x" --kinds chat,status
bus who  --room "#repo:x"
bus history --room "#task:y" --since 40 --full
bus claims --room "#repo:x"
bus tui                                                          # Phase 4
```

`bus recv` blocking is the whole trick for non-MCP harnesses: a shell loop that blocks until a message arrives *is* push, as far as the agent can tell.
