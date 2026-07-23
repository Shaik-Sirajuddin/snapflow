#!/usr/bin/env bash
# Runs inside the test container as the unprivileged 'tester' user.
# Exercises the real scripts/install.sh flow (same code path a real
# `curl | bash` user hits), then confirms snapflowd genuinely comes up and
# stays running headlessly -- no display, no desktop session, no systemd
# in this minimal scenario (SNAPFLOW_SKIP_SERVICE=1).
set -uo pipefail

FAILURES=0
fail() { echo "FAIL: $*" >&2; FAILURES=$((FAILURES + 1)); }
pass() { echo "PASS: $*"; }

ARTIFACT="$(find /artifact -maxdepth 1 -iname 'snapflow-linux-*.tar.gz' | head -n1)"
if [ -z "$ARTIFACT" ]; then
  fail "no snapflow-linux-*.tar.gz found under /artifact (bind-mount missing?)"
  exit 1
fi
echo "==> Using artifact: $ARTIFACT"

# ── Phase: real install.sh flow ────────────────────────────────────────
# SNAPFLOW_ASSET_URL is install.sh's own documented escape hatch for
# testing against a non-published build -- points it at the real CI
# artifact via a local file:// URL instead of the GitHub Releases API.
export SNAPFLOW_ASSET_URL="file://$ARTIFACT"
export SNAPFLOW_SKIP_SERVICE=1  # no systemd in this minimal scenario

echo "==> Running install.sh (SNAPFLOW_ASSET_URL=$SNAPFLOW_ASSET_URL)"
if bash /home/tester/install.sh; then
  pass "install.sh exited 0"
else
  fail "install.sh exited non-zero ($?)"
fi

BIN_DIR="$HOME/.local/bin"

# ── Phase: install-flow assertions ─────────────────────────────────────
if [ -x "$BIN_DIR/snapflowd" ]; then
  pass "snapflowd binary present and executable at $BIN_DIR/snapflowd"
else
  fail "snapflowd binary missing or not executable at $BIN_DIR/snapflowd"
fi

if [ -x "$BIN_DIR/snapflow" ]; then
  pass "snapflow (editor) binary present and executable at $BIN_DIR/snapflow"
else
  fail "snapflow (editor) binary missing or not executable at $BIN_DIR/snapflow"
fi

VERSION_FILE="$HOME/.local/share/snapflow/.snapflow-version"
if [ -s "$VERSION_FILE" ]; then
  pass "version stamp written: $(cat "$VERSION_FILE")"
else
  fail "version stamp file missing or empty: $VERSION_FILE"
fi

# ── Phase: headless daemon launch check ─────────────────────────────────
# Real evidence, not "no error was printed": snapflowd serve (per
# snapshotd/cmd/snapshotd/main.go cmdServe) logs "SDP control socket
# listening" and "MCP SSE endpoint listening" via slog to stderr, and
# writes a real pidfile at $SNAPSHOTD_HOME/control.sock.pid (default
# SNAPSHOTD_HOME is ~/.snapshotd). Poll for these instead of a fixed sleep.
LOG_FILE="$(mktemp)"
export PATH="$BIN_DIR:$PATH"
echo "==> Launching snapflowd serve headlessly..."
setsid "$BIN_DIR/snapflowd" serve >"$LOG_FILE" 2>&1 < /dev/null &
SNAPFLOWD_PID=$!
disown

SNAPSHOTD_HOME="${SNAPSHOTD_HOME:-$HOME/.snapshotd}"
PIDFILE="$SNAPSHOTD_HOME/control.sock.pid"

ready=0
for i in $(seq 1 20); do
  if grep -q "SDP control socket listening" "$LOG_FILE" 2>/dev/null && \
     grep -q "MCP SSE endpoint listening" "$LOG_FILE" 2>/dev/null; then
    ready=1
    break
  fi
  if ! kill -0 "$SNAPFLOWD_PID" 2>/dev/null; then
    fail "snapflowd process exited early (after ${i}s) -- see log below"
    break
  fi
  sleep 1
done

echo "==> snapflowd log output:"
cat "$LOG_FILE"

if [ "$ready" = "1" ]; then
  pass "snapflowd logged both readiness lines (SDP control socket + MCP SSE endpoint)"
else
  fail "snapflowd did not log both readiness lines within 20s"
fi

if kill -0 "$SNAPFLOWD_PID" 2>/dev/null; then
  pass "snapflowd process is alive (PID $SNAPFLOWD_PID via kill -0)"
else
  fail "snapflowd process (PID $SNAPFLOWD_PID) is not alive"
fi

if pgrep -f "snapflowd serve" >/dev/null 2>&1; then
  pgrep_out="$(pgrep -af "snapflowd serve")"
  pass "pgrep -f 'snapflowd serve' found a running process: $pgrep_out"
