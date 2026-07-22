//! Persistent WebSocket transport for the acpx gateway.
//!
//! The HTTP transport remains available for constrained deployments, but it
//! cannot receive live notifications. This module owns all raw WebSocket
//! framing so consumers use [`crate::Gateway`] rather than handling frames.

use crate::raw::ClientError;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

/// Hard ceiling on writing one WebSocket frame to the gateway connection.
/// A live TCP connection should accept a small JSON-RPC frame in
/// microseconds; anything stuck this long means the socket is wedged
/// (peer stopped reading, network partition, etc.), not merely slow, so
/// this is treated the same as a send failure rather than left to hang
/// the caller -- and every future caller behind it, since `sink` is a
/// single shared `Mutex` -- forever. Mirrors `acpx-core::router`'s
/// `BACKEND_WRITE_TIMEOUT` for the same reasoning on the gateway's own
/// backend-stdin side.
const WS_SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Hard ceiling on waiting for a gateway response once a request has been
/// sent. Deliberately generous -- long enough to comfortably exceed any
/// legitimate long-running turn or permission-approval wait the gateway
/// itself will still be servicing (`acpx_core::router`'s own
/// `BACKEND_IDLE_READ_TIMEOUT` backstop is 20 minutes) -- this exists
/// only to catch a connection that looks alive at the TCP level but will
/// never actually answer (e.g. the gateway process wedged without ever
/// closing the socket), so a caller doesn't hang forever and this
/// request's `pending` table entry doesn't leak for the rest of the
/// connection's lifetime.
const WS_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

type WsSink = futures_util::stream::SplitSink<
    WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

/// A server notification received on the persistent gateway connection.
///
/// ACPX intentionally leaves the JSON-RPC notification shape intact here.
/// Typed ACPX event mapping belongs in the SDK facade and panel reducer, not
/// in a WebSocket framing layer.
pub type GatewayNotification = serde_json::Value;

/// A multiplexed WebSocket connection to one gateway.
pub struct GatewayWsClient {
    sink: Mutex<WsSink>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<serde_json::Value, ClientError>>>>,
    notifications: broadcast::Sender<GatewayNotification>,
    // Fired once the reader task's frame loop exits (peer closed the
    // socket, a read errored, ...) -- lets a long-lived subscriber (a
    // live-notification forwarding task with no in-flight `call()` to
    // otherwise notice the drop) detect the connection died even with no
    // traffic at all, instead of silently sitting on a `broadcast::
    // Receiver` that will simply never receive anything again. See
    // `wait_for_disconnect`.
    disconnected: tokio::sync::Notify,
    is_disconnected: std::sync::atomic::AtomicBool,
}

