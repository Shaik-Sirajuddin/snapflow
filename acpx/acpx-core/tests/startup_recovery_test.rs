//! Proves proactive startup recovery sends backend `session/load` before a
//! recovered gateway session can receive a normal prompt.

use acpx_conductor::SpawnSpec;
use acpx_core::persistence::{
    sessions::{RecoveryMetadata, RecoveryMethod, RecoveryStatus},
    PersistenceStore,
};
use acpx_core::router::Router;
use serde_json::json;
use std::time::Duration;

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
