//! `tea-slint-model` Phase 4: the first real (behavior-preserving) wiring
//! of a Slint callback through `Msg` -> `update()`. See
//! `memory/rui/gen/plans/tea-slint-model/00-plan.md`.
//!
//! Dispatcher wrappers translate UI, host, and frame inputs into reducer
//! messages. The reducer owns state transitions, `sync()` owns Slint
//! projection, and this module keeps only lifecycle orchestration that
//! requires exclusive access to the live bridge.

use crate::dirty::{Dirty, ScalarField};
use crate::msg::{
    ChromeMsg, ComposeMsg, HostMsg, Msg, RequestMsg, SettingsMsg, SkillMsg, TerminalMsg, ThreadMsg,
    UiMsg,
};
use crate::sync::sync;
use crate::update::update;
use crate::ChatPanel;
use crate::PanelSingleton;
use slint::platform::WindowAdapter;
use slint::ComponentHandle;

fn execute_effects(panel: &PanelSingleton, effects: Vec<crate::effect::Effect>) {
    crate::effect_executor::execute_effects(panel, effects);
}

/// Execute the two effects that append an `AgentBridge` slot. The bridge's
/// slot vector is intentionally append-only, so these effects need exclusive
/// access to the live panel and must re-enter through `SessionAttached` with
/// the durable binding returned by the bridge.
fn execute_thread_lifecycle_effect(
    panel: &mut PanelSingleton,
    effect: crate::effect::Effect,
) -> crate::effect::EffectResultMsg {
    match effect {
        crate::effect::Effect::NewThread {
            real_index,
            display_name,
            provider,
            profile_name,
            ..
        } => {
            let result = panel
                .bridge
                .as_mut()
                .ok_or_else(|| crate::effect::EffectError::new("agent bridge unavailable"))
                .and_then(|bridge| {
                    bridge
                        .add_thread_with_profile_and_provider(
                            &display_name,
                            profile_name.as_deref(),
                            Some(&provider),
                        )
                        .map_err(|error| crate::effect::EffectError::new(error.to_string()))
                });
            let (thread_id, actual_provider, result) = match result {
                Ok(real_idx) => {
                    let binding = panel
                        .bridge
                        .as_ref()
                        .and_then(|bridge| bridge.thread_binding(real_idx));
                    let actual_provider = panel
                        .bridge
                        .as_ref()
                        .and_then(|bridge| bridge.thread_provider(real_idx));
                    match binding {
                        Some(binding) => (
                            Some(binding.thread_id),
                            actual_provider,
                            Ok(binding.session_id),
                        ),
                        None => (
                            None,
                            actual_provider,
                            Err(crate::effect::EffectError::new(
                                "bridge created a thread without a session binding",
                            )),
                        ),
                    }
                }
                Err(error) => (None, None, Err(error)),
            };
            let result = result.map_err(|error| {
                crate::effect::EffectError::new(format!("thread {real_index}: {error}"))
            });
            crate::effect::EffectResultMsg::SessionAttached {
                real_index,
                thread_id,
                provider: actual_provider,
                result,
            }
        }
        crate::effect::Effect::RecoverSessionAttach {
            real_index,
            session_id,
            provider,
            title,
        } => {
            let result = panel
                .bridge
                .as_mut()
                .ok_or_else(|| crate::effect::EffectError::new("agent bridge unavailable"))
                .and_then(|bridge| {
                    bridge
                        .add_thread_recovering_session(&title, &provider, &session_id)
                        .map_err(|error| crate::effect::EffectError::new(error.to_string()))
                });
            let (thread_id, actual_provider, result) = match result {
                Ok(real_idx) => {
                    let binding = panel
                        .bridge
                        .as_ref()
                        .and_then(|bridge| bridge.thread_binding(real_idx));
                    let actual_provider = panel
                        .bridge
                        .as_ref()
                        .and_then(|bridge| bridge.thread_provider(real_idx));
                    match binding {
                        Some(binding) => (
                            Some(binding.thread_id),
                            actual_provider,
                            Ok(binding.session_id),
                        ),
                        None => (
                            None,
                            actual_provider,
                            Err(crate::effect::EffectError::new(
                                "bridge created a recovery slot without a session binding",
                            )),
                        ),
                    }
                }
                Err(error) => (None, None, Err(error)),
            };
            let result = result.map_err(|error| {
                crate::effect::EffectError::new(format!("recovery thread {real_index}: {error}"))
            });
            crate::effect::EffectResultMsg::SessionAttached {
                real_index,
                thread_id,
                provider: actual_provider,
                result,
            }
        }
        other => panic!("unexpected lifecycle effect: {other:?}"),
    }
}

