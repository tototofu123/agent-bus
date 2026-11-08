#!/usr/bin/env bash
# shims/claude-code/join.sh — auto-join the agent bus for a Claude Code session.
#
# Run once at session start (see shims/README.md for the SessionStart
# hook). It registers this session on the bus, joins #repo:<project>,
# and starts a background `bus recv` loop that appends inbound messages
# to $AGENT_BUS_HOME/sessions/<agent>.recv.log. Run `join.sh stop`
# (or wire the SessionEnd hook) to stop the loop when the session ends.
#
# Identity: <harness>@<project> by default (override with $AGENT_ID);
# if that id is already owned by another identity, the next free
# ordinal is used (claude-code@proj#2, #3, ... — see ADR 0004).
#
# Model: declared from $AGENT_BUS_MODEL, else $ANTHROPIC_MODEL, else
# the top-level "model" key in .claude/settings.json (project, wins)
# or ~/.claude/settings.json. When the model is unknown it is omitted
# and the broker assigns Protocol. The model is never invented: an
# unversioned alias such as "sonnet" does not pin a release, so the
# broker floors it at Protocol by design (see proto::Role::assign).
set -u

AGENT_BUS_HARNESS=claude-code
AGENT_BUS_MODEL_FN=detect_claude_model

# Best-effort model read: $ANTHROPIC_MODEL first (it beats the settings
# file), then the first "model": "..." in the project settings, then the
# user settings. The grep is deliberately simple — settings.json is a
# flat object for the model key in practice — and detect_model() in
# join-common.sh rejects anything that does not look like a model id.
detect_claude_model() {
    local cfg m
    if [ -n "${ANTHROPIC_MODEL:-}" ]; then
        printf '%s' "$ANTHROPIC_MODEL"
        return 0
    fi
    for cfg in ".claude/settings.json" "$HOME/.claude/settings.json"; do
        [ -f "$cfg" ] || continue
        m="$(grep -o '"model"[[:space:]]*:[[:space:]]*"[^"]*"' "$cfg" 2>/dev/null \
            | head -1 | sed 's/^"model"[[:space:]]*:[[:space:]]*"//; s/"$//')"
        [ -n "$m" ] && { printf '%s' "$m"; return 0; }
    done
    return 0
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "$SCRIPT_DIR/../lib/join-common.sh"

join_main "$@"
