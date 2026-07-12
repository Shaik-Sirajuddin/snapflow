//! Integration tests for the HTTP/WS transport (`transport::http`/`ws`).
//! Follows the same synthetic stand-in "backend" trick as
//! `acpx-core/tests/router_dispatch_test.rs` (a tiny `sh -c '...'` script
//! that echoes back a canned JSON-RPC response) so these tests don't
//! depend on a real ACP adapter being installed.

use std::net::SocketAddr;
use std::sync::Arc;

use acpx_conductor::SpawnSpec;
use acpx_core::router::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as WsMessage;

// `acpx-server` is a binary-only crate (no `[lib]` target, and adding one
// is outside this task's file ownership -- `main.rs`/`Cargo.toml`'s crate
// shape are owned by the main agent's concurrent work). Integration tests
// for a bin-only crate can't `use acpx_server::...`, so instead we compile
// the actual `transport::http`/`transport::ws` source files directly into
// this test binary via `#[path]`. This exercises the real production code
// (not a copy), just via a different crate root than `main.rs`'s.
// Declared directly at this file's (crate) root -- not nested inside an
// extra `mod transport { .. }` wrapper -- so `#[path]` here resolves
// relative to this file's own directory (`tests/`) rather than an
// implicit `tests/transport/` subdirectory. `http.rs`'s internal
// `super::ws::ws_handler` reference still resolves correctly since
// `super` from this `http` module is this crate's root, which is exactly
// where `ws` is declared too -- mirroring `src/transport/mod.rs`'s
// `pub mod http; pub mod ws;` shape.
#[path = "../src/transport/http.rs"]
mod http;
#[path = "../src/transport/ws.rs"]
mod ws;

use http::{serve, SharedRouter};

/// Echoes back a canned `session/new` result carrying `sessionId`
/// `"backend-abc"`, or `{"ok": true}` for anything else -- same shape as
/// the reference stand-in in `acpx-core/tests/router_dispatch_test.rs`.
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

/// Same as the default stand-in, but the `session/new` result carries a
/// distinguishable `agentTag` field so tests can tell which registered
/// agent actually served a request -- used to verify `X-Acpx-Profile`
/// header routing picks the right backend.
fn stand_in_backend_script_with_tag(tag: &str) -> String {
    format!(
        r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"sessionId":"backend-abc","agentTag":"{tag}"}}}}\n' "$id"
  else
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"ok":true}}}}\n' "$id"
  fi
done
"#
    )
}

fn stand_in_backend_spec() -> SpawnSpec {
    SpawnSpec::new(
        "sh",
        vec!["-c".to_string(), STAND_IN_BACKEND_SCRIPT.to_string()],
    )
}

fn tagged_backend_spec(tag: &str) -> SpawnSpec {
    SpawnSpec::new(
        "sh",
        vec!["-c".to_string(), stand_in_backend_script_with_tag(tag)],
    )
}

/// Starts `transport::serve` on `127.0.0.1:0` (OS-assigned port) in a
/// background task and returns the resolved local address to connect to.
/// We bind the listener ourselves (rather than letting `serve` do it) so
/// we can hand back the real port before the caller needs it -- `serve`'s
/// signature takes a `SocketAddr` to bind, so we probe an ephemeral port
/// first and pass that in; a `0.0.0.0`/`127.0.0.1:0` bind never collides
/// with another test running concurrently.
async fn spawn_server(router: SharedRouter) -> SocketAddr {
    // Bind once here to learn the OS-assigned port, then drop the
    // listener and immediately hand the same address to `serve` -- the
    // brief gap is safe for a local test (nothing else in this process
    // binds ports concurrently).
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = probe.local_addr().expect("local_addr");
    drop(probe);

    tokio::spawn(async move {
        serve(router, addr, None).await.expect("transport::serve");
    });

    // Give the listener a moment to come up before the test issues its
    // first request.
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    addr
}

#[tokio::test]
async fn http_post_rpc_round_trips_gateway_native_method() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;

    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{addr}/rpc"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/list",
            "params": {}
        }))
        .send()
        .await
        .expect("POST /rpc");
    assert!(response.status().is_success());
    let body: serde_json::Value = response.json().await.expect("json body");
    assert_eq!(body["jsonrpc"], json!("2.0"));
    assert_eq!(body["id"], json!(1));
    assert_eq!(body["result"]["sessions"], json!([]));
}

#[tokio::test]
async fn http_post_rpc_session_new_routes_via_profile_header() {
    let mut router = Router::new("agent-a");
    router.register_agent("agent-a", tagged_backend_spec("A"));
    router.register_agent("agent-b", tagged_backend_spec("B"));
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;

    let client = reqwest::Client::new();

    // `_acpx.profile` resolves through the Phase 3 `ProfileStore`, not a
    // raw agent id directly (see `router.rs`'s `resolve_profile`) -- a
    // profile whose `agent_id` names an already-registered spec (as
    // "agent-b" is, above) reuses that spec directly rather than
    // requiring a live registry entry, so this test double works
    // unmodified as a profile target.
    let create_profile = client
        .post(format!("http://{addr}/rpc"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "profiles/create",
            "params": {"name": "agent-b", "agent_id": "agent-b"}
        }))
        .send()
        .await
        .expect("POST /rpc (profiles/create)");
    assert!(create_profile.status().is_success());

    // No header -> falls back to the default agent ("agent-a").
    let default_response = client
        .post(format!("http://{addr}/rpc"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .send()
        .await
        .expect("POST /rpc (no header)")
        .json::<serde_json::Value>()
        .await
        .expect("json body");
    assert_eq!(default_response["result"]["agentTag"], json!("A"));

    // X-Acpx-Profile header selects "agent-b" instead, even though the
    // inline params carry no _acpx field at all.
    let header_response = client
        .post(format!("http://{addr}/rpc"))
        .header("X-Acpx-Profile", "agent-b")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .send()
        .await
        .expect("POST /rpc (with header)")
        .json::<serde_json::Value>()
        .await
        .expect("json body");
    assert_eq!(header_response["result"]["agentTag"], json!("B"));

    // Header also wins over an inline params._acpx.profile that names a
    // *different* agent -- highest precedence per 02-architecture.md.
    let override_response = client
        .post(format!("http://{addr}/rpc"))
        .header("X-Acpx-Profile", "agent-b")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/new",
            "params": {"cwd": "/tmp", "_acpx": {"profile": "agent-a"}}
        }))
        .send()
        .await
        .expect("POST /rpc (header overrides inline)")
        .json::<serde_json::Value>()
        .await
        .expect("json body");
    assert_eq!(override_response["result"]["agentTag"], json!("B"));
}

#[tokio::test]
async fn ws_round_trips_a_request() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;

    let (mut socket, _response) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("ws connect");

    socket
        .send(WsMessage::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "session/new",
                "params": {"cwd": "/tmp"}
            })
            .to_string(),
        ))
        .await
        .expect("send ws frame");

    let reply = socket
        .next()
        .await
        .expect("ws stream ended early")
        .expect("ws frame error");
    let text = match reply {
        WsMessage::Text(text) => text,
        other => panic!("expected text frame, got {other:?}"),
    };
    let body: serde_json::Value = serde_json::from_str(&text).expect("json body");
    assert_eq!(body["jsonrpc"], json!("2.0"));
    assert_eq!(body["id"], json!(1));
    assert!(body["result"]["sessionId"].is_string());
    // The client only ever sees the gateway-issued id, never the backend's
    // own "backend-abc" -- same invariant `router_dispatch_test.rs` checks
    // directly against `Router`.
    assert_ne!(body["result"]["sessionId"], json!("backend-abc"));
}
