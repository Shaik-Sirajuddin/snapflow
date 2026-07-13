//! Full-lifecycle real-process test using *this machine's own already
//! logged-in* `claude`/`codex` CLI sessions -- no fabricated/injected API
//! keys anywhere, unlike `real_claude_multi_agent_test.rs` (which needs
//! `ACPX_LIVE_TEST_ANTHROPIC_*` credentials supplied by the caller). This
//! closes the gap the user asked about directly: "we already have
//! claude, codex binaries in this system, you can use that" -- prove the
//! real `acpx-server` binary can *detect* both via `agents/list`, spawn
//! each real npx-distributed ACP adapter under a profile with **no
//! `launch_overrides`/`provider` at all**, and have the adapter itself
//! pick up the ambient OAuth session (`~/.claude/.credentials.json` for
//! claude-agent-acp, the local codex CLI's own auth store for codex-acp)
//! -- proving the daemon + real adapter + real ambient auth + real model
//! call chain end to end, not just wiring.
//!
//! Manually verified once already (2026-07-13) via raw `curl` against a
//! live `acpx-server` process on this exact machine: claude-acp replied
//! `PONG` (real haiku call, ~$0.047 billed to the ambient account),
//! codex-acp replied `PANG` (real `codex/gpt-5.4-mini[low]` call via this
//! machine's bifrost-backed codex auth). This test automates that same
//! sequence through the real `acpx-client` SDK so it's reproducible, not
//! just a one-off manual check.
//!
//! **`#[ignore]`d and opt-in via `ACPX_LIVE_TEST_AMBIENT=1`** -- unlike
//! the fully-portable synthetic-backend tests, this one only works on a
//! machine that already has `claude`/`codex` CLIs installed and logged
//! in, makes real billed API calls against whatever account is logged
//! in, and hardcodes a cheap model id (`codex/gpt-5.4-mini`) that comes
//! from *this* machine's own model catalog (a bifrost-style proxy) --
//! not guaranteed to exist verbatim on a different machine's codex setup.
//! None of that is appropriate to run unconditionally in a shared CI
//! environment, so it stays opt-in exactly like `real_claude_multi_agent_
//! test.rs` and `acpx-registry/tests/live_registry.rs`.
//!
//! Run with:
//! ```text
//! ACPX_LIVE_TEST_AMBIENT=1 \
//! cargo test -p acpx-server --test real_ambient_multi_agent_test -- --ignored --nocapture
//! ```

use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use acpx_client::ext::{profiles, prompt};
use acpx_client::raw::GatewayClient;
use tokio::process::{Child, Command};

#[tokio::test]
#[ignore]
async fn ambient_claude_and_codex_profiles_hold_real_conversations_concurrently() {
    if std::env::var("ACPX_LIVE_TEST_AMBIENT").as_deref() != Ok("1") {
        eprintln!(
            "skipping: set ACPX_LIVE_TEST_AMBIENT=1 to run this test against this \
             machine's real, already-logged-in claude/codex CLI sessions (see this \
             file's top doc comment -- it makes real billed API calls)"
        );
        return;
    }

    let addr = ephemeral_addr().await;
    let _server = spawn_real_server(addr).await;
    let client = GatewayClient::new(format!("http://{addr}"));

    // Detection first: `agents/list` must report both real registry
    // entries as `installed` (node+npm on PATH is all `detect.rs` checks
    // for an npx-distributed agent -- see `acpx_core::detect::detect`)
    // before trusting either profile below to actually spawn.
    let agents = client
        .call("agents/list", serde_json::json!({}), None)
        .await
        .expect("agents/list");
    let list = agents["agents"].as_array().expect("agents array");
    for id in ["claude-acp", "codex-acp"] {
        let entry = list
            .iter()
            .find(|a| a["id"] == id)
            .unwrap_or_else(|| panic!("{id} missing from agents/list: {list:?}"));
        assert_eq!(
            entry["status"], "installed",
            "{id} not detected as installed -- is node/npm on PATH? entry: {entry:?}"
        );
    }

    // No `provider`/`launch_overrides` at all -- the whole point is that
    // the spawned adapter inherits this process's ambient environment
    // (`Supervisor`/`acpx_conductor::process` never strips the parent
    // env, only overlays `SpawnSpec.env` on top of it) and finds its own
    // already-authenticated CLI session, exactly like running
    // `npx -y @agentclientprotocol/claude-agent-acp` by hand would.
    profiles::create(
        &client,
        serde_json::json!({
            "name": "ambient-claude",
            "agent_id": "claude-acp",
            "provider": null,
            "key_ref": null,
            "launch_overrides": {},
            "mcp_servers": [],
        }),
    )
    .await
    .expect("profiles/create(ambient-claude)");
    profiles::create(
        &client,
        serde_json::json!({
            "name": "ambient-codex",
            "agent_id": "codex-acp",
            "provider": null,
            "key_ref": null,
            "launch_overrides": {},
            "mcp_servers": [],
        }),
    )
    .await
    .expect("profiles/create(ambient-codex)");

    let (claude_text, codex_text) = tokio::join!(
        run_claude_conversation(&client, "ambient-claude"),
        run_codex_conversation(&client, "ambient-codex"),
    );

    assert!(
        claude_text.to_uppercase().contains("PONG"),
        "claude-acp: expected a real model reply containing PONG, got {claude_text:?}"
    );
    assert!(
        codex_text.to_uppercase().contains("PANG"),
        "codex-acp: expected a real model reply containing PANG, got {codex_text:?}"
    );
}

