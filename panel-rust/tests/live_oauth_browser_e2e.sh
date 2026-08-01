#!/usr/bin/env bash
#
# Real, browser-driven MCP OAuth 2.1 end-to-end verification -- NOT part
# of `cargo test` (mirrors `host_vnc_dev.sh`'s "manual harness" posture:
# it depends on a real Chrome install + Playwright via `npx`, neither of
# which every CI/dev box is guaranteed to have). Complements
# `gateway_actor_mcp_agents_e2e_test.rs`'s automated stub-server test
# (which drives HTTP directly in Rust test code) by proving the exact
# same discovery/DCR/PKCE/loopback-listener/exchange code path also works
# when a real browser (Playwright + the system's installed google-chrome)
# clicks through a real HTML consent page, and by inspecting the real
# `acpx-server` debug log output for all four scenarios: success,
# token-endpoint failure, CSRF/state-mismatch rejection, and disconnect
# (`mcp_servers/logout`) -- plus a log-security check (the real access/
# refresh token values must never appear in the captured log).
#
# Requires: the real compiled `acpx-server` binary (`cargo build -p
# acpx-server` first), `python3`, `node`+`npm` (this script runs `npm
# install` in this directory on first use, per `package.json` -- needs
# internet the first time; `node_modules/` here is gitignored, not
# vendored), and a real `google-chrome` install (Playwright launches it
# via `channel: "chrome"`, not a separate bundled download -- ESM
# `import` resolution requires a real local `node_modules/playwright`
# next to `oauth_browser_driver.mjs`, not just an `npx`-cached copy:
# Node's ESM resolver, unlike CommonJS `require`, does not consult
# `NODE_PATH`/npx's temp install location).
#
# Usage: panel-rust/tests/live_oauth_browser_e2e.sh
# Env overrides: LIVE_OAUTH_STATE_DIR, LIVE_OAUTH_GATEWAY_PORT.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
state_dir="${LIVE_OAUTH_STATE_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/live-oauth-e2e.XXXXXX")}"
gateway_port="${LIVE_OAUTH_GATEWAY_PORT:-0}"
server_bin="${ACPX_SERVER_BIN:-$repo_root/acpx/target/debug/acpx-server}"
driver_dir="$repo_root/panel-rust/tests"

for binary in "$server_bin" python3 node npx curl google-chrome; do
    if ! command -v "$binary" >/dev/null 2>&1 && [ "$binary" != "$server_bin" ]; then
        echo "live_oauth_browser_e2e.sh: missing prerequisite: $binary" >&2
        exit 1
    fi
done
if [ ! -x "$server_bin" ]; then
    echo "live_oauth_browser_e2e.sh: acpx-server binary not found at $server_bin -- run \`cargo build -p acpx-server\` first" >&2
    exit 1
fi

echo "state dir: $state_dir"
mkdir -p "$state_dir/gw_state"

# Resolve a free port for the gateway's own HTTP bind (port 0 trick: bind
# then immediately close, same TOCTOU caveat every other spawn helper in
# this repo's e2e tests accepts).
if [ "$gateway_port" = "0" ]; then
    gateway_port=$(python3 -c "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); print(s.getsockname()[1]); s.close()")
fi
echo "gateway port: $gateway_port"

pids=()
pidfile="$state_dir/spawned.pids"
: >"$pidfile"
cleanup() {
    for pid in "${pids[@]:-}"; do
        kill -9 "$pid" >/dev/null 2>&1 || true
    done
    # `start_stub` below is invoked via `$(...)` command substitution (to
    # capture its printed origin), which runs it in a subshell -- any
    # `pids+=(...)` there would only mutate that subshell's own copy of
    # the array, never this one, silently leaking the spawned stub-server
    # processes on every run (caught live: two `live_oauth_browser_e2e.sh`
    # runs during this script's own verification left 6 orphaned
    # `oauth_stub_server.py` processes running). Real filesystem I/O
    # (this pidfile) is the one thing that *does* cross that subshell
    # boundary, so `start_stub` appends there instead of to `pids`.
    if [ -f "$pidfile" ]; then
        while IFS= read -r pid; do
            [ -n "$pid" ] && kill -9 "$pid" >/dev/null 2>&1 || true
        done <"$pidfile"
    fi
}
trap cleanup EXIT

