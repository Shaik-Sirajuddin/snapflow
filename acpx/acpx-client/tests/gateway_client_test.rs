//! End-to-end coverage for `acpx-client` (Phase 5) against a *real*
//! running gateway -- same `#[path]`-into-acpx-server's-transport-source
//! trick `acpx-server/tests/http_ws_transport_test.rs` uses (that crate
//! is bin-only, no `[lib]` target to depend on directly), so this
//! exercises the actual production `transport::http`/`ws` code, not a
//! hand-rolled fake server. Stand-in backend scripts follow the same
//! pattern as `acpx-core/tests/router_dispatch_test.rs`.

#[path = "../../acpx-server/src/transport/admin.rs"]
mod admin;
#[path = "../../acpx-server/src/transport/http.rs"]
mod http;
#[path = "../../acpx-server/src/transport/live.rs"]
mod live;
#[path = "../../acpx-server/src/transport/ws.rs"]
mod ws;

use acpx_client::ext::{admin::AdminClient, profiles, registry, sessions};
use acpx_client::raw::GatewayClient;
use acpx_conductor::SpawnSpec;
use acpx_core::{router::Router, PersistenceStore};
use acpx_proto::admin::CustomAgentSpec;
use acpx_registry::{Agent, Distribution, Registry};
use http::SharedRouter;
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;

const STAND_IN_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;

fn stand_in_backend_spec() -> SpawnSpec {
    SpawnSpec::new(
        "sh",
        vec!["-c".to_string(), STAND_IN_BACKEND_SCRIPT.to_string()],
    )
}

/// Same ephemeral-port bind-then-serve trick as
/// `http_ws_transport_test.rs`'s `spawn_server` -- see that file for why.
async fn spawn_server(router: SharedRouter) -> SocketAddr {
    spawn_server_with_auth(router, None).await
}

/// Same bring-up, but lets a test opt into `ACPX_AUTH_TOKEN`-style
/// bearer-token auth on the gateway it spins up.
async fn spawn_server_with_auth(router: SharedRouter, auth_token: Option<String>) -> SocketAddr {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = probe.local_addr().expect("local_addr");
    drop(probe);
    tokio::spawn(async move {
        http::serve(router, addr, auth_token)
            .await
            .expect("transport::serve");
    });
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    addr
}

fn admin_registry() -> Registry {
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

async fn spawn_admin_server(token: &str) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind admin listener");
    let address = listener.local_addr().expect("admin address");
    let store = PersistenceStore::open_in_memory().expect("in-memory admin database");
    let token = token.to_owned();
    tokio::spawn(async move {
        admin::serve_on(listener, token, store, admin_registry())
            .await
            .expect("serve admin transport");
    });
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(address).await.is_ok() {
            return address;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("admin listener {address} did not become ready");
}

#[tokio::test]
async fn raw_call_round_trips_a_gateway_native_method() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;

    let client = GatewayClient::new(format!("http://{addr}"));
    let result = client
        .call("session/list", serde_json::json!({}), None)
        .await
        .expect("session/list");
    assert_eq!(result["sessions"], serde_json::json!([]));
}

#[tokio::test]
async fn raw_call_surfaces_json_rpc_errors_as_client_errors() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;

    let client = GatewayClient::new(format!("http://{addr}"));
    let err = client
        .call("bogus/method", serde_json::json!({}), None)
        .await
        .expect_err("unknown method should error");
    assert!(matches!(err, acpx_client::raw::ClientError::Rpc { .. }));
}

#[tokio::test]
async fn ext_sessions_list_aggregates_across_the_gateway() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;
    let client = GatewayClient::new(format!("http://{addr}"));

    client
        .call("session/new", serde_json::json!({"cwd": "/tmp"}), None)
        .await
        .expect("session/new");

    let sessions = sessions::list(&client).await.expect("session/list");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].agent_id, "stand-in-agent");
}

#[tokio::test]
async fn ext_profiles_create_list_delete_round_trip() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;
    let client = GatewayClient::new(format!("http://{addr}"));

    let created = profiles::create(
        &client,
        serde_json::json!({"name": "work", "agent_id": "stand-in-agent"}),
    )
    .await
    .expect("profiles/create");
    assert_eq!(created["name"], "work");

    let listed = profiles::list(&client).await.expect("profiles/list");
    // `profiles/list` also includes one auto-seeded profile per
    // `Installed` ACP-registry agent (`ensure_default_profiles_seeded`,
    // see `acpx-core/tests/default_profile_seeding_test.rs`) alongside
    // this explicitly created one, so this asserts presence rather than
    // the list's exact length.
    assert!(listed.iter().any(|p| p["name"] == "work"));

    profiles::delete(&client, "work")
        .await
        .expect("profiles/delete");
    let listed_after = profiles::list(&client).await.expect("profiles/list");
    assert!(!listed_after.iter().any(|p| p["name"] == "work"));
}

