//! Real end-to-end coverage: spawns the actual compiled `acpx-server`
//! binary (not an in-process fake) with the actual compiled
//! `rui-mock-agent` binary as its backend, then drives it purely through
//! `rui-acpx-client`'s public API -- proving the full
//! panel-rust -> rui-acpx-client -> acpx-server -> backend-agent chain
//! round-trips for real, matching this project's established "spawn the
//! real binary, don't fake the boundary" testing discipline (see
//! `panel-rust`'s own headless smoke-test methodology).

use panel_rust::gateway_actor::spawn_acpx_thread;
use panel_rust::protocol_types::AgentEvent;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

/// Resolves the real, already-built `acpx-server` binary next to this
/// crate's own checkout -- mirrors `panel-rust/src/agent_bridge.rs`'s
/// `resolve_agent_command`'s dev-checkout-relative-path pattern.
fn acpx_server_bin() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../acpx/target/debug/acpx-server")
}

fn mock_agent_bin() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/debug/rui-mock-agent")
}

/// Binds an ephemeral TCP port synchronously (std, not tokio -- this
/// helper runs before any runtime is guaranteed up), then immediately
/// drops the listener so `acpx-server` can bind the same port itself.
/// Same "probe a free port, drop it, hand the number to the real process"
/// trick `acpx-server`'s own tests use.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local_addr").port()
}

/// Spawns a real `acpx-server` child, retrying the whole pick-port/
/// spawn/wait-for-connect cycle (bounded at 5 attempts) if the process
/// never becomes reachable within one attempt's own shorter window.
///
/// **Why this exists.** `free_port()`'s "bind a listener, read its
/// port, then drop it" trick has an unavoidable TOCTOU gap: a different
/// concurrently-running test's own `free_port()` call can claim the
/// exact same port before this function's spawned process binds it.
/// When that race is lost, `acpx-server` fails its bind and exits
/// immediately -- ported verbatim from `panel-rust::agent_bridge`'s
/// `spawn_acpx_server_with_retry` (see its doc comment for the full
/// root-cause writeup), whose fix this file's tests never picked up,
/// which is the confirmed cause of this file's own `resume_session_
/// replays_history_via_session_load` flake under parallel test load.
fn spawn_acpx_server_with_retry(
    configure: impl Fn(&mut Command, u16),
) -> (Child, String) {
    for attempt in 0..5 {
        let port = free_port();
        let mut command = Command::new(acpx_server_bin());
        configure(&mut command, port);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command.spawn().expect("spawn real acpx-server binary for test");

        let deadline = std::time::Instant::now() + Duration::from_millis(1500);
        let mut reachable = false;
        while std::time::Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                reachable = true;
                break;
            }
            if let Ok(Some(_status)) = child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        if reachable {
            return (child, format!("http://127.0.0.1:{port}"));
        }
        let _ = child.kill();
        let _ = child.wait();
        if attempt < 4 {
            std::thread::sleep(Duration::from_millis(50 * (attempt + 1)));
        }
    }
    panic!(
        "acpx-server never became reachable after 5 fresh-port attempts -- \
         this looks like more than ordinary port contention"
    );
}

struct GatewayProcess {
    child: Child,
    pub base_url: String,
}

impl GatewayProcess {
    /// Spawns a real `acpx-server` process with `persona` as both its
    /// `ACPX_DEFAULT_AGENT_ID` and the `RUI_MOCK_AGENT_PERSONA` its
    /// backend replies with -- the same shape
    /// `panel-rust::agent_bridge::ensure_gateway_running` uses in
    /// production, just parameterized for a test's own tempdir.
    async fn spawn(persona: &str, db_path: &std::path::Path) -> Self {
        let persona = persona.to_string();
        let db_path = db_path.to_path_buf();
        let (child, base_url) = spawn_acpx_server_with_retry(move |command, port| {
            command
                .env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"))
                .env(
                    "ACPX_BACKEND_CMD",
                    mock_agent_bin().to_string_lossy().to_string(),
                )
                .env("ACPX_DEFAULT_AGENT_ID", &persona)
                .env("ACPX_DB_PATH", &db_path)
                .env("RUI_MOCK_AGENT_PERSONA", &persona)
                .env("RUST_LOG", "error");
        });
        GatewayProcess { child, base_url }
    }
}

