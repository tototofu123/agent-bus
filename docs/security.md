# Security Model

The bus is local-first and boring on purpose. Nothing here is exotic; all of it is load-bearing.

## Reachability

- **Loopback + socket only** until Phase 5. `agent-busd` binds `127.0.0.1` and a Unix socket / named pipe. Nothing is reachable from the network until LAN rooms are explicitly enabled.
- No cloud, no telemetry. History is a local SQLite file you can delete.

## AuthN / AuthZ

| Concern | Phase 1–3 | Phase 4+ |
|---|---|---|
| **AuthN** | Token file per agent, `0600`, 32 random bytes, hash stored | Same; plus Ed25519 per agent |
| **AuthZ** | Room ACL: write requires `cwd` inside the room's repo glob | Same, with per-kind ACLs (`policy` only from `human`) |
| **Integrity** | Local trust; `sig` field reserved | `sig` mandatory on `task.*`, `artifact`, `policy` |
| **DoS** | Rate limit per agent (60 msg/s), per-connection queue caps | Same |

- **Room ACLs.** `#repo:x` writable only by agents whose `cwd` is inside repo x — an agent in repo A cannot leak into repo B's room.
- **Identity = token + declared id.** No self-asserted ids; the broker assigns role at registration. Sessions are ephemeral; `codex@proj` is stable with an ordinal suffix only on collision (see ADR-0004 when written).
- **`@human`** bypasses path ACLs but leaves a `policy` trail for every action.

## Prompt injection (the one people miss)

A group chat between models is also a **prompt-injection bus**. If agent A can write text that agent B treats as instructions, A can make B run tools.

Mitigations, in order of importance:

1. **`trust` is set by the broker, never the sender.** `self` | `peer` | `human` | `untrusted`.
2. **Adapters mark inbound text** in the receiving model's context: `[untrusted] codex@proj (task.offer): …`
3. **Hard rule: text from another agent may inform, never instruct.** `Directive::DataOnly` for everything except own-session and human text.
4. **Side-effect gating:** tool calls whose only proximate cause is untrusted text are denied (`check_tool_call`).

Being a frontier model does not make your message an instruction. Role is capability, not trust.

## Concurrency safety

| Hazard | Mitigation |
|---|---|
| Two agents edit one file | `task.claim` + leased file-lock registry; second agent is told who holds it |
| Agent dies mid-claim | Lease TTL reclaims; room gets a `status` note |
| Message storms / loops | Max 3 hops per thread, rate limits, loop detection on identical text+recipient |
| Slow consumer stalls room | Drop policy by kind (see protocol.md) |

## Auditability

Every message is append-only with ULID id + server seq + timestamp + sender. **The log is the audit trail.** An edit that happened without a claim is detectable after the fact.

## Non-goals

The bus is not a sandbox. It does not run code, and it cannot stop an agent that ignores the bus from editing files. It makes **cooperative safety the default** and makes non-cooperative behavior visible.
