//! Typed ACPX session-stream, transcript, and queue helpers.

use crate::{raw::ClientError, Gateway};
use acpx_proto::session_stream::{
    QueueMutationParams, QueueMutationResult, QueueSubscribeParams, QueueSubscribeResult,
    SessionPageResult, SessionPaginateParams, SessionSyncParams, SessionSyncResult,
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
    call_typed(gateway, "acpx/sessions/queue/subscribe", params).await
}

pub async fn mutate_queue(
    gateway: &Gateway,
    params: QueueMutationParams,
) -> Result<QueueMutationResult, ClientError> {
    call_typed(gateway, "session/queue", params).await
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
}
