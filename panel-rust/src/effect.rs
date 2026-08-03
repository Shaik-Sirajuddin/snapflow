//! `tea-slint-model` Phase 1: side-effect descriptions returned by
//! `update()` and executed by `EffectExecutor` (Phase 4) -- `update()`
//! itself never performs I/O, it only describes what should happen. See
//! `memory/rui/gen/plans/tea-slint-model/00-plan.md`.

/// Every `Effect` variant's result is `Result<_, EffectError>` -- see
/// 00-plan.md's "Effect-result contracts": there is no silent-failure
/// arm, every `Err` must be handled by `update()`'s exhaustive match and
/// turned into a `Dirty::Error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectError {
    pub message: String,
}

impl EffectError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for EffectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Side effects `update()` can request. `EffectExecutor` (Phase 4) spawns
/// one tokio task per `Effect`, calling into the existing `agent_bridge`/
/// `gateway_actor`/`settings_file`/`state_store` code -- those crates are
/// unchanged, just called from here instead of from inside `on_*`
/// closures. Each variant's result re-enters via
/// `slint::invoke_from_event_loop` as `Msg::Effect(EffectResultMsg::..)`.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Phase 0: cold-start hydration from `PanelStateStore`.
    LoadInitialState,
    /// PUI-014: create a new thread as a DEFERRED placeholder -- claims its
    /// positional slot index but opens no ACP session yet, so the provider
    /// stays editable until the first message triggers the attach (imperatively,
    /// in the `&mut` send dispatch). Carries only what
    /// `AgentBridge::add_thread_deferred` needs; profile/permission are read
    /// from the model thread at attach time.
    NewThreadDeferred {
        real_index: usize,
        display_name: String,
        provider: String,
    },
    CloseThread {
        real_index: usize,
    },
    PersistSelectedThread {
        thread_id: String,
    },
    ToggleBackground {
        real_index: usize,
    },
    DeleteThread {
        real_index: usize,
    },
    ArchiveThread {
        thread_id: String,
        archived: bool,
    },
    RenameThread {
        real_index: usize,
        name: String,
    },
    PersistThread {
        real_index: usize,
    },
    PersistThreadRecord {
        record: crate::state_store::ThreadRecord,
    },
    RecoverSessionAttach {
        real_index: usize,
        session_id: String,
        provider: String,
        title: String,
    },
    SendPrompt {
        thread_id: String,
        text: String,
    },
    /// Probe a provider/profile selection without attaching a real chat
    /// session. The bridge performs a pool acquire/release asynchronously
    /// and reports the result through the normal frame event stream.
    ProbeProvider {
        real_index: usize,
        provider: String,
        profile_name: Option<String>,
    },
    CancelGeneration {
        real_index: usize,
    },
    RespondAgentRequest {
        real_index: usize,
        request_id: String,
        approve: bool,
    },
    PermissionOptionSelected {
        real_index: usize,
        request_id: String,
        option: String,
    },
    LoadOlderMessages {
        real_index: usize,
    },
    LocalTerminalSpawn,
    LocalTerminalKill,
    LocalTerminalWrite {
        bytes: Vec<u8>,
    },
    /// PUI-002b: `terminal/kill` for an agent-created terminal, via
    /// `AgentBridge::kill_terminal`. Distinct from `LocalTerminalKill`
    /// (the client-local PTY, no gateway call).
    KillAgentTerminal {
        real_index: usize,
        terminal_id: String,
    },
    SaveSettings {
        input: crate::msg::SettingsSaveInput,
    },
    SetConfigOption {
        real_index: usize,
        key: String,
        value: String,
    },
    SetMode {
        real_index: usize,
        mode: String,
    },
    SaveDevMode {
        enabled: bool,
    },
    McpServerCreate {
        real_index: usize,
        entry: crate::protocol_types::McpServerEntry,
    },
    McpServerUpdate {
        real_index: usize,
        entry: crate::protocol_types::McpServerEntry,
    },
    McpServerDelete {
        real_index: usize,
        name: String,
    },
    McpServerEnabledChanged {
        real_index: usize,
        name: String,
        enabled: bool,
    },
    McpServerAuthenticate {
        real_index: usize,
        name: String,
    },
    McpServerLogout {
        real_index: usize,
        name: String,
    },
    McpServerToolEnabledChanged {
        real_index: usize,
        server_name: String,
        tool_name: String,
        enabled: bool,
    },
    /// Per-tool deferred (lazy-load) flag -- mirrors `McpServerTool
    /// EnabledChanged` exactly, same persisted `extra["tools"]` array,
    /// different field.
    McpServerToolDeferredChanged {
        real_index: usize,
        server_name: String,
        tool_name: String,
        deferred: bool,
    },
    /// Kicks off a real MCP `tools/list` probe (`mcp_servers/tools_
    /// fetch`) for one server; the actual tool list comes back on a
    /// later `mcp_servers/list` refresh, not from this effect's own
    /// result -- see `PanelSingleton::dispatch_mcp_server_tools_fetch`'s
    /// doc comment.
    McpServerToolsFetchRequested {
        real_index: usize,
        server_name: String,
    },
    ProfileCreate {
        real_index: usize,
        name: String,
        agent_id: Option<String>,
        terminal_enabled: bool,
        fs_enabled: bool,
    },
    ProfileDelete {
        real_index: usize,
        name: String,
    },
    AgentInstallRequested {
        real_index: usize,
        agent_id: String,
    },
    AgentSetEnabled {
        real_index: usize,
        agent_id: String,
        enabled: bool,
    },
    SkillWrite {
        path: std::path::PathBuf,
        content: String,
    },
    CreateSkill {
        name: String,
        scope: String,
        active_project_path: Option<String>,
    },
    #[allow(dead_code)]
    SkillDelete {
        path: std::path::PathBuf,
    },
    SkillPromoteToGlobal {
        path: std::path::PathBuf,
    },
    OpenSkillEditor {
        path: std::path::PathBuf,
    },
    OpenInEditor {
        editor_name: String,
        path: std::path::PathBuf,
    },
    OpenWithOsDefault {
        path: std::path::PathBuf,
    },
    /// skills_audit_report §2.1: write text to the system clipboard.
    ClipboardWrite {
        text: String,
    },
    /// Non-Slint-callback: propagate a Shotcut project-path change to the
    /// bridge (`AgentBridge::set_active_project_path` today), then produce
    /// a fresh skills list diff.
    SetActiveProjectPath {
        path: Option<String>,
    },
    /// PISO-7: the ONLY caller of both `AgentBridge::rebind_project_path`
    /// (live, in-memory, self-heals the running session immediately) and
    /// `PanelStateStore::rename_project_path` (durable, survives a
    /// restart) -- issued exclusively from `HostMsg::ProjectPathRenamed`'s
    /// handler, never from a bare `ProjectPathChanged`. See both of those
    /// methods' doc comments for why a sqlite-only or bridge-only rewrite
    /// each leave half the bug unfixed.
    RenameProjectAssociation {
        old: String,
        new: String,
        old_identity: crate::model::ProjectIdentity,
    },
    /// PISO-8 (project-isolation-mlt-binding plan): a throttled background
    /// poll of snapshotd's `daemon.list`/`daemon.listProjects` CLI
    /// subcommands, so a thread bound to a project the agent picked via
    /// its own `daemon.launch` MCP call -- invisibly, in a headless
    /// instance this panel's own host never opened -- can be flagged as
    /// actually live rather than merely recorded. Triggered from
    /// `update_frame` on `FrameInput::daemon_projects_refresh_due`, never
    /// from the UI thread directly (see `agent_bridge::
    /// fetch_daemon_project_instances`'s own doc comment).
    RefreshDaemonProjectInstances,
}

