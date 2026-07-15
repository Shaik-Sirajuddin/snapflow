//! Router lifecycle capacity coverage using lightweight shell backends.

use acpx_conductor::SpawnSpec;
use acpx_core::router::{Router, RouterError};
use acpx_core::{LifecycleConfig, TenantId};
use serde_json::json;
use std::path::Path;

const STAND_IN_BACKEND_SCRIPT: &str = r#"
printf spawned > "$1"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;

fn stand_in_backend_spec(start_marker: &Path) -> SpawnSpec {
    SpawnSpec::new(
        "sh",
        vec![
            "-c".to_string(),
            STAND_IN_BACKEND_SCRIPT.to_string(),
            "sh".to_string(),
            start_marker.display().to_string(),
        ],
    )
}

fn session_new(id: u64, profile: Option<&str>) -> serde_json::Value {
    let mut params = json!({"cwd": "/tmp"});
    if let Some(profile) = profile {
        params["_acpx"] = json!({"profile": profile});
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/new",
        "params": params,
    })
}

#[tokio::test]
async fn global_capacity_rejects_before_starting_the_selected_backend() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let primary_marker = tempdir.path().join("primary-started");
    let rejected_marker = tempdir.path().join("rejected-started");
    let mut router = Router::new("primary").with_lifecycle_config(LifecycleConfig {
        max_sessions_total: 1,
        max_sessions_per_tenant: 2,
    });
    router.register_agent("primary", stand_in_backend_spec(&primary_marker));
    router.register_agent("rejected", stand_in_backend_spec(&rejected_marker));

    router
        .dispatch(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "profiles/create",
            "params": {"name": "rejected-profile", "agent_id": "rejected"},
        }))
        .await
        .expect("create profile for the second backend");

    router
        .dispatch(session_new(2, None))
        .await
        .expect("first session/new");
    assert!(primary_marker.exists(), "first backend should have started");

    let error = router
        .dispatch(session_new(3, Some("rejected-profile")))
        .await
        .expect_err("global capacity should reject the second session/new");
    assert!(matches!(
        error,
        RouterError::GlobalSessionCapacity {
            current: 1,
            limit: 1
        }
    ));
    assert!(
        !rejected_marker.exists(),
        "capacity must be checked before the selected backend is spawned"
    );
}

#[tokio::test]
async fn tenant_capacity_is_independent_for_dispatch_for_tenant() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut router = Router::new("stand-in").with_lifecycle_config(LifecycleConfig {
        max_sessions_total: 3,
        max_sessions_per_tenant: 1,
    });
    router.register_agent(
        "stand-in",
        stand_in_backend_spec(&tempdir.path().join("stand-in-started")),
    );
    let tenant_a = TenantId::from("tenant-a");
    let tenant_b = TenantId::from("tenant-b");

    router
        .dispatch_for_tenant(&tenant_a, session_new(1, None))
        .await
        .expect("tenant A's first session");
    router
        .dispatch_for_tenant(&tenant_b, session_new(2, None))
        .await
        .expect("tenant B's first session");

    let error = router
        .dispatch_for_tenant(&tenant_a, session_new(3, None))
        .await
        .expect_err("tenant A's second session should hit its own limit");
    assert!(matches!(
        error,
        RouterError::TenantSessionCapacity {
            ref tenant,
            current: 1,
            limit: 1
        } if tenant == "tenant-a"
    ));
}
