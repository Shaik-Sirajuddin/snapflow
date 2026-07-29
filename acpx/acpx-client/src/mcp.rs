//! Typed MCP server config + `Gateway` methods for `mcp_servers/*`.
//!
//! Before this module, every caller (panel-rust's `agent_bridge.rs`
//! included) hand-built raw `serde_json::Value` payloads for `mcp_servers/
//! create|update|delete|list`, and `mcp_servers/authenticate|logout` had
//! no client-side wrapper at all. `acpx-core::McpServerStore` deliberately
//! stays untyped server-side (see its own doc comment -- it never
//! interprets an entry's fields, so re-typing it there would just be a
//! second place to keep in sync with the wire schema), but `acpx-client`
//! is exactly the boundary where a typed shape earns its keep: it is the
//! one place every caller (today just panel-rust, which already depends
//! on this crate directly) goes through, so a field that exists here is
//! guaranteed to survive the whole round trip instead of silently being
//! dropped by whichever call site happened to build the JSON by hand.
//!
//! Wire shape mirrors the MCP spec's own `mcpServers` config convention
//! (the same one Zed's `context_servers` setting uses): an internally
//! tagged `"type": "stdio" | "http"` discriminator, `command`/`args`/
//! `env`/`timeout` for `stdio`, `url`/`headers`/`timeout`/`oauth` for
//! `http`.

use crate::gateway::Gateway;
use crate::raw::ClientError;
use std::collections::HashMap;

/// The two MCP server transports ACP's own `mcpServers` array supports.
/// Internally tagged (`"type"`) rather than untagged: this crate's one
/// other tagged-enum precedent (panel-rust's `jsonl_store.rs::Line`) uses
/// the same shape, and an explicit tag gives a caller a real error
/// message ("missing/invalid \"type\"") instead of untagged's opaque
/// "data did not match any variant" when a field is misspelled.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpServerConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<u64>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        oauth: Option<OAuthClientConfig>,
    },
}

impl McpServerConfig {
    pub fn transport_name(&self) -> &'static str {
        match self {
            McpServerConfig::Stdio { .. } => "stdio",
            McpServerConfig::Http { .. } => "http",
        }
    }
}

/// User-supplied OAuth client identity for an `Http` server -- deliberately
/// `client_id` only, no `client_secret` field: `Router::authenticate_mcp_
/// server` prefers RFC 7591 dynamic client registration (a public client,
/// no secret needed) and only falls back to this override when the
/// authorization server has no registration endpoint. A confidential
/// client secret would need real at-rest protection this settings form
/// has no way to guarantee, so it is intentionally not collected here --
/// matches Zed's own `oauth.client_id`-only `ContextServerSettings` shape.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OAuthClientConfig {
    pub client_id: String,
}

/// Server-reported authentication state for an `Http` server with OAuth
/// configured. Mirrors Zed's `ContextServerState` status vocabulary
/// (`Stopped`/`Starting`/`Running`/`Error`/`AuthRequired`/
/// `Authenticating`) closely enough for a settings list row to reuse the
/// same status-dot color mapping, while staying just the subset acpx
/// itself can actually observe today (it has no live per-connection
/// health probe yet -- only "has a server been authenticated").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpAuthStatus {
    #[default]
    Unauthenticated,
    Authenticated,
}

/// One centrally-registered MCP server, as sent to/returned by
/// `mcp_servers/create|update|list`. `extra` retains any wire fields this
/// type doesn't yet model (forward-compatible with a future ACP/MCP
/// schema addition) -- deserializing never fails or silently drops data
/// just because the gateway sent one more key than this struct knows
/// about; it only ever refuses to omit a field this struct *does* know
/// about, which is the actual "incomplete data" bug this type exists to
/// close.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct McpServerEntry {
    pub name: String,
    pub enabled: bool,
    #[serde(flatten)]
    pub config: McpServerConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_status: Option<McpAuthStatus>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Hand-rolled rather than `#[derive(Deserialize)]` with two `#[serde(
/// flatten)]` fields (`config`, `extra`): serde's flatten deserializes
/// every flattened field from the *same* shared buffered map rather than
/// removing keys as each sibling consumes them, so a derived impl here
/// would put every one of `config`'s own keys (`type`/`command`/`url`/
/// ...) into `extra` too, not just genuinely-unrecognized ones -- a real
/// bug this crate's own test (`unknown_extra_fields_are_preserved_not_
/// dropped`) caught immediately. This impl explicitly removes each field
/// it recognizes before handing whatever's left over to `extra`.
impl<'de> serde::Deserialize<'de> for McpServerEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut map = serde_json::Map::deserialize(deserializer)?;
        let name = map
            .remove("name")
            .and_then(|v| v.as_str().map(str::to_string))
            .ok_or_else(|| serde::de::Error::missing_field("name"))?;
        let enabled = map.remove("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
        let auth_status = map
            .remove("auth_status")
            .map(serde_json::from_value)
            .transpose()
            .map_err(serde::de::Error::custom)?;

        let transport = map.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let config_fields: &[&str] = match transport.as_str() {
            "stdio" => &["type", "command", "args", "env", "timeout"],
            "http" => &["type", "url", "headers", "timeout", "oauth"],
            _ => &["type"],
        };
        let mut config_map = serde_json::Map::new();
        for &field in config_fields {
            if let Some(value) = map.remove(field) {
                config_map.insert(field.to_string(), value);
            }
        }
        let config = serde_json::from_value(serde_json::Value::Object(config_map))
            .map_err(serde::de::Error::custom)?;

        Ok(McpServerEntry {
            name,
            enabled,
            config,
            auth_status,
            extra: map,
        })
    }
}

