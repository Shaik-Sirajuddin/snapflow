//! End-to-end lifecycle reaper coverage against a real supervised shell
//! backend: expiry must close the backend session before the gateway mapping
//! is removed, while pinned sessions remain retained.

use std::time::Duration;

use acpx_conductor::SpawnSpec;
use acpx_core::{LifecycleConfig, Router};
use serde_json::json;

const BACKEND: &str = r#"
while IFS= read -r line; do
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  printf '%s\n' "$method" >> "$REAPER_LOG"
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
  fi
done
"#;

fn router(log: &std::path::Path) -> Router {
    let mut router = Router::new("stand-in").with_lifecycle_config(LifecycleConfig {
        idle_session_ttl: Duration::from_nanos(1),
        ..Default::default()
    });
    let mut spec = SpawnSpec::new("sh", vec!["-c".to_string(), BACKEND.to_string()]);
    spec.env
        .insert("REAPER_LOG".to_string(), log.display().to_string());
    router.register_agent("stand-in", spec);
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
async fn expired_session_is_closed_before_the_mapping_is_removed() {
    let log = std::env::temp_dir().join(format!("acpx-reaper-{}.log", uuid::Uuid::new_v4()));
    let mut router = router(&log);
    let session_id = new_session(&mut router, 1).await;
    tokio::time::sleep(Duration::from_millis(1)).await;

    let report = router
        .reap_expired_sessions(std::time::Instant::now())
        .await;
    assert_eq!(report.closed, 1);
    assert_eq!(report.failed, 0);

    let log_contents = tokio::fs::read_to_string(&log).await.expect("reaper log");
    assert!(log_contents.lines().any(|line| line == "session/close"));
    assert!(
        router
            .dispatch(json!({
                "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
                "params": {"sessionId": session_id, "prompt": []}
            }))
            .await
            .is_err(),
        "expired mapping must no longer accept a prompt"
    );
    let _ = tokio::fs::remove_file(log).await;
}

#[tokio::test]
async fn pinned_session_is_not_a_reaper_candidate() {
    let log = std::env::temp_dir().join(format!("acpx-reaper-pin-{}.log", uuid::Uuid::new_v4()));
    let mut router = router(&log);
    let session_id = new_session(&mut router, 1).await;
    router
        .set_session_pinned(&acpx_core::TenantId::default_tenant(), &session_id, true)
        .await
        .expect("pin session");
    tokio::time::sleep(Duration::from_millis(1)).await;
    let report = router
        .reap_expired_sessions(std::time::Instant::now())
        .await;
    assert_eq!(report.closed, 0);
    assert_eq!(report.failed, 0);
    let contents = tokio::fs::read_to_string(&log).await.unwrap_or_default();
    assert!(!contents.lines().any(|line| line == "session/close"));
    let _ = tokio::fs::remove_file(log).await;
}
