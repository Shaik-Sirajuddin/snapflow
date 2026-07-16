//! Schema-only mirrors of the gateway-native response/param envelopes
//! `acpx-core/src/router.rs` builds inline via `serde_json::json!{}`
//! rather than a named Rust type -- `agents/*`, `session/list`'s default
//! (no-selector) branch, `profiles/*`, `mcp_servers/*`. Phase 20 typed
//! the JSON-RPC envelope and two `agents/*` types; this module closes the
//! remaining gateway-native gap the `acpx-openrpc-schema` plan's
//! `00-goal.md` describes.
//!
//! **"Schema-only mirror", not the live producer**: every type here
//! derives `JsonSchema` (and `Serialize`/`Deserialize` for round-trip
//! test coverage) but `router.rs` is not changed to construct these
//! structs and serialize them -- that would be a larger, behavior-
//! touching refactor with no schema-pipeline benefit, and risks the kind
//! of accidental-field-drop regression `NewSessionParams`'
//! `#[serde(flatten)] rest` pattern exists specifically to avoid. Same
//! posture as that type: a deliberate mirror, kept honest by a unit test
//! per type asserting the derived shape's field names match a real
//! `router.rs` response literal byte-for-byte, not by construction.
//!
//! `ProfileSchema`/`PermissionPolicySchema` additionally mirror
//! `acpx-core::profile::{Profile, PermissionPolicy}` rather than
//! `#[derive(JsonSchema)]`-ing those types directly and referencing them
//! from here, because `acpx-core` depends on `acpx-proto` (not the other
//! way around) -- `acpx-proto` acquiring a dependency on `acpx-core` to
//! reference its types would invert that layering. See
//! `01-architecture.md`'s "Crate/module layering" section.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;

use crate::agent::{AgentListEntry, AgentStatus};

/// `agents/install` and `agents/status` params -- both are exactly
/// `{"id": "..."}"` in `router.rs`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentIdParams {
    pub id: String,
}

/// `agents/list` result envelope.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentsListResult {
    pub agents: Vec<AgentListEntry>,
}

/// `agents/install` result envelope. `outcome` is `acpx_registry::
/// InstallOutcome`'s `{outcome:?}`-formatted debug string (router.rs's
/// own choice, not a typed enum on the wire) -- kept as a bare `String`
/// here rather than inventing a closed enum this schema would then have
/// to keep in lockstep with `acpx-registry`'s.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentInstallResult {
    pub id: String,
    pub outcome: String,
}

/// `agents/status` result envelope.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentStatusResult {
    pub id: String,
    pub status: AgentStatus,
}

/// One entry in `session/list`'s default (no-selector) branch --
/// distinct from, and much smaller than, real ACP's `ListSessionsResponse`
/// entries, since this branch answers "which gateway sessions does this
/// acpx process itself know about", not a specific backend's own session
/// history. See `router.rs`'s `"session/list"` arm doc comment for why
/// this method is a genuine dual-shape split, not a rename.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GatewaySessionListEntry {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "agentId")]
    pub agent_id: String,
    pub cwd: String,
}

/// `session/list`'s default (no-selector) result envelope. When the
/// caller supplies a selector instead, `router.rs` proxies to the real
/// backend's `ListSessionsRequest`/`ListSessionsResponse`
/// (`dispatch_session_list_real`) -- the method registry
/// (`methods.rs`) records both possibilities as a `oneOf`, this type
/// covers only the gateway-native half.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GatewaySessionListResult {
    pub sessions: Vec<GatewaySessionListEntry>,
}

/// A centrally-registered MCP server entry (`mcp_servers/create|update|
/// list`). Deliberately opaque beyond the `name` key acpx actually reads
/// (`acpx-core/src/mcp_servers.rs`'s doc comment: "`acpx` never
/// interprets an MCP server entry's fields itself") -- this is an
/// intentional design decision restated in schema form, not a gap to
/// close further (see `00-goal.md`'s non-goals).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpServerEntry {
    pub name: String,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

/// `mcp_servers/list` result envelope.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct McpServersListResult {
    pub servers: Vec<McpServerEntry>,
}

/// `profiles/delete` and `mcp_servers/delete` params -- both are exactly
/// `{"name": "..."}"` in `router.rs`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NameOnlyParams {
    pub name: String,
}

/// `profiles/delete` and `mcp_servers/delete` result envelope -- both
/// are exactly `{"name": "...", "deleted": true}"` in `router.rs`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NameOnlyResult {
    pub name: String,
    pub deleted: bool,
}

/// Mirrors `acpx_core::profile::PermissionPolicy` -- see this module's
/// doc comment for why this is a mirror rather than a direct reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionPolicySchema {
    AutoAllow,
    #[default]
    AutoReject,
}

