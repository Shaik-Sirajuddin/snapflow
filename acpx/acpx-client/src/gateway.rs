//! Transport-neutral ACPX gateway facade.
//!
//! `Gateway` is the only API panel code should use. It prefers a persistent
//! WebSocket connection, exposes live notifications in that mode, and makes
//! HTTP fallback explicit so callers cannot accidentally show unavailable
//! interactive controls.

use crate::raw::{ClientError, GatewayClient};
use crate::ws::{GatewayNotification, GatewayWsClient};
use std::sync::Arc;
use tokio::sync::broadcast;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportMode {
    WebSocketInteractive,
    HttpDegraded,
}

pub struct Gateway {
    mode: TransportMode,
    http: GatewayClient,
    websocket: Option<Arc<GatewayWsClient>>,
}

/// A live agent-initiated request relayed from the gateway (see
/// `acpx_core::agent_relay`'s module doc comment on the server side --
/// this is the SDK-level counterpart to that same relay). Seen as a bare
/// notification (no `id`, `method == "acpx/agent_request"`) on
/// [`Gateway::subscribe`]'s stream; [`AgentRequest::from_notification`]
/// parses one out so panel/consumer code never hand-parses the raw JSON
/// shape itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRequest {
    /// This hub's own correlation id -- pass back unchanged to
    /// [`Gateway::respond_agent_request`]; distinct from `request`'s own
    /// JSON-RPC `id`, which belongs to the backend, not this relay.
    pub relay_id: String,
    pub session_id: String,
    /// The original backend-native JSON-RPC request frame, verbatim
    /// (`method`, `params`, and the backend's own `id`).
    pub request: serde_json::Value,
}

impl AgentRequest {
    /// `None` for anything that isn't a well-formed `acpx/agent_request`
    /// notification -- callers are expected to first check `subscribe()`
    /// frames for other shapes (`session/update`, etc.) before trying
    /// this, same as any other notification-kind discriminator.
    pub fn from_notification(value: &serde_json::Value) -> Option<Self> {
        if value.get("method").and_then(|m| m.as_str()) != Some("acpx/agent_request") {
            return None;
        }
        let params = value.get("params")?;
        Some(Self {
            relay_id: params.get("relayId")?.as_str()?.to_string(),
            session_id: params.get("sessionId")?.as_str()?.to_string(),
            request: params.get("request")?.clone(),
        })
    }

    /// The relayed request's own ACP method name (`session/request_
    /// permission` today), if present -- the discriminator a panel
    /// reducer switches on to pick which request-card UI to render.
    pub fn method(&self) -> Option<&str> {
        self.request.get("method").and_then(|m| m.as_str())
    }
}

impl Gateway {
    /// Connects to the persistent transport first. A failed handshake falls
    /// back to HTTP, whose lack of notifications is exposed by `mode()`.
    pub async fn connect(base_url: impl Into<String>) -> Self {
        let base_url = base_url.into();
        let http = GatewayClient::new(base_url.clone());
        match GatewayWsClient::connect(&base_url).await {
            Ok(websocket) => Self {
                mode: TransportMode::WebSocketInteractive,
                http,
                websocket: Some(websocket),
            },
            Err(_) => Self {
                mode: TransportMode::HttpDegraded,
                http,
                websocket: None,
            },
        }
    }

    pub fn http_degraded(base_url: impl Into<String>) -> Self {
        Self {
            mode: TransportMode::HttpDegraded,
            http: GatewayClient::new(base_url),
            websocket: None,
        }
    }

    pub fn mode(&self) -> TransportMode {
        self.mode
    }

    pub fn supports_interactive_requests(&self) -> bool {
        matches!(self.mode, TransportMode::WebSocketInteractive)
    }

