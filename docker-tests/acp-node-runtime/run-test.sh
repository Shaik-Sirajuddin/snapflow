#!/usr/bin/env bash
# Host driver for ACP Node runtime Docker matrix (M1–M10).
# Usage (from repo / worktree root):
#   docker-tests/acp-node-runtime/run-test.sh
#
# Builds acpx-server if needed, then runs the matrix image.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
IMAGE_TAG="snapflow-acp-node-runtime-test:local"

ACPX_BIN="${ACPX_SERVER_BIN:-}"
if [ -z "$ACPX_BIN" ]; then
  for c in \
    "$REPO_ROOT/acpx/target/debug/acpx-server" \
    "$REPO_ROOT/acpx/target/release/acpx-server"
  do
    if [ -x "$c" ]; then
      ACPX_BIN="$c"
      break
    fi
  done
fi

if [ -z "${ACPX_BIN:-}" ] || [ ! -x "$ACPX_BIN" ]; then
  echo "==> Building acpx-server (debug)..."
  (cd "$REPO_ROOT/acpx" && cargo build -p acpx-server)
  ACPX_BIN="$REPO_ROOT/acpx/target/debug/acpx-server"
fi

echo "==> Using acpx-server: $ACPX_BIN"
echo "==> Building image $IMAGE_TAG..."
docker build -t "$IMAGE_TAG" -f "$SCRIPT_DIR/Dockerfile" "$REPO_ROOT"

# Optional: if host has system node, mount it for true M7/M9 global path.
HOST_NODE_MOUNT=()
if command -v node >/dev/null 2>&1; then
  HOST_NODE="$(command -v node)"
  HOST_PREFIX="$(cd "$(dirname "$HOST_NODE")/.." && pwd)"
  if [ -x "$HOST_PREFIX/bin/npm" ]; then
    echo "==> Mounting host node prefix for M7/M9: $HOST_PREFIX"
    HOST_NODE_MOUNT=(-v "$HOST_PREFIX:/host-global-node:ro")
  fi
fi

echo "==> Running matrix container..."
set +e
docker run --rm \
  -v "$ACPX_BIN:/acpx-server:ro" \
  "${HOST_NODE_MOUNT[@]}" \
  -e ACPX_SERVER_BIN=/acpx-server \
  "$IMAGE_TAG"
STATUS=$?
set -e

if [ "$STATUS" -eq 0 ]; then
  echo "==> acp-node-runtime matrix: PASS"
else
  echo "==> acp-node-runtime matrix: FAIL ($STATUS)" >&2
fi
exit "$STATUS"
