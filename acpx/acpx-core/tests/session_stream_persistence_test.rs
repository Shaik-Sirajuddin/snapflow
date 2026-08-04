use acpx_conductor::SpawnSpec;
use acpx_core::router::dispatch_shared_for_tenant;
use acpx_core::{QueueStore, Router, TenantId, TranscriptStore};
use serde_json::json;
use std::sync::Arc;
use tempfile::tempdir;
use tokio::sync::Mutex;

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

const PROMPT_TOOL_CALL_BACKEND: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([^,}]*\).*/\1/p')
  if printf '%s' "$line" | grep -q '"method":"initialize"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[]}}\n' "$id"
  elif printf '%s' "$line" | grep -q '"method":"session/new"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-prompt"}}\n' "$id"
  elif printf '%s' "$line" | grep -q '"method":"session/prompt"'; then
    printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"backend-prompt","update":{"sessionUpdate":"tool_call","toolCallId":"call-1","title":"Read file","status":"completed"}}}'
    printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"backend-prompt","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"done"}}}}'
    printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
  fi
done
"#;

/// A paginated page must carry the whole turn -- the user's own prompt as
/// well as the tool call and agent text it produced. The prompt has no
/// backend `session/update` of its own, so it is only in the transcript if
/// the gateway persists it at dispatch time.
#[tokio::test]
async fn paginated_history_includes_the_user_prompt_and_tool_calls() {
    let directory = tempdir().unwrap();
    let mut router =
        Router::new("prompt-agent").with_transcript_store(TranscriptStore::new(directory.path()));
    router.register_agent(
        "prompt-agent",
        SpawnSpec::new("sh", vec!["-c".into(), PROMPT_TOOL_CALL_BACKEND.into()]),
    );
    let router = Arc::new(Mutex::new(router));
    let tenant = TenantId::default_tenant();

    let created = dispatch_shared_for_tenant(
        &router,
        &tenant,
        json!({
            "jsonrpc":"2.0", "id":1, "method":"session/new",
            "params":{"cwd":"/tmp"}
        }),
    )
    .await
    .unwrap();
    let session_id = created["result"]["sessionId"].as_str().unwrap().to_owned();

    dispatch_shared_for_tenant(
        &router,
        &tenant,
        json!({
            "jsonrpc":"2.0", "id":2, "method":"session/prompt",
            "params":{"sessionId":session_id,
                      "prompt":[{"type":"text","text":"hello there"}]}
        }),
    )
    .await
    .unwrap();

    let page = dispatch_shared_for_tenant(
        &router,
        &tenant,
        json!({
            "jsonrpc":"2.0", "id":3, "method":"acpx/sessions/paginate",
            "params":{"sessionId":session_id}
        }),
    )
    .await
    .unwrap();
    let messages = page["messages"].as_array().unwrap();
    let kinds = messages
        .iter()
        .map(|message| {
            message["params"]["update"]["sessionUpdate"]
                .as_str()
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        ["user_message_chunk", "tool_call", "agent_message_chunk"]
    );
    assert_eq!(
        messages[0]["params"]["update"]["content"]["text"],
        "hello there"
    );
    assert_eq!(messages[0]["params"]["sessionId"], session_id);
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

const QUEUE_DISPATCH_BACKEND: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([^,}]*\).*/\1/p')
  if printf '%s' "$line" | grep -q '"method":"initialize"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[]}}\n' "$id"
  elif printf '%s' "$line" | grep -q '"method":"session/new"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-queue"}}\n' "$id"
  elif printf '%s' "$line" | grep -q '"method":"session/prompt"'; then
    text=$(printf '%s' "$line" | sed -n 's/.*"prompt":\[.*"text":"\([^"]*\)".*$/\1/p')
    printf '%s\n' "$text" >> "$ACPX_QUEUE_CAPTURE"
    printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
  fi
done
"#;

#[tokio::test]
async fn shared_clients_are_fifo_and_auto_dispatch_promoted_queue_entries() {
    let directory = tempdir().unwrap();
    let capture = directory.path().join("prompts.txt");
    let backend =
        QUEUE_DISPATCH_BACKEND.replace("$ACPX_QUEUE_CAPTURE", &capture.display().to_string());
    let mut router = Router::new("queue-agent").with_queue_store(QueueStore::new(directory.path()));
    router.register_agent(
        "queue-agent",
        SpawnSpec::new("sh", vec!["-c".into(), backend]),
    );
    let router = Arc::new(Mutex::new(router));
    let tenant = TenantId::default_tenant();

    let created = dispatch_shared_for_tenant(
        &router,
        &tenant,
        json!({
            "jsonrpc":"2.0", "id":1, "method":"session/new",
            "params":{"cwd":"/tmp"}
        }),
    )
    .await
    .unwrap();
    let session_id = created["result"]["sessionId"].as_str().unwrap().to_owned();

    dispatch_shared_for_tenant(
        &router,
        &tenant,
        json!({
            "jsonrpc":"2.0", "id":2, "method":"session/queue",
            "params":{"sessionId":session_id,"operation":"pause",
                      "idempotencyKey":"client-a-pause"}
        }),
    )
    .await
    .unwrap();

    let queue_hub = { router.lock().await.queue_hub() };
    let mut client_a = queue_hub.subscribe(&session_id).await;
    let mut client_b = queue_hub.subscribe(&session_id).await;

    let first = dispatch_shared_for_tenant(
        &router,
        &tenant,
        json!({
            "jsonrpc":"2.0", "id":3, "method":"session/queue",
            "params":{"sessionId":session_id,"operation":"enqueue",
                      "text":"first","idempotencyKey":"client-a-1"}
        }),
    )
    .await
    .unwrap();
    let second = dispatch_shared_for_tenant(
        &router,
        &tenant,
        json!({
            "jsonrpc":"2.0", "id":4, "method":"session/queue",
            "params":{"sessionId":session_id,"operation":"enqueue",
                      "text":"second","idempotencyKey":"client-b-1"}
        }),
    )
    .await
    .unwrap();
    assert!(first["accepted"].as_bool().unwrap());
    assert!(second["accepted"].as_bool().unwrap());

    let resumed = dispatch_shared_for_tenant(
        &router,
        &tenant,
        json!({
            "jsonrpc":"2.0", "id":5, "method":"session/queue",
            "params":{"sessionId":session_id,"operation":"resume",
                      "idempotencyKey":"client-a-resume"}
        }),
    )
    .await
    .unwrap();
    assert!(resumed["accepted"].as_bool().unwrap());

    let subscription = dispatch_shared_for_tenant(
        &router,
        &tenant,
        json!({
            "jsonrpc":"2.0", "id":6, "method":"acpx/sessions/queue/subscribe",
            "params":{"sessionIds":[session_id]}
        }),
    )
    .await
    .unwrap();
    assert_eq!(subscription["snapshots"][0]["sessionId"], session_id);

    for receiver in [&mut client_a, &mut client_b] {
        let event = tokio::time::timeout(std::time::Duration::from_secs(3), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event["method"], "acpx/session/queue");
        assert_eq!(event["params"]["sessionId"], session_id);
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let lines = loop {
        if let Ok(content) = std::fs::read_to_string(&capture) {
            let lines = content
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if lines.len() >= 2 {
                break lines;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "queued prompts were not dispatched"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    };
    assert_eq!(&lines[..2], ["first", "second"]);
    assert!(router
        .lock()
        .await
        .queue_store()
        .unwrap()
        .snapshot(session_id)
        .await
        .unwrap()
        .queue
        .is_empty());
}
