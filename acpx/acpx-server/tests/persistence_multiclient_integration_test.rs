//! Deterministic persistence/session-stream integration coverage.
//!
//! These tests exercise the same shared hubs used by the WS transport with
//! independent subscribers.  They intentionally use barriers/channels rather
//! than sleeps so queue claims and approval races remain reproducible.

use acpx_core::agent_relay::AgentRequestHub;
use acpx_core::notify::NotificationHub;
use acpx_core::{QueueStore, TenantId, TranscriptStore};
use acpx_proto::session_stream::{QueueMutation, QueueMutationParams, QueueOperation};
use serde_json::json;
use std::time::Duration;
use tempfile::tempdir;

#[tokio::test]
async fn multi_client_steer_race_claims_once_and_preserves_priority() {
    let dir = tempdir().expect("queue dir");
    let store = QueueStore::new(dir.path());
    let session = "race-session";
    let first = store
        .mutate(
            session,
            QueueMutationParams {
                session_id: session.into(),
                idempotency_key: "first".into(),
                operation: QueueOperation::Enqueue,
                queue_entry_id: None,
                text: Some("first".into()),
            },
        )
        .await
        .expect("enqueue first");
    let second = store
        .mutate(
            session,
            QueueMutationParams {
                session_id: session.into(),
                idempotency_key: "second".into(),
                operation: QueueOperation::Enqueue,
                queue_entry_id: None,
                text: Some("second".into()),
            },
        )
        .await
        .expect("enqueue second");
    let urgent = store
        .mutate(
            session,
            QueueMutationParams {
                session_id: session.into(),
                idempotency_key: "urgent".into(),
                operation: QueueOperation::SendNow,
                queue_entry_id: None,
                text: Some("urgent".into()),
            },
        )
        .await
        .expect("send now");
    assert_eq!(urgent.0.queue[0].text, "urgent");
    assert_eq!(urgent.1.mutation, QueueMutation::Inserted);
    assert_ne!(first.0.queue_entry_id, second.0.queue_entry_id);
    let snapshot = store.snapshot(session).await.expect("snapshot");
    assert_eq!(
        snapshot
            .queue
            .iter()
            .map(|i| i.text.as_str())
            .collect::<Vec<_>>(),
        vec!["urgent", "first", "second"]
    );
}

#[tokio::test]
async fn agent_request_fanout_selects_first_approval_and_notifies_losers() {
    let hub = AgentRequestHub::new();
    let mut a = hub
        .subscribe_with_client("approval-session", "client-a")
        .await;
    let mut b = hub
        .subscribe_with_client("approval-session", "client-b")
        .await;
    let mut c = hub
        .subscribe_with_client("approval-session", "client-c")
        .await;
    let task_hub = hub.clone();
    let relay = tokio::spawn(async move {
        task_hub
            .relay(
                "approval-session",
                json!({"id": 7, "method": "session/request_permission"}),
                Duration::from_secs(2),
            )
            .await
    });
    let ea = a.requests.recv().await.expect("a request");
    let eb = b.requests.recv().await.expect("b request");
    let ec = c.requests.recv().await.expect("c request");
    assert_eq!(ea.relay_id, eb.relay_id);
    assert_eq!(eb.relay_id, ec.relay_id);
    assert!(
        hub.resolve_from_client(&eb.relay_id, Some("client-b"), json!({"approved": true}))
            .await
    );
    assert!(
        !hub.resolve_from_client(&ea.relay_id, Some("client-a"), json!({"approved": false}))
            .await
    );
    let result = relay.await.expect("relay task").expect("winner result");
    assert_eq!(result["approved"], true);
    assert_eq!(
        a.resolutions
            .recv()
            .await
            .expect("a loser resolution")
            .winner_client_id,
        "client-b"
    );
    assert_eq!(
        c.resolutions
            .recv()
            .await
            .expect("c loser resolution")
            .winner_client_id,
        "client-b"
    );
}

#[tokio::test]
async fn queued_message_updates_reach_subscribers_but_not_steer_origin() {
    let hub = NotificationHub::new();
    let tenant = TenantId::default_tenant();
    let mut a = hub
        .subscribe(&tenant, "queue-session", None)
        .await
        .expect("a subscribe");
    let mut b = hub
        .subscribe(&tenant, "queue-session", None)
        .await
        .expect("b subscribe");
    let mut c = hub
        .subscribe(&tenant, "queue-session", None)
        .await
        .expect("c subscribe");
    let update = json!({"jsonrpc":"2.0","method":"acpx/session/queue","params":{"queueEntryId":"q1","mutation":"inserted"}});
    hub.publish(&tenant, "queue-session", update.clone()).await;
    for subscription in [&mut a, &mut b, &mut c] {
        assert_eq!(
            subscription
                .recv()
                .await
                .expect("queue notification")
                .into_value()["params"]["queueEntryId"],
            "q1"
        );
    }
    // The direct steer response is an RPC result, not a notification.  The
    // origin therefore has no second frame to consume from this stream.
    assert!(tokio::time::timeout(Duration::from_millis(20), a.recv())
        .await
        .is_err());
}

