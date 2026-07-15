//! Phase 1 coverage: `SessionClient` against an in-process mock ACP peer,
//! per `chat-panel-acp-rust-sdk.md`'s phased plan step 1 -- "unit-tested
//! against a mock/stub ACP peer (in-process, not even a real subprocess
//! yet)". Uses `agent_client_protocol::Channel::duplex()` as the
//! transport: no subprocess, no stdio, just two in-memory ends of one
//! JSON-RPC connection.

use agent_client_protocol::schema::v1::{
    AgentCapabilities, ContentBlock, ContentChunk, InitializeResponse, ListSessionsResponse,
    LoadSessionResponse, NewSessionResponse, PromptResponse, SessionId, SessionInfo,
    SessionNotification, SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{Agent, Channel, ConnectionTo, Dispatch};
use rui_acp_client::{AgentEvent, SessionClient, ThreadId};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Minimal in-process mock agent: initialize succeeds, new-session mints an
/// id, prompt echoes the text back uppercased via one notification then
/// completes the turn. Deliberately simpler than `src/bin/mock_agent.rs`
/// (no list/load coverage here -- that's this file's second test).
async fn spawn_mock_agent(transport: Channel) {
    tokio::spawn(async move {
        let next_id = AtomicU64::new(1);
        let sessions: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let _ = Agent
            .builder()
            .on_receive_request(
                async move |req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        InitializeResponse::new(req.protocol_version)
                            .agent_capabilities(AgentCapabilities::new()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    let id = format!("test-session-{}", next_id.fetch_add(1, Ordering::SeqCst));
                    sessions.lock().unwrap().push(id.clone());
                    responder.respond(NewSessionResponse::new(SessionId::new(id)))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: agent_client_protocol::schema::v1::PromptRequest,
                            responder,
                            cx: ConnectionTo<agent_client_protocol::Client>| {
                    let text = req
                        .prompt
                        .into_iter()
                        .find_map(|b| match b {
                            ContentBlock::Text(t) => Some(t.text),
                            _ => None,
                        })
                        .unwrap_or_default();
                    cx.send_notification(SessionNotification::new(
                        req.session_id.clone(),
                        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                            TextContent::new(text.to_uppercase()),
                        ))),
                    ))?;
                    responder.respond(PromptResponse::new(StopReason::EndTurn))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_dispatch(
                async move |message: Dispatch, cx: ConnectionTo<agent_client_protocol::Client>| {
                    message.respond_with_error(
                        agent_client_protocol::util::internal_error("unhandled in mock"),
                        cx,
                    )
                },
                agent_client_protocol::on_receive_dispatch!(),
            )
            .connect_to(transport)
            .await;
    });
}

#[tokio::test]
async fn open_session_and_prompt_roundtrip() {
    let (client_end, agent_end) = Channel::duplex();
    spawn_mock_agent(agent_end).await;

    let mut client = SessionClient::new();
    let thread = client.bind_thread(ThreadId("t1".into()), client_end);

    let session_id = thread.open_session("/tmp").await.expect("open_session");
    assert!(session_id.starts_with("test-session-"));

    thread.send_prompt("hello world").await.expect("send_prompt");

    // Drain events until TurnEnded, collecting messages along the way.
    let mut messages = Vec::new();
    loop {
        match thread.events.recv().await.expect("event channel open") {
            AgentEvent::Message(m) => messages.push(m),
            AgentEvent::TurnEnded(reason) => {
                assert_eq!(reason, "end_turn");
                break;
            }
            AgentEvent::Error(e) => panic!("unexpected error event: {e}"),
            AgentEvent::PermissionRequest(req) => panic!(
                "unexpected PermissionRequest event on the direct-ACP path \
                 (only rui-acpx-client's actor ever emits this): {req:?}"
            ),
        }
    }

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text, "HELLO WORLD");
}

/// Second mock agent variant, exercising `list_sessions`/`load_session` --
/// the two calls Decision 2's resync sequence actually depends on.
async fn spawn_mock_agent_with_list_load(transport: Channel) {
    tokio::spawn(async move {
        let _ = Agent
            .builder()
            .on_receive_request(
                async move |req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        InitializeResponse::new(req.protocol_version)
                            .agent_capabilities(AgentCapabilities::new()),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::ListSessionsRequest,
                            responder,
                            _cx| {
                    let info = SessionInfo::new(SessionId::new("s-known"), "/tmp")
                        .title(Some("Known thread".to_string()))
                        .updated_at(Some("t42".to_string()));
                    responder.respond(ListSessionsResponse::new(vec![info]))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: agent_client_protocol::schema::v1::LoadSessionRequest,
                            responder,
                            cx: ConnectionTo<agent_client_protocol::Client>| {
                    cx.send_notification(SessionNotification::new(
                        req.session_id.clone(),
                        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                            TextContent::new("replayed"),
                        ))),
                    ))?;
                    responder.respond(LoadSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_dispatch(
                async move |message: Dispatch, cx: ConnectionTo<agent_client_protocol::Client>| {
                    message.respond_with_error(
                        agent_client_protocol::util::internal_error("unhandled in mock"),
                        cx,
                    )
                },
                agent_client_protocol::on_receive_dispatch!(),
            )
            .connect_to(transport)
            .await;
    });
}

#[tokio::test]
async fn list_sessions_and_load_session_resync() {
    let (client_end, agent_end) = Channel::duplex();
    spawn_mock_agent_with_list_load(agent_end).await;

    let mut client = SessionClient::new();
    let thread = client.bind_thread(ThreadId("t2".into()), client_end);

    // Have to initialize first -- open_session isn't required for
    // list/load, but Initialize is a protocol precondition the actor
    // performs automatically on connect, before any command is processed.
    let sessions = thread.list_sessions().await.expect("list_sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].acp_session_id, "s-known");
    assert_eq!(sessions[0].title.as_deref(), Some("Known thread"));
    assert_eq!(sessions[0].updated_at.as_deref(), Some("t42"));

    // Per Decision 2: this is the "diff detected -> resync" branch.
    assert!(rui_acp_client::JsonlStore::is_stale(
        None,
        &sessions[0].title,
        &sessions[0].updated_at
    ));

    thread
        .load_session("s-known", "/tmp")
        .await
        .expect("load_session");

    match thread.events.recv().await.expect("event channel open") {
        AgentEvent::Message(m) => assert_eq!(m.text, "replayed"),
        other => panic!("expected replayed message, got {other:?}"),
    }
}
