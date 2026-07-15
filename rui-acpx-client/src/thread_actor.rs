//! One background actor per chat thread, talking to a bound acpx-server
//! over `acpx-client::raw::GatewayClient`. Method names/shapes
//! deliberately mirror `rui_acp_client::session_client::ThreadHandle`
//! (`open_session`/`send_prompt`/`list_sessions`/`shutdown`/`take_events`)
//! -- `panel-rust/src/agent_bridge.rs`'s actor-forwarding loop needed only
//! an import/type swap for the acpx cutover, not a rewrite, because of
//! this deliberate shape match.

use crate::{classify_raw_update, AgentEvent};
use acpx_client::raw::ClientError;
use acpx_client::{AgentRequest, Gateway};
use rui_acp_client::{AgentRequestEvent, TerminalOutputEvent};
use std::path::PathBuf;
use tokio::sync::{mpsc, oneshot, watch};

#[derive(thiserror::Error, Debug)]
pub enum AcpxThreadError {
    #[error("no active session on this thread -- call open_session or resume_session first")]
    NoActiveSession,
    #[error("actor task for this thread is gone (shut down or panicked)")]
    ActorGone,
    #[error("acpx gateway error: {0}")]
    Gateway(#[from] ClientError),
    #[error("gateway response for session/new had no sessionId field")]
    MissingSessionId,
}

/// A summary of a session the bound gateway already knows about, from
/// `session/list` -- translated out of `acpx_client::ext::sessions`'s
/// wire-adjacent type so it doesn't leak past this crate's boundary,
/// mirroring `rui_acp_client::RemoteThreadInfo`'s role for the direct
/// path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteThreadInfo {
    pub acp_session_id: String,
    pub agent_id: String,
}

enum Command {
    OpenSession {
        cwd: PathBuf,
        /// `_acpx.profile` to send with `session/new`, if any -- see
        /// [`AcpxThreadHandle::open_session_with_profile`]'s doc
        /// comment. `None` (the shape every pre-existing caller via
        /// [`AcpxThreadHandle::open_session`] still gets) omits
        /// `_acpx.profile` entirely, i.e. native/unmanaged mode, byte-
        /// for-byte the same request this crate always sent before
        /// profile selection existed.
        profile: Option<String>,
        resp: oneshot::Sender<Result<String, AcpxThreadError>>,
    },
    /// `session/load` against an already-known gateway session id --
    /// the resume-after-relaunch path (verification requirement: closing
    /// and relaunching the app auto-reloads session instances, and
    /// resuming with a new message continues the same session via
    /// acpx-server, not a fresh `session/new`). Whatever history the
    /// backend replays via `session/update` notifications during the load
    /// is forwarded to `events`, same as any other message.
    ResumeSession {
        session_id: String,
        cwd: PathBuf,
        resp: oneshot::Sender<Result<(), AcpxThreadError>>,
    },
    SendPrompt {
        text: String,
        resp: oneshot::Sender<Result<(), AcpxThreadError>>,
    },
    ListSessions {
        resp: oneshot::Sender<Result<Vec<RemoteThreadInfo>, AcpxThreadError>>,
    },
    /// Explicit, opt-in-only `session/close`. Deliberately **never**
    /// sent by `shutdown()`/`Drop` -- see this crate's module doc and
    /// `agent_bridge.rs`'s teardown path: window/process close must not
    /// imply session close by default, only an explicit caller
    /// (a future per-thread "close on exit" setting) should ever send
    /// this.
    CloseSession {
        resp: oneshot::Sender<Result<(), AcpxThreadError>>,
    },
    Shutdown,
}

