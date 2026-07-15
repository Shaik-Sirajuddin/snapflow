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
//!
//! **Per-request concurrency (interactive relay addition).** Each inbound
//! frame's dispatch now runs in its own spawned task rather than being
//! awaited inline in this connection's read loop. This is not merely an
//! optimization: `AgentRequestHub`-relayed agent-initiated requests (see
//! `acpx_core::agent_relay`'s module doc comment) need this exact
//! connection to answer with a new inbound `acpx/agent_response` frame
//! *while* the triggering `session/prompt` dispatch is still in flight on
//! the very same connection -- if that dispatch were awaited inline here,
//! this loop could never read the answering frame at all, deadlocking
//! every relayed request against its own connection permanently. `session/
//! cancel`'s existing independent-worker client-side design (a second WS
//! connection just to avoid this same serialization) was a workaround for
//! this exact limitation; this fixes the limitation itself instead of
//! requiring every future interactive feature to route around it.
//! Response *frames* may now be written out of request order relative to
//! each other (fine: JSON-RPC responses are id-correlated, not
//! position-correlated), but writes to the shared sink are still
//! serialized one at a time via its `Arc<Mutex<..>>`, so no two frames
//! ever interleave mid-write.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

use acpx_core::agent_relay::AgentRequestHub;
use acpx_core::notify::NotificationHub;
use acpx_core::router::dispatch_shared_for_tenant;
use acpx_core::TenantId;

use super::http::{json_rpc_error, AppState, SharedRouter};
use super::live::{session_id_to_forget, session_id_to_watch};

type WsSink = futures_util::stream::SplitSink<WebSocket, Message>;

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

/// Write `value` out as one standalone frame. Serialization failure is
/// logged and swallowed (nothing sensible to retry); a send failure means
/// the connection is gone, also just logged here since each caller (the
/// per-request task, or a live forwarder task) has nothing further of its
/// own to do about a dead connection either way -- the main read loop
/// notices independently, via its own `stream.next()` ending.
async fn write_frame(sink: &Arc<AsyncMutex<WsSink>>, value: &serde_json::Value) {
    let payload = match serde_json::to_string(value) {
        Ok(payload) => payload,
        Err(err) => {
            tracing::error!(?err, "failed to serialize JSON-RPC frame");
            return;
        }
    };
    if let Err(err) = sink.lock().await.send(Message::Text(payload)).await {
        tracing::debug!(?err, "ws send failed (connection likely closed)");
    }
}

/// Subscribe this connection to `session_id`'s live `NotificationHub` and
/// `AgentRequestHub` streams, if it isn't already -- a no-op (not a
/// double-subscribe) when `session_id` is already in `watched`, matching
/// both hubs' own "last subscriber wins" contract, which this avoids
/// relying on unless truly needed. Spawns one small forwarder task per
/// hub, each writing its own frame shape out via [`write_frame`] for as
/// long as this connection (or its subscription) lasts.
async fn subscribe_if_new(
    watched: &Arc<AsyncMutex<HashSet<String>>>,
    hub: &NotificationHub,
    agent_relay: &AgentRequestHub,
    sink: &Arc<AsyncMutex<WsSink>>,
    session_id: String,
) {
    let newly_watched = watched.lock().await.insert(session_id.clone());
    if !newly_watched {
        return;
    }

    let mut updates_rx = hub.subscribe(session_id.clone()).await;
    let updates_sink = Arc::clone(sink);
    tokio::spawn(async move {
        while let Some(update) = updates_rx.recv().await {
            write_frame(&updates_sink, &update).await;
        }
    });

    let mut relay_rx = agent_relay.subscribe(session_id.clone()).await;
    let relay_sink = Arc::clone(sink);
    tokio::spawn(async move {
        while let Some(envelope) = relay_rx.recv().await {
            let frame = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "acpx/agent_request",
                "params": {
                    "relayId": envelope.relay_id,
                    "sessionId": envelope.gateway_session_id,
                    "request": envelope.request,
                }
            });
            write_frame(&relay_sink, &frame).await;
        }
    });
}

