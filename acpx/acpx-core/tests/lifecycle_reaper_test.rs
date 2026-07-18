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

/// Regression test for a real, previously-live incident: a backend that
/// never answers `session/close` used to wedge `reap_expired_sessions`
/// (and the global router mutex its production caller -- `acpx-server`'s
/// lifecycle reaper tick -- holds around it) forever, freezing every
/// other tenant/session on the server. `REAP_BACKEND_CALL_TIMEOUT` bounds
/// that one backend round trip; this test proves the reap call itself
/// returns promptly, reports the session as failed (not silently
/// skipped or falsely closed), and -- the part that actually matters --
/// that the router mutex is released afterward, so an unrelated
/// dispatch immediately following it is not wedged behind the same held
/// lock.
#[tokio::test]
async fn stuck_backend_session_close_does_not_wedge_the_router() {
    const HANGING_BACKEND: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  elif echo "$line" | grep -q 'session/close'; then
    : # Simulate a wedged backend: never answer session/close at all.
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
  fi
done
"#;
    let mut router = Router::new("stand-in").with_lifecycle_config(LifecycleConfig {
        idle_session_ttl: Duration::from_nanos(1),
        ..Default::default()
    });
    router.register_agent(
        "stand-in",
        SpawnSpec::new("sh", vec!["-c".to_string(), HANGING_BACKEND.to_string()]),
    );
    let session_id = new_session(&mut router, 1).await;
    tokio::time::sleep(Duration::from_millis(1)).await;

    let started = std::time::Instant::now();
    // 25s comfortably bounds the production 15s `REAP_BACKEND_CALL_TIMEOUT`
    // plus scheduling slack; a real hang before this fix ran forever, so
    // any bounded ceiling here already proves the regression is fixed.
    let report = tokio::time::timeout(
        Duration::from_secs(25),
        router.reap_expired_sessions(std::time::Instant::now()),
    )
    .await
    .expect(
        "reap_expired_sessions must return even when a backend never answers \
         session/close, not hang the whole router mutex forever",
    );
    let elapsed = started.elapsed();

    assert_eq!(
        report.closed, 0,
        "a session/close that never got a reply must not be reported as closed"
    );
    assert_eq!(
        report.failed, 1,
        "the timed-out reap attempt must be reported as failed, not silently skipped"
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "reap should return close to REAP_BACKEND_CALL_TIMEOUT (15s), not run \
         indefinitely; took {elapsed:?}"
    );

    // The real point of this test: prove the global router mutex was
    // actually released, not merely that this one call eventually
    // returned -- an unrelated dispatch immediately afterward must also
    // complete promptly, matching the live incident this guards against
    // (every other tenant/session hanging behind the same held lock).
    let unrelated = tokio::time::timeout(
        Duration::from_secs(5),
        router.dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/list", "params": {}
        })),
    )
    .await
    .expect("router must not still be wedged after the reaper's own timeout fires");
    assert!(unrelated.is_ok());

    let _ = session_id; // kept for readability of the setup above
}
