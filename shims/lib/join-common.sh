#!/usr/bin/env bash
# join-common.sh — shared auto-join logic for the agent-bus shell shims.
#
# Sourced by shims/codex/join.sh and shims/claude-code/join.sh. The two
# wrappers differ only in the harness name and in how the model is
# detected; everything else (project/room derivation, identity
# collision, register, join, the background recv loop) is identical.
#
# What a session gets:
#   - AGENT_ID exported as <harness>@<project> (or $AGENT_ID if already set)
#   - registered on the bus (role assigned by the broker from the declared
#     model — never self-asserted), joined to #repo:<project>
#   - a detached background `bus recv` loop that appends inbound messages
#     to $AGENT_BUS_HOME/sessions/<agent>.recv.log
#
# The loop's output goes to the log file, never to the hook's stdout:
# a background process that inherits the hook's stdout keeps the pipe
# open and can block the harness waiting for the hook to finish. The
# one line join.sh itself prints (room, agent id, role, log path) is
# what the harness adds to the session context.
#
# Usage (from a wrapper):
#   AGENT_BUS_HARNESS=codex            # required
#   AGENT_BUS_MODEL_FN=detect_model    # required: prints the model or nothing
#   source "$(dirname "$0")/../lib/join-common.sh"
#   join_main "$@"                     # or: join_main stop
#
# Env overrides (all optional):
#   AGENT_ID           identity; default <harness>@<project>
#   AGENT_BUS_ROOM     room; default #repo:<project>
#   AGENT_BUS_MODEL    model to declare; overrides harness detection
#   AGENT_BUS_CLI      path to the bus CLI; default `bus` on PATH
#   AGENT_BUS_HOME     bus state dir (tokens, sessions); the CLI's default
#   AGENT_BUS_PORT     broker port; default 47113 (the CLI's default)
#   AGENT_BUS_RECV_TIMEOUT  seconds each `bus recv` blocks; default 20
#
# Exit codes: 0 on success; 1 on any failure (broker unreachable,
# registration rejected, join rejected). Failures are loud: nothing is
# silently skipped, and a session that cannot reach the bus is told so
# on stdout (so the harness adds it to the session context).
set -u

# --- configuration (set by the wrapper before sourcing) -------------------
: "${AGENT_BUS_HARNESS:?wrapper must set AGENT_BUS_HARNESS}"
: "${AGENT_BUS_MODEL_FN:?wrapper must set AGENT_BUS_MODEL_FN}"

BUS="${AGENT_BUS_CLI:-bus}"
RECV_TIMEOUT="${AGENT_BUS_RECV_TIMEOUT:-20}"
# A broker outage must not spin the loop forever: after this many
# consecutive non-timeout recv failures the loop gives up (it logs why).
RECV_MAX_FAILURES=12
# Identity collision: how far the ordinal search goes (#2..#10).
MAX_ORDINAL=10

# --- helpers ---------------------------------------------------------------

die() { printf 'agent-bus: error: %s\n' "$*" >&2; exit 1; }

# Sanitize a name for use as an agent id / room suffix / filename:
# keep letters, digits, dot, dash, underscore; anything else becomes a
# dash. The broker rejects rooms with whitespace, and the token file
# name is sanitized the same way by the CLI.
sanitize() {
    printf '%s' "$1" | tr -c 'A-Za-z0-9._-' '-' | sed -e 's/-\+/-/g' -e 's/^-//' -e 's/-$//'
}

# The project for this session: the git repo root's basename when inside
# a git repo (a session started in a subdirectory still joins the repo's
# room), else the current directory's basename. Falls back to "workspace".
project_name() {
    local top name
    if top="$(git rev-parse --show-toplevel 2>/dev/null)" && [ -n "$top" ]; then
        name="$(basename "$top")"
    else
        name="$(basename "$PWD")"
    fi
    name="$(sanitize "$name")"
    [ -n "$name" ] || name="workspace"
    printf '%s' "$name"
}

# The model to declare to the broker. Precedence:
#   1. $AGENT_BUS_MODEL (explicit override, also used by the exit test)
#   2. the harness's own config (via $AGENT_BUS_MODEL_FN, best effort)
#   3. nothing — the broker then assigns Protocol, the honest default
#      for an unknown model.
# A value is only accepted if it looks like a model identifier: non-empty,
# <= 128 chars, no whitespace, and no config syntax (quotes, backtick, =,
# #) that would mean a config line was mis-parsed. A mis-parse must never
# become a declared model.
detect_model() {
    local m=""
    if [ -n "${AGENT_BUS_MODEL:-}" ]; then
        m="$AGENT_BUS_MODEL"
    else
        m="$("$AGENT_BUS_MODEL_FN" 2>/dev/null || true)"
    fi
    [ -n "$m" ] || return 0
    [ "${#m}" -le 128 ] || return 0
    printf '%s' "$m" | grep -q '[[:space:]]' && return 0
    # Characters that only ever come from a mis-parsed config line
    # (bad_chars holds just the characters; the [ ] go in the pattern).
    local bad_chars=$'"`\'=#'
    printf '%s' "$m" | grep -q "[$bad_chars]" && return 0
    printf '%s' "$m"
}

