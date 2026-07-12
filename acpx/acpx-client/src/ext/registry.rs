//! Registry query + client-initiated install calls (`agents/*`).
//! Phase 5 steps 21-22.

use crate::raw::{ClientError, GatewayClient};
use serde_json::json;

/// `agents/list` -- the registry's agent catalogue, each entry annotated
/// with this gateway's live detection status (Phase 2 steps 6-7).
pub async fn agents_list(client: &GatewayClient) -> Result<Vec<serde_json::Value>, ClientError> {
    let result = client.call("agents/list", json!({}), None).await?;
    Ok(result
        .get("agents")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default())
}

/// `agents/status` for one agent id.
pub async fn agents_status(
    client: &GatewayClient,
    agent_id: &str,
) -> Result<serde_json::Value, ClientError> {
    client
        .call("agents/status", json!({ "id": agent_id }), None)
        .await
}

/// `ext::registry::install(agent_id)` per the task draft's "runtime
/// installer in acpx, with initialized from acpx client" -- the
/// client-initiated trigger point step 22 describes.
///
/// **Not a polling/streamed job** -- this is a single call that blocks
/// until the gateway's own `agents/install` (Phase 4 step 19) returns,
/// which is itself synchronous today (confirm-runtime-on-PATH for
/// `npx`/`uvx`, or a full download+extract for `binary`). A real
/// progress/job model for a slow first install is an explicitly
/// undecided open risk (`05-open-risks.md`'s "client-initiated installer
/// needs a progress/job model" item) -- not resolved by this function;
/// documented here rather than silently assumed away. Callers that need
/// progress feedback today have no better option than a UI-level
/// "installing..." spinner around this call.
pub async fn install(
    client: &GatewayClient,
    agent_id: &str,
) -> Result<serde_json::Value, ClientError> {
    client
        .call("agents/install", json!({ "id": agent_id }), None)
        .await
}
