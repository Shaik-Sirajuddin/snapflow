//! Integration tests for the gateway-native `agents/*` methods, backed by
//! `acpx-registry`'s bundled fallback (no live network dependency -- the
//! router's registry cache falls back automatically per
//! `acpx_registry::fetch_registry_or_fallback`'s contract).

use acpx_core::router::Router;
use serde_json::json;
use std::sync::{Mutex, OnceLock};

/// Serialize real `agents/install` / npm pre-fetch tests so concurrent
/// suite workers don't stack three 120s `npm install`s and trip the
/// package-prefetch timeout.
fn install_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct TempHome {
    _dir: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
}

impl TempHome {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", dir.path());
        Self { _dir: dir, prev }
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}

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
async fn agents_status_for_known_npx_agent_is_not_installed_without_ready_marker() {
    let _guard = install_test_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _home = TempHome::new();
    let mut router = Router::new("codex-acp");
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "agents/status",
        "params": {"id": "codex-acp"}
    });
    let response = router.dispatch(request).await.expect("agents/status");
    // Node/npm present → not runtime_missing; without ready marker → not_installed.
    assert_eq!(response["result"]["status"], json!("not_installed"));
}

#[tokio::test]
async fn agents_install_then_status_is_installed_without_restart() {
    let _guard = install_test_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _home = TempHome::new();

    let mut router = Router::new("codex-acp");
    let install = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "agents/install",
        "params": {"id": "codex-acp"}
    });
    let install_resp = router.dispatch(install).await.expect("agents/install");
    assert_eq!(install_resp["result"]["id"], json!("codex-acp"));
    assert!(
        install_resp["result"]["outcome"]
            .as_str()
            .unwrap_or("")
            .contains("PackageReady"),
        "outcome={}",
        install_resp["result"]["outcome"]
    );

    let status = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "agents/status",
        "params": {"id": "codex-acp"}
    });
    let status_resp = router.dispatch(status).await.expect("agents/status");
    assert_eq!(status_resp["result"]["status"], json!("installed"));
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
    let _guard = install_test_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _home = TempHome::new();
    let mut router = Router::new("codex-acp");
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "agents/install",
        "params": {"id": "codex-acp"}
    });
    let response = router.dispatch(request).await.expect("agents/install");
    assert_eq!(response["result"]["id"], json!("codex-acp"));
    assert!(
        response["result"]["outcome"]
            .as_str()
            .unwrap_or("")
            .contains("PackageReady"),
        "outcome={}",
        response["result"]["outcome"]
    );
}

/// **`client_and_installer_contract` hardening, `acp-gateway-daemon`
/// plan.** `dispatch_shared`'s `agents/install` arm
/// (`dispatch_agents_install_shared`, added to stop this exact call from
/// holding the whole router mutex for the duration of a real
/// download/extract -- see that function's doc comment) must still
/// produce byte-for-byte the same `{id, outcome}` wire shape
/// `Router::dispatch` does, since real transports call the shared path
/// exclusively and `acpx-proto`'s `AgentInstallResult` type/round-trip
/// test assumes this exact shape.
#[tokio::test]
async fn dispatch_shared_agents_install_matches_direct_dispatch_shape() {
    use acpx_core::router::dispatch_shared;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let _guard = install_test_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _home = TempHome::new();
    let router = Arc::new(Mutex::new(Router::new("codex-acp")));
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "agents/install",
        "params": {"id": "codex-acp"}
    });
    let response = dispatch_shared(&router, request)
        .await
        .expect("dispatch_shared agents/install");
    assert_eq!(response["result"]["id"], json!("codex-acp"));
    assert!(response["result"]["outcome"].is_string());
    assert!(
        response["result"]["outcome"]
            .as_str()
            .unwrap_or("")
            .contains("PackageReady")
    );
}

/// Same shared path, unknown agent id -- must still error exactly like
/// `agents_install_with_unknown_agent_id_errors` (the direct-dispatch
/// counterpart in `gateway_native_coverage_test.rs`) does.
#[tokio::test]
async fn dispatch_shared_agents_install_unknown_agent_id_errors() {
    use acpx_core::router::dispatch_shared;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let router = Arc::new(Mutex::new(Router::new("codex-acp")));
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "agents/install",
        "params": {"id": "not-a-real-agent"}
    });
    assert!(dispatch_shared(&router, request).await.is_err());
}

/// Same shared path, missing `id` param -- mirrors
/// `agents_install_with_missing_id_param_errors`.
#[tokio::test]
async fn dispatch_shared_agents_install_missing_id_param_errors() {
    use acpx_core::router::dispatch_shared;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let router = Arc::new(Mutex::new(Router::new("codex-acp")));
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "agents/install",
        "params": {}
    });
    assert!(dispatch_shared(&router, request).await.is_err());
}