bus_home() {
    if [ -n "${AGENT_BUS_HOME:-}" ]; then
        printf '%s' "$AGENT_BUS_HOME"
    else
        printf '%s/.agent-bus' "$HOME"
    fi
}

sessions_dir() { printf '%s/sessions' "$(bus_home)"; }

# PID/log file for an agent id, same sanitizing rule as the CLI's
# token file name (alphanumeric plus . - _ kept, anything else _).
pid_file() {
    printf '%s/%s.recv.pid' "$(sessions_dir)" \
        "$(printf '%s' "$1" | tr -c 'A-Za-z0-9._-' '_')"
}

recv_log() {
    printf '%s/%s.recv.log' "$(sessions_dir)" \
        "$(printf '%s' "$1" | tr -c 'A-Za-z0-9._-' '_')"
}

# --- register --------------------------------------------------------------
# Register $1 as $AGENT_BUS_HARNESS (model $2, or none). On identity
# collision (the id is already registered by another identity we do not
# hold the token for) the broker answers "[auth] re-register requires
# token"; try the ordinal suffixes #2..#10 (the Phase 2 rule, ADR 0004)
# and use the first free one. Any other failure prints the reason and
# returns 1. Prints the agent id that was registered.
# NOTE: this runs inside a command substitution in join_main, so it must
# NOT call die (exit would only kill the subshell) — it returns 1 and
# the caller exits.
register_agent() {
    local base="$1" model="${2:-}" candidate="$1" n=1 out rc
    local reg_args
    while :; do
        reg_args=(register --agent "$candidate" --harness "$AGENT_BUS_HARNESS")
        [ -n "$model" ] && reg_args+=(--model "$model")
        out="$("$BUS" "${reg_args[@]}" 2>&1)"
        rc=$?
        if [ "$rc" -eq 0 ]; then
            # Never print the token: `bus register` only ever prints the
            # token's path, never its value, and we keep it that way.
            printf '%s\n' "$out" >&2
            printf '%s' "$candidate"
            return 0
        fi
        case "$out" in
            *'re-register requires token'*)
                # Someone else owns this id and we do not hold its token.
                n=$((n + 1))
                if [ "$n" -gt "$MAX_ORDINAL" ]; then
                    printf 'agent-bus: error: register failed: %s is taken and no ordinal up to #%s is free (%s)\n' \
                        "$base" "$MAX_ORDINAL" "$out" >&2
                    return 1
                fi
                candidate="${base}#${n}"
                continue
                ;;
            *)
                printf 'agent-bus: error: register failed for %s: %s\n' "$candidate" "$out" >&2
                return 1
                ;;
        esac
    done
}

# --- recv loop ---------------------------------------------------------------
# One detached loop per agent. It blocks in `bus recv` (timeout $RECV_TIMEOUT)
# and appends each inbound message to the log. It stops when:
#   - its PID is no longer the one in the PID file (a newer join.sh run
#     replaced it, or `join.sh stop` removed the file), or
#   - `bus recv` fails (not a timeout) $RECV_MAX_FAILURES times in a row
#     (the broker is gone; it logs why instead of spinning forever).
# A timeout (exit 3) is not a failure — it just means no message arrived.
recv_loop_body() {
    local agent="$1" room="$2" log="$3" pidf="$4"
    local my_pid="$BASHPID" failures=0 msg rc current
    trap '' HUP
    while :; do
        # The PID file is written by the parent right after the fork,
        # so on the first pass it may not exist yet — wait briefly
        # rather than exiting (a missing file at startup is a race,
        # not a stop request). Later passes: a missing or replaced
        # file means stop (or a newer loop took over), so exit.
        # `join.sh stop` also kills the process, so this wait only
        # ever delays a stop by ~1s at most.
        current=""
        for _ in 1 2 3 4 5 6 7 8 9 10; do
            current="$(cat "$pidf" 2>/dev/null || true)"
            [ -n "$current" ] && break
            sleep 0.1
        done
        [ "$current" = "$my_pid" ] || exit 0
        if msg="$("$BUS" recv --agent "$agent" --room "$room" \
                --timeout "$RECV_TIMEOUT" --format text 2>/dev/null)"; then
            failures=0
            [ -n "$msg" ] && printf '[bus] %s\n' "$msg" >> "$log"
        else
            rc=$?
            if [ "$rc" -eq 3 ]; then
                failures=0 # timeout: no message, keep looping
            else
                failures=$((failures + 1))
                if [ "$failures" -ge "$RECV_MAX_FAILURES" ]; then
                    printf '[bus] recv failed %s times, broker unreachable, stopping loop\n' \
                        "$failures" >> "$log"
                    exit 1
                fi
                sleep 5
            fi
        fi
    done
}

