//! `tea-slint-model` Phase 2: `update(&mut Model, Msg) -> (Vec<Effect>,
//! Vec<Dirty>)` -- the **sole** owner of state transitions. See
//! `memory/rui/gen/plans/tea-slint-model/00-plan.md`.
//!
//! **Status: additive, not yet wired.** `lib.rs`'s `on_*` closures still
//! run their own bodies directly -- this module is not yet called from
//! anywhere except its own tests. Phase 4 replaces each closure body with
//! a one-line `dispatch()` wrapper that ends up calling `update()`, one
//! domain at a time. Until then this is dead code by design (see
//! `model.rs`'s doc comment for the same note).
//!
//! The top-level `match` below is intentionally exhaustive with **no
//! wildcard arm** -- see 00-plan.md's "Exhaustiveness requirement": a
//! future `Msg` variant added without a matching arm here must fail to
//! compile, not silently no-op.

use crate::dirty::{Dirty, ErrorDetail, RowOp, ScalarField};
use crate::effect::{Effect, EffectResultMsg};
use crate::model::{Model, ThreadModel};
use crate::models::ThreadState;
use crate::msg::{
    ChromeMsg, ComposeMsg, HostMsg, Msg, RequestMsg, SettingsMsg, SkillMsg, TerminalMsg,
    ThreadMsg, UiMsg,
};

pub fn update(model: &mut Model, msg: Msg) -> (Vec<Effect>, Vec<Dirty>) {
    match msg {
        Msg::Ui(ui) => update_ui(model, ui),
        Msg::Effect(effect_result) => update_effect(model, effect_result),
        Msg::Host(host) => update_host(model, host),
        Msg::Frame(frame) => update_frame(model, frame),
    }
}

fn update_ui(model: &mut Model, msg: UiMsg) -> (Vec<Effect>, Vec<Dirty>) {
    match msg {
        UiMsg::Thread(m) => update_thread(model, m),
        UiMsg::Compose(m) => update_compose(model, m),
        UiMsg::Request(m) => update_request(model, m),
        UiMsg::Terminal(m) => update_terminal(model, m),
        UiMsg::Settings(m) => update_settings(model, m),
        UiMsg::Skill(m) => update_skill(model, m),
        UiMsg::Chrome(m) => update_chrome(model, m),
    }
}

/// Ported verbatim from `lib.rs`'s pre-existing `wrap_thread_index` (kept
/// there too, unchanged, for the not-yet-migrated closure that still
/// calls it directly -- see that function's own doc comment). Duplicated
/// rather than `pub(crate) use`d across the module boundary for now to
/// avoid widening `lib.rs`'s private surface before Phase 4 actually
/// deletes the original call site; Phase 7 (`remove_superseded_code`)
/// collapses this back to one definition once the old closure is gone.
fn wrap_thread_index(current: usize, delta: i32, visible_len: usize) -> usize {
    if visible_len == 0 {
        return 0;
    }
    ((current as i64 + delta as i64).rem_euclid(visible_len as i64)) as usize
}

