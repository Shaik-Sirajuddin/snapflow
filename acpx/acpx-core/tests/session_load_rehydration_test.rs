//! **Phase 8/9 addition.** `Router::rehydrate_session`'s fallback path --
//! before this, `session/load`/`session/resume`/`session/delete` were
//! generic `Proxied` methods that required the gateway session id to
//! already be a live key in the in-memory `SessionRegistry`, exactly
//! like `session/prompt`. That defeated the entire point of those
//! methods existing as distinct from `session/new`: they're meant to
//! work against a session this exact process never itself created (most
//! obviously, one from before a restart). `acpx-server/tests/real_
//! ambient_multi_agent_test.rs`'s `ambient_claude_session_load_survives_
//! a_real_gateway_restart` proves this against two genuinely separate
//! real processes and a real adapter; this file proves the same
//! `Router`-level logic deterministically and without any real
//! subprocess/billing, by using `session/close` (which phase 7 already
//! made evict the in-memory registry entry while leaving the durable
//! sqlite row alone) as a cheap, self-contained stand-in for "a restart
//! happened" -- the in-memory-miss code path `rehydrate_session` takes
//! is identical either way; only *how* the in-memory entry became
//! absent differs.

use acpx_conductor::SpawnSpec;
use acpx_core::persistence::{
    sessions::{RecoveryMetadata, RecoveryMethod, RecoveryStatus},
    PersistenceStore,
};
use acpx_core::router::{Router, RouterError};
use serde_json::json;
use std::time::Duration;

/// Always echoes a fixed backend session id, regardless of method --
/// good enough here since every request in these tests is either
/// `session/new` (needs a `sessionId` in the result) or a
/// `Proxied`/`GatewayNative` call this stand-in doesn't need to answer
/// with anything method-specific.
const STAND_IN_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
done
"#;

fn stand_in_backend_spec() -> SpawnSpec {
    SpawnSpec::new(
        "sh",
        vec!["-c".to_string(), STAND_IN_BACKEND_SCRIPT.to_string()],
    )
}

async fn wait_for_session_row(store: &PersistenceStore, gateway_id: &str) {
    for _ in 0..150 {
        if store
            .get_session(gateway_id.to_string())
            .await
            .expect("get_session")
            .is_some()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("session {gateway_id} never landed in the persistence store");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_load_rehydrates_after_session_close_evicts_the_in_memory_registry() {
    let store = PersistenceStore::open_in_memory().expect("open in-memory store");
    let mut router = Router::new("stand-in-agent").with_persistence(store.clone());
    router.register_agent("stand-in-agent", stand_in_backend_spec());

    let new_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .await
        .expect("session/new");
    let gateway_id = new_response["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();
    wait_for_session_row(&store, &gateway_id).await;

    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/close",
            "params": {"sessionId": gateway_id}
        }))
        .await
        .expect("session/close");

    // Without rehydration this would fail `UnknownSession` -- phase 7's
    // own fix made `session/close` evict the in-memory registry entry,
    // and until this phase nothing ever re-populated it.
    let load_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/load",
            "params": {"sessionId": gateway_id, "cwd": "/tmp"}
        }))
        .await
        .unwrap_or_else(|err| panic!("session/load should rehydrate from persistence: {err}"));
    assert!(
        load_response.get("error").is_none(),
        "session/load returned a JSON-RPC error: {load_response:?}"
    );

    // Genuinely re-registered, not just answered once: an ordinary
    // `session/prompt` against the same gateway id must work afterward.
    let prompt_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 4, "method": "session/prompt",
            "params": {"sessionId": gateway_id, "prompt": []}
        }))
        .await
        .expect("session/prompt after rehydration");
    assert!(prompt_response.get("error").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_delete_also_rehydrates_from_persistence() {
    let store = PersistenceStore::open_in_memory().expect("open in-memory store");
    let mut router = Router::new("stand-in-agent").with_persistence(store.clone());
    router.register_agent("stand-in-agent", stand_in_backend_spec());

    let new_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .await
        .expect("session/new");
    let gateway_id = new_response["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();
    wait_for_session_row(&store, &gateway_id).await;

    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/close",
            "params": {"sessionId": gateway_id}
        }))
        .await
        .expect("session/close");

    let delete_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/delete",
            "params": {"sessionId": gateway_id}
        }))
        .await
        .unwrap_or_else(|err| panic!("session/delete should rehydrate from persistence: {err}"));
    assert!(delete_response.get("error").is_none());
}

/// Rehydration is deliberately scoped to `session/load`/`session/resume`/
/// `session/delete` only -- every other `Proxied` method (`session/
/// prompt` here) must keep erroring `UnknownSession` for a gateway id
/// this process's in-memory registry has never held live, even with
/// persistence configured and a matching durable row present. Silently
/// reviving one on an arbitrary method would paper over real client bugs
/// (e.g. a stale/typo'd session id in an ordinary prompt call) instead of
/// surfacing them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_prompt_does_not_rehydrate_even_with_a_matching_persisted_row() {
    let store = PersistenceStore::open_in_memory().expect("open in-memory store");
    let mut router = Router::new("stand-in-agent").with_persistence(store.clone());
    router.register_agent("stand-in-agent", stand_in_backend_spec());

    let new_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .await
        .expect("session/new");
    let gateway_id = new_response["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();
    wait_for_session_row(&store, &gateway_id).await;

    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/close",
            "params": {"sessionId": gateway_id}
        }))
        .await
        .expect("session/close");

    let err = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": {"sessionId": gateway_id, "prompt": []}
        }))
        .await
        .expect_err("session/prompt must not rehydrate a closed session");
    assert!(matches!(err, RouterError::UnknownSession(id) if id == gateway_id));
}

/// No persistence configured at all -- `session/load` against an
/// entirely unknown gateway id must fail with the specific, honest
/// `SessionNotPersisted` error (distinguishing "recovery wasn't even
/// possible here" from `UnknownSession`'s "genuinely never existed and
/// this isn't a resumption method anyway"), not panic or silently
/// succeed.
#[tokio::test]
async fn session_load_without_persistence_configured_fails_clearly() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());

    let err = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/load",
            "params": {"sessionId": "never-existed", "cwd": "/tmp"}
        }))
        .await
        .expect_err("session/load with no persistence and an unknown id must fail");
    assert!(matches!(err, RouterError::SessionNotPersisted(id) if id == "never-existed"));
}

#[tokio::test]
async fn session_load_returns_retryable_error_while_durable_recovery_is_in_progress() {
    let store = PersistenceStore::open_in_memory().expect("open in-memory store");
    store
        .record_session_with_recovery(
            "gateway-restoring",
            "stand-in-agent",
            "backend-restoring",
            None,
            "2026-07-16T00:00:00Z",
            "default",
            RecoveryMetadata {
                cwd: Some("/tmp".to_string()),
                recovery_params: Some(json!({"cwd": "/tmp"})),
                status: RecoveryStatus::Restoring,
                recovery_method: RecoveryMethod::Load,
                ..RecoveryMetadata::default()
            },
        )
        .await
        .expect("seed restoring row");

    let mut router = Router::new("stand-in-agent").with_persistence(store);
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let err = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/load",
            "params": {"sessionId": "gateway-restoring", "cwd": "/tmp"}
        }))
        .await
        .expect_err("restoring sessions must not start a duplicate recovery");
    assert!(matches!(err, RouterError::SessionRestoring(id) if id == "gateway-restoring"));
}
