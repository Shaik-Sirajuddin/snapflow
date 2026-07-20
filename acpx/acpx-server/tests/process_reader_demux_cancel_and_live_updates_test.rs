//! **Closes a named verification-bar gap in the original design**
//! (`memory/acpx/tasks/zed_integration.yaml` task 7, stage 4: "load-test
//! two concurrent sessions on one shared process actually overlap in wall
//! time, cancel + live notifications still correct under concurrency").
//! `process_reader_demux_concurrency_test.rs` covers the overlap half;
//! this file covers the other half: with `ACPX_PROCESS_READER_DEMUX=1`
//! and two sessions genuinely concurrent on one shared backend process,
//! (a) cancelling one session's in-flight turn resolves it promptly and
//! does not disturb the other session's own turn, and (b) the surviving
//! session's live `session/update` notifications keep flowing correctly
//! through the whole thing.
//!
//! Real, already-compiled `acpx-server` binary; a real WS connection for
//! the surviving session (native `/ws`, which auto-subscribes to
//! `session/update`s for any session created over it -- same pattern
//! `agent_request_fs_terminal_relay_test.rs` uses) so live delivery is
//! actually observed, not just inferred; real HTTP for the cancelled
//! session, including a real `session/cancel` call.

use std::io::Write as _;
use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::process::{Child, Command};
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Every `session/prompt` streams a `session/update` every ~0.25s while
/// "thinking" for `delay` seconds, and honors an async `session/cancel`
/// notification (checked once per tick, not instantly -- real adapters
/// aren't instant either) by resolving early with `stopReason:
/// "cancelled"` instead of running out the full delay.
const STAND_IN_AGENT_SCRIPT: &str = r#"
import asyncio, sys, json, uuid

delay = float(sys.argv[1]) if len(sys.argv) > 1 else 2.0
tick = 0.25
write_lock = asyncio.Lock()
cancelled = set()

async def send(obj):
    line = json.dumps(obj) + "\n"
    async with write_lock:
        sys.stdout.write(line)
        sys.stdout.flush()

async def run_prompt(rid, session_id):
    elapsed = 0.0
    while elapsed < delay:
        if session_id in cancelled:
            cancelled.discard(session_id)
            await send({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": "cancelled"}})
            return
        await send({
            "jsonrpc": "2.0", "method": "session/update",
            "params": {"sessionId": session_id, "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": f"tick-{elapsed:.2f}"},
            }},
        })
        await asyncio.sleep(tick)
        elapsed += tick
    await send({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": "end_turn"}})

async def handle(req):
    rid = req.get("id")
    method = req.get("method")
    params = req.get("params") or {}
    if method == "initialize":
        await send({"jsonrpc": "2.0", "id": rid, "result": {
            "protocolVersion": 1, "agentCapabilities": {}, "authMethods": [],
        }})
    elif method == "session/new":
        await send({"jsonrpc": "2.0", "id": rid, "result": {"sessionId": str(uuid.uuid4())}})
    elif method == "session/prompt":
        asyncio.create_task(run_prompt(rid, params.get("sessionId")))
    elif method == "session/cancel":
        cancelled.add(params.get("sessionId"))
    elif method == "session/close":
        await send({"jsonrpc": "2.0", "id": rid, "result": {}})
    else:
        await send({"jsonrpc": "2.0", "id": rid, "result": {}})

async def main():
    reader = asyncio.StreamReader()
    protocol = asyncio.StreamReaderProtocol(reader)
    loop = asyncio.get_event_loop()
    await loop.connect_read_pipe(lambda: protocol, sys.stdin)
    while True:
        line = await reader.readline()
        if not line:
            break
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError:
            continue
        asyncio.create_task(handle(req))

asyncio.run(main())
"#;

const TURN_DELAY_SECS: f64 = 2.0;

fn unique_suffix() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    )
}

fn write_temp_file(prefix: &str, contents: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("{prefix}-{}", unique_suffix()));
    let mut file = std::fs::File::create(&path).expect("create temp file");
    file.write_all(contents.as_bytes())
        .expect("write temp file");
    path
}

async fn ephemeral_addr() -> SocketAddr {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = probe.local_addr().expect("local_addr");
    drop(probe);
    addr
}

async fn wait_for_listener(addr: SocketAddr) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("real binary never opened its HTTP listener");
}