start_stub() {
    local log_prefix="$1"
    local fail_token="$2"
    local out_log="$state_dir/${log_prefix}.stdout.log"
    local err_log="$state_dir/${log_prefix}.stderr.log"
    if [ "$fail_token" = "1" ]; then
        FAIL_TOKEN=1 python3 "$driver_dir/oauth_stub_server.py" 0 >"$out_log" 2>"$err_log" &
    else
        python3 "$driver_dir/oauth_stub_server.py" 0 >"$out_log" 2>"$err_log" &
    fi
    local pid=$!
    echo "$pid" >>"$pidfile"
    for _ in $(seq 1 50); do
        if grep -q "STUB_OAUTH_ORIGIN=" "$out_log" 2>/dev/null; then
            grep "STUB_OAUTH_ORIGIN=" "$out_log" | head -1 | cut -d= -f2
            return 0
        fi
        sleep 0.1
    done
    echo "live_oauth_browser_e2e.sh: stub oauth server ($log_prefix) never bound" >&2
    exit 1
}

rpc() {
    curl -s "http://127.0.0.1:$gateway_port/rpc" -X POST -H "Content-Type: application/json" -d "$1"
}

echo "== starting success-path stub oauth server =="
success_origin=$(start_stub "stub_success" "0")
echo "success stub origin: $success_origin"

echo "== starting failure-path stub oauth server (FAIL_TOKEN=1) =="
fail_origin=$(start_stub "stub_fail" "1")
echo "fail stub origin: $fail_origin"

echo "== starting real acpx-server (RUST_LOG=acpx_core=debug) =="
acpx_log="$state_dir/acpx_server.log"
ACPX_HTTP_BIND="127.0.0.1:$gateway_port" \
ACPX_BACKEND_CMD="sh -c 'while IFS= read -r line; do id=\$(echo \"\$line\" | grep -o \"\\\"id\\\":[0-9]*\" | head -1 | cut -d: -f2); echo \"{\\\"jsonrpc\\\":\\\"2.0\\\",\\\"id\\\":\$id,\\\"result\\\":{}}\"; done'" \
ACPX_DEFAULT_AGENT_ID="live-oauth-e2e-agent" \
ACPX_DB_PATH="$state_dir/gw_state/acpx.db" \
RUST_LOG="acpx_core=debug,acpx_server=debug" \
"$server_bin" >"$acpx_log" 2>&1 &
pids+=("$!")

for _ in $(seq 1 100); do
    if curl -s -o /dev/null "http://127.0.0.1:$gateway_port/rpc"; then
        break
    fi
    sleep 0.1
done

echo "== installing the browser driver's Playwright dependency (npm install) =="
( cd "$driver_dir" && npm install --silent --no-audit --no-fund )

fail_count=0