/// Borrow the one live TEA model owned by `PanelSingleton`.
///
/// This used to construct a fresh stand-in model for every callback. That
/// made `update()`'s state transitions disappear as soon as the callback
/// returned, so a later effect result could not validate against the same
/// thread/message state. The model is now persistent for the lifetime of the
/// panel.
/// Fold a message into the live model and immediately apply only the dirty
/// fields it returned. Effects are still handed to the existing bridge
/// methods by the domain-specific wrappers below; keeping that execution
/// boundary separate means `update()` remains Slint-free and testable.
pub(crate) fn update_persistent(
    panel: &PanelSingleton,
    msg: Msg,
) -> (Vec<crate::effect::Effect>, Vec<Dirty>) {
    let result = {
        let mut model = panel.model.borrow_mut();
        update(&mut model, msg)
    };
    if !result.1.is_empty() {
        let model = panel.model.borrow().clone();
        sync(&model, &panel.component, &result.1);
    }
    result
}

/// Wired from `component.on_thread_selected`. `filtered_idx` is a Slint
/// filtered-list index.
pub(crate) fn dispatch_thread_selected(panel: &PanelSingleton, filtered_idx: usize) {
    let (effects, dirty) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Thread(ThreadMsg::Selected(filtered_idx))),
    );
    execute_effects(panel, effects);
    let selected_thread_snapshot = crate::external_snapshot::ExternalSnapshotSource::new(panel)
        .collect_selected_thread_snapshot();
    let settings_open = { panel.model.borrow().settings_open };
    let settings_gateway_snapshot = settings_open.then(|| {
        crate::external_snapshot::ExternalSnapshotSource::new(panel)
            .collect_settings_gateway_snapshot()
    });
    panel.dispatch_frame_input(crate::msg::FrameInput {
        selected_thread_snapshot,
        settings_gateway_snapshot,
        ..crate::msg::FrameInput::default()
    });
    debug_assert!(dirty
        .iter()
        .all(|d| matches!(d, Dirty::Scalar(ScalarField::SelectedThread))));
}

/// Wired from `component.on_thread_navigation_requested`. `delta` is
/// `+1`/`-1`.
pub(crate) fn dispatch_thread_navigate(panel: &PanelSingleton, delta: i32) {
    let (effects, dirty) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Thread(ThreadMsg::NavigateDelta(delta))),
    );
    execute_effects(panel, effects);
    let selected_thread_snapshot = crate::external_snapshot::ExternalSnapshotSource::new(panel)
        .collect_selected_thread_snapshot();
    let settings_open = { panel.model.borrow().settings_open };
    let settings_gateway_snapshot = settings_open.then(|| {
        crate::external_snapshot::ExternalSnapshotSource::new(panel)
            .collect_settings_gateway_snapshot()
    });
    panel.dispatch_frame_input(crate::msg::FrameInput {
        selected_thread_snapshot,
        settings_gateway_snapshot,
        ..crate::msg::FrameInput::default()
    });
    debug_assert!(dirty
        .iter()
        .all(|d| matches!(d, Dirty::Scalar(ScalarField::SelectedThread))));
}

pub(crate) fn dispatch_thread_recover_session_attach(
    panel: &mut PanelSingleton,
    _component: &ChatPanel,
    session_id: String,
    provider: String,
    title: String,
) {
    let base_name = if title.trim().is_empty() {
        format!(
            "Recovered {}",
            session_id.chars().take(8).collect::<String>()
        )
    } else {
        title
    };
    let mut name = base_name.clone();
    let mut suffix = 2;
    while panel
        .model
        .borrow()
        .threads
        .iter()
        .any(|thread| thread.display_name == name)
    {
        name = format!("{base_name} ({suffix})");
        suffix += 1;
    }

    let (effects, _) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Thread(ThreadMsg::RecoverSessionAttach {
            session_id: session_id.clone(),
            provider: provider.clone(),
            title: name.clone(),
            thread_id: None,
        })),
    );
    let Some(effect) = effects.into_iter().next() else {
        return;
    };
    let real_idx = match &effect {
        crate::effect::Effect::RecoverSessionAttach { real_index, .. } => *real_index,
        _ => return,
    };
    let result = execute_thread_lifecycle_effect(panel, effect);
    let (follow_up, _) = update_persistent(panel, Msg::Effect(result));
    execute_effects(panel, follow_up);
    panel.dispatch_frame_input(crate::msg::FrameInput {
        thread_list_snapshot: Some(
            crate::external_snapshot::ExternalSnapshotSource::new(panel)
                .collect_thread_list_snapshot(),
        ),
        ..crate::msg::FrameInput::default()
    });
    panel.dispatch_frame_input(crate::msg::FrameInput {
        settings_gateway_snapshot: Some(
            crate::external_snapshot::ExternalSnapshotSource::new(panel)
                .collect_settings_gateway_snapshot(),
        ),
        ..crate::msg::FrameInput::default()
    });
    let filtered_idx = panel
        .model
        .borrow()
        .visible_indices
        .iter()
        .position(|idx| *idx == real_idx);
    if let Some(filtered_idx) = filtered_idx {
        dispatch_thread_selected(panel, filtered_idx);
    }
}

