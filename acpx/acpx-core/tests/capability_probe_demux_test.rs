//! **Regression: `process_reader_demux` capability-probe panic gap.**
//!
//! Same bug class already closed at 5 other call sites
//! (`dispatch_proxied_shared`, `dispatch_session_fork_shared`,
//! `dispatch_session_list_real_shared`, `backend_idle_scavenger`,
//! `reap_expired_sessions`) but for `Router::probe_adapter_capabilities`
//! specifically -- the one call site those fixes missed, and the hottest
//! one: `acp_bridge::refresh_models` calls it on essentially every live
//! bridge request (gated only by `MODEL_REFRESH_COOLDOWN`), against the
//! *same* per-agent shared backend a real session may already be using.
//! `probe_adapter_capabilities` always sent its own `session/new` and
//! `session/close` probe calls via `read_matching_response`'s
//! unconditional `reader_mut()`, which panics once any earlier live call
//! on this same shared backend process already activated
//! `process_reader_demux`. Unlike the reaper's background-task panic,
//! this one runs inline inside live request handling, so it kills the
//! task answering that in-flight client request with no reply at all --
//! indistinguishable from a permanent hang from the client's side.
//! Reproduces the exact live sequence: a real `dispatch_shared` call
//! activates demux for the process first (a real session using the
//! agent), then a capability probe runs against that same agent.

use std::sync::Arc;
use std::time::Duration;

use acpx_conductor::SpawnSpec;
use acpx_core::router::dispatch_shared;
use acpx_core::Router;
use serde_json::json;
use tokio::sync::Mutex;

const BACKEND: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if [ -z "$id" ]; then
    id=$(echo "$line" | grep -o '"id":"[^"]*"' | head -1 | cut -d: -f2)
  fi
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
  fi
done
"#;

fn router() -> Router {
    let mut router = Router::new("stand-in").with_process_reader_demux(true);
    let spec = SpawnSpec::new("sh", vec!["-c".to_string(), BACKEND.to_string()]);
    router.register_agent("stand-in", spec);
    router
}

#[tokio::test]
async fn capability_probe_works_after_demux_is_already_active_on_the_process() {
    let router = Arc::new(Mutex::new(router()));

    // A real live session on this agent activates `process_reader_demux`
    // for the shared backend process, exactly as a real Zed session
    // would.
    let new_response = dispatch_shared(
        &router,
        json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}}),
    )
    .await
    .expect("session/new activates process-reader-demux for this shared process");
    let session_id = new_response["result"]["sessionId"]
        .as_str()
        .expect("gateway session id")
        .to_string();

    // `refresh_models`'s exact call, against the same agent whose backend
    // process now has demux active. Must not panic.
    let capabilities = {
        let mut r = router.lock().await;
        r.probe_adapter_capabilities("stand-in", "/tmp").await
    };
    assert!(
        capabilities.is_ok(),
        "capability probe must not panic once demux is already active: {capabilities:?}"
    );

    // The router lock must have been released promptly too, not held for
    // the whole probe -- an unrelated dispatch right after must still go
    // through fast.
    let unrelated = tokio::time::timeout(
        Duration::from_secs(5),
        dispatch_shared(&router, json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
            "params": {"sessionId": session_id, "prompt": []}
        })),
    )
    .await;
    assert!(
        unrelated.is_ok(),
        "router must not still be wedged after the capability probe returns"
    );
}
