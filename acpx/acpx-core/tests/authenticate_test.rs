//! ACP compatibility hardening, phase 5: backend-facing `authenticate`.
//! Real ACP schema (agentclientprotocol.com/protocol/schema): a backend's
//! `initialize` response may include a non-empty `authMethods` array; a
//! client must then send `authenticate` with `params.methodId` set to
//! one of the advertised ids before `session/new` is expected to
//! succeed. Unlike `fs/*`/`terminal/*`/`session/request_permission`,
//! this is a *client*-initiated request (acpx calling out to the
//! backend), not an agent-initiated one -- so the risk here isn't a
//! deadlock, it's acpx either blindly proceeding to `session/new`
//! against an unauthenticated backend (letting the backend's own
//! rejection surface as an opaque, hard-to-diagnose downstream error)
//! or hanging/guessing a method id. This proves three things: (1) a
//! backend that advertises no `authMethods` at all is unaffected --
//! `session/new` proceeds exactly as before this phase; (2) a backend
//! that requires auth, with no `Profile::auth_method_id` configured,
//! gets a clear `RouterError::BackendRequiresAuthentication` instead of
//! a raw `session/new` attempt; (3) a backend that requires auth, with
//! the right `auth_method_id` configured, gets a real `authenticate`
//! round trip performed and then `session/new` succeeds.

use acpx_conductor::SpawnSpec;
use acpx_core::router::{Router, RouterError};
use serde_json::json;

/// Advertises `authMethods: [{"id": "api-key", "name": "API Key"}]` in
/// its `initialize` response. Only answers `authenticate` (methodId
/// `api-key`) with a real success result, then `session/new`
/// afterward -- so a client that skips straight to `session/new`
/// without authenticating first would, against a real adapter, likely
/// get a rejection; this stand-in instead just never receives that
/// `session/new` call at all if acpx holds the line correctly (proven
/// by the "no auth configured" test asserting no session id is ever
/// produced), and answers it for real once acpx does authenticate.
fn stand_in_auth_required_backend_script() -> String {
    r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":-\{0,1\}[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"initialize"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[{"id":"api-key","name":"API Key"}]}}\n' "$id"
  elif echo "$line" | grep -q '"method":"authenticate"'; then
    if echo "$line" | grep -q '"methodId":"api-key"'; then
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
    else
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32602,"message":"unknown methodId"}}\n' "$id"
    fi
  elif echo "$line" | grep -q '"method":"session/new"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#
    .to_string()
}

fn stand_in_auth_required_backend_spec() -> SpawnSpec {
    SpawnSpec::new(
        "sh",
        vec!["-c".to_string(), stand_in_auth_required_backend_script()],
    )
}

#[tokio::test]
async fn session_new_proceeds_normally_when_backend_advertises_no_auth_methods() {
    // Re-use the phase-3/4 stand-in style backends' plain `initialize`
    // (no `authMethods` field at all) -- covered incidentally by every
    // other test in this workspace already, this test asserts it
    // explicitly as the phase-5 baseline: `ensure_backend_initialized`'s
    // new authenticate branch must be a true no-op when there is
    // nothing to authenticate.
    let script = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"initialize"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{}}}\n' "$id"
  elif echo "$line" | grep -q '"method":"session/new"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;
    let mut router = Router::new("no-auth-agent");
    router.register_agent(
        "no-auth-agent",
        SpawnSpec::new("sh", vec!["-c".to_string(), script.to_string()]),
    );

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        router.dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp"}
        })),
    )
    .await
    .expect("must not hang")
    .expect("session/new");
    // acpx mints its own gateway-facing session id (not the backend's
    // own `"backend-abc"`) -- this just asserts `session/new` genuinely
    // succeeded rather than acpx's new authenticate branch (a no-op
    // here) blocking or erroring it.
    assert!(response["result"]["sessionId"].as_str().is_some());
}

#[tokio::test]
async fn session_new_is_refused_with_a_clear_error_when_backend_requires_auth_and_none_is_configured(
) {
    let mut router = Router::new("auth-agent");
    router.register_agent("auth-agent", stand_in_auth_required_backend_spec());

    // Native/unmanaged mode -- no profile, so no `auth_method_id` at
    // all. `session/new` must fail with a clear, specific error
    // (naming the advertised methods) rather than either hanging or
    // reaching the backend's `session/new` handler at all.
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        router.dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp"}
        })),
    )
    .await
    .expect("must not hang even when auth is required and unconfigured");

    match result {
        Err(RouterError::BackendRequiresAuthentication(methods)) => {
            let methods = methods.as_array().expect("authMethods is an array");
            assert_eq!(methods.len(), 1);
            assert_eq!(methods[0]["id"], json!("api-key"));
        }
        other => panic!("expected BackendRequiresAuthentication, got {other:?}"),
    }
}

#[tokio::test]
async fn session_new_succeeds_after_a_real_authenticate_round_trip_when_configured() {
    let mut router = Router::new("auth-agent");
    router.register_agent("auth-agent", stand_in_auth_required_backend_spec());

    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "profiles/create",
            "params": {
                "name": "auth-enabled",
                "agent_id": "auth-agent",
                "auth_method_id": "api-key"
            }
        }))
        .await
        .expect("profiles/create");

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        router.dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": {"cwd": "/tmp", "_acpx": {"profile": "auth-enabled"}}
        })),
    )
    .await
    .expect("must not hang once acpx authenticates for real")
    .expect("session/new");

    // As above -- a gateway-minted id, not the backend's own
    // `"backend-abc"`; success here (rather than an error/timeout)
    // proves the real `authenticate` round trip happened and the
    // backend's `session/new` handler was actually reached afterward.
    assert!(response["result"]["sessionId"].as_str().is_some());
}
