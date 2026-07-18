//! Integration tests for `transport::http`'s optional bearer-token auth
//! (`ACPX_AUTH_TOKEN` / `AuthConfig`), added as part of a post-Phase-6
//! self-review closing this workspace's previously-open "No auth/TLS
//! yet" gap (see `transport::http`'s module doc comment). Follows the
//! same `#[path]`-including-real-source technique as
//! `http_ws_transport_test.rs` -- see that file's doc comment for why.
use std::net::SocketAddr;
use std::sync::Arc;

use acpx_conductor::SpawnSpec;
use acpx_core::router::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::http::Request as WsRequest;

#[path = "../src/transport/http.rs"]
mod http;
#[path = "../src/transport/live.rs"]
mod live;
#[path = "../src/transport/ws.rs"]
mod ws;

use acpx_core::TenantId;
use http::{serve, serve_on_with_bridge_and_tenant_tokens, SharedRouter};

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

fn new_router() -> SharedRouter {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    Arc::new(Mutex::new(router))
}

/// Same ephemeral-port bring-up as `http_ws_transport_test.rs`, but takes
/// an explicit `auth_token` to exercise both the disabled (`None`) and
/// enabled (`Some(..)`) paths.
async fn spawn_server(router: SharedRouter, auth_token: Option<String>) -> SocketAddr {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = probe.local_addr().expect("local_addr");
    drop(probe);

    tokio::spawn(async move {
        serve(router, addr, auth_token)
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

fn ping_request() -> serde_json::Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": "agents/list", "params": {}})
}

/// Same bring-up as `spawn_server`, but wires identity-bound tenant
/// tokens (`ACPX_AUTH_TENANT_TOKENS` equivalent) through
/// `serve_on_with_bridge_and_tenant_tokens`, exercising the
/// `tenant_identity_boundary` hardening item from
/// `acpx-tenant-isolation`'s plan: a tenant is derived from the
/// authenticated token, not a self-declared header, whenever a
/// tenant-bound token is configured.
async fn spawn_server_with_tenant_tokens(
    router: SharedRouter,
    auth_token: Option<String>,
    tenant_tokens: Vec<(String, TenantId)>,
) -> SocketAddr {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = probe.local_addr().expect("local_addr");
    drop(probe);

    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
        serve_on_with_bridge_and_tenant_tokens(
            listener,
            router,
            auth_token,
            tenant_tokens,
            None,
            None,
        )
        .await
        .expect("transport::serve_on_with_bridge_and_tenant_tokens");
    });

    for _ in 0..50 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    addr
}

/// Same bring-up as `spawn_server_with_tenant_tokens`, but additionally
/// configures a tenant allowlist (`ACPX_TENANT_ALLOWLIST` equivalent),
/// covering the `tenant_namespace_governance` hardening item.
async fn spawn_server_with_tenant_allowlist(
    router: SharedRouter,
    allowlist: std::collections::HashSet<String>,
) -> SocketAddr {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = probe.local_addr().expect("local_addr");
    drop(probe);

    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
        serve_on_with_bridge_and_tenant_tokens(
            listener,
            router,
            None,
            Vec::new(),
            Some(allowlist),
            None,
        )
        .await
        .expect("transport::serve_on_with_bridge_and_tenant_tokens");
    });

    for _ in 0..50 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    addr
}

