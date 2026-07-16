//! Proves proactive startup recovery sends backend `session/load` before a
//! recovered gateway session can receive a normal prompt.

use acpx_conductor::SpawnSpec;
use acpx_core::persistence::{
    sessions::{RecoveryMetadata, RecoveryMethod, RecoveryStatus},
    PersistenceStore,
};
use acpx_core::{
    recover_open_sessions_shared,
    router::{Router, StartupRecoveryPolicy},
};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const RECORDING_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  printf '%s\t%s\n' "$method" "$line" >> "$RECOVERY_LOG"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([^,}]*\).*/\1/p')
  case "$method" in
    session/load)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"loaded":true}}\n' "$id"
      ;;
    session/resume)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"resumed":true}}\n' "$id"
      ;;
    session/prompt)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"prompted":true}}\n' "$id"
      ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      ;;
  esac
done
"#;

fn recording_backend_spec(log_path: &std::path::Path) -> SpawnSpec {
    let mut spec = SpawnSpec::new(
        "sh",
        vec!["-c".to_string(), RECORDING_BACKEND_SCRIPT.to_string()],
    );
    spec.env.insert(
        "RECOVERY_LOG".to_string(),
        log_path.to_string_lossy().into_owned(),
    );
    spec
}

fn delayed_recovery_spec(delay_seconds: u64, rejects_load: bool) -> SpawnSpec {
    let outcome = if rejects_load {
        r#"printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32000,"message":"rejected"}}\n' "$id""#
    } else {
        r#"printf '{"jsonrpc":"2.0","id":%s,"result":{"loaded":true}}\n' "$id""#
    };
    let script = format!(
        r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([^,}}]*\).*/\1/p')
  if printf '%s' "$line" | grep -q '"method":"session/load"'; then
    sleep {delay_seconds}
    {outcome}
  else
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{}}}}\n' "$id"
  fi
done
"#
    );
    SpawnSpec::new("sh", vec!["-c".to_string(), script])
}