fn update_thread(model: &mut Model, msg: ThreadMsg) -> (Vec<Effect>, Vec<Dirty>) {
    match msg {
        ThreadMsg::New => {
            model.threads.push(ThreadModel {
                display_name: "New thread".to_owned(),
                provider: "codex".to_owned(),
                ..ThreadModel::default()
            });
            let at = model.threads.len() - 1;
            (
                vec![Effect::NewThread {
                    display_name: model.threads[at].display_name.clone(),
                    provider: model.threads[at].provider.clone(),
                }],
                vec![Dirty::ThreadListDiff(vec![RowOp::Insert {
                    at,
                    row: Default::default(),
                }])],
            )
        }
        ThreadMsg::Selected(idx) => {
            // Clamp, don't no-op, to match the real
            // `select_visible_thread`'s own `filtered_idx.min(visible_len
            // - 1)` -- an out-of-range idx still selects the last thread
            // rather than being silently ignored.
            let visible_len = model.threads.len();
            if visible_len == 0 {
                return (vec![], vec![]);
            }
            model.selected_thread = idx.min(visible_len - 1);
            (vec![], vec![Dirty::Scalar(ScalarField::SelectedThread)])
        }
        ThreadMsg::NavigateDelta(delta) => {
            let visible_len = model.visible_indices.len().max(model.threads.len());
            if visible_len == 0 {
                return (vec![], vec![]);
            }
            let next = wrap_thread_index(model.selected_thread, delta, visible_len);
            model.selected_thread = next;
            (vec![], vec![Dirty::Scalar(ScalarField::SelectedThread)])
        }
        ThreadMsg::CloseRequested(idx) => {
            let Some(thread) = model.threads.get_mut(idx) else {
                return (vec![], vec![]);
            };
            thread.closed = true;
            (
                vec![Effect::PersistThread { real_index: idx }],
                vec![Dirty::ThreadRow(idx)],
            )
        }
        ThreadMsg::DeleteRequested(idx) => {
            if idx >= model.threads.len() {
                return (vec![], vec![]);
            }
            model.threads.remove(idx);
            if model.selected_thread >= model.threads.len() && !model.threads.is_empty() {
                model.selected_thread = model.threads.len() - 1;
            }
            (
                vec![Effect::DeleteThread { real_index: idx }],
                vec![Dirty::ThreadListDiff(vec![RowOp::Remove { at: idx }])],
            )
        }
        ThreadMsg::RenameRequested(idx, name) => {
            let Some(thread) = model.threads.get_mut(idx) else {
                return (vec![], vec![]);
            };
            thread.display_name = name.clone();
            (
                vec![Effect::RenameThread { real_index: idx, name }],
                vec![Dirty::ThreadRow(idx)],
            )
        }
        ThreadMsg::ToggleBackground(idx) => {
            if idx >= model.threads.len() {
                return (vec![], vec![]);
            }
            (
                vec![Effect::PersistThread { real_index: idx }],
                vec![Dirty::ThreadRow(idx)],
            )
        }
        ThreadMsg::RecoverSessionAttach {
            session_id,
            provider,
            title,
        } => {
            model.threads.push(ThreadModel {
                display_name: title,
                provider: provider.clone(),
                session_id: Some(session_id.clone()),
                ..ThreadModel::default()
            });
            let at = model.threads.len() - 1;
            (
                vec![Effect::RecoverSessionAttach {
                    real_index: at,
                    session_id,
                    provider,
                    title: model.threads[at].display_name.clone(),
                }],
                vec![Dirty::ThreadListDiff(vec![RowOp::Insert {
                    at,
                    row: Default::default(),
                }])],
            )
        }
    }
}

fn update_compose(model: &mut Model, msg: ComposeMsg) -> (Vec<Effect>, Vec<Dirty>) {
    let idx = model.selected_thread;
    match msg {
        ComposeMsg::SendRequested(text) => {
            let Some(thread) = model.threads.get_mut(idx) else {
                return (vec![], vec![]);
            };
            thread.state = ThreadState::Loading;
            (
                vec![Effect::SendPrompt { real_index: idx, text }],
                vec![Dirty::Connection {
                    thread_id: thread.session_id.clone().unwrap_or_default(),
                }],
            )
        }
        ComposeMsg::StopRequested => {
            let Some(thread) = model.threads.get_mut(idx) else {
                return (vec![], vec![]);
            };
            thread.state = ThreadState::Cancelling;
            (
                vec![Effect::CancelGeneration { real_index: idx }],
                vec![Dirty::ThreadRow(idx)],
            )
        }
        ComposeMsg::GenerationStopped => {
            let Some(thread) = model.threads.get_mut(idx) else {
                return (vec![], vec![]);
            };
            thread.state = ThreadState::Idle;
            (vec![], vec![Dirty::ThreadRow(idx)])
        }
        // Pure text-parsing helpers -- no Model mutation, no Dirty. These
        // exist as Msg variants for coverage completeness (see
        // 00-plan.md's callback mapping table) but their real logic stays
        // in `models::active_token_*`/`replace_active_token`, called
        // directly by the (still-unmigrated) TextUtil global callbacks.
        ComposeMsg::MentionTokenPrefix { .. }
        | ComposeMsg::MentionTokenQuery { .. }
        | ComposeMsg::MentionTokenReplace { .. }
        | ComposeMsg::WordBoundaryBefore { .. }
        | ComposeMsg::ContainsCi { .. } => (vec![], vec![]),
    }
}

