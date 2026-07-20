//! **True end-to-end coverage for the `BACKEND_HANDSHAKE_TIMEOUT` fix,**
//! spawning the real, already-compiled `acpx-server` binary and driving
//! it purely from outside the process -- a real TCP listener, a real
//! `POST /acp/rpc` -- same "spawn the real binary" pattern as
//! `binary_self_test.rs`/`provisioning_binary_test.rs`, not the
//! in-process `acp_bridge::dispatch` calls `acp_bridge.rs`'s own unit
//! tests make.
//!
//! Reproduces the exact live incident this investigation traced:
//! `POST /acp/rpc` `session/prompt` against a virtual bridge session
//! whose lazy-bind backend never answers ACP `initialize` used to return
//! `"bridge session binding is in progress; retry the request"`
//! forever, with no way for a real client to ever get unstuck short of
//! restarting the daemon. `router.rs`'s `BACKEND_HANDSHAKE_TIMEOUT` fix
//! bounds the handshake read; this file proves that bound is wired all
//! the way through the real process/HTTP boundary, at the real 30-second
//! production timeout (no test-only seam exists at this layer, so this
//! test genuinely waits it out).

use std::io::Write as _;
use std::net::SocketAddr;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::process::{Child, Command};

/// Silent stand-in backend: spawns, reads and discards every line
/// forever, never writes a single byte to stdout. A real `initialize`
/// request sent to it is therefore guaranteed to never be answered --
/// same idiom as `acpx-core/src/router.rs`'s own
/// `backend_handshake_timeout_kills_a_wedged_process_and_frees_the_lock`
/// unit test and `acp_bridge.rs`'s in-process bridge test, reused here
/// at the real-process layer.
const SILENT_BACKEND_SCRIPT: &str = "cat > /dev/null\n";

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
    _bridge_config_path: std::path::PathBuf,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// **The end-to-end reproduction + fix proof.** Spawns the real
