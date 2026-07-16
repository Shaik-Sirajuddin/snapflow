//! Tenant-scoped live session-update streams.
//!
//! Every gateway session owns one broadcast stream. Independent persistent
//! clients can subscribe concurrently without replacing one another; the
//! stream key includes the tenant so a contrived gateway-id collision cannot
//! cross a tenancy boundary.

use crate::TenantId;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

const DEFAULT_STREAM_CAPACITY: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StreamKey {
    tenant_id: TenantId,
    gateway_session_id: String,
}

#[derive(Clone)]
pub struct NotificationHub {
    streams: Arc<Mutex<HashMap<StreamKey, broadcast::Sender<serde_json::Value>>>>,
    capacity: usize,
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
        Self {
            streams: Arc::new(Mutex::new(HashMap::new())),
            capacity: capacity.max(1),
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
    ) -> broadcast::Receiver<serde_json::Value> {
        let key = Self::key(tenant_id, gateway_session_id);
        let mut streams = self.streams.lock().await;
        streams
            .entry(key)
            .or_insert_with(|| broadcast::channel(self.capacity).0)
            .subscribe()
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
        let sender = {
            let streams = self.streams.lock().await;
            streams
                .get(&Self::key(tenant_id, gateway_session_id))
                .cloned()
        };
        let Some(sender) = sender else {
            return false;
        };
        if sender.receiver_count() == 0 {
            return false;
        }
        sender.send(value).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn two_subscribers_receive_the_same_update() {
        let hub = NotificationHub::new();
        let tenant = TenantId::from("tenant-a");
        let mut first = hub.subscribe(&tenant, "session-1").await;
        let mut second = hub.subscribe(&tenant, "session-1").await;
        assert!(
            hub.publish(&tenant, "session-1", serde_json::json!({"n": 1}))
                .await
        );
        assert_eq!(first.recv().await.unwrap(), serde_json::json!({"n": 1}));
        assert_eq!(second.recv().await.unwrap(), serde_json::json!({"n": 1}));
    }

    #[tokio::test]
    async fn identical_gateway_ids_are_isolated_by_tenant() {
        let hub = NotificationHub::new();
        let tenant_a = TenantId::from("tenant-a");
        let tenant_b = TenantId::from("tenant-b");
        let mut a = hub.subscribe(&tenant_a, "forced-collision").await;
        let mut b = hub.subscribe(&tenant_b, "forced-collision").await;
        assert!(
            hub.publish(
                &tenant_a,
                "forced-collision",
                serde_json::json!({"tenant": "a"})
            )
            .await
        );
        assert_eq!(a.recv().await.unwrap(), serde_json::json!({"tenant": "a"}));
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
        let mut receiver = hub.subscribe(&tenant, "session-1").await;
        hub.remove_stream(&tenant, "session-1").await;
        assert!(receiver.recv().await.is_err());
        assert!(
            !hub.publish(&tenant, "session-1", serde_json::json!({"n": 1}))
                .await
        );
    }
}
