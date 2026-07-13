//! ACP compatibility hardening, phase 7: `session/cancel`. Real ACP
//! schema (agentclientprotocol.com/protocol/schema): `CancelNotification`
//! is a client-sent *notification* (no `id`), and a spec-compliant agent
//! never replies to it directly -- the already-in-flight `session/prompt`
//! call it's meant to interrupt is what eventually resolves, with
//! `stopReason: "cancelled"`. Before this phase, `session/cancel` was
//! routed through the same generic `Proxied` path as every other
//! session method: (1) unconditionally required an `id`
//! (`RouterError::MissingId` otherwise -- rejecting a spec-compliant
//! true notification before it ever reached a backend), and (2) blocked
//! on `read_matching_response` waiting for a reply the backend is never
//! supposed to send (a genuine deadlock against any backend that
//! implements the spec correctly). This workspace had zero tests
//! exercising `session/cancel` at all before this phase, despite the
//! spec calling it out as one of four baseline-MUST methods.
//!
//! The third, deeper bug (routing this through the same per-process lock
//! as every other proxied call would make cancellation practically
//! useless even once (1)/(2) are fixed, since an in-flight
//! `session/prompt` holds that lock for its entire duration) needs true
//! concurrency to prove -- see `acpx-server/tests/
//! session_cancel_concurrency_test.rs` for that. This file covers
//! everything provable against the simpler non-shared `Router::dispatch`
//! (in-process, no real concurrency needed): the exact wire shape acpx
//! sends, and that a silent backend doesn't hang the caller.

use acpx_core::router::{Router, RouterError};
use serde_json::json;
use std::time::Duration;

/// Answers `session/new` normally. On any line containing
/// `"method":"session/cancel"`, appends that raw line verbatim to
/// `capture_path` (so the test can inspect exactly what acpx sent over
/// the wire) and deliberately sends **no reply at all** -- matching the
/// real spec's "agent never replies to a cancel notification" behavior
/// byte-for-byte, which is exactly the shape that would hang a
/// regressed implementation.
fn stand_in_cancel_backend_script(capture_path: &str) -> String {
    format!(
        r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"session/new"'; then
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"sessionId":"backend-abc"}}}}\n' "$id"
  elif echo "$line" | grep -q '"method":"session/cancel"'; then
    echo "$line" >> {capture_path}
  else
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"ok":true}}}}\n' "$id"
  fi
done
"#
    )
}

fn stand_in_cancel_backend_spec(capture_path: &str) -> acpx_conductor::SpawnSpec {
    acpx_conductor::SpawnSpec::new(
        "sh",
        vec![
            "-c".to_string(),
            stand_in_cancel_backend_script(capture_path),
        ],
    )
}

fn unique_capture_path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "acpx-session-cancel-test-{label}-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ))
}

#[tokio::test]
async fn session_cancel_as_a_true_notification_with_no_id_completes_without_hanging() {
    let capture_path = unique_capture_path("no-id");
    let mut router = Router::new("cancel-agent");
    router.register_agent(
        "cancel-agent",
        stand_in_cancel_backend_spec(capture_path.to_str().unwrap()),
    );

    let new_response = router
        .dispatch(
            json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}}),
        )
        .await
        .expect("session/new");
    let gateway_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    // A real spec-compliant client sends this with no `id` field at all
    // -- not even `null`, the key is simply absent.
    let cancel_response = tokio::time::timeout(
        Duration::from_secs(5),
        router.dispatch(json!({
            "jsonrpc": "2.0", "method": "session/cancel",
            "params": {"sessionId": gateway_id}
        })),
    )
    .await
    .expect("session/cancel must not hang even though the backend never replies to it")
    .expect("session/cancel");

    assert_eq!(cancel_response["result"], json!({}));
    // No `id` was sent, so acpx echoes `null` back.
    assert_eq!(cancel_response["id"], serde_json::Value::Null);

    // Give the backend's own line-buffered write a moment to land on
    // disk before reading it back.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let captured =
        std::fs::read_to_string(&capture_path).expect("backend captured the cancel notification");
    let _ = std::fs::remove_file(&capture_path);
    assert!(
        captured.contains("backend-abc"),
        "expected the rewritten backend sessionId in the captured line, got: {captured}"
    );
    // The real ACP `CancelNotification` shape has no `id` key at all --
    // acpx must send exactly that, regardless of what the client itself
    // sent (or, as here, didn't send).
    assert!(
        !captured.contains("\"id\""),
        "session/cancel forwarded to the backend must have no \"id\" key (it's a notification \
         per the real ACP schema), got: {captured}"
    );
}

#[tokio::test]
async fn session_cancel_strips_the_id_before_forwarding_even_if_the_client_sent_one() {
    // Some real client SDKs may still attach an `id` to what's
    // semantically a notification (e.g. a generic JSON-RPC library that
    // always assigns one). acpx must still forward the real, id-less ACP
    // shape to the backend -- the client's own `id`, if any, is only
    // ever used for acpx's own reply to *that* client.
    let capture_path = unique_capture_path("with-id");
    let mut router = Router::new("cancel-agent");
    router.register_agent(
        "cancel-agent",
        stand_in_cancel_backend_spec(capture_path.to_str().unwrap()),
    );

    let new_response = router
        .dispatch(
            json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}}),
        )
        .await
        .expect("session/new");
    let gateway_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let cancel_response = tokio::time::timeout(
        Duration::from_secs(5),
        router.dispatch(json!({
            "jsonrpc": "2.0", "id": 99, "method": "session/cancel",
            "params": {"sessionId": gateway_id}
        })),
    )
    .await
    .expect("must not hang")
    .expect("session/cancel");

    assert_eq!(cancel_response["result"], json!({}));
    // The client's own id is echoed back to *it* -- that part is acpx's
    // own client-facing contract, not the backend-facing ACP shape.
    assert_eq!(cancel_response["id"], json!(99));

    tokio::time::sleep(Duration::from_millis(100)).await;
    let captured =
        std::fs::read_to_string(&capture_path).expect("backend captured the cancel notification");
    let _ = std::fs::remove_file(&capture_path);
    assert!(
        !captured.contains("\"id\""),
        "session/cancel forwarded to the backend must have no \"id\" key regardless of what \
         the client sent, got: {captured}"
    );
}

#[tokio::test]
async fn session_cancel_against_an_unknown_session_is_a_clear_error() {
    let mut router = Router::new("cancel-agent");
    router.register_agent(
        "cancel-agent",
        stand_in_cancel_backend_spec(unique_capture_path("unused").to_str().unwrap()),
    );

    let result = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/cancel",
            "params": {"sessionId": "never-existed"}
        }))
        .await;
    assert!(matches!(result, Err(RouterError::UnknownSession(_))));
}
