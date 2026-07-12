//! Phase 6 (`04-phased-plan.md` step 24): gateway-native surface that has
//! no upstream ACP-spec equivalent to test against, exercised through the
//! real `Router::dispatch` entry point rather than any store's own unit
//! tests -- see `COVERAGE.md`'s "Gaps" section for why these specific
//! cases (Node/npm-missing status, `agents/install` error paths,
//! `profiles/update`-on-missing, empty/multi-agent `session/list`) were
//! still open going into this phase. Same synthetic `sh -c '...'`
//! stand-in-backend trick as `router_dispatch_test.rs` (see that file's
//! doc comment) for anything that needs a fake backend process.
//!
//! One test here (`agents_status_reports_runtime_missing_...`) rewrites
//! the process-wide `PATH` env var to simulate a machine with no
//! `node`/`npm` on it, per `05-open-risks.md`'s "Node not found" status
//! note. Every `#[tokio::test]` function in this file is compiled into
//! one shared test binary and, by default, Rust's test harness runs them
//! concurrently on separate OS threads -- a bare `PATH` mutation in one
//! test would race with any other test in this same file that spawns a
//! `sh` stand-in backend (which itself needs to resolve `sh` via `PATH`).
//! `serialize()` below gives every test in this file a shared lock so
//! that race can't happen, at the cost of running this one file's tests
//! sequentially rather than in parallel -- acceptable since it's a small,
//! fast file, and no other test file in the workspace is affected (each
//! integration test file compiles to its own separate process).

use acpx_conductor::SpawnSpec;
use acpx_core::profile::Profile;
use acpx_core::router::Router;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

/// Acquire the whole-file serialization lock described in the module doc
/// comment. Recovers from a poisoned lock (an earlier test panicking
/// mid-body) rather than cascading the poison into every later test --
/// there's no shared mutable data behind this lock, it only exists to
/// order execution.
fn serialize() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Minimal stand-in backend: replies to `session/new` with a fixed
/// `sessionId`, and `{"ok": true}` to anything else. Same shape as
/// `router_dispatch_test.rs`'s `STAND_IN_BACKEND_SCRIPT`.
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

fn sample_profile(name: &str, agent_id: &str) -> Profile {
    Profile {
        name: name.to_string(),
        agent_id: agent_id.to_string(),
        provider: None,
        key_ref: None,
        launch_overrides: HashMap::new(),
        mcp_servers: vec![],
    }
}

// ---------------------------------------------------------------------
// Node/npm-missing status distinction (agents/status)
// ---------------------------------------------------------------------

/// `codex-acp` is `npx`-only in the bundled fallback registry (see
/// `agents_gateway_native_test.rs`'s sibling "installed" case, which
/// asserts the opposite outcome against this same environment's *real*
/// `PATH`). With `PATH` rewritten to contain nothing, `detect::detect`'s
/// `node`/`npm` lookup fails and `agents/status` must report
/// `runtime_missing` rather than `installed` or a generic error --
/// `05-open-risks.md` calls this out explicitly as a status a real
/// deployment needs to distinguish ("Node not found" vs. "not yet
/// installed").
#[tokio::test]
async fn agents_status_reports_runtime_missing_when_node_and_npm_absent_from_path() {
    let _guard = serialize();
    let original_path = std::env::var("PATH").ok();
    std::env::set_var("PATH", "");

    let mut router = Router::new("codex-acp");
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "agents/status",
        "params": {"id": "codex-acp"}
    });
    let outcome = router.dispatch(request).await;

    // Restore PATH before asserting so a failed assertion (or any later
    // test in this binary) never runs with a broken PATH.
    match original_path {
        Some(p) => std::env::set_var("PATH", p),
        None => std::env::remove_var("PATH"),
    }

    let response = outcome.expect("agents/status should succeed, not error, on a missing runtime");
    assert_eq!(response["result"]["id"], json!("codex-acp"));
    assert_eq!(response["result"]["status"], json!("runtime_missing"));
}

/// Same `PATH` rewrite, but through `agents/list`'s aggregate view rather
/// than the single-agent `agents/status` lookup -- both entry points
/// share `detect::detect` under the hood, but `agents/list` has never had
/// its per-entry status verified against a missing runtime specifically.
#[tokio::test]
async fn agents_list_reflects_runtime_missing_for_every_npx_only_agent_when_path_is_empty() {
    let _guard = serialize();
    let original_path = std::env::var("PATH").ok();
    std::env::set_var("PATH", "");

    let mut router = Router::new("codex-acp");
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": "agents/list", "params": {}});
    let outcome = router.dispatch(request).await;

    match original_path {
        Some(p) => std::env::set_var("PATH", p),
        None => std::env::remove_var("PATH"),
    }

    let response = outcome.expect("agents/list");
    let agents = response["result"]["agents"].as_array().unwrap();
    let claude = agents
        .iter()
        .find(|a| a["id"] == json!("claude-acp"))
        .expect("claude-acp present in the fallback registry");
    let codex = agents
        .iter()
        .find(|a| a["id"] == json!("codex-acp"))
        .expect("codex-acp present in the fallback registry");
    assert_eq!(claude["status"], json!("runtime_missing"));
    assert_eq!(codex["status"], json!("runtime_missing"));
}

