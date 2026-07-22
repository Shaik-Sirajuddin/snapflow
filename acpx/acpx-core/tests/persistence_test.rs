//! Phase 2 step 10 -- persistence round-trip tests, against an in-memory
//! sqlite database (no filesystem dependency, isolated per test).

use acpx_core::persistence::{
    sessions::{RecoveryMetadata, RecoveryMethod, RecoveryStatus},
    Direction, PersistenceStore,
};
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
            "default",
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
    assert_eq!(fetched.tenant_id, "default");
    // closed_at starts null.
    assert_eq!(fetched.closed_at, None);
    assert_eq!(fetched.status, RecoveryStatus::Active);
    assert_eq!(fetched.recovery_method, RecoveryMethod::None);
    assert_eq!(fetched.cwd, None);
    assert_eq!(fetched.recovery_params, None);
    assert_eq!(fetched.last_recovery_error, None);
    assert!(!fetched.pinned);
    assert!(fetched.created_at_unix_nanos.is_some());
    assert!(fetched.last_activity_at_unix_nanos.is_some());
    assert_eq!(fetched.bridge_session_id, None);
    assert_eq!(fetched.bridge_model_alias, None);
    assert_eq!(fetched.bridge_config_options, None);

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
    assert_eq!(closed.status, RecoveryStatus::Closed);
}

#[tokio::test]
async fn recovery_metadata_round_trips_and_filters_startup_candidates() {
    let store = PersistenceStore::open_in_memory().expect("open in-memory store");
    store
        .record_session_with_recovery(
            "gw-load",
            "codex-acp",
            "backend-load",
            None,
            "2026-07-12T00:00:00Z",
            "default",
            RecoveryMetadata {
                cwd: Some("/workspace/project".to_string()),
                recovery_params: Some(json!({"checkpoint": "abc"})),
                status: RecoveryStatus::Active,
                recovery_method: RecoveryMethod::Load,
                last_recovery_error: None,
                ..RecoveryMetadata::default()
            },
        )
        .await
        .expect("record recoverable session");
    store
        .record_session_with_recovery(
            "gw-none",
            "codex-acp",
            "backend-none",
            None,
            "2026-07-12T00:01:00Z",
            "default",
            RecoveryMetadata::default(),
        )
        .await
        .expect("record non-recoverable session");

    let recoverable = store
        .list_recoverable_sessions(None)
        .await
        .expect("list recoverable sessions");
    assert_eq!(recoverable.len(), 1);
    let session = &recoverable[0];
    assert_eq!(session.gateway_session_id, "gw-load");
    assert_eq!(session.cwd.as_deref(), Some("/workspace/project"));
    assert_eq!(session.recovery_params, Some(json!({"checkpoint": "abc"})));
    assert_eq!(session.recovery_method, RecoveryMethod::Load);
    assert_eq!(session.bridge_session_id, None);

    store
        .update_recovery_status(
            "gw-load",
            RecoveryStatus::RecoveryFailed,
            Some("backend unavailable".to_string()),
        )
        .await
        .expect("record recovery failure");
    let failed = store
        .get_session("gw-load")
        .await
        .expect("get session")
        .expect("session exists");
    assert_eq!(failed.status, RecoveryStatus::RecoveryFailed);
    assert_eq!(
        failed.last_recovery_error.as_deref(),
        Some("backend unavailable")
    );

    store
        .update_recovery_status("gw-load", RecoveryStatus::Restored, None)
        .await
        .expect("clear recovery failure");
    let restored = store
        .get_session("gw-load")
        .await
        .expect("get session")
        .expect("session exists");
    assert_eq!(restored.status, RecoveryStatus::Restored);
    assert_eq!(restored.last_recovery_error, None);

    store
        .close_session("gw-load", "2026-07-12T01:00:00Z")
        .await
        .expect("close session");
    assert!(store
        .list_recoverable_sessions(None)
        .await
        .expect("list recoverable sessions")
        .is_empty());
}