impl McpServerEntry {
    pub fn new(name: impl Into<String>, config: McpServerConfig) -> Self {
        Self {
            name: name.into(),
            enabled: true,
            config,
            auth_status: None,
            extra: serde_json::Map::new(),
        }
    }

    /// True for any `Http` server that isn't already authenticated *and*
    /// doesn't already carry a static `Authorization` header (a manually
    /// pasted bearer token is itself a complete auth story -- OAuth would
    /// be redundant, same as Zed's own `has_static_auth_header()`
    /// suppressing its OAuth UI). Deliberately **not** gated on
    /// `oauth.client_id` being pre-filled: `Router::authenticate_mcp_
    /// server` falls back to RFC 7591 dynamic client registration when
    /// no `client_id` override is configured, so a plain HTTP server
    /// (the common case -- the settings form's OAuth Client ID field is
    /// optional) must still be able to reach the Connect flow, not be
    /// silently excluded from it just because that one optional field
    /// was left blank.
    pub fn needs_auth(&self) -> bool {
        match &self.config {
            McpServerConfig::Http { headers, .. } => {
                !headers.contains_key("Authorization")
                    && self.auth_status != Some(McpAuthStatus::Authenticated)
            }
            McpServerConfig::Stdio { .. } => false,
        }
    }

    /// The `stdio` command, or `None` for an `Http` entry. Convenience for
    /// callers (panel-rust's settings list) that display a single
    /// "command or URL" summary column without matching on `config`
    /// themselves.
    pub fn command(&self) -> Option<&str> {
        match &self.config {
            McpServerConfig::Stdio { command, .. } => Some(command),
            McpServerConfig::Http { .. } => None,
        }
    }

    /// The `http` URL, or `None` for a `Stdio` entry.
    pub fn url(&self) -> Option<&str> {
        match &self.config {
            McpServerConfig::Stdio { .. } => None,
            McpServerConfig::Http { url, .. } => Some(url),
        }
    }
}

impl Gateway {
    /// `mcp_servers/list`. Entries the gateway returns in a shape this
    /// type can't parse are skipped (not a hard error) rather than
    /// failing the whole list -- one malformed entry (e.g. hand-edited
    /// provisioning JSON with a typo) should not make every *other*
    /// configured server disappear from the settings UI.
    pub async fn list_mcp_servers(&self) -> Result<Vec<McpServerEntry>, ClientError> {
        let response = self.call("mcp_servers/list", serde_json::json!({}), None).await?;
        let servers = response
            .get("servers")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(servers
            .into_iter()
            .filter_map(|raw| serde_json::from_value(raw).ok())
            .collect())
    }

    pub async fn create_mcp_server(&self, entry: &McpServerEntry) -> Result<(), ClientError> {
        let params = serde_json::to_value(entry).expect("McpServerEntry always serializes");
        self.call("mcp_servers/create", params, None).await?;
        Ok(())
    }

    pub async fn update_mcp_server(&self, entry: &McpServerEntry) -> Result<(), ClientError> {
        let params = serde_json::to_value(entry).expect("McpServerEntry always serializes");
        self.call("mcp_servers/update", params, None).await?;
        Ok(())
    }

    pub async fn delete_mcp_server(&self, name: &str) -> Result<(), ClientError> {
        self.call("mcp_servers/delete", serde_json::json!({ "name": name }), None)
            .await?;
        Ok(())
    }

