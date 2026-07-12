//! HTTP transport for the acpx gateway (Phase 2 step 11, see
//! `memory/acpx/gen/plans/acp-gateway-daemon/04-phased-plan.md`).
//!
//! **No auth/TLS yet** -- see `05-open-risks.md`'s "Transport security for
//! remote access" item. This module binds and serves plaintext HTTP/WS
//! only. Do not bind this to a public interface in production without
//! adding auth/TLS first.
//!
//! Exposes two endpoints on one axum router (WS lives in `ws.rs`, wired in
//! here so both share the same listener and `SharedRouter` state):
//! - `POST /rpc`: JSON-RPC-over-HTTP. Body is a raw JSON-RPC request;
//!   response body is the JSON-RPC response (success or error, both
//!   `200 OK` -- JSON-RPC errors are reported via the body's `error`
//!   field per convention, not via HTTP status).
//! - `GET /ws`: WebSocket upgrade, see `ws.rs`.
//!
//! `X-Acpx-Profile` header handling (`POST /rpc` only -- WS has no
//! per-message header equivalent, see `ws.rs`'s doc comment): per
//! `02-architecture.md`'s precedence section, the header is the
//! *highest*-precedence profile signal, above a `params._acpx.profile`
//! field the client may have set inline. When present on a `session/new`
//! request we set `params._acpx.profile` to the header value
//! unconditionally (overwriting any inline value), then let
//! `Router::dispatch` do its normal `_acpx` resolution/stripping -- this
//! module never needs to duplicate that stripping logic.
//!
//! **Auth (added post-Phase-6 self-review, closing the "No auth/TLS yet"
//! gap this doc comment used to leave open):** optional bearer-token
//! auth, gated on `ACPX_AUTH_TOKEN`. Unset (the default -- every
//! pre-existing test and the real-adapter e2e test all construct servers
//! without it) means fully unauthenticated, byte-for-byte the same
//! behavior as before this change. When set, both `POST /rpc` and the
//! `GET /ws` upgrade require `Authorization: Bearer <token>` matching
//! exactly (checked in constant time, see [`tokens_match`]), else `401
//! Unauthorized` with a JSON-RPC-shaped error body so a client parsing
//! every acpx response as JSON-RPC never has to special-case auth
//! failures. TLS is still not provided by this module -- pair
//! `ACPX_AUTH_TOKEN` with a TLS-terminating reverse proxy for any
//! non-loopback deployment, since a bearer token sent over plaintext HTTP
//! is only as safe as the transport it rides on.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use tokio::sync::Mutex;

use acpx_core::router::{dispatch_shared, Router, RouterError};

/// Shared, lockable handle to the one `Router` instance serving every
/// concurrent HTTP/WS client. The `Mutex` here is intentionally *not* held
/// for the duration of a whole request anymore -- see
/// `acpx_core::router::dispatch_shared`'s doc comment. Every transport in
/// this file calls `dispatch_shared(&router, ...)` rather than
/// `router.lock().await.dispatch(...)`, so this `Mutex` is only ever held
/// briefly for gateway-state bookkeeping, never across a backend agent's
/// real-LLM-latency stdio round trip -- concurrent requests against
/// *different* backend agents now genuinely run in parallel.
pub type SharedRouter = Arc<Mutex<Router>>;

/// Optional bearer token required on every `POST /rpc` request and `GET
/// /ws` upgrade. `None` (the default -- unset `ACPX_AUTH_TOKEN`) disables
/// auth entirely, preserving this workspace's pre-existing unauthenticated
/// behavior byte-for-byte. Cheaply `Clone`-able (`Arc<str>` inside) so it
/// can ride alongside `SharedRouter` in axum's per-request `State`.
#[derive(Clone, Default)]
pub struct AuthConfig {
    token: Option<Arc<str>>,
}

impl AuthConfig {
    pub fn new(token: Option<String>) -> Self {
        Self {
            token: token.map(|t| t.into()),
        }
    }

    /// True if this request's headers carry a valid bearer token, or auth
    /// is disabled entirely. `headers` missing the header, or carrying a
    /// non-`Bearer` / mismatched value, is unauthorized whenever a token
    /// is configured.
    pub(crate) fn authorize(&self, headers: &HeaderMap) -> bool {
        let Some(expected) = &self.token else {
            return true; // auth disabled
        };
        let Some(header_value) = headers.get(axum::http::header::AUTHORIZATION) else {
            return false;
        };
        let Ok(header_value) = header_value.to_str() else {
            return false;
        };
        let Some(presented) = header_value.strip_prefix("Bearer ") else {
            return false;
        };
        tokens_match(presented, expected)
    }
}

