//! Client-plane integration coverage for durable admin state.

use std::collections::BTreeMap;
use std::sync::Arc;

use acpx_core::router::{dispatch_shared_for_tenant, Router, RouterError};
use acpx_core::{
    AdminOps, AgentEnablement, CustomAgent, CustomAgentStore, PersistenceStore, TenantId,
};
use serde_json::json;
use tokio::sync::Mutex;

const CUSTOM_BACKEND: &str = r#"
echo started > "$CUSTOM_MARKER"
test "$CUSTOM_ENV" = "present" || exit 31
test -f workspace-marker || exit 32
while IFS= read -r line; do
  id=$(echo "$line" | grep -o '"id":[0-9]*' | head -1 | cut -d: -f2)
  if echo "$line" | grep -q 'session/new'; then
    printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"custom-backend"}}\n' "$id"
  else
    printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
  fi
done
"#;

fn custom_agent(marker: &std::path::Path, cwd: &std::path::Path) -> CustomAgent {
    CustomAgent {
        id: "custom-shell".to_owned(),
        name: "Custom Shell".to_owned(),
        command: "sh".to_owned(),
        args: vec!["-c".to_owned(), CUSTOM_BACKEND.to_owned()],
        env: BTreeMap::from([
            ("CUSTOM_ENV".to_owned(), "present".to_owned()),
            ("CUSTOM_MARKER".to_owned(), marker.display().to_string()),
        ]),
        cwd: Some(cwd.display().to_string()),
    }
}

#[tokio::test]
async fn client_plane_merges_custom_agents_and_enforces_enablement_before_spawn() {
    let workspace = tempfile::tempdir().expect("custom workspace");
    std::fs::write(workspace.path().join("workspace-marker"), "ok").expect("workspace marker");
    let process_marker = workspace.path().join("process-started");
    let store = PersistenceStore::open_in_memory().expect("durable store");
    let admin = AdminOps::new(
        AgentEnablement::new(store.clone()),
        CustomAgentStore::new(store.clone()),
        std::iter::empty::<String>(),
    );
    admin
        .create_custom_agent(custom_agent(&process_marker, workspace.path()))
        .await
        .expect("create custom agent");
    admin
        .set_enabled("custom-shell", false)
        .await
        .expect("disable custom agent");

    let router = Arc::new(Mutex::new(
        Router::new("default-agent").with_persistence(store),
    ));
    let tenant = TenantId::default_tenant();
    let new_request = || {
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "session/new",
            "params": {
                "cwd": workspace.path(),
                "mcpServers": [],
                "_acpx": {"agentId": "custom-shell"}
            }
        })
    };
    dispatch_shared_for_tenant(
        &router,
        &tenant,
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "profiles/create",
            "params": {"name": "custom-profile", "agent_id": "custom-shell"}
        }),
    )
    .await
    .expect("create profile for custom agent");

    let disabled = dispatch_shared_for_tenant(&router, &tenant, new_request())
        .await
        .expect_err("disabled custom agent is rejected");
    assert!(matches!(
        disabled,
        RouterError::AgentDisabled(ref id) if id == "custom-shell"
    ));
    assert!(
        !process_marker.exists(),
        "disabled session/new must not start the custom process"
    );
    let disabled_profile = dispatch_shared_for_tenant(
        &router,
        &tenant,
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "session/new",
            "params": {
                "cwd": workspace.path(),
                "mcpServers": [],
                "_acpx": {"profile": "custom-profile"}
            }
        }),
    )
    .await
    .expect_err("disabled custom profile is rejected");
    assert!(matches!(
        disabled_profile,
        RouterError::AgentDisabled(ref id) if id == "custom-shell"
    ));
    assert!(
        !process_marker.exists(),
        "disabled profile session/new must not resolve or start the custom process"
    );
    let disabled_list = dispatch_shared_for_tenant(
        &router,
        &tenant,
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "session/list",
            "params": {"_acpx": {"agentId": "custom-shell"}}
        }),
    )
    .await
    .expect_err("disabled session/list selector is rejected");
    assert!(matches!(
        disabled_list,
        RouterError::AgentDisabled(ref id) if id == "custom-shell"
    ));
    assert!(
        !process_marker.exists(),
        "disabled session/list must not start the custom process"
    );

    admin
        .set_enabled("custom-shell", true)
        .await
        .expect("enable custom agent");
    let list = dispatch_shared_for_tenant(
        &router,
        &tenant,
        json!({"jsonrpc": "2.0", "id": 8, "method": "agents/list", "params": {}}),
    )
    .await
    .expect("agents/list");
    let entry = list["result"]["agents"]
        .as_array()
        .expect("agents array")
        .iter()
        .find(|entry| entry["id"] == "custom-shell")
        .expect("custom agent in client-plane list");
    assert_eq!(entry["source"], json!("custom"));
    assert_eq!(entry["enabled"], json!(true));
    assert_eq!(entry["status"], json!("configured"));
    let status = dispatch_shared_for_tenant(
        &router,
        &tenant,
        json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "agents/status",
            "params": {"id": "custom-shell"}
        }),
    )
    .await
    .expect("custom agents/status");
    assert_eq!(status["result"]["status"], json!("configured"));

    let created = dispatch_shared_for_tenant(&router, &tenant, new_request())
        .await
        .expect("enabled custom session/new");
    let session_id = created["result"]["sessionId"]
        .as_str()
        .expect("gateway session id")
        .to_owned();
    assert!(
        process_marker.exists(),
        "custom process must receive its configured env and cwd"
    );

    admin
        .set_enabled("custom-shell", false)
        .await
        .expect("disable after session creation");
    let prompt = dispatch_shared_for_tenant(
        &router,
        &tenant,
        json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "session/prompt",
            "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "continue"}]}
        }),
    )
    .await
    .expect("existing session remains usable");
    assert_eq!(prompt["result"]["ok"], json!(true));

    admin
        .delete_custom_agent("custom-shell")
        .await
        .expect("delete custom agent");
    let deleted = dispatch_shared_for_tenant(&router, &tenant, new_request())
        .await
        .expect_err("deleted custom definition cannot reuse its stale supervisor spec");
    assert!(matches!(
        deleted,
        RouterError::UnknownAgentId(ref id) if id == "custom-shell"
    ));
}
