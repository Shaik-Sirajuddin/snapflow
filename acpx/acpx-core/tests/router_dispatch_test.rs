//! Integration test for `acpx_core::router::Router::dispatch` against a
//! tiny synthetic stand-in "backend" (a `sh` one-liner), since a real ACP
//! adapter (codex-acp/claude-agent-acp) isn't guaranteed to be installed
//! or logged in during CI. The script echoes back a crafted `session/new`
//! result carrying a `sessionId`, and a generic `{"ok": true}` result for
//! anything else -- enough to exercise the hybrid `session/new` ->
//! gateway-id-registration path and the proxied `session/prompt` ->
//! session-resolution path end to end.

use acpx_conductor::SpawnSpec;
use acpx_core::router::Router;
use serde_json::json;

/// Reads newline-delimited JSON-RPC requests, replies with a canned
/// `session/new` result (fixed backend session id `backend-abc`) or a
/// generic `{"ok": true}` result for any other method, always echoing the
/// request's own `id`.
const STAND_IN_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;

fn stand_in_backend_spec() -> SpawnSpec {
    SpawnSpec::new(
        "sh",
        vec!["-c".to_string(), STAND_IN_BACKEND_SCRIPT.to_string()],
    )
}

#[tokio::test]
async fn session_new_registers_gateway_id_and_hides_backend_id() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/new",
        "params": {"cwd": "/tmp"}
    });
    let response = router
        .dispatch(request)
        .await
        .expect("session/new dispatch");

    let session_id = response["result"]["sessionId"]
        .as_str()
        .expect("sessionId present");
    // The client must never see the backend's own session id -- a fresh
    // gateway-issued id is substituted in place.
    assert_ne!(session_id, "backend-abc");
}

#[tokio::test]
async fn session_prompt_resolves_gateway_id_to_backend_id() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());

    let new_request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/new",
        "params": {"cwd": "/tmp"}
    });
    let new_response = router.dispatch(new_request).await.expect("session/new");
    let gateway_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let prompt_request = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/prompt",
        "params": {"sessionId": gateway_id, "prompt": [{"type": "text", "text": "hi"}]}
    });
    let prompt_response = router
        .dispatch(prompt_request)
        .await
        .expect("session/prompt");
    assert_eq!(prompt_response["result"]["ok"], json!(true));
}

#[tokio::test]
async fn session_prompt_with_unknown_session_errors() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());

    let prompt_request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/prompt",
        "params": {"sessionId": "not-a-real-session", "prompt": []}
    });
    assert!(router.dispatch(prompt_request).await.is_err());
}

#[tokio::test]
async fn session_list_aggregates_registered_sessions() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());

    let new_request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/new",
        "params": {"cwd": "/tmp"}
    });
    router.dispatch(new_request).await.expect("session/new");

    let list_request = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/list",
        "params": {}
    });
    let list_response = router.dispatch(list_request).await.expect("session/list");
    let sessions = list_response["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["agentId"], json!("stand-in-agent"));
}
