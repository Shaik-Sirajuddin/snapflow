//! `rui-acpx-client`: the chat panel's acpx-gateway-backed session layer.
//!
//! Per `memory/rui/gen/plans/chat-panel/chat-panel-acpx-gateway-integration.md`
//! (revised: **full cutover**, not the additive per-thread toggle the v1.2
//! proposal defaulted to -- explicitly requested directly, not inferred:
//! "go full acpx usage for all"), every `panel-rust` chat thread's agent
//! connection is one of these, never a direct `rui-acp-client` subprocess
//! connection. This crate is the *only* place that depends on
//! `acpx-client` (the real acpx gateway SDK) -- `panel-rust` depends on
//! this crate's public API only ([`AcpxThreadHandle`], [`spawn_acpx_thread`],
//! plus the re-exported [`ChatMessage`]/[`MessageKind`]/[`AgentEvent`]
//! shared with the (still-present, jsonl-cache-owning) `rui-acp-client`
//! crate) and never sees `acpx-client`'s own wire types.
//!
//! ## Why this crate exists instead of extending `rui-acp-client` in place
//!
//! `acpx-client` pulls in `reqwest`/HTTP-transport dependencies that have
//! no reason to be in `rui-acp-client`'s own dependency graph (that crate
//! wraps a subprocess-stdio ACP connection -- no HTTP client needed at
//! all). Keeping them as two crates, both depended on by `panel-rust`,
//! matches this codebase's established modularity discipline (per-concern
//! crates, no monolithic growth) and keeps `rui-acp-client` a clean,
//! reusable "local ACP message model + jsonl cache" library regardless of
//! which transport(s) ever feed it.
//!
//! ## Wire shape: why raw-JSON `session/update` classification, not the
//! typed `agent_client_protocol::SessionUpdate` enum
//!
//! `acpx-client::raw::GatewayClient::call_with_updates` returns
//! `_acpx.updates` as a `Vec<serde_json::Value>` -- raw JSON-RPC
//! notification envelopes, not typed ACP schema values (the gateway
//! proxies bytes, it doesn't re-derive Rust types for a client SDK that
//! deliberately avoids depending on `agent-client-protocol` itself, see
//! `acpx-client::raw`'s own doc comment on that boundary). [`classify_raw_update`]
//! is this crate's equivalent of `rui-acp-client::session_client`'s
//! `classify_update`, operating on that raw JSON shape instead -- same
//! message-kind vocabulary and status-string convention, verified against
//! `acpx-core`'s own `session_update_forwarding_test.rs` fixture shapes.

//! Ported directly into `panel-rust` from the former standalone
//! `rui-acpx-client` crate (Phase 2 of `chat-panel-production-ui/
//! execution-plan.md`'s "delete rui-acp-client/rui-acpx-client as
//! separate crates" goal) -- logic and structure otherwise unchanged
//! from that crate's own `lib.rs`, only the shared-type import source
//! moved from `rui_acp_client::*` to `crate::protocol_types::*`.

mod thread_actor;

pub use crate::protocol_types::{
    AgentEvent, ChatMessage, ConfigOptionInfo, ConfigOptionValue, MessageKind, SessionModeInfo,
    SessionModesEvent,
};
pub use thread_actor::{
    spawn_acpx_thread, spawn_acpx_thread_with_delayed_gateway, spawn_acpx_thread_with_gateway,
    AcpxThreadError, AcpxThreadGatewaySetter, AcpxThreadHandle, ProfileSummary, RemoteThreadInfo,
};

