//! Integration tests for the gateway-native `agents/*` methods, backed by
//! `acpx-registry`'s bundled fallback (no live network dependency -- the
//! router's registry cache falls back automatically per
//! `acpx_registry::fetch_registry_or_fallback`'s contract).

use acpx_core::router::Router;
use serde_json::json;

#[tokio::test]
async fn agents_list_reports_the_big_three_from_the_fallback_registry() {
    let mut router = Router::new("codex-acp");
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "agents/list",
        "params": {}
    });
    let response = router.dispatch(request).await.expect("agents/list");
    let agents = response["result"]["agents"].as_array().unwrap();
    let ids: Vec<&str> = agents.iter().map(|a| a["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"claude-acp"));
    assert!(ids.contains(&"codex-acp"));
    assert!(ids.contains(&"gemini"));
}

#[tokio::test]
async fn agents_status_for_known_npx_agent_reflects_node_npm_presence() {
    let mut router = Router::new("codex-acp");
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "agents/status",
        "params": {"id": "codex-acp"}
    });
    let response = router.dispatch(request).await.expect("agents/status");
    // Node/npm are present in this environment (verified in Phase 0), so
    // status should be "installed", not "runtime_missing"/"not_installed".
    assert_eq!(response["result"]["status"], json!("installed"));
}

#[tokio::test]
async fn agents_status_for_unknown_agent_id_errors() {
    let mut router = Router::new("codex-acp");
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "agents/status",
        "params": {"id": "not-a-real-agent"}
    });
    assert!(router.dispatch(request).await.is_err());
}

#[tokio::test]
async fn agents_install_for_npx_agent_succeeds_when_node_npm_present() {
    let mut router = Router::new("codex-acp");
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "agents/install",
        "params": {"id": "codex-acp"}
    });
    let response = router.dispatch(request).await.expect("agents/install");
    assert_eq!(response["result"]["id"], json!("codex-acp"));
}
