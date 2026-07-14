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
    AgentCapabilities, ContentBlock, ContentChunk, InitializeResponse, ListSessionsResponse,
    LoadSessionResponse, NewSessionResponse, PromptResponse, SessionId, SessionInfo,
    SessionNotification, SessionUpdate, StopReason, TextContent, ToolCall, ToolCallId,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Dispatch, Result, Stdio};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

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

struct SessionState {
    title: String,
    updated_at: String,
    turn_count: u64,
}

static SESSIONS: Mutex<Option<HashMap<String, SessionState>>> = Mutex::new(None);

fn with_sessions<T>(f: impl FnOnce(&mut HashMap<String, SessionState>) -> T) -> T {
    let mut guard = SESSIONS.lock().expect("mock-agent session map poisoned");
    let map = guard.get_or_insert_with(HashMap::new);
    f(map)
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
    prompt_text: &str,
) -> Result<()> {
    connection.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Text(
            TextContent::new(format!("considering: {prompt_text}")),
        ))),
    ))?;
    connection.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::ToolCall(ToolCall::new(
            ToolCallId::new("mock-tool-1"),
            format!("mock_tool(input={prompt_text})"),
        )),
    ))?;
    connection.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
            TextContent::new(format!("{}{}", persona_prefix(), prompt_text.to_uppercase())),
        ))),
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
                        },
                    );
                });
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
                send_replay(&connection, &session_id, &text).await?;
                with_sessions(|sessions| {
                    if let Some(s) = sessions.get_mut(session_id.0.as_ref()) {
                        s.turn_count += 1;
                        s.title = format!("Turn {}: {}", s.turn_count, text);
                        s.updated_at = now_iso();
                    }
                });
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
                let known = with_sessions(|sessions| sessions.contains_key(request.session_id.0.as_ref()));
                if !known {
                    return responder.respond_with_error(agent_client_protocol::util::internal_error(
                        "unknown session id",
                    ));
                }
                send_replay(&connection, &request.session_id, "replayed history").await?;
                responder.respond(LoadSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
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
