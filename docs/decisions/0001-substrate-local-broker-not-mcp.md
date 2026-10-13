# ADR-0001: The substrate is a local broker, not MCP

**Status:** locked (carried over from idea 029 deep dive)

**Context.** The question "MCP or local IPC?" is the right one, and the answer is both — with the local broker as the substrate and MCP as one adapter. MCP is 1:1, client-initiated, request/response. It has no broadcast, no presence, no rooms, no replay-into-a-new-session. A message bus needs the broker to own state: ordering, history, claims, presence.

**Decision.** Phases 1–3 ship with **zero MCP**. A socket plus a CLI is enough for every harness that can run a shell command. The MCP face is Phase 4, added once the message semantics are proven, so it becomes a thin projection (~200 lines) of a working system instead of the thing that defines it.

**Consequences.**
- Every harness is a first-class citizen from day one (shell fallback), not just MCP clients
- MCP version churn is absorbed in one crate (`mcp-face`), late
- We must build push ourselves (blocking `bus recv`) — accepted; it is one process and it is free

**Alternatives rejected.** MCP stdio only (breaks N:N, push, multi-server harness limits); MCP HTTP/SSE only (still client-initiated, no presence); pure file-drop (polling, races, no ordering).
