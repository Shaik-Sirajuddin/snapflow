//! Profile-backed *session*-level process isolation tests
//! (`ACPX_SESSION_PROCESS_ISOLATION`, `backend_process_model` hardening
//! item, `acp-gateway-daemon` plan) -- mirrors
//! `tenant_process_isolation_test.rs`'s structure/style, but asserts on
//! per-session rather than per-tenant process allocation. Uses a real
//! shell child rather than a supervisor mock so assertions prove actual
//! OS process allocation, not just supervisor-key bookkeeping.

use acpx_conductor::SpawnSpec;
use acpx_core::{router::Router, TenantId};
use serde_json::json;

const BACKEND: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-session"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
  fi
done
"#;

fn backend_spec() -> SpawnSpec {
    SpawnSpec::new("sh", vec!["-c".to_string(), BACKEND.to_string()])
}

async fn create_profile(router: &mut Router) {
    router.register_agent("stand-in", backend_spec());
    router
        .dispatch(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "profiles/create",
            "params": {"name": "shared-profile", "agent_id": "stand-in"}
        }))
        .await
        .expect("profiles/create");
}

/// Returns the gateway session id minted for this call.
async fn open_profile_session(router: &mut Router, tenant: &TenantId, id: u64) -> String {
    let response = router
        .dispatch_for_tenant(
            tenant,
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "session/new",
                "params": {"cwd": "/tmp", "_acpx": {"profile": "shared-profile"}}
            }),
        )
        .await
        .expect("profile-backed session/new");
    response["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string()
}

#[tokio::test]
async fn session_process_isolation_uses_distinct_pids_for_the_same_profile_and_tenant() {
    let mut router = Router::new("unused").with_session_process_isolation(true);
    let tenant = TenantId::from("tenant-a");
    create_profile(&mut router).await;

    let session_a = open_profile_session(&mut router, &tenant, 2).await;
    let session_b = open_profile_session(&mut router, &tenant, 3).await;

    let pid_a = router
        .process_id_for_session(&tenant, &session_a)
        .await
        .expect("session A process");
    let pid_b = router
        .process_id_for_session(&tenant, &session_b)
        .await
        .expect("session B process");
    assert_ne!(
        pid_a, pid_b,
        "each session must receive its own dedicated backend process"
    );
}

#[tokio::test]
async fn session_process_isolation_is_off_by_default() {
    let mut router = Router::new("unused");
    let tenant = TenantId::from("tenant-a");
    create_profile(&mut router).await;

    let session_a = open_profile_session(&mut router, &tenant, 2).await;
    let session_b = open_profile_session(&mut router, &tenant, 3).await;

    let pid_a = router
        .process_id_for_session(&tenant, &session_a)
        .await
        .expect("session A process");
    let pid_b = router
        .process_id_for_session(&tenant, &session_b)
        .await
        .expect("session B process");
    assert_eq!(
        pid_a, pid_b,
        "default mode must preserve one shared backend process per profile"
    );
}

/// Composability: both flags enabled together layer the session key on
/// top of the tenant key, and two tenants each running one session still
/// never collide.
#[tokio::test]
async fn session_process_isolation_composes_with_tenant_process_isolation() {
    let mut router = Router::new("unused")
        .with_tenant_process_isolation(true)
        .with_session_process_isolation(true);
    let tenant_a = TenantId::from("tenant-a");
    let tenant_b = TenantId::from("tenant-b");
    create_profile(&mut router).await;

    let session_a = open_profile_session(&mut router, &tenant_a, 2).await;
    let session_b = open_profile_session(&mut router, &tenant_b, 3).await;

    let pid_a = router
        .process_id_for_session(&tenant_a, &session_a)
        .await
        .expect("tenant A session process");
    let pid_b = router
        .process_id_for_session(&tenant_b, &session_b)
        .await
        .expect("tenant B session process");
    assert_ne!(
        pid_a, pid_b,
        "distinct tenants and sessions never share a process"
    );
}

