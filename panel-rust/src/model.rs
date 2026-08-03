//! `tea-slint-model` Phase 0/1: `Model` is today's `PanelSingleton` state
//! fields, minus the `component: ChatPanel` handle, the render buffer,
//! and the window -- those stay owned by the FFI/render layer, not by
//! `update()`. See `memory/rui/gen/plans/tea-slint-model/00-plan.md`'s
//! ownership table: `Model` is mutated only inside `update()`, and
//! nothing outside `sync()` reads it to push a Slint `set_*` setter.
//!
//! `panel_rust_create` constructs this model and performs the cold-start
//! `Init -> LoadInitialState` transition before callbacks are installed.
//! Bridge-backed presentation data is collected externally, folded through
//! `Msg::Frame`, and projected by `sync()`.

use crate::agent_bridge::ThreadSpec;
use crate::appearance::AppearanceState;
use crate::conversation::TranscriptItem;
use crate::models::ThreadState;
use crate::protocol_types::{AvailableCommandInfo, ConfigOptionInfo, SessionModesEvent};
use crate::send_queue::SendQueue;
use slint::VecModel;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// Stable lifecycle identity for the project currently owned by the panel.
/// An untitled project is real state, not an empty path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum ProjectIdentity {
    #[default]
    None,
    Untitled(String),
    Saved(String),
}

impl ProjectIdentity {
    pub fn saved_path(&self) -> Option<&str> {
        match self {
            Self::Saved(path) => Some(path),
            Self::None | Self::Untitled(_) => None,
        }
    }
}

/// Result of `Effect::LoadInitialState` -- the same data
/// `panel_rust_create` reads from `PanelStateStore` today (thread
/// records, or the default thread set when the store is empty/missing),
/// now shaped as a plain value `update()` can fold into a fresh `Model`
/// with no Slint/FFI dependency, so it stays unit-testable per Phase 2's
/// verification requirement.
#[derive(Debug, Clone, PartialEq)]
pub struct InitialState {
    pub threads: Vec<ThreadSpec>,
    pub thread_ids: Vec<String>,
    pub selected_thread_id: Option<String>,
    pub permission_profiles: Vec<Option<String>>,
    pub thread_states: Vec<ThreadState>,
    /// Non-fatal failures collected while assembling cold-start state
    /// (settings load, panel-defaults sync, dev-mode persistence, bundled
    /// skill install, chat-thread-record restoration, agent-bridge
    /// unavailable, ...) that previously only reached `eprintln!`. Folded
    /// into `Dirty::Error` by `update()`'s `InitialStateLoaded` handler so
    /// cold-start problems are visible in the UI, not just stderr.
    pub startup_warnings: Vec<String>,
    /// Each restored/seeded thread's send queue, already loaded from its
    /// `<thread_id>.sendqueue.jsonl` (see `send_queue::SendQueue::load`)
    /// -- loading is real disk I/O, so it happens in `lib.rs` before this
    /// struct is built, never inside `update()`'s pure reducer. Indexed
    /// the same as `threads`/`thread_ids`; a missing/short entry falls
    /// back to an empty in-memory-only queue.
    pub send_queues: Vec<crate::send_queue::SendQueue>,
    /// ACPX owns queue persistence and dispatch for production sessions.
    /// Kept explicit so pure reducer tests can continue to exercise the
    /// legacy local queue contract.
    pub server_queue: bool,
    pub onboarding_completed: bool,
}

