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
use std::time::Duration;

/// Hard ceiling on one `POST {base_url}/rpc` round trip, `send()` through
/// reading the full response body. `reqwest::Client::new()` (this
/// module's previous construction) sets **no** timeout at all -- a
/// `session/prompt` call is a single blocking HTTP request the gateway
/// only answers once the whole turn (or an explicit error) resolves, so a
/// wedged gateway/backend (crashed or hung without closing the TCP
/// connection) left the caller's future pending forever: no
/// `AgentEvent::TurnEnded`, no `AgentEvent::Error`, nothing for
/// `panel-rust`'s reducer to react to, so the thread's busy/loading state
/// (set the moment the request went out) never clears. Mirrors
/// `acpx_client::ws::WS_RESPONSE_TIMEOUT`'s exact reasoning and value --
/// that module already documents this raw HTTP transport as its
/// fallback for "constrained deployments", so it needs the same
/// protection, not a shorter one: long enough to comfortably outlast any
/// legitimate long-running turn or permission-approval wait
/// (`acpx_core::router`'s own `BACKEND_IDLE_READ_TIMEOUT` backstop is 20
/// minutes), only there to catch a connection that looks alive at the
/// TCP level but will never actually answer.
pub const HTTP_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("HTTP request to acpx gateway failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("gateway returned a JSON-RPC error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("gateway response had neither \"result\" nor \"error\"")]
    MalformedResponse,
    #[error("WebSocket request to acpx gateway failed: {0}")]
    WebSocket(String),
    /// Local request-construction failure (e.g. a client-computed
    /// `mcpServers` entry that does not deserialize as an ACP `McpServer`).
    /// Not a transport or gateway error -- never retryable.
    #[error("invalid request parameters: {0}")]
    InvalidParams(String),
}

const RUNTIME_SHUTDOWN_ERROR_SUBSTR: &str = "being shutdown";

pub fn is_runtime_shutdown_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("tokio") && message.contains(RUNTIME_SHUTDOWN_ERROR_SUBSTR)
}

impl ClientError {
    /// Only transport loss and gateway-startup/recovery responses are safe
    /// to retry. Authentication and capacity errors are stable server-side
    /// conditions; retrying them just repeats the same failure or creates
    /// pressure while the user still has no valid credentials/capacity.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::WebSocket(message) => !is_runtime_shutdown_error(message),
            Self::Http(error) => error.is_connect() || error.is_timeout(),
            Self::Rpc { message, .. } => {
                let message = message.to_ascii_lowercase();
                (message.contains("restor") || message.contains("starting"))
                    && !message.contains("authentication")
                    && !message.contains("capacity")
            }
            Self::MalformedResponse | Self::InvalidParams(_) => false,
        }
    }

    pub fn is_authentication_or_capacity(&self) -> bool {
        match self {
            Self::Rpc { message, .. } => {
                let message = message.to_ascii_lowercase();
                message.contains("authentication") || message.contains("capacity")
            }
            _ => false,
        }
    }

    pub fn is_runtime_shutdown(&self) -> bool {
        matches!(self, Self::WebSocket(message) if is_runtime_shutdown_error(message))
    }
}

#[cfg(test)]
mod tests {
    use super::ClientError;

    #[test]
    fn retries_only_transport_and_startup_recovery_errors() {
        assert!(ClientError::WebSocket("connection reset".into()).is_transient());
        assert!(ClientError::Rpc {
            code: -32603,
            message: "gateway starting".into(),
        }
        .is_transient());
        assert!(ClientError::Rpc {
            code: -32000,
            message: "session is restoring".into(),
        }
        .is_transient());
        assert!(!ClientError::Rpc {
            code: -32000,
            message: "backend requires authentication before session/new".into(),
        }
        .is_transient());
        assert!(ClientError::Rpc {
            code: -32000,
            message: "backend requires authentication before session/new".into(),
        }
        .is_authentication_or_capacity());
        assert!(ClientError::Rpc {
            code: -32000,
            message: "session capacity reached for tenant default: 512/512 live gateway sessions"
                .into(),
        }
        .is_authentication_or_capacity());
    }

