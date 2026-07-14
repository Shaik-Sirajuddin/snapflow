//! One background actor per chat thread, talking to a bound acpx-server
//! over `acpx-client::raw::GatewayClient`. Method names/shapes
//! deliberately mirror `rui_acp_client::session_client::ThreadHandle`
//! (`open_session`/`send_prompt`/`list_sessions`/`shutdown`/`take_events`)
//! -- `panel-rust/src/agent_bridge.rs`'s actor-forwarding loop needed only
//! an import/type swap for the acpx cutover, not a rewrite, because of
//! this deliberate shape match.

use crate::{classify_raw_update, AgentEvent};
use acpx_client::ext::sessions as acpx_sessions;
use acpx_client::raw::{ClientError, GatewayClient};
use std::path::PathBuf;
use tokio::sync::{mpsc, oneshot};

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
    pub events: mpsc::UnboundedReceiver<AgentEvent>,
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
        self.call(|resp| Command::OpenSession { cwd, resp }).await
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
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let base_url = base_url.into();
    tokio::spawn(run_thread_actor(base_url, cmd_rx, event_tx));
    AcpxThreadHandle {
        cmd_tx,
        events: event_rx,
    }
}

/// Forwards every classified update in `updates` (in order) to `event_tx`,
/// dropping (not erroring on) anything `classify_raw_update` doesn't
/// recognize -- same tolerant behavior the direct-ACP actor has for
/// `SessionUpdate` variants it doesn't render.
fn forward_updates(updates: &[serde_json::Value], event_tx: &mpsc::UnboundedSender<AgentEvent>) {
    for update in updates {
        if let Some(msg) = classify_raw_update(update) {
            let _ = event_tx.send(AgentEvent::Message(msg));
        }
    }
}

async fn run_thread_actor(
    base_url: String,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
) {
    let client = GatewayClient::new(base_url);
    let mut session_id: Option<String> = None;

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            Command::OpenSession { cwd, resp } => {
                let params = serde_json::json!({
                    "cwd": cwd.to_string_lossy(),
                    "mcpServers": [],
                });
                let mut result = Err(AcpxThreadError::ActorGone);
                for attempt in 0..5 {
                    result = match client.call("session/new", params.clone(), None).await {
                        Ok(value) => match value.get("sessionId").and_then(|s| s.as_str()) {
                            Some(sid) => {
                                session_id = Some(sid.to_string());
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
                let result = client.call_with_updates("session/load", params, None).await;
                let outcome = match result {
                    Ok((_, updates)) => {
                        forward_updates(&updates, &event_tx);
                        session_id = Some(sid);
                        Ok(())
                    }
                    Err(e) => Err(e.into()),
                };
                let _ = resp.send(outcome);
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
                match client
                    .call_with_updates("session/prompt", params, None)
                    .await
                {
                    Ok((result, updates)) => {
                        forward_updates(&updates, &event_tx);
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
                let result = acpx_sessions::list(&client).await.map(|sessions| {
                    sessions
                        .into_iter()
                        .map(|s| RemoteThreadInfo {
                            acp_session_id: s.session_id,
                            agent_id: s.agent_id,
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
