# Phase 1 — Broker + CLI + Rooms (no MCP)

**Goal:** a boring, crash-safe local daemon and a CLI such that two shell scripts can hold a structured conversation in a room.

**Why first:** if the message semantics survive shell scripts, every richer harness will be easy. This phase also builds the sequencer and storage that every later phase stands on.

## Scope

**In**
- `crates/proto` — envelope types, kind payload schemas, NDJSON framing, role/caps assignment
- `crates/busd` — the daemon: socket transport (Unix socket / named pipe, TCP loopback fallback), room registry, single-writer sequencer, SQLite WAL store, append + fan-out, cursor replay
- `crates/bus-cli` — `bus register / join / leave / send / recv / tail / who / history`
- Blocking `bus recv` with `--timeout` — the push substitute for non-MCP harnesses
- Rate limiting (60 msg/s per agent), 1 MiB line cap
- Ephemeral TTL for `chat`/`status` (24 h) vs durable `task.*`/`artifact`/`policy`

**Out**
- Presence TTL / heartbeat (Phase 2), claims/locks (Phase 3), WS/REST (Phase 2), MCP (Phase 4), signatures (reserved field only)

## Prerequisites

- Rust toolchain (stable, tokio, rusqlite, clap, serde, ULID)
- The deep dive's storage schema (ideas repo 029) as the migration source

## Build plan

1. **`crates/proto`**: `Envelope`, `Agent`, `Kind` enum, payload schemas, NDJSON encode/decode with line cap, `Role::assign`, `allowed(role, kind)`. Unit tests for framing edge cases (empty line, >1 MiB, invalid UTF-8, version mismatch → `bye{proto}`).
2. **`crates/busd/src/store.rs`**: SQLite WAL migrations (`rooms`, `messages`, `agents`, `locks`, `artifacts` tables — schema in deep dive). Transaction helpers.
3. **`crates/busd/src/socket.rs`**: listener + per-connection task; handshake (`hello`/`welcome` with per-room cursor); read loop → dispatch.
4. **`crates/busd/src/router.rs`**: room registry; `append()` under per-room `Mutex` (never held across await): `UPDATE rooms SET seq = seq+1 RETURNING seq` + insert in one tx → assign seq → fan-out. Drop policy by kind (`drop_oldest` for chat/status, `block` for task.*/policy).
5. **`crates/busd/src/main.rs`**: config (socket path, DB path, limits), graceful shutdown, `--version`.
6. **`crates/bus-cli`**: subcommands mapping 1:1 to the protocol; `recv` blocks via long-poll on the socket; output formats `text`/`json`.
7. **Windows**: named pipe via tokio, TCP loopback `127.0.0.1:47113` as universal fallback (see ADR-0003 note).

## Implementation notes (deviations from the plan above)

- **Transport is TCP loopback by default, with a Windows named pipe served alongside.** TCP binds first because the bind is the lock that keeps a second broker off the same database; the pipe is an additional face on the same state, not a fallback (`bus --transport pipe` prefers it). Named pipes were added rather than dropped, so this scope item now stands met.
- **The listener lives in `session.rs`, not `socket.rs`.** It carries the per-connection protocol state machine (handshake → op loop → dispatch) alongside the accept loop, which is what made the file worth the name.
- **Ordering, latency, replay, backpressure, crash safety, the framing regression and the rate limit are verified at the full criteria.** Throughput is not: it measures ~20–31k msg/s against the 50k criterion depending on machine load, and the ceiling is the session layer's per-envelope work. The gate fails rather than being relaxed, so Phase 1 stays open.
- **`send_batch` was added to the protocol** so the broker could amortise one commit across many envelopes. The store's own ceiling with one commit per message was ~8k msg/s measured with no sockets, and batching several inserts into a *per-message* commit made things worse (a batch of one). The batch op took the gate from 7k to ~30k.
- **`read_line_capped` returned the wrong length for a frame larger than the BufReader.** It returned the newline offset *within the current `fill_buf` chunk* rather than the accumulated line length, so any line spanning several 64 KB chunks was truncated to the tail chunk the newline landed in. The symptom looked like mis-segmentation — a ~275 KB frame arriving as ~12–25 KB fragments, and pipelined frames reading as one over-cap line — but the accumulate-then-return-`buf.len()` fix is in, with `framing_large_line_reads_whole` covering ~275 KB sends and 256-message batches.
- **`op_send` runs its whole store interaction in one transaction** (`store::append_from_registered`): registration check, room ensure and append. It used to be four separate store calls, each committing — including an `INSERT OR IGNORE INTO rooms` on every message to a room that already existed. Folding them is what brought send p99 inside the 1 ms criterion.
- **Batching several inserts into a per-message commit was tried and removed.** It measured 6,139 msg/s against 8,759 for the direct path, because the send path commits each message as it is accepted, so every batch held exactly one message. The amortisation had to come from a multi-envelope frame, which is what `send_batch` is.
- **Latency is bimodal at the tail**: ~99% of sends land under 0.25 ms, ~1% land around 10 ms. `p99` therefore rides on a handful of samples and moves run to run, so the bench reports the histogram rather than the single figure.
- **`history` replies are paged in the suite.** A reply is a single NDJSON line and the protocol caps a line at 1 MiB, so a 50k-message room cannot come back in one reply.

## The proof (demo)

Two shell scripts, one room, zero harnesses:

```bash
./agent-a.sh   # registers codex@demo, joins #task:demo, sends task.offer, blocks on recv
./agent-b.sh   # registers claude@demo, joins #task:demo, recv → question → send answer
```

Then a third terminal: `bus history --room '#task:demo'` shows the whole ordered thread; kill and restart `agent-a.sh` and it resumes from its cursor.

## How to reach it (exit criteria)

| Test | Pass criterion |
|---|---|
| Ordering | 8 writers × 10k msgs into one room: seq strictly monotonic, zero gaps, all delivered |
| Latency | 1k round-trip `send`→`recv` on loopback: p99 < 1 ms |
| Throughput | 1 room, 1 KB payloads: ≥ 50k msg/s on one core |
| Crash safety | `kill -9` during append storm × 1000: DB consistent, no torn messages, no seq reuse |
| Replay | New joiner replays a 50k-message room in < 2 s, bounded memory |
| Backpressure | One subscriber stalled 60 s during 100k chat + 100 task msgs: all task msgs delivered, stalled subscriber evicted, room not stalled |
| Rate limit | Agent exceeding 60 msg/s is throttled with a typed error, not disconnected silently |

## Running the gates

```bash
cargo test --release --test phase1_gates -- --nocapture --test-threads=1
```

`--test-threads=1` is required for the numbers to mean anything: the throughput and latency gates measured together on this machine changed each other's results by more than the gates' own margins.

**The perf gates need a quiet machine.** On a box also running local inference the measured range was 14k–31k msg/s for the same binary, with load swinging 12–60% during a single run. Record the load alongside the number, and treat a wide spread as an unmeasured box rather than a regression:

```powershell
(Get-CimInstance Win32_Processor).LoadPercentage
```

Current standing: 7 of the 8 tests pass. Throughput is the one that does not — ~20–31k msg/s against the 50k criterion, and the ceiling is the session layer's per-envelope work (frame parse, envelope construction, fan-out's per-member clone), not the transport and not the rate limiter.

## Risks

- **Windows named pipes in tokio** are less trodden than Unix sockets — the TCP loopback fallback ships in the same phase, so this is a quality knob, not a blocker.
- **SQLite write contention** under the append storm — mitigated by single-writer-per-room mutex + WAL; benchmark gate will tell.
