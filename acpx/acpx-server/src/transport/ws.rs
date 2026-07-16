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

use acpx_core::router::{dispatch_shared_for_tenant, stream_resume_state_shared};
use acpx_core::{InteractionBinding, StreamResumeState, TenantId};

use super::http::{
    json_rpc_error, json_rpc_subscribe_error, resolve_authorized_tenant, AppState, SharedRouter,
    TenantAuthError,
};
use super::live::{session_id_to_forget, session_id_to_watch, take_resume_cursor};

/// Axum handler for `GET /ws`: upgrades the connection, then hands off to
/// `handle_socket` for the request/response loop.
pub async fn ws_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // Reject the upgrade outright on auth failure or a tenant-identity
    // mismatch -- there is no later point in a WS connection's lifetime
    // where headers are available again, so this is the only place
    // either can be enforced for this transport (see this module's doc
    // comment). The client sees a plain HTTP 401/403 response to its
    // upgrade request, never a WS close frame.
    let tenant_id = match resolve_authorized_tenant(&state.auth, &headers) {
        Ok(tenant) => tenant,
        Err(TenantAuthError::Unauthorized) => return StatusCode::UNAUTHORIZED.into_response(),
        Err(TenantAuthError::Mismatch | TenantAuthError::NotAllowed) => return StatusCode::FORBIDDEN.into_response(),
    };
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
    let Some(runtime) = state.bridge_runtime.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let tenant_id = match resolve_authorized_tenant(&state.auth, &headers) {
        Ok(tenant) => tenant,
        Err(TenantAuthError::Unauthorized) => return StatusCode::UNAUTHORIZED.into_response(),
        Err(TenantAuthError::Mismatch | TenantAuthError::NotAllowed) => return StatusCode::FORBIDDEN.into_response(),
    };
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
    // Shared (not a plain loop-local `HashSet`) because dispatch for each
    // inbound frame is now spawned onto its own task below -- see that
    // spawn's own doc comment for why this had to stop being inline.
    let watched: Arc<AsyncMutex<HashSet<String>>> = Arc::new(AsyncMutex::new(HashSet::new()));
    // Live-interaction wiring (see `acp_bridge::BridgeInteractionCtx`'s doc
    // comment): without this, a backend-initiated `session/request_permission`
    // mid-turn always falls through to the static policy auto-answer -- a
    // connected `/acp` client (Zed, a real ACP-conformant harness, ...) can
    // never be asked for confirmation or cancel a pending tool call, even
    // though the exact same interactive round trip already works for the
    // native (non-bridge) WS/stdio transports via this same `InteractionHub`.
    let interaction_hub = { router.lock().await.interaction_hub() };
    let (interaction_tx, mut interaction_rx) = mpsc::unbounded_channel();
    let interaction_bindings: Arc<AsyncMutex<HashMap<String, InteractionBinding>>> =
        Arc::new(AsyncMutex::new(HashMap::new()));
    let interaction_ctx = super::http::acp_bridge::BridgeInteractionCtx {
        hub: interaction_hub.clone(),
        sender: interaction_tx,
        bindings: Arc::clone(&interaction_bindings),
    };
    {
        let interaction_sink = Arc::clone(&sink);
        let forwarder_runtime = Arc::clone(&runtime);
        let forwarder_tenant = tenant_id.clone();
        tokio::spawn(async move {
            while let Some(mut request) = interaction_rx.recv().await {
                // The hub only ever knows the native/gateway session id
                // (see `try_forward_interaction` in `router.rs`); a bridge
                // client only ever understands its own virtual/public
                // session id, exactly the same translation the
                // `session/update` forwarder below already does.
                if let Some(native_session_id) = request
                    .pointer("/params/sessionId")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
                {
                    if let Some(virtual_id) = forwarder_runtime
                        .virtual_session_id(&forwarder_tenant, &native_session_id)
                    {
                        request["params"]["sessionId"] = serde_json::Value::String(virtual_id);
                    }
                }
                let Ok(frame) = serde_json::to_string(&request) else {
                    continue;
                };
                if interaction_sink
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
        // The client's answer to a backend-initiated interactive request
        // (see above): correlated directly back to the pending request by
        // `id`, must not enter bridge dispatch as if it were a new call.
        if request.get("method").is_none() && request.get("id").is_some() {
            interaction_hub.resolve(request).await;
            continue;
        }
        // Spawned, not awaited inline: a bridge-lazy-bound `session/prompt`
        // can block on a backend-initiated `session/request_permission`
        // mid-turn (see `BridgeInteractionCtx`), which only ever gets
        // answered by *this exact connection* sending back a reply frame --
        // the "response with no method" branch just above. Awaiting
        // dispatch inline here would starve that branch of ever running
        // for the whole rest of this turn, deadlocking every interactive
        // request until `DEFAULT_INTERACTION_TIMEOUT`: one read loop can't
        // both block on a call's result and stay free to read that same
        // call's own answer. Mirrors `transport::ws::handle_socket`'s
        // identical `tokio::spawn` around its own dispatch call.
        let router = Arc::clone(&router);
        let runtime = Arc::clone(&runtime);
        let tenant_id = tenant_id.clone();
        let sink = Arc::clone(&sink);
        let hub = hub.clone();
        let watched = Arc::clone(&watched);
        let interaction_ctx = interaction_ctx.clone();
        tokio::spawn(async move {
            let mut request = request;
            let _resume_cursor = take_resume_cursor(&mut request);
            let mut response =
                match super::http::acp_bridge::dispatch_with_interaction(
                    &router,
                    &runtime,
                    &tenant_id,
                    request.clone(),
                    Some(&interaction_ctx),
                )
                .await
                {
                    Ok(response) => response,
                    Err(error) => super::http::bridge_json_rpc_error(&request, error),
                };
            // The first lazy-bound prompt cannot be subscribed before it
            // binds, so Router buffers any early backend updates in its
            // native `_acpx.updates` extension. Flush those as normal ACP
            // frames before the final response; bridge clients must never
            // need to understand ACPX-only response extensions.
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
                    // Some ACP clients dispatch notifications on separate
                    // tasks. Give them one scheduling slice before the
                    // prompt response completes the turn and they snapshot
                    // accumulated text.
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
                    if watched.lock().await.remove(&native_id) {
                        hub.remove_stream(&tenant_id, &native_id).await;
                    }
                }
            } else if let Some(public_id) = bridge_session_id_to_watch(&request, &response, method)
            {
                if let Some(native_id) = runtime.bound_gateway_session_id(&tenant_id, &public_id) {
                    if watched.lock().await.insert(native_id.clone()) {
                        match hub
                            .subscribe_resuming(
                                &tenant_id,
                                native_id.clone(),
                                None,
                                StreamResumeState::default(),
                            )
                            .await
                        {
                            Ok(mut rx) => {
                                let forwarder_sink = Arc::clone(&sink);
                                let forwarder_runtime = Arc::clone(&runtime);
                                let forwarder_tenant = tenant_id.clone();
                                tokio::spawn(async move {
                                    loop {
                                        let mut update = match rx.recv().await {
                                            Ok(update) => update.into_value(),
                                            Err(
                                                tokio::sync::broadcast::error::RecvError::Lagged(
                                                    skipped,
                                                ),
                                            ) => {
                                                tracing::warn!(%skipped, "ACPX bridge notification subscriber lagged");
                                                continue;
                                            }
                                            Err(
                                                tokio::sync::broadcast::error::RecvError::Closed,
                                            ) => break,
                                        };
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
                                        update["params"]["sessionId"] =
                                            serde_json::Value::String(virtual_id);
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
                            Err(error) => {
                                watched.lock().await.remove(&native_id);
                                response = super::http::json_rpc_subscribe_error(&request, error);
                            }
                        }
                    }
                }
            }
            let Ok(frame) = serde_json::to_string(&response) else {
                return;
            };
            // A backend update published during this dispatch is already
            // queued for the per-session forwarder. Yield before writing
            // the terminal response so an ACP client observes streamed
            // updates first.
            tokio::task::yield_now().await;
            let _ = sink.lock().await.send(Message::Text(frame)).await;
        });
    }
    drop(watched);
    // Disconnects must release every interaction binding this connection
    // holds, or a future prompt on the same native session would forward
    // its interactive requests to a channel nobody is reading from
    // anymore, hanging until `DEFAULT_INTERACTION_TIMEOUT` instead of
    // failing over to the policy fallback right away.
    for binding in interaction_bindings.lock().await.values() {
        interaction_hub.unbind(binding).await;
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
    let deferred_watches = Arc::new(AsyncMutex::new(HashSet::<String>::new()));

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

        let mut request: serde_json::Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(?err, "ws frame is not valid JSON, dropping");
                continue;
            }
        };
        let resume_cursor = take_resume_cursor(&mut request);

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
            // A reconnect cursor changes the ordering requirement: install
            // the receiver before the potentially slow backend call. This
            // preserves live records that arrive while `session/resume` or
            // `session/load` is in flight even if they later roll out of
            // the bounded replay ring.
            let resumed_before_dispatch = if resume_cursor.is_some()
                && deferred_watches.lock().await.insert(session_id.clone())
            {
                let state = stream_resume_state_shared(&router, &tenant_id, &session_id).await;
                match hub
                    .subscribe_resuming(
                        &tenant_id,
                        session_id.clone(),
                        resume_cursor.clone(),
                        StreamResumeState {
                            backend_session_id: state.backend_session_id,
                            durable_state_changed: state.durable_state_changed,
                        },
                    )
                    .await
                {
                    Ok(mut rx) => {
                        let forwarder_sink = Arc::clone(&sink);
                        tokio::spawn(async move {
                            loop {
                                let update = match rx.recv().await {
                                    Ok(update) => update.into_value(),
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(
                                        skipped,
                                    )) => {
                                        tracing::warn!(%skipped, "ACPX notification subscriber lagged");
                                        continue;
                                    }
                                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                };
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
                        true
                    }
                    Err(error) => {
                        deferred_watches.lock().await.remove(&session_id);
                        send_frame!(json_rpc_subscribe_error(&request, error));
                        continue;
                    }
                }
            } else {
                false
            };
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
                .insert(session_id.clone(), binding);
            if let Some(previous) = previous {
                interaction_hub.unbind(&previous).await;
            }
            let subscribe_after_response = !resumed_before_dispatch
                && deferred_watches.lock().await.insert(session_id.clone());

            let router = Arc::clone(&router);
            let tenant_id = tenant_id.clone();
            let sink = Arc::clone(&sink);
            let hub = hub.clone();
            let deferred_watches = Arc::clone(&deferred_watches);
            tokio::spawn(async move {
                let mut response =
                    match dispatch_shared_for_tenant(&router, &tenant_id, request.clone()).await {
                        Ok(response) => response,
                        Err(error) => json_rpc_error(&request, error),
                    };
                if subscribe_after_response && response.get("error").is_none() {
                    let state = stream_resume_state_shared(&router, &tenant_id, &session_id).await;
                    match hub
                        .subscribe_resuming(
                            &tenant_id,
                            session_id.clone(),
                            resume_cursor.clone(),
                            StreamResumeState {
                                backend_session_id: state.backend_session_id,
                                durable_state_changed: state.durable_state_changed,
                            },
                        )
                        .await
                    {
                        Ok(mut rx) => {
                            let forwarder_sink = Arc::clone(&sink);
                            tokio::spawn(async move {
                                loop {
                                    let update = match rx.recv().await {
                                        Ok(update) => update.into_value(),
                                        Err(tokio::sync::broadcast::error::RecvError::Lagged(
                                            skipped,
                                        )) => {
                                            tracing::warn!(%skipped, "ACPX notification subscriber lagged");
                                            continue;
                                        }
                                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                            break
                                        }
                                    };
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
                        Err(error) => {
                            deferred_watches.lock().await.remove(&session_id);
                            response = json_rpc_subscribe_error(&request, error);
                        }
                    }
                } else if subscribe_after_response {
                    deferred_watches.lock().await.remove(&session_id);
                }
                let Ok(payload) = serde_json::to_string(&response) else {
                    return;
                };
                let _ = sink.lock().await.send(Message::Text(payload)).await;
            });
            continue;
        }

        let mut response = {
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
                hub.remove_stream(&tenant_id, &forget).await;
            }
            deferred_watches.lock().await.remove(&forget);
        } else if let Some(watch) = session_id_to_watch(&request, &response, method) {
            if watched.insert(watch.clone()) {
                let state = stream_resume_state_shared(&router, &tenant_id, &watch).await;
                match hub
                    .subscribe_resuming(
                        &tenant_id,
                        watch.clone(),
                        resume_cursor.clone(),
                        StreamResumeState {
                            backend_session_id: state.backend_session_id,
                            durable_state_changed: state.durable_state_changed,
                        },
                    )
                    .await
                {
                    Ok(mut rx) => {
                        deferred_watches.lock().await.insert(watch.clone());
                        let forwarder_sink = Arc::clone(&sink);
                        tokio::spawn(async move {
                            loop {
                                let update = match rx.recv().await {
                                    Ok(update) => update.into_value(),
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(
                                        skipped,
                                    )) => {
                                        tracing::warn!(%skipped, "ACPX notification subscriber lagged");
                                        continue;
                                    }
                                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                };
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
                    Err(error) => {
                        watched.remove(&watch);
                        response = json_rpc_subscribe_error(&request, error);
                    }
                }
            }
        }

        send_frame!(response);
    }

    drop(watched);
    for (_, binding) in interaction_bindings.lock().await.drain() {
        interaction_hub.unbind(&binding).await;
    }
}
