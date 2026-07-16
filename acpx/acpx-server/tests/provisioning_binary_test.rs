//! Black-box coverage for `ACPX_CONFIG_FILE` (see `src/provisioning.rs`)
//! against the real, already-compiled `acpx-server` binary -- same
//! spawn-the-real-binary pattern as `binary_self_test.rs`, since
//! `provisioning.rs`'s own unit tests only prove `Router::dispatch`
//! wiring, never that `main.rs` actually reads `ACPX_CONFIG_FILE` and
//! calls it before either transport starts.

use acpx_core::persistence::{
    sessions::{RecoveryMetadata, RecoveryMethod, RecoveryStatus},
    PersistenceStore,
};
use futures_util::{SinkExt, StreamExt};
use std::io::Write as _;
use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::process::{Child, Command};
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Same stand-in backend script used across this crate's tests (see
/// `binary_self_test.rs`'s doc comment) -- echoes a canned `session/new`
/// result (and the `initialize` handshake result, both share the generic
/// `{"ok": true}` fallback arm's shape via a fixed `sessionId`) so a
/// provisioned profile can actually complete a real `session/new`.
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

/// Records every backend call. The log path is the script's first argument
/// because `ACPX_BACKEND_CMD` only supports whitespace-separated arguments.
const RECOVERY_RECORDING_BACKEND_SCRIPT: &str = r#"
log_path=$1
fail_load_path=$2
while IFS= read -r line; do
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  printf '%s\t%s\n' "$method" "$line" >> "$log_path"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([^,}]*\).*/\1/p')
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  elif echo "$line" | grep -q 'session/load'; then
    if [ -n "$fail_load_path" ] && [ -e "$fail_load_path" ]; then
      exit 17
    fi
    printf '{"jsonrpc":"2.0","id":%s,"result":{"loaded":true}}\n' "$id"
  elif echo "$line" | grep -q 'session/prompt'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"prompted":true}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
  fi
done
"#;

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

async fn wait_for_log_line(log_path: &std::path::Path, prefix: &str) -> Vec<String> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(contents) = tokio::fs::read_to_string(log_path).await {
                let lines: Vec<String> = contents.lines().map(str::to_owned).collect();
                if lines.iter().any(|line| line.starts_with(prefix)) {
                    return lines;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("backend log never contained {prefix:?}"))
}

async fn seed_recoverable_session(db_path: &std::path::Path, profile_name: Option<&str>) {
    let store = PersistenceStore::open(db_path).expect("open sqlite persistence store");
    store
        .record_session_with_recovery(
            "gateway-recovered",
            "default",
            "backend-recovered",
            profile_name.map(str::to_owned),
            "2026-01-01T00:00:00Z",
            "default",
            RecoveryMetadata {
                cwd: Some("/workspace".to_string()),
                recovery_params: Some(json!({"cwd": "/workspace"})),
                status: RecoveryStatus::Active,
                recovery_method: RecoveryMethod::Load,
                last_recovery_error: None,
                ..RecoveryMetadata::default()
            },
        )
        .await
        .expect("seed recoverable session");
}