    pub fn subscribe(&self) -> Option<broadcast::Receiver<GatewayNotification>> {
        self.websocket.as_ref().map(|client| client.subscribe())
    }

    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
        profile: Option<&str>,
    ) -> Result<serde_json::Value, ClientError> {
        match &self.websocket {
            Some(websocket) => {
                // The WS transport binds profile selection into session/new
                // parameters. Header-only profile selection is HTTP-specific.
                let params = with_profile(params, profile);
                websocket.call(method, params).await
            }
            None => self.http.call(method, params, profile).await,
        }
    }

    /// HTTP returns buffered updates with the response. WebSocket callers
    /// receive them through `subscribe()` as live notifications instead.
    pub async fn call_with_updates(
        &self,
        method: &str,
        params: serde_json::Value,
        profile: Option<&str>,
    ) -> Result<(serde_json::Value, Vec<serde_json::Value>), ClientError> {
        match &self.websocket {
            Some(websocket) => {
                websocket
                    .call_with_updates(method, with_profile(params, profile))
                    .await
            }
            None => self.http.call_with_updates(method, params, profile).await,
        }
    }

    /// Answer a relayed [`AgentRequest`] by sending its `relay_id` and a
    /// JSON-RPC reply (`{"result": ..}` or `{"error": ..}`, echoing the
    /// backend's own request id -- callers typically build this by
    /// cloning `request.request["id"]` straight out of the `AgentRequest`
    /// they're answering) back over the same connection that received
    /// it. Returns whether the gateway still had a pending relay waiting
    /// for this exact `relay_id` (`false` covers both an unknown id and
    /// one whose server-side wait already timed out -- see
    /// `acpx_core::agent_relay::AgentRequestHub::resolve`'s doc comment).
    ///
    /// Only meaningful in [`TransportMode::WebSocketInteractive`] -- HTTP
    /// degraded mode has no live relay to answer at all (the gateway
    /// already auto-answered from policy before an HTTP response was
    /// even possible), so this returns `ClientError::WebSocket` there
    /// rather than silently pretending to succeed.
    pub async fn respond_agent_request(
        &self,
        relay_id: &str,
        response: serde_json::Value,
    ) -> Result<bool, ClientError> {
        let Some(websocket) = &self.websocket else {
            return Err(ClientError::WebSocket(
                "acpx/agent_response requires an interactive WebSocket connection; this \
                 Gateway is running in HTTP degraded mode with no live relay to answer"
                    .to_string(),
            ));
        };
        let result = websocket
            .call(
                "acpx/agent_response",
                serde_json::json!({"relayId": relay_id, "response": response}),
            )
            .await?;
        Ok(result
            .get("delivered")
            .and_then(|v| v.as_bool())
            .unwrap_or(false))
    }
}

fn with_profile(mut params: serde_json::Value, profile: Option<&str>) -> serde_json::Value {
    let Some(profile) = profile else {
        return params;
    };
    let Some(object) = params.as_object_mut() else {
        return params;
    };
    object.insert(
        "_acpx".to_string(),
        serde_json::json!({ "profile": profile }),
    );
    params
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_request_parses_a_well_formed_notification() {
        let value = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "acpx/agent_request",
            "params": {
                "relayId": "relay-1",
                "sessionId": "gw-1",
                "request": {"jsonrpc": "2.0", "id": 999, "method": "session/request_permission"}
            }
        });
        let parsed = AgentRequest::from_notification(&value).expect("parses");
        assert_eq!(parsed.relay_id, "relay-1");
        assert_eq!(parsed.session_id, "gw-1");
        assert_eq!(parsed.method(), Some("session/request_permission"));
    }

    #[test]
    fn agent_request_rejects_other_notification_shapes() {
        let session_update = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": "gw-1"}
        });
        assert!(AgentRequest::from_notification(&session_update).is_none());

        let malformed = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "acpx/agent_request",
            "params": {"sessionId": "gw-1"}
        });
        assert!(AgentRequest::from_notification(&malformed).is_none());
    }

    #[test]
    fn websocket_profile_is_embedded_in_acpx_params() {
        assert_eq!(
            with_profile(serde_json::json!({"cwd": "/tmp"}), Some("codex")),
            serde_json::json!({"cwd": "/tmp", "_acpx": {"profile": "codex"}})
        );
    }

    #[test]
    fn profile_is_not_added_when_absent() {
        assert_eq!(
            with_profile(serde_json::json!({"cwd": "/tmp"}), None),
            serde_json::json!({"cwd": "/tmp"})
        );
    }
}
