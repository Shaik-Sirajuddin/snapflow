#!/usr/bin/env bash
#
# Phase 5 host-process smoke harness. This intentionally drives the real
# Shotcut executable, the real acpx-server, and the compiled mock ACP backend.
# It keeps every durable location under one temporary directory so no user
# QSettings, panel cache, or ACPX SQLite state leaks into the run.
#
# It drives X11 directly with XTEST and asserts backend-received ACPX events.
# VNC is optional for manual inspection only; the automated gate never
# captures screenshots.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
state_dir="${PANEL_HOST_E2E_STATE_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/panel-host-e2e.XXXXXX")}"
keep_state="${PANEL_HOST_E2E_KEEP_STATE:-0}"
display="${PANEL_HOST_E2E_DISPLAY:-:109}"
screen="${PANEL_HOST_E2E_SCREEN:-1280x800x24}"
gateway_port="${PANEL_HOST_E2E_GATEWAY_PORT:-18790}"
dock_width="${PANEL_HOST_E2E_DOCK_WIDTH:-}"

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
# Keep both ends open in this shell. The server's stdio transport must not see
# EOF while its HTTP/WS transport is serving the embedded panel.
exec 3<>"$fifo"

server_pid=""
xvfb_pid=""
shotcut_pid=""
shotcut_run=0
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
        printf 'host E2E state retained at %s\n' "$state_dir"
    fi
}
trap cleanup EXIT INT TERM

Xvfb "$display" -screen 0 "$screen" -nolisten tcp >"$state_dir/xvfb.log" 2>&1 &
xvfb_pid="$!"
export DISPLAY="$display"

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

start_shotcut() {
    shotcut_run=$((shotcut_run + 1))
    local trace_env=()
    if [[ "${PANEL_HOST_E2E_DRIVE:-0}" == "1" || "${PANEL_HOST_E2E_RESTART:-0}" == "1" \
          || "${PANEL_HOST_E2E_CANCEL:-0}" == "1" || "${PANEL_HOST_E2E_PERMISSION:-0}" == "1" \
          || "${PANEL_HOST_E2E_TOOL_STREAM:-0}" == "1" \
          || "${PANEL_HOST_E2E_LOCAL_TERMINAL:-0}" == "1" ]]; then
        trace_env=(RUI_PANEL_INPUT_TRACE=1)
    fi
    env "${trace_env[@]}" \
    QSG_RENDER_LOOP=basic \
    RUI_ACP_CACHE_DIR="$state_dir/panel" \
    RUI_ACPX_CODEX_URL="http://127.0.0.1:$gateway_port" \
    RUI_ACPX_CLAUDE_URL="http://127.0.0.1:$gateway_port" \
    "$shotcut_bin" --appdata "$state_dir/shotcut" --noupgrade \
        >"$state_dir/shotcut.$shotcut_run.stdout.log" \
        2>"$state_dir/shotcut.$shotcut_run.stderr.log" &
    shotcut_pid="$!"

    sleep "${PANEL_HOST_E2E_SETTLE_SECONDS:-5}"
    if ! kill -0 "$shotcut_pid" 2>/dev/null; then
        printf 'Shotcut exited before event validation. See %s/shotcut.%s.stderr.log\n' \
            "$state_dir" "$shotcut_run" >&2
        exit 1
    fi
}

start_shotcut

if [[ "${PANEL_HOST_E2E_DRIVE:-0}" == "1" ]]; then
    if [[ -z "$dock_width" ]]; then
        printf 'PANEL_HOST_E2E_DRIVE requires PANEL_HOST_E2E_DOCK_WIDTH\n' >&2
        exit 1
    fi
    python3 "$repo_root/panel-rust/tests/host_e2e_driver.py" \
        --dock-width "$dock_width" \
        --event-log "$state_dir/acpx/backend-events.jsonl" \
        --host-log "$state_dir/shotcut.$shotcut_run.stderr.log" \
        --exercise-backspace \
        --wait-for-attachment \
        --wait-for-turn
