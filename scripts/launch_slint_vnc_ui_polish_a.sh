#!/usr/bin/env bash
set -euo pipefail

WORKTREE="${1:-/home/siraj/Desktop/codebases/prv/multimedia_agent/multi_media_main/.claude/worktrees/slint-hot-reload}"
DISPLAY_NUM=":111"
VNC_PORT="5911"
STATE_DIR="/tmp/slint-dev-viewer"

mkdir -p "$STATE_DIR"

# Clean up old processes on display :111 and port 5911
pkill -9 -f "Xvfb $DISPLAY_NUM" 2>/dev/null || true
pkill -9 -f "x11vnc -display $DISPLAY_NUM" 2>/dev/null || true
pkill -9 -f "slint-viewer" 2>/dev/null || true
sleep 1

# Start Xvfb
Xvfb "$DISPLAY_NUM" -screen 0 1280x900x24 -nolisten tcp > "$STATE_DIR/xvfb.log" 2>&1 &
xvfb_pid=$!

export DISPLAY="$DISPLAY_NUM"

# Wait for Xvfb
for _ in $(seq 1 50); do
    if xdpyinfo -display "$DISPLAY_NUM" >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done
xdpyinfo -display "$DISPLAY_NUM" >/dev/null

# Start x11vnc
x11vnc -display "$DISPLAY_NUM" -rfbport "$VNC_PORT" -forever -shared -nopw -bg -o "$STATE_DIR/x11vnc.log"

# Start fluxbox window manager
fluxbox > "$STATE_DIR/fluxbox.log" 2>&1 &

# Prepare dev_root.slint template
sed "s#__CHATPANEL_SLINT__#$WORKTREE/panel-rust/ui/app.slint#" \
  "$WORKTREE/panel-rust/tests/dev_root.slint.template" \
  > "$STATE_DIR/dev_root.slint"

# Launch slint-viewer with auto-reload
LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=softpipe /home/siraj/.cargo/bin/slint-viewer --auto-reload "$STATE_DIR/dev_root.slint" > "$STATE_DIR/slint-viewer.log" 2>&1 &
viewer_pid=$!

sleep 2

printf '\n=== Slint Hot Reload VNC Viewer is Live ===\n'
printf 'Worktree   : %s\n' "$WORKTREE"
printf 'Display    : %s\n' "$DISPLAY_NUM"
printf 'VNC        : localhost:%s (raw VNC, no password)\n' "$VNC_PORT"
printf 'Slint File : %s/panel-rust/ui/app.slint\n' "$WORKTREE"
printf 'Viewer PID : %s\n' "$viewer_pid"
printf '============================================\n\n'

# Tail log to keep background task active
tail -f "$STATE_DIR/slint-viewer.log"
