//! Phase 1's own gate ("A backend permission, FS, and terminal request
//! reaches only the owning WS client and receives exactly one valid
//! response") and the Phase 5 coverage matrix's "Permission / FS /
//! terminal approval" row ("owner/expiry/reconnect/foreign-client
//! rejection") both name a "foreign client" scenario that
//! `agent_request_relay_test.rs`'s own single-connection happy path
//! does not cover. This proves it directly against the real transport:
//!
//! `AgentRequestHub::relay` (see `acpx-core/src/agent_relay.rs`) only
//! ever pushes the live `acpx/agent_request` envelope to whichever *one*
//! connection is currently subscribed for the target gateway session --
//! a second, wholly unrelated connection that never touched that
//! session is never sent it at all, so the only way it could possibly
//! answer is by guessing/forging a `relayId` (`AgentRequestHub::
//! resolve` is a bare `relayId -> oneshot` lookup with no separate
//! per-connection ownership check -- see that module's doc comment).
//! This test is exactly that adversarial case: a second, never-
//! subscribed connection sends `acpx/agent_response` with a forged
//! `relayId` *while the real owner's relay is still pending*, and
//! asserts both that the forged attempt is rejected (`delivered:
//! false`, and it does not resolve or otherwise disturb the still-
//! pending relay) and that the real owner's own, subsequent answer
//! still lands correctly -- exactly one valid response, from the owner.
//!
//! Same duplicated-helpers convention as `agent_request_relay_test.rs`'s
//! own doc comment explains (independent `#[path]`-compiled test
//! binaries, no shared crate to put a common helper in).

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

/// Same stand-in permission backend as `agent_request_relay_test.rs`
/// (see that file's own doc comment for the exact protocol it plays).
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

type WsSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn read_json_frame(socket: &mut WsSocket) -> serde_json::Value {
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

#[tokio::test]
async fn a_foreign_connections_forged_relay_response_is_rejected_and_the_real_owner_still_answers()
{
    let mut router = Router::new("permission-agent");
    router.register_agent("permission-agent", stand_in_permission_backend_spec());
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router).await;

    // Connection A: the real owner. Creates the session and prompts it,
    // which subscribes it (per `transport::live::session_id_to_watch`)
    // for both live notifications and the interactive agent-request
    // relay -- see `agent_request_relay_test.rs`'s own doc comment for
    // why no separate subscribe step is needed.
    let (mut owner, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("owner ws connect");
    // Connection B: a second, wholly independent connection that never
    // touches this session at all -- the "foreign client".
    let (mut foreign, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("foreign ws connect");

    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        owner
            .send(WsMessage::Text(
                json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}})
                    .to_string(),
            ))
            .await
            .expect("send session/new");
        let new_reply = read_json_frame(&mut owner).await;
        let gateway_id = new_reply["result"]["sessionId"]
            .as_str()
            .expect("sessionId")
            .to_string();

        owner
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
            let frame = read_json_frame(&mut owner).await;
            if frame.get("method").and_then(|m| m.as_str()) == Some("acpx/agent_request") {
                let real_relay_id = frame["params"]["relayId"]
                    .as_str()
                    .expect("relayId")
                    .to_string();
                let backend_request_id = frame["params"]["request"]["id"].clone();

                // The foreign connection has no legitimate way to learn
                // `real_relay_id` (it was pushed only to the owner's own
                // socket) -- it forges an unrelated token and tries to
                // answer anyway, *before* the real owner gets a chance
                // to. If this were mistakenly accepted, the backend
                // would receive the foreign client's `reject-once`
                // instead of the owner's later `allow-once`, and/or the
                // owner's own later answer would be told "already
                // resolved" -- neither of which this test allows.
                foreign
                    .send(WsMessage::Text(
                        json!({
                            "jsonrpc": "2.0", "id": 100, "method": "acpx/agent_response",
                            "params": {
                                "relayId": "forged-not-a-real-relay-id",
                                "response": {
                                    "jsonrpc": "2.0",
                                    "id": backend_request_id,
                                    "result": {"outcome": {"outcome": "selected", "optionId": "reject-once"}}
                                }
                            }
                        })
                        .to_string(),
                    ))
                    .await
                    .expect("send forged acpx/agent_response");
                let forged_ack = read_json_frame(&mut foreign).await;
                assert_eq!(
                    forged_ack["result"]["delivered"],
                    json!(false),
                    "a forged relayId from a never-subscribed connection must not resolve anything"
                );

                // Now the real owner answers, using the real relayId it
                // alone received.
                owner
                    .send(WsMessage::Text(
                        json!({
                            "jsonrpc": "2.0", "id": 3, "method": "acpx/agent_response",
                            "params": {
                                "relayId": real_relay_id,
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
                    .expect("send real acpx/agent_response");
                continue;
            }
            if frame.get("id") == Some(&json!(3)) {
                assert_eq!(
                    frame["result"]["delivered"],
                    json!(true),
                    "the real owner's own answer must still be delivered"
                );
                ack_seen = true;
                continue;
            }
            if frame.get("id") == Some(&json!(2)) {
                assert!(ack_seen, "prompt result arrived before the owner's own relay ack");
                break frame;
            }
            panic!("unexpected frame on owner socket while awaiting relay/prompt flow: {frame:?}");
        }
    })
    .await
    .expect("relay flow timed out -- see this test's own doc comment");

    // The backend received the real owner's `allow-once`, never the
    // foreign connection's forged `reject-once` -- the one observable
    // signal that proves the forged response never reached the backend
    // at all, not merely that it lost a race.
    assert_eq!(outcome["result"]["chosenOptionId"], json!("allow-once"));
}
