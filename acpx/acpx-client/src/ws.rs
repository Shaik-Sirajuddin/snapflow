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
use tokio::sync::{broadcast, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

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
        });
        Ok(client)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<GatewayNotification> {
        self.notifications.subscribe()
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
        if let Err(error) = self.sink.lock().await.send(Message::Text(encoded)).await {
            self.pending.lock().await.remove(&id);
            return Err(ClientError::WebSocket(error.to_string()));
        }
        response_rx
            .await
            .map_err(|_| ClientError::WebSocket("gateway response channel closed".to_string()))?
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