fi

if [[ "${PANEL_HOST_E2E_NEW_THREAD:-0}" == "1" ]]; then
    if [[ -z "$dock_width" ]]; then
        printf 'PANEL_HOST_E2E_NEW_THREAD requires PANEL_HOST_E2E_DOCK_WIDTH\n' >&2
        exit 1
    fi
    if [[ "${PANEL_HOST_E2E_DRIVE:-0}" != "1" ]]; then
        printf 'PANEL_HOST_E2E_NEW_THREAD requires PANEL_HOST_E2E_DRIVE=1 (needs thread 0''s\n'
        printf 'prompt marker on the backend event log to prove session isolation)\n' >&2
        exit 1
    fi
    python3 "$repo_root/panel-rust/tests/host_e2e_driver.py" \
        --dock-width "$dock_width" \
        --event-log "$state_dir/acpx/backend-events.jsonl" \
        --host-log "$state_dir/shotcut.$shotcut_run.stderr.log" \
        --prompt "host e2e new thread prompt" \
        --new-thread-before \
        --wait-for-turn \
        --different-session-from "host e2e prompt"
fi

if [[ "${PANEL_HOST_E2E_PROVIDER_ISOLATION:-0}" == "1" ]]; then
    if [[ -z "$dock_width" ]]; then
        printf 'PANEL_HOST_E2E_PROVIDER_ISOLATION requires PANEL_HOST_E2E_DOCK_WIDTH\n' >&2
        exit 1
    fi
    if [[ "${PANEL_HOST_E2E_DRIVE:-0}" != "1" ]]; then
        printf 'PANEL_HOST_E2E_PROVIDER_ISOLATION requires PANEL_HOST_E2E_DRIVE=1 (needs\n'
        printf 'thread 0''s prompt marker on the backend event log to prove isolation)\n' >&2
        exit 1
    fi
    # Thread index 1 is the fixture's second default thread
    # (`DEFAULT_THREAD_NAMES` in lib.rs alternates codex/claude by index),
    # so selecting it and sending a distinguishable prompt proves the
    # Claude-provider thread's own session never crosses with thread 0's
    # Codex session -- no XTEST-driven thread creation needed since the
    # fixture already ships one thread per provider.
    python3 "$repo_root/panel-rust/tests/host_e2e_driver.py" \
        --dock-width "$dock_width" \
        --event-log "$state_dir/acpx/backend-events.jsonl" \
        --host-log "$state_dir/shotcut.$shotcut_run.stderr.log" \
        --prompt "host e2e provider isolation prompt" \
        --select-thread-row 1 \
        --wait-for-turn \
        --different-session-from "host e2e prompt"
fi

if [[ "${PANEL_HOST_E2E_CANCEL:-0}" == "1" ]]; then
    if [[ -z "$dock_width" ]]; then
        printf 'PANEL_HOST_E2E_CANCEL requires PANEL_HOST_E2E_DOCK_WIDTH\n' >&2
        exit 1
    fi
    # Self-contained: sends its own 'slow '-prefixed prompt (rui-mock-agent
    # blocks on it until a real session/cancel notification arrives, see
    # mock_agent.rs), so this does not depend on PANEL_HOST_E2E_DRIVE's own
    # "host e2e prompt" marker the way NEW_THREAD/PROVIDER_ISOLATION do.
    python3 "$repo_root/panel-rust/tests/host_e2e_driver.py" \
        --dock-width "$dock_width" \
        --event-log "$state_dir/acpx/backend-events.jsonl" \
        --host-log "$state_dir/shotcut.$shotcut_run.stderr.log" \
        --prompt "slow host e2e cancel" \
        --wait-for-attachment \
        --cancel-after-send
fi

