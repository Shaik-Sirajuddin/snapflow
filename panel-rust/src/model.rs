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
use crate::protocol_types::{ConfigOptionInfo, SessionModesEvent};
use crate::send_queue::SendQueue;
use slint::VecModel;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

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
    pub error: Option<String>,
    pub send_queue: SendQueue,
    /// Per-thread compose draft (leak_audit_report §2.5 / §4.2). The
    /// global `Model::compose_text` is only the *active* buffer for the
    /// displayed thread; switching saves/restores via this field.
    pub compose_draft: String,
    pub closed: bool,
    // setup-followups plan, archive_thread_backend_verify: purely local
    // presentation flag (see AgentBridge::archive_thread's doc comment) --
    // never sends an ACP request, unlike `closed`.
    pub archived: bool,
    /// Stable message identities currently known to the TEA model. Streaming
    /// effect results must resolve against this list, never a cached row
    /// index, before producing a `Dirty::MessageStreamingDelta`.
    pub message_ids: Vec<String>,
    pub transcript: Vec<TranscriptItem>,
    pub transcript_keys: Vec<String>,
    pub message_rows: Vec<crate::MessageItem>,
    pub has_older_messages: bool,
    pub pending_request: crate::PendingRequestItem,
    pub terminals: Vec<crate::TerminalItem>,
    pub expanded_terminal: Option<crate::TerminalItem>,
    pub local_terminal: crate::LocalTerminalItem,
    pub connection_status: String,
    pub session_modes: Option<SessionModesEvent>,
    pub config_options: Vec<ConfigOptionInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SkillEditorState {
    pub name: String,
    pub path: String,
    pub content: String,
    pub detected_editors: Vec<String>,
}

#[derive(Clone, Default)]
pub struct Model {
    pub threads: Vec<ThreadModel>,
    pub selected_thread: usize,
    pub compose_text: String,
    pub search_query: String,
    pub visible_indices: Vec<usize>,
    pub expanded: Vec<bool>,
    pub displayed_thread: Option<usize>,
    pub expanded_terminal_id: Option<String>,
    pub local_terminal_last_text: String,
    pub active_project_path: Option<String>,
    pub traced_attachment_threads: HashSet<String>,
    pub appearance: AppearanceState,
    pub theme_variant: String,
    pub settings_open: bool,
    pub settings_scope: String,
    pub default_profile: String,
    pub permission_profile: String,
    pub background_default: bool,
    pub default_agent_id: String,
    pub dev_mode: bool,
    pub background_override_set: bool,
    pub background_override: bool,
    pub available_profiles: Vec<crate::gateway_actor::ProfileSummary>,
    pub available_mcp_servers: Vec<crate::protocol_types::McpServerEntry>,
    pub agent_catalog: Vec<crate::protocol_types::AgentCatalogEntry>,
    pub recoverable_sessions: Vec<crate::gateway_actor::RemoteThreadInfo>,
    pub recovery_provider: String,
    pub active_skill_name: String,
    pub active_skill_path: String,
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
    pub messages_model: Rc<VecModel<crate::MessageItem>>,
    pub message_model_keys: RefCell<Vec<String>>,
    pub skills_model: Rc<VecModel<crate::SkillOption>>,
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
            error: None,
            send_queue: SendQueue::default(),
            compose_draft: String::new(),
            closed: false,
            archived: false,
            message_ids: Vec::new(),
            transcript: Vec::new(),
            transcript_keys: Vec::new(),
            message_rows: Vec::new(),
            has_older_messages: false,
            pending_request: crate::PendingRequestItem::default(),
            terminals: Vec::new(),
            expanded_terminal: None,
            local_terminal: crate::LocalTerminalItem::default(),
            connection_status: "Connecting...".to_owned(),
            session_modes: None,
            config_options: Vec::new(),
        }
    }
}

impl Model {
    pub(crate) fn thread_matches_id(thread: &ThreadModel, id: &str) -> bool {
        id.is_empty() || thread.thread_id == id || thread.session_id.as_deref() == Some(id)
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
        Self {
            threads,
            selected_thread,
            visible_indices: (0..thread_count).collect(),
            ..Self::default()
        }
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
                },
                ThreadSpec {
                    display_name: "Refactor filters".to_owned(),
                    provider: "claude".to_owned(),
                    session_id: Some("sess-2".to_owned()),
                    profile_name: Some("default".to_owned()),
                },
            ],
            thread_ids: vec!["thread-1".to_owned(), "thread-2".to_owned()],
            selected_thread_id: Some("sess-2".to_owned()),
            permission_profiles: vec![None, None],
            thread_states: vec![ThreadState::Idle, ThreadState::Idle],
            startup_warnings: vec![],
            send_queues: vec![],
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
            }],
            thread_ids: vec!["thread-1".to_owned()],
            selected_thread_id: None,
            permission_profiles: vec![Some("workspace".to_owned())],
            thread_states: vec![ThreadState::Error],
            startup_warnings: vec![],
            send_queues: vec![],
        });
        assert_eq!(model.threads[0].profile_name.as_deref(), Some("balanced"));
        assert_eq!(
            model.threads[0].permission_profile.as_deref(),
            Some("workspace")
        );
        assert_eq!(model.threads[0].state, ThreadState::Error);
    }
}
