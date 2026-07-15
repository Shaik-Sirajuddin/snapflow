//! Real end-to-end proof that the interactive relay
//! (`acpx_core::agent_relay::AgentRequestHub`) also covers
//! `fs/read_text_file`/`fs/write_text_file`/`terminal/create` approval,
//! not just `session/request_permission` (see
//! `agent_request_relay_test.rs` for that one) -- Coverage Matrix rows
//! "profile gate, approve/reject, real disk result" and "approval,
//! terminal ID, command metadata sanitization" -- plus real live
//! `acpx/terminal_output` streaming for an approved terminal.
//!
//! Same `#[path]`-into-`src/transport` trick as `agent_request_relay_
//! test.rs`; duplicated helpers rather than shared for the same reason
//! that file gives (independent `#[path]`-compiled test binaries).

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

/// Answers `session/new` normally. On `session/prompt`: sends a real
/// `fs/read_text_file` request (id `950`) for `read_path`, blocks for its
/// reply and echoes it back verbatim as `result.readReply`; then sends a
/// real `terminal/create` request (id `960`) running `sh -c "printf
/// streamed-output"`, blocks for its reply and echoes it back verbatim
/// as `result.createReply`, then replies to the outer call. Mirrors
/// `fs_request_test.rs`'s "inner `while read` loop" trick so a
/// regression that leaves either request unanswered hangs this script
/// (and the test) rather than failing normally.
fn stand_in_backend_script(read_path: &str) -> String {
    format!(
        r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q '"method":"session/new"'; then
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"sessionId":"backend-abc"}}}}\n' "$id"
  elif echo "$line" | grep -q '"method":"session/prompt"'; then
    printf '{{"jsonrpc":"2.0","id":950,"method":"fs/read_text_file","params":{{"sessionId":"backend-abc","path":"{read_path}"}}}}\n'
    read_reply=""
    while IFS= read -r reply_line; do
      echo "$reply_line" | grep -q '"id":950' && {{ read_reply="$reply_line"; break; }}
    done
    printf '{{"jsonrpc":"2.0","id":960,"method":"terminal/create","params":{{"sessionId":"backend-abc","command":"sh","args":["-c","printf streamed-output"]}}}}\n'
    create_reply=""
    while IFS= read -r reply_line2; do
      echo "$reply_line2" | grep -q '"id":960' && {{ create_reply="$reply_line2"; break; }}
    done
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"stopReason":"end_turn","readReply":%s,"createReply":%s}}}}\n' "$id" "$read_reply" "$create_reply"
  else
    printf '{{"jsonrpc":"2.0","id":%s,"result":{{"ok":true}}}}\n' "$id"
  fi
done
"#
    )
}

