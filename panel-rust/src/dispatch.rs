//! `tea-slint-model` Phase 4: the first real (behavior-preserving) wiring
//! of a Slint callback through `Msg` -> `update()`. See
//! `memory/rui/gen/plans/tea-slint-model/00-plan.md`.
//!
//! **Why this is a bridge, not the final shape.** `update()`/`Model`
//! don't yet own bridge/store-derived state (transcripts, per-thread
//! errors, terminals, session bindings) -- that's Phase 5+ scope, once
//! `sync/*.rs`'s id-keyed diffing lands for real. Reimplementing
//! `PanelSingleton::select_visible_thread`'s full persist+refresh cascade
//! (filtered->real index translation, `PanelStateStore` write,
//! `refresh_messages_for`'s seven-function fan-out) against `Model` right
//! now would mean duplicating all of that state with no working
//! bridge-aware equivalent yet -- a real regression risk in a live app,
//! not an abstraction nicety. So this phase's first domain cutover is
//! narrower than the plan's ideal end state: the Slint callback now
//! genuinely goes through `Msg::Ui(UiMsg::Thread(..))` ->
//! `update(&mut Model, msg)` (proving the real architecture compiles and
//! is unit-tested, not just simulated), and `update()`'s resulting
//! `Dirty::Scalar(SelectedThread)` is applied by delegating to the
//! existing, proven `PanelSingleton::select_visible_thread` -- not by a
//! parallel `sync()` call -- since that method is what actually owns the
//! bridge/store-aware cascade today. `sync()` still exists and is
//! unit-tested (Phase 3); it just isn't the thing this particular Dirty
//! gets routed through until Phase 5 gives `Model` real ownership of that
//! state.

use crate::dirty::{Dirty, ScalarField};
use crate::model::{Model, ThreadModel};
use crate::msg::{ComposeMsg, Msg, RequestMsg, SettingsMsg, TerminalMsg, ThreadMsg, UiMsg};
use crate::update::update;
use crate::ChatPanel;
use crate::PanelSingleton;

/// Builds the transient stand-in `Model` `update()` needs for a
/// Thread-domain selection message: just enough shape (thread count,
/// current selection) for `update()`'s `wrap_thread_index`/bounds-check
/// logic to produce the correct new index, matching
/// `select_visible_thread`'s own clamping semantics exactly (both use the
/// same `visible_len`).
fn thread_selection_model(panel: &PanelSingleton) -> Model {
    let visible_len = panel.visible_thread_count();
    Model {
        threads: (0..visible_len).map(|_| ThreadModel::default()).collect(),
        selected_thread: panel.selected_thread_index(),
        ..Model::default()
    }
}

/// Applies whatever `Dirty` markers `update()` returned for a
/// Thread-selection `Msg`, using `model`'s **already-computed** new
/// selection -- not a fresh re-read from `panel` (a first draft of this
/// function did that and silently discarded `update()`'s own result,
/// found live via `keyboard_shortcut_tests::
/// invoke_command_switches_threads_and_opens_search_without_any_focus`
/// regressing: `component.get_selected_thread()` stayed `0` after a
/// simulated "next thread" command because the real work was thrown away
/// and `select_visible_thread` got called with the *old* index again).
/// Only `Dirty::Scalar(SelectedThread)` is possible from the two call
/// sites below (see `update::update_thread`'s `Selected`/`NavigateDelta`
/// arms) -- an exhaustive match here still costs nothing and stays
/// honest about what this bridge function does and does not handle yet.
fn apply_thread_selection_dirty(panel: &PanelSingleton, model: &Model, dirty: Vec<Dirty>) {
    for d in dirty {
        match d {
            Dirty::Scalar(ScalarField::SelectedThread) => {
                panel.select_visible_thread(model.selected_thread);
            }
            other => {
                // No other Dirty variant is reachable from
                // ThreadMsg::Selected/NavigateDelta today -- see
                // update::update_thread. Not a silent no-op: surfaces
                // loudly in debug builds if that ever changes without
                // this bridge being updated too.
                debug_assert!(
                    false,
                    "thread-selection dispatch got an unexpected Dirty variant: {other:?}"
                );
            }
        }
    }
}

/// Wired from `component.on_thread_selected` (tea-slint-model Phase 4,
/// Thread domain). `filtered_idx` is a Slint filtered-list index, same
/// space as `select_visible_thread` already expects.
pub(crate) fn dispatch_thread_selected(panel: &PanelSingleton, filtered_idx: usize) {
    let mut model = thread_selection_model(panel);
    let (_effects, dirty) = update(
        &mut model,
        Msg::Ui(UiMsg::Thread(ThreadMsg::Selected(filtered_idx))),
    );
    apply_thread_selection_dirty(panel, &model, dirty);
}

