//! `rui-mock-agent`: a minimal, real ACP-compliant agent process, spoken
//! to over stdio. Exists purely as a test double for `rui-acp-client`'s
//! phase-2 e2e coverage ("spawn a real backend agent over stdio... any
//! ACP-compliant agent is fine for dev/validation, no specific agent
//! required by this plan" -- `chat-panel-acp-rust-sdk.md`).
//!
//! Behavior, deliberately simple and deterministic (no LLM call):
//! - `initialize`: always succeeds.
//! - `session/new`: mints a session id from an incrementing counter, remembers it.
//! - `session/prompt`: streams one `agent_thought_chunk`, one `tool_call`,
//!   then one `agent_message_chunk` echoing the prompt text back
//!   uppercased (so tests can assert on a known transformation instead of
//!   just "some string arrived"), then responds `StopReason::EndTurn`.
//! - `session/list`: returns every session created so far, with a
//!   `title`/`updated_at` that changes each time a prompt completes on it
//!   (so cache-staleness tests have something real to diff against).
//! - `session/load`: replays the same three notifications as a fresh
//!   prompt turn would have produced (a stand-in for "replay history").

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CloseSessionRequest, CloseSessionResponse, ContentBlock, ContentChunk,
    CancelNotification, DeleteSessionRequest, DeleteSessionResponse, InitializeResponse,
    ListSessionsResponse, LoadSessionResponse, NewSessionResponse, PromptResponse,
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    ResumeSessionRequest, ResumeSessionResponse, SessionId, SessionInfo,
    SessionNotification, SessionUpdate, StopReason, TextContent, ToolCall, ToolCallId,
    ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Dispatch, Result, Stdio};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::Notify;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// **acpx-gateway-integration addition.** When this process is spawned as
/// one provider's backend behind an `acpx-server` (see
/// `panel-rust/src/agent_bridge.rs`'s `resolve_gateway` /
/// `ensure_gateway_running`), `RUI_MOCK_AGENT_PERSONA` names which
/// provider it's standing in for (`"codex"`/`"claude"`). Prefixing every
/// reply with `[<PERSONA>]` is the concrete, checkable signal the
/// multi-provider isolation tests assert on: if two threads bound to two
/// different gateway processes ever got cross-wired, the wrong persona
/// tag would show up in a thread's transcript and the test would fail
/// instead of passing unnoticed. Unset (the direct, non-gateway dev path)
/// leaves replies byte-for-byte unchanged from before this existed.
fn persona_prefix() -> String {
    match std::env::var("RUI_MOCK_AGENT_PERSONA") {
        Ok(p) if !p.is_empty() => format!("[{}] ", p.to_uppercase()),
        _ => String::new(),
    }
}

/// Appends a machine-readable record when the real-process test harness asks
/// for backend evidence. This is intentionally optional so normal mock-agent
/// behavior stays unchanged outside an E2E run.
fn record_gateway_event(method: &str, session_id: Option<&str>, detail: &str) {
    let Ok(path) = std::env::var("RUI_MOCK_AGENT_EVENT_LOG") else {
        return;
    };
    let record = serde_json::json!({
        "method": method,
        "session_id": session_id,
        "detail": detail,
        "persona": std::env::var("RUI_MOCK_AGENT_PERSONA").unwrap_or_default(),
    });
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = writeln!(file, "{record}");
}

struct SessionState {
    title: String,
    updated_at: String,
    turn_count: u64,
    replay_turns: Vec<ReplayTurn>,
}

#[derive(Clone)]
struct ReplayTurn {
    prompt_text: String,
}

static SESSIONS: Mutex<Option<HashMap<String, SessionState>>> = Mutex::new(None);

fn with_sessions<T>(f: impl FnOnce(&mut HashMap<String, SessionState>) -> T) -> T {
    let mut guard = SESSIONS.lock().expect("mock-agent session map poisoned");
    let map = guard.get_or_insert_with(HashMap::new);
    f(map)
}

