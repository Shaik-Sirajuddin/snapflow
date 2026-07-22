//! Transport-neutral ACPX gateway facade.
//!
//! `Gateway` is the only API panel code should use. It prefers a persistent
//! WebSocket connection, exposes live notifications in that mode, and makes
//! HTTP fallback explicit so callers cannot accidentally show unavailable
//! interactive controls.

use crate::raw::{ClientError, GatewayClient};
use crate::ws::{GatewayNotification, GatewayWsClient};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::broadcast;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportMode {
    WebSocketInteractive,
    HttpDegraded,
}

/// Bound on how many times [`Gateway::reconnect`] retries a dropped
/// WebSocket connection (e.g. the gateway process died, or the panel's
/// own machine's network blipped) before giving up and falling back to
/// HTTP-degraded mode for that call. Chosen to recover from a brief
/// hiccup (a gateway restart typically completes in well under this
/// window) without hanging a user-initiated send/subscribe indefinitely
/// against a genuinely dead gateway.
const RECONNECT_MAX_ATTEMPTS: u32 = 3;
/// Hard ceiling on any single reconnect attempt -- a `connect_async` that
/// hangs (host unreachable, firewall silently dropping SYN, ...) would
/// otherwise stall this attempt indefinitely instead of moving on to the
/// next one.
const RECONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);
/// Linear backoff between attempts (attempt N waits `N *` this), so a
/// gateway that's mid-restart gets a little more time on each retry
/// rather than being hammered at a fixed interval.
const RECONNECT_BACKOFF_STEP: Duration = Duration::from_millis(500);