echo "== scenario 1/4: success (real browser click-through) =="
rpc "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"mcp_servers/create\",\"params\":{\"name\":\"browser-oauth-test\",\"type\":\"http\",\"url\":\"$success_origin/mcp\"}}" >/dev/null
auth_url=$(rpc '{"jsonrpc":"2.0","id":2,"method":"mcp_servers/authenticate","params":{"name":"browser-oauth-test"}}' | python3 -c "import json,sys; print(json.load(sys.stdin)['result']['authorizationUrl'])")
( cd "$driver_dir" && node oauth_browser_driver.mjs "$auth_url" )
sleep 1
status=$(rpc '{"jsonrpc":"2.0","id":3,"method":"mcp_servers/list","params":{}}' | python3 -c "
import json, sys
data = json.load(sys.stdin)
for s in data['result']['servers']:
    if s['name'] == 'browser-oauth-test':
        print(s.get('auth_status', ''))
")
if [ "$status" = "authenticated" ]; then
    echo "PASS: success scenario -- auth_status=authenticated"
else
    echo "FAIL: success scenario -- expected authenticated, got '$status'"
    fail_count=$((fail_count + 1))
fi

echo "== scenario 2/4: token endpoint failure =="
rpc "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"mcp_servers/create\",\"params\":{\"name\":\"browser-oauth-fail-test\",\"type\":\"http\",\"url\":\"$fail_origin/mcp\"}}" >/dev/null
auth_url=$(rpc '{"jsonrpc":"2.0","id":5,"method":"mcp_servers/authenticate","params":{"name":"browser-oauth-fail-test"}}' | python3 -c "import json,sys; print(json.load(sys.stdin)['result']['authorizationUrl'])")
( cd "$driver_dir" && node oauth_browser_driver.mjs "$auth_url" )
sleep 1
if grep -q "oauth token exchange failed.*browser-oauth-fail-test" "$acpx_log"; then
    echo "PASS: failure scenario -- token exchange failure logged"
else
    echo "FAIL: failure scenario -- no token-exchange-failed log line found"
    fail_count=$((fail_count + 1))
fi

echo "== scenario 3/4: CSRF / state mismatch =="
rpc "{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"mcp_servers/create\",\"params\":{\"name\":\"browser-oauth-csrf-test\",\"type\":\"http\",\"url\":\"$success_origin/mcp\"}}" >/dev/null
auth_url=$(rpc '{"jsonrpc":"2.0","id":7,"method":"mcp_servers/authenticate","params":{"name":"browser-oauth-csrf-test"}}' | python3 -c "import json,sys; print(json.load(sys.stdin)['result']['authorizationUrl'])")
redirect_uri=$(python3 -c "
import urllib.parse, sys
q = urllib.parse.urlparse('$auth_url').query
print(urllib.parse.parse_qs(q)['redirect_uri'][0])
")
curl -s -o /dev/null "${redirect_uri}?code=forged-code&state=wrong-state-value-attacker-controlled"
sleep 1
if grep -q "oauth callback state mismatch, discarding.*browser-oauth-csrf-test" "$acpx_log"; then
    echo "PASS: CSRF scenario -- state mismatch rejected and logged"
else
    echo "FAIL: CSRF scenario -- no state-mismatch log line found"
    fail_count=$((fail_count + 1))
fi

echo "== scenario 4/4: disconnect (mcp_servers/logout) =="
rpc '{"jsonrpc":"2.0","id":8,"method":"mcp_servers/logout","params":{"name":"browser-oauth-test"}}' >/dev/null
sleep 1
status=$(rpc '{"jsonrpc":"2.0","id":9,"method":"mcp_servers/list","params":{}}' | python3 -c "
import json, sys
data = json.load(sys.stdin)
for s in data['result']['servers']:
    if s['name'] == 'browser-oauth-test':
        print(s.get('auth_status', ''))
")
if [ "$status" = "unauthenticated" ]; then
    echo "PASS: disconnect scenario -- auth_status=unauthenticated"
else
    echo "FAIL: disconnect scenario -- expected unauthenticated, got '$status'"
    fail_count=$((fail_count + 1))
fi

echo "== security check: no raw token value ever appears in the captured log =="
leak_count=$(grep -c "browser-test-access-token\|browser-test-refresh-token\|browser-test-refreshed-access-token" "$acpx_log" || true)
if [ "$leak_count" = "0" ]; then
    echo "PASS: security check -- 0 occurrences of any real token value in $acpx_log"
else
    echo "FAIL: security check -- found $leak_count occurrence(s) of a raw token value in $acpx_log"
    fail_count=$((fail_count + 1))
fi

echo ""
echo "acpx-server log: $acpx_log"
if [ "$fail_count" -eq 0 ]; then
    echo "ALL 5 CHECKS PASSED"
    exit 0
else
    echo "$fail_count CHECK(S) FAILED"
    exit 1
fi