# Start the loop, or reuse the one already running for this agent.
# Re-running join.sh in the same session must not start a second loop.
start_recv_loop() {
    local agent="$1" room="$2"
    local pidf log pid
    pidf="$(pid_file "$agent")"
    log="$(recv_log "$agent")"
    mkdir -p "$(sessions_dir)"
    if [ -f "$pidf" ]; then
        pid="$(cat "$pidf" 2>/dev/null || true)"
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            printf 'agent-bus: recv loop already running (pid %s, log %s)\n' "$pid" "$log" >&2
            return 0
        fi
        # Stale PID file: a previous loop died without cleaning up.
        rm -f "$pidf"
    fi
    # Detached: stdin from /dev/null and stdout/stderr to the log, so the
    # loop never holds the hook's stdout pipe open (that would block the
    # harness waiting for the hook to finish) and never reads the harness's
    # stdin. disown so the parent shell does not wait on it.
    ( recv_loop_body "$agent" "$room" "$log" "$pidf" ) >> "$log" 2>&1 < /dev/null &
    pid=$!
    printf '%s' "$pid" > "$pidf"
    disown 2>/dev/null || true
    printf '[bus] recv loop started for %s in %s (pid %s)\n' "$agent" "$room" "$pid" >> "$log"
    printf 'agent-bus: recv loop started (pid %s, log %s)\n' "$pid" "$log" >&2
}

# Stop the loop for an agent id (used by `join.sh stop` and the
# SessionEnd hook). Safe to call when nothing is running.
stop_recv_loop() {
    local agent="$1" pidf pid
    pidf="$(pid_file "$agent")"
    if [ ! -f "$pidf" ]; then
        printf 'agent-bus: no recv loop running for %s\n' "$agent"
        return 0
    fi
    pid="$(cat "$pidf" 2>/dev/null || true)"
    rm -f "$pidf"
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
        kill "$pid" 2>/dev/null || true
        for _ in 1 2 3 4 5 6 7 8 9 10; do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.5
        done
        kill -0 "$pid" 2>/dev/null && kill -9 "$pid" 2>/dev/null || true
        printf 'agent-bus: recv loop stopped for %s (pid %s)\n' "$agent" "$pid"
    else
        printf 'agent-bus: recv loop for %s was already gone (stale pid file removed)\n' "$agent"
    fi
}

# --- main --------------------------------------------------------------------

join_main() {
    # join.sh stop [agent-id]: stop the recv loop for that agent.
    # With no id, stop the loop for this session's agent
    # ($AGENT_ID or <harness>@<project>). Pass the full id (with its
    # ordinal, e.g. codex@demo#2) when the session took one — the id
    # join.sh printed at session start.
    if [ "${1:-}" = "stop" ]; then
        local stop_id="${2:-}"
        if [ -z "$stop_id" ]; then
            local project
            project="$(project_name)"
            stop_id="${AGENT_ID:-${AGENT_BUS_HARNESS}@${project}}"
        fi
        stop_recv_loop "$stop_id"
        return 0
    fi

    command -v "$BUS" >/dev/null 2>&1 \
        || die "bus CLI not found ($BUS). Set AGENT_BUS_CLI to its path or put it on PATH."

    local project room base_id agent_id model
    project="$(project_name)"
    room="${AGENT_BUS_ROOM:-#repo:${project}}"
    base_id="${AGENT_ID:-${AGENT_BUS_HARNESS}@${project}}"
    model="$(detect_model)"

    # Register first (this is also the broker-reachability check: if the
    # broker is down, register fails here and the session is told so).
    # register_agent runs in a command substitution, so a failure there
    # only ends the subshell — check the exit code here and die loudly.
    agent_id="$(register_agent "$base_id" "$model")" \
        || die "registration failed (see above)"
    export AGENT_ID="$agent_id"

    "$BUS" join --agent "$agent_id" --room "$room" >/dev/null \
        || die "join failed for $agent_id in $room (broker rejected the join)"

    start_recv_loop "$agent_id" "$room"

    # The one line the harness adds to the session context. Short on
    # purpose: hook stdout is capped context. The token is never printed.
    printf 'agent-bus: you are %s in %s. Inbound bus messages are appended to %s; read them with `bus recv --agent %s --room %s` or `bus history --room %s`, and reply with `bus send --agent %s --room %s --kind chat --text ...`.\n' \
        "$agent_id" "$room" "$(recv_log "$agent_id")" \
        "$agent_id" "$room" "$room" "$agent_id" "$room"
}
