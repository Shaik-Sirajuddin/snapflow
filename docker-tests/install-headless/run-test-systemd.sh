#!/usr/bin/env bash
# Host-side driver for the systemd-service scenario: real systemd as PID 1,
# real `systemctl --user enable --now snapflowd` via install.sh's actual
# setup_linux_service path (SNAPFLOW_SKIP_SERVICE is NOT set here, unlike
# the plain headless scenario).
#
# Needs --privileged + cgroup passthrough for systemd itself to work in
# Docker -- that's the fragile, documented-here part. Test steps run via
# `docker exec` since this image's entrypoint is systemd, not a script.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
ARTIFACT_DIR="$SCRIPT_DIR/artifact"

if ! find "$ARTIFACT_DIR" -maxdepth 1 -iname 'snapflow-linux-*.tar.gz' -print -quit 2>/dev/null | grep -q .; then
  echo "error: no snapflow-linux-*.tar.gz in $ARTIFACT_DIR" >&2
  exit 1
fi
# Container-side path (artifact dir is bind-mounted to /artifact below) --
# NOT the host path found on disk here, which doesn't exist inside the
# container's filesystem.
ARTIFACT_BASENAME="$(find "$ARTIFACT_DIR" -maxdepth 1 -iname 'snapflow-linux-*.tar.gz' ! -iname '*upgrade-test*' -printf '%f\n' | head -n1)"
ARTIFACT="/artifact/$ARTIFACT_BASENAME"

IMAGE_TAG="snapflow-install-systemd-test:local"
CONTAINER_NAME="snapflow-install-systemd-test-run"

echo "==> Building systemd test image ($IMAGE_TAG)..."
docker build -t "$IMAGE_TAG" -f "$SCRIPT_DIR/Dockerfile.systemd" "$REPO_ROOT"

docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
cleanup() { docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo "==> Starting systemd container..."
docker run -d --name "$CONTAINER_NAME" \
  --privileged \
  --cgroupns=host \
  -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
  -v "$ARTIFACT_DIR:/artifact:ro" \
  "$IMAGE_TAG" >/dev/null

FAILURES=0
fail() { echo "FAIL: $*" >&2; FAILURES=$((FAILURES + 1)); }
pass() { echo "PASS: $*"; }
dexec() { docker exec "$CONTAINER_NAME" "$@"; }
dexec_tester() { docker exec -u tester "$CONTAINER_NAME" "$@"; }

echo "==> Waiting for systemd to reach a running state..."
ready=0
for i in $(seq 1 30); do
  state="$(dexec systemctl is-system-running 2>/dev/null || true)"
  if [ "$state" = "running" ] || [ "$state" = "degraded" ]; then
    ready=1
    break
  fi
  sleep 1
done
if [ "$ready" = "1" ]; then
  pass "systemd reached state '$state' within 30s"
else
  fail "systemd never reached running/degraded within 30s (last state: '$state')"
  echo "==> journalctl -xb for diagnosis:"
  dexec journalctl -xb --no-pager | tail -50 || true
  exit "$FAILURES"
fi

# systemctl --user needs a real logind session; lingering + a manual
# `loginctl enable-linger` gives the user manager a reason to start
# without an actual interactive login.
#
# tester's UID is NOT assumed to be 1000 -- ubuntu:24.04 may already have
# UID 1000 taken (it does: useradd gave tester UID 1001 in practice), so
# resolve it for real rather than hardcoding, which silently broke every
# XDG_RUNTIME_DIR/bus-path reference below in an earlier version of this
# script.
TESTER_UID="$(dexec id -u tester)"
RUNTIME_DIR="/run/user/$TESTER_UID"
echo "==> tester UID resolved to $TESTER_UID (runtime dir: $RUNTIME_DIR)"

echo "==> Enabling lingering for tester..."
dexec loginctl enable-linger tester
# Give the user manager a moment to actually start after enabling linger.
uid_ready=0
for i in $(seq 1 15); do
  if dexec test -S "$RUNTIME_DIR/bus" 2>/dev/null; then
    uid_ready=1
    break
  fi
  sleep 1
done
if [ "$uid_ready" = "1" ]; then
  pass "tester's user D-Bus session is up ($RUNTIME_DIR/bus)"
else
  fail "tester's user D-Bus session never came up within 15s"
fi

echo "==> Running install.sh as tester (real setup_linux_service path, no SNAPFLOW_SKIP_SERVICE)..."
INSTALL_LOG="$(mktemp)"
if docker exec -u tester -e "SNAPFLOW_ASSET_URL=file://$ARTIFACT" -e "XDG_RUNTIME_DIR=$RUNTIME_DIR" -e "DBUS_SESSION_BUS_ADDRESS=unix:path=$RUNTIME_DIR/bus" \
    "$CONTAINER_NAME" bash /home/tester/install.sh > "$INSTALL_LOG" 2>&1; then
  pass "install.sh (with real service setup) exited 0"
else
  fail "install.sh (with real service setup) exited non-zero"
fi
cat "$INSTALL_LOG"

echo "==> Checking systemctl --user status for snapflowd..."
sc_status=0
for i in $(seq 1 15); do
  if docker exec -u tester -e "XDG_RUNTIME_DIR=$RUNTIME_DIR" "$CONTAINER_NAME" \
      systemctl --user is-active snapflowd 2>/dev/null | grep -q "^active$"; then
    sc_status=1
    break
  fi
  sleep 1
done

if [ "$sc_status" = "1" ]; then
  pass "systemctl --user is-active snapflowd reports 'active'"
else
  fail "systemctl --user is-active snapflowd never reported 'active' within 15s"
fi

status_out="$(docker exec -u tester -e "XDG_RUNTIME_DIR=$RUNTIME_DIR" "$CONTAINER_NAME" systemctl --user status snapflowd --no-pager 2>&1 || true)"
echo "==> systemctl --user status snapflowd:"
echo "$status_out"
if echo "$status_out" | grep -qE "Main PID: [0-9]+ \(snapflowd\)"; then
  pass "systemctl --user status shows a real running Main PID for snapflowd"
else
  fail "systemctl --user status did not show a running Main PID for snapflowd"
fi

echo "==> Checking journal for real readiness log lines..."
journal_out="$(docker exec -u tester -e "XDG_RUNTIME_DIR=$RUNTIME_DIR" "$CONTAINER_NAME" journalctl --user -u snapflowd --no-pager 2>&1 || true)"
echo "$journal_out"
if echo "$journal_out" | grep -q "SDP control socket listening" && echo "$journal_out" | grep -q "MCP SSE endpoint listening"; then
  pass "journalctl --user -u snapflowd shows both real readiness log lines"
else
  fail "journalctl --user -u snapflowd missing one or both readiness log lines"
fi

echo "==> Summary: $FAILURES failure(s)"
if [ "$FAILURES" -eq 0 ]; then
  echo "==> install-systemd test suite: PASS"
else
  echo "==> install-systemd test suite: FAIL" >&2
fi
exit "$FAILURES"
