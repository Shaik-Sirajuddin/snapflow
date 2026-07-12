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
#[path = "../src/transport/ws.rs"]
mod ws;

use http::{serve, SharedRouter};

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