impl GatewayWsClient {
    /// Opens `GET /ws` for a gateway HTTP origin or explicit `ws://` URL.
    pub async fn connect(base_url: &str) -> Result<Arc<Self>, ClientError> {
        let url = websocket_url(base_url);
        let (stream, _) = connect_async(&url)
            .await
            .map_err(|error| ClientError::WebSocket(error.to_string()))?;
        let (sink, mut source) = stream.split();
        let (notifications, _) = broadcast::channel(256);
        let client = Arc::new(Self {
            sink: Mutex::new(sink),
            next_id: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            notifications,
            disconnected: tokio::sync::Notify::new(),
            is_disconnected: std::sync::atomic::AtomicBool::new(false),
        });
        let reader = Arc::clone(&client);
        tokio::spawn(async move {
            while let Some(frame) = source.next().await {
                match frame {
                    Ok(Message::Text(text)) => reader.deliver_frame(&text).await,
                    Ok(Message::Binary(bytes)) => {
                        if let Ok(text) = String::from_utf8(bytes) {
                            reader.deliver_frame(&text).await;
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => {}
                }
            }
            reader
                .fail_pending("gateway WebSocket connection closed")
                .await;
            reader
                .is_disconnected
                .store(true, std::sync::atomic::Ordering::SeqCst);
            reader.disconnected.notify_waiters();
        });
        Ok(client)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<GatewayNotification> {
        self.notifications.subscribe()
    }

    /// Resolves once this connection's reader loop has exited (peer
    /// closed it, a read errored, ...) -- or immediately, if that already
    /// happened before this call. A long-lived subscriber can race this
    /// against `Receiver::recv()` (via `tokio::select!`) to notice the
    /// connection died even during a quiet period with no notifications
    /// in flight, rather than only finding out the next time it happens
    /// to call something.
    pub async fn wait_for_disconnect(&self) {
        // Register as a waiter *before* checking the flag: `Notify`
        // only wakes tasks already waiting when `notify_waiters()` is
        // called, so checking the flag first and creating the
        // `Notified` future second would miss a disconnect that lands
        // in between (classic check-then-wait race). Creating the
        // future first means a `notify_waiters()` from this point
        // onward is guaranteed to be observed by this specific await.
        let notified = self.disconnected.notified();
        if self.is_disconnected.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        notified.await;
    }

    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ClientError> {
        let response = self.request(method, params).await?;
        response
            .get("result")
            .cloned()
            .ok_or(ClientError::MalformedResponse)
    }

    pub async fn call_with_updates(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(serde_json::Value, Vec<serde_json::Value>), ClientError> {
        let response = self.request(method, params).await?;
        let result = response
            .get("result")
            .cloned()
            .ok_or(ClientError::MalformedResponse)?;
        let updates = response
            .get("_acpx")
            .and_then(|extension| extension.get("updates"))
            .and_then(|updates| updates.as_array())
            .cloned()
            .unwrap_or_default();
        Ok((result, updates))
    }

    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (response_tx, response_rx) = oneshot::channel();
        self.pending.lock().await.insert(id, response_tx);
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let encoded = serde_json::to_string(&payload)
            .map_err(|error| ClientError::WebSocket(error.to_string()))?;
        let send = async { self.sink.lock().await.send(Message::Text(encoded)).await };
        match tokio::time::timeout(WS_SEND_TIMEOUT, send).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                self.pending.lock().await.remove(&id);
                return Err(ClientError::WebSocket(error.to_string()));
            }
            Err(_) => {
                // See `WS_SEND_TIMEOUT`'s doc comment: dropping `send`
                // here releases the `sink` lock it may still hold, so a
                // wedged socket write only fails this one request
                // instead of blocking every other caller sharing this
                // connection's single sink `Mutex` forever.
                self.pending.lock().await.remove(&id);
                return Err(ClientError::WebSocket(format!(
                    "gateway WebSocket send timed out after {WS_SEND_TIMEOUT:?}"
                )));
            }
        }
        match tokio::time::timeout(WS_RESPONSE_TIMEOUT, response_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(ClientError::WebSocket(
                "gateway response channel closed".to_string(),
            )),
            Err(_) => {
                // The connection never closed (or `fail_pending` would
                // have already resolved this), yet nothing answered
                // within `WS_RESPONSE_TIMEOUT` -- remove this call's own
                // entry so it doesn't leak in `pending` for the rest of
                // the connection's lifetime.
                self.pending.lock().await.remove(&id);
                Err(ClientError::WebSocket(format!(
                    "gateway response to {method:?} timed out after {WS_RESPONSE_TIMEOUT:?}"
                )))
            }
        }
    }

    async fn deliver_frame(&self, text: &str) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            return;
        };
        if let Some(id) = value.get("id").and_then(|id| id.as_i64()) {
            if let Some(sender) = self.pending.lock().await.remove(&id) {
                let result = if let Some(error) = value.get("error") {
                    Err(ClientError::Rpc {
                        code: error
                            .get("code")
                            .and_then(|code| code.as_i64())
                            .unwrap_or(0),
                        message: error
                            .get("message")
                            .and_then(|message| message.as_str())
                            .unwrap_or_default()
                            .to_owned(),
                    })
                } else {
                    Ok(value)
                };
                let _ = sender.send(result);
            }
            return;
        }
        let _ = self.notifications.send(value);
    }

    async fn fail_pending(&self, message: &str) {
        let mut pending = self.pending.lock().await;
        for (_, sender) in pending.drain() {
            let _ = sender.send(Err(ClientError::WebSocket(message.to_owned())));
        }
    }
}

fn websocket_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}/ws")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}/ws")
    } else if base.starts_with("ws://") || base.starts_with("wss://") {
        if base.ends_with("/ws") {
            base.to_owned()
        } else {
            format!("{base}/ws")
        }
    } else {
        format!("ws://{base}/ws")
    }
}

#[cfg(test)]
mod tests {
    use super::websocket_url;

    #[test]
    fn derives_ws_endpoint_from_http_origins() {
        assert_eq!(
            websocket_url("http://127.0.0.1:8790"),
            "ws://127.0.0.1:8790/ws"
        );
        assert_eq!(
            websocket_url("https://example.test/"),
            "wss://example.test/ws"
        );
        assert_eq!(
            websocket_url("ws://example.test/ws"),
            "ws://example.test/ws"
        );
    }
}
