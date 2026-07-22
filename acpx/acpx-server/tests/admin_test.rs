//! Integration coverage for the loopback-only admin transport and its
//! token separation from the ordinary ACPX client transport.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use acpx_conductor::SpawnSpec;
use acpx_core::{
    router::{dispatch_shared_for_tenant, Router},
    PersistenceStore, TenantId,
};
use acpx_registry::{Agent, Distribution, Registry};
use serde_json::json;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

#[path = "../src/transport/admin.rs"]
mod admin;
#[path = "../src/transport/http.rs"]
mod http;
#[path = "../src/transport/live.rs"]
mod live;
#[path = "../src/transport/ws.rs"]
mod ws;

use http::SharedRouter;

fn test_registry() -> Registry {
    Registry {
        version: "test".to_owned(),
        agents: vec![Agent {
            id: "registry-agent".to_owned(),
            name: "Registry Agent".to_owned(),
            version: "1.0.0".to_owned(),
            description: None,
            repository: None,
            website: None,
            authors: Vec::new(),
            license: None,
            icon: None,
            distribution: Distribution {
                npx: None,
                uvx: None,
                binary: Some(HashMap::new()),
            },
        }],
        extensions: Vec::new(),
    }
}

fn client_router() -> SharedRouter {
    let mut router = Router::new("stand-in-agent");
    router.register_agent(
        "stand-in-agent",
        SpawnSpec::new("sh", vec!["-c".to_owned(), "cat".to_owned()]),
    );
    Arc::new(Mutex::new(router))
}

async fn spawn_client_transport(auth_token: Option<String>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind client listener");
    let address = listener.local_addr().expect("client address");
    tokio::spawn(async move {
        http::serve_on(listener, client_router(), auth_token)
            .await
            .expect("serve client transport");
    });
    address
}

async fn spawn_admin_transport(token: &str) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind admin listener");
    let address = listener.local_addr().expect("admin address");
    let store = PersistenceStore::open_in_memory().expect("in-memory admin database");
    let token = token.to_owned();
    tokio::spawn(async move {
        admin::serve_on(listener, token, store, test_registry())
            .await
            .expect("serve admin transport");
    });
    address
}

async fn wait_for(address: SocketAddr) {
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(address).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("listener {address} did not become ready");
}

fn unique_temp_path(prefix: &str) -> std::path::PathBuf {
    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    );
    std::env::temp_dir().join(format!("{prefix}-{unique}"))
}

struct BinaryGuard {
    child: Child,
    database: std::path::PathBuf,
}

impl Drop for BinaryGuard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_file(&self.database);
    }
}

