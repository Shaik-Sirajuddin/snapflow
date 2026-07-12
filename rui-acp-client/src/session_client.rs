//! Per-thread ACP agent connections.
//!
//! Per Decision 4 (`chat-panel-acp-rust-sdk.md`), each logical chat thread
//! is bound to exactly one agent connection for its lifetime ("per-thread
//! static binding, not `acpx`-style dynamic routing" -- see that plan's
//! scope-boundary note). This module owns that binding: one background
//! actor task per thread, talking ACP over whatever transport it was given
//! (`AcpAgent` for a real subprocess, `Channel` for an in-process mock).
//!
//! v1 simplification (documented, not hidden): a prompt turn is drained to
//! completion (`StopReason`) inside the actor before the next command is
//! processed -- matches the SDK's own `read_to_string` reference pattern.
//! Mid-turn cancellation (`session/cancel`) is not wired yet; flagged as
//! follow-up work, not silently dropped scope.

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, InitializeRequest, ListSessionsRequest, LoadSessionRequest,
    PermissionOptionId, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionInfo, SessionNotification,
    SessionUpdate, StopReason as AcpStopReason,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{
    ActiveSession, Agent, Client, ConnectTo, ConnectionTo, SessionMessage,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::sync::{mpsc, oneshot};

#[derive(thiserror::Error, Debug)]
pub enum SessionClientError {
    #[error("no active session on this thread -- call open_session first")]
    NoActiveSession,
    #[error("actor task for this thread is gone (shut down or panicked)")]
    ActorGone,
    #[error("acp error: {0}")]
    Acp(String),
}

impl From<agent_client_protocol::Error> for SessionClientError {
    fn from(e: agent_client_protocol::Error) -> Self {
        SessionClientError::Acp(e.to_string())
    }
}

/// Opaque logical thread identifier, owned by `panel-rust` (the left-hand
/// thread list). Deliberately distinct from ACP's own `SessionId` -- a
/// thread can exist locally (jsonl cache) before any ACP session has been
/// opened for it, and a `ThreadId` is what indexes the actor pool, not the
/// wire protocol.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ThreadId(pub String);

/// Message content-block kinds the chat UI distinguishes visually, per
/// `ui.yaml` task 3's "ongoing commands runs highlighted, thinking
/// highlighted" requirement. Deliberately a small, closed set -- not a
/// pass-through of ACP's full `SessionUpdate` enum, so `panel-rust` never
/// has to match on wire variants (including ones this crate doesn't yet
/// forward, e.g. `Plan`/`AvailableCommandsUpdate` -- left as future work,
/// see `classify_update` below).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    User,
    Agent,
    Thinking,
    ToolCall,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub kind: MessageKind,
    pub text: String,
}

/// A summary of a session the bound agent already knows about, from
/// `session/list` -- translated out of `agent_client_protocol`'s wire type
/// so it doesn't leak past this crate's boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteThreadInfo {
    pub acp_session_id: String,
    pub title: Option<String>,
    pub updated_at: Option<String>,
}

impl From<SessionInfo> for RemoteThreadInfo {
    fn from(info: SessionInfo) -> Self {
        RemoteThreadInfo {
            acp_session_id: info.session_id.0.to_string(),
            title: info.title,
            updated_at: info.updated_at,
        }
    }
}

/// Events flowing out of a bound thread's actor, consumed from
/// `ThreadHandle::events`.
#[derive(Debug)]
pub enum AgentEvent {
    Message(ChatMessage),
    /// A prompt turn finished; carries the ACP stop reason as a
    /// human-readable tag (`"end_turn"`, `"cancelled"`, etc.) rather than
    /// re-exporting the wire enum.
    TurnEnded(String),
    Error(String),
}

enum Command {
    OpenSession {
        cwd: PathBuf,
        resp: oneshot::Sender<Result<String, SessionClientError>>,
    },
    SendPrompt {
        text: String,
        resp: oneshot::Sender<Result<(), SessionClientError>>,
    },
    ListSessions {
        resp: oneshot::Sender<Result<Vec<RemoteThreadInfo>, SessionClientError>>,
    },
    LoadSession {
        session_id: String,
        cwd: PathBuf,
        resp: oneshot::Sender<Result<(), SessionClientError>>,
    },
    Shutdown,
}