/// `acpx-server` binary with the strict `/acp` bridge enabled and a
/// silent stand-in backend, then drives it exactly the way a real ACP
/// client (Zed) would over `POST /acp/rpc`:
///
/// 1. `session/new` -- lazy binding, resolves immediately (no backend
///    round trip yet).
/// 2. `session/prompt` -- triggers real lazy `bind()`, which blocks
///    inside the real `initialize` handshake against the silent
///    backend. Sent from a background task since this genuinely blocks
///    for up to `BACKEND_HANDSHAKE_TIMEOUT` (30s) real wall-clock time.
/// 3. While still in flight, a retry over a second real HTTP request
///    observes the documented `"bridge session binding is in progress;
///    retry the request"` error -- proving the live incident's first
///    symptom is still exactly reproducible.
/// 4. The original call is awaited to completion: it must resolve (not
///    hang past a generous bound comfortably above 30s) with a real
///    backend-handshake-timeout error, not silently succeed or hang.
/// 5. A second retry after that point must observe the distinct,
///    terminal `"bridge session binding previously failed; create a new
///    session"` -- never `BindingInProgress` again. This is the
///    end-to-end proof that a real client is no longer stuck: it now
///    gets an actionable instruction (start a new session) instead of a
///    "retry forever" livelock, over the real network/process boundary,
///    not just an in-process call.
#[tokio::test]
async fn real_binary_bridge_binding_unsticks_itself_after_the_backend_handshake_times_out() {
    let addr = ephemeral_addr().await;
    let script_path = write_temp_file("acpx-acp-bridge-silent-backend", SILENT_BACKEND_SCRIPT);
    let bridge_config_path = write_temp_file(
        "acpx-acp-bridge-config",
        &json!({
            "default_model": "stand-in/default",
            "models": [{
                "id": "stand-in/default",
                "agent_id": "default",
                "model_id": "default"
            }]
        })
        .to_string(),
    );
    // Fresh, per-test sqlite path -- this host may already run a real
    // `acpx-server` (systemd) with its own `ACPX_DB_PATH` inherited into
    // this test's environment; reusing it would make startup proactively
    // recover that service's real durable sessions (slow, and a false
    // dependency on unrelated state) and could contend for the same
    // sqlite file. `ACPX_AUTH_TOKEN` is removed for the same inherited-
    // environment reason: this test's `reqwest` client sends no bearer
    // token, so an inherited token would make every request 401.
    let db_path = write_temp_file("acpx-acp-bridge-db", "");
    std::fs::remove_file(&db_path).expect("clear placeholder db file");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    cmd.env("ACPX_BACKEND_CMD", format!("sh {}", script_path.display()))
        .env("ACPX_HTTP_BIND", addr.to_string())
        .env("ACPX_ACP_BRIDGE_ENABLED", "1")
        .env(
            "ACPX_ACP_BRIDGE_CONFIG_FILE",
            bridge_config_path.display().to_string(),
        )
        .env("ACPX_DB_PATH", db_path.display().to_string())
        .env_remove("ACPX_AUTH_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().expect("spawn real acpx-server binary");
    let guard = ServerGuard {
        child,
        _script_path: script_path,
        _bridge_config_path: bridge_config_path,
    };

    wait_for_listener(addr).await;

    let client = reqwest::Client::new();

    // `session/new` never touches the backend at all (lazy binding), so
    // this resolves immediately with a virtual session id.
    let new_response = client
        .post(format!("http://{addr}/acp/rpc"))
        .json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp", "mcpServers": []}
        }))
        .send()
        .await
        .expect("POST /acp/rpc session/new")
        .json::<Value>()
        .await
        .expect("json body");
    let sid = new_response["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new returned no sessionId: {new_response:?}"))
        .to_string();

    // Triggers real lazy bind() against the real silent backend process.
    // Spawned so this test can observe the in-flight BindingInProgress
    // window, then await this same handle for the eventual resolution.
    let bg_client = client.clone();
    let bg_addr = addr;
    let bg_sid = sid.clone();
    let first_prompt = tokio::spawn(async move {
        bg_client
            .post(format!("http://{bg_addr}/acp/rpc"))
            .json(&json!({
                "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
                "params": {"sessionId": bg_sid, "prompt": []}
            }))
            .send()
            .await
            .expect("POST /acp/rpc session/prompt")
            .json::<Value>()
            .await
            .expect("json body")
    });

    // Give the backend prompt time to actually claim Binding ownership
    // and reach the real, wedged `initialize` read.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !first_prompt.is_finished(),
        "the first session/prompt should still be blocked inside bind() at this point"
    );

    // The exact retry the error message instructs a real client to
    // make, over a real, separate HTTP request -- the live incident's
    // first observable symptom.
    let retry = client
        .post(format!("http://{addr}/acp/rpc"))
        .json(&json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": {"sessionId": sid, "prompt": []}
        }))
        .send()
        .await
        .expect("POST /acp/rpc session/prompt retry")
        .json::<Value>()
        .await
        .expect("json body");
    assert!(
        retry["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("binding is in progress")),
        "expected the documented 'retry the request' error over real HTTP, got {retry:?}"
    );

    // Await the original call's real resolution -- bounded comfortably
    // above the real 30s BACKEND_HANDSHAKE_TIMEOUT so a regression back
    // to an unbounded hang still fails this test deterministically
    // instead of wedging the suite.
    let started = Instant::now();
    let first_outcome = tokio::time::timeout(Duration::from_secs(50), first_prompt)
        .await
        .expect(
            "the original session/prompt over real HTTP must eventually resolve, not hang \
             forever -- this is the true end-to-end proof BACKEND_HANDSHAKE_TIMEOUT is wired \
             all the way through the real process/HTTP boundary",
        )
        .expect("spawned task must not panic");
    assert!(
        started.elapsed() >= Duration::from_secs(28),
        "the original call resolved suspiciously fast ({:?}) for a real 30s handshake \
         timeout -- verify it genuinely waited out BACKEND_HANDSHAKE_TIMEOUT",
        started.elapsed()
    );
    assert!(
        first_outcome["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("initialize") && m.contains("handshake")),
        "expected the original call to fail with the real backend handshake timeout error, \
         got {first_outcome:?}"
    );

    // The end-to-end proof the livelock is gone: a real client hitting
    // the real HTTP boundary now gets the distinct, terminal
    // "create a new session" error, never BindingInProgress again.
    let post_failure_retry = client
        .post(format!("http://{addr}/acp/rpc"))
        .json(&json!({
            "jsonrpc": "2.0", "id": 4, "method": "session/prompt",
            "params": {"sessionId": sid, "prompt": []}
        }))
        .send()
        .await
        .expect("POST /acp/rpc session/prompt after handshake timeout")
        .json::<Value>()
        .await
        .expect("json body");
    assert!(
        post_failure_retry["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("previously failed")),
        "expected the terminal 'create a new session' error over real HTTP, got \
         {post_failure_retry:?}"
    );

    drop(guard);
}