/// Coverage-matrix `session/cancel` host-scenario support: a prompt whose
/// text starts with `slow ` never resolves on its own -- it blocks
/// (up to a generous safety-net timeout, so a real bug in the cancel path
/// fails the test instead of hanging the harness forever) until this
/// session's real ACP `session/cancel` notification arrives. One `Notify`
/// per session id, created lazily so the prompt handler (which blocks
/// first) and the cancel notification handler (which fires second, from
/// an independently dispatched task) always agree on the same instance
/// regardless of arrival order.
static CANCEL_NOTIFY: Mutex<Option<HashMap<String, Arc<Notify>>>> = Mutex::new(None);

fn cancel_notify_for(session_id: &str) -> Arc<Notify> {
    let mut guard = CANCEL_NOTIFY.lock().expect("mock-agent cancel map poisoned");
    let map = guard.get_or_insert_with(HashMap::new);
    map.entry(session_id.to_string())
        .or_insert_with(|| Arc::new(Notify::new()))
        .clone()
}

fn now_iso() -> String {
    // No chrono dependency for a test double -- a monotonically increasing
    // counter formatted as a fake timestamp is sufficient to prove
    // trailer-diff staleness detection without pulling in a time crate.
    format!("t{}", NEXT_ID.fetch_add(1, Ordering::SeqCst))
}

