//! End-to-end coverage for Phase 3 (`04-phased-plan.md` steps 12-17a):
//! `profiles/*`/`mcp_servers/*` JSON-RPC CRUD dispatch, and `session/new`
//! actually resolving `_acpx.profile` -> provider env injection + central
//! MCP server merge against a spawned stand-in backend. Same synthetic
//! `sh`-script-backend trick as `router_dispatch_test.rs` (see that file's
//! doc comment) -- these scripts additionally *echo back* what they
//! observed (received env vars, whether a given `mcpServers` entry was
//! present in the incoming request) so assertions can verify the gateway
//! actually injected/merged what it claims to, not just that dispatch
//! didn't error.

use acpx_conductor::SpawnSpec;
use acpx_core::keystore::KeyRef;
use acpx_core::profile::Profile;
use acpx_core::provider::{ProviderConfig, ProviderKind};
use acpx_core::router::Router;
use serde_json::json;
use std::collections::HashMap;

/// Stand-in backend that echoes back the raw `CODEX_API_KEY` env var it
/// was launched with (safe to embed directly -- the test's key values
/// never contain characters that would break JSON string quoting) plus
/// whether `CODEX_CONFIG` contains a given marker substring, and whether
/// the incoming request's raw text mentions a couple of marker MCP server
/// names -- enough to verify both provider-env injection and the
/// central/client `mcpServers` merge reached the spawned process, without
/// needing a JSON parser (or JSON-escaping a JSON value into a JSON
/// string) in shell.
const OBSERVING_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  saw_central=false
  echo "$line" | grep -q '"name":"central-fs"' && saw_central=true
  saw_client=false
  echo "$line" | grep -q '"name":"client-git"' && saw_client=true
  saw_base_url=false
  case "${CODEX_CONFIG:-}" in *"https://litellm.example.com/v1"*) saw_base_url=true ;; esac
  printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc","observedApiKey":"%s","observedConfigHasBaseUrl":%s,"sawCentralFs":%s,"sawClientGit":%s}}\n' \
    "$id" "${CODEX_API_KEY:-}" "$saw_base_url" "$saw_central" "$saw_client"
done
"#;

fn observing_backend_spec() -> SpawnSpec {
    SpawnSpec::new(
        "sh",
        vec!["-c".to_string(), OBSERVING_BACKEND_SCRIPT.to_string()],
    )
}

fn sample_profile(name: &str, agent_id: &str) -> Profile {
    Profile {
        name: name.to_string(),
        agent_id: agent_id.to_string(),
        provider: None,
        key_ref: None,
        launch_overrides: HashMap::new(),
        mcp_servers: vec![],
        permission_policy: Default::default(),
        allow_fs_access: false,
        allow_terminal_access: false,
        auth_method_id: None,
    }
}

#[tokio::test]
async fn profiles_crud_round_trips_via_dispatch() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", observing_backend_spec());

    let create = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "profiles/create",
            "params": {"name": "work", "agent_id": "stand-in-agent"}
        }))
        .await
        .expect("profiles/create");
    assert_eq!(create["result"]["name"], "work");

    // Duplicate create errors rather than silently overwriting.
    let dup = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "profiles/create",
            "params": {"name": "work", "agent_id": "stand-in-agent"}
        }))
        .await;
    assert!(dup.is_err());

    let list = router
        .dispatch(json!({"jsonrpc": "2.0", "id": 3, "method": "profiles/list", "params": {}}))
        .await
        .expect("profiles/list");
    // As of `ensure_default_profiles_seeded`, `profiles/list` also
    // includes one auto-seeded profile per `Installed` registry agent
    // (claude-acp/codex-acp/gemini in this environment -- see
    // `default_profile_seeding_test.rs`), so this asserts the explicit
    // "work" profile specifically rather than the list's total length.
    let profiles = list["result"]["profiles"].as_array().unwrap();
    assert_eq!(profiles.iter().filter(|p| p["name"] == "work").count(), 1);

    let update = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 4, "method": "profiles/update",
            "params": {"name": "work", "agent_id": "stand-in-agent", "mcp_servers": ["fs"]}
        }))
        .await
        .expect("profiles/update");
    assert_eq!(update["result"]["mcp_servers"], json!(["fs"]));

    let delete = router
        .dispatch(json!({"jsonrpc": "2.0", "id": 5, "method": "profiles/delete", "params": {"name": "work"}}))
        .await
        .expect("profiles/delete");
    assert_eq!(delete["result"]["deleted"], true);

    // Updating (or deleting) a now-nonexistent profile errors.
    assert!(router
        .dispatch(json!({"jsonrpc": "2.0", "id": 6, "method": "profiles/delete", "params": {"name": "work"}}))
        .await
        .is_err());
}

