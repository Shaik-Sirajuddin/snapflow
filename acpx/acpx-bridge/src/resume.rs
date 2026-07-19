//! Reconnect-cursor tracking for `acpx-acp-bridge`.
//!
//! `acpx-server`'s [`acpx_core::notify::NotificationHub`] already buffers
//! every `session/update` notification it publishes (bounded replay, see
//! that module's doc comment) and will replay the missed gap to a client
//! that reconnects with an `_acpx.resume` cursor -- but that cursor is an
//! ACPX-proprietary, additive `params._acpx.resume` extension
//! (`{lastSeq, epoch}`, stripped server-side by `acpx_server::transport::
//! live::take_resume_cursor`), not part of the ACP spec. A real ACP
//! client such as Zed has no notion of it and will never populate it.
//!
//! `acpx-acp-bridge` sits between that unaware ACP client (stdio) and
//! ACPX's WebSocket transport, and is therefore the one place that can
//! close this gap transparently: it already sees every `session/update`
//! frame flow past on its way to the client's stdout, each one stamped
//! with `params._acpx.{seq,epoch}` by `Envelope::into_value`. This module
//! tracks the newest `(seq, epoch)` per gateway session id from that
//! stream, and -- once the bridge's own WebSocket connection drops and
//! reconnects -- injects `_acpx.resume` into the next outgoing frame for
//! each affected session, so ACPX replays exactly the notifications the
//! client missed while the socket was down. Without this, every
//! reconnect silently resubscribes from "now", and any `session/update`
//! published during the gap (routinely the first part of whatever the
//! backend agent was mid-way through streaming) is gone for good.

use serde_json::Value;
use std::collections::HashMap;

/// Newest replay cursor observed for one gateway session, plus whether
/// the bridge's WebSocket connection has reconnected since it was last
/// used (i.e. whether the next outgoing frame for this session should
/// carry it).
#[derive(Debug, Clone)]
struct TrackedSession {
    last_seq: u64,
    epoch: String,
    needs_resync: bool,
}

/// Per-bridge-process cursor tracker. One instance lives for the whole
/// life of one `acpx-acp-bridge` process and survives every WebSocket
/// reconnect within it (only stdin EOF -- the parent editor actually
/// killing this child -- ends the process; see `main`'s doc comment).
#[derive(Debug, Default)]
pub struct ResumeTracker {
    sessions: HashMap<String, TrackedSession>,
    /// Outstanding `session/close`/`session/delete` request ids this
    /// tracker is waiting on a response for, so a *successful* one can
    /// forget the session (mirrors `acpx_server::transport::live::
    /// session_id_to_forget`'s request/response pairing -- the bridge
    /// only ever sees these as two independent frames, never a matched
    /// pair, so it must do its own tiny bit of id correlation here).
    pending_forget: HashMap<String, String>,
}

