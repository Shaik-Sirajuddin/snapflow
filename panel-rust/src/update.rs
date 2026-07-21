//! `tea-slint-model` Phase 2: `update(&mut Model, Msg) -> (Vec<Effect>,
//! Vec<Dirty>)` -- the **sole** owner of state transitions. See
//! `memory/rui/gen/plans/tea-slint-model/00-plan.md`.
//!
//! **Status: live through dispatchers.** Slint callbacks, selected FFI entry
//! points, cold-start hydration, and the frame tick call this reducer.
//! Returned effects are still delegated synchronously to proven bridge
//! methods while the standalone effect executor is completed.
//!
//! The top-level `match` below is intentionally exhaustive with **no
//! wildcard arm** -- see 00-plan.md's "Exhaustiveness requirement": a
//! future `Msg` variant added without a matching arm here must fail to
//! compile, not silently no-op.

use crate::dirty::{Dirty, ErrorDetail, ScalarField};
use crate::effect::{Effect, EffectResultMsg};
use crate::model::{Model, ThreadModel};
use crate::models::ThreadState;
use crate::msg::{
    ChromeMsg, ComposeMsg, HostMsg, Msg, RequestMsg, SettingsMsg, SkillMsg, TerminalMsg, ThreadMsg,
    UiMsg,
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

fn visible_thread_indices(model: &Model) -> Vec<usize> {
    let query = model.search_query.trim().to_lowercase();
    model
        .threads
        .iter()
        .enumerate()
        .filter(|(_, thread)| {
            query.is_empty() || thread.display_name.to_lowercase().contains(&query)
        })
        .map(|(idx, _)| idx)
        .collect()
}

fn current_visible_indices(model: &Model) -> Vec<usize> {
    if model.visible_indices.is_empty() && !model.threads.is_empty() {
        (0..model.threads.len()).collect()
    } else {
        model.visible_indices.clone()
    }
}

fn current_visible_keys(model: &Model) -> Vec<String> {
    current_visible_indices(model)
        .iter()
        .filter_map(|idx| {
            model
                .threads
                .get(*idx)
                .map(|thread| thread.thread_id.clone())
        })
        .collect()
}

fn selected_real_index(model: &Model) -> usize {
    current_visible_indices(model)
        .get(model.selected_thread)
        .copied()
        .unwrap_or(model.selected_thread)
}

fn visible_thread_row(model: &Model, real_index: usize) -> crate::models::VisibleThreadItem {
    let thread = model
        .threads
        .get(real_index)
        .expect("visible thread index must resolve in Model");
    crate::models::VisibleThreadItem {
        real_index,
        thread_id: thread.thread_id.clone(),
        item: crate::ThreadItem {
            name: thread.display_name.clone().into(),
            status: thread.state.as_str().into(),
            busy: matches!(thread.state, ThreadState::Loading),
            open: true,
            closed: thread.closed,
            ..crate::ThreadItem::default()
        },
    }
}

fn thread_list_dirty_with_keys(model: &mut Model, old_keys: Vec<String>) -> Dirty {
    let new_indices = visible_thread_indices(model);
    let new_keys: Vec<String> = new_indices
        .iter()
        .filter_map(|idx| {
            model
                .threads
                .get(*idx)
                .map(|thread| thread.thread_id.clone())
        })
        .collect();
    model.visible_indices = new_indices.clone();
    let rows = new_indices
        .iter()
        .map(|idx| visible_thread_row(model, *idx))
        .collect::<Vec<_>>();
    model.thread_rows = rows.clone();
    Dirty::ThreadListDiff(crate::dirty::diff_by_id(&old_keys, &new_keys, &rows))
}

fn update_thread(model: &mut Model, msg: ThreadMsg) -> (Vec<Effect>, Vec<Dirty>) {
    match msg {
        ThreadMsg::New => {
            let old_keys = current_visible_keys(model);
            model.compose_text.clear();
            model.search_query.clear();
            let real_index = model.threads.len();
            let thread_id = format!("thread:{real_index}");
            let display_name = format!("New thread {}", real_index + 1);
            let provider = match model.default_agent_id.as_str() {
                "claude" | "claude-code" => "claude",
                _ => "codex",
            }
            .to_owned();
            let profile_name =
                (!model.default_profile.is_empty()).then(|| model.default_profile.clone());
            let permission_profile =
                (!model.permission_profile.is_empty()).then(|| model.permission_profile.clone());
            model.threads.push(ThreadModel {
                thread_id: thread_id.clone(),
                display_name: display_name.clone(),
                provider: provider.clone(),
                profile_name: profile_name.clone(),
                permission_profile: permission_profile.clone(),
                ..ThreadModel::default()
            });
            let list_dirty = thread_list_dirty_with_keys(model, old_keys);
            (
                vec![Effect::NewThread {
                    real_index,
                    display_name,
                    provider,
                    profile_name,
                    permission_profile,
                }],
                vec![
                    list_dirty,
                    Dirty::Scalar(ScalarField::ComposeText),
                    Dirty::Scalar(ScalarField::SearchQuery),
                ],
            )
        }
        ThreadMsg::NewResolved {
            display_name,
            provider,
            profile_name,
            permission_profile,
            session_id,
            thread_id,
        } => {
            let old_keys = current_visible_keys(model);
            model.compose_text.clear();
            model.search_query.clear();
            let real_index = model.threads.len();
            model.threads.push(ThreadModel {
                thread_id: thread_id
                    .or_else(|| session_id.clone())
                    .unwrap_or_else(|| format!("thread:{real_index}")),
                display_name,
                provider,
                profile_name,
                permission_profile,
                session_id,
                ..ThreadModel::default()
            });
            let list_dirty = thread_list_dirty_with_keys(model, old_keys);
            (
                vec![],
                vec![
                    list_dirty,
                    Dirty::Scalar(ScalarField::ComposeText),
                    Dirty::Scalar(ScalarField::SearchQuery),
                ],
            )
        }
        ThreadMsg::Selected(idx) => {
            // Clamp, don't no-op, to match the real
            // `select_visible_thread`'s own `filtered_idx.min(visible_len
            // - 1)` -- an out-of-range idx still selects the last thread
            // rather than being silently ignored.
            let visible_len = if model.visible_indices.is_empty() {
                model.threads.len()
            } else {
                model.visible_indices.len()
            };
            if visible_len == 0 {
                return (vec![], vec![]);
            }
            model.selected_thread = idx.min(visible_len - 1);
            (vec![], vec![Dirty::Scalar(ScalarField::SelectedThread)])
        }
        ThreadMsg::NavigateDelta(delta) => {
            let visible_len = if model.visible_indices.is_empty() {
                model.threads.len()
            } else {
                model.visible_indices.len()
            };
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
            if matches!(thread.state, ThreadState::Loading | ThreadState::Cancelling) {
                thread.state = ThreadState::Idle;
            }
            (
                vec![Effect::PersistThread { real_index: idx }],
                vec![Dirty::ThreadRow(idx)],
            )
        }
        ThreadMsg::DeleteRequested(idx) => {
            let old_keys = current_visible_keys(model);
            let Some(thread) = model.threads.get_mut(idx) else {
                return (vec![], vec![]);
            };
            // AgentBridge keeps deleted slots in place and marks them
            // closed, so removing this Model row would shift every later
            // real index away from its bridge slot.
            thread.closed = true;
            thread.state = ThreadState::Idle;
            let list_dirty = thread_list_dirty_with_keys(model, old_keys);
            (
                vec![Effect::DeleteThread { real_index: idx }],
                vec![list_dirty],
            )
        }
        ThreadMsg::RenameRequested(idx, name) => {
            let old_keys = current_visible_keys(model);
            let Some(thread) = model.threads.get_mut(idx) else {
                return (vec![], vec![]);
            };
            thread.display_name = name.clone();
            let list_dirty = thread_list_dirty_with_keys(model, old_keys);
            (
                vec![Effect::RenameThread {
                    real_index: idx,
                    name,
                }],
                vec![list_dirty],
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
            thread_id,
        } => {
            let old_keys = current_visible_keys(model);
            model.search_query.clear();
            model.threads.push(ThreadModel {
                thread_id: thread_id.unwrap_or_else(|| format!("thread:{}", model.threads.len())),
                display_name: title,
                provider: provider.clone(),
                session_id: Some(session_id.clone()),
                ..ThreadModel::default()
            });
            let at = model.threads.len() - 1;
            let list_dirty = thread_list_dirty_with_keys(model, old_keys);
            (
                vec![Effect::RecoverSessionAttach {
                    real_index: at,
                    session_id,
                    provider,
                    title: model.threads[at].display_name.clone(),
                }],
                vec![list_dirty, Dirty::Scalar(ScalarField::SearchQuery)],
            )
        }
    }
}

fn update_compose(model: &mut Model, msg: ComposeMsg) -> (Vec<Effect>, Vec<Dirty>) {
    let idx = selected_real_index(model);
    match msg {
        ComposeMsg::SendRequested(text) => {
            model.compose_text.clear();
            let Some(thread) = model.threads.get_mut(idx) else {
                return (vec![], vec![]);
            };
            let thread_id = thread.thread_id.clone();
            if matches!(thread.state, ThreadState::Loading | ThreadState::Cancelling) {
                return match thread.send_queue.enqueue(text, false) {
                    Ok(_) => (
                        vec![],
                        vec![
                            Dirty::ThreadRow(idx),
                            Dirty::Scalar(ScalarField::ComposeText),
                        ],
                    ),
                    Err(error) => {
                        let message = error.to_string();
                        thread.error = Some(message.clone());
                        thread.state = ThreadState::Error;
                        (
                            vec![],
                            vec![
                                Dirty::Scalar(ScalarField::ComposeText),
                                Dirty::Error {
                                    thread_id,
                                    detail: ErrorDetail { message },
                                },
                            ],
                        )
                    }
                };
            }
            thread.error = None;
            thread.state = ThreadState::Loading;
            (
                vec![Effect::SendPrompt {
                    real_index: idx,
                    text,
                }],
                vec![
                    Dirty::Connection {
                        thread_id,
                    },
                    Dirty::Scalar(ScalarField::ComposeText),
                ],
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
    let idx = selected_real_index(model);
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
        TerminalMsg::LocalKeyInput(bytes) => (vec![Effect::LocalTerminalWrite { bytes }], vec![]),
    }
}

fn update_settings(model: &mut Model, msg: SettingsMsg) -> (Vec<Effect>, Vec<Dirty>) {
    let idx = selected_real_index(model);
    match msg {
        SettingsMsg::Open => {
            model.settings_open = true;
            (
                vec![],
                vec![Dirty::Scalar(ScalarField::SettingsOpen), Dirty::Settings],
            )
        }
        SettingsMsg::Close => {
            model.settings_open = false;
            (vec![], vec![Dirty::Scalar(ScalarField::SettingsOpen)])
        }
        SettingsMsg::Save(input) => {
            model.default_profile = input.default_profile.clone();
            model.permission_profile = input.permission_profile.clone();
            model.background_default = input.background_default;
            model.default_agent_id = input.default_agent_id.clone();
            model.background_override_set = input.background_override_set;
            model.background_override = input.background_override;
            model.settings_open = false;
            (
                vec![Effect::SaveSettings { input }],
                vec![
                    Dirty::Settings,
                    Dirty::Scalar(ScalarField::SettingsOpen),
                ],
            )
        }
        SettingsMsg::ScopeChanged(scope) => {
            model.settings_scope = scope;
            (
                vec![],
                vec![Dirty::Scalar(ScalarField::SettingsScope), Dirty::Settings],
            )
        }
        SettingsMsg::ConfigOptionSelected { key, value } => (
            vec![Effect::SetConfigOption {
                real_index: idx,
                key,
                value,
            }],
            vec![Dirty::Settings],
        ),
        SettingsMsg::ModeSelected(mode) => (
            vec![Effect::SetMode {
                real_index: idx,
                mode,
            }],
            vec![Dirty::Settings],
        ),
        SettingsMsg::DevModeToggled(enabled) => {
            model.dev_mode = enabled;
            (vec![Effect::SaveDevMode { enabled }], vec![Dirty::Settings])
        }
        SettingsMsg::McpServerCreate { name, command } => (
            vec![Effect::McpServerCreate {
                real_index: idx,
                name,
                command,
            }],
            vec![Dirty::Settings],
        ),
        SettingsMsg::McpServerDelete { name } => (
            vec![Effect::McpServerDelete {
                real_index: idx,
                name,
            }],
            vec![Dirty::Settings],
        ),
        SettingsMsg::McpServerEnabledChanged { name, enabled } => (
            vec![Effect::McpServerEnabledChanged {
                real_index: idx,
                name,
                enabled,
            }],
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
            vec![Effect::ProfileDelete {
                real_index: idx,
                name,
            }],
            vec![Dirty::Settings],
        ),
        SettingsMsg::AgentInstallRequested { agent_id } => (
            vec![Effect::AgentInstallRequested {
                real_index: idx,
                agent_id,
            }],
            vec![Dirty::Settings],
        ),
    }
}

fn update_skill(_model: &mut Model, msg: SkillMsg) -> (Vec<Effect>, Vec<Dirty>) {
    match msg {
        SkillMsg::NewSkillRequested { .. } => (vec![], vec![Dirty::SkillsListDiff(vec![])]),
        SkillMsg::ContentEdited { path, content } => (
            vec![Effect::SkillWrite { path, content }],
            vec![Dirty::SkillsListDiff(vec![])],
        ),
        SkillMsg::CopyPathRequested { .. } => (vec![], vec![]),
        SkillMsg::EditorOpenRequested { .. }
        | SkillMsg::OpenInEditorRequested { .. }
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
            let old_keys = current_visible_keys(model);
            model.search_query = query;
            let list_dirty = thread_list_dirty_with_keys(model, old_keys);
            model.selected_thread = model
                .selected_thread
                .min(model.visible_indices.len().saturating_sub(1));
            (
                vec![],
                vec![
                    Dirty::Scalar(ScalarField::SearchQuery),
                    list_dirty,
                    Dirty::Scalar(ScalarField::SelectedThread),
                ],
            )
        }
        ChromeMsg::SearchSubmitted { query, .. } => {
            let old_keys = current_visible_keys(model);
            model.search_query = query;
            let list_dirty = thread_list_dirty_with_keys(model, old_keys);
            model.selected_thread = model
                .selected_thread
                .min(model.visible_indices.len().saturating_sub(1));
            (
                vec![],
                vec![
                    Dirty::Scalar(ScalarField::SearchQuery),
                    list_dirty,
                    Dirty::Scalar(ScalarField::SelectedThread),
                ],
            )
        }
        ChromeMsg::ToggleExpanded(index) => {
            let Some(real_idx) = model.displayed_thread else {
                return (vec![], vec![]);
            };
            let Some(slot) = model.expanded.get_mut(index) else {
                return (vec![], vec![]);
            };
            *slot = !*slot;
            let Some(thread) = model.threads.get_mut(real_idx) else {
                return (vec![], vec![]);
            };
            let old_keys = thread.transcript_keys.clone();
            let rows = crate::models::to_message_rows_from_transcript(
                thread.transcript.clone(),
                &model.expanded,
            );
            thread.message_rows = rows.clone();
            (
                vec![],
                vec![Dirty::MessagesDiff {
                    thread_id: thread.thread_id.clone(),
                    ops: crate::dirty::diff_by_id(&old_keys, &thread.transcript_keys, &rows),
                }],
            )
        }
        ChromeMsg::ErrorBannerDismissed => {
            let real_idx = selected_real_index(model);
            let Some(thread) = model.threads.get_mut(real_idx) else {
                return (vec![], vec![]);
            };
            let thread_id = thread.thread_id.clone();
            thread.error = None;
            (
                vec![],
                vec![
                    Dirty::ThreadRow(real_idx),
                    Dirty::Error {
                        thread_id,
                        detail: ErrorDetail {
                            message: String::new(),
                        },
                    },
                ],
            )
        }
    }
}

fn update_host(model: &mut Model, msg: HostMsg) -> (Vec<Effect>, Vec<Dirty>) {
    match msg {
        HostMsg::InvokeCommand(command) => match command.as_str() {
            "previous-thread" => update_thread(model, ThreadMsg::NavigateDelta(-1)),
            "next-thread" => update_thread(model, ThreadMsg::NavigateDelta(1)),
            // Opening search is presentation-only; the dispatcher invokes
            // the generated Slint function after this reducer pass.
            _ => (vec![], vec![]),
        },
        HostMsg::InputKey { .. } => (vec![], vec![]),
        HostMsg::AppearanceChanged(state) => {
            let theme_variant = state
                .current()
                .map(|appearance| match appearance.color_scheme {
                    crate::appearance::ColorScheme::Dark => "dark",
                    crate::appearance::ColorScheme::Light => "light",
                })
                .unwrap_or("dark");
            model.appearance = state;
            model.theme_variant = theme_variant.to_owned();
            (vec![], vec![Dirty::Appearance])
        }
        HostMsg::ThemeChanged(theme) => {
            model.theme_variant = if theme.eq_ignore_ascii_case("light") {
                "light".to_owned()
            } else {
                "dark".to_owned()
            };
            (vec![], vec![Dirty::Theme])
        }
        HostMsg::ProjectPathChanged(path) => {
            model.active_project_path = path.clone();
            (
                vec![Effect::SetActiveProjectPath { path }],
                vec![Dirty::ProjectPath, Dirty::SkillsListDiff(vec![])],
            )
        }
        HostMsg::Init => (vec![Effect::LoadInitialState], vec![]),
    }
}

fn update_effect(model: &mut Model, msg: EffectResultMsg) -> (Vec<Effect>, Vec<Dirty>) {
    match msg {
        EffectResultMsg::InitialStateLoaded(Ok(initial)) => {
            // Replacing application state on cold start must not replace the
            // persistent Slint models. Their identity belongs to the panel
            // lifetime, not to one hydration result.
            let thread_model = model.thread_model.clone();
            let messages_model = model.messages_model.clone();
            let skills_model = model.skills_model.clone();
            let profiles_model = model.profiles_model.clone();
            let mcp_servers_model = model.mcp_servers_model.clone();
            let agent_catalog_model = model.agent_catalog_model.clone();
            let recoverable_sessions_model = model.recoverable_sessions_model.clone();
            let thread_keys = model.thread_model_keys.borrow().clone();
            let message_keys = model.message_model_keys.borrow().clone();
            let skill_keys = model.skill_model_keys.borrow().clone();
            let profile_keys = model.profile_model_keys.borrow().clone();
            let mcp_server_keys = model.mcp_server_model_keys.borrow().clone();
            let agent_catalog_keys = model.agent_catalog_model_keys.borrow().clone();
            let recoverable_session_keys = model.recoverable_session_model_keys.borrow().clone();
            *model = Model::from_initial_state(initial);
            model.thread_model = thread_model;
            model.messages_model = messages_model;
            model.skills_model = skills_model;
            model.profiles_model = profiles_model;
            model.mcp_servers_model = mcp_servers_model;
            model.agent_catalog_model = agent_catalog_model;
            model.recoverable_sessions_model = recoverable_sessions_model;
            *model.thread_model_keys.borrow_mut() = thread_keys.clone();
            *model.message_model_keys.borrow_mut() = message_keys;
            *model.skill_model_keys.borrow_mut() = skill_keys;
            *model.profile_model_keys.borrow_mut() = profile_keys;
            *model.mcp_server_model_keys.borrow_mut() = mcp_server_keys;
            *model.agent_catalog_model_keys.borrow_mut() = agent_catalog_keys;
            *model.recoverable_session_model_keys.borrow_mut() = recoverable_session_keys;
            let thread_list_dirty = thread_list_dirty_with_keys(model, thread_keys);
            // Cold start: everything is dirty, there is no prior row
            // identity to preserve (see 00-plan.md's known-gap section).
            (
                vec![],
                vec![
                    thread_list_dirty,
                    Dirty::Scalar(ScalarField::SelectedThread),
                ],
            )
        }
        EffectResultMsg::InitialStateLoaded(Err(err)) => (
            vec![],
            vec![Dirty::Error {
                thread_id: String::new(),
                detail: ErrorDetail {
                    message: err.message,
                },
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
                    detail: ErrorDetail {
                        message: err.message,
                    },
                }],
            ),
        },
        EffectResultMsg::ThreadRecordPersisted(result) => match result {
            Ok(()) => (vec![], vec![]),
            Err(err) => (
                vec![],
                vec![Dirty::Error {
                    thread_id: String::new(),
                    detail: ErrorDetail {
                        message: err.message,
                    },
                }],
            ),
        },
        EffectResultMsg::SessionAttached {
            real_index,
            thread_id,
            provider,
            result,
        } => {
            // Stale-target no-op contract (00-plan.md's "Effect-result
            // contracts"): the thread this result targets may have been
            // closed/removed before the attach completed.
            let Some(thread) = model.threads.get_mut(real_index) else {
                return (vec![], vec![]);
            };
            match result {
                Ok(session_id) => {
                    thread.session_id = Some(session_id);
                    if let Some(thread_id) = thread_id {
                        thread.thread_id = thread_id;
                    }
                    if let Some(provider) = provider {
                        thread.provider = provider;
                    }
                    (
                        vec![Effect::PersistThread { real_index }],
                        vec![Dirty::ThreadRow(real_index)],
                    )
                }
                Err(err) => (
                    vec![],
                    vec![Dirty::Error {
                        thread_id: thread.thread_id.clone(),
                        detail: ErrorDetail {
                            message: err.message,
                        },
                    }],
                ),
            }
        }
        EffectResultMsg::SkillWritten(Ok(())) | EffectResultMsg::SkillPromoted(Ok(())) => {
            (vec![], vec![])
        }
        EffectResultMsg::SkillWritten(Err(err)) | EffectResultMsg::SkillPromoted(Err(err)) => (
            vec![],
            vec![Dirty::Error {
                thread_id: String::new(),
                detail: ErrorDetail {
                    message: err.message,
                },
            }],
        ),
        EffectResultMsg::PromptStreamDelta {
            thread_id,
            message_id,
            delta,
        } => {
            // Stale-target no-op: either the thread was closed/deleted or
            // the message row was removed while the stream was in flight.
            // Resolve both identities before producing a Dirty marker.
            let thread_exists = model
                .threads
                .iter()
                .any(|thread| Model::thread_matches_id(thread, &thread_id));
            if !thread_exists {
                return (vec![], vec![]);
            }
            let Some(thread) = model
                .threads
                .iter_mut()
                .find(|thread| Model::thread_matches_id(thread, &thread_id))
            else {
                return (vec![], vec![]);
            };
            if !thread.message_ids.iter().any(|id| id == &message_id) {
                return (vec![], vec![]);
            }
            let candidates = [
                format!("assistant:{message_id}"),
                format!("thought:{message_id}"),
                format!("user:{message_id}"),
                format!("tool:{message_id}"),
            ];
            if let Some(index) = thread
                .transcript_keys
                .iter()
                .position(|key| candidates.iter().any(|candidate| candidate == key))
            {
                if let Some(row) = thread.message_rows.get_mut(index) {
                    row.text = format!("{}{}", row.text, delta).into();
                }
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
                                thread_id: thread.thread_id.clone(),
                                ops: vec![],
                            },
                            Dirty::Connection {
                                thread_id: thread.thread_id.clone(),
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
                            thread_id: thread.thread_id.clone(),
                            detail: ErrorDetail {
                                message: err.message,
                            },
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
                detail: ErrorDetail {
                    message: err.message,
                },
            }],
        ),
        EffectResultMsg::GatewayCallCompleted { real_index, result } => match result {
            Ok(()) => (
                vec![],
                vec![Dirty::Capabilities {
                    thread_id: model
                        .threads
                        .get(real_index)
                        .and_then(|t| t.session_id.clone())
                        .unwrap_or_default(),
                }],
            ),
            Err(err) => (
                vec![],
                vec![Dirty::Error {
                    thread_id: model
                        .threads
                        .get(real_index)
                        .and_then(|t| t.session_id.clone())
                        .unwrap_or_default(),
                    detail: ErrorDetail {
                        message: err.message,
                    },
                }],
            ),
        },
    }
}

fn update_frame(model: &mut Model, frame: crate::msg::FrameInput) -> (Vec<Effect>, Vec<Dirty>) {
    let mut effects = Vec::new();
    let mut dirty = Vec::new();
    for bridge_event in &frame.bridge_events {
        let Some(thread) = model.threads.get_mut(bridge_event.thread_index) else {
            continue;
        };
        match &bridge_event.event {
            crate::protocol_types::AgentEvent::Message(message) => {
                if let Some(message_id) = message.id.as_ref() {
                    if !thread.message_ids.iter().any(|id| id == message_id) {
                        thread.message_ids.push(message_id.clone());
                    }
                }
                dirty.push(Dirty::MessageAppended {
                    thread_id: thread.thread_id.clone(),
                });
            }
            crate::protocol_types::AgentEvent::TurnEnded(_) => {
                thread.state = ThreadState::Idle;
                thread.error = None;
                if let Some(entry) = thread
                    .send_queue
                    .on_generation_stopped(false)
                    .ok()
                    .flatten()
                {
                    thread.state = ThreadState::Loading;
                    effects.push(Effect::SendPrompt {
                        real_index: bridge_event.thread_index,
                        text: entry.text,
                    });
                }
                dirty.push(Dirty::ThreadRow(bridge_event.thread_index));
            }
            crate::protocol_types::AgentEvent::Error(error) => {
                thread.state = ThreadState::Error;
                thread.error = Some(error.clone());
                dirty.push(Dirty::Error {
                    thread_id: thread.thread_id.clone(),
                    detail: ErrorDetail {
                        message: error.clone(),
                    },
                });
            }
            crate::protocol_types::AgentEvent::PermissionRequest(_)
            | crate::protocol_types::AgentEvent::TerminalOutput(_)
            | crate::protocol_types::AgentEvent::SessionModes(_)
            | crate::protocol_types::AgentEvent::CurrentModeChanged(_)
            | crate::protocol_types::AgentEvent::ConfigOptions(_) => {
                dirty.push(Dirty::ThreadRow(bridge_event.thread_index));
            }
        }
    }
    if frame.bridge_events_pending {
        dirty.push(Dirty::MessagesDiff {
            thread_id: String::new(),
            ops: Vec::new(),
        });
        dirty.push(Dirty::Connection {
            thread_id: String::new(),
        });
    }
    for record in frame.thread_record_snapshots {
        if model
            .traced_attachment_threads
            .insert(record.thread_id.clone())
        {
            effects.push(Effect::PersistThreadRecord { record });
        }
    }
    if frame.settings_reload_pending {
        dirty.push(Dirty::Settings);
    }
    if frame.local_terminal_snapshot.is_some() {
        dirty.push(Dirty::LocalTerminal);
    }
    if frame.prepend_expanded_rows > 0 {
        let mut expanded = vec![false; frame.prepend_expanded_rows];
        expanded.append(&mut model.expanded);
        model.expanded = expanded;
    }
    if frame.clear_selected_thread {
        let old_keys = model.message_model_keys.borrow().clone();
        if model.displayed_thread.take().is_some() || !old_keys.is_empty() {
            dirty.push(Dirty::MessagesDiff {
                thread_id: String::new(),
                ops: crate::dirty::diff_by_id(
                    &old_keys,
                    &[],
                    &Vec::<crate::MessageItem>::new(),
                ),
            });
        }
    }
    if let Some(snapshot) = frame.thread_list_snapshot {
        let old_keys = model.thread_model_keys.borrow().clone();
        let changed = old_keys != snapshot.visible_thread_ids || model.thread_rows != snapshot.rows;
        for row in &snapshot.rows {
            if let Some(thread) = model.threads.get_mut(row.real_index) {
                thread.thread_id = row.thread_id.clone();
            }
        }
        if changed {
            model.visible_indices = snapshot.visible_indices.clone();
            model.thread_rows = snapshot.rows.clone();
            dirty.push(Dirty::ThreadListDiff(crate::dirty::diff_by_id(
                &old_keys,
                &snapshot.visible_thread_ids,
                &snapshot.rows,
            )));
        }
    }
    if let Some(snapshot) = frame.settings_gateway_snapshot {
        let changed = model.available_profiles != snapshot.profiles
            || model.available_mcp_servers != snapshot.mcp_servers
            || model.agent_catalog != snapshot.agents
            || model.recoverable_sessions != snapshot.recoverable_sessions
            || model.recovery_provider != snapshot.recovery_provider;
        if changed {
            model.available_profiles = snapshot.profiles;
            model.available_mcp_servers = snapshot.mcp_servers;
            model.agent_catalog = snapshot.agents;
            model.recoverable_sessions = snapshot.recoverable_sessions;
            model.recovery_provider = snapshot.recovery_provider;
            dirty.push(Dirty::Settings);
        }
    }
    if let Some(snapshot) = frame.settings_preferences_snapshot {
        let changed = model.settings_scope != snapshot.scope
            || model.default_profile != snapshot.default_profile
            || model.permission_profile != snapshot.permission_profile
            || model.background_default != snapshot.background_default
            || model.default_agent_id != snapshot.default_agent_id
            || model.dev_mode != snapshot.dev_mode
            || model.background_override_set != snapshot.background_override_set
            || model.background_override != snapshot.background_override;
        if changed {
            model.settings_scope = snapshot.scope;
            model.default_profile = snapshot.default_profile;
            model.permission_profile = snapshot.permission_profile;
            model.background_default = snapshot.background_default;
            model.default_agent_id = snapshot.default_agent_id;
            model.dev_mode = snapshot.dev_mode;
            model.background_override_set = snapshot.background_override_set;
            model.background_override = snapshot.background_override;
            dirty.push(Dirty::Settings);
        }
    }
    if let Some(skills) = frame.skills_snapshot {
        if model.skills != skills {
            let old_keys: Vec<std::path::PathBuf> = model
                .skills
                .iter()
                .map(|skill| skill.path.clone())
                .collect();
            let rows = crate::models::to_skill_option_rows(skills.clone());
            let new_keys: Vec<std::path::PathBuf> =
                skills.iter().map(|skill| skill.path.clone()).collect();
            model.skills = skills;
            dirty.push(Dirty::SkillsListDiff(crate::dirty::diff_by_id(
                &old_keys, &new_keys, &rows,
            )));
        }
    }
    if let Some(snapshot) = frame.selected_thread_snapshot {
        // The bridge index is only a collection-time location. Resolve the
        // snapshot by durable identity first so a concurrent list diff cannot
        // hydrate the wrong thread after indices shift.
        let target_index = if snapshot.thread_id.is_empty() {
            Some(snapshot.real_index)
        } else {
            model
                .threads
                .iter()
                .position(|thread| Model::thread_matches_id(thread, &snapshot.thread_id))
        };
        let Some(target_index) = target_index else {
            return (effects, dirty);
        };
        let switched_thread = model.displayed_thread != Some(target_index);
        if switched_thread {
            model.expanded.clear();
            model.displayed_thread = Some(target_index);
        }
        let transcript_row_count =
            crate::models::to_message_rows_from_transcript(snapshot.transcript.clone(), &[])
                .len();
        if model.expanded.len() < transcript_row_count {
            model.expanded.resize(transcript_row_count, false);
        }
        let expanded = model.expanded.clone();
        if let Some(thread) = model.threads.get_mut(target_index) {
            let thread_id = thread.thread_id.clone();
            let old_keys = thread.transcript_keys.clone();
            let rows = crate::models::to_message_rows_from_transcript(
                snapshot.transcript.clone(),
                &expanded,
            );
            let new_keys = crate::models::transcript_row_keys(&snapshot.transcript);
            let transcript_changed = old_keys != new_keys
                || thread.message_rows != rows
                || thread.transcript != snapshot.transcript;
            let pending_changed = thread.pending_request != snapshot.pending_request;
            let terminals_changed = thread.terminals != snapshot.terminals
                || thread.expanded_terminal != snapshot.expanded_terminal;
            let local_terminal_changed = thread.local_terminal != snapshot.local_terminal;
            let connection_changed = thread.connection_status != snapshot.connection_status;
            let capabilities_changed = thread.session_modes != snapshot.session_modes
                || thread.config_options != snapshot.config_options;

            thread.transcript = snapshot.transcript;
            thread.transcript_keys = new_keys.clone();
            thread.message_ids = new_keys
                .iter()
                .filter_map(|key| key.split_once(':').map(|(_, id)| id.to_owned()))
                .collect();
            thread.message_rows = rows.clone();
            thread.has_older_messages = snapshot.has_older_messages;
            thread.pending_request = snapshot.pending_request;
            thread.terminals = snapshot.terminals;
            thread.expanded_terminal = snapshot.expanded_terminal;
            thread.local_terminal = snapshot.local_terminal;
            thread.connection_status = snapshot.connection_status;
            thread.session_modes = snapshot.session_modes;
            thread.config_options = snapshot.config_options;

            if transcript_changed {
                dirty.push(Dirty::MessagesDiff {
                    thread_id: thread_id.clone(),
                    ops: crate::dirty::diff_by_id(&old_keys, &thread.transcript_keys, &rows),
                });
            }
            if pending_changed {
                dirty.push(Dirty::PendingRequest {
                    thread_id: thread_id.clone(),
                });
            }
            if terminals_changed {
                dirty.push(Dirty::Terminal {
                    id: thread
                        .expanded_terminal
                        .as_ref()
                        .map(|terminal| terminal.terminal_id.to_string())
                        .unwrap_or_default(),
                });
            }
            if local_terminal_changed {
                dirty.push(Dirty::LocalTerminal);
            }
            if connection_changed {
                dirty.push(Dirty::Connection {
                    thread_id: thread_id.clone(),
                });
            }
            if capabilities_changed {
                dirty.push(Dirty::Capabilities { thread_id });
            }
            if switched_thread {
                dirty.push(Dirty::Error {
                    thread_id: thread.thread_id.clone(),
                    detail: ErrorDetail {
                        message: thread.error.clone().unwrap_or_default(),
                    },
                });
            }
        }
    }
    (effects, dirty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dirty::RowOp;
    use crate::msg::FrameInput;

    fn model_with_threads(names: &[&str]) -> Model {
        let threads = names
            .iter()
            .enumerate()
            .map(|(idx, name)| ThreadModel {
                thread_id: format!("thread-{idx}"),
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
        let (_, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Thread(ThreadMsg::NavigateDelta(1))),
        );
        assert_eq!(model.selected_thread, 1);
        assert_eq!(dirty, vec![Dirty::Scalar(ScalarField::SelectedThread)]);
    }

    #[test]
    fn thread_navigate_delta_wraps_past_the_end() {
        let mut model = model_with_threads(&["a", "b", "c"]);
        model.selected_thread = 2;
        update(
            &mut model,
            Msg::Ui(UiMsg::Thread(ThreadMsg::NavigateDelta(1))),
        );
        assert_eq!(model.selected_thread, 0);
    }

    #[test]
    fn thread_navigate_delta_on_empty_list_does_not_panic() {
        let mut model = Model::default();
        let (effects, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Thread(ThreadMsg::NavigateDelta(1))),
        );
        assert_eq!(model.selected_thread, 0);
        assert!(effects.is_empty());
        assert!(dirty.is_empty());
    }

    #[test]
    fn host_previous_thread_command_uses_reducer_navigation() {
        let mut model = model_with_threads(&["a", "b", "c"]);
        model.selected_thread = 1;
        let (_, dirty) = update(
            &mut model,
            Msg::Host(HostMsg::InvokeCommand("previous-thread".to_owned())),
        );
        assert_eq!(model.selected_thread, 0);
        assert_eq!(dirty, vec![Dirty::Scalar(ScalarField::SelectedThread)]);
    }

    #[test]
    fn host_next_thread_command_uses_reducer_navigation() {
        let mut model = model_with_threads(&["a", "b", "c"]);
        model.selected_thread = 1;
        let (_, dirty) = update(
            &mut model,
            Msg::Host(HostMsg::InvokeCommand("next-thread".to_owned())),
        );
        assert_eq!(model.selected_thread, 2);
        assert_eq!(dirty, vec![Dirty::Scalar(ScalarField::SelectedThread)]);
    }

    #[test]
    fn host_project_path_change_emits_one_bridge_effect() {
        let mut model = Model::default();
        let (effects, dirty) = update(
            &mut model,
            Msg::Host(HostMsg::ProjectPathChanged(Some(
                "/tmp/project.mlt".to_owned(),
            ))),
        );
        assert_eq!(model.active_project_path.as_deref(), Some("/tmp/project.mlt"));
        assert_eq!(
            effects,
            vec![Effect::SetActiveProjectPath {
                path: Some("/tmp/project.mlt".to_owned())
            }]
        );
        assert_eq!(
            dirty,
            vec![
                Dirty::ProjectPath,
                Dirty::SkillsListDiff(Vec::new())
            ]
        );
    }

    #[test]
    fn thread_selected_out_of_range_clamps_to_the_last_thread() {
        // Matches the real select_visible_thread's own
        // `filtered_idx.min(visible_len - 1)` clamping -- not a no-op.
        let mut model = model_with_threads(&["a", "b"]);
        let (effects, dirty) = update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::Selected(5))));
        assert_eq!(model.selected_thread, 1);
        assert!(effects.is_empty());
        assert_eq!(dirty, vec![Dirty::Scalar(ScalarField::SelectedThread)]);
    }

    #[test]
    fn thread_selected_on_empty_list_is_a_no_op() {
        let mut model = Model::default();
        let (effects, dirty) = update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::Selected(0))));
        assert_eq!(model.selected_thread, 0);
        assert!(effects.is_empty());
        assert!(dirty.is_empty());
    }

    #[test]
    fn thread_delete_requested_closes_the_row_without_shifting_bridge_indices() {
        let mut model = model_with_threads(&["a", "b"]);
        model.selected_thread = 1;
        let (effects, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Thread(ThreadMsg::DeleteRequested(1))),
        );
        assert_eq!(model.threads.len(), 2);
        assert!(model.threads[1].closed);
        assert_eq!(model.selected_thread, 1);
        assert_eq!(effects, vec![Effect::DeleteThread { real_index: 1 }]);
        assert_eq!(dirty, vec![Dirty::ThreadListDiff(vec![])]);
    }

    #[test]
    fn new_thread_is_pending_until_attach_result_resolves_its_binding() {
        let mut model = model_with_threads(&["existing"]);
        model.default_profile = "safe".to_owned();
        model.permission_profile = "workspace".to_owned();
        model.default_agent_id = "claude".to_owned();

        let (effects, _) = update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::New)));
        assert_eq!(
            effects,
            vec![Effect::NewThread {
                real_index: 1,
                display_name: "New thread 2".to_owned(),
                provider: "claude".to_owned(),
                profile_name: Some("safe".to_owned()),
                permission_profile: Some("workspace".to_owned()),
            }]
        );
        assert_eq!(model.threads.len(), 2);
        assert!(model.threads[1].session_id.is_none());

        let (follow_up, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::SessionAttached {
                real_index: 1,
                thread_id: Some("durable-new".to_owned()),
                provider: Some("claude".to_owned()),
                result: Ok("session-new".to_owned()),
            }),
        );
        assert_eq!(follow_up, vec![Effect::PersistThread { real_index: 1 }]);
        assert_eq!(model.threads[1].thread_id, "durable-new");
        assert_eq!(model.threads[1].session_id.as_deref(), Some("session-new"));
        assert_eq!(dirty, vec![Dirty::ThreadRow(1)]);
    }

    #[test]
    fn closing_a_middle_thread_keeps_durable_ids_and_row_positions() {
        let mut model = model_with_threads(&["a", "b", "c"]);
        let (_, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Thread(ThreadMsg::DeleteRequested(1))),
        );
        assert_eq!(dirty, vec![Dirty::ThreadListDiff(vec![])]);
        assert_eq!(
            model
                .threads
                .iter()
                .map(|thread| thread.thread_id.as_str())
                .collect::<Vec<_>>(),
            vec!["thread-0", "thread-1", "thread-2"]
        );
        assert!(model.threads[1].closed);
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
    fn compose_send_requested_targets_the_real_thread_after_filtering() {
        let mut model = model_with_threads(&["first", "middle", "last"]);
        model.visible_indices = vec![0, 2];
        model.selected_thread = 1;
        let (effects, _) = update(
            &mut model,
            Msg::Ui(UiMsg::Compose(ComposeMsg::SendRequested("hi".to_owned()))),
        );
        assert_eq!(
            effects,
            vec![Effect::SendPrompt {
                real_index: 2,
                text: "hi".to_owned(),
            }]
        );
        assert_eq!(model.threads[2].state, ThreadState::Loading);
        assert_eq!(model.threads[1].state, ThreadState::Idle);
    }

    #[test]
    fn turn_ended_drains_a_queued_message_into_send_prompt_effect() {
        let mut model = model_with_threads(&["a"]);
        model.threads[0].session_id = Some("thread-1".to_owned());
        model.threads[0]
            .send_queue
            .enqueue("queued".to_owned(), false)
            .expect("queue entry");
        let (effects, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                bridge_events: vec![crate::agent_bridge::BridgeEvent {
                    thread_index: 0,
                    event: crate::protocol_types::AgentEvent::TurnEnded("end_turn".to_owned()),
                }],
                ..FrameInput::default()
            }),
        );
        assert_eq!(
            effects,
            vec![Effect::SendPrompt {
                real_index: 0,
                text: "queued".to_owned(),
            }]
        );
        assert_eq!(model.threads[0].state, ThreadState::Loading);
        assert!(dirty.contains(&Dirty::ThreadRow(0)));
    }

    #[test]
    fn frame_event_for_a_removed_thread_is_a_no_op() {
        let mut model = Model::default();
        let (effects, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                bridge_events: vec![crate::agent_bridge::BridgeEvent {
                    thread_index: 7,
                    event: crate::protocol_types::AgentEvent::TurnEnded("late".to_owned()),
                }],
                selected_thread_snapshot: Some(crate::msg::ThreadFrameSnapshot {
                    thread_id: "thread:7".to_owned(),
                    real_index: 7,
                    transcript: Vec::new(),
                    has_older_messages: false,
                    pending_request: crate::PendingRequestItem::default(),
                    terminals: Vec::new(),
                    expanded_terminal: None,
                    local_terminal: crate::LocalTerminalItem::default(),
                    connection_status: "Unavailable".to_owned(),
                    session_modes: None,
                    config_options: Vec::new(),
                }),
                ..FrameInput::default()
            }),
        );
        assert!(effects.is_empty());
        assert!(dirty.is_empty());
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
    fn prompt_stream_delta_for_a_removed_message_is_a_no_op() {
        let mut model = model_with_threads(&["a"]);
        model.threads[0].session_id = Some("thread-1".to_owned());
        let (_, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::PromptStreamDelta {
                thread_id: "thread-1".to_owned(),
                message_id: "removed-message".to_owned(),
                delta: "late".to_owned(),
            }),
        );
        assert!(dirty.is_empty());
    }

    #[test]
    fn prompt_stream_delta_for_an_existing_message_is_id_keyed() {
        let mut model = model_with_threads(&["a"]);
        model.threads[0].session_id = Some("thread-1".to_owned());
        model.threads[0].message_ids.push("message-1".to_owned());
        let (_, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::PromptStreamDelta {
                thread_id: "thread-1".to_owned(),
                message_id: "message-1".to_owned(),
                delta: "next".to_owned(),
            }),
        );
        assert_eq!(
            dirty,
            vec![Dirty::MessageStreamingDelta {
                thread_id: "thread-1".to_owned(),
                message_id: "message-1".to_owned(),
                delta: "next".to_owned(),
            }]
        );
    }

    #[test]
    fn prompt_stream_delta_accepts_durable_thread_id_before_session_attach() {
        let mut model = model_with_threads(&["a"]);
        model.threads[0].thread_id = "durable-thread-1".to_owned();
        model.threads[0].message_ids.push("message-1".to_owned());
        let (_, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::PromptStreamDelta {
                thread_id: "durable-thread-1".to_owned(),
                message_id: "message-1".to_owned(),
                delta: "next".to_owned(),
            }),
        );
        assert_eq!(
            dirty,
            vec![Dirty::MessageStreamingDelta {
                thread_id: "durable-thread-1".to_owned(),
                message_id: "message-1".to_owned(),
                delta: "next".to_owned(),
            }]
        );
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
        let profiles_model = model.profiles_model.clone();
        let mcp_servers_model = model.mcp_servers_model.clone();
        let agent_catalog_model = model.agent_catalog_model.clone();
        let recoverable_sessions_model = model.recoverable_sessions_model.clone();
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
                    thread_ids: vec!["thread-1".to_owned()],
                    selected_thread_id: None,
                    permission_profiles: vec![],
                    thread_states: vec![],
                },
            ))),
        );
        assert_eq!(model.threads.len(), 1);
        assert_eq!(model.threads[0].display_name, "fresh");
        assert!(std::rc::Rc::ptr_eq(&profiles_model, &model.profiles_model));
        assert!(std::rc::Rc::ptr_eq(
            &mcp_servers_model,
            &model.mcp_servers_model
        ));
        assert!(std::rc::Rc::ptr_eq(
            &agent_catalog_model,
            &model.agent_catalog_model
        ));
        assert!(std::rc::Rc::ptr_eq(
            &recoverable_sessions_model,
            &model.recoverable_sessions_model
        ));
        assert!(!dirty.is_empty());
    }

    #[test]
    fn frame_tick_with_no_real_change_is_a_no_op() {
        let mut model = Model::default();
        let (effects, dirty) = update(&mut model, Msg::Frame(FrameInput::default()));
        assert!(effects.is_empty());
        assert!(dirty.is_empty());
    }

    #[test]
    fn host_theme_changes_are_reducer_owned() {
        let mut model = Model::default();
        let (effects, dirty) = update(
            &mut model,
            Msg::Host(HostMsg::ThemeChanged("light".to_owned())),
        );
        assert!(effects.is_empty());
        assert_eq!(model.theme_variant, "light");
        assert_eq!(dirty, vec![Dirty::Theme]);
    }

    #[test]
    fn host_appearance_changes_mark_only_appearance_dirty() {
        let mut model = Model::default();
        let mut appearance = crate::appearance::AppearanceState::default();
        assert!(appearance.apply(crate::appearance::HostAppearance {
            generation: 1,
            color_scheme: crate::appearance::ColorScheme::Light,
            language_tag: "en-US".to_owned(),
            bundled_font: "Test Sans".to_owned(),
            font_scale: 1.25,
            density: 1.1,
        }));
        let (effects, dirty) =
            update(&mut model, Msg::Host(HostMsg::AppearanceChanged(appearance)));
        assert!(effects.is_empty());
        assert_eq!(model.theme_variant, "light");
        assert_eq!(dirty, vec![Dirty::Appearance]);
    }

    #[test]
    fn frame_attachment_snapshot_becomes_a_persistence_effect_once() {
        let mut model = model_with_threads(&["thread"]);
        let record = crate::state_store::ThreadRecord {
            thread_id: "thread-1".to_owned(),
            display_name: "thread".to_owned(),
            provider: "codex".to_owned(),
            session_id: "session-1".to_owned(),
            profile_name: None,
            permission_profile: None,
            background_session: None,
        };
        let input = FrameInput {
            thread_record_snapshots: vec![record.clone()],
            ..FrameInput::default()
        };
        let (effects, _) = update(&mut model, Msg::Frame(input.clone()));
        assert_eq!(
            effects,
            vec![Effect::PersistThreadRecord {
                record: record.clone()
            }]
        );
        let (effects, _) = update(&mut model, Msg::Frame(input));
        assert!(effects.is_empty());
    }

    #[test]
    fn frame_tick_marks_only_the_external_snapshots_that_changed() {
        let mut model = Model::default();
        let (effects, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                bridge_events: Vec::new(),
                bridge_events_pending: true,
                thread_record_snapshots: Vec::new(),
                settings_reload_pending: true,
                local_terminal_snapshot: Some("$ ".to_owned()),
                prepend_expanded_rows: 0,
                thread_list_snapshot: None,
                selected_thread_snapshot: None,
                clear_selected_thread: false,
                settings_gateway_snapshot: None,
                settings_preferences_snapshot: None,
                skills_snapshot: None,
            }),
        );
        assert!(effects.is_empty());
        assert!(dirty.contains(&Dirty::MessagesDiff {
            thread_id: String::new(),
            ops: Vec::new(),
        }));
        assert!(dirty.contains(&Dirty::Connection {
            thread_id: String::new(),
        }));
        assert!(dirty.contains(&Dirty::Settings));
        assert!(dirty.contains(&Dirty::LocalTerminal));
    }

    #[test]
    fn frame_snapshot_becomes_model_owned_presentation_state() {
        let mut model = model_with_threads(&["thread"]);
        model.threads[0].session_id = Some("thread-1".to_owned());
        model.displayed_thread = Some(0);
        let transcript = vec![crate::conversation::TranscriptItem::Assistant {
            message_id: "message-1".to_owned(),
            text: "hello".to_owned(),
            streaming: true,
        }];
        let (effects, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                selected_thread_snapshot: Some(crate::msg::ThreadFrameSnapshot {
                    thread_id: "thread-1".to_owned(),
                    real_index: 0,
                    transcript: transcript.clone(),
                    has_older_messages: true,
                    pending_request: crate::PendingRequestItem::default(),
                    terminals: vec![],
                    expanded_terminal: None,
                    local_terminal: crate::LocalTerminalItem::default(),
                    connection_status: "Live connection".to_owned(),
                    session_modes: None,
                    config_options: vec![],
                }),
                ..FrameInput::default()
            }),
        );
        assert!(effects.is_empty());
        assert_eq!(model.threads[0].transcript, transcript);
        assert_eq!(
            model.threads[0].transcript_keys,
            vec!["assistant:message-1"]
        );
        assert_eq!(model.threads[0].message_rows.len(), 1);
        assert!(model.threads[0].has_older_messages);
        assert_eq!(model.threads[0].connection_status, "Live connection");
        assert!(dirty.iter().any(|item| matches!(
            item,
            Dirty::MessagesDiff { thread_id, .. } if thread_id == "thread-0"
        )));
        assert!(dirty.iter().any(|item| matches!(
            item,
            Dirty::Connection { thread_id } if thread_id == "thread-0"
        )));
    }

    #[test]
    fn frame_snapshot_resolves_by_thread_id_after_index_shift() {
        let mut model = model_with_threads(&["first", "target", "last"]);
        model.threads[1].session_id = Some("session-target".to_owned());
        model.threads.insert(
            0,
            ThreadModel {
                thread_id: "inserted".to_owned(),
                ..ThreadModel::default()
            },
        );

        let transcript = vec![crate::conversation::TranscriptItem::Assistant {
            message_id: "shifted-message".to_owned(),
            text: "correct target".to_owned(),
            streaming: true,
        }];
        let (_, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                selected_thread_snapshot: Some(crate::msg::ThreadFrameSnapshot {
                    thread_id: "session-target".to_owned(),
                    real_index: 1,
                    transcript: transcript.clone(),
                    has_older_messages: false,
                    pending_request: crate::PendingRequestItem::default(),
                    terminals: vec![],
                    expanded_terminal: None,
                    local_terminal: crate::LocalTerminalItem::default(),
                    connection_status: "Live".to_owned(),
                    session_modes: None,
                    config_options: vec![],
                }),
                ..FrameInput::default()
            }),
        );

        assert!(model.threads[1].transcript.is_empty());
        assert_eq!(model.threads[2].display_name, "target");
        assert_eq!(model.threads[2].transcript, transcript);
        assert!(dirty.iter().any(|item| matches!(
            item,
            Dirty::MessagesDiff { thread_id, .. } if thread_id == "thread-1"
        )));
    }

    #[test]
    fn frame_settings_snapshot_becomes_model_owned_gateway_state() {
        let mut model = Model::default();
        let (effects, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                settings_gateway_snapshot: Some(crate::msg::SettingsGatewaySnapshot {
                    profiles: vec![crate::gateway_actor::ProfileSummary {
                        name: "safe".to_owned(),
                        agent_id: "codex".to_owned(),
                        allow_terminal_access: false,
                        allow_fs_access: true,
                    }],
                    mcp_servers: vec![],
                    agents: vec![],
                    recoverable_sessions: vec![],
                    recovery_provider: "codex".to_owned(),
                }),
                ..FrameInput::default()
            }),
        );

        assert!(effects.is_empty());
        assert_eq!(model.available_profiles.len(), 1);
        assert_eq!(model.available_profiles[0].name, "safe");
        assert_eq!(model.recovery_provider, "codex");
        assert!(dirty.contains(&Dirty::Settings));

        let unchanged_snapshot = crate::msg::SettingsGatewaySnapshot {
            profiles: model.available_profiles.clone(),
            mcp_servers: model.available_mcp_servers.clone(),
            agents: model.agent_catalog.clone(),
            recoverable_sessions: model.recoverable_sessions.clone(),
            recovery_provider: model.recovery_provider.clone(),
        };
        let (_, unchanged_dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                settings_gateway_snapshot: Some(unchanged_snapshot),
                ..FrameInput::default()
            }),
        );
        assert!(unchanged_dirty.is_empty());
    }

    #[test]
    fn frame_skills_snapshot_produces_id_keyed_skill_diff() {
        let mut model = Model::default();
        let skill = crate::skills_state::SkillEntry {
            name: "review".to_owned(),
            description: "Review code".to_owned(),
            path: std::path::PathBuf::from("/tmp/review"),
            scope: crate::skills_state::SkillScope::Global,
            started_from: None,
        };
        let (_, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                skills_snapshot: Some(vec![skill.clone()]),
                ..FrameInput::default()
            }),
        );

        assert_eq!(model.skills, vec![skill]);
        assert!(matches!(
            dirty.as_slice(),
            [Dirty::SkillsListDiff(ops)]
                if matches!(ops.as_slice(), [crate::dirty::RowOp::Insert { at: 0, .. }])
        ));
    }

    #[test]
    fn frame_thread_list_snapshot_uses_durable_ids_as_row_keys() {
        let mut model = Model::default();
        let row = crate::models::VisibleThreadItem {
            real_index: 4,
            thread_id: "durable-thread-4".to_owned(),
            item: crate::ThreadItem {
                name: "filtered".into(),
                ..crate::ThreadItem::default()
            },
        };
        let (_, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                thread_list_snapshot: Some(crate::msg::ThreadListSnapshot {
                    visible_indices: vec![4],
                    visible_thread_ids: vec!["durable-thread-4".to_owned()],
                    rows: vec![row.clone()],
                }),
                ..FrameInput::default()
            }),
        );
        assert_eq!(model.visible_indices, vec![4]);
        assert_eq!(model.thread_rows, vec![row]);
        assert!(matches!(
            dirty.as_slice(),
            [Dirty::ThreadListDiff(ops)]
                if matches!(ops.as_slice(), [RowOp::Insert { at: 0, .. }])
        ));
    }
}