/// One thread's `Model`-side state -- the former parallel-array fields in
/// `PanelSingleton`, grouped per thread so `update()` cannot let them drift
/// out of sync by construction.
#[derive(Debug, Clone)]
pub struct ThreadModel {
    /// Stable local identity. `session_id` is the remote ACP session and
    /// may be absent while a new thread is attaching.
    pub thread_id: String,
    pub display_name: String,
    pub provider: String,
    pub profile_name: Option<String>,
    pub permission_profile: Option<String>,
    pub session_id: Option<String>,
    pub state: ThreadState,
    pub last_activity_time: Option<std::time::Instant>,
    pub error: Option<String>,
    /// PROF-8 (`profile-only-backend-selection` plan): the agent is
    /// reachable but its backend advertised ACP `authMethods` with no
    /// `auth_method_id` configured for the session's profile -- acpx-core
    /// rejects `session/new` outright rather than proceeding
    /// (`RouterError::BackendRequiresAuthentication`). Distinct from
    /// `error`/`ThreadState::Error`: this is a persistent condition the
    /// chat top bar shows as a yellow strip (not a dismissible red
    /// failure banner), and it clears itself once a turn actually
    /// completes, not on manual dismissal. See
    /// `models::is_backend_requires_authentication_error`'s doc comment
    /// for how it's detected and why that detection is fragile by
    /// necessity.
    pub unauthenticated: bool,
    /// Whether any *visible agent output* (an agent message or tool call
    /// -- deliberately not thinking/thought chunks) has arrived since
    /// this thread's latest prompt was sent. Lets `update()`'s
    /// `TurnEnded` arm surface an explicit "the agent ended its turn
    /// without a response" notice instead of silently going idle --
    /// found live (2026-07-23): a provider-side tool_search bug ended
    /// every MCP-needing codex turn after only reasoning, and the UI
    /// showed nothing at all, indistinguishable from a hang.
    pub agent_content_this_turn: bool,
    pub send_queue: SendQueue,
    pub server_queue: bool,
    /// Per-thread compose draft (leak_audit_report §2.5 / §4.2). The
    /// global `Model::compose_text` is only the *active* buffer for the
    /// displayed thread; switching saves/restores via this field.
    pub compose_draft: String,
    pub closed: bool,
    // setup-followups plan, archive_thread_backend_verify: purely local
    // presentation flag (see AgentBridge::archive_thread's doc comment) --
    // never sends an ACP request, unlike `closed`.
    pub archived: bool,
    /// thread-unread-state: visible agent output arrived for this thread
    /// while some OTHER thread was displayed, and the user has not opened
    /// it since. Set in `update_frame`'s `AgentEvent::Message` arm (gated
    /// on `model.displayed_thread != Some(this thread)`, so a thread
    /// streaming its own reply in front of the user never flips), cleared
    /// in `apply_thread_selection_switch`.
    ///
    /// Deliberately in-memory only, unlike `archived`: bridge events exist
    /// only while this process runs the ACPX actor, so no content can be
    /// delivered to a thread while the panel is closed. A restart therefore
    /// has nothing the user could have missed, and hydrating every thread
    /// as read is the correct -- not merely the cheap -- behaviour. That is
    /// why this gets no `ThreadRuntimeSnapshot`/sqlite column.
    pub unread: bool,
    /// Stable message identities currently known to the TEA model. Streaming
    /// effect results must resolve against this list, never a cached row
    /// index, before producing a `Dirty::MessageStreamingDelta`.
    pub message_ids: Vec<String>,
    pub transcript: Vec<TranscriptItem>,
    pub transcript_keys: Vec<String>,
    /// `(send_queue, generation_in_flight)` captured the last time this
    /// thread's message rows were actually rebuilt from `transcript` in
    /// `update.rs`'s frame-poll fold. `None` until the first rebuild.
    ///
    /// The frame poll runs at 60-90fps and re-collects `snapshot.transcript`
    /// every tick regardless of whether it changed (`ExternalSnapshotSource::
    /// collect_thread_snapshot_for`'s doc comment). Before this field
    /// existed, the fold unconditionally re-ran `message_rows_for_thread_
    /// with_state` on every tick too -- re-cloning the whole transcript and
    /// re-parsing every tool row's `raw_input` JSON (`to_message_rows_from_
    /// transcript`'s `serde_json::from_str` call), even on an idle thread
    /// whose transcript was byte-identical to the previous tick. Comparing
    /// against this field (alongside `thread.transcript == snapshot.
    /// transcript`) lets that tick instead reuse the already-installed rows
    /// from `Model::thread_view_models` (cheap `Rc`-backed clones, no
    /// re-parse) -- see the frame-poll fold in `update.rs` for the actual
    /// gate. `send_queue`/`in_flight` are tracked too because either can
    /// change the row projection (queued rows, the "sending" flag) even
    /// while `transcript` itself stays the same.
    pub rows_synced_with: Option<(SendQueue, bool)>,
    #[cfg(test)]
    /// Legacy projection fixture retained only by unit tests that exercise
    /// the pre-migration shared-list reducer. Production rows live in the
    /// durable-ID keyed `ThreadViewModels` registry.
    pub message_rows: Vec<crate::MessageItem>,
    /// markdown-render-cache-layer plan, Phase 1/3: this thread's own
    /// message-key -> {row_index, content_hash, rendered markdown} cache
    /// (`panel-rust/src/thread_message_index.rs`), replacing the old
    /// global, text-keyed `MARKDOWN_CACHE`/`MARKDOWN_BLOCK_CACHE`
    /// thread_locals in `models.rs`. Per-thread (not global) so a
    /// worker-delivered background render has somewhere durable and
    /// correctly-scoped to land regardless of which thread is currently
    /// displayed -- see that plan's "Chosen state shape".
    /// `RefCell`-wrapped (not a plain field): main's retained per-thread
    /// ChatView architecture rebuilds rows via `sync.rs`'s
    /// `projected_thread_rows`, which only ever holds `&Model`/
    /// `&ThreadModel` (rows now live in the Slint-side `thread_view_models`
    /// registry, not a mutable Rust row cache) -- so this cache needs
    /// interior mutability to stay writable from that shared-reference
    /// context, the same shape `thread_view_models`'s own Slint models
    /// already use for the identical reason.
    pub markdown_render_index: RefCell<crate::thread_message_index::ThreadMessageIndex>,
    /// markdown-render-cache-layer plan, Phase 7 trigger-wiring: this
    /// thread's own render generation counter. Deliberately per-thread,
    /// not one shared global counter -- bumping it invalidates any
    /// in-flight background render for THIS thread only (e.g. because
    /// its transcript changed again before the previous render
    /// finished), without touching unrelated background pre-renders for
    /// other threads. See `markdown_worker::EpochCounter`'s own doc
    /// comment for why a shared/global counter caused real cross-test
    /// interference in this exact module.
    pub markdown_epoch: crate::markdown_worker::EpochCounter,
    /// Companion to `markdown_epoch`: de-dupes a redundant background
    /// render spawn for the exact same `(thread_id, epoch)` this thread
    /// already has in flight.
    pub markdown_in_flight: crate::markdown_worker::InFlightRegistry,
    pub has_older_messages: bool,
    pub pending_request: crate::PendingRequestItem,
    pub terminals: Vec<crate::TerminalItem>,
    pub expanded_terminal: Option<crate::TerminalItem>,
    /// Terminal-tabs phase: resolved `TerminalItem`s for every id in
    /// `Model::open_terminal_ids`, kept in that list's order -- see
    /// `ThreadFrameSnapshot::open_terminals`'s doc comment (this is the
    /// same field, just folded from the snapshot into the persistent
    /// per-thread model like `expanded_terminal` already is).
    pub open_terminals: Vec<crate::TerminalItem>,
    pub local_terminal: crate::LocalTerminalItem,
    pub connection_status: String,
    pub session_modes: Option<SessionModesEvent>,
    pub config_options: Vec<ConfigOptionInfo>,
    /// PUI-003: the agent's built-in slash commands for the `/` menu.
    pub available_commands: Vec<AvailableCommandInfo>,
    /// Per-thread slash-command filter. The source catalog remains
    /// immutable; the Slint command model is a derived visible copy.
    pub command_filter: String,
    /// Phase 18: live (used, size) token usage for the context ring.
    pub usage: (i64, i64),
    /// PROF-11: the agent's most recently pushed execution plan/todo
    /// list, from a live ACP `plan` session/update. Empty means no plan
    /// notification has arrived yet (or the backend never sends one),
    /// same capability-gating convention as `config_options`/
    /// `available_commands` above.
    pub plan: Vec<crate::protocol_types::PlanEntryInfo>,
    /// PROF-11: the most recently pushed live session title, from a
    /// `session_info_update` session/update. Deliberately separate from
    /// the durable, user-editable `display_name` above -- see
    /// `crate::agent_bridge::ThreadSlot::session_title`'s doc comment for
    /// why an agent-pushed title must never silently overwrite what the
    /// user typed.
    pub session_title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SkillEditorState {
    pub name: String,
    /// The skill's directory -- what "Copy path"/"Open in editor"/"Open
    /// with OS default" want (the whole folder, not just SKILL.md).
    pub path: String,
    /// PUI-010: `path.join("SKILL.md")`, kept as a distinct field so
    /// `Effect::SkillWrite` (content save) writes the file, not the
    /// directory. Every skill save wrote directly to `path` (the
    /// directory) before this field existed, so `std::fs::write` hit
    /// `ErrorKind::IsADirectory` (EISDIR) on every single save.
    pub content_path: String,
    pub content: String,
    pub detected_editors: Vec<String>,
}

#[derive(Clone, Default)]
pub struct Model {
    pub threads: Vec<ThreadModel>,
    /// O(1) durable identity -> `threads` index lookup for bridge/effect
    /// routing. The vector remains the source of insertion/display order.
    pub(crate) thread_id_index: HashMap<String, usize>,
    /// O(1) remote ACP session -> `threads` index lookup. A session can be
    /// absent while a newly-created thread is still pre-attach.
    pub(crate) session_id_index: HashMap<String, usize>,
    pub selected_thread: usize,
    pub compose_text: String,
    pub search_query: String,
    pub visible_indices: Vec<usize>,
    /// Index-parallel expand flags for the *currently displayed* list only.
    /// Durable expand lives on each retained thread's `MessageItem` rows.
    pub expanded: Vec<bool>,
    pub displayed_thread: Option<usize>,
    /// Who currently owns `messages_model` (durable thread id). Must match
    /// the displayed thread after every install; used to refuse cross-thread
    /// writes and detect owner mismatch after selection.
    #[cfg(test)]
    pub list_owner_thread_id: Option<String>,
    pub expanded_terminal_id: Option<String>,
    /// Terminal-tabs phase: every terminal id currently pinned open as a
    /// full-view tab, in the order tabs were first opened (not
    /// insertion-into-`terminals` order, which can shuffle independently
    /// on refresh). `expanded_terminal_id` above is the *active* one --
    /// always a member of this list while the overlay is open, and always
    /// `None`/empty together (see `update_terminal`'s `Expand`/
    /// `CloseTab`/`CloseOverlay` arms, which keep the two in lockstep).
    pub open_terminal_ids: Vec<String>,
    pub active_project_path: Option<String>,
    /// Lifecycle identity; `active_project_path` remains as a UI/compatibility
    /// projection until the per-project store migration is complete.
    pub active_project: ProjectIdentity,
    /// Monotonic lifecycle generation; every host project transition bumps
    /// this before any asynchronous snapshot/control-client work starts.
    pub project_generation: u64,
    pub project_lifecycle_reason: String,
    /// PISO-2 (project-isolation-mlt-binding plan): the `active_project_
    /// path` value the currently-applied thread list (`visible_indices`/
    /// `thread_rows`) was actually synced against -- distinct from
    /// `active_project_path` itself, which flips the INSTANT `HostMsg::
    /// ProjectPathChanged` arrives, one full reducer turn before the next
    /// poll tick's `ThreadListSnapshot` (collected against the new value)
    /// can land and get folded in. `update_frame`'s stale-snapshot guard
    /// compares an incoming snapshot's own tagged `active_project_path`
    /// against `Model::active_project_path` (drop if they disagree -- an
    /// in-flight fetch for a project the user has since left); this field
    /// is compared instead against the snapshot's tag to detect "this
    /// fold is the first one for a NEW project" so the selection reanchor
    /// can jump to that project's first thread instead of clamping to
    /// whatever numeric index the old, unrelated project's selection
    /// happened to be at (see `update_frame`'s `if let Some(snapshot) =
    /// frame.thread_list_snapshot` block).
    pub synced_project_path: Option<String>,
    /// PISO-8 (project-isolation-mlt-binding plan): every project with a
    /// currently-live snapshotd instance, as of the last successful
    /// `Effect::RefreshDaemonProjectInstances` poll (throttled, see
    /// `FrameInput::daemon_projects_refresh_due`). A failed poll leaves
    /// this at its previous value rather than clearing it -- see
    /// `EffectResultMsg::DaemonProjectInstancesLoaded`'s doc comment.
    /// Read directly (not via a `Dirty`) by `ExternalSnapshotSource::
    /// collect_thread_list_snapshot`'s next tick, same as `active_
    /// project_path` itself.
    pub live_daemon_projects: Vec<crate::agent_bridge::DaemonProjectInstance>,
    pub traced_attachment_threads: HashSet<String>,
    pub appearance: AppearanceState,
    pub theme_variant: String,
    // language-switch-sync plan: a QSettings locale code (e.g. "fr",
    // "zh_CN") pushed from Qt -- both once at ChatRustDock construction
    // (cold-start seed, mirroring updateProjectPath's own construction-
    // time call) and live on every later Settings > Language switch
    // (mirroring producerOpened's live-signal wiring). "" (Default)
    // means no push has happened yet -- sync() only calls
    // select_bundled_translation when this is non-empty; absent that
    // call, @tr() strings simply show their literal English source text.
    pub language: String,
    pub settings_open: bool,
    pub settings_scope: String,
    pub default_profile: String,
    pub permission_profile: String,
    pub background_default: bool,
    pub default_agent_id: String,
    pub dev_mode: bool,
    pub onboarding_completed: bool,
    /// Feature-flag gate (env-var driven, see `PANEL_PROFILE_WIRING_ENABLED`
    /// in lib.rs) for the "default profile"/"permission profile" settings
    /// controls in `agents_view.slint`. Both are genuinely dual-tier
    /// (visible/working under Project and Global scope alike, unlike the
    /// six categories gated Global-only in 6745aa0e) but are hidden
    /// entirely, in both scopes, until this flag is turned on. Computed
    /// once at startup from the environment and never mutated afterward.
    pub profile_wiring_enabled: bool,
    /// Feature-flag gate (env-var driven, see `beta_mode_enabled()` in
    /// lib.rs, `BETA_MODE`) for in-development UI surfaces: the Chat
    /// Defaults "Profile" field (`agents_view.slint`, additive on top of
    /// `profile_wiring_enabled` above) and the whole Harness settings tab
    /// (`left_tabs.slint` / `settings_page.slint`). Computed once at
    /// startup from the environment and never mutated afterward.
    pub beta_mode_enabled: bool,
    pub background_override_set: bool,
    pub background_override: bool,
    /// Skills settings view's "Show global skills" row
    /// (`skills_view.slint`). Genuinely dual-tier, same
    /// Project-overrides-Global mechanism as `background_default` --
    /// see `settings_file::SettingsDocument::show_global_skills`'s doc
    /// comment.
    pub show_global_skills: bool,
    pub available_profiles: Vec<crate::gateway_actor::ProfileSummary>,
    pub available_mcp_servers: Vec<crate::protocol_types::McpServerEntry>,
    pub agent_catalog: Vec<crate::protocol_types::AgentCatalogEntry>,
    pub agent_catalog_fetched: bool,
    pub agent_operations_in_flight: Vec<String>,
    /// Same shape/lifecycle as `agent_operations_in_flight` above, sourced
    /// from `AgentBridge::mcp_operations_in_flight` -- keys are `"<action>:
    /// <server-name>"` (see that method's own doc comment). Folded into
    /// `McpServerOption`'s per-row busy booleans by `to_mcp_server_option_
    /// rows`, driving the Spinner shown on whichever button's action is
    /// actually in flight.
    pub mcp_operations_in_flight: Vec<String>,
    /// Same shape/lifecycle as `mcp_operations_in_flight` above, sourced
    /// from `AgentBridge::recover_session_operations_in_flight`. Folded
    /// into `RemoteSessionOption.busy` by `to_remote_session_option_
    /// rows`, driving the Attach button's Spinner while a recoverable
    /// session's `session/load` is in flight (symptom #2: this row
    /// previously had no busy-state tracking at all).
    pub recover_session_operations_in_flight: Vec<String>,
    pub recoverable_sessions: Vec<crate::gateway_actor::RemoteThreadInfo>,
    pub recovery_provider: String,
    /// Review-gate fix (phase 32): true once a real thread-list snapshot
    /// has been folded. Before that, an empty `visible_indices` means
    /// "no filter applied yet" and index helpers fall back to all
    /// threads; after it, an empty visible list is REAL (e.g. the
    /// phase-26 project scope matched nothing) and the fallback must not
    /// silently retarget hidden threads.
    pub visible_list_synced: bool,
    /// Plan phase 28: shared action-feedback toast. `toast_seq` bumps on
    /// every show so the UI can restart its auto-hide timer even for an
    /// identical message.
    pub toast_message: String,
    pub toast_kind: String,
    pub toast_seq: i32,
    pub provider_errors: HashMap<String, String>,
    /// mcp-servers-settings follow-up (chat-view provider-switch loading
    /// indicator): providers with a `probe_provider_selection` acquire+
    /// release round-trip currently in flight (see `SettingsMsg::
    /// ProfileSelected`'s `Effect::ProbeProvider` dispatch for the insert
    /// and `AgentEvent::ProviderProbe`'s handling in `update_frame` for the
    /// removal on completion). Keyed by provider like `provider_errors`
    /// above, not by thread -- the probe itself has no other per-thread
    /// state worth tracking and this mirrors `provider_errors`'s own
    /// "keyed off the selected thread's *provider*" contract exactly (see
    /// `sync::selected_provider_unavailable`'s doc comment).
    pub provider_probes_in_flight: HashSet<String>,
    /// mcp-servers-settings follow-up (chat-view first-attach loading
    /// indicator): thread ids whose FIRST real ACP session attach --
    /// deferred to first send, see `dispatch::dispatch_compose_send_
    /// maybe_attach`'s doc comment -- is currently in flight. Inserted
    /// right there, in the same synchronous call that kicks off
    /// `AgentBridge::attach_deferred_thread_with_config_options`
    /// (`EffectResultMsg::SessionAttachStarted`); removed either on
    /// success -- `update_frame`'s `frame.thread_list_snapshot` fold,
    /// the real place a background attach's `session_id` transitions
    /// `None` -> `Some` today (`SessionAttached`'s `Ok` arm is dead for
    /// this path, see its own doc comment) -- or on failure, via the
    /// existing `SessionAttached` `Err` arm (synchronous provisioning
    /// failure) and `AgentEvent::Error` (async attach failure). Keyed by
    /// thread_id like `provider_probes_in_flight` is keyed by provider --
    /// this is inherently a per-thread transition, not a per-provider one.
    pub first_attach_in_flight: HashSet<String>,
    pub active_skill_name: String,
    pub active_skill_path: String,
    /// PUI-010: the actual SKILL.md file path (active_skill_path is the
    /// containing directory) -- content saves must write here, not the
    /// directory. See SkillEditorState::content_path's doc comment.
    pub active_skill_md_path: String,
    pub active_skill_content: String,
    /// skills_audit_report §3.1: true while SkillWrite is in flight.
    pub skill_saving: bool,
    pub detected_editors: Vec<String>,
    pub active_pane: String,
    pub skills: Vec<crate::skills_state::SkillEntry>,
    pub thread_rows: Vec<crate::models::VisibleThreadItem>,
    /// Persistent Slint models. `sync()` mutates these in place so row
    /// delegates retain identity across unrelated inserts/removals.
    pub thread_model: Rc<VecModel<crate::ThreadItem>>,
    pub thread_model_keys: RefCell<Vec<String>>,
    /// Stable Slint records that expose one thread-owned message model per
    /// durable thread to the UI.
    pub thread_views_model: Rc<VecModel<crate::ThreadViewItem>>,
    pub thread_views_model_keys: RefCell<Vec<String>>,
    /// Test-only fixtures for the retired shared-message implementation.
    /// Production ownership is exclusively in `thread_view_models`.
    #[cfg(test)]
    pub messages_model: Rc<VecModel<crate::MessageItem>>,
    #[cfg(test)]
    pub message_model_keys: RefCell<Vec<String>>,
    /// Stable per-thread Slint-facing message models. This is the sole
    /// production ownership foundation for retained ChatView instances.
    pub thread_view_models: crate::thread_view::ThreadViewModels,
    pub skills_model: Rc<VecModel<crate::SkillOption>>,
    /// PUI-003: the displayed thread's ACP available_commands, projected as
    /// SkillOption rows (name+description) for the compose `/` menu. Reuses
    /// SkillOption rather than a new Slint struct since the shape matches.
    pub commands_model: Rc<VecModel<crate::SkillOption>>,
    pub skill_model_keys: RefCell<Vec<std::path::PathBuf>>,
    pub profiles_model: Rc<VecModel<crate::ProfileOption>>,
    pub profile_model_keys: RefCell<Vec<String>>,
    pub mcp_servers_model: Rc<VecModel<crate::McpServerOption>>,
    pub mcp_server_model_keys: RefCell<Vec<String>>,
    pub agent_catalog_model: Rc<VecModel<crate::AgentCatalogEntry>>,
    pub agent_catalog_model_keys: RefCell<Vec<String>>,
    pub recoverable_sessions_model: Rc<VecModel<crate::RemoteSessionOption>>,
    pub recoverable_session_model_keys: RefCell<Vec<String>>,
    /// Agent terminals for the *currently displayed* thread. Reconciled
    /// in place so streaming output does not tear down row delegates.
    pub terminals_model: Rc<VecModel<crate::TerminalItem>>,
    pub terminal_model_keys: RefCell<Vec<String>>,
    /// Terminal-tabs phase: the currently displayed thread's open tab set
    /// (`ThreadModel::open_terminals`), reconciled the same way as
    /// `terminals_model` above so a streaming output update to one tab
    /// doesn't tear down and rebuild every tab's row delegate (which would
    /// reset the very per-tab `Flickable` scroll position tabs exist to
    /// preserve).
    pub open_terminals_model: Rc<VecModel<crate::TerminalItem>>,
    pub open_terminal_model_keys: RefCell<Vec<String>>,
}

/// The Slint-facing models and their identity caches survive reducer
/// hydration. Keep this inventory in one place: `InitialStateLoaded` can
/// replace all ordinary TEA fields without accidentally replacing a live
/// VecModel or dropping its paired key cache.
pub(crate) struct PersistentModels {
    pub(crate) thread_model: Rc<VecModel<crate::ThreadItem>>,
    pub(crate) thread_model_keys: Vec<String>,
    pub(crate) thread_views_model: Rc<VecModel<crate::ThreadViewItem>>,
    pub(crate) thread_views_model_keys: Vec<String>,
    #[cfg(test)]
    pub(crate) messages_model: Rc<VecModel<crate::MessageItem>>,
    #[cfg(test)]
    pub(crate) message_model_keys: Vec<String>,
    pub(crate) thread_view_models: crate::thread_view::ThreadViewModels,
    pub(crate) skills_model: Rc<VecModel<crate::SkillOption>>,
    pub(crate) skill_model_keys: Vec<std::path::PathBuf>,
    pub(crate) commands_model: Rc<VecModel<crate::SkillOption>>,
    pub(crate) profiles_model: Rc<VecModel<crate::ProfileOption>>,
    pub(crate) profile_model_keys: Vec<String>,
    pub(crate) mcp_servers_model: Rc<VecModel<crate::McpServerOption>>,
    pub(crate) mcp_server_model_keys: Vec<String>,
    pub(crate) agent_catalog_model: Rc<VecModel<crate::AgentCatalogEntry>>,
    pub(crate) agent_catalog_model_keys: Vec<String>,
    pub(crate) recoverable_sessions_model: Rc<VecModel<crate::RemoteSessionOption>>,
    pub(crate) recoverable_session_model_keys: Vec<String>,
    pub(crate) terminals_model: Rc<VecModel<crate::TerminalItem>>,
    pub(crate) terminal_model_keys: Vec<String>,
    pub(crate) open_terminals_model: Rc<VecModel<crate::TerminalItem>>,
    pub(crate) open_terminal_model_keys: Vec<String>,
}

impl Model {
    pub(crate) fn persistent_models(&self) -> PersistentModels {
        PersistentModels {
            thread_model: self.thread_model.clone(),
            thread_model_keys: self.thread_model_keys.borrow().clone(),
            thread_views_model: self.thread_views_model.clone(),
            thread_views_model_keys: self.thread_views_model_keys.borrow().clone(),
            #[cfg(test)]
            messages_model: self.messages_model.clone(),
            #[cfg(test)]
            message_model_keys: self.message_model_keys.borrow().clone(),
            thread_view_models: self.thread_view_models.clone(),
            skills_model: self.skills_model.clone(),
            skill_model_keys: self.skill_model_keys.borrow().clone(),
            commands_model: self.commands_model.clone(),
            profiles_model: self.profiles_model.clone(),
            profile_model_keys: self.profile_model_keys.borrow().clone(),
            mcp_servers_model: self.mcp_servers_model.clone(),
            mcp_server_model_keys: self.mcp_server_model_keys.borrow().clone(),
            agent_catalog_model: self.agent_catalog_model.clone(),
            agent_catalog_model_keys: self.agent_catalog_model_keys.borrow().clone(),
            recoverable_sessions_model: self.recoverable_sessions_model.clone(),
            recoverable_session_model_keys: self.recoverable_session_model_keys.borrow().clone(),
            terminals_model: self.terminals_model.clone(),
            terminal_model_keys: self.terminal_model_keys.borrow().clone(),
            open_terminals_model: self.open_terminals_model.clone(),
            open_terminal_model_keys: self.open_terminal_model_keys.borrow().clone(),
        }
    }

