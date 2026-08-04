//! Real-process regression test for the grok-build `authenticate`
//! cross-contamination bug.
//!
//! **The bug.** `Router::call_policy` falls back to the process-global
//! `native_auth_method_id` (`ACPX_NATIVE_AUTH_METHOD_ID`, which
//! `acpx-server` auto-derives from `~/.codex/auth.json` for the *default*
//! agent) for every profile that carries no `auth_method_id` of its own.
//! Auto-seeded registry profiles never carry one, so `grok-build`
//! inherited codex's method id and acpx sent grok a `methodId` grok had
//! never advertised. Confirmed live against a real daemon-managed
//! instance: `session/new` with `_acpx.profile = "grok-build"` failed
//! instantly with
//! `backend rejected authenticate: {"code":-32602,"message":"Invalid
//! params","data":"unsupported auth method: chat-gpt"}` -- grok's own
//! `initialize` advertises `authMethods` `["cached_token", "grok.com"]`
//! and `_meta.defaultAuthMethodId = "cached_token"`, never `chat-gpt`. No
//! grok-build session could ever be created and no session row was ever
//! persisted for it.
//!
//! This test pins that down end to end: a real `acpx-server` binary, a
//! real `npx`-spawned grok backend, this machine's real ambient
//! `~/.grok/auth.json` session, and a real prompt round trip to xAI --
//! with `ACPX_NATIVE_AUTH_METHOD_ID=chat-gpt` set explicitly so the
//! cross-contamination is reproduced deterministically instead of
//! depending on whatever `~/.codex/auth.json` happens to hold.
//!
//! **`#[ignore]`d and opt-in via `ACPX_LIVE_TEST_GROK=1`** -- same
//! convention as `real_ambient_multi_agent_test.rs`: it needs a logged-in
//! grok CLI on this machine and makes a real billed xAI call, neither of
//! which is appropriate to run unconditionally in shared CI.
//!
//! Run with:
//! ```text
//! ACPX_LIVE_TEST_GROK=1 \
//! cargo test -p acpx-server --test real_grok_native_auth_test -- --ignored --nocapture
//! ```

use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use acpx_client::ext::prompt;
use acpx_client::raw::GatewayClient;
use tokio::process::{Child, Command};

/// Matches the 30-60s bound the other real-backend tests use: a real
/// npx spawn plus a real xAI turn, with enough headroom that a slow
/// cold `npx` cache doesn't produce a flaky failure, but far short of
/// the "hangs forever" symptom this test exists to catch.
const TURN_BUDGET: Duration = Duration::from_secs(60);

#[tokio::test]
#[ignore]
async fn grok_build_profile_authenticates_and_completes_a_real_turn() {
    if std::env::var("ACPX_LIVE_TEST_GROK").as_deref() != Ok("1") {
        eprintln!(
            "skipping: set ACPX_LIVE_TEST_GROK=1 to run this test against this machine's \
             real, already-logged-in grok CLI session (see this file's top doc comment -- \
             it makes a real billed xAI call)"
        );
        return;
    }

    let addr = ephemeral_addr().await;
    let _server = spawn_real_server(addr).await;
    let client = GatewayClient::new(format!("http://{addr}"));

    let agents = client
        .call("agents/list", serde_json::json!({}), None)
        .await
        .expect("agents/list");
    let entry = agents["agents"]
        .as_array()
        .expect("agents array")
        .iter()
        .find(|a| a["id"] == "grok-build")
        .cloned()
        .expect(
            "grok-build missing from agents/list -- it only exists in the live ACP registry, \
             not the bundled fallback, so this needs network access to the registry CDN",
        );
    assert_eq!(
        entry["status"], "installed",
        "grok-build not detected as installed -- is the grok CLI/npx cache present? {entry:?}"
    );

    // Native/unmanaged mode, byte-for-byte what the panel sends: the
    // auto-seeded `grok-build` profile, which carries no
    // `auth_method_id` and therefore hits the global-native fallback
    // this test is pinning.
    let session = tokio::time::timeout(
        TURN_BUDGET,
        client.call(
            "session/new",
            serde_json::json!({
                "cwd": "/tmp",
                "mcpServers": [],
                "_acpx": {"profile": "grok-build"},
            }),
            None,
        ),
    )
    .await
    .expect("session/new (profile grok-build) never returned within the turn budget")
    .expect(
        "session/new (profile grok-build) failed -- before the fix this was \
         `backend rejected authenticate: unsupported auth method: chat-gpt`, because acpx \
         sent codex's global ACPX_NATIVE_AUTH_METHOD_ID to an agent that never advertised it",
    );
    let session_id = session["sessionId"]
        .as_str()
        .expect("session/new (profile grok-build) had no sessionId")
        .to_string();

    let turn = tokio::time::timeout(
        TURN_BUDGET,
        prompt::send(
            &client,
            &session_id,
            serde_json::json!([{
                "type": "text",
                "text": "Reply with exactly the single word PLONK and nothing else.",
            }]),
        ),
    )
    .await
    .expect("session/prompt (profile grok-build) never completed within the turn budget")
    .expect("session/prompt (profile grok-build) failed");

    eprintln!("grok-build replied: {:?}", turn.message_text);
    assert!(
        turn.message_text.to_uppercase().contains("PLONK"),
        "grok-build: expected a real model reply containing PLONK, got {:?}",
        turn.message_text
    );

    let _ = client
        .call(
            "session/close",
            serde_json::json!({"sessionId": session_id}),
            None,
        )
        .await;
}

async fn ephemeral_addr() -> SocketAddr {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = probe.local_addr().expect("local_addr");
    drop(probe);
    addr
}

async fn spawn_real_server(http_addr: SocketAddr) -> ServerGuard {
    let db = std::env::temp_dir().join(format!("acpx-grok-auth-test-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);
    let child = Command::new(env!("CARGO_BIN_EXE_acpx-server"))
        .env("ACPX_HTTP_BIND", http_addr.to_string())
        .env("ACPX_DB_PATH", &db)
        // The whole point: reproduce the live daemon's global native auth
        // method deterministically, without depending on this machine's
        // `~/.codex/auth.json`.
        .env("ACPX_NATIVE_AUTH_METHOD_ID", "chat-gpt")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn real acpx-server binary");

    for _ in 0..200 {
        if tokio::net::TcpStream::connect(http_addr).await.is_ok() {
            return ServerGuard { child };
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("real acpx-server binary never opened its HTTP listener on {http_addr}");
}

struct ServerGuard {
    #[allow(dead_code)]
    child: Child,
}
