//! Tenant-scoped live session-update streams.
//!
//! Every gateway session owns one broadcast stream. Independent persistent
//! clients can subscribe concurrently without replacing one another; the
//! stream key includes the tenant so a contrived gateway-id collision cannot
//! cross a tenancy boundary.

use crate::TenantId;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{broadcast, Mutex};

const DEFAULT_STREAM_CAPACITY: usize = 256;
const DEFAULT_MAX_SUBSCRIBERS: usize = 16;
const DEFAULT_REPLAY_BUFFER_SIZE: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SubscribeError {
    #[error("session already has the configured maximum of {max_subscribers} live subscribers")]
    TooManySubscribers { max_subscribers: usize },
    #[error("stream resume state is stale; a full session refresh is required")]
    ResyncRequired { epoch: String, seq: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StreamKey {
    tenant_id: TenantId,
    gateway_session_id: String,
}

/// One ordered notification retained for live fan-out and bounded replay.
#[derive(Debug, Clone)]
pub struct Envelope {
    seq: u64,
    epoch: String,
    value: serde_json::Value,
}

impl Envelope {
    /// ACPX's resume metadata is additive and remains scoped to the
    /// existing `params` object of a normal ACP `session/update` frame.
    pub fn into_value(mut self) -> serde_json::Value {
        if let Some(params) = self
            .value
            .get_mut("params")
            .and_then(serde_json::Value::as_object_mut)
        {
            let extension = params
                .entry("_acpx")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if let Some(extension) = extension.as_object_mut() {
                extension.insert("seq".to_string(), serde_json::Value::from(self.seq));
                extension.insert(
                    "epoch".to_string(),
                    serde_json::Value::String(self.epoch.clone()),
                );
            }
        }
        self.value
    }

    #[cfg(test)]
    fn seq(&self) -> u64 {
        self.seq
    }
}

struct SessionStream {
    tx: broadcast::Sender<Envelope>,
    replay: Mutex<VecDeque<Envelope>>,
    next_seq: AtomicU64,
    epoch: Mutex<EpochState>,
}

#[derive(Debug, Clone)]
struct EpochState {
    generation: u64,
    backend_session_id: Option<String>,
    value: String,
}

/// Additive client cursor supplied inside `_acpx.resume`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeCursor {
    pub last_seq: u64,
    pub epoch: String,
}

/// Current backend identity and durable-state observation for the session
/// being subscribed to. Transports obtain this from the router before
/// attaching a resumable receiver.
#[derive(Debug, Clone, Default)]
pub struct StreamResumeState {
    pub backend_session_id: Option<String>,
    pub durable_state_changed: bool,
}

/// A subscriber owns independent replay and live-read state. Replay is
/// drained before live traffic, while `live_floor` removes the overlapping
/// broadcast records captured by subscribe-before-snapshot.
#[derive(Debug)]
pub struct Subscription {
    replay: VecDeque<Envelope>,
    receiver: broadcast::Receiver<Envelope>,
    live_floor: Option<u64>,
}

impl Subscription {
    pub async fn recv(&mut self) -> Result<Envelope, broadcast::error::RecvError> {
        if let Some(envelope) = self.replay.pop_front() {
            return Ok(envelope);
        }
        loop {
            let envelope = self.receiver.recv().await?;
            if self.live_floor.is_none_or(|floor| envelope.seq > floor) {
                return Ok(envelope);
            }
        }
    }
}

fn epoch_value(
    key: &StreamKey,
    process_nonce: &str,
    generation: u64,
    backend_session_id: Option<&str>,
) -> String {
    // FNV-1a is sufficient here because epoch is an opaque correctness
    // token, not an authentication secret. Its output is deterministic
    // across calls and daemon runs, unlike randomized hash builders.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in key
        .tenant_id
        .0
        .bytes()
        .chain(key.gateway_session_id.bytes())
        .chain(process_nonce.bytes())
        .chain(generation.to_le_bytes())
        .chain(backend_session_id.unwrap_or_default().bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[derive(Clone)]
pub struct NotificationHub {
    streams: Arc<Mutex<HashMap<StreamKey, Arc<SessionStream>>>>,
    capacity: usize,
    max_subscribers: usize,
    replay_buffer_size: usize,
    process_nonce: Arc<str>,
}

impl Default for NotificationHub {
    fn default() -> Self {
        Self::new()
    }
}

impl NotificationHub {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_STREAM_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_limits(capacity, DEFAULT_MAX_SUBSCRIBERS)
    }

    /// Configure bounded delivery and admission independently. Both values
    /// must be positive so every created stream has a usable receiver and
    /// a deterministic connection limit.
    pub fn with_limits(capacity: usize, max_subscribers: usize) -> Self {
        Self::with_replay_limits(capacity, max_subscribers, DEFAULT_REPLAY_BUFFER_SIZE)
    }

    /// Configure delivery capacity, subscriber admission, and the bounded
    /// replay history independently.
    pub fn with_replay_limits(
        capacity: usize,
        max_subscribers: usize,
        replay_buffer_size: usize,
    ) -> Self {
        Self {
            streams: Arc::new(Mutex::new(HashMap::new())),
            capacity: capacity.max(1),
            max_subscribers: max_subscribers.max(1),
            replay_buffer_size: replay_buffer_size.max(1),
            process_nonce: uuid::Uuid::new_v4().to_string().into(),
        }
    }

    fn key(tenant_id: &TenantId, gateway_session_id: impl Into<String>) -> StreamKey {
        StreamKey {
            tenant_id: tenant_id.clone(),
            gateway_session_id: gateway_session_id.into(),
        }
    }

    /// Subscribe without affecting existing subscribers. A stream survives
    /// client disconnects so a later subscriber can attach to the same live
    /// session; [`Self::remove_stream`] is reserved for session deletion.
    pub async fn subscribe(
        &self,
        tenant_id: &TenantId,
        gateway_session_id: impl Into<String>,
        last_seq: Option<u64>,
    ) -> Result<Subscription, SubscribeError> {
        self.subscribe_resuming(
            tenant_id,
            gateway_session_id,
            last_seq.map(|last_seq| ResumeCursor {
                last_seq,
                // Legacy sequence-only callers must explicitly refresh once
                // epoch validation is enabled rather than silently resuming.
                epoch: String::new(),
            }),
            StreamResumeState::default(),
        )
        .await
    }

    /// Subscribe with a complete reconnect cursor and durable session state.
    pub async fn subscribe_resuming(
        &self,
        tenant_id: &TenantId,
        gateway_session_id: impl Into<String>,
        cursor: Option<ResumeCursor>,
        resume_state: StreamResumeState,
    ) -> Result<Subscription, SubscribeError> {
        let key = Self::key(tenant_id, gateway_session_id);
        let stream_key = key.clone();
        let process_nonce = Arc::clone(&self.process_nonce);
        let stream = {
            let mut streams = self.streams.lock().await;
            streams
                .entry(key)
                .or_insert_with(|| {
                    Arc::new(SessionStream {
                        tx: broadcast::channel(self.capacity).0,
                        replay: Mutex::new(VecDeque::new()),
                        next_seq: AtomicU64::new(1),
                        epoch: Mutex::new(EpochState {
                            generation: 0,
                            backend_session_id: None,
                            value: epoch_value(&stream_key, &process_nonce, 0, None),
                        }),
                    })
                })
                .clone()
        };
        if stream.tx.receiver_count() >= self.max_subscribers {
            return Err(SubscribeError::TooManySubscribers {
                max_subscribers: self.max_subscribers,
            });
        }

        let (current_epoch, clear_replay) = {
            let mut epoch = stream.epoch.lock().await;
            let backend_changed = epoch.backend_session_id != resume_state.backend_session_id
                && resume_state.backend_session_id.is_some();
            if backend_changed || resume_state.durable_state_changed {
                epoch.generation += 1;
                epoch.backend_session_id = resume_state.backend_session_id.clone();
                epoch.value = epoch_value(
                    &stream_key,
                    &self.process_nonce,
                    epoch.generation,
                    epoch.backend_session_id.as_deref(),
                );
            } else if epoch.backend_session_id.is_none()
                && resume_state.backend_session_id.is_some()
            {
                epoch.backend_session_id = resume_state.backend_session_id.clone();
                epoch.value = epoch_value(
                    &stream_key,
                    &self.process_nonce,
                    epoch.generation,
                    epoch.backend_session_id.as_deref(),
                );
            }
            (
                epoch.value.clone(),
                backend_changed || resume_state.durable_state_changed,
            )
        };
        if clear_replay {
            stream.replay.lock().await.clear();
        }

        if let Some(cursor) = &cursor {
            if cursor.epoch != current_epoch {
                return Err(SubscribeError::ResyncRequired {
                    epoch: current_epoch,
                    seq: stream.next_seq.load(Ordering::Relaxed).saturating_sub(1),
                });
            }
        }

        // Subscribe before taking the replay snapshot. A publication that
        // wins the replay lock is replayed and filtered from live delivery;
        // one that loses it lands only in the receiver. Either ordering is
        // exactly-once and ordered.
        let receiver = stream.tx.subscribe();
        let replay = if let Some(cursor) = cursor {
            stream
                .replay
                .lock()
                .await
                .iter()
                .filter(|envelope| envelope.seq > cursor.last_seq)
                .cloned()
                .collect()
        } else {
            VecDeque::new()
        };
        let live_floor = replay.back().map(|envelope| envelope.seq);
        Ok(Subscription {
            replay,
            receiver,
            live_floor,
        })
    }

    /// Remove a session's stream after the gateway session has closed.
    pub async fn remove_stream(&self, tenant_id: &TenantId, gateway_session_id: &str) {
        self.streams
            .lock()
            .await
            .remove(&Self::key(tenant_id, gateway_session_id));
    }

    /// Publish an update to every live subscriber. `false` means there were
    /// no receivers, so callers may retain their response-bundle fallback.
    pub async fn publish(
        &self,
        tenant_id: &TenantId,
        gateway_session_id: &str,
        value: serde_json::Value,
    ) -> bool {
        let stream = {
            let streams = self.streams.lock().await;
            streams
                .get(&Self::key(tenant_id, gateway_session_id))
                .cloned()
        };
        let Some(stream) = stream else {
            return false;
        };
        let envelope = Envelope {
            seq: stream.next_seq.fetch_add(1, Ordering::Relaxed),
            epoch: stream.epoch.lock().await.value.clone(),
            value,
        };
        {
            let mut replay = stream.replay.lock().await;
            replay.push_back(envelope.clone());
            while replay.len() > self.replay_buffer_size {
                replay.pop_front();
            }
        }
        stream.tx.send(envelope).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn two_subscribers_receive_the_same_update() {
        let hub = NotificationHub::new();
        let tenant = TenantId::from("tenant-a");
        let mut first = hub.subscribe(&tenant, "session-1", None).await.unwrap();
        let mut second = hub.subscribe(&tenant, "session-1", None).await.unwrap();
        assert!(
            hub.publish(&tenant, "session-1", serde_json::json!({"n": 1}))
                .await
        );
        assert_eq!(first.recv().await.unwrap().seq(), 1);
        assert_eq!(second.recv().await.unwrap().seq(), 1);
    }

    #[tokio::test]
    async fn identical_gateway_ids_are_isolated_by_tenant() {
        let hub = NotificationHub::new();
        let tenant_a = TenantId::from("tenant-a");
        let tenant_b = TenantId::from("tenant-b");
        let mut a = hub
            .subscribe(&tenant_a, "forced-collision", None)
            .await
            .unwrap();
        let mut b = hub
            .subscribe(&tenant_b, "forced-collision", None)
            .await
            .unwrap();
        assert!(
            hub.publish(
                &tenant_a,
                "forced-collision",
                serde_json::json!({"tenant": "a"})
            )
            .await
        );
        assert_eq!(a.recv().await.unwrap().seq(), 1);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), b.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn removed_stream_does_not_deliver_future_updates() {
        let hub = NotificationHub::new();
        let tenant = TenantId::from("tenant-a");
        let mut receiver = hub.subscribe(&tenant, "session-1", None).await.unwrap();
        hub.remove_stream(&tenant, "session-1").await;
        assert!(receiver.recv().await.is_err());
        assert!(
            !hub.publish(&tenant, "session-1", serde_json::json!({"n": 1}))
                .await
        );
    }

    #[tokio::test]
    async fn subscriber_limit_rejects_only_the_new_subscriber() {
        let hub = NotificationHub::with_limits(8, 2);
        let tenant = TenantId::from("tenant-a");
        let mut first = hub.subscribe(&tenant, "session-1", None).await.unwrap();
        let mut second = hub.subscribe(&tenant, "session-1", None).await.unwrap();

        assert!(matches!(
            hub.subscribe(&tenant, "session-1", None).await,
            Err(SubscribeError::TooManySubscribers { max_subscribers: 2 })
        ));
        assert!(
            hub.publish(&tenant, "session-1", serde_json::json!({"n": 1}))
                .await
        );
        assert_eq!(first.recv().await.unwrap().seq(), 1);
        assert_eq!(second.recv().await.unwrap().seq(), 1);
    }

    #[tokio::test]
    async fn resume_replays_only_records_after_the_client_cursor() {
        let hub = NotificationHub::with_replay_limits(8, 2, 3);
        let tenant = TenantId::from("tenant-a");
        let mut live = hub.subscribe(&tenant, "session-1", None).await.unwrap();
        assert!(
            hub.publish(&tenant, "session-1", serde_json::json!({"n": 1}))
                .await
        );
        let first = live.recv().await.unwrap();
        let cursor = ResumeCursor {
            last_seq: first.seq,
            epoch: first.epoch,
        };
        for n in 2..=3 {
            assert!(
                hub.publish(&tenant, "session-1", serde_json::json!({"n": n}))
                    .await
            );
        }
        drop(live);

        let mut resumed = hub
            .subscribe_resuming(
                &tenant,
                "session-1",
                Some(cursor),
                StreamResumeState::default(),
            )
            .await
            .unwrap();
        assert_eq!(resumed.recv().await.unwrap().seq(), 2);
        assert_eq!(resumed.recv().await.unwrap().seq(), 3);
        assert!(
            hub.publish(&tenant, "session-1", serde_json::json!({"n": 4}))
                .await
        );
        assert_eq!(resumed.recv().await.unwrap().seq(), 4);
    }

    #[tokio::test]
    async fn durable_drift_invalidates_the_epoch_and_requires_a_resync() {
        let hub = NotificationHub::new();
        let tenant = TenantId::from("tenant-a");
        let mut live = hub.subscribe(&tenant, "session-1", None).await.unwrap();
        assert!(
            hub.publish(&tenant, "session-1", serde_json::json!({"n": 1}))
                .await
        );
        let delivered = live.recv().await.unwrap();
        let old_epoch = delivered.epoch.clone();
        let cursor = ResumeCursor {
            last_seq: delivered.seq,
            epoch: old_epoch.clone(),
        };
        drop(live);

        let error = hub
            .subscribe_resuming(
                &tenant,
                "session-1",
                Some(cursor),
                StreamResumeState {
                    backend_session_id: None,
                    durable_state_changed: true,
                },
            )
            .await
            .expect_err("out-of-band durable change must not replay");
        match error {
            SubscribeError::ResyncRequired { epoch, seq } => {
                assert_ne!(epoch, old_epoch);
                assert_eq!(seq, 1);
            }
            other => panic!("expected resync error, got {other:?}"),
        }
    }
}
