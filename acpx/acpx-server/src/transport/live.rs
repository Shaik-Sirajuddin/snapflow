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

use acpx_core::ResumeCursor;
use serde_json::Value;

/// Extract ACPX's additive reconnect cursor before a request is proxied to
/// an ACP backend. `_acpx.resume` belongs to ACPX's persistent transport,
/// not the upstream ACP method's schema, so forwarding it would make strict
/// backends reject an otherwise valid `session/load`/`session/resume`.
///
/// The extension is deliberately accepted only with a non-negative integer
/// cursor. Invalid cursors are removed and treated as fresh subscriptions;
/// this preserves backend compatibility while never inventing replay state
/// from malformed client input.
pub fn take_resume_cursor(request: &mut Value) -> Option<ResumeCursor> {
    let params = request.get_mut("params")?.as_object_mut()?;
    let extension = params.get_mut("_acpx")?.as_object_mut()?;
    let cursor = extension.get("resume").and_then(|resume| {
        Some(ResumeCursor {
            last_seq: resume.get("lastSeq")?.as_u64()?,
            epoch: resume.get("epoch")?.as_str()?.to_string(),
        })
    });
    extension.remove("resume");
    if extension.is_empty() {
        params.remove("_acpx");
    }
    cursor
}

/// Which gateway session id (if any) `request`/`response` -- a JSON-RPC
/// pair that already went through `dispatch_shared` -- makes this
/// connection newly interested in subscribing to, via `acpx_core::notify::
/// NotificationHub::subscribe`.
///
/// - `session/new`: the client doesn't know the gateway session id until
///   *this* response -- `dispatch_session_new_shared` mints it and writes
///   it into `result.sessionId` before this function ever sees it.
/// - `session/fork`: the same "client doesn't know it yet" situation as
///   `session/new` -- `dispatch_session_fork_shared` mints a *brand new*
///   gateway session id for the forked session (distinct from the
///   source session named in `request.params.sessionId`) and writes it
///   into `result.sessionId`, exactly like `session/new` does. Falling
///   through to the generic `request.params.sessionId` branch below for
///   this method would incorrectly re-subscribe to the *source*
///   session's id instead -- and since a client's very next call after a
///   fork is typically `session/prompt` against the *forked* id (e.g.
///   `ACPAgent.ask_agent`'s fork-then-prompt sequence), that forked
///   session's live `session/update` notifications would have no
///   subscriber in place for the whole duration of that first prompt
///   call, silently falling back to the buffered `_acpx.updates`
///   fallback a standard ACP client never reads. Found via a real
///   OpenHands `ask_agent` (fork + prompt) call returning an empty
///   response despite the backend replying normally.
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
    if method == "session/new" || method == "session/fork" {
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
    fn session_fork_watches_the_newly_minted_forked_gateway_id_not_the_source_one() {
        // The request names the *source* session; the response mints a
        // brand new gateway id for the forked session -- the transport
        // must watch the latter, exactly like `session/new`, not fall
        // through to the generic `request.params.sessionId` branch (which
        // would incorrectly watch the source session again).
        let request = json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/fork",
            "params": {"sessionId": "gw-source", "cwd": "/tmp"}
        });
        let response = json!({"jsonrpc": "2.0", "id": 1, "result": {"sessionId": "gw-forked"}});
        assert_eq!(
            session_id_to_watch(&request, &response, "session/fork"),
            Some("gw-forked".to_string())
        );
    }

    #[test]
    fn a_failed_session_fork_yields_nothing_to_watch() {
        let request = json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/fork",
            "params": {"sessionId": "gw-source", "cwd": "/tmp"}
        });
        let response =
            json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32001, "message": "boom"}});
        assert_eq!(
            session_id_to_watch(&request, &response, "session/fork"),
            None
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

    #[test]
    fn resume_cursor_is_stripped_without_removing_other_acpx_extensions() {
        let mut request = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "session/load",
            "params": {
                "sessionId": "gw-1",
                "_acpx": {
                    "profile": "retained",
                    "resume": {"lastSeq": 42, "epoch": "epoch-1"}
                }
            }
        });
        assert_eq!(
            take_resume_cursor(&mut request),
            Some(ResumeCursor {
                last_seq: 42,
                epoch: "epoch-1".to_string()
            })
        );
        assert_eq!(request["params"]["_acpx"]["profile"], json!("retained"));
        assert!(request["params"]["_acpx"].get("resume").is_none());
    }

    #[test]
    fn malformed_resume_cursor_is_removed_and_ignored() {
        let mut request = json!({
            "params": {"_acpx": {"resume": {"lastSeq": "not-a-number"}}}
        });
        assert_eq!(take_resume_cursor(&mut request), None);
        assert!(request["params"].get("_acpx").is_none());
    }
}
