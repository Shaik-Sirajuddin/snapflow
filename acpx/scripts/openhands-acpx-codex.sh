#!/usr/bin/env bash
# Same integration point as `openhands-acpx-claude.sh` (see that file's
# doc comment for the full rationale), pre-wired to the real `codex-acp`
# adapter instead. Point OpenHands's `ACPAgentSettings.acp_command` at
# whichever of the two scripts matches the backend you want that
# conversation profile to use -- each is a complete, independent default,
# not a runtime switch (OpenHands's own `acp_server`/`acp_command` model
# has no per-conversation backend switch either; this mirrors that).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ACPX_SERVER_BIN="${ACPX_SERVER_BIN:-$SCRIPT_DIR/../target/release/acpx-server}"

export ACPX_BACKEND_CMD="${ACPX_BACKEND_CMD:-npx -y @agentclientprotocol/codex-acp@1.1.2}"
export ACPX_DEFAULT_AGENT_ID="${ACPX_DEFAULT_AGENT_ID:-codex-acp}"
export ACPX_HTTP_BIND="off"

exec "$ACPX_SERVER_BIN" "$@"
