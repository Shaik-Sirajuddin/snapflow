//! Interactive agent-request (permission/fs/terminal-approval) UI
//! projection and response-building -- kept apart from `agent_bridge.rs`
//! (which only forwards/queues the raw event) and `lib.rs` (FFI/event
//! wiring glue), per this crate's established modularity convention
//! (see `models.rs`'s doc comment).
//!
//! ## Approval model (Zed-aligned one-of select)
//!
//! Zed's `PermissionOptions::Flat` renders every ACP `PermissionOption`
//! as its own button; clicking one sends
//! `RequestPermissionOutcome::Selected { optionId, kind }` immediately.
//! That is a **one-of select**: the choice *is* the action, not a two-step
//! "pick then confirm".
//!
//! This module projects the same model for our panel:
//! - `session/request_permission` → real `params.options[]` rows
//! - `fs/*` / `terminal/create` → synthetic Approve / Reject rows
//! - unknown methods → empty options (card still shows title/summary)
//!
//! Keyboard conveniences map onto the same list: Ctrl+Enter ≈ first
//! `allow_*` option, Escape ≈ first `reject_*` (or cancel).

use crate::protocol_types::AgentRequestEvent;
use serde_json::Value;
use slint::{ModelRc, VecModel};

/// One selectable approval choice for the Slint card (mirrors
/// [`crate::PermissionOptionItem`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOptionView {
    pub option_id: String,
    pub name: String,
    pub kind: String,
    pub is_allow: bool,
}

/// What a request card actually shows: a short title (method-derived),
/// a human-readable one-line summary of the specific action being
/// requested, and the one-of option list.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingRequestView {
    pub relay_id: String,
    pub method: String,
    pub title: String,
    pub summary: String,
    pub options: Vec<PermissionOptionView>,
}

/// Whether this UI knows how to build a real response for `method`.
pub fn is_supported_method(method: &str) -> bool {
    matches!(
        method,
        "session/request_permission"
            | "fs/read_text_file"
            | "fs/write_text_file"
            | "terminal/create"
    )
}

fn kind_is_allow(kind: &str) -> bool {
    kind.starts_with("allow_") || kind == "approve"
}

fn kind_is_reject(kind: &str) -> bool {
    kind.starts_with("reject_") || kind == "reject"
}

/// Projects ACP `params.options` (or synthetic fs/terminal pairs) into
/// the one-of list the permission card renders.
pub fn extract_options(event: &AgentRequestEvent) -> Vec<PermissionOptionView> {
    match event.method.as_str() {
        "session/request_permission" => {
            let options = event
                .raw_request
                .get("params")
                .and_then(|p| p.get("options"))
                .and_then(|o| o.as_array())
                .cloned()
                .unwrap_or_default();
            options
                .iter()
                .filter_map(|opt| {
                    let option_id = opt
                        .get("optionId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if option_id.is_empty() {
                        return None;
                    }
                    let name = opt
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or(option_id.as_str())
                        .to_string();
                    let kind = opt
                        .get("kind")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    Some(PermissionOptionView {
                        is_allow: kind_is_allow(&kind),
                        option_id,
                        name,
                        kind,
                    })
                })
                .collect()
        }
        // Lightweight relay methods: synthesize a binary one-of so the
        // same card UX covers every supported request kind.
        "fs/read_text_file" | "fs/write_text_file" | "terminal/create" => vec![
            PermissionOptionView {
                option_id: "approve".into(),
                name: "Approve".into(),
                kind: "allow_once".into(),
                is_allow: true,
            },
            PermissionOptionView {
                option_id: "reject".into(),
                name: "Reject".into(),
                kind: "reject_once".into(),
                is_allow: false,
            },
        ],
        _ => Vec::new(),
    }
}