fn update_request(model: &mut Model, msg: RequestMsg) -> (Vec<Effect>, Vec<Dirty>) {
    let idx = model.selected_thread;
    match msg {
        RequestMsg::Approve(request_id) => (
            vec![Effect::RespondAgentRequest {
                real_index: idx,
                request_id,
                approve: true,
            }],
            vec![Dirty::PendingRequest {
                thread_id: model
                    .threads
                    .get(idx)
                    .and_then(|t| t.session_id.clone())
                    .unwrap_or_default(),
            }],
        ),
        RequestMsg::Reject(request_id) => (
            vec![Effect::RespondAgentRequest {
                real_index: idx,
                request_id,
                approve: false,
            }],
            vec![Dirty::PendingRequest {
                thread_id: model
                    .threads
                    .get(idx)
                    .and_then(|t| t.session_id.clone())
                    .unwrap_or_default(),
            }],
        ),
        RequestMsg::PermissionOptionSelected(request_id, option) => (
            vec![Effect::PermissionOptionSelected {
                real_index: idx,
                request_id,
                option,
            }],
            vec![],
        ),
        RequestMsg::LoadOlderRequested(thread_id) => (
            vec![Effect::LoadOlderMessages { real_index: idx }],
            vec![Dirty::MessagesDiff {
                thread_id,
                ops: vec![],
            }],
        ),
    }
}

fn update_terminal(model: &mut Model, msg: TerminalMsg) -> (Vec<Effect>, Vec<Dirty>) {
    match msg {
        TerminalMsg::Expand(id) => {
            model.expanded_terminal_id = Some(id.clone());
            (vec![], vec![Dirty::Terminal { id }])
        }
        TerminalMsg::CloseOverlay => {
            let id = model.expanded_terminal_id.take();
            (
                vec![],
                vec![id
                    .map(|id| Dirty::Terminal { id })
                    .unwrap_or(Dirty::LocalTerminal)],
            )
        }
        TerminalMsg::LocalToggle => (vec![Effect::LocalTerminalSpawn], vec![Dirty::LocalTerminal]),
        TerminalMsg::LocalClose => (vec![Effect::LocalTerminalKill], vec![Dirty::LocalTerminal]),
        TerminalMsg::LocalKeyInput(bytes) => {
            (vec![Effect::LocalTerminalWrite { bytes }], vec![])
        }
    }
}

fn update_settings(model: &mut Model, msg: SettingsMsg) -> (Vec<Effect>, Vec<Dirty>) {
    let idx = model.selected_thread;
    match msg {
        SettingsMsg::Open => {
            model.settings_open = true;
            (vec![], vec![Dirty::Scalar(ScalarField::SettingsOpen), Dirty::Settings])
        }
        SettingsMsg::Close => {
            model.settings_open = false;
            (vec![], vec![Dirty::Scalar(ScalarField::SettingsOpen)])
        }
        SettingsMsg::Save(doc) => (vec![Effect::SaveSettings { doc }], vec![Dirty::Settings]),
        SettingsMsg::ScopeChanged(scope) => {
            model.settings_scope = scope;
            (vec![], vec![Dirty::Scalar(ScalarField::SettingsScope), Dirty::Settings])
        }
        SettingsMsg::ConfigOptionSelected { key, value } => (
            vec![Effect::SetConfigOption { real_index: idx, key, value }],
            vec![Dirty::Settings],
        ),
        SettingsMsg::ModeSelected(mode) => (
            vec![Effect::SetMode { real_index: idx, mode }],
            vec![Dirty::Settings],
        ),
        SettingsMsg::DevModeToggled(enabled) => {
            (vec![Effect::SaveDevMode { enabled }], vec![Dirty::Settings])
        }
        SettingsMsg::McpServerCreate { name, command } => (
            vec![Effect::McpServerCreate { real_index: idx, name, command }],
            vec![Dirty::Settings],
        ),
        SettingsMsg::McpServerDelete { name } => (
            vec![Effect::McpServerDelete { real_index: idx, name }],
            vec![Dirty::Settings],
        ),
        SettingsMsg::McpServerEnabledChanged { name, enabled } => (
            vec![Effect::McpServerEnabledChanged { real_index: idx, name, enabled }],
            vec![Dirty::Settings],
        ),
        SettingsMsg::ProfileCreate {
            name,
            agent_id,
            terminal_enabled,
            fs_enabled,
        } => (
            vec![Effect::ProfileCreate {
                real_index: idx,
                name,
                agent_id,
                terminal_enabled,
                fs_enabled,
            }],
            vec![Dirty::Settings],
        ),
        SettingsMsg::ProfileDelete { name } => (
            vec![Effect::ProfileDelete { real_index: idx, name }],
            vec![Dirty::Settings],
        ),
        SettingsMsg::AgentInstallRequested { agent_id } => (
            vec![Effect::AgentInstallRequested { real_index: idx, agent_id }],
            vec![Dirty::Settings],
        ),
    }
}

