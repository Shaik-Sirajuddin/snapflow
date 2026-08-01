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
    #[allow(dead_code)]
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
    /// Live unsent text from the retained per-thread ChatArea composer.
    DraftChanged(String),
    SendRequested(String),
    StopRequested,
    #[allow(dead_code)]
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
    /// SCNA-03: pressing Return on an *empty* compose box immediately
    /// fast-tracks the front queue entry (send_queue.rs's
    /// try_fast_track/can_fast_track -- armed for one shot by the enqueue
    /// that just happened, cleared by consuming it here). No target index:
    /// unlike QueueSendNow, this always acts on the current front entry
    /// of the *selected* thread's queue, and is a safe no-op (via
    /// try_fast_track's own can_fast_track guard) if nothing is eligible
    /// -- the Slint side does not need to know the queue's state to fire
    /// this, only that the compose box was empty.
    QueueFastTrack,
    /// Stop in-flight generation and pause auto-drain of the send queue
    /// (QueuedMessageBar stop while an entry is marked `sending`).
    QueueStop,
    #[allow(dead_code)]
    MentionTokenPrefix {
        text: String,
        cursor: i32,
    },
    #[allow(dead_code)]
    MentionTokenQuery {
        text: String,
        cursor: i32,
    },
    #[allow(dead_code)]
    MentionTokenReplace {
        text: String,
        cursor: i32,
        replacement: String,
    },
    #[allow(dead_code)]
    WordBoundaryBefore {
        text: String,
        cursor: i32,
    },
    #[allow(dead_code)]
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
    /// PUI-002b: the terminals popup's `[x]` kill button, for an
    /// agent-created (`terminal/create`d) terminal -- distinct from
    /// `LocalClose` (the client-local PTY toggle above), which never
    /// touches the gateway at all.
    Kill(String),
    /// Terminal-tabs phase: switch which already-open tab is active,
    /// fired by clicking a tab inside the full-view overlay itself (not
    /// the popup -- that still goes through `Expand`, which both opens
    /// AND activates). A stray id (already closed/never opened) is
    /// ignored rather than treated as an implicit re-open, so a tab strip
    /// racing a close never resurrects a tab the user just dismissed.
    SelectTab(String),
    /// Terminal-tabs phase: dismiss one tab from the overlay's open set
    /// without touching the underlying terminal (no kill effect -- the
    /// process, if still running, keeps running and stays reachable via
    /// the popup). If the closed tab was active, activates its neighbor;
    /// closing the last open tab is equivalent to `CloseOverlay`.
    CloseTab(String),
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
    // Compose-bar **Provider** picker (ui label "Provider"; still named
    // ProfileSelected for history). Only meaningful while the selected
    // thread has no attached session (see ThreadItem.has-session) --
    // update() is a no-op if it already has one (ACP cannot retarget a
    // live session). Writes BOTH:
    //   - profile_name: ACPX profile name (session/_acpx.profile)
    //   - agent_id → thread.provider: which agent/gateway to attach
    // (dispatch_compose_send_maybe_attach reads thread.provider for
    // attach_deferred_thread — writing only profile_name left provider
    // stuck at create-time default and ignored the picker).
    ProfileSelected {
        profile_name: String,
        agent_id: String,
    },
    DevModeToggled(bool),
    /// Full typed create, from the settings form's transport-picker/args/
    /// env/headers/timeout/oauth fields (`mcp_servers_view.slint`'s
    /// `mcp-server-submit` callback with `is_edit: false`).
    McpServerCreate {
        entry: crate::protocol_types::McpServerEntry,
    },
    /// Same form, `is_edit: true` -- updates the already-existing entry
    /// named `entry.name` instead of creating a new one.
    McpServerUpdate {
        entry: crate::protocol_types::McpServerEntry,
    },
    McpServerDelete {
        name: String,
    },
    McpServerEnabledChanged {
        name: String,
        enabled: bool,
    },
    /// Begins the real MCP OAuth 2.1 flow (`mcp_servers/authenticate`)
    /// and opens the returned authorization URL in a browser -- see
    /// `PanelSingleton::dispatch_mcp_server_authenticate`'s doc comment.
    McpServerAuthenticate {
        name: String,
    },
    /// Forgets a server's OAuth token (`mcp_servers/logout`).
    McpServerLogout {
        name: String,
    },
    /// Per-tool enable toggle on one MCP server entry (persisted in the
    /// server's opaque JSON `tools` array via `mcp_servers/update`).
    McpServerToolEnabledChanged {
        server_name: String,
        tool_name: String,
        enabled: bool,
    },
    /// Per-tool deferred (lazy-load) toggle -- same persisted `tools`
    /// JSON array as [`SettingsMsg::McpServerToolEnabledChanged`].
    McpServerToolDeferredChanged {
        server_name: String,
        tool_name: String,
        deferred: bool,
    },
    /// "Fetch tools" / "Refresh tools" button on one MCP server row --
    /// kicks off a real `mcp_servers/tools_fetch` background probe (see
    /// `acpx_core::router::Router::spawn_mcp_tools_fetch`'s doc comment).
    McpServerToolsFetchRequested {
        server_name: String,
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
    // Genuinely dual-tier like `background_default` above -- see
    // `settings_file::SettingsDocument::show_global_skills`'s doc comment.
    pub show_global_skills: bool,
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
    CopyMessageRequested {
        text: String,
    },
    ErrorBannerDismissed,
    CompleteOnboarding,
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
    ProjectCreatedUntitled,
    ProjectClosed,
    /// PISO-7 (project-isolation-mlt-binding plan): an explicit host
    /// signal for an MLT Save-As, carrying BOTH the old and new project
    /// file paths -- deliberately a separate variant from
    /// `ProjectPathChanged` rather than something inferred from two
    /// consecutive values of it. `old`/`new` alone cannot distinguish
    /// "Save-As A -> B" from "close A, open B" (both look like the
    /// active path changing from A to B), and treating them alike would
    /// rebind B's own pre-existing threads onto A's history -- merging
    /// two real projects. Only the host genuinely knows which happened,
    /// so it must say so explicitly. `old` empty means "this project was
    /// untitled and is being saved for the first time", which is NOT a
    /// rename (see `update_host`'s handler).
    ProjectPathRenamed {
        old: String,
        new: String,
    },
    // language-switch-sync plan: a QSettings "language" locale code (e.g.
    // "fr", "zh_CN") pushed live from Qt's Settings > Language picker --
    // see MainWindow::languageChanged's doc comment for why this is a
    // real live signal (mirroring producerOpened), not construction-time
    // only (the theme precedent's known gap).
    LanguageChanged(String),
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
    /// Agent ids whose install/enablement RPC is still in flight.
    pub agent_operations_in_flight: Vec<String>,
    /// MCP server actions ("<action>:<server-name>") whose RPC is still
    /// in flight -- see `AgentBridge::mcp_operations_in_flight`'s doc
    /// comment.
    pub mcp_operations_in_flight: Vec<String>,
    /// Remote session ids with a Settings > Agents "Attach" `session/
    /// load` still in flight -- see `AgentBridge::recover_session_
    /// operations_in_flight`'s doc comment.
    pub recover_session_operations_in_flight: Vec<String>,
    pub skills_snapshot: Option<Vec<crate::skills_state::SkillEntry>>,
    /// PISO-8 (project-isolation-mlt-binding plan): true at most once
    /// every few seconds (see `ExternalSnapshotSource`'s throttle, mirrors
    /// `skills_rescan_due`'s thread-local-timer pattern), signaling
    /// `update_frame` to queue `Effect::RefreshDaemonProjectInstances`.
    /// Computing the throttle here (cheap, no I/O) rather than doing the
    /// real subprocess-backed poll itself on the frame-collection path
    /// keeps that path non-blocking, per the plan's data-path discipline.
    pub daemon_projects_refresh_due: bool,
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
    /// PISO-2 (project-isolation-mlt-binding plan): the `active_project_
    /// path` this snapshot's `visible_indices`/`rows` were filtered
    /// against (`ExternalSnapshotSource::collect_thread_list_snapshot`
    /// tags it with the exact value `retain_items_for_project` used, not
    /// a fresh re-read). `update_frame`'s stale-snapshot guard compares
    /// this against `Model::active_project_path` at APPLY time and drops
    /// the whole list-shape update on a mismatch -- a snapshot collected
    /// for a project the user has since switched away from must never
    /// overwrite the (by-then-already-updated) visible list, even though
    /// today's single-threaded synchronous poll loop makes that window
    /// vanishingly unlikely to hit in practice; this makes the guarantee
    /// explicit and independent of that timing accident.
    pub active_project_path: Option<String>,
}