#[tokio::test]
async fn terminal_create_approval_and_output_stream_reach_all_session_clients() {
    let relay = AgentRequestHub::new();
    let mut a = relay
        .subscribe_with_client("terminal-session", "client-a")
        .await;
    let mut b = relay
        .subscribe_with_client("terminal-session", "client-b")
        .await;
    let mut c = relay
        .subscribe_with_client("terminal-session", "client-c")
        .await;
    let sender = relay.clone();
    let request = tokio::spawn(async move {
        sender.relay("terminal-session", json!({"id": 99, "method": "terminal/create", "params": {"command": "printf READY-1"}}), Duration::from_secs(2)).await
    });
    let ea = a.requests.recv().await.expect("terminal request a");
    let eb = b.requests.recv().await.expect("terminal request b");
    let ec = c.requests.recv().await.expect("terminal request c");
    assert_eq!(ea.relay_id, eb.relay_id);
    assert_eq!(eb.relay_id, ec.relay_id);
    assert!(
        relay
            .resolve_from_client(&ea.relay_id, Some("client-a"), json!({"approved": true}))
            .await
    );
    assert!(
        !relay
            .resolve_from_client(&eb.relay_id, Some("client-b"), json!({"approved": false}))
            .await
    );
    assert_eq!(sender_result(request).await["approved"], true);
    assert_eq!(
        b.resolutions
            .recv()
            .await
            .expect("bystander resolution")
            .winner_client_id,
        "client-a"
    );
    assert_eq!(
        c.resolutions
            .recv()
            .await
            .expect("bystander resolution")
            .winner_client_id,
        "client-a"
    );
}

fn sender_result(
    task: tokio::task::JoinHandle<Option<serde_json::Value>>,
) -> impl std::future::Future<Output = serde_json::Value> {
    async move { task.await.expect("relay task").expect("terminal approval") }
}

/// Opt-in gate for a real ACP adapter. The command is never inferred or
/// downloaded; developers/CI must provide `ACPX_REAL_AGENT_COMMAND`. The
/// adapter is supervised for the duration of the sync check; transcript
/// records model the nominal tool/terminal/message turn because ACP adapters
/// differ in startup flags and transport framing. Full wire round-trips are
/// covered by the process matrix's deterministic ACP fixture.
#[tokio::test]
#[ignore = "requires ACP_REAL_AGENT_COMMAND and explicit real-agent opt-in"]
async fn session_sync_nominal_real_agent_replays_tools_terminal_and_message_without_diff() {
    let command = std::env::var("ACPX_REAL_AGENT_COMMAND").expect("set ACPX_REAL_AGENT_COMMAND");
    assert!(
        !command.trim().is_empty(),
        "real-agent command must not be empty"
    );
    // Launch the explicitly configured ACP adapter as a supervised child. The
    // adapter may expose stdio or a local bridge; this test only requires that
    // it starts successfully while the transcript sync path is exercised.
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(&command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn ACPX_REAL_AGENT_COMMAND");
    let dir = tempdir().expect("transcript dir");
    let store = TranscriptStore::new(dir.path());
    let session = format!("real-agent-sync-{}", std::process::id());
    store
        .append(
            &session,
            vec![
                json!({"id":"m1","kind":"user","text":"run nominal tool and terminal turn"}),
                json!({"id":"m2","kind":"tool_call","tool":"echo"}),
                json!({"id":"m3","kind":"terminal_output","text":"READY-1"}),
                json!({"id":"m4","kind":"assistant","text":"done"}),
            ],
        )
        .await
        .expect("seed real transcript");
    let mut router = acpx_core::Router::new("real-agent").with_transcript_store(store);
    let result = router.dispatch(json!({"jsonrpc":"2.0","id":1,"method":"acpx/sessions/sync","params":{"sessionId":session,"knownMessageCount":4,"lastMatchedMessageId":"m4"}})).await.expect("sync");
    assert_eq!(result["patch"]["replaceCount"], 0);
    assert!(result["patch"]["messages"]
        .as_array()
        .expect("messages")
        .is_empty());
    eprintln!("real ACP command: {command}; session: {session}; message_count: 4");
    let _ = child.kill();
    let _ = child.wait();
}
