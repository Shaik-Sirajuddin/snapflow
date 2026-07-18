#!/usr/bin/env bash
# Launches `acpx-server` as a stdio ACP subprocess, pre-wired to spawn the
# real `claude-agent-acp` adapter as its default (native/unmanaged-mode --
# no `_acpx.profile` needed) backend, for OpenHands's `ACPAgentSettings`
# `acp_server="custom"` / `acp_command=[<this script>]` integration point.
#
# Why a wrapper script and not a bare `acp_command=["acpx-server"]`:
# OpenHands's `ACPAgentSettings` has no env-var field, only `acp_command`
# (program + args) and `acp_args` -- `acpx-server` itself is entirely
# env-var configured (see `../README.md`'s "Configuration" section), so
# something has to set that env before exec'ing the real binary. This is
# that something.
#
# `ACPX_HTTP_BIND=off`: OpenHands spawns one `acpx-server` instance per
# conversation and only ever talks to its stdio, never HTTP/WS -- with
# HTTP/WS left at its default fixed port, two concurrent conversations
# would contend for the same port for a transport neither one uses (see
# `acpx-server/src/config.rs`'s `ACPX_HTTP_BIND` doc comment; a bind
# conflict alone is non-fatal to stdio since that fix, but there is still
# no reason to attempt -- and log a warning about -- a bind this process
# will never use).
#
# `ACPX_DB_PATH` -- deliberately left unset: OpenHands's own conversation
# store is this integration's persistence layer; acpx's session/
# transcript persistence would be redundant per-process state with no
# consumer, since each conversation gets its own disposable `acpx-server`
# instance rather than one long-lived shared daemon.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ACPX_SERVER_BIN="${ACPX_SERVER_BIN:-$SCRIPT_DIR/../target/release/acpx-server}"

export ACPX_BACKEND_CMD="${ACPX_BACKEND_CMD:-npx -y @agentclientprotocol/claude-agent-acp@0.58.1}"
export ACPX_DEFAULT_AGENT_ID="${ACPX_DEFAULT_AGENT_ID:-claude-acp}"
export ACPX_HTTP_BIND="off"

exec "$ACPX_SERVER_BIN" "$@"