#[tokio::test]
async fn allowlisted_tenant_header_is_accepted() {
    let addr = spawn_server_with_tenant_allowlist(
        new_router(),
        std::collections::HashSet::from(["acme".to_string()]),
    )
    .await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/rpc"))
        .header("X-Acpx-Tenant", "acme")
        .json(&tenant_probe_request())
        .send()
        .await
        .expect("POST /rpc");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
async fn non_allowlisted_tenant_header_is_rejected() {
    let addr = spawn_server_with_tenant_allowlist(
        new_router(),
        std::collections::HashSet::from(["acme".to_string()]),
    )
    .await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/rpc"))
        .header("X-Acpx-Tenant", "not-on-the-list")
        .json(&tenant_probe_request())
        .send()
        .await
        .expect("POST /rpc");
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

/// Absent header resolves to `default_tenant()` ("default"), which is
/// not itself in the allowlist here -- confirms the allowlist applies to
/// the implicit default tenant too, not just explicitly declared ones.
#[tokio::test]
async fn default_tenant_is_rejected_when_not_on_an_active_allowlist() {
    let addr = spawn_server_with_tenant_allowlist(
        new_router(),
        std::collections::HashSet::from(["acme".to_string()]),
    )
    .await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/rpc"))
        .json(&tenant_probe_request())
        .send()
        .await
        .expect("POST /rpc");
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

fn tenant_probe_request() -> serde_json::Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": "agents/list", "params": {}})
}

/// A request presenting a tenant-bound token gets that token's tenant,
/// regardless of any (absent, here) `X-Acpx-Tenant` header -- the
/// baseline "authenticated identity determines the tenant" case.
#[tokio::test]
async fn tenant_bound_token_resolves_its_own_tenant_without_header() {
    let addr = spawn_server_with_tenant_tokens(
        new_router(),
        None,
        vec![("tok-acme".to_string(), TenantId::from("acme"))],
    )
    .await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/rpc"))
        .bearer_auth("tok-acme")
        .json(&tenant_probe_request())
        .send()
        .await
        .expect("POST /rpc");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
}

/// A tenant-bound token's identity is authoritative: a request presenting
/// it while also claiming a *different* tenant via `X-Acpx-Tenant` is
/// rejected outright (`403`) rather than silently using either value.
#[tokio::test]
async fn tenant_bound_token_rejects_conflicting_header_claim() {
    let addr = spawn_server_with_tenant_tokens(
        new_router(),
        None,
        vec![("tok-acme".to_string(), TenantId::from("acme"))],
    )
    .await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/rpc"))
        .bearer_auth("tok-acme")
        .header("X-Acpx-Tenant", "someone-else")
        .json(&tenant_probe_request())
        .send()
        .await
        .expect("POST /rpc");
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

/// A tenant-bound token's identity still matches an *equal* header value
/// -- an honest client stating its own already-implied tenant is not
/// penalized for being explicit.
#[tokio::test]
async fn tenant_bound_token_allows_matching_header_claim() {
    let addr = spawn_server_with_tenant_tokens(
        new_router(),
        None,
        vec![("tok-acme".to_string(), TenantId::from("acme"))],
    )
    .await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/rpc"))
        .bearer_auth("tok-acme")
        .header("X-Acpx-Tenant", "acme")
        .json(&tenant_probe_request())
        .send()
        .await
        .expect("POST /rpc");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
}

/// The plain global token remains additive: it still authorizes
/// requests, and (carrying no tenant binding of its own) still falls
/// back to the pre-existing self-declared `X-Acpx-Tenant` header
/// behavior, unaffected by an unrelated tenant-bound token also being
/// configured.
#[tokio::test]
async fn global_token_still_falls_back_to_self_declared_tenant_header() {
    let addr = spawn_server_with_tenant_tokens(
        new_router(),
        Some("shared-secret".to_string()),
        vec![("tok-acme".to_string(), TenantId::from("acme"))],
    )
    .await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/rpc"))
        .bearer_auth("shared-secret")
        .header("X-Acpx-Tenant", "whatever-self-declared")
        .json(&tenant_probe_request())
        .send()
        .await
        .expect("POST /rpc");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
}

/// A token matching neither the global token nor any tenant-bound token
/// is unauthorized, same as the pre-existing single-token contract.
#[tokio::test]
async fn unrecognized_token_is_rejected_even_with_tenant_tokens_configured() {
    let addr = spawn_server_with_tenant_tokens(
        new_router(),
        Some("shared-secret".to_string()),
        vec![("tok-acme".to_string(), TenantId::from("acme"))],
    )
    .await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/rpc"))
        .bearer_auth("nope")
        .json(&tenant_probe_request())
        .send()
        .await
        .expect("POST /rpc");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

/// Baseline: `ACPX_AUTH_TOKEN` unset (`None` here) means every
/// pre-existing test's assumption -- fully unauthenticated -- still
/// holds. Not a new behavior, but the contract every other test in this
/// workspace already implicitly relies on; asserted explicitly here so a
/// future change to the default can't silently start requiring auth.
#[tokio::test]
async fn no_token_configured_requests_succeed_unauthenticated() {
    let addr = spawn_server(new_router(), None).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/rpc"))
        .json(&ping_request())
        .send()
        .await
        .expect("POST /rpc");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("json body");
    assert!(body.get("error").is_none(), "unexpected error: {body:?}");
}

#[tokio::test]
async fn health_reports_ready_without_persistence_and_requires_auth_when_configured() {
    let addr = spawn_server(new_router(), Some("s3cret".to_string())).await;
    let client = reqwest::Client::new();
    let rejected = client
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .expect("GET /health without auth");
    assert_eq!(rejected.status(), reqwest::StatusCode::UNAUTHORIZED);

    let response = client
        .get(format!("http://{addr}/health"))
        .bearer_auth("s3cret")
        .send()
        .await
        .expect("GET /health");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("health JSON");
    assert_eq!(body["status"], json!("ready"));
    assert_eq!(body["persistenceEnabled"], json!(false));
    assert_eq!(body["recovery"]["restoring"], json!(0));
}

#[tokio::test]
async fn correct_bearer_token_succeeds() {
    let addr = spawn_server(new_router(), Some("s3cret".to_string())).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/rpc"))
        .bearer_auth("s3cret")
        .json(&ping_request())
        .send()
        .await
        .expect("POST /rpc");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("json body");
    assert!(body.get("error").is_none(), "unexpected error: {body:?}");
}

#[tokio::test]
async fn missing_bearer_token_is_rejected() {
    let addr = spawn_server(new_router(), Some("s3cret".to_string())).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/rpc"))
        .json(&ping_request())
        .send()
        .await
        .expect("POST /rpc");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = resp.json().await.expect("json body");
    assert_eq!(
        body["error"]["code"],
        json!(-32001),
        "401 body should still be a parseable JSON-RPC error envelope: {body:?}"
    );
}

#[tokio::test]
async fn wrong_bearer_token_is_rejected() {
    let addr = spawn_server(new_router(), Some("s3cret".to_string())).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/rpc"))
        .bearer_auth("totally-wrong")
        .json(&ping_request())
        .send()
        .await
        .expect("POST /rpc");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

/// Manually builds the WS upgrade request (rather than
/// `tokio_tungstenite::connect_async`, which panics/errors opaquely on a
/// non-101 response) so a rejected upgrade's actual status code can be
/// asserted directly.
async fn raw_ws_upgrade_status(addr: SocketAddr, token: Option<&str>) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let key = generate_key();
    let mut request = format!(
        "GET /ws HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\n"
    );
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write upgrade request");

    let mut buf = vec![0u8; 512];
    let n = stream.read(&mut buf).await.expect("read response");
    let response = String::from_utf8_lossy(&buf[..n]);
    let status_line = response.lines().next().expect("status line");
    // e.g. "HTTP/1.1 101 Switching Protocols" or "HTTP/1.1 401 Unauthorized"
    status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("parseable status code")
}

#[tokio::test]
async fn ws_upgrade_with_correct_token_succeeds_and_round_trips() {
    let addr = spawn_server(new_router(), Some("s3cret".to_string())).await;

    let mut request = WsRequest::builder()
        .uri(format!("ws://{addr}/ws"))
        .header("Authorization", "Bearer s3cret")
        .header("Sec-WebSocket-Key", generate_key())
        .header("Sec-WebSocket-Version", "13")
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .body(())
        .expect("build ws request");
    request
        .headers_mut()
        .insert("Host", addr.to_string().parse().unwrap());
    let (mut ws_stream, response) = tokio_tungstenite::connect_async(request)
        .await
        .expect("ws connect with correct token");
    assert_eq!(response.status(), 101);

    ws_stream
        .send(tokio_tungstenite::tungstenite::Message::Text(
            ping_request().to_string(),
        ))
        .await
        .expect("send");
    let reply = ws_stream
        .next()
        .await
        .expect("reply present")
        .expect("reply ok");
    let reply: serde_json::Value =
        serde_json::from_str(&reply.into_text().unwrap()).expect("json reply");
    assert!(reply.get("error").is_none(), "unexpected error: {reply:?}");
}

#[tokio::test]
async fn ws_upgrade_without_token_is_rejected() {
    let addr = spawn_server(new_router(), Some("s3cret".to_string())).await;
    let status = raw_ws_upgrade_status(addr, None).await;
    assert_eq!(status, 401, "upgrade without a token must be rejected");
}

#[tokio::test]
async fn ws_upgrade_with_wrong_token_is_rejected() {
    let addr = spawn_server(new_router(), Some("s3cret".to_string())).await;
    let status = raw_ws_upgrade_status(addr, Some("nope")).await;
    assert_eq!(status, 401, "upgrade with a wrong token must be rejected");
}