struct ServerGuard {
    child: Child,
    _script_path: std::path::PathBuf,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

async fn rpc(client: &reqwest::Client, addr: SocketAddr, body: Value) -> Value {
    client
        .post(format!("http://{addr}/rpc"))
        .json(&body)
        .send()
        .await
        .expect("POST /rpc")
        .json::<Value>()
        .await
        .expect("json body")
}

type WsSocket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn ws_send(socket: &mut WsSocket, body: Value) {
    socket
        .send(WsMessage::Text(body.to_string()))
        .await
        .expect("ws send");
}

async fn ws_recv(socket: &mut WsSocket) -> Value {
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

/// **The core proof.** Session A (bound to a live WS connection) and
/// session B (plain HTTP) share the same backend process
/// (`ACPX_PROCESS_READER_DEMUX=1`, default isolation flags). A's
/// `session/prompt` is issued first and runs the full turn duration,
/// streaming live `session/update`s over its WS connection the whole
/// time. While A is still mid-turn, B's own `session/prompt` is fired and
/// then cancelled via a real `session/cancel` call. Both must resolve
/// correctly and independently: B cancels promptly without waiting out
/// its full turn, and A is completely undisturbed -- same live updates,
/// same eventual `end_turn`, on schedule.
#[tokio::test]
async fn cancelling_one_session_does_not_disturb_a_concurrent_sessions_live_updates() {
    let addr = ephemeral_addr().await;
    let script_path = write_temp_file("acpx-cancel-live-stand-in-agent", STAND_IN_AGENT_SCRIPT);
    let db_path = write_temp_file("acpx-cancel-live-db", "");
    std::fs::remove_file(&db_path).expect("clear placeholder db file");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    cmd.env(
        "ACPX_BACKEND_CMD",
        format!("python3 {} {TURN_DELAY_SECS}", script_path.display()),
    )
    .env("ACPX_HTTP_BIND", addr.to_string())
    .env("ACPX_DB_PATH", db_path.display().to_string())
    .env("ACPX_PROCESS_READER_DEMUX", "1")
    .env_remove("ACPX_AUTH_TOKEN")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    let child = cmd.spawn().expect("spawn real acpx-server binary");
    let guard = ServerGuard {
        child,
        _script_path: script_path,
    };
    wait_for_listener(addr).await;

    let http = reqwest::Client::new();

    // Session A: created and prompted entirely over one WS connection, so
    // live session/update notifications arrive interleaved on the same
    // socket -- exactly how a real persistent client observes them.
    let (mut ws, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("ws connect");
    ws_send(
        &mut ws,
        json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp", "mcpServers": []}}),
    )
    .await;
    let a_new = ws_recv(&mut ws).await;
    let sid_a = a_new["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new returned no sessionId: {a_new:?}"))
        .to_string();
    ws_send(
        &mut ws,
        json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {"sessionId": sid_a, "prompt": []}}),
    )
    .await;

    // Session B: plain HTTP, created only after A's prompt is already
    // in flight so both genuinely overlap on the shared backend process.
    let new_b = rpc(
        &http,
        addr,
        json!({"jsonrpc": "2.0", "id": 3, "method": "session/new", "params": {"cwd": "/tmp", "mcpServers": []}}),
    )
    .await;
    let sid_b = new_b["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new returned no sessionId: {new_b:?}"))
        .to_string();
    let http_b = http.clone();
    let addr_b = addr;
    let sid_b_c = sid_b.clone();
    let b_prompt = tokio::spawn(async move {
        rpc(
            &http_b,
            addr_b,
            json!({"jsonrpc": "2.0", "id": 4, "method": "session/prompt", "params": {"sessionId": sid_b_c, "prompt": []}}),
        )
        .await
    });

    // Give B's prompt time to genuinely register/be in flight, then
    // cancel it -- well before its own TURN_DELAY_SECS would elapse.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!b_prompt.is_finished(), "B's prompt should still be in flight when cancelled");
    let cancel_started = tokio::time::Instant::now();
    let cancel_reply = rpc(
        &http,
        addr,
        json!({"jsonrpc": "2.0", "id": 5, "method": "session/cancel", "params": {"sessionId": sid_b}}),
    )
    .await;
    // session/cancel is a notification on the wire to the backend, but
    // acpx's own transports are all request/response-shaped, so some
    // reply always comes back for it too (see dispatch_session_cancel's
    // doc comment) -- just confirm it didn't error.
    assert!(cancel_reply.get("error").is_none(), "{cancel_reply:?}");

    let b_outcome = b_prompt.await.expect("b_prompt task must not panic");
    let cancel_to_resolution = cancel_started.elapsed();
    assert_eq!(
        b_outcome["result"]["stopReason"],
        json!("cancelled"),
        "B's prompt must resolve with stopReason=cancelled: {b_outcome:?}"
    );
    assert!(
        cancel_to_resolution < Duration::from_secs_f64(TURN_DELAY_SECS - 0.5),
        "B's cancelled prompt took {cancel_to_resolution:?} to resolve after cancel -- expected \
         well under the full {TURN_DELAY_SECS}s turn duration, proving cancellation actually \
         interrupted it rather than it just running to completion anyway"
    );

    // Drain A's WS socket: live session/update frames until its own
    // session/prompt (id 2) resolves. Must see real live updates, must
    // resolve end_turn on schedule, and must never see anything
    // referencing B's session -- A's turn is completely undisturbed by
    // B's concurrent cancellation.
    let a_started = tokio::time::Instant::now();
    let mut a_update_count = 0usize;
    let a_final = loop {
        let frame = ws_recv(&mut ws).await;
        if frame.get("id") == Some(&json!(2)) {
            break frame;
        }
        assert_eq!(
            frame["method"], json!("session/update"),
            "unexpected frame on A's socket while waiting for its prompt reply: {frame:?}"
        );
        assert_eq!(
            frame["params"]["sessionId"], json!(sid_a),
            "A's socket must never see a live update for another session: {frame:?}"
        );
        a_update_count += 1;
    };
    let a_elapsed = a_started.elapsed();
    assert_eq!(
        a_final["result"]["stopReason"],
        json!("end_turn"),
        "A's own turn must complete normally, undisturbed by B's cancellation: {a_final:?}"
    );
    assert!(
        a_update_count >= 2,
        "expected at least 2 live session/update frames for A while B was concurrently \
         cancelled, got {a_update_count}"
    );
    assert!(
        a_elapsed >= Duration::from_secs_f64(TURN_DELAY_SECS - 0.5),
        "A's turn resolved suspiciously fast ({a_elapsed:?}) for a real {TURN_DELAY_SECS}s \
         simulated turn -- verify it genuinely ran its own full duration rather than being cut \
         short by B's cancellation"
    );

    drop(guard);
}
