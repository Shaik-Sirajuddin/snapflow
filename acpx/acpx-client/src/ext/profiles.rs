//! Profile selection/listing calls (`profiles/*`). Phase 5 step 21.
//!
//! Payloads are kept as raw `serde_json::Value` rather than duplicating
//! `acpx-core::profile::Profile`'s typed shape -- `acpx-client` doesn't
//! (and per `03-crate-and-folder-layout.md`'s client/server crate
//! boundary, shouldn't) depend on `acpx-core`. A caller that wants typed
//! profile payloads can build them with `serde_json::json!` using the
//! same field names `Profile` serializes to (`name`, `agent_id`,
//! `provider`, `key_ref`, `launch_overrides`, `mcp_servers`; see
//! `router.rs`'s `profiles/create`/`update` handlers for the exact
//! accepted shape, including the create/update-only `secret` field).

use crate::raw::{ClientError, GatewayClient};
use serde_json::json;

/// `profiles/list` -- every profile the gateway currently has registered.
pub async fn list(client: &GatewayClient) -> Result<Vec<serde_json::Value>, ClientError> {
    let result = client.call("profiles/list", json!({}), None).await?;
    Ok(result
        .get("profiles")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default())
}

/// `profiles/create`. Returns the created profile as the gateway echoed
/// it back (secret material itself is never echoed -- only the resulting
/// opaque `key_ref`, see `router.rs`'s `profiles/create` handler).
pub async fn create(
    client: &GatewayClient,
    profile: serde_json::Value,
) -> Result<serde_json::Value, ClientError> {
    client.call("profiles/create", profile, None).await
}

/// `profiles/update` -- same payload shape as `create`.
pub async fn update(
    client: &GatewayClient,
    profile: serde_json::Value,
) -> Result<serde_json::Value, ClientError> {
    client.call("profiles/update", profile, None).await
}

/// `profiles/delete`.
pub async fn delete(client: &GatewayClient, name: &str) -> Result<(), ClientError> {
    client
        .call("profiles/delete", json!({ "name": name }), None)
        .await?;
    Ok(())
}
