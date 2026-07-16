//! End-to-end coverage for `retention_administration`
//! (`acpx-session-lifecycle`'s `lifecycle_contract_completion` phase):
//! the `session/retention/get|list|pin|unpin|set_ttl` JSON-RPC namespace,
//! tenant ownership isolation, and the per-tenant pin quota. Same
//! synthetic `sh`-script stand-in backend pattern used throughout this
//! crate's other router-level tests (see `lifecycle_reaper_test.rs`'s
//! doc comment).

use std::time::Duration;

use acpx_conductor::SpawnSpec;
use acpx_core::router::RouterError;
use acpx_core::{LifecycleConfig, Router};
use serde_json::json;

const STAND_IN_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
  fi
done
"#;

fn router_with_lifecycle(lifecycle: LifecycleConfig) -> Router {
    let mut router = Router::new("stand-in").with_lifecycle_config(lifecycle);
    router.register_agent(
        "stand-in",
        SpawnSpec::new(
            "sh",
            vec!["-c".to_string(), STAND_IN_BACKEND_SCRIPT.to_string()],
        ),
    );
    router
}

async fn new_session(router: &mut Router, id: u64) -> String {
    let response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": id, "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .await
        .expect("session/new");
    response["result"]["sessionId"]
        .as_str()
        .expect("gateway session id")
        .to_string()
}

#[tokio::test]
async fn pin_unpin_and_get_round_trip_through_the_rpc_surface() {
    let mut router = router_with_lifecycle(LifecycleConfig::default());
    let session_id = new_session(&mut router, 1).await;

    let pin = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/retention/pin",
            "params": {"sessionId": session_id}
        }))
        .await
        .expect("session/retention/pin");
    assert_eq!(pin["result"]["pinned"], true);

    let get = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/retention/get",
            "params": {"sessionId": session_id}
        }))
        .await
        .expect("session/retention/get");
    assert_eq!(get["result"]["pinned"], true);
    assert_eq!(get["result"]["sessionId"], session_id);

    let unpin = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 4, "method": "session/retention/unpin",
            "params": {"sessionId": session_id}
        }))
        .await
        .expect("session/retention/unpin");
    assert_eq!(unpin["result"]["pinned"], false);
}

#[tokio::test]
async fn set_ttl_overrides_the_deployment_default_and_survives_a_clear() {
    let mut router = router_with_lifecycle(LifecycleConfig::default());
    let session_id = new_session(&mut router, 1).await;

    let set = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/retention/set_ttl",
            "params": {"sessionId": session_id, "idleTtlSeconds": 3600}
        }))
        .await
        .expect("session/retention/set_ttl");
    assert_eq!(set["result"]["customIdleTtlSeconds"], 3600);

    // Omitting `idleTtlSeconds` clears the override back to "use the
    // deployment default".
    let clear = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/retention/set_ttl",
            "params": {"sessionId": session_id}
        }))
        .await
        .expect("session/retention/set_ttl (clear)");
    assert!(clear["result"]["customIdleTtlSeconds"].is_null());
}

#[tokio::test]
async fn a_short_custom_ttl_reaps_a_session_before_the_deployment_default_would() {
    let mut router = router_with_lifecycle(LifecycleConfig {
        idle_session_ttl: Duration::from_secs(3600), // deployment default: effectively never
        ..Default::default()
    });
    let session_id = new_session(&mut router, 1).await;

    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/retention/set_ttl",
            "params": {"sessionId": session_id, "idleTtlSeconds": 0}
        }))
        .await
        .expect("session/retention/set_ttl");

    // `idleTtlSeconds: 0` -> `Duration::from_secs(0)`, so any elapsed time
    // at all is "idle beyond the custom TTL", proving `reap_candidates`
    // actually reads `custom_idle_ttl`, not just the deployment default.
    tokio::time::sleep(Duration::from_millis(5)).await;
    let report = router
        .reap_expired_sessions(std::time::Instant::now())
        .await;
    assert_eq!(report.closed, 1, "{report:?}");
}