    /// `mcp_servers/authenticate`. Returns the authorization URL the
    /// caller must open in a browser (see `acpx_core::router::Router::
    /// authenticate_mcp_server`'s doc comment) -- the rest of the OAuth
    /// flow completes asynchronously server-side; poll [`Self::
    /// list_mcp_servers`] afterward to observe `auth_status` flip to
    /// [`McpAuthStatus::Authenticated`].
    pub async fn authenticate_mcp_server(&self, name: &str) -> Result<String, ClientError> {
        let response = self
            .call(
                "mcp_servers/authenticate",
                serde_json::json!({ "name": name }),
                None,
            )
            .await?;
        response
            .get("authorizationUrl")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| ClientError::Rpc {
                code: 0,
                message: "mcp_servers/authenticate response missing authorizationUrl".to_string(),
            })
    }

    pub async fn logout_mcp_server(&self, name: &str) -> Result<(), ClientError> {
        self.call("mcp_servers/logout", serde_json::json!({ "name": name }), None)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdio_entry_round_trips_through_json() {
        let entry = McpServerEntry::new(
            "fs",
            McpServerConfig::Stdio {
                command: "mcp-fs".to_string(),
                args: vec!["--root".to_string(), "/tmp".to_string()],
                env: HashMap::from([("TOKEN".to_string(), "abc".to_string())]),
                timeout: Some(30),
            },
        );
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "stdio");
        assert_eq!(json["command"], "mcp-fs");
        assert_eq!(json["args"][0], "--root");
        let parsed: McpServerEntry = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, entry);
    }

    #[test]
    fn http_entry_with_oauth_round_trips() {
        let entry = McpServerEntry::new(
            "remote",
            McpServerConfig::Http {
                url: "https://example.com/mcp".to_string(),
                headers: HashMap::new(),
                timeout: None,
                oauth: Some(OAuthClientConfig {
                    client_id: "client-123".to_string(),
                }),
            },
        );
        assert!(entry.needs_auth());
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "http");
        assert_eq!(json["oauth"]["client_id"], "client-123");
        let parsed: McpServerEntry = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, entry);
    }

    #[test]
    fn authenticated_http_server_does_not_need_auth() {
        let mut entry = McpServerEntry::new(
            "remote",
            McpServerConfig::Http {
                url: "https://example.com/mcp".to_string(),
                headers: HashMap::new(),
                timeout: None,
                oauth: Some(OAuthClientConfig {
                    client_id: "client-123".to_string(),
                }),
            },
        );
        entry.auth_status = Some(McpAuthStatus::Authenticated);
        assert!(!entry.needs_auth());
    }

    #[test]
    fn http_server_with_static_auth_header_never_needs_auth() {
        let entry = McpServerEntry::new(
            "remote",
            McpServerConfig::Http {
                url: "https://example.com/mcp".to_string(),
                headers: HashMap::from([("Authorization".to_string(), "Bearer static".to_string())]),
                timeout: None,
                oauth: None,
            },
        );
        assert!(!entry.needs_auth());
    }

    /// The bug this test guards against: `needs_auth` must not require
    /// `oauth.client_id` to be pre-filled -- `Router::authenticate_mcp_
    /// server` falls back to dynamic client registration when it's
    /// absent, so a plain HTTP server with neither a static auth header
    /// nor a pre-configured OAuth client id must still be considered
    /// connectable, not silently excluded from the Connect flow.
    #[test]
    fn plain_http_server_with_no_static_auth_still_needs_auth() {
        let entry = McpServerEntry::new(
            "remote",
            McpServerConfig::Http {
                url: "https://example.com/mcp".to_string(),
                headers: HashMap::new(),
                timeout: None,
                oauth: None,
            },
        );
        assert!(
            entry.needs_auth(),
            "a plain HTTP server (no static auth header, no oauth.client_id) must still be \
             eligible for Connect via dynamic client registration"
        );
    }

    #[test]
    fn unknown_extra_fields_are_preserved_not_dropped() {
        let raw = serde_json::json!({
            "name": "fs",
            "type": "stdio",
            "command": "mcp-fs",
            "someFutureField": "keep-me"
        });
        let parsed: McpServerEntry = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.extra.get("someFutureField").unwrap(), "keep-me");
        let round_tripped = serde_json::to_value(&parsed).unwrap();
        assert_eq!(round_tripped["someFutureField"], "keep-me");
    }
}