/// Handle to one thread's bound agent actor. Cheap to hold onto; sending a
/// command and awaiting its response round-trips through the actor's
/// single-threaded command loop, so calls against the same handle are
/// naturally serialized (no risk of e.g. two concurrent prompts racing on
/// one `ActiveSession`).
pub struct ThreadHandle {
    cmd_tx: mpsc::UnboundedSender<Command>,
    /// Agent-originated events (streamed message chunks, turn-end, errors).
    /// Public so callers can `events.recv().await` in their own UI-refresh
    /// loop without this crate dictating that loop's shape.
    pub events: mpsc::UnboundedReceiver<AgentEvent>,
}

impl ThreadHandle {
    async fn call<T>(
        &self,
        make_cmd: impl FnOnce(oneshot::Sender<Result<T, SessionClientError>>) -> Command,
    ) -> Result<T, SessionClientError> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.cmd_tx
            .send(make_cmd(resp_tx))
            .map_err(|_| SessionClientError::ActorGone)?;
        resp_rx.await.map_err(|_| SessionClientError::ActorGone)?
    }

    /// Open (create) a new ACP session on this thread's bound agent.
    /// Returns the agent-assigned session id.
    pub async fn open_session(&self, cwd: impl Into<PathBuf>) -> Result<String, SessionClientError> {
        let cwd = cwd.into();
        self.call(|resp| Command::OpenSession { cwd, resp }).await
    }

    /// Send a prompt on the currently open session and drain the turn to
    /// completion, forwarding each content chunk to `events` as it arrives.
    /// Resolves once the turn ends (or errors).
    pub async fn send_prompt(&self, text: impl Into<String>) -> Result<(), SessionClientError> {
        let text = text.into();
        self.call(|resp| Command::SendPrompt { text, resp }).await
    }

    /// `session/list`, per Decision 2's resync sequence -- caller diffs the
    /// result against the jsonl trailer via `JsonlStore::is_stale`.
    pub async fn list_sessions(&self) -> Result<Vec<RemoteThreadInfo>, SessionClientError> {
        self.call(|resp| Command::ListSessions { resp }).await
    }

    /// `session/load`, per Decision 2 -- only called when `list_sessions`
    /// indicates the local cache is stale. Session-update notifications the
    /// agent replays during the load are forwarded to `events` same as any
    /// other message (see the actor's global notification handler).
    pub async fn load_session(
        &self,
        session_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
    ) -> Result<(), SessionClientError> {
        let session_id = session_id.into();
        let cwd = cwd.into();
        self.call(|resp| Command::LoadSession { session_id, cwd, resp }).await
    }

    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(Command::Shutdown);
    }

    /// Detach the event stream for independent consumption -- e.g. moving
    /// it into a forwarder task on a different executor/thread than the
    /// one holding this handle. After calling this, `events` is replaced
    /// with a closed, empty receiver: this handle remains valid for
    /// sending commands (`send_prompt`, etc.), it simply no longer
    /// receives events itself -- the caller owns that responsibility via
    /// the returned receiver from this point on.
    pub fn take_events(&mut self) -> mpsc::UnboundedReceiver<AgentEvent> {
        let (empty_tx, empty_rx) = mpsc::unbounded_channel();
        drop(empty_tx); // closed immediately -- self.events.recv() will just return None from now on
        std::mem::replace(&mut self.events, empty_rx)
    }
}

/// Spawn a standalone thread actor without registering it in a
/// [`SessionClient`] pool -- useful for callers (like `panel-rust`) that
/// want to own their own keying scheme instead of `ThreadId`. This is what
/// [`SessionClient::bind_thread`] itself calls internally.
pub fn spawn_thread<T>(transport: T) -> ThreadHandle
where
    T: ConnectTo<Client> + Send + 'static,
{
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    tokio::spawn(run_thread_actor(transport, cmd_rx, event_tx));
    ThreadHandle { cmd_tx, events: event_rx }
}