/// Handle to one thread's bound acpx-gateway actor. See
/// `rui_acp_client::session_client::ThreadHandle`'s doc comment -- the
/// same "commands serialize through one actor loop" guarantee applies
/// here.
pub struct AcpxThreadHandle {
    cmd_tx: mpsc::UnboundedSender<Command>,
    cancel_tx: mpsc::UnboundedSender<oneshot::Sender<Result<(), AcpxThreadError>>>,
    /// Independent channel for answering a live [`AgentEvent::
    /// PermissionRequest`] -- same "own worker, own gateway connection,
    /// never queued behind `cmd_tx`'s in-flight `SendPrompt`" reasoning
    /// as `cancel_tx`/`run_cancel_worker` above: a mid-turn permission
    /// decision must reach the gateway *while* `SendPrompt`'s own
    /// command is still occupying the main actor loop awaiting that
    /// exact turn's completion, so routing it through `cmd_tx` would
    /// deadlock (the answer the prompt call is waiting on would never
    /// be sent because the actor loop can't get back to `cmd_rx.recv()`
    /// until the prompt call itself returns).
    respond_tx: mpsc::UnboundedSender<RespondCommand>,
    pub events: mpsc::UnboundedReceiver<AgentEvent>,
}

struct RespondCommand {
    relay_id: String,
    response: serde_json::Value,
    resp: oneshot::Sender<Result<bool, AcpxThreadError>>,
}

impl AcpxThreadHandle {
    async fn call<T>(
        &self,
        make_cmd: impl FnOnce(oneshot::Sender<Result<T, AcpxThreadError>>) -> Command,
    ) -> Result<T, AcpxThreadError> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.cmd_tx
            .send(make_cmd(resp_tx))
            .map_err(|_| AcpxThreadError::ActorGone)?;
        resp_rx.await.map_err(|_| AcpxThreadError::ActorGone)?
    }

    /// `session/new` against this thread's bound gateway. Returns the
    /// gateway-issued session id (never the backend's own native id --
    /// per acpx's own design, only the gateway id is ever meaningful to
    /// a client).
    pub async fn open_session(&self, cwd: impl Into<PathBuf>) -> Result<String, AcpxThreadError> {
        let cwd = cwd.into();
        self.call(|resp| Command::OpenSession {
            cwd,
            profile: None,
            resp,
        })
        .await
    }

    /// Same as [`Self::open_session`], but selects a named ACPX profile
    /// (`_acpx.profile` in `session/new`'s params) -- e.g. to pick a
    /// profile with `allow_terminal_access`/`allow_fs_access` enabled,
    /// or a specific Codex/Claude configuration. `panel-rust`'s profile
    /// picker (Coverage Matrix row) is the intended production caller;
    /// exercised directly today by this crate's own tests.
    pub async fn open_session_with_profile(
        &self,
        cwd: impl Into<PathBuf>,
        profile: impl Into<String>,
    ) -> Result<String, AcpxThreadError> {
        let cwd = cwd.into();
        let profile = Some(profile.into());
        self.call(|resp| Command::OpenSession { cwd, profile, resp })
            .await
    }

    /// `session/load` against an already-known gateway session id --
    /// see [`Command::ResumeSession`]'s doc comment.
    pub async fn resume_session(
        &self,
        session_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
    ) -> Result<(), AcpxThreadError> {
        let session_id = session_id.into();
        let cwd = cwd.into();
        self.call(|resp| Command::ResumeSession {
            session_id,
            cwd,
            resp,
        })
        .await
    }

    /// Send a prompt on the currently open/resumed session and drain the
    /// turn to completion, forwarding every `session/update` the gateway
    /// aggregated (`_acpx.updates`) to `events` in order, then a final
    /// `AgentEvent::TurnEnded`.
    pub async fn send_prompt(&self, text: impl Into<String>) -> Result<(), AcpxThreadError> {
        let text = text.into();
        self.call(|resp| Command::SendPrompt { text, resp }).await
    }

    /// Gateway-aggregated `session/list` -- every session across every
    /// backend *this gateway* currently supervises, not just this
    /// thread's own.
    pub async fn list_sessions(&self) -> Result<Vec<RemoteThreadInfo>, AcpxThreadError> {
        self.call(|resp| Command::ListSessions { resp }).await
    }

    /// Explicit `session/close` -- opt-in only, see [`Command::CloseSession`].
    pub async fn close_session(&self) -> Result<(), AcpxThreadError> {
        self.call(|resp| Command::CloseSession { resp }).await
    }

    /// Sends `session/cancel` through an independent gateway connection, so
    /// it is never queued behind this handle's in-flight prompt command.
    pub async fn cancel_session(&self) -> Result<(), AcpxThreadError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.cancel_tx
            .send(response_tx)
            .map_err(|_| AcpxThreadError::ActorGone)?;
        response_rx.await.map_err(|_| AcpxThreadError::ActorGone)?
    }

    /// Answer a live [`AgentEvent::PermissionRequest`] -- `relay_id` must
    /// be the exact one that event carried; `response` is the decision
    /// payload the relay expects for that request's own method (a full
    /// native `RequestPermissionResponse`-shaped value for `session/
    /// request_permission`, or a `{"approved": bool}` decision envelope
    /// for `fs/*`/`terminal/create` -- see `acpx_core::router`'s
    /// `try_relay_agent_request`/`try_relay_approval` doc comments for
    /// which shape each method expects). Returns whether the gateway
    /// still had a pending relay waiting for this exact `relay_id`
    /// (`false` covers both an unknown id and one whose server-side
    /// wait already timed out).
    pub async fn respond_agent_request(
        &self,
        relay_id: impl Into<String>,
        response: serde_json::Value,
    ) -> Result<bool, AcpxThreadError> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.respond_tx
            .send(RespondCommand {
                relay_id: relay_id.into(),
                response,
                resp: resp_tx,
            })
            .map_err(|_| AcpxThreadError::ActorGone)?;
        resp_rx.await.map_err(|_| AcpxThreadError::ActorGone)?
    }

    /// Deliberately does **not** send `session/close` -- only stops this
    /// handle's own local actor task/command loop. The gateway-side
    /// session and its backend process are entirely unaffected, exactly
    /// the "window close does not imply session close" contract this
    /// crate's module doc describes.
    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(Command::Shutdown);
    }

    /// Detach the event stream for independent consumption -- same
    /// swap-for-a-closed-receiver trick as
    /// `rui_acp_client::ThreadHandle::take_events`.
    pub fn take_events(&mut self) -> mpsc::UnboundedReceiver<AgentEvent> {
        let (empty_tx, empty_rx) = mpsc::unbounded_channel();
        drop(empty_tx);
        std::mem::replace(&mut self.events, empty_rx)
    }
}