#[tokio::test]
async fn list_returns_only_the_calling_tenants_sessions() {
    let mut router = router_with_lifecycle(LifecycleConfig::default());
    let tenant_a = acpx_core::TenantId::from("tenant-a");
    let tenant_b = acpx_core::TenantId::from("tenant-b");

    let session_a = router
        .dispatch_for_tenant(
            &tenant_a,
            json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}}),
        )
        .await
        .expect("session/new tenant-a")["result"]["sessionId"]
        .as_str()
        .expect("gateway session id")
        .to_string();
    let _session_b = router
        .dispatch_for_tenant(
            &tenant_b,
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {"cwd": "/tmp"}}),
        )
        .await
        .expect("session/new tenant-b");

    let list_a = router
        .dispatch_for_tenant(
            &tenant_a,
            json!({"jsonrpc": "2.0", "id": 3, "method": "session/retention/list", "params": {}}),
        )
        .await
        .expect("session/retention/list tenant-a");
    let sessions = list_a["result"]["sessions"]
        .as_array()
        .expect("sessions array");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["sessionId"], session_a);

    // tenant-a can never resolve tenant-b's session via `get`, matching
    // every other tenant-ownership check in this crate.
    let cross_tenant_get = router
        .dispatch_for_tenant(
            &tenant_a,
            json!({
                "jsonrpc": "2.0", "id": 4, "method": "session/retention/get",
                "params": {"sessionId": _session_b["result"]["sessionId"]}
            }),
        )
        .await;
    assert!(cross_tenant_get.is_err());
}

#[tokio::test]
async fn pin_quota_rejects_a_pin_beyond_the_per_tenant_limit() {
    let mut router = router_with_lifecycle(LifecycleConfig {
        max_pinned_sessions_per_tenant: Some(1),
        ..Default::default()
    });
    let session_one = new_session(&mut router, 1).await;
    let session_two = new_session(&mut router, 2).await;

    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/retention/pin",
            "params": {"sessionId": session_one}
        }))
        .await
        .expect("first pin succeeds under the quota");

    let second_pin = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 4, "method": "session/retention/pin",
            "params": {"sessionId": session_two}
        }))
        .await;
    assert!(
        matches!(second_pin, Err(RouterError::PinQuotaExceeded { .. })),
        "{second_pin:?}"
    );

    // Re-pinning the already-pinned session must never itself trip the
    // quota it is already counted toward.
    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 5, "method": "session/retention/pin",
            "params": {"sessionId": session_one}
        }))
        .await
        .expect("re-pinning an already-pinned session stays within quota");
}

#[tokio::test]
async fn unpinning_frees_a_pin_quota_slot_for_another_session() {
    let mut router = router_with_lifecycle(LifecycleConfig {
        max_pinned_sessions_per_tenant: Some(1),
        ..Default::default()
    });
    let session_one = new_session(&mut router, 1).await;
    let session_two = new_session(&mut router, 2).await;

    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/retention/pin",
            "params": {"sessionId": session_one}
        }))
        .await
        .expect("first pin");
    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 4, "method": "session/retention/unpin",
            "params": {"sessionId": session_one}
        }))
        .await
        .expect("unpin");
    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 5, "method": "session/retention/pin",
            "params": {"sessionId": session_two}
        }))
        .await
        .expect("second session can now be pinned after the first freed its slot");
}

#[tokio::test]
async fn unknown_session_errors_on_every_retention_method() {
    let mut router = router_with_lifecycle(LifecycleConfig::default());
    for method in [
        "session/retention/get",
        "session/retention/pin",
        "session/retention/unpin",
        "session/retention/set_ttl",
    ] {
        let response = router
            .dispatch(json!({
                "jsonrpc": "2.0", "id": 1, "method": method,
                "params": {"sessionId": "does-not-exist", "idleTtlSeconds": 60}
            }))
            .await;
        assert!(
            response.is_err(),
            "{method} should error on an unknown session"
        );
    }
}
