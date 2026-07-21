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
    NewThread {
        real_index: usize,
        display_name: String,
        provider: String,
        profile_name: Option<String>,
        permission_profile: Option<String>,
    },
    DeleteThread {
        real_index: usize,
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
    SaveSettings {
        doc: String,
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
        name: String,
        command: String,
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
    SkillWrite {
        path: std::path::PathBuf,
        content: String,
    },
    SkillDelete {
        path: std::path::PathBuf,
    },
    SkillPromoteToGlobal {
        path: std::path::PathBuf,
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
    SkillPromoted(Result<(), EffectError>),
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
}
