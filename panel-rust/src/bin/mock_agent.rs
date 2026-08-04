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
//!   A `"stream "`-prefixed prompt instead sends its body as several
//!   partial `agent_message_chunk`s (fixed-size, mid-token splits
//!   included) with a real delay between each -- see the prompt handler's
//!   `"stream "` marker branch.
//! - `session/list`: returns every session created so far, with a
//!   `title`/`updated_at` that changes each time a prompt completes on it
//!   (so cache-staleness tests have something real to diff against).
//! - `session/load`: replays the same three notifications as a fresh
//!   prompt turn would have produced (a stand-in for "replay history").

use agent_client_protocol::schema::v1::{
    AgentCapabilities, AvailableCommand, AvailableCommandsUpdate, CancelNotification,
    CloseSessionRequest, CloseSessionResponse, ContentBlock, ContentChunk, CreateTerminalRequest,
    DeleteSessionRequest, DeleteSessionResponse, InitializeResponse, ListSessionsResponse,
    LoadSessionResponse, NewSessionResponse, PermissionOption, PermissionOptionKind, Plan,
    PlanEntry, PlanEntryPriority, PlanEntryStatus, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, ResumeSessionRequest, ResumeSessionResponse, SessionConfigOption,
    SessionConfigSelectOption, SessionId, SessionInfo, SessionInfoUpdate, SessionNotification,
    SessionUpdate, StopReason, TerminalOutputRequest, TextContent, ToolCall, ToolCallId,
    ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Dispatch, Handled, Result, Stdio};
use std::collections::HashMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
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

