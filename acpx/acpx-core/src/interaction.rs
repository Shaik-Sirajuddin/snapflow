//! Correlated forwarding for backend-initiated ACP requests.
//!
//! A backend can pause a prompt while it asks the ACP client for permission,
//! filesystem access, or terminal access. [`InteractionHub`] forwards that
//! request over the persistent client transport and maps its response back to
//! the backend's original JSON-RPC id. It deliberately does not own session
//! stream subscriptions; notification fan-out has independent lifecycle and
//! replay requirements.

use crate::TenantId;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex};

pub const DEFAULT_INTERACTION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct InteractionKey {
    tenant_id: TenantId,
    gateway_session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractionBinding {
    key: InteractionKey,
    id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum InteractionError {
    #[error("the client disconnected before responding to the agent request")]
    ClientDisconnected,
    #[error("timed out after {0:?} waiting for the client to answer the agent request")]
    TimedOut(Duration),
}

#[derive(Clone, Default)]
pub struct InteractionHub {
    state: Arc<Mutex<InteractionState>>,
}

#[derive(Default)]
struct InteractionState {
    bindings: HashMap<InteractionKey, Binding>,
    pending: HashMap<String, PendingInteraction>,
}

#[derive(Clone)]
struct Binding {
    id: String,
    sender: mpsc::UnboundedSender<serde_json::Value>,
}

struct PendingInteraction {
    binding_id: String,
    sender: oneshot::Sender<Result<serde_json::Value, InteractionError>>,
}

impl InteractionHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind one persistent client connection to a tenant-scoped session.
    /// A newer bind replaces the old owner, matching ACPX's current
    /// single-writer session ownership model.
    pub async fn bind(
        &self,
        tenant_id: TenantId,
        gateway_session_id: impl Into<String>,
        sender: mpsc::UnboundedSender<serde_json::Value>,
    ) -> InteractionBinding {
        let key = InteractionKey {
            tenant_id,
            gateway_session_id: gateway_session_id.into(),
        };
        let binding = InteractionBinding {
            key: key.clone(),
            id: uuid::Uuid::new_v4().to_string(),
        };
        self.state.lock().await.bindings.insert(
            key,
            Binding {
                id: binding.id.clone(),
                sender,
            },
        );
        binding
    }

    /// Remove a binding only if it still belongs to this exact connection.
    /// This prevents an old connection from removing a newer owner's binding.
    pub async fn unbind(&self, binding: &InteractionBinding) {
        let mut state = self.state.lock().await;
        if state
            .bindings
            .get(&binding.key)
            .is_some_and(|current| current.id == binding.id)
        {
            state.bindings.remove(&binding.key);
            let failed = state
                .pending
                .extract_if(|_, pending| pending.binding_id == binding.id)
                .map(|(_, pending)| pending.sender)
                .collect::<Vec<_>>();
            for sender in failed {
                let _ = sender.send(Err(InteractionError::ClientDisconnected));
            }
        }
    }

    /// Forward an agent-to-client request and wait for the matching client
    /// response. `None` means no persistent client is currently bound.
    pub async fn request(
        &self,
        tenant_id: &TenantId,
        gateway_session_id: &str,
        mut request: serde_json::Value,
        timeout: Duration,
    ) -> Result<Option<serde_json::Value>, InteractionError> {
        let key = InteractionKey {
            tenant_id: tenant_id.clone(),
            gateway_session_id: gateway_session_id.to_string(),
        };
        let public_id = format!("acpx-interaction-{}", uuid::Uuid::new_v4());
        request["id"] = serde_json::Value::String(public_id.clone());
        let (tx, rx) = oneshot::channel();

        {
            let mut state = self.state.lock().await;
            let Some(binding) = state.bindings.get(&key).cloned() else {
                return Ok(None);
            };
            state.pending.insert(
                public_id.clone(),
                PendingInteraction {
                    binding_id: binding.id.clone(),
                    sender: tx,
                },
            );
            if binding.sender.send(request).is_err() {
                state.pending.remove(&public_id);
                if state
                    .bindings
                    .get(&key)
                    .is_some_and(|current| current.id == binding.id)
                {
                    state.bindings.remove(&key);
                }
                return Err(InteractionError::ClientDisconnected);
            }
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(response))) => Ok(Some(response)),
            Ok(Ok(Err(error))) => Err(error),
            Ok(Err(_)) => Err(InteractionError::ClientDisconnected),
            Err(_) => {
                self.state.lock().await.pending.remove(&public_id);
                Err(InteractionError::TimedOut(timeout))
            }
        }
    }

    /// Resolve a response received on a persistent client transport. Returns
    /// false for ordinary responses that do not belong to an interaction.
    pub async fn resolve(&self, response: serde_json::Value) -> bool {
        let Some(id) = response.get("id").and_then(|id| id.as_str()) else {
            return false;
        };
        let pending = self.state.lock().await.pending.remove(id);
        pending.is_some_and(|pending| pending.sender.send(Ok(response)).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_uses_an_opaque_id_and_restores_the_client_response() {
        let hub = InteractionHub::new();
        let tenant = TenantId::from("tenant-a");
        let (tx, mut rx) = mpsc::unbounded_channel();
        hub.bind(tenant.clone(), "session-a", tx).await;

        let hub_for_request = hub.clone();
        let tenant_for_request = tenant.clone();
        let request = tokio::spawn(async move {
            hub_for_request
                .request(
                    &tenant_for_request,
                    "session-a",
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 42,
                        "method": "session/request_permission",
                        "params": {"sessionId": "session-a"}
                    }),
                    Duration::from_secs(1),
                )
                .await
                .expect("interaction request")
                .expect("bound client")
        });

        let forwarded = rx.recv().await.expect("forwarded request");
        let id = forwarded["id"]
            .as_str()
            .expect("opaque interaction id")
            .to_string();
        assert_ne!(id, "42");
        assert!(
            hub.resolve(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"outcome": "selected"}
            }))
            .await
        );
        assert_eq!(
            request.await.expect("request task"),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": forwarded["id"],
                "result": {"outcome": "selected"}
            })
        );
    }

    #[tokio::test]
    async fn an_old_binding_cannot_remove_a_new_owner() {
        let hub = InteractionHub::new();
        let tenant = TenantId::from("tenant-a");
        let (old_tx, _old_rx) = mpsc::unbounded_channel();
        let old = hub.bind(tenant.clone(), "session-a", old_tx).await;
        let (new_tx, mut new_rx) = mpsc::unbounded_channel();
        hub.bind(tenant.clone(), "session-a", new_tx).await;
        hub.unbind(&old).await;

        let hub_for_request = hub.clone();
        let tenant_for_request = tenant.clone();
        let pending = tokio::spawn(async move {
            hub_for_request
                .request(
                    &tenant_for_request,
                    "session-a",
                    serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "fs/read_text_file"}),
                    Duration::from_secs(1),
                )
                .await
        });
        let forwarded = new_rx.recv().await.expect("new owner receives request");
        hub.resolve(serde_json::json!({
            "jsonrpc": "2.0",
            "id": forwarded["id"],
            "result": {}
        }))
        .await;
        assert!(pending.await.expect("request task").is_ok());
    }
}