if [[ "${PANEL_HOST_E2E_PERMISSION:-0}" == "1" ]]; then
    if [[ -z "$dock_width" ]]; then
        printf 'PANEL_HOST_E2E_PERMISSION requires PANEL_HOST_E2E_DOCK_WIDTH\n' >&2
        exit 1
    fi
    # Self-contained, same reasoning as PANEL_HOST_E2E_CANCEL: sends its
    # own 'permission '-prefixed prompt (rui-mock-agent relays a real
    # session/request_permission request and blocks on the real client's
    # answer, see mock_agent.rs), so no PANEL_HOST_E2E_DRIVE dependency.
    python3 "$repo_root/panel-rust/tests/host_e2e_driver.py" \
        --dock-width "$dock_width" \
        --event-log "$state_dir/acpx/backend-events.jsonl" \
        --host-log "$state_dir/shotcut.$shotcut_run.stderr.log" \
        --prompt "permission host e2e run a risky command" \
        --wait-for-attachment \
        --permission-decision approve \
        --wait-for-turn
fi

if [[ "${PANEL_HOST_E2E_TOOL_STREAM:-0}" == "1" ]]; then
    if [[ -z "$dock_width" ]]; then
        printf 'PANEL_HOST_E2E_TOOL_STREAM requires PANEL_HOST_E2E_DOCK_WIDTH\n' >&2
        exit 1
    fi
    # Self-contained, same reasoning as PANEL_HOST_E2E_CANCEL/PERMISSION:
    # sends its own plain prompt (rui-mock-agent's default, non-slow,
    # non-permission handling always emits one thought chunk, one tool
    # call, and one uppercased message chunk per turn -- see
    # mock_agent.rs's send_replay), so no PANEL_HOST_E2E_DRIVE dependency.
    python3 "$repo_root/panel-rust/tests/host_e2e_driver.py" \
        --dock-width "$dock_width" \
        --event-log "$state_dir/acpx/backend-events.jsonl" \
        --host-log "$state_dir/shotcut.$shotcut_run.stderr.log" \
        --prompt "host e2e tool stream" \
        --wait-for-attachment \
        --wait-for-turn \
        --assert-tool-stream
fi

if [[ "${PANEL_HOST_E2E_LOCAL_TERMINAL:-0}" == "1" ]]; then
    if [[ -z "$dock_width" ]]; then
        printf 'PANEL_HOST_E2E_LOCAL_TERMINAL requires PANEL_HOST_E2E_DOCK_WIDTH\n' >&2
        exit 1
    fi
    # Entirely host/client-local -- no ACPX backend involvement, so no
    # --prompt/session flow at all, unlike every other scenario above.
    python3 "$repo_root/panel-rust/tests/host_e2e_driver.py" \
        --dock-width "$dock_width" \
        --event-log "$state_dir/acpx/backend-events.jsonl" \
        --host-log "$state_dir/shotcut.$shotcut_run.stderr.log" \
        --wait-for-attachment \
        --local-terminal-round-trip
fi

if [[ "${PANEL_HOST_E2E_RESTART:-0}" == "1" ]]; then
    if [[ -z "$dock_width" ]]; then
        printf 'PANEL_HOST_E2E_RESTART requires PANEL_HOST_E2E_DOCK_WIDTH\n' >&2
        exit 1
    fi
    kill "$shotcut_pid"
    wait "$shotcut_pid" || true
    shotcut_pid=""
    start_shotcut
    python3 "$repo_root/panel-rust/tests/host_e2e_driver.py" \
        --dock-width "$dock_width" \
        --event-log "$state_dir/acpx/backend-events.jsonl" \
        --host-log "$state_dir/shotcut.$shotcut_run.stderr.log" \
        --prompt "host e2e after restart" \
        --wait-for-attachment \
        --same-session-as "host e2e prompt"
fi

printf 'PASS host E2E smoke\n'
printf 'backend events: %s/acpx/backend-events.jsonl\n' "$state_dir"

if [[ "${PANEL_HOST_E2E_HOLD:-0}" == "1" ]]; then
    while kill -0 "$shotcut_pid" 2>/dev/null; do
        sleep 1
    done
fi
