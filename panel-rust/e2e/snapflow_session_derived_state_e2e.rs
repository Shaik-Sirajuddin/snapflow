//! Real cross-process coverage for snapflow session-derived state.
//!
//! This follows the existing gateway E2E pattern: a real `acpx-server` is
//! backed by the compiled `rui-mock-agent`, a real `snapshotd serve` owns the
//! MCP TCP listener, and the test uses the production Rust session client plus
//! a real MCP Streamable HTTP call. No daemon or ACPX handler is stubbed.

use panel_rust::gateway_actor::spawn_acpx_thread;
use panel_rust::snapflow_session_client::{
    SessionRef, SessionSnapshot, SessionUpdate, SnapflowSessionClient,
};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, CONTENT_TYPE};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio::time::sleep;

#[path = "../tests/common/mod.rs"]
mod common;
use common::{free_port, provision_mock_profile, spawn_acpx_server_with_retry};

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn snapshotd_module_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../snapshotd")
}

fn build_go_binary(out_dir: &Path, package: &str, name: &str) -> PathBuf {
    let output = out_dir.join(name);
    let result = Command::new("go")
        .current_dir(snapshotd_module_dir())
        .args(["build", "-o"])
        .arg(&output)
        .arg(package)
        .output()
        .expect("spawn go build");
    assert!(
        result.status.success(),
        "go build {package} failed:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
    output
}

async fn wait_for_tcp(port: u16, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for TCP port {port}");
}

async fn wait_for_file(path: &Path, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(contents) = tokio::fs::read_to_string(path).await {
            if !contents.trim().is_empty() {
                return contents;
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {:?}", path);
}

struct StreamableMcpClient {
    http: reqwest::Client,
    url: String,
    session_id: Option<String>,
    next_id: u64,
    context_token: String,
}

async fn wait_for_disconnected(client: &mut SnapflowSessionClient) -> SessionUpdate {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for disconnected update"
        );
        let update = client
            .next_update(remaining)
            .await
            .expect("receive session update while waiting for close");
        if update.snapshot.connection_status == "disconnected" {
            return update;
        }
    }
}

impl StreamableMcpClient {
    fn new(url: String, context_token: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            url,
            session_id: None,
            next_id: 1,
            context_token,
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        headers.insert(
            "X-Snapshotd-Context-Token",
            HeaderValue::from_str(&self.context_token).expect("context token header"),
        );
        if let Some(session_id) = &self.session_id {
            headers.insert(
                "Mcp-Session-Id",
                HeaderValue::from_str(session_id).expect("MCP session header"),
            );
        }
        let response = self
            .http
            .post(&self.url)
            .headers(headers)
            .timeout(Duration::from_secs(30))
            .json(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }))
            .send()
            .await
            .unwrap_or_else(|error| panic!("MCP {method} request failed: {error}"));
        let returned_session = response
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if returned_session.is_some() {
            self.session_id = returned_session;
        }
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|error| panic!("read MCP {method} response failed: {error}"));
        assert!(status.is_success(), "MCP {method} status {status}: {body}");
        let envelope: Value = serde_json::from_str(&body)
            .unwrap_or_else(|error| panic!("MCP {method} returned invalid JSON: {error}: {body}"));
        assert!(
            envelope.get("error").is_none(),
            "MCP {method} returned JSON-RPC error: {envelope}"
        );
        envelope.get("result").cloned().unwrap_or(envelope)
    }

    async fn initialize(&mut self) {
        self.call(
            "initialize",
            json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "snapflow-session-e2e", "version": "1"}
            }),
        )
        .await;
    }

    async fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let result = self
            .call("tools/call", json!({"name": name, "arguments": arguments}))
            .await;
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("MCP tool {name} returned no JSON text: {result}"));
        serde_json::from_str(text).unwrap_or_else(|error| {
            panic!("MCP tool {name} returned invalid JSON: {error}: {text}")
        })
    }
}

