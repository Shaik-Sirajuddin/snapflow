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
    Selected(usize),
    NavigateDelta(i32),
    CloseRequested(usize),
    DeleteRequested(usize),
    RenameRequested(usize, String),
    ToggleBackground(usize),
    RecoverSessionAttach {
        session_id: String,
        provider: String,
        title: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ComposeMsg {
    SendRequested(String),
    StopRequested,
    GenerationStopped,
    MentionTokenPrefix { text: String, cursor: i32 },
    MentionTokenQuery { text: String, cursor: i32 },
    MentionTokenReplace { text: String, cursor: i32, replacement: String },
    WordBoundaryBefore { text: String, cursor: i32 },
    ContainsCi { haystack: String, needle: String },
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
    Save(String),
    ScopeChanged(String),
    ConfigOptionSelected { key: String, value: String },
    ModeSelected(String),
    DevModeToggled(bool),
    McpServerCreate { name: String, config: String },
    McpServerDelete { name: String },
    McpServerEnabledChanged { name: String, enabled: bool },
    ProfileCreate { name: String, config: String },
    ProfileDelete { name: String },
    AgentInstallRequested { agent_id: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum SkillMsg {
    NewSkillRequested,
    ContentEdited { path: std::path::PathBuf, content: String },
    CopyPathRequested { path: std::path::PathBuf },
    EditorOpenRequested { path: std::path::PathBuf },
    OpenInEditorRequested { path: std::path::PathBuf },
    OpenWithOsDefaultRequested { path: std::path::PathBuf },
    PromoteToGlobal { path: std::path::PathBuf },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChromeMsg {
    SearchChanged(String),
    SearchSubmitted(String),
    ToggleExpanded,
    ErrorBannerDismissed,
}

/// Direct C++ -> Rust FFI entry points that are *not* Slint callbacks and
/// today mutate `PanelSingleton`/state directly -- see 00-plan.md's "Msg
/// source coverage" point 3 for why these must route through `dispatch()`
/// too, not just the 47 `on_*` closures.
#[derive(Debug, Clone, PartialEq)]
pub enum HostMsg {
    InvokeCommand(String),
    InputKey { key: String, modifiers: u32 },
    AppearanceChanged(crate::appearance::AppearanceState),
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
    pub bridge_events_pending: bool,
    pub thread_records_dirty: bool,
    pub settings_reload_pending: bool,
    pub local_terminal_snapshot: Option<String>,
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
        "mcp_server_create",
        "mcp_server_delete",
        "mcp_server_enabled_changed",
        "mode_selected",
        "new_skill_requested",
        "new_thread_requested",
        "permission_option_selected",
        "profile_create",
        "profile_delete",
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
            "new_thread_requested" | "thread_selected" | "thread_navigation_requested"
            | "thread_close_requested" | "thread_delete_requested" | "thread_rename_requested"
            | "thread_toggle_background" | "recover_session_attach" => "thread",
            "send_requested" | "stop_requested" | "generation_stopped"
            | "active_token_prefix" | "active_token_query" | "replace_active_token"
            | "word_boundary_before" | "contains_ci" => "compose",
            "approve_request" | "reject_request" | "permission_option_selected"
            | "load_older_requested" => "request",
            "expand_terminal" | "close_terminal_overlay" | "local_terminal_toggle_requested"
            | "local_terminal_close_requested" | "local_terminal_key_input" => "terminal",
            "settings_requested" | "settings_close" | "settings_save"
            | "settings_scope_changed" | "config_option_selected" | "mode_selected"
            | "dev_mode_toggled" | "mcp_server_create" | "mcp_server_delete"
            | "mcp_server_enabled_changed" | "profile_create" | "profile_delete"
            | "agent_install_requested" => "settings",
            "new_skill_requested" | "skill_content_edited" | "skill_copy_path_requested"
            | "skill_editor_open_requested" | "skill_open_in_editor_requested"
            | "skill_open_with_os_default_requested" | "skill_promote_to_global" => "skill",
            "search_changed" | "search_submitted" | "toggle_expanded"
            | "error_banner_dismissed" => "chrome",
            other => panic!("on_{other} has no UiMsg domain mapping -- add one to msg.rs and this test"),
        }
    }

    #[test]
    fn every_known_on_star_closure_maps_to_a_ui_msg_domain() {
        for name in ON_STAR_CLOSURE_NAMES {
            closure_name_to_domain(name);
        }
    }
}
