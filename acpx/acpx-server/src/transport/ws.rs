//! WebSocket transport for the acpx gateway (Phase 2 step 11). Wired into
//! the same axum router `http.rs` builds (`GET /ws`), sharing its
//! `SharedRouter` state -- see `http.rs`'s module doc comment for the
//! auth/TLS caveat, which applies equally here.
//!
//! No `X-Acpx-Profile` header equivalent on this transport: WS headers are
//! only present at the initial upgrade request, not per-message, and the
//! architecture doc scopes that header to HTTP/WS *request* framing, not a
//! whole connection's worth of subsequent JSON-RPC frames. A WS client
//! that wants managed mode uses the existing `params._acpx.profile` field
//! on its `session/new` frame instead -- `Router::dispatch` already
//! handles that path with zero extra code needed here.
//!
//! **Auth**: same `AuthConfig`/`ACPX_AUTH_TOKEN` gate as `http.rs`'s
//! `POST /rpc` (see that module's doc comment for the full contract).
//! Checked once, here, against the upgrade request's own headers --
//! that's the only point in a WS connection's lifetime where headers are
//! even available, so a rejected upgrade (missing/wrong token) is the
//! only enforcement point; there is no per-message re-check after that.
//!
//! **Tenant isolation (`acpx-tenant-isolation` Phase B).** `X-Acpx-Tenant`
//! is read once at upgrade time (the only point in a WS connection's
//! lifetime headers are available, same caveat as auth above) and cached
//! for that connection's entire lifetime -- a WS client is one fixed
//! tenant for its whole connection, never switchable mid-stream. Absent
//! means [`acpx_core::TenantId::default_tenant`].
//!
//! **Live `session/update` streaming (ACP compatibility phase 14).** This
//! is one of the two persistent, full-duplex transports (the other is
//! `stdio.rs`) that subscribes to `acpx_core::notify::NotificationHub`
//! for every gateway session this connection touches -- see
//! `transport::live`'s module doc comment for the subscribe/unsubscribe
//! decision logic shared with `stdio.rs`, and `acpx_core::notify`'s
//! module doc comment for why this exists at all (real ACP clients need
//! independent, live `session/update` notification frames, not just the
//! pre-existing `_acpx.updates` bundle at the end of a call). The
//! `WebSocket` is split into independent sink/stream halves so a live
//! forwarder task can write frames concurrently with this connection's
//! own request/response loop, both funneling through the same `Arc<Mutex<
//! ..>>`-wrapped sink so writes from either side never interleave
//! mid-frame.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex as AsyncMutex};

use acpx_core::router::dispatch_shared_for_tenant;
use acpx_core::{InteractionBinding, TenantId};

use super::http::{json_rpc_error, AppState, SharedRouter};
use super::live::{session_id_to_forget, session_id_to_watch};

/// Axum handler for `GET /ws`: upgrades the connection, then hands off to
/// `handle_socket` for the request/response loop.
pub async fn ws_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !state.auth.authorize(&headers) {
        // Reject the upgrade outright -- there is no later point in a WS
        // connection's lifetime where an `Authorization` header is
        // available again, so this is the only place auth can be
        // enforced for this transport. `401` here means the handshake
        // itself never completes (the client sees a plain HTTP 401
        // response to its upgrade request, not a WS close frame).
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let tenant_id = headers
        .get("x-acpx-tenant")
        .and_then(|v| v.to_str().ok())
        .map(TenantId::from)
        .unwrap_or_default();
    ws.on_upgrade(move |socket| handle_socket(socket, state.router, tenant_id))
}

/// Strict ACP bridge counterpart to [`ws_handler`]. It shares the same
/// auth and tenant boundary, but routes every frame through the bridge
/// virtual-session dispatcher so clients never need ACPX profile fields.
pub async fn acp_ws_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !state.auth.authorize(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(runtime) = state.bridge_runtime.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let tenant_id = headers
        .get("x-acpx-tenant")
        .and_then(|v| v.to_str().ok())
        .map(TenantId::from)
        .unwrap_or_default();
    ws.on_upgrade(move |socket| handle_acp_socket(socket, state.router, runtime, tenant_id))
}

