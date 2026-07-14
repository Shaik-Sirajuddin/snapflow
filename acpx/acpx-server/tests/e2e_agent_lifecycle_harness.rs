//! Reusable, backend-agnostic **end-to-end agent lifecycle** test harness
//! (`04-phased-plan.md` Phase 6, steps 25-26).
//!
//! Modeled on Zed's `crates/agent_servers/src/e2e_tests.rs` `common_e2e_tests!`
//! macro pattern: one shared test *body* (here, [`assert_detect_then_install`]
//! + [`assert_full_use_round_trip`]) is instantiated once per registry agent
//! id via the `agent_lifecycle_e2e_tests!` macro below, instead of three
//! hand-duplicated Claude/Codex/Gemini suites. The macro is generic over
//! "which registry agent id" (Zed's equivalent axis is "which `AgentServer`
//! impl"); a `profile` axis is deliberately not exercised here since Phase 3
//! profiles require a provider/key, which this environment has none of (see
//! the limitation note below) -- native/unmanaged mode (no `_acpx.profile`)
//! is what's actually driven per agent id.
//!
//! Each instantiation drives the full lifecycle through the real
//! `acpx_core::router::Router` -- the single dispatch entry point shared by
//! every transport (stdio/HTTP/WS/client SDK, see `COVERAGE.md`) -- across
//! three phases, per step 26:
//!
//! 1. **Detection** (Phase 2 step 6): `agents/list` must report the agent
//!    id, `agents/status` must report it `"installed"`.
//! 2. **Installation** (Phase 4 step 19): `agents/install` must confirm the
//!    `npx` runtime (real `node`/`npm` on `PATH` in this environment) and
//!    report `RuntimeConfirmed`, *and* the Node/npm-missing case is tested
//!    as a distinct expected `Err(RouterError::Install(InstallError::RuntimeMissing
//!    { .. }))`, not a panic/crash -- see
//!    `agents_install_reports_runtime_missing_as_an_error_not_a_crash`.
//! 3. **Use**: a full `session/new` -> `session/prompt` -> `session/close`
//!    round trip through the real `Router`, verifying the gateway rewrites
//!    the backend's own session id into a fresh gateway-issued one (never
//!    leaking the backend id to the "client" side of this test), and that
//!    the prompt/close calls resolve that gateway id back to the right
//!    backend before forwarding.
//!
//! ## Explicit limitation: the "use" phase is a synthetic stand-in, not a real adapter
//!
//! This environment has no real Anthropic/OpenAI/Google API keys and no
//! logged-in `claude-agent-acp`/`codex-acp`/`gemini-cli` adapter session, so
//! there is no way to actually run a real npx-resolved adapter process
//! end-to-end here (Phase 3 step 16's `COVERAGE.md` row notes the same gap
//! for `claude-agent-acp` specifically). Detection and installation *are*
//! exercised for real (real `node`/`npm` on `PATH`, real registry entries
//! from `registry.fallback.json`, real `agents/install` runtime-confirmation
//! logic) -- only the "use" phase's actual backend process is swapped out,
//! via `Router::register_agent`, for the same synthetic `sh -c '...'`
//! stand-in pattern used throughout this workspace (see
//! `acpx-core/tests/router_dispatch_test.rs`'s doc comment, and
//! `COVERAGE.md`'s "No real `npx`-installed-agent end-to-end test" gap
//! entry, which this file narrows but does not close). `register_agent`
//! overrides the supervisor's spawn spec for that agent id *after* the real
//! detect/install calls have already run against the registry-resolved
//! `npx` spec, so those two phases stay real while only the spawn used by
//! the "use" phase is synthetic.

use acpx_conductor::SpawnSpec;
use acpx_core::router::{Router, RouterError};
use serde_json::json;
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::sync::Mutex;

