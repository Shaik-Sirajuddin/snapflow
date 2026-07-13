//! ACP compatibility hardening, phase 6: acpx's own client-facing
//! `initialize`/`authenticate` handshake -- distinct from
//! `authenticate_test.rs` (phase 5, backend-facing: acpx calling out
//! to a spawned agent process) and `fs_request_test.rs`/
//! `terminal_request_test.rs` (agent-initiated requests acpx answers
//! mid-call). This is acpx itself answering as the ACP agent its own
//! clients think they're talking to -- the very first request any real
//! spec-compliant ACP editor/IDE sends, before `session/new` is ever
//! reached. Before this phase, `initialize`/`authenticate` fell through
//! `classify`'s `_ => MethodClass::Unknown`, so this exact opening
//! handshake would have failed immediately against every transport.
//!
//! Also verifies a second phase-6 recheck item from phase 5's list:
//! whether `permission_policy`/`allow_fs_access`/`allow_terminal_access`/
//! `auth_method_id` are exposed as first-class `profiles/*` response
//! fields (not just inline on `session/new`). Turns out this was
//! already true by construction -- `Profile` derives `Serialize` on
//! every `pub` field with no `#[serde(skip)]` anywhere, so
//! `profiles/create`'s response (which serializes the whole stored
//! `Profile`, see `redact_launch_overrides`'s call site in
//! `router.rs`) already included them the moment each field was added
//! in phases 3/4/5 -- nobody had verified it with an actual assertion
//! until now.

use acpx_core::router::{Router, RouterError};
use serde_json::json;

#[tokio::test]
async fn initialize_declares_real_capabilities_not_the_unknown_method_error() {
    let mut router = Router::new("unused-agent");

    let response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": 1, "clientCapabilities": {}}
        }))
        .await
        .expect("initialize");

    assert_eq!(response["result"]["protocolVersion"], json!(1));
    assert_eq!(response["result"]["authMethods"], json!([]));
    assert_eq!(
        response["result"]["agentCapabilities"]["loadSession"],
        json!(true)
    );
    assert_eq!(
        response["result"]["agentCapabilities"]["promptCapabilities"]["image"],
        json!(true)
    );
    assert_eq!(response["result"]["agentInfo"]["name"], json!("acpx"));

    // No backend process was ever spawned or even registered for this
    // router -- proves `initialize` really is gateway-native (Router::
    // classify's new `MethodClass::GatewayNative` arm), not accidentally
    // routed toward a nonexistent backend.
}

#[tokio::test]
async fn authenticate_is_refused_with_a_clear_error_since_initialize_advertises_no_auth_methods() {
    let mut router = Router::new("unused-agent");

    let result = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "authenticate",
            "params": {"methodId": "oauth-personal"}
        }))
        .await;

    match result {
        Err(RouterError::NoAuthMethodsAdvertised(Some(method_id))) => {
            assert_eq!(method_id, "oauth-personal");
        }
        other => panic!("expected NoAuthMethodsAdvertised, got {other:?}"),
    }
}

#[tokio::test]
async fn session_new_still_works_without_ever_calling_initialize_first() {
    // Native/unmanaged-mode clients (every pre-phase-6 test in this
    // workspace) never call `initialize` against acpx's own endpoint at
    // all -- only `ensure_backend_initialized`'s *backend*-facing
    // handshake happens, transparently, on first use. This proves phase
    // 6 is additive: `initialize` becoming a real, answerable method
    // doesn't make it a hard prerequisite acpx now enforces before
    // `session/new`.
    let script = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"session/new"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;
    let mut router = Router::new("agent-a");
    router.register_agent(
        "agent-a",
        acpx_conductor::SpawnSpec::new("sh", vec!["-c".to_string(), script.to_string()]),
    );

    let response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .await
        .expect("session/new without ever calling initialize first");
    assert!(response["result"]["sessionId"].as_str().is_some());
}

#[tokio::test]
async fn profiles_create_response_exposes_permission_and_access_fields_first_class() {
    let mut router = Router::new("agent-a");

    let response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "profiles/create",
            "params": {
                "name": "checked-profile",
                "agent_id": "agent-a",
                "permission_policy": "auto_allow",
                "allow_fs_access": true,
                "allow_terminal_access": true,
                "auth_method_id": "api-key"
            }
        }))
        .await
        .expect("profiles/create");

    assert_eq!(response["result"]["permission_policy"], json!("auto_allow"));
    assert_eq!(response["result"]["allow_fs_access"], json!(true));
    assert_eq!(response["result"]["allow_terminal_access"], json!(true));
    assert_eq!(response["result"]["auth_method_id"], json!("api-key"));

    // Same fields, same values, via `profiles/list` too -- not just the
    // `profiles/create` response's own echo.
    let list_response = router
        .dispatch(json!({"jsonrpc": "2.0", "id": 2, "method": "profiles/list", "params": {}}))
        .await
        .expect("profiles/list");
    let listed = list_response["result"]["profiles"]
        .as_array()
        .expect("profiles array")
        .iter()
        .find(|p| p["name"] == json!("checked-profile"))
        .expect("checked-profile listed");
    assert_eq!(listed["permission_policy"], json!("auto_allow"));
    assert_eq!(listed["allow_fs_access"], json!(true));
    assert_eq!(listed["allow_terminal_access"], json!(true));
    assert_eq!(listed["auth_method_id"], json!("api-key"));
}
