//! **True end-to-end proof for `config_hot_reload`** (phase 2 of
//! `memory/acpx/gen/acpx-concurrency-config-execution.meta.json`), spawning
//! the real, already-compiled `acpx-server` binary and driving it purely
//! from outside the process over real HTTP -- same "spawn the real binary"
//! pattern as `acp_bridge_binary_test.rs`/`process_reader_demux_concurrency_test.rs`.
//!
//! The gap this closes (`acpx-server/src/config.rs`'s `ServerConfig::
//! from_env`/`acpx-bridge`'s `BridgeConfig::from_env`, both boot-only
//! before this phase): editing the ACP bridge's model list required a
//! full process restart, which itself briefly drops every live backend
//! child process (`KillMode=control-group` under systemd) regardless of
//! `bg:true`. `BridgeRuntime::spawn_config_watcher` fixes this for the
//! bridge model-list config specifically (`ACPX_ACP_BRIDGE_CONFIG_FILE`) --
//! full `ServerConfig` hot-reload (auth tokens, isolation flags, lifecycle
//! timeouts) is out of scope for this phase, see the execution file.

use std::io::Write as _;
use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::process::{Child, Command};

const STAND_IN_AGENT_SCRIPT: &str = r#"
import sys, json, uuid

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
    except json.JSONDecodeError:
        continue
    rid = req.get("id")
    method = req.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": rid, "result": {
            "protocolVersion": 1,
            "agentCapabilities": {},
            "authMethods": [],
        }})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": rid, "result": {"sessionId": str(uuid.uuid4())}})
    elif method == "session/prompt":
        send({"jsonrpc": "2.0", "id": rid, "result": {"stopReason": "end_turn"}})
    elif method == "session/close":
        send({"jsonrpc": "2.0", "id": rid, "result": {}})
    else:
        send({"jsonrpc": "2.0", "id": rid, "result": {}})
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

fn bridge_config_json(default_model: &str) -> String {
    json!({
        "default_model": default_model,
        "models": [{
            "id": default_model,
            "agent_id": "default",
            "model_id": "native-model-id"
        }]
    })
    .to_string()
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

async fn get_models(client: &reqwest::Client, addr: SocketAddr) -> Value {
    client
        .get(format!("http://{addr}/acp/models"))
        .send()
        .await
        .expect("GET /acp/models")
        .json::<Value>()
        .await
        .expect("json body")
}

async fn rpc(client: &reqwest::Client, addr: SocketAddr, body: Value) -> Value {
    client
        .post(format!("http://{addr}/acp/rpc"))
        .json(&body)
        .send()
        .await
        .expect("POST /acp/rpc")
        .json::<Value>()
        .await
        .expect("json body")
}

/// Polls `GET /acp/models` until `defaultModel` matches `expected`, or
/// panics after `timeout` -- the watcher reacts to a real filesystem
/// event, not a fixed delay, so polling (rather than a single fixed
/// sleep) is what makes this test both fast in the common case and not
/// flaky under slow CI disk/inotify scheduling.
async fn wait_for_default_model(
    client: &reqwest::Client,
    addr: SocketAddr,
    expected: &str,
    timeout: Duration,
) -> Value {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let models = get_models(client, addr).await;
        if models["defaultModel"].as_str() == Some(expected) {
            return models;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "GET /acp/models never reflected defaultModel={expected:?} within {timeout:?}; \
                 last observed response: {models:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// **The core hot-reload proof.** Edits the live bridge config file on
/// disk while the real server is running and, with no restart, observes
/// `GET /acp/models` pick up the new `defaultModel`/model list. A session
/// bound before the edit must still work after it -- the reload must not
/// disturb anything session-scoped.
#[tokio::test]
async fn editing_the_bridge_config_file_hot_reloads_without_a_restart() {
    let addr = ephemeral_addr().await;
    let script_path = write_temp_file("acpx-hot-reload-stand-in-agent", STAND_IN_AGENT_SCRIPT);
    let bridge_config_path =
        write_temp_file("acpx-hot-reload-bridge-config", &bridge_config_json("stand-in/v1"));
    let db_path = write_temp_file("acpx-hot-reload-db", "");
    std::fs::remove_file(&db_path).expect("clear placeholder db file");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    cmd.env("ACPX_BACKEND_CMD", format!("python3 {}", script_path.display()))
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
    };
    wait_for_listener(addr).await;

    let client = reqwest::Client::new();

    let initial = get_models(&client, addr).await;
    assert_eq!(initial["defaultModel"], json!("stand-in/v1"));
    assert!(
        initial["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == json!("stand-in/v1")),
        "expected stand-in/v1 in the initial model list, got {initial:?}"
    );

    // Bind a real session against the pre-reload default model.
    let new_response = rpc(
        &client,
        addr,
        json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp", "mcpServers": []}}),
    )
    .await;
    let sid = new_response["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new returned no sessionId: {new_response:?}"))
        .to_string();
    let first_prompt = rpc(
        &client,
        addr,
        json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {"sessionId": sid, "prompt": []}}),
    )
    .await;
    assert_eq!(
        first_prompt["result"]["stopReason"],
        json!("end_turn"),
        "pre-reload prompt must succeed: {first_prompt:?}"
    );

    // The actual hot-reload: overwrite the config file in place with a
    // different default model and model list, no restart.
    std::fs::write(&bridge_config_path, bridge_config_json("stand-in/v2"))
        .expect("overwrite bridge config file");

    let reloaded = wait_for_default_model(&client, addr, "stand-in/v2", Duration::from_secs(5)).await;
    assert!(
        reloaded["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == json!("stand-in/v2")),
        "expected stand-in/v2 in the reloaded model list, got {reloaded:?}"
    );
    assert!(
        !reloaded["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == json!("stand-in/v1")),
        "expected stand-in/v1 to be gone after reload, got {reloaded:?}"
    );

    // No dropped sessions: the session bound *before* the reload must
    // still work *after* it.
    let post_reload_prompt = rpc(
        &client,
        addr,
        json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {"sessionId": sid, "prompt": []}}),
    )
    .await;
    assert_eq!(
        post_reload_prompt["result"]["stopReason"],
        json!("end_turn"),
        "a session bound before the config hot-reload must still work after it: {post_reload_prompt:?}"
    );

    drop(guard);
}

