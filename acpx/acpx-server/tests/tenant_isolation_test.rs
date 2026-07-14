//! **`acpx-tenant-isolation` Phase B** integration tests: real
//! `acpx-server` HTTP transport, two "clients" (plain `reqwest` calls
//! carrying different `X-Acpx-Tenant` headers -- standing in for two
//! separate ACP client processes connecting to the same daemon) sharing
//! one profile/backend, proving session data never crosses the tenant
//! boundary. Same synthetic stand-in "backend" trick and `#[path]`
//! compile-real-transport-source-directly approach as
//! `http_ws_transport_test.rs` (see that file's top-of-file comment for
//! why).

use std::net::SocketAddr;
use std::sync::Arc;

use acpx_conductor::SpawnSpec;
use acpx_core::router::Router;
use serde_json::json;
use tokio::sync::Mutex;

#[path = "../src/transport/http.rs"]
mod http;
#[path = "../src/transport/live.rs"]
mod live;
#[path = "../src/transport/ws.rs"]
mod ws;

use http::{serve, SharedRouter};

/// Replies to `session/new` with a fixed backend session id
/// (`backend-shared`, deliberately the *same* id every call, simulating
/// one shared physical backend process reused across multiple gateway
/// sessions/tenants -- see `01-architecture.md`'s "backend process
/// sharing stays the default" section) and to `session/list` with one
/// canned entry for that same id, so the per-backend `session/list` leak
/// this phase closes has something real to leak if the fix regresses.
const STAND_IN_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-shared"}}\n' "$id"
  elif echo "$line" | grep -q 'session/list'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessions":[{"sessionId":"backend-shared","cwd":"/tmp"}]}}\n' "$id"
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

/// Same as `http_ws_transport_test.rs`'s `spawn_server` -- bind an
/// ephemeral port, start `transport::serve` against it, return the
/// address once the listener is actually accepting connections.
async fn spawn_server(router: SharedRouter) -> SocketAddr {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = probe.local_addr().expect("local_addr");
    drop(probe);

    tokio::spawn(async move {
        serve(router, addr, None).await.expect("transport::serve");
    });

    for _ in 0..50 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    addr
}

/// **The core proof this whole plan exists for.** Two tenants ("tenant-a",
/// "tenant-b"), each its own `reqwest::Client` sending
/// `X-Acpx-Tenant: <tenant>` on every call -- standing in for two
/// independent ACP client processes -- each creates its own session
/// against the *same* registered agent (one shared physical backend
/// process, per acpx's own default), then each calls its own
/// gateway-aggregate `session/list` (no `_acpx` selector): each must see
/// only its own session, never the other tenant's, even though both
/// sessions live inside the exact same daemon process and
/// `SessionRegistry`.
#[tokio::test]
async fn two_tenants_never_see_each_others_sessions_in_the_gateway_aggregate() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;
    let client = reqwest::Client::new();

    let new_a = client
        .post(format!("http://{addr}/rpc"))
        .header("X-Acpx-Tenant", "tenant-a")
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp/a"}
        }))
        .send()
        .await
        .expect("POST /rpc (tenant-a session/new)")
        .json::<serde_json::Value>()
        .await
        .expect("json body");
    let gateway_id_a = new_a["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    let new_b = client
        .post(format!("http://{addr}/rpc"))
        .header("X-Acpx-Tenant", "tenant-b")
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp/b"}
        }))
        .send()
        .await
        .expect("POST /rpc (tenant-b session/new)")
        .json::<serde_json::Value>()
        .await
        .expect("json body");
    let gateway_id_b = new_b["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    assert_ne!(
        gateway_id_a, gateway_id_b,
        "each tenant's session/new must mint its own distinct gateway id"
    );

    let list_a = client
        .post(format!("http://{addr}/rpc"))
        .header("X-Acpx-Tenant", "tenant-a")
        .json(&json!({"jsonrpc": "2.0", "id": 2, "method": "session/list", "params": {}}))
        .send()
        .await
        .expect("POST /rpc (tenant-a session/list)")
        .json::<serde_json::Value>()
        .await
        .expect("json body");
    let sessions_a = list_a["result"]["sessions"]
        .as_array()
        .expect("sessions array");
    assert_eq!(
        sessions_a.len(),
        1,
        "tenant-a must see exactly its own one session: {sessions_a:?}"
    );
    assert_eq!(sessions_a[0]["sessionId"], json!(gateway_id_a));

    let list_b = client
        .post(format!("http://{addr}/rpc"))
        .header("X-Acpx-Tenant", "tenant-b")
        .json(&json!({"jsonrpc": "2.0", "id": 2, "method": "session/list", "params": {}}))
        .send()
        .await
        .expect("POST /rpc (tenant-b session/list)")
        .json::<serde_json::Value>()
        .await
        .expect("json body");
    let sessions_b = list_b["result"]["sessions"]
        .as_array()
        .expect("sessions array");
    assert_eq!(
        sessions_b.len(),
        1,
        "tenant-b must see exactly its own one session, not tenant-a's: {sessions_b:?}"
    );
    assert_eq!(sessions_b[0]["sessionId"], json!(gateway_id_b));

    // No header at all -> the implicit "default" tenant, which created
    // neither session above -- must see an empty aggregate, not either
    // tenant's session (proves the default tenant is genuinely its own
    // separate, empty namespace here, not an alias for "everyone").
    let list_default = client
        .post(format!("http://{addr}/rpc"))
        .json(&json!({"jsonrpc": "2.0", "id": 2, "method": "session/list", "params": {}}))
        .send()
        .await
        .expect("POST /rpc (default tenant session/list)")
        .json::<serde_json::Value>()
        .await
        .expect("json body");
    assert_eq!(list_default["result"]["sessions"], json!([]));
}

