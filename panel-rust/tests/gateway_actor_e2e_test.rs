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

    let mut opener = spawn_acpx_thread(gateway.base_url.clone());
    let mut opener_events = opener.take_events();
    let session_id = opener
        .open_session(std::env::current_dir().unwrap())
        .await
        .expect("open_session");
    opener
        .send_prompt("history before relaunch")
        .await
        .expect("seed session history");
    assert!(
        wait_for_message_containing(
            &mut opener_events,
            "HISTORY BEFORE RELAUNCH",
            Duration::from_secs(10),
        )
        .await
        .is_some(),
        "seed prompt must finish before simulating a panel relaunch"
    );
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
async fn reattach_session_uses_resume_without_replaying_history() {
    let db_dir = tempfile::tempdir().expect("tempdir");
    let gateway = GatewayProcess::spawn("codex", &db_dir.path().join("acpx.sqlite3")).await;

    let opener = spawn_acpx_thread(gateway.base_url.clone());
    let session_id = opener
        .open_session(std::env::current_dir().unwrap())
        .await
        .expect("open_session");
    opener
        .send_prompt("history must stay cached locally")
        .await
        .expect("seed session history");
    opener.shutdown();

    let mut reattacher = spawn_acpx_thread(gateway.base_url.clone());
    let mut events_rx = reattacher.take_events();
    reattacher
        .reattach_session(session_id, std::env::current_dir().unwrap())
        .await
        .expect("session/resume");

    let replayed = tokio::time::timeout(Duration::from_millis(250), events_rx.recv()).await;
    assert!(
        !matches!(replayed, Ok(Some(AgentEvent::Message(_)))),
        "session/resume must not replay cached history: {replayed:?}"
    );

    reattacher
        .send_prompt("new turn after reattach")
        .await
        .expect("prompt after session/resume");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut reply = None;
    let mut observed = Vec::new();
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(Some(event)) =
            tokio::time::timeout(remaining.min(Duration::from_millis(250)), events_rx.recv()).await
        else {
            continue;
        };
        match &event {
            AgentEvent::Message(message) if message.text.contains("NEW TURN AFTER REATTACH") => {
                reply = Some(message.text.clone());
                break;
            }
            _ => observed.push(format!("{event:?}")),
        }
    }
    assert!(
        reply.is_some(),
        "reattached session did not emit its next prompt response; observed {observed:?}"
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

/// Coverage Matrix `session/close`, `session/delete` row -- real,
/// backend-forwarded ACP methods (see `acpx-core::router`'s own
/// `MethodClass::Proxied` classification and doc comment on both), not
/// gateway-native bookkeeping alone. Proven with the same "only the live
/// relay path could produce this evidence" technique the rest of this
/// project's real-process tests use: `rui-mock-agent` is told (via
/// `RUI_MOCK_AGENT_EVENT_LOG`) to append one JSON line per `session/
/// close`/`session/delete` request it actually receives on its own
/// stdio, tagged with the real session id -- if `close_session`/
/// `delete_session` had silently no-op'd locally (e.g. because
/// `session_id` was never captured on the actor, the real bug class
/// `Command::CloseSession`'s own "never opened" no-op branch exists to
/// distinguish from), this log would stay empty or short a line and the
/// test would fail, not pass by coincidence.
#[tokio::test]
async fn close_then_delete_session_round_trip_through_a_real_gateway() {
    let db_dir = tempfile::tempdir().expect("tempdir");
    let event_log = db_dir.path().join("backend-events.jsonl");
    let persona = "codex".to_string();
    let db_path = db_dir.path().join("acpx.sqlite3");
    let event_log_for_env = event_log.clone();
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
            .env("RUI_MOCK_AGENT_EVENT_LOG", &event_log_for_env)
            .env("RUST_LOG", "error");
    });
    let gateway = GatewayProcess { child, base_url };

    let handle = spawn_acpx_thread(gateway.base_url.clone());
    let session_id = handle
        .open_session(std::env::current_dir().unwrap())
        .await
        .expect("open_session");

    handle.close_session().await.expect("close_session");

    // `session/close` evicts the in-memory registry entry -- proven at
    // the `acpx-core::router` unit-test layer already
    // (`session_load_rehydration_test.rs`); here, proven through the
    // real gateway process: `session/list` must no longer report a
    // closed session as live.
    let checker = spawn_acpx_thread(gateway.base_url.clone());
    let sessions_after_close = checker.list_sessions().await.expect("list_sessions");
    assert!(
        !sessions_after_close
            .iter()
            .any(|s| s.acp_session_id == session_id),
        "closed session {session_id} must not be reported as live by session/list; got {sessions_after_close:?}"
    );

    handle.delete_session().await.expect("delete_session");

    let events = std::fs::read_to_string(&event_log).unwrap_or_default();
    // The backend log records *its own* native session id
    // (`mock-session-N`), not the gateway-issued `session_id` this test
    // otherwise deals in -- `acpx-core::router::translate_or_register_
    // backend_session` deliberately never leaks the gateway id to the
    // backend. This test only ever opens one session, so an unscoped
    // method-name match is unambiguous; a real regression (the relay
    // silently no-op'ing instead of reaching the backend) would leave
    // these lines entirely absent, not merely mis-attributed.
    let close_line = events.lines().find(|line| line.contains("\"session/close\""));
    let delete_line = events.lines().find(|line| line.contains("\"session/delete\""));
    assert!(
        close_line.is_some(),
        "expected a real session/close request to reach the backend for {session_id}; log:\n{events}"
    );
    assert!(
        delete_line.is_some(),
        "expected a real session/delete request to reach the backend for {session_id}; log:\n{events}"
    );
}

