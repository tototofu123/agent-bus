# ADR-0002: NDJSON line framing over the wire

**Status:** locked

**Context.** The protocol must be debuggable from a terminal (`nc`, `tail`), implementable from any language with zero dependencies, and carried identically over socket, WS, REST, and MCP.

**Decision.** NDJSON — `\n`-terminated UTF-8 JSON, one object per line, max 1 MiB per line. Artifacts are by reference (hash/path), so nothing large ever crosses the wire.

**Consequences.**
- `nc`/`curl` are valid clients; the two-line shell fallback exists
- Human-inspectable logs; no schema tooling needed to debug
- ~a few bytes of overhead per message vs binary framing — irrelevant at the target rates (≥ 50k msg/s on one core, p99 < 1 ms)

**Alternatives rejected.** Binary framing (bytes win, debuggability loses, messages are small); msgpack/CBOR (dependency for zero gain at this size); protobuf (schema rigidity across harness shims).
