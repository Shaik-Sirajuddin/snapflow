# Shared by host_e2e_smoke.sh and host_e2e_mcp_smoke.sh -- sourced, not
# executed directly (no shebang, no `set -e` of its own: inherits the
# caller's `set -euo pipefail`).
#
# PROF-4 (`profile-only-backend-selection` plan): the panel used to reach
# `rui-mock-agent` by setting `ACPX_BACKEND_CMD` on the gateway process
# (removed from production in PROF-3) and relying on native/unmanaged mode
# (no `_acpx.profile`). Both host harnesses need the same replacement, so it
# lives here once instead of as two copies of the same curl sequence:
# register `rui-mock-agent` as a durable admin-plane custom agent, create a
# profile pointing at it, then write a real `settings.global.json` so the
# panel's own PROF-2 cold-start seed binds that profile as `_acpx.profile`.
#
# Registering the custom agent under the gateway's own `ACPX_DEFAULT_
# AGENT_ID` ("codex") does NOT work -- acpx-server's own `main.rs`
# unconditionally pre-registers a supervisor entry under that exact id at
# startup (its own bare npx-codex-acp default when `ACPX_BACKEND_CMD` is
# unset), so a later custom-agent registration for the same id 409s with
# "custom agent id codex conflicts with an existing registered backend"
# (verified against a real spawned acpx-server before writing this).
# Registering under a DISTINCT id ("mock-codex") and binding it through a
# real profile instead sidesteps the conflict rather than working around
# it -- the panel then never touches native mode at all.
#
# Args: $1 = gateway_port, $2 = admin_port, $3 = admin_token,
#       $4 = agent_bin (path to the compiled rui-mock-agent binary),
#       $5 = state_dir (the harness's own per-run temp directory --
#       settings.global.json is written to
#       "$state_dir/panel-settings/settings.global.json", matching
#       settings_file::SettingsPaths::from_env's `{RUI_ACP_CACHE_DIR}/../
#       panel-settings` derivation for a sibling `RUI_ACP_CACHE_DIR =
#       "$state_dir/panel"`, which both callers already set).
provision_mock_profile_via_admin() {
    local gateway_port="$1" admin_port="$2" admin_token="$3" agent_bin="$4" state_dir="$5"

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
    printf '{"schema_version":1,"default_agent_id":"codex"}' \
        >"$state_dir/panel-settings/settings.global.json"
}