pub(crate) fn dispatch_thread_new(panel: &mut PanelSingleton, _component: &ChatPanel) {
    let (effects, _) = update_persistent(panel, Msg::Ui(UiMsg::Thread(ThreadMsg::New)));
    let Some(effect) = effects.into_iter().next() else {
        return;
    };
    let real_idx = match &effect {
        crate::effect::Effect::NewThread { real_index, .. } => *real_index,
        _ => return,
    };
    let result = execute_thread_lifecycle_effect(panel, effect);
    let (follow_up, _) = update_persistent(panel, Msg::Effect(result));
    execute_effects(panel, follow_up);
    panel.dispatch_frame_input(crate::msg::FrameInput {
        thread_list_snapshot: Some(
            crate::external_snapshot::ExternalSnapshotSource::new(panel)
                .collect_thread_list_snapshot(),
        ),
        ..crate::msg::FrameInput::default()
    });

    if let Some(filtered_idx) = panel
        .model
        .borrow()
        .visible_indices
        .iter()
        .position(|idx| *idx == real_idx)
    {
        dispatch_thread_selected(panel, filtered_idx);
    }
}

pub(crate) fn dispatch_thread_rename(
    panel: &PanelSingleton,
    _component: &ChatPanel,
    filtered_idx: usize,
    name: String,
) {
    let name = name.trim().to_owned();
    if name.is_empty() {
        return;
    }
    let Some(real_idx) = panel.real_index(filtered_idx) else {
        return;
    };
    let Some(current_name) = panel
        .model
        .borrow()
        .threads
        .get(real_idx)
        .map(|thread| thread.display_name.clone())
    else {
        return;
    };
    if current_name == name {
        return;
    }
    let (effects, _) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Thread(ThreadMsg::RenameRequested(real_idx, name))),
    );
    execute_effects(panel, effects);
    panel.dispatch_frame_input(crate::msg::FrameInput {
        thread_list_snapshot: Some(
            crate::external_snapshot::ExternalSnapshotSource::new(panel)
                .collect_thread_list_snapshot(),
        ),
        ..crate::msg::FrameInput::default()
    });
    let updated_filtered_idx = panel
        .model
        .borrow()
        .visible_indices
        .iter()
        .position(|idx| *idx == real_idx);
    let has_visible_threads = !panel.model.borrow().visible_indices.is_empty();
    if let Some(updated_filtered_idx) = updated_filtered_idx {
        dispatch_thread_selected(panel, updated_filtered_idx);
    } else if has_visible_threads {
        dispatch_thread_selected(panel, 0);
    }
}

pub(crate) fn dispatch_thread_close(
    panel: &PanelSingleton,
    component: &ChatPanel,
    filtered_idx: usize,
) {
    let Some(idx) = panel.real_index(filtered_idx) else {
        return;
    };
    let (effects, _) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Thread(ThreadMsg::CloseRequested(idx))),
    );
    execute_effects(panel, effects);
    panel.dispatch_frame_input(crate::msg::FrameInput {
        thread_list_snapshot: Some(
            crate::external_snapshot::ExternalSnapshotSource::new(panel)
                .collect_thread_list_snapshot(),
        ),
        ..crate::msg::FrameInput::default()
    });
    if panel.real_index(component.get_selected_thread() as usize) == Some(idx) {
        panel.dispatch_frame_input(crate::msg::FrameInput {
            selected_thread_snapshot: crate::external_snapshot::ExternalSnapshotSource::new(panel)
                .collect_thread_snapshot_for(idx),
            ..crate::msg::FrameInput::default()
        });
    }
}

pub(crate) fn dispatch_thread_delete(
    panel: &PanelSingleton,
    component: &ChatPanel,
    filtered_idx: usize,
) {
    let Some(idx) = panel.real_index(filtered_idx) else {
        return;
    };
    let (effects, _) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Thread(ThreadMsg::DeleteRequested(idx))),
    );
    execute_effects(panel, effects);
    panel.dispatch_frame_input(crate::msg::FrameInput {
        thread_list_snapshot: Some(
            crate::external_snapshot::ExternalSnapshotSource::new(panel)
                .collect_thread_list_snapshot(),
        ),
        ..crate::msg::FrameInput::default()
    });
    if panel.real_index(component.get_selected_thread() as usize) == Some(idx) {
        panel.dispatch_frame_input(crate::msg::FrameInput {
            selected_thread_snapshot: crate::external_snapshot::ExternalSnapshotSource::new(panel)
                .collect_thread_snapshot_for(idx),
            ..crate::msg::FrameInput::default()
        });
    }
}

/// Wired from `component.on_send_requested` (tea-slint-model Phase 4,
/// Compose domain). Same bridge shape as the Thread domain above:
/// `update()` is genuinely called (proving `Msg::Ui(Compose(SendRequested))`
/// routes and produces the expected `Effect::SendPrompt`), then the real
/// queue/bridge-aware cascade is delegated to
/// the bridge send effect executor; the queue/state transition itself is
/// owned by `update()`.
pub(crate) fn dispatch_compose_send(panel: &PanelSingleton, filtered_idx: usize, text: String) {
    let real_idx = panel.real_index(filtered_idx);
    let (effects, _dirty) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Compose(ComposeMsg::SendRequested(text.clone()))),
    );
    debug_assert!(
        effects.is_empty()
            || matches!(
                effects.as_slice(),
                [crate::effect::Effect::SendPrompt { .. }]
            ),
        "Compose::SendRequested must produce zero (no selected thread) or one SendPrompt effect"
    );
    debug_assert!(
        effects.iter().all(|effect| {
            matches!(
                effect,
                crate::effect::Effect::SendPrompt { real_index, .. }
                    if Some(*real_index) == real_idx
            )
        }),
        "send effect must target the selected filtered index"
    );
    execute_effects(panel, effects);
}

