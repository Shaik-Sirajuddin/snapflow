//! Centrally-registered MCP server CRUD (`mcp_servers/*`) -- see
//! `acpx-core::mcp_servers::McpServerStore`'s doc comment for the
//! server-side merge semantics (client-supplied `mcpServers` entries win
//! by `name` over a profile's central ones at `session/new`). Added for
//! symmetry with `profiles.rs`: an `acpx-client` consumer that wants to
//! manage the central registry (settings-gear MCP server list) should
//! never need to hand-build raw `serde_json::Value` calls with the
//! method name as a string literal.
//!
//! Payloads are kept as raw `serde_json::Value`, same reasoning as
//! `profiles.rs`'s module doc: `acpx-client` doesn't depend on
//! `acpx-core`, and `McpServerStore` itself never interprets an entry's
//! fields beyond `"name"` (the merge key), so a typed struct here would
//! just be a second place to keep in sync with ACP's own `mcpServers`
//! element schema.

use crate::raw::{ClientError, GatewayClient};
use serde_json::json;

/// `mcp_servers/list` -- every centrally-registered MCP server this
/// gateway currently has.
pub async fn list(client: &GatewayClient) -> Result<Vec<serde_json::Value>, ClientError> {
    let result = client.call("mcp_servers/list", json!({}), None).await?;
    Ok(result
        .get("servers")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default())
}

/// `mcp_servers/create`. `entry` must include a `"name"` field (the
/// merge key) -- see `router.rs`'s `mcp_servers/create` handler.
pub async fn create(
    client: &GatewayClient,
    entry: serde_json::Value,
) -> Result<serde_json::Value, ClientError> {
    client.call("mcp_servers/create", entry, None).await
}

/// `mcp_servers/update` -- same payload shape as `create`, replaces the
/// existing entry with the same `"name"`.
pub async fn update(
    client: &GatewayClient,
    entry: serde_json::Value,
) -> Result<serde_json::Value, ClientError> {
    client.call("mcp_servers/update", entry, None).await
}

/// `mcp_servers/delete`.
pub async fn delete(client: &GatewayClient, name: &str) -> Result<(), ClientError> {
    client
        .call("mcp_servers/delete", json!({ "name": name }), None)
        .await?;
    Ok(())
}