/// Closing one session-isolated session must stop *only* its own
/// dedicated process, never a sibling session's -- proves
/// `Router::stop_if_session_scoped`'s safety property against the actual
/// reaper close path, not just its key-matching logic in isolation.
#[tokio::test]
async fn reaping_one_session_isolated_session_stops_only_its_own_process() {
    use std::time::{Duration, Instant};

    let mut router = Router::new("unused").with_session_process_isolation(true);
    let tenant = TenantId::from("tenant-a");
    // Force every session to be immediately reap-eligible once idle.
    let lifecycle = acpx_core::LifecycleConfig {
        idle_session_ttl: Duration::from_millis(1),
        ..Default::default()
    };
    router = router.with_lifecycle_config(lifecycle);
    create_profile(&mut router).await;

    let session_a = open_profile_session(&mut router, &tenant, 2).await;
    let session_b = open_profile_session(&mut router, &tenant, 3).await;

    // Capture each session's own supervisor key *before* reaping removes
    // its registry entry -- `process_status` needs the raw key, and
    // `supervisor_key_for_session` can no longer resolve it once the
    // session itself is gone.
    let key_a = router
        .supervisor_key_for_session(&tenant, &session_a)
        .expect("session A supervisor key");
    let key_b = router
        .supervisor_key_for_session(&tenant, &session_b)
        .expect("session B supervisor key");
    assert_ne!(key_a, key_b, "distinct sessions must mint distinct keys");

    router
        .process_id_for_session(&tenant, &session_a)
        .await
        .expect("session A process running before reap");
    router
        .process_id_for_session(&tenant, &session_b)
        .await
        .expect("session B process running before reap");

    tokio::time::sleep(Duration::from_millis(5)).await;
    let report = router.reap_expired_sessions(Instant::now()).await;
    assert_eq!(report.closed, 2, "both idle sessions should reap");

    // Both dedicated processes are now stopped -- neither survives its
    // own session's closure (this also proves the `:session:` marker
    // correctly matched real, router-minted keys, not just synthetic
    // ones built by hand in a unit test). `Supervisor::stop` removes the
    // entry outright, so `status` reports `NotStarted`, not `Exited`
    // (that variant is for a process that died on its own, unprompted).
    use acpx_conductor::supervisor::ProcessStatus;
    assert_eq!(router.process_status(&key_a), ProcessStatus::NotStarted);
    assert_eq!(router.process_status(&key_b), ProcessStatus::NotStarted);
}

/// **Regression test for a real bug** (`connector_reference_lifecycle`
/// hardening): an explicit client `session/close` -- the common case,
/// not just idle reaping -- used to evict the `SessionRegistry` entry
/// without ever calling `stop_if_session_scoped`, so a session-isolated
/// backend process leaked forever the moment a well-behaved client
/// closed its own session rather than letting it idle out. Covers
/// `Router::dispatch` (`dispatch_proxied`)'s direct path; the
/// concurrency-safe `dispatch_shared`/`dispatch_proxied_shared` twin
/// shares the exact same fix in the same commit.
#[tokio::test]
async fn explicit_session_close_stops_its_own_session_isolated_process() {
    use acpx_conductor::supervisor::ProcessStatus;

    let mut router = Router::new("unused").with_session_process_isolation(true);
    let tenant = TenantId::from("tenant-a");
    create_profile(&mut router).await;

    let session_a = open_profile_session(&mut router, &tenant, 2).await;
    let key_a = router
        .supervisor_key_for_session(&tenant, &session_a)
        .expect("session A supervisor key");
    router
        .process_id_for_session(&tenant, &session_a)
        .await
        .expect("session A process running before close");

    let response = router
        .dispatch_for_tenant(
            &tenant,
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "session/close",
                "params": {"sessionId": session_a}
            }),
        )
        .await
        .expect("session/close");
    assert!(response.get("result").is_some(), "{response:?}");

    assert_eq!(
        router.process_status(&key_a),
        ProcessStatus::NotStarted,
        "explicit session/close must stop the session's own dedicated process, \
         not just leave it running until an idle reap"
    );
}