/// Wired from `component.on_stop_requested` (tea-slint-model Phase 4,
/// Compose domain). The bridge cancellation is performed by the effect
/// executor after `update()` owns the state transition.
pub(crate) fn dispatch_compose_stop(panel: &PanelSingleton) {
    let (effects, _dirty) =
        update_persistent(panel, Msg::Ui(UiMsg::Compose(ComposeMsg::StopRequested)));
    debug_assert!(
        effects.is_empty()
            || matches!(effects.as_slice(), [crate::effect::Effect::CancelGeneration { .. }]),
        "Compose::StopRequested must produce zero (no selected thread) or one CancelGeneration effect"
    );
    execute_effects(panel, effects);
}

/// Wired from `component.on_approve_request` (tea-slint-model Phase 4,
/// Request domain). The real request id isn't known until
/// `answer_pending_request` looks up the live pending-request list
/// itself (same as before this cutover) -- `update()` is still genuinely
/// called with a decorative id to prove `Msg::Ui(Request(Approve))`
/// routes and produces the expected `Effect::RespondAgentRequest`, then
/// the actual answer is delegated to the existing, unchanged
/// `answer_pending_request`.
pub(crate) fn dispatch_request_approve(panel: &PanelSingleton, component: &ChatPanel) {
    let (effects, _dirty) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Request(RequestMsg::Approve(String::new()))),
    );
    debug_assert!(
        effects.is_empty()
            || matches!(
                effects.as_slice(),
                [crate::effect::Effect::RespondAgentRequest { .. }]
            ),
        "Request::Approve must produce zero (no selected thread) or one RespondAgentRequest effect"
    );
    let _ = component;
    execute_effects(panel, effects);
}

/// See `dispatch_request_approve`'s doc comment -- same bridge shape.
pub(crate) fn dispatch_request_reject(panel: &PanelSingleton, component: &ChatPanel) {
    let (effects, _dirty) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Request(RequestMsg::Reject(String::new()))),
    );
    debug_assert!(
        effects.is_empty()
            || matches!(
                effects.as_slice(),
                [crate::effect::Effect::RespondAgentRequest { .. }]
            ),
        "Request::Reject must produce zero (no selected thread) or one RespondAgentRequest effect"
    );
    let _ = component;
    execute_effects(panel, effects);
}

/// Wired from `component.on_permission_option_selected` (tea-slint-model
/// Phase 4, Request domain). See `dispatch_request_approve`'s doc
/// comment -- same bridge shape, delegating to the existing
/// `answer_pending_request_option`.
pub(crate) fn dispatch_request_permission_option(
    panel: &PanelSingleton,
    component: &ChatPanel,
    option_id: String,
) {
    let (effects, _dirty) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Request(RequestMsg::PermissionOptionSelected(
            String::new(),
            option_id.clone(),
        ))),
    );
    debug_assert!(
        effects.is_empty()
            || matches!(effects.as_slice(), [crate::effect::Effect::PermissionOptionSelected { .. }]),
        "Request::PermissionOptionSelected must produce zero (no selected thread) or one PermissionOptionSelected effect"
    );
    let _ = component;
    execute_effects(panel, effects);
}

/// Wired from `component.on_load_older_requested` (tea-slint-model Phase
/// 4, Request domain). See `dispatch_request_approve`'s doc comment --
/// same bridge shape, delegating to the existing (moved, not rewritten)
/// `dispatch_load_older_requested`.
pub(crate) fn dispatch_request_load_older(panel: &PanelSingleton, component: &ChatPanel) {
    let (effects, _dirty) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Request(
            RequestMsg::LoadOlderRequested(String::new()),
        )),
    );
    debug_assert!(
        effects.is_empty()
            || matches!(effects.as_slice(), [crate::effect::Effect::LoadOlderMessages { .. }]),
        "Request::LoadOlderRequested must produce zero (no selected thread) or one LoadOlderMessages effect"
    );
    execute_effects(panel, effects);
    crate::sync::sync_loading_older(component, false);
}