/// Coverage-matrix `session/cancel` row, host-scenario prerequisite: proves
/// the real, compiled `rui-mock-agent` binary itself (not the throwaway
/// bash stand-in `agent_bridge.rs`'s own cancel test uses) correctly
/// implements the `slow `-prefixed-prompt-blocks-until-`session/cancel`
/// contract the host XTEST Stop-button scenario needs. If `rui-mock-
/// agent`'s new cancel-notification handler never actually reached the
/// blocked prompt (e.g. the dispatch loop serialized instead of
/// concurrently running the notification handler while the prompt handler
/// was still pending -- a real risk this exact crate's own docs warn
/// about), this would hang until the prompt's own 20s safety-net timeout
/// and resolve with `"end_turn"` instead of `"cancelled"`, not the crate
/// panicking or erroring outright -- so this asserts the *reason string*
/// specifically, not merely that the turn eventually ended.
#[tokio::test]
async fn cancel_session_ends_a_real_mock_agent_slow_turn_as_cancelled() {
    let db_dir = tempfile::tempdir().expect("tempdir");
    let event_log = db_dir.path().join("backend-events.jsonl");
    let persona = "codex".to_string();
    let db_path = db_dir.path().join("acpx.sqlite3");
    let event_log_for_env = event_log.clone();
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
            .env("RUI_MOCK_AGENT_EVENT_LOG", &event_log_for_env)
            .env("RUST_LOG", "error");
    });
    let gateway = GatewayProcess { child, base_url };

    let mut handle = spawn_acpx_thread(gateway.base_url.clone());
    let mut events_rx = handle.take_events();
    handle
        .open_session(std::env::current_dir().unwrap())
        .await
        .expect("open_session");

    // Critical, easy to get wrong: `AcpxThreadHandle::send_prompt`'s own
    // doc comment says it "drain[s] the turn to completion" before
    // resolving -- it does not return once the request is merely
    // dispatched, unlike `AgentBridge::send_prompt` (the higher layer
    // `agent_bridge.rs`'s own slow-turn test uses, which really is
    // fire-and-forget). A first version of this test `.await`ed
    // `send_prompt` directly, which made it impossible to ever reach the
    // `cancel_session()` call while the prompt was still in flight --
    // the test still "worked" in the sense of eventually finishing (via
    // the mock agent's own 20s safety-net timeout), which is exactly the
    // kind of silent false pass this project's own established testing
    // discipline warns about; it was caught by asserting the *reason
    // string* specifically instead of merely "the turn ended eventually".
    // Fixed by polling the backend's own event log for real receipt of
    // the prompt and calling `cancel_session()` in an independent
    // concurrently-polled future via `tokio::join!`, so both genuinely
    // run at once on this one task instead of strictly sequentially.
    let prompt_fut = handle.send_prompt("slow cancel me");
    let coordinator_fut = async {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut prompt_seen = false;
        while std::time::Instant::now() < deadline && !prompt_seen {
            let events = std::fs::read_to_string(&event_log).unwrap_or_default();
            prompt_seen = events.lines().any(|line| line.contains("\"session/prompt\""));
            if !prompt_seen {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        assert!(
            prompt_seen,
            "mock agent never recorded receiving the slow session/prompt request"
        );
        handle.cancel_session().await.expect("cancel_session");
    };
    let (prompt_result, ()) = tokio::join!(prompt_fut, coordinator_fut);
    prompt_result.expect("send_prompt");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut stop_reason = None;
    while tokio::time::Instant::now() < deadline && stop_reason.is_none() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if let Ok(Some(AgentEvent::TurnEnded(reason))) =
            tokio::time::timeout(remaining.min(Duration::from_millis(200)), events_rx.recv()).await
        {
            stop_reason = Some(reason);
        }
    }
    assert_eq!(
        stop_reason.as_deref(),
        Some("cancelled"),
        "expected the slow turn to end with stopReason \"cancelled\" after cancel_session(), got {stop_reason:?}"
    );
}