/// Projects a raw [`AgentRequestEvent`] into what the Slint request-card
/// component renders.
pub fn to_pending_request_view(event: &AgentRequestEvent) -> PendingRequestView {
    let params = event.raw_request.get("params");
    let (title, summary) = match event.method.as_str() {
        "session/request_permission" => {
            let tool_title = params
                .and_then(|p| p.get("toolCall"))
                .and_then(|t| t.get("title"))
                .and_then(|t| t.as_str())
                .unwrap_or("a tool call");
            (
                "Permission requested".to_string(),
                format!("The agent wants to run: {tool_title}"),
            )
        }
        "fs/read_text_file" => {
            let path = params
                .and_then(|p| p.get("path"))
                .and_then(|p| p.as_str())
                .unwrap_or("(unknown path)");
            (
                "File read requested".to_string(),
                format!("The agent wants to read: {path}"),
            )
        }
        "fs/write_text_file" => {
            let path = params
                .and_then(|p| p.get("path"))
                .and_then(|p| p.as_str())
                .unwrap_or("(unknown path)");
            (
                "File write requested".to_string(),
                format!("The agent wants to write: {path}"),
            )
        }
        "terminal/create" => {
            let command = params
                .and_then(|p| p.get("command"))
                .and_then(|c| c.as_str())
                .unwrap_or("(unknown command)");
            let args: String = params
                .and_then(|p| p.get("args"))
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            (
                "Terminal command requested".to_string(),
                format!("The agent wants to run: {command} {args}")
                    .trim_end()
                    .to_string(),
            )
        }
        other => (
            format!("Unsupported request: {other}"),
            "This request kind isn't handled by this UI yet.".to_string(),
        ),
    };
    PendingRequestView {
        relay_id: event.relay_id.clone(),
        method: event.method.clone(),
        title,
        summary,
        options: extract_options(event),
    }
}

