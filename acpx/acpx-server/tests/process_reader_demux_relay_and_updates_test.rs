//! Regression coverage for the three `process_reader_demux` gaps closed
//! alongside flipping its default to on (see `Router::process_reader_
//! demux`'s field doc comment for the full "why it is now safe to
//! default on" writeup): before this fix, once `process_reader_demux`
//! activated for a shared backend process, every live
//! `InteractionHub`/`AgentRequestHub` relay (permission requests,
//! `fs/*`/`terminal/create` approvals) and every `POST /rpc` caller's
//! `_acpx.updates` silently stopped working for *every* session sharing
//! that process -- because `spawn_demux_consumer`'s single per-process
//! `LiveNotifyCtx` never knew any one session's `tenant_id`/
//! `gateway_session_id` ahead of time, and the four affected code paths
//! (`try_forward_interaction`, `try_relay_agent_request`, the
//! `terminal/create` live-stream spawn, and the legacy inline
//! `_acpx.updates` buffering) all just gave up on `None` instead of
//! resolving it per-frame the way `try_deliver_live` already did.
//!
//! Every test here forces `process_reader_demux` on explicitly
//! (`Router::with_process_reader_demux(true)`) rather than relying on
//! `ServerConfig`'s env-driven default, so these stay meaningful
//! regardless of which way that default is set -- this is exactly the
//! `#[path]`-compiled-real-transport-source trick `http_ws_transport_
//! test.rs`/`agent_request_relay_test.rs` already use, duplicated here
//! per those files' own "two independent test binaries, no shared
//! crate to put a helper in" rationale.

use std::net::SocketAddr;
use std::sync::Arc;

use acpx_bridge::{BridgeConfig, BridgeModel};
use acpx_conductor::SpawnSpec;
use acpx_core::router::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as WsMessage;

#[path = "../src/transport/http.rs"]
mod http;
#[path = "../src/transport/live.rs"]
mod live;
#[path = "../src/transport/ws.rs"]
mod ws;

use http::{serve, serve_on_with_bridge, SharedRouter};

async fn spawn_server(router: SharedRouter) -> SocketAddr {
    // Bind once here to learn the OS-assigned port, then drop the
    // listener and immediately hand the same address to `serve` -- same
    // "brief gap is safe for a local test" convention `http_ws_transport_
    // test.rs`'s own `spawn_server` uses.
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

async fn spawn_server_with_bridge(router: SharedRouter, bridge: BridgeConfig) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        serve_on_with_bridge(listener, router, None, Some(bridge))
            .await
            .expect("transport::serve_on_with_bridge");
    });
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    addr
}

/// Same stand-in "asks permission mid-turn" backend
/// `http_ws_transport_test.rs`'s `permission_asking_backend_spec` uses,
/// duplicated for this independent test binary.
fn permission_asking_backend_spec() -> SpawnSpec {
    let script = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"session/new"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-perm"}}\n' "$id"
  elif echo "$line" | grep -q '"method":"session/prompt"'; then
    printf '{"jsonrpc":"2.0","id":999,"method":"session/request_permission","params":{"sessionId":"backend-perm","toolCall":{"toolCallId":"call-1"},"options":[{"optionId":"allow-once","name":"Allow once","kind":"allow_once"},{"optionId":"reject-once","name":"Reject","kind":"reject_once"}]}}\n'
    while IFS= read -r reply_line; do
      echo "$reply_line" | grep -q '"id":"999"' && break
      echo "$reply_line" | grep -q '"id":999' && break
    done
    printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;
    SpawnSpec::new("sh", vec!["-c".to_string(), script.to_string()])
}

/// Same stand-in backend `agent_request_relay_test.rs`'s own `STAND_IN_
/// PERMISSION_BACKEND_SCRIPT` uses: echoes back whichever `optionId` it
/// was actually given as `result.chosenOptionId`, so a passing
/// assertion on that field can only mean the live relay path answered,
/// not `AutoReject`'s static `reject-once` fallback.
fn chosen_option_echoing_permission_backend_spec() -> SpawnSpec {
    let script = r#"
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
    SpawnSpec::new("sh", vec!["-c".to_string(), script.to_string()])
}

async fn ws_send(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    value: serde_json::Value,
) {
    socket
        .send(WsMessage::Text(value.to_string()))
        .await
        .expect("send frame");
}

