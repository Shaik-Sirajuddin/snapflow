#!/usr/bin/env bash
#
# Manual interactive dev harness -- NOT part of the automated test suite.
# Starts a real acpx-server (real rui-mock-agent backend) and a real
# Shotcut process under Xvfb, then binds x11vnc to that display so a
# human can connect a VNC client and click around the real embedded
# ChatRustDock directly. Mirrors host_e2e_smoke.sh's process wiring
# (fifo-kept-open stdio transport, health-check wait, same env vars)
# but has no XTEST driver and no cleanup trap -- it stays up until you
# kill it (see the printed pids.env path, or host_vnc_dev_stop.sh).
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
state_dir="${PANEL_VNC_DEV_STATE_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/panel-vnc-dev.XXXXXX")}"
display="${PANEL_VNC_DEV_DISPLAY:-:110}"
screen="${PANEL_VNC_DEV_SCREEN:-1280x800x24}"
gateway_port="${PANEL_VNC_DEV_GATEWAY_PORT:-18791}"
vnc_port="${PANEL_VNC_DEV_VNC_PORT:-5910}"
dock_width="${PANEL_VNC_DEV_DOCK_WIDTH:-320}"

server_bin="${ACPX_SERVER_BIN:-$repo_root/acpx/target/debug/acpx-server}"
agent_bin="${RUI_MOCK_AGENT_BIN:-$repo_root/panel-rust/target/debug/rui-mock-agent}"
shotcut_bin="${SHOTCUT_BIN:-$repo_root/shotcut/build/src/shotcut}"

for binary in "$server_bin" "$agent_bin" "$shotcut_bin" Xvfb x11vnc curl xdpyinfo; do
    if ! command -v "$binary" >/dev/null 2>&1 && [[ ! -x "$binary" ]]; then
        printf 'required executable is unavailable: %s\n' "$binary" >&2
        exit 1
    fi
done

mkdir -p "$state_dir"/{acpx,panel,shotcut}
fifo="$state_dir/acpx/stdin.fifo"
mkfifo "$fifo"
# Keep both ends open in this shell -- acpx-server's stdio transport must
# not see EOF while its HTTP/WS transport is serving the embedded panel.
exec 3<>"$fifo"

Xvfb "$display" -screen 0 "$screen" -nolisten tcp >"$state_dir/xvfb.log" 2>&1 &
xvfb_pid="$!"
export DISPLAY="$display"

for _ in $(seq 1 80); do
    if xdpyinfo -display "$display" >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done
xdpyinfo -display "$display" >/dev/null

x11vnc -display "$display" -rfbport "$vnc_port" -forever -shared -nopw \
    -bg -o "$state_dir/x11vnc.log"
vnc_pid="$(pgrep -f "x11vnc -display $display -rfbport $vnc_port" | head -1)"

ACPX_HTTP_BIND="127.0.0.1:$gateway_port" \
ACPX_BACKEND_CMD="$agent_bin" \
ACPX_DEFAULT_AGENT_ID="codex" \
ACPX_DB_PATH="$state_dir/acpx/gateway.sqlite3" \
RUI_MOCK_AGENT_EVENT_LOG="$state_dir/acpx/backend-events.jsonl" \
"$server_bin" <"$fifo" >"$state_dir/acpx/server.stdout.log" 2>"$state_dir/acpx/server.stderr.log" &
server_pid="$!"

for _ in $(seq 1 80); do
    if curl --fail --silent "http://127.0.0.1:$gateway_port/health" >/dev/null; then
        break
    fi
    sleep 0.1
done
curl --fail --silent "http://127.0.0.1:$gateway_port/health" >/dev/null

QSG_RENDER_LOOP=basic \
RUI_PANEL_INPUT_TRACE=1 \
PANEL_HOST_E2E_DOCK_WIDTH="$dock_width" \
RUI_ACP_CACHE_DIR="$state_dir/panel" \
RUI_ACPX_CODEX_URL="http://127.0.0.1:$gateway_port" \
RUI_ACPX_CLAUDE_URL="http://127.0.0.1:$gateway_port" \
"$shotcut_bin" --appdata "$state_dir/shotcut" --noupgrade \
    >"$state_dir/shotcut.stdout.log" \
    2>"$state_dir/shotcut.stderr.log" &
shotcut_pid="$!"

cat > "$state_dir/pids.env" <<EOF
xvfb_pid=$xvfb_pid
vnc_pid=$vnc_pid
server_pid=$server_pid
shotcut_pid=$shotcut_pid
display=$display
gateway_port=$gateway_port
vnc_port=$vnc_port
state_dir=$state_dir
EOF

printf '\n=== panel VNC dev harness is up ===\n'
printf 'state_dir : %s\n' "$state_dir"
printf 'display   : %s\n' "$display"
printf 'gateway   : http://127.0.0.1:%s (health ok)\n' "$gateway_port"
printf 'vnc       : localhost:%s  (raw VNC, no password -- vncviewer localhost:%s)\n' "$vnc_port" "$vnc_port"
printf 'pids      : xvfb=%s vnc=%s acpx-server=%s shotcut=%s\n' "$xvfb_pid" "$vnc_pid" "$server_pid" "$shotcut_pid"
printf 'stop with : kill %s %s %s %s\n' "$shotcut_pid" "$server_pid" "$vnc_pid" "$xvfb_pid"
printf 'logs      : %s/shotcut.stderr.log , %s/acpx/server.stderr.log\n' "$state_dir" "$state_dir"
printf '=====================================\n\n'

sleep "${PANEL_VNC_DEV_SETTLE_SECONDS:-5}"
if ! kill -0 "$shotcut_pid" 2>/dev/null; then
    printf 'Shotcut exited early -- see %s/shotcut.stderr.log\n' "$state_dir" >&2
    exit 1
fi
printf 'Shotcut is alive at %s. Harness stays resident to keep fd3/acpx-server stdio open;\n' "$(date +%T)"
printf 'this shell will now idle. Ctrl-C or kill the pids above to stop.\n'
# Keep this shell (and its held-open fifo fd 3) alive indefinitely so the
# gateway's stdio transport never sees EOF while you interact over VNC.
wait