/// Wired from `component.on_expand_terminal` (tea-slint-model Phase 4,
/// Terminal domain). See `dispatch_request_approve`'s doc comment for
/// the shared bridge shape -- `update()` runs for real, then the actual
/// selected-thread `FrameInput` snapshot.
pub(crate) fn dispatch_terminal_expand(
    panel: &PanelSingleton,
    component: &ChatPanel,
    terminal_id: String,
) {
    let (_effects, dirty) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Terminal(TerminalMsg::Expand(terminal_id.clone()))),
    );
    debug_assert!(
        matches!(dirty.as_slice(), [Dirty::Terminal { .. }]),
        "Terminal::Expand must always produce exactly one Dirty::Terminal"
    );
    let _ = component;
    panel.dispatch_frame_input(crate::msg::FrameInput {
        selected_thread_snapshot: crate::external_snapshot::ExternalSnapshotSource::new(panel)
            .collect_selected_thread_snapshot(),
        ..crate::msg::FrameInput::default()
    });
}

/// Wired from `component.on_close_terminal_overlay` (tea-slint-model
/// Phase 4, Terminal domain). See `dispatch_terminal_expand`'s doc
/// comment -- same bridge shape.
pub(crate) fn dispatch_terminal_close_overlay(panel: &PanelSingleton) {
    let (_effects, _dirty) =
        update_persistent(panel, Msg::Ui(UiMsg::Terminal(TerminalMsg::CloseOverlay)));
    panel.dispatch_frame_input(crate::msg::FrameInput {
        selected_thread_snapshot: crate::external_snapshot::ExternalSnapshotSource::new(panel)
            .collect_selected_thread_snapshot(),
        ..crate::msg::FrameInput::default()
    });
}

/// Wired from `component.on_local_terminal_toggle_requested`
/// (tea-slint-model Phase 4, Terminal domain). See
/// `dispatch_terminal_expand`'s doc comment -- same bridge shape.
pub(crate) fn dispatch_terminal_local_toggle(panel: &PanelSingleton, component: &ChatPanel) {
    let (effects, _dirty) =
        update_persistent(panel, Msg::Ui(UiMsg::Terminal(TerminalMsg::LocalToggle)));
    debug_assert!(
        matches!(
            effects.as_slice(),
            [crate::effect::Effect::LocalTerminalSpawn]
        ),
        "Terminal::LocalToggle must always produce exactly one LocalTerminalSpawn effect"
    );
    let _ = component;
    execute_effects(panel, effects);
}

/// Wired from `component.on_local_terminal_key_input` (tea-slint-model
/// Phase 4, Terminal domain). See `dispatch_terminal_expand`'s doc
/// comment -- same bridge shape.
pub(crate) fn dispatch_terminal_local_key_input(
    panel: &PanelSingleton,
    component: &ChatPanel,
    text: String,
) {
    let (effects, _dirty) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Terminal(TerminalMsg::LocalKeyInput(
            text.clone().into_bytes(),
        ))),
    );
    debug_assert!(
        matches!(
            effects.as_slice(),
            [crate::effect::Effect::LocalTerminalWrite { .. }]
        ),
        "Terminal::LocalKeyInput must always produce exactly one LocalTerminalWrite effect"
    );
    let _ = component;
    execute_effects(panel, effects);
}

/// Wired from `component.on_local_terminal_close_requested`
/// (tea-slint-model Phase 4, Terminal domain). See
/// `dispatch_terminal_expand`'s doc comment -- same bridge shape.
pub(crate) fn dispatch_terminal_local_close(panel: &PanelSingleton, component: &ChatPanel) {
    let (effects, _dirty) =
        update_persistent(panel, Msg::Ui(UiMsg::Terminal(TerminalMsg::LocalClose)));
    debug_assert!(
        matches!(
            effects.as_slice(),
            [crate::effect::Effect::LocalTerminalKill]
        ),
        "Terminal::LocalClose must always produce exactly one LocalTerminalKill effect"
    );
    let _ = component;
    execute_effects(panel, effects);
}

// Settings-domain wrappers (tea-slint-model Phase 4). Same bridge shape
// as the domains above: `update()` genuinely runs (proving the Msg
// routes), then the real cascade is delegated to the matching
// `dispatch_*`/`answer_*` PanelSingleton method, moved verbatim from the
// former closure bodies. Kept terser here (no per-function doc comment,
// no debug_assert on Dirty/Effect shape) since the pattern is now
// established by the four domains above -- see `dispatch_terminal_expand`
// for the fuller-commented version of the same shape.

pub(crate) fn dispatch_settings_open(panel: &PanelSingleton, component: &ChatPanel) {
    let _ = update_persistent(
        panel,
        Msg::Ui(UiMsg::Settings(crate::msg::SettingsMsg::Open)),
    );
    let _ = component;
    panel.dispatch_settings_requested();
}

pub(crate) fn dispatch_settings_scope_changed(
    panel: &PanelSingleton,
    component: &ChatPanel,
    scope: String,
) {
    let _ = update_persistent(
        panel,
        Msg::Ui(UiMsg::Settings(SettingsMsg::ScopeChanged(scope.clone()))),
    );
    let _ = component;
    panel.dispatch_settings_scope_changed(&scope);
}

