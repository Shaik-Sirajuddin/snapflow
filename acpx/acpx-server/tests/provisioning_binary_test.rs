//! Black-box coverage for `ACPX_CONFIG_FILE` (see `src/provisioning.rs`)
//! against the real, already-compiled `acpx-server` binary -- same
//! spawn-the-real-binary pattern as `binary_self_test.rs`, since
//! `provisioning.rs`'s own unit tests only prove `Router::dispatch`
//! wiring, never that `main.rs` actually reads `ACPX_CONFIG_FILE` and
//! calls it before either transport starts.

use std::io::Write as _;
use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::process::{Child, Command};

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

    let mut connected = false;
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            connected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(connected, "real binary never opened its HTTP listener");

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
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0]["name"], json!("work"));

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
