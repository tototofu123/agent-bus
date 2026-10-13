# Phase 5 — LAN & Federation

**Goal:** one room spans machines — desktop agents, a phone/edge node (028), and CI — with the same ordering and trust guarantees.

**Why last:** federation multiplies every unsolved problem (identity, paths, trust). It is only worth it once the local system is boring.

## Scope

**In**
- mDNS discovery (`mdns-sd`) of `agent-busd` instances on the LAN
- rustls **mTLS** + per-room shared secret for cross-machine rooms; explicit opt-in (`--lan` flag)
- Path translation layer: rooms scoped to a repo assume shared layout; map `C:/…` ↔ `/home/…` via a per-room path table (open question #2 from the deep dive)
- CI as a one-shot participant: a single HTTP client announcing build results into `#repo:<name>`
- 028 Perihel Embedded device node joins as a `transport`-role agent over loopback HTTP on its host, escalating over Wi-Fi
- Optional live topology view (sub-idea 029a): who's in which room, what they hold — reuse the Phase 4 web surface
- A2A interop sketch: document how an A2A task delegation maps onto `task.offer`/`task.handoff` envelopes (A2A could ride over the bus; do not implement an A2A server in this phase)

**Out**
- Internet-wide federation, Matrix-style homeservers (reference only), cloud relay

## Prerequisites

- Phase 4 exit criteria green (signatures mandatory before anything leaves loopback)
- A concrete second machine / device need (028 node, CI runner, second desk)

## Build plan

1. `--lan` mode: bind non-loopback, advertise via mDNS, require mTLS client certs
2. Room federation semantics: a room owned by one broker; remote agents connect as clients to the owning broker (hub model first — no multi-master). Ordering stays single-writer per room by construction
3. Path table per room + translation in artifact paths and claim globs
4. CI one-shot client (`bus announce --one-shot`) with a scoped token
5. Topology endpoint (`/topology`) consumed by the web UI
6. A2A mapping doc (`docs/a2a-mapping.md`)

## The proof (demo)

Desktop (Windows) + laptop/CI (Linux) in `#repo:x`: desktop agent offers a task; CI announces a failing build; the other machine's agent claims and posts `task.result`; ordering and `bus history` identical on both sides. Stretch: 028 node posts `status` from the edge.

## How to reach it (exit criteria)

| Test | Pass criterion |
|---|---|
| Discovery | Two brokers find each other on LAN via mDNS < 5 s |
| Trust | Unsigned/unmTLS connections rejected; no plaintext fallback when `--lan` |
| Cross-machine ordering | Phase 1 ordering test re-run split across two machines: seq monotonic, zero gaps |
| Path translation | Claim globs and artifact paths resolve on both OS layouts |
| CI participant | CI announces into the room with a scoped token that can send but not claim |
| Latency | LAN round-trip `send`→`recv` p99 < 50 ms |

## Risks

- **Hub model limits** (owning broker is a SPOF for that room) — acceptable v1; multi-master is explicitly out until a real need exists
- **NAT/firewall realities** on "LAN" — discovery falls back to explicit `--peer host:port`
- **A2A scope creep** — this phase ships a mapping document, not an implementation
