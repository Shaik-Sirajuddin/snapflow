#!/usr/bin/env bash
# Host-side driver for the desktop-stack scenario: real Qt6/X11/Mesa
# runtime libraries present (as any real desktop Linux machine has),
# confirming the installed `snapflow` GUI editor genuinely launches
# headlessly (QT_QPA_PLATFORM=offscreen) -- not just that the symlink
# exists, which the plain scenario already checks.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
ARTIFACT_DIR="$SCRIPT_DIR/artifact"

if ! find "$ARTIFACT_DIR" -maxdepth 1 -iname 'snapflow-linux-*.tar.gz' ! -iname '*upgrade-test*' -print -quit 2>/dev/null | grep -q .; then
  echo "error: no snapflow-linux-*.tar.gz in $ARTIFACT_DIR" >&2
  exit 1
fi

IMAGE_TAG="snapflow-install-desktop-test:local"

echo "==> Building desktop-stack test image ($IMAGE_TAG)..."
docker build -t "$IMAGE_TAG" -f "$SCRIPT_DIR/Dockerfile.desktop" "$REPO_ROOT"

echo "==> Running test container..."
set +e
docker run --rm \
  -v "$ARTIFACT_DIR:/artifact:ro" \
  "$IMAGE_TAG"
STATUS=$?
set -e

if [ "$STATUS" -eq 0 ]; then
  echo "==> install-desktop test suite: PASS"
else
  echo "==> install-desktop test suite: FAIL ($STATUS failure(s))" >&2
fi
exit "$STATUS"
