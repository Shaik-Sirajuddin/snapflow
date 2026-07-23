#!/usr/bin/env bash
# Host-side driver for the install.sh headless-install Docker test.
# Usage: docker-tests/install-headless/run-test.sh
#
# Requires an artifact already downloaded into
# docker-tests/install-headless/artifact/ (see 00-plan.md phase
# fetch_ci_artifact -- `gh run download <run-id> --name
# snapflow-linux-install-bundle -d docker-tests/install-headless/artifact`).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
ARTIFACT_DIR="$SCRIPT_DIR/artifact"

if ! find "$ARTIFACT_DIR" -maxdepth 1 -iname 'snapflow-linux-*.tar.gz' -print -quit 2>/dev/null | grep -q .; then
  echo "error: no snapflow-linux-*.tar.gz in $ARTIFACT_DIR" >&2
  echo "  fetch one first, e.g.:" >&2
  echo "  gh run download <run-id> --repo Shaik-Sirajuddin/snapflow --name snapflow-linux-install-bundle -D $ARTIFACT_DIR" >&2
  exit 1
fi

IMAGE_TAG="snapflow-install-headless-test:local"

echo "==> Building test image ($IMAGE_TAG)..."
docker build -t "$IMAGE_TAG" -f "$SCRIPT_DIR/Dockerfile" "$REPO_ROOT"

echo "==> Running test container..."
set +e
docker run --rm \
  -v "$ARTIFACT_DIR:/artifact:ro" \
  "$IMAGE_TAG"
STATUS=$?
set -e

if [ "$STATUS" -eq 0 ]; then
  echo "==> install-headless test suite: PASS"
else
  echo "==> install-headless test suite: FAIL ($STATUS failure(s) -- see PASS/FAIL lines above)" >&2
fi
exit "$STATUS"