/// **The real-backend-leak proof.** Both tenants share the exact same
/// physical backend process (one `stand-in-agent` registration), so the
/// backend's own `session/list` reply (via `_acpx.agentId`) would
/// legitimately include `backend-shared` for *either* tenant's request --
/// this test proves the per-backend path filters it correctly: only the
/// tenant that actually owns `backend-shared` in its own `SessionRegistry`
/// submap sees it; the other tenant's real per-backend `session/list`
/// comes back empty, never leaking the session id, cwd, or existence of
/// the other tenant's session.
#[tokio::test]
async fn real_per_backend_session_list_never_leaks_another_tenants_session() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;
    let client = reqwest::Client::new();

    // tenant-a creates the (only) session against this shared backend.
    let new_a = client
        .post(format!("http://{addr}/rpc"))
        .header("X-Acpx-Tenant", "tenant-a")
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp/a"}
        }))
        .send()
        .await
        .expect("POST /rpc (tenant-a session/new)")
        .json::<serde_json::Value>()
        .await
        .expect("json body");
    let gateway_id_a = new_a["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    // tenant-a's own real per-backend session/list: must see its session,
    // translated to its own already-known gateway id.
    let list_a = client
        .post(format!("http://{addr}/rpc"))
        .header("X-Acpx-Tenant", "tenant-a")
        .json(&json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/list",
            "params": {"_acpx": {"agentId": "stand-in-agent"}}
        }))
        .send()
        .await
        .expect("POST /rpc (tenant-a real session/list)")
        .json::<serde_json::Value>()
        .await
        .expect("json body");
    let sessions_a = list_a["result"]["sessions"]
        .as_array()
        .expect("sessions array");
    assert_eq!(
        sessions_a.len(),
        1,
        "tenant-a should see its own session: {sessions_a:?}"
    );
    assert_eq!(sessions_a[0]["sessionId"], json!(gateway_id_a));

    // tenant-b's real per-backend session/list against the *same*
    // backend must come back empty -- the backend legitimately reports
    // `backend-shared`, but it's owned by tenant-a, so the leak-fix
    // filter must drop it rather than let tenant-b adopt or even see it.
    let list_b = client
        .post(format!("http://{addr}/rpc"))
        .header("X-Acpx-Tenant", "tenant-b")
        .json(&json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/list",
            "params": {"_acpx": {"agentId": "stand-in-agent"}}
        }))
        .send()
        .await
        .expect("POST /rpc (tenant-b real session/list)")
        .json::<serde_json::Value>()
        .await
        .expect("json body");
    let sessions_b = list_b["result"]["sessions"]
        .as_array()
        .expect("sessions array");
    assert_eq!(
        sessions_b.len(),
        0,
        "tenant-b must never see tenant-a's session via the shared backend's own \
         session/list, even though the backend process reports it: {sessions_b:?}"
    );
}

/// A tenant cannot use another tenant's gateway session id at all --
/// `session/prompt` (a `Proxied` method) against tenant-a's id, issued as
/// tenant-b, must fail with the same "unknown session" error a
/// genuinely-nonexistent id would produce (no distinguishable
/// cross-tenant existence leak, per `01-architecture.md`).
#[tokio::test]
async fn a_tenant_cannot_prompt_against_another_tenants_gateway_session_id() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;
    let client = reqwest::Client::new();

    let new_a = client
        .post(format!("http://{addr}/rpc"))
        .header("X-Acpx-Tenant", "tenant-a")
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp/a"}
        }))
        .send()
        .await
        .expect("POST /rpc (tenant-a session/new)")
        .json::<serde_json::Value>()
        .await
        .expect("json body");
    let gateway_id_a = new_a["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    let prompt_as_b = client
        .post(format!("http://{addr}/rpc"))
        .header("X-Acpx-Tenant", "tenant-b")
        .json(&json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
            "params": {"sessionId": gateway_id_a, "prompt": []}
        }))
        .send()
        .await
        .expect("POST /rpc (tenant-b session/prompt against tenant-a's id)")
        .json::<serde_json::Value>()
        .await
        .expect("json body");

    assert!(
        prompt_as_b.get("error").is_some(),
        "tenant-b must not be able to use tenant-a's gateway session id at all: {prompt_as_b:?}"
    );
    let message = prompt_as_b["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("no session registered"),
        "expected an unknown-session error, got: {message}"
    );

    // The *same* prompt, issued as tenant-a (the actual owner), should
    // still work fine -- proves the rejection above is genuinely
    // tenant-scoped, not a general breakage of session/prompt against
    // this stand-in backend.
    let prompt_as_a = client
        .post(format!("http://{addr}/rpc"))
        .header("X-Acpx-Tenant", "tenant-a")
        .json(&json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": {"sessionId": gateway_id_a, "prompt": []}
        }))
        .send()
        .await
        .expect("POST /rpc (tenant-a session/prompt against its own id)")
        .json::<serde_json::Value>()
        .await
        .expect("json body");
    assert!(
        prompt_as_a.get("error").is_none(),
        "tenant-a's own prompt against its own session should succeed: {prompt_as_a:?}"
    );
}