fn update_skill(_model: &mut Model, msg: SkillMsg) -> (Vec<Effect>, Vec<Dirty>) {
    match msg {
        SkillMsg::NewSkillRequested { .. } => (vec![], vec![Dirty::SkillsListDiff(vec![])]),
        SkillMsg::ContentEdited { path, content } => {
            (vec![Effect::SkillWrite { path, content }], vec![Dirty::SkillsListDiff(vec![])])
        }
        SkillMsg::CopyPathRequested { .. } => (vec![], vec![]),
        SkillMsg::EditorOpenRequested { .. } | SkillMsg::OpenInEditorRequested { .. }
        | SkillMsg::OpenWithOsDefaultRequested { .. } => (vec![], vec![]),
        SkillMsg::PromoteToGlobal { path } => (
            vec![Effect::SkillPromoteToGlobal { path }],
            vec![Dirty::SkillsListDiff(vec![])],
        ),
    }
}

fn update_chrome(model: &mut Model, msg: ChromeMsg) -> (Vec<Effect>, Vec<Dirty>) {
    match msg {
        ChromeMsg::SearchChanged(query) => {
            model.search_query = query;
            (
                vec![],
                vec![
                    Dirty::Scalar(ScalarField::SearchQuery),
                    Dirty::ThreadListDiff(vec![]),
                ],
            )
        }
        ChromeMsg::SearchSubmitted { query, .. } => {
            model.search_query = query;
            (
                vec![],
                vec![
                    Dirty::Scalar(ScalarField::SearchQuery),
                    Dirty::ThreadListDiff(vec![]),
                ],
            )
        }
        ChromeMsg::ToggleExpanded(_) => (vec![], vec![]),
        ChromeMsg::ErrorBannerDismissed => (
            vec![],
            vec![Dirty::Error {
                thread_id: model
                    .threads
                    .get(model.selected_thread)
                    .and_then(|t| t.session_id.clone())
                    .unwrap_or_default(),
                detail: ErrorDetail {
                    message: String::new(),
                },
            }],
        ),
    }
}

fn update_host(model: &mut Model, msg: HostMsg) -> (Vec<Effect>, Vec<Dirty>) {
    match msg {
        HostMsg::InvokeCommand(_cmd) => (vec![], vec![]),
        HostMsg::InputKey { .. } => (vec![], vec![]),
        HostMsg::AppearanceChanged(_state) => (vec![], vec![Dirty::Settings]),
        HostMsg::ProjectPathChanged(path) => {
            model.active_project_path = path.clone();
            (
                vec![Effect::SetActiveProjectPath { path }],
                vec![Dirty::SkillsListDiff(vec![])],
            )
        }
        HostMsg::Init => (vec![Effect::LoadInitialState], vec![]),
    }
}

