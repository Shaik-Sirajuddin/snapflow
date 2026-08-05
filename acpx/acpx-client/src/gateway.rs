//! Transport-neutral ACPX gateway facade.
//!
//! `Gateway` is the only API panel code should use. It prefers a persistent
//! WebSocket connection, exposes live notifications in that mode, and makes
//! HTTP fallback explicit so callers cannot accidentally show unavailable
//! interactive controls.

use crate::raw::{ClientError, GatewayClient};
use crate::ws::{GatewayNotification, GatewayWsClient, SessionSubscription};
use agent_client_protocol::schema::v1::{
    McpServer, NewSessionRequest, ResumeSessionRequest, SessionId,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::broadcast;

/// ACP metadata namespace used to make one logical `session/new` request
/// identifiable across a lost-response reconnect.
pub const SESSION_CREATION_META_NAMESPACE: &str = "com.example.client";

/// Client-owned identity for one logical `session/new` operation.
///
/// A caller that performs its own higher-level retry should construct this
/// once and reuse it. [`Gateway::call`] also ensures that raw `session/new`
/// calls carry a generated identity before their transport attempt, so the
/// reconnect replay of that call uses the exact same value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCreationMetadata {
    pub creation_request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

impl SessionCreationMetadata {
    /// Generate a fresh identity for a new logical creation operation.
    pub fn generated() -> Self {
        Self {
            creation_request_id: uuid::Uuid::new_v4().to_string(),
            workspace_id: None,
        }
    }

    /// Use a caller-supplied UUID, for example when retry state is persisted
    /// outside this process.
    pub fn new(creation_request_id: impl Into<String>) -> Result<Self, ClientError> {
        let metadata = Self {
            creation_request_id: creation_request_id.into(),
            workspace_id: None,
        };
        metadata.validate()?;
        Ok(metadata)
    }

    /// Attach optional client correlation metadata. Empty workspace ids are
    /// rejected so the client and server enforce the same contract.
    pub fn with_workspace_id(
        mut self,
        workspace_id: impl Into<String>,
    ) -> Result<Self, ClientError> {
        self.workspace_id = Some(workspace_id.into());
        self.validate()?;
        Ok(self)
    }

    fn validate(&self) -> Result<(), ClientError> {
        uuid::Uuid::parse_str(&self.creation_request_id).map_err(|err| {
            ClientError::InvalidParams(format!("creationRequestId must be a UUID: {err}"))
        })?;
        if self.workspace_id.as_deref().is_some_and(str::is_empty) {
            return Err(ClientError::InvalidParams(
                "workspaceId must not be empty".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Merge creation identity into an ACP `session/new` params object while
/// preserving unrelated `_meta` namespaces.
pub fn with_session_creation_metadata(
    mut params: serde_json::Value,
    metadata: &SessionCreationMetadata,
) -> Result<serde_json::Value, ClientError> {
    metadata.validate()?;
    let params_object = params.as_object_mut().ok_or_else(|| {
        ClientError::InvalidParams("session/new params must be an object".to_owned())
    })?;
    let meta = params_object
        .entry("_meta")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let meta_object = meta.as_object_mut().ok_or_else(|| {
        ClientError::InvalidParams("session/new params._meta must be an object".to_owned())
    })?;
    meta_object.insert(
        SESSION_CREATION_META_NAMESPACE.to_owned(),
        serde_json::to_value(metadata).map_err(|err| {
            ClientError::InvalidParams(format!(
                "failed to serialize session creation metadata: {err}"
            ))
        })?,
    );
    Ok(params)
}

/// Ensure raw `session/new` params have a client creation identity. Existing
/// valid metadata is retained verbatim, which is what keeps a caller-supplied
/// identity stable across higher-level retries.
pub fn ensure_session_creation_metadata(
    params: serde_json::Value,
) -> Result<serde_json::Value, ClientError> {
    match session_creation_metadata(&params)? {
        Some(_) => Ok(params),
        None => with_session_creation_metadata(params, &SessionCreationMetadata::generated()),
    }
}

fn session_creation_metadata(
    params: &serde_json::Value,
) -> Result<Option<SessionCreationMetadata>, ClientError> {
    let Some(params_object) = params.as_object() else {
        return Err(ClientError::InvalidParams(
            "session/new params must be an object".to_owned(),
        ));
    };
    let Some(meta) = params_object.get("_meta") else {
        return Ok(None);
    };
    let meta_object = meta.as_object().ok_or_else(|| {
        ClientError::InvalidParams("session/new params._meta must be an object".to_owned())
    })?;
    let Some(client_meta) = meta_object.get(SESSION_CREATION_META_NAMESPACE) else {
        return Ok(None);
    };
    let metadata: SessionCreationMetadata =
        serde_json::from_value(client_meta.clone()).map_err(|err| {
            ClientError::InvalidParams(format!(
                "invalid {SESSION_CREATION_META_NAMESPACE} session creation metadata: {err}"
            ))
        })?;
    metadata.validate()?;
    Ok(Some(metadata))
}

/// Build typed ACP `session/new` params and attach ACPX's client creation
/// identity extension. The MCP entries are validated at this boundary.
pub fn build_new_session_params(
    cwd: impl AsRef<Path>,
    mcp_servers: &[serde_json::Value],
    metadata: &SessionCreationMetadata,
) -> Result<serde_json::Value, ClientError> {
    let typed_servers = deserialize_mcp_servers(mcp_servers)?;
    let request = NewSessionRequest::new(PathBuf::from(cwd.as_ref())).mcp_servers(typed_servers);
    let params = serde_json::to_value(&request).map_err(|err| {
        ClientError::InvalidParams(format!("failed to serialize NewSessionRequest: {err}"))
    })?;
    with_session_creation_metadata(params, metadata)
}

fn deserialize_mcp_servers(
    mcp_servers: &[serde_json::Value],
) -> Result<Vec<McpServer>, ClientError> {
    mcp_servers
        .iter()
        .cloned()
        .map(|entry| {
            serde_json::from_value::<McpServer>(entry).map_err(|err| {
                ClientError::InvalidParams(format!(
                    "mcpServers entry failed to deserialize as ACP McpServer: {err}"
                ))
            })
        })
        .collect()
}

/// Build typed `session/resume` params carrying the current client-computed
/// MCP server list.
///
/// Uses the real ACP `ResumeSessionRequest` builder (not a hand-rolled
/// `json!({...})` literal) so the wire shape stays aligned with the
/// protocol schema. `mcp_servers` is the same `Vec<Value>` panel-rust
/// already computes for `session/new` (`snapflowd_mcp_servers_entry`);
/// each entry is deserialized into `McpServer` at this boundary -- a
/// shape mismatch is a real bug and is surfaced via
/// [`ClientError::InvalidParams`], never silently dropped to empty.
pub fn build_resume_session_params(
    session_id: &str,
    cwd: impl AsRef<Path>,
    mcp_servers: &[serde_json::Value],
) -> Result<serde_json::Value, ClientError> {
    let typed_servers = deserialize_mcp_servers(mcp_servers)?;
    let request =
        ResumeSessionRequest::new(SessionId::new(session_id), PathBuf::from(cwd.as_ref()))
            .mcp_servers(typed_servers);
    serde_json::to_value(&request).map_err(|err| {
        ClientError::InvalidParams(format!("failed to serialize ResumeSessionRequest: {err}"))
    })
}

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
/// Bound the first WebSocket handshake too. A delayed initial handshake must
/// degrade to a later bounded request error, not strand thread actors.
const INITIAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Gateway {
    base_url: String,
    http: GatewayClient,
    // A plain (non-async) RwLock: every access here is a quick clone of
    // the `Arc` followed immediately by dropping the guard, never held
    // across an `.await`, so there's no risk of blocking the async
    // runtime -- and it lets `mode()`/`subscribe()` stay synchronous
    // (matching their existing signatures; no ripple to every caller).
    websocket: RwLock<Option<Arc<GatewayWsClient>>>,
    /// Serializes reconnect attempts from the session and out-of-band
    /// forwarders. Without this, both observers can create independent fresh
    /// sockets for one disconnect and continue receiving duplicate updates.
    reconnect_lock: tokio::sync::Mutex<()>,
    session_replays: Mutex<HashMap<String, SessionReplay>>,
    queue_subscriptions: Mutex<HashSet<String>>,
}

#[derive(Clone)]
struct SessionReplay {
    _method: String,
    params: serde_json::Value,
    profile: Option<String>,
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
        let websocket =
            tokio::time::timeout(INITIAL_CONNECT_TIMEOUT, GatewayWsClient::connect(&base_url))
                .await
                .ok()
                .and_then(Result::ok);
        Self {
            base_url,
            http,
            websocket: RwLock::new(websocket),
            reconnect_lock: tokio::sync::Mutex::new(()),
            session_replays: Mutex::new(HashMap::new()),
            queue_subscriptions: Mutex::new(HashSet::new()),
        }
    }

    pub fn http_degraded(base_url: impl Into<String>) -> Self {
        let base_url = base_url.into();
        Self {
            http: GatewayClient::new(base_url.clone()),
            base_url,
            websocket: RwLock::new(None),
            reconnect_lock: tokio::sync::Mutex::new(()),
            session_replays: Mutex::new(HashMap::new()),
            queue_subscriptions: Mutex::new(HashSet::new()),
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
        let _reconnect_guard = self.reconnect_lock.lock().await;
        // Another observer may have completed the reconnect while this
        // caller was waiting for the single-flight gate. Reuse that socket
        // instead of opening a second live subscription.
        if self
            .current_websocket()
            .is_some_and(|client| !client.is_disconnected())
        {
            return true;
        }
        for attempt in 1..=RECONNECT_MAX_ATTEMPTS {
            let attempt_result = tokio::time::timeout(
                RECONNECT_ATTEMPT_TIMEOUT,
                GatewayWsClient::connect(&self.base_url),
            )
            .await;
            if let Ok(Ok(client)) = attempt_result {
                *self
                    .websocket
                    .write()
                    .expect("gateway websocket lock poisoned") = Some(client);
                let session_ids = self
                    .queue_subscriptions
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>();
                if let Some(client) = self.current_websocket() {
                    if !session_ids.is_empty() {
                        let _ = client
                            .call(
                                "acpx/sessions/queue/subscribe",
                                serde_json::json!({ "sessionIds": session_ids }),
                            )
                            .await;
                    }
                }
                return true;
            }
            if attempt < RECONNECT_MAX_ATTEMPTS {
                tokio::time::sleep(RECONNECT_BACKOFF_STEP * attempt).await;
            }
        }
        *self
            .websocket
            .write()
            .expect("gateway websocket lock poisoned") = None;
        false
    }

    pub fn subscribe(&self) -> Option<broadcast::Receiver<GatewayNotification>> {
        self.current_websocket().map(|client| client.subscribe())
    }

    /// Subscribes to notifications for one ACP session on the shared
    /// WebSocket. Responses still use the pending request map; only live
    /// notifications are demultiplexed here.
    pub fn subscribe_session(&self, session_id: &str) -> Option<SessionSubscription> {
        self.current_websocket()
            .map(|client| client.subscribe_session(session_id))
    }

    /// Registers and establishes the separate server-owned queue stream for
    /// explicit session ids. The registry is replayed on every WebSocket
    /// reconnect, so queue callbacks cannot silently stop after a gateway
    /// restart while the panel still holds the same session actors.
    pub async fn subscribe_queue_sessions(
        &self,
        session_ids: &[String],
        cursor: Option<&str>,
    ) -> Result<serde_json::Value, ClientError> {
        {
            let mut subscriptions = self
                .queue_subscriptions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            subscriptions.extend(session_ids.iter().cloned());
        }
        self.call(
            "acpx/sessions/queue/subscribe",
            serde_json::json!({ "sessionIds": session_ids, "cursor": cursor }),
            None,
        )
        .await
    }

    /// Records the idempotent session operation needed to re-establish the
    /// server-side WebSocket watch after a reconnect. The caller should invoke
    /// this before sending `session/load`/`session/resume`, so an early replay
    /// notification cannot arrive before the local receiver exists.
    pub fn register_session_replay(
        &self,
        session_id: impl Into<String>,
        method: impl Into<String>,
        params: serde_json::Value,
        profile: Option<String>,
    ) {
        self.session_replays
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                session_id.into(),
                SessionReplay {
                    _method: method.into(),
                    params,
                    profile,
                },
            );
    }

    pub fn unregister_session_replay(&self, session_id: &str) {
        self.session_replays
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id);
    }

    /// Re-establishes one registered session binding on the current
    /// WebSocket. Reconnects deliberately use `session/resume`, not the
    /// originally registered `session/load`: the panel owns its local
    /// transcript and should not ask the gateway to replay it. The caller
    /// must install its local `subscribe_session` receiver
    /// before invoking this method.
    pub async fn rehydrate_session(&self, session_id: &str) -> Result<(), ClientError> {
        let replay = self
            .session_replays
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .cloned();
        let Some(replay) = replay else {
            return Ok(());
        };
        // Re-establish the independent queue watch before resuming the ACP
        // session so queued-state notifications cannot be missed during the
        // reconnect window. Queue events intentionally do not ride through
        // native session/resume.
        self.subscribe_queue_sessions(&[session_id.to_owned()], None)
            .await?;
        let Some(client) = self.current_websocket() else {
            return Err(ClientError::WebSocket(
                "no WebSocket available for session rehydration".to_owned(),
            ));
        };
        // Prefer the cwd/mcpServers recorded when the session was first
        // bound (OpenSession registers them on the load replay contract;
        // create/resume paths should too). Fall back to empty cwd / empty
        // mcp list only when the registered params genuinely lack them.
        let cwd = replay
            .params
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let mcp_servers = match replay.params.get("mcpServers") {
            Some(serde_json::Value::Array(entries)) => entries.as_slice(),
            _ => &[],
        };
        let params = build_resume_session_params(session_id, cwd, mcp_servers)?;
        client
            .call(
                "session/resume",
                with_profile(params, replay.profile.as_deref()),
            )
            .await
            .map(|_| ())
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
        // Generate at most once for this logical call, before either the
        // first send or reconnect branch can clone/replay the params.
        let params = prepare_call_params(method, params)?;
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
                    Err(ClientError::WebSocket(_))
                        if !is_safe_to_retry_after_reconnect(method, &params) =>
                    {
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
            None if !is_safe_to_retry_after_reconnect(method, &params) => {
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
            None => self.call_after_reconnect(method, params, profile).await,
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
        let params = prepare_call_params(method, params)?;
        match self.current_websocket() {
            Some(websocket) => {
                let full_params = with_profile(params.clone(), profile);
                match websocket.call_with_updates(method, full_params).await {
                    Err(ClientError::WebSocket(_))
                        if !is_safe_to_retry_after_reconnect(method, &params) =>
                    {
                        Err(ClientError::WebSocket(
                            "not retrying a non-idempotent method after a WebSocket failure \
                             (the request may have already reached the server)"
                                .to_owned(),
                        ))
                    }
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
            Some(websocket) => match websocket.call("acpx/agent_response", params.clone()).await {
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
            },
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

/// Prepare one logical call before its first transport attempt. The returned
/// params are the sole value used by both the initial send and reconnect
/// replay, so a generated creation id cannot drift between attempts.
fn prepare_call_params(
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, ClientError> {
    if method == "session/new" {
        ensure_session_creation_metadata(params)
    } else {
        Ok(params)
    }
}

/// A `session/new` replay is safe only when the request carries the durable
/// server deduplication key. Other methods keep their existing retry policy.
fn is_safe_to_retry_after_reconnect(method: &str, params: &serde_json::Value) -> bool {
    method != "session/new" || matches!(session_creation_metadata(params), Ok(Some(_)))
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

    #[test]
    fn session_new_builder_serializes_client_identity_and_preserves_acp_shape() {
        let metadata = SessionCreationMetadata::new("7ee35d23-17d5-4a62-92cb-d36d857fc272")
            .expect("valid UUID")
            .with_workspace_id("project-42")
            .expect("non-empty workspace id");
        let params = build_new_session_params(
            "/tmp/project",
            &[serde_json::json!({
                "name": "skills",
                "command": "/usr/bin/snapflowd-mcp",
                "args": [],
                "env": [],
            })],
            &metadata,
        )
        .expect("well-formed session/new params");

        assert_eq!(params["cwd"], "/tmp/project");
        assert_eq!(params["mcpServers"][0]["name"], "skills");
        assert_eq!(
            params["_meta"][SESSION_CREATION_META_NAMESPACE],
            serde_json::json!({
                "creationRequestId": "7ee35d23-17d5-4a62-92cb-d36d857fc272",
                "workspaceId": "project-42",
            })
        );
    }

    #[test]
    fn preparation_is_stable_and_preserves_other_meta_namespaces() {
        let unprepared = serde_json::json!({
            "cwd": "/tmp/project",
            "mcpServers": []
        });
        assert!(
            !is_safe_to_retry_after_reconnect("session/new", &unprepared),
            "both call() and call_with_updates() must fail closed before creation metadata exists"
        );

        let first = prepare_call_params(
            "session/new",
            serde_json::json!({
                "cwd": unprepared["cwd"],
                "mcpServers": unprepared["mcpServers"],
                "_meta": {"org.example.trace": {"traceId": "trace-1"}}
            }),
        )
        .expect("first preparation");
        let retry = prepare_call_params("session/new", first.clone())
            .expect("retry preparation retains the identity");

        assert_eq!(
            first["_meta"][SESSION_CREATION_META_NAMESPACE]["creationRequestId"],
            retry["_meta"][SESSION_CREATION_META_NAMESPACE]["creationRequestId"]
        );
        assert!(uuid::Uuid::parse_str(
            first["_meta"][SESSION_CREATION_META_NAMESPACE]["creationRequestId"]
                .as_str()
                .expect("creationRequestId string")
        )
        .is_ok());
        assert_eq!(retry["_meta"]["org.example.trace"]["traceId"], "trace-1");
        assert!(is_safe_to_retry_after_reconnect("session/new", &retry));
    }

    #[test]
    fn caller_supplied_identity_is_not_replaced_and_invalid_metadata_is_rejected() {
        let supplied = serde_json::json!({
            "cwd": "/tmp/project",
            "_meta": {
                "com.example.client": {
                    "creationRequestId": "7ee35d23-17d5-4a62-92cb-d36d857fc272"
                }
            }
        });
        let prepared =
            prepare_call_params("session/new", supplied.clone()).expect("valid supplied metadata");
        assert_eq!(prepared, supplied);

        let invalid = serde_json::json!({
            "cwd": "/tmp/project",
            "_meta": {
                "com.example.client": {
                    "creationRequestId": "not-a-uuid"
                }
            }
        });
        let err = prepare_call_params("session/new", invalid)
            .expect_err("invalid caller identity must fail before transport");
        assert!(matches!(err, ClientError::InvalidParams(_)));
    }

    #[test]
    fn failed_session_binding_can_be_removed_from_replay_registry() {
        let gateway = Gateway::http_degraded("http://127.0.0.1:1");
        gateway.register_session_replay(
            "failed-session",
            "session/load",
            serde_json::json!({"sessionId": "failed-session", "cwd": "/tmp"}),
            None,
        );
        assert!(gateway
            .session_replays
            .lock()
            .expect("replay registry lock")
            .contains_key("failed-session"));

        gateway.unregister_session_replay("failed-session");
        assert!(!gateway
            .session_replays
            .lock()
            .expect("replay registry lock")
            .contains_key("failed-session"));
    }

    /// Broad coverage for typed session/resume request construction:
    /// happy-path mcpServers round-trip, empty list allowed, malformed
    /// entry rejected (not silently dropped).
    #[test]
    fn build_resume_session_params_covers_shape_and_validation() {
        let mcp = vec![serde_json::json!({
            "name": "skills",
            "command": "/usr/bin/snapflowd-mcp",
            "args": ["--global-dir", "/tmp/skills"],
            "env": [],
        })];
        let params = build_resume_session_params("sess-1", "/tmp/project", &mcp)
            .expect("well-formed mcpServers must serialize");
        assert_eq!(params["sessionId"], "sess-1");
        assert_eq!(params["cwd"], "/tmp/project");
        let servers = params["mcpServers"]
            .as_array()
            .expect("mcpServers key must be present");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0]["name"], "skills");
        assert_eq!(servers[0]["command"], "/usr/bin/snapflowd-mcp");

        let empty = build_resume_session_params("sess-2", "/home/me", &[])
            .expect("empty mcp list is valid");
        assert_eq!(empty["sessionId"], "sess-2");
        if let Some(servers) = empty.get("mcpServers") {
            assert_eq!(servers, &serde_json::json!([]));
        }

        let err = build_resume_session_params(
            "sess-3",
            "/tmp",
            &[serde_json::json!({"name": "broken", "notARealField": true})],
        )
        .expect_err("malformed entry must not be silently dropped");
        assert!(
            matches!(err, ClientError::InvalidParams(ref m) if m.contains("mcpServers")),
            "expected InvalidParams naming mcpServers, got {err:?}"
        );
    }
}