async fn ws_receive(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> serde_json::Value {
    let frame = socket
        .next()
        .await
        .expect("socket ended")
        .expect("socket frame");
    let WsMessage::Text(text) = frame else {
        panic!("expected text frame");
    };
    serde_json::from_str(&text).expect("JSON frame")
}

/// The Zed-relevant regression: strict ACP bridge (`/acp/ws`, the exact
/// surface `acpx-acp-bridge` speaks to on Zed's behalf) forwards a live
/// mid-turn `session/request_permission` to the bound client instead of
/// silently auto-deciding it via policy, *even when the backend process
/// is already demuxed*. Byte-for-byte the same scenario as
/// `http_ws_transport_test.rs`'s `strict_acp_ws_forwards_backend_
/// permission_requests_to_the_bound_client`, just with `process_reader_
/// demux` forced on -- this is exactly the case that silently regressed
/// to the `AutoReject` fallback pre-fix (`InteractionHub` relay requires
/// `LiveNotifyCtx::tenant_id`, which `spawn_demux_consumer`'s ctx always
/// left `None`).
#[tokio::test(flavor = "multi_thread")]
async fn strict_acp_ws_forwards_permission_requests_under_demux() {
    let mut router = Router::new("permission-agent");
    router.register_agent("permission-agent", permission_asking_backend_spec());
    let router = router.with_process_reader_demux(true);
    let addr = spawn_server_with_bridge(
        Arc::new(Mutex::new(router)),
        BridgeConfig {
            default_model: "perm/model".to_string(),
            models: vec![BridgeModel {
                id: "perm/model".to_string(),
                name: None,
                agent_id: "permission-agent".to_string(),
                model_id: "perm-model".to_string(),
            }],
            max_virtual_sessions_per_tenant: None,
        },
    )
    .await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/acp/ws"))
        .await
        .expect("strict ACP websocket connect");

    ws_send(
        &mut socket,
        json!({"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp"}}),
    )
    .await;
    let created = ws_receive(&mut socket).await;
    let session_id = created["result"]["sessionId"].as_str().unwrap().to_string();

    ws_send(
        &mut socket,
        json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":session_id,"prompt":[]}}),
    )
    .await;

    // If this were falling through to the static auto-decide fallback
    // (the pre-fix regression under demux), this frame never arrives --
    // the very next frame received would already be the final `id: 2`
    // response, auto-decided by policy without this client ever knowing
    // it was asked.
    let permission_request = tokio::time::timeout(std::time::Duration::from_secs(10), ws_receive(&mut socket))
        .await
        .expect("timed out waiting for the live-relayed permission request -- demux broke the relay");
    assert_eq!(
        permission_request["method"],
        json!("session/request_permission"),
        "expected the backend's permission request forwarded live under demux, got {permission_request:?}"
    );
    let interaction_id = permission_request["id"].clone();

    ws_send(
        &mut socket,
        json!({
            "jsonrpc": "2.0",
            "id": interaction_id,
            "result": {"outcome": {"outcome": "selected", "optionId": "allow-once"}}
        }),
    )
    .await;

    let final_response = tokio::time::timeout(std::time::Duration::from_secs(10), ws_receive(&mut socket))
        .await
        .expect("timed out waiting for the prompt result after answering the relayed request");
    assert_eq!(final_response["id"], json!(2));
    assert_eq!(final_response["result"]["stopReason"], json!("end_turn"));
}

