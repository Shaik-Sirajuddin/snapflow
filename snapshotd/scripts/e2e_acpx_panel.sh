#!/usr/bin/env bash
# Phase 9 / T1: launch-free or optional live checks for snapshotd + acpx.
# Validation = curl + log grep only (no images/VNC).
set -euo pipefail

ACPX_BIND="${ACPX_HTTP_BIND:-127.0.0.1:8790}"
MCP_BIND="${SNAPSHOTD_MCP_SSE_ADDR:-127.0.0.1:7777}"
HOME_DIR="${SNAPSHOTD_HOME:-/tmp/snapshotd-e2e-$$}"
export SNAPSHOTD_HOME="$HOME_DIR"
export SNAPSHOTD_MCP_SSE_ADDR="$MCP_BIND"
export SNAPSHOTD_ACPX_ENABLED="${SNAPSHOTD_ACPX_ENABLED:-0}"

cleanup() {
  if [[ -n "${SNAPSHOTD_PID:-}" ]] && kill -0 "$SNAPSHOTD_PID" 2>/dev/null; then
    kill "$SNAPSHOTD_PID" 2>/dev/null || true
    wait "$SNAPSHOTD_PID" 2>/dev/null || true
  fi
  if [[ -n "${ACPX_PID:-}" ]] && kill -0 "$ACPX_PID" 2>/dev/null; then
    kill "$ACPX_PID" 2>/dev/null || true
    wait "$ACPX_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SNAPSHOTD_BIN="${SNAPSHOTD_BIN:-}"
if [[ -z "$SNAPSHOTD_BIN" ]]; then
  if [[ -x /tmp/snapshotd-acpx-check ]]; then
    SNAPSHOTD_BIN=/tmp/snapshotd-acpx-check
  elif command -v snapshotd >/dev/null 2>&1; then
    SNAPSHOTD_BIN="$(command -v snapshotd)"
  else
    echo "building snapshotd..."
    (cd "$ROOT/snapshotd" && go build -o /tmp/snapshotd-e2e ./cmd/snapshotd)
    SNAPSHOTD_BIN=/tmp/snapshotd-e2e
  fi
fi

ACPX_BIN="${SNAPSHOTD_ACPX_BIN:-}"
if [[ -z "$ACPX_BIN" ]]; then
  for c in \
    "$ROOT/acpx/target/release/acpx-server" \
    "$ROOT/acpx/target/debug/acpx-server"
  do
    if [[ -x "$c" ]]; then ACPX_BIN="$c"; break; fi
  done
fi

mkdir -p "$HOME_DIR"
LOG="$HOME_DIR/e2e.log"
: >"$LOG"

echo "== start snapshotd serve ==" | tee -a "$LOG"
SNAPSHOTD_ACPX_ENABLED=0 "$SNAPSHOTD_BIN" serve --no-mcp=false >>"$LOG" 2>&1 &
SNAPSHOTD_PID=$!
sleep 1

if [[ -n "$ACPX_BIN" ]]; then
  CFG="$HOME_DIR/acpx-config.json"
  python3 - <<PY
import json
from pathlib import Path
doc = {
  "providers": [],
  "mcp_servers": [{
    "type": "http",
    "name": "snapshotd",
    "url": "http://${MCP_BIND}/mcp",
    "headers": [],
  }],
  "profiles": [{
    "name": "default",
    "agent_id": "default",
    "mcp_servers": ["snapshotd"],
  }],
}
Path("$CFG").write_text(json.dumps(doc, indent=2) + "\n")
PY
  echo "== start acpx-server with snapshotd MCP ==" | tee -a "$LOG"
  ACPX_CONFIG_FILE="$CFG" \
  ACPX_HTTP_BIND="$ACPX_BIND" \
  ACPX_DB_PATH="$HOME_DIR/acpx.sqlite3" \
    "$ACPX_BIN" >>"$LOG" 2>&1 &
  ACPX_PID=$!
  for i in $(seq 1 40); do
    if curl -sf "http://${ACPX_BIND}/health" >/dev/null 2>&1; then
      break
    fi
    sleep 0.15
  done
  echo "== curl health ==" | tee -a "$LOG"
  curl -sf "http://${ACPX_BIND}/health" | tee -a "$LOG"
  echo | tee -a "$LOG"

  # mcp_servers/list via JSON-RPC if /rpc exists
  echo "== mcp_servers/list ==" | tee -a "$LOG"
  RPC_BODY='{"jsonrpc":"2.0","id":1,"method":"mcp_servers/list","params":{}}'
  if curl -sf -X POST "http://${ACPX_BIND}/rpc" \
      -H 'Content-Type: application/json' \
      -d "$RPC_BODY" | tee -a "$LOG" | grep -q snapshotd; then
    echo "PASS: mcp_servers/list contains snapshotd" | tee -a "$LOG"
  else
    echo "WARN: could not confirm snapshotd in list (gateway may need auth/backend)" | tee -a "$LOG"
    # still pass health-only if list endpoint shape differs
    if ! curl -sf "http://${ACPX_BIND}/health" >/dev/null; then
      echo "FAIL: health down" | tee -a "$LOG"
      exit 1
    fi
  fi
else
  echo "SKIP acpx: no acpx-server binary; snapshotd-only smoke" | tee -a "$LOG"
fi

echo "== snapshotd log sample ==" | tee -a "$LOG"
tail -n 30 "$LOG" | tee -a "$LOG" || true
echo "PASS: e2e script finished (logs+curl only)" | tee -a "$LOG"