/// Per-persona `available_commands_update` payload, PUI-003's compose `/`
/// menu is fed exclusively from a thread's own ACP `available_commands`
/// (`sync.rs`'s `sync_commands_model`) -- so two threads bound to two
/// different gateway processes must see two disjoint command lists if
/// (and only if) that per-thread isolation genuinely holds end to end.
/// Command *names* are deliberately persona-prefixed (`codex_*` /
/// `claude_*`) rather than shared generic names so a cross-wiring bug
/// (thread A's `/` menu showing thread B's agent's commands) is
/// unambiguous in a test assertion instead of two identical-looking
/// lists that happen to pass by coincidence. Unset/unknown persona
/// advertises no commands at all, matching `persona_prefix`'s own
/// "direct, non-gateway dev path stays unchanged" convention.
fn persona_commands() -> Vec<AvailableCommand> {
    match std::env::var("RUI_MOCK_AGENT_PERSONA")
        .unwrap_or_default()
        .as_str()
    {
        "codex" => vec![
            AvailableCommand::new("codex_plan", "Draft an execution plan (codex persona)"),
            AvailableCommand::new("codex_review", "Review a diff (codex persona)"),
        ],
        "claude" => vec![
            AvailableCommand::new("claude_plan", "Draft an execution plan (claude persona)"),
            AvailableCommand::new(
                "claude_summarize",
                "Summarize the conversation (claude persona)",
            ),
        ],
        "grok" => vec![AvailableCommand::new(
            "grok_search",
            "Search the web (grok persona)",
        )],
        _ => Vec::new(),
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

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct SessionState {
    title: String,
    updated_at: String,
    turn_count: u64,
    replay_turns: Vec<ReplayTurn>,
    /// pool-capability-fix regression coverage: this session's current
    /// `"model"` configOption value, mutated only by a real `session/
    /// set_config_option` call below -- lets an e2e test prove a pooled
    /// session's config state really does persist across a lease
    /// release/reacquire (or really doesn't, once `thread_actor.rs`
    /// resets it before release).
    model: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct ReplayTurn {
    prompt_text: String,
}

static SESSIONS: Mutex<Option<HashMap<String, SessionState>>> = Mutex::new(None);

fn with_sessions<T>(f: impl FnOnce(&mut HashMap<String, SessionState>) -> T) -> T {
    let mut guard = SESSIONS.lock().expect("mock-agent session map poisoned");
    let map = guard.get_or_insert_with(HashMap::new);
    f(map)
}

/// Optional durable state for the real-server restart matrix. Normal mock
/// agent runs stay process-local; setting this path makes session/list,
/// session/load, and session/resume survive a fresh mock-agent process while
/// keeping the fixture deterministic and isolated per test.
fn state_file() -> Option<std::path::PathBuf> {
    std::env::var_os("RUI_MOCK_AGENT_STATE_FILE").map(std::path::PathBuf::from)
}

fn load_persisted_sessions() {
    let Some(path) = state_file() else {
        return;
    };
    let Ok(bytes) = fs::read(path) else {
        return;
    };
    let Ok(sessions) = serde_json::from_slice::<HashMap<String, SessionState>>(&bytes) else {
        return;
    };
    let mut guard = SESSIONS.lock().expect("mock-agent session map poisoned");
    *guard = Some(sessions);
}

fn persist_sessions() {
    let Some(path) = state_file() else {
        return;
    };
    let sessions = {
        let guard = SESSIONS.lock().expect("mock-agent session map poisoned");
        guard.clone().unwrap_or_default()
    };
    let Ok(bytes) = serde_json::to_vec(&sessions) else {
        return;
    };
    let temp = path.with_extension("json.tmp");
    if fs::write(&temp, bytes).is_ok() {
        let _ = fs::rename(temp, path);
    }
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
    let mut guard = CANCEL_NOTIFY
        .lock()
        .expect("mock-agent cancel map poisoned");
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
    load_persisted_sessions();
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
            async move |request: agent_client_protocol::schema::v1::NewSessionRequest,
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
                            model: "mock-model-a".to_string(),
                        },
                    );
                });
                persist_sessions();
                // PISO-6: the live isolation matrix needs to assert each
                // thread's `session/new` actually carried its OWN project's
                // cwd (not the global override) -- `request.cwd` is the one
                // place that's visible from outside the panel process, so
                // it's logged as this event's `detail` instead of the
                // previously-discarded `""`. Every existing assertion in
                // this crate keys off `detail` by exact string match on
                // prompt text, never `session/new`'s, so this is additive.
                record_gateway_event("session/new", Some(&id), &request.cwd.to_string_lossy());
                // acpx-client-session-lease-pool regression coverage: a real
                // backend (claude-acp) advertises configOptions on its
                // session/new response; this mock previously returned a
                // bare response with none at all, which meant the
                // "fresh-create sessions never got capability events" bug
                // (see thread_actor.rs's AcquireAndAttach) was invisible to
                // every mock-agent-backed e2e scenario -- there was nothing
                // to fail to propagate. One fake "model" select option is
                // enough for a test to assert ChatInput's config dropdown
                // actually populates for a pool-created thread.
                let config_options = vec![SessionConfigOption::select(
                    "model",
                    "Model",
                    "mock-model-a",
                    vec![
                        SessionConfigSelectOption::new("mock-model-a", "Mock Model A"),
                        SessionConfigSelectOption::new("mock-model-b", "Mock Model B"),
                    ],
                )];
                responder.respond(
                    NewSessionResponse::new(SessionId::new(id)).config_options(config_options),
                )
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
                // PUI-003 e2e: advertise this persona's slash commands on
                // the session's first prompt turn -- not `session/new`,
                // because a notification sent before `NewSessionResponse`
                // is acknowledged races the client's session_id->thread
                // bookkeeping (the panel only starts routing a session's
                // notifications to a thread once it has processed that
                // response). The first prompt is the earliest point a
                // real client is guaranteed to already have that mapping
                // in place, so it's the deterministic point used here.
                let is_first_turn = with_sessions(|sessions| {
                    sessions
                        .get(session_id.0.as_ref())
                        .map(|s| s.turn_count == 0)
                        .unwrap_or(true)
                });
                if is_first_turn {
                    let commands = persona_commands();
                    if !commands.is_empty() {
                        let _ = connection.send_notification(SessionNotification::new(
                            session_id.clone(),
                            SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(
                                commands,
                            )),
                        ));
                    }
                }
                // PROF-11 e2e marker (same lowercase-plain-word convention
                // as `"slow "` below): a `"plan "`-prefixed prompt gets a
                // real `plan` session/update (two entries, one completed
                // one in-progress -- exercising both `PlanEntryStatus`
                // values a single notification could plausibly carry) and
                // a real `session_info_update` pushing a live title,
                // BEFORE the usual thought/tool_call/message-chunk turn --
                // gated behind this marker (not sent on every turn) so it
                // doesn't perturb the message-count/ordering assumptions
                // every other existing e2e test already depends on.
                if text.starts_with("plan ") {
                    let _ = connection.send_notification(SessionNotification::new(
                        session_id.clone(),
                        SessionUpdate::Plan(Plan::new(vec![
                            PlanEntry::new(
                                "Read the file",
                                PlanEntryPriority::High,
                                PlanEntryStatus::Completed,
                            ),
                            PlanEntry::new(
                                "Fix the bug",
                                PlanEntryPriority::Medium,
                                PlanEntryStatus::InProgress,
                            ),
                        ])),
                    ));
                    let _ = connection.send_notification(SessionNotification::new(
                        session_id.clone(),
                        SessionUpdate::SessionInfoUpdate(
                            SessionInfoUpdate::new().title("Fixing the login bug"),
                        ),
                    ));
                }
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
                // markdown-render-cache-layer MCP-04 coverage: a
                // `"stream "`-prefixed prompt sends its markdown body as
                // several partial `agent_message_chunk`s (mid-token splits
                // included) with a real delay between each, instead of the
                // usual single complete chunk `send_replay` sends below --
                // the only way a live e2e test can observe a genuinely
                // unterminated trailing block (e.g. `"**bol"` before
                // `"d**"` arrives) and confirm `heal_open_markers` +
                // `markdown_worker`'s epoch/dedupe machinery behave the
                // same live as they do against the synthetic partial
                // strings the unit tests use. Chunk boundaries are fixed
                // (every 6 bytes) rather than word-aligned so runs land
                // mid-marker often, not just between words.
                if let Some(marker_text) = text.strip_prefix("stream ") {
                    let body = format!("{}{}", persona_prefix(), marker_text);
                    let connection_for_stream = connection.clone();
                    let session_id_for_stream = session_id.clone();
                    tokio::spawn(async move {
                        let bytes = body.as_bytes();
                        const CHUNK_LEN: usize = 6;
                        let mut sent = 0usize;
                        while sent < bytes.len() {
                            let end = (sent + CHUNK_LEN).min(bytes.len());
                            // `body` is UTF-8; fall back to the next char
                            // boundary if a fixed byte cut lands mid-codepoint.
                            let mut end = end;
                            while end < bytes.len() && (bytes[end] & 0xC0) == 0x80 {
                                end += 1;
                            }
                            let piece = String::from_utf8_lossy(&bytes[sent..end]).into_owned();
                            let _ =
                                connection_for_stream.send_notification(SessionNotification::new(
                                    session_id_for_stream.clone(),
                                    SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                        ContentBlock::Text(TextContent::new(piece)),
                                    )),
                                ));
                            sent = end;
                            if sent < bytes.len() {
                                tokio::time::sleep(Duration::from_millis(150)).await;
                            }
                        }
                        let _ = responder.respond(PromptResponse::new(StopReason::EndTurn));
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
                            SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                ContentBlock::Text(TextContent::new(format!(
                                    "{}permission decision: {chosen}",
                                    persona_prefix()
                                ))),
                            )),
                        ));
                        let _ = responder.respond(PromptResponse::new(StopReason::EndTurn));
                    });
                    return Ok(());
                }
                // Real terminal relay coverage: ask the ACP client to create
                // and read a terminal, then include its output in the live
                // assistant update. This is opt-in via the `terminal ` prompt
                // marker so existing deterministic mock-agent scenarios keep
                // their stable three-update transcript.
                if let Some(marker_text) = text.strip_prefix("terminal ") {
                    let marker_text = marker_text.to_string();
                    let connection_for_terminal = connection.clone();
                    let session_id_for_terminal = session_id.clone();
                    tokio::spawn(async move {
                        // Invoke `echo` directly and pass the complete marker
                        // as one ACP argument, preserving both the terminal
                        // and assistant assertions even when the marker has
                        // spaces.
                        let command =
                            CreateTerminalRequest::new(session_id_for_terminal.clone(), "echo")
                                .args(vec![marker_text.clone()]);
                        let created = connection_for_terminal
                            .send_request(command)
                            .block_task()
                            .await;
                        let output = match created {
                            Ok(created) => {
                                let terminal_id = created.terminal_id.clone();
                                let mut output = String::new();
                                let mut last_error = None;
                                for _ in 0..40 {
                                    match connection_for_terminal
                                        .send_request(TerminalOutputRequest::new(
                                            session_id_for_terminal.clone(),
                                            terminal_id.clone(),
                                        ))
                                        .block_task()
                                        .await
                                    {
                                        Ok(result) => {
                                            output = result.output;
                                            if !output.is_empty() || result.exit_status.is_some() {
                                                break;
                                            }
                                        }
                                        Err(error) => {
                                            last_error = Some(format!("{error:?}"));
                                            break;
                                        }
                                    }
                                    tokio::time::sleep(Duration::from_millis(25)).await;
                                }
                                if let Some(error) = last_error {
                                    record_gateway_event(
                                        "terminal/output_error",
                                        Some(session_id_for_terminal.0.as_ref()),
                                        &error,
                                    );
                                    "terminal-output-error".to_string()
                                } else {
                                    output
                                }
                            }
                            Err(error) => {
                                record_gateway_event(
                                    "terminal/create_error",
                                    Some(session_id_for_terminal.0.as_ref()),
                                    &format!("{error:?}"),
                                );
                                "terminal-create-error".to_string()
                            }
                        };
                        record_gateway_event(
                            "terminal/output",
                            Some(session_id_for_terminal.0.as_ref()),
                            &output,
                        );
                        let _ =
                            connection_for_terminal.send_notification(SessionNotification::new(
                                session_id_for_terminal,
                                SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                    ContentBlock::Text(TextContent::new(format!(
                                        "{}{}",
                                        persona_prefix(),
                                        output
                                    ))),
                                )),
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
                persist_sessions();
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
                persist_sessions();
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
        .on_receive_request(
            // pool-capability-fix regression coverage: `session/set_
            // config_option` is a real, published ACP extension method
            // (see `acpx-core/src/router.rs`'s `MethodClass::Proxied`
            // comment) that this SDK's schema doesn't type -- acpx
            // forwards it to the connected agent byte-for-byte, so this
            // mock must actually answer it (not just advertise
            // configOptions on `session/new`) for a pool-reuse-leak test
            // to exercise the real wire path `thread_actor.rs`'s
            // `Command::SetConfigOption` drives. Registered last (via
            // the untyped fallback) so every method already claimed by a
            // typed handler above is unaffected; anything this handler
            // doesn't recognize is passed on to the `unhandled message`
            // catch-all below exactly as before this was added.
            async move |request: agent_client_protocol::UntypedMessage,
                        responder: agent_client_protocol::Responder<serde_json::Value>,
                        _connection| {
                if request.method() != "session/set_config_option" {
                    return Ok(agent_client_protocol::Handled::No {
                        message: (request, responder),
                        retry: false,
                    });
                }
                let params = request.params();
                let session_id = params
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let config_id = params
                    .get("configId")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let value = params
                    .get("value")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                record_gateway_event(
                    "session/set_config_option",
                    Some(&session_id),
                    &format!("{config_id}={value}"),
                );
                let current = with_sessions(|sessions| {
                    let Some(state) = sessions.get_mut(&session_id) else {
                        return value.clone();
                    };
                    if config_id == "model" {
                        state.model = value.clone();
                    }
                    state.model.clone()
                });
                responder.respond(serde_json::json!({
                    "configOptions": [{
                        "id": "model",
                        "name": "Model",
                        "type": "select",
                        "currentValue": current,
                        "options": [
                            {"value": "mock-model-a", "name": "Mock Model A"},
                            {"value": "mock-model-b", "name": "Mock Model B"}
                        ]
                    }]
                }))?;
                Ok(agent_client_protocol::Handled::Yes)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_dispatch(
            async move |message: Dispatch, cx: ConnectionTo<Client>| {
                // Responses belong to the SDK's pending-request router.  A
                // catch-all dispatch handler must pass them through; claiming
                // one here turns every typed agent->client response (for
                // example terminal/create) into an internal "unhandled
                // message" error before the waiting send_request task sees
                // it.  Requests/notifications are the only messages this
                // fallback is meant to reject.
                if let Dispatch::Response(_, _) = &message {
                    return Ok(Handled::No {
                        message,
                        retry: false,
                    });
                }
                record_gateway_event("unhandled", None, message.method());
                message.respond_with_error(
                    agent_client_protocol::util::internal_error("unhandled message"),
                    cx,
                )?;
                Ok(Handled::Yes)
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_to(Stdio::new())
        .await
}