/// Serializes every test in this file. Only
/// [`agents_install_reports_runtime_missing_as_an_error_not_a_crash`] truly
/// *needs* this (it mutates the process-wide `PATH` env var, which would
/// otherwise race against the real `node`/`npm` detection the other tests
/// in this binary rely on), but all tests take the same lock so none of
/// them can ever observe a `PATH` mutated mid-flight by that one -- cheap
/// insurance against exactly the kind of env-var flake this pattern is
/// prone to under cargo's default per-binary test parallelism.
static SERIAL: Mutex<()> = Mutex::const_new(());

static NEXT_ID: AtomicI64 = AtomicI64::new(1);

fn next_id() -> i64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Same stand-in "backend" shape as `acpx-core/tests/router_dispatch_test.rs`
/// and `acpx-client/tests/gateway_client_test.rs`: a tiny `sh -c '...'`
/// script that echoes a canned `session/new` result carrying a fixed
/// backend session id, or a generic `{"ok": true}` result for anything
/// else, always preserving the request's own `id`.
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

fn stand_in_backend_spec() -> SpawnSpec {
    SpawnSpec::new(
        "sh",
        vec!["-c".to_string(), STAND_IN_BACKEND_SCRIPT.to_string()],
    )
}

/// Phase 1 of the lifecycle: `agents/list` reports `agent_id`,
/// `agents/status` reports it installed, `agents/install` confirms the
/// runtime -- all through the real `Router`, against the real bundled
/// `registry.fallback.json` and this environment's real `node`/`npm` on
/// `PATH`. No stand-in backend involved yet; this is the "detect ->
/// install" half of step 26, and it's exercised for real per the doc
/// comment above.
async fn assert_detect_then_install(router: &mut Router, agent_id: &str) {
    let list_request = json!({
        "jsonrpc": "2.0",
        "id": next_id(),
        "method": "agents/list",
        "params": {}
    });
    let list_response = router
        .dispatch(list_request)
        .await
        .expect("agents/list must not error");
    let agents = list_response["result"]["agents"]
        .as_array()
        .expect("agents/list result.agents is an array");
    assert!(
        agents.iter().any(|a| a["id"] == json!(agent_id)),
        "expected {agent_id} in agents/list's fallback-registry-backed result, got {agents:?}"
    );

    let status_request = json!({
        "jsonrpc": "2.0",
        "id": next_id(),
        "method": "agents/status",
        "params": {"id": agent_id}
    });
    let status_response = router
        .dispatch(status_request)
        .await
        .expect("agents/status must not error for a known registry agent id");
    assert_eq!(
        status_response["result"]["status"],
        json!("installed"),
        "node/npm are real and present on PATH in this environment (Phase 0), \
         so {agent_id}'s npx-distributed status must be \"installed\""
    );

    let install_request = json!({
        "jsonrpc": "2.0",
        "id": next_id(),
        "method": "agents/install",
        "params": {"id": agent_id}
    });
    let install_response = router
        .dispatch(install_request)
        .await
        .expect("agents/install must succeed when node/npm are really on PATH");
    assert_eq!(install_response["result"]["id"], json!(agent_id));
    let outcome = install_response["result"]["outcome"]
        .as_str()
        .expect("outcome is a string");
    assert!(
        outcome.contains("RuntimeConfirmed"),
        "expected the npx runtime-confirmation outcome, got {outcome}"
    );
}

