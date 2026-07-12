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

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
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

/// Header carrying an explicit profile selection, highest precedence per
/// `02-architecture.md`. `axum`'s `HeaderMap` lookups are already
/// case-insensitive, so this lowercase constant matches `X-Acpx-Profile`
/// regardless of how the client cases it.
const PROFILE_HEADER: &str = "x-acpx-profile";

/// Start the HTTP/WS transport, serving `POST /rpc` and `GET /ws` against
/// the given shared `Router` until the listener errors or the task is
/// dropped/cancelled. Intended to be spawned as its own task (or awaited
/// directly) from `main.rs` alongside the stdio transport.
pub async fn serve(router: SharedRouter, bind_addr: SocketAddr) -> anyhow::Result<()> {
    let app = axum::Router::new()
        .route("/rpc", post(rpc_handler))
        .route("/ws", get(super::ws::ws_handler))
        .with_state(router);

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    tracing::info!(
        %bind_addr,
        "acpx-server HTTP/WS transport listening (no auth/TLS -- see 05-open-risks.md)"
    );
    axum::serve(listener, app).await?;
    Ok(())
}

async fn rpc_handler(
    State(router): State<SharedRouter>,
    headers: HeaderMap,
    Json(mut request): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    inject_profile_header(&headers, &mut request);
    let response = match dispatch_shared(&router, request.clone()).await {
        Ok(response) => response,
        Err(err) => json_rpc_error(&request, err),
    };
    Json(response)
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
