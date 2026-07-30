//! ACPX session-stream, transcript, and queue extension contracts.
//!
//! These are additive ACPX methods. Native `session/list`, `session/resume`,
//! and `session/load` remain the public ACP methods; these types describe only
//! the missing selected-session stream, bounded history, sync, and queue
//! surfaces.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const INITIAL_HISTORY_LIMIT: u32 = 50;
pub const OLDER_HISTORY_LIMIT: u32 = 40;
pub const MAX_HISTORY_LIMIT: u32 = 40;

fn default_background() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionsSubscribeParams {
    pub session_ids: Vec<String>,
    #[serde(default = "default_background")]
    pub background: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionsSubscribeResult {
    pub session_ids: Vec<String>,
    pub background: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionPaginateParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl SessionPaginateParams {
    pub fn effective_limit(&self) -> u32 {
        self.limit
            .unwrap_or(OLDER_HISTORY_LIMIT)
            .min(MAX_HISTORY_LIMIT)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionPageResult {
    pub session_id: String,
    pub messages: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionSyncParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_message_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_matched_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionSyncResult {
    pub session_id: String,
    pub patch: SessionTranscriptPatch,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionTranscriptPatch {
    pub start_index: u64,
    pub replace_count: u64,
    pub messages: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QueueSubscribeParams {
    pub session_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QueueSubscribeResult {
    pub session_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QueueMutationParams {
    pub session_id: String,
    pub idempotency_key: String,
    pub operation: QueueOperation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_entry_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum QueueOperation {
    Enqueue,
    SendNow,
    Cancel,
    Pause,
    Resume,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QueueMutationResult {
    pub session_id: String,
    pub idempotency_key: String,
    pub accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_entry_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribe_defaults_to_background_and_keeps_session_ids_explicit() {
        let params: SessionsSubscribeParams =
            serde_json::from_value(serde_json::json!({"sessionIds": ["s1"]})).unwrap();
        assert!(params.background);
        assert_eq!(params.session_ids, vec!["s1"]);
    }

    #[test]
    fn pagination_is_bounded_to_forty_and_defaults_to_forty() {
        let default = SessionPaginateParams {
            session_id: "s1".into(),
            before: None,
            limit: None,
        };
        assert_eq!(default.effective_limit(), 40);
        let oversized = SessionPaginateParams {
            limit: Some(500),
            ..default
        };
        assert_eq!(oversized.effective_limit(), 40);
    }

    #[test]
    fn queue_mutation_requires_client_idempotency_key_on_the_wire() {
        let missing = serde_json::from_value::<QueueMutationParams>(serde_json::json!({
            "sessionId": "s1",
            "operation": "enqueue",
            "text": "hello"
        }));
        assert!(missing.is_err());
    }
}
