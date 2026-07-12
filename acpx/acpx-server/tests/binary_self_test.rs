//! Black-box "self test": every other transport test in this crate
//! (`http_ws_transport_test.rs`, `e2e_single_agent_test.rs`) compiles
//! `acpx-server`'s own source files directly into the test binary via
//! `#[path]` (it's a bin-only crate with no `[lib]` target) and exercises
//! them in-process. That proves the *code* works, but never once boots
//! the actual, already-compiled `acpx-server` binary that gets shipped --
//! `main.rs`'s own `ServerConfig::from_env`, its concurrent
//! stdio-task/http-task `tokio::select!`, and the real OS process/TCP
//! boundary are never exercised by anything else in this workspace.
//!
//! This file closes that gap: it spawns the real binary via cargo's
//! `CARGO_BIN_EXE_acpx-server` (auto-populated by cargo for integration
//! tests in a crate that also builds a matching `[[bin]]`; cargo
//! guarantees the binary is built before any test binary that reads this
//! env var runs, so no manual build step is needed here), then drives it
//! purely from outside the process -- over its real stdin/stdout, a real
//! HTTP POST to its real TCP listener, and a real WebSocket upgrade --
//! exactly the way an operator, CI health check, or the `acpx-selftest`
//! CLI (`src/bin/selftest.rs`) would.
//!
//! Still uses the workspace's standard synthetic `sh -c '...'` stand-in
//! backend (see `acpx-core/tests/router_dispatch_test.rs`'s doc comment
//! for the pattern) so this doesn't depend on a real ACP adapter or API
//! key being available in this environment. `ACPX_BACKEND_CMD` is parsed
//! by naive whitespace-splitting in `src/config.rs` (`program` is the
//! first token, everything else is a separate arg), so the multi-line
//! stand-in script can't be inlined into the env var directly -- it's
//! written to a temp file first and referenced as `sh <path>`.

use std::io::Write as _;
use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Same stand-in backend script used throughout this workspace (see
/// `acpx-core/tests/router_dispatch_test.rs`): echoes a canned
/// `session/new` result with a fixed backend session id, or a generic
/// `{"ok": true}` result for anything else, always preserving the
/// request's own `id`.
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

/// Writes the stand-in script to a fresh temp file (unique per test via
/// pid + nanosecond timestamp, since this crate has no `tempfile`
/// dev-dependency and doesn't need one for this) and returns its path.
fn write_stand_in_script() -> std::path::PathBuf {
    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    );
    let path = std::env::temp_dir().join(format!("acpx-binary-self-test-{unique}.sh"));
    let mut file = std::fs::File::create(&path).expect("create stand-in script");
    file.write_all(STAND_IN_BACKEND_SCRIPT.as_bytes())
        .expect("write stand-in script");
    path
}

/// Probes an OS-assigned ephemeral port the same way
/// `http_ws_transport_test.rs::spawn_server` does, so the real child
/// process can be told a concrete `ACPX_HTTP_BIND` before it starts.
async fn ephemeral_addr() -> SocketAddr {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = probe.local_addr().expect("local_addr");
    drop(probe);
    addr
}

/// Owns the spawned `acpx-server` child process and its stand-in
/// backend script's temp file; kills the process and removes the temp
/// file on drop so a failing assertion never leaks either.
struct ServerGuard {
    child: Child,
    script_path: std::path::PathBuf,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_file(&self.script_path);
    }
}

/// Spawns the real, already-built `acpx-server` binary against the
/// stand-in backend and an ephemeral HTTP bind address, then polls until
/// its HTTP listener actually accepts a connection before returning --
/// this proves `main.rs`'s own startup path (config parsing, router
/// construction, concurrent stdio+HTTP tasks) really ran, not just that
/// the process forked.
async fn spawn_real_server(http_addr: SocketAddr) -> ServerGuard {
    let script_path = write_stand_in_script();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    cmd.env("ACPX_BACKEND_CMD", format!("sh {}", script_path.display()))
        .env("ACPX_HTTP_BIND", http_addr.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = cmd.spawn().expect("spawn real acpx-server binary");

    for _ in 0..100 {
        if tokio::net::TcpStream::connect(http_addr).await.is_ok() {
            return ServerGuard { child, script_path };
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("real acpx-server binary never opened its HTTP listener on {http_addr}");
}

#[tokio::test]
async fn real_binary_serves_http_rpc_end_to_end() {
    let addr = ephemeral_addr().await;
    let _server = spawn_real_server(addr).await;

    let client = reqwest::Client::new();

    // `session/list` on a freshly booted, real process: proves the whole
    // real startup path (config -> router -> HTTP transport) works, not
    // just that something is listening on the port.
    let list_response = client
        .post(format!("http://{addr}/rpc"))
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "session/list", "params": {}}))
        .send()
        .await
        .expect("POST /rpc session/list against the real binary");
    assert!(list_response.status().is_success());
    let list_body: Value = list_response.json().await.expect("json body");
    assert_eq!(list_body["jsonrpc"], json!("2.0"));
    assert_eq!(list_body["id"], json!(1));

    // Full session/new -> session/prompt -> session/close round trip,
    // proxied all the way through the real process to the real stand-in
    // backend subprocess it spawned itself (not one the test harness
    // spawned) -- the real `Router`/`acpx-conductor` supervising a real
    // child of a real child.
    let new_response = client
        .post(format!("http://{addr}/rpc"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .send()
        .await
        .expect("POST /rpc session/new");
    let new_body: Value = new_response.json().await.expect("json body");
    let gateway_session_id = new_body["result"]["sessionId"]
        .as_str()
        .expect("sessionId present")
        .to_string();
    // The client must never see the backend's own session id.
    assert_ne!(gateway_session_id, "backend-abc");

    let prompt_response = client
        .post(format!("http://{addr}/rpc"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {"sessionId": gateway_session_id, "prompt": [{"type": "text", "text": "hi"}]}
        }))
        .send()
        .await
        .expect("POST /rpc session/prompt");
    assert!(prompt_response.status().is_success());
    let prompt_body: Value = prompt_response.json().await.expect("json body");
    assert_eq!(prompt_body["result"], json!({"ok": true}));

    let close_response = client
        .post(format!("http://{addr}/rpc"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "session/close",
            "params": {"sessionId": gateway_session_id}
        }))
        .send()
        .await
        .expect("POST /rpc session/close");
    assert!(close_response.status().is_success());
}