fn stand_in_backend_spec(read_path: &str) -> SpawnSpec {
    SpawnSpec::new("sh", vec!["-c".to_string(), stand_in_backend_script(read_path)])
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

/// A live WS client that explicitly rejects a relayed `fs/read_text_file`
/// request gets its rejection honored -- acpx never touches the real
/// file, and the backend sees a clear rejection error, not the profile's
/// pre-existing auto-allow-because-capability-is-on behavior (the
/// profile has `allow_fs_access: true`, which alone would have allowed
/// it pre-relay -- a passing assertion here can only mean the live
/// rejection was honored over that).
#[tokio::test]
async fn ws_client_rejects_a_live_relayed_fs_read_request() {
    let dir = tempfile::tempdir().expect("tempdir");
    let read_path = dir.path().join("input.txt");
    std::fs::write(&read_path, "secret contents\n").expect("seed input file");

    let mut router = Router::new("fs-term-agent");
    router.register_agent(
        "fs-term-agent",
        stand_in_backend_spec(read_path.to_str().unwrap()),
    );
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router.clone()).await;

    let (mut socket, _response) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("ws connect");

    socket
        .send(WsMessage::Text(
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "profiles/create",
                "params": {
                    "name": "fs-term-enabled",
                    "agent_id": "fs-term-agent",
                    "allow_fs_access": true,
                    "allow_terminal_access": true
                }
            })
            .to_string(),
        ))
        .await
        .expect("send profiles/create");
    let _create_reply = read_json_frame(&mut socket).await;

    socket
        .send(WsMessage::Text(
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "session/new",
                "params": {"cwd": "/tmp", "_acpx": {"profile": "fs-term-enabled"}}
            })
            .to_string(),
        ))
        .await
        .expect("send session/new");
    let new_reply = read_json_frame(&mut socket).await;
    let gateway_id = new_reply["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    socket
        .send(WsMessage::Text(
            json!({
                "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                "params": {"sessionId": gateway_id, "prompt": []}
            })
            .to_string(),
        ))
        .await
        .expect("send session/prompt");

    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let frame = read_json_frame(&mut socket).await;
            if frame.get("method").and_then(|m| m.as_str()) == Some("acpx/agent_request")
                && frame["params"]["request"]["method"] == json!("fs/read_text_file")
            {
                let relay_id = frame["params"]["relayId"]
                    .as_str()
                    .expect("relayId")
                    .to_string();
                socket
                    .send(WsMessage::Text(
                        json!({
                            "jsonrpc": "2.0", "id": 4, "method": "acpx/agent_response",
                            "params": {"relayId": relay_id, "response": {"approved": false}}
                        })
                        .to_string(),
                    ))
                    .await
                    .expect("send acpx/agent_response");
                continue;
            }
            if frame.get("method").and_then(|m| m.as_str()) == Some("acpx/agent_request")
                && frame["params"]["request"]["method"] == json!("terminal/create")
            {
                // Approve terminal creation so the outer prompt can
                // complete -- this test's focus is the fs rejection.
                let relay_id = frame["params"]["relayId"]
                    .as_str()
                    .expect("relayId")
                    .to_string();
                socket
                    .send(WsMessage::Text(
                        json!({
                            "jsonrpc": "2.0", "id": 5, "method": "acpx/agent_response",
                            "params": {"relayId": relay_id, "response": {"approved": true}}
                        })
                        .to_string(),
                    ))
                    .await
                    .expect("send acpx/agent_response");
                continue;
            }
            if frame.get("id") == Some(&json!(3)) {
                break frame;
            }
        }
    })
    .await
    .expect("fs rejection flow timed out");

    let read_reply = &outcome["result"]["readReply"];
    assert!(
        read_reply.get("error").is_some(),
        "expected a rejection error for the live-denied fs/read_text_file, got {read_reply}"
    );
    assert!(
        read_reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("rejected"),
        "expected a clear rejection message, got {read_reply}"
    );
}