struct ServerGuard {
    child: Child,
    _script_path: std::path::PathBuf,
    _config_path: std::path::PathBuf,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

#[tokio::test]
async fn real_binary_applies_a_provisioning_file_at_startup() {
    let addr = ephemeral_addr().await;
    let script_path = write_temp_file("acpx-provisioning-backend", STAND_IN_BACKEND_SCRIPT);
    let config_path = write_temp_file(
        "acpx-provisioning-config",
        &json!({
            "providers": [{"name": "anthropic-default", "kind": "anthropic", "base_url": null}],
            "mcp_servers": [{"name": "fs", "command": "npx", "args": ["-y", "server-filesystem"]}],
            "profiles": [{
                "name": "work",
                "agent_id": "default",
                "provider": "anthropic-default",
                "mcp_servers": ["fs"]
            }]
        })
        .to_string(),
    );

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    cmd.env("ACPX_BACKEND_CMD", format!("sh {}", script_path.display()))
        .env("ACPX_HTTP_BIND", addr.to_string())
        .env("ACPX_CONFIG_FILE", config_path.display().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().expect("spawn real acpx-server binary");
    let mut guard = ServerGuard {
        child,
        _script_path: script_path,
        _config_path: config_path,
    };

    wait_for_listener(addr).await;

    let client = reqwest::Client::new();

    // profiles/list proves the provisioning file's profile was actually
    // created before the HTTP transport started accepting requests.
    let list = client
        .post(format!("http://{addr}/rpc"))
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "profiles/list", "params": {}}))
        .send()
        .await
        .expect("POST /rpc profiles/list")
        .json::<Value>()
        .await
        .expect("json body");
    let profiles = list["result"]["profiles"]
        .as_array()
        .expect("profiles array");
    // `profiles/list` now also includes auto-seeded profiles
    // (`ensure_default_profiles_seeded` -- one per `Installed` registry
    // agent, e.g. claude-acp/codex-acp/gemini in this environment)
    // alongside the provisioned one, so this asserts the provisioned
    // "work" profile is present rather than asserting the list's exact
    // length/order.
    assert!(
        profiles.iter().any(|p| p["name"] == json!("work")),
        "provisioned \"work\" profile missing from profiles/list: {profiles:?}"
    );

    // The provisioned profile is actually usable: session/new against it
    // resolves through to the real (test-spawned) stand-in backend.
    let session_new = client
        .post(format!("http://{addr}/rpc"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {"cwd": "/tmp", "_acpx": {"profile": "work"}}
        }))
        .send()
        .await
        .expect("POST /rpc session/new")
        .json::<Value>()
        .await
        .expect("json body");
    assert!(
        session_new["result"]["sessionId"].as_str().is_some(),
        "{session_new:?}"
    );

    let _ = guard.child.start_kill();
}

