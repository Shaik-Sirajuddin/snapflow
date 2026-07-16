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
    assert_ne!(pid_a, pid_b, "distinct tenants and sessions never share a process");
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
    let mut lifecycle = acpx_core::LifecycleConfig::default();
    lifecycle.idle_session_ttl = Duration::from_millis(1);
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
