//! Aggregated `session/list` call -- gateway-native, no upstream ACP
//! equivalent (a plain ACP agent has no concept of "every session across
//! every backend this gateway supervises"). Phase 5 step 21.

use crate::raw::{ClientError, GatewayClient};
use crate::Gateway;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub session_id: String,
    /// **Real, previously-latent bug fixed here (found while writing
    /// `panel-rust`'s recovery/import round-trip test)**: this field is
    /// only present on the wire for the *aggregate* `session/list`
    /// response (`list`/`list_gateway`, `router.rs`'s gateway-scoped
    /// `SessionRegistry` view). The real-backend-forwarded per-agent/
    /// per-profile response (`list_for_agent`, `router.rs`'s
    /// `dispatch_session_list_real`) forwards the backend's own native
    /// ACP `SessionInfo` shape verbatim, which has no `agentId` field at
    /// all -- gateway-id-to-agent mapping is bookkeeping the *gateway*
    /// owns, not something a plain ACP agent's own `session/list`
    /// response could ever include. Before this fix, `agent_id` being
    /// non-optional meant `serde_json::from_value(sessions)` failed for
    /// *every single entry* whenever it was missing, and `parse_sessions`'s
    /// `unwrap_or_default()` silently turned that failure into an empty
    /// `Vec` -- meaning `list_for_agent` always returned an empty list,
    /// unconditionally, regardless of how many real sessions existed.
    /// `#[serde(default)]` lets deserialization succeed either way;
    /// [`list_for_agent`] backfills this from its own `agent_id`
    /// parameter afterward, since it already knows the answer by
    /// construction.
    #[serde(default)]
    pub agent_id: String,
    /// Present when querying one backend's real ACP `session/list`;
    /// gateway-aggregated lists do not have an authoritative title.
    #[serde(default)]
    pub title: Option<String>,
    /// ACP's last-update token. Like `title`, this is backend metadata
    /// returned by selector-based lists rather than the aggregate registry.
    #[serde(default)]
    pub updated_at: Option<String>,
}

fn parse_sessions(result: serde_json::Value) -> Vec<SessionSummary> {
    let sessions = result.get("sessions").cloned().unwrap_or_default();
    serde_json::from_value(sessions).unwrap_or_default()
}

/// `session/list` -- every live session across every backend this
/// gateway currently supervises, per `router.rs`'s `SessionRegistry`
/// aggregation (Phase 2 step 8).
pub async fn list(client: &GatewayClient) -> Result<Vec<SessionSummary>, ClientError> {
    let result = client
        .call("session/list", serde_json::json!({}), None)
        .await?;
    Ok(parse_sessions(result))
}

/// The same aggregate list via the transport-neutral [`Gateway`] facade.
/// Consumers that need WebSocket-primary behavior must use this instead of
/// the HTTP-only [`list`] compatibility helper above.
pub async fn list_gateway(gateway: &Gateway) -> Result<Vec<SessionSummary>, ClientError> {
    let result = gateway
        .call("session/list", serde_json::json!({}), None)
        .await?;
    Ok(parse_sessions(result))
}

/// Real per-backend ACP `session/list`, selected by the ACPX supervisor
/// agent id. The gateway translates backend-native ids into gateway ids but
/// preserves ACP session metadata such as title and `updatedAt`, enabling a
/// local transcript cache to decide whether it needs a `session/load`.
pub async fn list_for_agent(
    gateway: &Gateway,
    agent_id: &str,
) -> Result<Vec<SessionSummary>, ClientError> {
    let result = gateway
        .call(
            "session/list",
            serde_json::json!({ "_acpx": { "agentId": agent_id } }),
            None,
        )
        .await?;
    let mut sessions = parse_sessions(result);
    // The wire response never carries `agentId` for this selector (see
    // `SessionSummary::agent_id`'s own doc comment) -- backfill it from
    // what this call already knows, so callers never see an empty
    // string where a real agent id obviously belongs.
    for session in &mut sessions {
        if session.agent_id.is_empty() {
            session.agent_id = agent_id.to_string();
        }
    }
    Ok(sessions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_backend_metadata_without_requiring_it_on_aggregate_lists() {
        let sessions = parse_sessions(serde_json::json!({
            "sessions": [{
                "sessionId": "gateway-1",
                "agentId": "codex",
                "title": "Fix export",
                "updatedAt": "2026-07-16T10:00:00Z"
            }, {
                "sessionId": "gateway-2",
                "agentId": "claude"
            }]
        }));

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].title.as_deref(), Some("Fix export"));
        assert_eq!(
            sessions[0].updated_at.as_deref(),
            Some("2026-07-16T10:00:00Z")
        );
        assert_eq!(sessions[1].title, None);
        assert_eq!(sessions[1].updated_at, None);
    }

    /// Regression test for the real bug fixed in this same change:
    /// the per-agent/per-profile selector's real wire shape has no
    /// `agentId` field at all (unlike the aggregate shape the test
    /// above covers) -- this must still parse every entry, not
    /// silently drop the whole list via `unwrap_or_default()`.
    #[test]
    fn parses_the_real_per_agent_selector_shape_which_has_no_agent_id_field() {
        let sessions = parse_sessions(serde_json::json!({
            "sessions": [{
                "sessionId": "backend-native-1",
                "cwd": "/tmp",
                "title": "New session",
                "updatedAt": "t2"
            }, {
                "sessionId": "backend-native-2",
                "cwd": "/tmp"
            }]
        }));

        assert_eq!(
            sessions.len(),
            2,
            "missing agentId on the wire must not drop every entry"
        );
        assert_eq!(sessions[0].session_id, "backend-native-1");
        assert_eq!(sessions[0].agent_id, "", "unbackfilled at the parse layer -- list_for_agent backfills separately");
        assert_eq!(sessions[1].session_id, "backend-native-2");
    }
}
