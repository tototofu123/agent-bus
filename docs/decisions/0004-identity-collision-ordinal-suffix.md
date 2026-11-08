# ADR-0004: Identity collision resolves with an ordinal suffix

**Status:** accepted (Phase 2; decided in the Phase 2 doc, formalized here)

**Context.** Two sessions of the same harness in one repo both want the id
`<harness>@<project>` (e.g. `codex@demo`). One agent id means one inbox:
the router delivers to the id, not to the session, so two sessions sharing
an id share one inbox and a message is consumed by whichever session's
`recv` happens to be blocking. The sessions also share one token file
(`$AGENT_BUS_HOME/tokens/<id>.token`), so the second session re-registers
as the same identity without the broker ever seeing a conflict.

**Decision.** When a session finds its base id already owned by an identity
whose token it does not hold, it takes the next free ordinal suffix:
`codex@demo#2`, `#3`, … up to `#10`, then fails loudly. The signal is the
broker's own: re-registering an existing id without its token is rejected
with `[auth] re-register requires token`, and re-registering with a
non-matching token is rejected with `[auth] token mismatch`. The shim
treats only the first as a collision (try the next ordinal); any other
register failure is fatal. The base id itself is never suffixed when it
is free, and re-running `join.sh` in the same session reuses the same id
(register is idempotent when the token file is present).

**Consequences.**
- Every session gets its own inbox, so a `task.offer` is delivered to
  every session in the room, not consumed by the first one awake.
- The ordinal is stable for the session's lifetime: it is the id the
  session prints at join time, and `join.sh stop <id>` takes it back.
- The rule is broker-enforced, not shim-enforced: the broker is the only
  authority on who owns an id, so two sessions racing for the same
  ordinal resolve by whoever registers first; the loser moves on.

**Known limitation.** The ordinal only fires when the colliding session
does not hold the id's token. Two sessions on one machine that share
`$AGENT_BUS_HOME` share the token file, so the second session
re-registers as the same identity and no ordinal is taken. That is the
common single-machine case, and it is currently unhandled: the two
sessions share one inbox. Fixing it needs a per-session slot the shim
can claim without the broker — e.g. a PID/state file keyed by the
harness's session id (Claude Code's `SessionStart` hook receives
`session_id` on stdin; Codex's hook event carries its own session
identifier) — which is future work, not this issue. Until then, sessions
that must be distinct on one machine set distinct `$AGENT_ID`s.

**Alternatives rejected.** PID suffix (`codex@demo#12345`): unique but
unstable and unreadable, and it breaks the "same session re-joins as the
same id" property. Random suffix: same problem, plus ungreppable. One
id per (harness, project) with no collision handling: the failure mode
the shim exists to fix — a session silently sharing another session's
inbox.
