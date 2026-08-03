//! Opt-in wire-level ACP adapter coverage for the persistence/sync contract.
//!
//! This test intentionally does not seed `TranscriptStore` or use an in-process
//! router.  `ACPX_REAL_AGENT_COMMAND` must name an ACP stdio adapter (for
//! example `npx -y @agentclientprotocol/claude-agent-acp`).  The test starts
//! the shipped `acpx-server` binary, drives it over HTTP, and lets that server
//! spawn the configured adapter.  It is ignored because adapters require
//! credentials and may incur a model charge.

use reqwest::Client;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::{Child, Command};

async fn ephemeral_addr() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    addr
}

async fn wait_for_listener(addr: SocketAddr) {
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("acpx-server did not open {addr}");
}

struct ServerGuard {
    child: Child,
    db_path: std::path::PathBuf,
}

impl ServerGuard {
    /// Stop the server and reap it before the test removes its persistence
    /// files.  `start_kill` alone only schedules SIGKILL; dropping the guard
    /// immediately can leave a short-lived child/zombie and a background
    /// SQLite/keyring writer racing the cleanup path.
    async fn shutdown(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        self.remove_persistence_files();
    }

    fn remove_persistence_files(&self) {
        let _ = std::fs::remove_file(&self.db_path);
        let keyring_path = std::path::PathBuf::from(format!("{}.keyring", self.db_path.display()));
        let _ = std::fs::remove_file(keyring_path);
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        self.remove_persistence_files();
    }
}

async fn rpc(client: &Client, addr: SocketAddr, id: i64, method: &str, params: Value) -> Value {
    client
        .post(format!("http://{addr}/rpc"))
        .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{method} request failed: {e}"))
        .json()
        .await
        .unwrap_or_else(|e| panic!("{method} response was not JSON: {e}"))
}

fn contains_marker(value: &Value, needles: &[&str]) -> bool {
    let text = value.to_string().to_ascii_lowercase();
    needles.iter().all(|needle| text.contains(needle))
}

#[tokio::test]
#[ignore = "requires ACPX_REAL_AGENT_COMMAND and real adapter credentials"]
async fn real_agent_turn_tool_terminal_then_sync_has_zero_diff() {
    let command = match std::env::var("ACPX_REAL_AGENT_COMMAND") {
        Ok(command) if !command.trim().is_empty() => command,
        _ => {
            eprintln!("skipping: set ACPX_REAL_AGENT_COMMAND to an ACP stdio adapter");
            return;
        }
    };
    let addr = ephemeral_addr().await;
    let db_path = std::env::temp_dir().join(format!(
        "acpx-real-sync-{}-{}.sqlite",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let child = Command::new(env!("CARGO_BIN_EXE_acpx-server"))
        .env("ACPX_HTTP_BIND", addr.to_string())
        .env("ACPX_DEFAULT_ACP_COMMAND", &command)
        .env("ACPX_DB_PATH", &db_path)
        .env_remove("ACPX_AUTH_TOKEN")
        .stdin(Stdio::null())
        // This test only asserts the HTTP/ACP wire.  Piped output is never
        // drained and can deadlock a long-running real adapter once its logs
        // fill the OS pipe; discard it so the server lifetime is independent
        // of adapter verbosity.
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn acpx-server");
    let _server = ServerGuard {
        child,
        db_path: db_path.clone(),
    };
    wait_for_listener(addr).await;
    let client = Client::new();
    let created = rpc(
        &client,
        addr,
        1,
        "session/new",
        json!({"cwd":"/tmp","mcpServers":[],"permissionProfile":"agent_full_access"}),
    )
    .await;
    assert!(created.get("error").is_none(), "session/new: {created}");
    let session = created["result"]["sessionId"].as_str().unwrap().to_owned();
    let prompt = std::env::var("ACPX_REAL_AGENT_PROMPT")
        .unwrap_or_else(|_| "terminal ACPX_REAL_AGENT_TERMINAL ACPX_REAL_AGENT_DONE".into());
    let response = tokio::time::timeout(
        Duration::from_secs(180),
        rpc(
            &client,
            addr,
            2,
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"text","text":prompt}]}),
        ),
    )
    .await
    .expect("real prompt timed out");
    assert!(
        response.get("error").is_none(),
        "session/prompt: {response}"
    );

    let loaded = rpc(
        &client,
        addr,
        3,
        "session/load",
        json!({"sessionId":session,"cwd":"/tmp","mcpServers":[]}),
    )
    .await;
    assert!(loaded.get("error").is_none(), "session/load: {loaded}");
    let canonical = loaded["_acpx"]["updates"]
        .as_array()
        .expect("session/load updates");
    assert!(
        !canonical.is_empty(),
        "real adapter returned no transcript updates"
    );
    let canonical_value = Value::Array(canonical.clone());
    assert!(
        contains_marker(&canonical_value, &["acpx_real_agent_done"]),
        "missing assistant reply in live transcript: {canonical_value}"
    );
    assert!(
        contains_marker(&canonical_value, &["terminal", "acpx_real_agent_terminal"]),
        "real tool/terminal path was not observed: {canonical_value}"
    );
    // `session/load`'s `_acpx.updates` is the backend replay stream, while
    // the gateway transcript also contains the user prompt that caused the
    // turn.  The client cursor for `acpx/sessions/sync` is therefore the
    // authoritative server transcript length, obtained from paginate, not
    // merely the number of backend replay notifications.
    let page = rpc(
        &client,
        addr,
        31,
        "acpx/sessions/paginate",
        json!({"sessionId":session}),
    )
    .await;
    assert!(page.get("error").is_none(), "paginate: {page}");
    let known_message_count = page["messages"]
        .as_array()
        .expect("paginated transcript messages")
        .len();
    let sync = rpc(
        &client,
        addr,
        4,
        "acpx/sessions/sync",
        json!({"sessionId":session,"knownMessageCount":known_message_count}),
    )
    .await;
    assert!(sync.get("error").is_none(), "session/sync: {sync}");
    let sync_result = sync.get("result").unwrap_or(&sync);
    assert_eq!(
        sync_result["patch"]["replaceCount"], 0,
        "sync produced a diff: {sync}"
    );
    assert!(sync_result["patch"]["messages"]
        .as_array()
        .unwrap()
        .is_empty());
    let closed = rpc(
        &client,
        addr,
        5,
        "session/close",
        json!({"sessionId":session}),
    )
    .await;
    assert!(closed.get("error").is_none(), "session/close: {closed}");
    _server.shutdown().await;
}