/// Same regression, the native (non-bridge) `AgentRequestHub` relay path
/// (used by `transport::ws`'s own `acpx/agent_request`/`acpx/agent_
/// response` envelope) instead of `InteractionHub` -- mirrors
/// `agent_request_relay_test.rs`'s `ws_client_answers_a_live_relayed_
/// permission_request`, with demux forced on. Pre-fix this fell straight
/// to the policy auto-answer too (`try_relay_agent_request` required
/// `LiveNotifyCtx::gateway_session_id` to already be `Some`, which
/// `spawn_demux_consumer`'s ctx never has).
#[tokio::test]
async fn native_ws_agent_request_relay_works_under_demux() {
    let mut router = Router::new("permission-agent-2");
    router.register_agent("permission-agent-2", chosen_option_echoing_permission_backend_spec());
    let router = router.with_process_reader_demux(true);
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;

    let (mut socket, _response) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("ws connect");

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        ws_send(
            &mut socket,
            json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}}),
        )
        .await;
        let new_reply = ws_receive(&mut socket).await;
        let gateway_id = new_reply["result"]["sessionId"].as_str().expect("sessionId").to_string();

        ws_send(
            &mut socket,
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
                "params": {"sessionId": gateway_id, "prompt": []}
            }),
        )
        .await;

        let mut ack_seen = false;
        loop {
            let frame = ws_receive(&mut socket).await;
            if frame.get("method").and_then(|m| m.as_str()) == Some("acpx/agent_request") {
                let relay_id = frame["params"]["relayId"].as_str().expect("relayId").to_string();
                let backend_request_id = frame["params"]["request"]["id"].clone();
                ws_send(
                    &mut socket,
                    json!({
                        "jsonrpc": "2.0", "id": 3, "method": "acpx/agent_response",
                        "params": {
                            "relayId": relay_id,
                            "response": {
                                "jsonrpc": "2.0",
                                "id": backend_request_id,
                                "result": {"outcome": {"outcome": "selected", "optionId": "allow-once"}}
                            }
                        }
                    }),
                )
                .await;
                continue;
            }
            if frame.get("id") == Some(&json!(3)) {
                assert_eq!(frame["result"]["delivered"], json!(true));
                ack_seen = true;
                continue;
            }
            if frame.get("id") == Some(&json!(2)) {
                assert!(ack_seen, "prompt result arrived before the relay ack");
                break frame;
            }
            panic!("unexpected frame while awaiting relay/prompt flow under demux: {frame:?}");
        }
    })
    .await
    .expect("relay flow under demux timed out -- see this test's own doc comment");

    assert_eq!(outcome["result"]["chosenOptionId"], json!("allow-once"));
}

/// A `session/prompt` backend that emits one `session/update` before
/// replying, no permission dance involved -- for the `POST /rpc`
/// buffering test below.
fn update_streaming_backend_spec() -> SpawnSpec {
    let script = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"session/new"'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-updates"}}\n' "$id"
  elif echo "$line" | grep -q '"method":"session/prompt"'; then
    printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"backend-updates","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"chunk-1"}}}}\n'
    sleep 0.3
    printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;
    SpawnSpec::new("sh", vec!["-c".to_string(), script.to_string()])
}

/// The `POST /rpc` data-loss regression: a pure-`reqwest`, no-WS/stdio-
/// ever-opened `session/prompt` caller (`transport::http`'s only
/// transport with zero live-push capability, see that module's own doc
/// comment) still sees a `session/update` the backend emitted mid-turn
/// in its own response's `_acpx.updates`, even though `process_reader_
/// demux` is on and the notification never touched this call's own read
/// loop at all -- proves `Router::pending_updates`'s buffer-and-drain
/// fallback actually closes the gap `process_reader_demux`'s field doc
/// comment used to cite as the reason the default stayed off.
#[tokio::test]
async fn http_only_caller_still_sees_updates_under_demux() {
    let mut router = Router::new("update-agent");
    router.register_agent("update-agent", update_streaming_backend_spec());
    let router = router.with_process_reader_demux(true);
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;

    let client = reqwest::Client::new();
    let new_reply: serde_json::Value = client
        .post(format!("http://{addr}/rpc"))
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}}))
        .send()
        .await
        .expect("session/new request")
        .json()
        .await
        .expect("session/new response body");
    let gateway_id = new_reply["result"]["sessionId"].as_str().expect("sessionId").to_string();

    let prompt_reply: serde_json::Value = client
        .post(format!("http://{addr}/rpc"))
        .json(&json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
            "params": {"sessionId": gateway_id, "prompt": []}
        }))
        .send()
        .await
        .expect("session/prompt request")
        .json()
        .await
        .expect("session/prompt response body");

    assert_eq!(prompt_reply["result"]["stopReason"], json!("end_turn"));
    let updates = prompt_reply["_acpx"]["updates"]
        .as_array()
        .expect("expected _acpx.updates to be populated from the pending-updates buffer");
    assert_eq!(updates.len(), 1, "unexpected _acpx.updates contents: {prompt_reply:?}");
    assert_eq!(
        updates[0]["params"]["sessionId"],
        json!(gateway_id),
        "buffered update must carry the client's gateway session id, not the backend-native one"
    );
    assert_eq!(
        updates[0]["update"]["sessionUpdate"].as_str().or_else(|| updates[0]["params"]["update"]["sessionUpdate"].as_str()),
        Some("agent_message_chunk"),
    );
}
