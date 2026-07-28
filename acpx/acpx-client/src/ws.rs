//! Persistent WebSocket transport for the acpx gateway.
//!
//! The HTTP transport remains available for constrained deployments, but it
//! cannot receive live notifications. This module owns all raw WebSocket
//! framing so consumers use [`crate::Gateway`] rather than handling frames.

use crate::raw::ClientError;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Weak};
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

struct SessionNotificationChannel {
    sender: broadcast::Sender<GatewayNotification>,
    subscribers: usize,
}

/// A session-scoped notification receiver. Dropping the last receiver for a
/// session unregisters that session's channel from the WebSocket client so a
/// long-lived gateway does not retain every historical session id forever.
pub struct SessionSubscription {
    receiver: broadcast::Receiver<GatewayNotification>,
    session_id: String,
    owner: Weak<GatewayWsClient>,
}

impl SessionSubscription {
    pub async fn recv(&mut self) -> Result<GatewayNotification, broadcast::error::RecvError> {
        self.receiver.recv().await
    }

    pub fn try_recv(&mut self) -> Result<GatewayNotification, broadcast::error::TryRecvError> {
        self.receiver.try_recv()
    }
}

impl Drop for SessionSubscription {
    fn drop(&mut self) {
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        owner.release_session_subscription(&self.session_id);
    }
}

/// A multiplexed WebSocket connection to one gateway.
pub struct GatewayWsClient {
    sink: Mutex<WsSink>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<serde_json::Value, ClientError>>>>,
    notifications: broadcast::Sender<GatewayNotification>,
    session_notifications: std::sync::Mutex<HashMap<String, SessionNotificationChannel>>,
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
            session_notifications: std::sync::Mutex::new(HashMap::new()),
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

    /// Returns a receiver for notifications belonging only to `session_id`.
    /// The raw subscription remains available for low-level gateway/admin
    /// consumers, but session actors must use this demultiplexed channel.
    pub fn subscribe_session(self: &Arc<Self>, session_id: &str) -> SessionSubscription {
        let receiver = self
            .session_notifications
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(session_id.to_owned())
            .and_modify(|channel| channel.subscribers += 1)
            .or_insert_with(|| SessionNotificationChannel {
                sender: broadcast::channel(256).0,
                subscribers: 1,
            })
            .sender
            .subscribe();
        SessionSubscription {
            receiver,
            session_id: session_id.to_owned(),
            owner: Arc::downgrade(self),
        }
    }

    fn release_session_subscription(&self, session_id: &str) {
        let mut channels = self
            .session_notifications
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let remove = channels
            .get_mut(session_id)
            .map(|channel| {
                channel.subscribers = channel.subscribers.saturating_sub(1);
                channel.subscribers == 0
            })
            .unwrap_or(false);
        if remove {
            channels.remove(session_id);
        }
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
        if self
            .is_disconnected
            .load(std::sync::atomic::Ordering::SeqCst)
        {
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
        let _ = self.notifications.send(value.clone());
        let Some(session_id) = value
            .get("params")
            .and_then(|params| params.get("sessionId"))
            .and_then(serde_json::Value::as_str)
        else {
            return;
        };
        let session_sender = self
            .session_notifications
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .map(|channel| channel.sender.clone());
        if let Some(sender) = session_sender {
            let _ = sender.send(value);
        }
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
    use super::{websocket_url, GatewayWsClient};
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::Message;

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

    #[tokio::test]
    async fn session_subscriptions_demultiplex_interleaved_notifications() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test websocket");
        let address = listener.local_addr().expect("test websocket address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept test websocket");
            let mut socket = accept_async(stream).await.expect("upgrade test websocket");
            // Do not emit notifications until the client has completed a
            // request round trip. The test installs both session receivers
            // before this barrier, making the demultiplexing assertion
            // deterministic rather than dependent on scheduler timing.
            let request = socket
                .next()
                .await
                .expect("client readiness request")
                .expect("read client readiness request");
            let request: serde_json::Value = match request {
                Message::Text(text) => serde_json::from_str(&text).expect("valid readiness JSON"),
                other => panic!("unexpected readiness frame: {other:?}"),
            };
            socket
                .send(Message::Text(
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": {"sessions": []}
                    })
                    .to_string(),
                ))
                .await
                .expect("send readiness response");
            for (session_id, text) in [("session-2", "two"), ("session-3", "three")] {
                socket
                    .send(Message::Text(
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": {
                                "sessionId": session_id,
                                "update": {"text": text}
                            }
                        })
                        .to_string(),
                    ))
                    .await
                    .expect("send test notification");
            }
        });

        let client = GatewayWsClient::connect(&format!("http://{address}"))
            .await
            .unwrap();
        let mut session_two = client.subscribe_session("session-2");
        let mut session_three = client.subscribe_session("session-3");
        client
            .call("session/list", serde_json::json!({}))
            .await
            .expect("complete subscription barrier");

        let two = tokio::time::timeout(std::time::Duration::from_secs(1), session_two.recv())
            .await
            .expect("session-2 notification timeout")
            .expect("session-2 notification channel closed");
        let three = tokio::time::timeout(std::time::Duration::from_secs(1), session_three.recv())
            .await
            .expect("session-3 notification timeout")
            .expect("session-3 notification channel closed");
        assert_eq!(two["params"]["sessionId"], "session-2");
        assert_eq!(two["params"]["update"]["text"], "two");
        assert_eq!(three["params"]["sessionId"], "session-3");
        assert_eq!(three["params"]["update"]["text"], "three");

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), session_two.recv())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), session_three.recv())
                .await
                .is_err()
        );
        server.await.expect("test websocket server");
    }
}
