//! Live relay of agent-initiated requests (`session/request_permission`,
//! and -- as more methods opt in -- `fs/*`/`terminal/*`) to whichever
//! transport connection currently owns the corresponding *gateway*
//! session, mirroring `crate::notify::NotificationHub`'s subscribe model
//! but bidirectional: a request goes *out* to the owning client, and
//! that client's eventual `acpx/agent_response` reply must come back in
//! and resolve the exact same relay attempt that's still waiting on it.
//!
//! **The gap this closes.** Before this existed, `router::read_matching_
//! response` answered every agent-initiated request itself, synchronously,
//! from a profile's static `permission_policy`/`allow_fs_access`/
//! `allow_terminal_access` settings -- a real human-in-the-loop decision
//! (e.g. "allow this tool call once") was structurally impossible: there
//! was no live client on the other end of that decision at all, only a
//! server-side default. See `router::LiveNotifyCtx`'s doc comment for how
//! a relay attempt is wired into that same read loop, and
//! `acpx-server/src/transport/ws.rs` for how a WS connection subscribes
//! and answers.
//!
//! **Fallback contract.** [`AgentRequestHub::relay`] returns `None` --
//! not an error -- both when nothing is subscribed for the target session
//! (e.g. an HTTP-only client, or a WS client that never claimed this
//! session) and when a subscriber exists but never answers before
//! `timeout` elapses. Either case is the caller's cue to fall back to the
//! pre-existing policy-based auto-answer unchanged, so a backend is never
//! left hanging just because no interactive client happened to be
//! attached.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex};

/// One relayed agent-initiated request pushed out to a subscribed
/// transport connection. `relay_id` is this hub's own correlation
/// identifier -- distinct from `request`'s own JSON-RPC `id`, which is
/// the *backend's* request id and must be echoed back verbatim in the
/// eventual reply written to the backend, not used to correlate the
/// relay itself (a client may hold several relays from different
/// backends/sessions whose own ids can collide).
#[derive(Debug, Clone)]
pub struct AgentRequestEnvelope {
    pub relay_id: String,
    pub gateway_session_id: String,
    /// The original backend-native JSON-RPC request frame, verbatim.
    pub request: serde_json::Value,
}

/// Cheaply cloneable (an `Arc` internally), same convention as
/// `NotificationHub` -- every clone shares the same subscriber/pending
/// maps so `Router::agent_request_hub()` can hand a clone to each
/// transport connection without going back through the `Router`'s own
/// lock for either side of a relay.
#[derive(Clone, Default)]
pub struct AgentRequestHub {
    subscribers: Arc<Mutex<HashMap<String, mpsc::Sender<AgentRequestEnvelope>>>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<serde_json::Value>>>>,
}

static RELAY_COUNTER: AtomicU64 = AtomicU64::new(1);

fn fresh_relay_id() -> String {
    // Monotonic counter plus wall-clock nanos: unique within one process's
    // lifetime (the counter) and across restarts for log correlation (the
    // timestamp) without pulling in a `uuid` dependency for what is purely
    // an in-process correlation token that never leaves this gateway.
    let n = RELAY_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("relay-{nanos:x}-{n}")
}

/// Capacity of each session's live agent-request delivery channel.
///
/// **Why bounded, not unbounded.** [`AgentRequestHub::relay`] already
/// bounds how long *it* waits for an answer (`timeout`, e.g. `router`'s
/// `PERMISSION_RELAY_TIMEOUT`), but the channel a subscribed connection's
/// forwarder task drains (`acpx-server/src/transport/ws.rs`'s per-session
/// relay loop) had no ceiling of its own: a backend emitting
/// agent-initiated requests faster than that connection's own send loop
/// can forward them (now itself bounded -- see `acpx-server::transport::
/// ws::WS_FRAME_WRITE_TIMEOUT`, but still finite per frame) had no bound
/// on how many envelopes could queue up in memory in the meantime. This
/// lock is dropped before ever sending (see [`AgentRequestHub::relay`]'s
/// body), so a bounded `send(..).await` here only ever delays this one
/// relay attempt -- it cannot deadlock any other caller of this hub.
const AGENT_REQUEST_QUEUE_CAPACITY: usize = 256;