    pub(crate) fn restore_persistent_models(&mut self, persistent: PersistentModels) {
        self.thread_model = persistent.thread_model;
        *self.thread_model_keys.borrow_mut() = persistent.thread_model_keys;
        self.thread_views_model = persistent.thread_views_model;
        *self.thread_views_model_keys.borrow_mut() = persistent.thread_views_model_keys;
        #[cfg(test)]
        {
            self.messages_model = persistent.messages_model;
            *self.message_model_keys.borrow_mut() = persistent.message_model_keys;
        }
        self.thread_view_models = persistent.thread_view_models;
        self.skills_model = persistent.skills_model;
        *self.skill_model_keys.borrow_mut() = persistent.skill_model_keys;
        self.commands_model = persistent.commands_model;
        self.profiles_model = persistent.profiles_model;
        *self.profile_model_keys.borrow_mut() = persistent.profile_model_keys;
        self.mcp_servers_model = persistent.mcp_servers_model;
        *self.mcp_server_model_keys.borrow_mut() = persistent.mcp_server_model_keys;
        self.agent_catalog_model = persistent.agent_catalog_model;
        *self.agent_catalog_model_keys.borrow_mut() = persistent.agent_catalog_model_keys;
        self.recoverable_sessions_model = persistent.recoverable_sessions_model;
        *self.recoverable_session_model_keys.borrow_mut() =
            persistent.recoverable_session_model_keys;
        self.terminals_model = persistent.terminals_model;
        *self.terminal_model_keys.borrow_mut() = persistent.terminal_model_keys;
        self.open_terminals_model = persistent.open_terminals_model;
        *self.open_terminal_model_keys.borrow_mut() = persistent.open_terminal_model_keys;
    }
}

impl Default for ThreadModel {
    fn default() -> Self {
        Self {
            thread_id: String::new(),
            display_name: String::new(),
            provider: String::new(),
            profile_name: None,
            permission_profile: None,
            session_id: None,
            state: ThreadState::Idle,
            last_activity_time: Some(std::time::Instant::now()),
            error: None,
            unauthenticated: false,
            agent_content_this_turn: false,
            send_queue: SendQueue::default(),
            server_queue: true,
            compose_draft: String::new(),
            closed: false,
            archived: false,
            unread: false,
            message_ids: Vec::new(),
            transcript: Vec::new(),
            transcript_keys: Vec::new(),
            rows_synced_with: None,
            #[cfg(test)]
            message_rows: Vec::new(),
            markdown_render_index: RefCell::new(crate::thread_message_index::ThreadMessageIndex::default()),
            markdown_epoch: crate::markdown_worker::EpochCounter::new(),
            markdown_in_flight: crate::markdown_worker::InFlightRegistry::new(),
            has_older_messages: false,
            pending_request: crate::PendingRequestItem::default(),
            terminals: Vec::new(),
            expanded_terminal: None,
            open_terminals: Vec::new(),
            local_terminal: crate::LocalTerminalItem::default(),
            connection_status: "Connecting...".to_owned(),
            session_modes: None,
            config_options: Vec::new(),
            available_commands: Vec::new(),
            command_filter: String::new(),
            usage: (0, 0),
            plan: Vec::new(),
            session_title: None,
        }
    }
}

impl Model {
    /// Rebuild the identity indices after a structural or identity change.
    /// Durable ids and session ids are intentionally indexed separately so a
    /// durable id always wins when an incoming identifier could match both.
    pub(crate) fn rebuild_thread_indices(&mut self) {
        self.thread_id_index.clear();
        self.session_id_index.clear();
        for (index, thread) in self.threads.iter().enumerate() {
            if !thread.thread_id.is_empty() {
                self.thread_id_index
                    .entry(thread.thread_id.clone())
                    .or_insert(index);
            }
            if let Some(session_id) = thread.session_id.as_deref().filter(|id| !id.is_empty()) {
                self.session_id_index
                    .entry(session_id.to_owned())
                    .or_insert(index);
            }
        }
        let thread_ids = self
            .threads
            .iter()
            .map(|thread| thread.thread_id.as_str())
            .collect::<Vec<_>>();
        self.thread_view_models
            .retain_thread_ids(thread_ids.iter().copied());
        self.thread_view_models
            .ensure_for_thread_ids(thread_ids.iter().copied());
    }