/// Spawn a standalone thread actor bound to one acpx-server instance at
/// `base_url` (e.g. `http://127.0.0.1:8790`). One actor per logical UI
/// thread, mirroring `rui_acp_client::spawn_thread`'s per-thread-static-
/// binding shape -- just against a gateway URL instead of a subprocess
/// transport.
pub fn spawn_acpx_thread(base_url: impl Into<String>) -> AcpxThreadHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (cancel_tx, cancel_rx) = mpsc::unbounded_channel();
    let (respond_tx, respond_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let base_url = base_url.into();
    let (session_tx, session_rx) = watch::channel(None::<String>);
    tokio::spawn(run_thread_actor(
        base_url.clone(),
        cmd_rx,
        event_tx,
        session_tx,
    ));
    tokio::spawn(run_cancel_worker(base_url.clone(), cancel_rx, session_rx));
    tokio::spawn(run_respond_worker(base_url, respond_rx));
    AcpxThreadHandle {
        cmd_tx,
        cancel_tx,
        respond_tx,
        events: event_rx,
    }
}

/// Independent worker with its own gateway connection, answering live
/// [`AgentEvent::PermissionRequest`]s -- see [`AcpxThreadHandle::
/// respond_agent_request`]'s doc comment for why this cannot share the
/// main actor's `cmd_tx`/`cmd_rx` loop. `base_url` alone is enough here
/// (unlike [`run_cancel_worker`], this never needs the bound session id
/// -- `acpx/agent_response` is addressed purely by `relay_id`, which the
/// server resolves against its own process-wide relay hub regardless of
 /// which connection sends it).
///
/// Connects **lazily**, on the first actual `RespondCommand`, unlike
/// [`run_thread_actor`]/[`run_cancel_worker`]'s eager `Gateway::connect`
/// at spawn time -- an interactive permission/fs/terminal decision is
/// comparatively rare next to the always-used session/prompt path, and a
/// third unconditional connection attempt per spawned thread has a real
/// cost (an extra live WS handshake per chat thread even for threads
/// that never see a single agent-initiated request in their lifetime).
/// Once connected, the same `Gateway` is reused for every subsequent
/// command on this worker, matching the other two workers' one-
/// connection-per-actor-lifetime shape.
async fn run_respond_worker(
    base_url: String,
    mut respond_rx: mpsc::UnboundedReceiver<RespondCommand>,
) {
    let mut client: Option<Gateway> = None;
    while let Some(cmd) = respond_rx.recv().await {
        let client = match &client {
            Some(client) => client,
            None => client.insert(Gateway::connect(base_url.clone()).await),
        };
        let result = client
            .respond_agent_request(&cmd.relay_id, cmd.response)
            .await
            .map_err(Into::into);
        let _ = cmd.resp.send(result);
    }
}

