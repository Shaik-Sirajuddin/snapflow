//! `tea-slint-model` Phase 0/1: `Model` is today's `PanelSingleton` state
//! fields, minus the `component: ChatPanel` handle, the render buffer,
//! and the window -- those stay owned by the FFI/render layer, not by
//! `update()`. See `memory/rui/gen/plans/tea-slint-model/00-plan.md`'s
//! ownership table: `Model` is mutated only inside `update()`, and
//! nothing outside `sync()` reads it to push a Slint `set_*` setter.
//!
//! **Status: additive, not yet wired.** This struct exists so Phase 2's
//! `update()` has a concrete type to operate on; `panel_rust_create` does
//! not construct one yet -- that lands with Phase 0's actual cutover,
//! once `Effect::LoadInitialState`/`EffectResultMsg::InitialStateLoaded`
//! are wired through a real `update()` (Phase 2). Until then
//! `PanelSingleton` remains the sole source of truth and this module has
//! no live callers, by design (see 00-plan.md Phase 4's "old closures for
//! not-yet-migrated domains keep working unchanged in the interim").

use crate::agent_bridge::ThreadSpec;
use crate::models::ThreadState;
use crate::send_queue::SendQueue;
use std::collections::HashSet;

/// Result of `Effect::LoadInitialState` -- the same data
/// `panel_rust_create` reads from `PanelStateStore` today (thread
/// records, or the default thread set when the store is empty/missing),
/// now shaped as a plain value `update()` can fold into a fresh `Model`
/// with no Slint/FFI dependency, so it stays unit-testable per Phase 2's
/// verification requirement.
#[derive(Debug, Clone, PartialEq)]
pub struct InitialState {
    pub threads: Vec<ThreadSpec>,
    pub selected_thread_id: Option<String>,
}

/// One thread's `Model`-side state -- the parallel-array fields
/// `PanelSingleton` keeps today (`thread_names`, `thread_profiles`,
/// `thread_state`, `send_queues`, ...), grouped per thread so `update()`
/// can no longer let them drift out of sync by construction. Migrating
/// the parallel-array representation itself is out of scope for Phase
/// 0/1 -- this mirrors today's fields 1:1; consolidating them into this
/// shape is Phase 2's job as each domain's closures are actually ported.
#[derive(Debug, Clone)]
pub struct ThreadModel {
    pub display_name: String,
    pub provider: String,
    pub profile_name: Option<String>,
    pub permission_profile: Option<String>,
    pub session_id: Option<String>,
    pub state: ThreadState,
    pub error: Option<String>,
    pub send_queue: SendQueue,
    pub closed: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Model {
    pub threads: Vec<ThreadModel>,
    pub selected_thread: usize,
    pub search_query: String,
    pub visible_indices: Vec<usize>,
    pub expanded: Vec<bool>,
    pub displayed_thread: Option<usize>,
    pub expanded_terminal_id: Option<String>,
    pub local_terminal_last_text: String,
    pub active_project_path: Option<String>,
    pub traced_attachment_threads: HashSet<String>,
    pub settings_open: bool,
    pub settings_scope: String,
}

impl Default for ThreadModel {
    fn default() -> Self {
        Self {
            display_name: String::new(),
            provider: String::new(),
            profile_name: None,
            permission_profile: None,
            session_id: None,
            state: ThreadState::Idle,
            error: None,
            send_queue: SendQueue::default(),
            closed: false,
        }
    }
}

impl Model {
    /// Folds `Effect::LoadInitialState`'s result into a fresh `Model` --
    /// the one legitimate "everything is dirty" case, since there is no
    /// prior row identity to preserve on cold start (see 00-plan.md's
    /// "Known gap: list resets still break row identity / animations").
    pub fn from_initial_state(initial: InitialState) -> Self {
        let threads = initial
            .threads
            .into_iter()
            .map(|spec| ThreadModel {
                display_name: spec.display_name,
                provider: spec.provider,
                profile_name: spec.profile_name,
                session_id: spec.session_id,
                ..ThreadModel::default()
            })
            .collect();
        Self {
            threads,
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
            selected_thread_id: None,
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
            selected_thread_id: Some("sess-1".to_owned()),
        };
        let model = Model::from_initial_state(initial);
        assert_eq!(model.threads.len(), 2);
        assert_eq!(model.threads[0].display_name, "Fix timeline crash");
        assert_eq!(model.threads[0].session_id.as_deref(), Some("sess-1"));
        assert_eq!(model.threads[1].provider, "claude");
        assert_eq!(model.threads[1].profile_name.as_deref(), Some("default"));
        // Every restored thread starts idle/error-free, mirroring
        // panel_rust_create's current behavior of never restoring
        // in-flight loading/error state across a restart.
        assert!(model.threads.iter().all(|t| t.state == ThreadState::Idle));
        assert!(model.threads.iter().all(|t| t.error.is_none()));
    }
}