impl Drop for GatewayProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Waits (bounded) for a `Message` event whose text contains `needle`,
/// returning its full text. Shared by every test below instead of each
/// hand-rolling its own poll loop.
async fn wait_for_message_containing(
    rx: &mut UnboundedReceiver<AgentEvent>,
    needle: &str,
    timeout: Duration,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if let Ok(Some(AgentEvent::Message(msg))) =
            tokio::time::timeout(remaining.min(Duration::from_millis(200)), rx.recv()).await
        {
            if msg.text.contains(needle) {
                return Some(msg.text);
            }
        }
    }
    None
}

#[allow(dead_code)] // available for a future test that specifically asserts stop-reason tagging
async fn wait_for_turn_ended(rx: &mut UnboundedReceiver<AgentEvent>, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if let Ok(Some(AgentEvent::TurnEnded(_))) =
            tokio::time::timeout(remaining.min(Duration::from_millis(200)), rx.recv()).await
        {
            return true;
        }
    }
    false
}

#[tokio::test]
async fn open_session_prompt_and_turn_ended_round_trip_through_a_real_gateway() {
    let db_dir = tempfile::tempdir().expect("tempdir");
    let gateway = GatewayProcess::spawn("codex", &db_dir.path().join("acpx.sqlite3")).await;

    let mut handle = spawn_acpx_thread(gateway.base_url.clone());
    let mut events_rx = handle.take_events();
    let session_id = handle
        .open_session(std::env::current_dir().unwrap())
        .await
        .expect("open_session");
    assert!(!session_id.is_empty());

    handle
        .send_prompt("hello gateway")
        .await
        .expect("send_prompt");

    let reply =
        wait_for_message_containing(&mut events_rx, "HELLO GATEWAY", Duration::from_secs(10)).await;
    assert!(
        reply.is_some(),
        "expected the mock agent's uppercased reply to arrive via the gateway"
    );
}

#[tokio::test]
async fn resume_session_replays_history_via_session_load() {
    let db_dir = tempfile::tempdir().expect("tempdir");
    let gateway = GatewayProcess::spawn("codex", &db_dir.path().join("acpx.sqlite3")).await;

    let opener = spawn_acpx_thread(gateway.base_url.clone());
    let session_id = opener
        .open_session(std::env::current_dir().unwrap())
        .await
        .expect("open_session");
    opener
        .send_prompt("history before relaunch")
        .await
        .expect("seed session history");
    opener.shutdown();

    // Fresh handle/actor, same gateway -- proves resume works against a
    // brand new local `AcpxThreadHandle`, not just a long-lived one
    // (the real shape a relaunched panel process would hit).
    let mut resumer = spawn_acpx_thread(gateway.base_url.clone());
    let mut events_rx = resumer.take_events();
    resumer
        .resume_session(session_id, std::env::current_dir().unwrap())
        .await
        .expect("resume_session");

    let reply = wait_for_message_containing(
        &mut events_rx,
        "HISTORY BEFORE RELAUNCH",
        Duration::from_secs(10),
    )
    .await;
    assert!(
        reply.is_some(),
        "expected session/load's replayed-history reply via the gateway"
    );
}

#[tokio::test]
async fn resume_session_retries_after_transient_gateway_errors() {
    // This narrow transport regression uses a deterministic HTTP peer:
    // the first two session/load calls return JSON-RPC errors, then the
    // third succeeds. The real-gateway relaunch test above covers the
    // success path; this peer isolates the startup-race retry contract
    // without depending on the mock agent retaining native state across a
    // replacement backend process.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind transient peer");
    let port = listener.local_addr().expect("peer address").port();
    let peer = std::thread::spawn(move || {
        use std::io::{Read, Write};
        for attempt in 0..4 {
            let (mut stream, _) = listener.accept().expect("accept transient request");
            let mut request = [0u8; 8192];
            let _ = stream.read(&mut request);
            let body = if attempt < 2 {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": attempt + 1,
                    "error": {"code": -32603, "message": "gateway starting"}
                })
            } else if attempt == 2 {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": attempt + 1,
                    "result": {"modes": {"availableModes": [], "currentModeId": "default"}},
                    "_acpx": {"updates": []}
                })
            } else {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": attempt + 1,
                    "result": {"stopReason": "end_turn"},
                    "_acpx": {"updates": []}
                })
            }
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("write transient response");
        }
    });

    let handle = spawn_acpx_thread(format!("http://127.0.0.1:{port}"));
    handle
        .resume_session("persisted-session", std::env::current_dir().unwrap())
        .await
        .expect("resume_session should retry transient gateway errors");
    handle
        .send_prompt("continues after retry")
        .await
        .expect("send_prompt should use the resumed session");
    peer.join().expect("transient peer");
}

