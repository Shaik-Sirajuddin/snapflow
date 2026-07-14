#!/usr/bin/env bash
set -euo pipefail

# One-shot, black-box self-test for the acpx workspace: builds everything,
# boots a real acpx-server against a trivial stand-in backend, then runs the
# acpx-selftest CLI (acpx-server/src/bin/selftest.rs) against it end-to-end.
# Intended for humans and CI to run as a smoke test on top of `cargo test
# --workspace` (which remains the primary correctness suite).

# Resolve the acpx workspace root regardless of the caller's cwd.
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

SERVER_PID=""
BACKEND_SCRIPT=""
STDIN_FIFO=""

cleanup() {
  local status=$?
  if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  if [[ -n "$BACKEND_SCRIPT" ]] && [[ -f "$BACKEND_SCRIPT" ]]; then
    rm -f "$BACKEND_SCRIPT"
  fi
  if [[ -n "$STDIN_FIFO" ]] && [[ -p "$STDIN_FIFO" ]]; then
    rm -f "$STDIN_FIFO"
  fi
  return "$status"
}
trap cleanup EXIT

echo "==> Building workspace (cargo build --workspace)"
cargo build --workspace

# Pick a free local TCP port by asking the OS for an ephemeral one (bind to
# port 0, read back the assigned port, close it). This is simpler and more
# portable across Linux setups than parsing `ss`/`nc` output, and avoids the
# race of grepping /proc/net/tcp for "unused" ports.
PORT="$(python3 -c '
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
')"
echo "==> Using port ${PORT}"

# Trivial stand-in backend: reads newline-delimited JSON-RPC requests and
# replies with a canned success result, echoing the request id back. Mirrors
# STAND_IN_BACKEND_SCRIPT in acpx-core/tests/router_dispatch_test.rs. Written
# to a temp file because ACPX_BACKEND_CMD is parsed by naive whitespace
# splitting (see acpx-server/src/config.rs) and can't hold an inline
# multi-word script.
BACKEND_SCRIPT="$(mktemp /tmp/acpx-selftest-backend.XXXXXX.sh)"
cat > "$BACKEND_SCRIPT" <<'EOF'
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
done
EOF

echo "==> Starting acpx-server on 127.0.0.1:${PORT}"
# acpx-server also runs a stdio transport that races the HTTP transport in a
# tokio::select! (see acpx-server/src/main.rs) -- if its stdin hits EOF
# immediately (the default for a backgrounded process with no live stdin),
# the whole process exits right away even though the HTTP listener is fine.
# Feed it a FIFO that we (the script) hold open for both reading and
# writing on fd 3 -- since the write end never closes until this script
# exits, the server's stdin never sees EOF, and unlike `sleep N` via
# process substitution this doesn't leak a background process.
STDIN_FIFO="$(mktemp -u /tmp/acpx-selftest-stdin.XXXXXX.fifo)"
mkfifo "$STDIN_FIFO"
exec 3<>"$STDIN_FIFO"
ACPX_HTTP_BIND="127.0.0.1:${PORT}" \
  ACPX_BACKEND_CMD="sh ${BACKEND_SCRIPT}" \
  target/debug/acpx-server <&3 &
SERVER_PID=$!

echo "==> Waiting for server to accept connections"
READY=""
for _ in $(seq 1 100); do
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "FAIL: acpx-server exited before it started accepting connections" >&2
    exit 1
  fi
  if python3 -c "
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.settimeout(0.2)
try:
    s.connect(('127.0.0.1', ${PORT}))
except OSError:
    sys.exit(1)
else:
    s.close()
    sys.exit(0)
" 2>/dev/null; then
    READY="1"
    break
  fi
  sleep 0.1
done

if [[ -z "$READY" ]]; then
  echo "FAIL: acpx-server never started accepting connections on 127.0.0.1:${PORT} within ~10s" >&2
  exit 1
fi
echo "==> Server is up"

echo "==> Running acpx-selftest against http://127.0.0.1:${PORT}"
SELFTEST_EXIT=0
target/debug/acpx-selftest --target "http://127.0.0.1:${PORT}" || SELFTEST_EXIT=$?

if [[ "$SELFTEST_EXIT" -eq 0 ]]; then
  echo "PASS: acpx-selftest succeeded against a live acpx-server"
else
  echo "FAIL: acpx-selftest exited with status ${SELFTEST_EXIT}" >&2
fi

exit "$SELFTEST_EXIT"
