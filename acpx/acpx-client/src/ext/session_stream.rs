//! Typed ACPX session-stream, transcript, and queue helpers.

use crate::{raw::ClientError, Gateway};
use acpx_proto::session_stream::{
    QueueItem, QueueMutation, QueueMutationEvent, QueueMutationParams, QueueMutationResult,
    QueueSubscribeParams, QueueSubscribeResult, SessionPageResult, SessionPaginateParams,
    SessionSteerParams, SessionSteerResult, SessionSyncParams, SessionSyncResult,
    SessionsSubscribeParams, SessionsSubscribeResult,
};

pub async fn subscribe(
    gateway: &Gateway,
    params: SessionsSubscribeParams,
) -> Result<SessionsSubscribeResult, ClientError> {
    call_typed(gateway, "acpx/sessions/subscribe", params).await
}

pub async fn paginate(
    gateway: &Gateway,
    params: SessionPaginateParams,
) -> Result<SessionPageResult, ClientError> {
    call_typed(gateway, "acpx/sessions/paginate", params).await
}

pub async fn sync(
    gateway: &Gateway,
    params: SessionSyncParams,
) -> Result<SessionSyncResult, ClientError> {
    call_typed(gateway, "acpx/sessions/sync", params).await
}

pub async fn subscribe_queue(
    gateway: &Gateway,
    params: QueueSubscribeParams,
) -> Result<QueueSubscribeResult, ClientError> {
    let result = gateway
        .subscribe_queue_sessions(&params.session_ids, params.cursor.as_deref())
        .await?;
    serde_json::from_value(result).map_err(|error| {
        ClientError::WebSocket(format!(
            "invalid acpx/sessions/queue/subscribe response: {error}"
        ))
    })
}

pub async fn mutate_queue(
    gateway: &Gateway,
    params: QueueMutationParams,
) -> Result<QueueMutationResult, ClientError> {
    call_typed(gateway, "session/queue", params).await
}

pub async fn steer(
    gateway: &Gateway,
    params: SessionSteerParams,
) -> Result<SessionSteerResult, ClientError> {
    params
        .validate()
        .map_err(|error| ClientError::WebSocket(error.to_string()))?;
    call_typed(gateway, "session/steer", params).await
}

/// Client-owned latest queue projection. Queue push notifications are
/// mutation events; consumers can render this projection or derive another
/// view without requiring ACPX to resend the entire queue.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueProjection {
    pub items: Vec<QueueItem>,
}

impl QueueProjection {
    pub fn from_snapshot(snapshot: &acpx_proto::session_stream::QueueSnapshot) -> Self {
        Self {
            items: snapshot.queue.clone(),
        }
    }

    pub fn apply_event(&mut self, event: &QueueMutationEvent) {
        match event.mutation {
            QueueMutation::Inserted => {
                if let Some(existing) = self
                    .items
                    .iter_mut()
                    .find(|item| item.queue_entry_id == event.queue_entry_id)
                {
                    if let Some(position) = event.position {
                        existing.position = position;
                    }
                    if let Some(key) = &event.idempotency_key {
                        existing.idempotency_key = key.clone();
                    }
                } else {
                    self.items.push(QueueItem {
                        queue_entry_id: event.queue_entry_id.clone(),
                        idempotency_key: event.idempotency_key.clone().unwrap_or_default(),
                        text: String::new(),
                        state: "queued".into(),
                        position: event.position.unwrap_or(self.items.len() as u32),
                    });
                }
            }
            QueueMutation::SentPrompt | QueueMutation::Removed => {
                self.items
                    .retain(|item| item.queue_entry_id != event.queue_entry_id);
            }
        }
        self.items.sort_by_key(|item| item.position);
        for (position, item) in self.items.iter_mut().enumerate() {
            item.position = position as u32;
        }
    }
}

async fn call_typed<P, R>(gateway: &Gateway, method: &str, params: P) -> Result<R, ClientError>
where
    P: serde::Serialize,
    R: serde::de::DeserializeOwned,
{
    let params = serde_json::to_value(params)
        .map_err(|error| ClientError::WebSocket(format!("invalid {method} params: {error}")))?;
    let result = gateway.call(method, params, None).await?;
    serde_json::from_value(result)
        .map_err(|error| ClientError::WebSocket(format!("invalid {method} response: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use acpx_proto::session_stream::{QueueOperation, OLDER_HISTORY_LIMIT};

    #[test]
    fn contracts_serialize_to_camel_case_wire_fields() {
        let page = serde_json::to_value(SessionPaginateParams {
            session_id: "s1".into(),
            before: None,
            limit: Some(OLDER_HISTORY_LIMIT),
        })
        .unwrap();
        assert_eq!(page["sessionId"], "s1");
        assert_eq!(page["limit"], 40);

        let queue = serde_json::to_value(QueueMutationParams {
            session_id: "s1".into(),
            idempotency_key: "client-1".into(),
            operation: QueueOperation::Enqueue,
            queue_entry_id: None,
            text: Some("hello".into()),
        })
        .unwrap();
        assert_eq!(queue["idempotencyKey"], "client-1");
        assert_eq!(queue["operation"], "enqueue");
    }

    #[test]
    fn queue_projection_applies_mutations_without_snapshot() {
        let mut projection = QueueProjection::default();
        projection.apply_event(&QueueMutationEvent {
            session_id: "s1".into(),
            queue_entry_id: "q1".into(),
            mutation: QueueMutation::Inserted,
            idempotency_key: Some("k1".into()),
            position: Some(0),
        });
        assert_eq!(projection.items.len(), 1);
        projection.apply_event(&QueueMutationEvent {
            session_id: "s1".into(),
            queue_entry_id: "q1".into(),
            mutation: QueueMutation::SentPrompt,
            idempotency_key: Some("k1".into()),
            position: None,
        });
        assert!(projection.items.is_empty());
    }
}
