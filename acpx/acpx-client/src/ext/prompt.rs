//! Convenience wrapper around `session/prompt` for the common case of
//! wanting the actual assistant reply text, not just `{stopReason,
//! usage}`. Added post-Phase-6 alongside the gateway's `_acpx.updates`
//! aggregation fix (see `acpx_core::router::read_matching_response`'s doc
//! comment) -- every real ACP adapter checked against a real published
//! package (`@agentclientprotocol/claude-agent-acp`) streams the actual
//! reply as `session/update` `agent_message_chunk` notifications during
//! the call, never in the JSON-RPC result itself. A caller that only used
//! `raw::GatewayClient::call("session/prompt", ...)` would get a
//! technically-successful response with no visible answer in it at all.
//!
//! Like the rest of `ext/`, this is a thin additive layer -- nothing here
//! changes `raw`'s own behavior, and a caller that wants the raw envelope
//! (e.g. to inspect `agent_thought_chunk`s, tool-call updates, or usage
//! metadata too) should call [`crate::raw::GatewayClient::call_with_updates`]
//! directly instead.

use crate::raw::{ClientError, GatewayClient};
use serde_json::Value;

/// Result of [`send`]: the raw `session/prompt` result (`stopReason`,
/// `usage`, ...) plus the concatenated assistant reply text extracted
/// from every `agent_message_chunk` update seen along the way, in
/// streaming order.
#[derive(Debug, Clone)]
pub struct PromptOutcome {
    pub result: Value,
    pub message_text: String,
    /// Every raw `session/update` notification observed during the call,
    /// unmodified, for a caller that needs more than just the assistant
    /// message text (tool calls, thought chunks, usage/cost updates...).
    pub updates: Vec<Value>,
}

/// Send one `session/prompt` turn and collect the assistant's reply text.
/// `prompt` is the raw ACP `prompt` content-block array (forwarded
/// byte-for-byte, same as `raw::GatewayClient::call` -- this function adds
/// no interpretation of the request itself, only of the response).
pub async fn send(
    client: &GatewayClient,
    session_id: &str,
    prompt: Value,
) -> Result<PromptOutcome, ClientError> {
    let (result, updates) = client
        .call_with_updates(
            "session/prompt",
            serde_json::json!({ "sessionId": session_id, "prompt": prompt }),
            None,
        )
        .await?;
    Ok(PromptOutcome {
        result,
        message_text: extract_message_text(&updates),
        updates,
    })
}

/// Concatenate every `agent_message_chunk`'s text content out of a list of
/// raw `session/update` notifications, in order. Free function (not tied
/// to a live `GatewayClient` call) so a caller that already collected
/// updates some other way (e.g. via `raw::GatewayClient::call_with_updates`
/// directly) can reuse the exact same extraction logic.
pub fn extract_message_text(updates: &[Value]) -> String {
    let mut out = String::new();
    for update in updates {
        if update.get("method").and_then(|m| m.as_str()) != Some("session/update") {
            continue;
        }
        let session_update = update
            .get("params")
            .and_then(|p| p.get("update"))
            .and_then(|u| u.get("sessionUpdate"))
            .and_then(|s| s.as_str());
        if session_update != Some("agent_message_chunk") {
            continue;
        }
        if let Some(text) = update
            .get("params")
            .and_then(|p| p.get("update"))
            .and_then(|u| u.get("content"))
            .and_then(|c| c.get("text"))
            .and_then(|t| t.as_str())
        {
            out.push_str(text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(text: &str) -> Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}
            }
        })
    }

    #[test]
    fn concatenates_message_chunks_in_order() {
        let updates = vec![chunk("Hello"), chunk(", "), chunk("world")];
        assert_eq!(extract_message_text(&updates), "Hello, world");
    }

    #[test]
    fn ignores_non_message_chunk_updates() {
        let thought = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "thinking..."}}
            }
        });
        let updates = vec![thought, chunk("actual reply")];
        assert_eq!(extract_message_text(&updates), "actual reply");
    }

    #[test]
    fn empty_updates_yields_empty_string() {
        assert_eq!(extract_message_text(&[]), "");
    }
}
