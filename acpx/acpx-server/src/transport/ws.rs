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
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

use acpx_core::router::dispatch_shared;

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
    ws.on_upgrade(move |socket| handle_socket(socket, state.router))
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
async fn handle_socket(socket: WebSocket, router: SharedRouter) {
    let (sink, mut stream) = socket.split();
    let sink = Arc::new(AsyncMutex::new(sink));
    let hub = { router.lock().await.notification_hub() };
    let mut watched: HashSet<String> = HashSet::new();

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

        let response = {
            match dispatch_shared(&router, request.clone()).await {
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
}
