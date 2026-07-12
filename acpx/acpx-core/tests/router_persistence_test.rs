//! Confirms `Router::with_persistence` actually records session metadata
//! and transcripts -- the fire-and-forget `tokio::spawn` writes in
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
async fn session_new_persists_session_metadata_and_transcripts() {
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

    let mut transcripts = Vec::new();
    for _ in 0..150 {
        transcripts = store
            .list_transcripts(gateway_id.clone())
            .await
            .expect("list_transcripts");
        if transcripts.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // One client->agent (the session/new request) and one agent->client
    // (the response, with the backend's raw session id already rewritten
    // to the gateway id) transcript entry.
    assert_eq!(transcripts.len(), 2);
}
