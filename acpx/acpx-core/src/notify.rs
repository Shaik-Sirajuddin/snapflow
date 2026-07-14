//! **ACP compatibility phase 14.** Live `session/update` (and, in
//! principle, any other bare/no-`id` backend notification) fan-out from
//! a backend process to whichever client connection currently owns the
//! corresponding *gateway* session -- decoupled from request/response
//! correlation entirely.
//!
//! **The gap this closes.** Before this phase, `router::read_matching_
//! response` only ever surfaced a backend's `session/update` notifications
//! by buffering them into the one in-flight call's own JSON-RPC response,
//! under `_acpx.updates` (see `router::attach_updates`) -- a real ACP
//! client (e.g. Zed) that expects independent, live `session/update`
//! notification frames *as they happen* (the core mechanism behind
//! incremental prompt-turn UX -- streamed message chunks, tool-call
//! progress, plan updates) never actually got that: it got one bundle at
//! the very end of the call instead, which defeats the entire point.
//!
//! **Scope, deliberately narrow.** This only wires up the two *persistent,
//! full-duplex* transports (`acpx-server`'s stdio and WebSocket
//! transports) -- `POST /rpc` is stateless request/response with no live
//! push channel available at all, so it keeps the pre-existing
//! `_acpx.updates` aggregation-in-response behavior completely unchanged,
//! by design, not as an oversight. See `router::LiveNotifyCtx`'s doc
//! comment for how a notification is routed to this hub (or falls back
//! to the old buffering behavior when nothing is subscribed) and
//! `acpx-server/src/transport/live.rs` for how a transport subscribes.
//!
//! **Ownership model, and why "last subscriber wins" is fine.** A gateway
//! session id is only ever handed back to the one client connection whose
//! `session/new`/`session/load`/`session/resume` call minted or supplied
//! it -- acpx has no concept of one session being actively driven by two
//! different client connections at once. [`NotificationHub::subscribe`]
//! replacing any previous subscriber for the same session id is therefore
//! not expected to matter in practice; it's documented here rather than
//! silently assumed, and is a strictly better failure mode than a stale
//! subscriber silently winning over a live one (the *newest* connection to
//! touch a session is the one still around to receive anything further).

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Cheaply cloneable (an `Arc` internally) -- every clone shares the same
/// underlying subscriber map, so `Router::notification_hub()` can hand out
/// a clone to each transport connection without any of them needing to go
/// back through the `Router`'s own lock to publish or subscribe.
#[derive(Clone, Default)]
pub struct NotificationHub {
    subscribers: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<serde_json::Value>>>>,
}

impl NotificationHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe to `gateway_session_id`'s live notification stream.
    /// Returns the receiving half; the caller is expected to spawn a task
    /// that drains it for the lifetime of the connection (or until this
    /// session is unsubscribed/closed) and writes each value out as its
    /// own standalone JSON-RPC frame.
    pub async fn subscribe(
        &self,
        gateway_session_id: impl Into<String>,
    ) -> mpsc::UnboundedReceiver<serde_json::Value> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscribers
            .lock()
            .await
            .insert(gateway_session_id.into(), tx);
        rx
    }

    /// Remove `gateway_session_id`'s live subscriber, if any. Safe to call
    /// even if nothing (or a different, since-replaced sender) is
    /// currently registered -- a no-op in that case, not an error.
    pub async fn unsubscribe(&self, gateway_session_id: &str) {
        self.subscribers.lock().await.remove(gateway_session_id);
    }

    /// Deliver `value` to `gateway_session_id`'s live subscriber, if one
    /// is currently registered and its receiver hasn't been dropped.
    /// Returns `true` on successful live delivery -- callers **must**
    /// treat that as "do not also buffer this for `_acpx.updates`": the
    /// same client would otherwise see it twice, once live and once
    /// bundled into its own call's response.
    pub async fn publish(&self, gateway_session_id: &str, value: serde_json::Value) -> bool {
        let subscribers = self.subscribers.lock().await;
        match subscribers.get(gateway_session_id) {
            Some(tx) => tx.send(value).is_ok(),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publish_with_no_subscriber_is_a_harmless_no_op() {
        let hub = NotificationHub::new();
        let delivered = hub
            .publish("nobody-subscribed", serde_json::json!({"hello": "world"}))
            .await;
        assert!(!delivered);
    }

    #[tokio::test]
    async fn subscribe_then_publish_round_trips() {
        let hub = NotificationHub::new();
        let mut rx = hub.subscribe("session-1").await;
        let delivered = hub
            .publish("session-1", serde_json::json!({"method": "session/update"}))
            .await;
        assert!(delivered);
        let received = rx.recv().await.expect("value delivered");
        assert_eq!(received, serde_json::json!({"method": "session/update"}));
    }

    #[tokio::test]
    async fn unsubscribe_then_publish_falls_back_to_not_delivered() {
        let hub = NotificationHub::new();
        let _rx = hub.subscribe("session-1").await;
        hub.unsubscribe("session-1").await;
        let delivered = hub
            .publish("session-1", serde_json::json!({"hello": "world"}))
            .await;
        assert!(!delivered);
    }

    #[tokio::test]
    async fn a_fresh_subscribe_replaces_the_previous_subscriber() {
        let hub = NotificationHub::new();
        let mut first_rx = hub.subscribe("session-1").await;
        let mut second_rx = hub.subscribe("session-1").await;
        let delivered = hub.publish("session-1", serde_json::json!({"n": 1})).await;
        assert!(delivered);
        assert_eq!(
            second_rx.recv().await.expect("second subscriber gets it"),
            serde_json::json!({"n": 1})
        );
        // The first (replaced) subscriber's channel is closed, not fed --
        // its `recv()` returns `None` once the sender side was dropped by
        // the second `subscribe` overwriting the map entry.
        assert_eq!(first_rx.recv().await, None);
    }
}
