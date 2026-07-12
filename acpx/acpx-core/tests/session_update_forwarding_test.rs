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
    // contract documented on `attach_updates` -- every synthetic stand-in
    // backend used elsewhere in this workspace's test suite never emits
    // notifications, so their responses must stay byte-for-byte identical
    // (no stray empty `_acpx` object appearing out of nowhere).
    let mut router = Router::new("streaming-agent");
    router.register_agent("streaming-agent", stand_in_streaming_backend_spec());

    let new_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .await
        .expect("session/new");
    assert!(new_response.get("_acpx").is_none());
}