else
  fail "pgrep -f 'snapflowd serve' found no running process"
fi

if [ -f "$PIDFILE" ]; then
  written_pid="$(cat "$PIDFILE" | tr -d '[:space:]')"
  if [ "$written_pid" = "$SNAPFLOWD_PID" ]; then
    pass "pidfile $PIDFILE contains the correct PID ($written_pid)"
  else
    fail "pidfile $PIDFILE contains PID $written_pid, expected $SNAPFLOWD_PID"
  fi
else
  fail "pidfile not found at $PIDFILE"
fi

# Clean shutdown check: SIGTERM should stop it (Restart=on-failure in the
# real systemd unit implies graceful SIGTERM handling matters).
kill -TERM "$SNAPFLOWD_PID" 2>/dev/null || true
term_ok=0
for i in $(seq 1 10); do
  if ! kill -0 "$SNAPFLOWD_PID" 2>/dev/null; then
    term_ok=1
    break
  fi
  sleep 1
done
if [ "$term_ok" = "1" ]; then
  pass "snapflowd exited cleanly after SIGTERM within 10s"
else
  fail "snapflowd still running 10s after SIGTERM"
  kill -9 "$SNAPFLOWD_PID" 2>/dev/null || true
fi

# ── Phase: upgrade scenario -- backup + user-data preservation ─────────
# Proves two things for real, not just by reading the code: (1) install.sh
# now backs up the previous bundle before overwriting it on upgrade, and
# (2) upgrading never touches real user/daemon data (SNAPSHOTD_HOME is a
# separate tree install.sh doesn't manage at all).
UPGRADE_ARTIFACT="$(find /artifact -maxdepth 1 -iname '*upgrade-test-v2*.tar.gz' | head -n1)"
if [ -z "$UPGRADE_ARTIFACT" ]; then
  fail "no upgrade-test-v2 artifact found under /artifact -- skipping upgrade scenario"
else
  MARKER="$SNAPSHOTD_HOME/USER_DATA_MARKER"
  mkdir -p "$SNAPSHOTD_HOME"
  echo "this-represents-real-user-project-data-$(date +%s)" > "$MARKER"
  marker_before="$(cat "$MARKER")"
  echo "==> Planted user-data marker: $marker_before"

  INSTALL_DIR="$HOME/.local/share/snapflow"
  OLD_BIN_CHECKSUM="$(sha256sum "$INSTALL_DIR/bin/snapflowd" 2>/dev/null | cut -d' ' -f1)"

  export SNAPFLOW_ASSET_URL="file://$UPGRADE_ARTIFACT"
  echo "==> Running install.sh again as an upgrade (SNAPFLOW_ASSET_URL=$SNAPFLOW_ASSET_URL)"
  if bash /home/tester/install.sh; then
    pass "upgrade install.sh run exited 0"
  else
    fail "upgrade install.sh run exited non-zero ($?)"
  fi

  if [ -x "$BIN_DIR/snapflowd" ] && [ -x "$BIN_DIR/snapflow" ]; then
    pass "post-upgrade: both binaries still present and executable"
  else
    fail "post-upgrade: snapflowd/snapflow binary missing or not executable"
  fi

  if [ -d "$INSTALL_DIR.prev" ] && [ -x "$INSTALL_DIR.prev/bin/snapflowd" ]; then
    prev_checksum="$(sha256sum "$INSTALL_DIR.prev/bin/snapflowd" | cut -d' ' -f1)"
    if [ -n "$OLD_BIN_CHECKSUM" ] && [ "$prev_checksum" = "$OLD_BIN_CHECKSUM" ]; then
      pass "$INSTALL_DIR.prev exists and contains the real previous bundle (checksum matches pre-upgrade binary)"
    else
      fail "$INSTALL_DIR.prev exists but its snapflowd checksum ($prev_checksum) doesn't match the pre-upgrade one ($OLD_BIN_CHECKSUM)"
    fi
  else
    fail "$INSTALL_DIR.prev missing or doesn't contain a valid backed-up bundle after upgrade"
  fi

  if [ -f "$MARKER" ]; then
    marker_after="$(cat "$MARKER")"
    if [ "$marker_after" = "$marker_before" ]; then
      pass "user-data marker under \$SNAPSHOTD_HOME survived the upgrade unchanged: $marker_after"
    else
      fail "user-data marker changed across upgrade: before='$marker_before' after='$marker_after'"
    fi
  else
    fail "user-data marker under \$SNAPSHOTD_HOME was deleted by the upgrade"
  fi
fi

echo "==> Summary: $FAILURES failure(s)"
exit "$FAILURES"