/// A malformed/rejecting provisioning file must fail the whole process's
/// startup, not boot a partially-configured or silently-unconfigured
/// gateway -- see `provisioning.rs`'s `apply` doc comment ("fails fast").
#[tokio::test]
async fn real_binary_refuses_to_start_with_an_invalid_provisioning_file() {
    let addr = ephemeral_addr().await;
    let script_path = write_temp_file("acpx-provisioning-backend", STAND_IN_BACKEND_SCRIPT);
    // Two profiles with the same name: the second `profiles/create`
    // dispatch fails with `AlreadyExists`, which `apply` propagates as a
    // hard error rather than skipping it.
    let config_path = write_temp_file(
        "acpx-provisioning-bad-config",
        &json!({
            "profiles": [
                {"name": "dup", "agent_id": "default"},
                {"name": "dup", "agent_id": "default"}
            ]
        })
        .to_string(),
    );

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    cmd.env("ACPX_BACKEND_CMD", format!("sh {}", script_path.display()))
        .env("ACPX_HTTP_BIND", addr.to_string())
        .env("ACPX_CONFIG_FILE", config_path.display().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().expect("spawn real acpx-server binary");

    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("process exited within 5s instead of hanging/serving")
        .expect("wait on child");
    assert!(
        !status.success(),
        "process should have exited non-zero on a bad provisioning file"
    );

    // Never opened the HTTP listener either -- provisioning runs before
    // either transport starts.
    assert!(
        tokio::net::TcpStream::connect(addr).await.is_err(),
        "HTTP listener should never have opened"
    );

    let _ = std::fs::remove_file(&script_path);
    let _ = std::fs::remove_file(&config_path);
}

/// Proactive recovery must finish before startup binds a client transport:
/// this starts a fresh compiled binary from a SQLite row and verifies the
/// stand-in backend sees `session/load` before this test sends its first
/// JSON-RPC request.
#[tokio::test]
async fn real_binary_recovers_open_sqlite_sessions_before_client_requests() {
    let addr = ephemeral_addr().await;
    let db_path = write_temp_file("acpx-startup-recovery-db", "");
    let log_path = write_temp_file("acpx-startup-recovery-log", "");
    let script_path = write_temp_file(
        "acpx-startup-recovery-backend",
        RECOVERY_RECORDING_BACKEND_SCRIPT,
    );
    let config_path = write_temp_file(
        "acpx-startup-recovery-config",
        &json!({
            "profiles": [{"name": "recovery", "agent_id": "default"}]
        })
        .to_string(),
    );

    let mut first_cmd = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    first_cmd
        .env(
            "ACPX_BACKEND_CMD",
            format!("sh {} {}", script_path.display(), log_path.display()),
        )
        .env("ACPX_HTTP_BIND", addr.to_string())
        .env("ACPX_DB_PATH", db_path.display().to_string())
        .env("ACPX_CONFIG_FILE", config_path.display().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let first_child = first_cmd
        .spawn()
        .expect("spawn first real acpx-server binary");
    let mut first_guard = ServerGuard {
        child: first_child,
        _script_path: script_path.clone(),
        _config_path: config_path.clone(),
    };

    wait_for_listener(addr).await;
    let session_new = reqwest::Client::new()
        .post(format!("http://{addr}/rpc"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/new",
            "params": {"cwd": "/workspace", "_acpx": {"profile": "recovery"}}
        }))
        .send()
        .await
        .expect("POST /rpc session/new")
        .json::<Value>()
        .await
        .expect("json body");
    let gateway_session_id = session_new["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new did not return a gateway session id: {session_new}"))
        .to_string();

    // The second process must recover solely from the SQLite row written by
    // this first real process, not from any inherited in-memory state.
    let _ = first_guard.child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(5), first_guard.child.wait())
        .await
        .expect("first server exited after kill");
    drop(first_guard);
    let _ = std::fs::remove_file(&log_path);

    let restart_addr = ephemeral_addr().await;
    let mut restart_cmd = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    restart_cmd
        .env(
            "ACPX_BACKEND_CMD",
            format!("sh {} {}", script_path.display(), log_path.display()),
        )
        .env("ACPX_HTTP_BIND", restart_addr.to_string())
        .env("ACPX_DB_PATH", db_path.display().to_string())
        .env("ACPX_CONFIG_FILE", config_path.display().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = restart_cmd
        .spawn()
        .expect("spawn restarted real acpx-server binary");
    let mut guard = ServerGuard {
        child,
        _script_path: script_path,
        _config_path: config_path,
    };

    // `wait_for_listener` cannot complete until recovery has completed,
    // because main calls recovery before binding the listener.
    wait_for_listener(restart_addr).await;
    let methods = wait_for_log_line(&log_path, "session/load\t").await;
    assert!(
        methods
            .iter()
            .any(|line| line.contains("\"sessionId\":\"backend-abc\"")),
        "startup recovery did not load the persisted backend session: {methods:?}"
    );

    let prompt = reqwest::Client::new()
        .post(format!("http://{restart_addr}/rpc"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/prompt",
            "params": {
                "sessionId": gateway_session_id,
                "prompt": [{"type": "text", "text": "after recovery"}]
            }
        }))
        .send()
        .await
        .expect("POST /rpc session/prompt")
        .json::<Value>()
        .await
        .expect("json body");
    assert_eq!(prompt["result"]["prompted"], true, "{prompt:?}");

    let methods = wait_for_log_line(&log_path, "session/prompt\t").await;
    let load_index = methods
        .iter()
        .position(|line| line.starts_with("session/load\t"))
        .expect("startup session/load");
    let prompt_index = methods
        .iter()
        .position(|line| line.starts_with("session/prompt\t"))
        .expect("client session/prompt");
    assert!(
        load_index < prompt_index,
        "startup recovery must load before any client prompt: {methods:?}"
    );

    let _ = guard.child.start_kill();
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(log_path);
}

