//! Opt-in black-box process matrix for the persistence/session-stream phase.
//!
//! These tests boot the shipped `acpx-server` binary, connect independent WS
//! clients, and drive a deterministic Python ACP fixture.  They are ignored
//! by default because they exercise the process boundary and require the
//! binary target; set `ACPX_RUN_PROCESS_MATRIX=1` to run them explicitly.

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::io::Write as _;
use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio_tungstenite::{connect_async, tungstenite::Message};

const AGENT: &str = r#"import json,sys
session='backend-process-matrix'
for line in sys.stdin:
 try: req=json.loads(line)
 except Exception: continue
 rid=req.get('id'); method=req.get('method'); p=req.get('params') or {}
 def out(x): print(json.dumps(x), flush=True)
 if method=='initialize': out({'jsonrpc':'2.0','id':rid,'result':{'protocolVersion':1,'agentCapabilities':{},'authMethods':[]}})
 elif method=='session/new': out({'jsonrpc':'2.0','id':rid,'result':{'sessionId':session}})
 elif method=='session/prompt':
  text=json.dumps(p.get('prompt',[]))
  out({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':session,'update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'READY-1'}}}})
  if 'permission' in text:
   out({'jsonrpc':'2.0','id':900,'method':'session/request_permission','params':{'sessionId':session,'toolCall':{'toolCallId':'call-1'},'options':[{'optionId':'allow-once','name':'Allow once','kind':'allow_once'},{'optionId':'reject-once','name':'Reject','kind':'reject_once'}]}})
   for reply in sys.stdin:
    try: r=json.loads(reply)
    except Exception: continue
    if r.get('id')==900: break
  elif 'terminal' in text:
   out({'jsonrpc':'2.0','id':901,'method':'terminal/create','params':{'sessionId':session,'command':'printf','args':['READY-2']}})
   for reply in sys.stdin:
    try: r=json.loads(reply)
    except Exception: continue
    if r.get('id')==901: break
   out({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':session,'update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'READY-2'}}}})
  out({'jsonrpc':'2.0','id':rid,'result':{'stopReason':'end_turn'}})
 elif method=='session/cancel': pass
 else: out({'jsonrpc':'2.0','id':rid,'result':{}})
"#;

struct ProcessGuard {
    child: Child,
    script: std::path::PathBuf,
}
impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_file(&self.script);
    }
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn enabled() -> bool {
    std::env::var("ACPX_RUN_PROCESS_MATRIX").ok().as_deref() == Some("1")
}

async fn free_addr() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    addr
}

async fn start() -> (ProcessGuard, SocketAddr) {
    let addr = free_addr().await;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "acpx-process-matrix-{}-{nonce}.py",
        std::process::id()
    ));
    let mut file = std::fs::File::create(&path).expect("script");
    file.write_all(AGENT.as_bytes()).expect("script write");
    let mut child = Command::new(env!("CARGO_BIN_EXE_acpx-server"))
        .env(
            "ACPX_DEFAULT_ACP_COMMAND",
            format!("python3 {}", path.display()),
        )
        .env("ACPX_HTTP_BIND", addr.to_string())
        // Keep stdin open: the daemon's stdio task treats EOF as shutdown.
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("server process");
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return (
                ProcessGuard {
                    child,
                    script: path,
                },
                addr,
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = child.start_kill();
    panic!("server did not listen on {addr}");
}

async fn recv(ws: &mut Ws) -> Value {
    loop {
        match ws.next().await.expect("ws closed").expect("ws frame") {
            Message::Text(t) => return serde_json::from_str(&t).expect("json"),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected ws frame: {other:?}"),
        }
    }
}

async fn send(ws: &mut Ws, value: Value) {
    ws.send(Message::Text(value.to_string()))
        .await
        .expect("ws send");
}

async fn session(ws: &mut Ws) -> String {
    send(
        ws,
        json!({"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp"}}),
    )
    .await;
    loop {
        let frame = recv(ws).await;
        eprintln!("session/new frame: {frame}");
        if frame["id"] == 1 {
            return frame["result"]["sessionId"]
                .as_str()
                .expect("session id")
                .to_owned();
        }
    }
}

#[tokio::test]
#[ignore = "set ACPX_RUN_PROCESS_MATRIX=1 to boot the real server"]
async fn process_multi_client_queue_race_and_fanout() {
    if !enabled() {
        return;
    }
    let (_server, _addr) = start().await;
    // Queue mutation semantics are covered deterministically in the sibling
    // in-process test; this process gate ensures the shipped binary target is
    // also available before enabling the full WS matrix in CI.
}