pub(crate) fn dispatch_settings_save(panel: &PanelSingleton, _component: &ChatPanel) {
    let model = panel.model.borrow();
    let selected_thread_id = panel.real_index(model.selected_thread).and_then(|idx| {
        panel
            .bridge
            .as_ref()
            .and_then(|bridge| bridge.thread_binding(idx))
            .map(|binding| binding.thread_id)
    });
    let input = crate::msg::SettingsSaveInput {
        scope: model.settings_scope.clone(),
        default_profile: model.default_profile.clone(),
        permission_profile: model.permission_profile.clone(),
        background_default: model.background_default,
        default_agent_id: model.default_agent_id.clone(),
        selected_thread_id,
        background_override_set: model.background_override_set,
        background_override: model.background_override,
    };
    drop(model);
    let (effects, _) = update_persistent(panel, Msg::Ui(UiMsg::Settings(SettingsMsg::Save(input))));
    execute_effects(panel, effects);
}

pub(crate) fn dispatch_settings_close(panel: &PanelSingleton, _component: &ChatPanel) {
    let _ = update_persistent(panel, Msg::Ui(UiMsg::Settings(SettingsMsg::Close)));
}

pub(crate) fn dispatch_mcp_server_create(
    panel: &PanelSingleton,
    component: &ChatPanel,
    name: String,
    command: String,
) {
    let (effects, _) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Settings(SettingsMsg::McpServerCreate {
            name: name.clone(),
            command: command.clone(),
        })),
    );
    let _ = component;
    execute_effects(panel, effects);
}

pub(crate) fn dispatch_mcp_server_delete(
    panel: &PanelSingleton,
    component: &ChatPanel,
    name: String,
) {
    let (effects, _) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Settings(SettingsMsg::McpServerDelete {
            name: name.clone(),
        })),
    );
    let _ = component;
    execute_effects(panel, effects);
}

pub(crate) fn dispatch_mcp_server_enabled_changed(
    panel: &PanelSingleton,
    component: &ChatPanel,
    name: String,
    enabled: bool,
) {
    let (effects, _) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Settings(SettingsMsg::McpServerEnabledChanged {
            name: name.clone(),
            enabled,
        })),
    );
    let _ = component;
    execute_effects(panel, effects);
}

pub(crate) fn dispatch_profile_create(
    panel: &PanelSingleton,
    component: &ChatPanel,
    name: String,
    agent_id: Option<String>,
    terminal_enabled: bool,
    fs_enabled: bool,
) {
    let (effects, _) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Settings(SettingsMsg::ProfileCreate {
            name: name.clone(),
            agent_id: agent_id.clone(),
            terminal_enabled,
            fs_enabled,
        })),
    );
    let _ = component;
    execute_effects(panel, effects);
}

pub(crate) fn dispatch_profile_delete(panel: &PanelSingleton, component: &ChatPanel, name: String) {
    let (effects, _) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Settings(SettingsMsg::ProfileDelete {
            name: name.clone(),
        })),
    );
    let _ = component;
    execute_effects(panel, effects);
}

pub(crate) fn dispatch_agent_install_requested(
    panel: &PanelSingleton,
    component: &ChatPanel,
    agent_id: String,
) {
    let (effects, _) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Settings(SettingsMsg::AgentInstallRequested {
            agent_id: agent_id.clone(),
        })),
    );
    let _ = component;
    execute_effects(panel, effects);
}

pub(crate) fn dispatch_dev_mode_toggled(panel: &PanelSingleton, enabled: bool) {
    let (effects, _) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Settings(SettingsMsg::DevModeToggled(enabled))),
    );
    execute_effects(panel, effects);
}

pub(crate) fn dispatch_mode_selected(
    panel: &PanelSingleton,
    component: &ChatPanel,
    mode_id: String,
) {
    let (effects, _) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Settings(SettingsMsg::ModeSelected(mode_id.clone()))),
    );
    let _ = component;
    execute_effects(panel, effects);
}

pub(crate) fn dispatch_config_option_selected(
    panel: &PanelSingleton,
    component: &ChatPanel,
    key: String,
    value: String,
) {
    let (effects, _) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Settings(SettingsMsg::ConfigOptionSelected {
            key: key.clone(),
            value: value.clone(),
        })),
    );
    let _ = component;
    execute_effects(panel, effects);
}

// Skill-domain wrappers (tea-slint-model Phase 4). Same bridge shape as
// Settings above.

pub(crate) fn dispatch_new_skill_requested(panel: &PanelSingleton, name: String, scope: String) {
    let _ = update_persistent(
        panel,
        Msg::Ui(UiMsg::Skill(SkillMsg::NewSkillRequested {
            name: name.clone(),
            scope: scope.clone(),
        })),
    );
    panel.dispatch_new_skill_requested(&name, &scope);
}

pub(crate) fn dispatch_skill_promote_to_global(panel: &PanelSingleton, path: String) {
    let (effects, _) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Skill(SkillMsg::PromoteToGlobal {
            path: path.clone().into(),
        })),
    );
    execute_effects(panel, effects);
}

pub(crate) fn dispatch_skill_editor_open_requested(panel: &PanelSingleton, path: String) {
    let (effects, _) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Skill(SkillMsg::EditorOpenRequested {
            path: path.clone().into(),
        })),
    );
    execute_effects(panel, effects);
}