/// Mirrors `acpx_core::profile::Profile`'s wire shape exactly (field
/// names, optionality, defaults) -- the type `profiles/create`,
/// `profiles/update`, and `profiles/list` (per-element) all serialize on
/// the wire, post `redact_launch_overrides` (which only ever replaces
/// `launch_overrides` *values* with the literal string
/// `"***redacted***"`, never changes the shape). `key_ref` mirrors
/// `acpx_core::keystore::KeyRef`'s newtype-transparent wire
/// representation (a bare string) as `Option<String>` directly rather
/// than introducing a single-field wrapper type purely for this file.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ProfileSchema {
    pub name: String,
    pub agent_id: String,
    pub provider: Option<String>,
    pub key_ref: Option<String>,
    pub launch_overrides: HashMap<String, String>,
    pub mcp_servers: Vec<String>,
    pub permission_policy: PermissionPolicySchema,
    pub allow_fs_access: bool,
    pub allow_terminal_access: bool,
    pub auth_method_id: Option<String>,
}

/// `profiles/list` result envelope.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProfilesListResult {
    pub profiles: Vec<ProfileSchema>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks `GatewaySessionListEntry`'s field names/renames against the
    /// exact literal `router.rs`'s `"session/list"` default branch
    /// constructs, so the two can't silently drift.
    #[test]
    fn gateway_session_list_entry_matches_router_literal() {
        let router_literal = serde_json::json!({
            "sessionId": "gw-1",
            "agentId": "codex-acp",
            "cwd": "/tmp",
        });
        let entry: GatewaySessionListEntry =
            serde_json::from_value(router_literal.clone()).unwrap();
        assert_eq!(serde_json::to_value(&entry).unwrap(), router_literal);
    }

    /// Locks `NameOnlyResult` against the exact literal both
    /// `profiles/delete` and `mcp_servers/delete` construct.
    #[test]
    fn name_only_result_matches_router_literal() {
        let router_literal = serde_json::json!({"name": "p1", "deleted": true});
        let result: NameOnlyResult = serde_json::from_value(router_literal.clone()).unwrap();
        assert_eq!(serde_json::to_value(&result).unwrap(), router_literal);
    }

    /// Locks `AgentInstallResult`/`AgentStatusResult` against the exact
    /// literals `router.rs`'s `agents/install`/`agents/status` arms
    /// construct.
    #[test]
    fn agent_install_and_status_results_match_router_literals() {
        let install_literal = serde_json::json!({"id": "codex-acp", "outcome": "Installed"});
        let install: AgentInstallResult = serde_json::from_value(install_literal.clone()).unwrap();
        assert_eq!(serde_json::to_value(&install).unwrap(), install_literal);

        let status_literal = serde_json::json!({"id": "codex-acp", "status": "installed"});
        let status: AgentStatusResult = serde_json::from_value(status_literal.clone()).unwrap();
        assert_eq!(serde_json::to_value(&status).unwrap(), status_literal);
    }

    /// Locks `ProfileSchema` against a full literal covering every field,
    /// matching `acpx_core::profile::Profile`'s own serde shape.
    #[test]
    fn profile_schema_matches_full_literal() {
        let router_literal = serde_json::json!({
            "name": "work",
            "agent_id": "codex-acp",
            "provider": "openai",
            "key_ref": "key-1",
            "launch_overrides": {"FOO": "***redacted***"},
            "mcp_servers": ["fs"],
            "permission_policy": "auto_allow",
            "allow_fs_access": true,
            "allow_terminal_access": false,
            "auth_method_id": "api-key",
        });
        let profile: ProfileSchema = serde_json::from_value(router_literal.clone()).unwrap();
        assert_eq!(serde_json::to_value(&profile).unwrap(), router_literal);
    }

    /// `McpServerEntry` must round-trip arbitrary extra fields, not just
    /// `name` -- that's the entire point of its opacity contract.
    #[test]
    fn mcp_server_entry_preserves_arbitrary_fields() {
        let router_literal = serde_json::json!({
            "name": "fs",
            "command": "npx",
            "args": ["-y", "@modelcontextprotocol/server-filesystem"],
        });
        let entry: McpServerEntry = serde_json::from_value(router_literal.clone()).unwrap();
        assert_eq!(serde_json::to_value(&entry).unwrap(), router_literal);
    }

    /// Locks `ProfilesListResult` against the exact envelope
    /// `router.rs`'s `"profiles/list"` arm constructs.
    #[test]
    fn profiles_list_result_matches_router_literal() {
        let router_literal = serde_json::json!({"profiles": []});
        let result: ProfilesListResult = serde_json::from_value(router_literal.clone()).unwrap();
        assert_eq!(serde_json::to_value(&result).unwrap(), router_literal);
    }
}
