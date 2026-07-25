#!/usr/bin/env bash
#
# MCP-driven host smoke harness -- companion to host_e2e_smoke.sh, not a
# replacement. Same real-process wiring (real Shotcut, real acpx-server,
# compiled mock ACP backend, one temp state dir), but interactions are
# driven through i_slint_backend_testing::mcp_server (SLINT_MCP_PORT) via
# host_e2e_mcp_driver.py instead of XTEST + dock-relative pixel math.
# Element lookups are by qualified id / accessible label, so this harness
# does not need PANEL_HOST_E2E_DOCK_WIDTH at all.
#
# Own display/port/state-dir defaults (:112, 18796/19099) so a run never
# collides with a concurrent host_e2e_smoke.sh (:109, 18790) or
# host_vnc_dev.sh (:110, 18791) run -- see memory/team/testing's own
# "never hand-roll a port, never conflict with an existing instance"
# convention.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
state_dir="${PANEL_HOST_E2E_MCP_STATE_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/panel-host-e2e-mcp.XXXXXX")}"
keep_state="${PANEL_HOST_E2E_MCP_KEEP_STATE:-0}"
display="${PANEL_HOST_E2E_MCP_DISPLAY:-:112}"
screen="${PANEL_HOST_E2E_MCP_SCREEN:-1280x800x24}"
gateway_port="${PANEL_HOST_E2E_MCP_GATEWAY_PORT:-18796}"
admin_port="${PANEL_HOST_E2E_MCP_ADMIN_PORT:-18797}"
admin_token="panel-host-e2e-mcp-admin-token-$$"
mcp_port="${PANEL_HOST_E2E_MCP_PORT:-19099}"
scenario="${1:?usage: host_e2e_mcp_smoke.sh <send-now|rename>}"

server_bin="${ACPX_SERVER_BIN:-$repo_root/acpx/target/debug/acpx-server}"
agent_bin="${RUI_MOCK_AGENT_BIN:-$repo_root/panel-rust/target/debug/rui-mock-agent}"
shotcut_bin="${SHOTCUT_BIN:-$repo_root/shotcut/build/src/shotcut}"

for binary in "$server_bin" "$agent_bin" "$shotcut_bin" Xvfb curl python3; do
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

server_pid=""
xvfb_pid=""
shotcut_pid=""
cleanup() {
    for pid in "$shotcut_pid" "$xvfb_pid" "$server_pid"; do
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    if [[ "$keep_state" != "1" ]]; then
        rm -rf "$state_dir"
    else
        printf 'host E2E MCP state retained at %s\n' "$state_dir"
    fi
}
trap cleanup EXIT INT TERM

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

ACPX_HTTP_BIND="127.0.0.1:$gateway_port" \
ACPX_DEFAULT_AGENT_ID="codex" \
ACPX_DB_PATH="$state_dir/acpx/gateway.sqlite3" \
ACPX_ADMIN_TOKEN="$admin_token" \
ACPX_ADMIN_BIND="127.0.0.1:$admin_port" \
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

# PROF-4 (profile-only-backend-selection plan): see host_e2e_smoke.sh's own
# comment on this same block for the full rationale -- registers
# rui-mock-agent as a durable admin-plane custom agent under "mock-codex"
# (deliberately not "codex" -- acpx-server's own main.rs unconditionally
# pre-registers a supervisor entry under ACPX_DEFAULT_AGENT_ID at startup,
# so reusing that id here would 409 with a "conflicts with an existing
# registered backend" error), then a profile literally named "codex"
# pointing at it, then a settings.global.json setting default_agent_id so
# the panel's own cold-start seed binds that profile as its _acpx.profile
# instead of ever touching native mode.
for _ in $(seq 1 80); do
    if curl --fail --silent -o /dev/null "http://127.0.0.1:$admin_port/admin/agents" \
        -H "Authorization: Bearer $admin_token"; then
        break
    fi
    sleep 0.1
done
curl --fail --silent -X POST "http://127.0.0.1:$admin_port/admin/agents/custom" \
    -H "Authorization: Bearer $admin_token" \
    -H "Content-Type: application/json" \
    -d "$(printf '{"id":"mock-codex","name":"mock-codex","command":"%s","args":[],"env":{"RUI_MOCK_AGENT_PERSONA":"codex"},"cwd":null}' "$agent_bin")" \
    >/dev/null
curl --fail --silent -X POST "http://127.0.0.1:$gateway_port/rpc" \
    -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","id":1,"method":"profiles/create","params":{"name":"codex","agent_id":"mock-codex"}}' \
    >/dev/null

mkdir -p "$state_dir/panel-settings"
printf '{"schema_version":1,"default_agent_id":"codex"}' >"$state_dir/panel-settings/settings.global.json"

env \
SLINT_MCP_PORT="$mcp_port" \
RUI_PANEL_INPUT_TRACE=1 \
QSG_RENDER_LOOP=basic \
RUI_ACP_CACHE_DIR="$state_dir/panel" \
RUI_ACPX_CODEX_URL="http://127.0.0.1:$gateway_port" \
RUI_ACPX_CLAUDE_URL="http://127.0.0.1:$gateway_port" \
"$shotcut_bin" --appdata "$state_dir/shotcut" --noupgrade \
    >"$state_dir/shotcut.stdout.log" \
    2>"$state_dir/shotcut.stderr.log" &
shotcut_pid="$!"

sleep "${PANEL_HOST_E2E_MCP_SETTLE_SECONDS:-5}"
if ! kill -0 "$shotcut_pid" 2>/dev/null; then
    printf 'Shotcut exited before the MCP scenario ran. See %s/shotcut.stderr.log\n' \
        "$state_dir" >&2
    exit 1
fi

python3 "$repo_root/panel-rust/tests/host_e2e_mcp_driver.py" \
    --mcp-url "http://127.0.0.1:$mcp_port/mcp" \
    --event-log "$state_dir/acpx/backend-events.jsonl" \
    --host-log "$state_dir/shotcut.stdout.log" \
    "$scenario"

printf 'backend events: %s/acpx/backend-events.jsonl\n' "$state_dir"
