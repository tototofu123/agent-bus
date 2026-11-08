#!/usr/bin/env bash
# shims/codex/join.sh — auto-join the agent bus for a Codex CLI session.
#
# Run once at session start (see shims/README.md for the SessionStart
# hook). It registers this session on the bus, joins #repo:<project>,
# and starts a background `bus recv` loop that appends inbound messages
# to $AGENT_BUS_HOME/sessions/<agent>.recv.log. Run `join.sh stop`
# (or wire the SessionEnd hook) to stop the loop when the session ends.
#
# Identity: <harness>@<project> by default (override with $AGENT_ID);
# if that id is already owned by another identity, the next free
# ordinal is used (codex@proj#2, #3, ... — see ADR 0004).
#
# Model: declared from $AGENT_BUS_MODEL, else the top-level `model =`
# in .codex/config.toml (project, wins) or ~/.codex/config.toml
# ($CODEX_HOME/config.toml if $CODEX_HOME is set). When the model is
# unknown it is omitted and the broker assigns Protocol. The model is
# never invented: an unversioned alias or a value the broker cannot
# place floors at Protocol by design (see proto::Role::assign).
set -u

AGENT_BUS_HARNESS=codex
AGENT_BUS_MODEL_FN=detect_codex_model

# Best-effort model read: the top-level `model = "..."` (or '...')
# before the first [section] in the project config, else the user
# config. A model under a [profile] or [model_providers.*] section is
# not read — the top-level default is what a session runs on unless a
# profile is explicitly selected.
detect_codex_model() {
    local cfg line m
    local user_cfg="${CODEX_HOME:-$HOME/.codex}/config.toml"
    for cfg in ".codex/config.toml" "$user_cfg"; do
        [ -f "$cfg" ] || continue
        line="$(awk '/^[[:space:]]*\[/ { exit } /^[[:space:]]*model[[:space:]]*=/ { print; exit }' "$cfg")"
        [ -n "$line" ] || continue
        m="$(printf '%s' "$line" | grep -o '"[^"]*"' | head -1 | tr -d '"')"
        [ -z "$m" ] && m="$(printf '%s' "$line" | grep -o "'[^']*'" | head -1 | tr -d "'")"
        [ -n "$m" ] && { printf '%s' "$m"; return 0; }
    done
    return 0
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "$SCRIPT_DIR/../lib/join-common.sh"

join_main "$@"
