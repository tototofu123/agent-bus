#!/usr/bin/env bash
# shims/test-join.sh — Phase 2 auto-join exit test.
#
# Runs 10 simulated session starts (5 codex + 5 claude-code) against a
# real broker on a test port with temp state, and asserts:
#   1. every start registers exactly once (10 distinct agents in the room),
#   2. every start joins the right room (#repo:demo, even when started
#      from a subdirectory — the room comes from the git root, not $PWD),
#   3. a task.offer posted to the room reaches every session's background
#      recv loop (each loop's log contains the offer),
#   4. re-running join.sh in the same session is idempotent (no duplicate
#      agent, no second recv loop),
#   5. `join.sh stop` stops a loop (base id and ordinal id).
#
# Each start gets its own AGENT_BUS_HOME, i.e. it simulates 10 distinct
# machines/users. That is what exercises the identity-collision rule:
# the 2nd codex start finds codex@demo taken and takes codex@demo#2,
# and so on. On one shared home the sessions would share the token file
# and therefore the identity — see ADR 0004 for that limitation.
#
# The test never touches the real broker (port 47113) or the real
# $AGENT_BUS_HOME: it uses a random test port and temp dirs throughout.
# Run from the repo root:  bash shims/test-join.sh
set -u

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUS="$REPO_ROOT/target/debug/bus"
BUSD="$REPO_ROOT/target/debug/busd"
export PATH="$HOME/.cargo/bin:$PATH"

PASS=0; FAIL=0
ok()   { PASS=$((PASS + 1)); printf 'PASS %s\n' "$1"; }
bad()  { FAIL=$((FAIL + 1)); printf 'FAIL %s\n' "$1"; }

TMP="$(mktemp -d)"
BUSD_PID=""
declare -a HOMES=()    # one AGENT_BUS_HOME per simulated start
declare -a AGENTS=()   # agent id per start, in start order

cleanup() {
    # Kill every recv loop by its PID file, then the broker. Runs on
    # success and on failure.
    local pidf pid
    for home in "${HOMES[@]}" "$TMP/home-poster" "$BROKER_HOME"; do
        for pidf in "$home"/sessions/*.recv.pid; do
            [ -f "$pidf" ] || continue
            pid="$(cat "$pidf" 2>/dev/null || true)"
            [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
            rm -f "$pidf"
        done
    done
    if [ -n "$BUSD_PID" ] && kill -0 "$BUSD_PID" 2>/dev/null; then
        kill "$BUSD_PID" 2>/dev/null || true
        wait "$BUSD_PID" 2>/dev/null || true
    fi
    rm -rf "$TMP"
}
trap cleanup EXIT

# --- build ---------------------------------------------------------------
printf 'building bus + busd...\n'
if ! ( cd "$REPO_ROOT" && cargo build -q -p bus-cli -p busd ); then
    bad "cargo build bus + busd"
    printf 'FAIL %s passed, %s failed\n' "$PASS" "$FAIL"
    exit 1
fi
ok "cargo build bus + busd"
[ -x "$BUS" ] && [ -x "$BUSD" ] || { bad "binaries exist"; exit 1; }

# --- broker on a random free port, temp state ------------------------------
free_port() {
    local p
    for _ in $(seq 1 50); do
        p=$((20000 + RANDOM % 20000))
        ( exec 3<>"/dev/tcp/127.0.0.1/$p" ) 2>/dev/null || { printf '%s' "$p"; return 0; }
    done
    return 1
}
PORT="$(free_port)" || { bad "find a free port"; exit 1; }
export AGENT_BUS_PORT="$PORT"
BROKER_HOME="$TMP/broker-home"
mkdir -p "$BROKER_HOME"
"$BUSD" --port "$PORT" --db "$BROKER_HOME/history.sqlite" \
    --pid-file "$BROKER_HOME/busd.pid" > "$TMP/busd.log" 2>&1 &
BUSD_PID=$!
# Wait for the broker to answer (up to 10 s).
ready=0
for _ in $(seq 1 50); do
    if AGENT_BUS_HOME="$BROKER_HOME" "$BUS" who >/dev/null 2>&1; then ready=1; break; fi
    sleep 0.2
done
[ "$ready" = 1 ] || { bad "broker answers on port $PORT"; cat "$TMP/busd.log"; exit 1; }
ok "broker answers on port $PORT"

# --- a git repo named demo ---------------------------------------------------
mkdir -p "$TMP/demo/sub"
( cd "$TMP/demo" && git init -q . ) || { bad "git init demo repo"; exit 1; }
ok "git repo demo"

# --- 10 session starts -------------------------------------------------------
# start_join $harness $home $cwd [$model]
# Runs join.sh once, records the agent id from its stdout, asserts exit 0.
start_join() {
    local harness="$1" home="$2" cwd="$3" model="${4:-}"
    local out rc agent
    mkdir -p "$home"
    out="$(cd "$cwd" && (
        export AGENT_BUS_HOME="$home" AGENT_BUS_CLI="$BUS"
        [ -n "$model" ] && export AGENT_BUS_MODEL="$model"
        bash "$REPO_ROOT/shims/$harness/join.sh" 2>"$TMP/last.err"
    ))"
    rc=$?
    if [ "$rc" -ne 0 ]; then
        bad "join.sh $harness (cwd $cwd) exits 0 (got $rc): $(head -1 "$TMP/last.err")"
        return 1
    fi
    agent="$(printf '%s' "$out" | sed -n 's/^agent-bus: you are \([^ ]*\) in .*/\1/p')"
    if [ -z "$agent" ]; then
        bad "join.sh $harness prints an agent id"
        return 1
    fi
    HOMES+=("$home")
    AGENTS+=("$agent")
    ok "join.sh $harness -> $agent"
}