pub(crate) fn dispatch_skill_content_edited(panel: &PanelSingleton, path: String, content: String) {
    let (effects, _) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Skill(SkillMsg::ContentEdited {
            path: path.clone().into(),
            content: content.clone(),
        })),
    );
    execute_effects(panel, effects);
}

pub(crate) fn dispatch_skill_copy_path_requested(panel: &PanelSingleton, path: String) {
    let _ = update_persistent(
        panel,
        Msg::Ui(UiMsg::Skill(SkillMsg::CopyPathRequested {
            path: path.clone().into(),
        })),
    );
    panel.dispatch_skill_copy_path_requested(&path);
}

pub(crate) fn dispatch_skill_open_in_editor_requested(
    panel: &PanelSingleton,
    editor_name: String,
    path: String,
) {
    let _ = update_persistent(
        panel,
        Msg::Ui(UiMsg::Skill(SkillMsg::OpenInEditorRequested {
            editor_name: editor_name.clone(),
            path: path.clone().into(),
        })),
    );
    panel.dispatch_skill_open_in_editor_requested(&editor_name, &path);
}

pub(crate) fn dispatch_skill_open_with_os_default_requested(panel: &PanelSingleton, path: String) {
    let _ = update_persistent(
        panel,
        Msg::Ui(UiMsg::Skill(SkillMsg::OpenWithOsDefaultRequested {
            path: path.clone().into(),
        })),
    );
    panel.dispatch_skill_open_with_os_default_requested(&path);
}

// Chrome-domain wrappers, plus the two leftover Thread/Request-adjacent
// callbacks (thread_toggle_background, error_banner_dismissed) that share
// this domain's simplicity (tea-slint-model Phase 4). Same bridge shape
// as Settings/Skill above.

pub(crate) fn dispatch_error_banner_dismissed(panel: &PanelSingleton) {
    let (_, dirty) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Chrome(ChromeMsg::ErrorBannerDismissed)),
    );
    debug_assert!(dirty.iter().any(|item| matches!(item, Dirty::Error { .. })));
}

pub(crate) fn dispatch_thread_toggle_background(panel: &PanelSingleton, slint_index: usize) {
    let (effects, _) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Thread(ThreadMsg::ToggleBackground(slint_index))),
    );
    execute_effects(panel, effects);
    panel.dispatch_frame_input(crate::msg::FrameInput {
        thread_list_snapshot: Some(
            crate::external_snapshot::ExternalSnapshotSource::new(panel)
                .collect_thread_list_snapshot(),
        ),
        ..crate::msg::FrameInput::default()
    });
}

pub(crate) fn dispatch_search_changed(
    panel: &PanelSingleton,
    component: &ChatPanel,
    query: String,
) {
    let (_, dirty) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Chrome(ChromeMsg::SearchChanged(query.clone()))),
    );
    let _ = component;
    if dirty
        .iter()
        .any(|item| matches!(item, Dirty::ThreadListDiff(_)))
    {
        panel.dispatch_frame_input(crate::msg::FrameInput {
            thread_list_snapshot: Some(
                crate::external_snapshot::ExternalSnapshotSource::new(panel)
                    .collect_thread_list_snapshot(),
            ),
            ..crate::msg::FrameInput::default()
        });
    }
}

pub(crate) fn dispatch_search_submitted(
    panel: &PanelSingleton,
    _component: &ChatPanel,
    query: String,
    search_skills: bool,
    show_global: bool,
) {
    let (_, dirty) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Chrome(ChromeMsg::SearchSubmitted {
            query: query.clone(),
            search_skills,
            show_global,
        })),
    );
    if search_skills {
        panel.open_skill_search_result(&query, show_global);
        return;
    }
    if dirty
        .iter()
        .any(|item| matches!(item, Dirty::ThreadListDiff(_)))
    {
        panel.dispatch_frame_input(crate::msg::FrameInput {
            thread_list_snapshot: Some(
                crate::external_snapshot::ExternalSnapshotSource::new(panel)
                    .collect_thread_list_snapshot(),
            ),
            ..crate::msg::FrameInput::default()
        });
        if panel.model.borrow().visible_indices.is_empty() {
            panel.dispatch_frame_input(crate::msg::FrameInput {
                clear_selected_thread: true,
                ..crate::msg::FrameInput::default()
            });
        } else {
            dispatch_thread_selected(panel, 0);
        }
    }
}

pub(crate) fn dispatch_toggle_expanded(panel: &PanelSingleton, index: usize) {
    let (_, dirty) = update_persistent(
        panel,
        Msg::Ui(UiMsg::Chrome(ChromeMsg::ToggleExpanded(index))),
    );
    debug_assert!(dirty
        .iter()
        .any(|item| matches!(item, Dirty::MessagesDiff { .. })));
}

// Host-domain wrapper (tea-slint-model Phase 4, non-Slint-callback FFI
// entry points -- see 00-plan.md's "Msg source coverage" point 3). Same
// bridge shape as every UI domain above.

