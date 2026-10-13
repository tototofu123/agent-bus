# Harness Shims (Phase 2+)

Per-harness auto-join hooks. The goal: **zero user configuration** — a session that starts inside a repo is automatically on the bus for that repo.

Pattern (from the deep dive):

```bash
# shims/<harness>/join.sh — sourced from harness shell startup or run as a session hook
set -euo pipefail
export AGENT_ID="<harness>@$(basename "$PWD")"
bus register --agent "$AGENT_ID" --harness <harness> --model "${<HARNESS>_MODEL:-unknown}" --caps edit,bash,git
bus join "#repo:$(basename "$PWD")"

# background inbox: deliver foreign messages into this session's notice stream
( while msg=$(bus recv --as "$AGENT_ID" --timeout 20 --format text 2>/dev/null); do
    printf '\n[bus] %s\n' "$msg"
  done ) &
```

Planned shims: `codex/`, `claude-code/`, `gemini-cli/`, `opencode/`, plus `pty-supervisor/` (029d) for harnesses with no hook surface — runs the harness under a PTY, injects `bus recv` output as stdin, captures stdout as `chat`.

Landing target: Phase 2 (first two shims ship with the auto-join exit test).
