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
use acpx_client::{AgentRequest, Gateway, TransportMode};
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

/// Same as [`spawn_server`], but also returns the server's own
/// `JoinHandle` so a test can `.abort()` it -- simulating a real gateway
/// process dying underneath a live connection, for the reconnect
/// regression test below.
async fn spawn_server_with_handle(router: SharedRouter) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = probe.local_addr().expect("local_addr");
    drop(probe);
    let handle = tokio::spawn(async move {
        // Deliberately not `.expect(...)`: the reconnect test aborts
        // this task while `serve` is still running, which would
        // otherwise print a spurious "future dropped" panic-looking
        // line on an intentional abort. Errors, if any, are dropped;
        // the caller observes success/failure through real network
        // calls instead.
        let _ = http::serve(router, addr, None).await;
    });
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    (addr, handle)
}

/// Binds a server on a *specific* address (rather than an ephemeral
/// port) -- used to rebind the exact address a previous server on this
/// same address was just killed on, for the reconnect regression test.
async fn spawn_server_on(addr: SocketAddr, router: SharedRouter) -> tokio::task::JoinHandle<()> {
    let handle = tokio::spawn(async move {
        http::serve(router, addr, None)
            .await
            .expect("transport::serve");
    });
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    handle
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
async fn gateway_facade_prefers_websocket_and_round_trips_rpc() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;

    let client = Gateway::connect(format!("http://{addr}")).await;
    assert_eq!(client.mode(), TransportMode::WebSocketInteractive);
    assert!(client.supports_interactive_requests());
    assert!(client.subscribe().is_some());

    let result = client
        .call("session/list", serde_json::json!({}), None)
        .await
        .expect("session/list over WebSocket");
    assert_eq!(result["sessions"], serde_json::json!([]));
}

/// Regression test: "websocket request to acpx gateway failed" with no
/// recovery -- `Gateway` used to connect exactly once at construction
/// and never retry, so a gateway process dying (killed, crashed,
/// restarted) permanently broke every thread bound to that connection
/// for the rest of the app's lifetime, no matter how long the user then
/// waited or how many messages they sent (this is exactly what happened
/// live this session: killing a stray gateway process left the panel
/// stuck against a dead socket with no way to recover short of
/// restarting the whole app).
///
/// Proves the real fix end to end, not just that `reconnect()` exists in
/// isolation: kill the real server process this `Gateway`'s WebSocket is
/// connected to, rebind a *second*, independent server on the exact same
/// address, and confirm a subsequent `call()` on the *same* `Gateway`
/// instance (same `base_url`, no test-only backdoor) transparently
/// reconnects and completes -- the actual "message send hits a dead
/// socket -> Gateway reconnects -> the request actually completes" path
/// a real `AgentBridge` call goes through.
#[tokio::test]
async fn gateway_call_survives_the_server_dying_and_restarting_by_reconnecting() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let (addr, server_task) = spawn_server_with_handle(router).await;

    let client = Gateway::connect(format!("http://{addr}")).await;
    assert_eq!(client.mode(), TransportMode::WebSocketInteractive);
    client
        .call("session/list", serde_json::json!({}), None)
        .await
        .expect("session/list must succeed against the live server");

    // Kill the server task -- the test-harness equivalent of the real
    // gateway process dying underneath a live connection. This drops
    // the listening socket and every accepted connection, so the
    // client's existing WebSocket stream observes a close/read error.
    server_task.abort();
    // Let the abort actually land and the port free up before rebinding
    // it -- best-effort; spawn_server_with_handle's own connect-retry
    // loop below still tolerates a slightly slow bind.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let mut second_router = Router::new("stand-in-agent");
    second_router.register_agent("stand-in-agent", stand_in_backend_spec());
    let second_router: SharedRouter = Arc::new(Mutex::new(second_router));
    let _second_server_task = spawn_server_on(addr, second_router).await;

    // No manual reconnect() call here -- call() itself must notice the
    // dead connection and recover on its own, exactly as a live
    // AgentBridge send would.
    let result = client
        .call("session/list", serde_json::json!({}), None)
        .await
        .expect("call must transparently reconnect and succeed against the restarted server");
    assert_eq!(result["sessions"], serde_json::json!([]));
    assert_eq!(
        client.mode(),
        TransportMode::WebSocketInteractive,
        "mode() must reflect the freshly reconnected WebSocket, not the dead one"
    );
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