# 5 codex starts. The first declares a model (exercises --model and the
# Peer role); the 4th starts from a subdirectory (exercises git-root
# room derivation). Each start has its own home, so the 2nd..5th must
# take the ordinal suffixes #2..#5.
start_join codex "$TMP/home-c1" "$TMP/demo" "gpt-5-codex"
start_join codex "$TMP/home-c2" "$TMP/demo"
start_join codex "$TMP/home-c3" "$TMP/demo"
start_join codex "$TMP/home-c4" "$TMP/demo/sub"
start_join codex "$TMP/home-c5" "$TMP/demo"
# 5 claude-code starts.
start_join claude-code "$TMP/home-l1" "$TMP/demo"
start_join claude-code "$TMP/home-l2" "$TMP/demo"
start_join claude-code "$TMP/home-l3" "$TMP/demo"
start_join claude-code "$TMP/home-l4" "$TMP/demo"
start_join claude-code "$TMP/home-l5" "$TMP/demo"

# --- 10 distinct agents, all in #repo:demo ----------------------------------
who_out="$(AGENT_BUS_HOME="$BROKER_HOME" "$BUS" who --room '#repo:demo')"
count="$(printf '%s\n' "$who_out" | grep -c '^codex@demo\|^claude-code@demo' || true)"
if [ "$count" = 10 ]; then ok "10 distinct agents in #repo:demo";
else bad "10 distinct agents in #repo:demo (got $count):
$who_out"; fi
# Ordinals: codex@demo, #2..#5 and claude-code@demo, #2..#5.
for want in codex@demo codex@demo#2 codex@demo#3 codex@demo#4 codex@demo#5 \
            claude-code@demo claude-code@demo#2 claude-code@demo#3 \
            claude-code@demo#4 claude-code@demo#5; do
    printf '%s\n' "$who_out" | grep -q "^$want " \
        && ok "agent $want present" || bad "agent $want present"
done
# The subdirectory start joined the repo room, not #repo:sub.
printf '%s\n' "$who_out" | grep -q '^codex@demo#4 ' \
    && ok "subdirectory start is codex@demo#4 (git root, not sub)" \
    || bad "subdirectory start is codex@demo#4 (git root, not sub)"
# Roles: the gpt-5-codex start is Peer, the model-less starts Protocol.
printf '%s\n' "$who_out" | grep '^codex@demo ' | grep -q 'role=peer' \
    && ok "codex@demo role=peer (gpt-5-codex)" || bad "codex@demo role=peer (gpt-5-codex)"
printf '%s\n' "$who_out" | grep '^codex@demo#2 ' | grep -q 'role=protocol' \
    && ok "codex@demo#2 role=protocol (no model)" || bad "codex@demo#2 role=protocol (no model)"

# --- a task.offer reaches every recv loop ------------------------------------
POSTER_HOME="$TMP/home-poster"
mkdir -p "$POSTER_HOME"
AGENT_BUS_HOME="$POSTER_HOME" "$BUS" register --agent poster@demo \
    --harness test --model qwen2.5-coder-7b >/dev/null \
    || { bad "poster registers"; exit 1; }