pub struct Gateway {
    base_url: String,
    http: GatewayClient,
    // A plain (non-async) RwLock: every access here is a quick clone of
    // the `Arc` followed immediately by dropping the guard, never held
    // across an `.await`, so there's no risk of blocking the async
    // runtime -- and it lets `mode()`/`subscribe()` stay synchronous
    // (matching their existing signatures; no ripple to every caller).
    websocket: RwLock<Option<Arc<GatewayWsClient>>>,
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
        let websocket = GatewayWsClient::connect(&base_url).await.ok();
        Self {
            base_url,
            http,
            websocket: RwLock::new(websocket),
        }
    }

    pub fn http_degraded(base_url: impl Into<String>) -> Self {
        let base_url = base_url.into();
        Self {
            http: GatewayClient::new(base_url.clone()),
            base_url,
            websocket: RwLock::new(None),
        }
    }

    /// Derived from whether a live WebSocket client is currently
    /// installed, not a separately-tracked flag -- so a successful
    /// `reconnect()` (or the initial degraded connect never having one)
    /// is always reflected immediately, with no separate field to drift
    /// out of sync.
    pub fn mode(&self) -> TransportMode {
        if self.current_websocket().is_some() {
            TransportMode::WebSocketInteractive
        } else {
            TransportMode::HttpDegraded
        }
    }

    pub fn supports_interactive_requests(&self) -> bool {
        matches!(self.mode(), TransportMode::WebSocketInteractive)
    }

    fn current_websocket(&self) -> Option<Arc<GatewayWsClient>> {
        self.websocket
            .read()
            .expect("gateway websocket lock poisoned")
            .clone()
    }

    /// Attempts to (re)establish the WebSocket connection, up to
    /// [`RECONNECT_MAX_ATTEMPTS`] times, each bounded by
    /// [`RECONNECT_ATTEMPT_TIMEOUT`] and separated by a linear backoff.
    /// On success, atomically swaps the fresh client in so every
    /// subsequent `call()`/`subscribe()` -- including a long-lived
    /// subscriber that re-subscribes after noticing its receiver has
    /// gone dead -- picks it up without any other code needing to know a
    /// reconnect happened. Returns whether a live connection exists
    /// afterward (`false` leaves this `Gateway` in HTTP-degraded mode,
    /// same as a fresh `connect()` whose initial handshake failed).
    pub async fn reconnect(&self) -> bool {
        for attempt in 1..=RECONNECT_MAX_ATTEMPTS {
            let attempt_result = tokio::time::timeout(
                RECONNECT_ATTEMPT_TIMEOUT,
                GatewayWsClient::connect(&self.base_url),
            )
            .await;
            if let Ok(Ok(client)) = attempt_result {
                *self.websocket.write().expect("gateway websocket lock poisoned") = Some(client);
                return true;
            }
            if attempt < RECONNECT_MAX_ATTEMPTS {
                tokio::time::sleep(RECONNECT_BACKOFF_STEP * attempt).await;
            }
        }
        *self.websocket.write().expect("gateway websocket lock poisoned") = None;
        false
    }

    pub fn subscribe(&self) -> Option<broadcast::Receiver<GatewayNotification>> {
        self.current_websocket().map(|client| client.subscribe())
    }

    /// Resolves when the *current* WebSocket connection dies, or
    /// immediately if there isn't one right now (HTTP-degraded mode --
    /// nothing further to wait for). A long-lived live-notification
    /// forwarding loop should race this (via `tokio::select!`) against
    /// its `Receiver::recv()` so it notices a dead connection even
    /// during a quiet period with no notifications and no `call()` in
    /// flight to otherwise trigger a reconnect -- see
    /// `GatewayWsClient::wait_for_disconnect`'s doc comment for why a
    /// bare `Receiver` alone can't signal this on its own.
    pub async fn wait_for_disconnect(&self) {
        match self.current_websocket() {
            Some(client) => client.wait_for_disconnect().await,
            None => {}
        }
    }

    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
        profile: Option<&str>,
    ) -> Result<serde_json::Value, ClientError> {
        match self.current_websocket() {
            Some(websocket) => {
                // The WS transport binds profile selection into session/new
                // parameters. Header-only profile selection is HTTP-specific.
                let full_params = with_profile(params.clone(), profile);
                match websocket.call(method, full_params).await {
                    // `ClientError::WebSocket` covers a dead/dropped
                    // connection (send failure, response channel closed,
                    // send/response timeout -- see ws.rs's `request()`), the
                    // signal that reconnecting is worth trying *for an
                    // idempotent method*. A `ClientError::Rpc` is the
                    // gateway alive and answering with a real JSON-RPC
                    // error; retrying on a fresh connection wouldn't
                    // change that, so it's returned as-is.
                    //
                    // **`acpx-reconnect-retry-duplicates-session-new`.**
                    // A "response timeout" specifically does not mean the
                    // server never received/processed the request, only
                    // that the *response* was lost or delayed. Blindly
                    // replaying a non-idempotent method here can create a
                    // second, real server-side effect the caller never
                    // asked for -- confirmed live: a real gateway process
                    // accumulated 512 duplicate `session/new`-created
                    // sessions over ~1.5h of otherwise normal use, with
                    // panel-rust's own call site invoked only 6-8 times.
                    // Every caller of a non-idempotent method already has
                    // (or should have) its own higher-level retry/
                    // fallback logic for a request that turns out to have
                    // genuinely failed (panel-rust's "cached session
                    // resume failed ... opening a fresh session" is
                    // exactly that) -- the transport silently retrying
                    // underneath it too is redundant at best, a silent
                    // resource leak at worst.
                    Err(ClientError::WebSocket(_)) if !is_safe_to_retry_after_reconnect(method) => {
                        Err(ClientError::WebSocket(
                            "not retrying a non-idempotent method after a WebSocket failure \
                             (the request may have already reached the server)"
                                .to_owned(),
                        ))
                    }
                    Err(ClientError::WebSocket(_)) => {
                        self.call_after_reconnect(method, params, profile).await
                    }
                    other => other,
                }
            }
            // **`acpx-reconnect-retry-duplicates-session-new`.** `None`
            // is *not* the same as "this exact request was never sent at
            // all" -- a background disconnect watcher can clear a dead
            // connection asynchronously, independent of any specific
            // in-flight call, so a request can still have gone out over
            // a connection that's already been cleared by the time this
            // match runs. For a non-idempotent method, still attempt
            // exactly once (reconnecting first if needed -- there's
            // nothing safer to fall back to), but never retry again if
            // that single attempt itself fails.
            None if !is_safe_to_retry_after_reconnect(method) => {
                if !self.reconnect().await {
                    return Err(ClientError::WebSocket(
                        "no connection available and reconnect failed".to_owned(),
                    ));
                }
                match self.current_websocket() {
                    Some(websocket) => {
                        let full_params = with_profile(params, profile);
                        websocket.call(method, full_params).await
                    }
                    None => Err(ClientError::WebSocket(
                        "reconnect reported success but no connection is available".to_owned(),
                    )),
                }
            }
            None => {
                self.call_after_reconnect(method, params, profile).await
            }
        }
    }

    /// Shared retry tail for `call()`/`call_with_updates()`: attempt
    /// [`Self::reconnect`] (its own bounded retries/timeouts), then
    /// replay the request exactly once more on the fresh connection.
    /// Falls back to the stateless HTTP transport if reconnecting never
    /// succeeds, so a caller still gets a real answer (just without live
    /// notifications) rather than an error a healthy-but-slow gateway
    /// would otherwise turn into a spurious failure. Takes the original
    /// `params`/`profile` (not the WS-shaped `with_profile` embedding),
    /// so the HTTP fallback still gets `profile` the way it actually
    /// expects it -- as a header, not a body field.
    async fn call_after_reconnect(
        &self,
        method: &str,
        params: serde_json::Value,
        profile: Option<&str>,
    ) -> Result<serde_json::Value, ClientError> {
        if self.reconnect().await {
            if let Some(client) = self.current_websocket() {
                let full_params = with_profile(params, profile);
                return client.call(method, full_params).await;
            }
        }
        self.http.call(method, params, profile).await
    }

    /// HTTP returns buffered updates with the response. WebSocket callers
    /// receive them through `subscribe()` as live notifications instead.
    pub async fn call_with_updates(
        &self,
        method: &str,
        params: serde_json::Value,
        profile: Option<&str>,
    ) -> Result<(serde_json::Value, Vec<serde_json::Value>), ClientError> {
        match self.current_websocket() {
            Some(websocket) => {
                let full_params = with_profile(params.clone(), profile);
                match websocket.call_with_updates(method, full_params).await {
                    Err(ClientError::WebSocket(_)) => {
                        self.call_with_updates_after_reconnect(method, params, profile)
                            .await
                    }
                    other => other,
                }
            }
            None => {
                self.call_with_updates_after_reconnect(method, params, profile)
                    .await
            }
        }
    }

    /// See [`Self::call_after_reconnect`] -- same retry/fallback shape
    /// (and same reason it takes the original `params`/`profile`), for
    /// the `_acpx.updates`-returning variant.
    async fn call_with_updates_after_reconnect(
        &self,
        method: &str,
        params: serde_json::Value,
        profile: Option<&str>,
    ) -> Result<(serde_json::Value, Vec<serde_json::Value>), ClientError> {
        if self.reconnect().await {
            if let Some(client) = self.current_websocket() {
                let full_params = with_profile(params, profile);
                return client.call_with_updates(method, full_params).await;
            }
        }
        self.http.call_with_updates(method, params, profile).await
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
        let params = serde_json::json!({"relayId": relay_id, "response": response});
        let no_interactive_connection = || {
            ClientError::WebSocket(
                "acpx/agent_response requires an interactive WebSocket connection; this \
                 Gateway is running in HTTP degraded mode with no live relay to answer"
                    .to_string(),
            )
        };
        let result = match self.current_websocket() {
            Some(websocket) => {
                match websocket.call("acpx/agent_response", params.clone()).await {
                    Err(ClientError::WebSocket(_)) => {
                        if self.reconnect().await {
                            match self.current_websocket() {
                                Some(client) => client.call("acpx/agent_response", params).await?,
                                None => return Err(no_interactive_connection()),
                            }
                        } else {
                            return Err(no_interactive_connection());
                        }
                    }
                    other => other?,
                }
            }
            None => {
                if self.reconnect().await {
                    match self.current_websocket() {
                        Some(client) => client.call("acpx/agent_response", params).await?,
                        None => return Err(no_interactive_connection()),
                    }
                } else {
                    return Err(no_interactive_connection());
                }
            }
        };
        Ok(result
            .get("delivered")
            .and_then(|v| v.as_bool())
            .unwrap_or(false))
    }
}

/// **`acpx-reconnect-retry-duplicates-session-new`.** `false` for methods
/// whose effect is not safe to risk duplicating -- currently just
/// `session/new`, the one genuinely confirmed live to leak real, distinct
/// sessions when retried blindly after a WebSocket failure whose cause
/// might have been a lost *response* rather than a lost *request*. Other
/// methods (`session/prompt` is arguably also unsafe; `session/close`/
/// `session/resume`/`session/load` are closer to idempotent from the
/// client's perspective) are intentionally left retry-eligible here --
/// narrowly scoped to the one method with confirmed real-world impact
/// rather than guessing at every other method's safety up front.
fn is_safe_to_retry_after_reconnect(method: &str) -> bool {
    method != "session/new"
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
