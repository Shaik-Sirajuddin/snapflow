#!/usr/bin/env bash
# Comprehensive ACP Node matrix (M1–M10). Runs as unprivileged tester.
# See memory/acpx/gen/plans/acp-local-node-runtime/00-plan.md
set -uo pipefail

FAILURES=0
pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*" >&2; FAILURES=$((FAILURES + 1)); }

export SNAPFLOW_INSTALL_DIR="${SNAPFLOW_INSTALL_DIR:-$HOME/.local/share/snapflow}"
# shellcheck disable=SC1091
. /home/tester/lib/acp-node-runtime.sh

# Strip any accidental node from PATH for baseline.
export PATH="/usr/bin:/bin"
unset SNAPFLOW_ACP_NODE_HOME 2>/dev/null || true

ACPX_BIN="${ACPX_SERVER_BIN:-/acpx-server}"

echo "========== M1: no system node, no bundle → missing =========="
src="$(acp_node_resolve_source)"
if [ "$src" = "missing" ]; then
  pass "M1 resolve=missing"
else
  fail "M1 expected missing, got $src"
fi

echo "========== M2: ensure bundled from official dist =========="
if acp_node_ensure 0; then
  pass "M2 acp_node_ensure exit 0"
else
  fail "M2 acp_node_ensure failed"
fi
src="$(acp_node_resolve_source)"
if [ "$src" = "bundled" ]; then
  pass "M2 resolve=bundled"
else
  fail "M2 expected bundled after ensure, got $src (system node leaked?)"
fi
bundle="$(acp_node_install_dir)/runtime/node"
if acp_node_prefix_ok "$bundle"; then
  pass "M2 prefix_ok $bundle"
else
  fail "M2 bundle incomplete at $bundle"
fi

echo "========== M3: doctor after ensure =========="
if acp_node_doctor; then
  pass "M3 doctor OK"
else
  fail "M3 doctor failed"
fi

echo "========== M4: version stamp =========="
ver="$(cat "$bundle/.version" 2>/dev/null || true)"
if [ -n "$ver" ]; then
  pass "M4 version stamp $ver"
else
  fail "M4 missing .version"
fi

# Apply bundled env for ACP children (sticky)
acp_node_export_for_acp
eval "$(acp_node_resolve | sed -n 's/^node=/NODE_BIN=/p;s/^npm=/NPM_BIN=/p;s/^npx=/NPX_BIN=/p')"

echo "========== M10: sticky same prefix =========="
nd="$(dirname "$NODE_BIN")"
md="$(dirname "$NPM_BIN")"
xd="$(dirname "$NPX_BIN")"
if [ "$nd" = "$md" ] && [ "$md" = "$xd" ]; then
  pass "M10 node/npm/npx same bin dir $nd"
else
  fail "M10 mixed prefixes node=$nd npm=$md npx=$xd"
fi

echo "========== M5/M6: ACP status + install with bundled node =========="
if [ ! -x "$ACPX_BIN" ]; then
  fail "M5/M6 acpx-server missing at $ACPX_BIN (mount build binary)"
else
  LOG="$(mktemp)"
  export PATH="$(dirname "$NODE_BIN"):$PATH"
  export SNAPFLOW_ACP_NODE_HOME="$(cd "$(dirname "$NODE_BIN")/.." && pwd)"
  export ACPX_HTTP_BIND="127.0.0.1:18790"
  export ACPX_DEFAULT_ACP_COMMAND="$NPX_BIN -y @agentclientprotocol/codex-acp@1.1.2"
  export ACPX_DEFAULT_AGENT_ID="codex-acp"
  export ACPX_DB_PATH="$HOME/acpx-docker.sqlite3"
  export RUST_LOG=info
  "$ACPX_BIN" >"$LOG" 2>&1 &
  ACPX_PID=$!
  ready=0
  for i in $(seq 1 30); do
    if curl -sf "http://127.0.0.1:18790/health" >/dev/null 2>&1; then
      ready=1
      break
    fi
    if ! kill -0 "$ACPX_PID" 2>/dev/null; then
      break
    fi
    sleep 0.5
  done
  if [ "$ready" = "1" ]; then
    pass "M5 acpx-server health OK"
  else
    fail "M5 acpx-server did not become healthy"
    echo "--- acpx log ---"; cat "$LOG" | tail -40
  fi

  # agents/status should not be runtime_missing with bundled node
  STATUS_JSON="$(curl -sf -X POST "http://127.0.0.1:18790/rpc" \
    -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"agents/status","params":{"id":"codex-acp"}}' 2>/dev/null || true)"
  if echo "$STATUS_JSON" | grep -q 'runtime_missing'; then
    fail "M5 agents/status runtime_missing: $STATUS_JSON"
  elif echo "$STATUS_JSON" | grep -qE 'not_installed|installed'; then
    pass "M5 agents/status has node (not runtime_missing): $STATUS_JSON"
  else
    fail "M5 unexpected status response: $STATUS_JSON"
  fi

  # agents/install real adapter pre-fetch via bundled npm
  INSTALL_JSON="$(curl -sf -X POST "http://127.0.0.1:18790/rpc" \
    -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","id":2,"method":"agents/install","params":{"id":"codex-acp"}}' 2>/dev/null || true)"
  if echo "$INSTALL_JSON" | grep -q 'PackageReady'; then
    pass "M6 agents/install PackageReady"
  else
    fail "M6 agents/install failed: $INSTALL_JSON"
    echo "--- acpx log ---"; cat "$LOG" | tail -40
  fi

  kill -TERM "$ACPX_PID" 2>/dev/null || true
  wait "$ACPX_PID" 2>/dev/null || true