fn update_effect(model: &mut Model, msg: EffectResultMsg) -> (Vec<Effect>, Vec<Dirty>) {
    match msg {
        EffectResultMsg::InitialStateLoaded(Ok(initial)) => {
            *model = Model::from_initial_state(initial);
            // Cold start: everything is dirty, there is no prior row
            // identity to preserve (see 00-plan.md's known-gap section).
            (
                vec![],
                vec![
                    Dirty::ThreadListDiff(vec![]),
                    Dirty::Scalar(ScalarField::SelectedThread),
                ],
            )
        }
        EffectResultMsg::InitialStateLoaded(Err(err)) => (
            vec![],
            vec![Dirty::Error {
                thread_id: String::new(),
                detail: ErrorDetail { message: err.message },
            }],
        ),
        EffectResultMsg::ThreadPersisted { real_index, result } => match result {
            Ok(()) => (vec![], vec![Dirty::ThreadRow(real_index)]),
            Err(err) => (
                vec![],
                vec![Dirty::Error {
                    thread_id: model
                        .threads
                        .get(real_index)
                        .and_then(|t| t.session_id.clone())
                        .unwrap_or_default(),
                    detail: ErrorDetail { message: err.message },
                }],
            ),
        },
        EffectResultMsg::SessionAttached { real_index, result } => {
            // Stale-target no-op contract (00-plan.md's "Effect-result
            // contracts"): the thread this result targets may have been
            // closed/removed before the attach completed.
            let Some(thread) = model.threads.get_mut(real_index) else {
                return (vec![], vec![]);
            };
            match result {
                Ok(session_id) => {
                    thread.session_id = Some(session_id);
                    (vec![], vec![Dirty::ThreadRow(real_index)])
                }
                Err(err) => (
                    vec![],
                    vec![Dirty::Error {
                        thread_id: thread.session_id.clone().unwrap_or_default(),
                        detail: ErrorDetail { message: err.message },
                    }],
                ),
            }
        }
        EffectResultMsg::PromptStreamDelta {
            thread_id,
            message_id,
            delta,
        } => {
            // Stale-target no-op: no thread in Model currently carries
            // this thread_id (closed mid-stream) -- no-op, not a panic.
            if !model
                .threads
                .iter()
                .any(|t| t.session_id.as_deref() == Some(thread_id.as_str()))
            {
                return (vec![], vec![]);
            }
            (
                vec![],
                vec![Dirty::MessageStreamingDelta {
                    thread_id,
                    message_id,
                    delta,
                }],
            )
        }
        EffectResultMsg::PromptSent { real_index, result } => {
            let Some(thread) = model.threads.get_mut(real_index) else {
                return (vec![], vec![]);
            };
            match result {
                Ok(()) => {
                    thread.state = ThreadState::Idle;
                    (
                        vec![],
                        vec![
                            Dirty::MessagesDiff {
                                thread_id: thread.session_id.clone().unwrap_or_default(),
                                ops: vec![],
                            },
                            Dirty::Connection {
                                thread_id: thread.session_id.clone().unwrap_or_default(),
                            },
                        ],
                    )
                }
                Err(err) => {
                    thread.state = ThreadState::Error;
                    thread.error = Some(err.message.clone());
                    (
                        vec![],
                        vec![Dirty::Error {
                            thread_id: thread.session_id.clone().unwrap_or_default(),
                            detail: ErrorDetail { message: err.message },
                        }],
                    )
                }
            }
        }
        EffectResultMsg::SettingsSaved(Ok(())) => (vec![], vec![Dirty::Settings]),
        EffectResultMsg::SettingsSaved(Err(err)) => (
            vec![],
            vec![Dirty::Error {
                thread_id: String::new(),
                detail: ErrorDetail { message: err.message },
            }],
        ),
        EffectResultMsg::GatewayCallCompleted { real_index, result } => match result {
            Ok(()) => (vec![], vec![Dirty::Capabilities {
                thread_id: model
                    .threads
                    .get(real_index)
                    .and_then(|t| t.session_id.clone())
                    .unwrap_or_default(),
            }]),
            Err(err) => (
                vec![],
                vec![Dirty::Error {
                    thread_id: model
                        .threads
                        .get(real_index)
                        .and_then(|t| t.session_id.clone())
                        .unwrap_or_default(),
                    detail: ErrorDetail { message: err.message },
                }],
            ),
        },
    }
}