#[tokio::test]
async fn recovery_failed_rows_are_excluded_from_the_eager_startup_batch() {
    // Regression test for a live incident: a `codex-acp` session that was
    // created but never completed a turn (so the backend never persisted
    // any rollout for it) fails `session/load`/`session/resume` on restart
    // with a permanent, deterministic backend error. Before this fix,
    // `list_recoverable_sessions` kept returning that same row on every
    // subsequent restart forever (only `status == 'closed'` was excluded),
    // so the eager startup batch re-attempted, and re-failed, identically,
    // every single time -- one doomed backend spawn per restart, plus a
    // permanently polluted `/health` `failed` counter.
    let store = PersistenceStore::open_in_memory().expect("open in-memory store");
    store
        .record_session_with_recovery(
            "gw-dead",
            "codex-acp",
            "backend-dead",
            None,
            "2026-07-18T21:45:00Z",
            "default",
            RecoveryMetadata {
                cwd: Some("/workspace/project".to_string()),
                recovery_params: Some(json!({})),
                status: RecoveryStatus::Active,
                recovery_method: RecoveryMethod::Load,
                last_recovery_error: None,
                ..RecoveryMetadata::default()
            },
        )
        .await
        .expect("record recoverable session");

    // First startup pass: the row is a genuine candidate.
    let candidates = store
        .list_recoverable_sessions(None)
        .await
        .expect("list recoverable sessions");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].gateway_session_id, "gw-dead");

    // The startup batch attempts it, the backend permanently rejects it
    // (mirrors codex-acp's real "no rollout found for thread id" error),
    // and the row is marked `RecoveryFailed` -- exactly what
    // `Router::recover_open_sessions`/`recover_open_sessions_shared` do on
    // an `Err` from `restore_open_session`.
    store
        .update_recovery_status(
            "gw-dead",
            RecoveryStatus::RecoveryFailed,
            Some(
                "backend rejected session/load: no rollout found for thread id backend-dead"
                    .to_string(),
            ),
        )
        .await
        .expect("record recovery failure");

    // Every subsequent restart's eager batch must never see this row
    // again -- it stays durable (inspectable via `get_session`) but is no
    // longer an unattended-retry candidate.
    let candidates_after_failure = store
        .list_recoverable_sessions(None)
        .await
        .expect("list recoverable sessions");
    assert!(
        candidates_after_failure.is_empty(),
        "a RecoveryFailed row must not be retried by the eager startup batch again"
    );

    let still_durable = store
        .get_session("gw-dead")
        .await
        .expect("get session")
        .expect("row still exists for inspection/on-demand retry");
    assert_eq!(still_durable.status, RecoveryStatus::RecoveryFailed);
    assert!(still_durable.last_recovery_error.is_some());
}

#[tokio::test]
async fn recovery_diagnostics_are_aggregated_and_errors_are_bounded() {
    let store = PersistenceStore::open_in_memory().expect("open in-memory store");
    for (id, status) in [
        ("gw-active", RecoveryStatus::Active),
        ("gw-restoring", RecoveryStatus::Restoring),
        ("gw-restored", RecoveryStatus::Restored),
        ("gw-failed", RecoveryStatus::RecoveryFailed),
    ] {
        store
            .record_session_with_recovery(
                id,
                "codex-acp",
                format!("backend-{id}"),
                None,
                "2026-07-16T00:00:00Z",
                "default",
                RecoveryMetadata {
                    status,
                    recovery_method: RecoveryMethod::Load,
                    ..RecoveryMetadata::default()
                },
            )
            .await
            .expect("record recovery row");
    }
    store
        .close_session("gw-active", "2026-07-16T01:00:00Z")
        .await
        .expect("close session");
    store
        .update_recovery_status(
            "gw-failed",
            RecoveryStatus::RecoveryFailed,
            Some(format!("first line\n{}", "x".repeat(700))),
        )
        .await
        .expect("persist bounded error");

    let counts = store
        .recovery_status_counts()
        .await
        .expect("read recovery diagnostics");
    assert_eq!(counts.active, 0);
    assert_eq!(counts.restoring, 1);
    assert_eq!(counts.restored, 1);
    assert_eq!(counts.recovery_failed, 1);
    assert_eq!(counts.closed, 1);
    let failed = store
        .get_session("gw-failed")
        .await
        .expect("get failed row")
        .expect("failed row exists");
    let error = failed.last_recovery_error.expect("bounded error");
    assert!(error.len() <= 515);
    assert!(!error.contains('\n'));
}