impl ResumeTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of sessions this tracker currently holds a cursor for.
    /// Diagnostic only (used in `main`'s reconnect log line).
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Call once right after every WebSocket connection *after* the
    /// first succeeds (never on the very first connect of the process --
    /// there is nothing to resume yet). Marks every currently tracked
    /// session as needing its cursor re-sent on its next outgoing frame.
    pub fn mark_all_for_resync(&mut self) {
        for session in self.sessions.values_mut() {
            session.needs_resync = true;
        }
    }

    /// Inspect one client -> server frame before it is forwarded. Only
    /// records outstanding `session/close`/`session/delete` calls (by
    /// request id) for [`Self::observe_incoming`] to resolve later;
    /// never mutates `frame` -- that is [`Self::prepare_outgoing`]'s job,
    /// kept separate so the two concerns (bookkeeping vs. mutation) stay
    /// independently testable.
    pub fn observe_outgoing(&mut self, frame: &Value) {
        let method = frame.get("method").and_then(Value::as_str);
        if !matches!(method, Some("session/close") | Some("session/delete")) {
            return;
        }
        let (Some(id), Some(session_id)) = (
            request_id_key(frame),
            frame
                .pointer("/params/sessionId")
                .and_then(Value::as_str)
                .map(str::to_string),
        ) else {
            return;
        };
        self.pending_forget.insert(id, session_id);
    }

    /// If this session currently needs a resync (set by
    /// [`Self::mark_all_for_resync`] and not yet consumed), inject
    /// `_acpx.resume` into `frame` -- preserving any other keys already
    /// under `params._acpx` (e.g. the `bg` background-mode override) --
    /// and clear the flag so a second outgoing frame for the same
    /// session in the same connection generation is left untouched.
    /// Returns whether `frame` was mutated, so callers can avoid
    /// needlessly re-serializing (and thus reordering) every untouched
    /// frame.
    pub fn prepare_outgoing(&mut self, frame: &mut Value) -> bool {
        let Some(session_id) = frame
            .pointer("/params/sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return false;
        };
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return false;
        };
        if !session.needs_resync {
            return false;
        }
        let Some(params) = frame.get_mut("params").and_then(Value::as_object_mut) else {
            return false;
        };
        let extension = params
            .entry("_acpx")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        let Some(extension) = extension.as_object_mut() else {
            return false;
        };
        extension.insert(
            "resume".to_string(),
            serde_json::json!({
                "lastSeq": session.last_seq,
                "epoch": session.epoch,
            }),
        );
        session.needs_resync = false;
        true
    }

    /// Inspect one server -> client frame after it is decoded, before it
    /// is written to stdout. Advances the tracked cursor for
    /// `session/update` notifications (the only method
    /// `Envelope::into_value` stamps with `_acpx.{seq,epoch}`), and
    /// resolves any outstanding close/delete recorded by
    /// [`Self::observe_outgoing`].
    pub fn observe_incoming(&mut self, frame: &Value) {
        if frame.get("method").and_then(Value::as_str) == Some("session/update") {
            self.observe_session_update(frame);
            return;
        }
        self.observe_possible_close_response(frame);
    }

    fn observe_session_update(&mut self, frame: &Value) {
        let Some(session_id) = frame
            .pointer("/params/sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return;
        };
        let (Some(seq), Some(epoch)) = (
            frame
                .pointer("/params/_acpx/seq")
                .and_then(Value::as_u64),
            frame
                .pointer("/params/_acpx/epoch")
                .and_then(Value::as_str)
                .map(str::to_string),
        ) else {
            // No ACPX resume metadata on this frame at all -- a stand-in
            // backend in a test, or a future notification kind
            // `Envelope::into_value` doesn't stamp. Nothing to track.
            return;
        };
        self.sessions
            .entry(session_id)
            .and_modify(|session| {
                session.last_seq = seq;
                session.epoch = epoch.clone();
            })
            .or_insert(TrackedSession {
                last_seq: seq,
                epoch,
                needs_resync: false,
            });
    }

    fn observe_possible_close_response(&mut self, frame: &Value) {
        let Some(id) = request_id_key(frame) else {
            return;
        };
        let Some(session_id) = self.pending_forget.remove(&id) else {
            return;
        };
        if frame.get("error").is_some() {
            // A failed close/delete leaves the session exactly as live as
            // it was -- forgetting its cursor here would strand any
            // client that retries against the still-open session with no
            // resume state at all.
            return;
        }
        self.sessions.remove(&session_id);
    }
}

