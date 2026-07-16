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