#[tokio::test]
async fn admin_surface_is_absent_when_not_mounted() {
    let address = spawn_client_transport(None).await;
    wait_for(address).await;

    let response = reqwest::Client::new()
        .get(format!("http://{address}/admin/agents"))
        .send()
        .await
        .expect("GET unmounted admin route");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn admin_routes_require_the_separate_admin_token() {
    let address = spawn_admin_transport("admin-secret").await;
    wait_for(address).await;
    let client = reqwest::Client::new();

    for token in [None, Some("wrong-token"), Some("client-secret")] {
        let mut request = client.get(format!("http://{address}/admin/agents"));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.expect("GET admin agents");
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    }

    let response = client
        .get(format!("http://{address}/admin/agents"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .expect("authorized GET admin agents");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("admin list JSON");
    assert_eq!(body["agents"][0]["id"], json!("registry-agent"));
    assert_eq!(body["agents"][0]["source"], json!("registry"));
    assert_eq!(body["agents"][0]["enabled"], json!(true));
}

#[tokio::test]
async fn client_and_admin_tokens_are_not_interchangeable() {
    let client_address = spawn_client_transport(Some("client-secret".to_owned())).await;
    let admin_address = spawn_admin_transport("admin-secret").await;
    wait_for(client_address).await;
    wait_for(admin_address).await;
    let client = reqwest::Client::new();

    let admin_rejected_by_client = client
        .post(format!("http://{client_address}/rpc"))
        .bearer_auth("admin-secret")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "agents/list", "params": {}}))
        .send()
        .await
        .expect("POST client route with admin token");
    assert_eq!(
        admin_rejected_by_client.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );

    let client_rejected_by_admin = client
        .get(format!("http://{admin_address}/admin/agents"))
        .bearer_auth("client-secret")
        .send()
        .await
        .expect("GET admin route with client token");
    assert_eq!(
        client_rejected_by_admin.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn custom_agent_crud_round_trips_over_admin_http() {
    let address = spawn_admin_transport("admin-secret").await;
    wait_for(address).await;
    let client = reqwest::Client::new();
    let agent = json!({
        "id": "custom-agent",
        "name": "Custom Agent",
        "command": "custom-acp",
        "args": ["--stdio"],
        "env": {"CUSTOM_MODE": "test"},
        "cwd": "/tmp"
    });

    let created = client
        .post(format!("http://{address}/admin/agents/custom"))
        .bearer_auth("admin-secret")
        .json(&agent)
        .send()
        .await
        .expect("create custom agent");
    assert_eq!(created.status(), reqwest::StatusCode::CREATED);

    let disabled = client
        .post(format!(
            "http://{address}/admin/agents/custom-agent/disable"
        ))
        .bearer_auth("admin-secret")
        .send()
        .await
        .expect("disable custom agent");
    let disabled_status = disabled.status();
    let disabled_body = disabled.text().await.expect("disable response body");
    assert_eq!(
        disabled_status,
        reqwest::StatusCode::OK,
        "disable response: {disabled_body}"
    );

    let listed = client
        .get(format!("http://{address}/admin/agents/custom"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .expect("list custom agents");
    assert_eq!(listed.status(), reqwest::StatusCode::OK);
    let listed: serde_json::Value = listed.json().await.expect("custom list JSON");
    assert_eq!(listed[0]["id"], json!("custom-agent"));

    let deleted = client
        .delete(format!("http://{address}/admin/agents/custom/custom-agent"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .expect("delete custom agent");
    assert_eq!(deleted.status(), reqwest::StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn deleting_a_custom_agent_stops_its_live_backend() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind admin listener");
    let address = listener.local_addr().expect("admin address");
    let store = PersistenceStore::open_in_memory().expect("in-memory admin database");
    let router: SharedRouter = Arc::new(Mutex::new(
        Router::new("stand-in-agent").with_persistence(store.clone()),
    ));
    let admin_router = Arc::clone(&router);
    tokio::spawn(async move {
        admin::serve_on_with_router(
            listener,
            "admin-secret".to_owned(),
            store,
            test_registry(),
            Some(admin_router),
        )
        .await
        .expect("serve admin transport");
    });
    wait_for(address).await;

    let client = reqwest::Client::new();
    let created = client
        .post(format!("http://{address}/admin/agents/custom"))
        .bearer_auth("admin-secret")
        .json(&json!({
            "id": "live-custom",
            "name": "Live Custom",
            "command": "sh",
            "args": [
                "-c",
                "while IFS= read -r line; do id=$(echo \"$line\" | sed -n 's/.*\"id\":\\([0-9]*\\).*/\\1/p'); printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"sessionId\":\"live\"}}\\n' \"$id\"; done"
            ],
            "env": {},
            "cwd": null
        }))
        .send()
        .await
        .expect("create custom agent");
    assert_eq!(created.status(), reqwest::StatusCode::CREATED);

    dispatch_shared_for_tenant(
        &router,
        &TenantId::default_tenant(),
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/new",
            "params": {
                "cwd": "/tmp",
                "mcpServers": [],
                "_acpx": {"agentId": "live-custom"}
            }
        }),
    )
    .await
    .expect("start custom backend");
    assert!(router
        .lock()
        .await
        .process_id("live-custom")
        .await
        .is_some());

    let deleted = client
        .delete(format!("http://{address}/admin/agents/custom/live-custom"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .expect("delete custom agent");
    assert_eq!(deleted.status(), reqwest::StatusCode::NO_CONTENT);
    assert!(router
        .lock()
        .await
        .process_id("live-custom")
        .await
        .is_none());
}

#[tokio::test]
async fn real_binary_starts_the_loopback_admin_listener() {
    let client_address = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind client probe");
        let address = listener.local_addr().expect("client probe address");
        drop(listener);
        address
    };
    let admin_address = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind admin probe");
        let address = listener.local_addr().expect("admin probe address");
        drop(listener);
        address
    };
    let database = unique_temp_path("acpx-admin-test.sqlite");
    let mut command = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    command
        .env("ACPX_BACKEND_CMD", "sh -c cat")
        .env("ACPX_HTTP_BIND", client_address.to_string())
        .env("ACPX_AUTH_TOKEN", "client-secret")
        .env("ACPX_ADMIN_TOKEN", "admin-secret")
        .env("ACPX_ADMIN_BIND", admin_address.to_string())
        .env("ACPX_DB_PATH", database.display().to_string())
        .env("ACPX_STARTUP_SESSION_RECOVERY_ENABLED", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let child = command.spawn().expect("spawn real acpx-server");
    let _server = BinaryGuard { child, database };

    for _ in 0..160 {
        if tokio::net::TcpStream::connect(admin_address).await.is_ok()
            && tokio::net::TcpStream::connect(client_address).await.is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let client = reqwest::Client::new();
    let admin_response = client
        .get(format!("http://{admin_address}/admin/agents"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .expect("GET real binary admin route");
    assert_eq!(admin_response.status(), reqwest::StatusCode::OK);

    let admin_token_on_client_route = client
        .post(format!("http://{client_address}/rpc"))
        .bearer_auth("admin-secret")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "agents/list", "params": {}}))
        .send()
        .await
        .expect("POST real binary client route");
    assert_eq!(
        admin_token_on_client_route.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
}

/// setup-followups plan, agent_settings_ordering_and_install_enable_
/// flow: this test's own real-binary + both-transports setup already
/// proved the two transports/tokens are correctly separated; this proves
/// the actual feature they exist for -- disabling an agent through the
/// real admin HTTP boundary really blocks a subsequent real
/// `session/new` on the real client transport, all against one genuinely
/// spawned `acpx-server` process (not the in-process `Router`/
/// `dispatch_shared_for_tenant` calls `client_and_admin_tokens_are_not_
/// interchangeable`'s sibling tests use). This is the exact combination
/// `acpxmgr.go`'s and `panel-rust::agent_bridge::spawn_gateway_process`'s
/// new `ACPX_ADMIN_TOKEN` generation/wiring exists to make possible in
/// production -- proving it here closes the loop on that work.
#[tokio::test]
async fn disabling_an_agent_over_the_real_admin_http_blocks_a_real_session_new() {
    let client_address = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind client probe");
        let address = listener.local_addr().expect("client probe address");
        drop(listener);
        address
    };
    let admin_address = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind admin probe");
        let address = listener.local_addr().expect("admin probe address");
        drop(listener);
        address
    };
    let database = unique_temp_path("acpx-admin-enable-test.sqlite");
    // A minimal stand-in ACP backend over stdio -- same "read the id out
    // of the request line, reply with a bare sessionId" shape
    // deleting_a_custom_agent_stops_its_live_backend's custom-agent
    // command above uses, registered here as the *default* agent instead
    // so a plain, unmanaged session/new (no _acpx.profile/agentId)
    // reaches it. Written to a script file (not an inline ACPX_BACKEND_CMD
    // string) since that env var is parsed as plain space-separated
    // program+args, not passed through a shell -- see
    // ServerConfig::from_env's own doc comment.
    let script_path = unique_temp_path("acpx-admin-enable-backend.sh");
    std::fs::write(
        &script_path,
        "#!/bin/sh\nwhile IFS= read -r line; do\n  id=$(echo \"$line\" | grep -o '\"id\":[0-9]*' | head -1 | cut -d: -f2)\n  printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"sessionId\":\"live\"}}\\n' \"$id\"\ndone\n",
    )
    .expect("write stand-in backend script");
    let mut command = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    command
        .env("ACPX_BACKEND_CMD", format!("sh {}", script_path.display()))
        .env("ACPX_DEFAULT_AGENT_ID", "codex-acp")
        .env("ACPX_HTTP_BIND", client_address.to_string())
        .env("ACPX_ADMIN_TOKEN", "admin-secret")
        .env("ACPX_ADMIN_BIND", admin_address.to_string())
        .env("ACPX_DB_PATH", database.display().to_string())
        .env("ACPX_STARTUP_SESSION_RECOVERY_ENABLED", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let child = command.spawn().expect("spawn real acpx-server");
    let _server = BinaryGuard { child, database };

    for _ in 0..160 {
        if tokio::net::TcpStream::connect(admin_address).await.is_ok()
            && tokio::net::TcpStream::connect(client_address).await.is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let client = reqwest::Client::new();
    let session_new = json!({
        "jsonrpc": "2.0", "id": 1, "method": "session/new",
        "params": {"cwd": "/tmp", "mcpServers": []}
    });

    // Enabled by default (AgentEnablement::is_enabled's documented
    // default) -- a real session/new against the real stand-in backend
    // must succeed before any disable call.
    let before = client
        .post(format!("http://{client_address}/rpc"))
        .json(&session_new)
        .send()
        .await
        .expect("POST session/new before disable")
        .json::<serde_json::Value>()
        .await
        .expect("parse session/new response");
    assert!(
        before.get("result").is_some(),
        "expected session/new to succeed while the agent is enabled, got: {before:?}"
    );

    // The real admin HTTP call this session's acpxmgr.go/spawn_gateway_
    // process wiring exists to make reachable in production.
    let disable = client
        .post(format!(
            "http://{admin_address}/admin/agents/codex-acp/disable"
        ))
        .bearer_auth("admin-secret")
        .send()
        .await
        .expect("POST disable over the real admin transport");
    assert_eq!(disable.status(), reqwest::StatusCode::OK);

    let after = client
        .post(format!("http://{client_address}/rpc"))
        .json(&session_new)
        .send()
        .await
        .expect("POST session/new after disable")
        .json::<serde_json::Value>()
        .await
        .expect("parse session/new response");
    assert!(
        after.get("error").is_some(),
        "expected session/new to fail once the agent is disabled via the real admin HTTP \
         boundary, got: {after:?}"
    );
    assert!(
        after["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase()
            .contains("disabled"),
        "expected a real AgentDisabled error message, got: {after:?}"
    );
}

/// **`e2e_session_teardown_automation` / `e2e_teardown_headless_test`.**
/// Headless (no VNC, no manual step, no live desktop) proof that the new
/// `/admin/sessions/close-all` + `/admin/sessions/count` endpoints do what
/// e2e/dev-test teardown needs: open several real sessions against a real
/// `acpx-server` process, confirm the count reflects them, close them all
/// through the real admin HTTP boundary, then confirm the count is
/// genuinely back to zero -- not just that each individual `session/
/// close` call returned success. Same real-binary + stand-in-backend
/// pattern as `disabling_an_agent_over_the_real_admin_http_blocks_a_real_
/// session_new` above.
#[tokio::test]
async fn closing_all_sessions_over_the_real_admin_http_leaves_the_tenant_count_at_zero() {
    let client_address = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind client probe");
        let address = listener.local_addr().expect("client probe address");
        drop(listener);
        address
    };
    let admin_address = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind admin probe");
        let address = listener.local_addr().expect("admin probe address");
        drop(listener);
        address
    };
    let database = unique_temp_path("acpx-admin-teardown-test.sqlite");
    let script_path = unique_temp_path("acpx-admin-teardown-backend.sh");
    std::fs::write(
        &script_path,
        "#!/bin/sh\nwhile IFS= read -r line; do\n  id=$(echo \"$line\" | grep -o '\"id\":[0-9]*' | head -1 | cut -d: -f2)\n  printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"sessionId\":\"live\"}}\\n' \"$id\"\ndone\n",
    )
    .expect("write stand-in backend script");
    let mut command = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    command
        .env("ACPX_BACKEND_CMD", format!("sh {}", script_path.display()))
        .env("ACPX_DEFAULT_AGENT_ID", "codex-acp")
        .env("ACPX_HTTP_BIND", client_address.to_string())
        .env("ACPX_ADMIN_TOKEN", "admin-secret")
        .env("ACPX_ADMIN_BIND", admin_address.to_string())
        .env("ACPX_DB_PATH", database.display().to_string())
        .env("ACPX_STARTUP_SESSION_RECOVERY_ENABLED", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let child = command.spawn().expect("spawn real acpx-server");
    let _server = BinaryGuard { child, database };

    for _ in 0..160 {
        if tokio::net::TcpStream::connect(admin_address).await.is_ok()
            && tokio::net::TcpStream::connect(client_address).await.is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let client = reqwest::Client::new();
    const SESSION_COUNT: usize = 3;
    for id in 1..=SESSION_COUNT {
        let response = client
            .post(format!("http://{client_address}/rpc"))
            .json(&json!({
                "jsonrpc": "2.0", "id": id, "method": "session/new",
                "params": {"cwd": "/tmp", "mcpServers": []}
            }))
            .send()
            .await
            .expect("POST session/new")
            .json::<serde_json::Value>()
            .await
            .expect("parse session/new response");
        assert!(
            response.get("result").is_some(),
            "expected session/new #{id} to succeed against the real stand-in backend, \
             got: {response:?}"
        );
    }

    let count_before = client
        .get(format!(
            "http://{admin_address}/admin/sessions/count?tenant=default"
        ))
        .bearer_auth("admin-secret")
        .send()
        .await
        .expect("GET session count before teardown")
        .json::<serde_json::Value>()
        .await
        .expect("parse session count response");
    assert_eq!(
        count_before["count"],
        json!(SESSION_COUNT),
        "expected the real admin session count to reflect every session/new opened above, \
         got: {count_before:?}"
    );

    let close_all = client
        .post(format!(
            "http://{admin_address}/admin/sessions/close-all?tenant=default"
        ))
        .bearer_auth("admin-secret")
        .send()
        .await
        .expect("POST close-all over the real admin transport");
    assert_eq!(close_all.status(), reqwest::StatusCode::OK);
    let close_all: serde_json::Value = close_all.json().await.expect("close-all response JSON");
    assert_eq!(
        close_all["closed"],
        json!(SESSION_COUNT),
        "expected close-all to report every session closed, got: {close_all:?}"
    );
    assert_eq!(close_all["failed"], json!(0), "got: {close_all:?}");

    let count_after = client
        .get(format!(
            "http://{admin_address}/admin/sessions/count?tenant=default"
        ))
        .bearer_auth("admin-secret")
        .send()
        .await
        .expect("GET session count after teardown")
        .json::<serde_json::Value>()
        .await
        .expect("parse session count response");
    assert_eq!(
        count_after["count"],
        json!(0),
        "expected the tenant's live session count to be genuinely zero after teardown, \
         not just each session/close call individually reporting success, got: {count_after:?}"
    );
}
