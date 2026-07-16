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
//!
//! **Tenant isolation (`acpx-tenant-isolation` Phase B).** Optional
//! `X-Acpx-Tenant` header, read fresh on every `POST /rpc` request (no
//! persistent connection state to cache it in, unlike WS -- see `ws.rs`'s
//! doc comment for that transport's equivalent, read-once-at-upgrade
//! handling). Absent means the implicit `"default"` tenant -- byte-for-
//! byte the same behavior as before this feature existed. This is a
//! self-declared partition key, **not** an authentication mechanism: see
//! `memory/acpx/gen/plans/acpx-tenant-isolation/00-goal.md`'s "Why auth
//! is out of scope" section. Applies to every method on this connection,
//! not just `session/new` (unlike `X-Acpx-Profile`, which only matters
//! for session creation) -- see that plan's `01-architecture.md` for why
//! this is a header rather than an `_acpx.tenant` request field.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use tokio::sync::Mutex;

use acpx_core::router::{dispatch_shared_for_tenant, Router, RouterError};
use acpx_core::TenantId;

#[path = "acp_bridge.rs"]
pub(crate) mod acp_bridge;

use self::acp_bridge::BridgeRuntime;

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
    /// `None` means `/acp/*` is intentionally not mounted. This is a
    /// feature gate, not an authorization check.
    pub bridge: Option<Arc<acpx_bridge::BridgeConfig>>,
    /// Virtual-session state exists only alongside an enabled bridge
    /// policy. It is shared by `/acp/rpc` and `/acp/ws`.
    pub bridge_runtime: Option<Arc<BridgeRuntime>>,
}

/// Header carrying an explicit profile selection, highest precedence per
/// `02-architecture.md`. `axum`'s `HeaderMap` lookups are already
/// case-insensitive, so this lowercase constant matches `X-Acpx-Profile`
/// regardless of how the client cases it.
const PROFILE_HEADER: &str = "x-acpx-profile";

/// Header carrying an explicit tenant selection (`acpx-tenant-isolation`
/// Phase B). `axum`'s `HeaderMap` lookups are already case-insensitive.
/// Absent means [`TenantId::default_tenant`].
const TENANT_HEADER: &str = "x-acpx-tenant";