#[tokio::test]
async fn two_gateways_stay_isolated_no_cross_provider_bleed() {
    let db_dir = tempfile::tempdir().expect("tempdir");
    let codex_gateway = GatewayProcess::spawn("codex", &db_dir.path().join("codex.sqlite3")).await;
    let claude_gateway =
        GatewayProcess::spawn("claude", &db_dir.path().join("claude.sqlite3")).await;

    let mut codex_handle = spawn_acpx_thread(codex_gateway.base_url.clone());
    let mut claude_handle = spawn_acpx_thread(claude_gateway.base_url.clone());
    let mut codex_events = codex_handle.take_events();
    let mut claude_events = claude_handle.take_events();

    codex_handle
        .open_session(std::env::current_dir().unwrap())
        .await
        .expect("codex open_session");
    claude_handle
        .open_session(std::env::current_dir().unwrap())
        .await
        .expect("claude open_session");

    // Fire both prompts concurrently -- the negative-control shape this
    // plan's Phase 3 explicitly calls for: if the two gateways were ever
    // wired to the same backend/registry, one persona tag could bleed
    // into the other thread's transcript.
    let (codex_result, claude_result) = tokio::join!(
        codex_handle.send_prompt("which model are you"),
        claude_handle.send_prompt("which model are you"),
    );
    codex_result.expect("codex send_prompt");
    claude_result.expect("claude send_prompt");

    let codex_reply = wait_for_message_containing(
        &mut codex_events,
        "WHICH MODEL ARE YOU",
        Duration::from_secs(10),
    )
    .await
    .expect("codex reply within deadline");
    let claude_reply = wait_for_message_containing(
        &mut claude_events,
        "WHICH MODEL ARE YOU",
        Duration::from_secs(10),
    )
    .await
    .expect("claude reply within deadline");

    assert!(
        codex_reply.starts_with("[CODEX]"),
        "codex thread got: {codex_reply:?}"
    );
    assert!(
        claude_reply.starts_with("[CLAUDE]"),
        "claude thread got: {claude_reply:?}"
    );
    assert_ne!(codex_reply, claude_reply);
}

#[tokio::test]
async fn window_close_does_not_close_the_gateway_session() {
    // "shutdown()" is the local equivalent of the panel window/process
    // going away -- see `AcpxThreadHandle::shutdown`'s doc comment. It
    // must never send `session/close`. Verified here by shutting the
    // local handle down, then listing sessions *directly against the
    // gateway* via a brand new handle -- the session must still be
    // reported as live.
    let db_dir = tempfile::tempdir().expect("tempdir");
    let gateway = GatewayProcess::spawn("codex", &db_dir.path().join("acpx.sqlite3")).await;

    let handle = spawn_acpx_thread(gateway.base_url.clone());
    let session_id = handle
        .open_session(std::env::current_dir().unwrap())
        .await
        .expect("open_session");
    handle.shutdown(); // simulates window/process close -- no session/close sent
    drop(handle);

    // Give the (now-stopped) actor task a moment to actually exit, then
    // reconnect fresh and ask the gateway directly.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let checker = spawn_acpx_thread(gateway.base_url.clone());
    let sessions = checker.list_sessions().await.expect("list_sessions");
    assert!(
        sessions.iter().any(|s| s.acp_session_id == session_id),
        "session {session_id} should still be live after local handle shutdown (no session/close was sent); got {sessions:?}"
    );
}