#[tokio::test]
async fn real_mock_backend_session_registration_and_project_snapshot_round_trip() {
    let build_dir = tempfile::tempdir().expect("build dir");
    let snapshotd_bin = build_go_binary(build_dir.path(), "./cmd/snapshotd", "snapshotd-bin");
    let sap_fixture_bin = build_go_binary(
        build_dir.path(),
        "./internal/procmgr/testdata/sap_fixture",
        "sap-fixture-bin",
    );

    let home_dir = tempfile::tempdir().expect("snapshotd home");
    let mcp_port = free_port();
    let _snapshotd = ChildGuard::new(
        Command::new(&snapshotd_bin)
            .arg("serve")
            .env("SNAPSHOTD_HOME", home_dir.path())
            .env("SNAPSHOT_BIN_PATH", &sap_fixture_bin)
            .env("SNAPSHOTD_MCP_SSE_ADDR", format!("127.0.0.1:{mcp_port}"))
            .env("SNAPSHOTD_ACPX_ENABLED", "false")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn real snapshotd serve"),
    );
    wait_for_tcp(mcp_port, Duration::from_secs(10)).await;
    let config_path = home_dir.path().join("mcp_config.json");
    let config: Value =
        serde_json::from_str(&wait_for_file(&config_path, Duration::from_secs(10)).await)
            .expect("parse snapshotd MCP config");
    let service_token = config["sessionServiceToken"]
        .as_str()
        .expect("snapshotd session service token")
        .to_owned();

    let acpx_db = home_dir.path().join("acpx.sqlite3");
    let admin_port = free_port();
    let admin_token = format!("snapflow-session-e2e-admin-{admin_port}");
    let admin_token_for_env = admin_token.clone();
    let (acpx_child, gateway_url) = spawn_acpx_server_with_retry(move |command, port| {
        command
            .env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"))
            .env("ACPX_DEFAULT_AGENT_ID", "codex")
            .env("ACPX_DB_PATH", &acpx_db)
            .env("ACPX_ADMIN_TOKEN", &admin_token_for_env)
            .env("ACPX_ADMIN_BIND", format!("127.0.0.1:{admin_port}"))
            .env("RUST_LOG", "error");
    });
    let _acpx = ChildGuard::new(acpx_child);
    let profile = provision_mock_profile(
        &gateway_url,
        admin_port,
        &admin_token,
        "codex",
        BTreeMap::new(),
    )
    .await;

    let context_token = format!("snapflow-session-context-{mcp_port}");
    let mcp_url = format!("http://127.0.0.1:{mcp_port}/mcp");
    let mcp_entry = json!({
        "type": "http",
        "name": "snapshotd",
        "url": mcp_url,
        "headers": [{"name": "X-Snapshotd-Context-Token", "value": context_token.clone()}],
    });
    let gateway = spawn_acpx_thread(gateway_url);
    let acp_session_id = gateway
        .open_session_with_profile(home_dir.path().to_path_buf(), &profile, vec![mcp_entry])
        .await
        .expect("real ACPX session/new through mock backend");
    assert!(!acp_session_id.is_empty());

    let ws_url = format!("ws://127.0.0.1:{mcp_port}/session/ws");
    let mut status_client = SnapflowSessionClient::connect(&ws_url, &service_token)
        .await
        .expect("connect production session client to real daemon");
    status_client
        .register_context(&context_token, &acp_session_id)
        .await
        .expect("register ACPX context through real session API");

    let mut mcp = StreamableMcpClient::new(mcp_url, context_token);
    mcp.initialize().await;
    let project = mcp
        .call_tool(
            "daemon.createProject",
            json!({"name": "session-e2e-project"}),
        )
        .await;
    let project_id = project["id"]
        .as_str()
        .or_else(|| project["ID"].as_str())
        .expect("daemon.createProject returned project id")
        .to_owned();
    let opened = mcp
        .call_tool("project.enter", json!({"projectId": project_id}))
        .await;
    assert_eq!(
        opened["projectId"]
            .as_str()
            .or_else(|| opened["id"].as_str()),
        Some(project_id.as_str())
    );

    let snapshots = status_client
        .subscribe(vec![SessionRef {
            snapflow_session_id: None,
            acp_session_id: Some(acp_session_id.clone()),
        }])
        .await
        .expect("subscribe to real daemon session snapshot");
    let snapshot: &SessionSnapshot = snapshots
        .first()
        .expect("real daemon returned initial session snapshot");
    assert_eq!(
        snapshot.acp_session_id.as_deref(),
        Some(acp_session_id.as_str())
    );
    assert_eq!(snapshot.connection_status, "connected");
    assert!(snapshot
        .project_path
        .as_deref()
        .is_some_and(|path| !path.is_empty()));
    assert!(
        snapshot.revision >= 3,
        "expected context + open transitions: {snapshot:?}"
    );

    // The point-to-point HTTP API must resolve the same transport session by
    // its ACPX identity and require the service-account bearer secret.
    let details = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{mcp_port}/session/details"))
        .bearer_auth(&service_token)
        .query(&[("sessionId", snapshot.session_id.as_str())])
        .send()
        .await
        .expect("request real session details")
        .error_for_status()
        .expect("session details status")
        .json::<Value>()
        .await
        .expect("decode real session details");
    assert_eq!(
        details["acpSessionId"].as_str(),
        Some(acp_session_id.as_str())
    );
    assert_eq!(
        details["projectPath"].as_str(),
        snapshot.project_path.as_deref()
    );
    let unauthorized = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{mcp_port}/session/details"))
        .query(&[("sessionId", snapshot.session_id.as_str())])
        .send()
        .await
        .expect("request unauthenticated session details");
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

    // A second same-token client must independently resolve the same session.
    let mut second_client = SnapflowSessionClient::connect(&ws_url, &service_token)
        .await
        .expect("connect second real session client");
    let second_snapshots = second_client
        .subscribe(vec![SessionRef {
            snapflow_session_id: None,
            acp_session_id: Some(acp_session_id.clone()),
        }])
        .await
        .expect("second client subscribe");
    assert_eq!(second_snapshots.len(), 1);
    assert_ne!(
        status_client.client_instance_id, second_client.client_instance_id,
        "same service account must still produce independent client instances"
    );

    // A project transition must be delivered independently to both clients,
    // not only to the client that initiated the subscription.
    mcp.call_tool("project.exit", json!({})).await;
    let first_close = wait_for_disconnected(&mut status_client).await;
    let second_close = wait_for_disconnected(&mut second_client).await;
    for update in [first_close, second_close] {
        assert_eq!(
            update.snapshot.acp_session_id.as_deref(),
            Some(acp_session_id.as_str())
        );
        assert_eq!(update.snapshot.connection_status, "disconnected");
        assert!(update.snapshot.project_path.is_none());
    }

    let _ = status_client.disconnect().await;
    let _ = second_client.disconnect().await;
}
