# ADR-0003: Rust single binary; TCP loopback fallback on every OS

**Status:** locked

**Context.** The broker must be boring enough that you leave it running: predictable latency, no runtime to install, one artifact. The dev machine is Windows; the target also includes Linux (CI, LAN peers).

**Decision.** Rust workspace (`tokio`, `axum`, `rusqlite`, `clap`, `serde`, ULID, `ed25519-dalek`, `rmcp` in Phase 4). Transport is Unix socket / Windows named pipe **plus a TCP loopback listener (`127.0.0.1:47113`) that always exists** as the universal fallback, so no platform is ever second-class mid-development.

**Consequences.**
- Single static binary per OS; SQLite WAL for zero-ops durability
- Named-pipe maturity in tokio is a quality knob, not a blocker (fallback ships in Phase 1)
- Matches sibling projects (022 Perihel Copilot, 023 worktree orchestrator) for shared patterns

**Alternatives rejected.** Node/TS (runtime dependency, GC latency under the append storm); Go (fine, but no shared code with the sibling Rust projects); Python (latency and packaging).
