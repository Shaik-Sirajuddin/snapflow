//! Closes the "biggest remaining architectural gap" flagged after ACP-
//! compatibility phase 1 (see `COVERAGE.md`): a backend that sends a
//! `session/request_permission` *request* (its own `id` + `method`, not a
//! notification) mid-call used to be silently misclassified as a
//! notification and never answered at all, deadlocking the backend
//! forever (it never gets a reply, so it never emits the *outer* call's
//! own matching response either). This proves the fix: acpx now answers
//! it automatically per `acpx_core::profile::PermissionPolicy`, and the
//! outer call still completes.

use acpx_conductor::SpawnSpec;
use acpx_core::router::Router;
use serde_json::json;

/// Answers `session/new` normally. On `session/prompt`, first sends a
/// real-shaped `session/request_permission` *request* (id `999`, picked
/// to never collide with a client-issued request id in this test) with
/// both an `allow_once` and a `reject_once` option, then blocks reading
/// more lines from its own stdin until it sees a reply whose `id` is
/// `999` -- exactly the real dependency a real ACP adapter has (it won't,
/// can't, produce the outer call's result until it gets an answer) -- and
/// only then answers the original `session/prompt` call. If acpx never
/// replies to the `999` id, this script (and thus the whole test) hangs
/// forever, which is exactly the bug this file exists to catch.
const STAND_IN_PERMISSION_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"session/new"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  elif echo "$line" | grep -q '"method":"session/prompt"'; then
    printf '{"jsonrpc":"2.0","id":999,"method":"session/request_permission","params":{"sessionId":"backend-abc","toolCall":{"toolCallId":"call-1"},"options":[{"optionId":"allow-once","name":"Allow once","kind":"allow_once"},{"optionId":"reject-once","name":"Reject","kind":"reject_once"}]}}\n'
    while IFS= read -r reply_line; do
      echo "$reply_line" | grep -q '"id":999' && break
    done
    printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;

fn stand_in_permission_backend_spec() -> SpawnSpec {
    SpawnSpec::new(
        "sh",
        vec![
            "-c".to_string(),
            STAND_IN_PERMISSION_BACKEND_SCRIPT.to_string(),
        ],
    )
}

#[tokio::test]
async fn session_prompt_auto_rejects_permission_request_by_default_and_still_completes() {
    let mut router = Router::new("permission-agent");
    router.register_agent("permission-agent", stand_in_permission_backend_spec());

    let new_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .await
        .expect("session/new");
    let gateway_id = new_response["result"]["sessionId"].as_str().unwrap();

    // No `_acpx.profile` -- native/unmanaged mode, so the default
    // `PermissionPolicy::AutoReject` applies (see that type's doc
    // comment). Not hanging at all, and completing with the backend's
    // real post-permission-answer result, is the primary assertion here.
    // Wrapped in a timeout rather than a bare `.await`: a regression of
    // the fix this test exists to catch is a genuine infinite hang (the
    // stand-in backend's own inner `while read` loop never breaks), which
    // would otherwise wedge this whole test binary rather than failing
    // it.
    let prompt_response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        router.dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
            "params": {"sessionId": gateway_id, "prompt": []}
        })),
    )
    .await
    .expect("session/prompt must not hang once acpx answers the permission request")
    .expect("session/prompt");
    assert_eq!(prompt_response["result"]["stopReason"], json!("end_turn"));

    let agent_requests = prompt_response["_acpx"]["agentRequests"]
        .as_array()
        .expect("agentRequests recorded");
    assert_eq!(agent_requests.len(), 1);
    assert_eq!(
        agent_requests[0]["reply"]["result"]["outcome"],
        json!({"outcome": "selected", "optionId": "reject-once"})
    );
}

#[tokio::test]
async fn session_prompt_auto_allows_permission_request_when_profile_opts_in() {
    let mut router = Router::new("permission-agent");
    router.register_agent("permission-agent", stand_in_permission_backend_spec());

    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "profiles/create",
            "params": {
                "name": "yolo",
                "agent_id": "permission-agent",
                "permission_policy": "auto_allow"
            }
        }))
        .await
        .expect("profiles/create");

    let new_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": {"cwd": "/tmp", "_acpx": {"profile": "yolo"}}
        }))
        .await
        .expect("session/new");
    let gateway_id = new_response["result"]["sessionId"].as_str().unwrap();

    let prompt_response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        router.dispatch(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": {"sessionId": gateway_id, "prompt": []}
        })),
    )
    .await
    .expect("session/prompt must not hang under the profile's auto_allow policy either")
    .expect("session/prompt");
    assert_eq!(prompt_response["result"]["stopReason"], json!("end_turn"));
    assert_eq!(
        prompt_response["_acpx"]["agentRequests"][0]["reply"]["result"]["outcome"],
        json!({"outcome": "selected", "optionId": "allow-once"})
    );
}
