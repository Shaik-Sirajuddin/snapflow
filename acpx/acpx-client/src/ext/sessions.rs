//! Aggregated `session/list` call -- gateway-native, no upstream ACP
//! equivalent (a plain ACP agent has no concept of "every session across
//! every backend this gateway supervises"). Phase 5 step 21.

use crate::raw::{ClientError, GatewayClient};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub session_id: String,
    pub agent_id: String,
}

/// `session/list` -- every live session across every backend this
/// gateway currently supervises, per `router.rs`'s `SessionRegistry`
/// aggregation (Phase 2 step 8).
pub async fn list(client: &GatewayClient) -> Result<Vec<SessionSummary>, ClientError> {
    let result = client
        .call("session/list", serde_json::json!({}), None)
        .await?;
    let sessions = result.get("sessions").cloned().unwrap_or_default();
    Ok(serde_json::from_value(sessions).unwrap_or_default())
}