/// Read-only settings data collected from the selected gateway for one
/// reducer turn. The gateway remains the source of truth; `update_frame`
/// owns the projected values after folding this snapshot.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SettingsGatewaySnapshot {
    pub profiles: Vec<crate::gateway_actor::ProfileSummary>,
    pub mcp_servers: Vec<crate::protocol_types::McpServerEntry>,
    pub agents: Vec<crate::protocol_types::AgentCatalogEntry>,
    pub recoverable_sessions: Vec<crate::gateway_actor::RemoteThreadInfo>,
    pub recovery_provider: String,
}

/// Read-only JSON/SQLite preferences collected for one reducer turn.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SettingsPreferencesSnapshot {
    pub scope: String,
    pub default_profile: String,
    pub permission_profile: String,
    pub background_default: bool,
    pub default_agent_id: String,
    pub dev_mode: bool,
    pub background_override_set: bool,
    pub background_override: bool,
    pub show_global_skills: bool,
}

/// Read-only bridge data collected for the currently displayed thread during
/// one frame. The bridge owns the live connections; the reducer owns the
/// resulting presentation state after this value is folded into `Model`.
#[derive(Debug, Clone, PartialEq, Default)]
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
    /// Phase (terminal-tabs): every terminal currently pinned open as a
    /// full-view tab, resolved from `Model::open_terminal_ids` and kept in
    /// that list's insertion order (NOT `terminals`' order, which can
    /// reorder/refresh independently) so the tab strip doesn't shuffle
    /// under the user. `expanded_terminal` above stays the single
    /// *active* tab's item (unchanged contract, still what the overlay's
    /// content pane renders); this is the full open set the tab strip
    /// renders alongside it.
    pub open_terminals: Vec<crate::TerminalItem>,
    pub local_terminal: crate::LocalTerminalItem,
    pub connection_status: String,
    pub session_modes: Option<crate::protocol_types::SessionModesEvent>,
    pub config_options: Vec<crate::protocol_types::ConfigOptionInfo>,
    /// PUI-003: the agent's built-in slash commands for the `/` menu.
    pub available_commands: Vec<crate::protocol_types::AvailableCommandInfo>,
    /// Phase 18: live (used, size) token usage for the context ring.
    pub usage: (i64, i64),
    /// PROF-11: the agent's most recently pushed execution plan/todo list.
    pub plan: Vec<crate::protocol_types::PlanEntryInfo>,
    /// PROF-11: the most recently pushed live session title.
    pub session_title: Option<String>,
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
        "terminal_tab_closed",
        "terminal_tab_selected",
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
            | "terminal_tab_selected"
            | "terminal_tab_closed"
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
