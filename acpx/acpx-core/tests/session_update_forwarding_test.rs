//! Proves the reverse-direction routing fix (see
//! `acpx_core::router::read_matching_response`'s doc comment and
//! `acpx/COVERAGE.md`'s "real ACP content delivery" section): a backend
//! that emits `session/update` notifications *before* answering a
//! `session/prompt` request -- exactly how real ACP adapters stream
//! assistant reply text, verified against `@agentclientprotocol/
//! claude-agent-acp` -- no longer has those notifications silently
//! dropped. They surface in the JSON-RPC response envelope's
//! `_acpx.updates` array.

use acpx_conductor::SpawnSpec;
use acpx_core::router::Router;
use serde_json::json;

/// Answers `session/new` normally, but for `session/prompt` first emits
/// two `session/update` notifications (no `id` field, so they'd never
/// match a pending request) before the actual matching result -- the
/// exact shape a real streaming ACP adapter produces.
const STAND_IN_STREAMING_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  elif echo "$line" | grep -q 'session/prompt'; then
    printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"backend-abc","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hello"}}}}\n'
    printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"backend-abc","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":", world"}}}}\n'
    printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;

fn stand_in_streaming_backend_spec() -> SpawnSpec {
    SpawnSpec::new(
        "sh",
        vec![
            "-c".to_string(),
            STAND_IN_STREAMING_BACKEND_SCRIPT.to_string(),
        ],
    )
}

#[tokio::test]
async fn session_prompt_response_aggregates_streamed_session_updates() {
    let mut router = Router::new("streaming-agent");
    router.register_agent("streaming-agent", stand_in_streaming_backend_spec());

    let new_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .await
        .expect("session/new");
    let gateway_id = new_response["result"]["sessionId"].as_str().unwrap();

    let prompt_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
            "params": {"sessionId": gateway_id, "prompt": [{"type": "text", "text": "hi"}]}
        }))
        .await
        .expect("session/prompt");

    // The final result is untouched (still just what the backend's
    // matching response carried).
    assert_eq!(prompt_response["result"]["stopReason"], json!("end_turn"));

    // But the two streamed notifications that arrived first are no longer
    // dropped -- they're aggregated in order under `_acpx.updates`.
    let updates = prompt_response["_acpx"]["updates"]
        .as_array()
        .expect("_acpx.updates present");
    assert_eq!(updates.len(), 2);
    assert_eq!(updates[0]["method"], json!("session/update"));
    assert_eq!(
        updates[0]["params"]["update"]["content"]["text"],
        json!("Hello")
    );
    assert_eq!(
        updates[1]["params"]["update"]["content"]["text"],
        json!(", world")
    );
}

#[tokio::test]
async fn session_new_response_has_no_acpx_updates_field_when_backend_emits_none() {
    // Regression guard for the "no-op when there's nothing to attach"
    // contract documented on `attach_updates`/`attach_session_new_extras`
    // -- every synthetic stand-in backend used elsewhere in this
    // workspace's test suite never emits `session/update` notifications,
    // so `_acpx.updates` must never appear out of nowhere. `_acpx` itself
    // is no longer guaranteed absent, though: since acpx now performs a
    // real ACP `initialize` handshake and captures whatever `result` the
    // backend answers with as `_acpx.agentCapabilities` (see
    // `ensure_backend_initialized`'s doc comment -- this closes a real
    // ACP-compatibility gap, not a test artifact), and this stand-in's
    // generic `{"ok": true}` reply to `initialize` becomes that captured
    // value, `_acpx.agentCapabilities` is expected to be present here.
    let mut router = Router::new("streaming-agent");
    router.register_agent("streaming-agent", stand_in_streaming_backend_spec());

    let new_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .await
        .expect("session/new");
    assert!(new_response["_acpx"].get("updates").is_none());
    assert_eq!(
        new_response["_acpx"]["agentCapabilities"],
        json!({"ok": true})
    );
}

/// Answers `initialize` with a realistic-shaped `agentCapabilities`
/// object (distinguishable from every other stand-in script's generic
/// `{"ok": true}` reply), and everything else like the plain stand-in
/// backend above.
const STAND_IN_CAPABILITIES_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"initialize"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true,"promptCapabilities":{"image":true}},"authMethods":[]}}\n' "$id"
  elif echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;

fn stand_in_capabilities_backend_spec() -> acpx_conductor::SpawnSpec {
    acpx_conductor::SpawnSpec::new(
        "sh",
        vec![
            "-c".to_string(),
            STAND_IN_CAPABILITIES_BACKEND_SCRIPT.to_string(),
        ],
    )
}

#[tokio::test]
async fn session_new_surfaces_the_backends_real_initialize_capabilities() {
    // Closes the "initialize response discarded" ACP-compatibility gap:
    // acpx used to perform the handshake purely to unblock `session/new`,
    // throwing away everything the backend actually said it supports.
    let mut router = Router::new("capable-agent");
    router.register_agent("capable-agent", stand_in_capabilities_backend_spec());

    let first = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .await
        .expect("session/new");
    assert_eq!(
        first["_acpx"]["agentCapabilities"],
        json!({
            "protocolVersion": 1,
            "agentCapabilities": {"loadSession": true, "promptCapabilities": {"image": true}},
            "authMethods": [],
        })
    );

    // A second `session/new` against the same still-running backend
    // process must keep surfacing the same captured capabilities -- the
    // real `initialize` handshake only ever happens once per process
    // (`BackendProcess::handshake_done`), so this proves the captured
    // value survives past the one call that triggered it, not just a
    // one-shot side effect of the handshake itself.
    let second = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .await
        .expect("second session/new");
    assert_eq!(
        second["_acpx"]["agentCapabilities"],
        first["_acpx"]["agentCapabilities"]
    );
}