async fn handle_acp_socket(
    socket: WebSocket,
    router: SharedRouter,
    runtime: Arc<super::http::acp_bridge::BridgeRuntime>,
    tenant_id: TenantId,
) {
    let (sink, mut stream) = socket.split();
    let sink = Arc::new(AsyncMutex::new(sink));
    let hub = { router.lock().await.notification_hub() };
    let mut watched: HashSet<String> = HashSet::new();
    while let Some(message) = stream.next().await {
        let text = match message {
            Ok(Message::Text(text)) => text,
            Ok(Message::Binary(bytes)) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => continue,
            },
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue,
        };
        let request: serde_json::Value = match serde_json::from_str(&text) {
            Ok(request) => request,
            Err(_) => continue,
        };
        let mut response =
            match super::http::acp_bridge::dispatch(&router, &runtime, &tenant_id, request.clone())
                .await
            {
                Ok(response) => response,
                Err(error) => super::http::bridge_json_rpc_error(&request, error),
            };
        // The first lazy-bound prompt cannot be subscribed before it binds,
        // so Router buffers any early backend updates in its native
        // `_acpx.updates` extension. Flush those as normal ACP frames before
        // the final response; bridge clients must never need to understand
        // ACPX-only response extensions.
        if let Some(updates) = response
            .get_mut("_acpx")
            .and_then(|value| value.get_mut("updates"))
            .and_then(|value| value.as_array_mut())
        {
            let mut flushed_updates = false;
            for mut update in std::mem::take(updates) {
                let Some(native_session_id) = update
                    .pointer("/params/sessionId")
                    .and_then(|value| value.as_str())
                else {
                    continue;
                };
                if runtime
                    .bound_gateway_session_id(&tenant_id, native_session_id)
                    .is_none()
                {
                    let Some(virtual_id) =
                        runtime.virtual_session_id(&tenant_id, native_session_id)
                    else {
                        continue;
                    };
                    update["params"]["sessionId"] = serde_json::Value::String(virtual_id);
                }
                let Ok(frame) = serde_json::to_string(&update) else {
                    continue;
                };
                if sink.lock().await.send(Message::Text(frame)).await.is_err() {
                    return;
                }
                flushed_updates = true;
            }
            if flushed_updates {
                // Some ACP clients dispatch notifications on separate tasks.
                // Give them one scheduling slice before the prompt response
                // completes the turn and they snapshot accumulated text.
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        if response
            .get("_acpx")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|extension| {
                extension
                    .get("updates")
                    .is_some_and(|value| value.as_array().is_some_and(Vec::is_empty))
            })
        {
            response
                .get_mut("_acpx")
                .and_then(serde_json::Value::as_object_mut)
                .expect("checked extension object")
                .remove("updates");
            if response
                .get("_acpx")
                .and_then(serde_json::Value::as_object)
                .is_some_and(serde_json::Map::is_empty)
            {
                response
                    .as_object_mut()
                    .expect("JSON-RPC object")
                    .remove("_acpx");
            }
        }
        let method = request
            .get("method")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        if let Some(public_id) = bridge_session_id_to_forget(&request, &response, method) {
            if let Some(native_id) = runtime.bound_gateway_session_id(&tenant_id, &public_id) {
                if watched.remove(&native_id) {
                    hub.unsubscribe(&native_id).await;
                }
            }
        } else if let Some(public_id) = bridge_session_id_to_watch(&request, &response, method) {
            if let Some(native_id) = runtime.bound_gateway_session_id(&tenant_id, &public_id) {
                if watched.insert(native_id.clone()) {
                    let mut rx = hub.subscribe(native_id).await;
                    let forwarder_sink = Arc::clone(&sink);
                    let forwarder_runtime = Arc::clone(&runtime);
                    let forwarder_tenant = tenant_id.clone();
                    tokio::spawn(async move {
                        while let Some(mut update) = rx.recv().await {
                            let Some(native_session_id) = update
                                .pointer("/params/sessionId")
                                .and_then(|value| value.as_str())
                            else {
                                continue;
                            };
                            let Some(virtual_id) = forwarder_runtime
                                .virtual_session_id(&forwarder_tenant, native_session_id)
                            else {
                                continue;
                            };
                            update["params"]["sessionId"] = serde_json::Value::String(virtual_id);
                            let Ok(frame) = serde_json::to_string(&update) else {
                                continue;
                            };
                            if forwarder_sink
                                .lock()
                                .await
                                .send(Message::Text(frame))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    });
                }
            }
        }
        let Ok(frame) = serde_json::to_string(&response) else {
            continue;
        };
        // A backend update published during this dispatch is already queued
        // for the per-session forwarder. Yield before writing the terminal
        // response so an ACP client observes streamed updates first.
        tokio::task::yield_now().await;
        if sink.lock().await.send(Message::Text(frame)).await.is_err() {
            break;
        }
    }
    for native_id in watched {
        hub.unsubscribe(&native_id).await;
    }
}

fn bridge_session_id_to_watch(
    request: &serde_json::Value,
    response: &serde_json::Value,
    method: &str,
) -> Option<String> {
    if response.get("error").is_some() {
        return None;
    }
    if method == "session/new" || method == "session/fork" {
        return response
            .pointer("/result/sessionId")
            .and_then(|value| value.as_str())
            .map(str::to_string);
    }
    request
        .pointer("/params/sessionId")
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

fn bridge_session_id_to_forget(
    request: &serde_json::Value,
    response: &serde_json::Value,
    method: &str,
) -> Option<String> {
    if response.get("error").is_some() || !matches!(method, "session/close" | "session/delete") {
        return None;
    }
    request
        .pointer("/params/sessionId")
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

/// One WS connection's request/response loop: each inbound text/binary
/// frame is parsed as a single JSON-RPC request, dispatched against the
/// shared `Router`, and the JSON-RPC response written back as one outbound
/// frame. Malformed frames are logged and dropped rather than closing the
/// connection, so one bad frame doesn't take down an otherwise-healthy
/// client session. Also subscribes/unsubscribes this connection to/from
/// `NotificationHub` per `transport::live::{session_id_to_watch,
/// session_id_to_forget}`, spawning one small forwarder task per newly
/// watched session that writes every live update out as its own
/// standalone frame for as long as this connection (or the session) lasts.
async fn handle_socket(socket: WebSocket, router: SharedRouter, tenant_id: TenantId) {
    let (sink, mut stream) = socket.split();
    let sink = Arc::new(AsyncMutex::new(sink));
    let hub = { router.lock().await.notification_hub() };
    let interaction_hub = { router.lock().await.interaction_hub() };
    let (interaction_tx, mut interaction_rx) = mpsc::unbounded_channel();
    let interaction_sink = Arc::clone(&sink);
    tokio::spawn(async move {
        while let Some(request) = interaction_rx.recv().await {
            let Ok(payload) = serde_json::to_string(&request) else {
                continue;
            };
            if interaction_sink
                .lock()
                .await
                .send(Message::Text(payload))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let mut watched: HashSet<String> = HashSet::new();
    let interaction_bindings =
        Arc::new(AsyncMutex::new(HashMap::<String, InteractionBinding>::new()));

    macro_rules! send_frame {
        ($value:expr) => {{
            let payload = match serde_json::to_string(&$value) {
                Ok(payload) => payload,
                Err(err) => {
                    tracing::error!(?err, "failed to serialize JSON-RPC frame");
                    continue;
                }
            };
            if sink
                .lock()
                .await
                .send(Message::Text(payload))
                .await
                .is_err()
            {
                break;
            }
        }};
    }

    while let Some(msg) = stream.next().await {
        let msg = match msg {
            Ok(msg) => msg,
            Err(err) => {
                tracing::warn!(?err, "ws recv error, closing connection");
                break;
            }
        };
        let text = match msg {
            Message::Text(text) => text,
            Message::Binary(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(err) => {
                    tracing::warn!(?err, "ws binary frame is not valid UTF-8 JSON, dropping");
                    continue;
                }
            },
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => continue,
        };

        let request: serde_json::Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(?err, "ws frame is not valid JSON, dropping");
                continue;
            }
        };

        // A response with no method can only be the client's answer to an
        // agent-initiated request sent by InteractionHub. It must not enter
        // Router dispatch: it is correlated directly back to the backend
        // request that is still awaiting it.
        if request.get("method").is_none() && request.get("id").is_some() {
            interaction_hub.resolve(request).await;
            continue;
        }

        // Prompt-like calls can block on an agent-initiated request. Bind
        // this connection before dispatch, then run the backend round trip
        // independently so this read loop remains available for the
        // correlated response above.
        if let Some(session_id) = request
            .pointer("/params/sessionId")
            .and_then(|value| value.as_str())
            .map(str::to_string)
        {
            let binding = interaction_hub
                .bind(
                    tenant_id.clone(),
                    session_id.clone(),
                    interaction_tx.clone(),
                )
                .await;
            let previous = interaction_bindings
                .lock()
                .await
                .insert(session_id, binding);
            if let Some(previous) = previous {
                interaction_hub.unbind(&previous).await;
            }

            let router = Arc::clone(&router);
            let tenant_id = tenant_id.clone();
            let sink = Arc::clone(&sink);
            tokio::spawn(async move {
                let response =
                    match dispatch_shared_for_tenant(&router, &tenant_id, request.clone()).await {
                        Ok(response) => response,
                        Err(error) => json_rpc_error(&request, error),
                    };
                let Ok(payload) = serde_json::to_string(&response) else {
                    return;
                };
                let _ = sink.lock().await.send(Message::Text(payload)).await;
            });
            continue;
        }

        let response = {
            match dispatch_shared_for_tenant(&router, &tenant_id, request.clone()).await {
                Ok(response) => response,
                Err(err) => json_rpc_error(&request, err),
            }
        };

        let method = request
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or_default();
        if let Some(forget) = session_id_to_forget(&request, &response, method) {
            if watched.remove(&forget) {
                hub.unsubscribe(&forget).await;
            }
        } else if let Some(watch) = session_id_to_watch(&request, &response, method) {
            if watched.insert(watch.clone()) {
                let mut rx = hub.subscribe(watch).await;
                let forwarder_sink = Arc::clone(&sink);
                tokio::spawn(async move {
                    while let Some(update) = rx.recv().await {
                        let Ok(payload) = serde_json::to_string(&update) else {
                            continue;
                        };
                        if forwarder_sink
                            .lock()
                            .await
                            .send(Message::Text(payload))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
            }
        }

        send_frame!(response);
    }

    for session_id in watched {
        hub.unsubscribe(&session_id).await;
    }
    for (_, binding) in interaction_bindings.lock().await.drain() {
        interaction_hub.unbind(&binding).await;
    }
}
