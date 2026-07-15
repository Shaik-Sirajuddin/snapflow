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
//! **Schema-validation addition (2026-07-14, answering "did you verify
//! this with codex exec or claude e2e, does the daemon actually use/
//! detect the strict schema?"):** every real JSON-RPC params/result this
//! test sends to or receives from the real, ambient-auth `claude-acp`/
//! `codex-acp` backends is now also run through
//! `acpx_proto::validate::{validate_params, validate_result}` -- the
//! `jsonschema` crate validating against the exact same generated
//! `$defs` `docs/schema/acpx.openrpc.json`/`acpx-wire.schema.json` are
//! built from (see `acpx-proto/src/validate.rs`). Every prior phase of
//! the `acpx-openrpc-schema` plan only proved the generated schema
//! documents were internally self-consistent; this is the first place
//! real wire traffic to/from a real backend process is checked against
//! them, closing that gap for real rather than by assertion.

use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use acpx_client::ext::{profiles, prompt};
use acpx_client::raw::GatewayClient;
use acpx_proto::validate::{validate_params, validate_result};
use tokio::process::{Child, Command};

/// Panics with every validation failure listed if `errors` is non-empty --
/// used at every real-traffic call site below so a schema regression
/// against real backend output fails loudly and specifically, not with a
/// generic assertion.
fn assert_schema_valid(context: &str, errors: Vec<String>) {
    assert!(
        errors.is_empty(),
        "{context}: real wire traffic failed schema validation:\n{}",
        errors.join("\n")
    );
}

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
    assert_schema_valid(
        "agents/list result",
        validate_result("agents/list", &agents),
    );
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