#[tokio::test]
async fn ext_profiles_create_via_client_then_session_new_via_header_uses_it() {
    // Exercises the client -> gateway -> profile-resolution -> spawned
    // process path end to end: `ext::profiles::create` registers a
    // profile pointing at the already-registered stand-in agent, then a
    // raw `session/new` call with `_acpx.profile` set picks it up via
    // `router.rs`'s `resolve_profile`.
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;
    let client = GatewayClient::new(format!("http://{addr}"));

    profiles::create(
        &client,
        serde_json::json!({"name": "use-stand-in", "agent_id": "stand-in-agent"}),
    )
    .await
    .expect("profiles/create");

    let result = client
        .call(
            "session/new",
            serde_json::json!({"cwd": "/tmp"}),
            Some("use-stand-in"),
        )
        .await
        .expect("session/new via profile");
    assert!(result["sessionId"].as_str().is_some());
}

#[tokio::test]
async fn ext_registry_agents_list_and_status_and_install_round_trip() {
    // Uses the real bundled `registry.fallback.json` (no live network
    // dependency, see `acpx-registry`'s own doc comments) -- Claude,
    // Codex, and Gemini are all npx-distributed there, and this test
    // environment has a real node/npm on PATH, so `agents/install`
    // actually exercises the "confirm runtime present" path for real
    // rather than mocking it.
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;
    let client = GatewayClient::new(format!("http://{addr}"));

    let agents = registry::agents_list(&client).await.expect("agents/list");
    assert!(!agents.is_empty());
    let codex_id = agents
        .iter()
        .find_map(|a| {
            let id = a.get("id")?.as_str()?;
            id.contains("codex").then(|| id.to_string())
        })
        .expect("fallback registry has a codex entry");

    let status = registry::agents_status(&client, &codex_id)
        .await
        .expect("agents/status");
    assert_eq!(status["id"], codex_id);

    let install = registry::install(&client, &codex_id)
        .await
        .expect("agents/install");
    assert_eq!(install["id"], codex_id);
    assert!(install["outcome"]
        .as_str()
        .unwrap()
        .contains("RuntimeConfirmed"));
}

/// **acpx client + acpx daemon auth, end to end**: proves
/// `GatewayClient::with_auth_token` genuinely round-trips a real call
/// against a gateway started with `ACPX_AUTH_TOKEN`-style auth enabled
/// (`http::serve(.., Some(token))`), and that a client with no/wrong
/// token is rejected -- closing the gap where server-side auth landed
/// without any client-side way to actually use it.
#[tokio::test]
async fn client_with_auth_token_round_trips_against_an_authenticated_gateway() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server_with_auth(router, Some("s3cret-token".to_string())).await;

    let authed_client =
        GatewayClient::new(format!("http://{addr}")).with_auth_token("s3cret-token");
    let result = authed_client
        .call("session/list", serde_json::json!({}), None)
        .await
        .expect("authenticated call should succeed");
    assert_eq!(result["sessions"], serde_json::json!([]));

    let unauthed_client = GatewayClient::new(format!("http://{addr}"));
    let err = unauthed_client
        .call("session/list", serde_json::json!({}), None)
        .await
        .expect_err("call with no token against an authenticated gateway must fail");
    assert!(matches!(
        err,
        acpx_client::raw::ClientError::Rpc { code: -32001, .. }
    ));

    let wrong_client = GatewayClient::new(format!("http://{addr}")).with_auth_token("wrong");
    let err = wrong_client
        .call("session/list", serde_json::json!({}), None)
        .await
        .expect_err("call with wrong token must fail");
    assert!(matches!(
        err,
        acpx_client::raw::ClientError::Rpc { code: -32001, .. }
    ));
}

#[tokio::test]
async fn admin_client_uses_its_own_http_plane_for_enablement_and_custom_crud() {
    let address = spawn_admin_server("admin-secret").await;
    let client = AdminClient::new(format!("http://{address}"), "admin-secret");
    let custom = CustomAgentSpec {
        id: "custom-client-agent".to_owned(),
        name: "Custom Client Agent".to_owned(),
        command: "custom-acp".to_owned(),
        args: vec!["--stdio".to_owned()],
        env: BTreeMap::from([("CUSTOM_MODE".to_owned(), "test".to_owned())]),
        cwd: Some("/tmp".to_owned()),
    };

    let disabled = client
        .disable_agent("registry-agent")
        .await
        .expect("disable registry agent");
    assert_eq!(disabled.id, "registry-agent");
    assert!(!disabled.enabled);

    let created = client
        .create_custom_agent(&custom)
        .await
        .expect("create custom agent");
    assert_eq!(created, custom);

    let listed = client
        .list_custom_agents()
        .await
        .expect("list custom agents");
    assert_eq!(listed, vec![custom.clone()]);

    let enabled = client
        .enable_agent(&custom.id)
        .await
        .expect("enable custom agent");
    assert_eq!(enabled.id, custom.id);
    assert!(enabled.enabled);

    client
        .delete_custom_agent(&custom.id)
        .await
        .expect("delete custom agent");
    assert!(client
        .list_custom_agents()
        .await
        .expect("list after delete")
        .is_empty());
}

#[tokio::test]
async fn admin_client_rejects_a_client_plane_token() {
    let address = spawn_admin_server("admin-secret").await;
    let client = AdminClient::new(format!("http://{address}"), "client-secret");

    let error = client
        .disable_agent("registry-agent")
        .await
        .expect_err("client-plane token must not authorize admin routes");
    assert!(matches!(
        error,
        acpx_client::ext::admin::AdminClientError::Response { status: 401, .. }
    ));
}