/// Maps one raw `session/update` JSON-RPC notification (as returned in
/// `acpx-client`'s `_acpx.updates` array) into this crate's shared
/// `ChatMessage` vocabulary. `None` for anything not shaped like a
/// `session/update` notification, or a `sessionUpdate` kind this UI
/// doesn't render as a message yet -- same deliberate scope narrowing
/// `rui_acp_client::session_client::classify_update` documents for the
/// direct-ACP path.
pub(crate) fn classify_raw_update(update: &serde_json::Value) -> Option<ChatMessage> {
    if update.get("method").and_then(|m| m.as_str()) != Some("session/update") {
        return None;
    }
    let session_update = update.get("params")?.get("update")?;
    let kind = session_update
        .get("sessionUpdate")
        .and_then(|k| k.as_str())?;

    let text_of = |v: &serde_json::Value| -> Option<String> {
        v.get("content")
            .and_then(|c| c.get("text"))
            .and_then(|t| t.as_str())
            .map(str::to_string)
    };
    // `messageId` (chunks) / `toolCallId` (tool calls) -- an RFD-status,
    // v1-optional field on the real wire (agentclientprotocol.com/rfds/
    // message-id); when present, this lets `AgentBridge`'s transcript
    // reducer merge by real id instead of falling back to its own
    // synthetic-adjacency heuristic (see `agent_bridge.rs`'s ingestion
    // logic).
    let id_of = |field: &str| -> Option<String> {
        session_update
            .get(field)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    // chat-items-redesign.md #9 (execution-view "api-call" variant):
    // `rawInput`/`rawOutput` are real wire fields on `tool_call`/
    // `tool_call_update` (`ToolCallUpdateFields`/`ToolCall` in
    // `agent-client-protocol`) that this classifier previously discarded
    // entirely -- not missing data, just unread data.
    let raw_of = |field: &str| -> Option<serde_json::Value> { session_update.get(field).cloned() };

    match kind {
        "agent_message_chunk" => text_of(session_update).map(|text| ChatMessage {
            kind: MessageKind::Agent,
            text,
            status: None,
            id: id_of("messageId"),
            raw_input: None,
            raw_output: None,
        }),
        "agent_thought_chunk" => text_of(session_update).map(|text| ChatMessage {
            kind: MessageKind::Thinking,
            text,
            status: None,
            id: id_of("messageId"),
            raw_input: None,
            raw_output: None,
        }),
        "user_message_chunk" => text_of(session_update).map(|text| ChatMessage {
            kind: MessageKind::User,
            text,
            status: None,
            id: None,
            raw_input: None,
            raw_output: None,
        }),
        // `tool_call`'s wire shape carries `toolCallId`/`title`/`status`
        // directly under `update` (not nested under a separate "fields"
        // object -- that's only `ToolCallUpdateFields`'s Rust-side struct
        // name, not a wire wrapper). A fresh tool_call always emits, even
        // with an empty title, mirroring `classify_update`'s direct-ACP
        // behavior for `SessionUpdate::ToolCall`.
        "tool_call" => {
            let title = session_update
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string();
            let status = session_update
                .get("status")
                .and_then(|s| s.as_str())
                .map(str::to_string);
            Some(ChatMessage {
                kind: MessageKind::ToolCall,
                text: title,
                status,
                id: id_of("toolCallId"),
                raw_input: raw_of("rawInput"),
                raw_output: raw_of("rawOutput"),
            })
        }
        // A status-only update (no title change) must still surface --
        // same fix `rui_acp_client::session_client::classify_update`
        // carries for `SessionUpdate::ToolCallUpdate`, mirrored here so
        // the gateway path doesn't regress a bug already fixed on the
        // direct path.
        "tool_call_update" => {
            let title = session_update
                .get("title")
                .and_then(|t| t.as_str())
                .map(str::to_string);
            let status = session_update
                .get("status")
                .and_then(|s| s.as_str())
                .map(str::to_string);
            if title.is_none() && status.is_none() {
                return None;
            }
            Some(ChatMessage {
                kind: MessageKind::ToolCall,
                text: title.unwrap_or_default(),
                status,
                id: id_of("toolCallId"),
                raw_input: raw_of("rawInput"),
                raw_output: raw_of("rawOutput"),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod classify_raw_update_tests {
    use super::*;
    use serde_json::json;

    fn update_notification(session_update: serde_json::Value) -> serde_json::Value {
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": "s1", "update": session_update}
        })
    }

    #[test]
    fn agent_message_chunk_maps_to_agent_kind() {
        let update = update_notification(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "hello"}
        }));
        let msg = classify_raw_update(&update).expect("message");
        assert_eq!(msg.kind, MessageKind::Agent);
        assert_eq!(msg.text, "hello");
        assert_eq!(msg.status, None);
        assert_eq!(msg.id, None);
    }

    #[test]
    fn agent_message_chunk_extracts_message_id_when_present() {
        let update = update_notification(json!({
            "sessionUpdate": "agent_message_chunk",
            "messageId": "msg-42",
            "content": {"type": "text", "text": "hello"}
        }));
        let msg = classify_raw_update(&update).expect("message");
        assert_eq!(msg.id.as_deref(), Some("msg-42"));
    }

    #[test]
    fn tool_call_extracts_tool_call_id() {
        let update = update_notification(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "tc-7",
            "title": "ffmpeg.export(...)",
            "status": "in_progress"
        }));
        let msg = classify_raw_update(&update).expect("message");
        assert_eq!(msg.id.as_deref(), Some("tc-7"));
    }

    #[test]
    fn agent_thought_chunk_maps_to_thinking_kind() {
        let update = update_notification(json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": "considering..."}
        }));
        let msg = classify_raw_update(&update).expect("message");
        assert_eq!(msg.kind, MessageKind::Thinking);
    }

    #[test]
    fn tool_call_carries_status_into_chat_message() {
        let update = update_notification(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "tc-1",
            "title": "ffmpeg.export(...)",
            "status": "in_progress"
        }));
        let msg = classify_raw_update(&update).expect("message");
        assert_eq!(msg.kind, MessageKind::ToolCall);
        assert_eq!(msg.text, "ffmpeg.export(...)");
        assert_eq!(msg.status.as_deref(), Some("in_progress"));
    }

    #[test]
    fn status_only_tool_call_update_still_produces_a_message() {
        let update = update_notification(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "tc-1",
            "status": "completed"
        }));
        let msg = classify_raw_update(&update).expect("message");
        assert_eq!(msg.kind, MessageKind::ToolCall);
        assert_eq!(msg.text, "");
        assert_eq!(msg.status.as_deref(), Some("completed"));
    }

    #[test]
    fn tool_call_update_with_neither_title_nor_status_is_dropped() {
        let update = update_notification(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "tc-1"
        }));
        assert!(classify_raw_update(&update).is_none());
    }

    #[test]
    fn non_session_update_method_is_ignored() {
        let other = json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}});
        assert!(classify_raw_update(&other).is_none());
    }

    #[test]
    fn unknown_session_update_kind_is_ignored_not_an_error() {
        let update = update_notification(json!({"sessionUpdate": "plan"}));
        assert!(classify_raw_update(&update).is_none());
    }
}