fi

echo "========== M7: global first wins when PATH has a 'global' node (bundle still present) =========="
# Prefer a mounted host node only if it actually runs npm inside the container
# (nvm/host mounts often break). Otherwise simulate global with PATH-first
# symlinks to the already-installed official bins under a different prefix.
GLOBAL_PREFIX=""
if [ -x /host-global-node/bin/node ] && [ -x /host-global-node/bin/npm ] && \
   PATH="/host-global-node/bin:/usr/bin:/bin" /host-global-node/bin/npm --version >/dev/null 2>&1; then
  GLOBAL_PREFIX="/host-global-node"
  echo "NOTE: M7 using host-global-node mount"
else
  GLOBAL_PREFIX="$HOME/fake-global-node"
  mkdir -p "$GLOBAL_PREFIX/bin"
  ln -sfn "$bundle/bin/node" "$GLOBAL_PREFIX/bin/node"
  ln -sfn "$bundle/bin/npm" "$GLOBAL_PREFIX/bin/npm"
  ln -sfn "$bundle/bin/npx" "$GLOBAL_PREFIX/bin/npx"
  echo "NOTE: M7 using fake-global-node (PATH-first) at $GLOBAL_PREFIX"
fi
export PATH="$GLOBAL_PREFIX/bin:/usr/bin:/bin"
unset SNAPFLOW_ACP_NODE_HOME
# Bundle remains under original SNAPFLOW_INSTALL_DIR
export SNAPFLOW_INSTALL_DIR="${SNAPFLOW_INSTALL_DIR:-$HOME/.local/share/snapflow}"
# Restore install dir used during M2 if we had not changed it yet
if [ ! -x "$SNAPFLOW_INSTALL_DIR/runtime/node/bin/node" ]; then
  export SNAPFLOW_INSTALL_DIR="$HOME/.local/share/snapflow"
fi

src="$(acp_node_resolve_source)"
if [ "$src" = "global" ]; then
  pass "M7 resolve=global (global-first) while bundle still installed"
else
  fail "M7 expected global, got $src"
fi
NODE_G="$(acp_node_resolve | sed -n 's/^node=//p')"
if echo "$NODE_G" | grep -qE 'host-global-node|fake-global-node'; then
  pass "M7 picked global-prefix path $NODE_G"
else
  fail "M7 did not pick global prefix: $NODE_G"
fi

echo "========== M9: reinstall adapter under global npm =========="
if [ -x "$ACPX_BIN" ]; then
  LOG2="$(mktemp)"
  export ACPX_HTTP_BIND="127.0.0.1:18791"
  unset SNAPFLOW_ACP_NODE_HOME
  export PATH="$GLOBAL_PREFIX/bin:/usr/bin:/bin"
  export ACPX_DEFAULT_ACP_COMMAND="$GLOBAL_PREFIX/bin/npx -y @agentclientprotocol/codex-acp@1.1.2"
  "$ACPX_BIN" >"$LOG2" 2>&1 &
  PID2=$!
  for i in $(seq 1 30); do
    curl -sf "http://127.0.0.1:18791/health" >/dev/null 2>&1 && break
    sleep 0.5
  done
  INST2="$(curl -sf -X POST "http://127.0.0.1:18791/rpc" \
    -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","id":3,"method":"agents/install","params":{"id":"codex-acp"}}' 2>/dev/null || true)"
  if echo "$INST2" | grep -q 'PackageReady'; then
    pass "M9 agents/install under global node PackageReady"
  else
    fail "M9 agents/install under global failed: $INST2"
    echo "--- acpx log ---"; tail -30 "$LOG2"
  fi
  kill -TERM "$PID2" 2>/dev/null || true
  wait "$PID2" 2>/dev/null || true
else
  fail "M9 skipped: no acpx-server"
fi

echo "========== M8: global only (no bundle under install dir) =========="
EMPTY="$HOME/empty-install"
mkdir -p "$EMPTY"
export SNAPFLOW_INSTALL_DIR="$EMPTY"
export PATH="$GLOBAL_PREFIX/bin:/usr/bin:/bin"
unset SNAPFLOW_ACP_NODE_HOME
src="$(acp_node_resolve_source)"
if [ "$src" = "global" ]; then
  pass "M8 resolve=global with no bundle under install dir"
else
  fail "M8 expected global, got $src"
fi

echo ""
if [ "$FAILURES" -eq 0 ]; then
  echo "==> acp-node-runtime matrix: ALL PASS"
  exit 0
else
  echo "==> acp-node-runtime matrix: $FAILURES FAIL(s)" >&2
  exit 1
fi
