//! Coverage for `Router::ensure_default_profiles_seeded`: `profiles/list`
//! (and `_acpx.profile` resolution) should surface one usable profile per
//! ACP-registry agent this host can actually launch, with zero
//! `ACPX_CONFIG_FILE`/`profiles/create` setup -- see `router.rs`'s doc
//! comment on that method for the full rationale. Uses the bundled
//! fallback registry (`acpx-registry/registry.fallback.json`: claude-acp/
//! codex-acp/gemini, all npx-distributed) -- no live network dependency,
//! same as `agents_gateway_native_test.rs`. Node/npm are present in this
//! environment (verified there too), so all three detect as `installed`.

use acpx_conductor::SpawnSpec;
use acpx_core::router::Router;
use serde_json::json;

/// Stand-in backend that answers any request with a fixed `session/new`
/// result, or `{"ok": true}` otherwise -- same pattern used throughout
/// this crate's tests (see `profile_resolution_test.rs`'s doc comment).
const STAND_IN_BACKEND_SCRIPT: &str = r#"
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"seeded-backend-abc"}}\n' "$id"
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

/// A completely unconfigured router (no `ACPX_CONFIG_FILE`, no
/// `profiles/create` ever called) still lists one profile per registry
/// agent detected `installed` on this host -- the whole point of
/// auto-seeding: `_acpx.profile` has something real to name out of the
/// box.
#[tokio::test]
async fn profiles_list_auto_seeds_one_profile_per_installed_registry_agent() {
    let mut router = Router::new("codex-acp");

    let list = router
        .dispatch(json!({"jsonrpc": "2.0", "id": 1, "method": "profiles/list", "params": {}}))
        .await
        .expect("profiles/list");
    let profiles = list["result"]["profiles"].as_array().unwrap();
    let names: Vec<&str> = profiles
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();

    for expected in ["claude-acp", "codex-acp", "gemini"] {
        assert!(
            names.contains(&expected),
            "expected auto-seeded profile {expected:?} in {names:?}"
        );
    }

    // Auto-seeded profiles are native (no provider/key) -- pure
    // ambient-env inheritance, matching `default_agent_id`'s unmanaged
    // mode, per `router.rs`'s doc comment.
    let codex_profile = profiles
        .iter()
        .find(|p| p["name"] == json!("codex-acp"))
        .expect("codex-acp seeded");
    assert_eq!(codex_profile["agent_id"], json!("codex-acp"));
    assert_eq!(codex_profile["provider"], json!(null));
    assert_eq!(codex_profile["key_ref"], json!(null));
}

/// Re-running `profiles/list` doesn't duplicate already-seeded profiles
/// (each call re-derives the same names, `ProfileStore::create`'s
/// name-collision guard silently no-ops the repeat).
#[tokio::test]
async fn repeated_profiles_list_calls_do_not_duplicate_seeded_profiles() {
    let mut router = Router::new("codex-acp");

    for _ in 0..3 {
        router
            .dispatch(json!({"jsonrpc": "2.0", "id": 1, "method": "profiles/list", "params": {}}))
            .await
            .expect("profiles/list");
    }
    let list = router
        .dispatch(json!({"jsonrpc": "2.0", "id": 2, "method": "profiles/list", "params": {}}))
        .await
        .expect("profiles/list");
    let profiles = list["result"]["profiles"].as_array().unwrap();
    let codex_count = profiles
        .iter()
        .filter(|p| p["name"] == json!("codex-acp"))
        .count();
    assert_eq!(codex_count, 1, "seeding must be idempotent: {profiles:?}");
}

/// An explicitly created profile that happens to share a name with a
/// registry agent id always wins -- auto-seeding only fills in names
/// nobody has claimed yet, it never overwrites.
#[tokio::test]
async fn explicit_profile_with_registry_agent_name_is_not_overwritten_by_seeding() {
    let mut router = Router::new("codex-acp");

    router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "profiles/create",
            "params": {
                "name": "codex-acp",
                "agent_id": "codex-acp",
                "launch_overrides": {"CODEX_TIMEOUT_MS": "99999"}
            }
        }))
        .await
        .expect("profiles/create");

    let list = router
        .dispatch(json!({"jsonrpc": "2.0", "id": 2, "method": "profiles/list", "params": {}}))
        .await
        .expect("profiles/list");
    let profiles = list["result"]["profiles"].as_array().unwrap();
    let codex_profiles: Vec<_> = profiles
        .iter()
        .filter(|p| p["name"] == json!("codex-acp"))
        .collect();
    assert_eq!(
        codex_profiles.len(),
        1,
        "explicit profile must not coexist with a duplicate seeded one"
    );
    // A synthetic seeded profile carries empty `launch_overrides` -- the
    // key being present at all (redacted, per `redact_launch_overrides`,
    // same as `router_dispatch_test.rs`'s redaction regression test)
    // proves this is still the explicitly created profile, not a
    // re-seeded default that clobbered it.
    assert_eq!(
        codex_profiles[0]["launch_overrides"]["CODEX_TIMEOUT_MS"],
        json!("***redacted***"),
        "seeding must not clobber the explicitly created profile's fields"
    );
}

/// End-to-end: `session/new` with `_acpx.profile` naming a registry agent
/// id resolves and spawns successfully with zero prior `profiles/create`/
/// `ACPX_CONFIG_FILE` setup -- the actual point of auto-seeding, not just
/// that `profiles/list` reports something. Uses a stand-in backend
/// pre-registered under the same supervisor key as the registry agent id
/// (`resolve_profile` prefers an already-registered `SpawnSpec` over a
/// fresh registry/npx lookup -- see its doc comment) so this stays fast
/// and network-free rather than actually invoking `npx`.
#[tokio::test]
async fn session_new_resolves_auto_seeded_profile_with_no_prior_provisioning() {
    let mut router = Router::new("unrelated-default");
    router.register_agent("codex-acp", stand_in_backend_spec());

    let response = router
        .dispatch(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/new",
            "params": {"cwd": "/tmp", "_acpx": {"profile": "codex-acp"}}
        }))
        .await
        .expect("session/new against auto-seeded profile");
    // The gateway mints its own session id rather than passing the
    // backend's raw one through -- presence, not the literal backend
    // value, is what proves `session/new` actually resolved and spawned
    // via the auto-seeded profile with no prior `profiles/create`/
    // `ACPX_CONFIG_FILE` setup.
    assert!(
        response["result"]["sessionId"].as_str().is_some(),
        "{response:?}"
    );
}