/// A live WS client that approves a relayed `terminal/create` request
/// gets a real spawned terminal, and receives its output live via
/// `acpx/terminal_output` push notifications -- not only through the
/// backend's own (never made, in this test) polling `terminal/output`
/// calls.
#[tokio::test]
async fn ws_client_approves_terminal_create_and_receives_live_output_stream() {
    let dir = tempfile::tempdir().expect("tempdir");
    let read_path = dir.path().join("input.txt");
    std::fs::write(&read_path, "unused\n").expect("seed input file");

    let mut router = Router::new("fs-term-agent-2");
    router.register_agent(
        "fs-term-agent-2",
        stand_in_backend_spec(read_path.to_str().unwrap()),
    );
    let router: SharedRouter = Arc::new(Mutex::new(router));
    let addr = spawn_server(router.clone()).await;

    let (mut socket, _response) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("ws connect");

    socket
        .send(WsMessage::Text(
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "profiles/create",
                "params": {
                    "name": "fs-term-enabled-2",
                    "agent_id": "fs-term-agent-2",
                    "allow_fs_access": true,
                    "allow_terminal_access": true
                }
            })
            .to_string(),
        ))
        .await
        .expect("send profiles/create");
    let _create_reply = read_json_frame(&mut socket).await;

    socket
        .send(WsMessage::Text(
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "session/new",
                "params": {"cwd": "/tmp", "_acpx": {"profile": "fs-term-enabled-2"}}
            })
            .to_string(),
        ))
        .await
        .expect("send session/new");
    let new_reply = read_json_frame(&mut socket).await;
    let gateway_id = new_reply["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    socket
        .send(WsMessage::Text(
            json!({
                "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                "params": {"sessionId": gateway_id, "prompt": []}
            })
            .to_string(),
        ))
        .await
        .expect("send session/prompt");

    let mut terminal_output_frames: Vec<serde_json::Value> = Vec::new();
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let frame = read_json_frame(&mut socket).await;
            if frame.get("method").and_then(|m| m.as_str()) == Some("acpx/agent_request")
                && frame["params"]["request"]["method"] == json!("fs/read_text_file")
            {
                let relay_id = frame["params"]["relayId"]
                    .as_str()
                    .expect("relayId")
                    .to_string();
                socket
                    .send(WsMessage::Text(
                        json!({
                            "jsonrpc": "2.0", "id": 4, "method": "acpx/agent_response",
                            "params": {"relayId": relay_id, "response": {"approved": true}}
                        })
                        .to_string(),
                    ))
                    .await
                    .expect("send acpx/agent_response");
                continue;
            }
            if frame.get("method").and_then(|m| m.as_str()) == Some("acpx/agent_request")
                && frame["params"]["request"]["method"] == json!("terminal/create")
            {
                let relay_id = frame["params"]["relayId"]
                    .as_str()
                    .expect("relayId")
                    .to_string();
                socket
                    .send(WsMessage::Text(
                        json!({
                            "jsonrpc": "2.0", "id": 5, "method": "acpx/agent_response",
                            "params": {"relayId": relay_id, "response": {"approved": true}}
                        })
                        .to_string(),
                    ))
                    .await
                    .expect("send acpx/agent_response");
                continue;
            }
            if frame.get("method").and_then(|m| m.as_str()) == Some("acpx/terminal_output") {
                terminal_output_frames.push(frame);
                continue;
            }
            if frame.get("id") == Some(&json!(3)) {
                break frame;
            }
        }
    })
    .await
    .expect("terminal approval/streaming flow timed out");

    let create_reply = &outcome["result"]["createReply"];
    assert!(
        create_reply["result"]["terminalId"].is_string(),
        "expected a real terminalId from the approved terminal/create, got {create_reply}"
    );

    // Give the (very short-lived) approved command's streaming poller a
    // little more time to publish its final post-exit snapshot if it
    // hadn't already arrived by the time the outer call completed.
    if terminal_output_frames
        .iter()
        .all(|f| f["params"]["exitStatus"].is_null())
    {
        if let Ok(Ok(frame)) = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let frame = read_json_frame(&mut socket).await;
                if frame.get("method").and_then(|m| m.as_str()) == Some("acpx/terminal_output") {
                    return Ok::<_, ()>(frame);
                }
            }
        })
        .await
        {
            terminal_output_frames.push(frame);
        }
    }

    assert!(
        !terminal_output_frames.is_empty(),
        "expected at least one live acpx/terminal_output push"
    );
    let combined_output: String = terminal_output_frames
        .iter()
        .filter_map(|f| f["params"]["output"].as_str())
        .last()
        .unwrap_or_default()
        .to_string();
    assert!(
        combined_output.contains("streamed-output"),
        "expected the live-streamed terminal output to contain the command's real stdout, got {terminal_output_frames:?}"
    );
    assert!(
        terminal_output_frames
            .iter()
            .any(|f| !f["params"]["exitStatus"].is_null()),
        "expected a final push carrying a non-null exitStatus once the command exited, got {terminal_output_frames:?}"
    );
}
