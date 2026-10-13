# Phase 3 — Claims, Handoff, Artifacts

**Goal:** the phase that pays for itself — no more clobbered edits, no more tasks dying at 80% context.

**Why now:** Phases 1–2 proved transport, ordering, and presence. Claims need all three (leases expire via presence; grant/deny must be reliably delivered); handoff needs the durable thread store.

## Scope

**In**
- `claims.rs`: leased, path-glob file-lock registry; expire-stale-first; conflict = glob intersection (expand both globs against the room's repo file list); renewal by re-claim; expiry posts a `status` note
- Kind wiring: `task.offer` → `task.claim` → broker `task.grant`/`task.deny` (deny carries holder + lease remaining) → `task.result` → `release`
- Role floor enforcement on `task.*` (only `conversation`/`peer` may claim; typed public deny otherwise)
- `artifacts`: content-addressed store (`sha256:…`), small inline / large on disk under `artifacts/`, `bus gc` sweep, size caps with explicit override
- `handoff.rs`: `compact(thread, budget_tokens)` — invariants first (goal, decisions/policies/answers, artifact refs, held locks), narrative compressed middle-out (keep first 20% + last 60% verbatim), open unanswered questions, next step from last `status`
- `status.ctx_used > 0.80` triggers a broker-offered `task.handoff`
- Handoff packet ≤ 4k tokens + `bus history --full` pointer (ship both: lossy packet AND the depth-on-demand pointer)

**Out**
- Human approval gates (Phase 4), signatures on durable kinds (Phase 4), path translation across machines (Phase 5)

## Prerequisites

- Phase 2 exit criteria green (presence TTL is the claim-expiry clock)
- Ideas 017 (token optimization) and 021 (instruction decomposition) as compaction references

## Build plan

1. `claims.rs` with the tx-based claim/deny/renew/expire from the deep dive; glob expansion against the room `cwd` file list
2. Route `task.claim` through the sequencer so grants/denies are in the durable log (auditable)
3. Artifact store + hash verification on write/read; `bus artifact put/get/ls`
4. `handoff.rs` compaction with token budget estimator; `bus handoff --to --thread --budget-tokens`
5. ctx-watch: broker watches `status` and offers handoff; receiving agent gets packet + system-prompt fragment (goal, decisions, artifacts, open questions, next step)
6. Chaos test harness (below) as a first-class cargo test/bench target

## The proof (demo)

1. **Race:** two agents send `task.claim` for `src/auth/login.ts` 50 ms apart. First gets `task.grant`; second gets `task.deny{holder, lease_remaining_s}` and pivots to tests (the t+4…t+8 flow from protocol.md, end to end).
2. **Handoff:** an agent reports `ctx_used: 0.85` mid-task; the bus offers handoff; a fresh session receives the ≤4k packet and continues without re-reading the whole thread.

## How to reach it (exit criteria)

| Test | Pass criterion |
|---|---|
| Lock correctness (chaos) | N agents claim overlapping globs with random kills: never two holders on an intersecting path; expired leases always reclaimed |
| Lease TTL | Kill an agent mid-claim: lock freed within TTL + sweep slack; room sees the `status` note |
| Role floor | Register a 0.6B-class agent, send `task.claim`: typed `deny{reason:"role_protocol_cannot_claim"}`, note visible in room, no lock taken |
| Handoff budget | 40 real ~20k-token threads: packet ≤ 4k tokens; goal + open questions **always** present |
| Handoff usefulness | Successor completes the task without loading full history in ≥ 80% of trials (manual judged set) |
| Artifact integrity | Ref → content hash verified on read; tampered file detected |
| GC | `bus gc --older-than 7d` removes only ephemerals + unreferenced artifacts |

## Risks

- **Glob expansion cost** on huge repos — cache the file list per room, invalidate on `task.result`/git events; fall back to literal-prefix intersection if list unavailable.
- **Compaction losing the wrong things** — the budget_fit split (invariants + head/tail verbatim) is the guard; the exit test makes "goal + open questions always present" non-negotiable.
