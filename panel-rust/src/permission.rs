//! Interactive agent-request (permission/fs/terminal-approval) UI
//! projection and response-building -- kept apart from `agent_bridge.rs`
//! (which only forwards/queues the raw event) and `lib.rs` (FFI/event
//! wiring glue), per this crate's established modularity convention
//! (see `models.rs`'s doc comment).
//!
//! v1 scope: a single approve/reject decision per request, not a full
//! N-option picker. For `session/request_permission` (which offers a
//! real ACP `options: Vec<PermissionOption>` list), "Approve" picks the
//! first `allow_once`/`allow_always`-kinded option (matching acpx's own
//! `build_permission_reply`'s `AutoAllow` fallback-to-first-option
//! behavior when no exact-kind match exists) and "Reject" picks the
//! first `reject_once`/`reject_always`-kinded option, falling back to
//! `{"outcome": "cancelled"}` if the backend offered no reject-kinded
//! option at all (mirrors `AutoReject`'s own fallback). For
//! `fs/read_text_file`/`fs/write_text_file`/`terminal/create`, both
//! buttons send the lightweight `{"approved": bool}` decision envelope
//! `acpx_core::router::try_relay_approval` expects (see that function's
//! doc comment) -- the real disk/process I/O always happens gateway-side
//! either way.

use crate::protocol_types::AgentRequestEvent;
use serde_json::Value;

/// What a request card actually shows: a short title (method-derived),
/// a human-readable one-line summary of the specific action being
/// requested, and whether this method is even answerable by this v1 UI
/// (unknown/future agent-initiated methods still render, so the request
/// is never silently invisible, but only offer a "Dismiss" affordance
/// -- see [`is_supported_method`]).
#[derive(Debug, Clone, PartialEq)]
pub struct PendingRequestView {
    pub relay_id: String,
    pub method: String,
    pub title: String,
    pub summary: String,
}

/// Whether this v1 UI knows how to build a real approve/reject response
/// for `method` -- see [`build_response`]. An unrecognized method still
/// gets a visible card (never silently dropped, per the Coverage
/// Matrix's "Unknown ACP update/request shapes... shown as safe generic
/// notices" contract), just without working action buttons.
pub fn is_supported_method(method: &str) -> bool {
    matches!(
        method,
        "session/request_permission"
            | "fs/read_text_file"
            | "fs/write_text_file"
            | "terminal/create"
    )
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
                format!("The agent wants to run: {command} {args}").trim_end().to_string(),
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
    }
}

/// Builds the response payload [`rui_acpx_client::AcpxThreadHandle::
/// respond_agent_request`] expects for `event`'s method, given the
/// user's `approved` decision -- see this module's doc comment for the
/// per-method shape contract.
pub fn build_response(event: &AgentRequestEvent, approved: bool) -> Value {
    if event.method != "session/request_permission" {
        return serde_json::json!({ "approved": approved });
    }
    let options: Vec<Value> = event
        .raw_request
        .get("params")
        .and_then(|p| p.get("options"))
        .and_then(|o| o.as_array())
        .cloned()
        .unwrap_or_default();
    let kind_prefix = if approved { "allow_" } else { "reject_" };
    let chosen = options
        .iter()
        .find(|opt| {
            opt.get("kind")
                .and_then(|k| k.as_str())
                .map(|k| k.starts_with(kind_prefix))
                .unwrap_or(false)
        })
        .or_else(|| if approved { options.first() } else { None });
    let outcome = match chosen.and_then(|opt| opt.get("optionId").and_then(|o| o.as_str())) {
        Some(option_id) => serde_json::json!({"outcome": "selected", "optionId": option_id}),
        None => serde_json::json!({"outcome": "cancelled"}),
    };
    // Unlike the `fs/*`/`terminal/create` decision envelope above,
    // `session/request_permission`'s relayed response is written
    // *verbatim* to the backend's own stdin as its reply frame (see
    // `acpx_core::router::try_relay_agent_request`'s doc comment: the
    // hub forwards exactly what the client sent, no server-side
    // wrapping) -- so this must be a complete JSON-RPC response
    // envelope with the backend's own request `id` echoed back, not
    // just the ACP `RequestPermissionResponse` payload alone. A
    // malformed/missing `id` on `raw_request` (shouldn't happen for a
    // real relayed request, but not fatal) falls back to `null`, which
    // the backend would reasonably treat as "reply I can't correlate"
    // rather than this call panicking.
    let backend_request_id = event
        .raw_request
        .get("id")
        .cloned()
        .unwrap_or(Value::Null);
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": backend_request_id,
        "result": { "outcome": outcome }
    })
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
                        {"optionId": "allow-once", "name": "Allow", "kind": "allow_once"},
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
        assert_eq!(build_response(&event, true), serde_json::json!({"approved": true}));
        assert_eq!(build_response(&event, false), serde_json::json!({"approved": false}));
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
    }
}