// ---------------------------------------------------------------------
// agents/install error paths
// ---------------------------------------------------------------------

#[tokio::test]
async fn agents_install_with_unknown_agent_id_errors() {
    let _guard = serialize();
    let mut router = Router::new("codex-acp");
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "agents/install",
        "params": {"id": "not-a-real-agent"}
    });
    let err = router
        .dispatch(request)
        .await
        .expect_err("install of an unregistered agent id must error, not silently no-op");
    assert!(
        err.to_string().contains("not-a-real-agent"),
        "error should name the unknown agent id, got: {err}"
    );
}

#[tokio::test]
async fn agents_install_with_missing_id_param_errors() {
    let _guard = serialize();
    let mut router = Router::new("codex-acp");
    // No `params.id` at all -- distinct failure mode from an unknown-but
    // present id (`RouterError::MissingAgentId` vs. `UnknownAgentId`).
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "agents/install",
        "params": {}
    });
    assert!(router.dispatch(request).await.is_err());
}

// ---------------------------------------------------------------------
// profiles/* CRUD error paths not already covered by
// `profile_resolution_test.rs`'s `profiles_crud_round_trips_via_dispatch`
// (which covers duplicate-create and delete-missing already -- see
// COVERAGE.md's Phase 3 row for step 14). `profiles/update` on a name
// that was never created is the one gap left.
// ---------------------------------------------------------------------

#[tokio::test]
async fn profiles_update_on_missing_name_errors_via_dispatch() {
    let _guard = serialize();
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "profiles/update",
        "params": {"name": "never-created", "agent_id": "stand-in-agent"}
    });
    let err = router
        .dispatch(request)
        .await
        .expect_err("updating a profile that was never created must error");
    assert!(
        err.to_string().contains("never-created"),
        "error should name the missing profile, got: {err}"
    );
}

#[tokio::test]
async fn profiles_delete_on_missing_name_errors_twice_in_a_row() {
    let _guard = serialize();
    // `profile_resolution_test.rs` already covers one delete-on-missing
    // case (deleting the same profile twice); this covers the simpler
    // "never existed at all" case as its own standalone assertion, so it
    // doesn't depend on a prior create succeeding first.
    let mut router = Router::new("stand-in-agent");
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "profiles/delete",
        "params": {"name": "was-never-created"}
    });
    assert!(router.dispatch(request).await.is_err());
}

// ---------------------------------------------------------------------
// session/list edge cases
// ---------------------------------------------------------------------

#[tokio::test]
async fn session_list_on_a_fresh_router_is_empty_not_an_error() {
    let _guard = serialize();
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());

    let request = json!({"jsonrpc": "2.0", "id": 1, "method": "session/list", "params": {}});
    let response = router
        .dispatch(request)
        .await
        .expect("session/list on an empty registry should still succeed");
    let sessions = response["result"]["sessions"].as_array().unwrap();
    assert!(sessions.is_empty());
}

#[tokio::test]
async fn session_list_aggregates_across_multiple_distinct_agents() {
    let _guard = serialize();
    // Native mode always routes to one fixed `default_agent_id`, so the
    // only full-dispatch way to get two *different* `agentId` values into
    // the session registry is via two profiles pointing at two
    // separately-registered backends -- `resolve_profile` registers each
    // profile's sessions under a `profile:<name>` supervisor key distinct
    // from the underlying agent id it wraps.
    let mut router = Router::new("stand-in-agent-a");
    router.register_agent("stand-in-agent-a", stand_in_backend_spec());
    router.register_agent("stand-in-agent-b", stand_in_backend_spec());

    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "profiles/create",
            "params": sample_profile("proj-a", "stand-in-agent-a")
        }))
        .await
        .expect("profiles/create proj-a");
    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "profiles/create",
            "params": sample_profile("proj-b", "stand-in-agent-b")
        }))
        .await
        .expect("profiles/create proj-b");

    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/new",
            "params": {"cwd": "/tmp", "_acpx": {"profile": "proj-a"}}
        }))
        .await
        .expect("session/new via proj-a");
    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 4, "method": "session/new",
            "params": {"cwd": "/tmp", "_acpx": {"profile": "proj-b"}}
        }))
        .await
        .expect("session/new via proj-b");

    let list_response = router
        .dispatch(json!({"jsonrpc": "2.0", "id": 5, "method": "session/list", "params": {}}))
        .await
        .expect("session/list");
    let sessions = list_response["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    let agent_ids: Vec<&str> = sessions
        .iter()
        .map(|s| s["agentId"].as_str().unwrap())
        .collect();
    assert!(agent_ids.contains(&"profile:proj-a"));
    assert!(agent_ids.contains(&"profile:proj-b"));
}
