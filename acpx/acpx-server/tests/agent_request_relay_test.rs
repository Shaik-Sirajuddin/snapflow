//! Real end-to-end proof of the interactive agent-request relay
//! (`acpx_core::agent_relay::AgentRequestHub`, wired into
//! `transport::ws`): a WS-connected client that subscribed to a gateway
//! session gets a live `acpx/agent_request` push for a mid-turn
//! `session/request_permission` the stand-in backend sends, answers it
//! itself via `acpx/agent_response`, and the backend's own `session/
//! prompt` result reflects *that* answer -- not the profile's static
//! auto-answer policy, which this test deliberately picks a different
//! outcome from (`AutoReject`'s default would choose `reject-once`; the
//! WS client instead relays `allow-once`) so a passing assertion can only
//! mean the live relay path actually ran, not a coincidental fallback.
//!
//! Uses the same `#[path]`-into-`src/transport` trick as
//! `http_ws_transport_test.rs` (see that file's doc comment for why) and
//! the same synthetic stand-in backend shell-script trick as
//! `acpx-core/tests/permission_request_test.rs`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

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

use http::{serve, SharedRouter};

/// Answers `session/new` normally. On `session/prompt`, sends a
/// real-shaped `session/request_permission` request (id `999`) offering
/// both `allow-once` and `reject-once`, blocks on its own stdin for the
/// matching reply (same real dependency a real ACP adapter has), and
/// echoes back whichever `optionId` it was actually given as
/// `result.chosenOptionId` -- the one observable signal this test uses
/// to tell "the live WS relay answered" (`allow-once`) apart from "the
/// profile's static `AutoReject` fallback answered instead"
/// (`reject-once`, since a `reject_once`-kinded option is offered).
const STAND_IN_PERMISSION_BACKEND_SCRIPT: &str = r#"
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

fn stand_in_permission_backend_spec() -> SpawnSpec {
    SpawnSpec::new(
        "sh",
        vec![
            "-c".to_string(),
            STAND_IN_PERMISSION_BACKEND_SCRIPT.to_string(),
        ],
    )
}

/// Same ephemeral-port bind-then-serve helper as `http_ws_transport_
/// test.rs`; duplicated rather than shared because these are two
/// independent `#[path]`-compiled test binaries with no common crate to
/// put a shared helper in.
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

#[tokio::test]
async fn ws_client_answers_a_live_relayed_permission_request() {
    let mut router = Router::new("permission-agent");
    router.register_agent("permission-agent", stand_in_permission_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;

    let (mut socket, _response) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("ws connect");

    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        socket
            .send(WsMessage::Text(
                json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}})
                    .to_string(),
            ))
            .await
            .expect("send session/new");
        let new_reply = read_json_frame(&mut socket).await;
        let gateway_id = new_reply["result"]["sessionId"]
            .as_str()
            .expect("sessionId")
            .to_string();

        // Claims (subscribes) this connection for `gateway_id` -- both
        // `NotificationHub` and `AgentRequestHub` -- as a side effect of
        // `session/new`'s own response, per `transport::live::
        // session_id_to_watch`. No separate subscribe step needed.
        socket
            .send(WsMessage::Text(
                json!({
                    "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
                    "params": {"sessionId": gateway_id, "prompt": []}
                })
                .to_string(),
            ))
            .await
            .expect("send session/prompt");

        let mut ack_seen = false;
        loop {
            let frame = read_json_frame(&mut socket).await;
            if frame.get("method").and_then(|m| m.as_str()) == Some("acpx/agent_request") {
                let relay_id = frame["params"]["relayId"].as_str().expect("relayId").to_string();
                let backend_request_id = frame["params"]["request"]["id"].clone();
                assert_eq!(frame["params"]["sessionId"], json!(gateway_id));
                assert_eq!(
                    frame["params"]["request"]["method"],
                    json!("session/request_permission")
                );
                socket
                    .send(WsMessage::Text(
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
                        })
                        .to_string(),
                    ))
                    .await
                    .expect("send acpx/agent_response");
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
            panic!("unexpected frame while awaiting relay/prompt flow: {frame:?}");
        }
    })
    .await
    .expect("relay flow timed out -- see this test's own doc comment");

    // The relayed `allow-once` answer, not `AutoReject`'s `reject-once`
    // fallback, is what the backend actually received -- the one
    // assertion that proves the live WS relay path ran end to end.
    assert_eq!(outcome["result"]["chosenOptionId"], json!("allow-once"));
}

async fn read_json_frame(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> serde_json::Value {
    let frame = socket
        .next()
        .await
        .expect("ws stream ended early")
        .expect("ws frame error");
    match frame {
        WsMessage::Text(text) => serde_json::from_str(&text).expect("json frame"),
        other => panic!("expected text frame, got {other:?}"),
    }
}
