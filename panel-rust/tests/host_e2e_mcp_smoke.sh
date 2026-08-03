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

# shellcheck source=host_e2e_admin_provision.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/host_e2e_admin_provision.sh"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
state_dir="${PANEL_HOST_E2E_MCP_STATE_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/panel-host-e2e-mcp.XXXXXX")}"
keep_state="${PANEL_HOST_E2E_MCP_KEEP_STATE:-0}"
display="${PANEL_HOST_E2E_MCP_DISPLAY:-:112}"
screen="${PANEL_HOST_E2E_MCP_SCREEN:-1280x800x24}"
gateway_port="${PANEL_HOST_E2E_MCP_GATEWAY_PORT:-18796}"
admin_port="${PANEL_HOST_E2E_MCP_ADMIN_PORT:-18797}"
admin_token="panel-host-e2e-mcp-admin-token-$$"
mcp_port="${PANEL_HOST_E2E_MCP_PORT:-19099}"
scenario="${1:?usage: host_e2e_mcp_smoke.sh <send-now|fast-track|queue-auto-drain|queue-during-init|queue-during-init-multi|queue-stop-with-multiple-queued|queue-restart|rename|startup-warning|mid-session-write-failure|real-agent-smoke>}"
project_path="${PANEL_HOST_E2E_MCP_PROJECT_PATH:-}"

if [[ "$scenario" == "queue-restart" ]]; then
    # The custom mock profile inherits this durable state file from acpx-server
    # so the replacement backend can answer the resumed session after restart.
    export RUI_MOCK_AGENT_STATE_FILE="${PANEL_HOST_E2E_MCP_STATE_FILE:-$state_dir/acpx/mock-agent-state.json}"
fi

server_bin="${ACPX_SERVER_BIN:-$repo_root/acpx/target/debug/acpx-server}"
agent_bin="${RUI_MOCK_AGENT_BIN:-$repo_root/panel-rust/target/debug/rui-mock-agent}"
shotcut_bin="${SHOTCUT_BIN:-$repo_root/shotcut/build/src/shotcut}"
# SCNA-09: real-agent-smoke skips the mock backend entirely -- ACPX_BACKEND_CMD
# stays unset so acpx-registry's own real fallback registry (ambient CLI auth,
# npx-spawned) resolves ACPX_DEFAULT_AGENT_ID for real, same as host_vnc_dev.sh.
# HOME must stay this machine's real $HOME (not a sandboxed one) so the
# ambient ~/.claude/.credentials.json OAuth this needs is actually found.
real_agent_id="${PANEL_HOST_E2E_MCP_REAL_AGENT_ID:-claude-acp}"

required_binaries=("$server_bin" "$shotcut_bin" Xvfb curl python3)
if [[ "$scenario" != "real-agent-smoke" ]]; then
    required_binaries+=("$agent_bin")
fi
for binary in "${required_binaries[@]}"; do
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
    chmod -f 755 "$state_dir/panel" 2>/dev/null || true
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

if [[ "$scenario" == "real-agent-smoke" ]]; then
    # SCNA-09: ambient registry agent, no mock / no admin mock profile.
    ACPX_HTTP_BIND="127.0.0.1:$gateway_port" \
    ACPX_DEFAULT_AGENT_ID="$real_agent_id" \
    ACPX_DB_PATH="$state_dir/acpx/gateway.sqlite3" \
    "$server_bin" <"$fifo" >"$state_dir/acpx/server.stdout.log" 2>"$state_dir/acpx/server.stderr.log" &
else
    # Main/PROF-4: profile-only mock via admin plane (no ACPX_BACKEND_CMD).
    ACPX_HTTP_BIND="127.0.0.1:$gateway_port" \
    ACPX_DEFAULT_AGENT_ID="codex" \
    ACPX_DB_PATH="$state_dir/acpx/gateway.sqlite3" \
    ACPX_STORAGE_DIR="$state_dir/acpx/storage" \
    ACPX_ADMIN_TOKEN="$admin_token" \
    ACPX_ADMIN_BIND="127.0.0.1:$admin_port" \
    RUI_MOCK_AGENT_EVENT_LOG="$state_dir/acpx/backend-events.jsonl" \
    "$server_bin" <"$fifo" >"$state_dir/acpx/server.stdout.log" 2>"$state_dir/acpx/server.stderr.log" &
fi
server_pid="$!"

for _ in $(seq 1 80); do
    if curl --fail --silent "http://127.0.0.1:$gateway_port/health" >/dev/null; then
        break
    fi
    sleep 0.1
done
curl --fail --silent "http://127.0.0.1:$gateway_port/health" >/dev/null

# PROF-4: mock profile via admin plane for non-real scenarios.
if [[ "$scenario" != "real-agent-smoke" ]]; then
    provision_mock_profile_via_admin "$gateway_port" "$admin_port" "$admin_token" "$agent_bin" "$state_dir"
fi

# SCNA-01: read-only panel cache dir before create for startup-warning.
if [[ "$scenario" == "startup-warning" ]]; then
    chmod 555 "$state_dir/panel"
