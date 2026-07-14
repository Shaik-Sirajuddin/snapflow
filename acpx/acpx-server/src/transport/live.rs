//! **ACP compatibility phase 14.** Shared helper used by both the stdio
//! (`transport/stdio.rs`) and WebSocket (`transport/ws.rs`) transports --
//! the two persistent, full-duplex transports in this workspace, as
//! opposed to `POST /rpc` (`transport/http.rs`), which is stateless
//! request/response with no live push channel available at all and is
//! deliberately left out of this mechanism entirely (see `acpx_core::
//! notify`'s module doc comment).
//!
//! Each of the two transports owns its own connection loop and its own
//! way of writing a frame back out (stdout vs. a WS sink), so this module
//! only factors out the one piece of logic both loops need identically:
//! deciding, from a request/response pair that already went through
//! `dispatch_shared`, which gateway session id (if any) that connection
//! should now be watching for live updates, and which one it should stop
//! watching.

use serde_json::Value;

/// Which gateway session id (if any) `request`/`response` -- a JSON-RPC
/// pair that already went through `dispatch_shared` -- makes this
/// connection newly interested in subscribing to, via `acpx_core::notify::
/// NotificationHub::subscribe`.
///
/// - `session/new`: the client doesn't know the gateway session id until
///   *this* response -- `dispatch_session_new_shared` mints it and writes
///   it into `result.sessionId` before this function ever sees it.
/// - Every other `Proxied` method that carries a `params.sessionId`
///   (`session/prompt`, `session/resume`, `session/load`, `session/set_
///   mode`, `session/set_config_option`, `session/close`, `session/
///   delete`) -- the client already supplied the gateway session id
///   itself in the *request*. Subscribing here too (not just on `session/
///   new`) matters for a connection that resumes/loads a session it
///   didn't itself create in this same process lifetime, or simply never
///   issued `session/prompt` until its second call.
///
/// Returns `None` on a JSON-RPC error response (nothing to subscribe to)
/// or for any method with no session id in play at all (`initialize`,
/// `agents/list`, an unqualified `session/list`, ...).
pub fn session_id_to_watch(request: &Value, response: &Value, method: &str) -> Option<String> {
    if response.get("error").is_some() {
        return None;
    }
    if method == "session/new" {
        return response
            .get("result")
            .and_then(|r| r.get("sessionId"))
            .and_then(|s| s.as_str())
            .map(str::to_string);
    }
    request
        .get("params")
        .and_then(|p| p.get("sessionId"))
        .and_then(|s| s.as_str())
        .map(str::to_string)
}

/// The mirror image of [`session_id_to_watch`]: a *successful* `session/
/// close` or `session/delete` response means that gateway session id is
/// gone for good (see `dispatch_proxied_shared`'s `session/close`
/// bookkeeping, which evicts it from `SessionRegistry` on exactly this
/// same condition) -- no further backend notification will ever legitimately
/// arrive for it, so a transport should stop watching it and let
/// `NotificationHub` drop the now-pointless channel rather than leaking it
/// for the rest of the connection's lifetime.
pub fn session_id_to_forget(request: &Value, response: &Value, method: &str) -> Option<String> {
    if response.get("error").is_some() {
        return None;
    }
    if !matches!(method, "session/close" | "session/delete") {
        return None;
    }
    request
        .get("params")
        .and_then(|p| p.get("sessionId"))
        .and_then(|s| s.as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_new_watches_the_minted_gateway_id_from_the_response() {
        let request =
            json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {"cwd": "/tmp"}});
        let response = json!({"jsonrpc": "2.0", "id": 1, "result": {"sessionId": "gw-1"}});
        assert_eq!(
            session_id_to_watch(&request, &response, "session/new"),
            Some("gw-1".to_string())
        );
    }

    #[test]
    fn session_prompt_watches_the_session_id_the_client_already_supplied() {
        let request = json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
            "params": {"sessionId": "gw-1", "prompt": []}
        });
        let response = json!({"jsonrpc": "2.0", "id": 2, "result": {"stopReason": "end_turn"}});
        assert_eq!(
            session_id_to_watch(&request, &response, "session/prompt"),
            Some("gw-1".to_string())
        );
    }

    #[test]
    fn an_error_response_never_yields_a_session_to_watch() {
        let request = json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
            "params": {"sessionId": "gw-1", "prompt": []}
        });
        let response =
            json!({"jsonrpc": "2.0", "id": 2, "error": {"code": -32001, "message": "boom"}});
        assert_eq!(
            session_id_to_watch(&request, &response, "session/prompt"),
            None
        );
    }

    #[test]
    fn methods_with_no_session_in_play_yield_nothing() {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "agents/list", "params": {}});
        let response = json!({"jsonrpc": "2.0", "id": 1, "result": {"agents": []}});
        assert_eq!(
            session_id_to_watch(&request, &response, "agents/list"),
            None
        );
    }

    #[test]
    fn session_close_is_forgotten_on_success_but_watched_by_nothing_else() {
        let request = json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/close",
            "params": {"sessionId": "gw-1"}
        });
        let response = json!({"jsonrpc": "2.0", "id": 3, "result": {}});
        assert_eq!(
            session_id_to_forget(&request, &response, "session/close"),
            Some("gw-1".to_string())
        );
        assert_eq!(
            session_id_to_watch(&request, &response, "session/close"),
            Some("gw-1".to_string())
        );
    }

    #[test]
    fn a_failed_session_close_is_not_forgotten() {
        let request = json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/close",
            "params": {"sessionId": "gw-1"}
        });
        let response = json!({"jsonrpc": "2.0", "id": 3, "error": {"code": -32602, "message": "unknown session"}});
        assert_eq!(
            session_id_to_forget(&request, &response, "session/close"),
            None
        );
    }
}
