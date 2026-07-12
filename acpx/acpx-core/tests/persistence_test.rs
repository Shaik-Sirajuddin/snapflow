//! Phase 2 step 10 -- persistence round-trip tests, against an in-memory
//! sqlite database (no filesystem dependency, isolated per test).

use acpx_core::persistence::{Direction, PersistenceStore};
use serde_json::json;

#[tokio::test]
async fn session_round_trips_and_starts_unclosed() {
    let store = PersistenceStore::open_in_memory().expect("open in-memory store");

    store
        .record_session(
            "gw-1",
            "codex-acp",
            "backend-1",
            Some("work-openai".to_string()),
            "2026-07-12T00:00:00Z",
        )
        .await
        .expect("record session");

    let fetched = store
        .get_session("gw-1")
        .await
        .expect("get_session")
        .expect("session exists");
    assert_eq!(fetched.gateway_session_id, "gw-1");
    assert_eq!(fetched.agent_id, "codex-acp");
    assert_eq!(fetched.backend_session_id, "backend-1");
    assert_eq!(fetched.profile_name.as_deref(), Some("work-openai"));
    assert_eq!(fetched.created_at, "2026-07-12T00:00:00Z");
    // closed_at starts null.
    assert_eq!(fetched.closed_at, None);

    store
        .close_session("gw-1", "2026-07-12T01:00:00Z")
        .await
        .expect("close session");

    let closed = store
        .get_session("gw-1")
        .await
        .expect("get_session")
        .expect("session still exists");
    assert_eq!(closed.closed_at.as_deref(), Some("2026-07-12T01:00:00Z"));
}

#[tokio::test]
async fn closing_an_unknown_session_errors() {
    let store = PersistenceStore::open_in_memory().expect("open in-memory store");
    let err = store
        .close_session("does-not-exist", "2026-07-12T00:00:00Z")
        .await
        .expect_err("closing a missing session should error");
    assert!(err.to_string().contains("does-not-exist"));
}

#[tokio::test]
async fn list_sessions_returns_every_recorded_session() {
    let store = PersistenceStore::open_in_memory().expect("open in-memory store");
    store
        .record_session(
            "gw-1",
            "codex-acp",
            "backend-1",
            None,
            "2026-07-12T00:00:00Z",
        )
        .await
        .expect("record first session");
    store
        .record_session(
            "gw-2",
            "claude-acp",
            "backend-2",
            None,
            "2026-07-12T00:01:00Z",
        )
        .await
        .expect("record second session");

    let all = store.list_sessions().await.expect("list_sessions");
    assert_eq!(all.len(), 2);
    assert!(all.iter().any(|s| s.gateway_session_id == "gw-1"));
    assert!(all.iter().any(|s| s.gateway_session_id == "gw-2"));
}

#[tokio::test]
async fn transcript_append_and_read_back_round_trips_in_order() {
    let store = PersistenceStore::open_in_memory().expect("open in-memory store");
    store
        .record_session(
            "gw-1",
            "codex-acp",
            "backend-1",
            None,
            "2026-07-12T00:00:00Z",
        )
        .await
        .expect("record session");

    let first_id = store
        .append_transcript(
            "gw-1",
            Direction::ClientToAgent,
            json!({"method": "session/prompt", "id": 1}),
            "2026-07-12T00:00:01Z",
        )
        .await
        .expect("append first transcript");
    let second_id = store
        .append_transcript(
            "gw-1",
            Direction::AgentToClient,
            json!({"result": {"stopReason": "end_turn"}, "id": 1}),
            "2026-07-12T00:00:02Z",
        )
        .await
        .expect("append second transcript");
    assert!(second_id > first_id);

    let records = store
        .list_transcripts("gw-1")
        .await
        .expect("list_transcripts");
    assert_eq!(records.len(), 2);

    assert_eq!(records[0].id, Some(first_id));
    assert_eq!(records[0].gateway_session_id, "gw-1");
    assert_eq!(records[0].direction, Direction::ClientToAgent);
    assert_eq!(
        records[0].payload,
        json!({"method": "session/prompt", "id": 1})
    );
    assert_eq!(records[0].recorded_at, "2026-07-12T00:00:01Z");

    assert_eq!(records[1].id, Some(second_id));
    assert_eq!(records[1].direction, Direction::AgentToClient);
    assert_eq!(
        records[1].payload,
        json!({"result": {"stopReason": "end_turn"}, "id": 1})
    );
}

#[tokio::test]
async fn transcripts_for_unknown_session_are_empty_not_an_error() {
    let store = PersistenceStore::open_in_memory().expect("open in-memory store");
    let records = store
        .list_transcripts("never-existed")
        .await
        .expect("list_transcripts");
    assert!(records.is_empty());
}

#[tokio::test]
async fn store_clone_shares_the_same_underlying_database() {
    // Exercises the Clone + spawn_blocking-safe shape described in
    // persistence/mod.rs -- concurrent handles from tokio::spawn should
    // all see the same data.
    let store = PersistenceStore::open_in_memory().expect("open in-memory store");
    let store2 = store.clone();

    let handle = tokio::spawn(async move {
        store2
            .record_session(
                "gw-1",
                "codex-acp",
                "backend-1",
                None,
                "2026-07-12T00:00:00Z",
            )
            .await
    });
    handle
        .await
        .expect("join spawned task")
        .expect("record session");

    let fetched = store.get_session("gw-1").await.expect("get_session");
    assert!(fetched.is_some());
}