#[tokio::test]
async fn real_binary_serves_websocket_end_to_end() {
    let addr = ephemeral_addr().await;
    let _server = spawn_real_server(addr).await;

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("connect to the real binary's WS endpoint");

    ws.send(WsMessage::Text(
        json!({"jsonrpc": "2.0", "id": 1, "method": "session/list", "params": {}}).to_string(),
    ))
    .await
    .expect("send over ws");

    let reply = ws
        .next()
        .await
        .expect("ws stream ended unexpectedly")
        .expect("ws message");
    let text = match reply {
        WsMessage::Text(t) => t,
        other => panic!("expected a text frame, got {other:?}"),
    };
    let body: Value = serde_json::from_str(&text).expect("parse ws json response");
    assert_eq!(body["jsonrpc"], json!("2.0"));
    assert_eq!(body["id"], json!(1));
}

#[tokio::test]
async fn real_binary_with_closed_stdin_still_serves_http() {
    // Regression test for a real startup bug found driving the
    // real-adapter e2e test (`real_claude_multi_agent_test.rs`) with a
    // `Stdio::null()` child: `main.rs` used to `tokio::select!` between
    // its stdio task and its HTTP task, so stdio hitting immediate EOF
    // (which is exactly what happens when stdin is `/dev/null`, e.g. any
    // daemonized/backgrounded/systemd-style launch, or this test) tore
    // down the *entire* process, HTTP/WS included, within milliseconds
    // of starting -- before it could ever accept a connection. Every
    // other test in this file avoids tripping this by keeping stdin
    // piped-and-open for the lifetime of the `Child` handle, which
    // masked the bug rather than covering it. This test deliberately
    // closes stdin up front and asserts the HTTP transport still comes
    // up and keeps serving requests well past the moment stdio would
    // have hit EOF.
    let script_path = write_stand_in_script();
    let addr = ephemeral_addr().await;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    cmd.env("ACPX_BACKEND_CMD", format!("sh {}", script_path.display()))
        .env("ACPX_HTTP_BIND", addr.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().expect("spawn real acpx-server binary");
    let _server = ServerGuard { child, script_path };

    let mut connected = false;
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            connected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        connected,
        "real acpx-server binary with closed stdin never opened its HTTP listener on {addr}"
    );

    // Give any latent stdio-EOF-triggered shutdown a real chance to fire
    // before proving the process is still alive and serving.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{addr}/rpc"))
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "session/list", "params": {}}))
        .send()
        .await
        .expect("POST /rpc session/list against the real binary after stdin EOF");
    assert!(response.status().is_success());
    let body: Value = response.json().await.expect("json body");
    assert_eq!(body["jsonrpc"], json!("2.0"));
}

#[tokio::test]
async fn real_binary_serves_stdio_end_to_end() {
    // A different ephemeral port than the other tests (each test spawns
    // its own process), even though this test only drives the process's
    // stdin/stdout -- `main.rs` always binds HTTP concurrently with
    // stdio, so a free port is still required for the process to start
    // successfully.
    let addr = ephemeral_addr().await;
    let mut server = spawn_real_server(addr).await;

    let mut stdin = server.child.stdin.take().expect("child stdin piped");
    let stdout = server.child.stdout.take().expect("child stdout piped");
    let mut stdout_lines = BufReader::new(stdout).lines();

    let request = json!({"jsonrpc": "2.0", "id": 1, "method": "session/list", "params": {}});
    stdin
        .write_all(format!("{request}\n").as_bytes())
        .await
        .expect("write request to real binary's stdin");
    stdin.flush().await.expect("flush stdin");

    let line = tokio::time::timeout(Duration::from_secs(5), stdout_lines.next_line())
        .await
        .expect("timed out waiting for stdio response from the real binary")
        .expect("read stdout line")
        .expect("child stdout closed before responding");
    let body: Value = serde_json::from_str(&line).expect("parse stdio json response");
    assert_eq!(body["jsonrpc"], json!("2.0"));
    assert_eq!(body["id"], json!(1));
}