#[tokio::test]
async fn mcp_servers_crud_round_trips_via_dispatch() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", observing_backend_spec());

    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "mcp_servers/create",
            "params": {"name": "fs", "command": "mcp-fs"}
        }))
        .await
        .expect("mcp_servers/create");

    let list = router
        .dispatch(json!({"jsonrpc": "2.0", "id": 2, "method": "mcp_servers/list", "params": {}}))
        .await
        .expect("mcp_servers/list");
    assert_eq!(list["result"]["servers"].as_array().unwrap().len(), 1);

    router
        .dispatch(json!({"jsonrpc": "2.0", "id": 3, "method": "mcp_servers/delete", "params": {"name": "fs"}}))
        .await
        .expect("mcp_servers/delete");
    let list_after = router
        .dispatch(json!({"jsonrpc": "2.0", "id": 4, "method": "mcp_servers/list", "params": {}}))
        .await
        .expect("mcp_servers/list");
    assert_eq!(list_after["result"]["servers"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn session_new_with_unknown_profile_errors() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", observing_backend_spec());

    let response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp", "_acpx": {"profile": "does-not-exist"}}
        }))
        .await;
    assert!(response.is_err());
}

#[tokio::test]
async fn session_new_with_profile_injects_resolved_provider_env() {
    let mut router = Router::new("stand-in-agent");
    // The profile's `agent_id` reuses the already-registered stand-in spec
    // (see `resolve_profile`'s "prefer an already-registered spec" path) --
    // no live/fallback registry lookup needed for this test.
    router.register_agent("stand-in-agent", observing_backend_spec());
    router.register_provider(ProviderConfig {
        name: "litellm-proxy".to_string(),
        kind: ProviderKind::LiteLlm,
        base_url: Some("https://litellm.example.com/v1".to_string()),
    });
    let key_ref: KeyRef = router.store_key("sk-test-secret");

    let create = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "profiles/create",
            "params": {
                "name": "work-litellm",
                "agent_id": "stand-in-agent",
                "provider": "litellm-proxy",
                "key_ref": key_ref
            }
        }))
        .await
        .expect("profiles/create");
    assert_eq!(create["result"]["provider"], "litellm-proxy");

    let response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": {"cwd": "/tmp", "_acpx": {"profile": "work-litellm"}}
        }))
        .await
        .expect("session/new");

    assert_eq!(response["result"]["observedApiKey"], "sk-test-secret");
    assert_eq!(response["result"]["observedConfigHasBaseUrl"], true);
}

#[tokio::test]
async fn session_new_native_mode_never_touches_profile_store() {
    // Omitting `_acpx.profile` entirely must stay indistinguishable from
    // Phase 2's native passthrough -- no CODEX_API_KEY/CODEX_CONFIG env,
    // even with profiles/providers registered but unused.
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", observing_backend_spec());
    router.register_provider(ProviderConfig {
        name: "litellm-proxy".to_string(),
        kind: ProviderKind::LiteLlm,
        base_url: Some("https://litellm.example.com/v1".to_string()),
    });

    let response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .await
        .expect("session/new");
    assert_eq!(response["result"]["observedApiKey"], "");
    assert_eq!(response["result"]["observedConfigHasBaseUrl"], false);
}

#[tokio::test]
async fn session_new_with_profile_merges_central_mcp_servers_with_client_ones_winning() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", observing_backend_spec());

    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "mcp_servers/create",
            "params": {"name": "central-fs", "command": "mcp-central-fs"}
        }))
        .await
        .expect("mcp_servers/create");

    let mut profile = sample_profile("with-mcp", "stand-in-agent");
    profile.mcp_servers = vec!["central-fs".to_string()];
    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "profiles/create",
            "params": profile
        }))
        .await
        .expect("profiles/create");

    // Client sends its own "client-git" server alongside the profile --
    // both should reach the backend (central servers are additive), and
    // the merge must not drop a client-sent entry with no name collision.
    let response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/new",
            "params": {
                "cwd": "/tmp",
                "mcpServers": [{"name": "client-git", "command": "mcp-git"}],
                "_acpx": {"profile": "with-mcp"}
            }
        }))
        .await
        .expect("session/new");
    assert_eq!(response["result"]["sawCentralFs"], true);
    assert_eq!(response["result"]["sawClientGit"], true);
}

#[tokio::test]
async fn session_new_profile_with_no_mcp_servers_leaves_params_untouched() {
    // A profile with an empty `mcp_servers` list must be a true no-op --
    // no `mcpServers` field appears in the forwarded request when the
    // client didn't send one either.
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", observing_backend_spec());
    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "profiles/create",
            "params": {"name": "plain", "agent_id": "stand-in-agent"}
        }))
        .await
        .expect("profiles/create");

    let response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": {"cwd": "/tmp", "_acpx": {"profile": "plain"}}
        }))
        .await
        .expect("session/new");
    assert_eq!(response["result"]["sawCentralFs"], false);
    assert_eq!(response["result"]["sawClientGit"], false);
}