/// **The "reject and log, keep old config live" proof.** An edit that
/// fails `BridgeConfig::validate` (here: `default_model` not declared in
/// `models`) must be discarded, not applied -- the server keeps serving
/// the last valid config, and does not crash or hang.
#[tokio::test]
async fn an_invalid_config_edit_is_rejected_and_the_previous_config_stays_live() {
    let addr = ephemeral_addr().await;
    let script_path = write_temp_file("acpx-hot-reload-stand-in-agent", STAND_IN_AGENT_SCRIPT);
    let bridge_config_path =
        write_temp_file("acpx-hot-reload-bridge-config", &bridge_config_json("stand-in/v1"));
    let db_path = write_temp_file("acpx-hot-reload-db", "");
    std::fs::remove_file(&db_path).expect("clear placeholder db file");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    cmd.env("ACPX_BACKEND_CMD", format!("python3 {}", script_path.display()))
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
    };
    wait_for_listener(addr).await;

    let client = reqwest::Client::new();
    let initial = get_models(&client, addr).await;
    assert_eq!(initial["defaultModel"], json!("stand-in/v1"));

    // Invalid: default_model references a model id that isn't declared.
    let invalid = json!({
        "default_model": "stand-in/does-not-exist",
        "models": [{"id": "stand-in/v1", "agent_id": "default", "model_id": "native-model-id"}]
    })
    .to_string();
    std::fs::write(&bridge_config_path, invalid).expect("write invalid bridge config");

    // Give the watcher plenty of time to observe and reject the edit,
    // then confirm the server is still serving the last valid config --
    // not crashed, not hung, not silently applying the invalid candidate.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let still_live = get_models(&client, addr).await;
    assert_eq!(
        still_live["defaultModel"],
        json!("stand-in/v1"),
        "an invalid config edit must be rejected, leaving the previous valid config live: {still_live:?}"
    );

    // The server must still be fully responsive, not wedged by the
    // rejected reload attempt.
    let new_response = rpc(
        &client,
        addr,
        json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp", "mcpServers": []}}),
    )
    .await;
    assert!(
        new_response["result"]["sessionId"].as_str().is_some(),
        "server must remain fully responsive after rejecting an invalid config edit: {new_response:?}"
    );

    drop(guard);
}
