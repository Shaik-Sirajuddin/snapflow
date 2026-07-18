//! Managed ACP capability probe coverage. The probe must not create a
//! gateway session, and it must close its backend-local disposable session.

use acpx_conductor::SpawnSpec;
use acpx_core::router::Router;
use std::fs;

const PROBE_BACKEND: &str = r#"
while IFS= read -r line; do
  if echo "$line" | grep -q '"method":"initialize"'; then
    printf '{"jsonrpc":"2.0","id":0,"result":{"agentInfo":{"version":"1.2.3"}}}\n'
  elif echo "$line" | grep -q '"method":"session/new"'; then
    printf '{"jsonrpc":"2.0","id":"acpx-capability-probe-new","result":{"sessionId":"probe-session","configOptions":[{"id":"model","name":"Model","category":"model","options":[{"value":"haiku","name":"Claude Haiku"}]},{"id":"permissionMode","name":"Permissions","category":"permission","options":[{"value":"acceptEdits","name":"Accept edits"}]}]}}\n'
  elif echo "$line" | grep -q '"method":"session/close"'; then
    printf 'closed\n' >> "$PROBE_LOG"
    printf '{"jsonrpc":"2.0","id":"acpx-capability-probe-close","result":{}}\n'
  fi
done
"#;

#[tokio::test]
async fn probe_discovers_and_caches_models_and_permission_modes() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("probe.log");
    let mut spec = SpawnSpec::new("sh", vec!["-c".to_string(), PROBE_BACKEND.to_string()]);
    spec.env
        .insert("PROBE_LOG".to_string(), log.display().to_string());

    let mut router = Router::new("probe-agent");
    router.register_agent("probe-agent", spec);

    let first = router
        .probe_adapter_capabilities("probe-agent", "/tmp")
        .await
        .expect("first probe");
    assert_eq!(first.adapter_version.as_deref(), Some("1.2.3"));
    assert_eq!(first.models[0].value, "haiku");
    assert_eq!(first.permission_modes[0].value, "acceptEdits");
    assert!(first.auth_methods.is_empty());

    let second = router
        .probe_adapter_capabilities("probe-agent", "/tmp")
        .await
        .expect("cached probe");
    assert_eq!(second, first);
    assert_eq!(fs::read_to_string(log).unwrap().lines().count(), 1);
}
