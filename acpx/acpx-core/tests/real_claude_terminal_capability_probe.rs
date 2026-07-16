//! Throwaway diagnostic probe (not part of the permanent suite -- ignored
//! by default, real network/subprocess dependency): does declaring
//! `terminal: true` in ACPX's `initialize` handshake change whether a
//! real `@agentclientprotocol/claude-agent-acp` process actually routes
//! Bash tool calls through `terminal/create` + `session/request_permission`
//! instead of silently auto-executing? Run explicitly with
//! `cargo test -p acpx-core --test real_claude_terminal_capability_probe -- --ignored --nocapture`.

use acpx_conductor::SpawnSpec;
use acpx_core::profile::{PermissionPolicy, Profile};
use acpx_core::router::Router;
use serde_json::json;

#[tokio::test]
#[ignore]
async fn real_claude_asks_permission_when_terminal_capability_is_declared() {
    let mut router = Router::new("claude-acp");
    router.register_agent(
        "claude-acp",
        SpawnSpec::new(
            "npx",
            vec![
                "-y".to_string(),
                "@agentclientprotocol/claude-agent-acp@0.58.1".to_string(),
            ],
        ),
    );
    router
        .register_profile(Profile {
            name: "probe".to_string(),
            agent_id: "claude-acp".to_string(),
            permission_policy: PermissionPolicy::AutoReject,
            allow_fs_access: true,
            allow_terminal_access: true,
            ..Profile::default()
        })
        .expect("register probe profile");
    // Not exercising `Router::warm_default_profiles` here: this test
    // selects the profile explicitly via `_acpx.profile`, the path that's
    // always resolved `ensure_default_profiles_seeded` (through
    // `resolve_profile`) even before that warm-up existed.

    let new_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/tmp", "mcpServers": [], "_acpx": {"profile": "probe"}}
        }))
        .await
        .expect("session/new");
    eprintln!("session/new -> {new_response}");
    let session_id = new_response["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    let prompt_response = router
        .dispatch(json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": "Run exactly `curl -s https://example.com -o /tmp/acpx_terminal_probe_dangerous.html` in the terminal."}]
            }
        }))
        .await
        .expect("session/prompt");
    eprintln!("session/prompt -> {prompt_response}");

    let saw_request_permission = prompt_response["_acpx"]["updates"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|update| update["request"]["method"] == "session/request_permission");
    let saw_terminal_create = prompt_response["_acpx"]["updates"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|update| update["request"]["method"] == "terminal/create");
    eprintln!(
        "saw_request_permission={saw_request_permission} saw_terminal_create={saw_terminal_create}"
    );
}