/// Forwards every classified `session/update` chunk in `updates` (in
/// order) to `event_tx` as an `AgentEvent::Message`, dropping (not
/// erroring on) anything `classify_raw_update` doesn't recognize --
/// same tolerant behavior the direct-ACP actor has for `SessionUpdate`
/// variants it doesn't render. **Does not** handle `acpx/agent_request`
/// or `acpx/terminal_output` notifications -- those are exclusively
/// [`spawn_out_of_band_notification_forwarder`]'s job now (see that
/// function's doc comment for why they were split out of this
/// function).
fn forward_updates(updates: &[serde_json::Value], event_tx: &mpsc::UnboundedSender<AgentEvent>) {
    for update in updates {
        if let Some(msg) = classify_raw_update(update) {
            let _ = event_tx.send(AgentEvent::Message(msg));
        }
    }
}

/// Spawns a task that forwards `acpx/agent_request` and
/// `acpx/terminal_output` notifications to `event_tx` for the entire
/// lifetime of `client`'s connection -- **independent** of
/// [`run_thread_actor`]'s own command loop and its `live_rx`-fed
/// `forward_updates` calls.
///
/// **Why this needs its own standalone subscription, not just another
/// branch in `forward_updates`.** `forward_updates`/`live_rx` are only
/// ever drained from inside `run_thread_actor`'s command-handling
/// arms (a `try_recv` sweep at the top of the loop, plus a bounded
/// racing/trailing-drain window scoped to one in-flight `SendPrompt`/
/// `ResumeSession` call) -- deliberately, since `session/update` message
/// chunk *ordering relative to that call's own completion* matters for
/// the streamed-typing UX. Once that call returns, the loop goes back to
/// blocking on `cmd_rx.recv().await`, and nothing drains `live_rx` again
/// until another command arrives. A live `acpx/terminal_output` push
/// (`acpx_core::router::spawn_terminal_output_stream` keeps polling for
/// as long as the terminal process runs, independent of whether the
/// `session/prompt` call that created it is still outstanding) would
/// then sit unread in `live_rx`'s buffer indefinitely if no further
/// command happened to be sent -- a real, previously-undiscovered gap
/// found by this crate's own `terminal_relay_e2e_test.rs`: a short-lived
/// backend command exits and its final push arrives well after
/// `session/prompt` has already completed, and no further command was
/// ever queued, so the push was silently lost.
///
/// **Why this doesn't double-deliver.** `client.subscribe()` (an
/// `acpx_client::ws::GatewayWsClient` broadcast channel) hands back a
/// fresh, independent `broadcast::Receiver` on every call -- this task's
/// subscription and `run_thread_actor`'s own are two separate receivers
/// of the same underlying broadcast, each seeing every notification
/// frame, so as long as each side only *acts* on the notification kinds
/// the other ignores, nothing is delivered twice: `forward_updates`
/// (fed by `run_thread_actor`'s own subscription) only recognizes
/// `session/update`-shaped frames now; this task only recognizes
/// `acpx/agent_request`/`acpx/terminal_output`-shaped frames. A `None`
/// from `client.subscribe()` (HTTP degraded mode -- no live push channel
/// at all) makes this a no-op, matching every other live-only code path
/// in this crate.
fn spawn_out_of_band_notification_forwarder(
    client: &Gateway,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
) {
    let Some(mut notifications) = client.subscribe() else {
        return;
    };
    tokio::spawn(async move {
        while let Ok(update) = notifications.recv().await {
            if let Some(request) = AgentRequest::from_notification(&update) {
                let method = request.method().unwrap_or_default().to_string();
                let _ = event_tx.send(AgentEvent::PermissionRequest(AgentRequestEvent {
                    relay_id: request.relay_id,
                    method,
                    raw_request: request.request,
                }));
            } else if let Some(term_ev) = parse_terminal_output(&update) {
                let _ = event_tx.send(AgentEvent::TerminalOutput(term_ev));
            }
        }
    });
}