/// Resolve this request's [`TenantId`] from `X-Acpx-Tenant`, defaulting to
/// [`TenantId::default_tenant`] when absent or not valid UTF-8 -- a
/// malformed header is treated the same as an absent one (fails open to
/// the default tenant) rather than rejecting the request outright, since
/// tenant scoping is a data partition, not an auth gate (see this
/// module's doc comment).
fn resolve_tenant(headers: &HeaderMap) -> TenantId {
    headers
        .get(TENANT_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(TenantId::from)
        .unwrap_or_default()
}

/// Start the HTTP/WS transport, serving `POST /rpc` and `GET /ws` against
/// the given shared `Router` until the listener errors or the task is
/// dropped/cancelled. Intended to be spawned as its own task (or awaited
/// directly) from `main.rs` alongside the stdio transport.
///
/// `auth_token`: `None` (pass `None` for the pre-existing, unauthenticated
/// behavior every test in this workspace already relies on) disables
/// auth; `Some(token)` requires `Authorization: Bearer <token>` on every
/// `POST /rpc` and the `GET /ws` upgrade. See this module's doc comment.
/// Kept as a small bind-then-serve convenience wrapper around
/// [`serve_on`] -- not called from this crate's own `main.rs` any more
/// (see `transport/mod.rs`'s doc comment), but every integration test in
/// this crate that exercises the HTTP/WS transport calls it against its
/// own `#[path]`-included physical copy of this same file (`tests/
/// auth_test.rs` et al.), where it very much is used; `#[allow(dead_code)]`
/// only silences the *this-crate's-own-`acpx-server`-binary* lint, which
/// has no visibility into those separately-compiled test copies.
#[allow(dead_code)]
pub async fn serve(
    router: SharedRouter,
    bind_addr: SocketAddr,
    auth_token: Option<String>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    serve_on(listener, router, auth_token).await
}

/// Same as [`serve`], but against an already-bound [`tokio::net::TcpListener`]
/// -- lets `main.rs` attempt the bind itself first (so a bind failure, e.g.
/// the default port already taken by another concurrently-running
/// `acpx-server` instance, can be handled as a non-fatal "run stdio only"
/// fallback rather than propagating out of this function and killing the
/// whole process, stdio transport included -- see `config.rs`'s
/// `ACPX_HTTP_BIND=off` doc comment for the full rationale).
pub async fn serve_on(
    listener: tokio::net::TcpListener,
    router: SharedRouter,
    auth_token: Option<String>,
) -> anyhow::Result<()> {
    serve_on_with_bridge(listener, router, auth_token, None).await
}

/// Same as [`serve_on`], with an explicitly enabled strict-ACP bridge.
/// Kept separate so all pre-existing callers/tests retain their legacy
/// `/rpc` + `/ws` surface without constructing bridge configuration.
pub async fn serve_on_with_bridge(
    listener: tokio::net::TcpListener,
    router: SharedRouter,
    auth_token: Option<String>,
    bridge: Option<acpx_bridge::BridgeConfig>,
) -> anyhow::Result<()> {
    let state = AppState {
        router,
        auth: AuthConfig::new(auth_token),
        bridge: bridge.map(Arc::new),
        bridge_runtime: None,
    };
    let state = AppState {
        bridge_runtime: state
            .bridge
            .as_ref()
            .map(|config| Arc::new(BridgeRuntime::new(Arc::clone(config)))),
        ..state
    };
    let auth_enabled = state.auth.token.is_some();
    let bridge_enabled = state.bridge.is_some();
    let mut app = axum::Router::new()
        .route("/rpc", post(rpc_handler))
        .route("/ws", get(super::ws::ws_handler));
    if bridge_enabled {
        app = app
            .route("/acp/agents", get(acp_agents_handler))
            .route("/acp/models", get(acp_models_handler))
            .route("/acp/rpc", post(acp_rpc_handler))
            .route("/acp/ws", get(super::ws::acp_ws_handler));
    }
    let app = app.with_state(state);

    tracing::info!(
        bind_addr = %listener.local_addr().map(|a| a.to_string()).unwrap_or_default(),
        auth_enabled,
        bridge_enabled,
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
    let tenant_id = resolve_tenant(&headers);
    let response =
        match dispatch_shared_for_tenant(&state.router, &tenant_id, request.clone()).await {
            Ok(response) => response,
            Err(err) => json_rpc_error(&request, err),
        };
    Json(response).into_response()
}

/// Standard-ACP HTTP bridge. Unlike native `/rpc`, this route has no
/// profile header injection and rejects `_acpx` request fields outright.
async fn acp_rpc_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<serde_json::Value>,
) -> Response {
    if !state.auth.authorize(&headers) {
        return unauthorized_response(&request);
    }
    let Some(runtime) = &state.bridge_runtime else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let tenant_id = resolve_tenant(&headers);
    match acp_bridge::dispatch(&state.router, runtime, &tenant_id, request.clone()).await {
        Ok(response) => Json(response).into_response(),
        Err(error) => Json(bridge_json_rpc_error(&request, error)).into_response(),
    }
}

/// Secret-safe view of the bridge-enabled adapters, derived from ACPX's own
/// native `agents/list` implementation rather than duplicating registry or
/// install detection in the HTTP transport.
async fn acp_agents_handler(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !state.auth.authorize(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(bridge) = &state.bridge else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let tenant_id = resolve_tenant(&headers);
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "acp-bridge-agents",
        "method": "agents/list",
        "params": {}
    });
    let response =
        match dispatch_shared_for_tenant(&state.router, &tenant_id, request.clone()).await {
            Ok(response) => response,
            Err(err) => return Json(json_rpc_error(&request, err)).into_response(),
        };
    let allowed = bridge.agent_ids();
    let agents = response
        .get("result")
        .and_then(|result| result.get("agents"))
        .and_then(serde_json::Value::as_array)
        .map(|agents| {
            agents
                .iter()
                .filter(|agent| {
                    agent
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|id| allowed.contains(id))
                })
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Json(serde_json::json!({ "agents": agents })).into_response()
}

/// Public model aliases are bridge policy filtered through ACPX's native
/// adapter detection result. No command, credential, profile, or provider
/// information is emitted here.
async fn acp_models_handler(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !state.auth.authorize(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(runtime) = &state.bridge_runtime else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let tenant_id = resolve_tenant(&headers);
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "acp-bridge-models",
        "method": "agents/list",
        "params": {}
    });
    let response =
        match dispatch_shared_for_tenant(&state.router, &tenant_id, request.clone()).await {
            Ok(response) => response,
            Err(err) => return Json(json_rpc_error(&request, err)).into_response(),
        };
    let agents_result = response.get("result").cloned().unwrap_or_default();
    runtime.refresh_models(&state.router).await;
    Json(serde_json::json!({
        "defaultModel": runtime.config.default_model,
        "models": runtime.public_models(&agents_result).await,
    }))
    .into_response()
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

pub(crate) fn bridge_json_rpc_error(
    request: &serde_json::Value,
    error: acp_bridge::BridgeDispatchError,
) -> serde_json::Value {
    let code = match error {
        acp_bridge::BridgeDispatchError::AcpxExtensionNotAllowed
        | acp_bridge::BridgeDispatchError::InvalidModelSelection
        | acp_bridge::BridgeDispatchError::UnknownModel(_)
        | acp_bridge::BridgeDispatchError::CrossAdapterModelSwitch
        | acp_bridge::BridgeDispatchError::MissingSessionId => -32602,
        _ => -32020,
    };
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": request.get("id").cloned().unwrap_or(serde_json::Value::Null),
        "error": { "code": code, "message": error.to_string() }
    })
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