#[tokio::test]
async fn bridge_binding_metadata_round_trips_and_overwrites_prior_selection() {
    let store = PersistenceStore::open_in_memory().expect("open in-memory store");
    store
        .record_session_with_recovery(
            "gw-bridge",
            "codex-acp",
            "backend-bridge",
            None,
            "2026-07-12T00:00:00Z",
            "tenant-a",
            RecoveryMetadata {
                recovery_params: Some(json!({"cwd": "/workspace", "mcpServers": []})),
                recovery_method: RecoveryMethod::Load,
                ..RecoveryMetadata::default()
            },
        )
        .await
        .expect("record native session");

    store
        .update_bridge_binding(
            "gw-bridge",
            "virtual-bridge".to_string(),
            "codex/gpt-5".to_string(),
            json!({"permissionMode": "acceptEdits"}),
        )
        .await
        .expect("persist initial bridge binding");
    store
        .update_bridge_binding(
            "gw-bridge",
            "virtual-bridge".to_string(),
            "codex/gpt-5.5".to_string(),
            json!({"permissionMode": "plan"}),
        )
        .await
        .expect("persist updated bridge binding");

    let fetched = store
        .get_session("gw-bridge")
        .await
        .expect("get session")
        .expect("session exists");
    assert_eq!(fetched.bridge_session_id.as_deref(), Some("virtual-bridge"));
    assert_eq!(fetched.bridge_model_alias.as_deref(), Some("codex/gpt-5.5"));
    assert_eq!(
        fetched.bridge_config_options,
        Some(json!({"permissionMode": "plan"}))
    );
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
            "default",
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
            "default",
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
            "default",
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
                "default",
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

#[tokio::test]
async fn distinct_tenants_persist_and_round_trip_their_own_tenant_id() {
    // **Phase C (`acpx-tenant-isolation`).** `record_session`'s new
    // `tenant_id` argument round-trips through `get_session`/
    // `list_sessions` untouched -- this is the persistence-layer half of
    // the cross-restart tenant guarantee; `router.rs`'s
    // `rehydrate_session` is what actually enforces it against the
    // *requesting* tenant, tested end to end in
    // `acpx-server/tests/tenant_isolation_test.rs`.
    let store = PersistenceStore::open_in_memory().expect("open in-memory store");
    store
        .record_session(
            "gw-a",
            "codex-acp",
            "backend-a",
            None,
            "2026-07-12T00:00:00Z",
            "tenant-a",
        )
        .await
        .expect("record tenant-a session");
    store
        .record_session(
            "gw-b",
            "codex-acp",
            "backend-b",
            None,
            "2026-07-12T00:00:01Z",
            "tenant-b",
        )
        .await
        .expect("record tenant-b session");

    let a = store
        .get_session("gw-a")
        .await
        .expect("get_session")
        .expect("tenant-a session exists");
    assert_eq!(a.tenant_id, "tenant-a");
    let b = store
        .get_session("gw-b")
        .await
        .expect("get_session")
        .expect("tenant-b session exists");
    assert_eq!(b.tenant_id, "tenant-b");

    let all = store.list_sessions().await.expect("list_sessions");
    assert_eq!(all.len(), 2);
    assert!(all.iter().any(|s| s.tenant_id == "tenant-a"));
    assert!(all.iter().any(|s| s.tenant_id == "tenant-b"));
}

#[tokio::test]
async fn pre_tenant_id_database_migrates_existing_rows_to_default() {
    // Simulates a database file created by a version of this crate before
    // `tenant_id` existed: build the pre-Phase-C schema by hand (no
    // `tenant_id` column at all), insert a row the old way, then reopen it
    // through `PersistenceStore::open` (the real migration path) and
    // confirm the pre-existing row backfills to `"default"` rather than
    // the open failing or the row silently vanishing.
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("pre-tenant.sqlite3");
    {
        let conn = rusqlite::Connection::open(&db_path).expect("open raw connection");
        conn.execute_batch(
            "CREATE TABLE sessions (
                gateway_session_id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                backend_session_id TEXT NOT NULL,
                profile_name TEXT,
                created_at TEXT NOT NULL,
                closed_at TEXT
            );",
        )
        .expect("create pre-tenant-id schema");
        conn.execute(
            "INSERT INTO sessions \
             (gateway_session_id, agent_id, backend_session_id, created_at) \
             VALUES ('gw-old', 'codex-acp', 'backend-old', '2026-07-01T00:00:00Z')",
            [],
        )
        .expect("insert pre-migration row");
    }

    let store = PersistenceStore::open(&db_path).expect("reopen through migration path");
    let migrated = store
        .get_session("gw-old")
        .await
        .expect("get_session")
        .expect("pre-existing row survives migration");
    assert_eq!(migrated.tenant_id, "default");

    // The migration must also be safe to run again on an already-migrated
    // database (every `PersistenceStore::open` call re-runs it) -- reopen
    // once more and confirm nothing breaks and the row is unchanged.
    let store2 = PersistenceStore::open(&db_path).expect("reopen a second time");
    let migrated_again = store2
        .get_session("gw-old")
        .await
        .expect("get_session")
        .expect("row still present");
    assert_eq!(migrated_again.tenant_id, "default");
}