/// **`connector_reference_lifecycle`.** `connector_idle_shutdown_ttl`
/// covers the *shared*, profile-scoped backend process model (session
/// process isolation off): a shared process must survive as long as
/// *any* session still references its supervisor key, get a grace
/// period once the last one closes, and only actually stop once
/// `reap_unreferenced_backends` observes that grace period has fully
/// elapsed with the key still unreferenced.
#[tokio::test]
async fn shared_backend_stops_after_idle_shutdown_ttl_once_unreferenced() {
    use acpx_conductor::supervisor::ProcessStatus;
    use acpx_core::lifecycle::LifecycleConfig;

    let ttl = std::time::Duration::from_secs(30);
    let mut router = Router::new("unused").with_lifecycle_config(LifecycleConfig {
        connector_idle_shutdown_ttl: Some(ttl),
        ..LifecycleConfig::default()
    });
    let tenant = TenantId::from("tenant-a");
    create_profile(&mut router).await;

    let session_a = open_profile_session(&mut router, &tenant, 1).await;
    let session_b = open_profile_session(&mut router, &tenant, 2).await;
    let key = router
        .supervisor_key_for_session(&tenant, &session_a)
        .expect("shared supervisor key");
    assert_eq!(
        key,
        router
            .supervisor_key_for_session(&tenant, &session_b)
            .expect("shared supervisor key"),
        "both sessions share the same profile-backed process"
    );

    // Closing one of two referencing sessions must not touch the still-
    // referenced shared process.
    close_session(&mut router, &tenant, &session_a, 3).await;
    assert_eq!(
        router.reap_unreferenced_backends(std::time::Instant::now()).await,
        0,
        "process is still referenced by session B"
    );
    assert_eq!(router.process_status(&key), ProcessStatus::Running);

    // Closing the last referencing session starts the grace period, but
    // a check before the TTL elapses must not stop it yet.
    close_session(&mut router, &tenant, &session_b, 4).await;
    assert_eq!(
        router
            .reap_unreferenced_backends(std::time::Instant::now() + ttl / 2)
            .await,
        0,
        "grace period has not elapsed yet"
    );
    assert_eq!(router.process_status(&key), ProcessStatus::Running);

    // Once the TTL has fully elapsed with the key still unreferenced,
    // the shared process is stopped. `Instant::now() + ttl` (rather than
    // an earlier fixed baseline plus `ttl`) guarantees this is strictly
    // after whatever real `Instant::now()` `mark_unreferenced_if_idle`
    // itself recorded above, regardless of how much wall time the
    // preceding dispatch calls actually took.
    assert_eq!(
        router
            .reap_unreferenced_backends(std::time::Instant::now() + ttl)
            .await,
        1,
        "grace period elapsed with zero referencing sessions"
    );
    assert_eq!(router.process_status(&key), ProcessStatus::NotStarted);
}

/// A new session opened against the same profile before the grace
/// period elapses cancels the pending shutdown -- the shared process
/// must never be stopped out from under a session that started
/// referencing it again in time.
#[tokio::test]
async fn shared_backend_idle_shutdown_is_cancelled_by_a_new_session() {
    use acpx_conductor::supervisor::ProcessStatus;
    use acpx_core::lifecycle::LifecycleConfig;

    let ttl = std::time::Duration::from_secs(30);
    let mut router = Router::new("unused").with_lifecycle_config(LifecycleConfig {
        connector_idle_shutdown_ttl: Some(ttl),
        ..LifecycleConfig::default()
    });
    let tenant = TenantId::from("tenant-a");
    create_profile(&mut router).await;

    let session_a = open_profile_session(&mut router, &tenant, 1).await;
    let key = router
        .supervisor_key_for_session(&tenant, &session_a)
        .expect("shared supervisor key");
    close_session(&mut router, &tenant, &session_a, 2).await;
    assert_eq!(
        router
            .reap_unreferenced_backends(std::time::Instant::now())
            .await,
        0
    );

    // A fresh session against the same profile re-references the key
    // before the TTL elapses.
    let _session_b = open_profile_session(&mut router, &tenant, 3).await;
    assert_eq!(
        router
            .reap_unreferenced_backends(std::time::Instant::now() + ttl)
            .await,
        0,
        "a new referencing session must cancel the pending shutdown"
    );
    assert_eq!(router.process_status(&key), ProcessStatus::Running);
}

/// Closes `session_id` via a real `session/close` dispatch and asserts
/// it succeeded.
async fn close_session(router: &mut Router, tenant: &TenantId, session_id: &str, id: u64) {
    let response = router
        .dispatch_for_tenant(
            tenant,
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "session/close",
                "params": {"sessionId": session_id}
            }),
        )
        .await
        .expect("session/close");
    assert!(response.get("result").is_some(), "{response:?}");
}
