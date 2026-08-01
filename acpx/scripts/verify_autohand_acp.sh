#!/usr/bin/env bash
set -euo pipefail

# Verifies the exact ACPX-managed AutoHand adapter path without requiring the
# AutoHand CLI or credentials. The expected result is an ACP authentication
# error, not a hanging session/new request.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ADAPTER_ROOT="${AUTOHAND_ADAPTER_ROOT:-${HOME}/.acpx/adapters/autohand}"
ADAPTER_ENTRY="${ADAPTER_ROOT}/node_modules/@autohandai/autohand-acp/dist/index.js"
ACPX_SERVER_BIN="${ACPX_SERVER_BIN:-${ROOT_DIR}/target/debug/acpx-server}"
PORT="${ACPX_VERIFY_PORT:-43991}"

if [[ ! -f "$ADAPTER_ENTRY" ]]; then
    echo "AutoHand ACP adapter not found: $ADAPTER_ENTRY" >&2
    exit 1
fi
if [[ ! -x "$ACPX_SERVER_BIN" ]]; then
    echo "acpx-server not found or not executable: $ACPX_SERVER_BIN" >&2
    echo "Build it with: cargo build --manifest-path ${ROOT_DIR}/Cargo.toml -p acpx-server" >&2
    exit 1
fi

RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/autohand-acpx-verify.XXXXXX")"
SERVER_LOG="${RUN_DIR}/acpx-server.log"
SERVER_PID=""
cleanup() {
    if [[ -n "$SERVER_PID" ]]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    rm -rf "$RUN_DIR"
}
trap cleanup EXIT

echo "Adapter: $ADAPTER_ENTRY"
echo "ACPX server: $ACPX_SERVER_BIN"

ACPX_HTTP_BIND="127.0.0.1:${PORT}" \
ACPX_DEFAULT_AGENT_ID=autohand \
ACPX_DEFAULT_ACP_COMMAND="node ${ADAPTER_ENTRY}" \
ACPX_DB_PATH="${RUN_DIR}/state.sqlite3" \
RUST_LOG=info \
"$ACPX_SERVER_BIN" >"${RUN_DIR}/stdout.log" 2>"$SERVER_LOG" < /dev/null &
SERVER_PID=$!

for _ in $(seq 1 100); do
    if curl -fsS "http://127.0.0.1:${PORT}/health" >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done

if ! curl -fsS "http://127.0.0.1:${PORT}/health" >/dev/null 2>&1; then
    echo "acpx-server did not become ready" >&2
    cat "$SERVER_LOG" >&2
    exit 1
fi

initialize_response="$(curl --fail-with-body --max-time 10 -sS \
    -X POST "http://127.0.0.1:${PORT}/rpc" \
    -H 'content-type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}')"
printf '%s\n' "$initialize_response" | grep -q '"authMethods"' || {
    echo "initialize did not expose authMethods" >&2
    exit 1
}

session_response="$(curl --fail-with-body --max-time 10 -sS \
    -X POST "http://127.0.0.1:${PORT}/rpc" \
    -H 'content-type: application/json' \
    -d '{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp"}}')"

printf '%s\n' "$session_response"
printf '%s\n' "$session_response" | grep -qi 'backend requires authentication' || {
    echo "session/new did not return the expected authentication error" >&2
    echo "Server log: $SERVER_LOG" >&2
    exit 1
}

echo "PASS: ACPX started the installed AutoHand adapter and propagated its auth error."
echo "Expected UI follow-up: route this pre-session error by bridge thread index so it does not remain Loading."
