//! **ACP compatibility phase 14.** Proves `dispatch_shared`'s new live
//! `session/update` delivery path (`router::LiveNotifyCtx`/`crate::notify::
//! NotificationHub`) end to end at the `Router` level, without needing a
//! real transport: a real synthetic stand-in backend that streams two
//! `session/update` notifications before answering `session/prompt`
//! (same shape as `session_update_forwarding_test.rs`'s stand-in, which
//! proves the pre-existing `_acpx.updates` buffering fallback still
//! works when nothing is subscribed).

use acpx_conductor::SpawnSpec;
use acpx_core::router::{dispatch_shared, Router, SharedRouterHandle};
use acpx_core::TenantId;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

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

fn shared_router(agent_id: &str) -> SharedRouterHandle {
    let mut router = Router::new(agent_id);
    router.register_agent(agent_id, stand_in_streaming_backend_spec());
    Arc::new(Mutex::new(router))
}

#[tokio::test]
async fn a_subscribed_session_receives_updates_live_and_the_response_carries_no_bundle() {
    let router = shared_router("streaming-agent");

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

    // Subscribe *before* the prompt call, exactly like a real WS/stdio
    // transport would once it learns the gateway session id -- this is
    // the crux of the whole phase: live delivery has to arrive *during*
    // the call, not just be recoverable afterward.
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

    // The two streamed chunks arrive on the live channel, in order, with
    // their `sessionId` translated from the backend-native id to the
    // gateway id (never the raw `backend-abc`) -- a subscriber has no use
    // for a backend-native id, only a real transport downstream client's
    // notification frame would.
    let first = rx.recv().await.expect("first live update");
    assert_eq!(first["params"]["update"]["content"]["text"], json!("Hello"));
    assert_eq!(first["params"]["sessionId"], json!(gateway_id));
    let second = rx.recv().await.expect("second live update");
    assert_eq!(
        second["params"]["update"]["content"]["text"],
        json!(", world")
    );
    assert_eq!(second["params"]["sessionId"], json!(gateway_id));

    // Delivered live means NOT also bundled into `_acpx.updates` -- a
    // subscribed client must never see the same update twice.
    assert!(prompt_response["_acpx"].get("updates").is_none());
    assert_eq!(prompt_response["result"]["stopReason"], json!("end_turn"));
}

#[tokio::test]
async fn an_unsubscribed_session_still_falls_back_to_the_acpx_updates_bundle() {
    // No live subscriber at all this time -- the pre-phase-14 contract
    // (`session_update_forwarding_test.rs`'s own assertion, re-proved
    // here through `dispatch_shared` specifically, the path that test
    // doesn't exercise) must still hold unmodified.
    let router = shared_router("streaming-agent-2");

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

    let prompt_response = dispatch_shared(
        &router,
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
            "params": {"sessionId": gateway_id, "prompt": [{"type": "text", "text": "hi"}]}
        }),
    )
    .await
    .expect("session/prompt");

    let updates = prompt_response["_acpx"]["updates"]
        .as_array()
        .expect("_acpx.updates present when nothing was subscribed");
    assert_eq!(updates.len(), 2);
    assert_eq!(
        updates[0]["params"]["update"]["content"]["text"],
        json!("Hello")
    );
}

#[tokio::test]
async fn unsubscribing_mid_stream_falls_back_to_buffering_for_the_rest_of_that_call() {
    // A defensive edge case: if a subscriber is removed (e.g. the
    // connection just dropped) partway through -- not exercised by
    // either test above, both of which keep one state for the whole
    // call -- the notifications that arrive afterward must still be
    // recoverable via `_acpx.updates`, not silently lost, since
    // `try_deliver_live` only skips buffering when it actually
    // succeeds at live delivery.
    let router = shared_router("streaming-agent-3");

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

    let hub = { router.lock().await.notification_hub() };
    // Subscribe, then immediately unsubscribe -- simulates the
    // subscriber having already gone away by the time any update
    // actually arrives.
    let _rx = hub
        .subscribe(&TenantId::default(), gateway_id.clone())
        .await;
    hub.remove_stream(&TenantId::default(), &gateway_id).await;

    let prompt_response = dispatch_shared(
        &router,
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
            "params": {"sessionId": gateway_id, "prompt": [{"type": "text", "text": "hi"}]}
        }),
    )
    .await
    .expect("session/prompt");

    let updates = prompt_response["_acpx"]["updates"]
        .as_array()
        .expect("_acpx.updates present -- nothing silently lost");
    assert_eq!(updates.len(), 2);
}