/// Wired from `component.on_thread_navigation_requested` (tea-slint-model
/// Phase 4, Thread domain). `delta` is `+1`/`-1` exactly like the
/// original closure's `wrap_thread_index` call.
pub(crate) fn dispatch_thread_navigate(panel: &PanelSingleton, delta: i32) {
    let mut model = thread_selection_model(panel);
    let (_effects, dirty) = update(
        &mut model,
        Msg::Ui(UiMsg::Thread(ThreadMsg::NavigateDelta(delta))),
    );
    apply_thread_selection_dirty(panel, &model, dirty);
}

/// Wired from `component.on_send_requested` (tea-slint-model Phase 4,
/// Compose domain). Same bridge shape as the Thread domain above:
/// `update()` is genuinely called (proving `Msg::Ui(Compose(SendRequested))`
/// routes and produces the expected `Effect::SendPrompt`), then the real
/// queue/bridge-aware cascade is delegated to
/// `PanelSingleton::dispatch_send_requested` (moved, not rewritten, from
/// the former `on_send_requested` closure body) since `Model` doesn't yet
/// own send-queue/bridge state.
pub(crate) fn dispatch_compose_send(panel: &PanelSingleton, filtered_idx: usize, text: String) {
    let mut model = thread_selection_model(panel);
    let (effects, _dirty) = update(
        &mut model,
        Msg::Ui(UiMsg::Compose(ComposeMsg::SendRequested(text.clone()))),
    );
    debug_assert!(
        effects.is_empty()
            || matches!(effects.as_slice(), [crate::effect::Effect::SendPrompt { .. }]),
        "Compose::SendRequested must produce zero (no selected thread) or one SendPrompt effect"
    );
    panel.dispatch_send_requested(filtered_idx, &text);
}

/// Wired from `component.on_stop_requested` (tea-slint-model Phase 4,
/// Compose domain). See `dispatch_compose_send`'s doc comment -- same
/// bridge shape, delegating to `PanelSingleton::dispatch_stop_requested`.
pub(crate) fn dispatch_compose_stop(panel: &PanelSingleton) {
    let mut model = thread_selection_model(panel);
    let (effects, _dirty) = update(&mut model, Msg::Ui(UiMsg::Compose(ComposeMsg::StopRequested)));
    debug_assert!(
        effects.is_empty()
            || matches!(effects.as_slice(), [crate::effect::Effect::CancelGeneration { .. }]),
        "Compose::StopRequested must produce zero (no selected thread) or one CancelGeneration effect"
    );
    panel.dispatch_stop_requested();
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
    let mut model = thread_selection_model(panel);
    let (effects, _dirty) = update(
        &mut model,
        Msg::Ui(UiMsg::Request(RequestMsg::Approve(String::new()))),
    );
    debug_assert!(
        effects.is_empty()
            || matches!(effects.as_slice(), [crate::effect::Effect::RespondAgentRequest { .. }]),
        "Request::Approve must produce zero (no selected thread) or one RespondAgentRequest effect"
    );
    panel.answer_pending_request(component, true);
}

