//! **ACP compatibility phase 15.** Proves `router::backend_idle_
//! scavenger` (spawned via `Router::spawn_idle_scavenger_if_new`, wired
//! into every `dispatch_shared` path that calls `Supervisor::
//! ensure_running`) actually closes the gap phase 14 documented and left
//! open: a `session/update` a backend emits while *no client call is in
//! flight against it* must still reach a live subscriber, without
//! requiring any further call to that backend to "flush" it off the pipe.
//!
//! The synthetic stand-in backend below answers `session/prompt`
//! immediately, then emits its `session/update` from a backgrounded
//! subshell *after* that response -- by the time the notification is
//! actually written to stdout, `read_matching_response`'s loop for that
//! call has already returned (it matched the response's `id` and exited
//! immediately), so nothing but the idle scavenger is ever positioned to
//! read it.

use acpx_conductor::SpawnSpec;
use acpx_core::router::{dispatch_shared, Router, SharedRouterHandle};
use acpx_core::TenantId;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Answers `session/prompt` right away, then -- from a backgrounded `(
/// ... ) &` subshell, so the main read loop is free to return to `read
/// -r line` immediately rather than blocking on the `sleep` -- emits one
/// `session/update` notification after a short delay with nothing else
/// ever calling this backend again.
const STAND_IN_DELAYED_NOTIFICATION_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-idle-1"}}\n' "$id"
  elif echo "$line" | grep -q 'session/prompt'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
    (sleep 0.2; printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"backend-idle-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"idle-update"}}}}\n') &
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;

fn shared_router(agent_id: &str) -> SharedRouterHandle {
    let mut router = Router::new(agent_id);
    router.register_agent(
        agent_id,
        SpawnSpec::new(
            "sh",
            vec![
                "-c".to_string(),
                STAND_IN_DELAYED_NOTIFICATION_BACKEND_SCRIPT.to_string(),
            ],
        ),
    );
    Arc::new(Mutex::new(router))
}

#[tokio::test]
async fn an_idle_notification_between_calls_still_reaches_a_live_subscriber_without_a_further_call()
{
    let router = shared_router("idle-scavenger-agent-1");

    let new_response = dispatch_shared(
        &router,
        json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}}),
    )
    .await
    .expect("session/new");
    let gateway_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    // Subscribe right after `session/new`'s response, exactly like a real
    // WS/stdio transport does (`transport::live::session_id_to_watch`) --
    // this is the subscription the idle scavenger's later `try_deliver_
    // live` call needs already in place.
    let hub = { router.lock().await.notification_hub() };
    let mut rx = hub
        .subscribe(&TenantId::default(), gateway_id.clone())
        .await;

    let prompt_response = dispatch_shared(
        &router,
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
            "params": {"sessionId": gateway_id, "prompt": [{"type": "text", "text": "hi"}]}
        }),
    )
    .await
    .expect("session/prompt");

    // The call itself returned with no bundled updates at all -- the
    // backend hadn't emitted anything yet by the time `read_matching_
    // response` matched this call's own response id and returned. If
    // phase 15 didn't exist, the notification emitted moments later would
    // simply sit unread in the pipe forever, since nothing else ever
    // calls this backend again in this test.
    assert!(prompt_response["_acpx"].get("updates").is_none());

    // No further call is ever made against this backend from here on --
    // only the idle scavenger task is left reading its stdout. The
    // notification must still arrive live.
    let update = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("idle scavenger should deliver the delayed update without any further call")
        .expect("live update");
    assert_eq!(
        update["params"]["update"]["content"]["text"],
        json!("idle-update")
    );
    assert_eq!(update["params"]["sessionId"], json!(gateway_id));
}

#[tokio::test]
async fn an_idle_notification_with_no_live_subscriber_is_discarded_without_wedging_the_backend() {
    // No subscription this time -- the idle scavenger has nothing to
    // deliver the update to and must discard it (see `backend_idle_
    // scavenger`'s doc comment), but discarding it must not leave the
    // backend's process lock stuck or otherwise prevent a later real
    // call from going through cleanly.
    let router = shared_router("idle-scavenger-agent-2");

    let new_response = dispatch_shared(
        &router,
        json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}}),
    )
    .await
    .expect("session/new");
    let gateway_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    dispatch_shared(
        &router,
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
            "params": {"sessionId": gateway_id, "prompt": [{"type": "text", "text": "hi"}]}
        }),
    )
    .await
    .expect("session/prompt");

    // Give the backgrounded delayed notification time to be emitted and
    // (silently, since nothing is subscribed) drained by the scavenger.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // A second, ordinary call against the same backend must still work
    // normally -- proof the scavenger's brief `try_lock` windows never
    // leave the process lock stuck or the stream desynchronized.
    let second_prompt = dispatch_shared(
        &router,
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": {"sessionId": gateway_id, "prompt": [{"type": "text", "text": "again"}]}
        }),
    )
    .await
    .expect("a second call against the same backend after an idle-discarded update");
    assert_eq!(second_prompt["result"]["stopReason"], json!("end_turn"));
}