/// `session/new` -> force the real adapter's cheapest model (`haiku`) via
/// `session/set_config_option` -> one `session/prompt` turn ->
/// `session/close`. Mirrors `real_claude_multi_agent_test.rs`'s
/// `run_two_turn_conversation` but single-turn (this test's goal is
/// proving ambient-auth detection/spawn/call, not re-proving the
/// multi-turn `_acpx.updates` aggregation fix a second time).
async fn run_claude_conversation(client: &GatewayClient, profile: &str) -> String {
    let new_result = client
        .call(
            "session/new",
            serde_json::json!({"cwd": "/tmp", "mcpServers": [], "_acpx": {"profile": profile}}),
            None,
        )
        .await
        .unwrap_or_else(|err| panic!("session/new (profile {profile}) failed: {err}"));
    let session_id = new_result["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new (profile {profile}) had no sessionId"))
        .to_string();

    client
        .call(
            "session/set_config_option",
            serde_json::json!({"sessionId": session_id, "configId": "model", "value": "haiku"}),
            None,
        )
        .await
        .unwrap_or_else(|err| panic!("set_config_option (profile {profile}) failed: {err}"));

    let turn = prompt::send(
        client,
        &session_id,
        serde_json::json!([{"type": "text", "text": "Reply with exactly the single word PONG and nothing else."}]),
    )
    .await
    .unwrap_or_else(|err| panic!("session/prompt (profile {profile}) failed: {err}"));

    let _ = client
        .call(
            "session/close",
            serde_json::json!({"sessionId": session_id}),
            None,
        )
        .await;

    turn.message_text
}

/// Same shape as [`run_claude_conversation`] but for `codex-acp`: model
/// ids come from this machine's own (bifrost-backed) codex model catalog,
/// not the upstream OpenAI catalog, so `codex/gpt-5.4-mini` is this
/// environment's cheapest/lowest-latency entry as observed manually, not
/// a portable assumption -- see this file's top doc comment.
async fn run_codex_conversation(client: &GatewayClient, profile: &str) -> String {
    let new_result = client
        .call(
            "session/new",
            serde_json::json!({"cwd": "/tmp", "mcpServers": [], "_acpx": {"profile": profile}}),
            None,
        )
        .await
        .unwrap_or_else(|err| panic!("session/new (profile {profile}) failed: {err}"));
    let session_id = new_result["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new (profile {profile}) had no sessionId"))
        .to_string();

    client
        .call(
            "session/set_config_option",
            serde_json::json!({"sessionId": session_id, "configId": "model", "value": "codex/gpt-5.4-mini"}),
            None,
        )
        .await
        .unwrap_or_else(|err| panic!("set_config_option (profile {profile}) failed: {err}"));

    let turn = prompt::send(
        client,
        &session_id,
        serde_json::json!([{"type": "text", "text": "Reply with exactly the single word PANG and nothing else."}]),
    )
    .await
    .unwrap_or_else(|err| panic!("session/prompt (profile {profile}) failed: {err}"));

    let _ = client
        .call(
            "session/close",
            serde_json::json!({"sessionId": session_id}),
            None,
        )
        .await;

    turn.message_text
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
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Spawns the real, already-compiled `acpx-server` binary against an
/// ephemeral HTTP bind address and waits for its listener to accept
/// connections. `ACPX_BACKEND_CMD`/default-agent is left unused -- every
/// session in this test goes through a profile.
async fn spawn_real_server(http_addr: SocketAddr) -> ServerGuard {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    cmd.env("ACPX_HTTP_BIND", http_addr.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = cmd.spawn().expect("spawn real acpx-server binary");

    for _ in 0..100 {
        if tokio::net::TcpStream::connect(http_addr).await.is_ok() {
            return ServerGuard { child };
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("real acpx-server binary never opened its HTTP listener on {http_addr}");
}
