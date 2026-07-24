//! One background actor per chat thread, talking to a bound acpx-server
//! over `acpx-client::raw::GatewayClient`. Method names/shapes
//! deliberately mirror `rui_acp_client::session_client::ThreadHandle`
//! (`open_session`/`send_prompt`/`list_sessions`/`shutdown`/`take_events`)
//! -- `panel-rust/src/agent_bridge.rs`'s actor-forwarding loop needed only
//! an import/type swap for the acpx cutover, not a rewrite, because of
//! this deliberate shape match.

use crate::gateway_actor::classify_raw_update;
use crate::protocol_types::AgentEvent;
use crate::protocol_types::{
    AgentRequestEvent, ConfigOptionInfo, ConfigOptionValue, SessionModeInfo, SessionModesEvent,
    TerminalOutputEvent,
};
use acpx_client::raw::ClientError;
use acpx_client::{AgentRequest, Gateway};
use std::path::PathBuf;
use std::sync::Arc;
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
    pub title: Option<String>,
    pub updated_at: Option<String>,
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
        /// `skill_injection_verification` phase: the `mcpServers` array
        /// `session/new` sends -- previously always `[]`. See
        /// [`AcpxThreadHandle::open_session`]'s doc comment.
        mcp_servers: Vec<serde_json::Value>,
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
        /// Same `mcpServers` addition as `OpenSession` above -- ACP's
        /// `LoadSessionRequest` requires this field too.
        mcp_servers: Vec<serde_json::Value>,
        resp: oneshot::Sender<Result<(), AcpxThreadError>>,
    },
    /// ACP's lighter `session/resume` reattachment path. Unlike
    /// `session/load`, it does not replay prior history.
    ReattachSession {
        session_id: String,
        cwd: PathBuf,
        resp: oneshot::Sender<Result<(), AcpxThreadError>>,
    },
    SendPrompt {
        text: String,
        resp: oneshot::Sender<Result<(), AcpxThreadError>>,
    },
    ListSessions {
        agent_id: Option<String>,
        resp: oneshot::Sender<Result<Vec<RemoteThreadInfo>, AcpxThreadError>>,
    },
    /// `profiles/list` -- every profile the bound gateway currently has
    /// registered, for a profile-picker UI. Read-only, no session
    /// binding involved -- safe to call before `OpenSession` has ever
    /// run on this handle.
    ListProfiles {
        resp: oneshot::Sender<Result<Vec<ProfileSummary>, AcpxThreadError>>,
    },
    /// Explicit, opt-in-only `session/close`. Deliberately **never**
    /// sent by `shutdown()`/`Drop` -- see this crate's module doc and
    /// `agent_bridge.rs`'s teardown path: window/process close must not
    /// imply session close by default, only an explicit caller
    /// (the per-thread "background" setting, `AgentBridge::close_thread`'s
    /// `background` parameter) should ever send this. `background: true`
    /// sends acpx-core's additive `_acpx.bg` extension field (see
    /// `LifecycleConfig::background_mode`'s doc comment) so the gateway
    /// treats this explicit close as a soft no-op -- the session (and
    /// its backend process) stays alive for a later resume, exactly as
    /// if the client had merely disconnected rather than explicitly
    /// closed.
    CloseSession {
        background: bool,
        resp: oneshot::Sender<Result<(), AcpxThreadError>>,
    },
    /// Real, stable v1 ACP `session/delete` -- permanently removes a
    /// session (backend-forwarded `Proxied` method, see `acpx-core::
    /// router`'s own doc comment on this method's classification).
    /// Deliberately requires an explicit caller, same "never sent by
    /// shutdown()/Drop" posture as [`Command::CloseSession`] -- in
    /// practice a caller should `session/close` first (this crate's own
    /// `acpx-core::router` rehydration test suite exercises exactly
    /// that close-then-delete order), but this command sends whatever
    /// `session_id` this handle currently knows regardless of whether
    /// it was ever explicitly closed first.
    DeleteSession {
        resp: oneshot::Sender<Result<(), AcpxThreadError>>,
    },
    /// `session/set_mode` -- see [`AcpxThreadHandle::set_mode`]'s doc
    /// comment.
    SetMode {
        mode_id: String,
        resp: oneshot::Sender<Result<(), AcpxThreadError>>,
    },
    /// `session/set_config_option` -- see [`AcpxThreadHandle::
    /// set_config_option`]'s doc comment.
    SetConfigOption {
        config_id: String,
        value: serde_json::Value,
        resp: oneshot::Sender<Result<(), AcpxThreadError>>,
    },
    /// `mcp_servers/list` -- every centrally-registered MCP server this
    /// gateway currently has, for a settings-gear MCP list UI. Read-only,
    /// no session binding involved, same "safe before `OpenSession`"
    /// shape as `ListProfiles`.
    ListMcpServers {
        resp: oneshot::Sender<Result<Vec<crate::protocol_types::McpServerEntry>, AcpxThreadError>>,
    },
    /// `mcp_servers/create`. `entry` must include a `"name"` field (the
    /// merge key `acpx-core::mcp_servers::McpServerStore` uses).
    CreateMcpServer {
        entry: serde_json::Value,
        resp: oneshot::Sender<Result<serde_json::Value, AcpxThreadError>>,
    },
    /// `mcp_servers/update` -- same payload shape as create.
    UpdateMcpServer {
        entry: serde_json::Value,
        resp: oneshot::Sender<Result<serde_json::Value, AcpxThreadError>>,
    },
    /// `mcp_servers/delete`.
    DeleteMcpServer {
        name: String,
        resp: oneshot::Sender<Result<(), AcpxThreadError>>,
    },
    /// `profiles/create`. `entry` must include a `"name"` field (the
    /// merge key `acpx-core::profile::ProfileStore` uses). See
    /// `acpx_client::ext::profiles::create`'s doc comment for the
    /// accepted payload shape (`name`, `agent_id`, `provider`,
    /// `key_ref`, `launch_overrides`, `mcp_servers`, and the
    /// create/update-only `secret` field).
    CreateProfile {
        entry: serde_json::Value,
        resp: oneshot::Sender<Result<serde_json::Value, AcpxThreadError>>,
    },
    /// `profiles/update` -- same payload shape as create.
    UpdateProfile {
        entry: serde_json::Value,
        resp: oneshot::Sender<Result<serde_json::Value, AcpxThreadError>>,
    },
    /// `profiles/delete`.
    DeleteProfile {
        name: String,
        resp: oneshot::Sender<Result<(), AcpxThreadError>>,
    },
    /// `agents/list` -- the registry's agent catalogue with this
    /// gateway's live detection status per entry, for an agent-catalog
    /// UI (installed/not-installed/runtime-missing chips). Read-only.
    ListAgents {
        resp:
            oneshot::Sender<Result<Vec<crate::protocol_types::AgentCatalogEntry>, AcpxThreadError>>,
    },
    /// `agents/status` for one agent id.
    AgentStatus {
        agent_id: String,
        resp: oneshot::Sender<Result<crate::protocol_types::AgentCatalogEntry, AcpxThreadError>>,
    },
    /// `agents/install` -- client-initiated installer trigger, see
    /// `acpx_client::ext::registry::install`'s doc comment: this blocks
    /// until the gateway's own synchronous install completes, no
    /// progress/job model yet.
    InstallAgent {
        agent_id: String,
        resp: oneshot::Sender<Result<serde_json::Value, AcpxThreadError>>,
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

/// Delivers the shared provider gateway to an actor created before its
/// connection has completed. The bridge uses this during restored-session
/// startup so cached UI state is available before network reconciliation.
#[derive(Clone)]
pub struct AcpxThreadGatewaySetter {
    gateway_tx: watch::Sender<Option<Arc<Gateway>>>,
}

impl AcpxThreadGatewaySetter {
    pub fn set_gateway(&self, gateway: Arc<Gateway>) {
        let _ = self.gateway_tx.send(Some(gateway));
    }
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
        self.open_session_with(cwd, None, Vec::new()).await
    }

    /// Same as [`Self::open_session`], but with an explicit `mcpServers`
    /// array (`skill_injection_verification` phase) -- the shared
    /// implementation both [`Self::open_session`] and [`Self::
    /// open_session_with_profile`] delegate to.
    pub async fn open_session_with(
        &self,
        cwd: impl Into<PathBuf>,
        profile: Option<String>,
        mcp_servers: Vec<serde_json::Value>,
    ) -> Result<String, AcpxThreadError> {
        let cwd = cwd.into();
        self.call(|resp| Command::OpenSession {
            cwd,
            profile,
            mcp_servers,
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
        mcp_servers: Vec<serde_json::Value>,
    ) -> Result<String, AcpxThreadError> {
        self.open_session_with(cwd, Some(profile.into()), mcp_servers)
            .await
    }

    /// `session/load` against an already-known gateway session id --
    /// see [`Command::ResumeSession`]'s doc comment.
    pub async fn resume_session(
        &self,
        session_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
        mcp_servers: Vec<serde_json::Value>,
    ) -> Result<(), AcpxThreadError> {
        let session_id = session_id.into();
        let cwd = cwd.into();
        self.call(|resp| Command::ResumeSession {
            session_id,
            cwd,
            mcp_servers,
            resp,
        })
        .await
    }

    /// Attaches this client connection to an existing session with ACP's
    /// no-history-replay `session/resume` operation.
    pub async fn reattach_session(
        &self,
        session_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
    ) -> Result<(), AcpxThreadError> {
        let session_id = session_id.into();
        let cwd = cwd.into();
        self.call(|resp| Command::ReattachSession {
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
        self.call(|resp| Command::ListSessions {
            agent_id: None,
            resp,
        })
        .await
    }

    /// Typed, per-backend `session/list`, preserving ACP `title` and
    /// `updatedAt` metadata for cache reconciliation.
    pub async fn list_sessions_for_agent(
        &self,
        agent_id: impl Into<String>,
    ) -> Result<Vec<RemoteThreadInfo>, AcpxThreadError> {
        let agent_id = Some(agent_id.into());
        self.call(|resp| Command::ListSessions { agent_id, resp })
            .await
    }

    /// `profiles/list` against this thread's bound gateway -- what a
    /// profile-picker UI populates its choices from. Safe to call before
    /// `open_session`/`open_session_with_profile` (no session-dependent
    /// state involved).
    pub async fn list_profiles(&self) -> Result<Vec<ProfileSummary>, AcpxThreadError> {
        self.call(|resp| Command::ListProfiles { resp }).await
    }

    /// Explicit `session/close` -- opt-in only, see [`Command::CloseSession`].
    pub async fn close_session(&self, background: bool) -> Result<(), AcpxThreadError> {
        self.call(|resp| Command::CloseSession { background, resp })
            .await
    }

    /// Real `session/delete` -- opt-in only, see [`Command::
    /// DeleteSession`]'s doc comment on ordering vs. `close_session`.
    pub async fn delete_session(&self) -> Result<(), AcpxThreadError> {
        self.call(|resp| Command::DeleteSession { resp }).await
    }

    /// `session/set_mode` against this thread's bound session --
    /// `mode_id` must be one of the ids [`AgentEvent::SessionModes`]
    /// most recently advertised (a real backend rejects an unknown
    /// mode id, per `session-modes` schema's own "must be one of the
    /// modes advertised" wording). Fails with [`AcpxThreadError::
    /// NoActiveSession`] if no session is open yet on this handle --
    /// mode selection is meaningless before `session/new`/`session/
    /// load` has bound one.
    pub async fn set_mode(&self, mode_id: impl Into<String>) -> Result<(), AcpxThreadError> {
        let mode_id = mode_id.into();
        self.call(|resp| Command::SetMode { mode_id, resp }).await
    }

    /// `session/set_config_option` against this thread's bound session
    /// -- `config_id` must be one of the ids [`AgentEvent::
    /// ConfigOptions`] most recently advertised, `value` one of that
    /// option's own `options[].value` entries for a `select`-kind
    /// option (see `ConfigOptionInfo::kind`'s doc comment on other
    /// kinds). The gateway forwards the full updated `configOptions[]`
    /// list in this call's own response -- the run loop re-emits it as
    /// a fresh [`AgentEvent::ConfigOptions`] the same way a live
    /// `config_option_update` notification would, so callers only need
    /// to watch `events`, not this method's `Ok(())` return, to learn
    /// the option's new resolved state (which may differ from `value`
    /// verbatim, and may also change *other* options' current values
    /// or availability -- both real, documented ACP behaviors, not a
    /// defect in this wrapper).
    pub async fn set_config_option(
        &self,
        config_id: impl Into<String>,
        value: serde_json::Value,
    ) -> Result<(), AcpxThreadError> {
        let config_id = config_id.into();
        self.call(|resp| Command::SetConfigOption {
            config_id,
            value,
            resp,
        })
        .await
    }

    /// `mcp_servers/list` against this thread's bound gateway -- what a
    /// settings-gear MCP server list populates from. Safe before
    /// `open_session`/`open_session_with_profile` (no session-dependent
    /// state), same shape as `list_profiles`. Returns typed `McpServer
    /// Entry` rows (Phase 2 step 3: "no Slint-adjacent code sees raw
    /// JSON"), parsed once here rather than left for `panel-rust::
    /// models` to hand-parse.
    pub async fn list_mcp_servers(
        &self,
    ) -> Result<Vec<crate::protocol_types::McpServerEntry>, AcpxThreadError> {
        self.call(|resp| Command::ListMcpServers { resp }).await
    }

    /// `mcp_servers/create`. `entry` must include a `"name"` field.
    pub async fn create_mcp_server(
        &self,
        entry: serde_json::Value,
    ) -> Result<serde_json::Value, AcpxThreadError> {
        self.call(|resp| Command::CreateMcpServer { entry, resp })
            .await
    }

    /// `mcp_servers/update` -- same payload shape as `create_mcp_server`.
    pub async fn update_mcp_server(
        &self,
        entry: serde_json::Value,
    ) -> Result<serde_json::Value, AcpxThreadError> {
        self.call(|resp| Command::UpdateMcpServer { entry, resp })
            .await
    }

    /// `mcp_servers/delete`.
    pub async fn delete_mcp_server(&self, name: impl Into<String>) -> Result<(), AcpxThreadError> {
        let name = name.into();
        self.call(|resp| Command::DeleteMcpServer { name, resp })
            .await
    }

    /// `profiles/create`. `entry` must include a `"name"` field, same
    /// shape `AcpxThreadHandle::list_profiles`'s `ProfileSummary`
    /// narrows down for read -- this is the raw create/update payload,
    /// so it accepts every field `acpx-core::profile::Profile` supports
    /// (see [`Command::CreateProfile`]'s doc comment).
    pub async fn create_profile(
        &self,
        entry: serde_json::Value,
    ) -> Result<serde_json::Value, AcpxThreadError> {
        self.call(|resp| Command::CreateProfile { entry, resp })
            .await
    }

    /// `profiles/update` -- same payload shape as [`Self::create_profile`].
    pub async fn update_profile(
        &self,
        entry: serde_json::Value,
    ) -> Result<serde_json::Value, AcpxThreadError> {
        self.call(|resp| Command::UpdateProfile { entry, resp })
            .await
    }

    /// `profiles/delete`.
    pub async fn delete_profile(&self, name: impl Into<String>) -> Result<(), AcpxThreadError> {
        let name = name.into();
        self.call(|resp| Command::DeleteProfile { name, resp })
            .await
    }

    /// `agents/list` -- the registry's agent catalogue with this
    /// gateway's live detection status per entry. Safe before a session
    /// is open, same shape as `list_profiles`/`list_mcp_servers`. Typed
    /// `AgentCatalogEntry` rows, same reasoning as `list_mcp_servers`.
    pub async fn list_agents(
        &self,
    ) -> Result<Vec<crate::protocol_types::AgentCatalogEntry>, AcpxThreadError> {
        self.call(|resp| Command::ListAgents { resp }).await
    }

    /// `agents/status` for one agent id. Typed `AgentCatalogEntry`, same
    /// reasoning as `list_agents`.
    pub async fn agent_status(
        &self,
        agent_id: impl Into<String>,
    ) -> Result<crate::protocol_types::AgentCatalogEntry, AcpxThreadError> {
        let agent_id = agent_id.into();
        self.call(|resp| Command::AgentStatus { agent_id, resp })
            .await
    }

    /// `agents/install` -- client-initiated installer trigger. Blocks
    /// until the gateway's own synchronous install completes; see
    /// [`Command::InstallAgent`]'s doc comment.
    pub async fn install_agent(
        &self,
        agent_id: impl Into<String>,
    ) -> Result<serde_json::Value, AcpxThreadError> {
        let agent_id = agent_id.into();
        self.call(|resp| Command::InstallAgent { agent_id, resp })
            .await
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

/// Spawn a standalone thread actor, connecting its own dedicated
/// `Gateway` to `base_url` (e.g. `http://127.0.0.1:8790`) first. Kept
/// for standalone/test callers that just want one thread against one
/// URL with no connection sharing -- internally this is now just
/// "connect once, then delegate to [`spawn_acpx_thread_with_gateway`]",
/// so a caller that *does* want to share one connection across several
/// threads (see that function's doc comment -- this crate's own
/// `AgentBridge` is exactly that caller) should call it directly
/// instead of this one.
pub fn spawn_acpx_thread(base_url: impl Into<String>) -> AcpxThreadHandle {
    let base_url = base_url.into();
    let (handle, gateway_setter) = spawn_acpx_thread_pending();
    tokio::spawn(async move {
        let gateway = Arc::new(Gateway::connect(base_url).await);
        gateway_setter.set_gateway(gateway);
    });
    handle
}

/// Spawn a thread actor that talks over an **already-connected**, shared
/// `Gateway` -- the plan's "one shared `acpx_client::Gateway` held by
/// `AgentBridge`" design (Phase 2 of `chat-panel-production-ui/
/// execution-plan.md`): every thread bound to the same provider/gateway
/// URL passes in the *same* `Arc<Gateway>` (one real WS/HTTP connection
/// per provider, not per thread), while each thread still gets its own
/// independent actor/command-loop/cancel-worker/respond-worker set, so
/// per-thread command serialization (the actual reason those three
/// workers exist -- see [`AcpxThreadHandle::respond_agent_request`]'s
/// doc comment) is unaffected by connection sharing. Safe because
/// `Gateway`'s own transport already multiplexes concurrent in-flight
/// requests by JSON-RPC id (`GatewayWsClient`'s `pending: Mutex<HashMap
/// <i64, oneshot::Sender<..>>>`, confirmed by reading `acpx-client::
/// ws.rs` directly before relying on it) -- a slow prompt on one thread
/// never blocks another thread's cancel/respond/prompt call from
/// completing on the same shared connection.
pub fn spawn_acpx_thread_with_gateway(gateway: Arc<Gateway>) -> AcpxThreadHandle {
    let (handle, gateway_setter) = spawn_acpx_thread_pending();
    gateway_setter.set_gateway(gateway);
    handle
}

/// Spawn an actor immediately, with its gateway delivered later through the
/// returned setter. Commands may be submitted before delivery; its workers
/// wait for the shared gateway rather than failing the command.
pub fn spawn_acpx_thread_with_delayed_gateway() -> (AcpxThreadHandle, AcpxThreadGatewaySetter) {
    spawn_acpx_thread_pending()
}

/// Shared plumbing for both public constructors above: sets up every
/// channel and spawns all three worker tasks immediately, but each
/// worker's very first action is to await a `Gateway` handed to it
/// through a `oneshot` -- `spawn_acpx_thread` fills that oneshot only
/// once its own `Gateway::connect` resolves; `spawn_acpx_thread_with_
/// gateway` fills it immediately with the caller's already-connected
/// `Arc<Gateway>`. A `broadcast`-backed oneshot substitute (a `watch`
/// channel seeded once) since four independent tasks (main loop, cancel
/// worker, respond worker, plus this function's own caller) all need to
/// read the same connected `Gateway` exactly once.
fn spawn_acpx_thread_pending() -> (AcpxThreadHandle, AcpxThreadGatewaySetter) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (cancel_tx, cancel_rx) = mpsc::unbounded_channel();
    let (respond_tx, respond_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (session_tx, session_rx) = watch::channel(None::<String>);
    let (gateway_tx, gateway_rx) = watch::channel(None::<Arc<Gateway>>);
    tokio::spawn(run_thread_actor(
        gateway_rx.clone(),
        cmd_rx,
        event_tx,
        session_tx,
    ));
    tokio::spawn(run_cancel_worker(gateway_rx.clone(), cancel_rx, session_rx));
    tokio::spawn(run_respond_worker(gateway_rx, respond_rx));
    (
        AcpxThreadHandle {
            cmd_tx,
            cancel_tx,
            respond_tx,
            events: event_rx,
        },
        AcpxThreadGatewaySetter { gateway_tx },
    )
}

/// Awaits `gateway_rx`'s first non-`None` value -- see
/// `spawn_acpx_thread_pending`'s doc comment on why every worker starts
/// this way instead of connecting itself.
async fn await_gateway(gateway_rx: &mut watch::Receiver<Option<Arc<Gateway>>>) -> Arc<Gateway> {
    loop {
        if let Some(gateway) = gateway_rx.borrow().clone() {
            return gateway;
        }
        if gateway_rx.changed().await.is_err() {
            // Sender dropped without ever sending -- only possible if
            // `spawn_acpx_thread`'s own `Gateway::connect` task panicked
            // before sending. Retry the borrow one last time in case of
            // a benign race, else this worker simply never proceeds
            // (matches every other unrecoverable-setup-failure path in
            // this actor, which likewise just stalls rather than
            // panicking the whole process).
            if let Some(gateway) = gateway_rx.borrow().clone() {
                return gateway;
            }
            std::future::pending::<()>().await;
        }
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
    mut gateway_rx: watch::Receiver<Option<Arc<Gateway>>>,
    mut respond_rx: mpsc::UnboundedReceiver<RespondCommand>,
) {
    let mut client: Option<Arc<Gateway>> = None;
    while let Some(cmd) = respond_rx.recv().await {
        let client = match &client {
            Some(client) => client,
            None => client.insert(await_gateway(&mut gateway_rx).await),
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
        } else if let Some(event) = parse_capability_update(update) {
            let _ = event_tx.send(event);
        }
    }
}

/// Recognizes a live `current_mode_update`/`config_option_update`
/// `session/update` notification (see [`AgentEvent::CurrentModeChanged`]/
/// [`AgentEvent::ConfigOptions`]'s doc comments for the wire shapes) and
/// maps it to the matching event. `None` for anything else, same
/// "operate on the raw JSON shape, tolerate unrecognized input"
/// convention `classify_raw_update`/`parse_terminal_output` both follow
/// -- called as a `classify_raw_update` fallback in [`forward_updates`]
/// so a session-capability notification isn't silently dropped just
/// because it isn't a chat-message-shaped update.
fn parse_capability_update(update: &serde_json::Value) -> Option<AgentEvent> {
    if update.get("method").and_then(|m| m.as_str()) != Some("session/update") {
        return None;
    }
    let session_update = update.get("params")?.get("update")?;
    match session_update
        .get("sessionUpdate")
        .and_then(|k| k.as_str())?
    {
        "usage_update" => {
            let used = session_update.get("used").and_then(|v| v.as_i64()).unwrap_or(0);
            let size = session_update.get("size").and_then(|v| v.as_i64()).unwrap_or(0);
            Some(AgentEvent::UsageUpdate { used, size })
        }
        "current_mode_update" => {
            let mode_id = session_update.get("currentModeId")?.as_str()?.to_string();
            Some(AgentEvent::CurrentModeChanged(mode_id))
        }
        "config_option_update" => {
            let options = parse_config_options(session_update.get("configOptions")?)?;
            Some(AgentEvent::ConfigOptions(options))
        }
        _ => None,
    }
}

/// Parses a `session/new`/`session/load`/`session/resume` response's
/// (or a live `config_option_update` notification's) `modes` field into
/// a [`SessionModesEvent`] -- `{currentModeId, availableModes: [{id,
/// name, description?}]}` per agentclientprotocol.com's real schema
/// (verified directly, not assumed -- see this crate's own e2e coverage
/// test for the exact fixture this was checked against). `None` if
/// `modes` is absent/null (an agent that doesn't advertise modes at
/// all) or missing `currentModeId`/`availableModes` entirely; an agent
/// advertising an *empty* `availableModes` array still produces
/// `Some(..)` with an empty `available` -- that is meaningfully
/// different from "no modes field at all" for a UI deciding whether to
/// show a selector at all.
fn parse_session_modes(modes: &serde_json::Value) -> Option<SessionModesEvent> {
    if modes.is_null() {
        return None;
    }
    let current_mode_id = modes.get("currentModeId")?.as_str()?.to_string();
    let available = modes
        .get("availableModes")?
        .as_array()?
        .iter()
        .filter_map(|mode| {
            Some(SessionModeInfo {
                id: mode.get("id")?.as_str()?.to_string(),
                name: mode
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_string(),
                description: mode
                    .get("description")
                    .and_then(|d| d.as_str())
                    .map(str::to_string),
            })
        })
        .collect();
    Some(SessionModesEvent {
        current_mode_id,
        available,
    })
}

/// Parses a `configOptions[]` array (a `session/new`/`session/load`/
/// `session/resume` response's `configOptions` field, a live `config_
/// option_update` notification's `configOptions`, or a `session/set_
/// config_option` response's `configOptions`) into this crate's
/// [`ConfigOptionInfo`] vocabulary -- `{id, name, description?,
/// category?, type, currentValue?, options?}` per
/// agentclientprotocol.com/protocol/session-config-options's documented
/// response shape. `None` if `list` isn't a JSON array at all; an entry
/// missing `id` is skipped (nothing usable to key a `session/set_
/// config_option` call on) rather than failing the whole list, same
/// per-entry tolerance `parse_session_modes` applies to `availableModes`.
fn parse_config_options(list: &serde_json::Value) -> Option<Vec<ConfigOptionInfo>> {
    let entries = list.as_array()?;
    Some(
        entries
            .iter()
            .filter_map(|entry| {
                let id = entry.get("id")?.as_str()?.to_string();
                let options = entry
                    .get("options")
                    .and_then(|o| o.as_array())
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(|value| {
                                Some(ConfigOptionValue {
                                    value: value.get("value")?.as_str()?.to_string(),
                                    name: value
                                        .get("name")
                                        .and_then(|n| n.as_str())
                                        .unwrap_or_default()
                                        .to_string(),
                                    description: value
                                        .get("description")
                                        .and_then(|d| d.as_str())
                                        .map(str::to_string),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Some(ConfigOptionInfo {
                    name: entry
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or(&id)
                        .to_string(),
                    id,
                    description: entry
                        .get("description")
                        .and_then(|d| d.as_str())
                        .map(str::to_string),
                    category: entry
                        .get("category")
                        .and_then(|c| c.as_str())
                        .map(str::to_string),
                    kind: entry
                        .get("type")
                        .and_then(|t| t.as_str())
                        .unwrap_or("select")
                        .to_string(),
                    current_value: entry
                        .get("currentValue")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    options,
                })
            })
            .collect(),
    )
}

/// Emits [`AgentEvent::SessionModes`]/[`AgentEvent::ConfigOptions`] for
/// whichever of a `session/new`/`session/load`/`session/resume`
/// response's `modes`/`configOptions` fields are actually present --
/// shared by [`run_thread_actor`]'s `OpenSession`/`ResumeSession` arms
/// so both the fresh-session and resumed-session paths advertise
/// capability state identically.
fn emit_capability_events(value: &serde_json::Value, event_tx: &mpsc::UnboundedSender<AgentEvent>) {
    if let Some(modes) = value.get("modes").and_then(parse_session_modes) {
        let _ = event_tx.send(AgentEvent::SessionModes(modes));
    }
    if let Some(options) = value.get("configOptions").and_then(parse_config_options) {
        let _ = event_tx.send(AgentEvent::ConfigOptions(options));
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
fn spawn_out_of_band_notification_forwarder(client: Arc<Gateway>, event_tx: mpsc::UnboundedSender<AgentEvent>) {
    if client.subscribe().is_none() {
        // HTTP-degraded mode with no live push channel at all -- matches
        // every other live-only code path in this crate; there is no
        // connection to reconnect either, so this stays a no-op rather
        // than looping on a `reconnect()` that has nothing to recover.
        return;
    }
    tokio::spawn(async move {
        // Outer loop: (re-)subscribe against whatever `Gateway` connection
        // is currently live, forward from it until it dies, then attempt
        // to reconnect (Gateway::reconnect's own bounded retries/timeouts)
        // and resubscribe -- rather than the task silently ending forever
        // the first time the connection drops (this exact gap is why a
        // killed/restarted gateway process used to permanently strand a
        // live thread with no recovery short of restarting the whole app).
        loop {
            let Some(mut notifications) = client.subscribe() else {
                if !client.reconnect().await {
                    return;
                }
                continue;
            };
            loop {
                tokio::select! {
                    update = notifications.recv() => {
                        match update {
                            Ok(update) => {
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
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                    // Notices the connection dying even during a quiet
                    // period with no notifications in flight to
                    // otherwise trigger this via `recv()`'s own Closed.
                    _ = client.wait_for_disconnect() => break,
                }
            }
            if !client.reconnect().await {
                return;
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
    let truncated = params
        .get("truncated")
        .and_then(|t| t.as_bool())
        .unwrap_or(false);
    let exit_status = params
        .get("exitStatus")
        .filter(|v| !v.is_null())
        .map(|status| {
            (
                status
                    .get("exitCode")
                    .and_then(|c| c.as_i64())
                    .map(|c| c as i32),
                status
                    .get("signal")
                    .and_then(|s| s.as_i64())
                    .map(|s| s as i32),
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
    mut gateway_rx: watch::Receiver<Option<Arc<Gateway>>>,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    session_tx: watch::Sender<Option<String>>,
) {
    let client = await_gateway(&mut gateway_rx).await;
    spawn_out_of_band_notification_forwarder(Arc::clone(&client), event_tx.clone());
    let (live_tx, mut live_rx) = mpsc::unbounded_channel();
    if client.subscribe().is_some() {
        // See spawn_out_of_band_notification_forwarder's doc comment --
        // same "reconnect and resubscribe rather than die forever on the
        // first disconnect" shape, for this actor's own live_rx feed
        // (session/update frames `forward_updates` below drains).
        let live_client = Arc::clone(&client);
        tokio::spawn(async move {
            loop {
                let Some(mut live_notifications) = live_client.subscribe() else {
                    if !live_client.reconnect().await {
                        return;
                    }
                    continue;
                };
                loop {
                    tokio::select! {
                        update = live_notifications.recv() => {
                            match update {
                                Ok(update) => { let _ = live_tx.send(update); }
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                            }
                        }
                        _ = live_client.wait_for_disconnect() => break,
                    }
                }
                if !live_client.reconnect().await {
                    return;
                }
            }
        });
    }
    let mut session_id: Option<String> = None;

    // Keep forwarding live session updates even while the user is idle.
    // `session/load` is allowed to replay after its RPC response; waiting
    // only inside a command handler stranded those late frames in
    // `live_rx` until the next user action.
    loop {
        let cmd = tokio::select! {
            Some(update) = live_rx.recv() => {
                forward_updates(&[update], &event_tx);
                continue;
            }
            command = cmd_rx.recv() => match command {
                Some(command) => command,
                None => break,
            },
        };
        match cmd {
            Command::OpenSession {
                cwd,
                profile,
                mcp_servers,
                resp,
            } => {
                let params = serde_json::json!({
                    "cwd": cwd.to_string_lossy(),
                    "mcpServers": mcp_servers,
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
                                emit_capability_events(&value, &event_tx);
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
                mcp_servers,
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
                    "mcpServers": mcp_servers,
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
                        Ok((value, updates)) => {
                            emit_capability_events(&value, &event_tx);
                            forward_updates(&updates, &event_tx);
                            // A session/load replay is allowed to start
                            // before its RPC response, but a busy real
                            // host can schedule the WS reader just after
                            // that response. Wait through one short UI
                            // frame budget before declaring the replay
                            // empty, then drain the rest already queued.
                            if let Ok(Some(update)) = tokio::time::timeout(
                                std::time::Duration::from_millis(500),
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
            Command::ReattachSession {
                session_id: sid,
                cwd,
                resp,
            } => {
                let params = serde_json::json!({
                    "sessionId": sid,
                    "cwd": cwd.to_string_lossy(),
                });
                let mut result = Err(AcpxThreadError::ActorGone);
                for attempt in 0..5 {
                    result = match client
                        .call_with_updates("session/resume", params.clone(), None)
                        .await
                    {
                        Ok((value, updates)) => {
                            emit_capability_events(&value, &event_tx);
                            forward_updates(&updates, &event_tx);
                            session_id = Some(sid.clone());
                            let _ = session_tx.send(Some(sid.clone()));
                            Ok(())
                        }
                        Err(error) => Err(error.into()),
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
                        // A resumed WS subscription can receive a burst of
                        // final notifications just after the prompt response.
                        // Keep draining until the stream is briefly quiet,
                        // bounded by a hard deadline so a bad backend can
                        // never hold the turn open indefinitely.
                        let deadline =
                            tokio::time::Instant::now() + std::time::Duration::from_millis(500);
                        let mut wait = std::time::Duration::from_millis(250);
                        while tokio::time::Instant::now() < deadline {
                            let remaining =
                                deadline.saturating_duration_since(tokio::time::Instant::now());
                            match tokio::time::timeout(wait.min(remaining), live_rx.recv()).await {
                                Ok(Some(update)) => {
                                    forward_updates(&[update], &event_tx);
                                    wait = std::time::Duration::from_millis(75);
                                }
                                Ok(None) | Err(_) => break,
                            }
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
            Command::ListSessions { agent_id, resp } => {
                let result = match agent_id {
                    Some(agent_id) => {
                        acpx_client::ext::sessions::list_for_agent(&client, &agent_id).await
                    }
                    None => acpx_client::ext::sessions::list_gateway(&client).await,
                }
                .map(|sessions| {
                    sessions
                        .into_iter()
                        .map(|session| RemoteThreadInfo {
                            acp_session_id: session.session_id,
                            agent_id: session.agent_id,
                            title: session.title,
                            updated_at: session.updated_at,
                        })
                        .collect()
                });
                let _ = resp.send(result.map_err(Into::into));
            }
            Command::ListProfiles { resp } => {
                let result = client
                    .call("profiles/list", serde_json::json!({}), None)
                    .await
                    .map(|value| {
                        value["profiles"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default()
                            .into_iter()
                            .filter_map(|profile| {
                                Some(ProfileSummary {
                                    name: profile.get("name")?.as_str()?.to_owned(),
                                    agent_id: profile
                                        .get("agent_id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default()
                                        .to_owned(),
                                    allow_terminal_access: profile
                                        .get("allow_terminal_access")
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false),
                                    allow_fs_access: profile
                                        .get("allow_fs_access")
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false),
                                })
                            })
                            .collect()
                    });
                let _ = resp.send(result.map_err(Into::into));
            }
            Command::CloseSession { background, resp } => {
                let Some(sid) = session_id.clone() else {
                    // Never opened -- closing a session that was never
                    // opened on this handle is a no-op success, not an
                    // error (nothing gateway-side to close).
                    let _ = resp.send(Ok(()));
                    continue;
                };
                let mut params = serde_json::json!({ "sessionId": sid });
                if background {
                    params["_acpx"] = serde_json::json!({ "bg": true });
                }
                let result = client.call("session/close", params, None).await;
                let _ = resp.send(result.map(|_| ()).map_err(Into::into));
            }
            Command::DeleteSession { resp } => {
                let Some(sid) = session_id.clone() else {
                    // Never opened -- nothing gateway-side to delete.
                    let _ = resp.send(Ok(()));
                    continue;
                };
                let params = serde_json::json!({ "sessionId": sid });
                let result = client.call("session/delete", params, None).await;
                let _ = resp.send(result.map(|_| ()).map_err(Into::into));
            }
            Command::SetMode { mode_id, resp } => {
                let Some(sid) = session_id.clone() else {
                    let _ = resp.send(Err(AcpxThreadError::NoActiveSession));
                    continue;
                };
                let params = serde_json::json!({ "sessionId": sid, "modeId": mode_id });
                let result = client.call("session/set_mode", params, None).await;
                let _ = resp.send(result.map(|_| ()).map_err(Into::into));
            }
            Command::SetConfigOption {
                config_id,
                value,
                resp,
            } => {
                let Some(sid) = session_id.clone() else {
                    let _ = resp.send(Err(AcpxThreadError::NoActiveSession));
                    continue;
                };
                let params = serde_json::json!({
                    "sessionId": sid,
                    "configId": config_id,
                    "value": value,
                });
                let result = client.call("session/set_config_option", params, None).await;
                match result {
                    Ok(value) => {
                        // The response carries the full updated
                        // `configOptions[]` -- see `set_config_option`'s
                        // own doc comment on why this crate re-emits it
                        // as a fresh event rather than leaving the
                        // caller to inspect this call's own `Ok(())`.
                        emit_capability_events(&value, &event_tx);
                        let _ = resp.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = resp.send(Err(e.into()));
                    }
                }
            }
            Command::Shutdown => break,
            Command::ListMcpServers { resp } => {
                // Deliberately `client.call(...)` (the transport-neutral
                // `Gateway` facade this actor already holds), not
                // `acpx_client::ext::mcp_servers::list` -- that helper is
                // typed against the raw HTTP-only `GatewayClient`, which
                // would silently drop this actor onto HTTP even in a live
                // WS session. Same reasoning `ListProfiles`/`ListSessions`
                // above already follow.
                let result = client
                    .call("mcp_servers/list", serde_json::json!({}), None)
                    .await
                    .map(|value| {
                        value
                            .get("servers")
                            .and_then(|s| s.as_array())
                            .map(|entries| {
                                entries
                                    .iter()
                                    .filter_map(crate::protocol_types::McpServerEntry::from_json)
                                    .collect()
                            })
                            .unwrap_or_default()
                    });
                let _ = resp.send(result.map_err(Into::into));
            }
            Command::CreateMcpServer { entry, resp } => {
                let result = client.call("mcp_servers/create", entry, None).await;
                let _ = resp.send(result.map_err(Into::into));
            }
            Command::UpdateMcpServer { entry, resp } => {
                let result = client.call("mcp_servers/update", entry, None).await;
                let _ = resp.send(result.map_err(Into::into));
            }
            Command::DeleteMcpServer { name, resp } => {
                let result = client
                    .call(
                        "mcp_servers/delete",
                        serde_json::json!({ "name": name }),
                        None,
                    )
                    .await
                    .map(|_| ());
                let _ = resp.send(result.map_err(Into::into));
            }
            Command::CreateProfile { entry, resp } => {
                let result = client.call("profiles/create", entry, None).await;
                let _ = resp.send(result.map_err(Into::into));
            }
            Command::UpdateProfile { entry, resp } => {
                let result = client.call("profiles/update", entry, None).await;
                let _ = resp.send(result.map_err(Into::into));
            }
            Command::DeleteProfile { name, resp } => {
                let result = client
                    .call("profiles/delete", serde_json::json!({ "name": name }), None)
                    .await
                    .map(|_| ());
                let _ = resp.send(result.map_err(Into::into));
            }
            Command::ListAgents { resp } => {
                let result = client
                    .call("agents/list", serde_json::json!({}), None)
                    .await
                    .map(|value| {
                        value
                            .get("agents")
                            .and_then(|a| a.as_array())
                            .map(|entries| {
                                entries
                                    .iter()
                                    .filter_map(crate::protocol_types::AgentCatalogEntry::from_json)
                                    .collect()
                            })
                            .unwrap_or_default()
                    });
                let _ = resp.send(result.map_err(Into::into));
            }
            Command::AgentStatus { agent_id, resp } => {
                let result = client
                    .call("agents/status", serde_json::json!({ "id": agent_id }), None)
                    .await
                    .and_then(|value| {
                        crate::protocol_types::AgentCatalogEntry::from_json(&value)
                            .ok_or_else(|| acpx_client::raw::ClientError::MalformedResponse)
                    });
                let _ = resp.send(result.map_err(Into::into));
            }
            Command::InstallAgent { agent_id, resp } => {
                let result = client
                    .call(
                        "agents/install",
                        serde_json::json!({ "id": agent_id }),
                        None,
                    )
                    .await;
                let _ = resp.send(result.map_err(Into::into));
            }
        }
    }
}

async fn run_cancel_worker(
    mut gateway_rx: watch::Receiver<Option<Arc<Gateway>>>,
    mut cancel_rx: mpsc::UnboundedReceiver<oneshot::Sender<Result<(), AcpxThreadError>>>,
    session_rx: watch::Receiver<Option<String>>,
) {
    let client = await_gateway(&mut gateway_rx).await;
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

/// A profile the bound gateway currently has registered, as returned by
/// `profiles/list` -- narrowed to the fields a profile-picker UI needs
/// (name to display/select, and the two capability gates that determine
/// whether picking this profile actually unlocks terminal/fs approval
/// cards, so the picker can show that inline rather than the user
/// discovering it only after a request silently auto-rejects).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSummary {
    pub name: String,
    pub agent_id: String,
    pub allow_terminal_access: bool,
    pub allow_fs_access: bool,
}

#[cfg(test)]
mod capability_parsing_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_session_modes_reads_current_and_available() {
        let modes = json!({
            "currentModeId": "ask",
            "availableModes": [
                {"id": "ask", "name": "Ask"},
                {"id": "code", "name": "Code", "description": "Autonomous coding"}
            ]
        });
        let parsed = parse_session_modes(&modes).expect("parses");
        assert_eq!(parsed.current_mode_id, "ask");
        assert_eq!(parsed.available.len(), 2);
        assert_eq!(parsed.available[1].id, "code");
        assert_eq!(
            parsed.available[1].description.as_deref(),
            Some("Autonomous coding")
        );
    }

    #[test]
    fn parse_session_modes_is_none_for_null_or_missing_fields() {
        assert!(parse_session_modes(&serde_json::Value::Null).is_none());
        assert!(parse_session_modes(&json!({"currentModeId": "ask"})).is_none());
        assert!(parse_session_modes(&json!({"availableModes": []})).is_none());
    }

    #[test]
    fn parse_session_modes_accepts_an_empty_available_list() {
        let modes = json!({"currentModeId": "ask", "availableModes": []});
        let parsed = parse_session_modes(&modes).expect("parses");
        assert_eq!(parsed.current_mode_id, "ask");
        assert!(parsed.available.is_empty());
    }

    #[test]
    fn parse_config_options_reads_select_options_and_current_value() {
        let options = json!([{
            "id": "model",
            "name": "Model",
            "description": "Which model to use",
            "category": "model",
            "type": "select",
            "currentValue": "gpt-5",
            "options": [
                {"value": "gpt-5", "name": "GPT-5"},
                {"value": "gpt-5-mini", "name": "GPT-5 mini", "description": "Cheaper"}
            ]
        }]);
        let parsed = parse_config_options(&options).expect("parses");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "model");
        assert_eq!(parsed[0].kind, "select");
        assert_eq!(parsed[0].current_value.as_deref(), Some("gpt-5"));
        assert_eq!(parsed[0].options.len(), 2);
        assert_eq!(parsed[0].options[1].description.as_deref(), Some("Cheaper"));
    }

    #[test]
    fn parse_config_options_skips_entries_without_an_id_but_keeps_the_rest() {
        let options = json!([
            {"name": "no id here"},
            {"id": "model", "currentValue": "gpt-5"}
        ]);
        let parsed = parse_config_options(&options).expect("parses");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "model");
        // Falls back to `id` when `name` is absent, and defaults `kind`
        // to "select" (every real backend observed in this workspace
        // only ever emits that kind today).
        assert_eq!(parsed[0].name, "model");
        assert_eq!(parsed[0].kind, "select");
    }

    #[test]
    fn parse_config_options_is_none_for_a_non_array_value() {
        assert!(parse_config_options(&json!({"id": "model"})).is_none());
    }

    #[test]
    fn parse_capability_update_recognizes_current_mode_update() {
        let update = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": "s1", "update": {
                "sessionUpdate": "current_mode_update",
                "currentModeId": "code"
            }}
        });
        match parse_capability_update(&update).expect("parses") {
            AgentEvent::CurrentModeChanged(id) => assert_eq!(id, "code"),
            other => panic!("expected CurrentModeChanged, got {other:?}"),
        }
    }

    #[test]
    fn parse_capability_update_recognizes_config_option_update() {
        let update = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": "s1", "update": {
                "sessionUpdate": "config_option_update",
                "configOptions": [{"id": "model", "currentValue": "gpt-5-mini"}]
            }}
        });
        match parse_capability_update(&update).expect("parses") {
            AgentEvent::ConfigOptions(options) => {
                assert_eq!(options.len(), 1);
                assert_eq!(options[0].current_value.as_deref(), Some("gpt-5-mini"));
            }
            other => panic!("expected ConfigOptions, got {other:?}"),
        }
    }

    #[test]
    fn parse_capability_update_ignores_unrelated_session_updates() {
        let update = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": "s1", "update": {"sessionUpdate": "plan"}}
        });
        assert!(parse_capability_update(&update).is_none());
    }
}
