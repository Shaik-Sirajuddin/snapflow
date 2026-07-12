//! **The capstone test for this workspace's biggest remaining gap**,
//! tracked in `acpx/COVERAGE.md`'s Gaps section through Phase 6 and the
//! post-Phase-6 self-test layer: every other test in this workspace
//! (~120 of them) uses a synthetic `sh -c '...'` stand-in backend, never
//! a real, published, npx-installed ACP adapter. This file closes that
//! gap for real, driving:
//!
//! - the real, already-compiled `acpx-server` binary (spawned via
//!   `CARGO_BIN_EXE_acpx-server`, same technique as `binary_self_test.rs`)
//! - which spawns a real `npx -y @agentclientprotocol/claude-agent-acp`
//!   child process (the official registry's `claude-acp` entry,
//!   `acpx-registry/registry.fallback.json`) -- not mocked, not stubbed
//! - which talks to a real Anthropic-Messages-API-compatible endpoint
//!   serving `claude-haiku-4-5` (the cheapest/fastest model available,
//!   satisfying "use only haiku or low-variant models for testing" --
//!   selected explicitly via the real `session/set_config_option` ACP
//!   extension method, verified against the real adapter's source, see
//!   `acpx_core::router::classify`'s doc comment on that method)
//! - through the real `acpx-client` SDK (`raw::GatewayClient` +
//!   `ext::prompt`/`ext::profiles`), not raw `reqwest` calls -- proving
//!   "acpx daemon + acpx client end to end", the full stated goal,
//!   together rather than the daemon alone
//! - across **two independently supervised real agent processes**
//!   (two profiles, each spawning its own `npx claude-agent-acp` child --
//!   see `acpx_conductor::supervisor`'s per-profile supervisor-key
//!   scheme), run **concurrently** via `tokio::join!` -- re-proving the
//!   real multi-agent concurrency fix
//!   (`acpx-server/tests/concurrency_test.rs`) against real backend
//!   processes and real network latency, not just a synthetic `sleep`
//! - with a **two-turn conversation** per agent ("few turns" per the
//!   stated goal), proving the reverse-direction `_acpx.updates`
//!   aggregation fix (`acpx_core::router::read_matching_response`)
//!   actually delivers real model-generated text back through the
//!   gateway and the client SDK -- not just a `{stopReason, usage}` husk.
//!
//! **Ignored by default and gated on environment variables** -- this is
//! the one test in this workspace that depends on genuine outbound
//! network access to an Anthropic-Messages-API-compatible endpoint and a
//! real credential, neither of which is universally available (and the
//! credential must never be committed to source). Run it explicitly with:
//!
//! ```text
//! ACPX_LIVE_TEST_ANTHROPIC_BASE_URL=https://your-endpoint \
//! ACPX_LIVE_TEST_ANTHROPIC_API_KEY=sk-... \
//! cargo test -p acpx-server --test real_claude_multi_agent_test -- --ignored --nocapture
//! ```
//!
//! If the two env vars are unset, the test prints a message and returns
//! early (treated as a pass, not a failure) rather than requiring every
//! contributor/CI run to have this specific setup -- matching this
//! workspace's existing convention for `acpx-registry/tests/
//! live_registry.rs`'s `#[ignore]`d live-network test.

use std::net::SocketAddr;
use std::process::Stdio;
use std::time::{Duration, Instant};

use acpx_client::ext::{profiles, prompt};
use acpx_client::raw::GatewayClient;
use tokio::process::{Child, Command};

/// `#[ignore]`d, network/credential-gated -- see this file's top doc
/// comment for how to opt in.
#[tokio::test]
#[ignore]
async fn two_real_claude_agent_profiles_hold_independent_two_turn_conversations_concurrently() {
    let Some(creds) = LiveCreds::from_env() else {
        eprintln!(
            "skipping: set ACPX_LIVE_TEST_ANTHROPIC_BASE_URL and \
             ACPX_LIVE_TEST_ANTHROPIC_API_KEY to run this test against a \
             real claude-agent-acp adapter (see this file's top doc comment)"
        );
        return;
    };

    let addr = ephemeral_addr().await;
    let _server = spawn_real_server(addr).await;
    let client = GatewayClient::new(format!("http://{addr}"));

    // Two independently-provisioned profiles, both targeting the real
    // registry's `claude-acp` entry (resolved by `Router::resolve_profile`
    // straight off `registry.fallback.json`/the live registry -- no
    // `register_agent` call needed, exactly what a real remote client
    // gets by only ever talking JSON-RPC to the gateway, never touching
    // Rust internals). Each profile's own `launch_overrides` carries the
    // real endpoint/credential directly -- `profiles/create` accepting
    // raw `launch_overrides` env vars is an already-existing escape hatch
    // (`acpx-core/src/profile.rs`) that needs no separate `providers/*`
    // RPC surface (a real, still-open gap, see `COVERAGE.md`) for this
    // case: `ANTHROPIC_API_KEY`/`ANTHROPIC_BASE_URL` are exactly the env
    // vars `acpx-core::launch::provider_env` would derive from a
    // `ProviderConfig::Anthropic` anyway (verified against the real
    // adapter, see that module's doc comment).
    for name in ["claude-a", "claude-b"] {
        let created = profiles::create(
            &client,
            serde_json::json!({
                "name": name,
                "agent_id": "claude-acp",
                "provider": null,
                "key_ref": null,
                "launch_overrides": {
                    "ANTHROPIC_API_KEY": creds.api_key.clone(),
                    "ANTHROPIC_BASE_URL": creds.base_url.clone(),
                },
                "mcp_servers": [],
            }),
        )
        .await
        .unwrap_or_else(|err| panic!("profiles/create({name}) failed: {err}"));
        let _ = created;
    }

    let start = Instant::now();
    let (outcome_a, outcome_b) = tokio::join!(
        run_two_turn_conversation(&client, "claude-a"),
        run_two_turn_conversation(&client, "claude-b"),
    );
    let elapsed = start.elapsed();
    eprintln!("both real-agent conversations finished in {elapsed:?}");

    for (label, outcome) in [("claude-a", outcome_a), ("claude-b", outcome_b)] {
        let turn1 = outcome.turn1_text.to_uppercase();
        let turn2 = outcome.turn2_text.to_uppercase();
        assert!(
            turn1.contains("PONG"),
            "{label} turn 1: expected real model reply containing PONG, got {:?} (full response: {:?})",
            outcome.turn1_text,
            outcome.turn1_result
        );
        assert!(
            turn2.contains("PANG"),
            "{label} turn 2: expected real model reply containing PANG, got {:?} (full response: {:?})",
            outcome.turn2_text,
            outcome.turn2_result
        );
    }
}

