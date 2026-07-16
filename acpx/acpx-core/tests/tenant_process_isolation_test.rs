//! Profile-backed tenant process isolation tests. These use a real shell
//! child rather than a supervisor mock so the assertions prove the physical
//! process allocation behavior operators configure.

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

async fn open_profile_session(router: &mut Router, tenant: &TenantId, id: u64) {
    router
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
}

#[tokio::test]
async fn tenant_process_isolation_uses_distinct_pids_for_one_profile() {
    let mut router = Router::new("unused").with_tenant_process_isolation(true);
    let tenant_a = TenantId::from("tenant-a");
    let tenant_b = TenantId::from("tenant-b");
    create_profile(&mut router).await;

    open_profile_session(&mut router, &tenant_a, 2).await;
    open_profile_session(&mut router, &tenant_b, 3).await;

    let a_pid = router
        .process_id("profile:shared-profile:tenant:tenant-a")
        .await
        .expect("tenant A process");
    let b_pid = router
        .process_id("profile:shared-profile:tenant:tenant-b")
        .await
        .expect("tenant B process");
    assert_ne!(a_pid, b_pid, "each tenant must receive its own backend PID");
}

#[tokio::test]
async fn tenant_process_isolation_is_off_by_default() {
    let mut router = Router::new("unused");
    let tenant_a = TenantId::from("tenant-a");
    let tenant_b = TenantId::from("tenant-b");
    create_profile(&mut router).await;

    open_profile_session(&mut router, &tenant_a, 2).await;
    let first_pid = router
        .process_id("profile:shared-profile")
        .await
        .expect("shared profile process");
    open_profile_session(&mut router, &tenant_b, 3).await;
    let second_pid = router
        .process_id("profile:shared-profile")
        .await
        .expect("shared profile process");

    assert_eq!(
        first_pid, second_pid,
        "default mode must preserve one shared backend process per profile"
    );
}

#[tokio::test]
async fn profiles_delete_stops_every_tenant_isolated_profile_process() {
    use acpx_conductor::supervisor::ProcessStatus;

    let mut router = Router::new("unused").with_tenant_process_isolation(true);
    let tenant_a = TenantId::from("tenant-a");
    let tenant_b = TenantId::from("tenant-b");
    create_profile(&mut router).await;
    open_profile_session(&mut router, &tenant_a, 2).await;
    open_profile_session(&mut router, &tenant_b, 3).await;

    router
        .dispatch(json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "profiles/delete",
            "params": {"name": "shared-profile"}
        }))
        .await
        .expect("profiles/delete");

    assert_eq!(
        router.process_status("profile:shared-profile:tenant:tenant-a"),
        ProcessStatus::NotStarted
    );
    assert_eq!(
        router.process_status("profile:shared-profile:tenant:tenant-b"),
        ProcessStatus::NotStarted
    );
}