impl AgentRequestHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe to `gateway_session_id`'s live agent-request stream.
    /// Same "last subscriber wins" contract as `NotificationHub::
    /// subscribe` -- a gateway session is only ever owned by one
    /// connection at a time in practice.
    pub async fn subscribe(
        &self,
        gateway_session_id: impl Into<String>,
    ) -> mpsc::Receiver<AgentRequestEnvelope> {
        let (tx, rx) = mpsc::channel(AGENT_REQUEST_QUEUE_CAPACITY);
        self.subscribers
            .lock()
            .await
            .insert(gateway_session_id.into(), tx);
        rx
    }

    /// Remove `gateway_session_id`'s live subscriber, if any. Safe to
    /// call even if nothing (or a since-replaced sender) is registered.
    pub async fn unsubscribe(&self, gateway_session_id: &str) {
        self.subscribers.lock().await.remove(gateway_session_id);
    }

    /// Attempt to relay `request` to whichever connection currently owns
    /// `gateway_session_id`, and wait up to `timeout` for its answer.
    /// `None` means "fall back to the policy default" -- see this
    /// module's doc comment for the full fallback contract.
    pub async fn relay(
        &self,
        gateway_session_id: &str,
        request: serde_json::Value,
        timeout: Duration,
    ) -> Option<serde_json::Value> {
        let sender = {
            let subscribers = self.subscribers.lock().await;
            subscribers.get(gateway_session_id)?.clone()
        };
        let relay_id = fresh_relay_id();
        let (reply_tx, reply_rx) = oneshot::channel();
        self.pending.lock().await.insert(relay_id.clone(), reply_tx);
        let envelope = AgentRequestEnvelope {
            relay_id: relay_id.clone(),
            gateway_session_id: gateway_session_id.to_string(),
            request,
        };
        // Bounded channel now (see `AGENT_REQUEST_QUEUE_CAPACITY`'s doc
        // comment) -- `send` can briefly wait on backpressure instead of
        // succeeding instantly, so it shares this call's own `timeout`
        // budget rather than being allowed to block beyond it.
        if tokio::time::timeout(timeout, sender.send(envelope))
            .await
            .map(|result| result.is_err())
            .unwrap_or(true)
        {
            // Either the subscriber's receiver was already dropped
            // (connection closing) but hadn't yet called `unsubscribe`,
            // or its queue stayed full for this call's entire `timeout`
            // (a connection too far behind to be a usable relay target
            // right now either way) -- both treated identically to "no
            // subscriber" rather than hanging further.
            self.pending.lock().await.remove(&relay_id);
            return None;
        }
        let outcome = tokio::time::timeout(timeout, reply_rx).await;
        self.pending.lock().await.remove(&relay_id);
        match outcome {
            Ok(Ok(value)) => Some(value),
            Ok(Err(_)) | Err(_) => None,
        }
    }

    /// Complete a pending relay with the client's `acpx/agent_response`
    /// payload. `true` means a still-waiting relay was resolved by this
    /// call; `false` covers both an unknown `relay_id` and one whose
    /// `relay` call already gave up waiting (a late reply after timeout)
    /// -- either way there's nothing left to deliver the value to, which
    /// is not an error, just a race the caller may want to log.
    pub async fn resolve(&self, relay_id: &str, response: serde_json::Value) -> bool {
        match self.pending.lock().await.remove(relay_id) {
            Some(sender) => sender.send(response).is_ok(),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn relay_with_no_subscriber_returns_none_immediately() {
        let hub = AgentRequestHub::new();
        let result = hub
            .relay(
                "gw-1",
                serde_json::json!({"method": "session/request_permission"}),
                Duration::from_millis(50),
            )
            .await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn subscribe_relay_resolve_round_trips() {
        let hub = AgentRequestHub::new();
        let mut rx = hub.subscribe("gw-1").await;
        let hub_clone = hub.clone();
        let relay_task = tokio::spawn(async move {
            hub_clone
                .relay(
                    "gw-1",
                    serde_json::json!({"id": 7, "method": "session/request_permission"}),
                    Duration::from_secs(5),
                )
                .await
        });
        let envelope = rx.recv().await.expect("envelope delivered");
        assert_eq!(
            envelope.request,
            serde_json::json!({"id": 7, "method": "session/request_permission"})
        );
        let delivered = hub
            .resolve(
                &envelope.relay_id,
                serde_json::json!({"jsonrpc": "2.0", "id": 7, "result": {"outcome": {"outcome": "selected", "optionId": "allow"}}}),
            )
            .await;
        assert!(delivered);
        let result = relay_task.await.unwrap();
        assert_eq!(
            result,
            Some(serde_json::json!({"jsonrpc": "2.0", "id": 7, "result": {"outcome": {"outcome": "selected", "optionId": "allow"}}}))
        );
    }

    #[tokio::test]
    async fn relay_times_out_when_subscriber_never_answers() {
        let hub = AgentRequestHub::new();
        let mut rx = hub.subscribe("gw-1").await;
        let hub_clone = hub.clone();
        let relay_task = tokio::spawn(async move {
            hub_clone
                .relay(
                    "gw-1",
                    serde_json::json!({"method": "session/request_permission"}),
                    Duration::from_millis(30),
                )
                .await
        });
        let _envelope = rx.recv().await.expect("envelope delivered");
        // Never call resolve -- the relay must give up on its own.
        let result = relay_task.await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn unsubscribe_then_relay_falls_back_to_none() {
        let hub = AgentRequestHub::new();
        let _rx = hub.subscribe("gw-1").await;
        hub.unsubscribe("gw-1").await;
        let result = hub
            .relay(
                "gw-1",
                serde_json::json!({"method": "session/request_permission"}),
                Duration::from_millis(30),
            )
            .await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn a_late_resolve_after_timeout_is_a_harmless_false() {
        let hub = AgentRequestHub::new();
        let mut rx = hub.subscribe("gw-1").await;
        let hub_clone = hub.clone();
        let relay_task = tokio::spawn(async move {
            hub_clone
                .relay(
                    "gw-1",
                    serde_json::json!({"method": "session/request_permission"}),
                    Duration::from_millis(20),
                )
                .await
        });
        let envelope = rx.recv().await.expect("envelope delivered");
        let result = relay_task.await.unwrap();
        assert!(result.is_none());
        let delivered = hub
            .resolve(&envelope.relay_id, serde_json::json!({"late": true}))
            .await;
        assert!(!delivered);
    }
}
