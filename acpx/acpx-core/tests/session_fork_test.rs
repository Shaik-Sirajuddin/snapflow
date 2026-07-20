//! Coverage for real ACP `session/fork` (`MethodClass::SessionFork` in
//! `router.rs`) -- a compatibility gap found and closed post-review: see
//! that enum variant's doc comment for the full story (upstream's own
//! `unstable_session_fork` Cargo feature, the real `claude-agent-acp`
//! 0.58.1 adapter's `sessionCapabilities.fork` advertisement, and why
//! this needed its own dispatch bucket rather than reusing `Proxied`/
//! `Hybrid`). Exercises both `Router::dispatch` (`dispatch_session_fork`)
//! and the `dispatch_shared`/`SharedRouterHandle` path
//! (`dispatch_session_fork_shared`), since the two are independently
//! written mirrors of each other -- same rationale as every other
//! `_shared`-suffixed test file in this crate (see e.g.
//! `session_list_real_shared_test.rs`'s doc comment).

use acpx_conductor::SpawnSpec;
use acpx_core::router::{dispatch_shared, Router};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Stand-in backend: `session/new` and `session/fork` both mint a fresh,
/// distinguishable backend session id (`fork` gets `forked-xyz`, `new`
/// gets `backend-abc`) so a test can assert the gateway id it gets back
/// for the forked session is a genuinely new, different gateway id from
/// the source session's -- not just that dispatch didn't error.
const STAND_IN_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/fork'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"forked-xyz"}}\n' "$id"
  elif echo "$line" | grep -q 'session/new'; then
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
async fn dispatch_session_fork_mints_a_new_gateway_session_distinct_from_the_source() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());

    let new_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}
        }))
        .await
        .expect("session/new");
    let source_gateway_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let fork_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/fork",
            "params": {"sessionId": source_gateway_id, "cwd": "/tmp/forked"}
        }))
        .await
        .expect("session/fork");
    let forked_gateway_id = fork_response["result"]["sessionId"]
        .as_str()
        .expect("session/fork result carries a sessionId")
        .to_string();

    // The client must never see the backend's own raw id either.
    assert_ne!(forked_gateway_id, "forked-xyz");
    // And it must be a genuinely new gateway session, not the source one
    // echoed back.
    assert_ne!(forked_gateway_id, source_gateway_id);

    // Both sessions are independently addressable afterward -- proxying
    // a call against each reaches the backend without error (proves the
    // new session was actually registered in `SessionRegistry`, not just
    // present in the one response).
    for session_id in [source_gateway_id, forked_gateway_id] {
        let close = router
            .dispatch(json!({
                "jsonrpc": "2.0", "id": 3, "method": "session/close",
                "params": {"sessionId": session_id}
            }))
            .await
            .expect("session/close");
        assert!(close.get("result").is_some());
    }
}

#[tokio::test]
async fn dispatch_session_fork_unknown_source_session_errors() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());

    let result = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/fork",
            "params": {"sessionId": "never-registered", "cwd": "/tmp"}
        }))
        .await;
    assert!(
        result.is_err(),
        "forking a nonexistent session must error, not panic or fabricate a session"
    );
}

#[tokio::test]
async fn dispatch_shared_session_fork_mints_a_new_gateway_session() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router = Arc::new(Mutex::new(router));

    let new_response = dispatch_shared(
        &router,
        json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}}),
    )
    .await
    .expect("session/new");
    let source_gateway_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let fork_response = dispatch_shared(
        &router,
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/fork",
            "params": {"sessionId": source_gateway_id, "cwd": "/tmp/forked"}
        }),
    )
    .await
    .expect("session/fork");
    let forked_gateway_id = fork_response["result"]["sessionId"]
        .as_str()
        .expect("session/fork result carries a sessionId")
        .to_string();
    assert_ne!(forked_gateway_id, source_gateway_id);
    assert_ne!(forked_gateway_id, "forked-xyz");

    // The forked session is independently addressable via the shared
    // dispatch path too.
    let close = dispatch_shared(
        &router,
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/close",
            "params": {"sessionId": forked_gateway_id}
        }),
    )
    .await
    .expect("session/close on the forked session");
    assert!(close.get("result").is_some());
}

#[tokio::test]
async fn session_fork_response_carries_backend_agent_updates_via_acpx_extension() {
    // Same `_acpx.updates` convention every other proxied/hybrid method
    // uses for interleaved `session/update` notifications -- proves
    // `dispatch_session_fork` routes through `attach_updates` like every
    // other backend round trip in this file, not a bespoke response
    // shape that would silently drop them.
    const NOTIFYING_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/fork'; then
    printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"src-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"forking..."}}}}\n'
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"forked-xyz"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"src-1"}}\n' "$id"
  fi
done
"#;
    let spec = SpawnSpec::new(
        "sh",
        vec!["-c".to_string(), NOTIFYING_BACKEND_SCRIPT.to_string()],
    );
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", spec);

    let new_response = router
        .dispatch(
            json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}}),
        )
        .await
        .expect("session/new");
    let source_gateway_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let fork_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/fork",
            "params": {"sessionId": source_gateway_id, "cwd": "/tmp/forked"}
        }))
        .await
        .expect("session/fork");
    let updates = fork_response["_acpx"]["updates"]
        .as_array()
        .expect("interleaved session/update notification buffered into _acpx.updates");
    assert_eq!(updates.len(), 1);
    assert_eq!(
        updates[0]["params"]["update"]["sessionUpdate"],
        json!("agent_message_chunk")
    );
}

/// **Regression: `process_reader_demux` fork-panic gap.** Before the fix,
/// `dispatch_session_fork_shared` always read its response via
/// `read_matching_response`'s `backend.reader_mut()`, unconditionally --
/// but once any earlier call against the *same* shared backend process
/// had already enabled process-reader-demux (`BackendProcess::
/// start_demux`, which takes the raw reader), `reader_mut()` panics
/// outright. With `process_reader_demux` now on by default, the first
/// `session/new` on this router already activates demux for this process
/// (see `dispatch_session_new_shared`'s own demux branch) -- so forking a
/// session against that same process is exactly the crash scenario this
/// test pins. Must complete normally, no panic.
#[tokio::test]
async fn dispatch_shared_session_fork_works_after_demux_is_already_active_on_the_process() {
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
    let source_gateway_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let fork_response = dispatch_shared(
        &router,
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/fork",
            "params": {"sessionId": source_gateway_id, "cwd": "/tmp/forked"}
        }),
    )
    .await
    .expect("session/fork must not panic on a process demux already activated");
    let forked_gateway_id = fork_response["result"]["sessionId"]
        .as_str()
        .expect("session/fork result carries a sessionId")
        .to_string();
    assert_ne!(forked_gateway_id, source_gateway_id);
    assert_ne!(forked_gateway_id, "forked-xyz");
}