/// Parses a bare `acpx/terminal_output` notification (see
/// `acpx_core::router::spawn_terminal_output_stream`'s doc comment for
/// the exact wire shape it publishes) into this crate's shared
/// `TerminalOutputEvent`. `None` for anything else, same "operate on
/// the raw JSON shape, tolerate unrecognized input" convention
/// `classify_raw_update` and `AgentRequest::from_notification` both
/// already follow.
fn parse_terminal_output(value: &serde_json::Value) -> Option<TerminalOutputEvent> {
    if value.get("method").and_then(|m| m.as_str()) != Some("acpx/terminal_output") {
        return None;
    }
    let params = value.get("params")?;
    let terminal_id = params.get("terminalId")?.as_str()?.to_string();
    let output = params.get("output")?.as_str()?.to_string();
    let truncated = params.get("truncated").and_then(|t| t.as_bool()).unwrap_or(false);
    let exit_status = params.get("exitStatus").filter(|v| !v.is_null()).map(|status| {
        (
            status.get("exitCode").and_then(|c| c.as_i64()).map(|c| c as i32),
            status.get("signal").and_then(|s| s.as_i64()).map(|s| s as i32),
        )
    });
    Some(TerminalOutputEvent {
        terminal_id,
        output,
        truncated,
        exit_status,
    })
}