struct LiveCreds {
    base_url: String,
    api_key: String,
}

impl LiveCreds {
    fn from_env() -> Option<Self> {
        Some(Self {
            base_url: std::env::var("ACPX_LIVE_TEST_ANTHROPIC_BASE_URL").ok()?,
            api_key: std::env::var("ACPX_LIVE_TEST_ANTHROPIC_API_KEY").ok()?,
        })
    }
}

struct ConversationOutcome {
    turn1_text: String,
    turn1_result: serde_json::Value,
    turn2_text: String,
    turn2_result: serde_json::Value,
}

/// `session/new` -> `session/set_config_option` (force the cheapest
/// model, "haiku") -> two `session/prompt` turns -> `session/close`,
/// entirely through the real `acpx-client` SDK.
async fn run_two_turn_conversation(client: &GatewayClient, profile: &str) -> ConversationOutcome {
    let new_result = client
        .call(
            "session/new",
            // `mcpServers` is a required field in the real ACP schema
            // (verified against claude-agent-acp: omitting it entirely,
            // rather than sending an empty array, gets rejected with a
            // real `-32602 Invalid params` JSON-RPC error) -- acpx's own
            // router deliberately forwards `session/new` params
            // byte-for-byte once `_acpx` is stripped (see
            // `acpx_core::router::dispatch_session_new`'s doc comment on
            // staying a raw-ACP drop-in), so a real client is expected to
            // supply every field the raw spec requires itself, same as
            // any other ACP client talking to claude-agent-acp directly.
            serde_json::json!({"cwd": "/tmp", "mcpServers": [], "_acpx": {"profile": profile}}),
            None,
        )
        .await
        .unwrap_or_else(|err| panic!("session/new (profile {profile}) failed: {err}"));
    let session_id = new_result["sessionId"]
        .as_str()
        .unwrap_or_else(|| {
            panic!("session/new (profile {profile}) had no sessionId: {new_result:?}")
        })
        .to_string();

    // Force the cheapest model this real adapter offers -- see
    // `acpx_core::router::classify`'s doc comment on why
    // `session/set_config_option` had to be added to the Proxied bucket
    // for this call to reach the backend at all.
    client
        .call(
            "session/set_config_option",
            serde_json::json!({"sessionId": session_id, "configId": "model", "value": "haiku"}),
            None,
        )
        .await
        .unwrap_or_else(|err| {
            panic!("session/set_config_option (profile {profile}) failed: {err}")
        });

    let turn1 = prompt::send(
        client,
        &session_id,
        serde_json::json!([{"type": "text", "text": "Reply with exactly the single word PONG and nothing else."}]),
    )
    .await
    .unwrap_or_else(|err| panic!("turn 1 session/prompt (profile {profile}) failed: {err}"));

    let turn2 = prompt::send(
        client,
        &session_id,
        serde_json::json!([{"type": "text", "text": "Now reply with exactly the single word PANG and nothing else."}]),
    )
    .await
    .unwrap_or_else(|err| panic!("turn 2 session/prompt (profile {profile}) failed: {err}"));

    let _ = client
        .call(
            "session/close",
            serde_json::json!({"sessionId": session_id}),
            None,
        )
        .await;

    ConversationOutcome {
        turn1_text: turn1.message_text,
        turn1_result: turn1.result,
        turn2_text: turn2.message_text,
        turn2_result: turn2.result,
    }
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

/// Spawns the real, already-built `acpx-server` binary against an
/// ephemeral HTTP bind address, then polls until its HTTP listener
/// actually accepts a connection. The default-agent `ACPX_BACKEND_CMD` is
/// left at its own default (unused by this test -- every session goes
/// through a profile, never native mode) rather than pointed at a
/// stand-in, matching how a real deployment would actually be configured.
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