AGENT_BUS_HOME="$POSTER_HOME" "$BUS" join --agent poster@demo \
    --room '#repo:demo' >/dev/null || { bad "poster joins"; exit 1; }
OFFER_TEXT="review the auth refactor before Friday"
AGENT_BUS_HOME="$POSTER_HOME" "$BUS" send --agent poster@demo \
    --room '#repo:demo' --kind task.offer --text "$OFFER_TEXT" \
    --payload '{"files":[]}' >/dev/null || { bad "poster sends task.offer"; exit 1; }
ok "poster sends task.offer"
# Each loop blocks up to 20 s in recv; poll the logs for up to 30 s.
deadline=$((SECONDS + 30))
got_all=0
while [ "$SECONDS" -lt "$deadline" ]; do
    got_all=1
    for i in "${!HOMES[@]}"; do
        log="$(ls "${HOMES[$i]}"/sessions/*.recv.log 2>/dev/null | head -1)"
        if [ -z "$log" ] || ! grep -qF "$OFFER_TEXT" "$log" 2>/dev/null; then
            got_all=0; break
        fi
    done
    [ "$got_all" = 1 ] && break
    sleep 1
done
[ "$got_all" = 1 ] && ok "all 10 recv loops got the task.offer" \
    || bad "all 10 recv loops got the task.offer"
# Per-loop check (so a failure names the session).
for i in "${!HOMES[@]}"; do
    log="$(ls "${HOMES[$i]}"/sessions/*.recv.log 2>/dev/null | head -1)"
    if [ -n "$log" ] && grep -qF "$OFFER_TEXT" "$log" 2>/dev/null; then
        ok "loop ${AGENTS[$i]} got the offer"
    else
        bad "loop ${AGENTS[$i]} got the offer (log: ${log:-none})"
    fi
done

# --- idempotency: re-run join.sh for the first start ------------------------
first_pid="$(cat "${HOMES[0]}"/sessions/*.recv.pid 2>/dev/null | head -1)"
( cd "$TMP/demo" && AGENT_BUS_HOME="${HOMES[0]}" AGENT_BUS_CLI="$BUS" \
    AGENT_BUS_MODEL="gpt-5-codex" \
    bash "$REPO_ROOT/shims/codex/join.sh" >/dev/null 2>&1 ) \
    && ok "re-run join.sh exits 0" || bad "re-run join.sh exits 0"
who2="$(AGENT_BUS_HOME="$BROKER_HOME" "$BUS" who --room '#repo:demo')"
count2="$(printf '%s\n' "$who2" | grep -c '^codex@demo\|^claude-code@demo' || true)"
[ "$count2" = 10 ] && ok "re-run adds no duplicate agent (still 10)" \
    || bad "re-run adds no duplicate agent (still 10, got $count2)"
second_pid="$(cat "${HOMES[0]}"/sessions/*.recv.pid 2>/dev/null | head -1)"
[ "$first_pid" = "$second_pid" ] && [ -n "$first_pid" ] \
    && ok "re-run reuses the recv loop (pid $first_pid)" \
    || bad "re-run reuses the recv loop (was $first_pid, now $second_pid)"

# --- stop: an ordinal loop and the base loop ---------------------------------
# codex@demo#2 lives in home-c2; stop it by full id.
stop_pid="$(cat "${HOMES[1]}"/sessions/*.recv.pid 2>/dev/null | head -1)"
( cd "$TMP/demo" && AGENT_BUS_HOME="${HOMES[1]}" AGENT_BUS_CLI="$BUS" \
    bash "$REPO_ROOT/shims/codex/join.sh" stop 'codex@demo#2' >/dev/null 2>&1 )
sleep 1
if [ -n "$stop_pid" ] && ! kill -0 "$stop_pid" 2>/dev/null \
    && [ ! -f "${HOMES[1]}"/sessions/codex_demo_2.recv.pid ]; then
    ok "join.sh stop codex@demo#2 kills the loop and removes the pid file"
else
    bad "join.sh stop codex@demo#2 kills the loop and removes the pid file"
fi

printf '\n%s passed, %s failed\n' "$PASS" "$FAIL"
[ "$FAIL" = 0 ]
