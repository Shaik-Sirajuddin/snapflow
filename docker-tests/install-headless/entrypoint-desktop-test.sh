#!/usr/bin/env bash
# Runs inside the desktop-stack test container as 'tester'. Confirms the
# real, installed `snapflow` GUI editor binary can actually be launched
# (offscreen QPA, no real display needed) -- not just that the symlink
# exists and is executable, which the plain scenario already checks.
set -uo pipefail

FAILURES=0
fail() { echo "FAIL: $*" >&2; FAILURES=$((FAILURES + 1)); }
pass() { echo "PASS: $*"; }

ARTIFACT="$(find /artifact -maxdepth 1 -iname 'snapflow-linux-*.tar.gz' ! -iname '*upgrade-test*' | head -n1)"
if [ -z "$ARTIFACT" ]; then
  fail "no snapflow-linux-*.tar.gz found under /artifact"
  exit 1
fi

export SNAPFLOW_ASSET_URL="file://$ARTIFACT"
export SNAPFLOW_SKIP_SERVICE=1

echo "==> Running install.sh..."
if bash /home/tester/install.sh; then
  pass "install.sh exited 0"
else
  fail "install.sh exited non-zero"
fi

BIN_DIR="$HOME/.local/bin"
LINK_TARGET="$(readlink -f "$BIN_DIR/snapflow" 2>/dev/null || true)"
if [[ "$LINK_TARGET" == */Snapflow.app/snapflow ]]; then
  pass "snapflow symlink resolves to the wrapper script, not the raw binary: $LINK_TARGET"
else
  fail "snapflow symlink does not resolve to the expected wrapper script (got: $LINK_TARGET)"
fi

echo "==> Launching snapflow --version with QT_QPA_PLATFORM=offscreen..."
OUT="$(mktemp)"
timeout 20 env QT_QPA_PLATFORM=offscreen "$BIN_DIR/snapflow" --version >"$OUT" 2>&1
STATUS=$?
echo "==> snapflow --version output (exit $STATUS):"
cat "$OUT"

if [ "$STATUS" -eq 0 ]; then
  pass "snapflow --version exited 0 -- the GUI editor binary genuinely launches"
else
  fail "snapflow --version exited $STATUS"
fi

if grep -qi "error while loading shared libraries" "$OUT"; then
  fail "still hitting a missing shared library: $(grep -i 'error while loading' "$OUT")"
fi

# ── Phase: full headless GUI process launch (not just --version) ───────
# Proves the actual editor app comes up and stays running headlessly,
# same spirit as the daemon's headless_daemon_launch_check.
echo "==> Launching full snapflow GUI process headlessly (QT_QPA_PLATFORM=offscreen)..."
GUI_LOG="$(mktemp)"
QT_QPA_PLATFORM=offscreen setsid "$BIN_DIR/snapflow" >"$GUI_LOG" 2>&1 < /dev/null &
GUI_PID=$!
disown
sleep 4

if kill -0 "$GUI_PID" 2>/dev/null; then
  pass "snapflow GUI process is alive 4s after launch (PID $GUI_PID)"
else
  fail "snapflow GUI process exited within 4s of launch"
fi

if pgrep -f "Snapflow.app/bin/snapflow" >/dev/null 2>&1; then
  pass "pgrep finds the real Snapflow.app/bin/snapflow process: $(pgrep -af 'Snapflow.app/bin/snapflow')"
else
  fail "pgrep found no Snapflow.app/bin/snapflow process"
fi

echo "==> GUI process log:"
cat "$GUI_LOG"

kill -TERM "$GUI_PID" 2>/dev/null || true
term_ok=0
for i in $(seq 1 10); do
  if ! kill -0 "$GUI_PID" 2>/dev/null; then
    term_ok=1
    break
  fi
  sleep 1
done
if [ "$term_ok" = "1" ]; then
  pass "snapflow GUI process exited cleanly after SIGTERM within 10s"
else
  fail "snapflow GUI process still running 10s after SIGTERM"
  kill -9 "$GUI_PID" 2>/dev/null || true
fi

echo "==> Summary: $FAILURES failure(s)"
exit "$FAILURES"