async fn send_replay(
    connection: &ConnectionTo<Client>,
    session_id: &SessionId,
    turn: &ReplayTurn,
) -> Result<()> {
    connection.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            format!("considering: {}", turn.prompt_text),
        )))),
    ))?;
    connection.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::ToolCall(ToolCall::new(
            ToolCallId::new("mock-tool-1"),
            format!("mock_tool(input={})", turn.prompt_text),
        )),
    ))?;
    connection.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            format!("{}{}", persona_prefix(), turn.prompt_text.to_uppercase()),
        )))),
    ))?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    Agent
        .builder()
        .name("rui-mock-agent")
        .on_receive_request(
            async move |initialize: agent_client_protocol::schema::v1::InitializeRequest,
                        responder,
                        _connection| {
                responder.respond(
                    InitializeResponse::new(initialize.protocol_version)
                        .agent_capabilities(AgentCapabilities::new()),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: agent_client_protocol::schema::v1::NewSessionRequest,
                        responder,
                        _connection| {
                let id = format!("mock-session-{}", NEXT_ID.fetch_add(1, Ordering::SeqCst));
                with_sessions(|sessions| {
                    sessions.insert(
                        id.clone(),
                        SessionState {
                            title: "New session".to_string(),
                            updated_at: now_iso(),
                            turn_count: 0,
                            replay_turns: Vec::new(),
                        },
                    );
                });
                record_gateway_event("session/new", Some(&id), "");
                responder.respond(NewSessionResponse::new(SessionId::new(id)))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: agent_client_protocol::schema::v1::PromptRequest,
                        responder,
                        connection: ConnectionTo<Client>| {
                let text = request
                    .prompt
                    .into_iter()
                    .find_map(|block| match block {
                        ContentBlock::Text(t) => Some(t.text),
                        _ => None,
                    })
                    .unwrap_or_default();
                let session_id = request.session_id.clone();
                record_gateway_event("session/prompt", Some(session_id.0.as_ref()), &text);
                // Lowercase, punctuation-free marker: the real host XTEST
                // driver (`host_e2e_driver.py`) taps unshifted keysyms one
                // character at a time with no modifier-key support, so an
                // uppercase/punctuation marker (this started as `"SLOW:"`)
                // silently arrives mangled (observed: typed into a live
                // dock via raw XTEST, `"SLOW:test cancel"` arrived at the
                // backend as `"slow;test cancel"` -- lowercased letters,
                // and `:` came through as its unshifted-keycap neighbor
                // `;`) instead of failing loudly. Every other host-driven
                // prompt marker in this project is already plain lowercase
                // for the same reason.
                if let Some(marker_text) = text.strip_prefix("slow ") {
                    // Coverage-matrix `session/cancel` host scenario: block
                    // this turn until a real `session/cancel` notification
                    // arrives for this session (or a generous safety-net
                    // timeout elapses, so a real regression in the cancel
                    // path fails loudly instead of hanging the harness).
                    // Handed off to an independent task so this handler
                    // returns immediately -- the dispatch loop must stay
                    // free to read and dispatch the later `session/cancel`
                    // notification while this prompt is still "in flight".
                    let notify = cancel_notify_for(session_id.0.as_ref());
                    let marker_text = marker_text.to_string();
                    let connection_for_wait = connection.clone();
                    let session_id_for_wait = session_id.clone();
                    tokio::spawn(async move {
                        let _ = connection_for_wait.send_notification(SessionNotification::new(
                            session_id_for_wait.clone(),
                            SessionUpdate::AgentThoughtChunk(ContentChunk::new(
                                ContentBlock::Text(TextContent::new(format!(
                                    "considering (slow): {marker_text}"
                                ))),
                            )),
                        ));
                        tokio::select! {
                            _ = notify.notified() => {
                                let _ = responder
                                    .respond(PromptResponse::new(StopReason::Cancelled));
                            }
                            _ = tokio::time::sleep(Duration::from_secs(20)) => {
                                let _ = responder
                                    .respond(PromptResponse::new(StopReason::EndTurn));
                            }
                        }
                    });
                    return Ok(());
                }
                if let Some(marker_text) = text.strip_prefix("permission ") {
                    // Coverage-matrix `session/request_permission` host
                    // scenario: sends a real ACP `session/request_
                    // permission` request out to the client (the panel,
                    // via acpx-server's live relay -- `acpx-core::
                    // agent_relay`) and blocks on its real decision,
                    // exactly the human-in-the-loop shape a real backend
                    // has. Handed off to an independent task for the
                    // same reason the `slow ` marker above is: the
                    // dispatch loop must stay free to keep processing
                    // other traffic while this request is outstanding.
                    let marker_text = marker_text.to_string();
                    let session_id_for_wait = session_id.clone();
                    let connection_for_wait = connection.clone();
                    tokio::spawn(async move {
                        let outcome = connection_for_wait
                            .send_request(RequestPermissionRequest::new(
                                session_id_for_wait.clone(),
                                ToolCallUpdate::new(
                                    ToolCallId::new("mock-tool-permission"),
                                    ToolCallUpdateFields::new().title(marker_text.clone()),
                                ),
                                vec![
                                    PermissionOption::new(
                                        "allow-once",
                                        "Allow once",
                                        PermissionOptionKind::AllowOnce,
                                    ),
                                    PermissionOption::new(
                                        "reject-once",
                                        "Reject",
                                        PermissionOptionKind::RejectOnce,
                                    ),
                                ],
                            ))
                            .block_task()
                            .await;
                        // The one observable signal a host test needs:
                        // which option (if any) the real client chose,
                        // recorded the same way `session/cancel` is
                        // above -- readable from the backend event log
                        // without depending on the panel's own reducer
                        // state.
                        let chosen = match outcome {
                            Ok(response) => match response.outcome {
                                RequestPermissionOutcome::Selected(selected) => {
                                    selected.option_id.0.to_string()
                                }
                                RequestPermissionOutcome::Cancelled => "cancelled".to_string(),
                                // `RequestPermissionOutcome` is
                                // `#[non_exhaustive]` -- a future ACP
                                // schema addition here is deliberately
                                // treated the same as a hung/absent
                                // response rather than a panic.
                                _ => "no-response".to_string(),
                            },
                            Err(_) => "no-response".to_string(),
                        };
                        record_gateway_event(
                            "session/request_permission",
                            Some(session_id_for_wait.0.as_ref()),
                            &chosen,
                        );
                        let _ = connection_for_wait.send_notification(SessionNotification::new(
                            session_id_for_wait.clone(),
                            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                                TextContent::new(format!(
                                    "{}permission decision: {chosen}",
                                    persona_prefix()
                                )),
                            ))),
                        ));
                        let _ = responder.respond(PromptResponse::new(StopReason::EndTurn));
                    });
                    return Ok(());
                }
                with_sessions(|sessions| {
                    if let Some(s) = sessions.get_mut(session_id.0.as_ref()) {
                        s.turn_count += 1;
                        s.title = format!("Turn {}: {}", s.turn_count, text);
                        s.updated_at = now_iso();
                        s.replay_turns.push(ReplayTurn {
                            prompt_text: text.clone(),
                        });
                    }
                });
                let turn = ReplayTurn { prompt_text: text };
                send_replay(&connection, &session_id, &turn).await?;
                responder.respond(PromptResponse::new(StopReason::EndTurn))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: agent_client_protocol::schema::v1::ListSessionsRequest,
                        responder,
                        _connection| {
                let sessions = with_sessions(|sessions| {
                    sessions
                        .iter()
                        .map(|(id, s)| {
                            SessionInfo::new(SessionId::new(id.clone()), "/")
                                .title(Some(s.title.clone()))
                                .updated_at(Some(s.updated_at.clone()))
                        })
                        .collect::<Vec<_>>()
                });
                responder.respond(ListSessionsResponse::new(sessions))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: agent_client_protocol::schema::v1::LoadSessionRequest,
                        responder,
                        connection: ConnectionTo<Client>| {
                let known =
                    with_sessions(|sessions| sessions.contains_key(request.session_id.0.as_ref()));
                if !known {
                    return responder.respond_with_error(
                        agent_client_protocol::util::internal_error("unknown session id"),
                    );
                }
                let turns = with_sessions(|sessions| {
                    sessions
                        .get(request.session_id.0.as_ref())
                        .map(|session| session.replay_turns.clone())
                        .unwrap_or_default()
                });
                for turn in turns {
                    send_replay(&connection, &request.session_id, &turn).await?;
                }
                responder.respond(LoadSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ResumeSessionRequest, responder, _connection| {
                let known =
                    with_sessions(|sessions| sessions.contains_key(request.session_id.0.as_ref()));
                if !known {
                    return responder.respond_with_error(
                        agent_client_protocol::util::internal_error("unknown session id"),
                    );
                }
               responder.respond(ResumeSessionResponse::new())
           },
           agent_client_protocol::on_receive_request!(),
       )
        .on_receive_request(
            async move |request: CloseSessionRequest, responder, _connection| {
                // Real, stable v1 ACP `session/close` -- Coverage Matrix
                // `session/close`/`session/delete` row. Deliberately does
                // *not* remove the session from `with_sessions`: closing
                // is meant to be a reversible step recoverable via
                // `session/load`/`session/delete`, same "close evicts the
                // in-memory registry, not the durable row" semantics
                // `acpx-core::router`'s own `session/close` handling
                // relies on for its own rehydration test suite.
                record_gateway_event("session/close", Some(request.session_id.0.as_ref()), "");
                responder.respond(CloseSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: DeleteSessionRequest, responder, _connection| {
                // Real, stable v1 ACP `session/delete` -- permanently
                // removes the session (unlike close, this really does
                // erase this stand-in's own in-memory record).
                record_gateway_event("session/delete", Some(request.session_id.0.as_ref()), "");
                with_sessions(|sessions| {
                    sessions.remove(request.session_id.0.as_ref());
                });
                responder.respond(DeleteSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: CancelNotification, _connection: ConnectionTo<Client>| {
                // Real, stable v1 ACP `session/cancel` -- Coverage Matrix
                // `session/cancel` row's host-scenario support. Wakes
                // whichever `session/prompt` call blocked itself waiting on
                // this exact session id's marker (a `slow `-prefixed
                // prompt, see the prompt handler above); a no-op if no
                // prompt is currently blocked on it (matches real ACP
                // agents, which tolerate a cancel with no in-flight turn).
                record_gateway_event(
                    "session/cancel",
                    Some(notification.session_id.0.as_ref()),
                    "",
                );
                // `notify_one`, not `notify_waiters`: the latter only wakes
                // *currently registered* waiters and drops the signal on
                // the floor if the prompt task hasn't reached its own
                // `.notified()` call yet (a real, reproduced race here --
                // this cancel notification can and does arrive before the
                // spawned prompt task finishes sending its pre-block
                // thought chunk and starts waiting). `notify_one` retains
                // a single permit for exactly that case, so a cancel that
                // wins the race still unblocks the very next `.notified()`
                // call instead of being silently lost until the 20s
                // safety-net timeout.
                cancel_notify_for(notification.session_id.0.as_ref()).notify_one();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_dispatch(
            async move |message: Dispatch, cx: ConnectionTo<Client>| {
                message.respond_with_error(
                    agent_client_protocol::util::internal_error("unhandled message"),
                    cx,
                )
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_to(Stdio::new())
        .await
}