fi

shotcut_project_args=()
if [[ -n "$project_path" ]]; then
    shotcut_project_args+=("$project_path")
fi

env \
SLINT_MCP_PORT="$mcp_port" \
RUI_PANEL_INPUT_TRACE=1 \
QSG_RENDER_LOOP=basic \
RUI_ACP_CACHE_DIR="$state_dir/panel" \
RUI_ACPX_CODEX_URL="http://127.0.0.1:$gateway_port" \
RUI_ACPX_CLAUDE_URL="http://127.0.0.1:$gateway_port" \
"$shotcut_bin" --appdata "$state_dir/shotcut" --noupgrade "${shotcut_project_args[@]}" \
    >"$state_dir/shotcut.stdout.log" \
    2>"$state_dir/shotcut.stderr.log" &
shotcut_pid="$!"

sleep "${PANEL_HOST_E2E_MCP_SETTLE_SECONDS:-5}"
if ! kill -0 "$shotcut_pid" 2>/dev/null; then
    printf 'Shotcut exited before the MCP scenario ran. See %s/shotcut.stderr.log\n' \
        "$state_dir" >&2
    exit 1
fi

driver_args=(
    --mcp-url "http://127.0.0.1:$mcp_port/mcp"
    --event-log "$state_dir/acpx/backend-events.jsonl"
    --host-log "$state_dir/shotcut.stdout.log"
    --state-dir "$state_dir"
)
if [[ "$scenario" == "real-agent-smoke" ]]; then
    # A real ambient-auth npx spawn (first-run package fetch/cache) plus a
    # real model round trip is slower and less bounded than the mock
    # agent's near-instant scripted replies.
    driver_args+=(--timeout "${PANEL_HOST_E2E_MCP_REAL_AGENT_TIMEOUT:-90}")
fi

if [[ "$scenario" == "queue-restart" ]]; then
    # Phase 1 leaves one authoritative queue row visible while the first turn
    # is active. Pause it before replacing acpx-server so restart cannot
    # consume the row before the preload assertion.
    python3 "$repo_root/panel-rust/tests/host_e2e_mcp_driver.py" \
        "${driver_args[@]}" queue-preload
    session_id="$(sed -n 's/.*attachment: thread=.*session=Some("\([^"]*\)").*/\1/p' \
        "$state_dir/shotcut.stdout.log" | tail -1)"
    [[ -n "$session_id" ]] || { echo 'queue-restart: no ACPX session id in panel trace' >&2; exit 1; }
    pause_response="$(curl --fail --silent -X POST "http://127.0.0.1:$gateway_port/rpc" \
        -H 'Content-Type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"id\":41,\"method\":\"session/queue\",\"params\":{\"sessionId\":\"$session_id\",\"operation\":\"pause\",\"idempotencyKey\":\"host-restart-pause\"}}")"
    printf '%s' "$pause_response" | rg -q '"error"' && { echo "queue-restart: pause failed: $pause_response" >&2; exit 1; }

    kill "$server_pid"
    wait "$server_pid" || true
    server_pid=""
    ACPX_HTTP_BIND="127.0.0.1:$gateway_port" \
    ACPX_DEFAULT_AGENT_ID="codex" \
    ACPX_DB_PATH="$state_dir/acpx/gateway.sqlite3" \
    ACPX_STORAGE_DIR="$state_dir/acpx/storage" \
    ACPX_ADMIN_TOKEN="$admin_token" \
    ACPX_ADMIN_BIND="127.0.0.1:$admin_port" \
    RUI_MOCK_AGENT_EVENT_LOG="$state_dir/acpx/backend-events.jsonl" \
    "$server_bin" <"$fifo" >"$state_dir/acpx/server.restart.stdout.log" \
        2>"$state_dir/acpx/server.restart.stderr.log" &
    server_pid="$!"
    for _ in $(seq 1 80); do
        if curl --fail --silent "http://127.0.0.1:$gateway_port/health" >/dev/null; then
            break
        fi
        sleep 0.1
    done
    curl --fail --silent "http://127.0.0.1:$gateway_port/health" >/dev/null
    resume_response="$(curl --fail --silent -X POST "http://127.0.0.1:$gateway_port/rpc" \
        -H 'Content-Type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"session/queue\",\"params\":{\"sessionId\":\"$session_id\",\"operation\":\"resume\",\"idempotencyKey\":\"host-restart-resume\"}}")"
    printf '%s' "$resume_response" | rg -q '"error"' && { echo "queue-restart: resume failed: $resume_response" >&2; exit 1; }
    python3 "$repo_root/panel-rust/tests/host_e2e_mcp_driver.py" \
        "${driver_args[@]}" queue-after-restart
else
    python3 "$repo_root/panel-rust/tests/host_e2e_mcp_driver.py" "${driver_args[@]}" "$scenario"
fi

printf 'backend events: %s/acpx/backend-events.jsonl\n' "$state_dir"
