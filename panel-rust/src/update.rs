//! `tea-slint-model` Phase 2: `update(&mut Model, Msg) -> (Vec<Effect>,
//! Vec<Dirty>)` -- the **sole** owner of state transitions. See
//! `memory/rui/gen/plans/tea-slint-model/00-plan.md`.
//!
//! **Status: live through dispatchers.** Slint callbacks, selected FFI entry
//! points, cold-start hydration, and the frame tick call this reducer.
//! Returned effects are executed by the dedicated effect executor.
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
use slint::Model as _;

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

/// Wrap a visible-list selection using the same behavior as the original
/// keyboard navigation path.
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
    if model.visible_indices.is_empty() && !model.threads.is_empty() && !model.visible_list_synced {
        // Pre-first-snapshot fallback only. After a real list sync an
        // empty visible list is genuine (project scope/search matched
        // nothing) -- expanding to all threads here silently retargeted
        // actions at hidden threads (review-gate finding 3).
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

// setup-followups plan, provider_fastmode_profile_persistence: pub(crate)
// so sync.rs can resolve the same real index a Msg-level dispatch would,
// instead of hand-rolling the visible_indices-empty-fallback logic
// (current_visible_indices' own reason for existing) a second time and
// risking it drifting out of sync with this one.
/// Plan phase 28: arm the shared action-feedback toast (one popup for
/// error/info/status of user actions) and return its Dirty. `toast_seq`
/// bumps every call so the UI restarts its auto-hide timer even when the
/// same message repeats.
pub(crate) fn show_toast(model: &mut Model, kind: &str, message: impl Into<String>) -> Dirty {
    model.toast_message = message.into();
    model.toast_kind = kind.to_owned();
    model.toast_seq = model.toast_seq.wrapping_add(1);
    Dirty::Toast
}

/// The real (storage) index of whichever thread is genuinely visible and
/// selected right now, or `None` if `model.selected_thread` does not
/// currently resolve to a visible row at all (e.g. an active thread-search
/// filter matches nothing, or momentarily matches fewer rows than the
/// stale selection position).
///
/// This used to fall back to `.unwrap_or(model.selected_thread)` on a
/// lookup miss -- reinterpreting a *filtered-list position* as if it were
/// already a *real storage index* whenever the two spaces happened to
/// overlap in range. That silently pointed every caller (compose send,
/// terminal kill, settings mode/profile/MCP changes, agent-request
/// approve/reject, ...) at whatever thread happened to occupy that real
/// slot -- not the thread actually selected/visible in the UI -- and in
/// `dispatch_compose_send`'s case (the one call site that cross-checks
/// this against a second, independently-computed real index) that
/// disagreement tripped a `debug_assert!` and aborted the whole process
/// (a live-reproduced crash: type a thread-search query that filters the
/// selected thread out of view, then Send). Returning `None` here instead
/// lets every caller degrade to "nothing to do" rather than guess.
pub(crate) fn selected_real_index(model: &Model) -> Option<usize> {
    current_visible_indices(model)
        .get(model.selected_thread)
        .copied()
}

// PROF-9 (`profile-only-backend-selection` plan): a thread's agent is
// "usable" for the purposes of granting it NEW MCP capability -- i.e. not
// ThreadState::Stale (PROF-7: agent_detected_for_profile came back false at
// attach time) and not ThreadModel.unauthenticated (PROF-8: the backend's
// initialize advertised auth methods and none is configured). Fails open
// (returns true) when idx is out of range, matching the fail-open posture
// of agent_detected_for_profile itself -- an unresolvable thread should
// never block an action, only a positively-confirmed-bad one should.
fn thread_agent_usable(model: &Model, idx: usize) -> bool {
    let Some(thread) = model.threads.get(idx) else {
        return true;
    };
    !matches!(thread.state, ThreadState::Stale) && !thread.unauthenticated
}

// setup-followups plan, archive_thread_backend_verify: pub(crate) so a
// real-backend test can build the exact row shape production actually
// produces, rather than hand-crafting a fixture that risks silently
// drifting from what this function really outputs.
pub(crate) fn format_relative_time(
    last_activity: Option<std::time::Instant>,
    state: &ThreadState,
) -> String {
    if matches!(state, ThreadState::Loading | ThreadState::Cancelling) {
        return "now".to_string();
    }
    let Some(t) = last_activity else {
        return "now".to_string();
    };
    let elapsed = t.elapsed().as_secs();
    if elapsed < 60 {
        "now".to_string()
    } else if elapsed < 3600 {
        format!("{}m", elapsed / 60)
    } else if elapsed < 86400 {
        format!("{}h", elapsed / 3600)
    } else if elapsed < 604800 {
        format!("{}d", elapsed / 86400)
    } else {
        format!("{}w", elapsed / 604800)
    }
}

pub(crate) fn visible_thread_row(
    model: &Model,
    real_index: usize,
) -> Option<crate::models::VisibleThreadItem> {
    let thread = model.threads.get(real_index)?;
    let rel_time = format_relative_time(thread.last_activity_time, &thread.state);
    // Prefer durable id; fall back to synthetic so keys always match
    // what ThreadListDiff stored when bridge binding was not yet known.
    let thread_id = if thread.thread_id.is_empty() {
        format!("thread:{real_index}")
    } else {
        thread.thread_id.clone()
    };
    // Preserve display-only fields last written by the frame snapshot
    // (description/provider/model/project/background). A bare
    // `ThreadItem::default()` here used to wipe them on every
    // Dirty::ThreadRow (send/cancel/turn-end), so the sidebar looked
    // stuck or "not updating" while only status flickered — and fought
    // the next frame's full snapshot (setup-followups
    // thread_view_items_not_updating_ui).
    let cached = model
        .thread_rows
        .iter()
        .find(|row| row.real_index == real_index || row.thread_id == thread_id)
        .map(|row| row.item.clone());
    let status = if thread.archived {
        "archived"
    } else if thread.closed {
        "closed"
    } else {
        thread.state.as_str()
    };
    Some(crate::models::VisibleThreadItem {
        real_index,
        thread_id,
        session_id: thread.session_id.clone(),
        // Not the external_snapshot collection path (this helper builds a
        // single row from already-folded model state, used by e.g. the
        // sidebar single-row refresh) -- no new agents/list read happens
        // here, so no new detection info either.
        agent_detected: None,
        item: crate::ThreadItem {
            name: thread.display_name.clone().into(),
            relative_time: rel_time.into(),
            status: status.into(),
            busy: matches!(thread.state, ThreadState::Loading | ThreadState::Cancelling)
                && !thread.closed
                && !thread.archived,
            open: true,
            closed: thread.closed,
            archived: thread.archived,
            profile_name: thread.profile_name.clone().unwrap_or_default().into(),
            has_session: thread.session_id.is_some(),
            description: cached
                .as_ref()
                .map(|c| c.description.clone())
                .unwrap_or_default(),
            background: cached.as_ref().map(|c| c.background).unwrap_or(false),
            provider: cached
                .as_ref()
                .map(|c| c.provider.clone())
                .unwrap_or_default(),
            model: cached.as_ref().map(|c| c.model.clone()).unwrap_or_default(),
            project_path: cached
                .as_ref()
                .map(|c| c.project_path.clone())
                .unwrap_or_default(),
            project_name: cached
                .as_ref()
                .map(|c| c.project_name.clone())
                .unwrap_or_default(),
            project_instance_live: cached
                .as_ref()
                .map(|c| c.project_instance_live)
                .unwrap_or(false),
        },
    })
}

fn thread_row_dirty(model: &Model, real_index: usize) -> Dirty {
    Dirty::ThreadRow {
        thread_id: model
            .threads
            .get(real_index)
            .map(|thread| thread.thread_id.clone())
            .unwrap_or_default(),
    }
}

fn thread_list_dirty_with_keys(model: &mut Model, old_keys: Vec<String>) -> Dirty {
    let new_indices = visible_thread_indices(model);
    let rows: Vec<crate::models::VisibleThreadItem> = new_indices
        .iter()
        .filter_map(|idx| visible_thread_row(model, *idx))
        .collect();
    // Keep indices aligned with rows that still resolve; never panic on a
    // stale filtered index (rust-audit: no expect on model hot path).
    let new_indices: Vec<usize> = rows.iter().map(|row| row.real_index).collect();
    let new_keys: Vec<String> = rows.iter().map(|row| row.thread_id.clone()).collect();
    model.visible_indices = new_indices;
    model.thread_rows = rows.clone();
    Dirty::ThreadListDiff(crate::dirty::diff_by_id(&old_keys, &new_keys, &rows))
}

/// leak_audit_report §1 / §4.1 + per_thread_compose_draft: after the
/// filtered selection index is set, swap compose draft and (when the
/// displayed real thread changes) immediately clear shared view models so
/// the previous thread does not flash into the new one while FrameInput
/// snapshot is still in flight.
fn apply_thread_selection_switch(model: &mut Model) -> (Vec<Effect>, Vec<Dirty>) {
    let real_idx = selected_real_index(model);
    let prev_displayed = model.displayed_thread;
    let switched = prev_displayed != real_idx;

    let mut dirty = vec![Dirty::Scalar(ScalarField::SelectedThread)];

    if switched {
        if std::env::var_os("RUI_THREAD_VIEW_TRACE").is_some() {
            eprintln!(
                "thread-view select from={:?} to={:?} durable={}",
                prev_displayed,
                real_idx,
                real_idx
                    .and_then(|idx| model.threads.get(idx))
                    .map(|thread| thread.thread_id.as_str())
                    .unwrap_or("")
            );
        }
        // Preserve only non-transcript compose state on the leaving thread.
        // Its retained ChatView owns the message rows and presentation state;
        // switching must not snapshot/copy them through a shared list cache.
        if let Some(prev) = prev_displayed {
            if let Some(thread) = model.threads.get_mut(prev) {
                thread.compose_draft = std::mem::take(&mut model.compose_text);
            }
        }
        model.compose_text = real_idx
            .and_then(|idx| model.threads.get(idx))
            .map(|thread| thread.compose_draft.clone())
            .unwrap_or_default();
        dirty.push(Dirty::Scalar(ScalarField::ComposeText));

        // Selection now changes the active retained ChatView. Its own
        // observable message model already contains B's rows, so switching
        // does not install/copy a list into a shared model.
        model.displayed_thread = real_idx;

        let target_id = real_idx
            .and_then(|idx| model.threads.get(idx))
            .map(|t| t.thread_id.clone())
            .unwrap_or_default();
        // Sibling panes: clear-then-frame-fill for pending/error/terminals
        // (same isolation class as messages; frame re-hydrates B).
        dirty.push(Dirty::PendingRequest {
            thread_id: target_id.clone(),
        });
        dirty.push(Dirty::Error {
            thread_id: target_id.clone(),
            detail: crate::dirty::ErrorDetail {
                message: real_idx
                    .and_then(|idx| model.threads.get(idx))
                    .and_then(|t| t.error.clone())
                    .unwrap_or_default(),
            },
        });
        dirty.push(Dirty::Terminal {
            id: real_idx
                .and_then(|idx| model.threads.get(idx))
                .and_then(|t| t.expanded_terminal.as_ref())
                .map(|t| t.terminal_id.to_string())
                .unwrap_or_default(),
        });
        dirty.push(Dirty::LocalTerminal);
        dirty.push(Dirty::Connection {
            thread_id: target_id,
        });
        dirty.push(Dirty::Capabilities {
            thread_id: real_idx
                .and_then(|idx| model.threads.get(idx))
                .and_then(|t| t.session_id.clone())
                .unwrap_or_default(),
        });
    }

    let thread_id = real_idx
        .and_then(|idx| model.threads.get(idx))
        .map(|thread| thread.thread_id.clone());
    (
        thread_id
            .map(|thread_id| vec![Effect::PersistSelectedThread { thread_id }])
            .unwrap_or_default(),
        dirty,
    )
}

/// Re-apply expand flags from prior rows by transcript key after a re-project.
fn merge_expanded_by_key(
    old_keys: &[String],
    old_rows: &[crate::MessageItem],
    new_keys: &[String],
    new_rows: &mut [crate::MessageItem],
) {
    let mut prior: std::collections::HashMap<&str, bool> = std::collections::HashMap::new();
    for (key, row) in old_keys.iter().zip(old_rows.iter()) {
        prior.insert(key.as_str(), row.expanded);
    }
    for (key, row) in new_keys.iter().zip(new_rows.iter_mut()) {
        if let Some(expanded) = prior.get(key.as_str()) {
            row.expanded = *expanded;
        }
    }
}

/// Frame-poll fold merges a fresh `config_options` snapshot (from
/// `AgentBridge::config_options_for_provider`, which never carries a
/// `current_value`) over the thread's existing options. A blind overwrite
/// would silently discard a user's just-made `SettingsMsg::
/// ConfigOptionSelected` pick on a deferred thread the very next frame,
/// before they had a chance to send. Carry the old `current_value` forward
/// onto the matching new option only when it's still a valid choice in the
/// new option's `options[].value` list; otherwise leave the fresh
/// snapshot's `current_value` as-is (never fabricate a value that isn't
/// actually offered anymore).
fn merge_config_options_preserving_current_value(
    old: &[crate::protocol_types::ConfigOptionInfo],
    new: Vec<crate::protocol_types::ConfigOptionInfo>,
) -> Vec<crate::protocol_types::ConfigOptionInfo> {
    new.into_iter()
        .map(|mut option| {
            if let Some(old_option) = old.iter().find(|candidate| candidate.id == option.id) {
                if let Some(value) = old_option.current_value.as_ref() {
                    if option
                        .options
                        .iter()
                        .any(|candidate| &candidate.value == value)
                    {
                        option.current_value = Some(value.clone());
                    }
                }
            }
            option
        })
        .collect()
}

fn update_thread(model: &mut Model, msg: ThreadMsg) -> (Vec<Effect>, Vec<Dirty>) {
    match msg {
        ThreadMsg::New => {
            // The panel-level ACPX gateway can create an unscoped deferred
            // thread before any project is open. Project identity remains
            // optional metadata and is never inferred from process cwd.
            let old_keys = current_visible_keys(model);
            model.compose_text.clear();
            model.search_query.clear();
            let real_index = model.threads.len();
            let thread_id = format!("thread:{real_index}");
            let display_name = format!("New thread {}", real_index + 1);
            // PROF-1/PROF-2: the agent id flows through as-is now, same
            // as `AgentBridge::resolve_provider_for` -- no more collapsing
            // every id down to "codex"/"claude" by a `contains("claude")`
            // guess (the exact `normalize_provider` shape PROF-1 deleted
            // from agent_bridge.rs, just reimplemented independently here
            // and missed in that pass: a real third agent id such as
            // "gemini-acp" would still have been forced onto "codex" at
            // this call site even after agent_bridge.rs itself stopped
            // normalizing). `model.default_agent_id` is used directly when
            // set; the same documented last-resort fallback
            // (`NO_PROVIDER_REQUESTED_FALLBACK`) applies when nothing is
            // configured at all, never an index/contains-based guess.
            let provider = if model.default_agent_id.is_empty() {
                crate::agent_bridge::NO_PROVIDER_REQUESTED_FALLBACK.to_owned()
            } else {
                model.default_agent_id.clone()
            };
            // The literal string "default" is a reserved sentinel, never a
            // real profile name -- see settings_file.rs's
            // non_default_sentinel and acpxmgr.go's WriteConfig doc
            // comment (the "snapflow-mcp-attach" profile's own agent_id
            // is deliberately the placeholder "default", which no real
            // backend is ever registered under). That fix only guards
            // settings loaded from disk into the panel; a raw
            // SettingsMsg::Save can still land a literal "default" in
            // `model.default_profile`/`permission_profile` directly (a
            // settings form re-saved without ever touching the profile
            // dropdown), which then forwards straight to `_acpx.profile`
            // on `session/new` and makes acpx-server try to dial a
            // nonexistent "default" agent forever ("agent default is in
            // crash backoff"). Guard at the point of use too.
            //
            // PROF-2: when no named profile is configured, fall back to
            // `default_agent_id` as the profile name -- acpx's own
            // `Router::ensure_default_profiles_seeded` auto-fills exactly
            // one profile per installed registry agent, named after that
            // agent's own id (`profile.name == agent.id`), specifically so
            // `_acpx.profile` never requires setup for the common case
            // (acpx-core/src/profile.rs's `ProfileSource` doc comment).
            // Without this, a thread with a real `default_agent_id` but no
            // hand-picked profile silently fell all the way through to
            // acpx-server's own native/unmanaged-mode default backend
            // (config.rs's bare `ACPX_BACKEND_CMD` fallback) instead of
            // the agent the user actually configured -- the "profile name
            // -> _acpx.profile" wiring the compose picker itself already
            // relies on, just never reached for the auto-picked case.
            let profile_name = (!model.default_profile.is_empty()
                && model.default_profile != "default")
                .then(|| model.default_profile.clone())
                .or_else(|| {
                    (!model.default_agent_id.is_empty()).then(|| model.default_agent_id.clone())
                });
            let permission_profile = (!model.permission_profile.is_empty()
                && model.permission_profile != "default")
                .then(|| model.permission_profile.clone());
            model.threads.push(ThreadModel {
                thread_id: thread_id.clone(),
                display_name: display_name.clone(),
                provider: provider.clone(),
                profile_name: profile_name.clone(),
                permission_profile: permission_profile.clone(),
                send_queue: new_thread_send_queue(&thread_id),
                ..ThreadModel::default()
            });
            model.rebuild_thread_indices();
            let list_dirty = thread_list_dirty_with_keys(model, old_keys);
            // PUI-014: create the thread DEFERRED -- no ACP session opens until
            // the first message is sent, so the provider/profile picker stays
            // editable. profile_name/permission_profile are already stored on
            // the model thread above and are read at attach time. The imperative
            // attach in the &mut send dispatch reads the then-current provider.
            (
                vec![Effect::NewThreadDeferred {
                    real_index,
                    display_name,
                    provider,
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
            // A deferred New request can complete after the host closes the
            // project. Never materialize that late result into an unscoped
            // thread or let it start an ACP session against process cwd.
            if matches!(model.active_project, crate::model::ProjectIdentity::None) {
                return (vec![], vec![]);
            }
            let old_keys = current_visible_keys(model);
            model.compose_text.clear();
            model.search_query.clear();
            let real_index = model.threads.len();
            let thread_id = thread_id
                .or_else(|| session_id.clone())
                .unwrap_or_else(|| format!("thread:{real_index}"));
            model.threads.push(ThreadModel {
                thread_id: thread_id.clone(),
                display_name,
                provider,
                profile_name,
                permission_profile,
                session_id,
                send_queue: new_thread_send_queue(&thread_id),
                ..ThreadModel::default()
            });
            model.rebuild_thread_indices();
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
            // Clamp, don't no-op: an out-of-range index still selects the
            // last thread rather than being silently ignored.
            let visible_len = if model.visible_indices.is_empty() {
                model.threads.len()
            } else {
                model.visible_indices.len()
            };
            if visible_len == 0 {
                return (vec![], vec![]);
            }
            model.selected_thread = idx.min(visible_len - 1);
            apply_thread_selection_switch(model)
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
            apply_thread_selection_switch(model)
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
                vec![Effect::CloseThread { real_index: idx }],
                vec![thread_row_dirty(model, idx)],
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
        ThreadMsg::ArchiveRequested(idx) => {
            let Some(thread) = model.threads.get_mut(idx) else {
                return (vec![], vec![]);
            };
            // Phase 19: TOGGLE -- the same action resumes an archived
            // thread (sidebar's archived rows wire it as Resume).
            let now_archived = !thread.archived;
            thread.archived = now_archived;
            let mut effects = vec![Effect::ArchiveThread {
                real_index: idx,
                archived: now_archived,
            }];
            let mut dirty = vec![thread_row_dirty(model, idx)];
            // Phase 19 pool cap: at most ARCHIVE_POOL_CAP archived
            // threads; beyond it the OLDEST archived thread is quietly
            // dropped -- permanent delete via the existing delete flow
            // (acpx session close/delete + local purge), per the
            // defaulted open question 1.
            const ARCHIVE_POOL_CAP: usize = 10;
            if now_archived {
                let archived_indices: Vec<usize> = model
                    .threads
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| t.archived && !t.closed)
                    .map(|(i, _)| i)
                    .collect();
                if archived_indices.len() > ARCHIVE_POOL_CAP {
                    // Oldest = first in model order (creation order). Never
                    // drop the thread the user JUST archived -- take the
                    // next-oldest instead (review-gate finding: skipping
                    // entirely left the pool at cap+1 until the next
                    // archive).
                    let drop_idx = archived_indices
                        .iter()
                        .copied()
                        .find(|candidate| *candidate != idx);
                    if let Some(drop_idx) = drop_idx {
                        if let Some(oldest) = model.threads.get_mut(drop_idx) {
                            oldest.closed = true;
                        }
                        effects.push(Effect::DeleteThread {
                            real_index: drop_idx,
                        });
                        dirty.push(thread_row_dirty(model, drop_idx));
                    }
                }
            }
            (effects, dirty)
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
                vec![Effect::ToggleBackground { real_index: idx }],
                vec![thread_row_dirty(model, idx)],
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
            let thread_id = thread_id.unwrap_or_else(|| format!("thread:{}", model.threads.len()));
            model.threads.push(ThreadModel {
                thread_id: thread_id.clone(),
                display_name: title,
                provider: provider.clone(),
                session_id: Some(session_id.clone()),
                send_queue: new_thread_send_queue(&thread_id),
                ..ThreadModel::default()
            });
            model.rebuild_thread_indices();
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

/// Rebuild transcript + send-queue projection after a queue mutation and
/// emit the matching `MessagesDiff` dirty set.
fn rebuild_send_queue_projection(model: &mut Model, idx: usize) -> (String, Vec<Dirty>) {
    let expanded = model.expanded.clone();
    let Some(thread) = model.threads.get_mut(idx) else {
        return (String::new(), vec![]);
    };
    let thread_id = thread.thread_id.clone();
    let old_keys = thread.transcript_keys.clone();
    let in_flight = matches!(thread.state, ThreadState::Loading | ThreadState::Cancelling);
    let (rows, keys) = crate::models::message_rows_for_thread_with_state(
        thread.transcript.clone(),
        &expanded,
        &thread.send_queue,
        in_flight,
        &mut *thread.markdown_render_index.borrow_mut(),
    );
    let ops = crate::dirty::diff_by_id(&old_keys, &keys, &rows);
    thread.transcript_keys = keys;
    (
        thread_id.clone(),
        vec![
            thread_row_dirty(model, idx),
            Dirty::MessagesDiff { thread_id, ops },
        ],
    )
}

/// A brand-new thread's send queue, wired to persist to
/// `<thread_id>.sendqueue.jsonl` going forward -- `send_queue.rs`'s own
/// module doc describes this persistence, but nothing previously called
/// `SendQueue::load`/`send_queue_path` outside that file's own tests, so
/// every `ThreadModel::default()` silently kept `persist_path: None` and
/// a queued-but-unsent message never survived a restart. Uses
/// `new_with_path` (no I/O) rather than `load`, since a genuinely new
/// thread has nothing to load; `Model::from_initial_state`'s cold-start
/// path is the one that actually restores prior queue content from disk.
fn new_thread_send_queue(thread_id: &str) -> crate::send_queue::SendQueue {
    crate::send_queue::SendQueue::new_with_path(crate::send_queue::send_queue_path(
        &crate::agent_bridge::resolve_cache_dir(),
        thread_id,
    ))
}

fn queue_entry_id_at(
    thread: &ThreadModel,
    message_index: usize,
) -> Option<crate::send_queue::QueueEntryId> {
    let key = thread.transcript_keys.get(message_index)?;
    let raw = key.strip_prefix("queue:")?;
    let n: u64 = raw.parse().ok()?;
    Some(crate::send_queue::QueueEntryId(n))
}

fn update_compose(model: &mut Model, msg: ComposeMsg) -> (Vec<Effect>, Vec<Dirty>) {
    // Every arm below that actually uses `idx` already immediately bails
    // out via its own `model.threads.get(idx)`/`get_mut(idx)` guard, and
    // the handful that don't (MentionToken*/WordBoundaryBefore/ContainsCi)
    // already unconditionally return `(vec![], vec![])` regardless of
    // `idx` -- so bailing here whenever nothing is genuinely selected/
    // visible changes nothing for those, and fixes every other arm at
    // once (see `selected_real_index`'s doc comment for the bug this
    // closes).
    let Some(idx) = selected_real_index(model) else {
        return (vec![], vec![]);
    };
    match msg {
        ComposeMsg::DraftChanged(text) => {
            model.compose_text = text.clone();
            if let Some(thread) = model.threads.get_mut(idx) {
                thread.compose_draft = text;
            }
            (vec![], vec![])
        }
        ComposeMsg::SendRequested(text) => {
            model.compose_text.clear();
            let Some(thread) = model.threads.get_mut(idx) else {
                return (vec![], vec![]);
            };
            let thread_id = thread.thread_id.clone();
            if matches!(thread.state, ThreadState::Loading | ThreadState::Cancelling) {
                return match thread.send_queue.enqueue(text, false) {
                    Ok(_) => {
                        // Rebuild message projection with queue rows so
                        // QueuedMessageBar appears immediately.
                        let expanded = model.expanded.clone();
                        let old_keys = thread.transcript_keys.clone();
                        let in_flight =
                            matches!(thread.state, ThreadState::Loading | ThreadState::Cancelling);
                        let (rows, keys) = crate::models::message_rows_for_thread_with_state(
                            thread.transcript.clone(),
                            &expanded,
                            &thread.send_queue,
                            in_flight,
                            &mut *thread.markdown_render_index.borrow_mut(),
                        );
                        let ops = crate::dirty::diff_by_id(&old_keys, &keys, &rows);
                        thread.transcript_keys = keys;
                        (
                            vec![],
                            vec![
                                thread_row_dirty(model, idx),
                                Dirty::Scalar(ScalarField::ComposeText),
                                Dirty::MessagesDiff {
                                    thread_id: thread_id.clone(),
                                    ops,
                                },
                            ],
                        )
                    }
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
            thread.agent_content_this_turn = false;
            thread.last_activity_time = Some(std::time::Instant::now());
            // Sending resumes auto-processing after a manual stop.
            thread.send_queue.resume();
            (
                vec![Effect::SendPrompt {
                    thread_id: thread_id.clone(),
                    text,
                }],
                vec![
                    // Without this, the sidebar spinner and the chat
                    // area's live-tail pulse (both driven by this row's
                    // rendered `status`/`busy`) didn't start until some
                    // unrelated event later forced a full thread-list
                    // rebuild -- "loading should start immediately on
                    // send" was true in `model.threads[idx].state` above,
                    // just not yet visible.
                    thread_row_dirty(model, idx),
                    Dirty::Connection { thread_id },
                    Dirty::Scalar(ScalarField::ComposeText),
                ],
            )
        }
        ComposeMsg::StopRequested | ComposeMsg::QueueStop => {
            let Some(thread) = model.threads.get_mut(idx) else {
                return (vec![], vec![]);
            };
            // Manual stop freezes the queue until the user re-engages
            // (SendQueue::pause / resume).
            thread.send_queue.pause();
            thread.state = ThreadState::Cancelling;
            (
                vec![Effect::CancelGeneration { real_index: idx }],
                vec![thread_row_dirty(model, idx)],
            )
        }
        ComposeMsg::GenerationStopped => {
            let Some(thread) = model.threads.get_mut(idx) else {
                return (vec![], vec![]);
            };
            thread.state = ThreadState::Idle;
            (vec![], vec![thread_row_dirty(model, idx)])
        }
        ComposeMsg::QueueCancel { message_index } => {
            let entry_id = {
                let Some(thread) = model.threads.get(idx) else {
                    return (vec![], vec![]);
                };
                match queue_entry_id_at(thread, message_index) {
                    Some(id) => id,
                    None => return (vec![], vec![]),
                }
            };
            let remove_result = {
                let Some(thread) = model.threads.get_mut(idx) else {
                    return (vec![], vec![]);
                };
                thread.send_queue.remove(entry_id)
            };
            match remove_result {
                Ok(Some(_)) => {
                    let (_thread_id, dirty) = rebuild_send_queue_projection(model, idx);
                    (vec![], dirty)
                }
                Ok(None) => (vec![], vec![]),
                Err(error) => {
                    let message = error.to_string();
                    let Some(thread) = model.threads.get_mut(idx) else {
                        return (vec![], vec![]);
                    };
                    let thread_id = thread.thread_id.clone();
                    thread.error = Some(message.clone());
                    (
                        vec![],
                        vec![Dirty::Error {
                            thread_id,
                            detail: ErrorDetail { message },
                        }],
                    )
                }
            }
        }
        ComposeMsg::QueueEdit { message_index } => {
            let entry_id = {
                let Some(thread) = model.threads.get(idx) else {
                    return (vec![], vec![]);
                };
                match queue_entry_id_at(thread, message_index) {
                    Some(id) => id,
                    None => return (vec![], vec![]),
                }
            };
            let remove_result = {
                let Some(thread) = model.threads.get_mut(idx) else {
                    return (vec![], vec![]);
                };
                thread.send_queue.remove(entry_id)
            };
            match remove_result {
                Ok(Some(entry)) => {
                    model.compose_text = entry.text;
                    let (_thread_id, mut dirty) = rebuild_send_queue_projection(model, idx);
                    dirty.push(Dirty::Scalar(ScalarField::ComposeText));
                    (vec![], dirty)
                }
                Ok(None) => (vec![], vec![]),
                Err(error) => {
                    let message = error.to_string();
                    let Some(thread) = model.threads.get_mut(idx) else {
                        return (vec![], vec![]);
                    };
                    let thread_id = thread.thread_id.clone();
                    thread.error = Some(message.clone());
                    (
                        vec![],
                        vec![Dirty::Error {
                            thread_id,
                            detail: ErrorDetail { message },
                        }],
                    )
                }
            }
        }
        ComposeMsg::QueueSendNow { message_index } => {
            let (entry_id, is_generating) = {
                let Some(thread) = model.threads.get(idx) else {
                    return (vec![], vec![]);
                };
                let Some(entry_id) = queue_entry_id_at(thread, message_index) else {
                    return (vec![], vec![]);
                };
                let is_generating =
                    matches!(thread.state, ThreadState::Loading | ThreadState::Cancelling);
                (entry_id, is_generating)
            };
            let send_now_result = {
                let Some(thread) = model.threads.get_mut(idx) else {
                    return (vec![], vec![]);
                };
                thread.send_queue.send_now(entry_id, is_generating)
            };
            match send_now_result {
                Ok(Some(entry)) => {
                    let Some(thread) = model.threads.get_mut(idx) else {
                        return (vec![], vec![]);
                    };
                    let thread_id = thread.thread_id.clone();
                    thread.error = None;
                    thread.state = ThreadState::Loading;
                    let (_thread_id, mut dirty) = rebuild_send_queue_projection(model, idx);
                    dirty.push(Dirty::Connection {
                        thread_id: thread_id.clone(),
                    });
                    let mut effects = Vec::with_capacity(2);
                    if is_generating {
                        // A turn is already in flight -- cancel it. The
                        // resulting Stopped/TurnEnded event is absorbed by
                        // the queue's AbsorbingCancel state (armed by
                        // send_now above) so it doesn't also auto-drain
                        // the next entry once send_prompt below starts a
                        // new one.
                        effects.push(Effect::CancelGeneration { real_index: idx });
                    }
                    effects.push(Effect::SendPrompt {
                        thread_id: thread_id.clone(),
                        text: entry.text,
                    });
                    (effects, dirty)
                }
                Ok(None) => (vec![], vec![]),
                Err(error) => {
                    let message = error.to_string();
                    let Some(thread) = model.threads.get_mut(idx) else {
                        return (vec![], vec![]);
                    };
                    let thread_id = thread.thread_id.clone();
                    thread.error = Some(message.clone());
                    (
                        vec![],
                        vec![Dirty::Error {
                            thread_id,
                            detail: ErrorDetail { message },
                        }],
                    )
                }
            }
        }
        ComposeMsg::QueueFastTrack => {
            let is_generating = {
                let Some(thread) = model.threads.get(idx) else {
                    return (vec![], vec![]);
                };
                matches!(thread.state, ThreadState::Loading | ThreadState::Cancelling)
            };
            let fast_track_result = {
                let Some(thread) = model.threads.get_mut(idx) else {
                    return (vec![], vec![]);
                };
                thread.send_queue.try_fast_track(is_generating)
            };
            match fast_track_result {
                Ok(Some(entry)) => {
                    let Some(thread) = model.threads.get_mut(idx) else {
                        return (vec![], vec![]);
                    };
                    let thread_id = thread.thread_id.clone();
                    thread.error = None;
                    thread.state = ThreadState::Loading;
                    let (_thread_id, mut dirty) = rebuild_send_queue_projection(model, idx);
                    dirty.push(Dirty::Connection {
                        thread_id: thread_id.clone(),
                    });
                    let mut effects = Vec::with_capacity(2);
                    if is_generating {
                        // Same AbsorbingCancel handoff as QueueSendNow --
                        // try_fast_track already armed it above.
                        effects.push(Effect::CancelGeneration { real_index: idx });
                    }
                    effects.push(Effect::SendPrompt {
                        thread_id: thread_id.clone(),
                        text: entry.text,
                    });
                    (effects, dirty)
                }
                // Safe no-op: no can_fast_track-eligible entry (queue
                // empty, or the last mutation wasn't a fresh enqueue) --
                // the Slint side fires this unconditionally on empty-
                // compose Return, so this is the expected common case.
                Ok(None) => (vec![], vec![]),
                Err(error) => {
                    let message = error.to_string();
                    let Some(thread) = model.threads.get_mut(idx) else {
                        return (vec![], vec![]);
                    };
                    let thread_id = thread.thread_id.clone();
                    thread.error = Some(message.clone());
                    (
                        vec![],
                        vec![Dirty::Error {
                            thread_id,
                            detail: ErrorDetail { message },
                        }],
                    )
                }
            }
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
    // Every arm targets whichever thread `idx` names with no bail-out of
    // its own (unlike `update_compose`) -- previously this fired an effect
    // at a possibly-wrong real index whenever `idx` was unsound (see
    // `selected_real_index`'s doc comment); now it's a clean no-op when
    // nothing is genuinely selected/visible.
    let Some(idx) = selected_real_index(model) else {
        return (vec![], vec![]);
    };
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
    // Only `Kill` below needs a real thread index -- the rest operate on
    // the terminal overlay/local-terminal state independent of thread
    // selection, so (unlike `update_compose`/`update_request`) the guard
    // stays local to that one arm rather than gating the whole function.
    match msg {
        TerminalMsg::Expand(id) => {
            // Opens (if not already open) AND activates -- the popup row
            // that fires this is the one path that can introduce a brand
            // new tab; the tab strip itself only ever fires `SelectTab`
            // for ids already in `open_terminal_ids`.
            if !model.open_terminal_ids.contains(&id) {
                model.open_terminal_ids.push(id.clone());
            }
            model.expanded_terminal_id = Some(id.clone());
            (vec![], vec![Dirty::Terminal { id }])
        }
        TerminalMsg::SelectTab(id) => {
            // Ignore a stray id rather than implicitly re-opening a tab
            // the user already dismissed (e.g. a tab-strip click racing a
            // `CloseTab` for the same id).
            if model.open_terminal_ids.contains(&id) {
                model.expanded_terminal_id = Some(id.clone());
            }
            (vec![], vec![Dirty::Terminal { id }])
        }
        TerminalMsg::CloseTab(id) => {
            if let Some(pos) = model.open_terminal_ids.iter().position(|open| *open == id) {
                model.open_terminal_ids.remove(pos);
                if model.expanded_terminal_id.as_deref() == Some(id.as_str()) {
                    // Prefer the tab that slid into the closed one's slot
                    // (what used to be its right-hand neighbor); fall back
                    // to the one before it; `None` if the list is now
                    // empty (last tab closed == whole overlay closed).
                    model.expanded_terminal_id = model
                        .open_terminal_ids
                        .get(pos)
                        .or_else(|| {
                            pos.checked_sub(1)
                                .and_then(|prev| model.open_terminal_ids.get(prev))
                        })
                        .cloned();
                }
            }
            (
                vec![],
                vec![Dirty::Terminal {
                    id: model.expanded_terminal_id.clone().unwrap_or_default(),
                }],
            )
        }
        TerminalMsg::CloseOverlay => {
            let id = model.expanded_terminal_id.take();
            model.open_terminal_ids.clear();
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
        TerminalMsg::Kill(terminal_id) => {
            let Some(idx) = selected_real_index(model) else {
                return (vec![], vec![]);
            };
            // No model mutation here -- the real exit is observed the
            // same way any other terminal exit is, via the next
            // `AgentEvent::TerminalOutput` carrying a non-null
            // `exitStatus` (see `AcpxThreadHandle::kill_terminal`'s doc
            // comment). Nothing to mark dirty until that arrives.
            (
                vec![Effect::KillAgentTerminal {
                    real_index: idx,
                    terminal_id,
                }],
                vec![],
            )
        }
    }
}

fn update_settings(model: &mut Model, msg: SettingsMsg) -> (Vec<Effect>, Vec<Dirty>) {
    // Open/Close/Save/ScopeChanged/DevModeToggled are global settings
    // actions that must still work with no thread genuinely selected/
    // visible (e.g. Settings should still open while a thread search
    // filters the list to nothing) -- so unlike `update_compose`/
    // `update_request`, `idx` is resolved per-arm below, only where an
    // arm actually targets a specific thread's gateway/session.
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
            // See ThreadMsg::New's comment: "default" is a reserved
            // sentinel that must never be treated as a real profile name,
            // including here where a settings form re-save (without the
            // user ever touching the profile dropdown) could otherwise
            // land the literal string straight into `model.default_profile`.
            model.default_profile = if input.default_profile == "default" {
                String::new()
            } else {
                input.default_profile.clone()
            };
            model.permission_profile = if input.permission_profile == "default" {
                String::new()
            } else {
                input.permission_profile.clone()
            };
            model.background_default = input.background_default;
            model.default_agent_id = input.default_agent_id.clone();
            model.background_override_set = input.background_override_set;
            model.background_override = input.background_override;
            model.show_global_skills = input.show_global_skills;
            model.settings_open = false;
            (
                vec![Effect::SaveSettings { input }],
                vec![Dirty::Settings, Dirty::Scalar(ScalarField::SettingsOpen)],
            )
        }
        SettingsMsg::ScopeChanged(scope) => {
            model.settings_scope = scope;
            (
                vec![],
                vec![Dirty::Scalar(ScalarField::SettingsScope), Dirty::Settings],
            )
        }
        SettingsMsg::ConfigOptionSelected { key, value } => {
            let Some(idx) = selected_real_index(model) else {
                return (vec![], vec![]);
            };
            let Some(thread) = model.threads.get_mut(idx) else {
                return (vec![], vec![]);
            };
            if thread.session_id.is_none() {
                if let Some(option) = thread.config_options.iter_mut().find(|option| {
                    option.id == key
                        && option
                            .options
                            .iter()
                            .any(|candidate| candidate.value == value)
                }) {
                    option.current_value = Some(value);
                }
                return (
                    vec![],
                    vec![
                        Dirty::Settings,
                        Dirty::Capabilities {
                            thread_id: thread.thread_id.clone(),
                        },
                    ],
                );
            }
            (
                vec![Effect::SetConfigOption {
                    real_index: idx,
                    key,
                    value,
                }],
                vec![Dirty::Settings],
            )
        }
        SettingsMsg::ModeSelected(mode) => {
            let Some(idx) = selected_real_index(model) else {
                return (vec![], vec![]);
            };
            (
                vec![Effect::SetMode {
                    real_index: idx,
                    mode,
                }],
                vec![Dirty::Settings],
            )
        }
        SettingsMsg::ProfileSelected {
            profile_name,
            agent_id,
        } => {
            let Some(idx) = selected_real_index(model) else {
                return (vec![], vec![]);
            };
            // Resolve agent_id before mutably borrowing the thread (may need
            // available_profiles if UI only sent the profile name).
            let resolved_agent = if !agent_id.is_empty() {
                agent_id
            } else {
                model
                    .available_profiles
                    .iter()
                    .find(|p| p.name == profile_name)
                    .map(|p| p.agent_id.clone())
                    .filter(|id| !id.is_empty())
                    .unwrap_or_default()
            };
            let Some(thread) = model.threads.get_mut(idx) else {
                return (vec![], vec![]);
            };
            // Silently ignored, not an error: the picker itself is only
            // ever interactive while has-session is false (see
            // ThreadItem.has-session's doc comment), so reaching this
            // with an already-attached thread means the UI raced a
            // session attach completing -- the picker will disable
            // itself on the very next Dirty::ThreadRow either way. No
            // Effect (unlike ModeSelected/ConfigOptionSelected): nothing
            // to tell the backend yet, since there's no session to send
            // it to -- attach_deferred_thread / open_session_maybe_profiled
            // read provider + profile_name from the model at first send.
            if thread.session_id.is_some() {
                return (vec![], vec![]);
            }
            thread.profile_name = Some(profile_name);
            // Critical: deferred attach uses thread.provider (agent id), not
            // profile_name. Leaving provider at create-time default made the
            // Provider picker a pure cosmetic change (always opened default
            // agent with profile as a secondary ACPX name).
            if !resolved_agent.is_empty() {
                thread.provider = resolved_agent;
            }
            let thread_id = thread.thread_id.clone();
            (
                vec![],
                vec![
                    Dirty::ThreadRow {
                        thread_id: thread_id.clone(),
                    },
                    Dirty::Capabilities { thread_id },
                ],
            )
        }
        SettingsMsg::DevModeToggled(enabled) => {
            model.dev_mode = enabled;
            (vec![Effect::SaveDevMode { enabled }], vec![Dirty::Settings])
        }
        // PROF-9 (`profile-only-backend-selection` plan): block MCP-server
        // actions that would make NEW capabilities available to an agent
        // that cannot serve them (create, or turning something on) when
        // the selected thread's agent is Stale (PROF-7) or unauthenticated
        // (PROF-8) -- so the user cannot drive MCP against an agent that
        // cannot serve it. Deliberately NOT blocked: delete (cleanup must
        // always be reachable, especially precisely when something is
        // broken), turning something OFF (same reasoning), and
        // authenticate (that flow is the MCP SERVER's own credentials --
        // orthogonal to whether the ACP AGENT itself is reachable/
        // authenticated, and blocking it here would trap a user trying to
        // fix the MCP side first).
        SettingsMsg::McpServerCreate { entry } => {
            let Some(idx) = selected_real_index(model) else {
                return (vec![], vec![]);
            };
            if !thread_agent_usable(model, idx) {
                let toast = show_toast(
                    model,
                    "error",
                    "Can't add an MCP server: this thread's agent is stale or unauthenticated",
                );
                return (vec![], vec![toast]);
            }
            (
                vec![Effect::McpServerCreate {
                    real_index: idx,
                    entry,
                }],
                vec![Dirty::Settings],
            )
        }
        SettingsMsg::McpServerUpdate { entry } => {
            let Some(idx) = selected_real_index(model) else {
                return (vec![], vec![]);
            };
            (
                vec![Effect::McpServerUpdate {
                    real_index: idx,
                    entry,
                }],
                vec![Dirty::Settings],
            )
        }
        SettingsMsg::McpServerDelete { name } => {
            let Some(idx) = selected_real_index(model) else {
                return (vec![], vec![]);
            };
            (
                vec![Effect::McpServerDelete {
                    real_index: idx,
                    name,
                }],
                vec![Dirty::Settings],
            )
        }
        SettingsMsg::McpServerEnabledChanged { name, enabled } => {
            let Some(idx) = selected_real_index(model) else {
                return (vec![], vec![]);
            };
            if enabled && !thread_agent_usable(model, idx) {
                let toast = show_toast(
                    model,
                    "error",
                    "Can't enable this MCP server: this thread's agent is stale or \
                     unauthenticated",
                );
                return (vec![], vec![toast]);
            }
            (
                vec![Effect::McpServerEnabledChanged {
                    real_index: idx,
                    name,
                    enabled,
                }],
                vec![Dirty::Settings],
            )
        }
        SettingsMsg::McpServerAuthenticate { name } => {
            let Some(idx) = selected_real_index(model) else {
                return (vec![], vec![]);
            };
            (
                vec![Effect::McpServerAuthenticate {
                    real_index: idx,
                    name,
                }],
                vec![Dirty::Settings],
            )
        }
        SettingsMsg::McpServerLogout { name } => {
            let Some(idx) = selected_real_index(model) else {
                return (vec![], vec![]);
            };
            (
                vec![Effect::McpServerLogout {
                    real_index: idx,
                    name,
                }],
                vec![Dirty::Settings],
            )
        }
        SettingsMsg::McpServerToolEnabledChanged {
            server_name,
            tool_name,
            enabled,
        } => {
            let Some(idx) = selected_real_index(model) else {
                return (vec![], vec![]);
            };
            if enabled && !thread_agent_usable(model, idx) {
                let toast = show_toast(
                    model,
                    "error",
                    "Can't enable this tool: this thread's agent is stale or unauthenticated",
                );
                return (vec![], vec![toast]);
            }
            (
                vec![Effect::McpServerToolEnabledChanged {
                    real_index: idx,
                    server_name,
                    tool_name,
                    enabled,
                }],
                vec![Dirty::Settings],
            )
        }
        SettingsMsg::McpServerToolDeferredChanged {
            server_name,
            tool_name,
            deferred,
        } => {
            let Some(idx) = selected_real_index(model) else {
                return (vec![], vec![]);
            };
            (
                vec![Effect::McpServerToolDeferredChanged {
                    real_index: idx,
                    server_name,
                    tool_name,
                    deferred,
                }],
                vec![Dirty::Settings],
            )
        }
        SettingsMsg::McpServerToolsFetchRequested { server_name } => {
            let Some(idx) = selected_real_index(model) else {
                return (vec![], vec![]);
            };
            (
                vec![Effect::McpServerToolsFetchRequested {
                    real_index: idx,
                    server_name,
                }],
                vec![Dirty::Settings],
            )
        }
        SettingsMsg::ProfileCreate {
            name,
            agent_id,
            terminal_enabled,
            fs_enabled,
        } => {
            let Some(idx) = selected_real_index(model) else {
                return (vec![], vec![]);
            };
            (
                vec![Effect::ProfileCreate {
                    real_index: idx,
                    name,
                    agent_id,
                    terminal_enabled,
                    fs_enabled,
                }],
                vec![Dirty::Settings],
            )
        }
        SettingsMsg::ProfileDelete { name } => {
            let Some(idx) = selected_real_index(model) else {
                return (vec![], vec![]);
            };
            (
                vec![Effect::ProfileDelete {
                    real_index: idx,
                    name,
                }],
                vec![Dirty::Settings],
            )
        }
        SettingsMsg::AgentInstallRequested { agent_id } => {
            let Some(idx) = selected_real_index(model) else {
                return (vec![], vec![]);
            };
            (
                vec![Effect::AgentInstallRequested {
                    real_index: idx,
                    agent_id,
                }],
                vec![Dirty::Settings],
            )
        }
        SettingsMsg::AgentSetEnabled { agent_id, enabled } => {
            let Some(idx) = selected_real_index(model) else {
                return (vec![], vec![]);
            };
            (
                vec![Effect::AgentSetEnabled {
                    real_index: idx,
                    agent_id,
                    enabled,
                }],
                vec![Dirty::Settings],
            )
        }
    }
}

fn update_skill(model: &mut Model, msg: SkillMsg) -> (Vec<Effect>, Vec<Dirty>) {
    match msg {
        SkillMsg::NewSkillRequested { name, scope } => (
            vec![Effect::CreateSkill {
                name,
                scope,
                active_project_path: model.active_project_path.clone(),
            }],
            vec![Dirty::SkillsListDiff(vec![])],
        ),
        SkillMsg::ContentEdited { path, content } => {
            model.skill_saving = true;
            // Plan phase 27 (skill editing pipeline): the model copy MUST
            // absorb the edit before Dirty::SkillEditor syncs. Without
            // this, sync_skill_editor_state pushed the STALE
            // active_skill_content back into the two-way-bound editor
            // text on every keystroke -- typing never stuck in the
            // editor and saves recorded no lasting delta (the live
            // "user typing doesn't reach the skill section" report).
            model.active_skill_content = content.clone();
            (
                vec![Effect::SkillWrite { path, content }],
                vec![Dirty::SkillEditor],
            )
        }
        SkillMsg::CopyPathRequested { path } => {
            let toast = show_toast(model, "info", "Path copied to clipboard");
            (
                vec![Effect::ClipboardWrite {
                    text: path.to_string_lossy().into_owned(),
                }],
                vec![toast],
            )
        }
        SkillMsg::EditorOpenRequested { path } => (vec![Effect::OpenSkillEditor { path }], vec![]),
        SkillMsg::OpenInEditorRequested { editor_name, path } => {
            (vec![Effect::OpenInEditor { editor_name, path }], vec![])
        }
        SkillMsg::OpenWithOsDefaultRequested { path } => {
            (vec![Effect::OpenWithOsDefault { path }], vec![])
        }
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
            if model.expanded.len() <= index {
                // Grow from row state if needed.
                let n = model
                    .threads
                    .get(real_idx)
                    .and_then(|thread| model.thread_view_models.get(&thread.thread_id))
                    .map(|messages| messages.row_count())
                    .unwrap_or(0);
                model.expanded.resize(n.max(index + 1), false);
            }
            let Some(slot) = model.expanded.get_mut(index) else {
                return (vec![], vec![]);
            };
            *slot = !*slot;
            #[cfg(test)]
            let expanded = *slot;
            let Some(thread) = model.threads.get_mut(real_idx) else {
                return (vec![], vec![]);
            };
            // One-row only: do not re-project the whole transcript. The
            // sync boundary applies the new flag to the retained model.
            let row_exists = model
                .thread_view_models
                .get(&thread.thread_id)
                .is_some_and(|messages| index < messages.row_count());
            #[cfg(test)]
            let row_exists = row_exists || index < thread.message_rows.len();
            if !row_exists {
                return (vec![], vec![]);
            }
            #[cfg(test)]
            if let Some(row) = thread.message_rows.get_mut(index) {
                row.expanded = expanded;
            }
            let thread_id = thread.thread_id.clone();
            (vec![], vec![Dirty::MessageRowPatch { thread_id, index }])
        }
        ChromeMsg::CopyMessageRequested { text } => (vec![Effect::ClipboardWrite { text }], vec![]),
        ChromeMsg::ErrorBannerDismissed => {
            let Some(real_idx) = selected_real_index(model) else {
                return (vec![], vec![]);
            };
            let Some(thread) = model.threads.get_mut(real_idx) else {
                return (vec![], vec![]);
            };
            let thread_id = thread.thread_id.clone();
            thread.error = None;
            (
                vec![],
                vec![
                    thread_row_dirty(model, real_idx),
                    Dirty::Error {
                        thread_id,
                        detail: ErrorDetail {
                            message: String::new(),
                        },
                    },
                ],
            )
        }
        ChromeMsg::CompleteOnboarding => {
            model.onboarding_completed = true;
            if let Err(err) = crate::settings_file::SettingsPaths::from_env().set_onboarding_completed(true) {
                eprintln!("panel-rust: failed to save onboarding_completed: {err}");
            }
            (vec![], vec![Dirty::Settings])
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
        HostMsg::LanguageChanged(language) => {
            model.language = language;
            (vec![], vec![Dirty::Language])
        }
        HostMsg::ProjectPathChanged(path) => {
            model.project_generation = model.project_generation.saturating_add(1);
            model.project_lifecycle_reason = if model.active_project_path.is_some() {
                "switched"
            } else if path.is_some() {
                "opened"
            } else {
                "closed"
            }
            .to_owned();
            model.active_project = path
                .clone()
                .map(crate::model::ProjectIdentity::Saved)
                .unwrap_or_default();
            model.active_project_path = path.clone();
            (
                vec![Effect::SetActiveProjectPath { path }],
                vec![Dirty::ProjectPath, Dirty::SkillsListDiff(vec![])],
            )
        }
        HostMsg::ProjectCreatedUntitled => {
            model.project_generation = model.project_generation.saturating_add(1);
            model.project_lifecycle_reason = "created_untitled".to_owned();
            let id = uuid::Uuid::new_v4().to_string();
            model.active_project = crate::model::ProjectIdentity::Untitled(id);
            model.active_project_path = None;
            (
                vec![Effect::SetActiveProjectPath { path: None }],
                vec![Dirty::ProjectPath, Dirty::SkillsListDiff(vec![])],
            )
        }
        HostMsg::ProjectClosed => {
            model.project_generation = model.project_generation.saturating_add(1);
            model.project_lifecycle_reason = "closed".to_owned();
            model.displayed_thread = None;
            model.active_project = crate::model::ProjectIdentity::None;
            model.active_project_path = None;
            (
                vec![Effect::SetActiveProjectPath { path: None }],
                vec![
                    Dirty::ProjectPath,
                    Dirty::PendingRequest {
                        thread_id: String::new(),
                    },
                    Dirty::Terminal { id: String::new() },
                    Dirty::LocalTerminal,
                    Dirty::SkillsListDiff(vec![]),
                ],
            )
        }
        HostMsg::ProjectPathRenamed { old, new } => {
            model.project_generation = model.project_generation.saturating_add(1);
            model.project_lifecycle_reason = "saved_as".to_owned();
            let old_identity = model.active_project.clone();
            // PISO-7: this is a SEPARATE branch from ProjectPathChanged
            // above, by design -- rebinding on a bare active-path change
            // would be unable to tell "Save-As A -> B" apart from "close
            // A, open B" and would merge two real projects' thread
            // histories. Only this explicit signal, which the host emits
            // exclusively for an actual rename, may issue the rebind
            // effect below.
            let new_path = (!new.is_empty()).then(|| new.clone());
            model.active_project = new_path
                .clone()
                .map(crate::model::ProjectIdentity::Saved)
                .unwrap_or_default();
            model.active_project_path = new_path.clone();
            let mut effects = vec![Effect::SetActiveProjectPath { path: new_path }];
            if !old.is_empty() && !new.is_empty() {
                effects.push(Effect::RenameProjectAssociation {
                    old,
                    new,
                    old_identity,
                });
            } else if old.is_empty() && !new.is_empty() {
                // First Save is a real staging-store migration. The
                // untitled identity is captured before the model changes to
                // Saved(new), so the effect can move the correct UUID store
                // and rebind its previously unscoped thread rows.
                effects.push(Effect::RenameProjectAssociation {
                    old,
                    new,
                    old_identity,
                });
            }
            (
                effects,
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
            // lifetime, not to one hydration result. The inventory lives in
            // Model::persistent_models/restore_persistent_models so new
            // model/key-cache pairs have one preservation list to update.
            let persistent = model.persistent_models();
            let thread_keys = persistent.thread_model_keys.clone();
            let startup_warnings = initial.startup_warnings.clone();
            // InitialState is storage/bridge hydration and intentionally does
            // not own the host lifecycle identity. Preserve the identity that
            // was bound before hydration instead of letting Model::default()
            // erase it during the wholesale reducer replacement.
            let active_project = model.active_project.clone();
            let active_project_path = model.active_project_path.clone();
            *model = Model::from_initial_state(initial);
            model.active_project = active_project;
            model.active_project_path = active_project_path;
            model.restore_persistent_models(persistent);
            let thread_list_dirty = thread_list_dirty_with_keys(model, thread_keys);
            // Cold start: everything is dirty, there is no prior row
            // identity to preserve (see 00-plan.md's known-gap section).
            let mut dirty = vec![
                thread_list_dirty,
                Dirty::Scalar(ScalarField::SelectedThread),
                // The identity is installed before hydration, but the root
                // Slint property is projection-owned. Without this marker a
                // valid initial project remained visually "no project" and
                // disabled the composer/New-thread controls until a later
                // host lifecycle event.
                Dirty::ProjectPath,
            ];
            // Non-fatal cold-start failures (settings load, panel-defaults
            // sync, thread-record restoration, ...) previously only
            // reached eprintln! -- surface them the same way any other
            // Effect failure is surfaced, instead of silently dropping
            // them once hydration itself otherwise succeeds.
            dirty.extend(startup_warnings.into_iter().map(|message| Dirty::Error {
                thread_id: String::new(),
                detail: ErrorDetail { message },
            }));
            (vec![], dirty)
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
            Ok(()) => (vec![], vec![thread_row_dirty(model, real_index)]),
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
        EffectResultMsg::StateEffectFailed { thread_id, message } => (
            vec![],
            vec![Dirty::Error {
                thread_id,
                detail: ErrorDetail { message },
            }],
        ),
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
                    model.rebuild_thread_indices();
                    (
                        vec![Effect::PersistThread { real_index }],
                        vec![thread_row_dirty(model, real_index)],
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
        // Skills list is refreshed by effect_executor before this
        // result is folded (see CreateSkill's refresh-before-open
        // order); do not emit an empty SkillsListDiff here -- that
        // would re-push the pre-create list and race the real rescan.
        EffectResultMsg::SkillCreated(Ok(path)) => {
            let toast = show_toast(model, "status", "Skill created");
            (vec![Effect::OpenSkillEditor { path }], vec![toast])
        }
        EffectResultMsg::SkillWritten(Ok(())) => {
            model.skill_saving = false;
            (vec![], vec![Dirty::SkillEditor])
        }
        EffectResultMsg::SkillPromoted(Ok(())) => {
            let toast = show_toast(model, "status", "Skill promoted to global");
            (vec![], vec![toast])
        }
        EffectResultMsg::ExternalEditorOpened(Ok(()))
        | EffectResultMsg::OsDefaultOpened(Ok(())) => (vec![], vec![]),
        EffectResultMsg::SkillEditorLoaded(Ok(state)) => {
            model.active_skill_name = state.name;
            model.active_skill_path = state.path;
            model.active_skill_md_path = state.content_path;
            model.active_skill_content = state.content;
            model.detected_editors = state.detected_editors;
            model.active_pane = "skill".to_owned();
            (vec![], vec![Dirty::SkillEditor])
        }
        EffectResultMsg::SkillEditorLoaded(Err(err)) => (
            vec![],
            vec![Dirty::Error {
                thread_id: String::new(),
                detail: ErrorDetail {
                    message: err.message,
                },
            }],
        ),
        // markdown-render-cache-layer plan Phase 2. Three independent
        // staleness/existence checks before this delivered render is
        // trusted, matching 00-plan.md's "Correctness rules for
        // delivery" exactly:
        // 1. `thread_id` must still resolve to a live thread (it may
        //    have closed/archived while this render was in flight).
        // 2. `key` must still have an entry in that thread's own
        //    `ThreadMessageIndex` (the message may have been removed --
        //    e.g. history truncation -- since the render was requested).
        // 3. `source_hash` must match what the index currently has
        //    recorded for `key` (the source text changed -- most
        //    commonly, streamed further -- since the render was
        //    requested; applying it would show stale content for newer
        //    text). A late/superseded-epoch result is not specifically
        //    checked here -- letting the worker's own cooperative
        //    cancellation (`EpochCounter`) handle that is an efficiency
        //    concern (don't do wasted parse work), not a correctness one
        //    (this hash check alone is sufficient to never apply stale
        //    content, even from a very late/never-cancelled delivery).
        //
        // Unconditional once validated: this write always lands in the
        // thread's own `message_rows`/`markdown_render_index`, whether
        // or not that thread is currently displayed (see the "which
        // thread is live vs. which thread should render" discussion this
        // phase is built on) -- `Dirty::MessageRowPatch`'s own existing
        // `displayed_thread_for_id` gate (unchanged, already tested) is
        // what decides whether the shared Slint model is actually
        // touched. A background thread's render is never lost, it's just
        // not synced to screen until the user switches to it.
        EffectResultMsg::MarkdownBlocksReady {
            thread_id,
            message_key: key,
            source_hash,
            blocks,
        } => {
            let Some(idx) = model.thread_index_for_id(&thread_id) else {
                crate::trace_host_input(format_args!(
                    "markdown worker dropped thread={thread_id} key={key} reason=stale_thread"
                ));
                return (vec![], vec![]);
            };
            let Some(thread) = model.threads.get_mut(idx) else {
                crate::trace_host_input(format_args!(
                    "markdown worker dropped thread={thread_id} key={key} reason=stale_thread"
                ));
                return (vec![], vec![]);
            };
            let Some(current_hash) = thread.markdown_render_index.borrow().content_hash_for(&key) else {
                crate::trace_host_input(format_args!(
                    "markdown worker dropped thread={thread_id} key={key} reason=stale_key"
                ));
                return (vec![], vec![]);
            };
            if current_hash != source_hash {
                crate::trace_host_input(format_args!(
                    "markdown worker dropped thread={thread_id} key={key} reason=stale_hash"
                ));
                return (vec![], vec![]);
            }
            let Some(row_index) = thread.markdown_render_index.borrow().row_index_for(&key) else {
                crate::trace_host_input(format_args!(
                    "markdown worker dropped thread={thread_id} key={key} reason=stale_key"
                ));
                return (vec![], vec![]);
            };
            let model_rc = crate::models::markdown_block_data_to_model(blocks);
            // Writing straight into `thread.message_rows` here was the old
            // row-cache model; that field is test-only now (see its own
            // doc comment) -- production has no Rust row cache to patch.
            // Writing into `markdown_render_index` alone is sufficient:
            // `sync.rs`'s `projected_thread_rows` rebuilds rows fresh via
            // `models::markdown_blocks_for`, which checks this same cache
            // and returns the value just set here, so the
            // `Dirty::MessageRowPatch` below is what actually triggers
            // that rebuild to pick it up.
            thread
                .markdown_render_index
                .borrow_mut()
                .set_rendered_blocks(&key, model_rc);
            crate::trace_host_input(format_args!(
                "markdown worker delivered thread={thread_id} key={key}"
            ));
            (vec![], vec![Dirty::MessageRowPatch { thread_id, index: row_index }])
        }
        // memory/acpx/gen/plans/acpx-skills/ phase 17: one of the 6
        // reactive-sync trigger call sites (create/promote/edit/agent-
        // enable/agent-disable/thread-start) failed to propagate a
        // skill to an attached agent. Best-effort, matching every
        // reactive-sync call site's own posture: surfaced via toast, not
        // retried, and deliberately NOT a Dirty::Error banner -- the
        // skill mutation itself already succeeded (it's on disk, it's in
        // the UI list), only the downstream agent propagation failed, so
        // this doesn't belong in the same "something is broken" channel
        // as an actual save/create/promote failure above.
        EffectResultMsg::SkillReactiveSyncFailed { operation, detail } => {
            let toast = show_toast(
                model,
                "error",
                format!("Skill sync to agent failed ({operation}): {detail}"),
            );
            (vec![], vec![toast])
        }
        EffectResultMsg::SkillWritten(Err(err)) => {
            model.skill_saving = false;
            let toast = show_toast(
                model,
                "error",
                format!("Skill save failed: {}", err.message),
            );
            (
                vec![],
                vec![
                    Dirty::SkillEditor,
                    toast,
                    Dirty::Error {
                        thread_id: String::new(),
                        detail: ErrorDetail {
                            message: err.message,
                        },
                    },
                ],
            )
        }
        EffectResultMsg::SkillCreated(Err(err))
        | EffectResultMsg::SkillPromoted(Err(err))
        | EffectResultMsg::ExternalEditorOpened(Err(err))
        | EffectResultMsg::OsDefaultOpened(Err(err)) => {
            // Phase 28: these are exactly the "skills top-bar button
            // failures show nothing" class -- global (no-thread) action
            // errors that the per-thread error banner never displayed.
            let toast = show_toast(model, "error", err.message.clone());
            (
                vec![],
                vec![
                    toast,
                    Dirty::Error {
                        thread_id: String::new(),
                        detail: ErrorDetail {
                            message: err.message,
                        },
                    },
                ],
            )
        }
        EffectResultMsg::PromptStreamDelta {
            thread_id,
            message_id,
            delta,
        } => {
            // Stale-target no-op: either the thread was closed/deleted or
            // the message row was removed while the stream was in flight.
            // Resolve both identities before producing a Dirty marker.
            let Some(target_index) = model.thread_index_for_id(&thread_id) else {
                return (vec![], vec![]);
            };
            let Some(thread) = model.threads.get_mut(target_index) else {
                return (vec![], vec![]);
            };
            if !thread.message_ids.iter().any(|id| id == &message_id) {
                return (vec![], vec![]);
            }
            #[cfg(test)]
            if let Some(index) = thread.transcript_keys.iter().position(|key| {
                [
                    format!("assistant:{message_id}"),
                    format!("thought:{message_id}"),
                    format!("user:{message_id}"),
                    format!("tool:{message_id}"),
                ]
                .iter()
                .any(|candidate| candidate == key)
            }) {
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
                    // Attachment/send failure must leave Loading and refresh
                    // the send/stop control. Dirty::Error alone only updates
                    // the banner; without ThreadRow/Connection the chat input
                    // stays stuck on "Stop response" forever (found live in
                    // full GUI matrix: open_session WebSocket failure → send
                    // requested → no prompt → Stop never clears).
                    thread.state = ThreadState::Error;
                    thread.error = Some(err.message.clone());
                    (
                        vec![],
                        vec![
                            Dirty::Error {
                                thread_id: thread.thread_id.clone(),
                                detail: ErrorDetail {
                                    message: err.message,
                                },
                            },
                            Dirty::ThreadRow {
                                thread_id: thread.thread_id.clone(),
                            },
                            Dirty::Connection {
                                thread_id: thread.thread_id.clone(),
                            },
                        ],
                    )
                }
            }
        }
        EffectResultMsg::SettingsSaved(Ok(())) => {
            let toast = show_toast(model, "status", "Settings saved");
            (vec![], vec![Dirty::Settings, toast])
        }
        EffectResultMsg::SettingsSaved(Err(err)) => {
            let toast = show_toast(
                model,
                "error",
                format!("Settings save failed: {}", err.message),
            );
            (
                vec![],
                vec![
                    toast,
                    Dirty::Error {
                        thread_id: String::new(),
                        detail: ErrorDetail {
                            message: err.message,
                        },
                    },
                ],
            )
        }
        EffectResultMsg::McpServerOperationCompleted(Ok(message)) => {
            let toast = show_toast(model, "status", message);
            (vec![], vec![toast])
        }
        EffectResultMsg::McpServerOperationCompleted(Err(err)) => {
            let toast = show_toast(model, "error", err.message);
            (vec![], vec![toast])
        }
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
        EffectResultMsg::DaemonProjectInstancesLoaded(result) => {
            // Best-effort background poll (PISO-8) -- a miss (daemon not
            // running, `snapshotd` binary missing, malformed output, ...)
            // leaves the previously cached instances in place rather than
            // clearing a real signal or surfacing a toast/error for a
            // background poll the user never triggered and cannot act
            // on. No `Dirty` needed either way: the very next frame's
            // `ThreadListSnapshot` collection already reads `model.
            // live_daemon_projects` fresh and its own row-content diff
            // (`update_frame`'s `changed` check) picks up the change.
            if let Ok(instances) = result {
                model.live_daemon_projects = instances;
            }
            (vec![], vec![])
        }
    }
}

fn update_frame(model: &mut Model, frame: crate::msg::FrameInput) -> (Vec<Effect>, Vec<Dirty>) {
    let mut effects = Vec::new();
    let mut dirty = Vec::new();
    for (event_index, bridge_event) in frame.bridge_events.iter().enumerate() {
        let Some(target_index) = frame
            .bridge_event_thread_ids
            .get(event_index)
            .filter(|thread_id| !thread_id.is_empty())
            .and_then(|thread_id| model.thread_index_for_id(thread_id))
        else {
            // A bridge event without a current durable/session identity is
            // stale or mid-attach. Never guess from its positional slot:
            // applying it to `thread_index` can mutate another conversation
            // after a reorder/close. The next frame will retry once binding
            // hydration makes the identity available.
            continue;
        };
        let target_thread_id = model.threads[target_index].thread_id.clone();
        let Some(thread) = model.threads.get_mut(target_index) else {
            continue;
        };
        match &bridge_event.event {
            crate::protocol_types::AgentEvent::Message(message) => {
                if let Some(message_id) = message.id.as_ref() {
                    if !thread.message_ids.iter().any(|id| id == message_id) {
                        thread.message_ids.push(message_id.clone());
                    }
                }
                // Visible output only: thinking/thought chunks don't
                // count -- the live failure this flag exists for (see
                // `ThreadModel::agent_content_this_turn`'s doc comment)
                // streamed reasoning summaries and then ended with no
                // message or tool call at all.
                // Reconnect/status spam is not a real reply either -- if
                // we counted it, a later hard failure would look like a
                // successful turn and leave Loading/Error handling wrong.
                let is_status_only = matches!(
                    message.kind,
                    crate::protocol_types::MessageKind::Agent
                ) && crate::models::agent_text_skips_markdown(&message.text);
                if matches!(
                    message.kind,
                    crate::protocol_types::MessageKind::Agent
                        | crate::protocol_types::MessageKind::ToolCall
                ) && !is_status_only
                {
                    thread.agent_content_this_turn = true;
                }
                thread.last_activity_time = Some(std::time::Instant::now());
                // Hard transport failures often arrive as ordinary agent
                // *message* text (not AgentEvent::Error) after a reconnect
                // storm -- e.g. "unexpected status 502 Bad Gateway". Without
                // this, thread.state stays Loading until TurnEnded, the send
                // button stays in stop/spinner mode, and the UI feels frozen
                // even though the event loop is still running.
                if matches!(message.kind, crate::protocol_types::MessageKind::Agent)
                    && crate::models::agent_text_is_hard_failure(&message.text)
                    && matches!(thread.state, ThreadState::Loading | ThreadState::Cancelling)
                {
                    let failure = message.text.trim().to_owned();
                    thread.state = ThreadState::Error;
                    thread.error = Some(failure.clone());
                    dirty.push(Dirty::Error {
                        thread_id: thread.thread_id.clone(),
                        detail: ErrorDetail {
                            message: failure,
                        },
                    });
                    dirty.push(Dirty::ThreadRow {
                        thread_id: thread.thread_id.clone(),
                    });
                }
                // One MessageAppended per thread per frame is enough: a
                // reconnect storm can emit many AgentEvent::Message ticks
                // before poll drains them; re-diffing the full message
                // model for each one freezes the UI thread.
                let thread_id = thread.thread_id.clone();
                let already = dirty.iter().any(|d| {
                    matches!(
                        d,
                        Dirty::MessageAppended { thread_id: id } if id == &thread_id
                    )
                });
                if !already {
                    dirty.push(Dirty::MessageAppended { thread_id });
                }
            }
            crate::protocol_types::AgentEvent::TurnEnded(reason) => {
                // Captured BEFORE the Idle reset below: only a turn this
                // session itself was generating on (Loading, and a
                // cancel is the user's own doing) qualifies for the
                // empty-turn notice -- a TurnEnded relayed while already
                // Idle (e.g. replay after a reconnect) must not
                // fabricate a notice about a turn we never watched
                // start.
                let was_generating = matches!(thread.state, ThreadState::Loading);
                thread.state = ThreadState::Idle;
                thread.error = None;
                // PROF-8: a turn that actually completed is proof the
                // agent is authenticated now (retried successfully, or an
                // operator fixed the profile) -- clear the persistent
                // banner rather than requiring manual dismissal.
                thread.unauthenticated = false;
                thread.last_activity_time = Some(std::time::Instant::now());
                crate::trace_host_input(format_args!(
                    "turn ended thread={} reason={:?}",
                    bridge_event.thread_index, reason
                ));
                // A turn that ends without ANY visible agent output is
                // indistinguishable from a hang in the UI -- surface it
                // explicitly (error card; state stays Idle so the user
                // can just re-send).
                if was_generating && !thread.agent_content_this_turn {
                    let message = format!(
                        "Agent ended its turn without a response (stopReason: {reason}). \
                         Check gateway-{}.stderr.log in the chat cache directory for \
                         backend diagnostics.",
                        thread.provider
                    );
                    thread.error = Some(message.clone());
                    dirty.push(Dirty::Error {
                        thread_id: thread.thread_id.clone(),
                        detail: ErrorDetail { message },
                    });
                }
                thread.agent_content_this_turn = false;
                if let Some(entry) = thread
                    .send_queue
                    .on_generation_stopped(false)
                    .ok()
                    .flatten()
                {
                    thread.state = ThreadState::Loading;
                    effects.push(Effect::SendPrompt {
                        thread_id: thread.thread_id.clone(),
                        text: entry.text,
                    });
                }
                dirty.push(Dirty::ThreadRow {
                    thread_id: thread.thread_id.clone(),
                });
            }
            crate::protocol_types::AgentEvent::Error(error) => {
                thread.state = ThreadState::Error;
                thread.error = Some(error.clone());
                // PROF-8: same event, a second real per-thread signal --
                // see `models::is_backend_requires_authentication_error`'s
                // doc comment for why this is a substring match and what
                // guards it.
                thread.unauthenticated =
                    crate::models::is_backend_requires_authentication_error(error);
                dirty.push(Dirty::Error {
                    thread_id: thread.thread_id.clone(),
                    detail: ErrorDetail {
                        message: error.clone(),
                    },
                });
                // Mirror PromptSent Err: leave Loading/Cancelling in the
                // visible send/stop control, not only the error banner.
                dirty.push(Dirty::ThreadRow {
                    thread_id: thread.thread_id.clone(),
                });
                dirty.push(Dirty::Connection {
                    thread_id: thread.thread_id.clone(),
                });
            }
            crate::protocol_types::AgentEvent::UsageUpdate { .. } => {
                // Live usage flows through the per-frame runtime
                // snapshot fold below (thread.usage) -- nothing to do
                // per-event beyond letting the frame refresh.
            }
            crate::protocol_types::AgentEvent::PermissionRequest(_)
            | crate::protocol_types::AgentEvent::TerminalOutput(_)
            | crate::protocol_types::AgentEvent::TerminalCreated(_)
            | crate::protocol_types::AgentEvent::SessionModes(_)
            | crate::protocol_types::AgentEvent::CurrentModeChanged(_)
            // PUI-003: the agent's slash commands flow through the per-frame
            // snapshot fold (thread.available_commands) like other caps.
            | crate::protocol_types::AgentEvent::AvailableCommands(_)
            // PROF-11: the agent's plan/todo list and any live session
            // title flow through the same per-frame snapshot fold
            // (thread.plan / thread.session_title).
            | crate::protocol_types::AgentEvent::PlanUpdate(_)
            | crate::protocol_types::AgentEvent::SessionInfoUpdate { .. } => {
                dirty.push(thread_row_dirty(model, target_index));
            }
            crate::protocol_types::AgentEvent::ConfigOptions(_) => {
                dirty.push(Dirty::Capabilities {
                    thread_id: target_thread_id,
                });
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
    if frame.daemon_projects_refresh_due {
        // PISO-8 (project-isolation-mlt-binding plan): a real subprocess
        // spawn + Unix socket dial, executed off the UI thread by
        // `effect_executor.rs` -- see `Effect::
        // RefreshDaemonProjectInstances`'s own doc comment.
        effects.push(Effect::RefreshDaemonProjectInstances);
    }
    if frame.prepend_expanded_rows > 0 {
        let mut expanded = vec![false; frame.prepend_expanded_rows];
        expanded.append(&mut model.expanded);
        model.expanded = expanded;
    }
    if frame.clear_selected_thread {
        model.displayed_thread = None;
    }
    if let Some(snapshot) = frame.thread_list_snapshot {
        let old_keys = model.thread_model_keys.borrow().clone();
        let changed = old_keys != snapshot.visible_thread_ids || model.thread_rows != snapshot.rows;
        for row in &snapshot.rows {
            if let Some(thread) = model.threads.get_mut(row.real_index) {
                thread.thread_id = row.thread_id.clone();
                // Review-gate fix (phase 32): fold a background-attached
                // session binding into the model. add_thread attaches in
                // the background (phase 30), so no SessionAttached fold
                // ever delivers the session id for `+`-created threads;
                // without this the profile picker never locks and
                // Effect::PersistThread never fires.
                if thread.session_id.is_none() {
                    if let Some(session_id) = row.session_id.clone() {
                        thread.session_id = Some(session_id);
                        // PROF-7: same transition, real per-thread state
                        // (not a render-time heuristic) -- row.agent_detected
                        // is only ever Some(..) on exactly this fold (see
                        // external_snapshot's own collection condition), so
                        // this can't re-fire or clobber a later legitimate
                        // state change.
                        if row.agent_detected == Some(false) {
                            thread.state = ThreadState::Stale;
                        }
                        effects.push(Effect::PersistThread {
                            real_index: row.real_index,
                        });
                        dirty.push(thread_row_dirty(model, row.real_index));
                        dirty.push(Dirty::Capabilities {
                            thread_id: row.thread_id.clone(),
                        });
                    }
                }
            }
        }
        // Thread ids/session ids may have been hydrated by the bridge row;
        // publish those identity changes to the reverse lookup maps before
        // the next notification or snapshot is routed.
        model.rebuild_thread_indices();
        // Review-gate fix (phase 32): hydrate bridge-persisted archived
        // flags (restarts previously left every ThreadModel::archived
        // false -- wrong sidebar counters, unenforced pool cap).
        for (idx, archived) in snapshot.archived_flags.iter().enumerate() {
            if let Some(thread) = model.threads.get_mut(idx) {
                thread.archived = *archived;
            }
        }
        model.visible_list_synced = true;
        // PISO-2 (project-isolation-mlt-binding plan) stale-async guard:
        // this snapshot was collected against `snapshot.active_project_
        // path`, tagged at collection time (see `ThreadListSnapshot::
        // active_project_path`'s doc comment). `HostMsg::ProjectPathChanged`
        // updates `model.active_project_path` synchronously, a full
        // reducer turn before any poll tick can re-collect a snapshot
        // against the new value -- so a snapshot whose tag disagrees with
        // the model's CURRENT value describes a project the user has
        // already left. Applying its visible-list SHAPE would show that
        // old project's threads next to the new project's indicator, the
        // exact cross-project leak this plan exists to close. Drop it and
        // wait for the next tick's snapshot instead of assuming this one
        // "usually" arrives in order. The per-row hydration above
        // (session id, archived flags) is thread-scoped, not
        // project-scoped, so it stays safe to apply unconditionally.
        let snapshot_matches_active_project =
            snapshot.active_project_path == model.active_project_path;
        if changed && snapshot_matches_active_project {
            // `selected_thread` is a *filtered* index into the visible
            // list, so before the visible order is rewritten (recency
            // resort on background activity, archive/resume moving rows
            // between sections, a new thread landing at the top), pin the
            // thread the user is actually on by durable id and re-anchor
            // the index afterwards. Without this the stale filtered index
            // silently retargets whichever thread now occupies that slot,
            // and the next frame snapshot renders *that* thread's
            // transcript -- the live "switching threads shows another
            // thread's messages" leak (plan phase 23).
            let selected_id = old_keys.get(model.selected_thread).cloned();
            model.visible_indices = snapshot.visible_indices.clone();
            model.thread_rows = snapshot.rows.clone();
            dirty.push(Dirty::ThreadListDiff(crate::dirty::diff_by_id(
                &old_keys,
                &snapshot.visible_thread_ids,
                &snapshot.rows,
            )));
            // PISO-2: the first snapshot folded in after a project switch
            // deliberately starts the user at that project's FIRST
            // thread, rather than clamping to whichever numeric filtered
            // index the OLD, unrelated project's selection happened to
            // sit at (that clamp is still the right fallback for a
            // same-project list change -- deletion/archive/resort -- so
            // it stays the default; only a genuine project switch
            // overrides it). See `Model::synced_project_path`'s doc
            // comment. An empty new list falls through to the
            // display-clearing arm below regardless of this choice.
            let project_switched = model.synced_project_path != snapshot.active_project_path;
            let reanchored = selected_id
                .and_then(|id| {
                    snapshot
                        .visible_thread_ids
                        .iter()
                        .position(|key| *key == id)
                })
                .unwrap_or_else(|| {
                    if project_switched {
                        0
                    } else {
                        model
                            .selected_thread
                            .min(snapshot.visible_thread_ids.len().saturating_sub(1))
                    }
                });
            if reanchored != model.selected_thread {
                model.selected_thread = reanchored;
                dirty.push(Dirty::Scalar(ScalarField::SelectedThread));
            }
            // Review-gate fix (phase 32): a project switch can scope the
            // visible list down to NOTHING. Without clearing the display,
            // the previous project's transcript kept rendering next to an
            // empty sidebar (and index fallbacks retargeted hidden
            // threads). Same convergence the clear_selected_thread arm
            // uses: drop displayed_thread and diff the shared model to
            // empty.
            if snapshot.visible_thread_ids.is_empty() {
                model.displayed_thread = None;
            }
        }
        if snapshot_matches_active_project {
            model.synced_project_path = snapshot.active_project_path.clone();
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
    if model.agent_operations_in_flight != frame.agent_operations_in_flight {
        model.agent_operations_in_flight = frame.agent_operations_in_flight;
        dirty.push(Dirty::Settings);
    }
    if model.mcp_operations_in_flight != frame.mcp_operations_in_flight {
        model.mcp_operations_in_flight = frame.mcp_operations_in_flight;
        dirty.push(Dirty::Settings);
    }
    if model.recover_session_operations_in_flight != frame.recover_session_operations_in_flight {
        model.recover_session_operations_in_flight = frame.recover_session_operations_in_flight;
        dirty.push(Dirty::Settings);
    }
    if let Some(snapshot) = frame.settings_preferences_snapshot {
        let changed = model.settings_scope != snapshot.scope
            || model.default_profile != snapshot.default_profile
            || model.permission_profile != snapshot.permission_profile
            || model.background_default != snapshot.background_default
            || model.default_agent_id != snapshot.default_agent_id
            || model.dev_mode != snapshot.dev_mode
            || model.background_override_set != snapshot.background_override_set
            || model.background_override != snapshot.background_override
            || model.show_global_skills != snapshot.show_global_skills;
        if changed {
            model.settings_scope = snapshot.scope;
            model.default_profile = snapshot.default_profile;
            model.permission_profile = snapshot.permission_profile;
            model.background_default = snapshot.background_default;
            model.default_agent_id = snapshot.default_agent_id;
            model.dev_mode = snapshot.dev_mode;
            model.background_override_set = snapshot.background_override_set;
            model.background_override = snapshot.background_override;
            model.show_global_skills = snapshot.show_global_skills;
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
            model.thread_index_for_id(&snapshot.thread_id)
        };
        let Some(target_index) = target_index else {
            // An unknown-identity snapshot can arrive after a deferred or
            // failed session disappears. Never leave the previous thread's
            // rows visible under the new selection; clear the shared list
            // and ownership instead of silently keeping stale content.
            model.displayed_thread = None;
            return (effects, dirty);
        };
        // SCNA-01: distinct from `switched_thread` below -- specifically
        // whether there was a *real* previously-displayed thread to leave.
        // `model.displayed_thread` starts `None` before cold-start
        // hydration's first frame, so that first frame's `switched_thread`
        // is also true; every restored thread starts idle/error-free (see
        // `Model::from_initial_state`'s own doc comment), so the
        // `Dirty::Error` this function pushes below on `switched_thread`
        // always carries an empty message on that first frame -- but
        // still unconditionally overwrites `last-error`, silently wiping
        // any global cold-start warning (InitialState::startup_warnings,
        // routed through `Dirty::Error{thread_id: "", ..}` a moment
        // earlier in the same synchronous panel_rust_create call) before
        // the window is ever shown. Captured before the
        // `selection_matches`-gated switched_thread below, since that
        // gate answers a different question (is this snapshot even for
        // the currently-selected thread) and must not affect this one
        // (was there a real previous thread to leave, regardless of
        // whether *this particular* snapshot ends up promoting a switch).
        let had_prior_displayed_thread = model.displayed_thread.is_some();
        // One-frame stale-collection guard (plan phase 23): the snapshot
        // was collected via `visible_indices[selected_thread]` *before*
        // this same frame's thread-list fold re-anchored the selection, so
        // after a visible-order rewrite it can legitimately describe a
        // thread the user is no longer on. Hydrating that thread's own
        // cache by durable id (below) stays correct either way, but
        // *promoting it to the displayed thread* is the message-leak bug:
        // sync.rs renders whatever `displayed_thread` points at, so only
        // switch the display when the snapshot is for the thread the user
        // actually has selected -- otherwise leave the display alone and
        // let the next tick collect the right thread's snapshot.
        let selection_matches = Some(target_index) == selected_real_index(model);
        // Owner mismatch covers selection that already set displayed_thread
        // but list/owner still needs hydrate from a fresh snapshot.
        let switched_thread = selection_matches && model.displayed_thread != Some(target_index);
        if switched_thread {
            // Rare path: frame promotes display without going through
            // apply_thread_selection_switch (e.g. cold first paint).
            model.displayed_thread = Some(target_index);
        }
        // Build expand vec from existing row flags / model.expanded, not
        // a hard clear on every switch (that dropped A→B→A expand state).
        // Only the row *count* is needed here -- `transcript_row_keys`
        // applies the identical Notice-only filter `to_message_rows_
        // from_transcript` does, without paying for a markdown render
        // that this call site never even uses.
        let transcript_row_count = crate::models::transcript_row_keys(&snapshot.transcript).len();
        if model.expanded.len() < transcript_row_count {
            model.expanded.resize(transcript_row_count, false);
        }
        let expanded = model.expanded.clone();
        if let Some(thread) = model.threads.get_mut(target_index) {
            let thread_id = thread.thread_id.clone();
            let old_keys = thread.transcript_keys.clone();
            // Include send-queue rows (QueuedMessageBar) in the projection.
            let in_flight = matches!(thread.state, ThreadState::Loading | ThreadState::Cancelling);
            let queue_state = (thread.send_queue.clone(), in_flight);
            // The frame poll runs at 60-90fps and collects `snapshot.
            // transcript` every tick regardless of whether it changed (see
            // `ExternalSnapshotSource::collect_thread_snapshot_for`'s doc
            // comment). Rebuilding rows from scratch on every such tick re-
            // clones the whole transcript and re-parses every tool row's
            // `raw_input` JSON (`to_message_rows_from_transcript`'s
            // `serde_json::from_str` call) even when nothing about this
            // thread changed since the last tick. When the transcript AND
            // the queue/in-flight state that also feed the projection are
            // both unchanged since the rows currently installed in
            // `thread_view_models` were built, reuse those rows (cheap
            // `Rc`-backed clones) instead of re-projecting from scratch.
            let rows_unchanged = !switched_thread
                && thread.transcript == snapshot.transcript
                && thread.rows_synced_with.as_ref() == Some(&queue_state);
            let (rows, new_keys) = if rows_unchanged {
                match model.thread_view_models.rows(&thread_id) {
                    Some(cached_rows) if cached_rows.len() == old_keys.len() => {
                        (cached_rows, old_keys.clone())
                    }
                    // Retained model and this thread's own key cache have
                    // desynced (should not happen; defensive only, mirrors
                    // `list_model::reconcile`'s own self-heal). Fall back to
                    // a full rebuild rather than serving a mismatched row
                    // list.
                    _ => crate::models::message_rows_for_thread_with_state(
                        snapshot.transcript.clone(),
                        &expanded,
                        &thread.send_queue,
                        in_flight,
                        &mut *thread.markdown_render_index.borrow_mut(),
                    ),
                }
            } else {
                let old_rows = model
                    .thread_view_models
                    .rows(&thread_id)
                    .unwrap_or_default();
                let (mut rows, new_keys) = crate::models::message_rows_for_thread_with_state(
                    snapshot.transcript.clone(),
                    &expanded,
                    &thread.send_queue,
                    in_flight,
                    &mut *thread.markdown_render_index.borrow_mut(),
                );
                // Preserve expand-by-key across re-project (live poll + switch).
                merge_expanded_by_key(&old_keys, &old_rows, &new_keys, &mut rows);
                (rows, new_keys)
            };
            thread.rows_synced_with = Some(queue_state);
            // `old_keys`/`thread.message_rows` are this thread's *own*
            // previously-cached copy, not what's actually still on screen.
            // A brand new thread's own cache is empty both before and
            // after this snapshot, so without `switched_thread` / owner
            // mismatch here the diff never fires on switch -- the shared
            // `messages_model` then keeps showing whatever the *previously
            // displayed* thread had (the "new chat shows prefill data from
            // another thread" bug). Any actual thread switch must always
            // resync the shared model, regardless of whether this thread's
            // own transcript happened to be unchanged since its last visit.
            // Deliberately not comparing `thread.message_rows != rows` here:
            // `MessageItem.markdown_lines` is a `ModelRc<MarkdownLine>`,
            // whose `PartialEq` (i-slint-core's `model.rs`) compares by
            // `Rc` pointer identity, not content -- `to_message_rows_from_
            // transcript` builds a brand-new `ModelRc` every call, so that
            // comparison was true on *every* poll tick for any thread with
            // an agent message, forcing a full resync at 60-90fps for no
            // real reason. Real content changes are already caught by
            // `thread.transcript != snapshot.transcript` (the raw,
            // ModelRc-free transcript data), and expand/collapse already
            // dispatches its own `Dirty::MessageRowPatch` (see
            // `ChromeMsg::ToggleExpanded`).
            let force_list_install = switched_thread;
            let transcript_changed = force_list_install
                || old_keys != new_keys
                || thread.transcript != snapshot.transcript;
            // Same "own cache vs. what's actually still on screen" gap as
            // `transcript_changed` above applies to every other
            // per-thread view fragment: force a resync on switch even when
            // the target thread's own diff is a no-op.
            let pending_changed =
                force_list_install || thread.pending_request != snapshot.pending_request;
            let terminals_changed = force_list_install
                || thread.terminals != snapshot.terminals
                || thread.expanded_terminal != snapshot.expanded_terminal
                || thread.open_terminals != snapshot.open_terminals;
            let local_terminal_changed =
                force_list_install || thread.local_terminal != snapshot.local_terminal;
            let local_terminal_output_changed =
                thread.local_terminal.screen_text != snapshot.local_terminal.screen_text;
            let connection_changed =
                force_list_install || thread.connection_status != snapshot.connection_status;
            let capabilities_changed = force_list_install
                || thread.session_modes != snapshot.session_modes
                || thread.config_options != snapshot.config_options
                || thread.usage != snapshot.usage
                || thread.plan != snapshot.plan
                || thread.session_title != snapshot.session_title;

            thread.transcript = snapshot.transcript;
            thread.transcript_keys = new_keys.clone();
            thread.message_ids = new_keys
                .iter()
                .filter_map(|key| key.split_once(':').map(|(_, id)| id.to_owned()))
                .collect();
            #[cfg(test)]
            {
                thread.message_rows = rows.clone();
            }
            thread.has_older_messages = snapshot.has_older_messages;
            thread.pending_request = snapshot.pending_request;
            thread.terminals = snapshot.terminals;
            thread.expanded_terminal = snapshot.expanded_terminal;
            thread.open_terminals = snapshot.open_terminals;
            thread.local_terminal = snapshot.local_terminal;
            thread.connection_status = snapshot.connection_status;
            thread.session_modes = snapshot.session_modes;
            let old_config_options = std::mem::take(&mut thread.config_options);
            thread.config_options = merge_config_options_preserving_current_value(
                &old_config_options,
                snapshot.config_options,
            );
            thread.available_commands = snapshot.available_commands;
            thread.usage = snapshot.usage;
            thread.plan = snapshot.plan;
            thread.session_title = snapshot.session_title;

            if transcript_changed {
                // Every transcript change, including cold hydration and a
                // switch to a previously retained view, is a diff against
                // that thread's own model. There is no shared-list install
                // on the selection path anymore.
                dirty.push(Dirty::MessagesDiff {
                    thread_id: thread_id.clone(),
                    ops: crate::dirty::diff_by_id(&old_keys, &thread.transcript_keys, &rows),
                });
            }
            // Keep the legacy scalar projection synchronized for non-message
            // consumers. The retained ChatView model remains the owner of
            // message row identity and presentation state.
            if selection_matches {
                model.expanded = rows.iter().map(|r| r.expanded).collect();
            }
            if pending_changed {
                if thread.pending_request.active {
                    // Coverage-matrix `session/request_permission` host
                    // scenario: the one observable signal that an agent-
                    // initiated request card is now live for this thread, so
                    // a host test can wait for it before clicking the card.
                    // (Restored: the pre-TEA refresh_pending_request_for
                    // emitted this; the TEA cutover dropped it.)
                    crate::trace_host_input(format_args!(
                        "pending request active thread={} method={}",
                        snapshot.real_index, thread.pending_request.method
                    ));
                }
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
            if local_terminal_output_changed {
                // Coverage-matrix "client PTY" host scenario: a real shell's
                // own screen buffer changing (not a UI flag flip) is the one
                // observable signal a genuine PTY is running -- trace a tail
                // preview so a host test can confirm it without a screenshot.
                let screen_text = thread.local_terminal.screen_text.as_str();
                if !screen_text.is_empty() {
                    let tail: String = screen_text
                        .chars()
                        .rev()
                        .take(80)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    let tail = tail.replace('\n', "\\n");
                    crate::trace_host_input(format_args!(
                        "local terminal output thread={} tail={:?}",
                        snapshot.real_index, tail
                    ));
                }
            }
            if connection_changed {
                dirty.push(Dirty::Connection {
                    thread_id: thread_id.clone(),
                });
            }
            if capabilities_changed {
                dirty.push(Dirty::Capabilities { thread_id });
            }
            // See had_prior_displayed_thread's doc comment above: skip on
            // cold start's implicit first display (nothing stale to
            // clear, and thread.error is always None there anyway per
            // Model::from_initial_state), so a global cold-start warning
            // banner set moments earlier survives instead of being wiped.
            if switched_thread && had_prior_displayed_thread {
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
    use std::path::Path;

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
        let mut model = Model {
            threads,
            // Reducer tests that exercise thread creation/send represent an
            // already-open project. Keep the no-project contract covered by
            // dedicated Model::default() tests instead of silently testing
            // the rejected path through this shared fixture.
            active_project: crate::model::ProjectIdentity::Saved(
                "/tmp/update-test-project.mlt".to_owned(),
            ),
            ..Model::default()
        };
        model.rebuild_thread_indices();
        model
    }

    /// Dirty set emitted when selection changes the active retained view.
    fn thread_switch_dirty(target_thread_id: &str) -> Vec<Dirty> {
        vec![
            Dirty::Scalar(ScalarField::SelectedThread),
            Dirty::Scalar(ScalarField::ComposeText),
            Dirty::PendingRequest {
                thread_id: target_thread_id.to_owned(),
            },
            Dirty::Error {
                thread_id: target_thread_id.to_owned(),
                detail: ErrorDetail {
                    message: String::new(),
                },
            },
            Dirty::Terminal { id: String::new() },
            Dirty::LocalTerminal,
            Dirty::Connection {
                thread_id: target_thread_id.to_owned(),
            },
            Dirty::Capabilities {
                thread_id: String::new(),
            },
        ]
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
        assert_eq!(dirty, thread_switch_dirty("thread-1"));
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
        assert_eq!(dirty, thread_switch_dirty("thread-0"));
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
        assert_eq!(dirty, thread_switch_dirty("thread-2"));
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
        assert_eq!(
            model.active_project_path.as_deref(),
            Some("/tmp/project.mlt")
        );
        assert_eq!(
            effects,
            vec![Effect::SetActiveProjectPath {
                path: Some("/tmp/project.mlt".to_owned())
            }]
        );
        assert_eq!(
            dirty,
            vec![Dirty::ProjectPath, Dirty::SkillsListDiff(Vec::new())]
        );
    }

    /// GUI matrix: Open → Save As → switch → close (panel lifecycle identity).
    #[test]
    fn gui_matrix_open_save_as_switch_close_bumps_generation_and_reasons() {
        let mut model = Model::default();
        assert_eq!(model.project_generation, 0);

        let (_, _) = update(
            &mut model,
            Msg::Host(HostMsg::ProjectPathChanged(Some(
                "/work/a/project.mlt".to_owned(),
            ))),
        );
        assert_eq!(model.project_lifecycle_reason, "opened");
        assert_eq!(model.project_generation, 1);
        assert!(matches!(
            model.active_project,
            crate::model::ProjectIdentity::Saved(_)
        ));

        let (effects, _) = update(
            &mut model,
            Msg::Host(HostMsg::ProjectPathRenamed {
                old: "/work/a/project.mlt".to_owned(),
                new: "/work/b/project.mlt".to_owned(),
            }),
        );
        assert_eq!(model.project_lifecycle_reason, "saved_as");
        assert_eq!(model.project_generation, 2);
        assert_eq!(
            model.active_project_path.as_deref(),
            Some("/work/b/project.mlt")
        );
        assert!(effects.iter().any(|e| matches!(
            e,
            Effect::RenameProjectAssociation { old, new, .. }
                if old == "/work/a/project.mlt" && new == "/work/b/project.mlt"
        )));

        let gen_before_switch = model.project_generation;
        let (_, _) = update(
            &mut model,
            Msg::Host(HostMsg::ProjectPathChanged(Some(
                "/work/a/project.mlt".to_owned(),
            ))),
        );
        assert_eq!(model.project_lifecycle_reason, "switched");
        assert_eq!(model.project_generation, gen_before_switch + 1);

        let (_, dirty) = update(&mut model, Msg::Host(HostMsg::ProjectClosed));
        assert_eq!(model.project_lifecycle_reason, "closed");
        assert!(matches!(
            model.active_project,
            crate::model::ProjectIdentity::None
        ));
        assert_eq!(model.active_project_path, None);
        assert!(dirty.iter().any(|d| matches!(d, Dirty::ProjectPath)));
    }

    /// GUI matrix: untitled staging then first Save emits rename association
    /// with empty old path so the staging store can move.
    #[test]
    fn gui_matrix_untitled_then_first_save_moves_staging_store() {
        let mut model = Model::default();
        let (_, _) = update(&mut model, Msg::Host(HostMsg::ProjectCreatedUntitled));
        assert_eq!(model.project_lifecycle_reason, "created_untitled");
        let crate::model::ProjectIdentity::Untitled(staging_id) = model.active_project.clone()
        else {
            panic!("expected Untitled identity after ProjectCreatedUntitled");
        };
        assert!(!staging_id.is_empty());
        assert_eq!(model.active_project_path, None);

        let old_identity = model.active_project.clone();
        let (effects, _) = update(
            &mut model,
            Msg::Host(HostMsg::ProjectPathRenamed {
                old: String::new(),
                new: "/work/saved/first.mlt".to_owned(),
            }),
        );
        assert_eq!(model.project_lifecycle_reason, "saved_as");
        assert_eq!(
            model.active_project_path.as_deref(),
            Some("/work/saved/first.mlt")
        );
        assert!(matches!(
            model.active_project,
            crate::model::ProjectIdentity::Saved(_)
        ));
        assert!(
            effects.iter().any(|e| match e {
                Effect::RenameProjectAssociation {
                    old,
                    new,
                    old_identity: oi,
                } => {
                    old.is_empty() && new == "/work/saved/first.mlt" && oi == &old_identity
                }
                _ => false,
            }),
            "first Save must migrate the untitled staging store: {effects:?}"
        );
        // Store dirs differ: staging UUID vs .snapflow/<stem>
        let staging = crate::project_store::project_store_dir(
            &crate::model::ProjectIdentity::Untitled(staging_id),
            Path::new("/cache"),
        );
        let saved = crate::project_store::project_store_dir(
            &crate::model::ProjectIdentity::Saved("/work/saved/first.mlt".into()),
            Path::new("/cache"),
        );
        assert_ne!(staging, saved);
        assert_eq!(
            saved,
            Some(std::path::PathBuf::from("/work/saved/.snapflow/first"))
        );
    }

    /// GUI matrix: no project open still permits an unscoped deferred thread.
    #[test]
    fn gui_matrix_no_project_allows_deferred_thread_and_send() {
        let mut model = Model::default();
        assert!(matches!(
            model.active_project,
            crate::model::ProjectIdentity::None
        ));
        let (e1, d1) = update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::New)));
        assert!(matches!(e1.as_slice(), [Effect::NewThreadDeferred { .. }]));
        assert!(!d1.is_empty() && model.threads.len() == 1);
        let (e2, d2) = update(
            &mut model,
            Msg::Ui(UiMsg::Compose(ComposeMsg::SendRequested(
                "should-attach".into(),
            ))),
        );
        assert!(!e2.is_empty() || !d2.is_empty());
    }

    /// GUI matrix: A→B switch drops late A thread-list snapshots (view isolation).
    #[test]
    fn gui_matrix_project_a_to_b_drops_stale_a_snapshots() {
        let mut model = model_with_threads(&["a0", "a1", "b0"]);
        model.active_project_path = Some("/work/b/project.mlt".to_owned());
        model.synced_project_path = Some("/work/b/project.mlt".to_owned());
        model.visible_indices = vec![2];
        model.selected_thread = 0;
        *model.thread_model_keys.borrow_mut() = vec!["thread-2".to_owned()];

        let (_, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                thread_list_snapshot: Some(crate::msg::ThreadListSnapshot {
                    visible_indices: vec![0, 1],
                    visible_thread_ids: vec!["thread-0".to_owned(), "thread-1".to_owned()],
                    rows: vec![visible_row(0, "thread-0"), visible_row(1, "thread-1")],
                    archived_flags: vec![],
                    active_project_path: Some("/work/a/project.mlt".to_owned()),
                }),
                ..FrameInput::default()
            }),
        );
        assert_eq!(model.visible_indices, vec![2]);
        assert!(!dirty
            .iter()
            .any(|item| matches!(item, Dirty::ThreadListDiff(_))));
    }

    /// GUI matrix: initial open sets Saved identity before any session attach
    /// effect beyond SetActiveProjectPath (attach gate path).
    #[test]
    fn gui_matrix_initial_project_sets_identity_before_attach() {
        let mut model = Model::default();
        let (effects, _) = update(
            &mut model,
            Msg::Host(HostMsg::ProjectPathChanged(Some(
                "/projects/demo/cut.mlt".to_owned(),
            ))),
        );
        assert!(matches!(
            model.active_project,
            crate::model::ProjectIdentity::Saved(ref p) if p == "/projects/demo/cut.mlt"
        ));
        // Only path-binding effect — no New/Attach effect is emitted here.
        assert_eq!(
            effects,
            vec![Effect::SetActiveProjectPath {
                path: Some("/projects/demo/cut.mlt".to_owned())
            }]
        );
        let store =
            crate::project_store::project_store_dir(&model.active_project, Path::new("/cache"))
                .expect("saved project must have a store dir");
        assert_eq!(
            store,
            std::path::PathBuf::from("/projects/demo/.snapflow/cut")
        );
        assert_ne!(store.as_os_str(), "/projects/demo/cut.mlt");
    }

    #[test]
    fn thread_selected_out_of_range_clamps_to_the_last_thread() {
        // Matches the dispatcher contract: out-of-range selection clamps
        // to the last visible thread rather than becoming a no-op.
        let mut model = model_with_threads(&["a", "b"]);
        let (effects, dirty) = update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::Selected(5))));
        assert_eq!(model.selected_thread, 1);
        assert_eq!(
            effects,
            vec![Effect::PersistSelectedThread {
                thread_id: "thread-1".to_owned()
            }]
        );
        assert_eq!(dirty, thread_switch_dirty("thread-1"));
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
    fn thread_close_requested_emits_one_close_effect() {
        let mut model = model_with_threads(&["a"]);
        let (effects, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Thread(ThreadMsg::CloseRequested(0))),
        );
        assert!(model.threads[0].closed);
        assert_eq!(effects, vec![Effect::CloseThread { real_index: 0 }]);
        assert_eq!(
            dirty,
            vec![Dirty::ThreadRow {
                thread_id: "thread-0".to_owned()
            }]
        );
    }

    /// send_queue.rs's disk persistence (SendQueue::load/send_queue_path)
    /// previously had zero call sites outside its own tests -- every
    /// thread's queue was `SendQueue::default()` (persist_path: None), so
    /// a queued-but-unsent message was silently lost on restart despite
    /// the fully-built persistence layer. This proves the wiring actually
    /// round-trips through a real file, the same way a restart would.
    #[test]
    fn a_new_threads_send_queue_persists_and_reloads_after_a_simulated_restart() {
        let cache_dir = tempfile::tempdir().expect("cache dir");
        let previous = std::env::var("RUI_ACP_CACHE_DIR").ok();
        unsafe {
            std::env::set_var("RUI_ACP_CACHE_DIR", cache_dir.path());
        }

        let mut model = Model::default();
        model.active_project =
            crate::model::ProjectIdentity::Saved("/tmp/send-queue-test-project.mlt".to_owned());
        update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::New)));
        let thread_id = model.threads[0].thread_id.clone();
        model.threads[0]
            .send_queue
            .enqueue("queued across a restart".to_owned(), false)
            .expect("enqueue must persist, not silently no-op");

        // Simulate a restart: load a fresh SendQueue for the same
        // thread_id the same way cold-start hydration does in lib.rs.
        let path = crate::send_queue::send_queue_path(
            &crate::agent_bridge::resolve_cache_dir(),
            &thread_id,
        );
        let reloaded = crate::send_queue::SendQueue::load(path).expect("reload queue from disk");
        assert_eq!(reloaded.len(), 1);
        assert_eq!(
            reloaded.first().map(|entry| entry.text.as_str()),
            Some("queued across a restart")
        );

        match previous {
            Some(value) => unsafe { std::env::set_var("RUI_ACP_CACHE_DIR", value) },
            None => unsafe { std::env::remove_var("RUI_ACP_CACHE_DIR") },
        }
    }

    #[test]
    fn agent_set_enabled_emits_one_admin_effect() {
        // setup-followups plan, agent_settings_ordering_and_install_
        // enable_flow: the real "install > enable" second step's Msg ->
        // Effect mapping.
        let mut model = model_with_threads(&["a"]);
        let (effects, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Settings(SettingsMsg::AgentSetEnabled {
                agent_id: "codex-acp".to_owned(),
                enabled: false,
            })),
        );
        assert_eq!(
            effects,
            vec![Effect::AgentSetEnabled {
                real_index: 0,
                agent_id: "codex-acp".to_owned(),
                enabled: false,
            }]
        );
        assert_eq!(dirty, vec![Dirty::Settings]);
    }

    #[test]
    fn selecting_a_thread_emits_one_persistence_effect() {
        let mut model = model_with_threads(&["a", "b"]);
        let (effects, dirty) = update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::Selected(1))));
        assert_eq!(
            effects,
            vec![Effect::PersistSelectedThread {
                thread_id: "thread-1".to_owned()
            }]
        );
        // Selection change switches the retained view and sibling panes; the
        // target message model is already owned by that thread.
        assert_eq!(dirty, thread_switch_dirty("thread-1"));
    }

    #[test]
    fn reselecting_the_already_displayed_thread_only_dirties_selected() {
        let mut model = model_with_threads(&["a", "b"]);
        model.selected_thread = 1;
        model.displayed_thread = Some(1);
        let (effects, dirty) = update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::Selected(1))));
        assert_eq!(
            effects,
            vec![Effect::PersistSelectedThread {
                thread_id: "thread-1".to_owned()
            }]
        );
        assert_eq!(dirty, vec![Dirty::Scalar(ScalarField::SelectedThread)]);
    }

    #[test]
    fn switching_threads_isolates_per_thread_compose_drafts() {
        // PUI-020 (panel-ui-task-triage): the compose draft is per-thread.
        // Typing in thread A then switching to B must save A's draft and
        // restore B's own (empty) draft -- B must never inherit A's text --
        // and switching back to A restores A's draft intact. (lib.rs syncs
        // the live component text into model.compose_text before dispatching
        // the switch; apply_thread_selection_switch clears displayed_thread,
        // which the next FrameInput re-promotes -- simulated here.)
        let mut model = model_with_threads(&["a", "b"]);
        model.selected_thread = 0;
        model.displayed_thread = Some(0);
        update(
            &mut model,
            Msg::Ui(UiMsg::Compose(ComposeMsg::DraftChanged(
                "draft for A".to_owned(),
            ))),
        );

        update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::Selected(1))));
        assert_eq!(
            model.threads[0].compose_draft, "draft for A",
            "outgoing thread A's draft must be saved"
        );
        assert_eq!(
            model.compose_text, "",
            "thread B must not inherit thread A's draft"
        );

        // Frame promotes B to displayed (real app); then user types into B.
        model.displayed_thread = Some(1);
        update(
            &mut model,
            Msg::Ui(UiMsg::Compose(ComposeMsg::DraftChanged(
                "draft for B".to_owned(),
            ))),
        );
        update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::Selected(0))));
        assert_eq!(
            model.threads[1].compose_draft, "draft for B",
            "outgoing thread B's draft must be saved"
        );
        assert_eq!(
            model.compose_text, "draft for A",
            "returning to thread A restores A's own draft intact"
        );
    }

    /// setup-followups plan, provider_fastmode_profile_persistence: the
    /// compose-bar profile picker is only ever meant to be interactive
    /// (per ThreadItem.has-session) while the selected thread has no
    /// attached session yet -- this proves the reducer itself enforces
    /// that, not just the Slint-side `enabled:` gate (a UI-only lock
    /// would still let a stale/racing dispatch mutate the model).
    #[test]
    fn profile_selected_updates_the_thread_only_while_it_has_no_session() {
        let mut model = model_with_threads(&["a"]);
        model.threads[0].provider = "codex".to_owned();
        let (effects, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Settings(SettingsMsg::ProfileSelected {
                profile_name: "codex-tools".to_owned(),
                agent_id: "codex-acp".to_owned(),
            })),
        );
        assert_eq!(
            model.threads[0].profile_name.as_deref(),
            Some("codex-tools")
        );
        assert_eq!(
            model.threads[0].provider, "codex-acp",
            "Provider picker must update thread.provider (agent id) for deferred attach"
        );
        assert!(
            effects.is_empty(),
            "no backend to notify yet -- nothing to send"
        );
        assert_eq!(
            dirty,
            vec![
                Dirty::ThreadRow {
                    thread_id: "thread-0".to_owned()
                },
                Dirty::Capabilities {
                    thread_id: "thread-0".to_owned()
                }
            ]
        );

        // Once a real session has attached, the same message must be a
        // pure no-op -- ACP has no primitive for moving a live session
        // to a different backend.
        model.threads[0].session_id = Some("real-session-1".to_owned());
        let (effects, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Settings(SettingsMsg::ProfileSelected {
                profile_name: "balanced".to_owned(),
                agent_id: "claude-acp".to_owned(),
            })),
        );
        assert_eq!(
            model.threads[0].profile_name.as_deref(),
            Some("codex-tools"),
            "profile must stay locked once a session has attached"
        );
        assert_eq!(
            model.threads[0].provider, "codex-acp",
            "provider must stay locked once a session has attached"
        );
        assert!(effects.is_empty());
        assert!(dirty.is_empty());
    }

    #[test]
    fn profile_selected_resolves_agent_id_from_catalog_when_ui_omits_it() {
        let mut model = model_with_threads(&["a"]);
        model.threads[0].provider = "stale-default".to_owned();
        model.available_profiles = vec![crate::gateway_actor::ProfileSummary {
            name: "claude-profile".to_owned(),
            agent_id: "claude-acp".to_owned(),
            allow_terminal_access: false,
            allow_fs_access: true,
        }];
        let _ = update(
            &mut model,
            Msg::Ui(UiMsg::Settings(SettingsMsg::ProfileSelected {
                profile_name: "claude-profile".to_owned(),
                agent_id: String::new(),
            })),
        );
        assert_eq!(
            model.threads[0].profile_name.as_deref(),
            Some("claude-profile")
        );
        assert_eq!(
            model.threads[0].provider, "claude-acp",
            "must resolve agent_id from available_profiles when not sent by UI"
        );
    }

    #[test]
    fn new_thread_provider_passes_any_configured_agent_id_through_unchanged() {
        // PROF-1/PROF-2: a real settings.global.json found live this
        // session had default_agent_id: "claude-acp" (the real registry
        // agent id, plausibly from a picker backed by the agent catalog)
        // rather than the short "claude" label this reducer's gateway
        // wiring used to special-case -- an exact-match list only
        // recognizing "claude"/"claude-code" silently routed everything
        // else, including "claude-acp", to codex. The fix is no longer a
        // bigger substring-match list (that still special-cases exactly
        // one family and mis-routes everyone else, e.g. "gemini-acp"
        // would still have landed on codex); `provider` now passes
        // `default_agent_id` through completely unchanged, matching
        // `AgentBridge::resolve_provider_for`'s own pass-through
        // contract, so any agent id -- claude family, gemini, or
        // anything not yet invented -- reaches its own gateway with zero
        // code changes here.
        for agent_id in [
            "claude",
            "claude-code",
            "claude-acp",
            "gemini-acp",
            "Claude-Opus-Next",
        ] {
            let mut model = model_with_threads(&[]);
            model.default_agent_id = agent_id.to_owned();
            let (effects, _) = update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::New)));
            assert!(
                matches!(
                    effects.as_slice(),
                    [Effect::NewThreadDeferred { provider, .. }] if provider == agent_id
                ),
                "default_agent_id {agent_id:?} must pass through unchanged as the provider, \
                 got: {effects:?}"
            );
        }
    }

    #[test]
    fn new_thread_with_no_default_profile_falls_back_to_default_agent_id_as_the_profile() {
        // PROF-2: `Router::ensure_default_profiles_seeded` auto-fills one
        // profile per installed registry agent, named after that agent's
        // own id -- so once a real `default_agent_id` is configured but
        // no profile has been hand-picked, using that same id as
        // `_acpx.profile` resolves without any `profiles/create` setup.
        // Before this, an unset `default_profile` always meant
        // native/unmanaged mode (`profile_name: None`) even when a real
        // default agent WAS configured, silently ignoring it for session
        // binding purposes.
        let mut model = model_with_threads(&[]);
        model.default_agent_id = "codex-acp".to_owned();
        update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::New)));
        assert_eq!(
            model.threads[0].profile_name.as_deref(),
            Some("codex-acp"),
            "with no explicit default_profile, the configured default_agent_id must be used \
             as the profile name"
        );
    }

    #[test]
    fn new_thread_with_neither_default_profile_nor_default_agent_id_stays_unprofiled() {
        // The genuine "nothing configured at all" case: no known agent id
        // to request a profile for, so the session must still open
        // native/unmanaged (profile_name stays None) rather than guessing
        // -- passing the bare gateway-routing fallback label
        // (`NO_PROVIDER_REQUESTED_FALLBACK`, not a real registry agent
        // id) as `_acpx.profile` would make session/new fail outright
        // instead of degrading gracefully.
        let mut model = model_with_threads(&[]);
        update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::New)));
        assert_eq!(model.threads[0].profile_name, None);
    }

    #[test]
    fn new_thread_is_pending_until_attach_result_resolves_its_binding() {
        let mut model = model_with_threads(&["existing"]);
        model.default_profile = "safe".to_owned();
        model.permission_profile = "workspace".to_owned();
        model.default_agent_id = "claude".to_owned();

        let (effects, _) = update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::New)));
        // PUI-014: New now creates a DEFERRED thread -- no eager attach, so the
        // provider/profile stay editable. The effect carries only what the
        // deferred slot needs; the profile/permission are retained on the model
        // thread and read at first-send attach time.
        assert_eq!(
            effects,
            vec![Effect::NewThreadDeferred {
                real_index: 1,
                display_name: "New thread 2".to_owned(),
                provider: "claude".to_owned(),
            }]
        );
        assert_eq!(model.threads.len(), 2);
        assert!(model.threads[1].session_id.is_none());
        assert_eq!(model.threads[1].profile_name.as_deref(), Some("safe"));
        assert_eq!(
            model.threads[1].permission_profile.as_deref(),
            Some("workspace")
        );

        // The first message's imperative attach eventually resolves the binding
        // exactly as before -- SessionAttached still folds the session id and
        // emits the persistence effect.
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
        assert_eq!(
            dirty,
            vec![Dirty::ThreadRow {
                thread_id: "durable-new".to_owned()
            }]
        );
    }

    // -- markdown-render-cache-layer plan Phase 2: EffectResultMsg::
    // MarkdownBlocksReady --

    fn sample_markdown_blocks() -> Vec<crate::models::MarkdownBlockData> {
        crate::models::build_markdown_block_data("Hello **world**.", false)
    }

    #[test]
    fn markdown_blocks_ready_patches_the_thread_cache_and_row_when_valid() {
        let mut model = model_with_threads(&["a"]);
        let key = "assistant:m1".to_owned();
        let text = "Hello **world**.";
        model.threads[0]
            .markdown_render_index
            .borrow_mut()
            .record(&key, 0, text);
        model.threads[0].message_rows = vec![crate::MessageItem {
            kind: "agent".into(),
            text: text.into(),
            ..crate::MessageItem::default()
        }];
        model.displayed_thread = Some(0);

        let (effects, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::MarkdownBlocksReady {
                thread_id: "thread-0".to_owned(),
                message_key: key.clone(),
                source_hash: crate::thread_message_index::hash_content(text),
                blocks: sample_markdown_blocks(),
            }),
        );

        assert!(effects.is_empty());
        assert_eq!(
            dirty,
            vec![Dirty::MessageRowPatch {
                thread_id: "thread-0".to_owned(),
                index: 0,
            }]
        );
        assert!(model.threads[0]
            .markdown_render_index
            .borrow()
            .rendered_blocks_for(&key)
            .is_some());
        // `message_rows` is test-only now (main's retained-per-thread-
        // ChatView architecture); see the sibling background-thread test
        // above for why the cache assertion is the meaningful check.
    }

    #[test]
    fn markdown_blocks_ready_for_unknown_thread_id_is_dropped_silently() {
        let mut model = model_with_threads(&["a"]);
        let (effects, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::MarkdownBlocksReady {
                thread_id: "no-such-thread".to_owned(),
                message_key: "assistant:m1".to_owned(),
                source_hash: 0,
                blocks: sample_markdown_blocks(),
            }),
        );
        assert!(effects.is_empty());
        assert!(dirty.is_empty());
    }

    #[test]
    fn markdown_blocks_ready_for_unknown_key_is_dropped_silently() {
        // The message this render targets no longer exists in the
        // thread's index (e.g. history truncation/removal since the
        // render was requested) -- never recorded, so content_hash_for
        // returns None and the whole delivery is dropped.
        let mut model = model_with_threads(&["a"]);
        let (effects, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::MarkdownBlocksReady {
                thread_id: "thread-0".to_owned(),
                message_key: "assistant:never-recorded".to_owned(),
                source_hash: 12345,
                blocks: sample_markdown_blocks(),
            }),
        );
        assert!(effects.is_empty());
        assert!(dirty.is_empty());
        assert!(model.threads[0]
            .markdown_render_index
            .borrow()
            .rendered_blocks_for("assistant:never-recorded")
            .is_none());
    }

    #[test]
    fn markdown_blocks_ready_with_stale_source_hash_is_dropped_not_applied() {
        // The source text changed (most commonly: streamed further)
        // since this render was requested -- applying it would show
        // stale content for newer text. Must not patch the row or
        // overwrite the cache with the stale render.
        let mut model = model_with_threads(&["a"]);
        let key = "assistant:m1".to_owned();
        model.threads[0]
            .markdown_render_index
            .borrow_mut()
            .record(&key, 0, "the text has since grown longer");
        model.threads[0].message_rows = vec![crate::MessageItem {
            kind: "agent".into(),
            text: "the text has since grown longer".into(),
            ..crate::MessageItem::default()
        }];
        model.displayed_thread = Some(0);

        let (effects, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::MarkdownBlocksReady {
                thread_id: "thread-0".to_owned(),
                message_key: key.clone(),
                // Hash of the OLD, shorter text this render was actually
                // requested for -- deliberately stale against the index.
                source_hash: crate::thread_message_index::hash_content("the text"),
                blocks: sample_markdown_blocks(),
            }),
        );

        assert!(effects.is_empty());
        assert!(dirty.is_empty(), "a stale-hash delivery must never patch anything");
        assert!(
            model.threads[0]
                .markdown_render_index
                .borrow()
                .rendered_blocks_for(&key)
                .is_none(),
            "the stale render must not be cached either"
        );
        // `message_rows` is test-only now; see the earlier tests' own
        // note on why the cache assertion above is the meaningful check.
    }

    #[test]
    fn markdown_blocks_ready_updates_a_background_non_displayed_thread_cache() {
        // The core guarantee this phase exists for: a render for a
        // thread the user is NOT currently looking at still lands in
        // that thread's own cache/row (ready for an instant switch
        // later) -- it just doesn't need to (and, via Dirty::
        // MessageRowPatch's own existing displayed_thread_for_id gate,
        // won't) touch the shared on-screen model.
        let mut model = model_with_threads(&["a", "b"]);
        let key = "assistant:m1".to_owned();
        let text = "Background thread content.";
        model.threads[1]
            .markdown_render_index
            .borrow_mut()
            .record(&key, 0, text);
        model.threads[1].message_rows = vec![crate::MessageItem {
            kind: "agent".into(),
            text: text.into(),
            ..crate::MessageItem::default()
        }];
        // Thread 0 (not thread 1) is the one currently displayed.
        model.displayed_thread = Some(0);

        let (effects, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::MarkdownBlocksReady {
                thread_id: "thread-1".to_owned(),
                message_key: key.clone(),
                source_hash: crate::thread_message_index::hash_content(text),
                blocks: sample_markdown_blocks(),
            }),
        );

        assert!(effects.is_empty());
        // The reducer still emits the patch (sync.rs's own existing gate
        // decides visibility at apply time, not the reducer) -- what
        // matters here is the underlying cache/row actually updated.
        assert_eq!(
            dirty,
            vec![Dirty::MessageRowPatch {
                thread_id: "thread-1".to_owned(),
                index: 0,
            }]
        );
        assert!(model.threads[1]
            .markdown_render_index
            .borrow()
            .rendered_blocks_for(&key)
            .is_some());
        // `message_rows` is test-only now (main's retained-per-thread-
        // ChatView architecture -- see its own doc comment); production
        // never writes it directly. `sync.rs`'s `projected_thread_rows`
        // rebuilds rows fresh from `markdown_render_index` on the next
        // pass, which is what the cache assertion above already proves
        // is ready to be picked up -- there's no separate row-patch write
        // left to check here.
    }

    #[test]
    fn new_thread_never_forwards_the_literal_default_profile_sentinel() {
        // Regression test: "agent default is in crash backoff". The
        // literal string "default" is a reserved acpx-server sentinel
        // (see acpxmgr.go's WriteConfig doc comment: the
        // "snapflow-mcp-attach" profile's own agent_id is deliberately
        // the placeholder "default", which no real backend is ever
        // registered under). A settings form re-saved without ever
        // touching the profile dropdown could land that literal string in
        // `model.default_profile`/`permission_profile`; forwarding it as
        // `_acpx.profile` on `session/new` makes acpx-server try to dial a
        // nonexistent "default" agent, fail, and crash-loop forever.
        let mut model = model_with_threads(&["existing"]);
        model.default_profile = "default".to_owned();
        model.permission_profile = "default".to_owned();
        model.default_agent_id = "codex".to_owned();

        let (effects, _) = update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::New)));

        assert_eq!(
            effects,
            vec![Effect::NewThreadDeferred {
                real_index: 1,
                display_name: "New thread 2".to_owned(),
                provider: "codex".to_owned(),
            }],
        );
        // PUI-014: the profile/permission are now read from the model thread at
        // attach time, so the "default" sentinel must be filtered out BEFORE
        // it is stored -- otherwise it would reach session/new at first send.
        // PROF-2: the sentinel being filtered doesn't mean `profile_name`
        // stays `None` here -- `default_agent_id` ("codex", a real,
        // non-sentinel value) is the documented fallback once the
        // explicit `default_profile` is filtered out, so the thread still
        // gets a usable profile binding instead of silently falling back
        // to native/unmanaged mode.
        assert_eq!(
            model.threads[1].profile_name.as_deref(),
            Some("codex"),
            "the literal \"default\" sentinel must never be stored, but default_agent_id is a \
             real fallback and must still be used"
        );
        assert_eq!(
            model.threads[1].permission_profile, None,
            "a literal \"default\" permission-profile must never be stored"
        );
    }

    #[test]
    fn no_project_allows_deferred_new_send_and_ignores_late_attach_results() {
        let mut model = Model::default();

        let (effects, dirty) = update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::New)));
        assert!(matches!(
            effects.as_slice(),
            [Effect::NewThreadDeferred { .. }]
        ));
        assert!(!dirty.is_empty());
        assert_eq!(model.threads.len(), 1);

        let (effects, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Compose(ComposeMsg::SendRequested(
                "blocked".to_owned(),
            ))),
        );
        assert!(matches!(effects.as_slice(), [Effect::SendPrompt { .. }]));
        assert!(!dirty.is_empty());

        let (effects, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Thread(ThreadMsg::NewResolved {
                display_name: "late".to_owned(),
                provider: "codex-acp".to_owned(),
                profile_name: None,
                permission_profile: None,
                session_id: Some("late-session".to_owned()),
                thread_id: Some("late-thread".to_owned()),
            })),
        );
        assert!(effects.is_empty());
        assert!(dirty.is_empty());
        assert_eq!(model.threads.len(), 1);
    }

    #[test]
    fn settings_save_never_persists_the_literal_default_profile_sentinel() {
        let mut model = model_with_threads(&["existing"]);
        let input = crate::msg::SettingsSaveInput {
            scope: "global".to_owned(),
            default_profile: "default".to_owned(),
            permission_profile: "default".to_owned(),
            background_default: false,
            default_agent_id: "codex".to_owned(),
            selected_thread_id: None,
            background_override_set: false,
            background_override: false,
            show_global_skills: true,
        };

        update(
            &mut model,
            Msg::Ui(UiMsg::Settings(SettingsMsg::Save(input))),
        );

        assert_eq!(model.default_profile, "");
        assert_eq!(model.permission_profile, "");
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
        let (effects, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Compose(ComposeMsg::SendRequested("hi".to_owned()))),
        );
        assert_eq!(model.threads[0].state, ThreadState::Loading);
        assert_eq!(
            effects,
            vec![Effect::SendPrompt {
                thread_id: "thread-0".to_owned(),
                text: "hi".to_owned()
            }]
        );
        // Regression test: "loading should start immediately on send".
        // Without `Dirty::ThreadRow(0)` here, `model.threads[0].state`
        // flips to `Loading` above, but nothing tells the sidebar's
        // `thread_model` (which the sidebar spinner and the chat area's
        // `sending`-derived live-tail pulse both read from) to actually
        // re-render that row -- it only caught up whenever some
        // unrelated event later forced a full thread-list rebuild.
        assert!(
            dirty.contains(&Dirty::ThreadRow {
                thread_id: "thread-0".to_owned()
            }),
            "sending a message must immediately dirty this thread's row so the loading \
             spinner/pulse starts right away, got: {dirty:?}"
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
                thread_id: "thread-2".to_owned(),
                text: "hi".to_owned(),
            }]
        );
        assert_eq!(model.threads[2].state, ThreadState::Loading);
        assert_eq!(model.threads[1].state, ThreadState::Idle);
    }

    /// Live-reproduced crash, root-caused and fixed here: typing a
    /// thread-search query that matches nothing filters the currently
    /// selected thread out of the visible list (`visible_indices` becomes
    /// genuinely empty, not just "not yet synced" -- see
    /// `current_visible_indices`'s own fallback condition). Before this
    /// fix, `selected_real_index` papered over that with
    /// `.unwrap_or(model.selected_thread)`, treating the *filtered-list
    /// position* number as if it were already a *real storage index* --
    /// which happened to still be in-bounds here (`threads.len() == 1`),
    /// so `update_compose` built a `SendPrompt { real_index: 0, .. }`
    /// effect anyway. Meanwhile `dispatch.rs`'s `dispatch_compose_send`
    /// independently computes the caller's own real index via
    /// `panel.real_index(filtered_idx)`, which reads raw
    /// `visible_indices` with no such fallback and correctly got `None`
    /// for the same empty list. The two disagreed, and
    /// `dispatch_compose_send`'s `debug_assert!("send effect must target
    /// the selected filtered index")` aborted the whole process --
    /// Rust panics can't unwind across the Slint/Qt host boundary here,
    /// so a hard `abort()`, not a graceful error. This is the actual
    /// unit-level proof that Send is now a clean no-op instead, whatever
    /// the caller-side index bookkeeping does.
    #[test]
    fn compose_send_requested_is_a_no_op_when_the_search_filter_hides_the_selection() {
        let mut model = model_with_threads(&["only thread"]);
        model.visible_list_synced = true;
        model.visible_indices = vec![]; // search query matched nothing
        model.selected_thread = 0;
        let (effects, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Compose(ComposeMsg::SendRequested("hi".to_owned()))),
        );
        assert_eq!(effects, vec![], "no thread is genuinely selected/visible right now");
        assert_eq!(dirty, vec![]);
        assert_eq!(
            model.threads[0].state,
            ThreadState::Idle,
            "must not silently start a turn on a thread that isn't the visible selection"
        );
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
                bridge_event_thread_ids: vec!["thread-1".to_owned()],
                ..FrameInput::default()
            }),
        );
        assert_eq!(
            effects,
            vec![Effect::SendPrompt {
                thread_id: "thread-0".to_owned(),
                text: "queued".to_owned(),
            }]
        );
        assert_eq!(model.threads[0].state, ThreadState::Loading);
        assert!(dirty.contains(&Dirty::ThreadRow {
            thread_id: "thread-0".to_owned()
        }));
    }

    #[test]
    fn empty_turn_while_generating_surfaces_an_explicit_notice() {
        // The live failure this guards (2026-07-23): a provider-side
        // tool_search bug ended every MCP-needing codex turn after only
        // reasoning -- no message, no tool call -- and the UI showed
        // nothing, indistinguishable from a hang.
        let mut model = model_with_threads(&["a"]);
        model.threads[0].state = ThreadState::Loading;
        model.threads[0].agent_content_this_turn = false;
        let (_effects, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                bridge_events: vec![crate::agent_bridge::BridgeEvent {
                    thread_index: 0,
                    event: crate::protocol_types::AgentEvent::TurnEnded("end_turn".to_owned()),
                }],
                bridge_event_thread_ids: vec!["thread-0".to_owned()],
                ..FrameInput::default()
            }),
        );
        // State stays Idle (user can just re-send), but the empty turn
        // is called out via the error surface.
        assert_eq!(model.threads[0].state, ThreadState::Idle);
        let error = model.threads[0]
            .error
            .as_deref()
            .expect("empty-turn notice set");
        assert!(error.contains("without a response"), "got: {error}");
        assert!(
            dirty.iter().any(|d| matches!(d, Dirty::Error { .. })),
            "expected a Dirty::Error for the notice"
        );
    }

    #[test]
    fn queue_cancel_removes_entry_and_rebuilds_message_rows() {
        let mut model = model_with_threads(&["a"]);
        model.threads[0].state = ThreadState::Loading;
        model.threads[0]
            .send_queue
            .enqueue("stay".to_owned(), false)
            .expect("queue");
        model.threads[0]
            .send_queue
            .enqueue("drop-me".to_owned(), false)
            .expect("queue");
        // Project once so transcript_keys include queue:{id} rows.
        let expanded = model.expanded.clone();
        let thread = &mut model.threads[0];
        let (rows, keys) = crate::models::message_rows_for_thread_with_state(
            thread.transcript.clone(),
            &expanded,
            &thread.send_queue,
            true,
            &mut *thread.markdown_render_index.borrow_mut(),
        );
        model.threads[0].message_rows = rows;
        model.threads[0].transcript_keys = keys;
        // Last queue row is "drop-me" (can_edit / most recent).
        let last = model.threads[0].transcript_keys.len() - 1;
        let (effects, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Compose(ComposeMsg::QueueCancel {
                message_index: last,
            })),
        );
        assert!(effects.is_empty());
        assert_eq!(model.threads[0].send_queue.len(), 1);
        assert_eq!(
            model.threads[0].send_queue.first().map(|e| e.text.as_str()),
            Some("stay")
        );
        assert!(
            dirty
                .iter()
                .any(|d| matches!(d, Dirty::MessagesDiff { .. })),
            "cancel must rebuild message rows, got {dirty:?}"
        );
    }

    #[test]
    fn turn_with_agent_content_ends_without_any_notice() {
        let mut model = model_with_threads(&["a"]);
        model.threads[0].state = ThreadState::Loading;
        model.threads[0].agent_content_this_turn = true;
        let (_effects, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                bridge_events: vec![crate::agent_bridge::BridgeEvent {
                    thread_index: 0,
                    event: crate::protocol_types::AgentEvent::TurnEnded("end_turn".to_owned()),
                }],
                bridge_event_thread_ids: vec!["thread-0".to_owned()],
                ..FrameInput::default()
            }),
        );
        assert_eq!(model.threads[0].state, ThreadState::Idle);
        assert!(model.threads[0].error.is_none());
        assert!(!dirty.iter().any(|d| matches!(d, Dirty::Error { .. })));
    }

    #[test]
    fn turn_ended_while_already_idle_never_fabricates_a_notice() {
        // Replayed/late TurnEnded (reconnect) on a thread this session
        // never watched generate must not invent an empty-turn error.
        let mut model = model_with_threads(&["a"]);
        model.threads[0].state = ThreadState::Idle;
        let (_effects, _dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                bridge_events: vec![crate::agent_bridge::BridgeEvent {
                    thread_index: 0,
                    event: crate::protocol_types::AgentEvent::TurnEnded("late".to_owned()),
                }],
                ..FrameInput::default()
            }),
        );
        assert!(model.threads[0].error.is_none());
    }

    #[test]
    fn queue_edit_moves_entry_text_into_compose() {
        let mut model = model_with_threads(&["a"]);
        model.threads[0].state = ThreadState::Loading;
        model.threads[0]
            .send_queue
            .enqueue("edit this".to_owned(), false)
            .expect("queue");
        let expanded = model.expanded.clone();
        let thread = &mut model.threads[0];
        let (rows, keys) = crate::models::message_rows_for_thread_with_state(
            thread.transcript.clone(),
            &expanded,
            &thread.send_queue,
            true,
            &mut *thread.markdown_render_index.borrow_mut(),
        );
        model.threads[0].message_rows = rows;
        model.threads[0].transcript_keys = keys;
        let last = model.threads[0].transcript_keys.len() - 1;
        let (effects, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Compose(ComposeMsg::QueueEdit {
                message_index: last,
            })),
        );
        assert!(effects.is_empty());
        assert!(model.threads[0].send_queue.is_empty());
        assert_eq!(model.compose_text, "edit this");
        assert!(dirty.contains(&Dirty::Scalar(ScalarField::ComposeText)));
    }

    #[test]
    fn queue_send_now_while_idle_sends_immediately_with_no_cancel() {
        let mut model = model_with_threads(&["a"]);
        // Idle: nothing in flight, so send-now is a plain immediate send.
        model.threads[0]
            .send_queue
            .enqueue("go now".to_owned(), false)
            .expect("queue");
        let expanded = model.expanded.clone();
        let thread = &mut model.threads[0];
        let (rows, keys) = crate::models::message_rows_for_thread_with_state(
            thread.transcript.clone(),
            &expanded,
            &thread.send_queue,
            false,
            &mut *thread.markdown_render_index.borrow_mut(),
        );
        model.threads[0].message_rows = rows;
        model.threads[0].transcript_keys = keys;
        let last = model.threads[0].transcript_keys.len() - 1;
        let (effects, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Compose(ComposeMsg::QueueSendNow {
                message_index: last,
            })),
        );
        assert_eq!(
            effects,
            vec![Effect::SendPrompt {
                thread_id: "thread-0".to_owned(),
                text: "go now".to_owned(),
            }]
        );
        assert!(model.threads[0].send_queue.is_empty());
        assert_eq!(model.threads[0].state, ThreadState::Loading);
        assert!(dirty.iter().any(|d| matches!(d, Dirty::Connection { .. })));
    }

    #[test]
    fn queue_send_now_while_generating_cancels_then_sends_and_arms_absorbing_cancel() {
        let mut model = model_with_threads(&["a"]);
        model.threads[0].state = ThreadState::Loading;
        model.threads[0]
            .send_queue
            .enqueue("front".to_owned(), false)
            .expect("queue");
        model.threads[0]
            .send_queue
            .enqueue("steer me".to_owned(), false)
            .expect("queue");
        let expanded = model.expanded.clone();
        let thread = &mut model.threads[0];
        let (rows, keys) = crate::models::message_rows_for_thread_with_state(
            thread.transcript.clone(),
            &expanded,
            &thread.send_queue,
            true,
            &mut *thread.markdown_render_index.borrow_mut(),
        );
        model.threads[0].message_rows = rows;
        model.threads[0].transcript_keys = keys;
        // The second (non-front) entry: "steer me".
        let target_index = model.threads[0].transcript_keys.len() - 1;
        let (effects, _dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Compose(ComposeMsg::QueueSendNow {
                message_index: target_index,
            })),
        );
        assert_eq!(
            effects,
            vec![
                Effect::CancelGeneration { real_index: 0 },
                Effect::SendPrompt {
                    thread_id: "thread-0".to_owned(),
                    text: "steer me".to_owned(),
                },
            ]
        );
        // "steer me" was pulled out; only "front" remains queued.
        assert_eq!(model.threads[0].send_queue.len(), 1);
        assert_eq!(
            model.threads[0]
                .send_queue
                .first()
                .map(|entry| entry.text.as_str()),
            Some("front")
        );
        assert_eq!(model.threads[0].state, ThreadState::Loading);
        // The eventual TurnEnded from the cancel above must not also
        // auto-drain "front" -- AbsorbingCancel swallows it once.
        let popped = model.threads[0]
            .send_queue
            .on_generation_stopped(false)
            .unwrap();
        assert!(
            popped.is_none(),
            "AbsorbingCancel must swallow this Stopped event"
        );
        assert_eq!(model.threads[0].send_queue.len(), 1);
    }

    #[test]
    fn queue_fast_track_while_idle_sends_immediately() {
        // SCNA-03: Return on an empty compose box right after enqueuing
        // (can_fast_track armed by enqueue itself) sends immediately.
        let mut model = model_with_threads(&["a"]);
        model.threads[0]
            .send_queue
            .enqueue("go now".to_owned(), false)
            .expect("queue");
        let (effects, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Compose(ComposeMsg::QueueFastTrack)),
        );
        assert_eq!(
            effects,
            vec![Effect::SendPrompt {
                thread_id: "thread-0".to_owned(),
                text: "go now".to_owned(),
            }]
        );
        assert!(model.threads[0].send_queue.is_empty());
        assert_eq!(model.threads[0].state, ThreadState::Loading);
        assert!(dirty.iter().any(|d| matches!(d, Dirty::Connection { .. })));
    }

    #[test]
    fn queue_fast_track_while_generating_cancels_then_sends_and_arms_absorbing_cancel() {
        let mut model = model_with_threads(&["a"]);
        model.threads[0].state = ThreadState::Loading;
        model.threads[0]
            .send_queue
            .enqueue("front".to_owned(), false)
            .expect("queue");
        let (effects, _dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Compose(ComposeMsg::QueueFastTrack)),
        );
        assert_eq!(
            effects,
            vec![
                Effect::CancelGeneration { real_index: 0 },
                Effect::SendPrompt {
                    thread_id: "thread-0".to_owned(),
                    text: "front".to_owned(),
                },
            ]
        );
        assert!(model.threads[0].send_queue.is_empty());
        assert_eq!(model.threads[0].state, ThreadState::Loading);
        // The eventual TurnEnded from the cancel above must not also
        // auto-drain -- AbsorbingCancel swallows it once, same
        // contract as QueueSendNow.
        let popped = model.threads[0]
            .send_queue
            .on_generation_stopped(false)
            .unwrap();
        assert!(
            popped.is_none(),
            "AbsorbingCancel must swallow this Stopped event"
        );
    }

    #[test]
    fn queue_fast_track_is_a_safe_no_op_when_nothing_is_eligible() {
        // No enqueue just happened (can_fast_track never armed) -- the
        // Slint side fires this unconditionally on empty-compose Return,
        // so this is the expected common case, not an error.
        let mut model = model_with_threads(&["a"]);
        model.threads[0]
            .send_queue
            .enqueue("already queued earlier".to_owned(), false)
            .expect("queue");
        // Consume the fast-track eligibility some other way (matches how
        // any queue mutation other than a fresh enqueue leaves nothing
        // eligible) -- send_now clears it same as try_fast_track would.
        let entry_id = model.threads[0]
            .send_queue
            .first_id()
            .expect("one entry queued");
        model.threads[0]
            .send_queue
            .send_now(entry_id, false)
            .expect("send_now");
        assert!(model.threads[0].send_queue.is_empty());

        let (effects, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Compose(ComposeMsg::QueueFastTrack)),
        );
        assert!(effects.is_empty());
        assert!(dirty.is_empty());
    }

    #[test]
    fn queue_stop_pauses_queue_and_cancels_generation() {
        let mut model = model_with_threads(&["a"]);
        model.threads[0].state = ThreadState::Loading;
        model.threads[0]
            .send_queue
            .enqueue("waiting".to_owned(), false)
            .expect("queue");
        let (effects, dirty) = update(&mut model, Msg::Ui(UiMsg::Compose(ComposeMsg::QueueStop)));
        assert_eq!(effects, vec![Effect::CancelGeneration { real_index: 0 }]);
        assert_eq!(model.threads[0].state, ThreadState::Cancelling);
        assert!(dirty.contains(&Dirty::ThreadRow {
            thread_id: "thread-0".to_owned()
        }));
        // Paused: TurnEnded must not auto-drain.
        let (effects2, _) = update(
            &mut model,
            Msg::Frame(FrameInput {
                bridge_events: vec![crate::agent_bridge::BridgeEvent {
                    thread_index: 0,
                    event: crate::protocol_types::AgentEvent::TurnEnded("cancelled".to_owned()),
                }],
                bridge_event_thread_ids: vec!["thread-0".to_owned()],
                ..FrameInput::default()
            }),
        );
        assert!(
            effects2.is_empty(),
            "paused queue must not auto-send after stop, got {effects2:?}"
        );
        assert_eq!(model.threads[0].send_queue.len(), 1);
    }

    #[test]
    fn cancelled_empty_turn_never_fires_the_empty_turn_notice() {
        // Interaction between setup-followups' queue-stop semantics and
        // main's empty-turn notice (adopted during the worktree
        // consolidation merge): a user-initiated stop ends the turn from
        // Cancelling with no agent output -- that is the user's own
        // doing, not a silent failure, so the "ended without a
        // response" notice must stay silent. Only a turn that dies from
        // Loading qualifies.
        let mut model = model_with_threads(&["a"]);
        model.threads[0].state = ThreadState::Cancelling;
        model.threads[0].agent_content_this_turn = false;
        let (_effects, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                bridge_events: vec![crate::agent_bridge::BridgeEvent {
                    thread_index: 0,
                    event: crate::protocol_types::AgentEvent::TurnEnded("cancelled".to_owned()),
                }],
                bridge_event_thread_ids: vec!["thread-0".to_owned()],
                ..FrameInput::default()
            }),
        );
        assert_eq!(model.threads[0].state, ThreadState::Idle);
        assert!(
            model.threads[0].error.is_none(),
            "user-cancelled empty turn must not fabricate a notice"
        );
        assert!(!dirty.iter().any(|d| matches!(d, Dirty::Error { .. })));
    }

    #[test]
    fn frame_event_resolves_by_durable_thread_id_after_model_row_shift() {
        let mut model = model_with_threads(&["target", "other"]);
        model.threads[0].thread_id = "target-id".to_owned();
        model.threads[1].thread_id = "other-id".to_owned();
        model.threads[0]
            .send_queue
            .enqueue("queued".to_owned(), false)
            .expect("queue entry");

        model.threads.swap(0, 1);
        let (effects, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                bridge_events: vec![crate::agent_bridge::BridgeEvent {
                    thread_index: 0,
                    event: crate::protocol_types::AgentEvent::TurnEnded("end_turn".to_owned()),
                }],
                bridge_event_thread_ids: vec!["target-id".to_owned()],
                ..FrameInput::default()
            }),
        );

        assert_eq!(
            effects,
            vec![Effect::SendPrompt {
                thread_id: "target-id".to_owned(),
                text: "queued".to_owned(),
            }]
        );
        assert_eq!(model.threads[0].thread_id, "other-id");
        assert_eq!(model.threads[1].thread_id, "target-id");
        assert_eq!(model.threads[1].state, ThreadState::Loading);
        assert!(dirty.contains(&Dirty::ThreadRow {
            thread_id: "target-id".to_owned()
        }));
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
                    open_terminals: vec![],
                    local_terminal: crate::LocalTerminalItem::default(),
                    connection_status: "Unavailable".to_owned(),
                    session_modes: None,
                    config_options: Vec::new(),
                    available_commands: Vec::new(),
                    plan: vec![],
                    session_title: None,
                    usage: (0, 0),
                }),
                ..FrameInput::default()
            }),
        );
        assert!(effects.is_empty());
        assert!(dirty.is_empty());
    }

    #[test]
    fn frame_event_with_unknown_identity_does_not_use_positional_fallback() {
        let mut model = model_with_threads(&["first", "second"]);
        model.threads[1].state = ThreadState::Loading;
        let (effects, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                bridge_events: vec![crate::agent_bridge::BridgeEvent {
                    thread_index: 1,
                    event: crate::protocol_types::AgentEvent::TurnEnded("stale".to_owned()),
                }],
                bridge_event_thread_ids: vec!["unknown-thread".to_owned()],
                ..FrameInput::default()
            }),
        );

        assert!(effects.is_empty());
        assert!(dirty.is_empty());
        assert_eq!(model.threads[1].state, ThreadState::Loading);
    }

    #[test]
    fn unknown_identity_snapshot_clears_previous_shared_message_list() {
        let mut model = model_with_threads(&["a"]);
        model.displayed_thread = Some(0);
        model.list_owner_thread_id = Some("thread-0".to_owned());
        model.messages_model.push(crate::MessageItem {
            text: "stale previous thread content".into(),
            ..Default::default()
        });
        *model.message_model_keys.borrow_mut() = vec!["assistant:stale".to_owned()];

        let (_, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                selected_thread_snapshot: Some(crate::msg::ThreadFrameSnapshot {
                    thread_id: "thread-that-no-longer-exists".to_owned(),
                    real_index: 99,
                    transcript: Vec::new(),
                    has_older_messages: false,
                    pending_request: crate::PendingRequestItem::default(),
                    terminals: Vec::new(),
                    expanded_terminal: None,
                    open_terminals: Vec::new(),
                    local_terminal: crate::LocalTerminalItem::default(),
                    connection_status: "Unavailable".to_owned(),
                    session_modes: None,
                    config_options: Vec::new(),
                    available_commands: Vec::new(),
                    plan: Vec::new(),
                    session_title: None,
                    usage: (0, 0),
                }),
                ..FrameInput::default()
            }),
        );

        assert_eq!(model.displayed_thread, None);
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
    fn streaming_delta_survives_unrelated_thread_insert_and_ignores_removed_target() {
        let mut model = model_with_threads(&["target", "unrelated"]);
        model.threads[0].session_id = Some("target-session".to_owned());
        model.threads[0].message_ids.push("message-1".to_owned());
        model.threads[0].transcript_keys = vec!["assistant:message-1".to_owned()];
        model.threads[0].message_rows = vec![crate::MessageItem {
            text: "hello".into(),
            ..crate::MessageItem::default()
        }];

        let (effects, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::PromptStreamDelta {
                thread_id: "target-session".to_owned(),
                message_id: "message-1".to_owned(),
                delta: " next".to_owned(),
            }),
        );
        assert!(effects.is_empty());
        assert_eq!(
            dirty,
            vec![Dirty::MessageStreamingDelta {
                thread_id: "target-session".to_owned(),
                message_id: "message-1".to_owned(),
                delta: " next".to_owned(),
            }]
        );
        assert_eq!(model.threads[0].message_rows[0].text, "hello next");

        let (_, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Thread(ThreadMsg::NewResolved {
                display_name: "inserted".to_owned(),
                provider: "codex".to_owned(),
                profile_name: None,
                permission_profile: None,
                session_id: Some("inserted-session".to_owned()),
                thread_id: Some("inserted-thread".to_owned()),
            })),
        );
        assert!(dirty.iter().any(|item| matches!(
            item,
            Dirty::ThreadListDiff(ops)
                if matches!(ops.as_slice(), [RowOp::Insert { at: 2, .. }])
        )));

        model.threads[0].closed = true;
        model.threads.remove(0);
        let (effects, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::PromptStreamDelta {
                thread_id: "target-session".to_owned(),
                message_id: "message-1".to_owned(),
                delta: " late".to_owned(),
            }),
        );
        assert!(effects.is_empty());
        assert!(dirty.is_empty());
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
    fn state_effect_failed_surfaces_as_dirty_error_not_silently_dropped() {
        let mut model = Model::default();
        let (effects, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::StateEffectFailed {
                thread_id: "thread-a".to_owned(),
                message: "failed to toggle background-session override: boom".to_owned(),
            }),
        );
        assert!(effects.is_empty());
        assert_eq!(
            dirty,
            vec![Dirty::Error {
                thread_id: "thread-a".to_owned(),
                detail: ErrorDetail {
                    message: "failed to toggle background-session override: boom".to_owned(),
                },
            }]
        );
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
        let commands_model = model.commands_model.clone();
        let open_terminals_model = model.open_terminals_model.clone();
        *model.open_terminal_model_keys.borrow_mut() = vec!["terminal-1".to_owned()];
        let (_, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::InitialStateLoaded(Ok(
                crate::model::InitialState {
                    threads: vec![crate::agent_bridge::ThreadSpec {
                        display_name: "fresh".to_owned(),
                        provider: "codex".to_owned(),
                        session_id: None,
                        profile_name: None,
                        project_path: None,
                    }],
                    thread_ids: vec!["thread-1".to_owned()],
                    selected_thread_id: None,
                    permission_profiles: vec![],
                    thread_states: vec![],
                    startup_warnings: vec![],
                    send_queues: vec![],
                    onboarding_completed: false,
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
        assert!(std::rc::Rc::ptr_eq(&commands_model, &model.commands_model));
        assert!(std::rc::Rc::ptr_eq(
            &open_terminals_model,
            &model.open_terminals_model
        ));
        assert_eq!(
            model.open_terminal_model_keys.borrow().as_slice(),
            ["terminal-1"]
        );
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
    fn initial_state_loaded_surfaces_startup_warnings_as_dirty_errors() {
        let mut model = Model::default();
        let (_, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::InitialStateLoaded(Ok(
                crate::model::InitialState {
                    threads: vec![],
                    thread_ids: vec![],
                    selected_thread_id: None,
                    permission_profiles: vec![],
                    thread_states: vec![],
                    startup_warnings: vec![
                        "panel settings persistence unavailable: boom".to_owned(),
                        "agent bridge unavailable, chat panel is display-only: boom".to_owned(),
                    ],
                    send_queues: vec![],
                    onboarding_completed: false,
                },
            ))),
        );
        let errors: Vec<&str> = dirty
            .iter()
            .filter_map(|d| match d {
                Dirty::Error { detail, .. } => Some(detail.message.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            errors,
            vec![
                "panel settings persistence unavailable: boom",
                "agent bridge unavailable, chat panel is display-only: boom",
            ]
        );
    }

    #[test]
    fn frame_tick_with_no_real_change_is_a_no_op() {
        let mut model = Model::default();
        let (effects, dirty) = update(&mut model, Msg::Frame(FrameInput::default()));
        assert!(effects.is_empty());
        assert!(dirty.is_empty());
    }

    #[test]
    fn repeated_poll_ticks_for_an_unchanged_agent_transcript_stop_resyncing_after_the_first() {
        // Regression test: `MessageItem.markdown_lines` is a
        // `ModelRc<MarkdownLine>`, whose `PartialEq` (i-slint-core's
        // `model.rs`) compares by `Rc` pointer identity, not content.
        // `to_message_rows_from_transcript` builds a brand-new `ModelRc`
        // every call, so comparing `thread.message_rows != rows` was true
        // on *every* poll tick for any thread with an agent-kind message,
        // even with byte-identical input -- forcing a full
        // `Dirty::MessagesDiff` resync at the 60-90fps poll rate for no
        // real reason. Two ticks with the exact same snapshot must settle:
        // the first may resync (populating the shared model for the first
        // time), the second must not.
        let mut model = model_with_threads(&["only"]);
        let snapshot = || crate::msg::ThreadFrameSnapshot {
            thread_id: "thread-0".to_owned(),
            real_index: 0,
            transcript: vec![crate::conversation::TranscriptItem::Assistant {
                message_id: "reply-1".to_owned(),
                text: "a steady-state agent reply".to_owned(),
                streaming: false,
            }],
            has_older_messages: false,
            pending_request: crate::PendingRequestItem::default(),
            terminals: vec![],
            expanded_terminal: None,
            open_terminals: vec![],
            local_terminal: crate::LocalTerminalItem::default(),
            connection_status: String::new(),
            session_modes: None,
            config_options: vec![],
            available_commands: vec![],
            plan: vec![],
            session_title: None,
            usage: (0, 0),
        };

        let (_, first_dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                selected_thread_snapshot: Some(snapshot()),
                ..FrameInput::default()
            }),
        );
        assert!(
            first_dirty
                .iter()
                .any(|item| matches!(item, Dirty::MessagesDiff { .. })),
            "first tick should populate the shared model: {first_dirty:?}"
        );

        let (_, second_dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                selected_thread_snapshot: Some(snapshot()),
                ..FrameInput::default()
            }),
        );
        assert!(
            !second_dirty
                .iter()
                .any(|item| matches!(item, Dirty::MessagesDiff { .. })),
            "second tick with an unchanged snapshot must not resync: {second_dirty:?}"
        );
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
        let (effects, dirty) = update(
            &mut model,
            Msg::Host(HostMsg::AppearanceChanged(appearance)),
        );
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
            project_path: None,
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

    // PISO-8 (project-isolation-mlt-binding plan): the throttle flag is the
    // ONLY thing that queues the background poll -- update_frame must
    // never spawn it on every tick (that would mean a real subprocess
    // spawn 60-90x/sec, exactly what the plan's data-path discipline note
    // forbids on this path).
    #[test]
    fn daemon_projects_refresh_due_queues_the_refresh_effect_only_when_true() {
        let mut model = Model::default();
        let (effects, _) = update(
            &mut model,
            Msg::Frame(crate::msg::FrameInput {
                daemon_projects_refresh_due: true,
                ..crate::msg::FrameInput::default()
            }),
        );
        assert_eq!(effects, vec![Effect::RefreshDaemonProjectInstances]);

        let (effects, _) = update(
            &mut model,
            Msg::Frame(crate::msg::FrameInput {
                daemon_projects_refresh_due: false,
                ..crate::msg::FrameInput::default()
            }),
        );
        assert!(effects.is_empty());
    }

    #[test]
    fn daemon_project_instances_loaded_replaces_model_on_ok_and_keeps_previous_on_err() {
        let mut model = Model::default();
        let instance = crate::agent_bridge::DaemonProjectInstance {
            project_path: "/work/b/project.mlt".to_string(),
            headless: true,
        };
        let (effects, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::DaemonProjectInstancesLoaded(Ok(vec![
                instance.clone(),
            ]))),
        );
        assert!(effects.is_empty());
        assert!(dirty.is_empty());
        assert_eq!(model.live_daemon_projects, vec![instance.clone()]);

        // A failed poll (daemon unreachable, ...) must not clear the
        // previously cached instances or surface an error toast for a
        // background poll the user never triggered.
        let (effects, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::DaemonProjectInstancesLoaded(Err(
                crate::effect::EffectError::new("connection refused"),
            ))),
        );
        assert!(effects.is_empty());
        assert!(dirty.is_empty());
        assert_eq!(model.live_daemon_projects, vec![instance]);
    }

    #[test]
    fn frame_tick_marks_only_the_external_snapshots_that_changed() {
        let mut model = Model::default();
        let (effects, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                bridge_events: Vec::new(),
                bridge_event_thread_ids: Vec::new(),
                bridge_events_pending: true,
                thread_record_snapshots: Vec::new(),
                settings_reload_pending: true,
                prepend_expanded_rows: 0,
                thread_list_snapshot: None,
                selected_thread_snapshot: None,
                clear_selected_thread: false,
                settings_gateway_snapshot: None,
                settings_preferences_snapshot: None,
                agent_operations_in_flight: Vec::new(),
                mcp_operations_in_flight: Vec::new(),
                recover_session_operations_in_flight: Vec::new(),
                skills_snapshot: None,
                daemon_projects_refresh_due: false,
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
                    open_terminals: vec![],
                    local_terminal: crate::LocalTerminalItem::default(),
                    connection_status: "Live connection".to_owned(),
                    session_modes: None,
                    config_options: vec![],
                    available_commands: vec![],
                    plan: vec![],
                    session_title: None,
                    usage: (0, 0),
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
                Dirty::MessagesDiff { thread_id, .. }
                if thread_id == "thread-0"
        )));
        assert!(dirty.iter().any(|item| matches!(
            item,
            Dirty::Connection { thread_id } if thread_id == "thread-0"
        )));
    }

    #[test]
    fn cold_starts_first_displayed_thread_snapshot_never_clears_a_global_error_banner() {
        // SCNA-01 regression: model.displayed_thread starts None before
        // cold-start hydration's first Frame. That first frame's own
        // "switched_thread" (None -> Some(0)) used to unconditionally
        // push Dirty::Error{thread_id: "thread-0", message: ""}
        // (thread.error is always None on a freshly-restored thread),
        // silently wiping out any InitialState::startup_warnings banner
        // set moments earlier in the same synchronous cold-start call,
        // before the window was ever shown. The fix: skip that push when
        // there was no real previously-displayed thread to leave.
        let mut model = model_with_threads(&["thread"]);
        assert_eq!(model.displayed_thread, None);
        let (_, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                selected_thread_snapshot: Some(crate::msg::ThreadFrameSnapshot {
                    thread_id: "thread-0".to_owned(),
                    real_index: 0,
                    ..crate::msg::ThreadFrameSnapshot::default()
                }),
                ..FrameInput::default()
            }),
        );
        assert_eq!(model.displayed_thread, Some(0));
        assert!(
            !dirty.iter().any(|item| matches!(item, Dirty::Error { .. })),
            "the first-ever displayed-thread snapshot must not emit a Dirty::Error, got {dirty:?}"
        );
    }

    #[test]
    fn a_real_thread_switch_still_clears_the_outgoing_threads_error_banner() {
        // Companion to the cold-start regression above: once a thread has
        // genuinely been displayed before, switching away from it must
        // still clear/refresh the error banner for the incoming thread --
        // this is the original leak_audit_report behavior the
        // had_prior_displayed_thread guard must not disable.
        let mut model = model_with_threads(&["first", "second"]);
        model.threads[0].session_id = Some("thread-0-session".to_owned());
        model.displayed_thread = Some(0);
        // phase-23's selection_matches gate requires the snapshot's target
        // to actually be the currently-selected thread, not just any
        // thread -- otherwise switched_thread is false regardless of this
        // test's own guard, for an unrelated reason.
        model.selected_thread = 1;
        let (_, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                selected_thread_snapshot: Some(crate::msg::ThreadFrameSnapshot {
                    thread_id: "thread-1".to_owned(),
                    real_index: 1,
                    ..crate::msg::ThreadFrameSnapshot::default()
                }),
                ..FrameInput::default()
            }),
        );
        assert!(
            dirty.iter().any(|item| matches!(
                item,
                Dirty::Error { thread_id, .. } if thread_id == "thread-1"
            )),
            "a genuine thread switch must still refresh the error banner, got {dirty:?}"
        );
    }

    #[test]
    fn switching_to_a_thread_with_a_coincidentally_unchanged_transcript_still_resyncs_the_shared_model(
    ) {
        // Regression test: "starting a new chat shows prefill data [from
        // the previous thread]". `transcript_changed` used to compare the
        // *target* thread's own transcript against its own previously
        // cached copy -- for a brand new thread both are empty, so the
        // comparison was a no-op and no `Dirty::MessagesDiff` fired. But
        // the *shared* `messages_model`/`message_model_keys` (what's
        // actually on screen) still held the *previously displayed*
        // thread's messages, which were never told to clear. The fix
        // forces a resync on every real `switched_thread` transition,
        // regardless of whether the newly-selected thread's own transcript
        // happened to be unchanged since its last visit.
        let mut model = model_with_threads(&["first", "second"]);
        model.threads[0].session_id = Some("thread-0-session".to_owned());
        model.threads[0].transcript = vec![crate::conversation::TranscriptItem::Assistant {
            message_id: "old-message".to_owned(),
            text: "leftover from the previous thread".to_owned(),
            streaming: false,
        }];
        model.threads[0].transcript_keys = vec!["assistant:old-message".to_owned()];
        model.displayed_thread = Some(0);
        model.selected_thread = 1;
        // The bug this test is actually about lives in the *shared*,
        // UI-facing `messages_model`/`message_model_keys` -- not in
        // per-thread state (`model.threads[0].transcript` above is real,
        // but asserting only against the returned `Dirty` marker (as this
        // test originally did) proves the reducer *decided* to resync,
        // not that the shared model actually ends up empty. Seed it with
        // thread-0's stale row directly, matching what would genuinely be
        // on screen while thread 0 was displayed, so the assertions below
        // can catch it surviving the switch.
        model.messages_model.push(crate::MessageItem {
            text: "leftover from the previous thread".into(),
            kind: "agent".into(),
            ..crate::MessageItem::default()
        });
        *model.message_model_keys.borrow_mut() = vec!["assistant:old-message".to_owned()];

        // Thread 1 is brand new: its own cached transcript is empty both
        // before and after this snapshot -- the exact "coincidentally
        // unchanged" case that used to suppress the dirty marker.
        let (_, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                selected_thread_snapshot: Some(crate::msg::ThreadFrameSnapshot {
                    thread_id: "thread-1".to_owned(),
                    real_index: 1,
                    transcript: vec![],
                    has_older_messages: false,
                    pending_request: crate::PendingRequestItem::default(),
                    terminals: vec![],
                    expanded_terminal: None,
                    open_terminals: vec![],
                    local_terminal: crate::LocalTerminalItem::default(),
                    connection_status: String::new(),
                    session_modes: None,
                    config_options: vec![],
                    available_commands: vec![],
                    plan: vec![],
                    session_title: None,
                    usage: (0, 0),
                }),
                ..FrameInput::default()
            }),
        );

        assert_eq!(model.displayed_thread, Some(1));
        let ops = dirty.iter().find_map(|item| match item {
            Dirty::MessagesDiff { thread_id, ops } if thread_id == "thread-1" => Some(ops.clone()),
            _ => None,
        });
        assert!(
            ops.is_some(),
            "switching to the new thread must update its retained message model even \
             though thread-1's own transcript diff is a no-op -- otherwise thread-0's \
             messages stay on screen as bogus 'prefill' data: {dirty:?}"
        );

        // Apply the same way sync() would through the compatibility helper;
        // production sync now targets the retained per-thread model.
        if let Some(ops) = ops {
            crate::sync::apply_message_ops(&model, "thread-1", &ops);
        }
        assert_eq!(
            model.messages_model.row_count(),
            0,
            "thread-0's stale message must not survive switching to the new, empty thread-1"
        );
        assert!(
            model.message_model_keys.borrow().is_empty(),
            "the shared message key cache must also clear, not just the visible row count"
        );
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
                    open_terminals: vec![],
                    local_terminal: crate::LocalTerminalItem::default(),
                    connection_status: "Live".to_owned(),
                    session_modes: None,
                    config_options: vec![],
                    available_commands: vec![],
                    plan: vec![],
                    session_title: None,
                    usage: (0, 0),
                }),
                ..FrameInput::default()
            }),
        );

        assert!(model.threads[1].transcript.is_empty());
        assert_eq!(model.threads[2].display_name, "target");
        assert_eq!(model.threads[2].transcript, transcript);
        assert!(dirty.iter().any(|item| matches!(
            item,
            Dirty::MessagesDiff { thread_id, .. }
            if thread_id == "thread-1"
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
            is_dev_only: false,
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
    fn skill_actions_are_described_as_effects() {
        let mut model = Model::default();
        model.active_project_path = Some("/tmp/project/shotcut.mlt".to_owned());

        let (effects, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Skill(SkillMsg::NewSkillRequested {
                name: "review".to_owned(),
                scope: "project".to_owned(),
            })),
        );
        assert!(matches!(
            effects.as_slice(),
            [Effect::CreateSkill {
                name,
                scope,
                active_project_path: Some(path),
            }] if name == "review" && scope == "project" && path == "/tmp/project/shotcut.mlt"
        ));
        assert!(matches!(dirty.as_slice(), [Dirty::SkillsListDiff(_)]));

        let (effects, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Skill(SkillMsg::OpenInEditorRequested {
                editor_name: "VS Code".to_owned(),
                path: "/tmp/project/review".into(),
            })),
        );
        assert!(matches!(
            effects.as_slice(),
            [Effect::OpenInEditor { editor_name, path }]
                if editor_name == "VS Code"
                    && path == &std::path::PathBuf::from("/tmp/project/review")
        ));
        assert!(dirty.is_empty());

        let (effects, _) = update(
            &mut model,
            Msg::Ui(UiMsg::Skill(SkillMsg::OpenWithOsDefaultRequested {
                path: "/tmp/project/review".into(),
            })),
        );
        assert!(matches!(
            effects.as_slice(),
            [Effect::OpenWithOsDefault { path }]
                if path == &std::path::PathBuf::from("/tmp/project/review")
        ));
    }

    #[test]
    fn skill_creation_result_opens_the_new_skill_in_the_model_editor() {
        let mut model = Model::default();
        let path = std::path::PathBuf::from("/tmp/review");
        let (effects, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::SkillCreated(Ok(path.clone()))),
        );
        assert_eq!(effects, vec![Effect::OpenSkillEditor { path }]);
        // Skills list is refreshed by the effect executor *before* this
        // result is folded; SkillCreated opens the editor and (phase 28)
        // arms the shared feedback toast.
        assert!(dirty.iter().all(|d| matches!(d, Dirty::Toast)));
        assert_eq!(model.toast_message, "Skill created");
    }

    #[test]
    fn frame_thread_list_snapshot_uses_durable_ids_as_row_keys() {
        let mut model = Model::default();
        let row = crate::models::VisibleThreadItem {
            real_index: 4,
            thread_id: "durable-thread-4".to_owned(),
            session_id: None,
            agent_detected: None,
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
                    archived_flags: vec![],
                    active_project_path: None,
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

    /// PROF-7: the actual state-setting deliverable -- when the frame fold
    /// hydrates a just-attached thread's `session_id` (the same
    /// `session_id.is_none()` transition `frame_thread_list_snapshot_uses_
    /// durable_ids_as_row_keys` above exercises) and the row's
    /// `agent_detected` says the bound agent was NOT found installed, the
    /// thread's state becomes `ThreadState::Stale` -- a real per-thread
    /// state written once at attach time, not a render-time heuristic.
    #[test]
    fn session_attach_with_agent_not_detected_marks_the_thread_stale() {
        let mut model = model_with_threads(&["Restored Thread"]);
        assert_eq!(model.threads[0].session_id, None);
        assert_eq!(model.threads[0].state, ThreadState::Idle);

        let row = crate::models::VisibleThreadItem {
            real_index: 0,
            thread_id: "thread-0".to_owned(),
            session_id: Some("real-session-id".to_owned()),
            agent_detected: Some(false),
            item: crate::ThreadItem::default(),
        };
        update(
            &mut model,
            Msg::Frame(FrameInput {
                thread_list_snapshot: Some(crate::msg::ThreadListSnapshot {
                    visible_indices: vec![0],
                    visible_thread_ids: vec!["thread-0".to_owned()],
                    rows: vec![row],
                    archived_flags: vec![],
                    active_project_path: None,
                }),
                ..FrameInput::default()
            }),
        );

        assert_eq!(
            model.threads[0].session_id.as_deref(),
            Some("real-session-id")
        );
        assert_eq!(
            model.threads[0].state,
            ThreadState::Stale,
            "an attach whose agent_detected read false must mark the thread Stale"
        );
    }

    /// Companion: the SAME transition, but `agent_detected: Some(true)`
    /// (or `None`, e.g. native/unmanaged mode) must leave the thread's
    /// state alone -- Stale is only ever set, never assumed.
    #[test]
    fn session_attach_with_agent_detected_or_unknown_does_not_mark_stale() {
        for agent_detected in [Some(true), None] {
            let mut model = model_with_threads(&["Restored Thread"]);
            let row = crate::models::VisibleThreadItem {
                real_index: 0,
                thread_id: "thread-0".to_owned(),
                session_id: Some("real-session-id".to_owned()),
                agent_detected,
                item: crate::ThreadItem::default(),
            };
            update(
                &mut model,
                Msg::Frame(FrameInput {
                    thread_list_snapshot: Some(crate::msg::ThreadListSnapshot {
                        visible_indices: vec![0],
                        visible_thread_ids: vec!["thread-0".to_owned()],
                        rows: vec![row],
                        archived_flags: vec![],
                        active_project_path: None,
                    }),
                    ..FrameInput::default()
                }),
            );
            assert_eq!(
                model.threads[0].state,
                ThreadState::Idle,
                "agent_detected={agent_detected:?} must never produce Stale"
            );
        }
    }

    /// PROF-9: creating an MCP server, or turning one (or a tool) ON, must
    /// be blocked with a toast -- not sent as a real Effect -- when the
    /// selected thread's agent is Stale or unauthenticated. Delete,
    /// turning something OFF, and Authenticate must NOT be blocked (see
    /// the doc comment on the McpServer* match arms in `update_settings`
    /// for why).
    #[test]
    fn mcp_server_create_and_enable_are_blocked_for_a_stale_or_unauthenticated_thread() {
        for make_unusable in [
            (|t: &mut ThreadModel| t.state = ThreadState::Stale) as fn(&mut ThreadModel),
            (|t: &mut ThreadModel| t.unauthenticated = true) as fn(&mut ThreadModel),
        ] {
            let mut model = model_with_threads(&["Thread"]);
            make_unusable(&mut model.threads[0]);

            let (effects, dirty) = update(
                &mut model,
                Msg::Ui(UiMsg::Settings(SettingsMsg::McpServerCreate {
                    entry: crate::protocol_types::McpServerEntry::new(
                        "srv",
                        crate::protocol_types::McpServerConfig::Stdio {
                            command: "cmd".to_owned(),
                            args: Vec::new(),
                            env: Default::default(),
                            timeout: None,
                        },
                    ),
                })),
            );
            assert!(
                effects.is_empty(),
                "McpServerCreate must not reach a real Effect when the agent is unusable"
            );
            assert!(matches!(dirty.as_slice(), [Dirty::Toast]));
            assert_eq!(model.toast_kind, "error");

            let (effects, dirty) = update(
                &mut model,
                Msg::Ui(UiMsg::Settings(SettingsMsg::McpServerEnabledChanged {
                    name: "srv".to_owned(),
                    enabled: true,
                })),
            );
            assert!(
                effects.is_empty(),
                "enabling must be blocked when the agent is unusable"
            );
            assert!(matches!(dirty.as_slice(), [Dirty::Toast]));

            let (effects, dirty) = update(
                &mut model,
                Msg::Ui(UiMsg::Settings(SettingsMsg::McpServerToolEnabledChanged {
                    server_name: "srv".to_owned(),
                    tool_name: "tool".to_owned(),
                    enabled: true,
                })),
            );
            assert!(
                effects.is_empty(),
                "enabling a tool must be blocked when the agent is unusable"
            );
            assert!(matches!(dirty.as_slice(), [Dirty::Toast]));
        }
    }

    /// Companion: a healthy thread (Idle, not unauthenticated) must never
    /// be blocked, and delete / disable / authenticate must always pass
    /// through regardless of thread health.
    #[test]
    fn mcp_server_actions_pass_through_for_a_healthy_thread_and_delete_disable_authenticate_always_pass(
    ) {
        let mut model = model_with_threads(&["Thread"]);
        assert_eq!(model.threads[0].state, ThreadState::Idle);
        assert!(!model.threads[0].unauthenticated);

        let (effects, _) = update(
            &mut model,
            Msg::Ui(UiMsg::Settings(SettingsMsg::McpServerCreate {
                entry: crate::protocol_types::McpServerEntry::new(
                    "srv",
                    crate::protocol_types::McpServerConfig::Stdio {
                        command: "cmd".to_owned(),
                        args: Vec::new(),
                        env: Default::default(),
                        timeout: None,
                    },
                ),
            })),
        );
        assert!(matches!(
            effects.as_slice(),
            [Effect::McpServerCreate { .. }]
        ));

        // Now make the thread unusable and confirm delete/disable/authenticate
        // still go through as real Effects.
        model.threads[0].state = ThreadState::Stale;

        let (effects, _) = update(
            &mut model,
            Msg::Ui(UiMsg::Settings(SettingsMsg::McpServerDelete {
                name: "srv".to_owned(),
            })),
        );
        assert!(
            matches!(effects.as_slice(), [Effect::McpServerDelete { .. }]),
            "delete must always be reachable, even for an unusable agent"
        );

        let (effects, _) = update(
            &mut model,
            Msg::Ui(UiMsg::Settings(SettingsMsg::McpServerEnabledChanged {
                name: "srv".to_owned(),
                enabled: false,
            })),
        );
        assert!(
            matches!(effects.as_slice(), [Effect::McpServerEnabledChanged { .. }]),
            "turning a server OFF must always be reachable"
        );

        let (effects, _) = update(
            &mut model,
            Msg::Ui(UiMsg::Settings(SettingsMsg::McpServerToolEnabledChanged {
                server_name: "srv".to_owned(),
                tool_name: "tool".to_owned(),
                enabled: false,
            })),
        );
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::McpServerToolEnabledChanged { .. }]
            ),
            "turning a tool OFF must always be reachable"
        );

        let (effects, _) = update(
            &mut model,
            Msg::Ui(UiMsg::Settings(SettingsMsg::McpServerAuthenticate {
                name: "srv".to_owned(),
            })),
        );
        assert!(
            matches!(effects.as_slice(), [Effect::McpServerAuthenticate { .. }]),
            "authenticate is the MCP server's own credentials, orthogonal to agent health"
        );
    }

    fn visible_row(real_index: usize, thread_id: &str) -> crate::models::VisibleThreadItem {
        crate::models::VisibleThreadItem {
            real_index,
            thread_id: thread_id.to_owned(),
            session_id: None,
            agent_detected: None,
            item: crate::ThreadItem::default(),
        }
    }

    #[test]
    fn visible_reorder_reanchors_selection_to_the_same_thread() {
        // Plan phase 23 (thread-switch message leak): `selected_thread` is
        // a filtered index, so a visible-order rewrite (recency resort,
        // archive/resume, new thread on top) used to silently retarget the
        // selection at whichever thread now occupies that slot -- the next
        // frame then rendered *that* thread's transcript. The fold must
        // re-anchor the index to the same durable thread id.
        let mut model = model_with_threads(&["a", "b", "c"]);
        model.visible_indices = vec![0, 1, 2];
        model.selected_thread = 1; // thread-1
        *model.thread_model_keys.borrow_mut() = vec![
            "thread-0".to_owned(),
            "thread-1".to_owned(),
            "thread-2".to_owned(),
        ];

        // thread-2 got background activity and resorted to the top;
        // thread-1 now sits at visible position 2.
        let (_, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                thread_list_snapshot: Some(crate::msg::ThreadListSnapshot {
                    visible_indices: vec![2, 0, 1],
                    visible_thread_ids: vec![
                        "thread-2".to_owned(),
                        "thread-0".to_owned(),
                        "thread-1".to_owned(),
                    ],
                    rows: vec![
                        visible_row(2, "thread-2"),
                        visible_row(0, "thread-0"),
                        visible_row(1, "thread-1"),
                    ],
                    archived_flags: vec![],
                    active_project_path: None,
                }),
                ..FrameInput::default()
            }),
        );

        assert_eq!(
            model.selected_thread, 2,
            "selection must follow thread-1 to its new visible slot"
        );
        assert_eq!(selected_real_index(&model), Some(1));
        assert!(dirty
            .iter()
            .any(|item| matches!(item, Dirty::Scalar(ScalarField::SelectedThread))));
    }

    #[test]
    fn frame_fold_hydrates_a_background_attached_session_binding() {
        // Phase-32 review finding 1: add_thread attaches in the background
        // (phase 30), so no SessionAttached fold ever carries the session
        // id for +-created threads -- the model's session_id stayed None
        // forever (profile picker never locked, PersistThread never
        // fired). The thread-list fold now hydrates it from the row.
        let mut model = model_with_threads(&["a"]);
        assert!(model.threads[0].session_id.is_none());
        let mut row = visible_row(0, "thread-0");
        row.session_id = Some("sess-live".to_owned());
        let (effects, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                thread_list_snapshot: Some(crate::msg::ThreadListSnapshot {
                    visible_indices: vec![0],
                    visible_thread_ids: vec!["thread-0".to_owned()],
                    rows: vec![row],
                    archived_flags: vec![],
                    active_project_path: None,
                }),
                ..FrameInput::default()
            }),
        );
        assert_eq!(model.threads[0].session_id.as_deref(), Some("sess-live"));
        assert!(effects
            .iter()
            .any(|e| matches!(e, Effect::PersistThread { real_index: 0 })));
        assert!(dirty
            .iter()
            .any(|d| matches!(d, Dirty::Capabilities { thread_id } if thread_id == "thread-0")));
    }

    #[test]
    fn frame_fold_hydrates_bridge_archived_flags() {
        // Phase-32 review finding 2: restarts left every
        // ThreadModel::archived false while the bridge restored the
        // persisted flags -- wrong sidebar counters, unenforced pool cap.
        let mut model = model_with_threads(&["a", "b"]);
        let (_, _) = update(
            &mut model,
            Msg::Frame(FrameInput {
                thread_list_snapshot: Some(crate::msg::ThreadListSnapshot {
                    visible_indices: vec![0, 1],
                    visible_thread_ids: vec!["thread-0".to_owned(), "thread-1".to_owned()],
                    rows: vec![visible_row(0, "thread-0"), visible_row(1, "thread-1")],
                    archived_flags: vec![true, false],
                    active_project_path: None,
                }),
                ..FrameInput::default()
            }),
        );
        assert!(model.threads[0].archived);
        assert!(!model.threads[1].archived);
    }

    #[test]
    fn empty_scoped_visible_list_clears_the_displayed_transcript() {
        // Phase-32 review finding 3: switching to a project with no
        // matching threads left the previous project's transcript on
        // screen and the empty-visible fallback retargeted hidden
        // threads. The fold now clears the display, and the fallback
        // stays off once a real list sync has happened.
        let mut model = model_with_threads(&["a"]);
        model.visible_indices = vec![0];
        model.displayed_thread = Some(0);
        *model.thread_model_keys.borrow_mut() = vec!["thread-0".to_owned()];
        *model.message_model_keys.borrow_mut() = vec!["assistant:m1".to_owned()];

        let (_, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                thread_list_snapshot: Some(crate::msg::ThreadListSnapshot {
                    visible_indices: vec![],
                    visible_thread_ids: vec![],
                    rows: vec![],
                    archived_flags: vec![],
                    active_project_path: None,
                }),
                ..FrameInput::default()
            }),
        );
        assert_eq!(model.displayed_thread, None);
        assert!(!dirty.iter().any(|d| matches!(
            d,
            Dirty::MessagesDiff { thread_id, .. } if thread_id.is_empty()
        )));
        // Post-sync fallback: an empty visible list is real now.
        assert!(model.visible_list_synced);
        assert!(current_visible_indices(&model).is_empty());
    }

    #[test]
    fn selection_clamps_when_the_selected_thread_leaves_the_visible_list() {
        let mut model = model_with_threads(&["a", "b"]);
        model.visible_indices = vec![0, 1];
        model.selected_thread = 1;
        *model.thread_model_keys.borrow_mut() = vec!["thread-0".to_owned(), "thread-1".to_owned()];

        let (_, _) = update(
            &mut model,
            Msg::Frame(FrameInput {
                thread_list_snapshot: Some(crate::msg::ThreadListSnapshot {
                    visible_indices: vec![0],
                    visible_thread_ids: vec!["thread-0".to_owned()],
                    rows: vec![visible_row(0, "thread-0")],
                    archived_flags: vec![],
                    active_project_path: None,
                }),
                ..FrameInput::default()
            }),
        );

        assert_eq!(model.selected_thread, 0, "gone thread clamps selection");
    }

    // PISO-2 (project-isolation-mlt-binding plan): rebind the visible
    // thread list and the active chat on a project switch.

    #[test]
    fn project_switch_reanchors_selection_to_the_new_projects_first_thread_not_a_numeric_clamp() {
        // Four threads, two per project. Project A is active with
        // thread-1 (filtered index 1) selected. Switching to project B
        // must land on thread-2 (B's first thread, filtered index 0) --
        // NOT on thread-3, which is what a plain `selected_thread.min(new
        // len - 1)` clamp (1.min(1) == 1) would have picked, purely
        // because 1 was the old numeric position.
        let mut model = model_with_threads(&["a", "b", "c", "d"]);
        model.active_project_path = Some("/work/a/project.mlt".to_owned());
        model.synced_project_path = Some("/work/a/project.mlt".to_owned());
        model.visible_indices = vec![0, 1];
        model.selected_thread = 1; // thread-1
        *model.thread_model_keys.borrow_mut() = vec!["thread-0".to_owned(), "thread-1".to_owned()];

        // The user switches the open MLT project to B.
        model.active_project_path = Some("/work/b/project.mlt".to_owned());

        let (_, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                thread_list_snapshot: Some(crate::msg::ThreadListSnapshot {
                    visible_indices: vec![2, 3],
                    visible_thread_ids: vec!["thread-2".to_owned(), "thread-3".to_owned()],
                    rows: vec![visible_row(2, "thread-2"), visible_row(3, "thread-3")],
                    archived_flags: vec![],
                    active_project_path: Some("/work/b/project.mlt".to_owned()),
                }),
                ..FrameInput::default()
            }),
        );

        assert_eq!(
            model.selected_thread, 0,
            "must land on B's first thread (thread-2), not a numeric clamp"
        );
        assert_eq!(selected_real_index(&model), Some(2));
        assert_eq!(
            model.synced_project_path.as_deref(),
            Some("/work/b/project.mlt")
        );
        assert!(dirty
            .iter()
            .any(|item| matches!(item, Dirty::Scalar(ScalarField::SelectedThread))));
    }

    #[test]
    fn a_thread_list_snapshot_collected_for_an_already_left_project_is_dropped() {
        // The snapshot was collected while project A was active, but by
        // the time it's folded in the user has already switched to B (a
        // later `HostMsg::ProjectPathChanged` updated `active_project_
        // path` first). Applying A's visible-list shape now would show A's
        // threads under B's indicator -- the cross-project leak this plan
        // exists to close. The fold must drop it, not assume it "usually"
        // arrives before the switch.
        let mut model = model_with_threads(&["a", "b"]);
        model.active_project_path = Some("/work/b/project.mlt".to_owned());
        model.synced_project_path = Some("/work/b/project.mlt".to_owned());
        model.visible_indices = vec![1];
        model.selected_thread = 0; // thread-1, B's only thread
        *model.thread_model_keys.borrow_mut() = vec!["thread-1".to_owned()];
        let stale_rows = vec![visible_row(0, "thread-0")];

        let (_, dirty) = update(
            &mut model,
            Msg::Frame(FrameInput {
                thread_list_snapshot: Some(crate::msg::ThreadListSnapshot {
                    visible_indices: vec![0],
                    visible_thread_ids: vec!["thread-0".to_owned()],
                    rows: stale_rows,
                    archived_flags: vec![],
                    active_project_path: Some("/work/a/project.mlt".to_owned()),
                }),
                ..FrameInput::default()
            }),
        );

        assert_eq!(
            model.visible_indices,
            vec![1],
            "stale project-A snapshot must not overwrite B's visible list"
        );
        assert_eq!(model.selected_thread, 0);
        assert_eq!(
            model.synced_project_path.as_deref(),
            Some("/work/b/project.mlt"),
            "a dropped snapshot must not mark B as synced against A's shape"
        );
        assert!(
            !dirty
                .iter()
                .any(|item| matches!(item, Dirty::ThreadListDiff(_))),
            "no list diff should be emitted for a snapshot that was dropped"
        );
    }

    #[test]
    fn action_results_arm_the_shared_feedback_toast() {
        // Plan phase 28: skills top-bar failures used to emit only a
        // global (empty thread_id) Dirty::Error that no surface showed.
        // Every action-result site now also arms the shared toast.
        let mut model = Model::default();
        let (_, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::SkillCreated(Err(
                crate::effect::EffectError::new("mkdir failed"),
            ))),
        );
        assert!(dirty.iter().any(|d| matches!(d, Dirty::Toast)));
        assert_eq!(model.toast_kind, "error");
        assert_eq!(model.toast_message, "mkdir failed");
        let seq_after_error = model.toast_seq;

        let (_, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::SettingsSaved(Ok(()))),
        );
        assert!(dirty.iter().any(|d| matches!(d, Dirty::Toast)));
        assert_eq!(model.toast_kind, "status");
        assert_eq!(model.toast_message, "Settings saved");
        assert_ne!(model.toast_seq, seq_after_error, "seq bumps every show");

        let (_, dirty) = update(
            &mut model,
            Msg::Ui(crate::msg::UiMsg::Skill(
                crate::msg::SkillMsg::CopyPathRequested {
                    path: "/skills/demo/SKILL.md".into(),
                },
            )),
        );
        assert!(dirty.iter().any(|d| matches!(d, Dirty::Toast)));
        assert_eq!(model.toast_kind, "info");
    }

    #[test]
    fn mcp_server_operation_result_arms_the_shared_feedback_toast() {
        // Every `dispatch_mcp_server_*` in lib.rs used to only `eprintln!`
        // on failure, with nothing shown in the UI at all. Now routed
        // through the same shared toast every other Settings action-result
        // already uses (see `action_results_arm_the_shared_feedback_toast`
        // just above).
        let mut model = Model::default();
        let (_, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::McpServerOperationCompleted(Err(
                crate::effect::EffectError::new("Failed to create MCP server \"fs\""),
            ))),
        );
        assert!(dirty.iter().any(|d| matches!(d, Dirty::Toast)));
        assert_eq!(model.toast_kind, "error");
        assert_eq!(model.toast_message, "Failed to create MCP server \"fs\"");
        let seq_after_error = model.toast_seq;

        let (_, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::McpServerOperationCompleted(Ok(
                "MCP server \"fs\" created".to_string(),
            ))),
        );
        assert!(dirty.iter().any(|d| matches!(d, Dirty::Toast)));
        assert_eq!(model.toast_kind, "status");
        assert_eq!(model.toast_message, "MCP server \"fs\" created");
        assert_ne!(model.toast_seq, seq_after_error, "seq bumps every show");
    }

    #[test]
    fn mcp_server_tool_deferred_changed_produces_a_matching_effect() {
        let mut model = Model::default();
        let (effects, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Settings(SettingsMsg::McpServerToolDeferredChanged {
                server_name: "fs".to_string(),
                tool_name: "read_file".to_string(),
                deferred: true,
            })),
        );
        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0],
            Effect::McpServerToolDeferredChanged {
                server_name,
                tool_name,
                deferred: true,
                ..
            } if server_name == "fs" && tool_name == "read_file"
        ));
        assert!(dirty.iter().any(|d| matches!(d, Dirty::Settings)));
    }

    #[test]
    fn mcp_server_tools_fetch_requested_produces_a_matching_effect() {
        let mut model = Model::default();
        let (effects, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Settings(SettingsMsg::McpServerToolsFetchRequested {
                server_name: "fs".to_string(),
            })),
        );
        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0],
            Effect::McpServerToolsFetchRequested { server_name, .. } if server_name == "fs"
        ));
        assert!(dirty.iter().any(|d| matches!(d, Dirty::Settings)));
    }

    #[test]
    fn skill_reactive_sync_failure_surfaces_as_a_toast_not_an_error_banner() {
        // memory/acpx/gen/plans/acpx-skills/ phase 17: reactive-sync
        // failures used to be eprintln!-only, invisible to the user.
        // Deliberately NOT Dirty::Error -- the skill mutation itself
        // already succeeded (on disk, in the UI list); only the
        // downstream agent-propagation step failed.
        let mut model = Model::default();
        let (effects, dirty) = update(
            &mut model,
            Msg::Effect(EffectResultMsg::SkillReactiveSyncFailed {
                operation: "create".to_owned(),
                detail: "codex-acp: no such file or directory".to_owned(),
            }),
        );
        assert!(
            effects.is_empty(),
            "a toast-only result produces no further effects"
        );
        assert!(dirty.iter().any(|d| matches!(d, Dirty::Toast)));
        assert!(
            !dirty.iter().any(|d| matches!(d, Dirty::Error { .. })),
            "reactive-sync failures are soft/best-effort -- must not also arm the hard error banner"
        );
        assert_eq!(model.toast_kind, "error");
        assert!(model.toast_message.contains("create"));
        assert!(model
            .toast_message
            .contains("codex-acp: no such file or directory"));
    }

    #[test]
    fn skill_content_edit_absorbs_into_the_model_before_sync_runs() {
        // Plan phase 27: ContentEdited used to emit Dirty::SkillEditor
        // WITHOUT updating model.active_skill_content -- sync then pushed
        // the stale content back into the two-way-bound editor text on
        // every keystroke, so typing never stuck and saves recorded no
        // lasting delta.
        let mut model = Model::default();
        model.active_skill_path = "/skills/demo/SKILL.md".to_owned();
        model.active_skill_content = "old body".to_owned();
        let (effects, dirty) = update(
            &mut model,
            Msg::Ui(crate::msg::UiMsg::Skill(
                crate::msg::SkillMsg::ContentEdited {
                    path: "/skills/demo/SKILL.md".into(),
                    content: "old body plus a typed delta".to_owned(),
                },
            )),
        );
        assert_eq!(model.active_skill_content, "old body plus a typed delta");
        assert!(model.skill_saving);
        assert!(matches!(
            effects.as_slice(),
            [Effect::SkillWrite { content, .. }] if content == "old body plus a typed delta"
        ));
        assert!(dirty.iter().any(|d| matches!(d, Dirty::SkillEditor)));
    }

    #[test]
    fn skill_editor_loaded_keeps_the_directory_and_the_skill_md_file_distinct() {
        // PUI-010: SkillEditorLoaded used to fold state.path (the skill
        // DIRECTORY) into the only path the model tracked
        // (active_skill_path), which app.slint's content-edited handler
        // then sent straight into Effect::SkillWrite -- every save wrote
        // to the directory and hit an EISDIR OS error (see
        // effect_executor::skill_editor_path_tests for the real
        // filesystem repro of that error). Fixed by tracking the two
        // paths separately; this proves the reducer keeps them apart.
        let mut model = Model::default();
        let (_, dirty) = update(
            &mut model,
            Msg::Effect(crate::effect::EffectResultMsg::SkillEditorLoaded(Ok(
                crate::model::SkillEditorState {
                    name: "demo".to_owned(),
                    path: "/skills/demo".to_owned(),
                    content_path: "/skills/demo/SKILL.md".to_owned(),
                    content: "body".to_owned(),
                    detected_editors: vec![],
                },
            ))),
        );
        assert_eq!(model.active_skill_path, "/skills/demo");
        assert_eq!(model.active_skill_md_path, "/skills/demo/SKILL.md");
        assert_ne!(model.active_skill_path, model.active_skill_md_path);
        assert!(dirty.iter().any(|d| matches!(d, Dirty::SkillEditor)));
    }

    #[test]
    fn stale_snapshot_for_an_unselected_thread_never_steals_the_display() {
        // Plan phase 23, second leg: a selected-thread snapshot is
        // collected via the *pre-reanchor* index mapping, so for one frame
        // it can describe a thread the user is no longer on. Its data may
        // hydrate that thread's own cache (by durable id), but it must not
        // flip `displayed_thread` -- sync.rs renders whatever
        // `displayed_thread` points at, and flipping it here was the
        // visible cross-thread message leak.
        let mut model = model_with_threads(&["a", "b"]);
        model.visible_indices = vec![0, 1];
        model.selected_thread = 0;
        model.displayed_thread = Some(0);

        let transcript = vec![crate::conversation::TranscriptItem::Assistant {
            message_id: "other-thread-msg".to_owned(),
            text: "belongs to thread-1".to_owned(),
            streaming: false,
        }];
        let (_, _) = update(
            &mut model,
            Msg::Frame(FrameInput {
                selected_thread_snapshot: Some(crate::msg::ThreadFrameSnapshot {
                    thread_id: "thread-1".to_owned(),
                    real_index: 1,
                    transcript: transcript.clone(),
                    has_older_messages: false,
                    pending_request: crate::PendingRequestItem::default(),
                    terminals: vec![],
                    expanded_terminal: None,
                    open_terminals: vec![],
                    local_terminal: crate::LocalTerminalItem::default(),
                    connection_status: String::new(),
                    session_modes: None,
                    config_options: vec![],
                    available_commands: vec![],
                    plan: vec![],
                    session_title: None,
                    usage: (0, 0),
                }),
                ..FrameInput::default()
            }),
        );

        assert_eq!(
            model.displayed_thread,
            Some(0),
            "a snapshot for an unselected thread must not become the displayed thread"
        );
        // Hydration by durable id still lands in the right thread's cache.
        assert_eq!(model.threads[1].transcript, transcript);
    }

    // Terminal-tabs phase: `update_terminal`'s `Expand`/`SelectTab`/
    // `CloseTab`/`CloseOverlay` arms are pure reducer logic over
    // `Model::open_terminal_ids`/`expanded_terminal_id`, so they're
    // testable directly through `update()` without a live `ChatPanel`
    // (see `sync_commands_model`'s doc comment in `sync.rs` for the same
    // "extract so it's testable without the platform" reasoning).

    #[test]
    fn terminal_expand_opens_a_new_tab_and_activates_it() {
        let mut model = model_with_threads(&["a"]);
        assert!(model.open_terminal_ids.is_empty());

        update(
            &mut model,
            Msg::Ui(UiMsg::Terminal(TerminalMsg::Expand("t1".to_owned()))),
        );

        assert_eq!(model.open_terminal_ids, vec!["t1".to_owned()]);
        assert_eq!(model.expanded_terminal_id, Some("t1".to_owned()));
    }

    #[test]
    fn terminal_expand_on_an_already_open_tab_activates_it_without_duplicating() {
        let mut model = model_with_threads(&["a"]);
        model.open_terminal_ids = vec!["t1".to_owned(), "t2".to_owned()];
        model.expanded_terminal_id = Some("t2".to_owned());

        update(
            &mut model,
            Msg::Ui(UiMsg::Terminal(TerminalMsg::Expand("t1".to_owned()))),
        );

        assert_eq!(
            model.open_terminal_ids,
            vec!["t1".to_owned(), "t2".to_owned()],
            "re-expanding an already-open tab must not push a duplicate entry"
        );
        assert_eq!(model.expanded_terminal_id, Some("t1".to_owned()));
    }

    #[test]
    fn terminal_select_tab_switches_active_among_open_tabs() {
        let mut model = model_with_threads(&["a"]);
        model.open_terminal_ids = vec!["t1".to_owned(), "t2".to_owned()];
        model.expanded_terminal_id = Some("t1".to_owned());

        update(
            &mut model,
            Msg::Ui(UiMsg::Terminal(TerminalMsg::SelectTab("t2".to_owned()))),
        );

        assert_eq!(model.expanded_terminal_id, Some("t2".to_owned()));
        assert_eq!(
            model.open_terminal_ids,
            vec!["t1".to_owned(), "t2".to_owned()]
        );
    }

    #[test]
    fn terminal_select_tab_ignores_an_id_that_is_not_open() {
        let mut model = model_with_threads(&["a"]);
        model.open_terminal_ids = vec!["t1".to_owned()];
        model.expanded_terminal_id = Some("t1".to_owned());

        update(
            &mut model,
            Msg::Ui(UiMsg::Terminal(TerminalMsg::SelectTab(
                "never-opened".to_owned(),
            ))),
        );

        assert_eq!(
            model.expanded_terminal_id,
            Some("t1".to_owned()),
            "a stray tab-strip id (e.g. racing a close) must not become active"
        );
        assert_eq!(model.open_terminal_ids, vec!["t1".to_owned()]);
    }

    #[test]
    fn terminal_close_tab_activates_the_tab_that_slides_into_its_slot() {
        let mut model = model_with_threads(&["a"]);
        model.open_terminal_ids = vec!["t1".to_owned(), "t2".to_owned(), "t3".to_owned()];
        model.expanded_terminal_id = Some("t2".to_owned());

        update(
            &mut model,
            Msg::Ui(UiMsg::Terminal(TerminalMsg::CloseTab("t2".to_owned()))),
        );

        assert_eq!(
            model.open_terminal_ids,
            vec!["t1".to_owned(), "t3".to_owned()]
        );
        assert_eq!(
            model.expanded_terminal_id,
            Some("t3".to_owned()),
            "closing the active tab should land on its former right-hand neighbor"
        );
    }

    #[test]
    fn terminal_close_tab_falls_back_to_the_previous_tab_when_closing_the_last_one() {
        let mut model = model_with_threads(&["a"]);
        model.open_terminal_ids = vec!["t1".to_owned(), "t2".to_owned()];
        model.expanded_terminal_id = Some("t2".to_owned());

        update(
            &mut model,
            Msg::Ui(UiMsg::Terminal(TerminalMsg::CloseTab("t2".to_owned()))),
        );

        assert_eq!(model.open_terminal_ids, vec!["t1".to_owned()]);
        assert_eq!(model.expanded_terminal_id, Some("t1".to_owned()));
    }

    #[test]
    fn terminal_close_tab_that_is_not_active_leaves_active_untouched() {
        let mut model = model_with_threads(&["a"]);
        model.open_terminal_ids = vec!["t1".to_owned(), "t2".to_owned()];
        model.expanded_terminal_id = Some("t1".to_owned());

        update(
            &mut model,
            Msg::Ui(UiMsg::Terminal(TerminalMsg::CloseTab("t2".to_owned()))),
        );

        assert_eq!(model.open_terminal_ids, vec!["t1".to_owned()]);
        assert_eq!(model.expanded_terminal_id, Some("t1".to_owned()));
    }

    #[test]
    fn terminal_close_the_last_open_tab_clears_the_active_id_entirely() {
        let mut model = model_with_threads(&["a"]);
        model.open_terminal_ids = vec!["t1".to_owned()];
        model.expanded_terminal_id = Some("t1".to_owned());

        update(
            &mut model,
            Msg::Ui(UiMsg::Terminal(TerminalMsg::CloseTab("t1".to_owned()))),
        );

        assert!(model.open_terminal_ids.is_empty());
        assert_eq!(
            model.expanded_terminal_id, None,
            "closing the only open tab must fully close the overlay, not leave a dangling active id"
        );
    }

    #[test]
    fn terminal_close_overlay_clears_every_open_tab_not_just_the_active_one() {
        let mut model = model_with_threads(&["a"]);
        model.open_terminal_ids = vec!["t1".to_owned(), "t2".to_owned(), "t3".to_owned()];
        model.expanded_terminal_id = Some("t2".to_owned());

        update(
            &mut model,
            Msg::Ui(UiMsg::Terminal(TerminalMsg::CloseOverlay)),
        );

        assert!(
            model.open_terminal_ids.is_empty(),
            "the overlay-wide Close/Escape path must close every tab, not just the active one"
        );
        assert_eq!(model.expanded_terminal_id, None);
    }

    // --- chat_view_audit §5 isolation + presentation e2e (reducer/sync) ---

    #[test]
    fn selection_switch_atomically_sets_displayed_owner_and_installs_target_list() {
        let mut model = model_with_threads(&["a", "b"]);
        model.displayed_thread = Some(0);
        model.list_owner_thread_id = Some("thread-0".to_owned());
        model.threads[0].transcript_keys = vec!["assistant:a1".to_owned()];
        model.threads[0].message_rows = vec![crate::MessageItem {
            text: "from A".into(),
            expanded: true,
            ..crate::MessageItem::default()
        }];
        model.threads[1].transcript_keys = vec!["assistant:b1".to_owned()];
        model.threads[1].message_rows = vec![crate::MessageItem {
            text: "from B".into(),
            ..crate::MessageItem::default()
        }];
        model.rebuild_thread_indices();
        crate::sync::apply_message_ops(&model, "thread-0", &[]);
        crate::sync::apply_message_ops(&model, "thread-1", &[]);
        model
            .messages_model
            .push(model.threads[0].message_rows[0].clone());
        *model.message_model_keys.borrow_mut() = vec!["assistant:a1".to_owned()];

        let (_, _dirty) = update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::Selected(1))));

        assert_eq!(model.displayed_thread, Some(1));
        assert_eq!(model.list_owner_thread_id.as_deref(), Some("thread-0"));
        assert_eq!(
            model
                .thread_view_models
                .get("thread-0")
                .unwrap()
                .row_data(0)
                .unwrap()
                .text,
            "from A"
        );
        assert_eq!(
            model
                .thread_view_models
                .get("thread-1")
                .unwrap()
                .row_data(0)
                .unwrap()
                .text,
            "from B"
        );
    }

    #[test]
    fn expand_survives_thread_switch_leave_and_return() {
        let mut model = model_with_threads(&["a", "b"]);
        model.selected_thread = 0;
        model.displayed_thread = Some(0);
        model.list_owner_thread_id = Some("thread-0".to_owned());
        model.threads[0].transcript_keys = vec!["thinking:t1".to_owned()];
        model.threads[0].message_rows = vec![crate::MessageItem {
            text: "thought".into(),
            kind: "thinking".into(),
            expanded: false,
            ..crate::MessageItem::default()
        }];
        model.rebuild_thread_indices();
        crate::sync::apply_message_ops(&model, "thread-0", &[]);
        model
            .messages_model
            .push(model.threads[0].message_rows[0].clone());
        *model.message_model_keys.borrow_mut() = vec!["thinking:t1".to_owned()];
        model.expanded = vec![false];

        // Expand on A.
        let (_, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Chrome(ChromeMsg::ToggleExpanded(0))),
        );
        assert!(matches!(
            dirty.as_slice(),
            [Dirty::MessageRowPatch {
                thread_id,
                index: 0
            }] if thread_id == "thread-0"
        ));
        assert!(model.threads[0].message_rows[0].expanded);
        crate::sync::apply_message_row_patch(&model, "thread-0", 0);
        assert!(
            model
                .thread_view_models
                .get("thread-0")
                .unwrap()
                .row_data(0)
                .unwrap()
                .expanded
        );

        // A → B
        let _ = update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::Selected(1))));
        assert_eq!(model.displayed_thread, Some(1));

        // B → A: the retained A model already owns the expanded row.
        let _ = update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::Selected(0))));
        assert!(
            model.threads[0].message_rows[0].expanded,
            "return to A must preserve expanded on the source row"
        );
        assert!(
            model
                .thread_view_models
                .get("thread-0")
                .unwrap()
                .row_data(0)
                .unwrap()
                .expanded,
            "retained A view must show expanded after return"
        );
    }

    #[test]
    fn toggle_expanded_is_one_row_patch_not_full_messages_diff() {
        let mut model = model_with_threads(&["only"]);
        model.displayed_thread = Some(0);
        model.threads[0].message_rows = vec![
            crate::MessageItem {
                text: "t".into(),
                kind: "thinking".into(),
                ..crate::MessageItem::default()
            },
            crate::MessageItem {
                text: "a".into(),
                ..crate::MessageItem::default()
            },
        ];
        model.expanded = vec![false, false];
        let (_, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Chrome(ChromeMsg::ToggleExpanded(0))),
        );
        assert!(
            !dirty
                .iter()
                .any(|d| matches!(d, Dirty::MessagesDiff { .. })),
            "expand must not full-rebuild list: {dirty:?}"
        );
        assert!(dirty
            .iter()
            .any(|d| matches!(d, Dirty::MessageRowPatch { index: 0, .. })));
    }

    fn config_option(
        id: &str,
        current_value: Option<&str>,
        values: &[&str],
    ) -> crate::protocol_types::ConfigOptionInfo {
        crate::protocol_types::ConfigOptionInfo {
            id: id.to_owned(),
            name: id.to_owned(),
            description: None,
            category: None,
            kind: "select".into(),
            current_value: current_value.map(str::to_owned),
            options: values
                .iter()
                .map(|value| crate::protocol_types::ConfigOptionValue {
                    value: (*value).to_owned(),
                    name: (*value).to_owned(),
                    description: None,
                })
                .collect(),
        }
    }

    #[test]
    fn config_option_merge_keeps_current_value_when_still_a_valid_choice() {
        let old = vec![config_option("model", Some("opus"), &["sonnet", "opus"])];
        let new = vec![config_option("model", None, &["sonnet", "opus", "haiku"])];
        let merged = merge_config_options_preserving_current_value(&old, new);
        assert_eq!(merged[0].current_value.as_deref(), Some("opus"));
    }

    #[test]
    fn config_option_merge_drops_current_value_once_it_is_no_longer_offered() {
        let old = vec![config_option("model", Some("opus"), &["sonnet", "opus"])];
        let new = vec![config_option("model", None, &["sonnet", "haiku"])];
        let merged = merge_config_options_preserving_current_value(&old, new);
        assert_eq!(
            merged[0].current_value, None,
            "must not fabricate a value that isn't a real choice in the fresh snapshot"
        );
    }
}