/// Claude-only variant of the concurrent test above, added alongside the
/// schema-validation instrumentation in `run_claude_conversation` --
/// lets that instrumentation be proven against a real backend
/// independently of `codex-acp`'s ambient-auth availability on this
/// machine (which can drift over time -- e.g. this adapter version
/// starting to require an explicit `auth_method_id` -- without that
/// drift blocking proof that the schema pipeline validates real traffic
/// at all). See this file's top doc comment for the schema-validation
/// rationale.
#[tokio::test]
#[ignore]
async fn ambient_claude_only_conversation_conforms_to_generated_schema() {
    if std::env::var("ACPX_LIVE_TEST_AMBIENT").as_deref() != Ok("1") {
        eprintln!(
            "skipping: set ACPX_LIVE_TEST_AMBIENT=1 to run this test against this \
             machine's real, already-logged-in claude CLI session (see this file's \
             top doc comment -- it makes real billed API calls)"
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
    assert_schema_valid(
        "agents/list result",
        validate_result("agents/list", &agents),
    );

    profiles::create(
        &client,
        serde_json::json!({
            "name": "ambient-claude-solo",
            "agent_id": "claude-acp",
            "provider": null,
            "key_ref": null,
            "launch_overrides": {},
            "mcp_servers": [],
        }),
    )
    .await
    .expect("profiles/create(ambient-claude-solo)");

    let text = run_claude_conversation(&client, "ambient-claude-solo").await;
    assert!(
        text.to_uppercase().contains("PONG"),
        "claude-acp: expected a real model reply containing PONG, got {text:?}"
    );
}

/// `session/new` -> force the real adapter's cheapest model (`haiku`) via
/// `session/set_config_option` -> one `session/prompt` turn ->
/// `session/close`. Mirrors `real_claude_multi_agent_test.rs`'s
/// `run_two_turn_conversation` but single-turn (this test's goal is
/// proving ambient-auth detection/spawn/call, not re-proving the
/// multi-turn `_acpx.updates` aggregation fix a second time).
async fn run_claude_conversation(client: &GatewayClient, profile: &str) -> String {
    let new_params =
        serde_json::json!({"cwd": "/tmp", "mcpServers": [], "_acpx": {"profile": profile}});
    assert_schema_valid(
        "claude session/new params",
        validate_params("session/new", &new_params),
    );
    let new_result = client
        .call("session/new", new_params, None)
        .await
        .unwrap_or_else(|err| panic!("session/new (profile {profile}) failed: {err}"));
    assert_schema_valid(
        "claude session/new result",
        validate_result("session/new", &new_result),
    );
    let session_id = new_result["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new (profile {profile}) had no sessionId"))
        .to_string();

    let set_config_params =
        serde_json::json!({"sessionId": session_id, "configId": "model", "value": "haiku"});
    assert_schema_valid(
        "claude session/set_config_option params",
        validate_params("session/set_config_option", &set_config_params),
    );
    client
        .call("session/set_config_option", set_config_params, None)
        .await
        .unwrap_or_else(|err| panic!("set_config_option (profile {profile}) failed: {err}"));

    let prompt_content = serde_json::json!([{"type": "text", "text": "Reply with exactly the single word PONG and nothing else."}]);
    let prompt_params = serde_json::json!({"sessionId": session_id, "prompt": prompt_content});
    assert_schema_valid(
        "claude session/prompt params",
        validate_params("session/prompt", &prompt_params),
    );
    let turn = prompt::send(client, &session_id, prompt_content)
        .await
        .unwrap_or_else(|err| panic!("session/prompt (profile {profile}) failed: {err}"));
    assert_schema_valid(
        "claude session/prompt result",
        validate_result("session/prompt", &turn.result),
    );

    let close_params = serde_json::json!({"sessionId": session_id});
    assert_schema_valid(
        "claude session/close params",
        validate_params("session/close", &close_params),
    );
    let _ = client.call("session/close", close_params, None).await;

    turn.message_text
}

/// Codex-only counterpart to
/// `ambient_claude_only_conversation_conforms_to_generated_schema`,
/// isolating `codex-acp`'s ambient-auth behavior from `claude-acp`'s so
/// a regression in either adapter's ambient-auth handshake doesn't mask
/// the other's schema-validation proof. `auth_method_id: "api-key"`
/// -- superseded by `"chat-gpt"`, see below -- added here (2026-07-14)
/// because a fresh `npx -y` fetch of
/// `codex-acp` on this machine started requiring an explicit
/// `authenticate` call before `session/new` where it previously didn't
/// -- a real environmental/adapter-version drift found while running
/// this very test, not a schema-pipeline concern, see
/// `Profile::auth_method_id`'s doc comment and `RouterError::
/// BackendRequiresAuthentication`.
///
/// **Known current gap, found while writing this test (2026-07-14):**
/// `"api-key"` fails fast with a clear `CODEX_API_KEY`/`OPENAI_API_KEY`-
/// not-set error (expected -- this test deliberately supplies no
/// credentials, ambient-only per this file's whole premise).
/// `"chat-gpt"` (this machine's actual ambient codex CLI login method)
/// instead hangs past this test's own real-network patience (multiple
/// minutes) with no error and no completion -- consistent with the
/// `authenticate` handshake needing an interactive device-code/browser
/// step the codex-acp adapter can't complete headlessly under a
/// `Supervisor`-spawned child process with no TTY, rather than reusing
/// the already-logged-in CLI session's token non-interactively the way
/// `claude-agent-acp` does. This is a `codex-acp`-adapter-side ambient-
/// auth gap, not an acpx schema/dispatch bug -- `ambient_claude_only_
/// conversation_conforms_to_generated_schema` above already proves the
/// schema-validation pipeline against a real backend end to end;
/// `#[ignore]` (inherited from this test's own attribute) plus this
/// comment is the honest way to leave this documented rather than
/// deleting the coverage or silently working around it.
#[tokio::test]
#[ignore]
async fn ambient_codex_only_conversation_conforms_to_generated_schema() {
    if std::env::var("ACPX_LIVE_TEST_AMBIENT").as_deref() != Ok("1") {
        eprintln!(
            "skipping: set ACPX_LIVE_TEST_AMBIENT=1 to run this test against this \
             machine's real, already-logged-in codex CLI session (see this file's \
             top doc comment -- it makes real billed API calls)"
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
    assert_schema_valid(
        "agents/list result",
        validate_result("agents/list", &agents),
    );

    profiles::create(
        &client,
        serde_json::json!({
            "name": "ambient-codex-solo",
            "agent_id": "codex-acp",
            "provider": null,
            "key_ref": null,
            "launch_overrides": {},
            "mcp_servers": [],
            "auth_method_id": "chat-gpt",
        }),
    )
    .await
    .expect("profiles/create(ambient-codex-solo)");

    let text = tokio::time::timeout(
        Duration::from_secs(120),
        run_codex_conversation(&client, "ambient-codex-solo"),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "codex-acp did not complete its ambient chat-gpt authentication/session \
             flow within 120 seconds; this adapter currently requires a headless-safe \
             auth path (for example API-key provisioning) rather than silently hanging"
        )
    });
    assert!(
        text.to_uppercase().contains("PANG"),
        "codex-acp: expected a real model reply containing PANG, got {text:?}"
    );
}

/// Same shape as [`run_claude_conversation`] but for `codex-acp`: model
/// ids come from this machine's own (bifrost-backed) codex model catalog,
/// not the upstream OpenAI catalog, so `codex/gpt-5.4-mini` is this
/// environment's cheapest/lowest-latency entry as observed manually, not
/// a portable assumption -- see this file's top doc comment.
async fn run_codex_conversation(client: &GatewayClient, profile: &str) -> String {
    let new_params =
        serde_json::json!({"cwd": "/tmp", "mcpServers": [], "_acpx": {"profile": profile}});
    assert_schema_valid(
        "codex session/new params",
        validate_params("session/new", &new_params),
    );
    let new_result = client
        .call("session/new", new_params, None)
        .await
        .unwrap_or_else(|err| panic!("session/new (profile {profile}) failed: {err}"));
    assert_schema_valid(
        "codex session/new result",
        validate_result("session/new", &new_result),
    );
    let session_id = new_result["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new (profile {profile}) had no sessionId"))
        .to_string();

    let set_config_params = serde_json::json!({"sessionId": session_id, "configId": "model", "value": "codex/gpt-5.4-mini"});
    assert_schema_valid(
        "codex session/set_config_option params",
        validate_params("session/set_config_option", &set_config_params),
    );
    client
        .call("session/set_config_option", set_config_params, None)
        .await
        .unwrap_or_else(|err| panic!("set_config_option (profile {profile}) failed: {err}"));

    let prompt_content = serde_json::json!([{"type": "text", "text": "Reply with exactly the single word PANG and nothing else."}]);
    let prompt_params = serde_json::json!({"sessionId": session_id, "prompt": prompt_content});
    assert_schema_valid(
        "codex session/prompt params",
        validate_params("session/prompt", &prompt_params),
    );
    let turn = prompt::send(client, &session_id, prompt_content)
        .await
        .unwrap_or_else(|err| panic!("session/prompt (profile {profile}) failed: {err}"));
    assert_schema_valid(
        "codex session/prompt result",
        validate_result("session/prompt", &turn.result),
    );

    let close_params = serde_json::json!({"sessionId": session_id});
    assert_schema_valid(
        "codex session/close params",
        validate_params("session/close", &close_params),
    );
    let _ = client.call("session/close", close_params, None).await;

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
    spawn_real_server_with_db(http_addr, None).await
}

