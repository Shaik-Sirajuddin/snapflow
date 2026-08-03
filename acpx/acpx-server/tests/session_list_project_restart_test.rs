//! Real, two-launch-style integration proof for the gap `thread-stream-
//! persistence`'s plan left uncovered: no existing test spun up a real
//! `acpx-server` process, created multiple project-scoped sessions through
//! it, cleanly stopped that process, launched a *second*, genuinely new
//! process instance against the *same* durable storage, and confirmed
//! `session/list` against the new process still returns every session
//! created before the restart.
//!
//! `startup_recovery_test.rs` (queue/session-count-after-restart via the
//! admin plane) and `durable_secret_store_binary_test.rs` (secret-store
//! restart) already prove the underlying durable-recovery plumbing exists;
//! neither one drives the actual `session/list` JSON-RPC method a real
//! client calls to discover a project's prior threads after an app
//! restart, which is exactly panel-rust's cold-start shape (create N
//! threads per project via `session/new`, then rediscover them next
//! launch). This file closes that specific gap using the real compiled
//! `acpx-server` binary (`CARGO_BIN_EXE_acpx-server`), mirroring
//! `startup_recovery_test.rs`'s established real-subprocess pattern rather
//! than inventing a new one.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};

struct BinaryGuard {
    child: Option<Child>,
}

impl BinaryGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    /// Takes ownership of the wrapped process for an explicit, awaited
    /// clean stop (see `stop_cleanly`), leaving the guard's own `Drop`
    /// with nothing left to kill.
    fn take_child(&mut self) -> Child {
        self.child.take().expect("child already taken")
    }
}

impl Drop for BinaryGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

fn unique_temp_path(prefix: &str) -> PathBuf {
    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    );
    std::env::temp_dir().join(format!("{prefix}-{unique}"))
}

/// Same "read the id (string or number) out of the request line, reply
/// with a bare sessionId" shape `startup_recovery_test.rs`'s stand-in
/// backend uses -- must handle both the small integer ids a real
/// client-driven `session/new`/`session/list` call uses *and* the string
/// `"acpx-startup-recovery:<gateway_session_id>"` ids startup recovery's
/// own internal `session/load` replay uses when this exact binary
/// restarts and rehydrates the sessions created in this test's first
/// launch. Each reply's `sessionId` is namespaced with the current
/// timestamp+pid so two sessions created in the same launch never
/// collide, and so a *different* backend id doesn't accidentally look
/// like a stale one plausibly still alive if this file's assumptions
/// about backend-id uniqueness ever changed.
fn write_stand_in_backend_script(path: &std::path::Path) {
    std::fs::write(
        path,
        "#!/bin/sh\nwhile IFS= read -r line; do\n  \
         id=$(echo \"$line\" | sed -n 's/.*\"id\":\\(\"[^\"]*\"\\|[0-9][0-9]*\\).*/\\1/p')\n  \
         uniq=\"$(date +%s%N)-$$-$RANDOM\"\n  \
         printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"sessionId\":\"backend-%s\"}}\\n' \"$id\" \"$uniq\"\n\
         done\n",
    )
    .expect("write stand-in backend script");
}

fn free_local_addr() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
    let address = listener.local_addr().expect("probe address");
    drop(listener);
    address
}