/// The pool of per-thread agent actors, per Decision 4's "hold multiple
/// concurrent SessionClient/agent-process connections at once, keyed by
/// thread" requirement.
#[derive(Default)]
pub struct SessionClient {
    threads: HashMap<ThreadId, ThreadHandle>,
}

impl SessionClient {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind `id` to a fresh agent actor talking over `transport`. If `id`
    /// was already bound, the previous actor is shut down first -- a
    /// thread is never bound to two live agent connections at once.
    pub fn bind_thread<T>(&mut self, id: ThreadId, transport: T) -> &mut ThreadHandle
    where
        T: ConnectTo<Client> + Send + 'static,
    {
        if let Some(existing) = self.threads.remove(&id) {
            existing.shutdown();
        }
        self.threads.insert(id.clone(), spawn_thread(transport));
        self.threads.get_mut(&id).expect("just inserted")
    }

    pub fn thread(&self, id: &ThreadId) -> Option<&ThreadHandle> {
        self.threads.get(id)
    }

    pub fn thread_mut(&mut self, id: &ThreadId) -> Option<&mut ThreadHandle> {
        self.threads.get_mut(id)
    }

    pub fn unbind_thread(&mut self, id: &ThreadId) {
        if let Some(handle) = self.threads.remove(id) {
            handle.shutdown();
        }
    }

    pub fn thread_ids(&self) -> impl Iterator<Item = &ThreadId> {
        self.threads.keys()
    }
}

/// Maps a wire `SessionUpdate` into this crate's small `ChatMessage`
/// vocabulary. Returns `None` for update kinds the chat UI doesn't render
/// as a message yet (plan/available-commands/mode/config/usage updates) --
/// deliberate scope narrowing for phase 1, not a bug: those are real ACP
/// updates this crate simply doesn't have a UI consumer for yet.
fn classify_update(update: SessionUpdate) -> Option<ChatMessage> {
    let extract_text = |chunk: ContentChunk| -> Option<String> {
        match chunk.content {
            ContentBlock::Text(t) => Some(t.text),
            _ => None,
        }
    };
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => extract_text(chunk).map(|text| ChatMessage {
            kind: MessageKind::Agent,
            text,
        }),
        SessionUpdate::AgentThoughtChunk(chunk) => extract_text(chunk).map(|text| ChatMessage {
            kind: MessageKind::Thinking,
            text,
        }),
        SessionUpdate::UserMessageChunk(chunk) => extract_text(chunk).map(|text| ChatMessage {
            kind: MessageKind::User,
            text,
        }),
        SessionUpdate::ToolCall(tool_call) => Some(ChatMessage {
            kind: MessageKind::ToolCall,
            text: tool_call.title,
        }),
        SessionUpdate::ToolCallUpdate(update) => update.fields.title.map(|title| ChatMessage {
            kind: MessageKind::ToolCall,
            text: title,
        }),
        _ => None,
    }
}

fn stop_reason_tag(reason: AcpStopReason) -> String {
    match reason {
        AcpStopReason::EndTurn => "end_turn",
        AcpStopReason::MaxTokens => "max_tokens",
        AcpStopReason::MaxTurnRequests => "max_turn_requests",
        AcpStopReason::Refusal => "refusal",
        AcpStopReason::Cancelled => "cancelled",
        _ => "unknown",
    }
    .to_string()
}

