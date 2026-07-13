//! **Phase 13.** Integration tests for dual-mode `session/list` (see
//! `acpx_core::router`'s `session_list_selector`/
//! `dispatch_session_list_real`/`translate_or_register_backend_session`)
//! against a tiny synthetic stand-in backend, mirroring
//! `router_dispatch_test.rs`'s established pattern -- a real ACP adapter
//! isn't guaranteed to be installed/logged in during CI, but this
//! method's real-vs-aggregate branching and backend-id -> gateway-id
//! translation logic are entirely acpx's own code, not adapter-specific
//! behavior, so a synthetic backend is the right tool to pin it down
//! precisely (the *real*-adapter side of this is covered separately by
//! `acpx-server/tests/real_ambient_multi_agent_test.rs`'s
//! `ambient_claude_session_list_*` tests, opt-in/real-billed).

use acpx_conductor::SpawnSpec;
use acpx_core::router::Router;
use serde_json::json;

/// Replies to `session/new` with a fixed backend session id
/// (`backend-abc`), to `session/list` with two canned `SessionInfo`
/// entries -- one matching `backend-abc` (so the translation path should
/// find and reuse the *already-registered* gateway id for it) and one
/// entirely novel `backend-xyz` (so the translation path should discover
/// and mint a *fresh* gateway id for it) -- and a generic `{"ok": true}`
/// for anything else (covers `session/close`, used below to prove a
/// freshly-minted id from `session/list` is genuinely dispatchable
/// afterward, not just an opaque echoed string).
const STAND_IN_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"backend-abc"}}\n' "$id"
  elif echo "$line" | grep -q 'session/list'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessions":[{"sessionId":"backend-abc","cwd":"/tmp"},{"sessionId":"backend-xyz","cwd":"/other"}]}}\n' "$id"
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

/// No `_acpx` selector at all -> acpx's own gateway-scoped aggregate,
/// unchanged in spirit from before phase 13 except it now also carries
/// `cwd` (see `SessionEntry::cwd`) so it's a strict superset of the old
/// shape, not a breaking change for any existing caller that only ever
/// read `sessionId`/`agentId`.
#[tokio::test]
async fn session_list_without_a_selector_stays_the_gateway_aggregate() {
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
        .expect("sessionId")
        .to_string();

    let list_response = router
        .dispatch(json!({"jsonrpc": "2.0", "id": 2, "method": "session/list", "params": {}}))
        .await
        .expect("session/list");
    let sessions = list_response["result"]["sessions"]
        .as_array()
        .expect("sessions array");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["sessionId"], json!(gateway_id));
    assert_eq!(sessions[0]["agentId"], json!("stand-in-agent"));
    assert_eq!(
        sessions[0]["cwd"],
        json!("/tmp"),
        "session/new's own params.cwd should now be tracked and surfaced \
         (phase 13 -- SessionEntry::cwd)"
    );
}

