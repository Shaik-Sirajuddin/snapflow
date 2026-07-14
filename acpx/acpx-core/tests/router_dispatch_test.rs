//! Integration test for `acpx_core::router::Router::dispatch` against a
//! tiny synthetic stand-in "backend" (a `sh` one-liner), since a real ACP
//! adapter (codex-acp/claude-agent-acp) isn't guaranteed to be installed
//! or logged in during CI. The script echoes back a crafted `session/new`
//! result carrying a `sessionId`, and a generic `{"ok": true}` result for
//! anything else -- enough to exercise the hybrid `session/new` ->
//! gateway-id-registration path and the proxied `session/prompt` ->
//! session-resolution path end to end.

use acpx_conductor::SpawnSpec;
use acpx_core::router::Router;
use serde_json::json;

/// Reads newline-delimited JSON-RPC requests, replies with a canned
/// `session/new` result (fixed backend session id `backend-abc`) or a
/// generic `{"ok": true}` result for any other method, always echoing the
/// request's own `id`.
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

#[tokio::test]
async fn session_new_registers_gateway_id_and_hides_backend_id() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/new",
        "params": {"cwd": "/tmp"}
    });
    let response = router
        .dispatch(request)
        .await
        .expect("session/new dispatch");

    let session_id = response["result"]["sessionId"]
        .as_str()
        .expect("sessionId present");
    // The client must never see the backend's own session id -- a fresh
    // gateway-issued id is substituted in place.
    assert_ne!(session_id, "backend-abc");
}

#[tokio::test]
async fn session_prompt_resolves_gateway_id_to_backend_id() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());

    let new_request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/new",
        "params": {"cwd": "/tmp"}
    });
    let new_response = router.dispatch(new_request).await.expect("session/new");
    let gateway_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let prompt_request = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/prompt",
        "params": {"sessionId": gateway_id, "prompt": [{"type": "text", "text": "hi"}]}
    });
    let prompt_response = router
        .dispatch(prompt_request)
        .await
        .expect("session/prompt");
    assert_eq!(prompt_response["result"]["ok"], json!(true));
}

#[tokio::test]
async fn session_prompt_with_unknown_session_errors() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());

    let prompt_request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/prompt",
        "params": {"sessionId": "not-a-real-session", "prompt": []}
    });
    assert!(router.dispatch(prompt_request).await.is_err());
}

#[tokio::test]
async fn session_list_aggregates_registered_sessions() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());

    let new_request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/new",
        "params": {"cwd": "/tmp"}
    });
    router.dispatch(new_request).await.expect("session/new");

    let list_request = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/list",
        "params": {}
    });
    let list_response = router.dispatch(list_request).await.expect("session/list");
    let sessions = list_response["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["agentId"], json!("stand-in-agent"));
}

/// **Regression test for a real bug**: `dispatch_proxied`'s `session/close`
/// handling used to only persist the close to sqlite and never evict the
/// gateway session id from the in-memory `SessionRegistry` -- meaning
/// every session ever opened stayed in `session/list`'s output forever
/// (and the registry's backing `HashMap` grew without bound over a
/// long-running daemon's lifetime). Proves both halves of the fix: the
/// session disappears from `session/list`, and a subsequent
/// `session/prompt` against the now-closed gateway session id is
/// rejected rather than silently forwarded to the backend.
#[tokio::test]
async fn session_close_evicts_session_from_registry_and_rejects_further_use() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());

    let new_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .await
        .expect("session/new");
    let gateway_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    // Sanity: session is visible before close.
    let list_before = router
        .dispatch(json!({"jsonrpc": "2.0", "id": 2, "method": "session/list", "params": {}}))
        .await
        .expect("session/list");
    assert_eq!(
        list_before["result"]["sessions"].as_array().unwrap().len(),
        1
    );

    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/close",
            "params": {"sessionId": gateway_id}
        }))
        .await
        .expect("session/close");

    let list_after = router
        .dispatch(json!({"jsonrpc": "2.0", "id": 4, "method": "session/list", "params": {}}))
        .await
        .expect("session/list");
    assert_eq!(
        list_after["result"]["sessions"].as_array().unwrap().len(),
        0,
        "closed session must be evicted from session/list, not linger forever"
    );

    let prompt_after_close = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 5, "method": "session/prompt",
            "params": {"sessionId": gateway_id, "prompt": []}
        }))
        .await;
    assert!(
        prompt_after_close.is_err(),
        "session/prompt against a closed gateway session id must error, not silently proxy"
    );
}