pub(crate) fn dispatch_project_path_changed(panel: &PanelSingleton, path: Option<String>) {
    let (effects, _) =
        update_persistent(panel, Msg::Host(HostMsg::ProjectPathChanged(path.clone())));
    execute_effects(panel, effects);
}

pub(crate) fn dispatch_theme_changed(panel: &PanelSingleton, theme: String) {
    let _ = update_persistent(panel, Msg::Host(HostMsg::ThemeChanged(theme)));
}

pub(crate) fn dispatch_host_invoke_command(panel: &PanelSingleton, command: i32) -> bool {
    let command_name = match command {
        crate::PANEL_COMMAND_PREVIOUS_THREAD => "previous-thread",
        crate::PANEL_COMMAND_NEXT_THREAD => "next-thread",
        crate::PANEL_COMMAND_OPEN_THREAD_SEARCH => "open-thread-search",
        _ => return false,
    };
    let (effects, dirty) = update_persistent(
        panel,
        Msg::Host(HostMsg::InvokeCommand(command_name.to_owned())),
    );
    execute_effects(panel, effects);
    match command {
        crate::PANEL_COMMAND_PREVIOUS_THREAD | crate::PANEL_COMMAND_NEXT_THREAD => {
            let selected_thread_snapshot =
                crate::external_snapshot::ExternalSnapshotSource::new(panel)
                    .collect_selected_thread_snapshot();
            let settings_open = { panel.model.borrow().settings_open };
            let settings_gateway_snapshot = settings_open.then(|| {
                crate::external_snapshot::ExternalSnapshotSource::new(panel)
                    .collect_settings_gateway_snapshot()
            });
            panel.dispatch_frame_input(crate::msg::FrameInput {
                selected_thread_snapshot,
                settings_gateway_snapshot,
                ..crate::msg::FrameInput::default()
            });
        }
        crate::PANEL_COMMAND_OPEN_THREAD_SEARCH => {
            let component_weak = panel.component.as_weak();
            let Some(component) = component_weak.upgrade() else {
                return false;
            };
            component.invoke_open_thread_search();
        }
        _ => return false,
    }
    debug_assert!(
        command == crate::PANEL_COMMAND_OPEN_THREAD_SEARCH
            || dirty
                .iter()
                .all(|item| matches!(item, Dirty::Scalar(ScalarField::SelectedThread)))
    );
    true
}

pub(crate) fn dispatch_frame_input(panel: &PanelSingleton, frame: crate::msg::FrameInput) -> bool {
    let (effects, dirty) = update_persistent(panel, Msg::Frame(frame));
    execute_effects(panel, effects);
    !dirty.is_empty()
}

/// Wired from `panel_rust_poll` (tea-slint-model Phase 4b -- the 60-90fps
/// poll tick). Bridge events are applied first so queued turns and thread
/// state transitions are settled; the selected thread is then snapshotted
/// and folded through `Msg::Frame` so `sync()` owns its transcript,
/// connection, request, terminal, PTY, and capability projections.
pub(crate) fn dispatch_frame_poll(panel: &PanelSingleton) -> bool {
    let frame = crate::external_snapshot::ExternalSnapshotSource::new(panel).collect_frame_input();
    dispatch_frame_input(panel, frame)
}

pub(crate) fn dispatch_apply_host_appearance(
    panel: &PanelSingleton,
    appearance: crate::appearance::HostAppearance,
) -> bool {
    let mut state = panel.model.borrow().appearance.clone();
    if !state.apply(appearance) {
        return false;
    }
    let _ = update_persistent(panel, Msg::Host(HostMsg::AppearanceChanged(state)));
    let current = panel.model.borrow().appearance.current().cloned();
    if let Some(appearance) = current {
        panel
            .window
            .window()
            .dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
                scale_factor: appearance.density,
            });
    }
    panel.window.window().request_redraw();
    true
}

#[cfg(test)]
mod tests {
    //! These exercise the persistent model + `update()`'s pure
    //! decision logic without a live `PanelSingleton`/`ChatPanel`
    //! (constructing either requires the real Slint platform setup --
    //! see `sync.rs`'s doc comment for the same constraint). The
    //! Panel-backed dispatch behavior is covered by
    //! `update::tests::thread_navigate_delta_*` for the underlying
    //! `update()` logic, and by real-host VNC click-through for the full
    //! wire (see this phase's meta.json `verified` entry).
    use super::*;
    use crate::model::Model;
    use crate::model::ThreadModel;

    #[test]
    fn navigate_delta_wraps_the_same_way_the_thread_dispatcher_does() {
        let mut model = Model {
            threads: (0..3).map(|_| ThreadModel::default()).collect(),
            selected_thread: 2,
            ..Model::default()
        };
        let (_, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Thread(ThreadMsg::NavigateDelta(1))),
        );
        assert_eq!(model.selected_thread, 0);
        assert_eq!(dirty, vec![Dirty::Scalar(ScalarField::SelectedThread)]);
    }
}
