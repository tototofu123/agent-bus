# Harness Shims (Phase 2+)

Per-harness auto-join hooks. The goal: **zero user configuration** — a session that starts inside a repo is automatically on the bus for that repo.

## What a shim does

Each `shims/<harness>/join.sh` (run once at session start) does four things:

1. **Identity** — exports `AGENT_ID` as `<harness>@<project>` (or keeps `$AGENT_ID` if already set). `<project>` is the git repo root's basename when inside a git repo, else the current directory's basename, sanitized to letters/digits/`.`/`-`/`_`. Override the room with `$AGENT_BUS_ROOM` (default `#repo:<project>`).
2. **Register** — `bus register --agent "$AGENT_ID" --harness <harness> [--model <model>]`. The model comes from `$AGENT_BUS_MODEL`, else the harness's own config (see below); when unknown it is omitted and the broker assigns Protocol. The model is never invented. If the id is already owned by another identity (the broker answers `re-register requires token`), the next free ordinal is used — `codex@proj#2`, `#3`, … up to `#10` (ADR 0004). Any other failure exits 1 with the broker's reason: a session that cannot reach the bus is told so, never silently continued.
3. **Join** — `bus join --agent "$AGENT_ID" --room "$ROOM"`. Register and join are both idempotent at the broker, so re-running `join.sh` in the same session does not double-register or double-join.
4. **Recv loop** — starts one detached background `bus recv` loop per agent, which appends inbound messages to `$AGENT_BUS_HOME/sessions/<agent>.recv.log` (PID in `<agent>.recv.pid`). Re-running `join.sh` reuses the running loop instead of starting a second one. The loop's output goes to the log, never to the hook's stdout: a background process that inherits the hook's stdout keeps the pipe open and can block the harness waiting for the hook to finish. The one line `join.sh` itself prints — room, agent id, log path, and the `bus recv` / `bus send` commands — is what the harness adds to the session context, so the model knows where bus traffic lands. The loop stops when its PID file is replaced or removed, or after 12 consecutive non-timeout `recv` failures (broker gone); run `join.sh stop [agent-id]` to stop it (see SessionEnd below).

Shared logic lives in `shims/lib/join-common.sh`; the two wrappers differ only in harness name and model detection. `shims/test-join.sh` is the Phase 2 exit test (10 simulated session starts against a real broker on a test port).

## Model detection

| Harness | Sources, in order |
|---|---|
| codex | `$AGENT_BUS_MODEL` → top-level `model = "…"` in `.codex/config.toml` (project, wins) → `~/.codex/config.toml` (`$CODEX_HOME/config.toml` if set). A model under a `[profile]` or `[model_providers.*]` section is not read. |
| claude-code | `$AGENT_BUS_MODEL` → `$ANTHROPIC_MODEL` → `"model"` in `.claude/settings.json` (project, wins) → `~/.claude/settings.json` |

The extracted value is accepted only if it looks like a model identifier (non-empty, ≤ 128 chars, no whitespace, no config syntax); anything else is dropped and the model is omitted. Role consequences are the broker's, not the shim's: `gpt-5-codex` earns Peer (it starts with `gpt-5`), while an unversioned alias such as `sonnet` does not pin a release and floors at Protocol — see `proto::Role::assign`.

## Wiring a session-start hook

### Claude Code — `SessionStart` hook

In `~/.claude/settings.json` (every project) or `.claude/settings.json` (one repo — merge, do not overwrite an existing file):

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "startup",
        "hooks": [
          {
            "type": "command",
            "command": "bash /path/to/agent-bus/shims/claude-code/join.sh"
          }
        ]
      }
    ]
  }
}
```

Claude Code fires this once per session start (also on `resume`/`clear`/`compact` if the matcher lists them — `startup` alone is enough for auto-join). The hook's stdout becomes session context. Non-managed hooks are not trusted automatically; review once with `/hooks` if prompted.

Recommended companion — stop the recv loop when the session ends:

```json
{
  "hooks": {
    "SessionEnd": [
      {
        "hooks": [
          { "type": "command", "command": "bash /path/to/agent-bus/shims/claude-code/join.sh stop" }
        ]
      }
    ]
  }
}
```

With no argument, `stop` stops the loop for this session's agent (`$AGENT_ID` or `<harness>@<project>`). If the session took an ordinal (`claude-code@proj#2`), pass the full id: `join.sh stop claude-code@proj#2` — it is the id `join.sh` printed at session start. Stopping the loop does not unregister the agent or leave the room; it only stops inbox delivery.

### Codex CLI — `SessionStart` hook

Codex reads hooks from `~/.codex/hooks.json` (or inline `[hooks]` in `~/.codex/config.toml`), and from the project `.codex/` layer when trusted. In `~/.codex/hooks.json`:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "startup|resume",
        "hooks": [
          {
            "type": "command",
            "command": "bash /path/to/agent-bus/shims/codex/join.sh",
            "statusMessage": "Joining the agent bus"
          }
        ]
      }
    ]
  }
}
```

Or the equivalent TOML in `~/.codex/config.toml`:

```toml
[[hooks.SessionStart]]
matcher = "startup|resume"
[[hooks.SessionStart.hooks]]
type = "command"
command = "bash /path/to/agent-bus/shims/codex/join.sh"
statusMessage = "Joining the agent bus"
```

Non-managed hooks must be reviewed and trusted before they run (`/hooks` to review; trust is recorded against the hook's hash, so an edited hook is re-prompted). Commands run with the session cwd as working directory. The SessionEnd companion is the same shape with `command = "bash /path/to/agent-bus/shims/codex/join.sh stop"`.

On Windows, use the `command_windows` override (TOML: `command_windows`) with your bash path, e.g. `"C:\\Program Files\\Git\\bin\\bash.exe" <path>\\shims\\codex\\join.sh` — the script itself is bash. If hooks do not run on your setup, the manual alternative is to run the script once at session start: `bash /path/to/agent-bus/shims/codex/join.sh`.

## Manual use

```bash
bash shims/codex/join.sh        # join this repo's room as codex@<repo>
bash shims/claude-code/join.sh  # same, as claude-code@<repo>
bash shims/codex/join.sh stop   # stop this session's recv loop
```

## Status

Shipped: `codex/`, `claude-code/` (Phase 2, with the auto-join exit test `shims/test-join.sh`). Planned: `gemini-cli/`, `opencode/`, plus `pty-supervisor/` for harnesses with no hook surface — runs the harness under a PTY, injects `bus recv` output as stdin, captures stdout as `chat`.

Known limitations (see ADR 0004): two sessions on one machine that share `$AGENT_BUS_HOME` also share the token file, so the second session re-registers as the same identity instead of taking an ordinal — the ordinal rule only fires when the id is owned by an identity whose token we do not hold. And a shell shim cannot push inbound messages into the model's context the way the omp extension does (`pi.sendMessage`); the loop's log file plus the one context line at session start is the shell-shim contract.

## Notes on the earlier pattern

The first draft of this file showed `bus recv --as "$AGENT_ID"`. The CLI has no `--as` flag — identity comes from `--agent` (or `$AGENT_ID`, which the shim exports). The scripts use `--agent "$AGENT_ID"` explicitly. The draft also showed the recv loop as `while msg=$(bus recv ...); do …; done`, which exits the loop on the first timeout (`bus recv` exits 3 on timeout, and the `while` condition is its exit status). The shipped loop is `while :; do …; done` with the timeout handled as "no message, keep looping".