    /// Resolve an incoming durable thread or remote session identity. The
    /// map entry is validated against the vector because `threads` is still a
    /// public ordered vector and a few legacy/test callers mutate it
    /// directly; those callers get a correct linear fallback until the next
    /// normal reducer rebuilds the indices.
    pub(crate) fn thread_index_for_id(&self, id: &str) -> Option<usize> {
        if id.is_empty() {
            return None;
        }
        if let Some(index) = self.thread_id_index.get(id).copied() {
            if self
                .threads
                .get(index)
                .is_some_and(|thread| thread.thread_id == id)
            {
                return Some(index);
            }
        }
        if let Some(index) = self.session_id_index.get(id).copied() {
            if self
                .threads
                .get(index)
                .is_some_and(|thread| thread.session_id.as_deref() == Some(id))
            {
                return Some(index);
            }
        }
        self.threads
            .iter()
            .position(|thread| thread.thread_id == id)
            .or_else(|| {
                self.threads
                    .iter()
                    .position(|thread| thread.session_id.as_deref() == Some(id))
            })
    }

    /// Folds `Effect::LoadInitialState`'s result into a fresh `Model` --
    /// the one legitimate "everything is dirty" case, since there is no
    /// prior row identity to preserve on cold start (see 00-plan.md's
    /// "Known gap: list resets still break row identity / animations").
    pub fn from_initial_state(initial: InitialState) -> Self {
        let selected_thread_id = initial.selected_thread_id;
        let threads: Vec<ThreadModel> = initial
            .threads
            .into_iter()
            .enumerate()
            .map(|(idx, spec)| ThreadModel {
                thread_id: initial
                    .thread_ids
                    .get(idx)
                    .cloned()
                    .filter(|id| !id.is_empty())
                    .or_else(|| spec.session_id.clone())
                    .unwrap_or_else(|| format!("thread:{idx}")),
                display_name: spec.display_name,
                provider: spec.provider,
                profile_name: spec.profile_name,
                permission_profile: initial.permission_profiles.get(idx).cloned().flatten(),
                state: initial
                    .thread_states
                    .get(idx)
                    .cloned()
                    .unwrap_or(ThreadState::Idle),
                session_id: spec.session_id,
                send_queue: initial.send_queues.get(idx).cloned().unwrap_or_default(),
                server_queue: initial.server_queue,
                ..ThreadModel::default()
            })
            .collect();
        let selected_thread = selected_thread_id
            .as_deref()
            .and_then(|thread_id| {
                threads
                    .iter()
                    .position(|thread| thread.session_id.as_deref() == Some(thread_id))
            })
            .unwrap_or(0);
        let thread_count = threads.len();
        let mut model = Self {
            threads,
            selected_thread,
            visible_indices: (0..thread_count).collect(),
            onboarding_completed: initial.onboarding_completed,
            ..Self::default()
        };
        model.rebuild_thread_indices();
        model
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_initial_state_cold_start_empty_db_produces_no_threads() {
        let model = Model::from_initial_state(InitialState {
            threads: vec![],
            thread_ids: vec![],
            selected_thread_id: None,
            permission_profiles: vec![],
            thread_states: vec![],
            startup_warnings: vec![],
            send_queues: vec![],
            server_queue: true,
            onboarding_completed: false,
        });
        assert!(model.threads.is_empty());
        assert_eq!(model.selected_thread, 0);
    }

    #[test]
    fn from_initial_state_restores_existing_thread_records() {
        let initial = InitialState {
            threads: vec![
                ThreadSpec {
                    display_name: "Fix timeline crash".to_owned(),
                    provider: "codex".to_owned(),
                    session_id: Some("sess-1".to_owned()),
                    profile_name: None,
                    project_path: None,
                },
                ThreadSpec {
                    display_name: "Refactor filters".to_owned(),
                    provider: "claude".to_owned(),
                    session_id: Some("sess-2".to_owned()),
                    profile_name: Some("default".to_owned()),
                    project_path: None,
                },
            ],
            thread_ids: vec!["thread-1".to_owned(), "thread-2".to_owned()],
            selected_thread_id: Some("sess-2".to_owned()),
            permission_profiles: vec![None, None],
            thread_states: vec![ThreadState::Idle, ThreadState::Idle],
            startup_warnings: vec![],
            send_queues: vec![],
            server_queue: true,
            onboarding_completed: false,
        };
        let model = Model::from_initial_state(initial);
        assert_eq!(model.threads.len(), 2);
        assert_eq!(model.threads[0].display_name, "Fix timeline crash");
        assert_eq!(model.threads[0].session_id.as_deref(), Some("sess-1"));
        assert_eq!(model.threads[1].provider, "claude");
        assert_eq!(model.threads[1].profile_name.as_deref(), Some("default"));
        assert_eq!(model.selected_thread, 1);
        // Every restored thread starts idle/error-free, mirroring
        // panel_rust_create's current behavior of never restoring
        // in-flight loading/error state across a restart.
        assert!(model.threads.iter().all(|t| t.state == ThreadState::Idle));
        assert!(model.threads.iter().all(|t| t.error.is_none()));
    }

    #[test]
    fn from_initial_state_restores_runtime_thread_fields_through_hydration() {
        let model = Model::from_initial_state(InitialState {
            threads: vec![ThreadSpec {
                display_name: "Needs approval".to_owned(),
                provider: "codex".to_owned(),
                session_id: Some("sess-1".to_owned()),
                profile_name: Some("balanced".to_owned()),
                project_path: None,
            }],
            thread_ids: vec!["thread-1".to_owned()],
            selected_thread_id: None,
            permission_profiles: vec![Some("workspace".to_owned())],
            thread_states: vec![ThreadState::Error],
            startup_warnings: vec![],
            send_queues: vec![],
            server_queue: true,
            onboarding_completed: false,
        });
        assert_eq!(model.threads[0].profile_name.as_deref(), Some("balanced"));
        assert_eq!(
            model.threads[0].permission_profile.as_deref(),
            Some("workspace")
        );
        assert_eq!(model.threads[0].state, ThreadState::Error);
    }

    #[test]
    fn thread_identity_indices_resolve_durable_and_session_ids() {
        let model = Model::from_initial_state(InitialState {
            threads: vec![
                ThreadSpec {
                    display_name: "First".to_owned(),
                    provider: "codex".to_owned(),
                    session_id: Some("session-first".to_owned()),
                    profile_name: None,
                    project_path: None,
                },
                ThreadSpec {
                    display_name: "Second".to_owned(),
                    provider: "claude".to_owned(),
                    session_id: Some("session-second".to_owned()),
                    profile_name: None,
                    project_path: None,
                },
            ],
            thread_ids: vec!["thread-first".to_owned(), "thread-second".to_owned()],
            selected_thread_id: None,
            permission_profiles: vec![None, None],
            thread_states: vec![ThreadState::Idle, ThreadState::Idle],
            startup_warnings: vec![],
            send_queues: vec![],
            server_queue: true,
            onboarding_completed: false,
        });

        assert_eq!(model.thread_index_for_id("thread-second"), Some(1));
        assert_eq!(model.thread_index_for_id("session-first"), Some(0));
        assert_eq!(model.thread_index_for_id("missing"), None);
    }
}