/// JSON-RPC ids are either a string or a number; normalize both to a
/// `String` so `pending_forget` can key on either shape a real backend
/// might use, without pulling in a third dependency just to hash a
/// `serde_json::Value`.
fn request_id_key(frame: &Value) -> Option<String> {
    match frame.get("id")? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_update(session_id: &str, seq: u64, epoch: &str) -> Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": {"sessionUpdate": "agent_message_chunk"},
                "_acpx": {"seq": seq, "epoch": epoch},
            }
        })
    }

    #[test]
    fn untracked_session_is_left_untouched() {
        let mut tracker = ResumeTracker::new();
        let mut frame = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/prompt",
            "params": {"sessionId": "s1", "prompt": []}
        });
        assert!(!tracker.prepare_outgoing(&mut frame));
        assert!(frame.pointer("/params/_acpx").is_none());
    }

    #[test]
    fn frame_without_a_session_id_is_left_untouched() {
        let mut tracker = ResumeTracker::new();
        tracker.observe_incoming(&session_update("s1", 3, "epoch-a"));
        tracker.mark_all_for_resync();
        let mut frame = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});
        assert!(!tracker.prepare_outgoing(&mut frame));
    }

    #[test]
    fn resync_injects_the_newest_observed_cursor() {
        let mut tracker = ResumeTracker::new();
        tracker.observe_incoming(&session_update("s1", 1, "epoch-a"));
        tracker.observe_incoming(&session_update("s1", 2, "epoch-a"));
        tracker.mark_all_for_resync();

        let mut frame = serde_json::json!({
            "jsonrpc": "2.0", "id": 5, "method": "session/prompt",
            "params": {"sessionId": "s1", "prompt": []}
        });
        assert!(tracker.prepare_outgoing(&mut frame));
        assert_eq!(frame["params"]["_acpx"]["resume"]["lastSeq"], 2);
        assert_eq!(frame["params"]["_acpx"]["resume"]["epoch"], "epoch-a");
    }

    #[test]
    fn resync_flag_clears_after_one_injection() {
        let mut tracker = ResumeTracker::new();
        tracker.observe_incoming(&session_update("s1", 1, "epoch-a"));
        tracker.mark_all_for_resync();

        let mut first = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/prompt",
            "params": {"sessionId": "s1", "prompt": []}
        });
        assert!(tracker.prepare_outgoing(&mut first));

        let mut second = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
            "params": {"sessionId": "s1", "prompt": []}
        });
        assert!(!tracker.prepare_outgoing(&mut second));
        assert!(second.pointer("/params/_acpx").is_none());
    }

    #[test]
    fn without_a_reconnect_no_resume_is_ever_injected() {
        let mut tracker = ResumeTracker::new();
        tracker.observe_incoming(&session_update("s1", 7, "epoch-a"));
        // No `mark_all_for_resync` call -- this is the common, no-drop
        // steady-state case.
        let mut frame = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/prompt",
            "params": {"sessionId": "s1", "prompt": []}
        });
        assert!(!tracker.prepare_outgoing(&mut frame));
    }

    #[test]
    fn injection_preserves_other_acpx_extension_keys() {
        let mut tracker = ResumeTracker::new();
        tracker.observe_incoming(&session_update("s1", 4, "epoch-b"));
        tracker.mark_all_for_resync();

        let mut frame = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/prompt",
            "params": {"sessionId": "s1", "prompt": [], "_acpx": {"bg": true}}
        });
        assert!(tracker.prepare_outgoing(&mut frame));
        assert_eq!(frame["params"]["_acpx"]["bg"], true);
        assert_eq!(frame["params"]["_acpx"]["resume"]["lastSeq"], 4);
    }

    #[test]
    fn a_second_session_reconnect_does_not_touch_an_unrelated_session() {
        let mut tracker = ResumeTracker::new();
        tracker.observe_incoming(&session_update("s1", 1, "epoch-a"));
        tracker.observe_incoming(&session_update("s2", 1, "epoch-a"));
        tracker.mark_all_for_resync();

        let mut frame = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/prompt",
            "params": {"sessionId": "s1", "prompt": []}
        });
        assert!(tracker.prepare_outgoing(&mut frame));

        // s2 still needs its own resync -- untouched by s1's frame.
        let mut other = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/prompt",
            "params": {"sessionId": "s2", "prompt": []}
        });
        assert!(tracker.prepare_outgoing(&mut other));
    }

    #[test]
    fn successful_close_forgets_the_session() {
        let mut tracker = ResumeTracker::new();
        tracker.observe_incoming(&session_update("s1", 1, "epoch-a"));
        tracker.observe_outgoing(&serde_json::json!({
            "jsonrpc": "2.0", "id": 9, "method": "session/close",
            "params": {"sessionId": "s1"}
        }));
        tracker.observe_incoming(&serde_json::json!({
            "jsonrpc": "2.0", "id": 9, "result": {}
        }));
        tracker.mark_all_for_resync();

        let mut frame = serde_json::json!({
            "jsonrpc": "2.0", "id": 10, "method": "session/prompt",
            "params": {"sessionId": "s1", "prompt": []}
        });
        assert!(!tracker.prepare_outgoing(&mut frame));
    }

    #[test]
    fn failed_close_keeps_the_session_tracked() {
        let mut tracker = ResumeTracker::new();
        tracker.observe_incoming(&session_update("s1", 1, "epoch-a"));
        tracker.observe_outgoing(&serde_json::json!({
            "jsonrpc": "2.0", "id": 9, "method": "session/close",
            "params": {"sessionId": "s1"}
        }));
        tracker.observe_incoming(&serde_json::json!({
            "jsonrpc": "2.0", "id": 9, "error": {"code": -1, "message": "nope"}
        }));
        tracker.mark_all_for_resync();

        let mut frame = serde_json::json!({
            "jsonrpc": "2.0", "id": 10, "method": "session/prompt",
            "params": {"sessionId": "s1", "prompt": []}
        });
        assert!(tracker.prepare_outgoing(&mut frame));
    }

    #[test]
    fn unrelated_response_ids_do_not_forget_anything() {
        let mut tracker = ResumeTracker::new();
        tracker.observe_incoming(&session_update("s1", 1, "epoch-a"));
        // A response to some other, non-close call.
        tracker.observe_incoming(&serde_json::json!({"jsonrpc": "2.0", "id": 999, "result": {}}));
        tracker.mark_all_for_resync();

        let mut frame = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/prompt",
            "params": {"sessionId": "s1", "prompt": []}
        });
        assert!(tracker.prepare_outgoing(&mut frame));
    }
}