/// One WS connection's request/response loop: each inbound text/binary
/// frame is parsed as a single JSON-RPC request, dispatched against the
/// shared `Router` in its own spawned task (see this module's "per-request
/// concurrency" doc comment above for why), and the JSON-RPC response
/// written back as one outbound frame once that dispatch completes,
/// possibly out of order relative to other in-flight requests on this
/// same connection. Malformed frames are logged and dropped rather than
/// closing the connection, so one bad frame doesn't take down an
/// otherwise-healthy client session. Also subscribes/unsubscribes this
/// connection to/from `NotificationHub`/`AgentRequestHub` per
/// `transport::live::{session_id_to_watch, session_id_to_forget}` and
/// [`subscribe_if_new`].
async fn handle_socket(socket: WebSocket, router: SharedRouter, tenant_id: TenantId) {
    let (sink, mut stream) = socket.split();
    let sink = Arc::new(AsyncMutex::new(sink));
    let hub = { router.lock().await.notification_hub() };
    let agent_relay = { router.lock().await.agent_request_hub() };
    let watched: Arc<AsyncMutex<HashSet<String>>> = Arc::new(AsyncMutex::new(HashSet::new()));

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
        let method = request
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_string();

        // **Interactive relay addition.** The client's answer to a
        // relayed agent-initiated request arrives as its own inbound
        // frame, correlated by `relayId` rather than this connection's
        // usual JSON-RPC id space -- handled here, before the session
        // watch/dispatch logic below, since it never targets `Router::
        // dispatch` at all, and stays inline (not spawned) since it's
        // always fast and non-blocking (no backend round trip) -- it must
        // never queue up behind a slow in-flight dispatch, since a slow
        // in-flight dispatch may be the very thing waiting on it. Always
        // acknowledged with `{"delivered": ..}` so a panel can distinguish
        // "the backend got your answer" from "this relay already expired"
        // (a late click after the 15-minute `PERMISSION_RELAY_TIMEOUT`,
        // or a stale/unknown `relayId`).
        if method == "acpx/agent_response" {
            let relay_id = request
                .get("params")
                .and_then(|p| p.get("relayId"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let response_value = request
                .get("params")
                .and_then(|p| p.get("response"))
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let delivered = agent_relay.resolve(&relay_id, response_value).await;
            let ack = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request.get("id").cloned().unwrap_or(serde_json::Value::Null),
                "result": {"delivered": delivered}
            });
            write_frame(&sink, &ack).await;
            continue;
        }

        // Reattachment can emit `session/update` before the RPC response
        // (a real adapter's own `loadSession`/history replay can start
        // streaming immediately). Claim the session before dispatch so
        // those notifications reach this reattaching connection instead
        // of whatever stale connection (if any) subscribed to it before
        // this one reconnected. Deliberately scoped to `session/load`/
        // `session/resume` only -- NOT every `Proxied` method with a
        // `sessionId` (a `session/prompt` against a session this
        // connection never touched before is not reattachment; claiming
        // it pre-dispatch would incorrectly steal live delivery away
        // from whichever *other* connection is that session's actual
        // current owner mid-turn -- caught by `multitenant_concurrency_
        // e2e_test.rs`'s `the_newest_same_tenant_connection_to_touch_a_
        // session_becomes_its_live_subscriber`, which pins down exactly
        // that multi-connection scenario). Every other Proxied method,
        // `session/prompt` included, still only ever subscribes in the
        // post-dispatch step below, per `session_id_to_watch`. Stays
        // inline (fast, no backend I/O) so subscription is always
        // registered strictly before the spawned dispatch task below ever
        // sends anything to the backend.
        if matches!(method.as_str(), "session/load" | "session/resume") {
            if let Some(session_id) = request
                .get("params")
                .and_then(|params| params.get("sessionId"))
                .and_then(|session_id| session_id.as_str())
                .map(str::to_owned)
            {
                subscribe_if_new(&watched, &hub, &agent_relay, &sink, session_id).await;
            }
        }

        // **Interactive relay addition.** Spawned, not awaited -- see
        // this module's "per-request concurrency" doc comment for why a
        // slow dispatch (in particular, one blocked on a relayed
        // agent-initiated request) must not stall this connection's own
        // read loop, which is exactly what would otherwise prevent the
        // answering `acpx/agent_response` frame above from ever being
        // read at all.
        let router = Arc::clone(&router);
        let tenant_id = tenant_id.clone();
        let hub = hub.clone();
        let agent_relay = agent_relay.clone();
        let sink = Arc::clone(&sink);
        let watched = Arc::clone(&watched);
        tokio::spawn(async move {
            let response = match dispatch_shared_for_tenant(&router, &tenant_id, request.clone())
                .await
            {
                Ok(response) => response,
                Err(err) => json_rpc_error(&request, err),
            };

            if let Some(forget) = session_id_to_forget(&request, &response, &method) {
                let removed = watched.lock().await.remove(&forget);
                if removed {
                    hub.unsubscribe(&forget).await;
                    agent_relay.unsubscribe(&forget).await;
                }
            } else if let Some(watch) = session_id_to_watch(&request, &response, &method) {
                subscribe_if_new(&watched, &hub, &agent_relay, &sink, watch).await;
            }

            write_frame(&sink, &response).await;
        });
    }

    for session_id in watched.lock().await.iter() {
        hub.unsubscribe(&session_id).await;
        agent_relay.unsubscribe(&session_id).await;
    }
}