/// Constant-time byte comparison -- deliberately not `==`/`str::eq`, which
/// short-circuit on the first mismatched byte and so leak timing
/// information proportional to how many leading bytes of a guess happen
/// to match the real token. No new dependency pulled in for this (no
/// `subtle` crate in this workspace's `Cargo.toml` yet) -- a manual
/// XOR-accumulate over every byte is a well-known, dependency-free
/// constant-time-comparison idiom sufficient for a bearer token check.
/// Differing lengths short-circuit (a length mismatch is not
/// secret-dependent information worth hiding -- token length isn't
/// itself sensitive).
fn tokens_match(presented: &str, expected: &str) -> bool {
    let (a, b) = (presented.as_bytes(), expected.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Combined axum `State` for both endpoints: the shared router plus the
/// (usually-disabled) auth config. A plain tuple `(SharedRouter,
/// AuthConfig)` would also implement `FromRef`, but a named struct keeps
/// `ws.rs`'s handler signature self-documenting.
#[derive(Clone)]
pub struct AppState {
    pub router: SharedRouter,
    pub auth: AuthConfig,
}

/// Header carrying an explicit profile selection, highest precedence per
/// `02-architecture.md`. `axum`'s `HeaderMap` lookups are already
/// case-insensitive, so this lowercase constant matches `X-Acpx-Profile`
/// regardless of how the client cases it.
const PROFILE_HEADER: &str = "x-acpx-profile";

/// Start the HTTP/WS transport, serving `POST /rpc` and `GET /ws` against
/// the given shared `Router` until the listener errors or the task is
/// dropped/cancelled. Intended to be spawned as its own task (or awaited
/// directly) from `main.rs` alongside the stdio transport.
///
/// `auth_token`: `None` (pass `None` for the pre-existing, unauthenticated
/// behavior every test in this workspace already relies on) disables
/// auth; `Some(token)` requires `Authorization: Bearer <token>` on every
/// `POST /rpc` and the `GET /ws` upgrade. See this module's doc comment.
pub async fn serve(
    router: SharedRouter,
    bind_addr: SocketAddr,
    auth_token: Option<String>,
) -> anyhow::Result<()> {
    let state = AppState {
        router,
        auth: AuthConfig::new(auth_token),
    };
    let auth_enabled = state.auth.token.is_some();
    let app = axum::Router::new()
        .route("/rpc", post(rpc_handler))
        .route("/ws", get(super::ws::ws_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    tracing::info!(
        %bind_addr,
        auth_enabled,
        "acpx-server HTTP/WS transport listening (no TLS -- see 05-open-risks.md; \
         set ACPX_AUTH_TOKEN for bearer-token auth)"
    );
    axum::serve(listener, app).await?;
    Ok(())
}

async fn rpc_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut request): Json<serde_json::Value>,
) -> Response {
    if !state.auth.authorize(&headers) {
        return unauthorized_response(&request);
    }
    inject_profile_header(&headers, &mut request);
    let response = match dispatch_shared(&state.router, request.clone()).await {
        Ok(response) => response,
        Err(err) => json_rpc_error(&request, err),
    };
    Json(response).into_response()
}

/// `401 Unauthorized` with a JSON-RPC-shaped error body (same envelope as
/// [`json_rpc_error`]) rather than an empty 401 -- keeps the wire contract
/// JSON-RPC-consistent for a client that always parses the response body
/// as JSON-RPC, auth failure or not.
fn unauthorized_response(request: &serde_json::Value) -> Response {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": request.get("id").cloned().unwrap_or(serde_json::Value::Null),
        "error": {
            "code": -32001,
            "message": "unauthorized: missing or invalid bearer token",
        }
    });
    (StatusCode::UNAUTHORIZED, Json(body)).into_response()
}

/// Per the precedence rule, an `X-Acpx-Profile` header on a `session/new`
/// request sets (and overwrites, if already present) `params._acpx.profile`
/// before dispatch. No-op for every other method or when the header is
/// absent -- `Router` itself falls through to the `_acpx` field or native
/// mode exactly as it does for stdio clients.
fn inject_profile_header(headers: &HeaderMap, request: &mut serde_json::Value) {
    let Some(profile) = headers
        .get(PROFILE_HEADER)
        .and_then(|value| value.to_str().ok())
    else {
        return;
    };
    if request.get("method").and_then(|m| m.as_str()) != Some("session/new") {
        return;
    }
    // Ensure params is an object we can inject into, creating one if the
    // request omitted it (or set it to something non-object, which we
    // treat as absent -- `Router::dispatch` surfaces a clearer error for a
    // genuinely malformed request than we could here).
    if !matches!(request.get("params"), Some(serde_json::Value::Object(_))) {
        request["params"] = serde_json::json!({});
    }
    request["params"]["_acpx"] = serde_json::json!({ "profile": profile });
}

/// Build a JSON-RPC error response body for a `RouterError`, echoing the
/// request's own `id` (or `null` if it had none) per JSON-RPC convention.
/// Shared with `ws.rs` since both transports need the same shape.
pub(crate) fn json_rpc_error(request: &serde_json::Value, err: RouterError) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": request.get("id").cloned().unwrap_or(serde_json::Value::Null),
        "error": {
            "code": -32000,
            "message": err.to_string(),
        }
    })
}
