# References & Integration Targets

Two groups: **local** (ideas-repo siblings this project plugs into) and **web** (prior art, specs, and SDKs to reuse or interop with — verified 2026-10).

## Local integrations (ideas repo)

| Idea | Relationship | Phase |
|---|---|---|
| **022 — Perihel Copilot** (finished) | Local-agent harness that joins the bus as a first-class participant; capability registry enables cost-aware routing ("ask local first, escalate after two failures") | 2 |
| **023 — Worktree Orchestrator** (deep dive) | 029 owns the *claims*; 023 owns the *isolation* — claims map to worktrees, not branches | 3 |
| **014 — Local LLM Agent Swarm** | Produces local agents that need exactly this bus to talk to cloud agents; revisit its Qwen2.5-Coder-7B pick per the model-floor table (native-FC 3B beats prompt-based 4B on multi-turn) | 2–3 |
| **017 — Token Optimization** | The compaction math inside `handoff.rs` (caveman + headroom techniques) | 3 |
| **021 — Instruction Automation** | Decomposition used to keep handoff packets executable by small successors | 3 |
| **028 — Perihel Embedded** (deep dive) | Device node as `transport`-role agent: loopback HTTP on host, Wi-Fi escalation, GBNF-constrained emissions = the `protocol` role floor in practice | 5 |

Sub-ideas of 029 (backlog, not scheduled): 029a dashboard, 029b standalone lock lib, 029c compactor standalone, 029d PTY supervisor for hook-less harnesses, 029e room templates/teams, 029f triage agent (sub-1B, grammar-constrained `speak|stay_silent|claim`).

## Web references & prior art

| Project | What it is | How we use it |
|---|---|---|
| **MCP** — [modelcontextprotocol.io](https://modelcontextprotocol.io) | Tool-call RPC, 1:1, client-initiated; spec active (Streamable HTTP transport) | Phase 4 adapter only (ADR-0001); track [transport roadmap](https://blog.modelcontextprotocol.io) |
| **rmcp** — [docs.rs/rmcp](https://docs.rs/rmcp) · [rust-sdk](https://github.com/modelcontextprotocol/rust-sdk) | Official Rust MCP SDK; `StreamableHttpService` mounts on axum | The Phase 4 `mcp-face` crate; pin versions to absorb spec churn |
| **A2A (Agent2Agent)** — Linux Foundation project (donated by Google, June 2025) | Vendor-neutral agent↔agent task delegation over HTTP; enterprise/remote, task-shaped | Phase 5: mapping doc (`task.offer`/`task.handoff` ↔ A2A tasks); A2A rides *over* the bus, not under it |
| **ACP (Agent Client Protocol, Zed)** | Standardizes editor ↔ agent | Orthogonal: ACP connects agent↔tool; bus connects agent↔agent |
| **AG-UI** | Agent ↔ frontend event protocol | Candidate for the Phase 4 web surface instead of bespoke events |
| **agent-mesh** ([writeup](https://htek.dev/articles/agent-mesh-cross-session-communication-copilot-cli)) | Single-file Copilot CLI extension; SQLite message bus between sessions | Closest prior art; validates the need, stays single-harness — bus is multi-harness with claims/handoff |
| **mcp-agent-bus** ([repo](https://github.com/josesuarezcordova/mcp-agent-bus)) | Local event-driven MCP message bus for coding agents | Validates the space; MCP-first where we are broker-first (their Phase-4-shaped build lacks broker-owned ordering/replay) |
| **NATS / ZeroMQ / Redis pub-sub** | Transport patterns | Reference for fan-out/backpressure; our contribution is the agent-shaped envelope + adapters, not a new wire protocol |
| **IRC / Matrix** | Rooms, mentions, presence, threads — the mental model | Semantics reference; Matrix federation informs Phase 5 |
| **BFCL** ([leaderboard](https://gorilla.cs.berkeley.edu/leaderboard.html)) + TinyLLM paper ([arXiv:2511.22138](https://arxiv.org/html/2511.22138)) | Measured multi-turn tool-use floors for small models | The empirical basis of the four-role floor table (in ideas 029 readme) — do not retune floors without re-reading it |

## Take from research (2026-10)

1. The local multi-harness bus space is **early** — closest projects are single-harness (agent-mesh) or MCP-shaped (mcp-agent-bus). Nothing owns broker-side ordering + claims + handoff. Green field for the locked architecture.
2. **A2A under Linux Foundation governance** makes a mapping doc (not implementation) the right Phase 5 deliverable — interop story without spec-churn risk.
3. **rmcp + Streamable HTTP** is confirmed as the Phase 4 path; keep the face thin.