    #[test]
    fn runtime_shutdown_websocket_errors_are_not_transient() {
        let error = ClientError::WebSocket(
            "IO error: A Tokio 1.x context was found, but it is being shutdown.".into(),
        );
        assert!(!error.is_transient());
        assert!(error.is_runtime_shutdown());
        assert!(ClientError::WebSocket("connection reset by peer".into()).is_transient());
    }
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
        Self::with_timeout(base_url, HTTP_RESPONSE_TIMEOUT)
    }

    /// Same as [`Self::new`], but with an explicit per-request timeout
    /// instead of [`HTTP_RESPONSE_TIMEOUT`] -- exists so tests can prove
    /// the timeout behavior (a wedged/never-responding peer) without
    /// actually waiting 30 minutes. Every real caller should use
    /// [`Self::new`].
    pub fn with_timeout(base_url: impl Into<String>, timeout: Duration) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
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

#[cfg(test)]
mod http_timeout_tests {
    //! Regression coverage for the "left loading forever" gap this file's
    //! `HTTP_RESPONSE_TIMEOUT` closes (see `GatewayClient::new`'s doc
    //! comment): before this fix, `GatewayClient` was built from
    //! `reqwest::Client::new()`, which sets no timeout at all, so a peer
    //! that accepts the TCP connection but never writes a response byte
    //! (a wedged gateway/backend -- crashed or hung without closing the
    //! socket) left `call`/`call_with_updates` pending indefinitely: no
    //! `Ok`, no `Err`, forever. A caller awaiting that future (panel-
    //! rust's `Command::SendPrompt` handler) never gets *anything* to
    //! react to, so the thread's busy/loading UI state, set the moment
    //! the request went out, never clears -- exactly the bug this proves
    //! is now closed.
    use super::{ClientError, GatewayClient};
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    /// Binds a listener that accepts one connection, reads whatever the
    /// client sends (so the write side doesn't itself block/reset), and
    /// then holds the socket open forever without ever writing a
    /// response -- the "wedged peer" this test proves `GatewayClient` no
    /// longer hangs against.
    async fn spawn_wedged_peer() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                // Drain the request so the client's write completes, then
                // simply never respond -- and never let the task/socket
                // drop, which would otherwise send a RST/FIN the client
                // could (correctly) treat as an ordinary connection-reset
                // error rather than exercising the timeout path.
                loop {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => continue,
                    }
                }
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn call_with_updates_times_out_instead_of_hanging_forever() {
        let base_url = spawn_wedged_peer().await;
        let client = GatewayClient::with_timeout(base_url, Duration::from_millis(300));

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            client.call_with_updates("session/prompt", serde_json::json!({}), None),
        )
        .await
        .expect(
            "GatewayClient::call_with_updates hung past its own configured timeout -- \
             the wedged-peer protection regressed",
        );

        match outcome {
            Err(ClientError::Http(error)) => {
                assert!(
                    error.is_timeout(),
                    "expected the configured request timeout to fire, got a different \
                     reqwest error: {error:?}"
                );
            }
            other => panic!("expected a timeout ClientError::Http, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn call_times_out_instead_of_hanging_forever() {
        let base_url = spawn_wedged_peer().await;
        let client = GatewayClient::with_timeout(base_url, Duration::from_millis(300));

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            client.call("session/prompt", serde_json::json!({}), None),
        )
        .await
        .expect("GatewayClient::call hung past its own configured timeout");

        let is_http_timeout =
            matches!(&outcome, Err(ClientError::Http(error)) if error.is_timeout());
        assert!(
            is_http_timeout,
            "expected a timeout ClientError::Http, got {outcome:?}"
        );
    }
}