async fn wait_for_ready(admin_address: std::net::SocketAddr, admin_token: &str) {
    let client = reqwest::Client::new();
    for _ in 0..200 {
        if let Ok(response) = client
            .get(format!(
                "http://{admin_address}/admin/sessions/count?tenant=default"
            ))
            .bearer_auth(admin_token)
            .send()
            .await
        {
            if response.status().is_success() {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("acpx-server admin transport never became ready at {admin_address}");
}

/// Spawns a real `acpx-server` process against the given durable database
/// and storage root, with a fresh stand-in ACP backend script (a fresh
/// script file per launch, matching `startup_recovery_test.rs`'s
/// convention, even though its contents are identical across launches).
fn spawn_server(
    database: &std::path::Path,
    storage_dir: &std::path::Path,
    client_address: std::net::SocketAddr,
    admin_address: std::net::SocketAddr,
    script_path: &std::path::Path,
) -> Child {
    write_stand_in_backend_script(script_path);
    let mut command = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    command
        .env(
            "ACPX_DEFAULT_ACP_COMMAND",
            format!("sh {}", script_path.display()),
        )
        .env("ACPX_DEFAULT_AGENT_ID", "codex-acp")
        .env("ACPX_HTTP_BIND", client_address.to_string())
        .env("ACPX_ADMIN_TOKEN", "admin-secret")
        .env("ACPX_ADMIN_BIND", admin_address.to_string())
        .env("ACPX_DB_PATH", database.display().to_string())
        .env("ACPX_STORAGE_DIR", storage_dir.display().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command.spawn().expect("spawn real acpx-server")
}

async fn rpc(
    client_address: std::net::SocketAddr,
    id: i64,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let response = reqwest::Client::new()
        .post(format!("http://{client_address}/rpc"))
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .send()
        .await
        .unwrap_or_else(|err| panic!("POST /rpc {method} failed: {err}"))
        .json::<serde_json::Value>()
        .await
        .unwrap_or_else(|err| panic!("parse /rpc {method} response: {err}"));
    assert!(
        response.get("error").is_none(),
        "{method} returned a JSON-RPC error: {response:?}"
    );
    response
}

/// Cleanly stops a process the way an app-quit/restart does -- graceful
/// termination, not a leftover lock-holding kill -- and waits for real
/// exit so the second launch never races the first one's hold on the
/// sqlite database file.
async fn stop_cleanly(mut child: Child) {
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}

/// **The gap this file closes.** Create several project-scoped sessions
/// through one real `acpx-server` launch, stop that process the way an
/// app quit/restart does, launch a genuinely new process instance
/// pointed at the same `ACPX_DB_PATH`/`ACPX_STORAGE_DIR`, and confirm
/// `session/list` against the *new* process still surfaces every session
/// created before the restart, each still carrying the project's `cwd` --
/// exactly the panel-rust cold-start shape of rediscovering a project's
/// prior threads next launch.
#[tokio::test]
async fn session_list_returns_project_scoped_sessions_after_a_real_server_restart() {
    let database = unique_temp_path("acpx-session-list-restart-test.sqlite");
    let storage_dir = unique_temp_path("acpx-session-list-restart-test-storage");
    std::fs::create_dir_all(&storage_dir).expect("create storage dir");
    let project_cwd = "/tmp/thread-stream-persistence-project-alpha";

    let admin_address = free_local_addr();
    let client_address = free_local_addr();
    let script_path = unique_temp_path("acpx-session-list-restart-backend-1.sh");

    let child = spawn_server(
        &database,
        &storage_dir,
        client_address,
        admin_address,
        &script_path,
    );
    let mut first_launch = BinaryGuard::new(child);
    wait_for_ready(admin_address, "admin-secret").await;

    // Mirror panel-rust's cold-start shape: create multiple threads/
    // sessions scoped to the same project path via session/new.
    let mut created_gateway_ids = Vec::new();
    for i in 0..3 {
        let response = rpc(
            client_address,
            i,
            "session/new",
            serde_json::json!({"cwd": project_cwd}),
        )
        .await;
        let gateway_id = response["result"]["sessionId"]
            .as_str()
            .expect("sessionId")
            .to_string();
        created_gateway_ids.push(gateway_id);
    }
    created_gateway_ids.sort();

    // Sanity: the first launch's own session/list already reflects all
    // three, each carrying the project cwd -- otherwise a restart-based
    // assertion below couldn't distinguish "recovery is broken" from
    // "session/new itself never worked".
    let pre_restart_list = rpc(client_address, 100, "session/list", serde_json::json!({})).await;
    let pre_restart_sessions = pre_restart_list["result"]["sessions"]
        .as_array()
        .expect("sessions array")
        .clone();
    assert_eq!(
        pre_restart_sessions.len(),
        3,
        "all 3 project-scoped sessions should be listed before any restart: \
         {pre_restart_sessions:?}"
    );
    for session in &pre_restart_sessions {
        assert_eq!(session["cwd"], serde_json::json!(project_cwd));
    }

    // Simulate app quit/restart: stop the first process cleanly, wait for
    // real exit (not just "kill signal sent") before starting the second
    // one against the same durable storage.
    stop_cleanly(first_launch.take_child()).await;

    // A genuinely new process instance, same ACPX_DB_PATH/ACPX_STORAGE_
    // DIR, same project path.
    let admin_address_2 = free_local_addr();
    let client_address_2 = free_local_addr();
    let script_path_2 = unique_temp_path("acpx-session-list-restart-backend-2.sh");
    let child_2 = spawn_server(
        &database,
        &storage_dir,
        client_address_2,
        admin_address_2,
        &script_path_2,
    );
    let _second_launch = BinaryGuard::new(child_2);
    wait_for_ready(admin_address_2, "admin-secret").await;

    // `session/list` has no project selector of its own (see `acpx-core`'s
    // `session_list_selector` -- only `_acpx.profile`/`_acpx.agentId`
    // exist); a client scopes to a project by filtering the gateway
    // aggregate's `cwd` field client-side, exactly like this assertion
    // does.
    let post_restart_list = rpc(client_address_2, 200, "session/list", serde_json::json!({})).await;
    let post_restart_sessions = post_restart_list["result"]["sessions"]
        .as_array()
        .expect("sessions array")
        .clone();
    let project_scoped_ids: Vec<String> = post_restart_sessions
        .iter()
        .filter(|session| session["cwd"] == serde_json::json!(project_cwd))
        .map(|session| {
            session["sessionId"]
                .as_str()
                .expect("sessionId")
                .to_string()
        })
        .collect();
    let mut project_scoped_ids_sorted = project_scoped_ids.clone();
    project_scoped_ids_sorted.sort();

    assert_eq!(
        project_scoped_ids_sorted, created_gateway_ids,
        "session/list against a fresh acpx-server process, pointed at the same \
         ACPX_DB_PATH/ACPX_STORAGE_DIR, must still surface every session created \
         before the restart, scoped to the same project cwd -- got {post_restart_sessions:?}"
    );

    let _ = std::fs::remove_file(&database);
    let _ = std::fs::remove_dir_all(&storage_dir);
}