#[tokio::test]
async fn pre_recovery_database_migrates_all_recovery_columns_idempotently() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("pre-recovery.sqlite3");
    {
        let conn = rusqlite::Connection::open(&db_path).expect("open raw connection");
        conn.execute_batch(
            "CREATE TABLE sessions (
                gateway_session_id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                backend_session_id TEXT NOT NULL,
                profile_name TEXT,
                created_at TEXT NOT NULL,
                closed_at TEXT,
                tenant_id TEXT NOT NULL DEFAULT 'default'
            );
            INSERT INTO sessions
                (gateway_session_id, agent_id, backend_session_id, created_at, tenant_id)
            VALUES
                ('gw-old', 'codex-acp', 'backend-old', '2026-07-01T00:00:00Z', 'default');",
        )
        .expect("create pre-recovery schema");
    }

    let store = PersistenceStore::open(&db_path).expect("migrate recovery columns");
    let migrated = store
        .get_session("gw-old")
        .await
        .expect("get session")
        .expect("pre-existing row survives migration");
    assert_eq!(migrated.status, RecoveryStatus::Active);
    assert_eq!(migrated.recovery_method, RecoveryMethod::None);
    assert_eq!(migrated.cwd, None);
    assert_eq!(migrated.recovery_params, None);
    assert_eq!(migrated.last_recovery_error, None);
    assert!(!migrated.pinned);
    assert_eq!(migrated.created_at_unix_nanos, None);
    assert_eq!(migrated.last_activity_at_unix_nanos, None);
    assert_eq!(migrated.bridge_session_id, None);
    assert_eq!(migrated.bridge_model_alias, None);
    assert_eq!(migrated.bridge_config_options, None);

    PersistenceStore::open(&db_path).expect("rerun idempotent recovery migration");
}
