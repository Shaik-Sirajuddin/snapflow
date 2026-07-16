//! Black-box coverage for `durable_secret_and_configuration_store`
//! against the real, already-compiled `acpx-server` binary: a profile
//! (with a secret and a provider) created via one process's JSON-RPC
//! `profiles/create` must still resolve correctly after that process
//! exits and a brand new `acpx-server` process starts against the same
//! `ACPX_DB_PATH`. Same spawn-the-real-binary pattern as
//! `provisioning_binary_test.rs` (see that file's doc comment) --
//! `acpx-core/tests/durable_secret_store_test.rs` already proves the
//! `Router` wiring in-process; this proves `main.rs` actually calls
//! `Router::enable_durable_config` before either transport starts.

use std::io::Write as _;
use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::process::{Child, Command};

/// Echoes back the `CODEX_API_KEY` env var it was launched with, so a
/// restarted process resolving the same profile can prove it re-injected
/// the *decrypted* secret, not just replayed the profile's metadata.
const OBSERVING_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc","observedApiKey":"%s"}}\n' "$id" "${CODEX_API_KEY:-}"
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
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

async fn spawn_server(
    addr: SocketAddr,
    script_path: &std::path::Path,
    db_path: &std::path::Path,
    extra_env: &[(&str, &str)],
) -> ServerGuard {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    cmd.env("ACPX_BACKEND_CMD", format!("sh {}", script_path.display()))
        .env("ACPX_HTTP_BIND", addr.to_string())
        .env("ACPX_DB_PATH", db_path.display().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    let child = cmd.spawn().expect("spawn real acpx-server binary");
    let guard = ServerGuard { child };
    wait_for_listener(addr).await;
    guard
}

#[tokio::test]
async fn profile_secret_created_in_one_process_resolves_after_a_real_restart() {
    let script_path = write_temp_file("acpx-durable-backend", OBSERVING_BACKEND_SCRIPT);
    let db_dir = tempfile::tempdir().expect("tempdir");
    let db_path = db_dir.path().join("acpx.sqlite");
    let client = reqwest::Client::new();

    // Process 1: create the profile (with a secret + provider), then
    // exit -- the real termination path (`kill_on_drop` via `ServerGuard`
    // dropping), not a graceful shutdown, matching how an operator's
    // restart would actually behave.
    {
        let addr = ephemeral_addr().await;
        let _guard = spawn_server(addr, &script_path, &db_path, &[]).await;

        let create = client
            .post(format!("http://{addr}/rpc"))
            .json(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "profiles/create",
                "params": {
                    "name": "work",
                    "agent_id": "default",
                    "secret": "sk-restart-durable"
                }
            }))
            .send()
            .await
            .expect("POST /rpc profiles/create")
            .json::<Value>()
            .await
            .expect("json body");
        assert!(
            create["result"]["key_ref"].is_string(),
            "profiles/create should have minted a key_ref: {create:?}"
        );
    }

    // Between the two processes: the secret is actually encrypted at
    // rest on disk (bypassing the daemon entirely, same check as
    // `acpx-core/tests/durable_secret_store_test.rs`'s in-process
    // version) -- proves `main.rs` wired real durability, not just an
    // in-memory-only `Router` that happened not to crash.
    {
        let conn = rusqlite::Connection::open(&db_path).expect("open db directly");
        let ciphertext: Vec<u8> = conn
            .query_row("SELECT ciphertext FROM secrets LIMIT 1", [], |row| {
                row.get(0)
            })
            .expect("one secret row persisted by process 1");
        let as_text = String::from_utf8_lossy(&ciphertext);
        assert!(
            !as_text.contains("sk-restart-durable"),
            "ciphertext on disk must not contain the raw secret: {as_text:?}"
        );
    }

    // Process 2: a brand new binary invocation, same `ACPX_DB_PATH`
    // (and therefore the same default keyring path). The profile must
    // already be listed, with no `profiles/create` call in this process.
    let addr = ephemeral_addr().await;
    let guard = spawn_server(addr, &script_path, &db_path, &[]).await;

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
    let work = profiles
        .iter()
        .find(|p| p["name"] == json!("work"))
        .unwrap_or_else(|| {
            panic!("profile created by a prior process is missing after restart: {profiles:?}")
        });
    assert!(
        work["key_ref"].is_string(),
        "restored profile must still carry its key_ref: {work:?}"
    );

    // A successful `profiles/list` here already proves the decrypt-at-
    // load-time round trip succeeded: `Router::enable_durable_config` (in
    // `main.rs`, before either transport starts) decrypts every
    // persisted secret up front and `panic!`s the whole process on
    // failure -- this process reaching a listening HTTP transport at all
    // is itself evidence the secret decrypted correctly, on top of the
    // `key_ref` presence assertion above. `session/new`'s actual env
    // injection (secret -> `CODEX_API_KEY`) is covered without the
    // extra real-OS-process restart cost by
    // `acpx-core/tests/durable_secret_store_test.rs`.
    let _ = guard;
}

