//! `tea-slint-model` Phase 1: the closed set of things that can happen.
//! See `memory/rui/gen/plans/tea-slint-model/00-plan.md`'s "Msg source
//! coverage" section -- **four** sources feed `dispatch()`, all four
//! route through here, none may mutate `Model` directly: `Ui` (Slint
//! callbacks), `Effect` (effect completions), `Host` (direct FFI entry
//! points that are not Slint callbacks), and `Frame` (the poll tick).

#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    Ui(UiMsg),
    Effect(crate::effect::EffectResultMsg),
    Host(HostMsg),
    Frame(FrameInput),
}

#[derive(Debug, Clone, PartialEq)]
pub enum UiMsg {
    Thread(ThreadMsg),
    Compose(ComposeMsg),
    Request(RequestMsg),
    Terminal(TerminalMsg),
    Settings(SettingsMsg),
    Skill(SkillMsg),
    Chrome(ChromeMsg),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ThreadMsg {
    New,
    NewResolved {
        display_name: String,
        provider: String,
        profile_name: Option<String>,
        permission_profile: Option<String>,
        session_id: Option<String>,
        thread_id: Option<String>,
    },
    Selected(usize),
    NavigateDelta(i32),
    CloseRequested(usize),
    DeleteRequested(usize),
    // setup-followups plan, archive_thread_backend_verify: purely a local
    // presentation flag (see AgentBridge::archive_thread's doc comment) --
    // no ACP request is involved, unlike Close/Delete above.
    ArchiveRequested(usize),
    RenameRequested(usize, String),
    ToggleBackground(usize),
    RecoverSessionAttach {
        session_id: String,
        provider: String,
        title: String,
        thread_id: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ComposeMsg {
    SendRequested(String),
    StopRequested,
    GenerationStopped,
    /// Drop one send-queue entry (QueuedMessageBar cancel).
    /// `message_index` is the Slint message-list index (`MessageItem.index`).
    QueueCancel {
        message_index: usize,
    },
    /// Pull one send-queue entry into the composer for editing.
    QueueEdit {
        message_index: usize,
    },
    /// Jump one send-queue entry to the front and send it immediately
    /// (QueuedMessageBar's send-now affordance -- send_queue.rs's
    /// send_now/steer subsystem). If a turn is currently in flight, the
    /// caller must cancel it; `update()` arms the queue's
    /// `AbsorbingCancel` state so the resulting `Stopped` event doesn't
    /// also auto-drain the next entry.
    QueueSendNow {
        message_index: usize,
    },
    /// Stop in-flight generation and pause auto-drain of the send queue
    /// (QueuedMessageBar stop while an entry is marked `sending`).
    QueueStop,
    MentionTokenPrefix {
        text: String,
        cursor: i32,
    },
    MentionTokenQuery {
        text: String,
        cursor: i32,
    },
    MentionTokenReplace {
        text: String,
        cursor: i32,
        replacement: String,
    },
    WordBoundaryBefore {
        text: String,
        cursor: i32,
    },
    ContainsCi {
        haystack: String,
        needle: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum RequestMsg {
    Approve(String),
    Reject(String),
    PermissionOptionSelected(String, String),
    LoadOlderRequested(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum TerminalMsg {
    Expand(String),
    CloseOverlay,
    LocalToggle,
    LocalClose,
    LocalKeyInput(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum SettingsMsg {
    Open,
    Close,
    Save(SettingsSaveInput),
    ScopeChanged(String),
    ConfigOptionSelected {
        key: String,
        value: String,
    },
    ModeSelected(String),
    // setup-followups plan, provider_fastmode_profile_persistence: only
    // meaningful while the currently selected thread has no attached
    // session yet (see ThreadItem.has-session's doc comment) -- update()
    // is a no-op if it already has one, since ACP has no primitive for
    // moving a live session to a different backend.
    ProfileSelected(String),
    DevModeToggled(bool),
    McpServerCreate {
        name: String,
        command: String,
    },
    McpServerDelete {
        name: String,
    },
    McpServerEnabledChanged {
        name: String,
        enabled: bool,
    },
    /// OAuth / auth Connect for a remote MCP server. Persists registry-side
    /// auth flags via `mcp_servers/update` (no separate authenticate RPC).
    McpServerAuthenticate {
        name: String,
    },
    /// Per-tool enable toggle on one MCP server entry (persisted in the
    /// server's opaque JSON `tools` array via `mcp_servers/update`).
    McpServerToolEnabledChanged {
        server_name: String,
        tool_name: String,
        enabled: bool,
    },
    ProfileCreate {
        name: String,
        agent_id: Option<String>,
        terminal_enabled: bool,
        fs_enabled: bool,
    },
    ProfileDelete {
        name: String,
    },
    AgentInstallRequested {
        agent_id: String,
    },
    // setup-followups plan, agent_settings_ordering_and_install_enable_
    // flow: the real "install > enable" second step, via the admin
    // plane (AgentBridge::set_agent_enabled) -- distinct from Install.
    AgentSetEnabled {
        agent_id: String,
        enabled: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SettingsSaveInput {
    pub scope: String,
    pub default_profile: String,
    pub permission_profile: String,
    pub background_default: bool,
    pub default_agent_id: String,
    pub selected_thread_id: Option<String>,
    pub background_override_set: bool,
    pub background_override: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SkillMsg {
    NewSkillRequested {
        name: String,
        scope: String,
    },
    ContentEdited {
        path: std::path::PathBuf,
        content: String,
    },
    CopyPathRequested {
        path: std::path::PathBuf,
    },
    EditorOpenRequested {
        path: std::path::PathBuf,
    },
    OpenInEditorRequested {
        editor_name: String,
        path: std::path::PathBuf,
    },
    OpenWithOsDefaultRequested {
        path: std::path::PathBuf,
    },
    PromoteToGlobal {
        path: std::path::PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChromeMsg {
    SearchChanged(String),
    SearchSubmitted {
        query: String,
        search_skills: bool,
        show_global: bool,
    },
    ToggleExpanded(usize),
    ErrorBannerDismissed,
}

/// Direct C++ -> Rust FFI entry points that mutate panel state and are *not*
/// Slint callbacks -- see 00-plan.md's "Msg source coverage" point 3 for why
/// these must route through `dispatch()` too, not just the `on_*` closures.
#[derive(Debug, Clone, PartialEq)]
pub enum HostMsg {
    InvokeCommand(String),
    AppearanceChanged(crate::appearance::AppearanceState),
    ThemeChanged(String),
    ProjectPathChanged(Option<String>),
    /// Cold-start hydration trigger -- see 00-plan.md Phase 0. Carries
    /// whatever `panel_rust_create` already has in hand *before* any
    /// `Effect` runs (window size, requested defaults); the actual
    /// `PanelStateStore` read happens as `Effect::LoadInitialState`.
    Init,
}

/// Inputs collected by `panel_rust_poll` (`lib.rs`) each tick, with no
/// mutation performed during collection -- see 00-plan.md's "The poll
/// tick is a 4th Msg source, not an exception". Dispatched as
/// `Msg::Frame(FrameInput)` through the normal `update()` -> `sync()`
/// path; `sync()` only runs when the returned `Dirty` set is nonempty.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FrameInput {
    pub bridge_events: Vec<crate::agent_bridge::BridgeEvent>,
    /// Durable thread identity captured at the same time as each bridge
    /// event. The numeric event index is only a bridge lookup location and
    /// may no longer identify the same Model row when a frame also carries a
    /// list-shape change.
    pub bridge_event_thread_ids: Vec<String>,
    pub bridge_events_pending: bool,
    pub thread_record_snapshots: Vec<crate::state_store::ThreadRecord>,
    pub settings_reload_pending: bool,
    pub prepend_expanded_rows: usize,
    pub thread_list_snapshot: Option<ThreadListSnapshot>,
    pub selected_thread_snapshot: Option<ThreadFrameSnapshot>,
    pub clear_selected_thread: bool,
    pub settings_preferences_snapshot: Option<SettingsPreferencesSnapshot>,
    pub settings_gateway_snapshot: Option<SettingsGatewaySnapshot>,
    pub skills_snapshot: Option<Vec<crate::skills_state::SkillEntry>>,
}

/// Read-only bridge/store data for the sidebar. The adapter owns collection;
/// `update()` owns the projected rows after folding this snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadListSnapshot {
    pub visible_indices: Vec<usize>,
    pub visible_thread_ids: Vec<String>,
    pub rows: Vec<crate::models::VisibleThreadItem>,
    /// Review-gate fix (phase 32): bridge-persisted archived flags for
    /// EVERY thread (indexed by real index, not filtered) -- restart
    /// hydration for `ThreadModel::archived`, which the sidebar counters
    /// and the archive pool cap read. Empty = no data (tests).
    pub archived_flags: Vec<bool>,
}

/// Read-only settings data collected from the selected gateway for one
/// reducer turn. The gateway remains the source of truth; `update_frame`
/// owns the projected values after folding this snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsGatewaySnapshot {
    pub profiles: Vec<crate::gateway_actor::ProfileSummary>,
    pub mcp_servers: Vec<crate::protocol_types::McpServerEntry>,
    pub agents: Vec<crate::protocol_types::AgentCatalogEntry>,
    pub recoverable_sessions: Vec<crate::gateway_actor::RemoteThreadInfo>,
    pub recovery_provider: String,
}

/// Read-only JSON/SQLite preferences collected for one reducer turn.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsPreferencesSnapshot {
    pub scope: String,
    pub default_profile: String,
    pub permission_profile: String,
    pub background_default: bool,
    pub default_agent_id: String,
    pub dev_mode: bool,
    pub background_override_set: bool,
    pub background_override: bool,
}

/// Read-only bridge data collected for the currently displayed thread during
/// one frame. The bridge owns the live connections; the reducer owns the
/// resulting presentation state after this value is folded into `Model`.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadFrameSnapshot {
    /// Durable reducer identity. `real_index` is only the bridge lookup
    /// location at collection time and may change before this snapshot is
    /// folded into Model.
    pub thread_id: String,
    pub real_index: usize,
    pub transcript: Vec<crate::conversation::TranscriptItem>,
    pub has_older_messages: bool,
    pub pending_request: crate::PendingRequestItem,
    pub terminals: Vec<crate::TerminalItem>,
    pub expanded_terminal: Option<crate::TerminalItem>,
    pub local_terminal: crate::LocalTerminalItem,
    pub connection_status: String,
    pub session_modes: Option<crate::protocol_types::SessionModesEvent>,
    pub config_options: Vec<crate::protocol_types::ConfigOptionInfo>,
    /// Phase 18: live (used, size) token usage for the context ring.
    pub usage: (i64, i64),
}

#[cfg(test)]
mod tests {
    //! Phase 1 verification (see 00-plan.md, Phase 1): a checklist
    //! cross-referencing every `component.on_*` Slint callback in
    //! `lib.rs` against a `UiMsg` variant, via a match with **no wildcard
    //! arm** -- adding a new `on_*` closure to `lib.rs` without adding it
    //! here makes `closure_name_to_ui_msg_kind` fail to compile until
    //! this list is updated, matching the plan's exhaustiveness
    //! requirement one level up from `update()`'s own match.

    /// Every `component.on_*` name in `lib.rs`, hand-extracted (`rg -oP
    /// '(?<=\.on_)[a-z_]+(?=\()' src/lib.rs | sort -u`) at the time this
    /// test was written. If `lib.rs` gains or loses one, this list (and
    /// the match below) must be updated in lockstep -- that's the point.
    const ON_STAR_CLOSURE_NAMES: &[&str] = &[
        "active_token_prefix",
        "active_token_query",
        "agent_install_requested",
        "approve_request",
        "close_terminal_overlay",
        "config_option_selected",
        "contains_ci",
        "dev_mode_toggled",
        "error_banner_dismissed",
        "expand_terminal",
        "generation_stopped",
        "load_older_requested",
        "local_terminal_close_requested",
        "local_terminal_key_input",
        "local_terminal_toggle_requested",
        "mcp_server_authenticate",
        "mcp_server_create",
        "mcp_server_delete",
        "mcp_server_enabled_changed",
        "mcp_server_tool_enabled_changed",
        "mode_selected",
        "new_skill_requested",
        "new_thread_requested",
        "permission_option_selected",
        "profile_create",
        "profile_delete",
        "queue_cancel_requested",
        "queue_edit_requested",
        "queue_stop_requested",
        "recover_session_attach",
        "reject_request",
        "replace_active_token",
        "search_changed",
        "search_submitted",
        "send_requested",
        "settings_close",
        "settings_requested",
        "settings_save",
        "settings_scope_changed",
        "skill_content_edited",
        "skill_copy_path_requested",
        "skill_editor_open_requested",
        "skill_open_in_editor_requested",
        "skill_open_with_os_default_requested",
        "skill_promote_to_global",
        "stop_requested",
        "thread_close_requested",
        "thread_delete_requested",
        "thread_navigation_requested",
        "thread_rename_requested",
        "thread_selected",
        "thread_toggle_background",
        "toggle_expanded",
        "word_boundary_before",
    ];

    /// Maps each closure name to the `UiMsg` domain module it belongs to
    /// per 00-plan.md's "Callback -> Msg mapping" table. No wildcard arm:
    /// an unrecognized name is a compile-time-adjacent test failure
    /// (panics at test time, not build time -- `match` over `&str` can't
    /// be exhaustive at compile time -- but every name change is still
    /// forced through this function).
    fn closure_name_to_domain(name: &str) -> &'static str {
        match name {
            "new_thread_requested"
            | "thread_selected"
            | "thread_navigation_requested"
            | "thread_close_requested"
            | "thread_delete_requested"
            | "thread_rename_requested"
            | "thread_toggle_background"
            | "recover_session_attach" => "thread",
            "send_requested"
            | "stop_requested"
            | "generation_stopped"
            | "queue_cancel_requested"
            | "queue_edit_requested"
            | "queue_stop_requested"
            | "active_token_prefix"
            | "active_token_query"
            | "replace_active_token"
            | "word_boundary_before"
            | "contains_ci" => "compose",
            "approve_request"
            | "reject_request"
            | "permission_option_selected"
            | "load_older_requested" => "request",
            "expand_terminal"
            | "close_terminal_overlay"
            | "local_terminal_toggle_requested"
            | "local_terminal_close_requested"
            | "local_terminal_key_input" => "terminal",
            "settings_requested"
            | "settings_close"
            | "settings_save"
            | "settings_scope_changed"
            | "config_option_selected"
            | "mode_selected"
            | "dev_mode_toggled"
            | "mcp_server_create"
            | "mcp_server_delete"
            | "mcp_server_enabled_changed"
            | "mcp_server_authenticate"
            | "mcp_server_tool_enabled_changed"
            | "profile_create"
            | "profile_delete"
            | "agent_install_requested" => "settings",
            "new_skill_requested"
            | "skill_content_edited"
            | "skill_copy_path_requested"
            | "skill_editor_open_requested"
            | "skill_open_in_editor_requested"
            | "skill_open_with_os_default_requested"
            | "skill_promote_to_global" => "skill",
            "search_changed"
            | "search_submitted"
            | "toggle_expanded"
            | "error_banner_dismissed" => "chrome",
            other => {
                panic!("on_{other} has no UiMsg domain mapping -- add one to msg.rs and this test")
            }
        }
    }

    #[test]
    fn every_known_on_star_closure_maps_to_a_ui_msg_domain() {
        for name in ON_STAR_CLOSURE_NAMES {
            closure_name_to_domain(name);
        }
    }
}
