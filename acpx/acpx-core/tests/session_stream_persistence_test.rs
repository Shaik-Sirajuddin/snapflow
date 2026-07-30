use acpx_conductor::SpawnSpec;
use acpx_core::{QueueStore, Router, TranscriptStore};
use serde_json::json;
use tempfile::tempdir;

const LOAD_REPLAY_BACKEND: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([^,}]*\).*/\1/p')
  if printf '%s' "$line" | grep -q '"method":"initialize"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[]}}\n' "$id"
  elif printf '%s' "$line" | grep -q '"method":"session/new"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-1"}}\n' "$id"
  elif printf '%s' "$line" | grep -q '"method":"session/load"'; then
    printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"backend-1","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"old"}}}}'
    printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"backend-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"new"}}}}'
    printf '{"jsonrpc":"2.0","id":%s,"result":{"loaded":true}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
  fi
done
"#;

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

#[tokio::test]
async fn sync_calls_native_load_and_returns_only_the_unmatched_suffix() {
    let directory = tempdir().unwrap();
    let store = TranscriptStore::new(directory.path());
    let mut router = Router::new("replay-agent").with_transcript_store(store.clone());
    router.register_agent(
        "replay-agent",
        SpawnSpec::new("sh", vec!["-c".into(), LOAD_REPLAY_BACKEND.into()]),
    );
    let created = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .await
        .unwrap();
    let session_id = created["result"]["sessionId"].as_str().unwrap().to_string();
    store
        .append(
            &session_id,
            vec![json!({
                "method": "session/update",
                "params": {"update": {"sessionUpdate": "user_message_chunk"}}
            })],
        )
        .await
        .unwrap();

    let response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "acpx/sessions/sync",
            "params": {"sessionId": session_id, "knownMessageCount": 1}
        }))
        .await
        .unwrap();
    assert_eq!(response["patch"]["startIndex"], 1);
    assert_eq!(response["patch"]["replaceCount"], 0);
    assert_eq!(response["patch"]["messages"].as_array().unwrap().len(), 1);
    assert_eq!(
        response["patch"]["messages"][0]["params"]["update"]["content"]["text"],
        "new"
    );
}

#[tokio::test]
async fn queue_endpoint_is_fifo_and_deduplicates_client_retries() {
    let directory = tempdir().unwrap();
    let mut router = Router::new("default").with_queue_store(QueueStore::new(directory.path()));
    let first = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/queue",
            "params": {
                "sessionId": "session-1", "operation": "enqueue",
                "text": "first", "idempotencyKey": "client-1"
            }
        }))
        .await
        .unwrap();
    assert_eq!(first["queue"].as_array().unwrap().len(), 1);

    let retry = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/queue",
            "params": {
                "sessionId": "session-1", "operation": "enqueue",
                "text": "first", "idempotencyKey": "client-1"
            }
        }))
        .await
        .unwrap();
    assert_eq!(retry["queue"].as_array().unwrap().len(), 1);

    let second = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/queue",
            "params": {
                "sessionId": "session-1", "operation": "enqueue",
                "text": "second", "idempotencyKey": "client-2"
            }
        }))
        .await
        .unwrap();
    assert_eq!(second["queue"][0]["text"], "first");
    assert_eq!(second["queue"][1]["text"], "second");
}