#[tokio::test]
async fn real_binary_skips_startup_recovery_when_disabled() {
    let addr = ephemeral_addr().await;
    let db_path = write_temp_file("acpx-startup-recovery-disabled-db", "");
    let log_path = write_temp_file("acpx-startup-recovery-disabled-log", "");
    let script_path = write_temp_file(
        "acpx-startup-recovery-disabled-backend",
        RECOVERY_RECORDING_BACKEND_SCRIPT,
    );
    seed_recoverable_session(&db_path, None).await;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    cmd.env(
        "ACPX_BACKEND_CMD",
        format!("sh {} {}", script_path.display(), log_path.display()),
    )
    .env("ACPX_HTTP_BIND", addr.to_string())
    .env("ACPX_DB_PATH", db_path.display().to_string())
    .env("ACPX_STARTUP_SESSION_RECOVERY_ENABLED", "0")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    let child = cmd.spawn().expect("spawn real acpx-server binary");
    let mut guard = ServerGuard {
        child,
        _script_path: script_path,
        _config_path: std::path::PathBuf::new(),
    };

    wait_for_listener(addr).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let log_contents = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        !log_contents.contains("session/load\t"),
        "disabled recovery still called the backend: {log_contents:?}"
    );

    let prompt = reqwest::Client::new()
        .post(format!("http://{addr}/rpc"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/prompt",
            "params": {"sessionId": "gateway-recovered", "prompt": []}
        }))
        .send()
        .await
        .expect("POST /rpc session/prompt")
        .json::<Value>()
        .await
        .expect("json body");
    assert!(
        prompt.get("error").is_some(),
        "disabled recovery should leave the persisted session unloaded: {prompt:?}"
    );

    let _ = guard.child.start_kill();
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(log_path);
}

#[tokio::test]
async fn real_binary_recovered_session_accepts_websocket_prompt() {
    let addr = ephemeral_addr().await;
    let db_path = write_temp_file("acpx-recovery-ws-db", "");
    let log_path = write_temp_file("acpx-recovery-ws-log", "");
    let script_path = write_temp_file(
        "acpx-recovery-ws-backend",
        RECOVERY_RECORDING_BACKEND_SCRIPT,
    );
    seed_recoverable_session(&db_path, None).await;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    cmd.env(
        "ACPX_BACKEND_CMD",
        format!("sh {} {}", script_path.display(), log_path.display()),
    )
    .env("ACPX_HTTP_BIND", addr.to_string())
    .env("ACPX_DB_PATH", db_path.display().to_string())
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    let child = cmd.spawn().expect("spawn recovering real binary");
    let mut guard = ServerGuard {
        child,
        _script_path: script_path,
        _config_path: std::path::PathBuf::new(),
    };
    wait_for_listener(addr).await;

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("connect recovered websocket");
    ws.send(WsMessage::Text(
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/prompt",
            "params": {"sessionId": "gateway-recovered", "prompt": []}
        })
        .to_string(),
    ))
    .await
    .expect("send websocket prompt");
    let response = ws
        .next()
        .await
        .expect("websocket stayed open")
        .expect("websocket response");
    let text = match response {
        WsMessage::Text(text) => text,
        other => panic!("expected text response, got {other:?}"),
    };
    let body: Value = serde_json::from_str(&text).expect("parse websocket JSON");
    assert_eq!(body["result"]["prompted"], json!(true), "{body:?}");

    let methods = wait_for_log_line(&log_path, "session/prompt\t").await;
    let load_index = methods
        .iter()
        .position(|line| line.starts_with("session/load\t"))
        .expect("startup session/load");
    let prompt_index = methods
        .iter()
        .position(|line| line.starts_with("session/prompt\t"))
        .expect("websocket session/prompt");
    assert!(load_index < prompt_index, "{methods:?}");

    let _ = guard.child.start_kill();
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(log_path);
}