/// See `dispatch_request_approve`'s doc comment -- same bridge shape.
pub(crate) fn dispatch_request_reject(panel: &PanelSingleton, component: &ChatPanel) {
    let mut model = thread_selection_model(panel);
    let (effects, _dirty) = update(
        &mut model,
        Msg::Ui(UiMsg::Request(RequestMsg::Reject(String::new()))),
    );
    debug_assert!(
        effects.is_empty()
            || matches!(effects.as_slice(), [crate::effect::Effect::RespondAgentRequest { .. }]),
        "Request::Reject must produce zero (no selected thread) or one RespondAgentRequest effect"
    );
    panel.answer_pending_request(component, false);
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
    let mut model = thread_selection_model(panel);
    let (effects, _dirty) = update(
        &mut model,
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
    panel.answer_pending_request_option(component, &option_id);
}

/// Wired from `component.on_load_older_requested` (tea-slint-model Phase
/// 4, Request domain). See `dispatch_request_approve`'s doc comment --
/// same bridge shape, delegating to the existing (moved, not rewritten)
/// `dispatch_load_older_requested`.
pub(crate) fn dispatch_request_load_older(panel: &PanelSingleton) {
    let mut model = thread_selection_model(panel);
    let (effects, _dirty) = update(
        &mut model,
        Msg::Ui(UiMsg::Request(RequestMsg::LoadOlderRequested(String::new()))),
    );
    debug_assert!(
        effects.is_empty()
            || matches!(effects.as_slice(), [crate::effect::Effect::LoadOlderMessages { .. }]),
        "Request::LoadOlderRequested must produce zero (no selected thread) or one LoadOlderMessages effect"
    );
    panel.dispatch_load_older_requested();
}

/// Wired from `component.on_expand_terminal` (tea-slint-model Phase 4,
/// Terminal domain). See `dispatch_request_approve`'s doc comment for
/// the shared bridge shape -- `update()` runs for real, then the actual
/// cascade is delegated to the existing `dispatch_expand_terminal`.
pub(crate) fn dispatch_terminal_expand(
    panel: &PanelSingleton,
    component: &ChatPanel,
    terminal_id: String,
) {
    let mut model = thread_selection_model(panel);
    let (_effects, dirty) = update(
        &mut model,
        Msg::Ui(UiMsg::Terminal(TerminalMsg::Expand(terminal_id.clone()))),
    );
    debug_assert!(
        matches!(dirty.as_slice(), [Dirty::Terminal { .. }]),
        "Terminal::Expand must always produce exactly one Dirty::Terminal"
    );
    panel.dispatch_expand_terminal(component, &terminal_id);
}

/// Wired from `component.on_close_terminal_overlay` (tea-slint-model
/// Phase 4, Terminal domain). See `dispatch_terminal_expand`'s doc
/// comment -- same bridge shape.
pub(crate) fn dispatch_terminal_close_overlay(panel: &PanelSingleton) {
    let mut model = thread_selection_model(panel);
    let (_effects, _dirty) = update(&mut model, Msg::Ui(UiMsg::Terminal(TerminalMsg::CloseOverlay)));
    panel.dispatch_close_terminal_overlay();
}

/// Wired from `component.on_local_terminal_toggle_requested`
/// (tea-slint-model Phase 4, Terminal domain). See
/// `dispatch_terminal_expand`'s doc comment -- same bridge shape.
pub(crate) fn dispatch_terminal_local_toggle(panel: &PanelSingleton, component: &ChatPanel) {
    let mut model = thread_selection_model(panel);
    let (effects, _dirty) = update(&mut model, Msg::Ui(UiMsg::Terminal(TerminalMsg::LocalToggle)));
    debug_assert!(
        matches!(effects.as_slice(), [crate::effect::Effect::LocalTerminalSpawn]),
        "Terminal::LocalToggle must always produce exactly one LocalTerminalSpawn effect"
    );
    panel.dispatch_local_terminal_toggle(component);
}

/// Wired from `component.on_local_terminal_key_input` (tea-slint-model
/// Phase 4, Terminal domain). See `dispatch_terminal_expand`'s doc
/// comment -- same bridge shape.
pub(crate) fn dispatch_terminal_local_key_input(
    panel: &PanelSingleton,
    component: &ChatPanel,
    text: String,
) {
    let mut model = thread_selection_model(panel);
    let (effects, _dirty) = update(
        &mut model,
        Msg::Ui(UiMsg::Terminal(TerminalMsg::LocalKeyInput(
            text.clone().into_bytes(),
        ))),
    );
    debug_assert!(
        matches!(effects.as_slice(), [crate::effect::Effect::LocalTerminalWrite { .. }]),
        "Terminal::LocalKeyInput must always produce exactly one LocalTerminalWrite effect"
    );
    panel.dispatch_local_terminal_key_input(component, &text);
}

/// Wired from `component.on_local_terminal_close_requested`
/// (tea-slint-model Phase 4, Terminal domain). See
/// `dispatch_terminal_expand`'s doc comment -- same bridge shape.
pub(crate) fn dispatch_terminal_local_close(panel: &PanelSingleton, component: &ChatPanel) {
    let mut model = thread_selection_model(panel);
    let (effects, _dirty) = update(&mut model, Msg::Ui(UiMsg::Terminal(TerminalMsg::LocalClose)));
    debug_assert!(
        matches!(effects.as_slice(), [crate::effect::Effect::LocalTerminalKill]),
        "Terminal::LocalClose must always produce exactly one LocalTerminalKill effect"
    );
    panel.dispatch_local_terminal_close(component);
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
    let mut model = thread_selection_model(panel);
    let _ = update(&mut model, Msg::Ui(UiMsg::Settings(crate::msg::SettingsMsg::Open)));
    panel.dispatch_settings_requested(component);
}

pub(crate) fn dispatch_settings_scope_changed(
    panel: &PanelSingleton,
    component: &ChatPanel,
    scope: String,
) {
    let mut model = thread_selection_model(panel);
    let _ = update(
        &mut model,
        Msg::Ui(UiMsg::Settings(SettingsMsg::ScopeChanged(scope.clone()))),
    );
    panel.dispatch_settings_scope_changed(component, &scope);
}

pub(crate) fn dispatch_settings_save(panel: &PanelSingleton, component: &ChatPanel) {
    let mut model = thread_selection_model(panel);
    let _ = update(&mut model, Msg::Ui(UiMsg::Settings(SettingsMsg::Save(String::new()))));
    panel.dispatch_settings_save(component);
}

pub(crate) fn dispatch_settings_close(_panel: &PanelSingleton, component: &ChatPanel) {
    component.set_settings_open(false);
}

pub(crate) fn dispatch_mcp_server_create(
    panel: &PanelSingleton,
    component: &ChatPanel,
    name: String,
    command: String,
) {
    let mut model = thread_selection_model(panel);
    let _ = update(
        &mut model,
        Msg::Ui(UiMsg::Settings(SettingsMsg::McpServerCreate {
            name: name.clone(),
            command: command.clone(),
        })),
    );
    panel.dispatch_mcp_server_create(component, &name, &command);
}

pub(crate) fn dispatch_mcp_server_delete(panel: &PanelSingleton, component: &ChatPanel, name: String) {
    let mut model = thread_selection_model(panel);
    let _ = update(
        &mut model,
        Msg::Ui(UiMsg::Settings(SettingsMsg::McpServerDelete { name: name.clone() })),
    );
    panel.dispatch_mcp_server_delete(component, &name);
}

pub(crate) fn dispatch_mcp_server_enabled_changed(
    panel: &PanelSingleton,
    component: &ChatPanel,
    name: String,
    enabled: bool,
) {
    let mut model = thread_selection_model(panel);
    let _ = update(
        &mut model,
        Msg::Ui(UiMsg::Settings(SettingsMsg::McpServerEnabledChanged {
            name: name.clone(),
            enabled,
        })),
    );
    panel.dispatch_mcp_server_enabled_changed(component, &name, enabled);
}

pub(crate) fn dispatch_profile_create(
    panel: &PanelSingleton,
    component: &ChatPanel,
    name: String,
    agent_id: Option<String>,
    terminal_enabled: bool,
    fs_enabled: bool,
) {
    let mut model = thread_selection_model(panel);
    let _ = update(
        &mut model,
        Msg::Ui(UiMsg::Settings(SettingsMsg::ProfileCreate {
            name: name.clone(),
            agent_id: agent_id.clone(),
            terminal_enabled,
            fs_enabled,
        })),
    );
    panel.dispatch_profile_create(component, &name, agent_id.as_deref(), terminal_enabled, fs_enabled);
}

pub(crate) fn dispatch_profile_delete(panel: &PanelSingleton, component: &ChatPanel, name: String) {
    let mut model = thread_selection_model(panel);
    let _ = update(
        &mut model,
        Msg::Ui(UiMsg::Settings(SettingsMsg::ProfileDelete { name: name.clone() })),
    );
    panel.dispatch_profile_delete(component, &name);
}

pub(crate) fn dispatch_agent_install_requested(
    panel: &PanelSingleton,
    component: &ChatPanel,
    agent_id: String,
) {
    let mut model = thread_selection_model(panel);
    let _ = update(
        &mut model,
        Msg::Ui(UiMsg::Settings(SettingsMsg::AgentInstallRequested {
            agent_id: agent_id.clone(),
        })),
    );
    panel.dispatch_agent_install_requested(component, &agent_id);
}

pub(crate) fn dispatch_dev_mode_toggled(panel: &PanelSingleton, enabled: bool) {
    let mut model = thread_selection_model(panel);
    let _ = update(&mut model, Msg::Ui(UiMsg::Settings(SettingsMsg::DevModeToggled(enabled))));
    panel.dispatch_dev_mode_toggled(enabled);
}

pub(crate) fn dispatch_mode_selected(panel: &PanelSingleton, component: &ChatPanel, mode_id: String) {
    let mut model = thread_selection_model(panel);
    let _ = update(&mut model, Msg::Ui(UiMsg::Settings(SettingsMsg::ModeSelected(mode_id.clone()))));
    panel.dispatch_mode_selected(component, &mode_id);
}

pub(crate) fn dispatch_config_option_selected(
    panel: &PanelSingleton,
    component: &ChatPanel,
    key: String,
    value: String,
) {
    let mut model = thread_selection_model(panel);
    let _ = update(
        &mut model,
        Msg::Ui(UiMsg::Settings(SettingsMsg::ConfigOptionSelected {
            key: key.clone(),
            value: value.clone(),
        })),
    );
    panel.dispatch_config_option_selected(component, &key, &value);
}

#[cfg(test)]
mod tests {
    //! These exercise `thread_selection_model` + `update()`'s pure
    //! decision logic without a live `PanelSingleton`/`ChatPanel`
    //! (constructing either requires the real Slint platform setup --
    //! see `sync.rs`'s doc comment for the same constraint). The
    //! `PanelSingleton`-touching half (`select_visible_thread` actually
    //! being called with the right index) is covered by
    //! `update::tests::thread_navigate_delta_*` for the underlying
    //! `update()` logic, and by real-host VNC click-through for the full
    //! wire (see this phase's meta.json `verified` entry).
    use super::*;

    #[test]
    fn navigate_delta_wraps_the_same_way_select_visible_thread_would_clamp() {
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