async fn run_thread_actor(
    base_url: String,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    session_tx: watch::Sender<Option<String>>,
) {
    let client = Gateway::connect(base_url).await;
    spawn_out_of_band_notification_forwarder(&client, event_tx.clone());
    let (live_tx, mut live_rx) = mpsc::unbounded_channel();
    if let Some(mut live_notifications) = client.subscribe() {
        tokio::spawn(async move {
            while let Ok(update) = live_notifications.recv().await {
                let _ = live_tx.send(update);
            }
        });
    }
    let mut session_id: Option<String> = None;

    while let Some(cmd) = cmd_rx.recv().await {
        while let Ok(update) = live_rx.try_recv() {
            forward_updates(&[update], &event_tx);
        }
        match cmd {
            Command::OpenSession { cwd, profile, resp } => {
                let params = serde_json::json!({
                    "cwd": cwd.to_string_lossy(),
                    "mcpServers": [],
                });
                let mut result = Err(AcpxThreadError::ActorGone);
                for attempt in 0..5 {
                    result = match client
                        .call("session/new", params.clone(), profile.as_deref())
                        .await
                    {
                        Ok(value) => match value.get("sessionId").and_then(|s| s.as_str()) {
                            Some(sid) => {
                                session_id = Some(sid.to_string());
                                let _ = session_tx.send(Some(sid.to_string()));
                                Ok(sid.to_string())
                            }
                            None => Err(AcpxThreadError::MissingSessionId),
                        },
                        Err(e) => Err(e.into()),
                    };
                    if result.is_ok() || attempt == 4 {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100 * (attempt + 1))).await;
                }
                let _ = resp.send(result);
            }
            Command::ResumeSession {
                session_id: sid,
                cwd,
                resp,
            } => {
                // `session/load`'s `cwd`/`mcpServers` are required fields
                // per the ACP schema (`LoadSessionRequest`), unlike
                // `session/prompt`/`session/close` which only need
                // `sessionId` -- discovered by a real `-32602 Invalid
                // params` round trip against a real gateway+backend
                // during this crate's own e2e test, not assumed from the
                // spec alone.
                let params = serde_json::json!({
                    "sessionId": sid,
                    "cwd": cwd.to_string_lossy(),
                    "mcpServers": [],
                });
                // Match session/new's bounded retry. A relaunched panel can
                // race an acpx-server that is still accepting its socket but
                // has not finished restoring its sqlite-backed registry.
                // Falling back to session/new after one failed load would
                // silently break continuity, so only report failure after
                // the same five-attempt startup window used for opening.
                let mut result = Err(AcpxThreadError::ActorGone);
                for attempt in 0..5 {
                    result = match client
                        .call_with_updates("session/load", params.clone(), None)
                        .await
                    {
                        Ok((_, updates)) => {
                            forward_updates(&updates, &event_tx);
                            if let Ok(Some(update)) = tokio::time::timeout(
                                std::time::Duration::from_millis(50),
                                live_rx.recv(),
                            )
                            .await
                            {
                                forward_updates(&[update], &event_tx);
                            }
                            while let Ok(update) = live_rx.try_recv() {
                                forward_updates(&[update], &event_tx);
                            }
                            session_id = Some(sid.clone());
                            let _ = session_tx.send(Some(sid.clone()));
                            Ok(())
                        }
                        Err(e) => Err(e.into()),
                    };
                    if result.is_ok() || attempt == 4 {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100 * (attempt + 1))).await;
                }
                let _ = resp.send(result);
            }
            Command::SendPrompt { text, resp } => {
                let Some(sid) = session_id.clone() else {
                    let _ = resp.send(Err(AcpxThreadError::NoActiveSession));
                    continue;
                };
                let params = serde_json::json!({
                    "sessionId": sid,
                    "prompt": [{"type": "text", "text": text}],
                });
                let prompt = client.call_with_updates("session/prompt", params, None);
                tokio::pin!(prompt);
                let outcome = loop {
                    tokio::select! {
                        update = live_rx.recv() => {
                            if let Some(update) = update {
                                forward_updates(&[update], &event_tx);
                            }
                        }
                        result = &mut prompt => break result,
                    }
                };
                match outcome {
                    Ok((result, updates)) => {
                        forward_updates(&updates, &event_tx);
                        if let Ok(Some(update)) = tokio::time::timeout(
                            std::time::Duration::from_millis(100),
                            live_rx.recv(),
                        )
                        .await
                        {
                            forward_updates(&[update], &event_tx);
                        }
                        while let Ok(update) = live_rx.try_recv() {
                            forward_updates(&[update], &event_tx);
                        }
                        let stop_reason = result
                            .get("stopReason")
                            .and_then(|s| s.as_str())
                            .unwrap_or("end_turn")
                            .to_string();
                        let _ = event_tx.send(AgentEvent::TurnEnded(stop_reason));
                        let _ = resp.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = event_tx.send(AgentEvent::Error(e.to_string()));
                        let _ = resp.send(Err(e.into()));
                    }
                }
            }
            Command::ListSessions { resp } => {
                let result = client
                    .call("session/list", serde_json::json!({}), None)
                    .await
                    .map(|value| {
                        value["sessions"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default()
                            .into_iter()
                            .filter_map(|session| {
                                Some(RemoteThreadInfo {
                                    acp_session_id: session.get("sessionId")?.as_str()?.to_owned(),
                                    agent_id: session.get("agentId")?.as_str()?.to_owned(),
                                })
                            })
                            .collect()
                    });
                let _ = resp.send(result.map_err(Into::into));
            }
            Command::CloseSession { resp } => {
                let Some(sid) = session_id.clone() else {
                    // Never opened -- closing a session that was never
                    // opened on this handle is a no-op success, not an
                    // error (nothing gateway-side to close).
                    let _ = resp.send(Ok(()));
                    continue;
                };
                let params = serde_json::json!({ "sessionId": sid });
                let result = client.call("session/close", params, None).await;
                let _ = resp.send(result.map(|_| ()).map_err(Into::into));
            }
            Command::Shutdown => break,
        }
    }
}

async fn run_cancel_worker(
    base_url: String,
    mut cancel_rx: mpsc::UnboundedReceiver<oneshot::Sender<Result<(), AcpxThreadError>>>,
    session_rx: watch::Receiver<Option<String>>,
) {
    let client = Gateway::connect(base_url).await;
    while let Some(response) = cancel_rx.recv().await {
        let session_id = { session_rx.borrow().clone() };
        let result = match session_id {
            Some(session_id) => client
                .call(
                    "session/cancel",
                    serde_json::json!({ "sessionId": session_id }),
                    None,
                )
                .await
                .map(|_| ())
                .map_err(Into::into),
            None => Err(AcpxThreadError::NoActiveSession),
        };
        let _ = response.send(result);
    }
}
