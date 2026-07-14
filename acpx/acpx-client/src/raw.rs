//! Raw ACP client transport: JSON-RPC-over-HTTP against an acpx
//! gateway's `POST /rpc` endpoint (`acpx-server/src/transport/http.rs`).
//! Phase 5 step 20.
//!
//! Intentionally near-zero *interpretation* logic (see
//! `03-crate-and-folder-layout.md`): the "unmodified raw primitives"
//! guarantee from the goal doc means this file never rewrites, validates,
//! or special-cases any ACP method name or params shape -- it only frames
//! a JSON-RPC 2.0 envelope (the exact wire shape `acpx-proto::jsonrpc`
//! describes, ACP's own spec being the shared contract) and unwraps the
//! envelope on the way back. `session/new`, `session/prompt`, etc. all
//! flow through [`GatewayClient::call`] unmodified; `ext/` is the only
//! place acpx-specific typed helpers live, layered strictly on top.
//!
//! **Deviation from the plan's literal step 20 wording** ("depend on a
//! standard ACP client SDK crate for raw protocol primitives"): rather
//! than adopting the official `agent-client-protocol` crate's
//! trait-based `Client` (designed around owning a subprocess's stdio
//! directly, not a remote HTTP gateway), this is a small hand-rolled
//! JSON-RPC-over-HTTP transport matching the wire shape that crate's spec
//! defines. `acpx-proto`'s re-exported `Request`/`Response` types (see
//! below) are still the shared contract for what goes over the wire --
//! only the transport mechanism (HTTP POST vs. owning a child process's
//! stdio) differs from a plain single-agent ACP client, which is the
//! entire point of `acpx` being a gateway a remote client talks to over
//! HTTP/WS rather than a library that spawns its own backend.

pub use acpx_proto::jsonrpc::{Request, RequestId, Response};

use std::sync::atomic::{AtomicI64, Ordering};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("HTTP request to acpx gateway failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("gateway returned a JSON-RPC error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("gateway response had neither \"result\" nor \"error\"")]
    MalformedResponse,
}

/// Raw JSON-RPC-over-HTTP transport to one acpx gateway instance. Every
/// call is a fresh `POST {base_url}/rpc` (matching `http.rs`'s
/// stateless-per-request handling); nothing here is a persistent
/// connection. Agent-initiated `session/update` traffic (the former
/// "reverse-direction messages" gap, now closed server-side -- see
/// `acpx_core::router::read_matching_response`'s doc comment) is *not*
/// pushed live over this HTTP transport -- it's aggregated by the gateway
/// into each response envelope's `_acpx.updates` array instead, which
/// [`GatewayClient::call_with_updates`] surfaces. A future WS-based `raw`
/// transport could still add genuinely live push on top; that remains
/// unbuilt, but a caller no longer *loses* the streamed content in the
/// meantime -- it just arrives batched with the final result rather than
/// incrementally.
pub struct GatewayClient {
    http: reqwest::Client,
    base_url: String,
    next_id: AtomicI64,
    /// Optional bearer token sent as `Authorization: Bearer <token>` on
    /// every call -- matches `acpx-server`'s optional `ACPX_AUTH_TOKEN`
    /// gate (`transport::http::AuthConfig`). `None` by default (every
    /// pre-existing caller of [`Self::new`] is unaffected), set via
    /// [`Self::with_auth_token`].
    auth_token: Option<String>,
}

impl GatewayClient {
    /// `base_url` is the gateway's HTTP origin, e.g. `http://127.0.0.1:8790`
    /// (no trailing slash, no `/rpc` suffix -- that's appended per call).
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into(),
            next_id: AtomicI64::new(1),
            auth_token: None,
        }
    }

    /// Attach a bearer token to send as `Authorization: Bearer <token>` on
    /// every subsequent call -- required when the target gateway was
    /// started with `ACPX_AUTH_TOKEN` set. Builder-style, so callers write
    /// `GatewayClient::new(url).with_auth_token(token)`.
    pub fn with_auth_token(mut self, token: impl Into<String>) -> Self {
        self.auth_token = Some(token.into());
        self
    }

    fn apply_auth(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth_token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    /// Issue one raw JSON-RPC call. `method`/`params` are forwarded
    /// byte-for-byte in the request body -- callers (typically `ext/`
    /// helpers) own picking valid ACP/acpx method names. `profile`, if
    /// set, is sent as the `X-Acpx-Profile` header -- the
    /// highest-precedence profile signal per `02-architecture.md`,
    /// letting a caller select a managed profile without needing to thread
    /// `_acpx.profile` through `params` by hand.
    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
        profile: Option<&str>,
    ) -> Result<serde_json::Value, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut request =
            self.http
                .post(format!("{}/rpc", self.base_url))
                .json(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                    "params": params,
                }));
        if let Some(profile) = profile {
            request = request.header("X-Acpx-Profile", profile);
        }
        request = self.apply_auth(request);
        let body: serde_json::Value = request.send().await?.json().await?;
        if let Some(error) = body.get("error") {
            return Err(ClientError::Rpc {
                code: error.get("code").and_then(|c| c.as_i64()).unwrap_or(0),
                message: error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
        body.get("result")
            .cloned()
            .ok_or(ClientError::MalformedResponse)
    }

    /// Same as [`Self::call`], but also returns whatever the gateway
    /// aggregated into `_acpx.updates` (empty if the backend never emitted
    /// any `session/update` notifications during this call, which is the
    /// common case for gateway-native/non-streaming methods). Callers that
    /// need the actual assistant reply text from a real ACP adapter's
    /// `session/prompt` -- the result itself only ever carries
    /// `{stopReason, usage}`, never message content -- should use this
    /// instead of [`Self::call`].
    pub async fn call_with_updates(
        &self,
        method: &str,
        params: serde_json::Value,
        profile: Option<&str>,
    ) -> Result<(serde_json::Value, Vec<serde_json::Value>), ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut request =
            self.http
                .post(format!("{}/rpc", self.base_url))
                .json(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                    "params": params,
                }));
        if let Some(profile) = profile {
            request = request.header("X-Acpx-Profile", profile);
        }
        request = self.apply_auth(request);
        let body: serde_json::Value = request.send().await?.json().await?;
        if let Some(error) = body.get("error") {
            return Err(ClientError::Rpc {
                code: error.get("code").and_then(|c| c.as_i64()).unwrap_or(0),
                message: error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
        let result = body
            .get("result")
            .cloned()
            .ok_or(ClientError::MalformedResponse)?;
        let updates = body
            .get("_acpx")
            .and_then(|ext| ext.get("updates"))
            .and_then(|u| u.as_array())
            .cloned()
            .unwrap_or_default();
        Ok((result, updates))
    }
}