/// With an `_acpx.agentId` selector, `session/list` becomes a real
/// per-backend `Proxied`-shaped forward: the stand-in backend's own
/// `session/list` reply is what comes back (not acpx's registry-derived
/// aggregate), and every `SessionInfo.sessionId` in it must be translated
/// from the backend's native id into a gateway id -- reusing the already-
/// known one for `backend-abc` (registered a moment earlier by
/// `session/new`) and minting a genuinely fresh, genuinely usable one for
/// the never-before-seen `backend-xyz`.
#[tokio::test]
async fn session_list_with_a_selector_proxies_to_the_real_backend_and_translates_ids() {
    let mut router = Router::new("stand-in-agent");
    router.register_agent("stand-in-agent", stand_in_backend_spec());

    let new_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp"}
        }))
        .await
        .expect("session/new");
    let known_gateway_id = new_response["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    let list_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/list",
            "params": {"_acpx": {"agentId": "stand-in-agent"}}
        }))
        .await
        .expect("session/list");
    let sessions = list_response["result"]["sessions"]
        .as_array()
        .expect("sessions array");
    assert_eq!(
        sessions.len(),
        2,
        "the real backend's own two-entry session/list reply should pass through, \
         not acpx's own (empty, since this router only ever registered the one \
         session/new'd above) gateway aggregate: {sessions:?}"
    );

    // Entry 0: `backend-abc`, already known -- must translate back to the
    // *exact same* gateway id `session/new` issued, not a new one.
    assert_eq!(
        sessions[0]["sessionId"],
        json!(known_gateway_id),
        "an already-registered backend session id must translate back to its \
         existing gateway id, not mint a duplicate"
    );
    assert_eq!(sessions[0]["cwd"], json!("/tmp"));

    // Entry 1: `backend-xyz`, never seen before this call -- must be a
    // freshly minted gateway id, distinct from the first.
    let discovered_gateway_id = sessions[1]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();
    assert_ne!(discovered_gateway_id, known_gateway_id);
    assert_ne!(
        discovered_gateway_id, "backend-xyz",
        "the raw backend-native id must never reach the client directly -- it must \
         be translated into a gateway id, exactly like session/new's own sessionId \
         rewrite"
    );
    assert_eq!(sessions[1]["cwd"], json!("/other"));

    // **The concrete proof this isn't just a cosmetic id swap.** The
    // newly-discovered id must be genuinely dispatchable through acpx
    // afterward -- `session/close` is `Proxied` and requires resolving
    // `params.sessionId` against the live `SessionRegistry` (or, failing
    // that, `rehydrate_session`'s persistence fallback); a bare opaque
    // string that was never actually registered would fail here with
    // `UnknownSession`.
    let close_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/close",
            "params": {"sessionId": discovered_gateway_id}
        }))
        .await
        .unwrap_or_else(|err| {
            panic!(
                "session/close on a session/list-discovered gateway id failed -- it was \
                 never really registered: {err}"
            )
        });
    assert_eq!(close_response["result"], json!({"ok": true}));
}

/// An `_acpx.profile` selector (rather than a raw `agentId`) must resolve
/// through the exact same `Router::resolve_profile` machinery
/// `session/new`'s own `_acpx.profile` uses -- proves the two `_acpx`
/// conventions are genuinely unified, not two independently-maintained
/// lookup paths that could silently drift apart.
#[tokio::test]
async fn session_list_with_a_profile_selector_resolves_through_the_same_profile_machinery() {
    let mut router = Router::new("native-default-unused");
    router.register_agent("stand-in-agent", stand_in_backend_spec());
    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "profiles/create",
            "params": {"name": "list-profile", "agent_id": "stand-in-agent"}
        }))
        .await
        .expect("profiles/create");

    let list_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/list",
            "params": {"_acpx": {"profile": "list-profile"}}
        }))
        .await
        .expect("session/list");
    let sessions = list_response["result"]["sessions"]
        .as_array()
        .expect("sessions array");
    assert_eq!(
        sessions.len(),
        2,
        "an _acpx.profile selector should reach the same stand-in backend's real \
         session/list reply as an _acpx.agentId selector does: {sessions:?}"
    );
}

/// A backend that rejects `session/list` outright (a real, if unusual,
/// possibility -- e.g. an agent that advertised the capability but hit an
/// internal error) must surface as a real `RouterError`, not panic, not
/// silently succeed with an empty list, and not be confused with acpx's
/// own `UnknownAgentId`/`UnknownProfile` errors.
#[tokio::test]
async fn session_list_surfaces_a_real_backend_rejection() {
    let rejecting_script = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32001,"message":"session/list: internal error"}}\n' "$id"
done
"#;
    let mut router = Router::new("stand-in-agent");
    router.register_agent(
        "stand-in-agent",
        SpawnSpec::new("sh", vec!["-c".to_string(), rejecting_script.to_string()]),
    );

    let err = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/list",
            "params": {"_acpx": {"agentId": "stand-in-agent"}}
        }))
        .await
        .expect_err("a backend session/list rejection must surface as a RouterError");
    let message = err.to_string();
    assert!(
        message.contains("session/list"),
        "error should mention session/list, got: {message}"
    );
}