/// Convert option views into the Slint model.
pub fn to_permission_option_model(
    options: Vec<PermissionOptionView>,
) -> ModelRc<crate::PermissionOptionItem> {
    let items: Vec<crate::PermissionOptionItem> = options
        .into_iter()
        .map(|o| crate::PermissionOptionItem {
            option_id: o.option_id.into(),
            name: o.name.into(),
            kind: o.kind.into(),
            is_allow: o.is_allow,
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

/// First allow_* (or synthetic approve) option id — used by Ctrl+Enter.
pub fn default_allow_option_id(options: &[PermissionOptionView]) -> Option<&str> {
    options
        .iter()
        .find(|o| o.is_allow)
        .map(|o| o.option_id.as_str())
}

/// First reject_* (or synthetic reject) option id — used by Escape.
pub fn default_reject_option_id(options: &[PermissionOptionView]) -> Option<&str> {
    options
        .iter()
        .find(|o| kind_is_reject(&o.kind))
        .map(|o| o.option_id.as_str())
}

/// Builds the response payload for a concrete option pick (one-of select).
///
/// - `session/request_permission` → full JSON-RPC result with
///   `{ outcome: { outcome: "selected", optionId } }` (or cancelled)
/// - fs/terminal → `{ "approved": bool }` for the synthetic approve/reject ids
pub fn build_response_for_option(event: &AgentRequestEvent, option_id: &str) -> Value {
    if event.method != "session/request_permission" {
        let approved = option_id != "reject" && !option_id.starts_with("reject");
        return serde_json::json!({ "approved": approved });
    }

    let options: Vec<Value> = event
        .raw_request
        .get("params")
        .and_then(|p| p.get("options"))
        .and_then(|o| o.as_array())
        .cloned()
        .unwrap_or_default();

    let chosen = options.iter().find(|opt| {
        opt.get("optionId")
            .and_then(|o| o.as_str())
            .map(|id| id == option_id)
            .unwrap_or(false)
    });

    let outcome = match chosen.and_then(|opt| opt.get("optionId").and_then(|o| o.as_str())) {
        Some(id) => serde_json::json!({"outcome": "selected", "optionId": id}),
        None => serde_json::json!({"outcome": "cancelled"}),
    };

    let backend_request_id = event.raw_request.get("id").cloned().unwrap_or(Value::Null);
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": backend_request_id,
        "result": { "outcome": outcome }
    })
}

/// Legacy approve/reject helper: picks the first matching kind (same
/// fallback policy as acpx AutoAllow / AutoReject). Prefer
/// [`build_response_for_option`] when the UI has a concrete option id.
pub fn build_response(event: &AgentRequestEvent, approved: bool) -> Value {
    let options = extract_options(event);
    let option_id = if approved {
        default_allow_option_id(&options)
    } else {
        default_reject_option_id(&options)
    };
    match option_id {
        Some(id) => build_response_for_option(event, id),
        None if !approved && event.method == "session/request_permission" => {
            // No reject option offered — cancel rather than guess.
            let backend_request_id = event.raw_request.get("id").cloned().unwrap_or(Value::Null);
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": backend_request_id,
                "result": { "outcome": { "outcome": "cancelled" } }
            })
        }
        None => build_response_for_option(event, if approved { "approve" } else { "reject" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn permission_request_event() -> AgentRequestEvent {
        AgentRequestEvent {
            relay_id: "relay-1".to_string(),
            method: "session/request_permission".to_string(),
            raw_request: serde_json::json!({
                "jsonrpc": "2.0", "id": 42, "method": "session/request_permission",
                "params": {
                    "sessionId": "s1",
                    "toolCall": {"toolCallId": "call-1", "title": "Run rm -rf"},
                    "options": [
                        {"optionId": "allow-once", "name": "Allow once", "kind": "allow_once"},
                        {"optionId": "allow-always", "name": "Allow always", "kind": "allow_always"},
                        {"optionId": "reject-once", "name": "Reject", "kind": "reject_once"}
                    ]
                }
            }),
        }
    }

    #[test]
    fn permission_view_summarizes_the_tool_call_title() {
        let view = to_pending_request_view(&permission_request_event());
        assert!(view.summary.contains("Run rm -rf"));
        assert_eq!(view.relay_id, "relay-1");
        assert_eq!(view.options.len(), 3);
        assert_eq!(view.options[0].option_id, "allow-once");
        assert!(view.options[0].is_allow);
        assert!(!view.options[2].is_allow);
    }

    #[test]
    fn one_of_select_uses_exact_option_id() {
        let response = build_response_for_option(&permission_request_event(), "allow-always");
        assert_eq!(
            response["result"]["outcome"],
            serde_json::json!({"outcome": "selected", "optionId": "allow-always"})
        );
        assert_eq!(response["id"], serde_json::json!(42));
    }

    #[test]
    fn permission_approve_selects_the_allow_kinded_option() {
        let response = build_response(&permission_request_event(), true);
        assert_eq!(
            response["result"]["outcome"],
            serde_json::json!({"outcome": "selected", "optionId": "allow-once"})
        );
        assert_eq!(response["id"], serde_json::json!(42));
        assert_eq!(response["jsonrpc"], serde_json::json!("2.0"));
    }

    #[test]
    fn permission_reject_selects_the_reject_kinded_option() {
        let response = build_response(&permission_request_event(), false);
        assert_eq!(
            response["result"]["outcome"],
            serde_json::json!({"outcome": "selected", "optionId": "reject-once"})
        );
    }

    #[test]
    fn permission_reject_with_no_reject_option_cancels_instead_of_guessing() {
        let mut event = permission_request_event();
        event.raw_request["params"]["options"] = serde_json::json!([
            {"optionId": "allow-once", "name": "Allow", "kind": "allow_once"}
        ]);
        let response = build_response(&event, false);
        assert_eq!(
            response["result"]["outcome"],
            serde_json::json!({"outcome": "cancelled"})
        );
    }

    #[test]
    fn fs_read_approve_sends_the_lightweight_approval_envelope() {
        let event = AgentRequestEvent {
            relay_id: "relay-2".to_string(),
            method: "fs/read_text_file".to_string(),
            raw_request: serde_json::json!({
                "jsonrpc": "2.0", "id": 7, "method": "fs/read_text_file",
                "params": {"sessionId": "s1", "path": "/tmp/secret.txt"}
            }),
        };
        let view = to_pending_request_view(&event);
        assert!(view.summary.contains("/tmp/secret.txt"));
        assert_eq!(view.options.len(), 2);
        assert_eq!(
            build_response(&event, true),
            serde_json::json!({"approved": true})
        );
        assert_eq!(
            build_response(&event, false),
            serde_json::json!({"approved": false})
        );
        assert_eq!(
            build_response_for_option(&event, "approve"),
            serde_json::json!({"approved": true})
        );
    }

    #[test]
    fn terminal_create_summarizes_command_and_args() {
        let event = AgentRequestEvent {
            relay_id: "relay-3".to_string(),
            method: "terminal/create".to_string(),
            raw_request: serde_json::json!({
                "jsonrpc": "2.0", "id": 9, "method": "terminal/create",
                "params": {"sessionId": "s1", "command": "sh", "args": ["-c", "rm -rf /"]}
            }),
        };
        let view = to_pending_request_view(&event);
        assert!(view.summary.contains("sh"));
        assert!(view.summary.contains("rm -rf /"));
    }

    #[test]
    fn unsupported_method_still_renders_a_visible_card() {
        let event = AgentRequestEvent {
            relay_id: "relay-4".to_string(),
            method: "some/future_method".to_string(),
            raw_request: serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "some/future_method"}),
        };
        assert!(!is_supported_method(&event.method));
        let view = to_pending_request_view(&event);
        assert!(view.title.contains("some/future_method"));
        assert!(view.options.is_empty());
    }
}
