//! **Closes the gap tracked in `memory/acpx/gen/acpx-claude-bridge-real-e2e-execution.meta.json`:**
//! no test in this workspace previously exercised the strict `/acp`
//! bridge (`bridge-config.json` model catalog, exactly the surface Zed's
//! custom-agent config talks to -- see `acpx-server/src/transport/
//! acp_bridge.rs`) against a *real* `claude-agent-acp` process. The two
//! existing real-Claude tests cover different paths:
//! - `real_claude_multi_agent_test.rs` uses the native gateway's
//!   `_acpx.profile` path with an injected `ANTHROPIC_API_KEY`.
//! - `real_ambient_multi_agent_test.rs` uses the native gateway's
//!   `agents/list` + `profiles/create` + `_acpx.profile` path with this
//!   machine's ambient OAuth login, never touching the bridge's
//!   model-catalog resolution (`bridge-config.json`'s
//!   `claude/sonnet -> agent_id claude-acp, model_id sonnet`) at all.
//!
//! This file drives the real, already-compiled `acpx-server` binary with
//! the strict bridge enabled, over real `POST /acp/rpc`, exactly the way
//! Zed's custom-agent transport does: `session/new` (no profile, no
//! `_acpx` extension) -> `session/set_config_option` (public model alias)
//! -> `session/prompt` (real lazy `bind()`, real `npx claude-agent-acp`
//! spawn, real ambient `~/.claude/.credentials.json` OAuth, real model
//! call) -> `session/close`. Uses the adapter's cheapest model
//! (`haiku`), same convention as `real_ambient_multi_agent_test.rs`.
//!
//! **`#[ignore]`d and opt-in via `ACPX_LIVE_TEST_AMBIENT=1`** -- makes a
//! real, billed API call against whatever `claude` CLI account is
//! logged in on this machine. Same convention as
//! `real_ambient_multi_agent_test.rs`; see that file's top doc comment
//! for the full rationale.
//!
//! Run with:
//! ```text
//! ACPX_LIVE_TEST_AMBIENT=1 \
//! cargo test -p acpx-server --test real_claude_bridge_turn_test -- --ignored --nocapture
//! ```

use std::io::Write as _;
use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::process::{Child, Command};

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
    panic!("real acpx-server binary never opened its HTTP listener on {addr}");
}

struct ServerGuard {
    child: Child,
    _bridge_config_path: std::path::PathBuf,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Concatenate every `agent_message_chunk`'s text content out of a
/// response's `_acpx.updates` array, in order -- same extraction logic
/// as `acpx_client::ext::prompt::extract_message_text`, reimplemented
/// here since this test drives the bridge over raw `reqwest` (matching
/// the real Zed transport), not the `acpx-client` SDK.
fn extract_message_text(response: &Value) -> String {
    let mut text = String::new();
    let Some(updates) = response.pointer("/_acpx/updates").and_then(Value::as_array) else {
        return text;
    };
    for update in updates {
        if update.pointer("/params/update/sessionUpdate").and_then(Value::as_str)
            != Some("agent_message_chunk")
        {
            continue;
        }
        if let Some(chunk) = update
            .pointer("/params/update/content/text")
            .and_then(Value::as_str)
        {
            text.push_str(chunk);
        }
    }
    text
}

#[tokio::test]
#[ignore]
async fn real_bridge_claude_prompt_round_trip_uses_ambient_oauth() {
    if std::env::var("ACPX_LIVE_TEST_AMBIENT").as_deref() != Ok("1") {
        eprintln!(
            "skipping: set ACPX_LIVE_TEST_AMBIENT=1 to run this test against this \
             machine's real, already-logged-in claude CLI session and the real strict \
             /acp bridge (see this file's top doc comment -- it makes a real billed \
             API call)"
        );
        return;
    }

    let addr = ephemeral_addr().await;
    let bridge_config_path = write_temp_file(
        "acpx-claude-bridge-config",
        &json!({
            "default_model": "claude/haiku-e2e",
            "models": [{
                "id": "claude/haiku-e2e",
                "name": "Claude Haiku (e2e)",
                "agent_id": "claude-acp",
                "model_id": "haiku"
            }]
        })
        .to_string(),
    );
    // Fresh, per-test sqlite path -- avoids proactively recovering this
    // host's real systemd-managed acpx-server sessions and avoids
    // contending for the same sqlite file. Auth token removed for the
    // same inherited-environment reason as `acp_bridge_binary_test.rs`.
    let db_path = write_temp_file("acpx-claude-bridge-db", "");
    std::fs::remove_file(&db_path).expect("clear placeholder db file");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    cmd.env("ACPX_HTTP_BIND", addr.to_string())
        .env("ACPX_ACP_BRIDGE_ENABLED", "1")
        .env(
            "ACPX_ACP_BRIDGE_CONFIG_FILE",
            bridge_config_path.display().to_string(),
        )
        .env("ACPX_DB_PATH", db_path.display().to_string())
        .env_remove("ACPX_AUTH_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().expect("spawn real acpx-server binary");
    let _guard = ServerGuard {
        child,
        _bridge_config_path: bridge_config_path,
    };

    wait_for_listener(addr).await;

    let client = reqwest::Client::new();
    let rpc = |method: &'static str, params: Value, id: i64| {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    };

    let new_response = client
        .post(format!("http://{addr}/acp/rpc"))
        .json(&rpc(
            "session/new",
            json!({"cwd": "/tmp", "mcpServers": []}),
            1,
        ))
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

    let set_model_response = client
        .post(format!("http://{addr}/acp/rpc"))
        .json(&rpc(
            "session/set_config_option",
            json!({"sessionId": sid, "configId": "model", "value": "claude/haiku-e2e"}),
            2,
        ))
        .send()
        .await
        .expect("POST /acp/rpc session/set_config_option")
        .json::<Value>()
        .await
        .expect("json body");
    assert!(
        set_model_response.get("error").is_none(),
        "session/set_config_option(model) failed: {set_model_response:?}"
    );

    // Triggers real lazy bind() -> real npx claude-agent-acp spawn ->
    // real ambient OAuth handshake -> real haiku model call. Bounded
    // well above the real BACKEND_HANDSHAKE_TIMEOUT (30s) plus real
    // network/model latency.
    let prompt_response = tokio::time::timeout(
        Duration::from_secs(90),
        client
            .post(format!("http://{addr}/acp/rpc"))
            .json(&rpc(
                "session/prompt",
                json!({
                    "sessionId": sid,
                    "prompt": [{"type": "text", "text": "Reply with exactly the single word PONG and nothing else."}]
                }),
                3,
            ))
            .send(),
    )
    .await
    .expect("session/prompt over the real strict /acp bridge must not hang")
    .expect("POST /acp/rpc session/prompt")
    .json::<Value>()
    .await
    .expect("json body");
    assert!(
        prompt_response.get("error").is_none(),
        "session/prompt against the real bridge-selected claude-acp/haiku model failed: \
         {prompt_response:?}"
    );

    let message_text = extract_message_text(&prompt_response);
    assert!(
        message_text.to_uppercase().contains("PONG"),
        "expected a real model reply containing PONG via the strict /acp bridge's \
         _acpx.updates aggregation, got {message_text:?} (full response: \
         {prompt_response:?})"
    );

    let close_response = client
        .post(format!("http://{addr}/acp/rpc"))
        .json(&rpc("session/close", json!({"sessionId": sid}), 4))
        .send()
        .await
        .expect("POST /acp/rpc session/close")
        .json::<Value>()
        .await
        .expect("json body");
    assert!(
        close_response.get("error").is_none(),
        "session/close failed: {close_response:?}"
    );
}