async fn run_thread_actor<T>(
    transport: T,
    cmd_rx: mpsc::UnboundedReceiver<Command>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
) where
    T: ConnectTo<Client> + Send + 'static,
{
    let notif_event_tx = event_tx.clone();
    let cmd_rx = std::sync::Mutex::new(Some(cmd_rx));
    let actor_event_tx = event_tx.clone();

    let result = Client
        .builder()
        .name("rui-acp-client")
        // v1: auto-approve every permission request, matching the SDK's own
        // `yolo_one_shot_client` reference example. Real permission-mode
        // wiring (surfacing the choice in the header per ui.yaml task 3) is
        // deliberate follow-up work, not silently assumed done here.
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                let option_id: Option<PermissionOptionId> =
                    request.options.first().map(|o| o.option_id.clone());
                if let Some(id) = option_id {
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
                    ))
                } else {
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // Catches session/update notifications that arrive *outside* an
        // ActiveSession's own dynamic handler -- concretely, the replay a
        // `session/load` response triggers, before any `open_session` call
        // re-establishes an ActiveSession for that same session id.
        .on_receive_notification(
            async move |notif: SessionNotification, _cx| {
                if let Some(msg) = classify_update(notif.update) {
                    let _ = notif_event_tx.send(AgentEvent::Message(msg));
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(transport, async move |cx: ConnectionTo<Agent>| {
            let mut cmd_rx = cmd_rx.lock().expect("not reentrant").take().expect("used once");
            actor_main(cx, &mut cmd_rx, &actor_event_tx).await
        })
        .await;

    if let Err(e) = result {
        let _ = event_tx.send(AgentEvent::Error(format!("connection ended: {e}")));
    }
}

async fn actor_main(
    cx: ConnectionTo<Agent>,
    cmd_rx: &mut mpsc::UnboundedReceiver<Command>,
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<(), agent_client_protocol::Error> {
    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
        .block_task()
        .await?;

    let mut active: Option<ActiveSession<'static, Agent>> = None;

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            Command::OpenSession { cwd, resp } => {
                match cx.build_session(cwd).block_task().start_session().await {
                    Ok(session) => {
                        let id = session.session_id().0.to_string();
                        active = Some(session);
                        let _ = resp.send(Ok(id));
                    }
                    Err(e) => {
                        let _ = resp.send(Err(e.into()));
                    }
                }
            }
            Command::SendPrompt { text, resp } => {
                let Some(session) = active.as_mut() else {
                    let _ = resp.send(Err(SessionClientError::NoActiveSession));
                    continue;
                };
                if let Err(e) = session.send_prompt(text) {
                    let _ = resp.send(Err(e.into()));
                    continue;
                }
                let outcome = drain_turn(session, event_tx).await;
                let _ = resp.send(outcome);
            }
            Command::ListSessions { resp } => {
                let result = cx
                    .send_request(ListSessionsRequest::new())
                    .block_task()
                    .await;
                let _ = resp.send(
                    result
                        .map(|r| r.sessions.into_iter().map(Into::into).collect())
                        .map_err(Into::into),
                );
            }
            Command::LoadSession { session_id, cwd, resp } => {
                let req = LoadSessionRequest::new(session_id, cwd);
                let result = cx.send_request(req).block_task().await;
                let _ = resp.send(result.map(|_| ()).map_err(Into::into));
            }
            Command::Shutdown => break,
        }
    }
    Ok(())
}

/// Drain one prompt turn's updates to completion, forwarding each as an
/// `AgentEvent::Message` (per the v1 simplification documented at module
/// top -- no interleaved command processing during a turn yet).
async fn drain_turn(
    session: &mut ActiveSession<'static, Agent>,
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Result<(), SessionClientError> {
    loop {
        match session.read_update().await {
            Ok(SessionMessage::SessionMessage(dispatch)) => {
                let event_tx = event_tx.clone();
                let _ = agent_client_protocol::util::MatchDispatch::new(dispatch)
                    .if_notification(async move |notif: SessionNotification| {
                        if let Some(msg) = classify_update(notif.update) {
                            let _ = event_tx.send(AgentEvent::Message(msg));
                        }
                        Ok(())
                    })
                    .await
                    .otherwise_ignore();
            }
            Ok(SessionMessage::StopReason(reason)) => {
                let _ = event_tx.send(AgentEvent::TurnEnded(stop_reason_tag(reason)));
                return Ok(());
            }
            Err(e) => {
                let err = SessionClientError::from(e);
                let _ = event_tx.send(AgentEvent::Error(err.to_string()));
                return Err(err);
            }
            Ok(_) => {
                // `SessionMessage` is `#[non_exhaustive]` on the SDK side;
                // any future variant it adds is intentionally ignored here
                // rather than treated as a compile break.
            }
        }
    }
}