fn update_frame(_model: &mut Model, _frame: crate::msg::FrameInput) -> (Vec<Effect>, Vec<Dirty>) {
    // Phase 4b migrates the real panel_rust_poll body here. Until then
    // this is an intentional no-op stub -- see 00-plan.md's "poll tick is
    // a 4th Msg source, not an exception" for why this arm exists at all
    // rather than being deferred to a later Msg variant addition.
    (vec![], vec![])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg::FrameInput;

    fn model_with_threads(names: &[&str]) -> Model {
        let threads = names
            .iter()
            .map(|name| ThreadModel {
                display_name: (*name).to_owned(),
                ..ThreadModel::default()
            })
            .collect();
        Model {
            threads,
            ..Model::default()
        }
    }

    #[test]
    fn thread_navigate_delta_advances_by_one() {
        let mut model = model_with_threads(&["a", "b", "c"]);
        model.selected_thread = 0;
        let (_, dirty) = update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::NavigateDelta(1))));
        assert_eq!(model.selected_thread, 1);
        assert_eq!(dirty, vec![Dirty::Scalar(ScalarField::SelectedThread)]);
    }

    #[test]
    fn thread_navigate_delta_wraps_past_the_end() {
        let mut model = model_with_threads(&["a", "b", "c"]);
        model.selected_thread = 2;
        update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::NavigateDelta(1))));
        assert_eq!(model.selected_thread, 0);
    }

    #[test]
    fn thread_navigate_delta_on_empty_list_does_not_panic() {
        let mut model = Model::default();
        let (effects, dirty) =
            update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::NavigateDelta(1))));
        assert_eq!(model.selected_thread, 0);
        assert!(effects.is_empty());
        assert!(dirty.is_empty());
    }

    #[test]
    fn thread_selected_out_of_range_clamps_to_the_last_thread() {
        // Matches the real select_visible_thread's own
        // `filtered_idx.min(visible_len - 1)` clamping -- not a no-op.
        let mut model = model_with_threads(&["a", "b"]);
        let (effects, dirty) =
            update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::Selected(5))));
        assert_eq!(model.selected_thread, 1);
        assert!(effects.is_empty());
        assert_eq!(dirty, vec![Dirty::Scalar(ScalarField::SelectedThread)]);
    }

    #[test]
    fn thread_selected_on_empty_list_is_a_no_op() {
        let mut model = Model::default();
        let (effects, dirty) =
            update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::Selected(0))));
        assert_eq!(model.selected_thread, 0);
        assert!(effects.is_empty());
        assert!(dirty.is_empty());
    }

    #[test]
    fn thread_delete_requested_removes_the_row_and_clamps_selection() {
        let mut model = model_with_threads(&["a", "b"]);
        model.selected_thread = 1;
        let (effects, dirty) =
            update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::DeleteRequested(1))));
        assert_eq!(model.threads.len(), 1);
        assert_eq!(model.selected_thread, 0);
        assert_eq!(effects, vec![Effect::DeleteThread { real_index: 1 }]);
        assert_eq!(
            dirty,
            vec![Dirty::ThreadListDiff(vec![RowOp::Remove { at: 1 }])]
        );
    }

    #[test]
    fn compose_send_requested_sets_loading_and_returns_send_prompt_effect() {
        let mut model = model_with_threads(&["a"]);
        let (effects, _) = update(
            &mut model,
            Msg::Ui(UiMsg::Compose(ComposeMsg::SendRequested("hi".to_owned()))),
        );
        assert_eq!(model.threads[0].state, ThreadState::Loading);
        assert_eq!(
            effects,
            vec![Effect::SendPrompt {
                real_index: 0,
                text: "hi".to_owned()
            }]
        );
    }

    #[test]
    fn prompt_stream_delta_for_a_thread_that_no_longer_exists_is_a_no_op() {
        let mut model = model_with_threads(&["a"]);
        let (effects, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::PromptStreamDelta {
                thread_id: "nonexistent-session".to_owned(),
                message_id: "m1".to_owned(),
                delta: "tok".to_owned(),
            }),
        );
        assert!(effects.is_empty());
        assert!(dirty.is_empty());
    }

    #[test]
    fn prompt_sent_error_sets_thread_error_state_not_silently_dropped() {
        let mut model = model_with_threads(&["a"]);
        let (_, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::PromptSent {
                real_index: 0,
                result: Err(crate::effect::EffectError::new("boom")),
            }),
        );
        assert_eq!(model.threads[0].state, ThreadState::Error);
        assert_eq!(model.threads[0].error.as_deref(), Some("boom"));
        assert!(matches!(dirty[0], Dirty::Error { .. }));
    }

    #[test]
    fn init_host_msg_requests_load_initial_state_effect() {
        let mut model = Model::default();
        let (effects, _) = update(&mut model, Msg::Host(HostMsg::Init));
        assert_eq!(effects, vec![Effect::LoadInitialState]);
    }

    #[test]
    fn initial_state_loaded_replaces_model_wholesale_on_cold_start() {
        let mut model = model_with_threads(&["stale"]);
        let (_, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::InitialStateLoaded(Ok(
                crate::model::InitialState {
                    threads: vec![crate::agent_bridge::ThreadSpec {
                        display_name: "fresh".to_owned(),
                        provider: "codex".to_owned(),
                        session_id: None,
                        profile_name: None,
                    }],
                    selected_thread_id: None,
                },
            ))),
        );
        assert_eq!(model.threads.len(), 1);
        assert_eq!(model.threads[0].display_name, "fresh");
        assert!(!dirty.is_empty());
    }

    #[test]
    fn frame_tick_with_no_real_change_is_a_no_op_stub_for_now() {
        let mut model = Model::default();
        let (effects, dirty) = update(&mut model, Msg::Frame(FrameInput::default()));
        assert!(effects.is_empty());
        assert!(dirty.is_empty());
    }
}
