//! **Phase 13.** `dispatch_shared`/`SharedRouterHandle` coverage for the
//! real, per-backend `session/list` path -- `dispatch_session_list_real_
//! shared` (in `acpx-core/src/router.rs`) is an independently-written
//! mirror of `Router::dispatch_session_list_real` (necessary duplication:
//! every `_shared` variant in this file exists specifically to release
//! the router lock before a backend round trip, a restructuring the
//! plain, single-`&mut self` `Router::dispatch` path doesn't need), so it
//! needs its own correctness coverage rather than assuming
//! `session_list_real_test.rs`'s `Router::dispatch`-based tests also
//! prove it. The second test here is the more important one: it proves
//! the concurrency property this whole file's own doc comment on
//! `dispatch_shared` promises -- a `session/list` call proxied to a real,
//! slow-to-respond backend must not block a *different* concurrent
//! client's call to a *different* backend, which is exactly the
//! multiplex-management guarantee this phase's `session/list` change
//! must not regress.

use acpx_conductor::SpawnSpec;
use acpx_core::router::{dispatch_shared, Router};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const STAND_IN_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  elif echo "$line" | grep -q 'session/list'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessions":[{"sessionId":"backend-abc","cwd":"/tmp"},{"sessionId":"backend-xyz","cwd":"/other"}]}}\n' "$id"
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
async fn dispatch_shared_session_list_selector_proxies_and_translates_ids() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router = Arc::new(Mutex::new(router));

    let new_response = dispatch_shared(
        &router,
        json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}}),
    )
    .await
    .expect("session/new");
    let known_gateway_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let list_response = dispatch_shared(
        &router,
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/list",
            "params": {"_acpx": {"agentId": "stand-in-agent"}}
        }),
    )
    .await
    .expect("session/list");
    let sessions = list_response["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0]["sessionId"], json!(known_gateway_id));
    let discovered_gateway_id = sessions[1]["sessionId"].as_str().unwrap().to_string();
    assert_ne!(discovered_gateway_id, "backend-xyz");

    // Same "genuinely dispatchable afterward" proof as the non-shared
    // test, this time through dispatch_shared end to end.
    let close_response = dispatch_shared(
        &router,
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/close",
            "params": {"sessionId": discovered_gateway_id}
        }),
    )
    .await
    .unwrap_or_else(|err| {
        panic!("session/close on a dispatch_shared-discovered gateway id failed: {err}")
    });
    assert_eq!(close_response["result"], json!({"ok": true}));
}

/// A backend that takes a real, measurable amount of time to answer
/// `session/list` (simulated here with `sleep 0.3`, well above any
/// plausible router-lock-acquisition jitter) must not block a
/// *concurrent* `dispatch_shared` call against a *different*, fast
/// backend for anywhere near that long. If this regressed to routing
/// through the generic `router.lock().await.dispatch(request).await`
/// arm (holding the whole-router lock for the entire backend round trip
/// -- see `dispatch_shared`'s own doc comment on exactly why every other
/// backend-talking method class avoids that), the fast call would be
/// stuck waiting behind the slow one and this test would time out/take
/// ~300ms instead of the tight bound asserted below.
#[tokio::test]
async fn dispatch_shared_session_list_does_not_block_a_concurrent_different_backend_call() {
    let slow_script = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/list'; then
    sleep 0.3
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessions":[{"sessionId":"slow-backend-1","cwd":"/tmp"}]}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;
    let fast_script = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"fast-backend-1"}}\n' "$id"
done
"#;

    let mut router = Router::new("fast-agent");
    router.register_agent(
        "slow-agent",
        SpawnSpec::new("sh", vec!["-c".to_string(), slow_script.to_string()]),
    );
    router.register_agent(
        "fast-agent",
        SpawnSpec::new("sh", vec!["-c".to_string(), fast_script.to_string()]),
    );
    let router = Arc::new(Mutex::new(router));

    let slow_router = router.clone();
    let slow_task = tokio::spawn(async move {
        dispatch_shared(
            &slow_router,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "session/list",
                "params": {"_acpx": {"agentId": "slow-agent"}}
            }),
        )
        .await
    });

    // Give the slow call a moment to actually start (acquire the router
    // lock, resolve the backend, begin its `sleep 0.3` round trip) before
    // firing the fast one -- without this, both futures could just
    // happen to interleave favorably by luck rather than because the
    // lock was genuinely released in time.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let started = std::time::Instant::now();
    let fast_response = dispatch_shared(
        &router,
        json!({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {"cwd": "/tmp"}}),
    )
    .await
    .expect("session/new against the fast, unrelated backend");
    let fast_elapsed = started.elapsed();

    assert_eq!(
        fast_response["result"]["sessionId"]
            .as_str()
            .map(|s| s != "fast-backend-1"),
        Some(true),
        "fast-agent's raw backend id must still be rewritten to a gateway id as normal"
    );
    assert!(
        fast_elapsed < Duration::from_millis(150),
        "session/new against an unrelated, fast backend took {fast_elapsed:?} while a \
         concurrent session/list against a slow backend was in flight -- the router lock \
         was held too long, defeating the multi-agent concurrency this test exists to guard"
    );

    let slow_result = tokio::time::timeout(Duration::from_secs(5), slow_task)
        .await
        .expect("slow session/list task should not hang")
        .expect("task join")
        .expect("session/list against slow-agent");
    assert_eq!(
        slow_result["result"]["sessions"][0]["sessionId"]
            .as_str()
            .map(|s| s != "slow-backend-1"),
        Some(true)
    );
}

/// **Regression: `process_reader_demux` session/list-panic gap.** Same
/// class of bug `session_fork_test.rs`'s equivalent test pins:
/// `dispatch_session_list_real_shared` always read its response via
/// `read_matching_response`'s `backend.reader_mut()`, unconditionally --
/// which panics once any earlier call on this same shared process
/// already activated `process_reader_demux`
/// (`BackendProcess::start_demux` takes the raw reader). With the flag
/// now on by default, the first `session/new` here already activates
/// demux for this process, so listing sessions against it right after is
/// exactly the crash scenario. Must complete normally, no panic.
#[tokio::test]
async fn dispatch_shared_session_list_works_after_demux_is_already_active_on_the_process() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router = router.with_process_reader_demux(true);
    let router = Arc::new(Mutex::new(router));

    let new_response = dispatch_shared(
        &router,
        json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}}),
    )
    .await
    .expect("session/new activates process-reader-demux for this shared process");
    let known_gateway_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let list_response = dispatch_shared(
        &router,
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/list",
            "params": {"_acpx": {"agentId": "stand-in-agent"}}
        }),
    )
    .await
    .expect("session/list must not panic on a process demux already activated");
    let sessions = list_response["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0]["sessionId"], json!(known_gateway_id));
}