async fn seed_load_candidate(store: &PersistenceStore, gateway_id: &str, agent_id: &str) {
    store
        .record_session_with_recovery(
            gateway_id,
            agent_id,
            format!("backend-{gateway_id}"),
            None,
            format!("2026-01-01T00:00:{gateway_id}Z"),
            "default",
            RecoveryMetadata {
                cwd: Some("/workspace".to_string()),
                recovery_params: Some(json!({"cwd": "/workspace"})),
                status: RecoveryStatus::Active,
                recovery_method: RecoveryMethod::Load,
                last_recovery_error: None,
            },
        )
        .await
        .expect("persist recovery candidate");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_recovery_loads_before_prompt_and_restores_gateway_session() {
    let store = PersistenceStore::open_in_memory().expect("open persistence store");
    store
        .record_session_with_recovery(
            "gateway-recovered",
            "stand-in-agent",
            "backend-recovered",
            None,
            "2026-01-01T00:00:00Z",
            "default",
            RecoveryMetadata {
                cwd: Some("/workspace".to_string()),
                recovery_params: Some(json!({
                    "cwd": "/workspace",
                    "mcpServers": [{"name": "persisted-mcp", "command": "mcp"}]
                })),
                status: RecoveryStatus::Active,
                recovery_method: RecoveryMethod::Load,
                last_recovery_error: None,
            },
        )
        .await
        .expect("persist recovery candidate");

    let log_path = std::env::temp_dir().join(format!(
        "acpx-startup-recovery-{}.log",
        uuid::Uuid::new_v4()
    ));
    let mut router = Router::new("stand-in-agent").with_persistence(store.clone());
    router.register_agent("stand-in-agent", recording_backend_spec(&log_path));

    let report = router
        .recover_open_sessions()
        .await
        .expect("startup recovery");
    assert_eq!(report.restored, 1);
    assert_eq!(report.failed, 0);
    assert_eq!(report.skipped, 0);

    let prompt = router
        .dispatch(json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "session/prompt",
            "params": {"sessionId": "gateway-recovered", "prompt": []}
        }))
        .await
        .expect("prompt recovered session");
    assert_eq!(prompt["result"]["prompted"], true);

    let methods = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(contents) = tokio::fs::read_to_string(&log_path).await {
                let lines: Vec<_> = contents.lines().collect();
                if lines
                    .iter()
                    .any(|line| line.starts_with("session/prompt\t"))
                {
                    return lines.into_iter().map(str::to_string).collect::<Vec<_>>();
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("backend log includes prompt");

    let load_index = methods
        .iter()
        .position(|line| line.starts_with("session/load\t"))
        .expect("startup recovery sent session/load");
    let prompt_index = methods
        .iter()
        .position(|line| line.starts_with("session/prompt\t"))
        .expect("recovered session accepted prompt");
    assert!(
        load_index < prompt_index,
        "session/load must precede any prompt: {methods:?}"
    );
    assert!(methods[load_index].contains("\"sessionId\":\"backend-recovered\""));
    assert!(methods[load_index].contains("\"cwd\":\"/workspace\""));
    assert!(methods[load_index].contains("\"name\":\"persisted-mcp\""));

    let persisted = store
        .get_session("gateway-recovered")
        .await
        .expect("read recovery status")
        .expect("recovery row remains");
    assert_eq!(persisted.status, RecoveryStatus::Restored);
    let _ = tokio::fs::remove_file(log_path).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_recovery_uses_persisted_resume_method_before_prompt() {
    let store = PersistenceStore::open_in_memory().expect("open persistence store");
    store
        .record_session_with_recovery(
            "gateway-resumed",
            "stand-in-agent",
            "backend-resumed",
            None,
            "2026-01-01T00:00:00Z",
            "default",
            RecoveryMetadata {
                cwd: Some("/workspace".to_string()),
                recovery_params: Some(json!({"cwd": "/workspace"})),
                status: RecoveryStatus::Active,
                recovery_method: RecoveryMethod::Resume,
                last_recovery_error: None,
            },
        )
        .await
        .expect("persist resume recovery candidate");

    let log_path = std::env::temp_dir().join(format!(
        "acpx-startup-resume-recovery-{}.log",
        uuid::Uuid::new_v4()
    ));
    let mut router = Router::new("stand-in-agent").with_persistence(store.clone());
    router.register_agent("stand-in-agent", recording_backend_spec(&log_path));

    let report = router
        .recover_open_sessions()
        .await
        .expect("startup resume recovery");
    assert_eq!(report.restored, 1);
    assert_eq!(report.failed, 0);
    assert_eq!(report.skipped, 0);

    let prompt = router
        .dispatch(json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "session/prompt",
            "params": {"sessionId": "gateway-resumed", "prompt": []}
        }))
        .await
        .expect("prompt resumed session");
    assert_eq!(prompt["result"]["prompted"], true);

    let methods = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(contents) = tokio::fs::read_to_string(&log_path).await {
                let lines: Vec<_> = contents.lines().collect();
                if lines
                    .iter()
                    .any(|line| line.starts_with("session/prompt\t"))
                {
                    return lines.into_iter().map(str::to_string).collect::<Vec<_>>();
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("backend log includes prompt");
    assert!(
        methods
            .iter()
            .any(|line| line.starts_with("session/resume\t")),
        "startup recovery must use session/resume: {methods:?}"
    );
    assert!(
        !methods
            .iter()
            .any(|line| line.starts_with("session/load\t")),
        "resume candidate must not be rewritten to session/load: {methods:?}"
    );
    let _ = tokio::fs::remove_file(log_path).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_recovery_scheduler_recovers_distinct_connectors_concurrently() {
    let store = PersistenceStore::open_in_memory().expect("open persistence store");
    seed_load_candidate(&store, "one", "agent-one").await;
    seed_load_candidate(&store, "two", "agent-two").await;

    let mut router = Router::new("unused").with_persistence(store);
    router.register_agent("agent-one", delayed_recovery_spec(1, false));
    router.register_agent("agent-two", delayed_recovery_spec(1, false));
    let router = Arc::new(Mutex::new(router));

    let started = Instant::now();
    let report = recover_open_sessions_shared(
        &router,
        StartupRecoveryPolicy {
            timeout: Duration::from_secs(3),
            concurrency: 2,
            fail_fast: false,
        },
    )
    .await
    .expect("parallel recovery");

    assert_eq!(report.restored, 2);
    assert_eq!(report.failed, 0);
    assert!(
        started.elapsed() < Duration::from_millis(1800),
        "two different connector processes should recover concurrently"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_recovery_scheduler_times_out_marks_failure_and_stops_connector() {
    use acpx_conductor::supervisor::ProcessStatus;

    let store = PersistenceStore::open_in_memory().expect("open persistence store");
    seed_load_candidate(&store, "timeout", "slow-agent").await;

    let mut router = Router::new("unused").with_persistence(store.clone());
    router.register_agent("slow-agent", delayed_recovery_spec(2, false));
    let router = Arc::new(Mutex::new(router));

    let report = recover_open_sessions_shared(
        &router,
        StartupRecoveryPolicy {
            timeout: Duration::from_millis(100),
            concurrency: 1,
            fail_fast: false,
        },
    )
    .await
    .expect("timeout is recorded rather than aborting non-fail-fast startup");

    assert_eq!(report.restored, 0);
    assert_eq!(report.failed, 1);
    let row = store
        .get_session("timeout")
        .await
        .expect("read recovery row")
        .expect("row exists");
    assert_eq!(row.status, RecoveryStatus::RecoveryFailed);
    assert!(row
        .last_recovery_error
        .as_deref()
        .is_some_and(|error| error.contains("timed out")));
    assert_eq!(
        router.lock().await.process_status("slow-agent"),
        ProcessStatus::NotStarted,
        "a timed-out stdio connector must be stopped before it can corrupt a later request"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_recovery_scheduler_fail_fast_surfaces_first_failed_session() {
    let store = PersistenceStore::open_in_memory().expect("open persistence store");
    seed_load_candidate(&store, "failing", "rejecting-agent").await;

    let mut router = Router::new("unused").with_persistence(store.clone());
    router.register_agent("rejecting-agent", delayed_recovery_spec(0, true));
    let router = Arc::new(Mutex::new(router));

    let error = recover_open_sessions_shared(
        &router,
        StartupRecoveryPolicy {
            timeout: Duration::from_secs(1),
            concurrency: 2,
            fail_fast: true,
        },
    )
    .await
    .expect_err("fail-fast recovery must keep startup unready");
    assert!(error.to_string().contains("startup recovery stopped"));
    assert_eq!(
        store
            .get_session("failing")
            .await
            .expect("read recovery row")
            .expect("row exists")
            .status,
        RecoveryStatus::RecoveryFailed
    );
}