/// Results feeding back into `Msg::Effect` -- one variant per `Effect`
/// above, wrapping that effect's typed `Result`.
#[derive(Debug, Clone, PartialEq)]
pub enum EffectResultMsg {
    InitialStateLoaded(Result<crate::model::InitialState, EffectError>),
    ThreadPersisted {
        real_index: usize,
        result: Result<(), EffectError>,
    },
    ThreadRecordPersisted(Result<(), EffectError>),
    SessionAttached {
        real_index: usize,
        thread_id: Option<String>,
        provider: Option<String>,
        result: Result<String, EffectError>,
    },
    /// mcp-servers-settings follow-up: dispatched synchronously from
    /// `dispatch::dispatch_compose_send_maybe_attach`, in the same call
    /// that kicks off `AgentBridge::attach_deferred_thread_with_config_
    /// options` for a deferred thread's first message -- marks
    /// `Model::first_attach_in_flight` before the background attach has
    /// any chance to resolve, so the chat-view pulsing indicator can show
    /// on the very next frame. Not a `Result` wrapper like the other
    /// variants here (there is nothing to fail synchronously that isn't
    /// already routed through `SessionAttached { result: Err(..) }`
    /// immediately after this dispatch) -- purely a "this started" marker.
    SessionAttachStarted {
        thread_id: String,
    },
    /// stale-provider-switch-pulse fix: `AgentBridge::probe_provider_
    /// selection` deliberately pushes NO `AgentEvent::ProviderProbe` at
    /// all for a thread with no resolvable project directory (a normal,
    /// fully-supported state -- see that method's own doc comment on why
    /// treating it as a probe failure would be wrong: a spurious
    /// "Provider unavailable" toast and a Send block for a reason that
    /// has nothing to do with the provider). But `SettingsMsg::
    /// ProfileSelected` already inserted `Model::provider_probes_in_flight`
    /// before dispatching `Effect::ProbeProvider`, unconditionally --
    /// the reducer has no visibility into bridge-side project state. With
    /// no event ever coming for that no-op case, the marker (and the
    /// "Switching provider..." pulse it drives) stayed stuck forever.
    /// `effect_executor.rs`'s `Effect::ProbeProvider` arm dispatches this
    /// the moment `probe_provider_selection` reports (via its bool return)
    /// that it will never push a completion event, clearing just the
    /// in-flight marker -- deliberately NOT touching `provider_errors`/
    /// the toast path, unlike `AgentEvent::ProviderProbe`'s `Ok`/`Err`
    /// arms, since no probe actually ran.
    ProviderProbeSkipped {
        real_index: usize,
        provider: String,
    },
    /// mcp-servers-settings plan: unlike `SessionAttached { result: Err(..)
    /// }` (a thread that already claimed a real `AgentBridge` slot failed
    /// to open its ACP session -- the row stays and shows an error, see
    /// that variant's fold), this is for the two lifecycle effects that
    /// create the slot itself (`Effect::NewThreadDeferred`/
    /// `Effect::RecoverSessionAttach`, both of which claim a `model.
    /// threads` row up front but only push an `AgentBridge` slot on
    /// success -- see `AgentBridge::add_thread_deferred`'s doc comment on
    /// the `model.threads[i] <-> slots[i]` invariant). When the bridge
    /// call fails, NO slot was ever pushed, so leaving the row in place
    /// (the `SessionAttached` Err pattern) permanently shifts every later
    /// real_index one off from its actual bridge slot -- e.g. a later
    /// `ArchiveThread { thread_id }` effect resolves the durable slot rather
    /// than a mutable row index, so it cannot land on a DIFFERENT
    /// thread's slot than the one the user archived. The fold for this
    /// variant removes the orphaned row instead, restoring alignment.
    ThreadCreationFailed {
        real_index: usize,
        message: String,
    },
    SkillWritten(Result<(), EffectError>),
    SkillCreated(Result<std::path::PathBuf, EffectError>),
    SkillPromoted(Result<(), EffectError>),
    ExternalEditorOpened(Result<(), EffectError>),
    OsDefaultOpened(Result<(), EffectError>),
    SkillEditorLoaded(Result<crate::model::SkillEditorState, EffectError>),
    /// markdown-render-cache-layer plan Phase 2. Deliberate exception to
    /// this enum's "one variant per `Effect` above" rule: the background
    /// render worker (`markdown_worker.rs`) is spawned directly (its own
    /// `spawn_background_render`/`_pooled` + `deliver`/`on_chunk`
    /// callbacks), not via a `Vec<Effect>` `update()` returns for
    /// `execute_effects` to run -- there is no matching `Effect::X`
    /// variant to add above, and adding an `Effect` nothing ever
    /// constructs would be misleading in the other direction. This is
    /// still the right bucket semantically (an async operation's result
    /// feeding back into the reducer), so it lives here rather than
    /// inventing a fifth top-level `Msg` source (see `msg.rs`'s "four
    /// sources" doc comment).
    ///
    /// `source_hash` must be produced by the exact same algorithm
    /// `ThreadMessageIndex::content_hash_for` compares against
    /// (`thread_message_index::hash_content`) -- see that function's
    /// doc comment. `message_key` is a durable message key
    /// (`models::transcript_row_key`'s format), resolved against the
    /// target thread's own `ThreadMessageIndex` at apply time, never a
    /// positional row index captured at spawn time.
    MarkdownBlocksReady {
        thread_id: String,
        message_key: String,
        source_hash: u64,
        blocks: Vec<crate::models::MarkdownBlockData>,
    },
    /// One of the 6 reactive-sync trigger call sites (create/promote/
    /// edit/agent-enable/agent-disable/thread-start) failed to propagate
    /// a skill to an attached agent -- see memory/acpx/gen/plans/
    /// acpx-skills/README.md's "reactive-sync failures are invisible to
    /// the user" gap. Sent alongside (not instead of) the existing
    /// eprintln! at each call site; best-effort, not retried.
    SkillReactiveSyncFailed {
        operation: String,
        detail: String,
    },
    /// A streamed token/chunk arriving mid-generation -- not a
    /// completion. See 00-plan.md's stale-target no-op contract: if
    /// `thread_id` no longer exists in `Model`, `update()` must no-op.
    #[allow(dead_code)]
    PromptStreamDelta {
        thread_id: String,
        message_id: String,
        delta: String,
    },
    PromptSent {
        real_index: usize,
        result: Result<(), EffectError>,
    },
    SettingsSaved(Result<(), EffectError>),
    #[allow(dead_code)]
    GatewayCallCompleted {
        real_index: usize,
        result: Result<(), EffectError>,
    },
    /// A state-mutating effect with no dedicated result variant failed --
    /// bridge lifecycle calls (`close_thread`/`archive_thread`/
    /// `delete_thread` returning `false`) and `PanelStateStore` writes
    /// (`PersistSelectedThread`/`ToggleBackground`/`RenameThread`) that
    /// previously only `eprintln!`'d on failure. `thread_id` is empty when
    /// no specific thread could be resolved (matches
    /// `ThreadRecordPersisted`'s convention).
    StateEffectFailed {
        thread_id: String,
        message: String,
    },
    /// One MCP server settings operation (create/update/delete/enable-
    /// toggle/authenticate/logout/tool-toggle) finished -- `Ok(message)`
    /// is a ready-to-show success string, `Err` a ready-to-show failure
    /// string. Reuses the same `show_toast`/`Dirty::Toast` popup
    /// `EffectResultMsg::SettingsSaved` already shows for "Settings
    /// saved"/"Settings save failed" -- this is the same shared, app-wide
    /// action-feedback bar, not a new component, just not previously
    /// reachable from this view's dispatch path (every MCP settings call
    /// used to only `eprintln!` on failure, with nothing shown in the UI
    /// at all).
    McpServerOperationCompleted(Result<String, EffectError>),
    /// PISO-8: result of `Effect::RefreshDaemonProjectInstances`. `Err`
    /// (daemon unreachable, `snapshotd` binary missing, malformed
    /// output, ...) is a best-effort miss, not a user-facing error --
    /// `update()` leaves the previously cached instances in place rather
    /// than surfacing a toast for a background poll the user never
    /// triggered and cannot act on.
    DaemonProjectInstancesLoaded(
        Result<Vec<crate::agent_bridge::DaemonProjectInstance>, EffectError>,
    ),
}
