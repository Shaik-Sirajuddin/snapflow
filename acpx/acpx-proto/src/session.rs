//! `session/*` payload types.
//!
//! Per `02-architecture.md`'s extension-channel rule, `session/new`'s
//! `params` shape is never redefined: `NewSessionParams` below is the plain
//! ACP shape, and `AcpxExt` is parsed as an optional sibling field on the
//! wire, not folded into the ACP struct itself. The router strips `_acpx`
//! before forwarding to a backend regardless of whether it was present.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Sibling extension object carried by `session/new` only: `{"_acpx": {"profile": "..."}}`.
/// Chosen as a single namespaced key (see architecture doc) so it can be
/// trivially stripped and can't collide with any current/future ACP field.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct AcpxExt {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// `session/new` request params. `_acpx` is additive and stripped by the
/// router prior to forwarding -- see `AcpxExt` above.
///
/// **`#[serde(rename_all = "camelCase")]` (added post-real-e2e-schema-
/// validation, `acpx-openrpc-schema` plan):** upstream's real
/// `NewSessionRequest` (agent-client-protocol-schema 1.4.0,
/// `src/v1/agent.rs`) carries this same container attribute, so
/// `mcp_servers` is `mcpServers` on the actual wire -- every real ACP
/// client (including this workspace's own `real_ambient_multi_agent_
/// test.rs`) sends `"mcpServers"`, never `"mcp_servers"`. Before this
/// fix, this struct's missing `rename_all` meant a real
/// `{"cwd":..., "mcpServers": [...]}` request silently deserialized
/// with `mcp_servers` defaulted to empty and the real array captured by
/// `rest` (the `#[serde(flatten)]` catch-all) instead -- functionally
/// harmless *only* because nothing ever read `NewSessionParams.
/// mcp_servers` at runtime (`router.rs` never uses this type at all,
/// operating on raw `serde_json::Value` directly instead -- see this
/// module's own doc comment on why `NewSessionParams` is schema/test
/// tooling, not the live producer), but it meant the generated schema
/// document was simply *wrong* about the real wire field name. Found by
/// running the schema pipeline's own real-backend e2e validation
/// (`acpx-proto::validate`) against this exact test's literal
/// `"mcpServers": []` params.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionParams {
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<Value>,
    #[serde(rename = "_acpx", default, skip_serializing_if = "Option::is_none")]
    pub acpx: Option<AcpxExt>,
    /// Anything else the real ACP schema defines that acpx doesn't need to
    /// interpret gets preserved here so proxying stays byte-faithful.
    #[serde(flatten)]
    pub rest: serde_json::Map<String, Value>,
}

/// Gateway-issued session id returned from a hybrid `session/new` call.
/// Distinct type from the backend's own session id (see
/// `acpx-core::session_registry`) so the two are never accidentally mixed up
/// at a call site.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct GatewaySessionId(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acpx_ext_is_additive_and_stripped_cleanly() {
        let raw = serde_json::json!({
            "cwd": "/tmp",
            "_acpx": {"profile": "work-openai"}
        });
        let mut params: NewSessionParams = serde_json::from_value(raw).unwrap();
        assert_eq!(
            params.acpx.as_ref().and_then(|a| a.profile.clone()),
            Some("work-openai".to_string())
        );
        // Router strips it before forwarding.
        params.acpx = None;
        let forwarded = serde_json::to_value(&params).unwrap();
        assert!(forwarded.get("_acpx").is_none());
    }

    #[test]
    fn raw_client_without_acpx_ext_is_unaffected() {
        let raw = serde_json::json!({"cwd": "/tmp"});
        let params: NewSessionParams = serde_json::from_value(raw).unwrap();
        assert!(params.acpx.is_none());
    }

    /// Regression test for the real bug found via
    /// `acpx-proto::validate` against `real_ambient_multi_agent_test.rs`'s
    /// actual wire literal: a real client sends `"mcpServers"`
    /// (camelCase, matching upstream `NewSessionRequest`'s own
    /// `#[serde(rename_all = "camelCase")]`), and it must land in the
    /// typed `mcp_servers` field, not silently fall through to `rest`.
    #[test]
    fn real_camel_case_mcp_servers_field_is_recognized() {
        let raw = serde_json::json!({
            "cwd": "/tmp",
            "mcpServers": [{"name": "fs", "command": "npx"}],
        });
        let params: NewSessionParams = serde_json::from_value(raw).unwrap();
        assert_eq!(
            params.mcp_servers.len(),
            1,
            "mcpServers fell through to `rest` instead of the typed field: {:?}",
            params.rest
        );
        assert!(params.rest.get("mcpServers").is_none());

        // Round-trips back out as `mcpServers`, not `mcp_servers`.
        let out = serde_json::to_value(&params).unwrap();
        assert!(out.get("mcpServers").is_some());
        assert!(out.get("mcp_servers").is_none());
    }
}