#[tokio::test]
async fn real_binary_survives_a_recovery_connector_outage() {
    let addr = ephemeral_addr().await;
    let db_path = write_temp_file("acpx-recovery-outage-db", "");
    let log_path = write_temp_file("acpx-recovery-outage-log", "");
    let fail_load_path = write_temp_file("acpx-recovery-outage-flag", "");
    let script_path = write_temp_file(
        "acpx-recovery-outage-backend",
        RECOVERY_RECORDING_BACKEND_SCRIPT,
    );
    seed_recoverable_session(&db_path, None).await;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    cmd.env(
        "ACPX_BACKEND_CMD",
        format!(
            "sh {} {} {}",
            script_path.display(),
            log_path.display(),
            fail_load_path.display()
        ),
    )
    .env("ACPX_HTTP_BIND", addr.to_string())
    .env("ACPX_DB_PATH", db_path.display().to_string())
    // The backend exits only while this sentinel exists. Removing it lets
    // the client retry native session/load against a replacement connector.
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    let child = cmd.spawn().expect("spawn recovering real binary");
    let mut guard = ServerGuard {
        child,
        _script_path: script_path,
        _config_path: std::path::PathBuf::new(),
    };
    wait_for_listener(addr).await;

    let client = reqwest::Client::new();
    let health = client
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .expect("GET /health")
        .json::<Value>()
        .await
        .expect("health JSON");
    assert_eq!(health["status"], json!("ready"), "{health:?}");
    assert_eq!(health["recovery"]["failed"], json!(1), "{health:?}");
    assert_eq!(health["recovery"]["restored"], json!(0), "{health:?}");

    let unavailable = client
        .post(format!("http://{addr}/rpc"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/prompt",
            "params": {"sessionId": "gateway-recovered", "prompt": []}
        }))
        .send()
        .await
        .expect("POST /rpc session/prompt")
        .json::<Value>()
        .await
        .expect("JSON-RPC response");
    assert!(
        unavailable["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("no session registered")),
        "failed recovery must not expose a live session: {unavailable:?}"
    );

    std::fs::remove_file(&fail_load_path).expect("remove outage sentinel");
    let recovered = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let response = client
                .post(format!("http://{addr}/rpc"))
                .json(&json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "session/load",
                    "params": {"sessionId": "gateway-recovered", "cwd": "/workspace"}
                }))
                .send()
                .await
                .expect("POST /rpc session/load")
                .json::<Value>()
                .await
                .expect("JSON-RPC response");
            if response["result"]["loaded"] == json!(true) {
                return response;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("connector never recovered from crash backoff");
    assert!(
        recovered["result"]["loaded"] == json!(true),
        "daemon must allow native recovery retry after connector backoff: {recovered:?}"
    );

    let prompt = client
        .post(format!("http://{addr}/rpc"))
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {"sessionId": "gateway-recovered", "prompt": []}
        }))
        .send()
        .await
        .expect("POST /rpc session/prompt after recovery retry")
        .json::<Value>()
        .await
        .expect("JSON-RPC response");
    assert_eq!(prompt["result"]["prompted"], json!(true), "{prompt:?}");

    let healed_health = client
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .expect("GET /health after retry")
        .json::<Value>()
        .await
        .expect("health JSON");
    assert_eq!(
        healed_health["recovery"]["failed"],
        json!(0),
        "{healed_health:?}"
    );
    assert_eq!(
        healed_health["recovery"]["restored"],
        json!(1),
        "{healed_health:?}"
    );

    let methods = wait_for_log_line(&log_path, "session/prompt\t").await;
    assert!(
        methods
            .iter()
            .any(|line| line.starts_with("session/load\t")),
        "recovery did not attempt the persisted session: {methods:?}"
    );

    let _ = guard.child.start_kill();
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(log_path);
}
