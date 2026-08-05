//! Confirms `Router::with_persistence` actually records session metadata
//! and advances durable state -- the fire-and-forget `tokio::spawn` writes in
//! `router.rs` are invisible to the caller of `dispatch`, so this test
//! polls the store briefly after dispatch returns rather than assuming
//! synchronous completion.

use acpx_conductor::SpawnSpec;
use acpx_core::persistence::PersistenceStore;
use acpx_core::router::Router;
use serde_json::json;
use std::time::Duration;

const STAND_IN_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
done
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_new_persists_session_metadata_and_state_revision() {
    let store = PersistenceStore::open_in_memory().expect("open in-memory store");
    let mut router = Router::new("stand-in-agent").with_persistence(store.clone());
    router.register_agent(
        "stand-in-agent",
        SpawnSpec::new(
            "sh",
            vec!["-c".to_string(), STAND_IN_BACKEND_SCRIPT.to_string()],
        ),
    );

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/new",
        "params": {"cwd": "/tmp"}
    });
    let response = router.dispatch(request).await.expect("session/new");
    let gateway_id = response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    // The persistence write is fire-and-forget (tokio::spawn) -- give it a
    // brief window to land rather than assuming it's synchronous with
    // dispatch's return.
    let mut sessions = Vec::new();
    for _ in 0..150 {
        sessions = store.list_sessions().await.expect("list_sessions");
        if !sessions.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].gateway_session_id, gateway_id);
    assert_eq!(sessions[0].agent_id, "stand-in-agent");
    assert!(sessions[0].closed_at.is_none());
    assert!(sessions[0].created_at_unix_nanos.is_some());
    assert!(sessions[0].last_activity_at_unix_nanos.is_some());

    router
        .set_session_pinned(&acpx_core::TenantId::default_tenant(), &gateway_id, true)
        .await
        .expect("pin persisted session");
    let pinned = store
        .get_session(gateway_id.clone())
        .await
        .expect("get pinned session")
        .expect("session exists");
    assert!(pinned.pinned);
    assert!(pinned.last_activity_at_unix_nanos.is_some());

    let mut state_revision = 0;
    for _ in 0..150 {
        state_revision = store
            .get_session(gateway_id.clone())
            .await
            .expect("get session")
            .expect("session exists")
            .state_revision;
        if state_revision >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(state_revision >= 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_new_creation_request_is_deduplicated_until_first_prompt() {
    let store = PersistenceStore::open_in_memory().expect("open in-memory store");
    let mut router = Router::new("stand-in-agent").with_persistence(store.clone());
    router.register_agent(
        "stand-in-agent",
        SpawnSpec::new(
            "sh",
            vec!["-c".to_string(), STAND_IN_BACKEND_SCRIPT.to_string()],
        ),
    );
    let creation_request_id = "7ee35d23-17d5-4a62-92cb-d36d857fc272";
    let new_request = |id| {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/new",
            "params": {
                "cwd": "/tmp",
                "_meta": {
                    "com.example.client": {
                        "creationRequestId": creation_request_id,
                        "workspaceId": "project-42"
                    }
                }
            }
        })
    };

    let first = router.dispatch(new_request(1)).await.expect("first new");
    let gateway_id = first["result"]["sessionId"]
        .as_str()
        .expect("gateway id")
        .to_string();
    assert_eq!(
        first["result"]["_meta"]["com.example.client"]["creationRequestId"],
        creation_request_id
    );

    let retry = router
        .dispatch(new_request(2))
        .await
        .expect("deduplicated new");
    assert_eq!(retry["result"]["sessionId"], gateway_id);
    assert_eq!(store.list_sessions().await.expect("sessions").len(), 1);

    let listed = router
        .dispatch(json!({"jsonrpc":"2.0", "id":3, "method":"session/list", "params":{}}))
        .await
        .expect("session/list");
    assert_eq!(
        listed["result"]["sessions"][0]["_meta"]["com.example.client"]["workspaceId"],
        "project-42"
    );

    router
        .dispatch(json!({
            "jsonrpc":"2.0", "id":4, "method":"session/prompt",
            "params":{"sessionId":gateway_id, "prompt":[{"type":"text","text":"hi"}]}
        }))
        .await
        .expect("prompt");
    let conflict = router
        .dispatch(new_request(5))
        .await
        .expect_err("used conflict");
    assert!(conflict.to_string().contains("already received a turn"));
}