/// Phase 2 of the lifecycle: a full `session/new` -> `session/prompt` ->
/// `session/close` round trip through the real `Router`, using the
/// synthetic stand-in backend registered under `agent_id` (see this file's
/// top doc comment for why -- no real adapter/API key is available in this
/// environment). Must be called *after* `assert_detect_then_install` so
/// the earlier phase still ran against the real registry-resolved `npx`
/// spec, not this override.
async fn assert_full_use_round_trip(router: &mut Router, agent_id: &str) {
    router.register_agent(agent_id, stand_in_backend_spec());

    let new_request = json!({
        "jsonrpc": "2.0",
        "id": next_id(),
        "method": "session/new",
        "params": {"cwd": "/tmp"}
    });
    let new_response = router
        .dispatch(new_request)
        .await
        .expect("session/new through the stand-in backend");
    let gateway_session_id = new_response["result"]["sessionId"]
        .as_str()
        .expect("session/new result.sessionId is a string")
        .to_string();
    // The gateway must substitute its own session id -- the backend's raw
    // "backend-abc" id must never reach this side of the Router.
    assert_ne!(gateway_session_id, "backend-abc");

    let prompt_request = json!({
        "jsonrpc": "2.0",
        "id": next_id(),
        "method": "session/prompt",
        "params": {
            "sessionId": gateway_session_id,
            "prompt": [{"type": "text", "text": "hello from the e2e lifecycle harness"}]
        }
    });
    let prompt_response = router
        .dispatch(prompt_request)
        .await
        .expect("session/prompt resolving the gateway session id");
    assert_eq!(prompt_response["result"]["ok"], json!(true));

    let close_request = json!({
        "jsonrpc": "2.0",
        "id": next_id(),
        "method": "session/close",
        "params": {"sessionId": gateway_session_id}
    });
    let close_response = router
        .dispatch(close_request)
        .await
        .expect("session/close resolving the gateway session id");
    assert_eq!(close_response["result"]["ok"], json!(true));
}

/// Instantiates the shared lifecycle body once for a given real registry
/// agent id -- the `common_e2e_tests!`-equivalent macro from step 25. Add a
/// new invocation here to cover another registry agent with zero
/// duplicated test logic.
macro_rules! agent_lifecycle_e2e_tests {
    ($mod_name:ident, $agent_id:expr) => {
        mod $mod_name {
            use super::*;

            #[tokio::test]
            async fn detect_install_then_use_round_trip() {
                let _serial = SERIAL.lock().await;
                let mut router = Router::new($agent_id);
                assert_detect_then_install(&mut router, $agent_id).await;
                assert_full_use_round_trip(&mut router, $agent_id).await;
            }
        }
    };
}

// Real registry agent ids from `acpx-registry/registry.fallback.json`
// (Phase 6 step 25/26's explicit "once per Claude/Codex/Gemini" scope).
agent_lifecycle_e2e_tests!(claude, "claude-acp");
agent_lifecycle_e2e_tests!(codex, "codex-acp");
agent_lifecycle_e2e_tests!(gemini, "gemini");

/// Step 26's other explicit lifecycle requirement: the Node/npm-missing
/// case must be "a distinct expected failure not a crash". Simulated by
/// temporarily clearing `PATH` for the duration of one `agents/install`
/// call (real npx-distributed agents have no other way to be "missing"
/// their runtime in an environment that otherwise has node/npm installed)
/// -- serialized against every other test in this file via `SERIAL` so
/// no concurrently-running test observes the broken `PATH`, and restored
/// unconditionally before the assertion so a failed assertion can't leak a
/// broken `PATH` into any test that runs after this one.
#[tokio::test]
async fn agents_install_reports_runtime_missing_as_an_error_not_a_crash() {
    let _serial = SERIAL.lock().await;
    let mut router = Router::new("codex-acp");

    let saved_path = std::env::var_os("PATH");
    // SAFETY: serialized via `SERIAL` above -- no other test in this
    // binary can be running concurrently to observe (or race to restore)
    // this process-wide mutation.
    unsafe {
        std::env::set_var("PATH", "");
    }

    let install_request = json!({
        "jsonrpc": "2.0",
        "id": next_id(),
        "method": "agents/install",
        "params": {"id": "codex-acp"}
    });
    let result = router.dispatch(install_request).await;

    // Restore PATH unconditionally before asserting anything, so a failed
    // assertion below can't leave every subsequent test in this binary
    // running with an empty PATH.
    unsafe {
        match &saved_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
    }

    let err = result.expect_err(
        "agents/install must surface a missing node/npm runtime as an Err, not panic or hang",
    );
    assert!(
        matches!(
            err,
            RouterError::Install(acpx_registry::InstallError::RuntimeMissing { .. })
        ),
        "expected RouterError::Install(InstallError::RuntimeMissing), got {err:?}"
    );
}