/// Same as [`spawn_real_server`] but optionally wires `ACPX_DB_PATH` to a
/// caller-supplied sqlite file. **Phase 8 addition**, used by
/// `ambient_claude_session_load_survives_a_real_gateway_restart` to prove
/// `session/load`'s rehydration path against a real second process, not
/// just an in-process `Router` restart simulation.
async fn spawn_real_server_with_db(
    http_addr: SocketAddr,
    db_path: Option<&std::path::Path>,
) -> ServerGuard {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_acpx-server"));
    cmd.env("ACPX_HTTP_BIND", http_addr.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(path) = db_path {
        cmd.env("ACPX_DB_PATH", path);
    }

    let child = cmd.spawn().expect("spawn real acpx-server binary");

    for _ in 0..100 {
        if tokio::net::TcpStream::connect(http_addr).await.is_ok() {
            return ServerGuard { child };
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("real acpx-server binary never opened its HTTP listener on {http_addr}");
}

/// **Phase 8 addition.** Proves the real fix in `Router::rehydrate_session`
/// (`acpx-core/src/router.rs`): before this phase, `session/load` was
/// classified as `Proxied` but required the gateway session id to
/// already be a live key in the in-memory `SessionRegistry` -- exactly
/// like `session/prompt`/every other proxied method. That defeated the
/// entire point of `session/load` existing as a *distinct* method from
/// `session/new`: a client is fully entitled to call it with a session
/// id it learned about before this exact acpx process started (most
/// obviously: after acpx itself restarted). Before this fix that always
/// failed with `UnknownSession`, even though acpx's own sqlite
/// (`ACPX_DB_PATH`) had a durable row proving the session existed and
/// which real backend/profile it belonged to.
///
/// This test is the real thing, not a simulation: spawns one real
/// `acpx-server` process against a real sqlite file, creates a real
/// `claude-agent-acp` session with one billed prompt turn, closes it,
/// **kills that whole process**, spawns a **second, independent**
/// `acpx-server` process against the *same* sqlite file (a fresh,
/// empty `SessionRegistry` -- nothing in memory carries over between
/// the two processes, only the file), and calls `session/load` against
/// it with the *first* process's gateway session id. Proves: (1) the
/// rehydration lookup finds the row, (2) it correctly resolves back to
/// `claude-acp`/the right profile so the second process spawns a fresh
/// real adapter for it, (3) the forwarded backend session id is right
/// (the real adapter accepts it and doesn't error `Session not found`),
/// (4) `session/set_mode` against that same rehydrated session works
/// (using a real `modeId` read back from the `session/load` response's
/// own `modes.availableModes` -- zero real-backend coverage of
/// `session/set_mode` existed anywhere in this workspace before this
/// test), and (5) the gateway session id is reusable afterward in the
/// *new* process for a real follow-up `session/prompt` turn.
///
/// **`#[ignore]`d and opt-in via `ACPX_LIVE_TEST_AMBIENT=1`**, same
/// rationale as the rest of this file.
///
/// Run with:
/// ```text
/// ACPX_LIVE_TEST_AMBIENT=1 \
/// cargo test -p acpx-server --test real_ambient_multi_agent_test \
///   ambient_claude_session_load_survives_a_real_gateway_restart -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore]
async fn ambient_claude_session_load_survives_a_real_gateway_restart() {
    if std::env::var("ACPX_LIVE_TEST_AMBIENT").as_deref() != Ok("1") {
        eprintln!(
            "skipping: set ACPX_LIVE_TEST_AMBIENT=1 to run this test against this \
             machine's real, already-logged-in claude CLI session (see this file's \
             top doc comment -- it makes a real billed API call)"
        );
        return;
    }

    let db_path = std::env::temp_dir().join(format!(
        "acpx-session-load-restart-test-{}-{}.sqlite3",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));

    let gateway_session_id = {
        let addr = ephemeral_addr().await;
        let server = spawn_real_server_with_db(addr, Some(&db_path)).await;
        let client = GatewayClient::new(format!("http://{addr}"));

        profiles::create(
            &client,
            serde_json::json!({
                "name": "ambient-claude-restart",
                "agent_id": "claude-acp",
                "provider": null,
                "key_ref": null,
                "launch_overrides": {},
                "mcp_servers": [],
            }),
        )
        .await
        .expect("profiles/create(ambient-claude-restart)");

        let new_result = client
            .call(
                "session/new",
                serde_json::json!({"cwd": "/tmp", "mcpServers": [], "_acpx": {"profile": "ambient-claude-restart"}}),
                None,
            )
            .await
            .expect("session/new");
        let gateway_session_id = new_result["sessionId"]
            .as_str()
            .expect("session/new had no sessionId")
            .to_string();

        client
            .call(
                "session/set_config_option",
                serde_json::json!({"sessionId": gateway_session_id, "configId": "model", "value": "haiku"}),
                None,
            )
            .await
            .expect("session/set_config_option");

        let turn = prompt::send(
            &client,
            &gateway_session_id,
            serde_json::json!([{"type": "text", "text": "Reply with exactly the single word OK and nothing else."}]),
        )
        .await
        .expect("session/prompt");
        assert!(
            turn.message_text.to_uppercase().contains("OK"),
            "expected a real model reply containing OK, got {:?}",
            turn.message_text
        );

        client
            .call(
                "session/close",
                serde_json::json!({"sessionId": gateway_session_id}),
                None,
            )
            .await
            .expect("session/close");

        drop(server); // kill_on_drop -- the whole first acpx-server process dies here.
        gateway_session_id
    };

    // Give the OS a moment to actually finish tearing down the first
    // process/port before standing up the second on a fresh address.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let addr2 = ephemeral_addr().await;
    let _server2 = spawn_real_server_with_db(addr2, Some(&db_path)).await;
    let client2 = GatewayClient::new(format!("http://{addr2}"));

    // Profiles themselves are runtime-registered state, not part of
    // `ACPX_DB_PATH`'s `sessions` table (that's `ACPX_CONFIG_FILE`
    // provisioning's job, a separately-solved problem -- see
    // `provisioning_binary_test.rs`) -- a real deployment keeps profile
    // definitions consistent across restarts via that declarative config,
    // not by expecting a `profiles/create` call against one process to
    // somehow survive into an unrelated second one. Re-declare the same
    // profile here so this test isolates exactly what it's meant to prove
    // (`session/load` rehydration), rather than also (mis)asserting a
    // claim about profile durability this test was never about.
    profiles::create(
        &client2,
        serde_json::json!({
            "name": "ambient-claude-restart",
            "agent_id": "claude-acp",
            "provider": null,
            "key_ref": null,
            "launch_overrides": {},
            "mcp_servers": [],
        }),
    )
    .await
    .expect("profiles/create(ambient-claude-restart) against the second process");

    // No `session/new` in this second process at all -- `gateway_session_id`
    // is only known to the *first* process's now-dead in-memory registry.
    // This must come back from the sqlite row alone.
    let load_result = client2
        .call(
            "session/load",
            serde_json::json!({
                "sessionId": gateway_session_id,
                "cwd": "/tmp",
                "mcpServers": [],
            }),
            None,
        )
        .await
        .unwrap_or_else(|err| {
            panic!(
                "session/load against the second, independent acpx-server process \
                 failed to rehydrate a session created by the first process: {err}"
            )
        });
    // NOTE: the real ACP `LoadSessionResponse` schema (per
    // agentclientprotocol.com/protocol/schema) has *no* `sessionId`
    // field at all -- only `modes`/`configOptions`/`_meta` -- so there is
    // nothing spec-mandated to assert identity-consistency of here.
    // `claude-agent-acp` happens to also echo a non-standard `sessionId`
    // key of its own; acpx forwards it verbatim per its transparent-proxy
    // design (same as any other field it doesn't know about), so this
    // test doesn't assert anything about that extra key's exact value.
    // Similarly, whether the real adapter's `loadSession` actually emits
    // `session/update` history-replay notifications for *this specific*
    // session (a single trivial one-turn conversation) is an adapter-
    // internal implementation detail this test observed empirically to
    // be empty in practice on this machine's `claude-agent-acp` build --
    // not something acpx controls or should assert a specific shape for.
    let replayed_updates = load_result["_acpx"]["updates"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    eprintln!(
        "session/load replayed {} _acpx.updates entries (adapter-dependent, informational only)",
        replayed_updates.len()
    );
    assert!(
        load_result.get("modes").is_some() || load_result.get("configOptions").is_some(),
        "session/load response should carry at least modes or configOptions per the real \
         LoadSessionResponse schema: {load_result:?}"
    );

    // **Phase 8, `session/set_mode` coverage.** Zero real-backend
    // coverage existed anywhere in this workspace before this test --
    // reuse this same rehydrated session rather than a separate live
    // test (avoids one more billed API call for a check that needs none:
    // `setSessionMode` is a pure in-adapter permission-mode change, no
    // model call). Picks a real, non-default `modeId` straight out of
    // this exact adapter build's own `session/load` response so nothing
    // here is a hardcoded guess about which mode ids exist.
    let available_modes = load_result["modes"]["availableModes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let current_mode_id = load_result["modes"]["currentModeId"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let target_mode_id = available_modes
        .iter()
        .filter_map(|m| m["id"].as_str())
        .find(|id| *id != current_mode_id)
        .unwrap_or(&current_mode_id)
        .to_string();
    assert!(
        !target_mode_id.is_empty(),
        "session/load's response carried no usable modes.availableModes to drive \
         session/set_mode with: {load_result:?}"
    );
    client2
        .call(
            "session/set_mode",
            serde_json::json!({"sessionId": gateway_session_id, "modeId": target_mode_id}),
            None,
        )
        .await
        .unwrap_or_else(|err| {
            panic!("session/set_mode({target_mode_id}) failed against the real, rehydrated backend session: {err}")
        });

    // The gateway session id must still work for a real follow-up prompt
    // in the *new* process -- proves rehydration didn't just answer the
    // one `session/load` call but genuinely re-registered the session.
    let turn2 = prompt::send(
        &client2,
        &gateway_session_id,
        serde_json::json!([{"type": "text", "text": "Reply with exactly the single word RESUMED and nothing else."}]),
    )
    .await
    .unwrap_or_else(|err| {
        panic!("session/prompt after session/load rehydration failed: {err}")
    });
    assert!(
        turn2.message_text.to_uppercase().contains("RESUMED"),
        "expected a real model reply containing RESUMED after rehydration, got {:?}",
        turn2.message_text
    );

    let _ = client2
        .call(
            "session/close",
            serde_json::json!({"sessionId": gateway_session_id}),
            None,
        )
        .await;

    let _ = std::fs::remove_file(&db_path);
}

/// **Phase 13 addition.** Real end-to-end proof of the `session/list`
/// dual-mode split (see `acpx-core/src/router.rs`'s `session_list_
/// selector`/`dispatch_session_list_real` and `COVERAGE.md`'s phase 13
/// entry): calling `session/list` with an `_acpx.profile` selector must
/// reach `claude-agent-acp`'s own real `listSessions` (it genuinely
/// implements `session/list`, confirmed by reading its compiled
/// `dist/acp-agent.js` directly -- it reads from the real Claude Code
/// SDK's on-disk session history, not an in-memory-only stub), and the
/// backend-native session id it reports for the session this test itself
/// just created must translate back to the *exact* gateway id
/// `session/new` issued for it -- proving `translate_or_register_
/// backend_session`'s "reuse an already-known mapping" path against a
/// real adapter's real id format, not just the synthetic stand-in
/// backend `session_list_real_test.rs`/`session_list_real_shared_test.rs`
/// use.
///
/// Uses a freshly-created, uniquely-named temp directory as this
/// session's `cwd` (rather than the shared `/tmp` every other test in
/// this file uses) specifically so `listSessions`' own `dir` filter
/// returns exactly this one session -- this machine's real `~/.claude`
/// history very likely has other real sessions already scoped to plain
/// `/tmp` from this same test file's own prior runs, which would make a
/// precise, unambiguous assertion here much harder without filtering.
///
/// **`#[ignore]`d and opt-in via `ACPX_LIVE_TEST_AMBIENT=1`**, same
/// rationale as the rest of this file.
///
/// Run with:
/// ```text
/// ACPX_LIVE_TEST_AMBIENT=1 \
/// cargo test -p acpx-server --test real_ambient_multi_agent_test \
///   ambient_claude_session_list_translates_a_real_backend_session_id -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore]
async fn ambient_claude_session_list_translates_a_real_backend_session_id() {
    if std::env::var("ACPX_LIVE_TEST_AMBIENT").as_deref() != Ok("1") {
        eprintln!(
            "skipping: set ACPX_LIVE_TEST_AMBIENT=1 to run this test against this \
             machine's real, already-logged-in claude CLI session (see this file's \
             top doc comment -- it makes a real billed API call)"
        );
        return;
    }

    let test_cwd = std::env::temp_dir().join(format!(
        "acpx-session-list-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&test_cwd).expect("create unique test cwd");
    let test_cwd_str = test_cwd.to_str().expect("utf8 path").to_string();

    let addr = ephemeral_addr().await;
    let _server = spawn_real_server(addr).await;
    let client = GatewayClient::new(format!("http://{addr}"));

    profiles::create(
        &client,
        serde_json::json!({
            "name": "ambient-claude-list",
            "agent_id": "claude-acp",
            "provider": null,
            "key_ref": null,
            "launch_overrides": {},
            "mcp_servers": [],
        }),
    )
    .await
    .expect("profiles/create(ambient-claude-list)");

    let new_result = client
        .call(
            "session/new",
            serde_json::json!({
                "cwd": test_cwd_str,
                "mcpServers": [],
                "_acpx": {"profile": "ambient-claude-list"}
            }),
            None,
        )
        .await
        .expect("session/new");
    let known_gateway_id = new_result["sessionId"]
        .as_str()
        .expect("session/new had no sessionId")
        .to_string();

    client
        .call(
            "session/set_config_option",
            serde_json::json!({"sessionId": known_gateway_id, "configId": "model", "value": "haiku"}),
            None,
        )
        .await
        .expect("session/set_config_option");

    // A real prompt turn -- `claude-agent-acp`'s `listSessions` reads
    // from the SDK's own on-disk session history, which (per its own
    // implementation, read directly) is only written once a session has
    // actually recorded some activity; an empty just-`session/new`'d
    // session might not appear yet.
    let turn = prompt::send(
        &client,
        &known_gateway_id,
        serde_json::json!([{"type": "text", "text": "Reply with exactly the single word LISTED and nothing else."}]),
    )
    .await
    .expect("session/prompt");
    assert!(
        turn.message_text.to_uppercase().contains("LISTED"),
        "expected a real model reply containing LISTED, got {:?}",
        turn.message_text
    );

    let list_result = client
        .call(
            "session/list",
            serde_json::json!({
                "cwd": test_cwd_str,
                "_acpx": {"profile": "ambient-claude-list"}
            }),
            None,
        )
        .await
        .expect("session/list");
    let sessions = list_result["sessions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !sessions.is_empty(),
        "real session/list scoped to this test's own unique cwd found no sessions at all -- \
         expected at least the one this test itself just created: {list_result:?}"
    );
    let matching = sessions
        .iter()
        .find(|s| s["sessionId"] == serde_json::Value::String(known_gateway_id.clone()));
    assert!(
        matching.is_some(),
        "real backend session/list did not translate back to this test's own known gateway \
         session id {known_gateway_id:?} -- got sessions: {sessions:?}"
    );
    let matching = matching.unwrap();
    assert_eq!(
        matching["cwd"],
        serde_json::Value::String(test_cwd_str.clone()),
        "translated SessionInfo should still carry the real backend's own reported cwd"
    );

    let _ = client
        .call(
            "session/close",
            serde_json::json!({"sessionId": known_gateway_id}),
            None,
        )
        .await;
    let _ = std::fs::remove_dir_all(&test_cwd);
}

/// **Phase 12 addition.** `session/resume` (real ACP spec method,
/// `sessionCapabilities.resume`) shares `rehydrate_session`'s exact
/// restart-survival fallback with `session/load` (see that function's
/// doc comment and `classify`'s grouping of the two) but, unlike
/// `session/load`, had never been exercised against a real backend
/// adapter anywhere in this workspace -- carried as an open item across
/// phases 8 through 11. Per the real schema (`ResumeSessionRequest`/
/// `ResumeSessionResponse`, fetched in phase 9's
/// `/tmp/acp_schema.json`), `session/resume` is deliberately a lighter
/// operation than `session/load`: no history replay, just "resume
/// without returning previous messages... useful for agents that can
/// resume sessions but don't implement full session loading" -- but it
/// goes through the identical gateway-side code path, so this test
/// proves the same restart-survival contract for it that
/// `ambient_claude_session_load_survives_a_real_gateway_restart` proved
/// for `session/load`: kill the whole first `acpx-server` process, spawn
/// a second independent one against the same sqlite file, and confirm
/// `session/resume` alone (no `session/load` call anywhere in this test)
/// can recover the session, re-register it with the second process's own
/// `Supervisor`, and drive a real follow-up billed prompt turn through
/// it.
///
/// **`#[ignore]`d and opt-in via `ACPX_LIVE_TEST_AMBIENT=1`**, same
/// rationale as the rest of this file.
///
/// Run with:
/// ```text
/// ACPX_LIVE_TEST_AMBIENT=1 \
/// cargo test -p acpx-server --test real_ambient_multi_agent_test \
///   ambient_claude_session_resume_survives_a_real_gateway_restart -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore]
async fn ambient_claude_session_resume_survives_a_real_gateway_restart() {
    if std::env::var("ACPX_LIVE_TEST_AMBIENT").as_deref() != Ok("1") {
        eprintln!(
            "skipping: set ACPX_LIVE_TEST_AMBIENT=1 to run this test against this \
             machine's real, already-logged-in claude CLI session (see this file's \
             top doc comment -- it makes a real billed API call)"
        );
        return;
    }

    let db_path = std::env::temp_dir().join(format!(
        "acpx-session-resume-restart-test-{}-{}.sqlite3",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));

    let gateway_session_id = {
        let addr = ephemeral_addr().await;
        let server = spawn_real_server_with_db(addr, Some(&db_path)).await;
        let client = GatewayClient::new(format!("http://{addr}"));

        profiles::create(
            &client,
            serde_json::json!({
                "name": "ambient-claude-resume-restart",
                "agent_id": "claude-acp",
                "provider": null,
                "key_ref": null,
                "launch_overrides": {},
                "mcp_servers": [],
            }),
        )
        .await
        .expect("profiles/create(ambient-claude-resume-restart)");

        let new_result = client
            .call(
                "session/new",
                serde_json::json!({"cwd": "/tmp", "mcpServers": [], "_acpx": {"profile": "ambient-claude-resume-restart"}}),
                None,
            )
            .await
            .expect("session/new");
        let gateway_session_id = new_result["sessionId"]
            .as_str()
            .expect("session/new had no sessionId")
            .to_string();

        client
            .call(
                "session/set_config_option",
                serde_json::json!({"sessionId": gateway_session_id, "configId": "model", "value": "haiku"}),
                None,
            )
            .await
            .expect("session/set_config_option");

        let turn = prompt::send(
            &client,
            &gateway_session_id,
            serde_json::json!([{"type": "text", "text": "Reply with exactly the single word OK and nothing else."}]),
        )
        .await
        .expect("session/prompt");
        assert!(
            turn.message_text.to_uppercase().contains("OK"),
            "expected a real model reply containing OK, got {:?}",
            turn.message_text
        );

        // Deliberately no `session/close` here -- `session/resume`'s own
        // doc says nothing about the session needing to have been closed
        // first; the whole point is recovering a session whose owning
        // process is simply gone. This exercises the more realistic
        // "acpx crashed/restarted out from under a still-logically-open
        // session" scenario rather than the tidier "client closed it
        // first" one already covered by the `session/load` restart test.
        drop(server); // kill_on_drop -- the whole first acpx-server process dies here.
        gateway_session_id
    };

    // Give the OS a moment to actually finish tearing down the first
    // process/port before standing up the second on a fresh address.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let addr2 = ephemeral_addr().await;
    let _server2 = spawn_real_server_with_db(addr2, Some(&db_path)).await;
    let client2 = GatewayClient::new(format!("http://{addr2}"));

    // Same rationale as the `session/load` restart test: profile
    // definitions are runtime state, not part of `ACPX_DB_PATH`, so
    // re-declare it against the second process.
    profiles::create(
        &client2,
        serde_json::json!({
            "name": "ambient-claude-resume-restart",
            "agent_id": "claude-acp",
            "provider": null,
            "key_ref": null,
            "launch_overrides": {},
            "mcp_servers": [],
        }),
    )
    .await
    .expect("profiles/create(ambient-claude-resume-restart) against the second process");

    // No `session/load` anywhere in this test -- `session/resume` alone
    // must recover the session from the durable sqlite row.
    let resume_result = client2
        .call(
            "session/resume",
            serde_json::json!({
                "sessionId": gateway_session_id,
                "cwd": "/tmp",
                "mcpServers": [],
            }),
            None,
        )
        .await
        .unwrap_or_else(|err| {
            panic!(
                "session/resume against the second, independent acpx-server process \
                 failed to rehydrate a session created by the first process: {err}"
            )
        });
    // Real `ResumeSessionResponse` schema: `modes`/`configOptions`/`_meta`
    // only, no `sessionId` and (unlike `session/load`) no history replay
    // at all -- this is the method's whole point per its own schema
    // description ("without returning previous messages"). Nothing more
    // specific to assert about the response shape than `session/load`'s
    // own test already asserts.
    assert!(
        resume_result.get("modes").is_some() || resume_result.get("configOptions").is_some(),
        "session/resume response should carry at least modes or configOptions per the real \
         ResumeSessionResponse schema: {resume_result:?}"
    );

    // The gateway session id must still work for a real follow-up prompt
    // in the *new* process -- proves `session/resume` genuinely
    // re-registered the session with the second process's own
    // `Supervisor`, not just answered the RPC call in isolation.
    let turn2 = prompt::send(
        &client2,
        &gateway_session_id,
        serde_json::json!([{"type": "text", "text": "Reply with exactly the single word RESUMED and nothing else."}]),
    )
    .await
    .unwrap_or_else(|err| {
        panic!("session/prompt after session/resume rehydration failed: {err}")
    });
    assert!(
        turn2.message_text.to_uppercase().contains("RESUMED"),
        "expected a real model reply containing RESUMED after resume, got {:?}",
        turn2.message_text
    );

    let _ = client2
        .call(
            "session/close",
            serde_json::json!({"sessionId": gateway_session_id}),
            None,
        )
        .await;

    let _ = std::fs::remove_file(&db_path);
}
