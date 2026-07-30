use acpx_core::{Router, TranscriptStore};
use serde_json::json;
use tempfile::tempdir;

#[tokio::test]
async fn router_serves_initial_and_older_transcript_pages_from_server_store() {
    let directory = tempdir().unwrap();
    let store = TranscriptStore::new(directory.path());
    store
        .append("session-1", (0..95).map(|id| json!({"id": id})).collect())
        .await
        .unwrap();

    let mut router = Router::new("default").with_transcript_store(store);
    let initial = router
        .dispatch(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "acpx/sessions/paginate",
            "params": {"sessionId": "session-1"}
        }))
        .await
        .unwrap();
    assert_eq!(initial["messages"].as_array().unwrap().len(), 50);
    let cursor = initial["nextCursor"].as_str().unwrap();

    let older = router
        .dispatch(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "acpx/sessions/paginate",
            "params": {"sessionId": "session-1", "before": cursor}
        }))
        .await
        .unwrap();
    assert_eq!(older["messages"].as_array().unwrap().len(), 40);
    assert_eq!(older["messages"][0]["id"], 5);
}
