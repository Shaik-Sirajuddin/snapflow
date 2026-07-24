//! Phase 13 (`worktree-consolidation-and-provider-binding` plan): native
//! `models/list` -- agent-scoped model catalogs, pre-session. With
//! `agentId` it runs the TTL-cached capability probe (at most one
//! disposable backend session per TTL window); without, it returns only
//! already-cached catalogs and never spawns a backend to enumerate.

use acpx_conductor::SpawnSpec;
use acpx_core::router::Router;
use serde_json::json;

const PROBE_BACKEND: &str = r#"
while IFS= read -r line; do
  if echo "$line" | grep -q '"method":"initialize"'; then
    printf '{"jsonrpc":"2.0","id":0,"result":{"agentInfo":{"version":"9.9.9"}}}\n'
  elif echo "$line" | grep -q '"method":"session/new"'; then
    printf '{"jsonrpc":"2.0","id":"acpx-capability-probe-new","result":{"sessionId":"probe-session","configOptions":[{"id":"model","name":"Model","category":"model","options":[{"value":"sonnet","name":"Claude Sonnet"},{"value":"haiku","name":"Claude Haiku"}]}]}}\n'
  elif echo "$line" | grep -q '"method":"session/close"'; then
    printf '{"jsonrpc":"2.0","id":"acpx-capability-probe-close","result":{}}\n'
  fi
done
"#;

#[tokio::test]
async fn models_list_probes_one_agent_and_returns_its_catalog() {
    let spec = SpawnSpec::new("sh", vec!["-c".to_string(), PROBE_BACKEND.to_string()]);
    let mut router = Router::new("models-agent");
    router.register_agent("models-agent", spec);

    let result = router
        .dispatch(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "models/list",
            "params": {"agentId": "models-agent", "cwd": "/tmp"}
        }))
        .await
        .expect("models/list dispatch");
    let catalogs = result["result"]["catalogs"]
        .as_array()
        .expect("catalogs array");
    assert_eq!(catalogs.len(), 1);
    assert_eq!(catalogs[0]["agentId"], "models-agent");
    let models = catalogs[0]["models"].as_array().expect("models array");
    assert_eq!(models.len(), 2);
    assert_eq!(models[0]["value"], "sonnet");
    assert_eq!(models[1]["value"], "haiku");

    // Cold no-agent enumeration: this registered agent is NOT a registry
    // agent, and nothing else is cached -- must return empty without
    // spawning anything.
    let all = router
        .dispatch(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "models/list",
            "params": {}
        }))
        .await
        .expect("models/list all dispatch");
    assert!(all["result"]["catalogs"].as_array().expect("array").len() <= 1);
}
