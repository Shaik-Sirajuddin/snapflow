#!/usr/bin/env bash
# OpenHands ACP command for a shared, already-running ACPX daemon.
#
# Unlike the backend-specific wrappers, this never starts acpx-server. One
# daemon owns the adapter registry, connector pool, retention policy, and
# durable sessions; each OpenHands ACP subprocess is only a local stdio
# bridge to its `/acp/ws` endpoint.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ACPX_ACP_BRIDGE_BIN="${ACPX_ACP_BRIDGE_BIN:-$SCRIPT_DIR/../target/release/acpx-acp-bridge}"
export ACPX_ACP_BRIDGE_URL="${ACPX_ACP_BRIDGE_URL:-ws://127.0.0.1:8790/acp/ws}"

exec "$ACPX_ACP_BRIDGE_BIN" "$@"
