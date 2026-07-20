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
    // Measured from here, right after A's own prompt is actually sent --
    // not from some later point after B's whole create/prompt/cancel
    // dance completes. This assertion exists to prove A's turn ran its
    // full, real `TURN_DELAY_SECS` undisturbed by B's concurrent
    // cancellation; anchoring the clock to A's own request is what that
    // claim requires. A previous version of this test started the clock
    // only once the code reached the final drain loop below (after the
    // 400ms sleep, B's cancel round trip, and awaiting `b_prompt`), which
    // silently baked ~500ms of *unrelated* setup time into the assertion
    // and left as little as ~0ms of slack against its own `TURN_DELAY_
    // SECS - 0.5` threshold -- flaky by construction, independent of any
    // real backend behavior (reproduces on unmodified `main`, no router
    // changes involved, ~3 of 4 runs locally).
    let a_started = tokio::time::Instant::now();

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

/// Shared spawn helper for the two tests below: same real-binary,
/// real-stand-in-agent setup the core proof above uses, parameterized only
/// on whether `ACPX_PROCESS_READER_DEMUX` is set.
async fn spawn_server_with_demux(demux: Option<&str>) -> (ServerGuard, SocketAddr) {
    let addr = ephemeral_addr().await;
    let script_path = write_temp_file("acpx-notif-stall-stand-in-agent", STAND_IN_AGENT_SCRIPT);
    let db_path = write_temp_file("acpx-notif-stall-db", "");
    std::fs::remove_file(&db_path).expect("clear placeholder db file");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    cmd.env(
        "ACPX_BACKEND_CMD",
        format!("python3 {} {TURN_DELAY_SECS}", script_path.display()),
    )
    .env("ACPX_HTTP_BIND", addr.to_string())
    .env("ACPX_DB_PATH", db_path.display().to_string())
    .env_remove("ACPX_AUTH_TOKEN")
    .env_remove("ACPX_PROCESS_READER_DEMUX")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    if let Some(value) = demux {
        cmd.env("ACPX_PROCESS_READER_DEMUX", value);
    }
    let child = cmd.spawn().expect("spawn real acpx-server binary");
    let guard = ServerGuard {
        child,
        _script_path: script_path,
    };
    wait_for_listener(addr).await;
    (guard, addr)
}

/// **Pins the live production bug this session's user report described:**
/// "two sessions of the same agent launched, notifications aren't
/// delivered to Zed." Explicitly forces `ACPX_PROCESS_READER_DEMUX=0`
/// (the flag is now on by default -- see `acpx-server/src/config.rs` --
/// so this legacy-serialized behavior is opt-in, not the ambient
/// default, going forward) -- session A (plain
/// HTTP) gets its `session/prompt` genuinely in flight first, holding the
/// shared backend process's per-process lock for A's *entire* turn (the
/// pre-demux legacy behavior this whole phase's doc comments describe).
/// Session B then tries to open a live WS connection and merely call
/// `session/new` on the *same* shared agent -- with the flag off, B's
/// `session/new` itself cannot even resolve until A's whole turn is done,
/// so B receives literally nothing (not even a session id, let alone any
/// `session/update`) for the entire window. This is a strictly worse
/// symptom than "missed a live update" -- the second thread just sits
/// with no response at all, which is exactly Zed's reported
/// stuck-in-loading behavior for a second concurrent thread on one agent.
#[tokio::test]
async fn demux_off_a_second_sessions_launch_and_live_updates_stall_behind_first_sessions_turn() {
    let (guard, addr) = spawn_server_with_demux(Some("0")).await;
    let http = reqwest::Client::new();

    let new_a = rpc(
        &http,
        addr,
        json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp", "mcpServers": []}}),
    )
    .await;
    let sid_a = new_a["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new returned no sessionId: {new_a:?}"))
        .to_string();
    let http_a = http.clone();
    let addr_a = addr;
    let sid_a_c = sid_a.clone();
    let a_prompt = tokio::spawn(async move {
        rpc(
            &http_a,
            addr_a,
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {"sessionId": sid_a_c, "prompt": []}}),
        )
        .await
    });

    // Give A's turn time to genuinely register/hold the shared process's
    // lock before B ever tries to touch it.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!a_prompt.is_finished(), "A's prompt should still be in flight when B connects");

    let (mut ws_b, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("ws connect");
    let b_new_started = tokio::time::Instant::now();
    ws_send(
        &mut ws_b,
        json!({"jsonrpc": "2.0", "id": 3, "method": "session/new", "params": {"cwd": "/tmp", "mcpServers": []}}),
    )
    .await;
    let b_new = ws_recv(&mut ws_b).await;
    let b_new_elapsed = b_new_started.elapsed();
    let sid_b = b_new["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new returned no sessionId: {b_new:?}"))
        .to_string();

    assert!(
        b_new_elapsed >= Duration::from_secs_f64(TURN_DELAY_SECS - 0.5),
        "with process_reader_demux explicitly forced off, B's session/new resolved in \
         {b_new_elapsed:?} while A's turn was still in flight on the shared backend process -- \
         expected it to be blocked for close to A's full {TURN_DELAY_SECS}s turn, proving the \
         per-process lock is held across A's entire turn and starves B of any response \
         (session/new result, and by extension any session/update) for that whole window \
         when the flag is off"
    );

    // B is not permanently broken -- once A's lock is released, B's own
    // turn proceeds and its live updates flow normally over the same WS
    // connection that was stalled a moment ago.
    ws_send(
        &mut ws_b,
        json!({"jsonrpc": "2.0", "id": 4, "method": "session/prompt", "params": {"sessionId": sid_b, "prompt": []}}),
    )
    .await;
    let mut b_update_count = 0usize;
    loop {
        let frame = ws_recv(&mut ws_b).await;
        if frame.get("id") == Some(&json!(4)) {
            assert_eq!(frame["result"]["stopReason"], json!("end_turn"), "{frame:?}");
            break;
        }
        assert_eq!(frame["method"], json!("session/update"), "{frame:?}");
        b_update_count += 1;
    }
    assert!(
        b_update_count >= 1,
        "B should receive at least one live session/update once it finally gets its own turn"
    );

    a_prompt.await.expect("a_prompt task must not panic");
    drop(guard);
}