/// The multiplex-management guard for this whole phase: a long-lived
/// streaming `session/prompt` call against one backend (slow to answer,
/// simulated with `sleep`) must not block a concurrent, unrelated
/// `session/new` against a *different* backend -- `try_deliver_live`'s
/// brief per-notification `router.lock().await` (inside `read_matching_
/// response`'s loop, itself already running under `dispatch_proxied_
/// shared`'s established "release the router lock before backend I/O"
/// pattern) must stay exactly that: brief, not held across the whole
/// slow call. Mirrors `session_list_real_shared_test.rs`'s own
/// `...does_not_block_a_concurrent_different_backend_call` test.
#[tokio::test]
async fn a_live_streaming_session_does_not_block_a_concurrent_different_backend_call() {
    let slow_streaming_script = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"slow-backend-abc"}}\n' "$id"
  elif echo "$line" | grep -q 'session/prompt'; then
    printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"slow-backend-abc","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"chunk-1"}}}}\n'
    sleep 0.3
    printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;
    let fast_script = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/list'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessions":[]}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"fast-backend-1"}}\n' "$id"
  fi
done
"#;

    // `default_agent_id` is `slow-streaming-agent` so the plain,
    // unqualified `session/new`/`session/prompt` calls below (`session/
    // new` has no `_acpx.agentId` selector -- only `session/list` does,
    // see `SessionListSelector`) land on it without needing a profile.
    // The concurrent "unrelated, fast backend" call uses `session/list`'s
    // `_acpx.agentId` selector to target `fast-agent` directly, the same
    // proven pattern `session_list_real_shared_test.rs`'s own concurrency
    // test uses -- still a real, independent `dispatch_shared` round trip
    // against a genuinely different backend process, which is the
    // property this test exists to guard.
    let mut router = Router::new("slow-streaming-agent");
    router.register_agent(
        "slow-streaming-agent",
        SpawnSpec::new(
            "sh",
            vec!["-c".to_string(), slow_streaming_script.to_string()],
        ),
    );
    router.register_agent(
        "fast-agent",
        SpawnSpec::new("sh", vec!["-c".to_string(), fast_script.to_string()]),
    );
    let router: SharedRouterHandle = Arc::new(Mutex::new(router));

    let new_response = dispatch_shared(
        &router,
        json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}}),
    )
    .await
    .expect("session/new against slow-streaming-agent");
    let gateway_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let hub = { router.lock().await.notification_hub() };
    let mut rx = hub
        .subscribe(&TenantId::default(), gateway_id.clone())
        .await;

    let slow_router = Arc::clone(&router);
    let slow_gateway_id = gateway_id.clone();
    let slow_task = tokio::spawn(async move {
        dispatch_shared(
            &slow_router,
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
                "params": {"sessionId": slow_gateway_id, "prompt": [{"type": "text", "text": "hi"}]}
            }),
        )
        .await
    });

    // Wait for the live chunk to actually arrive -- proof the slow call
    // has started and is genuinely mid-flight (past its own `session/new`
    // resolution, writing to its backend, blocked in `read_matching_
    // response`'s loop) before firing the fast, unrelated call.
    let chunk = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("live chunk should arrive promptly")
        .expect("live chunk");
    assert_eq!(
        chunk["params"]["update"]["content"]["text"],
        json!("chunk-1")
    );

    let started = std::time::Instant::now();
    let fast_response = dispatch_shared(
        &router,
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/list",
            "params": {"_acpx": {"agentId": "fast-agent"}}
        }),
    )
    .await
    .expect("session/list against the fast, unrelated backend");
    let fast_elapsed = started.elapsed();

    assert!(fast_response["result"]["sessions"].is_array());
    assert!(
        fast_elapsed < Duration::from_millis(150),
        "session/list against an unrelated, fast backend took {fast_elapsed:?} while a \
         concurrent live-streaming session/prompt against a slow backend was still in \
         flight -- the router lock was held too long during the live-notification path"
    );

    let slow_result = tokio::time::timeout(Duration::from_secs(5), slow_task)
        .await
        .expect("slow session/prompt task should not hang")
        .expect("task join")
        .expect("session/prompt against slow-streaming-agent");
    assert_eq!(slow_result["result"]["stopReason"], json!("end_turn"));
}