/// **acpx-client SDK level, end to end**: proves the whole interactive
/// relay contract works through the public [`Gateway`] facade, not just
/// the raw transport `acpx-server`'s own `agent_request_relay_test.rs`
/// already proves -- `Gateway::connect` picks up the live
/// `acpx/agent_request` notification via `subscribe()`,
/// `AgentRequest::from_notification` parses it, and
/// `Gateway::respond_agent_request` answers it, all through the same
/// APIs a real panel/consumer uses. Same stand-in backend script and
/// deliberately-distinguishable-outcome trick as `acpx-server`'s own
/// `agent_request_relay_test.rs` (see that file's doc comment): relaying
/// `allow-once` here can only be distinguished from the profile's
/// `AutoReject` policy default (`reject-once`, since a `reject_once`
/// option is offered) by the live relay path actually having run.
#[tokio::test]
async fn gateway_relays_a_live_permission_request_end_to_end() {
    let permission_script = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"session/new"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  elif echo "$line" | grep -q '"method":"session/prompt"'; then
    printf '{"jsonrpc":"2.0","id":999,"method":"session/request_permission","params":{"sessionId":"backend-abc","toolCall":{"toolCallId":"call-1"},"options":[{"optionId":"allow-once","name":"Allow once","kind":"allow_once"},{"optionId":"reject-once","name":"Reject","kind":"reject_once"}]}}\n'
    reply=""
    while IFS= read -r reply_line; do
      echo "$reply_line" | grep -q '"id":999' && { reply="$reply_line"; break; }
    done
    chosen=$(echo "$reply" | grep -o '"optionId":"[^"]*"' | head -1 | cut -d: -f2 | tr -d '"')
    printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn","chosenOptionId":"%s"}}\n' "$id" "$chosen"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;
    let mut router = Router::new("permission-agent");
    router.register_agent(
        "permission-agent",
        SpawnSpec::new("sh", vec!["-c".to_string(), permission_script.to_string()]),
    );
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;

    let gateway = Gateway::connect(format!("http://{addr}")).await;
    assert_eq!(gateway.mode(), TransportMode::WebSocketInteractive);
    let mut notifications = gateway.subscribe().expect("WS mode has notifications");

    let new_result = gateway
        .call("session/new", serde_json::json!({"cwd": "/tmp"}), None)
        .await
        .expect("session/new");
    let gateway_session_id = new_result["sessionId"].as_str().expect("sessionId").to_string();

    let prompt_params = serde_json::json!({"sessionId": gateway_session_id, "prompt": []});
    let gateway_for_prompt = &gateway;
    let prompt_task = async {
        gateway_for_prompt
            .call("session/prompt", prompt_params, None)
            .await
    };

    let answer_task = async {
        loop {
            let notification = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                notifications.recv(),
            )
            .await
            .expect("agent request notification arrives promptly")
            .expect("notification channel stays open");
            let Some(agent_request) = AgentRequest::from_notification(&notification) else {
                continue;
            };
            assert_eq!(agent_request.session_id, gateway_session_id);
            assert_eq!(agent_request.method(), Some("session/request_permission"));
            let backend_request_id = agent_request.request["id"].clone();
            let delivered = gateway
                .respond_agent_request(
                    &agent_request.relay_id,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": backend_request_id,
                        "result": {"outcome": {"outcome": "selected", "optionId": "allow-once"}}
                    }),
                )
                .await
                .expect("respond_agent_request");
            assert!(delivered);
            return;
        }
    };

    let (prompt_result, _) = tokio::join!(prompt_task, answer_task);
    let prompt_result = prompt_result.expect("session/prompt");
    assert_eq!(prompt_result["chosenOptionId"], serde_json::json!("allow-once"));
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

    // setup-followups plan, agent_settings_ordering_and_install_enable_
    // flow: the read side (panel-rust's AgentBridge::agent_enablement_map
    // uses this exact method) must reflect the disable above.
    let listed_agents = client.list_agents().await.expect("list_agents");
    let registry_agent = listed_agents
        .iter()
        .find(|a| a.id == "registry-agent")
        .expect("registry-agent present in admin list_agents");
    assert!(
        !registry_agent.enabled,
        "expected list_agents to reflect the disable_agent call above"
    );

    let reenabled = client
        .enable_agent("registry-agent")
        .await
        .expect("re-enable registry agent");
    assert!(reenabled.enabled);
    let listed_agents = client.list_agents().await.expect("list_agents after re-enable");
    assert!(
        listed_agents
            .iter()
            .find(|a| a.id == "registry-agent")
            .expect("registry-agent present")
            .enabled,
        "expected list_agents to reflect the re-enable"
    );

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