/// **The fix, proven from B's side.** Identical scenario to the test
/// above, `ACPX_PROCESS_READER_DEMUX=1` this time: B's `session/new` (and
/// then its live `session/update` stream) is not blocked behind A's
/// in-flight turn -- B gets an immediate session id and its own updates
/// arrive live over its WS connection well before A's turn finishes.
#[tokio::test]
async fn demux_on_a_second_sessions_launch_and_live_updates_do_not_stall_behind_first_sessions_turn(
) {
    let (guard, addr) = spawn_server_with_demux(Some("1")).await;
    let http = reqwest::Client::new();

    let new_a = rpc(
        &http,
        addr,
        json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp", "mcpServers": []}}),
    )
    .await;
    let sid_a = new_a["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new returned no sessionId: {new_a:?}"))
        .to_string();
    let http_a = http.clone();
    let addr_a = addr;
    let sid_a_c = sid_a.clone();
    let a_prompt = tokio::spawn(async move {
        rpc(
            &http_a,
            addr_a,
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {"sessionId": sid_a_c, "prompt": []}}),
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!a_prompt.is_finished(), "A's prompt should still be in flight when B connects");

    let (mut ws_b, _resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("ws connect");
    let b_new_started = tokio::time::Instant::now();
    ws_send(
        &mut ws_b,
        json!({"jsonrpc": "2.0", "id": 3, "method": "session/new", "params": {"cwd": "/tmp", "mcpServers": []}}),
    )
    .await;
    let b_new = ws_recv(&mut ws_b).await;
    let b_new_elapsed = b_new_started.elapsed();
    let sid_b = b_new["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new returned no sessionId: {b_new:?}"))
        .to_string();
    assert!(
        b_new_elapsed < Duration::from_secs(1),
        "B's session/new took {b_new_elapsed:?} with process_reader_demux ON while A's turn was \
         in flight on the same shared backend process -- expected sub-1s"
    );

    ws_send(
        &mut ws_b,
        json!({"jsonrpc": "2.0", "id": 4, "method": "session/prompt", "params": {"sessionId": sid_b, "prompt": []}}),
    )
    .await;
    let b_first_update_started = tokio::time::Instant::now();
    let first_b_frame = ws_recv(&mut ws_b).await;
    let b_first_update_elapsed = b_first_update_started.elapsed();
    assert_eq!(
        first_b_frame["method"], json!("session/update"),
        "expected B's first frame after its own prompt to be a live update, got {first_b_frame:?}"
    );
    assert!(
        b_first_update_elapsed < Duration::from_secs_f64(TURN_DELAY_SECS - 0.5),
        "B's first live session/update took {b_first_update_elapsed:?} to arrive while A was \
         still mid-turn on the shared process -- expected well under A's full \
         {TURN_DELAY_SECS}s turn, proving B's own updates aren't starved behind A's"
    );

    a_prompt.await.expect("a_prompt task must not panic");
    drop(guard);
}
