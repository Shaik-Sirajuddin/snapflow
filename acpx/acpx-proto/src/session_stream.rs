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
        let default = if self.before.is_none() {
            INITIAL_HISTORY_LIMIT
        } else {
            OLDER_HISTORY_LIMIT
        };
        let maximum = if self.before.is_none() {
            INITIAL_HISTORY_LIMIT
        } else {
            MAX_HISTORY_LIMIT
        };
        self.limit.unwrap_or(default).min(maximum)
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
    #[serde(default)]
    pub snapshots: Vec<QueueSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QueueSnapshot {
    pub session_id: String,
    pub queue: Vec<QueueItem>,
    pub paused: bool,
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
    pub queue: Vec<QueueItem>,
    pub paused: bool,
}

/// Explicitly steer the active turn.  This is an ACPX gateway method; it is
/// never forwarded to the backend as `session/steer`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionSteerParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<Vec<Value>>,
    pub idempotency_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_entry_id: Option<String>,
}

impl SessionSteerParams {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.queue_entry_id.is_none() && self.prompt.is_none() {
            return Err("either queueEntryId or prompt is required");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionSteerResult {
    pub session_id: String,
    pub accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_entry_id: Option<String>,
}

/// Lifecycle notification for an explicit session steer. This is separate
/// from queue deltas so clients can render the steer turn state directly.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionSteerEvent {
    pub session_id: String,
    pub state: SessionSteerState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_entry_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SessionSteerState {
    Queued,
    Dispatched,
    Completed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum QueueMutation {
    Inserted,
    #[serde(rename = "sent_prompt")]
    SentPrompt,
    Removed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QueueMutationEvent {
    pub session_id: String,
    pub queue_entry_id: String,
    pub mutation: QueueMutation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QueueItem {
    pub queue_entry_id: String,
    pub idempotency_key: String,
    pub text: String,
    pub state: String,
    pub position: u32,
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
    fn pagination_defaults_to_fifty_then_bounds_older_pages_to_forty() {
        let default = SessionPaginateParams {
            session_id: "s1".into(),
            before: None,
            limit: None,
        };
        assert_eq!(default.effective_limit(), 50);
        let oversized = SessionPaginateParams {
            limit: Some(500),
            ..default.clone()
        };
        assert_eq!(oversized.effective_limit(), 50);
        let older = SessionPaginateParams {
            before: Some("10".into()),
            limit: Some(500),
            ..default
        };
        assert_eq!(older.effective_limit(), 40);
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

    #[test]
    fn steer_requires_queue_entry_or_prompt() {
        let params = SessionSteerParams {
            session_id: "s1".into(),
            prompt: None,
            idempotency_key: "k".into(),
            queue_entry_id: None,
        };
        assert!(params.validate().is_err());
    }

    #[test]
    fn sent_prompt_mutation_uses_stable_snake_case_wire_name() {
        assert_eq!(
            serde_json::to_value(QueueMutation::SentPrompt).unwrap(),
            "sent_prompt"
        );
    }
}
