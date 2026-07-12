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

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;

use super::http::{json_rpc_error, SharedRouter};

/// Axum handler for `GET /ws`: upgrades the connection, then hands off to
/// `handle_socket` for the request/response loop.
pub async fn ws_handler(
    State(router): State<SharedRouter>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, router))
}

/// One WS connection's request/response loop: each inbound text/binary
/// frame is parsed as a single JSON-RPC request, dispatched against the
/// shared `Router`, and the JSON-RPC response written back as one outbound
/// frame. Malformed frames are logged and dropped rather than closing the
/// connection, so one bad frame doesn't take down an otherwise-healthy
/// client session.
async fn handle_socket(mut socket: WebSocket, router: SharedRouter) {
    while let Some(msg) = socket.recv().await {
        let msg = match msg {
            Ok(msg) => msg,
            Err(err) => {
                tracing::warn!(?err, "ws recv error, closing connection");
                return;
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
            Message::Close(_) => return,
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
            let mut router = router.lock().await;
            match router.dispatch(request.clone()).await {
                Ok(response) => response,
                Err(err) => json_rpc_error(&request, err),
            }
        };

        let payload = match serde_json::to_string(&response) {
            Ok(payload) => payload,
            Err(err) => {
                tracing::error!(?err, "failed to serialize JSON-RPC response");
                continue;
            }
        };
        if socket.send(Message::Text(payload)).await.is_err() {
            return;
        }
    }
}