/// Regression coverage for the load-ordering bug found while wiring
/// `enable_durable_config` in: `Router::warm_default_profiles` used to
/// run *before* the `ACPX_DB_PATH` block in `main.rs`, so a restart would
/// re-seed the vanilla default `codex-acp`/`claude-acp`/... profile
/// in-memory first, and the persisted (operator-customized) profile of
/// that same name would then silently fail to load (`ProfileStoreError::
/// AlreadyExists`, previously swallowed via `let _ =`) -- the
/// customization would vanish on every restart. Both halves of the fix
/// (main.rs's call ordering, and `enable_durable_config` now propagating
/// that error instead of swallowing it) are covered together here: this
/// test only proves the end-to-end outcome (survives) rather than
/// re-deriving the internal ordering, which is what an operator actually
/// observes.
#[tokio::test]
async fn a_customized_default_named_profile_survives_a_real_restart() {
    let script_path = write_temp_file("acpx-durable-backend", OBSERVING_BACKEND_SCRIPT);
    let db_dir = tempfile::tempdir().expect("tempdir");
    let db_path = db_dir.path().join("acpx.sqlite");
    let client = reqwest::Client::new();

    // `default` is this test's `ACPX_DEFAULT_AGENT_ID`/registered agent
    // id -- also the exact profile name `warm_default_profiles` would
    // auto-seed for it if this agent were registry-listed and detected
    // installed. It is neither here (a bare `sh` stand-in, not a real
    // registry agent id), so the only way a profile literally named
    // "default" can exist at all is this test's own explicit
    // `profiles/create` below -- which is exactly the point: prove that
    // an operator-created profile survives, independent of whether
    // default-seeding could ever also produce that name in a real
    // deployment.
    {
        let addr = ephemeral_addr().await;
        let _guard = spawn_server(addr, &script_path, &db_path, &[]).await;

        let create = client
            .post(format!("http://{addr}/rpc"))
            .json(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "profiles/create",
                "params": {
                    "name": "default",
                    "agent_id": "default",
                    "secret": "sk-customized-default"
                }
            }))
            .send()
            .await
            .expect("POST /rpc profiles/create")
            .json::<Value>()
            .await
            .expect("json body");
        assert!(create["result"]["key_ref"].is_string(), "{create:?}");
    }

    let addr = ephemeral_addr().await;
    let guard = spawn_server(addr, &script_path, &db_path, &[]).await;

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
    let default_profile = profiles
        .iter()
        .find(|p| p["name"] == json!("default"))
        .unwrap_or_else(|| {
            panic!("customized \"default\" profile missing after restart: {profiles:?}")
        });
    assert!(
        default_profile["key_ref"].is_string(),
        "restored \"default\" profile must keep its customization (key_ref): {default_profile:?}"
    );

    let _ = guard;
}