/// Same regression, exercised through `dispatch_shared`/`SharedRouterHandle`
/// -- the real multi-agent-concurrency dispatch path every transport
/// (`acpx-server`'s HTTP/WS/stdio) actually uses in production. Kept as a
/// separate test rather than assuming the plain `dispatch` path above
/// proves this one too, since the fix had to be applied independently in
/// `dispatch_proxied_shared` (see that function's own doc comment).
#[tokio::test]
async fn dispatch_shared_session_close_evicts_session_too() {
    use acpx_core::router::dispatch_shared;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    let router = Arc::new(Mutex::new(router));

    let new_response = dispatch_shared(
        &router,
        json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}}),
    )
    .await
    .expect("session/new");
    let gateway_id = new_response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    dispatch_shared(
        &router,
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/close",
            "params": {"sessionId": gateway_id}
        }),
    )
    .await
    .expect("session/close");

    let list_after = dispatch_shared(
        &router,
        json!({"jsonrpc": "2.0", "id": 3, "method": "session/list", "params": {}}),
    )
    .await
    .expect("session/list");
    assert_eq!(
        list_after["result"]["sessions"].as_array().unwrap().len(),
        0,
        "closed session must be evicted from session/list via dispatch_shared too"
    );

    let prompt_after_close = dispatch_shared(
        &router,
        json!({
            "jsonrpc": "2.0", "id": 4, "method": "session/prompt",
            "params": {"sessionId": gateway_id, "prompt": []}
        }),
    )
    .await;
    assert!(
        prompt_after_close.is_err(),
        "session/prompt against a closed gateway session id must error via dispatch_shared too"
    );
}

/// **Regression test for a real bug**: `dispatch_native`'s
/// `"profiles/delete"` arm used to only remove the `ProfileStore` entry,
/// never stopping whatever backend process had been spawned for that
/// profile (under supervisor key `"profile:<name>"`) -- an orphaned OS
/// child process leaked forever on every delete of a profile that had
/// ever actually been used. Proves the process is genuinely running
/// after `session/new`, then genuinely stopped after `profiles/delete`.
#[tokio::test]
async fn profiles_delete_stops_the_profiles_running_backend_process() {
    use acpx_conductor::supervisor::ProcessStatus;

    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());

    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "profiles/create",
            "params": {"name": "leak-test", "agent_id": "stand-in-agent"}
        }))
        .await
        .expect("profiles/create");

    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": {"cwd": "/tmp", "_acpx": {"profile": "leak-test"}}
        }))
        .await
        .expect("session/new");

    assert_eq!(
        router.process_status("profile:leak-test"),
        ProcessStatus::Running,
        "backend process should be running for the profile after session/new"
    );

    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 3, "method": "profiles/delete",
            "params": {"name": "leak-test"}
        }))
        .await
        .expect("profiles/delete");

    assert_eq!(
        router.process_status("profile:leak-test"),
        ProcessStatus::NotStarted,
        "profiles/delete must stop the profile's backend process, not leak it forever"
    );
}

/// **Regression test for a real bug**: `profiles/create`/`update`/`list`
/// used to echo `launch_overrides` values back byte-for-byte -- for a
/// gateway meant to serve multiple concurrent clients sharing one
/// `ACPX_AUTH_TOKEN`, that meant any client able to call `profiles/list`
/// could read another profile's raw secrets (e.g. an `ANTHROPIC_API_KEY`
/// set via that documented escape hatch) in plaintext. Proves values are
/// now masked in every response that echoes a profile back, while the
/// stored profile itself (and therefore real backend spawning) still
/// gets the real, unredacted value.
#[tokio::test]
async fn launch_overrides_values_are_redacted_in_every_profile_response() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());

    let create = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "profiles/create",
            "params": {
                "name": "secret-holder",
                "agent_id": "stand-in-agent",
                "launch_overrides": {"ANTHROPIC_API_KEY": "sk-real-secret-value"}
            }
        }))
        .await
        .expect("profiles/create");
    assert_eq!(
        create["result"]["launch_overrides"]["ANTHROPIC_API_KEY"],
        json!("***redacted***"),
        "profiles/create must not echo the raw secret value back"
    );

    let list = router
        .dispatch(json!({"jsonrpc": "2.0", "id": 2, "method": "profiles/list", "params": {}}))
        .await
        .expect("profiles/list");
    let listed = &list["result"]["profiles"][0];
    assert_eq!(
        listed["launch_overrides"]["ANTHROPIC_API_KEY"],
        json!("***redacted***"),
        "profiles/list must not leak a raw secret to any other client that can list profiles"
    );

    let update = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 3, "method": "profiles/update",
            "params": {
                "name": "secret-holder",
                "agent_id": "stand-in-agent",
                "launch_overrides": {"ANTHROPIC_API_KEY": "sk-real-secret-value"}
            }
        }))
        .await
        .expect("profiles/update");
    assert_eq!(
        update["result"]["launch_overrides"]["ANTHROPIC_API_KEY"],
        json!("***redacted***"),
        "profiles/update must not echo the raw secret value back either"
    );

    // The stored profile itself must still carry the real value -- this
    // is a response-serialization-only redaction, not data loss. Verified
    // indirectly: `session/new` against this profile must still be able
    // to inject the real env var into the spawned backend, which the
    // observing stand-in backend in `profile_resolution_test.rs` already
    // proves for the general env-injection path; here we just confirm
    // `session/new` doesn't error, i.e. the profile is still fully usable.
    let session = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 4, "method": "session/new",
            "params": {"cwd": "/tmp", "_acpx": {"profile": "secret-holder"}}
        }))
        .await;
    assert!(
        session.is_ok(),
        "profile must remain fully usable after redaction-on-echo: {session:?}"
    );
}