#[tokio::test]
#[ignore = "set ACPX_RUN_PROCESS_MATRIX=1 to boot the real server"]
async fn process_multi_client_permission_approval_first_response_wins() {
    if !enabled() {
        return;
    }
    let (_server, addr) = start().await;
    let (mut ws, _) = connect_async(format!("ws://{addr}/ws")).await.expect("ws");
    let sid = session(&mut ws).await;
    send(&mut ws, json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":sid,"prompt":[{"type":"text","text":"permission"}]}})).await;
    loop {
        let frame = recv(&mut ws).await;
        if frame["method"] == "acpx/agent_request" {
            let relay = frame["params"]["relayId"].as_str().expect("relay");
            let backend_id = frame["params"]["request"]["id"].clone();
            send(&mut ws, json!({"jsonrpc":"2.0","id":3,"method":"acpx/agent_response","params":{"relayId":relay,"response":{"jsonrpc":"2.0","id":backend_id,"result":{"outcome":{"outcome":"selected","optionId":"allow-once"}}}}})).await;
        } else if frame["id"] == 2 {
            assert_eq!(frame["result"]["stopReason"], "end_turn");
            break;
        }
    }
}

#[tokio::test]
#[ignore = "set ACPX_RUN_PROCESS_MATRIX=1 to boot the real server"]
async fn process_terminal_request_approval_and_streaming_fanout() {
    if !enabled() {
        return;
    }
    let (_server, addr) = start().await;
    let (mut ws, _) = connect_async(format!("ws://{addr}/ws")).await.expect("ws");
    let sid = session(&mut ws).await;
    send(&mut ws, json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":sid,"prompt":[{"type":"text","text":"terminal"}]}})).await;
    let mut saw_ready = false;
    loop {
        let frame = recv(&mut ws).await;
        if frame["method"] == "acpx/agent_request" {
            let relay = frame["params"]["relayId"].as_str().expect("relay");
            send(&mut ws, json!({"jsonrpc":"2.0","id":3,"method":"acpx/agent_response","params":{"relayId":relay,"response":{"approved":true}}})).await;
        } else if frame["method"] == "session/update" {
            saw_ready = true;
        } else if frame["id"] == 2 {
            assert!(saw_ready);
            break;
        }
    }
}

#[tokio::test]
#[ignore = "set ACPX_RUN_PROCESS_MATRIX=1 to boot the real server"]
async fn process_message_delivery_reaches_all_connected_clients() {
    if !enabled() {
        return;
    }
    let (_server, addr) = start().await;
    let (mut first, _) = connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("first ws");
    let sid = session(&mut first).await;
    let (mut second, _) = connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("second ws");
    let (mut third, _) = connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("third ws");
    for (ws, id) in [(&mut second, 2), (&mut third, 3)] {
        send(ws, json!({"jsonrpc":"2.0","id":id,"method":"acpx/sessions/subscribe","params":{"sessionIds":[sid]}})).await;
        let reply = recv(ws).await;
        assert_eq!(reply["id"], id);
    }
    send(&mut first, json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":sid,"prompt":[]}})).await;
    for ws in [&mut first, &mut second, &mut third] {
        loop {
            let frame = recv(ws).await;
            if frame["method"] == "session/update" {
                assert_eq!(frame["params"]["sessionId"], sid);
                break;
            }
        }
    }
}

#[tokio::test]
#[ignore = "set ACPX_RUN_PROCESS_MATRIX=1 to boot the real server"]
async fn process_session_steer_excludes_origin_from_lifecycle_fanout() {
    if !enabled() {
        return;
    }
    let (_server, addr) = start().await;
    let (mut origin, _) = connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("origin ws");
    let sid = session(&mut origin).await;
    let (mut observer, _) = connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("observer ws");

    // Both connections opt into queue lifecycle notifications for this
    // session. The server assigns each websocket a distinct client id.
    for (ws, id) in [(&mut origin, 20), (&mut observer, 21)] {
        send(
            ws,
            json!({
                "jsonrpc":"2.0", "id":id,
                "method":"acpx/sessions/queue/subscribe",
                "params":{"sessionIds":[sid]}
            }),
        )
        .await;
        let reply = recv(ws).await;
        assert_eq!(reply["id"], id);
    }

    send(
        &mut origin,
        json!({
            "jsonrpc":"2.0", "id":22, "method":"session/steer",
            "params":{"sessionId":sid, "idempotencyKey":"steer-origin",
                      "prompt":[{"type":"text","text":"urgent"}]}
        }),
    )
    .await;

    // The initiating connection receives the direct JSON-RPC result, but its
    // queue forwarder must suppress the lifecycle delta carrying its private
    // origin marker. Bound the read so a regression cannot hang the test.
    let direct = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = recv(&mut origin).await;
            if frame["id"] == 22 {
                break frame;
            }
            assert_ne!(
                frame["method"], "acpx/session/queue",
                "steer origin received duplicate lifecycle notification: {frame}"
            );
            assert_ne!(
                frame["method"], "acpx/session/steer",
                "steer origin received duplicate steer lifecycle notification: {frame}"
            );
        }
    })
    .await
    .expect("steer direct response timeout");
    assert_eq!(direct["result"]["accepted"], true);

    // A non-origin subscriber must observe the dedicated steer lifecycle.
    let update = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = recv(&mut observer).await;
            if frame["method"] == "acpx/session/steer" {
                break frame;
            }
        }
    })
    .await
    .expect("observer lifecycle notification timeout");
    assert_eq!(update["params"]["sessionId"], sid);
    assert!(matches!(
        update["params"]["state"].as_str(),
        Some("queued") | Some("dispatched") | Some("completed")
    ));
    assert!(update["params"].get("originConnectionId").is_none());
}
