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
        real_index: usize,
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
        real_index: usize,
        text: String,
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
    SkillWritten(Result<(), EffectError>),
    SkillCreated(Result<std::path::PathBuf, EffectError>),
    SkillPromoted(Result<(), EffectError>),
    ExternalEditorOpened(Result<(), EffectError>),
    OsDefaultOpened(Result<(), EffectError>),
    SkillEditorLoaded(Result<crate::model::SkillEditorState, EffectError>),
    /// One of the 6 reactive-sync trigger call sites (create/promote/
    /// edit/agent-enable/agent-disable/thread-start) failed to propagate
    /// a skill to an attached agent -- see memory/acpx/gen/plans/
    /// acpx-skills/README.md's "reactive-sync failures are invisible to
    /// the user" gap. Sent alongside (not instead of) the existing
    /// eprintln! at each call site; best-effort, not retried.
    SkillReactiveSyncFailed { operation: String, detail: String },
    /// A streamed token/chunk arriving mid-generation -- not a
    /// completion. See 00-plan.md's stale-target no-op contract: if
    /// `thread_id` no longer exists in `Model`, `update()` must no-op.
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
}
